//! Live-source audits backed by retained run files.
use crate::{JsonSchema, TS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub enum AuditInput {
    Paths { paths: Vec<String> },
    Snippet { name: String, text: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditReviewer {
    pub route: String,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditStartRequest {
    pub input: AuditInput,
    /// Base directory for the operator's relative paths.
    pub cwd: String,
    pub brief: String,
    pub reviewers: Vec<AuditReviewer>,
    /// Operator-named UTF-8 guidance file, read once at start.
    pub guidance: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AuditReadView {
    Reviewers,
    Findings,
    Finding,
    Output,
    Diagnostics,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditReadRequest {
    pub run_id: String,
    pub view: Option<AuditReadView>,
    /// One-based reviewer number or the retained reviewer label.
    pub reviewer: Option<String>,
    /// Omit to read the latest attempt; previous attempts remain available.
    pub attempt: Option<u32>,
    /// One-based finding number for view=finding.
    pub number: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    /// Entries for collection views; Unicode characters for literal output.
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditListRequest {
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditCancelRequest {
    pub run_id: String,
    pub reviewer: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AuditRetryRequest {
    pub run_id: String,
    pub reviewer: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditReviewerStatus {
    pub reviewer: String,
    pub selection: AuditReviewer,
    pub attempt: u32,
    pub trigger: String,
    pub state: String,
    pub reason: Option<String>,
    pub tool_calls: u64,
    pub finding_count: Option<usize>,
    pub has_output: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditRun {
    pub run_id: String,
    pub targets: Vec<String>,
    pub created_at: String,
    /// All retained attempts, in reviewer then attempt order.
    pub reviewers: Vec<AuditReviewerStatus>,
    pub complete: bool,
    pub summary: Option<String>,
    pub source_notice: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditRunResponse {
    pub run: AuditRun,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditReadResult {
    pub run: AuditRun,
    pub data: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditListPage {
    pub runs: Vec<AuditRun>,
    pub next_offset: Option<usize>,
    /// Named selections from reviewer-backends.json; "default" is the default preset.
    pub presets: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuditProgress {
    pub run_id: String,
    pub reviewer: Option<AuditReviewerStatus>,
    /// True only after the supervisor has published summary.md.
    pub complete: bool,
    pub error: Option<String>,
}

#[cfg(test)]
mod removal_tests {
    use crate::ClientRequest;
    use serde_json::json;

    #[test]
    fn only_file_backed_audit_requests_are_exposed() {
        for method in [
            "audit/capabilities",
            "audit/preflight",
            "audit/evidence",
            "audit/attempts",
            "audit/attempt/read",
            "audit/attempt/control",
            "audit/release-slot",
            "audit/source",
            "audit/source/read",
            "audit/source/list",
            "audit/source/search",
        ] {
            let Err(error) = serde_json::from_value::<ClientRequest>(
                json!({"id":1,"method":method,"params":{}}),
            ) else {
                panic!("obsolete method {method} was accepted");
            };
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("unknown variant `{method}`")),
                "{method}: {error}"
            );
        }
        for (method, params) in [
            (
                "audit/start",
                json!({"input":{"type":"paths","paths":["/source"]},"cwd":"/","brief":"review","reviewers":[{"route":"native","model":"fixture"}]}),
            ),
            ("audit/list", json!({})),
            ("audit/read", json!({"runId":"run"})),
            ("audit/cancel", json!({"runId":"run"})),
            ("audit/retry", json!({"runId":"run","reviewer":"1"})),
            ("thread/list", json!({})),
            ("thread/read", json!({"threadId":"thread"})),
        ] {
            assert!(
                serde_json::from_value::<ClientRequest>(
                    json!({"id":1,"method":method,"params":params})
                )
                .is_ok(),
                "{method}"
            );
        }
    }
}
