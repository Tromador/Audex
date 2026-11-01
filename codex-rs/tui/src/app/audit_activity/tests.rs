use super::*;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
use codex_app_server_protocol::{AUDIT_PROTOCOL_VERSION, AuditAttemptAction, ClientRequest};
use serde_json::json;

fn drain_session_setup(
    ops: &mut tokio::sync::mpsc::UnboundedReceiver<crate::app_command::AppCommand>,
) {
    let initial = ops.try_recv().expect("session setup lists skills");
    assert!(matches!(
        initial,
        crate::app_command::AppCommand::ListSkills { .. }
    ));
    assert!(ops.try_recv().is_err());
}

fn install(app: &mut App, thread: ThreadId, target: AuditAttemptView) {
    app.active_thread_id = Some(thread);
    app.primary_thread_id = Some(thread);
    let session = crate::app::tests::test_thread_session(thread, app.config.cwd.to_path_buf());
    app.chat_widget.handle_thread_session(session);
    app.overlay = Some(Overlay::AuditActivity(Box::new(
        crate::audit::activity::ActivityViewer::new(thread, target, &app.keymap),
    )));
}
fn cloud_session() -> (
    AppServerSession,
    tokio::sync::mpsc::Receiver<codex_app_server_client::CloudCommand>,
    tokio::sync::mpsc::UnboundedSender<codex_app_server_client::AppServerEvent>,
) {
    let (cloud, commands, incoming) = CloudClient::channel(8);
    (
        AppServerSession::new(
            crate::backend_id::BackendId::new(),
            AppServerClient::Cloud(cloud),
            ThreadParamsMode::Cloud,
        ),
        commands,
        incoming,
    )
}
async fn key(
    app: &mut App,
    tui: &mut tui::Tui,
    session: &mut AppServerSession,
    code: KeyCode,
) -> Result<()> {
    app.handle_tui_event(
        tui,
        session,
        TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn viewer_navigation_paste_and_search_never_reach_composer_or_inference() -> Result<()> {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let (mut session, mut commands, _incoming) = cloud_session();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let thread = ThreadId::new();
    install(&mut app, thread, crate::audit::tests::attempt_fixture());
    drain_session_setup(&mut ops);
    app.chat_widget
        .apply_external_edit("preserved operator draft".into());
    for code in [
        KeyCode::Up,
        KeyCode::PageUp,
        KeyCode::End,
        KeyCode::Enter,
        KeyCode::Char('r'),
        KeyCode::Char('/'),
        KeyCode::Char('R'),
        KeyCode::Char('C'),
        KeyCode::Char('I'),
    ] {
        key(&mut app, &mut tui, &mut session, code).await?;
    }
    app.handle_tui_event(
        &mut tui,
        &mut session,
        TuiEvent::Paste("R\nC\nI\nYes\n".into()),
    )
    .await?;
    key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?; // dismiss search
    assert!(matches!(app.overlay, Some(Overlay::AuditActivity(_))));
    app.handle_tui_event(&mut tui, &mut session, TuiEvent::Paste("C\nR\nI".into()))
        .await?;
    key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
    assert!(app.overlay.is_none());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "preserved operator draft"
    );
    install(&mut app, thread, crate::audit::tests::attempt_fixture());
    drain_session_setup(&mut ops);
    for character in ['C', 'R', 'I'] {
        let mut repeated = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
        repeated.kind = KeyEventKind::Repeat;
        app.handle_tui_event(&mut tui, &mut session, TuiEvent::Key(repeated))
            .await?;
    }
    app.handle_tui_event(
        &mut tui,
        &mut session,
        TuiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    )
    .await?;
    assert!(app.overlay.is_none());
    assert!(commands.try_recv().is_err());
    assert!(ops.try_recv().is_err());
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event,
            AppEvent::AuditCommand { .. } | AppEvent::AuditAttemptConfirm { .. }
        ))
    );
    Ok(())
}

