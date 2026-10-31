use super::*;

mod attempts;
mod corrections;
mod execution;
mod foundations;
mod remaining;
mod residual;
use codex_models_manager::{ModelOrigin, manager::StaticModelsManager};
use codex_protocol::openai_models::ModelsResponse;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Fake {
    observations: AtomicUsize,
    starts: AtomicUsize,
    work: StdMutex<HashMap<String, Observation>>,
    captures: StdMutex<HashMap<String, crate::audex::capture::CaptureHandle>>,
    root: StdMutex<Option<AbsolutePathBuf>>,
    pause_start: AtomicBool,
    entered: Notify,
    release: Notify,
    pause_inspect: AtomicBool,
    inspect_entered: Notify,
    inspect_release: Notify,
    pause_interrupt: AtomicBool,
    interrupt_entered: Notify,
    interrupt_release: Notify,
}

impl Fake {
    fn replace(&self, operation: &str, state: WorkState) {
        let mut work = self.work.lock().expect("work lock");
        let Some(Observation::Accepted { state: current, .. }) = work.get_mut(operation) else {
            panic!("accepted fake work");
        };
        *current = state;
    }
}

impl Dispatch for Fake {
    fn capture(&self, operation: &str) -> Option<crate::audex::capture::CaptureHandle> {
        self.captures
            .lock()
            .expect("captures")
            .get(operation)
            .cloned()
    }
    fn request_interrupt(&self, _operation: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.interrupt_entered.notify_one();
            if self.pause_interrupt.swap(false, Ordering::SeqCst) {
                self.interrupt_release.notified().await;
            }
        })
    }

    fn start(&self, request: DispatchRequest) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let root = self
                .root
                .lock()
                .expect("root lock")
                .clone()
                .expect("configured fake root");
            let store = AuditStore::open(root.clone())
                .await
                .expect("open observer store");
            let run = store
                .run_for_operation(&request.operation_id)
                .await
                .expect("durable run before dispatch");
            assert_eq!(run.operation_state, OperationState::Dispatching);
            assert!(
                root.join("snapshots")
                    .join(
                        SnapshotId::parse(request.snapshot_id.clone())
                            .expect("snapshot ID")
                            .digest()
                    )
                    .join("manifest.json")
                    .as_path()
                    .exists()
            );
            if self.pause_start.load(Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let observed = Observation::Accepted {
                operation_id: request.operation_id.clone(),
                snapshot_id: request.snapshot_id,
                receipt: format!("receipt:{}", request.operation_id),
                state: WorkState::Live,
            };
            self.work
                .lock()
                .expect("work lock")
                .insert(request.operation_id, observed.clone());
            observed
        })
    }
    fn inspect(&self, operation: String) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            self.observations.fetch_add(1, Ordering::SeqCst);
            let observed = self
                .work
                .lock()
                .expect("work lock")
                .get(&operation)
                .cloned()
                .unwrap_or(Observation::Absent);
            if self.pause_inspect.swap(false, Ordering::SeqCst) {
                self.inspect_entered.notify_one();
                self.inspect_release.notified().await;
            }
            observed
        })
    }
    fn interrupt(&self, operation: String) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            self.observations.fetch_add(1, Ordering::SeqCst);
            let root = self.root.lock().expect("root lock").clone().expect("root");
            let store = AuditStore::open(root).await.expect("observer store");
            let ids = store.list_non_terminal_runs().await.expect("runs");
            let mut found = false;
            for id in ids {
                let run = store.load_run(&id).await.expect("run");
                if run.operation_id == operation {
                    assert_eq!(
                        run.control_intent, "cancel_requested",
                        "intent precedes interrupt"
                    );
                    found = true;
                }
            }
            assert!(found);
            let observed = {
                let mut work = self.work.lock().expect("work lock");
                if let Some(observation @ Observation::Accepted { .. }) = work.get_mut(&operation) {
                    if let Observation::Accepted { state, .. } = observation {
                        // Completion wins a simultaneous interruption.
                        if !matches!(state, WorkState::Completed(_)) {
                            *state = WorkState::Interrupted;
                        }
                    }
                    observation.clone()
                } else {
                    Observation::Absent
                }
            };
            if self.pause_interrupt.swap(false, Ordering::SeqCst) {
                self.interrupt_entered.notify_one();
                self.interrupt_release.notified().await;
            }
            observed
        })
    }
}

