//! Versioned immutable run-wide report input. Operational receipts are not live inputs.
use super::*;
use crate::audex::store::activity::ActivityRecord;

pub(in crate::audex) const FORMAT_VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
pub(in crate::audex) struct CollectionInput {
    pub format_version: u32,
    pub selection_id: String,
    pub generation: i64,
    pub run_id: String,
    pub snapshot_id: String,
    pub manifest_digest: String,
    pub brief_digest: String,
    pub outcome: String,
    pub reviewers: Vec<ReviewerInput>,
}

#[derive(Serialize, Deserialize)]
pub(in crate::audex) struct ReviewerInput {
    pub reviewer_id: String,
    pub reviewer_ordinal: i64,
    pub assignment_id: String,
    pub assignment_ordinal: i64,
    pub attempt_ordinal: i64,
    pub operation_id: String,
    pub provider: String,
    pub requested_model: String,
    pub resolved_model: String,
    pub observed_model: Option<String>,
    pub observation_verified: bool,
    pub provider_caveats: Vec<String>,
    pub input_digest: String,
    pub policy_digest: String,
    pub disposition: String,
    pub assignment_state: String,
    pub failure_code: Option<String>,
    pub stop_reason: Option<String>,
    pub result_digest: Option<String>,
    pub result_path: Option<String>,
    pub completed: Option<ReportInput>,
    pub cutoff: i64,
    pub activity_digest: String,
    pub activity: Vec<ActivityRecord>,
    pub capture_gap: Option<String>,
    pub capture_failure: Option<String>,
    pub capture_truncated: bool,
    /// Absent only in the independent historical format-3 carrier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_facts: Option<super::facts::ProviderFacts>,
}

pub(in crate::audex) fn render(
    input: &CollectionInput,
) -> Result<RenderedReport, serde_json::Error> {
    validate(input)?;
    if input.format_version == 3 {
        let historical = serde_json::from_slice(&serde_json::to_vec(input)?)?;
        return super::collection_v3::render(&historical);
    }
    let input_digest = sha256_hex(&serde_json::to_vec(&(FORMAT_VERSION, input))?);
    let id = format!("audex-report-v{FORMAT_VERSION}:sha256:{input_digest}");
    let mut output = format!(
        "# Audex Audit Report\n\n- Report format: `{FORMAT_VERSION}`\n- Run: `{}`\n- Snapshot: `{}`\n- Selection: `{}`\n- Outcome: `{}`\n\n",
        escape_code(&input.run_id),
        escape_code(&input.snapshot_id),
        escape_code(&input.selection_id),
        escape_code(&input.outcome)
    );
    for reviewer in &input.reviewers {
        push_line(
            &mut output,
            &format!(
                "## Reviewer {}: {}",
                reviewer.reviewer_ordinal + 1,
                escape_inline(&reviewer.resolved_model)
            ),
        );
        push_line(&mut output, "");
        for (label, value) in [
            ("Reviewer", reviewer.reviewer_id.as_str()),
            ("Assignment", &reviewer.assignment_id),
            ("Operation", &reviewer.operation_id),
            ("Provider", &reviewer.provider),
            ("Requested model", &reviewer.requested_model),
            ("Resolved model", &reviewer.resolved_model),
            (
                "Observed served model",
                reviewer
                    .observed_model
                    .as_deref()
                    .unwrap_or("unavailable; unverified"),
            ),
            ("Disposition", &reviewer.disposition),
            ("Execution state", &reviewer.assignment_state),
        ] {
            push_line(&mut output, &format!("- {label}: `{}`", escape_code(value)));
        }
        push_line(
            &mut output,
            &format!("- Attempt: `{}`", reviewer.attempt_ordinal),
        );
        if let Some(facts) = &reviewer.provider_facts {
            super::facts::render(&mut output, facts)?;
        }
        for caveat in &reviewer.provider_caveats {
            write_section(&mut output, "Provider caveat", caveat);
        }
        if let Some(completed) = &reviewer.completed {
            push_line(
                &mut output,
                &format!(
                    "- Completeness: completed\n- Canonical result digest: `{}`\n",
                    reviewer.result_digest.as_deref().unwrap_or("")
                ),
            );
            // Reuse the established finding/evidence renderer, with section levels
            // nested beneath this independent reviewer. No synthesis or voting.
            let rendered = super::render(completed)?;
            let text = String::from_utf8(rendered.bytes).expect("renderer produces UTF-8");
            for line in text.lines().skip_while(|line| *line != "## Summary") {
                if line.starts_with("##") {
                    output.push('#');
                }
                push_line(&mut output, line);
            }
        } else {
            push_line(
                &mut output,
                "- Completeness: incomplete / excluded; no accepted completed opinion",
            );
            if let Some(reason) = &reviewer.stop_reason {
                write_section(&mut output, "Exclusion / stop reason", reason);
            }
            if let Some(reason) = &reviewer.failure_code {
                write_section(&mut output, "Execution failure reason", reason);
            }
            push_line(
                &mut output,
                &format!(
                    "- Received prefix cutoff: `{}`\n- Prefix digest: `{}`",
                    reviewer.cutoff, reviewer.activity_digest
                ),
            );
            for (label, value) in [
                ("Capture gap", &reviewer.capture_gap),
                ("Capture failure", &reviewer.capture_failure),
            ] {
                if let Some(value) = value {
                    write_section(&mut output, label, value);
                }
            }
            if reviewer.capture_truncated {
                push_line(&mut output, "- Capture truncated: yes");
            }
            write_section(
                &mut output,
                "Incomplete assistant output (unvalidated; not findings)",
                &reviewer
                    .activity
                    .iter()
                    .filter(|entry| {
                        matches!(entry.kind.as_str(), "assistant_delta" | "assistant_message")
                    })
                    .map(|entry| entry.payload.as_str())
                    .collect::<String>(),
            );
            push_line(
                &mut output,
                "\n**Source/tool activity and status (not findings)**\n",
            );
            for entry in &reviewer.activity {
                if !matches!(entry.kind.as_str(), "assistant_delta" | "assistant_message") {
                    write_section(&mut output, &entry.kind, &entry.payload);
                }
            }
        }
        push_line(&mut output, "");
    }
    let bytes = output.into_bytes();
    Ok(RenderedReport {
        id,
        input_digest,
        content_digest: sha256_hex(&bytes),
        bytes,
    })
}

/// Decode each frozen version with its own contract before using the common
/// selection carrier. Historical fields are never filled from live store state.
pub(in crate::audex) fn decode(json: &str) -> Result<CollectionInput, serde_json::Error> {
    #[derive(Deserialize)]
    struct Version {
        format_version: u32,
    }
    let version: Version = serde_json::from_str(json)?;
    let input = match version.format_version {
        3 => {
            let historical: super::collection_v3::CollectionInput = serde_json::from_str(json)?;
            serde_json::from_slice(&serde_json::to_vec(&historical)?)?
        }
        FORMAT_VERSION => serde_json::from_str(json)?,
        _ => return Err(invalid("unsupported frozen report format")),
    };
    validate(&input)?;
    Ok(input)
}

fn invalid(message: &str) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}

fn validate(input: &CollectionInput) -> Result<(), serde_json::Error> {
    if !matches!(input.format_version, 3 | FORMAT_VERSION)
        || input
            .reviewers
            .iter()
            .any(|r| r.provider_facts.is_some() != (input.format_version == FORMAT_VERSION))
    {
        return Err(invalid("frozen report version and provider facts disagree"));
    }
    Ok(())
}
