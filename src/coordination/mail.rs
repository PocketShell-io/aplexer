//! Which mailboxes a session reads, and what in them is unread.
//!
//! Subscription is "current plus declared locations": the session's own
//! workspace and every workspace it holds a declaration in get the full
//! inbox filter (broadcasts, engine matches, tag matches). Beyond that,
//! existing mailboxes are scanned through their reverse metadata for
//! messages *explicitly addressed by session UUID* — even already
//! acknowledged history, so `message show`/`reply` keep working after a
//! session moves or its declaration is released. A retained mailbox that is
//! not subscribed contributes only those explicit-UUID messages: never
//! unrelated broadcasts and never old tag matches, which may belong to a
//! later holder of the tag.
//!
//! The whole answer is computed in one pass (`inbox_snapshot`): the
//! awareness hook runs it on every tool call, so each mailbox's messages
//! are parsed exactly once there — the same listing decides whether the
//! mailbox attaches at all and which of its messages are unread — instead
//! of once per question.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::state;
use crate::messaging::{
    addressed_to, compact_cursor, cursor_lock_path, list_messages_in, mailbox_lock_path,
    maybe_gc_all, message_paths, read_cursor_file, retained_message_ids, reverse_metadata,
    MessageEnvelope, Recipient,
};
use crate::{canonical_workspace, read_session_record, FileLock, Paths};

/// Upper bound on mailboxes examined per call, so a state directory with an
/// unusual number of workspaces cannot turn an inbox check into an
/// unbounded scan. Directories are visited in sorted (stable) order.
const MAX_MAILBOX_SCAN: usize = 512;

/// The mailboxes attached to a session and its unread envelopes.
pub struct InboxSnapshot {
    pub mailboxes: Vec<PathBuf>,
    pub unread: Vec<MessageEnvelope>,
    /// False when a mailbox or cursor lock was too busy to take this pass,
    /// so `unread` may undercount. Callers that derive durable state from
    /// the answer (the notice claim) must treat an incomplete snapshot as
    /// "unknown", not as authoritative emptiness.
    pub complete: bool,
}

/// What mailboxes `id` reads and what in them is unread.
///
/// One filesystem pass per call: each candidate mailbox's messages are
/// listed (and parsed) exactly once, under a nonblocking mailbox lock, and
/// that single listing decides both whether the mailbox attaches at all
/// (subscription, or an explicit UUID address somewhere in it) and which
/// of its messages are unread. Contention never fails the snapshot: a busy
/// mailbox or cursor is skipped for this pass rather than waited on or
/// answered with an error — message files are only ever replaced
/// atomically, so the next call sees a consistent picture — because the
/// hottest caller is a hook running inside ordinary agent tool turns. The
/// [`InboxSnapshot::complete`] flag reports whether any skip happened, so
/// callers that persist conclusions from the answer can decline instead.
pub fn inbox_snapshot(paths: &Paths, id: Uuid) -> Result<InboxSnapshot> {
    // Opportunistic global maintenance. Errors are swallowed: pruning must
    // never fail a read.
    let _ = maybe_gc_all(paths);

    let record = read_session_record(paths, id).ok();
    let tag = record.as_ref().map(|r| r.tag.as_str()).unwrap_or("");
    let engine = record.as_ref().map(|r| r.engine.as_str()).unwrap_or("");

    let mut subscribed: Vec<PathBuf> = Vec::new();
    if let Some(record) = &record {
        if let Ok(workspace) = canonical_workspace(&record.workspace) {
            subscribed.push(workspace);
        }
    }
    for claim in state::load_tolerant(paths, id).unwrap_or_default() {
        if !subscribed.contains(&claim.workspace) {
            subscribed.push(claim.workspace);
        }
    }

    let messages_root = paths.state_root.join("messages");
    let mut directories: Vec<PathBuf> = match fs::read_dir(&messages_root) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect(),
        // No mailboxes exist yet: nothing to discover, and creating the
        // messages root is `ensure_workspace`'s job, not a reader's.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", messages_root.display()))
        }
    };
    directories.sort();
    directories.truncate(MAX_MAILBOX_SCAN);

    let mut snapshot = InboxSnapshot {
        mailboxes: subscribed.clone(),
        unread: Vec::new(),
        complete: true,
    };
    for workspace in &subscribed {
        process_mailbox(&mut snapshot, paths, id, tag, engine, workspace, true)?;
    }
    for directory in directories {
        let Some(workspace) = reverse_metadata(&directory) else {
            continue;
        };
        // A subscription workspace's discovered directory is already done:
        // the subscription phase read it (its own duplicate-free list means
        // only discovery needs the duplicate check).
        if snapshot.mailboxes.contains(&workspace) {
            continue;
        }
        process_mailbox(&mut snapshot, paths, id, tag, engine, &workspace, false)?;
    }
    snapshot.unread.sort_by_key(|envelope| envelope.id);
    Ok(snapshot)
}

