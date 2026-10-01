use super::*;

// Hard bounds for everything the report embeds. `--last` and `--max-bytes`
// override the two defaults; these constants are the fixed per-item caps,
// so the report can never balloon no matter what the session's transcript
// or history contains.
const EVENT_TEXT_CLIP: usize = 2_000;
const GIT_ENTRY_LIMIT: usize = 200;
const MESSAGE_LIMIT: usize = 10;

/// `a handoff [SESSION] [--engine E --path F] [--last N] [--max-bytes B]`
/// -- one read-only recovery bundle for another agent (or a human) to
/// recover a session from (issue #20): who the session is, what it was
/// working on, where its real evidence lives, and which parts of that
/// evidence are known-incomplete. Mirrors `a transcript`'s explicit-source
/// rules (`--engine`/`--path` reuse phase-1 validation and never touch the
/// bind sidecar), queries the live worker exactly like `a status`, and
/// streams everything to stdout: the only state-dir write is the
/// best-effort transcript bind, whose failure is reported as a gap rather
/// than failing the report. The workload is never interacted with.
pub(crate) fn cmd_handoff(paths: &Paths, args: HandoffArgs, json_output: bool) -> Result<()> {
    let record = resolve_transcript_target_record(paths, &args.target)?;
    // One status round-trip gives the live facts (`worker_reachable`,
    // history/record persistence errors, foreground command) and falls back
    // to the persisted record when the worker is gone -- the same query
    // `a status` makes, so the two can never disagree.
    let mut status = StatusData::load(record.clone());
    aplexer::warnings::sweep_warnings(paths);
    status.warning = aplexer::warnings::load_warning_for(paths, status.current.id);

    let current = status.current.clone();
    let session = status.json_value()?;
    let live_agent = live_agent(paths, &current);
    let (recovery, staleness_detail) = recovery_section(&current, &status);
    let transcript = transcript_section(paths, &current, &args, live_agent);
    let pty_tail = pty_tail_section(&current, &status, args.max_bytes);
    let screen = screen_section(paths, &current, &status, args.max_bytes);
    let workspace = workspace_section(paths, &current);
    let gaps = gaps_section(
        &status,
        &transcript,
        &pty_tail,
        &screen,
        staleness_detail.as_deref(),
    );

    let value = json!({
        "session": session,
        "recovery": recovery,
        "transcript": transcript.value,
        "pty_tail": pty_tail.value,
        "screen": screen.value,
        "workspace": workspace,
        "gaps": gaps,
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_human_handoff(&current, &status, &transcript, &pty_tail, &screen, &workspace, &gaps);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Live agent (identity-backed binding)
// ---------------------------------------------------------------------

/// The top-level agent process live in this session, if any: the anchor for
/// identity-backed transcript binding. Detection is the same `/proc` walk
/// `a status` reports (`agent`/`agent_profile`), extended with the pid --
/// only that process's open file descriptors are read, so a nested agent's
/// rollout can never be bound to the session.
fn live_agent(paths: &Paths, record: &SessionRecord) -> Option<aplexer::agent_events::LiveAgent> {
    let workload_pid = record.workload_pid?;
    if !record.worker_phase_active() {
        return None;
    }
    // The same variant table `a status`'s detection uses: configured
    // variations (zcodex...) classify, an unloadable config degrades to the
    // canonical agents.
    let variants = aplexer::Config::load(paths)
        .map(|config| aplexer::agent_kind::profile_variants(&config))
        .unwrap_or_default();
    aplexer::agent_kind::detect_agent_process(
        Path::new(aplexer::agent_kind::DEFAULT_PROC_ROOT),
        workload_pid,
        &variants,
    )
    .map(|agent| aplexer::agent_events::LiveAgent {
        pid: agent.pid,
        kind: agent.detected.kind,
    })
}

// ---------------------------------------------------------------------
// Recovery reason
// ---------------------------------------------------------------------

/// The observable reason this session needs (or deserves) a handoff, from
/// evidence only: lifecycle phase, worker liveness, control-plane
/// reachability, persistence errors. Never a guess about *why* the agent
/// stopped -- that lives in the transcript and the gaps.
fn recovery_section(record: &SessionRecord, status: &StatusData) -> (Value, Option<String>) {
    let mut detail = String::new();
    let state: String = if record.worker_alive() && !status.worker_reachable {
        detail.push_str(
            "the worker process is running but did not answer its control socket; \
             persisted evidence below may be stale and must not be treated as current",
        );
        "worker_unreachable".to_string()
    } else {
        match record.phase {
            Phase::Exited | Phase::Failed => {
                if let Some(exit) = &record.exit {
                    detail.push_str(&format!(
                        "exit code={:?} signal={:?} oom_killed={}",
                        exit.code, exit.signal, exit.oom_killed
                    ));
                }
                if let Some(error) = &record.error {
                    if !detail.is_empty() {
                        detail.push_str("; ");
                    }
                    detail.push_str(error);
                }
                match record.phase {
                    Phase::Failed => "failed".to_string(),
                    _ => "exited".to_string(),
                }
            }
            Phase::Starting => "starting".to_string(),
            Phase::Running => "running".to_string(),
            Phase::Exiting => "exiting".to_string(),
        }
    };
    let mut staleness = None;
    if let Some(error) = &status.history_persistence_error {
        let note = format!("history persistence is failing: {error}");
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        detail.push_str(&note);
        staleness = Some(note);
    }
    (
        json!({
            "state": state,
            "detail": detail,
            "worker_alive": record.worker_alive(),
            "worker_reachable": status.worker_reachable,
        }),
        staleness,
    )
}

// ---------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------

/// The transcript section: discovery (explicit source, bind sidecar,
/// live-fd exact binding, or heuristic), the bounded recent-conversation
/// window with last-user/last-assistant turns, and the full-log path +
/// byte cursor so the consumer can page the rest itself (`a transcript
/// --after`). Truthful by construction: when the log was not found, or
/// several candidates remain, `discovered` says so and `error`/`candidates`
/// carry the actionable detail instead of a plausible-looking window.
fn transcript_section(
    paths: &Paths,
    record: &SessionRecord,
    args: &HandoffArgs,
    live_agent: Option<aplexer::agent_events::LiveAgent>,
) -> Section {
    let mut value = json!({
        "discovered": false,
        "source": "none",
        "window_events": args.last,
        "max_line_bytes": DEFAULT_HANDOFF_MAX_LINE_BYTES,
    });
    let resolution = if let Some(explicit) = &args.path {
        match explicit_source(record, &args.engine.clone(), explicit) {
            Ok((path, engine)) => {
                // The log's own session id is part of "which log is this":
                // quote it even though no bind is read or written.
                let engine_session_id =
                    aplexer::agent_events::peek_continuation(&engine, &path);
                aplexer::agent_events::TranscriptResolution {
                    path: Some(path),
                    engine: Some(engine),
                    source: "explicit".into(),
                    engine_session_id,
                    ..Default::default()
                }
            }
            Err(error) => aplexer::agent_events::TranscriptResolution {
                source: "none".into(),
                error: Some(format!("{error:#}")),
                ..Default::default()
            },
        }
    } else {
        let bind_path = paths.state_session(record.id).join("transcript.json");
        aplexer::agent_events::resolve_transcript_detailed(record, &bind_path, Path::new(aplexer::agent_kind::DEFAULT_PROC_ROOT), live_agent)
    };
    let source = if resolution.source.is_empty() {
        "none"
    } else {
        resolution.source.as_str()
    };
    value["source"] = json!(source);
    value["bind"] = match &resolution.bind {
        Some(bind) => json!(bind),
        None => Value::Null,
    };
    value["error"] = match &resolution.error {
        Some(error) => json!(error),
        None => Value::Null,
    };
    if !resolution.candidates.is_empty() {
        value["candidates"] = json!(
            resolution.candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
        );
    }
    let Some(path) = resolution.path else {
        return Section {
            value,
            ok: false,
            detail: resolution
                .error
                .clone()
                .unwrap_or_else(|| "transcript not found".into()),
        };
    };
    let engine = resolution.engine.clone().unwrap_or_else(|| record.engine.clone());
    value["discovered"] = json!(true);
    value["engine"] = json!(engine);
    value["path"] = json!(path.display().to_string());
    let file_bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    // The completeness contract: everything outside the embedded window is
    // still in the file at `path`, behind this cursor, pageable with
    // `a transcript --after/--before`.
    value["file_bytes"] = json!(file_bytes);
    if let Some(native) = &resolution.engine_session_id {
        value["native_session_id"] = json!(native);
    }
    // Bounded recent conversation: stream the log, keep only the last
    // `--last` events, clip every text field.
    match aplexer::agent_events::read_transcript_tail(
        &engine,
        &path,
        args.last,
        Some(DEFAULT_HANDOFF_MAX_LINE_BYTES),
    ) {
        Ok(events) => {
            let last_user = events
                .iter()
                .rev()
                .find(|e| e.kind == "message" && e.role.as_deref() == Some("user"))
                .map(|e| clip_value(&e.content, EVENT_TEXT_CLIP));
            let last_assistant = events
                .iter()
                .rev()
                .find(|e| e.kind == "message" && e.role.as_deref() == Some("assistant"))
                .map(|e| clip_value(&e.content, EVENT_TEXT_CLIP));
            value["last_user_message"] = last_user.unwrap_or(Value::Null);
            value["last_assistant_message"] = last_assistant.unwrap_or(Value::Null);
            value["events"] = Value::Array(
                events
                    .iter()
                    .map(|event| slim_event(event, EVENT_TEXT_CLIP))
                    .collect(),
            );
        }
        Err(error) => {
            // The log exists but could not be parsed -- say so, with the
            // path still attached so the consumer can retry explicitly.
            value["parse_error"] = json!(format!("{error:#}"));
        }
    }
    let mut detail = format!(
        "{} (engine {engine}, {file_bytes} bytes{})",
        path.display(),
        resolution
            .engine_session_id
            .as_deref()
            .map(|id| format!(", native session {id}"))
            .unwrap_or_default()
    );
    if let Some(bind) = &resolution.bind {
        if let Some(error) = &bind.write_error {
            detail.push_str(&format!(
                "; bind NOT recorded ({}): {error}",
                bind.path.display()
            ));
        }
    }
    Section { value, ok: true, detail }
}

/// `a handoff --engine/--path`: the same validation `a transcript` applies
/// (phase-1 rules), without reading or writing the bind sidecar.
fn explicit_source(
    record: &SessionRecord,
    engine_arg: &Option<String>,
    explicit: &Path,
) -> Result<(PathBuf, String)> {
    let engine = engine_arg.clone().unwrap_or_else(|| record.engine.clone());
    aplexer::agent_events::validate_transcript_engine(&engine)?;
    let path = fs::canonicalize(explicit)
        .with_context(|| format!("transcript path {} is unavailable", explicit.display()))?;
    if !path.is_file() {
        bail!("transcript path {} is not a regular file", explicit.display());
    }
    fs::File::open(&path)
        .with_context(|| format!("cannot read transcript path {}", explicit.display()))?;
    Ok((path, engine))
}

fn slim_event(event: &aplexer::watch::UnifiedEvent, clip: usize) -> Value {
    let (content, clipped) = clip_text(&event.content, clip);
    json!({
        "kind": event.kind,
        "sequence": event.sequence,
        "timestamp": event.timestamp,
        "role": event.role,
        "content": content,
        "content_clipped": clipped,
        "tool_name": event.tool_name,
        "error": event.error,
    })
}

/// A string clipped to `max` bytes on a char boundary, with an honest flag
/// when the cut happened (never a silent ellipsis).
fn clip_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn clip_value(text: &str, max: usize) -> Value {
    let (content, clipped) = clip_text(text, max);
    if clipped {
        json!({"text": content, "clipped": true})
    } else {
        json!(content)
    }
}

// ---------------------------------------------------------------------
// PTY tail + screen
// ---------------------------------------------------------------------

/// The bounded raw PTY tail. A reachable worker serves it live (never
/// stale); a dead worker's persisted tail is authoritative post-mortem
/// evidence; a live-but-unreachable worker is reported unavailable -- the
/// persisted tail is NOT substituted, so stale bytes can never pose as
/// current output (issue #20).
fn pty_tail_section(record: &SessionRecord, status: &StatusData, cap: usize) -> Section {
    let mut value = json!({
        "window_bytes": cap,
        "history_path": record.history_path.display().to_string(),
    });
    let worker_gone =
        matches!(record.phase, Phase::Exited | Phase::Failed) || !record.worker_alive();
    let (ok, detail) = if status.worker_reachable {
        match rpc_capture(record, Some(cap)) {
            Ok(data) => {
                let lossy = std::str::from_utf8(&data).is_err();
                let truncated = data.len() >= cap;
                let (text, _) = clip_text(&String::from_utf8_lossy(&data), cap);
                value["source"] = json!("live");
                value["bytes"] = json!(data.len());
                value["truncated"] = json!(truncated);
                value["text"] = json!(text);
                value["lossy"] = json!(lossy);
                (
                    true,
                    format!("live tail, {} bytes (capped at {cap})", data.len()),
                )
            }
            Err(error) => {
                value["source"] = json!("unavailable");
                value["error"] = json!(format!("{error:#}"));
                (false, format!("live capture failed: {error:#}"))
            }
        }
    } else if worker_gone {
        match aplexer::read_persisted_history_tail(&record.history_path, Some(cap)) {
            Ok(data) => {
                let truncated = data.len() >= cap;
                let (text, _) = clip_text(&String::from_utf8_lossy(&data), cap);
                value["source"] = json!("persisted");
                value["bytes"] = json!(data.len());
                value["truncated"] = json!(truncated);
                value["text"] = json!(text);
                (
                    true,
                    format!(
                        "persisted tail, {} bytes (capped at {cap}); worker is gone, this is the last durable state",
                        data.len()
                    ),
                )
            }
            Err(error) => {
                value["source"] = json!("unavailable");
                value["error"] = json!(format!("{error:#}"));
                (false, format!("persisted history unreadable: {error:#}"))
            }
        }
    } else {
        value["source"] = json!("unavailable");
        value["stale_risk"] = json!(true);
        (
            false,
            "worker is alive but unreachable; the persisted tail was withheld because it may be stale"
                .to_string(),
        )
    };
    Section { value, ok, detail }
}

/// The rendered current screen as separate evidence (never merged into the
/// raw tail): live grid from the worker, or the plain-text screen.txt the
/// worker wrote at exit -- the same fallback `a capture --screen --plain`
/// uses. Unavailable (not stale-substituted) while the worker is merely
/// unreachable.
fn screen_section(paths: &Paths, record: &SessionRecord, status: &StatusData, cap: usize) -> Section {
    let mut value = json!({"window_bytes": cap});
    let worker_gone =
        matches!(record.phase, Phase::Exited | Phase::Failed) || !record.worker_alive();
    let (ok, detail) = if status.worker_reachable {
        match rpc_capture_screen(record, true) {
            Ok(data) => {
                let (text, _) = clip_text(&String::from_utf8_lossy(&data), cap);
                value["source"] = json!("live");
                value["bytes"] = json!(data.len());
                value["text"] = json!(text);
                (true, format!("live screen, {} bytes", data.len()))
            }
            Err(error) => {
                value["source"] = json!("unavailable");
                value["error"] = json!(format!("{error:#}"));
                (false, format!("screen capture failed: {error:#}"))
            }
        }
    } else if worker_gone {
        match fs::read(paths.screen_txt(record.id)) {
            Ok(data) => {
                let (text, _) = clip_text(&String::from_utf8_lossy(&data), cap);
                value["source"] = json!("persisted");
                value["bytes"] = json!(data.len());
                value["text"] = json!(text);
                (
                    true,
                    format!("persisted screen.txt, {} bytes (as of worker exit)", data.len()),
                )
            }
            Err(error) => {
                value["source"] = json!("unavailable");
                value["error"] = json!(format!("{error}"));
                (false, format!("no persisted screen: {error}"))
            }
        }
    } else {
        value["source"] = json!("unavailable");
        value["stale_risk"] = json!(true);
        (
            false,
            "worker is alive but unreachable; the persisted screen was withheld because it may be stale"
                .to_string(),
        )
    };
    Section { value, ok, detail }
}

// ---------------------------------------------------------------------
// Workspace artifacts
// ---------------------------------------------------------------------

/// Durable, read-only pointers to what the session was working in: git
/// branch/head/changed paths (names and status codes only -- never diffs),
/// and the workspace mailbox messages addressed to this session (unread
/// first). Both degrade to an honest unavailable note instead of failing
/// the report.
fn workspace_section(paths: &Paths, record: &SessionRecord) -> Value {
    json!({
        "cwd": record.cwd.display().to_string(),
        "git": git_section(&record.cwd),
        "messages": messages_section(paths, record),
    })
}

fn git_section(cwd: &Path) -> Value {
    let git = |git_args: &[&str]| -> Result<String> {
        let output = Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(cwd)
            .args(git_args)
            .output()
            .map_err(|error| anyhow!("git unavailable: {error}"))?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                git_args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let mut value = json!({"available": false});
    match git(&["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(branch) => {
            value["available"] = json!(true);
            value["branch"] = json!(branch.trim());
            value["head"] = json!(git(&["rev-parse", "--short=12", "HEAD"])
                .map(|head| head.trim().to_string())
                .unwrap_or_default());
            match git(&["status", "--porcelain"]) {
                Ok(status) => {
                    let entries: Vec<String> = status.lines().map(str::to_string).collect();
                    let truncated = entries.len() > GIT_ENTRY_LIMIT;
                    value["changed_paths"] =
                        json!(entries.into_iter().take(GIT_ENTRY_LIMIT).collect::<Vec<_>>());
                    value["truncated"] = json!(truncated);
                }
                Err(error) => value["status_error"] = json!(format!("{error:#}")),
            }
        }
        Err(error) => value["error"] = json!(format!("{error:#}")),
    }
    value
}

fn messages_section(paths: &Paths, record: &SessionRecord) -> Value {
    // Read-only: message_paths derives locations without creating them, and
    // an absent mailbox is the documented empty state, not an error.
    let mp = aplexer::messaging::message_paths(paths, &record.workspace);
    if !mp.msgs_dir.is_dir() {
        return json!({"available": false, "reason": "no mailbox for this workspace"});
    }
    let identity = aplexer::messaging::SessionIdentity {
        id: record.id,
        workspace: Some(record.workspace.clone()),
        tag: Some(record.tag.clone()),
        engine: Some(record.engine.clone()),
        profile: record.profile.clone(),
    };
    let parse = || -> Result<Vec<Value>> {
        let messages = aplexer::messaging::list_messages_in(&mp, &record.workspace)?;
        let cursor = aplexer::messaging::read_cursor_in(&mp, record.id)?;
        let mut slim = Vec::new();
        for message in messages.iter().rev() {
            if !identity.receives(message) || cursor.is_acked(message.id) {
                continue;
            }
            if slim.len() >= MESSAGE_LIMIT {
                break;
            }
            let (first_line, clipped) =
                clip_text(message.body.lines().next().unwrap_or_default(), 300);
            slim.push(json!({
                "id": message.id,
                "kind": message.kind,
                "from": message.from.tag,
                "unread": true,
                "first_line": first_line,
                "first_line_clipped": clipped,
            }));
        }
        Ok(slim)
    };
    match parse() {
        Ok(unread) => json!({"available": true, "unread": unread}),
        Err(error) => json!({"available": false, "error": format!("{error:#}")}),
    }
}

// ---------------------------------------------------------------------
// Completeness / gap report
// ---------------------------------------------------------------------

/// One evidence section with its verdict and the detail that qualifies it.
/// This is what keeps the bundle honest -- a report without it would look
/// complete even when the PTY tail hit ENOSPC or discovery was ambiguous.
#[derive(Debug)]
struct Section {
    value: Value,
    ok: bool,
    detail: String,
}

fn gaps_section(
    status: &StatusData,
    transcript: &Section,
    pty_tail: &Section,
    screen: &Section,
    staleness_detail: Option<&str>,
) -> Vec<Value> {
    let mut gaps = Vec::new();
    let record_error = status.record_persistence_error.as_deref();
    gaps.push(gap(
        "record",
        record_error.is_none(),
        record_error
            .map(str::to_string)
            .unwrap_or_else(|| "persisted record is current".to_string()),
    ));
    gaps.push(gap(
        "worker",
        status.worker_reachable,
        status
            .rpc_error
            .clone()
            .unwrap_or_else(|| "control socket answered".to_string()),
    ));
    let history_error = status.history_persistence_error.as_deref();
    gaps.push(gap(
        "history",
        history_error.is_none(),
        history_error
            .map(str::to_string)
            .unwrap_or_else(|| "history persistence healthy".to_string()),
    ));
    gaps.push(gap("transcript", transcript.ok, transcript.detail.clone()));
    gaps.push(gap("pty_tail", pty_tail.ok, pty_tail.detail.clone()));
    gaps.push(gap("screen", screen.ok, screen.detail.clone()));
    gaps.push(gap(
        "staleness",
        staleness_detail.is_none(),
        staleness_detail
            .map(str::to_string)
            .unwrap_or_else(|| {
                if status.worker_reachable {
                    "live evidence is current".to_string()
                } else {
                    "worker is gone; persisted evidence is the last durable state".to_string()
                }
            }),
    ));
    gaps
}

fn gap(source: &str, ok: bool, detail: String) -> Value {
    json!({"source": source, "ok": ok, "detail": detail})
}

// ---------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn print_human_handoff(
    record: &SessionRecord,
    status: &StatusData,
    transcript: &Section,
    pty_tail: &Section,
    screen: &Section,
    workspace: &Value,
    gaps: &[Value],
) {
    let state = derived_liveness(&record.phase, record.worker_alive(), record.created_at_ms);
    println!("handoff: {} ({})", record.tag, record.selector());
    println!("  id       {}", record.id);
    println!("  engine   {}", engine_profile(record));
    if let Some(agent) = extra_agent_label(record, aplexer::api::record_detected(record).as_ref()) {
        println!("  agent    {agent}");
    }
    if record.worker_alive() && !status.worker_reachable {
        println!("  state    unreachable (worker alive, control socket unanswered)");
    } else {
        println!("  state    {state}");
    }
    println!("  transcript {}", transcript.detail);
    if let Some(text) = transcript.value["last_user_message"].as_str() {
        println!("    last user: {}", text.lines().next().unwrap_or(""));
    } else if transcript.value["last_user_message"].get("text").is_some() {
        println!(
            "    last user: {} (clipped)",
            transcript.value["last_user_message"]["text"].as_str().unwrap_or("")
        );
    }
    if let Some(text) = transcript.value["last_assistant_message"].as_str() {
        println!("    last assistant: {}", text.lines().next().unwrap_or(""));
    } else if transcript.value["last_assistant_message"].get("text").is_some() {
        println!(
            "    last assistant: {} (clipped)",
            transcript.value["last_assistant_message"]["text"].as_str().unwrap_or("")
        );
    }
    println!("  pty tail   {}", pty_tail.detail);
    println!("  screen     {}", screen.detail);
    let git = &workspace["git"];
    if git["available"].as_bool().unwrap_or(false) {
        println!(
            "  workspace  git {} @ {}, {} changed path(s)",
            git["branch"].as_str().unwrap_or("?"),
            git["head"].as_str().unwrap_or("?"),
            git["changed_paths"].as_array().map(Vec::len).unwrap_or(0)
        );
    } else {
        println!("  workspace  not a git worktree (or git unavailable)");
    }
    let messages = &workspace["messages"];
    if messages["available"].as_bool().unwrap_or(false) {
        println!(
            "  messages   {} unread for this session",
            messages["unread"].as_array().map(Vec::len).unwrap_or(0)
        );
    }
    println!("  gaps");
    for gap in gaps {
        println!(
            "    [{}] {}: {}",
            if gap["ok"].as_bool().unwrap_or(false) { "ok " } else { "gap" },
            gap["source"].as_str().unwrap_or("?"),
            gap["detail"].as_str().unwrap_or("")
        );
    }
}
