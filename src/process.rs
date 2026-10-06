//! Process primitives: the SIGCHLD contract for child management,
//! kill-grace validation, `/proc` liveness and identity probing (pid reuse,
//! zombies, boot id), pidfd creation, session-id discovery from ancestor
//! environments, and the PTY/exec helpers used to spawn workers.

#[cfg(unix)]
use anyhow::{anyhow, Context};
use anyhow::{bail, Result};
use std::env;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::fs::{self, File};
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

#[cfg(windows)]
pub use self::windows_impl::*;

/// Long enough for graceful shutdown, but bounded so an authenticated local
/// client cannot monopolize a worker's serialized kill path indefinitely.
pub const MAX_KILL_GRACE_MS: u64 = 30_000;

/// Restore the standalone process contract needed by `std::process::Child`.
/// This changes a process-wide disposition and is therefore reserved for the
/// CLI and worker binaries, never the embeddable Rust/Python API.
#[cfg(unix)]
pub fn normalize_sigchld_for_child_management() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error()).context("restore SIGCHLD default disposition");
        }
    }
    Ok(())
}

/// Validate, without changing it, the embedding process's SIGCHLD contract.
/// Custom handlers remain installed. SIG_IGN and SA_NOCLDWAIT are rejected
/// because either may auto-reap a worker before the API can wait for it.
#[cfg(unix)]
pub fn ensure_sigchld_compatible_for_child_management() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) != 0 {
            return Err(io::Error::last_os_error()).context("inspect SIGCHLD disposition");
        }
        if action.sa_sigaction == libc::SIG_IGN {
            bail!(
                "SIGCHLD disposition is SIG_IGN; in-process session startup requires child wait ownership"
            );
        }
        if action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            bail!(
                "SIGCHLD disposition uses SA_NOCLDWAIT; in-process session startup requires child wait ownership"
            );
        }
    }
    Ok(())
}

pub fn kill_grace_duration(grace_ms: u64) -> Result<Duration> {
    if grace_ms > MAX_KILL_GRACE_MS {
        bail!("kill grace exceeds maximum of {MAX_KILL_GRACE_MS} ms");
    }
    Ok(Duration::from_millis(grace_ms))
}

/// Session identity for `a whoami` / bare `a transcript` / messaging.
///
/// Prefer `APLEXER_SESSION_ID` on this process (the worker stamps it on the
/// workload). If a tool subprocess cleared its environment, walk parent
/// `/proc/<pid>/environ` until we find the stamp -- agent CLIs often spawn
/// `bash`/`env -i` without passing the aplexer vars through.
pub fn discover_session_id() -> Option<Uuid> {
    let direct = parse_session_id_env(env::var_os("APLEXER_SESSION_ID"));
    #[cfg(unix)]
    {
        direct.or_else(session_id_from_ancestor_environ)
    }
    // No portable `/proc/<pid>/environ` equivalent: environment only.
    #[cfg(windows)]
    {
        direct
    }
}

pub(crate) fn parse_session_id_env(raw: Option<std::ffi::OsString>) -> Option<Uuid> {
    let raw = raw?.into_string().ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

#[cfg(unix)]
pub(crate) fn session_id_from_ancestor_environ() -> Option<Uuid> {
    let mut pid = proc_ppid(std::process::id())?;
    for _ in 0..64 {
        if pid == 0 {
            break;
        }
        if let Some(id) = session_id_in_proc_environ(pid) {
            return Some(id);
        }
        let next = proc_ppid(pid)?;
        if next == pid {
            break;
        }
        pid = next;
    }
    None
}

#[cfg(unix)]
pub(crate) fn proc_ppid(pid: u32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(unix)]
pub(crate) fn session_id_in_proc_environ(pid: u32) -> Option<Uuid> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    for entry in bytes.split(|b| *b == 0) {
        let Ok(s) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(val) = s.strip_prefix("APLEXER_SESSION_ID=") {
            if let Ok(id) = val.parse() {
                return Some(id);
            }
        }
    }
    None
}

/// Whether `pid` names a process that can still run code.
///
/// `kill(pid, 0)` alone is NOT that question. It succeeds for a zombie: an
/// exited process whose parent has not yet reaped it still occupies its pid
/// slot and still accepts (and discards) signals. Every liveness decision in
/// aplexer -- `worker_alive`, `workload_leader_alive`, and through them
/// `reap_verdict` and `a prune` -- is really asking "is there anything left
/// that could act", and a zombie's answer is no.
///
/// This matters because aplexer workers are child subreapers
/// (`PR_SET_CHILD_SUBREAPER`), so a session started from inside another
/// aplexer session reparents to that outer worker when its own parent goes
/// away. If the outer worker does not reap it, the dead session's pid stays
/// signalable indefinitely and every probe here reported it alive forever:
/// `a prune` retained records it should have removed, and tests that wait
/// for a pid to die failed with "pid NNNN did not die".
///
/// Uncertainty still fails closed: an unreadable `/proc/<pid>/stat` (a
/// hardened procfs, a racing exit) leaves the answer at the signalable
/// result, so a live process is never mistaken for a dead one.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    if !is_signalable_pid(pid) {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let signalable = rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    signalable && !process_is_zombie(pid)
}

