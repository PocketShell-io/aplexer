use super::*;

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = KI * 1024;
    const GI: u64 = MI * 1024;
    if bytes >= GI {
        format!("{:.1}G", bytes as f64 / GI as f64)
    } else if bytes >= MI {
        format!("{:.0}M", bytes as f64 / MI as f64)
    } else if bytes >= KI {
        format!("{:.0}K", bytes as f64 / KI as f64)
    } else {
        format!("{bytes}B")
    }
}

/// One `Operation::Status` round-trip, shared by the memory and
/// foreground-command indicators below so a refresh costs one worker
/// round-trip, not one per indicator. `None` on any RPC failure (worker
/// briefly unreachable) -- every indicator built from this just degrades to
/// "omitted" in that case.
pub(crate) fn live_status(record: &SessionRecord) -> Option<Value> {
    rpc_simple(record, Operation::Status, None).ok()
}

/// Everything a status-bar render needs that costs more than a `format!`:
/// the worker's Status answer (a socket round-trip, up to
/// `CONTROL_RPC_TIMEOUT` against a stalled worker), the detected agent (a
/// `/proc` walk) and the sibling list (a registry read).
///
/// **Fetched on the status thread only** (`refresh_live_status`), at most
/// once per `LIVE_STATUS_TTL`; every thread that draws the bar renders
/// from the copy in `StatusBarCtx::live`. Rendering used to fetch all
/// three itself, and the bar is rendered from the relay (every `Layout`
/// event, every chunk while a redraw is parked) and from the input thread
/// (a flash, the key overlay, the pager's exit) -- so a slow worker stalled
/// keystrokes and relayed output for seconds at a time, and a `working`
/// spinner cost a round-trip per 150 ms frame.
#[derive(Clone, Default)]
pub(crate) struct LiveStatus {
    /// The session the facts were fetched for. A switch swaps
    /// `StatusBarCtx::record` underneath the cache, and the new session
    /// must not borrow the old one's memory, foreground or siblings.
    pub(crate) session: Option<Uuid>,
    pub(crate) raw: Option<Value>,
    pub(crate) agent: Option<aplexer::agent_kind::DetectedAgent>,
    /// The workspace's sessions as `numbered_session_label`s, unjoined so
    /// the status bar can elide the list from the right (`+N`) instead of
    /// dropping it whole when the terminal is too narrow.
    pub(crate) siblings: Vec<String>,
    /// The git branch (or detached commit) checked out at the session's
    /// `cwd`, `None` outside a repository -- the one `LiveStatus` fact read
    /// from the filesystem rather than from the worker or the registry.
    pub(crate) branch: Option<String>,
    pub(crate) fetched_at: Option<Instant>,
}

/// How old the cached facts may be before the status thread fetches again.
/// One second keeps the memory readout and a fresh `working` push visible
/// within a beat while cutting the animating bar's round-trips from seven
/// a second to one.
pub(crate) const LIVE_STATUS_TTL: Duration = Duration::from_secs(1);

/// Fetch the attached session's live facts into `ctx.live`. Blocking, so
/// only the status thread and a session switch (where the relay is idle
/// by construction) call it.
pub(crate) fn refresh_live_status(ctx: &StatusBarCtx) {
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let fresh = LiveStatus {
        session: Some(record.id),
        raw: live_status(&record),
        agent: aplexer::api::record_detected(&record),
        siblings: workspace_summary(&ctx.paths, &record),
        branch: session_git_branch(&record),
        fetched_at: Some(Instant::now()),
    };
    *ctx.live.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
}

/// The cached facts for `session`, or none when the cache was fetched for
/// another session (or never): every indicator then degrades to "omitted",
/// exactly as a failed round-trip does.
pub(crate) fn cached_live_status(ctx: &StatusBarCtx, session: Uuid) -> LiveStatus {
    let live = ctx.live.lock().unwrap_or_else(PoisonError::into_inner);
    if live.session == Some(session) {
        live.clone()
    } else {
        LiveStatus::default()
    }
}

/// Whether the next draw should fetch first.
pub(crate) fn live_status_is_stale(ctx: &StatusBarCtx) -> bool {
    let session = ctx.record.lock().unwrap_or_else(PoisonError::into_inner).id;
    let live = ctx.live.lock().unwrap_or_else(PoisonError::into_inner);
    live.session != Some(session)
        || live
            .fetched_at
            .is_none_or(|fetched| fetched.elapsed() >= LIVE_STATUS_TTL)
}

