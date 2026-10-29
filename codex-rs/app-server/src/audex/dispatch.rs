//! Codex execution adapter. Workflow transitions remain exclusively in the controller.
use super::controller::{Dispatch, DispatchRequest, Observation, WorkState};
use super::reviewer::{self, PreparedReviewer, ReviewerRequest};
use super::snapshot::{CaptureLimits, SnapshotHandle, SnapshotId, SnapshotStore};
use super::store::{AuditStore, OperationState, SubmissionBoundary};
use codex_core::config::Config;
use codex_core::{CodexThread, ThreadManager};
use codex_exec_server::LocalFileSystem;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::{EventMsg, Op};
use codex_protocol::turn_input::StartIfIdleSubmission;
use codex_rollout::RolloutItem;
use codex_thread_store::{
    AddThreadAttachmentParams, LoadThreadHistoryParams, PersistContext, StoredModelContext,
    ThreadStore, ThreadStoreError,
};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Installs the existing app-server event reader, without a client subscription.
pub(super) type Listen =
    Arc<dyn Fn(ThreadId, Arc<CodexThread>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

#[cfg(test)]
type HistoryHook = Arc<
    dyn Fn(StoredModelContext) -> BoxFuture<'static, Result<StoredModelContext, ThreadStoreError>>
        + Send
        + Sync,
>;

#[cfg(test)]
#[derive(Default)]
pub(super) struct DispatchTestHooks {
    pub checkpoint: Option<Arc<dyn Fn(&'static str) -> BoxFuture<'static, ()> + Send + Sync>>,
    pub history: Option<HistoryHook>,
}

pub(super) struct CodexDispatch {
    config: Arc<Config>,
    models: SharedModelsManager,
    threads: Arc<ThreadManager>,
    history: Arc<dyn ThreadStore>,
    listen: Listen,
    store: OnceCell<AuditStore>,
    #[cfg(test)]
    pub hooks: DispatchTestHooks,
}

/// Payload-free diagnostics deliberately exclude provider bodies and credentials.
/// The stage and typed cause remain useful even when a backend error contains secrets.
struct DispatchFailure {
    diagnostic: String,
    unsubmitted: bool,
}

fn diagnostic(stage: &str, cause: &str) -> String {
    json!({"version":1,"stage":stage,"cause":cause}).to_string()
}

impl DispatchFailure {
    fn new(stage: &str, error: impl Into<anyhow::Error>, unsubmitted: bool) -> Self {
        let error = error.into();
        let cause = if let Some(error) = error.downcast_ref::<ThreadStoreError>() {
            match error {
                ThreadStoreError::ThreadNotFound { .. } => "thread_store_not_found".to_string(),
                ThreadStoreError::InvalidRequest { .. } => {
                    "thread_store_invalid_request".to_string()
                }
                ThreadStoreError::Conflict { .. } => "thread_store_conflict".to_string(),
                ThreadStoreError::Unsupported { operation } => {
                    format!("thread_store_unsupported:{operation}")
                }
                ThreadStoreError::Retryable { .. } => "thread_store_retryable".to_string(),
                ThreadStoreError::OutcomeUnknown { .. } => {
                    "thread_store_outcome_unknown".to_string()
                }
                ThreadStoreError::Internal { .. } => "thread_store_internal".to_string(),
            }
        } else if let Some(error) = error.downcast_ref::<super::store::AuditStoreError>() {
            use super::store::AuditStoreError;
            match error {
                AuditStoreError::Database(sqlx::Error::Database(error)) => {
                    format!("sqlite:{}", error.code().as_deref().unwrap_or("unknown"))
                }
                AuditStoreError::Database(sqlx::Error::PoolClosed) => {
                    "audit_store_pool_closed".to_string()
                }
                AuditStoreError::Database(_) => "audit_store_database_error".to_string(),
                AuditStoreError::InvalidInput(_) => "audit_store_invalid_input".to_string(),
                AuditStoreError::InvalidTransition(_) => {
                    "audit_store_invalid_transition".to_string()
                }
                AuditStoreError::RevisionConflict { .. } => {
                    "audit_store_revision_conflict".to_string()
                }
                AuditStoreError::IdempotencyConflict(_) => {
                    "audit_store_idempotency_conflict".to_string()
                }
                AuditStoreError::InvalidPersistedState(_) => {
                    "audit_store_invalid_state".to_string()
                }
                AuditStoreError::CorruptDatabase(_) => "audit_store_corrupt_database".to_string(),
                AuditStoreError::Migration(_) => "audit_store_migration_error".to_string(),
                AuditStoreError::Json(_) => "audit_store_json_error".to_string(),
                AuditStoreError::Io(error) => format!("io:{:?}", error.kind()),
            }
        } else if let Some(error) = error.downcast_ref::<codex_protocol::error::CodexErr>() {
            let kind: &'static str =
                codex_protocol::error::CodexErrKind::from(error.details()).into();
            format!("codex:{kind}")
        } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
            format!("io:{:?}", error.kind())
        } else {
            // Audex's own static preflight guards are safe to preserve verbatim.
            // Never retain arbitrary configuration or provider error text.
            let message = error.to_string();
            ["unknown reviewer provider", "model does not support the selected reasoning effort",
                "reviewer policy changed since run creation", "managed additional developer instructions require an explicit Audex compatibility decision",
                "managed hooks are incompatible with static Audex reviewers", "unsupported retained audit input"]
                .into_iter().find(|known| message == *known).unwrap_or("local_configuration_or_execution_error").to_string()
        };
        Self {
            diagnostic: diagnostic(stage, &cause),
            unsubmitted,
        }
    }

    fn observation(self) -> Observation {
        if self.unsubmitted {
            Observation::Unsubmitted(self.diagnostic)
        } else {
            Observation::Deferred(self.diagnostic)
        }
    }
}

type DispatchResult = Result<Observation, DispatchFailure>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RetainedInput {
    version: u32,
    brief: String,
    provider: String,
    model: String,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
}

impl CodexDispatch {
    pub(super) fn new(
        config: Arc<Config>,
        threads: Arc<ThreadManager>,
        history: Arc<dyn ThreadStore>,
        listen: Listen,
    ) -> Self {
        Self {
            models: threads.get_models_manager(),
            config,
            threads,
            history,
            listen,
            store: OnceCell::new(),
            #[cfg(test)]
            hooks: DispatchTestHooks::default(),
        }
    }

    async fn store(&self) -> anyhow::Result<&AuditStore> {
        self.store
            .get_or_try_init(|| AuditStore::open(self.config.codex_home.join("audex")))
            .await
            .map_err(Into::into)
    }

    async fn prepare(&self, operation: &str) -> anyhow::Result<(SnapshotHandle, PreparedReviewer)> {
        let store = self.store().await?;
        let run = store.run_for_operation(operation).await?;
        let (input, _, policy, _) = store.controller_input(&run.run_id).await?;
        let input: RetainedInput = serde_json::from_str(&input)?;
        anyhow::ensure!(input.version == 1, "unsupported retained audit input");
        let snapshot = SnapshotStore::new(
            self.config.codex_home.join("audex").join("snapshots"),
            Arc::new(LocalFileSystem::unsandboxed()),
            CaptureLimits::default(),
        )
        .open(&SnapshotId::parse(run.snapshot_id)?)
        .await?;
        let prepared = Box::pin(reviewer::prepare(
            &self.config,
            self.models.as_ref(),
            &snapshot,
            ReviewerRequest {
                run_id: run.run_id,
                assignment_id: run.assignment_id,
                brief: input.brief,
                provider: input.provider,
                model: input.model,
                reasoning_effort: input.reasoning_effort,
            },
            Arc::new(|_| {}),
        ))
        .await?;
        anyhow::ensure!(
            prepared.policy_digest == policy,
            "reviewer policy changed since run creation"
        );
        Ok((snapshot, prepared))
    }

    #[cfg(test)]
    async fn checkpoint(&self, stage: &'static str) {
        if let Some(checkpoint) = &self.hooks.checkpoint {
            checkpoint(stage).await;
        }
    }

    async fn start_inner(&self, request: DispatchRequest) -> DispatchResult {
        let store = self
            .store()
            .await
            .map_err(|e| DispatchFailure::new("open_store", e, false))?;
        let run = store
            .run_for_operation(&request.operation_id)
            .await
            .map_err(|e| DispatchFailure::new("read_operation", e, false))?;
        if run.run_id != request.run_id
            || run.assignment_id != request.assignment_id
            || run.snapshot_id != request.snapshot_id
        {
            return Ok(Observation::Uncertain(diagnostic(
                "dispatch_identity",
                "identity_mismatch",
            )));
        }
        let boundary = store
            .submission_boundary(&request.operation_id)
            .await
            .map_err(|e| DispatchFailure::new("read_submission_boundary", e, false))?;
        if boundary != SubmissionBoundary::Unsubmitted
            || run.operation_state != OperationState::Dispatching
        {
            return self.observe(&request.operation_id, false).await;
        }
        if run.control_intent == "cancel_requested" {
            return Ok(Observation::Unsubmitted(diagnostic(
                "before_submission",
                "cancel_requested",
            )));
        }
        let (_, mut prepared) = self
            .prepare(&request.operation_id)
            .await
            .map_err(|e| DispatchFailure::new("reviewer_preflight", e, true))?;
        let thread_id = self.threads.reserve_thread_id();
        if !store
            .bind_execution(
                &request.operation_id,
                &thread_id.to_string(),
                &prepared.policy_digest,
                &request.lease_owner,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            .map_err(|e| DispatchFailure::new("reserve_thread", e, true))?
        {
            return self.observe(&request.operation_id, false).await;
        }
        #[cfg(test)]
        self.checkpoint("reserved").await;
        prepared.options.reserved_thread_id = Some(thread_id);
        let thread = self
            .threads
            .start_thread(prepared.options)
            .await
            .map_err(|e| DispatchFailure::new("create_thread", e, true))?
            .thread;
        #[cfg(test)]
        self.checkpoint("thread_created").await;
        (self.listen)(thread_id, Arc::clone(&thread))
            .await
            .map_err(|e| DispatchFailure::new("install_listener", e, true))?;
        // Standard persistence activates lazy storage; Core's fallible checkpoint
        // then applies LiveThread's pending metadata. Neither step may be skipped
        // before attaching identity or crossing the submission boundary.
        self.history
            .persist_thread(thread_id, PersistContext::Standard)
            .await
            .map_err(|e| DispatchFailure::new("materialise_history", e, true))?;
        thread
            .checkpoint_preparation()
            .await
            .map_err(|e| DispatchFailure::new("prepare_history_metadata", e, true))?;
        thread
            .flush_rollout()
            .await
            .map_err(|e| DispatchFailure::new("flush_preparation", e, true))?;
        if self.history.supports_thread_attachments() {
            self.history.add_thread_attachment(AddThreadAttachmentParams { thread_id,
                attachment_type: "audex.review.v1".to_string(), identity_key: request.operation_id.clone(),
                payload: json!({"run_id":request.run_id,"assignment_id":request.assignment_id,"operation_id":request.operation_id,
                    "snapshot_id":request.snapshot_id,"policy_digest":prepared.policy_digest}),
            }).await.map_err(|e| DispatchFailure::new("attach_identity", e, true))?;
        }
        #[cfg(test)]
        self.checkpoint("history_materialised").await;
        if !store
            .begin_submission(
                &request.operation_id,
                &request.lease_owner,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            .map_err(|e| DispatchFailure::new("persist_submission_boundary", e, true))?
        {
            return Ok(Observation::Unsubmitted(diagnostic(
                "submission_admission",
                "cancelled_expired_or_superseded",
            )));
        }
        // Everything before this commit is definitely unsubmitted. Everything
        // after it is observation-only on recovery, even if the API never ran.
        #[cfg(test)]
        self.checkpoint("submission_started").await;
        prepared.input.start.turn_trigger = Some(format!("audex:{}", request.operation_id));
        match thread.start_turn_if_idle(prepared.input).await {
            Ok(StartIfIdleSubmission::Started { turn_id, .. }) => {
                #[cfg(test)]
                self.checkpoint("core_accepted").await;
                store
                    .execution_accepted(&request.operation_id, &turn_id)
                    .await
                    .map_err(|e| DispatchFailure::new("persist_acceptance", e, false))?;
                #[cfg(test)]
                self.checkpoint("acceptance_saved").await;
                Ok(accepted(
                    &request.operation_id,
                    &request.snapshot_id,
                    thread_id,
                    &turn_id,
                    WorkState::Live,
                ))
            }
            Ok(StartIfIdleSubmission::NotSubmitted { .. }) => Ok(Observation::Rejected(
                diagnostic("submit_turn", "core_declined_input"),
            )),
            Err(error) => Err(DispatchFailure::new("submit_turn", error, false)),
        }
    }

    async fn read_history(
        &self,
        thread_id: ThreadId,
    ) -> Result<StoredModelContext, DispatchFailure> {
        let history = self
            .history
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .map_err(|e| DispatchFailure::new("read_history", e, false))?;
        #[cfg(test)]
        let history = if let Some(hook) = &self.hooks.history {
            hook(history)
                .await
                .map_err(|e| DispatchFailure::new("read_history", e, false))?
        } else {
            history
        };
        Ok(history)
    }

    async fn observe(&self, operation: &str, interrupt: bool) -> DispatchResult {
        let store = self
            .store()
            .await
            .map_err(|e| DispatchFailure::new("open_store", e, false))?;
        let run = store
            .run_for_operation(operation)
            .await
            .map_err(|e| DispatchFailure::new("read_operation", e, false))?;
        let boundary = store
            .submission_boundary(operation)
            .await
            .map_err(|e| DispatchFailure::new("read_submission_boundary", e, false))?;
        if boundary == SubmissionBoundary::Unsubmitted {
            // Closing this operation also fences any late, lease-expired startup
            // owner at begin_submission. Do not automatically recreate its thread.
            return Ok(Observation::Unsubmitted(diagnostic(
                "startup_recovery",
                "interrupted_before_submission",
            )));
        }
        let Some((thread_id, retained_turn, policy)) = store
            .execution(operation)
            .await
            .map_err(|e| DispatchFailure::new("read_execution", e, false))?
        else {
            return Ok(Observation::Deferred(diagnostic(
                "execution_recovery",
                "reservation_or_acceptance_unconfirmed",
            )));
        };
        let thread_id = match ThreadId::from_string(&thread_id) {
            Ok(thread_id) => thread_id,
            Err(_) => {
                return Ok(Observation::Uncertain(diagnostic(
                    "execution_identity",
                    "invalid_thread_id",
                )));
            }
        };
        let (_, prepared) = self
            .prepare(operation)
            .await
            .map_err(|e| DispatchFailure::new("observation_preflight", e, false))?;
        if prepared.policy_digest != policy {
            return Ok(Observation::Uncertain(diagnostic(
                "execution_identity",
                "policy_binding_mismatch",
            )));
        }
        let live = match self.threads.get_thread(thread_id).await {
            Ok(thread) => Some(thread),
            Err(error)
                if matches!(
                    error.details(),
                    codex_protocol::error::CodexErrorDetails::ThreadNotFound(_)
                ) =>
            {
                None
            }
            Err(error) => return Err(DispatchFailure::new("lookup_runtime", error, false)),
        };
        if let Some(thread) = &live {
            if !super::controller::is_reviewer_source(
                thread.config_snapshot().await.thread_source.as_ref(),
            ) {
                return Ok(Observation::Uncertain(diagnostic(
                    "execution_identity",
                    "foreign_live_thread",
                )));
            }
            if thread.has_shutdown_status() {
                thread.wait_until_terminated().await;
            } else if let Err(error) = thread.flush_rollout().await {
                if thread.has_shutdown_status() {
                    thread.wait_until_terminated().await;
                } else {
                    return Err(DispatchFailure::new("flush_observation", error, false));
                }
            }
        }
        let mut history = self.read_history(thread_id).await?;
        // Shutdown can complete after the first read. Wait for Codex's existing
        // producer/writer barrier and take a fresh snapshot, never infer failure
        // from the stale read taken before that barrier.
        if let Some(thread) = &live
            && thread.has_shutdown_status()
        {
            thread.wait_until_terminated().await;
            history = self.read_history(thread_id).await?;
        }
        if history.thread_id != thread_id || !history.items.iter().any(|item| matches!(item,
            RolloutItem::SessionMeta(meta) if meta.meta.id == thread_id && super::controller::is_reviewer_source(meta.meta.thread_source.as_ref()))) {
            return Ok(Observation::Uncertain(diagnostic("history_identity", "foreign_persisted_thread")));
        }
        let mut turn = retained_turn;
        let trigger = format!("audex:{operation}");
        for item in &history.items {
            if let RolloutItem::EventMsg(EventMsg::TurnStarted(event)) = item
                && event
                    .turn_attribution
                    .as_ref()
                    .and_then(|a| a.turn_trigger.as_deref())
                    == Some(trigger.as_str())
            {
                if turn.as_ref().is_some_and(|id| id != &event.turn_id) {
                    return Ok(Observation::Uncertain(diagnostic(
                        "history_identity",
                        "conflicting_turns",
                    )));
                }
                turn = Some(event.turn_id.clone());
            }
        }
        let Some(turn) = turn else {
            return Ok(Observation::Deferred(diagnostic(
                "acceptance_observation",
                "turn_acceptance_unconfirmed",
            )));
        };
        store
            .execution_accepted(operation, &turn)
            .await
            .map_err(|e| DispatchFailure::new("persist_acceptance", e, false))?;
        if let Some(info) = history.items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => event.info.as_ref(),
            _ => None,
        }) {
            let usage = serde_json::to_string(info)
                .map_err(|e| DispatchFailure::new("encode_usage", e, false))?;
            store
                .execution_usage(operation, &usage)
                .await
                .map_err(|e| DispatchFailure::new("persist_usage", e, false))?;
        }
        if let Some(state) = terminal_state(&history.items, &turn) {
            return Ok(accepted(
                operation,
                &run.snapshot_id,
                thread_id,
                &turn,
                state,
            ));
        }
        if let Some(thread) = live
            && !thread.has_shutdown_status()
        {
            if interrupt {
                if run.control_intent != "cancel_requested" {
                    return Ok(Observation::Uncertain(diagnostic(
                        "interrupt",
                        "missing_durable_intent",
                    )));
                }
                thread
                    .submit(Op::Interrupt)
                    .await
                    .map_err(|e| DispatchFailure::new("interrupt", e, false))?;
            }
            return Ok(accepted(
                operation,
                &run.snapshot_id,
                thread_id,
                &turn,
                WorkState::Live,
            ));
        }
        // A cold or shut-down runtime is not proof that its last readable
        // history is final. Preserve acceptance and continue observation only.
        Ok(Observation::Deferred(diagnostic(
            "terminal_observation",
            "terminal_history_unconfirmed",
        )))
    }
}

fn terminal_state(items: &[RolloutItem], turn: &str) -> Option<WorkState> {
    let mut terminal = None;
    for item in items {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == turn => {
                if let Some(error) = &event.error {
                    terminal = Some(WorkState::Failed(
                        json!({"version":1,"stage":"reviewer_turn","cause":error.codex_error_info})
                            .to_string(),
                    ));
                } else {
                    return Some(WorkState::Completed(
                        event
                            .last_agent_message
                            .clone()
                            .unwrap_or_default()
                            .into_bytes(),
                    ));
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                if event.turn_id.as_deref() == Some(turn) =>
            {
                terminal = Some(WorkState::Interrupted)
            }
            _ => {}
        }
    }
    terminal
}

fn accepted(
    operation: &str,
    snapshot: &str,
    thread: ThreadId,
    turn: &str,
    state: WorkState,
) -> Observation {
    Observation::Accepted { operation_id: operation.to_string(), snapshot_id: snapshot.to_string(),
        receipt: json!({"version":1,"thread_id":thread,"turn_id":turn,"operation_id":operation,"snapshot_id":snapshot}).to_string(), state }
}

impl Dispatch for CodexDispatch {
    fn start(&self, request: DispatchRequest) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            Box::pin(self.start_inner(request))
                .await
                .unwrap_or_else(DispatchFailure::observation)
        })
    }
    fn inspect(&self, operation: String) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            self.observe(&operation, false)
                .await
                .unwrap_or_else(DispatchFailure::observation)
        })
    }
    fn interrupt(&self, operation: String) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            self.observe(&operation, true)
                .await
                .unwrap_or_else(DispatchFailure::observation)
        })
    }
}
