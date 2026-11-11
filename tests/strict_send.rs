//! Offline CLI protocol tests: no native session or agent is launched.
#[path = "support/messaging.rs"]
mod support;
use aplexer::sys::ipc::{Listener, Stream};
use aplexer::{
    atomic_write_json, frame_json, read_frame, write_json, Operation, Phase, Request, Response,
};
use serde_json::{json, Value};
use std::io::Write;
use std::process::Stdio;
use std::thread;

fn exercise(reply: Option<&str>) -> (std::process::Output, Value) {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Running, Some(std::process::id()), b"");
    record.engine = "codex".into();
    record.created_at_ms = aplexer::now_ms();
    // Keep only the fixture IPC endpoint short on Linux; state and ordinary
    // test temps still use the caller's repo-local TMPDIR. Never weaken the
    // production canonical socket-path check to accommodate a long checkout.
    #[cfg(target_os = "linux")]
    let ipc_runtime = {
        let root = std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() }));
        tempfile::Builder::new()
            .prefix("strict-send-")
            .tempdir_in(if root.is_dir() {
                root
            } else {
                std::env::temp_dir()
            })
            .unwrap()
    };
    #[cfg(target_os = "linux")]
    {
        record.socket_path = ipc_runtime
            .path()
            .join("sessions")
            .join(record.id.to_string())
            .join("control.sock");
    }
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    #[cfg(unix)]
    std::fs::create_dir_all(record.socket_path.parent().unwrap()).unwrap();
    let listener = Listener::bind(&record.socket_path).unwrap();
    #[cfg(unix)]
    listener.set_nonblocking(true).unwrap();
    let worker_record = record.clone();
    let response = reply.map(str::to_owned);
    let (accepted, accepted_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (mut stream, request) = loop {
            #[cfg(unix)]
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        break stream;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(error) => panic!("strict fake accept failed: {error}"),
                }
            };
            #[cfg(windows)]
            let mut stream = loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "strict request before fixture deadline");
                // ERROR_NO_DATA can replace an abandoned pipe and return None
                // before the timeout; keep accepting within the same deadline.
                if let Some(stream) = listener.accept_timeout(left).unwrap() {
                    break stream;
                }
            };
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "strict request before fixture deadline");
            stream.set_read_timeout(Some(left)).unwrap();
            let _ = accepted.send(());
            // Named-pipe availability probes connect and close without a frame.
            // An empty probe is not a request and must not end the fake worker.
            let Some(frame) = read_frame(&mut stream).unwrap() else {
                continue;
            };
            let request: Request = frame_json(frame).unwrap();
            break (stream, request);
        };
        assert_eq!(request.session_id, Some(worker_record.id));
        let Operation::SubmitStrict {
            operation_id,
            expected_tag,
            expected_workspace,
            expected_created_at_ms,
            deadline_ms,
            text,
        } = &request.operation
        else {
            panic!("strict send fell back to another operation")
        };
        assert_eq!(expected_tag, &worker_record.tag);
        assert_eq!(expected_workspace, &worker_record.workspace);
        assert_eq!(*expected_created_at_ms, worker_record.created_at_ms);
        assert_eq!(text, "offline strict payload");
        assert_eq!(*deadline_ms, worker_record.created_at_ms + 60_000);
        let evidence = json!({"operation_id":operation_id,"text_sha_only":true});
        if response.as_deref() == Some("unsupported") {
            write_json(
                &mut stream,
                &Response::error(request.request_id, "unsupported operation"),
            )
            .unwrap();
            return evidence;
        }
        if response.as_deref() == Some("malformed") {
            write_json(
                &mut stream,
                &Response::ok(
                    request.request_id,
                    json!({"status":"submitted_verified","consumed":null}),
                ),
            )
            .unwrap();
            return evidence;
        }
        if let Some(status) = response {
            let receipt_deadline = if status == "wrong_deadline" {
                deadline_ms + 1
            } else {
                *deadline_ms
            };
            let status = if status == "wrong_deadline" {
                "submitted_verified"
            } else {
                &status
            };
            write_json(&mut stream,&Response::ok(request.request_id,json!({"version":1,"session_id":worker_record.id,"status":status,"reason":"test_verdict","operation_id":operation_id,"consumed":null,"mutation":status!="refused","landed":status!="refused","submitted":status!="refused","deadline_ms":receipt_deadline,"request_sha256":aplexer::strict_submission_digest(worker_record.id,*operation_id,expected_tag,expected_workspace,*expected_created_at_ms,*deadline_ms,text).unwrap()}))).unwrap();
        }
        // Otherwise close the connection after accepting the request: lost reply.
        evidence
    });
    // Force the same empty readiness connection on Unix that Windows makes
    // during pipe availability checks. Hold it until accepted, then close it.
    #[cfg(unix)]
    let probe = Stream::connect(&record.socket_path).unwrap();
    #[cfg(windows)]
    let probe = Stream::connect(&record.socket_path, std::time::Duration::from_secs(3)).unwrap();
    accepted_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("empty readiness probe accepted");
    drop(probe);
    let mut command = h.command();
    #[cfg(target_os = "linux")]
    command.env("APLEXER_RUNTIME_DIR", ipc_runtime.path());
    command
        .args([
            "--json",
            "send",
            &record.id.to_string(),
            "--strict",
            "--enter",
            "--stdin",
            "--expect-tag",
            &record.tag,
            "--expect-workspace",
        ])
        .arg(&record.workspace)
        .args(["--idempotency-key", "11111111-1111-4111-8111-111111111111"])
        .args([
            "--deadline-ms",
            &(record.created_at_ms + 60_000).to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"offline strict payload")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let evidence = worker
        .join()
        .unwrap_or_else(|error| panic!("worker error {error:?}, child result {output:?}"));
    (output, evidence)
}
#[test]
fn strict_cli_uses_bound_operation_and_only_verified_submission_exits_zero() {
    for status in ["submitted_verified", "uncertain", "refused"] {
        let (output, _) = exercise(Some(status));
        assert_eq!(
            output.status.success(),
            status == "submitted_verified",
            "{output:?}"
        );
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["status"], status);
        assert!(receipt["consumed"].is_null());
        assert!(receipt["deadline_ms"].as_u64().unwrap() > 0);
    }
}
#[test]
fn strict_cli_lost_response_is_structured_uncertainty_without_legacy_fallback() {
    let (output, _) = exercise(None);
    assert!(!output.status.success());
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["reason"], "strict_transport_uncertain");
    assert!(receipt["mutation"].is_null());
}
#[test]
fn strict_cli_rejects_ambiguous_targets_and_raw_input_before_connecting() {
    let h = support::Harness::new();
    for args in [
        vec![
            "--json",
            "send",
            "prefix",
            "--strict",
            "--enter",
            "--expect-tag",
            "tag",
            "--expect-workspace",
            "/offline",
            "--idempotency-key",
            "11111111-1111-4111-8111-111111111111",
            "text",
        ],
        vec![
            "send",
            "11111111-1111-4111-8111-111111111111",
            "--strict",
            "--enter",
            "--raw",
            "text",
        ],
    ] {
        let output = h.command().args(args).output().unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn strict_cli_rejects_incomplete_verified_receipt_and_unsupported_worker() {
    for (reply, reason) in [
        ("malformed", "strict_receipt_unverified"),
        ("unsupported", "strict_transport_uncertain"),
        ("wrong_deadline", "strict_receipt_unverified"),
    ] {
        let (output, _) = exercise(Some(reply));
        assert!(!output.status.success());
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["status"], "uncertain");
        assert_eq!(receipt["reason"], reason);
    }
}

#[test]
fn strict_cli_requires_deadline_and_old_worker_cannot_decode_new_operation() {
    let h = support::Harness::new();
    let missing = h
        .command()
        .args([
            "send",
            "11111111-1111-4111-8111-111111111111",
            "--strict",
            "--enter",
            "--expect-tag",
            "tag",
            "--expect-workspace",
            "/offline",
            "--idempotency-key",
            "22222222-2222-4222-8222-222222222222",
            "text",
        ])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--deadline-ms"));

    // The previous worker accepted unknown fields in submit_strict. A new
    // field alone therefore cannot protect against that worker writing input.
    #[derive(serde::Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    enum OldOperation {
        SubmitStrict {
            operation_id: uuid::Uuid,
            expected_tag: String,
            expected_workspace: std::path::PathBuf,
            expected_created_at_ms: u64,
            text: String,
        },
    }
    let operation = Operation::SubmitStrict {
        operation_id: uuid::Uuid::new_v4(),
        expected_tag: "tag".into(),
        expected_workspace: "/offline".into(),
        expected_created_at_ms: 1,
        deadline_ms: 2,
        text: "payload".into(),
    };
    let wire = serde_json::to_value(&operation).unwrap();
    assert_eq!(wire["op"], "submit_strict_deadline");
    assert!(serde_json::from_value::<OldOperation>(wire.clone()).is_err());
    let mut old = wire;
    old["op"] = json!("submit_strict");
    let OldOperation::SubmitStrict {
        operation_id,
        expected_tag,
        expected_workspace,
        expected_created_at_ms,
        text,
    } = serde_json::from_value::<OldOperation>(old.clone()).unwrap();
    assert_eq!(operation_id.to_string(), old["operation_id"]);
    assert_eq!(expected_tag, "tag");
    assert_eq!(expected_workspace, std::path::Path::new("/offline"));
    assert_eq!(expected_created_at_ms, 1);
    assert_eq!(text, "payload");
    assert!(serde_json::from_value::<Operation>(old).is_err());
}

#[test]
fn strict_digest_binds_exact_compact_utf8_seven_tuple() {
    let digest = aplexer::strict_submission_digest(
        "11111111-1111-4111-8111-111111111111".parse().unwrap(),
        "22222222-2222-4222-8222-222222222222".parse().unwrap(),
        "tag",
        std::path::Path::new("/offline/λ"),
        123,
        456,
        "hello\n世界",
    )
    .unwrap();
    assert_eq!(
        digest,
        "8b937935123f706ab1cd7c11274cd14a6b3dfa1ec4ba4e4b9f61586bc61f39af"
    );
}
