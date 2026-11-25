mod regressions;
mod reports;

use super::record::Measurement;
use super::*;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::Semaphore;

const STRUCTURED: &[u8] = b"{\"summary\":\"No concrete defect\",\"findings\":[]}";

enum Step {
    Finish(Completion, Option<Vec<u8>>),
    Wait(Arc<Semaphore>, Vec<u8>),
    ErrorAfterOutput(Vec<u8>),
    BlockRecord(&'static str),
    Panic,
}

struct FakeReviewer {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    calls: Mutex<Vec<(RunId, usize, u32, Trigger)>>,
    entered: Semaphore,
}

impl Default for FakeReviewer {
    fn default() -> Self {
        Self {
            steps: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            entered: Semaphore::new(0),
        }
    }
}

impl FakeReviewer {
    fn script(&self, label: &str, steps: Vec<Step>) {
        self.steps
            .lock()
            .unwrap()
            .insert(label.into(), steps.into());
    }

    async fn started(&self, count: u32) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.acquire_many(count))
            .await
            .expect("reviewers start immediately")
            .unwrap()
            .forget();
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl Reviewer for FakeReviewer {
    fn validate(&self, selection: &ReviewerSelection) -> Result<()> {
        ensure!(selection.route == "fake", "route is not configured");
        Ok(())
    }

    fn run<'a>(&'a self, attempt: &'a mut Attempt) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let step = self
                .steps
                .lock()
                .unwrap()
                .get_mut(&attempt.status.reviewer.label)
                .and_then(VecDeque::pop_front)
                .expect("unexpected reviewer launch");
            self.calls.lock().unwrap().push((
                attempt.run.id,
                attempt.reviewer_index,
                attempt.status.attempt,
                attempt.status.trigger,
            ));
            attempt.progress(3)?;
            attempt.native_thread(
                "fake-thread".into(),
                attempt.directory.join("fake-rollout.jsonl"),
            )?;
            attempt.write_transcript(b"{\"fake\":true}\n")?;
            attempt.write_stderr(b"fake diagnostic\n")?;
            self.entered.add_permits(1);
            let (completion, output) = match step {
                Step::Finish(completion, output) => (completion, output),
                Step::Wait(gate, partial) => {
                    attempt.write_output(&partial)?;
                    tokio::select! {
                        permit = gate.acquire() => {
                            permit?.forget();
                            (Completion::Succeeded, Some(STRUCTURED.to_vec()))
                        }
                        _ = attempt.cancellation.cancelled() => {
                            // Exercise supervisor cancellation precedence over runner errors.
                            (Completion::Failed("runner stopped".into()), None)
                        }
                    }
                }
                Step::ErrorAfterOutput(output) => {
                    attempt.write_output(&output)?;
                    anyhow::bail!("fake transport failed");
                }
                Step::BlockRecord(name) => {
                    if name == "status.json" {
                        fs::rename(
                            attempt.directory.join(name),
                            attempt.directory.join("previous-status.json"),
                        )?;
                    }
                    fs::create_dir(attempt.directory.join(name))?;
                    (Completion::Succeeded, Some(STRUCTURED.to_vec()))
                }
                Step::Panic => panic!("fake reviewer panic"),
            };
            Ok(Outcome {
                completion,
                output,
                structured_output: None,
                usage: Some(Usage {
                    basis: Measurement::Measured,
                    input_tokens: Some(12),
                    output_tokens: Some(4),
                    cost_usd: Some(0.0),
                    cost_basis: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    model_usage: None,
                }),
            })
        })
    }
}

