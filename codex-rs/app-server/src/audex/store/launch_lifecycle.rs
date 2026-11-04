//! Operational evidence on the original launch. No new workflow/control authority.
use super::admission::{AdmissionIdentity, owner_active};
use super::*;

pub(in crate::audex) struct LaunchLifecycle {
    pub daemon: Option<String>,
    pub authority: Option<String>,
    pub ownership: String,
    pub cleanup_finished: bool,
}

impl AuditStore {
    pub(in crate::audex) async fn local_execution_view(
        &self,
        operation: &str,
    ) -> Result<Option<codex_app_server_protocol::AuditLocalExecution>, AuditStoreError> {
        let row = sqlx::query("SELECT local_status,local_stop_requested_at_ms,local_stop_confirmed_at_ms,cleanup_finished_at_ms FROM audit_client_launches WHERE operation_id=?")
            .bind(operation).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(codex_app_server_protocol::AuditLocalExecution {
                status: row.try_get("local_status")?,
                stop_requested_at_ms: row.try_get("local_stop_requested_at_ms")?,
                stop_confirmed_at_ms: row.try_get("local_stop_confirmed_at_ms")?,
                cleanup_finished_at_ms: row.try_get("cleanup_finished_at_ms")?,
            })
        })
        .transpose()
    }

    pub(in crate::audex) async fn unfinished_client_launches(
        &self,
    ) -> Result<Vec<String>, AuditStoreError> {
        Ok(sqlx::query_scalar("SELECT operation_id FROM audit_client_launches WHERE cleanup_finished_at_ms IS NULL ORDER BY intent_at_ms")
            .fetch_all(&self.pool).await?)
    }

    pub(in crate::audex) async fn launch_lifecycle(
        &self,
        operation: &str,
    ) -> Result<LaunchLifecycle, AuditStoreError> {
        let row = sqlx::query("SELECT daemon_identity_json,authority_json,ownership_json,local_stop_requested_at_ms,cleanup_finished_at_ms FROM audit_client_launches WHERE operation_id=?")
            .bind(operation).fetch_one(&self.pool).await?;
        Ok(LaunchLifecycle {
            daemon: row.try_get("daemon_identity_json")?,
            authority: row.try_get("authority_json")?,
            ownership: row.try_get("ownership_json")?,
            cleanup_finished: row
                .try_get::<Option<i64>, _>("cleanup_finished_at_ms")?
                .is_some(),
        })
    }

    /// Serialize loss of local capture authority with prompt writes, capture and controls.
    /// A committed terminal remains recoverable; never damage its evidence to fence a survivor.
    pub(in crate::audex) async fn fence_client_launch(
        &self,
        operation: &str,
        execution: &str,
        lost_owner: bool,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT authority_json,local_stop_requested_at_ms FROM audit_client_launches WHERE operation_id=? AND execution_id=?")
            .bind(operation).bind(execution).fetch_one(&mut *tx).await?;
        let authority: Option<String> = row.try_get("authority_json")?;
        let active = if let Some(raw) = authority {
            let identity: AdmissionIdentity = serde_json::from_str(&raw)?;
            owner_active(&mut tx, &identity, now_millis()).await?
        } else {
            false
        };
        let stop = lost_owner
            || !active
            || row
                .try_get::<Option<i64>, _>("local_stop_requested_at_ms")?
                .is_some();
        if stop {
            if lost_owner {
                // A verified dead owner cannot renew this original execution.
                // Expire its existing lease so normal observe-only recovery can
                // inspect committed terminal evidence immediately, without a new owner.
                sqlx::query("UPDATE operations SET lease_expires_at_ms=MIN(COALESCE(lease_expires_at_ms,0),?) WHERE id=? AND lease_owner IS NOT NULL AND EXISTS(SELECT 1 FROM audit_execution e WHERE e.operation_id=operations.id AND e.thread_id=?)")
                    .bind(now_millis()).bind(operation).bind(execution).execute(&mut *tx).await?;
            }
            sqlx::query("UPDATE audit_client_launches SET local_stop_requested_at_ms=COALESCE(local_stop_requested_at_ms,?),local_status=CASE WHEN cleanup_finished_at_ms IS NULL THEN 'stop_requested' ELSE local_status END WHERE operation_id=? AND execution_id=?")
                .bind(now_millis()).bind(operation).bind(execution).execute(&mut *tx).await?;
            sqlx::query("UPDATE audit_attempts SET capture_failure=COALESCE(capture_failure,'external_capture_owner_stopped'),capture_gap=COALESCE(capture_gap,'external owner stopped; uncommitted tail may be missing') WHERE assignment_id=(SELECT assignment_id FROM operations WHERE id=?) AND capture_finished=0 AND output_cutoff IS NULL AND disposition='active'")
                .bind(operation).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(stop)
    }

    pub(in crate::audex) async fn record_local_cleanup(
        &self,
        operation: &str,
        execution: &str,
        ownership: &str,
        stopped: bool,
        complete: bool,
        status: &str,
    ) -> Result<(), AuditStoreError> {
        // Receipt updates deliberately do not revise a contribution or report.
        sqlx::query("UPDATE audit_client_launches SET ownership_json=?,local_stop_confirmed_at_ms=CASE WHEN ? THEN COALESCE(local_stop_confirmed_at_ms,?) ELSE local_stop_confirmed_at_ms END,cleanup_finished_at_ms=CASE WHEN ? THEN COALESCE(cleanup_finished_at_ms,?) ELSE cleanup_finished_at_ms END,cleanup_checked_at_ms=?,local_status=? WHERE operation_id=? AND execution_id=?")
            .bind(ownership).bind(stopped).bind(now_millis()).bind(complete).bind(now_millis()).bind(now_millis()).bind(status)
            .bind(operation).bind(execution).execute(&self.pool).await?;
        Ok(())
    }
}
