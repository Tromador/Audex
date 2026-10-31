//! CLI controls over the real daemon's sole native event reader, with gated SSE.
use super::*;
use core_test_support::streaming_sse::{StreamingSseChunk, start_streaming_sse_server};
use tokio::sync::oneshot;

fn chunk(events: Vec<Value>, gate: Option<oneshot::Receiver<()>>) -> StreamingSseChunk {
    StreamingSseChunk {
        gate,
        body: responses::sse(events),
    }
}

async fn read_attempt(daemon: &TestDaemon, run: &str, assignment: &str) -> Result<Value> {
    let output = audit(daemon)
        .args(["attempt", "read", run, assignment])
        .output()?;
    check(&output, 0)?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn assistant_text(read: &Value) -> String {
    read["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|entry| entry["kind"] == "assistant_delta" || entry["kind"] == "assistant_message")
        .filter_map(|entry| entry["payload"].as_str())
        .collect()
}

async fn wait_text(
    daemon: &TestDaemon,
    run: &str,
    assignment: &str,
    expected: &str,
) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let read = read_attempt(daemon, run, assignment).await?;
            if assistant_text(&read) == expected {
                return Ok(read);
            }
            ensure!(
                read["attempt"]["captureFailure"].is_null(),
                "capture failed: {read}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_stream_partials_controls_retry_and_restart_use_the_production_event_path()
-> Result<()> {
    for action in ["cancel", "ignore", "retry"] {
        let (old_release, old_gate) = oneshot::channel();
        let (new_release, new_gate) = oneshot::channel();
        let fragment = "{\"unfinished\":\"ORIGINAL_RECEIVED_FRAGMENT";
        let replacement = "REPLACEMENT_ONLY_FRAGMENT";
        let original = vec![
            chunk(
                vec![
                    responses::ev_response_created("original"),
                    responses::ev_reasoning_item_added("private", &[]),
                    responses::ev_reasoning_summary_text_delta("PRIVATE_REASONING_CANARY"),
                    responses::ev_reasoning_item(
                        "private",
                        &["PRIVATE_REASONING_CANARY"],
                        &["PRIVATE_REASONING_CANARY"],
                    ),
                    responses::ev_message_item_added("partial", ""),
                    responses::ev_output_text_delta(fragment),
                ],
                None,
            ),
            chunk(
                vec![
                    responses::ev_output_text_delta("LATE_PROVIDER_PAYLOAD"),
                    responses::ev_assistant_message(
                        "partial",
                        &format!("{fragment}LATE_PROVIDER_PAYLOAD"),
                    ),
                    responses::ev_completed("original"),
                ],
                Some(old_gate),
            ),
        ];
        let successor = vec![
            chunk(
                vec![
                    responses::ev_response_created("replacement"),
                    responses::ev_message_item_added("fresh", ""),
                    responses::ev_output_text_delta(replacement),
                ],
                None,
            ),
            chunk(vec![responses::ev_completed("replacement")], Some(new_gate)),
        ];
        let (server, _) = start_streaming_sse_server(vec![original, successor]).await;
        let daemon = configured_url(server.uri())?;
        daemon.lifecycle("start")?;
        let source = daemon.home.path().join("example.rs");
        std::fs::write(&source, b"fn main() {}\n")?;
        let started = audit(&daemon)
            .arg("--file")
            .arg(&source)
            .arg("--detach")
            .output()?;
        check(&started, 0)?;
        let run = String::from_utf8(started.stdout)?.trim().to_owned();
        let listed = audit(&daemon).args(["attempt", "list", &run]).output()?;
        check(&listed, 0)?;
        let listed: Value = serde_json::from_slice(&listed.stdout)?;
        let assignment = listed["attempts"][0]["assignmentId"]
            .as_str()
            .context("assignment")?
            .to_owned();
        let before = wait_text(&daemon, &run, &assignment, fragment).await?;
        assert!(!before.to_string().contains("PRIVATE_REASONING_CANARY"));
        assert_eq!(before["attempt"]["completed"], false);
        assert!(before["rawResult"].is_null());
        let mut command = audit(&daemon);
        command.args([
            "attempt",
            action,
            &run,
            &assignment,
            "--action-key",
            "one-explicit-decision",
        ]);
        if action == "retry" {
            command.args(["--authorise-retry", "--acknowledge-possible-overlap"]);
        }
        let began = std::time::Instant::now();
        let decided = command.output()?;
        check(&decided, 0)?;
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "acknowledgement must precede provider EOF"
        );
        if action == "retry" {
            assert!(String::from_utf8_lossy(&decided.stderr).contains("overlapping charges"));
        }
        let decision: Value = serde_json::from_slice(&decided.stdout)?;
        let stopped = read_attempt(&daemon, &run, &assignment).await?;
        assert_eq!(assistant_text(&stopped), fragment);
        assert_eq!(stopped["attempt"]["outputCutoff"], decision["cutoff"]);
        assert_eq!(stopped["attempt"]["completed"], false);
        assert_eq!(
            command.output()?.stdout,
            decided.stdout,
            "lost acknowledgement replay must return the original identity"
        );
        let _ = old_release.send(());
        if action == "retry" {
            let fresh = decision["replacementAssignmentId"]
                .as_str()
                .context("fresh identity")?;
            assert_ne!(fresh, assignment);
            let current = wait_text(&daemon, &run, fresh, replacement).await?;
            assert_eq!(
                current["attempt"]["snapshotId"],
                before["attempt"]["snapshotId"]
            );
            assert_eq!(current["attempt"]["attemptOrdinal"], 2);
            let requests = server.requests().await;
            assert_eq!(requests.len(), 2);
            let request = String::from_utf8(requests[1].clone())?;
            assert!(request.contains(fresh));
            assert!(!request.contains(fragment) && !request.contains("ORIGINAL_RECEIVED_FRAGMENT"));
            assert!(!request.contains(&assignment));
            let pool = fixture_db(&daemon).await?;
            let sessions: Vec<String> =
                sqlx::query_scalar("SELECT thread_id FROM audit_execution ORDER BY operation_id")
                    .fetch_all(&pool)
                    .await?;
            assert_eq!(sessions.len(), 2);
            assert_ne!(sessions[0], sessions[1]);
            pool.close().await;
            let cancel = audit(&daemon)
                .args([
                    "attempt",
                    "cancel",
                    &run,
                    fresh,
                    "--action-key",
                    "stop-replacement",
                ])
                .output()?;
            check(&cancel, 0)?;
        }
        let _ = new_release.send(());
        daemon.lifecycle("stop")?;
        daemon.lifecycle("start")?;
        let recovered = read_attempt(&daemon, &run, &assignment).await?;
        assert_eq!(assistant_text(&recovered), fragment);
        assert_eq!(recovered["attempt"]["outputCutoff"], decision["cutoff"]);
        assert!(!recovered.to_string().contains("LATE_PROVIDER_PAYLOAD"));
        assert!(!recovered.to_string().contains("PRIVATE_REASONING_CANARY"));
        assert_eq!(recovered["attempt"]["completed"], false);
        assert_eq!(
            server.requests().await.len(),
            if action == "retry" { 2 } else { 1 }
        );
        daemon.lifecycle("stop")?;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_source_tool_activity_is_retained_separately_from_review_output() -> Result<()> {
    let (release, gate) = oneshot::channel();
    let tool = vec![chunk(
        vec![
            responses::ev_response_created("source-read"),
            responses::ev_function_call_with_namespace(
                "call-read",
                "audex_source",
                "read",
                r#"{"path":"example.rs","offset":0,"max_bytes":128}"#,
            ),
            responses::ev_completed("source-read"),
        ],
        None,
    )];
    let continuation = vec![
        chunk(vec![responses::ev_response_created("after-tool")], None),
        chunk(vec![responses::ev_completed("after-tool")], Some(gate)),
    ];
    let (server, _) = start_streaming_sse_server(vec![tool, continuation]).await;
    let daemon = configured_url(server.uri())?;
    daemon.lifecycle("start")?;
    let source = daemon.home.path().join("example.rs");
    std::fs::write(&source, b"fn retained_source() {}\n")?;
    let started = audit(&daemon)
        .arg("--file")
        .arg(&source)
        .arg("--detach")
        .output()?;
    check(&started, 0)?;
    let run = String::from_utf8(started.stdout)?.trim().to_owned();
    let listed = audit(&daemon).args(["attempt", "list", &run]).output()?;
    check(&listed, 0)?;
    let listed: Value = serde_json::from_slice(&listed.stdout)?;
    let assignment = listed["attempts"][0]["assignmentId"]
        .as_str()
        .context("assignment")?;
    let mut last_read = Value::Null;
    let read = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let read = read_attempt(&daemon, &run, assignment).await?;
            last_read = read.clone();
            if read["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .any(|entry| entry["kind"] == "tool_result")
            {
                return Ok::<_, anyhow::Error>(read);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("tool activity did not arrive: {last_read}"))??;
    let entries = read["entries"].as_array().expect("entries");
    let start = entries
        .iter()
        .find(|entry| entry["kind"] == "tool_start")
        .context("tool start")?;
    let result = entries
        .iter()
        .find(|entry| entry["kind"] == "tool_result")
        .context("tool result")?;
    assert_eq!(start["itemId"], "call-read");
    assert_eq!(result["itemId"], "call-read");
    assert!(
        result["payload"]
            .as_str()
            .context("payload")?
            .contains("retained_source")
    );
    assert!(assistant_text(&read).is_empty());
    assert_eq!(read["attempt"]["completed"], false);
    let stopped = audit(&daemon)
        .args([
            "attempt",
            "ignore",
            &run,
            assignment,
            "--action-key",
            "tool-only-stop",
        ])
        .output()?;
    check(&stopped, 0)?;
    let _ = release.send(());
    let retained = read_attempt(&daemon, &run, assignment).await?;
    assert_eq!(retained["attempt"]["completed"], false);
    assert!(retained["rawResult"].is_null());
    daemon.lifecycle("stop")?;
    server.shutdown().await;
    Ok(())
}
