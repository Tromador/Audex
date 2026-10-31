use super::result::AuditFindingV1;
mod evidence_lines;
use super::result::AuditResultV1;
use super::result::FindingConfidence;
use super::result::FindingEvidenceV1;
use super::result::FindingSeverity;
use super::result::ResultValidationError;
use super::result::parse_and_validate;
use super::snapshot::CaptureLimits;
use super::snapshot::CaptureRequest;
use super::snapshot::CaptureSource;
use super::snapshot::ContextDocuments;
use super::snapshot::SnapshotHandle;
use super::snapshot::SnapshotPath;
use super::snapshot::SnapshotStore;
use super::snapshot::manifest::sha256_hex;
use super::store::AssignmentState;
use super::store::AuditStore;
use super::store::AuditStoreError;
use super::store::ClaimOutcome;
use super::store::CreateRunRequest;
use super::store::ObservationOutcome;
use super::store::OperationState;
use super::store::ReportPublishOutcome;
use super::store::ResultCommitOutcome;
use super::store::RunPhase;
use codex_exec_server::LocalFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("test path should be absolute")
}

async fn capture_snapshot(temp: &tempfile::TempDir) -> SnapshotHandle {
    SnapshotStore::new(
        absolute(&temp.path().join("snapshots")),
        Arc::new(LocalFileSystem::unsandboxed()),
        CaptureLimits::default(),
    )
    .capture(CaptureRequest {
        source: CaptureSource::Snippet {
            logical_path: SnapshotPath::new("src/lib.rs").expect("valid logical path"),
            bytes: b"fn main() {}\n".to_vec(),
        },
        context_documents: ContextDocuments::default(),
    })
    .await
    .expect("capture snapshot")
}

fn digest(label: &str) -> String {
    sha256_hex(label.as_bytes())
}

fn create_request(snapshot: &SnapshotHandle, request_key: &str) -> CreateRunRequest {
    let manifest_bytes = snapshot
        .manifest()
        .to_pretty_bytes()
        .expect("serialize manifest");
    CreateRunRequest {
        request_key: request_key.to_string(),
        snapshot_id: snapshot.snapshot_id().to_string(),
        manifest_digest: sha256_hex(&manifest_bytes),
        manifest_path: format!(
            "snapshots/{}/manifest.json",
            snapshot.snapshot_id().digest()
        ),
        brief_digest: digest("brief"),
        policy_digest: digest("policy"),
        input_digest: digest("input"),
        provider: "seeded-test-provider".to_string(),
        model: "seeded-test-model".to_string(),
        reasoning_effort: Some("high".to_string()),
        reviewer_input_digest: digest("reviewer-input"),
        operation_input_digest: digest("operation-input"),
    }
}

fn valid_result(snapshot: &SnapshotHandle, assignment_id: &str) -> AuditResultV1 {
    AuditResultV1 {
        schema_version: 1,
        snapshot_id: snapshot.snapshot_id().to_string(),
        assignment_id: assignment_id.to_string(),
        summary: "One concrete issue was identified.".to_string(),
        findings: vec![AuditFindingV1 {
            severity: FindingSeverity::High,
            confidence: FindingConfidence::High,
            title: "Unchecked operation".to_string(),
            mechanism: "The operation accepts an unchecked value.".to_string(),
            impact: "Invalid state can be persisted.".to_string(),
            evidence: vec![FindingEvidenceV1 {
                description: "The unchecked call begins here.".to_string(),
                path: "src/lib.rs".to_string(),
                start_byte: 0,
                end_byte: 2,
                start_line: Some(1),
                end_line: Some(1),
            }],
            remediation: Some("Validate the value before use.".to_string()),
        }],
    }
}

async fn create_accepted_run(
    store: &AuditStore,
    snapshot: &SnapshotHandle,
    request_key: &str,
) -> super::store::CreatedRun {
    let created = store
        .create_run(&create_request(snapshot, request_key))
        .await
        .expect("create run");
    let revision = store
        .bind_snapshot(&created.run_id, created.revision)
        .await
        .expect("bind snapshot");
    assert_eq!(
        store
            .claim_operation(&created.operation_id, "test-owner", 100, 1_000)
            .await
            .expect("claim operation"),
        ClaimOutcome::Acquired
    );
    let revision = store
        .record_dispatch(&created.operation_id, "test-owner", revision, 200)
        .await
        .expect("record dispatch");
    let accepted = store
        .record_acceptance(
            &created.operation_id,
            revision,
            &format!("accepted:{}", created.operation_id),
            "seeded acceptance",
        )
        .await
        .expect("record acceptance");
    assert_eq!(
        accepted,
        ObservationOutcome::Applied {
            revision: revision + 1
        }
    );
    created
}

