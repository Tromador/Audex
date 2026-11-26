//! Explicit subscription conversation admission. Never enters audit execution.
use super::*;
use codex_app_server_protocol::{ThreadClaudeParams, ThreadClaudeResponse, ThreadItemPayload};
use codex_protocol::protocol::ExternalConversationExecution;
use codex_protocol::turn_input::TurnInputSubmission;

impl TurnRequestProcessor {
    pub(crate) async fn thread_claude(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadClaudeParams,
        history_reader: ThreadRequestProcessor,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (request_id, params, history_reader);
            return Err(invalid_request(
                "Claude subscription conversation requires the existing Linux client route",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use crate::audex::rpc::{AuditDiscussion, SessionTurn};
            use crate::audex::service::claude::conversation;
            if params.message.trim().is_empty() {
                return Err(invalid_request(
                    "Usage: /claude [--model MODEL] [--audit RUN_ID] <message>",
                ));
            }
            let (thread_id, thread) = self.load_thread(&params.thread_id).await?;
            self.ensure_direct_input_allowed(request_id, thread.as_ref())
                .await?;
            if thread.config_snapshot().await.ephemeral {
                return Err(invalid_request(
                    "Claude conversation requires a persistent Codex thread",
                ));
            }
            let root = self.config.codex_home.join("audex");
            // Resolve the audit's reviewer before admission, so an unusable
            // selection is reported without starting a turn.
            let evidence = params
                .audit
                .as_deref()
                .map(|run| {
                    let id = uuid::Uuid::parse_str(run)?;
                    let directory = root.join("runs").join(id.to_string());
                    let discussion = crate::audex::rpc::audit_discussion(
                        &root,
                        &directory,
                        params.model.clone(),
                    )?;
                    Ok::<_, anyhow::Error>((directory, discussion))
                })
                .transpose()
                .map_err(|e| invalid_request(format!("Retained audit: {e}")))?;
            let model = match &evidence {
                Some((_, discussion)) => discussion.selection().model.clone(),
                None => conversation::selected_model(&root, params.model)
                    .map_err(|e| invalid_request(e.to_string()))?,
            };
            let attribution = evidence.as_ref().map_or_else(
                || format!("Claude ({model})"),
                |(_, discussion)| discussion.attribution(),
            );
            let session_turn = match &evidence {
                Some((_, AuditDiscussion::Session { attempt, .. })) => Some(
                    SessionTurn::acquire(attempt)
                        .map_err(|e| invalid_request(format!("Retained audit: {e:#}")))?,
                ),
                _ => None,
            };
            let display = match &evidence {
                Some(_) => format!(
                    "/claude --model {model} --audit {} {}",
                    params.audit.as_deref().unwrap_or_default(),
                    params.message
                ),
                None => format!("/claude --model {model} {}", params.message),
            };
            let request =
                TurnInputRequest::user_input(vec![codex_protocol::user_input::UserInput::Text {
                    text: display,
                    text_elements: Vec::new(),
                }])
                .on_start(TurnStartOptions {
                    turn_trigger: Some("claude_conversation".into()),
                    ..Default::default()
                });
            let (ready, started) = oneshot::channel();
            let (mut response, result) = oneshot::channel();
            let execution = ExternalConversationExecution {
                attribution,
                ready,
                response: result,
            };
            let submission = thread
                .start_external_conversation(request, execution)
                .await
                .map_err(|e| internal_error(e.to_string()))?;
            let TurnInputSubmission::Started { turn_id, .. } = submission else {
                return Err(invalid_request(
                    "Claude conversation requires an idle thread; finish or cancel the current turn first",
                ));
            };
            let execution_model = model.clone();
            let execution_turn = turn_id.clone();
            // Disconnection does not replay or cancel a daemon-owned turn. Core's
            // response receiver is the lifetime authority, including explicit interrupt.
            tokio::spawn(async move {
                let work = async {
                    started
                        .await
                        .map_err(|_| anyhow::anyhow!("Claude turn stopped before submission"))?;
                    // Preserve M3's post-start hydration: new threads have no durable
                    // history until Core starts their first turn.
                    let mut turns = if thread.config_snapshot().await.history_mode
                        == codex_protocol::protocol::ThreadHistoryMode::Paginated
                    {
                        history_reader
                            .paginated_thread_full_turns(thread_id)
                            .await
                            .map_err(|e| anyhow::anyhow!("{}", e.message))?
                    } else {
                        let history = thread.load_history(true).await?;
                        build_legacy_api_turns_from_rollout_items(&history.items)
                    };
                    turns.retain(|turn| turn.id != execution_turn);
                    match evidence {
                        Some((
                            directory,
                            AuditDiscussion::Session {
                                selection, session, ..
                            },
                        )) => {
                            let Some(mut delivery) = session_turn else {
                                anyhow::bail!("Claude reviewer session was not reserved");
                            };
                            let thread = thread_id.to_string();
                            let (turns, scope) = unseen(turns, delivery.position(&thread));
                            let assignment = serde_json::json!({
                                "discussion": visible_discussion(turns, &execution_turn),
                                "discussion_scope": scope,
                                "operator_message": params.message,
                            });
                            let reply = crate::audex::rpc::claude_resume(
                                &root,
                                &directory,
                                &selection,
                                &session,
                                assignment,
                                delivery.first(),
                            )
                            .await?;
                            // The reply stands either way; a lost position only
                            // means the next turn may repeat some dialogue.
                            if let Err(error) = delivery.delivered(thread, execution_turn) {
                                tracing::warn!(
                                    "Claude discussion position was not saved: {error:#}"
                                );
                            }
                            Ok(reply)
                        }
                        Some((directory, AuditDiscussion::Reports { selection, .. })) => {
                            crate::audex::rpc::claude_conversation(
                                &root,
                                &directory,
                                &selection,
                                visible_discussion(turns, &execution_turn),
                                &params.message,
                            )
                            .await
                        }
                        None => {
                            conversation::run(
                                &root,
                                &execution_model,
                                visible_discussion(turns, &execution_turn),
                                &params.message,
                            )
                            .await
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = response.closed() => {},
                    result = work => { let _ = response.send(result.map_err(|e| format!("Claude: {e}"))); }
                }
            });
            Ok(Some(ThreadClaudeResponse { turn_id, model }.into()))
        }
    }
}

fn visible_discussion(
    turns: Vec<codex_app_server_protocol::Turn>,
    current_turn: &str,
) -> serde_json::Value {
    let mut discussion = Vec::new();
    for turn in turns {
        if turn.id == current_turn {
            continue;
        }
        // A Claude turn's operator input is presented as `/claude ...`; its one
        // answer is Claude's, with its attribution heading retained in the text.
        let mut speaker = "codex";
        for item in turn.items {
            match item.payload {
                ThreadItemPayload::UserMessage { content, .. } => {
                    let text = content
                        .into_iter()
                        .filter_map(|input| match input {
                            V2UserInput::Text { text, .. } => Some(text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    // The native review assignment is Audex scaffolding, not dialogue.
                    if text.starts_with(codex_protocol::turn_input::AUDEX_REVIEW_MARKER) {
                        continue;
                    }
                    if text.starts_with("/claude ") {
                        speaker = "claude";
                    }
                    if !text.is_empty() {
                        discussion.push(serde_json::json!({"speaker":"operator", "text":text}));
                    }
                }
                ThreadItemPayload::AgentMessage { text, .. } => {
                    discussion.push(serde_json::json!({"speaker":speaker, "text":text}));
                }
                _ => {}
            }
        }
    }
    serde_json::Value::Array(discussion)
}

/// Drops the turns a resumed session has already received: those up to and
/// including the last turn delivered from this conversation. A missing position
/// is reported to Claude, never guessed.
fn unseen(
    mut turns: Vec<codex_app_server_protocol::Turn>,
    position: Option<&str>,
) -> (Vec<codex_app_server_protocol::Turn>, &'static str) {
    let Some(position) = position else {
        return (
            turns,
            "The complete visible discussion in this Codex conversation before the operator's current message. You have not received it before.",
        );
    };
    match turns.iter().position(|turn| turn.id == position) {
        Some(index) => (
            turns.split_off(index + 1),
            "Only the visible discussion in this Codex conversation since your last reply in it. You received the earlier discussion with previous messages.",
        ),
        None => (
            turns,
            "The complete visible discussion in this Codex conversation. Your last reply in it was not found in its history, so some of this may repeat what you have already received.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visible_discussion_preserves_long_dialogue_and_excludes_internal_items() {
        let mut turns = Vec::new();
        for i in 0..200 {
            turns.push(serde_json::from_value(json!({
                "id":format!("turn-{i}"), "status":"completed", "items":[
                    {"type":"userMessage","id":format!("user-{i}"),"content":[{"type":"text","text":format!("question-{i}"),"text_elements":[]}]},
                    {"type":"agentMessage","id":format!("answer-{i}"),"text":format!("answer-{i}")},
                    {"type":"reasoning","id":format!("private-{i}"),"summary":["PRIVATE_SENTINEL"],"content":["PRIVATE_SENTINEL"]}
                ]
            })).unwrap());
        }
        let discussion = visible_discussion(turns, "turn-199");
        let entries = discussion.as_array().unwrap();
        assert_eq!(entries.len(), 398);
        assert_eq!(entries[0]["text"], "question-0");
        assert_eq!(entries[397]["text"], "answer-198");
        assert!(!discussion.to_string().contains("PRIVATE_SENTINEL"));
    }

    fn turn(id: &str, operator: &str, answer: &str) -> codex_app_server_protocol::Turn {
        serde_json::from_value(json!({
            "id":id, "status":"completed", "items":[
                {"type":"userMessage","id":format!("{id}-user"),"content":[{"type":"text","text":operator,"text_elements":[]}]},
                {"type":"agentMessage","id":format!("{id}-answer"),"text":answer}
            ]
        })).unwrap()
    }

    #[test]
    fn visible_discussion_attributes_speakers_and_omits_review_assignment() {
        let assignment = format!(
            "{}\n\nHIDDEN_ASSIGNMENT_SENTINEL",
            codex_protocol::turn_input::AUDEX_REVIEW_MARKER
        );
        let turns = vec![
            turn("review", &assignment, "Codex review findings"),
            turn(
                "ask",
                "Is the nullptr important?",
                "Codex: it is unimportant",
            ),
            turn(
                "claude",
                "/claude --model m --audit run Do you agree?",
                "**Claude (m) — claude, continuing its audit session**\n\nNo.",
            ),
        ];
        let discussion = visible_discussion(turns, "current");
        assert!(
            !discussion
                .to_string()
                .contains("HIDDEN_ASSIGNMENT_SENTINEL")
        );
        let speakers: Vec<_> = discussion
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["speaker"].as_str().unwrap())
            .collect();
        let expected = ["codex", "operator", "codex", "operator", "claude"];
        assert_eq!(speakers, expected);
    }

    fn ids(turns: &[codex_app_server_protocol::Turn]) -> Vec<&str> {
        turns.iter().map(|turn| turn.id.as_str()).collect()
    }

    #[test]
    fn unseen_turns_follow_the_last_delivery_or_report_a_missing_position() {
        let turns = || {
            ["a", "b", "c"]
                .into_iter()
                .map(|id| turn(id, id, id))
                .collect::<Vec<_>>()
        };
        let (all, scope) = unseen(turns(), None);
        assert_eq!(ids(&all), ["a", "b", "c"]);
        assert!(scope.contains("not received it before"));
        let (since, scope) = unseen(turns(), Some("b"));
        assert_eq!(ids(&since), ["c"]);
        assert!(scope.contains("since your last reply"));
        let (none, _) = unseen(turns(), Some("c"));
        assert!(none.is_empty());
        let (missing, scope) = unseen(turns(), Some("rolled-back"));
        assert_eq!(ids(&missing), ["a", "b", "c"]);
        assert!(scope.contains("was not found"));
    }
}
