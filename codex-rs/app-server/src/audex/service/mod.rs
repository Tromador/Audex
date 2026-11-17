//! File-backed audit orchestration. Deliberately not connected to RPC or the TUI.
pub(crate) mod native;
mod record;
mod result;
mod summary;
mod targets;

pub use record::AttemptStatus;
pub use record::ReviewerSelection;
pub use record::RunId;
pub use record::RunRecord;
pub use record::State;
pub use record::Trigger;
pub use record::Usage;
pub(super) use result::schema as output_schema;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use futures::FutureExt;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub enum Input {
    Paths(Vec<PathBuf>),
    Snippet { name: String, text: String },
}

pub struct StartRequest {
    pub input: Input,
    pub brief: String,
    pub guidance: Option<String>,
    pub reviewers: Vec<ReviewerSelection>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Progress {
        run: RunId,
        reviewer: usize,
        status: AttemptStatus,
    },
    Complete {
        run: RunId,
        summary: PathBuf,
    },
    Error {
        run: RunId,
        // None identifies a run-level failure, such as startup or its summary.
        reviewer: Option<usize>,
        reason: String,
    },
}

// The embedding application supplies its own notification sink; no RPC is involved.
pub type Events = Arc<dyn Fn(Event) + Send + Sync>;

/// A runner must observe cancellation, finish its cleanup, and return any partial
/// output. Dropping its future must also release its owned execution resources.
pub trait Reviewer: Send + Sync {
    fn validate(&self, selection: &ReviewerSelection) -> Result<()>;
    fn run<'a>(&'a self, attempt: &'a mut Attempt) -> BoxFuture<'a, Result<Outcome>>;
}

pub enum Completion {
    Succeeded,
    Failed(String),
    Cancelled,
    Interrupted(String),
}

pub struct Outcome {
    pub completion: Completion,
    pub output: Option<Vec<u8>>,
    pub usage: Option<Usage>,
}

/// Passed exclusively to the owning reviewer task. All attempt files have this
/// one writer; the supervisor reads them only after the task has finished.
pub struct Attempt {
    pub run: Arc<RunRecord>,
    pub reviewer_index: usize,
    pub cancellation: CancellationToken,
    directory: PathBuf,
    status: AttemptStatus,
    events: Events,
}

impl Attempt {
    pub fn status(&self) -> &AttemptStatus {
        &self.status
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn progress(&mut self, tool_calls: u64) -> Result<()> {
        self.status.tool_calls = tool_calls;
        self.publish()
    }

    pub fn permission_summary(&mut self, summary: String) -> Result<()> {
        self.status.reviewer.permission_summary = summary;
        self.publish()
    }

    pub fn native_thread(&mut self, id: String, rollout: impl Into<Option<PathBuf>>) -> Result<()> {
        self.status.native_thread_id = Some(id);
        self.status.rollout_path = rollout.into();
        self.publish()
    }

    pub fn write_output(&mut self, output: &[u8]) -> Result<()> {
        record::write_atomic(&self.directory.join("output.txt"), output)
    }

    pub fn write_transcript(&mut self, transcript: &[u8]) -> Result<()> {
        record::write_atomic(&self.directory.join("transcript.jsonl"), transcript)
    }

    pub fn write_stderr(&mut self, stderr: &[u8]) -> Result<()> {
        record::write_atomic(&self.directory.join("stderr.log"), stderr)
    }

    fn publish(&self) -> Result<()> {
        record::write_json(&self.directory.join("status.json"), &self.status)?;
        (self.events)(Event::Progress {
            run: self.run.id,
            reviewer: self.reviewer_index,
            status: self.status.clone(),
        });
        Ok(())
    }
}

struct ReviewerHandle {
    cancellation: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
    // Only process memory; restart never reconstructs an automatic retry budget.
    replacement_available: Arc<AtomicBool>,
}

struct RunHandle {
    record: Arc<RunRecord>,
    cancellation: CancellationToken,
    reviewers: HashMap<usize, ReviewerHandle>,
    remaining: Arc<Mutex<usize>>,
}

pub struct AuditService {
    runs_root: PathBuf,
    runner: Arc<dyn Reviewer>,
    events: Events,
    runs: HashMap<RunId, RunHandle>,
}

impl AuditService {
    /// One service owns the run root. Call at startup, before launching any work.
    pub fn open(codex_home: &Path, runner: Arc<dyn Reviewer>, events: Events) -> Result<Self> {
        let runs_root = codex_home.join("audex/runs");
        record::reconcile(&runs_root, |id, error| {
            (events)(Event::Error {
                run: id,
                reviewer: None,
                reason: format!("Startup reconciliation failed: {error:#}"),
            });
        })?;
        Ok(Self {
            runs_root,
            runner,
            events,
            runs: HashMap::new(),
        })
    }

