use super::*;
use tempfile::TempDir;

fn fixture(root: &Path) -> (Paths, PathBuf, SessionIdentity) {
    let paths = Paths {
        runtime_root: root.join("runtime"),
        state_root: root.join("state"),
        config_file: root.join("config.toml"),
    };
    paths.ensure().unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let consumer = SessionIdentity {
        id: Uuid::now_v7(),
        workspace: Some(workspace.clone()),
        tag: Some("reader".into()),
        engine: Some("codex".into()),
        profile: None,
    };
    (paths, workspace, consumer)
}

fn envelope(workspace: &Path, consumer: &SessionIdentity) -> MessageEnvelope {
    MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: workspace.to_path_buf(),
        created_at: now_secs(),
        from: MessageFrom::anonymous(),
        to: Recipient::Tag {
            tag: "reader".into(),
            session_id: Some(consumer.id),
        },
        kind: "note".into(),
        reply_to: None,
        body: "hello".into(),
        data: None,
        delivery: Delivery::Inbox,
    }
}

#[test]
fn existing_unread_returns_without_acknowledging() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let message = envelope(&workspace, &consumer);
    write_message(&paths, &message).unwrap();
    let result = wait_messages(&paths, &workspace, &consumer, Duration::from_secs(30)).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, message.id);
    assert!(!read_cursor(&paths, &workspace, consumer.id)
        .unwrap()
        .is_acked(message.id));
}

#[test]
fn subscription_precedes_snapshot_and_retains_early_publication() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let watch = MailboxWatch::subscribe(&mp.msgs_dir).unwrap();
    // Publication in the subscription/initial-scan gap is both readable
    // immediately and queued on the descriptor before polling begins.
    let message = envelope(&workspace, &consumer);
    write_message_in(&mp, &message).unwrap();
    assert_eq!(
        unread_snapshot(&mp, &workspace, &consumer).unwrap()[0].id,
        message.id
    );
    watch
        .wait_for_change(Instant::now() + Duration::from_secs(2))
        .unwrap();
}

#[test]
fn unrelated_publication_is_only_a_wake_hint() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let watch = MailboxWatch::subscribe(&mp.msgs_dir).unwrap();
    assert!(unread_snapshot(&mp, &workspace, &consumer)
        .unwrap()
        .is_empty());
    let mut unrelated = envelope(&workspace, &consumer);
    unrelated.to = Recipient::Tag {
        tag: "someone-else".into(),
        session_id: Some(Uuid::now_v7()),
    };
    write_message_in(&mp, &unrelated).unwrap();
    watch
        .wait_for_change(Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(unread_snapshot(&mp, &workspace, &consumer)
        .unwrap()
        .is_empty());
}

#[test]
fn later_publication_wakes_subscribed_watch() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let watch = MailboxWatch::subscribe(&mp.msgs_dir).unwrap();
    assert!(unread_snapshot(&mp, &workspace, &consumer)
        .unwrap()
        .is_empty());
    let message = envelope(&workspace, &consumer);
    let writer_mp = mp.clone();
    let writer_message = message.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        ready_rx.recv().unwrap();
        write_message_in(&writer_mp, &writer_message).unwrap();
    });
    ready_tx.send(()).unwrap();
    watch
        .wait_for_change(Instant::now() + Duration::from_secs(2))
        .unwrap();
    writer.join().unwrap();
    assert_eq!(
        unread_snapshot(&mp, &workspace, &consumer).unwrap()[0].id,
        message.id
    );
}

#[test]
fn acknowledged_messages_stay_hidden_and_timeout_preserves_cursor() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let message = envelope(&workspace, &consumer);
    write_message(&paths, &message).unwrap();
    ack_messages(&paths, &workspace, consumer.id, &[message.id]).unwrap();
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let cursor_path = mp.cursors_dir.join(format!("{}.json", consumer.id));
    let before = fs::read(&cursor_path).unwrap();
    assert!(
        wait_messages(&paths, &workspace, &consumer, Duration::from_millis(20))
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(&cursor_path).unwrap(), before);
}