/// The attached session's record as the state derivation should see it.
///
/// `ctx.record` is a snapshot from attach/switch time, but state-report
/// pushes land in the worker's in-memory record (and on disk) with no event
/// reaching the attached client -- deriving the state from the snapshot
/// alone would trust a push that is minutes old and miss every push made
/// after attach, which is exactly the "agent started working while I
/// watched" case the spinner exists for. The Status answer already
/// serializes the worker's live record (`public_session_record`), so
/// overlay its reported-state pair and its activity stamp onto the
/// snapshot: the activity stamp is half of the `idle` push's validity rule
/// (`watch::fresh_reported_state` retracts a resting push once newer PTY
/// output appears), so deriving from the attach-time stamp would judge
/// every post-attach rest against pre-attach output -- an agent that went
/// back to work after attach would keep its stale `idle` claim forever
/// from the bar's point of view. A missing field (older worker) or a
/// failed RPC (`raw` None) leaves the snapshot untouched, same degradation
/// as the memory indicator.
pub(crate) fn overlay_reported_state(record: &SessionRecord, raw: Option<&Value>) -> SessionRecord {
    let mut fresh = record.clone();
    let Some(raw) = raw else {
        return fresh;
    };
    if let Some(s) = raw.get("reported_state").and_then(Value::as_str) {
        fresh.reported_state = Some(s.to_string());
    }
    if let Some(ms) = raw.get("reported_state_at_ms").and_then(Value::as_u64) {
        fresh.reported_state_at_ms = Some(ms);
    }
    if let Some(ms) = raw.get("last_activity_ms").and_then(Value::as_u64) {
        fresh.last_activity_ms = Some(ms);
    }
    fresh
}

/// Live memory indicator from the session's cgroup, if it has one -- a
/// small "useful for our application" touch given aplexer's whole reason
/// for existing is resource-isolated agent sessions. Best-effort: absence
/// of cgroup stats in `raw` (no cgroup configured) just omits the
/// indicator rather than disrupting the status bar.
pub(crate) fn memory_indicator(record: &SessionRecord, raw: &Value) -> Option<String> {
    let current = raw.get("cgroup")?.get("memory_current")?.as_u64()?;
    let used = format_bytes(current);
    Some(match record.limits.memory_bytes {
        Some(max) => format!("{used}/{}", format_bytes(max)),
        None => used,
    })
}

/// Plain interactive shells: showing e.g. `[shell -> bash]` for an ordinary
/// shell session would be redundant noise (that's what `shell` already
/// means), not information. Only an actually interesting foreground
/// program -- something manually run inside the session that isn't just
/// its own shell -- is worth surfacing.
pub(crate) const PLAIN_SHELLS: &[&str] =
    &["sh", "bash", "zsh", "dash", "fish", "ksh", "tcsh", "csh"];

/// The live foreground-command override for the status bar, if there's
/// anything worth showing beyond `record.engine` alone (see
/// `foreground_command` in lib.rs and `Operation::Status`'s worker-side
/// handler for where `raw["foreground_command"]` comes from -- a live,
/// never-persisted read of the pty's current foreground process, the same
/// mechanism tmux uses for `pane_current_command`). `None` when: the
/// worker didn't report one (RPC failure, no foreground process group
/// yet); it's a bare interactive shell (`PLAIN_SHELLS`); or it's just the
/// engine's own launch command running as expected (e.g. a `codex`-engine
/// session actually running `codex` shouldn't redundantly show
/// `[codex -> codex]`).
pub(crate) fn foreground_override(record: &SessionRecord, raw: &Value) -> Option<String> {
    let fg = raw.get("foreground_command")?.as_str()?;
    if PLAIN_SHELLS.contains(&fg) {
        return None;
    }
    let launched = record
        .command
        .first()
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str());
    if launched == Some(fg) {
        return None;
    }
    Some(fg.to_string())
}

/// The detected agent's display name when it adds information beyond the
/// declared engine, `None` when it doesn't. One display rule for every
/// human surface (list rows, `a status`, the attach status bar): a session
/// declared `engine: "claude"` that is running claude says "claude" once;
/// a `shell` session running claude, or a `claude` session someone started
/// codex inside, gets the detected name appended. The engine compares by
/// family (`engine_family`): a `zcodex`-engine session running
/// zcodex says codex once, because zcodex is a codex variant, not a second
/// agent. A detected *variation* rides the same `engine/profile` spelling
/// the declared side uses (`codex/zcodex`); the default profile adds
/// nothing, so plain `codex` stays plain.
pub(crate) fn extra_agent_label(
    record: &SessionRecord,
    detected: Option<&aplexer::agent_kind::DetectedAgent>,
) -> Option<String> {
    let agent = detected?;
    (agent.kind.name() != aplexer::engine_family(&record.engine)).then(|| match &agent.profile {
        Some(profile) => format!("{}/{}", agent.kind.name(), profile),
        None => agent.kind.name().to_string(),
    })
}

