//! Exercise the S4 correction mechanisms through notifications and visible output.
use super::*;
use codex_app_server_client::{AppServerClient, CloudClient, CloudCommand, CloudOperation};
use serde_json::json;
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};

fn session(cloud: CloudClient) -> AppServerSession {
    AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(cloud),
        crate::app_server_session::ThreadParamsMode::Remote,
    )
}

fn display(app: &mut App, thread: ThreadId) {
    app.active_thread_id = Some(thread);
    app.primary_thread_id = Some(thread);
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            thread,
            app.config.cwd.to_path_buf(),
        ));
}

fn history(events: &mut UnboundedReceiver<AppEvent>) -> Vec<String> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.display_lines(160)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}

fn running() -> AuditRun {
    let mut run = super::tests::completed_fixture();
    run.complete = false;
    run.summary = None;
    for status in &mut run.reviewers {
        status.state = "running".into();
        status.finding_count = None;
        status.has_output = false;
    }
    run
}

async fn answer_read(
    app: &mut App,
    commands: &mut Receiver<CloudCommand>,
    events: &mut UnboundedReceiver<AppEvent>,
    run: &AuditRun,
) {
    let command = tokio::time::timeout(std::time::Duration::from_secs(2), commands.recv())
        .await
        .expect("audit read requested")
        .unwrap();
    assert!(
        matches!(command.operation, CloudOperation::Request(ref request)
        if matches!(**request, codex_app_server_protocol::ClientRequest::AuditRead { .. }))
    );
    command
        .response
        .send(Ok(Ok(json!({"run":run,"data":null}))))
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("audit read delivered")
            .unwrap();
        if let AppEvent::AuditUpdated {
            thread_id,
            run,
            error,
        } = event
        {
            app.audit_updated(thread_id, &run, error);
            break;
        }
    }
}

