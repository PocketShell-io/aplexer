#![cfg(unix)]

use serde_json::Value;
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn naturally_retired_output_survives_without_a_census_for_each_native_exit() {
    for (script, code, signal, expected) in [
        (
            "printf 'exit0-evidence\\n'; exit 0",
            Some(0),
            None,
            b"exit0-evidence\r\n".as_slice(),
        ),
        (
            "printf 'exit7-evidence\\n'; exit 7",
            Some(7),
            None,
            b"exit7-evidence\r\n".as_slice(),
        ),
        (
            "printf 'signal-evidence\\n'; kill -TERM $$",
            None,
            Some(15),
            b"signal-evidence\r\n".as_slice(),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        let launch = Command::new(env!("CARGO_BIN_EXE_aplexer"))
            .env("APLEXER_STATE_DIR", &state)
            .env("APLEXER_RUNTIME_DIR", &runtime)
            .env("APLEXER_CONFIG", root.path().join("no-config.toml"))
            .args([
                "--json",
                "start",
                "--env",
                "APLEXER_RETAIN_RETIRED_OUTPUT=1",
                "--workspace",
                workspace.to_str().unwrap(),
                "--tag",
                "hub-retirement-proof",
                "--",
                "/bin/sh",
                "-c",
                script,
            ])
            .output()
            .unwrap();
        assert!(
            launch.status.success(),
            "{}",
            String::from_utf8_lossy(&launch.stderr)
        );
        let launch: Value = serde_json::from_slice(&launch.stdout).unwrap();
        let id = launch["id"].as_str().unwrap();
        // No list/status/capture or host census between launch and receipt read.
        let file = state
            .join("retired-sessions")
            .join(id)
            .join("tombstone.json");
        let deadline = Instant::now() + Duration::from_secs(15);
        let receipt: Value = loop {
            if let Ok(data) = fs::read(&file) {
                break serde_json::from_slice(&data).unwrap();
            }
            assert!(Instant::now() < deadline, "receipt did not arrive: {id}");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(receipt["id"], id);
        assert_eq!(receipt["tag"], "hub-retirement-proof");
        assert_eq!(receipt["workspace"], workspace.to_str().unwrap());
        assert_eq!(receipt["created_at_ms"], launch["created_at_ms"]);
        assert_eq!(receipt["exit"]["code"], serde_json::json!(code));
        assert_eq!(receipt["exit"]["signal"], serde_json::json!(signal));
        assert_eq!(receipt["containment_empty"], true);
        assert!(
            receipt["exit"]["exited_at_ms"].as_u64().unwrap()
                >= receipt["created_at_ms"].as_u64().unwrap()
        );
        assert_eq!(receipt["output"]["status"], "complete");
        let bytes: Vec<u8> = serde_json::from_value(receipt["output"]["bytes"].clone()).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(receipt["output"]["observed_bytes"], bytes.len());
        let active = state.join("sessions").join(id);
        while active.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!active.exists(), "session must retire normally");
        println!("native exit proof: {}", receipt);
    }
}

#[test]
fn receipt_publication_retries_without_losing_memory_evidence() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    fs::create_dir(&state).unwrap();
    // A private test-owned file blocks creation of the receipt directory.
    let blocker = state.join("retired-sessions");
    fs::write(&blocker, b"test-only failure injection").unwrap();
    let launch = Command::new(env!("CARGO_BIN_EXE_aplexer"))
        .env("APLEXER_STATE_DIR", &state)
        .env("APLEXER_RUNTIME_DIR", root.path().join("runtime"))
        .env("APLEXER_CONFIG", root.path().join("no-config.toml"))
        .args([
            "--json",
            "start",
            "--env",
            "APLEXER_RETAIN_RETIRED_OUTPUT=1",
            "--workspace",
            root.path().to_str().unwrap(),
            "--tag",
            "receipt-retry",
            "--",
            "/bin/sh",
            "-c",
            "sleep 0.3; printf 'retry-evidence\\n'; exit 7",
        ])
        .output()
        .unwrap();
    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    let launch: Value = serde_json::from_slice(&launch.stdout).unwrap();
    let id = launch["id"].as_str().unwrap();
    thread::sleep(Duration::from_millis(800));
    let active = state.join("sessions").join(id);
    assert!(
        active.exists(),
        "failed publication must retain active state"
    );
    fs::remove_file(&blocker).unwrap();
    let file = state
        .join("retired-sessions")
        .join(id)
        .join("tombstone.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let receipt: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    assert_eq!(receipt["exit"]["code"], 7);
    assert_eq!(receipt["output"]["status"], "complete");
    let bytes: Vec<u8> = serde_json::from_value(receipt["output"]["bytes"].clone()).unwrap();
    assert_eq!(bytes, b"retry-evidence\r\n");
    while active.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!active.exists());
}

#[test]
fn ambient_environment_does_not_opt_unrelated_sessions_into_retention() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let launch = Command::new(env!("CARGO_BIN_EXE_aplexer"))
        .env("APLEXER_STATE_DIR", &state)
        .env("APLEXER_RUNTIME_DIR", root.path().join("runtime"))
        .env("APLEXER_CONFIG", root.path().join("no-config.toml"))
        .env("APLEXER_RETAIN_RETIRED_OUTPUT", "1")
        .args([
            "--json",
            "start",
            "--workspace",
            root.path().to_str().unwrap(),
            "--tag",
            "no-retention",
            "--",
            "/bin/sh",
            "-c",
            "printf ordinary",
        ])
        .output()
        .unwrap();
    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    let launch: Value = serde_json::from_slice(&launch.stdout).unwrap();
    let id = launch["id"].as_str().unwrap();
    let file = state
        .join("retired-sessions")
        .join(id)
        .join("tombstone.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let receipt: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    assert!(receipt.get("output").is_none());
    assert_eq!(receipt["exit"]["code"], 0);
}
