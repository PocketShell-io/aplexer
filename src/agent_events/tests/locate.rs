use super::*;

#[test]
fn encode_claude_cwd_replaces_slash_and_dot() {
    assert_eq!(
        encode_claude_cwd("/data/tmp/.tmpBU2mbw"),
        "-data-tmp--tmpBU2mbw"
    );
    assert_eq!(
        encode_claude_cwd("/tmp/aplexer-follow"),
        "-tmp-aplexer-follow"
    );
}

#[test]
fn encode_grok_cwd_matches_python_quote_safe_empty() {
    assert_eq!(
        encode_grok_cwd("/home/alexey/git/aplexer"),
        "%2Fhome%2Falexey%2Fgit%2Faplexer"
    );
    assert_eq!(
        encode_grok_cwd("/tmp/aplexer-tx-test"),
        "%2Ftmp%2Faplexer-tx-test"
    );
}

#[test]
fn bind_sidecar_reuses_path() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    std::fs::write(&log, "{}\n").unwrap();
    let bind = dir.path().join("transcript.json");
    let record = dummy_record("claude");
    // First locate would fail (HOME is not this dir); write a bind first.
    atomic_write_json(
        &bind,
        &TranscriptBind {
            path: log.clone(),
            engine_session_id: Some("x".into()),
            engine: None,
        },
    )
    .unwrap();
    let located = resolve_transcript(&record, &bind).unwrap();
    assert_eq!(located.path, log);
    assert_eq!(located.engine_session_id.as_deref(), Some("x"));
}

// ---------------------------------------------------------------------
// Issue #20: live-fd discovery, ambiguity refusal, bind-engine
// persistence. Synthetic /proc roots and CODEX_HOME dirs -- never a
// real HOME or live process.

fn codex_rollout(dir: &Path, name: &str, cwd: &str, answer: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n{}\n",
            json!({"type": "session_meta", "payload": {"id": "thread", "cwd": cwd}}),
            json!({"type": "response_item", "payload": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "continue the recap"}]
            }}),
            json!({"type": "response_item", "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": answer}]
            }})
        ),
    )
    .unwrap();
    path
}

/// A synthetic /proc tree: `pid` holds the given targets open on its file
/// descriptors, the same shape `open_jsonl_fds` reads back.
fn fake_proc(root: &Path, pid: u32, open: &[PathBuf]) -> PathBuf {
    let fd_dir = root.join(pid.to_string()).join("fd");
    std::fs::create_dir_all(&fd_dir).unwrap();
    for (n, target) in open.iter().enumerate() {
        std::os::unix::fs::symlink(target, fd_dir.join(n.to_string())).unwrap();
    }
    root.to_path_buf()
}

fn shell_session_in(dir: &Path, codex_home: &Path) -> SessionRecord {
    let mut record = dummy_record("shell");
    record.cwd = dir.join("proj");
    std::fs::create_dir_all(&record.cwd).unwrap();
    record.created_at_ms = crate::now_ms();
    record
        .env
        .insert("CODEX_HOME".into(), codex_home.display().to_string());
    record
}

#[test]
fn live_fd_binds_the_single_open_rollout_and_records_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("zcodex-home");
    let rollout = codex_rollout(
        &codex_home.join("sessions"),
        "rollout.jsonl",
        &dir.path().join("proj").display().to_string(),
        "mid-insert",
    );
    let proc_root = fake_proc(&dir.path().join("proc"), 4242, std::slice::from_ref(&rollout));
    let record = shell_session_in(dir.path(), &codex_home);
    let bind_path = dir.path().join("transcript.json");

    let resolution = resolve_transcript_detailed(
        &record,
        &bind_path,
        &proc_root,
        Some(LiveAgent {
            pid: 4242,
            kind: crate::agent_kind::AgentKind::Codex,
        }),
    );

    assert_eq!(resolution.path.as_deref(), Some(rollout.as_path()));
    assert_eq!(resolution.source, "live-fd");
    // A shell record hosting codex parses as codex, never shell.
    assert_eq!(resolution.engine.as_deref(), Some("codex"));
    assert_eq!(resolution.engine_session_id.as_deref(), Some("thread"));
    let bind = resolution.bind.as_ref().unwrap();
    assert!(bind.wrote, "bind write should succeed: {:?}", bind.write_error);
    let saved: TranscriptBind =
        serde_json::from_slice(&std::fs::read(&bind_path).unwrap()).unwrap();
    assert_eq!(saved.engine.as_deref(), Some("codex"));
    // The point of the engine: the log must read back as codex events.
    let events = read_transcript_events("codex", &rollout).unwrap();
    assert_eq!(events.len(), 2);
}

