//! Audit controls reuse the existing picker, prompt editor and transcript.
use super::*;
#[cfg(test)]
use codex_app_server_protocol::AuditSource;
use codex_app_server_protocol::{AuditListResponse, AuditStartParams};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const AUDIT_ATTEMPT_CONFIRMATION_VIEW_ID: &str = "audit_attempt_control_confirmation";

impl ChatWidget {
    pub(crate) fn confirm_audit_release(
        &mut self,
        run: codex_app_server_protocol::AuditRunView,
        decision: oneshot::Sender<bool>,
    ) {
        let decision = Arc::new(Mutex::new(Some(decision)));
        let accept = Arc::clone(&decision);
        let decline = Arc::clone(&decision);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Release unresolved audit capacity?".to_owned()),
            subtitle: Some(format!("Run: {}\nThe original reviewer may still start or continue inference and incur charges. Releasing this slot permits concurrent execution and additional charges. Its outcome stays unresolved; observation continues and any valid late result is retained. The original operation will never be resubmitted or reacquire capacity. Cancellation intent and unresolved observation are required. This action is permanent.", run.run_id)),
            items: vec![
                SelectionItem { name: "No, retain the capacity reservation".to_owned(), actions: vec![Box::new(move |_| decide(&decline, false))], dismiss_on_select: true, ..Default::default() },
                SelectionItem { name: "Yes, acknowledge concurrent execution and charges".to_owned(), actions: vec![Box::new(move |_| decide(&accept, true))], dismiss_on_select: true, ..Default::default() },
            ],
            on_cancel: Some(Box::new(move |_| decide(&decision, false))),
            ..SelectionViewParams::confirmation()
        });
        self.request_redraw();
    }

    pub(crate) fn request_audit_command(&mut self, args: String) {
        if let Some(thread_id) = self.thread_id() {
            self.app_event_tx
                .send(AppEvent::AuditCommand { thread_id, args });
        } else {
            self.add_error_message(
                "Wait for the operator conversation to start before using /audit.".to_owned(),
            );
        }
    }

    pub(crate) fn show_audit_list(&mut self, page: AuditListResponse) {
        let mut items = vec![SelectionItem {
            name: "Start a source audit".to_owned(),
            description: Some(
                "Choose source and ordered reviewers; confirm routes and disclosure before inference"
                    .to_owned(),
            ),
            actions: vec![Box::new(|tx| tx.send(AppEvent::AuditPrompt))],
            dismiss_on_select: true,
            ..Default::default()
        }];
        if let Some(thread_id) = self.thread_id() {
            for run in page.runs {
                let id = run.run_id.clone();
                items.push(SelectionItem {
                    name: format!("{} — {:?}", run.run_id, run.phase),
                    description: Some(crate::audit::progress(&run)),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::AuditCommand {
                            thread_id,
                            args: format!("reviewers {id}"),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
            if let Some(cursor) = page.next_cursor {
                items.push(SelectionItem {
                    name: "Older audits".to_owned(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::AuditCommand {
                            thread_id,
                            args: format!("list --after {cursor}"),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
        }
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Durable audits".to_owned()),
            subtitle: Some("Select an audit to manage reviewers, observe or open its published report. /audit abort RUN stops the whole audit; cancel is its compatibility alias.".to_owned()),
            items, is_searchable: true, ..SelectionViewParams::picker()
        });
        self.request_redraw();
    }

    pub(crate) fn show_audit_prompt(&mut self) {
        let tx = self.app_event_tx.clone();
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        self.bottom_pane.show_text_prompt(CustomPromptView::new(
            "Start a source audit".to_owned(),
            "--file ./example.rs --reviewer PROVIDER:MODEL --reviewer PROVIDER:MODEL [--sequential], or --snippet 'code' --name example.rs [--model MODEL --provider PROVIDER]".to_owned(),
            "--file ".to_owned(),
            Some("Single-reviewer defaults use this conversation. Repeat --reviewer in order (duplicates stay distinct); reviewer JSON supports executionAdapter/authenticationRoute and reasoningEffort/expectedServedModel. Claude subscription requires an explicit route and qualified dedicated profile; API and Ollama remain unavailable. --audex-md/--agents-md select explicit context. Confirm before submission.".to_owned()),
            Box::new(move |args| tx.send(AppEvent::AuditCommand { thread_id, args })),
        ));
        self.request_redraw();
    }

    pub(crate) fn show_audit_attempts(
        &mut self,
        run_id: String,
        attempts: Vec<codex_app_server_protocol::AuditAttemptView>,
    ) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        let id = run_id.clone();
        let mut items = vec![SelectionItem {
            name: "Read/follow authoritative audit report".to_owned(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::AuditCommand {
                    thread_id,
                    args: format!("read {id}"),
                })
            })],
            dismiss_on_select: true,
            ..Default::default()
        }];
        for attempt in attempts {
            let target = attempt.clone();
            items.push(SelectionItem {
                name: format!(
                    "Reviewer {} — {} — attempt {}{}",
                    attempt.reviewer_ordinal + 1,
                    attempt.resolved_model,
                    attempt.attempt_ordinal,
                    if attempt.current {
                        " (current)"
                    } else {
                        " (history)"
                    }
                ),
                description: Some(format!(
                    "{} / {}; contribution={}; execution={}; Retry={}; assignment={}",
                    attempt.provider,
                    attempt.authentication_route,
                    attempt.disposition,
                    attempt.operation_state,
                    attempt.retry_eligible,
                    attempt.assignment_id
                )),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditAttemptSelected {
                        thread_id,
                        attempt: target.clone(),
                        watch: false,
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        let id = run_id.clone();
        items.push(SelectionItem {name:"Abort the whole audit…".to_owned(),description:Some("Separate from reviewer Cancel; available until terminal publication. Confirmation required.".to_owned()),
            actions:vec![Box::new(move|tx|tx.send(AppEvent::AuditCommand {thread_id,args:format!("abort {id}")}))],dismiss_on_select:true,..Default::default()});
        self.bottom_pane.show_selection_view(SelectionViewParams {title:Some(format!("Audit {run_id}: reviewers and attempts")),subtitle:Some("Select exact current or historical assignment. Viewing sends no inference or control.".to_owned()),items,is_searchable:true,..SelectionViewParams::picker()});
        self.request_redraw();
    }

    pub(crate) fn show_audit_attempt(
        &mut self,
        target: codex_app_server_protocol::AuditAttemptView,
    ) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        let activity_target = target.clone();
        let run = target.run_id.clone();
        let assignment = target.assignment_id.clone();
        let mut items = vec![
            SelectionItem {
                name: "View retained reviewer activity".to_owned(),
                description: Some(
                    "Scroll or follow this exact attempt; historical attempts remain readable."
                        .to_owned(),
                ),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditActivityOpen {
                        thread_id,
                        attempt: activity_target.clone(),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Observe / Wait for this exact attempt".to_owned(),
                description: Some(
                    "No inference, no control, and no following replacement attempts.".to_owned(),
                ),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditCommand {
                        thread_id,
                        args: format!("attempt wait {run} {assignment}"),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];
        for action in crate::audit::attempt_actions(&target) {
            let run = target.run_id.clone();
            let assignment = target.assignment_id.clone();
            let spelling = match action {
                codex_app_server_protocol::AuditAttemptAction::Cancel => "cancel",
                codex_app_server_protocol::AuditAttemptAction::Ignore => "ignore",
                codex_app_server_protocol::AuditAttemptAction::Retry => "retry",
            };
            items.push(SelectionItem {
                name: format!("{action:?} this reviewer attempt…"),
                description: Some("Opens one confirmation for this exact assignment.".to_owned()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditCommand {
                        thread_id,
                        args: format!("attempt {spelling} {run} {assignment}"),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        let run = target.run_id.clone();
        items.push(SelectionItem {
            name: "Abort the whole audit…".to_owned(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::AuditCommand {
                    thread_id,
                    args: format!("abort {run}"),
                })
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Reviewer attempt management".to_owned()),
            subtitle: Some(crate::audit::attempt_summary(&target)),
            items,
            ..SelectionViewParams::picker()
        });
        self.request_redraw();
    }

    pub(crate) fn confirm_audit_attempt(
        &mut self,
        confirmation: crate::audit::AttemptControlConfirmation,
        decision: oneshot::Sender<bool>,
    ) {
        self.confirm_audit_decision(
            format!(
                "{:?} this exact reviewer attempt?",
                confirmation.request.action
            ),
            confirmation.disclosure(),
            "Yes, authorise this reviewer control",
            decision,
            Some(AUDIT_ATTEMPT_CONFIRMATION_VIEW_ID),
        );
    }

    /// Route only chooser keys to the established confirmation. No composer fallback.
    pub(crate) fn handle_audit_activity_confirmation_key(&mut self, key: KeyEvent) {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
        if !self.audit_activity_confirmation_active() || key.kind != KeyEventKind::Press {
            return;
        }
        let chooser = key.modifiers.is_empty()
            && matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Enter
                    | KeyCode::Esc
            );
        let cancel = key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c');
        if chooser || cancel {
            self.bottom_pane.handle_key_event(key);
        }
    }

    pub(crate) fn audit_activity_confirmation_active(&self) -> bool {
        self.active_view_is(AUDIT_ATTEMPT_CONFIRMATION_VIEW_ID)
            && !self.bottom_pane.keymap_contexts().is_warnings()
    }

    pub(crate) fn dismiss_audit_activity_confirmation(&mut self) {
        self.bottom_pane
            .dismiss_view_by_id(AUDIT_ATTEMPT_CONFIRMATION_VIEW_ID);
    }

    pub(crate) fn render_audit_activity_confirmation(&self, area: Rect, buf: &mut Buffer) {
        if self.audit_activity_confirmation_active() {
            let height = self.bottom_pane.desired_height(area.width).min(area.height);
            let panel = Rect::new(area.x, area.bottom() - height, area.width, height);
            ratatui::widgets::Clear.render(panel, buf);
            self.bottom_pane.render(panel, buf);
        }
    }

    pub(crate) fn confirm_audit_abort(&mut self, run_id: String, decision: oneshot::Sender<bool>) {
        self.confirm_audit_decision("Abort the whole audit?".to_owned(),format!("Run: {run_id}\nAbort stops wanting every outstanding reviewer and prevents queued/future inference. Accepted completed reviews and eligible received partials are preserved in an aborted report. Stop acknowledgement does not wait for remote termination or publication. This authority remains until terminal publication; a report already published stays final. Closing the TUI or Ctrl-C merely disconnects and never authorises Abort."),"Yes, Abort the whole audit",decision,None);
    }

    fn confirm_audit_decision(
        &mut self,
        title: String,
        disclosure: String,
        accept_label: &str,
        decision: oneshot::Sender<bool>,
        view_id: Option<&'static str>,
    ) {
        self.add_info_message(disclosure.clone(), None);
        let decision = Arc::new(Mutex::new(Some(decision)));
        let accept = Arc::clone(&decision);
        let decline = Arc::clone(&decision);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(title),
            view_id,
            subtitle: Some(disclosure),
            items: vec![
                SelectionItem {
                    name: "No, return without changes".to_owned(),
                    actions: vec![Box::new(move |_| decide(&decline, false))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: accept_label.to_owned(),
                    actions: vec![Box::new(move |_| decide(&accept, true))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            on_cancel: Some(Box::new(move |_| decide(&decision, false))),
            ..SelectionViewParams::confirmation()
        });
        self.request_redraw();
    }

    pub(crate) fn confirm_audit(
        &mut self,
        params: AuditStartParams,
        decision: oneshot::Sender<bool>,
    ) {
        let decision = Arc::new(Mutex::new(Some(decision)));
        let accept = Arc::clone(&decision);
        let decline = Arc::clone(&decision);
        let disclosure = audit_start_disclosure(&params);
        self.add_info_message(disclosure.clone(),Some("Review the complete disclosure in conversation history before authorising submission.".to_owned()));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Submit this isolated audit?".to_owned()),
            subtitle: Some(disclosure),
            items: vec![
                SelectionItem {
                    name: "No, return to the conversation".to_owned(),
                    actions: vec![Box::new(move |_| decide(&decline, false))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Yes, submit this audit".to_owned(),
                    actions: vec![Box::new(move |_| decide(&accept, true))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            on_cancel: Some(Box::new(move |_| decide(&decision, false))),
            ..SelectionViewParams::confirmation()
        });
        self.request_redraw();
    }
}

fn audit_start_disclosure(params: &AuditStartParams) -> String {
    codex_app_server_client::audit::start_disclosure(params)
}

fn decide(decision: &Arc<Mutex<Option<oneshot::Sender<bool>>>>, value: bool) {
    if let Some(sender) = decision
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = sender.send(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};

    fn params() -> AuditStartParams {
        AuditStartParams {
            preflight: None,
            reviewers: None,
            scheduling: Default::default(),
            expected_served_model: None,
            request_key: "request".into(),
            source: AuditSource::Snippet {
                name: "x.rs".into(),
                content: "source".into(),
            },
            brief: "brief".into(),
            provider: Some("provider".into()),
            model: Some("model".into()),
            reasoning_effort: None,
            audex_guidance: None,
            agents_evidence: None,
        }
    }

    #[tokio::test]
    async fn capacity_release_requires_explicit_confirmation_and_read_selection_does_not_release() {
        for (keys, expected) in [
            (vec![KeyCode::Enter], false),
            (vec![KeyCode::Esc], false),
            (vec![KeyCode::Down, KeyCode::Enter], true),
        ] {
            let (mut chat, _, _, mut ops) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            let run=serde_json::from_value(serde_json::json!({"runId":"run", "snapshotId":"snapshot", "assignmentId":"assignment", "phase":"reviewing","operationState":"accepted","observationPending":true,"capacityReleased":false,"revision":2,"cancellationRequested":true,"outcome":null,"rawResult":null,"report":null})).unwrap();
            let (sender, receiver) = oneshot::channel();
            chat.confirm_audit_release(run, sender);
            let popup = crate::chatwidget::tests::render_bottom_popup(&chat, 100);
            assert!(popup.contains("concurrent execution") && popup.contains("charges"));
            for key in keys {
                chat.handle_key_event(KeyEvent::from(key));
            }
            assert_eq!(receiver.await.unwrap(), expected);
            assert!(ops.try_recv().is_err());
        }
        let (mut chat, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        let thread = ThreadId::new();
        chat.thread_id = Some(thread);
        let run=serde_json::from_value(serde_json::json!({"runId":"run", "snapshotId":"snapshot", "assignmentId":"assignment", "phase":"reviewing","operationState":"accepted","observationPending":true,"capacityReleased":false,"revision":2,"cancellationRequested":true,"outcome":null,"rawResult":null,"report":null})).unwrap();
        chat.show_audit_list(AuditListResponse {
            protocol_version: codex_app_server_protocol::AUDIT_PROTOCOL_VERSION,
            runs: vec![run],
            next_cursor: None,
        });
        chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|e| matches!(e,AppEvent::AuditCommand {args,..} if args=="reviewers run"))
        );
    }

    #[tokio::test]
    async fn audit_confirmation_defaults_to_no_and_requires_explicit_acceptance() {
        for (keys, expected) in [
            (vec![KeyCode::Enter], false),
            (vec![KeyCode::Esc], false),
            (vec![KeyCode::Down, KeyCode::Enter], true),
        ] {
            let (mut chat, _, _, mut ops) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            let permissions = chat.config.permissions.permission_profile().clone();
            let (tx, rx) = oneshot::channel();
            chat.confirm_audit(params(), tx);
            for key in keys {
                chat.handle_key_event(KeyEvent::from(key));
            }
            assert_eq!(rx.await.unwrap(), expected);
            assert_eq!(chat.config.permissions.permission_profile(), &permissions);
            assert!(
                ops.try_recv().is_err(),
                "confirmation never submits an operator model turn"
            );
        }
    }

    #[tokio::test]
    async fn audit_slash_dispatch_keeps_the_operator_thread_and_commands() {
        let (mut chat, _, mut events, mut ops) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        let id = ThreadId::new();
        chat.thread_id = Some(id);
        chat.handle_slash_command_with_args_dispatch(
            SlashCommand::Audit,
            "read run-1".into(),
            Vec::new(),
        );
        assert_eq!(chat.thread_id(), Some(id));
        assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(event, AppEvent::AuditCommand { thread_id, args } if thread_id == id && args == "read run-1")));
        assert!(ops.try_recv().is_err());
        for command in [
            SlashCommand::Audit,
            SlashCommand::Review,
            SlashCommand::Resume,
            SlashCommand::Model,
            SlashCommand::Permissions,
            SlashCommand::Mcp,
        ] {
            assert_eq!(command.command().parse::<SlashCommand>().unwrap(), command);
        }
        assert!(SlashCommand::Audit.available_during_task());
    }
}

#[cfg(test)]
mod management_tests {
    use super::*;
    use codex_app_server_protocol::AuditAttemptAction;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test]
    async fn consequential_controls_default_to_no_and_disclose_bound_identity_and_consequences() {
        for action in [
            None,
            Some(AuditAttemptAction::Cancel),
            Some(AuditAttemptAction::Ignore),
            Some(AuditAttemptAction::Retry),
        ] {
            for (keys, accepted) in [
                (vec![KeyEvent::from(KeyCode::Enter)], false),
                (vec![KeyEvent::from(KeyCode::Esc)], false),
                (
                    vec![KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)],
                    false,
                ),
                (
                    vec![
                        KeyEvent::from(KeyCode::Down),
                        KeyEvent::from(KeyCode::Enter),
                    ],
                    true,
                ),
            ] {
                let (mut chat, _, _, mut ops) =
                    crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
                let (sender, receiver) = oneshot::channel();
                if let Some(action) = action.clone() {
                    let confirmation = crate::audit::AttemptControlConfirmation::new(
                        crate::audit::tests::attempt_fixture(),
                        action.clone(),
                        Some("action-key".into()),
                    );
                    let details = confirmation.disclosure();
                    assert!(details.contains("assignment-original"));
                    assert!(details.contains("native-model"));
                    match action {
                        AuditAttemptAction::Cancel => assert!(
                            details.contains("finalise immediately")
                                && details.contains("does not reserve")
                        ),
                        AuditAttemptAction::Ignore => assert!(
                            details.contains("permanently")
                                && details.contains("finalise immediately")
                        ),
                        AuditAttemptAction::Retry => assert!(
                            details.contains("overlapping execution and additional charges")
                                && details.contains("reserves no eligibility")
                        ),
                    }
                    chat.confirm_audit_attempt(confirmation, sender);
                } else {
                    chat.confirm_audit_abort("whole-run".into(), sender);
                }
                let rendered = crate::chatwidget::tests::render_bottom_popup(&chat, 100);
                assert!(rendered.contains("No, return without changes"));
                for key in keys {
                    chat.handle_key_event(key);
                }
                assert_eq!(receiver.await.unwrap(), accepted);
                assert!(ops.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn roster_disclosure_and_attempt_picker_keep_order_and_exact_history_targets() {
        let selection = |model: &str| codex_app_server_protocol::AuditReviewerSelection {
            provider: "local-provider".into(),
            model: model.into(),
            expected_served_model: None,
            reasoning_effort: None,
            execution_adapter: Default::default(),
            authentication_route: Default::default(),
        };
        let params = AuditStartParams {
            preflight: None,
            request_key: "stable".into(),
            source: AuditSource::Snippet {
                name: "source.rs".into(),
                content: "source canary".into(),
            },
            brief: "brief canary".into(),
            provider: None,
            model: None,
            reasoning_effort: None,
            reviewers: Some(vec![
                selection("model-first"),
                selection("model-first"),
                selection("model-last"),
            ]),
            scheduling: codex_app_server_protocol::AuditScheduling::Sequential,
            expected_served_model: None,
            audex_guidance: Some("explicit guidance".into()),
            agents_evidence: Some("explicit conventions".into()),
        };
        let disclosure = audit_start_disclosure(&params);
        for text in [
            "1. model-first",
            "2. model-first",
            "3. model-last",
            "local-provider",
            "daemon_configured_native",
            "Sequential",
            "source.rs",
            "source canary",
            "brief canary",
            "explicit guidance",
            "explicit conventions",
            "every selected provider",
            "queued reviewers",
        ] {
            assert!(disclosure.contains(text), "{text}");
        }
        let (mut chat, _, mut events, mut ops) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        chat.thread_id = Some(ThreadId::new());
        let (decision, answer) = oneshot::channel();
        chat.confirm_audit(params, decision);
        let popup = crate::chatwidget::tests::render_bottom_popup(&chat, 120);
        for text in [
            "model-first",
            "model-last",
            "local-provider",
            "daemon_configured_native",
            "Sequential",
        ] {
            assert!(
                popup.contains(text),
                "rendered disclosure missing {text}: {popup}"
            );
        }
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        assert!(!answer.await.unwrap());
        assert!(ops.try_recv().is_err());
        let current = crate::audit::tests::attempt_fixture();
        let mut history = current.clone();
        history.current = false;
        history.assignment_id = "assignment-history".into();
        history.retry_eligible = false;
        chat.show_audit_attempts("run-1".into(), vec![current.clone(), history.clone()]);
        chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        let selected = std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|event| {
                if let AppEvent::AuditAttemptSelected { attempt, .. } = event {
                    Some(attempt)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(selected.assignment_id, "assignment-history");
        assert!(!selected.current);
        assert!(ops.try_recv().is_err());
        chat.show_audit_attempt(current);
        chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        assert!(std::iter::from_fn(||events.try_recv().ok()).any(|event|matches!(event,AppEvent::AuditCommand {args,..} if args=="attempt cancel run-1 assignment-original")));
        assert!(ops.try_recv().is_err());
    }
}
