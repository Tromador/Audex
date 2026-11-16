use super::*;
use crate::audex::snapshot::CaptureFile;
use crate::audex::snapshot::manifest::SourceFormV1;
use pretty_assertions::assert_eq;

fn files(root: &Path, names: &[&str]) -> CaptureRequest {
    CaptureRequest {
        source: CaptureSource::FileList {
            files: names
                .iter()
                .map(|name| CaptureFile {
                    source_path: absolute(&root.join(name)),
                    logical_path: SnapshotPath::new(*name).unwrap(),
                })
                .collect(),
        },
        context_documents: ContextDocuments::default(),
    }
}

fn inputs(temp: &tempfile::TempDir) {
    for (path, bytes) in [
        ("one/shared.rs", "needle one\n"),
        ("two/shared.rs", "needle two\n"),
        ("main.rs", "needle main\n"),
    ] {
        let path = temp.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

#[tokio::test]
async fn complete_capture_is_sorted_immutable_and_order_independent() {
    let temp = tempfile::tempdir().unwrap();
    inputs(&temp);
    let store = local_store(&temp);
    let names = ["two/shared.rs", "main.rs", "one/shared.rs"];
    let preview = store.preview(files(temp.path(), &names)).await.unwrap();
    assert!(!store.root().exists());
    let snapshot = store.capture(files(temp.path(), &names)).await.unwrap();
    assert_eq!(*snapshot.manifest(), preview);
    let reverse = store
        .capture(files(
            temp.path(),
            &["one/shared.rs", "main.rs", "two/shared.rs"],
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.snapshot_id(), reverse.snapshot_id());
    assert_eq!(
        snapshot.manifest().identity.source_form,
        SourceFormV1::FileList
    );
    let access = snapshot.access(AccessLimits::default());
    assert_eq!(
        access
            .list()
            .await
            .unwrap()
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        ["main.rs", "one/shared.rs", "two/shared.rs"]
    );
    for (name, expected) in [
        ("one/shared.rs", "needle one\n"),
        ("two/shared.rs", "needle two\n"),
        ("main.rs", "needle main\n"),
    ] {
        std::fs::write(temp.path().join(name), "live changed").unwrap();
        let read = access
            .read(&SnapshotPath::new(name).unwrap(), 0, 100)
            .await
            .unwrap();
        assert_eq!(read.bytes, expected.as_bytes());
        assert!(
            std::fs::metadata(snapshot.source_root().join(name))
                .unwrap()
                .permissions()
                .readonly()
        );
    }
    let hits = access
        .search(SearchRequest {
            query: b"needle".to_vec(),
            path: None,
            max_results: 10,
        })
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        ["main.rs", "one/shared.rs", "two/shared.rs"]
    );
    store.open(snapshot.snapshot_id()).await.unwrap();
    make_tree_writable(temp.path());
}

#[tokio::test]
async fn invalid_or_incomplete_selections_publish_nothing() {
    let temp = tempfile::tempdir().unwrap();
    inputs(&temp);
    let store = local_store(&temp);
    for names in [
        vec![],
        vec!["main.rs", "main.rs"],
        vec!["main.rs", "missing.rs"],
    ] {
        assert!(store.capture(files(temp.path(), &names)).await.is_err());
        assert_eq!(published_directories(store.root().as_path()), 0);
    }
    // Individually valid logical names can still conflict as file and directory.
    let mut request = files(temp.path(), &["main.rs", "one/shared.rs"]);
    let CaptureSource::FileList { files } = &mut request.source else {
        unreachable!()
    };
    files[1].logical_path = SnapshotPath::new("main.rs/child.rs").unwrap();
    assert!(store.capture(request).await.is_err());
    assert_eq!(published_directories(store.root().as_path()), 0);
    assert!(
        local_store(&temp)
            .with_publication_failure()
            .capture(self::files(temp.path(), &["main.rs", "one/shared.rs"]))
            .await
            .is_err()
    );
    assert_eq!(published_directories(store.root().as_path()), 0);
    assert_eq!(
        std::fs::read_dir(store.root().join(".staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn changing_an_earlier_file_while_reading_a_later_one_rejects_the_set() {
    let temp = tempfile::tempdir().unwrap();
    inputs(&temp);
    let calls = Arc::new(AtomicUsize::new(0));
    let path = temp.path().join("main.rs");
    let store = local_store(&temp).with_after_first_file_read(Arc::new(move || {
        if calls.fetch_add(1, Ordering::SeqCst) == 1 {
            std::fs::write(&path, "changed").unwrap();
        }
    }));
    assert!(matches!(
        store
            .capture(files(temp.path(), &["main.rs", "one/shared.rs"]))
            .await,
        Err(SnapshotError::SourceChanged)
    ));
    assert_eq!(published_directories(store.root().as_path()), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn identical_bytes_share_objects_but_preserve_each_paths_executable_mode() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    for (name, mode) in [("a.sh", 0o755), ("b.sh", 0o644)] {
        let path = temp.path().join(name);
        std::fs::write(&path, "same bytes\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let snapshot = local_store(&temp)
        .capture(files(temp.path(), &["b.sh", "a.sh"]))
        .await
        .unwrap();
    let entries = &snapshot.manifest().identity.entries;
    assert_eq!(entries[0].sha256, entries[1].sha256);
    assert!(entries[0].executable && !entries[1].executable);
    assert_eq!(
        std::fs::metadata(snapshot.source_root().join("a.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o555
    );
    assert_eq!(
        std::fs::metadata(snapshot.source_root().join("b.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
    make_tree_writable(temp.path());
}
