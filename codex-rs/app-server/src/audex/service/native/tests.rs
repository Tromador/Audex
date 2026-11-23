use super::*;
use crate::audex::reviewer::native::{RULES, prepare};
use codex_extension_api::{ToolName, ToolPolicy};
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::{ModelOrigin, manager::StaticModelsManager};
use codex_protocol::config_types::EnvironmentVariablePattern;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use core_test_support::responses;
use core_test_support::test_codex::run_test_with_large_stack;
use serde_json::json;
use std::time::Duration;

mod sandbox;
mod tool_availability;

fn selection() -> ReviewerSelection {
    ReviewerSelection {
        label: "native".into(),
        route: "native".into(),
        model: "audit-test".into(),
        effort: None,
        permission_summary: "must be replaced by the effective permissions".into(),
    }
}

async fn fixture(temp: &tempfile::TempDir) -> Result<(Config, RunRecord, StaticModelsManager)> {
    let source = temp.path().join("source");
    let context = temp.path().join("context");
    fs::create_dir(&source)?;
    fs::create_dir(&context)?;
    fs::write(source.join("named.rs"), "source marker\nsecond line\n")?;
    fs::write(context.join("caller.rs"), "caller marker\n")?;
    fs::write(source.join("AGENTS.md"), "TARGET_AGENTS_SENTINEL")?;
    fs::write(temp.path().join("AGENTS.md"), "HOME_AGENTS_SENTINEL")?;
    let mut host = Config::load_default_with_cli_overrides_for_codex_home(
        temp.path().to_path_buf(),
        Vec::new(),
    )
    .await?;
    host.developer_instructions = Some("HOST_DEVELOPER_SENTINEL".into());
    host.base_instructions = Some("HOST_BASE_SENTINEL".into());
    host.notify = Some(vec!["forbidden-notify".into()]);
    host.project_doc_max_bytes = 9999;
    host.agents_enabled = true;
    host.cloud_skill_enabled = true;
    host.orchestrator_mcp_enabled = true;
    host.include_skill_instructions = true;
    host.include_apps_instructions = true;
    host.experimental_request_user_input_enabled = true;
    host.update_plan_enabled = true;
    host.web_search_mode.set(WebSearchMode::Live)?;
    for feature in [
        Feature::CodexHooks,
        Feature::Plugins,
        Feature::Apps,
        Feature::CodeMode,
        Feature::Collab,
        Feature::MemoryTool,
    ] {
        host.features.enable(feature)?;
    }
    let mcp = toml::from_str("command = \"forbidden-mcp\"")?;
    host.mcp_servers
        .set([("forbidden".into(), mcp)].into_iter().collect())?;
    host.codex_self_exe = Some(codex_utils_cargo_bin::cargo_bin("audex")?);
    host.codex_linux_sandbox_exe = host.codex_self_exe.clone();
    let run = RunRecord {
        id: Uuid::now_v7(),
        targets: super::super::targets::resolve(&[
            source.join("named.rs"),
            context.join("caller.rs"),
        ])?,
        brief: "Inspect the named files".into(),
        guidance: Some("Keep the full operator guidance".into()),
        reviewers: vec![selection()],
        created_at: record::now(),
    };
    let models = StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![codex_models_manager::model_info::model_info_from_slug(
                "audit-test",
            )],
        },
        ModelOrigin::ExplicitConfig,
    );
    Ok((host, run, models))
}

