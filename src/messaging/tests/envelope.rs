use super::*;

#[test]
fn recipient_shapes_round_trip() {
    let tag = Recipient::Tag {
        tag: "review".into(),
        session_id: None,
    };
    let json = serde_json::to_value(&tag).unwrap();
    assert_eq!(json, serde_json::json!({"tag": "review"}));
    let broadcast = Recipient::Broadcast { broadcast: true };
    assert_eq!(
        serde_json::to_value(&broadcast).unwrap(),
        serde_json::json!({"broadcast": true})
    );
    let engine = Recipient::Engine {
        engine: "codex".into(),
    };
    assert_eq!(
        serde_json::to_value(&engine).unwrap(),
        serde_json::json!({"engine": "codex"})
    );
    let back: Recipient = serde_json::from_value(json).unwrap();
    matches!(back, Recipient::Tag { .. });
}

#[test]
fn sender_without_workspace_from_older_messages_still_deserializes() {
    let sender: MessageFrom = serde_json::from_value(serde_json::json!({
        "session_id": Uuid::from_u128(1),
        "tag": "review",
        "external": false
    }))
    .unwrap();
    assert_eq!(sender.workspace, None);
}

#[test]
fn body_size_cap_rejects_oversized() {
    let big = "x".repeat(MAX_BODY_BYTES + 1);
    assert!(check_body_size(&big).is_err());
    let ok = "x".repeat(MAX_BODY_BYTES);
    assert!(check_body_size(&ok).is_ok());
}
