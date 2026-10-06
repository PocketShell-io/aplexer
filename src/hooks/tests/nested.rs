use super::*;

#[test]
fn merge_creates_hooks_object_from_empty() {
    let doc = merged(&CLAUDE_EVENTS, json!({}));
    // One group per (event, state) wiring; an event wired for two states
    // (SessionStart: working + awareness) carries two groups.
    for (event, state) in CLAUDE_EVENTS {
        let groups = doc["hooks"][event].as_array().unwrap();
        assert!(
            groups.iter().any(|g| group_reports(g, state)),
            "event {event} state {state}"
        );
    }
    let session_start = doc["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(session_start.len(), 2);
    assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
}

#[test]
fn merge_is_idempotent() {
    let once = merged(&CLAUDE_EVENTS, json!({}));
    let mut twice = once.clone();
    let changed = merge_nested_hooks(&mut twice, &CLAUDE_EVENTS, A_BIN).unwrap();
    assert_eq!(changed, 0);
    assert_eq!(once, twice);
}

#[cfg(unix)]
#[test]
fn awareness_hook_is_bounded_and_installs_one_source_per_engine() {
    let doc = merged(&CLAUDE_EVENTS, json!({}));
    for (event, state) in CLAUDE_EVENTS {
        if let Some(engine) = state.strip_prefix("awareness:") {
            let group = doc["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| group_reports(g, state))
                .unwrap();
            let command = group["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.contains(&format!("context hook --engine {engine}")),
                "{command}"
            );
            // The `|| true` guard, then the content marker.
            assert!(
                command.contains(" || true # aplexer-managed-awareness-hook-v1"),
                "{command}"
            );
            // Seconds-budget hosts get 30.
            assert_eq!(group["hooks"][0]["timeout"], 30);
        }
    }
    // Gemini's timeout unit is milliseconds (documented default 60000),
    // so its budget is 30000ms -- a bare 30 there would kill the hook
    // after 30ms.
    let doc = merged(&GEMINI_EVENTS, json!({}));
    let group = doc["hooks"]["SessionStart"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| group_reports(g, "awareness:gemini"))
        .unwrap();
    assert_eq!(group["hooks"][0]["timeout"], 30000);
    // State-report hooks carry no timeout: `|| true` is the guard.
    let working = doc["hooks"]["SessionStart"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| group_reports(g, "working"))
        .unwrap();
    assert!(working["hooks"][0].get("timeout").is_none());
}

#[test]
fn install_migrates_legacy_notice_commands_by_marker() {
    // A config written by an older binary carries the legacy managed
    // inbox-notice commands. Installing must replace exactly those (by
    // content marker, any engine) with the awareness wiring, keep
    // state-report and foreign hooks, and count the migration as a
    // change so `a init` reports it.
    let legacy = json!({"hooks": {
        "PostToolUse": [{"hooks": [
            {"type": "command", "command": format!("{} message hook-notice --engine claude 2>/dev/null || true # aplexer-managed-inbox-hook-v1", A_BIN)},
            {"type": "command", "command": "foreign-post-tool"}
        ]}],
        "Stop": [{"hooks": [
            {"type": "command", "command": format!("{} state-report idle || true", A_BIN)}
        ]}]
    }});
    let mut doc = legacy;
    let changed = merge_nested_hooks(&mut doc, &CLAUDE_EVENTS, A_BIN).unwrap();
    assert!(changed > 0);
    let commands: Vec<String> = doc["hooks"]["PostToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| {
            g["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["command"].as_str().unwrap_or("").to_string())
        })
        .collect();
    assert!(
        !commands
            .iter()
            .any(|c| c.contains("hook-notice") || c.contains("aplexer-managed-inbox-hook-v1")),
        "legacy notice survived: {commands:?}"
    );
    assert!(commands
        .iter()
        .any(|c| c.contains("context hook --engine claude")));
    assert!(commands.iter().any(|c| c.contains("foreign-post-tool")));
    assert!(group_reports(&doc["hooks"]["Stop"][0], "idle"));
    assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
    // Idempotent: migrating again changes nothing.
    assert_eq!(
        merge_nested_hooks(&mut doc, &CLAUDE_EVENTS, A_BIN).unwrap(),
        0
    );
}

#[test]
fn migration_counts_inner_removal_in_a_mixed_group() {
    // Regression: a PostToolUse group holding the legacy notice AND a
    // foreign hook, with the current awareness entry already installed.
    // Removing the notice leaves the group alive but must still count as
    // a change -- otherwise the installer skips the write and the legacy
    // entry survives forever.
    let legacy_command = format!(
        "{A_BIN} message hook-notice --engine claude 2>/dev/null || true # aplexer-managed-inbox-hook-v1"
    );
    let mut doc = json!({"hooks": {"PostToolUse": [{"hooks": [
        {"type": "command", "command": legacy_command},
        {"type": "command", "command": "foreign-post-tool"},
        {"type": "command", "command": format!("{A_BIN} context hook --engine claude 2>/dev/null || true # aplexer-managed-awareness-hook-v1")}
    ]}]}});
    let changed = merge_nested_hooks(&mut doc, &CLAUDE_EVENTS, A_BIN).unwrap();
    assert!(changed > 0, "inner notice removal must count as a change");
    let commands: Vec<String> = doc["hooks"]["PostToolUse"][0]["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["command"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert!(commands.iter().any(|c| c.contains("foreign-post-tool")));
    assert!(commands
        .iter()
        .any(|c| c.contains("context hook --engine claude")));
    // Settled: another merge changes nothing.
    assert_eq!(
        merge_nested_hooks(&mut doc, &CLAUDE_EVENTS, A_BIN).unwrap(),
        0
    );
}

#[cfg(unix)]
#[test]
fn post_tool_awareness_merges_and_unmerges_without_touching_foreign_hooks() {
    for (engine, events) in [
        ("claude", &CLAUDE_EVENTS[..]),
        ("codex", &CODEX_EVENTS[..]),
        ("grok", &GROK_EVENTS[..]),
    ] {
        let foreign = json!({"hooks": {"PostToolUse": [{"hooks": [
            {"type": "command", "command": "foreign-post-tool"},
            {"type": "command", "command": format!("echo context hook --engine {engine}")}
        ]}]}});
        let mut doc = merged(events, foreign.clone());
        let groups = doc["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups
            .iter()
            .any(|group| group_reports(group, &format!("awareness:{engine}"))));
        let awareness = groups
            .iter()
            .find(|group| group_reports(group, &format!("awareness:{engine}")))
            .unwrap();
        assert_eq!(awareness["hooks"][0]["timeout"], 30);
        assert_eq!(merge_nested_hooks(&mut doc, events, A_BIN).unwrap(), 0);
        assert!(missing_nested_hooks(&doc, events).is_empty());
        assert!(unmerge_nested_hooks(&mut doc));
        assert_eq!(doc["hooks"]["PostToolUse"], foreign["hooks"]["PostToolUse"]);
    }
}

#[test]
fn merge_preserves_existing_hooks_and_keys() {
    let start = json!({
        "permissions": {"deny": ["AskUserQuestion"]},
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "my-linter"}]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}]
        }
    });
    let doc = merged(&CLAUDE_EVENTS, start);
    // Unrelated top-level keys survive.
    assert_eq!(doc["permissions"]["deny"], json!(["AskUserQuestion"]));
    // Pre-existing Stop group survives alongside ours.
    let stop = doc["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    assert!(stop
        .iter()
        .any(|g| g["hooks"][0]["command"] == json!("my-linter")));
    // Untouched events survive byte-for-byte in value.
    assert_eq!(
        doc["hooks"]["PreToolUse"],
        json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}])
    );
}

