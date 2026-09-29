use super::*;

/// One control round-trip: connect, send `operation` (plus an optional data
/// frame), and read the matching response. Returns the stream too, for the
/// operations whose answer continues as a data frame (`read_data_frame`)
/// or as a subscription (`establish`).
pub(crate) fn rpc_call(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
) -> Result<(UnixStream, Value)> {
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
) -> Result<(UnixStream, Value)> {
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
pub(crate) fn read_data_frame(stream: &mut UnixStream, what: &str) -> Result<Vec<u8>> {
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

/// Submit agent input as text, then a distinct Enter event. Codex accepts an
/// explicit bracketed paste as one event and clears its paste-burst Enter
/// suppression state; plain text may still be draining through its key-event
/// queue after our wall-clock delay. Other engines keep their raw text input.
pub(crate) fn rpc_send_submitted(
    record: &SessionRecord,
    data: &[u8],
    text_input: bool,
) -> Result<()> {
    let Some((&b'\r', text)) = data.split_last() else {
        bail!("submitted input must end with carriage return");
    };
    let explicit_paste = text_input && aplexer::engine_family(&record.engine) == "codex";
    if explicit_paste && !text.is_empty() {
        rpc_send(record, b"\x1b[200~")?;
    }
    for chunk in text.chunks(MAX_FRAME_BYTES) {
        rpc_send(record, chunk)?;
    }
    if explicit_paste && !text.is_empty() {
        rpc_send(record, b"\x1b[201~")?;
    }
    if !text.is_empty() {
        thread::sleep(Duration::from_millis(300));
    }
    rpc_send(record, b"\r")
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
