#![cfg(windows)]
//! Real-session checks for the Windows process-inspection features: the
//! foreground command (from the session Job), session discovery through
//! ancestor environments when `APLEXER_SESSION_ID` is cleared, `a rename`
//! from inside a session, and `cd` workspace tracking. Each test drives the
//! real binary against a real ConPTY worker in private state directories.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    scratch: TempDir,
    sessions: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        Self {
            runtime: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
            scratch: TempDir::new().unwrap(),
            sessions: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", self.runtime.path().join("config.toml"))
            .env_remove("APLEXER_SESSION_ID");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn start_cmd_shell(&mut self, workspace: &Path, tag: &str) -> String {
        let out = self.run(&[
            "--json",
            "start",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            tag,
            "--",
            "cmd.exe",
        ]);
        assert!(
            out.status.success(),
            "start: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let id = value["id"].as_str().expect("id in start json").to_owned();
        self.sessions.push(id.clone());
        id
    }

    fn status(&self, id: &str) -> serde_json::Value {
        let out = self.run(&["--json", "status", id]);
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
    }

    fn type_line(&self, id: &str, line: &str) {
        let out = self.run(&["send", id, &format!("{line}\r")]);
        assert!(out.status.success(), "send: {:?}", out);
    }

    fn wait_for(&self, what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            if ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("timed out waiting for {what}");
    }

    /// A batch file that runs `body` with every aplexer env var the harness
    /// uses re-exported (the session shell inherits them from the worker,
    /// but being explicit keeps the test independent of that) and
    /// `APLEXER_SESSION_ID` cleared, as an agent's tool subprocess would.
    fn cleared_env_script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.scratch.path().join(name);
        let text = format!(
            "@echo off\r\nset APLEXER_RUNTIME_DIR={}\r\nset APLEXER_STATE_DIR={}\r\nset APLEXER_CONFIG={}\r\nset APLEXER_SESSION_ID=\r\n{}\r\n",
            self.runtime.path().display(),
            self.state.path().display(),
            self.runtime.path().join("config.toml").display(),
            body
        );
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for id in &self.sessions {
            let _ = self.command().args(["kill", id]).output();
        }
    }
}

fn foreground(value: &serde_json::Value) -> Option<String> {
    value["foreground_command"].as_str().map(str::to_owned)
}

#[test]
fn foreground_command_tracks_the_deepest_live_process() {
    let mut h = Harness::new();
    let ws = h.scratch.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = h.start_cmd_shell(&ws, "fg");
    h.wait_for("idle shell foreground", || {
        foreground(&h.status(&id)).as_deref() == Some("cmd")
    });
    h.type_line(&id, "ping -n 60 127.0.0.1 >nul");
    h.wait_for("ping in the foreground", || {
        foreground(&h.status(&id)).as_deref() == Some("ping")
    });
    // The human `a status` line hides plain shells but names ping.
    let text = h.run(&["status", &id]);
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("foreground: ping"),
        "{}",
        String::from_utf8_lossy(&text.stdout)
    );
    h.type_line(&id, "\x03");
    h.wait_for("shell back in the foreground", || {
        foreground(&h.status(&id)).as_deref() == Some("cmd")
    });
}

#[test]
fn whoami_and_rename_find_the_session_through_cleared_environments() {
    let mut h = Harness::new();
    let ws = h.scratch.path().join("ws2");
    std::fs::create_dir_all(&ws).unwrap();
    let id = h.start_cmd_shell(&ws, "anc");
    h.wait_for("worker ready", || h.status(&id)["id"] == id.as_str());

    let exe = env!("CARGO_BIN_EXE_aplexer");
    let who = h.scratch.path().join("who.txt");
    let script = h.cleared_env_script(
        "who.cmd",
        &format!(
            "\"{exe}\" whoami > \"{who}\" 2>&1\r\n\"{exe}\" rename --tag renamed >> \"{who}\" 2>&1",
            who = who.display()
        ),
    );
    // A child cmd, so clearing the variable never touches the session shell
    // itself (which is the ancestor that still carries it).
    h.type_line(&id, &format!("cmd /c \"\"{}\"\"", script.display()));
    h.wait_for("rename through an ancestor", || {
        h.status(&id)["tag"] == "renamed"
    });
    let text = std::fs::read_to_string(&who).unwrap();
    assert!(text.contains(&id), "whoami output: {text}");
}

#[test]
fn cd_in_a_cmd_shell_moves_the_workspace() {
    let mut h = Harness::new();
    let ws = h.scratch.path().join("ws3");
    let other = h.scratch.path().join("elsewhere");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let id = h.start_cmd_shell(&ws, "cd");
    h.wait_for("worker ready", || h.status(&id)["id"] == id.as_str());
    h.type_line(&id, &format!("cd /d \"{}\"", other.display()));
    let want = other.canonicalize().unwrap();
    h.wait_for("workspace to follow cd", || {
        let workspace = h.status(&id)["workspace"].as_str().map(PathBuf::from);
        workspace.is_some_and(|w| {
            w.to_string_lossy()
                .eq_ignore_ascii_case(&want.to_string_lossy().replace(r"\\?\", ""))
        })
    });
}

#[test]
fn agent_is_detected_from_image_name_and_from_a_cmd_c_wrapper() {
    let mut h = Harness::new();
    let ws = h.scratch.path().join("ws4");
    std::fs::create_dir_all(&ws).unwrap();
    // A cmd.exe renamed claude.exe stands in for the agent binary: detection
    // by image name, with the PEB command line as the second source.
    let fake = h.scratch.path().join("claude.exe");
    std::fs::copy(r"C:\Windows\System32\cmd.exe", &fake).unwrap();
    let out = h.run(&[
        "--json",
        "start",
        "--workspace",
        ws.to_str().unwrap(),
        "--tag",
        "img",
        "--",
        fake.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = value["id"].as_str().unwrap().to_owned();
    h.sessions.push(id.clone());
    h.wait_for("claude detected by image name", || {
        h.status(&id)["agent"] == "claude"
    });

    // A plain cmd shell running `cmd /c ... codex.cmd`: found by command line.
    let id = h.start_cmd_shell(&ws, "wrap");
    h.wait_for("worker ready", || h.status(&id)["id"] == id.as_str());
    assert!(h.status(&id)["agent"].is_null());
    h.type_line(&id, "cmd /c \"ping -n 60 127.0.0.1 >nul & rem codex.cmd\"");
    h.wait_for("codex detected through cmd /c", || {
        h.status(&id)["agent"] == "codex"
    });
}