#[test]
fn merge_accepts_a_foreign_state_report_hook_as_installed() {
    // A hand-written `a state-report` hook (different binary path)
    // already feeds ingestion; do not add a duplicate group.
    let start = json!({
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "a state-report idle || true"}]}]
        }
    });
    let mut doc = start;
    let changed = merge_nested_hooks(&mut doc, &[("Stop", "idle")], A_BIN).unwrap();
    assert_eq!(changed, 0);
    assert!(missing_nested_hooks(&doc, &[("Stop", "idle")]).is_empty());
}

#[test]
fn merge_replaces_a_schema_invalid_event_slot() {
    let doc = merged(&CODEX_EVENTS, json!({"hooks": {"Stop": "bogus"}}));
    assert!(missing_nested_hooks(&doc, &CODEX_EVENTS).is_empty());
}

#[test]
fn merge_refuses_a_non_object_root() {
    let mut doc = json!([1, 2, 3]);
    assert!(merge_nested_hooks(&mut doc, &CODEX_EVENTS, A_BIN).is_err());
    assert_eq!(doc, json!([1, 2, 3]));
}

#[test]
fn unmerge_removes_only_ours_and_drops_emptied_keys() {
    let mut doc = merged(&CLAUDE_EVENTS, json!({}));
    assert!(unmerge_nested_hooks(&mut doc));
    // Whole `hooks` object is gone: we created every key in it.
    assert_eq!(doc, json!({}));
}

#[test]
fn unmerge_keeps_user_hooks_in_shared_groups() {
    let start = json!({
        "hooks": {
            "Stop": [{
                "matcher": "x",
                "hooks": [
                    {"type": "command", "command": "my-linter"},
                    {"type": "command", "command": "a state-report idle || true"}
                ]
            }]
        }
    });
    let mut doc = start;
    assert!(unmerge_nested_hooks(&mut doc));
    assert_eq!(
        doc["hooks"]["Stop"],
        json!([{
            "matcher": "x",
            "hooks": [{"type": "command", "command": "my-linter"}]
        }])
    );
}

