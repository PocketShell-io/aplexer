//! A live, delegated workload scope whose initial process IS the workload.
//!
//! One reason this shape exists: the workload must be PLACED in the scope by
//! systemd, never moved in after the fact. Under Ubuntu's default
//! `nsdelegate` cgroup2 mount policy the delegated `user@1000.service`
//! subtree is a boundary that processes outside it (the worker lives in a
//! logind session scope) cannot cross -- writing even their own pid into a
//! `cgroup.procs` inside the subtree fails with EACCES, so the earlier
//! anchor-plus-migration design failed closed on exactly the boxes that
//! most need the containment (PocketShell-io/pocketshell-cli#13). What the
//! probes proved works is what `systemd-run --scope` already does for every
//! user service on the box: the child's pid rides the `StartTransientUnit`
//! call as `PIDs=`, and the manager -- which owns the delegated subtree --
//! places it at unit start. The worker therefore spawns `systemd-run` with
//! the workload argv after `--`, and the workload is born inside the scope.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::{
    check_cgroup_cleanup_deadline, command_output_until, current_cgroup_identity,
    kill_cgroup_path_until, read_counter, signal_cgroup_path_until, system_scope_escape_decision,
    systemd_run_scope, trusted_system_helper, verify_recorded_cgroup_identity,
    wait_for_scope_cgroup, CGROUP_V2_ROOT,
};
use crate::{ensure_sigchld_compatible_for_child_management, CgroupIdentity, Limits};

#[derive(Debug, Clone)]
pub struct Cgroup {
    pub(crate) path: PathBuf,
    pub(crate) identity: CgroupIdentity,
    pub(crate) initial_oom_kill: u64,
    /// Which manager bus the scope was created on (`--user` / `--system`),
    /// and the pre-resolved trusted `systemctl`, so teardown can retire the
    /// unit without re-resolving helpers (see `Cgroup::retire_unit`).
    pub(crate) bus_flag: &'static str,
    pub(crate) systemctl: PathBuf,
}

/// Everything a capped launch resolves BEFORE any process is spawned: the
/// placement decision, the trusted helpers, the unit name, and the kernel
/// cgroup identity the scope must validate against. Spawning is split off
/// (`ScopePlan::scope_command` + `ScopePlan::finish_startup`) because the
/// scope and the workload are one transaction now -- there is no anchor to
/// create first, so "create the containment domain" and "start the
/// workload" are the same systemd unit-start.
#[derive(Debug, Clone)]
pub struct ScopePlan {
    pub(crate) id: Uuid,
    pub(crate) unit: String,
    pub(crate) bus_flag: &'static str,
    pub(crate) systemd_run: PathBuf,
    pub(crate) systemctl: PathBuf,
    pub(crate) identity: CgroupIdentity,
    pub(crate) limits: Limits,
}

impl ScopePlan {
    /// Resolve every decision and helper for a capped launch without
    /// spawning anything. `Ok(None)` means no limit was requested -- the
    /// common case -- and the caller spawns the workload directly with no
    /// scope at all.
    ///
    // Which manager owns the new scope is a placement decision with a real
    // failure-domain consequence (issue #1): the default `--user` scope
    // lives beneath user@UID.service and dies with the per-user manager's
    // exit.target; the opt-in `--system` scope (APLEXER_LAUNCH_SYSTEM_SCOPE
    // = system, probed first via `system_scope_escape_decision`) lives under
    // the system manager and survives it. Probe failure downgrades to the
    // user manager with a printed warning -- limits still apply either way;
    // only the survival domain differs. A failure *after* a successful probe
    // fails closed exactly as the `--user` path always has: a validated
    // backend that then breaks is a real error, not a placement preference
    // to silently swap.
    pub fn prepare(id: Uuid, limits: &Limits) -> Result<Option<Self>> {
        ensure_sigchld_compatible_for_child_management()?;
        if !limits.requested() {
            return Ok(None);
        }
        let system_scope = match system_scope_escape_decision() {
            Ok(system_scope) => system_scope,
            Err(error) => {
                eprintln!(
                    "warning: APLEXER_LAUNCH_SYSTEM_SCOPE=system requested, but the \
                     system-scope backend is unavailable ({error:#}); the workload scope \
                     falls back to the per-user manager and inherits its exit.target \
                     failure domain"
                );
                false
            }
        };
        let bus_flag = if system_scope { "--system" } else { "--user" };
        // Resolve every executable before starting the scope. Ambient PATH is
        // intentionally irrelevant: a user-controlled shadow helper must not
        // choose or fabricate the containment domain we later trust.
        let systemd_run = trusted_system_helper("systemd-run")?;
        let systemctl = trusted_system_helper("systemctl")?;
        let identity = current_cgroup_identity()?;
        Ok(Some(Self {
            id,
            unit: format!("aplexer-workload-{id}"),
            bus_flag,
            systemd_run,
            systemctl,
            identity,
            limits: limits.clone(),
        }))
    }