async fn fixture(
    temp: &tempfile::TempDir,
) -> (
    Arc<AuditController>,
    Arc<Fake>,
    Arc<StdMutex<Vec<AuditProgressNotification>>>,
) {
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        temp.path().to_path_buf(),
        Vec::new(),
    )
    .await
    .expect("config");
    let models = Arc::new(StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![codex_models_manager::model_info::model_info_from_slug(
                "audit-test",
            )],
        },
        ModelOrigin::ExplicitConfig,
    ));
    let fake = Arc::new(Fake::default());
    *fake.root.lock().expect("root lock") = Some(config.codex_home.join("audex"));
    let notifications = Arc::new(StdMutex::new(Vec::new()));
    let sink = notifications.clone();
    (
        AuditController::new_with_worker_limit(
            Arc::new(config),
            models,
            fake.clone(),
            Arc::new(move |event| sink.lock().expect("sink").push(event)),
            1, // Exercise the existing single-worker option in legacy compatibility tests.
        ),
        fake,
        notifications,
    )
}

async fn api(
    controller: &AuditController,
    method: &str,
    params: Value,
) -> Result<Value, JSONRPCErrorError> {
    let request: ClientRequest =
        serde_json::from_value(json!({"id":1,"method":method,"params":params}))
            .expect("typed client request");
    controller
        .handle(request)
        .await
        .map(|response| serde_json::to_value(response).expect("typed response"))
}

async fn start(controller: &AuditController, key: &str) -> String {
    api(controller, "audit/start", json!({
        "requestKey":key, "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Static correctness audit", "provider":controller.config.model_provider_id, "model":"audit-test",
    })).await.expect("start")["run"]["runId"].as_str().expect("run ID").to_string()
}

async fn read(controller: &AuditController, id: &str) -> Value {
    api(controller, "audit/read", json!({"runId":id}))
        .await
        .expect("read")["run"]
        .clone()
}

fn empty_result(run: &AuditRunRecord) -> Vec<u8> {
    serde_json::to_vec(&json!({"schema_version":1,"snapshot_id":run.snapshot_id,
        "assignment_id":run.assignment_id,"summary":"No findings.","findings":[]}))
    .expect("result")
}

#[tokio::test]
async fn typed_api_capture_disconnect_notifications_and_reporting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (controller, fake, notifications) = fixture(&temp).await;
    assert!(api(&controller, "audit/start", json!({
        "requestKey":"failed", "source":{"type":"file","name":"example.rs","path":temp.path().join("missing")},
        "brief":"Audit", "provider":"openai", "model":"audit-test",
    })).await.is_err());
    assert_eq!(
        api(&controller, "audit/list", json!({}))
            .await
            .expect("list")["runs"],
        json!([])
    );
    let id = start(&controller, "typed-start").await;
    assert_eq!(
        start(&controller, "typed-start").await,
        id,
        "duplicate typed starts are idempotent"
    );
    // Dropping the request/response (client ownership) has no effect on process-owned work.
    controller.tick().await.expect("dispatch");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    assert_eq!(read(&controller, &id).await["phase"], "reviewing");
    notifications.lock().expect("sink").clear(); // client missed every notification
    let run = controller
        .store()
        .await
        .expect("store")
        .load_run(&id)
        .await
        .expect("run");
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    controller.tick().await.expect("commit");
    controller.tick().await.expect("report");
    let result = read(&controller, &id).await;
    assert_eq!(result["phase"], "completed");
    assert!(
        result["report"]
            .as_str()
            .expect("report")
            .contains("No findings")
    );
    assert!(
        result["rawResult"]
            .as_str()
            .expect("raw result")
            .contains("findings")
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    controller.tick().await.expect("idempotent completed tick");
    assert_eq!(read(&controller, &id).await, result);
}

#[tokio::test]
async fn cancellation_is_durable_and_completion_wins() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "cancel-running").await;
    controller.tick().await.expect("dispatch");
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("cancel");
    controller.tick().await.expect("interrupt after intent");
    assert_eq!(read(&controller, &id).await["phase"], "cancelled");
    let id = start(&controller, "completion-race").await;
    controller.tick().await.expect("dispatch");
    let run = controller
        .store()
        .await
        .expect("store")
        .load_run(&id)
        .await
        .expect("run");
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("cancel racing completion");
    controller.tick().await.expect("completion preserved");
    controller.tick().await.expect("report");
    assert_eq!(read(&controller, &id).await["phase"], "completed");
}

