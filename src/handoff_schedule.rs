//! The opt-in handoff schedule: a small, removable state file that turns
//! `a task run` into a daily scheduled launch (`a task handoff`).
//!
//! Deliberately NOT a scheduler: nothing here runs periodically, wakes up, or
//! installs a timer. The recurring trigger stays whatever the user already
//! runs (cron, a systemd timer, a tmux ping); `fire` is the one command such
//! a timer invokes, and it decides — timezone-aware, at most once per daily
//! slot — whether a launch is due. The entire owned state is one directory,
//! `<state>/task-handoff/`: `schedule.json` (written by `enable`, removed by
//! `disable`) and `fired/<slot>.json` claim markers. Disable removes exactly
//! that directory, is a no-op when it is already gone, never signals a
//! process, and leaves no cutoff behind: an engine cutoff only ever exists
//! inside the owned schedule (passed through to the launch) or as an explicit
//! `a task run` flag — never in engine/profile config.
//!
//! This schedules *launches*. It is not a context handoff: a different engine
//! starts fresh, and carrying context across engines is the prompt file's and
//! `a handoff`'s job.

use crate::persist::atomic_write_json;
use crate::task::rfc3339_utc;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

/// Bumped when the schedule file shape changes.
pub const HANDOFF_SCHEDULE_SCHEMA_VERSION: u32 = 1;

/// Everything the plugin owns, directly under the aplexer state root.
/// `disable` removes this directory and nothing else.
pub fn handoff_dir(state_root: &std::path::Path) -> PathBuf {
    state_root.join("task-handoff")
}

pub fn schedule_path(state_root: &std::path::Path) -> PathBuf {
    handoff_dir(state_root).join("schedule.json")
}

fn fired_dir(state_root: &std::path::Path) -> PathBuf {
    handoff_dir(state_root).join("fired")
}

/// The saved `a task run` argument set a fire launches (the evidence
/// directory stays per-launch on purpose: every fire writes its own).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffTaskSpec {
    pub prompt_file: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engine_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default)]
    pub no_skip_permissions: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_workspace: Option<PathBuf>,
    #[serde(default)]
    pub no_notify: bool,
    /// Passed through to the launch, where `a task run`'s cutoff routing
    /// applies it: before the instant the schedule's engine runs, at/after it
    /// the cutoff engine. Launch-time routing of new jobs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_engine: Option<String>,
}

/// One owned schedule file. Absent = disabled; there is no persisted
/// "enabled: false" state, so disable is a plain removal and repeat disable
/// is trivially idempotent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffSchedule {
    pub schema_version: u32,
    pub created_at: String,
    /// Daily local time `HH:MM` (machine timezone, DST-aware) after which at
    /// most one launch fires per local day. Absent: every `fire` invocation
    /// launches and the caller's timer alone decides when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub task: HandoffTaskSpec,
}

pub fn load_schedule(state_root: &std::path::Path) -> Result<Option<HandoffSchedule>> {
    let path = schedule_path(state_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let schedule: HandoffSchedule = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse schedule {}", path.display()))?;
    // A schedule written by a newer aplexer must never be launched with
    // this build's (older) semantics: refuse loudly instead.
    if schedule.schema_version > HANDOFF_SCHEDULE_SCHEMA_VERSION {
        bail!(
            "schedule {} has schema version {}, but this build understands at most {HANDOFF_SCHEDULE_SCHEMA_VERSION}; \
             upgrade aplexer or re-run `a task handoff enable`",
            path.display(),
            schedule.schema_version
        );
    }
    Ok(Some(schedule))
}

pub fn save_schedule(state_root: &std::path::Path, schedule: &HandoffSchedule) -> Result<PathBuf> {
    let dir = handoff_dir(state_root);
    crate::ensure_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = schedule_path(state_root);
    atomic_write_json(&path, schedule)?;
    Ok(path)
}

/// Claim the launch slot for `slot` by creating its marker with
/// `create_new` — the filesystem makes the race a win/lose, no lock
/// machinery. `Ok(false)` means another fire already claimed (or an older
/// marker exists): the caller launches nothing.
pub fn claim_slot(state_root: &std::path::Path, slot: &str, now_ms: u64) -> Result<bool> {
    let dir = fired_dir(state_root);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{slot}.json"));
    let body = json!({
        "slot": slot,
        "claimed_at": rfc3339_utc(now_ms),
    });
    match File::options().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(body.to_string().as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("claim {}", path.display())),
    }
}

