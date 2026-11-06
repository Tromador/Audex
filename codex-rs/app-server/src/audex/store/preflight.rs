//! Existing approvals can be disclosed without opening a migrating/writable store.
use super::*;

pub(in crate::audex) struct RetainedCommission {
    pub run_id: String,
    pub identity: String,
    pub inputs: Vec<(String, String)>,
}

impl AuditStore {
    pub(in crate::audex) async fn read_commission(
        root: &AbsolutePathBuf,
        key: &str,
    ) -> anyhow::Result<Option<RetainedCommission>> {
        if !Self::request_key_exists_read_only(root, key).await? {
            return Ok(None);
        }
        let sqlite =
            codex_state::SqliteConfig::from_sqlite_home_with_nfs_enforcement(root.clone(), false);
        let pool = sqlite
            .open_read_only_pool(&root.join(DATABASE_FILENAME), Some(BUSY_TIMEOUT))
            .await?;
        let result = async {
            let mut tx = pool.begin().await?;
            let record: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT r.id,i.request_identity_digest FROM audit_runs r JOIN audit_inputs i ON i.run_id=r.id WHERE r.request_key=?")
                .bind(key).fetch_optional(&mut *tx).await?;
            let Some((run_id, Some(identity))) = record else { return Ok(None); };
            let records: Vec<(String,String,String,String)> = sqlx::query_as(
                "SELECT i.request_json,i.policy_json,i.input_digest,i.policy_digest FROM audit_reviewers v JOIN audit_attempts a ON a.reviewer_id=v.id AND a.ordinal=1 JOIN audit_attempt_inputs i ON i.assignment_id=a.assignment_id WHERE v.run_id=? ORDER BY v.ordinal")
                .bind(&run_id).fetch_all(&mut *tx).await?;
            let mut inputs = Vec::new();
            for (raw,policy,input_digest,policy_digest) in records {
                anyhow::ensure!(sha256_hex(raw.as_bytes())==input_digest && sha256_hex(policy.as_bytes())==policy_digest,
                    "retained commissioning integrity mismatch");
                inputs.push((raw,policy));
            }
            tx.commit().await?;
            Ok::<_,anyhow::Error>(Some(RetainedCommission{run_id,identity,inputs}))
        }.await;
        pool.close().await;
        result
    }
}
