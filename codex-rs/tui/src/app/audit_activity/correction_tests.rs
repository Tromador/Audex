//! F1/F2 regressions over the actual app dispatcher and existing isolated typed-client fixture.
use super::*;
use crate::app::thread_events::{ThreadBufferedEvent, ThreadEventStore};
use codex_app_server_client::CloudCommand;

async fn open_confirmation(
    app: &mut App,
    tui: &mut tui::Tui,
    session: &mut AppServerSession,
    commands: &mut tokio::sync::mpsc::Receiver<CloudCommand>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    key(app, tui, session, KeyCode::Char('R')).await?;
    let capability = commands.recv().await.unwrap();
    assert!(
        matches!(capability.operation,CloudOperation::Request(r) if matches!(*r,ClientRequest::AuditCapabilities{..}))
    );
    capability
        .response
        .send(Ok(Ok(json!({"protocolVersion":AUDIT_PROTOCOL_VERSION}))))
        .unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if matches!(event, AppEvent::AuditAttemptConfirm { .. }) {
            app.handle_event(tui, session, event).await?;
            break;
        }
    }
    Ok(())
}
async fn accept(app: &mut App, tui: &mut tui::Tui, session: &mut AppServerSession) -> Result<()> {
    key(app, tui, session, KeyCode::Down).await?;
    key(app, tui, session, KeyCode::Enter).await?;
    Ok(())
}
fn respond(command: CloudCommand, outcome: &str) {
    let CloudOperation::Request(request) = command.operation else {
        panic!("request")
    };
    let ClientRequest::AuditAttemptControl { params, .. } = *request else {
        panic!("exact attempt control")
    };
    assert_eq!(params.assignment_id, "assignment-original");
    assert_eq!(params.action, AuditAttemptAction::Retry);
    assert!(params.authorise_retry && params.acknowledge_possible_overlap);
    match outcome {
        "success"=>command.response.send(Ok(Ok(json!({"actionKey":params.action_key,"assignmentId":params.assignment_id,"disposition":"superseded","cutoff":3,"replacementAssignmentId":"replacement","replacementOperationId":"replacement-op","completedWon":false,"possibleOverlap":true})))).unwrap(),
        "refusal"=>command.response.send(Ok(Err(codex_app_server_protocol::JSONRPCErrorError{code:-32602,message:"S4_REFUSED_ORIGINAL_TARGET".into(),data:None}))).unwrap(),
        "transport"=>command.response.send(Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset,"S4_CONTROL_TRANSPORT_LOSS"))).unwrap(),
        _=>panic!("outcome"),
    }
}
// Session setup can refresh installed plugin metadata while queued UI events are drained.
// Answer only that read-only fixture request; reject every unexpected inference/control request.
fn assert_only_fixture_inventory(commands: &mut tokio::sync::mpsc::Receiver<CloudCommand>) {
    while let Ok(command) = commands.try_recv() {
        let CloudOperation::Request(request) = command.operation else {
            panic!("unexpected non-request operation");
        };
        assert!(
            matches!(request.as_ref(), ClientRequest::PluginInstalled { .. }),
            "unexpected request: {}",
            serde_json::to_string(&request).unwrap()
        );
        command
            .response
            .send(Ok(Ok(
                json!({"marketplaces":[],"marketplaceLoadErrors":[]}),
            )))
            .unwrap();
    }
}

