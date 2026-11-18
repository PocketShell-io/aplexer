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

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::paths::{canonical_workspace, ensure_private_dir, Paths};
use crate::persist::{atomic_write_json, FileLock};
use crate::record::{observed_state, ExitInfo};
use crate::registry::read_record;
use crate::util::now_ms;
use crate::{list_records, read_session_record, Phase, SessionRecord};

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
///    died without recording an exit -- unless the phase is `exiting` and
///    the workload leader is gone too: `Exiting` is written only when a
///    stop/kill was accepted or the workload already ended, so a worker
///    that vanished in that window (its scope/cgroup torn down together
///    with the workload by `a kill`, a Hub stop, systemd) is a session
///    that ended, not one that crashed ([`finalize_exiting_corpse`] gives
///    it the exit record the worker never got to write);
/// 3. `Phase::Failed` -- the worker lived long enough to record a fatal
///    error (`record.error` carries the reason).
///
/// A plain `Exited` record -- including a signal-terminated workload from
/// `a kill` -- warns nothing: that is a session ending, not a session
/// crashing.
///
/// A `broken` record whose worker cgroup sat beneath a per-user systemd
/// manager that restarted *after* the session started gets its detail
/// attributed to that teardown (see [`attribute_systemd_teardown`]); the
/// same goes for a kernel OOM kill the systemd journal recorded in the
/// session's own unit, or manager-wide OOM pressure near detection (see
/// [`OomEvidence`]). The worker did not crash on its own in any of these,
/// and the next triage must not need journal forensics to say so.
/// The kind stays `Crash` -- sessions really died with unsaved work -- only
/// the misattribution ("died unexpectedly ... kill source unknown") is
/// fixed.
pub fn warning_for_record(record: &SessionRecord) -> Option<(WarningKind, String)> {
    warning_for_record_with(
        record,
        now_ms(),
        manager_boot_for(record),
        unit_teardown_for(record),
        Some(&oom_evidence_for(record, now_ms())),
    )
}

/// OOM-killer evidence for a dead-worker crash, resolved best-effort from
/// the systemd journal and injected into the predicate like the manager
/// boot, so tests pin it without touching the journal. Two tiers, most
/// specific first:
///
/// * [`OomEvidence::unit_kill_ms`] -- the journal's "killed by the OOM
///   killer" line for the session's *own* unit: definitive. The kernel
///   killed a process in this unit and the worker is gone with no exit
///   recorded; nobody was left to write `exit.oom_killed`.
/// * [`OomEvidence::manager_pressure`] -- OOM kills under the same
///   per-user manager within [`OOM_PRESSURE_LOOKBACK_MS`] before
///   detection: a storm that crosses unit boundaries, so the likeliest
///   kill source even when the worker's own unit never got a line
///   (journald drops and rate-limits under the very pressure it records).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OomEvidence {
    /// Epoch-ms of the newest kernel OOM kill inside the session's own
    /// systemd unit, or `None` when the journal has none or could not be
    /// read. Only meaningful after the session started.
    pub unit_kill_ms: Option<u64>,
    /// Manager-wide OOM kills near detection, or `None` when the record's
    /// cgroup names no per-user manager the query could read.
    pub manager_pressure: Option<ManagerOomPressure>,
}

/// [`OomEvidence::manager_pressure`]: how many processes the kernel OOM
/// killer killed under the same per-user manager, in which units, most
/// recently when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerOomPressure {
    /// Number of journal OOM-kill lines in the lookback window.
    pub kills: u32,
    /// Distinct victim units named by the journal, capped for the detail.
    pub units: Vec<String>,
    /// Epoch-ms of the newest kill in the window.
    pub last_kill_ms: u64,
}

/// How far back from detection the manager-pressure window reaches. The
/// detection sweep runs from every query command plus a 10-minute hygiene
/// timer, so first detection lags the death by minutes normally; 24h is a
/// generous ceiling that keeps the journal query bounded (2s deadline)
/// while still covering a session killed long before anyone looked.
/// Pressure older than that is too stale to call the likely kill source.
const OOM_PRESSURE_LOOKBACK_MS: u64 = 24 * 3_600_000;

