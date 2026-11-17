//! Files below the run root are records, never a source inventory.
use super::targets::Targets;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

pub type RunId = Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerSelection {
    pub label: String,
    pub route: String,
    pub model: String,
    pub effort: Option<String>,
    pub permission_summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub targets: Targets,
    pub brief: String,
    pub guidance: Option<String>,
    pub reviewers: Vec<ReviewerSelection>,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Initial,
    AutomaticReplacement,
    OperatorRetry,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptStatus {
    pub attempt: u32,
    pub state: State,
    pub reason: Option<String>,
    pub trigger: Trigger,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub reviewer: ReviewerSelection,
    pub tool_calls: u64,
    pub native_thread_id: Option<String>,
    pub rollout_path: Option<PathBuf>,
}

impl AttemptStatus {
    pub fn queued(attempt: u32, trigger: Trigger, reviewer: ReviewerSelection) -> Self {
        Self {
            attempt,
            state: State::Queued,
            reason: None,
            trigger,
            created_at: now(),
            started_at: None,
            finished_at: None,
            reviewer,
            tool_calls: 0,
            native_thread_id: None,
            rollout_path: None,
        }
    }

    pub fn finish(&mut self, state: State, reason: Option<String>) {
        self.state = state;
        self.reason = reason;
        self.finished_at = Some(now());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Measurement {
    Measured,
    Estimated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub basis: Measurement,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

/// The owner of the destination is its only writer. Tempfiles live beside it,
/// so replacement is a same-filesystem rename and interrupted writes stay private.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("record needs a parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn reviewer_dir(run_dir: &Path, index: usize, reviewer: &ReviewerSelection) -> PathBuf {
    run_dir
        .join("reviewers")
        .join(format!("{:02}-{}", index + 1, reviewer.label))
}

pub fn validate_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "reviewer label must contain only letters, digits, '-' or '_'"
    );
    Ok(())
}

/// Lists only Audex-owned attempt directories. Never used to inspect targets.
pub fn attempts(directory: &Path) -> Result<Vec<(PathBuf, AttemptStatus)>> {
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(number) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.strip_prefix("attempt-"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        // A crash before the queued record is published leaves no attempt to run.
        if !path.join("status.json").try_exists()? {
            continue;
        }
        let status: AttemptStatus = read_json(&path.join("status.json"))?;
        ensure!(
            status.attempt == number,
            "attempt directory and status disagree"
        );
        found.push((path, status));
    }
    found.sort_by_key(|(_, status)| status.attempt);
    Ok(found)
}

/// Called before there are any writers. Reconciliation never launches reviewers.
pub fn reconcile(runs_root: &Path, report: impl Fn(RunId, anyhow::Error)) -> Result<()> {
    fs::create_dir_all(runs_root)?;
    for entry in fs::read_dir(runs_root)? {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        // Errors reading the root or its entries still fail startup. Once an
        // entry identifies a run, damage within it must not block other runs.
        let reconciled = (|| {
            let directory = entry.path();
            if !entry.file_type()?.is_dir() || !directory.join("run.json").try_exists()? {
                return Ok(());
            }
            reconcile_run(&directory, id)
        })();
        if let Err(error) = reconciled {
            report(id, error);
        }
    }
    Ok(())
}

fn reconcile_run(directory: &Path, id: RunId) -> Result<()> {
    let run: RunRecord = read_json(&directory.join("run.json"))?;
    ensure!(run.id == id, "run directory and record disagree");
    let mut changed = false;
    for (index, reviewer) in run.reviewers.iter().enumerate() {
        validate_label(&reviewer.label)?;
        for (attempt_dir, mut status) in attempts(&reviewer_dir(directory, index, reviewer))? {
            if matches!(status.state, State::Queued | State::Running) {
                status.finish(State::Interrupted, Some("Audex restarted".into()));
                write_json(&attempt_dir.join("status.json"), &status)?;
                changed = true;
            }
        }
    }
    if changed {
        super::summary::render(directory)?;
    }
    Ok(())
}
