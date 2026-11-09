use crate::{JsonSchema, TS};
use serde::{Deserialize, Serialize};

/// Audex CLI contract, independent of the Codex executable's package version.
pub const AUDIT_PROTOCOL_VERSION: u32 = 8;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditCapabilitiesParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditCapabilitiesResponse {
    pub protocol_version: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub enum AuditSource {
    Snippet { name: String, content: String },
    File { name: String, path: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditStartParams {
    pub request_key: String,
    /// Read-only disclosure approved by the operator; required for subscription rosters.
    #[schemars(schema_with = "inline_optional_preflight", default)]
    pub preflight: Option<AuditPreflightResponse>,
    pub source: AuditSource,
    pub brief: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Ordered reviewer roster. Mutually exclusive with the singular native selection.
    pub reviewers: Option<Vec<AuditReviewerSelection>>,
    /// Explicitly qualified served-model alias for the singular selection.
    pub expected_served_model: Option<String>,
    #[serde(default)]
    pub scheduling: AuditScheduling,
    pub audex_guidance: Option<String>,
    pub agents_evidence: Option<String>,
}

/// Construction and local inspection only: no persistence, client launch or inference.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditPreflightParams {
    #[schemars(schema_with = "AuditStartParams::json_schema")]
    pub request: AuditStartParams,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditPreflightResponse {
    pub protocol_version: u32,
    /// Identical request already commissioned; Start only retrieves that approval.
    pub existing_run_id: Option<String>,
    pub identity: String,
    pub snapshot_id: String,
    pub manifest_digest: String,
    pub brief_digest: String,
    pub context_digest: String,
    pub reviewers: Vec<AuditReviewerBinding>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditReviewerBinding {
    pub selection: AuditReviewerSelection,
    pub resolved_model: String,
    pub expected_served_model: String,
    pub reasoning_effort: Option<String>,
    pub backend_configuration_identity: String,
    pub native_configuration_identity: Option<String>,
    pub model_metadata_digest: Option<String>,
    pub client: Option<AuditClaudeBinding>,
}

// RPC roots carry $schema in the export bundle. Inline their nested use so a
// root and a definition cannot collide under the same exported name.
#[cfg(test)]
fn inline_optional_preflight(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    use schemars::schema::{SchemaObject, SubschemaValidation};
    SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            any_of: Some(vec![
                AuditPreflightResponse::json_schema(generator),
                generator.subschema_for::<()>(),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}

/// Non-secret local bindings; account eligibility is checked by the owned client before prompting.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditClaudeBinding {
    pub executable: String,
    pub executable_digest: String,
    pub client_version: String,
    pub profile: String,
    pub runtime_root: String,
    pub policy_digest: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditReadParams {
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditCancelParams {
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditReleaseSlotParams {
    pub run_id: String,
    pub expected_revision: i64,
    pub reason: String,
    pub acknowledge_possible_concurrent_execution_and_charges: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditListParams {
    pub after: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditRunPhase {
    Created,
    SnapshotBound,
    Reviewing,
    Reporting,
    Reconciling,
    Completed,
    CompletedWithFailures,
    Failed,
    Cancelled,
}

/// Durable operation state; `unknown` is distinct from a failed review.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditOperationState {
    Planned,
    Dispatching,
    Accepted,
    ObservedTerminal,
    Committed,
    Failed,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditRunView {
    pub run_id: String,
    pub snapshot_id: String,
    pub assignment_id: String,
    pub phase: AuditRunPhase,
    pub operation_state: AuditOperationState,
    /// A retained observation issue remains unresolved. This is non-terminal:
    /// the daemon continues observing the original operation, without resampling.
    pub observation_pending: bool,
    /// The operator explicitly released capacity. This is not terminal evidence.
    pub capacity_released: bool,
    pub revision: i64,
    pub cancellation_requested: bool,
    pub outcome: Option<String>,
    /// Exact retained response, including invalid JSON, when a bounded result was received.
    pub raw_result: Option<String>,
    pub report: Option<String>,
    /// Findings-first presentation derived from the frozen collection. The report
    /// remains the exact historical diagnostic artefact for existing consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub findings_summary: Option<String>,
    #[serde(default)]
    pub collection: AuditCollectionView,
    /// Current attempts in logical reviewer order; history uses audit/attempts.
    #[serde(default)]
    pub reviewers: Vec<super::AuditAttemptView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditStartResponse {
    pub run: AuditRunView,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditReadResponse {
    pub run: AuditRunView,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditCancelResponse {
    pub run: AuditRunView,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditReleaseSlotResponse {
    pub run: AuditRunView,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditListResponse {
    /// Required even for an empty list, so clients can check before submitting work.
    pub protocol_version: u32,
    /// Newest-first metadata summaries. Results and reports are absent; audit/read
    /// retrieves those artefacts with strict integrity verification.
    pub runs: Vec<AuditRunView>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditProgressNotification {
    pub run_id: String,
    pub revision: i64,
    pub phase: AuditRunPhase,
    #[serde(default)]
    pub attempts: Vec<super::AuditAttemptProgress>,
}

/// Execution mechanism is distinct from the configured provider and authentication route.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
#[derive(strum_macros::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AuditExecutionAdapter {
    #[default]
    Native,
    ClaudeClient,
}

impl AuditExecutionAdapter {
    pub const ALL: [Self; 2] = [Self::Native, Self::ClaudeClient];
}

/// Authentication selection is independent of provider and model. Only the
/// daemon-configured native and qualified subscription routes are available for commissioning.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
#[derive(strum_macros::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AuditAuthenticationRoute {
    #[default]
    DaemonConfiguredNative,
    ClaudeSubscription,
    ClaudeApi,
    OllamaLocal,
}

impl AuditAuthenticationRoute {
    pub const ALL: [Self; 4] = [
        Self::DaemonConfiguredNative,
        Self::ClaudeSubscription,
        Self::ClaudeApi,
        Self::OllamaLocal,
    ];
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditReviewerSelection {
    pub provider: String,
    pub model: String,
    /// Expected authoritative identity; omission requires exact resolved-model matching.
    pub expected_served_model: Option<String>,
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub execution_adapter: AuditExecutionAdapter,
    #[serde(default)]
    pub authentication_route: AuditAuthenticationRoute,
}

impl AuditReviewerSelection {
    /// Structural validation only; availability is enforced by the controller.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.provider.trim().is_empty() || self.model.trim().is_empty() {
            return Err("reviewer provider and model must be nonempty");
        }
        if self
            .expected_served_model
            .as_ref()
            .is_some_and(|alias| alias.trim().is_empty() || alias.len() > 1024)
        {
            return Err("expected served-model identity must be nonempty and bounded");
        }
        if !matches!(
            (&self.execution_adapter, &self.authentication_route),
            (
                AuditExecutionAdapter::Native,
                AuditAuthenticationRoute::DaemonConfiguredNative
                    | AuditAuthenticationRoute::OllamaLocal
            ) | (
                AuditExecutionAdapter::ClaudeClient,
                AuditAuthenticationRoute::ClaudeSubscription | AuditAuthenticationRoute::ClaudeApi
            )
        ) {
            return Err("incompatible reviewer executor and authentication route");
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditScheduling {
    #[default]
    Automatic,
    Sequential,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditCollectionView {
    pub scheduling: AuditScheduling,
    pub ready: bool,
    pub reviewer_count: u32,
    pub completed_count: u32,
    pub excluded_count: u32,
    pub outstanding_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roster_wire_preserves_order_duplicates_and_retained_alias_choices() {
        let wire = json!({"requestKey":"same-key","source":{"type":"snippet","name":"x.rs","content":"source"},"brief":"brief",
            "scheduling":"sequential","reviewers":[{"provider":"native-custom","model":"alias","expectedServedModel":"served","executionAdapter":"native"},
            {"provider":"native-custom","model":"alias","reasoningEffort":"low"},{"provider":"native-custom","model":"other"}]});
        let params: AuditStartParams = serde_json::from_value(wire).expect("public roster");
        assert_eq!(params.scheduling, AuditScheduling::Sequential);
        let roster = params.reviewers.as_ref().expect("roster");
        assert_eq!(roster.len(), 3);
        assert_eq!(roster[0].model, roster[1].model);
        assert_eq!(roster[0].expected_served_model.as_deref(), Some("served"));
        assert_eq!(roster[1].reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            serde_json::from_value::<AuditStartParams>(
                serde_json::to_value(&params).expect("wire")
            )
            .expect("roundtrip"),
            params
        );
        for adapter in ["claude", "ollama"] {
            assert!(
                serde_json::from_value::<AuditReviewerSelection>(
                    json!({"provider":"native-custom","model":"m","executionAdapter":adapter})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn legacy_single_selection_wire_remains_supported() {
        let params:AuditStartParams=serde_json::from_value(json!({"requestKey":"single","source":{"type":"snippet","name":"x.rs","content":"source"},
            "brief":"brief","provider":"native-custom","model":"model"})).expect("single selection");
        assert!(params.reviewers.is_none());
        assert_eq!(params.model.as_deref(), Some("model"));
        assert_eq!(params.scheduling, AuditScheduling::Automatic);
        assert_eq!(AUDIT_PROTOCOL_VERSION, 8);
    }
}

#[cfg(test)]
#[path = "audit_typed_selection_tests.rs"]
mod typed_selection_tests;

/// Read-only conversation access. Offsets are zero based; finding numbers are one based.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditEvidenceParams {
    pub run_id: String,
    pub query: AuditEvidenceQuery,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "view", rename_all = "snake_case", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub enum AuditEvidenceQuery {
    Reviewers {
        #[serde(default)]
        offset: usize,
        /// Caller-selected page length; omission returns all remaining reviewers.
        limit: Option<usize>,
    },
    // Preserve the existing generated SDK name now that only `view` is required.
    #[schemars(title = "AuditEvidenceQuery1")]
    Findings {
        /// Omission returns complete findings across all retained reviewers.
        reviewer: Option<String>,
        #[serde(default)]
        offset: usize,
        /// Caller-selected page length; omission returns all remaining findings.
        limit: Option<usize>,
    },
    Finding {
        reviewer: String,
        number: usize,
        #[serde(default)]
        offset: usize,
        /// Optional JSON byte range; omission returns the complete remainder.
        #[serde(rename = "maxBytes")]
        max_bytes: Option<usize>,
    },
    Source {
        snapshot: String,
        path: String,
        #[serde(default)]
        offset: u64,
        /// Optional byte range; omission reads through EOF. No Audex size ceiling.
        #[serde(rename = "maxBytes")]
        max_bytes: Option<usize>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditEvidenceResponse {
    pub data: serde_json::Value,
}
