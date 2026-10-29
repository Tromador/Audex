use super::AccessLimits;
use super::CaptureLimits;
use super::CaptureRequest;
use super::CaptureSource;
use super::ContextDocuments;
use super::SearchRequest;
use super::SnapshotError;
use super::SnapshotId;
use super::SnapshotPath;
use super::SnapshotStore;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::LocalFileSystem;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::ReadFileOptions;
use codex_exec_server::RemoveOptions;
use codex_exec_server::WalkOptions;
use codex_exec_server::WalkOutcome;
use codex_exec_server::WriteFileOptions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("test path should be absolute")
}

fn local_store(temp: &tempfile::TempDir) -> SnapshotStore {
    SnapshotStore::new(
        absolute(&temp.path().join("snapshots")),
        Arc::new(LocalFileSystem::unsandboxed()),
        CaptureLimits::default(),
    )
}

fn snippet(path: &str, bytes: &[u8]) -> CaptureRequest {
    CaptureRequest {
        source: CaptureSource::Snippet {
            logical_path: SnapshotPath::new(path).expect("valid test path"),
            bytes: bytes.to_vec(),
        },
        context_documents: ContextDocuments::default(),
    }
}

fn file_request(source_path: &Path, logical_path: &str) -> CaptureRequest {
    CaptureRequest {
        source: CaptureSource::File {
            source_path: absolute(source_path),
            logical_path: SnapshotPath::new(logical_path).expect("valid test path"),
        },
        context_documents: ContextDocuments::default(),
    }
}

#[tokio::test]
async fn canonical_identity_has_stable_golden_vectors() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let handle = local_store(&temp)
        .capture(snippet("src/lib.rs", b"fn main() {}\n"))
        .await
        .expect("capture snippet");

    assert_eq!(
        handle.content_id().as_str(),
        "audex-content-v1:sha256:0b7d8ea08b8ec32fdab239b34feffa2e24fd88415879dee13262304a1c8fd02f"
    );
    assert_eq!(
        handle.snapshot_id().as_str(),
        "audex-source-v1:sha256:b9b7897e67cb61dc1963e9c23b13bdaaf7a836251a0f010d457de39f7ddfc81a"
    );
    make_tree_writable(temp.path());
}

#[tokio::test]
async fn identity_changes_only_with_declared_inputs() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let store = local_store(&temp);
    let first = store
        .capture(snippet("src/lib.rs", b"same\n"))
        .await
        .expect("capture first snippet");
    let repeated = store
        .capture(snippet("src/lib.rs", b"same\n"))
        .await
        .expect("capture repeated snippet");
    let changed_path = store
        .capture(snippet("src/main.rs", b"same\n"))
        .await
        .expect("capture changed path");
    let changed_bytes = store
        .capture(snippet("src/lib.rs", b"different\n"))
        .await
        .expect("capture changed bytes");
    let with_context = store
        .capture(CaptureRequest {
            source: CaptureSource::Snippet {
                logical_path: SnapshotPath::new("src/lib.rs").expect("valid path"),
                bytes: b"same\n".to_vec(),
            },
            context_documents: ContextDocuments {
                audex_guidance: Some(b"prioritise parsers\n".to_vec()),
                agents_evidence: None,
            },
        })
        .await
        .expect("capture context document");

    assert_eq!(first.content_id(), repeated.content_id());
    assert_eq!(first.snapshot_id(), repeated.snapshot_id());
    assert_ne!(first.content_id(), changed_path.content_id());
    assert_ne!(first.content_id(), changed_bytes.content_id());
    assert_eq!(first.content_id(), with_context.content_id());
    assert_ne!(first.snapshot_id(), with_context.snapshot_id());
    make_tree_writable(temp.path());
}

