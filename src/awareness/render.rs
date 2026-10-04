//! Turning core coordination state into bounded model-facing text.

use super::binding::BoundSession;
use super::payload::HookPayload;
use crate::messaging::MessageEnvelope;
use crate::{coordination, Paths};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// At most this many distinct foreign destinations per hook fire; a tool
/// storm touching a dozen trees must not balloon the injection.
pub(crate) const MAX_FOREIGN_DESTINATIONS: usize = 3;
/// Each destination's peer render is bounded; coordination state beyond
/// this is summarized as truncated rather than injected whole.
const MAX_FOREIGN_RENDER_CHARS: usize = 1200;
/// Unread ids listed per notice, matching the mailbox notice's cap.
const MAX_UNREAD_IDS: usize = 5;

pub(crate) struct Composition<'a> {
    /// Include the bootstrap instructions: startup events always, and the
    /// first delivery to an engine without a startup hook.
    pub(crate) bootstrap: bool,
    pub(crate) session: &'a BoundSession,
    /// `coordination::render_context` of the session's own workspace.
    pub(crate) rendered: &'a str,
    pub(crate) unread: &'a [MessageEnvelope],
    /// Every mailbox this session participates in (core multiworkspaces).
    pub(crate) mailboxes: &'a [PathBuf],
    pub(crate) foreign: &'a [(PathBuf, String)],
}

pub(crate) fn compose(parts: &Composition) -> String {
    let mut sections: Vec<String> = Vec::new();
    if parts.bootstrap {
        sections.push(bootstrap(parts));
    }
    let rendered = parts.rendered.trim();
    if !rendered.is_empty() {
        sections.push(rendered.to_string());
    }
    if !parts.unread.is_empty() {
        sections.push(unread_line(parts.unread));
    }
    for (destination, peers) in parts.foreign {
        sections.push(foreign_block(destination, peers));
    }
    sections.join("\n\n")
}

/// Startup bootstrap: behavior instructions that hold even with no peers,
/// plus the unread refs with their read/reply/ack verbs. Session fields
/// are sanitized/bounded here exactly like core sanitizes its projection
/// -- the bootstrap rides hook output, never raw records. Never mentions
/// message bodies -- ids only.
fn bootstrap(parts: &Composition) -> String {
    let sanitized = |text: &str, cap: usize| -> String {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        if cleaned.chars().count() > cap {
            cleaned.chars().take(cap).collect::<String>() + "…"
        } else {
            cleaned
        }
    };
    let tag = sanitized(&parts.session.tag, 40);
    let engine = sanitized(&parts.session.engine, 20);
    let workspace = sanitized(&parts.session.workspace.display().to_string(), 120);
    let mut lines = vec![format!(
        "Aplexer awareness bootstrap: session {tag:?} (engine {engine}, workspace {workspace})."
    )];
    lines.push(
        "Before editing files, run `a context` to see sibling sessions and their declared \
         tasks and scopes."
            .to_string(),
    );
    lines.push(
        "Declare your work before touching files in a workspace: `a work join <workspace> \
         --task \"...\" --mode edit --paths <scope>` (repeat --paths; use --mode read or \
         --mode review for non-edit work) and `a work leave <workspace>` when done. Edit \
         declarations are exclusive: never edit a scope another session has declared for \
         edit without first agreeing the overlap through `a message`; read and review \
         declarations are non-exclusive."
            .to_string(),
    );
    lines.push(
        "Check peer mail now: `a message inbox` lists unread messages, `a message show \
         <id>` reads one, `a message reply` answers, `a message ack <id>` acknowledges it \
         so it stops resurfacing."
            .to_string(),
    );
    if parts.mailboxes.len() > 1 {
        let dirs: Vec<String> = parts
            .mailboxes
            .iter()
            .map(|p| sanitized(&p.display().to_string(), 120))
            .collect();
        lines.push(format!(
            "You participate in {} workspace mailboxes: {}.",
            parts.mailboxes.len(),
            dirs.join(", ")
        ));
    }
    if !parts.unread.is_empty() {
        lines.push(unread_line(parts.unread));
    }
    lines.join("\n")
}

