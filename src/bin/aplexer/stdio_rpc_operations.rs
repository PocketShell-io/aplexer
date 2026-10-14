//! Request validation and UUID-pinned live worker operations.
use super::*;

pub(super) fn validate(call: &Call) -> Result<()> {
    if call.id.is_empty() || call.id.len() > 128 {
        bail!("id must be a nonempty string <=128 bytes");
    }
    if !call.params.is_null() && !call.params.is_object() {
        bail!("params must be an object");
    }
    match call.method.as_str() {
        "capabilities" | "ping" | "list" => {}
        "status" | "capture" | "send" => {
            if call.params["selector"].as_str().is_none() {
                bail!("selector must be a string");
            }
        }
        _ => bail!("unsupported method"),
    }
    for field in ["running", "submit"] {
        if let Some(v) = call.params.get(field) {
            if !v.is_boolean() {
                bail!("{field} must be a boolean");
            }
        }
    }
    for (field, allowed) in [("selectorMode", "psc"), ("format", "psc")] {
        if let Some(v) = call.params.get(field) {
            if v.as_str() != Some(allowed) {
                bail!("invalid {field}");
            }
        }
    }
    if call.method == "capture" {
        if let Some(v) = call.params.get("maxBytes") {
            if v.as_u64().is_none_or(|n| n > PAYLOAD_LIMIT as u64) {
                bail!("maxBytes must be an integer <=1 MiB");
            }
        }
        if call.params.get("mode").is_some_and(|v| !v.is_string()) {
            bail!("mode must be a string");
        }
        if !matches!(
            call.params["mode"].as_str().unwrap_or("history"),
            "history" | "screen" | "screenPlain"
        ) {
            bail!("invalid capture mode");
        }
    }
    if call.method == "send" {
        let text = call
            .params
            .get("payload")
            .or_else(|| call.params.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("payload must be a string"))?;
        if text.len() > PAYLOAD_LIMIT {
            bail!("send payload exceeds 1 MiB");
        }
        if call.params["submit"].as_bool().unwrap_or(false) {
            validate_submission_text(text.as_bytes())?;
        }
    }
    Ok(())
}

pub(super) fn execute(
    paths: &Paths,
    call: &Call,
    record: Option<SessionRecord>,
    row: Option<Value>,
    dispatched: &mut bool,
    info: &HelperInfo,
    state: &Arc<RwLock<Snapshot>>,
) -> Result<Value> {
    match call.method.as_str() {
        "capabilities" => {
            return Ok(
                json!({"protocol":1,"version":env!("CARGO_PKG_VERSION"),"pscSchema":3,"selectorModes":["aplexer","psc"],"versions":info.versions,"maxFrameBytes":FRAME_LIMIT,"maxSendBytes":PAYLOAD_LIMIT,"maxCaptureBytes":PAYLOAD_LIMIT,"methods":["capabilities","ping","list","status","capture","send"],"generation":info.generation}),
            )
        }
        "ping" => return Ok(json!({"ok":true,"generation":info.generation})),
        "list" => {
            let guard = state.read().unwrap();
            if let Some(e) = guard.unavailable() {
                bail!("{e}");
            }
            let mut rows = guard.rows.clone();
            if call.params["running"].as_bool().unwrap_or(false) {
                rows.as_array_mut().unwrap().retain(|r| {
                    r["worker_alive"] == true
                        && matches!(
                            r["phase"].as_str(),
                            Some("starting" | "running" | "exiting")
                        )
                });
            }
            if call.params["format"] == "psc" {
                let sessions: Vec<_> = guard
                    .records
                    .iter()
                    .filter_map(|r| {
                        let raw = rows
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|v| v["id"] == r.id.to_string())?;
                        psc::live(raw).then(|| psc::row(r, raw))
                    })
                    .collect();
                return Ok(json!({"schema":3,"sessions":sessions,"errors":[]}));
            }
            return Ok(rows);
        }
        _ => {}
    }
    // Pin the UUID once; a rename or replacement tag cannot redirect bytes.
    let selected = record.unwrap();
    let record = read_session_record(paths, selected.id)?;
    let psc_mode = call.params["selectorMode"] == "psc";
    let mut result = match call.method.as_str() {
        "status" => {
            let status = StatusData::load(record.clone());
            status.json_value_with_enrichment(row.as_ref())
        }
        "capture" => {
            let max = call.params["maxBytes"].as_u64().unwrap_or(16384) as usize;
            let mode = call.params["mode"].as_str().unwrap_or("history");
            let data = if mode == "history" {
                match rpc_capture(&record, Some(max)) {
                    Ok(data) => data,
                    Err(_) if record.worker_finished() || !record.worker_alive() => read_persisted_history_tail(&record.history_path, Some(max))?,
                    Err(e) => return Err(e).context("capture RPC failed while the worker process is still alive; refusing to return potentially stale persisted history"),
                }
            } else {
                match rpc_capture_screen(&record, mode == "screenPlain") {
                    Ok(data) => data,
                    Err(_)
                        if mode == "screenPlain"
                            && (record.worker_finished() || !record.worker_alive()) =>
                    {
                        fs::read(paths.screen_txt(record.id))?
                    }
                    Err(e) => return Err(e),
                }
            };
            if data.len() > max {
                bail!("capture exceeds maxBytes");
            }
            Ok(capture_json_value(&record, &data))
        }
        "send" => {
            check_attachable(&record)?;
            let mut data = call
                .params
                .get("payload")
                .or_else(|| call.params.get("text"))
                .and_then(Value::as_str)
                .unwrap()
                .as_bytes()
                .to_vec();
            let submit = call.params["submit"].as_bool().unwrap_or(false);
            *dispatched = true; // Conservatively uncertain from first worker attempt.
            let payload_bytes = data.len() + usize::from(submit);
            if psc_mode && data.iter().any(|b| matches!(b, b'\n' | b'\r')) {
                rpc_send(&record, b"\x1b[200~")?;
                rpc_send(&record, &data)?;
                rpc_send(&record, b"\x1b[201~")?;
                if submit {
                    thread::sleep(Duration::from_millis(300));
                    rpc_send(&record, b"\r")?;
                }
            } else if submit {
                data.push(b'\r');
                rpc_send_submitted(&record, &data, SubmissionKind::Text)?;
            } else if !data.is_empty() {
                rpc_send(&record, &data)?;
            }
            Ok(json!({"id":record.id,"bytes":payload_bytes}))
        }
        _ => unreachable!(),
    }?;
    if psc_mode {
        result["session"] = psc::row(&record, row.as_ref().unwrap_or(&Value::Null));
        if call.method == "capture" {
            result["text"] = result["utf8"].clone();
        }
    }
    Ok(result)
}
