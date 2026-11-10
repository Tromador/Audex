//! An operator-directed external answer shares Core's lifecycle and durable thread.
//! The host owns execution; this task never invokes a model or a workspace tool.
use super::{SessionTask, SessionTaskResult};
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_history::{CodexHarnessMetadata, ResponseItemEnvelope};
use codex_protocol::error::CodexErr;
use codex_protocol::items::{AgentMessageContent, AgentMessageItem, TurnItem};
use codex_protocol::models::{FunctionCallOutputPayload, MessagePhase, ResponseItem};
use codex_protocol::protocol::ExternalConversationExecution;
use codex_thread_store::PersistContext;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub(crate) struct ExternalConversationTask {
    execution: Mutex<Option<ExternalConversationExecution>>,
}

impl ExternalConversationTask {
    pub(crate) fn new(execution: ExternalConversationExecution) -> Self {
        Self {
            execution: Mutex::new(Some(execution)),
        }
    }
}

impl SessionTask for ExternalConversationTask {
    fn kind(&self) -> TaskKind {
        TaskKind::ExternalConversation
    }

    fn span_name(&self) -> &'static str {
        "session_task.external_conversation"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation: CancellationToken,
    ) -> SessionTaskResult {
        let execution = self
            .execution
            .lock()
            .expect("external execution")
            .take()
            .ok_or_else(|| CodexErr::Fatal("External conversation was already started".into()))?;
        session.emit_turn_started(&ctx, self.kind()).await;
        // Record only the admitted operator input, without executing prompt hooks.
        for item in input {
            crate::hook_runtime::record_pending_input(
                &session,
                &ctx,
                ctx.model_info(),
                item,
                Vec::new(),
                PersistContext::Standard,
            )
            .await;
        }
        session
            .flush_rollout()
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        execution
            .ready
            .send(())
            .map_err(|_| CodexErr::Fatal("External conversation host stopped".into()))?;
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
            result = execution.response => result
                .map_err(|_| CodexErr::Fatal("External conversation stopped; it was not replayed".into()))?
                .map_err(CodexErr::Fatal)?,
        };
        let text = format!("**{}**\n\n{}", execution.attribution, response);
        // Model-visible external evidence, rather than impersonating a Codex answer.
        let evidence = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: None,
            name: Some("claude_conversation".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text(text.clone()),
            internal_chat_message_metadata_passthrough: None,
        };
        // This is dialogue, not a tool excerpt. Its actual serialized byte size
        // conservatively covers every token; normal thread compaction still applies.
        let history_budget = serde_json::to_vec(&evidence)
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
            .len();
        session
            .record_annotated_conversation_items(
                &ctx,
                ctx.model_info(),
                vec![ResponseItemEnvelope {
                    item: evidence,
                    metadata: Some(CodexHarnessMetadata {
                        history_truncation_token_limit: Some(history_budget),
                        ..Default::default()
                    }),
                }],
            )
            .await;
        let item = TurnItem::AgentMessage(AgentMessageItem {
            id: uuid::Uuid::new_v4().to_string(),
            content: vec![AgentMessageContent::Text { text }],
            phase: Some(MessagePhase::FinalAnswer),
            moderation: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        });
        session.emit_turn_item_started(&ctx, &item).await;
        session.emit_turn_item_completed(&ctx, item).await;
        Ok(None)
    }
}
