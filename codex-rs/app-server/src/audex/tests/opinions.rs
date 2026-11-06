//! Shared extraction through real snapshots, durable completion and frozen reports.
use super::*;
use pretty_assertions::assert_eq;
mod recovery;
use codex_app_server_protocol::{
    AuditExtractionStatus as Status, AuditResultTransformation as Repair,
};

#[tokio::test]
async fn shared_output_contract_preserves_bytes_and_validates_every_structured_field()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let snapshot = capture_snapshot(&temp).await;
    let root = absolute(&temp.path().join("audex"));
    let store = AuditStore::open(root.clone()).await?;
    for case in [
        "strict",
        "structured_markup",
        "zero",
        "fence",
        "crlf",
        "bom",
        "bom_fence",
        "plain",
        "garbage",
        "malformed",
        "prose_fence",
        "multiple_fences",
        "wrong_assignment",
        "wrong_snapshot",
        "missing_id",
        "unknown_field",
        "foreign_path",
        "invalid_range",
    ] {
        let run = create_accepted_run(&store, &snapshot, case).await;
        let mut value = serde_json::to_value(valid_result(&snapshot, &run.assignment_id))?;
        match case {
            "zero" => value["findings"] = serde_json::json!([]),
            "structured_markup" => {
                value["summary"] =
                    "<script>alert(1)</script>\n[link](https://invalid.example)\n\u{1b}[31m".into()
            }
            "wrong_assignment" => value["assignment_id"] = "claimed-other-attempt".into(),
            "wrong_snapshot" => value["snapshot_id"] = "claimed-other-source".into(),
            "missing_id" => {
                value.as_object_mut().unwrap().remove("assignment_id");
            }
            "unknown_field" => value["extra"] = true.into(),
            "foreign_path" => value["findings"][0]["evidence"][0]["path"] = "outside.rs".into(),
            "invalid_range" => value["findings"][0]["evidence"][0]["end_byte"] = 99999.into(),
            _ => {}
        }
        let json = serde_json::to_string(&value)?;
        let raw = match case {
            "fence" => format!(" \n```json\n{json}\n```\n"),
            "crlf" => format!("```json\r\n{json}\r\n```"),
            "bom" => format!("\u{feff}{json}"),
            "bom_fence" => format!("\u{feff}```json\n{json}\n```"),
            "plain" => "The function appears to return its argument unchanged.\n<script>alert(1)</script>\n[link](https://invalid.example)\n```\n# heading\n\u{1b}[31m".into(),
            "garbage" => "!?~☃ xyzzy }}}".into(),
            "malformed" => "{\"summary\":\"opinion\",}".into(),
            "prose_fence" => format!("Here is my review:\n```json\n{json}\n```"),
            "multiple_fences" => format!("```json\n{json}\n```\n```json\n{json}\n```"),
            _ => json.clone(),
        };
        let structured = matches!(
            case,
            "strict" | "structured_markup" | "zero" | "fence" | "crlf" | "bom" | "bom_fence"
        );
        let normalised = matches!(case, "fence" | "crlf" | "bom" | "bom_fence");
        assert!(matches!(
            store
                .commit_seeded_result(&run.operation_id, 3, &snapshot, raw.as_bytes())
                .await?,
            ResultCommitOutcome::Committed { .. }
        ));
        let retained = store.load_run(&run.run_id).await?;
        assert_eq!(
            retained.assignment_state,
            AssignmentState::Succeeded,
            "{case}"
        );
        assert_eq!(
            store.raw_result(&retained)?.unwrap(),
            raw.as_bytes(),
            "{case}"
        );
        assert_eq!(
            retained.findings.len(),
            usize::from(structured && case != "zero"),
            "{case}"
        );
        let extraction = store.result_extraction(&run.assignment_id).await?.unwrap();
        assert_eq!(extraction.raw_digest, sha256_hex(raw.as_bytes()));
        assert_eq!(
            extraction.status,
            if normalised {
                Status::DeterministicallyNormalised
            } else if structured {
                Status::ValidatedStructured
            } else {
                Status::UnstructuredOpinion
            }
        );
        assert_eq!(extraction.validation_error.is_some(), !structured);
        if structured {
            assert_eq!(extraction.parse_digest, sha256_hex(json.as_bytes()));
        }
        if case == "bom_fence" {
            assert_eq!(
                extraction.transformations,
                vec![Repair::Utf8Bom, Repair::MarkdownJsonFence]
            );
        }
        if !structured {
            let summary: Option<String> =
                sqlx::query_scalar("SELECT summary FROM reviewer_assignments WHERE id=?")
                    .bind(&run.assignment_id)
                    .fetch_one(&store.pool_for_tests())
                    .await?;
            assert!(
                summary.is_none(),
                "unstructured output cannot invent a summary"
            );
        }
        let frozen = store.freeze_report(&run.run_id).await?;
        assert_eq!(frozen.outcome, "success");
        assert_eq!(frozen.reviewers[0].completed.is_some(), structured);
        assert_eq!(
            frozen.reviewers[0]
                .opinion
                .as_ref()
                .unwrap()
                .text
                .as_deref(),
            (!structured).then_some(raw.as_str())
        );
        let expected = crate::audex::report::collection::render(&frozen)?;
        let text = String::from_utf8(expected.bytes.clone())?;
        if !structured {
            assert!(text.contains("completed opinion; structured findings unavailable"));
            assert!(!text.contains("No findings were reported."));
            assert!(!text.contains("## Summary"));
        }
        if matches!(case, "plain" | "structured_markup") {
            assert!(text.contains("    <script>alert(1)</script>"));
            assert!(text.contains("    [link](https://invalid.example)"));
            assert!(!text.contains('\u{1b}'));
        }
        let reopened = AuditStore::open(root.clone()).await?;
        assert_eq!(
            reopened.result_extraction(&run.assignment_id).await?,
            Some(extraction)
        );
        let published = reopened.publish_report(&run.run_id, 0).await?;
        let report = match published {
            ReportPublishOutcome::Published { report, .. } => report,
            _ => panic!("new report"),
        };
        assert_eq!(reopened.read_report(&report)?, expected.bytes);
        assert_eq!(
            reopened
                .raw_result(&reopened.load_run(&run.run_id).await?)?
                .unwrap(),
            raw.as_bytes()
        );
        assert!(matches!(
            reopened
                .commit_seeded_result(&run.operation_id, 0, &snapshot, raw.as_bytes())
                .await?,
            ResultCommitOutcome::AlreadyCommitted { .. }
        ));
        reopened.close().await;
    }
    store.close().await;
    Ok(())
}

