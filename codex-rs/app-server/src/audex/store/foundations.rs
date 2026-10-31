//! Durable roster commissioning and exact attempt reconstruction over the existing ledger.
use super::*;

pub(in crate::audex) const DEFAULT_WORKERS: u32 = 3;
const MAX_REVIEWERS: usize = 64;

#[derive(Clone)]
pub(in crate::audex) struct ReviewerCommission {
    pub request: CreateRunRequest,
    /// Credential-free request and policy JSON; never a runtime session or history.
    pub input: Option<(String, String)>,
}

pub(super) async fn preflight_upgrade(pool: &SqlitePool) -> Result<(), AuditStoreError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='audit_runs')",
    )
    .fetch_one(pool)
    .await?;
    if !exists {
        return Ok(());
    }
    let upgraded: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version>=8 AND success=1)",
    )
    .fetch_one(pool)
    .await?;
    if !upgraded {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_runs WHERE phase NOT IN ('completed','completed_with_failures','failed','cancelled')")
            .fetch_one(pool).await?;
        if count != 0 {
            return Err(AuditStoreError::InvalidPersistedState(format!(
                "unsupported upgrade: {count} non-terminal legacy audit run(s); retained state was left intact"
            )));
        }
    }
    Ok(())
}

pub(super) async fn update_attempt_revision(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
    revision: i64,
    _now: i64,
) -> Result<(), AuditStoreError> {
    sqlx::query("UPDATE audit_attempts SET revision=? WHERE assignment_id=?")
        .bind(revision)
        .bind(assignment)
        .execute(connection)
        .await?;
    Ok(())
}

pub(super) async fn single_reviewer(
    connection: &mut sqlx::SqliteConnection,
    run: &str,
) -> Result<bool, AuditStoreError> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_reviewers WHERE run_id=?")
            .bind(run)
            .fetch_one(connection)
            .await?
            == 1,
    )
}

impl AuditStore {
    /// Commission the complete ordered roster atomically. This is a foundation
    /// boundary; public roster selection/report presentation is a later slice.
    pub(in crate::audex) async fn create_roster(
        &self,
        commissions: &[ReviewerCommission],
        identity: Option<&str>,
    ) -> Result<CreatedRun, AuditStoreError> {
        if commissions.is_empty() || commissions.len() > MAX_REVIEWERS {
            return Err(AuditStoreError::InvalidInput(format!(
                "reviewer roster must contain 1..{MAX_REVIEWERS} entries"
            )));
        }
        let request = &commissions[0].request;
        for commission in commissions {
            let candidate = &commission.request;
            validate_create_request(candidate)?;
            if candidate.request_key != request.request_key
                || candidate.snapshot_id != request.snapshot_id
                || candidate.manifest_digest != request.manifest_digest
                || candidate.manifest_path != request.manifest_path
                || candidate.brief_digest != request.brief_digest
            {
                return Err(AuditStoreError::InvalidInput(
                    "roster must share one request, sealed snapshot and brief".to_owned(),
                ));
            }
            if let Some((input, policy)) = &commission.input {
                if sha256_hex(input.as_bytes()) != candidate.input_digest
                    || sha256_hex(policy.as_bytes()) != candidate.policy_digest
                {
                    return Err(AuditStoreError::InvalidInput(
                        "retained attempt input/policy digest mismatch".to_owned(),
                    ));
                }
            }
        }
        let now = now_millis();
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) =
            read_created_run_by_request_key(&mut transaction, &request.request_key).await?
        {
            verify_idempotent_create(&mut transaction, request, &existing).await?;
            let retained: Vec<(String, String, Option<String>, String, String, String)> = sqlx::query_as(
                "SELECT a.provider,a.model,a.reasoning_effort,a.reviewer_input_digest,o.input_digest,t.policy_digest
                 FROM audit_reviewers v JOIN reviewer_assignments a ON a.id=v.current_assignment_id
                 JOIN audit_attempts t ON t.assignment_id=a.id
                 JOIN operations o ON o.assignment_id=a.id WHERE v.run_id=? ORDER BY v.ordinal")
                .bind(&existing.run_id).fetch_all(&mut *transaction).await?;
            let wanted: Vec<_> = commissions
                .iter()
                .map(|c| {
                    (
                        c.request.provider.clone(),
                        c.request.model.clone(),
                        c.request.reasoning_effort.clone(),
                        c.request.reviewer_input_digest.clone(),
                        c.request.operation_input_digest.clone(),
                        c.request.policy_digest.clone(),
                    )
                })
                .collect();
            let retained_identity: Option<String> = sqlx::query_scalar(
                "SELECT request_identity_digest FROM audit_inputs WHERE run_id=?",
            )
            .bind(&existing.run_id)
            .fetch_optional(&mut *transaction)
            .await?
            .flatten();
            if retained != wanted || retained_identity.as_deref() != identity {
                return Err(AuditStoreError::IdempotencyConflict(
                    request.request_key.clone(),
                ));
            }
            transaction.commit().await?;
            return Ok(existing);
        }
        let created = CreatedRun {
            run_id: new_id("run"),
            assignment_id: new_id("assignment"),
            operation_id: new_id("operation"),
            revision: 0,
        };
        sqlx::query(
            "INSERT INTO audit_runs (
                id, request_key, snapshot_id, manifest_digest, manifest_path,
                brief_digest, policy_digest, input_digest, phase, control_intent,
                revision, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'created', 'active', 0, ?, ?)",
        )
        .bind(&created.run_id)
        .bind(&request.request_key)
        .bind(&request.snapshot_id)
        .bind(&request.manifest_digest)
        .bind(&request.manifest_path)
        .bind(&request.brief_digest)
        .bind(&request.policy_digest)
        .bind(&request.input_digest)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;

