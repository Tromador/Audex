//! C6.2: real owned pipes -> capture journal -> result validator -> frozen report.
use super::*;

pub(super) async fn seed_stream(
    controller: &AuditController,
    temp: &tempfile::TempDir,
    mode: &str,
    gate: bool,
) -> anyhow::Result<String> {
    let id = seed_external(controller, temp, mode).await?;
    std::fs::write(
        temp.path().join("fake-client"),
        include_str!("../../../../external/fake_stream.py"),
    )?;
    std::fs::write(
        temp.path().join("stream-scenario.json"),
        json!({"mode":mode,"gate":gate}).to_string(),
    )?;
    Ok(id)
}

pub(super) async fn settled(
    controller: &AuditController,
    id: &str,
) -> anyhow::Result<AuditRunRecord> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            controller.drive(id).await?;
            let run = controller.store().await?.load_run(id).await?;
            if matches!(
                run.assignment_state,
                AssignmentState::Succeeded | AssignmentState::Failed | AssignmentState::Unknown
            ) {
                return Ok(run);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

async fn path_ready(path: &Path) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    Ok(())
}

pub(super) fn assert_no_private_files(path: &Path) -> anyhow::Result<()> {
    // Reading and closing a live SQLite file in this process drops its POSIX
    // locks, including locks held by SQLx worker threads. Inspect from a child
    // process so the privacy check cannot invalidate the running store's locks.
    let output = std::process::Command::new("/usr/bin/python3")
        .args([
            "-c",
            r#"
import os, pathlib, sys
for directory, _, files in os.walk(sys.argv[1]):
    for name in files:
        path = pathlib.Path(directory) / name
        if path.is_symlink() or not path.is_file():
            continue
        try:
            data = path.read_bytes()
        except FileNotFoundError:
            continue  # SQLite may remove a checkpointed WAL between enumeration and open.
        if b'PRIVATE_SENTINEL_MUST_NOT_PERSIST' in data:
            raise AssertionError('private bytes in ' + str(path))
"#,
        ])
        .arg(path)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "privacy scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn public_capture_deduplicates_by_identity_and_freezes_external_evidence() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-public-capture", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = seed_stream(&controller, &temp, "valid", false).await?;
        let run = settled(&controller, &id).await?;
        assert_eq!(run.assignment_state, AssignmentState::Succeeded);
        assert_eq!(finished(&controller, &id).await["phase"], "completed");
        let store = controller.store().await?;
        let activity = store.activity(&run.assignment_id, 0, 100).await?;
        let text = activity
            .iter()
            .filter(|e| e.kind.starts_with("assistant_"))
            .map(|e| e.payload.as_str())
            .collect::<String>();
        assert_eq!(
            text.matches("Public progress.").count(),
            2,
            "distinct message identities remain distinct"
        );
        assert_eq!(
            text.matches("Valid external review").count(),
            1,
            "partial/full final deduplication"
        );
        assert_eq!(
            activity.iter().filter(|e| e.kind == "tool_start").count(),
            2
        );
        assert_eq!(
            activity.iter().filter(|e| e.kind == "tool_result").count(),
            2
        );
        for call in ["call-1", "call-2"] {
            let start = activity
                .iter()
                .find(|e| e.kind == "tool_start" && e.item_id == call)
                .unwrap();
            let result = activity
                .iter()
                .find(|e| e.kind == "tool_result" && e.item_id == call)
                .unwrap();
            assert!(start.sequence < result.sequence);
            assert_eq!(result.payload, "{\"entries\":[]}");
        }
        assert!(
            activity
                .windows(2)
                .all(|w| w[1].sequence == w[0].sequence + 1)
        );
        assert!(
            store
                .generation_records(&run.assignment_id)
                .await?
                .is_empty(),
            "subscription must not fabricate permits"
        );
        let observations = store.model_evidence(&run.assignment_id).await?;
        assert_eq!(observations.len(), 3);
        assert!(
            observations.iter().all(|o| o.state == "verified"
                && o.observed_model.as_deref() == Some("claude-haiku-5-5"))
        );
        let frozen = serde_json::to_value(store.freeze_report(&id).await?)?;
        assert_eq!(frozen["format_version"], 5);
        let facts = &frozen["reviewers"][0]["provider_facts"];
        assert_eq!(facts["route"]["value"], "claude_subscription");
        assert_eq!(facts["versions"]["client_version"]["value"], "2.1.314");
        assert_eq!(
            facts["versions"]["client_executable_digest"]["value"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(facts["client_terminal"]["value"]["verified"], true);
        assert_eq!(facts["usage"]["value"].as_array().unwrap().len(), 4);
        assert!(facts["receipts"].as_array().unwrap().is_empty());
        let aggregate = facts["usage"]["value"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["evidence_source"] == "claude_client_terminal_aggregate_not_generation")
            .unwrap();
        assert_eq!(aggregate["cost"]["value"]["amount"], "0.0123");
        assert_eq!(aggregate["cost"]["value"]["measured"], false);
        let reference: (String,String,String) = sqlx::query_as("SELECT c.artefact_path,c.artefact_digest,a.client_terminal_digest FROM audit_client_terminal_evidence c JOIN reviewer_assignments a ON a.id=c.assignment_id WHERE c.assignment_id=?")
            .bind(&run.assignment_id).fetch_one(&store.pool_for_tests()).await?;
        assert_eq!(reference.1, reference.2);
        let bytes = std::fs::read(controller.root.join(&reference.0))?;
        assert_eq!(sha256_hex(&bytes), reference.1);
        let artifact: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(
            artifact["identity"]["execution_id"],
            store.execution(&run.operation_id).await?.unwrap().0
        );
        assert_eq!(artifact["terminal"]["result"], artifact["final_text"]);
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert_eq!(
            reopened.activity(&run.assignment_id, 0, 100).await?,
            activity
        );
        // Simulate a later operational receipt; the frozen input must remain byte-identical.
        sqlx::query("UPDATE audit_client_terminal_evidence SET usage_json=? WHERE assignment_id=?")
            .bind(r#"{"input_tokens":999,"cache_read_input_tokens":null,"output_tokens":999}"#)
            .bind(&run.assignment_id)
            .execute(&store.pool_for_tests())
            .await?;
        assert_eq!(
            serde_json::to_value(reopened.freeze_report(&id).await?)?,
            frozen
        );
        assert_no_private_files(controller.root.as_path())?;
        assert_eq!(
            receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
            1
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C62 public capture: ordered, single public content, stable tool identities, no private bytes; subscription completion and frozen format 4 verified"
        );
        Ok(())
    })
}

#[test]
fn stream_failures_and_unstructured_output_remain_distinct_without_replay() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-invalid-streams", || async {
        for mode in [
            "malformed",
            "oversized",
            "truncated",
            "eof",
            "interrupted",
            "stderr_bound",
            "record_bound",
            "stream_bound",
            "terminal_no_newline",
            "wrong_execution",
            "wrong_session",
            "missing_model",
            "wrong_model",
            "model_conflict",
            "wrong_terminal_session",
            "wrong_terminal_text",
            "no_final_message",
            "error_terminal",
            "invalid_result",
            "invalid_source",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, mode, false).await?;
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            if matches!(mode, "invalid_result" | "invalid_source") {
                assert_eq!(run.assignment_state, AssignmentState::Succeeded);
                assert!(run.findings.is_empty());
                assert_eq!(
                    store
                        .result_extraction(&run.assignment_id)
                        .await?
                        .unwrap()
                        .status,
                    codex_app_server_protocol::AuditExtractionStatus::UnstructuredOpinion
                );
            } else {
                assert_ne!(run.assignment_state, AssignmentState::Succeeded, "{mode}");
                assert!(store.result_extraction(&run.assignment_id).await?.is_none());
            }
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            for _ in 0..3 {
                controller.drive(&id).await?;
            }
            assert_eq!(
                store
                    .client_launch(&run.operation_id)
                    .await?
                    .unwrap()
                    .execution_id,
                launch.execution_id
            );
            assert_eq!(
                receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
                1
            );
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            assert_no_private_files(controller.root.as_path())?;
            if mode == "error_terminal" {
                api(&controller,"audit/attempt/control",json!({"runId":id,"assignmentId":run.assignment_id,"actionKey":"exclude-failed","action":"ignore","reason":"fixture exclusion"})).await.expect("exclude failed fixture");
                let frozen = serde_json::to_value(store.freeze_report(&id).await?)?;
                assert!(
                    frozen["reviewers"][0]["provider_facts"]["usage"]["value"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v["evidence_source"]
                            == "claude_client_failed_terminal_aggregate_not_generation")
                );
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C62 {mode}: {:?}, extraction remains separate; no prompt replay",
                run.assignment_state
            );
        }
        Ok(())
    })
}

