//! Notification-driven presentation of file-backed audits.
use super::*;
use codex_app_server_protocol::{AuditProgress, AuditRun};

#[derive(Default)]
pub(super) struct AuditUiState {
    runs: HashMap<(ThreadId, String), AuditRun>,
    pub(super) reports: crate::audit::ReportContexts,
    awaiting_comparison: std::collections::HashSet<(ThreadId, String)>,
    compared_runs: std::collections::HashSet<String>,
    pending_errors: HashMap<(ThreadId, String), String>,
    ready_comparisons: HashMap<ThreadId, Vec<String>>,
    displayed: HashMap<(ThreadId, String), String>,
    summaries: HashMap<(ThreadId, String), String>,
}

impl App {
    pub(super) fn audit_available(&self, session: &AppServerSession) -> bool {
        !session.uses_embedded_app_server()
            && !crate::uses_remote_workspace_or_environment(
                &self.app_server_target,
                self.environment_manager.as_ref(),
            )
    }
    pub(super) fn audit_command(
        &mut self,
        session: &AppServerSession,
        thread_id: ThreadId,
        args: String,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        if !self.audit_available(session) {
            self.chat_widget.add_error_message("Audits require the local managed app-server daemon. Start it with `audex app-server daemon start` and reconnect.".into());
            return;
        }
        let handle = session.request_handle();
        let events = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                crate::audit::execute(
                    handle,
                    thread_id,
                    crate::audit::parse_command(&args)?,
                    events.clone(),
                )
                .await
            }
            .await;
            if let Err(message) = result {
                events.send(AppEvent::AuditError { thread_id, message });
            }
        });
    }
    pub(super) fn audit_opened(
        &mut self,
        session: &AppServerSession,
        thread: ThreadId,
        run: AuditRun,
        compare: bool,
    ) {
        let key = (thread, run.run_id.clone());
        if (compare || !run.complete)
            && !self.audits.compared_runs.contains(&run.run_id)
            && !self
                .audits
                .awaiting_comparison
                .iter()
                .any(|(_, id)| id == &run.run_id)
        {
            self.audits.awaiting_comparison.insert(key.clone());
        }
        // An explicit read/picker selection shows the current counts on demand.
        self.audits.displayed.remove(&key);
        self.audits.runs.insert(key, run.clone());
        self.audit_updated(thread, &run, None);
        if !run.complete {
            // Close the race between start/read response and subscription to progress.
            // This is one read on opening; subsequent reads follow completion events.
            self.refresh_audit(session, thread, run.run_id);
        }
    }
    fn refresh_audit(&self, session: &AppServerSession, thread_id: ThreadId, id: String) {
        let client = codex_app_server_client::audit::AuditClient::new(session.request_handle());
        let events = self.app_event_tx.clone();
        tokio::spawn(async move {
            match client.read(codex_app_server_client::audit::read_run(id)).await {
                Ok(result) => events.send(AppEvent::AuditUpdated { thread_id, run: result.run, error: None }),
                Err(error) => events.send(AppEvent::AuditError { thread_id, message: format!("Audit read failed: {error}. Reopen with /audit read RUN; the audit was not cancelled.") }),
            }
        });
    }
    pub(super) fn audit_progress(&mut self, session: &AppServerSession, progress: &AuditProgress) {
        let keys: Vec<_> = self
            .audits
            .runs
            .keys()
            .filter(|(_, id)| id == &progress.run_id)
            .cloned()
            .collect();
        for key in keys {
            if progress.complete || (progress.reviewer.is_none() && progress.error.is_some()) {
                if let Some(message) = &progress.error
                    && let Some(run) = self.audits.runs.get(&key).cloned()
                {
                    self.audit_updated(key.0, &run, Some(message.clone()));
                }
                self.refresh_audit(session, key.0, key.1);
                continue;
            }
            let Some(mut run) = self.audits.runs.get(&key).cloned() else {
                continue;
            };
            if let Some(status) = &progress.reviewer {
                run.complete = false;
                run.summary = None;
                if let Some(old) = run
                    .reviewers
                    .iter_mut()
                    .find(|s| s.reviewer == status.reviewer && s.attempt == status.attempt)
                {
                    *old = status.clone();
                } else {
                    run.reviewers.push(status.clone());
                }
            }
            self.audit_updated(key.0, &run, progress.error.clone());
        }
    }
    pub(super) fn audit_updated(
        &mut self,
        thread: ThreadId,
        run: &AuditRun,
        error: Option<String>,
    ) {
        let key = (thread, run.run_id.clone());
        if let Some(error) = error {
            self.audits.pending_errors.insert(key.clone(), error);
        }
        let merged = merge_read(self.audits.runs.get(&key), run);
        let run = &merged;
        self.audits.runs.insert(key.clone(), run.clone());
        self.audits.reports.stage(thread, run);
        if run.complete
            && self.audits.awaiting_comparison.remove(&key)
            && crate::audit::latest(run)
                .iter()
                .filter(|s| s.state == "succeeded")
                .count()
                >= 2
            && self.audits.compared_runs.insert(run.run_id.clone())
        {
            self.audits
                .ready_comparisons
                .entry(thread)
                .or_default()
                .push(crate::audit::comparison_prompt(run));
        }
        if self.current_displayed_thread_id() != Some(thread) {
            return;
        }
        // Counts stay current in the run and its read/picker views. Only meaningful
        // status changes append another progress block to conversation history.
        let mut without_tool_counts = run.clone();
        for status in &mut without_tool_counts.reviewers {
            status.tool_calls = 0;
        }
        let progress_key = crate::audit::progress(&without_tool_counts);
        if self.audits.displayed.get(&key) != Some(&progress_key) {
            self.audits.displayed.insert(key.clone(), progress_key);
            self.chat_widget
                .add_info_message(crate::audit::progress(run), None);
        }
        if let Some(summary) = &run.summary
            && self.audits.summaries.get(&key) != Some(summary)
        {
            self.audits.summaries.insert(key.clone(), summary.clone());
            let mut lines = Vec::new();
            crate::markdown::append_markdown(summary, None, None, &mut lines);
            self.chat_widget.add_plain_history_lines(lines);
            self.chat_widget
                .add_info_message(run.source_notice.clone(), None);
        }
        if let Some(error) = self.audits.pending_errors.remove(&key) {
            self.chat_widget.add_error_message(error);
        }
        self.queue_ready_audit_comparisons(thread);
    }
    pub(super) fn show_ready_audits(&mut self, thread: ThreadId) {
        let runs: Vec<_> = self
            .audits
            .runs
            .iter()
            .filter(|((owner, _), _)| *owner == thread)
            .map(|(_, run)| run.clone())
            .collect();
        for run in runs {
            self.audit_updated(thread, &run, None);
        }
        self.queue_ready_audit_comparisons(thread);
    }
    pub(super) fn queue_ready_audit_comparisons(&mut self, thread: ThreadId) {
        if self.current_displayed_thread_id() != Some(thread) {
            return;
        }
        if let Some(prompts) = self.audits.ready_comparisons.remove(&thread) {
            for prompt in prompts {
                if !self.chat_widget.queue_internal_follow_up(prompt) {
                    self.chat_widget.add_error_message("The automatic comparison could not be queued. Findings remain available through audit_read.".into());
                }
            }
        }
    }
}

