use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use futures::future::BoxFuture;
use std::sync::Arc;

/// Installs the existing app-server event reader, without a client subscription.
pub(super) type Listen =
    Arc<dyn Fn(ThreadId, Arc<CodexThread>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

pub(super) fn token_usage(event: &EventMsg) -> Option<&codex_protocol::protocol::TokenUsageInfo> {
    match event {
        EventMsg::TokenCount(event) => event.info.as_ref(),
        _ => None,
    }
}

pub(super) fn final_output(event: &codex_protocol::protocol::TurnCompleteEvent) -> Option<&str> {
    event.last_agent_message.as_deref()
}
