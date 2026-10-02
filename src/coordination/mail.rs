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

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::state;
use crate::messaging::{
    addressed_to, compact_cursor, cursor_lock_path, list_messages_in, mailbox_lock_path,
    message_paths, read_cursor_file, retained_message_ids, workspace_key, MessageEnvelope,
    Recipient,
};
use crate::persist::read_bounded_regular_file;
use crate::{canonical_workspace, read_session_record, FileLock, Paths};

/// Upper bound on mailboxes examined per call, so a state directory with an
/// unusual number of workspaces cannot turn an inbox check into an
/// unbounded scan. Directories are visited in sorted (stable) order.
const MAX_MAILBOX_SCAN: usize = 512;

pub(crate) struct MailboxRef {
    pub workspace: PathBuf,
    pub subscribed: bool,
}

/// The workspaces whose mailboxes this session reads: subscription mailboxes
/// first, discovered mailboxes after. Canonical paths throughout, so alias
/// spellings collapse before mailbox keys are derived.
pub fn mailbox_workspaces(paths: &Paths, id: Uuid) -> Result<Vec<PathBuf>> {
    let mailboxes = mailboxes(paths, id)?;
    Ok(mailboxes
        .into_iter()
        .map(|mailbox| mailbox.workspace)
        .collect())
}

pub(crate) fn mailboxes(paths: &Paths, id: Uuid) -> Result<Vec<MailboxRef>> {
    let mut mailboxes: Vec<MailboxRef> = Vec::new();

    if let Ok(record) = read_session_record(paths, id) {
        if let Ok(workspace) = canonical_workspace(&record.workspace) {
            subscribe(&mut mailboxes, workspace, true);
        }
    }
    for claim in state::load_tolerant(paths, id).unwrap_or_default() {
        subscribe(&mut mailboxes, claim.workspace, true);
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
    for directory in directories {
        let Some(workspace) = reverse_metadata(&directory) else {
            continue;
        };
        if mailboxes.iter().any(|m| m.workspace == workspace) {
            continue;
        }
        if explicitly_addressed_in(paths, &workspace, id) {
            subscribe(&mut mailboxes, workspace, false);
        }
    }
    Ok(mailboxes)
}

fn subscribe(mailboxes: &mut Vec<MailboxRef>, workspace: PathBuf, subscribed: bool) {
    if !mailboxes.iter().any(|m| m.workspace == workspace) {
        mailboxes.push(MailboxRef {
            workspace,
            subscribed,
        });
    }
}

/// Messages for `id` that are still unread: subscription mailboxes use the
/// ordinary addressed-to filter, retained-only mailboxes require an explicit
/// UUID address, and both respect the session's cursor. Envelopes are
/// returned whole for the CLI to render; `context` projects them down to
/// workspace + id references on its own.
///
/// This is a side-effect-free, nonblocking snapshot: both locks are taken
/// with try-lock semantics and contention propagates as `Err` (never a
/// silent empty result — callers treat an empty snapshot as authoritative),
/// and the cursor is read, compacted, and evaluated in memory. Cursor
/// migration on disk belongs to the explicit ACK path, not to a snapshot a
/// hook runs on the model-request path.
pub fn unread_messages(paths: &Paths, id: Uuid) -> Result<Vec<MessageEnvelope>> {
    let record = read_session_record(paths, id).ok();
    let tag = record.as_ref().map(|r| r.tag.as_str()).unwrap_or("");
    let engine = record.as_ref().map(|r| r.engine.as_str()).unwrap_or("");

    let mut unread: Vec<MessageEnvelope> = Vec::new();
    for mailbox in mailboxes(paths, id)? {
        let mp = message_paths(paths, &mailbox.workspace);
        if !mp.msgs_dir.is_dir() {
            continue;
        }
        // Mailbox then cursor, matching the write path's lock order; both
        // nonblocking. Message files are only ever replaced atomically, so
        // the listing under these locks is a consistent snapshot.
        let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), true)
            .with_context(|| format!("mailbox {} is busy, retry", mailbox.workspace.display()))?;
        let _cursor = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, id), true)
            .with_context(|| format!("cursor in {} is busy, retry", mailbox.workspace.display()))?;
        let mut cursor = read_cursor_file(&mp.cursors_dir.join(format!("{id}.json")))?;
        compact_cursor(&mut cursor, &retained_message_ids(&mp.msgs_dir)?);
        for envelope in list_messages_in(&mp, &mailbox.workspace)? {
            let mine = if mailbox.subscribed {
                addressed_to(&envelope, id, tag, engine)
            } else {
                explicitly_addressed(&envelope, id)
            };
            if mine && !cursor.is_acked(envelope.id) {
                unread.push(envelope);
            }
        }
    }
    unread.sort_by_key(|envelope| envelope.id);
    Ok(unread)
}

/// The one addressing shape that means "this exact session, wherever it
/// lives": a tag recipient with the session id resolved at send time.
/// Broadcasts and engine matches are audience-wide; tag-only recipients may
/// name a tag this session merely holds now.
pub(crate) fn explicitly_addressed(envelope: &MessageEnvelope, id: Uuid) -> bool {
    matches!(&envelope.to, Recipient::Tag { session_id: Some(sid), .. } if *sid == id)
}

fn explicitly_addressed_in(paths: &Paths, workspace: &Path, id: Uuid) -> bool {
    // `workspace` passed the reverse-metadata check, so this derives the
    // same mailbox directory the scan found; reading through the public
    // helper keeps the key derivation in exactly one place.
    let mp = message_paths(paths, workspace);
    if !mp.msgs_dir.is_dir() {
        return false;
    }
    list_messages_in(&mp, workspace)
        .map(|messages| {
            messages
                .iter()
                .any(|envelope| explicitly_addressed(envelope, id))
        })
        .unwrap_or(false)
}

/// The reverse mapping every mailbox carries: `workspace.json` names the
/// canonical workspace the key directory was derived from. The mapping is
/// only adopted when re-deriving the key from the named workspace reproduces
/// the directory name — a stray or hand-edited metadata file contributes
/// nothing rather than pointing the scan at a foreign mailbox.
fn reverse_metadata(directory: &Path) -> Option<PathBuf> {
    const MAX_METADATA_BYTES: usize = 64 * 1024;
    let metadata_path = directory.join("workspace.json");
    let bytes = read_bounded_regular_file(&metadata_path, "mailbox metadata", MAX_METADATA_BYTES)
        .ok()??;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let workspace = PathBuf::from(value.get("workspace")?.as_str()?);
    if workspace_key(&workspace) != directory.file_name()?.to_str()? {
        return None;
    }
    Some(workspace)
}
