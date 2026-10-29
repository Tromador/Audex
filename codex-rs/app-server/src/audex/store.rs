use super::report;
use super::result;
use super::result::ResultValidationError;
use super::snapshot::SnapshotManifestV1;
use super::snapshot::manifest::sha256_hex;
use chrono::Utc;
use codex_utils_absolute_path::AbsolutePathBuf;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::migrate::MigrateError;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqliteAutoVacuum;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::NamedTempFile;
use thiserror::Error;
use uuid::Uuid;

static AUDEX_MIGRATOR: Migrator = sqlx_macros::migrate!("./migrations/audex");
const DATABASE_FILENAME: &str = "audit-state.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_ERROR_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct AuditStore {
    root: AbsolutePathBuf,
    database_path: PathBuf,
    pool: SqlitePool,
}

#[derive(Clone, Debug)]
pub(crate) struct CreateRunRequest {
    pub request_key: String,
    pub snapshot_id: String,
    pub manifest_digest: String,
    pub manifest_path: String,
    pub brief_digest: String,
    pub policy_digest: String,
    pub input_digest: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub reviewer_input_digest: String,
    pub operation_input_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreatedRun {
    pub run_id: String,
    pub assignment_id: String,
    pub operation_id: String,
    pub revision: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPhase {
    Created,
    SnapshotBound,
    Reviewing,
    Reporting,
    Reconciling,
    Completed,
    CompletedWithFailures,
    Failed,
    Cancelled,
}

impl RunPhase {
    fn parse(value: &str) -> Result<Self, AuditStoreError> {
        match value {
            "created" => Ok(Self::Created),
            "snapshot_bound" => Ok(Self::SnapshotBound),
            "reviewing" => Ok(Self::Reviewing),
            "reporting" => Ok(Self::Reporting),
            "reconciling" => Ok(Self::Reconciling),
            "completed" => Ok(Self::Completed),
            "completed_with_failures" => Ok(Self::CompletedWithFailures),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(AuditStoreError::InvalidPersistedState(format!(
                "unknown run phase {value}"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::SnapshotBound => "snapshot_bound",
            Self::Reviewing => "reviewing",
            Self::Reporting => "reporting",
            Self::Reconciling => "reconciling",
            Self::Completed => "completed",
            Self::CompletedWithFailures => "completed_with_failures",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::CompletedWithFailures | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssignmentState {
    Planned,
    ThreadReady,
    Dispatching,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

impl AssignmentState {
    fn parse(value: &str) -> Result<Self, AuditStoreError> {
        match value {
            "planned" => Ok(Self::Planned),
            "thread_ready" => Ok(Self::ThreadReady),
            "dispatching" => Ok(Self::Dispatching),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "unknown" => Ok(Self::Unknown),
            _ => Err(AuditStoreError::InvalidPersistedState(format!(
                "unknown assignment state {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationState {
    Planned,
    Dispatching,
    Accepted,
    ObservedTerminal,
    Committed,
    Failed,
    Unknown,
}

impl OperationState {
    fn parse(value: &str) -> Result<Self, AuditStoreError> {
        match value {
            "planned" => Ok(Self::Planned),
            "dispatching" => Ok(Self::Dispatching),
            "accepted" => Ok(Self::Accepted),
            "observed_terminal" => Ok(Self::ObservedTerminal),
            "committed" => Ok(Self::Committed),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            _ => Err(AuditStoreError::InvalidPersistedState(format!(
                "unknown operation state {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuditRunRecord {
    pub run_id: String,
    pub snapshot_id: String,
    pub phase: RunPhase,
    pub control_intent: String,
    pub revision: i64,
    pub outcome: Option<String>,
    pub assignment_id: String,
    pub assignment_state: AssignmentState,
    pub operation_id: String,
    pub operation_state: OperationState,
    pub result_digest: Option<String>,
    pub result_path: Option<String>,
    pub report: Option<ReportRecord>,
    pub findings: Vec<FindingRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FindingRecord {
    pub id: String,
    pub ordinal: i64,
    pub severity: String,
    pub confidence: String,
    pub title: String,
    pub mechanism: String,
    pub impact: String,
    pub remediation: Option<String>,
    pub verification_state: String,
    pub evidence: Vec<EvidenceRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EvidenceRecord {
    pub ordinal: i64,
    pub description: String,
    pub manifest_path: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportRecord {
    pub id: String,
    pub input_digest: String,
    pub relative_path: String,
    pub content_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClaimOutcome {
    Acquired,
    Busy,
    CancellationRequested,
    NotClaimable(OperationState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ObservationOutcome {
    Applied { revision: i64 },
    Duplicate { revision: i64 },
}

#[derive(Debug)]
pub(crate) enum ResultCommitOutcome {
    Committed {
        revision: i64,
        digest: String,
        relative_path: String,
    },
    AlreadyCommitted {
        revision: i64,
        digest: String,
        relative_path: String,
    },
    Rejected {
        revision: i64,
        error: ResultValidationError,
    },
    AlreadyRejected {
        revision: i64,
        digest: String,
        relative_path: Option<String>,
        failure: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReportPublishOutcome {
    Published { revision: i64, report: ReportRecord },
    AlreadyPublished { revision: i64, report: ReportRecord },
}

#[derive(Debug)]
struct OperationContext {
    run_id: String,
    assignment_id: String,
    snapshot_id: String,
    run_phase: RunPhase,
    control_intent: String,
    run_revision: i64,
    assignment_state: AssignmentState,
    operation_state: OperationState,
    lease_owner: Option<String>,
    lease_expires_at_ms: Option<i64>,
}

struct ResultStatus {
    state: AssignmentState,
    digest: Option<String>,
    relative_path: Option<String>,
    failure: Option<String>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "Audex requires FULL synchronous and MEMORY temp_store on every connection; codex-state fixes writable pools to NORMAL and does not expose per-connection temp_store"
)]
async fn open_pool(path: &Path) -> Result<SqlitePool, AuditStoreError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .auto_vacuum(SqliteAutoVacuum::Incremental)
        .busy_timeout(BUSY_TIMEOUT)
        .pragma("temp_store", "MEMORY");
    Ok(SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?)
}

async fn verify_database(pool: &SqlitePool) -> Result<(), AuditStoreError> {
    let checks = sqlx::query_scalar::<_, String>("PRAGMA quick_check")
        .fetch_all(pool)
        .await
        .map_err(|error| AuditStoreError::CorruptDatabase(error.to_string()))?;
    if checks.as_slice() != ["ok"] {
        return Err(AuditStoreError::CorruptDatabase(checks.join("; ")));
    }
    let (journal, synchronous, temp_store, auto_vacuum) = read_pragma_values(pool).await?;
    if journal != "wal" || synchronous != 2 || temp_store != 2 || auto_vacuum != 2 {
        return Err(AuditStoreError::InvalidPersistedState(format!(
            "required SQLite pragmas were not applied: journal_mode={journal}, synchronous={synchronous}, temp_store={temp_store}, auto_vacuum={auto_vacuum}"
        )));
    }
    Ok(())
}

async fn read_pragma_values(pool: &SqlitePool) -> Result<(String, i64, i64, i64), AuditStoreError> {
    let row = sqlx::query(
        "SELECT journal_mode, synchronous, temp_store, auto_vacuum
         FROM pragma_journal_mode, pragma_synchronous, pragma_temp_store, pragma_auto_vacuum",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        row.try_get("journal_mode")?,
        row.try_get("synchronous")?,
        row.try_get("temp_store")?,
        row.try_get("auto_vacuum")?,
    ))
}

async fn read_created_run_by_request_key(
    connection: &mut sqlx::SqliteConnection,
    request_key: &str,
) -> Result<Option<CreatedRun>, AuditStoreError> {
    Ok(sqlx::query(
        "SELECT r.id AS run_id, r.revision, a.id AS assignment_id, o.id AS operation_id
         FROM audit_runs r
         JOIN reviewer_assignments a ON a.run_id=r.id AND a.ordinal=0
         JOIN operations o ON o.assignment_id=a.id
         WHERE r.request_key=?",
    )
    .bind(request_key)
    .fetch_optional(connection)
    .await?
    .map(|row| CreatedRun {
        run_id: row.get("run_id"),
        assignment_id: row.get("assignment_id"),
        operation_id: row.get("operation_id"),
        revision: row.get("revision"),
    }))
}

async fn verify_idempotent_create(
    connection: &mut sqlx::SqliteConnection,
    request: &CreateRunRequest,
    existing: &CreatedRun,
) -> Result<(), AuditStoreError> {
    let row = sqlx::query(
        "SELECT r.snapshot_id, r.manifest_digest, r.manifest_path, r.brief_digest,
                r.policy_digest, r.input_digest, a.provider, a.model,
                a.reasoning_effort, a.reviewer_input_digest, o.input_digest AS operation_input_digest
         FROM audit_runs r
         JOIN reviewer_assignments a ON a.run_id=r.id AND a.ordinal=0
         JOIN operations o ON o.assignment_id=a.id
         WHERE r.id=?",
    )
    .bind(&existing.run_id)
    .fetch_one(connection)
    .await?;
    let matches = row.get::<String, _>("snapshot_id") == request.snapshot_id
        && row.get::<String, _>("manifest_digest") == request.manifest_digest
        && row.get::<String, _>("manifest_path") == request.manifest_path
        && row.get::<String, _>("brief_digest") == request.brief_digest
        && row.get::<String, _>("policy_digest") == request.policy_digest
        && row.get::<String, _>("input_digest") == request.input_digest
        && row.get::<String, _>("provider") == request.provider
        && row.get::<String, _>("model") == request.model
        && row.get::<Option<String>, _>("reasoning_effort") == request.reasoning_effort
        && row.get::<String, _>("reviewer_input_digest") == request.reviewer_input_digest
        && row.get::<String, _>("operation_input_digest") == request.operation_input_digest;
    if !matches {
        return Err(AuditStoreError::IdempotencyConflict(
            request.request_key.clone(),
        ));
    }
    Ok(())
}

async fn read_run_state<'e, E>(
    executor: E,
    run_id: &str,
) -> Result<(RunPhase, i64), AuditStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query("SELECT phase, revision FROM audit_runs WHERE id=?")
        .bind(run_id)
        .fetch_one(executor)
        .await?;
    Ok((
        RunPhase::parse(row.try_get("phase")?)?,
        row.try_get("revision")?,
    ))
}

async fn read_operation_context<'e, E>(
    executor: E,
    operation_id: &str,
) -> Result<OperationContext, AuditStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT r.id AS run_id, r.snapshot_id, r.phase, r.control_intent, r.revision,
                a.id AS assignment_id, a.state AS assignment_state,
                o.state AS operation_state, o.lease_owner, o.lease_expires_at_ms
         FROM operations o
         JOIN reviewer_assignments a ON a.id=o.assignment_id
         JOIN audit_runs r ON r.id=a.run_id
         WHERE o.id=?",
    )
    .bind(operation_id)
    .fetch_one(executor)
    .await?;
    Ok(OperationContext {
        run_id: row.try_get("run_id")?,
        assignment_id: row.try_get("assignment_id")?,
        snapshot_id: row.try_get("snapshot_id")?,
        run_phase: RunPhase::parse(row.try_get("phase")?)?,
        control_intent: row.try_get("control_intent")?,
        run_revision: row.try_get("revision")?,
        assignment_state: AssignmentState::parse(row.try_get("assignment_state")?)?,
        operation_state: OperationState::parse(row.try_get("operation_state")?)?,
        lease_owner: row.try_get("lease_owner")?,
        lease_expires_at_ms: row.try_get("lease_expires_at_ms")?,
    })
}

async fn read_result_status<'e, E>(
    executor: E,
    assignment_id: &str,
) -> Result<ResultStatus, AuditStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT state, result_digest, result_path, failure_code
         FROM reviewer_assignments WHERE id=?",
    )
    .bind(assignment_id)
    .fetch_one(executor)
    .await?;
    Ok(ResultStatus {
        state: AssignmentState::parse(row.try_get("state")?)?,
        digest: row.try_get("result_digest")?,
        relative_path: row.try_get("result_path")?,
        failure: row.try_get("failure_code")?,
    })
}

fn existing_result_outcome(
    existing: ResultStatus,
    digest: &str,
    revision: i64,
) -> Result<Option<ResultCommitOutcome>, AuditStoreError> {
    match existing.state {
        AssignmentState::Succeeded => {
            if existing.digest.as_deref() != Some(digest) {
                return Err(AuditStoreError::InvalidTransition(
                    "a different result is already committed".to_string(),
                ));
            }
            let relative_path = existing.relative_path.ok_or_else(|| {
                AuditStoreError::InvalidPersistedState(
                    "succeeded assignment has no result path".to_string(),
                )
            })?;
            Ok(Some(ResultCommitOutcome::AlreadyCommitted {
                revision,
                digest: digest.to_string(),
                relative_path,
            }))
        }
        AssignmentState::Failed if existing.failure.as_deref() == Some("invalid_result") => {
            if existing.digest.as_deref() != Some(digest) {
                return Err(AuditStoreError::InvalidTransition(
                    "a different invalid result is already recorded".to_string(),
                ));
            }
            Ok(Some(ResultCommitOutcome::AlreadyRejected {
                revision,
                digest: digest.to_string(),
                relative_path: existing.relative_path,
                failure: "invalid_result".to_string(),
            }))
        }
        AssignmentState::Failed | AssignmentState::Cancelled | AssignmentState::Unknown => {
            Err(AuditStoreError::InvalidTransition(format!(
                "assignment is already terminal in state {:?}",
                existing.state
            )))
        }
        _ => Ok(None),
    }
}

async fn update_run_phase(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    phase: RunPhase,
    revision: i64,
    now: i64,
) -> Result<(), AuditStoreError> {
    sqlx::query("UPDATE audit_runs SET phase=?, revision=?, updated_at_ms=? WHERE id=?")
        .bind(phase.as_str())
        .bind(revision)
        .bind(now)
        .bind(run_id)
        .execute(connection)
        .await?;
    Ok(())
}

async fn update_run_revision(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    revision: i64,
    now: i64,
) -> Result<(), AuditStoreError> {
    sqlx::query("UPDATE audit_runs SET revision=?, updated_at_ms=? WHERE id=?")
        .bind(revision)
        .bind(now)
        .bind(run_id)
        .execute(connection)
        .await?;
    Ok(())
}

async fn insert_findings(
    connection: &mut sqlx::SqliteConnection,
    assignment_id: &str,
    validated: &result::ValidatedAuditResult,
) -> Result<(), AuditStoreError> {
    for (ordinal, (finding, finding_id)) in validated
        .response
        .findings
        .iter()
        .zip(&validated.finding_ids)
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO findings (
                id, assignment_id, ordinal, severity, confidence, title,
                mechanism, impact, remediation, verification_state
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'unverified_model_claim')",
        )
        .bind(finding_id)
        .bind(assignment_id)
        .bind(i64::try_from(ordinal).map_err(|_| {
            AuditStoreError::InvalidInput("finding ordinal exceeds SQLite range".to_string())
        })?)
        .bind(finding.severity.as_str())
        .bind(finding.confidence.as_str())
        .bind(&finding.title)
        .bind(&finding.mechanism)
        .bind(&finding.impact)
        .bind(&finding.remediation)
        .execute(&mut *connection)
        .await?;
        for (evidence_ordinal, evidence) in finding.evidence.iter().enumerate() {
            sqlx::query(
                "INSERT INTO evidence (
                    id, finding_id, ordinal, description, manifest_path,
                    start_byte, end_byte, start_line, end_line
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(result::evidence_id(finding_id, evidence_ordinal))
            .bind(finding_id)
            .bind(i64::try_from(evidence_ordinal).map_err(|_| {
                AuditStoreError::InvalidInput("evidence ordinal exceeds SQLite range".to_string())
            })?)
            .bind(&evidence.description)
            .bind(&evidence.path)
            .bind(i64::try_from(evidence.start_byte).map_err(|_| {
                AuditStoreError::InvalidInput("evidence offset exceeds SQLite range".to_string())
            })?)
            .bind(i64::try_from(evidence.end_byte).map_err(|_| {
                AuditStoreError::InvalidInput("evidence offset exceeds SQLite range".to_string())
            })?)
            .bind(evidence.start_line.map(i64::from))
            .bind(evidence.end_line.map(i64::from))
            .execute(&mut *connection)
            .await?;
        }
    }
    Ok(())
}

async fn read_findings(
    pool: &SqlitePool,
    assignment_id: &str,
) -> Result<Vec<FindingRecord>, AuditStoreError> {
    let rows = sqlx::query(
        "SELECT id, ordinal, severity, confidence, title, mechanism, impact,
                remediation, verification_state
         FROM findings WHERE assignment_id=? ORDER BY ordinal",
    )
    .bind(assignment_id)
    .fetch_all(pool)
    .await?;
    let mut findings = Vec::with_capacity(rows.len());
    for row in rows {
        let finding_id: String = row.try_get("id")?;
        let evidence = sqlx::query(
            "SELECT ordinal, description, manifest_path, start_byte, end_byte,
                    start_line, end_line
             FROM evidence WHERE finding_id=? ORDER BY ordinal",
        )
        .bind(&finding_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(EvidenceRecord {
                ordinal: row.try_get("ordinal")?,
                description: row.try_get("description")?,
                manifest_path: row.try_get("manifest_path")?,
                start_byte: row.try_get("start_byte")?,
                end_byte: row.try_get("end_byte")?,
                start_line: row.try_get("start_line")?,
                end_line: row.try_get("end_line")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        findings.push(FindingRecord {
            id: finding_id,
            ordinal: row.try_get("ordinal")?,
            severity: row.try_get("severity")?,
            confidence: row.try_get("confidence")?,
            title: row.try_get("title")?,
            mechanism: row.try_get("mechanism")?,
            impact: row.try_get("impact")?,
            remediation: row.try_get("remediation")?,
            verification_state: row.try_get("verification_state")?,
            evidence,
        });
    }
    Ok(findings)
}

fn require_revision(expected: i64, actual: i64) -> Result<(), AuditStoreError> {
    if expected != actual {
        return Err(AuditStoreError::RevisionConflict { expected, actual });
    }
    Ok(())
}

fn require_phase(actual: RunPhase, expected: RunPhase) -> Result<(), AuditStoreError> {
    if actual != expected {
        return Err(AuditStoreError::InvalidTransition(format!(
            "expected run phase {}, found {}",
            expected.as_str(),
            actual.as_str()
        )));
    }
    Ok(())
}

fn require_assignment_state(
    actual: AssignmentState,
    expected: AssignmentState,
) -> Result<(), AuditStoreError> {
    if actual != expected {
        return Err(AuditStoreError::InvalidTransition(format!(
            "expected assignment state {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

fn require_operation_state(
    actual: OperationState,
    expected: OperationState,
) -> Result<(), AuditStoreError> {
    if actual != expected {
        return Err(AuditStoreError::InvalidTransition(format!(
            "expected operation state {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

fn validate_create_request(request: &CreateRunRequest) -> Result<(), AuditStoreError> {
    validate_nonempty("request key", &request.request_key, MAX_IDENTIFIER_BYTES)?;
    validate_nonempty("provider", &request.provider, MAX_IDENTIFIER_BYTES)?;
    validate_nonempty("model", &request.model, MAX_IDENTIFIER_BYTES)?;
    if let Some(reasoning_effort) = &request.reasoning_effort {
        validate_nonempty("reasoning effort", reasoning_effort, MAX_IDENTIFIER_BYTES)?;
    }
    super::snapshot::SnapshotId::parse(request.snapshot_id.clone())
        .map_err(|error| AuditStoreError::InvalidInput(error.to_string()))?;
    for (name, digest) in [
        ("manifest digest", &request.manifest_digest),
        ("brief digest", &request.brief_digest),
        ("policy digest", &request.policy_digest),
        ("input digest", &request.input_digest),
        ("reviewer input digest", &request.reviewer_input_digest),
        ("operation input digest", &request.operation_input_digest),
    ] {
        validate_digest(name, digest)?;
    }
    validate_relative_path(&request.manifest_path)?;
    Ok(())
}

fn validate_nonempty(name: &str, value: &str, max_bytes: usize) -> Result<(), AuditStoreError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(AuditStoreError::InvalidInput(format!(
            "{name} must be non-empty and at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn validate_digest(name: &str, digest: &str) -> Result<(), AuditStoreError> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(AuditStoreError::InvalidInput(format!(
            "{name} is not a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), AuditStoreError> {
    if path.contains(['\\', ':']) || path.chars().any(char::is_control) {
        return Err(AuditStoreError::InvalidInput(
            "artefact path contains a non-portable character".to_string(),
        ));
    }
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AuditStoreError::InvalidInput(
            "artefact path must be a normalized relative path".to_string(),
        ));
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, AuditStoreError> {
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

fn publish_immutable(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    expected_digest: &str,
) -> Result<PathBuf, AuditStoreError> {
    let path = safe_join(root, relative)?;
    if path.exists() {
        let existing = std::fs::read(&path)?;
        if sha256_hex(&existing) != expected_digest || existing != bytes {
            return Err(AuditStoreError::InvalidPersistedState(format!(
                "existing artefact {} does not match its identity",
                path.display()
            )));
        }
        return Ok(path);
    }
    let parent = path
        .parent()
        .ok_or_else(|| AuditStoreError::InvalidInput("artefact path has no parent".to_string()))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    make_file_read_only(temporary.path())?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read(&path)?;
            if sha256_hex(&existing) != expected_digest || existing != bytes {
                return Err(AuditStoreError::InvalidPersistedState(format!(
                    "concurrently published artefact {} does not match its identity",
                    path.display()
                )));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    sync_directory(parent)?;
    Ok(path)
}

fn verify_immutable(
    root: &Path,
    relative: &str,
    expected_digest: &str,
) -> Result<Vec<u8>, AuditStoreError> {
    validate_digest("artefact digest", expected_digest)?;
    let path = safe_join(root, relative)?;
    let bytes = std::fs::read(&path)?;
    if sha256_hex(&bytes) != expected_digest {
        return Err(AuditStoreError::InvalidPersistedState(format!(
            "published artefact {} does not match its digest",
            path.display()
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn make_file_read_only(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
}

#[cfg(not(unix))]
fn make_file_read_only(path: &Path) -> Result<(), std::io::Error> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        OpenOptions::new().read(true).open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn bounded_error(mut value: String) -> String {
    if value.len() <= MAX_ERROR_BYTES {
        return value;
    }
    let mut boundary = MAX_ERROR_BYTES;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

fn new_id(kind: &str) -> String {
    format!("audex-{kind}-{}", Uuid::now_v7())
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

#[derive(Debug, Error)]
pub(crate) enum AuditStoreError {
    #[error("invalid audit-store input: {0}")]
    InvalidInput(String),
    #[error("audit-store state transition is invalid: {0}")]
    InvalidTransition(String),
    #[error("run revision conflict: expected {expected}, found {actual}")]
    RevisionConflict { expected: i64, actual: i64 },
    #[error("idempotency key conflicts with a different operation: {0}")]
    IdempotencyConflict(String),
    #[error("persisted audit state is invalid: {0}")]
    InvalidPersistedState(String),
    #[error("audit database failed its integrity check: {0}")]
    CorruptDatabase(String),
    #[error("audit database migration failed: {0}")]
    Migration(#[from] MigrateError),
    #[error("audit database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("audit artefact JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("audit artefact filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

impl AuditStore {
    pub(super) async fn observation_lease_ready(
        &self,
        operation_id: &str,
        owner: &str,
        now: i64,
    ) -> Result<bool, AuditStoreError> {
        let row = sqlx::query("SELECT lease_owner, lease_expires_at_ms FROM operations WHERE id=?")
            .bind(operation_id)
            .fetch_one(&self.pool)
            .await?;
        let held_by: Option<String> = row.try_get("lease_owner")?;
        let expires: Option<i64> = row.try_get("lease_expires_at_ms")?;
        Ok(held_by.as_deref() == Some(owner) || expires.is_none_or(|expiry| expiry <= now))
    }

    pub(super) async fn controller_input(
        &self,
        run_id: &str,
    ) -> Result<(String, String, String, String), AuditStoreError> {
        let row = sqlx::query(
            "SELECT i.request_json, i.policy_json, r.policy_digest, r.manifest_digest, r.input_digest
             FROM audit_inputs i JOIN audit_runs r ON r.id=i.run_id WHERE i.run_id=?",
        ).bind(run_id).fetch_one(&self.pool).await?;
        let request_json: String = row.try_get("request_json")?;
        let input_digest: String = row.try_get("input_digest")?;
        if sha256_hex(request_json.as_bytes()) != input_digest {
            return Err(AuditStoreError::InvalidPersistedState(
                "retained audit input digest mismatch".to_string(),
            ));
        }
        Ok((
            request_json,
            row.try_get("policy_json")?,
            row.try_get("policy_digest")?,
            row.try_get("manifest_digest")?,
        ))
    }

    pub(super) async fn list_run_ids(
        &self,
        after: &str,
        limit: u32,
    ) -> Result<Vec<String>, AuditStoreError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM audit_runs WHERE id>? ORDER BY id LIMIT ?")
                .bind(after)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub(super) fn raw_result(
        &self,
        run: &AuditRunRecord,
    ) -> Result<Option<Vec<u8>>, AuditStoreError> {
        match (&run.result_path, &run.result_digest) {
            (Some(path), Some(digest)) => {
                Ok(Some(verify_immutable(self.root.as_path(), path, digest)?))
            }
            _ => Ok(None),
        }
    }

    /// Resolve work using positive terminal evidence; never turn ambiguity into a retry.
    pub(super) async fn finish_operation(
        &self,
        operation_id: &str,
        expected_revision: i64,
        terminal: AssignmentState,
        reason: &str,
    ) -> Result<i64, AuditStoreError> {
        if !matches!(
            terminal,
            AssignmentState::Failed | AssignmentState::Cancelled | AssignmentState::Unknown
        ) {
            return Err(AuditStoreError::InvalidInput(
                "invalid terminal classification".to_string(),
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let state = read_operation_context(&mut *tx, operation_id).await?;
        // A concurrently committed result wins; repeated terminal observations are harmless.
        if matches!(
            state.assignment_state,
            AssignmentState::Succeeded
                | AssignmentState::Failed
                | AssignmentState::Cancelled
                | AssignmentState::Unknown
        ) {
            tx.commit().await?;
            return Ok(state.run_revision);
        }
        require_revision(expected_revision, state.run_revision)?;
        let (assignment, operation, phase) = match terminal {
            AssignmentState::Cancelled => ("cancelled", "failed", "cancelled"),
            AssignmentState::Unknown => ("unknown", "unknown", "failed"),
            _ => ("failed", "failed", "failed"),
        };
        if terminal == AssignmentState::Cancelled && state.control_intent != "cancel_requested" {
            return Err(AuditStoreError::InvalidTransition(
                "cancellation requires durable intent".to_string(),
            ));
        }
        let now = now_millis();
        sqlx::query("UPDATE operations SET state=?, error=?, retry_classification='never_automatically_retry',
            lease_owner=NULL, lease_expires_at_ms=NULL, updated_at_ms=? WHERE id=?")
            .bind(operation).bind(bounded_error(reason.to_string())).bind(now).bind(operation_id)
            .execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE reviewer_assignments SET state=?, failure_code=?, updated_at_ms=? WHERE id=?",
        )
        .bind(assignment)
        .bind(reason)
        .bind(now)
        .bind(&state.assignment_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE audit_runs SET phase=?, outcome=?, revision=revision+1, updated_at_ms=? WHERE id=?")
            .bind(phase).bind(reason).bind(now).bind(&state.run_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(state.run_revision + 1)
    }

    pub(crate) async fn open(root: AbsolutePathBuf) -> Result<Self, AuditStoreError> {
        std::fs::create_dir_all(root.as_path())?;
        let database_path = root.join(DATABASE_FILENAME).into_path_buf();
        let pool = open_pool(&database_path).await?;
        if let Err(error) = verify_database(&pool).await {
            pool.close().await;
            return Err(error);
        }
        if let Err(error) = AUDEX_MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self {
            root,
            database_path,
            pool,
        })
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) async fn close(&self) {
        self.pool.close().await;
    }

    pub(crate) async fn create_run(
        &self,
        request: &CreateRunRequest,
    ) -> Result<CreatedRun, AuditStoreError> {
        self.create_run_with_input(request, None).await
    }

    pub(super) async fn create_run_with_input(
        &self,
        request: &CreateRunRequest,
        input: Option<(&str, &str)>,
    ) -> Result<CreatedRun, AuditStoreError> {
        validate_create_request(request)?;
        let now = now_millis();
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) =
            read_created_run_by_request_key(&mut transaction, &request.request_key).await?
        {
            verify_idempotent_create(&mut transaction, request, &existing).await?;
            transaction.commit().await?;
            return Ok(existing);
        }

        let created = CreatedRun {
            run_id: new_id("run"),
            assignment_id: new_id("assignment"),
            operation_id: new_id("operation"),
            revision: 0,
        };
        sqlx::query(
            "INSERT INTO audit_runs (
                id, request_key, snapshot_id, manifest_digest, manifest_path,
                brief_digest, policy_digest, input_digest, phase, control_intent,
                revision, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'created', 'active', 0, ?, ?)",
        )
        .bind(&created.run_id)
        .bind(&request.request_key)
        .bind(&request.snapshot_id)
        .bind(&request.manifest_digest)
        .bind(&request.manifest_path)
        .bind(&request.brief_digest)
        .bind(&request.policy_digest)
        .bind(&request.input_digest)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO reviewer_assignments (
                id, run_id, ordinal, snapshot_id, provider, model, reasoning_effort,
                reviewer_input_digest, state, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 0, ?, ?, ?, ?, ?, 'planned', ?, ?)",
        )
        .bind(&created.assignment_id)
        .bind(&created.run_id)
        .bind(&request.snapshot_id)
        .bind(&request.provider)
        .bind(&request.model)
        .bind(&request.reasoning_effort)
        .bind(&request.reviewer_input_digest)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO operations (
                id, assignment_id, kind, state, attempt, input_digest,
                created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'review', 'planned', 0, ?, ?, ?)",
        )
        .bind(&created.operation_id)
        .bind(&created.assignment_id)
        .bind(&request.operation_input_digest)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if let Some((request_json, policy_json)) = input {
            sqlx::query(
                "INSERT INTO audit_inputs(run_id, request_json, policy_json) VALUES (?, ?, ?)",
            )
            .bind(&created.run_id)
            .bind(request_json)
            .bind(policy_json)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(created)
    }

    pub(crate) async fn bind_snapshot(
        &self,
        run_id: &str,
        expected_revision: i64,
    ) -> Result<i64, AuditStoreError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (phase, revision) = read_run_state(&mut *transaction, run_id).await?;
        require_revision(expected_revision, revision)?;
        require_phase(phase, RunPhase::Created)?;
        let next_revision = revision + 1;
        sqlx::query(
            "UPDATE audit_runs
             SET phase='snapshot_bound', revision=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(next_revision)
        .bind(now_millis())
        .bind(run_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(next_revision)
    }

    pub(crate) async fn claim_operation(
        &self,
        operation_id: &str,
        owner: &str,
        now_ms: i64,
        lease_until_ms: i64,
    ) -> Result<ClaimOutcome, AuditStoreError> {
        validate_nonempty("lease owner", owner, MAX_IDENTIFIER_BYTES)?;
        if lease_until_ms <= now_ms {
            return Err(AuditStoreError::InvalidInput(
                "lease expiry must be after the claim time".to_string(),
            ));
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT o.state, o.lease_owner, o.lease_expires_at_ms,
                    r.phase, r.control_intent
             FROM operations o
             JOIN reviewer_assignments a ON a.id=o.assignment_id
             JOIN audit_runs r ON r.id=a.run_id
             WHERE o.id=?",
        )
        .bind(operation_id)
        .fetch_one(&mut *transaction)
        .await?;
        let state = OperationState::parse(row.try_get("state")?)?;
        let phase = RunPhase::parse(row.try_get("phase")?)?;
        let control_intent: String = row.try_get("control_intent")?;
        if control_intent == "cancel_requested" {
            transaction.commit().await?;
            return Ok(ClaimOutcome::CancellationRequested);
        }
        if control_intent != "active" {
            return Err(AuditStoreError::InvalidPersistedState(format!(
                "unknown run control intent {control_intent}"
            )));
        }
        if state != OperationState::Planned || phase != RunPhase::SnapshotBound {
            transaction.commit().await?;
            return Ok(ClaimOutcome::NotClaimable(state));
        }
        let other_active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id<>? AND (
                state IN ('dispatching', 'accepted') OR
                (state='planned' AND lease_owner IS NOT NULL AND lease_expires_at_ms>?)))",
        )
        .bind(operation_id)
        .bind(now_ms)
        .fetch_one(&mut *transaction)
        .await?;
        if other_active {
            transaction.commit().await?;
            return Ok(ClaimOutcome::Busy);
        }
        let existing_owner: Option<String> = row.try_get("lease_owner")?;
        let existing_expiry: Option<i64> = row.try_get("lease_expires_at_ms")?;
        if existing_owner.as_deref() != Some(owner)
            && existing_expiry.is_some_and(|expiry| expiry > now_ms)
        {
            transaction.commit().await?;
            return Ok(ClaimOutcome::Busy);
        }
        sqlx::query(
            "UPDATE operations
             SET lease_owner=?, lease_expires_at_ms=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(owner)
        .bind(lease_until_ms)
        .bind(now_millis())
        .bind(operation_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ClaimOutcome::Acquired)
    }

    pub(crate) async fn record_dispatch(
        &self,
        operation_id: &str,
        owner: &str,
        expected_revision: i64,
        now_ms: i64,
    ) -> Result<i64, AuditStoreError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let state = read_operation_context(&mut *transaction, operation_id).await?;
        require_revision(expected_revision, state.run_revision)?;
        require_phase(state.run_phase, RunPhase::SnapshotBound)?;
        require_assignment_state(state.assignment_state, AssignmentState::Planned)?;
        require_operation_state(state.operation_state, OperationState::Planned)?;
        if state.control_intent != "active" {
            return Err(AuditStoreError::InvalidTransition(
                "dispatch is forbidden after cancellation was requested".to_string(),
            ));
        }
        if state.lease_owner.as_deref() != Some(owner)
            || state
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry <= now_ms)
        {
            return Err(AuditStoreError::InvalidTransition(
                "dispatch requires the caller's unexpired operation lease".to_string(),
            ));
        }
        let next_revision = state.run_revision + 1;
        let now = now_millis();
        sqlx::query(
            "UPDATE operations
             SET state='dispatching', attempt=attempt+1, updated_at_ms=?
             WHERE id=?",
        )
        .bind(now)
        .bind(operation_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE reviewer_assignments SET state='dispatching', updated_at_ms=? WHERE id=?",
        )
        .bind(now)
        .bind(&state.assignment_id)
        .execute(&mut *transaction)
        .await?;
        update_run_phase(
            &mut transaction,
            &state.run_id,
            RunPhase::Reviewing,
            next_revision,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(next_revision)
    }

    pub(crate) async fn record_acceptance(
        &self,
        operation_id: &str,
        expected_revision: i64,
        idempotency_key: &str,
        evidence: &str,
    ) -> Result<ObservationOutcome, AuditStoreError> {
        validate_nonempty(
            "lifecycle idempotency key",
            idempotency_key,
            MAX_IDENTIFIER_BYTES,
        )?;
        validate_nonempty("acceptance evidence", evidence, MAX_ERROR_BYTES)?;
        let evidence_digest = sha256_hex(evidence.as_bytes());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = read_operation_context(&mut *transaction, operation_id).await?;
        if let Some(row) = sqlx::query(
            "SELECT assignment_id, kind, payload_digest
             FROM lifecycle_observations WHERE idempotency_key=?",
        )
        .bind(idempotency_key)
        .fetch_optional(&mut *transaction)
        .await?
        {
            let assignment_id: String = row.try_get("assignment_id")?;
            let kind: String = row.try_get("kind")?;
            let payload_digest: String = row.try_get("payload_digest")?;
            if assignment_id != context.assignment_id
                || kind != "accepted"
                || payload_digest != evidence_digest
            {
                return Err(AuditStoreError::IdempotencyConflict(
                    idempotency_key.to_string(),
                ));
            }
            transaction.commit().await?;
            return Ok(ObservationOutcome::Duplicate {
                revision: context.run_revision,
            });
        }

        require_revision(expected_revision, context.run_revision)?;
        require_phase(context.run_phase, RunPhase::Reviewing)?;
        require_assignment_state(context.assignment_state, AssignmentState::Dispatching)?;
        require_operation_state(context.operation_state, OperationState::Dispatching)?;
        let next_revision = context.run_revision + 1;
        let now = now_millis();
        sqlx::query(
            "INSERT INTO lifecycle_observations (
                idempotency_key, assignment_id, kind, payload_digest, created_at_ms
             ) VALUES (?, ?, 'accepted', ?, ?)",
        )
        .bind(idempotency_key)
        .bind(&context.assignment_id)
        .bind(&evidence_digest)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE operations
             SET state='accepted', acceptance_evidence=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(evidence)
        .bind(now)
        .bind(operation_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("UPDATE reviewer_assignments SET state='running', updated_at_ms=? WHERE id=?")
            .bind(now)
            .bind(&context.assignment_id)
            .execute(&mut *transaction)
            .await?;
        update_run_revision(&mut transaction, &context.run_id, next_revision, now).await?;
        transaction.commit().await?;
        Ok(ObservationOutcome::Applied {
            revision: next_revision,
        })
    }

    pub(crate) async fn request_cancel(
        &self,
        run_id: &str,
        expected_revision: i64,
    ) -> Result<i64, AuditStoreError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT phase, control_intent, revision FROM audit_runs WHERE id=?")
            .bind(run_id)
            .fetch_one(&mut *transaction)
            .await?;
        let phase = RunPhase::parse(row.try_get("phase")?)?;
        let control_intent: String = row.try_get("control_intent")?;
        let revision: i64 = row.try_get("revision")?;
        if phase.is_terminal() {
            return Err(AuditStoreError::InvalidTransition(format!(
                "cannot cancel a run in phase {}",
                phase.as_str()
            )));
        }
        if control_intent == "cancel_requested" {
            transaction.commit().await?;
            return Ok(revision);
        }
        require_revision(expected_revision, revision)?;
        let next_revision = revision + 1;
        sqlx::query(
            "UPDATE audit_runs
             SET control_intent='cancel_requested', revision=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(next_revision)
        .bind(now_millis())
        .bind(run_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(next_revision)
    }

    pub(crate) async fn commit_seeded_result(
        &self,
        operation_id: &str,
        expected_revision: i64,
        manifest: &SnapshotManifestV1,
        raw_result: &[u8],
    ) -> Result<ResultCommitOutcome, AuditStoreError> {
        let digest = sha256_hex(raw_result);
        let initial = read_operation_context(&self.pool, operation_id).await?;
        if initial.snapshot_id != manifest.snapshot_id.as_str() {
            return Err(AuditStoreError::InvalidInput(
                "result manifest does not match the persisted assignment snapshot".to_string(),
            ));
        }
        let existing = read_result_status(&self.pool, &initial.assignment_id).await?;
        if let Some(outcome) = existing_result_outcome(existing, &digest, initial.run_revision)? {
            match &outcome {
                ResultCommitOutcome::AlreadyCommitted {
                    digest,
                    relative_path,
                    ..
                } => {
                    verify_immutable(self.root.as_path(), relative_path, digest)?;
                }
                ResultCommitOutcome::AlreadyRejected {
                    digest,
                    relative_path: Some(relative_path),
                    ..
                } => {
                    verify_immutable(self.root.as_path(), relative_path, digest)?;
                }
                _ => {}
            }
            return Ok(outcome);
        }
        require_revision(expected_revision, initial.run_revision)?;
        require_phase(initial.run_phase, RunPhase::Reviewing)?;
        require_assignment_state(initial.assignment_state, AssignmentState::Running)?;
        require_operation_state(initial.operation_state, OperationState::Accepted)?;

        let relative_path = format!("results/{}/{}.json", initial.assignment_id, digest);
        let published_path = if raw_result.len() <= result::MAX_RAW_RESULT_BYTES {
            publish_immutable(self.root.as_path(), &relative_path, raw_result, &digest)?;
            Some(relative_path.clone())
        } else {
            None
        };
        let validation = result::parse_and_validate(raw_result, &initial.assignment_id, manifest);
        let validated = match validation {
            Ok(validated) => validated,
            Err(error) => {
                let revision = self
                    .reject_result(
                        operation_id,
                        expected_revision,
                        &digest,
                        published_path.as_deref(),
                        &error,
                    )
                    .await?;
                return Ok(ResultCommitOutcome::Rejected { revision, error });
            }
        };

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = read_operation_context(&mut *transaction, operation_id).await?;
        if matches!(
            context.assignment_state,
            AssignmentState::Succeeded | AssignmentState::Failed
        ) {
            let existing = read_result_status(&mut *transaction, &context.assignment_id).await?;
            let outcome = existing_result_outcome(existing, &digest, context.run_revision)?
                .ok_or_else(|| {
                    AuditStoreError::InvalidPersistedState(
                        "terminal assignment has no recorded result".to_string(),
                    )
                })?;
            transaction.commit().await?;
            match &outcome {
                ResultCommitOutcome::AlreadyCommitted {
                    digest,
                    relative_path,
                    ..
                } => {
                    verify_immutable(self.root.as_path(), relative_path, digest)?;
                }
                ResultCommitOutcome::AlreadyRejected {
                    digest,
                    relative_path: Some(relative_path),
                    ..
                } => {
                    verify_immutable(self.root.as_path(), relative_path, digest)?;
                }
                _ => {}
            }
            return Ok(outcome);
        }
        require_revision(expected_revision, context.run_revision)?;
        require_phase(context.run_phase, RunPhase::Reviewing)?;
        require_assignment_state(context.assignment_state, AssignmentState::Running)?;
        require_operation_state(context.operation_state, OperationState::Accepted)?;
        let next_revision = context.run_revision + 1;
        let now = now_millis();
        sqlx::query("UPDATE operations SET state='observed_terminal', updated_at_ms=? WHERE id=?")
            .bind(now)
            .bind(operation_id)
            .execute(&mut *transaction)
            .await?;
        insert_findings(&mut transaction, &context.assignment_id, &validated).await?;
        sqlx::query(
            "UPDATE operations
             SET state='committed', lease_owner=NULL, lease_expires_at_ms=NULL, updated_at_ms=?
             WHERE id=?",
        )
        .bind(now)
        .bind(operation_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE reviewer_assignments
             SET state='succeeded', result_digest=?, result_path=?,
                 result_schema_version=?, summary=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(&digest)
        .bind(&relative_path)
        .bind(i64::from(result::RESULT_SCHEMA_VERSION))
        .bind(&validated.response.summary)
        .bind(now)
        .bind(&context.assignment_id)
        .execute(&mut *transaction)
        .await?;
        update_run_phase(
            &mut transaction,
            &context.run_id,
            RunPhase::Reporting,
            next_revision,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(ResultCommitOutcome::Committed {
            revision: next_revision,
            digest,
            relative_path,
        })
    }

    async fn reject_result(
        &self,
        operation_id: &str,
        expected_revision: i64,
        digest: &str,
        relative_path: Option<&str>,
        error: &ResultValidationError,
    ) -> Result<i64, AuditStoreError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let context = read_operation_context(&mut *transaction, operation_id).await?;
        if context.assignment_state == AssignmentState::Failed {
            let existing = read_result_status(&mut *transaction, &context.assignment_id).await?;
            if existing.digest.as_deref() == Some(digest)
                && existing.failure.as_deref() == Some("invalid_result")
            {
                transaction.commit().await?;
                return Ok(context.run_revision);
            }
        }
        require_revision(expected_revision, context.run_revision)?;
        require_phase(context.run_phase, RunPhase::Reviewing)?;
        require_assignment_state(context.assignment_state, AssignmentState::Running)?;
        require_operation_state(context.operation_state, OperationState::Accepted)?;
        let next_revision = context.run_revision + 1;
        let now = now_millis();
        let error_text = bounded_error(error.to_string());
        sqlx::query(
            "UPDATE operations
             SET state='failed', retry_classification='non_retryable_invalid_result',
                 error=?, lease_owner=NULL, lease_expires_at_ms=NULL, updated_at_ms=?
             WHERE id=?",
        )
        .bind(&error_text)
        .bind(now)
        .bind(operation_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE reviewer_assignments
             SET state='failed', result_digest=?, result_path=?,
                 failure_code='invalid_result', updated_at_ms=?
             WHERE id=?",
        )
        .bind(digest)
        .bind(relative_path)
        .bind(now)
        .bind(&context.assignment_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE audit_runs
             SET phase='completed_with_failures', outcome='invalid_result',
                 revision=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(next_revision)
        .bind(now)
        .bind(&context.run_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(next_revision)
    }

    pub(crate) async fn publish_report(
        &self,
        run_id: &str,
        expected_revision: i64,
    ) -> Result<ReportPublishOutcome, AuditStoreError> {
        if let Some((revision, report)) = self.read_current_report(run_id).await? {
            self.read_report(&report)?;
            return Ok(ReportPublishOutcome::AlreadyPublished { revision, report });
        }
        let input = self.report_input(run_id).await?;
        let rendered = report::render(&input)?;
        let relative_path = format!("reports/{}/{}.md", run_id, rendered.input_digest);
        publish_immutable(
            self.root.as_path(),
            &relative_path,
            &rendered.bytes,
            &rendered.content_digest,
        )?;
        let record = ReportRecord {
            id: rendered.id,
            input_digest: rendered.input_digest,
            relative_path,
            content_digest: rendered.content_digest,
        };

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (phase, revision) = read_run_state(&mut *transaction, run_id).await?;
        if phase == RunPhase::Completed {
            transaction.commit().await?;
            if let Some((revision, report)) = self.read_current_report(run_id).await? {
                self.read_report(&report)?;
                return Ok(ReportPublishOutcome::AlreadyPublished { revision, report });
            }
            return Err(AuditStoreError::InvalidPersistedState(
                "completed run has no current report".to_string(),
            ));
        }
        require_revision(expected_revision, revision)?;
        require_phase(phase, RunPhase::Reporting)?;
        let next_revision = revision + 1;
        let now = now_millis();
        sqlx::query(
            "INSERT INTO reports (
                id, run_id, format_version, input_digest, relative_path,
                content_digest, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(run_id)
        .bind(i64::from(report::REPORT_FORMAT_VERSION))
        .bind(&record.input_digest)
        .bind(&record.relative_path)
        .bind(&record.content_digest)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE audit_runs
             SET phase='completed', outcome='success', current_report_id=?,
                 revision=?, updated_at_ms=?
             WHERE id=?",
        )
        .bind(&record.id)
        .bind(next_revision)
        .bind(now)
        .bind(run_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ReportPublishOutcome::Published {
            revision: next_revision,
            report: record,
        })
    }

    pub(crate) async fn load_run(&self, run_id: &str) -> Result<AuditRunRecord, AuditStoreError> {
        let row = sqlx::query(
            "SELECT r.snapshot_id, r.phase, r.control_intent, r.revision, r.outcome,
                    a.id AS assignment_id, a.state AS assignment_state,
                    a.result_digest, a.result_path,
                    o.id AS operation_id, o.state AS operation_state
             FROM audit_runs r
             JOIN reviewer_assignments a ON a.run_id=r.id
             JOIN operations o ON o.assignment_id=a.id
             WHERE r.id=? AND a.ordinal=0",
        )
        .bind(run_id)
        .fetch_one(&self.pool)
        .await?;
        let assignment_id: String = row.try_get("assignment_id")?;
        let record = AuditRunRecord {
            run_id: run_id.to_string(),
            snapshot_id: row.try_get("snapshot_id")?,
            phase: RunPhase::parse(row.try_get("phase")?)?,
            control_intent: row.try_get("control_intent")?,
            revision: row.try_get("revision")?,
            outcome: row.try_get("outcome")?,
            assignment_id: assignment_id.clone(),
            assignment_state: AssignmentState::parse(row.try_get("assignment_state")?)?,
            operation_id: row.try_get("operation_id")?,
            operation_state: OperationState::parse(row.try_get("operation_state")?)?,
            result_digest: row.try_get("result_digest")?,
            result_path: row.try_get("result_path")?,
            report: self.read_report_record(run_id).await?,
            findings: read_findings(&self.pool, &assignment_id).await?,
        };
        match (&record.result_digest, &record.result_path) {
            (Some(digest), Some(path)) => {
                verify_immutable(self.root.as_path(), path, digest)?;
            }
            (Some(_), None) if record.assignment_state == AssignmentState::Succeeded => {
                return Err(AuditStoreError::InvalidPersistedState(
                    "succeeded assignment has no result artefact".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(AuditStoreError::InvalidPersistedState(
                    "assignment result path has no digest".to_string(),
                ));
            }
            _ => {}
        }
        if let Some(report) = &record.report {
            self.read_report(report)?;
        }
        Ok(record)
    }

    pub(crate) async fn list_non_terminal_runs(&self) -> Result<Vec<String>, AuditStoreError> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM audit_runs
             WHERE phase NOT IN ('completed', 'completed_with_failures', 'failed', 'cancelled')
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub(crate) fn read_report(&self, report: &ReportRecord) -> Result<Vec<u8>, AuditStoreError> {
        verify_immutable(
            self.root.as_path(),
            &report.relative_path,
            &report.content_digest,
        )
    }

    async fn read_current_report(
        &self,
        run_id: &str,
    ) -> Result<Option<(i64, ReportRecord)>, AuditStoreError> {
        let row = sqlx::query(
            "SELECT r.revision, p.id, p.input_digest, p.relative_path, p.content_digest
             FROM audit_runs r
             JOIN reports p ON p.id=r.current_report_id
             WHERE r.id=? AND r.phase='completed'",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.try_get("revision")?,
                ReportRecord {
                    id: row.try_get("id")?,
                    input_digest: row.try_get("input_digest")?,
                    relative_path: row.try_get("relative_path")?,
                    content_digest: row.try_get("content_digest")?,
                },
            ))
        })
        .transpose()
    }

    async fn read_report_record(
        &self,
        run_id: &str,
    ) -> Result<Option<ReportRecord>, AuditStoreError> {
        Ok(sqlx::query(
            "SELECT p.id, p.input_digest, p.relative_path, p.content_digest
             FROM audit_runs r JOIN reports p ON p.id=r.current_report_id
             WHERE r.id=?",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| ReportRecord {
            id: row.get("id"),
            input_digest: row.get("input_digest"),
            relative_path: row.get("relative_path"),
            content_digest: row.get("content_digest"),
        }))
    }

    async fn report_input(&self, run_id: &str) -> Result<report::ReportInput, AuditStoreError> {
        let row = sqlx::query(
            "SELECT r.snapshot_id, r.phase, a.id AS assignment_id, a.state, a.summary
             FROM audit_runs r
             JOIN reviewer_assignments a ON a.run_id=r.id
             WHERE r.id=? AND a.ordinal=0",
        )
        .bind(run_id)
        .fetch_one(&self.pool)
        .await?;
        let phase = RunPhase::parse(row.try_get("phase")?)?;
        require_phase(phase, RunPhase::Reporting)?;
        let state = AssignmentState::parse(row.try_get("state")?)?;
        require_assignment_state(state, AssignmentState::Succeeded)?;
        let assignment_id: String = row.try_get("assignment_id")?;
        let findings = read_findings(&self.pool, &assignment_id).await?;
        Ok(report::ReportInput {
            run_id: run_id.to_string(),
            snapshot_id: row.try_get("snapshot_id")?,
            assignment_id,
            summary: row.try_get("summary")?,
            findings: findings
                .into_iter()
                .map(|finding| report::ReportFinding {
                    id: finding.id,
                    ordinal: finding.ordinal,
                    severity: finding.severity,
                    confidence: finding.confidence,
                    title: finding.title,
                    mechanism: finding.mechanism,
                    impact: finding.impact,
                    remediation: finding.remediation,
                    verification_state: finding.verification_state,
                    evidence: finding
                        .evidence
                        .into_iter()
                        .map(|evidence| report::ReportEvidence {
                            ordinal: evidence.ordinal,
                            description: evidence.description,
                            manifest_path: evidence.manifest_path,
                            start_byte: evidence.start_byte,
                            end_byte: evidence.end_byte,
                            start_line: evidence.start_line,
                            end_line: evidence.end_line,
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    #[cfg(test)]
    pub(super) async fn pragma_values(&self) -> Result<(String, i64, i64, i64), AuditStoreError> {
        read_pragma_values(&self.pool).await
    }

    #[cfg(test)]
    pub(super) async fn applied_migrations(&self) -> Result<Vec<i64>, AuditStoreError> {
        Ok(
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&self.pool)
                .await?,
        )
    }
}
