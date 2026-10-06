#![cfg(windows)]
//! `a start` must not leak any handle of its launcher into the detached
//! worker (or the workload).
//!
//! Over SSH (sshd -> MSYS bash -> `aplexer start`) the client's stdio are
//! pipes. If the long-lived worker holds a duplicate of one, the SSH client
//! blocks until the session ends even though the CLI already exited. The
//! worker is created with an explicit `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, so
//! no inheritable handle of the launcher may reach it: not a std handle and
//! not an unrelated inheritable handle either.
//!
//! The two scenarios run sequentially inside one test: a handle that is
//! inheritable while another test spawns a process would leak into *that*
//! child and make the assertions race.

use std::fs::File;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::process::{Command, Stdio};
use std::ptr::null_mut;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::CreatePipe;

/// An anonymous pipe whose write end is inheritable.
fn inheritable_pipe() -> (File, OwnedHandle) {
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    assert_ne!(unsafe { CreatePipe(&mut read, &mut write, &sa, 0) }, 0);
    // Only the write end is meant to leak.
    assert_ne!(
        unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) },
        0
    );
    unsafe {
        (
            File::from_raw_handle(read as RawHandle),
            OwnedHandle::from_raw_handle(write as RawHandle),
        )
    }
}

struct Harness {
    runtime: TempDir,
    state: TempDir,
    cwd: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self {
            runtime: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
            cwd: TempDir::new().unwrap(),
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        c.env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", self.runtime.path().join("config.toml"))
            .current_dir(self.cwd.path());
        c
    }

    fn start_args(c: &mut Command, tag: &str) {
        c.args([
            "--json",
            "start",
            "--tag",
            tag,
            "--",
            "ping",
            "-n",
            "300",
            "127.0.0.1",
        ]);
    }

    fn kill(&self, tag: &str) {
        let _ = self
            .command()
            .args(["kill", tag])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Read to EOF on a helper thread; the result arrives within `timeout` or the
/// pipe is still held open by somebody.
fn read_to_eof(mut read: File, timeout: Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = read.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(timeout).ok()
}

#[test]
fn start_does_not_leak_inheritable_handles_into_the_worker() {
    let h = Harness::new();

    // 1. An inheritable handle that is not one of the CLI's std handles.
    let (read, write) = inheritable_pipe();
    let mut c = h.command();
    Harness::start_args(&mut c, "leak-extra");
    // `Command` always inherits every inheritable handle, so the CLI gets
    // `write` as a plain extra handle.
    let status = c
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .unwrap();
    drop(write);
    assert!(status.success(), "start failed: {status:?}");
    let eof = read_to_eof(read, Duration::from_secs(10));
    h.kill("leak-extra");
    assert!(
        eof.is_some(),
        "a non-std inheritable handle stayed open after `a start` exited: leaked into the worker"
    );

    // 2. The same pipe as the CLI's stdout (what sshd/bash hands a command).
    let (read, write) = inheritable_pipe();
    let mut c = h.command();
    Harness::start_args(&mut c, "leak-stdout");
    c.stdin(Stdio::null())
        .stdout(Stdio::from(write))
        .stderr(Stdio::null());
    let mut child = c.spawn().unwrap();
    // Dropping the Command releases the parent's copy of the write end.
    drop(c);
    assert!(child.wait().unwrap().success());
    let out = read_to_eof(read, Duration::from_secs(10));
    h.kill("leak-stdout");
    let out = out.expect("stdout pipe stayed open after `a start` exited: leaked into the worker");
    let json: Value = serde_json::from_slice(&out).expect("start JSON on stdout");
    assert_eq!(json["tag"], "leak-stdout");
    assert_eq!(json["phase"], "running");
}