fn selection(label: &str) -> ReviewerSelection {
    ReviewerSelection {
        label: label.into(),
        route: "fake".into(),
        model: "fake-model".into(),
        effort: Some("high".into()),
        permission_summary: "Fake reviewer; no source tools".into(),
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    source: PathBuf,
    fake: Arc<FakeReviewer>,
    events: Arc<Mutex<Vec<Event>>>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("a.rs"), "one\ntwo\nthree")?;
        Ok(Self {
            _temp: temp,
            home,
            source,
            fake: Arc::new(FakeReviewer::default()),
            events: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn service(&self) -> Result<AuditService> {
        let events = self.events.clone();
        AuditService::open(
            &self.home,
            self.fake.clone(),
            Arc::new(move |event| events.lock().unwrap().push(event)),
        )
    }

    fn request(&self, labels: &[&str]) -> StartRequest {
        StartRequest {
            input: Input::Paths(vec![self.source.join("a.rs")]),
            brief: "Review the selected file".into(),
            guidance: Some("Keep minority findings".into()),
            reviewers: labels.iter().map(|label| selection(label)).collect(),
        }
    }

    fn attempts(
        &self,
        id: RunId,
        index: usize,
        label: &str,
    ) -> Result<Vec<(PathBuf, AttemptStatus)>> {
        record::attempts(&record::reviewer_dir(
            &self.home.join("audex/runs").join(id.to_string()),
            index,
            &selection(label),
        ))
    }
}

async fn finished(service: &mut AuditService, id: RunId) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), service.wait(id)).await??;
    Ok(())
}

#[tokio::test]
async fn lifecycle_starts_all_reviewers_and_regenerates_summary_from_files() -> Result<()> {
    let fixture = Fixture::new()?;
    let gate = Arc::new(Semaphore::new(0));
    for label in ["one", "two"] {
        fixture
            .fake
            .script(label, vec![Step::Wait(gate.clone(), b"partial".to_vec())]);
    }
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one", "two"]))?;
    assert_eq!(id.get_version_num(), 7);
    fixture.fake.started(2).await;
    for (index, label) in ["one", "two"].iter().enumerate() {
        let attempts = fixture.attempts(id, index, label)?;
        assert_eq!(attempts[0].1.state, State::Running);
        assert_eq!(attempts[0].1.tool_calls, 3);
    }
    assert!(service.regenerate_summary(id).is_err());
    assert!(service.retry(id, 0).await.is_err());
    gate.add_permits(2);
    finished(&mut service, id).await?;
    let directory = service.run_directory(id);
    let run: RunRecord = record::read_json(&directory.join("run.json"))?;
    assert_eq!(run.targets.paths, vec![fixture.source.join("a.rs")]);
    assert_eq!(
        run.targets.working_directories,
        vec![fixture.source.clone()]
    );
    assert_eq!(run.brief, "Review the selected file");
    assert_eq!(run.guidance.as_deref(), Some("Keep minority findings"));
    assert!(!directory.join("input").exists());
    for (index, label) in ["one", "two"].iter().enumerate() {
        let attempts = fixture.attempts(id, index, label)?;
        assert_eq!(attempts.len(), 1);
        let (path, status) = &attempts[0];
        assert_eq!(status.state, State::Succeeded);
        assert_eq!(status.trigger, Trigger::Initial);
        assert!(status.started_at.is_some() && status.finished_at.is_some());
        assert_eq!(status.reviewer, selection(label));
        assert_eq!(status.native_thread_id.as_deref(), Some("fake-thread"));
        assert_eq!(
            status.rollout_path.as_deref(),
            Some(path.join("fake-rollout.jsonl").as_path())
        );
        assert_eq!(fs::read(path.join("output.txt"))?, STRUCTURED);
        assert_eq!(
            fs::read(path.join("transcript.jsonl"))?,
            b"{\"fake\":true}\n"
        );
        assert_eq!(fs::read(path.join("stderr.log"))?, b"fake diagnostic\n");
        let usage: serde_json::Value = record::read_json(&path.join("usage.json"))?;
        assert_eq!(usage["basis"], "measured");
        assert_eq!(usage["input_tokens"], 12);
        let result: result::CheckedResult = record::read_json(&path.join("result.json"))?;
        assert!(result.findings.is_empty());
        assert_eq!(
            fs::read_dir(path)?.count(),
            6,
            "atomic writes leave no temporary files"
        );
    }
    let summary = fs::read_to_string(directory.join("summary.md"))?;
    assert!(summary.contains("succeeded") && summary.contains("No concrete defect"));
    fs::remove_file(directory.join("summary.md"))?;
    assert_eq!(service.regenerate_summary(id)?, summary);
    assert_eq!(
        fixture
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::Complete { .. }))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn individual_cancellation_preserves_partial_output_and_other_reviewers() -> Result<()> {
    let fixture = Fixture::new()?;
    let gate = Arc::new(Semaphore::new(0));
    fixture.fake.script(
        "one",
        vec![Step::Wait(gate.clone(), b"partial opinion".to_vec())],
    );
    fixture
        .fake
        .script("two", vec![Step::Wait(gate.clone(), Vec::new())]);
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one", "two"]))?;
    fixture.fake.started(2).await;
    service.cancel(id, Some(0))?;
    // Wait for the cancelled runner to observe its token before releasing the other.
    let run = service.runs.get_mut(&id).unwrap();
    run.reviewers
        .get_mut(&0)
        .unwrap()
        .task
        .as_mut()
        .unwrap()
        .await??;
    run.reviewers.get_mut(&0).unwrap().task = None;
    assert_eq!(fixture.attempts(id, 1, "two")?[0].1.state, State::Running);
    gate.add_permits(1);
    finished(&mut service, id).await?;
    let cancelled = fixture.attempts(id, 0, "one")?;
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].1.state, State::Cancelled);
    assert_eq!(
        fs::read(cancelled[0].0.join("output.txt"))?,
        b"partial opinion"
    );
    assert!(cancelled[0].0.join("transcript.jsonl").exists());
    assert_eq!(fixture.attempts(id, 1, "two")?[0].1.state, State::Succeeded);
    assert_eq!(fixture.fake.call_count(), 2);
    let summary = service.regenerate_summary(id)?;
    assert!(
        summary.contains("cancelled")
            && summary.contains("partial opinion")
            && summary.contains("succeeded")
    );
    Ok(())
}

