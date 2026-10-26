//! Unread mailbox waits: event-driven (inotify) on Linux, kqueue vnode
//! events on macOS, and a short-interval directory poll on Windows.

use super::*;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};

/// Return unread messages immediately, or wait until one arrives or `timeout`
/// expires. This does not acknowledge messages or change session state.
/// Subscribe before the first snapshot so an atomic publication cannot fall
/// between checking the mailbox and starting the watch.
///
/// Existing legacy mailboxes are migrated on entry by `ensure_workspace`.
/// Concurrent writes by older clients into the legacy mailbox do not wake
/// this stable-mailbox watch; use a current client for event-driven delivery.
pub fn wait_messages(
    paths: &Paths,
    workspace: &Path,
    consumer: &SessionIdentity,
    timeout: Duration,
) -> Result<Vec<MessageEnvelope>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("message wait timeout is too large"))?;
    let mp = retry_contention(deadline, || ensure_workspace_nonblocking(paths, workspace))?;
    let watch = MailboxWatch::subscribe(&mp.msgs_dir)?;
    loop {
        let messages = retry_contention(deadline, || unread_snapshot(&mp, workspace, consumer))?;
        if !messages.is_empty() || Instant::now() >= deadline {
            return Ok(messages);
        }
        // Events are only wake hints: unrelated recipients and queue overflow
        // both require a fresh, locked snapshot with the current cursor.
        watch.wait_for_change(deadline)?;
    }
}

fn unread_snapshot(
    mp: &MessagePaths,
    workspace: &Path,
    consumer: &SessionIdentity,
) -> Result<Vec<MessageEnvelope>> {
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(mp), true)?;
    let cursor = read_cursor_nonblocking(mp, consumer.id)?;
    let mut unread = Vec::new();
    for message in list_messages_in(mp, workspace)? {
        if consumer.receives(&message) && !cursor.is_acked(message.id) {
            unread.push(message);
        }
    }
    Ok(unread)
}

fn retry_contention<T>(deadline: Instant, mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                let contended = error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::WouldBlock);
                if !contended {
                    return Err(error);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error).context("message wait timed out acquiring mailbox state");
                }
                // flock has no unlock event. Only contention uses a short,
                // bounded retry; normal message arrival waits on inotify.
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
        }
    }
}

/// Windows has no inotify equivalent worth the complexity (ReadDirectoryChangesW
/// needs overlapped plumbing for a hint that is re-verified under lock anyway),
/// so the watch polls the mailbox directory's modification time. Events are
/// only wake hints, so an occasional spurious wake costs one locked rescan.
#[cfg(windows)]
struct MailboxWatch {
    directory: PathBuf,
    last: std::cell::Cell<Option<std::time::SystemTime>>,
}

#[cfg(windows)]
const WINDOWS_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Rescan at least this often even if the directory time did not move
/// (coarse filesystem timestamps), so no publication is ever missed.
#[cfg(windows)]
const WINDOWS_RESCAN_INTERVAL: Duration = Duration::from_millis(250);

#[cfg(windows)]
impl MailboxWatch {
    fn subscribe(directory: &Path) -> Result<Self> {
        let modified = fs::metadata(directory)
            .and_then(|metadata| metadata.modified())
            .ok();
        Ok(Self {
            directory: directory.to_path_buf(),
            last: std::cell::Cell::new(modified),
        })
    }

    fn wait_for_change(&self, deadline: Instant) -> Result<()> {
        let started = Instant::now();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            std::thread::sleep(remaining.min(WINDOWS_POLL_INTERVAL));
            let modified = fs::metadata(&self.directory)
                .and_then(|metadata| metadata.modified())
                .ok();
            if modified != self.last.get() {
                self.last.set(modified);
                return Ok(());
            }
            if started.elapsed() >= WINDOWS_RESCAN_INTERVAL {
                return Ok(());
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct MailboxWatch {
    fd: OwnedFd,
}

#[cfg(target_os = "linux")]
impl MailboxWatch {
    fn subscribe(directory: &Path) -> Result<Self> {
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error()).context("create mailbox watch");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let directory = CString::new(directory.as_os_str().as_bytes())?;
        let mask = libc::IN_MOVED_TO | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), directory.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error()).context("subscribe to mailbox publications");
        }
        Ok(Self { fd })
    }

    fn wait_for_change(&self, deadline: Instant) -> Result<()> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match self.poll(remaining) {
                Ok(false) => return Ok(()),
                Ok(true) => {
                    self.drain()?;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("wait for mailbox publication"),
            }
        }
    }

    fn poll(&self, remaining: Duration) -> io::Result<bool> {
        // Round upward because poll takes whole milliseconds. The caller
        // checks the monotonic deadline after every wake, including this cap.
        let millis = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("mailbox watch became unavailable"));
        }
        Ok(ready != 0)
    }

    fn drain(&self) -> Result<()> {
        // One bounded batch is enough for a wake hint. A continuous stream
        // must not keep us draining instead of scanning or checking timeout.
        let mut bytes = [0u8; 8192];
        let length =
            unsafe { libc::read(self.fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if length < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error).context("read mailbox watch events"),
            }
        }
        if length == 0 {
            bail!("mailbox watch closed unexpectedly");
        }
        check_events(&bytes[..length as usize])
    }
}

