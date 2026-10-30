//! Audit UI state is observational only. Dropping it never cancels durable work.
use super::*;
use codex_app_server_protocol::{AuditProgressNotification, AuditRunView};

#[derive(Default)]
pub(super) struct AuditUiState {
    watches: HashMap<(ThreadId, String), (JoinHandle<()>, Arc<tokio::sync::Notify>)>,
    pub(super) reports: crate::audit::ReportContexts,
}

impl AuditUiState {
    pub(super) fn wake(&self, progress: &AuditProgressNotification) {
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

    pub(super) fn audit_opened(
        &mut self,
        session: &AppServerSession,
        thread_id: ThreadId,
        run: AuditRunView,
    ) {
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
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        self.chat_widget
            .add_info_message(crate::audit::progress(run), run.report.as_ref().map(|_| format!("This report will accompany your next message as untrusted context. After restarting, retrieve it with /audit read {}.", run.run_id)));
        if let Some(report) = &run.report {
            // Render in normal scrollback, alongside the existing conversation.
            let mut lines = Vec::new();
            crate::markdown::append_markdown(report, None, None, &mut lines);
            self.chat_widget.add_plain_history_lines(lines);
        }
        if let Some(error) = error {
            self.chat_widget.add_error_message(error);
        } else if let Some(outcome) = &run.outcome {
            self.chat_widget.add_info_message(outcome.clone(), None);
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

    #[tokio::test]
    async fn audit_context_follows_explicit_start_and_steer_and_survives_rejection() -> Result<()> {
        for (steer, trigger, accepted) in [
            (false, "user", true),
            (true, "user", true),
            (false, "user", false),
            (true, "user", false),
            (false, "recap", true),
        ] {
            let (mut app, mut events, _ops) =
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
            let (client, mut commands, _incoming) = CloudClient::channel(1);
            let mut server = AppServerSession::new(
                crate::backend_id::BackendId::new(),
                AppServerClient::Cloud(client),
                ThreadParamsMode::Cloud,
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
                        assert!(entry.value.contains("Finding 1"));
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
            assert_eq!(result.is_ok(), accepted);
            assert_eq!(
                app.audits.reports.for_turn(thread).is_none(),
                accepted && trigger == "user"
            );
        }
        Ok(())
    }
}
