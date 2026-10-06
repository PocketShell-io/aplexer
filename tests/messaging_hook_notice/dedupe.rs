use super::*;

#[test]
fn concurrent_claims_dedupe_and_cooldown_retries() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "codex");
    let sent = send(&harness, &sender, &recipient);
    let outputs = parallel_hooks(&harness, &recipient);
    assert_eq!(
        outputs.iter().filter(|out| !out.stdout.is_empty()).count(),
        1
    );
    assert!(outputs
        .iter()
        .all(|out| out.status.success() && out.stderr.is_empty()));
    assert!(notice(&harness, &recipient, "codex", &hook_input(false))
        .stdout
        .is_empty());
    assert_retry_after_cooldown(&harness, &recipient, &sent);
}

#[test]
fn a_contended_pass_never_destroys_an_existing_notice_claim() {
    let harness = Harness::new();
    let sender = harness.record(Phase::Exited, None, b"");
    let recipient = engine_record(&harness, sender.workspace.as_path(), "codex");
    let sent = send(&harness, &sender, &recipient);
    let first = notice(&harness, &recipient, "codex", &hook_input(false));
    assert!(context(&first).contains(sent["id"].as_str().unwrap()));

    // A hook firing while the mailbox lock is held must stay quiet without
    // rewriting the claim file: its snapshot is partial, so pruning claims
    // against it would erase the claim above and the next hook would
    // re-notice a message that was already announced.
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let mailbox_lock = FileLock::exclusive(&mp.workspace_dir.join(".mailbox.lock"), false).unwrap();
    let contended = notice(&harness, &recipient, "codex", &hook_input(false));
    assert!(contended.status.success() && contended.stdout.is_empty());
    drop(mailbox_lock);

    let after = notice(&harness, &recipient, "codex", &hook_input(false));
    assert!(after.status.success() && after.stdout.is_empty());
    assert!(mp
        .cursors_dir
        .join(format!("{}.notice", recipient.id))
        .exists());
}

fn parallel_hooks(harness: &Harness, recipient: &SessionRecord) -> Vec<Output> {
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut command = harness.command();
            command.env("APLEXER_SESSION_ID", recipient.id.to_string());
            std::thread::spawn(move || run_hook(command, "codex", &hook_input(false)))
        })
        .collect();
    children
        .into_iter()
        .map(|child| child.join().unwrap())
        .collect()
}

fn assert_retry_after_cooldown(harness: &Harness, recipient: &SessionRecord, sent: &Value) {
    let mp = message_paths(&harness.paths(), &recipient.workspace);
    let path = mp.cursors_dir.join(format!("{}.notice", recipient.id));
    let old = aplexer::messaging::now_secs() - 601;
    atomic_write_json(
        &path,
        &json!({"claimed_at": {sent["id"].as_str().unwrap(): old}}),
    )
    .unwrap();
    let text = context(&notice(harness, recipient, "codex", &hook_input(false)));
    assert!(text.contains(sent["id"].as_str().unwrap()));
    assert!(!mp
        .cursors_dir
        .join(format!("{}.json", recipient.id))
        .exists());
}
