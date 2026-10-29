use super::snapshot::SnapshotManifestV1;
use super::snapshot::SnapshotPath;
use super::snapshot::manifest::sha256_hex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

pub(super) const RESULT_SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_RAW_RESULT_BYTES: usize = 1024 * 1024;
const MAX_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_FINDINGS: usize = 256;
const MAX_TITLE_BYTES: usize = 512;
const MAX_FIELD_BYTES: usize = 16 * 1024;
const MAX_EVIDENCE_PER_FINDING: usize = 16;
const MAX_TOTAL_EVIDENCE: usize = 1024;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditResultV1 {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub assignment_id: String,
    pub summary: String,
    pub findings: Vec<AuditFindingV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditFindingV1 {
    pub severity: FindingSeverity,
    pub confidence: FindingConfidence,
    pub title: String,
    pub mechanism: String,
    pub impact: String,
    pub evidence: Vec<FindingEvidenceV1>,
    pub remediation: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingSeverity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl FindingSeverity {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingConfidence {
    Low,
    Medium,
    High,
}

impl FindingConfidence {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FindingEvidenceV1 {
    pub description: String,
    pub path: String,
    pub start_byte: u64,
    pub end_byte: u64,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum ResultValidationError {
    #[error("result exceeds the {MAX_RAW_RESULT_BYTES}-byte limit")]
    TooLarge,
    #[error("result is not valid schema-v1 JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported result schema version {0}")]
    UnsupportedVersion(u32),
    #[error("result snapshot ID does not match the assignment")]
    SnapshotMismatch,
    #[error("result assignment ID does not match the assignment")]
    AssignmentMismatch,
    #[error("{0}")]
    Semantic(String),
}

#[derive(Clone, Debug)]
pub(super) struct ValidatedAuditResult {
    pub response: AuditResultV1,
    pub finding_ids: Vec<String>,
}

pub(super) fn parse_and_validate(
    raw: &[u8],
    expected_assignment_id: &str,
    manifest: &SnapshotManifestV1,
) -> Result<ValidatedAuditResult, ResultValidationError> {
    if raw.len() > MAX_RAW_RESULT_BYTES {
        return Err(ResultValidationError::TooLarge);
    }
    let response: AuditResultV1 = serde_json::from_slice(raw)
        .map_err(|error| ResultValidationError::InvalidJson(error.to_string()))?;
    validate_response(&response, expected_assignment_id, manifest)?;
    let finding_ids = response
        .findings
        .iter()
        .enumerate()
        .map(|(ordinal, finding)| finding_id(expected_assignment_id, ordinal, finding))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ValidatedAuditResult {
        response,
        finding_ids,
    })
}

fn validate_response(
    response: &AuditResultV1,
    expected_assignment_id: &str,
    manifest: &SnapshotManifestV1,
) -> Result<(), ResultValidationError> {
    if response.schema_version != RESULT_SCHEMA_VERSION {
        return Err(ResultValidationError::UnsupportedVersion(
            response.schema_version,
        ));
    }
    if response.snapshot_id != manifest.snapshot_id.as_str() {
        return Err(ResultValidationError::SnapshotMismatch);
    }
    if response.assignment_id != expected_assignment_id {
        return Err(ResultValidationError::AssignmentMismatch);
    }
    validate_text("summary", &response.summary, MAX_SUMMARY_BYTES)?;
    if response.findings.len() > MAX_FINDINGS {
        return Err(ResultValidationError::Semantic(format!(
            "result has more than {MAX_FINDINGS} findings"
        )));
    }

    let mut evidence_count = 0_usize;
    for (finding_ordinal, finding) in response.findings.iter().enumerate() {
        validate_text("finding title", &finding.title, MAX_TITLE_BYTES)?;
        validate_text("finding mechanism", &finding.mechanism, MAX_FIELD_BYTES)?;
        validate_text("finding impact", &finding.impact, MAX_FIELD_BYTES)?;
        if let Some(remediation) = &finding.remediation {
            validate_text("finding remediation", remediation, MAX_FIELD_BYTES)?;
        }
        if finding.evidence.is_empty() || finding.evidence.len() > MAX_EVIDENCE_PER_FINDING {
            return Err(ResultValidationError::Semantic(format!(
                "finding {finding_ordinal} must have between 1 and {MAX_EVIDENCE_PER_FINDING} evidence records"
            )));
        }
        evidence_count = evidence_count.saturating_add(finding.evidence.len());
        if evidence_count > MAX_TOTAL_EVIDENCE {
            return Err(ResultValidationError::Semantic(format!(
                "result has more than {MAX_TOTAL_EVIDENCE} evidence records"
            )));
        }
        for (evidence_ordinal, evidence) in finding.evidence.iter().enumerate() {
            validate_evidence(manifest, finding_ordinal, evidence_ordinal, evidence)?;
        }
    }
    Ok(())
}

fn validate_evidence(
    manifest: &SnapshotManifestV1,
    finding_ordinal: usize,
    evidence_ordinal: usize,
    evidence: &FindingEvidenceV1,
) -> Result<(), ResultValidationError> {
    validate_text(
        "evidence description",
        &evidence.description,
        MAX_FIELD_BYTES,
    )?;
    let path = SnapshotPath::new(evidence.path.clone()).map_err(|error| {
        ResultValidationError::Semantic(format!(
            "finding {finding_ordinal} evidence {evidence_ordinal} has an invalid path: {error}"
        ))
    })?;
    let entry = manifest
        .identity
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .ok_or_else(|| {
            ResultValidationError::Semantic(format!(
                "finding {finding_ordinal} evidence {evidence_ordinal} path is not in the snapshot manifest"
            ))
        })?;
    if evidence.start_byte >= evidence.end_byte || evidence.end_byte > entry.size {
        return Err(ResultValidationError::Semantic(format!(
            "finding {finding_ordinal} evidence {evidence_ordinal} has an invalid byte range"
        )));
    }
    match (evidence.start_line, evidence.end_line) {
        (None, None) => {}
        (Some(start), Some(end)) if start > 0 && start <= end => {}
        _ => {
            return Err(ResultValidationError::Semantic(format!(
                "finding {finding_ordinal} evidence {evidence_ordinal} has an invalid line range"
            )));
        }
    }
    Ok(())
}

fn validate_text(name: &str, value: &str, max_bytes: usize) -> Result<(), ResultValidationError> {
    if value.trim().is_empty() {
        return Err(ResultValidationError::Semantic(format!(
            "{name} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(ResultValidationError::Semantic(format!(
            "{name} exceeds {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn finding_id(
    assignment_id: &str,
    ordinal: usize,
    finding: &AuditFindingV1,
) -> Result<String, ResultValidationError> {
    #[derive(Serialize)]
    struct FindingIdentity<'a> {
        version: u32,
        assignment_id: &'a str,
        ordinal: usize,
        finding: &'a AuditFindingV1,
    }

    let identity = FindingIdentity {
        version: 1,
        assignment_id,
        ordinal,
        finding,
    };
    let bytes = serde_json::to_vec(&identity)
        .map_err(|error| ResultValidationError::InvalidJson(error.to_string()))?;
    Ok(format!("audex-finding-v1:sha256:{}", sha256_hex(&bytes)))
}

pub(super) fn evidence_id(finding_id: &str, ordinal: usize) -> String {
    let identity = format!("audex-evidence-v1\0{finding_id}\0{ordinal}");
    format!(
        "audex-evidence-v1:sha256:{}",
        sha256_hex(identity.as_bytes())
    )
}
