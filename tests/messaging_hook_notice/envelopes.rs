use super::*;

#[test]
fn both_engines_emit_bounded_ids_only_and_preserve_ack_and_pty() {
    for engine in ["claude", "codex"] {
        assert_engine_notice(engine);
    }
}

fn assert_engine_notice(engine: &str) {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, workspace.path(), engine);
    let ids: Vec<String> = (0..6)
        .map(|_| {
            send(&harness, &sender, &recipient)["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let cursor = mp.cursors_dir.join(format!("{}.json", recipient.id));
    assert!(!cursor.exists());
    let before_history = std::fs::read(&recipient.history_path).unwrap();
    let text = context(&notice(&harness, &recipient, engine, &hook_input(false)));
    assert_bounded_private_output(&ids, &text);
    assert!(!cursor.exists());
    let state =
        std::fs::read_to_string(mp.cursors_dir.join(format!("{}.notice", recipient.id))).unwrap();
    assert!(!state.contains(BODY) && !state.contains(SECRET));
    assert_eq!(
        std::fs::read(&recipient.history_path).unwrap(),
        before_history
    );
}

fn assert_bounded_private_output(ids: &[String], text: &str) {
    assert!(text.contains("6 unread"), "{text}");
    assert_eq!(
        ids.iter().filter(|id| text.contains(id.as_str())).count(),
        5
    );
    assert!(!text.contains(&ids[5]));
    assert!(!text.contains(BODY) && !text.contains(SECRET));
}
