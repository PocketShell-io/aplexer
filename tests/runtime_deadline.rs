use aplexer::{now_ms, Paths};
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
fn deadline_workload() {
    let Ok(mode) = std::env::var("APLEXER_DEADLINE_TEST") else {
        return;
    };
    if mode == "late" {
        std::thread::sleep(Duration::from_millis(3500));
        return;
    }
    if mode == "quick" {
        return;
    }
    if mode == "leader" {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "deadline_workload", "--nocapture"])
            .env("APLEXER_DEADLINE_TEST", "descendant")
            .spawn()
            .unwrap()
            .wait()
            .unwrap();
    }
    std::thread::sleep(Duration::from_secs(60));
}

fn command(paths: &Paths) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aplexer"));
    cmd.env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file);
    cmd
}

#[test]
fn worker_deadline_survives_launcher_exit_and_persists_empty_containment() {
    let temp = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: temp.path().join("runtime"),
        state_root: temp.path().join("state"),
        config_file: temp.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let deadline = now_ms() + 2500;
    let output = command(&paths)
        .args(["start", "--workspace"])
        .arg(temp.path())
        .args([
            "--tag",
            "deadline-proof",
            "--engine",
            "shell",
            "--json",
            "--deadline-ms",
            &deadline.to_string(),
            "--env",
            "APLEXER_DEADLINE_TEST=leader",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "deadline_workload", "--nocapture"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["deadline_ms"], deadline);
    let id = record["id"].as_str().unwrap().parse().unwrap();
    // The start controller has exited. Only the native worker owns this timer.
    let until = Instant::now() + Duration::from_secs(15);
    let receipt = loop {
        if let Some(receipt) = aplexer::retired::read_finished_tombstone(&paths, id) {
            if receipt.containment_empty == Some(true) {
                break receipt;
            }
        }
        assert!(
            Instant::now() < until,
            "deadline failed to retire its containment"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(receipt.deadline_ms, Some(deadline));
    assert_eq!(receipt.deadline_exceeded, Some(true));
    assert!(receipt.exit.is_some());
    while paths.record(id).exists() {
        assert!(Instant::now() < until, "finished record was not removed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let reloaded = aplexer::retired::read_finished_tombstone(&paths, id).unwrap();
    assert_eq!(reloaded.deadline_exceeded, Some(true));
}

#[test]
fn expired_deadline_refuses_before_claim() {
    let temp = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: temp.path().join("runtime"),
        state_root: temp.path().join("state"),
        config_file: temp.path().join("config.toml"),
    };
    let output = command(&paths)
        .args(["start", "--workspace"])
        .arg(temp.path())
        .args([
            "--tag",
            "expired",
            "--engine",
            "shell",
            "--json",
            "--deadline-ms",
            "1",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("runtime deadline"));
}

#[test]
fn early_exit_preserves_deadline_without_exceeded_marker() {
    let temp = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: temp.path().join("runtime"),
        state_root: temp.path().join("state"),
        config_file: temp.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let deadline = now_ms() + 30_000;
    let output = command(&paths)
        .args(["start", "--workspace"])
        .arg(temp.path())
        .args([
            "--tag",
            "quick",
            "--engine",
            "shell",
            "--json",
            "--deadline-ms",
            &deadline.to_string(),
            "--env",
            "APLEXER_DEADLINE_TEST=quick",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "deadline_workload", "--nocapture"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = record["id"].as_str().unwrap().parse().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(receipt) = aplexer::retired::read_finished_tombstone(&paths, id) {
            assert_eq!(receipt.deadline_ms, Some(deadline));
            assert_eq!(receipt.deadline_exceeded, None);
            assert_eq!(receipt.containment_empty, Some(true));
            break;
        }
        assert!(Instant::now() < until, "early exit did not retire");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn delayed_watchdog_cannot_hide_a_natural_exit_after_deadline() {
    let temp = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: temp.path().join("runtime"),
        state_root: temp.path().join("state"),
        config_file: temp.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let deadline = now_ms() + 2000;
    let output = command(&paths)
        .env("APLEXER_TEST_DELAY_DEADLINE_WATCHDOG_MS", "30000")
        .args(["start", "--workspace"])
        .arg(temp.path())
        .args([
            "--tag",
            "late",
            "--engine",
            "shell",
            "--json",
            "--deadline-ms",
            &deadline.to_string(),
            "--env",
            "APLEXER_DEADLINE_TEST=late",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "deadline_workload", "--nocapture"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = value["id"].as_str().unwrap().parse().unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(receipt) = aplexer::retired::read_finished_tombstone(&paths, id) {
            assert_eq!(receipt.exit.as_ref().unwrap().code, Some(0));
            assert!(receipt.exit.as_ref().unwrap().exited_at_ms >= deadline);
            assert_eq!(receipt.deadline_exceeded, Some(true));
            assert_eq!(receipt.containment_empty, Some(true));
            break;
        }
        assert!(Instant::now() < until, "late natural exit not retired");
        std::thread::sleep(Duration::from_millis(30));
    }
}
