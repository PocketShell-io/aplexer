//! Location + bind sidecar.

use super::*;

// ---------------------------------------------------------------------
// Location + bind sidecar.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TranscriptBind {
    pub(crate) path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) engine_session_id: Option<String>,
    /// The engine id the log parses as (the live binder records the detected
    /// agent's family, so a `shell` session's codex-family log still reads
    /// back as codex after the agent exits). `None` on binds written before
    /// the field existed and on heuristic binds, where the record's own
    /// engine is the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) engine: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocatedTranscript {
    pub path: PathBuf,
    pub engine_session_id: Option<String>,
    /// The engine id the log parses as: the bind sidecar's recorded engine
    /// when it has one, else the session record's own.
    pub engine: String,
}

/// Everything discovery concluded, in the shape a handoff report quotes: not
/// just the answer but the evidence and the near-misses, so a report can be
/// truthful about how a log was found (or why it was not).
#[derive(Debug, Clone, Default)]
pub struct TranscriptResolution {
    /// The located log, when exactly one candidate survived.
    pub path: Option<PathBuf>,
    /// The engine id to parse `path` with: the session's own engine when it
    /// has a transcript family, else the live agent's family name.
    pub engine: Option<String>,
    /// Which rule produced `path`: `bind` (sidecar), `live-fd` (the live
    /// agent's own open file descriptors), `heuristic` (cwd + mtime), or
    /// `none`.
    pub source: String,
    pub engine_session_id: Option<String>,
    /// The bind sidecar's state after this call, whenever one was consulted
    /// or written: what it points at, and whether the write succeeded.
    pub bind: Option<TranscriptBindReport>,
    /// Why no path was resolved. Never set alongside `path`.
    pub error: Option<String>,
    /// The equally-plausible candidates when discovery ended ambiguous, and
    /// the failed ones when a bound/located path disappeared.
    pub candidates: Vec<PathBuf>,
}

/// Readable report of one bind-sidecar encounter (issue #20: bind
/// persistence is best-effort, and a failed write must be visible, not
/// swallowed).
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptBindReport {
    pub path: PathBuf,
    /// The path an existing sidecar pointed at (a stale one names a missing
    /// file -- the re-discovery cue).
    pub points_to: Option<PathBuf>,
    /// Whether this call wrote the sidecar.
    pub wrote: bool,
    /// Why a write did not happen or failed (read-only state dir, ENOSPC).
    pub write_error: Option<String>,
}

/// The live agent a session's identity-backed binding keys off: its process
/// id (from the caller's `/proc` walk, [`crate::agent_kind::
/// detect_agent_process`]) and the family it classifies as. Only the
/// top-level agent's descriptors are read, so a nested agent's rollout can
/// never be bound to the session.
#[derive(Debug, Clone, Copy)]
pub struct LiveAgent {
    pub pid: u32,
    pub kind: crate::agent_kind::AgentKind,
}

/// The engine id a session's transcript parses as: the session's own engine
/// when it has a transcript family (claude/codex/grok or a variant like
/// zcodex), else the live agent's family -- a shell session hosting codex
/// gets `codex`, never `shell`.
fn session_transcript_engine(record: &SessionRecord, agent: Option<LiveAgent>) -> String {
    if validate_transcript_engine(&record.engine).is_ok() {
        return record.engine.clone();
    }
    agent
        .map(|agent| agent.kind.name().to_string())
        .unwrap_or_else(|| record.engine.clone())
}

/// Locate (or reuse the bound path of) the native log for `record`, and
/// report every conclusion. Writes `<state>/sessions/<id>/transcript.json`
/// on a successful first find so `--follow` and later pages do not re-run
/// the cwd/mtime heuristic. The original narrow contract, kept for
/// `a transcript`: bind first, then the newest-candidate heuristic.
pub fn resolve_transcript(record: &SessionRecord, bind_path: &Path) -> Result<LocatedTranscript> {
    let resolution = resolve_transcript_detailed(
        record,
        bind_path,
        Path::new(crate::agent_kind::DEFAULT_PROC_ROOT),
        None,
    );
    let Some(path) = resolution.path else {
        bail!(
            "{}",
            resolution
                .error
                .unwrap_or_else(|| "transcript not found".into())
        )
    };
    Ok(LocatedTranscript {
        path,
        engine_session_id: resolution.engine_session_id,
        engine: resolution.engine.unwrap_or_else(|| record.engine.clone()),
    })
}

