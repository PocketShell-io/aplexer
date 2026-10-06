//! Cross-platform workload helpers for the integration tests.
//!
//! Include with `#[path = "support/workload.rs"] mod workload;`. Everything
//! here yields the *same observable behaviour* on Unix and Windows so a test
//! body does not need `cfg` branches: argv vectors for a portable shell
//! workload, sandboxed home/profile environment, fake engine executables,
//! a foreign (unrelated) sleeper process, a liveness probe, and wall-clock
//! formatting without a `chrono` dev-dependency.
//!
//! Unix: `/bin/sh -c <script>`.  Windows: `cmd.exe /c <script>` (or
//! `powershell.exe` where unset-vs-empty environment reporting is needed).
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub const IS_WINDOWS: bool = cfg!(windows);

/// The platform's basic interactive shell executable.
pub fn shell_program() -> &'static str {
    if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/sh"
    }
}

/// A bash-like interactive shell argv for sessions that are typed into.
/// Unix: `/bin/bash --norc -i`. Windows: `cmd.exe`.
pub fn interactive_shell_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".into()]
    } else {
        vec!["/bin/bash".into(), "--norc".into(), "-i".into()]
    }
}

/// Login shell argv recorded in synthetic session records.
/// Unix: `/bin/bash -l`. Windows: `cmd.exe`.
pub fn login_shell_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".into()]
    } else {
        vec!["/bin/bash".into(), "-l".into()]
    }
}

/// TOML inline-array literal for an engine whose command is the platform's
/// login-ish shell (used as `command = <this>` in `[engines.shell]`).
pub fn shell_engine_toml() -> &'static str {
    if cfg!(windows) {
        "[\"cmd.exe\"]"
    } else {
        "[\"/bin/sh\", \"-l\"]"
    }
}

/// argv running `script` through the platform shell (`sh -c` / `cmd /c`).
pub fn shell_command(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".into(), "/c".into(), script.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }
}

/// argv for a program that exits 0 immediately.
pub fn true_command() -> Vec<String> {
    if cfg!(windows) {
        shell_command("exit 0")
    } else {
        vec!["/bin/true".into()]
    }
}

/// argv for a program that exits with `code` immediately.
pub fn exit_command(code: i32) -> Vec<String> {
    shell_command(&format!("exit {code}"))
}

/// Shell script text that sleeps `secs` seconds. Windows has no `sleep` and
/// `timeout` needs a console on stdin, so use a loopback `ping`.
pub fn sleep_script(secs: u32) -> String {
    if cfg!(windows) {
        format!("ping -n {} 127.0.0.1 >nul", secs + 1)
    } else {
        format!("sleep {secs}")
    }
}

/// argv for a workload that just sleeps `secs` seconds.
pub fn sleep_command(secs: u32) -> Vec<String> {
    shell_command(&sleep_script(secs))
}

/// argv that prints `text` on its own line, then sleeps `secs` seconds (so
/// the session stays alive for capture). `text` must be plain (no quotes,
/// `%`, `^`, `&`, `|`).
pub fn echo_then_sleep(text: &str, secs: u32) -> Vec<String> {
    if cfg!(windows) {
        shell_command(&format!("echo {text} & {}", sleep_script(secs)))
    } else {
        shell_command(&format!("echo '{text}'; {}", sleep_script(secs)))
    }
}

