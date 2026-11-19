//! Read retained run files, never the audited source.
use super::*;
use codex_app_server_protocol::{
    AuditReadRequest, AuditReadResult, AuditReadView, AuditReviewer, AuditReviewerStatus, AuditRun,
};
use serde_json::{Value, json};

pub(crate) const SOURCE_NOTICE: &str = "Source is live and may have changed since the review. Read it with your own source tools and state relevant differences. Agreement is not verification.";

pub(crate) fn selection(value: &ReviewerSelection) -> AuditReviewer {
    AuditReviewer {
        route: value.route.clone(),
        model: value.model.clone(),
        effort: value.effort.clone(),
    }
}

pub(crate) fn status(directory: &Path, value: &AttemptStatus) -> Result<AuditReviewerStatus> {
    let result = record::read_optional(&directory.join("result.json"))?
        .map(|bytes| serde_json::from_slice::<result::CheckedResult>(&bytes))
        .transpose()?;
    Ok(AuditReviewerStatus {
        reviewer: value.reviewer.label.clone(),
        selection: selection(&value.reviewer),
        attempt: value.attempt,
        trigger: serde_json::to_value(value.trigger)?
            .as_str()
            .context("trigger")?
            .into(),
        state: serde_json::to_value(value.state)?
            .as_str()
            .context("state")?
            .into(),
        reason: value.reason.clone(),
        tool_calls: value.tool_calls,
        finding_count: result.map(|r| r.findings.len()),
        has_output: directory
            .join("output.txt")
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false),
    })
}

pub(crate) fn record(directory: &Path) -> Result<RunRecord> {
    record::read_json(&directory.join("run.json"))
}

pub(crate) fn reviewer_index(run: &RunRecord, selector: &str) -> Result<usize> {
    if let Ok(number) = selector.parse::<usize>() {
        ensure!(
            number > 0 && number <= run.reviewers.len(),
            "unknown reviewer number"
        );
        return Ok(number - 1);
    }
    run.reviewers
        .iter()
        .position(|r| r.label == selector)
        .context("unknown reviewer label")
}

pub(crate) fn view(directory: &Path) -> Result<AuditRun> {
    let run = record(directory)?;
    let mut reviewers = Vec::new();
    let mut complete = true;
    for (index, reviewer) in run.reviewers.iter().enumerate() {
        record::validate_label(&reviewer.label)?;
        let attempts = record::attempts(&record::reviewer_dir(directory, index, reviewer))?;
        complete &= attempts
            .last()
            .is_some_and(|(_, s)| !matches!(s.state, State::Queued | State::Running));
        for (path, attempt) in attempts {
            reviewers.push(status(&path, &attempt)?);
        }
    }
    let summary = if complete {
        record::read_optional(&directory.join("summary.md"))?
            .map(String::from_utf8)
            .transpose()?
    } else {
        None
    };
    Ok(AuditRun {
        run_id: run.id.to_string(),
        targets: run
            .targets
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        created_at: run.created_at,
        reviewers,
        complete: complete && summary.is_some(),
        summary,
        source_notice: SOURCE_NOTICE.into(),
    })
}

pub(crate) fn list(root: &Path) -> Result<Vec<AuditRun>> {
    let mut ids = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|s| Uuid::parse_str(s).is_ok())
            && entry.path().join("run.json").try_exists()?
        {
            ids.push(entry.path());
        }
    }
    ids.sort();
    ids.reverse();
    ids.iter().map(|path| view(path)).collect()
}

