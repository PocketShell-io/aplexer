//! Which program the built-in `shell` engine runs.
//!
//! Precedence, highest first:
//!
//! 1. `APLEXER_SHELL` in the environment;
//! 2. an explicit `[engines.shell]` table in the config file (the whole
//!    engine definition, as it always was);
//! 3. the top-level `shell = ...` config key;
//! 4. the platform default: Unix `$SHELL -l`; Windows Git for Windows
//!    `bash --login -i`, then `pwsh.exe`, `powershell.exe`, `%COMSPEC%`.
//!
//! A value is a name (`bash`, `pwsh`, `powershell`, `cmd`), an absolute
//! path, or -- with the array form of the config key -- an explicit argv.
//! A value that cannot be found is an error naming the setting; it never
//! falls back to another shell.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::pathfix::which_in;

/// The `shell` config key: `"bash"`, `"C:\\Program Files\\Git\\bin\\bash.exe"`
/// or `["bash.exe", "--login", "-i"]` (explicit argv, used verbatim after
/// resolving element 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ShellSetting {
    Line(String),
    Argv(Vec<String>),
}

/// Why the shell was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSource {
    Env,
    ConfigEngine,
    ConfigKey,
    Default,
}

impl ShellSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellSource::Env => "env APLEXER_SHELL",
            ShellSource::ConfigEngine => "config [engines.shell]",
            ShellSource::ConfigKey => "config key `shell`",
            ShellSource::Default => "default order",
        }
    }
}

/// The resolved shell.
#[derive(Debug, Clone)]
pub struct ShellSelection {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub source: ShellSource,
    /// Human-readable account for `a doctor`: what was picked and what was
    /// skipped on the way.
    pub notes: Vec<String>,
}

/// Everything resolution reads from the machine, injectable for tests.
#[derive(Debug, Clone, Default)]
pub struct ShellContext {
    pub path: String,
    /// `%ProgramFiles%`, `%ProgramW6432%`, `%ProgramFiles(x86)%`.
    pub program_files: Vec<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub comspec: Option<String>,
    /// Unix `$SHELL`.
    pub login_shell: Option<String>,
}

impl ShellContext {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let mut program_files: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
            .iter()
            .filter_map(|name| var(name))
            .map(PathBuf::from)
            .collect();
        program_files.dedup();
        ShellContext {
            path: std::env::var("PATH").unwrap_or_default(),
            program_files,
            local_app_data: var("LOCALAPPDATA").map(PathBuf::from),
            comspec: var("COMSPEC"),
            login_shell: var("SHELL"),
        }
    }
}

/// The bash to use for helper probes (`~/.bashrc` alias launch commands):
/// on Windows the resolved Git-for-Windows bash (never the WSL launcher), on
/// Unix plain `bash` when it is on PATH.
pub fn bash_program() -> Option<String> {
    let ctx = ShellContext::from_env();
    if cfg!(windows) {
        find_bash_windows(&ctx, &mut Vec::new()).map(|p| p.to_string_lossy().into_owned())
    } else {
        which_in("bash", &ctx.path).map(|_| "bash".to_string())
    }
}

const POWERSHELL_POLICIES: [&str; 4] = ["bypass", "remotesigned", "unrestricted", "allsigned"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bash,
    PosixShell,
    PowerShell,
    Cmd,
    Other,
}

fn kind_of(program: &Path) -> Kind {
    let stem = program
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match stem.as_str() {
        "bash" => Kind::Bash,
        "sh" | "zsh" | "fish" | "dash" | "ksh" => Kind::PosixShell,
        "pwsh" | "powershell" => Kind::PowerShell,
        "cmd" => Kind::Cmd,
        _ => Kind::Other,
    }
}

/// The program is the WSL launcher (or another Store alias): it would start
/// a Linux distro instead of a Windows shell.
fn is_wsl_launcher(path: &Path) -> bool {
    path.ancestors().skip(1).any(|dir| {
        dir.file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .is_some_and(|n| {
                matches!(
                    n.as_str(),
                    "system32" | "sysnative" | "syswow64" | "windowsapps"
                )
            })
    })
}

