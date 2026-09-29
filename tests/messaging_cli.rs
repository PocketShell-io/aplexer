#[path = "support/messaging.rs"]
mod support;

use aplexer::{atomic_write_json, Phase};
use serde_json::Value;
use support::Harness;
use tempfile::TempDir;

#[test]
fn local_messages_follow_live_session_workspace_before_stale_environment() {
    let harness = Harness::new();
    let current_workspace = TempDir::new().unwrap();
    let stale_workspace = TempDir::new().unwrap();
    let sender = harness.record_in(current_workspace.path(), Phase::Exited, None, b"");
    let recipient = harness.record_in(current_workspace.path(), Phase::Exited, None, b"");

    let sent = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .env("APLEXER_WORKSPACE", stale_workspace.path())
        .args([
            "--json",
            "message",
            "send",
            "--to",
            &recipient.tag,
            "current workspace",
        ])
        .output()
        .unwrap();
    assert!(sent.status.success(), "{sent:?}");
    let envelope: Value = serde_json::from_slice(&sent.stdout).unwrap();
    assert_eq!(
        envelope["workspace"],
        current_workspace.path().to_str().unwrap()
    );

    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .env("APLEXER_WORKSPACE", stale_workspace.path())
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    let messages: Value = serde_json::from_slice(&inbox.stdout).unwrap();
    assert_eq!(messages[0]["id"], envelope["id"]);
}

