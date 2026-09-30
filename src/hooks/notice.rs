//! Managed PostToolUse command identity, kept separate from state-report.

use super::*;

const NOTICE_MARKER: &str = "aplexer-managed-inbox-hook-v1";

/// Synchronous model-context hook. It handles errors quietly; `|| true`
/// also protects ordinary tool execution if the binary disappears.
pub(crate) fn notice_command(a_bin: &str, engine: &str) -> String {
    format!(
        "{} message hook-notice --engine {engine} 2>/dev/null || true # {NOTICE_MARKER}",
        shell_quote(a_bin)
    )
}

pub(crate) fn reports_notice(command: &str, engine: &str) -> bool {
    command.trim_end().ends_with(&format!(
        " message hook-notice --engine {engine} 2>/dev/null || true # {NOTICE_MARKER}"
    ))
}

pub(crate) fn is_managed_hook_command(command: &str) -> bool {
    is_state_report_command(command)
        || ["claude", "codex"]
            .iter()
            .any(|engine| reports_notice(command, engine))
}