        for (ordinal, commission) in commissions.iter().enumerate() {
            let request = &commission.request;
            let assignment_id = if ordinal == 0 {
                created.assignment_id.clone()
            } else {
                new_id("assignment")
            };
            let operation_id = if ordinal == 0 {
                created.operation_id.clone()
            } else {
                new_id("operation")
            };
            let reviewer_id = new_id("reviewer");
            sqlx::query(
                "INSERT INTO reviewer_assignments (
                id, run_id, ordinal, snapshot_id, provider, model, reasoning_effort,
                reviewer_input_digest, state, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'planned', ?, ?)",
            )
            .bind(&assignment_id)
            .bind(&created.run_id)
            .bind(ordinal as i64)
            .bind(&request.snapshot_id)
            .bind(&request.provider)
            .bind(&request.model)
            .bind(&request.reasoning_effort)
            .bind(&request.reviewer_input_digest)
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO operations (
                id, assignment_id, kind, state, attempt, input_digest,
                created_at_ms, updated_at_ms, submission_boundary
             ) VALUES (?, ?, 'review', 'planned', 0, ?, ?, ?, 'unsubmitted')",
            )
            .bind(&operation_id)
            .bind(&assignment_id)
            .bind(&request.operation_input_digest)
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await?;

            sqlx::query("INSERT INTO audit_reviewers(id,run_id,ordinal,current_assignment_id) VALUES (?,?,?,?)")
                .bind(&reviewer_id).bind(&created.run_id).bind(ordinal as i64).bind(&assignment_id).execute(&mut *transaction).await?;
            sqlx::query(
                "INSERT INTO audit_attempts(assignment_id,reviewer_id,run_id,ordinal,policy_digest) VALUES (?,?,?,1,?)",
            )
            .bind(&assignment_id)
            .bind(&reviewer_id)
            .bind(&created.run_id)
            .bind(&request.policy_digest)
            .execute(&mut *transaction)
            .await?;
            if let Some((input, policy)) = &commission.input {
                sqlx::query("INSERT INTO audit_attempt_inputs(assignment_id,request_json,input_digest,policy_json,policy_digest) VALUES(?,?,?,?,?)")
                    .bind(&assignment_id).bind(input).bind(&request.input_digest).bind(policy).bind(&request.policy_digest)
                    .execute(&mut *transaction).await?;
            }
        }
        if let (Some((request_json, policy_json)), Some(request_identity_digest)) =
            (&commissions[0].input, identity)
        {
            sqlx::query(
                "INSERT INTO audit_inputs(run_id, request_json, policy_json,request_identity_digest) VALUES (?, ?, ?, ?)",
            )
            .bind(&created.run_id)
            .bind(request_json)
            .bind(policy_json)
            .bind(request_identity_digest)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(created)
    }

    pub(in crate::audex) async fn attempts(
        &self,
        run: &str,
    ) -> Result<Vec<AuditRunRecord>, AuditStoreError> {
        let operations: Vec<String> = sqlx::query_scalar(
            "SELECT o.id FROM audit_reviewers v JOIN audit_attempts t ON t.reviewer_id=v.id
             JOIN operations o ON o.assignment_id=t.assignment_id WHERE v.run_id=? ORDER BY v.ordinal,t.ordinal")
            .bind(run).fetch_all(&self.pool).await?;
        let mut attempts = Vec::with_capacity(operations.len());
        for operation in operations {
            attempts.push(self.load_operation_metadata(&operation).await?);
        }
        Ok(attempts)
    }

    pub(in crate::audex) async fn load_operation_metadata(
        &self,
        operation: &str,
    ) -> Result<AuditRunRecord, AuditStoreError> {
        self.load_selected_metadata("", Some(operation)).await
    }

    pub(super) async fn load_selected_metadata(
        &self,
        run_id: &str,
        operation: Option<&str>,
    ) -> Result<AuditRunRecord, AuditStoreError> {
        let row = sqlx::query(
            "SELECT r.id AS run_id, r.snapshot_id, r.phase, r.control_intent, r.revision, r.outcome,
                    a.id AS assignment_id, a.state AS assignment_state,
                    a.result_digest, a.result_path, a.provider, a.model, a.reasoning_effort,
                    v.id AS reviewer_id, v.ordinal AS reviewer_ordinal,
                    t.ordinal AS attempt_ordinal, t.revision AS attempt_revision, t.owner_generation, v.current_assignment_id=a.id AS current,
                    o.id AS operation_id, o.state AS operation_state, o.retry_classification,
                    EXISTS (SELECT 1 FROM audit_capacity_releases c WHERE c.operation_id=o.id) AS capacity_released
             FROM audit_runs r
             JOIN reviewer_assignments a ON a.run_id=r.id
             JOIN operations o ON o.assignment_id=a.id
             JOIN audit_attempts t ON t.assignment_id=a.id
             JOIN audit_reviewers v ON v.id=t.reviewer_id
             WHERE (? IS NULL AND r.id=? AND v.ordinal=0 AND v.current_assignment_id=a.id) OR o.id=?",
        )
        .bind(operation).bind(run_id).bind(operation)
        .fetch_one(&self.pool)
        .await?;
        let assignment_id: String = row.try_get("assignment_id")?;
        let phase = RunPhase::parse(row.try_get("phase")?)?;
        let operation_state = OperationState::parse(row.try_get("operation_state")?)?;
        let run_id: String = row.try_get("run_id")?;
        let record = AuditRunRecord {
            run_id: run_id.to_string(),
            snapshot_id: row.try_get("snapshot_id")?,
            phase,
            control_intent: row.try_get("control_intent")?,
            revision: row.try_get("revision")?,
            reviewer_id: row.try_get("reviewer_id")?,
            reviewer_ordinal: row.try_get("reviewer_ordinal")?,
            attempt_ordinal: row.try_get("attempt_ordinal")?,
            attempt_revision: row.try_get("attempt_revision")?,
            current: row.try_get("current")?,
            owner_generation: row.try_get("owner_generation")?,
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            reasoning_effort: row.try_get("reasoning_effort")?,
            outcome: row.try_get("outcome")?,
            assignment_id: assignment_id.clone(),
            assignment_state: AssignmentState::parse(row.try_get("assignment_state")?)?,
            operation_id: row.try_get("operation_id")?,
            operation_state,
            capacity_released: row.try_get("capacity_released")?,
            observation_pending: !phase.is_terminal()
                && matches!(
                    operation_state,
                    OperationState::Dispatching | OperationState::Accepted
                )
                && row
                    .try_get::<Option<String>, _>("retry_classification")?
                    .as_deref()
                    == Some("observe_only"),
            result_digest: row.try_get("result_digest")?,
            result_path: row.try_get("result_path")?,
            report: self.read_report_record(&run_id).await?,
            findings: Vec::new(),
        };
        Ok(record)
    }

    pub(in crate::audex) async fn attempt_input(
        &self,
        operation: &str,
    ) -> Result<(String, String, String, String), AuditStoreError> {
        let row = sqlx::query("SELECT i.request_json,i.input_digest,i.policy_json,i.policy_digest,r.manifest_digest, o.input_digest AS operation_input_digest,t.policy_digest AS bound_policy_digest
            FROM operations o JOIN reviewer_assignments a ON a.id=o.assignment_id
            JOIN audit_attempt_inputs i ON i.assignment_id=a.id JOIN audit_attempts t ON t.assignment_id=a.id JOIN audit_runs r ON r.id=a.run_id WHERE o.id=?")
            .bind(operation).fetch_one(&self.pool).await?;
        let request: String = row.try_get("request_json")?;
        let digest: String = row.try_get("input_digest")?;
        if sha256_hex(request.as_bytes()) != digest
            || row.try_get::<String, _>("operation_input_digest")? != digest
            || row.try_get::<String, _>("bound_policy_digest")?
                != row.try_get::<String, _>("policy_digest")?
        {
            return Err(AuditStoreError::InvalidPersistedState(
                "retained attempt input digest mismatch".to_owned(),
            ));
        }
        Ok((
            request,
            row.try_get("policy_json")?,
            row.try_get("policy_digest")?,
            row.try_get("manifest_digest")?,
        ))
    }

    pub(in crate::audex) async fn attempt_candidates(
        &self,
    ) -> Result<Vec<(String, i64, i64)>, AuditStoreError> {
        Ok(sqlx::query_as("SELECT o.id,r.revision,t.revision FROM audit_runs r
            JOIN audit_reviewers v ON v.run_id=r.id JOIN audit_attempts t ON t.assignment_id=v.current_assignment_id
            JOIN operations o ON o.assignment_id=t.assignment_id JOIN reviewer_assignments a ON a.id=t.assignment_id
            WHERE r.phase NOT IN ('completed','completed_with_failures','failed','cancelled')
            AND ((t.disposition='active' AND t.output_cutoff IS NULL AND a.state IN ('planned','thread_ready','dispatching','running')) OR (r.phase='reporting' AND v.ordinal=0))
            ORDER BY r.created_at_ms,r.id,v.ordinal,t.ordinal")
            .fetch_all(&self.pool).await?)
    }
}
