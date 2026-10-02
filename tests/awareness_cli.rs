#[path = "support/messaging.rs"]
mod support;

use aplexer::{atomic_write_json, Phase};
use serde_json::{json, Value};
use std::io::Write;
use std::process::Stdio;
use support::Harness;

fn hook(harness: &Harness, session: uuid::Uuid, engine: &str, payload: &Value) -> String {
    let mut child = harness
        .command()
        .env("APLEXER_SESSION_ID", session.to_string())
        .args(["context", "hook", "--engine", engine])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn native_payloads_use_harness_conversation_ids_not_aplexer_session_ids() {
    for engine in [
        "codex",
        "claude",
        "grok",
        "gemini",
        "antigravity",
        "opencode",
    ] {
        let harness = Harness::new();
        let mut record = harness.record(Phase::Running, Some(std::process::id()), b"");
        record.engine = if engine == "codex" { "zcodex" } else { engine }.into();
        atomic_write_json(&harness.paths().record(record.id), &record).unwrap();
        let native_id = uuid::Uuid::new_v4().to_string();
        let payload = match engine {
            "grok" => {
                json!({"hookEventName":"PostToolUse","sessionId":native_id,"toolName":"Bash"})
            }
            "antigravity" => {
                json!({"invocationNum":0,"initialNumSteps":0,"conversationId":native_id,"workspacePaths":[record.workspace]})
            }
            "opencode" => {
                json!({"hook_event_name":"tool.execute.after","session_id":"ses_native_non_uuid","tool_name":"bash"})
            }
            _ => json!({"hook_event_name":"SessionStart","session_id":native_id}),
        };
        let output = hook(&harness, record.id, engine, &payload);
        let text = if engine == "opencode" {
            output.clone()
        } else {
            let parsed: Value =
                serde_json::from_str(&output).unwrap_or_else(|_| panic!("{engine}: {output:?}"));
            if engine == "antigravity" {
                parsed["injectSteps"][0]["ephemeralMessage"]
                    .as_str()
                    .unwrap()
                    .into()
            } else {
                parsed["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap()
                    .into()
            }
        };
        assert!(text.contains("work join"), "{engine}: {text}");
        assert!(text.contains("message inbox"), "{engine}: {text}");
    }
}

#[test]
fn main_context_hooks_do_not_inject_into_marked_subagents() {
    let harness = Harness::new();
    let record = harness.record(Phase::Running, Some(std::process::id()), b"");
    let output = hook(
        &harness,
        record.id,
        "codex",
        &json!({
            "hook_event_name":"PostToolUse", "session_id":uuid::Uuid::new_v4(),
            "tool_name":"Bash", "agent_id":"child",
        }),
    );
    assert!(output.trim().is_empty());
}

#[test]
fn context_notice_contains_message_references_without_copying_message_body() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Running, Some(std::process::id()), b"");
    let recipient = harness.record(Phase::Running, Some(std::process::id()), b"");
    let sent = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args([
            "--json",
            "message",
            "send",
            "--to",
            &recipient.tag,
            "private-peer-body-do-not-inline",
        ])
        .output()
        .unwrap();
    assert!(sent.status.success());
    let message: Value = serde_json::from_slice(&sent.stdout).unwrap();
    let output = hook(
        &harness,
        recipient.id,
        "codex",
        &json!({
            "hook_event_name":"PostToolUse", "session_id":uuid::Uuid::new_v4(), "tool_name":"Bash",
        }),
    );
    assert!(output.contains(message["id"].as_str().unwrap()), "{output}");
    assert!(!output.contains("private-peer-body-do-not-inline"));
    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(String::from_utf8(inbox.stdout)
        .unwrap()
        .contains("private-peer-body-do-not-inline"));
}

#[test]
fn optional_context_hook_does_not_wait_for_a_busy_mailbox_cursor() {
    let harness = Harness::new();
    let record = harness.record(Phase::Running, Some(std::process::id()), b"");
    let mailbox =
        aplexer::messaging::ensure_workspace(&harness.paths(), &record.workspace).unwrap();
    let lock = aplexer::FileLock::exclusive(
        &mailbox.cursors_dir.join(format!("{}.lock", record.id)),
        false,
    )
    .unwrap();
    let mut child = harness
        .command()
        .env("APLEXER_SESSION_ID", record.id.to_string())
        .args(["context", "hook", "--engine", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({
                "hook_event_name":"SessionStart", "session_id":uuid::Uuid::new_v4(),
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let finished_before_release = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    drop(lock);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        finished_before_release,
        "optional hook blocked on a mailbox cursor"
    );
}
