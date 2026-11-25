//! Native Codex adapter; the app-server listener remains the sole event reader.
use super::record::Measurement;
use super::*;
use crate::audex::dispatch::{Listen, final_output, token_usage};
use crate::audex::reviewer;
use codex_core::TurnAdmissionState;
use codex_core::config::Config;
use codex_core::{CodexThread, ThreadManager};
use codex_protocol::ThreadId;
use codex_protocol::items::{AgentMessageContent, TurnItem};
use codex_protocol::protocol::{Event, EventMsg, Op};
use codex_protocol::turn_input::{NotSubmittedReason as Declined, StartIfIdleSubmission};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

/// How often a queued conversation review checks whether its conversation is idle.
const ADMISSION_POLL: Duration = Duration::from_millis(250);
/// How often a running conversation review checks that its conversation still exists.
const LIVENESS_POLL: Duration = Duration::from_secs(1);
/// How long cancellation waits for the interrupted turn to report its partial output.
const CANCEL_GRACE: Duration = Duration::from_secs(30);

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
            if let Some(conversation) = attempt.conversation() {
                let origin = conversation.thread_id.clone();
                return self.run_in_conversation(attempt, &origin).await;
            }
            let models = self.threads.get_models_manager();
            let mut prepared = reviewer::native::prepare(
                &self.config,
                models.as_ref(),
                &attempt.run,
                &attempt.status.reviewer,
                attempt.directory(),
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

impl NativeReviewer {
    /// The originating conversation performs this review as one ordinary turn.
    /// Audex observes only that turn and never shuts the conversation down.
    async fn run_in_conversation(&self, attempt: &mut Attempt, origin: &str) -> Result<Outcome> {
        let stopped = |completion| Outcome {
            completion,
            output: None,
            structured_output: None,
            usage: None,
        };
        let id = ThreadId::from_string(origin)?;
        let Ok(thread) = self.threads.get_thread(id).await else {
            return Ok(stopped(Completion::Interrupted(
                "Originating conversation is not loaded".into(),
            )));
        };
        let (events, mut updates) = NativeEvents::scoped();
        let events = Arc::new(events);
        let mut observer = ConversationObserver {
            thread: thread.clone(),
            events: events.clone(),
            turn: None,
        };
        thread.thread_extension_data().insert_arc(events.clone());
        let input = reviewer::native::conversation_input(&attempt.run);
        attempt.awaiting_admission()?;
        // Wait for the commissioning turn, or any other turn, to finish. Core's
        // start-if-idle settles a race with the operator's next input.
        let turn = loop {
            if attempt.cancellation.is_cancelled() {
                return Ok(stopped(Completion::Cancelled));
            }
            if thread.has_shutdown_status() {
                return Ok(stopped(Completion::Interrupted(
                    "Originating conversation closed before the review started".into(),
                )));
            }
            if matches!(
                thread.turn_admission_state().await,
                TurnAdmissionState::Idle
            ) {
                let settings = thread.config_snapshot().await;
                events.arm();
                match thread.start_turn_if_idle(input.clone()).await {
                    Ok(StartIfIdleSubmission::Started { turn_id, .. }) => {
                        events.bind(&turn_id);
                        observer.turn = Some(turn_id.clone());
                        attempt.admitted()?;
                        let effort = match settings.reasoning_effort {
                            Some(effort) => Some(effort),
                            None => self
                                .threads
                                .get_models_manager()
                                .get_model_info(
                                    &settings.model,
                                    &self.config.to_models_manager_config(),
                                )
                                .await
                                .default_reasoning_level
                                .clone(),
                        };
                        let effort = effort
                            .and_then(|effort| serde_json::to_value(effort).ok())
                            .and_then(|value| value.as_str().map(str::to_owned));
                        attempt.permission_summary(format!(
                            "Originating conversation {origin}: provider {}, model {}, effort {}. Approvals: {:?}. Active permission profile: {:?}; full access: {}. Tools, permissions and instructions are this conversation's own Audex configuration.",
                            settings.model_provider_id,
                            settings.model,
                            effort.as_deref().unwrap_or("unknown"),
                            settings.approval_policy,
                            settings.active_permission_profile,
                            settings.full_access,
                        ))?;
                        attempt.actual_settings(settings.model.clone(), effort)?;
                        break turn_id;
                    }
                    Ok(StartIfIdleSubmission::NotSubmitted { reason }) => match reason {
                        Declined::NotIdle | Declined::PendingTriggerTurn => events.disarm(),
                        Declined::ServerDraining => {
                            return Ok(stopped(Completion::Interrupted(
                                "Audex is shutting down".into(),
                            )));
                        }
                        reason => anyhow::bail!("conversation declined the review: {reason:?}"),
                    },
                    Err(error) => {
                        return Ok(stopped(Completion::Interrupted(format!(
                            "Originating conversation is unavailable: {error}"
                        ))));
                    }
                }
            }
            tokio::select! {
                biased;
                _ = attempt.cancellation.cancelled() => {}
                _ = tokio::time::sleep(ADMISSION_POLL) => {}
            }
        };
        attempt.conversation_turn(origin.to_owned(), thread.rollout_path(), Some(turn.clone()))?;
        if let Some(model) = thread.current_turn_model(&turn).await {
            attempt.reported_model(model)?;
        }
        let completion = loop {
            let (tool_calls, terminal) = {
                let state = updates.borrow_and_update();
                (state.tool_calls, state.terminal.clone())
            };
            if attempt.status().tool_calls != tool_calls {
                attempt.progress(tool_calls)?;
            }
            if let Some((terminal_turn, terminal)) = terminal {
                ensure!(
                    terminal_turn == turn,
                    "conversation review received another turn's result"
                );
                break terminal.completion();
            }
            tokio::select! {
                biased;
                _ = attempt.cancellation.cancelled() => {
                    // Address this turn only; a later operator turn is never interrupted.
                    interrupt_turn(&thread, &turn).await;
                    let _ = tokio::time::timeout(CANCEL_GRACE, async {
                        while updates.borrow().terminal.is_none() {
                            if updates.changed().await.is_err() {
                                break;
                            }
                        }
                    })
                    .await;
                    break Completion::Cancelled;
                }
                changed = updates.changed() => changed.context("conversation review observer closed")?,
                _ = tokio::time::sleep(LIVENESS_POLL) => {
                    if thread.has_shutdown_status() {
                        break Completion::Interrupted(
                            "Originating conversation closed during the review".into(),
                        );
                    }
                }
            }
        };
        observer.turn = None;
        drop(observer);
        let state = updates.borrow().clone();
        attempt.progress(state.tool_calls)?;
        Ok(Outcome {
            completion,
            output: (!state.output.is_empty()).then(|| state.output.into_bytes()),
            structured_output: None,
            usage: state.usage,
        })
    }
}

/// Interrupts the named turn if it is still the conversation's active turn.
async fn interrupt_turn(thread: &CodexThread, turn: &str) {
    let (reply, accepted) = oneshot::channel();
    let accepted = thread
        .submit(Op::InterruptIfNoPendingInput {
            turn_id: turn.to_owned(),
            reply,
        })
        .await
        .is_ok()
        && accepted.await.unwrap_or(false);
    if !accepted && thread.current_turn_id().await.as_deref() == Some(turn) {
        let _ = thread.submit(Op::Interrupt).await;
    }
}

/// Detaches the review's observer from the conversation on every exit. An
/// abandoned attempt also stops its admitted turn; the conversation stays open.
struct ConversationObserver {
    thread: Arc<CodexThread>,
    events: Arc<NativeEvents>,
    turn: Option<String>,
}

impl Drop for ConversationObserver {
    fn drop(&mut self) {
        let data = self.thread.thread_extension_data();
        if data
            .get::<NativeEvents>()
            .is_some_and(|current| Arc::ptr_eq(&current, &self.events))
        {
            data.remove::<NativeEvents>();
        }
        if let Some(turn) = self.turn.take() {
            let thread = self.thread.clone();
            tokio::spawn(async move { interrupt_turn(&thread, &turn).await });
        }
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

/// Which events a conversation review accepts from its shared conversation.
enum Scope {
    /// Before admission: other turns' events are ignored.
    Waiting,
    /// During admission: events are held until Core names the admitted turn.
    Arming(Vec<Event>),
    Turn(String),
}

/// Turn-only observation and usage for a review in its originating conversation.
struct Scoped {
    scope: Scope,
    responses: std::collections::HashSet<String>,
    measured: u64,
    input_tokens: u64,
    output_tokens: u64,
}

/// A latest-value channel, not an event journal. Only the attempt task writes files.
/// An isolated reviewer observes its whole thread; a conversation review is scoped.
pub(crate) struct NativeEvents(watch::Sender<NativeState>, Option<Mutex<Scoped>>);

impl NativeEvents {
    fn new() -> (Self, watch::Receiver<NativeState>) {
        let (sender, receiver) = watch::channel(NativeState::default());
        (Self(sender, None), receiver)
    }

    fn scoped() -> (Self, watch::Receiver<NativeState>) {
        let (sender, receiver) = watch::channel(NativeState::default());
        let scoped = Scoped {
            scope: Scope::Waiting,
            responses: Default::default(),
            measured: 0,
            input_tokens: 0,
            output_tokens: 0,
        };
        (Self(sender, Some(Mutex::new(scoped))), receiver)
    }

    fn scope(&self) -> Option<std::sync::MutexGuard<'_, Scoped>> {
        self.1.as_ref().map(|scoped| {
            scoped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
    }

    /// Holds events from now until `bind` names the admitted turn.
    fn arm(&self) {
        if let Some(mut scoped) = self.scope() {
            scoped.scope = Scope::Arming(Vec::new());
        }
    }

    fn disarm(&self) {
        if let Some(mut scoped) = self.scope() {
            scoped.scope = Scope::Waiting;
        }
    }

    /// Accepts the admitted turn's held and subsequent events, and nothing else.
    fn bind(&self, turn: &str) {
        let Some(mut scoped) = self.scope() else {
            return;
        };
        let held = match std::mem::replace(&mut scoped.scope, Scope::Turn(turn.to_owned())) {
            Scope::Arming(held) => held,
            Scope::Waiting | Scope::Turn(_) => Vec::new(),
        };
        for event in held.iter().filter(|event| event.id == turn) {
            self.observe_turn(&mut scoped, event);
        }
    }

    pub(crate) fn observe(&self, event: &Event) {
        let Some(mut scoped) = self.scope() else {
            return self.apply(event);
        };
        let accepted = match &mut scoped.scope {
            Scope::Waiting => false,
            Scope::Arming(held) => {
                held.push(event.clone());
                false
            }
            Scope::Turn(turn) => *turn == event.id,
        };
        if accepted {
            self.observe_turn(&mut scoped, event);
        }
    }

    fn observe_turn(&self, scoped: &mut Scoped, event: &Event) {
        match &event.msg {
            // Cumulative thread totals include earlier conversation; count this turn's
            // responses instead, once each. Missing measurements stay unknown.
            EventMsg::TokenCount(_) => {}
            EventMsg::RawResponseCompleted(completed) => {
                if !scoped.responses.insert(completed.response_id.clone()) {
                    return;
                }
                if let Some(usage) = &completed.token_usage {
                    scoped.measured += 1;
                    scoped.input_tokens += u64::try_from(usage.input_tokens).unwrap_or(0);
                    scoped.output_tokens += u64::try_from(usage.output_tokens).unwrap_or(0);
                }
                if scoped.measured == 0 {
                    return;
                }
                let complete = scoped.measured == scoped.responses.len() as u64;
                let usage = Usage {
                    basis: Measurement::Measured,
                    input_tokens: complete.then_some(scoped.input_tokens),
                    output_tokens: complete.then_some(scoped.output_tokens),
                    cost_usd: None,
                    cost_basis: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    model_usage: Some(serde_json::json!({
                        "scope": "review turn",
                        "responses": scoped.responses.len(),
                        "measuredResponses": scoped.measured,
                        "measuredInputTokens": scoped.input_tokens,
                        "measuredOutputTokens": scoped.output_tokens,
                    })),
                };
                self.0.send_modify(|state| state.usage = Some(usage));
            }
            _ => self.apply(event),
        }
    }

    pub(crate) fn listener(self: &Arc<Self>) -> NativeListener {
        self.0.send_modify(|state| state.listener_started = true);
        NativeListener(self.clone())
    }

    fn apply(&self, event: &Event) {
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
