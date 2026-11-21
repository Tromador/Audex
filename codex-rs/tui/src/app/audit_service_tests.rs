use super::*;
use serde_json::json;
pub(super) fn completed_fixture() -> AuditRun {
    serde_json::from_value(json!({"runId":"00000000-0000-4000-8000-000000000001","targets":["source"],"createdAt":"time","complete":true,"summary":"Findings summary","sourceNotice":"Source may have changed","reviewers":(1..=2).map(|n| json!({"reviewer":format!("reviewer-{n}"),"selection":{"route":"native","model":"fixture","effort":null},"attempt":1,"trigger":"initial","state":"succeeded","reason":null,"toolCalls":0,"findingCount":0,"hasOutput":true})).collect::<Vec<_>>()})).unwrap()
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
                assert!(!sent[0].contains("Compare completed audit:"));
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
                assert!(sent[0].contains("Compare completed audit:"));
            } else {
                assert!(sent[0].contains("Compare completed audit:"));
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
                AppEvent::InsertHistoryCell(cell) => Some(
                    cell.display_lines(120)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(shown.matches("Findings summary").count(), 0);
        assert!(!shown.contains("Audex automatic completion comparison"));
        assert!(!shown.contains("Agreement is not verification"));
        if completed < 2 {
            assert!(shown.contains("failure sentinel"));
        }
        assert!(!shown.contains("Source may have changed"));
    }
}

