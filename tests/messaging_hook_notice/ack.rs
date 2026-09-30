use super::*;

pub(super) fn assert_explicit_ack(harness: &Harness, recipient: &SessionRecord, id: &str) {
    let ack = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["message", "ack", id])
        .output()
        .unwrap();
    assert!(ack.status.success(), "{ack:?}");
    let inbox = harness
        .command()
        .env("APLEXER_SESSION_ID", recipient.id.to_string())
        .args(["--json", "message", "inbox"])
        .output()
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    let unread: Value = serde_json::from_slice(&inbox.stdout).unwrap();
    assert!(unread.as_array().unwrap().is_empty());
    assert_cursor_ack(harness, recipient, id);
    assert_no_notice_after_ack(harness, recipient, id);
}

fn assert_cursor_ack(harness: &Harness, recipient: &SessionRecord, id: &str) {
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let cursor: Value = serde_json::from_slice(
        &std::fs::read(mp.cursors_dir.join(format!("{}.json", recipient.id))).unwrap(),
    )
    .unwrap();
    assert!(cursor["exceptions"]
        .as_array()
        .unwrap()
        .contains(&json!(id)));
}

fn assert_no_notice_after_ack(harness: &Harness, recipient: &SessionRecord, id: &str) {
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let state = mp.cursors_dir.join(format!("{}.notice", recipient.id));
    let claims = std::collections::BTreeMap::from([(id.to_owned(), 0)]);
    atomic_write_json(&state, &json!({"claimed_at": claims})).unwrap();
    assert!(notice(harness, recipient, "codex", &hook_input(false))
        .stdout
        .is_empty());
    let state: Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
    assert!(state["claimed_at"].as_object().unwrap().is_empty());
}
