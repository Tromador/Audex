//! Stream to disk without a total-size ceiling; only one JSON record is buffered.
use super::super::record::Measurement;
use super::*;
use std::io::Write;
use tokio::io::unix::AsyncFd;

pub(super) const MAX_RECORD: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Stream {
    line: Vec<u8>,
    pub output: Option<Vec<u8>>,
    pub structured: Option<Value>,
    pub usage: Option<Usage>,
    pub model: Option<String>,
    pub tools: u64,
    pub result: Option<bool>,
    pub failure: Option<String>,
    /// The identity Audex assigned or resumed; any record reporting another fails.
    expected_session: Option<String>,
    /// Set once a client record confirms the session identity.
    pub session: Option<String>,
}

impl Stream {
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.event(&line)?;
            } else {
                ensure!(
                    self.line.len() < MAX_RECORD,
                    "Claude JSON record exceeds 8 MiB"
                );
                self.line.push(*byte);
            }
        }
        Ok(())
    }

    fn eof(&mut self) -> Result<()> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.event(&line)?;
        }
        Ok(())
    }

    fn event(&mut self, line: &[u8]) -> Result<()> {
        let value: Value = serde_json::from_slice(line).context("Malformed Claude JSON record")?;
        let kind = value["type"]
            .as_str()
            .context("Claude record has no type")?;
        if let Some(session) = value["session_id"].as_str() {
            if let Some(expected) = &self.expected_session {
                ensure!(
                    session == expected,
                    "Claude reported session {session}, not the requested session {expected}"
                );
            }
            self.session = Some(session.into());
        }
        match kind {
            "system" if value["subtype"] == "init" => {
                if let Some(model) = value["model"].as_str() {
                    self.model = Some(model.into());
                }
            }
            "assistant" => {
                if let Some(model) = value["message"]["model"].as_str() {
                    self.model = Some(model.into());
                }
                let content = value["message"]["content"]
                    .as_array()
                    .context("Claude assistant content is not an array")?;
                // --include-partial-messages is deliberately absent: each assistant
                // message supplies its complete tool blocks once, without deltas.
                self.tools += content
                    .iter()
                    .filter(|block| block["type"] == "tool_use")
                    .count() as u64;
            }
            "result" => {
                ensure!(
                    self.result.is_none(),
                    "Claude emitted more than one final result"
                );
                let is_error = value["is_error"].as_bool();
                // Explicit errors carry diagnostics, not reviewer contributions.
                // Keep real output even if another envelope field is malformed.
                if is_error != Some(true)
                    && let Some(text) = value["result"].as_str()
                {
                    self.output = Some(text.as_bytes().to_vec());
                }
                ensure!(
                    value
                        .get("result")
                        .is_none_or(|v| v.is_null() || v.is_string()),
                    "Claude final output is not text"
                );
                if let Some(model) = value["model"].as_str() {
                    self.model = Some(model.into());
                }
                let cost = value["total_cost_usd"]
                    .as_f64()
                    .filter(|cost| cost.is_finite() && *cost >= 0.0);
                let usage = &value["usage"];
                self.usage = Some(Usage {
                    basis: Measurement::Measured,
                    input_tokens: usage["input_tokens"].as_u64(),
                    output_tokens: usage["output_tokens"].as_u64(),
                    cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64(),
                    cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
                    cost_usd: cost,
                    cost_basis: cost.map(|_| Measurement::Estimated),
                    model_usage: value
                        .get("modelUsage")
                        .filter(|value| !value.is_null())
                        .cloned(),
                });
                self.structured = value
                    .get("structured_output")
                    .filter(|value| is_error != Some(true) && !value.is_null())
                    .cloned();
                let subtype = value["subtype"]
                    .as_str()
                    .context("Claude result has no subtype")?;
                let is_error = is_error.context("Claude result has no error flag")?;
                let success = subtype == "success" && !is_error;
                self.result = Some(success);
                if is_error {
                    let mut details = Vec::new();
                    if let Some(reason) =
                        value["terminal_reason"].as_str().filter(|s| !s.is_empty())
                    {
                        details.push(reason.to_owned());
                    }
                    if let Some(status) = value["api_error_status"].as_u64() {
                        details.push(format!("HTTP {status}"));
                    }
                    if let Some(text) = value["result"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        details.push(text.chars().take(256).collect());
                    }
                    if details.is_empty() && subtype != "success" {
                        details.push(subtype.to_owned());
                    }
                    self.failure = Some(if details.is_empty() {
                        "Claude reported an error".into()
                    } else {
                        format!("Claude error: {}", details.join("; "))
                    });
                } else if !success {
                    self.failure = Some(format!("Claude result: {subtype}"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Tempfile storage keeps stream memory bounded. Even future cancellation or a
/// panic publishes the bytes already captured, without replacing them with JSON.
struct Capture {
    file: Option<tempfile::NamedTempFile>,
    path: PathBuf,
}
impl Capture {
    fn new(directory: &Path, name: &str) -> Result<Self> {
        Ok(Self {
            file: Some(tempfile::NamedTempFile::new_in(directory)?),
            path: directory.join(name),
        })
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.as_mut().expect("open capture").write_all(bytes)?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.as_file().sync_all()?;
            file.persist(&self.path).map_err(|error| error.error)?;
        }
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::warn!("Claude capture publication failed: {error}");
        }
    }
}

pub(super) async fn run(
    launch: launch::Launch,
    directory: &Path,
    cancellation: &CancellationToken,
    session: Option<&str>,
    mut progress: impl FnMut(&Stream) -> Result<()>,
) -> Result<Outcome> {
    let mut transcript = Capture::new(directory, "transcript.jsonl")?;
    let mut diagnostics = Capture::new(directory, "stderr.log")?;
    let (spawned, prompt) = owner::spawn(launch).await?;
    let owner::Spawned {
        mut owner,
        stdin,
        stdout,
        stderr,
    } = spawned;
    let mut stdout = AsyncFd::new(stdout)?;
    let mut stderr = AsyncFd::new(stderr)?;
    let submission = owner::write(stdin, &prompt);
    tokio::pin!(submission);
    let mut submitted = false;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut exit = None;
    let mut cancelled = false;
    let mut error = None;
    let mut state = Stream {
        expected_session: session.map(str::to_owned),
        ..Default::default()
    };
    let mut bytes = [0u8; 8192];
    let mut stderr_bytes = [0u8; 8192];
    while stdout_open || stderr_open || exit.is_none() {
        tokio::select! {
            biased;
            _ = cancellation.cancelled(), if !cancelled => {
                cancelled = true;
                owner.stop();
            }
            status = &mut owner.exit, if exit.is_none() => {
                exit = Some(status.context("Claude owner stopped before reaping").and_then(|status| status.map_err(Into::into)));
            }
            result = &mut submission, if !submitted => {
                submitted = true;
                if let Err(failure) = result { error.get_or_insert(failure); owner.stop(); }
            }
            result = owner::read(&mut stdout, &mut bytes), if stdout_open => {
                match result {
                    Ok(size) => {
                        stdout_open = size != 0;
                        let captured = transcript.write(&bytes[..size]);
                        let parsed = if error.is_none() {
                            if size == 0 { state.eof() } else { state.bytes(&bytes[..size]) }
                        } else { Ok(()) };
                        // Publish any final text even when a later record is malformed.
                        let retained = state.output.as_ref().map_or(Ok(()), |output| record::write_atomic(&directory.join("output.txt"), output));
                        let reported = progress(&state);
                        if let Err(failure) = captured.and(parsed).and(retained).and(reported) {
                            error.get_or_insert(failure);
                            owner.stop();
                        }
                    }
                    Err(failure) => { stdout_open = false; error.get_or_insert(failure.into()); owner.stop(); }
                }
            }
            result = owner::read(&mut stderr, &mut stderr_bytes), if stderr_open => {
                match result {
                    Ok(size) => {
                        stderr_open = size != 0;
                        if let Err(failure) = diagnostics.write(&stderr_bytes[..size]) { error.get_or_insert(failure); owner.stop(); }
                    }
                    Err(failure) => { stderr_open = false; error.get_or_insert(failure.into()); owner.stop(); }
                }
            }
        }
    }
    transcript.finish()?;
    diagnostics.finish()?;
    let completion = if cancelled || cancellation.is_cancelled() {
        Completion::Cancelled
    } else if let Some(error) = error {
        Completion::Failed(format!("{error:#}"))
    } else {
        match exit.expect("owner finished") {
            Err(error) => Completion::Failed(format!("Claude cleanup failed: {error}")),
            Ok(status) if !status.success() => Completion::Failed(match state.failure {
                Some(reason) => format!("{reason} (Claude exited with {status})"),
                None => format!("Claude exited with {status}"),
            }),
            Ok(_) if state.result == Some(true) => Completion::Succeeded,
            Ok(_) => Completion::Failed(
                state
                    .failure
                    .unwrap_or_else(|| "Claude ended without a final result".into()),
            ),
        }
    };
    Ok(Outcome {
        completion,
        output: state.output,
        structured_output: state.structured,
        usage: state.usage,
    })
}
