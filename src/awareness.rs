//! Model-facing coordination context for native engine hooks.
//!
//! Every engine `a init` wires now calls `a context hook --engine <engine>`
//! at turn and tool boundaries with the engine's own hook JSON on stdin.
//! This module turns that payload into a bounded, model-visible nudge:
//! who else is working in the session's workspace (via `coordination`),
//! which peer mail is unread (ids only -- never bodies), and, when an
//! explicit tool argument names an absolute path outside the session's
//! workspace, which peers are active in the destination instead.
//!
//! Hard rules, in the order the hook imposes them:
//!
//! - Bounded: stdin is read with a 1 MiB cap and parsed once; a bigger or
//!   malformed payload is refused, never truncated into a half-truth.
//! - Bound to the actual session: `discover_session_id` must resolve to
//!   exactly one session record whose engine matches the hook's `--engine`
//!   (a `shell`-engine record hosts any CLI; a `zcodex` record answers the
//!   codex hook). Native conversation IDs occupy a separate namespace;
//!   the first ID for each engine binds independently to the aplexer session.
//!   Conflicting IDs and explicit subagent markers are ignored.
//! - Quiet: the hook runs inside ordinary agent tool turns, so lock
//!   contention on the awareness state file is answered with no output
//!   rather than a wait, and every error is swallowed by the CLI caller.
//!   Nothing here writes to a composer or wakes an idle session; the only
//!   effect ever is text the engine itself chose to inject.
//! - Per-consumer: each engine/event consumer keeps its own stable
//!   fingerprint (rendered context + unread ids + foreign destinations,
//!   never timestamps or ages) and a cooldown, so a fingerprint that did
//!   not change does not re-fire on every tool call. Startup events bypass
//!   both after identity checks: bootstrap is never fingerprint-suppressed.
//!   Legacy notice claims and explicit message acknowledgements use separate
//!   state and do not affect these fingerprints.
//!
//! Per-engine wire schemas (verified against each CLI's hook docs):
//!
//! - Claude, Codex: `SessionStart`/`UserPromptSubmit`/`PostToolUse` in,
//!   `{"hookSpecificOutput": {"hookEventName", "additionalContext"}}` out.
//! - Grok: camelCase `sessionId`/`toolName`/`hookEventName`, PostToolUse
//!   `additionalContext` only -- startup stdout is ignored by the CLI, so
//!   no startup wiring exists for it.
//! - Gemini: `SessionStart`/`BeforeAgent`/`AfterTool`, same
//!   `hookSpecificOutput` shape as Claude.
//! - Antigravity: `PreInvocation` with `workspacePaths` in; out is
//!   `{"injectSteps": [{"ephemeralMessage": "..."}]}`. Its
//!   PostToolUse cannot inject, so it is not wired.
//! - OpenCode: the plugin shells out on `tool.execute.after` with a flat
//!   `{hook_event_name, tool_name, session_id, tool_input}` JSON and gets
//!   plain text back, which it appends to `output.output` byte-for-byte.

mod binding;
mod payload;
mod render;
mod state;

#[cfg(test)]
mod tests;

use crate::messaging::MessageEnvelope;
use crate::Paths;
use anyhow::{bail, Result};
use std::io::Read;

pub use payload::MAX_HOOK_INPUT_BYTES;

