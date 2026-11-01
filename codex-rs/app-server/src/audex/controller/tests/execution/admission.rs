//! Real native model/tool loops, durable controls, and actual HTTP/WS providers.
use super::*;
use crate::audex::capture::CaptureHandle;
use crate::audex::dispatch::DispatchTestHooks;
use codex_protocol::protocol::{AgentStatus, EventMsg};
use std::sync::atomic::AtomicUsize;
use tokio::sync::Semaphore;

async fn wait(signal: &Notify) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), signal.notified()).await?;
    Ok(())
}
async fn settled(threads: &ThreadManager, id: &str) -> anyhow::Result<()> {
    let thread = threads.get_thread(ThreadId::from_string(id)?).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                thread.agent_status().await,
                AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Interrupted
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

fn native_reader() -> crate::audex::dispatch::Listen {
    Arc::new(|_, thread| {
        Box::pin(async move {
            tokio::spawn(async move {
                while let Ok(event) = thread.next_event().await {
                    if let Some(capture) = thread.thread_extension_data().get::<CaptureHandle>() {
                        capture.observe(&event).await;
                    }
                    if matches!(event.msg, EventMsg::ShutdownComplete) {
                        break;
                    }
                }
            });
            Ok(())
        })
    })
}

enum Provider {
    Http(wiremock::MockServer),
    Websocket(responses::WebSocketTestServer),
}
impl Provider {
    async fn new(websocket: bool, events: Vec<Value>) -> Self {
        if websocket {
            Self::Websocket(
                responses::start_websocket_server_with_headers(vec![
                    responses::WebSocketConnectionConfig {
                        requests: vec![
                            events,
                            vec![
                                ev_response_created("second-generation"),
                                ev_assistant_message("final", "invalid final JSON"),
                                ev_completed("second-generation"),
                            ],
                        ],
                        response_headers: Vec::new(),
                        accept_delay: None,
                        close_after_requests: false,
                    },
                ])
                .await,
            )
        } else {
            let server = responses::start_mock_server().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse(events)),
                )
                .mount(&server)
                .await;
            Self::Http(server)
        }
    }
    fn uri(&self) -> String {
        match self {
            Self::Http(server) => server.uri(),
            Self::Websocket(server) => server.uri().to_owned(),
        }
    }
    async fn generations(&self) -> usize {
        match self {
            Self::Http(server) => server
                .received_requests()
                .await
                .expect("requests")
                .iter()
                .filter(|r| r.method.as_str() == "POST")
                .count(),
            Self::Websocket(server) => server
                .connections()
                .iter()
                .flatten()
                .filter(|r| r.body_json()["type"] == "response.create")
                .count(),
        }
    }
    async fn shutdown(self) {
        if let Self::Websocket(server) = self {
            server.shutdown().await;
        }
    }
}

async fn stopped(
    controller: &AuditController,
    run: &AuditRunRecord,
    key: &str,
) -> anyhow::Result<Value> {
    let response=tokio::time::timeout(Duration::from_secs(2),api(controller,"audit/attempt/control",json!({
        "runId":run.run_id,"assignmentId":run.assignment_id,"action":"cancel","actionKey":key,"reason":"fixture stop",
        "authoriseRetry":false,"acknowledgePossibleOverlap":false,
    }))).await?.expect("durable cancel");
    assert!(
        controller
            .store()
            .await?
            .attempt_by_assignment(&run.run_id, &run.assignment_id)
            .await?
            .capacity_released
    );
    Ok(response)
}

