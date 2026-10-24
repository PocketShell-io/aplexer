#![cfg(target_os = "linux")]
use serde_json::Value;
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn retaining_stop_escalates_and_keeps_native_record_history_and_workspace() {
    let root = TempDir::new().unwrap();
    let runtime = TempDir::new_in(format!("/run/user/{}", unsafe { libc::geteuid() })).unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_aplexer"))
            .env("APLEXER_STATE_DIR", root.path().join("state"))
            .env("APLEXER_RUNTIME_DIR", runtime.path())
            .env("APLEXER_CONFIG", root.path().join("config.toml"))
            .args(args)
            .output()
            .unwrap()
    };
    let pid_file = workspace.join("tree-pids");
    let started = run(&["start", "--workspace", workspace.to_str().unwrap(), "--tag", "retained",
        "--engine", "shell", "--json", "--", "/bin/sh", "-c",
        "trap '' TERM; echo ready; setsid /bin/sh -c 'trap \"\" TERM; echo $$ > \"$1\"; sleep 300 & echo $! >> \"$1\"; wait' sh \"$1\" & wait", "sh", pid_file.to_str().unwrap()]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let row: Value = serde_json::from_slice(&started.stdout).unwrap();
    let id = row["id"].as_str().unwrap();
    let record_path = root
        .path()
        .join("state/sessions")
        .join(id)
        .join("session.json");
    let ready_deadline = Instant::now() + Duration::from_secs(2);
    while fs::read_to_string(&pid_file)
        .unwrap_or_default()
        .lines()
        .count()
        < 2
        && Instant::now() < ready_deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    let tree_pids: Vec<u32> = fs::read_to_string(&pid_file)
        .unwrap()
        .lines()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(tree_pids.len(), 2);
    let before: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    let begin = Instant::now();
    let stopped = run(&[
        "kill",
        id,
        "--signal",
        "TERM",
        "--grace-ms",
        "100",
        "--keep-record",
        "--json",
    ]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(begin.elapsed() < Duration::from_secs(8));
    assert!(begin.elapsed() >= Duration::from_millis(100));
    for pid in tree_pids {
        assert!(!aplexer::process_alive(pid), "descendant survived: {pid}");
    }
    for field in ["worker_pid", "workload_pid"] {
        if let Some(pid) = before[field].as_u64() {
            assert!(!aplexer::process_alive(pid as u32), "{} survived", field);
        }
    }
    let retained: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(retained["phase"], "exited");
    assert_eq!(retained["containment_empty"], true);
    assert_eq!(retained["created_at_ms"], before["created_at_ms"]);
    assert!(workspace.exists());
    assert!(record_path.with_file_name("history.bin").exists());
    assert!(run(&["list", "--json"]).status.success());
    assert!(record_path.exists(), "default list swept a retained stop");
    assert!(run(&["prune", "--json"]).status.success());
    assert!(record_path.exists(), "prune swept a retained stop");
    // An already-dead legacy worker can leave a stale "exiting" record;
    // UUID resolution still works after its workspace has disappeared.
    let mut stale = retained;
    stale["phase"] = serde_json::json!("exiting");
    stale["worker_pid"] = serde_json::Value::Null;
    stale["workload_pid"] = serde_json::Value::Null;
    fs::write(&record_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    fs::remove_dir_all(&workspace).unwrap();
    let reconciled = run(&["kill", id, "--keep-record", "--signal", "TERM", "--json"]);
    assert!(
        reconciled.status.success(),
        "{}",
        String::from_utf8_lossy(&reconciled.stderr)
    );
    let reconciled: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(reconciled["phase"], "exited");
    assert_eq!(reconciled["containment_empty"], true);
}