#[tokio::test]
async fn contextual_controls_use_shared_no_default_and_original_assignment_through_stale_confirmation()
-> Result<()> {
    for (shortcut, action) in [
        ('C', AuditAttemptAction::Cancel),
        ('R', AuditAttemptAction::Retry),
        ('I', AuditAttemptAction::Ignore),
    ] {
        for decision in ["default-no", "escape", "accept", "readiness", "superseded"] {
            if shortcut != 'R' && matches!(decision, "readiness" | "superseded") {
                continue;
            }
            let (mut app, mut events, mut ops) =
                crate::app::tests::make_test_app_with_channels().await;
            let (mut session, mut commands, _incoming) = cloud_session();
            let mut tui = crate::tui::test_support::make_test_tui()?;
            let thread = ThreadId::new();
            let target = crate::audit::tests::attempt_fixture();
            install(&mut app, thread, target.clone());
            drain_session_setup(&mut ops);
            key(&mut app, &mut tui, &mut session, KeyCode::Char(shortcut)).await?;
            let capability = tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await?
                .unwrap();
            assert!(
                matches!(capability.operation,CloudOperation::Request(r) if matches!(*r,ClientRequest::AuditCapabilities{..}))
            );
            capability
                .response
                .send(Ok(Ok(json!({"protocolVersion":AUDIT_PROTOCOL_VERSION}))))
                .unwrap();
            let confirmation_event = loop {
                let event = events.recv().await.unwrap();
                if matches!(event, AppEvent::AuditAttemptConfirm { .. }) {
                    break event;
                }
            };
            if let AppEvent::AuditAttemptConfirm { confirmation, .. } = &confirmation_event {
                assert_eq!(confirmation.target.assignment_id, target.assignment_id);
                assert_eq!(confirmation.request.action, action);
                let disclosure = confirmation.disclosure();
                assert!(disclosure.contains("assignment-original"));
                assert!(disclosure.contains("native-model"));
                match action {
                    AuditAttemptAction::Retry => assert!(disclosure.contains("additional charges")),
                    AuditAttemptAction::Ignore => assert!(disclosure.contains("permanently")),
                    AuditAttemptAction::Cancel => assert!(disclosure.contains("does not reserve")),
                }
            }
            app.handle_event(&mut tui, &mut session, confirmation_event)
                .await?;
            assert!(
                commands.try_recv().is_err(),
                "no mutation before acceptance"
            );
            app.handle_tui_event(
                &mut tui,
                &mut session,
                TuiEvent::Paste("Down\nYes\nR\n".into()),
            )
            .await?;
            key(&mut app, &mut tui, &mut session, KeyCode::Char('R')).await?;
            assert!(commands.try_recv().is_err());
            if matches!(decision, "readiness" | "superseded")
                && let Some(Overlay::AuditActivity(viewer)) = &mut app.overlay
            {
                viewer.target.retry_eligible = false;
                if decision == "superseded" {
                    viewer.target.current = false;
                    viewer.target.disposition = "superseded".into();
                }
            }
            if decision == "escape" {
                key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
            } else {
                if decision != "default-no" {
                    key(&mut app, &mut tui, &mut session, KeyCode::Down).await?;
                }
                key(&mut app, &mut tui, &mut session, KeyCode::Enter).await?;
            }
            if matches!(decision, "accept" | "readiness" | "superseded") {
                let command = tokio::time::timeout(Duration::from_secs(2), commands.recv())
                    .await?
                    .unwrap();
                let CloudOperation::Request(request) = command.operation else {
                    panic!("request")
                };
                let ClientRequest::AuditAttemptControl { params, .. } = *request else {
                    panic!("control only")
                };
                assert_eq!(params.assignment_id, "assignment-original");
                assert_eq!(params.run_id, "run-1");
                assert_eq!(params.action, action);
                assert_eq!(params.authorise_retry, shortcut == 'R');
                assert_eq!(params.acknowledge_possible_overlap, shortcut == 'R');
                assert!(!params.action_key.is_empty());
                if decision == "accept" {
                    command.response.send(Ok(Ok(json!({"actionKey":params.action_key,"assignmentId":params.assignment_id,"disposition":"cancelled","cutoff":3,"replacementAssignmentId":null,"replacementOperationId":null,"completedWon":false,"possibleOverlap":true})))).unwrap();
                } else {
                    command
                        .response
                        .send(Ok(Err(codex_app_server_protocol::JSONRPCErrorError {
                            code: -32602,
                            message: "original assignment is no longer eligible".into(),
                            data: None,
                        })))
                        .unwrap();
                }
            }
            loop {
                let event = events.recv().await.unwrap();
                if matches!(event, AppEvent::AuditActivityControlFinished { .. }) {
                    app.handle_event(&mut tui, &mut session, event).await?;
                    break;
                }
            }
            assert!(
                commands.try_recv().is_err(),
                "no successor lookup, retry replay, or inference"
            );
            assert!(ops.try_recv().is_err());
            assert!(
                matches!(&app.overlay,Some(Overlay::AuditActivity(viewer)) if viewer.target.assignment_id=="assignment-original" && !viewer.control_pending)
            );
            key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
            assert!(app.overlay.is_none());
        }
    }
    Ok(())
}

