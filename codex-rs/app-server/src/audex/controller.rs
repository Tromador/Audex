//! Process-owned workflow. Codex execution uses the same durable boundary as deterministic tests.
mod attempts;
use super::reviewer;
use super::snapshot::manifest::sha256_hex;
use super::snapshot::{
    CaptureLimits, CaptureRequest, CaptureSource, ContextDocuments, SnapshotError, SnapshotHandle,
    SnapshotId, SnapshotPath, SnapshotStore,
};
use super::store::{
    AssignmentState, AuditRunRecord, AuditStore, ClaimOutcome, CreateRunRequest, DEFAULT_WORKERS,
    OperationState, RunPhase,
};
use codex_app_server_protocol::*;
use codex_core::config::Config;
use codex_exec_server::LocalFileSystem;
use codex_models_manager::manager::SharedModelsManager;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::StreamExt;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::result::Result;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify, OnceCell};
use tokio_util::sync::CancellationToken;

const OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct DispatchRequest {
    pub run_id: String,
    pub assignment_id: String,
    pub operation_id: String,
    pub snapshot_id: String,
    pub lease_owner: String,
}

#[derive(Clone)]
pub(super) enum WorkState {
    Live,
    Completed(Vec<u8>),
    Failed(String),
    Interrupted,
    Ambiguous,
}

#[derive(Clone)]
pub(super) enum Observation {
    Absent,
    Rejected(String),
    /// A pre-submission observation must be rechecked transactionally before
    /// closing the operation, since another owner may still be preparing it.
    Unsubmitted(String),
    /// No sampling retry: keep the durable operation parked for observation.
    Deferred(String),
    /// Positive evidence of an identity/integrity contradiction, not mere absence.
    Uncertain(String),
    Accepted {
        operation_id: String,
        snapshot_id: String,
        receipt: String,
        state: WorkState,
    },
}

/// Implementations must not silently retry an accepted or ambiguous operation.
pub(super) trait Dispatch: Send + Sync {
    fn available(&self) -> bool {
        true
    }
    fn capture(&self, _operation: &str) -> Option<super::capture::CaptureHandle> {
        None
    }
    fn request_interrupt(&self, operation: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.interrupt(operation).await;
        })
    }
    fn start(&self, request: DispatchRequest) -> BoxFuture<'_, Observation>;
    fn inspect(&self, operation_id: String) -> BoxFuture<'_, Observation>;
    fn interrupt(&self, operation_id: String) -> BoxFuture<'_, Observation>;
    fn cleanup_preparation(&self, _operation_id: String) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Keep local recovery from observing a dispatch whose acceptance call has not
/// returned. This is advisory runtime state; cross-process recovery uses the lease.
struct PendingDispatch<'a> {
    pending: &'a StdMutex<HashSet<String>>,
    operation: String,
}

impl Drop for PendingDispatch<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .expect("pending attempts lock")
            .remove(&self.operation);
    }
}

#[derive(Clone, serde::Serialize)]
pub(super) struct ReviewerSelection {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

type ProgressVersion = (i64, Vec<(String, i64, i64, i64)>);

pub(crate) struct AuditController {
    root: AbsolutePathBuf,
    config: Arc<Config>,
    models: SharedModelsManager,
    store: OnceCell<AuditStore>,
    dispatch: Arc<dyn Dispatch>,
    owner: String,
    stop: CancellationToken,
    wake: Arc<Notify>,
    polling: Mutex<HashMap<String, ((i64, i64), tokio::time::Instant, u32)>>,
    notified_revisions: Mutex<HashMap<String, ProgressVersion>>,
    /// Orders local start/cancel/shutdown against crossing the dispatch boundary.
    gate: Mutex<()>,
    dispatch_pending: StdMutex<HashSet<String>>,
    scheduled_attempts: StdMutex<HashSet<String>>,
    worker_limit: u32,
    attempt_gates: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    notify: Arc<dyn Fn(AuditProgressNotification) + Send + Sync>,
    #[cfg(test)]
    fail_start_response: AtomicBool,
    #[cfg(test)]
    fail_control_response: AtomicBool,
}

impl AuditController {
    pub(crate) fn service(
        config: Arc<Config>,
        threads: Arc<codex_core::ThreadManager>,
        history: Arc<dyn codex_thread_store::ThreadStore>,
        listen: super::dispatch::Listen,
        cleanup: super::dispatch::Listen,
        outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    ) -> Arc<Self> {
        let dispatch = Arc::new(
            super::dispatch::CodexDispatch::new(
                Arc::clone(&config),
                Arc::clone(&threads),
                history,
                listen,
            )
            .with_cleanup(cleanup),
        );
        let controller = Self::new(
            config,
            threads.get_models_manager(),
            dispatch.clone(),
            Arc::new(move |progress| {
                outgoing.try_send_server_notification(ServerNotification::AuditProgress(progress));
            }),
        );
        dispatch.set_wake(controller.wake.clone());
        Self::spawn(&controller);
        controller
    }

    fn new(
        config: Arc<Config>,
        models: SharedModelsManager,
        dispatch: Arc<dyn Dispatch>,
        notify: Arc<dyn Fn(AuditProgressNotification) + Send + Sync>,
    ) -> Arc<Self> {
        Self::new_with_worker_limit(config, models, dispatch, notify, DEFAULT_WORKERS)
    }

