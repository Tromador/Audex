//! Format-4 facts are values at selection time, never live views or inferred usage.
use super::{escape_inline, push_line};
use crate::audex::retained::PolicyBindings;
use codex_app_server_protocol::{
    AuditAuthenticationRoute, AuditExecutionAdapter, AuditGenerationIdentity,
};
use serde::{Deserialize, Serialize};

/// Unavailable means applicable evidence was not retained. NotApplicable means
/// the selected route does not use the capability; neither means zero or false.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::audex) enum Fact<T> {
    Known { value: T },
    Unavailable { reason: String },
    NotApplicable,
}

impl<T> Fact<T> {
    pub fn known(value: T) -> Self {
        Self::Known { value }
    }

    pub fn unavailable(reason: &str) -> Self {
        Self::Unavailable {
            reason: reason.to_owned(),
        }
    }

    pub fn from_option(value: Option<T>, reason: &str) -> Self {
        value
            .map(Self::known)
            .unwrap_or_else(|| Self::unavailable(reason))
    }
}

/// Decode and render immutable older reports only; never an execution setting.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::audex) struct HistoricalReviewerLimits {
    max_attempts: Option<u32>,
    budget_usd: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct ProviderFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_limits: Option<HistoricalReviewerLimits>,
    pub executor: Fact<AuditExecutionAdapter>,
    pub route: Fact<AuditAuthenticationRoute>,
    pub admission_policy: Fact<String>,
    pub expected_served_model: Fact<String>,
    pub identity_evidence: Vec<AuditGenerationIdentity>,
    pub attribution_failure: Option<String>,
    pub retained_input_version: Fact<u32>,
    pub retained_bindings: Fact<PolicyBindings>,
    pub versions: VersionBindings,
    pub subscription: SubscriptionFacts,
    pub ollama: OllamaFacts,
    pub native_usage: Fact<NativeUsage>,
    pub usage: Fact<Vec<GenerationUsage>>,
    pub receipts: Vec<GenerationReceipt>,
    pub client_terminal: Fact<ClientTerminal>,
    pub capture_finished: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct VersionBindings {
    pub client_version: Fact<String>,
    pub client_executable_digest: Fact<String>,
    pub model_version: Fact<String>,
    pub model_metadata_digest: Fact<String>,
    pub backend_version: Fact<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct SubscriptionFacts {
    pub repair_disclosure: Fact<String>,
    pub repair_occurrences: Fact<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct OllamaFacts {
    pub model_digest: Fact<String>,
    pub context_tokens: Fact<u64>,
    pub output_tokens: Fact<u64>,
    pub truncation_detection: Fact<String>,
    pub truncation_observed: Fact<bool>,
    pub resource_tag: Fact<String>,
    pub resource_ownership: Fact<String>,
    pub resource_release: Fact<String>,
}

/// Retained native counters have aggregate/last scope, not request attribution.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct NativeUsage {
    pub total_token_usage: TokenCounters,
    pub last_token_usage: TokenCounters,
    pub last_usage_includes_reasoning: Option<bool>,
    pub model_context_window: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct TokenCounters {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: Option<i64>,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct GenerationUsage {
    pub request_id: String,
    pub evidence_source: String,
    pub input_tokens: Fact<u64>,
    pub cached_input_tokens: Fact<u64>,
    pub output_tokens: Fact<u64>,
    pub reasoning_tokens: Fact<u64>,
    pub cost: Fact<Cost>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct Cost {
    pub amount: String,
    pub currency: String,
    pub measured: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct GenerationReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_send_checks: Option<i64>,
    pub request_id: String,
    pub operation_id: String,
    pub execution_id: String,
    pub transport: String,
    pub admitted_at_ms: i64,
    pub send_started_at_ms: Option<i64>,
    pub confirmed_at_ms: Option<i64>,
    pub provider_response_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct ClientTerminal {
    pub operation_id: String,
    pub execution_id: String,
    pub terminal_id: String,
    pub successful: bool,
    pub verified: bool,
    pub result_digest: String,
}

fn field<T: Serialize>(
    output: &mut String,
    label: &str,
    fact: &Fact<T>,
) -> Result<(), serde_json::Error> {
    let value = match fact {
        Fact::Known { value } => match serde_json::to_value(value)? {
            serde_json::Value::String(s) => s,
            value => value.to_string(),
        },
        Fact::Unavailable { reason } => format!("unavailable ({reason})"),
        Fact::NotApplicable => "not applicable".to_owned(),
    };
    push_line(output, &format!("- {label}: {}", escape_inline(&value)));
    Ok(())
}

pub(in crate::audex) fn render(
    output: &mut String,
    facts: &ProviderFacts,
) -> Result<(), serde_json::Error> {
    field(output, "Executor", &facts.executor)?;
    field(output, "Route", &facts.route)?;
    field(output, "Admission policy", &facts.admission_policy)?;
    if let Some(limits) = &facts.operator_limits {
        push_line(
            output,
            &format!(
                "- Operator limits: {}. Attempts count whole reviewer executions. Monetary enforcement uses client cost estimates and may overshoot; it is not a subscription charge or provider invoice.",
                escape_inline(&serde_json::to_string(limits)?)
            ),
        );
    }
    field(
        output,
        "Expected served model",
        &facts.expected_served_model,
    )?;
    field(
        output,
        "Retained input version",
        &facts.retained_input_version,
    )?;
    field(
        output,
        "Retained source and policy bindings",
        &facts.retained_bindings,
    )?;
    field(
        output,
        "Client version binding",
        &facts.versions.client_version,
    )?;
    field(
        output,
        "Client executable digest",
        &facts.versions.client_executable_digest,
    )?;
    field(
        output,
        "Model version binding",
        &facts.versions.model_version,
    )?;
    field(
        output,
        "Model metadata digest",
        &facts.versions.model_metadata_digest,
    )?;
    field(
        output,
        "Backend version binding",
        &facts.versions.backend_version,
    )?;
    field(
        output,
        "Subscription repair disclosure",
        &facts.subscription.repair_disclosure,
    )?;
    field(
        output,
        "Subscription repair occurrences",
        &facts.subscription.repair_occurrences,
    )?;
    field(output, "Ollama model digest", &facts.ollama.model_digest)?;
    field(
        output,
        "Ollama context token limit",
        &facts.ollama.context_tokens,
    )?;
    field(
        output,
        "Ollama output token limit",
        &facts.ollama.output_tokens,
    )?;
    field(
        output,
        "Ollama truncation detection",
        &facts.ollama.truncation_detection,
    )?;
    field(
        output,
        "Ollama truncation observed",
        &facts.ollama.truncation_observed,
    )?;
    field(output, "Local resource tag", &facts.ollama.resource_tag)?;
    field(
        output,
        "Local resource ownership",
        &facts.ollama.resource_ownership,
    )?;
    field(
        output,
        "Local resource release",
        &facts.ollama.resource_release,
    )?;
    field(
        output,
        "Native aggregate and last usage counters",
        &facts.native_usage,
    )?;
    push_line(
        output,
        "- Native usage scope: retained runtime counters, which can include estimates; not per-generation attribution or billing evidence.",
    );
    field(output, "Per-generation usage evidence", &facts.usage)?;
    field(output, "Client terminal evidence", &facts.client_terminal)?;
    push_line(
        output,
        &format!(
            "- Capture finished at selection: {}",
            facts.capture_finished
        ),
    );
    if let Some(failure) = &facts.attribution_failure {
        push_line(
            output,
            &format!("- Attribution failure: {}", escape_inline(failure)),
        );
    }
    if facts.identity_evidence.is_empty() {
        push_line(
            output,
            "- Model identity evidence: unavailable; no retained observations",
        );
    }
    for item in &facts.identity_evidence {
        push_line(
            output,
            &format!(
                "- Model identity evidence: {}",
                escape_inline(&serde_json::to_string(item)?)
            ),
        );
    }
    if facts.receipts.is_empty() {
        push_line(output, "- Generation receipts: none retained");
    }
    for receipt in &facts.receipts {
        push_line(
            output,
            &format!(
                "- Generation receipt: {}",
                escape_inline(&serde_json::to_string(receipt)?)
            ),
        );
    }
    push_line(
        output,
        "- Receipt limits: admission is not proof of send or provider receipt; send-start is only a possible wire attempt. Null receipt fields mean no evidence retained at selection. Usage and billing are not inferred from admission or receipt.",
    );
    push_line(
        output,
        "- Facts are frozen at report selection. Later operational receipts remain separate and cannot change this contribution or outcome.",
    );
    Ok(())
}