#[test]
fn failed_stream_retains_received_prefix_and_requires_operator_retry() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-failed-stream-prefix", || async {
        for mode in ["malformed", "truncated", "eof"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, mode, false).await?;
            let run = settled(&controller, &id).await?;
            lifecycle_qualification::reconcile_until(&controller, &run.operation_id).await?;
            let store = controller.store().await?;
            let read = api(
                &controller,
                "audit/attempt/read",
                json!({
                    "runId":id,"assignmentId":run.assignment_id
                }),
            )
            .await
            .unwrap();
            assert_eq!(read["attempt"]["completed"], false, "{mode}");
            assert_eq!(read["attempt"]["retryEligible"], true, "{mode}");
            assert_eq!(
                read["attempt"]["recoveryStatus"], "operator_retry_required_surviving_output",
                "{mode}"
            );
            assert!(
                read["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["kind"] == "assistant_delta"
                        && entry["payload"] == "Public progress."),
                "{mode}: received prefix must survive the stream error"
            );
            assert!(
                store
                    .capture_state(&run.assignment_id)
                    .await?
                    .failure
                    .is_some()
            );
            assert!(store.result_extraction(&run.assignment_id).await?.is_none());
            for _ in 0..3 {
                controller.drive(&id).await?;
            }
            assert_eq!(store.attempts(&id).await?.len(), 1, "{mode}");
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            assert_no_private_files(controller.root.as_path())?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            store.close().await;
        }
        Ok(())
    })
}