/// `resolve_transcript` with the full evidence trail (issue #20 phase 2):
///
/// 1. **Bind**: a sidecar naming a readable file wins -- it is the exact
///    path a previous discovery proved, and survives the agent's exit.
/// 2. **Live-fd**: with a detected live agent, the files *that process*
///    holds open are identity-backed candidates -- a native log the agent
///    itself is appending to cannot belong to a different session in the
///    same cwd the way a newest-mtime pick can. Each open candidate is
///    validated against the session (mtime >= creation, and the engine's
///    own cwd evidence: claude/grok encode it in the path, codex records it
///    in the rollout's `session_meta`). Exactly one candidate binds;
///    several are an actionable ambiguity error with nothing bound.
/// 3. **Heuristic**: the cwd+ mtime search, kept as the fallback -- but
///    with the same ambiguity rule: several equally-plausible candidates is
///    an error naming them and the explicit `--engine/--path` way out, not
///    a silent newest-mtime pick from a shared cwd.
///
/// Bind persistence is best-effort throughout: a failed sidecar write is
/// reported, never fatal (ENOSPC must not hide a readable transcript).
pub fn resolve_transcript_detailed(
    record: &SessionRecord,
    bind_path: &Path,
    proc_root: &Path,
    agent: Option<LiveAgent>,
) -> TranscriptResolution {
    let engine = session_transcript_engine(record, agent);
    // 1. Bind sidecar.
    if let Ok(bytes) = fs::read(bind_path) {
        match serde_json::from_slice::<TranscriptBind>(&bytes) {
            Ok(bind) if bind.path.is_file() => {
                return TranscriptResolution {
                    path: Some(bind.path.clone()),
                    // A bind recorded by the live binder vouches for the
                    // engine too: a shell session's codex log keeps parsing
                    // as codex after the agent exits.
                    engine: Some(bind.engine.clone().unwrap_or(engine)),
                    source: "bind".into(),
                    engine_session_id: bind.engine_session_id,
                    bind: Some(TranscriptBindReport {
                        path: bind_path.to_path_buf(),
                        points_to: Some(bind.path),
                        wrote: false,
                        write_error: None,
                    }),
                    ..Default::default()
                };
            }
            // A stale sidecar (rotated log) falls through to re-discovery;
            // the report shows what it pointed at.
            Ok(bind) => {
                let mut resolution = discover_fresh(record, bind_path, proc_root, agent, &engine);
                if let Some(report) = resolution.bind.as_mut() {
                    report.points_to = Some(bind.path);
                }
                return resolution;
            }
            Err(_) => {}
        }
    }
    discover_fresh(record, bind_path, proc_root, agent, &engine)
}

/// Bind-sidecar-less discovery: live-fd first (identity-backed), then the
/// cwd+mtime heuristic. Both share the ambiguity rule.
fn discover_fresh(
    record: &SessionRecord,
    bind_path: &Path,
    proc_root: &Path,
    agent: Option<LiveAgent>,
    engine: &str,
) -> TranscriptResolution {
    let since = record.created_at_ms.saturating_sub(5_000);
    // 2. The live agent's own open descriptors.
    if let Some(agent) = agent {
        if record.worker_phase_active() {
            let family = engine_family(engine);
            let candidates: Vec<PathBuf> = open_jsonl_fds(proc_root, agent.pid)
                .into_iter()
                .filter(|path| candidate_matches_session(family, path, &record.cwd, since))
                .collect();
            match candidates.len() {
                1 => {
                    let path = candidates.into_iter().next().expect("one candidate");
                    return bind_resolution(record, bind_path, &path, engine, "live-fd");
                }
                count if count > 1 => {
                    return ambiguity(record, engine, candidates);
                }
                _ => {}
            }
        }
    }
    // 3. The cwd+mtime heuristic over the session's family.
    let candidates = transcript_candidates(engine, &record.cwd, record.created_at_ms, &record.env);
    match candidates.len() {
        1 => {
            let path = candidates.into_iter().next().expect("one candidate");
            bind_resolution(record, bind_path, &path, engine, "heuristic")
        }
        count if count > 1 => ambiguity(record, engine, candidates),
        _ => {
            let supported = validate_transcript_engine(engine).is_ok();
            TranscriptResolution {
                error: Some(if supported {
                    format!(
                        "no {engine} transcript found for session {} (cwd {}); the agent may not \
                         have written anything yet, or the log lives outside the default root -- \
                         locate it and pass --engine/--path explicitly",
                        record.id,
                        record.cwd.display()
                    )
                } else {
                    format!(
                        "session {} runs engine \"{engine}\", which has no native transcript \
                         family, and no supported live agent was detected in it; locate the \
                         native log and pass --engine/--path explicitly",
                        record.id
                    )
                }),
                ..Default::default()
            }
        }
    }
}

