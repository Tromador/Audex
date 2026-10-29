use super::snapshot::manifest::sha256_hex;
use serde::Serialize;

pub(super) const REPORT_FORMAT_VERSION: u32 = 1;

#[derive(Serialize)]
pub(super) struct ReportInput {
    pub run_id: String,
    pub snapshot_id: String,
    pub assignment_id: String,
    pub summary: String,
    pub findings: Vec<ReportFinding>,
}

#[derive(Serialize)]
pub(super) struct ReportFinding {
    pub id: String,
    pub ordinal: i64,
    pub severity: String,
    pub confidence: String,
    pub title: String,
    pub mechanism: String,
    pub impact: String,
    pub remediation: Option<String>,
    pub verification_state: String,
    pub evidence: Vec<ReportEvidence>,
}

#[derive(Serialize)]
pub(super) struct ReportEvidence {
    pub ordinal: i64,
    pub description: String,
    pub manifest_path: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
}

pub(super) struct RenderedReport {
    pub id: String,
    pub input_digest: String,
    pub content_digest: String,
    pub bytes: Vec<u8>,
}

pub(super) fn render(input: &ReportInput) -> Result<RenderedReport, serde_json::Error> {
    let input_digest = sha256_hex(&serde_json::to_vec(input)?);
    let id = format!("audex-report-v1:sha256:{input_digest}");
    let mut markdown = String::new();
    push_line(&mut markdown, "# Audex Audit Report");
    push_line(&mut markdown, "");
    push_line(
        &mut markdown,
        &format!("- Report format: `{REPORT_FORMAT_VERSION}`"),
    );
    push_line(
        &mut markdown,
        &format!("- Run: `{}`", escape_code(&input.run_id)),
    );
    push_line(
        &mut markdown,
        &format!("- Assignment: `{}`", escape_code(&input.assignment_id)),
    );
    push_line(
        &mut markdown,
        &format!("- Snapshot: `{}`", escape_code(&input.snapshot_id)),
    );
    push_line(&mut markdown, "");
    push_line(&mut markdown, "## Summary");
    push_line(&mut markdown, "");
    write_quoted(&mut markdown, &input.summary);
    push_line(&mut markdown, "");
    push_line(&mut markdown, "## Findings");
    push_line(&mut markdown, "");

    if input.findings.is_empty() {
        push_line(&mut markdown, "No findings were reported.");
    } else {
        for finding in &input.findings {
            push_line(
                &mut markdown,
                &format!(
                    "### {}. {}",
                    finding.ordinal + 1,
                    escape_inline(&finding.title)
                ),
            );
            push_line(&mut markdown, "");
            push_line(
                &mut markdown,
                &format!("- Finding ID: `{}`", escape_code(&finding.id)),
            );
            push_line(
                &mut markdown,
                &format!("- Severity: `{}`", escape_code(&finding.severity)),
            );
            push_line(
                &mut markdown,
                &format!("- Confidence: `{}`", escape_code(&finding.confidence)),
            );
            push_line(
                &mut markdown,
                &format!(
                    "- Verification: `{}`",
                    escape_code(&finding.verification_state)
                ),
            );
            write_section(&mut markdown, "Mechanism", &finding.mechanism);
            write_section(&mut markdown, "Impact", &finding.impact);
            push_line(&mut markdown, "**Evidence**");
            push_line(&mut markdown, "");
            for evidence in &finding.evidence {
                let mut line = format!(
                    "- `{}` bytes `{}..{}`",
                    escape_code(&evidence.manifest_path),
                    evidence.start_byte,
                    evidence.end_byte
                );
                if let (Some(start), Some(end)) = (evidence.start_line, evidence.end_line) {
                    line.push_str(&format!(", lines `{start}..{end}`"));
                }
                line.push_str(&format!(": {}", escape_inline(&evidence.description)));
                push_line(&mut markdown, &line);
            }
            if let Some(remediation) = &finding.remediation {
                write_section(&mut markdown, "Remediation", remediation);
            }
            push_line(&mut markdown, "");
        }
    }

    let bytes = markdown.into_bytes();
    let content_digest = sha256_hex(&bytes);
    Ok(RenderedReport {
        id,
        input_digest,
        content_digest,
        bytes,
    })
}

fn write_section(output: &mut String, heading: &str, body: &str) {
    push_line(output, "");
    push_line(output, &format!("**{heading}**"));
    push_line(output, "");
    write_quoted(output, body);
}

fn write_quoted(output: &mut String, body: &str) {
    for line in body.lines() {
        push_line(output, &format!("> {line}"));
    }
}

fn push_line(output: &mut String, line: &str) {
    output.push_str(line);
    output.push('\n');
}

fn escape_inline(value: &str) -> String {
    value
        .replace(['\n', '\r'], " ")
        .chars()
        .flat_map(|character| match character {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' => vec!['\\', character],
            _ => vec![character],
        })
        .collect()
}

fn escape_code(value: &str) -> String {
    value.replace('`', "\\`")
}
