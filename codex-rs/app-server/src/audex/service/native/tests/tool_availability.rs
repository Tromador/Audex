//! Exercise the real native adapter and Code Mode host with a local model fixture.
use super::*;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_protocol::openai_models::{ConfigShellToolType, ToolMode};

async fn qualify(
    mode: ToolMode,
    host_program: Option<PathBuf>,
    inspect_source: bool,
) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut host, run, _) = fixture(&temp).await?;
    let mut model = codex_models_manager::model_info::model_info_from_slug("audit-test");
    model.tool_mode = Some(mode);
    model.shell_type = ConfigShellToolType::UnifiedExec;
    let models = Arc::new(StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![model],
        },
        ModelOrigin::ExplicitConfig,
    ));
    let server = responses::start_mock_server().await;
    host.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    host.model_provider.supports_websockets = false;
    host.model_providers
        .insert(host.model_provider_id.clone(), host.model_provider.clone());
    let result = json!({"summary":"No defects found", "findings":[]}).to_string();
    let mut replies = Vec::new();
    if inspect_source {
        let call = if mode == ToolMode::CodeModeOnly {
            responses::ev_custom_tool_call(
                "read",
                "exec",
                r#"
const names = Object.keys(tools).sort();
if (JSON.stringify(names) !== '["exec_command","write_stdin"]') {
    throw new Error("Unexpected nested tools: " + JSON.stringify(names));
}
text(await tools.exec_command({cmd: "cat named.rs", yield_time_ms: 1000}));
text(await tools.exec_command({cmd: "printf forbidden > named.rs", yield_time_ms: 1000}));
"#,
            )
        } else {
            responses::ev_function_call(
                "read",
                "exec_command",
                &json!({
                    "cmd":"cat named.rs", "yield_time_ms":1000
                })
                .to_string(),
            )
        };
        replies.push(responses::sse(vec![
            responses::ev_response_created("read"),
            call,
            responses::ev_completed("read"),
        ]));
    }
    replies.push(responses::sse(vec![
        responses::ev_response_created("answer"),
        responses::ev_assistant_message("answer", &result),
        responses::ev_completed("answer"),
    ]));
    let response = responses::mount_sse_sequence(&server, replies).await;
    let provider: Arc<dyn codex_code_mode::CodeModeSessionProvider> = match host_program {
        Some(program) => Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
            program,
        )),
        None => Arc::new(codex_code_mode::DisabledCodeModeSessionProvider),
    };
    let harness = core_test_support::test_codex::test_codex()
        .with_models_manager(models)
        .with_config(|config| {
            config.model = Some("audit-test".into());
            config.model_reasoning_effort = None;
            config.features.disable(Feature::CodeModeHost).unwrap();
        })
        .with_thread_manager(move |manager| manager.with_code_mode_session_provider(provider))
        .build(&server)
        .await?;
    harness.codex.shutdown_and_wait().await?;
    let threads = harness.thread_manager.clone();
    let models = threads.get_models_manager();
    let effective = models
        .get_model_info("audit-test", &host.to_models_manager_config())
        .await;
    assert_eq!(effective.tool_mode, Some(mode));
    assert_eq!(effective.shell_type, ConfigShellToolType::UnifiedExec);
    let runner = Arc::new(NativeReviewer::new(
        Arc::new(host),
        threads.clone(),
        listener(),
        Arc::new(|_, _| Box::pin(async { Ok(()) })),
    ));
    let mut service = AuditService::open(temp.path(), runner, Arc::new(|_| {}))?;
    let id = service.start(StartRequest {
        input: Input::Paths(run.targets.paths.clone()),
        brief: run.brief,
        guidance: run.guidance,
        reviewers: vec![selection()],
    })?;
    tokio::time::timeout(Duration::from_secs(30), service.wait(id)).await??;
    let dir = attempt_directory(&service, id);
    let status: AttemptStatus = record::read_json(&dir.join("status.json"))?;
    println!(
        "Native fixture {mode:?}: state={:?}, tool_calls={}, inspected_source={inspect_source}",
        status.state, status.tool_calls
    );
    assert!(
        threads
            .get_thread(ThreadId::from_string(
                status.native_thread_id.as_deref().unwrap()
            )?)
            .await
            .is_err()
    );
    assert_eq!(status.state, State::Succeeded, "{status:?}");
    let checked: serde_json::Value = record::read_json(&dir.join("result.json"))?;
    assert_eq!(checked["findings"], json!([]));
    assert_eq!(checked["summary"], "No defects found");
    assert_eq!(
        response.requests().len(),
        if inspect_source { 2 } else { 1 }
    );
    let body = response.requests()[0].body_json();
    let mut names = Vec::new();
    for tool in body["tools"].as_array().unwrap() {
        if let Some(children) = tool["tools"].as_array() {
            assert_eq!(tool["name"], "functions");
            names.extend(
                children
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap().to_owned()),
            );
        } else {
            names.push(tool["name"].as_str().unwrap().to_owned());
        }
    }
    names.sort();
    assert_eq!(
        names,
        if mode == ToolMode::Direct {
            vec!["exec_command", "write_stdin"]
        } else {
            vec!["exec", "wait"]
        }
    );
    if inspect_source {
        assert_eq!(
            status.tool_calls, 1,
            "one model call, including Code Mode's two nested commands, counts once"
        );
        let output = if mode == ToolMode::Direct {
            response.function_call_output_text("read").unwrap()
        } else {
            response.requests()[1]
                .custom_tool_call_output("read")
                .to_string()
        };
        assert!(output.contains("source marker"), "{output}");
        if mode == ToolMode::CodeModeOnly {
            assert!(output.contains("Read-only file system"), "{output}");
        }
    } else {
        assert_eq!(
            status.tool_calls, 0,
            "no tool-count heuristic may reject a valid result"
        );
    }
    assert_eq!(
        fs::read_to_string(&run.targets.paths[0])?,
        "source marker\nsecond line\n"
    );
    assert!(!dir.parent().unwrap().join("attempt-2").exists());
    Ok(())
}

fn code_mode_host() -> Result<PathBuf> {
    Ok(codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?)
}

#[test]
fn code_mode_only_reviewer_reads_source_without_expanding_shell_permissions() -> Result<()> {
    run_test_with_large_stack("native-code-mode-tools", || async {
        qualify(ToolMode::CodeModeOnly, Some(code_mode_host()?), true).await
    })
}

#[test]
fn direct_reviewer_reads_source_and_preserves_zero_findings() -> Result<()> {
    run_test_with_large_stack("native-direct-tools", || async {
        qualify(ToolMode::Direct, None, true).await
    })
}

#[test]
fn zero_findings_and_zero_calls_are_valid_when_tools_are_available() -> Result<()> {
    run_test_with_large_stack("native-zero-findings", || async {
        for mode in [ToolMode::Direct, ToolMode::CodeModeOnly] {
            qualify(mode, Some(code_mode_host()?), false).await?;
        }
        Ok(())
    })
}
