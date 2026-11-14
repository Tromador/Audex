//! Production native reviewer loop with synthetic transport faults and context pressure.
use super::*;
use crate::audex::dispatch::DispatchTestHooks;
use std::sync::atomic::AtomicUsize;

mod recovered_tail;

/// Exercise the same user configuration loader as the daemon, before reviewer isolation.
pub(super) async fn context_config(
    temp: &tempfile::TempDir,
    background: bool,
    limit: i64,
) -> anyhow::Result<Config> {
    std::fs::write(
        temp.path().join("config.toml"),
        format!(
            "model_context_window = 100000\nmodel_auto_compact_token_limit = {limit}\n\n[features]\nasync_compaction_root_only = false\n\n[features.async_compaction]\nenabled = {background}\nmodel_async_compact_threshold_percent = 10\n"
        ),
    )?;
    Ok(codex_core::config::ConfigBuilder::default()
        .codex_home(temp.path().to_path_buf())
        .harness_overrides(codex_core::config::ConfigOverrides {
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?)
}

#[test]
fn native_http_and_stream_retries_complete_the_same_execution() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-native-retries", || async {
        for stream_failure in [false, true] {
            let server = responses::start_mock_server().await;
            let first = if stream_failure {
                // No terminal receipt: the normal stream recovery path must reconnect.
                responses::sse_response(sse(vec![ev_response_created("interrupted")]))
            } else {
                ResponseTemplate::new(503).set_body_json(json!({"error":{"message":"transient"}}))
            };
            let mock = responses::mount_response_sequence(
                &server,
                vec![
                    first,
                    responses::sse_response(sse(vec![
                        ev_response_created("recovered"),
                        ev_assistant_message("opinion", "Recovered by the native client."),
                        ev_completed("recovered"),
                    ])),
                ],
            )
            .await;
            let temp = tempfile::tempdir()?;
            let (controller, threads) = fixture_with_listener(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                admission::native_reader(),
            )
            .await;
            let id = start(&controller, "m5-transient").await;
            let store = controller.store().await?;
            let original = store.load_run_metadata(&id).await?;
            super::super::limits::retain_legacy_limits(
                store,
                &original,
                json!({"maxAttempts":1,"budgetUsd":"0.0001"}),
            )
            .await?;
            controller.tick().await?;
            let run = controller.store().await?.load_run_metadata(&id).await?;
            let (thread, _, _) = controller
                .store()
                .await?
                .execution(&run.operation_id)
                .await?
                .unwrap();
            recovery::wait_completed(&threads, ThreadId::from_string(&thread)?).await?;
            let completed = finished(&controller, &id).await;
            assert_eq!(completed["phase"], "completed");
            assert_eq!(mock.requests().len(), 2);
            assert_eq!(controller.store().await?.attempts(&id).await?.len(), 1);
            let sends: i64 = sqlx::query_scalar(
                "SELECT SUM(send_count) FROM audit_request_admissions WHERE assignment_id=?",
            )
            .bind(&run.assignment_id)
            .fetch_one(&controller.store().await?.pool_for_tests())
            .await?;
            assert_eq!(sends, 2, "observable send checks, not billing claims");
            assert_eq!(
                controller.store().await?.list_run_ids("", 100).await?.len(),
                1
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn native_local_remote_and_background_compaction_preserve_source_and_result() -> anyhow::Result<()>
{
    run_test_with_large_stack("m5-context", || async {
        for mode in ["legacy-local", "local", "remote", "background"] {
            let remote = matches!(mode, "remote" | "background");
            let background = mode == "background";
            let server = responses::start_mock_server().await;
            let normal = Arc::new(AtomicUsize::new(0));
            let compactions = Arc::new(AtomicUsize::new(0));
            let (calls, compacts) = (normal.clone(), compactions.clone());
            Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request:&wiremock::Request| {
                let body:Value=request.body_json().unwrap();
                let compact=body["input"].as_array().unwrap().iter().any(|item| item["type"]=="compaction_trigger")
                    || (!remote && body["tools"].as_array().is_none_or(Vec::is_empty));
                let mut events=vec![ev_response_created(if compact {"compact"}else{"sampling"})];
                let delay;
                if compact {
                    compacts.fetch_add(1,Ordering::SeqCst);
                    events.push(if remote { json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"M5_CHECKPOINT"}}) }
                        else { ev_assistant_message("summary","Sealed example.rs is being checked. Continue with the same assigned source and identity.") });
                    events.push(responses::ev_completed_with_tokens("compact",100));
                    delay=Duration::ZERO;
                } else {
                    let index=calls.fetch_add(1,Ordering::SeqCst);
                    if index<2 {
                        events.push(ev_assistant_message(&format!("progress-{index}"),"BEFORE_COMPACTION_PARTIAL"));
                        events.push(ev_function_call_with_namespace(&format!("read-{index}"),"audex_source","read",r#"{"path":"example.rs","offset":0,"max_bytes":32768}"#));
                        events.push(responses::ev_completed_with_tokens("sampling",if background && index==0 {15_000}else{25_000}));
                    } else {
                        events.push(ev_assistant_message("opinion","Context-intensive review complete."));
                        events.push(responses::ev_completed_with_tokens("sampling",100));
                    }
                    delay=if background && index==1 {Duration::from_millis(300)}else{Duration::ZERO};
                }
                responses::sse_response(sse(events)).insert_header("x-codex-turn-state","m5-context-state").set_delay(delay)
            }).mount(&server).await;
            let temp = tempfile::tempdir()?;
            let hooks = DispatchTestHooks {
                history_mode: (mode == "legacy-local")
                    .then_some(codex_protocol::protocol::ThreadHistoryMode::Legacy),
                config: Some(Arc::new(move |config| {
                    config
                        .model_provider
                        .capabilities
                        .get_or_insert_with(Default::default)
                        .remote_compaction = Some(if remote {
                        codex_model_provider_info::RemoteCompactionSupport::V2
                    } else {
                        codex_model_provider_info::RemoteCompactionSupport::Unsupported
                    });
                })),
                ..Default::default()
            };
            let config =
                context_config(&temp, background, if background { 50_000 } else { 20_000 }).await?;
            let (controller, threads) = fixture_from_config(
                config,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                admission::native_reader(),
                hooks,
                false,
            )
            .await;
            let source = format!(
                "// SEALED_M5_SOURCE\n{}",
                "pub const X: u32 = 1;\n".repeat(2000)
            );
            let started=api(&controller,"audit/start",json!({"requestKey":mode,"source":{"type":"snippet","name":"example.rs","content":source},"brief":"Context continuity fixture","provider":"openai","model":"audit-test"})).await.expect("commission");
            let id = started["run"]["runId"].as_str().unwrap();
            controller.tick().await?;
            let run = controller.store().await?.load_run_metadata(id).await?;
            let (thread, _, _) = controller
                .store()
                .await?
                .execution(&run.operation_id)
                .await?
                .unwrap();
            recovery::wait_completed(&threads, ThreadId::from_string(&thread)?).await?;
            // Lose the collection acknowledgement after a compacted turn has
            // completed. Recovery must use the original durable history.
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            let store = controller.store().await?;
            let current = store.run_for_operation(&run.operation_id).await?;
            store
                .finish_operation(
                    &run.operation_id,
                    current.attempt_revision,
                    AssignmentState::Unknown,
                    "qualification_collection_interrupted",
                )
                .await?;
            let completed = finished(&controller, id).await;
            assert_eq!(completed["phase"], "completed", "{mode}: {completed}");
            assert!(
                compactions.load(Ordering::SeqCst) > 0,
                "{mode}: no compaction request"
            );
            let requests = server.received_requests().await.unwrap();
            assert!(
                requests
                    .iter()
                    .any(|request| String::from_utf8_lossy(&request.body)
                        .contains("SEALED_M5_SOURCE")),
                "source tool bytes must reach continuation"
            );
            let store = controller.store().await?;
            assert_eq!(store.attempts(id).await?.len(), 1);
            assert!(
                store
                    .recovered_output(&run.operation_id)
                    .await?
                    .unwrap()
                    .contains("BEFORE_COMPACTION_PARTIAL"),
                "original messages survive model-context compaction"
            );
            let records = store.generation_records(&run.assignment_id).await?;
            assert_eq!(
                records.len(),
                requests.len(),
                "{mode}: every compaction/sampling request stays attributed"
            );
            assert!(
                store
                    .model_evidence(&run.assignment_id)
                    .await?
                    .iter()
                    .all(|e| e.state == "verified")
            );
            let before = store.raw_result(&store.load_run(id).await?)?.unwrap();
            let reopened = AuditStore::open(controller.root.clone()).await?;
            assert_eq!(
                reopened.raw_result(&reopened.load_run(id).await?)?.unwrap(),
                before
            );
            assert_eq!(store.list_run_ids("", 100).await?.len(), 1);
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn healthy_native_observation_keeps_interim_text_in_activity_without_recovery_projection()
-> anyhow::Result<()> {
    run_test_with_large_stack("m5-healthy-observation", || async {
        for legacy in [false, true] {
            let server = responses::start_mock_server().await;
            responses::mount_response_sequence(
                &server,
                vec![
                    responses::sse_response(sse(vec![
                        ev_response_created("first"),
                        ev_assistant_message("progress", "HEALTHY_INTERIM_TEXT"),
                        ev_function_call_with_namespace(
                            "read",
                            "audex_source",
                            "read",
                            r#"{"path":"example.rs","offset":0,"max_bytes":32768}"#,
                        ),
                        ev_completed("first"),
                    ])),
                    responses::sse_response(sse(vec![
                        ev_response_created("last"),
                        ev_assistant_message("opinion", "Completed review."),
                        ev_completed("last"),
                    ]))
                    .set_delay(Duration::from_secs(5)),
                ],
            )
            .await;
            let temp = tempfile::tempdir()?;
            let (controller, threads) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                admission::native_reader(),
                DispatchTestHooks {
                    history_mode: legacy
                        .then_some(codex_protocol::protocol::ThreadHistoryMode::Legacy),
                    ..Default::default()
                },
            )
            .await;
            let id = start(&controller, "healthy-observation").await;
            controller.tick().await?;
            let store = controller.store().await?;
            let run = store.load_run_metadata(&id).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if store
                        .activity(&run.assignment_id, 0, 64)
                        .await
                        .unwrap()
                        .iter()
                        .any(|entry| entry.payload.contains("HEALTHY_INTERIM_TEXT"))
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            for _ in 0..3 {
                controller.drive_operation(&run.operation_id).await?;
            }
            assert_eq!(
                store
                    .run_for_operation(&run.operation_id)
                    .await?
                    .assignment_state,
                AssignmentState::Running
            );
            let projection: Option<String> = sqlx::query_scalar(
                "SELECT recovered_output FROM audit_execution WHERE operation_id=?",
            )
            .bind(&run.operation_id)
            .fetch_one(&store.pool_for_tests())
            .await?;
            assert!(
                projection.is_none(),
                "routine observation must not persist recovery text"
            );
            let read = controller
                .read_attempt(AuditAttemptReadParams {
                    run_id: id.clone(),
                    assignment_id: run.assignment_id.clone(),
                    after_sequence: None,
                    limit: None,
                })
                .await?;
            assert!(read.recovered_output.is_none());
            assert!(read.attempt.recovery_status.is_none());
            let (thread, _, _) = store.execution(&run.operation_id).await?.unwrap();
            recovery::wait_completed(&threads, ThreadId::from_string(&thread)?).await?;
            assert_eq!(finished(&controller, &id).await["phase"], "completed");
            let read = controller
                .read_attempt(AuditAttemptReadParams {
                    run_id: id,
                    assignment_id: run.assignment_id,
                    after_sequence: None,
                    limit: None,
                })
                .await?;
            assert!(read.attempt.completed && read.recovered_output.is_none());
            assert!(
                read.entries
                    .iter()
                    .any(|entry| entry.payload.contains("HEALTHY_INTERIM_TEXT"))
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}
