//! One durable received sequence per attempt. This journal never establishes completion.
use super::*;
use serde::{Deserialize, Serialize};

pub(in crate::audex) const MAX_BATCH_ENTRIES: usize = 64;
pub(in crate::audex) const MAX_BATCH_BYTES: usize = 2 * 1024 * 1024;
pub(in crate::audex) const MAX_ENTRY_BYTES: usize = 1024 * 1024;
pub(in crate::audex) const MAX_CAPTURE_BYTES: i64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::audex) struct CaptureIdentity {
    pub assignment_id: String,
    pub session_id: String,
    pub owner_generation: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::audex) struct ActivityInput {
    pub producer_key: String,
    pub turn_id: String,
    pub item_id: String,
    pub kind: String,
    pub payload: String,
    pub text_offset: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::audex) struct ActivityRecord {
    pub sequence: i64,
    pub session_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub kind: String,
    pub payload: String,
    pub payload_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::audex) struct CaptureState {
    pub disposition: String,
    pub sequence: i64,
    pub cutoff: Option<i64>,
    pub started: bool,
    pub finished: bool,
    pub gap: Option<String>,
    pub failure: Option<String>,
    pub truncated: bool,
}

#[derive(Debug)]
pub(in crate::audex) struct Appended {
    pub sequence: i64,
    pub fenced: bool,
    pub truncated: bool,
}

pub(super) async fn capture_allowed(
    connection: &mut sqlx::SqliteConnection,
    identity: &CaptureIdentity,
) -> Result<bool, AuditStoreError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_attempts t
        JOIN audit_reviewers v ON v.id=t.reviewer_id JOIN audit_runs r ON r.id=t.run_id
        JOIN operations o ON o.assignment_id=t.assignment_id JOIN audit_execution e ON e.operation_id=o.id
        WHERE t.assignment_id=? AND t.owner_generation=? AND e.thread_id=?
        AND v.current_assignment_id=t.assignment_id AND t.disposition='active' AND t.output_cutoff IS NULL AND t.capture_failure IS NULL
        AND r.control_intent='active' AND r.phase NOT IN ('completed','completed_with_failures','failed','cancelled'))")
        .bind(&identity.assignment_id).bind(identity.owner_generation).bind(&identity.session_id)
        .fetch_one(connection).await?)
}