/// The list/status engine cell. A plain `shell` workload that detection
/// found an agent inside is labeled by the agent alone: `shell` is the
/// absence of a choice, so `shell -> codex` spent the column on noise when
/// `codex` is the fact. A declared engine keeps the `engine -> agent` form,
/// where the base carries real information (a claude session someone
/// started codex inside).
pub(crate) fn engine_label(
    record: &SessionRecord,
    detected: Option<&aplexer::agent_kind::DetectedAgent>,
) -> String {
    let agent = extra_agent_label(record, detected);
    if record.engine == "shell" {
        if let Some(agent) = agent {
            return agent;
        }
    }
    let base = engine_profile(record);
    match agent {
        Some(agent) => format!("{base} -> {agent}"),
        None => base,
    }
}

/// `engine/profile`, or the bare engine for a session started without a
/// profile -- the one spelling every human surface uses.
pub(crate) fn engine_profile(record: &SessionRecord) -> String {
    match &record.profile {
        Some(profile) => format!("{}/{}", record.engine, profile),
        None => record.engine.clone(),
    }
}

/// `{i}:{tag}[*][({state})]` for one session at position `i` (0-based) of
/// its workspace's `a list` order -- the label the status bar's sibling
/// segment prints, and the exact format the `Ctrl-b s` session picker
/// renders its rows from. Sharing this one formatter is what guarantees a
/// picker row and the status bar's same-numbered entry can never disagree
/// about the number, the `*`, or the state: both walk the same
/// `list_records` order (`Reverse(created_at_ms)`) that
/// `group_by_workspace` preserves within a group -- see the equivalence
/// note on `resolve_quick_index` -- and both print through here. `*` marks
/// the currently attached session; `(state)` is appended only when the
/// state is not "running" (the common case needs no label).
pub(crate) fn numbered_session_label(r: &SessionRecord, index: usize, current: Uuid) -> String {
    let (state, _) = session_ui_state(r, now_ms());
    let mut label = format!("{}:{}", index + 1, r.tag);
    if r.id == current {
        label.push('*');
    }
    // Running-ish states are the expected background (`running` while
    // working, `idle` while resting); anything else (a reported wait, a
    // death, a broken worker) is worth seeing while attached.
    if !matches!(state, "running" | "idle") {
        label.push_str(&format!("({state})"));
    }
    label
}

/// `{i}:{tag}[*][({state})]` for every session in the current workspace,
/// mirroring how `a list`'s tree groups sessions by workspace (see
/// `group_by_workspace`) -- a live glance at what else is running here
/// without detaching, and (unlike the old `sibling_summary` it replaces)
/// self-documenting: `i` is exactly the number `Ctrl-b 1`..`9` jumps to
/// (`pick_switch_target`'s `Index` arm), because both walk the same
/// `list_records` order (`Reverse(created_at_ms)`) that `group_by_workspace`
/// preserves within a group -- see the equivalence note on
/// `resolve_quick_index`. Rows are `numbered_session_label`, which is also
/// what the `Ctrl-b s` session picker prints, so the two can never drift.
/// Lists **all** sessions including the current one (the old version listed
/// only "the others") because the numbering only makes sense as a complete
/// index. Unjoined: the status bar joins with spaces and elides from the
/// right when the row is tight (`sibling_elisions`), so a crowded
/// workspace narrows to `1:main* 2:review … +3` instead of crowding every
/// other segment off the bar. Empty (single-session workspace): no
/// segment, same as before.
pub(crate) fn workspace_summary(paths: &Paths, record: &SessionRecord) -> Vec<String> {
    let records = match list_records(paths) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let siblings: Vec<SessionRecord> = records
        .into_iter()
        .filter(|r| r.workspace == record.workspace)
        .collect();
    if siblings.len() <= 1 {
        return Vec::new();
    }
    siblings
        .iter()
        .enumerate()
        .map(|(i, r)| numbered_session_label(r, i, record.id))
        .collect()
}