#[test]
fn stop_wins_native_tool_continuation_on_http_and_websocket_even_when_interrupt_is_blocked()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-native-stop-admission", || async {
        for websocket in [false, true] {
            let provider = Provider::new(
                websocket,
                vec![
                    ev_response_created("first-generation"),
                    ev_assistant_message("partial", "{unfinished public review"),
                    ev_function_call_with_namespace(
                        "read",
                        "audex_source",
                        "read",
                        r#"{"path":"example.rs","offset":0,"max_bytes":64}"#,
                    ),
                    ev_completed("first-generation"),
                ],
            )
            .await;
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let (signal, gate, count) = (entered.clone(), release.clone(), calls.clone());
            let hooks = DispatchTestHooks {
                // A tiny compaction threshold exercises the installed native disable flag.
                config: Some(Arc::new(|config| {
                    config.model_auto_compact_token_limit = Some(1)
                })),
                checkpoint: Some(Arc::new(move |at| {
                    let (signal, gate, count) = (signal.clone(), gate.clone(), count.clone());
                    Box::pin(async move {
                        if at == "before_generation_admission"
                            && count.fetch_add(1, Ordering::SeqCst) == 1
                        {
                            signal.notify_one();
                            gate.notified().await;
                        }
                    })
                })),
                ..Default::default()
            };
            let temp = tempfile::tempdir()?;
            let (controller, threads) = fixture_with_transport(
                &temp,
                &provider.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                native_reader(),
                hooks,
                websocket,
            )
            .await;
            let id = start(&controller, "tool-continuation-stop").await;
            controller.tick().await?;
            wait(&entered).await?;
            assert_eq!(provider.generations().await, 1);
            let store = controller.store().await?;
            let run = store.load_run_metadata(&id).await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if store
                        .activity(&run.assignment_id, 0, 64)
                        .await?
                        .iter()
                        .any(|entry| entry.payload.contains("unfinished public review"))
                    {
                        break anyhow::Ok(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
            let capture = controller
                .dispatch
                .capture(&run.operation_id)
                .expect("native collector");
            let interrupt_entered = Arc::new(Notify::new());
            let interrupt_release = Arc::new(Semaphore::new(0));
            capture.block_interrupt(interrupt_entered.clone(), interrupt_release.clone());
            let decision = stopped(&controller, &run, "stop-tool-continuation").await?;
            wait(&interrupt_entered).await?;
            let prefix = store.activity(&run.assignment_id, 0, 64).await?;
            assert!(
                prefix
                    .iter()
                    .any(|entry| entry.payload.contains("unfinished public review"))
            );
            assert!(prefix.iter().any(|entry| entry.kind == "tool_result"));
            let state = store.capture_state(&run.assignment_id).await?;
            assert_eq!(decision["cutoff"].as_i64(), state.cutoff);
            assert_eq!(store.generation_records(&run.assignment_id).await?.len(), 1);
            let (thread, _, _) = store
                .execution(&run.operation_id)
                .await?
                .expect("execution");
            release.notify_one();
            settled(&threads, &thread).await?;
            assert_eq!(
                provider.generations().await,
                1,
                "no continuation reached the actual provider"
            );
            assert_eq!(
                store.generation_records(&run.assignment_id).await?.len(),
                1,
                "no post-stop generation obtained authority"
            );
            assert_eq!(store.activity(&run.assignment_id, 0, 64).await?, prefix);
            assert_eq!(store.capture_state(&run.assignment_id).await?, state);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            interrupt_release.close();
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            provider.shutdown().await;
        }
        Ok(())
    })
}

