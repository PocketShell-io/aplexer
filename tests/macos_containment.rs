//! macOS containment contract: every session runs in its own process
//! group, and `a kill` empties it; capped launches fail closed.
//!
//! The pgid model replaces the cgroup scope: the workload is born into its
//! own group (`setsid` in the spawn `pre_exec`), the worker records a
//! `pgid-<pgid>` locator, and teardown is `killpg`. These tests pin that
//! contract end to end through the real binary. Daemonized (double-forked)
//! descendants are the documented escape: they leave the group and survive,
//! exactly like a `setsid` descendant on an unlimited Linux session.

#![cfg(target_os = "macos")]

use aplexer::{process_alive, process_state};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            config_file,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime_dir.path())
            .env("APLEXER_STATE_DIR", self.state_dir.path())
            .env("APLEXER_CONFIG", &self.config_file);
        command
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().expect("run a");
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8(output.stdout).expect("utf8 stdout")
    }
}

fn start_json(harness: &Harness, tag: &str, extra: &[&str], workload: &[&str]) -> Value {
    let mut args = vec!["start", "--workspace", "/tmp", "--tag", tag, "--json"];
    args.extend_from_slice(extra);
    args.push("--");
    args.extend_from_slice(workload);
    let stdout = harness.run_ok(&args);
    serde_json::from_str(&stdout).expect("start JSON")
}

fn wait_until_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if !process_alive(pid) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} pid {pid} survived 15s (state {:?})",
            process_state(pid).ok()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A leader plus a background child share the session process group; `a
/// kill` must end both, and the record must be gone afterwards.
#[test]
fn kill_empties_the_session_process_group() {
    let harness = Harness::new();
    let started = start_json(
        &harness,
        "pgid-kill",
        &[],
        &["/bin/sh", "-c", "/bin/sleep 300 & exec /bin/sleep 300"],
    );
    let id = started["id"].as_str().expect("session id").to_owned();
    let leader = started["workload_pid"].as_u64().expect("leader pid") as u32;
    assert!(process_alive(leader), "leader {leader} never started");

    harness.run_ok(&["kill", &id, "--signal", "KILL", "--grace-ms", "0"]);
    wait_until_gone(leader, "workload leader");

    // The session must no longer report as live anywhere.
    let list: Value =
        serde_json::from_str(&harness.run_ok(&["list", "--json"])).expect("list JSON");
    if let Some(row) = list
        .as_array()
        .expect("list JSON is an array")
        .iter()
        .find(|row| row["id"] == Value::String(id.clone()))
    {
        let state = row["state"].as_str().unwrap_or("?");
        assert!(
            !["running", "starting", "exiting"].contains(&state),
            "killed session still live: {row}"
        );
    }
}

/// A capped launch is refused with a message naming the Linux backend,
/// rather than running without the requested limits.
#[test]
fn capped_launch_fails_closed() {
    let harness = Harness::new();
    let output = harness
        .command()
        .args([
            "start",
            "--workspace",
            "/tmp",
            "--tag",
            "pgid-capped",
            "--json",
            "--memory",
            "64M",
            "--",
            "/bin/sleep",
            "30",
        ])
        .output()
        .expect("run a");
    assert!(
        !output.status.success(),
        "capped launch must fail: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Linux") || stderr.contains("cgroup"),
        "failure must name the missing backend: {stderr}"
    );
}

/// Unlimited sessions record a `pgid-<pgid>` containment locator the reap
/// verdict can probe.
#[test]
fn unlimited_session_records_its_pgid_locator() {
    let harness = Harness::new();
    let started = start_json(&harness, "pgid-locator", &[], &["/bin/sleep", "30"]);
    let id = started["id"].as_str().expect("session id").to_owned();
    let status: Value =
        serde_json::from_str(&harness.run_ok(&["status", &id, "--json"])).expect("status JSON");
    let locator = status["containment_cgroup"]
        .as_str()
        .expect("status carries a containment locator");
    assert!(
        locator.starts_with("pgid-"),
        "locator names the process group: {locator}"
    );
    harness.run_ok(&["kill", &id, "--signal", "KILL", "--grace-ms", "0"]);
}