/// Bounded and opaque by construction: the hook's whole job is deciding
/// whether a bounded payload belongs to this session, and if so answering
/// with engine-specific injection JSON or plain text.
pub fn hook_context(paths: &Paths, engine: &str, input: impl Read) -> Result<Option<String>> {
    let payload = payload::parse_hook_payload(input)?;
    let Some((event, startup)) = injectable_event(engine, payload.event.as_deref())? else {
        return Ok(None);
    };
    // Subagents inherit APLEXER_SESSION_ID but not the conversation; their
    // tool turns must not inject main-thread context.
    if payload.subagent {
        return Ok(None);
    }
    let Some(session) = binding::bind_session(paths, engine)? else {
        return Ok(None);
    };
    let ctx = crate::coordination::context(paths, Some(session.id), &session.workspace)?;
    let rendered = crate::coordination::render_context(&ctx);
    let unread: Vec<MessageEnvelope> = crate::coordination::unread_messages(paths, session.id)?;
    let mailboxes = crate::coordination::mailbox_workspaces(paths, session.id)?;
    let foreign = render::foreign_peers(paths, &session, &payload)?;
    let print = state::fingerprint(&rendered, &unread, &foreign);
    let admission = state::admit(
        paths,
        session.id,
        engine,
        &consumer_key(engine, event),
        payload.session_id.as_deref(),
        print,
        startup,
    )?;
    if !admission.emit {
        return Ok(None);
    }
    let text = render::compose(&render::Composition {
        session: &session,
        rendered: &rendered,
        unread: &unread,
        mailboxes: &mailboxes,
        foreign: &foreign,
        bootstrap: admission.needs_bootstrap,
    });
    Ok(Some(emit_context(engine, event, &text)))
}

/// Which events this engine's hook can inject on, and whether the fire is
/// the always-delivered startup (`SessionStart`-family) or a
/// fingerprint-gated update. Grok ignores hook stdout at startup and
/// Antigravity's PostToolUse cannot inject, so neither is wired there;
/// Antigravity's native payload carries no event name -- the engine
/// argument implies PreInvocation.
fn injectable_event(engine: &str, event: Option<&str>) -> Result<Option<(&'static str, bool)>> {
    match (engine, event) {
        ("claude", Some("SessionStart")) => Ok(Some(("SessionStart", true))),
        ("claude", Some("UserPromptSubmit")) => Ok(Some(("UserPromptSubmit", false))),
        ("claude", Some("PostToolUse")) => Ok(Some(("PostToolUse", false))),
        ("codex", Some("SessionStart")) => Ok(Some(("SessionStart", true))),
        ("codex", Some("UserPromptSubmit")) => Ok(Some(("UserPromptSubmit", false))),
        ("codex", Some("PostToolUse")) => Ok(Some(("PostToolUse", false))),
        // Startup stdout is ignored: PostToolUse is the only injection point.
        ("grok", Some("PostToolUse")) => Ok(Some(("PostToolUse", false))),
        ("gemini", Some("SessionStart")) => Ok(Some(("SessionStart", true))),
        ("gemini", Some("BeforeAgent")) => Ok(Some(("BeforeAgent", false))),
        ("gemini", Some("AfterTool")) => Ok(Some(("AfterTool", false))),
        ("antigravity", None | Some("PreInvocation")) => Ok(Some(("PreInvocation", true))),
        // The opencode plugin sends the native event name it shells out on.
        ("opencode", Some("tool.execute.after") | None) => Ok(Some(("tool.execute.after", false))),
        (engine, _) => {
            if matches!(
                engine,
                "claude" | "codex" | "grok" | "gemini" | "antigravity" | "opencode"
            ) {
                Ok(None)
            } else {
                bail!("unknown engine {engine:?} for context hooks");
            }
        }
    }
}

/// One awareness consumer per engine event: `claude:PostToolUse`,
/// `opencode:tool.execute.after`, ... so a suppressed PostToolUse never
/// silences a changed UserPromptSubmit.
fn consumer_key(engine: &str, event: &str) -> String {
    format!("{engine}:{event}")
}

/// Engine-specific injection envelope. Claude-family and Gemini wrap the
/// text as `additionalContext`; Antigravity takes ephemeral inject steps
/// (exactly `{"injectSteps":[{"ephemeralMessage": ...}]}`); the OpenCode
/// plugin asked for plain text (it appends to the tool output itself).
fn emit_context(engine: &str, event: &str, text: &str) -> String {
    match engine {
        "antigravity" => serde_json::json!({
            "injectSteps": [{"ephemeralMessage": text}]
        })
        .to_string(),
        "opencode" => text.to_string(),
        _ => serde_json::json!({"hookSpecificOutput": {
            "hookEventName": event, "additionalContext": text
        }})
        .to_string(),
    }
}
