use super::*;

fn rejected_case(state: Option<&str>, reported_at: Option<u64>, activity: Option<u64>) -> Case {
    let mut case = Case::new(false, "codex");
    case.recipient.reported_state = state.map(str::to_owned);
    case.recipient.reported_state_at_ms = reported_at;
    case.recipient.last_activity_ms = activity;
    case
}

/// Rejections without composer evidence: the fake recipient shows no prompt.
fn rejected_detail(case: &Case) -> String {
    let server = worker::Worker::start_without_prompt(&case.recipient);
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
    assert!(detail.contains("not at an empty input prompt"), "{detail}");
    assert!(detail.contains("observed empty input prompt"), "{detail}");
    assert!(detail.contains("leave it queued"), "{detail}");
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

/// A Claude/Codex waiting report goes stale in seconds while the agent sits
/// at its prompt; the observed empty composer is the readiness evidence.
#[cfg(unix)]
#[test]
fn expired_waiting_at_an_empty_prompt_is_submitted_once() {
    for engine in ["claude", "codex"] {
        let mut case = rejected_case(Some("waiting"), Some(1000), Some(900));
        case.recipient.engine = engine.into();
        atomic_write_json(
            &case.harness.paths().record(case.recipient.id),
            &case.recipient,
        )
        .unwrap();
        let server = worker::Worker::start(&case.recipient, false);
        let output = case.deliver(&case.sender);
        assert!(output.status.success(), "{engine}: {output:?}");
        status(&output, "submitted");
        let writes = server.finish();
        assert_eq!(writes.iter().filter(|w| w.as_slice() == b"\r").count(), 1);
    }
}

#[cfg(unix)]
#[test]
fn working_at_an_empty_prompt_stays_blocked() {
    let case = rejected_case(Some("working"), Some(aplexer::now_ms()), None);
    let server = worker::Worker::start(&case.recipient, false);
    let output = case.deliver(&case.sender);
    status(&output, "not-ready");
    assert!(server.finish().is_empty());
}

#[cfg(unix)]
#[test]
fn unsent_draft_in_the_recipient_prompt_is_not_ready_and_untouched() {
    let case = rejected_case(Some("waiting"), Some(aplexer::now_ms()), None);
    let server = worker::Worker::start(&case.recipient, false);
    // Someone else's draft: a plain write, no Enter.
    case.harness
        .command()
        .args(["send", &case.recipient.id.to_string(), "half typed"])
        .output()
        .unwrap();
    let output = case.deliver(&case.sender);
    status(&output, "not-ready");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["detail"].as_str().unwrap().contains("half typed"),
        "{value}"
    );
    assert_eq!(server.finish(), vec![b"half typed".to_vec()]);
    assert_eq!(case.stored(), case.envelope);
}

/// The reported bug: the agent folds Enter into the draft. That is never
/// `submitted`, and never answered with a second Enter.
#[cfg(unix)]
#[test]
fn enter_swallowed_into_the_draft_is_uncertain_with_exactly_one_enter() {
    let case = rejected_case(Some("waiting"), Some(aplexer::now_ms()), None);
    let server = worker::Worker::start_swallowing_enter(&case.recipient);
    let output = case
        .harness
        .command()
        .env("APLEXER_SESSION_ID", case.sender.id.to_string())
        .env("APLEXER_SUBMIT_TIMEOUT_MS", "300")
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
    status(&output, "delivery-uncertain");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["detail"].as_str().unwrap().contains("unsent draft"),
        "{value}"
    );
    let writes = server.finish();
    assert_eq!(writes.iter().filter(|w| w.as_slice() == b"\r").count(), 1);
    status(&case.deliver(&case.sender), "delivery-uncertain");
}

/// Pasted framed mail whose text never shows up in the composer: an empty
/// composer after Enter would prove nothing, so Enter is never sent and the
/// outcome is uncertain, not `submitted`.
#[cfg(unix)]
#[test]
fn text_never_rendered_gets_no_enter_and_is_never_submitted() {
    for engine in ["claude", "codex"] {
        let mut case = rejected_case(Some("waiting"), Some(aplexer::now_ms()), None);
        case.recipient.engine = engine.into();
        atomic_write_json(
            &case.harness.paths().record(case.recipient.id),
            &case.recipient,
        )
        .unwrap();
        let server = worker::Worker::start_never_rendering(&case.recipient);
        let output = case
            .harness
            .command()
            .env("APLEXER_SESSION_ID", case.sender.id.to_string())
            .env("APLEXER_SUBMIT_TIMEOUT_MS", "300")
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
        assert!(!output.status.success(), "{engine}: {output:?}");
        status(&output, "delivery-uncertain");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            value["detail"].as_str().unwrap().contains("never appeared"),
            "{value}"
        );
        let writes = server.finish();
        assert_eq!(
            writes.first().map(Vec::as_slice),
            Some(&b"\x1b[200~"[..]),
            "{engine}"
        );
        assert!(
            !writes.iter().any(|w| w.as_slice() == b"\r"),
            "{engine}: {writes:?}"
        );
    }
}
