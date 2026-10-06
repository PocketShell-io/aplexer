//! The engines every installation starts with.

use std::collections::BTreeMap;
use std::env;

use super::{Config, EngineConfig};

/// Transcript-family normalization: a variant engine -- a fork of a built-in
/// engine CLI with the same wire format and the same native conversation-log
/// location -- is identified with that engine's family for parsing, while
/// sessions and emitted events keep the variant's own id. `zcodex` (a
/// codex-rs fork: same `-c` overrides, same rollout JSONL under
/// `CODEX_HOME`/`~/.codex`) is the one shipped alias, so a user config that
/// defines its own `zcodex` engine rides the codex machinery; everything
/// else is its own family. aplexer ships no `zcodex` engine itself -- the
/// fork is one box's setup, defined in that box's config file (README,
/// "A real config").
pub fn engine_family(engine: &str) -> &str {
    match engine {
        "zcodex" => "codex",
        other => other,
    }
}

/// The default `shell` engine argv. Unix: `$SHELL -l` (login shell).
#[cfg(unix)]
fn default_shell_argv() -> Vec<String> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    vec![shell, "-l".to_string()]
}

/// Windows: `pwsh.exe` if on PATH, else `powershell.exe`, else `%COMSPEC%`
/// (then `cmd.exe`). No login flag: Windows shells have none.
#[cfg(windows)]
fn default_shell_argv() -> Vec<String> {
    let path = env::var("PATH").unwrap_or_default();
    for candidate in ["pwsh.exe", "powershell.exe"] {
        if let Some(found) = super::pathfix::which_in(candidate, &path) {
            return vec![found.to_string_lossy().into_owned()];
        }
    }
    vec![env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())]
}

impl Config {
    /// The engines every installation gets, before user config extends or
    /// overrides them. Variant engines (`zcodex`, a private claude wrapper)
    /// are deliberately absent: they are one installation's config, not a
    /// shared default -- define them in `[engines.*]` or `[profiles.*]`.
    /// `opencode` is the PocketShell built-in
    /// (tools/pocketshell/src/pocketshell/engines.py ::builtin_manifests)
    /// that aplexer's engine set was missing -- required for aplexer to
    /// become authoritative for pocketshell's engine registry
    /// (pocketshell-integration-plan.md 0.1).
    pub(super) fn builtin_engines() -> BTreeMap<String, EngineConfig> {
        fn engine(command: &[&str], skip_permissions_argv: &[&str]) -> EngineConfig {
            let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
            EngineConfig {
                command: strings(command),
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                skip_permissions_argv: strings(skip_permissions_argv),
                // task_argv is resolved per engine/family by `crate::task`'s
                // builtin table; builtins leave it empty so one table owns
                // the noninteractive defaults.
                task_argv: Vec::new(),
            }
        }
        let shell_argv = default_shell_argv();
        let shell_refs: Vec<&str> = shell_argv.iter().map(String::as_str).collect();
        // Skip-permissions argv is ported from pocketshell engines.py's
        // LaunchSpecs. `--auto` is opencode's documented flag for
        // "auto-approve permissions that are not explicitly denied";
        // `--yolo` is an undocumented (`hidden: true`) alias for the same
        // thing, so the documented one is what we pass.
        //
        // opencode's `external_directory` permission defaults to `"*":
        // "ask"` (packages/opencode/src/agent/agent.ts), so a launch
        // without this flag prompts for every path outside the project --
        // which is why opencode is listed here rather than left empty.
        // opencode's `permission` block in opencode.json is the primary
        // mechanism; this is the defense-in-depth second layer, and the two
        // must stay in sync (see `every_engine_with_a_skip_flag_declares_one`).
        // gemini is an aplexer-only extra with no pocketshell source, so it
        // stays empty.
        let engines: [(&str, &[&str], &[&str]); 7] = [
            ("shell", shell_refs.as_slice(), &[]),
            (
                "codex",
                &["codex", "-c", "check_for_update_on_startup=false"],
                &["--dangerously-bypass-approvals-and-sandbox"],
            ),
            ("claude", &["claude"], &["--dangerously-skip-permissions"]),
            ("gemini", &["gemini"], &[]),
            ("antigravity", &["agy"], &["--dangerously-skip-permissions"]),
            ("grok", &["grok"], &["--always-approve"]),
            ("opencode", &["opencode"], &["--auto"]),
        ];
        engines
            .into_iter()
            .map(|(name, command, skip)| (name.to_string(), engine(command, skip)))
            .collect()
    }
}
