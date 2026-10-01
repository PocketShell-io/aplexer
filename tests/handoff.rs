//! CLI tests for `a handoff` (issue #20): the compact, read-only recovery
//! bundle. Synthetic records, native logs, and bind sidecars in isolated
//! APLEXER_* dirs + HOME -- never a real user's sessions or engine logs.

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    home: TempDir,
    config_file: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let home = TempDir::new().expect("home tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            home,
            config_file,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime_dir.path());
        cmd.env("APLEXER_STATE_DIR", self.state_dir.path());
        cmd.env("APLEXER_CONFIG", &self.config_file);
        cmd.env("HOME", self.home.path());
        cmd.env_remove("APLEXER_SESSION_ID");
        cmd
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut cmd = self.command();
        cmd.args(args);
        run_with_timeout(cmd, timeout)
    }

    fn run_ok(&self, args: &[&str]) -> std::process::Output {
        let output = self.run(args, Duration::from_secs(10));
        assert!(
            output.status.success(),
            "`a {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn a");
    let (tx, rx) = mpsc::channel();
    let pid_hint = child.id();
    thread::spawn(move || {
        let output = child.wait_with_output();
        let _ = tx.send(output);
    });
    match rx.recv_timeout(timeout) {
        Ok(output) => output.expect("wait a"),
        Err(_) => {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid_hint.to_string()])
                .status();
            panic!("`a` timed out after {timeout:?}");
        }
    }
}

/// A dead `shell` session (the issue's shape: engine shell, agent gone,
/// no transcript family of its own) with a persisted PTY tail.
fn write_dead_session(h: &Harness, id: &str, cwd: &Path) -> PathBuf {
    let session_dir = h.state_dir.path().join("sessions").join(id);
    fs::create_dir_all(&session_dir).unwrap();
    let socket_path = h
        .runtime_dir
        .path()
        .join("sessions")
        .join(id)
        .join("control.sock");
    let history_path = session_dir.join("history.bin");
    fs::write(
        &history_path,
        "weekly limit hit mid-insert\nSIGKILL'ed mid-redraw\n",
    )
    .unwrap();
    let record = json!({
        "schema_version": 1,
        "id": id,
        "workspace": cwd.display().to_string(),
        "tag": "zoom",
        "engine": "shell",
        "command": ["/bin/bash", "-l"],
        "cwd": cwd.display().to_string(),
        "env": {},
        "env_unset": [],
        "limits": {},
        "history_bytes": 4096,
        "created_at_ms": 1_000,
        "updated_at_ms": 1_000,
        "phase": "exited",
        "socket_path": socket_path,
        "history_path": history_path,
        "exit": {"code": null, "signal": 9, "oom_killed": false, "exited_at_ms": 2_000},
    });
    fs::write(
        session_dir.join("session.json"),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
    session_dir
}

/// A codex rollout naming `cwd`, with one user and one assistant turn.
fn write_rollout(path: &Path, cwd: &Path, answer: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            "{}\n{}\n{}\n",
            json!({"type": "session_meta", "payload": {"id": "thread", "cwd": cwd}}),
            json!({"type": "response_item", "payload": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "insert the figures"}]
            }}),
            json!({"type": "response_item", "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": answer}]
            }})
        ),
    )
    .unwrap();
}

/// Restores read-permissions on drop so TempDir cleanup cannot fail.
struct WritableGuard(PathBuf);
impl WritableGuard {
    fn read_only(dir: &Path) -> Self {
        let mut perms = fs::metadata(dir).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(dir, perms).unwrap();
        Self(dir.to_path_buf())
    }
}
impl Drop for WritableGuard {
    fn drop(&mut self) {
        let mut perms = fs::metadata(&self.0).unwrap().permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&self.0, perms);
    }
}

fn gap<'a>(gaps: &'a [Value], source: &str) -> &'a Value {
    gaps.iter()
        .find(|gap| gap["source"] == source)
        .unwrap_or_else(|| panic!("no {source} gap in {gaps:?}"))
}