/// Folds one candidate mailbox into `snapshot`. Subscription mailboxes are
/// processed first (their directories may not exist yet), discovered
/// mailboxes after. A mailbox only attaches when this session has business
/// in it: a subscription, or a message explicitly addressed to it.
fn process_mailbox(
    snapshot: &mut InboxSnapshot,
    paths: &Paths,
    id: Uuid,
    tag: &str,
    engine: &str,
    workspace: &Path,
    is_subscribed: bool,
) -> Result<()> {
    let mp = message_paths(paths, workspace);
    if !mp.msgs_dir.is_dir() {
        return Ok(());
    }
    // Mailbox then cursor, matching the write path's lock order; both
    // nonblocking, and message files are only ever replaced atomically, so
    // the listing under these locks is a consistent snapshot.
    let Ok(_mailbox) = FileLock::exclusive(&mailbox_lock_path(&mp), true) else {
        snapshot.complete = false;
        return Ok(());
    };
    let messages = list_messages_in(&mp, workspace)?;
    if !is_subscribed && !messages.iter().any(|m| explicitly_addressed(m, id)) {
        return Ok(());
    }
    snapshot.mailboxes.push(workspace.to_path_buf());
    let Ok(_cursor) = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, id), true) else {
        snapshot.complete = false;
        return Ok(());
    };
    let mut cursor = read_cursor_file(&mp.cursors_dir.join(format!("{id}.json")))?;
    compact_cursor(&mut cursor, &retained_message_ids(&mp.msgs_dir)?);
    for envelope in messages {
        let mine = if is_subscribed {
            addressed_to(&envelope, id, tag, engine)
        } else {
            explicitly_addressed(&envelope, id)
        };
        if mine && !cursor.is_acked(envelope.id) {
            snapshot.unread.push(envelope);
        }
    }
    Ok(())
}

/// The workspaces whose mailboxes this session reads: subscription
/// mailboxes first, discovered mailboxes after. Canonical paths throughout,
/// so alias spellings collapse before mailbox keys are derived.
pub fn mailbox_workspaces(paths: &Paths, id: Uuid) -> Result<Vec<PathBuf>> {
    Ok(inbox_snapshot(paths, id)?.mailboxes)
}

/// Messages for `id` that are still unread: subscription mailboxes use the
/// ordinary addressed-to filter, retained-only mailboxes require an explicit
/// UUID address, and both respect the session's cursor. Envelopes are
/// returned whole for the CLI to render; `context` projects them down to
/// workspace + id references on its own.
pub fn unread_messages(paths: &Paths, id: Uuid) -> Result<Vec<MessageEnvelope>> {
    Ok(inbox_snapshot(paths, id)?.unread)
}

/// The one addressing shape that means "this exact session, wherever it
/// lives": a tag recipient with the session id resolved at send time.
/// Broadcasts and engine matches are audience-wide; tag-only recipients may
/// name a tag this session merely holds now.
pub(crate) fn explicitly_addressed(envelope: &MessageEnvelope, id: Uuid) -> bool {
    matches!(&envelope.to, Recipient::Tag { session_id: Some(sid), .. } if *sid == id)
}
