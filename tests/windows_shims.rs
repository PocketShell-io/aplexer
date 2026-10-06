#![cfg(windows)]
//! `a start` launches npm-style `.cmd` and `.ps1` shims under ConPTY, with
//! arguments surviving cmd.exe / PowerShell parsing intact.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        c.env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", self.runtime.path().join("config.toml"))
            .current_dir(self.work.path());
        c
    }

    fn start(&self, tag: &str, argv: &[&str]) {
        let out = self
            .command()
            .args(["--json", "start", "--tag", tag, "--"])
            .args(argv)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "start failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn kill(&self, tag: &str) {
        let _ = self
            .command()
            .args(["kill", tag])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{} never appeared", path.display());
}

#[test]
fn cmd_shim_runs_under_conpty_with_arguments_intact() {
    let h = Harness::new();
    let shim_dir = TempDir::new().unwrap();
    let shim = shim_dir.path().join("agent.cmd");
    std::fs::write(
        &shim,
        "@echo off\r\nset \"APX_A=%~1\"\r\nset \"APX_B=%~2\"\r\nset APX_> \"%~dp0out.txt\"\r\nping -n 30 127.0.0.1 >nul\r\n",
    )
    .unwrap();
    h.start("shimcmd", &[shim.to_str().unwrap(), "a&b|c", "100%"]);
    let got = wait_for(&shim_dir.path().join("out.txt"));
    h.kill("shimcmd");
    assert!(got.contains("APX_A=a&b|c"), "{got}");
    assert!(got.contains("APX_B=100%"), "{got}");
}

#[test]
fn ps1_shim_runs_under_conpty() {
    let h = Harness::new();
    let shim_dir = TempDir::new().unwrap();
    let shim = shim_dir.path().join("agent.ps1");
    std::fs::write(
        &shim,
        "$p = Join-Path $PSScriptRoot 'out.txt'\r\n[IO.File]::WriteAllLines($p, [string[]]$args)\r\nStart-Sleep 30\r\n",
    )
    .unwrap();
    h.start("shimps1", &[shim.to_str().unwrap(), "a b", "x&y"]);
    let got = wait_for(&shim_dir.path().join("out.txt"));
    h.kill("shimps1");
    assert_eq!(got.lines().collect::<Vec<_>>(), ["a b", "x&y"]);
}
