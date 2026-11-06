//! Frozen source-tool outcomes, independent of the reviewer's opinion text.
use super::collection::write_literal;
use super::push_line;
use crate::audex::source::diagnostics;
use crate::audex::store::activity::ActivityRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct SourceToolFacts {
    pub calls: Vec<SourceToolCall>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::audex) struct SourceToolCall {
    pub session_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub tool: Option<String>,
    pub arguments: Option<Value>,
    pub start_sequence: Option<i64>,
    pub result_sequence: Option<i64>,
    pub outcome: Outcome,
    pub failure: Option<Value>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::audex) enum Outcome {
    Success,
    Failed,
    Unavailable,
}

pub(in crate::audex) fn collect(activity: &[ActivityRecord]) -> SourceToolFacts {
    // Journal bounds also bound this projection; retain first-observed sequence
    // order and join by the full capture identity, never by call ID alone.
    let mut calls = Vec::<SourceToolCall>::new();
    let mut indices = BTreeMap::new();
    for entry in activity {
        if !matches!(entry.kind.as_str(), "tool_start" | "tool_result") {
            continue;
        }
        let key = (&entry.session_id, &entry.turn_id, &entry.item_id);
        let index = *indices.entry(key).or_insert_with(|| {
            calls.push(SourceToolCall {
                session_id: entry.session_id.clone(),
                turn_id: entry.turn_id.clone(),
                call_id: entry.item_id.clone(),
                tool: None,
                arguments: None,
                start_sequence: None,
                result_sequence: None,
                outcome: Outcome::Unavailable,
                failure: None,
            });
            calls.len() - 1
        });
        let call = &mut calls[index];
        let value = serde_json::from_str::<Value>(&entry.payload).unwrap_or(Value::Null);
        if entry.kind == "tool_start" {
            call.start_sequence = Some(entry.sequence);
            if let Some(tool) = value["tool"].as_str().and_then(diagnostics::operation) {
                call.tool = Some(tool.to_owned());
                call.arguments = value
                    .get("arguments")
                    .map(|v| diagnostics::arguments(tool, v));
            }
        } else {
            call.result_sequence = Some(entry.sequence);
            if value["error"] == "source_tool_error" {
                call.outcome = Outcome::Failed;
                call.failure = Some(diagnostics::failure(value["message"].as_str()));
            } else if value["entries"].is_array()
                || value["hits"].is_array()
                || (value["path"].is_string() && value["content"].is_string())
            {
                call.outcome = Outcome::Success;
            }
        }
    }
    SourceToolFacts { calls }
}

pub(in crate::audex) fn render(
    output: &mut String,
    facts: &SourceToolFacts,
) -> Result<(), serde_json::Error> {
    let count = |outcome| {
        facts
            .calls
            .iter()
            .filter(|call| call.outcome == outcome)
            .count()
    };
    push_line(
        output,
        &format!(
            "- Retained source-tool outcomes: {} succeeded; {} failed; {} unavailable.",
            count(Outcome::Success),
            count(Outcome::Failed),
            count(Outcome::Unavailable)
        ),
    );
    push_line(
        output,
        "- Source-tool facts cover only the retained prefix at selection; they do not establish review coverage or opinion correctness.",
    );
    for call in &facts.calls {
        write_literal(
            output,
            "Source-tool call (not a finding)",
            &serde_json::to_string(call)?,
        );
        if call
            .failure
            .as_ref()
            .is_some_and(|v| v["detail"] == "unavailable")
        {
            push_line(
                output,
                "- Source-tool error detail unavailable; transport detail was absent or not safe to retain.",
            );
        }
        if call.outcome == Outcome::Unavailable {
            push_line(
                output,
                "- Source-tool outcome unavailable; no recognised result was retained for this call.",
            );
        }
    }
    Ok(())
}
