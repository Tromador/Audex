//! Real TUI tool transport and Codex loops against a deterministic loopback provider.
use super::*;
use crate::app_server_session::{AppServerSession, ResumeModelSettings, TurnPermissionsOverride};
use crate::dynamic_tools_mcp::{DynamicToolMcpServer, ThreadToolTransport, ToolServices};
use crate::legacy_core::config::{Config, ConfigBuilder, ConfigOverrides};
use crate::test_support::boxed_future;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::{ServerNotification, UserInput};
use codex_config::LoaderOverrides;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::test_codex::run_test_with_large_stack;
use std::sync::Arc;
use tokio::sync::mpsc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

type Result<T> = color_eyre::Result<T>;

struct Home(tempfile::TempDir);
impl Home {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("AUDEX_TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&parent)?;
        Ok(Self(tempfile::tempdir_in(parent)?))
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        // Only this disposable fixture's sealed directories are made removable.
        #[cfg(unix)]
        fn unseal(path: &std::path::Path) {
            use std::os::unix::fs::PermissionsExt;
            if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
                return;
            }
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    unseal(&entry.path());
                }
            }
        }
        #[cfg(unix)]
        unseal(self.0.path());
    }
}

async fn fixture(provider: &MockServer) -> Result<(Home, Config, AppServerSession, ThreadId)> {
    let home = Home::new()?;
    app_test_support::MockResponsesConfig::new(&provider.uri())
        .with_model("audit-test")
        .disable_feature(Feature::Plugins)
        .disable_feature(Feature::RemotePlugin)
        .disable_feature(Feature::PluginDiscoveryPrefetch)
        .with_root_config("developer_instructions = 'OPERATOR_ONLY_INSTRUCTION'")
        .write(home.0.path())?;
    let config = ConfigBuilder::default()
        .codex_home(home.0.path().to_owned())
        .harness_overrides(ConfigOverrides {
            cwd: Some(home.0.path().to_owned()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut session = boxed_future!(crate::start_embedded_app_server_for_picker(&config)).await?;
    let thread = boxed_future!(session.start_thread(&config))
        .await?
        .session
        .thread_id;
    Ok((home, config, session, thread))
}

fn arguments() -> StartArguments {
    StartArguments {
        source: AuditSource::Snippet {
            name: "example.rs".into(),
            content: "fn main() {}\n".into(),
        },
        model: None,
        provider: None,
        brief: default_brief(),
        audex_guidance: None,
        agents_evidence: None,
    }
}

#[test]
fn confirmation_precedes_snapshot_run_and_provider_activity() -> anyhow::Result<()> {
    run_test_with_large_stack("tui-audit-confirmation", || async {
        confirmation_scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}

async fn confirmation_scenario() -> Result<()> {
    let provider = responses::start_mock_server().await;
    let (_home, _config, session, thread) = boxed_future!(fixture(&provider)).await?;
    let (tx, mut events) = mpsc::unbounded_channel();
    let audits = AuditClient::new(session.request_handle());
    let task = tokio::spawn(execute(
        session.request_handle(),
        thread,
        AuditAction::Start(arguments()),
        AppEventSender::new(tx),
    ));
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await?
        .unwrap();
    let AppEvent::AuditConfirm {
        params, decision, ..
    } = event
    else {
        panic!("expected confirmation")
    };
    assert_eq!(params.model, "audit-test");
    assert_eq!(params.provider, "mock_provider");
    assert!(
        audits
            .list(AuditListParams {
                after: None,
                limit: None
            })
            .await?
            .runs
            .is_empty()
    );
    assert!(provider.received_requests().await.unwrap().is_empty());
    decision.send(false).unwrap();
    assert!(task.await?.is_err());
    assert!(
        audits
            .list(AuditListParams {
                after: None,
                limit: None
            })
            .await?
            .runs
            .is_empty()
    );
    assert!(provider.received_requests().await.unwrap().is_empty());
    session.shutdown().await?;
    Ok(())
}

#[test]
fn explicit_audit_survives_frontend_observer_drop_and_cancels_only_on_request() -> anyhow::Result<()>
{
    run_test_with_large_stack("tui-audit-disconnect", || async {
        disconnect_scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}

async fn disconnect_scenario() -> Result<()> {
    let provider = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![responses::ev_completed("delayed")]))
                .set_delay(Duration::from_secs(60)),
        )
        .mount(&provider)
        .await;
    let (_home, config, session, thread) = boxed_future!(fixture(&provider)).await?;
    let action = parse_command(
        "--snippet 'fn main() {}' --name example.rs --model audit-test --provider mock_provider",
    )
    .unwrap()
    .action(config.cwd.as_path())
    .await
    .map_err(color_eyre::eyre::Report::msg)?;
    let (tx, mut events) = mpsc::unbounded_channel();
    let events_tx = AppEventSender::new(tx);
    let task = tokio::spawn(execute(
        session.request_handle(),
        thread,
        action,
        events_tx.clone(),
    ));
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await?
        .unwrap();
    let AppEvent::AuditConfirm { decision, .. } = event else {
        panic!("expected confirmation")
    };
    decision.send(true).unwrap();
    let started = task.await?.map_err(color_eyre::eyre::Report::msg)?;
    let id = started["runId"].as_str().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(15), async {
        while provider.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let audits = AuditClient::new(session.request_handle());
    let before = audits.read(id.clone()).await?;
    let observer = tokio::spawn(observe(
        session.request_handle(),
        thread,
        before,
        events_tx.clone(),
        Arc::new(tokio::sync::Notify::new()),
    ));
    observer.abort();
    let _ = observer.await;
    assert!(!audits.read(id.clone()).await?.cancellation_requested);
    let cancelled = execute(
        session.request_handle(),
        thread,
        AuditAction::Cancel(id.clone()),
        events_tx,
    )
    .await
    .map_err(color_eyre::eyre::Report::msg)?;
    assert_eq!(cancelled["cancellationRequested"], true);
    assert!(
        audits.read(id).await?.cancellation_requested,
        "intent survives the frontend's cancellation response"
    );
    assert_eq!(
        provider.received_requests().await.unwrap().len(),
        1,
        "no automatic inference retry"
    );
    session.shutdown().await?;
    Ok(())
}

fn tool_name(value: &Value) -> Option<(String, String)> {
    value.as_array()?.iter().find_map(|namespace| {
        let name = namespace["tools"]
            .as_array()?
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .find(|name| name.ends_with("audit_start"))?;
        Some((namespace["name"].as_str()?.to_owned(), name.to_owned()))
    })
}

async fn mount_provider(provider: &MockServer, source: std::path::PathBuf) {
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).expect("provider JSON");
        let text = body.to_string();
        let tools = body["tools"].to_string();
        let events = if tools.contains("audex_source") {
            assert!(!text.contains("OPERATOR_ONLY_INSTRUCTION"));
            assert!(!tools.contains("mcp__"));
            assert_eq!(body["tools"].as_array().expect("tools").len(), 1);
            assert_eq!(body["tools"][0]["tools"].as_array().expect("source tools").len(), 3);
            let input = body["input"].as_array().expect("input");
            let assignment = input.iter().filter_map(|item| item["content"].as_array()).flatten()
                .filter_map(|part| part["text"].as_str()).filter_map(|text| serde_json::from_str::<Value>(text).ok())
                .find(|value| value.get("assignment_id").is_some()).expect("assignment");
            if input.iter().any(|item| item["type"] == "function_call_output") {
                assert!(text.contains("fn main()"));
                let result = json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],
                    "summary":"Retained audit summary","findings":[{"severity":"low","confidence":"high","title":"Fixture finding",
                    "mechanism":"Fixture mechanism","impact":"Fixture impact","evidence":[{"description":"Fixture evidence","path":"example.rs","start_byte":0,"end_byte":2,"start_line":1,"end_line":1}],"remediation":"Fixture proposal"}]});
                vec![responses::ev_assistant_message("result", &result.to_string()), responses::ev_completed("review-final")]
            } else {
                vec![responses::ev_function_call_with_namespace("source-read", "audex_source", "read", r#"{"path":"example.rs","offset":0,"max_bytes":64}"#), responses::ev_completed("review-read")]
            }
        } else if text.contains("Explain the fixture finding") {
            assert!(text.contains("Retained audit summary"));
            assert!(text.contains("Fixture finding"));
            assert!(text.contains("audex_audit_result"));
            vec![responses::ev_assistant_message("discussion", "The finding cites the immutable snapshot; a correction can be proposed in this operator conversation."), responses::ev_completed("discussed")]
        } else if text.contains("function_call_output") {
            vec![responses::ev_assistant_message("submitted", "The audit is running independently; this conversation is still available."), responses::ev_completed("operator-continued")]
        } else {
            assert!(text.contains("OPERATOR_ONLY_INSTRUCTION"));
            let (namespace, tool) = tool_name(&body["tools"]).expect("audit_start is model-visible through the existing MCP bridge");
            let args = json!({"source":{"type":"file","name":"example.rs","path":source}}).to_string();
            vec![responses::ev_function_call_with_namespace("start-audit", &namespace, &tool, &args), responses::ev_completed("audit-proposal")]
        };
        ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(responses::sse(events))
    }).mount(provider).await;
}

async fn turn(
    session: &mut AppServerSession,
    thread: ThreadId,
    text: &str,
    config: &Config,
    context: Option<HashMap<String, AdditionalContextEntry>>,
) -> Result<()> {
    session
        .turn_start(
            thread,
            "user",
            Uuid::new_v4().to_string(),
            vec![UserInput::Text {
                text: text.to_owned(),
                text_elements: Vec::new(),
            }],
            context,
            config.cwd.to_path_buf(),
            None,
            None,
            TurnPermissionsOverride::Preserve,
            &config.workspace_roots,
            config.model.clone().unwrap(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
    Ok(())
}

#[test]
fn report_retention_does_not_advance_a_running_operator_turn() -> anyhow::Result<()> {
    run_test_with_large_stack("tui-audit-active-context", || async {
        active_context_scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}

async fn active_context_scenario() -> Result<()> {
    let provider = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_assistant_message("answer", "Finished the operator's request"),
                    responses::ev_completed("operator-done"),
                ]))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&provider)
        .await;
    let (_home, config, mut session, thread) = boxed_future!(fixture(&provider)).await?;
    turn(
        &mut session,
        thread,
        "Explain this code without starting an audit",
        &config,
        None,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    let view = AuditRunView {
        run_id: "retained-run".into(),
        snapshot_id: "snapshot".into(),
        assignment_id: "assignment".into(),
        phase: AuditRunPhase::Completed,
        operation_state: codex_app_server_protocol::AuditOperationState::Committed,
        observation_pending: false,
        capacity_released: false,
        revision: 1,
        cancellation_requested: false,
        outcome: None,
        raw_result: None,
        report: Some("# Audit report\nA completed audit's retained findings".into()),
    };
    let (tx, mut events) = mpsc::unbounded_channel();
    observe(
        session.request_handle(),
        thread,
        view,
        AppEventSender::new(tx),
        Arc::new(tokio::sync::Notify::new()),
    )
    .await;
    let AppEvent::AuditUpdated { run, .. } = events.recv().await.unwrap() else {
        panic!("expected report delivery")
    };
    let mut contexts = ReportContexts::default();
    contexts.stage(thread, &run);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = session.next_event().await {
            if matches!(event.event, AppServerEvent::ServerNotification(ref n) if matches!(n.as_ref(), ServerNotification::TurnCompleted(c) if c.thread_id == thread.to_string())) { break; }
        }
    }).await?;
    let requests = provider.received_requests().await.unwrap().len();
    assert_eq!(
        requests, 1,
        "receiving a completed audit report must not cause an unsolicited operator inference step"
    );
    let first = provider.received_requests().await.unwrap();
    assert!(!String::from_utf8_lossy(&first[0].body).contains("audex_audit_result"));
    turn(
        &mut session,
        thread,
        "Now discuss the retained report",
        &config,
        contexts.for_turn(thread),
    )
    .await?;
    contexts.acknowledged(thread);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = session.next_event().await {
            if matches!(event.event, AppServerEvent::ServerNotification(ref n) if matches!(n.as_ref(), ServerNotification::TurnCompleted(c) if c.thread_id == thread.to_string())) { break; }
        }
    }).await?;
    let requests = provider.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "only the next explicit message starts report discussion"
    );
    assert!(String::from_utf8_lossy(&requests[1].body).contains("audex_audit_result"));
    assert!(contexts.for_turn(thread).is_none());
    session.shutdown().await?;
    Ok(())
}

