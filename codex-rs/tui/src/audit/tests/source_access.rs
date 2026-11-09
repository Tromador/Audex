//! Production daemon-backed retrieval with ordinary Codex tool handling.
use super::*;
use codex_app_server_protocol::AuditStartParams;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../../../cli/tests/app_server_daemon/support.rs"]
mod daemon_support;

#[test]
fn managed_conversation_reads_a_complete_file_in_one_source_call() -> anyhow::Result<()> {
    run_test_with_large_stack("audit-source-direct", || exercise(false))
}

#[test]
fn managed_conversation_uses_codex_code_mode_to_search_and_reuse_verified_source()
-> anyhow::Result<()> {
    run_test_with_large_stack("audit-source-code-mode", || exercise(true))
}

async fn exercise(code_mode: bool) -> anyhow::Result<()> {
    let provider = responses::start_mock_server().await;
    let daemon = daemon_support::TestDaemon::new()?;
    let home = daemon.home.path();
    // The fixture's package contains the actual candidate host, without a fallback.
    let selected = home.join("packages/standalone/current/bin");
    codex_utils_cargo_bin::copy_executable(
        &codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?,
        &selected.join("codex-code-mode-host"),
    )?;
    let mut mock = app_test_support::MockResponsesConfig::new(&provider.uri())
        .with_model("audit-test")
        .disable_feature(Feature::Plugins)
        .disable_feature(Feature::RemotePlugin)
        .disable_feature(Feature::PluginDiscoveryPrefetch)
        .with_root_config("developer_instructions = 'OPERATOR_ONLY_INSTRUCTION'");
    mock = if code_mode {
        mock.enable_feature(Feature::CodeMode)
    } else {
        mock.disable_feature(Feature::CodeMode)
    };
    mock.write(home)?;
    let config = ConfigBuilder::default()
        .codex_home(home.to_owned())
        .harness_overrides(ConfigOverrides {
            cwd: Some(home.to_owned()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let repeats = if code_mode { 10_000 } else { 500 };
    let original = format!(
        "fn main() {{}}\n{}// ORIGINAL_MIDDLE\n{}// ORIGINAL_TAIL\n",
        "// retained padding\n".repeat(repeats),
        "// retained padding\n".repeat(repeats)
    );
    let source = home.join("example.rs");
    std::fs::write(&source, &original)?;
    mount_provider(&provider, source.clone()).await;
    let operator_calls = Arc::new(AtomicUsize::new(0));
    let observed = operator_calls.clone();
    let expected = original.clone();
    let reference = Arc::new(std::sync::Mutex::new(Value::Null));
    let provider_reference = reference.clone();
    // This matcher isolates the conversational follow-up from reviewer fixture traffic.
    Mock::given(|request: &wiremock::Request| String::from_utf8_lossy(&request.body).contains("SOURCE_ACCESS_FOLLOWUP"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let index = observed.fetch_add(1, Ordering::SeqCst);
            let run = provider_reference.lock().unwrap().clone();
            assert!(body["input"].to_string().contains(run["runId"].as_str().unwrap()), "compact retained reference");
            let args = json!({"runId":run["runId"],"snapshot":run["snapshotId"],"path":"example.rs"});
            let events = if !code_mode && index == 0 {
                let (namespace, tool) = named_tool(&body["tools"], "audit_source").expect("production audit gate exposes source tool");
                vec![responses::ev_function_call_with_namespace("whole-source", &namespace, &tool, &args.to_string()), responses::ev_completed("read-source")]
            } else if !code_mode {
                assert_eq!(index, 1, "one source call must suffice");
                let data = nested_object(&body["input"], "totalBytes").expect("complete source result");
                assert_eq!(data["content"], expected);
                assert_eq!(data["totalBytes"], expected.len());
                assert_eq!(data["eof"], true);
                vec![responses::ev_assistant_message("checked", "Examined the complete captured source."), responses::ev_completed("source-checked")]
            } else if index == 0 {
                let code = format!(r#"
const name = ALL_TOOLS.find(t => t.name.endsWith("audit_source")).name;
const result = await tools[name]({args});
if (result.isError) throw new Error(JSON.stringify(result));
const source = JSON.parse(result.content[0].text);
if (source.content.length !== {length} || !source.eof) throw new Error("incomplete source");
store("verified-source", source);
const matches = source.content.split("\n").flatMap((line, i) => line.includes("ORIGINAL_") ? [{{line:i+1,text:line}}] : []);
text({{identity:source.identity,bytes:source.totalBytes,matches}});
"#, args=args, length=expected.len());
                vec![responses::ev_custom_tool_call("source-code", "exec", &code), responses::ev_completed("searched-source")]
            } else if index == 1 {
                let input = body["input"].to_string();
                assert!(input.contains("ORIGINAL_MIDDLE") && input.contains("ORIGINAL_TAIL"));
                assert!(!input.contains("// retained padding"), "whole source must not be forced into model context");
                let code = r#"const source = load("verified-source"); text({bytes:source.totalBytes, excerpt:source.content.slice(65537, 67585), tail:source.content.slice(-17)});"#;
                vec![responses::ev_custom_tool_call("source-reuse", "exec", code), responses::ev_completed("reused-source")]
            } else {
                assert_eq!(index, 2);
                let data = nested_object(&body["input"], "excerpt").expect("cached source range");
                assert_eq!(data["excerpt"], &expected[65537..67585]);
                assert_eq!(data["bytes"], expected.len());
                assert!(!body["input"].to_string().contains("CHANGED_WORKING_COPY"));
                vec![responses::ev_assistant_message("checked", "Searched and examined the original source from one verified read."), responses::ev_completed("source-checked")]
            };
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .insert_header("OpenAI-Model", "audit-test").set_body_string(responses::sse(events))
        }).with_priority(1).mount(&provider).await;
    daemon.lifecycle("start")?;
    let advertised = codex_app_server_client::app_server_control_socket_path(home)?;
    let socket =
        codex_app_server_client::verified_local_daemon_socket(advertised.as_path())?.unwrap();
    let endpoint = codex_app_server_client::RemoteAppServerEndpoint::UnixSocket {
        socket_path: socket,
    };
    let target = crate::AppServerTarget::LocalDaemon {
        endpoint: endpoint.clone(),
        allow_embedded_fallback: false,
    };
    let client = crate::connect_remote_app_server(endpoint)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        client,
        target.thread_params_mode(),
    )
    .with_local_codex_home(&config.codex_home);
    let (tx, _events) = mpsc::unbounded_channel();
    // Use the production availability check, not a manually forced audits=true bridge.
    session
        .start_dynamic_tool_mcp(
            config.clone(),
            AppEventSender::new(tx),
            tokio::sync::broadcast::channel(16).0,
        )
        .await?;
    let thread = boxed_future!(session.start_thread(&config))
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?
        .session
        .thread_id;
    let audits = AuditClient::new(session.request_handle());
    let params: AuditStartParams = serde_json::from_value(
        json!({"provider":"mock_provider","model":"audit-test","requestKey":"source-access-fixture","source":{"type":"file","name":"example.rs","path":source},"brief":"fixture"}),
    )?;
    let started = audits.start(params).await?;
    let retained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let run = audits.read(started.run_id.clone()).await.unwrap();
            if run.report.is_some() {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(retained.phase, AuditRunPhase::Completed);
    assert_eq!(provider.received_requests().await.unwrap().len(), 2);
    std::fs::write(&source, "CHANGED_WORKING_COPY")?;
    *reference.lock().unwrap() = json!({"runId":retained.run_id,"snapshotId":retained.snapshot_id});
    let mut contexts = ReportContexts::default();
    contexts.stage(thread, &retained);
    turn(
        &mut session,
        thread,
        "SOURCE_ACCESS_FOLLOWUP: inspect the captured source",
        &config,
        contexts.for_turn(thread),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let mut source_calls = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = session.next_event().await {
            if let AppServerEvent::ServerNotification(notification) = event.event {
                match notification.as_ref() {
                    ServerNotification::ItemCompleted(completed)
                        if completed.thread_id == thread.to_string() =>
                    {
                        let item = serde_json::to_value(&completed.item).unwrap();
                        if item["tool"]
                            .as_str()
                            .is_some_and(|tool| tool.ends_with("audit_source"))
                        {
                            source_calls += 1;
                        }
                    }
                    ServerNotification::TurnCompleted(completed)
                        if completed.thread_id == thread.to_string() =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .await?;
    assert_eq!(
        source_calls, 1,
        "complete read and local inspection must need one retrieval"
    );
    let calls = if code_mode { 3 } else { 2 };
    assert_eq!(operator_calls.load(Ordering::SeqCst), calls);
    assert_eq!(provider.received_requests().await.unwrap().len(), 2 + calls);
    assert_eq!(
        audits.read(retained.run_id.clone()).await?.revision,
        retained.revision
    );
    assert_eq!(std::fs::read_to_string(&source)?, "CHANGED_WORKING_COPY");
    let evidence = json!({"codeMode":code_mode,"capturedBytes":original.len(),"sourceToolCalls":source_calls,"operatorProviderRequests":calls,"reviewerProviderRequests":2,"changedWorkingCopyIgnored":true});
    if let Some(root) = std::env::var_os("AUDEX_S5_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(root).join(if code_mode {
                "source-code-mode.json"
            } else {
                "source-direct.json"
            }),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
    }
    session.shutdown().await?;
    Ok(())
}
