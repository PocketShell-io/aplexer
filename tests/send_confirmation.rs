//! Exercise the real CLI/stdio send paths against a worker that can swallow
//! Enter. No engine account/network is needed, and this runs on Windows too.
#[path = "support/messaging.rs"]
mod support;
use aplexer::sys::ipc::Listener;
use aplexer::{
    atomic_write_json, frame_json, read_frame, write_frame, write_json, FrameKind, Operation,
    Phase, Request, Response,
};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

fn exercise(engine: &str, accepted_enter: usize, recognizable: bool, stdio: bool) {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Running, Some(std::process::id()), b"");
    record.engine = engine.into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    std::fs::write(&h.paths().config_file, format!("[engines.{engine}]\ncommand = [\"unused\"]\nenter_delay_ms = 0\nenter_settle = false\n")).unwrap();
    #[cfg(unix)]
    std::fs::create_dir_all(record.socket_path.parent().unwrap()).unwrap();
    let listener = Listener::bind(&record.socket_path).unwrap();
    #[cfg(unix)]
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    let worker_record = record.clone();
    let worker = thread::spawn(move || {
        let mut writes = Vec::new();
        let mut captures = 0;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !signal.load(Ordering::SeqCst) && Instant::now() < deadline {
            #[cfg(unix)]
            let stream = match listener.accept() {
                Ok((stream, _)) => Some(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                Err(e) => panic!("{e}"),
            };
            #[cfg(windows)]
            let stream = listener.accept_timeout(Duration::from_millis(10)).unwrap();
            let Some(mut stream) = stream else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request: Request = frame_json(read_frame(&mut stream).unwrap().unwrap()).unwrap();
            match request.operation {
                Operation::Status => write_json(
                    &mut stream,
                    &Response::ok(
                        request.request_id,
                        serde_json::to_value(&worker_record).unwrap(),
                    ),
                )
                .unwrap(),
                Operation::Send { bytes } => {
                    let data = read_frame(&mut stream).unwrap().unwrap().payload;
                    assert_eq!(bytes, data.len());
                    writes.push(data);
                    write_json(&mut stream, &Response::ok(request.request_id, json!({}))).unwrap();
                }
                Operation::CaptureScreen { .. } => {
                    captures += 1;
                    let enters = writes.iter().filter(|b| b.as_slice() == b"\r").count();
                    let pending = enters < accepted_enter;
                    let screen = match (worker_record.engine.as_str(), pending, recognizable) {
                        (_, _, false) => "unrecognized layout".to_owned(),
                        ("claude", true, _) => "────────\n❯ [Pasted text #1 +1 lines]\n────────".into(),
                        ("claude", false, _) => "❯ retry-probe\n────────\n❯ \n────────".into(),
                        ("opencode", true, _) => "┃ retry-probe\n┃\n┃ Build auto · Muse Spark\n╹▀▀▀▀".into(),
                        ("opencode", false, _) => "┃ retry-probe\n\n┃\n┃\n┃ Build auto · Muse Spark\n╹▀▀▀▀".into(),
                        (_, true, _) => "› [Pasted Content 123 chars]\n\n GPT-6-Luna · Context 100% left".into(),
                        (_, false, _) => "› retry-probe\n• done\n› Ask Codex to do anything\n\n GPT-6-Luna · Context 99% left".into(),
                    };
                    write_json(
                        &mut stream,
                        &Response::ok(request.request_id, json!({"bytes":screen.len()})),
                    )
                    .unwrap();
                    write_frame(&mut stream, FrameKind::Data, screen.as_bytes()).unwrap();
                }
                other => panic!("{other:?}"),
            }
        }
        (writes, captures)
    });
    let payload = if stdio {
        "retry-probe\nsecond line"
    } else {
        "retry-probe"
    };
    let success = if stdio {
        let mut child = h
            .command()
            .args(["rpc", "--stdio", "--protocol", "1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let request = serde_json::to_vec(&json!({"protocol":1,"id":"test","method":"send","params":{"selector":record.id,"selectorMode":"psc","payload":payload,"submit":true}})).unwrap();
        input
            .write_all(&(request.len() as u32).to_be_bytes())
            .unwrap();
        input.write_all(&request).unwrap();
        drop(input);
        let mut output = child.stdout.take().unwrap();
        let mut header = [0; 4];
        output.read_exact(&mut header).unwrap();
        let mut body = vec![0; u32::from_be_bytes(header) as usize];
        output.read_exact(&mut body).unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        child.wait().unwrap();
        response.get("error").is_none()
    } else {
        let result = h
            .command()
            .args(["send", &record.id.to_string(), payload, "--enter"])
            .output()
            .unwrap();
        if accepted_enter > 2 && recognizable {
            assert!(String::from_utf8_lossy(&result.stderr).contains("still pending"));
        }
        result.status.success()
    };
    stop.store(true, Ordering::SeqCst);
    let (writes, captures) = worker.join().unwrap();
    assert_eq!(success, !recognizable || accepted_enter <= 2);
    assert_eq!(
        writes.iter().filter(|b| b.as_slice() == b"\r").count(),
        if recognizable {
            accepted_enter.min(2)
        } else {
            1
        }
    );
    assert_eq!(
        writes
            .iter()
            .filter(|b| b.as_slice() == payload.as_bytes())
            .count(),
        1,
        "payload must never be replayed"
    );
    assert!(captures > 0, "must re-capture after Enter");
}

#[test]
fn swallowed_enter_retries_once_for_each_measured_engine() {
    for engine in ["codex", "claude", "opencode"] {
        exercise(engine, 2, true, false);
    }
}
#[test]
fn successful_submission_and_unrecognized_screens_never_get_extra_enter() {
    exercise("codex", 1, true, false);
    exercise("codex", 3, false, false);
}
#[test]
fn pending_after_retry_returns_error_without_replaying_text() {
    exercise("claude", 3, true, false);
}
#[test]
fn psc_multiline_paste_uses_the_same_confirmation_and_retry() {
    exercise("codex", 2, true, true);
}
