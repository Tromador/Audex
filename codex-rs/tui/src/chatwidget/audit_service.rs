//! Audit controls use the normal conversation picker and confirmation.
use super::*;
use codex_app_server_protocol::{AuditListPage, AuditStartRequest};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
impl ChatWidget {
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
    pub(crate) fn show_audit_prompt(&mut self) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        self.bottom_pane.show_text_prompt(CustomPromptView::new(
            "Start an audit".into(), "<path>… [--with PRESET] [--reviewer ROUTE:MODEL[:EFFORT]] [--brief TEXT] [--guidance FILE]".into(), String::new(),
            Some("Pasted input: --snippet TEXT --name FILENAME. One disclosure precedes submission.".into()),
            Box::new(move |args| tx.send(AppEvent::AuditCommand { thread_id, args }))));
        self.request_redraw();
    }
    pub(crate) fn show_audit_list(&mut self, page: AuditListPage) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        let mut items = vec![SelectionItem {
            name: "Start an audit".into(),
            actions: vec![Box::new(|tx| tx.send(AppEvent::AuditPrompt))],
            dismiss_on_select: true,
            ..Default::default()
        }];
        for run in page.runs {
            let id = run.run_id.clone();
            items.push(SelectionItem {
                name: format!("{} — {}", run.created_at, run.targets.join(", ")),
                description: Some(crate::audit::progress(&run)),
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
            title: Some("Audits".into()),
            items,
            is_searchable: true,
            ..SelectionViewParams::picker()
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
