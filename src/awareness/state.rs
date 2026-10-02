//! Per-session awareness state: the native harness conversation ids bound
//! per engine, which engines have already seen their bootstrap, and
//! per-consumer injection suppression (stable fingerprint + cooldown) so
//! an unchanged coordination picture does not re-fire on every tool call
//! while a real change still gets through immediately.
//!
//! Message ACKs live elsewhere (`messaging::notice`'s claim state and the
//! explicit cursor); nothing here acknowledges mail.

use super::*;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// Minimum spacing between two injections of the same unchanged
/// fingerprint, matching the mailbox notice's cooldown.
pub(crate) const CONTEXT_COOLDOWN_SECS: u64 = 10 * 60;
/// Awareness state is small and per-session; refuse to grow it without
/// bound (one entry per consumer kind, but a hostile state file stays
/// bounded like every other mailbox state file).
const MAX_STATE_BYTES: usize = 64 * 1024;
/// Most engines ever hooked per session; the map is cleared wholesale if
/// a hostile state file grows past this.
const MAX_CONSUMERS: usize = 64;

#[derive(Debug, Default, Serialize, Deserialize)]
struct AwarenessState {
    /// Native harness conversation id per hook engine. First fire wins;
    /// a different id later means some *other* conversation is running
    /// under this session's environment (a nested agent, a second CLI)
    /// and gets nothing.
    #[serde(default)]
    native_ids: BTreeMap<String, String>,
    /// Engines whose context (bootstrap or first update) was delivered.
    #[serde(default)]
    engines_seen: BTreeSet<String>,
    /// Last injected fingerprint per consumer kind (`engine:event`).
    #[serde(default)]
    consumers: BTreeMap<String, ConsumerState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConsumerState {
    fingerprint: u64,
    at: u64,
}

/// The admission decision for one hook fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admission {
    /// Emit context at all (fingerprint changed / cooldown elapsed /
    /// startup; `false` on suppression or lock contention).
    pub(crate) emit: bool,
    /// Include the bootstrap instructions: startup events always, and the
    /// first emission of an engine that has no startup hook (Grok's
    /// PostToolUse, OpenCode's tool.execute.after) carries them too.
    pub(crate) needs_bootstrap: bool,
}

fn state_path(paths: &Paths, session: uuid::Uuid) -> PathBuf {
    paths
        .state_root
        .join("awareness")
        .join(format!("{session}.json"))
}

/// The fingerprint hashes only stable identities: the rendered context
/// (core excludes volatile timestamps/ages), the unread message ids, and
/// the foreign destinations with their peer renders. Ages, wall-clock
/// times, and cooldown bookkeeping never enter it.
pub(crate) fn fingerprint(
    rendered: &str,
    unread: &[MessageEnvelope],
    foreign: &[(PathBuf, String)],
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rendered.hash(&mut hasher);
    for message in unread {
        message.id.hash(&mut hasher);
    }
    for (destination, peers) in foreign {
        destination.hash(&mut hasher);
        peers.hash(&mut hasher);
    }
    hasher.finish()
}

/// Native conversation ids are engine-generated, but a hostile or runaway
/// payload could carry a huge one; the awareness state file is capped at
/// 64 KiB and must never be poisoned by a single oversized id. Anything
/// longer than this (or carrying control characters) is rejected -- the
/// fire gets nothing and nothing is written.
const MAX_NATIVE_ID_BYTES: usize = 512;

fn usable_native_id(native_id: Option<&str>) -> bool {
    native_id.is_none_or(|id| id.len() <= MAX_NATIVE_ID_BYTES && !id.chars().any(char::is_control))
}

/// Decide one fire and record it. Every failure mode is quiet: lock
/// contention (another hook fire in flight) and unreadable state both
/// answer "no" for startup and updates alike -- "startup never
/// suppressed" means startup bypasses the fingerprint/cooldown
/// suppression, never the identity checks. A malformed, oversized, or
/// non-regular state file is *poisoned*: rewriting it fresh would erase
/// the native-id binding, so the fire gets nothing and the file stays.
///
/// Known limitation, documented: the first native conversation id an
/// engine fires under wins, so two *independent* harness conversations
/// taking turns inside one aplexer session would starve the second one
/// rather than interleave their context.
pub(crate) fn admit(
    paths: &Paths,
    session: uuid::Uuid,
    engine: &str,
    consumer: &str,
    native_id: Option<&str>,
    fingerprint: u64,
    startup: bool,
) -> Result<Admission> {
    let none = Admission {
        emit: false,
        needs_bootstrap: false,
    };
    // A hostile native id (oversized, control characters) never reaches
    // the state file: reject the fire outright.
    if !usable_native_id(native_id) {
        return Ok(none);
    }
    let dir = paths.state_root.join("awareness");
    crate::ensure_private_dir(&dir)?;
    let path = state_path(paths, session);
    let lock = match crate::FileLock::exclusive(&path.with_extension("lock"), true) {
        Ok(lock) => lock,
        Err(_) => return Ok(none),
    };
    let _guard = lock;
    let mut state = match read_state(&path) {
        Ok(Some(state)) => state,
        Ok(None) => AwarenessState::default(),
        // Poisoned state: never rewrite the binding away.
        Err(_) => return Ok(none),
    };
    let mut changed = false;
    // Native conversation-id binding: first fire per engine establishes,
    // a conflicting id later is a different conversation and gets nothing
    // (this is an identity guard, not suppression -- it holds at startup
    // too).
    if let Some(native) = native_id {
        match state.native_ids.get(engine) {
            Some(established) if established != native => {
                return Ok(Admission {
                    emit: false,
                    needs_bootstrap: false,
                });
            }
            None => {
                state
                    .native_ids
                    .insert(engine.to_string(), native.to_string());
                changed = true;
            }
            _ => {}
        }
    }
    let needs_bootstrap = !state.engines_seen.contains(engine);
    let emit = if startup {
        changed |= state.engines_seen.insert(engine.to_string());
        true
    } else {
        let now = crate::messaging::now_secs();
        let emit = match state.consumers.get(consumer) {
            Some(previous) => {
                previous.fingerprint != fingerprint
                    || now.saturating_sub(previous.at) >= CONTEXT_COOLDOWN_SECS
            }
            None => true,
        };
        if emit {
            state.consumers.insert(
                consumer.to_string(),
                ConsumerState {
                    fingerprint,
                    at: now,
                },
            );
            changed = true;
            // First delivery of an engine without a startup hook carries
            // the bootstrap instructions.
            changed |= state.engines_seen.insert(engine.to_string());
        }
        emit
    };
    if state.consumers.len() > MAX_CONSUMERS {
        state.consumers.clear();
    }
    if changed {
        crate::atomic_write_json(&path, &state)?;
    }
    Ok(Admission {
        emit,
        needs_bootstrap: startup || (emit && needs_bootstrap),
    })
}

/// `Ok(None)` for an absent file (fresh state is fine to create), an
/// error for anything unreadable (malformed JSON, oversized, non-regular
/// file) -- the caller must not replace a state it cannot read.
fn read_state(path: &std::path::Path) -> Result<Option<AwarenessState>> {
    match crate::persist::read_bounded_regular_file(path, "awareness state", MAX_STATE_BYTES) {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) => serde_json::from_slice(&bytes)
            .map(Some)
            .context("parse awareness state"),
        Err(e) => Err(e).context("read awareness state"),
    }
}
