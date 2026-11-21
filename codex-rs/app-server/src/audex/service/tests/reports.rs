use super::*;
use serde_json::json;

fn retained_fixture(route: &str) -> Result<(tempfile::TempDir, RunRecord, PathBuf)> {
    let temp = tempfile::tempdir()?;
    let mut reviewer = selection("original-reviewer");
    reviewer.route = route.into();
    reviewer.model = "historical-model".into();
    let run = RunRecord {
        id: RunId::new_v4(),
        targets: targets::Targets {
            paths: vec![temp.path().join("source-no-longer-present")],
            working_directories: vec![temp.path().join("historical-project")],
        },
        brief: "Original brief".into(),
        guidance: None,
        reviewers: vec![reviewer.clone()],
        created_at: "2021-02-03T04:05:06Z".into(),
    };
    record::write_json(&temp.path().join("run.json"), &run)?;
    fs::write(
        temp.path().join("summary.md"),
        "Aggregate summary remains unchanged.",
    )?;
    let attempt = record::reviewer_dir(temp.path(), 0, &reviewer).join("attempt-1");
    fs::create_dir_all(&attempt)?;
    let mut status = AttemptStatus::queued(1, Trigger::Initial, reviewer);
    status.finish(State::Succeeded, None);
    record::write_json(&attempt.join("status.json"), &status)?;
    let result = json!({
        "summary":"Do not ‘correct’ this: these is retained.\n\nA second paragraph.",
        "findings":[
            {"title":"Recieved NULL", "severity":"high", "confidence":"low", "path":"src/original.rs", "start_line":7,"end_line":9,"citation":"ok",
             "trigger":"Exactly when x == 0.","mechanism":"The value is `None` — perhaps.","impact":"It doesn't return.","evidence":"Line one.\n\nLine two: `call(x)`.","remediation":"Keep my recommendation, please."},
            {"title":"Second, literally.", "severity":"medium", "confidence":"high", "path":"other.rs", "start_line":3,"end_line":3,"citation":"path_not_found",
             "trigger":"Second trigger.","mechanism":"Second mechanism.","impact":"Second impact.","evidence":"Second evidence.","remediation":"Second recommendation."}
        ]
    });
    record::write_json(&attempt.join("result.json"), &result)?;
    fs::write(
        attempt.join("output.txt"),
        "Literal transport output is separate from the structured review.",
    )?;
    Ok((temp, run, attempt))
}

fn request(run: &RunRecord) -> codex_app_server_protocol::AuditReadRequest {
    serde_json::from_value(json!({"runId":run.id,"view":"report","reviewer":"1"})).unwrap()
}

#[test]
fn original_individual_report_preserves_exact_wording_fields_and_order_for_both_routes()
-> Result<()> {
    for route in ["native", "claude_subscription", "claude_api"] {
        let (temp, run, attempt) = retained_fixture(route)?;
        let files = [
            temp.path().join("run.json"),
            temp.path().join("summary.md"),
            attempt.join("status.json"),
            attempt.join("result.json"),
            attempt.join("output.txt"),
        ];
        let before = files
            .iter()
            .map(fs::read)
            .collect::<std::io::Result<Vec<_>>>()?;
        let response = read::read(temp.path(), &request(&run))?;
        let data = response.data.unwrap();
        assert_eq!(data["cwd"], json!(run.targets.working_directories[0]));
        assert_eq!(data["reviewer"], "original-reviewer");
        let expected = format!(
            "# Review — original-reviewer\n\n**Reviewer:** {route} / historical-model\n\n**Audit created:** 2021-02-03T04:05:06Z\n\n**Attempt:** 1 — succeeded\n\n{}",
            concat!(
                "## Summary\n\nDo not ‘correct’ this: these is retained.\n\nA second paragraph.\n\n",
                "## Finding 1: Recieved NULL\n\n",
                "**Severity:** high\n\n**Confidence:** low\n\n**Source:** src/original.rs:7-9\n\n**Citation check:** ok\n\n",
                "**Trigger:**\n\nExactly when x == 0.\n\n",
                "**Mechanism:**\n\nThe value is `None` — perhaps.\n\n",
                "**Impact:**\n\nIt doesn't return.\n\n",
                "**Evidence:**\n\nLine one.\n\nLine two: `call(x)`.\n\n",
                "**Remediation:**\n\nKeep my recommendation, please.\n\n",
                "## Finding 2: Second, literally.\n\n",
                "**Severity:** medium\n\n**Confidence:** high\n\n**Source:** other.rs:3-3\n\n**Citation check:** path_not_found\n\n",
                "**Trigger:**\n\nSecond trigger.\n\n",
                "**Mechanism:**\n\nSecond mechanism.\n\n",
                "**Impact:**\n\nSecond impact.\n\n",
                "**Evidence:**\n\nSecond evidence.\n\n",
                "**Remediation:**\n\nSecond recommendation.\n\n"
            )
        );
        assert_eq!(data["markdown"].as_str().unwrap(), expected);
        assert_eq!(
            files
                .iter()
                .map(fs::read)
                .collect::<std::io::Result<Vec<_>>>()?,
            before
        );
        assert!(
            !run.targets.paths[0].exists(),
            "historical retrieval needs no live source"
        );
    }
    Ok(())
}

#[test]
fn reports_use_latest_or_explicit_attempt_and_fall_back_to_original_unstructured_output()
-> Result<()> {
    for route in ["native", "claude_subscription"] {
        let (temp, run, original) = retained_fixture(route)?;
        let latest = original.with_file_name("attempt-2");
        fs::create_dir(&latest)?;
        let mut status = AttemptStatus::queued(2, Trigger::OperatorRetry, run.reviewers[0].clone());
        status.finish(State::Failed, Some("Interrupted transport".into()));
        record::write_json(&latest.join("status.json"), &status)?;
        let prose = "My unchanged, unstructured review.\n\n- Preserve this ordering.\n- And this typo: teh.\n\nRecommendation: don't rewrite me.\n";
        fs::write(latest.join("output.txt"), prose)?;
        let mut query = request(&run);
        query.reviewer = Some("original-reviewer".into());
        let latest_report = read::read(temp.path(), &query)?.data.unwrap();
        assert_eq!(latest_report["attempt"], 2);
        assert!(
            latest_report["markdown"]
                .as_str()
                .unwrap()
                .ends_with(&format!("## Review\n\n{prose}"))
        );
        query.attempt = Some(1);
        let earlier = read::read(temp.path(), &query)?.data.unwrap();
        assert_eq!(earlier["attempt"], 1);
        assert!(
            earlier["markdown"]
                .as_str()
                .unwrap()
                .contains("Recieved NULL")
        );
        assert!(!earlier["markdown"].as_str().unwrap().contains(prose));
        query.limit = Some(1);
        assert!(read::read(temp.path(), &query).is_err());
        query.limit = None;
        query.reviewer = None;
        assert!(read::read(temp.path(), &query).is_err());
        query.reviewer = Some("1".into());
        query.attempt = Some(99);
        assert!(read::read(temp.path(), &query).is_err());
        query.attempt = None;
        fs::remove_file(latest.join("output.txt"))?;
        assert!(
            read::read(temp.path(), &query)
                .unwrap_err()
                .to_string()
                .contains("No retained review")
        );
    }
    Ok(())
}
