//! Native delegated tasks: run one prompt against a configured engine
//! noninteractively, capture the evidence, and report completion through the
//! existing durable messaging identity.
//!
//! This is the native replacement for homemade orchestration glue (a prompt
//! file, a noninteractive engine exec, stdout/stderr/result files, the real
//! exit code, and an identity-bound completion notice). Everything session- or
//! registry-shaped is deliberately left to the existing machinery: the caller
//! hosts a task in a durable session with `a start -- a task run …` (which
//! records the real `parent_session` lineage), and the completion notice rides
//! the ordinary workspace mailbox with the calling session's own identity.
//!
//! Nothing here kills anything but the task's own child process group: a
//! timeout SIGKILLs the group the child was spawned into (`process_group(0)`),
//! never a session, a worker, or any foreign process, and an engine chosen by
//! cutoff routing only affects launches that have not started yet.

use crate::persist::atomic_write_json;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

/// Bumped when the START/RESULT record shape changes.
pub const TASK_RECORD_SCHEMA_VERSION: u32 = 1;

/// Default wait before a timed-out task's child group is SIGKILLed, mirroring
/// the reference glue's hard stop. Only applies when `--timeout-secs` is set.
pub const DEFAULT_TIMEOUT_SECS: u64 = 14_400;

/// Where task artifacts land when `--output-dir` is not given: under the
/// task's working directory, so evidence travels with the checkout it ran in.
pub const DEFAULT_OUTPUT_ROOT: &str = ".aplexer-tasks";

// -- Time helpers: RFC 3339 with an explicit UTC offset, no new dependency --

/// Parse an RFC 3339 timestamp that carries its UTC offset (`Z` or `±HH:MM`)
/// into seconds since the Unix epoch. A naive timestamp (no offset) is an
/// error on purpose: cutoff comparison must be timezone-aware, never silently
/// local. Fractional seconds are accepted and truncated.
pub fn parse_offset_timestamp(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let bad = || {
        anyhow!("timestamp {raw:?} must be RFC 3339 with an explicit UTC offset (e.g. 2026-10-04T03:00:00+02:00 or 2026-10-04T01:00:00Z)")
    };
    let (date, rest) = raw
        .split_once('T')
        .or_else(|| raw.split_once('t'))
        .ok_or_else(bad)?;
    let (y, m, d) = parse_date(date).ok_or_else(bad)?;
    let (time, offset) = split_offset(rest).ok_or_else(bad)?;
    let (hh, mm, ss) = parse_time(time).ok_or_else(bad)?;
    let offset_secs = offset_seconds(offset).ok_or_else(bad)?;

    let days = days_from_civil(y, m, d);
    let secs = days * 86_400 + hh as i64 * 3600 + mm as i64 * 60 + ss as i64 - offset_secs;
    u64::try_from(secs).map_err(|_| bad())
}

fn parse_date(date: &str) -> Option<(i64, u32, u32)> {
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

fn split_offset(rest: &str) -> Option<(&str, &str)> {
    if let Some(rest) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((rest, "Z"));
    }
    let index = rest.rfind(['+', '-'])?;
    if index == 0 {
        return None;
    }
    Some((&rest[..index], &rest[index..]))
}

fn parse_time(time: &str) -> Option<(u32, u32, u32)> {
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    // A fractional part must actually be digits: `03:00:00.Z` is malformed,
    // not a fraction that can be ignored.
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let mut parts = clock.split(':');
    let hh: u32 = parts.next()?.parse().ok()?;
    let mm: u32 = parts.next()?.parse().ok()?;
    let ss: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some((hh, mm, ss))
}

fn offset_seconds(offset: &str) -> Option<i64> {
    match offset {
        "Z" | "z" => Some(0),
        _ => {
            let sign = match offset.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = &offset[1..];
            let (hh, mm) = match body.split_once(':') {
                Some((hh, mm)) => (hh.parse::<i64>().ok()?, mm.parse::<i64>().ok()?),
                // ±HHMM compact form is legal RFC 3339.
                None if body.len() == 4 => (body[..2].parse().ok()?, body[2..].parse().ok()?),
                _ => return None,
            };
            if hh > 23 || mm > 59 {
                return None;
            }
            Some(sign * (hh * 3600 + mm * 60))
        }
    }
}

