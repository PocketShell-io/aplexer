#![cfg(windows)]
//! The Windows default shell is Git Bash (never the WSL launcher), started
//! login+interactive under ConPTY so `~/.bashrc` aliases load. Skipped with
//! a message when Git for Windows is not installed.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_aplexer");
const WAIT: Duration = Duration::from_secs(40);

struct Env {
    runtime: TempDir,
    state: TempDir,
    work: TempDir,
    home: TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            runtime: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        std::fs::write(
            env.home.path().join(".bashrc"),
            "alias zzprobe='echo ALIAS''OK'\n",
        )
        .unwrap();
        std::fs::write(
            env.runtime.path().join("config.toml"),
            "keep_exited = true\n",
        )
        .unwrap();
        env
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("APLEXER_RUNTIME_DIR", self.runtime.path());
        c.env("APLEXER_STATE_DIR", self.state.path());
        c.env("APLEXER_CONFIG", self.runtime.path().join("config.toml"));
        c.env("HOME", self.home.path());
        c.env_remove("APLEXER_SESSION");
        c.env_remove("APLEXER_SHELL");
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    fn start(&self, tag: &str, argv: &[&str]) -> String {
        let mut args = vec![
            "start",
            "--workspace",
            self.work.path().to_str().unwrap(),
            "--tag",
            tag,
            "--json",
        ];
        if !argv.is_empty() {
            args.push("--");
            args.extend_from_slice(argv);
        }
        let out = self.run(&args);
        assert!(out.status.success(), "start failed: {out:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["id"].as_str().unwrap().to_owned()
    }

    fn screen(&self, id: &str) -> String {
        let out = self.run(&["capture", id, "--screen"]);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn wait_screen(&self, id: &str, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let s = self.screen(id);
            if s.contains(needle) {
                return s;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {needle:?}; screen:\n{s}");
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn send(&self, id: &str, text: &str) {
        let out = self.run(&["send", id, text, "--enter"]);
        assert!(out.status.success(), "send failed: {out:?}");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let out = self.run(&["list", "--json"]);
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            let list = v
                .as_array()
                .cloned()
                .or_else(|| v["sessions"].as_array().cloned());
            for s in list.unwrap_or_default() {
                if let Some(id) = s["id"].as_str() {
                    let _ = self.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
                }
            }
        }
    }
}

fn git_bash_available() -> bool {
    match aplexer::bash_program() {
        Some(path) => !path.to_ascii_lowercase().contains("system32"),
        None => false,
    }
}

#[test]
fn default_shell_is_git_bash_with_aliases_ctrl_c_and_colour_env() {
    if !git_bash_available() {
        eprintln!("SKIP: Git for Windows bash is not installed");
        return;
    }
    let env = Env::new();
    let id = env.start("dflt", &[]);
    let record = env.run(&["status", &id, "--json"]);
    let status = String::from_utf8_lossy(&record.stdout).to_ascii_lowercase();
    assert!(status.contains("bash.exe"), "default is bash: {status}");

    // Prompt appears and it is bash (BASH_VERSION expands only in bash).
    env.send(&id, "echo IS-$((20+1))-${BASH_VERSION:+BASH}-X");
    env.wait_screen(&id, "IS-21-BASH-X");
    // TERM is set for ConPTY colour output; cwd is kept (CHERE_INVOKING).
    env.send(&id, "echo T=$TERM. W=$(pwd -W)");
    let s = env.wait_screen(&id, "T=xterm-256color.");
    let work = env.work.path().file_name().unwrap().to_string_lossy();
    assert!(s.contains(&*work), "cwd is the workspace ({work}): {s}");
    // ~/.bashrc was read (login+interactive).
    env.send(&id, "zzprobe");
    env.wait_screen(&id, "ALIASOK");
    // Ctrl-C interrupts a foreground command and the shell survives.
    env.send(&id, "sleep 60");
    thread::sleep(Duration::from_millis(800));
    let out = env.run(&["send", &id, "--hex", "03"]);
    assert!(out.status.success(), "{out:?}");
    // bash discards typeahead on SIGINT; let it redraw the prompt first.
    thread::sleep(Duration::from_millis(1500));
    env.send(&id, "echo ALIVE-$((40+2))");
    env.wait_screen(&id, "ALIVE-42");
}

#[test]
fn bashrc_alias_runs_as_launch_command() {
    if !git_bash_available() {
        eprintln!("SKIP: Git for Windows bash is not installed");
        return;
    }
    let env = Env::new();
    let id = env.start("alias", &["zzprobe"]);
    let deadline = Instant::now() + WAIT;
    loop {
        let out = env.run(&["capture", &id]);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.contains("ALIASOK") {
            return;
        }
        assert!(Instant::now() < deadline, "no ALIASOK in history: {text:?}");
        thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn unresolvable_shell_setting_names_the_variable() {
    let env = Env::new();
    let out = env
        .command()
        .env("APLEXER_SHELL", "definitely-not-a-shell")
        .args([
            "start",
            "--workspace",
            env.work.path().to_str().unwrap(),
            "--tag",
            "bad",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("APLEXER_SHELL"), "{err}");
    // Other engines are unaffected; doctor warns.
    let doctor = env
        .command()
        .env("APLEXER_SHELL", "definitely-not-a-shell")
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        text.contains("APLEXER_SHELL") && text.contains("not found"),
        "{text}"
    );
}

#[test]
fn shell_override_to_cmd_starts_cmd() {
    let env = Env::new();
    let out = env
        .command()
        .env("APLEXER_SHELL", "cmd")
        .args([
            "start",
            "--workspace",
            env.work.path().to_str().unwrap(),
            "--tag",
            "cmdsh",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(text.contains("cmd.exe"), "{text}");
}
