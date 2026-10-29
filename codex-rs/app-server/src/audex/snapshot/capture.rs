use super::CaptureLimits;
use super::SnapshotError;
use super::SnapshotHandle;
use super::SnapshotStore;
use super::manifest::ContextDocumentRole;
use super::manifest::ContextDocumentV1;
use super::manifest::SnapshotEntryKindV1;
use super::manifest::SnapshotEntryV1;
use super::manifest::SnapshotId;
use super::manifest::SnapshotManifestV1;
use super::manifest::SnapshotPath;
use super::manifest::SourceFormV1;
use super::manifest::sha256_hex;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub(crate) enum CaptureSource {
    Snippet {
        logical_path: SnapshotPath,
        bytes: Vec<u8>,
    },
    File {
        source_path: AbsolutePathBuf,
        logical_path: SnapshotPath,
    },
}

#[derive(Default)]
pub(crate) struct ContextDocuments {
    pub audex_guidance: Option<Vec<u8>>,
    pub agents_evidence: Option<Vec<u8>>,
}

pub(crate) struct CaptureRequest {
    pub source: CaptureSource,
    pub context_documents: ContextDocuments,
}

struct PreparedSource {
    form: SourceFormV1,
    logical_path: SnapshotPath,
    executable: bool,
    bytes: Vec<u8>,
}

struct PreparedContextDocument {
    record: ContextDocumentV1,
    bytes: Vec<u8>,
}

struct PublicationInput {
    root: PathBuf,
    manifest: SnapshotManifestV1,
    manifest_bytes: Vec<u8>,
    source_bytes: Vec<u8>,
    context_documents: Vec<PreparedContextDocument>,
    #[cfg(test)]
    fail_before_publication: bool,
}

pub(super) async fn capture(
    store: &SnapshotStore,
    request: CaptureRequest,
) -> Result<SnapshotHandle, SnapshotError> {
    let prepared_source = prepare_source(store, request.source).await?;
    let prepared_documents = prepare_context_documents(store.limits, request.context_documents)?;
    let source_digest = sha256_hex(&prepared_source.bytes);
    let source_entry = SnapshotEntryV1 {
        path: prepared_source.logical_path,
        kind: SnapshotEntryKindV1::RegularFile,
        executable: prepared_source.executable,
        size: prepared_source.bytes.len() as u64,
        sha256: source_digest,
    };
    let manifest = SnapshotManifestV1::build(
        prepared_source.form,
        store.limits,
        vec![source_entry],
        prepared_documents
            .iter()
            .map(|document| document.record.clone())
            .collect(),
    )?;
    let snapshot_id = manifest.snapshot_id.clone();
    let manifest_bytes = manifest.to_pretty_bytes()?;
    let publication = PublicationInput {
        root: store.root.as_path().to_path_buf(),
        manifest,
        manifest_bytes,
        source_bytes: prepared_source.bytes,
        context_documents: prepared_documents,
        #[cfg(test)]
        fail_before_publication: store.fail_before_publication,
    };

    tokio::task::spawn_blocking(move || publish(publication))
        .await
        .map_err(|error| SnapshotError::Task(error.to_string()))??;

    open(store, &snapshot_id).await
}

async fn prepare_source(
    store: &SnapshotStore,
    source: CaptureSource,
) -> Result<PreparedSource, SnapshotError> {
    match source {
        CaptureSource::Snippet {
            logical_path,
            bytes,
        } => {
            enforce_source_size(bytes.len() as u64, store.limits)?;
            Ok(PreparedSource {
                form: SourceFormV1::Snippet,
                logical_path,
                executable: false,
                bytes,
            })
        }
        CaptureSource::File {
            source_path,
            logical_path,
        } => prepare_file(store, source_path, logical_path).await,
    }
}

