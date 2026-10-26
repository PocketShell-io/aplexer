//! macOS containment: the session's process group is the containment
//! domain. See docs/macos-port.md.
//!
//! macOS has no cgroups, no systemd scopes, and no child subreaper. These
//! shims keep the worker's `Option<Cgroup>` code uniform: on macOS the
//! worker wraps its workload leader's process group in a [`Cgroup`]
//! (`Cgroup::for_leader`) and gets the same `kill_all_until` /
//! `signal_all_until` / `populated` / `cleanup` surface a limited Linux
//! session has. [`ScopePlan`] never plans anything: resource limits fail
//! closed with a clear error instead of running unenforced.
//!
//! Process-group semantics differ from cgroups in two documented ways:
//!
//! * Membership is by `pgid`, not by kernel hierarchy. The workload is
//!   born into its own group (`setsid()` in the spawn `pre_exec`, which
//!   also detaches the controlling terminal), so `killpg` reaches the
//!   leader and every descendant that stayed in the group. A descendant
//!   that calls `setsid()`/`setpgid()` itself leaves the group without
//!   leaving the session -- the same escape hatch unlimited Linux sessions
//!   have, handled by the same libproc descendant-tree teardown.
//! * There is no OOM killer accounting and no freezer: `oom_killed` is
//!   always false and `stats` reports the live group.

use crate::sys::macos;
use crate::{CgroupIdentity, Limits};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A session's containment domain: its workload leader's process group.
#[derive(Debug, Clone)]
pub struct Cgroup {
    pgid: i32,
    locator: PathBuf,
}

impl Cgroup {
    /// Wrap the process group of `leader`. `None` when the leader is
    /// already gone.
    pub fn for_leader(leader: u32) -> Option<Self> {
        let pgid = macos::proc_pgid(leader)? as i32;
        if pgid <= 0 {
            return None;
        }
        Some(Self {
            pgid,
            locator: PathBuf::from(format!("pgid-{pgid}")),
        })
    }

    /// The persisted locator: `pgid-<pgid>`. Never created as a file; the
    /// reap verdict parses it back. The plain `-` separator keeps the
    /// locator a portable filename on every filesystem.
    pub fn locator(&self) -> &Path {
        &self.locator
    }

    /// Kernel identity of the recorded domain: the boot time the group was
    /// observed in. A group recorded under a different boot cannot hold a
    /// live process (pids and pgids do not survive reboot), so a boot
    /// mismatch proves the domain empty the same way the Linux boot-id
    /// comparison does. `cgroup_namespace_device` carries the pgid the
    /// identity was taken for; the remaining coordinate fields have no
    /// macOS meaning and are zero.
    pub fn identity(&self) -> CgroupIdentity {
        CgroupIdentity {
            boot_id: macos::boot_time_us().map(|us| us.to_string()).unwrap_or_default(),
            cgroup_namespace_device: self.pgid as u64,
            cgroup_namespace_inode: 0,
            mount_namespace_device: 0,
            mount_namespace_inode: 0,
            cgroup_mount_id: 0,
            cgroup_root_device: 0,
            cgroup_root_inode: 0,
        }
    }

    /// Parse a persisted `pgid-<pgid>` locator back to its pgid.
    fn parse_locator(locator: &Path) -> Option<i32> {
        locator
            .to_str()?
            .strip_prefix("pgid-")?
            .parse::<i32>()
            .ok()
            .filter(|pgid| *pgid > 0)
    }

    /// Signal every group member. An empty group (`ESRCH`) is success.
    pub fn signal_all_until(&self, signal: i32, _deadline: Instant) -> Result<()> {
        macos::kill_pgid(self.pgid, signal).with_context(|| format!("signal pgid {}", self.pgid))
    }

    /// SIGKILL the group until no live (non-zombie) member remains or
    /// `deadline` passes. Non-children cannot be reaped, so zombies are
    /// excluded from the populated probe (launchd reaps orphaned zombies
    /// promptly); only an error, never a quiet success, on deadline.
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        kill_pgid_until_empty(self.pgid, deadline)
    }

    /// Whether any live, non-zombie process remains in the group. A
    /// machine-wide libproc walk, not `killpg(pgid, 0)`: the signal probe
    /// cannot tell a zombie member from a running one, and a zombie would
    /// read as populated forever.
    pub fn populated(&self) -> Result<bool> {
        Ok(pgid_populated_live(self.pgid))
    }

    /// No OOM accounting on macOS: there is no memory-limit enforcement to
    /// observe (limits fail closed in [`ScopePlan::prepare`]).
    pub fn oom_killed(&self) -> bool {
        false
    }

    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({
            "backend": "pgid",
            "pgid": self.pgid,
            "populated": self.populated().ok(),
        })
    }

    /// Nothing to remove: a process group evaporates with its last member.
    pub fn cleanup(&self) {}
}

