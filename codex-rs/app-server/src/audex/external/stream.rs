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

type Result<T> = std::result::Result<T, &'static str>;
const MAX_RECORD: usize = 2 * 1024 * 1024;
const MAX_STREAM: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_IDENTITIES: usize = 4096;
const MAX_STDERR: usize = 64 * 1024;

pub(super) struct Collector {
    pub store: AuditStore,
    pub capture: CaptureHandle,
    pub identity: AdmissionIdentity,
    pub route: String,
    pub expected_model: String,
    #[cfg(test)]
    pub checkpoint: Option<super::super::dispatch::Checkpoint>,
}

#[derive(Default)]
struct Message {
    model: String,
    blocks: HashMap<usize, Block>,
}
#[derive(Default)]
struct Block {
    kind: String,
    id: String,
    name: String,
    input: String,
    text_bytes: i64,
}
#[derive(Default)]
struct Stream {
    session: Option<String>,
    current: Option<String>,
    messages: HashMap<String, Message>,
    calls: HashMap<String, String>,
    seen: HashMap<String, String>,
    final_message: Option<(String, String, String)>,
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

async fn read_pipe<T: Read + AsRawFd>(
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
        if let Some(previous) = &state.session {
            if previous != session {
                return Err("external_session_identity_mismatch");
            }
        } else {
            if kind != "system" || value["subtype"] != "init" {
                return Err("external_session_init_missing");
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
        match kind {
            "system" => {
                let status = match value["subtype"].as_str() {
                    Some("init") => "client_session_observed",
                    Some("status") => "client_status_observed",
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
                if value["error"].is_string() || value["aborted"] == true {
                    return Err("external_assistant_failed");
                }
                let message = &value["message"];
                let id = identifier(&message["id"])?;
                let model = message["model"].as_str();
                self.model(session, id, model, Some(&usage(&message["usage"])))
                    .await?;
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
                for (index, block) in blocks.iter().enumerate() {
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
                    self.offer(session, uuid, format!("client-terminal-failed:{uuid}"), "status",
                        json!({"client_terminal":"failed","terminal_id":uuid,"usage":terminal_usage}).to_string(), None).await?;
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
            json!({"tool":name,"arguments":safe}).to_string(),
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
        return Ok(json!({"error":"source_tool_error"}));
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
