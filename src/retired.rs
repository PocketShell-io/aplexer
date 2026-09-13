//! Tombstones for sessions whose durable record is gone, so a late
//! `a kill` answers "already finished" with success instead of a spurious
//! failure (issue #2665).
//!
//! A session's records are removed deliberately in several places -- the
//! worker's finalization (clean exit or accepted kill), the CLI's
//! unreachable-worker recovery, `a forget --force`, `a prune`/the list
//! sweep -- and every one of them is a desired state an actor may learn
//! about late. `a kill` resolves its target before doing anything, so a
//! listing→kill race lands on "no matching session" and exit 1 even though
//! the kill's outcome is already fully achieved (#2661 hit exactly this).
//! Each removal therefore leaves a small tombstone under
//! `retired-sessions/<id>/`, and the kill's not-found path consults it. A
//! selector that never matched anything still fails loudly: a mistyped tag
//! must not become a silent no-op.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::paths::{canonical_workspace, ensure_private_dir, Paths};
use crate::persist::atomic_write_json;
use crate::util::now_ms;

pub const TOMBSTONE_FILE: &str = "tombstone.json";

/// How long a tombstone keeps answering "already finished" before `a prune`
/// removes it. Long enough for any realistic kill retry or stale listing,
/// short enough that the retired dir cannot grow without bound.
pub const TOMBSTONE_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Why the session's durable record was removed. The workload itself always
/// ended before any of these -- by exiting, by its kill, or by being
/// provably dead already -- which is what makes "already finished" honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TombstoneCause {
    /// The worker's own finalization removed it (clean exit or accepted
    /// kill RPC), or the CLI recovered an unreachable worker.
    Finished,
    /// `a forget --force` removed the record.
    Forgotten,
    /// `a prune` (or the default list's sweep) reaped a dead session.
    Pruned,
}

impl TombstoneCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            TombstoneCause::Finished => "finished",
            TombstoneCause::Forgotten => "forgotten",
            TombstoneCause::Pruned => "pruned",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishedTombstone {
    pub id: Uuid,
    pub workspace: PathBuf,
    pub tag: String,
    pub finished_at_ms: u64,
    pub cause: TombstoneCause,
}

/// Leaves the "this session is deliberately gone" marker. Best-effort by
/// design at call sites that are already mid-teardown: a missing tombstone
/// only degrades a later kill back to today's "no matching session", it
/// never breaks the removal itself.
pub fn write_finished_tombstone(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    tag: &str,
    cause: TombstoneCause,
) {
    let result = write_finished_tombstone_checked(paths, id, workspace, tag, cause);
    if let Err(error) = result {
        eprintln!("a: could not write finished-tombstone for session {id}: {error:#}");
    }
}

fn write_finished_tombstone_checked(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    tag: &str,
    cause: TombstoneCause,
) -> Result<()> {
    let dir = paths.retired_session(id);
    // A supersede archive owns this id when a full record was retired into
    // it; never overwrite that evidence -- the two cannot collide (an id
    // that was archived as a superseded predecessor never finalizes later),
    // so an existing dir here is exactly that archive.
    if dir.join("session.json").exists() {
        return Ok(());
    }
    ensure_private_dir(&dir)?;
    let tombstone = FinishedTombstone {
        id,
        workspace: workspace.to_path_buf(),
        tag: tag.to_string(),
        finished_at_ms: now_ms(),
        cause,
    };
    atomic_write_json(&dir.join(TOMBSTONE_FILE), &tombstone)
        .with_context(|| format!("write finished-tombstone for session {id}"))
}

