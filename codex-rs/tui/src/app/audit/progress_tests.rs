//! M2-F2: suppress repeated presentation without suppressing audit processing.
use super::*;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

fn run_fixture() -> AuditRunView {
    serde_json::from_value(json!({
        "runId":"progress-run", "snapshotId":"snapshot", "assignmentId":"assignment",
        "phase":"reviewing", "operationState":"accepted", "revision":7,
        "observationPending":false, "capacityReleased":false, "cancellationRequested":false,
        "outcome":null, "rawResult":null, "report":null,
        "collection":{"scheduling":"automatic","ready":false,"reviewerCount":2,
            "completedCount":0,"excludedCount":0,"outstandingCount":2}
    }))
    .unwrap()
}

fn display_thread(app: &mut App, thread: ThreadId) {
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
                cell.display_lines(1000)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}

fn progress_messages(shown: &[String]) -> Vec<String> {
    shown
        .iter()
        .filter(|text| text.contains("Audit progress-run:"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn identical_visible_audit_progress_is_displayed_once() {
    let (mut app, mut events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display_thread(&mut app, thread);
    let mut run = run_fixture();
    let visible = crate::audit::progress(&run);
    app.audit_updated(thread, &run, None);
    app.audit_updated(thread, &run, None);
    // Retained output changes the full record, but is absent from progress().
    let original = run.clone();
    run.raw_result = Some("retained internal output".into());
    assert_ne!(run, original);
    assert_eq!(crate::audit::progress(&run), visible);
    app.audit_updated(thread, &run, None);
    assert_eq!(progress_messages(&history(&mut events)).len(), 1);
}

#[tokio::test]
async fn visible_audit_changes_are_not_limited_to_revision_or_first_seen_text() {
    let (mut app, mut events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display_thread(&mut app, thread);
    let mut run = run_fixture();
    let mut expected = Vec::new();
    for step in 0..7 {
        match step {
            1 => run.observation_pending = true,
            2 => run.observation_pending = false, // A -> B -> A is real progress.
            3 => {
                run.collection.completed_count = 1;
                run.collection.outstanding_count = 1;
            }
            4 => run.cancellation_requested = true,
            5 => run.capacity_released = true,
            6 => run.revision += 1,
            _ => {}
        }
        expected.push(crate::audit::progress(&run));
        app.audit_updated(thread, &run, None);
        app.audit_updated(thread, &run, None);
    }
    let shown = progress_messages(&history(&mut events));
    assert_eq!(shown.len(), expected.len());
    for (shown, expected) in shown.iter().zip(expected) {
        assert!(shown.contains(&expected), "{shown:?}");
    }
}

#[tokio::test]
async fn audit_progress_is_scoped_to_displayed_conversation_and_run() {
    let (mut app, mut events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let foreground = ThreadId::new();
    let background = ThreadId::new();
    display_thread(&mut app, foreground);
    let mut run = run_fixture();
    app.audit_updated(foreground, &run, None);
    assert_eq!(progress_messages(&history(&mut events)).len(), 1);
    app.audit_updated(background, &run, None);
    assert!(history(&mut events).is_empty());
    display_thread(&mut app, background);
    app.audit_updated(background, &run, None);
    assert_eq!(progress_messages(&history(&mut events)).len(), 1);
    run.observation_pending = true;
    app.audit_updated(foreground, &run, None); // Hidden changes must not count as displayed.
    assert!(history(&mut events).is_empty());
    display_thread(&mut app, foreground);
    app.audit_updated(foreground, &run, None);
    app.audit_updated(foreground, &run, None);
    assert_eq!(progress_messages(&history(&mut events)).len(), 1);
    run.run_id = "another-run".into();
    app.audit_updated(foreground, &run, None);
    app.audit_updated(foreground, &run, None);
    assert_eq!(
        history(&mut events)
            .iter()
            .filter(|text| text.contains("Audit another-run:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn repeated_audit_progress_keeps_errors_results_context_and_one_comparison() {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display_thread(&mut app, thread);
    let mut run = run_fixture();
    run.phase = codex_app_server_protocol::AuditRunPhase::Cancelled;
    run.collection.completed_count = 2;
    run.collection.outstanding_count = 0;
    run.collection.ready = true;
    run.report = Some("DIAGNOSTIC_SENTINEL".into());
    run.findings_summary = Some("COMPLETED_FINDING_SENTINEL; ABORTED_JOB_SENTINEL".into());
    run.outcome = Some("ABORT_OUTCOME_SENTINEL".into());
    app.audits
        .awaiting_comparison
        .insert((thread, run.run_id.clone()));
    app.audit_updated(thread, &run, None);
    let first_context = app.audits.reports.for_turn(thread).unwrap();
    app.audits.reports.acknowledged(thread);
    let progress = crate::audit::progress(&run);
    run.findings_summary = Some("REOPENED_FINDING_SENTINEL; ABORTED_JOB_SENTINEL".into());
    assert_eq!(crate::audit::progress(&run), progress);
    app.audit_updated(thread, &run, Some("OBSERVATION_ERROR_SENTINEL".into()));
    assert_eq!(app.audits.reports.for_turn(thread).unwrap(), first_context);
    let shown = history(&mut events);
    assert_eq!(progress_messages(&shown).len(), 1);
    let shown = shown.join("\n");
    for marker in [
        "COMPLETED_FINDING_SENTINEL",
        "REOPENED_FINDING_SENTINEL",
        "ABORTED_JOB_SENTINEL",
        "ABORT_OUTCOME_SENTINEL",
        "OBSERVATION_ERROR_SENTINEL",
    ] {
        assert!(shown.contains(marker), "missing {marker}");
    }
    assert!(!shown.contains("DIAGNOSTIC_SENTINEL"));
    assert_eq!(
        std::iter::from_fn(|| ops.try_recv().ok())
            .filter(|op| matches!(op, AppCommand::UserTurn { .. }))
            .count(),
        1
    );
}

// Read/observe transport only: any accidental review/turn request fails the match.
#[tokio::test]
async fn observer_delivers_invisible_updates_and_reopened_history_keeps_findings() {
    use crate::app_server_session::ThreadParamsMode;
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    use codex_app_server_protocol::ClientRequest;
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display_thread(&mut app, thread);
    let mut run = run_fixture();
    let mut reviewer = crate::audit::tests::attempt_fixture();
    reviewer.run_id = run.run_id.clone();
    run.reviewers.push(reviewer);
    let (client, mut commands, _incoming) = CloudClient::channel(8);
    let session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(client),
        ThreadParamsMode::Remote,
    );
    app.audit_opened(&session, thread, run.clone());
    let mut shown = history(&mut events);
    let key = (thread, run.run_id.clone());
    for step in 0..3 {
        let before = run.clone();
        if step < 2 {
            run.reviewers[0].output_sequence += 1;
            assert_ne!(run, before);
            assert_eq!(
                crate::audit::progress(&run),
                crate::audit::progress(&before)
            );
        } else {
            run.phase = codex_app_server_protocol::AuditRunPhase::Completed;
            run.collection.completed_count = 2;
            run.collection.outstanding_count = 0;
            run.collection.ready = true;
            run.report = Some("DIAGNOSTIC_SENTINEL".into());
            run.findings_summary = Some("HISTORICAL_FINDING_SENTINEL".into());
        }
        app.audits.watches[&key].1.notify_one();
        let command = tokio::time::timeout(Duration::from_secs(5), commands.recv())
            .await
            .unwrap()
            .unwrap();
        let CloudOperation::Request(request) = command.operation else {
            panic!("read request")
        };
        assert!(matches!(*request, ClientRequest::AuditRead { .. }));
        command.response.send(Ok(Ok(json!({"run":run})))).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let AppEvent::AuditUpdated {
            thread_id,
            run: updated,
            error,
        } = event
        else {
            panic!("observer update")
        };
        assert_eq!(updated, run); // Every full-record change still reaches the app.
        app.audit_updated(thread_id, &updated, error);
        shown.extend(history(&mut events));
    }
    app.audits.watches.remove(&key).unwrap().0.await.unwrap();
    assert_eq!(progress_messages(&shown).len(), 2); // Reviewing then Completed.
    assert_eq!(
        std::iter::from_fn(|| ops.try_recv().ok())
            .filter(|op| matches!(op, AppCommand::UserTurn { .. }))
            .count(),
        1
    );

    // Two explicit historical opens must each display findings, without new comparisons.
    for _ in 0..2 {
        app.audit_opened(&session, thread, run.clone());
        app.audits.watches.remove(&key).unwrap().0.await.unwrap();
        let AppEvent::AuditUpdated {
            thread_id,
            run,
            error,
        } = events.recv().await.unwrap()
        else {
            panic!("historical update")
        };
        app.audit_updated(thread_id, &run, error);
        let reread = history(&mut events);
        assert!(progress_messages(&reread).is_empty());
        assert!(reread.join("\n").contains("HISTORICAL_FINDING_SENTINEL"));
        assert!(
            !std::iter::from_fn(|| ops.try_recv().ok())
                .any(|op| matches!(op, AppCommand::UserTurn { .. }))
        );
    }
    assert!(commands.try_recv().is_err());
}

#[tokio::test]
async fn audit_observation_reconnect_keeps_progress_deduplication_and_errors() {
    use crate::app_server_session::ThreadParamsMode;
    use codex_app_server_client::{AppServerClient, CloudClient};
    let (mut app, mut events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    display_thread(&mut app, thread);
    let mut run = run_fixture();
    let (client, mut commands, _incoming) = CloudClient::channel(8);
    let session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(client),
        ThreadParamsMode::Remote,
    );
    app.audit_opened(&session, thread, run.clone());
    let mut shown = history(&mut events);
    let key = (thread, run.run_id.clone());
    app.audits.watches[&key].1.notify_one();
    // Losing the pending read models the observer's normal transport-disconnect path.
    drop(
        tokio::time::timeout(Duration::from_secs(5), commands.recv())
            .await
            .unwrap()
            .unwrap(),
    );
    app.audits.watches.remove(&key).unwrap().0.await.unwrap();
    let AppEvent::AuditUpdated {
        thread_id,
        run: updated,
        error,
    } = events.recv().await.unwrap()
    else {
        panic!("disconnect update")
    };
    assert!(
        error
            .as_ref()
            .unwrap()
            .contains("Audit observation disconnected")
    );
    app.audit_updated(thread_id, &updated, error);
    shown.extend(history(&mut events));

    // /audit read reconnects with a fresh handle and the same full-record progress.
    let (client, mut commands, _incoming) = CloudClient::channel(8);
    let session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(client),
        ThreadParamsMode::Remote,
    );
    app.audit_opened(&session, thread, run.clone());
    shown.extend(history(&mut events));
    run.collection.completed_count = 1;
    run.collection.outstanding_count = 1;
    app.audits.watches[&key].1.notify_one();
    let command = tokio::time::timeout(Duration::from_secs(5), commands.recv())
        .await
        .unwrap()
        .unwrap();
    command.response.send(Ok(Ok(json!({"run":run})))).unwrap();
    let AppEvent::AuditUpdated {
        thread_id,
        run: updated,
        error,
    } = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("reconnected update")
    };
    app.audit_updated(thread_id, &updated, error);
    shown.extend(history(&mut events));
    assert_eq!(progress_messages(&shown).len(), 2);
    assert!(shown.join("\n").contains("Audit observation disconnected"));
    let (task, _) = app.audits.watches.remove(&key).unwrap();
    task.abort();
    let _ = task.await;
}
