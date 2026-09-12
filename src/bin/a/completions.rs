use super::*;

/// `a completions <shell>` -- writes the clap_complete-generated script for
/// the given shell to stdout, completing for the `a` binary name itself
/// (from `#[command(name = "a")]` on `Cli` above, not the `aplexer` package
/// name), so callers just redirect it into whatever path their shell's
/// completion loader scans.
///
/// The static script knows subcommands and flags only. `a`'s live
/// vocabulary -- sessions, engines, profiles -- is completed by the
/// `COMPLETE=bash a` hook in `run()` (see [`CompleteEnv`] and the
/// `complete_*` helpers below): the shell re-runs `a` itself on every TAB,
/// so candidates can be read straight out of the registry and config with
/// the same code the commands use, with no protocol between processes to
/// keep in sync. That shell code regenerates on each `source`, so unlike
/// the static scripts it should be sourced from the rc file, not saved.
pub(crate) fn cmd_completions(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(args.shell, &mut cmd, name, &mut io::stdout());
    Ok(())
}

/// The `#[arg(add = ...)]` registration for session selectors
/// (`workspace:tag`, plus bare tags of the current workspace), as on
/// `TargetArgs::selector` and `RenameArgs::selector`.
pub(crate) fn session_selector_completions() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        let paths = completion_paths();
        let cwd = env::current_dir().unwrap_or_default();
        complete_session_selectors(&paths, &cwd, &current.to_string_lossy())
    })
}

/// The `#[arg(add = ...)]` registration for bare session tags, as on
/// `TargetArgs::tag` (resolved in `--workspace`/cwd) and quick-attach.
pub(crate) fn session_tag_completions() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        let paths = completion_paths();
        let cwd = env::current_dir().unwrap_or_default();
        complete_session_tags(&paths, &cwd, &current.to_string_lossy())
    })
}

/// The `#[arg(add = ...)]` registration for engine ids, as on
/// `--engine` and `message send --to-engine`.
pub(crate) fn engine_completions() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        complete_engines(&completion_paths(), &current.to_string_lossy())
    })
}

/// The `#[arg(add = ...)]` registration for profile ids, as on `--profile`.
pub(crate) fn profile_completions() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        complete_profiles(&completion_paths(), &current.to_string_lossy())
    })
}

/// `Paths::discover` for a completion request. Completion must degrade to
/// "no candidates", never to a usage error splashed at the prompt, so a
/// broken environment just completes nothing.
pub(crate) fn completion_paths() -> Paths {
    Paths::discover().unwrap_or_else(|_| Paths {
        runtime_root: PathBuf::new(),
        state_root: PathBuf::new(),
        config_file: PathBuf::new(),
    })
}

/// Session selectors for the `SESSION` positional. Every known session is
/// offered as its `workspace:tag` selector; sessions in the completing
/// shell's own workspace are additionally offered as bare tags, since that
/// is the shorter form `resolve` accepts there. Live sessions sort ahead
/// of exited ones (display_order); the help carries the engine/profile and
/// liveness, the facts `a list` leads with. Unlike `ArgValueCandidates`,
/// completer output is not prefix-filtered by the completion engine, so
/// the typed prefix is matched here.
pub(crate) fn complete_session_selectors(
    paths: &Paths,
    cwd: &Path,
    current: &str,
) -> Vec<CompletionCandidate> {
    let mut out = BTreeMap::new();
    for record in session_records(paths) {
        let help = session_help(&record);
        let alive = record.worker_alive();
        push_candidate(&mut out, record.selector(), current, &help, alive);
        if record.workspace == cwd {
            push_candidate(&mut out, record.tag.clone(), current, &help, alive);
        }
    }
    finish_candidates(out)
}

/// Bare session tags for `--tag` and quick-attach: the tag is resolved in
/// one workspace, so each candidate's help names the workspace it belongs
/// to. Offered from every workspace -- the completer never sees the
/// `--workspace` flag typed alongside it.
pub(crate) fn complete_session_tags(
    paths: &Paths,
    cwd: &Path,
    current: &str,
) -> Vec<CompletionCandidate> {
    let mut out = BTreeMap::new();
    for record in session_records(paths) {
        let help = if record.workspace == cwd {
            session_help(&record)
        } else {
            format!("{}: {}", record.workspace.display(), session_help(&record))
        };
        push_candidate(
            &mut out,
            record.tag.clone(),
            current,
            &help,
            record.worker_alive(),
        );
    }
    finish_candidates(out)
}

/// Engine ids from `a engines`' source (`engines_json`): built-ins plus
/// config/discovered entries, with availability in the help since a
/// "missing" engine still starts (its command simply fails at spawn).
pub(crate) fn complete_engines(paths: &Paths, current: &str) -> Vec<CompletionCandidate> {
    let Ok(values) = aplexer::api::engines_json(paths) else {
        return Vec::new();
    };
    let mut out = BTreeMap::new();
    for engine in values.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(name) = engine["name"].as_str() else {
            continue;
        };
        let help = if engine["available"].as_bool().unwrap_or(false) {
            "available"
        } else {
            "missing"
        };
        push_candidate(&mut out, name.to_string(), current, help, true);
    }
    finish_candidates(out)
}

/// Profile ids from `a profiles`' source (`profiles_json`), with each
/// profile's engine in the help.
pub(crate) fn complete_profiles(paths: &Paths, current: &str) -> Vec<CompletionCandidate> {
    let Ok(values) = aplexer::api::profiles_json(paths) else {
        return Vec::new();
    };
    let mut out = BTreeMap::new();
    let Some(object) = values.as_object() else {
        return Vec::new();
    };
    for (name, profile) in object {
        let help = format!(
            "engine: {}",
            profile["engine"].as_str().unwrap_or("(default)")
        );
        push_candidate(&mut out, name.clone(), current, &help, true);
    }
    finish_candidates(out)
}

/// The registry read behind the session completers. Failure means no
/// candidates, same contract as `completion_paths`.
pub(crate) fn session_records(paths: &Paths) -> Vec<aplexer::SessionRecord> {
    aplexer::list_records(paths).unwrap_or_default()
}

/// The `<engine>[/profile], live/dead` summary shared by the session
/// completers' help text.
pub(crate) fn session_help(record: &aplexer::SessionRecord) -> String {
    let engine = match &record.profile {
        Some(profile) => format!("{}/{}", record.engine, profile),
        None => record.engine.clone(),
    };
    let state = if record.worker_alive() {
        "running"
    } else {
        "exited"
    };
    format!("{engine}, {state}")
}

/// One deduplicated, prefix-filtered candidate slot. The `BTreeMap` keyed
/// by value collapses the duplicate selectors stale registry dirs can
/// leave behind; a live record displaces an exited one sharing its value,
/// and `display_order` keeps the survivors grouped live-first.
pub(crate) fn push_candidate(
    out: &mut BTreeMap<String, (CompletionCandidate, bool)>,
    value: String,
    current: &str,
    help: &str,
    alive: bool,
) {
    if !value.starts_with(current) {
        return;
    }
    let candidate = CompletionCandidate::new(value.clone())
        .help(Some(help.to_owned().into()))
        .display_order(Some(usize::from(!alive)));
    match out.get(&value) {
        Some((_, true)) => {}
        _ => {
            out.insert(value, (candidate, alive));
        }
    }
}

pub(crate) fn finish_candidates(
    out: BTreeMap<String, (CompletionCandidate, bool)>,
) -> Vec<CompletionCandidate> {
    out.into_values().map(|(candidate, _)| candidate).collect()
}