#[cfg(unix)]
#[tokio::test]
async fn executable_mode_is_part_of_source_identity() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("create temp directory");
    let source = temp.path().join("script.sh");
    std::fs::write(&source, b"#!/bin/sh\nexit 0\n").expect("write source");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644))
        .expect("set source mode");
    let store = local_store(&temp);
    let plain = store
        .capture(file_request(&source, "script.sh"))
        .await
        .expect("capture plain file");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755))
        .expect("set executable mode");
    let executable = store
        .capture(file_request(&source, "script.sh"))
        .await
        .expect("capture executable file");

    assert_ne!(plain.content_id(), executable.content_id());
    assert!(!plain.manifest().identity.entries[0].executable);
    assert!(executable.manifest().identity.entries[0].executable);
    make_tree_writable(temp.path());
}

#[cfg(unix)]
#[tokio::test]
async fn file_capture_does_not_modify_source_bytes_or_mode() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("create temp directory");
    let source = temp.path().join("source.rs");
    let expected = b"pub fn answer() -> u8 { 42 }\n";
    std::fs::write(&source, expected).expect("write source");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751))
        .expect("set source mode");
    let before_mode = std::fs::metadata(&source)
        .expect("source metadata")
        .permissions()
        .mode();

    local_store(&temp)
        .capture(file_request(&source, "source.rs"))
        .await
        .expect("capture file");

    assert_eq!(std::fs::read(&source).expect("read source"), expected);
    assert_eq!(
        std::fs::metadata(&source)
            .expect("source metadata")
            .permissions()
            .mode(),
        before_mode
    );
    make_tree_writable(temp.path());
}

#[tokio::test]
async fn invalid_paths_and_oversize_inputs_publish_nothing() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let store = SnapshotStore::new(
        absolute(&temp.path().join("snapshots")),
        Arc::new(LocalFileSystem::unsandboxed()),
        CaptureLimits {
            max_source_bytes: 4,
            max_context_document_bytes: 3,
        },
    );
    assert!(SnapshotPath::new("../escape.rs").is_err());
    assert!(SnapshotPath::new("/absolute.rs").is_err());
    assert!(SnapshotPath::new("dir\\file.rs").is_err());
    assert!(matches!(
        store.capture(snippet("file.rs", b"12345")).await,
        Err(SnapshotError::SourceTooLarge { .. })
    ));
    assert!(matches!(
        store
            .capture(CaptureRequest {
                source: CaptureSource::Snippet {
                    logical_path: SnapshotPath::new("file.rs").expect("valid path"),
                    bytes: b"1234".to_vec(),
                },
                context_documents: ContextDocuments {
                    audex_guidance: Some(b"1234".to_vec()),
                    agents_evidence: None,
                },
            })
            .await,
        Err(SnapshotError::ContextDocumentTooLarge { .. })
    ));
    assert_eq!(published_directories(store.root().as_path()), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_and_special_file_sources_are_rejected() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("create temp directory");
    let regular = temp.path().join("regular.rs");
    let link = temp.path().join("link.rs");
    std::fs::write(&regular, b"safe\n").expect("write regular file");
    symlink(&regular, &link).expect("create symlink");
    let directory = temp.path().join("directory");
    std::fs::create_dir(&directory).expect("create directory");
    let fifo = temp.path().join("pipe");
    let fifo_path =
        std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("fifo path has no nul");
    // SAFETY: `fifo_path` is a live, NUL-terminated path and mkfifo does not retain the pointer.
    let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
    assert_eq!(result, 0, "create fifo");
    let store = local_store(&temp);

    for source in [&link, &directory, &fifo] {
        assert!(matches!(
            store.capture(file_request(source, "input.rs")).await,
            Err(SnapshotError::InvalidSource(_))
        ));
    }
    assert_eq!(published_directories(store.root().as_path()), 0);
}

#[tokio::test]
async fn changing_file_fails_capture() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let source = temp.path().join("changing.rs");
    std::fs::write(&source, b"first\n").expect("write source");
    let hook_path = source.clone();
    let store = local_store(&temp).with_after_first_file_read(Arc::new(move || {
        std::fs::write(&hook_path, b"second\n").expect("change source during capture");
    }));

    assert!(matches!(
        store.capture(file_request(&source, "changing.rs")).await,
        Err(SnapshotError::SourceChanged)
    ));
    assert_eq!(published_directories(store.root().as_path()), 0);
}