#[tokio::test]
async fn selected_historical_attempt_opens_activity_and_discards_closed_view_pages() -> Result<()> {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let (mut session, mut commands, _incoming) = cloud_session();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let thread = ThreadId::new();
    let mut target = crate::audit::tests::attempt_fixture();
    target.current = false;
    target.retry_eligible = false;
    target.disposition = "superseded".into();
    install(&mut app, thread, target.clone());
    drain_session_setup(&mut ops);
    app.overlay = None;
    app.chat_widget.show_audit_attempt(target.clone());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let open = loop {
        let event = events.recv().await.unwrap();
        if matches!(event, AppEvent::AuditActivityOpen { .. }) {
            break event;
        }
    };
    app.handle_event(&mut tui, &mut session, open).await?;
    let id = match &app.overlay {
        Some(Overlay::AuditActivity(viewer)) => viewer.id,
        _ => panic!("activity"),
    };
    let command = commands.recv().await.unwrap();
    assert!(
        matches!(command.operation,CloudOperation::Request(r) if matches!(&*r,ClientRequest::AuditAttemptRead{params,..} if params.assignment_id=="assignment-original" && params.after_sequence==Some(0)))
    );
    key(&mut app, &mut tui, &mut session, KeyCode::Char('R')).await?;
    key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
    let (consumed, ack) = tokio::sync::oneshot::channel();
    app.handle_event(
        &mut tui,
        &mut session,
        AppEvent::AuditActivityPage {
            viewer_id: id,
            reader_epoch: 1,
            page: Box::new(Err("disconnected".into())),
            consumed,
        },
    )
    .await?;
    assert!(ack.await.is_err());
    assert!(app.overlay.is_none());
    assert!(commands.try_recv().is_err());
    assert!(ops.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn pending_escape_declines_but_close_after_yes_finishes_the_authorised_control() -> Result<()>
{
    let (mut app, mut events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let (mut session, mut commands, _incoming) = cloud_session();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let thread = ThreadId::new();
    install(&mut app, thread, crate::audit::tests::attempt_fixture());
    key(&mut app, &mut tui, &mut session, KeyCode::Char('R')).await?;
    let capability = commands.recv().await.unwrap();
    key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
    assert!(app.overlay.is_none());
    let _ = capability
        .response
        .send(Ok(Ok(json!({"protocolVersion":AUDIT_PROTOCOL_VERSION}))));
    tokio::task::yield_now().await;
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|e| matches!(e, AppEvent::AuditAttemptConfirm { .. }))
    );
    assert!(commands.try_recv().is_err());
    install(&mut app, thread, crate::audit::tests::attempt_fixture());
    key(&mut app, &mut tui, &mut session, KeyCode::Char('R')).await?;
    let capability = commands.recv().await.unwrap();
    capability
        .response
        .send(Ok(Ok(json!({"protocolVersion":AUDIT_PROTOCOL_VERSION}))))
        .unwrap();
    let event = loop {
        let event = events.recv().await.unwrap();
        if matches!(event, AppEvent::AuditAttemptConfirm { .. }) {
            break event;
        }
    };
    app.handle_event(&mut tui, &mut session, event).await?;
    key(&mut app, &mut tui, &mut session, KeyCode::Down).await?;
    key(&mut app, &mut tui, &mut session, KeyCode::Enter).await?;
    key(&mut app, &mut tui, &mut session, KeyCode::Esc).await?;
    assert!(app.overlay.is_none());
    let command = tokio::time::timeout(Duration::from_secs(2), commands.recv())
        .await?
        .unwrap();
    let CloudOperation::Request(request) = command.operation else {
        panic!("request")
    };
    let ClientRequest::AuditAttemptControl { params, .. } = *request else {
        panic!("authorised control")
    };
    assert_eq!(params.assignment_id, "assignment-original");
    assert_eq!(params.action, AuditAttemptAction::Retry);
    command.response.send(Ok(Ok(json!({"actionKey":params.action_key,"assignmentId":params.assignment_id,"disposition":"superseded","cutoff":3,"replacementAssignmentId":"replacement","replacementOperationId":"replacement-op","completedWon":false,"possibleOverlap":true})))).unwrap();
    Ok(())
}

#[tokio::test]
async fn reconnect_discards_old_reader_pages_and_progress_wakes_without_revision_change()
-> Result<()> {
    let (mut app, _events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let (mut session, _commands, _incoming) = cloud_session();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let thread = ThreadId::new();
    let mut target = crate::audit::tests::attempt_fixture();
    target.retry_eligible = false;
    install(&mut app, thread, target.clone());
    let Some(Overlay::AuditActivity(viewer)) = &mut app.overlay else {
        panic!("viewer")
    };
    viewer.reader_epoch = 2;
    let id = viewer.id;
    let wake = viewer.wake.clone();
    app.audits.activity_wake = Some((target.run_id.clone(), wake.clone()));
    let (consumed, ack) = tokio::sync::oneshot::channel();
    let mut obsolete = target.clone();
    obsolete.retry_eligible = true;
    app.handle_event(
        &mut tui,
        &mut session,
        AppEvent::AuditActivityPage {
            viewer_id: id,
            reader_epoch: 1,
            page: Box::new(Ok(codex_app_server_protocol::AuditAttemptReadResponse {
                attempt: obsolete,
                entries: vec![],
                next_sequence: 0,
                raw_result: None,
            })),
            consumed,
        },
    )
    .await?;
    assert!(ack.await.is_err());
    assert!(
        matches!(&app.overlay,Some(Overlay::AuditActivity(viewer)) if !viewer.target.retry_eligible)
    );
    app.audits
        .wake(&codex_app_server_protocol::AuditProgressNotification {
            run_id: target.run_id,
            revision: 1,
            phase: codex_app_server_protocol::AuditRunPhase::Reviewing,
            attempts: vec![],
        });
    tokio::time::timeout(Duration::from_secs(1), wake.notified()).await?;
    Ok(())
}

#[path = "correction_tests.rs"]
mod corrections;
