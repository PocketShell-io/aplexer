use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};

fn fixture(root: &Path) -> (MessagePaths, MessageEnvelope) {
    let paths = test_paths(root);
    let mp = ensure_workspace(&paths, root).unwrap();
    let mut message = test_message(root, Uuid::now_v7());
    message.to = Recipient::Tag {
        tag: "peer".into(),
        session_id: Some(Uuid::now_v7()),
    };
    message.reply_to = Some(Uuid::now_v7());
    write_message_in(&mp, &message).unwrap();
    (mp, message)
}

#[test]
fn deferred_submission_preserves_envelope_and_does_not_ack() {
    let root = TempDir::new().unwrap();
    let (mp, mut message) = fixture(root.path());
    let outcome = submit_message_in(
        &mp,
        root.path(),
        message.id,
        |_| Ok(()),
        |seen| {
            assert_eq!(
                serde_json::to_value(seen).unwrap(),
                serde_json::to_value(&message).unwrap()
            );
            Ok(SubmissionStatus::Submitted)
        },
    )
    .unwrap();
    assert_eq!(outcome.status, SubmissionStatus::Submitted);
    assert_preserved_and_unacked(&mp, &mut message);
    let again =
        submit_message_in(&mp, root.path(), message.id, |_| panic!(), |_| panic!()).unwrap();
    assert_eq!(again.status, SubmissionStatus::AlreadySubmitted);
}

fn assert_preserved_and_unacked(mp: &MessagePaths, message: &mut MessageEnvelope) {
    message.delivery = Delivery::Pane;
    let stored = read_message_in(mp, &message.workspace, message.id).unwrap();
    assert_eq!(
        serde_json::to_value(stored).unwrap(),
        serde_json::to_value(&message).unwrap()
    );
    assert_eq!(list_messages_in(mp, &message.workspace).unwrap().len(), 1);
    let Recipient::Tag {
        session_id: Some(id),
        ..
    } = message.to
    else {
        panic!()
    };
    assert!(!read_cursor_in(mp, id).unwrap().is_acked(message.id));
}

#[test]
fn failure_before_reservation_can_retry_but_uncertain_write_cannot() {
    let root = TempDir::new().unwrap();
    let (mp, message) = fixture(root.path());
    let not_ready = submit_message_in(
        &mp,
        root.path(),
        message.id,
        |_| bail!("busy"),
        |_| panic!(),
    )
    .unwrap();
    assert_eq!(not_ready.status, SubmissionStatus::NotReady);
    assert!(!mp.msgs_dir.join(format!("{}.attempt", message.id)).exists());
    let failed = submit_message_in(
        &mp,
        root.path(),
        message.id,
        |_| Ok(()),
        |_| bail!("lost response after write"),
    )
    .unwrap();
    assert_eq!(failed.status, SubmissionStatus::DeliveryUncertain);
    let stored = read_message_in(&mp, root.path(), message.id).unwrap();
    assert_eq!(stored.delivery, Delivery::Inbox);
    let again =
        submit_message_in(&mp, root.path(), message.id, |_| panic!(), |_| panic!()).unwrap();
    assert_eq!(again.status, SubmissionStatus::DeliveryUncertain);
}

#[test]
fn acknowledged_message_does_not_write_input() {
    let root = TempDir::new().unwrap();
    let (mp, message) = fixture(root.path());
    let Recipient::Tag {
        session_id: Some(id),
        ..
    } = message.to
    else {
        panic!()
    };
    ack_messages_in(&mp, id, &[message.id]).unwrap();
    let outcome =
        submit_message_in(&mp, root.path(), message.id, |_| panic!(), |_| panic!()).unwrap();
    assert_eq!(outcome.status, SubmissionStatus::RecipientAcked);
}

#[test]
fn crash_reservation_prevents_another_submission_and_gc_removes_it() {
    let root = TempDir::new().unwrap();
    let (mp, mut message) = fixture(root.path());
    let marker = mp.msgs_dir.join(format!("{}.attempt", message.id));
    let crash = std::panic::catch_unwind(|| {
        submit_message_in(
            &mp,
            root.path(),
            message.id,
            |_| Ok(()),
            |_| panic!("interrupted transport"),
        )
    });
    assert!(crash.is_err());
    let outcome =
        submit_message_in(&mp, root.path(), message.id, |_| panic!(), |_| panic!()).unwrap();
    assert_eq!(outcome.status, SubmissionStatus::DeliveryUncertain);
    message.created_at = 1;
    write_message_file(&mp, &message);
    gc_workspace(&test_paths(root.path()), root.path()).unwrap();
    assert!(!marker.exists());
    assert!(list_messages_in(&mp, root.path()).unwrap().is_empty());
}

#[test]
fn concurrent_delivery_submits_once() {
    let root = TempDir::new().unwrap();
    let (mp, message) = fixture(root.path());
    let ready = Arc::new(Barrier::new(2));
    let writes = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..2 {
        threads.push(concurrent_submit(
            mp.clone(),
            message.clone(),
            ready.clone(),
            writes.clone(),
        ));
    }
    let statuses: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(statuses.contains(&SubmissionStatus::Submitted));
    assert!(statuses.contains(&SubmissionStatus::AlreadySubmitted));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}

fn concurrent_submit(
    mp: MessagePaths,
    message: MessageEnvelope,
    ready: Arc<Barrier>,
    writes: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<SubmissionStatus> {
    std::thread::spawn(move || {
        ready.wait();
        submit_message_in(
            &mp,
            &message.workspace,
            message.id,
            |_| Ok(()),
            |_| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(SubmissionStatus::Submitted)
            },
        )
        .unwrap()
        .status
    })
}