fn unread_line(unread: &[MessageEnvelope]) -> String {
    let ids: Vec<String> = unread
        .iter()
        .take(MAX_UNREAD_IDS)
        .map(|m| m.id.to_string())
        .collect();
    let more = if unread.len() > MAX_UNREAD_IDS {
        format!(" (and {} more)", unread.len() - MAX_UNREAD_IDS)
    } else {
        String::new()
    };
    format!(
        "You have {} unread peer message(s){more}: {}. Read them with `a message inbox`, \
         reply where a response is needed, and acknowledge with `a message ack` once \
         handled.",
        unread.len(),
        ids.join(", ")
    )
}

fn foreign_block(destination: &Path, peers: &str) -> String {
    let mut text = format!(
        "This tool targets {}, outside this session's workspace. Active work there:",
        destination.display()
    );
    let peers = peers.trim();
    if peers.is_empty() {
        text.push_str(" none recorded.");
    } else {
        text.push('\n');
        text.push_str(peers);
    }
    text.push_str(
        "\nBefore editing there, declare it: `a work join <that workspace> --task \"...\" \
         --mode edit --paths <scope>`, and coordinate overlapping scopes through `a \
         message`.",
    );
    text
}

/// For every explicit absolute tool path outside the session's workspace,
/// resolve its destination and render that destination's peers. Paths that
/// fail to canonicalize (not created yet) and paths inside the session's
/// own workspace are skipped; per-destination errors are swallowed -- a
/// peer render must never fail the tool call it rides on.
pub(crate) fn foreign_peers(
    paths: &Paths,
    session: &BoundSession,
    payload: &HookPayload,
) -> Result<Vec<(PathBuf, String)>> {
    let mut destinations: Vec<PathBuf> = Vec::new();
    for raw in payload
        .tool_paths
        .iter()
        .chain(payload.workspace_paths.iter())
    {
        if destinations.len() >= MAX_FOREIGN_DESTINATIONS {
            break;
        }
        let Some(destination) = outside_workspace(raw, &session.workspace) else {
            continue;
        };
        if !destinations.contains(&destination) {
            destinations.push(destination);
        }
    }
    Ok(destinations
        .into_iter()
        .filter_map(|destination| {
            // Per-destination failure (and an empty peer render) is fine:
            // foreign awareness is best-effort and must never fail the
            // tool call it rides on.
            let ctx = coordination::context(paths, Some(session.id), &destination).ok()?;
            let rendered = coordination::render_context(&ctx);
            let rendered = rendered.trim();
            if rendered.chars().count() > MAX_FOREIGN_RENDER_CHARS {
                let truncated: String = rendered
                    .chars()
                    .take(MAX_FOREIGN_RENDER_CHARS)
                    .collect::<String>()
                    + " …[truncated]";
                Some((destination, truncated))
            } else {
                Some((destination, rendered.to_string()))
            }
        })
        .collect())
}

/// Resolve `raw` to the destination whose peers matter: the nearest
/// existing ancestor of the path (a tool may target a file that does not
/// exist yet, and `file_path` names a file whose *directory* is the
/// shared space). Returns `None` for paths inside the session's own
/// workspace and for unprintable paths (control characters from a hostile
/// tool argument are refused, never injected).
pub(crate) fn outside_workspace(raw: &Path, workspace: &Path) -> Option<PathBuf> {
    let text = raw.display().to_string();
    if text.chars().any(char::is_control) {
        return None;
    }
    let mut candidate = raw;
    let canonical = loop {
        match candidate.canonicalize() {
            Ok(canonical) => break canonical,
            Err(_) => candidate = candidate.parent()?,
        }
    };
    let destination = if canonical.is_dir() {
        canonical
    } else {
        canonical.parent()?.to_path_buf()
    };
    if destination.starts_with(workspace) {
        return None;
    }
    Some(destination)
}
