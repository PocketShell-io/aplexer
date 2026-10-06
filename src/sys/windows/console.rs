//! Client terminal: raw mode, VT modes, size, tty checks, stdin readiness,
//! Ctrl-C/Ctrl-Break cleanup handler. Owner: agent "console".
//!
//! Seam (all "fd" arguments follow the Unix numbering: `0` = stdin,
//! `1` = stdout; anything else is treated as "not a console"):
//! - [`STDIN_FD`], [`STDOUT_FD`]
//! - [`is_tty`]`(fd) -> bool`
//! - [`terminal_size`]`(fd) -> Option<(rows, cols)>` from the visible window
//!   (`srWindow`); `0x0` is reported as-is, the caller applies its default.
//! - [`RawMode::enter`]`(fd)`: stdin without line/echo/processed input and
//!   with VT input; stdout gets VT processing (and no auto CR). Restored on
//!   `Drop`. Ctrl-C then arrives as byte `0x03` rather than as a signal.
//! - [`VtOutput::enable`]`()`: stdout VT processing only (for `attach` with
//!   a console stdout but a non-console stdin); restored on `Drop`.
//! - [`stdin_readable`]`(timeout) -> bool`: console (key-down events only,
//!   other events are discarded), pipe (`PeekNamedPipe`) and file stdin.
//!   Errors answer "yes", like the Unix `poll` wrapper.
//! - [`CleanupSignals::install`]`(callback)` / [`CleanupSignals::finish`]:
//!   `SetConsoleCtrlHandler` replacement for the Unix signal bridge. The
//!   callback runs on a handler thread. `finish` returns the exit code the
//!   process should end with if a signal was caught.
//! - [`parse_signal_name`]: contract signal parsing (TERM/INT/KILL only).

use std::io::{self, IsTerminal};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileType, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_CHAR, FILE_TYPE_PIPE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, PeekConsoleInputW, ReadConsoleInputW,
    SetConsoleCtrlHandler, SetConsoleMode, CONSOLE_SCREEN_BUFFER_INFO, CTRL_BREAK_EVENT,
    CTRL_C_EVENT, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS,
    ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WINDOW_INPUT, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub const STDIN_FD: i32 = 0;
pub const STDOUT_FD: i32 = 1;

/// `STATUS_CONTROL_C_EXIT`, the conventional exit status of a process ended
/// by Ctrl-C / Ctrl-Break.
pub const EXIT_CONTROL_C: i32 = 0xC000_013A_u32 as i32;

fn std_handle(fd: i32) -> Option<HANDLE> {
    let which = match fd {
        STDIN_FD => STD_INPUT_HANDLE,
        STDOUT_FD => STD_OUTPUT_HANDLE,
        _ => return None,
    };
    let handle = unsafe { GetStdHandle(which) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(handle)
    }
}

fn console_mode(handle: HANDLE) -> Option<u32> {
    let mut mode = 0u32;
    (unsafe { GetConsoleMode(handle, &mut mode) } != 0).then_some(mode)
}

/// Whether `fd` is attached to a console.
pub fn is_tty(fd: i32) -> bool {
    match fd {
        STDIN_FD => io::stdin().is_terminal(),
        STDOUT_FD => io::stdout().is_terminal(),
        _ => false,
    }
}

fn size_of_handle(handle: HANDLE) -> Option<(u16, u16)> {
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
        return None;
    }
    let w = &info.srWindow;
    let rows = i32::from(w.Bottom) - i32::from(w.Top) + 1;
    let cols = i32::from(w.Right) - i32::from(w.Left) + 1;
    Some((
        rows.clamp(0, i32::from(u16::MAX)) as u16,
        cols.clamp(0, i32::from(u16::MAX)) as u16,
    ))
}

/// Visible window size `(rows, cols)` of the console `fd` belongs to. For
/// stdin (or a redirected stdout) the active screen buffer is opened as
/// `CONOUT$`.
pub fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    if let Some(handle) = std_handle(fd) {
        if fd == STDOUT_FD {
            if let Some(size) = size_of_handle(handle) {
                return Some(size);
            }
        }
    }
    if fd != STDIN_FD && fd != STDOUT_FD {
        return None;
    }
    let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let size = size_of_handle(handle);
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    size
}

fn set_vt_output(handle: HANDLE, old: u32) -> bool {
    let wanted = old | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    if unsafe { SetConsoleMode(handle, wanted | DISABLE_NEWLINE_AUTO_RETURN) } != 0 {
        return true;
    }
    // Hosts without DISABLE_NEWLINE_AUTO_RETURN still accept plain VT.
    unsafe { SetConsoleMode(handle, wanted) != 0 }
}

/// Stdout VT processing, restored on drop.
pub struct VtOutput {
    handle: HANDLE,
    old: u32,
}

// HANDLE is a raw pointer; console handles are process-global and usable
// from any thread.
unsafe impl Send for VtOutput {}

