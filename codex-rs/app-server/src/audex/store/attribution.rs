//! Route-specific result authority over separate generation and client evidence.
use super::admission::AdmissionIdentity;
use super::*;
use crate::audex::retained::RetainedInput;
use codex_app_server_protocol::AuditAuthenticationRoute;

pub(super) async fn retained(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
) -> Result<Option<RetainedInput>, AuditStoreError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT request_json,input_digest FROM audit_attempt_inputs WHERE assignment_id=?",
    )
    .bind(assignment)
    .fetch_optional(connection)
    .await?;
    let Some((raw, digest)) = row else {
        return Ok(None);
    };
    if sha256_hex(raw.as_bytes()) != digest {
        return Err(AuditStoreError::InvalidPersistedState(
            "retained attribution input digest mismatch".to_owned(),
        ));
    }
    // Pre-roster retained inputs were opaque. They retain their original predicate.
    let value = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if value.get("version").is_none()
        && value.get("authenticationRoute").is_none()
        && value.get("executionAdapter").is_none()
        && value.get("requireServedIdentity").is_none()
    {
        return Ok(None);
    }
    RetainedInput::decode(&raw).map(Some).map_err(|_| {
        AuditStoreError::InvalidPersistedState("invalid retained attribution policy".to_owned())
    })
}

pub(super) async fn require_completion(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
    result_digest: &str,
) -> Result<(), AuditStoreError> {
    let input = retained(connection, assignment).await?;
    let failed: bool = sqlx::query_scalar(
        "SELECT attribution_failure IS NOT NULL FROM audit_attempts WHERE assignment_id=?",
    )
    .bind(assignment)
    .fetch_one(&mut *connection)
    .await?;
    let subscription = input
        .as_ref()
        .is_some_and(|i| i.route() == AuditAuthenticationRoute::ClaudeSubscription);
    let valid: bool = if subscription {
        let expected = input.as_ref().expect("subscription input").expected_model();
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_client_terminal_evidence c
            JOIN operations o ON o.id=c.operation_id AND o.assignment_id=c.assignment_id
            JOIN audit_execution e ON e.operation_id=o.id AND e.thread_id=c.execution_id
            WHERE c.assignment_id=? AND c.successful=1 AND c.verified=1 AND c.result_digest=?
            AND EXISTS(SELECT 1 FROM audit_client_model_observations m WHERE m.assignment_id=c.assignment_id AND m.operation_id=c.operation_id AND m.execution_id=c.execution_id)
            AND NOT EXISTS(SELECT 1 FROM audit_client_model_observations m WHERE m.assignment_id=c.assignment_id AND
                (m.operation_id<>c.operation_id OR m.execution_id<>c.execution_id OR m.identity_state<>'verified' OR m.expected_model<>? OR m.observed_model IS NULL OR m.observed_model<>?)))")
            .bind(assignment).bind(result_digest).bind(expected).bind(expected).fetch_one(&mut *connection).await?
    } else if input
        .as_ref()
        .is_some_and(|i| i.version == 2 || i.require_served_identity)
    {
        let expected = input.as_ref().expect("strict input").expected_model();
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_request_admissions WHERE assignment_id=? AND identity_state='verified' AND model_scope='generation')
            AND NOT EXISTS(SELECT 1 FROM audit_request_admissions g WHERE g.assignment_id=? AND
            (g.identity_state IN ('missing','mismatch') OR g.expected_model IS NULL OR g.expected_model<>?
             OR (g.observed_model IS NOT NULL AND g.observed_model<>?) OR g.send_started_at_ms IS NULL
             OR NOT EXISTS(SELECT 1 FROM operations o JOIN audit_execution e ON e.operation_id=o.id
                 WHERE o.assignment_id=g.assignment_id AND o.id=g.operation_id AND e.thread_id=g.execution_id)))")
            .bind(assignment).bind(assignment).bind(expected).bind(expected).fetch_one(&mut *connection).await?
    } else {
        sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM audit_request_admissions WHERE assignment_id=? AND expected_model IS NOT NULL AND identity_state IN ('missing','mismatch'))")
            .bind(assignment).fetch_one(&mut *connection).await?
    };
    if failed || !valid {
        return Err(AuditStoreError::InvalidTransition(if subscription {
            "completed subscription result has no verified bound terminal and message attribution"
        } else { "completed result has no verified per-generation model attribution" }.to_owned()));
    }
    Ok(())
}

async fn subscription_owner(
    connection: &mut sqlx::SqliteConnection,
    identity: &AdmissionIdentity,
) -> Result<RetainedInput, AuditStoreError> {
    let input = retained(connection, &identity.assignment_id)
        .await?
        .ok_or_else(|| {
            AuditStoreError::InvalidPersistedState("missing client attribution policy".to_owned())
        })?;
    if input.route() != AuditAuthenticationRoute::ClaudeSubscription
        || !super::admission::owner_active(connection, identity, now_millis()).await?
    {
        return Err(AuditStoreError::InvalidTransition(
            "client evidence route or execution owner is invalid".to_owned(),
        ));
    }
    Ok(input)
}