/// Every elision of the workspace's session labels, widest first: the
/// complete list, then the list with its last entry replaced by `+1`, and
/// so on down to the bare `+N` count. The status bar tries these in order
/// at each layout level -- before stepping down to a narrower layout --
/// because the `+N` tail keeps the one fact that matters about the hidden
/// entries (how many there are) on the bar for two-ish cells, where the
/// old all-or-nothing segment hid both the entries and the fact that they
/// existed at all. Each elision is strictly narrower than the previous
/// one (a label is at least `i:t`, three cells, and the `+N` tail costs
/// at most three), so first-fit picks the widest that fits.
pub(crate) fn sibling_elisions(labels: &[String]) -> Vec<String> {
    (0..=labels.len())
        .rev()
        .map(|keep| {
            let hidden = labels.len() - keep;
            let mut text = labels[..keep].join(" ");
            if hidden > 0 {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&format!("+{hidden}"));
            }
            text
        })
        .collect()
}

/// The git branch checked out at the session's `cwd`, for the status
/// bar's identity segment. Read straight from the repository's `HEAD`
/// file -- no `git` subprocess, no library: the fetch runs on the status
/// thread once per `LIVE_STATUS_TTL`, sharing the host with a PTY relay
/// that must stay responsive, and `HEAD` is the entire truth of "what is
/// checked out" for display purposes. `None` when `cwd` is not inside a
/// work tree, or its `.git` exists but cannot be read: an absent segment
/// beats a wrong one.
pub(crate) fn session_git_branch(record: &SessionRecord) -> Option<String> {
    git_head_branch(&record.cwd)
}

/// Walks up from `start` the way git itself does, stopping at the first
/// `.git` entry: a broken or exotic `.git` *is* the repository boundary,
/// and walking past it would report an outer repository that a nested
/// checkout shadows.
pub(crate) fn git_head_branch(start: &Path) -> Option<String> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            return repository_head_branch(&dot_git);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `HEAD`'s branch for the two shapes a `.git` entry takes: a directory
/// (an ordinary work tree) and a `gitdir:` pointer file (a linked work
/// tree or submodule). The pointer's target is resolved against the
/// `.git` file's directory when relative -- the same rule git applies.
fn repository_head_branch(dot_git: &Path) -> Option<String> {
    let head = if dot_git.is_dir() {
        dot_git.join("HEAD")
    } else {
        let pointer = fs::read_to_string(dot_git).ok()?;
        let target = Path::new(pointer.trim().strip_prefix("gitdir:")?.trim());
        let gitdir = if target.is_absolute() {
            target.to_path_buf()
        } else {
            dot_git.parent()?.join(target)
        };
        gitdir.join("HEAD")
    };
    parse_head(&fs::read_to_string(head).ok()?)
}

/// `ref: refs/heads/<branch>` names the branch (an unborn branch still
/// names the one that will be created, which is the fact a human needs);
/// a bare object name is a detached HEAD, abbreviated the way `git log
/// --oneline` abbreviates commits.
fn parse_head(head: &str) -> Option<String> {
    let line = head.trim();
    if let Some(reference) = line.strip_prefix("ref:") {
        return Some(
            reference
                .trim()
                .trim_start_matches("refs/heads/")
                .to_string(),
        );
    }
    let object = line.len() >= 7 && line.chars().all(|c| c.is_ascii_hexdigit());
    object.then(|| line.chars().take(7).collect())
}

/// Makes plain status-bar data safe to interpolate into terminal output.
/// Session records and transient errors can contain arbitrary persisted or
/// remote text; C0/C1 controls (including ESC, BEL, CR, and LF) must never be
/// allowed to become terminal instructions when the bar is drawn.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

pub(crate) fn terminal_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// The widest candidate that fits `cols`, sanitized for the terminal and
/// padded to the full row -- or `fallback`, truncated, when none does.
/// Candidates are rendered one at a time, so a wide terminal pays for the
/// first and a narrow one never formats the layouts it cannot show.
pub(crate) fn fit_bar_text(
    cols: usize,
    candidates: impl IntoIterator<Item = String>,
    fallback: &str,
) -> String {
    let fits = candidates
        .into_iter()
        .map(|candidate| sanitize_terminal_text(&candidate))
        .find(|candidate| terminal_display_width(candidate) <= cols);
    pad_or_truncate(
        &fits.unwrap_or_else(|| sanitize_terminal_text(fallback)),
        cols,
    )
}

/// Pads or truncates to exactly `cols` terminal display cells without
/// splitting an extended grapheme cluster. This keeps wide glyphs, combining
/// sequences, and emoji aligned while the reverse-video bar spans the full
/// terminal width like tmux's own.
pub(crate) fn pad_or_truncate(text: &str, cols: usize) -> String {
    let cols = cols.max(1);
    let mut rendered = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = terminal_display_width(grapheme);
        if grapheme_width > cols.saturating_sub(width) {
            break;
        }
        rendered.push_str(grapheme);
        width += grapheme_width;
    }
    rendered.push_str(&" ".repeat(cols - width));
    rendered
}
