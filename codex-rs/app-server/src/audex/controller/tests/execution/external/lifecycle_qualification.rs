//! Controls and recovery through the production composition, with actual owned processes.
use super::capture::{seed_stream, settled};
use super::*;

pub(super) async fn reconcile_until(
    controller: &Arc<AuditController>,
    operation: &str,
) -> anyhow::Result<Value> {
    let mut last = String::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            controller.tick().await?;
            let row: (String, String, Option<i64>, Option<i64>) = sqlx::query_as(
                "SELECT local_status,ownership_json,local_stop_confirmed_at_ms,cleanup_finished_at_ms FROM audit_client_launches WHERE operation_id=?")
                .bind(operation).fetch_one(&controller.store().await?.pool_for_tests()).await?;
            last = format!("status={}, evidence={}, stopped={:?}", row.0,row.1,row.2);
            if row.3.is_some() {
                assert!(row.2.is_some());
                return anyhow::Ok(serde_json::from_str(&row.1)?);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.map_err(|e| anyhow::anyhow!("cleanup timeout: {e}; {last}"))?
}

pub(super) async fn observed_socket(
    controller: &Arc<AuditController>,
    operation: &str,
) -> anyhow::Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            controller.dispatch.reconcile_resources().await?;
            let row = controller
                .store()
                .await?
                .launch_lifecycle(operation)
                .await?;
            let value: Value = serde_json::from_str(&row.ownership)?;
            if let Some(failure) = controller
                .store()
                .await?
                .client_capture_failure(
                    &controller
                        .store()
                        .await?
                        .run_for_operation(operation)
                        .await?
                        .assignment_id,
                )
                .await?
            {
                anyhow::bail!("socket observation capture failed: {failure}; {value}");
            }
            if value["sockets"].as_array().is_some_and(|s| !s.is_empty()) {
                return anyhow::Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[test]
fn exact_controls_stop_owned_group_bridge_and_socket_without_replay() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-controls", || async {
        for action in ["cancel", "ignore", "retry", "abort"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", true).await?;
            std::fs::write(
                temp.path().join("stream-scenario.json"),
                json!({"mode":"valid","gate":true,"lifecycle":true}).to_string(),
            )?;
            controller.drive(&id).await?;
            let ready = receipt(temp.path(), "lifecycle-ready.json")
                .await
                .map_err(|e| anyhow::anyhow!("lifecycle fixture startup: {e}"))?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let original = store.execution(&run.operation_id).await?.unwrap().0;
            let evidence = observed_socket(&controller, &run.operation_id).await?;
            assert!(
                evidence["processes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["role"] == "bridge")
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(capture) = controller.dispatch.capture(&run.operation_id) {
                        capture.flush().await?;
                    }
                    if !store.activity(&run.assignment_id, 0, 100).await?.is_empty() {
                        break anyhow::Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await??;
            let started = std::time::Instant::now();
            let response = if action == "abort" {
                controller.abort_audit(&id).await?;
                Value::Null
            } else {
                api(&controller, "audit/attempt/control", json!({"runId":id,"assignmentId":run.assignment_id,
                    "actionKey":format!("c63-{action}"),"action":action,"reason":"lifecycle qualification",
                    "authoriseRetry":action=="retry","acknowledgePossibleOverlap":true})).await.expect("control")
            };
            assert!(started.elapsed() < Duration::from_secs(2));
            // Do not admit the replacement until the fixture is deliberately released.
            if action == "retry" {
                assert_ne!(response["replacementAssignmentId"], run.assignment_id);
                std::fs::write(temp.path().join("before-terminal.release"), "go")?;
            }
            let clean = reconcile_until(&controller, &run.operation_id).await?;
            assert_eq!(
                clean["sockets"][0]["disposition"],
                "removed_after_local_stop"
            );
            let view = api(
                &controller,
                "audit/attempt/read",
                json!({"runId":id,"assignmentId":run.assignment_id}),
            )
            .await
            .expect("historical attempt");
            assert_eq!(
                view["attempt"]["localExecution"]["status"],
                "local_cleanup_confirmed"
            );
            assert!(view["attempt"]["localExecution"]["stopConfirmedAtMs"].is_i64());
            assert!(!Path::new(ready["socket"].as_str().unwrap()).exists());
            for role in ["client", "bridge", "member"] {
                let pid = ready[role].as_u64().unwrap() as u32;
                assert!(
                    codex_app_server_daemon::read_linux_process_identity(pid)?.is_none(),
                    "{role} survived"
                );
            }
            assert_eq!(
                store.execution(&run.operation_id).await?.unwrap().0,
                original
            );
            let activity = store.activity(&run.assignment_id, 0, 100).await?;
            controller.tick().await?;
            assert_eq!(store.activity(&run.assignment_id, 0, 100).await?, activity);
            if action == "abort" {
                let frozen = store.freeze_report(&id).await?;
                assert_eq!(frozen.outcome, "aborted_by_operator");
                assert!(frozen.reviewers[0].completed.is_none());
                assert!(!frozen.reviewers[0].activity.is_empty());
            }
            if action == "retry" {
                let replacement = response["replacementOperationId"].as_str().unwrap();
                assert_ne!(store.execution(replacement).await?.unwrap().0, original);
                assert!(
                    api(
                        &controller,
                        "audit/attempt/control",
                        json!({"runId":id,"assignmentId":run.assignment_id,
                    "actionKey":"stale-stop","action":"cancel","reason":"stale"})
                    )
                    .await
                    .is_err()
                );
            } else {
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("stream-launches"))?
                        .lines()
                        .count(),
                    1
                );
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C63 {action}: durable control, verified group/bridge/socket cleanup, original execution retained, no automatic replay"
            );
        }
        Ok(())
    })
}

