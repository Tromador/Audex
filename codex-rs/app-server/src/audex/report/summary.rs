//! A transient findings view; never a replacement for the immutable report.
use super::collection::CollectionInput;
use super::{ReportFinding, escape_inline, push_line};
use crate::audex::snapshot::SnapshotManifestV1;

pub(in crate::audex) fn render(input: &CollectionInput, manifest: &SnapshotManifestV1) -> String {
    let mut out = format!(
        "# Audit findings\n\nRun: {}\n\n",
        escape_inline(&input.run_id)
    );
    push_line(&mut out, "**Audited scope**");
    for entry in &manifest.identity.entries {
        push_line(
            &mut out,
            &format!(
                "- {} ({} captured bytes)",
                escape_inline(entry.path.as_str()),
                entry.size
            ),
        );
    }
    push_line(
        &mut out,
        &format!("\nSnapshot: {}\n", escape_inline(&input.snapshot_id)),
    );
    let completed_count = input
        .reviewers
        .iter()
        .filter(|reviewer| reviewer.completed.is_some() || reviewer.opinion.is_some())
        .count();
    if input.outcome == "aborted_by_operator" {
        push_line(
            &mut out,
            "**Audit aborted by operator. Completed reviewer opinions remain available.**\n",
        );
    }
    push_line(&mut out, &match completed_count {
        0 => "No reviewer completed. No completed findings are available.\n".to_owned(),
        1 => "One reviewer completed. Its findings are presented below; no cross-reviewer comparison is available.\n".to_owned(),
        count => format!("{count} reviewers completed. Only their completed opinions participate in the Codex comparison.\n"),
    });
    push_line(&mut out, "**Independent reviewers**");
    for reviewer in &input.reviewers {
        let status = if let Some(completed) = &reviewer.completed {
            format!(
                "completed; {} structured findings",
                completed.findings.len()
            )
        } else if reviewer.opinion.is_some() {
            "completed; unstructured opinion, no extracted findings".to_owned()
        } else {
            incomplete_status(&reviewer.assignment_state, &reviewer.disposition)
        };
        push_line(
            &mut out,
            &format!(
                "- Reviewer {}: {} / {} — {}{}",
                reviewer.reviewer_ordinal + 1,
                escape_inline(&reviewer.provider),
                escape_inline(&reviewer.resolved_model),
                escape_inline(&status),
                reviewer
                    .failure_code
                    .as_ref()
                    .map(|reason| format!(" ({})", excerpt(reason, 240)))
                    .unwrap_or_default()
            ),
        );
    }
    push_line(
        &mut out,
        "\nThese are reviewer claims. Verified source locations do not verify a defect. Failed or aborted jobs without completed opinions do not count as agreement, disagreement or clean reviews.\n",
    );
    if completed_count >= 2 {
        push_line(
            &mut out,
            "Relationships between claims have not yet been assessed; the Codex comparison is a separate, attributed assessment.\n",
        );
    }
    for reviewer in &input.reviewers {
        let label = format!(
            "Reviewer {}: {}",
            reviewer.reviewer_ordinal + 1,
            escape_inline(&reviewer.resolved_model)
        );
        push_line(&mut out, &format!("## {label}\n"));
        if let Some(completed) = &reviewer.completed {
            push_line(
                &mut out,
                &format!(
                    "Reviewer summary (unverified): {}\n",
                    excerpt(&completed.summary, 280)
                ),
            );
            if completed.findings.is_empty() {
                push_line(
                    &mut out,
                    "No findings reported. This is not proof that the source is defect-free.\n",
                );
            }
            for finding in &completed.findings {
                finding_summary(&mut out, &label, &reviewer.reviewer_id, finding);
            }
        } else if let Some(opinion) = &reviewer.opinion {
            push_line(
                &mut out,
                "Unstructured reviewer opinion (unverified; retained in full in diagnostics):",
            );
            push_line(
                &mut out,
                &excerpt(opinion.text.as_deref().unwrap_or(""), 360),
            );
        } else {
            push_line(
                &mut out,
                "No accepted findings. Any incomplete output remains available in diagnostics.",
            );
            if let Some(reason) = &reviewer.stop_reason {
                push_line(&mut out, &excerpt(reason, 240));
            }
        }
        push_line(&mut out, "");
    }
    push_line(
        &mut out,
        "Full findings and every evidence item remain available through audit_read; captured source through audit_source. Longer prose above is explicitly marked as an excerpt.",
    );
    push_line(
        &mut out,
        &format!(
            "\nDiagnostics: /audit diagnostics {} (TUI), or audex audit read {} --diagnostics (CLI).",
            escape_inline(&input.run_id),
            escape_inline(&input.run_id)
        ),
    );
    out
}