#[tokio::test]
async fn whole_run_cancel_then_operator_retry_retains_previous_attempts() -> Result<()> {
    let fixture = Fixture::new()?;
    let gate = Arc::new(Semaphore::new(0));
    fixture.fake.script(
        "one",
        vec![
            Step::Wait(gate.clone(), b"earlier output".to_vec()),
            Step::Finish(Completion::Succeeded, Some(STRUCTURED.to_vec())),
        ],
    );
    fixture
        .fake
        .script("two", vec![Step::Wait(gate, Vec::new())]);
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one", "two"]))?;
    fixture.fake.started(2).await;
    service.cancel(id, None)?;
    finished(&mut service, id).await?;
    for (index, label) in ["one", "two"].iter().enumerate() {
        assert_eq!(
            fixture.attempts(id, index, label)?[0].1.state,
            State::Cancelled
        );
    }
    let first = fixture.attempts(id, 0, "one")?[0].0.clone();
    let old_status = fs::read(first.join("status.json"))?;
    let request = fs::read(service.run_directory(id).join("run.json"))?;
    service.retry(id, 0).await?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(id, 0, "one")?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].1.trigger, Trigger::OperatorRetry);
    assert_eq!(attempts[1].1.state, State::Succeeded);
    assert_eq!(attempts[1].1.reviewer, attempts[0].1.reviewer);
    assert_eq!(fs::read(first.join("status.json"))?, old_status);
    assert_eq!(fs::read(first.join("output.txt"))?, b"earlier output");
    assert_eq!(
        fs::read(service.run_directory(id).join("run.json"))?,
        request
    );
    assert_eq!(fixture.attempts(id, 1, "two")?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancel_before_launch_does_not_call_or_replace_reviewer() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one"]))?;
    service.cancel(id, None)?;
    finished(&mut service, id).await?;
    assert_eq!(fixture.fake.call_count(), 0);
    let attempts = fixture.attempts(id, 0, "one")?;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].1.state, State::Cancelled);
    Ok(())
}