pub(super) async fn append_batch(
    connection: &mut sqlx::SqliteConnection,
    identity: &CaptureIdentity,
    inputs: &[ActivityInput],
) -> Result<Appended, AuditStoreError> {
    if inputs.len() > MAX_BATCH_ENTRIES
        || inputs.iter().map(|e| e.payload.len()).sum::<usize>() > MAX_BATCH_BYTES
    {
        return Err(AuditStoreError::InvalidInput(
            "activity batch exceeds its bound".to_owned(),
        ));
    }
    let (mut sequence, mut bytes, mut truncated): (i64,i64,bool) = sqlx::query_as(
        "SELECT output_sequence,output_bytes,capture_truncated FROM audit_attempts WHERE assignment_id=?")
        .bind(&identity.assignment_id).fetch_one(&mut *connection).await?;
    if !capture_allowed(connection, identity).await? {
        return Ok(Appended {
            sequence,
            fenced: true,
            truncated,
        });
    }
    for input in inputs {
        validate_nonempty("activity producer key", &input.producer_key, 1024)?;
        if input.payload.len() > MAX_ENTRY_BYTES
            || input.turn_id.len() > 512
            || input.item_id.len() > 512
            || !matches!(
                input.kind.as_str(),
                "assistant_delta"
                    | "assistant_message"
                    | "tool_start"
                    | "tool_result"
                    | "status"
                    | "gap"
                    | "truncation"
            )
        {
            return Err(AuditStoreError::InvalidInput(
                "invalid activity entry".to_owned(),
            ));
        }
        let source_digest = sha256_hex(&serde_json::to_vec(input)?);
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT source_digest FROM audit_activity WHERE assignment_id=? AND producer_key=?",
        )
        .bind(&identity.assignment_id)
        .bind(&input.producer_key)
        .fetch_optional(&mut *connection)
        .await?;
        if let Some(previous) = previous {
            if previous != source_digest {
                return Err(AuditStoreError::IdempotencyConflict(
                    input.producer_key.clone(),
                ));
            }
            continue;
        }
        let mut payload = input.payload.clone();
        if input.kind == "assistant_delta" || input.kind == "assistant_message" {
            let item: Option<(i64,bool)> = sqlx::query_as("SELECT text_bytes,complete FROM audit_activity_items WHERE assignment_id=? AND session_id=? AND turn_id=? AND item_id=?")
                .bind(&identity.assignment_id).bind(&identity.session_id).bind(&input.turn_id).bind(&input.item_id)
                .fetch_optional(&mut *connection).await?;
            let (offset, complete) = item.unwrap_or((0, false));
            if complete {
                return Err(AuditStoreError::InvalidTransition(
                    "activity item is already complete".to_owned(),
                ));
            }
            if input.kind == "assistant_delta" {
                if input.text_offset != Some(offset) {
                    return Err(AuditStoreError::InvalidTransition(
                        "activity delta offset does not match its durable prefix".to_owned(),
                    ));
                }
            } else {
                let prefix: Vec<String> = sqlx::query_scalar("SELECT payload FROM audit_activity WHERE assignment_id=? AND session_id=? AND turn_id=? AND item_id=? AND kind IN ('assistant_delta','assistant_message') ORDER BY sequence")
                    .bind(&identity.assignment_id).bind(&identity.session_id).bind(&input.turn_id).bind(&input.item_id)
                    .fetch_all(&mut *connection).await?;
                let prefix = prefix.concat();
                if !payload.starts_with(&prefix) {
                    return Err(AuditStoreError::InvalidTransition(
                        "complete message contradicts the received prefix".to_owned(),
                    ));
                }
                payload = payload[prefix.len()..].to_owned();
            }
            if bytes + payload.len() as i64 <= MAX_CAPTURE_BYTES && sequence < 65_536 {
                sqlx::query("INSERT INTO audit_activity_items(assignment_id,session_id,turn_id,item_id,text_bytes,complete) VALUES(?,?,?,?,?,?) ON CONFLICT(assignment_id,session_id,turn_id,item_id) DO UPDATE SET text_bytes=excluded.text_bytes,complete=excluded.complete")
                    .bind(&identity.assignment_id).bind(&identity.session_id).bind(&input.turn_id).bind(&input.item_id)
                    .bind(offset + payload.len() as i64).bind(input.kind=="assistant_message").execute(&mut *connection).await?;
            }
        }
        if bytes + payload.len() as i64 > MAX_CAPTURE_BYTES || sequence >= 65_536 {
            truncated = true;
            sqlx::query("UPDATE audit_attempts SET capture_truncated=1,capture_gap=COALESCE(capture_gap,'capture limit reached') WHERE assignment_id=?")
                .bind(&identity.assignment_id).execute(&mut *connection).await?;
            break;
        }
        sequence += 1;
        let digest = sha256_hex(payload.as_bytes());
        let record_digest = sha256_hex(&serde_json::to_vec(&serde_json::json!([
            identity.assignment_id,
            sequence,
            input.producer_key,
            identity.session_id,
            input.turn_id,
            input.item_id,
            input.kind,
            payload,
            digest,
            source_digest,
            input.text_offset
        ]))?);
        sqlx::query("INSERT INTO audit_activity(assignment_id,sequence,producer_key,session_id,turn_id,item_id,kind,payload,payload_digest,source_digest,text_offset,record_digest,created_at_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&identity.assignment_id).bind(sequence).bind(&input.producer_key).bind(&identity.session_id)
            .bind(&input.turn_id).bind(&input.item_id).bind(&input.kind).bind(&payload).bind(&digest).bind(&source_digest)
            .bind(input.text_offset).bind(record_digest).bind(now_millis()).execute(&mut *connection).await?;
        bytes += payload.len() as i64;
        if input.kind == "gap" || input.kind == "truncation" {
            sqlx::query("UPDATE audit_attempts SET capture_gap=COALESCE(capture_gap,?),capture_truncated=capture_truncated OR ? WHERE assignment_id=?")
                .bind(&payload).bind(input.kind=="truncation").bind(&identity.assignment_id).execute(&mut *connection).await?;
        }
    }
    sqlx::query("UPDATE audit_attempts SET output_sequence=?,output_bytes=? WHERE assignment_id=?")
        .bind(sequence)
        .bind(bytes)
        .bind(&identity.assignment_id)
        .execute(connection)
        .await?;
    Ok(Appended {
        sequence,
        fenced: false,
        truncated,
    })
}

impl AuditStore {
    pub(in crate::audex) async fn register_capture(
        &self,
        identity: &CaptureIdentity,
    ) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !capture_allowed(&mut tx, identity).await? {
            tx.commit().await?;
            return Ok(false);
        }
        sqlx::query("UPDATE audit_attempts SET capture_started=1 WHERE assignment_id=?")
            .bind(&identity.assignment_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(in crate::audex) async fn capture_is_active(
        &self,
        identity: &CaptureIdentity,
    ) -> Result<bool, AuditStoreError> {
        capture_allowed(&mut *self.pool.acquire().await?, identity).await
    }

    pub(in crate::audex) async fn append_activity(
        &self,
        identity: &CaptureIdentity,
        inputs: &[ActivityInput],
    ) -> Result<Appended, AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let appended = append_batch(&mut tx, identity, inputs).await?;
        tx.commit().await?;
        Ok(appended)
    }

