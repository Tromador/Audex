//! Attempt-owned bounded capture, fed by the ordinary native event reader.
//! Buffered data is never exposed as durable output and never parsed as a result.
use super::store::AuditStore;
use super::store::activity::{
    ActivityInput, CaptureIdentity, MAX_BATCH_BYTES, MAX_BATCH_ENTRIES, MAX_ENTRY_BYTES,
};
use super::store::attempts::ControlOutcome;
use codex_core::CodexThread;
use codex_protocol::items::{AgentMessageContent, TurnItem};
use codex_protocol::protocol::{Event, EventMsg, Op};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};

pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const FLUSH_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct CaptureHandle(Arc<CaptureInner>);

struct CaptureInner {
    store: AuditStore,
    identity: CaptureIdentity,
    pending: Mutex<Pending>,
    flushing: AsyncMutex<()>,
    runtime: Mutex<Weak<CodexThread>>,
    wake: Option<Arc<Notify>>,
}

#[derive(Default)]
struct Pending {
    entries: VecDeque<ActivityInput>,
    bytes: usize,
    closed: bool,
    source_finished: bool,
    control_markers: usize,
    failure: Option<&'static str>,
    offsets: HashMap<(String, String), i64>,
}

#[derive(Default)]
pub(super) struct CaptureHub(Mutex<HashMap<String, Weak<CaptureInner>>>);

impl CaptureHub {
    pub(super) fn get(&self, operation: &str) -> Option<CaptureHandle> {
        self.0
            .lock()
            .expect("capture hub")
            .get(operation)?
            .upgrade()
            .map(CaptureHandle)
    }
    pub(super) fn insert(&self, operation: String, capture: &CaptureHandle) {
        self.0
            .lock()
            .expect("capture hub")
            .insert(operation, Arc::downgrade(&capture.0));
    }
}

