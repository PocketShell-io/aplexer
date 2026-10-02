//! Binding a hook to the one actual session it speaks for.
//!
//! Two identities are in play and they must never be conflated:
//!
//! - the **aplexer session id**: what `discover_session_id` finds in the
//!   ambient environment (its own or an ancestor's), resolved to exactly
//!   one session record. This alone decides the binding.
//! - the **native harness conversation id** in the hook payload (Claude
//!   and Codex UUIDs, OpenCode `ses_...`, Antigravity `conversationId`).
//!   It lives in a different namespace and is never compared to aplexer
//!   UUIDs; `state::admit` records it per engine on first fire and rejects
//!   a *conflicting* one once established, so a second conversation
//!   spawned inside the same aplexer session cannot consume its context.

use crate::{discover_session_id, list_records, Paths};
use anyhow::Result;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct BoundSession {
    pub(crate) id: Uuid,
    pub(crate) workspace: PathBuf,
    pub(crate) tag: String,
    pub(crate) engine: String,
}

pub(crate) fn bind_session(paths: &Paths, engine: &str) -> Result<Option<BoundSession>> {
    bind_discovered(paths, engine, discover_session_id())
}

/// `bind_session` with the ambient id passed in, so tests exercise the
/// full binding rule without touching the process environment.
pub(crate) fn bind_discovered(
    paths: &Paths,
    engine: &str,
    discovered: Option<Uuid>,
) -> Result<Option<BoundSession>> {
    let Some(discovered) = discovered else {
        return Ok(None);
    };
    let records = list_records(paths)?;
    let mut matches = records.iter().filter(|record| record.id == discovered);
    let Some(record) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || !engine_matches(&record.engine, engine) {
        return Ok(None);
    }
    Ok(Some(BoundSession {
        id: record.id,
        workspace: record.workspace.clone(),
        tag: record.tag.clone(),
        engine: record.engine.clone(),
    }))
}

/// A `shell`-engine record hosts any CLI's hook; a `zcodex` record answers
/// the codex hook through the shared `CODEX_HOME` -- the same rule the
/// mailbox notice binds by.
pub(crate) fn engine_matches(record_engine: &str, hook_engine: &str) -> bool {
    record_engine == "shell"
        || record_engine == hook_engine
        || (hook_engine == "codex" && record_engine == "zcodex")
}
