//! Windows: the generated hook command lines are executed through cmd.exe,
//! powershell.exe and (when Git for Windows is present) bash.exe, against a
//! real fake `a.exe` living in a directory with spaces, and the install /
//! check / uninstall round trip runs on Windows-style paths.

use super::*;
use crate::hooks::winshell::{command_line, powershell_wrapper, HookShell};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::OnceLock;

/// A real `a.exe` stand-in: appends its argv to `calls.log` beside itself
/// and exits with $FAKE_A_EXIT (default 1, so tolerance tails are visible).
fn fake_a_template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("aplexer-fake-a-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("fake.rs");
        fs::write(
            &src,
            r#"fn main() {
    use std::io::Write;
    let exe = std::env::current_exe().unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut f = std::fs::OpenOptions::new().create(true).append(true)
        .open(exe.with_file_name("calls.log")).unwrap();
    writeln!(f, "{}", args.join(" ")).unwrap();
    let code = std::env::var("FAKE_A_EXIT").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    std::process::exit(code);
}"#,
        )
        .unwrap();
        let out = dir.join("fake-a.exe");
        let status = Command::new("rustc")
            .arg(&src)
            .arg("-o")
            .arg(&out)
            .arg("-O")
            .status()
            .expect("rustc on PATH");
        assert!(status.success());
        out
    })
}

/// Copy the fake into `<tmp>/<dir_name>/a.exe`; returns (exe, log path, guard).
fn fake_a(dir_name: &str) -> (PathBuf, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("a.exe");
    fs::copy(fake_a_template(), &exe).unwrap();
    let log = dir.join("calls.log");
    (exe, log, tmp)
}

#[derive(Clone, Copy, Debug)]
enum Runner {
    Cmd,
    PowerShell,
    Bash,
}

