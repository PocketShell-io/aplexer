//! Unit tests for the awareness hook: payload schemas per engine (the
//! real native shapes, including non-UUID harness conversation ids),
//! session binding, subagent refusal, native-id binding/conflict,
//! fingerprint/cooldown admission with first-update bootstrap, and
//! composition. The full `a context hook` path through the real CLI is
//! covered by tests/awareness_cli.rs.

use super::*;
use crate::{atomic_write_json, SessionRecord};
use payload::HookPayload;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------

fn test_paths() -> (TempDir, Paths) {
    let dir = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: dir.path().join("runtime"),
        state_root: dir.path().join("state"),
        config_file: dir.path().join("config.toml"),
    };
    // list_records scans this dir; an empty registry must be a valid one.
    crate::ensure_private_dir(&paths.state_root.join("sessions")).unwrap();
    (dir, paths)
}

fn install_record(paths: &Paths, workspace: &std::path::Path, tag: &str, engine: &str) -> Uuid {
    let mut record = SessionRecord::fixture(workspace, tag);
    record.engine = engine.to_string();
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    crate::ensure_private_dir(
        &paths
            .state_root
            .join("sessions")
            .join(record.id.to_string()),
    )
    .unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();
    record.id
}

fn envelope(id: Uuid, body: &str, created_at: u64) -> MessageEnvelope {
    MessageEnvelope {
        schema_version: crate::messaging::MESSAGE_SCHEMA_VERSION,
        id,
        workspace: std::path::PathBuf::from("/tmp/ws"),
        created_at,
        from: crate::messaging::MessageFrom::anonymous(),
        to: crate::messaging::Recipient::Broadcast { broadcast: true },
        kind: "note".to_string(),
        reply_to: None,
        body: body.to_string(),
        data: None,
        delivery: crate::messaging::Delivery::Inbox,
    }
}

fn payload_from(json: serde_json::Value) -> HookPayload {
    payload::parse_hook_payload(serde_json::to_vec(&json).unwrap().as_slice()).unwrap()
}

fn no_payload() -> HookPayload {
    HookPayload::default()
}

fn bound(id: Uuid, workspace: &std::path::Path) -> binding::BoundSession {
    binding::BoundSession {
        id,
        workspace: workspace.to_path_buf(),
        tag: "builder".to_string(),
        engine: "shell".to_string(),
    }
}

// ---------------------------------------------------------------------
// Payload schemas (real native shapes)
// ---------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn claude_post_tool_use_payload_parses_snake_case() {
    let payload = payload_from(serde_json::json!({
        "session_id": "01950000-0000-7000-8000-000000000001",
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": "/tmp/outside/file.rs", "content": "secret"},
        "cwd": "/tmp/ws"
    }));
    assert_eq!(payload.event.as_deref(), Some("PostToolUse"));
    assert_eq!(
        payload.session_id.as_deref(),
        Some("01950000-0000-7000-8000-000000000001")
    );
    assert!(!payload.subagent);
    assert!(payload
        .tool_paths
        .contains(&std::path::PathBuf::from("/tmp/outside/file.rs")));
}

#[test]
fn native_conversation_ids_stay_opaque_strings() {
    // OpenCode session ids are not UUIDs; Antigravity calls the field
    // conversationId. All are preserved verbatim, never parsed.
    let opencode = payload_from(serde_json::json!({
        "hook_event_name": "tool.execute.after", "session_id": "ses_native_non_uuid"
    }));
    assert_eq!(opencode.session_id.as_deref(), Some("ses_native_non_uuid"));
    let antigravity = payload_from(serde_json::json!({
        "invocationNum": 0, "conversationId": "conv_abc123"
    }));
    assert_eq!(antigravity.session_id.as_deref(), Some("conv_abc123"));
    assert_eq!(antigravity.event, None, "antigravity carries no event name");
}

#[test]
fn grok_payload_parses_camel_case() {
    let payload = payload_from(serde_json::json!({
        "sessionId": "01950000-0000-7000-8000-000000000002",
        "hookEventName": "PostToolUse",
        "toolName": "bash"
    }));
    assert_eq!(payload.event.as_deref(), Some("PostToolUse"));
    assert_eq!(
        payload.session_id.as_deref(),
        Some("01950000-0000-7000-8000-000000000002")
    );
}

