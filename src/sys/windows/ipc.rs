//! Named-pipe transport: Stream/Listener with timeouts, try_clone, shutdown,
//! owner-only DACL, FIRST_PIPE_INSTANCE. Owner: agent "ipc".
//!
//! Seam API (use through the cross-platform alias `crate::sys::ipc::{Stream, Listener}`):
//!
//! * [`pipe_name`]`(session: Uuid) -> io::Result<PathBuf>`: `\\.\pipe\aplexer-<user-sid>-<uuid>`.
//!   Store it in the record's `socket_path`.
//! * [`Listener::bind`]`(&Path)`: first instance (squatting fails with `AddrInUse`), owner-only
//!   DACL, remote clients rejected. [`Listener::accept_timeout`]`(Duration) -> io::Result<Option<Stream>>`
//!   returns `Ok(None)` on timeout; [`Listener::accept`] blocks.
//! * [`connect`]`(&Path, Duration) -> io::Result<Stream>`: bounded connect, retries `ERROR_PIPE_BUSY`.
//!   A missing pipe is `ErrorKind::NotFound`; an expired budget is `ErrorKind::TimedOut`.
//! * [`Stream`]: `Read + Write` (also for `&Stream`), `try_clone`, `set_read_timeout`,
//!   `set_write_timeout` (timeouts surface as `WouldBlock`, shared between clones like
//!   `SO_RCVTIMEO`), `shutdown(std::net::Shutdown)`, and [`Stream::peer_is_current_user`]
//!   (server side: client process token SID equals ours).
//! * [`pipe_exists`]`(&Path) -> bool`: cheap existence probe without connecting.

use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, LocalFree, DUPLICATE_SAME_ACCESS,
    ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    ERROR_SEM_TIMEOUT, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, ResetEvent,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const PIPE_BUFFER: u32 = 64 * 1024;
/// How often a wait with no deadline wakes to notice `shutdown`.
const IDLE_SLICE_MS: u32 = 250;

// ---------------------------------------------------------------- helpers

fn os_err(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn last_err() -> io::Error {
    os_err(unsafe { GetLastError() })
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut v: Vec<u16> = path.as_os_str().encode_wide().collect();
    if v.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipe name contains NUL",
        ));
    }
    v.push(0);
    Ok(v)
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct OwnedHandle(HANDLE);
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// One cached manual-reset event per thread for overlapped waits.
struct ThreadEvent(HANDLE);
impl Drop for ThreadEvent {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}
thread_local! {
    static EVENT: ThreadEvent = ThreadEvent(unsafe { CreateEventW(null(), 1, 0, null()) });
}

/// Run one overlapped operation to completion (or cancel it at `deadline` /
/// when `stop` flips). The operation and its buffers are never left in
/// flight: every path waits for the kernel to finish with them.
enum Outcome {
    Done(u32),
    Failed(u32),
    TimedOut(u32),
    Stopped(u32),
}

fn run_overlapped(
    handle: HANDLE,
    deadline: Option<Instant>,
    stop: &AtomicBool,
    start: impl FnOnce(*mut OVERLAPPED) -> i32,
) -> io::Result<Outcome> {
    let event = EVENT.with(|e| e.0);
    if event.is_null() {
        return Err(last_err());
    }
    unsafe { ResetEvent(event) };
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = event;
    if start(&mut ov) == 0 {
        let code = unsafe { GetLastError() };
        if code != ERROR_IO_PENDING {
            return Ok(Outcome::Failed(code));
        }
    }
    let mut cancelled: Option<bool> = None; // Some(timed_out)
    loop {
        let slice = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    cancelled = Some(true);
                    break;
                }
                (left.as_millis().min(IDLE_SLICE_MS as u128) as u32).max(1)
            }
            None => IDLE_SLICE_MS,
        };
        match unsafe { WaitForSingleObject(event, slice) } {
            WAIT_OBJECT_0 => break,
            WAIT_TIMEOUT => {
                if stop.load(Ordering::Acquire) {
                    cancelled = Some(false);
                    break;
                }
            }
            _ => {
                cancelled = Some(false);
                break;
            }
        }
    }
    if cancelled.is_some() {
        unsafe { CancelIoEx(handle, &ov) };
    }
    let mut n: u32 = 0;
    // Block until the kernel is done with `ov` and the buffer.
    let ok = unsafe { GetOverlappedResult(handle, &ov, &mut n, 1) };
    if ok != 0 {
        return Ok(Outcome::Done(n));
    }
    let code = unsafe { GetLastError() };
    Ok(match cancelled {
        Some(timed_out) if code == ERROR_OPERATION_ABORTED => {
            if timed_out {
                Outcome::TimedOut(n)
            } else {
                Outcome::Stopped(n)
            }
        }
        // Cancelled by `Stream::shutdown` from another thread.
        None if code == ERROR_OPERATION_ABORTED && stop.load(Ordering::Acquire) => {
            Outcome::Stopped(n)
        }
        _ => Outcome::Failed(code),
    })
}

