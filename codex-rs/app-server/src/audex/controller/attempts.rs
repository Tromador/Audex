use super::*;
use crate::audex::store::attempts::{AttemptAction, ControlRequest};

impl AuditController {
    async fn attempt_view(&self, record: &AuditRunRecord) -> anyhow::Result<AuditAttemptView> {
        let store = self.store().await?;
        let capture = store.capture_state(&record.assignment_id).await?;
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
            reasoning_effort: record.reasoning_effort.clone(),
            revision: record.attempt_revision,
            owner_generation: record.owner_generation,
            current: record.current,
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