fn find_bash_windows(ctx: &ShellContext, notes: &mut Vec<String>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for pf in &ctx.program_files {
        candidates.push(pf.join(r"Git\bin\bash.exe"));
        candidates.push(pf.join(r"Git\usr\bin\bash.exe"));
    }
    if let Some(local) = &ctx.local_app_data {
        candidates.push(local.join(r"Programs\Git\bin\bash.exe"));
        candidates.push(local.join(r"Programs\Git\usr\bin\bash.exe"));
    }
    if let Some(git) = which_in("git", &ctx.path) {
        // <root>\cmd\git.exe, <root>\bin\git.exe, <root>\mingw64\bin\git.exe
        for root in git.ancestors().skip(1).take(3) {
            candidates.push(root.join(r"bin\bash.exe"));
            candidates.push(root.join(r"usr\bin\bash.exe"));
        }
    }
    for candidate in candidates {
        if candidate.is_file() && !is_wsl_launcher(&candidate) {
            return Some(candidate);
        }
    }
    // Any other bash.exe on PATH (MSYS2, Cygwin), but never the WSL launcher.
    for dir in std::env::split_paths(&ctx.path) {
        let candidate = dir.join("bash.exe");
        if !candidate.is_file() {
            continue;
        }
        if is_wsl_launcher(&candidate) {
            notes.push(format!(
                "skipped {} (WSL launcher, would start WSL)",
                candidate.display()
            ));
            continue;
        }
        return Some(candidate);
    }
    None
}

fn find_bash(ctx: &ShellContext, notes: &mut Vec<String>) -> Option<PathBuf> {
    if cfg!(windows) {
        find_bash_windows(ctx, notes)
    } else {
        which_in("bash", &ctx.path)
    }
}

/// Resolve one program: a name from the known set, a bare executable on
/// PATH, or an existing path.
fn resolve_program(ctx: &ShellContext, name: &str, notes: &mut Vec<String>) -> Option<PathBuf> {
    let lower = name.to_ascii_lowercase();
    let bare = lower.strip_suffix(".exe").unwrap_or(&lower);
    if cfg!(windows) && !name.contains(['\\', '/', ':']) {
        match bare {
            "bash" => return find_bash(ctx, notes),
            "cmd" => {
                if let Some(c) = ctx
                    .comspec
                    .as_ref()
                    .map(PathBuf::from)
                    .filter(|p| p.is_file())
                {
                    return Some(c);
                }
            }
            _ => {}
        }
    } else if !cfg!(windows) && name == "bash" {
        return find_bash(ctx, notes);
    }
    which_in(name, &ctx.path)
}

/// Split `shell` / `APLEXER_SHELL` text into tokens, honouring double
/// quotes. Backslashes are literal (Windows paths).
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    for c in text.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        tokens.push(current);
    }
    tokens
}

fn default_args(kind: Kind) -> Vec<String> {
    match (kind, cfg!(windows)) {
        (Kind::Bash, true) => vec!["--login".into(), "-i".into()],
        (Kind::Bash | Kind::PosixShell, false) => vec!["-l".into()],
        _ => Vec::new(),
    }
}

fn bash_env(kind: Kind) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if cfg!(windows) && kind == Kind::Bash {
        // A login bash cd's to $HOME unless told otherwise; keep the cwd.
        env.insert("CHERE_INVOKING".to_string(), "1".to_string());
        env.insert("TERM".to_string(), "xterm-256color".to_string());
    }
    env
}

fn build(
    program: PathBuf,
    args: Vec<String>,
    source: ShellSource,
    policy: Option<&str>,
    notes: Vec<String>,
) -> ShellSelection {
    let kind = kind_of(&program);
    let mut argv = vec![program.to_string_lossy().into_owned()];
    if let (Kind::PowerShell, Some(policy)) = (kind, policy) {
        let has = args
            .iter()
            .any(|a| matches!(a.to_ascii_lowercase().as_str(), "-executionpolicy" | "-ep"));
        if !has {
            argv.push("-ExecutionPolicy".into());
            argv.push(policy.to_string());
        }
    }
    argv.extend(args);
    ShellSelection {
        argv,
        env: bash_env(kind),
        source,
        notes,
    }
}

