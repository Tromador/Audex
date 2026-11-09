//! Audit observation and operator-authorised controls. Dropping UI state never stops durable work.
use super::*;
use codex_app_server_protocol::{AuditProgressNotification, AuditRunView};

#[derive(Default)]
pub(super) struct AuditUiState {
    watches: HashMap<(ThreadId, String), (JoinHandle<()>, Arc<tokio::sync::Notify>)>,
    attempt_watches:
        HashMap<(ThreadId, String, String), (JoinHandle<()>, Arc<tokio::sync::Notify>)>,
    pub(super) reports: crate::audit::ReportContexts,
    awaiting_comparison: std::collections::HashSet<(ThreadId, String)>,
    ready_comparisons: HashMap<ThreadId, Vec<String>>,
    // Last progress actually displayed, scoped to the audit's conversation. Hidden
    // updates and observer restarts must not advance or reset this presentation state.
    displayed_progress: HashMap<(ThreadId, String), String>,
    pub(super) activity_wake: Option<(String, Arc<tokio::sync::Notify>)>,
}

impl AuditUiState {
    pub(super) fn wake(&self, progress: &AuditProgressNotification) {
        if let Some((run, wake)) = &self.activity_wake
            && run == &progress.run_id
        {
            wake.notify_one();
        }
        for ((_, run, _), (_, wake)) in &self.attempt_watches {
            if run == &progress.run_id {
                wake.notify_one();
            }
        }
        for ((_, id), (_, wake)) in &self.watches {
            if id == &progress.run_id {
                wake.notify_one();
            }
        }
    }
}

impl Drop for AuditUiState {
    fn drop(&mut self) {
        // Only local observation is stopped; no audit/cancel is sent.
        for (task, _) in self.attempt_watches.values() {
            task.abort();
        }
        for (task, _) in self.watches.values() {
            task.abort();
        }
    }
}

impl App {
    pub(super) fn audit_available(&self, session: &AppServerSession) -> bool {
        !session.uses_embedded_app_server()
            && !crate::uses_remote_workspace_or_environment(
                &self.app_server_target,
                self.environment_manager.as_ref(),
            )
    }

    pub(super) fn audit_command(
        &mut self,
        session: &AppServerSession,
        thread_id: ThreadId,
        args: String,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        if !self.audit_available(session) {
            self.chat_widget.add_error_message("Durable audits require the local managed app-server daemon. Use `audex app-server daemon start` and reconnect; the operator conversation remains available.".to_owned());
            return;
        }
        let handle = session.request_handle();
        let events = self.app_event_tx.clone();
        let cwd = self.chat_widget.config_ref().cwd.to_path_buf();
        tokio::spawn(async move {
            let result = async {
                let action = crate::audit::parse_command(&args)?.action(&cwd).await?;
                crate::audit::execute(handle, thread_id, action, events.clone()).await
            }
            .await;
            if let Err(message) = result {
                events.send(AppEvent::AuditError { thread_id, message });
            }
        });
    }

    pub(super) fn audit_attempt_selected(
        &mut self,
        session: &AppServerSession,
        thread_id: ThreadId,
        attempt: codex_app_server_protocol::AuditAttemptView,
        watch: bool,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        self.chat_widget.show_audit_attempt(attempt.clone());
        if !watch {
            return;
        }
        let key = (
            thread_id,
            attempt.run_id.clone(),
            attempt.assignment_id.clone(),
        );
        if let Some((task, wake)) = self.audits.attempt_watches.get(&key)
            && !task.is_finished()
        {
            wake.notify_one();
            return;
        }
        let wake = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(crate::audit::observe_attempt(
            session.request_handle(),
            thread_id,
            attempt,
            self.app_event_tx.clone(),
            wake.clone(),
        ));
        self.audits.attempt_watches.insert(key, (task, wake));
    }