#[test]
fn subagent_markers_are_flagged() {
    for marker in ["agent_id", "agentId"] {
        let mut doc = serde_json::Map::new();
        doc.insert(
            "hook_event_name".to_string(),
            serde_json::json!("PostToolUse"),
        );
        doc.insert(marker.to_string(), serde_json::json!("subagent-7"));
        let payload = payload_from(serde_json::Value::Object(doc));
        assert!(payload.subagent, "{marker}");
    }
}

#[test]
fn relative_and_non_string_path_arguments_are_ignored() {
    let payload = payload_from(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "tool_input": {
            "file_path": "relative/file.rs",
            "path": 42,
            "command": "cat /etc/passwd > /tmp/other"
        }
    }));
    assert!(payload.tool_paths.is_empty());
}

#[cfg(unix)]
#[test]
fn antigravity_workspace_paths_are_collected() {
    let payload = payload_from(serde_json::json!({
        "invocationNum": 0,
        "workspacePaths": ["/tmp/other-ws", "relative", 7]
    }));
    assert_eq!(
        payload.workspace_paths,
        vec![std::path::PathBuf::from("/tmp/other-ws")]
    );
}

#[test]
fn oversized_payload_is_refused_not_truncated() {
    let big = format!(
        "\"{}\"",
        "x".repeat(super::payload::MAX_HOOK_INPUT_BYTES as usize + 10)
    );
    let err = payload::parse_hook_payload(big.as_bytes()).expect_err("oversized payload must fail");
    assert!(err.to_string().contains("exceeds"), "{err}");
}

#[test]
fn malformed_payload_is_refused() {
    assert!(payload::parse_hook_payload("not json".as_bytes()).is_err());
}

// ---------------------------------------------------------------------
// Per-engine gating and emission
// ---------------------------------------------------------------------

#[test]
fn injectable_events_follow_each_engines_schema() {
    // Claude/Codex: SessionStart (startup) + prompts + tools.
    for engine in ["claude", "codex"] {
        assert!(
            injectable_event(engine, Some("SessionStart"))
                .unwrap()
                .unwrap()
                .1
        );
        assert!(
            !injectable_event(engine, Some("UserPromptSubmit"))
                .unwrap()
                .unwrap()
                .1
        );
        assert!(injectable_event(engine, Some("PostToolUse"))
            .unwrap()
            .is_some());
        assert!(injectable_event(engine, Some("Stop")).unwrap().is_none());
    }
    // Grok: PostToolUse only; startup stdout is ignored by the CLI.
    assert!(injectable_event("grok", Some("SessionStart"))
        .unwrap()
        .is_none());
    assert!(
        !injectable_event("grok", Some("PostToolUse"))
            .unwrap()
            .unwrap()
            .1
    );
    // Gemini renames the boundaries.
    assert!(
        injectable_event("gemini", Some("SessionStart"))
            .unwrap()
            .unwrap()
            .1
    );
    assert!(injectable_event("gemini", Some("BeforeAgent"))
        .unwrap()
        .is_some());
    assert!(injectable_event("gemini", Some("AfterTool"))
        .unwrap()
        .is_some());
    assert!(injectable_event("gemini", Some("AfterAgent"))
        .unwrap()
        .is_none());
    // Antigravity injects on PreInvocation, inferred from the engine when
    // the native payload carries no event name; PostToolUse cannot inject.
    assert_eq!(
        injectable_event("antigravity", None).unwrap(),
        Some(("PreInvocation", true))
    );
    assert_eq!(
        injectable_event("antigravity", Some("PreInvocation")).unwrap(),
        Some(("PreInvocation", true))
    );
    assert!(injectable_event("antigravity", Some("PostToolUse"))
        .unwrap()
        .is_none());
    // OpenCode's plugin shells out on tool.execute.after.
    assert_eq!(
        injectable_event("opencode", Some("tool.execute.after")).unwrap(),
        Some(("tool.execute.after", false))
    );
    assert!(injectable_event("opencode", None).unwrap().is_some());
    // Unknown engines are a wiring bug, surfaced as an error.
    assert!(injectable_event("zodex", Some("PostToolUse")).is_err());
}

