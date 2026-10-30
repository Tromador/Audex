//! Resolves controller targets using the caller's registered identity.
//! Legacy callers can still supply their captured session source for path resolution.

use super::LocalAgentControl;
use super::LocalAgentRuntime;
use crate::agent::api::AgentTarget;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;

impl LocalAgentControl {
    /// Audex roots are owned by their durable workflow, not by another model's
    /// agent tools. Check cold metadata too, before any legacy resume can load it.
    pub(super) async fn ensure_agent_target_access(&self, target: ThreadId) -> CodexResult<()> {
        let manager = self.runtime.upgrade()?;
        let source = if let Ok(thread) = manager.get_thread(target).await {
            thread.config_snapshot().await.thread_source
        } else {
            match manager
                .read_stored_thread(codex_thread_store::ReadThreadParams {
                    thread_id: target,
                    include_archived: true,
                    include_history: false,
                })
                .await
            {
                Ok(stored) => stored.thread_source,
                Err(error)
                    if matches!(
                        error.details(),
                        codex_protocol::error::CodexErrorDetails::ThreadNotFound(_)
                    ) =>
                {
                    None
                }
                Err(error) => return Err(error),
            }
        };
        if matches!(source,Some(codex_protocol::protocol::ThreadSource::Feature(name)) if name=="audex-reviewer")
        {
            return Err(CodexErr::InvalidRequest(
                "Audex reviewer threads are owned by the audit controller".to_owned(),
            ));
        }
        Ok(())
    }

    pub(crate) fn resolve_target(
        &self,
        caller: ThreadId,
        target: &AgentTarget,
    ) -> CodexResult<ThreadId> {
        match target {
            AgentTarget::Id(thread_id) => Ok(*thread_id),
            AgentTarget::Reference(reference) => {
                let caller = self.runtime.ensure_agent_known(caller)?;
                self.runtime.resolve_path_reference(
                    &caller.agent_path.unwrap_or_else(AgentPath::root),
                    reference,
                )
            }
        }
    }
}

impl LocalAgentRuntime {
    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        self.resolve_path_reference(&current_agent_path, agent_reference)
    }

    pub(super) fn resolve_path_reference(
        &self,
        current_agent_path: &AgentPath,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.registry.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }
}