#[test]
fn capture_storage_failure_stops_child_and_preserves_native_peer() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-capture-failure", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = seed_stream(&controller, &temp, "valid", true).await?;
        let store = controller.store().await?;
        sqlx::query("CREATE TRIGGER c62_capture_disk_failure BEFORE INSERT ON audit_activity BEGIN SELECT RAISE(ABORT,'owned fixture failure'); END").execute(&store.pool_for_tests()).await?;
        let run = settled(&controller, &id).await?;
        assert_ne!(run.assignment_state, AssignmentState::Succeeded);
        assert!(
            store
                .capture_state(&run.assignment_id)
                .await?
                .failure
                .is_some()
        );
        let pid = receipt(temp.path(), "stream-prompt-receipt.json").await?["pid"]
            .as_u64()
            .unwrap() as u32;
        gone(pid).await?;
        assert!(
            store
                .client_terminal_result(&run.operation_id)
                .await?
                .is_none()
        );
        sqlx::query("DROP TRIGGER c62_capture_disk_failure")
            .execute(&store.pool_for_tests())
            .await?;
        let native = start(&controller, "native-after-capture-failure").await;
        let native_run = store.load_run(&native).await?;
        let raw = json!({"schema_version":1,"snapshot_id":native_run.snapshot_id,"assignment_id":native_run.assignment_id,"summary":"Healthy native peer","findings":[]}).to_string();
        let mock = responses::mount_sse_sequence(
            &server,
            vec![sse(vec![
                ev_response_created("native"),
                ev_assistant_message("native-message", &raw),
                ev_completed("native"),
            ])],
        )
        .await;
        assert_eq!(finished(&controller, &native).await["phase"], "completed");
        assert_eq!(mock.requests().len(), 1);
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C62 capture storage failure: child stopped, no terminal acceptance; native completed once"
        );
        Ok(())
    })
}

#[test]
fn immutable_terminal_recovers_after_persistence_without_child_or_replay() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-terminal-recovery", || async {
        for corrupt in [false, true] {
            inspect_terminal_fixture(corrupt, None).await?;
        }
        Ok(())
    })
}