    pub fn run_directory(&self, id: RunId) -> PathBuf {
        self.runs_root.join(id.to_string())
    }

    pub fn start(&mut self, request: StartRequest) -> Result<RunId> {
        ensure!(
            !request.reviewers.is_empty(),
            "at least one reviewer is required"
        );
        for reviewer in &request.reviewers {
            record::validate_label(&reviewer.label)?;
            self.runner.validate(reviewer)?;
        }
        let paths = match &request.input {
            Input::Paths(paths) => Some(targets::resolve(paths)?),
            Input::Snippet { name, .. } => {
                let mut components = Path::new(name).components();
                ensure!(
                    matches!(components.next(), Some(Component::Normal(_)))
                        && components.next().is_none(),
                    "snippet name must be a single filename"
                );
                None
            }
        };
        let id = Uuid::now_v7();
        let directory = self.run_directory(id);
        fs::create_dir(&directory)?;
        let targets = if let Some(paths) = paths {
            paths
        } else {
            let Input::Snippet { name, text } = request.input else {
                unreachable!()
            };
            let input = directory.join("input");
            fs::create_dir(&input)?;
            let path = input.join(name);
            record::write_atomic(&path, text.as_bytes())?;
            targets::resolve(&[path])?
        };
        let run = Arc::new(RunRecord {
            id,
            targets,
            brief: request.brief,
            guidance: request.guidance,
            reviewers: request.reviewers,
            created_at: record::now(),
        });
        record::write_json(&directory.join("run.json"), &*run)?;
        let mut handle = RunHandle {
            record: run.clone(),
            cancellation: CancellationToken::new(),
            reviewers: HashMap::new(),
            remaining: Arc::new(Mutex::new(0)),
        };
        // Prepare every queued record before starting tasks; a setup error cannot
        // leave an unowned task running. No reviewer waits for another reviewer.
        let mut queued = Vec::new();
        for index in 0..run.reviewers.len() {
            queued.push(prepare(
                &directory,
                run.clone(),
                index,
                Trigger::Initial,
                handle.cancellation.child_token(),
                self.events.clone(),
            )?);
        }
        *handle.remaining.lock().expect("run completion lock") = queued.len();
        for attempt in queued {
            let index = attempt.reviewer_index;
            let available = Arc::new(AtomicBool::new(true));
            let reviewer = self.spawn(attempt, available, handle.remaining.clone());
            handle.reviewers.insert(index, reviewer);
        }
        self.runs.insert(id, handle);
        Ok(id)
    }

    pub fn cancel(&self, id: RunId, reviewer: Option<usize>) -> Result<()> {
        let run = self
            .runs
            .get(&id)
            .context("run has no tasks in this process")?;
        if let Some(index) = reviewer {
            run.reviewers
                .get(&index)
                .context("reviewer has no task")?
                .cancellation
                .cancel();
        } else {
            run.cancellation.cancel();
        }
        Ok(())
    }

