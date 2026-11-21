//! Operator choices for one retained audit. Reviewer controls identify the attempt
//! the operator saw, even when several reviewers use the same model.
use super::*;
use codex_app_server_protocol::AuditReadResult;

pub(crate) struct MenuItem {
    pub name: String,
    pub action: AuditAction,
    pub attempt: Option<u32>,
}

fn unsuccessful(status: &AuditReviewerStatus) -> bool {
    matches!(
        status.state.as_str(),
        "failed" | "cancelled" | "interrupted"
    )
}

fn single_success(run: &AuditRun) -> Option<&AuditReviewerStatus> {
    let reviewers = latest(run);
    let mut successes = reviewers.iter().copied().filter(|s| s.state == "succeeded");
    let success = successes.next()?;
    (successes.next().is_none()
        && reviewers.len() > 1
        && reviewers
            .iter()
            .all(|s| s.state == "succeeded" || unsuccessful(s)))
    .then_some(success)
}

fn reviewer_read(
    run: &AuditRun,
    status: &AuditReviewerStatus,
    view: AuditReadView,
) -> AuditReadRequest {
    AuditReadRequest {
        view: Some(view),
        reviewer: Some(status.reviewer.clone()),
        attempt: Some(status.attempt),
        ..read_run(run.run_id.clone())
    }
}

pub(crate) fn items(run: &AuditRun) -> Vec<MenuItem> {
    let mut items = Vec::new();
    let mut running = false;
    for status in latest(run) {
        let identity = format!(
            "{} ({}:{}, {})",
            status.reviewer, status.selection.route, status.selection.model, status.state
        );
        if status.state == "succeeded" || status.has_output || status.finding_count.is_some() {
            items.push(MenuItem {
                name: format!("View review — {identity}"),
                action: AuditAction::Read(reviewer_read(run, status, AuditReadView::Report)),
                attempt: Some(status.attempt),
            });
        }
        let (verb, action) = match status.state.as_str() {
            "queued" | "running" => {
                running = true;
                (
                    "Cancel",
                    AuditAction::Cancel(AuditCancelRequest {
                        run_id: run.run_id.clone(),
                        reviewer: Some(status.reviewer.clone()),
                    }),
                )
            }
            _ if unsuccessful(status) => (
                "Retry",
                AuditAction::Retry(AuditRetryRequest {
                    run_id: run.run_id.clone(),
                    reviewer: status.reviewer.clone(),
                }),
            ),
            _ => continue,
        };
        items.push(MenuItem {
            name: format!("{verb} — {identity}"),
            action,
            attempt: Some(status.attempt),
        });
    }
    if running {
        items.push(MenuItem {
            name: "Cancel whole audit".into(),
            action: AuditAction::Cancel(AuditCancelRequest {
                run_id: run.run_id.clone(),
                reviewer: None,
            }),
            attempt: None,
        });
    }
    if let Some(success) = single_success(run) {
        let (view, limit) = if success.finding_count.is_some() {
            (AuditReadView::Findings, 5)
        } else {
            (AuditReadView::Output, 600)
        };
        let mut request = reviewer_read(run, success, view);
        request.limit = Some(limit);
        items.push(MenuItem {
            name: "Proceed with the available review".into(),
            action: AuditAction::Proceed(request),
            attempt: Some(success.attempt),
        });
    }
    items
}

// Cancellation and retry address the reviewer on the wire. Recheck the latest
// attempt before dispatching a control selected from a previously opened menu.
pub(crate) async fn validate_control(
    client: &AuditClient,
    run_id: String,
    action: &AuditAction,
    attempt: Option<u32>,
) -> Result<(), String> {
    if matches!(action, AuditAction::Cancel(_) | AuditAction::Retry(_)) {
        let result = client
            .read(read_run(run_id))
            .await
            .map_err(|e| e.to_string())?;
        if !items(&result.run)
            .iter()
            .any(|item| &item.action == action && item.attempt == attempt)
        {
            return Err("This audit has changed. Reopen /audit for its current actions.".into());
        }
    }
    Ok(())
}

pub(super) fn single_review_summary(
    result: &AuditReadResult,
    request: &AuditReadRequest,
) -> Result<ReportPresentation, String> {
    let run = &result.run;
    let success = single_success(run)
        .filter(|s| {
            Some(s.reviewer.as_str()) == request.reviewer.as_deref()
                && Some(s.attempt) == request.attempt
        })
        .ok_or("The available reviews have changed. Reopen /audit.")?;
    let data = result
        .data
        .as_ref()
        .ok_or("Missing retained review material")?;
    let mut lines = vec![
        format!("## Single-review summary — {}", label(run)),
        format!(
            "Available review: {} ({}:{}, attempt {}).",
            success.reviewer, success.selection.route, success.selection.model, success.attempt
        ),
    ];
    for status in latest(run).into_iter().filter(|s| unsuccessful(s)) {
        lines.push(format!(
            "No successful review from {} ({}:{}) — {}.",
            status.reviewer, status.selection.route, status.selection.model, status.state
        ));
    }
    if let Some(count) = success.finding_count {
        let findings = data["items"]
            .as_array()
            .ok_or("Missing retained findings")?;
        lines.push(format!(
            "{} retained findings ({} shown).",
            count,
            findings.len()
        ));
        for entry in findings {
            let finding = &entry["finding"];
            let title = finding["title"]
                .as_str()
                .ok_or("Missing retained finding title")?;
            let path = finding["path"]
                .as_str()
                .ok_or("Missing retained finding path")?;
            let start = finding["start_line"]
                .as_u64()
                .ok_or("Missing retained finding location")?;
            let end = finding["end_line"]
                .as_u64()
                .ok_or("Missing retained finding location")?;
            lines.push(format!("- {title} — {path}:{start}-{end}"));
        }
    } else {
        let content = data["content"].as_str().ok_or("Missing retained output")?;
        lines.push(if content.is_empty() {
            "No review text was retained.".into()
        } else {
            format!("Retained output excerpt:\n\n{content}")
        });
    }
    Ok(ReportPresentation {
        markdown: lines.join("\n\n"),
        cwd: None,
    })
}
