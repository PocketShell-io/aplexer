use super::*;

pub(crate) fn deliver_pane(
    records: &[SessionRecord],
    workspace: &Path,
    envelope: &MessageEnvelope,
    raw: bool,
    no_enter: bool,
) -> Result<()> {
    if envelope.body.len() > MAX_BODY_BYTES {
        bail!("message body exceeds the {MAX_BODY_BYTES}-byte cap");
    }
    let Recipient::Tag { tag, session_id } = &envelope.to else {
        bail!("pane delivery requires a single tag target");
    };
    let cross_workspace = envelope
        .from
        .workspace
        .as_ref()
        .is_some_and(|source| source != workspace);
    let record = if cross_workspace {
        let target_id = session_id
            .ok_or_else(|| anyhow!("cross-workspace pane delivery requires a target session id"))?;
        records
            .iter()
            .find(|record| record.id == target_id && record.workspace == workspace)
    } else {
        session_by_tag(records, workspace, tag)
    }
    .ok_or_else(|| anyhow!("no matching target session for {tag:?} in this workspace"))?;
    if !record.worker_alive() {
        bail!("session {tag:?} is not running; pane delivery requires a live target");
    }
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
        .with_context(|| format!("submit message in session {tag:?}'s PTY"))
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
    if pane.pane {
        let Recipient::Tag { .. } = &envelope.to else {
            bail!("--pane requires a single --to TAG target: no pane broadcast");
        };
    }
    write_message_in(mp, &envelope)?;
    if pane.pane {
        match deliver_pane(records, workspace, &envelope, pane.raw, pane.no_enter) {
            Ok(()) => match mark_pane_delivered_in(mp, &envelope) {
                Ok(()) => {
                    envelope.delivery = Delivery::Pane;
                    if let Recipient::Tag {
                        session_id: Some(sid),
                        ..
                    } = &envelope.to
                    {
                        let _ = ack_messages_in(mp, *sid, &[envelope.id]);
                    }
                }
                Err(error) => eprintln!(
                    "a: pane input was written for message {}, but its durable copy remains inbox; recipient may read it again ({error:#})",
                    envelope.id
                ),
            },
            Err(error) if pane.or_inbox => eprintln!(
                "a: pane input failed for message {}; durable inbox copy remains ({error:#})",
                envelope.id
            ),
            Err(error) => bail!(
                "pane input failed for message {}: durable inbox copy recorded (delivery=inbox); inspect that id before retrying ({error:#})",
                envelope.id
            ),
        }
    }
    let _ = maybe_gc_in(mp, workspace, records);
    Ok(envelope)
}