    /// `systemd-run <bus> --scope --collect --unit=<unit> -p Delegate=yes`
    /// plus one unit property per requested limit, with the workload argv
    /// verbatim after `--` as the scope's initial process. The env, PTY, and
    /// session setup the caller layers onto this `Command` land on the
    /// `systemd-run` wrapper and are inherited by its child: for `--scope`,
    /// systemd-run forks, the parent drives the bus transaction, and the
    /// forked child -- the pid handed to systemd as `PIDs=` -- execs the argv
    /// with the wrapper's environment, cwd, and stdio intact.
    pub fn scope_command(&self, workload: &[OsString]) -> Command {
        let mut command =
            systemd_run_scope(self.systemd_run.clone(), self.bus_flag, &self.unit, false);
        command.arg("-p").arg("Delegate=yes");
        if let Some(value) = self.limits.memory_bytes {
            command.arg("-p").arg(format!("MemoryMax={value}"));
            // Without a swap cap, hitting MemoryMax doesn't OOM-kill the
            // workload -- it swaps unboundedly instead, which both defeats
            // the purpose of a memory limit and risks host-wide I/O
            // pressure that *would* leak into unrelated sessions. A
            // memory-limited session gets no swap; a configurable swap
            // allowance is not yet exposed by the CLI.
            command.arg("-p").arg("MemorySwapMax=0");
        }
        if let Some(value) = self.limits.pids {
            command.arg("-p").arg(format!("TasksMax={value}"));
        }
        if let Some(quota) = self.limits.cpu_quota_us {
            let period = self.limits.cpu_period_us.unwrap_or(100_000);
            let percent = ((quota as f64 / period as f64) * 100.0).ceil().max(1.0) as u64;
            command.arg("-p").arg(format!("CPUQuota={percent}%"));
        }
        command.arg("--");
        for arg in workload {
            command.arg(arg);
        }
        command
    }

    /// Complete the capped launch after the wrapper has been spawned: wait
    /// for the scope systemd was asked to create, fail closed if the
    /// delegated controllers a requested limit needs are missing, discover
    /// the workload leader from the scope's own membership, and pin the OOM
    /// baseline. Returns the live cgroup and the leader's pid (the worker
    /// tracks the wrapper as its direct child, but the leader is the pid
    /// liveness, reclaim, and status reporting are all about).
    ///
    /// Any failure here tears the transaction down -- wrapper included --
    /// and returns the error: an uncapped workload must never survive a
    /// capped start attempt, so limits fail closed (the scope may hold a
    /// briefly-running workload while this verdict is being formed; the
    /// teardown kills it).
    pub fn finish_startup(&self, supervisor: &mut std::process::Child) -> Result<(Cgroup, u32)> {
        let outcome = self.verify_and_pin();
        match outcome {
            Ok((cgroup, leader)) => Ok((cgroup, leader)),
            Err(error) => Err(self.failure_teardown(supervisor, error)),
        }
    }

    fn verify_and_pin(&self) -> Result<(Cgroup, u32)> {
        let path = wait_for_scope_cgroup(
            self.id,
            &self.unit,
            &self.identity,
            &self.systemctl,
            self.bus_flag,
            Duration::from_secs(5),
        )
        .context("limits fail closed")?;
        verify_delegated_controllers(&path, &self.limits)?;
        let leader = scope_leader_pid(&path, &self.unit)?;
        let cgroup = Cgroup {
            path,
            identity: self.identity.clone(),
            initial_oom_kill: 0,
            bus_flag: self.bus_flag,
            systemctl: self.systemctl.clone(),
        };
        let initial_oom_kill = oom_kill_count(&cgroup.path);
        Ok((
            Cgroup {
                initial_oom_kill,
                ..cgroup
            },
            leader,
        ))
    }