/// Distinct victim units kept in [`ManagerOomPressure`]; more are counted
/// in `kills` but not named.
const MANAGER_PRESSURE_UNIT_CAP: usize = 6;

/// The live [`OomEvidence`] for a record, best-effort and cached per
/// process: the journal is queried once per dead session per process. A
/// stale cache entry can only miss a *later* attribution (a pressure storm
/// that started after the first sweep), never invent one -- the sweep runs
/// in fresh CLI processes and the 10-minute hygiene timer, so a missed
/// window is re-checked minutes later. Off Linux there is no systemd
/// journal: no evidence, the generic detail stands.
#[cfg(target_os = "linux")]
fn oom_evidence_for(record: &SessionRecord, now: u64) -> OomEvidence {
    static EVIDENCE: OnceLock<Mutex<HashMap<Uuid, OomEvidence>>> = OnceLock::new();
    let cache = EVIDENCE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(evidence) = cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(&record.id).cloned())
    {
        return evidence;
    }
    let evidence = resolve_oom_evidence(record, now);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(record.id, evidence.clone());
    }
    evidence
}

#[cfg(not(target_os = "linux"))]
fn oom_evidence_for(_record: &SessionRecord, _now: u64) -> OomEvidence {
    OomEvidence::default()
}

/// Query the systemd journal for the record's own unit and its per-user
/// manager, bounded by the session's birth (no kill can precede it) and
/// the detection clock. Only records whose cgroup names a unit/manager
/// produce evidence; anything else stays generic.
#[cfg(target_os = "linux")]
fn resolve_oom_evidence(record: &SessionRecord, now: u64) -> OomEvidence {
    let mut evidence = OomEvidence::default();
    let Some(cgroup) = record.worker_cgroup.as_deref() else {
        return evidence;
    };
    if let Some(scope) = crate::placement::systemd_unit_from_cgroup(cgroup) {
        evidence.unit_kill_ms = crate::systemd_unit_oom_kill_ms(&scope, record.created_at_ms);
    }
    if crate::placement::classify_cgroup_path(cgroup)
        == crate::placement::CgroupPlacement::UserManager
    {
        if let Some(uid) = crate::placement::user_manager_uid_from_cgroup(cgroup) {
            let kills = crate::user_manager_oom_kills(
                uid,
                now.saturating_sub(OOM_PRESSURE_LOOKBACK_MS),
                now,
            );
            if !kills.is_empty() {
                let mut units: Vec<String> = Vec::new();
                for kill in &kills {
                    if let Some(unit) = &kill.unit {
                        if !units.contains(unit) {
                            units.push(unit.clone());
                            if units.len() >= MANAGER_PRESSURE_UNIT_CAP {
                                break;
                            }
                        }
                    }
                }
                evidence.manager_pressure = Some(ManagerOomPressure {
                    kills: kills.len() as u32,
                    units,
                    last_kill_ms: kills.last().map(|kill| kill.at_ms).unwrap_or(0),
                });
            }
        }
    }
    evidence
}

/// The shared warning predicate with injected clocks: `now` is the wall
/// clock the liveness window is read against, and `manager_started_ms` is
/// the owning per-user manager's boot when the record's cgroup names one
/// (see [`crate::user_manager_started_ms`]). Production passes the live
/// values; tests pin both without touching systemd or `/proc`.
pub fn warning_for_record_at(
    record: &SessionRecord,
    now: u64,
    manager_started_ms: Option<u64>,
) -> Option<(WarningKind, String)> {
    warning_for_record_with(record, now, manager_started_ms, None, None)
}