#[tokio::test]
async fn failure_before_publication_leaves_no_valid_snapshot() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let store = local_store(&temp).with_publication_failure();
    assert!(
        store
            .capture(snippet("input.rs", b"content\n"))
            .await
            .is_err()
    );
    assert_eq!(published_directories(store.root().as_path()), 0);

    let staging = store.root().join(".staging").join("interrupted");
    std::fs::create_dir_all(staging.as_path()).expect("create interrupted staging");
    std::fs::write(staging.join("manifest.json").as_path(), b"{}")
        .expect("write incomplete manifest");
    let absent = SnapshotId::parse(format!("audex-source-v1:sha256:{}", "0".repeat(64)))
        .expect("valid test snapshot ID");
    assert!(matches!(
        store.open(&absent).await,
        Err(SnapshotError::NotFound(_))
    ));
}

#[tokio::test]
async fn tampering_with_manifest_object_or_materialisation_is_detected() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let store = local_store(&temp);

    let manifest_handle = store
        .capture(snippet("manifest.rs", b"manifest\n"))
        .await
        .expect("capture manifest fixture");
    let manifest_root = store.root().join(manifest_handle.snapshot_id().digest());
    let manifest_path = manifest_root.join("manifest.json");
    make_file_writable(manifest_path.as_path());
    let manifest_bytes = std::fs::read(manifest_path.as_path()).expect("read manifest");
    let changed = String::from_utf8(manifest_bytes)
        .expect("manifest is UTF-8")
        .replace("\"layout_version\": 1", "\"layout_version\": 2");
    std::fs::write(manifest_path.as_path(), changed).expect("tamper manifest");
    assert!(store.open(manifest_handle.snapshot_id()).await.is_err());

    let object_handle = store
        .capture(snippet("object.rs", b"object!!\n"))
        .await
        .expect("capture object fixture");
    let object_entry = &object_handle.manifest().identity.entries[0];
    let object_path = store
        .root()
        .join(object_handle.snapshot_id().digest())
        .join("objects")
        .join("sha256")
        .join(&object_entry.sha256);
    make_file_writable(object_path.as_path());
    std::fs::write(object_path.as_path(), b"changed!\n").expect("tamper object");
    assert!(store.open(object_handle.snapshot_id()).await.is_err());

    let source_handle = store
        .capture(snippet("source.rs", b"source!!\n"))
        .await
        .expect("capture source fixture");
    let source_path = store
        .root()
        .join(source_handle.snapshot_id().digest())
        .join("source")
        .join("source.rs");
    make_file_writable(source_path.as_path());
    std::fs::write(source_path.as_path(), b"changed!\n").expect("tamper materialisation");
    assert!(store.open(source_handle.snapshot_id()).await.is_err());
    make_tree_writable(temp.path());
}

#[tokio::test]
async fn access_is_manifest_bound_bounded_and_literal() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let handle = local_store(&temp)
        .capture(snippet("src/input.txt", b"aaaa\nsecond line\n"))
        .await
        .expect("capture access fixture");
    let access = handle.access(AccessLimits {
        max_read_bytes: 4,
        max_search_query_bytes: 8,
        max_search_results: 3,
    });
    let path = SnapshotPath::new("src/input.txt").expect("valid path");
    assert_eq!(access.list().await.expect("list entries").len(), 1);
    assert_eq!(
        access.read(&path, 1, 3).await.expect("read range").bytes,
        b"aaa"
    );
    assert!(matches!(
        access.read(&path, 0, 5).await,
        Err(SnapshotError::AccessLimit(_))
    ));
    assert!(matches!(
        access
            .read(&SnapshotPath::new("other.txt").expect("valid path"), 0, 1)
            .await,
        Err(SnapshotError::EntryNotFound(_))
    ));
    let hits = access
        .search(SearchRequest {
            query: b"aa".to_vec(),
            path: Some(path),
            max_results: 3,
        })
        .await
        .expect("literal search");
    assert_eq!(
        hits.iter().map(|hit| hit.offset).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(matches!(
        access
            .search(SearchRequest {
                query: Vec::new(),
                path: None,
                max_results: 1,
            })
            .await,
        Err(SnapshotError::AccessLimit(_))
    ));
    make_tree_writable(temp.path());
}

