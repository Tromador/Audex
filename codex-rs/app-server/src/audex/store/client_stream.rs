//! Execution-bound stream receipts and immutable terminal evidence. No workflow authority.
use super::admission::{AdmissionIdentity, owner_active};
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct ClientUsage {
    pub input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub estimated_cost_usd: Option<String>,
    pub output_tokens: Option<u64>,
}

/// Only public terminal fields are retained. No arbitrary extras, errors or raw JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct ClientResultEvent {
    pub r#type: String,
    pub subtype: String,
    pub is_error: bool,
    pub uuid: String,
    pub session_id: String,
    pub result: String,
    pub usage: ClientUsage,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub total_cost_usd: Option<serde_json::Number>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct ClientTerminalArtefact {
    pub version: u32,
    pub identity: AdmissionIdentity,
    pub route: String,
    pub final_message_id: String,
    pub final_message_model: String,
    pub final_text: String,
    pub terminal: ClientResultEvent,
}

fn invalid() -> AuditStoreError {
    AuditStoreError::InvalidPersistedState(
        "external terminal identity or evidence is invalid".into(),
    )
}

async fn require_owner(
    connection: &mut sqlx::SqliteConnection,
    identity: &AdmissionIdentity,
    session: &str,
) -> Result<(), AuditStoreError> {
    validate_nonempty("client session", session, 256)?;
    if !owner_active(connection, identity, now_millis()).await? {
        return Err(invalid());
    }
    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM audit_client_launches
        WHERE operation_id=? AND execution_id=? AND client_session_id=?
        AND identity_at_ms IS NOT NULL AND prompt_write_started_at_ms IS NOT NULL)",
    )
    .bind(&identity.operation_id)
    .bind(&identity.execution_id)
    .bind(session)
    .fetch_one(connection)
    .await?;
    if !valid {
        return Err(invalid());
    }
    Ok(())
}

