//! Engine executable resolution under differing PATHs, and the pin writer
//! behind `a doctor --fix` (issue #19).
//!
//! The PocketShell app drives `a start` over non-interactive SSH, where
//! `~/.bashrc` never sources the version managers (nvm et al.) that put
//! `codex`/`claude`/`gemini`/`opencode` on an interactive shell's PATH. An
//! engine that resolves only through such a tool therefore works from the
//! user's terminal and fails from the app with "not found in PATH". The
//! durable fix is machine-local self-healing: `a doctor` compares each
//! configured executable's resolution under the invoking shell's PATH
//! against a minimal non-interactive PATH, and `--fix` persists the
//! resolved absolute path into the user's `config.toml` — the only PATH
//! independent form an engine command can take. Version-manager drift is
//! covered from the other side: a pinned absolute path whose file has since
//! vanished (the active node version moved, say) is flagged as stale and
//! re-resolved from the current PATH.

use anyhow::{bail, Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::Config;
use crate::paths::home_dir;
use crate::Paths;

/// The PATH a fresh non-interactive shell session has: the sshd default on
/// Debian-family systems, plus `~/.local/bin` — the one user-writable
/// directory installs land in without a version manager (`grok` is the
/// working example: it resolves for the app precisely because it lives
/// there). Anything that needs more than this to resolve is exactly what
/// the app's non-interactive launch cannot reproduce.
const MINIMAL_PATH_DIRS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// The minimal PATH string the engine-resolution check probes against:
/// [`MINIMAL_PATH_DIRS`] plus the user's `~/.local/bin` when a home
/// directory is known.
pub fn minimal_path() -> String {
    let mut dirs: Vec<String> = MINIMAL_PATH_DIRS
        .iter()
        .map(|dir| dir.to_string())
        .collect();
    if let Ok(home) = home_dir() {
        dirs.push(home.join(".local/bin").to_string_lossy().into_owned());
    }
    dirs.join(":")
}

/// True when a command word goes through PATH lookup at all: anything with
/// a slash in it is resolved (or fails to resolve) directly.
pub fn is_bare_executable(name: &str) -> bool {
    !name.contains('/')
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// which(1) over an explicit PATH string, so the check can probe two
/// different PATHs in one process. A bare name resolves to the first
/// directory on `path_var` holding an executable file; a path with a slash
/// in it is checked directly.
pub fn which_in(name: &str, path_var: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let direct = PathBuf::from(name);
        return is_executable_file(&direct).then_some(direct);
    }
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .find_map(|dir| {
            let candidate = Path::new(dir).join(name);
            is_executable_file(&candidate).then_some(candidate)
        })
}

/// What the two-PATH comparison says about one configured executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionVerdict {
    /// Resolves the same way everywhere it needs to (or is pinned to an
    /// existing absolute path).
    Resolved,
    /// Found under neither PATH: the engine is simply not installed, which
    /// is a legitimate state, not a defect (`a doctor` says so without
    /// offering a fix).
    NotInstalled,
    /// A bare name that only the invoking shell's PATH can resolve (or that
    /// resolves to different binaries under the two PATHs): the app's
    /// non-interactive launch would break or change here. `--fix` pins the
    /// current resolution.
    NeedsPin,
    /// A pinned absolute path whose file no longer exists — version-manager
    /// drift. `--fix` re-resolves the basename from the current PATH.
    StalePin,
}

impl ResolutionVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResolutionVerdict::Resolved => "resolved",
            ResolutionVerdict::NotInstalled => "not_installed",
            ResolutionVerdict::NeedsPin => "needs_pin",
            ResolutionVerdict::StalePin => "stale_pin",
        }
    }

    fn wants_fix(&self) -> bool {
        matches!(
            self,
            ResolutionVerdict::NeedsPin | ResolutionVerdict::StalePin
        )
    }
}

/// One configured executable (an engine's `command[0]`, a profile's
/// `executable`, or a profile `command`'s first element) with its verdict.
#[derive(Debug, Clone)]
pub struct ResolutionRow {
    /// `"engine"` or `"profile"`.
    pub kind: &'static str,
    pub name: String,
    /// The command word as configured — bare name or path.
    pub executable: String,
    pub verdict: ResolutionVerdict,
    /// Resolution under the invoking shell's PATH (for a stale pin: none —
    /// the configured path itself is gone).
    pub current: Option<PathBuf>,
    /// Resolution under the minimal non-interactive PATH.
    pub minimal: Option<PathBuf>,
    /// What `--fix` would pin: the current resolution, or for a stale pin
    /// the basename re-resolved from the current PATH.
    pub fix_candidate: Option<PathBuf>,
    pub detail: String,
}

