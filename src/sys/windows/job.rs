//! Job Objects: containment, tree kill, emptiness, accounting/usage, process
//! identity (pid + creation time), pinned process handles. Owner: agent "job".
//!
//! Seam API (all `io::Result`, no `anyhow`, so it can be unit-tested alone):
//!
//! * [`Job`] -- one named Job Object per session (`aplexer-<uuid>`),
//!   `KILL_ON_JOB_CLOSE`, no breakaway. `Job::create(id, &JobLimits)`,
//!   `Job::open(id)` (None when no such job: every handle closed, so every
//!   member is dead), `assign_pid`, `terminate`, `kill_until_empty`,
//!   `process_ids`, `active_processes`, `is_empty`, `accounting`,
//!   `as_raw_handle` (for `PROC_THREAD_ATTRIBUTE_JOB_LIST` at spawn so a
//!   workload is contained before it can fork). `Job` is cheap to `Clone`.
//! * [`install_session_job`] / [`session_job`] -- the process-wide job of the
//!   running worker, used by the Windows branches of `worker::procs`.
//! * [`ProcessIdentity`] `{pid, creation_time}` (FILETIME as u64),
//!   [`process_identity`], [`verify_identity`] -> [`IdentityCheck`].
//! * [`PinnedProcess`] -- a process handle pinned to an identity: `open`,
//!   `open_identity`, `terminate`, `is_exited`, `wait`, `cpu_time_100ns`.
//! * [`process_alive`], [`child_pids`] (ToolHelp), [`process_usage`].

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    ERROR_MORE_DATA, FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// Job access rights (not exported as typed constants by windows-sys).
const JOB_OBJECT_ASSIGN_PROCESS: u32 = 0x0001;
const JOB_OBJECT_SET_ATTRIBUTES: u32 = 0x0002;
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;
const SYNCHRONIZE: u32 = 0x0010_0000;

/// Exit code given to processes ended by `terminate` when the caller has no
/// better one. Windows has no "killed by signal" status.
pub const KILLED_EXIT_CODE: u32 = 1;

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Map "no such process" onto `NotFound` so callers can use the same
/// `ErrorKind` test the Linux code uses for a vanished `/proc/<pid>`.
fn process_open_error() -> io::Error {
    let code = unsafe { GetLastError() };
    if code == ERROR_INVALID_PARAMETER || code == ERROR_FILE_NOT_FOUND {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no such process (OpenProcess: invalid parameter)",
        )
    } else {
        io::Error::from_raw_os_error(code as i32)
    }
}

/// An owned kernel handle, closed on drop.
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

// A kernel HANDLE is usable from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(last_error())
        } else {
            Ok(Self(handle))
        }
    }

    pub fn as_raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

// --- Process identity --------------------------------------------------------

/// A process pinned across pid reuse: the pid plus its creation time
/// (FILETIME, 100 ns ticks since 1601). Replaces the Linux
/// `{pid, start_time_ticks, boot_id}` triple; a reboot changes the creation
/// time of any recycled pid, so no boot id is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time: u64,
}

/// How the process at a recorded identity's pid compares with it right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCheck {
    /// Same pid, same creation time, still running: the recorded process.
    Verified,
    /// No process holds that pid, or it has exited (the Windows analogue of
    /// "gone or zombie": it can no longer run code).
    Gone,
    /// The pid was recycled by a later process.
    Reused { recorded: u64, current: u64 },
}

fn open_process(pid: u32, access: u32) -> io::Result<OwnedHandle> {
    if pid == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, "pid 0"));
    }
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(process_open_error());
    }
    Ok(OwnedHandle(handle))
}

fn process_times(handle: HANDLE) -> io::Result<(u64, u64)> {
    let mut creation: FILETIME = unsafe { zeroed() };
    let mut exit: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(last_error());
    }
    Ok((
        filetime_u64(&creation),
        filetime_u64(&kernel).saturating_add(filetime_u64(&user)),
    ))
}

fn handle_exited(handle: HANDLE) -> io::Result<bool> {
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(last_error()),
        other => Err(io::Error::other(format!("unexpected wait result {other}"))),
    }
}