impl AuditStore {
    /// Client-owned message observations remain distinct from strict admissions.
    pub(in crate::audex) async fn observe_client_message(
        &self,
        identity: &AdmissionIdentity,
        message_id: &str,
        model: Option<&str>,
        verified: bool,
    ) -> Result<(), AuditStoreError> {
        validate_nonempty("client message identity", message_id, MAX_IDENTIFIER_BYTES)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let input = subscription_owner(&mut tx, identity).await?;
        let model = model.filter(|m| !m.trim().is_empty() && m.len() <= 1024);
        let state = match model {
            None => "missing",
            Some(model) if model != input.expected_model() => "mismatch",
            Some(_) if !verified => "unverified",
            Some(_) => "verified",
        };
        let previous: Option<(String, Option<String>, String)> = sqlx::query_as("SELECT operation_id,observed_model,identity_state FROM audit_client_model_observations WHERE assignment_id=? AND execution_id=? AND message_id=?")
            .bind(&identity.assignment_id).bind(&identity.execution_id).bind(message_id).fetch_optional(&mut *tx).await?;
        let mut conflict = false;
        if let Some((operation, observed, status)) = previous {
            conflict = operation != identity.operation_id
                || observed.as_deref() != model
                || status != state;
        } else {
            sqlx::query("INSERT INTO audit_client_model_observations(assignment_id,operation_id,execution_id,message_id,expected_model,observed_model,identity_state) VALUES(?,?,?,?,?,?,?)")
                .bind(&identity.assignment_id).bind(&identity.operation_id).bind(&identity.execution_id).bind(message_id)
                .bind(input.expected_model()).bind(model).bind(state).execute(&mut *tx).await?;
        }
        if conflict || state != "verified" {
            sqlx::query("UPDATE audit_attempts SET attribution_failure=COALESCE(attribution_failure,'client_message_identity_failure'),revision=revision+1 WHERE assignment_id=?")
                .bind(&identity.assignment_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if conflict || state != "verified" {
            return Err(AuditStoreError::InvalidTransition(
                "client message identity is missing, wrong, unverified or conflicting".to_owned(),
            ));
        }
        Ok(())
    }

    /// Legacy attribution-contract fixture. Production uses bound stream artefacts.
    #[cfg(test)]
    pub(in crate::audex) async fn record_client_terminal(
        &self,
        identity: &AdmissionIdentity,
        terminal_id: &str,
        successful: bool,
        verified: bool,
        raw_result: &[u8],
    ) -> Result<(), AuditStoreError> {
        validate_nonempty(
            "client terminal identity",
            terminal_id,
            MAX_IDENTIFIER_BYTES,
        )?;
        if raw_result.len() > result::MAX_RAW_RESULT_BYTES {
            return Err(AuditStoreError::InvalidInput(
                "client result exceeds retention bound".to_owned(),
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        subscription_owner(&mut tx, identity).await?;
        let digest = sha256_hex(raw_result);
        // Immutable raw final text is recoverable independently of process exit.
        let path = format!("client-terminal/{}/{}.json", identity.assignment_id, digest);
        publish_immutable(self.root.as_path(), &path, raw_result, &digest)?;
        let previous: Option<(String,String,String,bool,bool,String)> = sqlx::query_as("SELECT operation_id,execution_id,terminal_id,successful,verified,result_digest FROM audit_client_terminal_evidence WHERE assignment_id=?")
            .bind(&identity.assignment_id).fetch_optional(&mut *tx).await?;
        let evidence = (
            identity.operation_id.clone(),
            identity.execution_id.clone(),
            terminal_id.to_owned(),
            successful,
            verified,
            digest.clone(),
        );
        if let Some(previous) = previous {
            if previous != evidence {
                sqlx::query("UPDATE audit_attempts SET attribution_failure=COALESCE(attribution_failure,'client_terminal_conflict'),revision=revision+1 WHERE assignment_id=?")
                    .bind(&identity.assignment_id).execute(&mut *tx).await?;
                tx.commit().await?;
                return Err(AuditStoreError::InvalidTransition(
                    "client terminal evidence conflicts".to_owned(),
                ));
            }
        } else {
            sqlx::query("INSERT INTO audit_client_terminal_evidence(assignment_id,operation_id,execution_id,terminal_id,successful,verified,result_digest) VALUES(?,?,?,?,?,?,?)")
                .bind(&identity.assignment_id).bind(&identity.operation_id).bind(&identity.execution_id).bind(terminal_id)
                .bind(successful).bind(verified).bind(digest).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
