//! Real app-server/Core/owned-client boundaries with local deterministic providers only.
use app_test_support::{MockResponsesConfig, TestAppServer};
use codex_app_server_protocol::*;
use core_test_support::responses;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

struct Fixture {
    home: TempDir,
    runtime: TempDir,
    client: PathBuf,
    host_home: PathBuf,
}

impl Fixture {
    fn new(server: &str) -> anyhow::Result<Self> {
        let home = tempfile::tempdir()?;
        let client = home.path().join("client");
        let host_home = home.path().join("host-home");
        let profile = home.path().join("profile");
        let runtime = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(3)
            .tempdir_in(std::env::var("AUDEX_CLIENT_TEST_RUNTIME_ROOT")?)?;
        for path in [&client, &profile, &host_home] {
            std::fs::create_dir(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))?;
        let executable = client.join("claude");
        std::fs::write(&executable, include_str!("claude_fixture.py"))?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(client.join("mode"), "normal")?;
        std::fs::write(
            profile.join("settings.json"),
            json!({
                "disableAllHooks":true,"disableClaudeAiConnectors":true,"syncClaudeAiPlugins":false,
                "syncClaudeAiSkills":false,"autoMemoryEnabled":false,"crossSessionInbound":"refuse",
                "permissions":{"deny":["SendMessage","ListAgents"]}
            })
            .to_string(),
        )?;
        std::fs::create_dir(home.path().join("audex"))?;
        std::fs::write(home.path().join("audex/reviewer-backends.json"), json!({
            "version":1, "claudeSubscription":{"executable":executable,"clientVersion":"2.1.319",
                "profile":profile,"runtimeRoot":runtime.path()}
        }).to_string())?;
        MockResponsesConfig::new(server)
            .with_root_config("developer_instructions = \"HIDDEN_DEVELOPER_SENTINEL\"")
            .write(home.path())?;
        std::fs::write(home.path().join("AGENTS.md"), "PRIVATE_WORKSPACE_SENTINEL")?;
        Ok(Self {
            home,
            runtime,
            client,
            host_home,
        })
    }

    async fn server(&self) -> anyhow::Result<TestAppServer> {
        TestAppServer::builder()
            .with_codex_home(self.home.path())
            .with_env_overrides(&[("HOME", self.host_home.to_str())])
            .build_initialized()
            .await
    }

    fn prompts(&self) -> Vec<Value> {
        std::fs::read_to_string(self.client.join("prompts.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn wait_prompt(&self, count: usize) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.prompts().len() < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        Ok(())
    }

    async fn wait_cleanup(&self) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while std::fs::read_dir(self.runtime.path())
                .unwrap()
                .next()
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        Ok(())
    }
}

fn input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.into(),
        text_elements: vec![],
    }]
}

async fn claude(
    client: &mut TestAppServer,
    thread: &str,
    model: Option<&str>,
    message: &str,
) -> anyhow::Result<ThreadClaudeResponse> {
    client
        .request(|id| ClientRequest::ThreadClaude {
            request_id: id,
            params: ThreadClaudeParams {
                thread_id: thread.into(),
                message: message.into(),
                model: model.map(str::to_owned),
            },
        })
        .await
}

async fn completed(
    client: &mut TestAppServer,
    turn: &str,
) -> anyhow::Result<TurnCompletedNotification> {
    let notification = tokio::time::timeout(
        Duration::from_secs(20),
        client.read_stream_until_matching_notification("Claude turn completed", |n| {
            n.method == "turn/completed"
                && n.params.as_ref().is_some_and(|p| p["turn"]["id"] == turn)
        }),
    )
    .await??;
    Ok(serde_json::from_value(notification.params.unwrap())?)
}

async fn read(client: &mut TestAppServer, thread: &str) -> anyhow::Result<ThreadReadResponse> {
    client
        .request(|id| ClientRequest::ThreadRead {
            request_id: id,
            params: ThreadReadParams {
                thread_id: thread.into(),
                include_turns: true,
            },
        })
        .await
}

