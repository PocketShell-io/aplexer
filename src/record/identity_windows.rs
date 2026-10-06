//! Windows worker-start identity persistence and the recorded-worker signal
//! path: pinning "the same Windows process" across pid reuse by
//! `{pid, creation FILETIME}`, and refusing to act on anything less.
//! Counterpart of `identity.rs` (Linux); same crate-visible surface.

use super::SessionRecord;
use crate::persist::TEMP_COUNTER;
use crate::sys::windows::job::{self, IdentityCheck, Job, PinnedProcess};
use crate::sys::windows::signal::Signal;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::Ordering;

pub(crate) use job::ProcessIdentity;

pub(crate) const WORKER_IDENTITY_FILE: &str = "worker.identity.json";

/// How the process at a recorded identity's pid compares with that identity
/// right now.
pub(crate) enum WorkerIdentity {
    /// Same pid, same creation time, still running: the recorded worker.
    Verified,
    /// No live process holds that pid any more.
    Gone,
    /// The pid was recycled by a later process.
    #[cfg_attr(windows, allow(dead_code))]
    PidReused { recorded: u64, current: u64 },
}

/// The one identity comparison behind `worker_alive` and
/// `signal_recorded_worker`. `Err` is "could not tell"; callers decide which
/// way that fails.
pub(crate) fn verify_worker_identity(identity: &ProcessIdentity) -> Result<WorkerIdentity> {
    Ok(match job::verify_identity(identity)? {
        IdentityCheck::Verified => WorkerIdentity::Verified,
        IdentityCheck::Gone => WorkerIdentity::Gone,
        IdentityCheck::Reused { recorded, current } => {
            WorkerIdentity::PidReused { recorded, current }
        }
    })
}

pub(crate) fn read_worker_identity(record: &SessionRecord) -> Result<Option<ProcessIdentity>> {
    let parent = record
        .history_path
        .parent()
        .ok_or_else(|| anyhow!("session {} has no state directory", record.id))?;
    let path = parent.join(WORKER_IDENTITY_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
    };
    if !metadata.file_type().is_file() {
        bail!("untrusted worker identity file {}", path.display());
    }
    let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))
}

/// Capture the worker identity on the first record write that contains a
/// worker pid, exactly as the Linux implementation does: only the process
/// registering itself may create the immutable file.
pub(crate) fn persist_worker_identity_once(path: &Path, value: &Value) -> Result<()> {
    if path.file_name() != Some(OsStr::new("session.json")) {
        return Ok(());
    }
    let Some(pid) = value
        .as_object()
        .and_then(|object| object.get("worker_pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(());
    };
    if pid != std::process::id() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let identity_path = parent.join(WORKER_IDENTITY_FILE);
    if identity_path.try_exists()? {
        return Ok(());
    }

    let identity = job::process_identity(pid)
        .with_context(|| format!("inspect worker pid {pid} before recording its identity"))?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{WORKER_IDENTITY_FILE}.{}.{}.tmp",
        std::process::id(),
        seq
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        serde_json::to_writer(&mut file, &identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);

        // A hard link is an atomic no-replace publication (NTFS). If another
        // writer won the race, retain its earlier identity.
        match fs::hard_link(&temp, &identity_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("publish {}", identity_path.display()));
            }
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

/// Signal the worker recorded for a session only if it is still the exact
/// process registered at startup. The pinned handle keeps the verified
/// process from being replaced between the check and the action.
///
/// Windows supports `0` (probe) and `KILL` here: KILL ends the worker and
/// the session's whole Job Object. Graceful signals travel over the control
/// socket (as Ctrl-C into the PTY), never to a bare worker pid; every other
/// signal is unsupported on Windows.
pub fn signal_recorded_worker(record: &SessionRecord, signal: i32) -> Result<()> {
    let Some(pid) = record.worker_pid else {
        return Ok(());
    };
    let untrusted = || {
        format!(
            "session {} has no trustworthy recorded worker identity; refusing to signal pid {}",
            record.id, pid
        )
    };
    let identity = read_worker_identity(record)
        .with_context(untrusted)?
        .ok_or_else(|| anyhow!(untrusted()))?;
    if identity.pid != pid {
        bail!(
            "session {} recorded worker pid {}, but its identity belongs to pid {}; refusing to signal",
            record.id,
            pid,
            identity.pid
        );
    }
    let signal = Signal::from_wire(signal)?;

    let pinned = match PinnedProcess::open_identity(&identity)
        .with_context(|| format!("open worker pid {pid}"))?
    {
        Ok(pinned) => pinned,
        Err(IdentityCheck::Gone) | Err(IdentityCheck::Verified) => return Ok(()),
        Err(IdentityCheck::Reused { recorded, current }) => bail!(
            "worker pid {} for session {} has been reused (recorded creation {}, current creation {}); refusing to signal",
            pid,
            record.id,
            recorded,
            current
        ),
    };
    match signal {
        Signal::Probe => Ok(()),
        Signal::Kill => {
            pinned
                .terminate(job::KILLED_EXIT_CODE)
                .with_context(|| format!("terminate worker pid {pid}"))?;
            if let Some(session_job) = Job::open(record.id).context("open session job")? {
                session_job
                    .terminate(job::KILLED_EXIT_CODE)
                    .context("terminate session job")?;
            }
            Ok(())
        }
        Signal::Interrupt | Signal::Terminate => bail!(
            "graceful signal {} to worker pid {pid} needs the control socket on Windows",
            signal.wire()
        ),
    }
}