/// Days since 1970-01-01 from a proleptic Gregorian civil date
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Format epoch milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
pub fn rfc3339_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z",
        h = tod / 3600,
        mi = (tod % 3600) / 60,
        s = tod % 60
    )
}

/// Inverse of `days_from_civil` (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// -- Engine routing: cutoff-aware, launch-time only --

/// Which engine a *new* task launch uses: `engine`, unless a cutoff instant
/// (parsed timezone-aware from its own explicit offset) has passed, in which
/// case `cutoff_engine`. Purely a launch-time decision — nothing running is
/// ever interrupted, and the caller owns any context handoff between engines.
pub fn route_engine(
    engine: &str,
    cutoff: Option<&str>,
    cutoff_engine: Option<&str>,
    now: u64,
) -> Result<String> {
    match (cutoff, cutoff_engine) {
        (None, None) => Ok(engine.to_string()),
        (Some(cutoff), Some(cutoff_engine)) => {
            let cutoff_secs = parse_offset_timestamp(cutoff)?;
            if now / 1000 >= cutoff_secs {
                Ok(cutoff_engine.to_string())
            } else {
                Ok(engine.to_string())
            }
        }
        (Some(_), None) => bail!("--cutoff requires --cutoff-engine"),
        (None, Some(_)) => bail!("--cutoff-engine requires --cutoff"),
    }
}

/// The per-engine argv that turns a resolved engine command into a
/// noninteractive prompt run, with the prompt text appended as the final argv
/// element. `configured` is the engine's optional `task_argv` from the user's
/// config file; when absent the built-in table answers by engine id/family.
/// `None` means "no known noninteractive mode": the caller must refuse rather
/// than guess an engine's flags.
pub fn noninteractive_argv(engine: &str, configured: Option<&[String]>) -> Option<Vec<String>> {
    if let Some(argv) = configured {
        return Some(argv.to_vec());
    }
    builtin_task_argv(engine).map(|argv| argv.iter().map(|s| s.to_string()).collect())
}

/// Built-in noninteractive argv by engine id, falling back to the engine's
/// transcript family (`engine_family`, e.g. a user-configured `zcodex` fork
/// speaking the codex wire format). Engines with genuinely unknown
/// noninteractive flags return `None` — a refusal, never a guess.
fn builtin_task_argv(engine: &str) -> Option<&'static [&'static str]> {
    let family = crate::engine_family(engine);
    match family {
        // `codex exec [OPTIONS] [PROMPT]`: noninteractive run of one prompt;
        // `--json` emits machine-readable events on stdout; the repo check
        // is off because task cwd is often a bare worktree.
        "codex" => Some(&["exec", "--json", "--skip-git-repo-check"]),
        "antigravity" => Some(&["-p"]),
        "claude" => Some(&["-p"]),
        "gemini" => Some(&["-p"]),
        "opencode" => Some(&["run"]),
        _ => None,
    }
}

// -- Task records: the on-disk evidence contract --

/// Written before the child is spawned, so a crashed or killed launcher still
/// leaves the launch facts behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStartRecord {
    pub schema_version: u32,
    pub task_id: Uuid,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The child argv with the prompt elided (size + sha256 only): argv
    /// fidelity for debugging, without duplicating a possibly huge prompt.
    pub argv: Vec<String>,
    pub prompt_path: PathBuf,
    pub prompt_bytes: u64,
    pub prompt_sha256: String,
    pub cwd: PathBuf,
    pub output_dir: PathBuf,
    /// The session `a task run` ran inside — the real parent of this task,
    /// exactly the identity the completion notice is sent as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<ParentSession>,
    pub started_at: String,
    pub started_ms: u64,
}

/// Written after the child settles (naturally or via timeout). The actual
/// exit status is the contract: `a task run` exits with the same code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResultRecord {
    pub schema_version: u32,
    pub task_id: Uuid,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub output_dir: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    pub prompt_path: PathBuf,
    pub prompt_bytes: u64,
    pub prompt_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<ParentSession>,
    pub started_at: String,
    pub started_ms: u64,
    pub ended_at: String,
    pub ended_ms: u64,
    /// Actual child exit code, or 124 on timeout.
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub notice: NoticeRecord,
}

