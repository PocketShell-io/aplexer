#[path = "messaging_hook_notice/ack.rs"]
mod ack;
#[path = "messaging_hook_notice/busy.rs"]
mod busy;
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
fn both_engines_emit_bounded_ids_only_and_preserve_ack_and_pty() {
    for engine in ["claude", "codex"] {
        let harness = Harness::new();
        let workspace = TempDir::new().unwrap();
        let sender = harness.record(Phase::Exited, None, b"");
        let recipient = engine_record(&harness, workspace.path(), engine);
        let ids: Vec<String> = (0..6)
            .map(|_| {
                send(&harness, &sender, &recipient)["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let mp = message_paths(&harness.paths(), &recipient.workspace);
        let cursor = mp.cursors_dir.join(format!("{}.json", recipient.id));
        assert!(!cursor.exists());
        let before_history = std::fs::read(&recipient.history_path).unwrap();
        let text = context(&notice(&harness, &recipient, engine, &hook_input(false)));
        assert!(text.contains("6 unread"), "{text}");
        assert_eq!(
            ids.iter().filter(|id| text.contains(id.as_str())).count(),
            5
        );
        assert!(!text.contains(&ids[5]));
        assert!(!text.contains(BODY) && !text.contains(SECRET));
        assert!(!cursor.exists());
        let state =
            std::fs::read_to_string(mp.cursors_dir.join(format!("{}.notice", recipient.id)))
                .unwrap();
        assert!(!state.contains(BODY) && !state.contains(SECRET));
        assert_eq!(
            std::fs::read(&recipient.history_path).unwrap(),
            before_history
        );
    }
}

#[test]
fn concurrent_claims_dedupe_and_cooldown_retries() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "codex");
    let sent = send(&harness, &sender, &recipient);
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut command = harness.command();
            command.env("APLEXER_SESSION_ID", recipient.id.to_string());
            std::thread::spawn(move || run_hook(command, "codex", &hook_input(false)))
        })
        .collect();
    let outputs: Vec<Output> = children
        .into_iter()
        .map(|child| child.join().unwrap())
        .collect();
    assert_eq!(
        outputs.iter().filter(|out| !out.stdout.is_empty()).count(),
        1
    );
    assert!(outputs
        .iter()
        .all(|out| out.status.success() && out.stderr.is_empty()));
    let repeat = notice(&harness, &recipient, "codex", &hook_input(false));
    assert!(repeat.stdout.is_empty());
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let path = mp.cursors_dir.join(format!("{}.notice", recipient.id));
    let old = aplexer::messaging::now_secs() - 601;
    atomic_write_json(
        &path,
        &json!({"claimed_at": {sent["id"].as_str().unwrap(): old}}),
    )
    .unwrap();
    assert!(
        context(&notice(&harness, &recipient, "codex", &hook_input(false)))
            .contains(sent["id"].as_str().unwrap())
    );
    assert!(!mp
        .cursors_dir
        .join(format!("{}.json", recipient.id))
        .exists());
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

#[test]
fn legacy_cursor_is_interpreted_without_ack_write_and_stale_claims_are_pruned() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "claude");
    let first = send(&harness, &sender, &recipient);
    let second = send(&harness, &sender, &recipient);
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let cursor = mp.cursors_dir.join(format!("{}.json", recipient.id));
    let original = serde_json::to_vec(&json!({"acked_through": first["id"]})).unwrap();
    std::fs::write(&cursor, &original).unwrap();
    let text = context(&notice(&harness, &recipient, "claude", &hook_input(false)));
    assert!(!text.contains(first["id"].as_str().unwrap()));
    assert!(text.contains(second["id"].as_str().unwrap()));
    assert_eq!(std::fs::read(&cursor).unwrap(), original);
    let third = send(&harness, &sender, &recipient);
    let second_path = mp
        .msgs_dir
        .join(format!("{}.json", second["id"].as_str().unwrap()));
    std::fs::remove_file(second_path).unwrap();
    let text = context(&notice(&harness, &recipient, "claude", &hook_input(false)));
    assert!(text.contains(third["id"].as_str().unwrap()));
    let state: Value = serde_json::from_slice(
        &std::fs::read(mp.cursors_dir.join(format!("{}.notice", recipient.id))).unwrap(),
    )
    .unwrap();
    assert!(state["claimed_at"]
        .get(second["id"].as_str().unwrap())
        .is_none());
    assert_eq!(std::fs::read(&cursor).unwrap(), original);
    let ack = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["message", "ack", third["id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(ack.status.success(), "{ack:?}");
    assert!(notice(&harness, &recipient, "claude", &hook_input(false))
        .stdout
        .is_empty());
    let state: Value = serde_json::from_slice(
        &std::fs::read(mp.cursors_dir.join(format!("{}.notice", recipient.id))).unwrap(),
    )
    .unwrap();
    assert!(state["claimed_at"].as_object().unwrap().is_empty());
}
