//! `APLEXER_*` environment bindings vs. process reality: a shell snapshot
//! can carry another session's (or workspace's) variables into a process,
//! and every identity-resolving command then follows the stale values.
//! These tests pin the diagnose-only contract: name both identities and the
//! fix, never auto-substitute.

#[path = "support/messaging.rs"]
mod support;

use aplexer::Phase;
use serde_json::Value;
use std::process::Command;
use support::Harness;
use tempfile::TempDir;

/// A live session whose worker is not in the caller's ancestry is exactly
/// what a stale `APLEXER_SESSION_ID` looks like from outside the session:
/// `whoami --json` must keep resolving it (diagnose-only) but report the
/// drift in a `binding_check` field, alongside the env-vs-record workspace
/// disagreement.
#[test]
fn whoami_reports_env_binding_drift() {
    let harness = Harness::new();
    let worker = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let session = harness.record(Phase::Running, Some(worker.id()), b"");
    let stale_workspace = TempDir::new().unwrap();

    let out = harness
        .command()
        .env("APLEXER_SESSION_ID", session.id.to_string())
        .env("APLEXER_WORKSPACE", stale_workspace.path())
        .args(["--json", "whoami"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let session_id = session.id.to_string();
    assert_eq!(report["id"], session_id.as_str());
    assert_eq!(report["binding_check"]["ok"], false);
    let issues = report["binding_check"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 2, "{issues:?}");
    let joined = serde_json::to_string(&issues).unwrap();
    assert!(
        joined.contains("not in this process's ancestry"),
        "{joined}"
    );
    assert!(joined.contains("APLEXER_WORKSPACE"), "{joined}");
    assert!(joined.contains("unset the stale"), "{joined}");
    drop(worker);
}

/// No recorded worker pid means ancestry can neither confirm nor contradict
/// the binding: no drift claim, `ok` stays true.
#[test]
fn whoami_binding_check_stays_quiet_without_something_to_contradict() {
    let harness = Harness::new();
    let plain = harness.record(Phase::Exited, None, b"");

    let out = harness
        .command()
        .env("APLEXER_SESSION_ID", plain.id.to_string())
        .env_remove("APLEXER_WORKSPACE")
        .args(["--json", "whoami"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["binding_check"]["ok"], true);
    assert_eq!(
        report["binding_check"]["issues"].as_array().unwrap().len(),
        0
    );
}

/// The messaging entry points share the same diagnostic: a send whose
/// identity comes from a stale binding still goes through (never blocked,
/// never substituted) but names the drift on stderr.
#[test]
fn message_send_warns_on_stale_binding_but_still_sends() {
    let harness = Harness::new();
    let worker = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let stale = harness.record(Phase::Running, Some(worker.id()), b"");
    let recipient = harness.record(Phase::Exited, None, b"");

    let out = harness
        .command()
        .env("APLEXER_SESSION_ID", stale.id.to_string())
        .env_remove("APLEXER_WORKSPACE")
        .args(["--json", "message", "send", "--to", &recipient.tag, "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let sent: Value = serde_json::from_slice(&out.stdout).unwrap();
    let stale_id = stale.id.to_string();
    assert_eq!(sent["from"]["session_id"], stale_id.as_str());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not in this process's ancestry"),
        "{stderr}"
    );
    drop(worker);
}
