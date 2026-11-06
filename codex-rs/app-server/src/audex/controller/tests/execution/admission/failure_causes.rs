//! Typed native failures through admission, durable collection and public reporting.
use super::*;
use codex_protocol::protocol::CodexErrorInfo;

const PRIVATE_ERROR: &str = "PRIVATE_PROVIDER_BODY credential=fixture-secret";

fn wrapped_error(status: u16, code: &str) -> Value {
    json!({"type":"error","status":status,"error":{
        "type":code,"code":code,"message":PRIVATE_ERROR
    }})
}

fn created(model: &str) -> Value {
    let mut event = ev_response_created("failed-request");
    event["response"]["model"] = json!(model);
    event
}

async fn failure_record(
    controller: &Arc<AuditController>,
    run: &AuditRunRecord,
    case: &str,
) -> anyhow::Result<Value> {
    let store = controller.store().await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            controller.tick().await?;
            if store.load_run(&run.run_id).await?.assignment_state == AssignmentState::Failed {
                break anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let (error, retry) = recovery::issue(controller, &run.operation_id).await?;
    assert_eq!(retry.as_deref(), Some("never_automatically_retry"));
    let error = error.expect("retained provider failure");
    let diagnostic: Value = serde_json::from_str(&error)?;
    assert_eq!(diagnostic["version"], 1);
    assert_eq!(diagnostic["stage"], "reviewer_turn");
    assert_eq!(
        diagnostic.as_object().unwrap().len(),
        3,
        "payload-free failure"
    );
    let category: CodexErrorInfo = serde_json::from_value(diagnostic["cause"].clone())?;
    let attempt = api(
        controller,
        "audit/attempt/read",
        json!({
            "runId":run.run_id,"assignmentId":run.assignment_id
        }),
    )
    .await
    .expect("public attempt");
    assert_eq!(attempt["attempt"]["completed"], false);
    assert!(attempt["rawResult"].is_null());
    assert!(store.result_extraction(&run.assignment_id).await?.is_none());
    let evidence = &attempt["attempt"]["modelEvidence"];
    assert_eq!(evidence.as_array().unwrap().len(), 1);
    let attribution: Option<String> =
        sqlx::query_scalar("SELECT attribution_failure FROM audit_attempts WHERE assignment_id=?")
            .bind(&run.assignment_id)
            .fetch_one(&store.pool_for_tests())
            .await?;
    // An explicit fixture operator decision closes the failed contribution for publication.
    // It must not authorise a replacement generation.
    api(
        controller,
        "audit/attempt/control",
        json!({
            "runId":run.run_id,"assignmentId":run.assignment_id,"action":"ignore",
            "actionKey":"publish-failure","reason":"fixture failure collection",
            "authoriseRetry":false,"acknowledgePossibleOverlap":false
        }),
    )
    .await
    .expect("ignore failed contribution");
    let public = finished(controller, &run.run_id).await;
    let report = public["report"].as_str().expect("public failure report");
    assert!(
        report.contains(&error),
        "public report must retain the complete typed diagnostic"
    );
    for text in [&error, report] {
        assert!(!text.contains(PRIVATE_ERROR));
        assert!(!text.contains("fixture-secret"));
        assert!(!text.contains("local-dummy"));
    }
    for _ in 0..3 {
        controller.tick().await?;
    }
    let records = store.generation_records(&run.assignment_id).await?;
    assert_eq!(records.len(), 1, "no additional admission or fallback");
    assert!(records[0].send_started_at_ms.is_some());
    let summary = json!({
        "category":category,"identityState":evidence[0]["state"],
        "confirmed":records[0].confirmed_at_ms.is_some(),
        "observedModel":evidence[0]["observedModel"],
        "evidenceScope":evidence[0]["evidenceScope"],"attributionFailure":attribution
    });
    if let Some(directory) = std::env::var_os("AUDEX_FAILURE_EVIDENCE") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join(format!("{case}.json")),
            serde_json::to_vec_pretty(
                &json!({"summary":summary,"diagnostic":diagnostic,"attempt":attempt,"public":public}),
            )?,
        )?;
    }
    Ok(summary)
}

fn expected(
    category: CodexErrorInfo,
    state: &str,
    model: Option<&str>,
    attribution: Option<&str>,
) -> Value {
    json!({"category":category,"identityState":state,"observedModel":model,
        "confirmed":state != "pending",
        "evidenceScope":model.map(|_| "generation"),"attributionFailure":attribution})
}