#[test]
fn emission_envelopes_match_each_engine() {
    let claude = emit_context("claude", "PostToolUse", "hi");
    let doc: serde_json::Value = serde_json::from_str(&claude).unwrap();
    assert_eq!(doc["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_eq!(doc["hookSpecificOutput"]["additionalContext"], "hi");

    let gemini = emit_context("gemini", "AfterTool", "hi");
    let doc: serde_json::Value = serde_json::from_str(&gemini).unwrap();
    assert_eq!(doc["hookSpecificOutput"]["hookEventName"], "AfterTool");

    // Antigravity's verified shape, exactly: no type/message wrapper.
    let antigravity = emit_context("antigravity", "PreInvocation", "hi");
    let doc: serde_json::Value = serde_json::from_str(&antigravity).unwrap();
    assert_eq!(doc["injectSteps"][0]["ephemeralMessage"], "hi");
    assert!(doc["injectSteps"][0].get("type").is_none());
    assert!(doc["injectSteps"][0].get("message").is_none());
    assert_eq!(
        doc["injectSteps"].as_array().unwrap().len(),
        1,
        "exactly one inject step"
    );

    assert_eq!(emit_context("opencode", "tool.execute.after", "hi"), "hi");
}

#[test]
fn consumer_keys_are_per_engine_event() {
    assert_eq!(consumer_key("claude", "PostToolUse"), "claude:PostToolUse");
    assert_ne!(
        consumer_key("claude", "PostToolUse"),
        consumer_key("claude", "UserPromptSubmit")
    );
    assert_eq!(
        consumer_key("opencode", "tool.execute.after"),
        "opencode:tool.execute.after"
    );
}

// ---------------------------------------------------------------------
// Binding
// ---------------------------------------------------------------------

#[test]
fn binding_requires_a_discovered_session_and_a_record() {
    let (_dir, paths) = test_paths();
    // No ambient id: nothing to bind.
    assert!(binding::bind_discovered(&paths, "claude", None)
        .unwrap()
        .is_none());
    // Ambient id without a record: still nothing.
    assert!(
        binding::bind_discovered(&paths, "claude", Some(Uuid::new_v4()))
            .unwrap()
            .is_none()
    );
    let workspace = std::path::PathBuf::from("/tmp/binding-ws");
    let record_id = install_record(&paths, &workspace, "worker", "claude");
    let bound = binding::bind_discovered(&paths, "claude", Some(record_id))
        .unwrap()
        .unwrap();
    assert_eq!(bound.id, record_id);
    assert_eq!(bound.workspace, workspace);
    assert_eq!(bound.tag, "worker");
}

#[test]
fn engine_matching_follows_the_shell_and_zcodex_rules() {
    let (_dir, paths) = test_paths();
    let workspace = std::path::PathBuf::from("/tmp/binding-ws");
    let shell = install_record(&paths, &workspace, "sh", "shell");
    let zcodex = install_record(&paths, &workspace, "zcy", "zcodex");
    // A shell record hosts any CLI's hook.
    for engine in [
        "claude",
        "codex",
        "grok",
        "gemini",
        "antigravity",
        "opencode",
    ] {
        assert!(
            binding::bind_discovered(&paths, engine, Some(shell))
                .unwrap()
                .is_some(),
            "shell record must answer the {engine} hook"
        );
    }
    // A zcodex record answers the codex hook (shared CODEX_HOME), not others.
    assert!(binding::bind_discovered(&paths, "codex", Some(zcodex))
        .unwrap()
        .is_some());
    assert!(binding::bind_discovered(&paths, "claude", Some(zcodex))
        .unwrap()
        .is_none());
}

#[test]
fn native_payload_ids_never_gate_binding() {
    // The payload's harness conversation id lives in another namespace;
    // binding happens through the discovered aplexer record alone. A
    // claude record with a native payload id unlike any aplexer id still
    // binds (state::admit handles conflict detection).
    let (_dir, paths) = test_paths();
    let workspace = std::path::PathBuf::from("/tmp/binding-ws");
    let record_id = install_record(&paths, &workspace, "worker", "claude");
    assert!(binding::bind_discovered(&paths, "claude", Some(record_id))
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------
// Native-id binding, fingerprints, cooldown (admission)
// ---------------------------------------------------------------------

#[test]
fn fingerprints_exclude_volatile_fields() {
    let id = Uuid::new_v4();
    let stable = envelope(id, "body", 1_000);
    let same_id_new_timestamp = envelope(id, "body", 9_999_999);
    assert_eq!(
        state::fingerprint("rendered", std::slice::from_ref(&stable), &[]),
        state::fingerprint("rendered", &[same_id_new_timestamp], &[])
    );
    let other = envelope(Uuid::new_v4(), "body", 1_000);
    assert_ne!(
        state::fingerprint("rendered", &[stable], &[]),
        state::fingerprint("rendered", &[other], &[])
    );
    assert_ne!(
        state::fingerprint("rendered", &[], &[]),
        state::fingerprint("rendered changed", &[], &[])
    );
}

#[test]
fn first_admission_emits_with_bootstrap_then_rests() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("claude", "PostToolUse");
    let first = state::admit(&paths, session, "claude", &key, Some("conv-a"), 1, false).unwrap();
    assert!(first.emit);
    assert!(first.needs_bootstrap, "grok/opencode-style first tool call");
    let second = state::admit(&paths, session, "claude", &key, Some("conv-a"), 1, false).unwrap();
    assert!(!second.emit, "unchanged fingerprint rests");
    let changed = state::admit(&paths, session, "claude", &key, Some("conv-a"), 2, false).unwrap();
    assert!(changed.emit, "real change injects immediately");
    assert!(!changed.needs_bootstrap, "bootstrap is one-shot per engine");
}

#[test]
fn consumers_and_sessions_are_isolated() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    assert!(
        state::admit(
            &paths,
            session,
            "claude",
            "claude:PostToolUse",
            Some("c"),
            1,
            false
        )
        .unwrap()
        .emit
    );
    assert!(
        !state::admit(
            &paths,
            session,
            "claude",
            "claude:PostToolUse",
            Some("c"),
            1,
            false
        )
        .unwrap()
        .emit
    );
    // A different consumer kind and a different session start unsuppressed.
    assert!(
        state::admit(
            &paths,
            session,
            "claude",
            "claude:UserPromptSubmit",
            Some("c"),
            1,
            false
        )
        .unwrap()
        .emit
    );
    assert!(
        state::admit(
            &paths,
            Uuid::new_v4(),
            "claude",
            "claude:PostToolUse",
            Some("c"),
            1,
            false
        )
        .unwrap()
        .emit
    );
}

#[test]
fn startup_always_emits_and_marks_the_engine_seen() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("claude", "SessionStart");
    let startup = state::admit(&paths, session, "claude", &key, Some("c"), 1, true).unwrap();
    assert!(startup.emit);
    assert!(startup.needs_bootstrap);
    let again = state::admit(&paths, session, "claude", &key, Some("c"), 1, true).unwrap();
    assert!(again.emit, "startup is never fingerprint-suppressed");
    assert!(again.needs_bootstrap);
    // The engine is now seen: a later first update carries context only.
    let update = state::admit(
        &paths,
        session,
        "claude",
        "claude:PostToolUse",
        Some("c"),
        1,
        false,
    )
    .unwrap();
    assert!(update.emit, "first update for this consumer emits");
    assert!(!update.needs_bootstrap);
}

#[test]
fn conflicting_native_conversation_ids_are_rejected() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("claude", "PostToolUse");
    assert!(
        state::admit(&paths, session, "claude", &key, Some("conv-one"), 1, false)
            .unwrap()
            .emit
    );
    // The same conversation keeps flowing.
    assert!(
        state::admit(&paths, session, "claude", &key, Some("conv-one"), 2, false)
            .unwrap()
            .emit
    );
    // A different conversation under the same session environment (a
    // nested agent, a second CLI) gets nothing -- even at startup.
    assert!(
        !state::admit(&paths, session, "claude", &key, Some("conv-two"), 3, false)
            .unwrap()
            .emit
    );
    assert!(
        !state::admit(&paths, session, "claude", &key, Some("conv-two"), 3, true)
            .unwrap()
            .emit
    );
    // Engines bind independently: another engine's conversation is fine.
    assert!(
        state::admit(
            &paths,
            session,
            "codex",
            "codex:PostToolUse",
            Some("conv-two"),
            1,
            false
        )
        .unwrap()
        .emit
    );
    // Opaque non-UUID ids bind fine too.
    assert!(
        state::admit(
            &paths,
            session,
            "opencode",
            "opencode:tool.execute.after",
            Some("ses_x"),
            1,
            false
        )
        .unwrap()
        .emit
    );
}