    /// Kill the wrapper, empty the scope, and retire the unit, folding every
    /// cleanup failure into the original error. Order matters: the wrapper
    /// dies first, so a child still waiting for its start signal can never
    /// start; anything that already exec'd is SIGKILLed through the scope;
    /// the removal and unit stop are bookkeeping on the way out.
    fn failure_teardown(
        &self,
        supervisor: &mut std::process::Child,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let mut failures = Vec::new();
        match kill_supervisor_child(supervisor) {
            Ok(()) => {}
            Err(cleanup_error) => {
                failures.push(format!("stop systemd-run wrapper: {cleanup_error:#}"))
            }
        }
        // The authoritative path is known only once the scope query has
        // succeeded; before that, the unit stop is the best-effort fallback
        // (KillMode=control-group takes the members with it).
        if let Ok(path) = self.query_scope_path() {
            let deadline = Instant::now() + Duration::from_secs(5);
            if let Err(cleanup_error) = kill_cgroup_path_until(&path, deadline) {
                failures.push(format!("kill scope members: {cleanup_error:#}"));
            }
            let _ = fs::remove_dir(&path);
        }
        retire_scope_unit(
            &self.systemctl,
            self.bus_flag,
            &format!("{}.scope", self.unit),
        );
        if failures.is_empty() {
            error
        } else {
            anyhow!(
                "{error:#}; scope teardown failures: {}",
                failures.join("; ")
            )
        }
    }

    fn query_scope_path(&self) -> Result<PathBuf> {
        let mut command = Command::new(&self.systemctl);
        command.args([
            self.bus_flag,
            "show",
            &format!("{}.scope", self.unit),
            "-p",
            "ControlGroup",
            "--value",
        ]);
        let output = command_output_until(
            &mut command,
            Instant::now() + Duration::from_secs(5),
            "query systemd scope for teardown",
        )?;
        if !output.status.success() {
            bail!("systemctl show exited with {}", output.status);
        }
        let value = std::str::from_utf8(&output.stdout)
            .context("decode systemd ControlGroup output")?
            .trim();
        if value.is_empty() || value == "/" {
            bail!("scope has no ControlGroup to tear down");
        }
        super::control_group_locator(self.id, value)
    }
}

/// The workload leader systemd placed in the scope: the scope's initial --
/// and at this point only -- member. Polled briefly because the START job
/// that moves the wrapper's child into the scope completes before the
/// wrapper's parent exits, but the cgroup directory and its first member
/// can become visible a beat apart. The leader is the smallest pid present:
/// members the workload forked in its first milliseconds are strictly
/// younger than the pid systemd was handed at placement.
fn scope_leader_pid(path: &Path, unit: &str) -> Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(path.join("cgroup.procs")) {
            let leader = text
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
                .min();
            if let Some(leader) = leader {
                return Ok(leader);
            }
        }
        check_cgroup_cleanup_deadline(
            deadline,
            &format!("wait for systemd to place the workload leader of {unit} in its scope"),
        )?;
        thread::sleep(Duration::from_millis(10));
    }
}

/// A scope without the controller a requested limit needs cannot enforce
/// it; limits fail closed rather than silently not applying.
fn verify_delegated_controllers(path: &Path, limits: &Limits) -> Result<()> {
    for (requested, file, controller) in [
        (limits.memory_bytes.is_some(), "memory.max", "memory"),
        (limits.pids.is_some(), "pids.max", "pids"),
    ] {
        if requested && !path.join(file).exists() {
            bail!("systemd did not delegate the {controller} controller; limits fail closed");
        }
    }
    Ok(())
}

/// The kernel's `oom_kill` count for a cgroup; a scope without the memory
/// controller (or one already collected) has no such counter and reads 0.
fn oom_kill_count(path: &Path) -> u64 {
    read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0)
}

/// Kill a spawned wrapper child and reap it, tolerating an already-dead
/// child (its status may have been collected by an earlier best-effort
/// pass). The wrapper is a pre-reaper-arming startup child, so a plain
/// blocking `wait` owns its status exactly once.
fn kill_supervisor_child(supervisor: &mut std::process::Child) -> Result<()> {
    match supervisor.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error).context("kill systemd-run wrapper"),
    }
    supervisor.wait().context("reap systemd-run wrapper")?;
    Ok(())
}

