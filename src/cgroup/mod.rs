//! cgroup-v2 containment: identity of the kernel domain a session was
//! created in, trusted-helper system-scope escape, scope creation whose
//! initial process is the workload itself, live probing, and bounded
//! cleanup/kill/reap recovery of
//! recorded cgroups after a worker death.
//!
//! Linux only. On Windows this module exposes just the job-backed
//! `Cgroup`/`ScopePlan` shims in `windows.rs`, so the worker's containment
//! code is written once against one handle type.

#[cfg(target_os = "linux")]
mod identity;
#[cfg(target_os = "linux")]
mod recovery;
#[cfg(target_os = "linux")]
mod scope;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use identity::*;
#[cfg(target_os = "linux")]
pub use recovery::*;
#[cfg(target_os = "linux")]
pub use scope::*;
#[cfg(target_os = "linux")]
pub use systemd::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
pub(crate) const MAX_CGROUP_RECOVERY_MEMBERS: usize = 4096;
#[cfg(target_os = "linux")]
pub(crate) const MAX_CGROUP_PROCS_BYTES: u64 = 128 * 1024;
#[cfg(target_os = "linux")]
pub(crate) const CGROUP_RECOVERY_FD_RESERVE: u64 = 16;

#[cfg(target_os = "linux")]
pub(crate) const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";
#[cfg(target_os = "linux")]
pub(crate) const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
#[cfg(target_os = "linux")]
pub(crate) const TRUSTED_HELPER_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/run/current-system/sw/bin",
];
