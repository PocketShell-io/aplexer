//! ConPTY: open/resize/close pseudoconsole, raw CreateProcessW with
//! PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, detached worker spawn. Owner: agent "conpty".
//!
//! Depends only on `std` and `windows-sys` so it can be built standalone.
//!
//! # Seam API
//! * [`PtyMaster::open(rows, cols)`] creates a pseudoconsole. It is cheap to
//!   [`Clone`] (an `Arc`); all clones share one HPCON.
//!   * [`PtyMaster::reader`] / [`PtyMaster::writer`] hand out independent
//!     `std::fs::File`s (duplicated handles): the output stream (VT bytes from
//!     the workload) and the input stream (bytes typed at the workload).
//!   * [`PtyMaster::resize`] = `ResizePseudoConsole`.
//!   * [`PtyMaster::close`] closes the pseudoconsole without ever blocking the
//!     caller. **ConPTY never reports EOF on the output pipe until the
//!     pseudoconsole is closed**, so when the workload exits the owner must
//!     call `close()` for the reader thread to see EOF. Closing blocks inside
//!     conhost until the output pipe is drained, so `close()` runs
//!     `ClosePseudoConsole` on a helper thread and relies on the existing reader
//!     to drain; [`PtyMaster::close_and_drain`] instead drains in the calling
//!     thread and returns the trailing bytes (use it when no reader is running).
//!     `Drop` of the last clone does `close_and_drain`.
//! * [`spawn_workload`] (argv, cwd, env overrides, optional pty, optional job
//!   `HANDLE`) -> [`Workload`] with `pid()`, `try_wait()`, `wait()`,
//!   `wait_timeout()`, `terminate()`, `raw_handle()`, `try_clone()`. With a job
//!   handle the process is created suspended, assigned, then resumed, so no
//!   child can escape before containment. Exit codes are raw `u32` (no signals).
//! * [`spawn_detached_worker`] runs a `std::process::Command` with
//!   `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` (+ breakaway
//!   from the caller's job when permitted) and returns the `std` `Child`.
//! * [`find_executable`] resolves a program name via PATH x PATHEXT.

use std::collections::BTreeMap;
use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

fn hresult_err(hr: i32, what: &str) -> io::Error {
    io::Error::other(format!("{what} failed: HRESULT 0x{:08x}", hr as u32))
}

fn last_err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

fn coord(rows: u16, cols: u16) -> COORD {
    COORD {
        X: cols.max(1).min(i16::MAX as u16) as i16,
        Y: rows.max(1).min(i16::MAX as u16) as i16,
    }
}

// ---------------------------------------------------------------------------
// PtyMaster
// ---------------------------------------------------------------------------

struct PtyInner {
    /// `HPCON` stored as an integer so the struct is `Send + Sync`.
    hpc: Mutex<Option<isize>>,
    /// Write end of the conpty input pipe (we type into it).
    input: File,
    /// Read end of the conpty output pipe (VT stream).
    output: File,
}