#[tokio::test]
async fn migrations_pragmas_reopen_and_recovery_are_durable() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let root = absolute(&temp.path().join("audex"));
    let store = AuditStore::open(root.clone()).await.expect("open store");
    assert_eq!(
        store.pragma_values().await.expect("read pragmas"),
        ("wal".to_string(), 2, 2, 2)
    );
    assert_eq!(
        store.applied_migrations().await.expect("read migrations"),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
    let created = store
        .create_run(&create_request(&snapshot, "reopen"))
        .await
        .expect("create run");
    store
        .bind_snapshot(&created.run_id, 0)
        .await
        .expect("bind snapshot");
    store.close().await;

    let reopened = AuditStore::open(root).await.expect("reopen store");
    let record = reopened
        .load_run(&created.run_id)
        .await
        .expect("recover run");
    assert_eq!(record.phase, RunPhase::SnapshotBound);
    assert_eq!(record.assignment_state, AssignmentState::Planned);
    assert_eq!(record.operation_state, OperationState::Planned);
    assert_eq!(
        reopened
            .list_non_terminal_runs()
            .await
            .expect("list recoverable runs"),
        vec![created.run_id]
    );
}

#[tokio::test]
async fn create_and_observations_are_idempotent_but_conflicts_fail() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex")))
        .await
        .expect("open store");
    let request = create_request(&snapshot, "idempotent-create");
    let created = store.create_run(&request).await.expect("create run");
    assert_eq!(
        store.create_run(&request).await.expect("repeat create"),
        created
    );
    let mut conflicting = request;
    conflicting.model = "different-model".to_string();
    assert!(matches!(
        store.create_run(&conflicting).await,
        Err(AuditStoreError::IdempotencyConflict(_))
    ));

    let revision = store
        .bind_snapshot(&created.run_id, 0)
        .await
        .expect("bind snapshot");
    assert!(matches!(
        store.bind_snapshot(&created.run_id, 0).await,
        Err(AuditStoreError::RevisionConflict { .. })
    ));
    assert!(matches!(
        store
            .record_dispatch(&created.operation_id, "owner", revision, 20)
            .await,
        Err(AuditStoreError::InvalidTransition(_))
    ));
    store
        .claim_operation(&created.operation_id, "owner", 10, 100)
        .await
        .expect("claim");
    let revision = store
        .record_dispatch(&created.operation_id, "owner", revision, 20)
        .await
        .expect("dispatch");
    let first = store
        .record_acceptance(
            &created.operation_id,
            revision,
            "same-observation",
            "accepted",
        )
        .await
        .expect("first observation");
    assert_eq!(first, ObservationOutcome::Applied { revision: 3 });
    assert_eq!(
        store
            .record_acceptance(
                &created.operation_id,
                revision,
                "same-observation",
                "accepted",
            )
            .await
            .expect("duplicate observation"),
        ObservationOutcome::Duplicate { revision: 3 }
    );
    assert!(matches!(
        store
            .record_acceptance(
                &created.operation_id,
                3,
                "same-observation",
                "different evidence",
            )
            .await,
        Err(AuditStoreError::IdempotencyConflict(_))
    ));
}

#[tokio::test]
async fn competing_leases_have_one_winner() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let root = absolute(&temp.path().join("audex"));
    let first = AuditStore::open(root.clone())
        .await
        .expect("open first store");
    let created = first
        .create_run(&create_request(&snapshot, "lease"))
        .await
        .expect("create run");
    first
        .bind_snapshot(&created.run_id, 0)
        .await
        .expect("bind snapshot");
    let second = AuditStore::open(root).await.expect("open second store");
    let (left, right) = tokio::join!(
        first.claim_operation(&created.operation_id, "left", 100, 1_000),
        second.claim_operation(&created.operation_id, "right", 100, 1_000)
    );
    let outcomes = [left.expect("left claim"), right.expect("right claim")];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ClaimOutcome::Acquired)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ClaimOutcome::Busy)
            .count(),
        1
    );
}