/// Never plans a scope on macOS: resource limits have no launchd-backed
/// enforcement wired, so a capped launch fails closed here with a message
/// that names the platform, instead of running without the requested
/// limits. Unlimited launches (`Ok(None)`) spawn directly, exactly like
/// the Linux no-limits path, and the worker still wraps the leader's
/// process group in a [`Cgroup`].
#[derive(Debug, Clone)]
pub struct ScopePlan {
    id: Uuid,
    limits: Limits,
}

impl ScopePlan {
    pub fn prepare(id: Uuid, limits: &Limits) -> Result<Option<Self>> {
        // Same embedding contract as Linux: a host that ignores SIGCHLD
        // could auto-reap the workload before the worker waits on it.
        crate::ensure_sigchld_compatible_for_child_management()?;
        if !limits.requested() {
            return Ok(None);
        }
        bail!(
            "resource limits (memory, pids, cpu) need the Linux cgroup backend; \
             macOS sessions run with process-group containment only, so a capped \
             launch for session {id} is refused rather than run unenforced"
        );
    }

    /// Build the workload command directly: no `systemd-run` wrapper
    /// exists on macOS. Carries the requested limits for the record only;
    /// [`ScopePlan::prepare`] never yields a plan with limits set, so this
    /// stays the unlimited path.
    pub fn scope_command(&self, workload: &[std::ffi::OsString]) -> std::process::Command {
        let program = workload.first().cloned().unwrap_or_default();
        let mut command = std::process::Command::new(program);
        if workload.len() > 1 {
            command.args(&workload[1..]);
        }
        let _ = &self.limits;
        command
    }

    /// The workload leader's process group becomes the containment domain.
    /// The leader's pid is the discovery handle the same way the scope
    /// membership is on Linux.
    pub fn finish_startup(&self, supervisor: &mut std::process::Child) -> Result<(Cgroup, u32)> {
        let leader = supervisor.id();
        let cgroup = Cgroup::for_leader(leader)
            .ok_or_else(|| anyhow::anyhow!("workload leader {leader} exited before containment was recorded"))?;
        Ok((cgroup, leader))
    }

    #[allow(dead_code)]
    pub fn id(&self) -> Uuid {
        self.id
    }
}

/// The doctor's containment probe: process groups are always available, so
/// this always succeeds and simply states the backend. Kept as a function
/// (rather than inlining `Ok(())`) so the probe restructuring that
/// introduced the Linux placeholder scope has a symmetric call site.
pub fn probe_placeholder_scope(_limits: &Limits) -> Result<()> {
    Ok(())
}

/// Resolve the (never limited) plan; see [`ScopePlan::prepare`].
pub fn prepare_scope_plan(id: Uuid, limits: &Limits) -> Result<Option<ScopePlan>> {
    ScopePlan::prepare(id, limits)
}

/// The system-scope escape has no macOS backend: there is no per-user
/// manager exit failure domain to escape from, and no launchd placement
/// story is wired. Always "not requested", so the worker and workload
/// stay in the ambient launch context behind `setsid()`.
pub fn system_scope_escape_decision() -> Result<bool> {
    Ok(false)
}

/// Unreachable through [`system_scope_escape_decision`]: kept so the
/// launch path's Unix shape compiles unchanged.
pub fn wrap_worker_in_system_scope(_id: Uuid, _worker: &mut std::process::Command) -> Result<()> {
    bail!("no system-scope worker backend on macOS (launchd placement is not wired)")
}

