//! F5 consumer contract: every command/flag Hub and PocketShell drive
//! must exist in the built binary, or release is blocked.
//!
//! Reads `tests/consumers/{hub,pocketshell}.json` (audit §4.5 + code refs
//! in each file's `source`) and asserts each `command` exists and each
//! `flags` token appears in its `--help`. A renamed or dropped flag fails
//! here instead of shipping a silent downgrade to Hub/PocketShell hosts.
//!
//! Live behavior (start/status/capture/kill round-trip, labels, isolation)
//! is pinned by `hub_labels.rs` and `session_isolation.rs`; this file pins
//! the static surface.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

fn consumers_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/consumers")
}

fn load_consumer(name: &str) -> Value {
    let path = consumers_dir().join(format!("{name}.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn help_text(argv: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_aplexer"))
        .args(argv)
        .output()
        .expect("spawn aplexer for --help");
    assert!(
        output.status.success(),
        "`a {}` --help failed (status {:?}): {}",
        argv.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn check_consumer(name: &str) {
    let doc = load_consumer(name);
    assert_eq!(
        doc["consumer"], name,
        "consumer name mismatch in {name}.json"
    );
    assert_eq!(doc["version"], 1, "consumer contract version must be 1");
    let cases = doc["cases"].as_array().expect("cases must be an array");
    assert!(!cases.is_empty(), "{name}.json lists no cases");
    for case in cases {
        let id = case["id"].as_str().expect("case id");
        let command = case["command"].as_str().expect("command");
        let flags = case["flags"].as_array().expect("flags");
        let source = case["source"].as_str().unwrap_or("");
        assert!(!source.is_empty(), "case {id} must cite its source");
        if command == "--version" {
            let output = Command::new(env!("CARGO_BIN_EXE_aplexer"))
                .arg("--version")
                .output()
                .expect("spawn aplexer --version");
            assert!(
                output.status.success(),
                "case {id}: `a --version` failed: {}",
                String::from_utf8_lossy(&output.stderr),
            );
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                stdout.split_whitespace().count() == 2
                    && (stdout.starts_with("a ") || stdout.starts_with("aplexer ")),
                "case {id}: unexpected --version output {stdout:?}"
            );
            continue;
        }
        let parts: Vec<&str> = command.split_whitespace().collect();
        let mut argv: Vec<&str> = parts.clone();
        argv.push("--help");
        let help = help_text(&argv);
        for flag in flags {
            let flag = flag.as_str().expect("flag string");
            assert!(
                help.contains(flag),
                "case {id} ({name}.json): `a {command}` no longer accepts {flag}; \
                 Hub/PocketShell passes it ({source}). Help:\n{help}"
            );
        }
    }
}

#[test]
fn hub_consumer_surface_exists() {
    check_consumer("hub");
}

#[test]
fn pocketshell_consumer_surface_exists() {
    check_consumer("pocketshell");
}

#[test]
fn consumer_files_match_schema_shape() {
    for name in ["hub", "pocketshell"] {
        let doc = load_consumer(name);
        assert!(doc.get("consumer").is_some());
        assert!(doc.get("version").is_some());
        assert!(doc.get("source").is_some());
        assert!(doc.get("cases").is_some());
    }
}
