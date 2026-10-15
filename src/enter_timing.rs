//! The gap between writing submitted text and the Enter that submits it.
//!
//! `a send --enter` and pane mail write the message first and the Enter
//! keystroke (`\r`) as a distinct, later write. Agent TUIs drain pasted
//! bytes through their own key-event queues, and several -- codex most
//! sensitively -- swallow an Enter that lands while a paste burst is still
//! being consumed: the text sits in the composer and the submit never
//! happens. The wall-clock gap before Enter is therefore an engine family
//! property, configurable per engine (`[engines.<id>] enter_delay_ms`),
//! with an optional screen-driven extension (`enter_settle = true`): keep
//! polling the captured screen until the composer line has stopped
//! changing, so an unusually large paste extends the wait instead of
//! racing it.
//!
//! Settle detection alone is not sufficient: a paste suppression window can
//! outlive the moment the composer renders the paste, so `enter_delay_ms`
//! stays the floor even when `enter_settle` is on -- settle only ever
//! extends the wait, never shortens it.

/// The shipped gap for every engine family without a measured or configured
/// entry. This is the value earlier builds hard-coded in
/// `rpc_send_submitted`.
pub const DEFAULT_ENTER_DELAY_MS: u64 = 300;

/// A configured `enter_delay_ms` beyond this is rejected: the delay runs on
/// every submitted send to that engine, so a typo (seconds, minutes) must
/// fail the config load instead of silently slowing every pane message.
pub const MAX_ENTER_DELAY_MS: u64 = 10_000;

/// How often settle detection re-captures the screen.
pub const SETTLE_POLL_MS: u64 = 100;

/// Settle detection needs this many consecutive identical composer
/// observations before it believes the TUI finished echoing the paste.
pub const SETTLE_STABLE_POLLS: u32 = 3;

/// Settle detection gives up this long after the floor delay elapsed and
/// sends anyway: an unreadable or perpetually-redrawing screen must not
/// hold a message forever (best effort, like the plain delay).
pub const SETTLE_MAX_WAIT_MS: u64 = 2_000;

/// The built-in per-engine-family gap, used when the engine has no
/// `enter_delay_ms` in the config file. Measured per family by
/// `tools/enter_delay_matrix.py`; `zcodex` maps to the `codex` family
/// before this lookup (crate::engine_family).
pub fn builtin_enter_delay_ms(family: &str) -> u64 {
    match family {
        // Measured floors from the 2026-10-14 matrix
        // (tools/enter_delay_matrix.py): codex swallowed Enter on multi-line
        // pastes at 0-100ms and was clean at >=200ms, but a real incident
        // under box load lost an Enter at 300ms, so the shipped floor keeps
        // margin. claude tolerated instant Enter (54/54 including 0ms) and
        // keeps a small headroom; unmeasured families stay at the shipped
        // default.
        "codex" => 400,
        "claude" => 150,
        _ => DEFAULT_ENTER_DELAY_MS,
    }
}

/// The family builtin for settle detection: codex TUIs keep draining a
/// paste burst after the composer renders it, so codex-family sends watch
/// the screen past the floor; everywhere else the floor alone ships.
pub fn builtin_enter_settle(family: &str) -> bool {
    family == "codex"
}

/// The resolved wait before Enter for one target engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmissionTiming {
    /// Minimum wall-clock gap between the last pasted byte and Enter.
    pub delay_ms: u64,
    /// Also watch the screen for the composer to stop changing, extending
    /// the wait past `delay_ms` while the paste is still being echoed
    /// (bounded by `SETTLE_MAX_WAIT_MS`).
    pub settle: bool,
}

impl SubmissionTiming {
    /// Resolve one engine's timing from its optional config entry: the
    /// configured delay and settle flag win over the built-in family
    /// defaults (`None` entries take the family builtin).
    pub fn resolve(
        engine: &str,
        configured_delay_ms: Option<u64>,
        configured_settle: Option<bool>,
    ) -> Self {
        let family = crate::engine_family(engine);
        SubmissionTiming {
            delay_ms: configured_delay_ms.unwrap_or_else(|| builtin_enter_delay_ms(family)),
            settle: configured_settle.unwrap_or_else(|| builtin_enter_settle(family)),
        }
    }

    /// The timing for an engine with no usable config entry.
    pub fn builtin(engine: &str) -> Self {
        Self::resolve(engine, None, None)
    }
}

/// One observation of the composer for settle detection: where the cursor
/// sits and what the cursor's row reads. A TUI that finished consuming a
/// paste has placed the cursor and drawn the composer's final content;
/// while it is still draining, either keeps changing. Capture failure
/// degrades to the default snapshot, which reads as instant stability and
/// fails the wait open (the floor delay still applies).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposerSnapshot {
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_line: String,
}

impl ComposerSnapshot {
    /// Read one snapshot out of a parsed screen, using the repo-standard
    /// `Screen::rows(0, cols)` text (screen/tracker.rs `parser_row_texts`).
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        let (_, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let cursor_line = screen
            .rows(0, cols)
            .nth(cursor_row as usize)
            .unwrap_or_default();
        ComposerSnapshot {
            cursor_row,
            cursor_col,
            cursor_line,
        }
    }
}

/// Accumulates composer observations and reports how many consecutive
/// identical ones have been seen. Any change -- cursor movement, redrawn
/// text -- restarts the count, so the caller can simply poll, observe, and
/// compare against `SETTLE_STABLE_POLLS`.
#[derive(Debug, Default)]
pub struct SettleWatcher {
    last: Option<ComposerSnapshot>,
    stable_polls: u32,
}

impl SettleWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observation; returns the consecutive-identical count
    /// including it.
    pub fn observe(&mut self, snapshot: ComposerSnapshot) -> u32 {
        if self.last.as_ref() == Some(&snapshot) {
            self.stable_polls = self.stable_polls.saturating_add(1);
        } else {
            self.stable_polls = 1;
            self.last = Some(snapshot);
        }
        self.stable_polls
    }
}

/// Recognize the submitted payload in the *current composer*, never in the
/// conversation above it. Unknown layouts fail closed: they cannot justify an
/// extra keystroke. Plain screen captures retain the actual terminal geometry.
/// The returned fingerprint ignores wrapping/whitespace, so an Enter swallowed
/// as a newline still counts as the same pending input. The per-engine screen
/// model lives in `crate::delivery_ack`.
pub fn pending_composer(engine: &str, screen: &str, payload: &str) -> Option<String> {
    crate::delivery_ack::pending_payload(engine, screen, payload)
}