#[tokio::test]
async fn historical_selection_switches_both_contexts_without_inference_and_deletion_clears_it() {
    let (mut app, _events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let a = ThreadId::new();
    let b = ThreadId::new();
    let (cloud, mut commands, _incoming) = codex_app_server_client::CloudClient::channel(1);
    let session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        codex_app_server_client::AppServerClient::Cloud(cloud),
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    let first = completed_fixture();
    let mut second = first.clone();
    second.run_id = "00000000-0000-4000-8000-000000000002".into();
    app.audit_opened(&session, a, first.clone(), false);
    app.audit_opened(&session, b, first.clone(), false);
    app.audit_opened(&session, a, second.clone(), false);
    app.show_ready_audits(a);
    assert_eq!(
        app.audits.reports.active_id(a),
        Some(second.run_id.as_str())
    );
    assert_eq!(app.audits.reports.active_id(b), Some(first.run_id.as_str()));
    let context = app.audits.reports.for_turn(a).unwrap();
    assert!(context["audex_active_audit"].value.contains(&second.run_id));
    assert!(!context["audex_active_audit"].value.contains(&first.run_id));
    assert_eq!(
        crate::claude::audit_context(None, false, app.audits.reports.active_id(a)),
        Some(second.run_id.clone())
    );
    assert!(ops.try_recv().is_err());
    assert!(commands.try_recv().is_err());
    app.forget_audit(&second.run_id);
    app.audit_updated(a, &second, None);
    app.audit_opened(&session, a, second.clone(), false);
    assert!(app.audits.reports.active_id(a).is_none());
    assert!(!app.audits.runs.contains_key(&(a, second.run_id.clone())));
    assert!(
        app.audits.reports.for_turn(a).unwrap()["audex_active_audit"]
            .value
            .contains("No audit is active")
    );
    assert_eq!(app.audits.reports.active_id(b), Some(first.run_id.as_str()));
}

#[tokio::test]
async fn automatic_comparison_sends_full_evidence_only_as_additional_context() {
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    use codex_app_server_protocol::ClientRequest;
    let (mut app, _events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    app.active_thread_id = Some(thread);
    app.primary_thread_id = Some(thread);
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            thread,
            app.config.cwd.to_path_buf(),
        ));
    while ops.try_recv().is_ok() {}
    let run = completed_fixture();
    app.audits
        .awaiting_comparison
        .insert((thread, run.run_id.clone()));
    app.audit_updated(thread, &run, None);
    let op = std::iter::from_fn(|| ops.try_recv().ok())
        .find(|op| matches!(op, AppCommand::UserTurn { .. }))
        .unwrap();
    let AppCommand::UserTurn {
        items,
        audit_comparison,
        ..
    } = &op
    else {
        unreachable!()
    };
    assert_eq!(audit_comparison.unwrap().to_string(), run.run_id);
    assert!(
        !serde_json::to_string(items)
            .unwrap()
            .contains("Findings summary")
    );
    assert_eq!(
        app.audits.reports.active_id(thread),
        Some(run.run_id.as_str())
    );
    let (cloud, mut commands, _incoming) = CloudClient::channel(4);
    let mut server = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(cloud),
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    let work = super::super::user_turn_submission::try_submit_user_turn(
        &mut app,
        &mut server,
        thread,
        &op,
    );
    let peer = async {
        let command = commands.recv().await.unwrap();
        let CloudOperation::Request(request) = command.operation else {
            panic!()
        };
        let ClientRequest::TurnStart { params, .. } = *request else {
            panic!("unexpected request: {request:?}")
        };
        let context = params.additional_context.unwrap();
        assert!(
            context["audex_comparison"]
                .value
                .contains("Findings summary")
        );
        assert!(
            context["audex_comparison"]
                .value
                .contains("one concise Codex synthesis")
        );
        assert!(
            !serde_json::to_string(&params.input)
                .unwrap()
                .contains("Findings summary")
        );
        command
            .response
            .send(Ok(Ok(
                json!({"turn":{"id":"comparison","status":"inProgress","items":[]}}),
            )))
            .unwrap();
    };
    let (result, ()) = tokio::join!(work, peer);
    assert!(result.unwrap());
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
async fn claude_wire_requests_follow_selection_model_override_and_opt_out() {
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    use codex_app_server_protocol::ClientRequest;
    let (mut app, _events, _ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let mut server = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(cloud),
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    for (args, selected, expected, model) in [
        ("question", None, None, None),
        ("question", Some("first"), Some("first"), None),
        (
            "--model chosen question",
            Some("first"),
            Some("first"),
            Some("chosen"),
        ),
        ("question", Some("second"), Some("second"), None),
        (
            "--audit explicit question",
            Some("second"),
            Some("explicit"),
            None,
        ),
        ("--no-audit question", Some("second"), None, None),
    ] {
        if let Some(selected) = selected {
            let mut run = completed_fixture();
            run.run_id = selected.into();
            app.audits.reports.stage(thread, &run);
        }
        let request = crate::claude::parse(args).unwrap();
        let op = AppCommand::ClaudeTurn {
            message: request.message,
            model: request.model,
            audit: request.audit,
            no_audit: request.no_audit,
        };
        let work = app.try_submit_active_thread_op_via_app_server(&mut server, thread, &op);
        let peer = async {
            let command = commands.recv().await.unwrap();
            let CloudOperation::Request(request) = command.operation else {
                panic!()
            };
            let ClientRequest::ThreadClaude { params, .. } = *request else {
                panic!()
            };
            assert_eq!(params.audit.as_deref(), expected);
            assert_eq!(params.model.as_deref(), model);
            assert_eq!(params.thread_id, thread.to_string());
            assert_eq!(params.message, "question");
            command
                .response
                .send(Ok(Ok(json!({"turnId":"fixture","model":"chosen"}))))
                .unwrap();
        };
        let (result, ()) = tokio::join!(work, peer);
        assert!(result.unwrap());
    }
}

#[tokio::test]
async fn deleted_audit_comparison_is_skipped_without_losing_the_operator_draft() {
    use codex_app_server_client::{AppServerClient, CloudClient};
    let (mut app, _events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let thread = ThreadId::new();
    app.active_thread_id = Some(thread);
    app.primary_thread_id = Some(thread);
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            thread,
            app.config.cwd.to_path_buf(),
        ));
    app.chat_widget
        .restore_user_message_to_composer("Keep my draft".into());
    while ops.try_recv().is_ok() {}
    let run = completed_fixture();
    app.audits
        .awaiting_comparison
        .insert((thread, run.run_id.clone()));
    app.audit_updated(thread, &run, None);
    let op = std::iter::from_fn(|| ops.try_recv().ok())
        .find(|op| matches!(op, AppCommand::UserTurn { .. }))
        .unwrap();
    app.forget_audit(&run.run_id);
    let (cloud, mut commands, _incoming) = CloudClient::channel(1);
    let mut server = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        AppServerClient::Cloud(cloud),
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    assert!(
        super::super::user_turn_submission::try_submit_user_turn(
            &mut app,
            &mut server,
            thread,
            &op
        )
        .await
        .unwrap()
    );
    assert!(commands.try_recv().is_err());
    assert!(!app.chat_widget.is_user_turn_pending_or_running());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "Keep my draft"
    );
}

