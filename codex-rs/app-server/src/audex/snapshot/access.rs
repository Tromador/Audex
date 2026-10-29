use super::SnapshotEntryV1;
use super::SnapshotError;
use super::SnapshotHandle;
use super::SnapshotPath;
use super::manifest::sha256_hex;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccessLimits {
    pub max_read_bytes: usize,
    pub max_search_query_bytes: usize,
    pub max_search_results: usize,
}

impl Default for AccessLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 64 * 1024,
            max_search_query_bytes: 1024,
            max_search_results: 100,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SnapshotAccess {
    handle: SnapshotHandle,
    limits: AccessLimits,
}

impl SnapshotAccess {
    pub(super) fn new(handle: SnapshotHandle, limits: AccessLimits) -> Self {
        Self { handle, limits }
    }

    pub(crate) async fn list(&self) -> Result<Vec<SnapshotEntryV1>, SnapshotError> {
        for entry in &self.handle.manifest.identity.entries {
            let path = self.handle.source_path(&entry.path);
            self.verify_metadata(&path, entry).await?;
        }
        Ok(self.handle.manifest.identity.entries.clone())
    }

    pub(crate) async fn read(
        &self,
        path: &SnapshotPath,
        offset: u64,
        max_bytes: usize,
    ) -> Result<VerifiedBytes, SnapshotError> {
        if max_bytes == 0 || max_bytes > self.limits.max_read_bytes {
            return Err(SnapshotError::AccessLimit(format!(
                "read size must be between 1 and {} bytes",
                self.limits.max_read_bytes
            )));
        }
        let entry = self.entry(path)?;
        if offset > entry.size {
            return Err(SnapshotError::AccessLimit(format!(
                "read offset {offset} exceeds the entry size {}",
                entry.size
            )));
        }
        let bytes = self.read_all_verified(entry).await?;
        let start = usize::try_from(offset).map_err(|_| {
            SnapshotError::AccessLimit("read offset does not fit this platform".to_string())
        })?;
        let end = start.saturating_add(max_bytes).min(bytes.len());
        Ok(VerifiedBytes {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            offset,
            eof: end == bytes.len(),
            bytes: bytes[start..end].to_vec(),
        })
    }

    pub(crate) async fn search(
        &self,
        request: SearchRequest,
    ) -> Result<Vec<SearchHit>, SnapshotError> {
        if request.query.is_empty() || request.query.len() > self.limits.max_search_query_bytes {
            return Err(SnapshotError::AccessLimit(format!(
                "literal query must be between 1 and {} bytes",
                self.limits.max_search_query_bytes
            )));
        }
        if request.max_results == 0 || request.max_results > self.limits.max_search_results {
            return Err(SnapshotError::AccessLimit(format!(
                "search result limit must be between 1 and {}",
                self.limits.max_search_results
            )));
        }

        let entries = match &request.path {
            Some(path) => vec![self.entry(path)?],
            None => self.handle.manifest.identity.entries.iter().collect(),
        };
        let mut hits = Vec::new();
        for entry in entries {
            let bytes = self.read_all_verified(entry).await?;
            let remaining = request.max_results - hits.len();
            for offset in literal_offsets(&bytes, &request.query, remaining) {
                hits.push(SearchHit {
                    path: entry.path.clone(),
                    sha256: entry.sha256.clone(),
                    offset: offset as u64,
                });
                if hits.len() == request.max_results {
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }

    fn entry(&self, path: &SnapshotPath) -> Result<&SnapshotEntryV1, SnapshotError> {
        self.handle
            .manifest
            .identity
            .entries
            .iter()
            .find(|entry| &entry.path == path)
            .ok_or_else(|| SnapshotError::EntryNotFound(path.to_string()))
    }

    async fn read_all_verified(&self, entry: &SnapshotEntryV1) -> Result<Vec<u8>, SnapshotError> {
        let path = self.handle.source_path(&entry.path);
        self.verify_metadata(&path, entry).await?;
        let uri = PathUri::from_abs_path(&path);
        let bytes = self
            .handle
            .file_system
            .read_file(
                &uri,
                ReadFileOptions {
                    follow_symlinks: false,
                },
                None,
            )
            .await
            .map_err(|error| integrity_error(&path, error))?;
        if bytes.len() as u64 != entry.size || sha256_hex(&bytes) != entry.sha256 {
            return Err(SnapshotError::Integrity(format!(
                "materialised content changed for `{}`",
                entry.path
            )));
        }
        Ok(bytes)
    }

    async fn verify_metadata(
        &self,
        path: &AbsolutePathBuf,
        entry: &SnapshotEntryV1,
    ) -> Result<(), SnapshotError> {
        let uri = PathUri::from_abs_path(path);
        let metadata = self
            .handle
            .file_system
            .get_metadata(
                &uri,
                GetMetadataOptions {
                    follow_symlinks: false,
                },
                None,
            )
            .await
            .map_err(|error| integrity_error(path, error))?;
        if !metadata.is_file
            || metadata.is_directory
            || metadata.is_symlink
            || metadata.size != entry.size
        {
            return Err(SnapshotError::Integrity(format!(
                "materialised entry metadata changed for `{}`",
                entry.path
            )));
        }
        Ok(())
    }
}

fn integrity_error(path: &AbsolutePathBuf, error: std::io::Error) -> SnapshotError {
    SnapshotError::Integrity(format!(
        "cannot read `{}`: {error}",
        path.as_path().display()
    ))
}

pub(crate) struct SearchRequest {
    pub query: Vec<u8>,
    pub path: Option<SnapshotPath>,
    pub max_results: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchHit {
    pub path: SnapshotPath,
    pub sha256: String,
    pub offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedBytes {
    pub path: SnapshotPath,
    pub sha256: String,
    pub offset: u64,
    pub eof: bool,
    pub bytes: Vec<u8>,
}

/// Returns overlapping literal byte matches in linear time using a small KMP prefix table.
fn literal_offsets(haystack: &[u8], needle: &[u8], max_results: usize) -> Vec<usize> {
    if needle.is_empty() || max_results == 0 {
        return Vec::new();
    }

    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for index in 1..needle.len() {
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
            prefix[index] = matched;
        }
    }

    let mut offsets = Vec::new();
    matched = 0;
    for (index, byte) in haystack.iter().enumerate() {
        while matched > 0 && *byte != needle[matched] {
            matched = prefix[matched - 1];
        }
        if *byte == needle[matched] {
            matched += 1;
            if matched == needle.len() {
                offsets.push(index + 1 - needle.len());
                if offsets.len() == max_results {
                    break;
                }
                matched = prefix[matched - 1];
            }
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::literal_offsets;

    #[test]
    fn literal_offsets_stops_at_the_requested_limit() {
        assert_eq!(literal_offsets(b"aaaaaaaa", b"a", 1), vec![0]);
        assert_eq!(literal_offsets(b"aaaa", b"aa", 2), vec![0, 1]);
        assert!(literal_offsets(b"aaaa", b"a", 0).is_empty());
    }
}