impl App {
    pub(super) fn reconnect_audits(&self, session: &AppServerSession) {
        for ((thread, id), run) in &self.audits.runs {
            if !run.complete {
                self.refresh_audit(session, *thread, id.clone());
            }
        }
    }
}

// Responses can overtake notifications. Never replace a known terminal attempt
// with its earlier queued/running view, or lose an attempt started by operator retry.
fn merge_read(current: Option<&AuditRun>, incoming: &AuditRun) -> AuditRun {
    let Some(current) = current else {
        return incoming.clone();
    };
    let mut next = incoming.clone();
    let mut missing_attempt = false;
    for previous in &current.reviewers {
        if let Some(status) = next
            .reviewers
            .iter_mut()
            .find(|s| s.reviewer == previous.reviewer && s.attempt == previous.attempt)
        {
            if !matches!(previous.state.as_str(), "queued" | "running")
                && matches!(status.state.as_str(), "queued" | "running")
            {
                *status = previous.clone();
            }
            status.tool_calls = status.tool_calls.max(previous.tool_calls);
        } else {
            next.reviewers.push(previous.clone());
            missing_attempt = true;
        }
    }
    if missing_attempt {
        next.complete = false;
        next.summary = None;
    }
    let same_attempts = next.reviewers.len() == current.reviewers.len()
        && next.reviewers.iter().all(|s| {
            current
                .reviewers
                .iter()
                .any(|p| p.reviewer == s.reviewer && p.attempt == s.attempt)
        });
    if current.complete && same_attempts {
        next.complete = true;
        next.summary = current.summary.clone();
    }
    next
}

#[cfg(test)]
#[path = "audit_service_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "audit_service_correction_tests.rs"]
mod correction_tests;
