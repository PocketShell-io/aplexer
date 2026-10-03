use super::*;

fn delivery_recipient(message: &MessageEnvelope) -> Result<Uuid> {
    match message.to {
        Recipient::Tag {
            session_id: Some(id),
            ..
        } => Ok(id),
        _ => bail!("message delivery requires one recorded recipient session UUID"),
    }
}

fn authorize_delivery(records: &[SessionRecord], message: &MessageEnvelope) -> Result<()> {
    let id =
        discover_session_id().ok_or_else(|| anyhow!("delivery requires a session identity"))?;
    let caller = records
        .iter()
        .find(|record| record.id == id)
        .ok_or_else(|| anyhow!("calling session is no longer recorded"))?;
    let source = message
        .from
        .workspace
        .as_deref()
        .unwrap_or(&message.workspace);
    if message.from.session_id == Some(id) && caller.workspace == source {
        return Ok(());
    }
    if delivery_recipient(message)? == id && caller.workspace == message.workspace {
        return Ok(());
    }
    bail!(
        "only the original sender or recipient may deliver message {}",
        message.id
    )
}

fn delivery_target<'a>(
    records: &'a [SessionRecord],
    message: &MessageEnvelope,
) -> Result<&'a SessionRecord> {
    let id = delivery_recipient(message)?;
    records
        .iter()
        .find(|record| record.id == id && record.workspace == message.workspace)
        .ok_or_else(|| anyhow!("original recipient session {id} is no longer recorded"))
}

