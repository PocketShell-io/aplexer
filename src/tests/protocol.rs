//! Unit tests for the wire protocol.

use super::*;

#[test]
fn frame_round_trip() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, FrameKind::Data, b"a\0b").unwrap();
    let mut cursor = io::Cursor::new(bytes);
    let frame = read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(frame.kind, FrameKind::Data);
    assert_eq!(frame.payload, b"a\0b");
}

#[test]
fn bound_request_remains_readable_by_legacy_workers() {
    #[derive(Deserialize)]
    struct LegacyRequest {
        version: u16,
        request_id: String,
        #[serde(flatten)]
        operation: Operation,
    }

    let request = Request::new(Uuid::new_v4(), Operation::Ping);
    let legacy: LegacyRequest =
        serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
    assert_eq!(legacy.version, PROTOCOL_VERSION);
    assert_eq!(legacy.request_id, request.request_id);
    assert!(matches!(legacy.operation, Operation::Ping));
}

#[test]
fn attach_without_want_record_is_the_old_client_form() {
    // An old client's Attach request carries none of the opt-in fields.
    // Its serde must land on `want_record: false` so a new worker never
    // queues a `ServerEvent::RecordUpdated` for it -- the old client's
    // `serde_json::from_slice::<ServerEvent>` would hard-fail on the
    // unrecognized `event` tag and tear the attach down.
    let legacy = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "request_id": "legacy-attach",
        "op": "attach",
        "history_bytes": 4096,
    });
    let request: Request = serde_json::from_value(legacy).unwrap();
    match request.operation {
        Operation::Attach {
            want_record,
            want_screen,
            history_bytes,
            ..
        } => {
            assert!(!want_record);
            assert!(!want_screen);
            assert_eq!(history_bytes, Some(4096));
        }
        other => panic!("expected Attach, got {other:?}"),
    }
}

#[test]
fn record_updated_event_round_trips() {
    let event = ServerEvent::RecordUpdated {
        record: Box::new(SessionRecord::fixture("/ws/propagation", "renamed")),
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    match serde_json::from_slice::<ServerEvent>(&bytes).unwrap() {
        ServerEvent::RecordUpdated { record } => assert_eq!(record.tag, "renamed"),
        other => panic!("expected RecordUpdated, got {other:?}"),
    }
}