#[test]
fn explicit_source_bundle_survives_an_unwritable_state_dir() {
    let h = Harness::new();
    let cwd = h.home.path().join("recap");
    fs::create_dir_all(&cwd).unwrap();
    let id = "00000000-0000-0000-0000-000000000001";
    let session_dir = write_dead_session(&h, id, &cwd);
    // The rollout lives outside ~/.codex: the caller located it themselves.
    let rollout = h.runtime_dir.path().join("rollout.jsonl");
    write_rollout(&rollout, &cwd, "weekly limit hit mid-insert");
    // A full/read-only state dir must not stop the report.
    let _guard = WritableGuard::read_only(&session_dir);

    let output = h.run_ok(&[
        "handoff",
        id,
        "--engine",
        "codex",
        "--path",
        rollout.to_str().unwrap(),
        "--json",
    ]);
    let bundle: Value = serde_json::from_slice(&output.stdout).unwrap();

    let transcript = &bundle["transcript"];
    assert_eq!(transcript["discovered"], json!(true));
    assert_eq!(transcript["source"], "explicit");
    assert_eq!(transcript["engine"], "codex");
    assert_eq!(transcript["native_session_id"], "thread");
    assert_eq!(transcript["last_user_message"], "insert the figures");
    assert_eq!(
        transcript["last_assistant_message"],
        "weekly limit hit mid-insert"
    );
    // Explicit sources never touch the bind sidecar -- here provably,
    // because the session dir could not have accepted a write.
    assert!(!session_dir.join("transcript.json").exists());
    // The dead worker's persisted PTY tail is the post-mortem evidence.
    let pty_tail = &bundle["pty_tail"];
    assert_eq!(pty_tail["source"], "persisted");
    assert!(pty_tail["text"]
        .as_str()
        .unwrap()
        .contains("SIGKILL'ed mid-redraw"));
    // Every evidence source reports its completeness explicitly.
    let gaps = bundle["gaps"].as_array().unwrap();
    assert_eq!(gaps.len(), 7);
    for source in [
        "record",
        "worker",
        "history",
        "transcript",
        "pty_tail",
        "screen",
        "staleness",
    ] {
        gap(gaps, source);
    }
    // The worker is gone: its gap says so instead of pretending liveness.
    assert_eq!(gap(gaps, "worker")["ok"], json!(false));
    assert_eq!(bundle["session"]["id"], id);
}

#[test]
fn bound_log_stays_usable_after_the_agent_exits() {
    let h = Harness::new();
    let cwd = h.home.path().join("recap");
    fs::create_dir_all(&cwd).unwrap();
    let id = "00000000-0000-0000-0000-000000000002";
    write_dead_session(&h, id, &cwd);
    let rollout = h.home.path().join(".codex/sessions/rollout.jsonl");
    write_rollout(&rollout, &cwd, "figures inserted");
    // The bind a previous live discovery wrote: exact path + the engine it
    // vouches for, so the shell record reads its codex log post-mortem.
    let session_dir = h.state_dir.path().join("sessions").join(id);
    fs::write(
        session_dir.join("transcript.json"),
        serde_json::to_vec_pretty(&json!({
            "path": rollout,
            "engine_session_id": "thread",
            "engine": "codex",
        }))
        .unwrap(),
    )
    .unwrap();

    let output = h.run_ok(&["handoff", id, "--json"]);
    let bundle: Value = serde_json::from_slice(&output.stdout).unwrap();

    let transcript = &bundle["transcript"];
    assert_eq!(transcript["discovered"], json!(true));
    assert_eq!(transcript["source"], "bind");
    assert_eq!(transcript["engine"], "codex");
    assert_eq!(transcript["native_session_id"], "thread");
    assert_eq!(transcript["last_user_message"], "insert the figures");
    assert_eq!(transcript["last_assistant_message"], "figures inserted");
    assert_eq!(transcript["events"].as_array().unwrap().len(), 2);
}

#[test]
fn missing_transcript_is_an_actionable_gap_not_a_failure() {
    let h = Harness::new();
    let cwd = h.home.path().join("recap");
    fs::create_dir_all(&cwd).unwrap();
    let id = "00000000-0000-0000-0000-000000000003";
    write_dead_session(&h, id, &cwd);

    let output = h.run_ok(&["handoff", id, "--json"]);
    let bundle: Value = serde_json::from_slice(&output.stdout).unwrap();

    let transcript = &bundle["transcript"];
    assert_eq!(transcript["discovered"], json!(false));
    let error = transcript["error"].as_str().unwrap();
    assert!(
        error.contains("--engine") && error.contains("--path"),
        "{error}"
    );
    let gaps = bundle["gaps"].as_array().unwrap();
    assert_eq!(gap(gaps, "transcript")["ok"], json!(false));
    // No worker: its gap says so instead of pretending liveness.
    assert_eq!(gap(gaps, "worker")["ok"], json!(false));
    // The persisted tail the dead worker left is still readable evidence.
    assert_eq!(gap(gaps, "pty_tail")["ok"], json!(true));
    assert_eq!(bundle["pty_tail"]["source"], "persisted");
}

#[test]
fn human_bundle_prints_sections_and_gaps() {
    let h = Harness::new();
    let cwd = h.home.path().join("recap");
    fs::create_dir_all(&cwd).unwrap();
    let id = "00000000-0000-0000-0000-000000000004";
    write_dead_session(&h, id, &cwd);

    let output = h.run_ok(&["handoff", id]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("handoff:"), "{text}");
    assert!(text.contains("zoom"), "{text}");
    assert!(text.contains("transcript"), "{text}");
    assert!(text.contains("pty tail"), "{text}");
    assert!(text.contains("gaps"), "{text}");
    assert!(
        text.contains("[gap] worker") || text.contains("[gap] "),
        "{text}"
    );
}
