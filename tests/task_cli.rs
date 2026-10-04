//! `a task run` integration tests, driven entirely by fake child programs:
//! prompt argv preservation, actual success/failure status, timeout that only
//! ever kills the task's own process group, two-task isolation, real
//! parent/message binding through the mailbox, and the timezone-aware Berlin
//! cutoff routing.

// The shared Harness carries helpers other suites use that these tests do
// not; the module is declared for `paths`/`command`/`record_in` only.
#[allow(dead_code)]
#[path = "support/messaging.rs"]
mod support;

use aplexer::{atomic_write_json, task::sha256_hex};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use support::Harness;
use tempfile::TempDir;
use uuid::Uuid;

/// A fake engine that prints every argv element, its cwd, and selected env
/// vars, then exits with $FAKE_EXIT (default 0). The prompt is always the
/// final argv element, so `argv[last]=<prompt>` pins argv preservation.
const FAKE_ENGINE: &str = r#"#!/bin/sh
i=0
for a in "$@"; do
  echo "argv[$i]=$a"
  i=$((i+1))
done
echo "cwd=$PWD"
echo "session=${APLEXER_SESSION_ID:-none}"
echo "stripped=${MY_CUSTOM_STRIP:-none}"
echo "kept=${KEEP_ME:-none}"
exit ${FAKE_EXIT:-0}
"#;

/// A fake engine that ignores everything and just sleeps past any test
/// timeout, so the timeout path (group kill, exit 124) is what's exercised.
const SLEEPING_ENGINE: &str = "#!/bin/sh
sleep 30
exit 0
";

fn write_executable(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path.to_str().unwrap().to_string()
}

/// Harness with a config defining two fake engines over the given script:
/// `codex` (builtin codex-family task argv + skip-permissions flag) and
/// `antigravity` (config-provided `task_argv = ["-p"]`, proving the config
/// override path).
fn harness_with_fake_engine(script: &str) -> (Harness, TempDir, TempDir) {
    let harness = Harness::new();
    let engine_dir = TempDir::new().unwrap();
    let fake = write_executable(engine_dir.path(), "fake-engine", script);
    let config = format!(
        r#"
[engines.codex]
command = ["{fake}"]
skip_permissions_argv = ["--dangerously-bypass-approvals-and-sandbox"]
env_unset = ["MY_CUSTOM_STRIP"]

[engines.antigravity]
command = ["{fake}"]
task_argv = ["-p"]
"#
    );
    fs::write(harness.paths().config_file, config).unwrap();
    let cwd = TempDir::new().unwrap();
    (harness, engine_dir, cwd)
}

fn write_prompt(dir: &Path, body: &str) -> String {
    let path = dir.join("prompt.md");
    fs::write(&path, body).unwrap();
    path.to_str().unwrap().to_string()
}

fn task_run(harness: &Harness) -> Command {
    harness.command()
}

fn read_result(output_dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(output_dir.join("RESULT.json")).unwrap()).unwrap()
}

fn stdout_log(output_dir: &Path) -> String {
    fs::read_to_string(output_dir.join("stdout.log")).unwrap()
}

