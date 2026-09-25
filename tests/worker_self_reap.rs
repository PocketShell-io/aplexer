//! Issue #21: a live worker whose durable state is deleted out from under it
//! must self-reap instead of serving a socket no client can reach again.
//!
//! Integration tests start real workers with runtime/state scoped to
//! per-test `TempDir`s. When the test process exits -- including on an
//! assertion unwind -- both directories vanish and the worker is orphaned to
//! init. Until now nothing watched for "my durable record is gone": the
//! socket-recovery path refuses to republish without the record, so the
//! orphan served an unreachable socket forever, invisible to `a list` and
//! unkillable through the CLI.
//!
//! The contract pinned here, alongside `runtime_socket_recovery.rs`:
//!
//! * deleting the durable state under a live worker makes the worker kill
//!   its contained workload, drain, and exit on its own -- within seconds,
//!   with no `a kill` (there is no record left to resolve one anyway);
//! * deleting only the runtime dir keeps the worker alive (recovery), so
//!   this is a durable-state check, not a filesystem flinch.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    id: Option<String>,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
            id: None,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, Duration::from_secs(10))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait for command: {error}"),
        Err(_) => {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("command {pid} exceeded {timeout:?}");
        }
    }
}

fn process_gone(pid: u32) -> bool {
    // kill(pid, 0) probes liveness without signalling; ESRCH means the pid
    // is gone (a recycled pid inside this test's few-second window is not a
    // realistic shape on this machine's pid allocation).
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[test]
fn worker_self_reaps_when_its_durable_state_is_deleted_under_it() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let start = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "self-reap",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let started: Value = serde_json::from_slice(&start.stdout).unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    harness.id = Some(id.clone());

    let status = harness.run(&["status", &id, "--json"]);
    assert!(status.status.success(), "status RPC failed after start");
    let served: Value = serde_json::from_slice(&status.stdout).unwrap();
    let worker_pid = served["worker_pid"].as_u64().expect("worker_pid in status");
    let workload_pid = served["workload_pid"]
        .as_u64()
        .expect("workload_pid in status");
    let socket = PathBuf::from(served["socket_path"].as_str().unwrap());
    let runtime_session = socket.parent().unwrap().to_path_buf();
    assert!(!process_gone(worker_pid as u32), "worker must be live");
    assert!(!process_gone(workload_pid as u32), "workload must be live");

    // The TempDir-drop shape: the whole durable state for this session
    // disappears while the worker (and its workload) keep running.
    let durable_session = harness.state.path().join("sessions").join(&id);
    fs::remove_dir_all(&durable_session).unwrap();

    // No CLI surface can address the session any more; the only honest
    // observation is the process tree and the filesystem. The worker must
    // notice the vanished record on an idle tick, kill the contained
    // workload, and exit -- leaving neither process nor runtime dir behind.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if process_gone(worker_pid as u32)
            && process_gone(workload_pid as u32)
            && fs::symlink_metadata(&socket).is_err()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker {worker_pid} did not self-reap after its durable state was deleted \
             (workload gone: {}, socket gone: {})",
            process_gone(workload_pid as u32),
            fs::symlink_metadata(&socket).is_err()
        );
        thread::sleep(Duration::from_millis(50));
    }

    // The self-reap teardown removes the runtime session dir (socket already
    // gone above); nothing of this session may keep the harness's kill in
    // Drop from being a plain no-op.
    assert!(
        fs::symlink_metadata(&runtime_session).is_err(),
        "runtime session dir survived the self-reap"
    );
    let _ = harness.id.take();
}

/// The sibling contract: deleting only the runtime dir must NOT end the
/// worker -- that shape is the recovery path (`recover_control_socket`),
/// pinned end-to-end in `runtime_socket_recovery.rs`. Here it is pinned in
/// the cheap direction only: the worker is still alive afterwards, so a
/// regression turning this check into a filesystem flinch cannot hide.
#[test]
fn deleting_only_the_runtime_dir_does_not_self_reap_the_worker() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let start = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "runtime-only-delete",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let started: Value = serde_json::from_slice(&start.stdout).unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    harness.id = Some(id.clone());

    let status = harness.run(&["status", &id, "--json"]);
    let served: Value = serde_json::from_slice(&status.stdout).unwrap();
    let worker_pid = served["worker_pid"].as_u64().expect("worker_pid in status");
    let runtime_session = PathBuf::from(served["socket_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();

    fs::remove_dir_all(&runtime_session).unwrap();
    // Well past one idle tick (500 ms): a durable-state check that also
    // fired on runtime-dir deletion would have torn the worker down here.
    thread::sleep(Duration::from_millis(1_500));
    assert!(
        !process_gone(worker_pid as u32),
        "worker self-reaped over a runtime-dir-only deletion; \
         only a vanished durable record may end it"
    );
}
