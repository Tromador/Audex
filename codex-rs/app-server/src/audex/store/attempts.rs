//! Attempt-scoped operator decisions, atomically ordered with received activity.
use super::activity::{ActivityInput, CaptureIdentity, append_batch};
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::audex) enum AttemptAction {
    Cancel,
    Ignore,
    Retry,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(in crate::audex) struct ControlRequest {
    pub run_id: String,
    pub assignment_id: String,
    pub action_key: String,
    pub action: AttemptAction,
    pub reason: String,
    pub authorise_retry: bool,
    pub acknowledge_possible_overlap: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::audex) struct ControlOutcome {
    pub action_key: String,
    pub assignment_id: String,
    pub disposition: String,
    pub cutoff: Option<i64>,
    pub replacement_assignment_id: Option<String>,
    pub replacement_operation_id: Option<String>,
    pub completed_won: bool,
    pub possible_overlap: bool,
    pub attempt_revision: i64,
    pub output_sequence: i64,
    pub run_revision: i64,
    pub run_phase: String,
}

pub(super) async fn require_active_attempt(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
) -> Result<(), AuditStoreError> {
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_attempts t JOIN audit_reviewers v ON v.id=t.reviewer_id WHERE t.assignment_id=? AND t.disposition='active' AND v.current_assignment_id=t.assignment_id AND t.output_cutoff IS NULL AND t.capture_failure IS NULL)")
        .bind(assignment).fetch_one(connection).await?;
    if !active {
        return Err(AuditStoreError::InvalidTransition(
            "attempt is stopped or superseded".to_owned(),
        ));
    }
    Ok(())
}

impl AuditStore {
    pub(in crate::audex) async fn attempt_by_assignment(
        &self,
        run: &str,
        assignment: &str,
    ) -> Result<AuditRunRecord, AuditStoreError> {
        let operation: String = sqlx::query_scalar("SELECT o.id FROM operations o JOIN reviewer_assignments a ON a.id=o.assignment_id WHERE a.run_id=? AND a.id=?")
            .bind(run).bind(assignment).fetch_one(&self.pool).await?;
        self.load_operation_metadata(&operation).await
    }

    pub(in crate::audex) async fn possible_overlap(
        &self,
        operation: &str,
    ) -> Result<bool, AuditStoreError> {
        Ok(sqlx::query_scalar("SELECT submission_boundary<>'unsubmitted' AND state<>'committed' FROM operations WHERE id=?")
            .bind(operation).fetch_one(&self.pool).await?)
    }

