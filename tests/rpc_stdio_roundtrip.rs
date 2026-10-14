//! PSC-RPC-1 framed-stdio round trip that runs on every platform, including
//! the Windows CI job.
//!
//! `tests/stdio_rpc.rs` is `#![cfg(unix)]`, so the resident helper was never
//! exercised on Windows. This test spawns `a rpc --stdio --protocol 1` and
//! drives the hello / list / snapshot round trip over framed stdio: frame
//! headers are big-endian u32 lengths, stdout carries only frames, and an
//! exited record keeps the test deterministic (no live worker, no polling for
//! background refresh). The history deliberately mixes CRLF and LF so a
//! text-mode stdio regression on any platform breaks CLI parity loudly.
#[path = "support/messaging.rs"]
mod support;
use aplexer::Phase;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

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
            // Diagnostics go to stderr; never pipe it without a reader, or a
            // chatty helper could block on a full pipe.
            .stderr(Stdio::null())
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
        // Closing stdin delivers pipe EOF; the helper must exit on its own.
        self.input.take();
        if std::thread::panicking() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_round_trip_capabilities_list_status_capture() {
    let h = support::Harness::new();
    let record = h.record(Phase::Exited, None, b"hello\r\nworld\n");
    let mut rpc = Helper::new(&h);

    // Hello: capabilities identify protocol 1 and the full method surface.
    let capabilities = rpc.call("capabilities", json!({}));
    assert_eq!(capabilities["result"]["protocol"], 1);
    let methods = capabilities["result"]["methods"].as_array().unwrap();
    for method in ["capabilities", "ping", "list", "status", "capture", "send"] {
        assert!(
            methods.contains(&json!(method)),
            "capabilities missing method {method}: {capabilities}"
        );
    }
    let generation = capabilities["result"]["generation"].as_str().unwrap();

    // A live-helper round trip stays in the same generation.
    let ping = rpc.call("ping", json!({}));
    assert_eq!(ping["result"]["ok"], true);
    assert_eq!(ping["result"]["generation"], generation);

    // List matches `a list --json` exactly and contains the fixture record.
    let expected: Value = serde_json::from_slice(
        &h.command()
            .args(["list", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let listing = rpc.call("list", json!({}));
    assert_eq!(listing["result"], expected);
    assert!(
        listing["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == record.id.to_string()),
        "list is missing the fixture record: {listing}"
    );

    // Snapshot: status matches `a status --json` for the fixture record.
    let expected: Value = serde_json::from_slice(
        &h.command()
            .args(["status", &record.id.to_string(), "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        rpc.call("status", json!({"selector": record.id}))["result"],
        expected
    );

    // Capture matches `a capture --json`, CRLF intact.
    let expected: Value = serde_json::from_slice(
        &h.command()
            .args(["capture", &record.id.to_string(), "--json", "--bytes", "64"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        rpc.call("capture", json!({"selector": record.id, "maxBytes": 64}))["result"],
        expected
    );

    // Unknown methods never dispatch.
    let bad = rpc.call("delete", json!({}));
    assert_eq!(bad["error"]["code"], "unsupported_method");
    assert_eq!(bad["error"]["notDispatched"], true);
}
