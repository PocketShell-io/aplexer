//! Run a short CLI command with a deadline, capturing its output, in a test
//! binary that also keeps long-lived children (a pty-attached `a attach`)
//! running on other threads.
//!
//! Include with `#[path = "support/captured_command.rs"] mod captured_command;`.
//!
//! Output goes to anonymous temp files, not pipes, and completion is the
//! child's exit, not EOF. On macOS, std creates a capture pipe with `pipe()`
//! and only then marks it close-on-exec, so a sibling test thread that forks
//! in between inherits the write end. When that sibling is a long-lived
//! `a attach`, `wait_with_output` never sees EOF: the command has exited (a
//! zombie), yet the caller reports "did not finish within 15s". Files are
//! opened `O_CLOEXEC` atomically, and even a leaked descriptor would not hold
//! up a wait on the pid.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Output {
    let stdout = tempfile::tempfile().expect("stdout capture file");
    let stderr = tempfile::tempfile().expect("stderr capture file");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().expect("dup stdout capture")))
        .stderr(Stdio::from(stderr.try_clone().expect("dup stderr capture")));
    let mut child = cmd.spawn().expect("failed to spawn command");
    // The parent's copies of the child's stdio go with `cmd`.
    drop(cmd);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let pid = child.id();
                let _ = child.kill();
                let _ = child.wait();
                panic!("command (pid {pid}) did not finish within {timeout:?}");
            }
            Err(error) => panic!("failed to wait for command: {error}"),
        }
    };
    Output {
        status,
        stdout: read_back(stdout),
        stderr: read_back(stderr),
    }
}

fn read_back(mut file: File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0)).expect("rewind capture file");
    file.read_to_end(&mut bytes).expect("read capture file");
    bytes
}