/// The single-character run state from field 3 of `/proc/<pid>/stat`
/// (`R` running, `S`/`D` sleeping, `T` stopped, `Z` zombie, `X` dead).
#[cfg(unix)]
pub fn process_state(pid: u32) -> Result<char> {
    process_state_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

/// `process_state` against an arbitrary `/proc` root, so the zombie rules
/// below can be pinned by ordinary unit tests on a synthetic tree instead of
/// requiring a real process in a specific state -- the same split
/// `direct_child_pids_in` uses.
#[cfg(unix)]
pub(crate) fn process_state_in(proc_root: &Path, pid: u32) -> Result<char> {
    let (stat_path, after_comm) = proc_stat_after_comm(proc_root, pid)?;
    after_comm
        .split_whitespace()
        .next()
        .and_then(|state| state.chars().next())
        .ok_or_else(|| anyhow!("malformed {}", stat_path.display()))
}

/// `<proc_root>/<pid>/stat` from field 3 (the state) onwards, with the
/// file's path for error messages. The parenthesized comm field may itself
/// contain spaces or `)`, so the fields after it are located from its final
/// close-paren, never by a naive whitespace split of the whole line.
#[cfg(unix)]
fn proc_stat_after_comm(proc_root: &Path, pid: u32) -> Result<(PathBuf, String)> {
    let stat_path = proc_root.join(pid.to_string()).join("stat");
    let stat =
        fs::read_to_string(&stat_path).with_context(|| format!("read {}", stat_path.display()))?;
    let after_comm = stat
        .rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .ok_or_else(|| anyhow!("malformed {}", stat_path.display()))?
        .to_owned();
    Ok((stat_path, after_comm))
}

/// Whether `pid` has exited but has not been reaped by its parent.
///
/// The `Z` in `/proc/<pid>/stat` is necessary but not sufficient. A thread
/// group leader that called `pthread_exit` (or bare `exit(2)`) while its
/// sibling threads keep running also reads as `Z`, and that process is very
/// much still executing code -- measured, not assumed: such a leader shows
/// `state=Z` with two entries under `/proc/<pid>/task`. Calling it dead
/// would let a multi-threaded workload be declared contained while its
/// threads ran on. So the thread group must also be down to nothing but the
/// leader's corpse, which is exactly the state `waitpid` will return for.
///
/// Every unreadable answer is reported as "not a zombie": callers use this
/// to subtract the dead from a liveness answer, and an unknown state must
/// never subtract a process that may still be running.
#[cfg(unix)]
pub fn process_is_zombie(pid: u32) -> bool {
    process_is_zombie_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

#[cfg(unix)]
pub(crate) fn process_is_zombie_in(proc_root: &Path, pid: u32) -> bool {
    matches!(process_state_in(proc_root, pid), Ok('Z'))
        && thread_group_holds_only_the_leader(proc_root, pid)
}

/// Whether `<proc>/<pid>/task` contains exactly one entry, i.e. no sibling
/// thread of `pid` is left. Any read failure answers `false`, keeping the
/// caller on the "may still be running" side.
#[cfg(unix)]
pub(crate) fn thread_group_holds_only_the_leader(proc_root: &Path, pid: u32) -> bool {
    let Ok(tasks) = fs::read_dir(proc_root.join(pid.to_string()).join("task")) else {
        return false;
    };
    let mut seen = 0_usize;
    for task in tasks {
        if task.is_err() {
            return false;
        }
        seen += 1;
        if seen > 1 {
            return false;
        }
    }
    seen == 1
}

/// Linux process start time (field 22 of `/proc/<pid>/stat`), measured in
/// clock ticks since boot. Combined with the pid, this distinguishes a
/// persisted process from a later process that reused its numeric pid.
#[cfg(unix)]
pub fn process_start_time_ticks(pid: u32) -> Result<u64> {
    process_start_time_ticks_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

/// `process_start_time_ticks` against an arbitrary `/proc` root, the same
/// split `process_state_in` has, so the field arithmetic can be pinned on a
/// synthetic stat line.
#[cfg(unix)]
pub(crate) fn process_start_time_ticks_in(proc_root: &Path, pid: u32) -> Result<u64> {
    let (stat_path, after_comm) = proc_stat_after_comm(proc_root, pid)?;
    after_comm
        .split_whitespace()
        .nth(19) // field 3 is index 0 here; starttime is field 22
        .ok_or_else(|| anyhow!("{} has no process start time", stat_path.display()))?
        .parse()
        .with_context(|| format!("parse process start time from {}", stat_path.display()))
}

#[cfg(unix)]
pub(crate) fn linux_boot_id() -> Result<String> {
    // Cached: the boot id cannot change without a reboot, and every
    // `worker_alive` probe (i.e. every `a list` row, twice per row in the old
    // plain rendering) read it from disk. Benchmark PLAN P1.1: `a list` with
    // dozens of sessions did dozens of redundant reads of this one tiny
    // file; cache it process-wide after the first successful read.
    static CACHED_BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(cached) = CACHED_BOOT_ID.get() {
        return Ok(cached.clone());
    }
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot identity")?;
    let boot_id = boot_id.trim();
    if boot_id.is_empty() {
        bail!("Linux boot identity is empty");
    }
    let owned = boot_id.to_owned();
    let _ = CACHED_BOOT_ID.set(owned.clone());
    Ok(owned)
}

/// Whether `pid` can name one process to `kill(2)`/`pidfd_open(2)`. Pid 0
/// means "my process group" to `kill`, and anything above `i32::MAX` wraps
/// to a negative `pid_t`, which addresses a whole group (or every process)
/// instead of the persisted pid it came from.
#[cfg(unix)]
fn is_signalable_pid(pid: u32) -> bool {
    pid != 0 && pid <= i32::MAX as u32
}

#[cfg(unix)]
pub(crate) fn pidfd_open(pid: u32) -> io::Result<File> {
    if !is_signalable_pid(pid) {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn command_exists(command: &[String]) -> bool {
    command
        .first()
        .map(|p| executable_available(p))
        .unwrap_or(false)
}

pub fn worker_executable() -> Result<PathBuf> {
    if let Some(path) = env::var_os("APLEXER_WORKER") {
        return Ok(PathBuf::from(path));
    }
    // One executable is both the user-facing CLI and the worker: re-exec
    // this same binary as `<self> worker --id …`. current_exe resolves
    // symlinks, so reaching it through the `a` alias still lands on the
    // real aplexer binary. When this code runs embedded in another program
    // (the Python bindings), the host executable is not aplexer -- fall
    // back to the sibling `aplexer` next to it, then to PATH.
    let current = env::current_exe()?;
    match current.file_name().and_then(|name| name.to_str()) {
        Some("aplexer") | Some("a") => return Ok(current),
        #[cfg(windows)]
        Some("aplexer.exe") | Some("a.exe") => return Ok(current),
        _ => {}
    }
    if let Some(parent) = current.parent() {
        let sibling = parent.join(if cfg!(windows) {
            "aplexer.exe"
        } else {
            "aplexer"
        });
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("aplexer"))
}

#[cfg(unix)]
pub fn set_cloexec(fd: RawFd, enabled: bool) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let next = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
pub fn open_pty(rows: u16, cols: u16) -> Result<(File, File)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error()).context("posix_openpt");
    }
    let cleanup_master = || unsafe {
        libc::close(master);
    };
    if unsafe { libc::grantpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("grantpt");
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("unlockpt");
    }
    // `libc::c_char` is unsigned on some Linux architectures (including
    // aarch64), so keep the buffer's element type aligned with libc rather
    // than assuming x86_64's signed `char`.
    let mut name = vec![0 as libc::c_char; 256];
    if unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("ptsname_r");
    }
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("open PTY slave");
    }
    // The initial size is best-effort; the worker applies the attaching
    // client's real size with `set_winsize` as soon as it knows it.
    let _ = set_winsize(master, rows, cols);
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

#[cfg(unix)]
pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error()).context("TIOCSWINSZ");
    }
    Ok(())
}

