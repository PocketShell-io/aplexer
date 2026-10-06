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

#[cfg(test)]
mod tests {
    use super::*;

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
