//! Public Responses lifecycle identity through the real native loop and store.
use super::*;

fn lifecycle(kind: &str, id: &str, model: Option<&str>) -> Value {
    let mut event = if kind == "response.created" {
        ev_response_created(id)
    } else {
        ev_completed(id)
    };
    event["type"] = json!(kind);
    event["response"]["object"] = json!("response");
    if let Some(model) = model {
        event["response"]["model"] = json!(model);
    }
    event
}

fn tool_generation(id: &str) -> Vec<Value> {
    vec![
        lifecycle("response.created", id, Some("audit-test")),
        ev_function_call_with_namespace(
            "read",
            "audex_source",
            "read",
            r#"{"path":"example.rs","offset":0,"max_bytes":64}"#,
        ),
        lifecycle("response.completed", id, Some("audit-test")),
    ]
}

fn final_generation(id: &str, model: Option<&str>) -> Vec<Value> {
    vec![
        lifecycle("response.created", id, model),
        ev_assistant_message("final", "A completed fixture opinion."),
        lifecycle("response.completed", id, model),
    ]
}

async fn check(
    requests: Vec<Vec<Value>>,
    headers: Vec<(String, String)>,
    expected_states: &[&str],
    expected_success: bool,
) -> anyhow::Result<()> {
    // No synthetic model headers are added to the standard lifecycle fixture.
    let provider = Provider::Websocket(
        responses::start_websocket_server_with_headers(vec![
            responses::WebSocketConnectionConfig {
                requests,
                response_headers: headers,
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
    let id = start(&controller, "websocket-response-model").await;
    controller.tick().await?;
    let store = controller.store().await?;
    let run = store.load_run_metadata(&id).await?;
    let (thread, _, _) = store
        .execution(&run.operation_id)
        .await?
        .expect("execution");
    settled(&threads, &thread).await?;
    // Agent status precedes durable collection. A failed active assignment does
    // not imply whole-audit publication; wait for this assignment's terminal state.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            controller.tick().await?;
            if matches!(
                store.load_run(&id).await?.assignment_state,
                AssignmentState::Succeeded | AssignmentState::Failed
            ) {
                break anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let evidence = store.model_evidence(&run.assignment_id).await?;
    assert_eq!(
        provider.generations().await,
        expected_states.len(),
        "no replay"
    );
    assert_eq!(evidence.len(), expected_states.len());
    for (record, expected) in evidence.iter().zip(expected_states) {
        assert_eq!(&record.state, expected);
        assert_eq!(record.expected_model.as_deref(), Some("audit-test"));
        if *expected == "verified" {
            assert_eq!(record.observed_model.as_deref(), Some("audit-test"));
            assert_eq!(record.evidence_scope.as_deref(), Some("generation"));
        }
    }
    let result = store.load_run(&id).await?;
    assert_eq!(
        result.assignment_state,
        if expected_success {
            AssignmentState::Succeeded
        } else {
            AssignmentState::Failed
        }
    );
    if expected_success {
        assert_eq!(
            store.raw_result(&result)?,
            Some(b"A completed fixture opinion.".to_vec())
        );
        assert_eq!(
            store
                .result_extraction(&run.assignment_id)
                .await?
                .expect("extraction")
                .status,
            codex_app_server_protocol::AuditExtractionStatus::UnstructuredOpinion
        );
    } else {
        assert!(store.result_extraction(&run.assignment_id).await?.is_none());
    }
    threads
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    provider.shutdown().await;
    Ok(())
}

#[test]
fn matching_lifecycle_models_bind_each_tool_generation() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-matching", || async {
        check(
            vec![
                tool_generation("first"),
                final_generation("second", Some("audit-test")),
            ],
            vec![],
            &["verified", "verified"],
            true,
        )
        .await
    })
}

#[test]
fn terminal_model_can_attest_an_already_bound_response() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-terminal", || async {
        let mut events = final_generation("first", None);
        events[2]["response"]["model"] = json!("audit-test");
        check(vec![events], vec![], &["verified"], true).await
    })
}

#[test]
fn missing_or_request_like_output_models_do_not_attest() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-missing", || async {
        let mut events = final_generation("first", None);
        events[1]["response"] = json!({"id":"first","model":"audit-test"});
        check(vec![events], vec![], &["missing"], false).await
    })
}

#[test]
fn mismatched_and_conflicting_lifecycle_models_fail_closed() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-conflicts", || async {
        check(
            vec![final_generation("first", Some("other-model"))],
            vec![],
            &["mismatch"],
            false,
        )
        .await?;
        let mut events = final_generation("first", Some("audit-test"));
        events[2]["response"]["model"] = json!("other-model");
        check(vec![events], vec![], &["mismatch"], false).await?;
        let mut events = final_generation("first", Some("audit-test"));
        events[0]["response"]["headers"] = json!({"OpenAI-Model":"other-model"});
        check(vec![events], vec![], &["mismatch"], false).await
    })
}

#[test]
fn a_prior_generation_cannot_supply_the_current_model_or_response_id() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-stale", || async {
        // A prior model must not be cached into a later generation with none.
        check(
            vec![tool_generation("first"), final_generation("second", None)],
            vec![],
            &["verified", "missing"],
            false,
        )
        .await?;
        // A delayed old terminal must not attest the new generation.
        let mut events = final_generation("second", None);
        events[2] = lifecycle("response.completed", "first", Some("audit-test"));
        check(
            vec![tool_generation("first"), events],
            vec![],
            &["verified", "missing"],
            false,
        )
        .await?;
        // Replaying the entire old lifecycle must not bind a fresh admission.
        check(
            vec![
                tool_generation("first"),
                final_generation("first", Some("audit-test")),
            ],
            vec![],
            &["verified", "missing"],
            false,
        )
        .await
    })
}

#[test]
fn a_connection_model_cannot_replace_generation_identity() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-connection", || async {
        check(
            vec![final_generation("first", None)],
            vec![("OpenAI-Model".to_owned(), "audit-test".to_owned())],
            &["missing"],
            false,
        )
        .await
    })
}

#[test]
fn a_terminal_without_creation_cannot_bind_its_own_identity() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-ws-lifecycle-unbound", || async {
        check(
            vec![vec![lifecycle(
                "response.completed",
                "unbound",
                Some("audit-test"),
            )]],
            vec![],
            &["missing"],
            false,
        )
        .await
    })
}
