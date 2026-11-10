//! Qualified 2.1.314 client corrections. This observes sends; it never admits one.
use super::{Result, identifier};
use crate::audex::store::activity::ActivityInput;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(super) const CUTOFF: &str = "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";
pub(super) const EMPTY: &str = "[Your previous response had no visible output. Please continue and produce a user-visible response.]";
pub(super) const TOOL: &str =
    "The previous response failed to produce a valid tool call. Please retry the tool call now.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Cutoff,
    Empty,
    Tool,
}
impl Class {
    fn name(self) -> &'static str {
        match self {
            Self::Cutoff => "max_tokens_continuation",
            Self::Empty => "empty_end_turn",
            Self::Tool => "missing_tool_call",
        }
    }
    fn bound(self) -> u8 {
        match self {
            Self::Cutoff => 3,
            Self::Empty => 1,
            Self::Tool => 2,
        }
    }
}

#[derive(Default)]
struct Generation {
    stop: Option<String>,
    visible: bool,
    tools: HashSet<String>,
    results: HashSet<String>,
}
impl Generation {
    fn class(&self) -> Option<Class> {
        match self.stop.as_deref() {
            Some("max_tokens") => Some(Class::Cutoff),
            Some("end_turn") if !self.visible && self.tools.is_empty() => Some(Class::Empty),
            Some("tool_use") if self.tools.is_empty() => Some(Class::Tool),
            _ => None,
        }
    }
    fn tool_continuation(&self) -> bool {
        !self.tools.is_empty()
            && self.tools == self.results
            && matches!(self.stop.as_deref(), Some("tool_use") | None)
    }
}

pub(super) struct Notice {
    pub activity: ActivityInput,
    pub anomaly: bool,
}

#[derive(Default)]
pub(super) struct Tracker {
    messages: HashMap<String, Generation>,
    last: Option<String>,
    notices: HashSet<String>,
    prompted: Option<Class>,
    chain: Option<(Class, u8)>,
    cutoff_continuation: bool,
}

impl Tracker {
    /// The pinned client can emit a successful result for an unfinished turn.
    /// Require end_turn evidence bound to the final generation, and refuse any
    /// conflicting terminal stop. Missing evidence is not a completed opinion.
    pub(super) fn require_finished(&self, message: &str, terminal_stop: &Value) -> Result<()> {
        let observed = self.messages.get(message).and_then(|g| g.stop.as_deref());
        super::require_finished(self.last.as_deref(), message, observed, terminal_stop)
    }

    pub(super) fn has_cutoff_continuation(&self) -> bool {
        self.cutoff_continuation
    }
    fn notice(
        &mut self,
        session: &str,
        anchor: &str,
        class: Option<Class>,
        evidence: &'static str,
    ) -> Option<Notice> {
        // A later end_turn can contain only the continuation tail. Remember
        // the cut-off even when a later tool round resets the qualified chain.
        self.cutoff_continuation |= class == Some(Class::Cutoff);
        // The fixed prompt and the following generation describe the same send.
        let key = format!("client-policy:{anchor}");
        if !self.notices.insert(key.clone()) {
            return None;
        }
        let within_qualification = class.is_some_and(|class| {
            let count = match self.chain {
                Some((old, count)) if old == class => count.saturating_add(1),
                _ => 1,
            };
            self.chain = Some((class, count));
            count <= class.bound()
        });
        let label = if within_qualification {
            "client-initiated repair occurred"
        } else {
            "client request policy anomaly"
        };
        Some(Notice { anomaly: !within_qualification, activity: ActivityInput {
            producer_key: key,
            turn_id: session.into(),
            item_id: anchor.into(),
            kind: "status".into(),
            payload: json!({
                "label": label,
                "class": class.map(Class::name),
                "evidence": evidence,
                "qualification_exceeded": class.is_some() && !within_qualification,
                "predecessor_message_id": self.last,
                "stop_reason": self.last.as_ref().and_then(|id| self.messages.get(id)).and_then(|g| g.stop.as_deref()),
                "limit": "observed client behaviour; not Audex Retry or a request ceiling",
                "stop": "best-effort interruption requested; send prevention and remote charges unconfirmed"
            }).to_string(),
            text_offset: None,
        }})
    }