/// Bind `path` best-effort and report the resolution. A failed sidecar
/// write surfaces in the bind report instead of failing the resolution
/// (issue #20: ENOSPC must not hide a readable transcript).
fn bind_resolution(
    _record: &SessionRecord,
    bind_path: &Path,
    path: &Path,
    engine: &str,
    source: &str,
) -> TranscriptResolution {
    let engine_session_id = peek_continuation(engine, path);
    let mut report = TranscriptBindReport {
        path: bind_path.to_path_buf(),
        points_to: None,
        wrote: false,
        write_error: None,
    };
    let bind = TranscriptBind {
        path: path.to_path_buf(),
        engine_session_id: engine_session_id.clone(),
        engine: Some(engine.to_string()),
    };
    match atomic_write_json(bind_path, &bind) {
        Ok(()) => report.wrote = true,
        Err(error) => report.write_error = Some(format!("{error:#}")),
    }
    TranscriptResolution {
        path: Some(path.to_path_buf()),
        engine: Some(engine.to_string()),
        source: source.into(),
        engine_session_id,
        bind: Some(report),
        ..Default::default()
    }
}

/// Several candidates survived validation: refuse to guess. The error names
/// every candidate and the explicit way out, and nothing is bound -- a wrong
/// automatic bind is worse than no bind (issue #20).
fn ambiguity(
    record: &SessionRecord,
    engine: &str,
    candidates: Vec<PathBuf>,
) -> TranscriptResolution {
    let listed = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    TranscriptResolution {
        error: Some(format!(
            "{} plausible native {engine} logs match session {} (cwd {}); refusing to guess. \
             Name one explicitly: a handoff {} --engine {engine} --path FILE. Candidates \
             (newest first): {listed}",
            candidates.len(),
            record.id,
            record.cwd.display(),
            record.id
        )),
        candidates,
        ..Default::default()
    }
}