#[tokio::test]
async fn construction_preserves_targets_and_enforces_isolation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut host, mut run, models) = fixture(&temp).await?;
    let ambient_policy = ShellEnvironmentPolicy {
        inherit: ShellEnvironmentPolicyInherit::All,
        ignore_default_excludes: true,
        exclude: vec![EnvironmentVariablePattern::new_case_insensitive("PATH")],
        r#set: [("DATABASE_URL".into(), "synthetic-host-override".into())]
            .into_iter()
            .collect(),
        include_only: vec![EnvironmentVariablePattern::new_case_insensitive("*")],
        use_profile: true,
    };
    host.permissions.shell_environment_policy = ambient_policy.clone();
    // Both explicit file and directory selections preserve their exact scope.
    for directories in [false, true] {
        if directories {
            run.targets
                .paths
                .clone_from(&run.targets.working_directories);
        }
        let prepared = prepare(&host, &models, &run, &selection(), temp.path()).await?;
        let config = &prepared.options.config;
        assert_eq!(config.cwd.as_path(), run.targets.working_directories[0]);
        assert_eq!(
            config
                .workspace_roots
                .iter()
                .map(|r| r.as_path().to_path_buf())
                .collect::<Vec<_>>(),
            run.targets.working_directories
        );
        let fs = config.permissions.file_system_sandbox_policy();
        assert!(
            fs.get_writable_roots_with_cwd(config.cwd.as_path())
                .is_empty()
        );
        assert!(!fs.has_full_disk_read_access());
        assert!(!fs.has_full_disk_write_access());
        for root in &run.targets.working_directories {
            assert!(fs.can_read_local_path_with_cwd(root, config.cwd.as_path()));
            assert!(!fs.can_write_local_path_with_cwd(root, config.cwd.as_path()));
        }
        assert!(!fs.can_read_local_path_with_cwd(temp.path(), config.cwd.as_path()));
        assert!(!fs.can_read_local_path_with_cwd(Path::new("/mnt/c"), config.cwd.as_path()));
        assert_eq!(
            config.permissions.network_sandbox_policy(),
            NetworkSandboxPolicy::Restricted
        );
        assert!(config.permissions.network.is_none());
        assert_eq!(
            config.permissions.approval_policy.value(),
            AskForApproval::Never
        );
        assert!(!config.permissions.allow_login_shell);
        let shell_policy = &config.permissions.shell_environment_policy;
        assert_eq!(shell_policy.inherit, ShellEnvironmentPolicyInherit::Core);
        assert!(!shell_policy.ignore_default_excludes);
        assert!(shell_policy.exclude.is_empty());
        assert!(shell_policy.r#set.is_empty());
        assert!(shell_policy.include_only.is_empty());
        assert!(!shell_policy.use_profile);
        assert_eq!(config.project_doc_max_bytes, 0);
        assert!(config.project_doc_fallback_filenames.is_empty());
        assert_eq!(
            config.config_layer_stack.effective_config(),
            host.config_layer_stack.effective_config()
        );
        assert!(config.base_instructions.is_none());
        assert_eq!(config.developer_instructions.as_deref(), Some(RULES));
        assert!(RULES.contains("Do not execute source"));
        assert!(config.notify.is_none());
        assert!(config.mcp_servers.get().contains_key("forbidden"));
        assert_eq!(config.plugins, host.plugins);
        assert_eq!(config.web_search_mode.value(), WebSearchMode::Live);
        assert!(!config.agents_enabled && !config.cloud_skill_enabled);
        assert_eq!(config.orchestrator_mcp_enabled, host.orchestrator_mcp_enabled);
        assert!(!config.include_skill_instructions && !config.include_apps_instructions);
        assert!(!config.experimental_request_user_input_enabled && !config.update_plan_enabled);
        assert!(!config.allow_persistent_dynamic_tool_updates);
        for feature in [
            Feature::CodexHooks,
            Feature::Plugins,
            Feature::Apps,
            Feature::CodeMode,
            Feature::Collab,
            Feature::MemoryTool,
        ] {
            assert!(config.features.enabled(feature));
        }
        assert!(config.features.enabled(Feature::ShellTool));
        assert!(config.features.enabled(Feature::UnifiedExec));
        assert_eq!(config.codex_linux_sandbox_exe, host.codex_linux_sandbox_exe);
        assert_eq!(
            config.model_provider.request_max_retries,
            host.model_provider.request_max_retries
        );
        assert_eq!(
            config.model_provider.stream_max_retries,
            host.model_provider.stream_max_retries
        );
        assert_eq!(
            config.model_auto_compact_token_limit,
            host.model_auto_compact_token_limit
        );
        assert_eq!(config.model_context_window, host.model_context_window);
        let extensions = &prepared.options.thread_extension_init;
        assert!(extensions.get::<codex_extension_api::SessionIsolation>().is_none());
        let policy = extensions.get::<ToolPolicy>().unwrap();
        assert!(policy.allowed_tools.is_some());
        assert!(policy.require_managed_sandbox && policy.require_unified_exec);
        assert!(!policy.expose_additional_permissions);
        assert!(policy.allows(&ToolName::plain("exec_command")));
        assert!(policy.allows(&ToolName::namespaced("web", "run")));
        assert!(policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__context7",
            "_query_docs"
        )));
        assert!(policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__github",
            "_fetch_file"
        )));
        assert!(!policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__github",
            "_update_file"
        )));
        assert!(!policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__github",
            "_merge_pull_request"
        )));
        assert!(!policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__zen_storage",
            "_write_file"
        )));
        assert!(!policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__gmail",
            "_fetch"
        )));
        assert!(!policy.allows(&ToolName::namespaced(
            "mcp__codex_apps__zen_storage",
            "_read_text_file"
        )));
        assert!(prepared.options.dynamic_tools.is_empty());
        assert!(
            prepared
                .options
                .client_mcp_extensions
                .iter()
                .next()
                .is_none()
        );
        assert!(prepared.options.user_instructions.is_none());
        assert!(prepared.options.thread_instructions_provider.is_none());
        assert!(prepared.options.inherited_environments.is_none());
        assert!(matches!(
            prepared.options.initial_history,
            codex_rollout::InitialHistory::New
        ));
        assert_eq!(
            prepared.input.start.final_output_json_schema,
            Some(output_schema())
        );
        let codex_protocol::turn_input::TurnInput::UserInput { content, .. } = prepared.input.input
        else {
            panic!("user assignment");
        };
        let codex_protocol::user_input::UserInput::Text { text, .. } = &content[0] else {
            panic!("assignment text");
        };
        let assignment: serde_json::Value = serde_json::from_str(text)?;
        assert_eq!(assignment["targets"], json!(run.targets.paths));
        assert_eq!(assignment["guidance"], json!(run.guidance));
    }
    assert_eq!(host.project_doc_max_bytes, 9999);
    assert_eq!(host.permissions.shell_environment_policy, ambient_policy);
    Ok(())
}

