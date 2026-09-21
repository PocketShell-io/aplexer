//! Codex NATIVE rollout transcript (`~/.codex/sessions/.../<id>.jsonl`).
//! Parses only `response_item` rows (the raw per-turn model log) to avoid
//! double-counting against the separate `event_msg` progress-notification
//! rows, which mirror the same content.

use super::*;

// ---------------------------------------------------------------------
// Codex NATIVE rollout transcript (`~/.codex/sessions/.../<id>.jsonl`).
// Parses only `response_item` rows (the raw per-turn model log) to avoid
// double-counting against the separate `event_msg` progress-notification
// rows, which mirror the same content.
// ---------------------------------------------------------------------

fn codex_native_text_parts(content: &Value, allowed: &[&str]) -> Vec<String> {
    match content {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![t.to_string()]
            }
        }
        Value::Object(_) => {
            let block_type = content.get("type").and_then(|t| t.as_str());
            if let Some(bt) = block_type {
                if !allowed.contains(&bt) {
                    return Vec::new();
                }
            }
            if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                let t = text.trim();
                if !t.is_empty() {
                    return vec![t.to_string()];
                }
            }
            content
                .get("content")
                .map(|c| codex_native_text_parts(c, allowed))
                .unwrap_or_default()
        }
        Value::Array(items) => items
            .iter()
            .flat_map(|i| codex_native_text_parts(i, allowed))
            .collect(),
        _ => Vec::new(),
    }
}

fn codex_native_tool_output(output: &Value) -> String {
    match output {
        // Tool output is often a plain string containing meaningful line
        // breaks and indentation. Preserve it, including an empty result.
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => {
            let parts = codex_native_text_parts(other, &["input_text", "output_text", "text"]);
            if parts.is_empty() {
                other.to_string()
            } else {
                parts.join("\n\n")
            }
        }
    }
}

fn stamp_tool_call_id(event: &mut UnifiedEvent, item: &Value) {
    if let Some(id) = str_field(item, "call_id") {
        event.metadata.insert("tool_call_id".into(), json!(id));
    }
}

pub(crate) fn codex_native_events(payload: &Value) -> Vec<UnifiedEvent> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Vec::new();
    }
    let Some(item) = payload.get("payload") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match item.get("type").and_then(|t| t.as_str()) {
        Some("message") => {
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "user" || role == "assistant" {
                let parts = codex_native_text_parts(
                    item.get("content").unwrap_or(&Value::Null),
                    &["input_text", "output_text", "text"],
                );
                let text = parts.join("\n\n");
                if !text.is_empty() {
                    let mut e = ev("message");
                    e.role = Some(role.to_string());
                    e.content = text;
                    out.push(e);
                }
            }
        }
        Some("custom_tool_call" | "function_call") => {
            let mut e = ev("tool_call");
            e.role = Some("assistant".to_string());
            e.tool_name = str_field(item, "name");
            let input = item.get("arguments").or_else(|| item.get("input"));
            e.tool_input = input.map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
            stamp_tool_call_id(&mut e, item);
            out.push(e);
        }
        Some("custom_tool_call_output" | "function_call_output") => {
            if let Some(output) = item.get("output") {
                let mut e = ev("tool_result");
                e.tool_output = Some(codex_native_tool_output(output));
                stamp_tool_call_id(&mut e, item);
                out.push(e);
            }
        }
        // "reasoning" and other response_item shapes: no stable text field
        // to surface (codex's reasoning items ship only encrypted content
        // on this CLI version) -- deliberately skipped, not an omission bug.
        _ => {}
    }
    out
}

/// `{"type":"session_meta","payload":{"id":"<thread-id>",...}}` -- the
/// codex rollout's own thread/session identifier (matches the `thread_id`
/// carried by every later `event_msg` row in the same file).
pub(crate) fn codex_native_continuation(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    payload.get("payload").and_then(|p| str_field(p, "id"))
}

/// The codex rollout's own working directory, from the same `session_meta`
/// row -- used by `locate_codex_transcript` to disambiguate candidate files
/// beyond the mtime heuristic.
pub(crate) fn codex_native_cwd(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    payload.get("payload").and_then(|p| str_field(p, "cwd"))
}
