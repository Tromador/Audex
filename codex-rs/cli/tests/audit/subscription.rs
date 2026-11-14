//! Actual CLI -> managed daemon -> public preflight/start -> owned child and source MCP.
use super::*;
#[path = "subscription_fixture.rs"]
mod fixture;

fn start(daemon: &TestDaemon, key: &str, mixed: bool) -> Result<Output> {
    let source = daemon.home.path().join("example.rs");
    std::fs::write(&source, "fn main() {}\n")?;
    let mut command = audit(daemon);
    command
        .arg("--file")
        .arg(source)
        .args(["--request-key", key, "--detach"]);
    if mixed {
        command.args(["--reviewer", "audit_fixture:audit-test"]);
    }
    let mut selection = fixture::selection();
    if key == "public-alias" {
        selection["expectedServedModel"] = json!("claude-qualified-alias");
    }
    command.args(["--reviewer", &selection.to_string()]);
    let output = command.output()?;
    check(&output, 0)?;
    let disclosure = String::from_utf8_lossy(&output.stderr);
    for text in [
        "including queued reviewers",
        "subscription quota",
        "Ordinary client retries",
        "without an Audex-imposed attempt or spending ceiling",
        "Official client executable:",
        "Dedicated subscription profile:",
        "Private runtime root:",
        "expected served: claude-",
        "Brief SHA-256:",
        "Context SHA-256:",
    ] {
        assert!(disclosure.contains(text), "missing {text}: {disclosure}");
    }
    Ok(output)
}

fn attempts(daemon: &TestDaemon, run: &str) -> Result<Vec<Value>> {
    let output = audit(daemon).args(["attempt", "list", run]).output()?;
    check(&output, 0)?;
    Ok(serde_json::from_slice::<Value>(&output.stdout)?["attempts"]
        .as_array()
        .unwrap()
        .clone())
}

