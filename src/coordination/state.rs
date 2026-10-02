//! The per-session coordination sidecar: `coordination.json` under the
//! session's state directory, guarded by a file lock and written with the
//! crate's atomic-write discipline. The sidecar is bounded and private: it
//! lives in the session's 0700 state directory, its files are 0600, and the
//! declaration count is capped.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use super::GitInfo;
use super::WorkMode;
use crate::persist::read_bounded_regular_file;
use crate::{atomic_write_json, now_ms, FileLock, Paths};

const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: usize = 256 * 1024;
const MAX_DECLARATIONS: usize = 64;

/// One advisory claim on one canonical workspace. The owning session id is
/// deliberately not a field: identity comes from the sidecar's own location
/// (`state_session(id)`), so a participation is inert data that survives
/// renames and can be dropped wholesale with the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participation {
    /// The canonical declared directory. Aliases (symlinks, `..`, relative
    /// spellings) resolve here before anything is stored, so two sessions
    /// naming the same checkout by different paths share one claim.
    pub workspace: PathBuf,
    pub task: String,
    pub mode: WorkMode,
    pub scopes: Vec<String>,
    /// Git facts captured at join time: the worktree checkout root and the
    /// repository's common dir, when the directory is inside a git repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CoordinationState {
    schema_version: u32,
    session_id: Uuid,
    updated_at_ms: u64,
    #[serde(default)]
    declarations: Vec<Participation>,
}

impl CoordinationState {
    fn fresh(id: Uuid) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            session_id: id,
            updated_at_ms: now_ms(),
            declarations: Vec::new(),
        }
    }
}

pub(crate) fn state_path(paths: &Paths, id: Uuid) -> PathBuf {
    paths.state_session(id).join("coordination.json")
}

fn lock_path(paths: &Paths, id: Uuid) -> PathBuf {
    paths.state_session(id).join("coordination.lock")
}

/// Strict load for the sidecar's owner: a malformed file is surfaced, never
/// silently replaced, because overwriting coordination state could drop a
/// live claim. A missing file is the normal first-join state.
fn load_strict(paths: &Paths, id: Uuid) -> Result<CoordinationState> {
    let path = state_path(paths, id);
    let Some(bytes) = read_bounded_regular_file(&path, "coordination state", MAX_STATE_BYTES)?
    else {
        return Ok(CoordinationState::fresh(id));
    };
    let state: CoordinationState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse coordination state {}", path.display()))?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        bail!(
            "unsupported coordination state schema {} in {}",
            state.schema_version,
            path.display()
        );
    }
    if state.session_id != id {
        bail!(
            "coordination state {} belongs to session {}, not {id}",
            path.display(),
            state.session_id
        );
    }
    Ok(state)
}

/// Tolerant load for peer scans (`a context` reads every session's sidecar):
/// missing, unreadable, or foreign files contribute no claims and no errors.
/// Peer declarations are advisory; one bad file must not hide the rest of the
/// workspace picture.
pub(crate) fn load_tolerant(paths: &Paths, id: Uuid) -> Option<Vec<Participation>> {
    load_strict(paths, id).ok().map(|state| state.declarations)
}

/// Applies `f` to the session's declaration list under the sidecar's lock,
/// writing back only when `f` reports a mutation. Lock order matches the
/// rest of the crate: one lock, no nesting.
pub(crate) fn update<T>(
    paths: &Paths,
    id: Uuid,
    f: impl FnOnce(&mut Vec<Participation>) -> Result<(T, bool)>,
) -> Result<T> {
    let _lock = FileLock::exclusive(&lock_path(paths, id), false)?;
    let mut state = load_strict(paths, id)?;
    let (value, mutated) = f(&mut state.declarations)?;
    if mutated {
        state.updated_at_ms = now_ms();
        // Reject the next state before writing anything: a rejected join
        // must preserve the previous sidecar exactly, or a valid repeated
        // join could make the session's own state unreadable and its
        // claims unreleasable.
        let bytes = serde_json::to_vec_pretty(&state).context("serialize coordination state")?;
        if bytes.len() + 1 > MAX_STATE_BYTES {
            bail!(
                "coordination state would exceed the {MAX_STATE_BYTES}-byte cap ({} bytes); \
                 release a declaration with `a work leave` first",
                bytes.len() + 1
            );
        }
        atomic_write_json(&state_path(paths, id), &state)?;
    }
    Ok(value)
}

pub(crate) fn push_bounded(
    declarations: &mut Vec<Participation>,
    participation: Participation,
) -> Result<()> {
    if declarations.len() >= MAX_DECLARATIONS {
        bail!(
            "at most {MAX_DECLARATIONS} declarations per session; release one with `a work leave` first"
        );
    }
    declarations.push(participation);
    Ok(())
}