#[test]
fn invalidated_watch_and_corrupt_cursor_report_errors() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    fs::write(
        mp.cursors_dir.join(format!("{}.json", consumer.id)),
        b"invalid",
    )
    .unwrap();
    let error = wait_messages(&paths, &workspace, &consumer, Duration::ZERO).unwrap_err();
    assert!(format!("{error:#}").contains("parse mailbox cursor"));
    let watch = MailboxWatch::subscribe(&mp.msgs_dir).unwrap();
    fs::rename(&mp.msgs_dir, mp.workspace_dir.join("moved-msgs")).unwrap();
    let error = watch
        .wait_for_change(Instant::now() + Duration::from_secs(2))
        .unwrap_err();
    assert!(error.to_string().contains("invalidated"));
}

fn legacy_mailbox(paths: &Paths, workspace: &Path) -> MessagePaths {
    let legacy = message_paths_for_key(paths, &legacy_workspace_key(workspace));
    ensure_private_dir(&legacy.msgs_dir).unwrap();
    ensure_private_dir(&legacy.cursors_dir).unwrap();
    atomic_write_json(
        &legacy.workspace_file,
        &serde_json::json!({"workspace": workspace}),
    )
    .unwrap();
    legacy
}

#[test]
fn initial_legacy_migration_is_readable() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    ensure_workspace(&paths, &workspace).unwrap();
    let legacy = legacy_mailbox(&paths, &workspace);
    let message = envelope(&workspace, &consumer);
    write_message_in(&legacy, &message).unwrap();
    assert_eq!(
        wait_messages(&paths, &workspace, &consumer, Duration::ZERO).unwrap()[0].id,
        message.id
    );
}

#[test]
#[cfg(unix)]
fn legacy_publication_does_not_wake_stable_watch() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let stable = ensure_workspace(&paths, &workspace).unwrap();
    let legacy = legacy_mailbox(&paths, &workspace);
    let watch = MailboxWatch::subscribe(&stable.msgs_dir).unwrap();
    let later = envelope(&workspace, &consumer);
    write_message_in(&legacy, &later).unwrap();
    // The stable descriptor has no event for a publication in another tree.
    let mut pollfd = libc::pollfd {
        fd: watch.fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut pollfd, 1, 0) }, 0);
    assert!(!unread_snapshot(&stable, &workspace, &consumer)
        .unwrap()
        .iter()
        .any(|m| m.id == later.id));
    // A subsequent current operation still migrates the old writer's message.
    assert!(wait_messages(&paths, &workspace, &consumer, Duration::ZERO)
        .unwrap()
        .iter()
        .any(|m| m.id == later.id));
}

#[test]
fn lock_contention_cannot_exceed_wait_deadline() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, consumer) = fixture(root.path());
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let migration = paths
        .state_root
        .join("messages")
        .join(format!(".{}.migration.lock", workspace_key(&workspace)));
    for path in [
        migration,
        mailbox_lock_path(&mp),
        cursor_lock_path(&mp.cursors_dir, consumer.id),
    ] {
        for timeout in [Duration::ZERO, Duration::from_millis(20)] {
            assert_lock_deadline(&paths, &workspace, &consumer, &path, timeout);
        }
    }
}

fn assert_lock_deadline(
    paths: &Paths,
    workspace: &Path,
    consumer: &SessionIdentity,
    path: &Path,
    timeout: Duration,
) {
    let lock = FileLock::exclusive(path, false).unwrap();
    let paths = paths.clone();
    let workspace = workspace.to_path_buf();
    let consumer = consumer.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(wait_messages(&paths, &workspace, &consumer, timeout))
            .unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(1));
    // Release before asserting so a regression cannot strand the test worker.
    drop(lock);
    worker.join().unwrap();
    let error = result
        .expect("lock acquisition exceeded wait deadline")
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("timed out acquiring mailbox state"));
}

#[cfg(unix)]
#[test]
fn event_batches_leave_time_to_rescan_under_publication_storm() {
    let root = TempDir::new().unwrap();
    let (paths, workspace, _) = fixture(root.path());
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let watch = MailboxWatch::subscribe(&mp.msgs_dir).unwrap();
    // More than one read's worth of distinct publications is already queued.
    for index in 0..400 {
        let temporary = mp.msgs_dir.join("temporary");
        fs::write(&temporary, b"hint").unwrap();
        fs::rename(&temporary, mp.msgs_dir.join(format!("{index}.hint"))).unwrap();
    }
    watch.drain().unwrap();
    assert!(
        watch.poll(Duration::ZERO).unwrap(),
        "one wake must consume only a bounded batch"
    );
}