#[test]
fn prompt_reaches_child_argv_verbatim_and_success_exits_zero() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    let prompt = "delegated prompt with spaces 42";
    let prompt_path = write_prompt(cwd.path(), prompt);
    let output_dir = cwd.path().join("out-success");

    let run = task_run(&harness)
        .args([
            "--json",
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--env",
            "KEEP_ME=kept-value",
            "--env",
            "MY_CUSTOM_STRIP=must-not-reach-child",
            "--no-notify",
        ])
        .output()
        .unwrap();

    assert!(
        run.status.success(),
        "task run failed: {} {}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    let result = read_result(&output_dir);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["engine"], "codex");
    assert_eq!(result["notice"]["status"], "disabled");

    // The prompt arrived as the final argv element, byte-for-byte. ($@ in
    // the fake script excludes the program itself, so printed indices start
    // at the first argument after the engine executable.)
    let log = stdout_log(&output_dir);
    let last_argv = log
        .lines()
        .rfind(|line| line.starts_with("argv["))
        .expect("fake engine printed argv");
    assert_eq!(last_argv, format!("argv[3]={prompt}")); // exec --json --skip-git-repo-check + prompt
    assert!(log.contains("argv[0]=exec"));
    assert!(log.contains("argv[1]=--json"));
    assert!(log.contains("argv[2]=--skip-git-repo-check"));

    // The explicit env reached the child; the stripped one did not
    // (env_unset wins over explicit values, matching the worker's ordering).
    assert!(log.contains("kept=kept-value"), "{log}");
    assert!(log.contains("stripped=none"), "{log}");
    assert_eq!(
        result["cwd"],
        cwd.path().to_str().unwrap(),
        "child ran in the requested cwd"
    );

    // The record elides the prompt but fingerprints it.
    let argv: Vec<&str> = result["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!argv.iter().any(|a| a.contains(&prompt.to_string())));
    assert_eq!(
        argv.last().unwrap(),
        &format!(
            "<prompt: {} bytes, sha256={}>",
            prompt.len(),
            sha256_hex(prompt.as_bytes())
        )
    );
    assert_eq!(result["prompt_sha256"], sha256_hex(prompt.as_bytes()));

    // START.json was written before the child ran.
    assert!(output_dir.join("START.json").exists());
}

#[test]
fn task_failure_propagates_the_childs_actual_exit_code() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    let prompt_path = write_prompt(cwd.path(), "failing task prompt");
    let output_dir = cwd.path().join("out-failure");

    let run = task_run(&harness)
        .env("FAKE_EXIT", "3")
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "antigravity", // config task_argv = ["-p"]: prompt right after -p
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();

    assert_eq!(
        run.status.code(),
        Some(3),
        "the command's own exit code must be the child's actual exit code"
    );
    let result = read_result(&output_dir);
    assert_eq!(result["exit_code"], 3);
    assert_eq!(result["engine"], "antigravity");
    // antigravity task_argv from config: `-p` then the prompt, no codex flags.
    let log = stdout_log(&output_dir);
    let argv_lines: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("argv["))
        .collect();
    assert_eq!(
        argv_lines.join("\n"),
        "argv[0]=-p\nargv[1]=failing task prompt"
    );
}

#[test]
fn unspawnable_engine_task_exits_127_with_an_honest_result() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    // An engine whose executable does not exist: the child can never start.
    // The launcher must exit 127 -- never 0 -- so the hosting script sees a
    // launch that did not happen, and RESULT.json keeps the evidence.
    let config = format!(
        r#"
[engines.codex]
command = ["{}/no-such-engine"]
"#,
        cwd.path().display()
    );
    fs::write(harness.paths().config_file, config).unwrap();

    let prompt_path = write_prompt(cwd.path(), "never-starts prompt");
    let output_dir = cwd.path().join("out-unspawnable");
    let run = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();

    assert_eq!(
        run.status.code(),
        Some(127),
        "a child that never started must exit 127: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result = read_result(&output_dir);
    assert_eq!(result["exit_code"], 127);
    assert_eq!(result["timed_out"], false);
    assert!(result["error"].as_str().unwrap().contains("no-such-engine"));
    // The notice honestly records the failure instead of staying silent.
    assert_eq!(result["notice"]["status"], "failed");
    // START.json predates the spawn, so the launch facts survive.
    assert!(output_dir.join("START.json").exists());
    let summary = String::from_utf8_lossy(&run.stdout);
    assert!(summary.contains("exit 127"), "{summary}");
}