// ------------------------------------------------------------ identities

fn token_user_sid_bytes(token: HANDLE) -> io::Result<Vec<u8>> {
    let mut len = 0u32;
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(last_err());
    }
    let mut buf = vec![0u8; len as usize];
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0
    {
        return Err(last_err());
    }
    Ok(buf)
}

fn sid_of(buf: &[u8]) -> *mut c_void {
    // SAFETY: buf is a TOKEN_USER filled by GetTokenInformation.
    unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
}

fn current_token_user() -> io::Result<Vec<u8>> {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_err());
    }
    let token = OwnedHandle(token);
    token_user_sid_bytes(token.0)
}

/// The current user's SID in string form (`S-1-5-21-...`).
pub fn current_user_sid_string() -> io::Result<String> {
    let user = current_token_user()?;
    let mut out: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(sid_of(&user), &mut out) } == 0 {
        return Err(last_err());
    }
    let mut len = 0usize;
    unsafe {
        while *out.add(len) != 0 {
            len += 1;
        }
    }
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, len) });
    unsafe { LocalFree(out.cast()) };
    Ok(s)
}

/// `\\.\pipe\aplexer-<user-sid>-<session-uuid>`.
pub fn pipe_name(session: uuid::Uuid) -> io::Result<PathBuf> {
    Ok(PathBuf::from(format!(
        r"\\.\pipe\aplexer-{}-{}",
        current_user_sid_string()?,
        session.as_hyphenated()
    )))
}

/// Owns a security descriptor made from SDDL granting only the current user.
struct OwnerOnlySd(*mut c_void);
unsafe impl Send for OwnerOnlySd {}
unsafe impl Sync for OwnerOnlySd {}
impl OwnerOnlySd {
    fn new() -> io::Result<Self> {
        let sddl = wide_str(&format!("D:P(A;;GA;;;{})", current_user_sid_string()?));
        let mut sd: *mut c_void = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        } == 0
        {
            return Err(last_err());
        }
        Ok(Self(sd))
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}
impl Drop for OwnerOnlySd {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

// ----------------------------------------------------------------- Stream

#[derive(Default)]
struct Shared {
    /// 0 = no timeout.
    read_ms: AtomicU64,
    write_ms: AtomicU64,
    shutdown: AtomicBool,
}

/// One end of a connected named pipe. Clones share timeouts and shutdown.
pub struct Stream {
    handle: OwnedHandle,
    shared: Arc<Shared>,
}

fn timeout_ms(t: Option<Duration>) -> io::Result<u64> {
    match t {
        None => Ok(0),
        Some(d) if d.is_zero() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot set a 0 duration timeout",
        )),
        Some(d) => Ok((d.as_millis() as u64).max(1)),
    }
}

impl Stream {
    fn from_handle(handle: HANDLE) -> Self {
        Self {
            handle: OwnedHandle(handle),
            shared: Arc::new(Shared::default()),
        }
    }

    pub fn try_clone(&self) -> io::Result<Stream> {
        let mut dup: HANDLE = null_mut();
        let ok = unsafe {
            let p = GetCurrentProcess();
            DuplicateHandle(p, self.handle.0, p, &mut dup, 0, 0, DUPLICATE_SAME_ACCESS)
        };
        if ok == 0 {
            return Err(last_err());
        }
        Ok(Stream {
            handle: OwnedHandle(dup),
            shared: self.shared.clone(),
        })
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.shared.read_ms.store(timeout_ms(t)?, Ordering::Release);
        Ok(())
    }

