//! Subscription exception through owned pipes, journal, controller and frozen reports.
use super::capture::{assert_no_private_files, settled};
use super::*;

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

async fn assert_anomalous_frozen_report(
    controller: &AuditController,
    id: &str,
    events: &[Value],
) -> anyhow::Result<()> {
    controller.abort_audit(id).await?;
    let store = controller.store().await?;
    let frozen = serde_json::to_value(store.freeze_report(id).await?)?;
    let facts = &frozen["reviewers"][0]["provider_facts"]["subscription"];
    let count = &facts["repair_occurrences"];
    assert_eq!(count["status"], "unavailable");
    assert!(count.get("value").is_none());
    assert!(
        count["reason"]
            .as_str()
            .unwrap()
            .contains("client-policy anomalies")
    );
    let disclosure = facts["repair_disclosure"]["value"].as_str().unwrap();
    for event in events {
        assert!(disclosure.contains(&event.to_string()));
    }
    store.publish_report(id, 0).await?;
    let report = store.load_run(id).await?.report.unwrap();
    let markdown = std::fs::read_to_string(controller.root.join(&report.relative_path))?;
    assert!(markdown.contains("Subscription repair occurrences: unavailable"));
    assert!(!markdown.contains("Subscription repair occurrences: 0"));
    assert!(markdown.contains("client request policy anomaly"));
    Ok(())
}

fn hold(
    stage: &'static str,
) -> (
    crate::audex::dispatch::DispatchTestHooks,
    Arc<Notify>,
    Arc<tokio::sync::Semaphore>,
) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, barrier) = (entered.clone(), release.clone());
    let terminal_ready = Arc::new(tokio::sync::Semaphore::new(0));
    let hooks = crate::audex::dispatch::DispatchTestHooks {
        checkpoint: Some(Arc::new(move |name| {
            let (signal, barrier) = (signal.clone(), barrier.clone());
            let terminal_ready = terminal_ready.clone();
            Box::pin(async move {
                if stage == "client_terminal_received" && name == "client_policy_stop" {
                    terminal_ready.acquire().await.unwrap().forget();
                }
                if name == stage {
                    terminal_ready.add_permits(1);
                    signal.notify_one();
                    barrier.acquire().await.unwrap().forget();
                }
            })
        })),
        ..Default::default()
    };
    (hooks, entered, release)
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
fn all_qualified_prompts_and_subsequent_generations_stop_with_durable_partial_history()
-> anyhow::Result<()> {
    run_test_with_large_stack("c72-classes", || async {
        for (trigger, class) in [
            ("max_tokens", "max_tokens_continuation"),
            ("end_turn", "empty_end_turn"),
            ("tool_use", "missing_tool_call"),
        ] {
            for prompts in [false, true] {
                let temp = tempfile::tempdir()?;
                let server = responses::start_mock_server().await;
                let (controller, threads) = real_fixture(&temp, &server.uri()).await;
                let id = seed(
                    &controller,
                    &temp,
                    json!({"mode":trigger,"prompts":prompts,"partial_start":true,"streamed_trigger":!prompts}),
                )
                .await?;
                let run = settled(&controller, &id).await?;
                assert!(matches!(
                    run.assignment_state,
                    AssignmentState::Failed | AssignmentState::Unknown
                ));
                let store = controller.store().await?;
                let events = notices(store, &run.assignment_id).await?;
                assert_eq!(events.len(), 1, "{trigger}, prompts={prompts}");
                assert_eq!(events[0]["label"], "client-initiated repair occurred");
                assert_eq!(events[0]["class"], class);
                assert_eq!(
                    events[0]["evidence"],
                    if prompts {
                        "fixed_corrective_prompt"
                    } else {
                        "subsequent_generation"
                    }
                );
                assert_eq!(
                    store.capture_state(&run.assignment_id).await?.disposition,
                    "active"
                );
                assert!(
                    store
                        .client_terminal_result(&run.operation_id)
                        .await?
                        .is_none()
                );
                assert!(
                    store
                        .generation_records(&run.assignment_id)
                        .await?
                        .is_empty()
                );
                // Failure remains wanted until an explicit operator decision.
                assert!(store.freeze_report(&id).await.is_err());
                controller.abort_audit(&id).await?;
                let frozen = store.freeze_report(&id).await?;
                assert!(frozen.reviewers[0].completed.is_none());
                let facts = serde_json::to_value(&frozen)?;
                assert_eq!(
                    facts["reviewers"][0]["provider_facts"]["subscription"]["repair_occurrences"]["value"],
                    1
                );
                assert!(
                    facts
                        .to_string()
                        .contains("client-initiated repair occurred")
                );
                one_prompt(&temp, &controller, &run.operation_id).await?;
                threads
                    .shutdown_all_threads_bounded(Duration::from_secs(5))
                    .await;
                println!(
                    "C72 {class}, prompts={prompts}: durable repair, incomplete wanted history, explicit Abort, no replay"
                );
            }
        }
        Ok(())
    })
}