#[tokio::test]
async fn background_completion_waits_then_queues_only_one_synthesis_on_return() {
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
    assert_eq!(summaries, 0);
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

#[tokio::test]
async fn direct_audit_reports_stay_with_their_thread_and_preserve_markdown() {
    let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
    let visible = ThreadId::new();
    let background = ThreadId::new();
    app.active_thread_id = Some(visible);
    app.primary_thread_id = Some(visible);
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            visible,
            app.config.cwd.to_path_buf(),
        ));
    while events.try_recv().is_ok() {}
    while ops.try_recv().is_ok() {}
    let original = "# Review — reviewer-2\n\n## Summary\n\nThese are ORIGINAL words.\n\n## Finding 1: Keep this typo: recieved\n\n**Evidence:**\n\n`value == 7` — unchanged.\n";
    app.enqueue_audit_report(
        background,
        crate::audit::ReportPresentation {
            markdown: original.into(),
            cwd: Some("/historical/source".into()),
        },
    )
    .await;
    assert!(
        events.try_recv().is_err(),
        "background reports cannot leak into the displayed thread"
    );
    let buffered = {
        let mut store = app.thread_event_channels[&background].store.lock().await;
        store.rebase_buffer_after_session_refresh();
        assert_eq!(store.buffer.len(), 1);
        store.buffer.back().unwrap().clone()
    };
    let mut refreshed = buffered.clone();
    assert!(
        super::super::thread_events::ThreadEventStore::event_survives_session_refresh(
            &mut refreshed
        )
    );
    app.active_thread_id = Some(background);
    app.handle_thread_event_replay(buffered);
    let cells = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(cell),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].copy_source(), Some(original));
    let rendered = cells[0]
        .display_lines(120)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("These are ORIGINAL words.") && rendered.contains("recieved"));
    assert!(
        ops.try_recv().is_err(),
        "display and replay never submit inference"
    );
}

#[test]
fn menu_actions_follow_latest_attempts_even_with_identical_reviewer_models() {
    use crate::audit::{AuditAction, menu};
    use codex_app_server_protocol::AuditReadView;
    let mut run = completed_fixture();
    // The earlier successful review stays readable by explicit attempt.
    let mut next = run.reviewers[1].clone();
    next.attempt = 2;
    next.finding_count = None;
    run.reviewers.push(next);
    for state in [
        "queued",
        "running",
        "failed",
        "cancelled",
        "interrupted",
        "succeeded",
    ] {
        for retained in [true, false] {
            run.reviewers[2].state = state.into();
            run.reviewers[2].has_output = retained;
            let items = menu::items(&run);
            let live = matches!(state, "queued" | "running");
            let unsuccessful = !live && state != "succeeded";
            let count = |action: fn(&AuditAction) -> bool| {
                items.iter().filter(|item| action(&item.action)).count()
            };
            assert_eq!(
                count(|a| matches!(a, AuditAction::Cancel(_))),
                if live { 2 } else { 0 }
            );
            assert_eq!(
                count(|a| matches!(a, AuditAction::Retry(_))),
                usize::from(unsuccessful)
            );
            assert_eq!(
                count(|a| matches!(a, AuditAction::Proceed(_))),
                usize::from(unsuccessful)
            );
            assert_eq!(
                count(|a| matches!(a, AuditAction::Read(_))),
                1 + usize::from(retained || state == "succeeded")
            );
            for item in items {
                assert!(!item.name.contains(&run.run_id));
                match item.action {
                    AuditAction::Read(params) => {
                        assert_eq!(params.view, Some(AuditReadView::Report));
                        let reviewer = params.reviewer.unwrap();
                        assert!(item.name.contains(&reviewer));
                        assert_eq!(
                            params.attempt,
                            Some(if reviewer == "reviewer-1" { 1 } else { 2 })
                        );
                    }
                    AuditAction::Cancel(params) => {
                        assert_eq!(params.run_id, run.run_id);
                        if let Some(reviewer) = params.reviewer {
                            assert_eq!(reviewer, "reviewer-2");
                            assert_eq!(item.attempt, Some(2));
                        }
                    }
                    AuditAction::Retry(params) => {
                        assert_eq!(params.run_id, run.run_id);
                        assert_eq!(params.reviewer, "reviewer-2");
                        assert_eq!(item.attempt, Some(2));
                    }
                    AuditAction::Proceed(params) => {
                        assert_eq!(params.reviewer.as_deref(), Some("reviewer-1"))
                    }
                    _ => panic!("unexpected reviewer action"),
                }
            }
        }
    }
}