fn explicit(
    ctx: &ShellContext,
    setting: &ShellSetting,
    source: ShellSource,
    label: &str,
    policy: Option<&str>,
) -> Result<ShellSelection> {
    let mut notes = Vec::new();
    let (tokens, verbatim) = match setting {
        ShellSetting::Argv(argv) => (argv.clone(), true),
        ShellSetting::Line(line) => {
            let whole = line.trim().trim_matches('"');
            // An unquoted path with spaces ("C:\Program Files\...") is one program.
            if !whole.is_empty()
                && whole.contains(['\\', '/'])
                && which_in(whole, &ctx.path).is_some()
            {
                (vec![whole.to_string()], false)
            } else {
                (tokenize(line), false)
            }
        }
    };
    let Some(name) = tokens.first().filter(|t| !t.is_empty()) else {
        bail!(
            "{label} is empty; set it to a shell name such as \"bash\" or \"pwsh\", or a full path"
        );
    };
    let Some(program) = resolve_program(ctx, name, &mut notes) else {
        let hint = if cfg!(windows) && name.to_ascii_lowercase().trim_end_matches(".exe") == "bash"
        {
            " (install Git for Windows, or give the full path to bash.exe; the WSL launcher in System32 is never used)"
        } else {
            ""
        };
        bail!("{label}: shell {name:?} was not found{hint}");
    };
    let kind = kind_of(&program);
    let args = if verbatim || tokens.len() > 1 {
        tokens[1..].to_vec()
    } else {
        default_args(kind)
    };
    notes.push(format!("{label} selects {}", program.display()));
    Ok(build(program, args, source, policy, notes))
}

/// The platform default shell.
fn default_shell(ctx: &ShellContext, policy: Option<&str>) -> ShellSelection {
    let mut notes = vec!["no APLEXER_SHELL and no `shell` config key".to_string()];
    if cfg!(windows) {
        if let Some(bash) = find_bash_windows(ctx, &mut notes) {
            notes.push(format!("default order: Git Bash at {}", bash.display()));
            return build(
                bash,
                vec!["--login".into(), "-i".into()],
                ShellSource::Default,
                policy,
                notes,
            );
        }
        notes.push("Git for Windows bash not found".to_string());
        for candidate in ["pwsh.exe", "powershell.exe"] {
            if let Some(found) = which_in(candidate, &ctx.path) {
                notes.push(format!("default order: {}", found.display()));
                return build(found, Vec::new(), ShellSource::Default, policy, notes);
            }
        }
        let comspec = ctx.comspec.clone().unwrap_or_else(|| "cmd.exe".into());
        notes.push(format!("default order: {comspec}"));
        return ShellSelection {
            argv: vec![comspec],
            env: BTreeMap::new(),
            source: ShellSource::Default,
            notes,
        };
    }
    let shell = ctx.login_shell.clone().unwrap_or_else(|| "/bin/sh".into());
    notes.push(format!("default: $SHELL -l ({shell})"));
    ShellSelection {
        argv: vec![shell, "-l".to_string()],
        env: BTreeMap::new(),
        source: ShellSource::Default,
        notes,
    }
}

fn validate_policy(policy: Option<&str>) -> Result<Option<String>> {
    match policy {
        None => Ok(None),
        Some(value) => {
            let lower = value.to_ascii_lowercase();
            if POWERSHELL_POLICIES.contains(&lower.as_str()) {
                Ok(Some(value.to_string()))
            } else {
                Err(anyhow!(
                    "config key `powershell_execution_policy` is {value:?}; expected one of {}",
                    POWERSHELL_POLICIES.join(", ")
                ))
            }
        }
    }
}