// Test host for the same extension/event boundary used by listen_for_audex.
fn listener() -> Listen {
    Arc::new(|_, thread| {
        Box::pin(async move {
            let events = thread
                .thread_extension_data()
                .get::<NativeEvents>()
                .unwrap();
            let guard = events.listener();
            tokio::spawn(async move {
                let _guard = guard;
                while let Ok(event) = thread.next_event().await {
                    events.observe(&event);
                    if matches!(event.msg, EventMsg::ShutdownComplete) {
                        break;
                    }
                }
            });
            Ok(())
        })
    })
}

fn attempt_directory(service: &AuditService, id: RunId) -> PathBuf {
    service
        .run_directory(id)
        .join("reviewers/01-native/attempt-1")
}

#[test]
fn native_turn_delivers_effective_tools_schema_output_and_usage_to_s1() -> Result<()> {
    run_test_with_large_stack("s2-native-turn", || async {
        let temp = tempfile::tempdir()?;
        let (mut host, run, _) = fixture(&temp).await?;
        let server = responses::start_mock_server().await;
        host.model_provider.base_url = Some(format!("{}/v1", server.uri()));
        host.model_provider.supports_websockets = false;
        host.model_providers
            .insert(host.model_provider_id.clone(), host.model_provider.clone());
        let result = json!({"summary":"Controlled native result", "findings":[{
            "path":"named.rs", "start_line":1, "end_line":2, "title":"Fixture", "severity":"low", "confidence":"high", "trigger":"Fixture", "mechanism":"Fixture", "impact":"Fixture", "evidence":"source marker", "remediation":"Fixture"}]}).to_string();
        let mut completed = responses::ev_completed("s2");
        completed["response"]["usage"] =
            json!({"input_tokens":123, "output_tokens":45, "total_tokens":168});
        let response = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_response_created("read"),
                    responses::ev_function_call(
                        "read",
                        "exec_command",
                        &json!({"cmd":"cat named.rs", "yield_time_ms":1000}).to_string(),
                    ),
                    responses::ev_completed("read"),
                ]),
                responses::sse(vec![
                    responses::ev_response_created("s2"),
                    responses::ev_assistant_message("answer", &result),
                    completed,
                ]),
            ],
        )
        .await;
        let threads = Arc::new(
            codex_core::test_support::thread_manager_with_models_provider(
                CodexAuth::from_api_key("dummy"),
                host.model_provider.clone(),
            ),
        );
        let cleanup_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cleanup: Listen = {
            let count = cleanup_count.clone();
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
        };
        let runner = Arc::new(NativeReviewer::new(
            Arc::new(host),
            threads.clone(),
            listener(),
            cleanup,
        ));
        let mut service = AuditService::open(temp.path(), runner, Arc::new(|_| {}))?;
        let id = service.start(StartRequest {
            input: Input::Paths(run.targets.paths),
            brief: run.brief,
            guidance: run.guidance,
            reviewers: vec![selection()],
        })?;
        tokio::time::timeout(Duration::from_secs(30), service.wait(id)).await??;
        let dir = attempt_directory(&service, id);
        let status: AttemptStatus = record::read_json(&dir.join("status.json"))?;
        assert_eq!(status.state, State::Succeeded, "{status:?}");
        assert!(
            status
                .reviewer
                .permission_summary
                .contains("No writable local roots; shell network restricted")
        );
        assert!(
            !status
                .reviewer
                .permission_summary
                .contains("must be replaced")
        );
        assert!(
            status
                .rollout_path
                .as_ref()
                .is_some_and(|path| path.exists())
        );
        assert!(
            threads
                .get_thread(ThreadId::from_string(
                    status.native_thread_id.as_deref().unwrap()
                )?)
                .await
                .is_err()
        );
        assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read_to_string(dir.join("output.txt"))?, result);
        let checked: serde_json::Value = record::read_json(&dir.join("result.json"))?;
        assert_eq!(checked["findings"][0]["citation"], "ok");
        let usage: Usage = record::read_json(&dir.join("usage.json"))?;
        assert!(matches!(usage.basis, Measurement::Measured));
        assert_eq!(usage.input_tokens, Some(123));
        assert_eq!(usage.output_tokens, Some(45));
        assert_eq!(usage.cost_usd, None);
        assert!(!temp.path().join("audex/audit-state.sqlite3").exists());
        assert!(!temp.path().join("audex/snapshots").exists());
        assert_eq!(response.requests().len(), 2);
        for request in response.requests() {
            assert_eq!(
                request.header("authorization").as_deref(),
                Some("Bearer dummy")
            );
        }
        let tool_output = response.function_call_output_text("read").unwrap();
        assert!(tool_output.contains("source marker"), "{tool_output}");
        assert_eq!(status.tool_calls, 1);
        let body = response.requests()[0].body_json();
        let mut names = Vec::new();
        for tool in body["tools"].as_array().unwrap() {
            if let Some(children) = tool["tools"].as_array() {
                assert_eq!(tool["name"], "functions");
                names.extend(
                    children
                        .iter()
                        .map(|t| t["name"].as_str().unwrap().to_owned()),
                );
            } else {
                names.push(tool["name"].as_str().unwrap().to_owned());
            }
        }
        names.sort();
        assert!(names.contains(&"exec_command".to_owned()));
        assert!(names.contains(&"write_stdin".to_owned()));
        assert_eq!(body["text"]["format"]["schema"], output_schema());
        for sentinel in [
            "HOST_DEVELOPER_SENTINEL",
            "HOST_BASE_SENTINEL",
            "TARGET_AGENTS_SENTINEL",
            "HOME_AGENTS_SENTINEL",
        ] {
            assert!(!body.to_string().contains(sentinel), "inherited {sentinel}");
        }
        Ok(())
    })
}

