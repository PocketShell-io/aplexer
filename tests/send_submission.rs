//! `a send --enter` against a real worker whose workload paints an agent
//! composer the way Claude Code does: `❯` prompt, cursor at the caret,
//! bracketed paste advertised. The emulated agent drains its input late (a
//! loaded event loop), and like Claude it folds a carriage return that
//! arrives in the same read as typed text into the draft as a newline -- the
//! reported "exit 0, pty_written, unsent multiline draft" failure. Every
//! submission is appended to a log file, so the assertions see what the
//! agent actually took, not what aplexer wrote.
#![cfg(unix)]

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const COMPOSER: &str = r#"
import os, sys, time, tty
log = sys.argv[1]
tty.setraw(0)
draft, pasting = b"", False
def paint():
    lines = draft.split(b"\n")
    if len(lines) > 10:
        lines = [b"[Pasted text +%d lines]" % len(lines)]
    out = b"\x1b[?2004h\x1b[2J\x1b[1;1Hagent ready\x1b[3;1H" + b"-" * 40
    for i, line in enumerate(lines):
        out += b"\x1b[%d;1H%s %s" % (4 + i, "❯".encode() if i == 0 else b" ", line)
    out += b"\x1b[%d;1H" % (4 + len(lines)) + b"-" * 40
    out += b"\x1b[%d;%dH" % (3 + len(lines), 3 + len(lines[-1].decode(errors="replace")))
    os.write(1, out)
def submit():
    global draft
    with open(log, "ab") as f:
        f.write(draft.hex().encode() + b"\n")
    draft = b""
paint()
while True:
    time.sleep(0.4)  # a busy agent drains its PTY late, a few KiB per read
    data, typed = os.read(0, 4096), False
    while data:
        if pasting:
            body, end, data = data.partition(b"\x1b[201~")
            draft += body.replace(b"\r", b"\n")
            pasting = not end
        elif data.startswith(b"\x1b[200~"):
            pasting, data = True, data[6:]
        elif data.startswith(b"\r"):
            # An Enter drained in the same read as typed text is part of it.
            if typed:
                draft += b"\n"
            else:
                submit()
            data = data[1:]
        else:
            stops = [i for i in (data.find(b"\r"), data.find(b"\x1b[200~")) if i >= 0]
            cut = min(stops, default=len(data))
            draft, data, typed = draft + data[:cut], data[cut:], True
    paint()
"#;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    workspace: TempDir,
    log: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let script = runtime.path().join("composer.py");
        std::fs::write(&script, COMPOSER).unwrap();
        let log = runtime.path().join("submitted.log");
        std::fs::write(&log, "").unwrap();
        let command = serde_json::to_string(&[
            "python3",
            "-u",
            script.to_str().unwrap(),
            log.to_str().unwrap(),
        ])
        .unwrap();
        std::fs::write(
            runtime.path().join("config.toml"),
            format!("version = 1\n[engines.claude]\ncommand = {command}\n"),
        )
        .unwrap();
        Self {
            runtime,
            state,
            workspace,
            log,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", self.runtime.path().join("config.toml"));
        cmd
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    fn start(&self) -> String {
        let out = self.run(
            &[
                "start",
                "--workspace",
                self.workspace.path().to_str().unwrap(),
                "--tag",
                "composer",
                "--engine",
                "claude",
                "--no-skip-permissions",
                "--json",
            ],
            b"",
        );
        assert!(out.status.success(), "{out:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        let id = value["id"].as_str().unwrap().to_owned();
        self.wait_screen(&id, "❯");
        id
    }

    fn wait_screen(&self, id: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self.run(&["capture", id, "--screen", "--plain"], b"");
            if String::from_utf8_lossy(&out.stdout).contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "screen never showed {needle:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn submissions(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(|hex| String::from_utf8(decode_hex(hex)).unwrap())
            .collect()
    }

    fn kill(&self, id: &str) {
        self.run(&["kill", id, "--signal", "KILL"], b"");
    }
}

fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn multiline_stdin_enter_is_submitted_once_by_a_lagging_agent() {
    let harness = Harness::new();
    let id = harness.start();
    let text = "first line\nsecond line\nthird line\n";
    let out = harness.run(
        &["--json", "send", &id, "--stdin", "--enter"],
        text.as_bytes(),
    );
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["status"], "submitted", "{status}");
    assert_eq!(harness.submissions(), vec![text.to_owned()]);
    harness.kill(&id);
}

#[test]
fn long_text_is_submitted_once() {
    let harness = Harness::new();
    let id = harness.start();
    let text: String = (0..300).map(|i| format!("context line {i}\n")).collect();
    let out = harness.run(
        &["--json", "send", &id, "--stdin", "--enter"],
        text.as_bytes(),
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(harness.submissions(), vec![text]);
    harness.kill(&id);
}

#[test]
fn an_existing_draft_is_neither_extended_nor_submitted() {
    let harness = Harness::new();
    let id = harness.start();
    let typed = harness.run(&["send", &id, "someone's draft"], b"");
    assert!(typed.status.success(), "{typed:?}");
    harness.wait_screen(&id, "someone's draft");
    let out = harness.run(&["--json", "send", &id, "--stdin", "--enter"], b"mail\n");
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not at an empty input prompt"),
        "{out:?}"
    );
    harness.wait_screen(&id, "❯ someone's draft");
    assert!(harness.submissions().is_empty());
    harness.kill(&id);
}
