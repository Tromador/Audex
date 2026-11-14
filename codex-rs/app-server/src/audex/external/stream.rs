//! Bounded official-client JSONL projection into the existing capture journal.
//! Raw records and opaque stderr never leave this reader's bounded memory.
use super::super::capture::CaptureHandle;
use super::super::snapshot::manifest::sha256_hex;
use super::super::store::AuditStore;
use super::super::store::activity::ActivityInput;
use super::super::store::admission::AdmissionIdentity;
use super::super::store::client_stream::{ClientResultEvent, ClientTerminalArtefact, ClientUsage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{ChildStderr, ChildStdout};
use tokio::io::unix::AsyncFd;

mod repair;

type Result<T> = std::result::Result<T, &'static str>;
pub(super) const MAX_RECORD: usize = 2 * 1024 * 1024;
const MAX_STREAM: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_IDENTITIES: usize = 4096;
const MAX_STDERR: usize = 64 * 1024;

pub(super) struct Collector {
    pub store: AuditStore,
    pub capture: CaptureHandle,
    pub identity: AdmissionIdentity,
    pub route: String,
    pub billing_source: &'static str,
    pub expected_model: String,
    pub manifest: super::super::snapshot::manifest::SnapshotManifestV1,
    #[cfg(test)]
    pub checkpoint: Option<super::super::dispatch::Checkpoint>,
}

#[derive(Default)]
struct Message {
    model: String,
    blocks: HashMap<usize, Block>,
    streamed: bool,
    active_block: Option<usize>,
}
#[derive(Default)]
struct Block {
    kind: String,
    id: String,
    name: String,
    input: String,
    text_bytes: i64,
    full_seen: bool,
}
#[derive(Default)]
struct Stream {
    repairs: repair::Tracker,
    session: Option<String>,
    current: Option<String>,
    messages: HashMap<String, Message>,
    calls: HashMap<String, String>,
    seen: HashMap<String, String>,
    final_message: Option<(String, String, String)>,
}

impl Message {
    /// Full records can contain one block of a streamed message. Match the wire
    /// block identity/order, never the text, including private blocks' positions.
    fn full_indices(&mut self, blocks: &[Value]) -> Result<Vec<usize>> {
        let whole = self.streamed
            && blocks.len() == self.blocks.len()
            && blocks.iter().enumerate().all(|(index, block)| {
                self.blocks
                    .get(&index)
                    .is_some_and(|old| Some(old.kind.as_str()) == block["type"].as_str())
            });
        let mut indices = Vec::new();
        for (position, source) in blocks.iter().enumerate() {
            let kind = source["type"]
                .as_str()
                .ok_or("external_block_type_missing")?;
            let call = if kind == "tool_use" {
                Some(identifier(&source["id"])?)
            } else {
                None
            };
            let known_call = call.and_then(|call| {
                self.blocks
                    .iter()
                    .find(|(_, block)| block.kind == "tool_use" && block.id == call)
                    .map(|(index, _)| *index)
            });
            let index = if let Some(index) = known_call {
                index
            } else if whole {
                position
            } else if blocks.len() == 1 && self.active_block.is_some() {
                self.active_block.unwrap()
            } else if self.streamed {
                self.blocks
                    .iter()
                    .filter(|(_, block)| !block.full_seen && block.kind == kind)
                    .map(|(index, _)| *index)
                    .min()
                    .ok_or("external_full_block_unbound")?
            } else {
                // Without partial indices, the full representation's position
                // is its only established block identity. Repeated full records
                // cannot silently invent additional blocks for this message.
                position
            };
            if index >= MAX_IDENTITIES {
                return Err("external_block_index_invalid");
            }
            let block = self.blocks.entry(index).or_insert_with(|| Block {
                kind: kind.into(),
                id: call.unwrap_or_default().into(),
                ..Default::default()
            });
            if block.kind != kind || call.is_some_and(|call| block.id != call) {
                return Err("external_block_identity_conflict");
            }
            block.full_seen = true;
            indices.push(index);
        }
        Ok(indices)
    }
}

/// Shared completion evidence for audit and conversation capture.
pub(super) fn require_finished(
    last: Option<&str>,
    message: &str,
    observed: Option<&str>,
    terminal_stop: &Value,
) -> Result<()> {
    let terminal = terminal_stop.as_str();
    if last != Some(message)
        || (!terminal_stop.is_null() && terminal.is_none())
        || observed.is_some_and(|stop| stop != "end_turn")
        || terminal.is_some_and(|stop| stop != "end_turn")
        || (observed.is_none() && terminal.is_none())
    {
        return Err("external_final_generation_incomplete");
    }
    Ok(())
}

fn identifier(v: &Value) -> Result<&str> {
    v.as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 256
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        })
        .ok_or("external_stream_identity_invalid")
}
fn usage(v: &Value) -> ClientUsage {
    ClientUsage {
        input_tokens: v["input_tokens"].as_u64(),
        cache_read_input_tokens: v["cache_read_input_tokens"].as_u64(),
        cache_creation_input_tokens: v["cache_creation_input_tokens"].as_u64(),
        estimated_cost_usd: None,
        output_tokens: v["output_tokens"].as_u64(),
    }
}