/// The calling session a task ran inside. Recorded from the ambient
/// `APLEXER_SESSION_ID` plus its live session record — never from a flag, so
/// it can only be a real session this process actually belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParentSession {
    pub id: Uuid,
    pub tag: String,
    pub workspace: PathBuf,
}

/// What happened to the durable completion notice. A notice problem never
/// discards the task result: `status` says what to look at instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoticeRecord {
    /// sent | no-session-identity | target-unresolved | failed
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl NoticeRecord {
    pub fn skipped(status: &str, detail: String) -> Self {
        Self {
            status: status.to_string(),
            message_id: None,
            detail: Some(detail),
        }
    }
}

/// SHA-256 of arbitrary bytes, hex-encoded — the prompt fingerprint in the
/// START/RESULT records.
pub fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    crate::history::hex_encode(&hasher.finalize())
}

/// The child argv for the record: everything except the prompt, then one
/// elided marker naming the prompt's size and digest.
pub fn record_argv(mut argv: Vec<String>, prompt_bytes: u64, prompt_sha256: &str) -> Vec<String> {
    argv.push(format!(
        "<prompt: {prompt_bytes} bytes, sha256={prompt_sha256}>"
    ));
    argv
}

/// Default output directory: `<cwd>/.aplexer-tasks/<UTC stamp>-<engine>-<id8>`.
/// The id suffix keeps two tasks of the same engine started in the same second
/// from sharing a directory.
pub fn default_output_dir(cwd: &Path, engine: &str, started_ms: u64, task_id: Uuid) -> PathBuf {
    let stamp = rfc3339_utc(started_ms);
    let compact: String = stamp.chars().filter(|c| c.is_ascii_digit()).collect();
    let id8 = task_id.simple().to_string()[..8].to_string();
    cwd.join(DEFAULT_OUTPUT_ROOT)
        .join(format!("{compact}-{engine}-{id8}"))
}

/// Write a record atomically (`atomic_write_json`), passing the path back for
/// the caller's record.
pub fn write_task_record(path: &Path, record: &impl Serialize) -> Result<PathBuf> {
    atomic_write_json(path, record)?;
    Ok(path.to_path_buf())
}

// -- Child execution: spawn, capture, bounded wait, own-group kill --

/// Outcome of waiting for the task child. `exit_signal` is set when the child
/// died to a signal instead of exiting normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildOutcome {
    pub exit_code: i32,
    pub exit_signal: Option<i32>,
    pub timed_out: bool,
}