pub(super) async fn websocket_errors() -> anyhow::Result<()> {
    let mut cases = Vec::new();
    for (status, code) in [
        (401, "unauthorized"),
        (503, "server_error"),
        (400, "previous_response_not_found"),
    ] {
        cases.push((
            format!("ws-{status}"),
            vec![wrapped_error(status, code)],
            expected(
                if status == 400 {
                    // The existing previous-response recovery classifier uses Stream -> Other.
                    CodexErrorInfo::Other
                } else {
                    CodexErrorInfo::HttpConnectionFailed {
                        http_status_code: Some(status),
                    }
                },
                "pending",
                None,
                None,
            ),
        ));
    }
    for attributed in [false, true] {
        for (name, error, category) in [
            (
                "usage-limit",
                wrapped_error(429, "usage_limit_reached"),
                CodexErrorInfo::UsageLimitExceeded,
            ),
            (
                "rate-limit",
                json!({"type":"response.failed","response":{"id":"failed-request",
                "error":{"code":"rate_limit_exceeded","message":PRIVATE_ERROR}}}),
                CodexErrorInfo::RateLimitExceeded,
            ),
        ] {
            let mut events = Vec::new();
            if attributed {
                events.push(created("audit-test"));
            }
            events.push(if name == "rate-limit" && !attributed {
                wrapped_error(503, "slow_down")
            } else {
                error
            });
            cases.push((
                format!("ws-{name}-{attributed}"),
                events,
                expected(
                    category,
                    if attributed { "observed" } else { "pending" },
                    attributed.then_some("audit-test"),
                    None,
                ),
            ));
        }
    }
    for (name, model, state, attribution) in [
        ("missing", None, "missing", "served_model_missing"),
        (
            "mismatch",
            Some("wrong-model"),
            "mismatch",
            "served_model_mismatch",
        ),
    ] {
        let mut completion = ev_completed("failed-request");
        if let Some(model) = model {
            completion["response"]["model"] = json!(model);
        }
        cases.push((
            format!("ws-completed-{name}"),
            vec![ev_response_created("failed-request"), completion],
            expected(CodexErrorInfo::Other, state, model, Some(attribution)),
        ));
    }
    let mut actual = Vec::new();
    let mut wanted = Vec::new();
    for (name, events, expected) in cases {
        // No synthetic identity headers: early failures have no model evidence.
        let provider = Provider::Websocket(
            responses::start_websocket_server_with_headers(vec![
                responses::WebSocketConnectionConfig {
                    requests: vec![
                        events,
                        vec![created("audit-test"), ev_completed("forbidden-replay")],
                    ],
                    response_headers: vec![],
                    accept_delay: None,
                    close_after_requests: false,
                },
            ])
            .await,
        );
        let temp = tempfile::tempdir()?;
        let (controller, threads) = fixture_with_transport(
            &temp,
            &provider.uri(),
            CodexAuth::from_api_key("local-dummy"),
            false,
            native_reader(),
            DispatchTestHooks::default(),
            true,
        )
        .await;
        let id = start(&controller, &name).await;
        controller.tick().await?;
        let run = controller.store().await?.load_run_metadata(&id).await?;
        let (thread, _, _) = controller
            .store()
            .await?
            .execution(&run.operation_id)
            .await?
            .expect("execution");
        settled(&threads, &thread).await?;
        actual.push((
            name.clone(),
            failure_record(&controller, &run, &name).await?,
        ));
        wanted.push((name, expected));
        assert_eq!(provider.generations().await, 1, "no resend");
        if let Provider::Websocket(server) = &provider {
            assert_eq!(server.handshakes().len(), 1, "no reconnect or fallback");
        }
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        provider.shutdown().await;
    }
    assert_eq!(actual, wanted);
    Ok(())
}

#[test]
fn native_http_failure_categories_survive_public_reporting() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-http-failure-causes", || async {
        let mut actual = Vec::new();
        let mut wanted = Vec::new();
        for (status, code, category) in [
            (
                401,
                "unauthorized",
                CodexErrorInfo::HttpConnectionFailed {
                    http_status_code: Some(401),
                },
            ),
            (
                503,
                "server_error",
                CodexErrorInfo::HttpConnectionFailed {
                    http_status_code: Some(503),
                },
            ),
            (
                429,
                "usage_limit_reached",
                CodexErrorInfo::UsageLimitExceeded,
            ),
        ] {
            let server = responses::start_mock_server().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(wrapped_error(status, code)),
                )
                .mount(&server)
                .await;
            let provider = Provider::Http(server);
            let temp = tempfile::tempdir()?;
            let (controller, threads) = fixture_with_transport(
                &temp,
                &provider.uri(),
                CodexAuth::from_api_key("local-dummy"),
                false,
                native_reader(),
                DispatchTestHooks::default(),
                false,
            )
            .await;
            let name = format!("http-{status}");
            let id = start(&controller, &name).await;
            controller.tick().await?;
            let store = controller.store().await?;
            let run = store.load_run_metadata(&id).await?;
            let (thread, _, _) = store
                .execution(&run.operation_id)
                .await?
                .expect("execution");
            settled(&threads, &thread).await?;
            actual.push((
                name.clone(),
                failure_record(&controller, &run, &name).await?,
            ));
            wanted.push((name, expected(category, "pending", None, None)));
            assert_eq!(provider.generations().await, 1, "no resend");
            if let Provider::Http(server) = &provider {
                assert_eq!(
                    server.received_requests().await.expect("requests").len(),
                    1,
                    "no fallback"
                );
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        assert_eq!(actual, wanted);
        Ok(())
    })
}