#[test]
fn natural_language_bridge_keeps_operator_conversation_and_retains_report_after_restart()
-> anyhow::Result<()> {
    run_test_with_large_stack("tui-audit-conversation", || async {
        conversational_scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}

async fn conversational_scenario() -> Result<()> {
    let provider = responses::start_mock_server().await;
    let (home, config, mut session, _initial_thread) = boxed_future!(fixture(&provider)).await?;
    let source = home.0.path().join("example.rs");
    std::fs::write(&source, "fn main() {}\n")?;
    mount_provider(&provider, source.clone()).await;
    let (tx, mut events) = mpsc::unbounded_channel();
    let events_tx = AppEventSender::new(tx);
    let bridge = DynamicToolMcpServer::start(
        session.request_handle(),
        Default::default(),
        config.features.get().clone(),
        events_tx.clone(),
        tokio::sync::broadcast::channel(16).0,
        None,
        ToolServices {
            task_tools: true,
            audits: true,
            worktrees: None,
        },
    )
    .await?;
    session = session.with_thread_tool_transport(ThreadToolTransport::Mcp(Arc::new(bridge)));
    // A fresh operator root receives the ordinary tool set plus the existing MCP bridge.
    let thread = boxed_future!(session.start_thread(&config))
        .await?
        .session
        .thread_id;
    turn(
        &mut session,
        thread,
        "Please audit this file",
        &config,
        None,
    )
    .await?;
    let (view, operator_done) = tokio::time::timeout(Duration::from_secs(30), async {
        let mut done = false;
        loop {
            tokio::select! {
                event = events.recv() => match event.unwrap() {
                    AppEvent::AuditConfirm { thread_id, params, decision } => {
                        assert_eq!(thread_id, thread);
                        assert_eq!(params.provider, "mock_provider");
                        decision.send(true).unwrap();
                    }
                    AppEvent::AuditOpened { run: view, .. } => {
                        tokio::spawn(observe(session.request_handle(), thread, view.clone(), events_tx.clone(), Arc::new(tokio::sync::Notify::new())));
                    }
                    AppEvent::AuditUpdated { run: view, error, .. } => {
                        assert!(error.is_none(), "{error:?}");
                        if view.report.is_some() { return (view, done); }
                    }
                    _ => {},
                },
                event = session.next_event() => if let Some(event) = event
                    && let AppServerEvent::ServerNotification(notification) = event.event
                    && let ServerNotification::TurnCompleted(completed) = *notification
                    && completed.thread_id == thread.to_string() {
                    done = true;
                },
            }
            // UI events and app-server notifications are independent queues; observing
            // operator completion does not imply AuditOpened has been dequeued yet.
            if done {
                let page = AuditClient::new(session.request_handle()).list(AuditListParams { after: None, limit: Some(1) }).await.unwrap();
                assert!(!page.runs.is_empty(), "operator completion must follow durable submission");
            }
        }
    }).await?;
    assert_eq!(view.phase, AuditRunPhase::Completed);
    assert!(view.report.as_ref().unwrap().contains("Fixture finding"));
    assert_eq!(std::fs::read_to_string(&source)?, "fn main() {}\n");
    if !operator_done {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = session.next_event().await {
                if matches!(event.event, AppServerEvent::ServerNotification(ref n) if matches!(n.as_ref(), ServerNotification::TurnCompleted(c) if c.thread_id == thread.to_string())) { break; }
            }
        }).await?;
    }
    assert_eq!(
        provider.received_requests().await.unwrap().len(),
        4,
        "two operator requests and one reviewer tool roundtrip; retaining a report starts no inference"
    );
    session.shutdown().await?;
    let mut reopened = boxed_future!(crate::start_embedded_app_server_for_picker(&config)).await?;
    let settings = crate::local_settings::LocalSettings::from(&config);
    boxed_future!(reopened.resume_thread(
        &settings,
        config.clone(),
        thread,
        ResumeModelSettings::RestoreFromThread
    ))
    .await?;
    let retained = AuditClient::new(reopened.request_handle())
        .read(view.run_id.clone())
        .await?;
    assert_eq!(retained.report, view.report);
    assert_eq!(provider.received_requests().await.unwrap().len(), 4);
    // No operator message was sent before exit. /audit read reloads the durable
    // report, and only the next deliberate turn persists it as untrusted context.
    let action = parse_command(&format!("read {}", view.run_id))
        .unwrap()
        .action(config.cwd.as_path())
        .await
        .map_err(color_eyre::eyre::Report::msg)?;
    execute(reopened.request_handle(), thread, action, events_tx.clone())
        .await
        .map_err(color_eyre::eyre::Report::msg)?;
    let AppEvent::AuditOpened { run: loaded, .. } = events.recv().await.unwrap() else {
        panic!("expected reopened audit")
    };
    let mut contexts = ReportContexts::default();
    contexts.stage(thread, &loaded);
    turn(
        &mut reopened,
        thread,
        "Explain the fixture finding and propose a correction",
        &config,
        contexts.for_turn(thread),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = reopened.next_event().await {
            if matches!(event.event, AppServerEvent::ServerNotification(ref n) if matches!(n.as_ref(), ServerNotification::TurnCompleted(c) if c.thread_id == thread.to_string())) { break; }
        }
    }).await?;
    assert_eq!(
        provider.received_requests().await.unwrap().len(),
        5,
        "discussion is a separate operator request, not another audit sample"
    );
    contexts.acknowledged(thread);
    reopened.shutdown().await?;
    let mut resumed = boxed_future!(crate::start_embedded_app_server_for_picker(&config)).await?;
    boxed_future!(resumed.resume_thread(
        &settings,
        config.clone(),
        thread,
        ResumeModelSettings::RestoreFromThread
    ))
    .await?;
    assert_eq!(provider.received_requests().await.unwrap().len(), 5);
    // The report is now ordinary retained Codex context, without another read or attachment.
    turn(
        &mut resumed,
        thread,
        "Explain the fixture finding again",
        &config,
        None,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = resumed.next_event().await {
            if matches!(event.event, AppServerEvent::ServerNotification(ref n) if matches!(n.as_ref(), ServerNotification::TurnCompleted(c) if c.thread_id == thread.to_string())) { break; }
        }
    }).await?;
    assert_eq!(provider.received_requests().await.unwrap().len(), 6);
    resumed.shutdown().await?;
    Ok(())
}

