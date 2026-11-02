//! One-use generation authority, ordered by the same SQLite writer as controls.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::audex) struct AdmissionIdentity {
    pub run_id: String,
    pub assignment_id: String,
    pub operation_id: String,
    pub execution_id: String,
    pub owner: String,
    pub owner_generation: i64,
    pub run_stop_generation: i64,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::audex) struct GenerationRecord {
    pub request_id: String,
    pub execution_id: String,
    pub transport: String,
    pub send_started_at_ms: Option<i64>,
    pub confirmed_at_ms: Option<i64>,
    pub provider_response_id: Option<String>,
}

pub(super) async fn owner_active(
    connection: &mut sqlx::SqliteConnection,
    identity: &AdmissionIdentity,
    now: i64,
) -> Result<bool, AuditStoreError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_attempts t
        JOIN audit_reviewers v ON v.id=t.reviewer_id JOIN audit_runs r ON r.id=t.run_id
        JOIN operations o ON o.assignment_id=t.assignment_id JOIN audit_execution e ON e.operation_id=o.id
        WHERE t.run_id=? AND t.assignment_id=? AND o.id=? AND e.thread_id=? AND o.lease_owner=?
        AND t.owner_generation=? AND r.stop_generation=? AND o.lease_expires_at_ms>?
        AND v.current_assignment_id=t.assignment_id AND t.disposition='active' AND t.output_cutoff IS NULL AND t.capture_failure IS NULL AND t.attribution_failure IS NULL
        AND r.control_intent='active' AND r.phase IN ('snapshot_bound','reviewing')
        AND o.state IN ('dispatching','accepted')
        AND NOT EXISTS(SELECT 1 FROM audit_capacity_releases c WHERE c.operation_id=o.id))")
        .bind(&identity.run_id).bind(&identity.assignment_id).bind(&identity.operation_id).bind(&identity.execution_id).bind(&identity.owner)
        .bind(identity.owner_generation).bind(identity.run_stop_generation).bind(now).fetch_one(connection).await?)
}