impl ResolutionRow {
    /// Whether `a doctor --fix` has something to do for this row.
    pub fn wants_fix(&self) -> bool {
        self.verdict.wants_fix()
    }

    fn bare(
        kind: &'static str,
        name: &str,
        executable: &str,
        current_path: &str,
        minimal: &str,
    ) -> Self {
        let current = which_in(executable, current_path);
        let under_minimal = which_in(executable, minimal);
        let (verdict, detail) = match (&current, &under_minimal) {
            (Some(found), Some(same)) if found == same => (
                ResolutionVerdict::Resolved,
                format!("resolves to {} under both PATHs", found.display()),
            ),
            (Some(found), Some(other)) => (
                ResolutionVerdict::NeedsPin,
                format!(
                    "resolves to {} via the invoking shell's PATH but {} under a non-interactive session's PATH",
                    found.display(),
                    other.display()
                ),
            ),
            (Some(found), None) => (
                ResolutionVerdict::NeedsPin,
                format!(
                    "resolves to {} via the invoking shell's PATH only; a non-interactive session (the app's SSH) would not find it",
                    found.display()
                ),
            ),
            (None, _) => (
                ResolutionVerdict::NotInstalled,
                "not found under either PATH (not installed)".to_string(),
            ),
        };
        ResolutionRow {
            kind,
            name: name.to_string(),
            executable: executable.to_string(),
            verdict,
            fix_candidate: current.clone(),
            current,
            minimal: under_minimal,
            detail,
        }
    }

    fn pinned(kind: &'static str, name: &str, executable: &str, current_path: &str) -> Self {
        let (verdict, detail, fix_candidate) = if is_executable_file(Path::new(executable)) {
            (
                ResolutionVerdict::Resolved,
                format!("pinned absolute path {}", executable),
                None,
            )
        } else {
            let re_resolved = Path::new(executable)
                .file_name()
                .and_then(|base| base.to_str())
                .and_then(|base| which_in(base, current_path));
            (
                ResolutionVerdict::StalePin,
                format!(
                    "pinned path {} no longer exists (version-manager drift?); a non-interactive session would fail to launch this",
                    executable
                ),
                re_resolved,
            )
        };
        ResolutionRow {
            kind,
            name: name.to_string(),
            executable: executable.to_string(),
            verdict,
            fix_candidate,
            current: None,
            minimal: None,
            detail,
        }
    }

    fn row(
        kind: &'static str,
        name: &str,
        executable: &str,
        current_path: &str,
        minimal: &str,
    ) -> Self {
        if is_bare_executable(executable) {
            Self::bare(kind, name, executable, current_path, minimal)
        } else {
            Self::pinned(kind, name, executable, current_path)
        }
    }
}

/// Every engine `command[0]` and profile `executable`/`command[0]` in the
/// merged configuration, probed against the two PATH strings.
pub fn resolution_rows(
    config: &Config,
    current_path: &str,
    minimal_path: &str,
) -> Vec<ResolutionRow> {
    let mut rows = Vec::new();
    for (name, engine) in &config.engines {
        if let Some(executable) = engine.command.first() {
            rows.push(ResolutionRow::row(
                "engine",
                name,
                executable,
                current_path,
                minimal_path,
            ));
        }
    }
    for (name, profile) in &config.profiles {
        let executable = profile.executable.clone().or_else(|| {
            profile
                .command
                .as_ref()
                .and_then(|command| command.first().cloned())
        });
        if let Some(executable) = executable {
            rows.push(ResolutionRow::row(
                "profile",
                name,
                &executable,
                current_path,
                minimal_path,
            ));
        }
    }
    rows
}

/// One persisted pin from `a doctor --fix`.
#[derive(Debug)]
pub struct AppliedPin {
    /// Where the pin landed, e.g. `engines.codex` or `profiles.zcodex`.
    pub target: String,
    /// The command word as it was configured.
    pub from: String,
    /// The absolute path now pinned.
    pub to: PathBuf,
}