    pub fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.shared
            .write_ms
            .store(timeout_ms(t)?, Ordering::Release);
        Ok(())
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(match self.shared.read_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        })
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(match self.shared.write_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        })
    }

    /// Named pipes have no half-close. Any shutdown flips a flag shared by
    /// all clones: pending and future reads see EOF, writes fail with
    /// `BrokenPipe`. The peer observes EOF once the last clone is dropped
    /// (buffered data stays readable until then).
    pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
        self.shared.shutdown.store(true, Ordering::Release);
        unsafe { CancelIoEx(self.handle.0, null()) };
        Ok(())
    }

    /// Server side: is the connected client a process of the current user?
    pub fn peer_is_current_user(&self) -> io::Result<bool> {
        let mut pid = 0u32;
        if unsafe { GetNamedPipeClientProcessId(self.handle.0, &mut pid) } == 0 {
            return Err(last_err());
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            let code = unsafe { GetLastError() };
            if code == ERROR_ACCESS_DENIED {
                return Ok(false);
            }
            return Err(os_err(code));
        }
        let process = OwnedHandle(process);
        let mut token: HANDLE = null_mut();
        if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_ACCESS_DENIED {
                return Ok(false);
            }
            return Err(os_err(code));
        }
        let token = OwnedHandle(token);
        let theirs = token_user_sid_bytes(token.0)?;
        let ours = current_token_user()?;
        Ok(unsafe { EqualSid(sid_of(&theirs), sid_of(&ours)) } != 0)
    }

    fn read_inner(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.shared.shutdown.load(Ordering::Acquire) {
            return Ok(0);
        }
        let ms = self.shared.read_ms.load(Ordering::Acquire);
        let deadline = (ms != 0).then(|| Instant::now() + Duration::from_millis(ms));
        let len = buf.len().min(1 << 30) as u32;
        let ptr = buf.as_mut_ptr();
        let out = run_overlapped(
            self.handle.0,
            deadline,
            &self.shared.shutdown,
            |ov| unsafe { ReadFile(self.handle.0, ptr.cast(), len, null_mut(), ov) },
        )?;
        match out {
            Outcome::Done(n) => Ok(n as usize),
            Outcome::TimedOut(n) if n > 0 => Ok(n as usize),
            Outcome::TimedOut(_) => {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"))
            }
            Outcome::Stopped(n) => Ok(n as usize),
            Outcome::Failed(code)
                if code == ERROR_BROKEN_PIPE || code == ERROR_PIPE_NOT_CONNECTED =>
            {
                Ok(0)
            }
            Outcome::Failed(code) => Err(os_err(code)),
        }
    }

    fn write_inner(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let ms = self.shared.write_ms.load(Ordering::Acquire);
        let deadline = (ms != 0).then(|| Instant::now() + Duration::from_millis(ms));
        let len = buf.len().min(1 << 30) as u32;
        let ptr = buf.as_ptr();
        let out = run_overlapped(
            self.handle.0,
            deadline,
            &self.shared.shutdown,
            |ov| unsafe { WriteFile(self.handle.0, ptr.cast(), len, null_mut(), ov) },
        )?;
        match out {
            Outcome::Done(n) => Ok(n as usize),
            Outcome::TimedOut(n) if n > 0 => Ok(n as usize),
            Outcome::TimedOut(_) => {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "write timed out"))
            }
            Outcome::Stopped(n) if n > 0 => Ok(n as usize),
            Outcome::Stopped(_) => Err(io::ErrorKind::BrokenPipe.into()),
            Outcome::Failed(code)
                if code == ERROR_NO_DATA
                    || code == ERROR_BROKEN_PIPE
                    || code == ERROR_PIPE_NOT_CONNECTED =>
            {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            Outcome::Failed(code) => Err(os_err(code)),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_inner(buf)
    }
}
impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_inner(buf)
    }
}
impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_inner(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_inner(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

// --------------------------------------------------------------- Listener

/// Server endpoint. Always keeps one unconnected instance waiting; after each
/// accept the next instance is created before the connected one is returned.
pub struct Listener {
    name: Vec<u16>,
    sd: OwnerOnlySd,
    /// The instance currently listening (None only after a failed re-create).
    pending: Mutex<Option<OwnedHandle>>,
    stop: AtomicBool,
}

impl Listener {
    fn create_instance(&self, first: bool) -> io::Result<OwnedHandle> {
        let sa = self.sd.attributes();
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let h = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            return Err(
                if first && (code == ERROR_ACCESS_DENIED || code == ERROR_PIPE_BUSY) {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("pipe name already in use (os error {code})"),
                    )
                } else {
                    os_err(code)
                },
            );
        }
        Ok(OwnedHandle(h))
    }

    /// Create the pipe. Fails with `AddrInUse` if the name already exists.
    pub fn bind(path: &Path) -> io::Result<Listener> {
        let listener = Listener {
            name: wide(path)?,
            sd: OwnerOnlySd::new()?,
            pending: Mutex::new(None),
            stop: AtomicBool::new(false),
        };
        let first = listener.create_instance(true)?;
        *listener.pending.lock().unwrap() = Some(first);
        Ok(listener)
    }

    pub fn accept(&self) -> io::Result<Stream> {
        loop {
            if let Some(s) = self.accept_timeout(Duration::from_secs(3600))? {
                return Ok(s);
            }
        }
    }

    /// Wait up to `timeout` for a client. `Ok(None)` means nobody came.
    pub fn accept_timeout(&self, timeout: Duration) -> io::Result<Option<Stream>> {
        let mut guard = self.pending.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.create_instance(false)?);
        }
        let h = guard.as_ref().unwrap().0;
        let deadline = Instant::now() + timeout;
        let out = run_overlapped(h, Some(deadline), &self.stop, |ov| unsafe {
            ConnectNamedPipe(h, ov)
        })?;
        match out {
            Outcome::Done(_) => {}
            Outcome::Failed(code) if code == ERROR_PIPE_CONNECTED => {}
            Outcome::TimedOut(_) => return Ok(None),
            Outcome::Stopped(_) => return Ok(None),
            Outcome::Failed(code) => return Err(os_err(code)),
        }
        let connected = guard.take().unwrap();
        // Re-create right away so there is always an instance to find.
        // Failure is tolerated: the next accept retries.
        *guard = self.create_instance(false).ok();
        drop(guard);
        Ok(Some(Stream::from_handle(into_raw(connected))))
    }
}

