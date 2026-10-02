//! Managed command identity, kept separate from state-report.
//!
//! Two generations of managed model-context commands exist:
//!
//! - Legacy: `a message hook-notice --engine <engine>` under
//!   `aplexer-managed-inbox-hook-v1`. Nothing installs it anymore; the
//!   installer replaces it (by marker) with the awareness command, and
//!   uninstall removes it from configs written by older binaries.
//! - Current: `a context hook --engine <engine>` under
//!   `aplexer-managed-awareness-hook-v1`, one injection source per engine
//!   across the engine's context-capable events.

use super::*;

const LEGACY_NOTICE_MARKER: &str = "aplexer-managed-inbox-hook-v1";
pub(crate) const AWARENESS_MARKER: &str = "aplexer-managed-awareness-hook-v1";

/// The current synchronous model-context command. It handles errors
/// quietly; `|| true` also protects ordinary tool execution if the binary
/// disappears.
pub(crate) fn context_command(a_bin: &str, engine: &str) -> String {
    format!(
        "{} context hook --engine {engine} 2>/dev/null || true # {AWARENESS_MARKER}",
        shell_quote(a_bin)
    )
}

/// Whether a command is the current awareness hook for `engine` (used for
/// precise per-event status, the same way `reports_state` works).
pub(crate) fn reports_context(command: &str, engine: &str) -> bool {
    command.trim_end().ends_with(&format!(
        " context hook --engine {engine} 2>/dev/null || true # {AWARENESS_MARKER}"
    ))
}

/// Whether a command is a legacy inbox-notice command, any engine. The
/// marker is the identity: engine-specific matching would leave an old
/// `--engine zzz` install behind forever.
pub(crate) fn is_legacy_notice_command(command: &str) -> bool {
    command
        .trim_end()
        .ends_with(&format!("2>/dev/null || true # {LEGACY_NOTICE_MARKER}"))
}

/// Whether a command is a current awareness hook, any engine.
pub(crate) fn is_awareness_command(command: &str) -> bool {
    command
        .trim_end()
        .ends_with(&format!("|| true # {AWARENESS_MARKER}"))
}

pub(crate) fn is_managed_hook_command(command: &str) -> bool {
    is_state_report_command(command)
        || is_legacy_notice_command(command)
        || is_awareness_command(command)
}