/// The name (from `/proc/<pgid>/comm`) of whatever is currently in the
/// foreground of the pty referred to by `fd` -- the same mechanism tmux
/// uses for `pane_current_command`: `tcgetpgrp(fd)` to get the foreground
/// process group of the pty (this updates automatically as the shell
/// forks/foregrounds jobs, standard POSIX job control -- no polling of the
/// workload itself needed), then read that pgid's name straight out of
/// procfs. `comm` is used over parsing `/proc/<pid>/stat`'s second field
/// because it's already a single line stripped of parens and args.
///
/// `fd` need not be `fd`'s own controlling terminal -- this is exactly how
/// tmux's server (which is not part of the pane's session) queries a pty
/// it merely holds the master side of. Best-effort throughout: any failure
/// (no foreground group yet, the process exited between the two syscalls,
/// procfs unmounted) yields `None` rather than an error, since this is a
/// cosmetic status-bar signal, never something worth failing a request or
/// blocking a hot loop over.
#[cfg(unix)]
pub fn foreground_command(fd: RawFd) -> Option<String> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid <= 0 {
        return None;
    }
    let comm = fs::read_to_string(format!("/proc/{pgid}/comm")).ok()?;
    let trimmed = comm.trim_end_matches('\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(unix)]
pub fn peer_uid(fd: RawFd) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok(cred.uid)
}