    fn new_with_worker_limit(
        config: Arc<Config>,
        models: SharedModelsManager,
        dispatch: Arc<dyn Dispatch>,
        notify: Arc<dyn Fn(AuditProgressNotification) + Send + Sync>,
        worker_limit: u32,
    ) -> Arc<Self> {
        assert!(worker_limit > 0);
        Arc::new(Self {
            root: config.codex_home.join("audex"),
            config,
            models,
            dispatch,
            notify,
            store: OnceCell::new(),
            owner: uuid::Uuid::now_v7().to_string(),
            stop: CancellationToken::new(),
            wake: Arc::new(Notify::new()),
            polling: Mutex::new(HashMap::new()),
            notified_revisions: Mutex::new(HashMap::new()),
            gate: Mutex::new(()),
            dispatch_pending: StdMutex::new(HashSet::new()),
            scheduled_attempts: StdMutex::new(HashSet::new()),
            worker_limit,
            attempt_gates: StdMutex::new(HashMap::new()),
            #[cfg(test)]
            fail_start_response: AtomicBool::new(false),
            #[cfg(test)]
            fail_control_response: AtomicBool::new(false),
        })
    }

    fn snapshots(&self) -> SnapshotStore {
        SnapshotStore::new(
            self.root.join("snapshots"),
            Arc::new(LocalFileSystem::unsandboxed()),
            CaptureLimits::default(),
        )
    }

    async fn store(&self) -> anyhow::Result<&AuditStore> {
        self.store
            .get_or_try_init(|| AuditStore::open(self.root.clone()))
            .await
            .map_err(Into::into)
    }

    fn spawn(controller: &Arc<Self>) {
        let weak = Arc::downgrade(controller);
        let stop = controller.stop.clone();
        tokio::spawn(async move {
            let mut unavailable_delay = 1u64;
            loop {
                let Some(controller) = weak.upgrade() else {
                    break;
                };
                if stop.is_cancelled() {
                    break;
                }
                if let Err(error) = controller.recover_runs(true).await {
                    tracing::warn!(%error, "Audex workflow unavailable; retained state was not replaced");
                    unavailable_delay = (unavailable_delay * 2).min(60);
                } else {
                    unavailable_delay = 1;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = controller.wake.notified() => { controller.polling.lock().await.clear(); unavailable_delay=1; },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(unavailable_delay)) => {},
                }
            }
        });
    }

    pub(crate) async fn quiesce(&self) {
        self.stop_new_dispatch();
        let _gate = self.gate.lock().await;
    }

    pub(crate) fn stop_new_dispatch(&self) {
        self.stop.cancel();
    }

