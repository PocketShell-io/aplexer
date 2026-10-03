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
    let screen_bytes = match rpc_capture_screen(record, true) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(()),
    };
    let screen_text = String::from_utf8_lossy(&screen_bytes);
    if let Some(draft) = detect_composer_draft(&screen_text, &record.engine) {
        bail!("recipient composer has an unsubmitted draft in progress ({draft}); delivery fail-closed");
    }
    Ok(())
}

pub(crate) fn detect_composer_draft(screen_text: &str, _engine: &str) -> Option<String> {
    for line in screen_text.lines() {
        let trimmed = line.trim();
        if trimmed.contains("[draft in progress]") {
            return Some("unsubmitted user draft detected".into());
        }
        if let Some(pos) = trimmed.find('❯') {
            let after = trimmed[pos + '❯'.len_utf8()..]
                .trim()
                .trim_end_matches('│')
                .trim();
            if !after.is_empty() && after != "Ask Codex to do anything" {
                return Some(after.to_string());
            }
        }
    }
    None
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
            rpc_send_submitted(
                target,
                &pane_input_bytes(current, false, false),
                SubmissionKind::FramedMessage,
            )?;
            verify_pane_delivery(target, current, false)
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
