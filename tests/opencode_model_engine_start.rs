#![cfg(unix)]
//! `a start --engine <opencode> --model <model>` must produce a live session
//! running the requested model (intake 20e5209b AC2), end to end.
//!
//! The intake's engine form died because aplexer appended `--model <model>`
//! to opencode's argv, and opencode's top-level TUI command has no `--model`
//! flag: it printed usage, exited 1 within seconds, and the session vanished
//! from `a list` while `a start` still reported success. The fix delivers the
//! model through `OPENCODE_CONFIG_CONTENT` instead.
//!
//! The real opencode binary is not a test dependency: a fake executable named
//! `opencode` stands in for the 2.0.32 behavior the intake was filed against
//! -- `--model` in argv means instant death. The pre-fix launcher fails these
//! assertions (the workload exits before the readiness probe), the fixed one
//! keeps the session live with the model in the inline config.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().expect("run aplexer CLI");
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}): stdout={} stderr={}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run_ok(args);
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("`a {}` did not print JSON ({error})", args.join(" ")))
    }

    fn kill_ok(&self, id: &str) {
        self.run_ok(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    }
}

/// An executable named `opencode` (the file stem is what `apply_model` keys
/// on) that records its argv and the model config it was handed, exits 42 if
/// `--model` appears in argv -- the exact shape that killed the intake's
/// sessions -- and otherwise stays alive like a real TUI.
fn write_fake_opencode(dir: &Path, proof: &Path) -> PathBuf {
    let program = dir.join("fake-bin");
    fs::create_dir_all(&program).expect("create fake-bin dir");
    let script = program.join("opencode");
    let proof = proof.to_str().expect("UTF-8 proof path");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             {{\n\
             echo ARGV_BEGIN\n\
             for arg in \"$@\"; do echo \"argv:$arg\"; done\n\
             echo ARGV_END\n\
             echo \"MODEL_ENV:$OPENCODE_CONFIG_CONTENT\"\n\
             }} > '{proof}' 2>&1\n\
             for arg in \"$@\"; do\n\
             if [ \"$arg\" = \"--model\" ]; then exit 42; fi\n\
             done\n\
             exec sleep 60\n"
        ),
    )
    .expect("write fake opencode script");
    let mut permissions = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make script executable");
    script
}

fn wait_until_live(harness: &Harness, workspace: &TempDir, tag: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = harness.command(&["--json", "list"]).output().expect("list");
        let snapshot: Value = serde_json::from_slice(&output.stdout).expect("list JSON");
        let canonical = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace");
        let target = canonical.to_str().expect("UTF-8 workspace");
        let live = snapshot
            .as_array()
            .expect("snapshot array")
            .iter()
            .any(|row| {
                row["workspace"] == target && row["tag"] == tag && row["worker_alive"] == true
            });
        if live {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session {tag} never became live in `a list`"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The AC2 contract, end to end: the engine form with `--model` starts a
/// session that is live in `a list`, the model reaches the engine through
/// `OPENCODE_CONFIG_CONTENT`, and `--model` never appears in the workload
/// argv -- because for opencode that argv is what killed the session.
#[test]
fn engine_start_with_model_keeps_session_live_via_inline_config() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let proof = workspace.path().join("opencode-proof.txt");
    let script = write_fake_opencode(workspace.path(), &proof);
    let model = "opencode-go/muse-spark-1.3-contributor";
    fs::write(
        &harness.config,
        format!(
            "[engines.muse-fake]\n\
             command = [\"{}\"]\n\
             skip_permissions_argv = [\"--auto\"]\n",
            script.display()
        ),
    )
    .expect("write harness config");

    let record = harness.json(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "model-e2e",
        "--engine",
        "muse-fake",
        "--model",
        model,
    ]);
    assert_eq!(
        record["phase"], "running",
        "start did not return a live session: {record}"
    );
    let id = record["id"]
        .as_str()
        .expect("start JSON carries the session id")
        .to_string();

    // The intake's headline symptom: the session vanished from `a list`
    // within seconds of a successful-looking start.
    wait_until_live(&harness, &workspace, "model-e2e");

    let proof_text = fs::read_to_string(&proof).expect("fake opencode proof");
    assert!(
        !proof_text.contains("argv:--model"),
        "--model was appended to opencode's argv again (the intake's root cause): {proof_text:?}"
    );
    let expected_config = serde_json::json!({ "model": model }).to_string();
    assert!(
        proof_text.contains(&format!("MODEL_ENV:{expected_config}")),
        "model was not delivered through OPENCODE_CONFIG_CONTENT: {proof_text:?}"
    );

    harness.kill_ok(&id);
}

/// Without `--model`, the engine launch must stay untouched: no inline model
/// config is injected, and the session is live all the same.
#[test]
fn engine_start_without_model_starts_and_injects_nothing() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let proof = workspace.path().join("opencode-proof.txt");
    let script = write_fake_opencode(workspace.path(), &proof);
    fs::write(
        &harness.config,
        format!(
            "[engines.muse-fake]\n\
             command = [\"{}\"]\n\
             skip_permissions_argv = [\"--auto\"]\n",
            script.display()
        ),
    )
    .expect("write harness config");

    let record = harness.json(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "no-model",
        "--engine",
        "muse-fake",
    ]);
    assert_eq!(
        record["phase"], "running",
        "plain engine start did not return a live session: {record}"
    );
    let id = record["id"]
        .as_str()
        .expect("start JSON carries the session id")
        .to_string();
    wait_until_live(&harness, &workspace, "no-model");

    let proof_text = fs::read_to_string(&proof).expect("fake opencode proof");
    assert!(
        proof_text.contains("MODEL_ENV:"),
        "fake opencode was never started: {proof_text:?}"
    );
    assert!(
        !proof_text.contains("\"model\""),
        "model config was injected without --model: {proof_text:?}"
    );

    harness.kill_ok(&id);
}
