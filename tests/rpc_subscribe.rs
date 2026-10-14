//! PSC-RPC-2 push subscriptions, on every platform (like
//! `tests/rpc_stdio_roundtrip.rs`, this file has no `cfg` gate so the
//! Windows CI job exercises the helper too).
//!
//! A background reader thread demultiplexes the single stdout stream:
//! responses carry `id`, events carry `event` and never `id`.
#[path = "support/messaging.rs"]
mod support;
use aplexer::{atomic_write_json, Phase};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Helper {
    child: Child,
    input: Option<ChildStdin>,
    rx: mpsc::Receiver<Option<Value>>,
    next: usize,
    pending: Vec<Value>,
    protocol: u32,
    /// When false, the reader thread stops draining stdout, so the pipe
    /// fills and the helper's bounded event queue overflows (gap test).
    flow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Helper {
    fn new(h: &support::Harness, protocol: u32) -> Self {
        let mut child = h
            .command()
            .args(["rpc", "--stdio", "--protocol", &protocol.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut output = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let flow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let gate = flow.clone();
        std::thread::spawn(move || loop {
            while !gate.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut header = [0; 4];
            if output.read_exact(&mut header).is_err() {
                break;
            }
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            if output.read_exact(&mut bytes).is_err() {
                break;
            }
            if tx.send(serde_json::from_slice(&bytes).ok()).is_err() {
                break;
            }
        });
        Self {
            input: child.stdin.take(),
            child,
            rx,
            next: 0,
            pending: Vec::new(),
            protocol,
            flow,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> String {
        self.next += 1;
        let id = self.next.to_string();
        let bytes = serde_json::to_vec(
            &json!({"protocol":self.protocol,"id":id,"method":method,"params":params}),
        )
        .unwrap();
        let input = self.input.as_mut().unwrap();
        input
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        input.write_all(&bytes).unwrap();
        input.flush().unwrap();
        id
    }

    fn set_flow(&mut self, flow: bool) {
        self.flow.store(flow, std::sync::atomic::Ordering::SeqCst);
    }

    fn recv_fresh(&mut self, timeout: Duration) -> Value {
        match self.rx.recv_timeout(timeout) {
            Ok(Some(value)) => value,
            Ok(None) => panic!("helper sent an unparsable frame"),
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("timed out waiting for a frame"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("helper exited mid-test"),
        }
    }

    fn take_from_stash(&mut self, want: impl Fn(&Value) -> bool) -> Option<Value> {
        self.pending
            .iter()
            .position(want)
            .map(|i| self.pending.remove(i))
    }

    fn expect_response(&mut self, id: &str, timeout: Duration) -> Value {
        let want = |f: &Value| f.get("id") == Some(&Value::String(id.to_owned()));
        if let Some(frame) = self.take_from_stash(want) {
            return frame;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let frame = self.recv_fresh(deadline.saturating_duration_since(Instant::now()));
            if want(&frame) {
                return frame;
            }
            self.pending.push(frame);
        }
    }

    fn expect_event(&mut self, event: &str, timeout: Duration) -> Value {
        let want = |f: &Value| f.get("event") == Some(&Value::String(event.to_owned()));
        if let Some(frame) = self.take_from_stash(want) {
            return frame;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let frame = self.recv_fresh(deadline.saturating_duration_since(Instant::now()));
            if want(&frame) {
                return frame;
            }
            self.pending.push(frame);
        }
    }

    fn expect_quiet(&mut self, quiet: Duration) {
        assert!(self.pending.is_empty(), "stale frames: {:?}", self.pending);
        match self.rx.recv_timeout(quiet) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(_) => panic!("expected no frame, got one"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("helper exited mid-test"),
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Closing stdin delivers pipe EOF; the helper must exit on its own.
        self.input.take();
        if std::thread::panicking() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// A live row for psc-format snapshots without a worker: a fresh Starting
/// record derives `starting` from its phase alone.
fn live_record(h: &support::Harness) -> aplexer::SessionRecord {
    let mut record = h.record(Phase::Starting, None, b"");
    record.created_at_ms = aplexer::now_ms();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    record
}

/// A screen-mirror fixture without a worker: a Running record whose worker
/// is gone falls back to `screen.txt` for `screenPlain` captures.
fn screen_record(h: &support::Harness, screen: &[u8]) -> aplexer::SessionRecord {
    let record = h.record(Phase::Running, None, b"");
    std::fs::write(h.paths().screen_txt(record.id), screen).unwrap();
    record
}

fn subscribe(rpc: &mut Helper, params: Value) -> (String, Value) {
    let id = rpc.send("subscribe", params);
    let response = rpc.expect_response(&id, Duration::from_secs(5));
    let result = response["result"].clone();
    (id, result)
}

#[test]
fn protocol_1_knows_no_subscribe_and_never_pushes() {
    let h = support::Harness::new();
    h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h, 1);
    // The protocol-1 request shape differs only in the version field.
    let id = rpc.send("capabilities", json!({}));
    let caps = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(caps["result"]["protocol"], 1);
    assert_eq!(
        caps["result"]["methods"],
        json!(["capabilities", "ping", "list", "status", "capture", "send"])
    );
    let id = rpc.send("subscribe", json!({"topics": ["sessions"]}));
    let reply = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(reply["error"]["code"], "unsupported_method");
    assert_eq!(reply["error"]["notDispatched"], true);
    // The channel still works, and every frame carries an id: no events.
    let id = rpc.send("ping", json!({}));
    let pong = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(pong["result"]["ok"], true);
    rpc.expect_quiet(Duration::from_millis(300));
}

#[test]
fn subscribe_answers_then_snapshots_matching_list() {
    let h = support::Harness::new();
    let record = h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h, 2);
    let caps_id = rpc.send("capabilities", json!({}));
    let caps = rpc.expect_response(&caps_id, Duration::from_secs(5));
    assert_eq!(caps["result"]["protocol"], 2);
    assert!(caps["result"]["methods"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "subscribe"));

    let (_, result) = subscribe(&mut rpc, json!({"topics": ["sessions"]}));
    let sub = result["subscription"].as_str().unwrap().to_owned();
    let snapshot = rpc.expect_event("snapshot", Duration::from_secs(5));
    assert_eq!(snapshot["subscription"], sub);
    assert_eq!(snapshot["topic"], "sessions");
    assert_eq!(snapshot["seq"], result["seq"]);
    assert!(snapshot.get("id").is_none(), "events never carry an id");
    let list_id = rpc.send("list", json!({}));
    let list = rpc.expect_response(&list_id, Duration::from_secs(5));
    assert_eq!(snapshot["data"], list["result"]);
    assert!(snapshot["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == record.id.to_string()));

    // psc format mirrors the schema-3 list instead of the raw array.
    let (_, psc_result) = subscribe(&mut rpc, json!({"topics": ["sessions"], "format": "psc"}));
    let psc_snapshot = rpc.expect_event("snapshot", Duration::from_secs(5));
    assert_eq!(psc_snapshot["data"]["schema"], 3);
    assert_eq!(psc_snapshot["seq"], psc_result["seq"]);
}

#[test]
fn registry_change_triggers_a_snapshot() {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h, 2);
    let (_, result) = subscribe(&mut rpc, json!({"topics": ["sessions"]}));
    rpc.expect_event("snapshot", Duration::from_secs(5));
    let before = Instant::now();
    record.tag = "changed-tag".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let snapshot = rpc.expect_event("snapshot", Duration::from_secs(5));
    assert!(
        before.elapsed() < Duration::from_secs(2),
        "snapshot took {:?}",
        before.elapsed()
    );
    assert!(snapshot["seq"].as_u64().unwrap() > result["seq"].as_u64().unwrap());
    assert!(snapshot["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["tag"] == "changed-tag"));
}

#[test]
fn rapid_changes_coalesce_into_one_snapshot() {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h, 2);
    subscribe(&mut rpc, json!({"topics": ["sessions"]}));
    rpc.expect_event("snapshot", Duration::from_secs(5));
    // A burst of rewrites must collapse: the publisher emits at most one
    // snapshot per 50 ms, so consecutive emissions stay ~50 ms apart no
    // matter how fast the registry changes underneath.
    record.tag = "tag-a".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let first_arrived = Instant::now();
    let first = rpc.expect_event("snapshot", Duration::from_secs(5));
    assert!(first["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["tag"] == "tag-a"));
    // A second change immediately after the first emission cannot produce
    // another snapshot until the coalescing window passes.
    record.tag = "tag-b".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let second = rpc.expect_event("snapshot", Duration::from_secs(5));
    assert!(second["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["tag"] == "tag-b"));
    let gap = first_arrived.elapsed();
    assert!(
        gap >= Duration::from_millis(35),
        "snapshots only {gap:?} apart: the 50 ms coalescing window was not honored"
    );
    // Nothing else changed: the channel goes quiet (heartbeats start at 5 s).
    rpc.expect_quiet(Duration::from_millis(400));
}

#[test]
fn screen_topic_pushes_updates_and_ends() {
    let h = support::Harness::new();
    let record = screen_record(&h, b"screen-one");
    let mut rpc = Helper::new(&h, 2);
    let topic = format!("screen:{}", record.id);
    let (_, result) = subscribe(&mut rpc, json!({"topics": [topic]}));
    let sub = result["subscription"].as_str().unwrap().to_owned();
    let first = rpc.expect_event("screen", Duration::from_secs(5));
    assert_eq!(first["subscription"], sub);
    assert_eq!(first["topic"], topic);
    assert_eq!(first["data"]["utf8"], "screen-one");

    std::fs::write(h.paths().screen_txt(record.id), b"screen-two").unwrap();
    let second = rpc.expect_event("screen", Duration::from_secs(5));
    assert_eq!(second["data"]["utf8"], "screen-two");
    assert!(second["seq"].as_u64().unwrap() > first["seq"].as_u64().unwrap());

    // Ending the session ends the topic, then silence even on new bytes.
    let mut ended_record = record.clone();
    ended_record.phase = Phase::Exited;
    atomic_write_json(&h.paths().record(record.id), &ended_record).unwrap();
    let ended = rpc.expect_event("ended", Duration::from_secs(5));
    assert_eq!(ended["subscription"], sub);
    assert_eq!(ended["topic"], topic);
    assert!(ended.get("data").is_none());
    std::fs::write(h.paths().screen_txt(record.id), b"screen-three").unwrap();
    rpc.expect_quiet(Duration::from_millis(400));
}

#[test]
fn unsubscribe_stops_events() {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h, 2);
    let (_, result) = subscribe(&mut rpc, json!({"topics": ["sessions"]}));
    let sub = result["subscription"].as_str().unwrap().to_owned();
    rpc.expect_event("snapshot", Duration::from_secs(5));
    let id = rpc.send("unsubscribe", json!({"subscription": sub}));
    let reply = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(reply["result"]["unsubscribed"], sub);
    record.tag = "after-unsub".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    rpc.expect_quiet(Duration::from_millis(500));
    let id = rpc.send("unsubscribe", json!({"subscription": sub}));
    let reply = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(reply["error"]["code"], "invalid_request");
}

#[test]
fn subscription_limits_and_bad_selectors_fail_closed() {
    let h = support::Harness::new();
    // All fixtures exist before the helper spawns: the resident snapshot
    // loads them synchronously, so no refresh race can flake resolution.
    let mut screen_ids = Vec::new();
    for _ in 0..17 {
        let record = screen_record(&h, b"x");
        screen_ids.push(record.id.to_string());
    }
    let mut rpc = Helper::new(&h, 2);
    for bad in [
        json!({"topics": ["nope"]}),
        json!({"topics": []}),
        json!({"topics": ["sessions", "sessions"]}),
        json!({"topics": ["screen:"]}),
        json!({"topics": ["sessions"], "format": "bogus"}),
    ] {
        let id = rpc.send("subscribe", bad);
        let reply = rpc.expect_response(&id, Duration::from_secs(5));
        assert_eq!(reply["error"]["code"], "invalid_request");
        assert_eq!(reply["error"]["notDispatched"], true);
    }
    let id = rpc.send(
        "subscribe",
        json!({"topics": ["screen:00000000-0000-0000-0000-000000000000"]}),
    );
    let reply = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(reply["error"]["code"], "not_found");
    assert_eq!(reply["error"]["notDispatched"], true);

    // Sixteen screen topics are accepted; the seventeenth is refused.
    let topics: Vec<String> = screen_ids[..16]
        .iter()
        .map(|id| format!("screen:{id}"))
        .collect();
    let (_, result) = subscribe(&mut rpc, json!({"topics": topics}));
    assert!(result["subscription"].as_str().is_some());
    let id = rpc.send(
        "subscribe",
        json!({"topics": [format!("screen:{}", screen_ids[16])]}),
    );
    let reply = rpc.expect_response(&id, Duration::from_secs(5));
    assert_eq!(reply["error"]["code"], "invalid_request");
}

#[test]
fn dropped_frames_resume_with_gap_and_latest_state() {
    let h = support::Harness::new();
    let mut record = h.record(Phase::Exited, None, b"");
    let mut rpc = Helper::new(&h, 2);
    subscribe(&mut rpc, json!({"topics": ["sessions"]}));
    rpc.expect_event("snapshot", Duration::from_secs(5));
    // Stop draining: emissions outpace the deaf client until the bounded
    // queue overflows and the gap flag is set.
    rpc.set_flow(false);
    let hammered = Instant::now();
    let mut n = 0;
    while hammered.elapsed() < Duration::from_secs(6) {
        record.tag = format!("hammer-{n}");
        atomic_write_json(&h.paths().record(record.id), &record).unwrap();
        n += 1;
    }
    rpc.set_flow(true);
    // The reader may have drained pre-drop frames first; the gap marker is
    // the first frame the helper popped after dropping.
    let mut found = false;
    for _ in 0..300 {
        let frame = rpc.expect_event("snapshot", Duration::from_secs(15));
        if frame.get("gap") == Some(&Value::Bool(true)) {
            found = true;
            break;
        }
    }
    assert!(found, "no frame carried gap:true");
    // After resuming, the next change arrives clean and current. Stale
    // backlog frames may still precede it, so scan to the final tag.
    record.tag = "final-tag".into();
    atomic_write_json(&h.paths().record(record.id), &record).unwrap();
    let mut fresh = None;
    for _ in 0..50 {
        let frame = rpc.expect_event("snapshot", Duration::from_secs(10));
        if frame["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["tag"] == "final-tag")
        {
            fresh = Some(frame);
            break;
        }
    }
    let fresh = fresh.expect("never saw the final snapshot");
    assert!(fresh.get("gap").is_none());
}

#[test]
fn heartbeat_bounds_replica_age() {
    let h = support::Harness::new();
    live_record(&h);
    let mut rpc = Helper::new(&h, 2);
    let caps_id = rpc.send("capabilities", json!({}));
    let caps = rpc.expect_response(&caps_id, Duration::from_secs(5));
    let generation = caps["result"]["generation"].as_str().unwrap().to_owned();
    subscribe(&mut rpc, json!({"topics": ["sessions"], "format": "psc"}));
    let snapshot = rpc.expect_event("snapshot", Duration::from_secs(5));
    let heartbeat = rpc.expect_event("heartbeat", Duration::from_secs(12));
    assert_eq!(heartbeat["subscription"], snapshot["subscription"]);
    assert_eq!(heartbeat["topic"], "sessions");
    assert_eq!(heartbeat["generation"], generation);
    assert!(heartbeat["seq"].as_u64().unwrap() > snapshot["seq"].as_u64().unwrap());
}
