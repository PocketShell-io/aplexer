//! Program resolution and safe command-line construction for Windows
//! launches. One resolver serves the launcher (`pty::spawn_workload`,
//! `pty::spawn_detached_worker`), the `a start` pre-flight
//! (`process::executable_available`) and `a doctor` (`config::pathfix`), so a
//! program that `a doctor` calls resolvable is exactly one the launcher can run.
//!
//! # Seam API
//! * [`resolve`] / [`resolve_in_process_env`]: program name -> [`Resolved`]
//!   (`path` + [`LaunchKind`]). Only `PATHEXT` extensions (restricted to the
//!   launchable set `.com .exe .bat .cmd .ps1`) are accepted. The extensionless
//!   file is never a candidate: in an npm global bin dir it is a `sh` shim that
//!   `CreateProcessW` rejects with ERROR_BAD_EXE_FORMAT (193).
//! * [`plan`]: `(argv, Resolved, env)` -> [`LaunchPlan`] (application name and
//!   command line for `CreateProcessW`). `.cmd`/`.bat` shims run as
//!   `cmd.exe /d /s /c ""shim" "arg" ..."` with cmd-safe quoting (the
//!   BatBadBut class, CVE-2024-24576); `.ps1` runs through `pwsh`/`powershell`
//!   `-NoProfile -File`.
//! * [`normalize_cwd`], [`validate_env`]: cheap input hardening.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// How a resolved program has to be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    /// `.exe` / `.com`: straight `CreateProcessW`.
    Exe,
    /// `.cmd` / `.bat`: through `cmd.exe /d /s /c`.
    CmdShim,
    /// `.ps1`: through `pwsh` / `powershell -NoProfile -File`.
    Ps1Shim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub kind: LaunchKind,
}

/// `CreateProcessW` command lines are limited to 32767 UTF-16 units including
/// the terminating NUL.
pub const MAX_COMMAND_LINE: usize = 32767;
/// `cmd.exe` refuses command lines longer than 8191 characters.
pub const MAX_CMD_LINE: usize = 8191;

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

fn ext_kind(ext: &str) -> Option<LaunchKind> {
    match ext {
        "exe" | "com" => Some(LaunchKind::Exe),
        "cmd" | "bat" => Some(LaunchKind::CmdShim),
        "ps1" => Some(LaunchKind::Ps1Shim),
        _ => None,
    }
}

fn path_kind(path: &Path) -> Option<LaunchKind> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    ext_kind(&ext)
}

/// `PATHEXT` entries (trimmed, lowercase, without the dot), in order, kept
/// only when launchable. `None`/blank means the default list.
pub fn parse_pathext(raw: Option<&OsStr>) -> Vec<String> {
    let raw = raw
        .map(|r| r.to_string_lossy().into_owned())
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PATHEXT.to_string());
    let mut out: Vec<String> = Vec::new();
    for e in raw.split(';') {
        let e = e.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext_kind(&e).is_some() && !out.contains(&e) {
            out.push(e);
        }
    }
    // `.ps1` ranks below every other extension: npm installs `.cmd` and
    // `.ps1` shims side by side, and the `.ps1` one trips over a Restricted
    // PowerShell execution policy, so it is a last resort.
    out.sort_by_key(|e| e == "ps1");
    out
}

fn has_separator(program: &OsStr) -> bool {
    program
        .to_string_lossy()
        .chars()
        .any(|c| c == '\\' || c == '/' || c == ':')
}

