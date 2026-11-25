use super::*;

fn retained_run(fixture: &Fixture, state: State) -> Result<(RunId, PathBuf, PathBuf)> {
    let id = Uuid::now_v7();
    let directory = fixture.home.join("audex/runs").join(id.to_string());
    let run = RunRecord {
        id,
        targets: targets::resolve(std::slice::from_ref(&fixture.source))?,
        brief: "retained review".into(),
        guidance: None,
        reviewers: vec![selection("one")],
        created_at: record::now(),
        conversation: None,
    };
    let attempt = record::reviewer_dir(&directory, 0, &run.reviewers[0]).join("attempt-1");
    fs::create_dir_all(&attempt)?;
    record::write_json(&directory.join("run.json"), &run)?;
    let mut status = AttemptStatus::queued(1, Trigger::Initial, selection("one"));
    status.state = state;
    record::write_json(&attempt.join("status.json"), &status)?;
    Ok((id, directory, attempt))
}

#[test]
fn startup_isolates_malformed_runs_and_summary_failures() -> Result<()> {
    for damaged in ["run.json", "status.json", "result.json", "summary.md"] {
        let fixture = Fixture::new()?;
        let (bad_id, bad_run, bad_attempt) = retained_run(&fixture, State::Running)?;
        match damaged {
            "run.json" => fs::write(bad_run.join(damaged), b"invalid JSON")?,
            "summary.md" => fs::create_dir(bad_run.join(damaged))?,
            _ => fs::write(bad_attempt.join(damaged), b"invalid JSON")?,
        }
        let (good_id, good_run, _) = retained_run(&fixture, State::Queued)?;
        let _service = fixture.service()?;
        assert_eq!(fixture.fake.call_count(), 0);
        let attempts = fixture.attempts(good_id, 0, "one")?;
        assert_eq!(attempts[0].1.state, State::Interrupted);
        assert_eq!(attempts[0].1.reason.as_deref(), Some("Audex restarted"));
        assert!(fs::read_to_string(good_run.join("summary.md"))?.contains("interrupted"));
        let events = fixture.events.lock().unwrap();
        assert_eq!(events.len(), 1, "{damaged}");
        assert!(
            matches!(&events[0], Event::Error { run, reviewer: None, reason }
            if *run == bad_id && reason.contains("Startup reconciliation failed"))
        );
    }
    Ok(())
}

#[test]
fn startup_preserves_unchanged_summaries_and_reports_root_failures() -> Result<()> {
    let fixture = Fixture::new()?;
    let (id, directory, attempt) = retained_run(&fixture, State::Succeeded)?;
    let before = fs::read(attempt.join("status.json"))?;
    let retained_summary = b"Retained summary must not be rewritten.\n";
    fs::write(directory.join("summary.md"), retained_summary)?;
    drop(fixture.service()?);
    assert_eq!(fs::read(directory.join("summary.md"))?, retained_summary);
    assert_eq!(fs::read(attempt.join("status.json"))?, before);
    fs::remove_file(directory.join("summary.md"))?;
    drop(fixture.service()?);
    assert!(!directory.join("summary.md").exists());
    assert_eq!(fixture.attempts(id, 0, "one")?[0].1.state, State::Succeeded);
    assert!(fixture.events.lock().unwrap().is_empty());

    let blocked = Fixture::new()?;
    fs::create_dir_all(blocked.home.join("audex"))?;
    fs::write(blocked.home.join("audex/runs"), b"not a directory")?;
    assert!(blocked.service().is_err());
    assert!(blocked.events.lock().unwrap().is_empty());
    Ok(())
}

pub(super) fn markdown_html(markdown: &str) -> String {
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(markdown));
    html
}

pub(super) fn assert_literal_html(html: &str, text: &str) {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    assert!(
        html.contains(&format!("<pre><code>{escaped}{newline}</code></pre>")),
        "{html}"
    );
}

