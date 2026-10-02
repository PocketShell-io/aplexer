/// The shared selector body (`resolve_selector` / `resolve_tag_in_workspace`)
/// with the workspace explicit -- the tag-matching half that `a agent` and
/// `a rename` share with `a status`/`a capture`/`a send`. The env-dependent
/// "which workspace is current" half is covered end to end by
/// `tests/session_lookup.rs` (the session-binding leg).
#[test]
fn resolve_tag_in_workspace_prefers_the_live_holder_and_lists_candidates() {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let workspace = state_dir.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();

    let seed = |tag: &str| {
        let mut record = mk_record(workspace.to_str().unwrap(), tag, Phase::Running);
        record.socket_path = paths.socket(record.id);
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), &record).unwrap();
        record
    };
    let live = seed("main");
    let corpse = {
        let mut record = seed("main");
        record.phase = Phase::Exited;
        record.worker_pid = None;
        record.workload_pid = None;
        record.containment_empty = Some(true);
        record.exit = Some(aplexer::ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 1,
        });
        atomic_write_json(&paths.record(record.id), &record).unwrap();
        record
    };

    // The pair is held by a corpse next to the live session: the tag means
    // the live holder.
    let picked = resolve_tag_in_workspace(&paths, "main", &workspace).unwrap();
    assert_eq!(picked.id, live.id, "the corpse shadowed the live holder");

    // A tag nothing holds: the tag-shaped error, naming the workspace.
    let error = resolve_tag_in_workspace(&paths, "missing", &workspace)
        .expect_err("a tag nothing holds must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("no session tagged 'missing'"),
        "{message}"
    );

    // Two live holders: the ambiguity error lists every candidate by short
    // UUID prefix -- never a bare "no matching session".
    let second = seed("main");
    let error = resolve_tag_in_workspace(&paths, "main", &workspace)
        .expect_err("two live holders must fail loudly");
    let message = format!("{error:#}");
    assert!(message.contains("ambiguous"), "{message}");
    for candidate in [&live.id, &second.id, &corpse.id] {
        let prefix = &candidate.simple().to_string()[..8];
        assert!(
            message.contains(prefix),
            "candidates must include {prefix}: {message}"
        );
    }
}

/// The engine preference is a record-level choice
/// (`api::record_detected_process_with`), not a walk-level one: the walk
/// hands back every agent-named process it found and the record's engine
/// picks. The unit-level halves are pinned in `agent_kind::tests`; here
/// the mapping from a record's engine id is checked through the same
/// entry the surfaces use, with `engine: "shell"` as the no-signal case.
#[test]
fn engine_kind_mapping_covers_the_shipped_alias_and_shell() {
    use aplexer::agent_kind::{engine_kind, AgentKind};
    assert_eq!(engine_kind("codex"), Some(AgentKind::Codex));
    assert_eq!(
        engine_kind("zcodex"),
        Some(AgentKind::Codex),
        "the shipped alias rides its family"
    );
    assert_eq!(engine_kind("shell"), None);
}
