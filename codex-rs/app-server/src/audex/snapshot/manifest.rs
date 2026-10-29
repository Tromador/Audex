use super::CaptureLimits;
use super::SnapshotError;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use sha2::Digest;
use sha2::Sha256;
use std::fmt;

const CONTENT_ID_PREFIX: &str = "audex-content-v1:sha256:";
const SNAPSHOT_ID_PREFIX: &str = "audex-source-v1:sha256:";
const MAX_LOGICAL_PATH_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SnapshotPath(String);

impl SnapshotPath {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, SnapshotError> {
        let value = value.into();
        validate_snapshot_path(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl fmt::Display for SnapshotPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for SnapshotPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SnapshotPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_snapshot_path(path: &str) -> Result<(), SnapshotError> {
    if path.is_empty() {
        return Err(SnapshotError::InvalidPath("path is empty".to_string()));
    }
    if path.len() > MAX_LOGICAL_PATH_BYTES {
        return Err(SnapshotError::InvalidPath(format!(
            "path exceeds {MAX_LOGICAL_PATH_BYTES} bytes"
        )));
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err(SnapshotError::InvalidPath(
            "path must be relative and normalized".to_string(),
        ));
    }
    if path.contains('\\') || path.contains(':') {
        return Err(SnapshotError::InvalidPath(
            "path contains a non-portable separator or prefix".to_string(),
        ));
    }

    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(SnapshotError::InvalidPath(
                "path contains an empty, current, or parent component".to_string(),
            ));
        }
        if component.ends_with(['.', ' ']) || component.chars().any(char::is_control) {
            return Err(SnapshotError::InvalidPath(format!(
                "path component `{component}` is not portable"
            )));
        }
        let device_stem = component
            .split_once('.')
            .map_or(component, |(stem, _)| stem)
            .to_ascii_uppercase();
        let reserved_device = matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || device_stem
                .strip_prefix("COM")
                .or_else(|| device_stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
                });
        if reserved_device {
            return Err(SnapshotError::InvalidPath(format!(
                "path component `{component}` is reserved on Windows"
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ContentId(String);

impl ContentId {
    fn from_digest(digest: String) -> Self {
        Self(format!("{CONTENT_ID_PREFIX}{digest}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SnapshotId(String);

impl SnapshotId {
    fn from_digest(digest: String) -> Self {
        Self(format!("{SNAPSHOT_ID_PREFIX}{digest}"))
    }

    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, SnapshotError> {
        let value = value.into();
        let digest = value.strip_prefix(SNAPSHOT_ID_PREFIX).ok_or_else(|| {
            SnapshotError::InvalidManifest("snapshot ID has an invalid prefix".to_string())
        })?;
        validate_digest(digest)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn digest(&self) -> &str {
        self.0.strip_prefix(SNAPSHOT_ID_PREFIX).unwrap_or_default()
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceFormV1 {
    Snippet,
    File,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextDocumentRole {
    AudexGuidance,
    AgentsEvidence,
}

impl ContextDocumentRole {
    pub(crate) fn logical_path(self) -> &'static str {
        match self {
            Self::AudexGuidance => "AUDEX.md",
            Self::AgentsEvidence => "AGENTS.md",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotEntryV1 {
    pub path: SnapshotPath,
    pub kind: SnapshotEntryKindV1,
    pub executable: bool,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotEntryKindV1 {
    RegularFile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextDocumentV1 {
    pub role: ContextDocumentRole,
    pub path: SnapshotPath,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapturePolicyV1 {
    pub version: u32,
    pub max_source_bytes: u64,
    pub max_context_document_bytes: u64,
}

impl From<CaptureLimits> for CapturePolicyV1 {
    fn from(limits: CaptureLimits) -> Self {
        Self {
            version: 1,
            max_source_bytes: limits.max_source_bytes,
            max_context_document_bytes: limits.max_context_document_bytes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotIdentityV1 {
    pub format_version: u32,
    pub hash_algorithm: String,
    pub source_form: SourceFormV1,
    pub capture_policy: CapturePolicyV1,
    pub entries: Vec<SnapshotEntryV1>,
    pub context_documents: Vec<ContextDocumentV1>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotProvenanceV1 {
    pub source_form: SourceFormV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationV1 {
    pub layout_version: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotManifestV1 {
    pub identity: SnapshotIdentityV1,
    pub content_id: ContentId,
    pub snapshot_id: SnapshotId,
    pub provenance: SnapshotProvenanceV1,
    pub publication: PublicationV1,
}

impl SnapshotManifestV1 {
    pub(crate) fn build(
        source_form: SourceFormV1,
        limits: CaptureLimits,
        mut entries: Vec<SnapshotEntryV1>,
        mut context_documents: Vec<ContextDocumentV1>,
    ) -> Result<Self, SnapshotError> {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        context_documents.sort_by(|left, right| {
            left.role
                .cmp(&right.role)
                .then_with(|| left.path.cmp(&right.path))
        });
        let identity = SnapshotIdentityV1 {
            format_version: 1,
            hash_algorithm: "sha256".to_string(),
            source_form,
            capture_policy: limits.into(),
            entries,
            context_documents,
        };
        validate_identity(&identity)?;
        let content_id = content_id(&identity.entries)?;
        let snapshot_id = SnapshotId::from_digest(hash_canonical(&identity)?);
        Ok(Self {
            identity,
            content_id,
            snapshot_id,
            provenance: SnapshotProvenanceV1 { source_form },
            publication: PublicationV1 { layout_version: 1 },
        })
    }

    pub(crate) fn validate(&self) -> Result<(), SnapshotError> {
        validate_identity(&self.identity)?;
        if self.provenance.source_form != self.identity.source_form {
            return Err(SnapshotError::InvalidManifest(
                "provenance source form does not match identity".to_string(),
            ));
        }
        if self.publication.layout_version != 1 {
            return Err(SnapshotError::InvalidManifest(format!(
                "unsupported publication layout version {}",
                self.publication.layout_version
            )));
        }
        let expected_content_id = content_id(&self.identity.entries)?;
        if self.content_id != expected_content_id {
            return Err(SnapshotError::Integrity(
                "content ID does not match the manifest entries".to_string(),
            ));
        }
        let expected_snapshot_id = SnapshotId::from_digest(hash_canonical(&self.identity)?);
        if self.snapshot_id != expected_snapshot_id {
            return Err(SnapshotError::Integrity(
                "snapshot ID does not match the canonical identity".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn to_pretty_bytes(&self) -> Result<Vec<u8>, SnapshotError> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[derive(Serialize)]
struct ContentIdentityV1<'a> {
    format_version: u32,
    hash_algorithm: &'static str,
    entries: &'a [SnapshotEntryV1],
}

fn content_id(entries: &[SnapshotEntryV1]) -> Result<ContentId, SnapshotError> {
    Ok(ContentId::from_digest(hash_canonical(
        &ContentIdentityV1 {
            format_version: 1,
            hash_algorithm: "sha256",
            entries,
        },
    )?))
}

fn validate_identity(identity: &SnapshotIdentityV1) -> Result<(), SnapshotError> {
    if identity.format_version != 1 {
        return Err(SnapshotError::InvalidManifest(format!(
            "unsupported identity format version {}",
            identity.format_version
        )));
    }
    if identity.hash_algorithm != "sha256" {
        return Err(SnapshotError::InvalidManifest(
            "unsupported hash algorithm".to_string(),
        ));
    }
    if identity.capture_policy.version != 1 {
        return Err(SnapshotError::InvalidManifest(format!(
            "unsupported capture policy version {}",
            identity.capture_policy.version
        )));
    }
    if identity.entries.len() != 1 {
        return Err(SnapshotError::InvalidManifest(
            "first-slice snapshots must contain exactly one source entry".to_string(),
        ));
    }
    if !identity
        .entries
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        return Err(SnapshotError::InvalidManifest(
            "source entries are not strictly sorted".to_string(),
        ));
    }
    for entry in &identity.entries {
        validate_digest(&entry.sha256)?;
        if entry.size > identity.capture_policy.max_source_bytes {
            return Err(SnapshotError::InvalidManifest(
                "source entry exceeds the recorded capture policy".to_string(),
            ));
        }
    }
    if identity.context_documents.len() > 2
        || !identity
            .context_documents
            .windows(2)
            .all(|pair| (pair[0].role, &pair[0].path) < (pair[1].role, &pair[1].path))
    {
        return Err(SnapshotError::InvalidManifest(
            "context documents are duplicated or not strictly sorted".to_string(),
        ));
    }
    for document in &identity.context_documents {
        validate_digest(&document.sha256)?;
        if document.path.as_str() != document.role.logical_path() {
            return Err(SnapshotError::InvalidManifest(
                "context document path does not match its semantic role".to_string(),
            ));
        }
        if document.size > identity.capture_policy.max_context_document_bytes {
            return Err(SnapshotError::InvalidManifest(
                "context document exceeds the recorded capture policy".to_string(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let result = write!(encoded, "{byte:02x}");
        debug_assert!(result.is_ok(), "writing to a String cannot fail");
    }
    encoded
}

fn hash_canonical(value: &impl Serialize) -> Result<String, SnapshotError> {
    Ok(sha256_hex(&serde_json::to_vec(value)?))
}

fn validate_digest(digest: &str) -> Result<(), SnapshotError> {
    if digest.len() != 64
        || !digest
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(SnapshotError::InvalidManifest(
            "SHA-256 digest is not 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    Ok(())
}