#[test]
fn duplicate_observations_do_not_hide_distinct_repairs_or_qualification_overruns()
-> anyhow::Result<()> {
    run_test_with_large_stack("c72-occurrences", || async {
        for (trigger, bound) in [("max_tokens", 3), ("tool_use", 2), ("end_turn", 1)] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (hooks, entered, release) = hold("client_policy_stop");
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
                json!({"mode":trigger,"count":bound+1,"duplicates":true}),
            )
            .await?;
            controller.drive(&id).await?;
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            let events = notices(store, &run.assignment_id).await?;
            release.add_permits(1);
            assert_eq!(events.len(), bound + 1, "{trigger}: {events:?}");
            assert_eq!(
                events
                    .iter()
                    .filter(|v| v["label"] == "client-initiated repair occurred")
                    .count(),
                bound
            );
            assert_eq!(
                events.last().unwrap()["label"],
                "client request policy anomaly"
            );
            assert_eq!(events.last().unwrap()["qualification_exceeded"], true);
            assert_ne!(run.assignment_state, AssignmentState::Succeeded);
            assert_anomalous_frozen_report(&controller, &id, &events).await?;
            one_prompt(&temp, &controller, &run.operation_id).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C72 {trigger}: {bound} distinct repairs, duplicate prompt/start observations deduplicated, excess classified as policy anomaly"
            );
        }
        Ok(())
    })
}

#[test]
fn unexpected_generations_prompts_refusal_and_resends_are_policy_anomalies() -> anyhow::Result<()> {
    run_test_with_large_stack("c72-anomalies", || async {
        for mode in [
            "unexpected",
            "refusal",
            "resend",
            "unknown_prompt",
            "restarted_message",
            "contradictory",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed(
                &controller,
                &temp,
                json!({"mode":mode,"prompts":mode=="unknown_prompt" || mode=="refusal" || mode=="contradictory"}),
            )
            .await?;
            let run = settled(&controller, &id).await?;
            let events = notices(controller.store().await?, &run.assignment_id).await?;
            assert_eq!(events.len(), 1, "{mode}");
            assert_eq!(
                events[0]["label"], "client request policy anomaly",
                "{mode}"
            );
            assert_ne!(run.assignment_state, AssignmentState::Succeeded);
            assert_anomalous_frozen_report(&controller, &id, &events).await?;
            one_prompt(&temp, &controller, &run.operation_id).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!("C72 {mode}: policy anomaly retained, completion refused, no replay");
        }
        Ok(())
    })
}

#[test]
fn terminal_and_internal_stop_follow_store_order_without_changing_frozen_reports()
-> anyhow::Result<()> {
    run_test_with_large_stack("c72-terminal-race", || async {
        for terminal_first in [true, false] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (hooks, entered, release) = hold(if terminal_first {
                "client_policy_stop"
            } else {
                "client_terminal_received"
            });
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
                json!({"mode":"max_tokens","complete":true,"partial_start":true}),
            )
            .await?;
            controller.drive(&id).await?;
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let run = settled(&controller, &id).await?;
            assert_eq!(
                run.assignment_state == AssignmentState::Succeeded,
                terminal_first
            );
            let store = controller.store().await?;
            assert_eq!(notices(store, &run.assignment_id).await?.len(), 1);
            if !terminal_first {
                controller.abort_audit(&id).await?;
            }
            let frozen = store.freeze_report(&id).await?;
            assert_eq!(frozen.reviewers[0].completed.is_some(), terminal_first);
            let before = serde_json::to_vec(&frozen)?;
            store.publish_report(&id, 0).await?;
            let report = store.load_run(&id).await?.report.unwrap();
            let markdown = std::fs::read_to_string(controller.root.join(&report.relative_path))?;
            assert!(markdown.contains("client-initiated repair occurred"));
            release.add_permits(1);
            one_prompt(&temp, &controller, &run.operation_id).await?;
            assert_eq!(
                serde_json::to_vec(&store.freeze_report(&id).await?)?,
                before
            );
            assert_eq!(store.load_run(&id).await?.report, Some(report));
            let reopened = AuditStore::open(controller.root.clone()).await?;
            assert_eq!(notices(&reopened, &run.assignment_id).await?.len(), 1);
            assert_eq!(
                reopened
                    .client_terminal_result(&run.operation_id)
                    .await?
                    .is_some(),
                terminal_first
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C72 terminal_first={terminal_first}: writer order honoured, disclosure frozen, reopen and later cleanup preserve report identity"
            );
        }
        Ok(())
    })
}