impl PtyInner {
    fn take_hpc(&self) -> Option<isize> {
        self.hpc.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for PtyInner {
    fn drop(&mut self) {
        if let Some(hpc) = self.take_hpc() {
            let _ = close_and_drain_raw(hpc, &self.output);
        }
    }
}

/// Master side of a pseudoconsole. See the module docs.
#[derive(Clone)]
pub struct PtyMaster(Arc<PtyInner>);

impl PtyMaster {
    /// Create a pseudoconsole of the given size.
    pub fn open(rows: u16, cols: u16) -> io::Result<PtyMaster> {
        unsafe {
            let (mut in_read, mut in_write): (HANDLE, HANDLE) = (null_mut(), null_mut());
            let (mut out_read, mut out_write): (HANDLE, HANDLE) = (null_mut(), null_mut());
            if CreatePipe(&mut in_read, &mut in_write, null(), 0) == 0 {
                return Err(last_err("CreatePipe(input)"));
            }
            if CreatePipe(&mut out_read, &mut out_write, null(), 0) == 0 {
                let e = last_err("CreatePipe(output)");
                CloseHandle(in_read);
                CloseHandle(in_write);
                return Err(e);
            }
            let mut hpc: HPCON = zeroed();
            let hr = CreatePseudoConsole(coord(rows, cols), in_read, out_write, 0, &mut hpc);
            // The pseudoconsole duplicated the ends it needs.
            CloseHandle(in_read);
            CloseHandle(out_write);
            if hr < 0 {
                CloseHandle(in_write);
                CloseHandle(out_read);
                return Err(hresult_err(hr, "CreatePseudoConsole"));
            }
            Ok(PtyMaster(Arc::new(PtyInner {
                hpc: Mutex::new(Some(hpc as isize)),
                input: File::from_raw_handle(in_write as RawHandle),
                output: File::from_raw_handle(out_read as RawHandle),
            })))
        }
    }

    /// An independent handle to the output stream (workload -> us).
    pub fn reader(&self) -> io::Result<File> {
        self.0.output.try_clone()
    }

    /// An independent handle to the input stream (us -> workload).
    pub fn writer(&self) -> io::Result<File> {
        self.0.input.try_clone()
    }

    /// Apply a new size (`ResizePseudoConsole`).
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let guard = self
            .0
            .hpc
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?;
        let hpc = guard
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pseudoconsole is closed"))?;
        let hr = unsafe { ResizePseudoConsole(hpc as HPCON, coord(rows, cols)) };
        if hr < 0 {
            return Err(hresult_err(hr, "ResizePseudoConsole"));
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.0.hpc.lock().map(|g| g.is_none()).unwrap_or(true)
    }

    fn raw_hpc(&self) -> io::Result<isize> {
        self.0
            .hpc
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pseudoconsole is closed"))
    }

    /// Close the pseudoconsole without blocking. The caller's reader must keep
    /// draining; it sees EOF once conhost has flushed and exited. Idempotent.
    pub fn close(&self) {
        if let Some(hpc) = self.0.take_hpc() {
            // ClosePseudoConsole can block until the output pipe is drained
            // (pre-24H2 conhost), so never run it on the caller's thread.
            let _ = std::thread::Builder::new()
                .name("aplexer-conpty-close".into())
                .spawn(move || unsafe { ClosePseudoConsole(hpc as HPCON) });
        }
    }

    /// Close the pseudoconsole while draining the output pipe in this thread;
    /// returns the bytes that were still pending (capped at 1 MiB). Use when
    /// no other thread is reading. Idempotent (second call returns empty).
    pub fn close_and_drain(&self) -> Vec<u8> {
        match self.0.take_hpc() {
            Some(hpc) => close_and_drain_raw(hpc, &self.0.output),
            None => Vec::new(),
        }
    }
}

fn close_and_drain_raw(hpc: isize, output: &File) -> Vec<u8> {
    const CAP: usize = 1 << 20;
    let Ok(mut drain) = output.try_clone() else {
        // Cannot drain: fall back to the non-blocking close.
        let _ = std::thread::spawn(move || unsafe { ClosePseudoConsole(hpc as HPCON) });
        return Vec::new();
    };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let drainer = std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match drain.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tail.len() < CAP {
                        tail.extend_from_slice(&buf[..n.min(CAP - tail.len())]);
                    }
                }
            }
        }
        let _ = tx.send(tail);
    });
    let closer = std::thread::spawn(move || unsafe { ClosePseudoConsole(hpc as HPCON) });
    // Both finish once conhost has exited. Bound the wait: a stuck conhost
    // must not wedge the worker's shutdown.
    let tail = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    if closer.is_finished() {
        let _ = closer.join();
    }
    if drainer.is_finished() {
        let _ = drainer.join();
    }
    tail
}

// ---------------------------------------------------------------------------
// Workload
// ---------------------------------------------------------------------------

/// A spawned process (the PTY workload leader or a plain process).
pub struct Workload {
    pid: u32,
    process: OwnedHandle,
}

impl Workload {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The process handle (valid while `self` lives). Waitable.
    pub fn raw_handle(&self) -> HANDLE {
        self.process.as_raw_handle() as HANDLE
    }

    /// A second owner of the same process handle (e.g. one for the waiter
    /// thread, one for the control path).
    pub fn try_clone(&self) -> io::Result<Workload> {
        Ok(Workload {
            pid: self.pid,
            process: self.process.try_clone()?,
        })
    }

