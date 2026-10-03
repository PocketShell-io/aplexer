use super::*;

fn pane_target<'a>(
    records: &'a [SessionRecord],
    workspace: &Path,
    envelope: &MessageEnvelope,
) -> Result<&'a SessionRecord> {
    check_body_size(&envelope.body)?;
    let Recipient::Tag {
        tag, session_id, ..
    } = &envelope.to
    else {
        bail!("pane delivery requires a single tag target");
    };
    let cross_workspace = envelope
        .from
        .workspace
        .as_ref()
        .is_some_and(|source| source != workspace);
    let record = select_pane_record(records, workspace, tag, *session_id, cross_workspace)?;
    if !record.worker_alive() {
        bail!("session {tag:?} is not running; pane delivery requires a live target");
    }
    Ok(record)
}

fn select_pane_record<'a>(
    records: &'a [SessionRecord],
    workspace: &Path,
    tag: &str,
    session_id: Option<Uuid>,
    cross_workspace: bool,
) -> Result<&'a SessionRecord> {
    let record = if cross_workspace {
        let id = session_id
            .ok_or_else(|| anyhow!("cross-workspace pane delivery requires a target session id"))?;
        records
            .iter()
            .find(|record| record.id == id && record.workspace == workspace)
    } else {
        session_by_tag(records, workspace, tag)
    };
    record.ok_or_else(|| anyhow!("no matching target session for {tag:?} in this workspace"))
}

pub(crate) fn deliver_pane(
    records: &[SessionRecord],
    workspace: &Path,
    envelope: &MessageEnvelope,
    raw: bool,
    no_enter: bool,
) -> Result<()> {
    let record = pane_target(records, workspace, envelope)?;
    let tag = &record.tag;
    let pre_state = record.reported_state.as_deref().unwrap_or("idle");
    let input = pane_input_bytes(envelope, raw, no_enter);
    if no_enter {
        return rpc_send(record, &input)
            .with_context(|| format!("inject into session {tag:?}'s PTY"));
    }
    let kind = if raw {
        SubmissionKind::Raw
    } else {
        SubmissionKind::FramedMessage
    };
    rpc_send_submitted(record, &input, kind)
        .with_context(|| format!("submit message in session {tag:?}'s PTY"))?;
    verify_pane_delivery(record, envelope, raw, pre_state)
}

pub(crate) fn verify_pane_delivery(
    record: &SessionRecord,
    envelope: &MessageEnvelope,
    raw: bool,
    pre_state: &str,
) -> Result<()> {
    let needle = if raw {
        envelope
            .body
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim())
            .unwrap_or("")
    } else {
        &envelope.id.to_string()
    };

    let timeout_ms = std::env::var("APLEXER_PANE_VERIFY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1000);
    let timeout = Duration::from_millis(timeout_ms);
    let poll_interval = Duration::from_millis(25);
    let start = Instant::now();

    while start.elapsed() < timeout {
        // 1. Check plain screen capture for echo of needle
        if let Ok(screen_bytes) = rpc_capture_screen(record, true) {
            let screen_text = String::from_utf8_lossy(&screen_bytes);
            if !needle.is_empty() && screen_text.contains(needle) {
                return Ok(());
            }
        }

        // 2. Check recent PTY capture for echo of needle
        if let Ok(pty_bytes) = rpc_capture(record, Some(4096)) {
            let pty_text = String::from_utf8_lossy(&pty_bytes);
            if !needle.is_empty() && pty_text.contains(needle) {
                return Ok(());
            }
        }

        // 3. Check live status for correlated transition from idle/waiting to working
        if pre_state != "working" {
            if let Ok(raw_status) = rpc_simple(record, Operation::Status, None) {
                if let Ok(live) = serde_json::from_value::<SessionRecord>(raw_status) {
                    let now = now_ms();
                    let (state, _) = session_ui_state(&live, now);
                    if matches!(state, "running" | "working")
                        || live.reported_state.as_deref() == Some("working")
                    {
                        return Ok(());
                    }
                }
            }
        }

        thread::sleep(poll_interval);
    }

    bail!(
        "pane submission not confirmed: message id or text was not observed in target session screen/PTY and session did not transition to working within {:?}",
        timeout
    )
}