#[test]
fn repair_disclosure_survives_held_flush_and_operator_controls_including_supersession()
-> anyhow::Result<()> {
    run_test_with_large_stack("c72-controls", || async {
        for action in ["cancel", "ignore", "abort", "retry"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (hooks, entered, release) = hold("client_policy_stop");
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
                "C72 {action}: exact-attempt disclosure retained despite held flush, internal stop cannot retarget successor or rewrite frozen history"
            );
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
fn healthy_native_peer_completes_while_subscription_is_interrupted() -> anyhow::Result<()> {
    run_test_with_large_stack("c72-native-peer", || async {
        let temp = tempfile::tempdir()?;
        let arrived = Arc::new(Notify::new());
        let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let body = Arc::new(tokio::sync::Mutex::new(String::new()));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (signal, gate, response_body, requests) = (
            arrived.clone(),
            response_gate.clone(),
            body.clone(),
            count.clone(),
        );
        let endpoint = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move || {
                let (signal, gate, body, requests) = (
                    signal.clone(),
                    gate.clone(),
                    response_body.clone(),
                    requests.clone(),
                );
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    signal.notify_one();
                    gate.acquire().await.unwrap().forget();
                    (
                        [("content-type", "text/event-stream")],
                        body.lock().await.clone(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let endpoint_task = tokio::spawn(async move { axum::serve(listener, endpoint).await });
        let (controller, threads) = real_fixture(&temp, &url).await;
        let response = api(&controller, "audit/start", json!({"requestKey":"repair-peer",
            "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},"brief":"Static correctness audit",
            "reviewers":[{"provider":controller.config.model_provider_id,"model":"audit-test"},
                         {"provider":controller.config.model_provider_id,"model":"audit-test"}]})).await.expect("roster");
        let id = response["run"]["runId"].as_str().unwrap();
        retain_external(&controller, &temp, id).await?;
        std::fs::write(
            temp.path().join("fake-client"),
            include_str!("../../../../external/fake_repairs.py"),
        )?;
        std::fs::write(
            temp.path().join("stream-scenario.json"),
            json!({"mode":"max_tokens","subsequent":false,"gate_start":true}).to_string(),
        )?;
        let store = controller.store().await?;
        let attempts = store.attempts(id).await?;
        let (external, native) = (&attempts[0], &attempts[1]);
        let raw = json!({"schema_version":1,"assignment_id":native.assignment_id,"snapshot_id":native.snapshot_id,
                         "summary":"Native peer completed independently","findings":[]}).to_string();
        *body.lock().await = sse(vec![
            ev_response_created("native-repair-peer"),
            ev_assistant_message("native-final", &raw),
            ev_completed("native-repair-peer"),
        ]);
        controller.drive(id).await?;
        tokio::time::timeout(Duration::from_secs(10), arrived.notified()).await?;
        std::fs::write(temp.path().join("before-public.release"), "go")?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                controller.drive(id).await?;
                let e = store.run_for_operation(&external.operation_id).await?;
                if matches!(
                    e.assignment_state,
                    AssignmentState::Failed | AssignmentState::Unknown
                ) {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_ne!(
            store
                .run_for_operation(&native.operation_id)
                .await?
                .assignment_state,
            AssignmentState::Succeeded
        );
        assert_eq!(notices(store, &external.assignment_id).await?.len(), 1);
        response_gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                controller.drive(id).await?;
                let n = store.run_for_operation(&native.operation_id).await?;
                let e = store.run_for_operation(&external.operation_id).await?;
                if n.assignment_state == AssignmentState::Succeeded
                    && matches!(
                        e.assignment_state,
                        AssignmentState::Failed | AssignmentState::Unknown
                    )
                {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(notices(store, &external.assignment_id).await?.len(), 1);
        assert!(notices(store, &native.assignment_id).await?.is_empty());
        controller.abort_audit(id).await?;
        let report = store.freeze_report(id).await?;
        assert!(report.reviewers[0].completed.is_none());
        assert!(report.reviewers[1].completed.is_some());
        one_prompt(&temp, &controller, &external.operation_id).await?;
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        endpoint_task.abort();
        let _ = endpoint_task.await;
        println!(
            "C72 mixed roster: native request held in flight until subscription interruption, then completed; exactly one native request and one client prompt"
        );
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

#[test]
fn separate_corrective_chains_do_not_turn_observed_bounds_into_attempt_ceilings()
-> anyhow::Result<()> {
    run_test_with_large_stack("c72-separate-chains", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (hooks, entered, release) = hold("client_policy_stop");
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
            json!({"mode":"max_tokens","count":4,"reset_after":2}),
        )
        .await?;
        controller.drive(&id).await?;
        tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
        let run = settled(&controller, &id).await?;
        release.add_permits(1);
        let events = notices(controller.store().await?, &run.assignment_id).await?;
        assert_eq!(events.len(), 4);
        assert!(
            events
                .iter()
                .all(|e| e["label"] == "client-initiated repair occurred")
        );
        assert_ne!(run.assignment_state, AssignmentState::Succeeded);
        controller.abort_audit(&id).await?;
        let frozen = serde_json::to_value(controller.store().await?.freeze_report(&id).await?)?;
        assert_eq!(
            frozen["reviewers"][0]["provider_facts"]["subscription"]["repair_occurrences"],
            json!({"status":"known","value":4})
        );
        one_prompt(&temp, &controller, &run.operation_id).await?;
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C72 separate occurrences: two cut-off chains of two, separated by a legitimate tool round; four repair observations, no invented whole-attempt ceiling"
        );
        Ok(())
    })
}