#[tokio::test]
async fn absent_and_empty_failure_get_exactly_one_replacement_even_after_operator_retry()
-> Result<()> {
    for output in [None, Some(Vec::new())] {
        let fixture = Fixture::new()?;
        fixture.fake.script(
            "one",
            vec![
                Step::Finish(Completion::Failed("first failure".into()), output),
                Step::Finish(Completion::Failed("replacement failure".into()), None),
                Step::Finish(Completion::Failed("operator attempt failed".into()), None),
            ],
        );
        let mut service = fixture.service()?;
        let id = service.start(fixture.request(&["one"]))?;
        finished(&mut service, id).await?;
        let attempts = fixture.attempts(id, 0, "one")?;
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].1.trigger, Trigger::Initial);
        assert_eq!(attempts[1].1.trigger, Trigger::AutomaticReplacement);
        assert!(attempts.iter().all(|(_, s)| s.state == State::Failed));
        assert_eq!(fixture.fake.call_count(), 2);
        service.retry(id, 0).await?;
        finished(&mut service, id).await?;
        assert_eq!(fixture.fake.call_count(), 3);
        let attempts = fixture.attempts(id, 0, "one")?;
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[2].1.trigger, Trigger::OperatorRetry);
    }
    Ok(())
}

#[tokio::test]
async fn replacement_can_succeed_and_retains_failed_first_attempt() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.fake.script(
        "one",
        vec![
            Step::Finish(Completion::Failed("launch failed".into()), None),
            Step::Finish(Completion::Succeeded, Some(STRUCTURED.to_vec())),
        ],
    );
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one"]))?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(id, 0, "one")?;
    assert_eq!(attempts[0].1.state, State::Failed);
    assert_eq!(attempts[1].1.state, State::Succeeded);
    let summary = service.regenerate_summary(id)?;
    assert!(
        summary.contains("launch failed")
            && summary.contains("automatic replacement")
            && summary.contains("succeeded")
    );
    Ok(())
}