/// argv that prints one `label=value` (or `label=[value]` when `bracket`)
/// per `(label, ENV_VAR)` pair, space separated on one line, with `unset`
/// for variables absent from the workload's environment, then sleeps.
///
/// Reports exactly the shape the original `printf '...%s...' "${VAR-unset}"`
/// scripts did, so assertions like `shell-api=visible shell-remove=unset`
/// are portable.
pub fn env_report_command(items: &[(&str, &str)], bracket: bool, sleep_secs: u32) -> Vec<String> {
    if cfg!(windows) {
        let mut parts = Vec::new();
        for (label, var) in items {
            let (open, close) = if bracket { ("[", "]") } else { ("", "") };
            parts.push(format!("'{label}={open}' + (g '{var}') + '{close}'"));
        }
        let script = format!(
            "function g($n){{ $v=[Environment]::GetEnvironmentVariable($n); \
             if($null -eq $v -or $v -eq ''){{'unset'}}else{{$v}} }}; \
             Write-Output ({}); Start-Sleep {sleep_secs}",
            parts.join(" + ' ' + ")
        );
        vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ]
    } else {
        let fmt = items
            .iter()
            .map(|(label, _)| {
                if bracket {
                    format!("{label}=[%s]")
                } else {
                    format!("{label}=%s")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let args = items
            .iter()
            .map(|(_, var)| format!("\"${{{var}-unset}}\""))
            .collect::<Vec<_>>()
            .join(" ");
        shell_command(&format!("printf '{fmt}\\n' {args}; sleep {sleep_secs}"))
    }
}

/// How long to poll `capture` for a marker: Windows shells (and ConPTY
/// start-up) are noticeably slower than `/bin/sh`.
pub fn capture_deadline() -> Duration {
    Duration::from_secs(if cfg!(windows) { 20 } else { 5 })
}

/// Point a child's home/profile at `home` so it never reads the real user's
/// files. Unix: `HOME`. Windows: `USERPROFILE`, `HOME`, `LOCALAPPDATA`,
/// `APPDATA` (the latter two under `home/AppData`).
pub fn sandbox_home(command: &mut Command, home: &Path) {
    command.env("HOME", home);
    if cfg!(windows) {
        let local = home.join("AppData").join("Local");
        let roaming = home.join("AppData").join("Roaming");
        let _ = fs::create_dir_all(&local);
        let _ = fs::create_dir_all(&roaming);
        command
            .env("USERPROFILE", home)
            .env("LOCALAPPDATA", local)
            .env("APPDATA", roaming);
    }
}

/// Escape a filesystem path for use inside a TOML basic string (`"..."`).
pub fn toml_path(path: &str) -> String {
    path.replace('\\', "\\\\")
}

/// Directory-name encoding Claude Code uses for `~/.claude/projects/<dir>`.
pub fn claude_project_dir_name(cwd: &Path) -> String {
    let text = cwd.display().to_string();
    if cfg!(windows) {
        text.replace(['/', '\\', ':', '.'], "-")
    } else {
        text.replace(['/', '.'], "-")
    }
}

/// Fake engine body that prints `argv[i]=<arg>` for each argument and exits
/// with `$FAKE_EXIT` (default 0). With `full`, also prints `cwd=`,
/// `session=`, `stripped=` (`$MY_CUSTOM_STRIP`) and `kept=` (`$KEEP_ME`),
/// each defaulting to `none`.
pub fn fake_engine_script(full: bool) -> String {
    if cfg!(windows) {
        let mut s = String::from(
            "@echo off\r\nsetlocal enabledelayedexpansion\r\nset i=0\r\n:loop\r\n\
             if \"%~1\"==\"\" goto done\r\necho argv[!i!]=%~1\r\nset /a i+=1\r\nshift\r\n\
             goto loop\r\n:done\r\n",
        );
        if full {
            s.push_str("echo cwd=%CD%\r\n");
            for (label, var) in [
                ("session", "APLEXER_SESSION_ID"),
                ("stripped", "MY_CUSTOM_STRIP"),
                ("kept", "KEEP_ME"),
            ] {
                s.push_str(&format!(
                    "if defined {var} (echo {label}=%{var}%) else (echo {label}=none)\r\n"
                ));
            }
        }
        s.push_str("if defined FAKE_EXIT (exit /b %FAKE_EXIT%)\r\nexit /b 0\r\n");
        s
    } else {
        let mut s = String::from(
            "#!/bin/sh\ni=0\nfor a in \"$@\"; do\n  echo \"argv[$i]=$a\"\n  i=$((i+1))\ndone\n",
        );
        if full {
            s.push_str(
                "echo \"cwd=$PWD\"\necho \"session=${APLEXER_SESSION_ID:-none}\"\n\
                 echo \"stripped=${MY_CUSTOM_STRIP:-none}\"\necho \"kept=${KEEP_ME:-none}\"\n",
            );
        }
        s.push_str("exit ${FAKE_EXIT:-0}\n");
        s
    }
}

/// Fake engine body that ignores its arguments and sleeps `secs` seconds.
pub fn sleeping_engine_script(secs: u32) -> String {
    if cfg!(windows) {
        format!("@echo off\r\n{}\r\nexit /b 0\r\n", sleep_script(secs))
    } else {
        format!("#!/bin/sh\nsleep {secs}\nexit 0\n")
    }
}

/// Write `body` as an executable named `name` in `dir` (Windows: `<name>.cmd`)
/// and return its absolute path as a string.
pub fn write_executable(dir: &Path, name: &str, body: &str) -> String {
    let file = if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    };
    let path = dir.join(file);
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    path.to_str().unwrap().to_string()
}

/// Spawn an unrelated long-running process (the "foreign" bystander that a
/// kill/disable must never touch).
pub fn spawn_foreign_sleeper(secs: u32) -> Child {
    if cfg!(windows) {
        Command::new("ping")
            .args(["-n", &(secs + 1).to_string(), "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn foreign ping")
    } else {
        Command::new("sleep")
            .arg(secs.to_string())
            .spawn()
            .expect("spawn foreign sleep")
    }
}

/// Whether `pid` is a live process (Unix: `kill -0`; Windows: `tasklist`).
pub fn process_alive(pid: u32) -> bool {
    if cfg!(windows) {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("run tasklist");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .expect("probe process")
            .success()
    }
}

/// Forcefully terminate `pid` (best effort).
pub fn kill_process(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    } else {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// Wall-clock formats for schedule/cutoff tests.
pub enum DateFmt {
    /// Local `HH:MM`.
    Hhmm,
    /// Local time with explicit offset, RFC 3339 seconds precision.
    Iso8601Seconds,
    /// Local time without an offset, `YYYY-MM-DDTHH:MM:SS`.
    NaiveIso,
}

/// Local wall-clock time shifted by `minutes` (negative = past), formatted.
/// Unix uses `date -d`; Windows uses PowerShell `Get-Date`.
pub fn date_shifted(minutes: i64, fmt: DateFmt) -> String {
    if cfg!(windows) {
        let pattern = match fmt {
            DateFmt::Hhmm => "HH:mm",
            DateFmt::Iso8601Seconds => "yyyy-MM-ddTHH:mm:sszzz",
            DateFmt::NaiveIso => "yyyy-MM-ddTHH:mm:ss",
        };
        let script = format!("(Get-Date).AddMinutes({minutes}).ToString('{pattern}')");
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .expect("run powershell Get-Date");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        let offset = format!("{minutes} minutes");
        let args: Vec<&str> = match fmt {
            DateFmt::Hhmm => vec!["+%H:%M", "-d", &offset],
            DateFmt::Iso8601Seconds => vec!["--iso-8601=seconds", "-d", &offset],
            DateFmt::NaiveIso => vec!["-d", &offset, "+%Y-%m-%dT%H:%M:%S"],
        };
        let output = Command::new("date").args(args).output().unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}