/// Make the manager forget a scope unit after the direct removal.
///
/// `fs::remove_dir` is what guarantees the containment domain is gone, but
/// it also destroys the manager's watch on the scope's `cgroup.events`
/// before the manager can observe the scope emptying: the unit never
/// transitions to inactive, and the `--collect` set at creation never
/// fires. Every capped session leaked one permanently-"running" unit record
/// this way (1000+ accumulated in a week in the field;
/// PocketShell-io/pocketshell-cli#12). A bounded, best-effort `systemctl
/// stop` makes the manager re-evaluate the unit -- with the cgroup already
/// gone, the stop completes immediately and the record is unloaded. Like
/// the removal itself this is bookkeeping, not containment, so failure is
/// ignored.
fn retire_scope_unit(systemctl: &Path, bus_flag: &str, unit: &str) {
    let mut command = Command::new(systemctl);
    command.args([bus_flag, "stop", unit]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let _ = command_output_until(&mut command, deadline, "retire systemd scope unit");
}

impl Cgroup {
    pub fn locator(&self) -> &Path {
        &self.path
    }
    /// The same cgroup in `/proc/<pid>/cgroup` form (`/<relative>` under the
    /// cgroup-v2 root), so launch-time validation can compare what systemd
    /// was asked to create against what the workload actually reports being
    /// in (issue #1).
    pub fn proc_path(&self) -> String {
        let relative = self
            .path
            .strip_prefix(CGROUP_V2_ROOT)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .to_string();
        format!("/{}", relative.trim_start_matches('/'))
    }
    pub fn identity(&self) -> &CgroupIdentity {
        &self.identity
    }
    /// The path, once the live kernel domain has been re-pinned to the one
    /// this cgroup was created in: the precondition for every destructive
    /// pass over its members.
    fn recovered_path(&self, deadline: Instant) -> Result<&Path> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        Ok(&self.path)
    }
    pub fn signal_all_until(&self, signal: i32, deadline: Instant) -> Result<()> {
        signal_cgroup_path_until(self.recovered_path(deadline)?, signal, deadline)
    }
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        kill_cgroup_path_until(self.recovered_path(deadline)?, deadline)
    }
    pub fn populated(&self) -> Result<bool> {
        super::live_cgroup_populated_with(&self.identity, || {
            read_counter(&self.path.join("cgroup.events"), "populated")
        })
    }
    pub fn oom_killed(&self) -> bool {
        oom_kill_count(&self.path) > self.initial_oom_kill
    }
    /// Live telemetry for a still-running cgroup. A workload's own OOM kill
    /// only shows up in the session record's `exit` field once the tracked
    /// PTY-owning process itself exits -- a subprocess it launched can be
    /// OOM-killed by the kernel while the shell survives, which is common
    /// and otherwise invisible. `a status` surfaces this live instead of
    /// only at session exit.
    pub fn stats(&self) -> serde_json::Value {
        let read_value = |name: &str| -> Option<u64> {
            fs::read_to_string(self.path.join(name))
                .ok()
                .and_then(|text| text.trim().parse().ok())
        };
        let oom_kill_total = oom_kill_count(&self.path);
        serde_json::json!({
            "memory_current": read_value("memory.current"),
            "memory_peak": read_value("memory.peak"),
            "memory_swap_current": read_value("memory.swap.current"),
            "oom_kill_count": oom_kill_total,
            "oom_kill_count_since_start": oom_kill_total.saturating_sub(self.initial_oom_kill),
            // Status telemetry is explicitly best-effort; lifecycle and kill
            // paths call `populated` directly and propagate every error.
            "populated": self.populated().ok(),
        })
    }
    pub fn cleanup(&self) {
        let _ = fs::remove_dir(&self.path);
        retire_scope_unit(&self.systemctl, self.bus_flag, &self.unit_for_retire());
    }
    fn unit_for_retire(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string()
    }
}