fn into_raw(h: OwnedHandle) -> HANDLE {
    let raw = h.0;
    std::mem::forget(h);
    raw
}

// ---------------------------------------------------------------- connect

/// Cheap existence probe: does a pipe of this name currently exist?
pub fn pipe_exists(path: &Path) -> bool {
    let Ok(name) = wide(path) else { return false };
    if unsafe { WaitNamedPipeW(name.as_ptr(), 1) } != 0 {
        return true;
    }
    let code = unsafe { GetLastError() };
    code != ERROR_FILE_NOT_FOUND
}

/// Connect, waiting at most `timeout` (including busy-instance retries).
pub fn connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
    let name = wide(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect {} timed out", path.display()),
            ));
        }
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(Stream::from_handle(h));
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_PIPE_BUSY {
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = (left.as_millis().min(100) as u32).max(1);
            if unsafe { WaitNamedPipeW(name.as_ptr(), ms) } == 0 {
                let c = unsafe { GetLastError() };
                if c != ERROR_SEM_TIMEOUT && c != ERROR_PIPE_BUSY {
                    return Err(os_err(c));
                }
            }
            continue;
        }
        return Err(os_err(code));
    }
}

impl Stream {
    /// Same as the free [`connect`]; mirrors `UnixStream::connect`.
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
        connect(path, timeout)
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn unique() -> PathBuf {
        pipe_name(uuid::Uuid::new_v4()).unwrap()
    }

    #[test]
    fn name_has_sid_and_uuid() {
        let id = uuid::Uuid::new_v4();
        let n = pipe_name(id).unwrap();
        let s = n.to_string_lossy().into_owned();
        assert!(s.starts_with(r"\\.\pipe\aplexer-S-1-"), "{s}");
        assert!(s.ends_with(&id.to_string()));
    }

