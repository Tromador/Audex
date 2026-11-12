//! Retained conversation authority: one existing audit, no execution or write methods.
use super::controller::evidence::retained_evidence;
use super::snapshot::{CaptureLimits, SnapshotId, SnapshotStore, manifest::sha256_hex};
use super::source::{MAX_RESULT_BYTES, SourceService, TOOL_NAMES};
use super::store::AuditStore;
use codex_app_server_protocol::{AuditEvidenceParams, AuditEvidenceQuery};
use codex_exec_server::LocalFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EvidenceIdentity {
    pub run_id: String,
    pub snapshot_id: String,
    pub manifest_digest: String,
}

pub(crate) struct ConversationEvidence {
    pub identity: EvidenceIdentity,
    store: AuditStore,
    snapshots: SnapshotStore,
    source: SourceService,
}

impl ConversationEvidence {
    pub(crate) async fn open(root: &Path, run_id: &str) -> anyhow::Result<Self> {
        let root = AbsolutePathBuf::from_absolute_path(root)?;
        let store = AuditStore::open_read_only(root.clone()).await?;
        let snapshots = SnapshotStore::new(
            root.join("snapshots"),
            Arc::new(LocalFileSystem::unsandboxed()),
            CaptureLimits::default(),
        );
        let run = store.load_run_metadata(run_id).await?;
        anyhow::ensure!(run.phase.is_terminal(), "select a retained, terminal audit");
        let (_, _, _, manifest_digest) = store.attempt_input(&run.operation_id).await?;
        let snapshot = snapshots
            .open(&SnapshotId::parse(run.snapshot_id.clone())?)
            .await?;
        anyhow::ensure!(
            sha256_hex(&snapshot.manifest().to_pretty_bytes()?) == manifest_digest,
            "snapshot manifest binding mismatch"
        );
        Ok(Self {
            identity: EvidenceIdentity {
                run_id: run.run_id,
                snapshot_id: run.snapshot_id,
                manifest_digest,
            },
            store,
            snapshots,
            source: SourceService::new(&snapshot),
        })
    }

    pub(crate) async fn execute(&self, name: &str, arguments: &str) -> anyhow::Result<Value> {
        anyhow::ensure!(
            arguments.len() <= 16 * 1024,
            "evidence arguments exceed bridge bound"
        );
        let mut value = if name == "evidence" {
            let query: AuditEvidenceQuery = serde_json::from_str(arguments)?;
            retained_evidence(
                &self.store,
                &self.snapshots,
                AuditEvidenceParams {
                    run_id: self.identity.run_id.clone(),
                    query,
                },
            )
            .await?
        } else {
            anyhow::ensure!(TOOL_NAMES.contains(&name), "unknown retained evidence tool");
            self.source
                .execute(name, arguments)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
        };
        if let Some(identity) = value.get("identity") {
            anyhow::ensure!(
                *identity == serde_json::to_value(&self.identity)?,
                "retained audit binding changed"
            );
        }
        value["identity"] = json!(self.identity);
        anyhow::ensure!(
            value.to_string().len() <= MAX_RESULT_BYTES,
            "evidence result exceeds 500000 encoded bytes; use limit or maxBytes and continue with nextOffset"
        );
        Ok(value)
    }
}

pub(crate) fn tool_names() -> Vec<String> {
    TOOL_NAMES
        .into_iter()
        .chain(["evidence"])
        .map(|name| format!("mcp__audex_source__{name}"))
        .collect()
}
