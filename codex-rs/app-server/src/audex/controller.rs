//! Process-owned workflow. Codex execution uses the same durable boundary as deterministic tests.
use super::reviewer;
use super::snapshot::manifest::sha256_hex;
use super::snapshot::{
    CaptureLimits, CaptureRequest, CaptureSource, ContextDocuments, SnapshotHandle, SnapshotId,
    SnapshotPath, SnapshotStore,
};
use super::store::{
    AssignmentState, AuditRunRecord, AuditStore, ClaimOutcome, CreateRunRequest, OperationState,
    RunPhase,
};
use codex_app_server_protocol::*;
use codex_core::config::Config;
use codex_exec_server::LocalFileSystem;
use codex_models_manager::manager::SharedModelsManager;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::result::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify, OnceCell};
use tokio_util::sync::CancellationToken;

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
    fn start(&self, request: DispatchRequest) -> BoxFuture<'_, Observation>;
    fn inspect(&self, operation_id: String) -> BoxFuture<'_, Observation>;
    fn interrupt(&self, operation_id: String) -> BoxFuture<'_, Observation>;
}

/// Keep local recovery from observing a dispatch whose acceptance call has not
/// returned. This is advisory runtime state; cross-process recovery uses the lease.
struct PendingDispatch<'a>(&'a AtomicBool);

impl Drop for PendingDispatch<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub(crate) struct AuditController {
    root: AbsolutePathBuf,
    config: Arc<Config>,
    models: SharedModelsManager,
    store: OnceCell<AuditStore>,
    dispatch: Arc<dyn Dispatch>,
    owner: String,
    stop: CancellationToken,
    wake: Notify,
    /// Orders local start/cancel/shutdown against crossing the dispatch boundary.
    gate: Mutex<()>,
    dispatch_pending: AtomicBool,
    notify: Arc<dyn Fn(AuditProgressNotification) + Send + Sync>,
}

impl AuditController {
    pub(crate) fn service(
        config: Arc<Config>,
        threads: Arc<codex_core::ThreadManager>,
        history: Arc<dyn codex_thread_store::ThreadStore>,
        listen: super::dispatch::Listen,
        outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    ) -> Arc<Self> {
        let dispatch = Arc::new(super::dispatch::CodexDispatch::new(
            Arc::clone(&config),
            Arc::clone(&threads),
            history,
            listen,
        ));
        let controller = Self::new(
            config,
            threads.get_models_manager(),
            dispatch,
            Arc::new(move |progress| {
                outgoing.try_send_server_notification(ServerNotification::AuditProgress(progress));
            }),
        );
        Self::spawn(&controller);
        controller
    }

