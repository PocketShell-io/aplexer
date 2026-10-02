//! Bounded hook payload reading and per-engine schema parsing.
//!
//! Every field is read defensively off one parsed `serde_json::Value`:
//! snake_case for the Claude/Codex/Gemini family and the OpenCode plugin
//! payload, camelCase for Grok, plus Antigravity's `workspacePaths`. Tool
//! arguments are never echoed -- only explicit path-ish keys are lifted,
//! and only when they are absolute strings.
//!
//! The payload's session id is the *native harness conversation id*
//! (Claude/Codex UUIDs, OpenCode `ses_...`, Antigravity `conversationId`).
//! It is kept as an opaque string and never compared against aplexer
//! session UUIDs -- different namespaces (see `binding`).

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The hook input cap, matching the mailbox notice's. A payload over this
/// is refused outright: truncating JSON mid-object would parse as garbage
/// anyway, and unbounded reads would let a runaway tool response stall an
/// ordinary agent tool call.
pub const MAX_HOOK_INPUT_BYTES: u64 = 1024 * 1024;

/// Explicit tool-argument keys that may name a workspace or file. Shell
/// command strings are deliberately absent: an arbitrary command's text is
/// never parsed to establish ownership.
pub const PATH_ARG_KEYS: [&str; 6] = [
    "workdir",
    "cwd",
    "directory",
    "file_path",
    "filePath",
    "path",
];

#[derive(Debug, Default)]
pub(crate) struct HookPayload {
    /// The engine's own event name, e.g. `SessionStart`, `PostToolUse`,
    /// `tool.execute.after`. Antigravity's native PreInvocation payload
    /// carries none -- the engine argument decides there.
    pub(crate) event: Option<String>,
    /// The native harness conversation id, opaque (any string shape).
    pub(crate) session_id: Option<String>,
    /// Subagent marker (`agent_id`/`agentId` present) -- any value counts.
    pub(crate) subagent: bool,
    /// Absolute paths named by explicit tool arguments (`tool_input`) and,
    /// for the flat OpenCode/Antigravity payloads, the top level.
    pub(crate) tool_paths: Vec<PathBuf>,
    /// Antigravity `PreInvocation` names the invocation's workspaces.
    pub(crate) workspace_paths: Vec<PathBuf>,
}

pub(crate) fn parse_hook_payload(input: impl Read) -> Result<HookPayload> {
    let mut bytes = Vec::new();
    input
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read hook payload")?;
    if bytes.len() as u64 > MAX_HOOK_INPUT_BYTES {
        anyhow::bail!("hook payload exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    let doc: Value = serde_json::from_slice(&bytes).context("parse hook payload as JSON")?;
    Ok(from_value(doc))
}

fn from_value(doc: Value) -> HookPayload {
    let string = |value: Option<&Value>| -> Option<String> {
        value
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    let event = string(doc.get("hook_event_name")).or_else(|| string(doc.get("hookEventName")));
    let session_id = string(doc.get("session_id")).or_else(|| string(doc.get("sessionId")));
    let session_id = session_id.or_else(|| string(doc.get("conversationId")));
    let subagent = doc.get("agent_id").is_some() || doc.get("agentId").is_some();
    let mut tool_paths = collect_paths(doc.get("tool_input").or_else(|| doc.get("toolInput")));
    // Flat payloads (the OpenCode plugin, Antigravity) carry the safe
    // arguments at the top level; Claude-family sessions also repeat their
    // cwd there.
    tool_paths.extend(collect_paths(Some(&doc)));
    let workspace_paths = doc
        .get("workspacePaths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(|p| p.as_str())
                .filter(|p| Path::new(p).is_absolute())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    HookPayload {
        event,
        session_id,
        subagent,
        tool_paths,
        workspace_paths,
    }
}

/// Absolute string values under the explicit path keys, shallow only.
/// `filePath` is read too (the OpenCode plugin's args are camelCase); only
/// the resulting path matters here, the JS side normalizes the key.
fn collect_paths(value: Option<&Value>) -> Vec<PathBuf> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Vec::new();
    };
    PATH_ARG_KEYS
        .iter()
        .filter_map(|key| map.get(*key))
        .filter_map(Value::as_str)
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
        .collect()
}