impl AuditStore {
    /// Internal best-effort stop, ordered with terminal persistence and operator controls.
    /// It does not exclude wanted work or grant replay authority.
    pub(in crate::audex) async fn stop_client_for_policy(
        &self,
        identity: &AdmissionIdentity,
        capture: &activity::CaptureIdentity,
        prefix: &[activity::ActivityInput],
    ) -> Result<(), AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if capture.assignment_id != identity.assignment_id
            || capture.session_id != identity.execution_id
            || capture.owner_generation != identity.owner_generation
        {
            return Err(invalid());
        }
        if owner_active(&mut tx, identity, now_millis()).await? {
            let appended = activity::append_batch(&mut tx, capture, prefix).await?;
            if appended.fenced {
                return Err(invalid());
            }
            sqlx::query("UPDATE audit_client_launches SET local_stop_requested_at_ms=COALESCE(local_stop_requested_at_ms,?),local_status='stop_requested' WHERE operation_id=? AND execution_id=?")
                .bind(now_millis()).bind(&identity.operation_id).bind(&identity.execution_id)
                .execute(&mut *tx).await?;
            sqlx::query("UPDATE audit_attempts SET capture_failure='client_request_policy_stop',capture_gap=COALESCE(capture_gap,'client policy interruption; retained output is incomplete') WHERE assignment_id=?")
                .bind(&identity.assignment_id).execute(&mut *tx).await?;
        }
        // If terminal persistence or a control won, leave its exact contribution alone.
        tx.commit().await?;
        Ok(())
    }

    pub(in crate::audex) async fn client_capture_failure(
        &self,
        assignment: &str,
    ) -> Result<Option<String>, AuditStoreError> {
        Ok(sqlx::query_scalar("SELECT COALESCE(capture_failure,attribution_failure) FROM audit_attempts WHERE assignment_id=?")
            .bind(assignment).fetch_one(&self.pool).await?)
    }

    pub(in crate::audex) async fn bind_client_session(
        &self,
        identity: &AdmissionIdentity,
        session: &str,
        accepted: bool,
    ) -> Result<(), AuditStoreError> {
        validate_nonempty("client session", session, 256)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !owner_active(&mut tx, identity, now_millis()).await? {
            return Err(invalid());
        }
        let changed = sqlx::query("UPDATE audit_client_launches SET client_session_id=?,
            client_accepted_at_ms=CASE WHEN ? THEN COALESCE(client_accepted_at_ms,?) ELSE client_accepted_at_ms END
            WHERE operation_id=? AND execution_id=? AND identity_at_ms IS NOT NULL
            AND prompt_write_started_at_ms IS NOT NULL AND (client_session_id IS NULL OR client_session_id=?)")
            .bind(session).bind(accepted).bind(now_millis()).bind(&identity.operation_id)
            .bind(&identity.execution_id).bind(session).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(invalid());
        }
        tx.commit().await?;
        Ok(())
    }

    pub(in crate::audex) async fn observe_bound_client_message(
        &self,
        identity: &AdmissionIdentity,
        session: &str,
        message: &str,
        model: Option<&str>,
        usage: Option<&ClientUsage>,
    ) -> Result<(), AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_owner(&mut tx, identity, session).await?;
        let input = attribution::retained(&mut tx, &identity.assignment_id)
            .await?
            .ok_or_else(invalid)?;
        tx.commit().await?;
        if input.route() == codex_app_server_protocol::AuditAuthenticationRoute::ClaudeSubscription
        {
            self.observe_client_message(identity, message, model, true)
                .await?;
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            require_owner(&mut tx, identity, session).await?;
            let previous: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT client_session_id,usage_json FROM audit_client_model_observations
                WHERE assignment_id=? AND execution_id=? AND message_id=?",
            )
            .bind(&identity.assignment_id)
            .bind(&identity.execution_id)
            .bind(message)
            .fetch_one(&mut *tx)
            .await?;
            let usage = usage.map(serde_json::to_string).transpose()?;
            if previous.0.as_deref().is_some_and(|old| old != session)
                || (previous.1.is_some() && usage.is_some() && previous.1 != usage)
            {
                return Err(invalid());
            }
            sqlx::query("UPDATE audit_client_model_observations SET client_session_id=?,usage_json=COALESCE(usage_json,?)
                WHERE assignment_id=? AND execution_id=? AND message_id=?")
                .bind(session).bind(usage).bind(&identity.assignment_id).bind(&identity.execution_id).bind(message)
                .execute(&mut *tx).await?;
            tx.commit().await?;
        } else if model != Some(input.expected_model()) {
            return Err(invalid());
        }
        Ok(())
    }

    pub(in crate::audex) async fn persist_client_terminal(
        &self,
        evidence: &ClientTerminalArtefact,
        capture: &activity::CaptureIdentity,
        prefix: &[activity::ActivityInput],
    ) -> Result<(), AuditStoreError> {
        let id = &evidence.identity;
        let terminal = &evidence.terminal;
        validate_nonempty("terminal identity", &terminal.uuid, 256)?;
        validate_nonempty("final message", &evidence.final_message_id, 256)?;
        if evidence.version != 1
            || terminal.r#type != "result"
            || terminal.subtype != "success"
            || terminal.is_error
            || evidence.final_text != terminal.result
            || evidence.final_text.len() > result::MAX_RAW_RESULT_BYTES
            || capture.assignment_id != id.assignment_id
            || capture.session_id != id.execution_id
            || capture.owner_generation != id.owner_generation
        {
            return Err(invalid());
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_owner(&mut tx, id, &terminal.session_id).await?;
        let input = attribution::retained(&mut tx, &id.assignment_id)
            .await?
            .ok_or_else(invalid)?;
        let route: String =
            sqlx::query_scalar("SELECT route FROM audit_client_launches WHERE operation_id=?")
                .bind(&id.operation_id)
                .fetch_one(&mut *tx)
                .await?;
        if route != evidence.route || evidence.final_message_model != input.expected_model() {
            return Err(invalid());
        }
        let appended = activity::append_batch(&mut tx, capture, prefix).await?;
        if appended.fenced || appended.truncated {
            return Err(invalid());
        }
        let bytes = serde_json::to_vec(evidence)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(invalid());
        }
        let digest = sha256_hex(&bytes);
        let path = format!("client-terminal/{}/{}.json", id.assignment_id, digest);
        publish_immutable(self.root.as_path(), &path, &bytes, &digest)?;
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT artefact_digest FROM audit_client_terminal_evidence WHERE assignment_id=?",
        )
        .bind(&id.assignment_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        if previous.as_ref().is_some_and(|old| old != &digest) {
            return Err(invalid());
        }
        sqlx::query("INSERT INTO audit_client_terminal_evidence
            (assignment_id,operation_id,execution_id,terminal_id,successful,verified,result_digest,client_session_id,final_message_id,artefact_path,artefact_digest,usage_json)
            VALUES(?,?,?,?,1,1,?,?,?,?,?,?) ON CONFLICT(assignment_id) DO NOTHING")
            .bind(&id.assignment_id).bind(&id.operation_id).bind(&id.execution_id).bind(&terminal.uuid)
            .bind(sha256_hex(evidence.final_text.as_bytes())).bind(&terminal.session_id).bind(&evidence.final_message_id)
            .bind(&path).bind(&digest).bind(serde_json::to_string(&terminal.usage)?).execute(&mut *tx).await?;
        sqlx::query("UPDATE audit_attempts SET capture_finished=1 WHERE assignment_id=?")
            .bind(&id.assignment_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Read original immutable evidence. No child, prompt or inference is involved.
    pub(in crate::audex) async fn client_terminal_result(
        &self,
        operation: &str,
    ) -> Result<Option<Vec<u8>>, AuditStoreError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as("SELECT assignment_id,result_digest FROM audit_client_terminal_evidence WHERE operation_id=?")
            .bind(operation).fetch_optional(&mut *tx).await?;
        let Some((assignment, digest)) = row else {
            return Ok(None);
        };
        let evidence = require_bound_terminal(&mut tx, self.root.as_path(), &assignment, &digest)
            .await?
            .ok_or_else(invalid)?;
        attribution::require_completion(&mut tx, &assignment, &digest).await?;
        tx.commit().await?;
        Ok(Some(evidence.final_text.into_bytes()))
    }
}

pub(super) async fn require_bound_terminal(
    connection: &mut sqlx::SqliteConnection,
    root: &Path,
    assignment: &str,
    result_digest: &str,
) -> Result<Option<ClientTerminalArtefact>, AuditStoreError> {
    let launch = sqlx::query("SELECT l.*,t.run_id,o.assignment_id,e.thread_id AS bound_execution_id FROM audit_client_launches l
        JOIN operations o ON o.id=l.operation_id JOIN audit_attempts t ON t.assignment_id=o.assignment_id LEFT JOIN audit_execution e ON e.operation_id=o.id
        WHERE o.assignment_id=?")
        .bind(assignment).fetch_optional(&mut *connection).await?;
    let Some(launch) = launch else {
        return Ok(None);
    };
    let terminal =
        sqlx::query("SELECT * FROM audit_client_terminal_evidence WHERE assignment_id=?")
            .bind(assignment)
            .fetch_optional(&mut *connection)
            .await?
            .ok_or_else(invalid)?;
    let path: Option<String> = terminal.try_get("artefact_path")?;
    let digest: Option<String> = terminal.try_get("artefact_digest")?;
    let path = path.ok_or_else(invalid)?;
    if std::fs::metadata(safe_join(root, &path)?)?.len() > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    let bytes = verify_immutable(root, &path, &digest.ok_or_else(invalid)?)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    let evidence: ClientTerminalArtefact = serde_json::from_slice(&bytes)?;
    let id = &evidence.identity;
    let event = &evidence.terminal;
    let session: Option<String> = launch.try_get("client_session_id")?;
    let recorded_session: Option<String> = terminal.try_get("client_session_id")?;
    let message: Option<String> = terminal.try_get("final_message_id")?;
    if evidence.version != 1
        || id.assignment_id != assignment
        || id.run_id != launch.try_get::<String, _>("run_id")?
        || id.operation_id != launch.try_get::<String, _>("operation_id")?
        || id.execution_id != launch.try_get::<String, _>("execution_id")?
        || launch
            .try_get::<Option<String>, _>("bound_execution_id")?
            .as_deref()
            != Some(id.execution_id.as_str())
        || id.operation_id != terminal.try_get::<String, _>("operation_id")?
        || id.execution_id != terminal.try_get::<String, _>("execution_id")?
        || evidence.route != launch.try_get::<String, _>("route")?
        || session.as_deref() != Some(event.session_id.as_str())
        || session != recorded_session
        || message.as_deref() != Some(evidence.final_message_id.as_str())
        || event.uuid != terminal.try_get::<String, _>("terminal_id")?
        || event.r#type != "result"
        || event.subtype != "success"
        || event.is_error
        || !terminal.try_get::<bool, _>("successful")?
        || !terminal.try_get::<bool, _>("verified")?
        || event.result != evidence.final_text
        || sha256_hex(evidence.final_text.as_bytes()) != result_digest
        || result_digest != terminal.try_get::<String, _>("result_digest")?
        || launch
            .try_get::<Option<i64>, _>("client_accepted_at_ms")?
            .is_none()
    {
        return Err(invalid());
    }
    let clean: bool = sqlx::query_scalar("SELECT capture_finished=1 AND capture_failure IS NULL AND capture_truncated=0 AND attribution_failure IS NULL FROM audit_attempts WHERE assignment_id=?")
        .bind(assignment).fetch_one(&mut *connection).await?;
    if !clean {
        return Err(invalid());
    }
    let input = attribution::retained(connection, assignment)
        .await?
        .ok_or_else(invalid)?;
    if input.expected_model() != evidence.final_message_model {
        return Err(invalid());
    }
    if evidence.route == "claude_subscription" {
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_client_model_observations WHERE assignment_id=? AND execution_id=? AND message_id=? AND client_session_id=?)
            AND NOT EXISTS(SELECT 1 FROM audit_client_model_observations WHERE assignment_id=? AND (client_session_id IS NULL OR client_session_id<>?))")
            .bind(assignment).bind(&id.execution_id).bind(&evidence.final_message_id).bind(&event.session_id)
            .bind(assignment).bind(&event.session_id).fetch_one(connection).await?;
        if !valid {
            return Err(invalid());
        }
    }
    Ok(Some(evidence))
}
