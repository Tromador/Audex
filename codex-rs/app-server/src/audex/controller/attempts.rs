use super::*;
use crate::audex::store::attempts::{AttemptAction, ControlRequest};

impl AuditController {
    pub(super) async fn revalidate_retained(&self, record: &AuditRunRecord) -> anyhow::Result<()> {
        let (raw, policy_json, policy_digest, manifest_digest) = self
            .store()
            .await?
            .attempt_input(&record.operation_id)
            .await?;
        let input = super::super::retained::RetainedInput::decode(&raw)?;
        input.require_native()?;
        let settings = super::super::backends::BackendSettings::load(self.root.as_path())?;
        let snapshot = self
            .snapshots()
            .open(&SnapshotId::parse(record.snapshot_id.clone())?)
            .await?;
        anyhow::ensure!(
            sha256_hex(policy_json.as_bytes()) == policy_digest
                && sha256_hex(&snapshot.manifest().to_pretty_bytes()?) == manifest_digest,
            "retained Retry source or policy digest mismatch"
        );
        let prepared = Box::pin(reviewer::prepare(
            &self.config,
            self.models.as_ref(),
            &snapshot,
            reviewer::ReviewerRequest {
                run_id: record.run_id.clone(),
                assignment_id: record.assignment_id.clone(),
                brief: input.brief.clone(),
                provider: input.provider.clone(),
                model: input.model.clone(),
                reasoning_effort: input.reasoning_effort.clone(),
            },
            Arc::new(|_| {}),
        ))
        .await?;
        anyhow::ensure!(
            prepared.policy_digest == policy_digest,
            "reviewer policy changed since run creation"
        );
        input.validate_prepared(
            &snapshot,
            &prepared,
            &self.config,
            &settings,
            if record.scheduling == "sequential" {
                AuditScheduling::Sequential
            } else {
                AuditScheduling::Automatic
            },
        )
    }

