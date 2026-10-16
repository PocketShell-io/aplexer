//! Windows hook command lines: one `a` invocation rendered for the shell
//! that will run it.
//!
//! Engines hand a hook `command` string to whatever shell they use on
//! Windows (Claude Code: Git Bash or PowerShell; Codex: cmd; Gemini:
//! PowerShell; others unknown), and the three disagree on every part of the
//! line that matters here:
//!
//! | shell | spaced path       | "never fail" tail |
//! |-------|-------------------|-------------------|
//! | bash  | `"p" args`        | `\|\| true`       |
//! | cmd   | `"p" args`        | `\|\| exit 0`     |
//! | pwsh  | `& "p" args`      | `; exit 0`        |
//!
//! `HookShell::Auto` (the default, override with `APLEXER_HOOK_SHELL` =
//! `bash|cmd|powershell|auto`) emits a line that parses and runs under all
//! three using a `powershell.exe -Command` wrapper that preserves stdin
//! and stdout and always exits successfully, including outside a session.
//! Paths use forward slashes because Windows APIs accept them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookShell {
    Auto,
    Bash,
    Cmd,
    PowerShell,
}

impl HookShell {
    pub fn from_env() -> Self {
        match std::env::var("APLEXER_HOOK_SHELL")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "bash" | "sh" | "posix" => Self::Bash,
            "cmd" => Self::Cmd,
            "powershell" | "pwsh" => Self::PowerShell,
            _ => Self::Auto,
        }
    }
}

fn is_plain(word: &str) -> bool {
    !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@_-+=:,./~".contains(&b))
}

fn forward(path: &str) -> String {
    path.replace('\\', "/")
}

/// Render `<a_bin> <args>` (args are plain words, no quoting needed) for
/// `shell`.
pub fn command_line(shell: HookShell, a_bin: &str, args: &str) -> String {
    let path = forward(a_bin);
    let quoted = || format!("\"{path}\"");
    match shell {
        HookShell::Bash => {
            let p = if is_plain(&path) {
                path.clone()
            } else {
                quoted()
            };
            format!("{p} {args} || true")
        }
        HookShell::Cmd => {
            let p = if is_plain(&path) {
                path.clone()
            } else {
                quoted()
            };
            format!("{p} {args} || exit 0")
        }
        HookShell::PowerShell => {
            if is_plain(&path) {
                format!("{path} {args}; exit 0")
            } else {
                format!("& {} {args}; exit 0", quoted())
            }
        }
        HookShell::Auto => powershell_wrapper(a_bin, args),
    }
}

/// `powershell.exe -Command "& '<path>' <args>; exit 0"`: valid in cmd, bash
/// and PowerShell alike, whatever the path holds (spaces, parentheses, `'`).
pub fn powershell_wrapper(a_bin: &str, args: &str) -> String {
    let ps_path = forward(a_bin).replace('\'', "''");
    format!("powershell.exe -NoProfile -NonInteractive -Command \"& '{ps_path}' {args}; exit 0\"")
}

/// Strip the tail/wrapper decoration every form above can carry from the
/// end of a command, leaving `<launcher> <args>`.
pub fn strip_tail(command: &str) -> &str {
    let mut rest = command.trim_end();
    loop {
        let before = rest;
        for tail in ["|| true", "|| exit 0", "; exit 0\"", "; exit 0", "\""] {
            if let Some(stripped) = rest.strip_suffix(tail) {
                rest = stripped.trim_end();
                break;
            }
        }
        if rest == before {
            return rest;
        }
    }
}