#[tokio::test]
async fn output_completed_cancelled_and_interrupted_attempts_are_never_replaced() -> Result<()> {
    let cases = vec![
        (
            Completion::Failed("bad output".into()),
            Some(b"unparseable opinion".to_vec()),
            State::Failed,
        ),
        (
            Completion::Failed("whitespace output".into()),
            Some(b" \n".to_vec()),
            State::Failed,
        ),
        (
            Completion::Failed("failure with findings".into()),
            Some(STRUCTURED.to_vec()),
            State::Failed,
        ),
        (Completion::Succeeded, None, State::Succeeded),
        (Completion::Succeeded, Some(Vec::new()), State::Succeeded),
        (
            Completion::Succeeded,
            Some(STRUCTURED.to_vec()),
            State::Succeeded,
        ),
        (Completion::Cancelled, None, State::Cancelled),
        (
            Completion::Interrupted("runner interrupted".into()),
            None,
            State::Interrupted,
        ),
    ];
    for (completion, output, state) in cases {
        let fixture = Fixture::new()?;
        fixture
            .fake
            .script("one", vec![Step::Finish(completion, output)]);
        let mut service = fixture.service()?;
        let id = service.start(fixture.request(&["one"]))?;
        finished(&mut service, id).await?;
        let attempts = fixture.attempts(id, 0, "one")?;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].1.state, state);
        assert_eq!(fixture.fake.call_count(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn unparseable_nonempty_output_is_retained_and_shown_as_an_opinion() -> Result<()> {
    let fixture = Fixture::new()?;
    let raw = b"A minority opinion.\n# pretend status\n```\n<untrusted>\n";
    fixture.fake.script(
        "one",
        vec![Step::Finish(Completion::Succeeded, Some(raw.to_vec()))],
    );
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one"]))?;
    finished(&mut service, id).await?;
    let path = &fixture.attempts(id, 0, "one")?[0].0;
    assert_eq!(fs::read(path.join("output.txt"))?, raw);
    assert!(!path.join("result.json").exists());
    let summary = service.regenerate_summary(id)?;
    assert!(summary.contains("Unstructured opinion"));
    regressions::assert_literal_html(
        &regressions::markdown_html(&summary),
        std::str::from_utf8(raw)?,
    );
    assert_eq!(fixture.fake.call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn runner_errors_keep_written_output_and_panics_are_terminal() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.fake.script(
        "one",
        vec![Step::ErrorAfterOutput(b"retained opinion".to_vec())],
    );
    fixture.fake.script(
        "two",
        vec![
            Step::Panic,
            Step::Finish(Completion::Failed("replacement failed".into()), None),
        ],
    );
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one", "two"]))?;
    finished(&mut service, id).await?;
    assert_eq!(fixture.attempts(id, 0, "one")?.len(), 1);
    let attempts = fixture.attempts(id, 1, "two")?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].1.state, State::Failed);
    assert_eq!(
        attempts[0].1.reason.as_deref(),
        Some("Reviewer task panicked")
    );
    Ok(())
}

#[tokio::test]
async fn startup_interrupts_only_unfinished_attempts_and_never_relaunches() -> Result<()> {
    let fixture = Fixture::new()?;
    let directory = fixture
        .home
        .join("audex/runs")
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&directory)?;
    let id = Uuid::parse_str(directory.file_name().unwrap().to_str().unwrap())?;
    let states = [
        State::Queued,
        State::Running,
        State::Failed,
        State::Succeeded,
        State::Cancelled,
        State::Interrupted,
    ];
    let run = RunRecord {
        id,
        targets: targets::resolve(std::slice::from_ref(&fixture.source))?,
        brief: "preserved brief".into(),
        guidance: None,
        reviewers: states
            .iter()
            .enumerate()
            .map(|(i, _)| selection(&format!("reviewer-{i}")))
            .collect(),
        created_at: record::now(),
        conversation: None,
    };
    record::write_json(&directory.join("run.json"), &run)?;
    let mut before = Vec::new();
    for (index, state) in states.iter().enumerate() {
        let path = record::reviewer_dir(&directory, index, &run.reviewers[index]).join("attempt-1");
        fs::create_dir_all(&path)?;
        let mut status = AttemptStatus::queued(1, Trigger::Initial, run.reviewers[index].clone());
        status.state = *state;
        record::write_json(&path.join("status.json"), &status)?;
        record::write_atomic(&path.join("output.txt"), b"partial retained output")?;
        before.push(fs::read(path.join("status.json"))?);
    }
    let mut service = fixture.service()?;
    assert_eq!(fixture.fake.call_count(), 0);
    assert!(service.runs.is_empty());
    for (index, state) in states.iter().enumerate() {
        let attempts = fixture.attempts(id, index, &format!("reviewer-{index}"))?;
        let (path, status) = &attempts[0];
        if matches!(state, State::Queued | State::Running) {
            assert_eq!(status.state, State::Interrupted);
            assert_eq!(status.reason.as_deref(), Some("Audex restarted"));
            assert!(status.finished_at.is_some());
        } else {
            assert_eq!(fs::read(path.join("status.json"))?, before[index]);
        }
        assert_eq!(
            fs::read(path.join("output.txt"))?,
            b"partial retained output"
        );
    }
    fixture.fake.script(
        "reviewer-0",
        vec![Step::Finish(
            Completion::Failed("retry failed with no output".into()),
            None,
        )],
    );
    service.retry(id, 0).await?;
    finished(&mut service, id).await?;
    assert_eq!(
        fixture.fake.call_count(),
        1,
        "operator retry after restart cannot auto-replace"
    );
    assert_eq!(fixture.attempts(id, 0, "reviewer-0")?.len(), 2);
    drop(service);
    let _reopened = fixture.service()?;
    assert_eq!(fixture.fake.call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn dropping_a_running_service_leaves_work_for_startup_interruption() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.fake.script(
        "one",
        vec![Step::Wait(Arc::new(Semaphore::new(0)), b"partial".to_vec())],
    );
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one"]))?;
    fixture.fake.started(1).await;
    drop(service);
    tokio::task::yield_now().await;
    let _reopened = fixture.service()?;
    assert_eq!(fixture.fake.call_count(), 1);
    let attempts = fixture.attempts(id, 0, "one")?;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].1.state, State::Interrupted);
    Ok(())
}