impl CaptureHandle {
    pub(super) async fn new(
        store: AuditStore,
        identity: CaptureIdentity,
        wake: Option<Arc<Notify>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            store.register_capture(&identity).await?,
            "capture identity is already stopped or stale"
        );
        let capture = Self(Arc::new(CaptureInner {
            store,
            identity,
            pending: Mutex::new(Pending::default()),
            flushing: AsyncMutex::new(()),
            runtime: Mutex::new(Weak::new()),
            wake,
        }));
        let weak = Arc::downgrade(&capture.0);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(
                tokio::time::Instant::now() + FLUSH_INTERVAL,
                FLUSH_INTERVAL,
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let capture = Self(inner);
                if capture.0.pending.lock().expect("capture buffer").closed {
                    break;
                }
                if capture.flush().await.is_err() {
                    break;
                }
            }
        });
        Ok(capture)
    }

    pub(super) fn bind_runtime(&self, runtime: &Arc<CodexThread>) {
        *self.0.runtime.lock().expect("capture runtime") = Arc::downgrade(runtime);
    }

    #[cfg(test)]
    pub(super) async fn hold_flush(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.flushing.lock().await
    }

    pub(super) fn identity(&self) -> &CaptureIdentity {
        &self.0.identity
    }

    /// The prefix remains in memory during an in-flight flush. A control can
    /// therefore copy it without waiting on the flushing task; store deduplication
    /// orders a competing flush and the control transaction.
    pub(super) fn control_prefix(&self) -> Vec<ActivityInput> {
        let mut pending = self.0.pending.lock().expect("capture buffer");
        pending.control_markers += 1;
        pending.entries.iter().cloned().collect()
    }

    pub(super) fn control_finished(&self, response: Option<&ControlOutcome>) {
        let mut pending = self.0.pending.lock().expect("capture buffer");
        pending.control_markers = pending.control_markers.saturating_sub(1);
        if response.is_some_and(|result| result.cutoff.is_some() || result.completed_won) {
            pending.closed = true;
            pending.entries.clear();
            pending.bytes = 0;
        }
    }

    pub(super) fn completion_finished(&self, committed: bool) {
        let mut pending = self.0.pending.lock().expect("capture buffer");
        pending.control_markers = pending.control_markers.saturating_sub(1);
        if committed {
            pending.closed = true;
            pending.entries.clear();
            pending.bytes = 0;
        }
    }

    pub(super) fn failure(&self) -> Option<&'static str> {
        self.0.pending.lock().expect("capture buffer").failure
    }

    pub(crate) fn request_interrupt(&self) {
        let Some(runtime) = self.0.runtime.lock().expect("capture runtime").upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let _ =
                tokio::time::timeout(Duration::from_secs(5), runtime.submit(Op::Interrupt)).await;
        });
    }

    async fn failed(&self, reason: &'static str) {
        let reason = {
            let mut pending = self.0.pending.lock().expect("capture buffer");
            let reason = *pending.failure.get_or_insert(reason);
            pending.closed = true;
            reason
        };
        self.request_interrupt();
        let _ = self
            .0
            .store
            .note_capture_failure(&self.0.identity.assignment_id, reason)
            .await;
        if let Some(wake) = &self.0.wake {
            wake.notify_one();
        }
    }

    pub(super) async fn flush(&self) -> anyhow::Result<()> {
        let _flush = self.0.flushing.lock().await;
        let inputs: Vec<_> = {
            let pending = self.0.pending.lock().expect("capture buffer");
            if pending.closed || pending.control_markers != 0 {
                return Ok(());
            }
            pending.entries.iter().cloned().collect()
        };
        if inputs.is_empty() {
            match self.0.store.capture_is_active(&self.0.identity).await {
                Ok(true) => return self.finish_if_drained().await,
                Ok(false) => {
                    self.0.pending.lock().expect("capture buffer").closed = true;
                    self.request_interrupt();
                    return Ok(());
                }
                Err(error) => {
                    self.failed("capture ownership could not be checked").await;
                    return Err(error.into());
                }
            }
        }
        let appended = match self
            .0
            .store
            .append_activity(&self.0.identity, &inputs)
            .await
        {
            Ok(appended) => appended,
            Err(error) => {
                self.failed("activity could not be durably retained").await;
                return Err(error.into());
            }
        };
        {
            let mut pending = self.0.pending.lock().expect("capture buffer");
            if appended.fenced {
                pending.closed = true;
                pending.entries.clear();
                pending.bytes = 0;
            } else {
                for entry in &inputs {
                    if pending
                        .entries
                        .front()
                        .is_some_and(|front| front.producer_key == entry.producer_key)
                    {
                        let removed = pending.entries.pop_front().expect("front exists");
                        pending.bytes -= removed.payload.len();
                    }
                }
            }
        }
        if appended.fenced {
            self.request_interrupt();
        }
        if appended.truncated {
            self.failed("capture limit reached; output is truncated")
                .await;
        }
        if !inputs.is_empty()
            && let Some(wake) = &self.0.wake
        {
            wake.notify_one();
        }
        self.finish_if_drained().await
    }

    async fn finish_if_drained(&self) -> anyhow::Result<()> {
        {
            let mut pending = self.0.pending.lock().expect("capture buffer");
            if pending.closed
                || !pending.source_finished
                || !pending.entries.is_empty()
                || pending.control_markers != 0
            {
                return Ok(());
            }
            pending.closed = true;
        }
        self.0.store.finish_capture(&self.0.identity).await?;
        Ok(())
    }

    async fn offer(&self, input: ActivityInput) -> anyhow::Result<()> {
        if input.turn_id.len() > 512 || input.item_id.len() > 512 || input.producer_key.len() > 1024
        {
            self.failed("activity identity exceeded capture bound")
                .await;
            return Err(anyhow::anyhow!("activity identity exceeded capture bound"));
        }
        if input.payload.len() > MAX_ENTRY_BYTES {
            self.failed("activity entry exceeded capture bound; output is truncated")
                .await;
            return Err(anyhow::anyhow!("activity entry exceeded capture bound"));
        }
        for _ in 0..2 {
            let queued = {
                let mut pending = self.0.pending.lock().expect("capture buffer");
                if pending.closed {
                    return Ok(());
                }
                if pending.entries.len() < MAX_BATCH_ENTRIES
                    && pending.bytes + input.payload.len() <= MAX_BATCH_BYTES
                {
                    pending.bytes += input.payload.len();
                    pending.entries.push_back(input.clone());
                    Some(pending.bytes >= FLUSH_BYTES || pending.entries.len() == MAX_BATCH_ENTRIES)
                } else {
                    None
                }
            };
            if let Some(flush) = queued {
                if flush {
                    self.flush().await?;
                }
                return Ok(());
            }
            self.flush().await?;
        }
        self.failed("capture buffer bound exceeded; output may be missing")
            .await;
        Err(anyhow::anyhow!("capture buffer bound exceeded"))
    }

    pub(super) async fn allowed(&self) -> bool {
        if self.0.pending.lock().expect("capture buffer").closed {
            return false;
        }
        matches!(
            self.0.store.capture_is_active(&self.0.identity).await,
            Ok(true)
        )
    }

    pub(super) async fn tool(
        &self,
        turn: &str,
        call: &str,
        kind: &str,
        payload: String,
    ) -> anyhow::Result<()> {
        self.offer(ActivityInput {
            producer_key: format!("{kind}:{turn}:{call}"),
            turn_id: turn.to_owned(),
            item_id: call.to_owned(),
            kind: kind.to_owned(),
            payload,
            text_offset: None,
        })
        .await
    }

    async fn status(&self, turn: &str, status: &str) -> anyhow::Result<()> {
        self.offer(ActivityInput {
            producer_key: format!("status:{turn}:{status}"),
            turn_id: turn.to_owned(),
            item_id: String::new(),
            kind: "status".to_owned(),
            payload: status.to_owned(),
            text_offset: None,
        })
        .await
    }

    async fn finish(&self) -> anyhow::Result<()> {
        self.0
            .pending
            .lock()
            .expect("capture buffer")
            .source_finished = true;
        self.flush().await
    }

    /// Called by the existing sole event reader before ordinary event translation.
    /// Deliberately excludes reasoning, raw response bodies, arbitrary errors,
    /// auth/configuration messages and all other ambient event payloads.
    pub(crate) async fn observe(&self, event: &Event) {
        let result = async {
            match &event.msg {
                EventMsg::AgentMessageContentDelta(delta)
                    if delta.thread_id == self.0.identity.session_id =>
                {
                    if delta.turn_id.len() > 512 || delta.item_id.len() > 512 {
                        return Err(anyhow::anyhow!("activity identity exceeded capture bound"));
                    }
                    let key = (delta.turn_id.clone(), delta.item_id.clone());
                    let offset = {
                        let mut pending = self.0.pending.lock().expect("capture buffer");
                        if pending.closed {
                            return Ok(());
                        }
                        if !pending.offsets.contains_key(&key) && pending.offsets.len() >= 4096 {
                            drop(pending);
                            return Err(anyhow::anyhow!("too many open captured items"));
                        }
                        let offset = pending.offsets.entry(key).or_default();
                        let previous = *offset;
                        *offset += delta.delta.len() as i64;
                        previous
                    };
                    self.offer(ActivityInput {
                        producer_key: format!(
                            "delta:{}:{}:{}",
                            delta.turn_id, delta.item_id, offset
                        ),
                        turn_id: delta.turn_id.clone(),
                        item_id: delta.item_id.clone(),
                        kind: "assistant_delta".to_owned(),
                        payload: delta.delta.clone(),
                        text_offset: Some(offset),
                    })
                    .await?;
                }
                EventMsg::ItemCompleted(completed)
                    if completed.thread_id.to_string() == self.0.identity.session_id =>
                {
                    if let TurnItem::AgentMessage(message) = &completed.item {
                        let payload = message
                            .content
                            .iter()
                            .map(|part| match part {
                                AgentMessageContent::Text { text } => text.as_str(),
                            })
                            .collect::<String>();
                        self.offer(ActivityInput {
                            producer_key: format!("message:{}:{}", completed.turn_id, message.id),
                            turn_id: completed.turn_id.clone(),
                            item_id: message.id.clone(),
                            kind: "assistant_message".to_owned(),
                            payload,
                            text_offset: None,
                        })
                        .await?;
                        self.0
                            .pending
                            .lock()
                            .expect("capture buffer")
                            .offsets
                            .remove(&(completed.turn_id.clone(), message.id.clone()));
                    }
                }
                EventMsg::TurnComplete(completed) => {
                    self.status(&completed.turn_id, "turn_completed").await?;
                    self.finish().await?;
                }
                EventMsg::TurnAborted(aborted) => {
                    self.status(
                        aborted.turn_id.as_deref().unwrap_or(&event.id),
                        "turn_aborted",
                    )
                    .await?;
                    self.finish().await?;
                }
                EventMsg::Error(_) => self.status(&event.id, "execution_error").await?,
                _ => {}
            }
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            self.failed("native activity capture failed").await;
        }
    }
}
