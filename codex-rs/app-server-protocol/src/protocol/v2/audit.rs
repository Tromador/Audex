use crate::{JsonSchema, TS};
use serde::{Deserialize, Serialize};

/// Audex CLI contract, independent of the Codex executable's package version.
pub const AUDIT_PROTOCOL_VERSION: u32 = 3;

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
    pub source: AuditSource,
    pub brief: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub audex_guidance: Option<String>,
    pub agents_evidence: Option<String>,
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
