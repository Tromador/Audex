//! Native Codex adapter; the app-server listener remains the sole event reader.
use super::record::Measurement;
use super::*;
use crate::audex::dispatch::{Listen, final_output, token_usage};
use crate::audex::reviewer;
use codex_core::config::Config;
use codex_core::{CodexThread, ThreadManager};
use codex_protocol::ThreadId;
use codex_protocol::items::{AgentMessageContent, TurnItem};
use codex_protocol::protocol::{Event, EventMsg, Op};
use codex_protocol::turn_input::StartIfIdleSubmission;
use tokio::sync::watch;

pub struct NativeReviewer {
    config: Arc<Config>,
    threads: Arc<ThreadManager>,
    listen: Listen,
    cleanup: Listen,
}

impl NativeReviewer {
    /// Uses the host's selected native provider and provider-scoped model catalogue.
    pub fn new(
        config: Arc<Config>,
        threads: Arc<ThreadManager>,
        listen: Listen,
        cleanup: Listen,
    ) -> Self {
        Self {
            config,
            threads,
            listen,
            cleanup,
        }
    }
}

impl Reviewer for NativeReviewer {
    fn validate(&self, selection: &ReviewerSelection) -> Result<()> {
        reviewer::native::validate(&self.config, selection)
    }

    fn run<'a>(&'a self, attempt: &'a mut Attempt) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let models = self.threads.get_models_manager();
            let mut prepared = reviewer::native::prepare(
                &self.config,
                models.as_ref(),
                &attempt.run,
                &attempt.status.reviewer,
            )
            .await?;
            attempt.permission_summary(prepared.permission_summary)?;
            if attempt.cancellation.is_cancelled() {
                return Ok(Outcome {
                    completion: Completion::Cancelled,
                    output: None,
                    structured_output: None,
                    usage: None,
                });
            }
            let (events, mut updates) = NativeEvents::new();
            prepared.options.thread_extension_init.insert(events);
            let created = self.threads.start_thread(prepared.options).await?;
            let id = created.thread_id;
            let thread = created.thread;
            let mut owner = NativeThread {
                id,
                thread: thread.clone(),
                threads: self.threads.clone(),
                cleanup: self.cleanup.clone(),
                stopped: false,
            };
            // Once created, every exit (including task abort) owns thread shutdown.
            let execution = async {
                attempt.native_thread(id.to_string(), thread.rollout_path())?;
                (self.listen)(id, thread.clone()).await?;
                if attempt.cancellation.is_cancelled() {
                    return Ok(Completion::Cancelled);
                }
                let turn = match thread.start_turn_if_idle(prepared.input).await? {
                    StartIfIdleSubmission::Started { turn_id, .. } => turn_id,
                    StartIfIdleSubmission::NotSubmitted { .. } => anyhow::bail!("native reviewer input was not submitted"),
                };
                loop {
                    let (tool_calls, terminal, listener_stopped) = {
                        let state = updates.borrow_and_update();
                        (state.tool_calls, state.terminal.clone(), state.listener_stopped)
                    };
                    if attempt.status().tool_calls != tool_calls {
                        attempt.progress(tool_calls)?;
                    }
                    if let Some((terminal_turn, terminal)) = terminal {
                        ensure!(terminal_turn == turn, "native reviewer received another turn's result");
                        return Ok(terminal.completion());
                    }
                    ensure!(!listener_stopped, "native reviewer event listener stopped before completion");
                    tokio::select! {
                        biased;
                        _ = attempt.cancellation.cancelled() => return Ok(Completion::Cancelled),
                        changed = updates.changed() => changed.context("native reviewer event listener closed")?,
                    }
                }
            }.await;
            // Interrupt on cancellation or an adapter error, then use Core's
            // shutdown barrier even after success (compaction may still be active).
            if attempt.cancellation.is_cancelled()
                || !matches!(execution, Ok(Completion::Succeeded))
            {
                let _ = thread.submit(Op::Interrupt).await;
            }
            let shutdown = thread.shutdown_and_wait().await;
            let cleanup = if shutdown.is_ok() {
                // The listener drains final output and TokenCount before closing.
                // No history read or rollout polling is involved.
                if updates.borrow().listener_started {
                    while !updates.borrow().listener_stopped {
                        if updates.changed().await.is_err() {
                            break;
                        }
                    }
                }
                owner.cleanup().await
            } else {
                Ok(())
            };
            let state = updates.borrow().clone();
            attempt.native_thread(id.to_string(), thread.rollout_path())?;
            attempt.progress(state.tool_calls)?;
            let completion = match (execution, shutdown, cleanup) {
                (_, Err(error), _) => {
                    Completion::Failed(format!("Native thread shutdown failed: {error}"))
                }
                (_, _, Err(error)) | (Err(error), _, _) => Completion::Failed(error.to_string()),
                (Ok(completion), _, _) => completion,
            };
            Ok(Outcome {
                completion,
                output: (!state.output.is_empty()).then(|| state.output.into_bytes()),
                structured_output: None,
                usage: state.usage,
            })
        })
    }
}

struct NativeThread {
    id: ThreadId,
    thread: Arc<CodexThread>,
    threads: Arc<ThreadManager>,
    cleanup: Listen,
    stopped: bool,
}

