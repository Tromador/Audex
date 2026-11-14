use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn migration_from_sixteen_does_not_reclassify_historical_results_or_reports()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let snapshot = capture_snapshot(&temp).await;
    let root = absolute(&temp.path().join("audex"));
    let store = AuditStore::open(root.clone()).await?;
    let run = create_accepted_run(&store, &snapshot, "historical").await;
    let raw = serde_json::to_vec(&valid_result(&snapshot, &run.assignment_id))?;
    store
        .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
        .await?;
    let mut frozen = store.freeze_report(&run.run_id).await?;
    frozen.format_version = 4;
    frozen.reviewers[0].opinion = None;
    frozen.reviewers[0].source_tools = None;
    let input = serde_json::to_string(&frozen)?;
    let expected = crate::audex::report::collection::render(&frozen)?;
    sqlx::query("UPDATE audit_report_selections SET input_json=?,input_digest=? WHERE id=?")
        .bind(&input)
        .bind(sha256_hex(input.as_bytes()))
        .bind(&frozen.selection_id)
        .execute(&store.pool_for_tests())
        .await?;
    // Reconstruct the pre-upgrade schema ONLY in this owned fixture. The older
    // migration checksums and all original result/report rows remain intact.
    for statement in [
        "ALTER TABLE reviewer_assignments DROP COLUMN result_extraction_json",
        "ALTER TABLE reviewer_assignments DROP COLUMN result_extraction_digest",
        "DELETE FROM _sqlx_migrations WHERE version=17",
    ] {
        sqlx::query(statement)
            .execute(&store.pool_for_tests())
            .await?;
    }
    store.close().await;
    let upgraded = AuditStore::open(root.clone()).await?;
    assert_eq!(upgraded.applied_migrations().await?.last(), Some(&18));
    assert!(
        upgraded
            .result_extraction(&run.assignment_id)
            .await?
            .is_none()
    );
    assert_eq!(
        upgraded
            .raw_result(&upgraded.load_run(&run.run_id).await?)?
            .unwrap(),
        raw
    );
    assert_eq!(
        serde_json::to_string(&upgraded.freeze_report(&run.run_id).await?)?,
        input
    );
    let report = match upgraded.publish_report(&run.run_id, 0).await? {
        ReportPublishOutcome::Published { report, .. } => report,
        _ => panic!("new publication"),
    };
    assert_eq!(report.id, expected.id);
    assert_eq!(upgraded.read_report(&report)?, expected.bytes);
    upgraded.close().await;
    let reopened = AuditStore::open(root).await?;
    assert_eq!(reopened.read_report(&report)?, expected.bytes);
    assert!(
        reopened
            .result_extraction(&run.assignment_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn full_bound_is_retained_and_corrupt_extraction_metadata_refuses_freeze()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex"))).await?;
    let run = create_accepted_run(&store, &snapshot, "bounded-opinion").await;
    let raw = vec![b'x'; crate::audex::result::MAX_RAW_RESULT_BYTES];
    store
        .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
        .await?;
    assert_eq!(
        store
            .raw_result(&store.load_run(&run.run_id).await?)?
            .unwrap(),
        raw
    );
    let extraction = store.result_extraction(&run.assignment_id).await?.unwrap();
    assert_eq!(extraction.status, Status::UnstructuredOpinion);
    sqlx::query("UPDATE reviewer_assignments SET result_extraction_digest='corrupt' WHERE id=?")
        .bind(&run.assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
    assert!(store.result_extraction(&run.assignment_id).await.is_err());
    assert!(store.freeze_report(&run.run_id).await.is_err());
    assert_eq!(
        store
            .raw_result(&store.load_run(&run.run_id).await?)?
            .unwrap(),
        raw
    );
    let json = serde_json::to_string(&extraction)?;
    sqlx::query("UPDATE reviewer_assignments SET result_extraction_digest=? WHERE id=?")
        .bind(sha256_hex(json.as_bytes()))
        .bind(&run.assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
    let frozen = store.freeze_report(&run.run_id).await?;
    assert_eq!(
        frozen.reviewers[0]
            .opinion
            .as_ref()
            .unwrap()
            .text
            .as_ref()
            .unwrap()
            .as_bytes(),
        raw
    );
    assert_eq!(frozen.outcome, "success");
    Ok(())
}
