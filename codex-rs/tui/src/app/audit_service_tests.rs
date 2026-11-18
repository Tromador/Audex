use super::*;
use serde_json::json;
pub(super) fn completed_fixture() -> AuditRun {
    serde_json::from_value(json!({"runId":"comparison-run","targets":["source"],"createdAt":"time","complete":true,"summary":"Findings summary","sourceNotice":"Source may have changed","reviewers":(1..=2).map(|n| json!({"reviewer":format!("reviewer-{n}"),"selection":{"route":"native","model":"fixture","effort":null},"attempt":1,"trigger":"initial","state":"succeeded","reason":null,"toolCalls":0,"findingCount":0,"hasOutput":true})).collect::<Vec<_>>()})).unwrap()
}
#[tokio::test]
async fn automatic_comparison_waits_for_the_active_turn_and_preserves_the_draft() {
    let (mut app, _events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    app.active_thread_id = Some(thread);
    app.primary_thread_id = Some(thread);
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            thread,
            app.config.cwd.to_path_buf(),
        ));
    app.chat_widget.handle_server_notification(serde_json::from_value(json!({"method":"turn/started","params":{"threadId":thread.to_string(),"turn":{"id":"active","status":"inProgress","items":[]}}})).unwrap(), None);
    app.chat_widget
        .restore_user_message_to_composer("Operator draft".into());
    while ops.try_recv().is_ok() {}
    let run = completed_fixture();
    app.audits
        .awaiting_comparison
        .insert((thread, run.run_id.clone()));
    app.audit_updated(thread, &run, None);
    assert!(
        !std::iter::from_fn(|| ops.try_recv().ok())
            .any(|op| matches!(op, AppCommand::UserTurn { .. }))
    );
    assert_eq!(app.chat_widget.queued_user_message_texts().len(), 1);
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "Operator draft"
    );
    app.chat_widget.handle_server_notification(serde_json::from_value(json!({"method":"turn/completed","params":{"threadId":thread.to_string(),"turn":{"id":"active","status":"completed","items":[]}}})).unwrap(), None);
    assert!(
        std::iter::from_fn(|| ops.try_recv().ok())
            .any(|op| matches!(op, AppCommand::UserTurn { .. }))
    );
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "Operator draft"
    );
}

