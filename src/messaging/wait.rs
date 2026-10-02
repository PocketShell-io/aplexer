//! Event-driven unread mailbox waits on Linux.

use super::*;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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

struct MailboxWatch {
    fd: OwnedFd,
}

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
