//! Launch evidence belongs to the existing operation, not a separate workflow.
use super::admission::{AdmissionIdentity, owner_active};
use super::*;

#[derive(Clone, Debug)]
pub(in crate::audex) struct LaunchRecord {
    pub operation_id: String,
    pub execution_id: String,
    pub route: String,
    pub configuration_identity: String,
    pub launch_configuration_digest: String,
    pub executable: String,
    pub client_version: String,
    pub mcp_config_path: String,
    pub intent_at_ms: i64,
    pub pid: Option<i64>,
    pub start_ticks: Option<i64>,
    pub boot_id: Option<String>,
    pub process_group: Option<i64>,
    pub parent_pid: Option<i64>,
    pub identity_at_ms: Option<i64>,
    pub prompt_write_started_at_ms: Option<i64>,
}

pub(super) async fn read_launch(
    pool: &SqlitePool,
    operation: &str,
) -> Result<Option<LaunchRecord>, AuditStoreError> {
    let Some(row) = sqlx::query("SELECT * FROM audit_client_launches WHERE operation_id=?")
        .bind(operation)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(LaunchRecord {
        operation_id: row.try_get("operation_id")?,
        execution_id: row.try_get("execution_id")?,
        route: row.try_get("route")?,
        configuration_identity: row.try_get("configuration_identity")?,
        launch_configuration_digest: row.try_get("launch_configuration_digest")?,
        executable: row.try_get("executable")?,
        client_version: row.try_get("client_version")?,
        mcp_config_path: row.try_get("mcp_config_path")?,
        intent_at_ms: row.try_get("intent_at_ms")?,
        pid: row.try_get("pid")?,
        start_ticks: row.try_get("start_ticks")?,
        boot_id: row.try_get("boot_id")?,
        process_group: row.try_get("process_group")?,
        parent_pid: row.try_get("parent_pid")?,
        identity_at_ms: row.try_get("identity_at_ms")?,
        prompt_write_started_at_ms: row.try_get("prompt_write_started_at_ms")?,
    }))
}

impl AuditStore {
    pub(in crate::audex) async fn client_launch(
        &self,
        operation: &str,
    ) -> Result<Option<LaunchRecord>, AuditStoreError> {
        read_launch(&self.pool, operation).await
    }

    pub(in crate::audex) async fn record_launch_intent(
        &self,
        identity: &AdmissionIdentity,
        record: &LaunchRecord,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if identity.operation_id != record.operation_id
            || identity.execution_id != record.execution_id
            || !owner_active(&mut tx, identity, now_millis()).await?
        {
            return Ok(false);
        }
        let changed = sqlx::query("INSERT INTO audit_client_launches
            (operation_id,execution_id,route,configuration_identity,launch_configuration_digest,executable,client_version,mcp_config_path,intent_at_ms)
            SELECT id,?,?,?,?,?,?,?,? FROM operations WHERE id=? AND submission_boundary='unsubmitted'
            ON CONFLICT(operation_id) DO NOTHING")
            .bind(&record.execution_id).bind(&record.route).bind(&record.configuration_identity)
            .bind(&record.launch_configuration_digest).bind(&record.executable).bind(&record.client_version)
            .bind(&record.mcp_config_path).bind(now_millis()).bind(&record.operation_id)
            .execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(changed)
    }

    #[cfg(target_os = "linux")]
    pub(in crate::audex) async fn record_launch_child(
        &self,
        identity: &AdmissionIdentity,
        child: &codex_app_server_daemon::LinuxChildIdentity,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !owner_active(&mut tx, identity, now_millis()).await? {
            return Ok(false);
        }
        let changed = sqlx::query("UPDATE audit_client_launches SET pid=?,start_ticks=?,boot_id=?,process_group=?,parent_pid=?,identity_at_ms=?
            WHERE operation_id=? AND execution_id=? AND pid IS NULL AND prompt_write_started_at_ms IS NULL")
            .bind(i64::from(child.pid)).bind(i64::try_from(child.start_ticks).map_err(|_| AuditStoreError::InvalidInput("child start ticks overflow".into()))?)
            .bind(&child.boot_id).bind(i64::from(child.process_group)).bind(i64::from(child.parent_pid)).bind(now_millis())
            .bind(&identity.operation_id).bind(&identity.execution_id).execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(changed)
    }

    /// Both records cross the boundary in the same writer transaction as controls.
    pub(in crate::audex) async fn begin_client_prompt(
        &self,
        identity: &AdmissionIdentity,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !owner_active(&mut tx, identity, now_millis()).await? {
            return Ok(false);
        }
        let changed = sqlx::query("UPDATE operations SET submission_boundary='submission_started',updated_at_ms=?
            WHERE id=? AND state='dispatching' AND submission_boundary='unsubmitted'
            AND EXISTS(SELECT 1 FROM audit_client_launches l WHERE l.operation_id=operations.id
                AND l.execution_id=? AND l.identity_at_ms IS NOT NULL AND l.prompt_write_started_at_ms IS NULL)")
            .bind(now_millis()).bind(&identity.operation_id).bind(&identity.execution_id)
            .execute(&mut *tx).await?.rows_affected() == 1;
        if changed {
            sqlx::query("UPDATE audit_client_launches SET prompt_write_started_at_ms=? WHERE operation_id=?")
                .bind(now_millis()).bind(&identity.operation_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(changed)
    }
}

#[cfg(target_os = "linux")]
impl LaunchRecord {
    pub(in crate::audex) fn child_identity(
        &self,
    ) -> Option<codex_app_server_daemon::LinuxChildIdentity> {
        Some(codex_app_server_daemon::LinuxChildIdentity {
            pid: self.pid?.try_into().ok()?,
            parent_pid: self.parent_pid?.try_into().ok()?,
            process_group: self.process_group?.try_into().ok()?,
            boot_id: self.boot_id.clone()?,
            start_ticks: self.start_ticks?.try_into().ok()?,
        })
    }
}