/// Persist a fix candidate for every pin-worthy row into the user's
/// `config.toml`, preserving everything else in the file (toml_edit keeps
/// comments and layout; only the touched tables change). An engine the user
/// never configured gets its full merged command written with the resolved
/// absolute argv[0] — pinning builtin arguments as of today's aplexer, the
/// same trade the manual config fix made. Rows without a resolvable
/// candidate are reported in the returned failures and change nothing.
pub fn apply_pins(
    paths: &Paths,
    config: &Config,
    rows: &[ResolutionRow],
) -> Result<(Vec<AppliedPin>, Vec<String>)> {
    let text = match fs::read_to_string(&paths.config_file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context(format!("read {}", paths.config_file.display())),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", paths.config_file.display()))?;

    let mut applied = Vec::new();
    let mut failures = Vec::new();
    for row in rows {
        if !row.verdict.wants_fix() {
            continue;
        }
        let kind_plural = if row.kind == "engine" {
            "engines"
        } else {
            "profiles"
        };
        let Some(resolved) = &row.fix_candidate else {
            failures.push(format!(
                "{kind_plural}.{}: {} cannot be resolved from the current PATH either; nothing to pin",
                row.name, row.executable
            ));
            continue;
        };
        let resolved = resolved.clone();
        let table = ensure_child_table(doc.as_table_mut(), kind_plural);
        let entry = ensure_child_table(table, &row.name);
        if row.kind == "engine" {
            let Some(engine) = config.engines.get(&row.name) else {
                bail!("engine {} vanished between probe and fix", row.name);
            };
            let mut command = engine.command.clone();
            command[0] = resolved.to_string_lossy().into_owned();
            entry.insert("command", toml_edit::value(strings_to_toml_array(&command)));
        } else if config.profiles.get(&row.name).unwrap().command.is_some() {
            let mut command = config
                .profiles
                .get(&row.name)
                .unwrap()
                .command
                .clone()
                .unwrap();
            command[0] = resolved.to_string_lossy().into_owned();
            entry.insert("command", toml_edit::value(strings_to_toml_array(&command)));
        } else {
            entry.insert(
                "executable",
                toml_edit::value(resolved.to_string_lossy().into_owned()),
            );
        }
        applied.push(AppliedPin {
            target: format!("{kind_plural}.{}", row.name),
            from: row.executable.clone(),
            to: resolved,
        });
    }

    if !applied.is_empty() {
        crate::ensure_private_dir(
            paths
                .config_file
                .parent()
                .ok_or_else(|| anyhow::anyhow!("config file has no parent directory"))?,
        )?;
        crate::atomic_write_bytes(&paths.config_file, doc.to_string().as_bytes())?;
    }
    Ok((applied, failures))
}

fn strings_to_toml_array(values: &[String]) -> toml_edit::Array {
    let mut array = toml_edit::Array::new();
    for value in values {
        array.push(value.as_str());
    }
    array
}

/// The child table `key` of `table`, created when absent — or when present
/// as something other than a table (the config schema would have rejected
/// the file before we got here, but the writer must not panic on it).
fn ensure_child_table<'a>(table: &'a mut toml_edit::Table, key: &str) -> &'a mut toml_edit::Table {
    if !matches!(table.get(key), Some(item) if item.is_table()) {
        table.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    table
        .get_mut(key)
        .unwrap()
        .as_table_mut()
        .expect("just ensured a table")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn bin_dir_with(name: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join(name);
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
        let dir_path = dir.path().to_string_lossy().into_owned();
        (dir, dir_path)
    }

    fn engine(command: &[&str]) -> super::super::EngineConfig {
        super::super::EngineConfig {
            command: command.iter().map(|value| value.to_string()).collect(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            skip_permissions_argv: Vec::new(),
        }
    }

    fn config_with_engines(entries: &[(&str, &[&str])]) -> Config {
        Config {
            engines: entries
                .iter()
                .map(|(name, command)| (name.to_string(), engine(command)))
                .collect(),
            ..Config::default()
        }
    }

    #[test]
    fn which_in_finds_executables_and_skips_non_executable_files() {
        let (dir, dir_path) = bin_dir_with("fakeagent");
        let found = which_in("fakeagent", &dir_path).unwrap();
        assert_eq!(found, dir.path().join("fakeagent"));
        assert_eq!(which_in("missing", &dir_path), None);

        let plain = dir.path().join("plain");
        fs::write(&plain, "not executable").unwrap();
        assert_eq!(which_in("plain", &dir_path), None);
    }

    #[test]
    fn which_in_checks_slashed_paths_directly_and_first_dir_wins() {
        let (first, first_path) = bin_dir_with("dup");
        let (second, second_path) = bin_dir_with("dup");
        let both = format!("{first_path}:{second_path}");
        assert_eq!(which_in("dup", &both), Some(first.path().join("dup")));

        let missing = first.path().join("gone");
        assert_eq!(which_in(&missing.to_string_lossy(), &both), None);
        assert_eq!(
            which_in(&second.path().join("dup").to_string_lossy(), &both),
            Some(second.path().join("dup"))
        );
        let _ = first_path;
    }

    #[test]
    fn minimal_path_covers_system_dirs_and_local_bin() {
        let minimal = minimal_path();
        assert!(minimal.starts_with("/usr/local/sbin:"));
        assert!(minimal.split(':').any(|dir| dir.ends_with("/.local/bin")));
        assert!(minimal.split(':').any(|dir| dir == "/usr/bin"));
    }

    #[test]
    fn a_bare_name_only_the_shell_path_resolves_is_a_pin_candidate() {
        let (_dir, dir_path) = bin_dir_with("fakecodex");
        let minimal = "/usr/bin:/bin";
        let row = ResolutionRow::row("engine", "codex", "fakecodex", &dir_path, minimal);
        assert_eq!(row.verdict, ResolutionVerdict::NeedsPin);
        assert_eq!(
            row.fix_candidate,
            Some(which_in("fakecodex", &dir_path).unwrap())
        );
        assert!(row.detail.contains("non-interactive"), "{}", row.detail);
    }

    #[test]
    fn a_bare_name_on_the_minimal_path_needs_nothing() {
        let (_dir, dir_path) = bin_dir_with("systemtool");
        let row = ResolutionRow::row("engine", "shell", "systemtool", &dir_path, &dir_path);
        assert_eq!(row.verdict, ResolutionVerdict::Resolved);
        assert!(!row.verdict.wants_fix());
    }

    #[test]
    fn a_missing_everywhere_name_is_not_installed_not_broken() {
        let row = ResolutionRow::row("engine", "grok", "grok", "/nonexistent", "/usr/bin");
        assert_eq!(row.verdict, ResolutionVerdict::NotInstalled);
        assert!(!row.verdict.wants_fix());
    }

    #[test]
    fn divergent_resolution_between_the_two_paths_is_flagged() {
        let (_shell_dir, shell_dir_path) = bin_dir_with("duo");
        let (other_dir, _other) = bin_dir_with("duo");
        let row = ResolutionRow::row(
            "engine",
            "duo",
            "duo",
            &shell_dir_path,
            &other_dir.path().to_string_lossy(),
        );
        assert_eq!(row.verdict, ResolutionVerdict::NeedsPin);
        assert!(row.detail.contains("but"), "{}", row.detail);
    }

    #[test]
    fn a_pinned_path_that_vanished_is_stale_and_re_resolves_from_basename() {
        let (_dir, dir_path) = bin_dir_with("drifted");
        let stale = format!("{dir_path}/old-version/drifted");
        let row = ResolutionRow::row("engine", "drifted", &stale, &dir_path, "/usr/bin");
        assert_eq!(row.verdict, ResolutionVerdict::StalePin);
        assert_eq!(
            row.fix_candidate,
            Some(which_in("drifted", &dir_path).unwrap())
        );
    }

    #[test]
    fn resolution_rows_cover_engines_and_profile_executables_and_commands() {
        let mut config = config_with_engines(&[("codex", &["codex", "-c", "x"])]);
        config.profiles.insert(
            "zcodex".into(),
            super::super::ProfileConfig {
                engine: Some("codex".into()),
                executable: Some("zcodex".into()),
                ..Default::default()
            },
        );
        config.profiles.insert(
            "review".into(),
            super::super::ProfileConfig {
                command: Some(vec!["reviewtool".into(), "--fast".into()]),
                ..Default::default()
            },
        );
        let rows = resolution_rows(&config, "/nonexistent", "/usr/bin");
        let executables: Vec<&str> = rows.iter().map(|row| row.executable.as_str()).collect();
        assert_eq!(executables, ["codex", "reviewtool", "zcodex"]);
        assert_eq!(rows[0].kind, "engine");
        assert_eq!(rows[1].kind, "profile");
        assert_eq!(rows[2].kind, "profile");
    }

    fn test_paths(temp: &tempfile::TempDir, config_name: &str) -> Paths {
        Paths {
            runtime_root: temp.path().join("runtime"),
            state_root: temp.path().join("state"),
            config_file: temp.path().join(config_name),
        }
    }

    #[test]
    fn apply_pins_writes_the_merged_engine_command_and_keeps_comments() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_dir, dir_path) = bin_dir_with("fakecodex");
        let paths = test_paths(&temp, "config.toml");
        fs::write(
            &paths.config_file,
            "version = 1\n# my codex, kept for offline runs\n[engines.fakecodex]\ncommand = [\"fakecodex\", \"serve\"]\n",
        )
        .unwrap();
        let config = config_with_engines(&[("fakecodex", &["fakecodex", "serve"])]);
        let rows = resolution_rows(&config, &dir_path, "/usr/bin");
        let (applied, failures) = apply_pins(&paths, &config, &rows).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].target, "engines.fakecodex");

        let written = fs::read_to_string(&paths.config_file).unwrap();
        assert!(
            written.contains("# my codex, kept for offline runs"),
            "comment lost: {written}"
        );
        let reparsed: Config = toml::from_str(&written).unwrap();
        let expected = which_in("fakecodex", &dir_path).unwrap();
        assert_eq!(
            reparsed.engines["fakecodex"].command,
            vec![expected.to_string_lossy().into_owned(), "serve".into()]
        );
    }

    #[test]
    fn apply_pins_creates_an_engine_entry_for_a_builtin_and_a_config_file_from_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_dir, dir_path) = bin_dir_with("freshgem");
        let paths = test_paths(&temp, "config.toml");
        let mut config = config_with_engines(&[("freshgem", &["freshgem", "--fast"])]);
        config.profiles.insert(
            "zed".into(),
            super::super::ProfileConfig {
                engine: Some("freshgem".into()),
                executable: Some("freshgem".into()),
                ..Default::default()
            },
        );
        let rows = resolution_rows(&config, &dir_path, "/usr/bin");
        let (applied, failures) = apply_pins(&paths, &config, &rows).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(applied.len(), 2);

        let written = fs::read_to_string(&paths.config_file).unwrap();
        let reparsed: Config = toml::from_str(&written).unwrap();
        let expected = which_in("freshgem", &dir_path).unwrap();
        assert_eq!(
            reparsed.engines["freshgem"].command[0],
            expected.to_string_lossy()
        );
        assert_eq!(reparsed.engines["freshgem"].command[1], "--fast");
        assert_eq!(
            reparsed.profiles["zed"].executable.as_deref(),
            Some(expected.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn apply_pins_reports_an_unresolvable_row_and_leaves_the_file_alone() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(&temp, "config.toml");
        fs::write(&paths.config_file, "version = 1\n").unwrap();
        let stale = "/gone/bin/ghost".to_string();
        let row = ResolutionRow {
            kind: "engine",
            name: "ghost".into(),
            executable: stale.clone(),
            verdict: ResolutionVerdict::StalePin,
            current: None,
            minimal: None,
            fix_candidate: None,
            detail: "stale".into(),
        };
        let config = config_with_engines(&[("ghost", &["ghost"])]);
        let before = fs::read_to_string(&paths.config_file).unwrap();
        let (applied, failures) = apply_pins(&paths, &config, &[row]).unwrap();
        assert!(applied.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("engines.ghost"), "{failures:?}");
        assert_eq!(
            fs::read_to_string(&paths.config_file).unwrap(),
            before,
            "an unfixable row must not touch the file"
        );
    }

    #[test]
    fn apply_pins_rewrites_a_stale_profile_command_in_place() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_dir, dir_path) = bin_dir_with("reviewtool");
        let paths = test_paths(&temp, "config.toml");
        let stale = "/gone/bin/reviewtool".to_string();
        fs::write(
            &paths.config_file,
            format!("version = 1\n[profiles.review]\ncommand = [\"{stale}\", \"--fast\"]\n"),
        )
        .unwrap();
        let mut config = Config::default();
        config.profiles.insert(
            "review".into(),
            super::super::ProfileConfig {
                command: Some(vec![stale.clone(), "--fast".into()]),
                ..Default::default()
            },
        );
        let rows = resolution_rows(&config, &dir_path, "/usr/bin");
        assert_eq!(rows[0].verdict, ResolutionVerdict::StalePin);
        let (applied, failures) = apply_pins(&paths, &config, &rows).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(applied[0].target, "profiles.review");

        let reparsed: Config =
            toml::from_str(&fs::read_to_string(&paths.config_file).unwrap()).unwrap();
        let expected = which_in("reviewtool", &dir_path).unwrap();
        assert_eq!(
            reparsed.profiles["review"].command,
            Some(vec![
                expected.to_string_lossy().into_owned(),
                "--fast".into()
            ])
        );
    }
}
