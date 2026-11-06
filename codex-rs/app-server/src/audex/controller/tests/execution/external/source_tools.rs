//! Offline owned-client receipts through the real collector, completion and publication.
use super::*;
use crate::audex::report::source_tools::Outcome;

#[path = "source_capture_race.rs"]
mod capture_race;

#[test]
fn claude_failed_read_receipts_survive_completed_opinions_and_publication() -> anyhow::Result<()> {
    run_test_with_large_stack("source-tool-failures", || async {
        let limit = "read size must be between 1 and 65536 bytes";
        let secret = "PRIVATE_SENTINEL_MUST_NOT_PERSIST";
        for (mode, content, expected) in [
            (
                "valid",
                json!([{"type":"text","text":limit},{"type":"thinking","thinking":secret}]),
                Some(limit),
            ),
            (
                "invalid_result",
                json!("read offset 999 exceeds the entry size 13"),
                Some("read offset 999 exceeds the entry size 13"),
            ),
            (
                "valid",
                json!("source access limit exceeded"),
                Some("source access limit exceeded"),
            ),
            (
                "valid",
                json!(format!("storage failure at /host/{secret}")),
                None,
            ),
            ("valid", json!(format!("{limit}: /host/{secret}")), None),
            (
                "valid",
                json!([{"type":"text","text":limit},{"type":"text","text":secret}]),
                None,
            ),
            ("valid", Value::Null, None),
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = capture::seed_stream(&controller, &temp, mode, false).await?;
            std::fs::write(
                temp.path().join("stream-scenario.json"),
                json!({"mode":mode,"gate":false,"source_error":content}).to_string(),
            )?;
            let run = capture::settled(&controller, &id).await?;
            let failure: Option<String> =
                sqlx::query_scalar("SELECT failure_code FROM reviewer_assignments WHERE id=?")
                    .bind(&run.assignment_id)
                    .fetch_one(&controller.store().await?.pool_for_tests())
                    .await?;
            assert_eq!(
                run.assignment_state,
                AssignmentState::Succeeded,
                "retained failure: {failure:?}"
            );
            let public = finished(&controller, &id).await;
            assert_eq!(public["outcome"], "success");
            let store = controller.store().await?;
            let activity = store.activity(&run.assignment_id, 0, 100).await?;
            let receipt = activity
                .iter()
                .find(|e| e.kind == "tool_result" && e.item_id == "call-1")
                .unwrap();
            let error: Value = serde_json::from_str(&receipt.payload)?;
            assert_eq!(error["error"], "source_tool_error");
            assert_eq!(error["message"].as_str(), expected);
            assert_eq!(
                error["detail"],
                if expected.is_some() {
                    "available"
                } else {
                    "unavailable"
                }
            );
            assert_eq!(
                activity.iter().filter(|e| e.kind == "tool_result").count(),
                2,
                "repeated receipts deduplicate"
            );
            let frozen = store.freeze_report(&id).await?;
            assert_eq!(frozen.outcome, "success");
            assert!(frozen.reviewers[0].opinion.is_some());
            let calls = &frozen.reviewers[0].source_tools.as_ref().unwrap().calls;
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].call_id, "call-1");
            assert_eq!(calls[0].tool.as_deref(), Some("read"));
            assert_eq!(calls[0].outcome, Outcome::Failed);
            assert_eq!(calls[0].failure.as_ref().unwrap(), &error);
            assert_eq!(
                calls[0].arguments,
                Some(json!({"path":"example.rs","offset":0,"max_bytes":200000}))
            );
            assert_eq!(calls[1].outcome, Outcome::Success);
            let text = public["report"].as_str().unwrap();
            assert!(text.contains("1 succeeded; 1 failed; 0 unavailable"));
            assert!(text.contains("source_tool_error"));
            assert!(text.contains(&run.operation_id));
            assert!(text.contains(r"Route: claude\_subscription"));
            assert!(text.contains("- Attempt: `1`"));
            assert!(text.contains("call-1"));
            if let Some(message) = expected {
                assert!(text.contains(message));
            } else {
                assert!(text.contains("error detail unavailable"));
            }
            assert!(!text.contains(secret));
            assert!(!text.contains("/host/"));
            let reopened = AuditStore::open(controller.root.clone()).await?;
            assert_eq!(
                serde_json::to_vec(&reopened.freeze_report(&id).await?)?,
                serde_json::to_vec(&frozen)?
            );
            assert_eq!(
                reopened.read_report(&reopened.load_run(&id).await?.report.unwrap())?,
                text.as_bytes()
            );
            assert!(
                !store
                    .raw_result(&store.load_run(&id).await?)?
                    .unwrap()
                    .windows(b"source_tool_error".len())
                    .any(|w| w == b"source_tool_error"),
                "the failure is independent of the completed opinion"
            );
            capture::assert_no_private_files(controller.root.as_path())?;
            assert!(
                server.received_requests().await.unwrap().is_empty(),
                "no provider calls"
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            reopened.close().await;
        }
        Ok(())
    })
}