fn marker(outcome: &str) -> &'static str {
    match outcome {
        "success" => "Reviewer control recorded for assignment-original.",
        "refusal" => "S4_REFUSED_ORIGINAL_TARGET",
        "transport" => "S4_CONTROL_TRANSPORT_LOSS",
        _ => panic!("outcome"),
    }
}
async fn finish_and_render(
    app: &mut App,
    tui: &mut tui::Tui,
    session: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    thread: ThreadId,
) -> Result<()> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await?
            .unwrap();
        let finished = matches!(&event,AppEvent::AuditActivityControlFinished{thread_id,..} if *thread_id==thread);
        app.handle_event(tui, session, event).await?;
        if finished {
            break;
        }
    }
    while let Ok(event) = events.try_recv() {
        app.handle_event(tui, session, event).await?;
    }
    Ok(())
}
fn history(app: &App) -> String {
    app.transcript_cells
        .iter()
        .flat_map(|cell| cell.display_lines(240))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
fn push_unrelated_approval(app: &mut App, thread: ThreadId) {
    use crate::bottom_pane::{ApprovalRequest, ExecApprovalRequest};
    use codex_app_server_protocol::CommandExecutionApprovalDecision::{Accept, Cancel, Decline};
    app.chat_widget
        .push_approval_request(ApprovalRequest::Exec(ExecApprovalRequest {
            kind: Default::default(),
            thread_id: thread,
            thread_label: None,
            id: "unrelated-command".into(),
            environment_id: None,
            command: vec!["UNRELATED_COMMAND_REQUIRING_APPROVAL".into()],
            reason: Some("Must never receive viewer keys".into()),
            available_decisions: vec![Accept, Decline, Cancel],
            network_approval_context: None,
            additional_permissions: None,
        }));
}

#[tokio::test]
async fn outcomes_reach_the_open_view_or_origin_history_once_after_esc_reconnect_or_replacement()
-> Result<()> {
    for mode in ["open", "escape", "reconnect", "replacement"] {
        for outcome in ["success", "refusal", "transport"] {
            let (mut app, mut events, mut ops) =
                crate::app::tests::make_test_app_with_channels().await;
            let (mut session, mut commands, _incoming) = cloud_session();
            let mut tui = crate::tui::test_support::make_test_tui()?;
            let thread = ThreadId::new();
            install(&mut app, thread, crate::audit::tests::attempt_fixture());
            drain_session_setup(&mut ops);
            open_confirmation(&mut app, &mut tui, &mut session, &mut commands, &mut events).await?;
            accept(&mut app, &mut tui, &mut session).await?;
            match mode {
                "open" => {}
                "escape" => {
                    key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
                    assert!(app.overlay.is_none());
                }
                "reconnect" => {
                    app.app_server_target = AppServerTarget::LocalDaemon {
                        endpoint: codex_app_server_client::RemoteAppServerEndpoint::UnixSocket {
                            socket_path: app.config.codex_home.join("owned-unused.sock"),
                        },
                        allow_embedded_fallback: false,
                    };
                    assert!(app.begin_reconnect());
                    assert!(app.overlay.is_none());
                }
                "replacement" => {
                    let mut other = crate::audit::tests::attempt_fixture();
                    other.assignment_id = "replacement-view".into();
                    install(&mut app, thread, other);
                    drain_session_setup(&mut ops);
                }
                _ => unreachable!(),
            }
            let command = tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await?
                .unwrap();
            respond(command, outcome);
            finish_and_render(&mut app, &mut tui, &mut session, &mut events, thread).await?;
            let history = history(&app);
            if mode == "open" {
                assert_eq!(
                    history.matches(marker(outcome)).count(),
                    0,
                    "outcome must not appear in both destinations"
                );
                let Some(Overlay::AuditActivity(viewer)) = &mut app.overlay else {
                    panic!("same viewer")
                };
                let area = ratatui::layout::Rect::new(0, 0, 240, 60);
                let mut buf = ratatui::buffer::Buffer::empty(area);
                viewer.render(area, &mut buf);
                let visible = buf
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert_eq!(visible.matches(marker(outcome)).count(), 1);
            } else {
                assert_eq!(
                    history.matches(marker(outcome)).count(),
                    1,
                    "{mode}/{outcome}: {history}"
                );
            }
            assert_only_fixture_inventory(&mut commands);
            assert!(ops.try_recv().is_err());
            let snapshot = app
                .ensure_thread_channel(thread)
                .store
                .lock()
                .await
                .snapshot();
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .filter(|event| matches!(event, ThreadBufferedEvent::AuditControlOutcome(_)))
                    .count(),
                usize::from(mode != "open")
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn switched_threads_retain_the_origin_outcome_for_replay_without_misattribution() -> Result<()>
{
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let (mut session, mut commands, _incoming) = cloud_session();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let origin = ThreadId::new();
    install(&mut app, origin, crate::audit::tests::attempt_fixture());
    drain_session_setup(&mut ops);
    open_confirmation(&mut app, &mut tui, &mut session, &mut commands, &mut events).await?;
    accept(&mut app, &mut tui, &mut session).await?;
    // Drain origin disclosure before switching, as the normal UI event loop does.
    while let Ok(event) = events.try_recv() {
        app.handle_event(&mut tui, &mut session, event).await?;
    }
    let other = ThreadId::new();
    install(&mut app, other, crate::audit::tests::attempt_fixture());
    drain_session_setup(&mut ops);
    app.transcript_cells.clear();
    respond(commands.recv().await.unwrap(), "refusal");
    finish_and_render(&mut app, &mut tui, &mut session, &mut events, origin).await?;
    assert!(!history(&app).contains(marker("refusal")));
    let snapshot = app
        .ensure_thread_channel(origin)
        .store
        .lock()
        .await
        .snapshot();
    let outcomes = snapshot
        .events
        .into_iter()
        .filter(|event| matches!(event, ThreadBufferedEvent::AuditControlOutcome(_)))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 1);
    assert!(
        app.ensure_thread_channel(other)
            .store
            .lock()
            .await
            .snapshot()
            .events
            .iter()
            .all(|event| !matches!(event, ThreadBufferedEvent::AuditControlOutcome(_)))
    );
    let mut retained = outcomes[0].clone();
    assert!(ThreadEventStore::event_survives_session_refresh(
        &mut retained
    ));
    install(&mut app, origin, crate::audit::tests::attempt_fixture());
    drain_session_setup(&mut ops);
    app.transcript_cells.clear();
    app.handle_thread_event_replay(retained);
    while let Ok(event) = events.try_recv() {
        app.handle_event(&mut tui, &mut session, event).await?;
    }
    assert_eq!(history(&app).matches(marker("refusal")).count(), 1);
    assert_only_fixture_inventory(&mut commands);
    assert!(ops.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn unrelated_approvals_above_or_below_the_shared_confirmation_never_receive_viewer_keys()
-> Result<()> {
    for position in ["above", "below"] {
        let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
        let (mut session, mut commands, _incoming) = cloud_session();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let thread = ThreadId::new();
        install(&mut app, thread, crate::audit::tests::attempt_fixture());
        drain_session_setup(&mut ops);
        if position == "below" {
            push_unrelated_approval(&mut app, thread);
        }
        open_confirmation(&mut app, &mut tui, &mut session, &mut commands, &mut events).await?;
        assert!(app.chat_widget.audit_activity_confirmation_active());
        if position == "above" {
            push_unrelated_approval(&mut app, thread);
            assert!(!app.chat_widget.audit_activity_confirmation_active());
            assert!(
                crate::chatwidget::tests::render_bottom_popup(&app.chat_widget, 120)
                    .contains("UNRELATED_COMMAND_REQUIRING_APPROVAL")
            );
            for code in [KeyCode::Enter, KeyCode::Down, KeyCode::Tab, KeyCode::Enter] {
                key(&mut app, &mut tui, &mut session, code).await?;
            }
            app.handle_tui_event(
                &mut tui,
                &mut session,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            )
            .await?;
            assert!(ops.try_recv().is_err());
            assert!(commands.try_recv().is_err());
        } else {
            accept(&mut app, &mut tui, &mut session).await?;
            assert!(!app.chat_widget.audit_activity_confirmation_active());
            key(&mut app, &mut tui, &mut session, KeyCode::Enter).await?;
            assert!(ops.try_recv().is_err());
        }
        key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
        assert!(app.overlay.is_none());
        assert!(
            crate::chatwidget::tests::render_bottom_popup(&app.chat_widget, 120)
                .contains("UNRELATED_COMMAND_REQUIRING_APPROVAL")
        );
        assert!(ops.try_recv().is_err());
        if position == "below" {
            respond(commands.recv().await.unwrap(), "success");
            finish_and_render(&mut app, &mut tui, &mut session, &mut events, thread).await?;
            assert_eq!(history(&app).matches(marker("success")).count(), 1);
        } else {
            tokio::task::yield_now().await;
            assert!(commands.try_recv().is_err());
        }
        // The unrelated approval still belongs to the main TUI, and has received no decision.
        assert!(
            crate::chatwidget::tests::render_bottom_popup(&app.chat_widget, 120)
                .contains("UNRELATED_COMMAND_REQUIRING_APPROVAL")
        );
        assert!(ops.try_recv().is_err());
    }
    Ok(())
}
