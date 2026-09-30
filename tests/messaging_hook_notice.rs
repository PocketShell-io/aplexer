#[path = "messaging_hook_notice/ack.rs"]
mod ack;
#[path = "messaging_hook_notice/busy.rs"]
mod busy;
#[path = "messaging_hook_notice/dedupe.rs"]
mod dedupe;
#[path = "messaging_hook_notice/envelopes.rs"]
mod envelopes;
#[path = "messaging_hook_notice/state.rs"]
mod state;
#[path = "support/messaging.rs"]
mod support;

use aplexer::{atomic_write_json, messaging::message_paths, FileLock, Phase, SessionRecord};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use support::Harness;
use tempfile::TempDir;

const SECRET: &str = "HOOK_INPUT_SECRET_63";
const BODY: &str = "MESSAGE_BODY_SECRET_74";

fn engine_record(harness: &Harness, workspace: &std::path::Path, engine: &str) -> SessionRecord {
    let mut record = harness.record_in(workspace, Phase::Exited, None, b"draft-sentinel");
    record.engine = engine.into();
    atomic_write_json(&harness.paths().record(record.id), &record).unwrap();
    record
}

fn hook_input(agent_id: bool) -> Vec<u8> {
    let mut input = json!({"hook_event_name": "PostToolUse", "session_id": "harness-session",
        "tool_name": "Bash", "tool_use_id": "tool-1",
        "tool_input": {"credential": SECRET}, "tool_response": {"content": SECRET}});
    if agent_id {
        input["agent_id"] = json!("child-agent");
    }
    serde_json::to_vec(&input).unwrap()
}

fn run_hook(mut command: Command, engine: &str, input: &[u8]) -> Output {
    let mut child = command
        .args(["message", "hook-notice", "--engine", engine])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn send(harness: &Harness, sender: &SessionRecord, recipient: &SessionRecord) -> Value {
    let output = harness
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
            BODY,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn notice(harness: &Harness, recipient: &SessionRecord, engine: &str, input: &[u8]) -> Output {
    let mut command = harness.command();
    command.env("APLEXER_SESSION_ID", recipient.id.to_string());
    run_hook(command, engine, input)
}

fn context(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn subagent_and_unbound_calls_are_quiet_without_notice_mutation() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "claude");
    let _ = send(&harness, &sender, &recipient);
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let state = mp.cursors_dir.join(format!("{}.notice", recipient.id));
    let child = notice(&harness, &recipient, "claude", &hook_input(true));
    assert!(child.status.success() && child.stdout.is_empty() && child.stderr.is_empty());
    let malformed = notice(&harness, &recipient, "claude", b"not-json-secret");
    assert!(
        malformed.status.success() && malformed.stdout.is_empty() && malformed.stderr.is_empty()
    );
    let wrong_engine = notice(&harness, &recipient, "codex", &hook_input(false));
    assert!(wrong_engine.status.success() && wrong_engine.stdout.is_empty());
    let mut missing = harness.command();
    missing.env_remove("APLEXER_SESSION_ID");
    let unbound = run_hook(missing, "claude", &hook_input(false));
    assert!(unbound.status.success() && unbound.stdout.is_empty());
    let mut command = harness.command();
    command.env("APLEXER_SESSION_ID", uuid::Uuid::now_v7().to_string());
    let unknown = run_hook(command, "claude", &hook_input(false));
    assert!(unknown.status.success() && unknown.stdout.is_empty());
    assert!(!state.exists());
    assert!(!mp
        .cursors_dir
        .join(format!("{}.json", recipient.id))
        .exists());
}

#[test]
fn shell_launched_codex_gets_notice_then_explicit_cross_workspace_reply() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = harness.record_in(workspace.path(), Phase::Exited, None, b"human-draft");
    assert_eq!(recipient.engine, "shell");
    let sent = send(&harness, &sender, &recipient);
    let id = sent["id"].as_str().unwrap();
    let text = context(&notice(&harness, &recipient, "codex", &hook_input(false)));
    assert!(text.contains(id) && !text.contains(BODY));
    let reply = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["--json", "message", "reply", id, "confirmed"])
        .output()
        .unwrap();
    assert!(reply.status.success(), "{reply:?}");
    let response: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(response["reply_to"], id);
    assert_eq!(response["to"]["session_id"], sender.id.to_string());
    assert_eq!(response["workspace"], sender.workspace.to_str().unwrap());
    ack::assert_explicit_ack(&harness, &recipient, id);
    assert_eq!(
        std::fs::read(&recipient.history_path).unwrap(),
        b"human-draft"
    );
}