#[tokio::test]
async fn competing_controllers_dispatch_once_and_reopen_reporting_locally() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (first, fake, _) = fixture(&temp).await;
    let second = AuditController::new(
        first.config.clone(),
        first.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    let id = start(&first, "lease-competition").await;
    let _ = tokio::join!(first.tick(), second.tick());
    first.tick().await.expect("settle");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    let run = first
        .store()
        .await
        .expect("store")
        .load_run(&id)
        .await
        .expect("run");
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    // Either controller can win the lease; only that live owner observes work
    // during its unexpired lease. Completion must not assume an ordering winner.
    let owner: String = sqlx::query_scalar("SELECT lease_owner FROM operations WHERE id=?")
        .bind(&run.operation_id)
        .fetch_one(&first.store().await.expect("store").pool_for_tests())
        .await
        .expect("lease holder");
    let holder = if owner == first.owner {
        &first
    } else {
        &second
    };
    holder.tick().await.expect("commit without report");
    assert_eq!(read(&first, &id).await["phase"], "reporting");
    drop(first);
    second.tick().await.expect("report after restart");
    assert_eq!(read(&second, &id).await["phase"], "completed");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn crash_windows_use_evidence_without_replaying_dispatch() {
    // Planned, dispatching without evidence, dispatching with acceptance, accepted live,
    // accepted terminal, interrupted and ambiguous histories.
    for mode in [
        "planned",
        "absent",
        "matching",
        "live",
        "terminal",
        "interrupted",
        "ambiguous",
        "failed",
        "rejected",
        "mismatched",
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (first, fake, _) = fixture(&temp).await;
        let id = start(&first, mode).await;
        let store = first.store().await.expect("store");
        let run = store.load_run(&id).await.expect("run");
        if mode != "planned" {
            let revision = store.bind_snapshot(&id, run.revision).await.expect("bind");
            let now = chrono::Utc::now().timestamp_millis() - 2000;
            store
                .claim_operation(&run.operation_id, "crashed-owner", now, now + 1000)
                .await
                .expect("claim");
            let revision = store
                .record_dispatch(&run.operation_id, "crashed-owner", revision, now)
                .await
                .expect("dispatching durable");
            if mode != "absent" {
                let state = match mode {
                    "terminal" => WorkState::Completed(empty_result(&run)),
                    "interrupted" => WorkState::Interrupted,
                    "ambiguous" => WorkState::Ambiguous,
                    "failed" => WorkState::Failed("reviewer_failed".to_string()),
                    _ => WorkState::Live,
                };
                fake.work.lock().expect("work").insert(
                    run.operation_id.clone(),
                    Observation::Accepted {
                        operation_id: run.operation_id.clone(),
                        snapshot_id: if mode == "mismatched" {
                            "wrong-snapshot".to_string()
                        } else {
                            run.snapshot_id.clone()
                        },
                        receipt: "positive-evidence".into(),
                        state,
                    },
                );
                if mode == "rejected" {
                    fake.work.lock().expect("work").insert(
                        run.operation_id.clone(),
                        Observation::Rejected("not_accepted".to_string()),
                    );
                }
                if !matches!(mode, "matching" | "rejected" | "mismatched") {
                    store
                        .record_acceptance(
                            &run.operation_id,
                            revision,
                            &format!("{}:accepted", run.operation_id),
                            "positive-evidence",
                        )
                        .await
                        .expect("accepted");
                }
            }
        }
        let recovered = AuditController::new(
            first.config.clone(),
            first.models.clone(),
            fake.clone(),
            Arc::new(|_| {}),
        );
        drop(first);
        recovered.tick().await.expect("recover");
        let view = read(&recovered, &id).await;
        match mode {
            "planned" | "matching" | "live" | "absent" | "ambiguous" => {
                assert_eq!(view["phase"], "reviewing", "{mode}")
            }
            "terminal" => assert_eq!(view["phase"], "reporting"),
            _ => assert_eq!(view["phase"], "failed", "{mode}"),
        }
        let recovered_run = recovered
            .store()
            .await
            .expect("store")
            .load_run(&id)
            .await
            .expect("run");
        if matches!(mode, "absent" | "ambiguous") {
            // Missing outcome evidence is not proof of termination
            // and must not implicitly release the global capacity reservation.
            assert_eq!(
                recovered_run.assignment_state,
                if mode == "ambiguous" {
                    AssignmentState::Running
                } else {
                    AssignmentState::Dispatching
                }
            );
            assert_eq!(view["observationPending"], true);
            assert_eq!(view["capacityReleased"], false);
        }
        if matches!(mode, "interrupted" | "failed" | "rejected") {
            assert_eq!(recovered_run.assignment_state, AssignmentState::Failed);
        }
        if mode == "mismatched" {
            assert_eq!(recovered_run.operation_state, OperationState::Unknown);
            assert_eq!(recovered_run.assignment_state, AssignmentState::Unknown);
            assert_eq!(view["observationPending"], false);
        }
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            usize::from(mode == "planned"),
            "{mode}"
        );
        let stable = view["revision"].clone();
        recovered.tick().await.expect("repeat recovery");
        if mode != "terminal" {
            assert_eq!(read(&recovered, &id).await["revision"], stable);
        }
    }
}

