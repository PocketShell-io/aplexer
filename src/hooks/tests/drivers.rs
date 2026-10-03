use super::*;

#[test]
fn opencode_plugin_embeds_the_a_binary_and_maps_events() {
    let source = opencode_plugin_source(A_BIN);
    assert!(source.contains(A_BIN));
    for event in [
        "session.status",
        "session.idle",
        "permission.asked",
        "session.error",
        "session.created",
        "tool.execute.before",
    ] {
        assert!(source.contains(event), "missing {event}");
    }
    for state in ["idle", "waiting", "working"] {
        assert!(source.contains(state), "missing {state}");
    }
    // busy/retry (the session.status payload) must map back to work, or the
    // first idle push stays sticky for all later turns in the same session.
    for status in ["busy", "retry"] {
        assert!(source.contains(status), "missing {status}");
    }
    assert!(source.contains("sessionStates"), "must have aggregate active session states");
    assert!(source.contains("cancelGlobalIdle"), "must cancel global idle on busy/tool/error/permission");
    assert!(source.contains("evaluateAggregateState"), "must evaluate aggregate state across sessions");
    assert!(source.contains("resolveCallKey"), "must resolve call key with disambiguation");
    assert!(source.contains("unkeyedPending"), "must track unkeyed pending calls in FIFO queue");
}

#[test]
fn grok_events_exclude_notification_to_prevent_idle_clobber() {
    assert!(!GROK_EVENTS.iter().any(|(event, _)| *event == "Notification"));
    assert!(GROK_EVENTS.iter().any(|(event, state)| *event == "Stop" && *state == "idle"));
}

#[test]
fn opencode_plugin_awareness_after_appends_without_clobbering() {
    let source = opencode_plugin_source(A_BIN);
    // The after callback shells out argv with a timeout and speaks the
    // exact payload the awareness parser reads.
    assert!(source.contains("tool.execute.after"), "{source}");
    // argv form: the plugin never routes through a shell.
    assert!(
        source.contains(r#"["context", "hook", "--engine", "opencode"]"#),
        "{source}"
    );
    assert!(source.contains("timeout: 5000"), "{source}");
    for key in ["hook_event_name", "tool_name", "session_id", "tool_input"] {
        assert!(source.contains(key), "missing payload key {key}");
    }
    // The before callback saves the safe path arguments for after.
    assert!(source.contains("SAFE_ARG_KEYS"), "{source}");
    assert!(source.contains("file_path"), "{source}");
    // Append-only: original tool output is preserved byte-for-byte (suffix
    // append, never a replace/trim of the original), and title/metadata
    // are untouched (the Node runtime test in tests/coordination_packages.py
    // pins the package bundle's behavior; this pins the same contract in
    // the built-in plugin).
    assert!(
        source.contains("output.output = output.output + \"\\n\\n\" +"),
        "{source}"
    );
    assert!(!source.contains(".replace"), "{source}");
    assert!(!source.contains("output.output.trim()"), "{source}");
    assert!(
        source.contains("state-report"),
        "check_opencode greps for this"
    );
}

#[test]
fn normalize_engine_filter_maps_zcodex_onto_codex() {
    assert_eq!(normalize_engine_filter("zcodex").unwrap(), "codex");
    assert_eq!(normalize_engine_filter("codex").unwrap(), "codex");
    assert!(normalize_engine_filter("shell").is_err());
}

#[test]
fn engine_drivers_match_the_documented_engine_list() {
    let driven: Vec<&str> = ENGINE_DRIVERS.iter().map(|d| d.engine).collect();
    assert_eq!(driven, HOOK_ENGINES);
}

#[test]
fn install_check_uninstall_round_trip_in_a_throwaway_home() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path();
    let targets = resolve_targets(home, None, None, &[]);
    // check before install: nothing installed.
    let statuses = check(&targets, None);
    assert_eq!(statuses.len(), HOOK_ENGINES.len());
    assert!(statuses.iter().all(|s| !s.installed));
    // install: everything installs (fresh home, no foreign config).
    let installed = install(&targets, A_BIN, None);
    assert!(installed.iter().all(|s| s.installed), "{installed:?}");
    // check after install: fully initialized.
    let statuses = check(&targets, None);
    assert!(statuses.iter().all(|s| s.installed), "{statuses:?}");
    // install again: idempotent, nothing new.
    let again = install(&targets, A_BIN, None);
    assert!(again.iter().all(|s| s.action == "present"), "{again:?}");
    // uninstall: removes what install added.
    let removed = uninstall(&targets, None);
    assert!(removed.iter().all(|s| s.action != "error"), "{removed:?}");
    let statuses = check(&targets, None);
    assert!(statuses.iter().all(|s| !s.installed));
}

