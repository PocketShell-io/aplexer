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
    let (state, source) = session_ui_state(&live, now);
    if source != "reported" || !matches!(state, "waiting" | "idle") {
        bail!("{}", readiness_detail(&live, state, source, now));
    }
    check_prompt_empty(record)?;
    Ok(())
}

fn check_prompt_empty(record: &SessionRecord) -> Result<()> {
    let screen_bytes = rpc_capture_screen(record, true)
        .context("capture screen to verify recipient composer is ready")?;
    let screen_text = String::from_utf8_lossy(&screen_bytes);
    if let Some(draft) = detect_composer_draft(&screen_text, &record.engine) {
        bail!("recipient composer has an unsubmitted draft in progress ({draft}); delivery fail-closed");
    }
    Ok(())
}

pub(crate) fn detect_composer_draft(screen_text: &str, engine: &str) -> Option<String> {
    if screen_text.contains("[draft in progress]") {
        return Some("unsubmitted user draft detected".into());
    }

    let all_lines: Vec<&str> = screen_text.lines().collect();
    if all_lines.is_empty() {
        return None;
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
            || t.contains("Build auto")
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
        return None;
    }

    let active_slice = &all_lines[..end_idx];

    match engine {
        "opencode" => {
            let mut box_lines = Vec::new();
            for &line in active_slice.iter().rev() {
                let trimmed = line.trim();
                if trimmed.starts_with('┃')
                    || trimmed.starts_with('│')
                    || trimmed.ends_with('┃')
                    || trimmed.ends_with('│')
                {
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
                    })
                    .trim();
                if !inner.is_empty()
                    && inner != "Ask a question..."
                    && inner != "Type a message..."
                    && !inner.contains("ctrl+p")
                    && !inner.contains("commands")
                {
                    return Some(inner.to_string());
                }
            }
            None
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

            if let Some(p_idx) = prompt_idx {
                let prompt_line = active_slice[p_idx].trim();
                let prompt_char = if prompt_line.contains('›') { '›' } else { '❯' };
                let pos = prompt_line.find(prompt_char).unwrap();
                let prompt_text = prompt_line[pos + prompt_char.len_utf8()..].trim();

                if !prompt_text.is_empty() && prompt_text != "Ask Codex to do anything" {
                    return Some(prompt_text.to_string());
                }

                for &line in &active_slice[p_idx + 1..] {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !is_footer_or_status(trimmed) {
                        return Some(trimmed.to_string());
                    }
                }
            }
            None
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
            if let Some(p_idx) = prompt_idx {
                let prompt_line = active_slice[p_idx].trim();
                let pos = prompt_line.find('❯').unwrap();
                let after = prompt_line[pos + '❯'.len_utf8()..]
                    .trim()
                    .trim_end_matches('│')
                    .trim();
                if !after.is_empty() {
                    return Some(after.to_string());
                }
                for &line in &active_slice[p_idx + 1..] {
                    let trimmed = line.trim().trim_end_matches('│').trim();
                    if !trimmed.is_empty() && !is_footer_or_status(trimmed) {
                        return Some(trimmed.to_string());
                    }
                }
            }
            None
        }
        "shell" => {
            if let Some(&last_line) = active_slice.last() {
                let trimmed = last_line.trim();
                for prompt_char in ['$', '#', '%', '❯', '>'] {
                    if let Some(pos) = trimmed.rfind(prompt_char) {
                        let after = trimmed[pos + prompt_char.len_utf8()..].trim();
                        if !after.is_empty() {
                            return Some(after.to_string());
                        }
                    }
                }
            }
            None
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
            if let Some(p_idx) = prompt_idx {
                let prompt_line = active_slice[p_idx].trim();
                for prompt_char in ['›', '❯', '$', '%', '>'] {
                    if let Some(pos) = prompt_line.find(prompt_char) {
                        let after = prompt_line[pos + prompt_char.len_utf8()..]
                            .trim()
                            .trim_end_matches('│')
                            .trim();
                        if !after.is_empty() && after != "Ask Codex to do anything" {
                            return Some(after.to_string());
                        }
                    }
                }
                for &line in &active_slice[p_idx + 1..] {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !is_footer_or_status(trimmed) {
                        return Some(trimmed.to_string());
                    }
                }
            }
            None
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
    fn test_opencode_composer_empty_with_placeholder() {
        let screen = "│ Chat message 1 │\n│ Chat message 2 │\n┌────────────────┐\n│ Ask a question... │\n└────────────────┘\nctrl+p commands";
        assert_eq!(detect_composer_draft(screen, "opencode"), None);
    }

    #[test]
    fn test_opencode_composer_single_line_draft() {
        let screen = "│ Chat message 1 │\n┌────────────────┐\n│ my draft text  │\n└────────────────┘\nctrl+p commands";
        assert_eq!(
            detect_composer_draft(screen, "opencode"),
            Some("my draft text".into())
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
            detect_composer_draft(&screen, "opencode"),
            Some("line 1 of draft".into())
        );
    }

    #[test]
    fn test_codex_composer_empty_with_placeholder() {
        let screen =
            "› cargo test\ntest result: ok\n› Ask Codex to do anything\n? for shortcuts";
        assert_eq!(detect_composer_draft(screen, "codex"), None);
    }

    #[test]
    fn test_codex_composer_empty_just_prompt() {
        let screen = "› cargo test\ntest result: ok\n› \n? for shortcuts";
        assert_eq!(detect_composer_draft(screen, "codex"), None);
    }

    #[test]
    fn test_codex_composer_single_line_draft() {
        let screen =
            "› cargo test\ntest result: ok\n› refactor this module\n? for shortcuts";
        assert_eq!(
            detect_composer_draft(screen, "codex"),
            Some("refactor this module".into())
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
            detect_composer_draft(&screen, "codex"),
            Some("line 1 of draft".into())
        );
    }

    #[test]
    fn test_shell_engine_empty_prompt() {
        let screen = "alexey@host:~$ ls -la\ntotal 0\nalexey@host:~$ ";
        assert_eq!(detect_composer_draft(screen, "shell"), None);
    }

    #[test]
    fn test_shell_engine_active_draft() {
        let screen = "alexey@host:~$ ls -la\ntotal 0\nalexey@host:~$ git status";
        assert_eq!(
            detect_composer_draft(screen, "shell"),
            Some("git status".into())
        );
    }
}

