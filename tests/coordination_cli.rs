#[path = "support/messaging.rs"]
mod support;

use aplexer::{atomic_write_json, Phase};
use serde_json::Value;
use std::process::{Command, Output};
use support::Harness;
use tempfile::TempDir;

fn json(command: &mut Command) -> Value {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn successful(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    output
}

#[test]
fn declarations_make_remote_work_visible_without_relocating_identity() {
    let harness = Harness::new();
    let destination = TempDir::new().unwrap();
    let agent = harness.record(Phase::Running, Some(std::process::id()), b"");
    let peer = harness.record_in(
        destination.path(),
        Phase::Running,
        Some(std::process::id()),
        b"",
    );
    let declaration = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", agent.id.to_string())
            .args([
                "--json",
                "work",
                "join",
                destination.path().to_str().unwrap(),
                "--task",
                "Fix login flow",
                "--mode",
                "edit",
                "--paths",
                "src/auth/**",
            ]),
    );
    assert!(declaration.to_string().contains("Fix login flow"));
    let saved = aplexer::read_record(&harness.paths().record(agent.id)).unwrap();
    assert_eq!(saved.id, agent.id);
    assert_eq!(saved.workspace, agent.workspace);
    let context = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", peer.id.to_string())
            .args([
                "--json",
                "context",
                "--workspace",
                destination.path().to_str().unwrap(),
            ]),
    );
    assert!(context.to_string().contains("Fix login flow"));
    assert!(context.to_string().contains("src/auth/**"));
    let released = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", agent.id.to_string())
            .args([
                "--json",
                "work",
                "leave",
                destination.path().to_str().unwrap(),
            ]),
    );
    assert_eq!(released["released"], true);
    let repeated = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", agent.id.to_string())
            .args([
                "--json",
                "work",
                "leave",
                destination.path().to_str().unwrap(),
            ]),
    );
    assert_eq!(repeated["released"], false);
    let after = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", peer.id.to_string())
            .args([
                "--json",
                "context",
                "--workspace",
                destination.path().to_str().unwrap(),
            ]),
    );
    assert!(!after.to_string().contains("Fix login flow"));
}

#[test]
fn context_is_safe_to_show_to_peers() {
    let harness = Harness::new();
    let mut peer = harness.record(Phase::Running, Some(std::process::id()), b"");
    peer.env.insert("TOKEN".into(), "private-env-secret".into());
    peer.command.push("private-command-secret".into());
    atomic_write_json(&harness.paths().record(peer.id), &peer).unwrap();
    let output = successful(harness.command().args([
        "--json",
        "context",
        "--workspace",
        peer.workspace.to_str().unwrap(),
    ]));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&peer.tag));
    assert!(!text.contains("private-env-secret"));
    assert!(!text.contains("private-command-secret"));
}

#[test]
fn declarations_require_a_real_session_and_reject_escaping_scopes() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let no_identity = harness
        .command()
        .env_remove("APLEXER_SESSION_ID")
        .args([
            "work",
            "join",
            workspace.path().to_str().unwrap(),
            "--task",
            "Task",
        ])
        .output()
        .unwrap();
    assert!(!no_identity.status.success());
    let agent = harness.record(Phase::Running, Some(std::process::id()), b"");
    let escaping = harness
        .command()
        .env("APLEXER_SESSION_ID", agent.id.to_string())
        .args([
            "work",
            "join",
            workspace.path().to_str().unwrap(),
            "--task",
            "Task",
            "--paths",
            "../outside",
        ])
        .output()
        .unwrap();
    assert!(!escaping.status.success());
}

#[test]
fn inbox_show_reply_and_ack_survive_both_sessions_moving() {
    let harness = Harness::new();
    let moved_recipient = TempDir::new().unwrap();
    let moved_sender = TempDir::new().unwrap();
    let mut sender = harness.record(Phase::Exited, None, b"");
    let mut recipient = harness.record(Phase::Exited, None, b"");
    let sent = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", sender.id.to_string())
            .args([
                "--json",
                "message",
                "send",
                "--to",
                &recipient.tag,
                "Please confirm",
            ]),
    );
    let id = sent["id"].as_str().unwrap();
    recipient.workspace = moved_recipient.path().to_path_buf();
    sender.workspace = moved_sender.path().to_path_buf();
    atomic_write_json(&harness.paths().record(recipient.id), &recipient).unwrap();
    atomic_write_json(&harness.paths().record(sender.id), &sender).unwrap();
    let inbox = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", recipient.id.to_string())
            .args(["--json", "message", "inbox"]),
    );
    assert_eq!(inbox[0]["id"], sent["id"]);
    let shown = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", recipient.id.to_string())
            .args(["--json", "message", "show", id]),
    );
    assert_eq!(shown["id"], sent["id"]);
    let reply = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", recipient.id.to_string())
            .args(["--json", "message", "reply", id, "Confirmed"]),
    );
    assert_eq!(reply["workspace"], moved_sender.path().to_str().unwrap());
    assert_eq!(reply["from"]["session_id"], recipient.id.to_string());
    let sender_inbox = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", sender.id.to_string())
            .args(["--json", "message", "inbox"]),
    );
    assert_eq!(sender_inbox[0]["id"], reply["id"]);
    let ack = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", recipient.id.to_string())
            .args(["--json", "message", "ack", id]),
    );
    assert_eq!(ack["acked"][0], sent["id"]);
    let after = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", recipient.id.to_string())
            .args(["--json", "message", "inbox"]),
    );
    assert!(after.as_array().unwrap().is_empty());
    let shown_after_ack = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", recipient.id.to_string())
            .args(["--json", "message", "show", id]),
    );
    assert_eq!(shown_after_ack["id"], sent["id"]);
}

