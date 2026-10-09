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

/// The original sender or recipient session may deliver. A caller with no
/// session identity has exactly the authority it had to send: it may deliver
/// external mail (no sender session) in the mailbox of the directory it runs
/// in, never session mail and never across workspaces via `--workspace`.
fn authorize_delivery(
    records: &[SessionRecord],
    message: &MessageEnvelope,
    explicit_workspace: bool,
) -> Result<()> {
    let Some(id) = discover_session_id() else {
        if message.from.external && message.from.session_id.is_none() && !explicit_workspace {
            return Ok(());
        }
        bail!(
            "delivery requires a session identity: without one, only external mail in the \
             current directory's own workspace may be delivered (no --workspace)"
        );
    };
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
    let reported_rest = source == "reported" && matches!(state, "waiting" | "idle");
    if !observable_composer(&live) {
        if reported_rest {
            return Ok(());
        }
        bail!("{}", readiness_detail(&live, state, source, now));
    }
    // A waiting report goes stale in seconds while the agent keeps sitting
    // at its prompt, so for an observable composer the empty prompt is the
    // readiness evidence -- but never for an agent reported or seen working,
    // which would take the message mid-turn, and never over a draft.
    if source == "lifecycle" || state == "running" {
        bail!("{}", readiness_detail(&live, state, source, now));
    }
    let composer = capture_composer(record)?;
    if composer.empty {
        return Ok(());
    }
    let prompt = format!(
        "recipient {} is not at an empty input prompt (an unsent draft or a dialog is \
         showing); nothing was written. Composer: {:?}",
        record.id, composer.region
    );
    if reported_rest {
        bail!("{prompt}");
    }
    bail!("{prompt}. {}", readiness_detail(&live, state, source, now))
}

fn readiness_detail(record: &SessionRecord, state: &str, source: &str, now: u64) -> String {
    let reason = readiness_reason(record, state, source, now);
    format!(
        "recipient {} readiness unavailable: {reason}; derived={state} source={source}; reported={:?} reported_at_ms={:?} last_activity_ms={:?}. Delivery needs a fresh waiting/idle harness report, or (Claude/Codex) an observed empty input prompt while the agent is not working. Retry this same message once the recipient is at rest; otherwise leave it queued.",
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
    authorize_delivery(&records, &message, args.workspace.is_some())?;
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
            )
            .map(Submission::status)
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
