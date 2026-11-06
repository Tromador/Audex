//! Versioned approved inputs. Version 1 remains decodable without rewriting its bytes.
use super::backends::BackendSettings;
use super::reviewer::PreparedReviewer;
use super::snapshot::{AccessLimits, SnapshotHandle, manifest::sha256_hex};
use codex_app_server_protocol::{
    AuditAuthenticationRoute, AuditExecutionAdapter, AuditReviewerSelection, AuditScheduling,
};
use codex_core::config::Config;
use codex_protocol::openai_models::ReasoningEffort;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RetainedInput {
    pub version: u32,
    pub brief: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub require_served_identity: bool,
    pub expected_served_model: Option<String>,
    pub resolved_model: Option<String>,
    pub execution_adapter: Option<AuditExecutionAdapter>,
    pub authentication_route: Option<AuditAuthenticationRoute>,
    pub bindings: Option<PolicyBindings>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct PolicyBindings {
    pub version: u32,
    pub snapshot_id: String,
    pub manifest_digest: String,
    pub brief_digest: String,
    pub context_digest: String,
    pub policy_digest: String,
    pub backend_configuration_identity: String,
    pub native_configuration_identity: Option<String>,
    pub scheduling: AuditScheduling,
    pub tool_exposure: Vec<String>,
    pub limits: ExecutionLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ExecutionLimits {
    pub request_retries: u32,
    pub stream_retries: u32,
    pub automatic_compaction: bool,
    pub tool_argument_bytes: usize,
    pub read_bytes: usize,
    pub search_query_bytes: usize,
    pub search_results: usize,
}

impl ExecutionLimits {
    pub(super) fn native() -> Self {
        let access = AccessLimits::default();
        Self {
            request_retries: 0,
            stream_retries: 0,
            automatic_compaction: false,
            tool_argument_bytes: 16 * 1024,
            read_bytes: access.max_read_bytes,
            search_query_bytes: access.max_search_query_bytes,
            search_results: access.max_search_results,
        }
    }
}

impl RetainedInput {
    pub fn decode(raw: &str) -> anyhow::Result<Self> {
        let input: Self = serde_json::from_str(raw)?;
        anyhow::ensure!(
            matches!(input.version, 1 | 2),
            "unsupported retained audit input"
        );
        AuditReviewerSelection {
            provider: input.provider.clone(),
            model: input.model.clone(),
            reasoning_effort: input.reasoning_effort.as_ref().map(ToString::to_string),
            expected_served_model: input.expected_served_model.clone(),
            execution_adapter: input.adapter(),
            authentication_route: input.route(),
        }
        .validate()
        .map_err(anyhow::Error::msg)?;
        if input.version == 1 {
            anyhow::ensure!(
                input.bindings.is_none()
                    && input.route() == AuditAuthenticationRoute::DaemonConfiguredNative,
                "invalid legacy retained audit input"
            );
        } else {
            anyhow::ensure!(
                input.require_served_identity
                    && input.execution_adapter.is_some()
                    && input.authentication_route.is_some()
                    && input
                        .resolved_model
                        .as_ref()
                        .is_some_and(|s| !s.trim().is_empty()),
                "incomplete retained model and route binding"
            );
            let binding = input
                .bindings
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing retained policy bindings"))?;
            anyhow::ensure!(
                binding.version == 1 && binding.brief_digest == sha256_hex(input.brief.as_bytes()),
                "invalid retained policy binding"
            );
            for digest in [
                &binding.manifest_digest,
                &binding.brief_digest,
                &binding.context_digest,
                &binding.policy_digest,
                &binding.backend_configuration_identity,
            ] {
                anyhow::ensure!(
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                    "invalid retained policy digest"
                );
            }
        }
        Ok(input)
    }

    pub fn route(&self) -> AuditAuthenticationRoute {
        self.authentication_route.clone().unwrap_or_default()
    }

    pub fn adapter(&self) -> AuditExecutionAdapter {
        self.execution_adapter.clone().unwrap_or_default()
    }

    pub fn expected_model(&self) -> &str {
        self.expected_served_model
            .as_deref()
            .or(self.resolved_model.as_deref())
            .unwrap_or(&self.model)
    }

    pub fn require_native(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.route() == AuditAuthenticationRoute::DaemonConfiguredNative
                && self.adapter() == AuditExecutionAdapter::Native,
            "retained reviewer route is unavailable for execution"
        );
        Ok(())
    }

    pub fn validate_backend_configuration(&self, settings: &BackendSettings) -> anyhow::Result<()> {
        let current = settings.identity(&self.route())?;
        if let Some(binding) = &self.bindings {
            anyhow::ensure!(
                binding.backend_configuration_identity == current,
                "backend configuration changed since approval"
            );
        }
        Ok(())
    }

    pub fn validate_prepared(
        &self,
        snapshot: &SnapshotHandle,
        prepared: &PreparedReviewer,
        config: &Config,
        settings: &BackendSettings,
        scheduling: AuditScheduling,
    ) -> anyhow::Result<()> {
        self.require_native()?;
        self.validate_backend_configuration(settings)?;
        if let Some(binding) = &self.bindings {
            let current = PolicyBindings::native(
                snapshot,
                &self.brief,
                prepared,
                config,
                &self.provider,
                settings,
                scheduling,
            )?;
            anyhow::ensure!(
                *binding == current,
                "retained reviewer configuration or policy changed"
            );
            anyhow::ensure!(
                self.resolved_model.as_deref()
                    == prepared.policy["model_metadata"]["slug"].as_str(),
                "retained resolved model changed"
            );
        }
        Ok(())
    }
}

impl PolicyBindings {
    pub fn native(
        snapshot: &SnapshotHandle,
        brief: &str,
        prepared: &PreparedReviewer,
        config: &Config,
        provider: &str,
        settings: &BackendSettings,
        scheduling: AuditScheduling,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            version: 1,
            snapshot_id: snapshot.snapshot_id().to_string(),
            manifest_digest: sha256_hex(&snapshot.manifest().to_pretty_bytes()?),
            brief_digest: sha256_hex(brief.as_bytes()),
            context_digest: sha256_hex(&serde_json::to_vec(
                &snapshot.manifest().identity.context_documents,
            )?),
            policy_digest: prepared.policy_digest.clone(),
            backend_configuration_identity: settings
                .identity(&AuditAuthenticationRoute::DaemonConfiguredNative)?,
            native_configuration_identity: Some(native_configuration_identity(config, provider)?),
            scheduling,
            tool_exposure: super::source::TOOL_NAMES
                .map(|name| format!("audex_source.{name}"))
                .to_vec(),
            limits: ExecutionLimits::native(),
        })
    }
}

