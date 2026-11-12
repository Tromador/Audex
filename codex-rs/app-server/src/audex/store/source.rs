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
        let pool = read_only_pool(root).await?;
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
        let mut initial_parent = None;
        loop {
            // This read-only bridge cannot renew its launch lease. Waiting ends
            // when the original authority expires, is stopped or loses its parent.
            anyhow::ensure!(
                self.check().await?,
                "source authority refused during launch"
            );
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
            let actual = codex_app_server_daemon::read_linux_child_identity(
                parent,
                &launch.mcp_config_path,
            )?;
            anyhow::ensure!(
                actual.pid == parent && actual.process_group == parent,
                "bridge has a foreign launch parent"
            );
            if let Some(initial) = &initial_parent {
                anyhow::ensure!(
                    initial == &actual,
                    "launch parent identity changed while waiting"
                );
            }
            if let Some(expected) = launch.child_identity() {
                anyhow::ensure!(actual == expected, "launch parent identity changed");
                anyhow::ensure!(self.check().await?, "source authority refused after launch");
                return Ok(());
            }
            if initial_parent.is_none() {
                // One fixed, payload-free operational diagnostic; stdout remains MCP only.
                eprintln!("Audex source bridge waiting for committed launch identity");
                initial_parent = Some(actual);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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

// Shared by both bridge profiles; never creates, migrates or repairs a database.
pub(super) async fn read_only_pool(root: &AbsolutePathBuf) -> anyhow::Result<SqlitePool> {
    let sqlite =
        codex_state::SqliteConfig::from_sqlite_home_with_nfs_enforcement(root.clone(), false);
    let pool = sqlite
        .open_read_only_pool(&root.join(DATABASE_FILENAME), Some(BUSY_TIMEOUT))
        .await?;
    let applied: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
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
    Ok(pool)
}