#[test]
fn engine_filter_limits_the_drivers() {
    let dir = tempfile::TempDir::new().unwrap();
    let targets = resolve_targets(dir.path(), None, None, &[]);
    let only = install(&targets, A_BIN, Some("opencode"));
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].engine, "opencode");
    let statuses = check(&targets, Some("opencode"));
    assert!(statuses.iter().all(|s| s.installed));
    let others = check(&targets, Some("claude"));
    assert!(others.iter().all(|s| !s.installed));
}

#[test]
fn install_writes_through_a_symlinked_settings_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("home");
    let dotfiles = dir.path().join("dotfiles");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("claude-settings.json");
    fs::write(&real, "{\"permissions\": {}}\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
    let link = home.join(".claude").join("settings.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // A dotfiles link whose target does not exist yet.
    let dangling_target = dotfiles.join("gemini").join("settings.json");
    let dangling = home.join(".gemini").join("settings.json");
    std::os::unix::fs::symlink(&dangling_target, &dangling).unwrap();

    let targets = resolve_targets(&home, None, None, &[]);
    let statuses = install(&targets, A_BIN, None);
    assert!(statuses.iter().all(|s| s.installed), "{statuses:?}");

    for path in [&link, &dangling] {
        assert!(
            fs::symlink_metadata(path).unwrap().file_type().is_symlink(),
            "{} is no longer a symlink",
            path.display()
        );
    }
    let doc: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
    assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
    assert_eq!(doc["permissions"], json!({}));
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640,
        "mode of the real file was not preserved"
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&dangling_target).unwrap()).unwrap();
    assert!(missing_nested_hooks(&doc, &GEMINI_EVENTS).is_empty());
    assert_eq!(
        fs::metadata(&dangling_target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn antigravity_uses_named_hooks_and_preserves_other_definitions() {
    let dir = tempfile::TempDir::new().unwrap();
    let targets = resolve_targets(dir.path(), None, None, &[]);
    let path = &targets.antigravity_hooks;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let foreign = serde_json::json!({"PreInvocation": [{"command": "echo hello"}]});
    fs::write(
        path,
        serde_json::to_string(&serde_json::json!({"my-hook": foreign})).unwrap(),
    )
    .unwrap();
    assert!(install(&targets, A_BIN, Some("antigravity"))[0].installed);
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(doc["my-hook"], foreign);
    assert!(doc["aplexer-state-report"]["PreInvocation"][0]["command"]
        .as_str()
        .unwrap()
        .contains("state-report working"));
    assert!(doc["aplexer-state-report"]["Stop"][0]["command"]
        .as_str()
        .unwrap()
        .contains("state-report idle"));
    doc["aplexer-state-report"]["enabled"] = Value::Bool(false);
    fs::write(path, serde_json::to_string(&doc).unwrap()).unwrap();
    assert!(!check(&targets, Some("antigravity"))[0].installed);
    assert_eq!(
        uninstall(&targets, Some("antigravity"))[0].action,
        "removed"
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(doc, serde_json::json!({"my-hook": foreign}));
    assert_eq!(normalize_engine_filter("agy").unwrap(), "antigravity");
}

#[test]
fn antigravity_refuses_to_overwrite_malformed_hooks() {
    let dir = tempfile::TempDir::new().unwrap();
    let targets = resolve_targets(dir.path(), None, None, &[]);
    let path = &targets.antigravity_hooks;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for original in ["{broken", "[]"] {
        fs::write(path, original).unwrap();
        assert_eq!(
            install(&targets, A_BIN, Some("antigravity"))[0].action,
            "error"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}

#[test]
fn opencode_plugin_handles_unkeyed_and_duplicate_callid_same_tool_concurrent_calls_runtime() {
    let dir = tempfile::TempDir::new().unwrap();
    let module_path = dir.path().join("plugin.mjs");
    let plugin_src = opencode_plugin_source("/bin/true");
    fs::write(&module_path, plugin_src).unwrap();

    let node_test = r#"
import { pathToFileURL } from 'node:url';
import assert from 'node:assert';

const pluginModule = await import(pathToFileURL(process.argv[2]).href);
const hooks = await pluginModule.AplexerStateReport();

// Variant A: Two concurrent same-tool calls with IDENTICAL callID ('dup-call')
const inputA1 = { tool: 'bash', callID: 'dup-call' };
const inputA2 = { tool: 'bash', callID: 'dup-call' };

await hooks['tool.execute.before'](inputA1);
assert.ok(inputA1._aplexer_call_key, 'inputA1 should receive assigned key');

// Retransmitted before on inputA1 should be idempotent (same key)
const origKeyA1 = inputA1._aplexer_call_key;
await hooks['tool.execute.before'](inputA1);
assert.strictEqual(inputA1._aplexer_call_key, origKeyA1, 'retransmitted before must be idempotent');

await hooks['tool.execute.before'](inputA2);
assert.ok(inputA2._aplexer_call_key, 'inputA2 should receive assigned key');
assert.notStrictEqual(inputA1._aplexer_call_key, inputA2._aplexer_call_key, 'identical callIDs must receive distinct keys');

// Finish tool A1: tool A2 is STILL ACTIVE!
await hooks['tool.execute.after'](inputA1, { output: 'result A1' });

// Finish tool A2: now all are done!
await hooks['tool.execute.after'](inputA2, { output: 'result A2' });

// Variant B: Two concurrent same-tool calls with ABSENT callID
const inputB1 = { tool: 'bash' };
const inputB2 = { tool: 'bash' };

await hooks['tool.execute.before'](inputB1);
assert.ok(inputB1._aplexer_id, 'inputB1 should receive assigned anon id');

await hooks['tool.execute.before'](inputB2);
assert.ok(inputB2._aplexer_id, 'inputB2 should receive assigned anon id');
assert.notStrictEqual(inputB1._aplexer_id, inputB2._aplexer_id, 'absent callIDs must receive distinct anon ids');

// Finish tool B1: tool B2 is STILL ACTIVE!
await hooks['tool.execute.after'](inputB1, { output: 'result B1' });

// Finish tool B2: now both done!
await hooks['tool.execute.after'](inputB2, { output: 'result B2' });

// Detached objects on after:
await hooks['tool.execute.before']({ tool: 'grep', callID: 'same-detached' });
await hooks['tool.execute.before']({ tool: 'grep', callID: 'same-detached' });
await hooks['tool.execute.after']({ tool: 'grep', callID: 'same-detached' }, { output: 'grep 1' });
await hooks['tool.execute.after']({ tool: 'grep', callID: 'same-detached' }, { output: 'grep 2' });

console.log('VARIANT_A_AND_B_CONCURRENT_OK');
"#;

    let test_path = dir.path().join("test.mjs");
    fs::write(&test_path, node_test).unwrap();

    let output = std::process::Command::new("node")
        .arg(&test_path)
        .arg(&module_path)
        .output()
        .expect("failed to execute node");

    assert!(
        output.status.success(),
        "Node test failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("VARIANT_A_AND_B_CONCURRENT_OK"));
}