    /// `Ok(Some(code))` once exited, `Ok(None)` while running.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        self.wait_ms(0)
    }

    /// Block until exit; returns the exit code.
    pub fn wait(&self) -> io::Result<u32> {
        Ok(self.wait_ms(INFINITE)?.expect("infinite wait returned"))
    }

    /// Wait up to `timeout`; `None` on timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<u32>> {
        self.wait_ms(timeout.as_millis().min(u32::MAX as u128 - 1) as u32)
    }

    fn wait_ms(&self, ms: u32) -> io::Result<Option<u32>> {
        unsafe {
            match WaitForSingleObject(self.raw_handle(), ms) {
                WAIT_OBJECT_0 => {
                    let mut code = 0u32;
                    if GetExitCodeProcess(self.raw_handle(), &mut code) == 0 {
                        return Err(last_err("GetExitCodeProcess"));
                    }
                    Ok(Some(code))
                }
                WAIT_TIMEOUT => Ok(None),
                _ => Err(last_err("WaitForSingleObject")),
            }
        }
    }

    /// `TerminateProcess` on this process only (use the job for the tree).
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        if unsafe { TerminateProcess(self.raw_handle(), exit_code) } == 0 {
            let e = io::Error::last_os_error();
            // Already gone is success.
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Command line / environment / program resolution
// ---------------------------------------------------------------------------

fn append_arg(out: &mut Vec<u16>, arg: &OsStr) {
    let wide: Vec<u16> = arg.encode_wide().collect();
    let quote = wide.is_empty()
        || wide
            .iter()
            .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'\n' as u16 || c == 0x0b);
    if quote {
        out.push(b'"' as u16);
    }
    let mut backslashes = 0usize;
    for &c in &wide {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else {
            if c == b'"' as u16 {
                out.extend(std::iter::repeat_n(b'\\' as u16, backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    if quote {
        out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        out.push(b'"' as u16);
    }
}

/// Build a CreateProcessW command line from argv with the standard
/// `CommandLineToArgvW`-compatible quoting rules.
pub fn build_command_line<S: AsRef<OsStr>>(argv: &[S]) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            out.push(b' ' as u16);
        }
        append_arg(&mut out, a.as_ref());
    }
    out.push(0);
    out
}

/// Overrides applied on top of the current process environment: `Some` sets
/// (case-insensitively replacing), `None` removes.
pub type EnvOverrides = BTreeMap<OsString, Option<OsString>>;

fn upper(s: &OsStr) -> String {
    s.to_string_lossy().to_uppercase()
}

fn effective_env(overrides: &EnvOverrides) -> Vec<(OsString, OsString)> {
    let mut map: BTreeMap<String, (OsString, OsString)> = BTreeMap::new();
    for (k, v) in std::env::vars_os() {
        map.insert(upper(&k), (k, v));
    }
    for (k, v) in overrides {
        match v {
            Some(v) => {
                map.insert(upper(k), (k.clone(), v.clone()));
            }
            None => {
                map.remove(&upper(k));
            }
        }
    }
    map.into_values().collect()
}

fn env_block(vars: &[(OsString, OsString)]) -> Vec<u16> {
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn wide_z(s: &OsStr) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}

fn pathext() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    raw.split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn with_ext_candidates(base: &Path, exts: &[String]) -> Vec<PathBuf> {
    let mut v = vec![base.to_path_buf()];
    for e in exts {
        let mut s = base.as_os_str().to_os_string();
        s.push(e);
        v.push(PathBuf::from(s));
    }
    v
}

fn find_executable_with_path(program: &OsStr, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let exts = pathext();
    let p = Path::new(program);
    let has_sep = program
        .to_string_lossy()
        .chars()
        .any(|c| c == '\\' || c == '/' || c == ':');
    if has_sep {
        return with_ext_candidates(p, &exts)
            .into_iter()
            .find(|c| c.is_file());
    }
    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        for cand in with_ext_candidates(&dir.join(p), &exts) {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Resolve `program` like a shell would: explicit paths as-is, bare names
/// through `PATH` with `PATHEXT` (the current directory is not searched).
pub fn find_executable(program: &str) -> Option<PathBuf> {
    find_executable_with_path(OsStr::new(program), std::env::var_os("PATH").as_deref())
}

// ---------------------------------------------------------------------------
// spawn_workload
// ---------------------------------------------------------------------------

struct AttrList {
    _buf: Vec<usize>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttrList {
    fn new(count: u32) -> io::Result<AttrList> {
        unsafe {
            let mut size = 0usize;
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size);
            let mut buf = vec![0usize; size.div_ceil(size_of::<usize>()).max(1)];
            let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(list, count, 0, &mut size) == 0 {
                return Err(last_err("InitializeProcThreadAttributeList"));
            }
            Ok(AttrList { _buf: buf, list })
        }
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

/// Spawn `argv` in `cwd`.
///
/// * `env`: overrides applied over this process's environment (`None` removes).
/// * `pty`: attach the process to this pseudoconsole (stdio = the conpty).
///   Without it stdio is NUL and no console window is created.
/// * `job`: a raw Job Object `HANDLE` owned by the caller. The process is
///   created suspended, assigned to the job, then resumed; on assignment
///   failure it is terminated and the error returned.
pub fn spawn_workload<S: AsRef<OsStr>>(
    argv: &[S],
    cwd: &Path,
    env: &EnvOverrides,
    pty: Option<&PtyMaster>,
    job: Option<HANDLE>,
) -> io::Result<Workload> {
    let first = argv
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let vars = effective_env(env);
    let path_var = vars
        .iter()
        .find(|(k, _)| upper(k) == "PATH")
        .map(|(_, v)| v.clone());
    let app = find_executable_with_path(first.as_ref(), path_var.as_deref()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("program not found: {}", first.as_ref().to_string_lossy()),
        )
    })?;
    let app_w = wide_z(app.as_os_str());
    let mut cmdline = build_command_line(argv);
    let envblock = env_block(&vars);
    let cwd_w = wide_z(cwd.as_os_str());

    unsafe {
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
        if job.is_some() {
            flags |= CREATE_SUSPENDED;
        }
        let attrs = AttrList::new(1)?;
        // Keep the NUL handle alive until CreateProcessW returns.
        let nul;
        let mut nul_h: [HANDLE; 1] = [null_mut()];
        let inherit;
        match pty {
            Some(pty) => {
                let hpc = pty.raw_hpc()?;
                // The attribute value is the HPCON itself, not a pointer to it.
                if UpdateProcThreadAttribute(
                    attrs.list,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    hpc as *const c_void,
                    size_of::<HPCON>(),
                    null_mut(),
                    null(),
                ) == 0
                {
                    return Err(last_err("UpdateProcThreadAttribute(PSEUDOCONSOLE)"));
                }
                // Without this, a creator whose own std handles are redirected
                // (pipes under a service, a test harness, `a start` in a
                // script) leaks them into the child, which then writes to
                // them instead of the pseudoconsole. INVALID_HANDLE_VALUE
                // forces the child onto the conpty (same as wezterm/portable-pty).
                si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
                si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
                si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
                inherit = 0;
            }
            None => {
                nul = File::options().read(true).write(true).open("NUL")?;
                let h = nul.as_raw_handle() as HANDLE;
                if SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                    return Err(last_err("SetHandleInformation"));
                }
                nul_h[0] = h;
                if UpdateProcThreadAttribute(
                    attrs.list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    nul_h.as_ptr() as *const c_void,
                    size_of::<HANDLE>(),
                    null_mut(),
                    null(),
                ) == 0
                {
                    return Err(last_err("UpdateProcThreadAttribute(HANDLE_LIST)"));
                }
                si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = h;
                si.StartupInfo.hStdOutput = h;
                si.StartupInfo.hStdError = h;
                flags |= CREATE_NO_WINDOW;
                inherit = 1;
            }
        }
        si.lpAttributeList = attrs.list;

        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            inherit,
            flags,
            envblock.as_ptr() as *const c_void,
            cwd_w.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        );
        if ok == 0 {
            return Err(last_err(&format!("CreateProcessW({})", app.display())));
        }
        let process = OwnedHandle::from_raw_handle(pi.hProcess as RawHandle);
        let thread = OwnedHandle::from_raw_handle(pi.hThread as RawHandle);
        let workload = Workload {
            pid: pi.dwProcessId,
            process,
        };
        if let Some(job) = job {
            if AssignProcessToJobObject(job, workload.raw_handle()) == 0 {
                let e = last_err("AssignProcessToJobObject");
                let _ = workload.terminate(1);
                return Err(e);
            }
            if ResumeThread(thread.as_raw_handle() as HANDLE) == u32::MAX {
                let e = last_err("ResumeThread");
                let _ = workload.terminate(1);
                return Err(e);
            }
        }
        drop(thread);
        Ok(workload)
    }
}

// ---------------------------------------------------------------------------
// Detached worker
// ---------------------------------------------------------------------------

/// Spawn the (already configured: args, env, stdio -> NUL / worker.log) worker
/// fully detached from the launching console and process group. Tries
/// `CREATE_BREAKAWAY_FROM_JOB` first so the worker survives the launcher's
/// job being closed; if the job forbids breakaway, retries without it.
///
/// std::process::Command always creates with InheritHandles = TRUE, so the
/// launcher's own inheritable std handles (a pipe from  start | ..., a CI
/// log capture) would leak into the long-lived worker and keep the reader
/// waiting for EOF until the session ends. Their inherit flag is cleared for
/// the duration of the spawn; the worker's own stdio is duplicated by std
/// from the Stdio values and is unaffected.
pub fn spawn_detached_worker(command: &mut Command) -> io::Result<Child> {
    let _guard = NoStdHandleInherit::new();
    // No CREATE_NEW_PROCESS_GROUP: it would disable Ctrl-C for the worker, and
    // that state is inherited by every workload, so the graceful signal (a
    // Ctrl-C byte on the pseudoconsole) would never reach them. DETACHED_PROCESS
    // already keeps console control events from the launcher away.
    let base = DETACHED_PROCESS | CREATE_NO_WINDOW;
    match command
        .creation_flags(base | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Ok(child) => Ok(child),
        Err(e) if e.raw_os_error() == Some(5) => {
            // The launcher's job (e.g. an sshd exec session) forbids breakaway.
            // The worker then lives in that job and, if it is KILL_ON_JOB_CLOSE,
            // ends with it: the session survives only as long as the launcher's
            // job (for SSH: the connection). Say so rather than fail silently.
            if launcher_job_kills_on_close() {
                eprintln!(
                    "aplexer: warning: this process runs in a Job Object that forbids \
                     breakaway and kills its members when closed (typical of an SSH \
                     exec session); the new session will end when that job closes. \
                     Start it from a context that allows breakaway (e.g. a scheduled \
                     task or a local console)."
                );
            }
            command.creation_flags(base).spawn()
        }
        Err(e) => Err(e),
    }
}

/// True when the current process is in a Job Object with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set.
fn launcher_job_kills_on_close() -> bool {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        ) != 0
            && info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0
    }
}

