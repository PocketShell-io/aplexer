//! Per-session process count and CPU, from one `/proc` pass.
//!
//! Count is the live process tree of the session worker: the worker itself
//! and every process whose nearest ancestor in the queried set is that
//! worker. A session started inside another session is its own root, so the
//! parent's count does not swallow the child.
//!
//! CPU is a rate, which needs two samples. [`bracket_session_proc_usage`]
//! takes a sample the caller already made, waits only until a short window
//! has elapsed (the wait is zero when the caller already spent that long),
//! and samples again. [`cached_session_proc_usage`] never waits: it turns
//! the previous call's sample into a rate when that sample is recent, and
//! otherwise reports the count with no rate. Neither path sleeps on a cold
//! cache for longer than the bracket window, and the JSON path does not
//! sleep at all.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

use crate::persist::atomic_write_json;

/// How long a bracketed sample waits, at most, so a single human listing
/// can show a rate. Callers that already did this much work since the first
/// sample do not wait at all.
pub const PROC_CPU_WINDOW: Duration = Duration::from_millis(40);

/// Cache deltas younger than this reuse the previous rate instead of
/// publishing a one-tick estimate.
const CPU_MIN_DELTA_MS: u64 = 200;
/// Older than this, the stored counters are not "what it is doing now".
const CPU_MAX_DELTA_MS: u64 = 30_000;

/// Below this, a short sample of an idle tree is tick noise, not load.
const CPU_DISPLAY_MIN_PERCENT: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionProcUsage {
    pub processes: u32,
    /// Percent of one core, summed across the tree. `None` when this call
    /// had no second sample close enough to turn counters into a rate.
    pub cpu_percent: Option<f64>,
}

impl SessionProcUsage {
    pub fn cpu_percent_1dp(self) -> Option<f64> {
        self.cpu_percent
            .filter(|cpu| cpu.is_finite() && *cpu >= 0.0)
            .map(|cpu| (cpu * 10.0).round() / 10.0)
    }

    /// List-row text: `17p` plus ` 340%` when the rate is above the noise
    /// floor. Empty when the worker has no live tree.
    pub fn list_suffix(self) -> String {
        if self.processes == 0 {
            return String::new();
        }
        let mut text = format!("{}p", self.processes);
        if let Some(cpu) = self.cpu_percent_1dp() {
            if cpu >= CPU_DISPLAY_MIN_PERCENT {
                text.push_str(&format!(" {cpu:.0}%"));
            }
        }
        text
    }