#[test]
fn pending_observer_recovers_and_delivers_report_once_without_another_inference()
-> anyhow::Result<()> {
    run_test_with_large_stack("tui-audit-pending-watch", || async {
        let provider = responses::start_mock_server().await;
        let (home, _config, session, thread) = boxed_future!(fixture(&provider))
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        mount_provider(&provider, home.0.path().join("example.rs")).await;
        let audits = AuditClient::new(session.request_handle());
        let mut initial = audits
            .start(AuditStartParams {
                request_key: "pending-watcher".into(),
                source: arguments().source,
                brief: default_brief(),
                model: "audit-test".into(),
                provider: "mock_provider".into(),
                reasoning_effort: None,
                audex_guidance: None,
                agents_evidence: None,
            })
            .await?;
        let complete = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let view = audits.read(initial.run_id.clone()).await?;
                if terminal(&view) {
                    break Ok::<_, codex_app_server_client::audit::AuditClientError>(view);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        assert!(complete.report.is_some());
        // The frontend's last durable read was pending. Recovering on the daemon
        // before its next notification/read must not abandon the watch.
        initial.phase = AuditRunPhase::Reviewing;
        initial.operation_state = codex_app_server_protocol::AuditOperationState::Accepted;
        initial.revision = complete.revision - 1;
        initial.observation_pending = true;
        let requests = provider.received_requests().await.expect("requests").len();
        let (tx, mut events) = mpsc::unbounded_channel();
        let wake = Arc::new(tokio::sync::Notify::new());
        wake.notify_one();
        let watch = tokio::spawn(observe(
            session.request_handle(),
            thread,
            initial,
            AppEventSender::new(tx),
            wake.clone(),
        ));
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await?
            .expect("pending watcher must remain active");
        let AppEvent::AuditUpdated { run, error, .. } = event else {
            panic!("expected report update")
        };
        assert!(error.is_none());
        assert_eq!(run.report, complete.report);
        assert!(!run.observation_pending);
        watch.await?;
        wake.notify_one();
        assert!(
            events.recv().await.is_none(),
            "completed watcher sends its report only once"
        );
        assert_eq!(
            provider.received_requests().await.expect("requests").len(),
            requests
        );
        session.shutdown().await?;
        Ok(())
    })
}
