//! One exact attempt's public journal. Readers own no execution or collection authority.
use super::*;
use crate::history_cell::{PlainHistoryCell, sanitize_user_text};
use crate::pager_overlay::TranscriptHistoryState;
use crate::pager_overlay::TranscriptOverlay;
use codex_app_server_protocol::{
    AuditAttemptReadParams, AuditAttemptReadResponse, AuditAttemptView,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Widget},
};
use std::sync::Arc;
use tokio::{sync::Notify, task::JoinHandle};

const PAGE_SIZE: u32 = 128;

pub(crate) struct ActivityViewer {
    pub(crate) id: Uuid,
    pub(crate) thread_id: ThreadId,
    pub(crate) target: AuditAttemptView,
    pub(crate) transcript: TranscriptOverlay,
    pub(crate) wake: Arc<Notify>,
    pub(crate) cursor: i64,
    pub(crate) reader_epoch: u64,
    pub(crate) control_pending: bool,
    pub(crate) confirmation_visible: bool,
    pub(crate) control_task: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    connected: bool,
    assistant_seen: bool,
    notice: Option<String>,
}

impl ActivityViewer {
    pub(crate) fn new(
        thread_id: ThreadId,
        target: AuditAttemptView,
        keymap: &crate::keymap::RuntimeKeymap,
    ) -> Self {
        let mut transcript = TranscriptOverlay::new(Vec::new(), keymap.pager.clone());
        transcript.set_keymap_bindings(keymap);
        transcript.set_history_state(TranscriptHistoryState::Complete);
        Self {
            id: Uuid::new_v4(),
            thread_id,
            target,
            transcript,
            wake: Arc::new(Notify::new()),
            cursor: 0,
            reader_epoch: 0,
            control_pending: false,
            confirmation_visible: false,
            control_task: None,
            reader: None,
            connected: false,
            assistant_seen: false,
            notice: None,
        }
    }

    pub(crate) fn reconnect(&mut self, handle: AppServerRequestHandle, events: AppEventSender) {
        if let Some(task) = self.reader.take() {
            task.abort();
        }
        self.connected = false;
        self.reader_epoch += 1;
        self.reader = Some(tokio::spawn(read_activity(
            handle,
            self.id,
            self.reader_epoch,
            self.target.run_id.clone(),
            self.target.assignment_id.clone(),
            self.cursor,
            events,
            self.wake.clone(),
        )));
    }

    pub(crate) fn apply(&mut self, page: Result<AuditAttemptReadResponse, String>) {
        let Ok(page) = page else {
            self.connected = false;
            return;
        };
        if page.attempt.run_id != self.target.run_id
            || page.attempt.assignment_id != self.target.assignment_id
        {
            self.connected = false;
            return;
        }
        self.connected = true;
        self.target = page.attempt;
        let bound = self
            .target
            .output_cutoff
            .unwrap_or(self.target.output_sequence);
        for entry in page.entries {
            if entry.sequence <= self.cursor || entry.sequence > bound {
                continue;
            }
            let label = match entry.kind.as_str() {
                "assistant_delta" => {
                    self.assistant_seen = true;
                    "Assistant output — unvalidated delta"
                }
                "assistant_message" => {
                    self.assistant_seen = true;
                    "Assistant output — unvalidated complete message"
                }
                "tool_start" | "tool_result" => "Source/tool activity — not findings",
                "status" => "Execution status — not review text",
                "gap" => "CAPTURE GAP — material may be missing",
                "truncation" => "CAPTURE TRUNCATED — retained prefix only",
                _ => "Unsupported activity withheld",
            };
            let mut lines = vec![Line::from(format!("#{} {label}", entry.sequence))];
            if entry.sequence > self.cursor + 1 {
                lines.push(Line::from("CAPTURE GAP — discontinuous durable sequence"));
            }
            if matches!(
                entry.kind.as_str(),
                "assistant_delta"
                    | "assistant_message"
                    | "tool_start"
                    | "tool_result"
                    | "status"
                    | "gap"
                    | "truncation"
            ) {
                let safe = sanitize_user_text(entry.payload.into());
                lines.extend(safe.split('\n').map(|text| Line::from(text.to_owned())));
            }
            self.transcript
                .insert_cell(Arc::new(PlainHistoryCell::new(lines)));
            self.cursor = entry.sequence;
        }
    }