    /// One status line, without the label column.
    pub fn detail(self) -> String {
        match self.cpu_percent_1dp() {
            Some(cpu) if cpu >= CPU_DISPLAY_MIN_PERCENT => {
                format!("{} processes · {cpu:.0}% cpu", self.processes)
            }
            _ => format!("{} processes", self.processes),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProcRow {
    pub(crate) pid: u32,
    pub(crate) start_ticks: u64,
    pub(crate) jiffies: u64,
    pub(crate) processes: u32,
}

#[derive(Debug, Clone)]
pub struct ProcSnapshot {
    at_ms: u64,
    pub(crate) rows: BTreeMap<Uuid, ProcRow>,
}

struct ProcStat {
    ppid: u32,
    jiffies: u64,
    start_ticks: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageCache {
    mono_ms: u64,
    sessions: BTreeMap<Uuid, UsageCacheEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageCacheEntry {
    pid: u32,
    start_ticks: u64,
    jiffies: u64,
    processes: u32,
    #[serde(default)]
    cpu_percent: Option<f64>,
}

pub fn clock_ticks_hz() -> u64 {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 {
        hz as u64
    } else {
        100
    }
}

pub(crate) fn mono_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add((ts.tv_nsec as u64) / 1_000_000)
}

/// `utime + stime` over `dt_ms`, as a percent of one core.
pub fn cpu_percent(before_jiffies: u64, after_jiffies: u64, dt_ms: u64, hz: u64) -> Option<f64> {
    if dt_ms == 0 || hz == 0 || after_jiffies < before_jiffies {
        return None;
    }
    let cores = (after_jiffies - before_jiffies) as f64 / hz as f64 / (dt_ms as f64 / 1000.0);
    Some(cores * 100.0)
}

/// One pass over `proc_root`. Every root is present in the snapshot, with
/// `processes == 0` when that pid is not a live process.
pub fn scan_session_procs(proc_root: &Path, roots: &[(Uuid, u32)]) -> ProcSnapshot {
    let at_ms = mono_ms();
    let mut rows: BTreeMap<Uuid, ProcRow> = BTreeMap::new();
    let mut root_by_pid: HashMap<u32, Uuid> = HashMap::with_capacity(roots.len());
    for (id, pid) in roots {
        if *pid == 0 {
            continue;
        }
        rows.insert(
            *id,
            ProcRow {
                pid: *pid,
                start_ticks: 0,
                jiffies: 0,
                processes: 0,
            },
        );
        root_by_pid.insert(*pid, *id);
    }
    if rows.is_empty() {
        return ProcSnapshot { at_ms, rows };
    }

    let mut stats: HashMap<u32, ProcStat> = HashMap::new();
    let Ok(entries) = fs::read_dir(proc_root) else {
        return ProcSnapshot { at_ms, rows };
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path().join("stat")) else {
            continue;
        };
        if let Some(stat) = parse_stat(&bytes) {
            stats.insert(pid, stat);
        }
    }

    let mut memo: HashMap<u32, Option<Uuid>> = HashMap::new();
    let pids: Vec<u32> = stats.keys().copied().collect();
    for pid in pids {
        let Some(owner) = owner_of(pid, &stats, &root_by_pid, &mut memo) else {
            continue;
        };
        let Some(stat) = stats.get(&pid) else {
            continue;
        };
        let Some(row) = rows.get_mut(&owner) else {
            continue;
        };
        row.processes = row.processes.saturating_add(1);
        row.jiffies = row.jiffies.saturating_add(stat.jiffies);
        if pid == row.pid {
            row.start_ticks = stat.start_ticks;
        }
    }
    ProcSnapshot { at_ms, rows }
}

/// Second sample for a human listing. Sleeps only the remainder of
/// `min_window` after `first.at_ms`, then stores the sample so a later
/// cache-only caller (JSON) can rate it without waiting.
pub fn bracket_session_proc_usage(
    proc_root: &Path,
    cache_path: &Path,
    first: &ProcSnapshot,
    roots: &[(Uuid, u32)],
    min_window: Duration,
) -> BTreeMap<Uuid, SessionProcUsage> {
    if roots.is_empty() {
        return BTreeMap::new();
    }
    let min_ms = u64::try_from(min_window.as_millis()).unwrap_or(u64::MAX);
    let elapsed = mono_ms().saturating_sub(first.at_ms);
    if elapsed < min_ms {
        thread::sleep(Duration::from_millis(min_ms - elapsed));
    }
    let second = scan_session_procs(proc_root, roots);
    let hz = clock_ticks_hz();
    let dt = second.at_ms.saturating_sub(first.at_ms);
    let mut usage = BTreeMap::new();
    for (id, row) in &second.rows {
        let cpu = first
            .rows
            .get(id)
            .and_then(|prev| rate_if_same(prev, row, dt, hz, 20));
        usage.insert(
            *id,
            SessionProcUsage {
                processes: row.processes,
                cpu_percent: cpu,
            },
        );
    }
    let _ = write_cache(cache_path, &second, &usage);
    usage
}

/// One sample, rated against the cache when that sample is between
/// [`CPU_MIN_DELTA_MS`] and [`CPU_MAX_DELTA_MS`] old. Never sleeps.
pub fn cached_session_proc_usage(
    proc_root: &Path,
    cache_path: &Path,
    roots: &[(Uuid, u32)],
) -> BTreeMap<Uuid, SessionProcUsage> {
    let now = scan_session_procs(proc_root, roots);
    let cache = read_cache(cache_path);
    let dt = cache
        .as_ref()
        .map(|cached| now.at_ms.saturating_sub(cached.mono_ms))
        .unwrap_or(0);
    let hz = clock_ticks_hz();
    let mut usage = BTreeMap::new();
    for (id, row) in &now.rows {
        let cpu = cache.as_ref().and_then(|cached| {
            let prev = cached.sessions.get(id)?;
            if prev.pid != row.pid || prev.start_ticks != row.start_ticks || row.start_ticks == 0 {
                return None;
            }
            if dt < CPU_MIN_DELTA_MS {
                return prev.cpu_percent;
            }
            if dt > CPU_MAX_DELTA_MS {
                return None;
            }
            cpu_percent(prev.jiffies, row.jiffies, dt, hz)
        });
        usage.insert(
            *id,
            SessionProcUsage {
                processes: row.processes,
                cpu_percent: cpu,
            },
        );
    }
    let _ = write_cache(cache_path, &now, &usage);
    usage
}

fn rate_if_same(prev: &ProcRow, row: &ProcRow, dt_ms: u64, hz: u64, min_dt_ms: u64) -> Option<f64> {
    if prev.pid != row.pid || prev.start_ticks != row.start_ticks || row.start_ticks == 0 {
        return None;
    }
    if dt_ms < min_dt_ms {
        return None;
    }
    cpu_percent(prev.jiffies, row.jiffies, dt_ms, hz)
}

fn owner_of(
    start: u32,
    stats: &HashMap<u32, ProcStat>,
    roots: &HashMap<u32, Uuid>,
    memo: &mut HashMap<u32, Option<Uuid>>,
) -> Option<Uuid> {
    let mut chain = Vec::new();
    let mut current = start;
    loop {
        if let Some(known) = memo.get(&current) {
            let answer = *known;
            for step in chain {
                memo.insert(step, answer);
            }
            return answer;
        }
        if let Some(id) = roots.get(&current) {
            let answer = Some(*id);
            memo.insert(current, answer);
            for step in chain {
                memo.insert(step, answer);
            }
            return answer;
        }
        if chain.contains(&current) || chain.len() >= 64 {
            for step in chain {
                memo.insert(step, None);
            }
            return None;
        }
        let Some(stat) = stats.get(&current) else {
            for step in chain {
                memo.insert(step, None);
            }
            return None;
        };
        chain.push(current);
        if stat.ppid == 0 || stat.ppid == current {
            for step in chain {
                memo.insert(step, None);
            }
            return None;
        }
        current = stat.ppid;
    }
}

/// `/proc/<pid>/stat` after the comm field, which is wrapped in parentheses
/// and may itself contain spaces and parentheses.
fn parse_stat(bytes: &[u8]) -> Option<ProcStat> {
    let end = bytes.iter().rposition(|byte| *byte == b')')?;
    let rest = bytes.get(end + 1..)?;
    let mut fields = rest
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let _state = fields.next()?;
    let ppid = parse_u32(fields.next()?)?;
    // pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    for _ in 0..9 {
        fields.next()?;
    }
    let utime = parse_u64(fields.next()?)?;
    let stime = parse_u64(fields.next()?)?;
    // cutime cstime priority nice num_threads itrealvalue
    for _ in 0..6 {
        fields.next()?;
    }
    let start_ticks = parse_u64(fields.next()?)?;
    Some(ProcStat {
        ppid,
        jiffies: utime.saturating_add(stime),
        start_ticks,
    })
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn read_cache(path: &Path) -> Option<UsageCache> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(
    path: &Path,
    snapshot: &ProcSnapshot,
    usage: &BTreeMap<Uuid, SessionProcUsage>,
) -> Result<(), ()> {
    let sessions = snapshot
        .rows
        .iter()
        .map(|(id, row)| {
            (
                *id,
                UsageCacheEntry {
                    pid: row.pid,
                    start_ticks: row.start_ticks,
                    jiffies: row.jiffies,
                    processes: row.processes,
                    cpu_percent: usage.get(id).and_then(|sample| sample.cpu_percent),
                },
            )
        })
        .collect();
    atomic_write_json(
        path,
        &UsageCache {
            mono_ms: snapshot.at_ms,
            sessions,
        },
    )
    .map_err(|_| ())
}