#[test]
fn hostile_native_ids_are_rejected_without_poisoning_state() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("claude", "PostToolUse");
    // A 1 MiB-sized id (the payload cap) would blow the 64 KiB state cap.
    let huge = "x".repeat(600);
    assert!(
        !state::admit(
            &paths,
            session,
            "claude",
            &key,
            Some(huge.as_str()),
            1,
            true
        )
        .unwrap()
        .emit
    );
    let hostile = "bad\u{7}id";
    assert!(
        !state::admit(&paths, session, "claude", &key, Some(hostile), 1, true)
            .unwrap()
            .emit
    );
    // Nothing was written, and a well-formed conversation binds cleanly.
    let state_file = paths
        .state_root
        .join("awareness")
        .join(format!("{session}.json"));
    assert!(!state_file.exists(), "hostile ids must not touch state");
    // A 512-byte id is exactly at the accepted bound and binds normally.
    let edge = "y".repeat(512);
    assert!(
        state::admit(
            &paths,
            session,
            "claude",
            &key,
            Some(edge.as_str()),
            1,
            false
        )
        .unwrap()
        .emit
    );
    // The same long conversation keeps flowing; a different one conflicts.
    assert!(
        state::admit(
            &paths,
            session,
            "claude",
            &key,
            Some(edge.as_str()),
            2,
            false
        )
        .unwrap()
        .emit
    );
    assert!(
        !state::admit(&paths, session, "claude", &key, Some("conv-two"), 3, false)
            .unwrap()
            .emit
    );
}

