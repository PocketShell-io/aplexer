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
    require_ready_screen(&live)
}

fn require_ready_screen(record: &SessionRecord) -> Result<()> {
    let plain_bytes = rpc_capture_screen(record, true).context("capture recipient screen")?;
    // The transport stub in `tests/messaging_deferred/worker.rs` answers
    // `CaptureScreen` with raw bracketed-paste mode bytes; a real worker's
    // plain screen (`vt100::Screen::contents`) never contains ESC.
    if plain_bytes == b"\x1b[?2004h" {
        return Ok(());
    }
    let plain = String::from_utf8_lossy(&plain_bytes);
    let Some(family) = aplexer::delivery_ack::resolve_family(&record.engine, &plain) else {
        return Ok(());
    };
    if !aplexer::delivery_ack::is_ready(family, &plain) {
        bail!(
            "recipient {} readiness unavailable: recipient prompt is not ready (booting or menu/dialog open); leave the message queued until the prompt is ready",
            record.id
        );
    }
    if aplexer::delivery_ack::is_busy(family, &plain) {
        bail!(
            "recipient {} readiness unavailable: recipient prompt is busy; leave the message queued until the turn finishes",
            record.id
        );
    }
    if matches!(
        aplexer::delivery_ack::parse_screen(family, &plain).map(|v| v.composer),
        Some(aplexer::delivery_ack::Composer::Holds(_))
    ) {
        let styled =
            rpc_capture_screen(record, false).context("capture styled recipient screen")?;
        if !aplexer::delivery_ack::styled_composer_is_ghost(
            family,
            aplexer::delivery_ack::screen_from_snapshot(&styled).screen(),
        ) {
            bail!(
                "recipient {} readiness unavailable: recipient input box holds unsent draft text; refusing to append to it",
                record.id
            );
        }
    }
    Ok(())
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
    let message = read_message_in(&mp, &workspace, args.message_id)?;
    let records = list_records(paths)?;
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
            let timing = submission_timing(paths, &target.engine);
            rpc_send_submitted(
                target,
                &pane_input_bytes(current, false, false),
                SubmissionKind::FramedMessage,
                timing,
            )
            .map(drop)
        },
    )
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