#[test]
fn transient_terminal_read_failure_preserves_execution_and_original_evidence() -> anyhow::Result<()>
{
    run_test_with_large_stack("c62-terminal-read-retry", || async {
        for failure in ["filesystem", "database"] {
            inspect_terminal_fixture(false, Some(failure)).await?;
        }
        Ok(())
    })
}

async fn inspect_terminal_fixture(corrupt: bool, read_failure: Option<&str>) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let server = responses::start_mock_server().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let blocked = Arc::new(tokio::sync::Semaphore::new(0));
    let notify = entered.clone();
    let barrier = blocked.clone();
    let hooks = crate::audex::dispatch::DispatchTestHooks {
        checkpoint: Some(Arc::new(move |stage| {
            let notify = notify.clone();
            let barrier = barrier.clone();
            Box::pin(async move {
                if stage == "terminal_persisted" {
                    notify.notify_one();
                    let _permit = barrier.acquire().await;
                }
            })
        })),
        ..Default::default()
    };
    let (controller, threads) = fixture_with_hooks(
        &temp,
        &server.uri(),
        CodexAuth::from_api_key("local-dummy"),
        false,
        Arc::new(|_, _| Box::pin(async { Ok(()) })),
        hooks,
    )
    .await;
    let id = seed_stream(&controller, &temp, "valid", false).await?;
    controller.drive(&id).await?;
    tokio::time::timeout(Duration::from_secs(15), entered.notified()).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    assert_ne!(run.assignment_state, AssignmentState::Succeeded);
    let raw = store
        .client_terminal_result(&run.operation_id)
        .await?
        .expect("persisted original");
    controller
        .dispatch
        .request_interrupt(run.operation_id.clone())
        .await;
    let original_execution = store.execution(&run.operation_id).await?;
    if let Some(failure) = read_failure {
        let path: String = sqlx::query_scalar(
            "SELECT artefact_path FROM audit_client_terminal_evidence WHERE assignment_id=?",
        )
        .bind(&run.assignment_id)
        .fetch_one(&store.pool_for_tests())
        .await?;
        let path = controller.root.join(path);
        if failure == "filesystem" {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))?;
            assert!(
                matches!(store.client_terminal_result(&run.operation_id).await,
                        Err(crate::audex::store::AuditStoreError::Io(error))
                        if error.kind() == std::io::ErrorKind::PermissionDenied)
            );
        } else {
            // Make only the terminal-evidence query unavailable in this owned database.
            sqlx::query(
                "ALTER TABLE audit_client_terminal_evidence RENAME TO c62_r2_unavailable_terminal",
            )
            .execute(&store.pool_for_tests())
            .await?;
            assert!(matches!(
                store.client_terminal_result(&run.operation_id).await,
                Err(crate::audex::store::AuditStoreError::Database(_))
            ));
        }
        let inspected = controller.drive(&id).await;
        let pending = store.load_run(&id).await?;
        let issue: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT error,retry_classification FROM operations WHERE id=?")
                .bind(&run.operation_id)
                .fetch_one(&store.pool_for_tests())
                .await?;
        // Restore availability before assertions, including on the pre-fix failing path.
        if failure == "filesystem" {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))?;
        } else {
            sqlx::query(
                "ALTER TABLE c62_r2_unavailable_terminal RENAME TO audit_client_terminal_evidence",
            )
            .execute(&store.pool_for_tests())
            .await?;
        }
        inspected?;
        assert_eq!(
            pending.assignment_state, run.assignment_state,
            "{failure}: read unavailability must not settle the attempt"
        );
        assert_eq!(pending.operation_state, run.operation_state);
        assert_eq!(
            issue.0.as_deref(),
            Some("external_terminal_evidence_unavailable")
        );
        assert_eq!(issue.1.as_deref(), Some("observe_only"));
        assert_eq!(
            store.execution(&run.operation_id).await?,
            original_execution
        );
        assert_eq!(
            store
                .client_terminal_result(&run.operation_id)
                .await?
                .unwrap(),
            raw
        );
    }
    if corrupt {
        let path: String = sqlx::query_scalar(
            "SELECT artefact_path FROM audit_client_terminal_evidence WHERE assignment_id=?",
        )
        .bind(&run.assignment_id)
        .fetch_one(&store.pool_for_tests())
        .await?;
        let path = controller.root.join(path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(&path, b"tampered")?;
    }
    let reopened = AuditStore::open(controller.root.clone()).await?;
    if corrupt {
        assert!(
            reopened
                .client_terminal_result(&run.operation_id)
                .await
                .is_err()
        );
    } else {
        assert_eq!(
            reopened
                .client_terminal_result(&run.operation_id)
                .await?
                .unwrap(),
            raw
        );
    }
    assert_eq!(
        settled(&controller, &id).await?.assignment_state,
        if corrupt {
            AssignmentState::Unknown
        } else {
            AssignmentState::Succeeded
        }
    );
    assert_eq!(
        receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
        1
    );
    threads
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(
        store.execution(&run.operation_id).await?,
        original_execution
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("stream-launches"))?
            .lines()
            .count(),
        1
    );
    println!(
        "C62 terminal persistence/read recovery: corrupt={corrupt}, read_failure={read_failure:?}, original evidence verified or refused, one launch and prompt"
    );
    Ok(())
}

