use super::*;

#[test]
fn legacy_cursor_is_interpreted_without_ack_write_and_stale_claims_are_pruned() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "claude");
    let first = send(&harness, &sender, &recipient);
    let second = send(&harness, &sender, &recipient);
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let cursor = mp.cursors_dir.join(format!("{}.json", recipient.id));
    let original = serde_json::to_vec(&json!({"acked_through": first["id"]})).unwrap();
    std::fs::write(&cursor, &original).unwrap();
    assert_legacy_notice(&harness, &recipient, &first, &second, &cursor, &original);
    let third = send(&harness, &sender, &recipient);
    std::fs::remove_file(
        mp.msgs_dir
            .join(format!("{}.json", second["id"].as_str().unwrap())),
    )
    .unwrap();
    assert_pruned_notice(&harness, &recipient, &second, &third, &cursor, &original);
    assert_ack_prunes_claim(&harness, &recipient, &third);
}

fn assert_legacy_notice(
    harness: &Harness,
    recipient: &SessionRecord,
    first: &Value,
    second: &Value,
    cursor: &std::path::Path,
    original: &[u8],
) {
    let text = context(&notice(harness, recipient, "claude", &hook_input(false)));
    assert!(!text.contains(first["id"].as_str().unwrap()));
    assert!(text.contains(second["id"].as_str().unwrap()));
    assert_eq!(std::fs::read(cursor).unwrap(), original);
}

fn assert_pruned_notice(
    harness: &Harness,
    recipient: &SessionRecord,
    second: &Value,
    third: &Value,
    cursor: &std::path::Path,
    original: &[u8],
) {
    let text = context(&notice(harness, recipient, "claude", &hook_input(false)));
    assert!(text.contains(third["id"].as_str().unwrap()));
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let state: Value = serde_json::from_slice(
        &std::fs::read(mp.cursors_dir.join(format!("{}.notice", recipient.id))).unwrap(),
    )
    .unwrap();
    assert!(state["claimed_at"]
        .get(second["id"].as_str().unwrap())
        .is_none());
    assert_eq!(std::fs::read(cursor).unwrap(), original);
}

fn assert_ack_prunes_claim(harness: &Harness, recipient: &SessionRecord, third: &Value) {
    let ack = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["message", "ack", third["id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(ack.status.success(), "{ack:?}");
    assert!(notice(harness, recipient, "claude", &hook_input(false))
        .stdout
        .is_empty());
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let state: Value = serde_json::from_slice(
        &std::fs::read(mp.cursors_dir.join(format!("{}.notice", recipient.id))).unwrap(),
    )
    .unwrap();
    assert!(state["claimed_at"].as_object().unwrap().is_empty());
}
