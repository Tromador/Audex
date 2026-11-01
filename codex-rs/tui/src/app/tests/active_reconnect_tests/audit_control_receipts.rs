//! Residual F1: exercise the real reconnect channel swap and conversation reconstruction.
use super::*;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_client::{AppServerClient, CloudClient};
use pretty_assertions::assert_eq;

async fn receipt_survives_full_reconnect(
    outcome: &str,
    background: bool,
    displayed_before_disconnect: bool,
) -> Result<()> {
    let (mut app, mut events, mut ops) = make_test_app_with_channels().await;
    let origin = ThreadId::new();
    let displayed = if background { ThreadId::new() } else { origin };
    app.config.model = Some("gpt-test".into());
    app.local_settings.tui.show_server_version_notice = false;
    app.active_thread_id = Some(displayed);
    app.primary_thread_id = Some(displayed);
    let displayed_session = test_thread_session(displayed, app.config.cwd.to_path_buf());
    for id in [origin, displayed] {
        let cached = test_thread_session(id, app.config.cwd.to_path_buf());
        app.ensure_thread_channel(id)
            .store
            .lock()
            .await
            .set_session(cached, Vec::new());
    }
    app.chat_widget.handle_thread_session(displayed_session);
    // Initial session setup is unrelated to the receipt, and has not run any client requests.
    while events.try_recv().is_ok() {}
    while let Ok(op) = ops.try_recv() {
        assert!(matches!(op, AppCommand::ListSkills { .. }));
    }
    let (cloud, mut old_commands, _incoming) = CloudClient::channel(8);
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(cloud),
        ThreadParamsMode::Cloud,
    );
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let detail = match outcome {
        "success" => "S4_RECONNECT_RECORDED assignment-original action-key-original",
        "refusal" => {
            "S4_RECONNECT_REFUSED assignment-original action-key-original: collection ready"
        }
        "transport" => {
            "S4_RECONNECT_TRANSPORT_LOSS assignment-original action-key-original: acknowledgement uncertain"
        }
        _ => unreachable!(),
    };
    let receipt = AppEvent::AuditActivityControlFinished {
        viewer_id: uuid::Uuid::new_v4(),
        thread_id: origin,
        result: if outcome == "success" {
            Ok(detail.into())
        } else {
            Err(detail.into())
        },
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    app.app_server_target = AppServerTarget::Remote { endpoint };
    let cwd = app.config.cwd.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_reconnect_requests(tokio_tungstenite::accept_async(stream).await?, |request| {
            let id = request.params.as_ref().and_then(|params|params["threadId"].as_str()).unwrap_or("");
            let thread = json!({"id":id,"sessionId":id,"preview":"existing conversation","ephemeral":false,
                "modelProvider":"test-provider","createdAt":0,"updatedAt":0,"status":{"type":"idle"},
                "cwd":cwd,"cliVersion":"0.0.0","source":"cli","turns":[]});
            if matches!(request.method.as_str(), "thread/resume" | "thread/read") {
                let _: codex_app_server_protocol::Thread = serde_json::from_value(thread.clone()).expect("valid reconnect thread fixture");
            }
            std::future::ready(Some(match request.method.as_str() {
                "thread/resume" => json!({"result":{"thread":thread,"model":"gpt-test","modelProvider":"test-provider","cwd":cwd,
                    "approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"dangerFullAccess"},"reasoningEffort":null}}),
                "thread/read" => json!({"result":{"thread":thread}}),
                "thread/list" | "thread/loaded/list" => json!({"result":{"data":[],"nextCursor":null}}),
                "thread/goal/get" => json!({"result":{"goal":null}}),
                "plugin/installed" => json!({"result":{"marketplaces":[],"marketplaceLoadErrors":[]}}),
                method => panic!("unexpected reconnect request; no control/inference is permitted: {method}"),
            }))
        }).await
    });
    if displayed_before_disconnect {
        app.handle_event(&mut tui, &mut session, receipt).await?;
        let before = drain_history(&mut app, &mut tui, &mut session, &mut events).await?;
        assert_eq!(before.matches(detail).count(), 1);
        assert!(app.begin_reconnect());
    } else {
        assert!(app.begin_reconnect());
        app.handle_event(&mut tui, &mut session, receipt).await?;
        let offline = drain_history(&mut app, &mut tui, &mut session, &mut events).await?;
        assert_eq!(offline.matches(detail).count(), usize::from(!background));
    }
    assert!(app.reconnect.offline);
    eprintln!("F1 phase: reconnect ({outcome}, background={background})");
    let connected = tokio::time::timeout(
        Duration::from_secs(10),
        reconnect(
            session.backend_id(),
            app.app_server_target.clone(),
            app.config.clone(),
            app.local_settings.clone(),
            Some(displayed),
            None,
            session.thread_tool_transport(),
            ReconnectPresentation::Conversation,
        ),
    )
    .await
    .expect("bounded reconnect handshake")?;
    eprintln!("F1 phase: finish_reconnect");
    tokio::time::timeout(
        Duration::from_secs(10),
        app.finish_reconnect(
            &mut tui,
            &mut session,
            &mut events,
            connected,
            CODEX_CLI_VERSION,
        ),
    )
    .await
    .expect("bounded reconnect finish/rebuild")?;
    assert!(!app.reconnect.offline);
    assert_eq!(app.current_displayed_thread_id(), Some(displayed));
    let rebuilt = drain_history(&mut app, &mut tui, &mut session, &mut events).await?;
    assert_eq!(
        rebuilt.matches(detail).count(),
        usize::from(!background),
        "rebuilt origin attribution"
    );
    let retained = app
        .ensure_thread_channel(origin)
        .store
        .lock()
        .await
        .snapshot()
        .events;
    assert_eq!(
        retained
            .iter()
            .filter(|event| matches!(event, ThreadBufferedEvent::AuditControlOutcome(_)))
            .count(),
        1
    );
    if background {
        assert!(
            app.ensure_thread_channel(displayed)
                .store
                .lock()
                .await
                .snapshot()
                .events
                .iter()
                .all(|event| !matches!(event, ThreadBufferedEvent::AuditControlOutcome(_)))
        );
        eprintln!("F1 phase: select background origin");
        tokio::time::timeout(
            Duration::from_secs(10),
            app.select_agent_thread(&mut tui, &mut session, origin),
        )
        .await
        .expect("bounded origin selection")?;
        assert_eq!(app.current_displayed_thread_id(), Some(origin));
        assert_eq!(
            app.ensure_thread_channel(origin)
                .store
                .lock()
                .await
                .snapshot()
                .events
                .iter()
                .filter(|event| matches!(event, ThreadBufferedEvent::AuditControlOutcome(_)))
                .count(),
            1
        );
        let selected = drain_history(&mut app, &mut tui, &mut session, &mut events).await?;
        assert_eq!(
            selected.matches(detail).count(),
            1,
            "selected origin must show exact error/action details once"
        );
    }
    // No old-handle operations, local inference/control operations or new audit decisions.
    assert!(old_commands.try_recv().is_err());
    while let Ok(op) = ops.try_recv() {
        assert!(matches!(op, AppCommand::ListSkills { .. }));
    }
    eprintln!("F1 phase: shutdown fixture");
    tokio::time::timeout(Duration::from_secs(10), session.shutdown())
        .await
        .expect("bounded fixture client shutdown")?;
    let methods = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("bounded fixture server shutdown")??;
    // A background thread without a rollout path receives the existing settings refresh
    // after live attachment, in addition to the displayed resume and background attach.
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "thread/resume")
            .count(),
        if background { 3 } else { 1 }
    );
    assert!(!methods.iter().any(|method| method.starts_with("audit/")
        || matches!(
            method.as_str(),
            "turn/start" | "turn/steer" | "thread/start"
        )));
    Ok(())
}

#[tokio::test]
async fn full_reconnect_rebuild_keeps_displayed_control_receipts_exactly_once() -> Result<()> {
    for outcome in ["success", "refusal", "transport"] {
        for already_displayed in [false, true] {
            receipt_survives_full_reconnect(outcome, false, already_displayed).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn full_reconnect_keeps_background_control_receipts_until_conversation_selection()
-> Result<()> {
    for outcome in ["success", "refusal", "transport"] {
        receipt_survives_full_reconnect(outcome, true, false).await?;
    }
    Ok(())
}