#[tokio::test]
async fn events_retain_partial_output_and_terminal_failure_and_count_both_tools() -> Result<()> {
    let (events, state) = NativeEvents::new();
    for msg in [
        json!({"type":"raw_response_item", "item":{"type":"function_call", "name":"exec_command", "call_id":"a", "arguments":"{}"}}),
        json!({"type":"raw_response_item", "item":{"type":"function_call", "name":"write_stdin", "call_id":"b", "arguments":"{}"}}),
        json!({"type":"agent_message_content_delta", "thread_id":"thread", "turn_id":"turn", "item_id":"answer", "delta":"retained partial"}),
        json!({"type":"turn_complete", "turn_id":"turn", "last_agent_message":null, "error":{"message":"controlled failure", "codex_error_info":null}}),
    ] {
        events.observe(&Event {
            id: "turn".into(),
            msg: serde_json::from_value(msg)?,
        });
    }
    let current = state.borrow();
    assert_eq!(current.output, "retained partial");
    assert_eq!(current.tool_calls, 2);
    assert!(
        matches!(&current.terminal, Some((_, Terminal::Failed(reason))) if reason == "controlled failure")
    );
    Ok(())
}

#[test]
fn tool_activity_counts_model_calls_without_outputs_or_nested_execution() -> Result<()> {
    for namespace in [None, Some("functions")] {
        for calls in [
            vec![
                json!({"type":"function_call", "name":"exec_command", "call_id":"a", "arguments":"{}", "namespace":namespace}),
                json!({"type":"function_call", "name":"write_stdin", "call_id":"b", "arguments":"{}", "namespace":namespace}),
            ],
            vec![
                json!({"type":"custom_tool_call", "name":"exec", "call_id":"a", "input":"text(await tools.exec_command({cmd:'cat source'}));", "namespace":namespace}),
                json!({"type":"function_call", "name":"wait", "call_id":"b", "arguments":"{}", "namespace":namespace}),
            ],
        ] {
            let (events, state) = NativeEvents::new();
            for (index, item) in calls.into_iter().enumerate() {
                events.observe(&Event {
                    id: "turn".into(),
                    msg: serde_json::from_value(json!({"type":"raw_response_item", "item":item}))?,
                });
                assert_eq!(state.borrow().tool_calls, (index + 1) as u64);
            }
            for item in [
                json!({"type":"function_call_output", "call_id":"b", "output":"done"}),
                json!({"type":"custom_tool_call_output", "call_id":"a", "output":"source"}),
                json!({"type":"function_call", "name":"unrelated", "call_id":"c", "arguments":"{}"}),
                json!({"type":"custom_tool_call", "name":"unrelated", "call_id":"d", "input":""}),
                json!({"type":"function_call", "name":"exec", "call_id":"e", "arguments":"{}"}),
                json!({"type":"custom_tool_call", "name":"wait", "call_id":"f", "input":""}),
            ] {
                events.observe(&Event {
                    id: "turn".into(),
                    msg: serde_json::from_value(json!({"type":"raw_response_item", "item":item}))?,
                });
                assert_eq!(state.borrow().tool_calls, 2);
            }
            // Both direct and nested shell execution report these lifecycle events.
            for msg in [
                json!({"type":"exec_command_begin", "call_id":"shell", "turn_id":"turn", "command":["cat","source"], "cwd":"file:///", "parsed_cmd":[]}),
                json!({"type":"exec_command_end", "call_id":"shell", "turn_id":"turn", "command":["cat","source"], "cwd":"file:///", "parsed_cmd":[], "exit_code":0, "duration":{"secs":0,"nanos":1}, "status":"completed"}),
            ] {
                events.observe(&Event {
                    id: "turn".into(),
                    msg: serde_json::from_value(msg)?,
                });
                assert_eq!(state.borrow().tool_calls, 2);
            }
        }
    }
    Ok(())
}

