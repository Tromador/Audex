//! Regenerated exclusively from the run record and retained attempt files.
use super::record;
use super::record::AttemptStatus;
use super::record::RunRecord;
use super::result::CheckedResult;
use anyhow::Result;
use std::fmt::Write;
use std::path::Path;

// Treat reviewer prose as text, so embedded Markdown cannot masquerade as run status.
fn prose(out: &mut String, text: &str) {
    // A fence at column zero ends any preceding list. Make it longer than
    // every backtick run in the text, including embedded or unclosed fences.
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    let _ = writeln!(out, "\n{fence}");
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    let _ = writeln!(out, "{fence}\n");
}

fn word(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn status_line(out: &mut String, status: &AttemptStatus) {
    let _ = writeln!(
        out,
        "- Attempt {} ({}): {}",
        status.attempt,
        word(&status.trigger).replace('_', " "),
        word(&status.state)
    );
    if let Some(reason) = &status.reason {
        prose(out, reason);
    }
}

pub fn render(run_dir: &Path) -> Result<String> {
    let run: RunRecord = record::read_json(&run_dir.join("run.json"))?;
    let mut out = String::from("# Audit summary\n\nTargets:\n\n");
    for target in &run.targets.paths {
        prose(&mut out, &target.display().to_string());
    }
    writeln!(out, "Created: {}\n", run.created_at)?;
    out.push_str("Reviewers:\n\n");
    for (index, reviewer) in run.reviewers.iter().enumerate() {
        let attempts = record::attempts(&record::reviewer_dir(run_dir, index, reviewer))?;
        let state = attempts
            .last()
            .map(|(_, status)| word(&status.state))
            .unwrap_or_else(|| "no attempt recorded".into());
        writeln!(out, "- {}: {state}", reviewer.label)?;
    }
    out.push('\n');
    for (index, reviewer) in run.reviewers.iter().enumerate() {
        writeln!(out, "## {}\n", reviewer.label)?;
        prose(
            &mut out,
            &format!(
                "{} / {}{}",
                reviewer.route,
                reviewer.model,
                reviewer
                    .effort
                    .as_ref()
                    .map(|e| format!(" / {e}"))
                    .unwrap_or_default()
            ),
        );
        let attempts = record::attempts(&record::reviewer_dir(run_dir, index, reviewer))?;
        if attempts.is_empty() {
            out.push_str("No attempt was recorded.\n\n");
        }
        for (directory, status) in attempts {
            status_line(&mut out, &status);
            if directory.join("result.json").try_exists()? {
                let result: CheckedResult = record::read_json(&directory.join("result.json"))?;
                prose(&mut out, &result.summary);
                writeln!(out, "Findings: {}\n", result.findings.len())?;
                for (number, checked) in result.findings.iter().enumerate() {
                    let finding = &checked.finding;
                    prose(
                        &mut out,
                        &format!(
                            "{}. {} [{}; {} confidence]\n{}:{}-{} — citation: {}\nTrigger: {}\nMechanism: {}\nImpact: {}\nEvidence: {}\nRemediation: {}",
                            number + 1,
                            finding.title,
                            word(&finding.severity),
                            word(&finding.confidence),
                            finding.path,
                            finding.start_line,
                            finding.end_line,
                            word(&checked.citation),
                            finding.trigger,
                            finding.mechanism,
                            finding.impact,
                            finding.evidence,
                            finding.remediation
                        ),
                    );
                }
            } else if let Some(output) = record::read_optional(&directory.join("output.txt"))? {
                if !output.is_empty() {
                    out.push_str("Unstructured opinion (raw output retained):\n\n");
                    prose(&mut out, &String::from_utf8_lossy(&output));
                }
            }
        }
    }
    out.push_str("Citation checks are location checks, not verification of findings. Agreement is not verification.\n");
    record::write_atomic(&run_dir.join("summary.md"), out.as_bytes())?;
    Ok(out)
}

/// Render one retained attempt for direct display without changing its wording
/// or the aggregate summary used by comparison. This never writes run files.
pub(super) fn render_review(
    run: &RunRecord,
    directory: &Path,
    status: &AttemptStatus,
) -> Result<String> {
    let mut out = format!(
        "# Review — {}\n\n**Reviewer:** {} / {}\n\n**Audit created:** {}\n\n**Attempt:** {} — {}\n\n",
        status.reviewer.label,
        status.reviewer.route,
        status.reviewer.model,
        run.created_at,
        status.attempt,
        word(&status.state)
    );
    if let Some(bytes) = record::read_optional(&directory.join("result.json"))? {
        let result: CheckedResult = serde_json::from_slice(&bytes)?;
        writeln!(out, "## Summary\n\n{}\n", result.summary)?;
        for (index, checked) in result.findings.iter().enumerate() {
            let finding = &checked.finding;
            writeln!(out, "## Finding {}: {}\n", index + 1, finding.title)?;
            writeln!(
                out,
                "**Severity:** {}\n\n**Confidence:** {}\n\n**Source:** {}:{}-{}\n\n**Citation check:** {}\n",
                word(&finding.severity),
                word(&finding.confidence),
                finding.path,
                finding.start_line,
                finding.end_line,
                word(&checked.citation)
            )?;
            for (label, text) in [
                ("Trigger", &finding.trigger),
                ("Mechanism", &finding.mechanism),
                ("Impact", &finding.impact),
                ("Evidence", &finding.evidence),
                ("Remediation", &finding.remediation),
            ] {
                writeln!(out, "**{label}:**\n\n{text}\n")?;
            }
        }
    } else {
        let output = record::read_optional(&directory.join("output.txt"))?
            .filter(|bytes| !bytes.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No retained review for {} attempt {} ({})",
                    status.reviewer.label,
                    status.attempt,
                    word(&status.state)
                )
            })?;
        out.push_str("## Review\n\n");
        out.push_str(&String::from_utf8(output)?);
    }
    Ok(out)
}
