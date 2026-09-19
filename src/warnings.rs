//! Durable crash warnings: the ack-gated record that a session crashed or
//! died OOM-killed, designed to outlive the session's own record.
//!
//! Two deaths need this. An OOM-killed workload keeps its terminal record
//! (`finalize_session` refuses to remove it), but `a prune` reaps that
//! record like any other finished session and the diagnosis is gone. And a
//! worker that itself died (SIGKILL, reboot, a bug) has nobody left to
//! write anything: its record sits `broken` -- phase still `running`, no
//! exit -- until the next `a prune` reaps it too. Either way the normal
//! cleanup verbs erase the very fact a human needs to see, so the fact
//! lives in its own sidecar file under `state_root/warnings/<id>.json`,
//! written once, shown by every listing surface, and removed only by an
//! explicit `a ack`.
//!
//! Both write paths share one predicate ([`warning_for_record`]): the
//! worker records what it saw at finalization (OOM kill, fatal error), and
//! the CLI-side sweep ([`sweep_warnings`]) catches what no worker can
//! report -- the dead-worker crash -- plus anything an older worker build
//! never wrote. The sweep runs from the query commands (`a list`,
//! `a snapshot`, `a status`, `a warnings`) because a crash is by
//! definition first observed by whoever is not the dead worker. Reads are
//! best-effort and writes write-if-absent (the earliest detection time
//! wins), so concurrent `a` processes cannot corrupt or re-date a warning.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::paths::{canonical_workspace, ensure_private_dir, Paths};
use crate::persist::atomic_write_json;
use crate::record::observed_state;
use crate::util::now_ms;
use crate::{list_records, Phase, SessionRecord};

pub const WARNINGS_SCHEMA_VERSION: u32 = 1;

/// What kind of death the warning records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// The workload was killed by the kernel OOM killer (`ExitInfo::oom_killed`).
    Oom,
    /// The session crashed: the worker died without any recorded exit
    /// (`observed_state` == `broken`), or recorded a fatal error
    /// (`Phase::Failed`).
    Crash,
}

impl WarningKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WarningKind::Oom => "oom",
            WarningKind::Crash => "crash",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWarning {
    pub schema_version: u32,
    pub session: Uuid,
    pub workspace: PathBuf,
    pub tag: String,
    pub engine: String,
    pub kind: WarningKind,
    /// The one-sentence human explanation shown in listings.
    pub detail: String,
    /// When the crash was first recorded, ms since epoch. A write-if-absent
    /// store, so this is the earliest observation, not the last.
    pub created_at_ms: u64,
}

impl SessionWarning {
    /// The `workspace:tag` form every `a` selector surface uses.
    pub fn selector(&self) -> String {
        format!("{}:{}", self.workspace.display(), self.tag)
    }

    /// The consumer-facing object embedded in `a snapshot`/`a status --json`
    /// rows and printed by `a warnings --json`.
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "session": self.session,
            "workspace": self.workspace,
            "tag": self.tag,
            "engine": self.engine,
            "kind": self.kind,
            "detail": self.detail,
            "created_at_ms": self.created_at_ms,
        })
    }
}

