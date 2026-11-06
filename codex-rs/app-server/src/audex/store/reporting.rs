//! Run-level readiness, immutable selections and publication share the store's
//! existing immediate-transaction authority with attempt controls and results.
use super::*;
use crate::audex::report::collection::{self, CollectionInput, OpinionInput, ReviewerInput};

pub(super) async fn settle_collection(
    connection: &mut sqlx::SqliteConnection,
    run: &str,
) -> Result<(), AuditStoreError> {
    sqlx::query("UPDATE audit_runs SET collection_ready=1,phase='reporting',outcome=NULL,revision=revision+1,updated_at_ms=?
        WHERE id=? AND collection_ready=0 AND control_intent='active' AND current_report_id IS NULL
        AND EXISTS(SELECT 1 FROM audit_reviewers WHERE run_id=audit_runs.id)
        AND NOT EXISTS(SELECT 1 FROM audit_reviewers v JOIN audit_attempts t ON t.assignment_id=v.current_assignment_id
            JOIN reviewer_assignments a ON a.id=t.assignment_id
            WHERE v.run_id=audit_runs.id AND t.disposition='active' AND a.state<>'succeeded')")
        .bind(now_millis()).bind(run).execute(connection).await?;
    Ok(())
}

fn content(
    run: &str,
    snapshot: &str,
    assignment: &str,
    summary: String,
    findings: Vec<FindingRecord>,
) -> report::ReportInput {
    report::ReportInput {
        run_id: run.to_owned(),
        snapshot_id: snapshot.to_owned(),
        assignment_id: assignment.to_owned(),
        summary,
        findings: findings
            .into_iter()
            .map(|f| report::ReportFinding {
                id: f.id,
                ordinal: f.ordinal,
                severity: f.severity,
                confidence: f.confidence,
                title: f.title,
                mechanism: f.mechanism,
                impact: f.impact,
                remediation: f.remediation,
                verification_state: f.verification_state,
                evidence: f
                    .evidence
                    .into_iter()
                    .map(|e| report::ReportEvidence {
                        lines_verified: e.lines_verified,
                        ordinal: e.ordinal,
                        description: e.description,
                        manifest_path: e.manifest_path,
                        start_byte: e.start_byte,
                        end_byte: e.end_byte,
                        start_line: e.start_line,
                        end_line: e.end_line,
                    })
                    .collect(),
            })
            .collect(),
    }
}

impl AuditStore {
    /// Commit the audit-wide decision and all contribution cutoffs atomically.
    /// Prefix snapshots never require a collector flush or runtime join.
    pub(in crate::audex) async fn abort_run(
        &self,
        run: &str,
        prefixes: &[(activity::CaptureIdentity, Vec<activity::ActivityInput>)],
    ) -> Result<i64, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        #[cfg(test)]
        self.wait_transition_for_test("abort").await;
        let row = sqlx::query(
            "SELECT current_report_id,control_intent,revision,phase FROM audit_runs WHERE id=?",
        )
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        let revision: i64 = row.try_get("revision")?;
        // Publication winning first is final. Replays never supersede twice.
        if row
            .try_get::<Option<String>, _>("current_report_id")?
            .is_some()
            || row.try_get::<String, _>("control_intent")? == "cancel_requested"
            || matches!(
                row.try_get::<String, _>("phase")?.as_str(),
                "completed" | "completed_with_failures" | "failed" | "cancelled"
            )
        {
            tx.commit().await?;
            return Ok(revision);
        }
        let rows = sqlx::query("SELECT t.assignment_id,t.revision,t.disposition,t.output_cutoff,a.state,o.id AS operation_id
            FROM audit_reviewers v JOIN audit_attempts t ON t.assignment_id=v.current_assignment_id
            JOIN reviewer_assignments a ON a.id=t.assignment_id JOIN operations o ON o.assignment_id=a.id WHERE v.run_id=? ORDER BY v.ordinal")
            .bind(run).fetch_all(&mut *tx).await?;
        for row in rows {
            if row.try_get::<String, _>("state")? == "succeeded" {
                continue;
            }
            let assignment: String = row.try_get("assignment_id")?;
            if row.try_get::<Option<i64>, _>("output_cutoff")?.is_none() {
                let prefix = prefixes
                    .iter()
                    .find(|(identity, _)| identity.assignment_id == assignment)
                    .map(|(identity, entries)| (identity, entries.as_slice()));
                attempts::retain_control_prefix(&mut tx, &assignment, prefix).await?;
            }
            sqlx::query("UPDATE audit_attempts SET disposition=CASE WHEN disposition='active' THEN 'cancelled' ELSE disposition END,revision=revision+1,owner_generation=owner_generation+1 WHERE assignment_id=?")
                .bind(&assignment).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO audit_capacity_releases(operation_id,consent_revision,reason,created_at_ms) VALUES(?,?,'whole-audit Abort',?) ON CONFLICT(operation_id) DO NOTHING")
                .bind(row.try_get::<String,_>("operation_id")?).bind(row.try_get::<i64,_>("revision")?).bind(now_millis()).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE audit_runs SET control_intent='cancel_requested',stop_generation=stop_generation+1,collection_ready=1,phase='reporting',outcome=NULL,authoritative_selection_id=NULL,revision=revision+1,updated_at_ms=? WHERE id=?")
            .bind(now_millis()).bind(run).execute(&mut *tx).await?;
        // Freeze under the same authority as supersession; a previous normal
        // selection remains retained but can no longer publish.
        self.select_report(&mut tx, run).await?;
        tx.commit().await?;
        Ok(revision + 1)
    }

    async fn select_report(
        &self,
        connection: &mut sqlx::SqliteConnection,
        run: &str,
    ) -> Result<CollectionInput, AuditStoreError> {
        let row = sqlx::query("SELECT snapshot_id,manifest_digest,brief_digest,collection_ready,control_intent,selection_generation,authoritative_selection_id,current_report_id,phase FROM audit_runs WHERE id=?")
            .bind(run).fetch_one(&mut *connection).await?;
        if let Some(id) = row.try_get::<Option<String>, _>("authoritative_selection_id")? {
            return self.load_selection(connection, &id).await;
        }
        if !row.try_get::<bool, _>("collection_ready")?
            || row.try_get::<String, _>("phase")? != "reporting"
            || row
                .try_get::<Option<String>, _>("current_report_id")?
                .is_some()
        {
            return Err(AuditStoreError::InvalidTransition(
                "collection is not ready for selection".to_owned(),
            ));
        }
        let generation = row.try_get::<i64, _>("selection_generation")? + 1;
        let snapshot: String = row.try_get("snapshot_id")?;
        let rows = sqlx::query("SELECT v.id AS reviewer_id,v.ordinal AS reviewer_ordinal,t.ordinal AS attempt_ordinal,t.disposition,
            t.output_cutoff,t.capture_gap,t.capture_failure,t.capture_truncated,t.policy_digest,
            a.id,a.ordinal,a.provider,a.model,a.state,a.failure_code,a.result_path,a.result_digest,a.summary,o.id AS operation_id,o.input_digest,c.reason AS stop_reason
            FROM audit_reviewers v JOIN audit_attempts t ON t.assignment_id=v.current_assignment_id
            JOIN reviewer_assignments a ON a.id=t.assignment_id JOIN operations o ON o.assignment_id=a.id
            LEFT JOIN audit_capacity_releases c ON c.operation_id=o.id
            WHERE v.run_id=? ORDER BY v.ordinal")
            .bind(run).fetch_all(&mut *connection).await?;
        let mut reviewers = Vec::with_capacity(rows.len());
        for row in rows {
            let assignment: String = row.try_get("id")?;
            let state: String = row.try_get("state")?;
            let completed = state == "succeeded";
            let cutoff: Option<i64> = row.try_get("output_cutoff")?;
            let cutoff = cutoff.ok_or_else(|| {
                AuditStoreError::InvalidPersistedState(
                    "selected contribution has no cutoff".to_owned(),
                )
            })?;
            let result_path: Option<String> = row.try_get("result_path")?;
            let result_digest: Option<String> = row.try_get("result_digest")?;
            let extraction = super::extraction::read(connection, &assignment).await?;
            let unstructured = extraction.as_ref().is_some_and(|e| {
                e.status == codex_app_server_protocol::AuditExtractionStatus::UnstructuredOpinion
            });
            let opinion = if let Some(extraction) = extraction {
                let text = if unstructured {
                    let path = result_path.as_deref().ok_or_else(|| {
                        AuditStoreError::InvalidPersistedState("opinion has no raw path".into())
                    })?;
                    let bytes =
                        verify_immutable(self.root.as_path(), path, &extraction.raw_digest)?;
                    Some(String::from_utf8(bytes).map_err(|_| {
                        AuditStoreError::InvalidPersistedState("opinion is not UTF-8".into())
                    })?)
                } else {
                    None
                };
                Some(OpinionInput { extraction, text })
            } else {
                None
            };
            // Tool outcomes are independent of opinion acceptance. Freeze the
            // same bounded prefix for completed and incomplete attempts. Integrity
            // is checked in load_selection, preserving Abort's durable decision.
            let mut activity = Vec::new();
            let mut after = 0;
            while after < cutoff {
                let entries = activity::read_selected_activity(
                    connection,
                    &assignment,
                    after,
                    256,
                    cutoff,
                    false,
                )
                .await?;
                let Some(last) = entries.last() else {
                    break;
                };
                after = last.sequence;
                activity.extend(entries);
            }
            let source_tools = crate::audex::report::source_tools::collect(&activity);
            let body = if completed && !unstructured {
                Some(content(
                    run,
                    &snapshot,
                    &assignment,
                    row.try_get("summary")?,
                    read_findings(connection, &assignment).await?,
                ))
            } else {
                None
            };
            let model: String = row.try_get("model")?;
            let retained = super::attribution::retained(connection, &assignment).await?;
            let subscription = retained.as_ref().is_some_and(|input| {
                input.route()
                    == codex_app_server_protocol::AuditAuthenticationRoute::ClaudeSubscription
            });
            let evidence = sqlx::query(if subscription {
                "SELECT expected_model,observed_model,identity_state,'message' AS model_scope FROM audit_client_model_observations WHERE assignment_id=? ORDER BY execution_id,message_id"
            } else {
                "SELECT expected_model,observed_model,identity_state,model_scope FROM audit_request_admissions WHERE assignment_id=? ORDER BY admitted_at_ms,request_id"
            }).bind(&assignment).fetch_all(&mut *connection).await?;
            let verified = !evidence.is_empty()
                && evidence.iter().all(|e| {
                    e.get::<String, _>("identity_state") == "verified"
                        && e.get::<Option<String>, _>("model_scope").as_deref()
                            == Some(if subscription {
                                "message"
                            } else {
                                "generation"
                            })
                });
            let observed_model = evidence
                .iter()
                .rev()
                .find_map(|e| e.get::<Option<String>, _>("observed_model"));
            let mut caveats = Vec::new();
            if evidence
                .iter()
                .any(|e| e.get::<Option<String>, _>("model_scope").as_deref() == Some("connection"))
            {
                caveats.push("Connection model metadata alone does not establish per-generation served identity.".to_owned());
            }
            if !verified {
                caveats.push(
                    "Served-model identity was not verified for this contribution.".to_owned(),
                );
            }
            if let Some(expected) = evidence
                .first()
                .and_then(|e| e.get::<Option<String>, _>("expected_model"))
                && expected != model
            {
                caveats.push(format!(
                    "Explicit commissioned served-model alias: {model} -> {expected}"
                ));
            }
            if evidence.iter().any(|e| {
                matches!(
                    e.get::<String, _>("identity_state").as_str(),
                    "missing" | "mismatch"
                )
            }) {
                caveats.push("Attribution failed; received material is unvalidated history, not accepted findings.".to_owned());
            }
            let provider_facts =
                super::report_facts::collect(connection, &assignment, retained.as_ref()).await?;
            reviewers.push(ReviewerInput {
                reviewer_id: row.try_get("reviewer_id")?,
                reviewer_ordinal: row.try_get("reviewer_ordinal")?,
                assignment_id: assignment,
                assignment_ordinal: row.try_get("ordinal")?,
                attempt_ordinal: row.try_get("attempt_ordinal")?,
                operation_id: row.try_get("operation_id")?,
                provider: row.try_get("provider")?,
                requested_model: retained
                    .as_ref()
                    .map(|i| i.model.clone())
                    .unwrap_or_else(|| model.clone()),
                resolved_model: retained
                    .as_ref()
                    .and_then(|i| i.resolved_model.clone())
                    .unwrap_or(model),
                observed_model,
                observation_verified: verified,
                provider_caveats: caveats,
                input_digest: row.try_get("input_digest")?,
                policy_digest: row.try_get("policy_digest")?,
                disposition: row.try_get("disposition")?,
                assignment_state: state,
                failure_code: row.try_get("failure_code")?,
                stop_reason: row.try_get("stop_reason")?,
                result_path,
                result_digest,
                completed: body,
                opinion,
                cutoff,
                activity_digest: sha256_hex(&serde_json::to_vec(&activity)?),
                activity,
                capture_gap: row.try_get("capture_gap")?,
                capture_failure: row.try_get("capture_failure")?,
                capture_truncated: row.try_get("capture_truncated")?,
                provider_facts: Some(provider_facts),
                source_tools: Some(source_tools),
            });
        }
        let outcome = if row.try_get::<String, _>("control_intent")? == "cancel_requested" {
            "aborted_by_operator"
        } else if reviewers
            .iter()
            .any(|r| r.completed.is_some() || r.opinion.is_some())
        {
            "success"
        } else {
            "no_completed_review"
        };
        let input = CollectionInput {
            format_version: collection::FORMAT_VERSION,
            selection_id: format!("{run}:selection:{generation}"),
            generation,
            run_id: run.to_owned(),
            snapshot_id: snapshot,
            manifest_digest: row.try_get("manifest_digest")?,
            brief_digest: row.try_get("brief_digest")?,
            outcome: outcome.to_owned(),
            reviewers,
        };
        let json = serde_json::to_string(&input)?;
        sqlx::query("INSERT INTO audit_report_selections(id,run_id,generation,input_json,input_digest) VALUES(?,?,?,?,?)")
            .bind(&input.selection_id).bind(run).bind(generation).bind(&json).bind(sha256_hex(json.as_bytes())).execute(&mut *connection).await?;
        sqlx::query(
            "UPDATE audit_runs SET authoritative_selection_id=?,selection_generation=? WHERE id=?",
        )
        .bind(&input.selection_id)
        .bind(generation)
        .bind(run)
        .execute(connection)
        .await?;
        Ok(input)
    }

    async fn load_selection(
        &self,
        connection: &mut sqlx::SqliteConnection,
        id: &str,
    ) -> Result<CollectionInput, AuditStoreError> {
        let (json, digest): (String, String) = sqlx::query_as(
            "SELECT input_json,input_digest FROM audit_report_selections WHERE id=?",
        )
        .bind(id)
        .fetch_one(&mut *connection)
        .await?;
        if sha256_hex(json.as_bytes()) != digest {
            return Err(AuditStoreError::InvalidPersistedState(
                "frozen selection digest mismatch".to_owned(),
            ));
        }
        let input = collection::decode(&json)?;
        if input.selection_id != id {
            return Err(AuditStoreError::InvalidPersistedState(
                "frozen selection identity mismatch".to_owned(),
            ));
        }
        for reviewer in &input.reviewers {
            if (reviewer.completed.is_some() || reviewer.opinion.is_some())
                && (reviewer.result_path.is_none() || reviewer.result_digest.is_none())
            {
                return Err(AuditStoreError::InvalidPersistedState(
                    "completed selection has no canonical result".to_owned(),
                ));
            }
            if let (Some(path), Some(digest)) = (&reviewer.result_path, &reviewer.result_digest) {
                verify_immutable(self.root.as_path(), path, digest)?;
            }
            if reviewer.source_tools.is_some()
                || (reviewer.completed.is_none() && reviewer.opinion.is_none())
            {
                let mut entries = Vec::new();
                while entries.len() < reviewer.cutoff as usize {
                    entries.extend(
                        activity::verified_activity(
                            connection,
                            &reviewer.assignment_id,
                            entries.len() as i64,
                            256,
                            reviewer.cutoff,
                        )
                        .await?,
                    );
                }
                if sha256_hex(&serde_json::to_vec(&entries)?) != reviewer.activity_digest
                    || entries != reviewer.activity
                {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "selected activity prefix mismatch".to_owned(),
                    ));
                }
            }
        }
        Ok(input)
    }

    pub(super) async fn verify_published_selection(
        &self,
        run: &str,
        report: &ReportRecord,
    ) -> Result<(), AuditStoreError> {
        let mut connection = self.pool.acquire().await?;
        let id: Option<String> =
            sqlx::query_scalar("SELECT authoritative_selection_id FROM audit_runs WHERE id=?")
                .bind(run)
                .fetch_one(&mut *connection)
                .await?;
        if let Some(id) = id {
            let input = self.load_selection(&mut connection, &id).await?;
            let rendered = collection::render(&input)?;
            if rendered.id != report.id
                || rendered.input_digest != report.input_digest
                || rendered.content_digest != report.content_digest
            {
                return Err(AuditStoreError::InvalidPersistedState(
                    "published report differs from authoritative frozen selection".to_owned(),
                ));
            }
        } // Historical reports have no collection selection and retain their original identity.
        Ok(())
    }

    pub(in crate::audex) async fn freeze_report(
        &self,
        run: &str,
    ) -> Result<CollectionInput, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let selected = self.select_report(&mut tx, run).await?;
        let input = self.load_selection(&mut tx, &selected.selection_id).await?;
        tx.commit().await?;
        Ok(input)
    }

    pub(super) async fn publish_selection(
        &self,
        run: &str,
    ) -> Result<ReportPublishOutcome, AuditStoreError> {
        if let Some((revision, report)) = self.read_current_report(run).await? {
            self.read_report(&report)?;
            return Ok(ReportPublishOutcome::AlreadyPublished { revision, report });
        }
        let input = self.freeze_report(run).await?;
        self.publish_frozen(&input).await
    }

    pub(in crate::audex) async fn publish_frozen(
        &self,
        input: &CollectionInput,
    ) -> Result<ReportPublishOutcome, AuditStoreError> {
        let rendered = collection::render(input)?;
        let path = format!("reports/{}/{}.md", input.run_id, rendered.input_digest);
        publish_immutable(
            self.root.as_path(),
            &path,
            &rendered.bytes,
            &rendered.content_digest,
        )?;
        let record = ReportRecord {
            id: rendered.id,
            input_digest: rendered.input_digest,
            content_digest: rendered.content_digest,
            relative_path: path,
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        #[cfg(test)]
        self.wait_transition_for_test("publish").await;
        let row = sqlx::query("SELECT authoritative_selection_id,current_report_id,revision FROM audit_runs WHERE id=?")
            .bind(&input.run_id).fetch_one(&mut *tx).await?;
        if row
            .try_get::<Option<String>, _>("current_report_id")?
            .is_some()
        {
            tx.commit().await?;
            let (revision, report) =
                self.read_current_report(&input.run_id)
                    .await?
                    .ok_or_else(|| {
                        AuditStoreError::InvalidPersistedState(
                            "published report unavailable".to_owned(),
                        )
                    })?;
            self.read_report(&report)?;
            return Ok(ReportPublishOutcome::AlreadyPublished { revision, report });
        }
        if row
            .try_get::<Option<String>, _>("authoritative_selection_id")?
            .as_deref()
            != Some(&input.selection_id)
        {
            return Err(AuditStoreError::InvalidTransition(
                "report selection was superseded".to_owned(),
            ));
        }
        // Check retained authoritative inputs again after file creation and before
        // committing finality. Candidate bytes alone never establish authority.
        let authoritative = self.load_selection(&mut tx, &input.selection_id).await?;
        if serde_json::to_vec(&authoritative)? != serde_json::to_vec(input)? {
            return Err(AuditStoreError::InvalidPersistedState(
                "candidate differs from frozen selection".to_owned(),
            ));
        }
        let revision = row.try_get::<i64, _>("revision")? + 1;
        sqlx::query("INSERT INTO reports(id,run_id,format_version,input_digest,relative_path,content_digest,created_at_ms) VALUES(?,?,?,?,?,?,?)")
            .bind(&record.id).bind(&input.run_id).bind(i64::from(input.format_version)).bind(&record.input_digest).bind(&record.relative_path).bind(&record.content_digest).bind(now_millis()).execute(&mut *tx).await?;
        let phase = if input.outcome == "success" {
            "completed"
        } else {
            "cancelled"
        };
        sqlx::query("UPDATE audit_runs SET current_report_id=?,phase=?,outcome=?,revision=?,updated_at_ms=? WHERE id=?")
            .bind(&record.id).bind(phase).bind(&input.outcome).bind(revision).bind(now_millis()).bind(&input.run_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ReportPublishOutcome::Published {
            revision,
            report: record,
        })
    }
}

#[cfg(test)]
pub(super) type TransitionBarrier = (
    String,
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Semaphore>,
);

#[cfg(test)]
impl AuditStore {
    pub(in crate::audex) fn pause_transition_for_test(
        &self,
        name: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Semaphore>,
    ) {
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        *self.transition_barrier.lock().expect("transition barrier") =
            Some((name.to_owned(), entered.clone(), release.clone()));
        (entered, release)
    }
    pub(in crate::audex) async fn wait_transition_for_test(&self, name: &str) {
        let barrier = {
            let mut barrier = self.transition_barrier.lock().expect("transition barrier");
            if barrier
                .as_ref()
                .is_some_and(|(expected, _, _)| expected == name)
            {
                barrier.take()
            } else {
                None
            }
        };
        if let Some((_, entered, release)) = barrier {
            entered.notify_one();
            release.acquire().await.expect("barrier released").forget();
        }
    }
}