/// The identity of the process currently holding `pid`.
/// `ErrorKind::NotFound` when there is none.
pub fn process_identity(pid: u32) -> io::Result<ProcessIdentity> {
    let handle = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let (creation_time, _) = process_times(handle.as_raw())?;
    Ok(ProcessIdentity { pid, creation_time })
}

/// Compare `identity` with whatever holds its pid now. The check runs on a
/// handle, which pins the process object, so a recycle cannot slip in
/// between the creation-time read and the exit probe. `Err` is "could not
/// tell" (access denied, ...); callers decide which way that fails.
pub fn verify_identity(identity: &ProcessIdentity) -> io::Result<IdentityCheck> {
    match PinnedProcess::open_identity(identity)? {
        Ok(_) => Ok(IdentityCheck::Verified),
        Err(check) => Ok(check),
    }
}

/// Whether `pid` names a process that can still run code. Access-denied
/// answers "alive": uncertainty must never subtract a live process.
pub fn process_alive(pid: u32) -> bool {
    match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE) {
        Ok(handle) => !handle_exited(handle.as_raw()).unwrap_or(false),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

// --- Pinned process handle ---------------------------------------------------

/// A process handle pinned to one identity. While the handle is open the pid
/// cannot be recycled, so `terminate` can never hit an unrelated process.
#[derive(Debug)]
pub struct PinnedProcess {
    handle: OwnedHandle,
    identity: ProcessIdentity,
}

impl PinnedProcess {
    /// Pin whatever currently holds `pid`. `Ok(None)` when no such process
    /// (or it has already exited).
    pub fn open(pid: u32) -> io::Result<Option<Self>> {
        let handle = match open_process(
            pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
        ) {
            Ok(handle) => handle,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let (creation_time, _) = process_times(handle.as_raw())?;
        let pinned = Self {
            handle,
            identity: ProcessIdentity { pid, creation_time },
        };
        if pinned.is_exited()? {
            return Ok(None);
        }
        Ok(Some(pinned))
    }

    /// Pin `identity` exactly. Outer `Err` is "could not tell"; the inner
    /// `Err` says why the recorded process is not there.
    pub fn open_identity(identity: &ProcessIdentity) -> io::Result<Result<Self, IdentityCheck>> {
        let handle = match open_process(
            identity.pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
        ) {
            Ok(handle) => handle,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Err(IdentityCheck::Gone))
            }
            Err(error) => return Err(error),
        };
        let (creation_time, _) = process_times(handle.as_raw())?;
        if creation_time != identity.creation_time {
            return Ok(Err(IdentityCheck::Reused {
                recorded: identity.creation_time,
                current: creation_time,
            }));
        }
        let pinned = Self {
            handle,
            identity: *identity,
        };
        if pinned.is_exited()? {
            return Ok(Err(IdentityCheck::Gone));
        }
        Ok(Ok(pinned))
    }

    pub fn pid(&self) -> u32 {
        self.identity.pid
    }

    pub fn creation_time(&self) -> u64 {
        self.identity.creation_time
    }

    pub fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    pub fn as_raw_handle(&self) -> HANDLE {
        self.handle.as_raw()
    }

    pub fn is_exited(&self) -> io::Result<bool> {
        handle_exited(self.handle.as_raw())
    }

    /// Block until exit or `timeout`; true when the process exited.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        match unsafe { WaitForSingleObject(self.handle.as_raw(), ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(last_error()),
        }
    }

    /// End this process only (not its children). A process that has already
    /// exited is not an error.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        if unsafe { TerminateProcess(self.handle.as_raw(), exit_code) } != 0 {
            return Ok(());
        }
        let error = last_error();
        if self.is_exited().unwrap_or(false) {
            return Ok(());
        }
        Err(error)
    }

    /// Kernel plus user CPU time so far, in 100 ns units.
    pub fn cpu_time_100ns(&self) -> io::Result<u64> {
        process_times(self.handle.as_raw()).map(|(_, cpu)| cpu)
    }

    pub fn working_set_bytes(&self) -> io::Result<u64> {
        working_set(self.handle.as_raw())
    }
}

fn working_set(handle: HANDLE) -> io::Result<u64> {
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { GetProcessMemoryInfo(handle, &mut counters, counters.cb) } == 0 {
        return Err(last_error());
    }
    Ok(counters.WorkingSetSize as u64)
}