/// Regular `*.jsonl` files `pid` currently holds open, read off
/// `/proc/<pid>/fd`. These are identity-backed candidates: a log the agent
/// process is actively appending to belongs to that agent, not merely to
/// whoever wrote last in a shared cwd. Read failures (a descriptor closed
/// mid-scan, an unreadable `/proc` entry) skip quietly -- the walk degrades
/// to fewer candidates, never an error.
pub fn open_jsonl_fds(proc_root: &Path, pid: u32) -> Vec<PathBuf> {
    let fd_dir = proc_root.join(pid.to_string()).join("fd");
    let Ok(entries) = fs::read_dir(&fd_dir) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        // A deleted-but-open log still names its path with a kernel
        // suffix; the stem is the identity that matters.
        let target = target.as_os_str().to_string_lossy();
        let target = target.strip_suffix(" (deleted)").unwrap_or(&target);
        let path = PathBuf::from(target);
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Whether `path` is a native log of the engine family that could belong to
/// this session: newest-enough to be the session's, and -- where the
/// engine's own layout carries cwd evidence -- consistent with the
/// session's cwd. The same tests the per-family locators apply, applied to
/// arbitrary candidate paths picked up from live file descriptors.
fn candidate_matches_session(family: &str, path: &Path, cwd: &Path, since_ms: u64) -> bool {
    if file_mtime_ms(path).is_none_or(|mtime| mtime < since_ms) {
        return false;
    }
    let cwd_str = cwd.display().to_string();
    match family {
        "claude" => path
            .parent()
            .and_then(|dir| dir.file_name())
            .is_some_and(|dir| dir == std::ffi::OsStr::new(&encode_claude_cwd(&cwd_str))),
        "codex" => rollout_cwd(path).is_none_or(|rollout_cwd| rollout_cwd == cwd_str),
        "grok" => {
            path.file_name().is_some_and(|name| name == "updates.jsonl")
                && path
                    .parent()
                    .and_then(|dir| dir.parent())
                    .and_then(|dir| dir.file_name())
                    .is_some_and(|dir| dir == std::ffi::OsStr::new(&encode_grok_cwd(&cwd_str)))
        }
        _ => false,
    }
}

pub fn locate_transcript(
    engine: &str,
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    transcript_candidates(engine, cwd, created_at_ms, env)
        .into_iter()
        .next()
}

/// Every validated candidate for `engine`'s family at `cwd`, newest-mtime
/// first. The single-path locators pick the head; ambiguity-aware callers
/// (`resolve_transcript_detailed`) need the whole list to tell "found" from
/// "several equally plausible".
pub fn transcript_candidates(
    engine: &str,
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    match engine_family(engine) {
        "claude" => claude_transcript_candidates(cwd, created_at_ms, env),
        "codex" => codex_transcript_candidates(cwd, created_at_ms, env),
        "grok" => grok_transcript_candidates(cwd, created_at_ms, env),
        _ => Vec::new(),
    }
}

pub fn peek_continuation(engine: &str, path: &Path) -> Option<String> {
    let format = wire_format_for(engine).ok()?;
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut assembler = JsonAssembler::default();
    for line in reader.lines().map_while(std::io::Result::ok).take(64) {
        let Some(payload) = assembler.feed(&line).payload else {
            continue;
        };
        let (_events, continuation) = translate(format, &payload);
        if continuation.is_some() {
            return continuation;
        }
    }
    None
}

/// Claude Code: `~/.claude/projects/<encoded-cwd>/<session>.jsonl`
/// (or `$CLAUDE_CONFIG_DIR/projects/...` for profiles). Encoding matches
/// PocketShell `AgentDetector.encodeClaudeCwd`: `/` and `.` both become `-`. aplexer has no direct handle on the underlying
/// claude session id, only the aplexer session's own `cwd` and
/// `created_at_ms` -- so this picks the most-recently-modified `*.jsonl`
/// directly under that cwd's project directory whose mtime is not earlier
/// than the aplexer session's creation (with a few seconds of slack for
/// startup ordering). This is a heuristic, not an exact session-id match:
/// if two aplexer claude sessions share the exact same cwd and are both
/// live, the bind sidecar is what keeps later reads on the first-found
/// file. Documented, not silently assumed.
pub fn locate_claude_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    claude_transcript_candidates(cwd, created_at_ms, env)
        .into_iter()
        .next()
}

/// Every claude candidate: direct children of the cwd's project directory
/// with an mtime not earlier than the session's creation, newest first.
pub fn claude_transcript_candidates(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    let config_dir = env
        .get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")));
    let Some(config_dir) = config_dir else {
        return Vec::new();
    };
    let encoded = encode_claude_cwd(&cwd.display().to_string());
    let dir = config_dir.join("projects").join(encoded);
    if !dir.is_dir() {
        return Vec::new();
    }
    let since = created_at_ms.saturating_sub(5_000);
    // Direct children only: claude's project dirs also hold a `subagents/`
    // subdirectory, which is deliberately NOT walked -- those are sub-agent
    // transcripts, not the top-level session.
    let children = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        });
    candidates_newest_first(children, since)
}

/// Codex: `~/.codex/sessions/<YYYY>/<MM>/<DD>/<session>.jsonl`, date-
/// partitioned so the tree is walked rather than computed directly
/// (`agent_log.py::_resolve_codex_path`). Each rollout file's first line is
/// a `session_meta` row carrying its own `cwd`, which lets this disambiguate
/// candidates precisely rather than relying on mtime alone.
pub fn locate_codex_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    codex_transcript_candidates(cwd, created_at_ms, env)
        .into_iter()
        .next()
}