#[test]
fn cancellation_stops_native_execution_and_retains_partial_output() -> Result<()> {
    run_test_with_large_stack("s2-native-cancel", || async {
        let temp = tempfile::tempdir()?;
        let (mut host, run, _) = fixture(&temp).await?;
        let server = responses::start_mock_server().await;
        host.model_provider.base_url = Some(format!("{}/v1", server.uri()));
        host.model_provider.supports_websockets = false;
        host.model_providers
            .insert(host.model_provider_id.clone(), host.model_provider.clone());
        responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_response_created("s2-cancel"),
                responses::ev_assistant_message("partial", "Retained partial review"),
                responses::ev_function_call(
                    "sleep",
                    "exec_command",
                    &json!({"cmd":"/bin/sleep 30", "yield_time_ms":1000}).to_string(),
                ),
                responses::ev_completed_with_tokens("s2-cancel", 17),
            ]),
        )
        .await;
        let threads = Arc::new(
            codex_core::test_support::thread_manager_with_models_provider(
                CodexAuth::from_api_key("dummy"),
                host.model_provider.clone(),
            ),
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        let listen: Listen = {
            let entered = entered.clone();
            Arc::new(move |_, thread| {
                let entered = entered.clone();
                Box::pin(async move {
                    let events = thread
                        .thread_extension_data()
                        .get::<NativeEvents>()
                        .unwrap();
                    let guard = events.listener();
                    tokio::spawn(async move {
                        let _guard = guard;
                        while let Ok(event) = thread.next_event().await {
                            events.observe(&event);
                            if matches!(event.msg, EventMsg::ExecCommandBegin(_)) {
                                entered.notify_one();
                            }
                            if matches!(event.msg, EventMsg::ShutdownComplete) {
                                break;
                            }
                        }
                    });
                    Ok(())
                })
            })
        };
        let runner = Arc::new(NativeReviewer::new(
            Arc::new(host),
            threads.clone(),
            listen,
            Arc::new(|_, _| Box::pin(async { Ok(()) })),
        ));
        let mut service = AuditService::open(temp.path(), runner, Arc::new(|_| {}))?;
        let id = service.start(StartRequest {
            input: Input::Paths(run.targets.paths),
            brief: run.brief,
            guidance: None,
            reviewers: vec![selection()],
        })?;
        tokio::time::timeout(Duration::from_secs(20), entered.notified()).await?;
        service.cancel(id, Some(0))?;
        tokio::time::timeout(Duration::from_secs(20), service.wait(id)).await??;
        let dir = attempt_directory(&service, id);
        let status: AttemptStatus = record::read_json(&dir.join("status.json"))?;
        assert_eq!(status.state, State::Cancelled);
        assert_eq!(status.tool_calls, 1);
        assert_eq!(
            fs::read_to_string(dir.join("output.txt"))?,
            "Retained partial review"
        );
        assert!(!dir.parent().unwrap().join("attempt-2").exists());
        assert!(
            threads
                .get_thread(ThreadId::from_string(
                    status.native_thread_id.as_deref().unwrap()
                )?)
                .await
                .is_err()
        );
        Ok(())
    })
}

