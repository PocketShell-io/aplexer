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
//!   this is a durable-state check, not a filesystem flinch;
//! * deleting both, in the order a `TempDir`-scoped harness drops them
//!   (runtime field first, state second -- issue #22's orphan shape), still
//!   ends the worker: recovery republishes from the record while it lasts,
//!   and the record's own disappearance then ends the worker cleanly.
//! * deleting the durable state while the worker is still *starting* (the
//!   window before the vanished-record check is armed) must abort the
//!   start: the startup record writes are replacements, so they fail rather
//!   than resurrect the record, and the rollback leaves nothing behind
//!   (issue #22, the startup half of the same orphan shape).

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
    deleted_state_self_reaps(false);
}

#[test]
fn pending_history_after_status_cannot_revive_deleted_state() {
    deleted_state_self_reaps(true);
}

fn deleted_state_self_reaps(delay_status: bool) {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    if delay_status {
        // Exercise terminal-record persistence, not only clean auto-removal.
        fs::write(&harness.config, "keep_exited = true\n").unwrap();
    }
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

    if delay_status {
        thread::sleep(Duration::from_millis(350));
    }
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
    assert!(!durable_session.exists(), "durable state was resurrected");
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

/// Issue #22's orphan shape: the TempDir drop that orphans a test's worker
/// removes the runtime session dir first and the durable state second
/// (harness struct fields drop in declaration order). In between, recovery
/// legitimately republishes the socket from the still-present record -- so
/// this is not a contradiction of the sibling test above, but its sequel:
/// once the record goes too, the worker must still notice on an idle tick,
/// kill its contained workload, finalize, and exit, leaving neither process
/// nor session directory behind.
#[test]
fn worker_self_reaps_when_its_whole_footprint_is_deleted_in_tempdir_drop_order() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let start = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "tempdir-drop-order",
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
    let durable_session = harness.state.path().join("sessions").join(&id);
    assert!(!process_gone(worker_pid as u32), "worker must be live");
    assert!(!process_gone(workload_pid as u32), "workload must be live");

    fs::remove_dir_all(&runtime_session).unwrap();
    fs::remove_dir_all(&durable_session).unwrap();

    // Same observation limits as the durable-state test above: no CLI
    // surface can address the session any more, so the only honest
    // observations are the process tree and the filesystem.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if process_gone(worker_pid as u32)
            && process_gone(workload_pid as u32)
            && fs::symlink_metadata(&runtime_session).is_err()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker {worker_pid} did not self-reap after its runtime session dir and \
             durable state were both deleted (workload gone: {}, runtime dir gone: {})",
            process_gone(workload_pid as u32),
            fs::symlink_metadata(&runtime_session).is_err()
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!durable_session.exists(), "durable state was resurrected");
    let _ = harness.id.take();
}

/// Last-resort cleanup so a failing run cannot leak the processes it
/// started: the paused/aborted startup's worker and workload pids, plus the
/// `a start` client itself. Everything here is already gone on the green
/// path, so the kills are no-ops.
#[cfg(feature = "startup-test-hooks")]
struct StartupTestCleanup {
    pids: Vec<u32>,
    client: Option<std::process::Child>,
}