    pub async fn retry(&mut self, id: RunId, index: usize) -> Result<()> {
        let directory = self.run_directory(id);
        if !self.runs.contains_key(&id) {
            let run: RunRecord = record::read_json(&directory.join("run.json"))?;
            ensure!(run.id == id, "run directory and record disagree");
            self.runs.insert(
                id,
                RunHandle {
                    record: Arc::new(run),
                    cancellation: CancellationToken::new(),
                    reviewers: HashMap::new(),
                    remaining: Arc::new(Mutex::new(0)),
                },
            );
        }
        let run = self.runs.get_mut(&id).expect("loaded run");
        let selection = run
            .record
            .reviewers
            .get(index)
            .context("unknown reviewer")?;
        record::validate_label(&selection.label)?;
        self.runner.validate(selection)?;
        let available = if let Some(previous) = run.reviewers.get_mut(&index) {
            ensure!(
                previous.task.as_ref().is_none_or(JoinHandle::is_finished),
                "reviewer is still running"
            );
            if let Some(task) = previous.task.take() {
                if let Err(error) = task
                    .await
                    .context("previous reviewer task failed")
                    .and_then(|r| r)
                {
                    (self.events)(Event::Error {
                        run: id,
                        reviewer: Some(index),
                        reason: format!("Previous task failed before operator retry: {error:#}"),
                    });
                }
            }
            previous.replacement_available.clone()
        } else {
            // Operator retry can revive a retained run, but never auto-replacement.
            Arc::new(AtomicBool::new(false))
        };
        if run.cancellation.is_cancelled() {
            run.cancellation = CancellationToken::new();
        }
        let remaining = run.remaining.clone();
        // Register the retry while holding the completion lock, so another
        // reviewer cannot publish a completed summary between prepare and spawn.
        let mut pending = remaining.lock().expect("run completion lock");
        let attempt = prepare(
            &directory,
            run.record.clone(),
            index,
            Trigger::OperatorRetry,
            run.cancellation.child_token(),
            self.events.clone(),
        )?;
        *pending += 1;
        drop(pending);
        let reviewer = self.spawn(attempt, available, remaining);
        self.runs
            .get_mut(&id)
            .expect("loaded run")
            .reviewers
            .insert(index, reviewer);
        Ok(())
    }

