use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn lines_are_derived_without_discarding_findings_or_changing_raw_response() {
    for source in [
        b"a\r\n\xce\xb2\nlast".as_slice(),
        b"a\n\xce\xb2\nlast\n".as_slice(),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let snapshot = SnapshotStore::new(
            absolute(&temp.path().join("snapshots")),
            Arc::new(LocalFileSystem::unsandboxed()),
            CaptureLimits::default(),
        )
        .capture(CaptureRequest {
            source: CaptureSource::Snippet {
                logical_path: SnapshotPath::new("src/lib.rs").expect("path"),
                bytes: source.to_vec(),
            },
            context_documents: ContextDocuments::default(),
        })
        .await
        .expect("snapshot");
        let store = AuditStore::open(absolute(&temp.path().join("audex")))
            .await
            .expect("store");
        let run = create_accepted_run(&store, &snapshot, "line-check").await;
        let mut result = valid_result(&snapshot, &run.assignment_id);
        let template = result.findings[0].clone();
        result.findings.clear();
        // Wrong, absent, partial, zero and reversed numeric claims must not
        // invalidate unrelated findings. Include newline bytes, UTF-8 byte
        // interiors, the final byte and half-open ranges ending exactly at EOF.
        for (index, (start, end, first, last)) in [
            (0, 1, Some(99), Some(99)),
            (2, 3, Some(500), Some(500)),
            (1, source.len() as u64, None, None),
            (3, 4, Some(0), None),
            (0, source.len() as u64, Some(40), Some(1)),
            (source.len() as u64 - 1, source.len() as u64, None, Some(8)),
        ]
        .into_iter()
        .enumerate()
        {
            let mut finding = template.clone();
            finding.title = format!("Finding {index}");
            let evidence = &mut finding.evidence[0];
            evidence.start_byte = start;
            evidence.end_byte = end;
            evidence.start_line = first;
            evidence.end_line = last;
            result.findings.push(finding);
        }
        let mut raw_value = serde_json::to_value(&result).expect("value");
        // Older providers may omit nullable fields altogether.
        let omitted = raw_value["findings"][2]["evidence"][0]
            .as_object_mut()
            .expect("evidence");
        omitted.remove("start_line");
        omitted.remove("end_line");
        let raw = serde_json::to_vec(&raw_value).expect("raw");
        let original_ids = parse_and_validate(&raw, &run.assignment_id, snapshot.manifest())
            .expect("strict byte validation")
            .finding_ids;
        assert!(matches!(
            store
                .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
                .await
                .expect("commit"),
            ResultCommitOutcome::Committed { .. }
        ));
        let retained = store.load_run(&run.run_id).await.expect("retained");
        assert_eq!(
            store
                .raw_result(&retained)
                .expect("verified raw")
                .expect("raw"),
            raw
        );
        assert_eq!(retained.findings.len(), result.findings.len());
        for ((finding, original), id) in retained
            .findings
            .iter()
            .zip(&result.findings)
            .zip(original_ids)
        {
            assert_eq!(finding.id, id);
            let stored = &finding.evidence[0];
            let bytes = &original.evidence[0];
            let line_at = |offset: u64| {
                1 + source[..offset as usize]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count() as i64
            };
            assert_eq!(stored.start_line, Some(line_at(bytes.start_byte)));
            assert_eq!(stored.end_line, Some(line_at(bytes.end_byte - 1)));
            assert!(stored.lines_verified);
        }
        store
            .publish_report(&run.run_id, retained.revision)
            .await
            .expect("report");
        let complete = store.load_run(&run.run_id).await.expect("complete");
        let report = String::from_utf8(
            store
                .read_report(complete.report.as_ref().expect("report"))
                .expect("bytes"),
        )
        .expect("UTF8");
        assert!(report.contains("verified lines") && report.contains("Report format: `2`"));
        assert!(!report.contains("verified lines `99..99`"));
        assert!(matches!(
            store
                .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
                .await
                .expect("idempotent raw retry"),
            ResultCommitOutcome::AlreadyCommitted { .. }
        ));
        assert_eq!(
            store
                .read_report(complete.report.as_ref().expect("report"))
                .expect("same report"),
            report.as_bytes()
        );
    }
}

#[tokio::test]
async fn source_verification_failure_does_not_reject_findings_with_absent_line_claims() {
    let temp = tempfile::tempdir().expect("temp");
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex")))
        .await
        .expect("store");
    let run = create_accepted_run(&store, &snapshot, "line-source-failure").await;
    let mut result = valid_result(&snapshot, &run.assignment_id);
    result.findings[0].evidence[0].start_line = None;
    result.findings[0].evidence[0].end_line = None;
    let raw = serde_json::to_vec(&result).expect("raw");
    let source = snapshot.source_root().join("src/lib.rs");
    let original = std::fs::read(source.as_path()).expect("original");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(source.as_path(), std::fs::Permissions::from_mode(0o644))
            .expect("test-only unseal");
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(source.as_path())
            .expect("metadata")
            .permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(source.as_path(), permissions).expect("test-only unseal");
    }
    std::fs::write(source.as_path(), vec![b'x'; original.len()]).expect("tamper fixture");
    assert!(
        matches!(
            store
                .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
                .await,
            Err(AuditStoreError::Snapshot(_))
        ),
        "source integrity is not a cosmetic reviewer error"
    );
    let waiting = store.load_run(&run.run_id).await.expect("run");
    assert_eq!(waiting.operation_state, OperationState::Accepted);
    assert!(waiting.findings.is_empty() && waiting.result_digest.is_none());
    std::fs::write(source.as_path(), original).expect("restore fixture");
    assert!(matches!(
        store
            .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
            .await
            .expect("recover same result"),
        ResultCommitOutcome::Committed { .. }
    ));
    let recovered = store.load_run(&run.run_id).await.expect("recovered");
    assert_eq!(
        store
            .raw_result(&recovered)
            .expect("raw")
            .expect("retained"),
        raw
    );
}

#[tokio::test]
async fn legacy_line_claims_are_labelled_unverified_in_new_reports() {
    let temp = tempfile::tempdir().expect("temp");
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex")))
        .await
        .expect("store");
    let run = create_accepted_run(&store, &snapshot, "legacy-lines").await;
    let raw = serde_json::to_vec(&valid_result(&snapshot, &run.assignment_id)).expect("raw");
    store
        .commit_seeded_result(&run.operation_id, 3, &snapshot, &raw)
        .await
        .expect("commit");
    sqlx::query("UPDATE evidence SET lines_verified=0")
        .execute(&store.pool_for_tests())
        .await
        .expect("simulate migrated legacy row");
    let run = store.load_run(&run.run_id).await.expect("run");
    store
        .publish_report(&run.run_id, run.revision)
        .await
        .expect("report");
    let run = store.load_run(&run.run_id).await.expect("complete");
    let text = String::from_utf8(
        store
            .read_report(run.report.as_ref().expect("report"))
            .expect("bytes"),
    )
    .expect("UTF8");
    assert!(text.contains("reviewer line claim `1..1` (unverified)"));
    assert!(!text.contains("verified lines"));
}
