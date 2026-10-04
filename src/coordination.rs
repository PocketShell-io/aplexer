//! Work declarations and cross-session workspace context (`a work join` /
//! `a work leave` / `a context`, docs/coordination-packages.md).
//!
//! A declaration is one session's advisory claim on a workspace: what it is
//! doing (`task`), how (`mode`), and where it intends to edit (relative
//! `scopes`). Declarations live in a per-session sidecar,
//! `paths.state_session(id)/coordination.json`, next to the session record:
//! the session id stays the stable identity and the participation list is
//! replaceable data, so renaming, moving, or exiting never rewrites claims.
//! `context` assembles the safe, peer-presentable picture of a directory --
//! identities, declarations, relations, conservative overlaps, and unread
//! message pointers -- and never surfaces environment, command lines, or
//! message bodies.
//!
//! Everything here is advisory. Nothing enforces a scope; overlapping claims
//! are settled between peers over the messaging inbox, and read/review
//! overlap is by definition not exclusive ownership.

mod gitinfo;
mod mail;
mod state;
#[cfg(test)]
mod tests;
mod view;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path};
use uuid::Uuid;

use crate::{canonical_workspace, now_ms, read_session_record, Paths};

pub use gitinfo::GitInfo;
pub use mail::{mailbox_workspaces, unread_messages};
pub use state::Participation;
pub use view::{
    context, render_context, DeclarationView, Overlap, PeerContext, Relation, SessionSnapshot,
    UnreadRef, WorkspaceContext, WorkspaceLocation,
};

/// How a session intends to touch a declared workspace. Serialize as the
/// lowercase wire word (`"read"`, `"edit"`, `"review"`) so JSON output and
/// human rendering agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkMode {
    Read,
    Edit,
    Review,
}

impl WorkMode {
    /// The wire/display word, identical to this enum's serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkMode::Read => "read",
            WorkMode::Edit => "edit",
            WorkMode::Review => "review",
        }
    }
}

impl std::fmt::Display for WorkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

const MAX_TASK_BYTES: usize = 2048;
const MAX_SCOPES: usize = 64;
const MAX_SCOPE_BYTES: usize = 512;

/// Records a participation for `id` in `workspace`, replacing any earlier
/// declaration for the same canonical directory. Requires an existing session
/// record (the session is the only identity a claim can hang off), a
/// non-empty bounded task, and relative scopes that cannot escape the
/// workspace. Idle time never releases a declaration; only [`leave`] does.
pub fn join(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    task: &str,
    mode: WorkMode,
    scopes: &[String],
) -> Result<Participation> {
    read_session_record(paths, id).with_context(|| {
        format!("work declarations require an existing session record for {id}")
    })?;
    let canonical = canonical_workspace(workspace)
        .with_context(|| format!("canonicalize declared workspace {}", workspace.display()))?;
    ensure_directory(&canonical, "declared")?;
    let task = validate_task(task)?;
    let scopes = validate_scopes(scopes)?;
    let git = gitinfo::git_info(&canonical);
    let now = now_ms();
    let mut participation = Participation {
        workspace: canonical,
        task,
        mode,
        scopes,
        git,
        created_at_ms: now,
        updated_at_ms: now,
    };
    state::update(paths, id, |claims| {
        if let Some(existing) = claims
            .iter_mut()
            .find(|d| d.workspace == participation.workspace)
        {
            participation.created_at_ms = existing.created_at_ms;
            *existing = participation.clone();
        } else {
            state::push_bounded(claims, participation.clone())?;
        }
        Ok(((), true))
    })?;
    Ok(participation)
}

/// Removes `id`'s declaration for `workspace` and reports whether one was
/// removed. Tolerates a session whose record or state directory is already
/// gone: there is nothing left to release, which is `false`, not an error.
pub fn leave(paths: &Paths, id: Uuid, workspace: &Path) -> Result<bool> {
    if !paths.state_session(id).is_dir() {
        return Ok(false);
    }
    let canonical = canonical_workspace(workspace)
        .with_context(|| format!("canonicalize released workspace {}", workspace.display()))?;
    state::update(paths, id, |claims| {
        let before = claims.len();
        claims.retain(|d| d.workspace != canonical);
        let released = claims.len() != before;
        Ok((released, released))
    })
}

fn validate_task(task: &str) -> Result<String> {
    let bounded = sanitize_text(task, MAX_TASK_BYTES);
    if bounded.is_empty() {
        bail!("task must not be empty");
    }
    // The sanitizer's cap is in characters; a multibyte task could exceed
    // the byte budget fourfold. Both bounds hold on what is stored.
    if bounded.len() > MAX_TASK_BYTES {
        bail!(
            "task exceeds the {MAX_TASK_BYTES}-byte cap (got {} bytes)",
            bounded.len()
        );
    }
    Ok(bounded)
}

/// Only directories are valid workspaces: a file path is almost certainly a
/// typo'd scope, and storing one would put file-level claims in a
/// workspace-level field. Missing paths stay allowed (a workspace may be
/// created after it is declared).
pub(crate) fn ensure_directory(canonical: &Path, role: &str) -> Result<()> {
    if fs::metadata(canonical)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
    {
        bail!(
            "{role} workspace {} is a file, not a directory",
            canonical.display()
        );
    }
    Ok(())
}

fn validate_scopes(scopes: &[String]) -> Result<Vec<String>> {
    if scopes.len() > MAX_SCOPES {
        bail!(
            "at most {MAX_SCOPES} scopes per declaration (got {})",
            scopes.len()
        );
    }
    let mut validated: Vec<String> = Vec::new();
    for scope in scopes {
        let scope = validate_scope(scope)?;
        if !validated.contains(&scope) {
            validated.push(scope);
        }
    }
    Ok(validated)
}

fn validate_scope(scope: &str) -> Result<String> {
    if scope.is_empty() {
        bail!("scope must not be empty");
    }
    if scope.len() > MAX_SCOPE_BYTES {
        bail!(
            "scope exceeds the {MAX_SCOPE_BYTES}-byte cap (got {} bytes)",
            scope.len()
        );
    }
    if scope.chars().any(char::is_control) {
        bail!("scope contains control characters");
    }
    let path = Path::new(scope);
    if path.is_absolute() {
        bail!("scope {scope:?} must be relative to the workspace");
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                bail!("scope {scope:?} must not escape the workspace");
            }
            Component::Prefix(_) => {
                bail!("scope {scope:?} is not a valid relative path");
            }
            _ => {}
        }
    }
    Ok(scope.to_string())
}

/// Control characters (including ANSI escapes) become spaces, edges are
/// trimmed, and the result is capped at `max_chars` characters with an
/// ellipsis. Everything peers render or store about each other goes through
/// this: peer-provided text is bounded, inert coordination data.
pub(crate) fn sanitize_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}
