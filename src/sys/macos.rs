//! macOS platform seam: libproc/sysctl process inspection and the small
//! POSIX pieces that differ from Linux. See docs/macos-port.md.
//!
//! Linux code stays where it is behind `#[cfg(target_os = "linux")]` (or
//! `#[cfg(unix)]` where macOS shares it: PTY setup, Unix sockets, flock,
//! waitpid, signals). Everything here is implemented against libc's
//! `proc_*` bindings plus `sysctl(2)`, so no new crate dependency is
//! needed. Callers in `process.rs`, `pidfd.rs`, `proc_usage.rs`, and
//! `cgroup/macos.rs` share these rather than declaring their own externs.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// `p_stat` values from `<sys/proc.h>` (`SZOMB`, `SRUN`, ...). libc does
/// not export them, so they are pinned here next to their only reader.
pub const P_STAT_SIDL: u32 = 1;
pub const P_STAT_SRUN: u32 = 2;
pub const P_STAT_SSLEEP: u32 = 3;
pub const P_STAT_SSTOP: u32 = 4;
pub const P_STAT_SZOMB: u32 = 5;

/// One `PROC_PIDTBSDINFO` read: ppid, status, start time, pgid. `None`
/// when the pid is gone (or unreadable), never an error, matching the
/// best-effort contract of every caller.
fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid_i32 = u32_to_pid(pid)?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let read = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if read != std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int {
        return None;
    }
    if info.pbi_pid != pid {
        // Pid recycled between the caller's walk and this read.
        return None;
    }
    Some(info)
}

fn u32_to_pid(pid: u32) -> Option<libc::pid_t> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    Some(pid as libc::pid_t)
}

/// Parent pid, via `PROC_PIDTBSDINFO`. `None` for a missing process.
pub fn proc_ppid(pid: u32) -> Option<u32> {
    bsdinfo(pid).map(|info| info.pbi_ppid)
}

/// Process-group id, via `PROC_PIDTBSDINFO`. `None` for a missing process.
pub fn proc_pgid(pid: u32) -> Option<u32> {
    bsdinfo(pid).map(|info| info.pbi_pgid)
}

/// Whether the process has exited but not been reaped. Thread-group
/// subtleties from the Linux port do not apply: macOS reports the zombie
/// state per process in `pbi_status`, and a zombie holds no subtree (its
/// children were reparented at exit).
pub fn proc_is_zombie(pid: u32) -> bool {
    bsdinfo(pid).is_some_and(|info| info.pbi_status == P_STAT_SZOMB)
}

/// Single-character run state in the `/proc/<pid>/stat` alphabet the rest
/// of the worker reasons in (`R` running, `S` sleeping, `T` stopped,
/// `Z` zombie). `None` for a missing process.
pub fn proc_state_char(pid: u32) -> Option<char> {
    let status = bsdinfo(pid)?.pbi_status;
    Some(match status {
        P_STAT_SIDL => 'I',
        P_STAT_SRUN => 'R',
        P_STAT_SSLEEP => 'S',
        P_STAT_SSTOP => 'T',
        P_STAT_SZOMB => 'Z',
        _ => return None,
    })
}

/// Start time as microseconds since the Unix epoch, from
/// `pbi_start_tvsec/tvusec`. Equality is the only operation callers need
/// (pid-reuse pinning), so the unit differs deliberately from Linux's
/// clock ticks: records never cross operating systems.
pub fn proc_start_time_us(pid: u32) -> Option<u64> {
    let info = bsdinfo(pid)?;
    Some(info.pbi_start_tvsec.saturating_mul(1_000_000).saturating_add(info.pbi_start_tvusec))
}

/// Short executable name (`pbi_name`, up to 2*MAXCOMLEN), for the
/// foreground-command status signal.
pub fn proc_name_of(pid: u32) -> Option<String> {
    let pid_i32 = u32_to_pid(pid)?;
    let mut buffer = [0 as libc::c_char; 64];
    let read = unsafe {
        libc::proc_name(
            pid_i32,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len() as u32,
        )
    };
    if read <= 0 {
        return None;
    }
    let bytes: Vec<u8> = buffer
        .iter()
        .take(read as usize)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8(bytes).ok().filter(|name| !name.is_empty())
}

/// Executable path via `proc_pidpath`, for workspace derivation.
pub fn proc_path_of(pid: u32) -> Option<PathBuf> {
    let pid_i32 = u32_to_pid(pid)?;
    let mut buffer = vec![0 as libc::c_char; libc::PATH_MAX as usize];
    let read = unsafe {
        libc::proc_pidpath(
            pid_i32,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len() as u32,
        )
    };
    if read <= 0 {
        return None;
    }
    let bytes: Vec<u8> = buffer.iter().take(read as usize).map(|c| *c as u8).collect();
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// Every live pid on the machine, via `proc_listallpids`. Callers filter
/// by ppid themselves (see [`direct_children_of`]).
pub fn all_pids() -> Vec<u32> {
    // 32k pids is far above any real machine; a short read means the
    // table fit, a full read means retry once with headroom.
    for capacity in [32768_usize, 262144] {
        let mut buffer = vec![0 as libc::c_int; capacity];
        let bytes = unsafe {
            libc::proc_listallpids(
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                (capacity * std::mem::size_of::<libc::c_int>()) as libc::c_int,
            )
        };
        if bytes <= 0 {
            return Vec::new();
        }
        let count = bytes as usize / std::mem::size_of::<libc::c_int>();
        if count < capacity {
            return buffer[..count].iter().map(|pid| *pid as u32).collect();
        }
    }
    Vec::new()
}

/// Direct children of `ppid`, via `proc_listchildpids`: one syscall, no
/// machine-wide walk, no procfs-text parsing.
pub fn direct_children_of(ppid: u32) -> Vec<u32> {
    let parent = match u32_to_pid(ppid) {
        Some(pid) => pid,
        None => return Vec::new(),
    };
    for capacity in [256_usize, 16384] {
        let mut buffer = vec![0 as libc::c_int; capacity];
        let bytes = unsafe {
            libc::proc_listchildpids(
                parent,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                (capacity * std::mem::size_of::<libc::c_int>()) as libc::c_int,
            )
        };
        if bytes < 0 {
            return Vec::new();
        }
        let count = bytes as usize / std::mem::size_of::<libc::c_int>();
        if count < capacity {
            return buffer[..count].iter().map(|pid| *pid as u32).collect();
        }
    }
    Vec::new()
}

/// `(user_ns, system_ns)` CPU counters for one process, via
/// `PROC_PIDTASKINFO`. `None` for a missing process.
pub fn proc_task_cpu_ns(pid: u32) -> Option<(u64, u64)> {
    let pid_i32 = u32_to_pid(pid)?;
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let read = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast::<libc::c_void>(),
            std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int,
        )
    };
    if read != std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int {
        return None;
    }
    Some((info.pti_total_user, info.pti_total_system))
}