/// Clears HANDLE_FLAG_INHERIT on this process's std handles, restoring each
/// one that had it set on drop.
struct NoStdHandleInherit(Vec<(HANDLE, u32)>);

impl NoStdHandleInherit {
    fn new() -> Self {
        use windows_sys::Win32::Foundation::GetHandleInformation;
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut saved = Vec::new();
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(id) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags = 0u32;
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                continue;
            }
            if flags & HANDLE_FLAG_INHERIT != 0
                && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0
            {
                saved.push((handle, HANDLE_FLAG_INHERIT));
            }
        }
        Self(saved)
    }
}

impl Drop for NoStdHandleInherit {
    fn drop(&mut self) {
        for (handle, flags) in &self.0 {
            unsafe { SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, *flags) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn read_until(mut reader: File, needle: &str, secs: u64) -> String {
        let (tx, rx) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
                all.extend(chunk);
                if String::from_utf8_lossy(&all).contains(needle) {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&all).into_owned()
    }

    fn cwd() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    #[test]
    fn quoting_matches_argv_rules() {
        let w = build_command_line(&["a b", "c\"d", "e\\", "", "x"]);
        let s = String::from_utf16(&w[..w.len() - 1]).unwrap();
        assert_eq!(s, r#""a b" c\"d e\ "" x"#);
        let w = build_command_line(&["a b\\"]);
        assert_eq!(String::from_utf16(&w[..w.len() - 1]).unwrap(), r#""a b\\""#);
    }

    #[test]
    fn conpty_echo_roundtrip() {
        let pty = PtyMaster::open(24, 80).unwrap();
        let reader = pty.reader().unwrap();
        let wl = spawn_workload(
            &["cmd.exe", "/c", "echo", "hi_conpty"],
            &cwd(),
            &EnvOverrides::new(),
            Some(&pty),
            None,
        )
        .unwrap();
        assert!(wl.pid() > 0);
        let out = read_until(reader, "hi_conpty", 15);
        assert!(out.contains("hi_conpty"), "output was {out:?}");
        assert_eq!(wl.wait().unwrap(), 0);
        assert_eq!(wl.try_wait().unwrap(), Some(0));
        pty.resize(40, 100).unwrap();
        // Close must not deadlock even though nobody is reading.
        pty.close_and_drain();
        assert!(pty.is_closed());
        assert!(pty.resize(10, 10).is_err());
    }

    #[test]
    fn conpty_close_gives_reader_eof() {
        let pty = PtyMaster::open(24, 80).unwrap();
        let mut reader = pty.reader().unwrap();
        let wl = spawn_workload(
            &["cmd.exe", "/c", "exit", "0"],
            &cwd(),
            &EnvOverrides::new(),
            Some(&pty),
            None,
        )
        .unwrap();
        wl.wait().unwrap();
        pty.close();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut b = [0u8; 4096];
            while matches!(reader.read(&mut b), Ok(n) if n > 0) {}
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("reader EOF");
    }

    #[test]
    fn conpty_input_reaches_workload() {
        use std::io::Write;
        let pty = PtyMaster::open(24, 80).unwrap();
        let reader = pty.reader().unwrap();
        let mut writer = pty.writer().unwrap();
        let wl = spawn_workload(
            &["cmd.exe", "/q", "/k"],
            &cwd(),
            &EnvOverrides::new(),
            Some(&pty),
            None,
        )
        .unwrap();
        writer.write_all(b"echo typed_in_pty\r\n").unwrap();
        let out = read_until(reader, "typed_in_pty", 15);
        assert!(out.contains("typed_in_pty"), "output was {out:?}");
        writer.write_all(b"exit 7\r\n").unwrap();
        assert_eq!(wl.wait_timeout(Duration::from_secs(10)).unwrap(), Some(7));
        pty.close_and_drain();
    }

    #[test]
    fn plain_spawn_exit_code_and_env() {
        let mut env = EnvOverrides::new();
        env.insert("APLEXER_TEST_VAR".into(), Some("seven".into()));
        let wl = spawn_workload(
            &[
                "cmd.exe",
                "/c",
                "if \"%APLEXER_TEST_VAR%\"==\"seven\" (exit 3) else (exit 9)",
            ],
            &cwd(),
            &env,
            None,
            None,
        )
        .unwrap();
        assert_eq!(wl.wait().unwrap(), 3);
    }

    #[test]
    fn missing_program_is_not_found() {
        let e = spawn_workload(
            &["definitely-not-a-program-xyz"],
            &cwd(),
            &EnvOverrides::new(),
            None,
            None,
        )
        .err()
        .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn job_assignment_and_kill_on_close() {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let job = CreateJobObjectW(null(), null());
            assert!(!job.is_null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            assert_ne!(
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ),
                0
            );
            let pty = PtyMaster::open(24, 80).unwrap();
            let wl = spawn_workload(
                &["cmd.exe", "/q", "/k"],
                &cwd(),
                &EnvOverrides::new(),
                Some(&pty),
                Some(job),
            )
            .unwrap();
            assert_eq!(wl.try_wait().unwrap(), None);
            CloseHandle(job);
            assert!(wl.wait_timeout(Duration::from_secs(10)).unwrap().is_some());
            pty.close_and_drain();
        }
    }

    #[test]
    fn detached_worker_spawns() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "exit", "0"]);
        let mut child = spawn_detached_worker(&mut cmd).unwrap();
        assert!(child.wait().unwrap().success());
    }
}