/// Terminal execution can fail before a frozen report exists (for example, corrupt capture).
/// Show honest status without opening the unavailable source or inventing an opinion.
pub(in crate::audex) fn without_completed_report(
    run_id: &str,
    snapshot_id: &str,
    outcome: Option<&str>,
    reviewers: &[codex_app_server_protocol::AuditAttemptView],
) -> String {
    let mut out = format!(
        "# Audit findings\n\nRun: {}\n\nSnapshot: {}\n\nNo reviewer completed. No completed findings are available.\n\nAudit outcome: {}\n\n",
        escape_inline(run_id),
        escape_inline(snapshot_id),
        excerpt(outcome.unwrap_or("no terminal explanation"), 240)
    );
    for reviewer in reviewers {
        push_line(
            &mut out,
            &format!(
                "- Reviewer {}: {} / {} — {}",
                reviewer.reviewer_ordinal + 1,
                escape_inline(&reviewer.provider),
                escape_inline(&reviewer.resolved_model),
                escape_inline(&incomplete_status(
                    &reviewer.assignment_state,
                    &reviewer.disposition
                ))
            ),
        );
    }
    push_line(
        &mut out,
        "\nNo completed report was published. Retained attempt activity remains available; incomplete output is not a completed opinion or a clean review.",
    );
    out
}

fn incomplete_status(assignment_state: &str, disposition: &str) -> String {
    let disposition = match disposition {
        "cancelled" => "aborted/cancelled by operator",
        "ignored" => "excluded by operator",
        other => other,
    };
    format!(
        "execution state at publication: {assignment_state}; {disposition}; no accepted completed opinion"
    )
}

fn finding_summary(out: &mut String, reviewer: &str, reviewer_id: &str, finding: &ReportFinding) {
    push_line(
        out,
        &format!(
            "### {}. {}\n",
            finding.ordinal + 1,
            excerpt(&finding.title, 512)
        ),
    );
    push_line(
        out,
        &format!(
            "{}; finding {}. Severity: {}; confidence: {}; verification: {}.",
            reviewer,
            finding.ordinal + 1,
            escape_inline(&finding.severity),
            escape_inline(&finding.confidence),
            escape_inline(&finding.verification_state)
        ),
    );
    push_line(
        out,
        &format!(
            "\nMechanism / trigger (reviewer claim): {}",
            excerpt(&finding.mechanism, 400)
        ),
    );
    push_line(
        out,
        &format!(
            "\nImpact (reviewer claim): {}",
            excerpt(&finding.impact, 280)
        ),
    );
    for evidence in &finding.evidence {
        let lines = match (evidence.start_line, evidence.end_line) {
            (Some(start), Some(end)) => format!(
                "; {} lines {start}–{end}",
                if evidence.lines_verified {
                    "verified source"
                } else {
                    "unverified reviewer"
                }
            ),
            _ => String::new(),
        };
        push_line(
            out,
            &format!(
                "- Evidence: {} bytes {}..{}{}: {}",
                escape_inline(&evidence.manifest_path),
                evidence.start_byte,
                evidence.end_byte,
                lines,
                excerpt(&evidence.description, 180)
            ),
        );
    }
    if let Some(remediation) = &finding.remediation {
        push_line(
            out,
            &format!(
                "\nSuggested correction (reviewer claim): {}",
                excerpt(remediation, 200)
            ),
        );
    }
    push_line(
        out,
        &format!(
            "\nFull finding: audit_read view=finding, reviewer={}, number={}.\n",
            escape_inline(reviewer_id),
            finding.ordinal + 1
        ),
    );
}

fn excerpt(value: &str, characters: usize) -> String {
    let visible: String = value
        .chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    let end = visible.char_indices().nth(characters).map(|(i, _)| i);
    match end {
        Some(end) => format!(
            "{} … [excerpt; full text retained]",
            escape_inline(&visible[..end])
        ),
        None => escape_inline(&visible),
    }
}

#[cfg(test)]
mod tests;