pub fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
    {
        value.to_owned()
    } else if cfg!(windows) {
        // PowerShell single-quote rule: only `'` needs doubling.
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(unix)]
pub fn c_string(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(unix)]
pub fn executable_available(program: &str) -> bool {
    fn is_executable_file(path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    }

    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate);
    }
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| is_executable_file(&dir.join(program))))
        .unwrap_or(false)
}

/// Windows counterparts of the Linux-only probes above. PTY work lives in
/// `crate::sys::windows::pty` (`PtyMaster`, `spawn_workload`); there is no fd
/// based `open_pty`/`set_winsize`/`foreground_command` on Windows.
#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub use crate::sys::windows::pty::{
        spawn_detached_worker, spawn_workload as spawn_conpty_workload, PtyMaster, Workload,
    };

    /// No SIGCHLD on Windows: nothing to normalize or validate.
    pub fn normalize_sigchld_for_child_management() -> Result<()> {
        Ok(())
    }

    /// No SIGCHLD on Windows: nothing to normalize or validate.
    pub fn ensure_sigchld_compatible_for_child_management() -> Result<()> {
        Ok(())
    }

    /// Whether `pid` names a process that has not exited. Fails closed: when
    /// the process cannot be opened for a reason other than "no such pid"
    /// (access denied) it is reported alive.
    pub fn process_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // ERROR_INVALID_PARAMETER (87): no such process.
                return io::Error::last_os_error().raw_os_error() != Some(87);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            !ok || code == STILL_ACTIVE as u32
        }
    }

    /// No zombies on Windows: an exited process is simply not alive.
    pub fn process_is_zombie(_pid: u32) -> bool {
        false
    }

    /// Process identity token: the creation FILETIME (100 ns ticks since 1601)
    /// from `GetProcessTimes`. With the pid it distinguishes pid reuse.
    pub fn process_start_time_ticks(pid: u32) -> Result<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0;
            let err = io::Error::last_os_error();
            CloseHandle(h);
            if !ok {
                return Err(err.into());
            }
            Ok(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
        }
    }

    /// Boot identity: system boot time (FILETIME) rendered as a string,
    /// from `NtQuerySystemInformation(SystemTimeOfDayInformation)`. Cached.
    pub(crate) fn linux_boot_id() -> Result<String> {
        #[repr(C)]
        struct TimeOfDay {
            boot_time: i64,
            current_time: i64,
            _rest: [u8; 32],
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn NtQuerySystemInformation(class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
        }
        static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        if let Some(c) = CACHED.get() {
            return Ok(c.clone());
        }
        let mut info = TimeOfDay {
            boot_time: 0,
            current_time: 0,
            _rest: [0; 32],
        };
        let mut ret = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                3, // SystemTimeOfDayInformation
                &mut info as *mut _ as *mut u8,
                std::mem::size_of::<TimeOfDay>() as u32,
                &mut ret,
            )
        };
        if status < 0 || info.boot_time == 0 {
            bail!("query Windows boot time failed (NTSTATUS 0x{:08x})", status as u32);
        }
        let id = format!("boot-{}", info.boot_time);
        let _ = CACHED.set(id.clone());
        Ok(id)
    }

    /// Whether `program` (bare name via PATH x PATHEXT, or explicit path)
    /// resolves to an existing file.
    pub fn executable_available(program: &str) -> bool {
        crate::sys::windows::pty::find_executable(program).is_some()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn self_is_alive_with_stable_identity() {
            let pid = std::process::id();
            assert!(process_alive(pid));
            assert!(!process_is_zombie(pid));
            assert_eq!(
                process_start_time_ticks(pid).unwrap(),
                process_start_time_ticks(pid).unwrap()
            );
            assert!(!linux_boot_id().unwrap().is_empty());
        }

        #[test]
        fn cmd_resolves() {
            assert!(executable_available("cmd"));
            assert!(!executable_available("definitely-not-a-program-xyz"));
        }
    }
}