pub(super) async fn read_pipe<T: Read + AsRawFd>(
    pipe: &mut AsyncFd<T>,
    bytes: &mut [u8],
) -> std::io::Result<usize> {
    loop {
        let mut ready = pipe.readable_mut().await?;
        match ready.try_io(|inner| inner.get_mut().read(bytes)) {
            Ok(result) => return result,
            Err(_) => continue,
        }
    }
}

impl Collector {
    pub(super) async fn collect(self, stdout: ChildStdout, stderr: ChildStderr) {
        let result = self.read(stdout, stderr).await;
        if let Err(reason) = result {
            // Preserve the admitted public prefix before failure closes capture.
            // A fast EOF or malformed tail may precede the periodic flush.
            // Flush retains the existing control/ownership fences and records
            // storage failure itself; neither outcome verifies a complete review.
            let _ = self.capture.flush().await;
            self.capture.failed(reason).await;
        }
    }

    async fn read(&self, stdout: ChildStdout, stderr: ChildStderr) -> Result<()> {
        let mut stdout = AsyncFd::new(stdout).map_err(|_| "external_stdout_transport_failed")?;
        let mut stderr = AsyncFd::new(stderr).map_err(|_| "external_stderr_transport_failed")?;
        let mut state = Stream::default();
        let mut line = Vec::new();
        let mut bytes = [0u8; 8192];
        let mut diagnostics = [0u8; 4096];
        let mut total = 0;
        let mut records = 0;
        let mut stderr_bytes = 0;
        let mut stderr_open = true;
        loop {
            tokio::select! {
                biased;
                // Diagnostics have no trusted public schema: retain only byte counts.
                result = read_pipe(&mut stderr, &mut diagnostics), if stderr_open => {
                    let n = result.map_err(|_| "external_stderr_transport_failed")?;
                    stderr_open = n != 0;
                    stderr_bytes += n;
                    if stderr_bytes > MAX_STDERR { return Err("external_stderr_bound_exhausted"); }
                    if n != 0 {
                        self.capture.offer(ActivityInput {
                            producer_key: format!("client-stderr:{stderr_bytes}"), turn_id: self.identity.execution_id.clone(),
                            item_id: String::new(), kind: "status".into(),
                            payload: format!("client stderr: {stderr_bytes} bytes observed; contents redacted"), text_offset: None,
                        }).await.map_err(|_| "external_capture_write_failed")?;
                    }
                }
                result = read_pipe(&mut stdout, &mut bytes) => {
                    let n = result.map_err(|_| "external_stdout_transport_failed")?;
                    if n == 0 {
                        return Err(if line.is_empty() { "external_eof_without_terminal" } else { "external_truncated_jsonl_tail" });
                    }
                    if !self.capture.allowed().await { return Err("external_capture_authority_unavailable"); }
                    total += n;
                    if total > MAX_STREAM { return Err("external_stream_byte_bound_exhausted"); }
                    for byte in &bytes[..n] {
                        if *byte != b'\n' {
                            if line.len() == MAX_RECORD { return Err("external_jsonl_record_bound_exhausted"); }
                            line.push(*byte);
                            continue;
                        }
                        records += 1;
                        if records > MAX_RECORDS { return Err("external_stream_record_bound_exhausted"); }
                        let event: Value = serde_json::from_slice(&line).map_err(|_| "external_malformed_jsonl")?;
                        line.clear();
                        if let Some(terminal) = self.event(&mut state, &event).await? {
                            #[cfg(test)]
                            if let Some(checkpoint) = &self.checkpoint { checkpoint("client_terminal_received").await; }
                            // Join any in-flight batch/failure before publishing terminal
                            // evidence. A failed flush cannot race a recoverable success.
                            self.capture.flush().await.map_err(|_| "external_capture_write_failed")?;
                            if self.capture.failure().is_some() { return Err("external_capture_write_failed"); }
                            let prefix = self.capture.control_prefix();
                            let persisted = self.store.persist_client_terminal(&terminal, self.capture.identity(), &prefix).await;
                            self.capture.completion_finished(persisted.is_ok());
                            persisted.map_err(|_| "external_terminal_evidence_write_failed")?;
                            self.capture.notify_committed();
                            #[cfg(test)]
                            if let Some(checkpoint) = &self.checkpoint { checkpoint("terminal_persisted").await; }
                            // Terminal success ends the stream authority. Full lifecycle receipts are C6.3.
                            self.capture.request_interrupt();
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    async fn offer(
        &self,
        session: &str,
        item: &str,
        key: String,
        kind: &str,
        payload: String,
        offset: Option<i64>,
    ) -> Result<()> {
        self.capture
            .offer(ActivityInput {
                producer_key: key,
                turn_id: session.into(),
                item_id: item.into(),
                kind: kind.into(),
                payload,
                text_offset: offset,
            })
            .await
            .map_err(|_| "external_capture_write_failed")
    }

    async fn model(
        &self,
        session: &str,
        id: &str,
        model: Option<&str>,
        counters: Option<&ClientUsage>,
    ) -> Result<()> {
        self.store
            .bind_client_session(&self.identity, session, true)
            .await
            .map_err(|_| "external_session_binding_failed")?;
        self.store
            .observe_bound_client_message(&self.identity, session, id, model, counters)
            .await
            .map_err(|_| "external_message_attribution_failed")?;
        if model != Some(self.expected_model.as_str()) {
            return Err("external_message_attribution_failed");
        }
        Ok(())
    }

    async fn event(
        &self,
        state: &mut Stream,
        value: &Value,
    ) -> Result<Option<ClientTerminalArtefact>> {
        // Optional identity assertions cannot override the inherited owned-pipe binding.
        for (name, expected) in [
            ("execution_id", &self.identity.execution_id),
            ("nonce", &self.identity.execution_id),
            ("operation_id", &self.identity.operation_id),
            ("assignment_id", &self.identity.assignment_id),
            ("run_id", &self.identity.run_id),
        ] {
            if value
                .get(name)
                .is_some_and(|v| v.as_str() != Some(expected))
            {
                return Err("external_execution_identity_mismatch");
            }
        }
        if value
            .get("parent_tool_use_id")
            .is_some_and(|v| !v.is_null())
            || value.get("agent_id").is_some_and(|v| !v.is_null())
        {
            return Err("external_unbound_subagent_evidence");
        }
        let session = identifier(&value["session_id"])?;
        let kind = value["type"]
            .as_str()
            .ok_or("external_record_type_missing")?;
        // The pinned bare client can emit this UI-only notification before init.
        // It supplies no review, session acceptance or model evidence.
        if state.session.is_none() && kind == "system" && value["subtype"] == "ui_invalidate" {
            return Ok(None);
        }
        if let Some(previous) = &state.session {
            if previous != session {
                return Err("external_session_identity_mismatch");
            }
        } else {
            if kind != "system" || value["subtype"] != "init" {
                return Err("external_session_init_missing");
            }
            if self.route == "claude_subscription" && value["apiKeySource"] != self.billing_source {
                // Init follows prompt delivery. Retain the submission boundary;
                // this mismatch is never evidence that a charge was prevented.
                return Err("external_billing_source_mismatch_after_submission_charge_unknown");
            }
            self.store
                .bind_client_session(&self.identity, session, false)
                .await
                .map_err(|_| "external_session_binding_failed")?;
            state.session = Some(session.into());
        }
        let uuid = identifier(&value["uuid"])?;
        let digest =
            sha256_hex(&serde_json::to_vec(value).map_err(|_| "external_malformed_jsonl")?);
        if let Some(previous) = state.seen.get(uuid) {
            if previous != &digest {
                return Err("external_event_identity_conflict");
            }
            return Ok(None);
        }
        if state.seen.len() >= MAX_RECORDS {
            return Err("external_event_identity_bound_exhausted");
        }
        state.seen.insert(uuid.into(), digest); // ephemeral only, including hashes; never persisted
        if self.route == "claude_subscription"
            && let Some(notice) = state.repairs.observe(value)?
        {
            if !self
                .capture
                .client_policy_notice(notice.activity)
                .await
                .map_err(|_| "external_policy_disclosure_write_failed")?
            {
                return Err("external_policy_observation_fenced");
            }
            #[cfg(test)]
            if let Some(checkpoint) = &self.checkpoint {
                checkpoint("client_continuation_observed").await;
            }
        }
        #[cfg(test)]
        if self
            .store
            .root_path()
            .join("m5-compaction-percent")
            .exists()
            && matches!(kind, "assistant" | "result")
            && (value["error"].is_string() || value["is_error"] == true)
        {
            // Qualification diagnostics contain only fixed labels, never client
            // error text, credentials or arbitrary provider response fields.
            let labels = [
                "success",
                "error_max_budget_usd",
                "error_during_execution",
                "authentication_failed",
                "billing_error",
                "rate_limit",
                "invalid_request",
                "server_error",
                "unknown",
                "budget_exhausted",
                "api_error",
                "blocking_limit",
                "prompt_too_long",
                "model_error",
                "turn_setup_failed",
            ];
            let selected = ["error", "subtype", "terminal_reason"].map(|field| {
                (
                    field,
                    value[field]
                        .as_str()
                        .filter(|v| labels.contains(v))
                        .unwrap_or("unclassified"),
                )
            });
            let text = value["message"]["content"].to_string().to_lowercase();
            let terms = [
                "budget", "credit", "quota", "limit", "auth", "expired", "rate", "429", "400",
                "401", "403", "500",
            ];
            let matched = terms
                .into_iter()
                .filter(|term| text.contains(term))
                .collect::<Vec<_>>();
            eprintln!(
                "M5 client failure labels: {selected:?}; diagnostic word matches: {matched:?}"
            );
        }
        match kind {
            "system" => {
                let status = match value["subtype"].as_str() {
                    Some("init") => "client_session_observed",
                    Some("status") => "client_status_observed",
                    Some("compact_boundary") => "client_compaction_completed",
                    Some("api_retry") => "client_transport_retry_observed",
                    _ => "client_system_event_redacted",
                };
                self.offer(
                    session,
                    "",
                    format!("client-status:{uuid}"),
                    "status",
                    status.into(),
                    None,
                )
                .await?;
            }
            "stream_event" => self.partial(state, session, uuid, &value["event"]).await?,
            "assistant" => {
                if value["aborted"] == true {
                    return Err("external_assistant_failed");
                }
                if value["error"].is_string() {
                    // The official client emits synthetic assistant errors before
                    // its terminal result (including a budget stop), and may retry
                    // them. Keep observing; this is neither model output nor a
                    // complete execution receipt. Never persist opaque errors.
                    self.offer(
                        session,
                        "",
                        format!("client-error:{uuid}"),
                        "status",
                        "client_error_observed_awaiting_terminal".into(),
                        None,
                    )
                    .await?;
                    return Ok(None);
                }
                let message = &value["message"];
                let id = identifier(&message["id"])?;
                let model = message["model"].as_str();
                let mut message_usage = usage(&message["usage"]);
                // Official per-step output is a placeholder. Only the terminal
                // aggregate supplies the client's accumulated output observation.
                message_usage.output_tokens = None;
                self.model(session, id, model, Some(&message_usage)).await?;
                let model = model.ok_or("external_model_missing")?;
                if state.messages.len() >= MAX_IDENTITIES && !state.messages.contains_key(id) {
                    return Err("external_message_bound_exhausted");
                }
                if let Some(old) = state.messages.get(id)
                    && old.model != model
                {
                    return Err("external_model_conflict");
                }
                state.messages.entry(id.into()).or_default().model = model.into();
                let blocks = message["content"]
                    .as_array()
                    .ok_or("external_assistant_content_invalid")?;
                let mut text = String::new();
                let mut tools = false;
                let indices = state
                    .messages
                    .get_mut(id)
                    .expect("bound message")
                    .full_indices(blocks)?;
                for (index, block) in indices.into_iter().zip(blocks) {
                    match block["type"].as_str() {
                        Some("text") => {
                            let content = block["text"]
                                .as_str()
                                .ok_or("external_public_text_invalid")?;
                            text.push_str(content);
                            let item = format!("{id}:{index}");
                            self.offer(
                                session,
                                &item,
                                format!("client-message:{item}"),
                                "assistant_message",
                                content.into(),
                                None,
                            )
                            .await?;
                        }
                        Some("tool_use") => {
                            tools = true;
                            self.tool(state, session, id, block).await?;
                        }
                        // Private reasoning, signatures and unknown content types have no public projection.
                        _ => {}
                    }
                }
                state.final_message = if !tools && !text.is_empty() {
                    Some((id.into(), model.into(), text))
                } else {
                    None
                };
            }
            "user" => {
                if let Some(blocks) = value["message"]["content"].as_array() {
                    for block in blocks {
                        if block["type"] != "tool_result" {
                            continue;
                        }
                        let call = identifier(&block["tool_use_id"])?;
                        if !state.calls.contains_key(call) {
                            return Err("external_unbound_tool_result");
                        }
                        let payload = safe_tool_result(block)?;
                        self.offer(
                            session,
                            call,
                            format!("client-tool-result:{call}"),
                            "tool_result",
                            payload.to_string(),
                            None,
                        )
                        .await?;
                    }
                }
            }
            "result" => {
                let mut terminal_usage = usage(&value["usage"]);
                let cost = value["total_cost_usd"]
                    .as_number()
                    .filter(|n| n.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0))
                    .cloned();
                terminal_usage.estimated_cost_usd = cost.as_ref().map(ToString::to_string);
                if value["subtype"] != "success" || value["is_error"] != false {
                    // Retain safe failure usage/receipt in the same journal; opaque
                    // errors and diagnostic text have no public projection.
                    let subtype = match value["subtype"].as_str() {
                        Some("success") => "success_with_error",
                        Some("error_max_budget_usd") => "error_max_budget_usd",
                        Some("error_max_turns") => "error_max_turns",
                        Some("error_during_execution") => "error_during_execution",
                        Some("error_max_structured_output_retries") => {
                            "error_max_structured_output_retries"
                        }
                        _ => "unrecognised_failure",
                    };
                    self.offer(session, uuid, format!("client-terminal-failed:{uuid}"), "status",
                        json!({"client_terminal":"failed","terminal_subtype":subtype,"terminal_id":uuid,"usage":terminal_usage}).to_string(), None).await?;
                    self.capture
                        .flush()
                        .await
                        .map_err(|_| "external_capture_write_failed")?;
                    return Err("external_unsuccessful_terminal");
                }
                let (message, model, text) = state
                    .final_message
                    .as_ref()
                    .ok_or("external_final_public_message_missing")?;
                if value["result"].as_str() != Some(text.as_str()) {
                    return Err("external_terminal_public_text_mismatch");
                }
                if self.route == "claude_subscription" {
                    // A cut-off repair may return only its last generation.
                    // Reuse the shared semantic validator; never concatenate
                    // captured fragments or turn an unstructured tail into an opinion.
                    let finished = state
                        .repairs
                        .require_finished(message, &value["stop_reason"])
                        .and_then(|()| {
                            if state.repairs.has_cutoff_continuation()
                                && !super::super::result::extract(
                                    text.as_bytes(),
                                    &self.identity.assignment_id,
                                    &self.manifest,
                                )
                                .is_ok_and(|opinion| opinion.structured.is_some())
                            {
                                Err("external_incomplete_continuation")
                            } else {
                                Ok(())
                            }
                        });
                    if let Err(reason) = finished {
                        self.offer(session, uuid, format!("client-terminal-failed:{uuid}"), "status",
                            json!({"client_terminal":"failed","terminal_id":uuid,"usage":terminal_usage}).to_string(), None).await?;
                        // Closing failed capture fences later writes. Retain the
                        // eligible original prefix first, as for error terminals.
                        self.capture
                            .flush()
                            .await
                            .map_err(|_| "external_capture_write_failed")?;
                        return Err(reason);
                    }
                }
                self.offer(
                    session,
                    uuid,
                    format!("client-terminal:{uuid}"),
                    "status",
                    "client_terminal_success_observed".into(),
                    None,
                )
                .await?;
                return Ok(Some(ClientTerminalArtefact {
                    version: 1,
                    identity: self.identity.clone(),
                    route: self.route.clone(),
                    final_message_id: message.clone(),
                    final_message_model: model.clone(),
                    final_text: text.clone(),
                    terminal: ClientResultEvent {
                        r#type: "result".into(),
                        subtype: "success".into(),
                        is_error: false,
                        uuid: uuid.into(),
                        session_id: session.into(),
                        result: text.clone(),
                        usage: terminal_usage,
                        num_turns: value["num_turns"].as_u64(),
                        duration_ms: value["duration_ms"].as_u64(),
                        total_cost_usd: cost,
                    },
                }));
            }
            "rate_limit_event" => {
                self.offer(
                    session,
                    uuid,
                    format!("client-status:{uuid}"),
                    "status",
                    "client_rate_limit_status_observed".into(),
                    None,
                )
                .await?
            }
            _ => return Err("external_record_type_unsupported"),
        }
        Ok(None)
    }

    async fn tool(
        &self,
        state: &mut Stream,
        session: &str,
        message: &str,
        block: &Value,
    ) -> Result<()> {
        let call = identifier(&block["id"])?;
        let name = block["name"].as_str().ok_or("external_tool_name_missing")?;
        let keys: &[&str] = match name {
            "mcp__audex_source__list" => &[],
            "mcp__audex_source__read" => &["path", "offset", "max_bytes"],
            "mcp__audex_source__search" => &["query", "path", "max_results"],
            _ => return Err("external_tool_not_allowed"),
        };
        if state.calls.len() >= MAX_IDENTITIES && !state.calls.contains_key(call) {
            return Err("external_call_bound_exhausted");
        }
        if let Some(previous) = state.calls.insert(call.into(), message.into())
            && previous != message
        {
            return Err("external_tool_identity_conflict");
        }
        let input = &block["input"];
        if !input.is_object() {
            return Err("external_tool_arguments_invalid");
        }
        let safe = project(input, keys);
        if safe.to_string().len() > 16 * 1024 {
            return Err("external_tool_arguments_bound_exhausted");
        }
        self.offer(
            session,
            call,
            format!("client-tool-start:{call}"),
            "tool_start",
            json!({"tool":name,"arguments":crate::audex::source::diagnostics::arguments(name, &safe)}).to_string(),
            None,
        )
        .await
    }

    async fn partial(
        &self,
        state: &mut Stream,
        session: &str,
        uuid: &str,
        event: &Value,
    ) -> Result<()> {
        match event["type"].as_str() {
            Some("message_start") => {
                let id = identifier(&event["message"]["id"])?;
                let model = event["message"]["model"].as_str();
                self.model(session, id, model, None).await?;
                if state.messages.len() >= MAX_IDENTITIES {
                    return Err("external_message_bound_exhausted");
                }
                if state.messages.contains_key(id) {
                    return Err("external_message_restart_unsupported");
                }
                state.messages.insert(
                    id.into(),
                    Message {
                        model: model.unwrap().into(),
                        streamed: true,
                        ..Default::default()
                    },
                );
                state.current = Some(id.into());
            }
            Some("content_block_start" | "content_block_delta" | "content_block_stop") => {
                let id = state
                    .current
                    .clone()
                    .ok_or("external_partial_message_unbound")?;
                let message = state
                    .messages
                    .get_mut(&id)
                    .ok_or("external_partial_message_unbound")?;
                let index = event["index"]
                    .as_u64()
                    .filter(|n| *n < MAX_IDENTITIES as u64)
                    .ok_or("external_block_index_invalid")? as usize;
                match event["type"].as_str().unwrap() {
                    "content_block_start" => {
                        if message.blocks.contains_key(&index) {
                            return Err("external_block_identity_conflict");
                        }
                        let source = &event["content_block"];
                        let kind = source["type"]
                            .as_str()
                            .ok_or("external_block_type_missing")?;
                        let mut block = Block {
                            kind: kind.into(),
                            ..Default::default()
                        };
                        if kind == "tool_use" {
                            block.id = identifier(&source["id"])?.into();
                            block.name = source["name"]
                                .as_str()
                                .ok_or("external_tool_name_missing")?
                                .into();
                            if source["input"].as_object().is_some_and(|o| !o.is_empty()) {
                                block.input = source["input"].to_string();
                            }
                        } else if kind == "text" {
                            let text = source["text"]
                                .as_str()
                                .ok_or("external_public_text_invalid")?;
                            if !text.is_empty() {
                                self.offer(
                                    session,
                                    &format!("{id}:{index}"),
                                    format!("client-delta:{uuid}"),
                                    "assistant_delta",
                                    text.into(),
                                    Some(0),
                                )
                                .await?;
                                block.text_bytes = text.len() as i64;
                            }
                        }
                        message.blocks.insert(index, block);
                        message.active_block = Some(index);
                    }
                    "content_block_delta" => {
                        let block = message
                            .blocks
                            .get_mut(&index)
                            .ok_or("external_partial_block_unbound")?;
                        let delta = &event["delta"];
                        if block.kind == "text" && delta["type"] == "text_delta" {
                            let text = delta["text"]
                                .as_str()
                                .ok_or("external_public_text_invalid")?;
                            self.offer(
                                session,
                                &format!("{id}:{index}"),
                                format!("client-delta:{uuid}"),
                                "assistant_delta",
                                text.into(),
                                Some(block.text_bytes),
                            )
                            .await?;
                            block.text_bytes += text.len() as i64;
                        } else if block.kind == "tool_use" && delta["type"] == "input_json_delta" {
                            let text = delta["partial_json"]
                                .as_str()
                                .ok_or("external_tool_arguments_invalid")?;
                            if block.input.len() + text.len() > 16 * 1024 {
                                return Err("external_tool_arguments_bound_exhausted");
                            }
                            block.input.push_str(text);
                        }
                    }
                    _ => {
                        let block = message
                            .blocks
                            .get(&index)
                            .ok_or("external_partial_block_unbound")?;
                        if block.kind == "tool_use" {
                            let input: Value = if block.input.is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&block.input)
                                    .map_err(|_| "external_tool_arguments_invalid")?
                            };
                            let tool = json!({"id":block.id,"name":block.name,"input":input});
                            self.tool(state, session, &id, &tool).await?;
                        }
                        state
                            .messages
                            .get_mut(&id)
                            .expect("bound message")
                            .active_block = None;
                    }
                }
            }
            Some("message_stop") => {
                state.current = None;
            }
            Some("message_delta" | "ping") => {}
            _ => return Err("external_partial_type_unsupported"),
        }
        Ok(())
    }
}

fn project(value: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                value
                    .get(*key)
                    .filter(|v| v.is_string() || v.is_number() || v.is_boolean() || v.is_null())
                    .map(|v| ((*key).into(), v.clone()))
            })
            .collect(),
    )
}