/// SIGKILL a group until no live (non-zombie) member remains or `deadline`
/// passes. The populated probe is the zombie-excluding libproc walk, not
/// `killpg(pgid, 0)`, so a zombie member can never pin the loop forever.
fn kill_pgid_until_empty(pgid: i32, deadline: Instant) -> Result<()> {
    loop {
        macos::kill_pgid(pgid, libc::SIGKILL).with_context(|| format!("kill pgid {pgid}"))?;
        if !pgid_populated_live(pgid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out proving pgid {pgid} empty");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether any live, non-zombie process remains in `pgid`.
fn pgid_populated_live(pgid: i32) -> bool {
    macos::all_pids().iter().any(|pid| {
        let info_pgid = macos::proc_pgid(*pid).map(|pgid| pgid as i32);
        info_pgid == Some(pgid) && !macos::proc_is_zombie(*pid)
    })
}

/// Validate and empty a recorded `pgid-<pgid>` domain: the startup-rollback
/// counterpart of [`Cgroup::kill_all_until`]. A locator from a different
/// boot is empty by construction (groups do not survive reboot); an
/// unparseable locator fails closed.
#[allow(clippy::too_many_arguments)]
pub fn cleanup_recorded_cgroup_until(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
    deadline: Instant,
) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("timed out cleaning recorded pgid for session {id}");
    }
    let Some(pgid) = Cgroup::parse_locator(locator) else {
        bail!("session {id} has an unparseable pgid locator {}", locator.display());
    };
    if let Some(identity) = identity {
        let boot = macos::boot_time_us().map(|us| us.to_string()).unwrap_or_default();
        if !identity.boot_id.is_empty() && identity.boot_id != boot {
            return Ok(());
        }
    }
    if signal != libc::SIGKILL {
        // Graceful signal first, mirroring the Linux recovery shape: only
        // the overall deadline (checked by the loops below) can fail here,
        // and expiry escalates to SIGKILL rather than erroring.
        macos::kill_pgid(pgid, signal)
            .with_context(|| format!("signal recorded pgid {pgid}"))?;
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .unwrap_or(deadline)
            .min(deadline);
        while pgid_populated_live(pgid) {
            if Instant::now() >= grace_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    kill_pgid_until_empty(pgid, deadline)
}

/// Recover a recorded `pgid-<pgid>` domain after its worker died: the
/// startup-rollback counterpart shares the deadline-sharing `..._until`
/// form, this is the client-command entry point.
pub fn cleanup_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(grace)
        .and_then(|deadline| deadline.checked_add(Duration::from_secs(2)))
        .ok_or_else(|| anyhow::anyhow!("pgid cleanup deadline overflow"))?;
    cleanup_recorded_cgroup_until(id, locator, identity, signal, grace, deadline)
}

/// Preflight a durable locator before destroying a broken session's worker.
/// Performs no signalling; only establishes that later pgid recovery will
/// operate inside the recorded domain (parseable locator, same boot).
pub fn validate_recorded_cgroup_locator(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<()> {
    let Some(pgid) = Cgroup::parse_locator(locator) else {
        anyhow::bail!(
            "session {id} has an unparseable pgid locator {}",
            locator.display()
        );
    };
    if let Some(identity) = identity {
        let boot = macos::boot_time_us().map(|us| us.to_string()).unwrap_or_default();
        if !identity.boot_id.is_empty() && identity.boot_id != boot {
            anyhow::bail!("session {id} recorded pgid {pgid} under a different boot");
        }
    }
    Ok(())
}

/// Read-only emptiness check for a recorded `pgid-<pgid>` domain: the
/// reap-verdict probe. Signals nothing. A foreign-boot or vanished domain
/// is empty; an unparseable locator fails closed (retain).
pub(crate) fn recorded_pgroup_observed_empty(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<bool> {
    let Some(pgid) = Cgroup::parse_locator(locator) else {
        bail!("session {id} has an unparseable pgid locator {}", locator.display());
    };
    if let Some(identity) = identity {
        let boot = macos::boot_time_us().map(|us| us.to_string()).unwrap_or_default();
        if !identity.boot_id.is_empty() && identity.boot_id != boot {
            return Ok(true);
        }
    }
    Ok(!macos::pgid_populated(pgid)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unlimited() -> Limits {
        Limits {
            memory_bytes: None,
            pids: None,
            cpu_quota_us: None,
            cpu_period_us: None,
        }
    }

    #[test]
    fn unlimited_launch_plans_no_scope() {
        let plan = ScopePlan::prepare(Uuid::new_v4(), &unlimited()).unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn capped_launch_fails_closed() {
        let limited = Limits {
            memory_bytes: Some(64 * 1024 * 1024),
            pids: None,
            cpu_quota_us: None,
            cpu_period_us: None,
        };
        let error = ScopePlan::prepare(Uuid::new_v4(), &limited).unwrap_err();
        assert!(error.to_string().contains("Linux"), "{error:#}");
    }

    #[test]
    fn placeholder_scope_probe_always_passes() {
        probe_placeholder_scope(&unlimited()).unwrap();
    }

    #[test]
    fn leader_group_is_a_live_containment_domain() {
        let me = std::process::id();
        let cgroup = Cgroup::for_leader(me).expect("own process group");
        assert!(cgroup.populated().unwrap());
        assert!(!cgroup.oom_killed());
        let stats = cgroup.stats();
        assert_eq!(stats["backend"], "pgid");
        // A group that cannot exist is empty, never an error.
        assert!(!macos::pgid_populated(1 << 30).unwrap_or(true));
    }

    #[test]
    fn locator_round_trips_and_empty_probes_read_empty() {
        let me = std::process::id();
        let cgroup = Cgroup::for_leader(me).expect("own process group");
        let locator = cgroup.locator().to_path_buf();
        assert!(Cgroup::parse_locator(&locator).is_some());
        // Our own live group is populated, so the verdict retains.
        assert!(!recorded_pgroup_observed_empty(Uuid::new_v4(), &locator, None).unwrap());
        // A foreign boot cannot hold a live group.
        let mut identity = cgroup.identity();
        identity.boot_id = "00000000-0000-0000-0000-000000000000".into();
        assert!(recorded_pgroup_observed_empty(Uuid::new_v4(), &locator, Some(&identity)).unwrap());
        // An unparseable locator fails closed.
        assert!(recorded_pgroup_observed_empty(
            Uuid::new_v4(),
            Path::new("/sys/fs/cgroup/aplexer-workload-x.scope"),
            None
        )
        .is_err());
    }
}
