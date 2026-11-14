//! Evidence for recovering the original execution and one replacement per reviewer.
use super::*;

pub(super) async fn stopped(
    connection: &mut sqlx::SqliteConnection,
    operation: &str,
) -> Result<bool, AuditStoreError> {
    Ok(sqlx::query_scalar("SELECT submission_boundary='unsubmitted' OR state='committed'
        OR EXISTS(SELECT 1 FROM audit_execution e WHERE e.operation_id=operations.id AND e.stopped_at_ms IS NOT NULL)
        OR EXISTS(SELECT 1 FROM audit_client_launches l WHERE l.operation_id=operations.id AND l.local_stop_confirmed_at_ms IS NOT NULL)
        FROM operations WHERE id=?").bind(operation).fetch_one(connection).await?)
}

pub(super) async fn recovery_refusal(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
) -> Result<Option<&'static str>, AuditStoreError> {
    let row = sqlx::query("SELECT o.id,v.automatic_recovery_used,a.result_path,a.state,t.disposition,
        EXISTS(SELECT 1 FROM audit_activity x WHERE x.assignment_id=t.assignment_id
            AND x.kind IN ('assistant_delta','assistant_message') AND length(x.payload)>0)
        OR EXISTS(SELECT 1 FROM audit_execution e WHERE e.operation_id=o.id AND length(e.recovered_output)>0)
        OR EXISTS(SELECT 1 FROM audit_client_terminal_evidence c WHERE c.operation_id=o.id) AS has_output
        FROM audit_attempts t JOIN audit_reviewers v ON v.id=t.reviewer_id
        JOIN operations o ON o.assignment_id=t.assignment_id JOIN reviewer_assignments a ON a.id=t.assignment_id
        WHERE t.assignment_id=?").bind(assignment).fetch_one(&mut *connection).await?;
    if row.try_get::<String, _>("disposition")? != "active" {
        return Ok(Some("operator_stopped_attempt"));
    }
    if row.try_get::<bool, _>("has_output")?
        || row.try_get::<Option<String>, _>("result_path")?.is_some()
    {
        return Ok(Some("operator_retry_required_surviving_output"));
    }
    if row.try_get::<bool, _>("automatic_recovery_used")? {
        return Ok(Some(
            "automatic_recovery_exhausted_operator_investigation_required",
        ));
    }
    if !matches!(
        row.try_get::<String, _>("state")?.as_str(),
        "failed" | "unknown"
    ) {
        return Ok(Some("original_execution_not_failed"));
    }
    if !stopped(connection, &row.try_get::<String, _>("id")?).await? {
        return Ok(Some("original_execution_stop_unconfirmed"));
    }
    Ok(None)
}

pub(super) async fn require_recoverable_contribution(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
) -> Result<(), AuditStoreError> {
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_attempts t JOIN audit_reviewers v ON v.id=t.reviewer_id JOIN audit_runs r ON r.id=t.run_id WHERE t.assignment_id=? AND t.disposition='active' AND v.current_assignment_id=t.assignment_id AND r.control_intent='active' AND r.collection_ready=0)")
        .bind(assignment).fetch_one(connection).await?;
    if !active {
        return Err(AuditStoreError::InvalidTransition(
            "original contribution was stopped, superseded or frozen".into(),
        ));
    }
    Ok(())
}

impl AuditStore {
    pub(in crate::audex) async fn mark_execution_stopped(
        &self,
        operation: &str,
    ) -> Result<(), AuditStoreError> {
        sqlx::query("UPDATE audit_execution SET stopped_at_ms=COALESCE(stopped_at_ms,?) WHERE operation_id=?")
            .bind(now_millis()).bind(operation).execute(&self.pool).await?;
        Ok(())
    }

    pub(in crate::audex) async fn native_owner_stopped(
        &self,
        operation: &str,
    ) -> Result<bool, AuditStoreError> {
        #[cfg(target_os = "linux")]
        {
            let raw: Option<String> = sqlx::query_scalar(
                "SELECT owner_process_json FROM audit_execution WHERE operation_id=?",
            )
            .bind(operation)
            .fetch_one(&self.pool)
            .await?;
            if let Some(raw) = raw {
                let expected: codex_app_server_daemon::LinuxChildIdentity =
                    serde_json::from_str(&raw)?;
                return Ok(
                    match codex_app_server_daemon::read_linux_process_identity(expected.pid) {
                        Ok(None) => true,
                        Ok(Some(actual)) => {
                            actual.boot_id != expected.boot_id
                                || actual.start_ticks != expected.start_ticks
                        }
                        Err(_) => false,
                    },
                );
            }
        }
        let _ = operation;
        Ok(false)
    }

    pub(in crate::audex) async fn retain_recovered_output(
        &self,
        operation: &str,
        text: &str,
    ) -> Result<(), AuditStoreError> {
        if !text.is_empty() {
            // Each caller supplies one complete recovery projection, never a lone
            // streaming fragment. Re-observation replaces that projection.
            sqlx::query("UPDATE audit_execution SET recovered_output=? WHERE operation_id=? AND COALESCE(recovered_output,'')<>?")
                .bind(text).bind(operation).bind(text).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Keep completed native history and journal-only items, using identities rather than text.
    pub(in crate::audex) async fn retain_recovered_native_output(
        &self,
        operation: &str,
        session: &str,
        turn: &str,
        history: &[(String, String)],
    ) -> Result<(), AuditStoreError> {
        let mut journal: std::collections::VecDeque<_> =
            self.recovered_activity_messages(operation).await?.into();
        let mut messages = Vec::new();
        for (item, text) in history {
            if !item.is_empty()
                && let Some(index) =
                    journal
                        .iter()
                        .position(|((session_id, turn_id, item_id), _)| {
                            session_id == session && turn_id == turn && item_id == item
                        })
            {
                // Preserve journal-only items before this shared history anchor.
                messages.extend(journal.drain(..index).map(|(_, text)| text));
                journal.pop_front();
            }
            if !text.is_empty() {
                // History can contain a complete message missing from capture,
                // or more text than its journal prefix. Never discard it.
                messages.push(text.clone());
            }
        }
        messages.extend(journal.into_iter().map(|(_, text)| text));
        self.retain_recovered_output(operation, &messages.join("\n\n"))
            .await
    }

    pub(in crate::audex) async fn recovered_output(
        &self,
        operation: &str,
    ) -> Result<Option<String>, AuditStoreError> {
        let retained: Option<String> =
            sqlx::query_scalar("SELECT recovered_output FROM audit_execution WHERE operation_id=?")
                .bind(operation)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        if let Some(text) = retained {
            return Ok((!text.is_empty()).then_some(text));
        }
        let text = self
            .recovered_activity_messages(operation)
            .await?
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n");
        // A terminal cutoff makes this projection stable. Cache it once so
        // polling an operator decision does not rescan the complete journal.
        // A concurrently retained native history snapshot takes precedence.
        sqlx::query("UPDATE audit_execution SET recovered_output=? WHERE operation_id=? AND recovered_output IS NULL AND EXISTS(SELECT 1 FROM operations o JOIN audit_attempts t ON t.assignment_id=o.assignment_id JOIN reviewer_assignments a ON a.id=t.assignment_id WHERE o.id=audit_execution.operation_id AND t.output_cutoff IS NOT NULL AND a.state IN ('failed','unknown'))")
            .bind(&text).bind(operation).execute(&self.pool).await?;
        Ok((!text.is_empty()).then_some(text))
    }

    async fn recovered_activity_messages(
        &self,
        operation: &str,
    ) -> Result<Vec<((String, String, String), String)>, AuditStoreError> {
        let assignment: String =
            sqlx::query_scalar("SELECT assignment_id FROM operations WHERE id=?")
                .bind(operation)
                .fetch_one(&self.pool)
                .await?;
        let mut messages = Vec::new();
        let mut items = std::collections::HashMap::new();
        let mut cursor = 0;
        loop {
            let page = self.activity(&assignment, cursor, 256).await?;
            if page.is_empty() {
                break;
            }
            for entry in page {
                cursor = entry.sequence;
                if matches!(entry.kind.as_str(), "assistant_delta" | "assistant_message") {
                    let key = (entry.session_id, entry.turn_id, entry.item_id);
                    let index = *items.entry(key.clone()).or_insert_with(|| {
                        messages.push((key, String::new()));
                        messages.len() - 1
                    });
                    // append_batch already checks byte offsets and stores only
                    // the unseen suffix of a completed message. Joining those
                    // verified records reconstructs each item exactly once.
                    messages[index].1.push_str(&entry.payload);
                }
            }
        }
        messages.retain(|(_, text)| !text.is_empty());
        Ok(messages)
    }

    pub(in crate::audex) async fn recovery_status(
        &self,
        assignment: &str,
    ) -> Result<Option<String>, AuditStoreError> {
        Ok(
            // A historical ceiling refusal no longer describes current eligibility.
            sqlx::query_scalar("SELECT CASE WHEN recovery_status='automatic_recovery_refused_retained_policy_or_limit' THEN 'automatic_recovery_revalidation_required' ELSE NULLIF(recovery_status,'configured_reviewer_limit_blocks_replacement') END FROM audit_attempts WHERE assignment_id=?")
                .bind(assignment)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub(in crate::audex) async fn set_recovery_status(
        &self,
        assignment: &str,
        status: &str,
    ) -> Result<(), AuditStoreError> {
        sqlx::query("UPDATE audit_attempts SET recovery_status=?,revision=revision+1 WHERE assignment_id=? AND COALESCE(recovery_status,'')<>?")
            .bind(status).bind(assignment).bind(status).execute(&self.pool).await?;
        Ok(())
    }

    pub(in crate::audex) async fn recovery_ready(
        &self,
        assignment: &str,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let refusal = recovery_refusal(&mut tx, assignment).await?;
        tx.commit().await?;
        if let Some(reason) = refusal {
            self.set_recovery_status(assignment, reason).await?;
        }
        Ok(refusal.is_none())
    }
}
