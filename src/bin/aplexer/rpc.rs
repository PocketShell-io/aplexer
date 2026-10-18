use super::*;
use aplexer::delivery_ack::{self, AckPolicy, AckTarget, DeliveryReport, DeliveryStatus};
use aplexer::sys::ipc::Stream;

/// One control round-trip: connect, send `operation` (plus an optional data
/// frame), and read the matching response. Returns the stream too, for the
/// operations whose answer continues as a data frame (`read_data_frame`)
/// or as a subscription (`establish`).
pub(crate) fn rpc_call(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
) -> Result<(Stream, Value)> {
    rpc_call_within(record, operation, data, CONTROL_RPC_TIMEOUT)
}

/// `rpc_call` whose reply may legitimately take longer than the control
/// deadline: `Kill` answers only after the grace window and the SIGKILL
/// sweep (`api::kill_response_timeout`).
pub(crate) fn rpc_call_within(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
    response_timeout: Duration,
) -> Result<(Stream, Value)> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, operation);
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    if let Some(bytes) = data {
        write_frame(&mut stream, FrameKind::Data, bytes)?;
    }
    stream
        .set_read_timeout(Some(response_timeout))
        .context("set control response deadline")?;
    let result = read_response(&mut stream, &id)?;
    Ok((stream, result))
}

/// The data frame a response promised, named by `what` in the error.
pub(crate) fn read_data_frame(stream: &mut Stream, what: &str) -> Result<Vec<u8>> {
    let frame = read_frame(stream)?.ok_or_else(|| anyhow!("missing {what}"))?;
    if frame.kind != FrameKind::Data {
        bail!("expected {what}");
    }
    Ok(frame.payload)
}

pub(crate) fn rpc_simple(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
) -> Result<Value> {
    rpc_call(record, operation, data).map(|(_, result)| result)
}
pub(crate) fn rpc_send(record: &SessionRecord, data: &[u8]) -> Result<()> {
    rpc_simple(record, Operation::Send { bytes: data.len() }, Some(data))?;
    Ok(())
}

pub(crate) enum SubmissionKind {
    Raw,
    Text,
    FramedMessage,
    /// Always an explicit bracketed paste (stdio-RPC multi-line payloads).
    Paste,
}

pub(crate) fn validate_submission_text(data: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(data)
        .context("--enter expects UTF-8 text; use --raw or --hex for literal bytes")?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        bail!("--enter text contains terminal controls; use --raw or --hex for literal bytes");
    }
    Ok(())
}

/// How much of `aplexer::delivery_ack` a submitted send runs.
#[derive(Clone, Copy)]
pub(crate) enum AckMode {
    /// Paste and Enter once; never read the screen (`a send --no-ack`).
    Off,
    /// Wait for ready, verify landing and submission, retry, and wait up to
    /// this many ms for the agent to start working (0: skip that check).
    Verify {
        ack_timeout_ms: u64,
        ready_timeout_ms: u64,
        max_resends: u32,
    },
}

impl AckMode {
    /// Pane mail, deferred delivery and stdio RPC: landing and submission
    /// are verified with retries, without waiting for the turn to start.
    pub(crate) const SUBMIT: AckMode = AckMode::Verify {
        ack_timeout_ms: 0,
        ready_timeout_ms: 30_000,
        max_resends: 1,
    };
}

/// A session once it has been running this long is past its boot: its
/// screen gets a short readiness grace instead of the full ready timeout,
/// so an unrecognized layout (a dialog, a new TUI release) costs seconds,
/// not the boot budget, on every send.
const BOOT_WINDOW_MS: u64 = 180_000;
const SETTLED_READY_GRACE_MS: u64 = 3_000;