#[test]
fn terminal_report_is_final_while_socket_cleanup_is_pending_and_reconstructed() -> anyhow::Result<()>
{
    run_test_with_large_stack("c63-terminal-cleanup", || async {
        for replacement in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", true).await?;
            std::fs::write(
                temp.path().join("stream-scenario.json"),
                json!({"mode":"valid","gate":true,"lifecycle":true}).to_string(),
            )?;
            controller.drive(&id).await?;
            let ready = receipt(temp.path(), "lifecycle-ready.json")
                .await
                .map_err(|e| anyhow::anyhow!("lifecycle fixture startup: {e}"))?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            observed_socket(&controller, &run.operation_id).await?;
            std::fs::write(temp.path().join("before-terminal.release"), "go")?;
            assert_eq!(
                settled(&controller, &id).await?.assignment_state,
                AssignmentState::Succeeded
            );
            let report = finished(&controller, &id).await;
            let published_identity = store.load_run(&id).await?.report;
            let frozen = serde_json::to_value(store.freeze_report(&id).await?)?;
            assert_eq!(report["phase"], "completed");
            assert!(
                !store
                    .launch_lifecycle(&run.operation_id)
                    .await?
                    .cleanup_finished
            );
            let socket = Path::new(ready["socket"].as_str().unwrap());
            let foreign = if replacement {
                std::fs::remove_file(socket)?;
                Some(std::os::unix::net::UnixListener::bind(socket)?)
            } else {
                None
            };
            // Fresh production dispatch, store and controller; no active work candidate is required.
            let (reopened, reopened_threads) = real_fixture(&temp, &server.uri()).await;
            let clean = reconcile_until(&reopened, &run.operation_id).await?;
            assert_eq!(
                clean["sockets"][0]["disposition"],
                if replacement {
                    "replacement_preserved"
                } else {
                    "removed_after_local_stop"
                }
            );
            assert_eq!(socket.exists(), replacement);
            // The live operational receipt may change; frozen review evidence and
            // published identity/bytes must remain exactly the same.
            assert_eq!(read(&reopened, &id).await["report"], report["report"]);
            assert_eq!(store.load_run(&id).await?.report, published_identity);
            assert_eq!(
                serde_json::to_value(store.freeze_report(&id).await?)?,
                frozen
            );
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            drop(foreign);
            if replacement {
                std::fs::remove_file(socket)?;
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            reopened_threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C63 terminal parent recovery: replacement={replacement}; report unchanged; recorded socket only; no model replay"
            );
        }
        Ok(())
    })
}