fn git_bash() -> Option<PathBuf> {
    [
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn run_line(runner: Runner, line: &str, exit: &str) -> Option<i32> {
    let mut cmd = match runner {
        Runner::Cmd => {
            // What node/python `shell: true` do: /d /s /c "<line>".
            let mut c = Command::new("cmd.exe");
            c.args(["/d", "/s", "/c"]).raw_arg(format!("\"{line}\""));
            c
        }
        Runner::PowerShell => {
            let mut c = Command::new("powershell.exe");
            c.args(["-NoProfile", "-NonInteractive", "-Command", line]);
            c
        }
        Runner::Bash => {
            let mut c = Command::new(git_bash()?);
            c.args(["-c", line]);
            c
        }
    };
    let out = cmd.env("FAKE_A_EXIT", exit).output().expect("spawn shell");
    Some(out.status.code().expect("exit code"))
}

fn calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Run `line` under `runner` against a failing fake: the hook must have
/// reached the binary exactly once with the right argv and (when
/// `tolerant`) swallowed its failure.
fn assert_runs(runner: Runner, line: &str, log: &Path, tolerant: bool, want_args: &str) {
    let _ = fs::remove_file(log);
    let Some(code) = run_line(runner, line, "1") else {
        eprintln!("bash not found; skipping {runner:?}");
        return;
    };
    assert_eq!(
        calls(log),
        vec![want_args.to_string()],
        "{runner:?}: {line}"
    );
    if tolerant {
        assert_eq!(code, 0, "{runner:?} must tolerate failure: {line}");
    } else {
        assert_eq!(code, 1, "{runner:?}: {line}");
    }
    // And success stays success.
    let _ = fs::remove_file(log);
    assert_eq!(run_line(runner, line, "0"), Some(0), "{runner:?}: {line}");
}

const DIRS: [&str; 3] = ["plain", "Program Files x", "it's (x86) dir"];

#[test]
fn bash_dialect_runs_under_bash_with_spaced_paths() {
    for dir in DIRS {
        let (exe, log, _g) = fake_a(dir);
        let line = command_line(HookShell::Bash, exe.to_str().unwrap(), "state-report idle");
        assert!(line.ends_with("|| true"), "{line}");
        assert_runs(Runner::Bash, &line, &log, true, "state-report idle");
    }
}

#[test]
fn cmd_dialect_runs_under_cmd_and_bash_with_spaced_paths() {
    for dir in DIRS {
        let (exe, log, _g) = fake_a(dir);
        let line = command_line(
            HookShell::Cmd,
            exe.to_str().unwrap(),
            "state-report working",
        );
        assert!(line.ends_with("|| exit 0"), "{line}");
        assert_runs(Runner::Cmd, &line, &log, true, "state-report working");
        assert_runs(Runner::Bash, &line, &log, true, "state-report working");
    }
}

#[test]
fn powershell_dialect_runs_under_powershell_with_spaced_paths() {
    for dir in DIRS {
        let (exe, log, _g) = fake_a(dir);
        let line = command_line(
            HookShell::PowerShell,
            exe.to_str().unwrap(),
            "state-report waiting",
        );
        if dir.contains(' ') {
            assert!(line.starts_with("& \""), "{line}");
        }
        assert_runs(
            Runner::PowerShell,
            &line,
            &log,
            true,
            "state-report waiting",
        );
    }
}

#[test]
fn auto_dialect_runs_everywhere() {
    for dir in DIRS {
        let (exe, log, _g) = fake_a(dir);
        let line = command_line(HookShell::Auto, exe.to_str().unwrap(), "state-report idle");
        // Tail-less when the path needs no quoting (or has an 8.3 name).
        let tolerant = line.contains("exit 0");
        for runner in [Runner::Cmd, Runner::PowerShell, Runner::Bash] {
            assert_runs(runner, &line, &log, tolerant, "state-report idle");
        }
    }
}

#[test]
fn powershell_wrapper_runs_everywhere_whatever_the_path() {
    for dir in DIRS {
        let (exe, log, _g) = fake_a(dir);
        let line = powershell_wrapper(exe.to_str().unwrap(), "context hook --engine claude");
        assert!(reports_context(&line, "claude"), "{line}");
        for runner in [Runner::Cmd, Runner::PowerShell, Runner::Bash] {
            assert_runs(runner, &line, &log, true, "context hook --engine claude");
        }
    }
}

#[test]
fn every_generated_form_is_detected_as_ours() {
    for shell in [
        HookShell::Auto,
        HookShell::Bash,
        HookShell::Cmd,
        HookShell::PowerShell,
    ] {
        for bin in [
            r"C:\bin\a.exe",
            r"C:\Program Files\a b\a.exe",
            r"C:\it's\a.exe",
        ] {
            let report = command_line(shell, bin, "state-report idle");
            assert!(is_state_report_command(&report), "{report}");
            assert!(reports_state(&report, "idle"), "{report}");
            assert!(!reports_state(&report, "working"), "{report}");
            let ctx = command_line(shell, bin, "context hook --engine gemini");
            assert!(reports_context(&ctx, "gemini"), "{ctx}");
            assert!(!reports_context(&ctx, "codex"), "{ctx}");
            assert!(is_managed_hook_command(&ctx), "{ctx}");
        }
    }
}

#[test]
fn install_check_uninstall_round_trip_on_windows_paths() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("Some User");
    let a_bin = r"C:\Program Files\aplexer\a.exe";
    let targets = resolve_targets(&home, None, None, &[]);
    assert_eq!(
        targets.claude_settings,
        vec![home.join(".claude").join("settings.json")]
    );
    // Pre-existing foreign content in the shared settings files survives.
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude").join("settings.json"),
        "{\"permissions\": {\"allow\": [\"Bash(dir)\"]}}\n",
    )
    .unwrap();

    let first = install(&targets, a_bin, None);
    assert!(first.iter().all(|s| s.installed), "{first:?}");
    assert!(check(&targets, None).iter().all(|s| s.installed));
    let paths = [
        home.join(".claude").join("settings.json"),
        home.join(".codex").join("hooks.json"),
        home.join(".grok").join("hooks").join("aplexer.json"),
        home.join(".gemini").join("settings.json"),
        home.join(".gemini").join("config").join("hooks.json"),
        home.join(".config")
            .join("opencode")
            .join("plugin")
            .join(OPENCODE_PLUGIN_FILENAME),
    ];
    let snapshot: Vec<String> = paths
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .collect();
    for text in &snapshot[..5] {
        assert!(!text.contains("|| true"), "{text}");
        assert!(text.contains("state-report"), "{text}");
    }
    // Idempotent: a second install rewrites nothing.
    let second = install(&targets, a_bin, None);
    assert!(second.iter().all(|s| s.installed));
    for (p, before) in paths.iter().zip(&snapshot) {
        assert_eq!(&fs::read_to_string(p).unwrap(), before, "{}", p.display());
    }

    let removed = uninstall(&targets, None);
    assert!(removed.iter().all(|s| s.action != "error"), "{removed:?}");
    assert!(check(&targets, None).iter().all(|s| !s.installed));
    let claude: Value = serde_json::from_str(&fs::read_to_string(&paths[0]).unwrap()).unwrap();
    assert_eq!(claude, json!({"permissions": {"allow": ["Bash(dir)"]}}));
    for left in &paths[1..5] {
        let text = fs::read_to_string(left).unwrap_or_default();
        assert!(
            !text.contains("state-report"),
            "{} kept hooks",
            left.display()
        );
    }
    assert!(!paths[5].exists());
}

#[test]
fn hooks_written_to_disk_actually_run() {
    // End to end: install into settings.json with the real fake binary
    // (spaced dir), read a command back out of the file, execute it.
    let (exe, log, _g) = fake_a("Program Files y");
    let home = tempfile::TempDir::new().unwrap();
    let targets = resolve_targets(home.path(), None, None, &[]);
    let a_bin = exe.to_str().unwrap();
    assert!(install(&targets, a_bin, Some("claude"))[0].installed);
    let doc: Value =
        serde_json::from_str(&fs::read_to_string(&targets.claude_settings[0]).unwrap()).unwrap();
    let command = doc["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    for runner in [Runner::Cmd, Runner::PowerShell, Runner::Bash] {
        assert_runs(
            runner,
            command,
            &log,
            command.contains("exit 0"),
            "state-report gated-idle",
        );
    }
}