/// Submit agent input as text, then a distinct Enter event, and acknowledge
/// the delivery (`aplexer::delivery_ack`). Codex accepts an explicit
/// bracketed paste as one event and clears its paste-burst Enter
/// suppression state; plain text may still be draining through its
/// key-event queue after our wall-clock delay. OpenCode also needs explicit
/// framing to keep large bursts from disappearing in its raw key-event path.
/// Framed pane mail also honors a live workload's advertised paste mode,
/// even if the session engine is `shell`.
///
/// A delivery that fails (text never landed, or stayed in the input box
/// after every Enter) is an error; every other verdict is returned.
pub(crate) fn rpc_send_submitted(
    record: &SessionRecord,
    data: &[u8],
    kind: SubmissionKind,
    timing: SubmissionTiming,
) -> Result<DeliveryReport> {
    let report = deliver_submitted(record, data, kind, timing, AckMode::SUBMIT)?;
    if report.status == DeliveryStatus::Failed {
        bail!("{}", report.summary());
    }
    Ok(report)
}

/// `rpc_send_submitted` without mapping a failed verdict to an error, for
/// callers that report the verdict themselves (`a send --json`).
pub(crate) fn deliver_submitted(
    record: &SessionRecord,
    data: &[u8],
    kind: SubmissionKind,
    timing: SubmissionTiming,
    mode: AckMode,
) -> Result<DeliveryReport> {
    let Some((&b'\r', text)) = data.split_last() else {
        bail!("submitted input must end with carriage return");
    };
    let explicit_paste = match kind {
        SubmissionKind::Raw => false,
        SubmissionKind::Paste => true,
        SubmissionKind::Text => {
            matches!(aplexer::engine_family(&record.engine), "codex" | "opencode")
        }
        SubmissionKind::FramedMessage => {
            aplexer::engine_family(&record.engine) == "codex"
                || terminal_accepts_bracketed_paste(record)?
        }
    };
    let mut target = RpcAckTarget {
        record,
        text,
        explicit_paste,
        timing,
    };
    let payload = String::from_utf8_lossy(text);
    let AckMode::Verify {
        ack_timeout_ms,
        ready_timeout_ms,
        max_resends,
    } = mode
    else {
        // Engine "" has no screen model: paste once, Enter once.
        return delivery_ack::deliver(&mut target, "", &payload, AckPolicy::for_engine(""));
    };
    let booting = now_ms().saturating_sub(record.created_at_ms) < BOOT_WINDOW_MS;
    let policy = AckPolicy {
        ack_timeout_ms,
        max_resends,
        ready_timeout_ms: if booting {
            ready_timeout_ms
        } else {
            ready_timeout_ms.min(SETTLED_READY_GRACE_MS)
        },
        late_look_ms: if booting {
            AckPolicy::for_engine(&record.engine).late_look_ms
        } else {
            SETTLED_READY_GRACE_MS
        },
        ..AckPolicy::for_engine(&record.engine)
    };
    delivery_ack::deliver(&mut target, &record.engine, &payload, policy)
}

/// `delivery_ack::AckTarget` over the session's control socket.
struct RpcAckTarget<'a> {
    record: &'a SessionRecord,
    text: &'a [u8],
    explicit_paste: bool,
    timing: SubmissionTiming,
}