async fn wait(
    daemon: &TestDaemon,
    run: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Result<Vec<Value>> {
    // The repair fixture continues for 30 seconds before its own exit. M5
    // leaves that continuation to the client; allow its terminal observation.
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let rows = attempts(daemon, run)?;
            if predicate(&rows) {
                return Ok(rows);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

fn observe(daemon: &TestDaemon, run: &str, assignment: &str) -> Result<Value> {
    let output = audit(daemon)
        .args(["attempt", "read", run, assignment, "--limit", "256"])
        .output()?;
    check(&output, 0)?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn control(daemon: &TestDaemon, run: &str, assignment: &str, action: &str) -> Result<Output> {
    let mut command = audit(daemon);
    command.args([
        "attempt",
        action,
        run,
        assignment,
        "--action-key",
        &format!("public-{action}"),
    ]);
    if action == "retry" {
        command.args(["--authorise-retry", "--acknowledge-possible-overlap"]);
    }
    Ok(command.output()?)
}

fn evidence(label: &str, value: Value, report: &Output) -> Result<()> {
    if let Some(root) = std::env::var_os("AUDEX_PROCESS_EVIDENCE_DIR") {
        let root = std::path::PathBuf::from(root).join(format!("subscription-{label}"));
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("evidence.json"),
            serde_json::to_vec_pretty(&value)?,
        )?;
        std::fs::write(root.join("report.md"), &report.stdout)?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_subscription_and_mixed_opinions_keep_extraction_and_reports_distinct() -> Result<()>
{
    for (mixed, mode) in [
        (false, "valid"),
        (true, "valid"),
        (false, "alias"),
        (false, "fenced"),
        (true, "plain"),
        (true, "wrong_claim"),
    ] {
        let alias = mode == "alias";
        let key = if alias {
            "public-alias"
        } else {
            "public-complete"
        };
        let server = responses::start_mock_server().await;
        provider(&server, if mode == "plain" { "plain" } else { "normal" }).await;
        let daemon = configured(&server)?;
        let child = fixture::configure(daemon.home.path(), mode)?;
        daemon.lifecycle("start")?;
        let started = start(&daemon, key, mixed)?;
        let run = String::from_utf8(started.stdout)?.trim().to_owned();
        let rows = wait(&daemon, &run, |rows| {
            rows.iter().all(|r| r["completed"] == true)
        })
        .await?;
        assert_eq!(rows.len(), if mixed { 2 } else { 1 });
        let subscription = rows.last().unwrap();
        assert_eq!(subscription["authenticationRoute"], "claude_subscription");
        assert_eq!(subscription["provider"], "anthropic");
        if alias {
            assert_eq!(
                subscription["expectedServedModel"],
                "claude-qualified-alias"
            );
        }
        let history = observe(
            &daemon,
            &run,
            subscription["assignmentId"].as_str().unwrap(),
        )?;
        assert_eq!(
            history["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["kind"] == "tool_start")
                .count(),
            3
        );
        let report = audit(&daemon)
            .args(["read", &run, "--follow", "--diagnostics"])
            .output()?;
        check(&report, 0)?;
        let text = String::from_utf8_lossy(&report.stdout);
        assert!(text.contains("Report format: `5`"));
        let status = if matches!(mode, "plain" | "wrong_claim") {
            "unstructured_opinion"
        } else if mode == "fenced" {
            "deterministically_normalised"
        } else {
            "validated_structured"
        };
        assert_eq!(subscription["extraction"]["status"], status);
        if mode == "plain" {
            assert_eq!(rows[0]["extraction"]["status"], "unstructured_opinion");
        }
        assert!(text.contains(status));
        assert!(history["rawResult"].is_string());
        if mode == "fenced" {
            assert!(
                history["rawResult"]
                    .as_str()
                    .unwrap()
                    .starts_with("```json\n")
            );
        }
        if mode == "wrong_claim" {
            assert!(text.contains("result assignment ID does not match"));
        }
        assert!(text.contains("Independent public subscription review"));
        assert!(text.contains(r"Route: claude\_subscription"));
        assert!(text.contains("claude-haiku-5-5"));
        let replay = start(&daemon, key, mixed)?;
        assert_eq!(String::from_utf8(replay.stdout)?.trim(), run);
        assert_eq!(fixture::prompts(&child), 1);
        daemon.lifecycle("stop")?;
        // Replaying an approval must work after local backend configuration disappears.
        std::fs::remove_file(daemon.home.path().join("audex/reviewer-backends.json"))?;
        daemon.lifecycle("start")?;
        let replay = start(&daemon, key, mixed)?;
        assert_eq!(String::from_utf8(replay.stdout)?.trim(), run);
        assert!(String::from_utf8_lossy(&replay.stderr).contains("Existing audit:"));
        for _ in 0..2 {
            let reopened = audit(&daemon)
                .args(["read", &run, "--diagnostics"])
                .output()?;
            check(&reopened, 0)?;
            assert_eq!(reopened.stdout, report.stdout);
            assert_eq!(
                observe(
                    &daemon,
                    &run,
                    subscription["assignmentId"].as_str().unwrap()
                )?["entries"],
                history["entries"]
            );
        }
        assert_eq!(fixture::prompts(&child), 1);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            if mixed { 2 } else { 0 }
        );
        evidence(
            &format!("{mode}-mixed-{mixed}"),
            json!({"attempts":rows,"history":history,"prompts":1,"disclosure":String::from_utf8_lossy(&started.stderr)}),
            &report,
        )?;
        daemon.lifecycle("stop")?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_subscription_failures_preserve_native_peer_and_durable_repair_disclosure()
-> Result<()> {
    for mode in [
        "missing_model",
        "wrong_model",
        "wrong_billing",
        "repair",
        "interrupted",
    ] {
        let server = responses::start_mock_server().await;
        provider(&server, "normal").await;
        let daemon = configured(&server)?;
        let child = fixture::configure(daemon.home.path(), mode)?;
        daemon.lifecycle("start")?;
        let started = start(&daemon, mode, true)?;
        let run = String::from_utf8(started.stdout)?.trim().to_owned();
        let empty_failure = matches!(mode, "missing_model" | "wrong_model" | "wrong_billing");
        let rows = wait(&daemon, &run, |rows| {
            rows[0]["completed"] == true
                && rows.iter().any(|row| {
                    row["provider"] == "anthropic"
                        && row["current"] == true
                        && row["recoveryStatus"]
                            == if empty_failure {
                                "automatic_recovery_exhausted_operator_investigation_required"
                            } else {
                                "operator_retry_required_surviving_output"
                            }
                })
        })
        .await?;
        let current = rows
            .iter()
            .find(|row| row["provider"] == "anthropic" && row["current"] == true)
            .unwrap();
        assert_eq!(rows.len(), if empty_failure { 3 } else { 2 });
        assert_eq!(current["completed"], false);
        assert_eq!(current["disposition"], "active");
        let history = observe(&daemon, &run, current["assignmentId"].as_str().unwrap())?;
        if mode == "interrupted" {
            assert!(
                history["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["kind"] == "assistant_delta"
                        && entry["payload"] == "Independent subscription review progress.")
            );
        }
        if mode == "repair" {
            assert!(
                history
                    .to_string()
                    .contains("client-initiated repair occurred")
            );
        }
        let pending = audit(&daemon).args(["read", &run]).output()?;
        check(&pending, 5)?;
        let aborted = audit(&daemon).args(["abort", &run]).output()?;
        check(&aborted, 6)?;
        let report = audit(&daemon)
            .args(["read", &run, "--diagnostics"])
            .output()?;
        check(&report, 6)?;
        assert!(String::from_utf8_lossy(&report.stdout).contains("CLI fixture audit"));
        if mode == "repair" {
            assert!(
                String::from_utf8_lossy(&report.stdout)
                    .contains("client-initiated repair occurred")
            );
        }
        let prompts = if empty_failure { 2 } else { 1 };
        assert_eq!(fixture::prompts(&child), prompts);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        let frozen = audit(&daemon)
            .args(["read", &run, "--diagnostics"])
            .output()?;
        check(&frozen, 6)?;
        assert_eq!(frozen.stdout, report.stdout);
        evidence(
            mode,
            json!({"attempts":rows,"history":history,"nativeRequests":2,"prompts":prompts}),
            &report,
        )?;
        daemon.lifecycle("stop")?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_subscription_exact_controls_and_restart_never_replay_the_original() -> Result<()> {
    for action in ["cancel", "ignore", "retry", "abort", "restart"] {
        let server = responses::start_mock_server().await;
        provider(&server, "normal").await;
        let daemon = configured(&server)?;
        let child = fixture::configure(daemon.home.path(), "held")?;
        daemon.lifecycle("start")?;
        let started = start(&daemon, action, true)?;
        let run = String::from_utf8(started.stdout)?.trim().to_owned();
        let rows = wait(&daemon, &run, |rows| {
            rows[0]["completed"] == true && rows[1]["outputSequence"].as_i64().unwrap_or(0) >= 7
        })
        .await?;
        let assignment = rows[1]["assignmentId"].as_str().unwrap();
        let history = observe(&daemon, &run, assignment)?;
        assert_eq!(fixture::prompts(&child), 1);
        let report = if action == "restart" {
            daemon.lifecycle("stop")?;
            daemon.lifecycle("start")?;
            let reopened = observe(&daemon, &run, assignment)?;
            assert!(!reopened["entries"].as_array().unwrap().is_empty());
            assert_eq!(fixture::prompts(&child), 1);
            audit(&daemon).args(["abort", &run]).output()?
        } else if action == "abort" {
            audit(&daemon).args(["abort", &run]).output()?
        } else {
            if action == "retry" {
                fixture::set_mode(&child, "valid")?;
            }
            let decision = control(&daemon, &run, assignment, action)?;
            check(&decision, 0)?;
            if action == "retry" {
                wait(&daemon, &run, |rows| {
                    rows.iter()
                        .filter(|r| r["current"] == true)
                        .all(|r| r["completed"] == true)
                })
                .await?;
                assert_eq!(fixture::prompts(&child), 2);
                let old = observe(&daemon, &run, assignment)?;
                assert_eq!(old["attempt"]["current"], false);
                check(&control(&daemon, &run, assignment, "cancel")?, 1)?;
            }
            audit(&daemon).args(["read", &run, "--follow"]).output()?
        };
        check(
            &report,
            if matches!(action, "abort" | "restart") {
                6
            } else {
                0
            },
        )?;
        if action != "retry" {
            assert_eq!(fixture::prompts(&child), 1);
        }
        if action == "ignore" {
            check(&control(&daemon, &run, assignment, "retry")?, 1)?;
        }
        let frozen = audit(&daemon).args(["read", &run]).output()?;
        assert_eq!(frozen.stdout, report.stdout);
        evidence(
            action,
            json!({"before":rows,"history":history,"after":attempts(&daemon,&run)?,"prompts":fixture::prompts(&child)}),
            &report,
        )?;
        daemon.lifecycle("stop")?;
    }
    Ok(())
}
