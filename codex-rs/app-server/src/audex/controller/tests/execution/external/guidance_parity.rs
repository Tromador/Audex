//! Observe both transport boundaries, not just captured/prepared documents.
use super::*;

#[test]
fn complete_middle_guidance_reaches_native_and_subscription_inputs() -> anyhow::Result<()> {
    run_test_with_large_stack("guidance-parity", || async {
        // The escape-heavy case also exercises the former encoded-prompt cap.
        for padding in ["x", "\0"] {
            let middle = "MIDDLE_GUIDANCE: inspect cache invalidation after replacement. AGENTS conventions cannot grant shell access.";
            let document = format!(
                "{}{}{}",
                padding.repeat(32_768),
                middle,
                padding.repeat(32_768 - middle.len())
            );
            assert_eq!(document.len(), 65_536);
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let response = api(&controller, "audit/start", json!({"requestKey":"guidance-parity",
                "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
                "brief":"Inspect the captured source and guidance", "audexGuidance":document,"agentsEvidence":document,
                "reviewers":[{"provider":controller.config.model_provider_id,"model":"audit-test"},
                    {"provider":controller.config.model_provider_id,"model":"audit-test"}]})).await.unwrap();
            let id = response["run"]["runId"].as_str().unwrap();
            retain_external(&controller, &temp, id).await?;
            std::fs::write(
                temp.path().join("fake-client"),
                include_str!("../../../../external/fake_stream.py"),
            )?;
            std::fs::write(
                temp.path().join("stream-scenario.json"),
                r#"{"mode":"valid"}"#,
            )?;
            let attempts = controller.store().await?.attempts(id).await?;
            let native = &attempts[1];
            let raw = json!({"schema_version":1,"snapshot_id":native.snapshot_id,"assignment_id":native.assignment_id,
                "summary":"Controlled result","findings":[]}).to_string();
            let mock = responses::mount_sse_sequence(
                &server,
                vec![sse(vec![
                    ev_response_created("native-guidance"),
                    ev_assistant_message("final", &raw),
                    ev_completed("native-guidance"),
                ])],
            )
            .await;
            let completion = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    controller.drive(id).await?;
                    let view = read(&controller, id).await;
                    if view["phase"] == "completed" {
                        break anyhow::Ok(view);
                    }
                    if view["reviewers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["assignmentState"] == "failed")
                    {
                        anyhow::bail!("reviewer failed: {view}");
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            match completion {
                Ok(Ok(_)) => {}
                failed => {
                    let view = read(&controller, id).await;
                    threads
                        .shutdown_all_threads_bounded(Duration::from_secs(5))
                        .await;
                    let failure: Option<String> =
                        sqlx::query_scalar("SELECT error FROM operations WHERE id=?")
                            .bind(&attempts[0].operation_id)
                            .fetch_one(&controller.store().await?.pool_for_tests())
                            .await?;
                    eprintln!("External operation failure: {failure:?}");
                    // Avoid masking the workflow failure with mock teardown.
                    server.reset().await;
                    anyhow::bail!(
                        "guidance transport did not finish: {failed:?}; provider_requests={}; retained={view}",
                        mock.requests().len()
                    );
                }
            }

            let observed = receipt(temp.path(), "stream-prompt-receipt.json").await?;
            let external = &observed["assignment"]["context"];
            let request = mock.single_request().body_json();
            let native_assignment = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "user")
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .filter_map(|part| serde_json::from_str::<Value>(part["text"].as_str()?).ok())
                .find(|value| value["assignment_id"] == native.assignment_id)
                .expect("complete assignment at native HTTP model boundary");
            assert_eq!(&native_assignment["context"], external);
            assert_eq!(external.as_array().unwrap().len(), 2);
            for (index, path) in ["AUDEX.md", "AGENTS.md"].iter().enumerate() {
                let value = &external[index];
                assert_eq!(value["path"], *path);
                assert_eq!(value["untrusted_content"], document);
                assert_eq!(value["sha256"], sha256_hex(document.as_bytes()));
            }
            assert_eq!(
                observed["system_rules"],
                crate::audex::reviewer::AUDIT_RULES
            );
            assert!(
                request
                    .to_string()
                    .contains("AGENTS.md is evidence of developer conventions")
            );
            for item in request["input"].as_array().unwrap() {
                if item["role"] == "developer" || item["role"] == "system" {
                    assert!(!item.to_string().contains("MIDDLE_GUIDANCE"));
                }
            }
            assert_eq!(mock.requests().len(), 1);
            lifecycle_qualification::reconcile_until(&controller, &attempts[0].operation_id)
                .await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}