/// The fired slot markers, sorted — `status`'s history view.
pub fn fired_slots(state_root: &std::path::Path) -> Result<Vec<String>> {
    let dir = fired_dir(state_root);
    let mut slots = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(slots),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(slot) = name.strip_suffix(".json") {
            slots.push(slot.to_string());
        }
    }
    slots.sort();
    Ok(slots)
}

/// Whether a slot's claim marker exists (the cheap pre-check; `claim_slot`
/// stays the authority).
pub fn slot_fired(state_root: &std::path::Path, slot: &str) -> bool {
    fired_dir(state_root).join(format!("{slot}.json")).exists()
}

/// Remove the owned directory. `Ok(false)` = already gone (the idempotent
/// repeat-disable answer). Only this directory is ever a target.
pub fn remove_handoff_state(state_root: &std::path::Path) -> Result<bool> {
    match fs::remove_dir_all(handoff_dir(state_root)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("remove {}", handoff_dir(state_root).display()))
        }
    }
}

// -- Wall-clock helpers: local time of day via libc's TZ rules, no new
//    dependency. localtime/mktime know the machine timezone including DST,
//    which is exactly what "03:00 Berlin" means. --

/// A local wall-clock reading (timezone-resolved by libc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalWall {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

impl LocalWall {
    /// The daily slot key: the local date the launch belongs to.
    pub fn date_key(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Local wall-clock time for epoch seconds.
#[cfg(windows)]
pub fn local_wall(epoch_secs: i64) -> Result<LocalWall> {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let Some(local) = Local.timestamp_opt(epoch_secs, 0).single() else {
        bail!("local time conversion failed for epoch {epoch_secs}");
    };
    Ok(LocalWall {
        year: i64::from(local.year()),
        month: local.month(),
        day: local.day(),
        hour: local.hour(),
        minute: local.minute(),
    })
}

/// Local wall-clock time for epoch seconds.
#[cfg(unix)]
pub fn local_wall(epoch_secs: i64) -> Result<LocalWall> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs: libc::time_t = epoch_secs;
    let resolved = unsafe { libc::localtime_r(&secs, &mut tm) };
    if resolved.is_null() {
        bail!("localtime_r failed for epoch {epoch_secs}");
    }
    Ok(LocalWall {
        year: tm.tm_year as i64 + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    })
}

/// Epoch seconds for a local wall-clock time (`tm_isdst = -1`: libc resolves
/// DST). Round-trips with [`local_wall`].
#[cfg(windows)]
pub fn local_epoch(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Result<i64> {
    use chrono::{Duration, Local, NaiveDate, TimeZone};
    let bad = || anyhow!("invalid local time {year:04}-{month:02}-{day:02} {hour:02}:{minute:02}");
    let naive = i32::try_from(year)
        .ok()
        .and_then(|year| NaiveDate::from_ymd_opt(year, month, day))
        .and_then(|date| date.and_hms_opt(hour, minute, 0))
        .ok_or_else(bad)?;
    // Ambiguous (DST fall-back) takes the earlier instant; a nonexistent
    // (spring-forward) time resolves an hour later, like mktime's
    // normalisation, by probing one hour ahead and stepping back.
    if let Some(local) = Local.from_local_datetime(&naive).earliest() {
        return Ok(local.timestamp());
    }
    Local
        .from_local_datetime(&(naive + Duration::hours(1)))
        .earliest()
        .map(|local| local.timestamp())
        .ok_or_else(bad)
}

/// Epoch seconds for a local wall-clock time (`tm_isdst = -1`: libc resolves
/// DST). Round-trips with [`local_wall`].
#[cfg(unix)]
pub fn local_epoch(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Result<i64> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (year - 1900) as i32;
    tm.tm_mon = month as i32 - 1;
    tm.tm_mday = day as i32;
    tm.tm_hour = hour as i32;
    tm.tm_min = minute as i32;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let epoch = unsafe { libc::mktime(&mut tm) };
    if epoch < 0 {
        bail!("mktime failed for {year:04}-{month:02}-{day:02} {hour:02}:{minute:02} local");
    }
    Ok(epoch)
}

/// Parse the schedule's daily time: strict `HH:MM`, two digits each.
pub fn parse_hhmm(raw: &str) -> Result<(u32, u32)> {
    let bad =
        || anyhow!("--at must be HH:MM local time with two digits each (e.g. 03:00), got {raw:?}");
    let Some((hh, mm)) = raw.split_once(':') else {
        return Err(bad());
    };
    if hh.len() != 2 || mm.len() != 2 {
        return Err(bad());
    }
    // Digits only: `u32::from_str` would happily take a leading `+`.
    if !hh.bytes().all(|b| b.is_ascii_digit()) || !mm.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let hh: u32 = hh.parse().map_err(|_| bad())?;
    let mm: u32 = mm.parse().map_err(|_| bad())?;
    if hh > 23 || mm > 59 {
        return Err(bad());
    }
    Ok((hh, mm))
}

/// Whether `fire` may launch now, given the schedule's daily time (if any),
/// the current wall clock, and the claim markers. Pure so tests can drive it
/// with a fully fake clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireDecision {
    /// Before today's `HH:MM`; `next_due_secs` is today's due instant.
    NotDue { next_due_secs: i64 },
    /// This slot's claim marker already exists (or was lost to a racing
    /// fire): at most one launch per slot.
    AlreadyFired { slot: String },
    /// Launch; `slot` names the claim marker to create first.
    Due { slot: String },
}

pub fn evaluate(
    wall: LocalWall,
    at: Option<(u32, u32)>,
    now_ms: u64,
    slot_fired: impl Fn(&str) -> bool,
) -> Result<FireDecision> {
    let Some((hh, mm)) = at else {
        // No daily window: every invocation launches (the caller's timer
        // owns timing). The slot key still deduplicates same-minute
        // double-invocations of an overlapping timer.
        let slot = format!("{}T{:02}{:02}", wall.date_key(), wall.hour, wall.minute);
        return Ok(if slot_fired(&slot) {
            FireDecision::AlreadyFired { slot }
        } else {
            FireDecision::Due { slot }
        });
    };
    let due = local_epoch(wall.year, wall.month, wall.day, hh, mm)?;
    let now_secs = now_ms / 1000;
    if i64::try_from(now_secs)? < due {
        return Ok(FireDecision::NotDue { next_due_secs: due });
    }
    let slot = wall.date_key();
    Ok(if slot_fired(&slot) {
        FireDecision::AlreadyFired { slot }
    } else {
        FireDecision::Due { slot }
    })
}

/// The next due instant for `status`'s display: today's `HH:MM` if still
/// ahead, else tomorrow's (libc resolves the DST shift). Without a daily
/// time, every invocation is due: report now.
pub fn next_due(at: Option<(u32, u32)>, now_ms: u64) -> Result<i64> {
    let Some((hh, mm)) = at else {
        return i64::try_from(now_ms / 1000).map_err(Into::into);
    };
    let wall = local_wall(i64::try_from(now_ms / 1000)?)?;
    let due_today = local_epoch(wall.year, wall.month, wall.day, hh, mm)?;
    if i64::try_from(now_ms / 1000)? < due_today {
        return Ok(due_today);
    }
    let tomorrow = local_wall(due_today + 86_400)?;
    local_epoch(tomorrow.year, tomorrow.month, tomorrow.day, hh, mm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hhmm_is_strict() {
        assert_eq!(parse_hhmm("03:00").unwrap(), (3, 0));
        assert_eq!(parse_hhmm("23:59").unwrap(), (23, 59));
        for bad in [
            "3:00", "0300", "24:00", "03:60", "03", "03:0", "", "aa:bb", "03:00:00",
            // A leading `+` parses as a number but is not a clock reading.
            "+3:00", "03:+0",
        ] {
            assert!(parse_hhmm(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn schedule_from_a_newer_schema_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = handoff_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        let future = format!(
            r#"{{"schema_version": {}, "created_at": "2026-10-04T00:00:00.000Z", "task": {{"prompt_file": "/tmp/p"}}}}"#,
            HANDOFF_SCHEDULE_SCHEMA_VERSION + 1
        );
        std::fs::write(schedule_path(tmp.path()), future).unwrap();
        let error = load_schedule(tmp.path()).unwrap_err();
        assert!(
            error.to_string().contains("schema version"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn local_wall_and_epoch_round_trip() {
        // Any timezone: mktime → localtime must return the same wall time.
        let epoch = local_epoch(2026, 10, 4, 3, 0).unwrap();
        let wall = local_wall(epoch).unwrap();
        assert_eq!(wall.year, 2026);
        assert_eq!(wall.month, 10);
        assert_eq!(wall.day, 4);
        assert_eq!(wall.hour, 3);
        assert_eq!(wall.minute, 0);
        assert_eq!(wall.date_key(), "2026-10-04");
    }

    fn wall_at(day: u32, hour: u32, minute: u32) -> LocalWall {
        LocalWall {
            year: 2026,
            month: 10,
            day,
            hour,
            minute,
        }
    }

    #[test]
    fn evaluate_not_due_before_the_daily_time() {
        let now = local_epoch(2026, 10, 4, 2, 59).unwrap() as u64 * 1000;
        assert_eq!(
            evaluate(wall_at(4, 2, 59), Some((3, 0)), now, |_| false).unwrap(),
            FireDecision::NotDue {
                next_due_secs: local_epoch(2026, 10, 4, 3, 0).unwrap()
            }
        );
    }

    #[test]
    fn evaluate_due_after_and_at_the_daily_time_once_per_slot() {
        let now = local_epoch(2026, 10, 4, 3, 0).unwrap() as u64 * 1000;
        // At the instant, unclaimed: due, slot = local date.
        assert_eq!(
            evaluate(wall_at(4, 3, 0), Some((3, 0)), now, |_| false).unwrap(),
            FireDecision::Due {
                slot: "2026-10-04".into()
            }
        );
        // Claimed: never twice in one slot.
        assert_eq!(
            evaluate(wall_at(4, 9, 0), Some((3, 0)), now + 6 * 3_600_000, |s| s
                == "2026-10-04")
            .unwrap(),
            FireDecision::AlreadyFired {
                slot: "2026-10-04".into()
            }
        );
    }

    #[test]
    fn evaluate_without_at_is_always_due_with_minute_slot() {
        let now = local_epoch(2026, 10, 4, 3, 5).unwrap() as u64 * 1000;
        assert_eq!(
            evaluate(wall_at(4, 3, 5), None, now, |_| false).unwrap(),
            FireDecision::Due {
                slot: "2026-10-04T0305".into()
            }
        );
        assert_eq!(
            evaluate(wall_at(4, 3, 5), None, now, |s| s == "2026-10-04T0305").unwrap(),
            FireDecision::AlreadyFired {
                slot: "2026-10-04T0305".into()
            }
        );
    }

    #[test]
    fn next_due_reports_today_then_tomorrow() {
        let at = Some((3, 0));
        let before = local_epoch(2026, 10, 4, 2, 0).unwrap() as u64 * 1000;
        let after = local_epoch(2026, 10, 4, 4, 0).unwrap() as u64 * 1000;
        assert_eq!(
            next_due(at, before).unwrap(),
            local_epoch(2026, 10, 4, 3, 0).unwrap()
        );
        assert_eq!(
            next_due(at, after).unwrap(),
            local_epoch(2026, 10, 5, 3, 0).unwrap()
        );
        // No daily time: every invocation is due now.
        assert_eq!(next_due(None, after).unwrap(), after as i64 / 1000);
    }

    #[test]
    fn schedule_file_round_trips_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let schedule = HandoffSchedule {
            schema_version: HANDOFF_SCHEDULE_SCHEMA_VERSION,
            created_at: "2026-10-04T00:00:00.000Z".into(),
            at: Some("03:00".into()),
            task: HandoffTaskSpec {
                prompt_file: "/tmp/ROLE.md".into(),
                engine: Some("antigravity".into()),
                profile: None,
                cwd: None,
                timeout_secs: Some(3600),
                engine_args: vec![],
                env: vec!["K=V".into()],
                no_skip_permissions: true,
                notify_to: None,
                notify_workspace: None,
                no_notify: false,
                cutoff: None,
                cutoff_engine: None,
            },
        };
        let path = save_schedule(tmp.path(), &schedule).unwrap();
        assert_eq!(path, schedule_path(tmp.path()));
        assert_eq!(load_schedule(tmp.path()).unwrap().as_ref(), Some(&schedule));

        // The whole owned state is exactly this directory; removing it is
        // the disable operation, and a second removal reports already-gone.
        assert!(remove_handoff_state(tmp.path()).unwrap());
        assert!(!remove_handoff_state(tmp.path()).unwrap());
        assert!(load_schedule(tmp.path()).unwrap().is_none());
    }
}