/// `a doctor`'s delegated-scope probe: exercise the exact launch
/// implementation end to end with a short-lived placeholder workload --
/// trusted helper resolution, the scope transaction, controller delegation,
/// and member placement -- by launching `sleep 2` as the scope's initial
/// process and letting it exit on its own. No existing cgroup or workload
/// is modified; `--collect` reaps the probe scope once the placeholder
/// exits.
pub fn probe_placeholder_scope(limits: &Limits) -> Result<()> {
    let id = Uuid::new_v4();
    let plan = prepare_scope_plan(id, limits)?
        .ok_or_else(|| anyhow!("limit probe prepared no scope despite requested limits"))?;
    let sleep = trusted_system_helper("sleep")?;
    let workload = vec![sleep.into_os_string(), OsString::from("2")];
    let mut command = plan.scope_command(&workload);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut supervisor = command
        .spawn()
        .context("spawn systemd-run probe scope; limits fail closed")?;
    let probe = (|| -> Result<()> {
        let (cgroup, _leader) = plan.finish_startup(&mut supervisor)?;
        // The launch path verifies memory and pids delegation; the doctor's
        // bar is stricter -- it advertises cpu too, so prove that file as
        // well rather than let `a doctor` say "ok" past a missing one.
        if limits.cpu_quota_us.is_some() && !cgroup.locator().join("cpu.max").is_file() {
            bail!(
                "delegated scope is missing {}",
                cgroup.locator().join("cpu.max").display()
            );
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match supervisor.try_wait()? {
                Some(status) if status.success() => break,
                Some(status) => bail!("probe scope's placeholder exited with {status}"),
                None if Instant::now() >= deadline => {
                    bail!("probe scope's placeholder did not exit within 10s")
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
        cgroup.cleanup();
        Ok(())
    })();
    if probe.is_err() {
        // No-op once the wrapper has already been reaped by a teardown pass.
        let _ = kill_supervisor_child(&mut supervisor);
    }
    probe
}

/// Resolve every decision and helper for a capped launch without spawning
/// anything; see [`ScopePlan::prepare`].
pub fn prepare_scope_plan(id: Uuid, limits: &Limits) -> Result<Option<ScopePlan>> {
    ScopePlan::prepare(id, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn plan_with_limits(limits: Limits) -> ScopePlan {
        ScopePlan {
            id: Uuid::new_v4(),
            unit: "aplexer-workload-test".into(),
            bus_flag: "--user",
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
            systemctl: PathBuf::from("/usr/bin/systemctl"),
            identity: current_cgroup_identity().unwrap(),
            limits,
        }
    }

    #[test]
    pub(super) fn scope_command_wraps_the_workload_verbatim() {
        let limits = Limits {
            memory_bytes: Some(64 * 1024 * 1024),
            pids: Some(16),
            cpu_quota_us: Some(10_000),
            cpu_period_us: Some(100_000),
        };
        let plan = plan_with_limits(limits);
        let workload = vec![OsString::from("/bin/bash"), OsString::from("-l")];
        let command = plan.scope_command(&workload);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("-- separator");
        assert_eq!(
            &args[..separator],
            &[
                "--user",
                "--scope",
                "--collect",
                "--unit=aplexer-workload-test",
                "-p",
                "Delegate=yes",
                "-p",
                "MemoryMax=67108864",
                "-p",
                "MemorySwapMax=0",
                "-p",
                "TasksMax=16",
                "-p",
                "CPUQuota=10%",
            ],
            "scope properties must stay launch-shaped: {args:?}"
        );
        assert_eq!(&args[separator + 1..], &["/bin/bash", "-l"]);
        assert_eq!(
            command.get_program(),
            "/usr/bin/systemd-run",
            "the trusted helper, not a PATH lookup, must be the program"
        );
    }

    #[test]
    pub(super) fn controller_verification_fails_closed_on_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let limits = Limits {
            memory_bytes: Some(1024),
            pids: None,
            cpu_quota_us: None,
            cpu_period_us: None,
        };
        assert!(
            verify_delegated_controllers(directory.path(), &limits).is_err(),
            "a scope without memory.max must fail a memory-limited launch"
        );
        fs::write(directory.path().join("memory.max"), b"max\n").unwrap();
        assert!(
            verify_delegated_controllers(directory.path(), &limits).is_ok(),
            "with memory.max present the memory limit is enforceable"
        );
    }

    #[test]
    pub(super) fn scope_leader_is_the_smallest_member_pid() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("cgroup.procs"), b"4100 42 7\n").unwrap();
        let leader = scope_leader_pid(directory.path(), "aplexer-workload-test").unwrap();
        assert_eq!(leader, 7, "the placement pid predates any forked member");

        let empty = tempfile::tempdir().unwrap();
        fs::write(empty.path().join("cgroup.procs"), b"\n").unwrap();
        let error = scope_leader_pid(empty.path(), "aplexer-workload-test");
        assert!(
            error.is_err(),
            "an unpopulated scope must not yield a leader pid"
        );
    }
}