/// Explicit non-secret projection of the existing daemon configuration. Credential
/// contents are never resolved, serialised or hashed; their references/mode are bound.
pub(super) fn native_configuration_identity(
    config: &Config,
    provider: &str,
) -> anyhow::Result<String> {
    let p = config
        .model_providers
        .get(provider)
        .or_else(|| (config.model_provider_id == provider).then_some(&config.model_provider))
        .ok_or_else(|| anyhow::anyhow!("unknown reviewer provider"))?;
    Ok(sha256_hex(&serde_json::to_vec(&json!({
        "version":1, "provider":provider, "baseUrl":p.base_url, "wireApi":p.wire_api,
        "envKey":p.env_key, "envHeaders":p.env_http_headers.as_ref().map(|headers| headers.iter().collect::<std::collections::BTreeMap<_,_>>()), "credentialFile":p.credential_file,
        "requiresOpenaiAuth":p.requires_openai_auth, "commandAuth":p.auth.is_some(),
        "gatewayOauth":p.gateway_oauth.is_some(), "aws":p.aws.is_some(),
        "configuredBearer":p.experimental_bearer_token.is_some(),
        "projectId":p.project_id, "location":p.location, "capabilities":p.capabilities,
        "websockets":p.supports_websockets, "idleTimeout":p.stream_idle_timeout_ms,
        "connectTimeout":p.websocket_connect_timeout_ms, "chatgptBaseUrl":config.chatgpt_base_url,
        "forcedLoginMethod":config.forced_login_method, "workspace":config.forced_chatgpt_workspace_id,
        "serviceTier":config.service_tier, "residency":config.enforce_residency.get().as_ref().map(|value| match value { codex_config::ResidencyRequirement::Us => "us" }),
        "respectSystemProxy":config.respect_system_proxy
    }))?))
}