    /// Never waits on a runtime, capture mutex, network stream, or process exit.
    /// The caller can include its available local prefix; another owner can stop
    /// without it and records the possible uncommitted gap instead.
    pub(in crate::audex) async fn control_attempt(
        &self,
        request: &ControlRequest,
        prefix: Option<(&CaptureIdentity, &[ActivityInput])>,
    ) -> Result<ControlOutcome, AuditStoreError> {
        validate_nonempty("action key", &request.action_key, MAX_IDENTIFIER_BYTES)?;
        validate_nonempty("control reason", &request.reason, MAX_ERROR_BYTES)?;
        let digest = sha256_hex(&serde_json::to_vec(request)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some((stored, response)) = sqlx::query_as::<_, (String, String)>(
            "SELECT request_digest,response_json FROM audit_attempt_actions WHERE action_key=?",
        )
        .bind(&request.action_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if stored != digest {
                return Err(AuditStoreError::IdempotencyConflict(
                    request.action_key.clone(),
                ));
            }
            let response = serde_json::from_str(&response)?;
            tx.commit().await?;
            return Ok(response);
        }
        let row = sqlx::query("SELECT t.reviewer_id,t.ordinal,t.revision,t.owner_generation,t.disposition,t.output_cutoff,t.capture_started,t.capture_finished,
            v.current_assignment_id,a.state,r.phase,r.control_intent,r.current_report_id,o.id AS operation_id,o.submission_boundary,o.state AS operation_state
            FROM audit_attempts t JOIN audit_reviewers v ON v.id=t.reviewer_id JOIN reviewer_assignments a ON a.id=t.assignment_id
            JOIN audit_runs r ON r.id=t.run_id JOIN operations o ON o.assignment_id=a.id WHERE t.run_id=? AND t.assignment_id=?")
            .bind(&request.run_id).bind(&request.assignment_id).fetch_one(&mut *tx).await?;
        let current: String = row.try_get("current_assignment_id")?;
        if current != request.assignment_id {
            return Err(AuditStoreError::InvalidTransition(
                "stale control targets a superseded attempt".to_owned(),
            ));
        }
        let operation: String = row.try_get("operation_id")?;
        let disposition: String = row.try_get("disposition")?;
        let assignment_state = AssignmentState::parse(row.try_get("state")?)?;
        let possible_overlap = row.try_get::<String, _>("submission_boundary")? != "unsubmitted"
            && row.try_get::<String, _>("operation_state")? != "committed";
        let mut response = ControlOutcome {
            action_key: request.action_key.clone(),
            assignment_id: request.assignment_id.clone(),
            disposition: disposition.clone(),
            cutoff: row.try_get("output_cutoff")?,
            replacement_assignment_id: None,
            replacement_operation_id: None,
            completed_won: false,
            possible_overlap,
            attempt_revision: 0,
            output_sequence: 0,
            run_revision: 0,
            run_phase: String::new(),
        };
        if request.action != AttemptAction::Retry && assignment_state == AssignmentState::Succeeded
        {
            response.completed_won = true;
        } else {
            if row.try_get::<String, _>("control_intent")? != "active" {
                return Err(AuditStoreError::InvalidTransition(
                    "the audit has a run-wide stop decision".to_owned(),
                ));
            }
            if request.action == AttemptAction::Retry {
                if !request.authorise_retry {
                    return Err(AuditStoreError::InvalidInput(
                        "Retry requires explicit operator authorisation".to_owned(),
                    ));
                }
                if possible_overlap && !request.acknowledge_possible_overlap {
                    return Err(AuditStoreError::InvalidInput("the prior request may remain active and incur overlapping charges; explicit acknowledgement required".to_owned()));
                }
                if row
                    .try_get::<Option<String>, _>("current_report_id")?
                    .is_some()
                    || row.try_get::<String, _>("phase")? == "reporting"
                {
                    return Err(AuditStoreError::InvalidTransition(
                        "a frozen report contribution cannot be replaced".to_owned(),
                    ));
                }
            } else if disposition != "active" {
                let wanted = if request.action == AttemptAction::Cancel {
                    "cancelled"
                } else {
                    "ignored"
                };
                if disposition != wanted {
                    return Err(AuditStoreError::InvalidTransition(
                        "attempt already has a different stop decision".to_owned(),
                    ));
                }
            }
            if request.action == AttemptAction::Retry {
                let retained = sqlx::query("SELECT i.request_json,i.input_digest,i.policy_json,i.policy_digest,o.input_digest AS operation_digest,t.policy_digest AS bound_policy FROM audit_attempt_inputs i JOIN audit_attempts t ON t.assignment_id=i.assignment_id JOIN operations o ON o.assignment_id=i.assignment_id WHERE i.assignment_id=?")
                    .bind(&request.assignment_id).fetch_one(&mut *tx).await?;
                let input_digest: String = retained.try_get("input_digest")?;
                let policy_digest: String = retained.try_get("policy_digest")?;
                if sha256_hex(retained.try_get::<String, _>("request_json")?.as_bytes())
                    != input_digest
                    || retained.try_get::<String, _>("operation_digest")? != input_digest
                    || sha256_hex(retained.try_get::<String, _>("policy_json")?.as_bytes())
                        != policy_digest
                    || retained.try_get::<String, _>("bound_policy")? != policy_digest
                {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "retained Retry input or policy digest mismatch".to_owned(),
                    ));
                }
            }
            let revision: i64 = row.try_get("revision")?;
            if response.cutoff.is_none() {
                let mut local_prefix = false;
                if let Some((identity, entries)) = prefix {
                    if identity.assignment_id != request.assignment_id {
                        return Err(AuditStoreError::InvalidInput(
                            "control prefix belongs to another attempt".to_owned(),
                        ));
                    }
                    sqlx::query("SAVEPOINT audex_control_prefix")
                        .execute(&mut *tx)
                        .await?;
                    match append_batch(&mut tx, identity, entries).await {
                        Ok(appended) => {
                            local_prefix = !appended.fenced;
                            sqlx::query("RELEASE audex_control_prefix")
                                .execute(&mut *tx)
                                .await?;
                        }
                        Err(AuditStoreError::Database(error)) => return Err(error.into()),
                        Err(_) => {
                            sqlx::query("ROLLBACK TO audex_control_prefix")
                                .execute(&mut *tx)
                                .await?;
                            sqlx::query("RELEASE audex_control_prefix")
                                .execute(&mut *tx)
                                .await?;
                            sqlx::query("UPDATE audit_attempts SET capture_gap=COALESCE(capture_gap,'local capture prefix could not be retained at control') WHERE assignment_id=?")
                                .bind(&request.assignment_id).execute(&mut *tx).await?;
                        }
                    }
                }
                if !local_prefix
                    && row.try_get::<bool, _>("capture_started")?
                    && !row.try_get::<bool, _>("capture_finished")?
                {
                    sqlx::query("UPDATE audit_attempts SET capture_gap=COALESCE(capture_gap,'collector unavailable at control; uncommitted activity may be missing') WHERE assignment_id=?")
                        .bind(&request.assignment_id).execute(&mut *tx).await?;
                }
                sqlx::query("UPDATE audit_attempts SET output_cutoff=output_sequence,capture_finished=1 WHERE assignment_id=?")
                    .bind(&request.assignment_id).execute(&mut *tx).await?;
            }
            let next_disposition = match request.action {
                AttemptAction::Cancel => "cancelled",
                AttemptAction::Ignore => "ignored",
                AttemptAction::Retry => "superseded",
            };
            if disposition == "active" || request.action == AttemptAction::Retry {
                sqlx::query("UPDATE audit_attempts SET disposition=?,revision=revision+1,owner_generation=owner_generation+1 WHERE assignment_id=?")
                    .bind(next_disposition).bind(&request.assignment_id).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO audit_capacity_releases(operation_id,consent_revision,reason,created_at_ms) VALUES(?,?,?,?) ON CONFLICT(operation_id) DO NOTHING")
                .bind(&operation).bind(revision).bind(&request.reason).bind(now_millis()).execute(&mut *tx).await?;
            response.disposition = next_disposition.to_owned();
            response.cutoff = sqlx::query_scalar(
                "SELECT output_cutoff FROM audit_attempts WHERE assignment_id=?",
            )
            .bind(&request.assignment_id)
            .fetch_one(&mut *tx)
            .await?;
            if request.action == AttemptAction::Retry {
                let reviewer: String = row.try_get("reviewer_id")?;
                let ordinal: i64 = row.try_get("ordinal")?;
                let assignment = new_id("assignment");
                let replacement = new_id("operation");
                let now = now_millis();
                sqlx::query("INSERT INTO reviewer_assignments(id,run_id,ordinal,snapshot_id,provider,model,reasoning_effort,reviewer_input_digest,state,created_at_ms,updated_at_ms)
                    SELECT ?,run_id,(SELECT COALESCE(MAX(ordinal),-1)+1 FROM reviewer_assignments WHERE run_id=?),snapshot_id,provider,model,reasoning_effort,reviewer_input_digest,'planned',?,?
                    FROM reviewer_assignments WHERE id=?")
                    .bind(&assignment).bind(&request.run_id).bind(now).bind(now).bind(&request.assignment_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO operations(id,assignment_id,kind,state,attempt,input_digest,submission_boundary,created_at_ms,updated_at_ms)
                    SELECT ?,?,'review','planned',0,input_digest,'unsubmitted',?,? FROM operations WHERE id=?")
                    .bind(&replacement).bind(&assignment).bind(now).bind(now).bind(&operation).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO audit_attempts(assignment_id,reviewer_id,run_id,ordinal,predecessor_assignment_id,revision,policy_digest)
                    SELECT ?,reviewer_id,run_id,?,assignment_id,CASE WHEN (SELECT phase FROM audit_runs WHERE id=run_id)='created' THEN 0 ELSE 1 END,policy_digest FROM audit_attempts WHERE assignment_id=?")
                    .bind(&assignment).bind(ordinal+1).bind(&request.assignment_id).execute(&mut *tx).await?;
                let copied = sqlx::query("INSERT INTO audit_attempt_inputs(assignment_id,request_json,input_digest,policy_json,policy_digest)
                    SELECT ?,request_json,input_digest,policy_json,policy_digest FROM audit_attempt_inputs WHERE assignment_id=?")
                    .bind(&assignment).bind(&request.assignment_id).execute(&mut *tx).await?.rows_affected();
                if copied != 1 {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "retained input is unavailable for Retry".to_owned(),
                    ));
                }
                sqlx::query("UPDATE audit_reviewers SET current_assignment_id=? WHERE id=? AND current_assignment_id=?")
                    .bind(&assignment).bind(&reviewer).bind(&request.assignment_id).execute(&mut *tx).await?;
                // Reopening a terminal single-review audit is genuinely run-wide.
                sqlx::query("UPDATE audit_runs SET phase='reviewing',outcome=NULL,revision=revision+1,updated_at_ms=? WHERE id=? AND phase IN ('failed','cancelled','completed_with_failures')")
                    .bind(now).bind(&request.run_id).execute(&mut *tx).await?;
                response.replacement_assignment_id = Some(assignment);
                response.replacement_operation_id = Some(replacement);
            } else if single_reviewer(&mut tx, &request.run_id).await? {
                sqlx::query("UPDATE audit_runs SET phase='cancelled',outcome='no_completed_review',revision=revision+1,updated_at_ms=? WHERE id=? AND phase NOT IN ('completed','cancelled')")
                    .bind(now_millis()).bind(&request.run_id).execute(&mut *tx).await?;
            }
        }
        let versions: (i64,i64,i64,String) = sqlx::query_as("SELECT t.revision,t.output_sequence,r.revision,r.phase FROM audit_attempts t JOIN audit_runs r ON r.id=t.run_id WHERE t.assignment_id=?")
            .bind(&request.assignment_id).fetch_one(&mut *tx).await?;
        response.attempt_revision = versions.0;
        response.output_sequence = versions.1;
        response.run_revision = versions.2;
        response.run_phase = versions.3;
        sqlx::query("INSERT INTO audit_attempt_actions(action_key,assignment_id,request_digest,response_json,created_at_ms) VALUES(?,?,?,?,?)")
            .bind(&request.action_key).bind(&request.assignment_id).bind(&digest).bind(serde_json::to_string(&response)?).bind(now_millis()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(response)
    }
}
