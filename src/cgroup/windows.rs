//! Windows has no cgroups: the session's Job Object is the containment
//! domain. These shims keep the worker's `Option<Cgroup>` code uniform: on
//! Windows the worker wraps its session job in a [`Cgroup`] (`Cgroup::new`)
//! and gets the same `kill_all_until` / `signal_all_until` / `populated` /
//! `cleanup` surface a limited Linux session has. [`ScopePlan`] never plans
//! anything: resource limits are applied to the job directly
//! (`sys::windows::job::JobLimits`).

use crate::sys::windows::job::{Job, KILLED_EXIT_CODE};
use crate::sys::windows::signal::{deliver_installed, Signal};
use crate::Limits;
use anyhow::{Context, Result};
use std::time::Instant;
use uuid::Uuid;

/// A session's containment domain: its Job Object.
#[derive(Debug, Clone)]
pub struct Cgroup {
    job: Job,
}

impl Cgroup {
    pub fn new(job: Job) -> Self {
        Self { job }
    }

    pub fn job(&self) -> &Job {
        &self.job
    }

    /// Windows has no cgroup locator; the job name is derived from the
    /// session id, so there is nothing to record.
    pub fn name(&self) -> &str {
        self.job.name()
    }

    /// Deliver a wire signal: INT/TERM write Ctrl-C through the installed PTY
    /// writer, KILL terminates the job, anything else is rejected.
    pub fn signal_all_until(&self, signal: i32, deadline: Instant) -> Result<()> {
        match Signal::from_wire(signal)? {
            Signal::Kill => self.kill_all_until(deadline),
            _ => deliver_installed(signal, &self.job).context("signal session job"),
        }
    }

    /// Terminate every member and wait until the job is observed empty.
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        self.job
            .kill_until_empty(deadline)
            .context("kill containment job")
    }

    pub fn populated(&self) -> Result<bool> {
        Ok(!self.job.is_empty().context("inspect containment job")?)
    }

    /// A process was ended for exceeding the job memory limit (the limit
    /// listener kills the offender with `STATUS_COMMITMENT_LIMIT`).
    pub fn oom_killed(&self) -> bool {
        self.job.oom_kills() > 0
    }

    pub fn stats(&self) -> serde_json::Value {
        let accounting = self.job.accounting().ok();
        let limits = self.job.limits();
        serde_json::json!({
            "memory_limit_bytes": limits.memory_bytes,
            "pids_limit": limits.pids,
            "cpu_quota_us": limits.cpu_quota_us,
            "cpu_period_us": limits.cpu_quota_us.map(|_| limits.cpu_period_us.unwrap_or(100_000)),
            "oom_kill_count": self.job.oom_kills(),
            "oom_kill_count_since_start": self.job.oom_kills(),
            "pid_limit_hits": self.job.pid_limit_hits(),
            "populated": self.populated().ok(),
            "job": self.job.name(),
            "active_processes": accounting.map(|a| a.active_processes),
            "cpu_time_100ns": accounting.map(|a| a.cpu_time_100ns),
            "peak_memory_bytes": self.job.peak_memory_bytes().ok(),
        })
    }

    /// Dropping the last handle closes the job (`KILL_ON_JOB_CLOSE`); nothing
    /// else to remove.
    pub fn cleanup(&self) {}
}

/// Never plans a scope on Windows.
#[derive(Debug, Clone)]
pub struct ScopePlan;

impl ScopePlan {
    pub fn prepare(_id: Uuid, _limits: &Limits) -> Result<Option<Self>> {
        Ok(None)
    }
}

/// Terminate-all exit code, re-exported for callers that terminate a bare job.
#[allow(dead_code)]
pub const JOB_KILL_EXIT_CODE: u32 = KILLED_EXIT_CODE;