/// The not-found answer for `a kill`: the tombstone matching the target, if
/// any. Selector forms mirror `resolve_record`'s own matching -- `ws:tag`
/// pairs, UUID/prefix, explicit `--tag` (± `--workspace`), and a plain word
/// as a tag in the current workspace -- so a tombstone is found exactly
/// when the record would have been found had it still existed.
pub fn lookup_finished_tombstone(
    paths: &Paths,
    selector: Option<&str>,
    workspace: Option<&Path>,
    tag: Option<&str>,
    cwd_workspace: Option<&Path>,
) -> Option<FinishedTombstone> {
    let mut query: Vec<(Option<PathBuf>, Option<String>, Option<String>)> = Vec::new();
    if let Some(tag) = tag {
        // resolve_record canonicalizes the query workspace (hard-failing if
        // that is impossible); the raw text stays the fallback so a stored
        // non-canonical workspace still matches, as in the pair branch.
        let ws = match workspace {
            Some(ws) => Some(match canonical_workspace(ws) {
                Ok(canonical) => canonical,
                Err(_) => ws.to_path_buf(),
            }),
            None => cwd_workspace.map(Path::to_path_buf),
        };
        query.push((ws, Some(tag.to_string()), None));
    }
    if let Some(selector) = selector {
        if let Some((ws_text, pair_tag)) = selector.rsplit_once(':') {
            let ws = Some(match canonical_workspace(Path::new(ws_text)) {
                Ok(canonical) => canonical,
                Err(_) => PathBuf::from(ws_text),
            });
            query.push((ws, Some(pair_tag.to_string()), None));
        } else if looks_like_uuid_prefix(selector) {
            query.push((None, None, Some(selector.to_ascii_lowercase())));
        } else if let Some(ws) = cwd_workspace {
            query.push((Some(ws.to_path_buf()), Some(selector.to_string()), None));
        }
    }
    if query.is_empty() {
        return None;
    }
    let mut matches: Vec<FinishedTombstone> = read_tombstones(paths)
        .into_iter()
        .filter(|tombstone| {
            query
                .iter()
                .any(|(ws, tag, id_prefix)| match (id_prefix, tag) {
                    // UUID/prefix form: identity alone, exactly like
                    // resolve_record's selector branch.
                    (Some(needle), _) => tombstone.id.to_string().starts_with(needle),
                    // Every tag form carries a workspace by construction; both
                    // halves must match, or any same-named tag anywhere would.
                    (None, Some(tag)) => {
                        tombstone.tag == *tag
                            && ws.as_ref().is_some_and(|ws| {
                                tombstone.workspace == *ws
                                    || canonical_workspace(&tombstone.workspace)
                                        .map(|canonical| canonical == *ws)
                                        .unwrap_or(false)
                            })
                    }
                    (None, None) => false,
                })
        })
        .collect();
    matches.sort_by_key(|tombstone| std::cmp::Reverse(tombstone.finished_at_ms));
    matches.into_iter().next()
}

/// `resolve_record`'s UUID-prefix rule: only strings of 8..=32 hex digits
/// (dashes ignored) are id candidates, so an ordinary word always stays a
/// tag.
fn looks_like_uuid_prefix(selector: &str) -> bool {
    let mut hex_digits = 0;
    for byte in selector.bytes() {
        if byte == b'-' {
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
        hex_digits += 1;
    }
    (8..=32).contains(&hex_digits)
}

fn read_tombstones(paths: &Paths) -> Vec<FinishedTombstone> {
    let entries = match std::fs::read_dir(paths.retired_sessions_dir()) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join(TOMBSTONE_FILE))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&text).ok()
        })
        .collect()
}

/// Removes tombstones older than [`TOMBSTONE_TTL`]; returns their ids. Only
/// tombstone-only dirs are touched -- a supersede archive holds a full
/// record and has its own lifecycle.
pub fn prune_expired_tombstones(paths: &Paths) -> Result<Vec<Uuid>> {
    let now = now_ms();
    let mut expired = Vec::new();
    let entries = match std::fs::read_dir(paths.retired_sessions_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(expired),
        Err(error) => return Err(error).context("read retired-sessions dir"),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let dir = entry.path();
        let file = dir.join(TOMBSTONE_FILE);
        let tombstone: FinishedTombstone = match std::fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(tombstone) => tombstone,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        if now.saturating_sub(tombstone.finished_at_ms) < TOMBSTONE_TTL.as_millis() as u64 {
            continue;
        }
        if std::fs::remove_dir_all(&dir).is_ok() {
            expired.push(tombstone.id);
        }
    }
    Ok(expired)
}

/// The JSON answer `a kill --json` prints for a tombstoned target.
pub fn tombstone_kill_json(tombstone: &FinishedTombstone) -> serde_json::Value {
    json!({
        "found": false,
        "finished": true,
        "id": tombstone.id,
        "workspace": tombstone.workspace,
        "tag": tombstone.tag,
        "finished_at_ms": tombstone.finished_at_ms,
        "cause": tombstone.cause,
    })
}
