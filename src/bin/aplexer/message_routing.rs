use super::*;

// -- Inter-agent messaging (docs/inter-agent-messaging-design.md) --

/// Workspace for local message commands: the live session record follows `cd`,
/// then `$APLEXER_WORKSPACE` supports older sessions, then cwd.
pub(crate) fn resolve_message_workspace(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return canonical_workspace(p);
    }
    if let Some(id) = discover_session_id() {
        if let Ok(paths) = Paths::discover() {
            if let Ok(record) = read_record(&paths.record(id)) {
                return Ok(record.workspace);
            }
        }
    }
    if let Ok(v) = env::var("APLEXER_WORKSPACE") {
        if !v.is_empty() {
            return canonical_workspace(Path::new(&v));
        }
    }
    canonical_workspace(Path::new("."))
}

/// Resolves the `--to`/`--all`/`--to-engine` triple into a `Recipient`,
/// applying the typo guard of design doc section 2.3: a tag that has never
/// existed in this workspace is rejected with the list of known tags unless
/// `--queue` is passed. Broadcast/engine forms always succeed.
pub(crate) fn build_recipient(
    records: &[SessionRecord],
    workspace: &Path,
    to: Option<&str>,
    all: bool,
    to_engine: Option<&str>,
    queue: bool,
) -> Result<Recipient> {
    let chosen = [to.is_some(), all, to_engine.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if chosen == 0 {
        bail!("specify exactly one of --to TAG, --all, or --to-engine ENGINE");
    }
    if chosen > 1 {
        bail!("--to, --all, and --to-engine are mutually exclusive");
    }
    if let Some(tag) = to {
        let existing = session_by_tag(records, workspace, tag);
        if existing.is_none() && !queue {
            let known = known_tags(records, workspace);
            let hint = if known.is_empty() {
                "no session has ever run in this workspace".to_string()
            } else {
                format!("known tags: {}", known.join(", "))
            };
            bail!(
                "no session tagged {tag:?} has ever existed in this workspace ({hint}); pass \
                 --queue to park a message for a session that will be created later"
            );
        }
        return Ok(Recipient::Tag {
            tag: tag.to_string(),
            session_id: existing.map(|r| r.id),
        });
    }
    if all {
        return Ok(Recipient::Broadcast { broadcast: true });
    }
    Ok(Recipient::Engine {
        engine: to_engine.unwrap().to_string(),
    })
}

/// Use the calling session record for cross-workspace traffic. The CLI
/// deliberately does not accept a `--from` override on this route.
fn current_session_sender(
    records: &[SessionRecord],
    workspace: Option<&Path>,
) -> Result<MessageFrom> {
    let id = discover_session_id()
        .ok_or_else(|| anyhow!("cross-workspace messaging requires an aplexer session identity"))?;
    let record = records
        .iter()
        .find(|record| record.id == id && workspace.is_none_or(|ws| record.workspace == ws))
        .ok_or_else(|| anyhow!("calling aplexer session {id} has no matching session record"))?;
    Ok(MessageFrom {
        session_id: Some(record.id),
        workspace: Some(record.workspace.clone()),
        tag: Some(record.tag.clone()),
        engine: Some(record.engine.clone()),
        profile: record.profile.clone(),
        external: false,
    })
}

fn send_source(
    args: &MessageSendArgs,
    records: &[SessionRecord],
    workspace: &Path,
) -> Result<MessageFrom> {
    if args.workspace.is_some() {
        if args.from.is_some() {
            bail!("--from cannot be used with cross-workspace --workspace delivery");
        }
        current_session_sender(records, None)
    } else {
        Ok(MessageFrom::from_identity(resolve_identity(
            records,
            workspace,
            args.from.as_deref(),
        )?))
    }
}

fn send_target(
    args: &MessageSendArgs,
    records: &[SessionRecord],
    workspace: &Path,
) -> Result<Recipient> {
    let to = build_recipient(
        records,
        workspace,
        args.to.as_deref(),
        args.all,
        args.to_engine.as_deref(),
        args.queue,
    )?;
    if args.workspace.is_some()
        && matches!(
            to,
            Recipient::Tag {
                session_id: None,
                ..
            }
        )
    {
        bail!("cross-workspace sends require an existing target session; --queue cannot address an unknown session id");
    }
    Ok(to)
}

fn finish_and_print(
    paths: &Paths,
    records: &[SessionRecord],
    envelope: MessageEnvelope,
    pane: &PaneDeliveryArgs,
    json_output: bool,
) -> Result<()> {
    let workspace = envelope.workspace.clone();
    let mp = ensure_workspace(paths, &workspace)?;
    let sent = finish_send(&mp, records, &workspace, envelope, pane)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&sent)?);
    } else {
        println!("{}", sent.id);
    }
    Ok(())
}