impl NativeThread {
    async fn cleanup(&mut self) -> Result<()> {
        if self
            .threads
            .remove_thread_if_matches(&self.id, &self.thread)
            .await
            .is_some()
        {
            (self.cleanup)(self.id, self.thread.clone()).await?;
        }
        self.stopped = true;
        Ok(())
    }
}

impl Drop for NativeThread {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let mut owner = Self {
            id: self.id,
            thread: self.thread.clone(),
            threads: self.threads.clone(),
            cleanup: self.cleanup.clone(),
            stopped: true,
        };
        tokio::spawn(async move {
            let _ = owner.thread.submit(Op::Interrupt).await;
            if let Err(error) = owner.thread.shutdown_and_wait().await {
                tracing::warn!("Native reviewer shutdown failed: {error}");
            } else if let Err(error) = owner.cleanup().await {
                tracing::warn!("Native reviewer listener cleanup failed: {error}");
            }
        });
    }
}

#[derive(Clone)]
enum Terminal {
    Succeeded,
    Failed(String),
    Interrupted,
}
impl Terminal {
    fn completion(self) -> Completion {
        match self {
            Self::Succeeded => Completion::Succeeded,
            Self::Failed(reason) => Completion::Failed(reason),
            Self::Interrupted => Completion::Interrupted("Native turn interrupted".into()),
        }
    }
}

#[derive(Clone, Default)]
struct NativeState {
    output: String,
    item: String,
    tool_calls: u64,
    usage: Option<Usage>,
    terminal: Option<(String, Terminal)>,
    listener_started: bool,
    listener_stopped: bool,
}

/// A latest-value channel, not an event journal. Only the attempt task writes files.
#[derive(Clone)]
pub(crate) struct NativeEvents(watch::Sender<NativeState>);

impl NativeEvents {
    fn new() -> (Self, watch::Receiver<NativeState>) {
        let (sender, receiver) = watch::channel(NativeState::default());
        (Self(sender), receiver)
    }

    pub(crate) fn listener(self: &Arc<Self>) -> NativeListener {
        self.0.send_modify(|state| state.listener_started = true);
        NativeListener(self.clone())
    }

    pub(crate) fn observe(&self, event: &Event) {
        self.0.send_if_modified(|state| {
            match &event.msg {
                EventMsg::AgentMessageContentDelta(delta) => {
                    if state.item != delta.item_id {
                        state.output.clear();
                        state.item.clone_from(&delta.item_id);
                    }
                    state.output.push_str(&delta.delta);
                }
                EventMsg::ItemCompleted(completed) => {
                    let TurnItem::AgentMessage(message) = &completed.item else {
                        return false;
                    };
                    state.item.clone_from(&message.id);
                    state.output = message
                        .content
                        .iter()
                        .map(|part| match part {
                            AgentMessageContent::Text { text } => text.as_str(),
                        })
                        .collect();
                }
                EventMsg::RawResponseItem(raw) => {
                    // Count model-issued calls once. Nested Code Mode commands emit
                    // execution events, not model response items, and are not counted again.
                    match &raw.item {
                        codex_protocol::models::ResponseItem::FunctionCall { name, .. }
                            if matches!(name.as_str(), "exec_command" | "write_stdin" | "wait") => {
                        }
                        codex_protocol::models::ResponseItem::CustomToolCall { name, .. }
                            if name == "exec" => {}
                        _ => return false,
                    }
                    state.tool_calls += 1;
                }
                EventMsg::TokenCount(_) => {
                    let Some(info) = token_usage(&event.msg) else {
                        return false;
                    };
                    state.usage = Some(Usage {
                        basis: Measurement::Measured,
                        input_tokens: info.total_token_usage.input_tokens.try_into().ok(),
                        output_tokens: info.total_token_usage.output_tokens.try_into().ok(),
                        cost_usd: None,
                        cost_basis: None,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        model_usage: None,
                    });
                }
                EventMsg::TurnComplete(completed) => {
                    if let Some(output) = final_output(completed) {
                        state.output = output.to_owned();
                    }
                    state.terminal = Some((
                        completed.turn_id.clone(),
                        completed
                            .error
                            .as_ref()
                            .map_or(Terminal::Succeeded, |error| {
                                Terminal::Failed(error.message.clone())
                            }),
                    ));
                }
                EventMsg::TurnAborted(aborted) => {
                    state.terminal = Some((
                        aborted.turn_id.clone().unwrap_or_else(|| event.id.clone()),
                        Terminal::Interrupted,
                    ));
                }
                _ => return false,
            }
            true
        });
    }
}

/// Signals every listener exit, including cancellation and unwinding.
pub(crate) struct NativeListener(Arc<NativeEvents>);
impl Drop for NativeListener {
    fn drop(&mut self) {
        self.0.0.send_modify(|state| state.listener_stopped = true);
    }
}

#[cfg(test)]
mod tests;

pub(crate) fn is_reviewer_source(source: Option<&codex_protocol::protocol::ThreadSource>) -> bool {
    matches!(source, Some(codex_protocol::protocol::ThreadSource::Feature(name)) if name == "audex-reviewer")
}