/// Every codex candidate under the sessions root, newest first: recent
/// enough to be the session's, and (when the rollout's `session_meta` is
/// readable) naming the session's cwd.
pub fn codex_transcript_candidates(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    let root = env
        .get("CODEX_HOME")
        .map(|h| PathBuf::from(h).join("sessions"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/sessions")));
    let Some(root) = root else {
        return Vec::new();
    };
    if !root.is_dir() {
        return Vec::new();
    }
    let since = created_at_ms.saturating_sub(5_000);
    let cwd_str = cwd.display().to_string();
    let mut rollouts = Vec::new();
    walk_jsonl(&root, &mut |path| rollouts.push(path.to_path_buf()));
    // Only rollouts recent enough to matter are opened: one whose
    // session_meta names a different cwd is not ours, whatever its mtime,
    // while one without a readable session_meta stays a candidate on
    // mtime alone.
    let ours = rollouts
        .into_iter()
        .filter(|path| file_mtime_ms(path).is_some_and(|mtime| mtime >= since))
        .filter(|path| rollout_cwd(path).is_none_or(|rollout_cwd| rollout_cwd == cwd_str));
    candidates_newest_first(ours, since)
}

/// The `cwd` recorded in a codex rollout's first (`session_meta`) row.
fn rollout_cwd(path: &Path) -> Option<String> {
    let mut first_line = String::new();
    BufReader::new(File::open(path).ok()?)
        .read_line(&mut first_line)
        .ok()?;
    codex_native_cwd(&serde_json::from_str::<Value>(first_line.trim()).ok()?)
}

/// Grok Build: `$GROK_HOME/sessions/<urlencoded-cwd>/<session-id>/updates.jsonl`
/// (default `GROK_HOME` is `~/.grok`). Percent-encoding matches
/// `urllib.parse.quote(cwd, safe="")` in pocketshell's `agent_log.py`.
pub fn locate_grok_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    grok_transcript_candidates(cwd, created_at_ms, env)
        .into_iter()
        .next()
}

/// Every grok candidate: `updates.jsonl` files under the cwd's encoded
/// project directory, newest first.
pub fn grok_transcript_candidates(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    let Some(root) = grok_sessions_root(env) else {
        return Vec::new();
    };
    if !root.is_dir() {
        return Vec::new();
    }
    let since = created_at_ms.saturating_sub(5_000);
    let encoded = encode_grok_cwd(&cwd.display().to_string());
    let project = root.join(&encoded);
    // Stay inside this cwd's encoded directory. Walking every grok session
    // tree would bind an unrelated live session (this agent's own
    // updates.jsonl is the usual false match).
    if project.is_dir() {
        let updates = fs::read_dir(&project)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join("updates.jsonl"))
            .filter(|candidate| candidate.is_file());
        return candidates_newest_first(updates, since);
    }
    Vec::new()
}

fn grok_sessions_root(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = env
        .get("GROK_HOME")
        .cloned()
        .or_else(|| std::env::var("GROK_HOME").ok())
    {
        return Some(PathBuf::from(home).join("sessions"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".grok/sessions"))
}

pub(crate) fn encode_claude_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim();
    if trimmed.is_empty() {
        "-".into()
    } else {
        trimmed.replace(['/', '.'], "-")
    }
}

pub(crate) fn encode_grok_cwd(cwd: &str) -> String {
    // urllib.parse.quote(cwd, safe="") -- RFC 3986 unreserved
    // (ALPHA / DIGIT / "-" / "." / "_" / "~") stay literal; everything
    // else, including `/`, is percent-encoded. `safe=""` only *adds*
    // extra unencoded bytes; it does not encode `-_.~`.
    let trimmed = cwd.trim();
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
    let mut out = String::new();
    for b in trimmed.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

/// Every `*.jsonl` under `dir`, recursively.
fn walk_jsonl(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            visit(&path);
        }
    }
}

/// The candidates whose mtime is `>= since_ms`, newest first (ties broken
/// by path so the order is deterministic). The single-path locators pick
/// the head; ambiguity-aware callers need the whole ordered list.
fn candidates_newest_first(
    candidates: impl IntoIterator<Item = PathBuf>,
    since_ms: u64,
) -> Vec<PathBuf> {
    let mut scored: Vec<(u64, PathBuf)> = candidates
        .into_iter()
        .filter_map(|path| file_mtime_ms(&path).map(|mtime| (mtime, path)))
        .filter(|(mtime, _)| *mtime >= since_ms)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, path)| path).collect()
}