#[cfg(target_os = "linux")]
fn check_events(bytes: &[u8]) -> Result<()> {
    let mut offset = 0;
    let header = std::mem::size_of::<libc::inotify_event>();
    let invalid = libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT;
    while offset + header <= bytes.len() {
        let event = unsafe {
            bytes
                .as_ptr()
                .add(offset)
                .cast::<libc::inotify_event>()
                .read_unaligned()
        };
        if event.mask & invalid != 0 {
            bail!("mailbox watch was invalidated");
        }
        // IN_Q_OVERFLOW is a wake hint: rescan all retained envelopes.
        offset += header + event.len as usize;
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/wait.rs"]
mod tests;

/// macOS watch: kqueue `EVFILT_VNODE` on the mailbox directory, the direct
/// counterpart of the Linux inotify watch above. The directory fd stays open
/// for the watch's life (the vnode filter is tied to it); `NOTE_DELETE` /
/// `NOTE_REVOKE` invalidate the watch the same way `IN_IGNORED` does, and
/// everything else is a wake hint re-verified under lock by the caller.
///
/// Rename caveat: kevent does not reliably report the watched directory
/// itself being renamed, so every wake (and every poll slice) also stats
/// the path -- a vanished directory invalidates the watch.
#[cfg(target_os = "macos")]
struct MailboxWatch {
    kq: OwnedFd,
    #[allow(dead_code)]
    dir: OwnedFd,
    directory: PathBuf,
}

#[cfg(target_os = "macos")]
impl MailboxWatch {
    fn subscribe(directory: &Path) -> Result<Self> {
        let directory_buf = directory.to_path_buf();
        let directory = CString::new(directory.as_os_str().as_bytes())?;
        let raw_dir = unsafe { libc::open(directory.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if raw_dir < 0 {
            return Err(io::Error::last_os_error()).context("open mailbox directory for kqueue");
        }
        let dir = unsafe { OwnedFd::from_raw_fd(raw_dir) };
        let raw_kq = unsafe { libc::kqueue() };
        if raw_kq < 0 {
            return Err(io::Error::last_os_error()).context("create mailbox kqueue");
        }
        let kq = unsafe { OwnedFd::from_raw_fd(raw_kq) };
        let change = libc::kevent {
            ident: dir.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let registered = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if registered < 0 {
            return Err(io::Error::last_os_error()).context("subscribe to mailbox vnode events");
        }
        Ok(Self {
            kq,
            dir,
            directory: directory_buf,
        })
    }

    fn wait_for_change(&self, deadline: Instant) -> Result<()> {
        self.check_exists()?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            // Cap poll slices: kevent does not reliably report the watched
            // directory itself being renamed, so a vanished path is noticed
            // on the next slice at the latest instead of only at deadline.
            match self.poll(remaining.min(Duration::from_millis(250))) {
                Ok(false) => {
                    self.check_exists()?;
                }
                Ok(true) => {
                    self.drain()?;
                    self.check_exists()?;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("wait for mailbox publication"),
            }
        }
    }

    /// The rename/deletion probe kevent cannot be trusted with: a watched
    /// directory that no longer resolves invalidates the watch the same
    /// way `NOTE_DELETE` does.
    fn check_exists(&self) -> Result<()> {
        match fs::metadata(&self.directory) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => bail!("mailbox watch was invalidated"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                bail!("mailbox watch was invalidated")
            }
            Err(error) => Err(error).context("stat mailbox watch directory"),
        }
    }

    fn poll(&self, remaining: Duration) -> io::Result<bool> {
        let millis = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd: self.kq.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("mailbox watch became unavailable"));
        }
        Ok(ready != 0)
    }

    fn drain(&self) -> Result<()> {
        // One bounded batch is enough for a wake hint; a continuous stream
        // must not keep us draining instead of scanning or checking timeout.
        let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let count = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                &timeout,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error).context("read mailbox watch events"),
            }
        }
        if count == 0 {
            return Ok(());
        }
        check_vnode_events(&events[..count as usize])
    }
}

#[cfg(target_os = "macos")]
fn check_vnode_events(events: &[libc::kevent]) -> Result<()> {
    const INVALID: u32 = libc::NOTE_DELETE | libc::NOTE_REVOKE;
    for event in events {
        if event.fflags & INVALID != 0 {
            bail!("mailbox watch was invalidated");
        }
        // NOTE_WRITE / NOTE_EXTEND / NOTE_ATTRIB are wake hints: rescan
        // all retained envelopes.
    }
    Ok(())
}
