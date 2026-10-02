//! Bounded, per-consumer PostToolUse notice claims. This state is separate
//! from the explicit acknowledgement cursor.

use super::*;
use std::collections::BTreeMap;
use std::io::Read;

const MAX_HOOK_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_NOTICE_IDS: usize = 5;
pub const NOTICE_COOLDOWN_SECS: u64 = 10 * 60;

#[derive(Default, Deserialize, Serialize)]
struct NoticeState {
    #[serde(default)]
    claimed_at: BTreeMap<Uuid, u64>,
}

fn main_tool_event(input: impl Read) -> Result<bool> {
    let mut bytes = Vec::new();
    input
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HOOK_INPUT_BYTES {
        return Ok(false);
    }
    let event: Value = serde_json::from_slice(&bytes)?;
    Ok(
        event.get("hook_event_name").and_then(Value::as_str) == Some("PostToolUse")
            && event
                .get("session_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
            && event
                .get("tool_name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
            && event.get("agent_id").is_none(),
    )
}

fn bound_recipient(paths: &Paths, engine: &str) -> Result<Option<SessionIdentity>> {
    let Some(id) = crate::discover_session_id() else {
        return Ok(None);
    };
    let records = list_records(paths)?;
    let mut matches = records.iter().filter(|record| record.id == id);
    let Some(record) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || !engine_matches(&record.engine, engine) {
        return Ok(None);
    }
    let identity = resolve_identity(&records, &record.workspace, None)?;
    Ok(identity.filter(|identity| identity.workspace.as_deref() == Some(&record.workspace)))
}

fn engine_matches(record: &str, requested: &str) -> bool {
    record == "shell" || record == requested || (requested == "codex" && record == "zcodex")
}

fn notice_path(mp: &MessagePaths, id: Uuid) -> PathBuf {
    mp.cursors_dir.join(format!("{id}.notice"))
}

fn read_notice(path: &Path) -> Result<NoticeState> {
    let Some(bytes) = read_bounded_regular_file(path, "mailbox notice", MAX_MAILBOX_STATE_BYTES)?
    else {
        return Ok(NoticeState::default());
    };
    serde_json::from_slice(&bytes).context("parse mailbox notice")
}

fn notice_lock_path(mp: &MessagePaths, id: Uuid) -> PathBuf {
    mp.cursors_dir.join(format!("{id}.notice.lock"))
}

fn claim_notice(paths: &Paths, identity: &SessionIdentity) -> Result<Option<(usize, Vec<Uuid>)>> {
    // This callback is optional; a busy mailbox must not hold a tool
    // result. The unread snapshot comes from core (its multiworkspace,
    // addressing and retained-message filtering, nonblocking locks); the
    // claim bookkeeping below is guarded by its own dedicated lock so two
    // concurrent callbacks can never both notice the same ids.
    let snapshot = match crate::coordination::unread_messages(paths, identity.id) {
        Ok(messages) => messages,
        Err(_) => return Ok(None),
    };
    let unread: Vec<Uuid> = snapshot.iter().map(|message| message.id).collect();
    let home = identity.workspace.as_deref().unwrap();
    let mp = match ensure_workspace_nonblocking(paths, home) {
        Ok(mp) => mp,
        Err(_) => return Ok(None),
    };
    let _notice = match FileLock::exclusive(&notice_lock_path(&mp, identity.id), true) {
        Ok(lock) => lock,
        Err(_) => return Ok(None),
    };
    let path = notice_path(&mp, identity.id);
    let mut state = read_notice(&path)?;
    let before = state.claimed_at.len();
    state.claimed_at.retain(|id, _| unread.contains(id));
    let now = now_secs();
    let ids = eligible_ids(&unread, &state, now);
    if ids.is_empty() && state.claimed_at.len() == before {
        return Ok(None);
    }
    for id in &ids {
        state.claimed_at.insert(*id, now);
    }
    atomic_write_json(&path, &state)?;
    Ok((!ids.is_empty()).then_some((unread.len(), ids)))
}

fn eligible_ids(unread: &[Uuid], state: &NoticeState, now: u64) -> Vec<Uuid> {
    unread
        .iter()
        .copied()
        .filter(|id| {
            state
                .claimed_at
                .get(id)
                .is_none_or(|then| now.saturating_sub(*then) >= NOTICE_COOLDOWN_SECS)
        })
        .take(MAX_NOTICE_IDS)
        .collect()
}

/// Returns a model-visible JSON envelope, or no output when input or binding
/// is invalid. The caller intentionally swallows errors so tools keep running.
/// Unread is counted across every mailbox the session participates in (core
/// multiworkspaces); the claim state itself stays anchored to the session's
/// home workspace.
pub fn hook_notice(paths: &Paths, engine: &str, input: impl Read) -> Result<Option<String>> {
    if !main_tool_event(input)? {
        return Ok(None);
    }
    let Some(identity) = bound_recipient(paths, engine)? else {
        return Ok(None);
    };
    let Some((count, ids)) = claim_notice(paths, &identity)? else {
        return Ok(None);
    };
    let context = format!(
        "Aplexer: {count} unread peer message(s). IDs: {}. Run `a message inbox` to read; acknowledge explicitly with `a message ack`.",
        ids.iter().map(Uuid::to_string).collect::<Vec<_>>().join(", ")
    );
    Ok(Some(
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PostToolUse", "additionalContext": context
        }})
        .to_string(),
    ))
}