#[test]
fn preadmitted_native_requests_stay_accounted_when_stop_suppresses_the_wire_send()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-native-admission-first", || async {
        for websocket in [false, true] {
            let provider = Provider::new(websocket, vec![ev_completed("must-not-arrive")]).await;
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let (signal, gate) = (entered.clone(), release.clone());
            let hooks = DispatchTestHooks {
                checkpoint: Some(Arc::new(move |at| {
                    let (signal, gate) = (signal.clone(), gate.clone());
                    Box::pin(async move {
                        if at == "generation_admitted" {
                            signal.notify_one();
                            gate.notified().await;
                        }
                    })
                })),
                ..Default::default()
            };
            let temp = tempfile::tempdir()?;
            let (controller, threads) = fixture_with_transport(
                &temp,
                &provider.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                native_reader(),
                hooks,
                websocket,
            )
            .await;
            let id = start(&controller, "admission-first").await;
            controller.tick().await?;
            wait(&entered).await?;
            let store = controller.store().await?;
            let run = store.load_run_metadata(&id).await?;
            let before = store.generation_records(&run.assignment_id).await?;
            assert_eq!(before.len(), 1);
            assert!(before[0].send_started_at_ms.is_none() && before[0].confirmed_at_ms.is_none());
            assert_eq!(provider.generations().await, 0);
            let capture = controller
                .dispatch
                .capture(&run.operation_id)
                .expect("collector");
            let interrupt_entered = Arc::new(Notify::new());
            let interrupt_release = Arc::new(Semaphore::new(0));
            capture.block_interrupt(interrupt_entered.clone(), interrupt_release.clone());
            stopped(&controller, &run, "stop-preadmitted").await?;
            wait(&interrupt_entered).await?;
            release.notify_one();
            let (thread, _, _) = store
                .execution(&run.operation_id)
                .await?
                .expect("execution");
            settled(&threads, &thread).await?;
            assert_eq!(provider.generations().await, 0);
            assert_eq!(
                store.generation_records(&run.assignment_id).await?,
                before,
                "admission is never reclassified as unsubmitted"
            );
            let reopened = AuditStore::open(controller.root.clone()).await?;
            assert_eq!(
                reopened.generation_records(&run.assignment_id).await?,
                before
            );
            interrupt_release.close();
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            provider.shutdown().await;
        }
        Ok(())
    })
}