#[tokio::test]
async fn snippet_is_operator_text_and_invalid_requests_launch_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut service = fixture.service()?;
    let mut request = fixture.request(&["one"]);
    request.reviewers[0].route = "unconfigured".into();
    assert!(service.start(request).is_err());
    let mut request = fixture.request(&["one"]);
    request.input = Input::Paths(vec![fixture.source.join("absent")]);
    assert!(service.start(request).is_err());
    let mut request = fixture.request(&["one"]);
    request.input = Input::Snippet {
        name: "../escape".into(),
        text: "source".into(),
    };
    assert!(service.start(request).is_err());
    // Invalid requests create nothing, not even the lazily created runs root.
    assert!(!service.runs_root.try_exists()?);
    fixture
        .fake
        .script("one", vec![Step::Finish(Completion::Succeeded, None)]);
    let mut request = fixture.request(&["one"]);
    request.input = Input::Snippet {
        name: "example.rs".into(),
        text: "operator text\n".into(),
    };
    let id = service.start(request)?;
    finished(&mut service, id).await?;
    let run: RunRecord = record::read_json(&service.run_directory(id).join("run.json"))?;
    assert_eq!(
        run.targets.paths,
        vec![service.run_directory(id).join("input/example.rs")]
    );
    assert_eq!(
        run.targets.working_directories,
        vec![service.run_directory(id).join("input")]
    );
    assert_eq!(fs::read(&run.targets.paths[0])?, b"operator text\n");
    Ok(())
}

fn finding(path: &str, start_line: u64, end_line: u64) -> result::Finding {
    result::Finding {
        path: path.into(),
        start_line,
        end_line,
        title: "Claim".into(),
        severity: result::Severity::High,
        confidence: result::Confidence::Medium,
        trigger: "Input condition".into(),
        mechanism: "Unchecked value".into(),
        impact: "Wrong result".into(),
        evidence: "Source claim".into(),
        remediation: "Validate input".into(),
    }
}