#[tokio::test]
async fn remote_audits_use_private_local_cwd_and_read_only_zen_tools() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (host, mut run, models) = fixture(&temp).await?;
    run.targets = super::super::targets::resolve_mixed(
        &[PathBuf::from("O:/source/project")],
        &[Some(true)],
    )?;
    let private = temp.path().join("private");
    fs::create_dir(&private)?;
    let prepared = prepare(&host, &models, &run, &selection(), &private).await?;
    assert_eq!(prepared.options.config.cwd.as_path(), private.as_path());
    assert_eq!(
        prepared.options.config.workspace_roots[0].as_path(),
        private.as_path()
    );
    let policy = prepared.options.thread_extension_init.get::<ToolPolicy>().unwrap();
    assert!(policy.allowed_tools.is_some());
    assert!(policy.allows(&ToolName::namespaced(
        "mcp__codex_apps__zen_storage",
        "_read_text_file"
    )));
    assert!(!policy.allows(&ToolName::namespaced(
        "mcp__codex_apps__zen_storage",
        "_write_file"
    )));
    assert!(!policy.allows(&ToolName::namespaced(
        "mcp__codex_apps__zen_storage",
        "_delete_file"
    )));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn helper_runtime_read_resolves_only_the_executable_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut host, run, models) = fixture(&temp).await?;
    let runtime = temp.path().join("runtime");
    fs::create_dir(&runtime)?;
    let executable = runtime.join("helper");
    let neighbour = runtime.join("neighbour");
    fs::write(&executable, "construction fixture only")?;
    fs::write(&neighbour, "outside source access")?;
    let alias = temp.path().join("helper-alias");
    std::os::unix::fs::symlink(&executable, &alias)?;
    host.codex_linux_sandbox_exe = Some(alias);
    let prepared = prepare(&host, &models, &run, &selection(), temp.path()).await?;
    let config = prepared.options.config;
    assert_eq!(config.codex_linux_sandbox_exe, Some(executable.clone()));
    let fs = config.permissions.file_system_sandbox_policy();
    assert!(fs.can_read_local_path_with_cwd(&executable, config.cwd.as_path()));
    assert!(!fs.can_write_local_path_with_cwd(&executable, config.cwd.as_path()));
    assert!(!fs.can_read_local_path_with_cwd(&runtime, config.cwd.as_path()));
    assert!(!fs.can_read_local_path_with_cwd(&neighbour, config.cwd.as_path()));
    assert!(
        prepared
            .permission_summary
            .contains(executable.to_str().unwrap())
    );
    assert!(prepared.permission_summary.contains("exact file"));
    host.codex_linux_sandbox_exe = Some(runtime);
    assert!(prepare(&host, &models, &run, &selection(), temp.path()).await.is_err());
    Ok(())
}
