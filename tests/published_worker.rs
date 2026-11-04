#![cfg(any(target_os = "linux", windows))]

use aplexer::{now_ms, Paths};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn published_workload() {
    if std::env::var_os("APLEXER_PUBLISHED_WORKLOAD").is_some() {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn command(binary: &Path, paths: &Paths) -> Command {
    let mut cmd = Command::new(binary);
    // No inherited worker override or session identity can rescue a broken
    // resolver, and no process in this test can reach the user's state.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("APLEXER_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file);
    cmd
}

struct Cleanup<'a> {
    paths: &'a Paths,
    id: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = command(Path::new(env!("CARGO_BIN_EXE_aplexer")), self.paths)
            .args(["kill", &self.id, "--keep-record"])
            .output();
    }
}

#[test]
fn published_names_reexec_the_same_worker_and_enforce_its_deadline() {
    #[cfg(target_os = "linux")]
    let names = [
        "aplexer-linux-amd64",
        "aplexer-linux-arm64",
        "a-linux-amd64",
        "a-linux-arm64",
    ];
    #[cfg(windows)]
    let names = [
        "aplexer-windows-amd64.exe",
        "aplexer-windows-arm64.exe",
        "a-windows-amd64.exe",
        "a-windows-arm64.exe",
    ];
    for name in names {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("no-config.toml"),
        };
        paths.ensure().unwrap();
        let binary = root.path().join(name);
        std::fs::copy(env!("CARGO_BIN_EXE_aplexer"), &binary).unwrap();
        // The old resolver selects this incompatible embedded host instead
        // of the copied release CLI. It fails deterministically, independent
        // of anything installed on PATH. The fixed resolver never runs it.
        let sibling = root.path().join(if cfg!(windows) {
            "aplexer.exe"
        } else {
            "aplexer"
        });
        std::fs::copy(std::env::current_exe().unwrap(), sibling).unwrap();
        let deadline = now_ms() + 3000;
        let output = command(&binary, &paths)
            .args(["start", "--workspace"])
            .arg(root.path())
            .args([
                "--tag",
                "published-worker",
                "--engine",
                "shell",
                "--json",
                "--deadline-ms",
                &deadline.to_string(),
                "--env",
                "APLEXER_PUBLISHED_WORKLOAD=1",
                "--",
            ])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "published_workload", "--nocapture"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        let record: Value = serde_json::from_slice(&output.stdout).unwrap();
        let id = record["id"].as_str().unwrap();
        let _cleanup = Cleanup {
            paths: &paths,
            id: id.to_owned(),
        };
        assert_eq!(record["deadline_ms"], deadline, "{name}");
        let id = id.parse().unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(receipt) = aplexer::retired::read_finished_tombstone(&paths, id) {
                assert_eq!(receipt.deadline_ms, Some(deadline), "{name}");
                assert_eq!(receipt.deadline_exceeded, Some(true), "{name}");
                assert_eq!(receipt.containment_empty, Some(true), "{name}");
                assert!(receipt.exit.is_some(), "{name}");
                if !paths.record(id).exists() {
                    break;
                }
            }
            assert!(
                Instant::now() < until,
                "{name}: deadline did not retire worker"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