    #[test]
    fn echo() {
        let path = unique();
        let l = Listener::bind(&path).unwrap();
        let t = thread::spawn(move || {
            let mut s = l.accept_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert!(s.peer_is_current_user().unwrap());
            let mut b = [0u8; 5];
            s.read_exact(&mut b).unwrap();
            s.write_all(&b).unwrap();
            // EOF after the client closes.
            assert_eq!(s.read(&mut b).unwrap(), 0);
        });
        let mut c = connect(&path, Duration::from_secs(5)).unwrap();
        c.write_all(b"hello").unwrap();
        let mut b = [0u8; 5];
        c.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"hello");
        drop(c);
        t.join().unwrap();
    }

    #[test]
    fn many_sequential_clients_and_exists() {
        let path = unique();
        let l = Listener::bind(&path).unwrap();
        assert!(pipe_exists(&path));
        let t = thread::spawn(move || {
            for _ in 0..5 {
                let mut s = l.accept_timeout(Duration::from_secs(5)).unwrap().unwrap();
                let mut b = [0u8; 1];
                s.read_exact(&mut b).unwrap();
                s.write_all(&b).unwrap();
            }
        });
        for i in 0..5u8 {
            let mut c = connect(&path, Duration::from_secs(5)).unwrap();
            c.write_all(&[i]).unwrap();
            let mut b = [0u8; 1];
            c.read_exact(&mut b).unwrap();
            assert_eq!(b[0], i);
        }
        t.join().unwrap();
        assert!(!pipe_exists(&path));
    }

    #[test]
    fn timeouts() {
        let path = unique();
        let l = Listener::bind(&path).unwrap();
        // accept timeout
        let t0 = Instant::now();
        assert!(l
            .accept_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
        assert!(t0.elapsed() >= Duration::from_millis(90));
        let c = connect(&path, Duration::from_secs(5)).unwrap();
        let s = l.accept_timeout(Duration::from_secs(5)).unwrap().unwrap();
        // read timeout surfaces as WouldBlock; connection stays usable
        c.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut b = [0u8; 1];
        let e = (&c).read(&mut b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        (&s).write_all(b"x").unwrap();
        assert_eq!((&c).read(&mut b).unwrap(), 1);
        // write timeout when nobody reads and the buffer fills
        s.set_write_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let chunk = vec![0u8; 1 << 20];
        let e = loop {
            match (&s).write(&chunk) {
                Ok(n) => assert!(n > 0),
                Err(e) => break e,
            }
        };
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        // connect timeout against a missing pipe is NotFound
        let missing = unique();
        assert_eq!(
            connect(&missing, Duration::from_millis(100))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn clone_shutdown_unblocks_reader() {
        let path = unique();
        let l = Listener::bind(&path).unwrap();
        let c = connect(&path, Duration::from_secs(5)).unwrap();
        let _s = l.accept_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let r = c.try_clone().unwrap();
        let t = thread::spawn(move || {
            let mut b = [0u8; 1];
            (&r).read(&mut b).unwrap()
        });
        thread::sleep(Duration::from_millis(100));
        c.shutdown(Shutdown::Both).unwrap();
        assert_eq!(t.join().unwrap(), 0);
        assert_eq!(
            (&c).write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn second_instance_squat_rejected() {
        let path = unique();
        let _l = Listener::bind(&path).unwrap();
        let e = Listener::bind(&path).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn large_payload_full_duplex() {
        let path = unique();
        let l = Listener::bind(&path).unwrap();
        const N: usize = 8 * 1024 * 1024;
        let t = thread::spawn(move || {
            let s = l.accept_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let w = s.try_clone().unwrap();
            let wt = thread::spawn(move || {
                let data: Vec<u8> = (0..N).map(|i| (i % 251) as u8).collect();
                (&w).write_all(&data).unwrap();
            });
            let mut got = Vec::new();
            let mut buf = vec![0u8; 65536];
            while got.len() < N {
                let n = (&s).read(&mut buf).unwrap();
                assert!(n > 0);
                got.extend_from_slice(&buf[..n]);
            }
            wt.join().unwrap();
            got
        });
        let c = connect(&path, Duration::from_secs(5)).unwrap();
        let w = c.try_clone().unwrap();
        let wt = thread::spawn(move || {
            let data: Vec<u8> = (0..N).map(|i| (i % 241) as u8).collect();
            (&w).write_all(&data).unwrap();
        });
        let mut got = Vec::new();
        let mut buf = vec![0u8; 65536];
        while got.len() < N {
            let n = (&c).read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        wt.join().unwrap();
        assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        let server_got = t.join().unwrap();
        assert!(server_got
            .iter()
            .enumerate()
            .all(|(i, b)| *b == (i % 241) as u8));
    }
}