    pub(super) fn audit_opened(
        &mut self,
        session: &AppServerSession,
        thread_id: ThreadId,
        run: AuditRunView,
    ) {
        // Reopening an already completed historical run is a read, not another
        // automatic inference request. Observed live runs compare on completion.
        if !crate::audit::terminal(&run) {
            self.audits
                .awaiting_comparison
                .insert((thread_id, run.run_id.clone()));
        }
        if run.report.is_none() {
            self.audit_updated(thread_id, &run, None);
        }
        let key = (thread_id, run.run_id.clone());
        if let Some((task, wake)) = self.audits.watches.get(&key)
            && !task.is_finished()
        {
            wake.notify_one();
            return;
        }
        let wake = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(crate::audit::observe(
            session.request_handle(),
            thread_id,
            run,
            self.app_event_tx.clone(),
            Arc::clone(&wake),
        ));
        self.audits.watches.insert(key, (task, wake));
    }

    pub(super) fn audit_updated(
        &mut self,
        thread_id: ThreadId,
        run: &AuditRunView,
        error: Option<String>,
    ) {
        self.audits.reports.stage(thread_id, run);
        if crate::audit::terminal(run)
            && self
                .audits
                .awaiting_comparison
                .remove(&(thread_id, run.run_id.clone()))
            && run.collection.completed_count >= 2
        {
            self.audits
                .ready_comparisons
                .entry(thread_id)
                .or_default()
                .push(crate::audit::comparison_prompt(
                    &run.run_id,
                    &run.snapshot_id,
                ));
        }
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        let progress = crate::audit::progress(run);
        let key = (thread_id, run.run_id.clone());
        if self.audits.displayed_progress.get(&key) != Some(&progress) {
            self.audits.displayed_progress.insert(key, progress.clone());
            self.chat_widget
                .add_info_message(progress, run.report.as_ref().filter(|_|crate::audit::terminal(run)).map(|_| format!("A compact audit reference will accompany your next message. Ask about findings or their captured source; Codex can retrieve both. After restarting, reopen it with /audit read {}.", run.run_id)));
        }
        // Only progress is deduplicated. Explicit reads still show findings, and
        // every update must stage evidence, surface errors and drain ready comparisons.
        if let Some(report) = run
            .findings_summary
            .as_ref()
            .or(run.report.as_ref())
            .filter(|_| crate::audit::terminal(run))
        {
            // The full immutable report is displayed only by explicit diagnostics.
            let mut lines = Vec::new();
            crate::markdown::append_markdown(report, None, None, &mut lines);
            self.chat_widget.add_plain_history_lines(lines);
        }
        if let Some(error) = error {
            self.chat_widget.add_error_message(error);
        } else if let Some(outcome) = &run.outcome {
            self.chat_widget.add_info_message(outcome.clone(), None);
        }
        self.queue_ready_audit_comparisons(thread_id);
    }

