//! Shared exact-attempt confirmation and submission for main TUI and audit viewers.
use super::*;
use codex_app_server_protocol::{
    AuditAttemptAction, AuditAttemptControlParams, AuditAttemptControlResponse, AuditAttemptView,
};

#[derive(Clone, Debug)]
pub(crate) enum AttemptOperation {
    Observe,
    Wait,
    Control {
        action: AuditAttemptAction,
        action_key: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum AttemptCommand {
    List {
        run_id: String,
    },
    #[command(visible_alias = "read")]
    Observe {
        run_id: String,
        assignment_id: String,
    },
    Wait {
        run_id: String,
        assignment_id: String,
    },
    Cancel {
        run_id: String,
        assignment_id: String,
        #[arg(long)]
        action_key: Option<String>,
    },
    Ignore {
        run_id: String,
        assignment_id: String,
        #[arg(long)]
        action_key: Option<String>,
    },
    Retry {
        run_id: String,
        assignment_id: String,
        #[arg(long)]
        action_key: Option<String>,
    },
}
impl AttemptCommand {
    pub(super) fn action(self) -> AuditAction {
        let (run_id, assignment_id, operation) = match self {
            Self::List { run_id } => return AuditAction::Attempts(run_id),
            Self::Observe {
                run_id,
                assignment_id,
            } => (run_id, assignment_id, AttemptOperation::Observe),
            Self::Wait {
                run_id,
                assignment_id,
            } => (run_id, assignment_id, AttemptOperation::Wait),
            Self::Cancel {
                run_id,
                assignment_id,
                action_key,
            } => (
                run_id,
                assignment_id,
                AttemptOperation::Control {
                    action: AuditAttemptAction::Cancel,
                    action_key,
                },
            ),
            Self::Ignore {
                run_id,
                assignment_id,
                action_key,
            } => (
                run_id,
                assignment_id,
                AttemptOperation::Control {
                    action: AuditAttemptAction::Ignore,
                    action_key,
                },
            ),
            Self::Retry {
                run_id,
                assignment_id,
                action_key,
            } => (
                run_id,
                assignment_id,
                AttemptOperation::Control {
                    action: AuditAttemptAction::Retry,
                    action_key,
                },
            ),
        };
        AuditAction::Attempt {
            run_id,
            assignment_id,
            operation,
        }
    }
}

/// Immutable authorisation target. No refresh, current-attempt substitution or reservation.
#[derive(Clone, Debug)]
pub(crate) struct AttemptControlConfirmation {
    pub(crate) target: AuditAttemptView,
    pub(crate) request: AuditAttemptControlParams,
}
impl AttemptControlConfirmation {
    pub(crate) fn new(
        target: AuditAttemptView,
        action: AuditAttemptAction,
        action_key: Option<String>,
    ) -> Self {
        let retry = action == AuditAttemptAction::Retry;
        let request = AuditAttemptControlParams {
            run_id: target.run_id.clone(),
            assignment_id: target.assignment_id.clone(),
            action_key: action_key.unwrap_or_else(|| Uuid::new_v4().to_string()),
            action,
            reason: "Explicit operator TUI decision".to_owned(),
            authorise_retry: retry,
            acknowledge_possible_overlap: retry,
        };
        Self { target, request }
    }
    pub(crate) fn disclosure(&self) -> String {
        let consequences = match self.request.action {
            AuditAttemptAction::Cancel => {
                "Cancel removes this attempt from outstanding work and freezes its eligible received prefix. If it settles the last outstanding contribution, collection may finalise immediately and Retry closes immediately. Cancel does not reserve or guarantee a later Retry. The old execution may continue; received output remains unvalidated history."
            }
            AuditAttemptAction::Ignore => {
                "Ignore permanently excludes this reviewer from further Retry in this audit; a new audit is needed to reconsider it. Its eligible received prefix remains unvalidated history. If this settles the last outstanding contribution, collection may finalise immediately and Retry closes immediately. The old execution may continue."
            }
            AuditAttemptAction::Retry => {
                "Retry authorises one fresh attempt with the same sealed source and brief. The prior execution may still be running: possible overlapping execution and additional charges are explicitly acknowledged by Yes. No preceding Cancel or separate capacity release is needed. This confirmation reserves no eligibility; readiness or another decision can cause the server to refuse this exact target."
            }
        };
        format!(
            "{}\n\nOperation: {:?}\n{}\n\nAction key: {}. Reuse this key with the same operation and exact assignment if acknowledgement is lost. State changes never retarget this authorisation.",
            attempt_summary(&self.target),
            self.request.action,
            consequences,
            self.request.action_key
        )
    }
}

pub(crate) fn attempt_summary(target: &AuditAttemptView) -> String {
    format!(
        "Reviewer {}: {}\nModel: {} / {}\nRoute: {}\nRun: {}\nAssignment: {} (attempt {})\n{}; contribution={}; execution={}; current={}; Retry eligible={}\nCapture: sequence={}, gap={:?}, failure={:?}, truncated={}\nReceived output is unvalidated history; only an accepted canonical result establishes completed findings.",
        target.reviewer_ordinal + 1,
        target.reviewer_id,
        target.resolved_model,
        target.provider,
        target.authentication_route,
        target.run_id,
        target.assignment_id,
        target.attempt_ordinal,
        if target.completed {
            "Completed validated contribution"
        } else {
            "Incomplete / needs decision"
        },
        target.disposition,
        target.operation_state,
        target.current,
        target.retry_eligible,
        target.output_sequence,
        target.capture_gap,
        target.capture_failure,
        target.capture_truncated
    )
}

/// Presentation hints only. Every explicit/stale request still reaches server validation.
pub(crate) fn attempt_actions(target: &AuditAttemptView) -> Vec<AuditAttemptAction> {
    if !target.current || target.completed {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if target.disposition == "active" {
        if target.operation_state != "failed" {
            actions.push(AuditAttemptAction::Cancel);
        }
        actions.push(AuditAttemptAction::Ignore);
    }
    if target.retry_eligible && target.disposition != "ignored" {
        actions.push(AuditAttemptAction::Retry);
    }
    actions
}

/// One shared confirmation and one exact mutation. S4 can call this directly.
pub(crate) async fn confirm_and_control_attempt(
    audits: &AuditClient,
    thread_id: ThreadId,
    target: AuditAttemptView,
    action: AuditAttemptAction,
    action_key: Option<String>,
    events: &AppEventSender,
) -> Result<AuditAttemptControlResponse, String> {
    audits
        .check_compatibility()
        .await
        .map_err(|error| error.to_string())?;
    let confirmation = AttemptControlConfirmation::new(target, action, action_key);
    let request = confirmation.request.clone();
    let (decision, receiver) = oneshot::channel();
    events.send(AppEvent::AuditAttemptConfirm {
        thread_id,
        confirmation,
        decision,
    });
    if !receiver.await.unwrap_or(false) {
        return Err(
            "Reviewer control was not submitted: operator declined or disconnected".to_owned(),
        );
    }
    audits.control_attempt(request.clone()).await.map_err(|error|format!("Reviewer control request error: {error}. Exact assignment: {}. Action key: {}. No successor was targeted and no automatic Retry was sent.",request.assignment_id,request.action_key))
}

/// Observe one fixed assignment. Ending this task only ends local observation.
pub(crate) async fn observe_attempt(
    handle: AppServerRequestHandle,
    thread_id: ThreadId,
    mut target: AuditAttemptView,
    events: AppEventSender,
    wake: std::sync::Arc<tokio::sync::Notify>,
) {
    let audits = AuditClient::new(handle);
    events.send(AppEvent::AuditAttemptObserved {
        thread_id,
        attempt: target.clone(),
        error: None,
    });
    loop {
        if events.app_event_tx.is_closed()
            || target.completed
            || !target.current
            || target.disposition != "active"
            || matches!(target.operation_state.as_str(), "failed" | "unknown")
        {
            return;
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(1))=>{},_=wake.notified()=>{},_=events.app_event_tx.closed()=>return,}
        match audits
            .read_attempt(codex_app_server_protocol::AuditAttemptReadParams {
                run_id: target.run_id.clone(),
                assignment_id: target.assignment_id.clone(),
                after_sequence: Some(target.output_sequence),
                limit: Some(1),
            })
            .await
        {
            Ok(next) => {
                if next.attempt == target {
                    continue;
                }
                target = next.attempt;
                events.send(AppEvent::AuditAttemptObserved {
                    thread_id,
                    attempt: target.clone(),
                    error: None,
                });
            }
            Err(error) => {
                events.send(AppEvent::AuditAttemptObserved {thread_id,attempt:target,error:Some(format!("Attempt observation disconnected: {error}. No control was sent. Reconnect with /audit attempt wait RUN ASSIGNMENT."))});
                return;
            }
        }
    }
}