impl AuditStore {
    pub(in crate::audex) async fn run_stop_generation(
        &self,
        run: &str,
    ) -> Result<i64, AuditStoreError> {
        Ok(
            sqlx::query_scalar("SELECT stop_generation FROM audit_runs WHERE id=?")
                .bind(run)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub(in crate::audex) async fn renew_execution_lease(
        &self,
        identity: &AdmissionIdentity,
        now: i64,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Pool/writer waits must not let a pre-wait timestamp revive an expired owner.
        let now = now.max(now_millis());
        let active = owner_active(&mut tx, identity, now).await?;
        if active {
            sqlx::query("UPDATE operations SET lease_expires_at_ms=? WHERE id=? AND lease_owner=?")
                .bind(now + 30_000)
                .bind(&identity.operation_id)
                .bind(&identity.owner)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(active)
    }

    pub(in crate::audex) async fn admit_generation(
        &self,
        identity: &AdmissionIdentity,
        request: &str,
        transport: &str,
        now: i64,
    ) -> Result<(), AuditStoreError> {
        validate_nonempty("logical request identity", request, MAX_IDENTIFIER_BYTES)?;
        validate_nonempty("generation transport", transport, 128)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Pool/writer waits must not let a pre-wait timestamp revive an expired owner.
        let now = now.max(now_millis());
        if !owner_active(&mut tx, identity, now).await? {
            return Err(AuditStoreError::InvalidTransition(
                "generation owner is expired, stopped or superseded".to_owned(),
            ));
        }
        let submitted:bool=sqlx::query_scalar("SELECT submission_boundary IN ('submission_started','accepted') FROM operations WHERE id=?")
            .bind(&identity.operation_id).fetch_one(&mut *tx).await?;
        if !submitted {
            return Err(AuditStoreError::InvalidTransition(
                "execution has not crossed its initial submission boundary".to_owned(),
            ));
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM audit_request_admissions WHERE request_id=?)",
        )
        .bind(request)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            return Err(AuditStoreError::InvalidTransition(
                "logical generation was already admitted; replay is forbidden".to_owned(),
            ));
        }
        let input: String = sqlx::query_scalar(
            "SELECT request_json FROM audit_attempt_inputs WHERE assignment_id=?",
        )
        .bind(&identity.assignment_id)
        .fetch_one(&mut *tx)
        .await?;
        let input: serde_json::Value = serde_json::from_str(&input)?;
        let expected = if input["requireServedIdentity"] == true
            || matches!(transport, "native_http" | "native_websocket")
        {
            let ready:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM audit_request_admissions WHERE assignment_id=? AND (identity_state NOT IN ('legacy','verified') OR (expected_model IS NOT NULL AND COALESCE(model_scope,'')<>'generation')))")
                .bind(&identity.assignment_id).fetch_one(&mut *tx).await?;
            if !ready {
                return Err(AuditStoreError::InvalidTransition(
                    "prior generation attribution is incomplete".to_owned(),
                ));
            }
            Some(
                input["expectedServedModel"]
                    .as_str()
                    .or_else(|| input["resolvedModel"].as_str())
                    .or_else(|| input["model"].as_str())
                    .ok_or_else(|| {
                        AuditStoreError::InvalidPersistedState(
                            "missing retained model binding".to_owned(),
                        )
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        sqlx::query("INSERT INTO audit_request_admissions(request_id,assignment_id,operation_id,execution_id,owner,owner_generation,run_stop_generation,transport,admitted_at_ms,expected_model,identity_state) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
            .bind(request).bind(&identity.assignment_id).bind(&identity.operation_id).bind(&identity.execution_id).bind(&identity.owner)
            .bind(identity.owner_generation).bind(identity.run_stop_generation).bind(transport).bind(now).bind(&expected).bind(if expected.is_some() { "pending" } else { "legacy" }).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A best-effort final fence. This records a possible wire attempt, never a receipt.
    pub(in crate::audex) async fn mark_generation_send(
        &self,
        identity: &AdmissionIdentity,
        request: &str,
        now: i64,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Pool/writer waits must not let a pre-wait timestamp revive an expired owner.
        let now = now.max(now_millis());
        let mut allowed = false;
        if owner_active(&mut tx, identity, now).await? {
            allowed=sqlx::query("UPDATE audit_request_admissions SET send_started_at_ms=? WHERE request_id=? AND assignment_id=? AND execution_id=? AND owner=? AND owner_generation=? AND run_stop_generation=? AND send_started_at_ms IS NULL")
                .bind(now).bind(request).bind(&identity.assignment_id).bind(&identity.execution_id).bind(&identity.owner)
                .bind(identity.owner_generation).bind(identity.run_stop_generation).execute(&mut *tx).await?.rows_affected()==1;
        }
        tx.commit().await?;
        Ok(allowed)
    }

    /// Receipts may arrive after a stop. They never change accepted output or selection.
    pub(in crate::audex) async fn confirm_generation(
        &self,
        identity: &AdmissionIdentity,
        request: &str,
        response: Option<&str>,
        now: i64,
    ) -> Result<(), AuditStoreError> {
        if let Some(response) = response {
            validate_nonempty("provider response identity", response, MAX_IDENTIFIER_BYTES)?;
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row=sqlx::query("SELECT provider_response_id,send_started_at_ms FROM audit_request_admissions WHERE request_id=? AND assignment_id=? AND execution_id=? AND owner=? AND owner_generation=?")
            .bind(request).bind(&identity.assignment_id).bind(&identity.execution_id).bind(&identity.owner).bind(identity.owner_generation).fetch_one(&mut *tx).await?;
        let previous: Option<String> = row.try_get("provider_response_id")?;
        if row
            .try_get::<Option<i64>, _>("send_started_at_ms")?
            .is_none()
            || previous
                .as_deref()
                .zip(response)
                .is_some_and(|(a, b)| a != b)
        {
            return Err(AuditStoreError::InvalidPersistedState(
                "provider receipt does not match the admitted request".to_owned(),
            ));
        }
        sqlx::query("UPDATE audit_request_admissions SET confirmed_at_ms=COALESCE(confirmed_at_ms,?),provider_response_id=COALESCE(provider_response_id,?) WHERE request_id=?")
            .bind(now).bind(response).bind(request).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The same writer orders model disqualification with admission and operator controls.
    /// Capture remains available: failed attribution must not turn received text into findings.
    pub(in crate::audex) async fn attribute_generation(
        &self,
        identity: &AdmissionIdentity,
        request: &str,
        model: Option<&str>,
        finished: bool,
    ) -> Result<(), AuditStoreError> {
        self.attribute_model(identity, request, model, finished, true)
            .await
    }

    pub(in crate::audex) async fn check_connection_model(
        &self,
        identity: &AdmissionIdentity,
        request: &str,
        model: &str,
    ) -> Result<(), AuditStoreError> {
        self.attribute_model(identity, request, Some(model), false, false)
            .await
    }

    async fn attribute_model(
        &self,
        identity: &AdmissionIdentity,
        request: &str,
        model: Option<&str>,
        finished: bool,
        qualifying: bool,
    ) -> Result<(), AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row=sqlx::query("SELECT expected_model,observed_model,identity_state,model_scope FROM audit_request_admissions WHERE request_id=? AND assignment_id=? AND execution_id=? AND owner=? AND owner_generation=? AND send_started_at_ms IS NOT NULL")
            .bind(request).bind(&identity.assignment_id).bind(&identity.execution_id).bind(&identity.owner).bind(identity.owner_generation).fetch_one(&mut *tx).await?;
        let expected: Option<String> = row.try_get("expected_model")?;
        let Some(expected) = expected else {
            tx.commit().await?;
            return Ok(());
        };
        let mut observed: Option<String> = row.try_get("observed_model")?;
        let previous: String = row.try_get("identity_state")?;
        let mut scope: Option<String> = row.try_get("model_scope")?;
        let mut failure = matches!(previous.as_str(), "missing" | "mismatch");
        if let Some(model) = model {
            // Bound hostile metadata before retaining it; still durably fail the generation.
            if model.is_empty() || model.len() > 1024 {
                failure = true;
            } else {
                sqlx::query("INSERT INTO audit_model_observations(request_id,ordinal,model,evidence_source) SELECT ?,COALESCE(MAX(ordinal),0)+1,?,? FROM audit_model_observations WHERE request_id=?")
                    .bind(request).bind(model).bind(if qualifying {"native_transport_response"}else{"native_connection"}).bind(request).execute(&mut *tx).await?;
                scope = Some(
                    if qualifying {
                        "generation"
                    } else {
                        "connection"
                    }
                    .to_owned(),
                );
                failure |=
                    model != expected || observed.as_deref().is_some_and(|value| value != model);
                observed = Some(model.to_owned());
            }
        }
        let state = if failure {
            "mismatch"
        } else if finished && (observed.is_none() || scope.as_deref() != Some("generation")) {
            "missing"
        } else if (finished || previous == "verified") && scope.as_deref() == Some("generation") {
            "verified"
        } else if observed.is_some() && scope.as_deref() == Some("generation") {
            "observed"
        } else {
            "pending"
        };
        let failed = matches!(state, "missing" | "mismatch");
        sqlx::query("UPDATE audit_request_admissions SET observed_model=?,identity_state=?,model_scope=? WHERE request_id=?")
            .bind(&observed).bind(state).bind(&scope).bind(request).execute(&mut *tx).await?;
        if failed {
            sqlx::query("UPDATE audit_attempts SET attribution_failure=COALESCE(attribution_failure,?),revision=revision+1 WHERE assignment_id=?")
                .bind(format!("served_model_{state}")).bind(&identity.assignment_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if failed {
            return Err(AuditStoreError::InvalidTransition(format!(
                "served model {state}"
            )));
        }
        Ok(())
    }

    pub(in crate::audex) async fn retained_selection(
        &self,
        assignment: &str,
    ) -> Result<serde_json::Value, AuditStoreError> {
        let retained: Option<(String, String)> = sqlx::query_as(
            "SELECT request_json,input_digest FROM audit_attempt_inputs WHERE assignment_id=?",
        )
        .bind(assignment)
        .fetch_optional(&self.pool)
        .await?;
        match retained {
            Some((input, digest)) => {
                if sha256_hex(input.as_bytes()) != digest {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "retained reviewer input digest mismatch".to_owned(),
                    ));
                }
                // Pre-roster historical inputs may not have been JSON.
                Ok(serde_json::from_str(&input).unwrap_or(serde_json::Value::Null))
            }
            None => Ok(serde_json::Value::Null),
        }
    }

    pub(in crate::audex) async fn model_evidence(
        &self,
        assignment: &str,
    ) -> Result<Vec<codex_app_server_protocol::AuditGenerationIdentity>, AuditStoreError> {
        sqlx::query("SELECT request_id,transport,expected_model,observed_model,identity_state,model_scope FROM audit_request_admissions WHERE assignment_id=? ORDER BY admitted_at_ms,request_id")
            .bind(assignment).fetch_all(&self.pool).await?.into_iter().map(|row|Ok(codex_app_server_protocol::AuditGenerationIdentity {
                request_id:row.try_get("request_id")?,transport:row.try_get("transport")?,expected_model:row.try_get("expected_model")?,observed_model:row.try_get("observed_model")?,state:row.try_get("identity_state")?,evidence_scope:row.try_get("model_scope")?,
            })).collect()
    }

    #[cfg(test)]
    pub(in crate::audex) async fn generation_records(
        &self,
        assignment: &str,
    ) -> Result<Vec<GenerationRecord>, AuditStoreError> {
        sqlx::query("SELECT request_id,execution_id,transport,send_started_at_ms,confirmed_at_ms,provider_response_id FROM audit_request_admissions WHERE assignment_id=? ORDER BY admitted_at_ms,request_id")
            .bind(assignment).fetch_all(&self.pool).await?.into_iter().map(|row|Ok(GenerationRecord {
                request_id:row.try_get("request_id")?,execution_id:row.try_get("execution_id")?,transport:row.try_get("transport")?,
                send_started_at_ms:row.try_get("send_started_at_ms")?,confirmed_at_ms:row.try_get("confirmed_at_ms")?,provider_response_id:row.try_get("provider_response_id")?,
            })).collect()
    }
}