#[test]
fn citation_checks_all_statuses_and_named_file_context_without_listing() -> Result<()> {
    use result::CitationStatus::*;
    let fixture = Fixture::new()?;
    let second = fixture._temp.path().join("second");
    fs::create_dir(&second)?;
    fs::write(second.join("context.rs"), "context\r\nlast\n")?;
    fs::write(fixture.source.join("caller.rs"), "caller\n")?;
    fs::write(fixture.source.join("empty.rs"), "")?;
    let targets = targets::resolve(&[fixture.source.join("a.rs"), second.clone()])?;
    for (path, start, end, expected) in [
        ("a.rs".to_owned(), 1, 3, Ok),
        ("caller.rs".to_owned(), 1, 1, Ok),
        (second.join("context.rs").display().to_string(), 2, 2, Ok),
        ("missing.rs".to_owned(), 1, 1, PathNotFound),
        ("../outside.rs".to_owned(), 1, 1, OutsideWorkingDirectories),
        ("a.rs".to_owned(), 0, 1, LinesOutOfRange),
        ("a.rs".to_owned(), 3, 2, LinesOutOfRange),
        ("a.rs".to_owned(), 1, 4, LinesOutOfRange),
        ("empty.rs".to_owned(), 1, 1, LinesOutOfRange),
        (".".to_owned(), 1, 1, PathNotFound),
    ] {
        assert_eq!(
            result::check_citation(&finding(&path, start, end), &targets),
            expected,
            "{path}:{start}-{end}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&second, fixture.source.join("allowed"))?;
        assert_eq!(
            result::check_citation(&finding("allowed/context.rs", 1, 2), &targets),
            Ok
        );
        let outside = fixture._temp.path().join("outside.rs");
        fs::write(&outside, "outside")?;
        std::os::unix::fs::symlink(&outside, fixture.source.join("escape.rs"))?;
        assert_eq!(
            result::check_citation(&finding("escape.rs", 1, 1), &targets),
            OutsideWorkingDirectories
        );
    }
    anyhow::Ok(())
}

#[test]
fn remote_citations_are_scope_checked_without_local_reads() -> Result<()> {
    use result::CitationStatus::*;
    let targets = targets::resolve_mixed(&[PathBuf::from("O:/source/repo/a.rs")], &[Some(false)])?;
    assert_eq!(
        result::check_citation(&finding("a.rs", 1, 7), &targets),
        RemoteUnverified
    );
    assert_eq!(
        result::check_citation(&finding("O:/source/repo/context.rs", 1, 3), &targets),
        RemoteUnverified
    );
    assert_eq!(
        result::check_citation(&finding("O:/source/elsewhere.rs", 1, 3), &targets),
        OutsideWorkingDirectories
    );
    assert_eq!(
        result::check_citation(&finding("../elsewhere.rs", 1, 3), &targets),
        OutsideWorkingDirectories
    );
    assert_eq!(
        result::check_citation(&finding("a.rs", 0, 1), &targets),
        LinesOutOfRange
    );
    anyhow::Ok(())
}

#[test]
fn line_schema_parser_keeps_all_finding_fields() -> Result<()> {
    let fixture = Fixture::new()?;
    let targets = targets::resolve(std::slice::from_ref(&fixture.source))?;
    let result = result::ReviewResult {
        summary: "Review claim".into(),
        findings: vec![finding("a.rs", 1, 3)],
    };
    let raw = serde_json::to_string(&result)?;
    for text in [raw.clone(), format!("\u{feff}```json\r\n{raw}\r\n```\n")] {
        let checked = result::collect(text.as_bytes(), &targets).expect("structured opinion");
        assert_eq!(checked.findings[0].citation, result::CitationStatus::Ok);
        assert_eq!(checked.findings[0].finding.trigger, "Input condition");
    }
    assert!(result::collect(b"{\"summary\":\"missing findings\"}", &targets).is_none());
    assert!(result::collect(b"\xff", &targets).is_none());
    let schema = result::schema().to_string();
    assert!(
        schema.contains("start_line") && schema.contains("end_line") && schema.contains("trigger")
    );
    assert!(!schema.contains("start_byte") && !schema.contains("snapshot"));
    Ok(())
}

#[tokio::test]
async fn structured_findings_persist_citation_flags_and_render_all_fields() -> Result<()> {
    let fixture = Fixture::new()?;
    let result = result::ReviewResult {
        summary: "Reviewer claim".into(),
        findings: vec![finding("a.rs", 1, 3), finding("missing.rs", 1, 1)],
    };
    let raw = serde_json::to_vec(&result)?;
    fixture.fake.script(
        "one",
        vec![Step::Finish(Completion::Succeeded, Some(raw.clone()))],
    );
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one"]))?;
    finished(&mut service, id).await?;
    let path = &fixture.attempts(id, 0, "one")?[0].0;
    assert_eq!(fs::read(path.join("output.txt"))?, raw);
    let checked: result::CheckedResult = record::read_json(&path.join("result.json"))?;
    assert_eq!(checked.findings[0].citation, result::CitationStatus::Ok);
    assert_eq!(
        checked.findings[1].citation,
        result::CitationStatus::PathNotFound
    );
    // Regeneration uses retained citation checks, even after the live file goes.
    fs::remove_file(fixture.source.join("a.rs"))?;
    let summary = service.regenerate_summary(id)?;
    for expected in [
        "a.rs:1-3",
        "citation: ok",
        "citation: path_not_found",
        "Trigger: Input condition",
        "Mechanism: Unchecked value",
        "Impact: Wrong result",
        "Evidence: Source claim",
        "Remediation: Validate input",
    ] {
        assert!(summary.contains(expected), "{expected}");
    }
    assert!(summary.find("one: succeeded") < summary.find("Reviewer claim"));
    assert!(!summary.contains("Source is live"));
    Ok(())
}

#[tokio::test]
async fn cancellation_of_a_replacement_cannot_start_another_attempt() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.fake.script(
        "one",
        vec![
            Step::Finish(Completion::Failed("zero output".into()), None),
            Step::Wait(Arc::new(Semaphore::new(0)), Vec::new()),
        ],
    );
    let mut service = fixture.service()?;
    let id = service.start(fixture.request(&["one"]))?;
    fixture.fake.started(2).await;
    service.cancel(id, Some(0))?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(id, 0, "one")?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].1.trigger, Trigger::AutomaticReplacement);
    assert_eq!(attempts[1].1.state, State::Cancelled);
    assert_eq!(fixture.fake.call_count(), 2);
    Ok(())
}

