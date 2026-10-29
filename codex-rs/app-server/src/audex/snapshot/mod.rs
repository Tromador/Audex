mod access;
mod capture;
pub(super) mod manifest;

#[cfg(test)]
mod tests;

use codex_exec_server::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::sync::Arc;
use thiserror::Error;

#[allow(unused_imports)]
pub(crate) use access::AccessLimits;
#[allow(unused_imports)]
pub(crate) use access::SearchHit;
#[allow(unused_imports)]
pub(crate) use access::SearchRequest;
#[allow(unused_imports)]
pub(crate) use access::SnapshotAccess;
#[allow(unused_imports)]
pub(crate) use access::VerifiedBytes;
#[allow(unused_imports)]
pub(crate) use capture::CaptureRequest;
#[allow(unused_imports)]
pub(crate) use capture::CaptureSource;
#[allow(unused_imports)]
pub(crate) use capture::ContextDocuments;
#[allow(unused_imports)]
pub(crate) use manifest::ContentId;
#[allow(unused_imports)]
pub(crate) use manifest::SnapshotEntryV1;
#[allow(unused_imports)]
pub(crate) use manifest::SnapshotId;
#[allow(unused_imports)]
pub(crate) use manifest::SnapshotManifestV1;
#[allow(unused_imports)]
pub(crate) use manifest::SnapshotPath;

pub(crate) const DEFAULT_MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_CONTEXT_DOCUMENT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureLimits {
    pub max_source_bytes: u64,
    pub max_context_document_bytes: u64,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            max_context_document_bytes: DEFAULT_MAX_CONTEXT_DOCUMENT_BYTES,
        }
    }
}

pub(crate) struct SnapshotStore {
    root: AbsolutePathBuf,
    file_system: Arc<dyn ExecutorFileSystem>,
    limits: CaptureLimits,
    #[cfg(test)]
    after_first_file_read: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    fail_before_publication: bool,
}

impl SnapshotStore {
    pub(crate) fn new(
        root: AbsolutePathBuf,
        file_system: Arc<dyn ExecutorFileSystem>,
        limits: CaptureLimits,
    ) -> Self {
        Self {
            root,
            file_system,
            limits,
            #[cfg(test)]
            after_first_file_read: None,
            #[cfg(test)]
            fail_before_publication: false,
        }
    }

    #[cfg(test)]
    fn with_after_first_file_read(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.after_first_file_read = Some(hook);
        self
    }

    #[cfg(test)]
    fn with_publication_failure(mut self) -> Self {
        self.fail_before_publication = true;
        self
    }

    pub(crate) fn root(&self) -> &AbsolutePathBuf {
        &self.root
    }

    pub(crate) async fn capture(
        &self,
        request: CaptureRequest,
    ) -> Result<SnapshotHandle, SnapshotError> {
        capture::capture(self, request).await
    }

    pub(crate) async fn open(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<SnapshotHandle, SnapshotError> {
        capture::open(self, snapshot_id).await
    }
}

#[derive(Clone)]
pub(crate) struct SnapshotHandle {
    snapshot_root: AbsolutePathBuf,
    manifest: Arc<SnapshotManifestV1>,
    file_system: Arc<dyn ExecutorFileSystem>,
}

impl SnapshotHandle {
    fn new(
        snapshot_root: AbsolutePathBuf,
        manifest: SnapshotManifestV1,
        file_system: Arc<dyn ExecutorFileSystem>,
    ) -> Self {
        Self {
            snapshot_root,
            manifest: Arc::new(manifest),
            file_system,
        }
    }

    pub(crate) fn snapshot_id(&self) -> &SnapshotId {
        &self.manifest.snapshot_id
    }

    pub(crate) fn content_id(&self) -> &ContentId {
        &self.manifest.content_id
    }

    pub(crate) fn manifest(&self) -> &SnapshotManifestV1 {
        self.manifest.as_ref()
    }

    pub(crate) fn access(&self, limits: AccessLimits) -> SnapshotAccess {
        SnapshotAccess::new(self.clone(), limits)
    }

    fn source_path(&self, logical_path: &SnapshotPath) -> AbsolutePathBuf {
        logical_path
            .components()
            .fold(self.snapshot_root.join("source"), |path, component| {
                path.join(component)
            })
    }
}

#[derive(Debug, Error)]
pub(crate) enum SnapshotError {
    #[error("invalid snapshot path: {0}")]
    InvalidPath(String),
    #[error("invalid source: {0}")]
    InvalidSource(String),
    #[error("source exceeds the configured limit of {limit} bytes")]
    SourceTooLarge { limit: u64 },
    #[error("context document `{name}` exceeds the configured limit of {limit} bytes")]
    ContextDocumentTooLarge { name: String, limit: u64 },
    #[error("source changed while it was being captured")]
    SourceChanged,
    #[error("invalid snapshot manifest: {0}")]
    InvalidManifest(String),
    #[error("snapshot integrity check failed: {0}")]
    Integrity(String),
    #[error("snapshot access limit exceeded: {0}")]
    AccessLimit(String),
    #[error("snapshot entry was not found: {0}")]
    EntryNotFound(String),
    #[error("snapshot does not exist: {0}")]
    NotFound(String),
    #[error("snapshot JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("snapshot filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot filesystem error while {context}: {source}")]
    IoContext {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("snapshot publication task failed: {0}")]
    Task(String),
}
