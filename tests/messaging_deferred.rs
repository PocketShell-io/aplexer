#[path = "support/messaging.rs"]
mod support;
#[path = "messaging_deferred/worker.rs"]
mod worker;

use aplexer::{atomic_write_json, messaging::*, Phase, SessionRecord};
use serde_json::Value;
use std::process::Output;
use support::Harness;
use tempfile::TempDir;
use uuid::Uuid;

struct Case {
    harness: Harness,
    _workspace: TempDir,
    sender: SessionRecord,
    recipient: SessionRecord,
    envelope: Value,
}

impl Case {
    fn new(cross: bool, engine: &str) -> Self {
        let harness = Harness::new();
        let workspace = TempDir::new().unwrap();
        let sender = harness.record(Phase::Exited, None, b"");
        let mut destination = sender.workspace.as_path();
        if cross {
            destination = workspace.path();
        }
        let mut recipient =
            harness.record_in(destination, Phase::Running, Some(std::process::id()), b"");
        recipient.engine = engine.into();
        recipient.reported_state = Some("waiting".into());
        recipient.reported_state_at_ms = Some(aplexer::now_ms());
        atomic_write_json(&harness.paths().record(recipient.id), &recipient).unwrap();
        let envelope = queue_message(&harness, &sender, &recipient);
        Self {
            harness,
            _workspace: workspace,
            sender,
            recipient,
            envelope,
        }
    }

    fn id(&self) -> Uuid {
        Uuid::parse_str(self.envelope["id"].as_str().unwrap()).unwrap()
    }

    fn deliver(&self, caller: &SessionRecord) -> Output {
        self.harness
            .command()
            .env("APLEXER_SESSION_ID", caller.id.to_string())
            .args([
                "--json",
                "message",
                "deliver",
                &self.id().to_string(),
                "--workspace",
                self.recipient.workspace.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    }

    fn stored(&self) -> Value {
        serde_json::to_value(
            read_message(&self.harness.paths(), &self.recipient.workspace, self.id()).unwrap(),
        )
        .unwrap()
    }
}

fn status(output: &Output, expected: &str) {
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON outcome");
    assert_eq!(value["status"], expected, "{output:?}");
}

fn verify_delivery(cross: bool, engine: &str) {
    let case = Case::new(cross, engine);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case.deliver(&case.sender);
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    status(&case.deliver(&case.recipient), "already-submitted");
    let mut expected = case.envelope.clone();
    expected["delivery"] = "pane".into();
    assert_eq!(case.stored(), expected);
    let writes = server.finish();
    assert_eq!(writes.len(), 4);
    assert_eq!(writes[0], b"\x1b[200~");
    assert!(String::from_utf8_lossy(&writes[1]).contains(&format!("id={}", case.id())));
    assert!(String::from_utf8_lossy(&writes[1]).contains(&format!("session={}", case.sender.id)));
    assert_eq!(writes[2], b"\x1b[201~");
    assert_eq!(writes[3], b"\r");
}

#[test]
fn same_and_cross_workspace_keep_identity_and_submit_one_framed_input() {
    for cross in [false, true] {
        for engine in ["codex", "claude", "shell"] {
            verify_delivery(cross, engine);
        }
    }
}

#[test]
fn acknowledged_and_unrelated_callers_cannot_inject() {
    let case = Case::new(true, "codex");
    let stranger = case.harness.record(Phase::Exited, None, b"");
    let denied = case.deliver(&stranger);
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("only the original sender or recipient")
    );
    ack_messages(
        &case.harness.paths(),
        &case.recipient.workspace,
        case.recipient.id,
        &[case.id()],
    )
    .unwrap();
    status(&case.deliver(&case.sender), "recipient-acked");
    assert_eq!(case.stored(), case.envelope);
}

#[test]
fn reused_tag_does_not_redirect_message_to_replacement_session() {
    let case = Case::new(false, "codex");
    let mut replacement = case
        .harness
        .record(Phase::Running, Some(std::process::id()), b"");
    replacement.tag = case.recipient.tag.clone();
    atomic_write_json(&case.harness.paths().record(replacement.id), &replacement).unwrap();
    std::fs::remove_file(case.harness.paths().record(case.recipient.id)).unwrap();
    let result = case.deliver(&case.sender);
    assert!(!result.status.success());
    status(&result, "not-ready");
    assert_eq!(case.stored(), case.envelope);
}

#[test]
fn working_recipient_receives_no_input_and_reservation_stays_retryable() {
    let mut case = Case::new(false, "codex");
    case.recipient.reported_state = Some("working".into());
    let server = worker::Worker::start(&case.recipient, false);
    let result = case.deliver(&case.sender);
    status(&result, "not-ready");
    assert!(!result.status.success());
    assert!(server.finish().is_empty());
    assert_eq!(case.stored(), case.envelope);
    assert!(
        !message_paths(&case.harness.paths(), &case.recipient.workspace)
            .msgs_dir
            .join(format!("{}.attempt", case.id()))
            .exists()
    );
}

#[test]
fn lost_transport_response_is_uncertain_and_never_retried() {
    let case = Case::new(true, "codex");
    let server = worker::Worker::start(&case.recipient, true);
    let first = case.deliver(&case.sender);
    assert!(!first.status.success());
    status(&first, "delivery-uncertain");
    status(&case.deliver(&case.sender), "delivery-uncertain");
    assert_eq!(server.finish(), vec![b"\x1b[200~".to_vec()]);
    assert_eq!(case.stored(), case.envelope);
}

#[test]
fn dead_recipient_leaves_message_queued_without_reserving_input() {
    let mut case = Case::new(false, "codex");
    case.recipient.worker_pid = None;
    atomic_write_json(
        &case.harness.paths().record(case.recipient.id),
        &case.recipient,
    )
    .unwrap();
    status(&case.deliver(&case.sender), "not-ready");
    assert_eq!(case.stored(), case.envelope);
}

#[test]
fn broadcast_and_missing_messages_never_reach_transport() {
    let mut case = Case::new(false, "codex");
    let mut message =
        read_message(&case.harness.paths(), &case.recipient.workspace, case.id()).unwrap();
    message.id = Uuid::now_v7();
    message.to = Recipient::Broadcast { broadcast: true };
    write_message(&case.harness.paths(), &message).unwrap();
    case.envelope = serde_json::to_value(message).unwrap();
    let output = case.deliver(&case.sender);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("one recorded recipient"));
    case.envelope["id"] = Uuid::now_v7().to_string().into();
    assert!(!case.deliver(&case.sender).status.success());
}

#[test]
fn oversized_stored_body_is_rejected_before_any_transport() {
    let mut case = Case::new(false, "codex");
    case.envelope["body"] = "x".repeat(MAX_BODY_BYTES + 1).into();
    let mp = message_paths(&case.harness.paths(), &case.recipient.workspace);
    atomic_write_json(
        &mp.msgs_dir.join(format!("{}.json", case.id())),
        &case.envelope,
    )
    .unwrap();
    let result = case.deliver(&case.sender);
    assert!(!result.status.success());
    status(&result, "not-ready");
    assert!(String::from_utf8_lossy(&result.stdout).contains("message body exceeds"));
    assert!(!mp.msgs_dir.join(format!("{}.attempt", case.id())).exists());
    assert_eq!(case.stored(), case.envelope);
}

fn queue_message(harness: &Harness, sender: &SessionRecord, recipient: &SessionRecord) -> Value {
    let sent = harness
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
            "unchanged message",
        ])
        .output()
        .unwrap();
    assert!(sent.status.success(), "{sent:?}");
    serde_json::from_slice(&sent.stdout).unwrap()
}