#[test]
fn timeout_kills_only_the_task_group_and_exits_124() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(SLEEPING_ENGINE);
    let prompt_path = write_prompt(cwd.path(), "sleeping task prompt");
    let output_dir = cwd.path().join("out-timeout");

    // A foreign long-running process the task must never touch.
    let mut foreign = Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn foreign sleep");
    let foreign_pid = foreign.id();

    let started = std::time::Instant::now();
    let run = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--timeout-secs",
            "2",
            "--no-notify",
        ])
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(run.status.code(), Some(124), "timeout must exit 124");
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "timeout must not wait for the full child sleep; took {elapsed:?}"
    );
    let result = read_result(&output_dir);
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["exit_code"], 124);

    // The unrelated foreign process survived the task's timeout kill.
    let foreign_alive = Command::new("kill")
        .args(["-0", &foreign_pid.to_string()])
        .status()
        .expect("probe foreign process")
        .success();
    assert!(foreign_alive, "foreign process must survive a task timeout");
    let _ = foreign.kill();
    let _ = foreign.wait();
}

#[test]
fn two_tasks_run_in_isolation_with_distinct_artifacts() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    let prompt_path = write_prompt(cwd.path(), "isolated prompt");

    for name in ["task-one", "task-two"] {
        let run = task_run(&harness)
            .args([
                "task",
                "run",
                "--prompt-file",
                &prompt_path,
                "--engine",
                "codex",
                "--no-skip-permissions",
                "--cwd",
                cwd.path().to_str().unwrap(),
                "--output-dir",
                cwd.path().join(name).to_str().unwrap(),
                "--no-notify",
            ])
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }

    let first = read_result(&cwd.path().join("task-one"));
    let second = read_result(&cwd.path().join("task-two"));
    assert_ne!(first["task_id"], second["task_id"], "distinct task ids");
    assert_ne!(first["output_dir"], second["output_dir"]);
    // Each output directory holds its own complete evidence set.
    for name in ["task-one", "task-two"] {
        let dir = cwd.path().join(name);
        for file in ["START.json", "RESULT.json", "stdout.log", "stderr.log"] {
            assert!(dir.join(file).exists(), "{name}/{file} missing");
        }
    }
    // A finished output directory is refused without --overwrite.
    let rerun = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            cwd.path().join("task-one").to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(!rerun.status.success());
    assert!(
        String::from_utf8_lossy(&rerun.stderr).contains("RESULT.json"),
        "refusal must name the completed run: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
}

