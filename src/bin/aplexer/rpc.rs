use super::*;
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
}

/// What an Enter-terminated write achieved, as far as aplexer can observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Submission {
    /// The draft was seen leaving an agent composer (`composer.rs`): the
    /// agent took it, as a new turn or into its own queue while busy.
    Submitted,
    /// Text and Enter reached a PTY whose input cannot be observed (a shell
    /// or an unpinned TUI); whether it was submitted is unknown.
    Injected,
}

impl Submission {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Injected => "injected",
        }
    }

    pub(crate) fn status(self) -> SubmissionStatus {
        match self {
            Self::Submitted => SubmissionStatus::Submitted,
            Self::Injected => SubmissionStatus::Injected,
        }
    }
}

/// Text above this many bytes is pasted, never typed. A PTY hands an agent
/// a few KiB per read, so a longer typed burst can render half-drained and
/// take the following Enter into its tail; a bracketed paste stays one event
/// however it is split. Claude already treats a typed burst this long as a
/// paste, so nothing it would have read as typed is lost.
const TYPED_TEXT_LIMIT: usize = 1024;

/// Submit agent input as text, then a distinct Enter event. Codex accepts an
/// explicit bracketed paste as one event and clears its paste-burst Enter
/// suppression state; framed pane mail also honors a live workload's
/// advertised paste mode, even if the session engine is `shell`. Short plain
/// text for Claude stays typed: Claude hands bracketed pastes to the model as
/// pasted content, not as the operator's own words.
///
/// Text for Claude and Codex composers is observed (`submit_observed`), so
/// Enter follows the rendered text rather than a guess at the agent's input
/// latency, and success means the draft left the composer. Raw key bytes and
/// a bare Enter are keystrokes the caller chose (a menu answer, a dialog
/// confirmation), so they -- like every other target -- get the blind write
/// with a pause sized to Codex's 120 ms paste window.
pub(crate) fn rpc_send_submitted(
    record: &SessionRecord,
    data: &[u8],
    kind: SubmissionKind,
) -> Result<Submission> {
    let Some((&b'\r', text)) = data.split_last() else {
        bail!("submitted input must end with carriage return");
    };
    let raw = matches!(kind, SubmissionKind::Raw);
    if observable_composer(record) && !raw && !text.is_empty() {
        let before = capture_composer(record)?;
        let framed = matches!(kind, SubmissionKind::FramedMessage);
        let paste = aplexer::engine_family(&record.engine) == "codex"
            || (before.bracketed_paste && (framed || text.len() > TYPED_TEXT_LIMIT));
        submit_observed(record, &before, text, paste)?;
        return Ok(Submission::Submitted);
    }
    let explicit_paste = match kind {
        SubmissionKind::Raw | SubmissionKind::Text => false,
        SubmissionKind::FramedMessage => terminal_accepts_bracketed_paste(record)?,
    };
    write_text(record, text, explicit_paste)?;
    if !text.is_empty() {
        thread::sleep(Duration::from_millis(300));
    }
    rpc_send(record, b"\r")?;
    Ok(Submission::Injected)
}

fn terminal_accepts_bracketed_paste(record: &SessionRecord) -> Result<bool> {
    Ok(capture_composer(record)?.bracketed_paste)
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