    fn generation(&mut self, session: &str, id: &str, start: bool, uuid: &str) -> Option<Notice> {
        if self.messages.contains_key(id) {
            return if start {
                self.notice(session, uuid, None, "repeated_message_start")
            } else {
                None
            };
        }
        let notice = if let Some(previous) = self.last.clone() {
            let generation = &self.messages[&previous];
            let class = self.prompted.take().or_else(|| generation.class());
            if class.is_some() {
                self.notice(session, &previous, class, "subsequent_generation")
            } else if generation.tool_continuation() {
                self.chain = None;
                None
            } else {
                self.notice(session, uuid, None, "unexpected_generation")
            }
        } else {
            self.prompted = None;
            None
        };
        self.messages.insert(id.into(), Generation::default());
        self.last = Some(id.into());
        notice
    }

    pub(super) fn observe(&mut self, value: &Value) -> Result<Option<Notice>> {
        if self.notices.len() >= super::MAX_IDENTITIES {
            return Err("external_policy_observation_bound_exhausted");
        }
        let session = identifier(&value["session_id"])?;
        let uuid = identifier(&value["uuid"])?;
        let (event, partial) = if value["type"] == "stream_event" {
            (&value["event"], true)
        } else {
            (value, false)
        };
        let mut notice = None;
        match event["type"].as_str() {
            Some("message_start" | "assistant") => {
                let message = &event["message"];
                let id = identifier(&message["id"])?;
                if self.messages.len() >= super::MAX_IDENTITIES && !self.messages.contains_key(id) {
                    return Err("external_message_bound_exhausted");
                }
                notice = self.generation(session, id, partial, uuid);
                let generation = self.messages.get_mut(id).expect("observed generation");
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        observe_block(generation, block)?;
                    }
                }
                if let Some(stop) = message["stop_reason"].as_str() {
                    generation.stop = Some(safe_stop(stop).into());
                }
            }
            Some("message_delta") => {
                if let Some(generation) =
                    self.last.as_ref().and_then(|id| self.messages.get_mut(id))
                    && let Some(stop) = event["delta"]["stop_reason"].as_str()
                {
                    generation.stop = Some(safe_stop(stop).into());
                }
            }
            Some("content_block_start" | "content_block_delta") => {
                if let Some(generation) =
                    self.last.as_ref().and_then(|id| self.messages.get_mut(id))
                {
                    if event["type"] == "content_block_start" {
                        observe_block(generation, &event["content_block"])?;
                    } else if event["delta"]["type"] == "text_delta" {
                        generation.visible |= event["delta"]["text"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty());
                    }
                }
            }
            Some("user") => {
                let content = &value["message"]["content"];
                let text = content.as_str().or_else(|| {
                    let blocks = content.as_array()?;
                    (blocks.len() == 1 && blocks[0]["type"] == "text")
                        .then(|| blocks[0]["text"].as_str())
                        .flatten()
                });
                let class = match text {
                    Some(CUTOFF) => Some(Class::Cutoff),
                    Some(EMPTY) => Some(Class::Empty),
                    Some(TOOL) => Some(Class::Tool),
                    _ => None,
                };
                if let Some(class) = class {
                    let anchor = self.last.clone().unwrap_or_else(|| uuid.into());
                    let contradictory = self
                        .last
                        .as_ref()
                        .and_then(|id| self.messages.get(id))
                        .is_some_and(|g| g.stop.is_some() && g.class() != Some(class));
                    if contradictory {
                        notice =
                            self.notice(session, uuid, None, "fixed_prompt_inconsistent_with_stop");
                    } else {
                        self.prompted = Some(class);
                        notice =
                            self.notice(session, &anchor, Some(class), "fixed_corrective_prompt");
                    }
                } else if let Some(blocks) = content
                    .as_array()
                    .filter(|b| !b.is_empty() && b.iter().all(|v| v["type"] == "tool_result"))
                {
                    for block in blocks {
                        let call = identifier(&block["tool_use_id"])?;
                        if let Some(generation) =
                            self.last.as_ref().and_then(|id| self.messages.get_mut(id))
                        {
                            generation.results.insert(call.into());
                        }
                    }
                } else {
                    notice = self.notice(session, uuid, None, "unexpected_client_prompt");
                }
            }
            Some("system") if value["subtype"] == "api_retry" => {
                notice = self.notice(session, uuid, None, "client_resend_observed");
            }
            _ => {}
        }
        Ok(notice)
    }
}

fn observe_block(generation: &mut Generation, block: &Value) -> Result<()> {
    if block["type"] == "text" {
        generation.visible |= block["text"].as_str().is_some_and(|s| !s.is_empty());
    } else if block["type"] == "tool_use" {
        generation.tools.insert(identifier(&block["id"])?.into());
    }
    Ok(())
}

fn safe_stop(stop: &str) -> &str {
    match stop {
        "max_tokens" | "end_turn" | "tool_use" | "refusal" | "pause_turn" | "stop_sequence" => stop,
        _ => "unrecognised",
    }
}