/// The single warning predicate, shared by the worker's finalization write
/// and the CLI's query-time sweep, so the two can never disagree about what
/// counts as a crash. Ordered most-specific first:
///
/// 1. `exit.oom_killed` -- the kernel's own diagnosis, recorded by the
///    worker's lifecycle at finalize;
/// 2. `observed_state` == `broken` -- the record still claims an active
///    phase but the worker process is gone past the startup window, so it
///    died without recording an exit;
/// 3. `Phase::Failed` -- the worker lived long enough to record a fatal
///    error (`record.error` carries the reason).
///
/// A plain `Exited` record -- including a signal-terminated workload from
/// `a kill` -- warns nothing: that is a session ending, not a session
/// crashing.
pub fn warning_for_record(record: &SessionRecord) -> Option<(WarningKind, String)> {
    if let Some(exit) = &record.exit {
        if exit.oom_killed {
            return Some((
                WarningKind::Oom,
                format!(
                    "workload was killed by the kernel OOM killer (code {:?}, signal {:?})",
                    exit.code, exit.signal
                ),
            ));
        }
    }
    if observed_state(
        &record.phase,
        record.worker_alive(),
        record.created_at_ms,
        now_ms(),
    ) == "broken"
    {
        return Some((
            WarningKind::Crash,
            format!(
                "worker died unexpectedly while {} (no exit recorded)",
                record.phase.name()
            ),
        ));
    }
    if record.phase == Phase::Failed {
        return Some((
            WarningKind::Crash,
            match &record.error {
                Some(error) => format!("worker failed: {error}"),
                None => "worker failed".to_string(),
            },
        ));
    }
    None
}

fn warning_path(paths: &Paths, id: Uuid) -> PathBuf {
    paths.warnings_dir().join(format!("{id}.json"))
}

/// Where an acknowledged warning rests. Not deleted: the tombstone is what
/// keeps [`sweep_warnings`] from re-creating the same crash's warning on
/// the next query while the broken record itself can linger in the
/// registry until `a prune`. An ack must stick.
fn acked_path(paths: &Paths, id: Uuid) -> PathBuf {
    paths
        .warnings_dir()
        .join("acked")
        .join(format!("{id}.json"))
}

/// The unacknowledged warnings on disk, newest crash first. Unreadable or
/// unparseable files are skipped, never surfaced as errors: a listing must
/// not fail over a torn sidecar.
pub fn load_warnings(paths: &Paths) -> Vec<SessionWarning> {
    let entries = match std::fs::read_dir(paths.warnings_dir()) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut warnings: Vec<SessionWarning> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&text).ok()
        })
        .collect();
    warnings.sort_by_key(|warning| std::cmp::Reverse(warning.created_at_ms));
    warnings
}

