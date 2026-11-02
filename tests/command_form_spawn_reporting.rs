#![cfg(unix)]
//! What `a start`'s literal-command form reports when the workload dies
//! during startup (intake 20e5209b AC3).
//!
//! The intake reported the command form printing a spawn-failure message
//! ("Command failed to spawn: Aborted") for a session that actually started
//! and worked. No such message exists anywhere in aplexer -- and these tests
//! pin the contract that keeps it that way, in both directions:
//!
//! * a workload the OS killed with SIGABRT right after a successful spawn is
//!   a completed session, never a spawn failure: the start must succeed and
//!   print no spawn-failure/crash wording at all;
//! * a real spawn failure -- the binary does not exist -- must fail the start
//!   with a clear non-zero error naming the cause, and leave no session.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = state.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run aplexer CLI")
    }

    fn start_command_form(&self, workspace: &TempDir, tag: &str, workload: &[&str]) -> Output {
        let mut args = vec![
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
        ];
        args.extend_from_slice(workload);
        self.run(&args)
    }

    /// Live (worker-alive, non-terminal phase) rows for one workspace.
    fn live_rows(&self, workspace: &TempDir) -> Vec<Value> {
        let output = self.run(&["--json", "list"]);
        assert!(
            output.status.success(),
            "`a list` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot: Value = serde_json::from_slice(&output.stdout).expect("list JSON");
        let target = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace")
            .to_str()
            .expect("UTF-8 workspace")
            .to_string();
        snapshot
            .as_array()
            .expect("snapshot array")
            .iter()
            .filter(|row| {
                row["workspace"] == target
                    && row["worker_alive"] == true
                    && row["phase"] != "exited"
                    && row["phase"] != "failed"
            })
            .cloned()
            .collect()
    }

    /// Poll until the workspace holds no live session, so a self-retiring
    /// workload's cleanup is not a timing lottery.
    fn wait_until_gone(&self, workspace: &TempDir, tag: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let lingering = self
                .live_rows(workspace)
                .iter()
                .any(|row| row["tag"] == tag);
            if !lingering {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "session {tag} is still live after its workload died"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A workload the OS kills with SIGABRT immediately after a successful spawn
/// is a completed session, not a spawn failure: the start must succeed, and
/// its stderr must carry no spawn-failure or crash wording at all. This is
/// the anti-regression for the false "Command failed to spawn: Aborted"
/// report -- no aplexer code path may describe a signal-killed workload as a
/// failed spawn.
#[test]
fn signal_killed_workload_is_not_reported_as_spawn_failure() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let output = harness.start_command_form(
        &workspace,
        "abrt",
        &["--", "/bin/sh", "-c", "kill -s ABRT $$"],
    );
    assert!(
        output.status.success(),
        "a signal-killed workload was reported as a startup failure (status {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for forbidden in [
        "spawn",
        "Command failed",
        "Aborted",
        "worker exited during startup",
        "worker startup failed",
    ] {
        assert!(
            !stderr.contains(forbidden),
            "stderr falsely reports a spawn/crash failure for a signal-killed workload: {stderr:?}"
        );
    }
    // The session must not linger either: the workload died, so the worker
    // records the exit and retires the session on its own.
    harness.wait_until_gone(&workspace, "abrt");
}

/// The other AC3 direction: a workload that genuinely cannot be spawned (the
/// binary does not exist) must fail the start with a clear non-zero error
/// naming the cause, and must not create a session.
#[test]
fn missing_workload_binary_fails_with_clear_spawn_error() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let missing = workspace.path().join("no-such-binary-20e5209b");
    let output = harness.start_command_form(
        &workspace,
        "missing",
        &["--", missing.to_str().expect("UTF-8 path")],
    );
    assert!(
        !output.status.success(),
        "a start whose workload binary does not exist must fail, not start a dead session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("command is not executable or was not found in PATH"),
        "spawn failure error does not name the cause: {stderr:?}"
    );
    assert!(
        stderr.contains("no-such-binary-20e5209b"),
        "spawn failure error does not name the binary: {stderr:?}"
    );
    harness.wait_until_gone(&workspace, "missing");
}