#[cfg(unix)]
#[tokio::test]
async fn access_never_follows_a_replaced_materialisation_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("create temp directory");
    let store = local_store(&temp);
    let handle = store
        .capture(snippet("input.rs", b"inside\n"))
        .await
        .expect("capture source");
    let source_root = store
        .root()
        .join(handle.snapshot_id().digest())
        .join("source");
    make_directory_writable(source_root.as_path());
    let materialised = source_root.join("input.rs");
    make_file_writable(materialised.as_path());
    std::fs::remove_file(materialised.as_path()).expect("remove materialisation");
    let outside = temp.path().join("outside.rs");
    std::fs::write(&outside, b"outside\n").expect("write outside source");
    symlink(&outside, materialised.as_path()).expect("replace with symlink");

    let access = handle.access(AccessLimits::default());
    assert!(matches!(
        access
            .read(&SnapshotPath::new("input.rs").expect("valid path"), 0, 8)
            .await,
        Err(SnapshotError::Integrity(_))
    ));
    make_tree_writable(temp.path());
}

#[tokio::test]
async fn access_uses_the_injected_executor_filesystem() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let counting = Arc::new(CountingFileSystem::new());
    let file_system: Arc<dyn ExecutorFileSystem> = counting.clone();
    let store = SnapshotStore::new(
        absolute(&temp.path().join("snapshots")),
        file_system,
        CaptureLimits::default(),
    );
    let handle = store
        .capture(snippet("input.txt", b"needle in a haystack\n"))
        .await
        .expect("capture fixture");
    counting.reset();
    let access = handle.access(AccessLimits::default());
    let path = SnapshotPath::new("input.txt").expect("valid path");

    access.list().await.expect("list entries");
    access.read(&path, 0, 6).await.expect("read entry");
    access
        .search(SearchRequest {
            query: b"needle".to_vec(),
            path: None,
            max_results: 10,
        })
        .await
        .expect("search entry");

    assert!(counting.metadata_calls.load(Ordering::SeqCst) >= 3);
    assert!(counting.read_calls.load(Ordering::SeqCst) >= 2);
    make_tree_writable(temp.path());
}

fn published_directories(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != ".staging")
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count()
}

#[cfg(unix)]
fn make_file_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .expect("make file writable");
}

#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)]
fn make_file_writable(path: &Path) {
    let mut permissions = std::fs::metadata(path)
        .expect("file metadata")
        .permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).expect("make file writable");
}

#[cfg(unix)]
fn make_directory_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make directory writable");
}

#[cfg(not(unix))]
fn make_directory_writable(_path: &Path) {}

fn make_tree_writable(root: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return;
    };
    if metadata.is_dir() {
        make_directory_writable(root);
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                make_tree_writable(&entry.path());
            }
        }
    } else if !metadata.file_type().is_symlink() {
        make_file_writable(root);
    }
}

struct CountingFileSystem {
    inner: LocalFileSystem,
    read_calls: AtomicUsize,
    metadata_calls: AtomicUsize,
}

impl CountingFileSystem {
    fn new() -> Self {
        Self {
            inner: LocalFileSystem::unsandboxed(),
            read_calls: AtomicUsize::new(0),
            metadata_calls: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        self.read_calls.store(0, Ordering::SeqCst);
        self.metadata_calls.store(0, Ordering::SeqCst);
    }
}

impl ExecutorFileSystem for CountingFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        self.inner.canonicalize(path, sandbox)
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        options: ReadFileOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read_file(path, options, sandbox)
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        self.inner.read_file_stream(path, sandbox)
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        options: WriteFileOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.write_file(path, contents, options, sandbox)
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        options: GetMetadataOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        self.metadata_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_metadata(path, options, sandbox)
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        self.inner.read_directory(path, sandbox)
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        self.inner.walk(path, options, sandbox)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.remove(path, options, sandbox)
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner
            .copy(source_path, destination_path, options, sandbox)
    }
}
