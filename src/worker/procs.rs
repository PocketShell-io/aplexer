//! Process-tree operations over /proc: the subreaper flag, descendant
//! enumeration, and descendant teardown.
//!
//! One reason to exist: for an unlimited session the worker's own descendant
//! tree IS the containment boundary, so signalling or killing a workload
//! means walking that tree through pidfd-held handles -- a recycled pid can
//! never be signalled in place of the process the handle was opened for --
//! and reaping adopted strays along the way.

use super::*;

/// Make the worker the reparenting boundary for daemonized workload
/// descendants. This is process-wide on Linux and must happen before the
/// workload is spawned. It does not require systemd or cgroup delegation.
#[cfg(unix)]
pub(super) fn enable_child_subreaper() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error()).context("enable child subreaper");
    }
    Ok(())
}

/// The worker's own direct children, from every thread's `children` file
/// (`pidfd::direct_child_pids_in`).
#[cfg(unix)]
pub(super) fn direct_child_pids(pid: u32) -> Result<Vec<u32>> {
    direct_child_pids_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

/// The /proc root is ignored on Windows (ToolHelp parent links).
#[cfg(windows)]
pub(super) fn direct_child_pids(pid: u32) -> Result<Vec<u32>> {
    direct_child_pids_in(Path::new(""), pid)
}

/// Every process under `root` that can still run code; zombies excluded
/// (see `pidfd::descendant_pids` for why that is what containment needs).
pub(super) fn descendant_pids(root: u32) -> Result<Vec<u32>> {
    crate::pidfd::descendant_pids(root)
}

pub(super) type DescendantHandle = crate::pidfd::PidHandle;

pub(super) fn descendant_handles(root: u32) -> Result<Vec<DescendantHandle>> {
    descendant_pids(root)?
        .into_iter()
        .filter_map(|pid| DescendantHandle::open(pid, None).transpose())
        .collect()
}

#[cfg(unix)]
pub(super) fn signal_descendants(root: u32, signal: i32) -> Result<usize> {
    let handles = descendant_handles(root)?;
    for handle in &handles {
        handle.signal(signal)?;
    }
    Ok(handles.len())
}

/// Repeated scans close the fork-vs-scan race: the first pass stops the
/// parents, and later passes catch children created immediately before the
/// signal arrived. pidfds make every individual signal immune to pid reuse.
#[cfg(unix)]
pub(super) fn kill_descendants(root: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if signal_descendants(root, libc::SIGKILL)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out killing contained workload descendants");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

/// The leader has its own `Child::wait` thread. Only after that waiter has
/// reported completion may the lifecycle thread reap any other child,
/// avoiding a waitpid(-1) race that could steal the leader's exit status.
#[cfg(unix)]
pub(super) fn reap_adopted_children() -> Result<usize> {
    let mut reaped = 0;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid > 0 {
            reaped += 1;
            continue;
        }
        if pid == 0 {
            return Ok(reaped);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(reaped);
        }
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error).context("reap adopted workload descendant");
    }
}

// --- Windows -------------------------------------------------------------------
//
// There is no subreaper, no SIGCHLD and no waitpid: containment is the
// session's Job Object (`sys::windows::job`), registered process-wide with
// `install_session_job` by the worker's startup. The functions below keep the
// Unix names and shapes so the lifecycle/runtime code reads the same.

/// No-op: the Job Object, not reparenting, is the containment boundary.
#[cfg(windows)]
pub(super) fn enable_child_subreaper() -> Result<()> {
    Ok(())
}

/// Deliver a wire signal to the session. Returns how many processes the
/// signal reached: the job's members for KILL, 1 for a graceful Ctrl-C, 0
/// when nothing was left to signal. Unsupported signals are errors.
#[cfg(windows)]
pub(super) fn signal_descendants(root: u32, signal: i32) -> Result<usize> {
    use crate::sys::windows::job::{session_job, KILLED_EXIT_CODE};
    use crate::sys::windows::signal::Signal;

    match Signal::from_wire(signal)? {
        Signal::Probe => Ok(descendant_pids(root)?.len()),
        Signal::Interrupt | Signal::Terminate => {
            let members = descendant_pids(root)?.len();
            if members == 0 {
                return Ok(0);
            }
            crate::sys::windows::signal::write_graceful_input()
                .context("write Ctrl-C to the PTY")?;
            Ok(1)
        }
        Signal::Kill => match session_job() {
            Some(job) => {
                let members = job.process_ids().context("list session job members")?.len();
                if members > 0 {
                    job.terminate(KILLED_EXIT_CODE)
                        .context("terminate session job")?;
                }
                Ok(members)
            }
            None => {
                let handles = descendant_handles(root)?;
                for handle in &handles {
                    handle.signal(signal)?;
                }
                Ok(handles.len())
            }
        },
    }
}

/// Terminate the session job and wait (bounded) until it is observed empty.
#[cfg(windows)]
pub(super) fn kill_descendants(root: u32, timeout: Duration) -> Result<()> {
    use crate::sys::windows::job::session_job;

    let deadline = Instant::now() + timeout;
    if let Some(job) = session_job() {
        return job
            .kill_until_empty(deadline)
            .context("timed out killing contained workload descendants");
    }
    loop {
        if signal_descendants(root, crate::sys::windows::signal::SIGKILL)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out killing contained workload descendants");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

/// No adopted children to reap on Windows: exited processes never linger as
/// zombies and the job's membership already excludes them.
#[cfg(windows)]
pub(super) fn reap_adopted_children() -> Result<usize> {
    Ok(0)
}