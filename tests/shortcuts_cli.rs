#![cfg(unix)]
//! Integration tests for configurable `a shortcut` management and `a <shortcut>`
//! create-or-attach behavior.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = tempfile::tempdir_in("/tmp").unwrap();
        let state = TempDir::new().unwrap();
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            // Keep default_engine non-shell in config tests so we prove missing
            // shortcut sessions start a `shell` session, not `default_engine`.
            .env("APLEXER_SHELL", "/bin/sh");
        command
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command.args(args);
        command.output().unwrap()
    }

    fn list_sessions(&self) -> Vec<Value> {
        let out = self.output(&["--json", "list"]);
        assert!(out.status.success(), "list failed: {out:?}");
        let val: Value = serde_json::from_slice(&out.stdout).unwrap();
        val.as_array().cloned().unwrap_or_default()
    }

    fn kill_session(&self, id: &str) {
        let out = self.output(&["kill", id]);
        assert!(out.status.success(), "kill failed: {out:?}");
    }
}

struct PtyRun {
    child: Child,
    master: File,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl PtyRun {
    fn spawn(harness: &Harness, args: &[&str]) -> Self {
        let (master, slave) = aplexer::open_pty(24, 80).unwrap();
        let mut command = harness.command();
        command.args(args);
        command.stdin(Stdio::from(slave.try_clone().unwrap()));
        command.stdout(Stdio::from(slave.try_clone().unwrap()));
        command.stderr(Stdio::from(slave.try_clone().unwrap()));
        let child = command.spawn().unwrap();
        drop(slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut reader = master.try_clone().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            master,
            captured,
        }
    }

    fn wait_for(&self, needle: &[u8], what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self.captured.lock().unwrap().clone();
            if out.windows(needle.len()).any(|window| window == needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {what} ({:?}); captured:\n{}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&out).replace('\x1b', "<ESC>")
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                thread::sleep(Duration::from_millis(50));
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("child did not exit in time");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn shortcut_cli_add_list_remove_and_validation() {
    let harness = Harness::new();
    let ws = TempDir::new().unwrap();
    let canonical_ws = aplexer::canonical_workspace(ws.path()).unwrap();

    // Empty list
    let out = harness.output(&["--json", "shortcut", "list"]);
    assert!(out.status.success());
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed, serde_json::json!({}));

    // Add dev -> <ws>:mgmt
    let out = harness.output(&[
        "--json",
        "shortcut",
        "add",
        "dev",
        ws.path().to_str().unwrap(),
        "--tag",
        "mgmt",
    ]);
    assert!(out.status.success(), "shortcut add failed: {out:?}");
    let added: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(added["name"], "dev");
    assert_eq!(added["workspace"], canonical_ws.to_string_lossy().as_ref());
    assert_eq!(added["tag"], "mgmt");

    // List shows dev
    let out = harness.output(&["shortcut"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("dev"));
    assert!(text.contains("mgmt"));

    // Reject reserved names
    for bad in ["list", "ls", "help", "123", "-bad"] {
        let out = harness.output(&["shortcut", "add", bad, ws.path().to_str().unwrap()]);
        assert!(!out.status.success(), "expected {bad:?} to be rejected");
    }

    // Remove dev
    let out = harness.output(&["--json", "shortcut", "remove", "dev"]);
    assert!(out.status.success());
    let out = harness.output(&["shortcut", "remove", "dev"]);
    assert!(!out.status.success());
}

#[test]
fn shortcut_starts_shell_when_missing_and_reattaches_when_present() {
    let harness = Harness::new();
    let ws = TempDir::new().unwrap();
    let canonical_ws = aplexer::canonical_workspace(ws.path()).unwrap();

    // Configure shortcut via config.toml (with a non-shell default_engine to
    // prove missing shortcut sessions start `shell`, not `default_engine`).
    fs::write(
        &harness.config,
        format!(
            "version = 1\ndefault_engine = \"claude\"\n\n[shortcuts.dev]\nworkspace = {:?}\ntag = \"mgmt\"\n\n[shortcuts.list]\nworkspace = {:?}\ntag = \"shadow\"\n",
            ws.path(),
            ws.path(),
        ),
    )
    .unwrap();

    // Subcommand `list` still takes precedence over `[shortcuts.list]`.
    assert!(harness.list_sessions().is_empty());

    // 1. Session missing: `a dev` starts a `shell` session in `ws` with tag `mgmt` and attaches.
    let mut first = PtyRun::spawn(&harness, &["dev"]);
    first.wait_for(b"mgmt", "status bar showing shortcut tag");
    first.send(&[0x02, b'd']);
    let status = first.wait_exit();
    assert!(status.success(), "first `a dev` failed: {status}");

    let sessions = harness.list_sessions();
    assert_eq!(sessions.len(), 1, "expected 1 session, got {sessions:?}");
    let id = sessions[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(sessions[0]["engine"], "shell");
    assert_eq!(sessions[0]["tag"], "mgmt");
    assert_eq!(
        sessions[0]["workspace"],
        canonical_ws.to_string_lossy().as_ref()
    );

    // 2. Session present: `a dev` attaches to the existing session instead of creating a new one.
    let mut second = PtyRun::spawn(&harness, &["dev"]);
    second.wait_for(b"mgmt", "status bar on reattach");
    second.send(&[0x02, b'd']);
    let status = second.wait_exit();
    assert!(status.success(), "second `a dev` failed: {status}");

    let sessions_after = harness.list_sessions();
    assert_eq!(sessions_after.len(), 1);
    assert_eq!(sessions_after[0]["id"], id);

    harness.kill_session(&id);
}