#[test]
fn controls_commit_only_the_received_prefix_while_flush_is_held() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-control-prefix", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = seed_stream(&controller, &temp, "valid", true).await?;
        std::fs::write(
            temp.path().join("stream-scenario.json"),
            json!({"mode":"valid","gate":true,"gate_start":true}).to_string(),
        )?;
        controller.drive(&id).await?;
        path_ready(&temp.path().join("before-public.ready")).await?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let capture = controller
            .dispatch
            .capture(&run.operation_id)
            .expect("external capture");
        let held = capture.hold_flush().await;
        std::fs::write(temp.path().join("before-public.release"), "go")?;
        path_ready(&temp.path().join("before-terminal.ready")).await?;
        let prefix = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let prefix = capture.control_prefix();
                capture.completion_finished(false);
                if prefix
                    .iter()
                    .any(|entry| entry.kind == "tool_result" && entry.item_id == "call-2")
                {
                    break prefix;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        assert!(!prefix.is_empty());
        assert!(
            store.activity(&run.assignment_id, 0, 100).await?.is_empty(),
            "buffered output must not be historical activity"
        );
        let decision=api(&controller,"audit/attempt/control",json!({"runId":id,"assignmentId":run.assignment_id,"actionKey":"c62-stop","action":"cancel","reason":"fixture prefix decision"})).await.expect("prefix control");
        let activity = store.activity(&run.assignment_id, 0, 100).await?;
        assert!(!activity.is_empty());
        assert_eq!(
            decision["cutoff"].as_i64(),
            activity.last().map(|entry| entry.sequence)
        );
        assert!(
            activity
                .iter()
                .any(|entry| entry.kind == "tool_result" && entry.item_id == "call-2")
        );
        drop(held);
        std::fs::write(temp.path().join("before-terminal.release"), "late")?;
        controller.drive(&id).await?;
        assert_eq!(store.activity(&run.assignment_id, 0, 100).await?, activity);
        assert!(
            store
                .client_terminal_result(&run.operation_id)
                .await?
                .is_none()
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C62 received prefix: unavailable to readers before commit; control transaction retained prefix and fenced terminal"
        );
        Ok(())
    })
}