/// Send to a sibling or a named destination workspace.
pub(crate) fn cmd_message_send(
    paths: &Paths,
    args: MessageSendArgs,
    json_output: bool,
) -> Result<()> {
    if args.workspace.is_some() && (args.all || args.to_engine.is_some() || args.to.is_none()) {
        bail!("--workspace requires a single --to TAG target");
    }
    if args.pane_delivery.pane && (args.all || args.to_engine.is_some()) {
        bail!("--pane cannot be combined with --all or --to-engine: no pane broadcast");
    }
    if args.pane_delivery.pane && args.to.is_none() {
        bail!("--pane requires --to TAG");
    }
    check_body_size(&args.text)?;
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let records = list_records(paths)?;
    let data = parse_data_arg(args.data.as_deref())?;
    let from = send_source(&args, &records, &workspace)?;
    let to = send_target(&args, &records, &workspace)?;
    let envelope = MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: workspace.clone(),
        created_at: now_secs(),
        from,
        to,
        kind: args.kind,
        reply_to: None,
        body: args.text,
        data,
        delivery: Delivery::Inbox,
    };
    finish_and_print(paths, &records, envelope, &args.pane_delivery, json_output)
}

fn reply_source(
    args: &MessageReplyArgs,
    records: &[SessionRecord],
    workspace: &Path,
    destination: &Path,
) -> Result<MessageFrom> {
    if destination != workspace {
        if args.from.is_some() {
            bail!("--from cannot be used with cross-workspace replies");
        }
        current_session_sender(records, Some(workspace))
    } else {
        Ok(MessageFrom::from_identity(resolve_identity(
            records,
            workspace,
            args.from.as_deref(),
        )?))
    }
}

fn reply_target(
    original: &MessageEnvelope,
    records: &[SessionRecord],
    destination: &Path,
) -> Result<Recipient> {
    let original_tag = original.from.tag.as_ref().ok_or_else(|| {
        anyhow!(
            "original message {} was sent anonymously (no sender tag)",
            original.id
        )
    })?;
    if destination != original.workspace {
        let sender_id = original.from.session_id.ok_or_else(|| {
            anyhow!("cross-workspace reply requires the original sender's session id")
        })?;
        let target = records
            .iter()
            .find(|record| record.id == sender_id && record.workspace == destination)
            .ok_or_else(|| anyhow!("original sender session is no longer recorded"))?;
        return Ok(Recipient::Tag {
            tag: target.tag.clone(),
            session_id: Some(target.id),
        });
    }
    let target = session_by_tag(records, destination, original_tag);
    Ok(Recipient::Tag {
        tag: original_tag.clone(),
        session_id: original.from.session_id.or(target.map(|r| r.id)),
    })
}

pub(crate) fn cmd_message_reply(
    paths: &Paths,
    args: MessageReplyArgs,
    json_output: bool,
) -> Result<()> {
    check_body_size(&args.text)?;
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let mp = ensure_workspace(paths, &workspace)?;
    let original = read_message_in(&mp, &workspace, args.message_id)
        .with_context(|| format!("no such message {}", args.message_id))?;
    let data = parse_data_arg(args.data.as_deref())?;
    let destination = original
        .from
        .workspace
        .clone()
        .unwrap_or_else(|| workspace.clone());
    let from = reply_source(&args, &records, &workspace, &destination)?;
    let to = reply_target(&original, &records, &destination)?;
    let envelope = MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: destination.clone(),
        created_at: now_secs(),
        from,
        to,
        kind: args.kind.unwrap_or_else(|| "reply".to_string()),
        reply_to: Some(original.id),
        body: args.text,
        data,
        delivery: Delivery::Inbox,
    };
    finish_and_print(paths, &records, envelope, &args.pane_delivery, json_output)
}