/// Spawn `argv` in `cwd` with `env_set` applied over the inherited
/// environment and `env_unset` removed last (the same strip-wins ordering the
/// worker uses), stdout/stderr into the given files, and wait at most
/// `timeout`; on expiry SIGKILL the child's own process group and report exit
/// 124. The kill goes to the group spawned for this child only — never the
/// caller's group, a session worker, or any unrelated process.
pub fn run_task_child(
    argv: &[String],
    cwd: &Path,
    env_set: &BTreeMap<String, String>,
    env_unset: &[String],
    stdout_file: &Path,
    stderr_file: &Path,
    timeout: Option<Duration>,
) -> Result<ChildOutcome> {
    let program = argv.first().ok_or_else(|| anyhow!("task argv is empty"))?;
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .envs(env_set)
        // Noninteractive by construction: no stdin to hang off, output to the
        // evidence files, not to whatever terminal happens to be attached.
        .stdin(Stdio::null())
        .stdout(process_stdio(stdout_file)?)
        .stderr(process_stdio(stderr_file)?);
    // Own process group: the timeout kill below addresses exactly this
    // group and nothing else (a foreign-process kill can never be a side
    // effect of a task timeout). On Windows the same containment is a Job
    // Object holding only this child (see `task_job`).
    #[cfg(unix)]
    command.process_group(0);
    // Provider-key / configured strip, applied LAST so it wins over env_set,
    // exactly like the worker's workload spawn ordering.
    for name in env_unset {
        command.env_remove(name);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn task child {}", program))?;
    #[cfg(unix)]
    let pid = child.id();
    // Dropping the job (KILL_ON_JOB_CLOSE) also reaps any stragglers the
    // child left behind when this function returns.
    #[cfg(windows)]
    let job = {
        use std::os::windows::io::AsRawHandle;
        let job = task_job::Job::create_kill_on_close().context("create task job object")?;
        job.assign_process(child.as_raw_handle() as _)
            .context("assign task child to job object")?;
        job
    };

    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let outcome = loop {
        match child.try_wait()? {
            Some(status) => {
                let exit_signal = exit_signal_of(&status);
                let exit_code = status.code().unwrap_or_else(|| match exit_signal {
                    Some(signal) => 128 + signal,
                    None => 1,
                });
                break ChildOutcome {
                    exit_code,
                    exit_signal,
                    timed_out: false,
                };
            }
            None => {
                if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                    #[cfg(unix)]
                    kill_child_group(pid);
                    // Terminated with 124 so the reported exit code is the timeout
                    // code (Unix gets it from the missing exit status instead).
                    #[cfg(windows)]
                    if let Err(error) = job.terminate(124) {
                        eprintln!("a: task timeout kill of job object failed: {error}");
                    }
                    let status = child.wait()?;
                    // After SIGKILL there is no exit code; if the child
                    // happened to exit normally in the instant before the
                    // kill landed, its real status is the honest answer and
                    // `timed_out` records that the kill was attempted.
                    break ChildOutcome {
                        exit_code: status.code().unwrap_or(124),
                        exit_signal: exit_signal_of(&status),
                        timed_out: true,
                    };
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    };
    Ok(outcome)
}

fn process_stdio(path: &Path) -> Result<Stdio> {
    let file = File::create(path)
        .with_context(|| format!("create task output file {}", path.display()))?;
    Ok(Stdio::from(file))
}

#[cfg(unix)]
fn exit_signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

/// Windows has no signals; a killed process just reports an exit code.
#[cfg(windows)]
fn exit_signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Minimal local Job Object wrapper for task timeouts. Same names as the
/// shared `sys::windows::job::Job::{create_kill_on_close, assign_process,
/// terminate}`; switch to that once it lands.
#[cfg(windows)]
mod task_job {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    impl Job {
        pub fn create_kill_on_close() -> io::Result<Job> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let job = Job(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(job)
            }
        }

        pub fn assign_process(&self, process: HANDLE) -> io::Result<()> {
            if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.0, exit_code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// SIGKILL the child's own process group. ESRCH (already gone) is success;
/// every other error is reported, never swept under a "best effort".
#[cfg(unix)]
fn kill_child_group(pid: u32) {
    let pid = libc::pid_t::try_from(pid).unwrap_or(0);
    if pid <= 0 {
        return;
    }
    let rc = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!("a: task timeout kill of group -{pid} failed: {error}");
        }
    }
}

/// Flush helper used by the CLI after writing human-readable progress lines.
pub fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

/// Convenience for callers composing the notice body: one line naming the
/// engine, the exit status, and where the evidence lives.
pub fn notice_body(engine: &str, result: &TaskResultRecord) -> String {
    let status = if result.timed_out {
        "timeout".to_string()
    } else {
        format!("exit {}", result.exit_code)
    };
    format!(
        "Task terminal: engine {engine}, {status}. Evidence: {} (RESULT.json, stdout.log, stderr.log)",
        result.output_dir.display()
    )
}

/// Structured payload riding the notice envelope's `data` field.
pub fn notice_data(result: &TaskResultRecord) -> serde_json::Value {
    json!({
        "task_id": result.task_id,
        "engine": result.engine,
        "exit_code": result.exit_code,
        "timed_out": result.timed_out,
        "output_dir": result.output_dir,
        "result_path": result_path(&result.output_dir),
        "parent_session": result.parent_session,
    })
}

pub fn result_path(output_dir: &Path) -> PathBuf {
    output_dir.join("RESULT.json")
}

pub fn start_record_path(output_dir: &Path) -> PathBuf {
    output_dir.join("START.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BERLIN_CUTOFF: &str = "2026-10-04T03:00:00+02:00";

    #[test]
    fn offset_timestamp_parses_to_epoch_seconds() {
        // 2026-10-04T01:00:00Z == 2026-10-04T03:00:00+02:00.
        assert_eq!(
            parse_offset_timestamp(BERLIN_CUTOFF).unwrap(),
            parse_offset_timestamp("2026-10-04T01:00:00Z").unwrap()
        );
        // Independent epoch value for that instant (date -u -d ... +%s).
        assert_eq!(
            parse_offset_timestamp("2026-10-04T01:00:00Z").unwrap(),
            1_791_075_600
        );
        // Compact offset and fractional seconds are accepted.
        assert_eq!(
            parse_offset_timestamp("2026-10-04T03:00:00.500+0200").unwrap(),
            1_791_075_600
        );
        assert_eq!(parse_offset_timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
    }

    #[test]
    fn offset_timestamp_rejects_naive_and_malformed() {
        for bad in [
            "2026-10-04T03:00:00",       // no offset: must fail, never assume local
            "2026-10-04 03:00:00Z",      // space separator is not accepted
            "2026-10-04T03:00:00+25:00", // impossible offset
            "not-a-time",
            "2026-13-01T00:00:00Z",
            "",
        ] {
            assert!(parse_offset_timestamp(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn cutoff_routes_only_after_the_instant() {
        let cutoff = parse_offset_timestamp(BERLIN_CUTOFF).unwrap();
        let before = (cutoff - 1) * 1000;
        let at = cutoff * 1000;
        assert_eq!(
            route_engine("zcodex", Some(BERLIN_CUTOFF), Some("antigravity"), before).unwrap(),
            "zcodex"
        );
        assert_eq!(
            route_engine("zcodex", Some(BERLIN_CUTOFF), Some("antigravity"), at).unwrap(),
            "antigravity"
        );
        assert_eq!(
            route_engine(
                "zcodex",
                Some(BERLIN_CUTOFF),
                Some("antigravity"),
                at + 3_600_000
            )
            .unwrap(),
            "antigravity"
        );
        // No cutoff: the requested engine always wins.
        assert_eq!(route_engine("zcodex", None, None, at).unwrap(), "zcodex");
        // Half a flag pair is an error, never a silent fallback.
        assert!(route_engine("zcodex", Some(BERLIN_CUTOFF), None, at).is_err());
        assert!(route_engine("zcodex", None, Some("antigravity"), at).is_err());
    }

    #[test]
    fn noninteractive_argv_by_family_and_config() {
        // The codex family (including a user-configured zcodex fork) execs.
        assert_eq!(
            noninteractive_argv("zcodex", None).unwrap(),
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string()
            ]
        );
        assert_eq!(
            noninteractive_argv("codex", None).unwrap(),
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string()
            ]
        );
        assert_eq!(
            noninteractive_argv("antigravity", None).unwrap(),
            vec!["-p".to_string()]
        );
        // Engine-specific config wins over the builtin table.
        assert_eq!(
            noninteractive_argv(
                "antigravity",
                Some(&[
                    "-p".to_string(),
                    "--output-format".to_string(),
                    "json".to_string()
                ])
            )
            .unwrap(),
            vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "json".to_string()
            ]
        );
        // Unknown engines refuse rather than guess.
        assert!(noninteractive_argv("shell", None).is_none());
        assert!(noninteractive_argv("grok", None).is_none());
    }

    #[test]
    fn rfc3339_utc_formats_epoch_millis() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_utc(1_791_075_600_123), "2026-10-04T01:00:00.123Z");
        // Round-trip through the parser.
        let ms = 1_791_075_600_000;
        assert_eq!(parse_offset_timestamp(&rfc3339_utc(ms)).unwrap(), ms / 1000);
    }

    #[test]
    fn default_output_dir_is_unique_per_task() {
        let cwd = Path::new("/tmp/ws");
        let a = default_output_dir(cwd, "zcodex", 1_792_774_800_000, Uuid::nil());
        let b = default_output_dir(cwd, "zcodex", 1_792_774_800_000, Uuid::now_v7());
        assert_ne!(a, b, "same-second tasks must not share a directory");
        assert!(a.starts_with(cwd.join(".aplexer-tasks")));
    }

    #[test]
    fn record_argv_elides_prompt_but_keeps_fingerprint() {
        let argv = record_argv(vec!["agy".into(), "-p".into()], 42, "abc123");
        assert_eq!(argv[..2], ["agy", "-p"]);
        assert_eq!(argv[2], "<prompt: 42 bytes, sha256=abc123>");
        assert_eq!(argv.len(), 3);
    }
}
