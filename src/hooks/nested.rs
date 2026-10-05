//! Nested-hook JSON merge (Claude settings.json, Codex hooks.json,
//! Grok aplexer.json, Gemini settings.json share one shape).

use super::*;

// ---------------------------------------------------------------------------
// Nested-hook JSON merge (Claude settings.json, Codex hooks.json,
// Grok aplexer.json, Gemini settings.json share one shape)
// ---------------------------------------------------------------------------

/// One hook group in the nested format. `timeout` is in each engine's own
/// unit -- seconds for the Claude family, milliseconds for Gemini (whose
/// documented default is 60000 ms) -- so an awareness hook gets a real
/// thirty-second budget on either host.
fn our_group(command: String, timeout: Option<i64>) -> Value {
    let mut hook = serde_json::json!({"type": "command", "command": command});
    if let Some(timeout) = timeout {
        hook["timeout"] = serde_json::json!(timeout);
    }
    serde_json::json!({"hooks": [hook]})
}

/// Does a hook group already contain a state-report entry for `state`?
pub(crate) fn group_reports(group: &Value, state: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| reports_state(c, state))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// The command a `(event, state)` wiring installs.
fn wiring_command(state: &str, a_bin: &str) -> String {
    match state.strip_prefix("awareness:") {
        Some(engine) => context_command(a_bin, engine),
        None => state_report_command(a_bin, state),
    }
}

/// The timeout a `(event, state)` wiring carries: awareness hooks are
/// bounded everywhere, but the budget must absorb host-wide load spikes
/// (dozens of sessions firing hooks at once) and the throttled global mail
/// sweep, so it is generous in each host's unit; state-report hooks rely
/// on `|| true` alone.
fn wiring_timeout(state: &str) -> Option<i64> {
    match state.strip_prefix("awareness:") {
        Some("gemini") => Some(30000),
        Some(_) => Some(30),
        None => None,
    }
}

/// Aligns an existing group's managed entries with the current wiring:
/// the timeout (`wiring_timeout`) and, for awareness hooks, the command's
/// embedded binary path. Both must converge, or an install from before a
/// budget raise -- or from before `a` moved to another path -- would stay
/// stale forever (the merge would otherwise skip the group entirely, since
/// idempotence keys on the group already existing). Only entries this
/// wiring owns -- matched by content, path-independently -- are touched.
/// Returns 1 when the document changed.
fn refresh_managed_entry(group: &mut Value, state: &str, a_bin: &str) -> usize {
    let timeout = wiring_timeout(state);
    let current_command = match state.strip_prefix("awareness:") {
        Some(engine) => Some(context_command(a_bin, engine)),
        // State-report commands are not converged: `|| true` makes a stale
        // path harmless, and rewriting them is not this wiring's business.
        None => None,
    };
    let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut changed = 0;
    for hook in inner.iter_mut() {
        let ours = hook
            .get("command")
            .and_then(Value::as_str)
            .map(|c| reports_state(c, state))
            .unwrap_or(false);
        if !ours {
            continue;
        }
        let mut stale = false;
        if let (Some(timeout), Some(want)) = (timeout, current_command.as_deref()) {
            if hook.get("command").and_then(Value::as_str) != Some(want) {
                hook["command"] = serde_json::json!(want);
                stale = true;
            }
            if hook.get("timeout").and_then(Value::as_i64) != Some(timeout) {
                hook["timeout"] = serde_json::json!(timeout);
                stale = true;
            }
        }
        if stale {
            changed = 1;
        }
    }
    changed
}