fn probe(base: &Path, exts: &[String]) -> Option<Resolved> {
    // An explicit launchable extension wins as-is.
    if let Some(kind) = path_kind(base) {
        if base.is_file() {
            return Some(Resolved {
                path: base.to_path_buf(),
                kind,
            });
        }
    }
    // Otherwise append each PATHEXT extension. The bare name itself is never
    // a candidate.
    for e in exts {
        let mut s = base.as_os_str().to_os_string();
        s.push(".");
        s.push(e);
        let cand = PathBuf::from(s);
        if cand.is_file() {
            return Some(Resolved {
                kind: ext_kind(e)?,
                path: cand,
            });
        }
    }
    // Last resort in this directory: a lone `.ps1` shim (what PowerShell
    // itself would run for the bare name), even when PATHEXT omits it.
    if !exts.iter().any(|e| e == "ps1") {
        let mut s = base.as_os_str().to_os_string();
        s.push(".ps1");
        let cand = PathBuf::from(s);
        if cand.is_file() {
            return Some(Resolved {
                kind: LaunchKind::Ps1Shim,
                path: cand,
            });
        }
    }
    None
}

/// Resolve `program` like `CreateProcess`/a shell would, but accepting only
/// launchable files. Names with a separator or drive are used directly
/// (relative ones are joined onto `cwd`); bare names are searched through
/// `path_var` (the current directory is not searched).
pub fn resolve(
    program: &OsStr,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
    cwd: Option<&Path>,
) -> Option<Resolved> {
    let exts = parse_pathext(pathext);
    let p = Path::new(program);
    if has_separator(program) {
        let full = match cwd {
            Some(cwd) if p.is_relative() => cwd.join(p),
            _ => p.to_path_buf(),
        };
        return probe(&full, &exts);
    }
    for dir in std::env::split_paths(path_var?) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if let Some(r) = probe(&dir.join(p), &exts) {
            return Some(r);
        }
    }
    None
}