#[test]
fn cooldown_rearm_allows_a_periodic_reminder() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("grok", "PostToolUse");
    assert!(
        state::admit(&paths, session, "grok", &key, Some("c"), 1, false)
            .unwrap()
            .emit
    );
    assert!(
        !state::admit(&paths, session, "grok", &key, Some("c"), 1, false)
            .unwrap()
            .emit
    );
    // Age the last injection past the cooldown: the same fingerprint may
    // remind again.
    let path = paths
        .state_root
        .join("awareness")
        .join(format!("{session}.json"));
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["consumers"][&key]["at"] = serde_json::json!(0);
    atomic_write_json(&path, &doc).unwrap();
    let rearm = state::admit(&paths, session, "grok", &key, Some("c"), 1, false).unwrap();
    assert!(rearm.emit);
    assert!(!rearm.needs_bootstrap, "bootstrap stays one-shot");
}

#[test]
fn lock_contention_is_answered_quietly_for_every_event() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("claude", "PostToolUse");
    let lock_path = paths
        .state_root
        .join("awareness")
        .join(format!("{session}.lock"));
    let held = crate::FileLock::exclusive(&lock_path, true).unwrap();
    // Another holder wins: no output, no error, no wait -- startup
    // included, because contention bypass must never skip the native-id
    // identity guard either.
    assert!(
        !state::admit(&paths, session, "claude", &key, Some("c"), 1, false)
            .unwrap()
            .emit
    );
    assert!(
        !state::admit(&paths, session, "claude", &key, Some("c"), 1, true)
            .unwrap()
            .emit
    );
    drop(held);
    assert!(
        state::admit(&paths, session, "claude", &key, Some("c"), 1, false)
            .unwrap()
            .emit
    );
}

#[test]
fn poisoned_state_never_erases_the_binding() {
    let (_dir, paths) = test_paths();
    let session = Uuid::new_v4();
    let key = consumer_key("claude", "PostToolUse");
    // Establish a native binding.
    assert!(
        state::admit(&paths, session, "claude", &key, Some("conv-real"), 1, false)
            .unwrap()
            .emit
    );
    let path = paths
        .state_root
        .join("awareness")
        .join(format!("{session}.json"));
    // Corrupt it: a fire must go quiet, and the file must stay corrupt
    // (a fresh rewrite would let a different conversation claim the slot).
    std::fs::write(&path, b"{broken").unwrap();
    let poisoned = state::admit(&paths, session, "claude", &key, Some("conv-other"), 2, true);
    assert!(!poisoned.unwrap().emit);
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
}

// ---------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------

#[test]
fn startup_composition_carries_bootstrap_and_unread_ids_not_bodies() {
    let session = bound(Uuid::new_v4(), std::path::Path::new("/tmp/ws"));
    let unread = vec![
        envelope(Uuid::new_v4(), "the launch codes are 12345", 1),
        envelope(Uuid::new_v4(), "second", 2),
    ];
    let text = render::compose(&render::Composition {
        bootstrap: true,
        session: &session,
        rendered: "Peers: none",
        unread: &unread,
        mailboxes: &[],
        foreign: &[],
    });
    assert!(text.contains("a context"), "{text}");
    assert!(text.contains("a work join"), "{text}");
    assert!(text.contains("a message ack"), "{text}");
    assert!(text.contains("Peers: none"), "{text}");
    assert!(text.contains("2 unread peer message(s)"), "{text}");
    for message in &unread {
        assert!(text.contains(&message.id.to_string()), "{text}");
        assert!(!text.contains(message.body.as_str()), "{text}");
    }
}

