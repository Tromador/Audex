//! Extraction facts are committed with the existing authoritative completion.
//! The assignment references its immutable raw output, snapshot, retained route,
//! execution and model/terminal evidence; reviewer text cannot supply authority.
use super::*;
use codex_app_server_protocol::{AuditExtractionStatus, AuditResultExtraction};

pub(super) async fn read(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
) -> Result<Option<AuditResultExtraction>, AuditStoreError> {
    let row = sqlx::query("SELECT result_digest,result_extraction_json,result_extraction_digest,state FROM reviewer_assignments WHERE id=?")
        .bind(assignment).fetch_one(connection).await?;
    let raw: Option<String> = row.try_get("result_extraction_json")?;
    let digest: Option<String> = row.try_get("result_extraction_digest")?;
    match (raw, digest) {
        (None, None) => Ok(None), // Historical completion keeps its existing contract.
        (Some(raw), Some(digest)) if sha256_hex(raw.as_bytes()) == digest => {
            let value: AuditResultExtraction = serde_json::from_str(&raw)?;
            if value.version != 1
                || row
                    .try_get::<Option<String>, _>("result_digest")?
                    .as_deref()
                    != Some(&value.raw_digest)
                || row.try_get::<String, _>("state")? != "succeeded"
                || (value.status == AuditExtractionStatus::UnstructuredOpinion)
                    != value.validation_error.is_some()
            {
                return Err(AuditStoreError::InvalidPersistedState(
                    "invalid completed opinion extraction binding".into(),
                ));
            }
            Ok(Some(value))
        }
        _ => Err(AuditStoreError::InvalidPersistedState(
            "result extraction digest mismatch".into(),
        )),
    }
}

impl AuditStore {
    pub(in crate::audex) async fn result_extraction(
        &self,
        assignment: &str,
    ) -> Result<Option<AuditResultExtraction>, AuditStoreError> {
        read(&mut *self.pool.acquire().await?, assignment).await
    }
}
