//! Pinned-client shape and usage qualifications from C6.2 independent review.
use super::*;

#[test]
fn shared_message_records_reconstruct_stream_blocks_without_duplicates() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-multi-record", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = capture::seed_stream(&controller, &temp, "multi_record", false).await?;
        let run = capture::settled(&controller, &id).await?;
        assert_eq!(run.assignment_state, AssignmentState::Succeeded);
        let store = controller.store().await?;
        let activity = store.activity(&run.assignment_id, 0, 100).await?;
        let text = activity
            .iter()
            .filter(|e| e.kind.starts_with("assistant_"))
            .map(|e| e.payload.as_str())
            .collect::<String>();
        assert_eq!(text.matches("Distinct equal block.").count(), 2);
        assert_eq!(text.matches("Valid external review").count(), 1);
        for call in ["multi-call-1", "multi-call-2"] {
            assert_eq!(
                activity
                    .iter()
                    .filter(|e| e.kind == "tool_start" && e.item_id == call)
                    .count(),
                1
            );
            assert_eq!(
                activity
                    .iter()
                    .filter(|e| e.kind == "tool_result" && e.item_id == call)
                    .count(),
                1
            );
        }
        assert!(
            activity
                .windows(2)
                .all(|w| w[1].sequence == w[0].sequence + 1)
        );
        assert!(
            store
                .capture_state(&run.assignment_id)
                .await?
                .failure
                .is_none()
        );
        capture::assert_no_private_files(controller.root.as_path())?;
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C63 R1: thinking and two equal public blocks share one message; partial/full capture once; repeated and distinct tools preserve call identity"
        );
        Ok(())
    })
}

#[test]
fn per_message_output_placeholders_are_unavailable_but_terminal_usage_is_retained()
-> anyhow::Result<()> {
    run_test_with_large_stack("c63-usage", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = capture::seed_stream(&controller, &temp, "valid", false).await?;
        let run = capture::settled(&controller, &id).await?;
        assert_eq!(run.assignment_state, AssignmentState::Succeeded);
        let store = controller.store().await?;
        let raw: Vec<String> = sqlx::query_scalar(
            "SELECT usage_json FROM audit_client_model_observations WHERE assignment_id=?",
        )
        .bind(&run.assignment_id)
        .fetch_all(&store.pool_for_tests())
        .await?;
        for usage in raw {
            let usage: Value = serde_json::from_str(&usage)?;
            assert!(
                usage["output_tokens"].is_null(),
                "client message output is a placeholder"
            );
            assert_eq!(usage["input_tokens"], 11);
        }
        let frozen = serde_json::to_value(store.freeze_report(&id).await?)?;
        let rows = frozen["reviewers"][0]["provider_facts"]["usage"]["value"]
            .as_array()
            .unwrap();
        for row in rows {
            if row["evidence_source"] == "claude_client_message" {
                assert_eq!(row["output_tokens"]["status"], "unavailable");
                assert!(
                    row["output_tokens"]["reason"]
                        .as_str()
                        .unwrap()
                        .contains("placeholder")
                );
            } else {
                assert_eq!(row["output_tokens"]["value"], 35);
                assert_eq!(row["cost"]["value"]["measured"], false);
            }
        }
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C63 R3: no per-message output placeholder persisted or projected as usage; terminal aggregate retained separately"
        );
        Ok(())
    })
}