#[tokio::test]
async fn s4_f1_tool_counts_do_not_append_history_but_state_and_attempt_changes_do() {
    let (mut app, mut events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display(&mut app, thread);
    history(&mut events);
    let run = running();
    app.audit_updated(thread, &run, None);
    assert_eq!(history(&mut events).len(), 1);
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let session = session(cloud);
    let mut status = run.reviewers[0].clone();
    for count in 1..=5 {
        status.tool_calls = count;
        app.audit_progress(
            &session,
            &AuditProgress {
                run_id: run.run_id.clone(),
                reviewer: Some(status.clone()),
                complete: false,
                error: None,
            },
        );
        assert!(
            history(&mut events).is_empty(),
            "count-only updates must not add history"
        );
    }
    let retained = app.audits.runs[&(thread, run.run_id.clone())].clone();
    assert_eq!(retained.reviewers[0].tool_calls, 5);
    assert!(crate::audit::progress(&retained).contains("running — 5 tool calls"));
    app.audit_opened(&session, thread, retained.clone(), false);
    let shown = history(&mut events);
    assert_eq!(shown.len(), 1);
    assert!(shown[0].contains("running — 5 tool calls"));
    answer_read(&mut app, &mut commands, &mut events, &retained).await;
    assert!(history(&mut events).is_empty());
    for (attempt, trigger, state, detail) in [
        (1, "initial", "failed", "failed — fixture failure"),
        (
            2,
            "automatic_replacement",
            "running",
            "attempt 2 (automatic replacement)",
        ),
        (3, "operator_retry", "queued", "attempt 3 (operator retry)"),
        (3, "operator_retry", "succeeded", "succeeded — 2 findings"),
    ] {
        status.attempt = attempt;
        status.trigger = trigger.into();
        status.state = state.into();
        status.reason = Some("fixture failure".into());
        status.finding_count = (state == "succeeded").then_some(2);
        app.audit_progress(
            &session,
            &AuditProgress {
                run_id: run.run_id.clone(),
                reviewer: Some(status.clone()),
                complete: false,
                error: None,
            },
        );
        let shown = history(&mut events);
        assert_eq!(
            shown.len(),
            1,
            "state/attempt transition must remain visible"
        );
        assert!(shown[0].contains(detail), "{}", shown[0]);
    }
}

#[tokio::test]
async fn s4_f2_two_conversations_schedule_one_comparison_even_after_reopening() {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let a = ThreadId::new();
    let b = ThreadId::new();
    display(&mut app, a);
    history(&mut events);
    while ops.try_recv().is_ok() {}
    let (cloud, mut commands, _incoming) = CloudClient::channel(4);
    let session = session(cloud);
    let run = running();
    app.audit_opened(&session, a, run.clone(), true);
    answer_read(&mut app, &mut commands, &mut events, &run).await;
    app.audit_opened(&session, b, run.clone(), false);
    answer_read(&mut app, &mut commands, &mut events, &run).await;
    app.audit_progress(
        &session,
        &AuditProgress {
            run_id: run.run_id.clone(),
            reviewer: None,
            complete: true,
            error: None,
        },
    );
    let complete = super::tests::completed_fixture();
    for _ in 0..2 {
        answer_read(&mut app, &mut commands, &mut events, &complete).await;
    }
    // Finish A's comparison before visiting B, so B is genuinely idle.
    for (method, status) in [
        ("turn/started", "inProgress"),
        ("turn/completed", "completed"),
    ] {
        app.chat_widget.handle_server_notification(
            serde_json::from_value(json!({
                "method":method,"params":{"threadId":a.to_string(),"turn":{
                    "id":"comparison-a","status":status,"items":[]}}
            }))
            .unwrap(),
            None,
        );
    }
    display(&mut app, b);
    app.show_ready_audits(b);
    let comparisons = std::iter::from_fn(|| ops.try_recv().ok())
        .filter_map(|op| match op {
            AppCommand::UserTurn { items, .. } => Some(serde_json::to_string(&items).unwrap()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(comparisons.len(), 1);
    assert!(comparisons[0].contains("Compare completed audit:"));
    // An older running response must not give a completed run another comparison owner.
    app.audit_opened(&session, b, run, false);
    answer_read(&mut app, &mut commands, &mut events, &complete).await;
    app.show_ready_audits(b);
    assert!(
        !std::iter::from_fn(|| ops.try_recv().ok())
            .any(|op| matches!(op, AppCommand::UserTurn { .. }))
    );
}

#[tokio::test]
async fn s4_f2_reopen_without_an_owner_and_reconnect_still_compare_once() {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display(&mut app, thread);
    history(&mut events);
    while ops.try_recv().is_ok() {}
    let (cloud, mut commands, _incoming) = CloudClient::channel(2);
    let session = session(cloud);
    let run = running();
    app.audit_opened(&session, thread, run.clone(), false);
    answer_read(&mut app, &mut commands, &mut events, &run).await;
    app.reconnect_audits(&session);
    answer_read(
        &mut app,
        &mut commands,
        &mut events,
        &super::tests::completed_fixture(),
    )
    .await;
    app.show_ready_audits(thread);
    assert_eq!(
        std::iter::from_fn(|| ops.try_recv().ok())
            .filter(|op| matches!(op, AppCommand::UserTurn { .. }))
            .count(),
        1
    );
}

pub(super) async fn assert_summary_error_visible(background: bool) {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display(&mut app, if background { ThreadId::new() } else { thread });
    let mut run = super::tests::completed_fixture();
    run.complete = false;
    run.summary = None;
    app.audits
        .awaiting_comparison
        .insert((thread, run.run_id.clone()));
    app.audit_updated(thread, &run, None);
    history(&mut events);
    while ops.try_recv().is_ok() {}
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let session = session(cloud);
    app.audit_progress(
        &session,
        &AuditProgress {
            run_id: run.run_id.clone(),
            reviewer: None,
            complete: false,
            error: Some("summary write failed".into()),
        },
    );
    if background {
        assert!(
            history(&mut events).is_empty(),
            "background error must not leak into another conversation"
        );
        answer_read(&mut app, &mut commands, &mut events, &run).await;
        assert!(history(&mut events).is_empty());
        display(&mut app, thread);
        app.show_ready_audits(thread);
    }
    let shown = history(&mut events).join("\n");
    assert_eq!(shown.matches("summary write failed").count(), 1);
    if !background {
        answer_read(&mut app, &mut commands, &mut events, &run).await;
    }
    app.show_ready_audits(thread);
    app.show_ready_audits(thread);
    assert!(
        !history(&mut events)
            .join("\n")
            .contains("summary write failed")
    );
    let retained = &app.audits.runs[&(thread, run.run_id.clone())];
    assert!(!retained.complete);
    assert!(retained.summary.is_none());
    assert!(
        !std::iter::from_fn(|| ops.try_recv().ok())
            .any(|op| matches!(op, AppCommand::UserTurn { .. }))
    );
}

#[tokio::test]
async fn bare_audit_reads_only_this_threads_selection_and_explicit_list_still_lists() {
    use codex_app_server_protocol::ClientRequest;
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    let other = ThreadId::new();
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let session = session(cloud);
    display(&mut app, thread);
    while ops.try_recv().is_ok() {}
    let mut selected = super::tests::completed_fixture();
    let other_run = selected.clone();
    selected.run_id = "selected-historical-run".into();
    app.audit_opened(&session, other, other_run, false);
    for (args, has_selection) in [("", false), ("", true), ("list", true)] {
        if has_selection {
            app.audit_opened(&session, thread, selected.clone(), false);
        }
        history(&mut events);
        app.audit_command(&session, thread, args.into());
        let command = commands.recv().await.unwrap();
        let CloudOperation::Request(request) = command.operation else {
            panic!()
        };
        if has_selection && args.is_empty() {
            let ClientRequest::AuditRead { params, .. } = *request else {
                panic!()
            };
            assert_eq!(params.run_id, selected.run_id);
            assert!(params.view.is_none());
            command
                .response
                .send(Ok(Ok(json!({"run":selected,"data":null}))))
                .unwrap();
            assert!(
                matches!(events.recv().await, Some(AppEvent::AuditMenuLoaded { thread_id, run })
                if thread_id == thread && run.run_id == selected.run_id)
            );
        } else {
            assert!(matches!(*request, ClientRequest::AuditList { .. }));
            command
                .response
                .send(Ok(Ok(json!({"runs":[],"nextOffset":null,"presets":{}}))))
                .unwrap();
            assert!(
                matches!(events.recv().await, Some(AppEvent::AuditListLoaded { thread_id, .. }) if thread_id == thread)
            );
        }
        assert!(ops.try_recv().is_err());
    }
}

#[tokio::test]
async fn partial_audit_proceed_displays_retained_findings_and_retry_still_compares() {
    use crate::audit::{AuditAction, menu};
    use codex_app_server_protocol::{AuditReadView, ClientRequest};
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let session = session(cloud);
    display(&mut app, thread);
    while ops.try_recv().is_ok() {}
    let mut run = super::tests::completed_fixture();
    run.reviewers[0].finding_count = Some(1);
    run.reviewers[1].state = "interrupted".into();
    run.reviewers[1].finding_count = None;
    app.audit_opened(&session, thread, run.clone(), true);
    let initial = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !initial
            .iter()
            .any(|e| matches!(e, AppEvent::AuditReport { .. }))
    );
    assert!(commands.try_recv().is_err() && ops.try_recv().is_err());
    let proceed = menu::items(&run)
        .into_iter()
        .find(|i| matches!(i.action, AuditAction::Proceed(_)))
        .unwrap();
    app.run_audit_action(
        &session,
        thread,
        Ok(proceed.action),
        Some((run.run_id.clone(), proceed.attempt)),
    );
    let command = commands.recv().await.unwrap();
    let CloudOperation::Request(request) = command.operation else {
        panic!()
    };
    let ClientRequest::AuditRead { params, .. } = *request else {
        panic!()
    };
    assert_eq!(params.view, Some(AuditReadView::Findings));
    assert_eq!(params.reviewer.as_deref(), Some("reviewer-1"));
    let data = json!({"items":[{"finding":{"title":"Retained finding", "path":"a.rs", "start_line":3, "end_line":5}}]});
    command
        .response
        .send(Ok(Ok(json!({"run":run,"data":data}))))
        .unwrap();
    let Some(AppEvent::AuditReport { thread_id, report }) = events.recv().await else {
        panic!()
    };
    assert_eq!(thread_id, thread);
    assert!(report.markdown.contains("Retained finding — a.rs:3-5"));
    assert!(
        report.markdown.contains("reviewer-1")
            && report
                .markdown
                .contains("reviewer-2 (native:fixture) — interrupted")
    );
    app.enqueue_audit_report(thread_id, report).await;
    assert!(
        history(&mut events)
            .iter()
            .any(|cell| cell.contains("Retained finding"))
    );
    assert!(ops.try_recv().is_err() && commands.try_recv().is_err());
    assert!(app.audits.compared_runs.is_empty());
    assert_eq!(app.audits.runs[&(thread, run.run_id.clone())], run);

    let retry = menu::items(&run)
        .into_iter()
        .find(|i| matches!(i.action, AuditAction::Retry(_)))
        .unwrap();
    app.run_audit_action(
        &session,
        thread,
        Ok(retry.action),
        Some((run.run_id.clone(), retry.attempt)),
    );
    let command = commands.recv().await.unwrap();
    assert!(
        matches!(command.operation, CloudOperation::Request(r) if matches!(*r, ClientRequest::AuditRead { .. }))
    );
    command
        .response
        .send(Ok(Ok(json!({"run":run,"data":null}))))
        .unwrap();
    let command = commands.recv().await.unwrap();
    let CloudOperation::Request(request) = command.operation else {
        panic!()
    };
    let ClientRequest::AuditRetry { params, .. } = *request else {
        panic!()
    };
    assert_eq!(
        (params.run_id, params.reviewer),
        (run.run_id.clone(), "reviewer-2".into())
    );
    assert!(ops.try_recv().is_err());
    let mut second = run.reviewers[1].clone();
    second.attempt += 1;
    second.trigger = "operator_retry".into();
    second.state = "succeeded".into();
    run.reviewers.push(second);
    command.response.send(Ok(Ok(json!({"run":run})))).unwrap();
    let Some(AppEvent::AuditOpened {
        thread_id,
        run,
        compare,
    }) = events.recv().await
    else {
        panic!()
    };
    assert!(compare);
    app.audit_opened(&session, thread_id, run.clone(), compare);
    app.audit_updated(thread_id, &run, None);
    assert_eq!(
        std::iter::from_fn(|| ops.try_recv().ok())
            .filter(|op| matches!(op, AppCommand::UserTurn { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn stale_retry_cannot_resubmit_a_newer_attempt_from_the_same_reviewer() {
    use crate::audit::{AuditAction, menu};
    use codex_app_server_protocol::ClientRequest;
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let session = session(cloud);
    display(&mut app, thread);
    while ops.try_recv().is_ok() {}
    let mut run = super::tests::completed_fixture();
    run.reviewers[1].state = "failed".into();
    app.audit_opened(&session, thread, run.clone(), false);
    history(&mut events);
    let item = menu::items(&run)
        .into_iter()
        .find(|i| matches!(i.action, AuditAction::Retry(_)))
        .unwrap();
    app.run_audit_action(
        &session,
        thread,
        Ok(item.action),
        Some((run.run_id.clone(), item.attempt)),
    );
    let command = commands.recv().await.unwrap();
    assert!(
        matches!(command.operation, CloudOperation::Request(r) if matches!(*r, ClientRequest::AuditRead { .. }))
    );
    let mut newer = run.reviewers[1].clone();
    newer.attempt += 1;
    run.reviewers.push(newer);
    command
        .response
        .send(Ok(Ok(json!({"run":run,"data":null}))))
        .unwrap();
    assert!(
        matches!(events.recv().await, Some(AppEvent::AuditError { thread_id, message })
        if thread_id == thread && message.contains("has changed"))
    );
    assert!(commands.try_recv().is_err() && ops.try_recv().is_err());
}
