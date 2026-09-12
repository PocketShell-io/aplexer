/// `mk_record` from status.rs + the `seeded_registry` shape from
/// switching.rs, for a whole list of records at once. The tempdirs are
/// returned alongside `Paths` so they outlive the completers' reads.
fn completion_registry(
    records: &mut [SessionRecord],
) -> (Paths, tempfile::TempDir, tempfile::TempDir) {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    for record in records.iter_mut() {
        record.socket_path = paths.socket(record.id);
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), record).unwrap();
    }
    (paths, state_dir, runtime_dir)
}

fn candidate_values(candidates: &[CompletionCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|c| c.get_value().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn selectors_complete_everywhere_and_bare_tags_only_in_their_workspace() {
    let live = mk_record("/ws/alpha", "review", Phase::Running);
    let mut dead = mk_record("/ws/beta", "done", Phase::Exited);
    dead.worker_pid = None;
    let (paths, _state_dir, _runtime_dir) = completion_registry(&mut [live, dead]);
    let cwd = PathBuf::from("/ws/alpha");

    let values = candidate_values(&complete_session_selectors(&paths, &cwd, ""));
    assert!(
        values.contains(&"/ws/alpha:review".to_string()),
        "{values:?}"
    );
    assert!(values.contains(&"/ws/beta:done".to_string()), "{values:?}");
    // The cwd workspace's tag is offered bare; the other one is not --
    // `resolve` would only find it qualified.
    assert!(values.contains(&"review".to_string()), "{values:?}");
    assert!(!values.contains(&"done".to_string()), "{values:?}");

    // The typed prefix is matched here: completer output is not
    // prefix-filtered by the completion engine.
    let review = candidate_values(&complete_session_selectors(&paths, &cwd, "rev"));
    assert_eq!(review, vec!["review".to_string()]);
    let beta = candidate_values(&complete_session_selectors(&paths, &cwd, "/ws/beta"));
    assert_eq!(beta, vec!["/ws/beta:done".to_string()]);
}

#[test]
fn selector_help_carries_engine_and_liveness() {
    let mut live = mk_record("/ws/alpha", "review", Phase::Running);
    let mut dead = mk_record("/ws/beta", "done", Phase::Exited);
    dead.worker_pid = None;
    live.profile = Some("work".to_string());
    let (paths, _state_dir, _runtime_dir) = completion_registry(&mut [live, dead]);

    let candidates = complete_session_selectors(&paths, &PathBuf::from("/ws/alpha"), "");
    let help = |selector: &str| {
        candidates
            .iter()
            .find(|c| c.get_value() == selector)
            .unwrap()
            .get_help()
            .unwrap()
            .clone()
            .to_string()
    };
    // StyledStr renders without markup for plain text.
    assert_eq!(help("/ws/alpha:review"), "shell/work, running");
    assert_eq!(help("/ws/beta:done"), "shell, exited");
}

#[test]
fn tags_complete_across_workspaces_and_qualify_their_help() {
    let live = mk_record("/ws/alpha", "review", Phase::Running);
    let mut dead = mk_record("/ws/beta", "done", Phase::Exited);
    dead.worker_pid = None;
    let (paths, _state_dir, _runtime_dir) = completion_registry(&mut [live, dead]);
    let cwd = PathBuf::from("/ws/alpha");

    // `--tag` resolves in one workspace the completer cannot see, so every
    // tag is offered; the cwd one keeps the short help, the rest name
    // their workspace.
    let all = complete_session_tags(&paths, &cwd, "");
    let values = candidate_values(&all);
    assert!(values.contains(&"review".to_string()), "{values:?}");
    assert!(values.contains(&"done".to_string()), "{values:?}");
    let done = all.iter().find(|c| c.get_value() == "done").unwrap();
    assert!(
        done.get_help()
            .unwrap()
            .clone()
            .to_string()
            .starts_with("/ws/beta:"),
        "{:?}",
        done.get_help()
    );

    let done_only = candidate_values(&complete_session_tags(&paths, &cwd, "do"));
    assert_eq!(done_only, vec!["done".to_string()]);
}

#[test]
fn engines_and_profiles_complete_from_config_over_builtins() {
    let (paths, _state_dir, _runtime_dir) = completion_registry(&mut []);
    fs::write(
        &paths.config_file,
        "version = 1\n\n[engines.godex]\ncommand = [\"echo\", \"hi\"]\n\n[profiles.work]\nengine = \"godex\"\n",
    )
    .unwrap();

    let engines = candidate_values(&complete_engines(&paths, ""));
    assert!(engines.contains(&"shell".to_string()), "{engines:?}");
    assert!(engines.contains(&"godex".to_string()), "{engines:?}");

    let profiles = candidate_values(&complete_profiles(&paths, ""));
    assert!(profiles.contains(&"work".to_string()), "{profiles:?}");
    // Auto-discovered profiles from the real HOME may also be present;
    // the prefix filter proves the typed-prefix matching still narrows.
    let work = candidate_values(&complete_profiles(&paths, "wor"));
    assert_eq!(work, vec!["work".to_string()]);
}

#[test]
fn session_completions_degrade_to_no_candidates_on_a_broken_registry() {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    // No `ensure()`, no sessions dir: `list_records` errors and the
    // completion must surface nothing, never a usage error at the prompt.
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    let cwd = env::current_dir().unwrap();
    assert!(complete_session_selectors(&paths, &cwd, "").is_empty());
    assert!(complete_session_tags(&paths, &cwd, "").is_empty());
}
