//! Small public diagnostics shared by capture and frozen reports. Never retain
//! arbitrary error text, search strings, host paths or private reasoning here.
use super::{AccessLimits, SnapshotPath, TOOL_NAMES};
use serde_json::{Value, json};

pub(in crate::audex) fn operation(name: &str) -> Option<&str> {
    let name = name
        .strip_prefix("mcp__audex_source__")
        .or_else(|| name.strip_prefix("audex_source__"))
        .unwrap_or(name);
    TOOL_NAMES.contains(&name).then_some(name)
}

pub(in crate::audex) fn arguments_json(name: &str, raw: &str) -> Value {
    if raw.len() > 16 * 1024 {
        return json!({"unavailable":"arguments exceed 16 KiB"});
    }
    match serde_json::from_str(raw) {
        Ok(value) => arguments(name, &value),
        Err(_) => json!({"unavailable":"invalid JSON"}),
    }
}

pub(in crate::audex) fn arguments(name: &str, input: &Value) -> Value {
    let mut safe = json!({});
    let Some(name) = operation(name) else {
        return safe;
    };
    if name == "list" {
        return safe;
    }
    if let Some(path) = input.get("path") {
        if path.is_null()
            || path.as_str().is_some_and(|path| {
                SnapshotPath::new(path).is_ok()
                    && !path
                        .chars()
                        .any(|c| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
            })
        {
            safe["path"] = path.clone();
        } else {
            safe["path_status"] = json!("redacted: invalid source-relative path");
        }
    }
    let keys: &[&str] = if name == "read" {
        &["offset", "max_bytes"]
    } else {
        &["max_results"]
    };
    for key in keys {
        if let Some(value) = input.get(*key).filter(|value| value.is_number()) {
            safe[*key] = value.clone();
        }
    }
    if name == "search" {
        if let Some(query) = input["query"].as_str() {
            safe["query_bytes"] = json!(query.len());
        } else if let Some(bytes) = input["query_bytes"].as_u64() {
            safe["query_bytes"] = json!(bytes);
        }
    }
    // Preserve only our fixed diagnostic markers when reprojecting retained starts.
    for (key, values) in [
        (
            "path_status",
            &["redacted: invalid source-relative path"][..],
        ),
        (
            "unavailable",
            &["arguments exceed 16 KiB", "invalid JSON"][..],
        ),
    ] {
        if input[key]
            .as_str()
            .is_some_and(|value| values.contains(&value))
        {
            safe[key] = input[key].clone();
        }
    }
    safe
}

pub(in crate::audex) fn failure(message: Option<&str>) -> Value {
    let mut value = json!({"error":"source_tool_error","detail":"unavailable"});
    if let Some(message) = message
        && let Some(category) = category(message)
    {
        value["category"] = json!(category);
        value["message"] = json!(message);
        value["detail"] = json!("available");
    }
    value
}

fn category(message: &str) -> Option<&'static str> {
    // Match entire messages, never prefixes/substrings: transports can carry
    // arbitrary storage diagnostics with a safe-looking phrase inside them.
    if message.len() > 256 {
        return None;
    }
    let limits = AccessLimits::default();
    if [
        format!(
            "read size must be between 1 and {} bytes",
            limits.max_read_bytes
        ),
        format!(
            "literal query must be between 1 and {} bytes",
            limits.max_search_query_bytes
        ),
        format!(
            "search result limit must be between 1 and {}",
            limits.max_search_results
        ),
    ]
    .iter()
    .any(|safe| safe == message)
        || matches!(
            message,
            "source access limit exceeded"
                | "read offset does not fit this platform"
                | super::RESULT_LIMIT_MESSAGE
        )
        || message.strip_prefix("read offset ").is_some_and(|rest| {
            rest.split_once(" exceeds the entry size ")
                .is_some_and(|(offset, size)| {
                    [offset, size].into_iter().all(|n| {
                        !n.is_empty()
                            && n.bytes().all(|b| b.is_ascii_digit())
                            && n.parse::<u64>().is_ok()
                    })
                })
        })
    {
        return Some("source_access_limit");
    }
    match message {
        "source tool arguments exceed 16 KiB" => Some("source_argument_limit"),
        "invalid source tool arguments" => Some("invalid_source_arguments"),
        "invalid manifest path" => Some("invalid_manifest_path"),
        "path is not in the snapshot manifest" => Some("path_not_in_manifest"),
        "reviewer attempt is stopped" => Some("attempt_stopped"),
        _ => None,
    }
}