#[test]
fn api_client_terminal_without_strict_admissions_is_refused() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-strict-api", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = seed_stream(&controller, &temp, "valid", false).await?;
        let settings: BackendSettings = serde_json::from_value(json!({"version":1,"claudeApi":{
            "executable":temp.path().join("fake-client"),"clientVersion":"2.1.314","runtimeRoot":temp.path().join("client-runtime").canonicalize()?,
            "credential":{"kind":"environment","reference":"UNSET_C62_FIXTURE_KEY"},"upstream":"https://api.anthropic.com"
        }}))?;
        std::fs::write(
            controller.root.join("reviewer-backends.json"),
            serde_json::to_vec(&settings)?,
        )?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let mut input = RetainedInput::decode(&store.attempt_input(&run.operation_id).await?.0)?;
        input.authentication_route = Some(AuditAuthenticationRoute::ClaudeApi);
        input
            .bindings
            .as_mut()
            .unwrap()
            .backend_configuration_identity = settings.identity(&input.route())?;
        let raw = serde_json::to_string(&input)?;
        let digest = sha256_hex(raw.as_bytes());
        sqlx::query(
            "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
        )
        .bind(raw)
        .bind(&digest)
        .bind(&run.assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
        sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
            .bind(digest)
            .bind(&run.operation_id)
            .execute(&store.pool_for_tests())
            .await?;
        let result = settled(&controller, &id).await?;
        assert_eq!(result.assignment_state, AssignmentState::Unknown);
        assert!(
            store
                .generation_records(&run.assignment_id)
                .await?
                .is_empty()
        );
        let retained:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_client_terminal_evidence WHERE assignment_id=? AND artefact_digest IS NOT NULL)").bind(&run.assignment_id).fetch_one(&store.pool_for_tests()).await?;
        assert!(
            retained,
            "original terminal retained without pretending it was admissible"
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C62 strict Claude API: permit-free terminal retained but rejected; no fabricated admissions"
        );
        Ok(())
    })
}

#[test]
fn client_session_cannot_be_reused_by_another_attempt() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-session-binding", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let first = seed_stream(&controller, &temp, "first", true).await?;
        std::fs::write(
            temp.path().join("stream-scenario.json"),
            json!({"mode":"valid","gate":true,"session":"shared-session"}).to_string(),
        )?;
        controller.drive(&first).await?;
        path_ready(&temp.path().join("before-terminal.ready")).await?;
        let store = controller.store().await?;
        let first_run = store.load_run(&first).await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if store
                    .client_launch(&first_run.operation_id)
                    .await?
                    .unwrap()
                    .client_session_id
                    .is_some()
                {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        let second = seed_stream(&controller, &temp, "second", false).await?;
        std::fs::write(
            temp.path().join("stream-scenario.json"),
            json!({"mode":"valid","session":"shared-session"}).to_string(),
        )?;
        let second_run = settled(&controller, &second).await?;
        assert!(matches!(
            second_run.assignment_state,
            AssignmentState::Unknown | AssignmentState::Failed
        ));
        assert!(
            store
                .model_evidence(&second_run.assignment_id)
                .await?
                .is_empty()
        );
        std::fs::write(temp.path().join("before-terminal.release"), "go")?;
        assert_eq!(
            settled(&controller, &first).await?.assignment_state,
            AssignmentState::Succeeded
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C62 cross-attempt session reuse: rejected without contaminating either contribution"
        );
        Ok(())
    })
}

#[test]
fn attribution_and_capture_failures_win_between_observation_and_acceptance() -> anyhow::Result<()> {
    run_test_with_large_stack("c62-observation-races", || async {
        for (mode, initial) in [("missing_model", true), ("malformed", false)] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, mode, true).await?;
            if initial {
                std::fs::write(
                    temp.path().join("stream-scenario.json"),
                    json!({"mode":mode,"gate_start":true}).to_string(),
                )?;
            } else {
                controller.drive(&id).await?;
                path_ready(&temp.path().join("before-terminal.ready")).await?;
            }
            let store = controller.store().await?;
            let (entered, release) = store.pause_transition_for_test("external_observation");
            let driver = controller.clone();
            let run_id = id.clone();
            let task = tokio::spawn(async move { driver.drive(&run_id).await });
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let run = store.load_run(&id).await?;
            std::fs::write(
                temp.path().join(if initial {
                    "before-public.release"
                } else {
                    "before-terminal.release"
                }),
                "go",
            )?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if store
                        .client_capture_failure(&run.assignment_id)
                        .await?
                        .is_some()
                    {
                        break anyhow::Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await??;
            release.add_permits(1);
            task.await??;
            let after = settled(&controller, &id).await?;
            assert!(matches!(
                after.assignment_state,
                AssignmentState::Failed | AssignmentState::Unknown
            ));
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            assert!(
                store
                    .client_terminal_result(&run.operation_id)
                    .await?
                    .is_none()
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C62 observation transaction race {mode}: durable failure retained, no stale acceptance or redispatch"
            );
        }
        Ok(())
    })
}
