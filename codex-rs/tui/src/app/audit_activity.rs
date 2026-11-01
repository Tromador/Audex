//! Activity overlay composition: public journal navigation and S3's confirmed controls only.
use super::*;
use codex_app_server_protocol::AuditAttemptView;

impl App {
    pub(super) fn open_audit_activity(
        &mut self,
        tui: &mut tui::Tui,
        session: &AppServerSession,
        thread_id: ThreadId,
        target: AuditAttemptView,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) || !self.audit_available(session) {
            return;
        }
        self.cancel_pending_key_chord();
        if self.overlay.is_some() {
            self.close_transcript_overlay(tui);
        }
        let mut viewer =
            crate::audit::activity::ActivityViewer::new(thread_id, target, &self.keymap);
        viewer.reconnect(session.request_handle(), self.app_event_tx.clone());
        self.audits.activity_wake = Some((viewer.target.run_id.clone(), viewer.wake.clone()));
        let _ = tui.enter_alt_screen();
        self.overlay = Some(Overlay::AuditActivity(Box::new(viewer)));
        tui.frame_requester().schedule_frame();
    }

    /// Retain an outcome in the existing originating-thread replay buffer. Only the
    /// displayed origin receives a live history insertion; another thread receives none.
    pub(super) async fn enqueue_audit_control_outcome(
        &mut self,
        thread_id: ThreadId,
        result: std::result::Result<String, String>,
    ) {
        let event = super::thread_events::ThreadBufferedEvent::AuditControlOutcome(result);
        self.ensure_thread_channel(thread_id)
            .store
            .lock()
            .await
            .push_buffered_event(event.clone());
        if self.current_displayed_thread_id() == Some(thread_id) {
            self.handle_thread_event_now(event);
        }
    }

    pub(super) fn show_audit_control_outcome(
        &mut self,
        result: std::result::Result<String, String>,
    ) {
        match result {
            Ok(message) => self.chat_widget.add_info_message(message, None),
            Err(message) => self.chat_widget.add_error_message(message),
        }
    }

    pub(super) fn reconnect_audit_activity(&mut self, session: &AppServerSession) {
        if let Some(Overlay::AuditActivity(viewer)) = &mut self.overlay {
            viewer.reconnect(session.request_handle(), self.app_event_tx.clone());
        }
    }

    /// Called before global shortcuts, disconnected-composer handling or paste routing.
    pub(super) fn handle_audit_activity_event(
        &mut self,
        tui: &mut tui::Tui,
        session: &AppServerSession,
        event: TuiEvent,
    ) -> Result<()> {
        let Some(Overlay::AuditActivity(mut viewer)) = self.overlay.take() else {
            return Ok(());
        };
        tui.set_mouse_capture(tui::MouseCapture::Enabled)?;
        let mut close = false;
        if viewer.control_pending {
            if viewer.confirmation_visible
                && let TuiEvent::Key(key) = &event
            {
                if !self.chat_widget.audit_activity_confirmation_active()
                    && key.code == KeyCode::Esc
                    && key.kind == KeyEventKind::Press
                {
                    close = true;
                } else if self.chat_widget.audit_activity_confirmation_active() {
                    self.chat_widget
                        .handle_audit_activity_confirmation_key(*key);
                    if !self.chat_widget.audit_activity_confirmation_active() {
                        // A delivered Yes must finish even if the viewer closes immediately.
                        // Dropping the handle detaches this already-decided shared submission.
                        viewer.control_task.take();
                    }
                }
            }
            if !viewer.confirmation_visible
                && matches!(&event, TuiEvent::Key(key) if key.code==KeyCode::Esc && key.kind==KeyEventKind::Press)
            {
                close = true;
            }
        } else if let TuiEvent::Key(key) = &event {
            if let Some(action) = viewer.action_for_key(*key) {
                viewer.control_pending = true;
                let target = viewer.target.clone();
                let id = viewer.id;
                let thread_id = viewer.thread_id;
                let events = self.app_event_tx.clone();
                let audits =
                    codex_app_server_client::audit::AuditClient::new(session.request_handle());
                viewer.control_task = Some(tokio::spawn(async move {
                    let result = crate::audit::confirm_and_control_attempt(
                        &audits, thread_id, target, action, None, &events,
                    )
                    .await;
                    let result = result.map(|result| {
                        format!("Reviewer control recorded for {}.", result.assignment_id)
                    });
                    events.send(AppEvent::AuditActivityControlFinished {
                        viewer_id: id,
                        thread_id,
                        result,
                    });
                }));
            } else if key.code == KeyCode::Esc
                && key.kind == KeyEventKind::Press
                && !viewer.transcript.has_active_interaction()
            {
                close = true;
            } else {
                viewer.transcript.handle_event(tui, event)?;
                close = viewer.transcript.is_done();
            }
        } else if !matches!(
            &event,
            TuiEvent::Draw | TuiEvent::Resume | TuiEvent::Resize(_) | TuiEvent::FocusGained
        ) {
            // Paste can only edit transcript search. It never reaches confirmation or composer.
            viewer.transcript.handle_event(tui, event)?;
        }
        if close {
            if viewer.confirmation_visible {
                self.chat_widget.dismiss_audit_activity_confirmation();
            }
            self.audits.activity_wake = None;
            self.overlay = Some(Overlay::AuditActivity(viewer));
            self.close_transcript_overlay(tui);
            tui.set_mouse_capture(tui::MouseCapture::Inherit)?;
            return Ok(());
        }
        let mut scanning = false;
        tui.draw(u16::MAX, |frame| {
            scanning = viewer.render(frame.area(), frame.buffer_mut());
            if let Some(cursor) = viewer.transcript.activity_cursor() {
                frame.set_cursor_position(cursor);
            }
            if viewer.confirmation_visible {
                self.chat_widget
                    .render_audit_activity_confirmation(frame.area(), frame.buffer_mut());
            }
        })?;
        if scanning {
            tui.frame_requester()
                .schedule_frame_in(crate::tui::TARGET_FRAME_INTERVAL);
        }
        self.overlay = Some(Overlay::AuditActivity(viewer));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