async fn prepare_file(
    store: &SnapshotStore,
    source_path: AbsolutePathBuf,
    logical_path: SnapshotPath,
) -> Result<PreparedSource, SnapshotError> {
    let uri = PathUri::from_abs_path(&source_path);
    let options = GetMetadataOptions {
        follow_symlinks: false,
    };
    let before = store
        .file_system
        .get_metadata(&uri, options, None)
        .await
        .map_err(|error| SnapshotError::InvalidSource(error.to_string()))?;
    if !before.is_file || before.is_directory || before.is_symlink {
        return Err(SnapshotError::InvalidSource(
            "path is not a regular file".to_string(),
        ));
    }
    enforce_source_size(before.size, store.limits)?;
    // ExecutorFileSystem deliberately exposes portable metadata only. Local file capture also
    // needs executable mode and a stable race fingerprint, so inspect those local-only fields
    // around the two no-follow ExecutorFileSystem reads.
    let local_before = LocalFileFingerprint::read(source_path.as_path())?;
    let executable = local_before.executable;
    let read_options = ReadFileOptions {
        follow_symlinks: false,
    };
    let first = store
        .file_system
        .read_file(&uri, read_options, None)
        .await
        .map_err(|error| SnapshotError::InvalidSource(error.to_string()))?;
    enforce_source_size(first.len() as u64, store.limits)?;

    #[cfg(test)]
    if let Some(hook) = &store.after_first_file_read {
        hook();
    }

    let second = store
        .file_system
        .read_file(&uri, read_options, None)
        .await
        .map_err(|_| SnapshotError::SourceChanged)?;
    let after = store
        .file_system
        .get_metadata(&uri, options, None)
        .await
        .map_err(|_| SnapshotError::SourceChanged)?;
    let local_after = LocalFileFingerprint::read(source_path.as_path())
        .map_err(|_| SnapshotError::SourceChanged)?;
    if first != second || before != after || local_before != local_after {
        return Err(SnapshotError::SourceChanged);
    }
    enforce_source_size(second.len() as u64, store.limits)?;
    Ok(PreparedSource {
        form: SourceFormV1::File,
        logical_path,
        executable,
        bytes: first,
    })
}

fn enforce_source_size(size: u64, limits: CaptureLimits) -> Result<(), SnapshotError> {
    if size > limits.max_source_bytes {
        return Err(SnapshotError::SourceTooLarge {
            limit: limits.max_source_bytes,
        });
    }
    Ok(())
}

fn prepare_context_documents(
    limits: CaptureLimits,
    documents: ContextDocuments,
) -> Result<Vec<PreparedContextDocument>, SnapshotError> {
    let mut prepared = Vec::new();
    for (role, bytes) in [
        (ContextDocumentRole::AudexGuidance, documents.audex_guidance),
        (
            ContextDocumentRole::AgentsEvidence,
            documents.agents_evidence,
        ),
    ] {
        let Some(bytes) = bytes else {
            continue;
        };
        if bytes.len() as u64 > limits.max_context_document_bytes {
            return Err(SnapshotError::ContextDocumentTooLarge {
                name: role.logical_path().to_string(),
                limit: limits.max_context_document_bytes,
            });
        }
        prepared.push(PreparedContextDocument {
            record: ContextDocumentV1 {
                role,
                path: SnapshotPath::new(role.logical_path())?,
                size: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
            },
            bytes,
        });
    }
    Ok(prepared)
}