#[test]
fn retained_message_dispatcher_serves_log_show_and_gc() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record(Phase::Exited, None, b"");
    let sent = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args(["--json", "message", "send", "--to", &recipient.tag, "hello"])
        .output()
        .unwrap();
    assert!(sent.status.success(), "{sent:?}");
    let envelope: Value = serde_json::from_slice(&sent.stdout).unwrap();

    let log = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args(["--json", "message", "log"])
        .output()
        .unwrap();
    assert!(log.status.success(), "{log:?}");
    let logged: Value = serde_json::from_slice(&log.stdout).unwrap();
    assert_eq!(logged[0]["id"], envelope["id"]);

    let show = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args([
            "--json",
            "message",
            "show",
            envelope["id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["id"], envelope["id"]);

    let gc = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args(["--json", "message", "gc"])
        .output()
        .unwrap();
    assert!(gc.status.success(), "{gc:?}");
    let report: Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["remaining"], 1);
}

#[test]
fn cross_workspace_message_reply_reaches_original_session_inbox() {
    let harness = Harness::new();
    let recipient_workspace = TempDir::new().unwrap();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record_in(recipient_workspace.path(), Phase::Exited, None, b"");

    let send = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .env("APLEXER_WORKSPACE", &sender.workspace)
        .args([
            "--json",
            "message",
            "send",
            "--workspace",
            recipient.workspace.to_str().unwrap(),
            "--to",
            &recipient.tag,
            "please confirm",
        ])
        .output()
        .unwrap();
    assert!(send.status.success(), "{send:?}");
    let sent: Value = serde_json::from_slice(&send.stdout).unwrap();
    assert_eq!(sent["from"]["session_id"], sender.id.to_string());
    assert_eq!(
        sent["from"]["workspace"],
        sender.workspace.to_str().unwrap()
    );

    let spoof = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args([
            "message",
            "send",
            "--workspace",
            recipient.workspace.to_str().unwrap(),
            "--from",
            &recipient.tag,
            "--to",
            &recipient.tag,
            "spoof",
        ])
        .output()
        .unwrap();
    assert!(!spoof.status.success(), "{spoof:?}");
    assert!(String::from_utf8_lossy(&spoof.stderr).contains("--from cannot be used"));

    let unresolved = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args([
            "message",
            "send",
            "--workspace",
            recipient.workspace.to_str().unwrap(),
            "--to",
            "not-created-yet",
            "--queue",
            "unaddressable",
        ])
        .output()
        .unwrap();
    assert!(!unresolved.status.success(), "{unresolved:?}");
    assert!(String::from_utf8_lossy(&unresolved.stderr).contains("existing target session"));

    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .env("APLEXER_WORKSPACE", &recipient.workspace)
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    let incoming: Value = serde_json::from_slice(&inbox.stdout).unwrap();
    assert_eq!(incoming[0]["id"], sent["id"]);

    let reply = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .env("APLEXER_WORKSPACE", &recipient.workspace)
        .args([
            "--json",
            "message",
            "reply",
            sent["id"].as_str().unwrap(),
            "confirmed",
        ])
        .output()
        .unwrap();
    assert!(reply.status.success(), "{reply:?}");
    let replied: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(replied["workspace"], sender.workspace.to_str().unwrap());
    assert_eq!(replied["to"]["session_id"], sender.id.to_string());
    assert_eq!(replied["reply_to"], sent["id"]);

    let mut retagged_sender = sender.clone();
    retagged_sender.tag = "new-owner-tag".into();
    atomic_write_json(&harness.paths().record(sender.id), &retagged_sender).unwrap();
    let mut reused = harness.record(Phase::Exited, None, b"");
    reused.tag = sender.tag.clone();
    atomic_write_json(&harness.paths().record(reused.id), &reused).unwrap();
    let reused_inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", reused.id.to_string())
        .env("APLEXER_WORKSPACE", &reused.workspace)
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(reused_inbox.status.success(), "{reused_inbox:?}");
    let leaked: Value = serde_json::from_slice(&reused_inbox.stdout).unwrap();
    assert!(leaked.as_array().unwrap().is_empty(), "{leaked}");

    let sender_inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .env("APLEXER_WORKSPACE", &sender.workspace)
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(sender_inbox.status.success(), "{sender_inbox:?}");
    let responses: Value = serde_json::from_slice(&sender_inbox.stdout).unwrap();
    assert_eq!(responses[0]["id"], replied["id"]);
}

#[test]
fn failed_pane_injection_returns_durable_inbox_outcome() {
    let harness = Harness::new();
    let recipient_workspace = TempDir::new().unwrap();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record_in(recipient_workspace.path(), Phase::Exited, None, b"");
    let send = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args([
            "--json",
            "message",
            "send",
            "--workspace",
            recipient.workspace.to_str().unwrap(),
            "--to",
            &recipient.tag,
            "--pane",
            "--or-inbox",
            "message for stopped agent",
        ])
        .output()
        .unwrap();
    assert!(send.status.success(), "{send:?}");
    let sent: Value = serde_json::from_slice(&send.stdout).unwrap();
    assert_eq!(sent["delivery"], "inbox");
    assert!(String::from_utf8_lossy(&send.stderr).contains("durable inbox copy remains"));

    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .env("APLEXER_WORKSPACE", &recipient.workspace)
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    let incoming: Value = serde_json::from_slice(&inbox.stdout).unwrap();
    assert_eq!(incoming[0]["id"], sent["id"]);
}

#[test]
fn failed_strict_pane_reports_stored_message_id_without_inviting_retry() {
    let harness = Harness::new();
    let recipient_workspace = TempDir::new().unwrap();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record_in(recipient_workspace.path(), Phase::Exited, None, b"");
    let send = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args([
            "message",
            "send",
            "--workspace",
            recipient.workspace.to_str().unwrap(),
            "--to",
            &recipient.tag,
            "--pane",
            "strict pane",
        ])
        .output()
        .unwrap();
    assert!(!send.status.success(), "{send:?}");
    let error = String::from_utf8_lossy(&send.stderr);
    assert!(
        error.contains("durable inbox copy recorded (delivery=inbox)"),
        "{error}"
    );
    assert!(error.contains("inspect that id before retrying"), "{error}");

    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .env("APLEXER_WORKSPACE", &recipient.workspace)
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    let messages: Value = serde_json::from_slice(&inbox.stdout).unwrap();
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["delivery"], "inbox");
    assert!(
        error.contains(messages[0]["id"].as_str().unwrap()),
        "{error}"
    );
}

#[test]
fn same_workspace_message_reply_and_ack_still_work() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record(Phase::Exited, None, b"");
    let send = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .env("APLEXER_WORKSPACE", &sender.workspace)
        .args(["--json", "message", "send", "--to", &recipient.tag, "hello"])
        .output()
        .unwrap();
    assert!(send.status.success(), "{send:?}");
    let sent: Value = serde_json::from_slice(&send.stdout).unwrap();

    let reply = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .env("APLEXER_WORKSPACE", &recipient.workspace)
        .args([
            "--json",
            "message",
            "reply",
            sent["id"].as_str().unwrap(),
            "done",
        ])
        .output()
        .unwrap();
    assert!(reply.status.success(), "{reply:?}");
    let replied: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(replied["workspace"], sender.workspace.to_str().unwrap());

    let ack = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .env("APLEXER_WORKSPACE", &sender.workspace)
        .args(["--json", "message", "ack", replied["id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(ack.status.success(), "{ack:?}");
    let acknowledged: Value = serde_json::from_slice(&ack.stdout).unwrap();
    assert_eq!(acknowledged["acked"][0], replied["id"]);

    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .env("APLEXER_WORKSPACE", &sender.workspace)
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    let unread: Value = serde_json::from_slice(&inbox.stdout).unwrap();
    assert_eq!(unread.as_array().unwrap().len(), 0);
}
