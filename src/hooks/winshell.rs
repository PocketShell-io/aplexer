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
//! three: the binary's 8.3 short path when it needs quoting and one exists
//! (no quoting, no tail), otherwise a `powershell.exe -Command "& '..' ..;
//! exit 0"` wrapper, whose outer double quotes every shell reads as one word.
//! Paths use forward slashes: a bare `C:\x\a.exe` loses its backslashes
//! under bash, while Windows APIs accept `/`.

use std::path::Path;

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

/// The 8.3 form of an existing path, when the volume has one.
fn short_path(path: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    if !Path::new(path).exists() {
        return None;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n as usize]))
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
        HookShell::Auto => {
            if is_plain(&path) {
                return format!("{path} {args}");
            }
            if let Some(short) = short_path(a_bin).map(|s| forward(&s)) {
                if is_plain(&short) {
                    return format!("{short} {args}");
                }
            }
            powershell_wrapper(a_bin, args)
        }
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