    fn new(
        config: Arc<Config>,
        models: SharedModelsManager,
        dispatch: Arc<dyn Dispatch>,
        notify: Arc<dyn Fn(AuditProgressNotification) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            root: config.codex_home.join("audex"),
            config,
            models,
            dispatch,
            notify,
            store: OnceCell::new(),
            owner: uuid::Uuid::now_v7().to_string(),
            stop: CancellationToken::new(),
            wake: Notify::new(),
            gate: Mutex::new(()),
            dispatch_pending: AtomicBool::new(false),
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
            loop {
                let Some(controller) = weak.upgrade() else {
                    break;
                };
                if stop.is_cancelled() {
                    break;
                }
                if let Err(error) = controller.tick().await {
                    tracing::warn!(%error, "Audex workflow unavailable; retained state was not replaced");
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = controller.wake.notified() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
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
        Box::pin(self.handle_inner(request))
            .await
            .map_err(|error| crate::error_code::invalid_request(format!("Audex: {error}")))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize local durable start and cancellation transitions against dispatch admission"
    )]
    async fn handle_inner(&self, request: ClientRequest) -> anyhow::Result<ClientResponsePayload> {
        match request {
            ClientRequest::AuditStart { params, .. } => {
                let _gate = self.gate.lock().await;
                anyhow::ensure!(!self.stop.is_cancelled(), "server is quiescing");
                anyhow::ensure!(
                    self.dispatch.available(),
                    "reviewer execution is unavailable"
                );
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
                // IDs are not policy fields. Prepare once for preflight before creating durable work;
                // later dispatch construction uses the durable run and assignment identifiers.
                let prepared = Box::pin(reviewer::prepare(
                    &self.config,
                    self.models.as_ref(),
                    &snapshot,
                    reviewer::ReviewerRequest {
                        run_id: "preflight".to_string(),
                        assignment_id: "preflight".to_string(),
                        brief: params.brief.clone(),
                        provider: params.provider.clone(),
                        model: params.model.clone(),
                        reasoning_effort: params
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
                // Source and project documents remain immutable snapshot objects, not
                // duplicate large blobs in SQLite. Recovery needs only this audit input.
                let request_json = serde_json::to_string(&serde_json::json!({
                    "version":1, "brief":params.brief, "provider":params.provider,
                    "model":params.model,
                    "reasoningEffort":prepared.options.config.model_reasoning_effort,
                }))?;
                let request_digest = sha256_hex(request_json.as_bytes());
                let created = self
                    .store()
                    .await?
                    .create_run_with_input(
                        &CreateRunRequest {
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
                            provider: params.provider,
                            model: params.model,
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
                        Some((&request_json, &policy_json)),
                    )
                    .await?;
                self.wake.notify_one();
                Ok(AuditStartResponse {
                    run: self.view(&created.run_id).await?,
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
                    runs.push(self.view(&id).await?);
                }
                Ok(AuditListResponse {
                    protocol_version: AUDIT_PROTOCOL_VERSION,
                    runs,
                    next_cursor,
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
            _ => anyhow::bail!("not an audit request"),
        }
    }

    async fn view(&self, id: &str) -> anyhow::Result<AuditRunView> {
        let store = self.store().await?;
        let run = store.load_run(id).await?;
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
            revision: run.revision,
            cancellation_requested: run.control_intent == "cancel_requested",
            outcome: run.outcome.clone(),
            raw_result: store.raw_result(&run)?.map(String::from_utf8).transpose()?,
            report: run
                .report
                .as_ref()
                .map(|report| store.read_report(report))
                .transpose()?
                .map(String::from_utf8)
                .transpose()?,
        })
    }

    async fn tick(&self) -> anyhow::Result<()> {
        let store = self.store().await?;
        for id in store.list_non_terminal_runs().await? {
            if self.stop.is_cancelled() {
                break;
            }
            if let Err(error) = self.drive(&id).await {
                tracing::warn!(run_id=%id, %error, "Audex run retained for recovery");
            }
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize local workflow transitions; release the gate before every dispatcher await"
    )]
    async fn drive(&self, id: &str) -> anyhow::Result<()> {
        let _gate = self.gate.lock().await;
        if self.dispatch_pending.load(Ordering::SeqCst) {
            return Ok(());
        }
        let store = self.store().await?;
        let mut run = store.load_run(id).await?;
        let snapshots = self.snapshots();
        let snapshot = match snapshots
            .open(&SnapshotId::parse(run.snapshot_id.clone())?)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.revision,
                        AssignmentState::Failed,
                        "snapshot_integrity_failure",
                    )
                    .await?;
                self.progress(id).await?;
                return Ok(());
            }
        };
        // Do not recapture a file or reread its original working directory on restart.
        let (_, policy_json, policy_digest, manifest_digest) = store.controller_input(id).await?;
        anyhow::ensure!(
            sha256_hex(policy_json.as_bytes()) == policy_digest,
            "retained reviewer policy digest mismatch"
        );
        anyhow::ensure!(
            sha256_hex(&snapshot.manifest().to_pretty_bytes()?) == manifest_digest,
            "retained snapshot manifest digest mismatch"
        );
        if run.phase == RunPhase::Reporting {
            store.publish_report(id, run.revision).await?;
            return self.progress(id).await;
        }
        if run.phase == RunPhase::Created {
            store.bind_snapshot(id, run.revision).await?;
            run = store.load_run(id).await?;
        }
        if run.operation_state == OperationState::Planned {
            if run.control_intent == "cancel_requested" {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.revision,
                        AssignmentState::Cancelled,
                        "cancelled_before_dispatch",
                    )
                    .await?;
            } else if self.dispatch.available() && !self.stop.is_cancelled() {
                let now = chrono::Utc::now().timestamp_millis();
                if store
                    .claim_operation(&run.operation_id, &self.owner, now, now + 30_000)
                    .await?
                    != ClaimOutcome::Acquired
                {
                    return Ok(());
                }
                store
                    .record_dispatch(&run.operation_id, &self.owner, run.revision, now)
                    .await?;
                if self.stop.is_cancelled() {
                    return Ok(());
                }
                run = store.load_run(id).await?;
                self.dispatch_pending.store(true, Ordering::SeqCst);
                let _pending = PendingDispatch(&self.dispatch_pending);
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
                self.reconcile_observation(&run, &snapshot, observation)
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
            drop(_gate);
            let observation = if run.control_intent == "cancel_requested" {
                self.dispatch.interrupt(run.operation_id.clone()).await
            } else {
                self.dispatch.inspect(run.operation_id.clone()).await
            };
            self.reconcile_observation(&run, &snapshot, observation)
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
        snapshot: &SnapshotHandle,
        observation: Observation,
    ) -> anyhow::Result<()> {
        let _gate = self.gate.lock().await;
        let current = self.store().await?.load_run(&observed_run.run_id).await?;
        anyhow::ensure!(
            current.operation_id == observed_run.operation_id
                && current.snapshot_id == observed_run.snapshot_id,
            "observation no longer belongs to the retained operation"
        );
        // A result already committed by another drive wins over any older
        // inspection or interruption. Reporting must not regress either.
        if terminal(current.phase) || current.phase == RunPhase::Reporting {
            return Ok(());
        }
        if current.revision != observed_run.revision {
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
        self.apply(&current, snapshot, observation).await
    }

    async fn apply(
        &self,
        run: &AuditRunRecord,
        snapshot: &SnapshotHandle,
        observation: Observation,
    ) -> anyhow::Result<()> {
        let store = self.store().await?;
        let (receipt, state) = match observation {
            Observation::Unsubmitted(diagnostic) => {
                store
                    .finish_unsubmitted_operation(
                        &run.operation_id,
                        run.revision,
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
                    .record_observation_issue(&run.operation_id, run.revision, &diagnostic)
                    .await?;
                return Ok(());
            }
            Observation::Uncertain(diagnostic) => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.revision,
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
                        run.revision,
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
            _ => {
                store
                    .finish_operation(
                        &run.operation_id,
                        run.revision,
                        AssignmentState::Unknown,
                        "acceptance_or_outcome_unknown",
                    )
                    .await?;
                return Ok(());
            }
        };
        let mut revision = run.revision;
        if run.operation_state == OperationState::Dispatching {
            store
                .record_acceptance(
                    &run.operation_id,
                    revision,
                    &format!("{}:accepted", run.operation_id),
                    &receipt,
                )
                .await?;
            revision = store.load_run(&run.run_id).await?.revision;
        }
        match state {
            WorkState::Live => {}
            WorkState::Completed(raw) => {
                store
                    .commit_seeded_result(&run.operation_id, revision, snapshot.manifest(), &raw)
                    .await?;
            }
            WorkState::Failed(reason) => {
                store
                    .finish_operation(
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
                store
                    .finish_operation(&run.operation_id, revision, state, "interrupted")
                    .await?;
            }
            WorkState::Ambiguous => {
                store
                    .finish_operation(
                        &run.operation_id,
                        revision,
                        AssignmentState::Unknown,
                        "outcome_unknown",
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn progress(&self, id: &str) -> anyhow::Result<()> {
        let run = self.store().await?.load_run(id).await?;
        (self.notify)(AuditProgressNotification {
            run_id: run.run_id,
            revision: run.revision,
            phase: phase(run.phase),
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
