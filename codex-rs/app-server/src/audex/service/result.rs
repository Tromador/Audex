//! Citation flags check locations, not the truth of a reviewer's claims.
use super::targets::Targets;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewResult {
    pub summary: String,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub title: String,
    pub severity: Severity,
    pub confidence: Confidence,
    pub trigger: String,
    pub mechanism: String,
    pub impact: String,
    pub evidence: String,
    pub remediation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Informational,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationStatus {
    Ok,
    PathNotFound,
    OutsideWorkingDirectories,
    LinesOutOfRange,
    RemoteUnverified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckedFinding {
    #[serde(flatten)]
    pub finding: Finding,
    pub citation: CitationStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckedResult {
    pub summary: String,
    pub findings: Vec<CheckedFinding>,
}

pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(ReviewResult)).expect("result schema serialises")
}

pub fn collect(raw: &[u8], targets: &Targets) -> Option<CheckedResult> {
    let text = std::str::from_utf8(raw).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).trim();
    let text = if let Some(body) = text
        .strip_prefix("```json\r\n")
        .or_else(|| text.strip_prefix("```json\n"))
    {
        body.strip_suffix("\r\n```")
            .or_else(|| body.strip_suffix("\n```"))?
    } else {
        text
    };
    let result: ReviewResult = serde_json::from_str(text).ok()?;
    Some(CheckedResult {
        summary: result.summary,
        findings: result
            .findings
            .into_iter()
            .map(|finding| {
                let citation = check_citation(&finding, targets);
                CheckedFinding { finding, citation }
            })
            .collect(),
    })
}

fn normalise(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

pub fn check_citation(finding: &Finding, targets: &Targets) -> CitationStatus {
    let Some(first) = targets.working_directories.first() else {
        return CitationStatus::OutsideWorkingDirectories;
    };
    let supplied = Path::new(&finding.path);
    let remote = super::targets::is_zen_storage_path(supplied)
        || (!supplied.is_absolute()
            && super::targets::is_zen_storage_path(first));
    if remote {
        let candidate = if super::targets::is_zen_storage_path(supplied) {
            supplied.to_path_buf()
        } else {
            first.join(supplied)
        };
        let candidate = normalise(&candidate);
        if !targets
            .working_directories
            .iter()
            .filter(|root| super::targets::is_zen_storage_path(root))
            .any(|root| candidate.starts_with(normalise(root)))
        {
            return CitationStatus::OutsideWorkingDirectories;
        }
        return if finding.start_line == 0 || finding.end_line < finding.start_line {
            CitationStatus::LinesOutOfRange
        } else {
            CitationStatus::RemoteUnverified
        };
    }
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        first.join(supplied)
    };
    // Do not open a path outside the permitted roots, including symlink escapes.
    // Canonicalisation uses the original path so '..' after a symlink retains OS semantics.
    let path = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(_) => {
            return if targets
                .working_directories
                .iter()
                .any(|root| normalise(&candidate).starts_with(root))
            {
                CitationStatus::PathNotFound
            } else {
                CitationStatus::OutsideWorkingDirectories
            };
        }
    };
    if !targets
        .working_directories
        .iter()
        .any(|root| path.starts_with(root))
    {
        return CitationStatus::OutsideWorkingDirectories;
    }
    // A directory or special file is not a source citation. No directory read occurs.
    if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return CitationStatus::PathNotFound;
    }
    let Ok(bytes) = fs::read(path) else {
        return CitationStatus::PathNotFound;
    };
    let lines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64
        + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    if finding.start_line == 0 || finding.end_line < finding.start_line || finding.end_line > lines
    {
        CitationStatus::LinesOutOfRange
    } else {
        CitationStatus::Ok
    }
}