#[tokio::test]
async fn cancellation_prevents_claim_and_dispatch_even_with_an_existing_lease() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex")))
        .await
        .expect("open store");
    let created = store
        .create_run(&create_request(&snapshot, "cancel-before-dispatch"))
        .await
        .expect("create run");
    let revision = store
        .bind_snapshot(&created.run_id, 0)
        .await
        .expect("bind snapshot");
    assert_eq!(
        store
            .claim_operation(&created.operation_id, "owner", 100, 1_000)
            .await
            .expect("claim operation"),
        ClaimOutcome::Acquired
    );
    let revision = store
        .request_cancel(&created.run_id, revision)
        .await
        .expect("request cancellation");
    assert_eq!(revision, 2);
    assert_eq!(
        store
            .claim_operation(&created.operation_id, "owner", 200, 1_000)
            .await
            .expect("claim cancelled operation"),
        ClaimOutcome::CancellationRequested
    );
    assert!(matches!(
        store
            .record_dispatch(
                &created.operation_id,
                "owner",
                store
                    .load_run_metadata(&created.run_id)
                    .await
                    .expect("attempt revision")
                    .attempt_revision,
                200
            )
            .await,
        Err(AuditStoreError::InvalidTransition(_))
    ));
    let recovered = store
        .load_run(&created.run_id)
        .await
        .expect("load cancelled run");
    assert_eq!(recovered.control_intent, "cancel_requested");
    assert_eq!(recovered.phase, RunPhase::SnapshotBound);
    assert_eq!(recovered.operation_state, OperationState::Planned);
}

#[tokio::test]
async fn structured_results_reject_unknown_and_semantically_invalid_data() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let assignment_id = "audex-assignment-test";
    let valid = valid_result(&snapshot, assignment_id);
    let valid_bytes = serde_json::to_vec(&valid).expect("serialize valid result");
    assert!(parse_and_validate(&valid_bytes, assignment_id, snapshot.manifest()).is_ok());
    let mut unknown = serde_json::to_value(&valid).expect("serialize result value");
    unknown
        .as_object_mut()
        .expect("object result")
        .insert("unexpected".to_string(), serde_json::Value::Bool(true));
    assert!(matches!(
        parse_and_validate(
            &serde_json::to_vec(&unknown).expect("serialize unknown field"),
            assignment_id,
            snapshot.manifest(),
        ),
        Err(ResultValidationError::InvalidJson(_))
    ));
    let mut invalid_path = valid.clone();
    invalid_path.findings[0].evidence[0].path = "../escape.rs".to_string();
    assert!(matches!(
        parse_and_validate(
            &serde_json::to_vec(&invalid_path).expect("serialize invalid path"),
            assignment_id,
            snapshot.manifest(),
        ),
        Err(ResultValidationError::Semantic(_))
    ));
    let mut invalid_range = valid;
    invalid_range.findings[0].evidence[0].end_byte = 10_000;
    assert!(matches!(
        parse_and_validate(
            &serde_json::to_vec(&invalid_range).expect("serialize invalid range"),
            assignment_id,
            snapshot.manifest(),
        ),
        Err(ResultValidationError::Semantic(_))
    ));
    let schema = schemars::schema_for!(AuditResultV1);
    assert!(matches!(
        schema
            .schema
            .object
            .as_ref()
            .expect("result object schema")
            .additional_properties
            .as_deref(),
        Some(schemars::schema::Schema::Bool(false))
    ));
}