async fn replay_source_receipt(
    label: &str,
    receipt: Value,
    operation: &str,
    arguments: Value,
    expected: Option<Value>,
) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let server = responses::start_mock_server().await;
    let (controller, threads) = real_fixture(&temp, &server.uri()).await;
    let id = capture::seed_stream(&controller, &temp, "valid", false).await?;
    std::fs::write(
        temp.path().join("stream-scenario.json"),
        json!({"mode":"valid","gate":false,
        "source_receipt":receipt,"source_operation":operation,"source_arguments":arguments})
        .to_string(),
    )?;
    let run = capture::settled(&controller, &id).await?;
    let store = controller.store().await?;
    let activity = store.activity(&run.assignment_id, 0, 100).await?;
    let result = activity
        .iter()
        .find(|e| e.kind == "tool_result" && e.item_id == "call-1");
    let failure = store.client_capture_failure(&run.assignment_id).await?;
    if let Some(expected) = expected {
        assert_eq!(
            run.assignment_state,
            AssignmentState::Succeeded,
            "{label}: {failure:?}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&result.expect("retained complete source").payload)?,
            expected
        );
        let frozen = store.freeze_report(&id).await?;
        assert!(frozen.reviewers[0].opinion.is_some());
        assert_eq!(
            frozen.reviewers[0].source_tools.as_ref().unwrap().calls[0].outcome,
            Outcome::Success
        );
    } else {
        assert_eq!(run.assignment_state, AssignmentState::Failed, "{label}");
        assert!(
            result.is_none(),
            "a substituted notice cannot claim source coverage"
        );
        assert_eq!(failure.as_deref(), Some("external_tool_result_invalid"));
    }
    if let Ok(root) = std::env::var("AUDEX_FIDELITY_EVIDENCE") {
        let root = PathBuf::from(root).join(label);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("collector.json"),
            json!({"assignment_state":format!("{:?}",run.assignment_state),
            "capture_failure":failure,"activity":activity})
            .to_string(),
        )?;
    }
    capture::assert_no_private_files(controller.root.as_path())?;
    assert!(server.received_requests().await.unwrap().is_empty());
    threads
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}

#[test]
fn claude_source_delivery_shapes_are_complete_or_fail_without_false_coverage() -> anyhow::Result<()>
{
    run_test_with_large_stack("source-output-collector", || async {
        let value = json!({"path":"example.rs","sha256":"a".repeat(64),"offset":0,"end_offset":65536,
            "eof":true,"encoding":"utf8","content":"\0".repeat(65536)});
        replay_source_receipt(
            "complete",
            json!({"type":"tool_result","content":[{"type":"text","text":value.to_string()}]}),
            "read",
            json!({"path":"example.rs","offset":0,"max_bytes":65536}),
            Some(value),
        )
        .await?;
        for (label, text) in [
            (
                "character-replacement",
                "<persisted-output>\nOutput too large (60.2KB). Full output saved to: /private/PRIVATE_SENTINEL_MUST_NOT_PERSIST\n\nPreview (first 2KB):\n{\"content\":\"xxx\n</persisted-output>",
            ),
            (
                "token-replacement",
                "Error: result (192,013 characters) exceeds maximum allowed tokens. Output has been saved to /private/PRIVATE_SENTINEL_MUST_NOT_PERSIST.",
            ),
        ] {
            replay_source_receipt(
                label,
                json!({"type":"tool_result","is_error":false,"content":text}),
                "list",
                json!({}),
                None,
            )
            .await?;
        }
        Ok(())
    })
}

#[test]
#[ignore = "replays retained offline pinned-client receipts; never starts the client"]
fn recorded_claude_receipts_match_retained_source_evidence() -> anyhow::Result<()> {
    run_test_with_large_stack("recorded-source-output", || async {
        let root = PathBuf::from(std::env::var("AUDEX_FIDELITY_PINNED_EVIDENCE")?);
        let cases: Vec<Value> =
            serde_json::from_slice(&std::fs::read(root.join("source-cases.json"))?)?;
        let stream = std::fs::read_to_string(root.join("stream.jsonl"))?;
        let records = stream
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        for case in cases {
            let receipt = records
                .iter()
                .filter(|v| v["type"] == "user")
                .filter_map(|v| v["message"]["content"].as_array())
                .flatten()
                .find(|b| b["tool_use_id"] == case["id"])
                .unwrap()
                .clone();
            let content = &receipt["content"];
            let text = content.as_str().map(str::to_owned).unwrap_or_else(|| {
                content
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect()
            });
            let expected = serde_json::from_str::<Value>(&text).ok();
            replay_source_receipt(
                case["id"].as_str().unwrap(),
                receipt,
                case["name"].as_str().unwrap(),
                case["input"].clone(),
                expected,
            )
            .await?;
        }
        Ok(())
    })
}