/// Merge our `(event, state)` wirings into a nested-hooks document.
/// Returns the number of events changed (removals of superseded entries
/// count). Idempotent: a second merge with the same commands changes
/// nothing.
///
/// Migration: legacy managed inbox-notice commands (identified by their
/// content marker, any engine) are removed as part of the merge -- the
/// awareness wiring supersedes them, and leaving both would double-inject
/// on PostToolUse. Foreign commands are never touched.
///
/// Shape errors (non-object root, non-object `hooks`) are refused rather
/// than clobbered -- the file may hold something newer than this tool
/// understands. A non-array event slot is schema-invalid in every engine,
/// so it is replaced (there is nothing meaningful to preserve).
pub fn merge_nested_hooks(doc: &mut Value, events: &[(&str, &str)], a_bin: &str) -> Result<usize> {
    let root = doc.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("hooks document root is not a JSON object; left untouched")
    })?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("\"hooks\" key is not an object; left untouched"))?;
    let mut changed = strip_legacy_notices(hooks);
    for (event, state) in events {
        let slot = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let groups = match slot.as_array_mut() {
            Some(groups) => groups,
            None => {
                *slot = Value::Array(Vec::new());
                slot.as_array_mut().expect("just set to array")
            }
        };
        match groups.iter_mut().find(|g| group_reports(g, state)) {
            // Already wired: converge stale managed wiring (a budget raise
            // or a moved `a` must reach existing installs on their next
            // install), leaving foreign groups untouched.
            Some(group) => changed += refresh_managed_entry(group, state, a_bin),
            None => {
                let command = wiring_command(state, a_bin);
                groups.push(our_group(command, wiring_timeout(state)));
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// Remove superseded legacy inbox-notice hook entries from every event
/// group, ours-by-marker only. Returns how many hook entries were
/// removed -- inner commands count even when a shared group survives with
/// a foreign hook still in it, so a migration that only swaps managed
/// entries still reports a change and gets written.
fn strip_legacy_notices(hooks: &mut serde_json::Map<String, Value>) -> usize {
    let mut removed = 0;
    for (_, slot) in hooks.iter_mut() {
        let Some(groups) = slot.as_array_mut() else {
            continue;
        };
        groups.retain_mut(|group| {
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = inner.len();
            inner.retain(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| !is_legacy_notice_command(c))
                    .unwrap_or(true)
            });
            removed += before - inner.len();
            !inner.is_empty()
        });
    }
    removed
}

/// Removes every managed hook (state-report, the current awareness
/// context hook, and legacy inbox-notice commands) from a nested hooks
/// document, keyed by the ours-by-content command match alone, across ALL
/// event groups -- not just the events the current install tables name.
/// The event tables change between releases (SubagentStop was unmapped
/// from `idle` in 2026-09); an uninstall that iterated only the current
/// names would leave a retired event's entry behind forever, pushing
/// state from an event this version no longer believes in. Foreign
/// commands are never touched, whatever the event. Drops emptied groups,
/// events, and the top-level `hooks` object. Returns true when anything
/// changed.
pub fn unmerge_nested_hooks(doc: &mut Value) -> bool {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut kept = Vec::with_capacity(groups.len());
        for group in groups.drain(..) {
            match group {
                Value::Object(mut map) => {
                    let inner = map.get_mut("hooks").and_then(Value::as_array_mut);
                    match inner {
                        Some(inner) => {
                            let inner_before = inner.len();
                            inner.retain(|h| {
                                h.get("command")
                                    .and_then(Value::as_str)
                                    .map(|c| !is_managed_hook_command(c))
                                    .unwrap_or(true)
                            });
                            if inner.len() != inner_before {
                                changed = true;
                            }
                            if !inner.is_empty() {
                                kept.push(Value::Object(map));
                            } else {
                                changed = true;
                            }
                        }
                        None => kept.push(Value::Object(map)),
                    }
                }
                other => kept.push(other),
            }
        }
        if kept.is_empty() {
            hooks.remove(&event);
        } else {
            hooks.insert(event, Value::Array(kept));
        }
    }
    if hooks.is_empty() {
        if let Some(root) = doc.as_object_mut() {
            root.remove("hooks");
        }
    }
    changed
}

/// Which required `(event, state)` wirings are missing from a document.
/// Empty means installed. A missing/unparseable file counts as all
/// missing (callers treat absent files as "not installed", not as errors).
/// An event wired for two states (state-report + awareness on the same
/// `SessionStart`) is reported once.
pub fn missing_nested_hooks(doc: &Value, events: &[(&str, &str)]) -> Vec<String> {
    let mut missing = Vec::new();
    let hooks = doc.get("hooks").and_then(Value::as_object);
    for (event, state) in events {
        let present = hooks
            .and_then(|h| h.get(*event))
            .and_then(Value::as_array)
            .map(|groups| groups.iter().any(|g| group_reports(g, state)))
            .unwrap_or(false);
        if !present && !missing.iter().any(|name| name == event) {
            missing.push((*event).to_string());
        }
    }
    missing
}