#[tokio::test]
async fn malformed_results_no_redispatch_and_quiesce_blocks_new_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "invalid-result").await;
    controller.tick().await.expect("dispatch");
    let run = controller
        .store()
        .await
        .expect("store")
        .load_run(&id)
        .await
        .expect("run");
    fake.replace(
        &run.operation_id,
        WorkState::Completed(b"not JSON".to_vec()),
    );
    controller.tick().await.expect("reject");
    controller.tick().await.expect("no retry");
    assert_eq!(
        read(&controller, &id).await["phase"],
        "completed_with_failures"
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    let invalid_id = start(&controller, "invalid-identities").await;
    controller.tick().await.expect("dispatch semantic case");
    let invalid_run = controller
        .store()
        .await
        .expect("store")
        .load_run(&invalid_id)
        .await
        .expect("run");
    let raw = serde_json::to_vec(&json!({
        "schema_version":1, "snapshot_id":"wrong-snapshot",
        "assignment_id":invalid_run.assignment_id, "summary":"Invalid identity", "findings":[],
    }))
    .expect("result");
    fake.replace(&invalid_run.operation_id, WorkState::Completed(raw));
    controller.tick().await.expect("semantic rejection");
    assert_eq!(
        read(&controller, &invalid_id).await["phase"],
        "completed_with_failures"
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    controller.quiesce().await;
    assert!(
        api(
            &controller,
            "audit/start",
            json!({
                "requestKey":"quiesced", "source":{"type":"snippet","name":"x","content":"x"},
                "brief":"Audit", "provider":"openai", "model":"audit-test",
            })
        )
        .await
        .is_err()
    );
    assert!(
        api(&controller, "audit/read", json!({"runId":id}))
            .await
            .is_ok()
    );
}

#[test]
fn daemon_partition_preserves_ordinary_thread_sources() {
    use codex_protocol::protocol::ThreadSource;
    assert!(is_reviewer_source(Some(&ThreadSource::Feature(
        "audex-reviewer".into()
    ))));
    assert!(!is_reviewer_source(None));
    assert!(!is_reviewer_source(Some(&ThreadSource::Feature(
        "ordinary-feature".into()
    ))));
}

#[tokio::test]
async fn cancellation_can_commit_while_acceptance_is_in_flight() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "in-flight-cancel").await;
    fake.pause_start.store(true, Ordering::SeqCst);
    let worker = controller.clone();
    let task = tokio::spawn(async move { worker.tick().await });
    fake.entered.notified().await;
    controller.drive(&id).await.expect("concurrent local drive");
    assert_eq!(
        controller
            .store()
            .await
            .expect("store")
            .load_run(&id)
            .await
            .expect("pending run")
            .operation_state,
        OperationState::Dispatching,
        "a local drive must not interpret an in-flight dispatch as missing evidence",
    );
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        api(&controller, "audit/cancel", json!({"runId":id})),
    )
    .await
    .expect("cancel is not blocked by dispatch")
    .expect("cancel");
    assert_eq!(cancelled["run"]["cancellationRequested"], true);
    fake.release.notify_one();
    task.await.expect("worker joined").expect("accepted");
    controller.tick().await.expect("interrupt");
    assert_eq!(read(&controller, &id).await["phase"], "cancelled");
}

