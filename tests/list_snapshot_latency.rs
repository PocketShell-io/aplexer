#![cfg(unix)]
//! Regression test for PocketShell-io/aplexer#65:
//! `a --json list` and `a --json snapshot` with N=50 sessions must finish in
//! under 1s (previously ~22s when `sweep_warnings` ran uncached, un-grepped
//! `journalctl` across dead records on every listing).

use aplexer::{atomic_write_json, Paths, SCHEMA_VERSION};
use serde_json::json;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

fn write_test_record(paths: &Paths, index: usize, phase: &str, now_ms: u64) -> Uuid {
    let id = Uuid::new_v4();
    std::fs::create_dir_all(paths.state_session(id)).expect("session state dir");
    let record = json!({
        "schema_version": SCHEMA_VERSION,
        "id": id,
        "workspace": format!("/tmp/ws-{}", index % 5),
        "tag": format!("s-{index}"),
        "engine": "shell",
        "command": ["/bin/sh"],
        "cwd": "/tmp",
        "env": {},
        "env_unset": [],
        "limits": {},
        "history_bytes": 65536,
        "created_at_ms": now_ms.saturating_sub(60_000),
        "updated_at_ms": now_ms.saturating_sub(30_000),
        "phase": phase,
        "worker_pid": 999999,
        "worker_cgroup": format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/run-u{}.scope",
            900_000 + index
        ),
        "socket_path": paths.socket(id),
        "history_path": paths.history(id),
    });
    atomic_write_json(&paths.record(id), &record).expect("write record");
    id
}

#[test]
fn list_and_snapshot_with_50_sessions_finish_under_1s() {
    let runtime = TempDir::new().expect("runtime tempdir");
    let state = TempDir::new().expect("state tempdir");
    let config = state.path().join("config.toml");
    let paths = Paths::discover_with(Some(runtime.path()), Some(state.path()), Some(&config))
        .expect("discover paths");
    paths.ensure().expect("ensure paths");
    std::fs::create_dir_all(paths.warnings_dir().join("acked")).expect("acked dir");

    let now_ms = aplexer::now_ms();
    for i in 0..50 {
        let phase = if i < 25 { "exited" } else { "running" };
        let id = write_test_record(&paths, i, phase, now_ms);
        // Pre-populate warning / acked tombstones for the broken records so
        // this measures steady-state `list` / `snapshot` latency without
        // shelling out to `systemctl`/`journalctl` 25 times.
        if (25..40).contains(&i) {
            std::fs::write(paths.warnings_dir().join(format!("{id}.json")), "{}")
                .expect("write warning");
        } else if i >= 40 {
            std::fs::write(
                paths
                    .warnings_dir()
                    .join("acked")
                    .join(format!("{id}.json")),
                "{}",
            )
            .expect("write acked");
        }
    }

    for subcommand in ["list", "snapshot"] {
        let start = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_aplexer"))
            .env("APLEXER_RUNTIME_DIR", runtime.path())
            .env("APLEXER_STATE_DIR", state.path())
            .env("APLEXER_CONFIG", &config)
            .args(["--json", subcommand])
            .output()
            .expect("run aplexer CLI");
        let elapsed = start.elapsed();
        assert!(
            output.status.success(),
            "`aplexer --json {subcommand}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "`aplexer --json {subcommand}` with 50 sessions took {elapsed:?} (limit: 1s)"
        );
    }
}
