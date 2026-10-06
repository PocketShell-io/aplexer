#![cfg(windows)]
//! End-to-end Job Object limits on Windows: `--memory` OOM kill reporting,
//! `--pids`, status limit telemetry, and worker-crash tree teardown
//! (`KILL_ON_JOB_CLOSE`).

use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    work: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self {
            runtime: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aplexer"))
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", self.runtime.path().join("config.toml"))
            // Do not inherit an enclosing aplexer session's identity: with
            // APLEXER_WORKSPACE/SESSION_ID set (tests run from inside an agent
            // session), `status <tag>` resolves in the OUTER workspace.
            .env_remove("APLEXER_WORKSPACE")
            .env_remove("APLEXER_SESSION_ID")
            .env_remove("APLEXER_TAG")
            .current_dir(self.work.path())
            .args(args)
            .output()
            .expect("run aplexer")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "`a {}` failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn status(&self, tag: &str) -> Value {
        let out = self.run(&["status", tag, "--json"]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    fn wait_until(&self, what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("timed out waiting for {what}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for tag in ["mem", "lim", "crash"] {
            let _ = self.run(&["kill", tag]);
        }
    }
}

fn process_alive(pid: u32) -> bool {
    aplexer::sys::windows::job::process_alive(pid)
}

#[test]
fn memory_limit_reports_oom_like_linux() {
    let h = Harness::new();
    let script = "$l=New-Object System.Collections.ArrayList; for($i=0;$i -lt 60;$i++){ \
        $b=New-Object byte[] 20MB; for($j=0;$j -lt $b.Length;$j+=4096){$b[$j]=1}; \
        [void]$l.Add($b); Start-Sleep -Milliseconds 50 }; Start-Sleep 30";
    h.ok(&[
        "start",
        "--tag",
        "mem",
        "--memory",
        "200M",
        "--",
        "powershell",
        "-NoProfile",
        "-Command",
        script,
    ]);
    h.wait_until("oom exit record", || {
        h.status("mem")["exit"]["oom_killed"] == Value::Bool(true)
    });
    let status = h.status("mem");
    assert_eq!(status["state"], "exited");
    let warnings = h.ok(&["warnings"]);
    assert!(warnings.contains("oom"), "{warnings}");
}

#[test]
fn limits_are_reported_in_status() {
    let h = Harness::new();
    h.ok(&[
        "start",
        "--tag",
        "lim",
        "--memory",
        "512M",
        "--pids",
        "50",
        "--cpu-quota-us",
        "50000",
        "--",
        "cmd",
        "/c",
        "ping -n 60 127.0.0.1 >nul",
    ]);
    h.wait_until("limits in status", || {
        h.status("lim")["cgroup"]["pids_limit"] == 50
    });
    let cg = &h.status("lim")["cgroup"];
    assert_eq!(cg["memory_limit_bytes"], 512u64 << 20);
    assert_eq!(cg["cpu_quota_us"], 50_000);
    assert_eq!(cg["cpu_period_us"], 100_000);
    assert_eq!(cg["oom_kill_count"], 0);
}

#[test]
fn worker_crash_kills_the_workload_tree() {
    let h = Harness::new();
    h.ok(&[
        "start",
        "--tag",
        "crash",
        "--",
        "cmd",
        "/c",
        "ping -n 120 127.0.0.1 >nul",
    ]);
    h.wait_until("tree running", || {
        h.status("crash")["cgroup"]["active_processes"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    });
    let status = h.status("crash");
    let worker = status["worker_pid"].as_u64().unwrap() as u32;
    let workload = status["workload_pid"].as_u64().unwrap() as u32;
    let ids = aplexer::sys::windows::job::Job::open(status["id"].as_str().unwrap())
        .unwrap()
        .expect("job exists while the worker lives")
        .process_ids()
        .unwrap();
    assert!(ids.len() >= 2, "{ids:?}");
    let pinned = aplexer::sys::windows::job::PinnedProcess::open(worker)
        .unwrap()
        .unwrap();
    pinned.terminate(1).unwrap();
    h.wait_until("tree gone", || {
        !process_alive(workload) && ids.iter().all(|&pid| !process_alive(pid))
    });
    // The orphaned record is reapable and `a prune` removes it.
    h.wait_until("session listed broken", || {
        let list = h.ok(&["list"]);
        list.contains("broken") || list.contains("stopped")
    });
    h.ok(&["prune"]);
    assert!(!h.ok(&["list"]).contains("crash"));
}
