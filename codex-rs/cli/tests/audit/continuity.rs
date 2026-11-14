//! M5 public CLI qualification with owned daemon state and a loopback provider.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_empty_execution_replaces_once_and_operator_can_continue() -> Result<()> {
    {
        let server = responses::start_mock_server().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let daemon = configured(&server)?;
        let source = daemon.home.path().join("example.rs");
        std::fs::write(&source, "fn main() {}\n")?;
        daemon.lifecycle("start")?;
        let selection = json!({"provider":"audit_fixture","model":"audit-test"});
        let submitted = audit(&daemon)
            .arg("--file")
            .arg(&source)
            .args(["--detach", "--reviewer", &selection.to_string()])
            .output()?;
        check(&submitted, 0)?;
        let run = String::from_utf8(submitted.stdout)?.trim().to_owned();
        let expected = 2;
        let reason = "automatic_recovery_exhausted_operator_investigation_required";
        let attempts = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let listed = audit(&daemon).args(["attempt", "list", &run]).output()?;
                check(&listed, 0)?;
                let value: Value = serde_json::from_slice(&listed.stdout)?;
                let rows = value["attempts"].as_array().context("attempt rows")?;
                if rows.len() == expected
                    && rows
                        .iter()
                        .any(|row| row["current"] == true && row["recoveryStatus"] == reason)
                {
                    break anyhow::Ok(rows.clone());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        assert_eq!(
            server.received_requests().await.context("requests")?.len(),
            expected
        );
        for attempt in &attempts {
            assert_eq!(attempt["runId"], run);
            assert_eq!(attempt["snapshotId"], attempts[0]["snapshotId"]);
            assert_eq!(attempt["provider"], "audit_fixture");
            assert_eq!(attempt["model"], "audit-test");
        }
        let current = attempts.iter().find(|row| row["current"] == true).unwrap();
        assert_eq!(current["retryEligible"], true);
        assert!(current.get("limits").is_none());
        let retried = audit(&daemon)
            .args([
                "attempt",
                "retry",
                &run,
                current["assignmentId"].as_str().unwrap(),
                "--authorise-retry",
                "--action-key",
                "operator-continues",
                "--reason",
                "continue unfinished review",
            ])
            .output()?;
        check(&retried, 0)?;
        check(&audit(&daemon).args(["cancel", &run]).output()?, 6)?;
        let pool = fixture_db(&daemon).await?;
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_runs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            audits, 1,
            "automatic replacement and explicit retry stay in the same audit"
        );
        pool.close().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_context_compacts_through_installed_cli_and_daemon() -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = responses::start_mock_server().await;
    let samples = Arc::new(AtomicUsize::new(0));
    let compactions = Arc::new(AtomicUsize::new(0));
    let (calls, compacts) = (samples.clone(), compactions.clone());
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let text = body.to_string();
            assert!(!text.contains("HOST_DEVELOPER_FORBIDDEN"));
            assert!(!text.contains("AMBIENT_AGENTS_FORBIDDEN"));
            let compact = body["tools"].as_array().is_none_or(Vec::is_empty);
            let mut events = vec![responses::ev_response_created("context-fixture")];
            if compact {
                compacts.fetch_add(1, Ordering::SeqCst);
                events.push(responses::ev_assistant_message(
                    "checkpoint",
                    "Continue reading the same captured example.rs.",
                ));
                events.push(responses::ev_completed_with_tokens("context-fixture", 100));
            } else {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                if index < 2 {
                    events.push(responses::ev_function_call_with_namespace(
                        &format!("read-{index}"),
                        "audex_source",
                        "read",
                        &json!({"path":"example.rs","offset":index*60000,"max_bytes":60000})
                            .to_string(),
                    ));
                    events.push(responses::ev_completed_with_tokens(
                        "context-fixture",
                        25000,
                    ));
                } else {
                    events.push(responses::ev_assistant_message(
                        "opinion",
                        "Completed the configured context-continuity review.",
                    ));
                    events.push(responses::ev_completed_with_tokens("context-fixture", 100));
                }
            }
            responses::sse_response(responses::sse(events))
                .insert_header("openai-model", "audit-test")
        })
        .mount(&server)
        .await;
    let daemon = configured(&server)?;
    let config = daemon.home.path().join("config.toml");
    let original = std::fs::read_to_string(&config)?;
    std::fs::write(
        &config,
        format!(
            "model_context_window = 100000\nmodel_auto_compact_token_limit = 12000\n{}",
            original.replace("[features]\n", "[features]\nasync_compaction = false\n")
        ),
    )?;
    let source = daemon.home.path().join("example.rs");
    std::fs::write(
        &source,
        "pub const CONTINUITY_SOURCE: u32 = 1;\n".repeat(5000),
    )?;
    daemon.lifecycle("start")?;
    let selection = json!({"provider":"audit_fixture","model":"audit-test"});
    let submitted = audit(&daemon)
        .arg("--file")
        .arg(&source)
        .args(["--detach", "--reviewer", &selection.to_string()])
        .output()?;
    check(&submitted, 0)?;
    let run = String::from_utf8(submitted.stdout)?.trim().to_owned();
    let attempt = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let listed = audit(&daemon).args(["attempt", "list", &run]).output()?;
            check(&listed, 0)?;
            let value: Value = serde_json::from_slice(&listed.stdout)?;
            let rows = value["attempts"].as_array().context("attempts")?;
            ensure!(
                rows.len() == 1,
                "context continuation must retain one attempt"
            );
            if rows[0]["completed"] == true {
                break anyhow::Ok(rows[0].clone());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let assignment = attempt["assignmentId"].as_str().context("assignment")?;
    let observed = audit(&daemon)
        .args([
            "attempt",
            "read",
            &run,
            assignment,
            "--after-sequence",
            "0",
            "--limit",
            "256",
        ])
        .output()?;
    check(&observed, 0)?;
    let activity: Value = serde_json::from_slice(&observed.stdout)?;
    let entries = activity["entries"].as_array().context("activity")?;
    let compact_sequence = entries
        .iter()
        .find(|entry| entry["payload"] == "client_compaction_completed")
        .and_then(|entry| entry["sequence"].as_i64())
        .context("no retained compaction activity")?;
    assert!(entries.iter().any(|entry| entry["kind"] == "tool_start"
        && entry["sequence"].as_i64().unwrap() > compact_sequence));
    assert!(compactions.load(Ordering::SeqCst) > 0);
    assert_eq!(samples.load(Ordering::SeqCst), 3);
    assert!(attempt.get("limits").is_none_or(Value::is_null));
    assert!(activity["recoveredOutput"].is_null());
    let pool = fixture_db(&daemon).await?;
    let policy: String =
        sqlx::query_scalar("SELECT policy_json FROM audit_attempt_inputs WHERE assignment_id=?")
            .bind(assignment)
            .fetch_one(&pool)
            .await?;
    let policy: Value = serde_json::from_str(&policy)?;
    assert_eq!(policy["model_metadata"]["context_window"], 100000);
    assert_eq!(policy["model_metadata"]["auto_compact_token_limit"], 12000);
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_runs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(runs, 1);
    pool.close().await;
    println!(
        "M5 public configuration: compactions={}, one audit/attempt, retained window=100000 and limit=12000",
        compactions.load(Ordering::SeqCst)
    );
    Ok(())
}