/// Like [`warning_for_record_at`], also accepting the systemd journal's
/// OOM-killer evidence (`oom_evidence`) for kill-source attribution; tests
/// construct it directly, production resolves it via [`oom_evidence_for`].
pub fn warning_for_record_with(
    record: &SessionRecord,
    now: u64,
    manager_started_ms: Option<u64>,
    unit_teardown_ms: Option<u64>,
    oom_evidence: Option<&OomEvidence>,
) -> Option<(WarningKind, String)> {
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
    if is_finished_exiting_corpse(record) {
        return None;
    }
    if observed_state(
        &record.phase,
        record.worker_alive(),
        record.created_at_ms,
        now,
    ) == "broken"
    {
        let base = match record.error.as_deref() {
            Some(error) => format!(
                "worker died unexpectedly while {} (no exit recorded): {error}",
                record.phase.name()
            ),
            None => format!(
                "worker died unexpectedly while {} (no exit recorded; termination signal and kill source are unknown)",
                record.phase.name()
            ),
        };
        return Some((
            WarningKind::Crash,
            attribute_systemd_teardown(
                record,
                base,
                manager_started_ms,
                unit_teardown_ms,
                oom_evidence,
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

/// The manager boot for a record, best-effort and cached per process: the
/// uid parsed from the recorded worker cgroup, resolved once through
/// [`crate::user_manager_started_ms`]. A stale cache entry can only *miss*
/// an attribution (fall back to the generic detail), never invent one -- a
/// cached boot always predates a manager restart that happened after the
/// lookup -- so no invalidation is needed. Off Linux there are no per-user
/// systemd managers, and the answer is always `None`.
#[cfg(target_os = "linux")]
fn manager_boot_for(record: &SessionRecord) -> Option<u64> {
    static BOOT: OnceLock<Mutex<HashMap<u32, Option<u64>>>> = OnceLock::new();
    let uid = record
        .worker_cgroup
        .as_deref()
        .and_then(crate::placement::user_manager_uid_from_cgroup)?;
    let cache = BOOT.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(boot) = cache.lock().ok().and_then(|cache| cache.get(&uid).copied()) {
        return boot;
    }
    let boot = crate::user_manager_started_ms(uid);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(uid, boot);
    }
    boot
}

/// Off Linux there are no per-user systemd managers: no boot, no
/// attribution, the generic detail stands.
#[cfg(not(target_os = "linux"))]
fn manager_boot_for(_record: &SessionRecord) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn unit_teardown_for(record: &SessionRecord) -> Option<u64> {
    static UNIT_TEARDOWN: OnceLock<
        Mutex<HashMap<crate::placement::SystemdUnitScope, Option<u64>>>,
    > = OnceLock::new();
    let scope = record
        .worker_cgroup
        .as_deref()
        .and_then(crate::placement::systemd_unit_from_cgroup)?;
    let cache = UNIT_TEARDOWN.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(ts) = cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(&scope).copied())
    {
        return ts;
    }
    let ts = crate::systemd_unit_teardown_ms(&scope);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(scope, ts);
    }
    ts
}

#[cfg(not(target_os = "linux"))]
fn unit_teardown_for(_record: &SessionRecord) -> Option<u64> {
    None
}

/// Clock-skew margin for the teardown attribution: the manager boot must
/// postdate the session start by more than this. Both clocks are wall time
/// from the same machine (session start from `now_ms`, manager boot from
/// kernel `btime` plus a monotonic offset), so seconds of disagreement are
/// impossible; the margin only guards second-resolution truncation, never a
/// real ordering.
const MANAGER_BOOT_SKEW_MS: u64 = 5_000;

/// Attribute a dead-worker crash to a per-user systemd manager teardown,
/// the session's own systemd unit being OOM-killed, its owning unit
/// stopping/restarting, or manager-wide OOM pressure -- whichever the
/// record plus journal evidence can prove, most specific first.
fn attribute_systemd_teardown(
    record: &SessionRecord,
    base: String,
    manager_started_ms: Option<u64>,
    unit_teardown_ms: Option<u64>,
    oom_evidence: Option<&OomEvidence>,
) -> String {
    let cgroup = record.worker_cgroup.as_deref();
    if let Some(boot_ms) = manager_started_ms.filter(|boot_ms| *boot_ms != 0) {
        if cgroup.map(crate::placement::classify_cgroup_path)
            == Some(crate::placement::CgroupPlacement::UserManager)
            && boot_ms > record.created_at_ms.saturating_add(MANAGER_BOOT_SKEW_MS)
        {
            if let Some(uid) = cgroup.and_then(crate::placement::user_manager_uid_from_cgroup) {
                let gap_ms = boot_ms.saturating_sub(record.created_at_ms);
                return format!(
                    "{base}; the per-user systemd manager user@{uid}.service started ~{} after this session started, tearing down every session beneath it together",
                    format_duration_gap(gap_ms)
                );
            }
        }
    }
    if let Some(kill_ms) = oom_evidence
        .and_then(|oom| oom.unit_kill_ms)
        .filter(|ts| *ts != 0)
    {
        if kill_ms > record.created_at_ms.saturating_add(MANAGER_BOOT_SKEW_MS) {
            if let Some(unit_scope) = cgroup.and_then(crate::placement::systemd_unit_from_cgroup) {
                let gap_ms = kill_ms.saturating_sub(record.created_at_ms);
                return format!(
                    "{base}; the kernel OOM killer killed a process in this session's systemd unit {} ~{} after the session started (systemd journal)",
                    unit_scope.unit_name(),
                    format_duration_gap(gap_ms)
                );
            }
        }
    }
    if let Some(teardown_ms) = unit_teardown_ms.filter(|ts| *ts != 0) {
        if teardown_ms > record.created_at_ms.saturating_add(MANAGER_BOOT_SKEW_MS) {
            if let Some(unit_scope) = cgroup.and_then(crate::placement::systemd_unit_from_cgroup) {
                let gap_ms = teardown_ms.saturating_sub(record.created_at_ms);
                return format!(
                    "{base}; the systemd unit {} stopped or restarted ~{} after this session started, tearing down processes in its cgroup",
                    unit_scope.unit_name(),
                    format_duration_gap(gap_ms)
                );
            }
        }
    }
    if let Some(pressure) = oom_evidence.and_then(|oom| oom.manager_pressure.as_ref()) {
        if pressure.kills > 0
            && pressure.last_kill_ms != 0
            // The pressure journal is the per-user manager's; a worker
            // outside that tree (login session, system slice) was not
            // threatened by it. Mirrors the resolver's own guard.
            && cgroup.map(crate::placement::classify_cgroup_path)
                == Some(crate::placement::CgroupPlacement::UserManager)
        {
            if let Some(uid) = cgroup.and_then(crate::placement::user_manager_uid_from_cgroup) {
                return format!(
                    "{base}; the kernel OOM killer killed {} process(es) under the same per-user systemd manager user@{uid}.service within the day before this crash was detected ({}), so kernel memory pressure is the likely kill source (systemd journal)",
                    pressure.kills,
                    format_pressure_units(&pressure.units)
                );
            }
        }
    }
    base
}

/// The victim-unit list of a manager-pressure attribution, bounded:
/// `a.scope, b.scope, c.scope, …` when the journal named more units than
/// are rendered (the `kills` count already carries the magnitude), or
/// `unnamed victims` when every kill line named only a slice.
fn format_pressure_units(units: &[String]) -> String {
    if units.is_empty() {
        return "unnamed victims".to_string();
    }
    let named: Vec<&str> = units.iter().take(3).map(String::as_str).collect();
    let mut text = named.join(", ");
    if units.len() > 3 {
        text += ", …";
    }
    text
}

/// Human gap between session start and manager boot for the teardown
/// detail: `40s`, `25m`, `14h`, `3d`. Rounds down.
fn format_duration_gap(ms: u64) -> String {
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

/// The detail to preserve when a record is removed. For an unexpected dead
/// worker this is the same diagnosis shown by `a warnings`; for a recovered
/// exit with no status it records that the signal/source was unavailable.
pub fn diagnostic_error_for_record(record: &SessionRecord) -> Option<String> {
    if let Some((_, detail)) = warning_for_record(record) {
        return Some(detail);
    }
    if record
        .exit
        .as_ref()
        .is_some_and(|exit| exit.code.is_none() && exit.signal.is_none() && !exit.oom_killed)
    {
        return Some(
            "worker exit was observed, but no exit code or termination signal was recorded; the kill source is unknown"
                .to_string(),
        );
    }
    if is_finished_exiting_corpse(record) {
        return Some(
            "worker and workload disappeared after a stop/kill request; no worker exit record or signal was captured"
                .to_string(),
        );
    }
    record.error.clone()
}

/// An `Exiting` record whose worker AND workload leader are both gone and
/// that never got an exit record: a deliberate stop (or a natural workload
/// exit) whose worker was torn down before it could finalize.
fn is_finished_exiting_corpse(record: &SessionRecord) -> bool {
    record.phase == Phase::Exiting
        && record.exit.is_none()
        && !record.worker_alive()
        && !record.workload_leader_alive()
}

/// Write the exit record a worker died before writing, for a record that
/// [`is_finished_exiting_corpse`]: phase `Exited`, an exit with no code or
/// signal known, and `containment_empty` explicitly NOT proven (the worker
/// that would have proven it is gone), so `a prune`/`a kill` still verify
/// containment before reclaiming. Re-reads under the registry lock and
/// refuses on a changed generation, so it never clobbers a live worker's
/// write. Returns whether a record was written.
pub fn finalize_exiting_corpse(paths: &Paths, record: &SessionRecord) -> Result<bool> {
    if !is_finished_exiting_corpse(record) {
        return Ok(false);
    }
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let Ok(mut current) = read_record(&paths.record(record.id)) else {
        return Ok(false);
    };
    if current.created_at_ms != record.created_at_ms || !is_finished_exiting_corpse(&current) {
        return Ok(false);
    }
    current.phase = Phase::Exited;
    if current.containment_empty != Some(true) {
        current.containment_empty = Some(false);
    }
    current.exit = Some(ExitInfo {
        code: None,
        signal: None,
        oom_killed: false,
        exited_at_ms: now_ms(),
    });
    current.updated_at_ms = now_ms();
    atomic_write_json(&paths.record(record.id), &current)?;
    Ok(true)
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
    // A dead-worker verdict judges a record snapshot read BEFORE the
    // liveness probe, and a cleanly finishing worker rewrites (Exited +
    // exit) or removes (fast path + finished tombstone) its record before
    // its process exits. So a snapshot taken a moment before finalization
    // plus a probe taken a moment after it reads `exiting`/`running` with
    // no worker -- the false "died unexpectedly" warning every list sweep
    // and the resident RPC registry's cached records used to raise for
    // normal Hub stops. Re-read the record now, after the worker was seen
    // dead: if it still claims an active phase with no exit, nobody
    // finalized it and the crash is real; if it is gone or terminal, the
    // session ended normally (or was retired) and warns nothing new.
    let (kind, detail) = if kind == WarningKind::Crash && record.worker_phase_active() {
        let Ok(current) = read_session_record(paths, record.id) else {
            return Ok(false);
        };
        match warning_for_record(&current) {
            Some(verdict) => verdict,
            None => return Ok(false),
        }
    } else {
        (kind, detail)
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

/// Materialize a crash warning for a startup-timeout rollback (intake
/// 7cc124d9). The record itself is gone -- rollback deleted it after
/// terminating the worker tree -- so `record_warning`'s re-read guard cannot
/// run; the timeout is the diagnosis, stated explicitly. Write-if-absent
/// like every other warning path.
pub fn record_startup_timeout_warning(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    tag: &str,
    engine: &str,
    detail: &str,
) -> Result<bool> {
    record_startup_warning(paths, id, workspace, tag, engine, detail)
}

/// Materialize a crash warning for a worker failure before readiness. The
/// start process captured the worker's exit status, so this warning can
/// distinguish an external signal from the launcher's own timeout rollback.
pub fn record_startup_failure_warning(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    tag: &str,
    engine: &str,
    detail: &str,
) -> Result<bool> {
    record_startup_warning(paths, id, workspace, tag, engine, detail)
}

fn record_startup_warning(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    tag: &str,
    engine: &str,
    detail: &str,
) -> Result<bool> {
    let path = warning_path(paths, id);
    if path.exists() || acked_path(paths, id).exists() {
        return Ok(false);
    }
    ensure_private_dir(&paths.warnings_dir())?;
    let warning = SessionWarning {
        schema_version: WARNINGS_SCHEMA_VERSION,
        session: id,
        workspace: workspace.to_path_buf(),
        tag: tag.to_string(),
        engine: engine.to_string(),
        kind: WarningKind::Crash,
        detail: detail.to_string(),
        created_at_ms: now_ms(),
    };
    atomic_write_json(&path, &warning)
        .with_context(|| format!("write startup warning for session {id}"))?;
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
    for mut record in list_records(paths).unwrap_or_default() {
        if matches!(finalize_exiting_corpse(paths, &record), Ok(true)) {
            if let Ok(updated) = read_record(&paths.record(record.id)) {
                record = updated;
            }
        }
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
