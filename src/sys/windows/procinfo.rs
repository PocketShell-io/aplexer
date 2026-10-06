//! Process inspection: name/cmdline/environ/cwd/children of a pid, so the
//! /proc readers have a Windows backend. Owner: agent "procinfo".
//!
//! Seam API (all best-effort, `None`/empty on any failure, never panics):
//! - [`normalize_image_name`]`(&str) -> String`: basename, `.exe` stripped, lowercased.
//! - [`image_name`]`(pid) -> Option<String>`: normalized image name.
//! - [`cmdline`]`(pid) -> Option<String>`: raw command line (PEB, x64 targets only).
//! - [`environ`]`(pid) -> Option<Vec<(String, String)>>` / [`environ_var`]`(pid, name)`
//!   (case-insensitive name match, as on Windows).
//! - [`cwd`]`(pid) -> Option<PathBuf>`: current directory (PEB).
//! - [`snapshot`]`() -> Vec<ProcEntry>` and [`children`]`(pid) -> Vec<u32>` (direct, sorted)
//!   via `CreateToolhelp32Snapshot`.
//!
//! PEB reads need `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`, so they
//! work for same-user processes only and not for 32-bit (WOW64) targets.

use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::Wdk::System::Threading::NtQueryInformationProcess;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// Upper bound on any single remote read (environment blocks can be large).
const MAX_READ: usize = 4 * 1024 * 1024;

// x64 layouts (stable since Windows 8; EnvironmentSize since Vista).
const PEB_PROCESS_PARAMETERS: usize = 0x20;
const PARAMS_CURRENT_DIRECTORY: usize = 0x38; // UNICODE_STRING DosPath
const PARAMS_COMMAND_LINE: usize = 0x70; // UNICODE_STRING
const PARAMS_ENVIRONMENT: usize = 0x80; // PVOID
const PARAMS_ENVIRONMENT_SIZE: usize = 0x3F0; // SIZE_T

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// Normalized image name (see [`normalize_image_name`]).
    pub name: String,
}

/// Basename of `image`, with a trailing `.exe` removed (case-insensitive) and
/// lowercased: `C:\Tools\Claude.EXE` -> `claude`.
pub fn normalize_image_name(image: &str) -> String {
    let base = image.rsplit(['\\', '/']).next().unwrap_or(image);
    let lower = base.trim().to_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_owned)
        .unwrap_or(lower)
}

fn wide_to_string(w: &[u16]) -> String {
    OsString::from_wide(w).to_string_lossy().into_owned()
}

fn nul_trimmed(buf: &[u16]) -> &[u16] {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    &buf[..end]
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(pid: u32, access: u32) -> Option<Handle> {
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

/// Whether an anonymous kill-on-close Job Object can be created (what the
/// session containment layer needs). Used by `a doctor`; the probe job is
/// closed immediately.
pub fn job_object_support() -> Result<(), String> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let _guard = Handle(job);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

pub fn snapshot() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return out;
        }
        let _guard = Handle(snap);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            out.push(ProcEntry {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                name: normalize_image_name(&wide_to_string(nul_trimmed(&entry.szExeFile))),
            });
            ok = Process32NextW(snap, &mut entry);
        }
    }
    out
}

/// Direct children of `pid`, ascending. Pid reuse can make a stale parent
/// link look like a child; callers already treat the walk as best-effort.
pub fn children(pid: u32) -> Vec<u32> {
    let mut kids: Vec<u32> = snapshot()
        .into_iter()
        .filter(|e| e.ppid == pid && e.pid != pid)
        .map(|e| e.pid)
        .collect();
    kids.sort_unstable();
    kids
}

pub fn image_name(pid: u32) -> Option<String> {
    if let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut len) };
        if ok != 0 && len > 0 {
            return Some(normalize_image_name(&wide_to_string(&buf[..len as usize])));
        }
    }
    snapshot()
        .into_iter()
        .find(|e| e.pid == pid)
        .map(|e| e.name)
}

fn read_mem(h: HANDLE, addr: usize, len: usize) -> Option<Vec<u8>> {
    if addr == 0 || len > MAX_READ {
        return None;
    }
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            h,
            addr as *const c_void,
            buf.as_mut_ptr().cast(),
            len,
            &mut got,
        )
    };
    if ok == 0 {
        return None;
    }
    buf.truncate(got);
    Some(buf)
}

fn read_usize(h: HANDLE, addr: usize) -> Option<usize> {
    let b = read_mem(h, addr, 8)?;
    Some(usize::from_le_bytes(b.try_into().ok()?))
}

