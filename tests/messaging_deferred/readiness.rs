use super::*;

fn rejected_case(state: Option<&str>, reported_at: Option<u64>, activity: Option<u64>) -> Case {
    let mut case = Case::new(false, "codex");
    case.recipient.reported_state = state.map(str::to_owned);
    case.recipient.reported_state_at_ms = reported_at;
    case.recipient.last_activity_ms = activity;
    case
}

fn rejected_detail(case: &Case) -> String {
    let empty_home = tempfile::TempDir::new().unwrap();
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", empty_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
    assert_eq!(case.stored(), case.envelope);
    let mp = message_paths(&case.harness.paths(), &case.recipient.workspace);
    assert!(!mp.msgs_dir.join(format!("{}.attempt", case.id())).exists());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["detail"].as_str().unwrap().to_owned()
}

#[test]
fn contradicted_idle_explains_live_evidence_and_recipient_owned_recovery() {
    let case = rejected_case(Some("idle"), Some(1000), Some(3001));
    let detail = rejected_detail(&case);
    assert!(
        detail.contains("idle report contradicted by later PTY output"),
        "{detail}"
    );
    assert!(detail.contains(&case.recipient.id.to_string()), "{detail}");
    assert!(detail.contains("derived=idle source=activity"), "{detail}");
    assert!(detail.contains("reported_at_ms=Some(1000)"), "{detail}");
    assert!(detail.contains("last_activity_ms=Some(3001)"), "{detail}");
    assert!(
        detail.contains("Prompt capture does not refresh harness state"),
        "{detail}"
    );
    assert!(detail.contains("genuine harness state event"), "{detail}");
    assert!(detail.contains("re-inspect its prompt"), "{detail}");
}

#[test]
fn expired_waiting_has_an_actionable_reason_without_writing_input() {
    let case = rejected_case(Some("waiting"), Some(1000), Some(900));
    let detail = rejected_detail(&case);
    assert!(detail.contains("waiting report expired"), "{detail}");
    assert!(!detail.contains("idle report contradicted"), "{detail}");
}

#[test]
fn current_working_remains_blocked_without_idle_override_advice() {
    let case = rejected_case(Some("working"), Some(aplexer::now_ms()), None);
    let detail = rejected_detail(&case);
    assert!(detail.contains("recipient reported working"), "{detail}");
    assert!(!detail.contains("state-report idle"), "{detail}");
}

#[test]
fn missing_report_and_timestamp_have_distinct_diagnostics() {
    let absent = rejected_case(None, None, None);
    assert!(rejected_detail(&absent).contains("missing reported state"));
    let timestamp = rejected_case(Some("idle"), None, Some(900));
    assert!(rejected_detail(&timestamp).contains("missing reported-state timestamp"));
}

#[test]
fn exiting_lifecycle_overrides_even_a_valid_report() {
    let mut case = rejected_case(Some("idle"), Some(1000), Some(900));
    case.recipient.phase = Phase::Exiting;
    let detail = rejected_detail(&case);
    assert!(
        detail.contains("recipient lifecycle does not accept delivery"),
        "{detail}"
    );
    assert!(
        detail.contains("derived=exiting source=lifecycle"),
        "{detail}"
    );
}

#[test]
fn old_quiet_idle_stays_eligible_and_same_envelope_is_submitted_once() {
    let case = rejected_case(Some("idle"), Some(1000), Some(900));
    let server = worker::Worker::start(&case.recipient, false);
    let output = case.deliver(&case.sender);
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    status(&case.deliver(&case.recipient), "already-submitted");
    let mut expected = case.envelope.clone();
    expected["delivery"] = "pane".into();
    assert_eq!(case.stored(), expected);
    assert_eq!(server.finish().len(), 4);
}

#[test]
fn antigravity_busy_working_is_blocked_without_writing_input() {
    let mut case = Case::new(false, "antigravity");
    case.recipient.reported_state = Some("working".into());
    case.recipient.reported_state_at_ms = Some(aplexer::now_ms());
    case.recipient.last_activity_ms = None;
    let detail = rejected_detail(&case);
    assert!(detail.contains("recipient reported working"), "{detail}");
    assert!(!detail.contains("state-report idle"), "{detail}");
}

#[test]
fn antigravity_missing_hooks_idle_is_contradicted_by_later_activity() {
    let empty_home = tempfile::TempDir::new().unwrap();
    let mut case = Case::new(false, "antigravity");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", empty_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let detail = value["detail"].as_str().unwrap();
    assert!(
        detail.contains("idle report contradicted by later PTY output"),
        "{detail}"
    );
}

#[test]
fn antigravity_with_installed_hooks_permits_idle_delivery_despite_tui_output() {
    let temp_home = tempfile::TempDir::new().unwrap();
    let hooks_dir = temp_home.path().join(".gemini").join("config");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let hooks_content = serde_json::json!({
        "aplexer-state-report": {
            "PreInvocation": [
                {"type": "command", "command": "/bin/a state-report working || true"}
            ],
            "Stop": [
                {"type": "command", "command": "/bin/a state-report idle || true"}
            ]
        }
    });
    std::fs::write(
        hooks_dir.join("hooks.json"),
        serde_json::to_string_pretty(&hooks_content).unwrap(),
    )
    .unwrap();

    let mut case = Case::new(false, "antigravity");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    assert_eq!(server.finish().len(), 4);
}