/// `(creation FILETIME, cpu time in 100 ns, working set bytes)` for a pid, or
/// `None` when it is not a live process. Used by the `proc_usage` backend.
pub fn process_usage(pid: u32) -> Option<(u64, u64, u64)> {
    let pinned = PinnedProcess::open(pid).ok()??;
    let cpu = pinned.cpu_time_100ns().ok()?;
    let mem = pinned.working_set_bytes().unwrap_or(0);
    Some((pinned.creation_time(), cpu, mem))
}

/// Direct children of `pid` per a ToolHelp snapshot. Windows keeps the parent
/// pid of a dead parent, so this can over-report a recycled parent; callers
/// that need certainty use a [`Job`] instead.
pub fn child_pids(pid: u32) -> io::Result<Vec<u32>> {
    let snapshot = OwnedHandle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut children = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot.as_raw(), &mut entry) };
    while ok != 0 {
        if entry.th32ParentProcessID == pid && entry.th32ProcessID != pid {
            children.push(entry.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snapshot.as_raw(), &mut entry) };
    }
    Ok(children)
}

// --- Job Object ----------------------------------------------------------------

/// Resource limits for a session job, from the record's `Limits`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobLimits {
    /// Committed memory for the whole job (`JobMemoryLimit`).
    pub memory_bytes: Option<u64>,
    /// Maximum simultaneously active processes (`ActiveProcessLimit`).
    pub pids: Option<u64>,
    /// CPU time per `cpu_period_us` (default 100 ms), as a hard cap.
    pub cpu_quota_us: Option<u64>,
    pub cpu_period_us: Option<u64>,
}

/// Counters from `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobAccounting {
    pub active_processes: u32,
    pub total_processes: u32,
    /// User + kernel time of every process that ever ran in the job
    /// (including exited ones), in 100 ns units.
    pub cpu_time_100ns: u64,
}

/// The kernel object name for a session's job.
pub fn job_name(id: impl std::fmt::Display) -> String {
    format!("aplexer-{id}")
}

/// One named Job Object. Cloning shares the same handle; the job's members
/// are killed when the last handle anywhere closes (`KILL_ON_JOB_CLOSE`).
#[derive(Debug, Clone)]
pub struct Job {
    handle: Arc<OwnedHandle>,
    name: String,
}

