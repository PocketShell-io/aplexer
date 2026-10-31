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

#[cfg(unix)]
#[test]
fn busy_draft_and_menu_prompts_refuse_without_reserving_while_ghost_suggestion_delivers() {
    let rule = "\u{2500}".repeat(48);
    let busy_plain = format!(
        "❯ Reply\n\n● ok\n\n✶ Stewing… (3s)\n{rule}\n❯ \n{rule}\n  ⏵⏵ bypass permissions on · esc to interrupt"
    );
    let draft_plain =
        format!("❯ Reply\n\n● ok\n\n✻ Cooked for 2s\n{rule}\n❯ unsent user draft\n{rule}\n  ⏵⏵ bypass permissions on");
    let menu_plain = "Select a model:\n  1. Sonnet\n  2. Opus\n";

    let render_styled = |raw: &str| {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(raw.replace('\n', "\r\n").as_bytes());
        (
            parser.screen().contents().into_bytes(),
            parser.screen().state_formatted(),
        )
    };

    for (plain_src, expected_reason) in [
        (busy_plain.as_str(), "recipient prompt is busy"),
        (
            draft_plain.as_str(),
            "recipient input box holds unsent draft text",
        ),
        (menu_plain, "recipient prompt is not ready"),
    ] {
        let mut case = Case::new(false, "claude");
        case.recipient.reported_state = Some("idle".into());
        case.recipient.reported_state_at_ms = Some(1000);
        case.recipient.last_activity_ms = Some(900);
        let (plain_bytes, styled_bytes) = render_styled(plain_src);
        let server =
            worker::Worker::start_with_screen(&case.recipient, false, plain_bytes, styled_bytes);
        let output = case.deliver(&case.sender);
        assert!(!output.status.success(), "{output:?}");
        status(&output, "not-ready");
        assert!(server.finish().is_empty());
        let mp = message_paths(&case.harness.paths(), &case.recipient.workspace);
        assert!(!mp.msgs_dir.join(format!("{}.attempt", case.id())).exists());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let detail = value["detail"].as_str().unwrap();
        assert!(
            detail.contains(expected_reason),
            "expected {expected_reason:?} in {detail:?}"
        );
    }

    // A faint SGR 2 ghost suggestion in the composer is not a user draft and
    // must still accept delivery once.
    let mut case = Case::new(false, "claude");
    case.recipient.reported_state = Some("idle".into());
    case.recipient.reported_state_at_ms = Some(1000);
    case.recipient.last_activity_ms = Some(900);
    let ghost_raw = format!(
        "\x1b[?2004h❯ Reply\r\n\r\n● ok\r\n\r\n✻ Cooked for 2s\r\n{rule}\r\n❯ \x1b[2mack the message\x1b[0m\r\n{rule}\r\n  ⏵⏵ bypass permissions on"
    );
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(ghost_raw.as_bytes());
    let server = worker::Worker::start_with_screen(
        &case.recipient,
        false,
        parser.screen().contents().into_bytes(),
        parser.screen().state_formatted(),
    );
    let output = case.deliver(&case.sender);
    assert!(output.status.success(), "{output:?}");
    status(&output, "submitted");
    status(&case.deliver(&case.recipient), "already-submitted");
    assert_eq!(server.finish().len(), 4);
}