    /// Text/search/selection owns its keys before C/R/I. Only an explicit uppercase press
    /// outside those modes opens a control; paste and repeated keys cannot authorise one.
    pub(crate) fn action_for_key(
        &self,
        key: KeyEvent,
    ) -> Option<codex_app_server_protocol::AuditAttemptAction> {
        if self.control_pending
            || self.transcript.has_active_interaction()
            || key.kind != KeyEventKind::Press
            || !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            return None;
        }
        use codex_app_server_protocol::AuditAttemptAction as Action;
        let action = match key.code {
            KeyCode::Char('C') => Action::Cancel,
            KeyCode::Char('R') => Action::Retry,
            KeyCode::Char('I') => Action::Ignore,
            _ => return None,
        };
        attempt_actions(&self.target)
            .contains(&action)
            .then_some(action)
    }

    pub(crate) fn finish_control(&mut self, message: String) {
        self.control_pending = false;
        self.confirmation_visible = false;
        self.control_task = None;
        self.notice = Some(message);
        self.wake.notify_one();
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) -> bool {
        let connection = if self.connected {
            "Connected"
        } else {
            "Disconnected / reconnecting"
        };
        let activity = if self.cursor == 0 && self.target.operation_state == "planned" {
            "Queued; no observable committed activity"
        } else if self.cursor == 0 {
            "No observable committed activity"
        } else if !self.assistant_seen {
            "No review output available (tool/status activity only)"
        } else {
            "Retained public output; awaiting committed activity; unvalidated history"
        };
        let identity = format!(
            "Reviewer {}: {} | {} / {}\nAssignment: {} | attempt {} | {}\n{}; execution={}; current={}; {}\n{} | durable sequence={} | cutoff={:?}\n{} | gap={:?}; failure={:?}; truncated={}",
            self.target.reviewer_ordinal + 1,
            self.target.reviewer_id,
            self.target.resolved_model,
            self.target.provider,
            self.target.assignment_id,
            self.target.attempt_ordinal,
            if self.target.completed {
                "Completed validated contribution"
            } else {
                "Incomplete output"
            },
            self.target.disposition,
            self.target.operation_state,
            self.target.current,
            connection,
            activity,
            self.cursor,
            self.target.output_cutoff,
            self.notice
                .as_deref()
                .unwrap_or("Activity is not a published report"),
            self.target.capture_gap,
            self.target.capture_failure,
            self.target.capture_truncated
        );
        let header = Paragraph::new(sanitize_user_text(identity.into()).into_owned())
            .wrap(ratatui::widgets::Wrap { trim: false });
        let header_height =
            (header.line_count(area.width.max(1)) as u16).min(area.height.saturating_sub(6));
        header.render(Rect::new(area.x, area.y, area.width, header_height), buf);
        let body = Rect::new(
            area.x,
            area.y + header_height,
            area.width,
            area.height.saturating_sub(header_height),
        );
        let scanning = self.transcript.prepare_activity_search(body.width);
        self.transcript.render(body, buf);
        if !self.transcript.has_active_interaction() && body.height > 1 {
            let actions = attempt_actions(&self.target)
                .iter()
                .map(|action| match action {
                    codex_app_server_protocol::AuditAttemptAction::Cancel => "C Cancel",
                    codex_app_server_protocol::AuditAttemptAction::Retry => "R Retry",
                    codex_app_server_protocol::AuditAttemptAction::Ignore => "I Ignore",
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let footer = Rect::new(body.x, body.bottom() - 2, body.width, 1);
            ratatui::widgets::Clear.render(footer, buf);
            let hint = if self.control_pending {
                "Esc return · Waiting for shared reviewer control".to_owned()
            } else if actions.is_empty() {
                "Esc return · End tail · / search".to_owned()
            } else {
                format!("Esc return · {actions} · End tail · / search")
            };
            Line::from(hint).render(footer, buf);
        }
        scanning
    }
}

impl Drop for ActivityViewer {
    fn drop(&mut self) {
        if let Some(task) = self.reader.take() {
            task.abort();
        }
        if let Some(task) = self.control_task.take() {
            task.abort();
        }
    }
}

/// At most one unconsumed page per viewer. A slow/closed view can only stall this
/// disposable reader, never the collector. Notifications are hints; polling catches misses.
async fn read_activity(
    handle: AppServerRequestHandle,
    viewer_id: Uuid,
    reader_epoch: u64,
    run_id: String,
    assignment_id: String,
    mut cursor: i64,
    events: AppEventSender,
    wake: Arc<Notify>,
) {
    let audits = AuditClient::new(handle);
    loop {
        let page = audits
            .read_attempt(AuditAttemptReadParams {
                run_id: run_id.clone(),
                assignment_id: assignment_id.clone(),
                after_sequence: Some(cursor),
                limit: Some(PAGE_SIZE),
            })
            .await
            .map_err(|_| "Activity read unavailable".to_owned());
        let next = page.as_ref().ok().map_or(cursor, |page| page.next_sequence);
        let more = page.as_ref().is_ok_and(|page| {
            next > cursor
                && next
                    < page
                        .attempt
                        .output_cutoff
                        .unwrap_or(page.attempt.output_sequence)
        });
        let (consumed, ack) = oneshot::channel();
        events.send(AppEvent::AuditActivityPage {
            viewer_id,
            reader_epoch,
            page: Box::new(page),
            consumed,
        });
        if ack.await.is_err() {
            return;
        }
        cursor = next;
        if !more {
            tokio::select! { _=tokio::time::sleep(Duration::from_secs(1))=>{}, _=wake.notified()=>{}, _=events.app_event_tx.closed()=>return }
        }
    }
}

#[cfg(test)]
mod tests;
