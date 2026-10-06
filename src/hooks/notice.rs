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
//!
//! On Unix the current command carries a `2>/dev/null || true # <marker>`
//! tail. That tail is not valid under cmd/PowerShell, so on Windows the
//! command is the bare `a context hook --engine <engine>`; detection accepts
//! both forms on every platform so configs survive a move between shells.

use super::*;

const LEGACY_NOTICE_MARKER: &str = "aplexer-managed-inbox-hook-v1";
pub(crate) const AWARENESS_MARKER: &str = "aplexer-managed-awareness-hook-v1";
const CONTEXT_INFIX: &str = " context hook --engine ";

/// The current synchronous model-context command. It handles errors
/// quietly; `|| true` also protects ordinary tool execution if the binary
/// disappears.
pub(crate) fn context_command(a_bin: &str, engine: &str) -> String {
    #[cfg(windows)]
    {
        format!("{} context hook --engine {engine}", shell_quote(a_bin))
    }
    #[cfg(not(windows))]
    format!(
        "{} context hook --engine {engine} 2>/dev/null || true # {AWARENESS_MARKER}",
        shell_quote(a_bin)
    )
}

/// The engine named by a current awareness command (either tail form), or
/// `None` when the command is not one.
fn awareness_engine(command: &str) -> Option<&str> {
    let mut rest = command.trim_end();
    let marker_tail = format!(" # {AWARENESS_MARKER}");
    let had_marker = if let Some(stripped) = rest.strip_suffix(&marker_tail) {
        rest = stripped.trim_end();
        true
    } else {
        false
    };
    if let Some(stripped) = rest.strip_suffix("|| true") {
        rest = stripped.trim_end();
        rest = rest.strip_suffix("2>/dev/null").unwrap_or(rest).trim_end();
    } else if had_marker {
        // A marker with an unrecognised tail is not ours to interpret.
        return None;
    }
    let (_, engine) = rest.rsplit_once(CONTEXT_INFIX)?;
    (!engine.is_empty() && !engine.contains(char::is_whitespace)).then_some(engine)
}

/// Whether a command is the current awareness hook for `engine` (used for
/// precise per-event status, the same way `reports_state` works).
pub(crate) fn reports_context(command: &str, engine: &str) -> bool {
    awareness_engine(command) == Some(engine)
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
    awareness_engine(command).is_some()
}

pub(crate) fn is_managed_hook_command(command: &str) -> bool {
    is_state_report_command(command)
        || is_legacy_notice_command(command)
        || is_awareness_command(command)
}

#[cfg(test)]
mod form_tests {
    use super::*;

    #[test]
    fn both_tail_forms_identify_the_awareness_hook() {
        let unix =
            format!("/bin/a context hook --engine claude 2>/dev/null || true # {AWARENESS_MARKER}");
        let bare = "C:/bin/a.exe context hook --engine claude";
        for command in [unix.as_str(), bare] {
            assert!(reports_context(command, "claude"), "{command}");
            assert!(!reports_context(command, "codex"), "{command}");
            assert!(is_awareness_command(command), "{command}");
            assert!(is_managed_hook_command(command), "{command}");
        }
        assert!(!is_awareness_command(
            "a message hook-notice --engine claude"
        ));
        assert!(!is_awareness_command(
            "a context hook --engine claude extra"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_commands_avoid_posix_tails_and_quote_spaces() {
        let command = context_command(r"C:\Program Files\a\a.exe", "codex");
        assert_eq!(
            command,
            "\"C:/Program Files/a/a.exe\" context hook --engine codex"
        );
        assert!(reports_context(&command, "codex"));
        let report = state_report_command(r"C:\bin\a.exe", "idle");
        assert_eq!(report, "C:/bin/a.exe state-report idle");
        assert!(!report.contains("||"));
        assert!(codex_notify_line(r"C:\bin\a.exe")
            .starts_with(r#"notify = ["C:/bin/a.exe", "state-report", "idle"]"#));
    }
}
