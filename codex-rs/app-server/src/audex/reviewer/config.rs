//! Isolated configuration for the native reviewer.
use super::*;

pub(super) async fn isolated_config(
    host: &Config,
    models: &dyn ModelsManager,
    request: &ReviewerRequest,
) -> anyhow::Result<Config> {
    validate_selection_requirements(host, &request.provider, &request.model)?;
    let requirements = host.config_layer_stack.requirements();
    let provider = host
        .model_providers
        .get(&request.provider)
        .or_else(|| (host.model_provider_id == request.provider).then_some(&host.model_provider))
        .ok_or_else(|| anyhow::anyhow!("unknown reviewer provider"))?
        .clone();

    // Inherit the running client's MCP/Apps configuration rather than reconstructing
    // a fresh layer stack that discards its registered connections.
    let mut config = host.clone();
    config.config_layer_stack = host
        .config_layer_stack
        .clone()
        .with_user_and_project_exec_policy_rules_ignored(true);
    config.application_network_policy = host.application_network_policy.clone();
    config.application_auth_route_config = host.application_auth_route_config.clone();
    config.cli_auth_credentials_store_mode = host.cli_auth_credentials_store_mode;
    config.forced_login_method = host.forced_login_method;
    config.forced_chatgpt_workspace_id = host.forced_chatgpt_workspace_id.clone();
    config.chatgpt_base_url = host.chatgpt_base_url.clone();
    config.chatgpt_base_url_is_explicit = host.chatgpt_base_url_is_explicit;
    config.respect_system_proxy = host.respect_system_proxy;
    config.service_tier = host.service_tier.clone();
    // Keep resolved native context controls while excluding ambient instructions and tools.
    config.model_context_window = host.model_context_window;
    config.model_auto_compact_token_limit = host.model_auto_compact_token_limit;
    config.model_auto_compact_token_limit_scope = host.model_auto_compact_token_limit_scope;
    config.model_post_turn_compact_threshold_percent =
        host.model_post_turn_compact_threshold_percent;
    config.async_compaction_threshold_percent = host.async_compaction_threshold_percent;
    config.enforce_residency = host.enforce_residency.clone();
    config.sqlite = host.sqlite.clone();
    config.log_dir = host.log_dir.clone();
    if requirements.otel.is_some() {
        config.otel = host.otel.clone();
    }
    requirements
        .approvals_reviewer
        .can_set(&config.approvals_reviewer)?;
    config.model_provider_id = request.provider.clone();
    config.model_provider = provider.clone();
    config.model_providers = [(request.provider.clone(), provider)].into_iter().collect();
    config.model = Some(request.model.clone());
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    config.personality = Some(codex_protocol::config_types::Personality::None);
    let model = models
        .get_model_info(&request.model, &config.to_models_manager_config())
        .await;
    let effort = request
        .reasoning_effort
        .clone()
        .or(model.default_reasoning_level.clone());
    if let Some(effort) = &effort {
        anyhow::ensure!(
            model
                .supported_reasoning_levels
                .iter()
                .any(|level| &level.effort == effort),
            "model does not support the selected reasoning effort"
        );
    }
    config.model_reasoning_effort = effort.clone();
    config.project_doc_max_bytes = 0;
    config.project_doc_fallback_filenames.clear();
    config.include_permissions_instructions = false;
    config.include_apps_instructions = false;
    config.include_collaboration_mode_instructions = false;
    config.include_skill_instructions = false;
    config.cloud_skill_enabled = false;
    config.include_environment_context = false;
    config.include_environment_context_time = false;
    config.notify = None;
    config.agents_enabled = false;
    config.agent_roles.clear();
    config.experimental_request_user_input_enabled = false;
    config.update_plan_enabled = false;
    config.allow_persistent_dynamic_tool_updates = false;
    config.extra_config = None;
    config.token_budget = None;
    config.token_budget_startup_config = None;
    config.rollout_budget = None;
    config.current_time_reminder = None;
    config.features = host.features.clone();
    config.web_search_mode = host.web_search_mode.clone();
    config.web_search_mode.set(WebSearchMode::Live)?;
    config.mcp_servers = host.mcp_servers.clone();
    Ok(config)
}
