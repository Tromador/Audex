//! Non-secret configuration references. Loading never probes or enables a backend.
use super::snapshot::manifest::sha256_hex;
use codex_app_server_protocol::AuditAuthenticationRoute;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct BackendSettings {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_conversation_model: Option<String>,
    pub claude_subscription: Option<ClaudeSubscription>,
    pub claude_api: Option<ClaudeApi>,
    pub ollama_local: Option<OllamaLocal>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ClaudeSubscription {
    pub executable: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_version: String,
    pub profile: String,
    pub runtime_root: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ClaudeApi {
    pub executable: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_version: String,
    pub runtime_root: String,
    pub credential: CredentialReference,
    pub upstream: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "kind",
    content = "reference",
    rename_all = "snake_case"
)]
pub(super) enum CredentialReference {
    Environment(String),
    File(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct OllamaLocal {
    pub endpoint: String,
    pub resource_tag: String,
    pub model: String,
    pub model_digest: String,
    pub context_tokens: u64,
    pub output_tokens: u64,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            version: 1,
            claude_conversation_model: None,
            claude_subscription: None,
            claude_api: None,
            ollama_local: None,
        }
    }
}

impl BackendSettings {
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let bytes = match std::fs::read(root.join("reviewer-backends.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => anyhow::bail!("reviewer-backends.json is unavailable"),
        };
        anyhow::ensure!(
            bytes.len() <= 64 * 1024,
            "reviewer-backends.json exceeds 64 KiB"
        );
        // Do not echo unknown values: an incorrectly supplied field may contain a secret.
        let settings: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid reviewer-backends.json schema"))?;
        settings.validate()?;
        Ok(settings)
    }

    /// Validate the complete file, then bind only this route's configuration.
    pub fn identity(&self, route: &AuditAuthenticationRoute) -> anyhow::Result<String> {
        self.validate()?;
        let mut selected = serde_json::json!({"version": self.version});
        match route {
            AuditAuthenticationRoute::DaemonConfiguredNative => {}
            AuditAuthenticationRoute::ClaudeSubscription => {
                selected["claudeSubscription"] = serde_json::to_value(&self.claude_subscription)?;
            }
            AuditAuthenticationRoute::ClaudeApi => {
                selected["claudeApi"] = serde_json::to_value(&self.claude_api)?;
            }
            AuditAuthenticationRoute::OllamaLocal => {
                selected["ollamaLocal"] = serde_json::to_value(&self.ollama_local)?;
            }
        }
        Ok(sha256_hex(&serde_json::to_vec(&selected)?))
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported reviewer-backends.json version"
        );
        if let Some(model) = &self.claude_conversation_model {
            bounded(model)?;
        }
        if let Some(c) = &self.claude_subscription {
            absolute(&c.executable)?;
            if !c.client_version.is_empty() {
                bounded(&c.client_version)?;
            }
            absolute(&c.profile)?;
            absolute(&c.runtime_root)?;
        }
        if let Some(c) = &self.claude_api {
            absolute(&c.executable)?;
            if !c.client_version.is_empty() {
                bounded(&c.client_version)?;
            }
            absolute(&c.runtime_root)?;
            endpoint(&c.upstream)?;
            match &c.credential {
                CredentialReference::File(path) => absolute(path)?,
                CredentialReference::Environment(name) => {
                    bounded(name)?;
                    anyhow::ensure!(
                        name.bytes().enumerate().all(|(i, b)| b == b'_'
                            || b.is_ascii_alphabetic()
                            || (i > 0 && b.is_ascii_digit())),
                        "invalid credential environment reference"
                    );
                }
            }
        }
        if let Some(c) = &self.ollama_local {
            endpoint(&c.endpoint)?;
            bounded(&c.resource_tag)?;
            bounded(&c.model)?;
            bounded(&c.model_digest)?;
            anyhow::ensure!(
                c.context_tokens > 0 && c.output_tokens > 0 && c.output_tokens <= c.context_tokens,
                "invalid Ollama model limits"
            );
        }
        Ok(())
    }
}

fn bounded(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
        "invalid backend configuration reference"
    );
    Ok(())
}

fn absolute(value: &str) -> anyhow::Result<()> {
    bounded(value)?;
    anyhow::ensure!(
        Path::new(value).is_absolute(),
        "backend path reference must be absolute"
    );
    Ok(())
}

fn endpoint(value: &str) -> anyhow::Result<()> {
    bounded(value)?;
    let url = url::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid backend endpoint"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "backend endpoint must be an HTTP destination without credentials, query or fragment"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