fn require_ready_prompt(record: &SessionRecord) -> Result<()> {
    if !record.worker_alive() {
        bail!("recipient worker is not running");
    }
    let raw = rpc_simple(record, Operation::Status, None)?;
    let live: SessionRecord = serde_json::from_value(raw).context("read live recipient status")?;
    let now = now_ms();

    let screen_bytes = rpc_capture_screen(record, true)
        .context("capture screen to verify recipient composer is ready")?;
    let screen_text = String::from_utf8_lossy(&screen_bytes);
    let prompt_state = classify_composer_prompt(&screen_text, &record.engine);

    match evaluate_readiness_verdict(&prompt_state, &live, now) {
        ReadinessVerdict::Ready => Ok(()),
        ReadinessVerdict::Reject(err) => bail!("{err}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadinessVerdict {
    Ready,
    Reject(String),
}

pub(crate) fn evaluate_readiness_verdict(
    prompt_state: &PromptState,
    record: &SessionRecord,
    now: u64,
) -> ReadinessVerdict {
    match prompt_state {
        PromptState::Draft(draft) => {
            return ReadinessVerdict::Reject(format!(
                "recipient composer has an unsubmitted draft in progress ({draft}); delivery fail-closed"
            ));
        }
        PromptState::Unknown(reason) => {
            return ReadinessVerdict::Reject(format!(
                "recipient composer prompt state is unknown ({reason}); delivery fail-closed"
            ));
        }
        PromptState::Empty => {}
    }

    // 2. Active running (whether reported or inferred/heuristic) fails closed
    let (state, source) = session_ui_state(record, now);
    if state == "running" {
        return ReadinessVerdict::Reject(readiness_detail(record, state, source, now));
    }

    let has_hooks = aplexer::hooks::session_record_has_lifecycle_hooks(record);

    // 3. Contradicted resting state check:
    // If the agent reported resting ('idle' or 'waiting'), did newer PTY activity land after the push?
    // Engine-specific: Antigravity background cursor/timer redraws are exempted; all other engines fail closed.
    if let (Some(rep), Some(at)) = (record.reported_state.as_deref(), record.reported_state_at_ms) {
        if rep == "idle" && aplexer::watch::idle_was_contradicted_with_hooks(record, at, has_hooks) {
            return ReadinessVerdict::Reject(format!(
                "recipient reported idle at {at}ms, but subsequent PTY activity contradicted resting state; delivery fail-closed"
            ));
        }
        if rep == "waiting" && record.engine != "antigravity" {
            if let Some(last_act) = record.last_activity_ms {
                if last_act > at.saturating_add(aplexer::watch::IDLE_ACTIVITY_GRACE_MS) {
                    return ReadinessVerdict::Reject(format!(
                        "recipient reported waiting at {at}ms, but subsequent PTY activity occurred at {last_act}ms (+{}ms); resting state contradicted, delivery fail-closed",
                        last_act.saturating_sub(at)
                    ));
                }
            }
        }
    }

    // 4. Only authentic fresh resting report (uncontradicted idle or waiting) is ready:
    if source == "reported" && matches!(state, "waiting" | "idle") {
        return ReadinessVerdict::Ready;
    }

    // All other states (stale reports, inferred/heuristic states without fresh report, no hooks) fail closed.
    // Expired waiting + empty composer does NOT prove completed turn; fail closed.
    ReadinessVerdict::Reject(readiness_detail(record, state, source, now))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PromptState {
    Empty,
    Draft(String),
    Unknown(String),
}

#[allow(dead_code)]
pub(crate) fn detect_composer_draft(screen_text: &str, engine: &str) -> Option<String> {
    match classify_composer_prompt(screen_text, engine) {
        PromptState::Draft(draft) => Some(draft),
        _ => None,
    }
}

pub(crate) fn classify_composer_prompt(screen_text: &str, engine: &str) -> PromptState {
    if screen_text.contains("[draft in progress]") {
        return PromptState::Draft("unsubmitted user draft detected".into());
    }

    let all_lines: Vec<&str> = screen_text.lines().collect();
    if all_lines.is_empty() {
        return PromptState::Unknown("screen capture is empty".into());
    }

    fn is_footer_or_status(line: &str) -> bool {
        let t = line.trim();
        if t.is_empty() {
            return false;
        }
        t.contains("ctrl+")
            || t.contains("Ctrl+")
            || t.contains("^C")
            || t.contains("ESC")
            || t.contains("commands")
            || t.contains("shortcuts")
            || t.contains("Shift+Tab")
            || t.contains("Normal interactive session requested")
            || t.starts_with("tokens:")
            || t.starts_with("model:")
            || t.starts_with("? help")
            || t.starts_with("? for shortcuts")
    }

    let mut end_idx = all_lines.len();
    while end_idx > 0 {
        let line = all_lines[end_idx - 1].trim();
        if line.is_empty() || is_footer_or_status(line) {
            end_idx -= 1;
        } else {
            break;
        }
    }
    if end_idx == 0 {
        return PromptState::Unknown("screen only contains footers or empty lines".into());
    }

    let active_slice = &all_lines[..end_idx];

    match engine {
        "opencode" => {
            let mut box_lines = Vec::new();
            let mut saw_bottom_border = false;
            for &line in active_slice.iter().rev() {
                let trimmed = line.trim();
                if trimmed.starts_with('╹')
                    || trimmed.starts_with('▀')
                    || trimmed.starts_with('└')
                    || trimmed.starts_with('╰')
                {
                    saw_bottom_border = true;
                    continue;
                }
                if trimmed.starts_with('┃') || trimmed.starts_with('│') {
                    box_lines.push(line);
                } else if trimmed.starts_with('┌')
                    || trimmed.starts_with('╭')
                    || trimmed.starts_with('+')
                {
                    break;
                } else if !box_lines.is_empty() {
                    break;
                }
            }

            if box_lines.is_empty() && !saw_bottom_border {
                return PromptState::Unknown("opencode composer box not recognized".into());
            }

            for line in box_lines.iter().rev() {
                let trimmed = line.trim();
                let inner = trimmed
                    .trim_matches(|c| {
                        c == '┃'
                            || c == '│'
                            || c == '┌'
                            || c == '┐'
                            || c == '└'
                            || c == '┘'
                            || c == '─'
                            || c == '╹'
                            || c == '▀'
                    })
                    .trim();
                if inner.is_empty()
                    || inner == "Ask a question..."
                    || inner == "Type a message..."
                    || inner.starts_with("Build auto")
                    || inner.contains("ctrl+p")
                    || inner.contains("commands")
                {
                    continue;
                }
                return PromptState::Draft(inner.to_string());
            }
            PromptState::Empty
        }
        "codex" | "zcodex" => {
            let mut prompt_idx = None;
            for (idx, &line) in active_slice.iter().enumerate().rev() {
                let trimmed = line.trim();
                if trimmed.contains('›') || trimmed.contains('❯') {
                    prompt_idx = Some(idx);
                    break;
                }
            }

            let Some(p_idx) = prompt_idx else {
                return PromptState::Unknown("codex prompt marker not found".into());
            };

            let prompt_line = active_slice[p_idx].trim();
            let prompt_char = if prompt_line.contains('›') { '›' } else { '❯' };
            let pos = prompt_line.find(prompt_char).unwrap();
            let prompt_text = prompt_line[pos + prompt_char.len_utf8()..].trim();

            if !prompt_text.is_empty() && prompt_text != "Ask Codex to do anything" {
                return PromptState::Draft(prompt_text.to_string());
            }

            for &line in &active_slice[p_idx + 1..] {
                let trimmed = line.trim();
                if trimmed.is_empty()
                    || is_footer_or_status(trimmed)
                    || trimmed.contains("glm-")
                    || trimmed.contains("Normal interactive session")
                {
                    continue;
                }
                return PromptState::Draft(trimmed.to_string());
            }

            PromptState::Empty
        }
        "grok" => {
            let mut prompt_idx = None;
            for (idx, &line) in active_slice.iter().enumerate().rev() {
                let trimmed = line.trim();
                if trimmed.contains('❯') {
                    prompt_idx = Some(idx);
                    break;
                }
            }

            let Some(p_idx) = prompt_idx else {
                return PromptState::Unknown("grok prompt marker not found".into());
            };

            let prompt_line = active_slice[p_idx].trim();
            let pos = prompt_line.find('❯').unwrap();
            let after = prompt_line[pos + '❯'.len_utf8()..]
                .trim()
                .trim_end_matches('│')
                .trim();
            if !after.is_empty() {
                return PromptState::Draft(after.to_string());
            }

            for &line in &active_slice[p_idx + 1..] {
                let trimmed = line.trim();
                if trimmed.is_empty()
                    || is_footer_or_status(trimmed)
                    || trimmed.starts_with('╰')
                    || trimmed.contains("Grok")
                    || trimmed.contains("always-approve")
                {
                    continue;
                }
                let inner = trimmed.trim_end_matches('│').trim();
                if !inner.is_empty() {
                    return PromptState::Draft(inner.to_string());
                }
            }

            PromptState::Empty
        }
        "shell" => {
            let mut prompt_found = false;
            if let Some(&last_line) = active_slice.last() {
                let trimmed = last_line.trim();
                for prompt_char in ['$', '#', '%', '❯', '>'] {
                    if let Some(pos) = trimmed.rfind(prompt_char) {
                        prompt_found = true;
                        let after = trimmed[pos + prompt_char.len_utf8()..].trim();
                        if !after.is_empty() {
                            return PromptState::Draft(after.to_string());
                        }
                    }
                }
            }
            if prompt_found {
                PromptState::Empty
            } else {
                PromptState::Unknown("shell prompt marker not found".into())
            }
        }
        _ => {
            let mut prompt_idx = None;
            for (idx, &line) in active_slice.iter().enumerate().rev() {
                let trimmed = line.trim();
                if trimmed.contains('›')
                    || trimmed.contains('❯')
                    || trimmed.ends_with('$')
                    || trimmed.ends_with('%')
                {
                    prompt_idx = Some(idx);
                    break;
                }
            }

            let Some(p_idx) = prompt_idx else {
                return PromptState::Unknown("generic prompt marker not found".into());
            };

            let prompt_line = active_slice[p_idx].trim();
            for prompt_char in ['›', '❯', '$', '%', '>'] {
                if let Some(pos) = prompt_line.find(prompt_char) {
                    let after = prompt_line[pos + prompt_char.len_utf8()..]
                        .trim()
                        .trim_end_matches('│')
                        .trim();
                    if !after.is_empty() && after != "Ask Codex to do anything" {
                        return PromptState::Draft(after.to_string());
                    }
                }
            }
            for &line in &active_slice[p_idx + 1..] {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !is_footer_or_status(trimmed) {
                    return PromptState::Draft(trimmed.to_string());
                }
            }

            PromptState::Empty
        }
    }
}

fn readiness_detail(record: &SessionRecord, state: &str, source: &str, now: u64) -> String {
    let reason = readiness_reason(record, state, source, now);
    format!(
        "recipient {} readiness unavailable: {reason}; derived={state} source={source}; reported={:?} reported_at_ms={:?} last_activity_ms={:?}. Prompt capture does not refresh harness state. The original recipient must obtain a genuine harness state event; re-inspect its prompt before explicitly delivering this same message. If no current harness event is available, leave the message queued.",
        record.id, record.reported_state, record.reported_state_at_ms, record.last_activity_ms,
    )
}

fn readiness_reason(record: &SessionRecord, state: &str, source: &str, now: u64) -> &'static str {
    if record.phase != Phase::Running || source == "lifecycle" {
        return "recipient lifecycle does not accept delivery";
    }
    if state == "running" && source == "reported" {
        return "recipient reported working";
    }
    aplexer::watch::reported_state_rejection(record, now)
        .unwrap_or("recipient semantic readiness unavailable")
}

fn deliver_existing(paths: &Paths, args: MessageDeliverArgs) -> Result<SubmissionOutcome> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let mp = ensure_workspace(paths, &workspace)?;
    let records = list_records(paths)?;
    let message = match args.message_id {
        Some(id) => read_message_in(&mp, &workspace, id)?,
        None => find_next_queued_message(paths, &records)?,
    };
    authorize_delivery(&records, &message)?;
    delivery_recipient(&message)?;
    submit_message_in(
        &mp,
        &workspace,
        message.id,
        |current| {
            check_body_size(&current.body)?;
            require_ready_prompt(delivery_target(&records, current)?)
        },
        |current| {
            let target = delivery_target(&records, current)?;
            let pre_state = target.reported_state.as_deref().unwrap_or("idle");
            rpc_send_submitted(
                target,
                &pane_input_bytes(current, false, false),
                SubmissionKind::FramedMessage,
            )?;
            verify_pane_delivery(target, current, false, pre_state)
        },
    )
}

fn find_next_queued_message(paths: &Paths, records: &[SessionRecord]) -> Result<MessageEnvelope> {
    let caller_id = discover_session_id()
        .ok_or_else(|| anyhow!("continuation queue delivery requires a session identity"))?;
    let caller = records
        .iter()
        .find(|r| r.id == caller_id)
        .ok_or_else(|| anyhow!("calling session is no longer recorded"))?;

    let unread = aplexer::coordination::unread_messages(paths, caller.id)?;
    for msg in unread {
        let mp = message_paths(paths, &msg.workspace);
        if msg.delivery == Delivery::Inbox
            && !mp.msgs_dir.join(format!("{}.attempt", msg.id)).exists()
        {
            return Ok(msg);
        }
    }
    bail!("no queued inbox messages found for session {}", caller.tag);
}

pub(crate) fn cmd_message_deliver(
    paths: &Paths,
    args: MessageDeliverArgs,
    json_output: bool,
) -> Result<()> {
    let outcome = deliver_existing(paths, args)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!("{} {}", outcome.id, serde_json::to_value(outcome.status)?);
        if let Some(detail) = &outcome.detail {
            eprintln!("{detail}");
        }
    }
    if matches!(
        outcome.status,
        SubmissionStatus::NotReady | SubmissionStatus::DeliveryUncertain
    ) {
        bail!("message {} was not confirmed submitted; inspect its delivery status before further action", outcome.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opencode_real_empty_capture() {
        let screen = "  ┃  Build auto · Muse Spark 1.3 Contributor OpenCode Go                                     \n  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀  \n   /home/alexey/git/cloudflare-agent-git              434.1K (41%) · $0.52  ctrl+p commands  ";
        assert_eq!(
            classify_composer_prompt(screen, "opencode"),
            PromptState::Empty
        );
    }

    #[test]
    fn test_opencode_composer_empty_with_placeholder() {
        let screen = "│ Chat message 1 │\n│ Chat message 2 │\n┌────────────────┐\n│ Ask a question... │\n└────────────────┘\nctrl+p commands";
        assert_eq!(
            classify_composer_prompt(screen, "opencode"),
            PromptState::Empty
        );
    }

    #[test]
    fn test_opencode_composer_single_line_draft() {
        let screen = "│ Chat message 1 │\n┌────────────────┐\n│ my draft text  │\n└────────────────┘\nctrl+p commands";
        assert_eq!(
            classify_composer_prompt(screen, "opencode"),
            PromptState::Draft("my draft text".into())
        );
    }

    #[test]
    fn test_opencode_composer_multiline_draft_long() {
        let mut screen = String::from("│ Chat message 1 │\n┌────────────────┐\n");
        for i in 1..=15 {
            screen.push_str(&format!("│ line {i} of draft │\n"));
        }
        screen.push_str("└────────────────┘\nctrl+p commands");
        assert_eq!(
            classify_composer_prompt(&screen, "opencode"),
            PromptState::Draft("line 1 of draft".into())
        );
    }

    #[test]
    fn test_grok_real_empty_capture() {
        let screen = "  ╭───────────────────────────────────────────────────────────────────────────────────────╮  \n  │ ❯                                                                                     │  \n  ╰────────────────────────────────────────────────── Grok 4.7 (medium) · always-approve ─╯  \n                                                                                             \n  Shift+Tab:mode  │  Ctrl+.:shortcuts                                                        ";
        assert_eq!(classify_composer_prompt(screen, "grok"), PromptState::Empty);
    }

    #[test]
    fn test_grok_draft_capture() {
        let screen = "  ╭───────────────────────────────────────────────────────────────────────────────────────╮  \n  │ ❯ unsubmitted grok draft                                                              │  \n  ╰────────────────────────────────────────────────── Grok 4.7 (medium) · always-approve ─╯  \n                                                                                             \n  Shift+Tab:mode  │  Ctrl+.:shortcuts                                                        ";
        assert_eq!(
            classify_composer_prompt(screen, "grok"),
            PromptState::Draft("unsubmitted grok draft".into())
        );
    }

    #[test]
    fn test_codex_real_empty_capture() {
        let screen = "  Worked for 29m 26s · 4:04 AM\n \n \n› Ask Codex to do anything\n \n  glm-5.3-flash max · ~/git/cloudflare-agent-git · Normal interactive session requested";
        assert_eq!(
            classify_composer_prompt(screen, "codex"),
            PromptState::Empty
        );
    }

    #[test]
    fn test_codex_composer_empty_just_prompt() {
        let screen = "› cargo test\ntest result: ok\n› \n? for shortcuts";
        assert_eq!(
            classify_composer_prompt(screen, "codex"),
            PromptState::Empty
        );
    }

    #[test]
    fn test_codex_composer_single_line_draft() {
        let screen =
            "› cargo test\ntest result: ok\n› refactor this module\n? for shortcuts";
        assert_eq!(
            classify_composer_prompt(screen, "codex"),
            PromptState::Draft("refactor this module".into())
        );
    }

    #[test]
    fn test_codex_composer_multiline_draft_long() {
        let mut screen = String::from("› cargo test\ntest result: ok\n› line 1 of draft\n");
        for i in 2..=15 {
            screen.push_str(&format!("  line {i} of draft\n"));
        }
        screen.push_str("? for shortcuts");
        assert_eq!(
            classify_composer_prompt(&screen, "codex"),
            PromptState::Draft("line 1 of draft".into())
        );
    }

    #[test]
    fn test_shell_engine_empty_prompt() {
        let screen = "alexey@host:~$ ls -la\ntotal 0\nalexey@host:~$ ";
        assert_eq!(
            classify_composer_prompt(screen, "shell"),
            PromptState::Empty
        );
    }

    #[test]
    fn test_shell_engine_active_draft() {
        let screen = "alexey@host:~$ ls -la\ntotal 0\nalexey@host:~$ git status";
        assert_eq!(
            classify_composer_prompt(screen, "shell"),
            PromptState::Draft("git status".into())
        );
    }

    #[test]
    fn test_unknown_unrecognized_screen() {
        let screen = "just some random output with no recognizable prompt or borders\nanother line";
        assert!(matches!(
            classify_composer_prompt(screen, "codex"),
            PromptState::Unknown(_)
        ));
    }

    #[test]
    fn test_readiness_verdict_rejects_draft() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "idle",
            "reported_state_at_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Draft("my draft".into());
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 1000);
        assert!(
            matches!(verdict, ReadinessVerdict::Reject(err) if err.contains("unsubmitted draft in progress"))
        );
    }

    #[test]
    fn test_readiness_verdict_rejects_unknown() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "idle",
            "reported_state_at_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Unknown("garbled screen".into());
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 1000);
        assert!(
            matches!(verdict, ReadinessVerdict::Reject(err) if err.contains("prompt state is unknown"))
        );
    }

    #[test]
    fn test_readiness_verdict_rejects_active_running() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "running",
            "reported_state_at_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 1500);
        assert!(matches!(verdict, ReadinessVerdict::Reject(_)));
    }

    #[test]
    fn test_readiness_verdict_accepts_fresh_idle() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "idle",
            "reported_state_at_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 1500);
        assert_eq!(verdict, ReadinessVerdict::Ready);
    }

    #[test]
    fn test_readiness_verdict_accepts_fresh_waiting() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "waiting",
            "reported_state_at_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 1500);
        assert_eq!(verdict, ReadinessVerdict::Ready);
    }

    #[test]
    fn test_readiness_verdict_rejects_old_idle_with_subsequent_pty_activity() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "idle",
            "reported_state_at_ms": 1000,
            "last_activity_ms": 4000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 5000);
        assert!(
            matches!(verdict, ReadinessVerdict::Reject(err) if err.contains("resting state contradicted"))
        );
    }

    #[test]
    fn test_readiness_verdict_rejects_inferred_running_with_empty_composer() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "shell",
            "command": ["bash"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "last_activity_ms": 9500,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 10000);
        assert!(matches!(verdict, ReadinessVerdict::Reject(_)));
    }

    #[test]
    fn test_readiness_verdict_rejects_old_waiting_with_newer_activity() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "waiting",
            "reported_state_at_ms": 1000,
            "last_activity_ms": 4500,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 5000);
        assert!(
            matches!(verdict, ReadinessVerdict::Reject(err) if err.contains("resting state contradicted"))
        );
    }

    #[test]
    fn test_readiness_verdict_rejects_expired_waiting_without_hooks() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "shell",
            "command": ["bash"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "waiting",
            "reported_state_at_ms": 1000,
            "last_activity_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 20000);
        assert!(matches!(verdict, ReadinessVerdict::Reject(_)));
    }

    #[test]
    fn test_readiness_verdict_accepts_antigravity_idle_with_background_timer_redraw() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "antigravity",
            "command": ["agy"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "idle",
            "reported_state_at_ms": 1000,
            "last_activity_ms": 5000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        // Antigravity TUI continuous background timer/cursor redraws are exempted from contradicting idle
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 5500);
        assert_eq!(verdict, ReadinessVerdict::Ready);
    }

    #[test]
    fn test_readiness_verdict_rejects_expired_waiting_even_with_empty_composer_and_hooks() {
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "/tmp",
            "tag": "test",
            "engine": "opencode",
            "command": ["opencode"],
            "cwd": "/tmp",
            "phase": "running",
            "history_bytes": 0,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "reported_state": "waiting",
            "reported_state_at_ms": 1000,
            "last_activity_ms": 1000,
        }))
        .unwrap();
        let prompt_state = PromptState::Empty;
        // After 20 seconds, waiting report is expired (>8s). Even with empty composer and hooks,
        // it must reject because silent background tool execution can share an empty composer footer.
        let verdict = evaluate_readiness_verdict(&prompt_state, &record, 20000);
        assert!(matches!(verdict, ReadinessVerdict::Reject(_)));
    }
}


