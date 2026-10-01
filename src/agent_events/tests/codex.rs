use super::*;

#[test]
fn codex_native_message_response_item() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
    )
    .unwrap();
    let events = codex_native_events(&payload);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "message");
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
    assert_eq!(events[0].content, "hello");
}

#[test]
fn codex_native_tool_call_and_result() {
    let call: Value = serde_json::from_str(
        r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"ls"}}"#,
    )
    .unwrap();
    let events = codex_native_events(&call);
    assert_eq!(events[0].kind, "tool_call");
    assert_eq!(events[0].tool_name.as_deref(), Some("exec"));

    let output: Value = serde_json::from_str(
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"ok"}]}}"#,
    )
    .unwrap();
    let events = codex_native_events(&output);
    assert_eq!(events[0].kind, "tool_result");
    assert_eq!(events[0].tool_output.as_deref(), Some("ok"));
}

#[test]
fn codex_function_call_and_result_keep_their_call_id_and_content() {
    let call = json!({
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": "exec_command",
            "call_id": "call-42",
            "arguments": "{\"cmd\":\"ls\"}"
        }
    });
    let events = codex_native_events(&call);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool_call");
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
    assert_eq!(events[0].tool_name.as_deref(), Some("exec_command"));
    assert_eq!(events[0].tool_input.as_deref(), Some("{\"cmd\":\"ls\"}"));
    assert_eq!(events[0].metadata["tool_call_id"], "call-42");

    let result = json!({
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": "call-42",
            "output": " first line\n  second line\n"
        }
    });
    let events = codex_native_events(&result);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool_result");
    assert_eq!(
        events[0].tool_output.as_deref(),
        Some(" first line\n  second line\n")
    );
    assert_eq!(events[0].metadata["tool_call_id"], "call-42");
}

#[test]
fn codex_function_call_empty_result_is_still_emitted() {
    let result = json!({
        "type": "response_item",
        "payload": {"type": "function_call_output", "call_id": "call-empty", "output": ""}
    });
    let events = codex_native_events(&result);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool_result");
    assert_eq!(events[0].tool_output.as_deref(), Some(""));
    assert_eq!(events[0].metadata["tool_call_id"], "call-empty");
}

#[test]
fn codex_function_call_object_arguments_are_kept_verbatim() {
    // A rollout that ships `arguments` as a structured value instead of the
    // usual JSON-encoded string (both shapes exist across codex versions)
    // must still surface the command, not be dropped for lacking a string.
    let call = json!({
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": "shell",
            "call_id": "call-obj",
            "arguments": {"command": ["bash", "-lc", "ls"]}
        }
    });
    let events = codex_native_events(&call);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool_call");
    assert_eq!(events[0].tool_name.as_deref(), Some("shell"));
    assert_eq!(
        events[0].tool_input.as_deref(),
        Some(r#"{"command":["bash","-lc","ls"]}"#)
    );
    assert_eq!(events[0].metadata["tool_call_id"], "call-obj");
}

#[test]
fn codex_rollout_stream_keeps_function_calls_and_results_in_order() {
    // The zoom-rollout acceptance shape (issue #20): a real rollout is
    // mostly `response_item` function rows -- 787 `function_call` and 762
    // `function_call_output` there against 44 message rows -- and a parser
    // that recognizes only the `custom_tool_call` alias emits a transcript
    // that looks readable while omitting nearly all of the agent's tool
    // work. This pins file-level coverage: calls and results stream out in
    // file order, each stamped with the `call_id` that pairs them.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    let rows = [
        r#"{"timestamp":"2026-09-21T10:00:00.000Z","type":"session_meta","payload":{"id":"thread-zoom","cwd":"/tmp/zoom"}}"#,
        r#"{"timestamp":"2026-09-21T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"insert the figures"}]}}"#,
        r#"{"timestamp":"2026-09-21T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Working on it."}]}}"#,
        r#"{"timestamp":"2026-09-21T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"command\":\"ls figures\"}"}}"#,
        r#"{"timestamp":"2026-09-21T10:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"fig1.png\nfig2.png"}}"#,
        r#"{"timestamp":"2026-09-21T10:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"inserting"}}"#,
        r#"{"timestamp":"2026-09-21T10:00:06.000Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","call_id":"call-2","arguments":"{\"patch\":\"...\"}"}}"#,
        r#"{"timestamp":"2026-09-21T10:00:07.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-2","output":"Done!"}}"#,
    ];
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    drop(file);

    let events = read_transcript_events("zcodex", &path).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind).collect();
    // The event_msg progress row mirrors response_item content and is
    // deliberately not re-emitted; the session_meta row is metadata.
    assert_eq!(
        kinds,
        [
            "message",
            "message",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result"
        ]
    );
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[1].content, "Working on it.");
    assert_eq!(events[2].tool_name.as_deref(), Some("exec_command"));
    assert_eq!(events[2].metadata["tool_call_id"], "call-1");
    assert_eq!(events[3].tool_output.as_deref(), Some("fig1.png\nfig2.png"));
    assert_eq!(events[3].metadata["tool_call_id"], "call-1");
    assert_eq!(events[4].tool_name.as_deref(), Some("apply_patch"));
    assert_eq!(events[5].tool_output.as_deref(), Some("Done!"));
    assert_eq!(events[5].metadata["tool_call_id"], "call-2");
    // Sequences stay dense across the whole file, so an `--after` cursor
    // from a handoff report resumes at the right row.
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, sequence as u64);
    }
}

#[test]
fn codex_native_continuation_from_session_meta() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"session_meta","payload":{"id":"thread-abc","cwd":"/tmp/x"}}"#,
    )
    .unwrap();
    assert_eq!(
        codex_native_continuation(&payload).as_deref(),
        Some("thread-abc")
    );
    assert_eq!(codex_native_cwd(&payload).as_deref(), Some("/tmp/x"));
}

#[test]
fn zcodex_rides_the_codex_machinery() {
    // Family identification: the variant parses with codex's wire format...
    assert!(matches!(
        wire_format_for("zcodex").unwrap(),
        WireFormat::CodexNative
    ));
    // ...while events emitted from its rollout carry the variant's own
    // engine id, not the family's.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-09-06T12:00:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    let events = read_transcript_events("zcodex", &path).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].engine, "zcodex");
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
    assert_eq!(events[0].content, "hello");

    // Location rides the codex heuristic too: the CODEX_HOME sessions
    // tree, disambiguated by the rollout's own session_meta cwd.
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions/2026/09/06");
    std::fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join("thread-z.jsonl");
    std::fs::write(
        &rollout,
        concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-z","cwd":"/tmp/zcodex-work"}}"#,
            "\n",
        ),
    )
    .unwrap();
    let mut env = BTreeMap::new();
    env.insert("CODEX_HOME".to_string(), home.path().display().to_string());
    let found = locate_transcript("zcodex", Path::new("/tmp/zcodex-work"), now_ms(), &env).unwrap();
    assert_eq!(found, rollout);
}