impl Drop for StartupTestCleanup {
    fn drop(&mut self) {
        if let Some(client) = &mut self.client {
            let _ = client.kill();
            let _ = client.wait();
        }
        for pid in self.pids.drain(..) {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// The startup half of the same contract (issue #22): the vanished-record
/// check is armed only once the serve loop begins, while bring-up -- the
/// workload spawn, placement, and history open between the worker-lock read
/// and the Running commit -- takes real time. A durable state deleted inside
/// that window used to be written back into existence by the startup's own
/// record writes (`atomic_write_json` recreates parent directories), and the
/// worker committed Running over a session whose state tree was gone: a
/// detached orphan serving a socket no client could ever address again.
///
/// Now every startup record write is a replacement of the launcher's record,
/// so the deletion fails the very next write; the start aborts, the worker
/// kills and reaps the workload it had already spawned, takes its runtime
/// artifacts back out, and exits -- without resurrecting the record, without
/// the rollback's `Failed` record, without the runtime dir.
///
/// The pause/resume startup hooks land the deletion squarely inside the
/// bring-up window instead of racing it; the launcher-side hook keeps the
/// `a start` client waiting for the worker's exit instead of readiness-
/// polling (and eventually TERM-ing) a paused worker.
#[cfg(feature = "startup-test-hooks")]
#[test]
fn deleted_durable_state_during_startup_aborts_the_worker_without_resurrection() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let startup_marker = harness.runtime.path().join("workload-spawned.marker");
    let resume_marker = harness.runtime.path().join("resume-startup.marker");

    let mut start = harness.command();
    start.args([
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "startup-self-reap",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    start.env(
        "APLEXER_TEST_PAUSE_WORKER_STARTUP_AT",
        "after_workload_spawn",
    );
    start.env("APLEXER_TEST_WORKER_STARTUP_MARKER", &startup_marker);
    start.env("APLEXER_TEST_RESUME_WORKER_STARTUP_FILE", &resume_marker);
    start.env("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL", "1");
    let client = start
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut cleanup = StartupTestCleanup {
        pids: Vec::new(),
        client: Some(client),
    };

    // The marker is written after the workload spawn and just before the
    // pause, so its existence pins the worker inside the bring-up window
    // with both durable registrations already written.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !startup_marker.exists() {
        assert!(
            Instant::now() < deadline,
            "worker never reached the after_workload_spawn pause point"
        );
        assert!(
            cleanup
                .client
                .as_mut()
                .expect("client held until the end")
                .try_wait()
                .unwrap()
                .is_none(),
            "start client exited before the worker paused"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let workload_pid: u32 = fs::read_to_string(&startup_marker)
        .unwrap()
        .trim()
        .parse()
        .expect("startup marker carries the workload pid");

    // The worker registered itself durably before spawning the workload; its
    // identity file names the process that is paused mid-startup here.
    let state_sessions = harness.state.path().join("sessions");
    let session_dir = fs::read_dir(&state_sessions)
        .unwrap()
        .next()
        .expect("one starting session under the state root")
        .unwrap()
        .path();
    let identity: Value = serde_json::from_str(
        &fs::read_to_string(session_dir.join("worker.identity.json")).unwrap(),
    )
    .unwrap();
    let worker_pid = identity["pid"].as_u64().expect("worker pid") as u32;
    cleanup.pids = vec![worker_pid, workload_pid];

    // The TempDir-drop shape, landed inside bring-up: the whole durable
    // state for this session disappears while the worker is still starting.
    fs::remove_dir_all(&session_dir).unwrap();
    fs::write(&resume_marker, b"resume").unwrap();

    // The next startup record write is a replacement and must fail: the
    // worker kills the workload it already spawned, takes its runtime
    // artifacts back out, and exits. It used to commit Running over the
    // resurrected record and live on as an orphan instead.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if process_gone(worker_pid) && process_gone(workload_pid) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker {worker_pid} stayed up after its durable state was deleted \
             mid-startup (workload gone: {})",
            process_gone(workload_pid)
        );
        thread::sleep(Duration::from_millis(20));
    }

    // Nothing of the session may come back: not the durable record the
    // startup writes used to resurrect, not the Failed record the rollback
    // used to persist over the deletion, not the runtime dir.
    assert!(
        fs::symlink_metadata(&session_dir).is_err(),
        "worker resurrected durable state deleted during startup"
    );
    let runtime_session = harness
        .runtime
        .path()
        .join("sessions")
        .join(session_dir.file_name().unwrap());
    assert!(
        fs::symlink_metadata(&runtime_session).is_err(),
        "runtime session dir survived the aborted startup"
    );

    // Wait the client out so it is reaped by this test, not leaked: with the
    // worker gone before readiness it reports the abort and exits on its own.
    let mut client = cleanup.client.take().expect("client held until the end");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match client.try_wait().unwrap() {
            Some(_) => break,
            None => assert!(
                Instant::now() < deadline,
                "start client did not exit after the worker aborted startup"
            ),
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = client.wait_with_output();
}
