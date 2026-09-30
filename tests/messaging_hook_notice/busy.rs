use super::*;

#[test]
fn busy_mailbox_lock_does_not_hold_a_tool_completion() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "claude");
    let _ = send(&harness, &sender, &recipient);
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let _lock = FileLock::exclusive(&mp.workspace_dir.join(".mailbox.lock"), false).unwrap();
    let start = std::time::Instant::now();
    let output = notice(&harness, &recipient, "claude", &hook_input(false));
    assert!(output.status.success() && output.stdout.is_empty());
    assert!(start.elapsed().as_secs() < 2);
    assert!(!mp
        .cursors_dir
        .join(format!("{}.notice", recipient.id))
        .exists());
}