#[test]
fn native_http_errors_and_context_overflow_do_not_replay_or_compact() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-native-no-resend", || async {
        for status in [401, 503, 200] {
            let server = responses::start_mock_server().await;
            let response = if status == 200 {
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(sse(vec![
                    ev_response_created("failed-request"),ev_assistant_message("partial","{invalid received partial"),
                    json!({"type":"response.failed","response":{"id":"failed-request","error":{"code":"context_length_exceeded","message":"fixture context limit"}}}),
                ]))
            } else {
                ResponseTemplate::new(status)
                    .set_body_json(json!({"error":{"message":"fixture failure"}}))
            };
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(response)
                .mount(&server)
                .await;
            let temp = tempfile::tempdir()?;
            let hooks = DispatchTestHooks {
                config: Some(Arc::new(|config| {
                    config.model_auto_compact_token_limit = Some(1)
                })),
                ..Default::default()
            };
            let (controller, threads) = fixture_with_transport(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                native_reader(),
                hooks,
                false,
            )
            .await;
            let id = start(&controller, "no-error-replay").await;
            controller.tick().await?;
            let store = controller.store().await?;
            let run = store.load_run_metadata(&id).await?;
            let (thread, _, _) = store
                .execution(&run.operation_id)
                .await?
                .expect("execution");
            settled(&threads, &thread).await?;
            controller.tick().await?;
            assert_eq!(server.received_requests().await.expect("requests").len(), 1);
            let records = store.generation_records(&run.assignment_id).await?;
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].confirmed_at_ms.is_some(), status == 200);
            if status == 200 {
                assert!(
                    store
                        .activity(&run.assignment_id, 0, 64)
                        .await?
                        .iter()
                        .any(|entry| entry.payload.contains("invalid received partial"))
                );
            }
            assert_ne!(
                store.load_run_metadata(&id).await?.assignment_state,
                AssignmentState::Succeeded
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn native_websocket_errors_cannot_resend_or_fall_back_to_another_generation() -> anyhow::Result<()>
{
    run_test_with_large_stack("audex-native-ws-no-resend", || async {
        for (status, code) in [
            (401, "unauthorized"),
            (503, "server_error"),
            (400, "previous_response_not_found"),
        ] {
            let provider=Provider::new(true,vec![json!({"type":"error","status":status,"error":{"code":code,"message":"fixture failure"}})]).await;
            let temp = tempfile::tempdir()?;
            let (controller, threads) = fixture_with_transport(
                &temp,
                &provider.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                native_reader(),
                DispatchTestHooks::default(),
                true,
            )
            .await;
            let id = start(&controller, "websocket-no-replay").await;
            controller.tick().await?;
            let store = controller.store().await?;
            let run = store.load_run_metadata(&id).await?;
            let (thread, _, _) = store
                .execution(&run.operation_id)
                .await?
                .expect("execution");
            settled(&threads, &thread).await?;
            assert_eq!(provider.generations().await, 1);
            let records = store.generation_records(&run.assignment_id).await?;
            assert_eq!(
                records.len(),
                1,
                "neither WS recovery nor HTTP fallback can obtain a second permit"
            );
            assert!(
                records[0].send_started_at_ms.is_some() && records[0].confirmed_at_ms.is_none()
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            provider.shutdown().await;
        }
        Ok(())
    })
}

#[test]
fn legitimate_websocket_tool_continuation_gets_a_fresh_one_use_request_without_compaction()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-native-ws-continuation", || async {
        let provider = Provider::new(
            true,
            vec![
                ev_response_created("first-generation"),
                ev_function_call_with_namespace(
                    "read",
                    "audex_source",
                    "read",
                    r#"{"path":"example.rs","offset":0,"max_bytes":64}"#,
                ),
                ev_completed("first-generation"),
            ],
        )
        .await;
        let temp = tempfile::tempdir()?;
        let hooks = DispatchTestHooks {
            config: Some(Arc::new(|config| {
                config.model_auto_compact_token_limit = Some(1)
            })),
            ..Default::default()
        };
        let (controller, threads) = fixture_with_transport(
            &temp,
            &provider.uri(),
            CodexAuth::from_api_key("dummy"),
            false,
            native_reader(),
            hooks,
            true,
        )
        .await;
        let id = start(&controller, "legitimate-continuation").await;
        controller.tick().await?;
        let store = controller.store().await?;
        let run = store.load_run_metadata(&id).await?;
        let (thread, _, _) = store
            .execution(&run.operation_id)
            .await?
            .expect("execution");
        settled(&threads, &thread).await?;
        controller.tick().await?;
        assert_eq!(
            provider.generations().await,
            2,
            "only the original request and actual tool continuation"
        );
        let records = store.generation_records(&run.assignment_id).await?;
        assert_eq!(records.len(), 2);
        assert_ne!(records[0].request_id, records[1].request_id);
        assert!(
            records
                .iter()
                .all(|record| record.confirmed_at_ms.is_some())
        );
        let result = store.load_run(&id).await?;
        assert_eq!(
            result.assignment_state,
            AssignmentState::Failed,
            "an invalid terminal model response is still invalid"
        );
        assert!(result.findings.is_empty());
        assert_eq!(
            store.raw_result(&result)?,
            Some(b"invalid final JSON".to_vec())
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        provider.shutdown().await;
        Ok(())
    })
}

#[test]
fn websocket_handshake_fallback_before_admission_can_make_one_http_generation() -> anyhow::Result<()>
{
    run_test_with_large_stack("audex-native-pre-admission-fallback", || async {
        let server = responses::start_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(426))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_response_created("http-fallback"),
                        ev_assistant_message("answer", "invalid result"),
                        ev_completed("http-fallback"),
                    ])),
            )
            .mount(&server)
            .await;
        let temp = tempfile::tempdir()?;
        let (controller, threads) = fixture_with_transport(
            &temp,
            &server.uri(),
            CodexAuth::from_api_key("dummy"),
            false,
            native_reader(),
            DispatchTestHooks::default(),
            true,
        )
        .await;
        let id = start(&controller, "pre-admission-fallback").await;
        controller.tick().await?;
        let store = controller.store().await?;
        let run = store.load_run_metadata(&id).await?;
        let (thread, _, _) = store
            .execution(&run.operation_id)
            .await?
            .expect("execution");
        settled(&threads, &thread).await?;
        let records = store.generation_records(&run.assignment_id).await?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].transport, "native_http");
        assert!(records[0].confirmed_at_ms.is_some());
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method.as_str() == "POST")
                .count(),
            1
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        Ok(())
    })
}
