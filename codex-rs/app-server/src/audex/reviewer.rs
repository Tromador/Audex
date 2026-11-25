use codex_core::StartThreadOptions;
use codex_core::config::Config;
use codex_extension_api::ToolPolicy;
use codex_features::Feature;
use codex_models_manager::manager::ModelsManager;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::{ManagedFileSystemPermissions, PermissionProfile};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::{
    FileSystemAccessMode, FileSystemSandboxEntry, NetworkSandboxPolicy,
};
use codex_protocol::protocol::{AskForApproval, SessionSource, ThreadSource};
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use serde_json::json;

mod config;
pub(super) mod native;

pub(super) struct ReviewerRequest {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
}

pub(super) fn validate_selection_requirements(
    host: &Config,
    provider: &str,
    model: &str,
) -> anyhow::Result<()> {
    let requirements = host.config_layer_stack.requirements();
    // These mandatory contributions cannot be proven compatible by text inspection.
    // Reject rather than silently strip them or let them widen reviewer capabilities.
    anyhow::ensure!(
        requirements.additional_developer_instructions.is_none(),
        "managed additional developer instructions require an explicit Audex compatibility decision"
    );
    anyhow::ensure!(
        requirements.managed_hooks.is_none(),
        "managed hooks are incompatible with static Audex reviewers"
    );
    anyhow::ensure!(
        !requirements.auto_review_required_for_model(model),
        "managed automatic review conflicts with Audex approval policy Never"
    );
    if let Some(required) = &requirements.model_provider {
        anyhow::ensure!(
            required.value == provider,
            "reviewer provider conflicts with managed provider requirement"
        );
    }
    Ok(())
}