#[test]
fn unmerge_sweeps_retired_events_but_leaves_foreign_hooks_there() {
    // SubagentStop was unmapped from `idle` in 2026-09. An uninstall
    // keyed on the current install tables would never visit its group,
    // leaving our entry pushing idle from an event this version no
    // longer believes in -- while a foreign program's SubagentStop hook
    // in the same document must survive untouched.
    let start = json!({
        "hooks": {
            "Stop": [{
                "hooks": [{"type": "command", "command": "a state-report idle || true"}]
            }],
            "SubagentStop": [{
                "hooks": [
                    {"type": "command", "command": "python3 /opt/pocketshell/hooks/claude_hook.py"},
                    {"type": "command", "command": "/usr/local/bin/a state-report idle || true"}
                ]
            }]
        }
    });
    let mut doc = start;
    assert!(unmerge_nested_hooks(&mut doc));
    assert!(doc["hooks"].get("Stop").is_none());
    assert_eq!(
        doc["hooks"]["SubagentStop"],
        json!([{
            "hooks": [
                {"type": "command", "command": "python3 /opt/pocketshell/hooks/claude_hook.py"}
            ]
        }])
    );
}

#[test]
fn missing_reports_every_absent_event_once() {
    let missing = missing_nested_hooks(&json!({}), &GEMINI_EVENTS);
    // Events wired for two states are reported once.
    let distinct: std::collections::BTreeSet<&str> =
        GEMINI_EVENTS.iter().map(|(event, _)| *event).collect();
    assert_eq!(missing.len(), distinct.len());
    assert!(missing.contains(&"AfterTool".to_string()));
    let doc = merged(&GEMINI_EVENTS, json!({}));
    assert!(missing_nested_hooks(&doc, &GEMINI_EVENTS).is_empty());
}

#[test]
fn uninstall_sweeps_every_managed_generation() {
    // A config carrying all three generations -- current awareness, legacy
    // inbox-notice (any engine), and state-report -- loses exactly those;
    // foreign hooks survive.
    let start = json!({"hooks": {"PostToolUse": [{"hooks": [
        {"type": "command", "command": format!("{A_BIN} context hook --engine zzz 2>/dev/null || true # aplexer-managed-awareness-hook-v1")},
        {"type": "command", "command": format!("{A_BIN} message hook-notice --engine claude 2>/dev/null || true # aplexer-managed-inbox-hook-v1")},
        {"type": "command", "command": format!("{A_BIN} message hook-notice --engine grok 2>/dev/null || true # aplexer-managed-inbox-hook-v1")},
        {"type": "command", "command": "foreign-post-tool"}
    ]}]}});
    let mut doc = start;
    assert!(unmerge_nested_hooks(&mut doc));
    let commands: Vec<&str> = doc["hooks"]["PostToolUse"][0]["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["command"].as_str())
        .collect();
    assert_eq!(commands, vec!["foreign-post-tool"]);
}

#[test]
fn install_refreshes_stale_managed_timeout_and_binary_path() {
    for (engine, events) in [
        ("claude", &CLAUDE_EVENTS[..]),
        ("codex", &CODEX_EVENTS[..]),
        ("gemini", &GEMINI_EVENTS[..]),
    ] {
        let expected = if engine == "gemini" { 30000 } else { 30 };
        let stale_budget = if engine == "gemini" { 5000 } else { 5 };
        // An install from before a budget raise and before `a` moved: the
        // wiring exists, with the old timeout and an old binary path, on
        // an event this engine's table actually wires.
        let (event, state) = events
            .iter()
            .copied()
            .find(|(_, s)| s.strip_prefix("awareness:") == Some(engine))
            .unwrap();
        let stale_command = context_command(A_BIN, engine).replace(A_BIN, "/old/path/a");
        assert_ne!(stale_command, context_command(A_BIN, engine));
        let mut doc = json!({"hooks": {event: [{"hooks": [{
            "type": "command",
            "command": stale_command,
            "timeout": stale_budget
        }]}]}});
        let awareness_group = |doc: &Value| {
            doc["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| group_reports(g, state))
                .unwrap()
                .clone()
        };
        assert_eq!(awareness_group(&doc)["hooks"][0]["timeout"], stale_budget);
        // The merge converges both instead of leaving them stale.
        assert_ne!(merge_nested_hooks(&mut doc, events, A_BIN).unwrap(), 0);
        assert_eq!(awareness_group(&doc)["hooks"][0]["timeout"], expected);
        assert_eq!(
            awareness_group(&doc)["hooks"][0]["command"],
            context_command(A_BIN, engine)
        );
        // Converged: another merge changes nothing.
        assert_eq!(merge_nested_hooks(&mut doc, events, A_BIN).unwrap(), 0);
    }
}
