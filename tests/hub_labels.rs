#![cfg(unix)]
//! F5 AC5: Hub session binding labels.
//!
//! `a start --label KEY=VALUE` persists labels on the session record and
//! surfaces them in `list --json` / `snapshot` / `status --json`, so
//! PocketShell can render "Open in Hub" from aplexer metadata alone.
//! Hub marks managed sessions with `hub.managed=true`,
//! `hub.session=<hub-session-id>` and optionally `hub.task=<task-id>`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::{Builder, TempDir};

/// Runtime dir must stay short: aplexer binds
/// `<runtime>/sessions/<uuid>/control.sock` (59 bytes of suffix) and
/// `sun_path` caps at 108, so a deep pytest-style TMPDIR kills the worker
/// for reasons unrelated to the contract (same lesson as
/// pocketshell-cli/tests/test_aplexer_contract.py).
fn short_tempdir(prefix: &str) -> TempDir {
    for base in ["/dev/shm", "/tmp"] {
        if std::path::Path::new(base).is_dir() {
            if let Ok(dir) = Builder::new().prefix(prefix).tempdir_in(base) {
                // 59 = len("/sessions/") + 36 (uuid) + len("/control.sock")
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

fn start_with_labels(harness: &Harness, workspace: &Path, tag: &str, labels: &[&str]) -> Value {
    let mut args = vec![
        "--json",
        "start",
        "--workspace",
        workspace.to_str().unwrap(),
        "--tag",
        tag,
    ];
    for label in labels {
        args.push("--label");
        args.push(label);
    }
    args.extend(["--", "/bin/sh", "-c", "sleep 60"]);
    let stdout = harness.run_ok(&args);
    serde_json::from_str(&stdout).expect("start JSON")
}

#[test]
fn hub_labels_persist_and_surface_in_list_snapshot_status() {
    let harness = Harness::new();
    let workspace = harness.runtime_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let record = start_with_labels(
        &harness,
        &workspace,
        "hub-main",
        &[
            "hub.managed=true",
            "hub.session=task-hub-abc123",
            "hub.task=t-1",
        ],
    );
    let id = record["id"].as_str().expect("id").to_string();
    assert_eq!(record["tag"], "hub-main");
    assert_eq!(record["labels"]["hub.managed"], "true");
    assert_eq!(record["labels"]["hub.session"], "task-hub-abc123");
    assert_eq!(record["labels"]["hub.task"], "t-1");

    // list --json surfaces the same labels.
    let list_out = harness.run_ok(&["--json", "list"]);
    let rows: Vec<Value> = serde_json::from_str(&list_out).expect("list JSON");
    let row = rows.iter().find(|r| r["id"] == id).expect("row in list");
    assert_eq!(row["labels"]["hub.session"], "task-hub-abc123");

    // snapshot surfaces them too (PocketShell's first probe).
    let snap_out = harness.run_ok(&["--json", "snapshot"]);
    let snap_rows: Vec<Value> = serde_json::from_str(&snap_out).expect("snapshot JSON");
    let snap_row = snap_rows
        .iter()
        .find(|r| r["id"] == id)
        .expect("row in snapshot");
    assert_eq!(snap_row["labels"]["hub.managed"], "true");

    // status --json as well.
    let status_out = harness.run_ok(&["status", &id, "--json"]);
    let status: Value = serde_json::from_str(&status_out).expect("status JSON");
    // status merges the record; labels must survive the merge.
    let status_labels = status
        .get("labels")
        .or_else(|| status.get("record").and_then(|r| r.get("labels")));
    assert!(
        status_labels.is_some(),
        "status --json must carry labels: {status}"
    );

    // Cleanup.
    let kill = harness.run(&[
        "kill",
        "--workspace",
        workspace.to_str().unwrap(),
        "--tag",
        "hub-main",
    ]);
    assert!(
        kill.status.success(),
        "kill failed: {}",
        String::from_utf8_lossy(&kill.stderr)
    );
}

#[test]
fn unlabeled_sessions_carry_empty_labels_and_bad_labels_fail() {
    let harness = Harness::new();
    let workspace = harness.runtime_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    // No --label: record carries empty labels (absent or {}), never null garbage.
    let stdout = harness.run_ok(&[
        "--json",
        "start",
        "--workspace",
        workspace.to_str().unwrap(),
        "--tag",
        "plain",
        "--",
        "/bin/sh",
        "-c",
        "sleep 60",
    ]);
    let record: Value = serde_json::from_str(&stdout).expect("start JSON");
    let id = record["id"].as_str().unwrap().to_string();
    assert!(
        record
            .get("labels")
            .is_none_or(|l| l == &Value::Object(Default::default())),
        "unlabeled session must have empty labels, got {}",
        record["labels"]
    );

    // Bad labels fail loudly, no session created.
    for bad in ["no-equals-sign", "=empty-key", "bad key=x", "k=🔥"] {
        let output = harness.run(&[
            "start",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            "bad",
            "--label",
            bad,
            "--",
            "/bin/sh",
            "-c",
            "sleep 1",
        ]);
        assert!(
            !output.status.success(),
            "bad label {bad:?} must fail, got {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let kill = harness.run(&[
        "kill",
        "--workspace",
        workspace.to_str().unwrap(),
        "--tag",
        "plain",
    ]);
    assert!(kill.status.success());
    let _ = id;
}

#[test]
fn duplicate_label_keys_are_refused() {
    let harness = Harness::new();
    let workspace = harness.runtime_dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let output = harness.run(&[
        "start",
        "--workspace",
        workspace.to_str().unwrap(),
        "--tag",
        "dup",
        "--label",
        "hub.session=a",
        "--label",
        "hub.session=b",
        "--",
        "/bin/sh",
        "-c",
        "sleep 1",
    ]);
    assert!(!output.status.success(), "duplicate label keys must fail");
}