impl AckTarget for RpcAckTarget<'_> {
    /// Screen reads are best effort: a failed capture is "no evidence".
    fn capture(&mut self) -> Option<String> {
        let bytes = rpc_capture_screen(self.record, true).ok()?;
        String::from_utf8(bytes).ok()
    }

    /// Styled bytes for ghost-suggestion detection, best effort like
    /// `capture`: same snapshot the SVG renderer replays client-side.
    fn capture_styled(&mut self) -> Option<Vec<u8>> {
        rpc_capture_screen(self.record, false).ok()
    }

    /// Writes the payload in frame-sized chunks, inside one bracketed paste
    /// when the engine needs it, then waits the engine's floor (and settle)
    /// before the caller's Enter.
    fn paste(&mut self) -> Result<()> {
        if self.text.is_empty() {
            return Ok(());
        }
        if self.explicit_paste {
            rpc_send(self.record, b"\x1b[200~")?;
        }
        for chunk in self.text.chunks(MAX_FRAME_BYTES) {
            rpc_send(self.record, chunk)?;
        }
        if self.explicit_paste {
            rpc_send(self.record, b"\x1b[201~")?;
        }
        wait_before_enter(self.record, self.timing);
        Ok(())
    }

    fn enter(&mut self) -> Result<()> {
        rpc_send(self.record, b"\r")
    }

    fn reported_working_since(&mut self, since_ms: u64) -> bool {
        rpc_simple(self.record, Operation::Status, None)
            .ok()
            .and_then(|raw| serde_json::from_value::<SessionRecord>(raw).ok())
            .is_some_and(|live| {
                live.reported_state.as_deref() == Some("working")
                    && live.reported_state_at_ms.is_some_and(|at| at >= since_ms)
            })
    }

    fn sleep_ms(&mut self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }

    fn now_ms(&mut self) -> u64 {
        now_ms()
    }
}

/// The wall-clock gap between the last pasted byte and Enter: the timing's
/// floor delay, optionally extended by settle detection -- poll the
/// captured screen until the composer line stops changing (bounded), so a
/// large paste is not raced. Pure delay when settle is off.
pub(crate) fn wait_before_enter(record: &SessionRecord, timing: SubmissionTiming) {
    let floor = Duration::from_millis(timing.delay_ms);
    if !timing.settle {
        thread::sleep(floor);
        return;
    }
    let start = Instant::now();
    let give_up = floor + Duration::from_millis(SETTLE_MAX_WAIT_MS);
    let mut watcher = SettleWatcher::new();
    loop {
        let stable = watcher.observe(composer_snapshot(record));
        if start.elapsed() >= give_up || (start.elapsed() >= floor && stable >= SETTLE_STABLE_POLLS)
        {
            break;
        }
        thread::sleep(Duration::from_millis(SETTLE_POLL_MS));
    }
}

/// One settle observation of the target pane. An unreadable screen degrades
/// to the default snapshot, which reads as stability and fails the wait
/// open; the floor delay still applies.
fn composer_snapshot(record: &SessionRecord) -> ComposerSnapshot {
    rpc_capture_screen(record, false)
        .ok()
        .map(|bytes| {
            let mut parser = vt100::Parser::new(24, 80, 0);
            parser.process(&bytes);
            ComposerSnapshot::from_screen(parser.screen())
        })
        .unwrap_or_default()
}

/// The submission timing for one engine, from the state dir's config; a
/// config load failure degrades to the built-in family default so a broken
/// config file never blocks a send.
pub(crate) fn submission_timing(paths: &Paths, engine: &str) -> SubmissionTiming {
    Config::load(paths)
        .map(|config| config.submission_timing(engine))
        .unwrap_or_else(|_| SubmissionTiming::builtin(engine))
}

fn terminal_accepts_bracketed_paste(record: &SessionRecord) -> Result<bool> {
    let snapshot = rpc_capture_screen(record, false)?;
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(&snapshot);
    Ok(parser.screen().bracketed_paste())
}
pub(crate) fn rpc_capture(record: &SessionRecord, max: Option<usize>) -> Result<Vec<u8>> {
    let (mut stream, _) = rpc_call(record, Operation::Capture { max_bytes: max }, None)?;
    read_data_frame(&mut stream, "capture data")
}
/// `a capture --screen [--plain]` (docs/terminal-state-design.md section 8):
/// `rpc_capture`'s shape exactly, against `Operation::CaptureScreen`.
pub(crate) fn rpc_capture_screen(record: &SessionRecord, plain: bool) -> Result<Vec<u8>> {
    let (mut stream, _) = rpc_call(record, Operation::CaptureScreen { plain }, None)?;
    read_data_frame(&mut stream, "screen capture data")
}