#[tokio::test]
async fn seeded_findings_persist_and_report_is_reproducible_after_reopen() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let root = absolute(&temp.path().join("audex"));
    let store = AuditStore::open(root.clone()).await.expect("open store");
    let created = create_accepted_run(&store, &snapshot, "valid-result").await;
    let raw = serde_json::to_vec(&valid_result(&snapshot, &created.assignment_id))
        .expect("serialize result");
    let committed = store
        .commit_seeded_result(&created.operation_id, 3, &snapshot, &raw)
        .await
        .expect("commit result");
    let ResultCommitOutcome::Committed {
        revision,
        digest,
        relative_path,
    } = committed
    else {
        panic!("expected committed result");
    };
    assert_eq!(revision, 4);
    assert_eq!(digest, sha256_hex(&raw));
    assert!(temp.path().join("audex").join(relative_path).is_file());
    assert!(matches!(
        store
            .commit_seeded_result(&created.operation_id, 3, &snapshot, &raw)
            .await
            .expect("repeat result commit"),
        ResultCommitOutcome::AlreadyCommitted { revision: 4, .. }
    ));
    let published = store
        .publish_report(
            &created.run_id,
            store
                .load_run_metadata(&created.run_id)
                .await
                .expect("run revision")
                .revision,
        )
        .await
        .expect("publish report");
    let ReportPublishOutcome::Published { revision, report } = published else {
        panic!("expected published report");
    };
    assert_eq!(revision, 4);
    let report_bytes = store.read_report(&report).expect("read report");
    assert_eq!(report.content_digest, sha256_hex(&report_bytes));
    assert!(
        String::from_utf8(report_bytes.clone())
            .expect("UTF-8 report")
            .contains("Unchecked operation")
    );
    let loaded = store.load_run(&created.run_id).await.expect("load run");
    assert_eq!(loaded.phase, RunPhase::Completed);
    assert_eq!(loaded.findings.len(), 1);
    assert_eq!(
        loaded.findings[0].verification_state,
        "unverified_model_claim"
    );
    store.close().await;

    let reopened = AuditStore::open(root).await.expect("reopen store");
    let loaded = reopened
        .load_run(&created.run_id)
        .await
        .expect("reload run");
    assert_eq!(loaded.report.as_ref(), Some(&report));
    assert_eq!(
        reopened
            .read_report(loaded.report.as_ref().expect("persisted report"))
            .expect("read reopened report"),
        report_bytes
    );
    assert_eq!(
        reopened
            .publish_report(&created.run_id, 4)
            .await
            .expect("repeat publication"),
        ReportPublishOutcome::AlreadyPublished {
            revision: 4,
            report
        }
    );
}

#[tokio::test]
async fn empty_findings_succeed_and_invalid_seeded_results_fail_terminally() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let snapshot = capture_snapshot(&temp).await;
    let store = AuditStore::open(absolute(&temp.path().join("audex")))
        .await
        .expect("open store");
    let empty = create_accepted_run(&store, &snapshot, "empty-result").await;
    let raw = serde_json::to_vec(&AuditResultV1 {
        schema_version: 1,
        snapshot_id: snapshot.snapshot_id().to_string(),
        assignment_id: empty.assignment_id.clone(),
        summary: "No findings.".to_string(),
        findings: Vec::new(),
    })
    .expect("serialize empty result");
    assert!(matches!(
        store
            .commit_seeded_result(&empty.operation_id, 3, &snapshot, &raw)
            .await
            .expect("commit empty result"),
        ResultCommitOutcome::Committed { revision: 4, .. }
    ));
    assert!(matches!(
        store
            .publish_report(&empty.run_id, 3)
            .await
            .expect("publish empty report"),
        ReportPublishOutcome::Published { revision: 4, .. }
    ));

    let invalid = create_accepted_run(&store, &snapshot, "invalid-result").await;
    let raw = br#"{"schema_version":1,"not":"the required result"}"#;
    assert!(matches!(
        store
            .commit_seeded_result(&invalid.operation_id, 3, &snapshot, raw)
            .await
            .expect("record invalid result"),
        ResultCommitOutcome::Rejected { revision: 4, .. }
    ));
    let loaded = store
        .load_run(&invalid.run_id)
        .await
        .expect("load failed run");
    assert_eq!(loaded.phase, RunPhase::CompletedWithFailures);
    assert_eq!(loaded.assignment_state, AssignmentState::Failed);
    assert_eq!(loaded.operation_state, OperationState::Failed);
    assert!(loaded.findings.is_empty());
}

#[tokio::test]
async fn corrupt_database_fails_closed_without_recreation() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let root = absolute(&temp.path().join("audex"));
    let store = AuditStore::open(root.clone()).await.expect("open store");
    let database_path = store.database_path().to_path_buf();
    store.close().await;
    let corrupt = b"this is not a SQLite database";
    std::fs::write(&database_path, corrupt).expect("corrupt database");

    assert!(AuditStore::open(root).await.is_err());
    assert_eq!(
        std::fs::read(database_path).expect("read corrupt database"),
        corrupt
    );
}