#[tokio::test]
async fn claude_conversation_rate_limit_status_keeps_one_complete_answer() -> anyhow::Result<()> {
    let mock = responses::start_mock_server().await;
    let f = Fixture::new(&mock.uri())?;
    std::fs::write(f.client.join("mode"), "rate_limit")?;
    let mut client = f.server().await?;
    let thread = client
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let c = claude(&mut client, &thread, None, "status before answer").await?;
    let done = completed(&mut client, &c.turn_id).await?;
    assert_eq!(done.turn.status, TurnStatus::Completed, "{done:?}");
    let history = read(&mut client, &thread).await?;
    let answers: Vec<_> = history.thread.turns[0]
        .items
        .iter()
        .filter_map(|item| {
            if let ThreadItemPayload::AgentMessage { text, .. } = &item.payload {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        answers,
        ["**Claude (claude-haiku-5-5)**\n\nClaude fixture answer: status before answer"]
    );
    assert_eq!(f.prompts().len(), 1);
    assert!(mock.received_requests().await.unwrap().is_empty());
    let audits: AuditListResponse = client
        .request(|id| ClientRequest::AuditList {
            request_id: id,
            params: serde_json::from_value(json!({})).unwrap(),
        })
        .await?;
    assert!(
        serde_json::to_value(audits)?["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.shutdown_gracefully().await?;
    f.wait_cleanup().await?;
    Ok(())
}

#[test_case::test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case::test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn claude_conversation_text_blocks_survive_delivery_restart_and_followups(
    history_mode: ThreadHistoryMode,
) -> anyhow::Result<()> {
    const FULL: &str = "First public block. Second public block.";
    const ATTRIBUTED: &str =
        "**Claude (claude-haiku-5-5)**\n\nFirst public block. Second public block.";
    let mock = responses::start_mock_server().await;
    let f = Fixture::new(&mock.uri())?;
    std::fs::write(f.client.join("mode"), "text_blocks")?;
    let mut client = f.server().await?;
    let thread = client
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread
        .id;
    let c = claude(&mut client, &thread, None, "two public blocks").await?;
    let notification = client
        .read_stream_until_matching_notification("complete Claude text", |n| {
            n.method == "item/completed"
                && n.params.as_ref().is_some_and(|p| {
                    p["turnId"] == c.turn_id && p["item"]["type"] == "agentMessage"
                })
        })
        .await?;
    assert_eq!(notification.params.unwrap()["item"]["text"], ATTRIBUTED);
    assert_eq!(
        completed(&mut client, &c.turn_id).await?.turn.status,
        TurnStatus::Completed
    );
    assert_eq!(f.prompts().len(), 1);
    assert!(mock.received_requests().await.unwrap().is_empty());
    client.shutdown_gracefully().await?;

    let mut client = f.server().await?;
    let _: ThreadResumeResponse = client
        .request(|id| ClientRequest::ThreadResume {
            request_id: id,
            params: ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    let retained = read(&mut client, &thread).await?;
    let answers: Vec<_> = retained.thread.turns[0]
        .items
        .iter()
        .filter_map(|item| {
            if let ThreadItemPayload::AgentMessage { text, .. } = &item.payload {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(answers, [ATTRIBUTED]);
    assert!(!serde_json::to_string(&retained)?.contains("PRIVATE_THINKING_SENTINEL"));
    let next = responses::mount_sse_once(
        &mock,
        responses::sse(vec![
            responses::ev_assistant_message("next", "Codex considers both blocks"),
            responses::ev_completed("next"),
        ]),
    )
    .await;
    let done = client
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.clone(),
            input: input("consider both Claude blocks"),
            ..Default::default()
        })
        .await?;
    assert_eq!(done.turn.status, TurnStatus::Completed);
    let native_input = next.single_request().body_json().to_string();
    assert!(native_input.contains(FULL));
    assert!(!native_input.contains("PRIVATE_THINKING_SENTINEL"));
    std::fs::write(f.client.join("mode"), "normal")?;
    let c = claude(&mut client, &thread, None, "respond to Codex").await?;
    assert_eq!(
        completed(&mut client, &c.turn_id).await?.turn.status,
        TurnStatus::Completed
    );
    let prompts = f.prompts();
    assert_eq!(prompts.len(), 2);
    let discussion = prompts[1]["prompt"]["discussion"].as_array().unwrap();
    assert!(discussion.iter().any(|entry| entry["text"] == ATTRIBUTED));
    let sent = prompts[1]["prompt"].to_string();
    assert!(sent.contains("Codex considers both blocks"));
    assert!(!sent.contains("PRIVATE_THINKING_SENTINEL"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
    let audits: AuditListResponse = client
        .request(|id| ClientRequest::AuditList {
            request_id: id,
            params: serde_json::from_value(json!({})).unwrap(),
        })
        .await?;
    assert!(
        serde_json::to_value(audits)?["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.shutdown_gracefully().await?;
    f.wait_cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn claude_conversation_text_blocks_reject_terminal_mismatch_without_replay()
-> anyhow::Result<()> {
    let mock = responses::start_mock_server().await;
    let f = Fixture::new(&mock.uri())?;
    std::fs::write(f.client.join("mode"), "text_mismatch")?;
    let mut client = f.server().await?;
    let thread = client
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let c = claude(&mut client, &thread, None, "mismatched completion").await?;
    let done = completed(&mut client, &c.turn_id).await?;
    assert_eq!(done.turn.status, TurnStatus::Failed);
    assert!(
        done.turn
            .error
            .unwrap()
            .message
            .contains("terminal text does not match")
    );
    let retained = read(&mut client, &thread).await?;
    assert!(
        retained.thread.turns[0]
            .items
            .iter()
            .all(|item| !matches!(item.payload, ThreadItemPayload::AgentMessage { .. }))
    );
    assert_eq!(f.prompts().len(), 1);
    assert!(mock.received_requests().await.unwrap().is_empty());
    client.shutdown_gracefully().await?;
    f.wait_cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn claude_conversation_routes_context_persistence_and_no_audit() -> anyhow::Result<()> {
    let mock = responses::start_mock_server().await;
    let first = responses::mount_sse_once(
        &mock,
        responses::sse(vec![
            responses::ev_assistant_message("codex-first", "Codex first argument"),
            responses::ev_completed("first"),
        ]),
    )
    .await;
    let f = Fixture::new(&mock.uri())?;
    let mut client = f.server().await?;
    let thread = client
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let turn = client
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.clone(),
            input: input("ordinary question"),
            ..Default::default()
        })
        .await?;
    assert_eq!(turn.turn.status, TurnStatus::Completed);
    assert_eq!(first.requests().len(), 1);
    let native_context = first.single_request().body_json().to_string();
    assert!(native_context.contains("HIDDEN_DEVELOPER_SENTINEL"));
    assert!(native_context.contains("PRIVATE_WORKSPACE_SENTINEL"));
    assert!(f.prompts().is_empty());
    let c = claude(&mut client, &thread, None, "challenge the first argument").await?;
    assert_eq!(c.model, "claude-haiku-5-5");
    assert_eq!(
        completed(&mut client, &c.turn_id).await?.turn.status,
        TurnStatus::Completed
    );
    let history = serde_json::to_value(read(&mut client, &thread).await?)?.to_string();
    assert!(history.contains("Claude (claude-haiku-5-5)"), "{history}");
    assert!(history.contains("Claude fixture answer"));
    let prompts = f.prompts();
    assert_eq!(prompts.len(), 1);
    let sent = prompts[0]["prompt"].to_string();
    assert!(
        sent.contains("ordinary question") && sent.contains("Codex first argument"),
        "{sent}"
    );
    assert!(
        !sent.contains("HIDDEN_DEVELOPER_SENTINEL") && !sent.contains("PRIVATE_WORKSPACE_SENTINEL")
    );
    let next = responses::mount_sse_once(
        &mock,
        responses::sse(vec![
            responses::ev_assistant_message("codex-next", "Codex subsequent argument"),
            responses::ev_completed("next"),
        ]),
    )
    .await;
    let turn = client
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.clone(),
            input: input("consider Claude's response"),
            ..Default::default()
        })
        .await?;
    assert_eq!(turn.turn.status, TurnStatus::Completed);
    assert!(
        next.single_request()
            .body_json()
            .to_string()
            .contains("Claude fixture answer")
    );
    // Cold reopen uses the same durable Codex thread; it cannot start a Claude invocation.
    client.shutdown_gracefully().await?;
    let mut client = f.server().await?;
    let _: ThreadResumeResponse = client
        .request(|id| ClientRequest::ThreadResume {
            request_id: id,
            params: ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(f.prompts().len(), 1);
    let c = claude(
        &mut client,
        &thread,
        Some("claude-sonnet-5-5"),
        "respond to Codex's subsequent argument",
    )
    .await?;
    assert_eq!(c.model, "claude-sonnet-5-5");
    assert_eq!(
        completed(&mut client, &c.turn_id).await?.turn.status,
        TurnStatus::Completed
    );
    let prompts = f.prompts();
    assert_eq!(prompts.len(), 2);
    let sent = prompts[1]["prompt"].to_string();
    for expected in [
        "Codex first argument",
        "Claude fixture answer",
        "Codex subsequent argument",
    ] {
        assert!(sent.contains(expected), "missing {expected}: {sent}");
    }
    let history = read(&mut client, &thread).await?;
    assert_eq!(history.thread.turns.len(), 4);
    assert_eq!(first.requests().len(), 1);
    assert_eq!(next.requests().len(), 1);
    let audits: AuditListResponse = client
        .request(|id| ClientRequest::AuditList {
            request_id: id,
            params: serde_json::from_value(json!({})).unwrap(),
        })
        .await?;
    assert!(
        serde_json::to_value(audits)?["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.shutdown_gracefully().await?;
    f.wait_cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn claude_conversation_errors_cancellation_and_no_replay() -> anyhow::Result<()> {
    let mock = responses::start_mock_server().await;
    let f = Fixture::new(&mock.uri())?;
    let mut client = f.server().await?;
    let thread = client
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    for (index, mode) in ["wrong_model", "tools", "error", "eof", "incomplete"]
        .iter()
        .enumerate()
    {
        std::fs::write(f.client.join("mode"), mode)?;
        let c = claude(&mut client, &thread, None, "a controlled failure").await?;
        let done = completed(&mut client, &c.turn_id).await?;
        assert_eq!(done.turn.status, TurnStatus::Failed, "{mode}: {done:?}");
        assert!(done.turn.error.is_some());
        assert_eq!(f.prompts().len(), index + 1);
        f.wait_cleanup().await?;
    }
    std::fs::write(f.client.join("mode"), "held")?;
    let c = claude(&mut client, &thread, None, "cancel this response").await?;
    f.wait_prompt(6).await?;
    let request = client
        .send_request(
            "thread/claude",
            Some(json!({
                "threadId":thread, "message":"must not launch concurrently"
            })),
        )
        .await?;
    let rejected = client
        .read_stream_until_error_message(RequestId::Integer(request))
        .await?;
    assert!(rejected.error.message.contains("idle thread"));
    client
        .interrupt_turn_and_wait_for_aborted(thread.clone(), c.turn_id, Duration::from_secs(10))
        .await?;
    f.wait_cleanup().await?;
    assert_eq!(f.prompts().len(), 6);
    client.shutdown_gracefully().await?;
    let mut client = f.server().await?;
    let _: ThreadResumeResponse = client
        .request(|id| ClientRequest::ThreadResume {
            request_id: id,
            params: ThreadResumeParams {
                thread_id: thread,
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(f.prompts().len(), 6);
    assert!(mock.received_requests().await.unwrap().is_empty());
    client.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn claude_conversation_reconnect_legacy_history_and_configured_model() -> anyhow::Result<()> {
    use super::connection_handling_websocket::*;
    let mock = responses::start_mock_server().await;
    let native = responses::mount_sse_once(
        &mock,
        responses::sse(vec![
            responses::ev_assistant_message("legacy-codex", "Codex legacy argument"),
            responses::ev_completed("legacy-codex-turn"),
        ]),
    )
    .await;
    let f = Fixture::new(&mock.uri())?;
    let path = f.home.path().join("audex/reviewer-backends.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    settings["claudeConversationModel"] = json!("claude-sonnet-5-5");
    std::fs::write(path, settings.to_string())?;
    std::fs::write(f.client.join("mode"), "held")?;
    let (mut process, address) = spawn_websocket_server_with_env(
        f.home.path(),
        "ws://127.0.0.1:0",
        &[],
        &[("HOME", f.host_home.to_str().unwrap())],
    )
    .await?;
    let mut ws = connect_websocket(address).await?;
    send_request(&mut ws, "initialize", 1, Some(json!({"clientInfo":{"name":"m3-fixture","version":"1"},"capabilities":{"experimentalApi":true}}))).await?;
    read_response_for_id(&mut ws, 1).await?;
    send_request(
        &mut ws,
        "thread/start",
        2,
        Some(json!({"historyMode":"legacy"})),
    )
    .await?;
    let thread = read_response_for_id(&mut ws, 2).await?.result["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send_request(&mut ws, "turn/start", 20, Some(json!({
        "threadId":thread, "input":[{"type":"text","text":"ordinary legacy question","text_elements":[]}]
    }))).await?;
    let (_, native_done) =
        read_response_and_notification_for_method(&mut ws, 20, "turn/completed").await?;
    assert_eq!(native_done.params.unwrap()["turn"]["status"], "completed");
    let native_context = native.single_request().body_json().to_string();
    assert!(native_context.contains("HIDDEN_DEVELOPER_SENTINEL"));
    assert!(native_context.contains("PRIVATE_WORKSPACE_SENTINEL"));
    send_request(
        &mut ws,
        "thread/claude",
        3,
        Some(json!({"threadId":thread,"message":"held across disconnect"})),
    )
    .await?;
    let c = read_response_for_id(&mut ws, 3).await?.result;
    assert_eq!(c["model"], "claude-sonnet-5-5");
    f.wait_prompt(1).await?;
    let sent = f.prompts()[0]["prompt"].to_string();
    assert!(sent.contains("ordinary legacy question") && sent.contains("Codex legacy argument"));
    assert!(
        !sent.contains("HIDDEN_DEVELOPER_SENTINEL") && !sent.contains("PRIVATE_WORKSPACE_SENTINEL")
    );
    // A rejected request from another connection must not subscribe that observer.
    let mut observer = connect_websocket(address).await?;
    send_request(&mut observer, "initialize", 100, Some(json!({"clientInfo":{"name":"m3-observer","version":"1"},"capabilities":{"experimentalApi":true}}))).await?;
    read_response_for_id(&mut observer, 100).await?;
    send_request(
        &mut observer,
        "thread/claude",
        101,
        Some(json!({"threadId":thread,"message":"reject without subscribing"})),
    )
    .await?;
    let rejected = read_error_for_id(&mut observer, 101).await?;
    assert!(rejected.error.message.contains("idle thread"));
    send_request(
        &mut observer,
        "thread/unsubscribe",
        102,
        Some(json!({"threadId":thread})),
    )
    .await?;
    assert_eq!(
        read_response_for_id(&mut observer, 102).await?.result["status"],
        "notSubscribed"
    );
    observer.close(None).await?;
    assert_eq!(f.prompts().len(), 1);
    ws.close(None).await?;
    drop(ws);
    let mut ws = connect_websocket(address).await?;
    send_request(&mut ws, "initialize", 4, Some(json!({"clientInfo":{"name":"m3-reconnected","version":"1"},"capabilities":{"experimentalApi":true}}))).await?;
    read_response_for_id(&mut ws, 4).await?;
    send_request(
        &mut ws,
        "thread/resume",
        5,
        Some(json!({"threadId":thread})),
    )
    .await?;
    read_response_for_id(&mut ws, 5).await?;
    assert_eq!(f.prompts().len(), 1);
    std::fs::write(f.client.join("release"), "release")?;
    let done = read_notification_for_method(&mut ws, "turn/completed").await?;
    assert_eq!(done.params.unwrap()["turn"]["status"], "completed");
    send_request(
        &mut ws,
        "thread/claude",
        6,
        Some(json!({"threadId":thread,"message":"continue after reconnect"})),
    )
    .await?;
    read_response_for_id(&mut ws, 6).await?;
    read_notification_for_method(&mut ws, "turn/completed").await?;
    assert_eq!(f.prompts().len(), 2);
    assert!(
        f.prompts()[1]["prompt"]
            .to_string()
            .contains("Claude fixture answer: held across disconnect")
    );
    f.wait_cleanup().await?;
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
    // Abrupt daemon death kills the owned client. Resume must not resubmit its prompt.
    std::fs::remove_file(f.client.join("release"))?;
    send_request(
        &mut ws,
        "thread/claude",
        7,
        Some(json!({"threadId":thread,"message":"held across daemon death"})),
    )
    .await?;
    let interrupted = read_response_for_id(&mut ws, 7).await?.result["turnId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.wait_prompt(3).await?;
    let client_pid = f.prompts()[2]["pid"].as_u64().unwrap() as u32;
    process.kill().await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if codex_app_server_daemon::read_linux_process_identity(client_pid)
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let mut reopened = f.server().await?;
    let resumed: ThreadResumeResponse = reopened
        .request(|id| ClientRequest::ThreadResume {
            request_id: id,
            params: ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    let stopped = resumed
        .thread
        .turns
        .iter()
        .find(|turn| turn.id == interrupted)
        .unwrap();
    assert_eq!(stopped.status, TurnStatus::Interrupted);
    assert_eq!(f.prompts().len(), 3);
    reopened.shutdown_gracefully().await?;
    Ok(())
}