#[test]
fn completion_notice_carries_the_real_parent_identity_to_the_target() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    // The calling session: a task hosted in a session whose record lives in
    // the task workspace (what `a start` gives a hosted task).
    let task_workspace = TempDir::new().unwrap();
    let parent = harness.record_in(task_workspace.path(), aplexer::Phase::Exited, None, b"");
    // The recipient: root's `main`-style session in another workspace.
    let root_workspace = TempDir::new().unwrap();
    let root = harness.record_in(root_workspace.path(), aplexer::Phase::Exited, None, b"");

    let prompt_path = write_prompt(cwd.path(), "noticing prompt");
    let output_dir = cwd.path().join("out-notice");

    let run = task_run(&harness)
        .env("APLEXER_SESSION_ID", parent.id.to_string())
        .env("APLEXER_WORKSPACE", task_workspace.path())
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--notify-workspace",
            root_workspace.path().to_str().unwrap(),
            "--notify-to",
            &root.tag,
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let result = read_result(&output_dir);
    // Real parent association: the calling session's own record, no flag.
    assert_eq!(result["parent_session"]["id"], parent.id.to_string());
    assert_eq!(result["parent_session"]["tag"], parent.tag);
    assert_eq!(
        result["parent_session"]["workspace"],
        task_workspace.path().to_str().unwrap()
    );
    // The notice was sent and its id recorded.
    assert_eq!(result["notice"]["status"], "sent");
    let notice_id = result["notice"]["message_id"].as_str().unwrap().to_string();

    // The durable envelope is in the root workspace mailbox, addressed to
    // the target, and *from* the parent session -- never anonymous, never a
    // faked --from.
    let log = task_run(&harness)
        .args([
            "--json",
            "message",
            "log",
            "--workspace",
            root_workspace.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    let messages: Vec<Value> = serde_json::from_slice(&log.stdout).unwrap();
    let notice = messages
        .iter()
        .find(|m| m["id"] == notice_id.as_str())
        .expect("notice envelope in the target workspace mailbox");
    assert_eq!(notice["kind"], "task-result");
    assert_eq!(notice["from"]["session_id"], parent.id.to_string());
    assert_eq!(notice["from"]["tag"], parent.tag);
    assert_eq!(
        notice["from"]["workspace"],
        task_workspace.path().to_str().unwrap()
    );
    assert_eq!(notice["to"]["tag"], root.tag);
    assert!(
        notice["body"].as_str().unwrap().contains("exit 0"),
        "body names the actual status: {}",
        notice["body"]
    );
    assert_eq!(
        notice["data"]["task_id"], result["task_id"],
        "notice data binds the envelope to the task record"
    );
    assert_eq!(
        notice["data"]["result_path"],
        output_dir.join("RESULT.json").to_str().unwrap()
    );
}

#[test]
fn notice_without_a_session_identity_is_skipped_not_faked() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    let prompt_path = write_prompt(cwd.path(), "anonymous prompt");
    let output_dir = cwd.path().join("out-no-identity");

    // No APLEXER_SESSION_ID of its own: a bare-terminal run. The task itself
    // still works; the notice is never sent and never faked. Which skip
    // classification applies depends on the host environment: a clean
    // environment has no session id at all ("no-session-identity"), while a
    // run inside a real session's process tree (as on this box) discovers an
    // ancestor stamp whose record does not exist in this state directory
    // ("identity-unresolved" -- the runner glue's identity-mismatch case).
    // Either way: no envelope, no sender, result retained.
    let run = task_run(&harness)
        .env_remove("APLEXER_SESSION_ID")
        .env_remove("APLEXER_WORKSPACE")
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result = read_result(&output_dir);
    assert_eq!(result["parent_session"], Value::Null);
    let status = result["notice"]["status"].as_str().unwrap();
    assert!(
        status == "no-session-identity" || status == "identity-unresolved",
        "expected an honest skip, got {status}"
    );
    assert!(result["notice"]["message_id"].is_null());
    assert!(result["notice"]["detail"].is_string());
    // No mailbox was written anywhere: the output dir holds exactly the
    // task artifacts.
    for entry in fs::read_dir(&output_dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            matches!(
                name.as_str(),
                "START.json" | "RESULT.json" | "stdout.log" | "stderr.log"
            ),
            "unexpected artifact {name}"
        );
    }
}

