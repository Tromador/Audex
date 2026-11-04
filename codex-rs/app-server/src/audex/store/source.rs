//! Read-only source access consumes existing attempt/execution authority; it grants none.
use super::admission::AdmissionIdentity;
use super::*;
use crate::audex::source::SourceAuthority;
use futures::future::BoxFuture;
use std::sync::Arc;

pub(in crate::audex) struct ReadOnlySource {
    pool: SqlitePool,
    identity: AdmissionIdentity,
    snapshot: String,
    manifest: String,
}

impl ReadOnlySource {
    pub(in crate::audex) async fn open(
        root: &AbsolutePathBuf,
        identity: AdmissionIdentity,
        snapshot: String,
        manifest: String,
    ) -> anyhow::Result<Arc<Self>> {
        let sqlite =
            codex_state::SqliteConfig::from_sqlite_home_with_nfs_enforcement(root.clone(), false);
        let pool = sqlite
            .open_read_only_pool(&root.join(DATABASE_FILENAME), Some(BUSY_TIMEOUT))
            .await?;
        let applied: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as(
            "SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&pool)
        .await?;
        anyhow::ensure!(
            applied.len() == AUDEX_MIGRATOR.iter().count(),
            "unsupported audit schema"
        );
        for ((version, success, checksum), expected) in applied.iter().zip(AUDEX_MIGRATOR.iter()) {
            anyhow::ensure!(
                *success
                    && *version == expected.version
                    && checksum.as_slice() == expected.checksum.as_ref(),
                "unsupported audit schema"
            );
        }
        let authority = Arc::new(Self {
            pool,
            identity,
            snapshot,
            manifest,
        });
        anyhow::ensure!(authority.check().await?, "source authority refused");
        Ok(authority)
    }

    #[cfg(target_os = "linux")]
    pub(in crate::audex) async fn verify_launch_parent(
        &self,
        parent: u32,
        required: bool,
    ) -> anyhow::Result<()> {
        let Some(launch) =
            super::launch::read_launch(&self.pool, &self.identity.operation_id).await?
        else {
            anyhow::ensure!(!required, "missing launch authority");
            return Ok(());
        };
        anyhow::ensure!(
            launch.execution_id == self.identity.execution_id,
            "launch nonce mismatch"
        );
        let expected = launch
            .child_identity()
            .ok_or_else(|| anyhow::anyhow!("unverified launch parent"))?;
        anyhow::ensure!(
            expected.pid == parent && expected.process_group == parent,
            "bridge has a foreign launch parent"
        );
        let actual =
            codex_app_server_daemon::read_linux_child_identity(parent, &launch.mcp_config_path)?;
        anyhow::ensure!(actual == expected, "launch parent identity changed");
        Ok(())
    }

    async fn check(&self) -> Result<bool, AuditStoreError> {
        // A new read transaction on every check sees the latest committed stop.
        // Share native admission's owner predicate rather than recreate its rules.
        let mut tx = self.pool.begin().await?;
        let allowed = super::admission::owner_active(
            &mut tx,
            &self.identity,
            chrono::Utc::now().timestamp_millis(),
        )
        .await?;
        let bound: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_runs r
            JOIN reviewer_assignments a ON a.run_id=r.id JOIN operations o ON o.assignment_id=a.id
            JOIN audit_execution e ON e.operation_id=o.id JOIN audit_attempts t ON t.assignment_id=a.id
            WHERE r.id=? AND a.id=? AND o.id=? AND r.snapshot_id=? AND r.manifest_digest=?
            AND a.snapshot_id=r.snapshot_id AND e.policy_digest=t.policy_digest)")
            .bind(&self.identity.run_id).bind(&self.identity.assignment_id).bind(&self.identity.operation_id)
            .bind(&self.snapshot).bind(&self.manifest).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(allowed && bound)
    }
}

impl SourceAuthority for ReadOnlySource {
    fn allowed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async { matches!(self.check().await, Ok(true)) })
    }
}