impl VtOutput {
    /// `None` when stdout is not a console or refuses VT processing.
    pub fn enable() -> Option<Self> {
        let handle = std_handle(STDOUT_FD)?;
        let old = console_mode(handle)?;
        set_vt_output(handle, old).then_some(Self { handle, old })
    }
}

impl Drop for VtOutput {
    fn drop(&mut self) {
        unsafe { SetConsoleMode(self.handle, self.old) };
    }
}

/// Raw console input (VT input, no line/echo/processed), with stdout VT
/// processing when stdout is a console. Restores both modes on drop.
pub struct RawMode {
    input: HANDLE,
    old_input: u32,
    output: Option<VtOutput>,
}

unsafe impl Send for RawMode {}

impl RawMode {
    pub fn enter(fd: i32) -> io::Result<Self> {
        Self::enter_with_mouse(fd, false)
    }

    /// Like [`RawMode::enter`]. With `mouse` the console is also taken out of
    /// Quick Edit mode and mouse input is enabled: in a classic conhost window
    /// Quick Edit swallows the mouse for text selection, so the VT mouse
    /// reports the client asks for (`?1000h`/`?1006h`) would never arrive.
    /// Windows Terminal is unaffected either way. All bits are restored on
    /// drop.
    pub fn enter_with_mouse(fd: i32, mouse: bool) -> io::Result<Self> {
        let input = std_handle(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a console handle"))?;
        let old_input = console_mode(input).ok_or_else(io::Error::last_os_error)?;
        let mut raw = (old_input
            & !(ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_WINDOW_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if mouse {
            // ENABLE_EXTENDED_FLAGS must be set for the Quick Edit bit to be
            // honoured at all.
            raw = (raw & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT;
        }
        if unsafe { SetConsoleMode(input, raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            input,
            old_input,
            output: VtOutput::enable(),
        })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { SetConsoleMode(self.input, self.old_input) };
        // `output` restores itself afterwards.
        let _ = self.output.take();
    }
}

/// Wait up to `timeout` for readable data on `handle`.
fn handle_readable(handle: HANDLE, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let file_type = unsafe { GetFileType(handle) };
    if console_mode(handle).is_some() {
        return console_readable(handle, deadline);
    }
    if file_type == FILE_TYPE_PIPE {
        return pipe_readable(handle, deadline);
    }
    // Regular files (and anything unknown) are always "readable"; a read
    // reports EOF or the error.
    let _ = FILE_TYPE_CHAR;
    true
}

fn remaining_ms(deadline: Instant) -> u32 {
    let left = deadline.saturating_duration_since(Instant::now());
    // Round up so a sub-millisecond remainder doesn't spin.
    (left.as_micros().div_ceil(1000)).min(u128::from(u32::MAX - 1)) as u32
}

fn console_readable(handle: HANDLE, deadline: Instant) -> bool {
    loop {
        match unsafe { WaitForSingleObject(handle, remaining_ms(deadline)) } {
            WAIT_OBJECT_0 => {}
            WAIT_FAILED => return true,
            _ => return false,
        }
        let mut records: [INPUT_RECORD; 32] = unsafe { std::mem::zeroed() };
        let mut count = 0u32;
        if unsafe { PeekConsoleInputW(handle, records.as_mut_ptr(), 32, &mut count) } == 0 {
            return true;
        }
        let has_key = records[..count as usize]
            .iter()
            .any(|r| r.EventType == KEY_EVENT as u16 && unsafe { r.Event.KeyEvent.bKeyDown } != 0);
        if has_key {
            return true;
        }
        // Only focus/mouse/resize/key-up records: they would keep the handle
        // signalled forever, so drop them and keep waiting.
        if count > 0 {
            let mut consumed = 0u32;
            if unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), count, &mut consumed) } == 0
            {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

fn pipe_readable(handle: HANDLE, deadline: Instant) -> bool {
    loop {
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        };
        if ok == 0 || available > 0 {
            // Broken pipe (EOF) or an error: a read will report it.
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Whether stdin has input waiting, waiting up to `timeout` for it.
pub fn stdin_readable(timeout: Duration) -> bool {
    match std_handle(STDIN_FD) {
        Some(handle) => handle_readable(handle, timeout),
        None => true,
    }
}

/// Exit code for a console control event.
pub fn exit_code_for_event(event: u32) -> i32 {
    if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
        EXIT_CONTROL_C
    } else {
        1
    }
}

struct Handler {
    callback: Arc<dyn Fn(u32) + Send + Sync>,
    done: Arc<(Mutex<bool>, Condvar)>,
}

static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);
static CAUGHT: AtomicI32 = AtomicI32::new(0);
/// How long a close/logoff/shutdown handler may block for cleanup; Windows
/// terminates the process shortly after anyway.
const CLOSE_WAIT: Duration = Duration::from_millis(4500);

unsafe extern "system" fn ctrl_handler(event: u32) -> i32 {
    let (callback, done) = {
        let guard = HANDLER.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(h) => (h.callback.clone(), h.done.clone()),
            None => return 0,
        }
    };
    let _ = CAUGHT.compare_exchange(
        0,
        exit_code_for_event(event),
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    callback(event);
    if event != CTRL_C_EVENT && event != CTRL_BREAK_EVENT {
        // The process dies when this returns: hold it until cleanup is done.
        let (lock, cv) = &*done;
        let mut finished = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + CLOSE_WAIT;
        while !*finished {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            finished = cv
                .wait_timeout(finished, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    1
}

/// Console control-event bridge. Only one may be installed at a time.
pub struct CleanupSignals {
    done: Arc<(Mutex<bool>, Condvar)>,
    installed: bool,
}

impl CleanupSignals {
    /// Install the handler. `callback(event)` runs on a Windows-created
    /// thread when Ctrl-C/Ctrl-Break/close/logoff/shutdown arrives.
    pub fn install(callback: impl Fn(u32) + Send + Sync + 'static) -> io::Result<Self> {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        {
            let mut guard = HANDLER.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "console cleanup handler already installed",
                ));
            }
            CAUGHT.store(0, Ordering::SeqCst);
            *guard = Some(Handler {
                callback: Arc::new(callback),
                done: done.clone(),
            });
        }
        if unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) } == 0 {
            let error = io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
            *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Err(error);
        }
        Ok(Self {
            done,
            installed: true,
        })
    }

    /// Uninstall; returns the exit code to finish with if an event was caught.
    pub fn finish(mut self) -> Option<i32> {
        self.uninstall();
        match CAUGHT.swap(0, Ordering::SeqCst) {
            0 => None,
            code => Some(code),
        }
    }

    fn uninstall(&mut self) {
        if !self.installed {
            return;
        }
        self.installed = false;
        unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 0) };
        *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let (lock, cv) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
}

impl Drop for CleanupSignals {
    fn drop(&mut self) {
        self.uninstall();
    }
}

/// Parse a `--signal` value. The wire stays Unix-numbered: TERM=15, INT=2,
/// KILL=9. Others are rejected with an explanatory message.
pub fn parse_signal_name(raw: &str) -> Result<i32, String> {
    let upper = raw.trim().trim_start_matches("SIG").to_ascii_uppercase();
    match upper.as_str() {
        "TERM" | "15" => Ok(15),
        "INT" | "2" => Ok(2),
        "KILL" | "9" => Ok(9),
        "HUP" | "QUIT" | "USR1" | "USR2" | "1" | "3" | "10" | "12" => Err(format!(
            "signal {raw:?} is not supported on Windows; use TERM, INT or KILL"
        )),
        _ => Err(format!(
            "unknown or unsupported signal {raw:?} on Windows; use TERM, INT or KILL"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    #[test]
    fn signal_names() {
        assert_eq!(parse_signal_name("term"), Ok(15));
        assert_eq!(parse_signal_name("SIGINT"), Ok(2));
        assert_eq!(parse_signal_name(" KILL "), Ok(9));
        assert_eq!(parse_signal_name("9"), Ok(9));
        for bad in ["HUP", "SIGQUIT", "usr1", "USR2", "1", "bogus"] {
            assert!(
                parse_signal_name(bad).unwrap_err().contains("Windows"),
                "{bad}"
            );
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code_for_event(CTRL_C_EVENT), EXIT_CONTROL_C);
        assert_eq!(exit_code_for_event(CTRL_BREAK_EVENT), EXIT_CONTROL_C);
        assert_eq!(exit_code_for_event(2), 1);
    }

    #[test]
    fn bad_fd_is_not_a_console() {
        assert!(!is_tty(7));
        assert!(terminal_size(7).is_none());
        assert!(RawMode::enter(7).is_err());
    }

    #[test]
    fn pipe_readiness() {
        let (mut r, mut w): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
        assert_ne!(unsafe { CreatePipe(&mut r, &mut w, ptr::null(), 0) }, 0);
        assert!(!handle_readable(r, Duration::from_millis(20)));
        let mut written = 0u32;
        assert_ne!(
            unsafe { WriteFile(w, b"x".as_ptr(), 1, &mut written, ptr::null_mut()) },
            0
        );
        assert!(handle_readable(r, Duration::from_millis(20)));
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(w);
        }
        // Writer closed: EOF counts as readable.
        let mut b = [0u8; 1];
        let mut n = 0u32;
        unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                r,
                b.as_mut_ptr(),
                1,
                &mut n,
                ptr::null_mut(),
            );
        }
        assert!(handle_readable(r, Duration::from_millis(20)));
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(r);
        }
    }

    #[test]
    fn cleanup_signals_install_once() {
        let first = CleanupSignals::install(|_| {}).expect("install");
        assert!(CleanupSignals::install(|_| {}).is_err());
        assert_eq!(first.finish(), None);
        let again = CleanupSignals::install(|_| {}).expect("reinstall");
        drop(again);
    }
}
