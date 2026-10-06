use super::*;

fn rejected_case(state: Option<&str>, reported_at: Option<u64>, activity: Option<u64>) -> Case {
    let mut case = Case::new(false, "codex");
    case.recipient.reported_state = state.map(str::to_owned);
    case.recipient.reported_state_at_ms = reported_at;
    case.recipient.last_activity_ms = activity;
    case
}

fn rejected_detail(case: &Case) -> String {
    let server = worker::Worker::start(&case.recipient, false);
    let output = case.deliver(&case.sender);
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

#[cfg(unix)]
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