/// The warning recorded for one session, if any.
pub fn load_warning_for(paths: &Paths, id: Uuid) -> Option<SessionWarning> {
    let text = std::fs::read_to_string(warning_path(paths, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Materialize the warning for `record` when its state warrants one,
/// leaving any existing file (and its original `created_at_ms`) untouched.
/// A session the user already acknowledged ([`acknowledge_warnings`]) is
/// never re-warned for the same crash: the ack must stick even though the
/// broken record can linger until `a prune` -- that is what the `acked/`
/// tombstone encodes. Returns whether a new warning was created.
pub fn record_warning(paths: &Paths, record: &SessionRecord) -> Result<bool> {
    let Some((kind, detail)) = warning_for_record(record) else {
        return Ok(false);
    };
    let path = warning_path(paths, record.id);
    if path.exists() || acked_path(paths, record.id).exists() {
        return Ok(false);
    }
    ensure_private_dir(&paths.warnings_dir())?;
    let warning = SessionWarning {
        schema_version: WARNINGS_SCHEMA_VERSION,
        session: record.id,
        workspace: record.workspace.clone(),
        tag: record.tag.clone(),
        engine: record.engine.clone(),
        kind,
        detail,
        created_at_ms: now_ms(),
    };
    atomic_write_json(&path, &warning)
        .with_context(|| format!("write crash warning for session {}", record.id))?;
    Ok(true)
}

/// `record_warning` for call sites that must not fail the command they run
/// inside (`a list`'s sweep, the worker's finalization): a warning that
/// cannot be written is reported on stderr and the command continues.
pub fn record_warning_best_effort(paths: &Paths, record: &SessionRecord) -> bool {
    match record_warning(paths, record) {
        Ok(created) => created,
        Err(error) => {
            eprintln!(
                "a: could not record crash warning for session {}: {error:#}",
                record.id
            );
            false
        }
    }
}

/// The query-time detection sweep: every registry record that currently
/// warrants a warning gets one if it does not have it yet. This is what
/// catches the dead-worker crash, which no worker can report itself. Runs
/// best-effort over each record (a registry mid-write must not fail the
/// listing that triggered the sweep) and returns how many new warnings it
/// created. Callers that are about to reap corpses must run this FIRST --
/// the swept record is the evidence.
pub fn sweep_warnings(paths: &Paths) -> usize {
    let mut created = 0;
    for record in list_records(paths).unwrap_or_default() {
        if record_warning_best_effort(paths, &record) {
            created += 1;
        }
    }
    created
}

/// Acknowledge warnings by session id: move the sidecar files under
/// `acked/`, so every listing stops showing them and the sweep stops
/// re-materializing them. Returns the ids actually acknowledged (an
/// already-acknowledged id is not an error -- the goal state is "no
/// warning showing").
pub fn acknowledge_warnings(paths: &Paths, ids: &[Uuid]) -> Result<Vec<Uuid>> {
    ensure_private_dir(&paths.warnings_dir().join("acked"))?;
    let mut acknowledged = Vec::new();
    for id in ids {
        match std::fs::rename(warning_path(paths, *id), acked_path(paths, *id)) {
            Ok(()) => acknowledged.push(*id),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("acknowledge crash warning"),
        }
    }
    Ok(acknowledged)
}

/// `a ack`'s target matching, over the warning files themselves -- the
/// session's record is usually long gone by the time anyone acknowledges.
/// Selector forms mirror `resolve_record`'s own matching, so a warning is
/// addressed exactly the way its session would have been: `ws:tag` pairs,
/// UUID/prefix, explicit `--tag` (± `--workspace`), and a plain word as a
/// tag in the current workspace.
pub fn select_warnings(
    paths: &Paths,
    selector: Option<&str>,
    workspace: Option<&Path>,
    tag: Option<&str>,
    cwd_workspace: Option<&Path>,
) -> Vec<SessionWarning> {
    let mut query: Vec<(Option<PathBuf>, Option<String>, Option<String>)> = Vec::new();
    if let Some(tag) = tag {
        let ws = match workspace {
            Some(ws) => Some(canonical_workspace(ws).unwrap_or_else(|_| ws.to_path_buf())),
            None => cwd_workspace.map(Path::to_path_buf),
        };
        query.push((ws, Some(tag.to_string()), None));
    }
    if let Some(selector) = selector {
        if let Some((ws_text, pair_tag)) = selector.rsplit_once(':') {
            let ws = Some(
                canonical_workspace(Path::new(ws_text)).unwrap_or_else(|_| PathBuf::from(ws_text)),
            );
            query.push((ws, Some(pair_tag.to_string()), None));
        } else if looks_like_uuid_prefix(selector) {
            query.push((None, None, Some(selector.to_ascii_lowercase())));
        } else if let Some(ws) = cwd_workspace {
            query.push((Some(ws.to_path_buf()), Some(selector.to_string()), None));
        }
    }
    if query.is_empty() {
        return load_warnings(paths);
    }
    load_warnings(paths)
        .into_iter()
        .filter(|warning| {
            query
                .iter()
                .any(|(ws, tag, id_prefix)| match (id_prefix, tag) {
                    (Some(needle), _) => warning.session.to_string().starts_with(needle),
                    (None, Some(tag)) => {
                        warning.tag == *tag
                            && ws.as_ref().is_some_and(|ws| {
                                warning.workspace == *ws
                                    || canonical_workspace(&warning.workspace)
                                        .map(|canonical| canonical == *ws)
                                        .unwrap_or(false)
                            })
                    }
                    (None, None) => false,
                })
        })
        .collect()
}

/// `resolve_record`'s UUID-prefix rule (see `retired.rs`): only strings of
/// 8..=32 hex digits, dashes ignored, are id candidates.
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