fn safe_tool_result(block: &Value) -> Result<Value> {
    if block["is_error"] == true {
        // Only complete, allow-listed source messages cross this privacy boundary.
        // Unknown envelopes or redacted errors still retain the failure fact.
        let content = &block["content"];
        let text = content.as_str().or_else(|| {
            let blocks = content.as_array()?;
            let mut texts = blocks.iter().filter(|b| b["type"] == "text");
            let text = texts.next()?["text"].as_str()?;
            texts.next().is_none().then_some(text)
        });
        return Ok(crate::audex::source::diagnostics::failure(text));
    }
    let content = &block["content"];
    let text = if let Some(text) = content.as_str() {
        text.to_owned()
    } else {
        content
            .as_array()
            .ok_or("external_tool_result_invalid")?
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<String>()
    };
    let value: Value = serde_json::from_str(&text).map_err(|_| "external_tool_result_invalid")?;
    if let Some(entries) = value["entries"].as_array() {
        Ok(
            json!({"entries": entries.iter().map(|v| project(v,&["path","kind","executable","size","sha256"])).collect::<Vec<_>>()}),
        )
    } else if let Some(hits) = value["hits"].as_array() {
        Ok(
            json!({"hits": hits.iter().map(|v| project(v,&["path","sha256","offset"])).collect::<Vec<_>>()}),
        )
    } else if value["path"].is_string() && value["content"].is_string() {
        Ok(project(
            &value,
            &[
                "path",
                "sha256",
                "offset",
                "end_offset",
                "eof",
                "encoding",
                "content",
            ],
        ))
    } else {
        Err("external_tool_result_schema_unknown")
    }
}