/// [`resolve`] against this process's own `PATH`/`PATHEXT` and cwd.
pub fn resolve_in_process_env(program: &str) -> Option<Resolved> {
    resolve(
        OsStr::new(program),
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
        None,
    )
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Strip a `\\?\` (or `\\?\UNC\`) prefix and require `cwd` to be a directory.
pub fn normalize_cwd(cwd: &Path) -> io::Result<PathBuf> {
    let s = cwd.as_os_str().to_string_lossy();
    let stripped: PathBuf = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        cwd.to_path_buf()
    };
    if !stripped.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "working directory is not a directory: {}",
                stripped.display()
            ),
        ));
    }
    Ok(stripped)
}

/// Reject environment entries Windows cannot represent.
pub fn validate_env(vars: &[(OsString, OsString)]) -> io::Result<()> {
    for (k, v) in vars {
        if k.encode_wide().any(|c| c == 0) || v.encode_wide().any(|c| c == 0) {
            return Err(invalid(format!(
                "environment variable {} contains a NUL character",
                k.to_string_lossy()
            )));
        }
        if k.is_empty() || k.encode_wide().skip(1).any(|c| c == b'=' as u16) {
            return Err(invalid(format!(
                "invalid environment variable name: {}",
                k.to_string_lossy()
            )));
        }
    }
    Ok(())
}

/// What to hand to `CreateProcessW`.
#[derive(Debug)]
pub struct LaunchPlan {
    /// `lpApplicationName`.
    pub application: PathBuf,
    /// `lpCommandLine`, NUL-terminated.
    pub cmdline: Vec<u16>,
}

fn append_crt_arg(out: &mut Vec<u16>, wide: &[u16]) {
    let quote = wide.is_empty()
        || wide
            .iter()
            .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'\n' as u16 || c == 0x0b);
    if quote {
        out.push(b'"' as u16);
    }
    let mut backslashes = 0usize;
    for &c in wide {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else {
            if c == b'"' as u16 {
                out.extend(std::iter::repeat_n(b'\\' as u16, backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    if quote {
        out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        out.push(b'"' as u16);
    }
}

fn check_arg(arg: &OsStr) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(invalid(format!(
            "argument contains a NUL character: {}",
            arg.to_string_lossy().replace('\0', "\\0")
        )));
    }
    Ok(wide)
}

/// CommandLineToArgvW-compatible command line from argv (NUL-terminated).
/// Errors on NUL in an argument and on lines over [`MAX_COMMAND_LINE`].
pub fn crt_command_line<S: AsRef<OsStr>>(argv: &[S]) -> io::Result<Vec<u16>> {
    let mut out = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            out.push(b' ' as u16);
        }
        append_crt_arg(&mut out, &check_arg(a.as_ref())?);
    }
    finish(out)
}

fn finish(mut line: Vec<u16>) -> io::Result<Vec<u16>> {
    if line.len() + 1 > MAX_COMMAND_LINE {
        return Err(invalid(format!(
            "command line is too long ({} characters; Windows allows {})",
            line.len(),
            MAX_COMMAND_LINE - 1
        )));
    }
    line.push(0);
    Ok(line)
}

/// Append one argument for a batch file, always quoted: inside quotes cmd.exe
/// (and the batch file's later `%*` expansion) treats `& | < > ^ ( )` as
/// literal, `"` is doubled (a CRT program reads `""` inside quotes as one
/// quote) and `%` becomes `%%cd:~,%` so no `%VAR%` is expanded. Backslashes are
/// doubled only before a quote (an internal one or the closing one).
fn append_cmd_arg(out: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
    let wide = check_arg(arg)?;
    if wide.iter().any(|&c| c == b'\r' as u16 || c == b'\n' as u16) {
        return Err(invalid(format!(
            "argument contains a line break, which cmd.exe cannot pass safely to a batch file: {:?}",
            arg.to_string_lossy()
        )));
    }
    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for &c in &wide {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else {
            if c == b'"' as u16 {
                out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                out.push(b'"' as u16);
            } else if c == b'%' as u16 {
                out.extend("%%cd:~,".encode_utf16());
            }
            backslashes = 0;
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
    out.push(b'"' as u16);
    Ok(())
}

fn trusted_cmd_exe() -> PathBuf {
    if let Some(c) = std::env::var_os("COMSPEC").map(PathBuf::from) {
        if c.is_file() {
            return c;
        }
    }
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("cmd.exe")
}

/// `cmd.exe /d /s /c ""<shim>" "arg" ..."`.
pub fn cmd_shim_command_line(cmd_exe: &Path, shim: &Path, args: &[&OsStr]) -> io::Result<Vec<u16>> {
    let shim_w: Vec<u16> = shim.as_os_str().encode_wide().collect();
    if shim_w.iter().any(|&c| {
        c == 0 || c == b'"' as u16 || c == b'%' as u16 || c == b'\r' as u16 || c == b'\n' as u16
    }) {
        return Err(invalid(format!(
            "cannot run batch file with a %, \" or line break in its path: {}",
            shim.display()
        )));
    }
    let mut out: Vec<u16> = Vec::new();
    append_crt_arg(
        &mut out,
        &cmd_exe.as_os_str().encode_wide().collect::<Vec<_>>(),
    );
    out.extend(" /d /s /c \"".encode_utf16());
    out.push(b'"' as u16);
    out.extend(&shim_w);
    out.push(b'"' as u16);
    let inner_start = out.len();
    for a in args {
        out.push(b' ' as u16);
        append_cmd_arg(&mut out, a)?;
    }
    out.push(b'"' as u16);
    if out.len() - inner_start > MAX_CMD_LINE {
        return Err(invalid(format!(
            "batch file command line is too long for cmd.exe ({} characters; limit {})",
            out.len() - inner_start,
            MAX_CMD_LINE
        )));
    }
    finish(out)
}

fn powershell_for(vars: &[(OsString, OsString)]) -> PathBuf {
    let env = |name: &str| {
        vars.iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let path = env("PATH");
    let pathext = env("PATHEXT");
    for name in ["pwsh", "powershell"] {
        if let Some(r) = resolve(OsStr::new(name), path.as_deref(), pathext.as_deref(), None) {
            if r.kind == LaunchKind::Exe {
                return r.path;
            }
        }
    }
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
}

/// Build the `CreateProcessW` inputs for launching `argv` as `resolved`.
/// `vars` is the effective workload environment (PATH lookup for `pwsh`).
pub fn plan<S: AsRef<OsStr>>(
    argv: &[S],
    resolved: &Resolved,
    vars: &[(OsString, OsString)],
) -> io::Result<LaunchPlan> {
    match resolved.kind {
        LaunchKind::Exe => Ok(LaunchPlan {
            application: resolved.path.clone(),
            cmdline: crt_command_line(argv)?,
        }),
        LaunchKind::CmdShim => {
            let cmd = trusted_cmd_exe();
            let args: Vec<&OsStr> = argv.iter().skip(1).map(|a| a.as_ref()).collect();
            Ok(LaunchPlan {
                cmdline: cmd_shim_command_line(&cmd, &resolved.path, &args)?,
                application: cmd,
            })
        }
        LaunchKind::Ps1Shim => {
            let ps = powershell_for(vars);
            let mut line: Vec<OsString> = vec![
                ps.clone().into_os_string(),
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                resolved.path.clone().into_os_string(),
            ];
            line.extend(argv.iter().skip(1).map(|a| a.as_ref().to_os_string()));
            Ok(LaunchPlan {
                cmdline: crt_command_line(&line)?,
                application: ps,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn shim_dir() -> tempfile::TempDir {
        let d = tempfile::TempDir::new().unwrap();
        fs::write(d.path().join("tool"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(d.path().join("tool.cmd"), "@echo off\r\n").unwrap();
        fs::write(d.path().join("tool.exe"), b"MZ").unwrap();
        d
    }

    fn r(name: &str, dir: &tempfile::TempDir, pathext: Option<&str>) -> Option<Resolved> {
        resolve(
            OsStr::new(name),
            Some(dir.path().as_os_str()),
            pathext.map(OsStr::new),
            None,
        )
    }

    #[test]
    fn extensionless_sh_shim_is_never_chosen() {
        let d = tempfile::TempDir::new().unwrap();
        fs::write(d.path().join("claude"), "#!/bin/sh\n").unwrap();
        assert_eq!(r("claude", &d, None), None);
        fs::write(d.path().join("claude.cmd"), "@echo off\r\n").unwrap();
        let got = r("claude", &d, None).unwrap();
        assert_eq!(got.path, d.path().join("claude.cmd"));
        assert_eq!(got.kind, LaunchKind::CmdShim);
        // Naming the extensionless file explicitly resolves to the sibling too.
        let explicit = resolve(d.path().join("claude").as_os_str(), None, None, None).unwrap();
        assert_eq!(explicit.path, d.path().join("claude.cmd"));
    }

    #[test]
    fn pathext_order_and_normalisation() {
        let d = shim_dir();
        assert_eq!(r("tool", &d, None).unwrap().kind, LaunchKind::Exe);
        // Default order puts .EXE before .CMD.
        assert_eq!(
            r("tool", &d, Some(".cmd;.exe")).unwrap().path,
            d.path().join("tool.cmd")
        );
        // Whitespace, case, empty entries and unlaunchable ones are tolerated.
        assert_eq!(
            r("tool", &d, Some(" .CMD ;;.VBS;.JS ; .EXE")).unwrap().path,
            d.path().join("tool.cmd")
        );
        // Only unlaunchable entries: nothing resolves (not even the sh shim).
        assert_eq!(r("tool", &d, Some(".VBS;.JS")), None);
        // Blank PATHEXT falls back to the default.
        assert_eq!(r("tool", &d, Some("  ")).unwrap().kind, LaunchKind::Exe);
        assert_eq!(
            parse_pathext(Some(OsStr::new(".Cmd; .EXE;.cmd"))),
            ["cmd", "exe"]
        );
    }

    #[test]
    fn explicit_extensions_and_ps1() {
        let d = shim_dir();
        fs::write(d.path().join("s.ps1"), "exit 0\r\n").unwrap();
        assert_eq!(r("tool.cmd", &d, None).unwrap().kind, LaunchKind::CmdShim);
        assert_eq!(r("tool.exe", &d, None).unwrap().kind, LaunchKind::Exe);
        assert_eq!(r("s.ps1", &d, None).unwrap().kind, LaunchKind::Ps1Shim);
        // A lone .ps1 is the last resort for a bare name.
        assert_eq!(r("s", &d, None).unwrap().kind, LaunchKind::Ps1Shim);
        assert_eq!(r("s", &d, Some(".PS1")).unwrap().kind, LaunchKind::Ps1Shim);
        assert_eq!(r("missing", &d, None), None);
    }

    #[test]
    fn cmd_shim_beats_ps1_even_when_pathext_lists_ps1_first() {
        let d = tempfile::TempDir::new().unwrap();
        for n in ["codex", "codex.cmd", "codex.ps1"] {
            fs::write(d.path().join(n), "x").unwrap();
        }
        for pe in [None, Some(".PS1;.CMD"), Some(".CMD;.PS1")] {
            let got = r("codex", &d, pe).unwrap();
            assert_eq!(got.path, d.path().join("codex.cmd"), "{pe:?}");
            assert_eq!(got.kind, LaunchKind::CmdShim);
        }
        // An exe in the same dir beats both.
        fs::write(d.path().join("codex.exe"), b"MZ").unwrap();
        assert_eq!(
            r("codex", &d, Some(".PS1;.EXE")).unwrap().kind,
            LaunchKind::Exe
        );
    }

    #[test]
    fn relative_programs_join_onto_cwd() {
        let d = shim_dir();
        let rel = Path::new(".").join("tool");
        let got = resolve(rel.as_os_str(), None, None, Some(d.path())).unwrap();
        assert_eq!(got.path, d.path().join(".").join("tool.exe"));
        assert_eq!(resolve(rel.as_os_str(), None, None, None), None);
    }

    #[test]
    fn cwd_normalisation() {
        let d = tempfile::TempDir::new().unwrap();
        let verbatim = PathBuf::from(format!(r"\\?\{}", d.path().display()));
        let got = normalize_cwd(&verbatim).unwrap();
        assert!(!got.to_string_lossy().starts_with(r"\\?\"));
        assert!(got.is_dir());
        let file = d.path().join("f");
        fs::write(&file, "x").unwrap();
        let e = normalize_cwd(&file).unwrap_err();
        assert!(e.to_string().contains("not a directory"), "{e}");
    }

    #[test]
    fn rejects_nul_newline_and_overlong() {
        assert!(crt_command_line(&["a\0b"]).is_err());
        let long = "x".repeat(MAX_COMMAND_LINE);
        let e = crt_command_line(&[long.as_str()]).unwrap_err();
        assert!(e.to_string().contains("too long"), "{e}");
        let d = shim_dir();
        let shim = Resolved {
            path: d.path().join("tool.cmd"),
            kind: LaunchKind::CmdShim,
        };
        for bad in ["a\nb", "a\rb", "a\0b"] {
            assert!(plan(&["tool", bad], &shim, &[]).is_err(), "{bad:?}");
        }
        assert!(validate_env(&[("A".into(), "x\0".into())]).is_err());
        assert!(validate_env(&[("A=B".into(), "x".into())]).is_err());
        assert!(validate_env(&[("A".into(), "x".into())]).is_ok());
    }

    #[test]
    fn cmd_shim_has_the_expected_shape() {
        let line = cmd_shim_command_line(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            Path::new(r"C:\npm\claude.cmd"),
            &[OsStr::new("--resume"), OsStr::new(r#"a&b "c""#)],
        )
        .unwrap();
        let s = String::from_utf16(&line[..line.len() - 1]).unwrap();
        assert_eq!(
            s,
            r#"C:\Windows\System32\cmd.exe /d /s /c ""C:\npm\claude.cmd" "--resume" "a&b ""c"""""#
        );
    }
}