#[tokio::test]
async fn background_worker_survives_notification_receiver_disconnect() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (fixture, fake, _) = fixture(&temp).await;
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let process_owned = AuditController::new(
        fixture.config.clone(),
        fixture.models.clone(),
        fake.clone(),
        Arc::new(move |notification| {
            let _ = sender.try_send(notification);
        }),
    );
    AuditController::spawn(&process_owned);
    let id = start(&process_owned, "disconnected-client").await;
    drop(receiver);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let run = process_owned
                .store()
                .await
                .expect("store")
                .load_run(&id)
                .await
                .expect("run");
            if run.operation_state == OperationState::Accepted {
                fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A new read client recovers completion despite losing all progress notifications.
        while read(&process_owned, &id).await["phase"] != "completed" {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background audit completes without a connected observer");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    process_owned.quiesce().await;
}

#[cfg(unix)]
#[tokio::test]
async fn startup_recovery_verifies_snapshot_before_any_dispatch() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("tempdir");
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "missing-snapshot").await;
    let run = controller
        .store()
        .await
        .expect("store")
        .load_run(&id)
        .await
        .expect("run");
    // Tamper only with a test artefact. Recovery must verify the retained identity
    // rather than recapturing the original input.
    let snapshot = controller.root.join("snapshots").join(
        SnapshotId::parse(run.snapshot_id)
            .expect("snapshot ID")
            .digest(),
    );
    let manifest = snapshot.join("manifest.json");
    std::fs::set_permissions(manifest.as_path(), std::fs::Permissions::from_mode(0o600))
        .expect("test manifest permissions");
    std::fs::write(manifest.as_path(), b"tampered").expect("tamper test manifest");
    controller.tick().await.expect("classify corrupt snapshot");
    assert_eq!(read(&controller, &id).await["phase"], "failed");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_commits_while_inspection_is_suspended() {
    for outcome in ["live", "completed", "superseded"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (controller, fake, _) = fixture(&temp).await;
        let id = start(&controller, outcome).await;
        controller.drive(&id).await.expect("dispatch");
        let run = controller
            .store()
            .await
            .expect("store")
            .load_run(&id)
            .await
            .expect("run");
        if outcome == "completed" {
            fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
        }
        fake.pause_inspect.store(true, Ordering::SeqCst);
        let worker = controller.clone();
        let observing_id = id.clone();
        let task = tokio::spawn(async move { worker.drive(&observing_id).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fake.inspect_entered.notified(),
        )
        .await
        .expect("inspection suspended");

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            api(&controller, "audit/cancel", json!({"runId":id})),
        )
        .await
        .expect("cancel commits before inspection resumes")
        .expect("cancel");
        assert_eq!(cancelled["run"]["cancellationRequested"], true);
        let durable = controller
            .store()
            .await
            .expect("store")
            .load_run(&id)
            .await
            .expect("durable intent");
        assert_eq!(durable.control_intent, "cancel_requested");
        assert!(durable.revision > run.revision);

        let completed = if outcome == "superseded" {
            fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
            controller.drive(&id).await.expect("concurrent completion");
            controller.drive(&id).await.expect("concurrent report");
            Some(read(&controller, &id).await)
        } else {
            None
        };
        fake.inspect_release.notify_one();
        task.await
            .expect("observer joined")
            .expect("reconcile observation");
        if let Some(completed) = completed {
            assert_eq!(
                read(&controller, &id).await,
                completed,
                "stale live observation cannot regress completion"
            );
        } else {
            controller
                .drive(&id)
                .await
                .expect("reconcile current intent or report");
            let expected = if outcome == "completed" {
                "completed"
            } else {
                "cancelled"
            };
            assert_eq!(read(&controller, &id).await["phase"], expected);
        }
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1, "{outcome}");
    }
}

#[tokio::test]
async fn suspended_interruption_does_not_block_cancel_or_regress_completed_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "suspended-interrupt").await;
    controller.drive(&id).await.expect("dispatch");
    let run = controller
        .store()
        .await
        .expect("store")
        .load_run(&id)
        .await
        .expect("run");
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("durable cancellation");
    fake.pause_interrupt.store(true, Ordering::SeqCst);
    let worker = controller.clone();
    let interrupting_id = id.clone();
    let task = tokio::spawn(async move { worker.drive(&interrupting_id).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fake.interrupt_entered.notified(),
    )
    .await
    .expect("interruption suspended");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        api(&controller, "audit/cancel", json!({"runId":id})),
    )
    .await
    .expect("repeated cancel is not blocked by interruption")
    .expect("cancel");

    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    controller.drive(&id).await.expect("concurrent completion");
    controller.drive(&id).await.expect("report");
    let completed = read(&controller, &id).await;
    assert_eq!(completed["phase"], "completed");
    fake.interrupt_release.notify_one();
    task.await
        .expect("interrupter joined")
        .expect("reconcile stale interruption");
    assert_eq!(read(&controller, &id).await, completed);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
}
