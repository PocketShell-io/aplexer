//! `a completions` must not panic when its reader leaves before the script.

use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

/// `a completions bash | head` used to panic: clap_complete unwraps every
/// write into its target, so a reader exiting early turned the broken pipe
/// into a `Result::unwrap()` panic instead of a clean exit. The read end
/// here is closed before the child can start, so its first write is
/// guaranteed to hit a dead pipe -- the deterministic version of that race.
#[test]
fn completions_exit_cleanly_when_the_reader_leaves_first() {
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    // Safety: the fd comes straight from `into_raw_fd` and is handed to
    // `Stdio`, which takes ownership of exactly that fd.
    let stdout = unsafe { Stdio::from_raw_fd(writer.into_raw_fd()) };
    let child = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["completions", "bash"])
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a completions bash failed against a closed pipe: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "panic leaked to stderr: {stderr}"
    );
}