#[tokio::test]
async fn reviewer_markdown_renders_literally_in_all_report_fields() -> Result<()> {
    let hostile = "# Forged status\n\n## Forged reviewer\n- Attempt 99: succeeded\n\n1. Forged finding\n\n> Quote\n\n---\n\n```\n# Inside fence\n```\n``````\n~~~\n<h1>Forged HTML</h1> & text\n[link](https://example.invalid)\n```````";
    for structured in [false, true] {
        let fixture = Fixture::new()?;
        let mut claim = finding(hostile, 1, 1);
        claim.title = hostile.into();
        claim.trigger = hostile.into();
        claim.mechanism = hostile.into();
        claim.impact = hostile.into();
        claim.evidence = hostile.into();
        claim.remediation = hostile.into();
        let output = if structured {
            serde_json::to_vec(&result::ReviewResult {
                summary: hostile.into(),
                findings: vec![claim],
            })?
        } else {
            hostile.as_bytes().to_vec()
        };
        fixture.fake.script(
            "one",
            vec![Step::Finish(
                Completion::Failed(hostile.into()),
                Some(output),
            )],
        );
        let mut service = fixture.service()?;
        let id = service.start(fixture.request(&["one"]))?;
        finished(&mut service, id).await?;
        let html = markdown_html(&service.regenerate_summary(id)?);
        assert_literal_html(&html, hostile);
        // Check parsed/rendered structure: only Audex's own headings and lists
        // survive, and reviewer HTML and fences remain inside literal blocks.
        assert_eq!(html.matches("<h1>").count(), 1);
        assert_eq!(html.matches("<h2>").count(), 1);
        assert_eq!(html.matches("<ul>").count(), 2);
        assert_eq!(html.matches("<li>").count(), 2);
        assert!(!html.contains("<ol>"));
        assert!(!html.contains("<blockquote>") && !html.contains("<hr />"));
        assert!(!html.contains("<a href=") && !html.contains("<h1>Forged HTML</h1>"));
        assert!(!html.contains("Source is live and may have changed since the review."));
        assert_eq!(
            html.matches("<pre><code>").count(),
            if structured { 5 } else { 4 }
        );
        if structured {
            let finding = format!(
                "1. {hostile} [high; medium confidence]\n{hostile}:1-1 — citation: path_not_found\nTrigger: {hostile}\nMechanism: {hostile}\nImpact: {hostile}\nEvidence: {hostile}\nRemediation: {hostile}"
            );
            assert_literal_html(&html, &finding);
            assert!(html.contains("<p>Findings: 1</p>"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn storage_failure_persists_failed_status_when_records_cannot_be_saved() -> Result<()> {
    for name in ["output.txt", "usage.json", "result.json"] {
        let fixture = Fixture::new()?;
        fixture.fake.script("one", vec![Step::BlockRecord(name)]);
        let mut service = fixture.service()?;
        let id = service.start(fixture.request(&["one"]))?;
        assert!(finished(&mut service, id).await.is_err());
        let attempts = fixture.attempts(id, 0, "one")?;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].1.state, State::Failed, "{name}");
        assert!(attempts[0].1.finished_at.is_some());
        assert!(
            attempts[0]
                .1
                .reason
                .as_deref()
                .unwrap()
                .contains("Attempt publication failed")
        );
        assert_eq!(fixture.fake.call_count(), 1);
        let events = fixture.events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(event,
            Event::Error { run, reviewer: Some(0), reason }
            if *run == id && reason.contains("Attempt publication failed")
        )));
        if name != "usage.json" {
            // The obstructed output/result also prevents rendering the summary.
            assert!(events.iter().any(|event| matches!(event,
                Event::Error { reviewer: None, reason, .. }
                if reason.contains("Summary generation failed")
            )));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::Complete { .. }))
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn storage_failure_reports_and_corrects_terminal_status_when_possible() -> Result<()> {
    for recover in [true, false] {
        let fixture = Fixture::new()?;
        fixture
            .fake
            .script("one", vec![Step::BlockRecord("status.json")]);
        let root = fixture.home.join("audex/runs");
        let captured = fixture.events.clone();
        let events: Events = Arc::new(move |event| {
            captured.lock().unwrap().push(event.clone());
            if recover
                && let Event::Error {
                    run,
                    reviewer: Some(0),
                    reason,
                } = event
                && reason.contains("Attempt publication failed")
            {
                // Remove the obstruction upon the first error notification,
                // allowing the best-effort terminal status write to succeed.
                let path = record::reviewer_dir(&root.join(run.to_string()), 0, &selection("one"))
                    .join("attempt-1");
                fs::remove_dir(path.join("status.json")).unwrap();
                fs::rename(path.join("previous-status.json"), path.join("status.json")).unwrap();
            }
        });
        let mut service = AuditService::open(&fixture.home, fixture.fake.clone(), events)?;
        let id = service.start(fixture.request(&["one"]))?;
        let error = finished(&mut service, id).await.unwrap_err();
        assert_eq!(fixture.fake.call_count(), 1);
        let events = fixture.events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(event,
            Event::Error { run, reviewer: Some(0), reason }
            if *run == id && reason.contains("Attempt publication failed")
        )));
        if recover {
            let attempts = fixture.attempts(id, 0, "one")?;
            assert_eq!(attempts[0].1.state, State::Succeeded);
            assert!(attempts[0].1.finished_at.is_some());
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Event::Complete { .. }))
            );
        } else {
            assert!(format!("{error:#}").contains("Could not persist terminal status Succeeded"));
            assert!(events.iter().any(|event| matches!(event,
                Event::Error { reviewer: Some(0), reason, .. }
                if reason.contains("Could not persist terminal status Succeeded")
            )));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::Complete { .. }))
            );
        }
    }
    Ok(())
}