    /// Join current tasks, including their automatic replacements and summary.
    pub async fn wait(&mut self, id: RunId) -> Result<()> {
        let run = self
            .runs
            .get_mut(&id)
            .context("run has no tasks in this process")?;
        let mut errors = Vec::new();
        for reviewer in run.reviewers.values_mut() {
            if let Some(task) = reviewer.task.as_mut() {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => errors.push(format!("{error:#}")),
                    Err(error) => errors.push(error.to_string()),
                }
                reviewer.task = None;
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }

    pub fn regenerate_summary(&self, id: RunId) -> Result<String> {
        if let Some(run) = self.runs.get(&id) {
            let remaining = run.remaining.lock().expect("run completion lock");
            ensure!(*remaining == 0, "reviewers are still running");
            return summary::render(&self.run_directory(id));
        }
        summary::render(&self.run_directory(id))
    }

    fn spawn(
        &self,
        mut attempt: Attempt,
        available: Arc<AtomicBool>,
        remaining: Arc<Mutex<usize>>,
    ) -> ReviewerHandle {
        let cancellation = attempt.cancellation.clone();
        let replacement_available = available.clone();
        let runner = self.runner.clone();
        let directory = self.run_directory(attempt.run.id);
        let events = self.events.clone();
        let task = tokio::spawn(async move {
            let id = attempt.run.id;
            let index = attempt.reviewer_index;
            let mut result = drive(&mut attempt, runner, available, &directory).await;
            if let Err(error) = &result {
                let reason = format!("Attempt publication failed: {error:#}");
                if matches!(attempt.status.state, State::Queued | State::Running) {
                    attempt.status.finish(State::Failed, Some(reason.clone()));
                }
                (events)(Event::Error {
                    run: id,
                    reviewer: Some(index),
                    reason,
                });
                // Retain the intended terminal state, or fail an attempt whose
                // output could not be saved. Report the original error even if
                // this one best-effort status write succeeds.
                if let Err(error) = attempt.publish() {
                    let reason = format!(
                        "Could not persist terminal status {:?}: {error:#}",
                        attempt.status.state,
                    );
                    (events)(Event::Error {
                        run: id,
                        reviewer: Some(index),
                        reason: reason.clone(),
                    });
                    result = result.context(reason);
                }
            }
            let mut pending = remaining.lock().expect("run completion lock");
            *pending -= 1;
            if *pending == 0 {
                match summary::render(&directory) {
                    Ok(_) => (events)(Event::Complete {
                        run: id,
                        summary: directory.join("summary.md"),
                    }),
                    Err(error) => {
                        let reason = format!("Summary generation failed: {error:#}");
                        (events)(Event::Error {
                            run: id,
                            reviewer: None,
                            reason: reason.clone(),
                        });
                        result = match result {
                            Ok(()) => Err(error.context(reason)),
                            Err(previous) => Err(previous.context(reason)),
                        };
                    }
                }
            }
            result
        });
        ReviewerHandle {
            cancellation,
            task: Some(task),
            replacement_available,
        }
    }
}

impl Drop for AuditService {
    fn drop(&mut self) {
        for run in self.runs.values() {
            run.cancellation.cancel();
            for reviewer in run.reviewers.values() {
                if let Some(task) = &reviewer.task {
                    task.abort();
                }
            }
        }
    }
}

fn prepare(
    directory: &Path,
    run: Arc<RunRecord>,
    index: usize,
    trigger: Trigger,
    cancellation: CancellationToken,
    events: Events,
) -> Result<Attempt> {
    let reviewer = &run.reviewers[index];
    let parent = record::reviewer_dir(directory, index, reviewer);
    fs::create_dir_all(&parent)?;
    let mut number = record::attempts(&parent)?
        .last()
        .map_or(1, |(_, s)| s.attempt + 1);
    let directory = loop {
        let path = parent.join(format!("attempt-{number}"));
        match fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
            Err(error) => return Err(error.into()),
        }
    };
    let status = AttemptStatus::queued(number, trigger, reviewer.clone());
    let attempt = Attempt {
        run,
        reviewer_index: index,
        cancellation,
        directory,
        status,
        events,
    };
    attempt.publish()?;
    Ok(attempt)
}

async fn drive(
    attempt: &mut Attempt,
    runner: Arc<dyn Reviewer>,
    available: Arc<AtomicBool>,
    run_dir: &Path,
) -> Result<()> {
    loop {
        if attempt.cancellation.is_cancelled() {
            attempt
                .status
                .finish(State::Cancelled, Some("Operator cancelled".into()));
            attempt.publish()?;
            return Ok(());
        }
        attempt.status.state = State::Running;
        attempt.status.started_at = Some(record::now());
        attempt.publish()?;
        let outcome = match AssertUnwindSafe(runner.run(attempt)).catch_unwind().await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => Outcome {
                completion: Completion::Failed(error.to_string()),
                output: None,
                usage: None,
            },
            Err(_) => Outcome {
                completion: Completion::Failed("Reviewer task panicked".into()),
                output: None,
                usage: None,
            },
        };
        if let Some(output) = outcome.output {
            attempt.write_output(&output)?;
        }
        if let Some(usage) = outcome.usage {
            record::write_json(&attempt.directory.join("usage.json"), &usage)?;
        }
        let output = record::read_optional(&attempt.directory.join("output.txt"))?;
        if let Some(result) = output
            .as_deref()
            .and_then(|raw| result::collect(raw, &attempt.run.targets))
        {
            record::write_json(&attempt.directory.join("result.json"), &result)?;
        }
        let (state, reason) = if attempt.cancellation.is_cancelled() {
            (State::Cancelled, Some("Operator cancelled".into()))
        } else {
            match outcome.completion {
                Completion::Succeeded => (State::Succeeded, None),
                Completion::Failed(reason) => (State::Failed, Some(reason)),
                Completion::Cancelled => (State::Cancelled, Some("Reviewer cancelled".into())),
                Completion::Interrupted(reason) => (State::Interrupted, Some(reason)),
            }
        };
        attempt.status.finish(state, reason);
        attempt.publish()?;
        let replace = state == State::Failed
            && output.as_ref().is_none_or(Vec::is_empty)
            && attempt.status.trigger != Trigger::AutomaticReplacement
            && !attempt.cancellation.is_cancelled()
            && available.swap(false, Ordering::SeqCst);
        if !replace {
            return Ok(());
        }
        *attempt = prepare(
            run_dir,
            attempt.run.clone(),
            attempt.reviewer_index,
            Trigger::AutomaticReplacement,
            attempt.cancellation.clone(),
            attempt.events.clone(),
        )?;
    }
}

#[cfg(test)]
mod tests;