    /// The same typed handler is used by MessageProcessor and deterministic tests.
    pub(crate) async fn handle(
        &self,
        request: ClientRequest,
    ) -> Result<ClientResponsePayload, JSONRPCErrorError> {
        let key = if let ClientRequest::AuditStart { params, .. } = &request {
            Some(params.request_key.clone())
        } else {
            None
        };
        // Established from captured input, before mutable preflight. Never recapture
        // a live file while trying to identify a failed start acknowledgement.
        let mut start_identity = None;
        match Box::pin(self.handle_inner(request, &mut start_identity)).await {
            Ok(response) => Ok(response),
            Err(error) => {
                let mut response = crate::error_code::invalid_request(format!("Audex: {error}"));
                if matches!(
                    error.downcast_ref::<super::store::AuditStoreError>(),
                    Some(super::store::AuditStoreError::IdempotencyConflict(_))
                ) {
                    response.data = Some(
                        serde_json::json!({"reason":"auditIdempotencyConflict","requestKey":key}),
                    );
                } else if let (Some(key), Some(identity)) = (key, start_identity) {
                    match async {
                        self.store()
                            .await?
                            .find_idempotent_request(&key, &identity)
                            .await
                            .map_err(anyhow::Error::from)
                    }
                    .await
                    {
                        Ok(Some(run)) => {
                            response.data = Some(
                                serde_json::json!({"reason":"auditRunRetained","requestKey":key,"runId":run.run_id,"assignmentId":run.assignment_id,"operationId":run.operation_id}),
                            );
                        }
                        Ok(None) => {}
                        Err(error)
                            if matches!(
                                error.downcast_ref::<super::store::AuditStoreError>(),
                                Some(super::store::AuditStoreError::IdempotencyConflict(_))
                            ) =>
                        {
                            response.data = Some(
                                serde_json::json!({"reason":"auditIdempotencyConflict","requestKey":key}),
                            );
                        }
                        Err(_) => {
                            response.data = Some(
                                serde_json::json!({"reason":"auditStartOutcomeUnconfirmed","requestKey":key}),
                            );
                        }
                    }
                }
                Err(response)
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize local durable start and cancellation transitions against dispatch admission"
    )]
    async fn handle_inner(
        &self,
        request: ClientRequest,
        start_identity: &mut Option<String>,
    ) -> anyhow::Result<ClientResponsePayload> {
        match request {
            ClientRequest::AuditCapabilities { .. } => Ok(AuditCapabilitiesResponse {
                protocol_version: AUDIT_PROTOCOL_VERSION,
            }
            .into()),
            ClientRequest::AuditStart { params, .. } => {
                let selection = ReviewerSelection {
                    provider: params.provider.clone(),
                    model: params.model.clone(),
                    reasoning_effort: params.reasoning_effort.clone(),
                };
                Ok(AuditStartResponse {
                    run: self
                        .commission_roster(params, vec![selection], start_identity)
                        .await?,
                }
                .into())
            }
            ClientRequest::AuditRead { params, .. } => Ok(AuditReadResponse {
                run: self.view(&params.run_id).await?,
            }
            .into()),
            ClientRequest::AuditList { params, .. } => {
                let limit = params.limit.unwrap_or(20);
                anyhow::ensure!(
                    (1..=100).contains(&limit),
                    "audit list limit must be 1..100"
                );
                let mut ids = self
                    .store()
                    .await?
                    .list_run_ids(params.after.as_deref().unwrap_or(""), limit + 1)
                    .await?;
                let next_cursor = if ids.len() > limit as usize {
                    ids.pop();
                    ids.last().cloned()
                } else {
                    None
                };
                let mut runs = Vec::new();
                for id in ids {
                    runs.push(self.summary(&id).await?);
                }
                Ok(AuditListResponse {
                    protocol_version: AUDIT_PROTOCOL_VERSION,
                    runs,
                    next_cursor,
                }
                .into())
            }
            ClientRequest::AuditReleaseSlot { params, .. } => {
                anyhow::ensure!(
                    params.acknowledge_possible_concurrent_execution_and_charges,
                    "slot release requires explicit acknowledgement of possible concurrent execution and additional charges"
                );
                let _gate = self.gate.lock().await;
                self.store()
                    .await?
                    .release_capacity(&params.run_id, params.expected_revision, &params.reason)
                    .await?;
                self.wake.notify_one();
                self.progress(&params.run_id).await?;
                Ok(AuditReleaseSlotResponse {
                    run: self.summary(&params.run_id).await?,
                }
                .into())
            }
            ClientRequest::AuditCancel { params, .. } => {
                let _gate = self.gate.lock().await;
                let store = self.store().await?;
                let run = store.load_run(&params.run_id).await?;
                if !terminal(run.phase) && run.phase != RunPhase::Reporting {
                    store.request_cancel(&run.run_id, run.revision).await?;
                    // Only the worker interrupts, after durable intent has committed.
                    self.wake.notify_one();
                }
                Ok(AuditCancelResponse {
                    run: self.view(&params.run_id).await?,
                }
                .into())
            }
            ClientRequest::AuditAttempts { params, .. } => {
                Ok(self.attempt_views(&params.run_id).await?.into())
            }
            ClientRequest::AuditAttemptRead { params, .. } => {
                Ok(self.read_attempt(params).await?.into())
            }
            ClientRequest::AuditAttemptControl { params, .. } => {
                Ok(self.control_attempt(params).await?.into())
            }
            _ => anyhow::bail!("not an audit request"),
        }
    }

    /// Internal foundation commissioning. Frontends retain their current single-reviewer contract.
    async fn commission_roster(
        &self,
        params: AuditStartParams,
        reviewers: Vec<ReviewerSelection>,
        start_identity: &mut Option<String>,
    ) -> anyhow::Result<AuditRunView> {
        anyhow::ensure!(
            !reviewers.is_empty() && reviewers.len() <= 64,
            "reviewer roster must contain 1..64 entries"
        );
        let _gate = self.gate.lock().await;
        let capture = CaptureRequest {
            source: match &params.source {
                AuditSource::Snippet { name, content } => CaptureSource::Snippet {
                    logical_path: SnapshotPath::new(name.clone())?,
                    bytes: content.as_bytes().to_vec(),
                },
                AuditSource::File { name, path } => CaptureSource::File {
                    logical_path: SnapshotPath::new(name.clone())?,
                    source_path: AbsolutePathBuf::from_absolute_path(path)?,
                },
            },
            context_documents: ContextDocuments {
                audex_guidance: params
                    .audex_guidance
                    .as_ref()
                    .map(|text| text.as_bytes().to_vec()),
                agents_evidence: params
                    .agents_evidence
                    .as_ref()
                    .map(|text| text.as_bytes().to_vec()),
            },
        };
        let snapshots = self.snapshots();
        let captured = snapshots.capture(capture).await?;
        let snapshot = snapshots.open(captured.snapshot_id()).await?;
        let mut identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
            "version":1,"snapshotId":snapshot.snapshot_id(),"brief":params.brief,
            "provider":reviewers[0].provider,"model":reviewers[0].model,"reasoningEffort":reviewers[0].reasoning_effort,
        }))?);
        if reviewers.len() != 1 {
            identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
                "snapshotId": snapshot.snapshot_id(), "brief": params.brief, "reviewers": reviewers,
            }))?);
        }
        *start_identity = Some(identity.clone());
        if let Some(existing) = self
            .store()
            .await?
            .find_idempotent_request(&params.request_key, &identity)
            .await?
        {
            return self.view(&existing.run_id).await;
        }
        anyhow::ensure!(!self.stop.is_cancelled(), "server is quiescing");
        anyhow::ensure!(
            self.dispatch.available(),
            "reviewer execution is unavailable"
        );
        let mut commissions = Vec::with_capacity(reviewers.len());
        for selection in reviewers {
            // Preflight never starts a thread; all attempts are commissioned together.
            let prepared = Box::pin(reviewer::prepare(
                &self.config,
                self.models.as_ref(),
                &snapshot,
                reviewer::ReviewerRequest {
                    run_id: "preflight".to_owned(),
                    assignment_id: "preflight".to_owned(),
                    brief: params.brief.clone(),
                    provider: selection.provider.clone(),
                    model: selection.model.clone(),
                    reasoning_effort: selection
                        .reasoning_effort
                        .as_deref()
                        .map(str::parse)
                        .transpose()
                        .map_err(anyhow::Error::msg)?,
                },
                Arc::new(|_| {}),
            ))
            .await?;
            let policy_json = serde_json::to_string(&prepared.policy)?;
            let request_json = serde_json::to_string(&serde_json::json!({
                "version":1, "brief":params.brief, "provider":selection.provider, "model":selection.model,
                "reasoningEffort":prepared.options.config.model_reasoning_effort,
            }))?;
            let request_digest = sha256_hex(request_json.as_bytes());
            commissions.push(super::store::ReviewerCommission {
                request: CreateRunRequest {
                    request_key: params.request_key.clone(),
                    snapshot_id: snapshot.snapshot_id().to_string(),
                    manifest_digest: sha256_hex(&snapshot.manifest().to_pretty_bytes()?),
                    manifest_path: format!(
                        "snapshots/{}/manifest.json",
                        snapshot.snapshot_id().digest()
                    ),
                    brief_digest: sha256_hex(params.brief.as_bytes()),
                    policy_digest: prepared.policy_digest.clone(),
                    input_digest: request_digest.clone(),
                    provider: selection.provider,
                    model: selection.model,
                    reasoning_effort: prepared
                        .options
                        .config
                        .model_reasoning_effort
                        .as_ref()
                        .map(ToString::to_string),
                    reviewer_input_digest: sha256_hex(
                        format!("{request_digest}:{}", prepared.policy_digest).as_bytes(),
                    ),
                    operation_input_digest: request_digest,
                },
                input: Some((request_json, policy_json)),
            });
        }
        let created = self
            .store()
            .await?
            .create_roster(&commissions, Some(&identity))
            .await?;
        self.wake.notify_one();
        #[cfg(test)]
        anyhow::ensure!(
            !self.fail_start_response.load(Ordering::SeqCst),
            "injected acknowledgement failure"
        );
        self.view(&created.run_id).await
    }

    async fn view(&self, id: &str) -> anyhow::Result<AuditRunView> {
        self.run_view(id, true).await
    }

    async fn summary(&self, id: &str) -> anyhow::Result<AuditRunView> {
        self.run_view(id, false).await
    }

    async fn run_view(&self, id: &str, artefacts: bool) -> anyhow::Result<AuditRunView> {
        let store = self.store().await?;
        let run = if artefacts {
            store.load_run(id).await?
        } else {
            store.load_run_metadata(id).await?
        };
        Ok(AuditRunView {
            run_id: run.run_id.clone(),
            snapshot_id: run.snapshot_id.clone(),
            assignment_id: run.assignment_id.clone(),
            phase: phase(run.phase),
            operation_state: match run.operation_state {
                OperationState::Planned => AuditOperationState::Planned,
                OperationState::Dispatching => AuditOperationState::Dispatching,
                OperationState::Accepted => AuditOperationState::Accepted,
                OperationState::ObservedTerminal => AuditOperationState::ObservedTerminal,
                OperationState::Committed => AuditOperationState::Committed,
                OperationState::Failed => AuditOperationState::Failed,
                OperationState::Unknown => AuditOperationState::Unknown,
            },
            observation_pending: run.observation_pending,
            capacity_released: run.capacity_released,
            revision: run.revision,
            cancellation_requested: run.control_intent == "cancel_requested",
            outcome: run.outcome.clone(),
            raw_result: if artefacts {
                store.raw_result(&run)?.map(String::from_utf8).transpose()?
            } else {
                None
            },
            report: if artefacts {
                run.report
                    .as_ref()
                    .map(|report| store.read_report(report))
                    .transpose()?
                    .map(String::from_utf8)
                    .transpose()?
            } else {
                None
            },
        })
    }

    #[cfg(test)]
    async fn tick(self: &Arc<Self>) -> anyhow::Result<()> {
        self.recover_runs(false).await
    }

    async fn recover_runs(self: &Arc<Self>, scheduled: bool) -> anyhow::Result<()> {
        let candidates = self.store().await?.attempt_candidates().await?;
        if !scheduled {
            let results = futures::stream::iter(candidates)
                .map(|(id, _, _)| async move { self.drive_operation(&id).await })
                .buffer_unordered(64)
                .collect::<Vec<_>>()
                .await;
            for result in results {
                result?;
            }
            return Ok(());
        }
        for (operation, run_revision, attempt_revision) in candidates {
            if self.stop.is_cancelled() {
                break;
            }
            let version = (run_revision, attempt_revision);
            if self
                .polling
                .lock()
                .await
                .get(&operation)
                .is_some_and(|(previous, due, _)| {
                    *previous == version && *due > tokio::time::Instant::now()
                })
            {
                continue;
            }
            if !self
                .scheduled_attempts
                .lock()
                .expect("scheduled attempts lock")
                .insert(operation.clone())
            {
                continue;
            }
            let controller = self.clone();
            tokio::spawn(async move {
                let _pending = PendingDispatch {
                    pending: &controller.scheduled_attempts,
                    operation: operation.clone(),
                };
                let previous = controller.polling.lock().await.get(&operation).copied();
                let result = async {
                    controller.drive_operation(&operation).await?;
                    let after = controller
                        .store()
                        .await?
                        .load_operation_metadata(&operation)
                        .await?;
                    if after.phase == RunPhase::Reporting {
                        // Rendering is local work and needs no further observation delay.
                        controller.drive_operation(&operation).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                let failed = if let Err(error) = result {
                    tracing::warn!(operation_id=%operation, %error, "Audex attempt retained for recovery");
                    true
                } else {
                    false
                };
                if let Ok(after) = controller
                    .store()
                    .await
                    .expect("initialised store")
                    .load_operation_metadata(&operation)
                    .await
                {
                    let n = if failed || after.observation_pending {
                        previous.map_or(1, |(_, _, n)| n.saturating_add(1))
                    } else {
                        0
                    };
                    let delay = if n > 0 { 2u64.pow(n.min(6)).min(60) } else { 1 };
                    controller.polling.lock().await.insert(
                        operation,
                        (
                            (after.revision, after.attempt_revision),
                            tokio::time::Instant::now() + std::time::Duration::from_secs(delay),
                            n,
                        ),
                    );
                }
            });
        }
        Ok(())
    }

    #[cfg(test)]
    async fn drive(&self, id: &str) -> anyhow::Result<()> {
        let attempts = self.store().await?.attempts(id).await?;
        let results = futures::stream::iter(attempts)
            .map(|attempt| async move { self.drive_operation(&attempt.operation_id).await })
            .buffer_unordered(64)
            .collect::<Vec<_>>()
            .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize local workflow transitions; release the gate before every dispatcher await"
    )]
    async fn drive_operation(&self, operation: &str) -> anyhow::Result<()> {
        let gate = self.attempt_gate(operation);
        let _gate = gate.lock().await;
        if self
            .dispatch_pending
            .lock()
            .expect("pending attempts lock")
            .contains(operation)
        {
            return Ok(());
        }
        let store = self.store().await?;
        let mut run = store.run_for_operation(operation).await?;
        let id = run.run_id.clone();
        let id = id.as_str();
        if terminal(run.phase) {
            return Ok(());
        }
        let capture_state = store.capture_state(&run.assignment_id).await?;
        if capture_state.disposition != "active" {
            return Ok(());
        }
        if let Some(failure) = capture_state.failure {
            if !matches!(
                run.assignment_state,
                AssignmentState::Succeeded
                    | AssignmentState::Failed
                    | AssignmentState::Cancelled
                    | AssignmentState::Unknown
            ) {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        AssignmentState::Failed,
                        &failure,
                    )
                    .await?;
                return self.progress(id).await;
            }
        }
        if run.phase == RunPhase::Reporting {
            store.publish_report(id, run.revision).await?;
            return self.progress(id).await;
        }
        if run.operation_state == OperationState::Planned
            && run.control_intent == "cancel_requested"
        {
            store
                .finish_operation(
                    &run.operation_id,
                    run.attempt_revision,
                    AssignmentState::Cancelled,
                    "cancelled_before_dispatch",
                )
                .await?;
            return self.progress(id).await;
        }
        let snapshots = self.snapshots();
        let mut snapshot_issue = None;
        let snapshot = match snapshots
            .open(&SnapshotId::parse(run.snapshot_id.clone())?)
            .await
        {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                let integrity = matches!(
                    error,
                    SnapshotError::Integrity(_)
                        | SnapshotError::InvalidManifest(_)
                        | SnapshotError::Json(_)
                        | SnapshotError::InvalidPath(_)
                );
                if run.operation_state == OperationState::Planned && integrity {
                    store
                        .finish_operation(
                            &run.operation_id,
                            run.attempt_revision,
                            AssignmentState::Failed,
                            "snapshot_integrity_failure",
                        )
                        .await?;
                    self.progress(id).await?;
                    return Ok(());
                }
                snapshot_issue = Some(if integrity {
                    "snapshot_integrity_failure"
                } else {
                    "snapshot_access_unavailable"
                });
                None
            }
        };
        // Do not recapture a file or reread its original working directory on restart.
        let (_, policy_json, policy_digest, manifest_digest) =
            match store.attempt_input(operation).await {
                Ok(input) => input,
                Err(_) => {
                    store
                        .record_observation_issue(
                            &run.operation_id,
                            run.attempt_revision,
                            "retained_execution_input_unavailable",
                        )
                        .await?;
                    return self.progress(id).await;
                }
            };
        if sha256_hex(policy_json.as_bytes()) != policy_digest {
            store
                .record_observation_issue(
                    &run.operation_id,
                    run.attempt_revision,
                    "retained_policy_integrity_failure",
                )
                .await?;
            return self.progress(id).await;
        }
        let snapshot = if let Some(snapshot) = snapshot {
            if sha256_hex(&snapshot.manifest().to_pretty_bytes()?) != manifest_digest {
                snapshot_issue = Some("snapshot_manifest_binding_mismatch");
                None
            } else {
                Some(snapshot)
            }
        } else {
            None
        };
        if run.phase == RunPhase::Created {
            if snapshot.is_none() {
                store
                    .record_observation_issue(
                        &run.operation_id,
                        run.attempt_revision,
                        snapshot_issue.unwrap_or("snapshot_unavailable"),
                    )
                    .await?;
                return self.progress(id).await;
            }
            {
                let _binding = self.gate.lock().await;
                let current = store.run_for_operation(operation).await?;
                if current.phase == RunPhase::Created {
                    store.bind_snapshot(id, current.revision).await?;
                }
            }
            run = store.run_for_operation(operation).await?;
        }
        if run.operation_state == OperationState::Planned {
            if run.control_intent == "cancel_requested" {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        AssignmentState::Cancelled,
                        "cancelled_before_dispatch",
                    )
                    .await?;
            } else if snapshot.is_none() {
                store
                    .record_observation_issue(
                        &run.operation_id,
                        run.attempt_revision,
                        snapshot_issue.unwrap_or("snapshot_unavailable"),
                    )
                    .await?;
            } else if self.dispatch.available() && !self.stop.is_cancelled() {
                let now = chrono::Utc::now().timestamp_millis();
                if store
                    .claim_operation_with_limit(
                        &run.operation_id,
                        &self.owner,
                        now,
                        now + 30_000,
                        self.worker_limit,
                    )
                    .await?
                    != ClaimOutcome::Acquired
                {
                    return Ok(());
                }
                store
                    .record_dispatch(&run.operation_id, &self.owner, run.attempt_revision, now)
                    .await?;
                if self.stop.is_cancelled() {
                    return Ok(());
                }
                run = store.run_for_operation(operation).await?;
                self.dispatch_pending
                    .lock()
                    .expect("pending attempts lock")
                    .insert(operation.to_owned());
                let _pending = PendingDispatch {
                    pending: &self.dispatch_pending,
                    operation: operation.to_owned(),
                };
                drop(_gate);
                // Dispatching is durable before this boundary. No recovery path repeats it.
                let observation = self
                    .dispatch
                    .start(DispatchRequest {
                        run_id: id.to_string(),
                        assignment_id: run.assignment_id.clone(),
                        operation_id: run.operation_id.clone(),
                        snapshot_id: run.snapshot_id.clone(),
                        lease_owner: self.owner.clone(),
                    })
                    .await;
                self.reconcile_observation(&run, snapshot.as_ref(), snapshot_issue, observation)
                    .await?;
            }
        } else if matches!(
            run.operation_state,
            OperationState::Dispatching | OperationState::Accepted
        ) {
            // Another live controller can be between committing dispatching and
            // recording the dispatcher's receipt. Absence during its lease is
            // not crash evidence and must not turn its operation into unknown.
            if !store
                .observation_lease_ready(
                    &run.operation_id,
                    &self.owner,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await?
            {
                return Ok(());
            }
            if self.dispatch.capture(&run.operation_id).is_none() {
                store
                    .note_unavailable_capture_owner(
                        &run.operation_id,
                        &self.owner,
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await?;
            }
            drop(_gate);
            // Bound observation only, never submission. A stalled old runtime
            // cannot indefinitely monopolise the serial recovery worker.
            let observation = tokio::time::timeout(OBSERVATION_TIMEOUT, async {
                if run.control_intent == "cancel_requested" {
                    self.dispatch.interrupt(run.operation_id.clone()).await
                } else {
                    self.dispatch.inspect(run.operation_id.clone()).await
                }
            })
            .await
            .unwrap_or_else(|_| Observation::Deferred("execution_observation_timeout".to_string()));
            self.reconcile_observation(&run, snapshot.as_ref(), snapshot_issue, observation)
                .await?;
        }
        self.progress(id).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize reconciliation against local cancellation and completion; dispatcher calls have already returned"
    )]
    async fn reconcile_observation(
        &self,
        observed_run: &AuditRunRecord,
        snapshot: Option<&SnapshotHandle>,
        snapshot_issue: Option<&str>,
        observation: Observation,
    ) -> anyhow::Result<()> {
        let gate = self.attempt_gate(&observed_run.operation_id);
        let _gate = gate.lock().await;
        let current = self
            .store()
            .await?
            .run_for_operation(&observed_run.operation_id)
            .await?;
        anyhow::ensure!(
            current.operation_id == observed_run.operation_id
                && current.snapshot_id == observed_run.snapshot_id,
            "observation no longer belongs to the retained operation"
        );
        // A result already committed by another drive wins over any older
        // inspection or interruption. Reporting must not regress either.
        if self
            .store()
            .await?
            .capture_state(&current.assignment_id)
            .await?
            .disposition
            != "active"
        {
            return Ok(());
        }
        if terminal(current.phase)
            || current.phase == RunPhase::Reporting
            || matches!(
                current.assignment_state,
                AssignmentState::Succeeded
                    | AssignmentState::Failed
                    | AssignmentState::Cancelled
                    | AssignmentState::Unknown
            )
        {
            return Ok(());
        }
        if current.attempt_revision != observed_run.attempt_revision
            || current.control_intent != observed_run.control_intent
            || current.owner_generation != observed_run.owner_generation
        {
            let positive_evidence = matches!(
                &observation,
                Observation::Accepted { operation_id, snapshot_id, receipt, .. }
                    if operation_id == &current.operation_id
                        && snapshot_id == &current.snapshot_id
                        && !receipt.is_empty()
            );
            let completed = matches!(
                &observation,
                Observation::Accepted {
                    state: WorkState::Completed(_),
                    ..
                }
            );
            let cancelled_before_acceptance = current.operation_state
                == OperationState::Dispatching
                && current.control_intent == "cancel_requested"
                && matches!(
                    &observation,
                    Observation::Rejected(_) | Observation::Unsubmitted(_)
                );
            // Retain matching acceptance and completed results even if cancellation
            // arrived during the call. A definite pre-acceptance rejection also
            // closes cancellation safely. Other stale observations are superseded;
            // the next drive observes the current intent without dispatching again.
            if !cancelled_before_acceptance
                && (!positive_evidence
                    || (current.operation_state != OperationState::Dispatching && !completed))
            {
                return Ok(());
            }
        }
        let cleanup = matches!(&observation, Observation::Unsubmitted(_));
        self.apply(&current, snapshot, snapshot_issue, observation)
            .await?;
        drop(_gate);
        if cleanup {
            self.dispatch
                .cleanup_preparation(current.operation_id)
                .await;
        }
        Ok(())
    }

    fn attempt_gate(&self, operation: &str) -> Arc<Mutex<()>> {
        self.attempt_gates
            .lock()
            .expect("attempt gates lock")
            .entry(operation.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn apply(
        &self,
        run: &AuditRunRecord,
        snapshot: Option<&SnapshotHandle>,
        snapshot_issue: Option<&str>,
        observation: Observation,
    ) -> anyhow::Result<()> {
        let store = self.store().await?;
        let (receipt, state) = match observation {
            Observation::Unsubmitted(diagnostic) => {
                store
                    .finish_unsubmitted_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        if run.control_intent == "cancel_requested" {
                            AssignmentState::Cancelled
                        } else {
                            AssignmentState::Failed
                        },
                        &diagnostic,
                    )
                    .await?;
                return Ok(());
            }
            Observation::Deferred(diagnostic) => {
                store
                    .record_observation_issue(
                        &run.operation_id,
                        run.attempt_revision,
                        snapshot_issue.unwrap_or(&diagnostic),
                    )
                    .await?;
                return Ok(());
            }
            Observation::Uncertain(diagnostic) => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        AssignmentState::Unknown,
                        &diagnostic,
                    )
                    .await?;
                return Ok(());
            }
            Observation::Accepted {
                operation_id,
                snapshot_id,
                receipt,
                state,
            } if operation_id == run.operation_id
                && snapshot_id == run.snapshot_id
                && !receipt.is_empty() =>
            {
                (receipt, state)
            }
            Observation::Rejected(reason) if run.operation_state == OperationState::Dispatching => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        if run.control_intent == "cancel_requested" {
                            AssignmentState::Cancelled
                        } else {
                            AssignmentState::Failed
                        },
                        &reason,
                    )
                    .await?;
                return Ok(());
            }
            Observation::Accepted {
                operation_id,
                snapshot_id,
                ..
            } if operation_id != run.operation_id || snapshot_id != run.snapshot_id => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        AssignmentState::Unknown,
                        "acceptance_identity_contradiction",
                    )
                    .await?;
                return Ok(());
            }
            Observation::Rejected(reason) => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.attempt_revision,
                        AssignmentState::Unknown,
                        &format!("rejection_contradicts_acceptance: {reason}"),
                    )
                    .await?;
                return Ok(());
            }
            Observation::Absent | Observation::Accepted { .. } => {
                store
                    .record_observation_issue(
                        &run.operation_id,
                        run.attempt_revision,
                        "acceptance_or_outcome_unconfirmed",
                    )
                    .await?;
                return Ok(());
            }
        };
        let mut revision = run.attempt_revision;
        if run.operation_state == OperationState::Dispatching {
            store
                .record_acceptance(
                    &run.operation_id,
                    revision,
                    &format!("{}:accepted", run.operation_id),
                    &receipt,
                )
                .await?;
            revision = store
                .run_for_operation(&run.operation_id)
                .await?
                .attempt_revision;
        }
        match state {
            WorkState::Live => {
                if let Some(issue) = snapshot_issue {
                    store
                        .record_observation_issue(&run.operation_id, revision, issue)
                        .await?;
                } else if run.observation_pending {
                    store
                        .clear_observation_issue(&run.operation_id, revision)
                        .await?;
                }
            }
            WorkState::Completed(raw) => {
                let Some(snapshot) = snapshot else {
                    store
                        .record_observation_issue(
                            &run.operation_id,
                            revision,
                            snapshot_issue.unwrap_or("snapshot_unavailable_for_result"),
                        )
                        .await?;
                    return Ok(());
                };
                let capture = self.dispatch.capture(&run.operation_id);
                if let Some(failure) = capture.as_ref().and_then(|capture| capture.failure()) {
                    store
                        .finish_operation(
                            &run.operation_id,
                            revision,
                            AssignmentState::Failed,
                            failure,
                        )
                        .await?;
                    return Ok(());
                }
                let prefix = capture.as_ref().map(|capture| capture.control_prefix());
                let committed = store
                    .commit_result_with_prefix(
                        &run.operation_id,
                        revision,
                        snapshot,
                        &raw,
                        capture
                            .as_ref()
                            .zip(prefix.as_ref())
                            .map(|(capture, prefix)| (capture.identity(), prefix.as_slice())),
                    )
                    .await;
                if let Some(capture) = &capture {
                    capture.completion_finished(committed.is_ok());
                }
                match committed {
                    Err(super::store::AuditStoreError::Snapshot(_)) => {
                        store
                            .record_observation_issue(
                                &run.operation_id,
                                revision,
                                "snapshot_unavailable_for_result_validation",
                            )
                            .await?;
                    }
                    result => {
                        result?;
                    }
                }
            }
            WorkState::Failed(reason) => {
                self.finish_observed_operation(
                    &run.operation_id,
                    revision,
                    AssignmentState::Failed,
                    &reason,
                )
                .await?;
            }
            WorkState::Interrupted => {
                let state = if run.control_intent == "cancel_requested" {
                    AssignmentState::Cancelled
                } else {
                    AssignmentState::Failed
                };
                self.finish_observed_operation(&run.operation_id, revision, state, "interrupted")
                    .await?;
            }
            WorkState::Ambiguous => {
                store
                    .record_observation_issue(&run.operation_id, revision, "outcome_unconfirmed")
                    .await?;
            }
        }
        Ok(())
    }

    async fn finish_observed_operation(
        &self,
        operation: &str,
        revision: i64,
        state: AssignmentState,
        reason: &str,
    ) -> anyhow::Result<()> {
        let capture = self.dispatch.capture(operation);
        let prefix = capture.as_ref().map(|capture| capture.control_prefix());
        let result = self
            .store()
            .await?
            .finish_operation_with_prefix(
                operation,
                revision,
                state,
                reason,
                capture
                    .as_ref()
                    .zip(prefix.as_ref())
                    .map(|(capture, prefix)| (capture.identity(), prefix.as_slice())),
            )
            .await;
        if let Some(capture) = capture {
            capture.completion_finished(result.is_ok());
        }
        result?;
        Ok(())
    }

    async fn progress(&self, id: &str) -> anyhow::Result<()> {
        let run = self.store().await?.load_run_metadata(id).await?;
        let mut notified = self.notified_revisions.lock().await;
        let attempts = self.store().await?.attempts(id).await?;
        let mut stamps = Vec::with_capacity(attempts.len());
        let mut progress = Vec::with_capacity(attempts.len());
        for attempt in attempts {
            let sequence = self
                .store()
                .await?
                .capture_state(&attempt.assignment_id)
                .await?
                .sequence;
            progress.push(AuditAttemptProgress {
                assignment_id: attempt.assignment_id.clone(),
                revision: attempt.attempt_revision,
                output_sequence: sequence,
            });
            stamps.push((
                attempt.assignment_id,
                attempt.attempt_revision,
                attempt.owner_generation,
                sequence,
            ));
        }
        let version = (run.revision, stamps);
        if notified.get(id) == Some(&version) {
            return Ok(());
        }
        notified.insert(id.to_owned(), version);
        (self.notify)(AuditProgressNotification {
            run_id: run.run_id,
            revision: run.revision,
            phase: phase(run.phase),
            attempts: progress,
        });
        Ok(())
    }
}

impl Drop for AuditController {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn terminal(phase: RunPhase) -> bool {
    matches!(
        phase,
        RunPhase::Completed
            | RunPhase::CompletedWithFailures
            | RunPhase::Failed
            | RunPhase::Cancelled
    )
}

pub(crate) fn is_reviewer_source(source: Option<&codex_protocol::protocol::ThreadSource>) -> bool {
    matches!(source, Some(codex_protocol::protocol::ThreadSource::Feature(name)) if name == "audex-reviewer")
}

fn phase(value: RunPhase) -> AuditRunPhase {
    match value {
        RunPhase::Created => AuditRunPhase::Created,
        RunPhase::SnapshotBound => AuditRunPhase::SnapshotBound,
        RunPhase::Reviewing => AuditRunPhase::Reviewing,
        RunPhase::Reporting => AuditRunPhase::Reporting,
        RunPhase::Reconciling => AuditRunPhase::Reconciling,
        RunPhase::Completed => AuditRunPhase::Completed,
        RunPhase::CompletedWithFailures => AuditRunPhase::CompletedWithFailures,
        RunPhase::Failed => AuditRunPhase::Failed,
        RunPhase::Cancelled => AuditRunPhase::Cancelled,
    }
}

#[cfg(test)]
mod tests;