impl Job {
    /// Create the session job. Fails with `AlreadyExists` instead of
    /// adopting a job someone else already holds under this name.
    pub fn create(id: impl std::fmt::Display, limits: &JobLimits) -> io::Result<Self> {
        let name = job_name(id);
        let wname = wide(&name);
        let raw = unsafe { CreateJobObjectW(null(), wname.as_ptr()) };
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle::new(raw)?;
        if already_exists {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("job object {name} already exists"),
            ));
        }
        let job = Self {
            handle: Arc::new(handle),
            name,
        };
        job.apply_limits(limits)?;
        Ok(job)
    }

    /// Reopen a session's job by name. `Ok(None)` when it no longer exists,
    /// which means every handle to it closed and `KILL_ON_JOB_CLOSE` already
    /// ended every member.
    pub fn open(id: impl std::fmt::Display) -> io::Result<Option<Self>> {
        let name = job_name(id);
        let wname = wide(&name);
        let raw = unsafe {
            OpenJobObjectW(
                JOB_OBJECT_ASSIGN_PROCESS
                    | JOB_OBJECT_SET_ATTRIBUTES
                    | JOB_OBJECT_QUERY
                    | JOB_OBJECT_TERMINATE,
                0,
                wname.as_ptr(),
            )
        };
        if raw.is_null() {
            let code = unsafe { GetLastError() };
            if code == ERROR_FILE_NOT_FOUND || code == ERROR_INVALID_PARAMETER {
                return Ok(None);
            }
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(Some(Self {
            handle: Arc::new(OwnedHandle(raw)),
            name,
        }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// For `PROC_THREAD_ATTRIBUTE_JOB_LIST`: the workload is then born inside
    /// the job and can never fork out of it before assignment.
    pub fn as_raw_handle(&self) -> HANDLE {
        self.handle.as_raw()
    }

    fn apply_limits(&self, limits: &JobLimits) -> io::Result<()> {
        // No JOB_OBJECT_LIMIT_BREAKAWAY_OK / SILENT_BREAKAWAY_OK: members
        // cannot escape. KILL_ON_JOB_CLOSE is the cleanup guarantee.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(bytes) = limits.memory_bytes {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
        }
        if let Some(pids) = limits.pids {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = u32::try_from(pids).unwrap_or(u32::MAX);
        }
        info.BasicLimitInformation.LimitFlags = flags;
        let ok = unsafe {
            SetInformationJobObject(
                self.handle.as_raw(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        if let Some(quota) = limits.cpu_quota_us {
            self.apply_cpu_cap(quota, limits.cpu_period_us.unwrap_or(100_000))?;
        }
        Ok(())
    }

    fn apply_cpu_cap(&self, quota_us: u64, period_us: u64) -> io::Result<()> {
        let mut system: SYSTEM_INFO = unsafe { zeroed() };
        unsafe { GetSystemInfo(&mut system) };
        let cpus = u64::from(system.dwNumberOfProcessors.max(1));
        // CpuRate is hundredths of a percent of the whole machine.
        let rate = (u128::from(quota_us) * 10_000 / u128::from(period_us.max(1)) / u128::from(cpus))
            .clamp(1, 10_000) as u32;
        let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { zeroed() };
        info.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        info.Anonymous.CpuRate = rate;
        let ok = unsafe {
            SetInformationJobObject(
                self.handle.as_raw(),
                JobObjectCpuRateControlInformation,
                (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Put a process into the job (by pid; the caller owns the race with the
    /// process forking before this returns -- prefer the JOB_LIST attribute).
    pub fn assign_pid(&self, pid: u32) -> io::Result<()> {
        let process = open_process(
            pid,
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
        )?;
        self.assign_handle(process.as_raw())
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn assign_handle(&self, process: HANDLE) -> io::Result<()> {
        if unsafe { AssignProcessToJobObject(self.handle.as_raw(), process) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// `TerminateJobObject`: ends every member. Processes die asynchronously;
    /// use [`Job::kill_until_empty`] to wait for emptiness.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.handle.as_raw(), exit_code) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Terminate, then poll until the job holds no process or `deadline`
    /// passes. Only an observed-empty job is proof, so a deadline is an error.
    pub fn kill_until_empty(&self, deadline: Instant) -> io::Result<()> {
        loop {
            self.terminate(KILLED_EXIT_CODE)?;
            if self.is_empty()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out proving job {} empty", self.name),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn accounting(&self) -> io::Result<JobAccounting> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle.as_raw(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(JobAccounting {
            active_processes: info.ActiveProcesses,
            total_processes: info.TotalProcesses,
            cpu_time_100ns: (info.TotalUserTime.max(0) as u64)
                .saturating_add(info.TotalKernelTime.max(0) as u64),
        })
    }

    pub fn active_processes(&self) -> io::Result<u32> {
        self.accounting().map(|a| a.active_processes)
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        self.active_processes().map(|n| n == 0)
    }

    /// Peak committed memory of the job (`PeakJobMemoryUsed`).
    pub fn peak_memory_bytes(&self) -> io::Result<u64> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle.as_raw(),
                JobObjectExtendedLimitInformation,
                (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(info.PeakJobMemoryUsed as u64)
    }

    /// Pids of every process currently in the job.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        let header = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut capacity = 64usize;
        loop {
            let bytes = header + capacity * size_of::<usize>();
            // u64 backing keeps the buffer 8-byte aligned for the header.
            let mut buffer = vec![0u64; bytes.div_ceil(8)];
            let ok = unsafe {
                QueryInformationJobObject(
                    self.handle.as_raw(),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    bytes as u32,
                    null_mut(),
                )
            };
            let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let assigned = unsafe { (*list).NumberOfAssignedProcesses } as usize;
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_MORE_DATA {
                    capacity = capacity.max(assigned) * 2;
                    continue;
                }
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            let count = unsafe { (*list).NumberOfProcessIdsInList } as usize;
            if assigned > count {
                capacity = assigned + 16;
                continue;
            }
            let ids = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<u8>().add(header).cast::<usize>(),
                    count,
                )
            };
            return Ok(ids.iter().map(|&pid| pid as u32).collect());
        }
    }
}

// --- The running worker's own job ---------------------------------------------

static SESSION_JOB: OnceLock<Job> = OnceLock::new();

/// Register the worker's session job process-wide (once). Returns false if a
/// job was already installed.
pub fn install_session_job(job: Job) -> bool {
    SESSION_JOB.set(job).is_ok()
}

pub fn session_job() -> Option<&'static Job> {
    SESSION_JOB.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn spawn_ping() -> std::process::Child {
        Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    fn unique() -> String {
        format!("test-{}-{:?}", std::process::id(), Instant::now())
            .replace([' ', '{', '}', ':', '.'], "")
    }

    #[test]
    fn job_contains_and_terminates_a_process_tree() {
        let job = Job::create(unique(), &JobLimits::default()).unwrap();
        assert!(job.is_empty().unwrap());
        let mut child = spawn_ping();
        job.assign_pid(child.id()).unwrap();
        // cmd starts ping; wait until the tree shows up in the job.
        let deadline = Instant::now() + Duration::from_secs(5);
        while job.process_ids().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let ids = job.process_ids().unwrap();
        assert!(ids.contains(&child.id()), "{ids:?}");
        assert!(ids.len() >= 2, "ping child must be contained: {ids:?}");
        assert!(!job.is_empty().unwrap());
        assert!(job.accounting().unwrap().total_processes >= 2);

        job.kill_until_empty(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert!(job.is_empty().unwrap());
        assert!(job.process_ids().unwrap().is_empty());
        child.wait().unwrap();
        // The job drops a member from its active count a moment before the
        // process object is signaled, so give the kernel a bounded beat.
        for pid in ids {
            let deadline = Instant::now() + Duration::from_secs(5);
            while process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!process_alive(pid), "pid {pid} survived");
        }
    }

    #[test]
    fn kill_on_job_close_ends_members_and_name_disappears() {
        let id = unique();
        let job = Job::create(&id, &JobLimits::default()).unwrap();
        let mut child = spawn_ping();
        job.assign_pid(child.id()).unwrap();
        assert!(Job::open(&id).unwrap().is_some());
        assert_eq!(
            Job::create(&id, &JobLimits::default()).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(job);
        // Would block ~30 s if the member survived the last handle closing.
        let started = Instant::now();
        child.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(Job::open(&id).unwrap().is_none());
    }

    #[test]
    fn limits_apply_without_error() {
        let limits = JobLimits {
            memory_bytes: Some(512 << 20),
            pids: Some(64),
            cpu_quota_us: Some(50_000),
            cpu_period_us: Some(100_000),
        };
        let job = Job::create(unique(), &limits).unwrap();
        let mut child = spawn_ping();
        job.assign_pid(child.id()).unwrap();
        job.kill_until_empty(Instant::now() + Duration::from_secs(5))
            .unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn identity_pins_and_detects_exit_and_reuse() {
        let me = process_identity(std::process::id()).unwrap();
        assert_eq!(verify_identity(&me).unwrap(), IdentityCheck::Verified);
        let forged = ProcessIdentity {
            pid: me.pid,
            creation_time: me.creation_time + 1,
        };
        assert!(matches!(
            verify_identity(&forged).unwrap(),
            IdentityCheck::Reused { .. }
        ));

        let mut child = spawn_ping();
        let pinned = PinnedProcess::open(child.id()).unwrap().unwrap();
        assert!(!pinned.is_exited().unwrap());
        assert!(pinned.cpu_time_100ns().is_ok());
        let identity = pinned.identity();
        pinned.terminate(7).unwrap();
        assert!(pinned.wait(Duration::from_secs(5)).unwrap());
        assert!(pinned.is_exited().unwrap());
        pinned.terminate(7).unwrap(); // already gone is not an error
        assert_eq!(verify_identity(&identity).unwrap(), IdentityCheck::Gone);
        assert_eq!(child.wait().unwrap().code(), Some(7));
        assert!(PinnedProcess::open(u32::MAX - 1).unwrap().is_none());
        assert!(!process_alive(u32::MAX - 1));
    }

    #[test]
    fn usage_and_children_of_current_process() {
        let (creation, _cpu, mem) = process_usage(std::process::id()).unwrap();
        assert!(creation > 0 && mem > 0);
        let mut child = spawn_ping();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            found = child_pids(std::process::id())
                .unwrap()
                .contains(&child.id());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(found);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
