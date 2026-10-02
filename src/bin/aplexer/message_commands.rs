use super::*;

/// Messages addressed to `consumer` that it has not acknowledged, in id
/// (= time) order.
fn unread_messages(
    mp: &MessagePaths,
    workspace: &Path,
    consumer: &SessionIdentity,
) -> Result<Vec<MessageEnvelope>> {
    let cursor = read_cursor_in(mp, consumer.id)?;
    Ok(list_messages_in(mp, workspace)?
        .into_iter()
        .filter(|m| consumer.receives(m) && !cursor.is_acked(m.id))
        .collect())
}

pub(crate) fn print_message_line(m: &MessageEnvelope) {
    let sender = m.from.tag.clone().unwrap_or_else(|| {
        if m.from.external {
            "external".into()
        } else {
            "?".into()
        }
    });
    let to_desc = match &m.to {
        Recipient::Tag { tag, .. } => format!("to:{tag}"),
        Recipient::Broadcast { .. } => "to:*".to_string(),
        Recipient::Engine { engine } => format!("to:engine:{engine}"),
    };
    let delivery = match m.delivery {
        Delivery::Inbox => "",
        Delivery::Pane => " [pane]",
    };
    let first_line = m.body.lines().next().unwrap_or("");
    println!(
        "{}  [{}] {sender} -> {to_desc}{delivery}  {first_line}",
        m.id, m.kind
    );
}

pub(crate) fn print_message_details(m: &MessageEnvelope) {
    println!("id: {}", m.id);
    println!("workspace: {}", m.workspace.display());
    println!("created_at: {}", m.created_at);
    let sender = m.from.tag.clone().unwrap_or_else(|| {
        if m.from.external {
            "(external)".into()
        } else {
            "(unknown)".into()
        }
    });
    println!(
        "from: {sender}{}",
        m.from
            .engine
            .as_deref()
            .map(|e| format!(" [{e}]"))
            .unwrap_or_default()
    );
    match &m.to {
        Recipient::Tag { tag, .. } => println!("to: {tag}"),
        Recipient::Broadcast { .. } => println!("to: * (broadcast)"),
        Recipient::Engine { engine } => println!("to: engine:{engine}"),
    }
    println!("kind: {}", m.kind);
    if let Some(r) = m.reply_to {
        println!("reply_to: {r}");
    }
    println!(
        "delivery: {}",
        match m.delivery {
            Delivery::Inbox => "inbox",
            Delivery::Pane => "pane",
        }
    );
    println!("---");
    println!("{}", m.body);
    if let Some(d) = &m.data {
        println!("---");
        println!("data: {d}");
    }
}

pub(crate) fn cmd_message(paths: &Paths, args: MessageArgs, json_output: bool) -> Result<()> {
    match args.command {
        MessageCommand::Send(a) => cmd_message_send(paths, a, json_output),
        MessageCommand::Reply(a) => cmd_message_reply(paths, a, json_output),
        MessageCommand::Deliver(a) => cmd_message_deliver(paths, a, json_output),
        MessageCommand::Inbox(a) => cmd_message_inbox(paths, a, json_output),
        MessageCommand::Wait(a) => cmd_message_wait(paths, a, json_output),
        MessageCommand::Log(a) => cmd_message_log(paths, a, json_output),
        MessageCommand::Show(a) => cmd_message_show(paths, a, json_output),
        MessageCommand::Ack(a) => cmd_message_ack(paths, a, json_output),
        MessageCommand::Gc(a) => cmd_message_gc(paths, a, json_output),
        MessageCommand::HookNotice(a) => {
            cmd_message_hook_notice(paths, a);
            Ok(())
        }
    }
}

fn cmd_message_wait(paths: &Paths, args: MessageWaitArgs, json_output: bool) -> Result<()> {
    let (workspace, consumer) = wait_consumer(paths)?;
    let messages = wait_messages(
        paths,
        &workspace,
        &consumer,
        std::time::Duration::from_secs(args.timeout),
    )?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no unread messages");
    } else {
        for message in &messages {
            print_message_line(message);
        }
    }
    Ok(())
}

fn wait_consumer(paths: &Paths) -> Result<(PathBuf, SessionIdentity)> {
    let id = discover_session_id()
        .ok_or_else(|| anyhow!("message wait requires an aplexer session identity"))?;
    let records = list_records(paths)?;
    let record = records
        .iter()
        .find(|record| record.id == id)
        .ok_or_else(|| anyhow!("message wait requires an existing session record"))?;
    let consumer = SessionIdentity {
        id: record.id,
        workspace: Some(record.workspace.clone()),
        tag: Some(record.tag.clone()),
        engine: Some(record.engine.clone()),
        profile: record.profile.clone(),
    };
    Ok((record.workspace.clone(), consumer))
}

fn cmd_message_hook_notice(paths: &Paths, args: MessageHookNoticeArgs) {
    let engine = match args.engine {
        NoticeEngine::Claude => "claude",
        NoticeEngine::Codex => "codex",
    };
    if let Ok(Some(output)) = hook_notice(paths, engine, io::stdin().lock()) {
        println!("{output}");
    }
}