#[cfg(unix)]
#[test]
fn fresh_interactive_claude_and_codex_sessions_accept_message_deliver() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let rule = "\u{2500}".repeat(48);
    let claude_ready = format!(
        "\x1b[?2004h\x1b[2J\x1b[H{}",
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/screens/claude/ready.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
        .replace('\n', "\r\n")
    );
    let claude_landed = format!(
        "\x1b[2J\x1b[H ▐▛███▛█   Claude Code v2.1.289\r\n\r\n{rule}\r\n❯ [Pasted text #1 +5 lines]\r\n{rule}\r\n  ⏵⏵ bypass permissions on"
    );
    let claude_submitted = format!(
        "\x1b[2J\x1b[H ▐▛███▛█   Claude Code v2.1.289\r\n\r\n❯ unchanged message\r\n\r\n✶ Stewing…\r\n{rule}\r\n❯ \r\n{rule}\r\n  ⏵⏵ bypass permissions on · esc to interrupt"
    );

    let codex_ready = format!(
        "\x1b[?2004h\x1b[2J\x1b[H{}",
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/screens/codex/ready.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
        .replace('\n', "\r\n")
    );
    let codex_landed = "\x1b[2J\x1b[H  >_ OpenAI Codex (v0.162.1)\r\n\r\n› [Pasted Content 120 chars]\r\n\r\n  GPT-6-Luna low · Context 100% left · /tmp/ws".to_string();
    let codex_submitted = "\x1b[2J\x1b[H  >_ OpenAI Codex (v0.162.1)\r\n\r\n› unchanged message\r\n\r\n• Working (0s • esc to interrupt)\r\n\r\n› Ask Codex to do anything\r\n\r\n  GPT-6-Luna low · Context 100% left · /tmp/ws".to_string();

    for (engine, ready_text, landed_text, submitted_text, hook_before_prompt) in [
        (
            "claude",
            claude_ready.as_str(),
            claude_landed.as_str(),
            claude_submitted.as_str(),
            true,
        ),
        (
            "claude",
            claude_ready.as_str(),
            claude_landed.as_str(),
            claude_submitted.as_str(),
            false,
        ),
        (
            "codex",
            codex_ready.as_str(),
            codex_landed.as_str(),
            codex_submitted.as_str(),
            true,
        ),
        (
            "codex",
            codex_ready.as_str(),
            codex_landed.as_str(),
            codex_submitted.as_str(),
            false,
        ),
    ] {
        let harness = Harness::new();
        let ws = TempDir::new().unwrap();
        let sender = harness.record_in(ws.path(), Phase::Exited, None, b"");
        let ready_file = ws.path().join(format!("{engine}-ready.bin"));
        let landed_file = ws.path().join(format!("{engine}-landed.bin"));
        let submitted_file = ws.path().join(format!("{engine}-submitted.bin"));
        std::fs::write(&ready_file, ready_text.as_bytes()).unwrap();
        std::fs::write(&landed_file, landed_text.as_bytes()).unwrap();
        std::fs::write(&submitted_file, submitted_text.as_bytes()).unwrap();
        let gate_file = ws.path().join(format!("{engine}-gate"));

        let script = if hook_before_prompt {
            "stty -echo -icanon; while [ ! -e \"$1\" ]; do sleep 0.02; done; cat \"$2\"; IFS= read -r -d '~' _ && IFS= read -r -d '~' _; cat \"$3\"; IFS= read -r -n 1 _; cat \"$4\"; sleep 300"
        } else {
            "stty -echo -icanon; cat \"$2\"; IFS= read -r -d '~' _ && IFS= read -r -d '~' _; cat \"$3\"; IFS= read -r -n 1 _; cat \"$4\"; sleep 300"
        };
        let start = harness
            .command()
            .args([
                "--json",
                "start",
                "--workspace",
                ws.path().to_str().unwrap(),
                "--tag",
                &format!("fresh-{engine}"),
                "--engine",
                engine,
                "--",
                "bash",
                "-c",
                script,
                "bash",
                gate_file.to_str().unwrap(),
                ready_file.to_str().unwrap(),
                landed_file.to_str().unwrap(),
                submitted_file.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(start.status.success(), "{start:?}");
        let started: Value = serde_json::from_slice(&start.stdout).unwrap();
        let recipient_id = started["id"].as_str().unwrap().to_string();

        let fire_session_start = || {
            let mut hook = harness
                .command()
                .env("APLEXER_SESSION_ID", &recipient_id)
                .args(["state-report", "working"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            hook.stdin
                .as_mut()
                .unwrap()
                .write_all(br#"{"hook_event_name":"SessionStart","session_id":"s-fresh"}"#)
                .unwrap();
            let out = hook.wait_with_output().unwrap();
            assert!(out.status.success(), "{out:?}");
        };

        if hook_before_prompt {
            fire_session_start();
            std::fs::write(&gate_file, b"go").unwrap();
        } else {
            // Wait until the ready prompt is drawn on the worker's screen, then
            // fire SessionStart afterwards.
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let cap = harness
                    .command()
                    .args(["capture", "--screen", "--plain", &recipient_id])
                    .output()
                    .unwrap();
                let text = String::from_utf8_lossy(&cap.stdout);
                if aplexer::delivery_ack::is_ready(engine, &text) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "prompt never became ready: {text}"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            fire_session_start();
        }

        // Wait until the ready prompt is drawn on the worker's screen.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let cap = harness
                .command()
                .args(["capture", "--screen", "--plain", &recipient_id])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&cap.stdout);
            if aplexer::delivery_ack::is_ready(engine, &text) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "prompt never became ready: {text}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        let recipient_record = aplexer::read_record(
            &harness
                .paths()
                .record(Uuid::parse_str(&recipient_id).unwrap()),
        )
        .unwrap();
        let envelope = queue_message(&harness, &sender, &recipient_record);
        let msg_id = envelope["id"].as_str().unwrap();

        let deliver = harness
            .command()
            .env("APLEXER_SESSION_ID", sender.id.to_string())
            .args([
                "--json",
                "message",
                "deliver",
                msg_id,
                "--workspace",
                recipient_record.workspace.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            deliver.status.success(),
            "{engine} (hook_before_prompt={hook_before_prompt}) failed deliver: stdout={} stderr={}",
            String::from_utf8_lossy(&deliver.stdout),
            String::from_utf8_lossy(&deliver.stderr)
        );
        status(&deliver, "submitted");

        let _ = harness
            .command()
            .args(["kill", &recipient_id, "--signal", "KILL", "--grace-ms", "0"])
            .output();
        let _ = Command::new("true").status();
    }
}

#[cfg(unix)]
#[test]
fn claude_post_stop_footer_and_ghost_redraw_keeps_idle_deliverable() {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let harness = Harness::new();
    let ws = TempDir::new().unwrap();
    let sender = harness.record_in(ws.path(), Phase::Exited, None, b"");

    let rule = "\u{2500}".repeat(48);
    let stop_hooks_screen = format!(
        "\x1b[?2004h\x1b[2J\x1b[H❯ Reply with ACKDONE\r\n\r\n● ACKDONE\r\n\r\n✢ Sublimating… (running Stop hooks… 1/3 · 19s)\r\n{rule}\r\n❯ \r\n{rule}\r\n  ⏵⏵ bypass permissions on · esc to interrupt"
    );
    let post_stop_redraw = format!(
        "\x1b[?2026h\x1b[2J\x1b[H❯ Reply with ACKDONE\r\n\r\n● ACKDONE\r\n\r\n✻ Cooked for 19s · done 3:08 PM\r\n{rule}\r\n❯ \x1b[2mack the message\x1b[0m\r\n{rule}\r\n  ⏵⏵ bypass permissions on · ↓ 208 tokens · thinking\x1b[?2026l"
    );
    let claude_landed = format!(
        "\x1b[2J\x1b[H❯ Reply with ACKDONE\r\n\r\n● ACKDONE\r\n\r\n✻ Cooked for 19s · done 3:08 PM\r\n{rule}\r\n❯ [Pasted text #1 +5 lines]\r\n{rule}\r\n  ⏵⏵ bypass permissions on"
    );
    let claude_submitted = format!(
        "\x1b[2J\x1b[H❯ Reply with ACKDONE\r\n\r\n● ACKDONE\r\n\r\n❯ unchanged message\r\n\r\n✶ Stewing…\r\n{rule}\r\n❯ \r\n{rule}\r\n  ⏵⏵ bypass permissions on · esc to interrupt"
    );

    let phase1_file = ws.path().join("phase1.bin");
    let phase2_file = ws.path().join("phase2.bin");
    let landed_file = ws.path().join("landed.bin");
    let submitted_file = ws.path().join("submitted.bin");
    let gate_file = ws.path().join("redraw-gate");
    std::fs::write(&phase1_file, stop_hooks_screen.as_bytes()).unwrap();
    std::fs::write(&phase2_file, post_stop_redraw.as_bytes()).unwrap();
    std::fs::write(&landed_file, claude_landed.as_bytes()).unwrap();
    std::fs::write(&submitted_file, claude_submitted.as_bytes()).unwrap();

    let start = harness
        .command()
        .args([
            "--json",
            "start",
            "--workspace",
            ws.path().to_str().unwrap(),
            "--tag",
            "claude-stop-redraw",
            "--engine",
            "claude",
            "--",
            "bash",
            "-c",
            "stty -echo -icanon; cat \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.02; done; cat \"$3\"; IFS= read -r -d '~' _ && IFS= read -r -d '~' _; cat \"$4\"; IFS= read -r -n 1 _; cat \"$5\"; sleep 300",
            "bash",
            phase1_file.to_str().unwrap(),
            gate_file.to_str().unwrap(),
            phase2_file.to_str().unwrap(),
            landed_file.to_str().unwrap(),
            submitted_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(start.status.success(), "{start:?}");
    let started: Value = serde_json::from_slice(&start.stdout).unwrap();
    let recipient_id = started["id"].as_str().unwrap().to_string();

    // Wait for phase 1 screen to be parsed by the worker.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let cap = harness
            .command()
            .args(["capture", "--screen", "--plain", &recipient_id])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&cap.stdout);
        if text.contains("running Stop hooks") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "phase 1 screen never appeared: {text}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Run `a state-report gated-idle` during Stop hooks, just as Claude Code does.
    let mut stop_hook = harness
        .command()
        .env("APLEXER_SESSION_ID", &recipient_id)
        .args(["state-report", "gated-idle"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    stop_hook
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"hook_event_name":"Stop","session_id":"s-stop","background_tasks":[]}"#)
        .unwrap();
    let out = stop_hook.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");

    // Sleep past IDLE_ACTIVITY_GRACE_MS (2_000 ms), then trigger the post-Stop
    // status/footer/ghost-suggestion redraw.
    std::thread::sleep(Duration::from_millis(2_300));
    std::fs::write(&gate_file, b"go").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let cap = harness
            .command()
            .args(["capture", "--screen", "--plain", &recipient_id])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&cap.stdout);
        if text.contains("Cooked for 19s") && text.contains("ack the message") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "post-Stop redraw never appeared: {text}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let recipient_record = aplexer::read_record(
        &harness
            .paths()
            .record(Uuid::parse_str(&recipient_id).unwrap()),
    )
    .unwrap();
    let envelope = queue_message(&harness, &sender, &recipient_record);
    let msg_id = envelope["id"].as_str().unwrap();

    let deliver = harness
        .command()
        .env("APLEXER_SESSION_ID", sender.id.to_string())
        .args([
            "--json",
            "message",
            "deliver",
            msg_id,
            "--workspace",
            recipient_record.workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        deliver.status.success(),
        "post-Stop redraw contradicted idle: stdout={} stderr={}",
        String::from_utf8_lossy(&deliver.stdout),
        String::from_utf8_lossy(&deliver.stderr)
    );
    status(&deliver, "submitted");

    let _ = harness
        .command()
        .args(["kill", &recipient_id, "--signal", "KILL", "--grace-ms", "0"])
        .output();
}