#[test]
fn cutoff_routes_new_launches_timezone_aware_never_midflight() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    let prompt_path = write_prompt(cwd.path(), "routed prompt");

    // The Berlin-cutoff instant with its explicit offset: past it, new
    // launches use the cutoff engine; before it, the requested engine. The
    // comparison is between absolute instants -- 01:00Z == 03:00+02:00.
    let run_after = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            cwd.path().join("after").to_str().unwrap(),
            "--cutoff",
            "2020-01-01T00:00:00Z", // long past
            "--cutoff-engine",
            "antigravity",
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(
        run_after.status.success(),
        "{}",
        String::from_utf8_lossy(&run_after.stderr)
    );
    assert_eq!(
        read_result(&cwd.path().join("after"))["engine"],
        "antigravity"
    );

    let run_before = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            cwd.path().join("before").to_str().unwrap(),
            "--cutoff",
            "2099-01-01T00:00:00+00:00", // far future
            "--cutoff-engine",
            "antigravity",
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(
        run_before.status.success(),
        "{}",
        String::from_utf8_lossy(&run_before.stderr)
    );
    assert_eq!(read_result(&cwd.path().join("before"))["engine"], "codex");

    // The offset spelling decides: 03:00+02:00 is the same instant as
    // 01:00Z, so a cutoff written in Berlin time and one written in UTC
    // route identically. (Both instants are genuinely in the past here --
    // unlike 2026-10-04T03:00+02:00, which from this session's launch
    // evening is still hours ahead.)
    let run_berlin = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            cwd.path().join("berlin-past").to_str().unwrap(),
            "--cutoff",
            "2026-10-03T00:00:00+02:00", // before this session even launched
            "--cutoff-engine",
            "antigravity",
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(
        run_berlin.status.success(),
        "{}",
        String::from_utf8_lossy(&run_berlin.stderr)
    );
    assert_eq!(
        read_result(&cwd.path().join("berlin-past"))["engine"],
        "antigravity"
    );

    // Half a routing pair is a hard error, not a silent fallback.
    let run_half = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            cwd.path().join("half").to_str().unwrap(),
            "--cutoff",
            "not-a-timestamp",
            "--cutoff-engine",
            "antigravity",
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(!run_half.status.success());
    assert!(
        String::from_utf8_lossy(&run_half.stderr).contains("RFC 3339"),
        "naive/malformed cutoff must fail loudly: {}",
        String::from_utf8_lossy(&run_half.stderr)
    );
}

#[test]
fn skip_permissions_argv_is_appended_by_default_and_opt_out_honored() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    let prompt_path = write_prompt(cwd.path(), "permissions prompt");

    let with_default = cwd.path().join("perm-default");
    let run = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            with_default.to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let log = stdout_log(&with_default);
    assert!(
        log.contains("argv[3]=--dangerously-bypass-approvals-and-sandbox"),
        "skip-permissions argv appended by default: {log}"
    );
    assert_eq!(
        log.lines().rfind(|l| l.starts_with("argv[")),
        Some("argv[4]=permissions prompt")
    );

    let opted_out = cwd.path().join("perm-optout");
    let run = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "codex",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            opted_out.to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(run.status.success());
    assert!(
        !stdout_log(&opted_out).contains("bypass-approvals"),
        "--no-skip-permissions suppresses the flag"
    );
}

#[test]
fn unknown_engine_refuses_rather_than_guessing_flags() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(FAKE_ENGINE);
    // A shell engine has no noninteractive mode and no config task_argv.
    let config = format!(
        r#"
[engines.codex]
command = ["{}"]
"#,
        write_executable(cwd.path(), "unused-fake", FAKE_ENGINE)
    );
    fs::write(harness.paths().config_file, config).unwrap();

    let prompt_path = write_prompt(cwd.path(), "shell prompt");
    let run = task_run(&harness)
        .args([
            "task",
            "run",
            "--prompt-file",
            &prompt_path,
            "--engine",
            "shell",
            "--no-skip-permissions",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--output-dir",
            cwd.path().join("shell-refusal").to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("task_argv"),
        "refusal must point at the config escape hatch: {stderr}"
    );
    // Nothing was launched, so no RESULT.json exists to mistake for a run.
    assert!(!cwd
        .path()
        .join("shell-refusal")
        .join("RESULT.json")
        .exists());
}

/// Keep one integration probe of the record round-trip through the actual
/// atomic writer used by the CLI (the unit tests cover the pure parts).
#[test]
fn task_record_round_trips_through_atomic_writer() {
    let harness = Harness::new();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rec").join("RESULT.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = serde_json::json!({ "task_id": Uuid::now_v7().to_string(), "exit_code": 0 });
    let written = aplexer::task::write_task_record(&path, &record).unwrap();
    assert_eq!(written, path);
    let read: Value = serde_json::from_str(&fs::read_to_string(&written).unwrap()).unwrap();
    assert_eq!(read, record);
    // PATHS harness kept alive for its temp cleanup ordering.
    let _ = atomic_write_json(&harness.paths().config_file, &record);
}