fn read_unicode_string(h: HANDLE, addr: usize) -> Option<Vec<u16>> {
    // UNICODE_STRING { u16 Length; u16 MaximumLength; pad; PWSTR Buffer } (x64)
    let head = read_mem(h, addr, 16)?;
    let len = u16::from_le_bytes([head[0], head[1]]) as usize;
    let buf = usize::from_le_bytes(head[8..16].try_into().ok()?);
    if len == 0 {
        return Some(Vec::new());
    }
    let raw = read_mem(h, buf, len)?;
    Some(
        raw.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

/// Open `pid` and resolve its `RTL_USER_PROCESS_PARAMETERS` address.
/// `None` for 32-bit targets, other users' processes, or non-x64 hosts.
fn with_params<T>(pid: u32, f: impl FnOnce(HANDLE, usize) -> Option<T>) -> Option<T> {
    if !cfg!(target_pointer_width = "64") {
        return None;
    }
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    unsafe {
        // ProcessWow64Information (26): non-zero PEB32 pointer => 32-bit target.
        let mut wow: usize = 0;
        let st = NtQueryInformationProcess(
            h.0,
            26,
            (&mut wow as *mut usize).cast(),
            std::mem::size_of::<usize>() as u32,
            std::ptr::null_mut(),
        );
        if st < 0 || wow != 0 {
            return None;
        }
        // PROCESS_BASIC_INFORMATION (class 0): PebBaseAddress is the 2nd pointer.
        let mut pbi = [0usize; 6];
        let st = NtQueryInformationProcess(
            h.0,
            0,
            pbi.as_mut_ptr().cast(),
            std::mem::size_of_val(&pbi) as u32,
            std::ptr::null_mut(),
        );
        if st < 0 || pbi[1] == 0 {
            return None;
        }
        let params = read_usize(h.0, pbi[1] + PEB_PROCESS_PARAMETERS)?;
        if params == 0 {
            return None;
        }
        f(h.0, params)
    }
}

pub fn cmdline(pid: u32) -> Option<String> {
    with_params(pid, |h, p| {
        read_unicode_string(h, p + PARAMS_COMMAND_LINE).map(|w| wide_to_string(&w))
    })
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    with_params(pid, |h, p| {
        let w = read_unicode_string(h, p + PARAMS_CURRENT_DIRECTORY)?;
        if w.is_empty() {
            return None;
        }
        let s = wide_to_string(&w);
        // DosPath carries a trailing backslash (`C:\dir\`); keep drive roots intact.
        let t = if s.len() > 3 {
            s.trim_end_matches('\\')
        } else {
            s.as_str()
        };
        Some(PathBuf::from(t))
    })
}

pub fn environ(pid: u32) -> Option<Vec<(String, String)>> {
    with_params(pid, |h, p| {
        let env = read_usize(h, p + PARAMS_ENVIRONMENT)?;
        let size = read_usize(h, p + PARAMS_ENVIRONMENT_SIZE)?.min(MAX_READ);
        let raw = read_mem(h, env, size)?;
        let wide: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(parse_env_block(&wide))
    })
}

/// Case-insensitive lookup, matching Windows environment semantics.
pub fn environ_var(pid: u32, name: &str) -> Option<String> {
    environ(pid)?
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// Parse a `K=V\0K=V\0\0` block. Entries starting with `=` (`=C:=C:\dir`) are
/// per-drive cwd bookkeeping, not variables, and are skipped.
pub fn parse_env_block(wide: &[u16]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in wide.split(|&c| c == 0) {
        if entry.is_empty() {
            break;
        }
        let s = wide_to_string(entry);
        if s.starts_with('=') {
            continue;
        }
        if let Some((k, v)) = s.split_once('=') {
            out.push((k.to_owned(), v.to_owned()));
        }
    }
    out
}

/// Process creation time (FILETIME, 100 ns ticks since 1601).
pub fn creation_time(pid: u32) -> Option<u64> {
    crate::sys::windows::job::process_identity(pid)
        .ok()
        .map(|identity| identity.creation_time)
}

/// Parent of `pid` from the ToolHelp entries, or `None` when there is no
/// parent record or the recorded parent pid has been *reused*: a ToolHelp
/// parent link outlives the parent, so a parent that was created after its
/// child cannot be the real one.
fn validated_parent(
    by_pid: &std::collections::HashMap<u32, u32>,
    pid: u32,
    created: &mut impl FnMut(u32) -> Option<u64>,
) -> Option<u32> {
    let ppid = *by_pid.get(&pid)?;
    if ppid == 0 || ppid == pid || !by_pid.contains_key(&ppid) {
        return None;
    }
    match (created(ppid), created(pid)) {
        (Some(parent), Some(child)) if parent > child => None,
        (None, _) => None,
        _ => Some(ppid),
    }
}

/// Ancestor chain of `pid`, nearest first, at most `limit` entries, stopping
/// at the first missing/reused/cyclic link. Does not include `pid`.
pub fn ancestors(pid: u32, limit: usize) -> Vec<u32> {
    let by_pid: std::collections::HashMap<u32, u32> =
        snapshot().into_iter().map(|e| (e.pid, e.ppid)).collect();
    ancestors_in(&by_pid, pid, limit, &mut creation_time)
}

fn ancestors_in(
    by_pid: &std::collections::HashMap<u32, u32>,
    pid: u32,
    limit: usize,
    created: &mut impl FnMut(u32) -> Option<u64>,
) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    while out.len() < limit {
        let Some(parent) = validated_parent(by_pid, cur, created) else {
            break;
        };
        if parent == pid || out.contains(&parent) {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

/// Image names that are console plumbing, never a foreground command.
const CONSOLE_HELPERS: &[&str] = &["conhost", "openconsole"];

/// One live process as the foreground picker sees it.
#[derive(Debug, Clone)]
pub struct FgNode {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

/// Pick the foreground command of a session: the deepest live descendant of
/// `leader` (parent links within `nodes`), console helpers ignored; among
/// equally deep ones the newest by `created`, then the highest pid. Nodes not
/// linked to the leader (their parent exited) are detached background work
/// and only considered when the leader itself is gone.
fn pick_foreground(
    leader: u32,
    nodes: &[FgNode],
    created: &mut impl FnMut(u32) -> Option<u64>,
) -> Option<String> {
    let parent: std::collections::HashMap<u32, u32> =
        nodes.iter().map(|n| (n.pid, n.ppid)).collect();
    let depth_of = |pid: u32| -> Option<usize> {
        let mut cur = pid;
        for depth in 0..=nodes.len() {
            if cur == leader {
                return Some(depth);
            }
            cur = *parent.get(&cur)?;
        }
        None
    };
    let mut best: Vec<(&FgNode, usize)> = Vec::new();
    let mut best_depth = 0usize;
    let leader_alive = parent.contains_key(&leader);
    for node in nodes {
        if CONSOLE_HELPERS.contains(&node.name.as_str()) {
            continue;
        }
        let depth = if leader_alive {
            match depth_of(node.pid) {
                Some(d) => d,
                None => continue,
            }
        } else {
            0
        };
        if best.is_empty() || depth > best_depth {
            best_depth = depth;
            best.clear();
        }
        if depth == best_depth {
            best.push((node, depth));
        }
    }
    best.into_iter()
        .max_by_key(|(n, _)| (created(n.pid).unwrap_or(0), n.pid))
        .map(|(n, _)| n.name.clone())
        .filter(|name| !name.is_empty())
}

/// The foreground command of the session whose workload leader is `leader`.
/// `members` is the session Job's process list when known; with `None` the
/// leader's descendants are taken from the ToolHelp parent links. Returns the
/// normalized image name (`ping`, `node`, `pwsh`). Best-effort, `None` when
/// nothing is alive.
pub fn foreground_command(leader: u32, members: Option<&[u32]>) -> Option<String> {
    let all = snapshot();
    let nodes: Vec<FgNode> = match members {
        Some(pids) => {
            let set: std::collections::HashSet<u32> = pids.iter().copied().collect();
            all.into_iter()
                .filter(|e| set.contains(&e.pid))
                .map(|e| FgNode {
                    pid: e.pid,
                    ppid: e.ppid,
                    name: e.name,
                })
                .collect()
        }
        None => {
            let mut keep = std::collections::HashSet::from([leader]);
            // Entries are not ordered by ancestry: iterate to a fixed point.
            loop {
                let before = keep.len();
                for e in &all {
                    if keep.contains(&e.ppid) {
                        keep.insert(e.pid);
                    }
                }
                if keep.len() == before {
                    break;
                }
            }
            all.into_iter()
                .filter(|e| keep.contains(&e.pid))
                .map(|e| FgNode {
                    pid: e.pid,
                    ppid: e.ppid,
                    name: e.name,
                })
                .collect()
        }
    };
    pick_foreground(leader, &nodes, &mut creation_time)
}

#[link(name = "ntdll")]
extern "system" {
    #[link_name = "NtQuerySystemInformation"]
    fn nt_query_system_information(class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}

/// `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX` (x64 layout, 40 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntryEx {
    object: usize,
    pid: usize,
    handle: usize,
    access: u32,
    backtrace: u16,
    type_index: u16,
    attributes: u32,
    reserved: u32,
}

fn system_handles() -> Option<Vec<HandleEntryEx>> {
    const SYSTEM_EXTENDED_HANDLE_INFORMATION: u32 = 64;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
    let mut size = 4usize << 20;
    for _ in 0..8 {
        let mut buf = vec![0u64; size.div_ceil(8)];
        let mut ret = 0u32;
        let st = unsafe {
            nt_query_system_information(
                SYSTEM_EXTENDED_HANDLE_INFORMATION,
                buf.as_mut_ptr().cast::<u8>(),
                size as u32,
                &mut ret,
            )
        };
        if st == STATUS_INFO_LENGTH_MISMATCH {
            size = (ret as usize).max(size) * 2;
            continue;
        }
        if st < 0 {
            return None;
        }
        let words = buf.as_ptr().cast::<usize>();
        let count = unsafe { *words };
        let header = 2 * std::mem::size_of::<usize>();
        let max = (size - header) / std::mem::size_of::<HandleEntryEx>();
        let count = count.min(max);
        let entries = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr()
                    .cast::<u8>()
                    .add(header)
                    .cast::<HandleEntryEx>(),
                count,
            )
        };
        return Some(entries.to_vec());
    }
    None
}

/// Regular files `pid` currently holds open whose extension is `ext`
/// (without the dot, case-insensitive): the Windows counterpart of scanning
/// `/proc/<pid>/fd`. Uses the system handle table plus `DuplicateHandle`, so
/// it works for same-user processes only (others yield an empty list), and
/// only sees handles open at this instant (Node's append-and-close writers
/// leave nothing to see). Never blocks: only `FILE_TYPE_DISK` handles are
/// resolved to a path.
pub fn open_files_with_ext(pid: u32, ext: &str) -> Vec<PathBuf> {
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileType, GetFinalPathNameByHandleW, FILE_TYPE_DISK,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, PROCESS_DUP_HANDLE};

    let mut out = Vec::new();
    // The object-type index of "File" differs between Windows builds: learn it
    // from a file handle this very process owns (opened before the table is
    // captured, or it would not be in it).
    let Ok(probe) = std::fs::File::open(std::env::current_exe().unwrap_or_default()) else {
        return out;
    };
    let Some(entries) = system_handles() else {
        return out;
    };
    let probe_handle = std::os::windows::io::AsRawHandle::as_raw_handle(&probe) as usize;
    let me = std::process::id() as usize;
    let Some(file_type) = entries
        .iter()
        .find(|e| e.pid == me && e.handle == probe_handle)
        .map(|e| e.type_index)
    else {
        return out;
    };
    let Some(target) = open(pid, PROCESS_DUP_HANDLE) else {
        return out;
    };
    let mut inspected = 0usize;
    for e in entries
        .iter()
        .filter(|e| e.pid == pid as usize && e.type_index == file_type)
    {
        inspected += 1;
        if inspected > 4096 {
            break;
        }
        let mut dup: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            DuplicateHandle(
                target.0,
                e.handle as HANDLE,
                GetCurrentProcess(),
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 || dup.is_null() {
            continue;
        }
        let dup = Handle(dup);
        if unsafe { GetFileType(dup.0) } != FILE_TYPE_DISK {
            continue;
        }
        let mut buf = vec![0u16; 4096];
        let n = unsafe { GetFinalPathNameByHandleW(dup.0, buf.as_mut_ptr(), buf.len() as u32, 0) };
        if n == 0 || n as usize > buf.len() {
            continue;
        }
        let text = wide_to_string(&buf[..n as usize]);
        let text = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned))
            .unwrap_or(text);
        let path = PathBuf::from(text);
        if path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case(ext))
        {
            out.push(path);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(pid: u32, ppid: u32, name: &str) -> FgNode {
        FgNode {
            pid,
            ppid,
            name: name.into(),
        }
    }

    #[test]
    fn foreground_prefers_deepest_then_newest_and_skips_helpers() {
        let nodes = [
            node(10, 1, "pwsh"),
            node(11, 10, "conhost"),
            node(12, 10, "cmd"),
            node(13, 12, "node"),
            node(14, 12, "ping"),
            node(99, 77, "orphan"),
        ];
        let mut created = |pid: u32| Some(u64::from(pid));
        // 14 is newer than 13 at the same depth; the orphan never wins.
        assert_eq!(
            pick_foreground(10, &nodes, &mut created).as_deref(),
            Some("ping")
        );
        assert_eq!(
            pick_foreground(10, &nodes[..2], &mut created).as_deref(),
            Some("pwsh")
        );
        // Leader gone: fall back to whatever lives.
        assert!(pick_foreground(5, &nodes[3..4], &mut created).is_some());
        assert_eq!(pick_foreground(10, &[], &mut created), None);
    }

    #[test]
    fn ancestors_stop_at_reused_parent_pids() {
        let by_pid: std::collections::HashMap<u32, u32> =
            [(5, 4), (4, 3), (3, 2), (2, 0)].into_iter().collect();
        // Lower pid == created earlier: parents predate their children.
        let mut ok = |pid: u32| Some(u64::from(pid));
        assert_eq!(ancestors_in(&by_pid, 5, 10, &mut ok), vec![4, 3, 2]);
        // pid 3 "was created after" its child 4: a recycled pid.
        let mut reused = |pid: u32| Some(if pid == 3 { 500 } else { u64::from(pid) });
        assert_eq!(ancestors_in(&by_pid, 5, 10, &mut reused), vec![4]);
        assert_eq!(ancestors_in(&by_pid, 5, 2, &mut ok), vec![4, 3]);
    }

    #[test]
    fn ancestors_of_a_real_child_include_us() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 4 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let chain = ancestors(child.id(), 8);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(chain.first(), Some(&std::process::id()));
    }

    #[test]
    fn foreground_of_a_live_tree_is_the_leaf() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 6 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let mut seen = None;
        for _ in 0..40 {
            seen = foreground_command(child.id(), None);
            if seen.as_deref() == Some("ping") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(seen.as_deref(), Some("ping"));
    }

    #[test]
    fn inaccessible_and_missing_processes_degrade_to_none() {
        // pid 4 is the System process (protected); 0xFFFF_FFF0 does not exist.
        for pid in [0, 4, 0xFFFF_FFF0] {
            assert!(environ(pid).is_none(), "environ {pid}");
            assert!(cmdline(pid).is_none(), "cmdline {pid}");
            assert!(cwd(pid).is_none(), "cwd {pid}");
            assert!(open_files_with_ext(pid, "jsonl").is_empty());
            assert!(ancestors(pid, 8).len() <= 8);
        }
        assert_eq!(foreground_command(0xFFFF_FFF0, None), None);
    }

    #[test]
    fn open_files_sees_a_held_file_of_another_process() {
        // This process holds the file; query ourselves by pid.
        let dir = std::env::temp_dir().join(format!("aplexer-openfiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("held.jsonl");
        let held = std::fs::File::create(&path).unwrap();
        let found = open_files_with_ext(std::process::id(), "JSONL");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            found.iter().any(|p| p.ends_with("held.jsonl")),
            "found {found:?}"
        );
    }

    #[test]
    fn normalizes_image_names() {
        assert_eq!(normalize_image_name("Claude.EXE"), "claude");
        assert_eq!(normalize_image_name(r"C:\Tools\pwsh.exe"), "pwsh");
        assert_eq!(normalize_image_name("node"), "node");
    }

    #[test]
    fn parses_env_block() {
        let w: Vec<u16> = "A=1\0=C:=C:\\x\0B=two=2\0\0".encode_utf16().collect();
        assert_eq!(
            parse_env_block(&w),
            vec![("A".into(), "1".into()), ("B".into(), "two=2".into())]
        );
    }

    #[test]
    fn inspects_self() {
        let me = std::process::id();
        assert!(image_name(me).is_some());
        let cl = cmdline(me).expect("own cmdline");
        assert!(!cl.is_empty());
        assert_eq!(cwd(me).unwrap(), std::env::current_dir().unwrap());
        assert!(environ_var(me, "path").is_some());
        assert!(snapshot().iter().any(|e| e.pid == me));
    }

    #[test]
    fn job_objects_supported() {
        job_object_support().unwrap();
    }

    #[test]
    fn finds_children() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let kids = children(std::process::id());
        let found = kids.contains(&child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(found);
    }
}
