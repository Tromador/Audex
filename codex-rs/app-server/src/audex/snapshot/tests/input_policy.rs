//! New admission policy and compatibility with frozen version 1 identity bytes.
use super::*;
use crate::audex::snapshot::manifest::SnapshotManifestV1;
use pretty_assertions::assert_eq;

const LEGACY_MANIFEST: &[u8] = include_bytes!("fixtures/capture-policy-v1.json");

fn legacy_manifest() -> SnapshotManifestV1 {
    serde_json::from_slice(LEGACY_MANIFEST).expect("frozen version 1 manifest")
}

#[tokio::test]
async fn both_context_roles_cross_the_former_boundary_without_loss() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let store = local_store(&temp);
    for size in [65_536, 65_537] {
        let guidance = format!("{}{}", "é".repeat(32_768), "A".repeat(size - 65_536));
        let evidence = format!("{}{}", "ø".repeat(32_768), "G".repeat(size - 65_536));
        let mut request = snippet("src/lib.rs", b"fn main() {}\n");
        request.context_documents = ContextDocuments {
            audex_guidance: Some(guidance.as_bytes().to_vec()),
            agents_evidence: Some(evidence.as_bytes().to_vec()),
        };
        let captured = store
            .capture(request)
            .await
            .expect("capture complete inputs");
        let reopened = store.open(captured.snapshot_id()).await.expect("reopen");
        let policy = reopened.manifest().identity.capture_policy;
        assert_eq!(policy.version, 2);
        assert_eq!(policy.max_source_bytes, 32 * 1024 * 1024);
        assert_eq!(policy.max_context_document_bytes, None);
        let documents = reopened
            .context_documents()
            .await
            .expect("verified documents");
        assert_eq!(documents.len(), 2);
        for ((path, _, bytes), (expected_path, expected)) in documents.iter().zip([
            ("AUDEX.md", guidance.as_bytes()),
            ("AGENTS.md", evidence.as_bytes()),
        ]) {
            assert_eq!(path, expected_path);
            assert_eq!(bytes.len(), size);
            assert_eq!(bytes, expected);
        }
    }
    make_tree_writable(temp.path());
}

#[tokio::test]
async fn legacy_snapshot_reopens_without_rewriting_identity_or_document_bytes() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let store = local_store(&temp);
    let mut request = snippet("src/lib.rs", b"fn main() {}\n");
    request.context_documents = ContextDocuments {
        audex_guidance: Some(vec![b'A'; 65_536]),
        agents_evidence: Some(vec![b'G'; 65_536]),
    };
    let captured = store.capture(request).await.expect("create sealed objects");
    let legacy = legacy_manifest();
    legacy.validate().expect("original canonical identity");
    assert_eq!(captured.content_id(), &legacy.content_id);
    assert_ne!(captured.snapshot_id(), &legacy.snapshot_id);
    let current_root = store.root().join(captured.snapshot_id().digest());
    let legacy_root = store.root().join(legacy.snapshot_id.digest());
    let manifest_path = current_root.join("manifest.json");
    make_file_writable(manifest_path.as_path());
    std::fs::write(manifest_path.as_path(), LEGACY_MANIFEST).expect("install frozen manifest");
    let mut permissions = std::fs::metadata(manifest_path.as_path())
        .unwrap()
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(manifest_path.as_path(), permissions).unwrap();
    std::fs::rename(current_root.as_path(), legacy_root.as_path())
        .expect("legacy identity directory");

    let reopened = store
        .open(&legacy.snapshot_id)
        .await
        .expect("open version 1");
    assert_eq!(reopened.manifest(), &legacy);
    let documents = reopened
        .context_documents()
        .await
        .expect("legacy documents");
    assert_eq!(documents[0].2, vec![b'A'; 65_536]);
    assert_eq!(documents[1].2, vec![b'G'; 65_536]);
    assert_eq!(
        std::fs::read(legacy_root.join("manifest.json").as_path()).unwrap(),
        LEGACY_MANIFEST
    );

    let object = legacy_root
        .join("objects")
        .join("sha256")
        .join(&documents[0].1);
    make_file_writable(object.as_path());
    std::fs::write(object.as_path(), vec![b'X'; 65_536]).expect("tamper same-size context object");
    assert!(matches!(
        store.open(&legacy.snapshot_id).await,
        Err(SnapshotError::Integrity(_))
    ));
    assert!(matches!(
        reopened.context_documents().await,
        Err(SnapshotError::Integrity(_))
    ));
    make_tree_writable(temp.path());
}

#[test]
fn legacy_golden_and_recorded_limits_remain_authoritative() {
    let mut legacy = legacy_manifest();
    legacy.identity.context_documents.clear();
    legacy.snapshot_id = SnapshotId::parse(
        "audex-source-v1:sha256:b9b7897e67cb61dc1963e9c23b13bdaaf7a836251a0f010d457de39f7ddfc81a"
            .to_owned(),
    )
    .expect("original golden ID");
    legacy.validate().expect("unchanged pre-M6 golden identity");

    for index in 0..2 {
        let mut oversized = legacy_manifest();
        oversized.identity.context_documents[index].size = 65_537;
        assert!(matches!(
            oversized.validate(),
            Err(SnapshotError::InvalidManifest(_))
        ));
    }
    let mut oversized_source = legacy_manifest();
    oversized_source.identity.entries[0].size = 32 * 1024 * 1024 + 1;
    assert!(matches!(
        oversized_source.validate(),
        Err(SnapshotError::InvalidManifest(_))
    ));
}

#[test]
fn malformed_policies_and_mismatched_identities_are_rejected() {
    for (version, limit) in [(1, None), (2, Some(65_536)), (3, None)] {
        let mut manifest = legacy_manifest();
        manifest.identity.capture_policy.version = version;
        manifest.identity.capture_policy.max_context_document_bytes = limit;
        assert!(matches!(
            manifest.validate(),
            Err(SnapshotError::InvalidManifest(_))
        ));
    }
    let mut changed_policy = legacy_manifest();
    changed_policy.identity.capture_policy.version = 2;
    changed_policy
        .identity
        .capture_policy
        .max_context_document_bytes = None;
    assert!(matches!(
        changed_policy.validate(),
        Err(SnapshotError::Integrity(_))
    ));
    let mut missing_limit: serde_json::Value = serde_json::from_slice(LEGACY_MANIFEST).unwrap();
    missing_limit["identity"]["capture_policy"]
        .as_object_mut()
        .unwrap()
        .remove("max_context_document_bytes");
    let malformed: SnapshotManifestV1 = serde_json::from_value(missing_limit).unwrap();
    assert!(matches!(
        malformed.validate(),
        Err(SnapshotError::InvalidManifest(_))
    ));
    let mut unknown_field: serde_json::Value = serde_json::from_slice(LEGACY_MANIFEST).unwrap();
    unknown_field["identity"]["capture_policy"]["unrecognised"] = true.into();
    assert!(serde_json::from_value::<SnapshotManifestV1>(unknown_field).is_err());
}
