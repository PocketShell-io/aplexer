#[path = "support/messaging.rs"]
mod support;

use aplexer::messaging::{ack_messages, read_cursor};
use aplexer::{Phase, SessionRecord};
use serde_json::Value;
use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use support::Harness;
use tempfile::TempDir;
use uuid::Uuid;

fn wait_command(harness: &Harness, recipient: &SessionRecord, timeout: &str) -> Command {
    let mut command = harness.command();
    command.env("APLEXER_SESSION_ID", recipient.id.to_string());
    command.args(["--json", "message", "wait", "--timeout", timeout]);
    command
}

fn send(harness: &Harness, sender: &SessionRecord, recipient: &SessionRecord) -> Value {
    let output = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .arg("--json")
        .args(["message", "send", "--workspace"])
        .arg(&recipient.workspace)
        .args(["--to", &recipient.tag, "hello"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn has_watch(pid: u32) -> bool {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Ok(info) = fs::read_to_string(entry.path()) {
            if info.lines().any(|line| line.starts_with("inotify wd:")) {
                return true;
            }
        }
    }
    false
}

fn await_subscription(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !has_watch(child.id()) {
        assert!(
            child.try_wait().unwrap().is_none(),
            "wait exited before subscribing"
        );
        assert!(Instant::now() < deadline, "wait never subscribed");
        std::thread::yield_now();
    }
}

#[test]
fn cross_workspace_publication_wakes_cli_wait_without_acknowledging() {
    let harness = Harness::new();
    let source = TempDir::new().unwrap();
    let sender = harness.record_in(source.path(), Phase::Exited, None, b"");
    let recipient = harness.record(Phase::Exited, None, b"");
    let mut child = wait_command(&harness, &recipient, "5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    await_subscription(&mut child);
    let envelope = send(&harness, &sender, &recipient);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let messages: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], envelope["id"]);
    let id = Uuid::parse_str(envelope["id"].as_str().unwrap()).unwrap();
    assert!(
        !read_cursor(&harness.paths(), &recipient.workspace, recipient.id)
            .unwrap()
            .is_acked(id)
    );
}

#[test]
fn unrelated_recipient_publication_does_not_finish_wait_early() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record(Phase::Exited, None, b"");
    let other = harness.record(Phase::Exited, None, b"");
    let start = Instant::now();
    let mut child = wait_command(&harness, &recipient, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    await_subscription(&mut child);
    send(&harness, &sender, &other);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::json!([])
    );
    assert!(start.elapsed() >= Duration::from_secs(1));
}

#[test]
fn wait_uses_stored_identity_despite_stale_workspace_tag_and_cwd() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record(Phase::Exited, None, b"");
    let stale = TempDir::new().unwrap();
    let envelope = send(&harness, &sender, &recipient);
    let output = wait_command(&harness, &recipient, "0")
        .env("APLEXER_WORKSPACE", stale.path())
        .env("APLEXER_TAG", "wrong")
        .current_dir(stale.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let messages: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(messages[0]["id"], envelope["id"]);
}

#[test]
fn acknowledged_mail_times_out_as_empty_array_and_text_is_compatible() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record(Phase::Exited, None, b"");
    let envelope = send(&harness, &sender, &recipient);
    let id = Uuid::parse_str(envelope["id"].as_str().unwrap()).unwrap();
    ack_messages(&harness.paths(), &recipient.workspace, recipient.id, &[id]).unwrap();
    let output = wait_command(&harness, &recipient, "0").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::json!([])
    );
    let text = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["message", "wait", "--timeout", "0"])
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    assert_eq!(text.stdout, b"no unread messages\n");
}

#[test]
fn wait_rejects_orphan_identity_and_consumer_overrides() {
    let harness = Harness::new();
    let output = harness
        .command()
        .env("APLEXER_SESSION_ID", Uuid::now_v7().to_string())
        .args(["message", "wait", "--timeout", "0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("existing session record"));
    for flag in ["--from", "--workspace"] {
        let output = harness
            .command()
            .args(["message", "wait", flag, "anything"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
}
