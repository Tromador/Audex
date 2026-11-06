//! Actual native source execution -> retained capture -> completion -> public report.
use super::*;
use crate::audex::capture::CaptureHandle;
use crate::audex::report::source_tools::Outcome;
use crate::audex::snapshot::SnapshotHandle;
use crate::audex::tools::SourceTools;
use codex_extension_api::{ExtensionData, ToolContributor};
use codex_tools::{
    ConversationHistory, NoopTurnItemEmitter, ToolCall, ToolCallSource, ToolPayload,
};

async fn source_call(
    snapshot: &SnapshotHandle,
    capture: &CaptureHandle,
    name: &str,
    arguments: &str,
    turn: &str,
    plain: bool,
) -> Result<(), String> {
    let tools = SourceTools::with_plain_functions(snapshot, plain).unwrap();
    let scope = ExtensionData::new("native-source-outcomes");
    scope.insert(capture.clone());
    let tools = tools.tools(&scope, &scope);
    let tool = &tools[crate::audex::source::TOOL_NAMES
        .iter()
        .position(|n| *n == name)
        .unwrap()];
    tool.handle(ToolCall {
        turn_id: turn.into(),
        call_id: "same-call-id".into(),
        tool_name: tool.tool_name(),
        model: "fixture".into(),
        codex_turn_metadata: None,
        truncation_policy: codex_protocol::protocol::TruncationPolicy::Bytes(usize::MAX),
        source: ToolCallSource::Direct,
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: Arc::new(NoopTurnItemEmitter),
        attachment_store: Arc::new(codex_attachment_store::InlineAttachmentStore),
        ephemeral: false,
        environments: Vec::new(),
        payload: ToolPayload::Function {
            arguments: arguments.into(),
        },
    })
    .await
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[tokio::test]
async fn native_failed_reads_freeze_beside_completed_opinions_and_incomplete_attempts()
-> anyhow::Result<()> {
    for mode in ["structured", "unstructured", "incomplete", "clean"] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        let id = start(&controller, mode).await;
        controller.tick().await?;
        let store = controller.store().await?;
        let attempt = store.load_run_metadata(&id).await?;
        let capture = super::super::attempts::capture(&controller, &fake, &attempt).await?;
        let snapshot = controller
            .snapshots()
            .open(&SnapshotId::parse(attempt.snapshot_id.clone())?)
            .await?;
        if mode != "clean" {
            let error = source_call(
                &snapshot,
                &capture,
                "read",
                r#"{"path":"example.rs","offset":0,"max_bytes":200000}"#,
                "failed-turn",
                false,
            )
            .await
            .unwrap_err();
            assert_eq!(error, "read size must be between 1 and 65536 bytes");
        }
        source_call(
            &snapshot,
            &capture,
            "read",
            r#"{"path":"example.rs","offset":0,"max_bytes":1}"#,
            "successful-turn",
            true,
        )
        .await
        .unwrap();
        capture.finish().await?;
        let activity = store.activity(&attempt.assignment_id, 0, 100).await?;
        assert_eq!(activity.len(), if mode == "clean" { 2 } else { 4 });
        if mode != "clean" {
            assert_eq!(activity[0].kind, "tool_start");
            assert_eq!(
                serde_json::from_str::<Value>(&activity[0].payload)?["arguments"],
                json!({"path":"example.rs","offset":0,"max_bytes":200000})
            );
            assert_eq!(activity[1].kind, "tool_result");
            assert_eq!(activity[1].item_id, "same-call-id");
            assert_eq!(
                serde_json::from_str::<Value>(&activity[1].payload)?["category"],
                "source_access_limit"
            );
        }
        if mode == "incomplete" {
            store
                .control_attempt(
                    &control(&id, &attempt, "cancel", AttemptAction::Cancel),
                    None,
                )
                .await?;
        } else if mode == "unstructured" {
            attest_fake_completion(store, &attempt).await?;
            assert!(matches!(
                store
                    .commit_seeded_result(
                        &attempt.operation_id,
                        attempt.attempt_revision,
                        &snapshot,
                        b"Not assessed"
                    )
                    .await?,
                ResultCommitOutcome::Committed { .. }
            ));
        } else {
            complete(&controller, &attempt, false).await?;
        }
        let frozen = store.freeze_report(&id).await?;
        assert_eq!(
            frozen.outcome,
            if mode == "incomplete" {
                "no_completed_review"
            } else {
                "success"
            }
        );
        assert_eq!(frozen.reviewers[0].opinion.is_some(), mode != "incomplete");
        let calls = &frozen.reviewers[0].source_tools.as_ref().unwrap().calls;
        assert_eq!(calls.len(), if mode == "clean" { 1 } else { 2 });
        assert_eq!(calls.last().unwrap().outcome, Outcome::Success);
        if mode != "clean" {
            assert_eq!(calls[0].outcome, Outcome::Failed);
            assert_eq!(calls[0].turn_id, "failed-turn");
        }
        let encoded = serde_json::to_vec(&frozen)?;
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert_eq!(
            serde_json::to_vec(&reopened.freeze_report(&id).await?)?,
            encoded
        );
        let report = record(reopened.publish_frozen(&frozen).await?);
        let bytes = reopened.read_report(&report)?;
        let public = api(&controller, "audit/read", json!({"runId":id}))
            .await
            .unwrap();
        assert_eq!(public["run"]["report"].as_str().unwrap().as_bytes(), bytes);
        let text = String::from_utf8(bytes.clone())?;
        assert!(text.contains(&attempt.operation_id));
        assert!(text.contains("- Attempt: `1`"));
        assert!(text.contains(r"Route: daemon\_configured\_native"));
        assert!(text.contains("same-call-id"));
        if mode == "clean" {
            assert!(text.contains("1 succeeded; 0 failed; 0 unavailable"));
            assert!(!text.contains("source_tool_error"));
        } else {
            assert!(text.contains("1 succeeded; 1 failed; 0 unavailable"));
            assert!(text.contains("source_tool_error"));
            assert!(text.contains("read size must be between 1 and 65536 bytes"));
            assert!(text.contains("200000"));
        }
        // Late capture cannot amend a frozen/public report.
        source_call(&snapshot, &capture, "read", "{}", "late-turn", false)
            .await
            .unwrap_err();
        assert_eq!(reopened.read_report(&report)?, bytes);
        assert_eq!(
            serde_json::to_vec(&reopened.freeze_report(&id).await?)?,
            encoded
        );
        // Completed contributions now verify their selected activity too.
        sqlx::query(
            "UPDATE audit_activity SET payload='tampered' WHERE assignment_id=? AND sequence=1",
        )
        .bind(&attempt.assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
        assert!(reopened.freeze_report(&id).await.is_err());
        reopened.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn native_failed_argument_capture_redacts_paths_queries_and_unparsed_input()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "private-arguments").await;
    controller.tick().await?;
    let store = controller.store().await?;
    let attempt = store.load_run_metadata(&id).await?;
    let capture = super::super::attempts::capture(&controller, &fake, &attempt).await?;
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(attempt.snapshot_id.clone())?)
        .await?;
    for (turn, name, args) in [
        (
            "host",
            "read",
            json!({"path":"/host/PRIVATE_ARGUMENT","offset":-1,"max_bytes":3}).to_string(),
        ),
        (
            "query",
            "search",
            json!({"query":"PRIVATE_ARGUMENT","path":null,"max_results":0}).to_string(),
        ),
        ("malformed", "read", "{PRIVATE_ARGUMENT".into()),
        ("oversized", "read", "PRIVATE_ARGUMENT".repeat(2048)),
        (
            "controls",
            "read",
            json!({"path":"bad\u{202e}PRIVATE_ARGUMENT","offset":0,"max_bytes":1}).to_string(),
        ),
        (
            "offset",
            "read",
            json!({"path":"example.rs","offset":999,"max_bytes":1}).to_string(),
        ),
    ] {
        assert!(
            source_call(&snapshot, &capture, name, &args, turn, false)
                .await
                .is_err()
        );
    }
    capture.finish().await?;
    let activity = store.activity(&attempt.assignment_id, 0, 100).await?;
    assert_eq!(activity.len(), 12);
    let encoded = serde_json::to_string(&activity)?;
    assert!(!encoded.contains("PRIVATE_ARGUMENT"));
    assert!(!encoded.contains("/host/"));
    assert!(encoded.contains("query_bytes"));
    assert!(encoded.contains("invalid JSON"));
    assert!(encoded.contains("arguments exceed 16 KiB"));
    assert!(encoded.contains("read offset 999 exceeds the entry size 13"));
    assert!(activity.iter().all(|e| e.payload.len() < 512));
    let query: Value = serde_json::from_str(&activity[2].payload)?;
    assert_eq!(query["arguments"]["query_bytes"], 16);
    assert_eq!(query["arguments"]["max_results"], 0);
    complete(&controller, &attempt, false).await?;
    let frozen = store.freeze_report(&id).await?;
    let calls = &frozen.reviewers[0].source_tools.as_ref().unwrap().calls;
    assert_eq!(calls.len(), 6);
    assert!(calls.iter().all(|call| call.outcome == Outcome::Failed));
    let report = record(store.publish_frozen(&frozen).await?);
    assert!(!String::from_utf8(store.read_report(&report)?)?.contains("PRIVATE_ARGUMENT"));
    Ok(())
}