fn publish(input: PublicationInput) -> Result<(), SnapshotError> {
    let snapshots_root = input.root;
    let staging_root = snapshots_root.join(".staging");
    std::fs::create_dir_all(&staging_root)?;
    let final_root = snapshots_root.join(input.manifest.snapshot_id.digest());
    if final_root.exists() {
        return Ok(());
    }

    let staging = tempfile::Builder::new()
        .prefix("capture-")
        .tempdir_in(&staging_root)?;
    let object_root = staging.path().join("objects").join("sha256");
    std::fs::create_dir_all(&object_root)?;
    write_new_file(
        &object_root.join(&input.manifest.identity.entries[0].sha256),
        &input.source_bytes,
    )?;
    for document in &input.context_documents {
        let path = object_root.join(&document.record.sha256);
        if !path.exists() {
            write_new_file(&path, &document.bytes)?;
        }
    }

    let entry = &input.manifest.identity.entries[0];
    let source_path = join_logical(staging.path().join("source"), &entry.path);
    let source_parent = source_path.parent().ok_or_else(|| {
        SnapshotError::InvalidPath("source entry has no materialisation parent".to_string())
    })?;
    std::fs::create_dir_all(source_parent)?;
    write_new_file(&source_path, &input.source_bytes)?;

    // The manifest is written last. A staging directory is never a published snapshot.
    write_new_file(&staging.path().join("manifest.json"), &input.manifest_bytes)?;
    sync_directory(&object_root)?;
    sync_directory(source_parent)?;
    sync_directory(staging.path())?;
    #[cfg(test)]
    if input.fail_before_publication {
        return Err(SnapshotError::Io(io::Error::other(
            "injected failure before snapshot publication",
        )));
    }
    make_tree_read_only(staging.path(), entry).map_err(|source| SnapshotError::IoContext {
        context: "making the staged snapshot read-only".to_string(),
        source,
    })?;

    match std::fs::rename(staging.path(), &final_root) {
        Ok(()) => {
            let _published_path = staging.keep();
            set_directory_read_only(&final_root).map_err(|source| SnapshotError::IoContext {
                context: "freezing the published snapshot root".to_string(),
                source,
            })?;
            sync_directory(&snapshots_root).map_err(|source| SnapshotError::IoContext {
                context: "syncing the snapshot store after publication".to_string(),
                source,
            })?;
            Ok(())
        }
        Err(_error) if final_root.exists() => {
            make_tree_writable(staging.path());
            drop(staging);
            Ok(())
        }
        Err(error) => {
            make_tree_writable(staging.path());
            Err(SnapshotError::IoContext {
                context: format!("publishing staged snapshot to `{}`", final_root.display()),
                source: error,
            })
        }
    }
}

pub(super) async fn open(
    store: &SnapshotStore,
    snapshot_id: &SnapshotId,
) -> Result<SnapshotHandle, SnapshotError> {
    let snapshot_root = store.root.join(snapshot_id.digest());
    if !snapshot_root.as_path().exists() {
        return Err(SnapshotError::NotFound(snapshot_id.to_string()));
    }
    verify_published_root_is_read_only(snapshot_root.as_path())?;
    let manifest_path = snapshot_root.join("manifest.json");
    let manifest_bytes = read_nofollow(store, &manifest_path).await?;
    let manifest: SnapshotManifestV1 = serde_json::from_slice(&manifest_bytes)?;
    manifest.validate()?;
    if &manifest.snapshot_id != snapshot_id {
        return Err(SnapshotError::Integrity(
            "published directory does not match its manifest snapshot ID".to_string(),
        ));
    }

    for entry in &manifest.identity.entries {
        let object_path = snapshot_root
            .join("objects")
            .join("sha256")
            .join(&entry.sha256);
        verify_file(store, &object_path, entry.size, &entry.sha256).await?;
        let materialized_path = entry
            .path
            .components()
            .fold(snapshot_root.join("source"), |path, component| {
                path.join(component)
            });
        verify_file(store, &materialized_path, entry.size, &entry.sha256).await?;
        if native_executable(materialized_path.as_path()).map_err(|error| {
            SnapshotError::Integrity(format!(
                "cannot inspect materialised mode for `{}`: {error}",
                entry.path
            ))
        })? != entry.executable
        {
            return Err(SnapshotError::Integrity(format!(
                "materialised mode does not match `{}`",
                entry.path
            )));
        }
    }
    for document in &manifest.identity.context_documents {
        let object_path = snapshot_root
            .join("objects")
            .join("sha256")
            .join(&document.sha256);
        verify_file(store, &object_path, document.size, &document.sha256).await?;
    }

    Ok(SnapshotHandle::new(
        snapshot_root,
        manifest,
        store.file_system.clone(),
    ))
}