#[test]
fn live_fd_with_two_plausible_rollouts_refuses_and_binds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("zcodex-home");
    let cwd_str = dir.path().join("proj").display().to_string();
    let top = codex_rollout(&codex_home.join("sessions"), "top.jsonl", &cwd_str, "top");
    let nested = codex_rollout(&codex_home.join("sessions"), "nested.jsonl", &cwd_str, "nested");
    let proc_root = fake_proc(&dir.path().join("proc"), 4242, &[top, nested]);
    let record = shell_session_in(dir.path(), &codex_home);
    let bind_path = dir.path().join("transcript.json");

    let resolution = resolve_transcript_detailed(
        &record,
        &bind_path,
        &proc_root,
        Some(LiveAgent {
            pid: 4242,
            kind: crate::agent_kind::AgentKind::Codex,
        }),
    );

    assert!(resolution.path.is_none());
    let error = resolution.error.as_deref().unwrap();
    assert!(error.contains("refusing to guess"), "{error}");
    assert!(error.contains("top.jsonl") && error.contains("nested.jsonl"), "{error}");
    assert_eq!(resolution.candidates.len(), 2);
    // A wrong automatic bind is worse than no bind: nothing was written.
    assert!(!bind_path.exists());
}

#[test]
fn bound_log_stays_usable_after_the_agent_exits() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("zcodex-home");
    let rollout = codex_rollout(
        &codex_home.join("sessions"),
        "rollout.jsonl",
        &dir.path().join("proj").display().to_string(),
        "done for now",
    );
    let record = shell_session_in(dir.path(), &codex_home);
    let bind_path = dir.path().join("transcript.json");
    atomic_write_json(
        &bind_path,
        &TranscriptBind {
            path: rollout.clone(),
            engine_session_id: Some("thread".into()),
            engine: Some("codex".into()),
        },
    )
    .unwrap();

    // No live agent: the sidecar is the only evidence left, and it must
    // still resolve -- with the engine it vouches for, not the record's
    // transcript-less `shell`.
    let resolution = resolve_transcript_detailed(&record, &bind_path, &dir.path().join("proc"), None);

    assert_eq!(resolution.source, "bind");
    assert_eq!(resolution.path.as_deref(), Some(rollout.as_path()));
    assert_eq!(resolution.engine.as_deref(), Some("codex"));
    let located = resolve_transcript(&record, &bind_path).unwrap();
    assert_eq!(located.engine, "codex");
    assert_eq!(located.path, rollout);
}

#[test]
fn stale_bind_falls_through_to_rediscovery_and_names_the_old_path() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("codex-home");
    let rollout = codex_rollout(
        &codex_home.join("sessions"),
        "rollout.jsonl",
        &dir.path().join("proj").display().to_string(),
        "rotated away",
    );
    let mut record = dummy_record("codex");
    record.cwd = dir.path().join("proj");
    std::fs::create_dir_all(&record.cwd).unwrap();
    record.created_at_ms = crate::now_ms();
    record
        .env
        .insert("CODEX_HOME".into(), codex_home.display().to_string());
    let bind_path = dir.path().join("transcript.json");
    let gone = dir.path().join("gone.jsonl");
    atomic_write_json(
        &bind_path,
        &TranscriptBind {
            path: gone.clone(),
            engine_session_id: None,
            engine: None,
        },
    )
    .unwrap();

    let resolution = resolve_transcript_detailed(&record, &bind_path, &dir.path().join("proc"), None);

    assert_eq!(resolution.path.as_deref(), Some(rollout.as_path()));
    assert_eq!(resolution.source, "heuristic");
    let bind = resolution.bind.as_ref().unwrap();
    assert_eq!(bind.points_to.as_deref(), Some(gone.as_path()));
    assert!(bind.wrote);
}

#[test]
fn heuristic_ambiguity_names_candidates_and_binds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("codex-home");
    let cwd_str = dir.path().join("proj").display().to_string();
    let first = codex_rollout(&codex_home.join("sessions"), "a.jsonl", &cwd_str, "one");
    let second = codex_rollout(&codex_home.join("sessions"), "b.jsonl", &cwd_str, "two");
    let mut record = dummy_record("codex");
    record.cwd = dir.path().join("proj");
    std::fs::create_dir_all(&record.cwd).unwrap();
    record.created_at_ms = crate::now_ms();
    record
        .env
        .insert("CODEX_HOME".into(), codex_home.display().to_string());
    let bind_path = dir.path().join("transcript.json");

    let resolution = resolve_transcript_detailed(&record, &bind_path, &dir.path().join("proc"), None);

    assert!(resolution.path.is_none());
    let error = resolution.error.as_deref().unwrap();
    assert!(error.contains("refusing to guess"), "{error}");
    assert!(error.contains("--engine") && error.contains("--path"), "{error}");
    assert_eq!(resolution.candidates, vec![first, second]);
    assert!(!bind_path.exists());
}
