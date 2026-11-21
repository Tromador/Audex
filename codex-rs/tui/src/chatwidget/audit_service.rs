//! Audit controls use the normal conversation picker and confirmation.
use super::*;
use codex_app_server_protocol::{AuditListPage, AuditStartRequest};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
impl ChatWidget {
    pub(crate) fn show_audit_report(&mut self, report: crate::audit::ReportPresentation) {
        let cwd = report.cwd.as_deref().unwrap_or(self.config.cwd.as_path());
        self.add_to_history(
            history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                report.markdown,
                cwd,
                None,
            ),
        );
    }
    pub(crate) fn discard_deleted_audit_comparison(&mut self, client_id: &str) {
        self.take_submitted_message(client_id);
        self.refresh_pending_input_preview();
        self.update_task_running_state();
        self.add_info_message(
            "The deleted audit's automatic comparison was skipped.".into(),
            None,
        );
        self.maybe_send_next_queued_input();
    }
    pub(crate) fn request_audit_command(&mut self, args: String) {
        if let Some(thread_id) = self.thread_id() {
            self.app_event_tx
                .send(AppEvent::AuditCommand { thread_id, args });
        } else {
            self.add_error_message(
                "Wait for the conversation to start before using /audit.".into(),
            );
        }
    }
    pub(crate) fn show_active_audit(
        &mut self,
        thread_id: ThreadId,
        run: &codex_app_server_protocol::AuditRun,
    ) {
        if self.thread_id() != Some(thread_id) {
            return;
        }
        let mut items = Vec::new();
        for item in crate::audit::menu::items(run) {
            let run_id = run.run_id.clone();
            items.push(SelectionItem {
                name: item.name,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditMenuAction {
                        thread_id,
                        run_id: run_id.clone(),
                        action: item.action.clone(),
                        attempt: item.attempt,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items.push(SelectionItem {
            name: "Audit history".into(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::AuditCommand {
                    thread_id,
                    args: "list".into(),
                })
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        items.push(SelectionItem {
            name: "Back to conversation".into(),
            dismiss_on_select: true,
            ..Default::default()
        });
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some("active-audit"),
            title: Some("Active audit".into()),
            subtitle: Some(crate::audit::history_details(run)),
            items,
            ..SelectionViewParams::picker()
        });
        self.request_redraw();
    }
    pub(crate) fn show_audit_list(&mut self, page: AuditListPage, active: Option<&str>) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        let initial_selected_idx = page
            .runs
            .iter()
            .position(|run| active == Some(run.run_id.as_str()));
        let mut items = Vec::new();
        for run in page.runs {
            let id = run.run_id.clone();
            let delete = run.clone();
            items.push(SelectionItem {
                name: crate::audit::history_label(&run),
                name_prefix_spans: if active == Some(run.run_id.as_str()) {
                    vec!["● ".into()]
                } else {
                    vec!["  ".into()]
                },
                description: Some(crate::audit::history_details(&run)),
                search_value: Some(format!(
                    "{} {}",
                    crate::audit::label(&run),
                    crate::audit::history_details(&run)
                )),
                secondary_action: Some(crate::bottom_pane::SelectionSecondaryAction {
                    keep_open: true,
                    key: crate::key_hint::plain(crossterm::event::KeyCode::Delete),
                    action: Box::new(move |tx| {
                        tx.send(AppEvent::AuditDeleteRequested {
                            thread_id,
                            run: delete.clone(),
                        })
                    }),
                    footer_hint: "enter select · delete remove audit · esc back".into(),
                }),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditCommand {
                        thread_id,
                        args: format!("read {id}"),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        if let Some(offset) = page.next_offset {
            items.push(SelectionItem {
                name: "Older audits".into(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::AuditCommand {
                        thread_id,
                        args: format!("list --offset {offset}"),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some("audit-history"),
            title: Some("Audit history".into()),
            subtitle: Some(
                if items.is_empty() {
                    "No retained audits. Esc returns to the conversation."
                } else {
                    "Enter selects for Codex and /claude · ● active audit"
                }
                .into(),
            ),
            items,
            is_searchable: true,
            initial_selected_idx,
            row_display: crate::bottom_pane::SelectionRowDisplay::SingleLine,
            // Keep details below the highlighted row at every terminal width.
            description_layout:
                crate::bottom_pane::SelectionDescriptionLayout::SelectedBelowWhenNarrow {
                    min_description_width: u16::MAX,
                },
            ..SelectionViewParams::picker()
        });
        self.request_redraw();
    }
    pub(crate) fn close_audit_history(&mut self) {
        self.bottom_pane.dismiss_view_by_id("audit-history");
    }
    pub(crate) fn confirm_audit_delete(&mut self, run: codex_app_server_protocol::AuditRun) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Delete this audit?".into()),
            subtitle: Some(format!(
                "{} — remove its retained reviews and records. Audited source is unaffected.",
                crate::audit::label(&run)
            )),
            items: vec![
                SelectionItem {
                    name: "No, keep this audit".into(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Yes, delete this audit".into(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::AuditDeleteConfirmed {
                            thread_id,
                            run_id: run.run_id.clone(),
                        })
                    })],
                    dismiss_on_select: true,
                    require_explicit_confirmation: true,
                    ..Default::default()
                },
            ],
            ..SelectionViewParams::confirmation()
        });
        self.request_redraw();
    }
    pub(crate) fn confirm_audit(
        &mut self,
        params: AuditStartRequest,
        decision: oneshot::Sender<bool>,
    ) {
        let decision = Arc::new(Mutex::new(Some(decision)));
        let accept = Arc::clone(&decision);
        let decline = Arc::clone(&decision);
        let disclosure = codex_app_server_client::audit::start_disclosure(&params);
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

    #[tokio::test]
    async fn history_rows_are_compact_with_selected_details_at_representative_widths() {
        let run: codex_app_server_protocol::AuditRun = serde_json::from_value(serde_json::json!({
            "runId":"00000000-0000-4000-8000-000000000001", "targets":["/a/long/project/path/to/target.rs"],
            "createdAt":"2026-11-19T12:00:00Z", "complete":true, "summary":null, "sourceNotice":"internal rules",
            "reviewers":(1..=2).map(|n| serde_json::json!({
                "reviewer":format!("reviewer-{n}"), "selection":{"route":"native","model":"fixture","effort":null},
                "attempt":1,"trigger":"initial","state":"succeeded","reason":null,"toolCalls":4,"findingCount":2,"hasOutput":true
            })).collect::<Vec<_>>()
        })).unwrap();
        for width in [60, 80, 145] {
            let (mut chat, _, _, _) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            chat.thread_id = Some(ThreadId::new());
            let mut other = run.clone();
            other.run_id = "00000000-0000-4000-8000-000000000002".into();
            other.targets = vec!["/another/project/other.rs".into()];
            other.reviewers[0].selection.model = "other-model".into();
            chat.show_audit_list(
                AuditListPage {
                    runs: vec![run.clone(), other],
                    next_offset: None,
                    presets: Default::default(),
                },
                Some(&run.run_id),
            );
            let history = crate::chatwidget::tests::render_bottom_popup(&chat, width)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                history.contains("19 Nov 2026 12:00 · completed"),
                "{history}"
            );
            assert!(history.contains("●"), "{history}");
            assert!(
                history.contains("Targets: /a/long/project/path/to/target.rs"),
                "{history}"
            );
            assert!(
                history.contains("reviewer-2 (native:fixture, succeeded)"),
                "{history}"
            );
            assert!(
                !history.contains("other-model"),
                "only highlighted audit details should expand: {history}"
            );
            assert!(
                !history.contains("tool calls") && !history.contains("Start an audit"),
                "{history}"
            );
            assert!(!history.contains(&run.run_id));
            chat.handle_key_event(KeyCode::Esc.into());
            chat.confirm_audit_delete(run.clone());
            let confirmation = crate::chatwidget::tests::render_bottom_popup(&chat, width)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                confirmation.contains("target.rs — remove"),
                "{confirmation}"
            );
            assert!(confirmation.contains("No, keep this audit"));
        }
    }

    #[tokio::test]
    async fn history_selection_and_delete_require_distinct_operator_actions() {
        let run: codex_app_server_protocol::AuditRun = serde_json::from_value(serde_json::json!({
            "runId":"00000000-0000-4000-8000-000000000001", "targets":["selected source"],
            "createdAt":"2026-11-19T12:00:00Z", "reviewers":[], "complete":true,
            "summary":"private retained review", "sourceNotice":"private rules"
        }))
        .unwrap();
        for key in [KeyCode::Enter, KeyCode::Delete] {
            let (mut chat, _, mut events, mut ops) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            chat.thread_id = Some(ThreadId::new());
            chat.show_audit_list(
                AuditListPage {
                    runs: vec![run.clone()],
                    next_offset: Some(20),
                    presets: Default::default(),
                },
                None,
            );
            assert!(
                events.try_recv().is_err(),
                "opening history performs no action"
            );
            chat.handle_key_event(key.into());
            let actions = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
            if key == KeyCode::Enter {
                assert!(actions.iter().any(|e| matches!(e, AppEvent::AuditCommand { args, .. } if args == &format!("read {}", run.run_id))));
            } else {
                assert!(actions.iter().any(|e| matches!(e, AppEvent::AuditDeleteRequested { run: selected, .. } if selected.run_id == run.run_id)));
                assert!(
                    !actions
                        .iter()
                        .any(|e| matches!(e, AppEvent::AuditDeleteConfirmed { .. }))
                );
            }
            assert!(
                ops.try_recv().is_err(),
                "navigation never submits inference"
            );
        }
        for (keys, deletes) in [
            (vec![KeyCode::Enter], false),
            (vec![KeyCode::Esc], false),
            (vec![KeyCode::Down, KeyCode::Enter], true),
        ] {
            let (mut chat, _, mut events, mut ops) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            chat.thread_id = Some(ThreadId::new());
            chat.confirm_audit_delete(run.clone());
            for key in keys {
                chat.handle_key_event(key.into());
            }
            let confirmed = std::iter::from_fn(|| events.try_recv().ok()).filter(|e| matches!(e, AppEvent::AuditDeleteConfirmed { run_id, .. } if run_id == &run.run_id)).count();
            assert_eq!(confirmed, usize::from(deletes));
            assert!(ops.try_recv().is_err());
        }
    }
    #[tokio::test]
    async fn history_search_pagination_active_default_and_empty_history_are_safe() {
        let run: codex_app_server_protocol::AuditRun = serde_json::from_value(serde_json::json!({
            "runId":"first", "targets":["/one/first.rs"], "createdAt":"2026-11-19T12:00:00Z",
            "reviewers":[], "complete":true, "summary":null, "sourceNotice":""
        }))
        .unwrap();
        let mut second = run.clone();
        second.run_id = "second".into();
        second.targets = vec!["/two/second.rs".into()];
        for (keys, active, expected) in [
            (vec![KeyCode::Enter], Some("second"), Some("read second")),
            (
                vec![KeyCode::Down, KeyCode::Down, KeyCode::Enter],
                None,
                Some("list --offset 20"),
            ),
            (
                "/two/"
                    .chars()
                    .map(KeyCode::Char)
                    .chain([KeyCode::Enter])
                    .collect(),
                None,
                Some("read second"),
            ),
            (vec![KeyCode::Esc], None, None),
        ] {
            let (mut chat, _, mut events, mut ops) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            chat.thread_id = Some(ThreadId::new());
            chat.show_audit_list(
                AuditListPage {
                    runs: vec![run.clone(), second.clone()],
                    next_offset: Some(20),
                    presets: Default::default(),
                },
                active,
            );
            for key in keys {
                chat.handle_key_event(key.into());
            }
            let actions = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
            let commands = actions
                .iter()
                .filter_map(|e| match e {
                    AppEvent::AuditCommand { args, .. } => Some(args.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(commands, expected.into_iter().collect::<Vec<_>>());
            assert!(ops.try_recv().is_err());
        }
        let (mut chat, _, mut events, mut ops) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        chat.thread_id = Some(ThreadId::new());
        chat.show_audit_list(
            AuditListPage {
                runs: vec![],
                next_offset: None,
                presets: Default::default(),
            },
            None,
        );
        assert!(
            crate::chatwidget::tests::render_bottom_popup(&chat, 80).contains("No retained audits")
        );
        chat.handle_key_event(KeyCode::Enter.into());
        assert!(events.try_recv().is_err() && ops.try_recv().is_err());
    }
    #[tokio::test]
    async fn declining_or_dismissing_deletion_restores_the_same_filtered_history_row() {
        for decline in [KeyCode::Enter, KeyCode::Esc] {
            let (mut chat, _, mut events, mut ops) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            chat.thread_id = Some(ThreadId::new());
            let first: codex_app_server_protocol::AuditRun = serde_json::from_value(serde_json::json!({
                "runId":"first", "targets":["/page-two/first.rs"], "createdAt":"2026-11-19T12:00:00Z",
                "reviewers":[], "complete":true, "summary":null, "sourceNotice":""
            })).unwrap();
            let mut second = first.clone();
            second.run_id = "second".into();
            second.targets = vec!["/page-two/second.rs".into()];
            chat.show_audit_list(
                AuditListPage {
                    runs: vec![first, second.clone()],
                    next_offset: Some(40),
                    presets: Default::default(),
                },
                None,
            );
            for c in "page-two".chars() {
                chat.handle_key_event(KeyCode::Char(c).into());
            }
            chat.handle_key_event(KeyCode::Down.into());
            let before = crate::chatwidget::tests::render_bottom_popup(&chat, 80);
            chat.handle_key_event(KeyCode::Delete.into());
            let AppEvent::AuditDeleteRequested { run, .. } = events.try_recv().unwrap() else {
                panic!()
            };
            assert_eq!(run.run_id, "second");
            chat.confirm_audit_delete(run);
            chat.handle_key_event(decline.into());
            assert_eq!(
                crate::chatwidget::tests::render_bottom_popup(&chat, 80),
                before
            );
            assert!(events.try_recv().is_err() && ops.try_recv().is_err());
            chat.handle_key_event(KeyCode::Enter.into());
            assert!(
                matches!(events.try_recv().unwrap(), AppEvent::AuditCommand { args, .. } if args == "read second")
            );
            assert_ne!(chat.bottom_pane.active_view_id(), Some("audit-history"));
        }
    }

    #[tokio::test]
    async fn accepted_deletion_can_close_parent_before_refreshing_history() {
        let (mut chat, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        chat.thread_id = Some(ThreadId::new());
        let run: codex_app_server_protocol::AuditRun = serde_json::from_value(serde_json::json!({
            "runId":"selected", "targets":["source"], "createdAt":"time", "reviewers":[], "complete":true, "summary":null, "sourceNotice":""
        })).unwrap();
        chat.show_audit_list(
            AuditListPage {
                runs: vec![run],
                next_offset: None,
                presets: Default::default(),
            },
            None,
        );
        chat.handle_key_event(KeyCode::Delete.into());
        let AppEvent::AuditDeleteRequested { run, .. } = events.try_recv().unwrap() else {
            panic!()
        };
        chat.confirm_audit_delete(run);
        chat.handle_key_event(KeyCode::Down.into());
        chat.handle_key_event(KeyCode::Enter.into());
        assert!(
            matches!(events.try_recv().unwrap(), AppEvent::AuditDeleteConfirmed { run_id, .. } if run_id == "selected")
        );
        chat.close_audit_history();
        assert_ne!(chat.bottom_pane.active_view_id(), Some("audit-history"));
    }

    #[tokio::test]
    async fn active_audit_rows_route_to_the_originating_thread_and_escape_is_quiet() {
        let run = serde_json::from_value(serde_json::json!({
            "runId":"hidden-run-id", "targets":["/history/source.rs"], "createdAt":"time",
            "complete":true, "summary":"private summary", "sourceNotice":"",
            "reviewers":[{"reviewer":"right", "selection":{"route":"native","model":"chosen"},
                "attempt":2, "trigger":"operator_retry", "state":"succeeded", "toolCalls":1,
                "hasOutput":true, "findingCount":0}]
        }))
        .unwrap();
        let (mut chat, _, mut events, mut ops) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        let thread = ThreadId::new();
        chat.thread_id = Some(thread);
        for key in [KeyCode::Esc, KeyCode::Enter] {
            chat.show_active_audit(thread, &run);
            let shown = crate::chatwidget::tests::render_bottom_popup(&chat, 100);
            assert!(
                shown.contains("source.rs") && shown.contains("chosen") && shown.contains("right")
            );
            assert!(!shown.contains("hidden-run-id") && !shown.contains("private summary"));
            chat.handle_key_event(key.into());
            if key == KeyCode::Enter {
                let AppEvent::AuditMenuAction {
                    thread_id,
                    run_id,
                    action,
                    ..
                } = events.try_recv().unwrap()
                else {
                    panic!()
                };
                assert_eq!(thread_id, thread);
                assert_eq!(run_id, run.run_id);
                let crate::audit::AuditAction::Read(params) = action else {
                    panic!()
                };
                assert_eq!(
                    params.view,
                    Some(codex_app_server_protocol::AuditReadView::Report)
                );
                assert_eq!(params.reviewer.as_deref(), Some("right"));
                assert_eq!(params.attempt, Some(2));
            }
            assert!(events.try_recv().is_err() && ops.try_recv().is_err());
        }
    }

    fn params() -> AuditStartRequest {
        AuditStartRequest {
            input: codex_app_server_protocol::AuditInput::Paths {
                paths: vec!["selected directory".into()],
            },
            cwd: "/source".into(),
            brief: "brief".into(),
            reviewers: vec![codex_app_server_protocol::AuditReviewer {
                route: "native".into(),
                model: "chosen".into(),
                effort: Some("high".into()),
            }],
            guidance: Some("AUDEX.md".into()),
            guidance_text: None,
        }
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
        chat.handle_slash_command_dispatch(SlashCommand::Audit);
        assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
            matches!(event, AppEvent::AuditCommand { thread_id, args } if thread_id == id && args.is_empty())
        }));
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