/// The bytes `--pane` delivery injects: the message, framed with its sender
/// unless `raw`, and -- by default, the tmuxctl behavior -- a trailing
/// return, so a message typed into an agent's prompt actually submits
/// instead of sitting there unconfirmed. `--no-enter` drops the return for
/// the rare target that should compose rather than submit.
pub(crate) fn pane_input_bytes(envelope: &MessageEnvelope, raw: bool, no_enter: bool) -> Vec<u8> {
    let mut out = if raw {
        envelope.body.as_bytes().to_vec()
    } else {
        let sender = envelope.from.tag.as_deref().unwrap_or("external");
        let source = envelope
            .from
            .workspace
            .as_deref()
            .unwrap_or(&envelope.workspace);
        let session = envelope
            .from
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "external".to_string());
        format!(
            "[aplexer message id={} from={sender} session={session} workspace={}; reply with: aplexer message reply {} '<text>'] {}",
            envelope.id,
            source.display(),
            envelope.id,
            envelope.body
        )
        .into_bytes()
    };
    if !no_enter {
        out.push(b'\r');
    }
    out
}

pub(crate) fn parse_data_arg(raw: Option<&str>) -> Result<Option<Value>> {
    raw.map(|s| serde_json::from_str::<Value>(s).context("--data must be valid JSON"))
        .transpose()
}

/// Persist before writing to the PTY: a pane frame's reply id always names
/// real mail. If injection fails after persistence, the recipient still has
/// an unread inbox message and the caller gets that explicit outcome.
pub(crate) fn finish_send(
    mp: &MessagePaths,
    records: &[SessionRecord],
    workspace: &Path,
    mut envelope: MessageEnvelope,
    pane: &PaneDeliveryArgs,
) -> Result<MessageEnvelope> {
    if pane.pane && !matches!(envelope.to, Recipient::Tag { .. }) {
        bail!("--pane requires a single --to TAG target: no pane broadcast");
    }
    envelope = aplexer::messaging::write_message_in_idempotent(mp, &envelope)?;
    if pane.pane {
        finish_pane(mp, records, workspace, &mut envelope, pane)?;
    }
    let _ = maybe_gc_in(mp, workspace, records);
    Ok(envelope)
}

fn finish_pane(
    mp: &MessagePaths,
    records: &[SessionRecord],
    workspace: &Path,
    envelope: &mut MessageEnvelope,
    pane: &PaneDeliveryArgs,
) -> Result<()> {
    let outcome = submit_message_in(
        mp,
        workspace,
        envelope.id,
        |current| pane_target(records, workspace, current).map(|_| ()),
        |current| deliver_pane(records, workspace, current, pane.raw, pane.no_enter),
    )?;
    match outcome.status {
        SubmissionStatus::Submitted | SubmissionStatus::AlreadySubmitted => {
            acknowledge_initial_pane(mp, envelope);
            Ok(())
        }
        SubmissionStatus::RecipientAcked => Ok(()),
        _ => report_pane_failure(&outcome, pane.or_inbox),
    }
}

fn acknowledge_initial_pane(mp: &MessagePaths, envelope: &mut MessageEnvelope) {
    envelope.delivery = Delivery::Pane;
    if let Recipient::Tag {
        session_id: Some(id),
        ..
    } = envelope.to
    {
        let _ = ack_messages_in(mp, id, &[envelope.id]);
    }
}

fn report_pane_failure(outcome: &SubmissionOutcome, or_inbox: bool) -> Result<()> {
    let detail = outcome
        .detail
        .as_deref()
        .unwrap_or("previous attempt may have written input");
    let message = format!(
        "pane input failed for message {}: durable inbox copy recorded (delivery=inbox); inspect that id before retrying ({detail})",
        outcome.id,
    );
    if or_inbox {
        eprintln!(
            "a: pane input failed for message {}; durable inbox copy remains ({detail})",
            outcome.id
        );
        return Ok(());
    }
    bail!("{message}")
}
