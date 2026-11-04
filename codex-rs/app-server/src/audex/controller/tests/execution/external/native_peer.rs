//! Mixed roster: actual native Core and an external sibling share one audit.
use super::*;

#[test]
fn native_peer_finishes_while_external_sibling_runs_fails_or_is_cancelled() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-mixed-native", || async {
        for behaviour in ["running", "failed", "cancelled"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let response = api(&controller,"audit/start",json!({"requestKey":"mixed", "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
                "brief":"Static correctness audit", "reviewers":[
                    {"provider":controller.config.model_provider_id,"model":"audit-test"},
                    {"provider":controller.config.model_provider_id,"model":"audit-test"}]})).await.expect("roster");
            let id = response["run"]["runId"].as_str().unwrap();
            retain_external(&controller, &temp, id).await?;
            std::fs::write(
                temp.path().join("fake-client"),
                include_str!("../../../../external/fake_stream.py"),
            )?;
            std::fs::write(
                temp.path().join("stream-scenario.json"),
                json!({"mode":if behaviour=="failed" {"malformed"} else {"valid"},"gate":true})
                    .to_string(),
            )?;
            let store = controller.store().await?;
            let attempts = store.attempts(id).await?;
            let external = &attempts[0];
            let native = &attempts[1];
            let raw = json!({"schema_version":1,"assignment_id":native.assignment_id,"snapshot_id":native.snapshot_id,
                "summary":"Concurrent native review","findings":[]}).to_string();
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v1/responses"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse(vec![
                            ev_response_created("native-peer"),
                            ev_assistant_message("native-final", &raw),
                            ev_completed("native-peer"),
                        ]))
                        .set_delay(Duration::from_secs(1)),
                )
                .expect(1)
                .mount(&server)
                .await;
            controller.drive(id).await?;
            receipt(temp.path(), "stream-prompt-receipt.json").await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if server
                        .received_requests()
                        .await
                        .unwrap()
                        .iter()
                        .any(|r| r.url.path() == "/v1/responses")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("native request timeout: {e}"))?;
            assert_ne!(
                store
                    .run_for_operation(&native.operation_id)
                    .await?
                    .assignment_state,
                AssignmentState::Succeeded
            );
            if behaviour == "cancelled" {
                api(&controller,"audit/attempt/control",json!({"runId":id,"assignmentId":external.assignment_id,
                    "actionKey":"stop-external","action":"cancel","reason":"mixed peer qualification"})).await.expect("cancel");
            } else if behaviour == "failed" {
                std::fs::write(temp.path().join("before-terminal.release"), "go")?;
            }
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    controller.drive(id).await?;
                    if store
                        .run_for_operation(&native.operation_id)
                        .await?
                        .assignment_state
                        == AssignmentState::Succeeded
                    {
                        break anyhow::Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("native completion timeout: {e}"))??;
            if behaviour == "running" {
                let launch = store.client_launch(&external.operation_id).await?.unwrap();
                assert!(
                    codex_app_server_daemon::read_linux_child_identity(
                        launch.pid.unwrap() as u32,
                        &launch.mcp_config_path
                    )
                    .is_ok()
                );
                std::fs::write(temp.path().join("before-terminal.release"), "go")?;
            }
            if behaviour == "failed" {
                assert!(matches!(
                    store
                        .run_for_operation(&external.operation_id)
                        .await?
                        .assignment_state,
                    AssignmentState::Failed | AssignmentState::Unknown
                ));
                // A failed roster contribution still needs the operator's explicit
                // disposition. Native success is independent of that decision.
                api(&controller,"audit/attempt/control",json!({"runId":id,"assignmentId":external.assignment_id,
                    "actionKey":"retain-failed-partial","action":"cancel","reason":"retain failed partial"})).await.expect("disposition");
            }
            let final_run = finished(&controller, id).await;
            // Existing collection semantics publish success when at least one
            // canonical contribution is complete; each reviewer's status stays explicit.
            assert_eq!(final_run["phase"], "completed");
            let frozen = store.freeze_report(id).await?;
            assert!(frozen.reviewers[1].completed.is_some());
            assert_eq!(
                frozen.reviewers[0].completed.is_some(),
                behaviour == "running"
            );
            super::lifecycle_qualification::reconcile_until(&controller, &external.operation_id)
                .await?;
            assert_eq!(
                store.generation_records(&native.assignment_id).await?.len(),
                1
            );
            assert!(
                store
                    .generation_records(&external.assignment_id)
                    .await?
                    .is_empty()
            );
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C63 same-audit concurrent native peer: external={behaviour}; one real native generation accepted; no cross-attempt stop/capture contamination"
            );
        }
        Ok(())
    })
}