#[test]
fn disjoint_edit_scopes_do_not_report_a_collision() {
    let harness = Harness::new();
    let first = harness.record(Phase::Running, Some(std::process::id()), b"");
    let second = harness.record(Phase::Running, Some(std::process::id()), b"");
    for (record, scope) in [(&first, "src/core/**"), (&second, "src/hooks/**")] {
        successful(
            harness
                .command()
                .env("APLEXER_SESSION_ID", record.id.to_string())
                .args([
                    "work",
                    "join",
                    record.workspace.to_str().unwrap(),
                    "--task",
                    "Parallel implementation",
                    "--paths",
                    scope,
                ]),
        );
    }
    let context = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", first.id.to_string())
            .args([
                "--json",
                "context",
                "--workspace",
                first.workspace.to_str().unwrap(),
            ]),
    );
    assert!(
        context["overlaps"].as_array().unwrap().is_empty(),
        "{context}"
    );
    successful(
        harness
            .command()
            .env("APLEXER_SESSION_ID", second.id.to_string())
            .args([
                "work",
                "join",
                second.workspace.to_str().unwrap(),
                "--task",
                "Shared core fix",
                "--paths",
                "src/core/file.rs",
            ]),
    );
    let overlap = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", first.id.to_string())
            .args([
                "--json",
                "context",
                "--workspace",
                first.workspace.to_str().unwrap(),
            ]),
    );
    assert!(
        !overlap["overlaps"].as_array().unwrap().is_empty(),
        "{overlap}"
    );
}

#[test]
fn context_includes_shared_checkout_subdirectories_and_related_worktrees() {
    let harness = Harness::new();
    let root = tempfile::TempDir::new().unwrap();
    let checkout = root.path().join("repo");
    std::fs::create_dir(&checkout).unwrap();
    successful(Command::new("git").args(["init", "--quiet"]).arg(&checkout));
    successful(Command::new("git").arg("-C").arg(&checkout).args([
        "-c",
        "user.name=Coordination Test",
        "-c",
        "user.email=coordination@example.invalid",
        "commit",
        "--allow-empty",
        "--quiet",
        "-m",
        "Fixture",
    ]));
    let sibling = root.path().join("worktree");
    successful(
        Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&sibling),
    );
    let subdirectory = checkout.join("src");
    std::fs::create_dir(&subdirectory).unwrap();
    let same = harness.record_in(&checkout, Phase::Running, Some(std::process::id()), b"");
    let related = harness.record_in(&sibling, Phase::Running, Some(std::process::id()), b"");
    let context = json(
        harness
            .command()
            .args(["--json", "context", "--workspace"])
            .arg(&subdirectory),
    );
    let peers = context["peers"].as_array().unwrap();
    assert!(
        peers
            .iter()
            .any(|p| p["session"]["id"] == same.id.to_string() && p["relation"] == "same_checkout"),
        "{context}"
    );
    assert!(
        peers
            .iter()
            .any(|p| p["session"]["id"] == related.id.to_string()
                && p["relation"] == "related_worktree"),
        "{context}"
    );
}

#[test]
fn wildcard_scopes_cannot_hide_potential_edit_collisions() {
    let harness = Harness::new();
    let first = harness.record(Phase::Running, Some(std::process::id()), b"");
    let second = harness.record(Phase::Running, Some(std::process::id()), b"");
    for (ours, theirs) in [
        ("src/core*", "src/core-utils"),
        ("src/**/file", "src/deep/nested/file"),
    ] {
        for (record, scope) in [(&first, ours), (&second, theirs)] {
            successful(
                harness
                    .command()
                    .env("APLEXER_SESSION_ID", record.id.to_string())
                    .args([
                        "work",
                        "join",
                        record.workspace.to_str().unwrap(),
                        "--task",
                        "Potential shared edits",
                        "--paths",
                        scope,
                    ]),
            );
        }
        let context = json(
            harness
                .command()
                .env("APLEXER_SESSION_ID", first.id.to_string())
                .args([
                    "--json",
                    "context",
                    "--workspace",
                    first.workspace.to_str().unwrap(),
                ]),
        );
        assert!(
            !context["overlaps"].as_array().unwrap().is_empty(),
            "{ours} vs {theirs}: {context}"
        );
    }
}

#[test]
fn all_visitor_declarations_in_a_checkout_remain_visible() {
    let harness = Harness::new();
    let repo = tempfile::TempDir::new().unwrap();
    successful(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(repo.path()),
    );
    let source = repo.path().join("src");
    let tests = repo.path().join("tests");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&tests).unwrap();
    let caller = harness.record(Phase::Running, Some(std::process::id()), b"");
    let visitor = harness.record(Phase::Running, Some(std::process::id()), b"");
    successful(
        harness
            .command()
            .env("APLEXER_SESSION_ID", caller.id.to_string())
            .args(["work", "join"])
            .arg(repo.path())
            .args(["--task", "Own test changes", "--paths", "tests/**"]),
    );
    for (directory, task) in [
        (&source, "Visitor source changes"),
        (&tests, "Visitor test changes"),
    ] {
        successful(
            harness
                .command()
                .env("APLEXER_SESSION_ID", visitor.id.to_string())
                .args(["work", "join"])
                .arg(directory)
                .args(["--task", task]),
        );
    }
    let context = json(
        harness
            .command()
            .env("APLEXER_SESSION_ID", caller.id.to_string())
            .args(["--json", "context", "--workspace"])
            .arg(repo.path()),
    );
    assert!(
        context.to_string().contains("Visitor source changes"),
        "{context}"
    );
    assert!(
        context.to_string().contains("Visitor test changes"),
        "{context}"
    );
    assert!(
        !context["overlaps"].as_array().unwrap().is_empty(),
        "{context}"
    );
}
