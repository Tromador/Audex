//! Audit controls reuse the existing picker, prompt editor and transcript.
use super::*;
use codex_app_server_protocol::{AuditListResponse, AuditSource, AuditStartParams};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

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
                "Choose file/snippet, model and provider; confirm before inference".to_owned(),
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
                            args: format!("read {id}"),
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
            subtitle: Some("Open a report for display and context on your next message. /audit cancel RUN requests cancellation.".to_owned()),
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
            "--file ./example.rs [--model MODEL --provider PROVIDER] or --snippet 'code' --name example.rs".to_owned(),
            "--file ".to_owned(),
            Some("Defaults to this conversation's model/provider. Optional --audex-md and --agents-md are explicit context only.".to_owned()),
            Box::new(move |args| tx.send(AppEvent::AuditCommand { thread_id, args })),
        ));
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
        let source = match &params.source {
            AuditSource::File { name, path } => format!("file {path} (snapshot name {name})"),
            AuditSource::Snippet { name, content } => {
                format!("snippet {name} ({} UTF-8 bytes)", content.len())
            }
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Submit this isolated audit?".to_owned()),
            subtitle: Some(format!("Source: {source}\nReviewer: {} / {}\nBrief: {}\nAUDEX guidance: {}; AGENTS evidence: {}\nSelected source and explicit context may be disclosed to this provider. A retained immutable snapshot will be created. Operator tools and history are not inherited.\nRequest key: {}", params.model, params.provider, params.brief,
                params.audex_guidance.as_ref().map_or(0, String::len), params.agents_evidence.as_ref().map_or(0, String::len), params.request_key)),
            items: vec![
                SelectionItem { name: "No, return to the conversation".to_owned(), actions: vec![Box::new(move |_| decide(&decline, false))], dismiss_on_select: true, ..Default::default() },
                SelectionItem { name: "Yes, submit this audit".to_owned(), actions: vec![Box::new(move |_| decide(&accept, true))], dismiss_on_select: true, ..Default::default() },
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

    fn params() -> AuditStartParams {
        AuditStartParams {
            request_key: "request".into(),
            source: AuditSource::Snippet {
                name: "x.rs".into(),
                content: "source".into(),
            },
            brief: "brief".into(),
            provider: "provider".into(),
            model: "model".into(),
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
                .any(|e| matches!(e,AppEvent::AuditCommand {args,..} if args=="read run"))
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