pub(crate) fn read(directory: &Path, params: &AuditReadRequest) -> Result<AuditReadResult> {
    let run = view(directory)?;
    let Some(query) = &params.view else {
        return Ok(AuditReadResult { run, data: None });
    };
    ensure!(params.limit != Some(0), "limit must be positive");
    let record = record(directory)?;
    let selected = params
        .reviewer
        .as_deref()
        .map(|s| reviewer_index(&record, s))
        .transpose()?;
    if matches!(query, AuditReadView::Finding | AuditReadView::Output) {
        ensure!(selected.is_some(), "select a reviewer");
    }
    let mut entries = Vec::new();
    for (index, reviewer) in record.reviewers.iter().enumerate() {
        if selected.is_some_and(|s| s != index) {
            continue;
        }
        record::validate_label(&reviewer.label)?;
        let attempts = record::attempts(&record::reviewer_dir(directory, index, reviewer))?;
        let latest = attempts.last().map(|(_, s)| s.attempt);
        if let Some(number) = params.attempt {
            ensure!(
                attempts.iter().any(|(_, s)| s.attempt == number),
                "unknown attempt"
            );
        }
        for (path, attempt) in attempts {
            if params.attempt.is_some_and(|n| n != attempt.attempt) {
                continue;
            }
            if !matches!(query, AuditReadView::Reviewers | AuditReadView::Diagnostics)
                && Some(attempt.attempt) != params.attempt.or(latest)
            {
                continue;
            }
            match query {
                AuditReadView::Reviewers => {
                    entries.push(serde_json::to_value(status(&path, &attempt)?)?)
                }
                AuditReadView::Diagnostics => {
                    let usage = record::read_optional(&path.join("usage.json"))?
                        .map(|s| serde_json::from_slice::<Value>(&s))
                        .transpose()?;
                    let stderr = record::read_optional(&path.join("stderr.log"))?
                        .map(|s| String::from_utf8_lossy(&s).into_owned())
                        .unwrap_or_default();
                    let tail = stderr
                        .lines()
                        .rev()
                        .take(40)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");
                    let transcript = if path.join("transcript.jsonl").exists() {
                        Some(path.join("transcript.jsonl"))
                    } else {
                        attempt.rollout_path.clone()
                    };
                    entries.push(json!({"reviewer":reviewer.label,"attempt":attempt.attempt,"status":attempt,"usage":usage,"stderrTail":tail,"transcriptPath":transcript,"toolCallSummary":{"count":attempt.tool_calls}}));
                }
                AuditReadView::Output => {
                    let output = record::read_optional(&path.join("output.txt"))?;
                    let text = output
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default();
                    let total = text.chars().count();
                    let content: String = text
                        .chars()
                        .skip(params.offset)
                        .take(params.limit.unwrap_or(usize::MAX))
                        .collect();
                    let end = params.offset.min(total) + content.chars().count();
                    return Ok(AuditReadResult {
                        run,
                        data: Some(
                            json!({"reviewer":reviewer.label,"attempt":attempt.attempt,"content":content,"available":output.is_some(),"nextOffset":(end < total).then_some(end),"sourceNotice":SOURCE_NOTICE}),
                        ),
                    });
                }
                AuditReadView::Findings | AuditReadView::Finding => {
                    if let Some(bytes) = record::read_optional(&path.join("result.json"))? {
                        let result: result::CheckedResult = serde_json::from_slice(&bytes)?;
                        for (number, finding) in result.findings.into_iter().enumerate() {
                            if matches!(query, AuditReadView::Finding)
                                && params.number != Some(number + 1)
                            {
                                continue;
                            }
                            entries.push(json!({"reviewer":reviewer.label,"attempt":attempt.attempt,"number":number+1,"state":attempt.state,"finding":finding,"verification":"unverified model claim"}));
                        }
                    }
                }
            }
        }
    }
    if matches!(query, AuditReadView::Finding) {
        ensure!(!entries.is_empty(), "unknown finding number");
    }
    let total = entries.len();
    let page: Vec<_> = entries
        .into_iter()
        .skip(params.offset)
        .take(params.limit.unwrap_or(usize::MAX))
        .collect();
    let end = params.offset.min(total) + page.len();
    Ok(AuditReadResult {
        run,
        data: Some(
            json!({"items":page,"nextOffset":(end < total).then_some(end),"sourceNotice":SOURCE_NOTICE}),
        ),
    })
}
