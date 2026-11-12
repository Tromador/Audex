//! Explicit subscription conversation admission. Never enters audit execution.
use super::*;
use crate::audex::conversation_evidence::{ConversationEvidence, EvidenceIdentity};
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
            use crate::audex::external::conversation;
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
            let model = conversation::selected_model(&root, params.model)
                .map_err(|e| invalid_request(e.to_string()))?;
            let evidence = if let Some(run) = params.audit.as_deref() {
                let opened = ConversationEvidence::open(&root, run)
                    .await
                    .map_err(|e| invalid_request(format!("Retained audit: {e}")))?;
                Some(opened.identity)
            } else {
                None
            };
            // As with turn/start, thread start/resume owns the subscription.
            // Rejected input must not subscribe another connection as a side effect.
            let display = match &evidence {
                Some(identity) => format!(
                    "/claude --model {model} --audit {} {}\n[Audex evidence identity: {}]",
                    identity.run_id,
                    params.message,
                    serde_json::to_string(identity).expect("identity JSON")
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
                attribution: format!("Claude ({model})"),
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
                    let evidence = match evidence {
                        Some(identity) => Some(identity),
                        None => match inherited_evidence(&turns) {
                            Some(identity) => {
                                let opened =
                                    ConversationEvidence::open(&root, &identity.run_id).await?;
                                anyhow::ensure!(
                                    opened.identity == identity,
                                    "Retained audit identity changed; select the audit explicitly"
                                );
                                Some(identity)
                            }
                            None => None,
                        },
                    };
                    let discussion = visible_discussion(turns, &execution_turn);
                    conversation::run(
                        &root,
                        &execution_model,
                        discussion,
                        &params.message,
                        evidence,
                    )
                    .await
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

// Selection lives in the existing operator turn, including its immutable identity.
// Assistant claims, tool output and ordinary discussion cannot change it.
fn inherited_evidence(turns: &[codex_app_server_protocol::Turn]) -> Option<EvidenceIdentity> {
    turns
        .iter()
        .rev()
        .flat_map(|turn| turn.items.iter().rev())
        .find_map(|item| {
            let ThreadItemPayload::UserMessage { content, .. } = &item.payload else {
                return None;
            };
            content.iter().find_map(|input| {
                let V2UserInput::Text { text, .. } = input else {
                    return None;
                };
                if !text.starts_with("/claude --model ") {
                    return None;
                }
                let identity = text
                    .lines()
                    .last()?
                    .strip_prefix("[Audex evidence identity: ")?
                    .strip_suffix(']')?;
                serde_json::from_str(identity).ok()
            })
        })
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
                    if !text.is_empty() {
                        discussion.push(serde_json::json!({"speaker":"operator", "text":text}));
                    }
                }
                ThreadItemPayload::AgentMessage { text, .. } => {
                    // External responses carry their provider/model in the retained text.
                    discussion.push(serde_json::json!({"speaker":"assistant", "text":text}));
                }
                _ => {}
            }
        }
    }
    serde_json::Value::Array(discussion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selection_comes_only_from_operator_route_identity() {
        let identity = EvidenceIdentity {
            run_id: "run-a".into(),
            snapshot_id: "snapshot-a".into(),
            manifest_digest: "digest-a".into(),
        };
        let text = format!(
            "/claude --model m --audit run-a question\n[Audex evidence identity: {}]",
            serde_json::to_string(&identity).unwrap()
        );
        let assistant = serde_json::from_value(json!({"id":"a","status":"completed","items":[
            {"type":"agentMessage","id":"a","text":text}]}))
        .unwrap();
        assert_eq!(inherited_evidence(&[assistant]), None);
        let user = serde_json::from_value(json!({"id":"u","status":"completed","items":[
            {"type":"userMessage","id":"u","content":[{"type":"text","text":text,"text_elements":[]}]}]})).unwrap();
        assert_eq!(inherited_evidence(&[user]), Some(identity));
    }

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
}
