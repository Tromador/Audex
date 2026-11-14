//! Subscription exception through owned pipes, journal, controller and frozen reports.
use super::capture::{assert_no_private_files, settled};
use super::*;

#[path = "repairs/finished_output.rs"]
mod finished_output;

pub(super) async fn seed(
    controller: &AuditController,
    temp: &tempfile::TempDir,
    scenario: Value,
) -> anyhow::Result<String> {
    let id = seed_external(controller, temp, "repairs").await?;
    std::fs::write(
        temp.path().join("fake-client"),
        include_str!("../../../../external/fake_repairs.py"),
    )?;
    std::fs::write(
        temp.path().join("stream-scenario.json"),
        scenario.to_string(),
    )?;
    Ok(id)
}

async fn notices(store: &AuditStore, assignment: &str) -> anyhow::Result<Vec<Value>> {
    Ok(store
        .activity(assignment, 0, 256)
        .await?
        .into_iter()
        .filter(|event| event.kind == "status" && event.payload.contains("\"label\""))
        .map(|event| serde_json::from_str(&event.payload))
        .collect::<Result<_, _>>()?)
}

async fn one_prompt(
    temp: &tempfile::TempDir,
    controller: &Arc<AuditController>,
    operation: &str,
) -> anyhow::Result<()> {
    assert_eq!(
        receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
        1
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("stream-launches"))?
            .lines()
            .count(),
        1
    );
    super::lifecycle_qualification::reconcile_until(controller, operation).await?;
    assert_no_private_files(controller.root.as_path())?;
    Ok(())
}

#[test]
fn client_continuations_complete_without_a_second_execution() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-client-continuation", || async {
        for (mode, prompts, count) in [
            ("max_tokens", true, 1),
            ("end_turn", false, 1),
            ("tool_use", true, 1),
            ("max_tokens", true, 5),
            ("end_turn", true, 3),
            ("tool_use", true, 4),
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed(
                &controller,
                &temp,
                json!({"mode":mode,"prompts":prompts,
                "count":count,"duplicates":true,"complete":true,"partial_start":true}),
            )
            .await?;
            let run = settled(&controller, &id).await?;
            assert_eq!(
                run.assignment_state,
                AssignmentState::Succeeded,
                "{mode}/{count}"
            );
            let store = controller.store().await?;
            assert_eq!(store.attempts(&id).await?.len(), 1);
            assert!(
                store
                    .client_terminal_result(&run.operation_id)
                    .await?
                    .is_some()
            );
            let events = notices(store, &run.assignment_id).await?;
            assert_eq!(events.len(), count);
            assert!(
                store
                    .generation_records(&run.assignment_id)
                    .await?
                    .is_empty()
            );
            let before = serde_json::to_vec(&store.freeze_report(&id).await?)?;
            one_prompt(&temp, &controller, &run.operation_id).await?;
            let reopened = AuditStore::open(controller.root.clone()).await?;
            assert_eq!(
                serde_json::to_vec(&reopened.freeze_report(&id).await?)?,
                before
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn client_retry_notification_preserves_partial_output_until_operator_stop() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-client-retry-observation", || async {
        for mode in ["resend", "unknown_prompt", "refusal", "unexpected"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed(&controller, &temp, json!({"mode":mode})).await?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while notices(store, &run.assignment_id).await?.is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                anyhow::Ok(())
            })
            .await??;
            assert!(
                store
                    .local_execution_view(&run.operation_id)
                    .await?
                    .unwrap()
                    .stop_requested_at_ms
                    .is_none()
            );
            assert_eq!(store.attempts(&id).await?.len(), 1);
            controller.abort_audit(&id).await?;
            let before = store.activity(&run.assignment_id, 0, 256).await?;
            one_prompt(&temp, &controller, &run.operation_id).await?;
            assert_eq!(store.activity(&run.assignment_id, 0, 256).await?, before);
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn policy_disclosure_survives_exhausted_or_conflicting_public_capture() -> anyhow::Result<()> {
    run_test_with_large_stack("c72-capture-bound", || async {
        for mode in ["max_tokens", "conflicting_prefix"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed(
                &controller,
                &temp,
                json!({"mode":mode,"gate_start":true,"subsequent":false}),
            )
            .await?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let capture = controller.dispatch.capture(&run.operation_id).unwrap();
            let flush = capture.hold_flush().await;
            if mode == "max_tokens" {
                sqlx::query("UPDATE audit_attempts SET output_bytes=? WHERE assignment_id=?")
                    .bind(crate::audex::store::activity::MAX_CAPTURE_BYTES)
                    .bind(&run.assignment_id)
                    .execute(&store.pool_for_tests())
                    .await?;
            }
            std::fs::write(temp.path().join("before-public.release"), "go")?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if !notices(store, &run.assignment_id).await?.is_empty() {
                        break anyhow::Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await??;
            let state = store.capture_state(&run.assignment_id).await?;
            assert!(if mode == "max_tokens" {
                state.truncated
            } else {
                state.failure.is_some()
            });
            controller.abort_audit(&id).await?;
            drop(flush);
            assert!(
                serde_json::to_string(&store.freeze_report(&id).await?)?
                    .contains("client-initiated repair occurred")
            );
            one_prompt(&temp, &controller, &run.operation_id).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C72 capture {mode}: failed public prefix remains incomplete, bounded policy marker survived Abort and freeze"
            );
        }
        Ok(())
    })
}

#[test]
fn abnormal_stop_alone_is_not_a_corrective_send() -> anyhow::Result<()> {
    run_test_with_large_stack("c72-no-send", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = seed(
            &controller,
            &temp,
            json!({"mode":"max_tokens","prompts":false,"subsequent":false}),
        )
        .await?;
        controller.drive(&id).await?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if store
                    .activity(&run.assignment_id, 0, 100)
                    .await?
                    .iter()
                    .any(|e| e.payload == "Incomplete finding draft.")
                {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert!(notices(store, &run.assignment_id).await?.is_empty());
        assert!(
            store
                .local_execution_view(&run.operation_id)
                .await?
                .unwrap()
                .stop_requested_at_ms
                .is_none()
        );
        controller.abort_audit(&id).await?;
        let frozen = serde_json::to_value(store.freeze_report(&id).await?)?;
        assert_eq!(
            frozen["reviewers"][0]["provider_facts"]["subscription"]["repair_occurrences"],
            json!({"status":"unavailable","reason":"no retained evidence at selection"})
        );
        one_prompt(&temp, &controller, &run.operation_id).await?;
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C72 abnormal stop alone: no invented repair occurrence or automatic stop; explicit Abort terminates owned child"
        );
        Ok(())
    })
}
fn hold_continuation() -> (
    crate::audex::dispatch::DispatchTestHooks,
    Arc<Notify>,
    Arc<tokio::sync::Semaphore>,
) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, barrier) = (entered.clone(), release.clone());
    let hooks = crate::audex::dispatch::DispatchTestHooks {
        checkpoint: Some(Arc::new(move |name| {
            let (signal, barrier) = (signal.clone(), barrier.clone());
            Box::pin(async move {
                if name == "client_continuation_observed" {
                    signal.notify_one();
                    barrier.acquire().await.unwrap().forget();
                }
            })
        })),
        ..Default::default()
    };
    (hooks, entered, release)
}

