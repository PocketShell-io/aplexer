#![cfg(unix)]
#[path = "support/messaging.rs"]
mod support;
#[path = "messaging_deferred/worker.rs"]
mod worker;
use aplexer::{atomic_write_json, Phase};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::{Duration, Instant};

struct Helper {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    next: usize,
}
impl Helper {
    fn new(h: &support::Harness) -> Self {
        let mut child = h
            .command()
            .args(["rpc", "--stdio", "--protocol", "1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: child.stdout.take().unwrap(),
            child,
            next: 0,
        }
    }
    fn write(&mut self, value: Value) {
        let bytes = serde_json::to_vec(&value).unwrap();
        let input = self.input.as_mut().unwrap();
        input
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        input.write_all(&bytes).unwrap();
        input.flush().unwrap();
    }
    fn read(&mut self) -> Value {
        let mut header = [0; 4];
        self.output.read_exact(&mut header).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
        self.output.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        self.write(
            json!({"protocol":1,"id":self.next.to_string(),"method":method,"params":params}),
        );
        self.read()
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.input.take();
        if std::thread::panicking() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn list_status_capture_json_parity_and_incremental_changes() {
    let h = support::Harness::new();
    let mut r = h.record(Phase::Exited, None, b"raw\xffbytes\n");
    let mut rpc = Helper::new(&h);
    let expected: Value = serde_json::from_slice(
        &h.command()
            .args(["list", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(rpc.call("list", json!({}))["result"], expected);
    let expected: Value = serde_json::from_slice(
        &h.command()
            .args(["status", &r.id.to_string(), "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        rpc.call("status", json!({"selector":r.id}))["result"],
        expected
    );
    let expected: Value = serde_json::from_slice(
        &h.command()
            .args(["capture", &r.id.to_string(), "--json", "--bytes", "16"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        rpc.call("capture", json!({"selector":r.id,"maxBytes":16}))["result"],
        expected
    );
    r.tag = "renamed".into();
    atomic_write_json(&h.paths().record(r.id), &r).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if rpc.call("list", json!({}))["result"][0]["tag"] == "renamed" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let new = h.record(Phase::Exited, None, b"new");
    loop {
        if rpc.call("list", json!({}))["result"]
            .as_array()
            .unwrap()
            .len()
            == 2
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_dir_all(h.paths().state_session(new.id)).unwrap();
    loop {
        if rpc.call("list", json!({}))["result"]
            .as_array()
            .unwrap()
            .len()
            == 1
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn multiplexing_send_order_duplicate_and_validation() {
    let h = support::Harness::new();
    let mut r = h.record(Phase::Running, Some(std::process::id()), b"");
    r.engine = "codex".into();
    atomic_write_json(&h.paths().record(r.id), &r).unwrap();
    let server = worker::Worker::start(&r, false);
    let mut rpc = Helper::new(&h);
    // Wait for initial indexing before measuring the deliberately slow send.
    assert!(rpc.call("ping", json!({}))["result"]["ok"]
        .as_bool()
        .unwrap());
    rpc.write(json!({"protocol":1,"id":"send1","method":"send","params":{"selector":r.id,"payload":"first","submit":true}}));
    rpc.write(json!({"protocol":1,"id":"send2","method":"send","params":{"selector":r.id,"payload":"second","submit":true}}));
    rpc.write(json!({"protocol":1,"id":"ping","method":"ping","params":{}}));
    let reply = rpc.read();
    assert_eq!(reply["id"], "ping");
    let replies = [rpc.read(), rpc.read()];
    assert!(replies.iter().all(|v| v.get("result").is_some()));
    rpc.write(json!({"protocol":1,"id":"send1","method":"send","params":{"selector":r.id,"payload":"duplicate"}}));
    assert_eq!(rpc.read()["error"]["code"], "duplicate_id");
    let bad = rpc.call(
        "send",
        json!({"selector":r.id,"payload":"\u{1b}evil","submit":true}),
    );
    assert_eq!(bad["error"]["notDispatched"], true);
    let bad = rpc.call(
        "send",
        json!({"selector":r.id,"payload":"x","submit":"yes"}),
    );
    assert_eq!(bad["error"]["notDispatched"], true);
    drop(rpc);
    let writes = server.finish();
    assert_eq!(
        writes,
        vec![
            b"\x1b[200~".to_vec(),
            b"first".to_vec(),
            b"\x1b[201~".to_vec(),
            b"\r".to_vec(),
            b"\x1b[200~".to_vec(),
            b"second".to_vec(),
            b"\x1b[201~".to_vec(),
            b"\r".to_vec()
        ]
    );
}

#[test]
fn psc_selector_parity_and_schema3() {
    let h = support::Harness::new();
    let mut one = h.record(Phase::Running, Some(std::process::id()), b"");
    one.tag = "shared".into();
    one.reported_state = Some("waiting".into());
    one.reported_state_at_ms = Some(aplexer::now_ms());
    atomic_write_json(&h.paths().record(one.id), &one).unwrap();
    let mut two = h.record_in(
        std::path::Path::new("/different-workspace"),
        Phase::Running,
        Some(std::process::id()),
        b"",
    );
    two.tag = "shared".into();
    atomic_write_json(&h.paths().record(two.id), &two).unwrap();
    let server = worker::Worker::start(&one, false);
    let mut rpc = Helper::new(&h);
    let listing = rpc.call("list", json!({"format":"psc"}));
    assert_eq!(listing["result"]["schema"], 3);
    assert_eq!(listing["result"]["sessions"].as_array().unwrap().len(), 2);
    let error = rpc.call("status", json!({"selector":"shared","selectorMode":"psc"}));
    assert_eq!(error["error"]["code"], "SESSION_AMBIGUOUS");
    assert_eq!(
        error["error"]["details"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let missing = rpc.call("status", json!({"selector":"missing","selectorMode":"psc"}));
    assert_eq!(missing["error"]["code"], "SESSION_NOT_FOUND");
    let prefix = &one.id.to_string()[..4];
    // UUIDv7 fixture records share their timestamp prefix, so it is ambiguous.
    assert_eq!(
        rpc.call("status", json!({"selector":prefix,"selectorMode":"psc"}))["error"]["code"],
        "SESSION_AMBIGUOUS"
    );
    let value = rpc.call("status", json!({"selector":one.id,"selectorMode":"psc"}));
    assert_eq!(value["result"]["session"]["id"], one.id.to_string());
    assert_eq!(value["result"]["session"]["engine"], Value::Null);
    assert_eq!(value["result"]["session"]["agent_state"], "waiting");
    drop(rpc);
    assert!(server.finish().is_empty());
}

#[test]
fn invalid_protocol_and_unknown_method_never_dispatch() {
    let h = support::Harness::new();
    h.paths().ensure().unwrap();
    let mut rpc = Helper::new(&h);
    rpc.write(json!({"protocol":2,"id":"bad","method":"ping"}));
    assert_eq!(rpc.read()["error"]["code"], "unsupported_protocol");
    assert_eq!(
        rpc.call("delete", json!({}))["error"]["notDispatched"],
        true
    );
    assert_eq!(
        rpc.call("capture", json!({"selector":"x","maxBytes":1048577}))["error"]["notDispatched"],
        true
    );
    assert_eq!(
        rpc.call("ping", json!([]))["error"]["code"],
        "invalid_request"
    );
}

#[test]
fn reconciliation_and_warning_ack_preserve_cli_outputs() {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Failed, None, b"failed");
    record.tag = "before".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let mut rpc = Helper::new(&h);
    let initial = rpc.call("list", json!({}));
    assert_eq!(initial["result"][0]["warning"]["kind"], "crash");
    // Simulate a dropped incremental event by preserving the old metadata
    // signature. A strict full scan must nevertheless repair the resident row.
    let file = std::fs::File::open(h.paths().record(record.id)).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    record.tag = "after!".into(); // same byte length
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    std::fs::File::options()
        .write(true)
        .open(h.paths().record(record.id))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if rpc.call("list", json!({}))["result"][0]["tag"] == "after!" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    aplexer::warnings::acknowledge_warnings(&h.paths(), &[record.id]).unwrap();
    loop {
        if rpc.call("list", json!({}))["result"][0]["warning"].is_null() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = h.command().args(["list", "--json"]).output().unwrap();
    assert_eq!(
        rpc.call("list", json!({}))["result"],
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    );
}

#[test]
fn psc_multiline_no_enter_is_framed_without_submission() {
    let h = support::Harness::new();
    let record = h.record(Phase::Running, Some(std::process::id()), b"");
    let server = worker::Worker::start(&record, false);
    let mut rpc = Helper::new(&h);
    let result = rpc.call(
        "send",
        json!({"selector":record.id,"selectorMode":"psc","text":"one\ntwo","submit":false}),
    );
    assert_eq!(result["result"]["bytes"], 7);
    assert_eq!(result["result"]["session"]["id"], record.id.to_string());
    drop(rpc);
    assert_eq!(
        server.finish(),
        vec![
            b"\x1b[200~".to_vec(),
            b"one\ntwo".to_vec(),
            b"\x1b[201~".to_vec()
        ]
    );
}

#[test]
fn live_holder_agent_detection_and_screen_capture_match_cli() {
    let h = support::Harness::new();
    let mut dead = h.record(Phase::Exited, None, b"");
    dead.tag = "claim".into();
    atomic_write_json(&h.paths().record(dead.id), &dead).unwrap();
    let mut live = h.record(Phase::Exiting, Some(std::process::id()), b"");
    live.tag = "claim".into();
    live.workload_pid = Some(std::process::id());
    live.agent_override = Some("codex".into());
    atomic_write_json(&h.paths().record(live.id), &live).unwrap();
    let server = worker::Worker::start(&live, false);
    let mut rpc = Helper::new(&h);
    let selected = rpc.call("status", json!({"selector":live.selector()}));
    assert_eq!(selected["result"]["id"], live.id.to_string());
    assert_eq!(selected["result"]["agent"], "codex");
    let listing = rpc.call("list", json!({"running":true}));
    let expected = h
        .command()
        .args(["list", "--running", "--json"])
        .output()
        .unwrap();
    let mut expected: Value = serde_json::from_slice(&expected.stdout).unwrap();
    let mut actual = listing["result"].clone();
    // CPU/process values are separately timed samples; the fake worker is
    // this test process, whose children change while the CLI is launched.
    assert!(actual[0]["processes"].as_u64().unwrap() > 0);
    for rows in [&mut expected, &mut actual] {
        for row in rows.as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("cpu_percent");
            row.as_object_mut().unwrap().remove("processes");
        }
    }
    assert_eq!(actual, expected);
    let expected = h
        .command()
        .args([
            "capture",
            &live.id.to_string(),
            "--screen",
            "--plain",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        rpc.call("capture", json!({"selector":live.id,"mode":"screenPlain"}))["result"],
        serde_json::from_slice::<Value>(&expected.stdout).unwrap()
    );
    drop(rpc);
    assert!(server.finish().is_empty());
}

#[test]
fn uncertain_send_is_never_reported_as_not_dispatched_or_replayed() {
    let h = support::Harness::new();
    let record = h.record(Phase::Running, Some(std::process::id()), b"");
    let server = worker::Worker::start(&record, true);
    let mut rpc = Helper::new(&h);
    rpc.write(json!({"protocol":1,"id":"uncertain","method":"send","params":{"selector":record.id,"payload":"one"}}));
    assert_eq!(rpc.read()["error"]["notDispatched"], false);
    rpc.write(json!({"protocol":1,"id":"uncertain","method":"send","params":{"selector":record.id,"payload":"one"}}));
    assert_eq!(rpc.read()["error"]["code"], "duplicate_id");
    drop(rpc);
    assert_eq!(server.finish(), vec![b"one".to_vec()]);
}

#[test]
fn registry_corruption_fails_closed_and_recovery_refreshes_rows() {
    let h = support::Harness::new();
    let record = h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h);
    assert!(rpc.call("list", json!({})).get("result").is_some());
    std::fs::write(h.paths().record(record.id), b"{").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let reply = rpc.call("list", json!({}));
        if reply.get("error").is_some() {
            assert_eq!(reply["error"]["code"], "registry_unavailable");
            assert_eq!(reply["error"]["notDispatched"], true);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    loop {
        let reply = rpc.call("list", json!({}));
        if reply.get("result").is_some() {
            assert_eq!(reply["result"][0]["id"], record.id.to_string());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