    pub(super) async fn abort_audit(&self, run: &str) -> anyhow::Result<()> {
        let store = self.store().await?;
        // No attempt gate: a stalled observation or termination cannot delay stop.
        let records = store.attempts(run).await?;
        let captures = records
            .iter()
            .filter_map(|record| self.dispatch.capture(&record.operation_id))
            .collect::<Vec<_>>();
        let prefixes = captures
            .iter()
            .map(|capture| (capture.identity().clone(), capture.control_prefix()))
            .collect::<Vec<_>>();
        let result = store.abort_run(run, &prefixes).await;
        for capture in &captures {
            capture.completion_finished(result.is_ok());
        }
        result?;
        // Retry may have committed between prefix sampling and Abort. Interrupt
        // the store's current stopped attempts, including any such replacement.
        let stopped = store.load_run_metadata(run).await?.control_intent == "cancel_requested";
        for record in store.attempts(run).await?.into_iter().filter(|record| {
            stopped && record.current && record.assignment_state != AssignmentState::Succeeded
        }) {
            let dispatch = self.dispatch.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    dispatch.request_interrupt(record.operation_id),
                )
                .await;
            });
        }
        self.wake.notify_one();
        self.progress(run).await
    }

    async fn attempt_view(&self, record: &AuditRunRecord) -> anyhow::Result<AuditAttemptView> {
        let store = self.store().await?;
        let capture = store.capture_state(&record.assignment_id).await?;
        let retained = store.retained_selection(&record.assignment_id).await?;
        Ok(AuditAttemptView {
            run_id: record.run_id.clone(),
            reviewer_id: record.reviewer_id.clone(),
            reviewer_ordinal: record.reviewer_ordinal,
            assignment_id: record.assignment_id.clone(),
            attempt_ordinal: record.attempt_ordinal,
            operation_id: record.operation_id.clone(),
            snapshot_id: record.snapshot_id.clone(),
            provider: record.provider.clone(),
            model: record.model.clone(),
            requested_model: record.model.clone(),
            resolved_model: retained["resolvedModel"]
                .as_str()
                .unwrap_or(&record.model)
                .to_owned(),
            expected_served_model: retained["expectedServedModel"].as_str().map(str::to_owned),
            reasoning_effort: record.reasoning_effort.clone(),
            revision: record.attempt_revision,
            owner_generation: record.owner_generation,
            current: record.current,
            retry_eligible: record.current
                && !record.collection_ready
                && record.control_intent == "active"
                && matches!(record.phase, RunPhase::SnapshotBound | RunPhase::Reviewing)
                && record.phase != RunPhase::Reporting
                && record.assignment_state != AssignmentState::Succeeded
                && capture.disposition != "ignored",
            execution_adapter: retained["executionAdapter"]
                .as_str()
                .map(|value| serde_json::from_value(serde_json::Value::String(value.to_owned())))
                .transpose()?
                .unwrap_or_default(),
            authentication_route: retained["authenticationRoute"]
                .as_str()
                .unwrap_or("native_legacy_unspecified")
                .to_owned(),
            model_evidence: store.model_evidence(&record.assignment_id).await?,
            disposition: capture.disposition,
            assignment_state: match record.assignment_state {
                AssignmentState::Planned => "planned",
                AssignmentState::ThreadReady => "thread_ready",
                AssignmentState::Dispatching => "dispatching",
                AssignmentState::Running => "running",
                AssignmentState::Succeeded => "succeeded",
                AssignmentState::Failed => "failed",
                AssignmentState::Cancelled => "cancelled",
                AssignmentState::Unknown => "unknown",
            }
            .to_owned(),
            operation_state: match record.operation_state {
                OperationState::Planned => "planned",
                OperationState::Dispatching => "dispatching",
                OperationState::Accepted => "accepted",
                OperationState::ObservedTerminal => "observed_terminal",
                OperationState::Committed => "committed",
                OperationState::Failed => "failed",
                OperationState::Unknown => "unknown",
            }
            .to_owned(),
            completed: record.assignment_state == AssignmentState::Succeeded,
            possible_overlap: store.possible_overlap(&record.operation_id).await?,
            output_sequence: capture.sequence,
            output_cutoff: capture.cutoff,
            capture_gap: capture.gap,
            capture_failure: capture.failure,
            capture_truncated: capture.truncated,
        })
    }

    pub(super) async fn attempt_views(&self, run: &str) -> anyhow::Result<AuditAttemptsResponse> {
        // Reject a missing run explicitly, even if it would have an empty roster.
        self.store().await?.load_run_metadata(run).await?;
        let records = self.store().await?.attempts(run).await?;
        let mut attempts = Vec::with_capacity(records.len());
        for record in &records {
            attempts.push(self.attempt_view(record).await?);
        }
        Ok(AuditAttemptsResponse { attempts })
    }

    pub(super) async fn read_attempt(
        &self,
        params: AuditAttemptReadParams,
    ) -> anyhow::Result<AuditAttemptReadResponse> {
        let store = self.store().await?;
        let record = store
            .attempt_by_assignment(&params.run_id, &params.assignment_id)
            .await?;
        let after = params.after_sequence.unwrap_or(0);
        let entries = store
            .activity(&params.assignment_id, after, params.limit.unwrap_or(64))
            .await?
            .into_iter()
            .map(|entry| AuditActivityEntry {
                sequence: entry.sequence,
                session_id: entry.session_id,
                turn_id: entry.turn_id,
                item_id: entry.item_id,
                kind: entry.kind,
                payload: entry.payload,
                payload_digest: entry.payload_digest,
            })
            .collect::<Vec<_>>();
        let next_sequence = entries.last().map_or(after, |entry| entry.sequence);
        Ok(AuditAttemptReadResponse {
            attempt: self.attempt_view(&record).await?,
            entries,
            next_sequence,
            raw_result: store
                .raw_result(&record)?
                .map(String::from_utf8)
                .transpose()?,
        })
    }

    pub(super) async fn control_attempt(
        &self,
        params: AuditAttemptControlParams,
    ) -> anyhow::Result<AuditAttemptControlResponse> {
        let store = self.store().await?;
        let record = store
            .attempt_by_assignment(&params.run_id, &params.assignment_id)
            .await?;
        let request = ControlRequest {
            run_id: params.run_id.clone(),
            assignment_id: params.assignment_id,
            action_key: params.action_key,
            action: match params.action {
                AuditAttemptAction::Cancel => AttemptAction::Cancel,
                AuditAttemptAction::Ignore => AttemptAction::Ignore,
                AuditAttemptAction::Retry => AttemptAction::Retry,
            },
            reason: params.reason,
            authorise_retry: params.authorise_retry,
            acknowledge_possible_overlap: params.acknowledge_possible_overlap,
        };
        if request.action == AttemptAction::Retry
            && !store.has_control_action(&request.action_key).await?
        {
            self.revalidate_retained(&record).await?;
        }
        // No attempt transition gate or collector flush mutex is acquired here.
        let capture = self.dispatch.capture(&record.operation_id);
        let prefix = capture.as_ref().map(|capture| capture.control_prefix());
        let result = store
            .control_attempt(
                &request,
                capture
                    .as_ref()
                    .zip(prefix.as_ref())
                    .map(|(capture, inputs)| (capture.identity(), inputs.as_slice())),
            )
            .await;
        if let Some(capture) = &capture {
            capture.control_finished(result.as_ref().ok());
        }
        let result = result?;
        if !result.completed_won {
            let dispatch = self.dispatch.clone();
            let operation = record.operation_id;
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    dispatch.request_interrupt(operation),
                )
                .await;
            });
        }
        self.wake.notify_one();
        let control_phase = match result.run_phase.as_str() {
            "created" => AuditRunPhase::Created,
            "snapshot_bound" => AuditRunPhase::SnapshotBound,
            "reviewing" => AuditRunPhase::Reviewing,
            "reporting" => AuditRunPhase::Reporting,
            "reconciling" => AuditRunPhase::Reconciling,
            "completed" => AuditRunPhase::Completed,
            "completed_with_failures" => AuditRunPhase::CompletedWithFailures,
            "failed" => AuditRunPhase::Failed,
            _ => AuditRunPhase::Cancelled,
        };
        (self.notify)(AuditProgressNotification {
            run_id: params.run_id,
            revision: result.run_revision,
            phase: control_phase,
            attempts: vec![AuditAttemptProgress {
                assignment_id: result.assignment_id.clone(),
                revision: result.attempt_revision,
                output_sequence: result.output_sequence,
            }],
        });
        #[cfg(test)]
        anyhow::ensure!(
            !self.fail_control_response.load(Ordering::SeqCst),
            "injected control acknowledgement loss"
        );
        Ok(AuditAttemptControlResponse {
            action_key: result.action_key,
            assignment_id: result.assignment_id,
            disposition: result.disposition,
            cutoff: result.cutoff,
            replacement_assignment_id: result.replacement_assignment_id,
            replacement_operation_id: result.replacement_operation_id,
            completed_won: result.completed_won,
            possible_overlap: result.possible_overlap,
        })
    }
}