/// Boot time as microseconds since the Unix epoch, via `kern.boottime`.
/// macOS has no `/proc/sys/kernel/random/boot_id`; this is the reboot
/// detector the worker-identity comparison uses instead (same role:
//  a boot that changed proves a recorded pid identity is stale).
pub fn boot_time_us() -> Option<u64> {
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::timeval>();
    let mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let rc = unsafe {
        libc::sysctl(
            mib.as_ptr() as *mut libc::c_int,
            mib.len() as libc::c_uint,
            (&mut boot as *mut libc::timeval).cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(
        (boot.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(boot.tv_usec as u64),
    )
}

/// Raw environment block of a process (`KERN_PROCARGS2`), as NUL-separated
/// bytes. `None` when the process is gone or its arguments are unreadable
/// (another user's process, hardened runtime).
pub fn proc_environ_bytes(pid: u32) -> Option<Vec<u8>> {
    proc_args(pid).map(|args| args.env)
}

/// Argument vector of a process (`KERN_PROCARGS2`), lossy-decoded. `None`
/// when the process is gone or its arguments are unreadable.
pub fn proc_argv_of(pid: u32) -> Option<Vec<String>> {
    proc_args(pid).map(|args| args.argv)
}

struct ProcArgs {
    argv: Vec<String>,
    env: Vec<u8>,
}

fn proc_args(pid: u32) -> Option<ProcArgs> {
    let pid_i32 = u32_to_pid(pid)?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid_i32];
    let mut size: libc::size_t = 0;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 || size > 2 * 1024 * 1024 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buffer.truncate(size);
    // Layout: argc(u32) execpath(NUL-term) padding argv[argc](NUL-term)
    // padding envp(NUL-term...). Skip argc, the exec path, then argc argv
    // entries; the rest is the environment block.
    let mut cursor: usize = 4;
    let take_cstring = |cursor: &mut usize| {
        let start = *cursor;
        while *cursor < buffer.len() && buffer[*cursor] != 0 {
            *cursor += 1;
        }
        if *cursor >= buffer.len() {
            return None;
        }
        let bytes = buffer[start..*cursor].to_vec();
        *cursor += 1;
        Some(bytes)
    };
    take_cstring(&mut cursor)?;
    let argc = u32::from_ne_bytes(buffer[0..4].try_into().ok()?) as usize;
    let mut argv = Vec::with_capacity(argc.min(1024));
    for _ in 0..argc {
        let arg = take_cstring(&mut cursor)?;
        argv.push(String::from_utf8_lossy(&arg).into_owned());
    }
    // Skip padding NULs between argv and envp.
    while cursor < buffer.len() && buffer[cursor] == 0 {
        cursor += 1;
    }
    if cursor >= buffer.len() {
        return None;
    }
    Some(ProcArgs {
        argv,
        env: buffer[cursor..].to_vec(),
    })
}

/// Current working directory of a process, via
/// `PROC_PIDVNODEPATHINFO` (`pvi_cdir`). `None` for a missing process.
pub fn proc_cwd_of(pid: u32) -> Option<PathBuf> {
    let pid_i32 = u32_to_pid(pid)?;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let read = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast::<libc::c_void>(),
            std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
        )
    };
    if read != std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int {
        return None;
    }
    let raw: &[libc::c_char; 1024] = unsafe { std::mem::transmute(&info.pvi_cdir.vip_path) };
    let len = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    let bytes: Vec<u8> = raw[..len].iter().map(|c| *c as u8).collect();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// Look up one `NAME=value` entry in a `KERN_PROCARGS2` environment block.
pub fn environ_lookup(block: &[u8], name: &str) -> Option<String> {
    for entry in block.split(|b| *b == 0) {
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(value) = text.strip_prefix(name) {
            if let Some(value) = value.strip_prefix('=') {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `kill(pgid, 0)` emptiness probe for a process-group containment domain:
/// `Ok(true)` populated, `Ok(false)` gone, `Err` on anything else.
pub fn pgid_populated(pgid: i32) -> io::Result<bool> {
    if pgid <= 0 {
        return Ok(false);
    }
    let rc = unsafe { libc::killpg(pgid, 0) };
    if rc == 0 {
        return Ok(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        // Exists but owned by another user: populated, not empty.
        Some(libc::EPERM) => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Signal every member of a process group. `ESRCH` (empty group) is
/// success; anything else is returned.
pub fn kill_pgid(pgid: i32, signal: libc::c_int) -> io::Result<()> {
    if pgid <= 0 {
        return Ok(());
    }
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}
