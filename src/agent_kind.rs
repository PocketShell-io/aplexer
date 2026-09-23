//! Which coding agent is running inside a session, detected at query time.
//!
//! Every pocketshell-created aplexer session is `engine: "shell"` with the
//! agent launched by hand inside it, so `engine` cannot name the agent. What
//! aplexer does own is the workload process (`SessionRecord::workload_pid`)
//! and, through `/proc/<pid>/task/*/children`, its whole descendant tree --
//! the same walk `worker::descendant_pids` uses for containment. This module
//! reuses that walker to answer "which agent is live in this session right
//! now" from the process tree, and -- through `DetectedAgent` -- which of
//! the agent's configured variations (profiles, spec.md 9) that agent runs
//! as. Everything the resolution needs comes from the configuration rather
//! than from this module: the profile env vars from `config::discovery`'s
//! rule table (`PROFILE_DISCOVERY_RULES`, the same table that
//! auto-discovers profile dirs), and the variation tokens (`zcodex`,
//! `zodex`, a user profile's executable, ...) from [`profile_variants`]
//! over the loaded `Config` -- so a variation defined only in some other
//! installation's config is detected there without a code change.
//!
//! Two deliberate properties:
//!
//! * **Nothing is persisted.** Detection runs only when a caller asks for
//!   `a list --json` / `a snapshot` / `a status --json`. A record on disk
//!   never carries an `agent` field, so it cannot go stale, and the worker's
//!   hot path never pays for this.
//! * **Every read is defensive.** A pid that exits between the `children`
//!   read and the `comm` read, an unreadable `/proc` entry, a permission
//!   error -- all are skipped. Detection degrades to "no agent found"
//!   (`None`), never to an error that would fail the whole listing.

use std::fmt;

use anyhow::{bail, Result};
use serde::Serialize;

mod detect;
mod profile;
mod rules;
#[cfg(test)]
mod tests;

pub use detect::{detect_agent, detect_agent_detailed};
pub use profile::{profile_variants, ProfileVariants};
pub use rules::classify_token;

/// The agent a bare token names, without any process to inspect: the same
/// classifier the `/proc` walk applies to a comm/cmdline, applied to the
/// token the user pinned with `a agent` (`SessionRecord::agent_override`).
/// `claude`/`codex`/... resolve through the canonical rules; a configured
/// variation's token (`zcodex`, a profile's executable basename) resolves
/// to that variation's kind and profile id. `None` for a token nothing
/// classifies -- callers degrade to live detection rather than report a
/// pin the config no longer knows.
pub fn resolve_agent_token(token: &str, variants: &ProfileVariants) -> Option<DetectedAgent> {
    rules::classify_token_detailed(token, variants)
        .map(|(kind, profile)| DetectedAgent { kind, profile })
}

/// Whether the token `a agent` is asked to pin (`SessionRecord::agent_override`)
/// names an agent the way detection would spell it: a canonical agent name
/// (`claude`), or one of the config's variation tokens (`zcodex`, a profile
/// id, an engine id, a command basename -- [`profile_variants`]). The
/// refusal names what would resolve, so the fix is in the error. `config`
/// is `None` when the config cannot be loaded, and then only the canonical
/// names validate -- the same degradation `default_profile_variants`
/// applies to detection, so a broken config can never make pinning fail
/// completely.
pub fn validate_agent_token(token: &str, config: Option<&crate::config::Config>) -> Result<()> {
    if resolve_agent_token(token, &config.map(profile_variants).unwrap_or_default()).is_some() {
        return Ok(());
    }
    let mut known: Vec<String> = AgentKind::ALL
        .iter()
        .map(|kind| kind.name().to_owned())
        .collect();
    if let Some(config) = config {
        known.extend(profile_variants(config).keys().cloned());
    }
    bail!(
        "unknown agent token {token:?}; expected an agent name or a configured variation: {}",
        known.join(", ")
    )
}

/// The `/proc` root detection reads. Injectable so the unit tests classify a
/// synthetic tree with zero live processes.
pub const DEFAULT_PROC_ROOT: &str = "/proc";

/// An agent aplexer can recognise from a workload's process tree. The serde
/// representation is the lowercase name that appears on the wire, identical
/// to the kinds pocketshell's own classifier returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
    Grok,
}

impl AgentKind {
    /// Every kind there is a canonical token rule for, in the rules' own
    /// order -- the vocabulary `a agent`'s refusal suggests.
    pub const ALL: [AgentKind; 4] = [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Opencode,
        AgentKind::Grok,
    ];

    /// The wire/display name, identical to this enum's serde representation.
    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
            AgentKind::Grok => "grok",
        }
    }

    /// The profile-config environment variable this agent honours and the
    /// basename of its default config dir, taken from `config::discovery`'s
    /// rule table -- the single source both auto-discovery and detection
    /// read, so they can never disagree about where a variation lives.
    /// `None` for the agents with no rule -- opencode has no profile env
    /// var and grok is not known to have one, so neither can run as
    /// anything but the default profile.
    pub(super) fn profile_env(self) -> Option<(&'static str, &'static str)> {
        crate::config::PROFILE_DISCOVERY_RULES
            .iter()
            .find(|rule| rule.engine == self.name())
            .map(|rule| (rule.env_var, rule.default_dirname))
    }
}

/// Which agent is live in a session, plus which of the agent's configured
/// variations ("profiles", spec.md 9) it is running as. Detection names the
/// variation the same way `config::discovery` names profiles -- the config
/// dir's stem minus its leading dot (`~/.zodex` -> `zodex`) -- so a detected
/// stem is always the id that profile is (or would be) registered under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    /// The variation's stem when the agent runs as a named profile, `None`
    /// for the engine's own default config. Never a display label, always
    /// a profile id: `profile_label` is where the "default" spelling comes
    /// from.
    pub profile: Option<String>,
}

impl DetectedAgent {
    /// The wire/display name of the variation: the profile id, or
    /// `"default"` when the agent runs the engine's own config untouched.
    pub fn profile_label(&self) -> &str {
        self.profile.as_deref().unwrap_or("default")
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}