/// Resolve the shell from the env override, the config key and the default.
/// (`[engines.shell]` is handled by the caller: it replaces the result.)
pub fn select_shell(
    ctx: &ShellContext,
    env_override: Option<&str>,
    key: Option<&ShellSetting>,
    policy: Option<&str>,
) -> Result<ShellSelection> {
    let policy = validate_policy(policy)?;
    let policy = policy.as_deref();
    if let Some(value) = env_override.filter(|v| !v.trim().is_empty()) {
        return explicit(
            ctx,
            &ShellSetting::Line(value.to_string()),
            ShellSource::Env,
            "APLEXER_SHELL",
            policy,
        );
    }
    if let Some(setting) = key {
        return explicit(
            ctx,
            setting,
            ShellSource::ConfigKey,
            "config key `shell`",
            policy,
        );
    }
    Ok(default_shell(ctx, policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"MZ").unwrap();
    }

    fn ctx_with(path_dirs: &[&Path]) -> ShellContext {
        ShellContext {
            path: std::env::join_paths(path_dirs)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            ..ShellContext::default()
        }
    }

    #[test]
    fn tokenizer_honours_quotes_and_keeps_backslashes() {
        assert_eq!(
            tokenize(r#""C:\Program Files\Git\bin\bash.exe" --login -i"#),
            vec![r"C:\Program Files\Git\bin\bash.exe", "--login", "-i"]
        );
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn config_key_parses_as_string_or_array() {
        #[derive(Deserialize)]
        struct W {
            shell: ShellSetting,
        }
        let w: W = toml::from_str(r#"shell = "bash""#).unwrap();
        assert_eq!(w.shell, ShellSetting::Line("bash".into()));
        let w: W = toml::from_str(r#"shell = ["pwsh", "-NoLogo"]"#).unwrap();
        assert_eq!(
            w.shell,
            ShellSetting::Argv(vec!["pwsh".into(), "-NoLogo".into()])
        );
    }

    #[test]
    fn invalid_policy_is_rejected_naming_the_key() {
        let err = select_shell(&ShellContext::default(), None, None, Some("nope")).unwrap_err();
        assert!(format!("{err:#}").contains("powershell_execution_policy"));
    }

    #[cfg(windows)]
    mod windows {
        use super::*;

        fn git_install(root: &Path) -> PathBuf {
            let bash = root.join(r"Git\bin\bash.exe");
            touch(&bash);
            bash
        }

        fn fake_system32(root: &Path) -> PathBuf {
            let dir = root.join("Windows").join("System32");
            touch(&dir.join("bash.exe"));
            dir
        }

        #[test]
        fn wsl_launcher_is_never_chosen_from_path() {
            let tmp = TempDir::new().unwrap();
            let sys32 = fake_system32(tmp.path());
            let ctx = ctx_with(&[&sys32]);
            let mut notes = Vec::new();
            assert_eq!(find_bash_windows(&ctx, &mut notes), None);
            assert!(notes.iter().any(|n| n.contains("WSL")));
            let err = select_shell(&ctx, None, Some(&ShellSetting::Line("bash".into())), None)
                .unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("config key `shell`") && text.contains("not found"),
                "{text}"
            );
        }

        #[test]
        fn git_bash_beats_wsl_bash_earlier_on_path() {
            let tmp = TempDir::new().unwrap();
            let sys32 = fake_system32(tmp.path());
            let pf = tmp.path().join("pf");
            let bash = git_install(&pf);
            let mut ctx = ctx_with(&[&sys32]);
            ctx.program_files = vec![pf];
            let sel = select_shell(&ctx, None, None, None).unwrap();
            assert_eq!(
                sel.argv,
                vec![
                    bash.to_string_lossy().into_owned(),
                    "--login".into(),
                    "-i".into()
                ]
            );
            assert_eq!(sel.source, ShellSource::Default);
            assert_eq!(sel.env.get("CHERE_INVOKING").map(String::as_str), Some("1"));
            assert_eq!(
                sel.env.get("TERM").map(String::as_str),
                Some("xterm-256color")
            );
        }

        #[test]
        fn bash_is_derived_from_git_exe_location() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().join("PortableGit");
            touch(&root.join(r"bin\bash.exe"));
            touch(&root.join(r"cmd\git.exe"));
            let ctx = ctx_with(&[&root.join("cmd")]);
            let found = find_bash_windows(&ctx, &mut Vec::new()).unwrap();
            assert_eq!(found, root.join(r"bin\bash.exe"));
        }

        #[test]
        fn local_appdata_git_is_probed() {
            let tmp = TempDir::new().unwrap();
            let bash = tmp.path().join(r"Programs\Git\bin\bash.exe");
            touch(&bash);
            let mut ctx = ctx_with(&[]);
            ctx.local_app_data = Some(tmp.path().to_path_buf());
            assert_eq!(find_bash_windows(&ctx, &mut Vec::new()), Some(bash));
        }

        #[test]
        fn default_falls_through_bash_pwsh_powershell_comspec() {
            let tmp = TempDir::new().unwrap();
            let sys32 = fake_system32(tmp.path());
            let bin = tmp.path().join("bin");
            touch(&bin.join("powershell.exe"));
            let mut ctx = ctx_with(&[&sys32, &bin]);
            ctx.comspec = Some(r"C:\Windows\System32\cmd.exe".into());
            let sel = select_shell(&ctx, None, None, None).unwrap();
            assert_eq!(
                sel.argv,
                vec![bin.join("powershell.exe").to_string_lossy().into_owned()]
            );
            touch(&bin.join("pwsh.exe"));
            let sel = select_shell(&ctx, None, None, None).unwrap();
            assert!(sel.argv[0].ends_with("pwsh.exe"));
            let empty = ctx_with(&[]);
            let mut empty = empty;
            empty.comspec = Some("X:\\cmd.exe".into());
            let sel = select_shell(&empty, None, None, None).unwrap();
            assert_eq!(sel.argv, vec!["X:\\cmd.exe".to_string()]);
        }

        #[test]
        fn env_beats_config_key_and_unresolvable_env_names_the_variable() {
            let tmp = TempDir::new().unwrap();
            let bin = tmp.path().join("bin");
            touch(&bin.join("pwsh.exe"));
            touch(&bin.join("powershell.exe"));
            let ctx = ctx_with(&[&bin]);
            let key = ShellSetting::Line("powershell".into());
            let sel = select_shell(&ctx, Some("pwsh"), Some(&key), None).unwrap();
            assert!(sel.argv[0].ends_with("pwsh.exe"));
            assert_eq!(sel.source, ShellSource::Env);
            let sel = select_shell(&ctx, None, Some(&key), None).unwrap();
            assert!(sel.argv[0].ends_with("powershell.exe"));
            assert_eq!(sel.source, ShellSource::ConfigKey);
            let err = select_shell(&ctx, Some("nosuchshell"), Some(&key), None).unwrap_err();
            assert!(format!("{err:#}").contains("APLEXER_SHELL"));
        }

        #[test]
        fn absolute_path_with_spaces_and_array_args() {
            let tmp = TempDir::new().unwrap();
            let sh = tmp.path().join("My Shells").join("pwsh.exe");
            touch(&sh);
            let ctx = ctx_with(&[]);
            let line = ShellSetting::Line(sh.to_string_lossy().into_owned());
            let sel = select_shell(&ctx, None, Some(&line), None).unwrap();
            assert_eq!(sel.argv, vec![sh.to_string_lossy().into_owned()]);
            let argv =
                ShellSetting::Argv(vec![sh.to_string_lossy().into_owned(), "-NoLogo".into()]);
            let sel = select_shell(&ctx, None, Some(&argv), None).unwrap();
            assert_eq!(sel.argv[1], "-NoLogo");
            let missing =
                ShellSetting::Line(tmp.path().join("gone.exe").to_string_lossy().into_owned());
            assert!(select_shell(&ctx, None, Some(&missing), None).is_err());
        }

        #[test]
        fn explicit_bash_gets_login_interactive_args() {
            let tmp = TempDir::new().unwrap();
            let pf = tmp.path().join("pf");
            let bash = git_install(&pf);
            let mut ctx = ctx_with(&[]);
            ctx.program_files = vec![pf];
            let key = ShellSetting::Line("bash".into());
            let sel = select_shell(&ctx, None, Some(&key), None).unwrap();
            assert_eq!(
                sel.argv,
                vec![
                    bash.to_string_lossy().into_owned(),
                    "--login".into(),
                    "-i".into()
                ]
            );
            let key = ShellSetting::Line("bash -i".into());
            let sel = select_shell(&ctx, None, Some(&key), None).unwrap();
            assert_eq!(sel.argv[1..], ["-i".to_string()]);
        }

        #[test]
        fn execution_policy_is_opt_in_and_powershell_only() {
            let tmp = TempDir::new().unwrap();
            let bin = tmp.path().join("bin");
            touch(&bin.join("pwsh.exe"));
            touch(&bin.join("other.exe"));
            let ctx = ctx_with(&[&bin]);
            let key = ShellSetting::Line("pwsh".into());
            let off = select_shell(&ctx, None, Some(&key), None).unwrap();
            assert_eq!(off.argv.len(), 1);
            let on = select_shell(&ctx, None, Some(&key), Some("bypass")).unwrap();
            assert_eq!(
                on.argv[1..],
                ["-ExecutionPolicy".to_string(), "bypass".to_string()]
            );
            let other = ShellSetting::Line("other".into());
            let sel = select_shell(&ctx, None, Some(&other), Some("bypass")).unwrap();
            assert_eq!(sel.argv.len(), 1);
            // an explicit -ExecutionPolicy in the args is not duplicated
            let argv = ShellSetting::Argv(vec![
                "pwsh".into(),
                "-ExecutionPolicy".into(),
                "RemoteSigned".into(),
            ]);
            let sel = select_shell(&ctx, None, Some(&argv), Some("bypass")).unwrap();
            assert_eq!(
                sel.argv
                    .iter()
                    .filter(|a| a.eq_ignore_ascii_case("-ExecutionPolicy"))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn config_file_keys_parse_and_unknown_keys_still_rejected() {
        let c: crate::Config = toml::from_str(
            "shell = [\"bash\", \"-l\"]
powershell_execution_policy = \"bypass\"
",
        )
        .unwrap();
        assert_eq!(
            c.shell,
            Some(ShellSetting::Argv(vec!["bash".into(), "-l".into()]))
        );
        assert_eq!(c.powershell_execution_policy.as_deref(), Some("bypass"));
        assert!(toml::from_str::<crate::Config>("shel = \"x\"").is_err());
    }

    #[test]
    fn unresolvable_config_shell_is_recorded_not_silently_replaced() {
        let mut config = crate::Config {
            engines: crate::Config::builtin_engines(),
            shell: Some(ShellSetting::Line("no-such-shell-zz".into())),
            ..crate::Config::default()
        };
        config.apply_shell(false, &ShellContext::default(), None);
        let error = config.shell_error.clone().unwrap();
        assert!(error.contains("config key `shell`"), "{error}");
        let direct: Vec<String> = Vec::new();
        let err = config
            .resolve(
                direct,
                Some("shell"),
                None,
                Path::new("."),
                None,
                &Default::default(),
                &crate::Limits::default(),
                None,
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("config key `shell`"));
    }

    #[test]
    fn explicit_engines_shell_wins_over_key_but_not_over_env() {
        let mut config = crate::Config {
            engines: crate::Config::builtin_engines(),
            shell: Some(ShellSetting::Line("no-such-shell-zz".into())),
            ..crate::Config::default()
        };
        config.engines.get_mut("shell").unwrap().command = vec!["mine".into()];
        config.apply_shell(true, &ShellContext::default(), None);
        assert!(config.shell_error.is_none());
        let selection = config.shell_selection.clone().unwrap();
        assert_eq!(selection.source, ShellSource::ConfigEngine);
        assert_eq!(selection.argv, vec!["mine".to_string()]);
        config.apply_shell(true, &ShellContext::default(), Some("no-such-env-shell"));
        assert!(config.shell_error.unwrap().contains("APLEXER_SHELL"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_default_is_login_shell_unchanged() {
        let mut ctx = ShellContext::default();
        ctx.login_shell = Some("/bin/zsh".into());
        let sel = select_shell(&ctx, None, None, None).unwrap();
        assert_eq!(sel.argv, vec!["/bin/zsh".to_string(), "-l".to_string()]);
        assert!(sel.env.is_empty());
    }
}