pub(crate) fn cmd_message_inbox(
    paths: &Paths,
    args: MessageInboxArgs,
    json_output: bool,
) -> Result<()> {
    let _ = args.new; // `--new` is accepted for CLI-surface compatibility; unread is already the default (design doc section 7).
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let consumer = SessionIdentity::required(resolve_identity(
        &records,
        &workspace,
        args.from.as_deref(),
    )?)?;
    let messages = if args.from.is_none() && records.iter().any(|r| r.id == consumer.id) {
        aplexer::coordination::unread_messages(paths, consumer.id)?
    } else {
        let mp = ensure_workspace(paths, &workspace)?;
        let _ = maybe_gc_in(&mp, &workspace, &records);
        unread_messages(&mp, &workspace, &consumer)?
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no unread messages");
    } else {
        for m in &messages {
            print_message_line(m);
        }
    }
    Ok(())
}

pub(crate) fn cmd_message_log(
    paths: &Paths,
    args: MessageLogArgs,
    json_output: bool,
) -> Result<()> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let mp = ensure_workspace(paths, &workspace)?;
    let _ = maybe_gc_in(&mp, &workspace, &list_records(paths)?);
    let messages = list_messages_in(&mp, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no messages");
    } else {
        for m in &messages {
            print_message_line(m);
        }
    }
    Ok(())
}

pub(crate) fn cmd_message_show(
    paths: &Paths,
    args: MessageShowArgs,
    json_output: bool,
) -> Result<()> {
    let workspace = resolve_message_workspace(None)?;
    let message = read_consumer_message(paths, &workspace, args.message_id)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&message)?);
    } else {
        print_message_details(&message);
    }
    Ok(())
}

pub(crate) fn cmd_message_ack(
    paths: &Paths,
    args: MessageAckArgs,
    json_output: bool,
) -> Result<()> {
    if args.all && !args.message_ids.is_empty() {
        bail!("cannot combine --all with explicit message ids");
    }
    if !args.all && args.message_ids.is_empty() {
        bail!("specify at least one message id, or --all");
    }
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let consumer = SessionIdentity::required(resolve_identity(
        &records,
        &workspace,
        args.from.as_deref(),
    )?)?;
    if args.from.is_none() && records.iter().any(|r| r.id == consumer.id) {
        return ack_participating_mailboxes(paths, &consumer, args, json_output);
    }
    let mp = ensure_workspace(paths, &workspace)?;
    let ids: Vec<Uuid> = if args.all {
        unread_messages(&mp, &workspace, &consumer)?
            .into_iter()
            .map(|m| m.id)
            .collect()
    } else {
        args.message_ids
    };
    let acked = ack_messages_in(&mp, consumer.id, &ids)?;
    let unknown: Vec<Uuid> = ids
        .iter()
        .filter(|id| !acked.contains(id))
        .copied()
        .collect();
    if json_output {
        println!("{}", json!({"acked": acked, "unknown": unknown}));
    } else {
        println!("acked {} message(s)", acked.len());
        if !unknown.is_empty() {
            eprintln!(
                "a: {} id(s) not in this mailbox (pruned, or never here): {}",
                unknown.len(),
                unknown
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

/// Follow the stable consumer UUID across declared and retained mailboxes.
/// The current workspace log remains inspectable for backwards compatibility.
pub(crate) fn read_consumer_message(
    paths: &Paths,
    workspace: &Path,
    message_id: Uuid,
) -> Result<MessageEnvelope> {
    if let Ok(message) = read_message(paths, workspace, message_id) {
        return Ok(message);
    }
    if let Some(id) = discover_session_id() {
        for mailbox in aplexer::coordination::mailbox_workspaces(paths, id)? {
            if mailbox == workspace {
                continue;
            }
            if let Ok(message) = read_message(paths, &mailbox, message_id) {
                return Ok(message);
            }
        }
    }
    bail!("no such message {message_id}")
}

fn ack_participating_mailboxes(
    paths: &Paths,
    consumer: &SessionIdentity,
    args: MessageAckArgs,
    json_output: bool,
) -> Result<()> {
    let messages = aplexer::coordination::unread_messages(paths, consumer.id)?;
    let ids: Vec<Uuid> = if args.all {
        messages.iter().map(|m| m.id).collect()
    } else {
        args.message_ids
    };
    let mut acked = Vec::new();
    for workspace in aplexer::coordination::mailbox_workspaces(paths, consumer.id)? {
        let mp = ensure_workspace(paths, &workspace)?;
        // Only acknowledge IDs actually addressed to this consumer, rather than
        // adding another workspace's UUIDs to an unrelated cursor.
        let local: Vec<Uuid> = list_messages_in(&mp, &workspace)?
            .iter()
            .filter(|m| ids.contains(&m.id) && consumer.receives(m))
            .map(|m| m.id)
            .collect();
        acked.extend(ack_messages_in(&mp, consumer.id, &local)?);
    }
    acked.sort();
    acked.dedup();
    let unknown: Vec<_> = ids
        .iter()
        .filter(|id| !acked.contains(id))
        .copied()
        .collect();
    if json_output {
        println!("{}", json!({"acked": acked, "unknown": unknown}));
    } else {
        println!("acked {} message(s)", acked.len());
        if !unknown.is_empty() {
            eprintln!(
                "a: {} id(s) not addressed to this consumer in its mailboxes",
                unknown.len()
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_message_gc(paths: &Paths, args: MessageGcArgs, json_output: bool) -> Result<()> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let report = gc_workspace(paths, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "removed {} message(s), {} remaining",
            report.removed, report.remaining
        );
    }
    Ok(())
}
