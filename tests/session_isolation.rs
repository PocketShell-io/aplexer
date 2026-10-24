#![cfg(unix)]
//! F5 AC4 (aplexer side): the behaviors Hub's copied path logic must match.
//!
//! Hub duplicated `paths.rs` precedence (explicit > APLEXER_* > XDG > home)
//! in `bridge.py`, `retired_native.py`, `launcher_recovery.py`. These tests
//! pin the authoritative answers so the Hub copies (or their replacements
//! by aplexer calls) can be checked against the contract:
//! - same tag in two workspaces are distinct sessions;
//! - a stale incarnation (old UUID / old generation) never resolves to the
//!   new session;
//! - different state roots isolate registries entirely.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::{Builder, TempDir};

/// Runtime dir must stay short for AF_UNIX (see hub_labels.rs).
fn short_tempdir(prefix: &str) -> TempDir {
    for base in ["/dev/shm", "/tmp"] {
        if std::path::Path::new(base).is_dir() {
            if let Ok(dir) = Builder::new().prefix(prefix).tempdir_in(base) {
                if dir.path().to_string_lossy().len() + 59 < 108 {
                    return dir;
                }
            }
        }
    }
    panic!("no temp base short enough for an AF_UNIX control socket");
}

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = short_tempdir("apx-rt-");
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

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, Duration::from_secs(15))
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn start(&self, workspace: &Path, tag: &str) -> Value {
        let stdout = self.run_ok(&[
            "--json",
            "start",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            tag,
            "--",
            "/bin/sh",
            "-c",
            "sleep 60",
        ]);
        serde_json::from_str(&stdout).expect("start JSON")
    }

    fn kill(&self, workspace: &Path, tag: &str) {
        let output = self.run(&[
            "kill",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            tag,
        ]);
        assert!(
            output.status.success(),
            "kill {tag} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command pid {pid} exceeded timeout {timeout:?}");
        }
    }
}

#[test]
fn same_tag_in_two_workspaces_are_distinct_sessions() {
    let harness = Harness::new();
    let ws_a = harness.runtime_dir.path().join("a");
    let ws_b = harness.runtime_dir.path().join("b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    let rec_a = harness.start(&ws_a, "main");
    let rec_b = harness.start(&ws_b, "main");
    assert_ne!(rec_a["id"], rec_b["id"], "same tag must not share an id");

    // Scoped kills are independent: killing A leaves B running.
    harness.kill(&ws_a, "main");
    let list_out = harness.run_ok(&["--json", "list"]);
    let rows: Vec<Value> = serde_json::from_str(&list_out).expect("list JSON");
    assert!(
        rows.iter().all(|r| r["id"] != rec_a["id"]),
        "killed session must be gone"
    );
    assert!(
        rows.iter().any(|r| r["id"] == rec_b["id"]),
        "sibling workspace session must survive: {rows:?}"
    );

    // Scoped resolution is unambiguous: each workspace+tag names its own id.
    let status_b = harness.run_ok(&[
        "status",
        "--workspace",
        ws_b.to_str().unwrap(),
        "--tag",
        "main",
        "--json",
    ]);
    assert!(
        status_b.contains(rec_b["id"].as_str().unwrap()),
        "scoped status in B must name B's session: {status_b}"
    );

    harness.kill(&ws_b, "main");
}

#[test]
fn stale_incarnation_never_resolves_to_the_new_session() {
    let harness = Harness::new();
    let workspace = harness.runtime_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let first = harness.start(&workspace, "job");
    let first_id = first["id"].as_str().unwrap().to_string();
    let first_gen = first["created_at_ms"].as_u64().unwrap();
    harness.kill(&workspace, "job");

    // Wait for the old record to leave the registry.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let list_out = harness.run_ok(&["--json", "list", "--all"]);
        let rows: Vec<Value> = serde_json::from_str(&list_out).expect("list JSON");
        if rows.iter().all(|r| r["id"] != first_id) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "old record never left"
        );
        thread::sleep(Duration::from_millis(50));
    }

    // Reuse the tag: the new session is a different incarnation.
    thread::sleep(Duration::from_millis(5));
    let second = harness.start(&workspace, "job");
    let second_id = second["id"].as_str().unwrap().to_string();
    let second_gen = second["created_at_ms"].as_u64().unwrap();
    assert_ne!(first_id, second_id);
    assert!(
        second_gen >= first_gen,
        "generation must not go backwards: {first_gen} -> {second_gen}"
    );

    // The stale UUID no longer addresses anything live.
    let stale = harness.run(&["status", &first_id, "--json"]);
    if stale.status.success() {
        let value: Value =
            serde_json::from_str(&String::from_utf8_lossy(&stale.stdout)).expect("status JSON");
        let phase = value
            .get("phase")
            .and_then(|p| p.as_str())
            .unwrap_or("unknown");
        assert!(
            !["running", "starting"].contains(&phase),
            "stale id must never report live: {value}"
        );
    }
    // The live tag addresses the NEW id, never the old one.
    let live = harness.run_ok(&[
        "status",
        "--workspace",
        workspace.to_str().unwrap(),
        "--tag",
        "job",
        "--json",
    ]);
    // status --json for a workspace+tag prints the record or a status object;
    // either way the output must name the new session, not the stale one.
    assert!(
        live.contains(&second_id),
        "scoped status must name the new incarnation: {live}"
    );
    assert!(
        !live.contains(&first_id),
        "scoped status must not name the stale incarnation: {live}"
    );

    harness.kill(&workspace, "job");
}

#[test]
fn different_state_roots_isolate_registries() {
    let first = Harness::new();
    let second = Harness::new();
    let ws = first.runtime_dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let rec = first.start(&ws, "iso");
    let id = rec["id"].as_str().unwrap().to_string();

    // The second state root sees nothing of the first.
    let list_out = second.run_ok(&["--json", "list", "--all"]);
    let rows: Vec<Value> = serde_json::from_str(&list_out).expect("list JSON");
    assert!(
        rows.iter().all(|r| r["id"] != id),
        "isolated state root leaked a session: {rows:?}"
    );
    let missing = second.run(&["status", &id, "--json"]);
    assert!(
        !missing.status.success(),
        "isolated root must not resolve a foreign id"
    );

    first.kill(&ws, "iso");
}