    pub(super) fn queue_ready_audit_comparisons(&mut self, thread_id: ThreadId) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        if let Some(prompts) = self.audits.ready_comparisons.remove(&thread_id) {
            for prompt in prompts {
                // Reuse ordinary input queuing, including active-turn ordering,
                // rejection, uncertain delivery, cancellation and recovery.
                if !self.chat_widget.queue_internal_follow_up(prompt) {
                    self.chat_widget.add_error_message("The automatic audit comparison could not be queued. The findings remain available; ask Codex to compare them when ready.".into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server_session::ThreadParamsMode;
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    use codex_app_server_protocol::{AdditionalContextKind, ClientRequest, UserInput};
    use serde_json::json;

    fn completed_fixture() -> AuditRunView {
        serde_json::from_value(json!({
            "runId":"comparison-run", "snapshotId":"snapshot", "assignmentId":"assignment", "phase":"completed",
            "operationState":"committed", "observationPending":false, "capacityReleased":false, "revision":2, "cancellationRequested":false,
            "outcome":null, "rawResult":null, "report":"# DIAGNOSTIC_RECEIPT_SENTINEL",
            "findingsSummary":"# Findings\n\nSUMMARY_FINDING_SENTINEL",
            "collection":{"scheduling":"automatic","ready":true,"reviewerCount":2,"completedCount":2,"excludedCount":0,"outstandingCount":0}
        })).unwrap()
    }

    #[tokio::test]
    async fn completed_audit_shows_findings_and_submits_one_ordinary_comparison_turn() -> Result<()>
    {
        let (mut app, mut events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
        let thread = ThreadId::new();
        app.active_thread_id = Some(thread);
        app.primary_thread_id = Some(thread);
        app.chat_widget
            .handle_thread_session(crate::app::tests::test_thread_session(
                thread,
                app.config.cwd.to_path_buf(),
            ));
        let run = completed_fixture();
        let (client, mut commands, _incoming) = CloudClient::channel(1);
        let mut server = AppServerSession::new(
            crate::backend_id::BackendId::new(),
            AppServerClient::Cloud(client),
            ThreadParamsMode::Remote,
        );
        let mut pending = run.clone();
        pending.phase = codex_app_server_protocol::AuditRunPhase::Reviewing;
        pending.report = None;
        pending.findings_summary = None;
        app.audit_opened(&server, thread, pending);
        app.audit_updated(thread, &run, None);
        app.audit_updated(thread, &run, None);
        let op = std::iter::from_fn(|| ops.try_recv().ok())
            .find(|op| matches!(op, AppCommand::UserTurn { .. }))
            .expect("automatic normal turn");
        let AppCommand::UserTurn { items, .. } = &op else {
            unreachable!()
        };
        let prompt = serde_json::to_string(items)?;
        assert!(prompt.contains("Audex automatic completion comparison"));
        assert!(prompt.contains("EVERY original finding"));
        assert!(prompt.contains("audit_read") && prompt.contains("audit_source"));
        assert!(!prompt.contains("DIAGNOSTIC_RECEIPT_SENTINEL"));
        assert!(app.chat_widget.queued_user_message_texts().is_empty());
        let shown = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(
                    cell.display_lines(100)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("SUMMARY_FINDING_SENTINEL"));
        assert!(!shown.contains("DIAGNOSTIC_RECEIPT_SENTINEL"));
        let (result, ()) = tokio::join!(
            super::super::user_turn_submission::try_submit_user_turn(
                &mut app,
                &mut server,
                thread,
                &op
            ),
            async {
                let command = commands.recv().await.unwrap();
                let CloudOperation::Request(request) = command.operation else {
                    panic!("request")
                };
                let ClientRequest::TurnStart { params, .. } = *request else {
                    panic!("normal start; never a reviewer or steer")
                };
                assert!(
                    params
                        .additional_context
                        .unwrap()
                        .values()
                        .all(|entry| entry.kind == AdditionalContextKind::Untrusted)
                );
                command
                    .response
                    .send(Ok(Ok(
                        json!({"turn":{"id":"comparison", "items":[], "status":"inProgress"}}),
                    )))
                    .unwrap();
            }
        );
        assert!(result.is_ok());
        assert!(app.audits.reports.for_turn(thread).is_none());
        Ok(())
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
    async fn terminal_completion_policy_queues_only_two_or_more_completed_opinions() {
        for phase in ["completed", "completed_with_failures", "cancelled"] {
            for completed in 0..=3 {
                let (mut app, mut events, mut ops) =
                    crate::app::tests::make_test_app_with_channels().await;
                let thread = ThreadId::new();
                app.active_thread_id = Some(thread);
                app.primary_thread_id = Some(thread);
                app.chat_widget
                    .handle_thread_session(crate::app::tests::test_thread_session(
                        thread,
                        app.config.cwd.to_path_buf(),
                    ));
                let mut run = completed_fixture();
                run.phase = serde_json::from_value(json!(phase)).unwrap();
                run.collection.reviewer_count = 5;
                run.collection.completed_count = completed;
                run.collection.excluded_count = 5 - completed;
                run.findings_summary = Some(format!(
                    "Completed opinions: {completed}. Failed reviewer: failure-sentinel. Aborted reviewer: abort-sentinel."
                ));
                if phase == "cancelled" {
                    run.cancellation_requested = true;
                    run.outcome = Some("aborted_by_operator".into());
                }
                app.audits
                    .awaiting_comparison
                    .insert((thread, run.run_id.clone()));
                app.audit_updated(thread, &run, None);
                app.audit_updated(thread, &run, None);
                let turns = std::iter::from_fn(|| ops.try_recv().ok())
                    .filter_map(|op| match op {
                        AppCommand::UserTurn { items, .. } => {
                            Some(serde_json::to_string(&items).unwrap())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    turns.len(),
                    usize::from(completed >= 2),
                    "{phase}, {completed}"
                );
                if let Some(prompt) = turns.first() {
                    assert!(prompt.contains("Only reviewers with completed opinions participate"));
                    assert!(
                        prompt.contains("Explicitly identify every failed or aborted reviewer job")
                    );
                    assert!(prompt.contains("An overall audit abort does not suppress comparison"));
                    assert!(
                        prompt.contains("Never repeat inference to replace failed or aborted jobs")
                    );
                }
                let shown = std::iter::from_fn(|| events.try_recv().ok())
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
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(shown.contains("failure-sentinel"));
                assert!(shown.contains("abort-sentinel"));
                assert!(app.audits.awaiting_comparison.is_empty());
                assert!(app.audits.ready_comparisons.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn historical_failed_and_background_audits_do_not_start_unrequested_comparisons() {
        let (mut app, _events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
        let thread = ThreadId::new();
        app.active_thread_id = Some(thread);
        app.primary_thread_id = Some(thread);
        app.chat_widget
            .handle_thread_session(crate::app::tests::test_thread_session(
                thread,
                app.config.cwd.to_path_buf(),
            ));
        let mut run = completed_fixture();
        app.audit_updated(thread, &run, None); // Historical read, never armed.
        assert!(
            !std::iter::from_fn(|| ops.try_recv().ok())
                .any(|op| matches!(op, AppCommand::UserTurn { .. }))
        );
        app.audits
            .awaiting_comparison
            .insert((thread, run.run_id.clone()));
        run.collection.completed_count = 0;
        app.audit_updated(thread, &run, None);
        assert!(
            !std::iter::from_fn(|| ops.try_recv().ok())
                .any(|op| matches!(op, AppCommand::UserTurn { .. }))
        );
        run.collection.completed_count = 2;
        let background = ThreadId::new();
        app.audits
            .awaiting_comparison
            .insert((background, run.run_id.clone()));
        app.audit_updated(background, &run, None);
        assert!(
            !std::iter::from_fn(|| ops.try_recv().ok())
                .any(|op| matches!(op, AppCommand::UserTurn { .. }))
        );
        assert_eq!(app.audits.ready_comparisons[&background].len(), 1);
        app.audit_updated(background, &run, None);
        assert_eq!(app.audits.ready_comparisons[&background].len(), 1);
    }

    #[tokio::test]
    async fn audit_context_follows_explicit_start_and_steer_and_survives_rejection() -> Result<()> {
        for (steer, trigger, accepted) in [
            (false, "user", true),
            (true, "user", true),
            (false, "user", false),
            (true, "user", false),
            (false, "recap", true),
        ] {
            let (mut app, mut events, mut ops) =
                crate::app::tests::make_test_app_with_channels().await;
            let thread = ThreadId::new();
            app.active_thread_id = Some(thread);
            app.primary_thread_id = Some(thread);
            let session =
                crate::app::tests::test_thread_session(thread, app.config.cwd.to_path_buf());
            app.chat_widget.handle_thread_session(session);
            if steer {
                app.ensure_thread_channel(thread)
                    .store
                    .lock()
                    .await
                    .set_active_turn_id("active".into());
            }
            let run: AuditRunView = serde_json::from_value(json!({
                "runId":"report-run", "snapshotId":"snapshot", "assignmentId":"assignment", "phase":"completed",
                "operationState":"committed", "observationPending":false, "capacityReleased":false, "revision":1, "cancellationRequested":false,
                "outcome":null, "rawResult":null, "report":"# Retained report\n\n## Finding 1\nEvidence"
            }))?;
            app.audit_updated(thread, &run, None);
            let displayed = std::iter::from_fn(|| events.try_recv().ok())
                .filter_map(|event| match event {
                    AppEvent::InsertHistoryCell(cell) => Some(
                        cell.display_lines(100)
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                displayed.contains("Finding 1"),
                "reports display before any inference"
            );
            assert!(app.audits.reports.for_turn(ThreadId::new()).is_none());
            let op = AppCommand::UserTurn {
                turn_trigger: trigger,
                cloud_submission: crate::app_command::CloudSubmissionIntent::StartOrSteer,
                client_user_message_id: "operator-message".into(),
                items: vec![UserInput::Text {
                    text: "Discuss finding 1".into(),
                    text_elements: Vec::new(),
                }],
                cwd: app.config.cwd.to_path_buf(),
                approval_policy: codex_app_server_protocol::AskForApproval::Never,
                approvals_reviewer: None,
                active_permission_profile: None,
                model: "gpt-test".into(),
                effort: None,
                summary: None,
                service_tier: None,
                final_output_json_schema: None,
                collaboration_mode: None,
            };
            // Operator turns go through the composer so a rejection has a real draft to restore.
            let op = if trigger == "user" {
                app.chat_widget
                    .restore_user_message_to_composer("Discuss finding 1".to_string().into());
                app.chat_widget
                    .handle_key_event(crossterm::event::KeyCode::Enter.into());
                let submitted = std::iter::from_fn(|| ops.try_recv().ok())
                    .find(|op| matches!(op, AppCommand::UserTurn { .. }))
                    .expect("composer submits a user turn");
                assert!(matches!(
                    &submitted,
                    AppCommand::UserTurn {
                        cloud_submission: crate::app_command::CloudSubmissionIntent::StartOrSteer,
                        ..
                    }
                ));
                submitted
            } else {
                op
            };
            let (client, mut commands, _incoming) = CloudClient::channel(1);
            let mut server = AppServerSession::new(
                crate::backend_id::BackendId::new(),
                AppServerClient::Cloud(client),
                ThreadParamsMode::Remote,
            );
            let (result, ()) = tokio::join!(
                super::super::user_turn_submission::try_submit_user_turn(
                    &mut app,
                    &mut server,
                    thread,
                    &op
                ),
                async {
                    let command = commands.recv().await.unwrap();
                    let CloudOperation::Request(request) = command.operation else {
                        panic!("expected typed request")
                    };
                    let context = match *request {
                        ClientRequest::TurnStart { params, .. } => {
                            assert!(!steer);
                            params.additional_context
                        }
                        ClientRequest::TurnSteer { params, .. } => {
                            assert!(steer);
                            params.additional_context
                        }
                        _ => panic!("expected user-turn submission"),
                    };
                    if trigger == "user" {
                        let context = context.unwrap();
                        let entry = context.values().next().unwrap();
                        assert_eq!(entry.kind, AdditionalContextKind::Untrusted);
                        assert!(
                            entry.value.contains("report-run")
                                && entry.value.contains("audit_read")
                        );
                        assert!(!entry.value.contains("Finding 1"));
                    } else {
                        assert!(context.is_none());
                    }
                    let reply = if accepted {
                        Ok(if steer {
                            json!({"turnId":"active"})
                        } else {
                            json!({"turn":{"id":"turn", "items":[], "status":"inProgress"}})
                        })
                    } else {
                        Err(codex_app_server_protocol::JSONRPCErrorError {
                            code: -32602,
                            message: "rejected fixture input".into(),
                            data: None,
                        })
                    };
                    command.response.send(Ok(reply)).unwrap();
                }
            );
            // Rejection is handled, not returned. An unhandled failure would be Err here, and an
            // unconfirmed one would queue the message instead of restoring the draft.
            assert!(result.is_ok());
            if !accepted {
                let shown = std::iter::from_fn(|| events.try_recv().ok())
                    .filter_map(|event| match event {
                        AppEvent::InsertHistoryCell(cell) => Some(
                            cell.display_lines(100)
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(shown.contains("rejected fixture input"), "{shown}");
                assert_eq!(shown.contains("Failed to start turn"), !steer, "{shown}");
                assert_eq!(
                    app.chat_widget.composer_text_with_pending(),
                    "Discuss finding 1"
                );
                assert!(app.chat_widget.queued_user_message_texts().is_empty());
                let retained = app
                    .audits
                    .reports
                    .for_turn(thread)
                    .expect("report survives rejection");
                assert!(
                    retained
                        .values()
                        .any(|entry| entry.value.contains("report-run")
                            && entry.value.contains("audit_source"))
                );
            }
            assert_eq!(
                app.audits.reports.for_turn(thread).is_none(),
                accepted && trigger == "user"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "audit/progress_tests.rs"]
mod progress_tests;
