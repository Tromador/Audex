use crate::{JsonSchema, TS};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditAttemptAction {
    Cancel,
    Ignore,
    Retry,
}

/// Versioned deterministic extraction facts; completion provenance stays on the attempt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditResultExtraction {
    pub version: u32,
    pub raw_digest: String,
    pub status: AuditExtractionStatus,
    pub parse_digest: String,
    pub transformations: Vec<AuditResultTransformation>,
    pub validation_error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditExtractionStatus {
    ValidatedStructured,
    DeterministicallyNormalised,
    UnstructuredOpinion,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditResultTransformation {
    Utf8Bom,
    MarkdownJsonFence,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditAttemptsParams {
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditAttemptControlParams {
    pub run_id: String,
    pub assignment_id: String,
    /// Reuse the same key and payload after a lost acknowledgement.
    pub action_key: String,
    pub action: AuditAttemptAction,
    pub reason: String,
    #[serde(default)]
    pub authorise_retry: bool,
    #[serde(default)]
    pub acknowledge_possible_overlap: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditAttemptReadParams {
    pub run_id: String,
    pub assignment_id: String,
    pub after_sequence: Option<i64>,
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditAttemptView {
    pub run_id: String,
    pub reviewer_id: String,
    pub reviewer_ordinal: i64,
    pub assignment_id: String,
    pub attempt_ordinal: i64,
    pub operation_id: String,
    pub snapshot_id: String,
    pub provider: String,
    /// Requested selection, retained for compatibility.
    pub model: String,
    pub requested_model: String,
    pub resolved_model: String,
    pub expected_served_model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub revision: i64,
    pub owner_generation: i64,
    pub current: bool,
    pub retry_eligible: bool,
    pub execution_adapter: super::AuditExecutionAdapter,
    /// Uses the retained native route; this does not claim an API/subscription billing amount.
    pub authentication_route: String,
    /// Retained per-generation evidence, including failures; reads never refresh configuration.
    pub model_evidence: Vec<AuditGenerationIdentity>,
    /// Contribution disposition is independent of execution certainty.
    pub disposition: String,
    pub assignment_state: String,
    pub operation_state: String,
    /// An authoritative completed opinion, independent of structured extraction.
    /// Never inferred from activity, process exit alone or reviewer identity claims.
    pub completed: bool,
    /// Absent for historical results predating the shared extraction contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub extraction: Option<AuditResultExtraction>,
    pub possible_overlap: bool,
    pub output_sequence: i64,
    pub output_cutoff: Option<i64>,
    pub capture_gap: Option<String>,
    pub capture_failure: Option<String>,
    pub capture_truncated: bool,
    /// Live local cleanup receipts, independent of the frozen contribution and remote billing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub local_execution: Option<AuditLocalExecution>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditLocalExecution {
    pub status: String,
    pub stop_requested_at_ms: Option<i64>,
    pub stop_confirmed_at_ms: Option<i64>,
    pub cleanup_finished_at_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditActivityEntry {
    pub sequence: i64,
    pub session_id: String,
    pub turn_id: String,
    pub item_id: String,
    /// Assistant text, source-tool activity and status remain distinct.
    pub kind: String,
    /// Exact retained public material. Activity is unvalidated, including JSON.
    pub payload: String,
    pub payload_digest: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditAttemptsResponse {
    pub attempts: Vec<AuditAttemptView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditAttemptReadResponse {
    pub attempt: AuditAttemptView,
    pub entries: Vec<AuditActivityEntry>,
    pub next_sequence: i64,
    /// The original terminal response, if retained; invalid JSON stays invalid.
    pub raw_result: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditAttemptControlResponse {
    pub action_key: String,
    pub assignment_id: String,
    pub disposition: String,
    pub cutoff: Option<i64>,
    pub replacement_assignment_id: Option<String>,
    pub replacement_operation_id: Option<String>,
    pub completed_won: bool,
    pub possible_overlap: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditAttemptProgress {
    pub assignment_id: String,
    pub revision: i64,
    pub output_sequence: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditGenerationIdentity {
    pub request_id: String,
    pub transport: String,
    pub expected_model: Option<String>,
    pub observed_model: Option<String>,
    /// Connection observations cannot establish per-generation attribution.
    pub evidence_scope: Option<String>,
    pub state: String,
}