#[tokio::test]
async fn retry_remains_available_after_a_completed_task_reports_storage_failure() -> Result<()> {
    let fixture = Fixture::new()?;
    let block_summary = Arc::new(AtomicBool::new(true));
    let block = block_summary.clone();
    let root = fixture.home.join("audex/runs");
    let captured = fixture.events.clone();
    let events: Events = Arc::new(move |event| {
        captured.lock().unwrap().push(event.clone());
        if let Event::Progress { run, status, .. } = event
            && status.state == State::Succeeded
            && block.swap(false, Ordering::SeqCst)
        {
            // The attempt completes, but publishing summary.md must fail.
            fs::create_dir(root.join(run.to_string()).join("summary.md")).unwrap();
        }
    });
    fixture.fake.script(
        "one",
        vec![
            Step::Finish(Completion::Succeeded, Some(STRUCTURED.to_vec())),
            Step::Finish(Completion::Succeeded, Some(STRUCTURED.to_vec())),
        ],
    );
    let mut service = AuditService::open(&fixture.home, fixture.fake.clone(), events)?;
    let id = service.start(fixture.request(&["one"]))?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !service.runs[&id].reviewers[&0]
            .task
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    {
        let events = fixture.events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(event,
            Event::Error { run, reviewer: None, reason }
            if *run == id && reason.contains("Summary generation failed")
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Complete { .. }))
        );
    }
    let first = fixture.attempts(id, 0, "one")?[0].0.clone();
    let old_status = fs::read(first.join("status.json"))?;
    fs::remove_dir(service.run_directory(id).join("summary.md"))?;
    service.retry(id, 0).await?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(id, 0, "one")?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].1.trigger, Trigger::OperatorRetry);
    assert_eq!(attempts[1].1.state, State::Succeeded);
    assert_eq!(fixture.fake.call_count(), 2);
    assert_eq!(fs::read(first.join("status.json"))?, old_status);
    assert_eq!(fs::read(first.join("output.txt"))?, STRUCTURED);
    let events = fixture.events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(event,
        Event::Error { run, reviewer: Some(0), reason }
        if *run == id && reason.contains("Previous task failed before operator retry")
            && reason.contains("Summary generation failed")
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Complete { .. }))
            .count(),
        1
    );
    Ok(())
}