async fn verify_file(
    store: &SnapshotStore,
    path: &AbsolutePathBuf,
    expected_size: u64,
    expected_digest: &str,
) -> Result<(), SnapshotError> {
    let bytes = read_nofollow(store, path).await?;
    if bytes.len() as u64 != expected_size || sha256_hex(&bytes) != expected_digest {
        return Err(SnapshotError::Integrity(format!(
            "content does not match `{}`",
            path.as_path().display()
        )));
    }
    Ok(())
}

async fn read_nofollow(
    store: &SnapshotStore,
    path: &AbsolutePathBuf,
) -> Result<Vec<u8>, SnapshotError> {
    let uri = PathUri::from_abs_path(path);
    let metadata = store
        .file_system
        .get_metadata(
            &uri,
            GetMetadataOptions {
                follow_symlinks: false,
            },
            None,
        )
        .await
        .map_err(|error| {
            SnapshotError::Integrity(format!(
                "cannot inspect `{}`: {error}",
                path.as_path().display()
            ))
        })?;
    if !metadata.is_file || metadata.is_directory || metadata.is_symlink {
        return Err(SnapshotError::Integrity(format!(
            "path is not a regular file: {}",
            path.as_path().display()
        )));
    }
    store
        .file_system
        .read_file(
            &uri,
            ReadFileOptions {
                follow_symlinks: false,
            },
            None,
        )
        .await
        .map_err(|error| {
            SnapshotError::Integrity(format!(
                "cannot read `{}`: {error}",
                path.as_path().display()
            ))
        })
}

fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn join_logical(mut root: PathBuf, path: &SnapshotPath) -> PathBuf {
    for component in path.components() {
        root.push(component);
    }
    root
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn make_tree_read_only(root: &Path, source_entry: &SnapshotEntryV1) -> io::Result<()> {
    let source_path = join_logical(root.join("source"), &source_entry.path);
    for entry in walk_paths(root)? {
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.is_dir() {
            if entry != root {
                set_directory_read_only(&entry)?;
            }
        } else {
            set_file_read_only(&entry, entry == source_path && source_entry.executable)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn verify_published_root_is_read_only(path: &Path) -> Result<(), SnapshotError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o222 != 0 {
        return Err(SnapshotError::Integrity(
            "published snapshot root is not read-only".to_string(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_published_root_is_read_only(path: &Path) -> Result<(), SnapshotError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(SnapshotError::Integrity(
            "published snapshot root is not a directory".to_string(),
        ));
    }
    Ok(())
}

fn walk_paths(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(path) = pending.pop() {
        if std::fs::symlink_metadata(&path)?.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
        paths.push(path);
    }
    paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    Ok(paths)
}

#[cfg(unix)]
fn set_file_read_only(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o555 } else { 0o444 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_file_read_only(path: &Path, _executable: bool) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn set_directory_read_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))
}

#[cfg(not(unix))]
fn set_directory_read_only(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn make_tree_writable(root: &Path) {
    let Ok(mut paths) = walk_paths(root) else {
        return;
    };
    paths.sort_by_key(|path| path.components().count());
    for path in paths {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let _ = set_path_writable(&path, metadata.is_dir());
    }
}

#[cfg(unix)]
fn set_path_writable(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if directory { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)]
fn set_path_writable(path: &Path, _directory: bool) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalFileFingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    readonly: bool,
    executable: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
}

impl LocalFileFingerprint {
    fn read(path: &Path) -> Result<Self, SnapshotError> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| SnapshotError::InvalidSource(error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SnapshotError::InvalidSource(
                "path is not a regular non-symlink file".to_string(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                readonly: metadata.permissions().readonly(),
                executable: metadata.mode() & 0o111 != 0,
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                change_seconds: metadata.ctime(),
                change_nanoseconds: metadata.ctime_nsec(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                readonly: metadata.permissions().readonly(),
                executable: false,
            })
        }
    }
}

#[cfg(unix)]
fn native_executable(path: &Path) -> Result<bool, SnapshotError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SnapshotError::Integrity(format!(
            "materialisation is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn native_executable(path: &Path) -> Result<bool, SnapshotError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SnapshotError::Integrity(format!(
            "materialisation is not a regular file: {}",
            path.display()
        )));
    }
    Ok(false)
}