#[tokio::test]
async fn unaltered_live_fence_passes_semantic_validation_in_a_new_store() -> anyhow::Result<()> {
    const RAW: &[u8] = include_bytes!("../report/fixtures/live-fenced-result.txt");
    const ASSIGNMENT: &str = "audex-assignment-01a1a897-e3f9-7a20-812d-1d1009905a9b";
    assert_eq!(
        sha256_hex(RAW),
        "acdf90e9fe6ba06f06ab7fb6efbd75b948313b09940e5287ed725e20d62a3605"
    );
    let temp = tempfile::tempdir()?;
    let snapshot = SnapshotStore::new(
        absolute(&temp.path().join("snapshots")),
        Arc::new(LocalFileSystem::unsandboxed()),
        CaptureLimits::default(),
    )
    .capture(CaptureRequest {
        source: CaptureSource::Snippet {
            logical_path: SnapshotPath::new("example.rs")?,
            bytes: b"pub fn identity(value: u8) -> u8 { value }\n".to_vec(),
        },
        context_documents: ContextDocuments::default(),
    })
    .await?;
    assert_eq!(
        snapshot.snapshot_id().as_str(),
        "audex-source-v1:sha256:e525d1acaa156833264a4d2db49ca69014eaa493eed874be0b1123dd20944fe1"
    );
    assert!(parse_and_validate(RAW, ASSIGNMENT, snapshot.manifest()).is_err());
    let extracted = crate::audex::result::extract(RAW, ASSIGNMENT, snapshot.manifest())?;
    assert_eq!(
        extracted.metadata.status,
        Status::DeterministicallyNormalised
    );
    assert!(extracted.structured.unwrap().response.findings.is_empty());
    let store = AuditStore::open(absolute(&temp.path().join("audex"))).await?;
    let run = create_accepted_run(&store, &snapshot, "live-output-offline-fixture").await;
    // Assign the original public identifier to this NEW isolated seeded fixture.
    // No historical database, executable, profile or live operation is opened.
    let pool = store.pool_for_tests();
    let mut tx = pool.begin().await?;
    sqlx::query("PRAGMA defer_foreign_keys=ON")
        .execute(&mut *tx)
        .await?;
    for query in [
        "UPDATE reviewer_assignments SET id=? WHERE id=?",
        "UPDATE operations SET assignment_id=? WHERE assignment_id=?",
        "UPDATE audit_attempts SET assignment_id=? WHERE assignment_id=?",
        "UPDATE audit_reviewers SET current_assignment_id=? WHERE current_assignment_id=?",
        "UPDATE lifecycle_observations SET assignment_id=? WHERE assignment_id=?",
    ] {
        sqlx::query(query)
            .bind(ASSIGNMENT)
            .bind(&run.assignment_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    assert!(matches!(
        store
            .commit_seeded_result(&run.operation_id, 3, &snapshot, RAW)
            .await?,
        ResultCommitOutcome::Committed { .. }
    ));
    let record = store.load_run(&run.run_id).await?;
    assert_eq!(record.assignment_id, ASSIGNMENT);
    assert_eq!(store.raw_result(&record)?.unwrap(), RAW);
    let frozen = store.freeze_report(&run.run_id).await?;
    assert!(
        frozen.reviewers[0]
            .completed
            .as_ref()
            .unwrap()
            .findings
            .is_empty()
    );
    assert_eq!(
        frozen.reviewers[0]
            .opinion
            .as_ref()
            .unwrap()
            .extraction
            .transformations,
        vec![Repair::MarkdownJsonFence]
    );
    store.publish_report(&run.run_id, 0).await?;
    assert_eq!(
        store.load_run(&run.run_id).await?.phase,
        RunPhase::Completed
    );
    Ok(())
}

#[tokio::test]
async fn empty_or_unrepresentable_output_does_not_become_an_opinion() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex"))).await?;
    for (index, raw) in [
        Vec::new(),
        b" \n\t".to_vec(),
        vec![0xff],
        vec![b'x'; 1024 * 1024 + 1],
    ]
    .into_iter()
    .enumerate()
    {
        let run = create_accepted_run(&store, &snapshot, &format!("empty-{index}")).await;
        assert!(matches!(
            store
                .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
                .await?,
            ResultCommitOutcome::Rejected { .. }
        ));
        assert_eq!(
            store.load_run(&run.run_id).await?.assignment_state,
            AssignmentState::Failed
        );
        assert!(store.result_extraction(&run.assignment_id).await?.is_none());
    }
    Ok(())
}