#[test]
fn repair_disclosure_survives_held_flush_and_operator_controls_including_supersession()
-> anyhow::Result<()> {
    run_test_with_large_stack("c72-controls", || async {
        for action in ["cancel", "ignore", "abort", "retry"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (hooks, entered, release) = hold_continuation();
            let (controller, threads) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                hooks,
            )
            .await;
            let id = seed(
                &controller,
                &temp,
                json!({"mode":"max_tokens","gate_start":true,"subsequent":false}),
            )
            .await?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let capture = controller.dispatch.capture(&run.operation_id).unwrap();
            let flush = capture.hold_flush().await;
            std::fs::write(temp.path().join("before-public.release"), "go")?;
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            assert_eq!(
                notices(store, &run.assignment_id).await?.len(),
                1,
                "notice must not wait on periodic flush"
            );
            if action == "retry" {
                std::fs::write(
                    temp.path().join("fake-client"),
                    include_str!("../../../../external/fake_stream.py"),
                )?;
                std::fs::write(
                    temp.path().join("stream-scenario.json"),
                    json!({"mode":"valid"}).to_string(),
                )?;
            }
            if action == "abort" {
                controller.abort_audit(&id).await?;
            } else {
                api(&controller, "audit/attempt/control", json!({"runId":id,"assignmentId":run.assignment_id,
                    "actionKey":action,"action":action,"reason":"repair disposition", "authoriseRetry":action=="retry",
                    "acknowledgePossibleOverlap":true})).await.expect("exact control");
            }
            let before = store.activity(&run.assignment_id, 0, 256).await?;
            drop(flush);
            release.add_permits(1);
            if action != "retry" {
                one_prompt(&temp, &controller, &run.operation_id).await?;
            }
            assert_eq!(store.activity(&run.assignment_id, 0, 256).await?, before);
            if action == "retry" {
                let attempts = store.attempts(&id).await?;
                let replacement = attempts
                    .iter()
                    .find(|a| a.assignment_id != run.assignment_id)
                    .unwrap();
                assert_eq!(
                    settled(&controller, &id).await?.assignment_state,
                    AssignmentState::Succeeded
                );
                assert!(notices(store, &replacement.assignment_id).await?.is_empty());
                let original = store.client_launch(&run.operation_id).await?.unwrap();
                let successor = store
                    .client_launch(&replacement.operation_id)
                    .await?
                    .unwrap();
                assert_ne!(original.execution_id, successor.execution_id);
                let launches = std::fs::read_to_string(temp.path().join("stream-launches"))?;
                assert_eq!(
                    launches
                        .lines()
                        .filter(|id| *id == original.execution_id)
                        .count(),
                    1
                );
                assert_eq!(
                    launches
                        .lines()
                        .filter(|id| *id == successor.execution_id)
                        .count(),
                    1
                );
                super::lifecycle_qualification::reconcile_until(&controller, &run.operation_id)
                    .await?;
                super::lifecycle_qualification::reconcile_until(
                    &controller,
                    &replacement.operation_id,
                )
                .await?;
                assert_no_private_files(controller.root.as_path())?;
            }
            let frozen = serde_json::to_value(store.freeze_report(&id).await?)?;
            // Existing report selection follows the current attempt only. A fresh
            // authorised successor must not inherit its predecessor's repair label.
            assert_eq!(
                frozen
                    .to_string()
                    .contains("client-initiated repair occurred"),
                action != "retry"
            );
            assert_eq!(notices(store, &run.assignment_id).await?.len(), 1);
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C72 {action}: exact-attempt disclosure retained despite held flush, old observation cannot retarget successor or rewrite frozen history"
            );
        }
        Ok(())
    })
}
