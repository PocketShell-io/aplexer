//! `a task handoff` integration tests, driven by fake engine programs (the
//! fake clock lives in the lib unit tests): opt-in enable, disabled-by-
//! default no-op fires, at-most-one launch per slot, repeat-disable
//! idempotence that removes only the owned state, a running task that
//! disable never cancels, foreign processes that survive, and cutoff
//! passthrough routing.

// The shared Harness carries helpers other suites use that these tests do
// not; the module is declared for `paths`/`command`/`record_in` only.
#[allow(dead_code)]
#[path = "support/messaging.rs"]
mod support;
#[allow(dead_code)]
#[path = "support/workload.rs"]
mod workload;

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::Harness;
use tempfile::TempDir;

/// Harness with fake `codex` (builtin task argv) and `antigravity`
/// (config-provided `task_argv`) engines.
fn harness_with_fake_engine(script: &str) -> (Harness, TempDir, TempDir) {
    let harness = Harness::new();
    let engine_dir = TempDir::new().unwrap();
    let fake = workload::toml_path(&workload::write_executable(engine_dir.path(), "fake-engine", script));
    let config = format!(
        r#"
[engines.codex]
command = ["{fake}"]
skip_permissions_argv = ["--dangerously-bypass-approvals-and-sandbox"]

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

fn handoff_dir(harness: &Harness) -> std::path::PathBuf {
    harness.paths().state_root.join("task-handoff")
}

fn task_dirs(cwd: &Path) -> Vec<std::path::PathBuf> {
    let root = cwd.join(".aplexer-tasks");
    let mut dirs: Vec<_> = fs::read_dir(&root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn sole_result(cwd: &Path) -> Value {
    let dirs = task_dirs(cwd);
    assert_eq!(dirs.len(), 1, "expected exactly one task dir in {cwd:?}");
    serde_json::from_str(&fs::read_to_string(dirs[0].join("RESULT.json")).unwrap()).unwrap()
}

fn status_json(harness: &Harness) -> Value {
    let output = harness
        .command()
        .args(["--json", "task", "handoff", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap()
}

#[test]
fn handoff_is_disabled_by_default_and_fire_is_a_noop() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));

    let status = harness
        .command()
        .args(["task", "handoff", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("disabled"), "unexpected status: {stdout}");

    let fire = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    // A schedule-less fire is a successful no-op: removing the plugin must
    // never break an existing timer entry.
    assert!(fire.status.success(), "fire errored: {fire:?}");
    assert!(String::from_utf8_lossy(&fire.stdout).contains("nothing to fire"));
    assert!(!handoff_dir(&harness).exists(), "no owned state may exist");
    assert!(
        task_dirs(cwd.path()).is_empty(),
        "a disabled handoff must launch nothing"
    );
    let status_json = status_json(&harness);
    assert_eq!(status_json["enabled"], false);
    assert_eq!(status_json["schedule"], Value::Null);
}

#[test]
fn enable_is_opt_in_and_fire_launches_once_per_slot() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "scheduled handoff prompt");

    // 00:00 local is always due for today's slot, whatever the wall clock.
    let enable = harness
        .command()
        .args([
            "--json",
            "task",
            "handoff",
            "enable",
            "--at",
            "00:00",
            "--prompt-file",
            &prompt,
            "--engine",
            "codex",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(
        enable.status.success(),
        "enable failed: {}",
        String::from_utf8_lossy(&enable.stderr)
    );

    // Opt-in wrote the owned schedule file with the saved task spec.
    let schedule: Value = serde_json::from_str(
        &fs::read_to_string(handoff_dir(&harness).join("schedule.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(schedule["at"], "00:00");
    assert_eq!(schedule["task"]["engine"], "codex");
    assert_eq!(schedule["task"]["prompt_file"], prompt.as_str());

    let status = status_json(&harness);
    assert_eq!(status["enabled"], true);
    assert!(
        status["next_due_secs"].is_u64(),
        "status must show next due"
    );

    let fire = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    assert!(
        fire.status.success(),
        "fire failed: {}",
        String::from_utf8_lossy(&fire.stderr)
    );
    let result = sole_result(cwd.path());
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["engine"], "codex");

    // Second fire in the same slot: successful no-op, no second launch.
    let fire_again = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    assert!(
        fire_again.status.success(),
        "repeat fire errored: {fire_again:?}"
    );
    assert!(
        String::from_utf8_lossy(&fire_again.stdout).contains("already fired"),
        "unexpected repeat fire output: {}",
        String::from_utf8_lossy(&fire_again.stdout)
    );
    assert_eq!(
        task_dirs(cwd.path()).len(),
        1,
        "at most one launch per slot"
    );
    assert_eq!(status_json(&harness)["fired"].as_array().unwrap().len(), 1);
}

#[test]
fn fire_before_the_daily_time_is_a_noop() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "not yet due");
    // Two minutes ahead of now; except within the midnight wrap window this
    // is strictly in the future, and the wrap is skipped explicitly below.
    let at = workload::date_shifted(2, workload::DateFmt::Hhmm);
    let now = workload::date_shifted(0, workload::DateFmt::Hhmm);
    if at <= now {
        return; // midnight wrap: +2 minutes rolled into the next day
    }

    let enable = harness
        .command()
        .args([
            "task",
            "handoff",
            "enable",
            "--at",
            &at,
            "--prompt-file",
            &prompt,
            "--engine",
            "codex",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(enable.status.success(), "enable failed: {enable:?}");

    let fire = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    assert!(
        fire.status.success(),
        "not-due fire must still exit 0: {fire:?}"
    );
    let stdout = String::from_utf8_lossy(&fire.stdout);
    assert!(
        stdout.contains("not due"),
        "unexpected fire output: {stdout}"
    );
    assert!(task_dirs(cwd.path()).is_empty(), "nothing may launch early");
    assert!(!handoff_dir(&harness).join("fired").exists());
}

#[test]
fn disable_removes_only_owned_state_and_is_idempotent() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "disable me twice");
    let enable = |extra: &[&str], at: &str| {
        let mut args = vec![
            "task",
            "handoff",
            "enable",
            "--at",
            at,
            "--prompt-file",
            prompt.as_str(),
            "--engine",
            "codex",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--no-notify",
        ];
        args.extend_from_slice(extra);
        harness.command().args(args).output().unwrap()
    };
    assert!(enable(&[], "00:00").status.success());

    // A fired slot plus foreign state that shares the parent directory.
    assert!(harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(task_dirs(cwd.path()).len(), 1);
    let foreign_file = harness.paths().state_root.join("unrelated-state.json");
    fs::write(&foreign_file, "keep me").unwrap();
    let foreign_dir = harness.paths().state_root.join("task-handoff-neighbor");
    fs::create_dir_all(&foreign_dir).unwrap();
    fs::write(foreign_dir.join("x"), "keep me too").unwrap();

    let disable = harness
        .command()
        .args(["task", "handoff", "disable"])
        .output()
        .unwrap();
    assert!(disable.status.success(), "disable failed: {disable:?}");
    assert!(String::from_utf8_lossy(&disable.stdout).contains("removed"));
    assert!(!handoff_dir(&harness).exists(), "owned state must be gone");
    assert!(foreign_file.exists(), "foreign state must survive disable");
    assert!(foreign_dir.join("x").exists(), "foreign dirs must survive");
    // Task evidence is never deleted by a disable.
    assert_eq!(task_dirs(cwd.path()).len(), 1, "task evidence must survive");

    // Repeat disable: idempotent success, still only a no-op.
    let disable_again = harness
        .command()
        .args(["task", "handoff", "disable"])
        .output()
        .unwrap();
    assert!(disable_again.status.success(), "repeat disable errored");
    let stdout = String::from_utf8_lossy(&disable_again.stdout);
    assert!(stdout.contains("already disabled"), "unexpected: {stdout}");
    assert_eq!(status_json(&harness)["enabled"], false);
    assert_eq!(task_dirs(cwd.path()).len(), 1);

    // After disable, a timer's fire is a no-op again.
    let fire = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    assert!(fire.status.success());
    assert_eq!(task_dirs(cwd.path()).len(), 1, "no launch after disable");
}

#[test]
fn disable_never_cancels_a_running_task_or_foreign_processes() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::sleeping_engine_script(4));
    let prompt = write_prompt(cwd.path(), "must complete naturally");
    let enable = harness
        .command()
        .args([
            "task",
            "handoff",
            "enable",
            "--at",
            "00:00",
            "--prompt-file",
            &prompt,
            "--engine",
            "codex",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(enable.status.success(), "enable failed: {enable:?}");

    // Launch via fire as a real background process.
    let mut fire_child = harness
        .command()
        .args(["task", "handoff", "fire"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let dirs = loop {
        let dirs = task_dirs(cwd.path());
        if dirs
            .first()
            .is_some_and(|dir| dir.join("START.json").exists())
        {
            break dirs;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "scheduled task never started"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(dirs.len(), 1);

    // A foreign long-running process alive across the disable.
    let mut foreign = workload::spawn_foreign_sleeper(60);

    // Disable while the task runs: it must not touch the task child or
    // anything else alive.
    let disable = harness
        .command()
        .args(["task", "handoff", "disable"])
        .output()
        .unwrap();
    assert!(disable.status.success(), "disable failed: {disable:?}");
    assert!(!handoff_dir(&harness).exists());
    assert!(
        foreign.try_wait().unwrap().is_none(),
        "disable killed an unrelated process"
    );

    // The in-flight task runs to natural completion and keeps its result.
    let status = fire_child.wait().unwrap();
    assert!(
        status.success(),
        "fire/task exited {status} — disable interfered with a running task"
    );
    let result = sole_result(cwd.path());
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["timed_out"], false);

    foreign.kill().unwrap();
    foreign.wait().unwrap();
}

#[test]
fn enable_without_at_launches_when_fired() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "no daily window");

    let enable = harness
        .command()
        .args([
            "task",
            "handoff",
            "enable",
            "--prompt-file",
            &prompt,
            "--engine",
            "codex",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(enable.status.success(), "enable failed: {enable:?}");

    let fire = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    assert!(fire.status.success(), "fire failed: {fire:?}");
    let result = sole_result(cwd.path());
    assert_eq!(result["exit_code"], 0);
    // Without a daily window the slot key carries the minute, visible in
    // the fired history.
    let fired = status_json(&harness)["fired"].as_array().unwrap().clone();
    assert_eq!(fired.len(), 1);
    assert!(
        fired[0].as_str().unwrap().contains('T'),
        "slot should be date + time-of-fire: {fired:?}"
    );
}

#[test]
fn cutoff_passthrough_routes_the_scheduled_launch() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "routed by cutoff");
    let past = workload::date_shifted(-60, workload::DateFmt::Iso8601Seconds);


    let enable = harness
        .command()
        .args([
            "task",
            "handoff",
            "enable",
            "--at",
            "00:00",
            "--prompt-file",
            &prompt,
            "--engine",
            "codex",
            "--cutoff",
            &past,
            "--cutoff-engine",
            "antigravity",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--no-notify",
        ])
        .output()
        .unwrap();
    assert!(enable.status.success(), "enable failed: {enable:?}");

    let fire = harness
        .command()
        .args(["task", "handoff", "fire"])
        .output()
        .unwrap();
    assert!(fire.status.success(), "fire failed: {fire:?}");
    // Routing is `a task run`'s own launch-time logic, fed by the schedule.
    let result = sole_result(cwd.path());
    assert_eq!(result["engine"], "antigravity");
}

#[test]
fn enable_rejects_bad_input_without_writing_state() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "never fired");
    let schedule = handoff_dir(&harness).join("schedule.json");
    let base = [
        "task",
        "handoff",
        "enable",
        "--engine",
        "codex",
        "--cwd",
        cwd.path().to_str().unwrap(),
        "--no-notify",
    ];

    let past_naive = workload::date_shifted(-60, workload::DateFmt::NaiveIso);


    let bad_cases: Vec<Vec<String>> = vec![
        // Malformed daily times.
        vec!["--at", "3:00", "--prompt-file", prompt.clone().as_str()]
            .into_iter()
            .map(String::from)
            .collect(),
        vec!["--at", "24:00", "--prompt-file", prompt.clone().as_str()]
            .into_iter()
            .map(String::from)
            .collect(),
        vec!["--at", "0300", "--prompt-file", prompt.as_str()]
            .into_iter()
            .map(String::from)
            .collect(),
        // Half a cutoff pair, and a naive (offset-less) cutoff.
        vec![
            "--prompt-file",
            prompt.as_str(),
            "--cutoff",
            past_naive.as_str(),
            "--cutoff-engine",
            "antigravity",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        vec![
            "--prompt-file",
            prompt.as_str(),
            "--cutoff",
            "2026-10-04T03:00:00+02:00",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        vec![
            "--prompt-file",
            prompt.as_str(),
            "--cutoff-engine",
            "antigravity",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        // Scheduling evidence locations is refused.
        vec![
            "--prompt-file",
            prompt.as_str(),
            "--output-dir",
            cwd.path().join("out").to_str().unwrap(),
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        // Unreadable prompt.
        vec!["--prompt-file", "/nonexistent/prompt.md"]
            .into_iter()
            .map(String::from)
            .collect(),
    ];
    for case in bad_cases {
        let mut args: Vec<&str> = base.to_vec();
        for arg in &case {
            args.push(arg);
        }
        let attempt = harness.command().args(&args).output().unwrap();
        assert!(
            !attempt.status.success(),
            "enable must refuse {args:?}: {}",
            String::from_utf8_lossy(&attempt.stdout)
        );
        assert!(!schedule.exists(), "refused enable must write no state");
        assert!(task_dirs(cwd.path()).is_empty());
    }
}

#[test]
fn enable_twice_replaces_the_schedule() {
    let (harness, _engine_dir, cwd) = harness_with_fake_engine(&workload::fake_engine_script(false));
    let prompt = write_prompt(cwd.path(), "replaceable");

    for (at, engine) in [("00:00", "codex"), ("01:00", "antigravity")] {
        let enable = harness
            .command()
            .args([
                "task",
                "handoff",
                "enable",
                "--at",
                at,
                "--prompt-file",
                &prompt,
                "--engine",
                engine,
                "--cwd",
                cwd.path().to_str().unwrap(),
                "--no-notify",
            ])
            .output()
            .unwrap();
        assert!(
            enable.status.success(),
            "enable {at}/{engine} failed: {enable:?}"
        );
    }
    let schedule: Value = serde_json::from_str(
        &fs::read_to_string(handoff_dir(&harness).join("schedule.json")).unwrap(),
    )
    .unwrap();
    // The last explicit enable wins; there is exactly one owned schedule.
    assert_eq!(schedule["at"], "01:00");
    assert_eq!(schedule["task"]["engine"], "antigravity");
}