    pub(in crate::audex) async fn capture_state(
        &self,
        assignment: &str,
    ) -> Result<CaptureState, AuditStoreError> {
        let row = sqlx::query("SELECT disposition,output_sequence,output_cutoff,capture_started,capture_finished,capture_gap,capture_failure,capture_truncated FROM audit_attempts WHERE assignment_id=?")
            .bind(assignment).fetch_one(&self.pool).await?;
        Ok(CaptureState {
            disposition: row.try_get("disposition")?,
            sequence: row.try_get("output_sequence")?,
            cutoff: row.try_get("output_cutoff")?,
            started: row.try_get("capture_started")?,
            finished: row.try_get("capture_finished")?,
            gap: row.try_get("capture_gap")?,
            failure: row.try_get("capture_failure")?,
            truncated: row.try_get("capture_truncated")?,
        })
    }

    pub(in crate::audex) async fn activity(
        &self,
        assignment: &str,
        after: i64,
        limit: u32,
    ) -> Result<Vec<ActivityRecord>, AuditStoreError> {
        if after < 0 || !(1..=256).contains(&limit) {
            return Err(AuditStoreError::InvalidInput(
                "activity cursor/limit is invalid".to_owned(),
            ));
        }
        let state = self.capture_state(assignment).await?;
        let bound = state.cutoff.unwrap_or(state.sequence);
        if after > bound {
            return Err(AuditStoreError::InvalidInput(
                "activity cursor is beyond the durable prefix".to_owned(),
            ));
        }
        let rows = sqlx::query("SELECT sequence,producer_key,session_id,turn_id,item_id,kind,payload,payload_digest,source_digest,text_offset,record_digest FROM audit_activity WHERE assignment_id=? AND sequence>? AND sequence<=? ORDER BY sequence LIMIT ?")
            .bind(assignment).bind(after).bind(bound).bind(i64::from(limit)).fetch_all(&self.pool).await?;
        if rows.len() as i64 != (bound - after).min(i64::from(limit)) {
            return Err(AuditStoreError::InvalidPersistedState(
                "activity prefix has a missing entry".to_owned(),
            ));
        }
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| {
                let sequence: i64 = row.try_get("sequence")?;
                if sequence != after + index as i64 + 1 {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "activity sequence is not contiguous".to_owned(),
                    ));
                }
                let actual = serde_json::json!([
                    assignment,
                    sequence,
                    row.try_get::<String, _>("producer_key")?,
                    row.try_get::<String, _>("session_id")?,
                    row.try_get::<String, _>("turn_id")?,
                    row.try_get::<String, _>("item_id")?,
                    row.try_get::<String, _>("kind")?,
                    row.try_get::<String, _>("payload")?,
                    row.try_get::<String, _>("payload_digest")?,
                    row.try_get::<String, _>("source_digest")?,
                    row.try_get::<Option<i64>, _>("text_offset")?
                ]);
                if sha256_hex(&serde_json::to_vec(&actual)?)
                    != row.try_get::<String, _>("record_digest")?
                {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "activity record digest mismatch".to_owned(),
                    ));
                }
                let payload: String = row.try_get("payload")?;
                let digest: String = row.try_get("payload_digest")?;
                if sha256_hex(payload.as_bytes()) != digest {
                    return Err(AuditStoreError::InvalidPersistedState(
                        "activity payload digest mismatch".to_owned(),
                    ));
                }
                Ok(ActivityRecord {
                    sequence: row.try_get("sequence")?,
                    session_id: row.try_get("session_id")?,
                    turn_id: row.try_get("turn_id")?,
                    item_id: row.try_get("item_id")?,
                    kind: row.try_get("kind")?,
                    payload,
                    payload_digest: digest,
                })
            })
            .collect()
    }

    pub(in crate::audex) async fn finish_capture(
        &self,
        identity: &CaptureIdentity,
    ) -> Result<(), AuditStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if capture_allowed(&mut tx, identity).await? {
            sqlx::query("UPDATE audit_attempts SET capture_finished=1 WHERE assignment_id=?")
                .bind(&identity.assignment_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(in crate::audex) async fn note_unavailable_capture_owner(
        &self,
        operation: &str,
        owner: &str,
        now: i64,
    ) -> Result<(), AuditStoreError> {
        sqlx::query("UPDATE audit_attempts SET capture_gap=COALESCE(capture_gap,'collector unavailable after lease expiry; uncommitted tail may be missing') WHERE capture_started=1 AND capture_finished=0 AND disposition='active' AND output_cutoff IS NULL AND assignment_id IN (SELECT assignment_id FROM operations WHERE id=? AND lease_owner<>? AND lease_expires_at_ms<=?)")
            .bind(operation).bind(owner).bind(now).execute(&self.pool).await?;
        Ok(())
    }

    pub(in crate::audex) async fn note_capture_failure(
        &self,
        assignment: &str,
        reason: &str,
    ) -> Result<(), AuditStoreError> {
        sqlx::query("UPDATE audit_attempts SET capture_gap=COALESCE(capture_gap,?),capture_failure=COALESCE(capture_failure,?),capture_truncated=capture_truncated OR ? WHERE assignment_id=? AND disposition='active' AND output_cutoff IS NULL")
            .bind(reason).bind(reason).bind(reason.contains("truncat")).bind(assignment).execute(&self.pool).await?;
        Ok(())
    }
}