#[test]
fn opencode_missing_hooks_idle_is_contradicted_by_later_activity() {
    let empty_home = tempfile::TempDir::new().unwrap();
    let mut case = Case::new(false, "opencode");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", empty_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let detail = value["detail"].as_str().unwrap();
    assert!(
        detail.contains("idle report contradicted by later PTY output"),
        "{detail}"
    );
}

#[test]
fn opencode_with_installed_hooks_permits_idle_delivery_despite_tui_output() {
    let temp_home = tempfile::TempDir::new().unwrap();
    let plugin_dir = temp_home.path().join(".config").join("opencode").join("plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let plugin_code = aplexer::hooks::opencode_plugin_source("/bin/a");
    std::fs::write(plugin_dir.join("aplexer-state-report.js"), plugin_code).unwrap();

    let mut case = Case::new(false, "opencode");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    assert_eq!(server.finish().len(), 4);
}

#[test]
fn zcodex_missing_hooks_idle_is_contradicted_by_later_activity() {
    let empty_home = tempfile::TempDir::new().unwrap();
    let mut case = Case::new(false, "zcodex");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", empty_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let detail = value["detail"].as_str().unwrap();
    assert!(
        detail.contains("idle report contradicted by later PTY output"),
        "{detail}"
    );
}

#[test]
fn zcodex_with_installed_hooks_permits_idle_delivery_despite_tui_output() {
    let temp_home = tempfile::TempDir::new().unwrap();
    let codex_dir = temp_home.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let hooks_content = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {"hooks": [{"type": "command", "command": "/bin/a state-report working || true"}]}
            ],
            "Stop": [
                {"hooks": [{"type": "command", "command": "/bin/a state-report idle || true"}]}
            ],
            "UserPromptSubmit": [
                {"hooks": [{"type": "command", "command": "/bin/a state-report working || true"}]}
            ]
        }
    });
    std::fs::write(
        codex_dir.join("hooks.json"),
        serde_json::to_string_pretty(&hooks_content).unwrap(),
    )
    .unwrap();

    let mut case = Case::new(false, "zcodex");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    assert_eq!(server.finish().len(), 4);
}

#[test]
fn zcodex_multi_turn_working_and_idle_transitions_preserve_delivery_integrity() {
    let temp_home = tempfile::TempDir::new().unwrap();
    let codex_dir = temp_home.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let hooks_content = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {"hooks": [{"type": "command", "command": "/bin/a state-report working || true"}]}
            ],
            "Stop": [
                {"hooks": [{"type": "command", "command": "/bin/a state-report idle || true"}]}
            ],
            "UserPromptSubmit": [
                {"hooks": [{"type": "command", "command": "/bin/a state-report working || true"}]}
            ]
        }
    });
    std::fs::write(
        codex_dir.join("hooks.json"),
        serde_json::to_string_pretty(&hooks_content).unwrap(),
    )
    .unwrap();

    let mut case = Case::new(false, "zcodex");

    // Turn 1 active working: delivery must be rejected
    case.recipient.reported_state = Some("working".into());
    case.recipient.reported_state_at_ms = Some(aplexer::now_ms());
    case.recipient.last_activity_ms = None;
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    status(&output, "not-ready");
    assert!(server.finish().is_empty());

    // Turn 1 finishes: reported idle, subsequent PTY activity: delivery must succeed
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    assert_eq!(server.finish().len(), 4);

    // Turn 2 active working (UserPromptSubmit): delivery must be blocked again
    let mut case2 = Case::new(false, "zcodex");
    case2.recipient.reported_state = Some("working".into());
    case2.recipient.reported_state_at_ms = Some(aplexer::now_ms());
    case2.recipient.last_activity_ms = None;
    let server = worker::Worker::start(&case2.recipient, false);
    let output = case2
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case2.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case2.id().to_string(),
            "--workspace",
            case2.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
}

#[test]
fn opencode_multi_turn_working_and_idle_transitions_preserve_delivery_integrity() {
    let temp_home = tempfile::TempDir::new().unwrap();
    let plugin_dir = temp_home.path().join(".config").join("opencode").join("plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let plugin_code = aplexer::hooks::opencode_plugin_source("/bin/a");
    std::fs::write(plugin_dir.join("aplexer-state-report.js"), plugin_code).unwrap();

    let mut case = Case::new(false, "opencode");

    // Turn 1 active working: delivery must be rejected
    case.recipient.reported_state = Some("working".into());
    case.recipient.reported_state_at_ms = Some(aplexer::now_ms());
    case.recipient.last_activity_ms = None;
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    status(&output, "not-ready");
    assert!(server.finish().is_empty());

    // Turn 1 finishes: reported idle, subsequent PTY activity: delivery must succeed
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(3001);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case.id().to_string(),
            "--workspace",
            case.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    assert_eq!(server.finish().len(), 4);

    // Turn 2 active working: delivery must be blocked again
    let mut case2 = Case::new(false, "opencode");
    case2.recipient.reported_state = Some("working".into());
    case2.recipient.reported_state_at_ms = Some(aplexer::now_ms());
    case2.recipient.last_activity_ms = None;
    let server = worker::Worker::start(&case2.recipient, false);
    let output = case2
        .harness
        .command()
        .env("HOME", temp_home.path())
        .env("APLEXER_SESSION_ID", case2.sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            &case2.id().to_string(),
            "--workspace",
            case2.recipient.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
}




