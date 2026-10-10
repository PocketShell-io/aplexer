#![cfg(unix)]
#[path = "support/messaging.rs"]
mod support;
#[path = "messaging_deferred/worker.rs"]
mod worker;

use aplexer::{atomic_write_json, Phase};
use std::io::Write;
use std::process::Stdio;

fn send(engine: &str, flags: &[&str], data: &[u8]) -> (std::process::Output, Vec<Vec<u8>>) {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Running, Some(std::process::id()), b"");
    record.engine = engine.into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let server = worker::Worker::start(&record, false);
    let mut command = h.command();
    command.args(["--json", "send", &record.id.to_string(), "--stdin"]);
    command
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let result = child.wait_with_output().unwrap();
    (result, server.finish())
}

#[test]
fn piped_long_unicode_multiline_text_is_one_codex_paste_then_enter() {
    let text = "line αβ\n".repeat(1024);
    let (output, writes) = send("codex", &["--enter"], text.as_bytes());
    assert!(output.status.success(), "{output:?}");
    assert_eq!(writes[0], b"\x1b[200~");
    assert_eq!(writes[writes.len() - 2], b"\x1b[201~");
    assert_eq!(writes.last().unwrap(), b"\r");
    assert_eq!(writes[1..writes.len() - 2].concat(), text.as_bytes());
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "pty_written");
    assert_eq!(receipt["enter_written"], true);
    assert!(receipt["consumed"].is_null()); // Fake transport cannot assert a turn.
}

#[test]
fn raw_hex_shell_and_empty_enter_keep_literal_intent() {
    for (engine, flags, input, expected) in [
        (
            "codex",
            vec!["--raw", "--enter"],
            b"\x00\xff".as_slice(),
            b"\x00\xff\r".as_slice(),
        ),
        (
            "codex",
            vec!["--hex", "--enter"],
            b"00 ff".as_slice(),
            b"\x00\xff\r".as_slice(),
        ),
        (
            "shell",
            vec!["--enter"],
            b"echo ok".as_slice(),
            b"echo ok\r".as_slice(),
        ),
        ("codex", vec!["--enter"], b"".as_slice(), b"\r".as_slice()),
        (
            "codex",
            vec![],
            b"\x00\xff".as_slice(),
            b"\x00\xff".as_slice(),
        ),
    ] {
        let (output, writes) = send(engine, &flags, input);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(writes.concat(), expected);
        assert!(writes.iter().all(|w| !w.starts_with(b"\x1b[200~")));
    }
}

#[test]
fn binary_and_terminal_controls_require_explicit_raw_before_any_write() {
    for input in [
        b"\xff".as_slice(),
        b"\x1b[201~evil".as_slice(),
        b"\x00".as_slice(),
    ] {
        let (output, writes) = send("codex", &["--enter"], input);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--raw"));
        assert!(writes.is_empty());
    }
}

#[test]
fn positional_text_and_piped_text_use_the_same_codex_submission() {
    let text = "supported positional recovery\nαβ";
    let (_, piped) = send("codex", &["--enter"], text.as_bytes());
    let h = support::Harness::new();
    let mut record = h.record(Phase::Running, Some(std::process::id()), b"");
    record.engine = "codex".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let server = worker::Worker::start(&record, false);
    let output = h
        .command()
        .args(["send", &record.id.to_string(), text, "--enter"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(server.finish(), piped);
}