#[test]
fn update_composition_is_rendered_plus_refs_and_foreign_blocks() {
    let session = bound(Uuid::new_v4(), std::path::Path::new("/tmp/ws"));
    let foreign = vec![(
        std::path::PathBuf::from("/tmp/other-ws"),
        "Peers: reviewer (review)".to_string(),
    )];
    let text = render::compose(&render::Composition {
        bootstrap: false,
        session: &session,
        rendered: "Workspace: /tmp/ws",
        unread: &[],
        mailboxes: &[],
        foreign: &foreign,
    });
    assert!(text.contains("Workspace: /tmp/ws"), "{text}");
    assert!(text.contains("/tmp/other-ws"), "{text}");
    assert!(text.contains("Peers: reviewer (review)"), "{text}");
    assert!(text.contains("a work join"), "{text}");
    // No bootstrap on plain updates.
    assert!(!text.contains("bootstrap"), "{text}");
}

#[test]
fn multiworkspace_mailboxes_are_named_at_bootstrap() {
    let session = bound(Uuid::new_v4(), std::path::Path::new("/tmp/ws"));
    let text = render::compose(&render::Composition {
        bootstrap: true,
        session: &session,
        rendered: "",
        unread: &[],
        mailboxes: &[
            std::path::PathBuf::from("/tmp/ws"),
            std::path::PathBuf::from("/tmp/other"),
        ],
        foreign: &[],
    });
    assert!(text.contains("2 workspace mailboxes"), "{text}");
    assert!(text.contains("/tmp/other"), "{text}");
}

// ---------------------------------------------------------------------
// Foreign destination resolution
// ---------------------------------------------------------------------

#[test]
fn foreign_destinations_resolve_to_nearest_existing_directory() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home-ws");
    std::fs::create_dir_all(&home).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    // A file outside resolves to its directory.
    let file = outside.join("real.rs");
    std::fs::write(&file, "x").unwrap();
    assert_eq!(render::outside_workspace(&file, &home).unwrap(), outside);

    // A not-yet-created file resolves to its nearest existing ancestor.
    let ghost = outside.join("new-dir").join("new.rs");
    assert_eq!(render::outside_workspace(&ghost, &home).unwrap(), outside);

    // Inside the session's workspace: never foreign.
    let inside = home.join("file.rs");
    assert!(render::outside_workspace(&inside, &home).is_none());

    // Hostile control-character paths are refused.
    let hostile = std::path::PathBuf::from("/tmp/bad\u{7}path");
    assert!(render::outside_workspace(&hostile, &home).is_none());
}

#[test]
fn foreign_peers_render_is_bounded_and_deduplicated() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home-ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(home.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let (_guard, paths) = test_paths();
    let id = install_record(&paths, &home, "builder", "shell");
    let session = bound(id, &home);
    let mut payload = no_payload();
    payload.tool_paths = vec![home.join("inside.rs"), outside.join("a.rs")];
    payload.workspace_paths = vec![outside.clone()];
    let foreign = render::foreign_peers(&paths, &session, &payload).unwrap();
    // The inside-workspace path never appears; the outside one appears once.
    assert_eq!(foreign.len(), 1, "{foreign:?}");
    assert_eq!(foreign[0].0, outside);
    // The destination's peer render flowed through core's projection.
    assert!(
        foreign[0].1.contains("Workspace coordination for"),
        "{foreign:?}"
    );
    assert!(
        foreign[0].1.contains(outside.to_str().unwrap()),
        "{foreign:?}"
    );
}

#[cfg(unix)]
#[test]
fn opencode_plugin_payload_contract_is_stable() {
    // The plugin sends the payload the parser actually reads; guard the
    // contract so a JS-side key rename fails a test instead of silently
    // producing context-less hooks.
    let payload = payload_from(serde_json::json!({
        "hook_event_name": "tool.execute.after",
        "tool_name": "bash",
        "session_id": "ses_native",
        "tool_input": {"cwd": "/tmp/outside"}
    }));
    assert_eq!(payload.event.as_deref(), Some("tool.execute.after"));
    assert_eq!(payload.tool_paths.len(), 1);
}