#[tokio::test]
async fn audit_completion_preserves_pending_question_answer_and_normal_queue_order() {
    use codex_app_server_protocol::{
        ItemCompletedNotification, ServerNotification, ThreadItemPayload,
    };
    use codex_protocol::items::AsyncUserInputQuestion;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    for collapsed in [false, true] {
        for earlier_queued in [false, true] {
            let (mut app, _events, mut ops) =
                crate::app::tests::make_test_app_with_channels().await;
            let thread = ThreadId::new();
            app.active_thread_id = Some(thread);
            app.primary_thread_id = Some(thread);
            app.chat_widget
                .handle_thread_session(crate::app::tests::test_thread_session(
                    thread,
                    app.config.cwd.to_path_buf(),
                ));
            app.chat_widget.handle_server_notification(serde_json::from_value(json!({"method":"turn/started","params":{"threadId":thread.to_string(),"turn":{"id":"active","status":"inProgress","items":[]}}})).unwrap(), None);
            if earlier_queued {
                assert!(app.chat_widget.queue_user_message_with_options(
                    "Earlier operator message".into(),
                    crate::bottom_pane::QueuedInputAction::Literal,
                    Vec::new(),
                ));
            }
            app.chat_widget
                .restore_user_message_to_composer("Operator draft".into());
            app.chat_widget.handle_server_notification(
                ServerNotification::ItemCompleted(ItemCompletedNotification::new(
                    thread.to_string(),
                    "active".into(),
                    ThreadItemPayload::AgentMessage {
                        id: "question".into(),
                        text: String::new(),
                        phase: None,
                        moderation: None,
                        memory_citation: None,
                        delivery: None,
                        questions: Some(vec![AsyncUserInputQuestion {
                            title: "Which environment?".into(),
                            options: None,
                        }]),
                    },
                    None,
                    0,
                )),
                None,
            );
            app.chat_widget
                .handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
            app.chat_widget.handle_paste("Partly entered answer".into());
            if collapsed {
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
            }
            let questions = app
                .chat_widget
                .capture_thread_input_state()
                .unwrap()
                .questions;
            assert!(format!("{questions:?}").contains("Partly entered answer"));
            while ops.try_recv().is_ok() {}
            let run = completed_fixture();
            app.audits
                .awaiting_comparison
                .insert((thread, run.run_id.clone()));
            app.audit_updated(thread, &run, None);
            app.audit_updated(thread, &run, None);
            assert_eq!(
                app.chat_widget
                    .capture_thread_input_state()
                    .unwrap()
                    .questions,
                questions
            );
            assert_eq!(
                app.chat_widget.composer_text_with_pending(),
                "Operator draft"
            );
            assert_eq!(
                app.chat_widget.queued_user_message_texts().len(),
                1 + usize::from(earlier_queued)
            );
            assert!(
                !std::iter::from_fn(|| ops.try_recv().ok())
                    .any(|op| matches!(op, AppCommand::UserTurn { .. }))
            );

            app.chat_widget.handle_server_notification(serde_json::from_value(json!({"method":"turn/completed","params":{"threadId":thread.to_string(),"turn":{"id":"active","status":"completed","items":[]}}})).unwrap(), None);
            let recovered = "Operator draft\n\n> Which environment?\n\nPartly entered answer";
            assert_eq!(app.chat_widget.composer_text_with_pending(), recovered);
            let sent = std::iter::from_fn(|| ops.try_recv().ok())
                .filter_map(|op| match op {
                    AppCommand::UserTurn { items, .. } => {
                        Some(serde_json::to_string(&items).unwrap())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(sent.len(), 1);
            if earlier_queued {
                assert!(sent[0].contains("Earlier operator message"));
                assert!(!sent[0].contains("Audex automatic completion comparison"));
                app.chat_widget.handle_server_notification(serde_json::from_value(json!({"method":"turn/started","params":{"threadId":thread.to_string(),"turn":{"id":"earlier","status":"inProgress","items":[]}}})).unwrap(), None);
                app.chat_widget.handle_server_notification(serde_json::from_value(json!({"method":"turn/completed","params":{"threadId":thread.to_string(),"turn":{"id":"earlier","status":"completed","items":[]}}})).unwrap(), None);
                let sent = std::iter::from_fn(|| ops.try_recv().ok())
                    .filter_map(|op| match op {
                        AppCommand::UserTurn { items, .. } => {
                            Some(serde_json::to_string(&items).unwrap())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(sent.len(), 1);
                assert!(sent[0].contains("Audex automatic completion comparison"));
            } else {
                assert!(sent[0].contains("Audex automatic completion comparison"));
            }
            assert!(app.chat_widget.queued_user_message_texts().is_empty());
            assert_eq!(app.chat_widget.composer_text_with_pending(), recovered);
        }
    }
}

#[tokio::test]
async fn completion_policy_summary_failed_reviewers_and_no_duplicate_comparison() {
    for completed in 0..=2 {
        let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
        let thread = ThreadId::new();
        app.active_thread_id = Some(thread);
        app.primary_thread_id = Some(thread);
        app.chat_widget
            .handle_thread_session(crate::app::tests::test_thread_session(
                thread,
                app.config.cwd.to_path_buf(),
            ));
        let mut run = completed_fixture();
        for status in run.reviewers.iter_mut().skip(completed) {
            status.state = "failed".into();
            status.reason = Some("failure sentinel".into());
        }
        app.audits
            .awaiting_comparison
            .insert((thread, run.run_id.clone()));
        app.audit_updated(thread, &run, None);
        app.audit_updated(thread, &run, None);
        let turns = std::iter::from_fn(|| ops.try_recv().ok())
            .filter(|op| matches!(op, AppCommand::UserTurn { .. }))
            .count();
        assert_eq!(turns, usize::from(completed >= 2));
        let shown = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|e| match e {
                AppEvent::InsertHistoryCell(cell)
                    if !cell.as_any().is::<crate::history_cell::UserHistoryCell>() =>
                {
                    Some(
                        cell.display_lines(120)
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(shown.matches("Findings summary").count(), 1);
        if completed < 2 {
            assert!(shown.contains("failure sentinel"));
        }
        assert!(shown.contains("Source may have changed"));
    }
}

#[test]
fn stale_reads_do_not_undo_completion_or_hide_retries() {
    let current = completed_fixture();
    let mut stale = current.clone();
    stale.complete = false;
    stale.summary = None;
    stale.reviewers[0].state = "running".into();
    assert_eq!(merge_read(Some(&current), &stale), current);
    let mut retry = current.clone();
    retry.complete = false;
    retry.summary = None;
    let mut attempt = retry.reviewers[0].clone();
    attempt.attempt = 2;
    attempt.trigger = "operator_retry".into();
    attempt.state = "queued".into();
    retry.reviewers.push(attempt);
    let merged = merge_read(Some(&retry), &current);
    assert!(!merged.complete);
    assert_eq!(merged.reviewers.len(), 3);
}

#[tokio::test]
async fn background_completion_waits_then_shows_summary_once_on_return() {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    app.active_thread_id = Some(ThreadId::new());
    let run = completed_fixture();
    app.audits
        .awaiting_comparison
        .insert((thread, run.run_id.clone()));
    while events.try_recv().is_ok() {}
    while ops.try_recv().is_ok() {}
    app.audit_updated(thread, &run, None);
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|e| matches!(e, AppEvent::InsertHistoryCell(_)))
    );
    assert!(ops.try_recv().is_err());
    assert_eq!(app.audits.ready_comparisons[&thread].len(), 1);
    app.active_thread_id = Some(thread);
    app.primary_thread_id = Some(thread);
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            thread,
            app.config.cwd.to_path_buf(),
        ));
    app.show_ready_audits(thread);
    app.show_ready_audits(thread);
    let summaries = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|e| match e {
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<crate::history_cell::PlainHistoryCell>() =>
            {
                cell.display_lines(120)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .contains("Findings summary")
            }
            _ => false,
        })
        .count();
    assert_eq!(summaries, 1);
    assert_eq!(
        std::iter::from_fn(|| ops.try_recv().ok())
            .filter(|op| matches!(op, AppCommand::UserTurn { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn s4_f3_summary_error_is_visible_in_the_foreground() {
    super::correction_tests::assert_summary_error_visible(false).await;
}

#[tokio::test]
async fn s4_f3_background_summary_error_is_visible_on_return() {
    super::correction_tests::assert_summary_error_visible(true).await;
}
