//! Delivery acknowledgement for submitted agent input (`a send --enter`).
//!
//! A PTY write proves only that bytes reached the terminal. Agent TUIs lose
//! input in ways the transport cannot see: a paste that lands while the TUI
//! is still booting is dropped, an Enter that lands mid-paste is swallowed
//! and the text sits in the composer, and a submitted prompt can leave the
//! agent idle. This module reads the rendered screen per engine family and
//! drives a bounded state machine:
//!
//! 1. **ready** -- wait (bounded) for the engine's input box to be usable;
//! 2. **landed** -- after the paste, the composer must show the payload; a
//!    composer positively seen *empty* means the text was dropped, and the
//!    payload is resent (bounded);
//! 3. **submitted** -- after Enter, the composer must no longer hold the
//!    payload; a payload still pending gets another Enter (bounded);
//! 4. **working** -- within the ack window the agent must show it took the
//!    turn: a busy indicator, a harness `working` report newer than the send,
//!    or the transcript moving after the prompt's echo.
//!
//! Every retry is gated on positive screen evidence. An unrecognized layout
//! never justifies a keystroke or a resend: it degrades to `unverified`, the
//! pre-acknowledgement behavior (paste, one Enter).
//!
//! A record that does not name a screen-model engine (`shell` wrapping
//! Claude Code, a wrapper script) is still verified: one capture is sniffed
//! for a known composer, and a match upgrades the delivery from
//! `unverified` to the full state machine under that family's model.
//!
//! The detectors are pure functions over a plain screen capture
//! (`a capture --screen --plain`) and are tested against recorded screens in
//! `tests/fixtures/screens/<engine>/`.

use serde::Serialize;

/// What the engine's input box currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composer {
    /// The input box is visible and holds no text (placeholder hints count
    /// as empty).
    Empty,
    /// The input box holds text; whitespace is removed so wrapping and a
    /// swallowed Enter (rendered as a newline) compare equal.
    Holds(String),
}

/// One parsed screen: the composer and the regions around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    pub composer: Composer,
    /// Everything above the input box: transcript, status/spinner line.
    pub above: Vec<String>,
    /// Everything below the input box: model line, hints, footer.
    pub below: Vec<String>,
}

fn strip(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn is_placeholder(engine_family: &str, input: &str) -> bool {
    match engine_family {
        "codex" => input == "AskCodextodoanything" || input.starts_with("Implement{feature}"),
        "claude" => input.starts_with("Try\""),
        "opencode" => input.starts_with("Askanything…"),
        _ => false,
    }
}

/// Codex and its forks (zcodex) put a model line under the composer, after
/// a blank row: `GPT-6-Luna low · Context 100% left` or, on Z.ai builds,
/// `glm-5.3-flash max · ~/repo`. While booting only `? for shortcuts` shows.
fn codex_footer_at(lines: &[&str], i: usize) -> bool {
    let line = lines[i];
    line.contains("Context ")
        || line.contains("context left")
        || line.contains("? for shortcuts")
        || (i > 0
            && lines[i - 1].trim().is_empty()
            && line.starts_with(' ')
            && line.contains(" · "))
}

/// Parse a plain screen into composer and surroundings. `None` means the
/// layout is not one this engine family is known to draw (booting, a modal
/// dialog, an unknown engine): callers must treat that as "unknown", never
/// as "empty".
pub fn parse_screen(engine: &str, screen: &str) -> Option<ScreenView> {
    let lines: Vec<&str> = screen.lines().collect();
    let family = crate::engine_family(engine);
    let (start, end, input) = match family {
        "codex" => {
            let start = lines
                .iter()
                .rposition(|line| line.trim_start().starts_with('›'))?;
            let end = (start + 1..lines.len()).find(|&i| codex_footer_at(&lines, i))?;
            (start, end, lines[start..end].join("\n"))
        }
        "claude" => {
            // Claude borders its live composer; transcript prompts have no
            // surrounding borders.
            let start = lines
                .iter()
                .rposition(|line| line.trim_start().starts_with('❯'))?;
            if start == 0 || !lines[start - 1].trim().starts_with('─') {
                return None;
            }
            let end = (start + 1..lines.len()).find(|&i| lines[i].trim().starts_with('─'))?;
            (start - 1, end + 1, lines[start..end].join("\n"))
        }
        "opencode" => {
            // Both the welcome screen and active chat use the last bordered
            // input box. A submitted turn above it can have the same left bar.
            let bottom = lines
                .iter()
                .rposition(|line| line.trim_start().starts_with("╹▀"))?;
            let mut start = bottom;
            while start > 0 && lines[start - 1].trim_start().starts_with('┃') {
                start -= 1;
            }
            // The final bar row is the agent/model selector, not editable text.
            if bottom < start + 2 {
                return None;
            }
            let input = lines[start..bottom - 1]
                .iter()
                .map(|line| line.trim_start().trim_start_matches('┃').trim())
                .collect::<Vec<_>>()
                .join("\n");
            (start, bottom + 1, input)
        }
        _ => return None,
    };
    let input = strip(input.trim_start().trim_start_matches(['›', '❯']));
    let composer = if input.is_empty() || is_placeholder(family, &input) {
        Composer::Empty
    } else {
        Composer::Holds(input)
    };
    Some(ScreenView {
        composer,
        above: lines[..start].iter().map(|s| s.to_string()).collect(),
        below: lines[end.min(lines.len())..]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    })
}

/// The engine families with a screen model, most distinctive first.
const SCREEN_FAMILIES: [&str; 3] = ["claude", "codex", "opencode"];

/// The screen model that verifies an engine's own deliveries: `None` when
/// its family has none (`shell`, a private wrapper) and sniffing decides.
pub(crate) fn known_family(engine: &str) -> Option<&'static str> {
    let family = crate::engine_family(engine);
    SCREEN_FAMILIES
        .iter()
        .copied()
        .find(|known| *known == family)
}

/// Which known family's TUI a screen draws. The composers are distinctive
/// (a bordered claude prompt, a codex chevron, opencode bars), so a drawn
/// input box matches exactly one family.
pub(crate) fn sniff_family(screen: &str) -> Option<&'static str> {
    SCREEN_FAMILIES
        .iter()
        .copied()
        .find(|family| parse_screen(family, screen).is_some())
}

/// Whether composer text is the submitted payload: an engine paste marker
/// (`[Pasted text #1 +30 lines]`, `[Pasted Content 2237 chars]`,
/// `[Pasted ~31 lines]`) or the payload's first or last 32 visible chars.
pub fn holds_payload(engine: &str, input: &str, payload: &str) -> bool {
    let payload = strip(payload);
    if payload.is_empty() {
        return false;
    }
    let pasted = match crate::engine_family(engine) {
        "codex" => input.starts_with("[PastedContent"),
        "claude" => input.starts_with("[Pastedtext#"),
        "opencode" => input.starts_with("[Pasted~"),
        _ => false,
    };
    pasted || input.contains(&head(&payload)) || input.contains(&tail(&payload))
}

fn head(payload: &str) -> String {
    payload.chars().take(32).collect()
}

fn tail(payload: &str) -> String {
    let chars: Vec<char> = payload.chars().collect();
    chars[chars.len().saturating_sub(32)..].iter().collect()
}

/// The composer's normalized text when it holds `payload`; `None` for an
/// empty composer, other input, or an unknown layout.
pub fn pending_payload(engine: &str, screen: &str, payload: &str) -> Option<String> {
    match parse_screen(engine, screen)?.composer {
        Composer::Holds(input) if holds_payload(engine, &input, payload) => Some(input),
        _ => None,
    }
}

/// The engine has drawn a usable input box. Codex-family TUIs draw the
/// composer before the model is loaded and drop or delay input meanwhile, so
/// they are ready only once the model line shows under it.
pub fn is_ready(engine: &str, screen: &str) -> bool {
    let Some(view) = parse_screen(engine, screen) else {
        return false;
    };
    match crate::engine_family(engine) {
        "codex" => view.below.iter().any(|line| line.contains(" · ")),
        _ => true,
    }
}

/// The agent is running a turn, judged from the engine's busy indicator:
/// Claude's spinner verb (`✶ Stewing…`) or `esc to interrupt`, Codex's
/// `• Working (3s • esc to interrupt)`, OpenCode's `esc interrupt` footer.
/// Only the rows next to the input box are read, so transcript text that
/// quotes an indicator cannot fake one.
pub fn is_busy(engine: &str, screen: &str) -> bool {
    let Some(view) = parse_screen(engine, screen) else {
        return false;
    };
    let status_rows = &view.above[view.above.len().saturating_sub(6)..];
    match crate::engine_family(engine) {
        "claude" => {
            status_rows.iter().any(|line| claude_spinner(line))
                || view
                    .below
                    .iter()
                    .any(|line| line.contains("esc to interrupt"))
        }
        "codex" => status_rows.iter().any(|line| {
            let line = line.trim_start();
            line.contains("esc to interrupt") || line.starts_with("• Working (")
        }),
        "opencode" => view.below.iter().any(|line| line.contains("esc interrupt")),
        _ => false,
    }
}

/// `✶ Stewing…`, `· Hyperspacing… (1m 26s · ↓ 6.2k tokens)`; the finished
/// line (`✻ Brewed for 2s · done 2:55 PM`) has no ellipsis after its verb.
fn claude_spinner(line: &str) -> bool {
    let mut chars = line.trim_start().chars();
    let Some(glyph) = chars.next() else {
        return false;
    };
    if !matches!(glyph, '·' | '✢' | '✳' | '✶' | '✻' | '✽' | '*') || chars.next() != Some(' ')
    {
        return false;
    }
    let rest: String = chars.collect();
    rest.split_whitespace()
        .next()
        .is_some_and(|verb| verb.ends_with('…'))
}

/// How many times the payload's tail shows in the transcript above the input
/// box. A rise after submission is the prompt's echo: the engine accepted the
/// turn. Leading bars and prompt glyphs are ignored with all whitespace.
pub fn echo_count(engine: &str, screen: &str, payload: &str) -> usize {
    let Some(view) = parse_screen(engine, screen) else {
        return 0;
    };
    let payload = strip(payload);
    if payload.is_empty() {
        return 0;
    }
    let transcript: String = view
        .above
        .iter()
        .map(|line| strip(line.trim_start().trim_start_matches(['┃', '›', '❯'])))
        .collect();
    let needle = tail(&payload);
    transcript.matches(needle.as_str()).count()
}

/// The verdict of one acknowledged delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Submitted, and the agent showed it took the turn.
    DeliveredWorking,
    /// Submitted (the input box cleared and/or the prompt echoed), but no
    /// sign of work within the ack window.
    DeliveredIdle,
    /// Submitted; the working check was not requested.
    Submitted,
    /// Bytes were written but the screen could not be read for this engine:
    /// no claim either way.
    Unverified,
    /// The text never arrived or stayed stuck in the input box after every
    /// retry.
    Failed,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::DeliveredWorking => "delivered_working",
            DeliveryStatus::DeliveredIdle => "delivered_idle",
            DeliveryStatus::Submitted => "submitted",
            DeliveryStatus::Unverified => "unverified",
            DeliveryStatus::Failed => "failed",
        }
    }

    /// `a send` exit code: 0 delivered (or not checkable), 3 delivered but
    /// idle, 1 failed.
    pub fn exit_code(self) -> i32 {
        match self {
            DeliveryStatus::DeliveredIdle => 3,
            DeliveryStatus::Failed => 1,
            _ => 0,
        }
    }
}

/// Bounds for one acknowledged delivery. All waits are upper bounds; each
/// phase ends as soon as its evidence shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckPolicy {
    /// Wait this long for the input box to become usable before pasting.
    pub ready_timeout_ms: u64,
    /// When the input box was unreadable at paste time, wait this long
    /// after Enter for it to become readable and take a second look.
    pub late_look_ms: u64,
    /// After a paste, wait this long for the composer to show it.
    pub landing_timeout_ms: u64,
    /// Extra full sends after a paste was positively lost.
    pub max_resends: u32,
    /// Total Enter presses allowed while the payload stays pending.
    pub max_enters: u32,
    /// After each Enter, wait this long for the composer to clear.
    pub submit_timeout_ms: u64,
    /// After submission, wait this long for the agent to start working;
    /// 0 skips the check (status `submitted`).
    pub ack_timeout_ms: u64,
    pub poll_ms: u64,
}

impl AckPolicy {
    /// The per-engine-family defaults. OpenCode repaints slowest under load.
    pub fn for_engine(engine: &str) -> Self {
        let opencode = crate::engine_family(engine) == "opencode";
        AckPolicy {
            ready_timeout_ms: 30_000,
            late_look_ms: 10_000,
            landing_timeout_ms: if opencode { 3_000 } else { 2_000 },
            max_resends: 1,
            max_enters: 3,
            submit_timeout_ms: if opencode { 2_500 } else { 1_500 },
            ack_timeout_ms: 10_000,
            poll_ms: 200,
        }
    }
}

/// Before resending a dropped paste, wait at most this long for the input
/// box to be ready (OpenCode measured ~7s from start to a usable box).
const RESEND_READY_TIMEOUT_MS: u64 = 10_000;

/// What the state machine needs from a session. `paste` writes the payload
/// (framing included) and waits the engine's floor before Enter.
pub trait AckTarget {
    fn capture(&mut self) -> Option<String>;
    fn paste(&mut self) -> anyhow::Result<()>;
    fn enter(&mut self) -> anyhow::Result<()>;
    /// The harness reported `working` at or after `since_ms` (wall clock).
    fn reported_working_since(&mut self, since_ms: u64) -> bool;
    fn sleep_ms(&mut self, ms: u64);
    fn now_ms(&mut self) -> u64;
}

/// Machine-readable outcome, printed by `a send --json` under `delivery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeliveryReport {
    pub status: DeliveryStatus,
    pub engine: String,
    /// The input box was usable before the first paste (`None`: unknown
    /// layout).
    pub ready: Option<bool>,
    pub ready_wait_ms: u64,
    /// The agent was already busy: the prompt is queued behind its turn.
    pub queued: bool,
    /// Full payload sends (1 + resends).
    pub sends: u32,
    pub enter_presses: u32,
    /// The composer showed the payload before Enter.
    pub landed: Option<bool>,
    /// The composer cleared after Enter.
    pub submitted: Option<bool>,
    /// The prompt's echo appeared in the transcript.
    pub echoed: bool,
    /// `busy_indicator`, `harness_working`, or `transcript_activity`.
    pub working_evidence: Option<&'static str>,
    pub elapsed_ms: u64,
    pub detail: String,
}

impl DeliveryReport {
    pub fn summary(&self) -> String {
        format!(
            "{}: {}",
            self.status.as_str().replace('_', " "),
            self.detail
        )
    }
}

/// Run one acknowledged delivery of `payload` to an `engine` session.
pub fn deliver(
    target: &mut dyn AckTarget,
    engine: &str,
    payload: &str,
    policy: AckPolicy,
) -> anyhow::Result<DeliveryReport> {
    let started = target.now_ms();
    let mut report = DeliveryReport {
        status: DeliveryStatus::Unverified,
        engine: engine.to_owned(),
        ready: None,
        ready_wait_ms: 0,
        queued: false,
        sends: 0,
        enter_presses: 0,
        landed: None,
        submitted: None,
        echoed: false,
        working_evidence: None,
        elapsed_ms: 0,
        detail: String::new(),
    };
    let mut verify_engine = engine.to_owned();
    let mut known = known_family(engine).is_some();
    if !known && !engine.is_empty() {
        // A record without its own screen model may still be showing a known
        // agent TUI (`shell` wrapping Claude Code): sniff one capture and
        // verify against that family. The empty engine is the --no-ack
        // sentinel, which opted out of screen reads entirely.
        if let Some(family) = target.capture().as_deref().and_then(|s| sniff_family(s)) {
            verify_engine = family.to_owned();
            known = true;
        }
    }
    if !known || strip(payload).is_empty() {
        target.paste()?;
        target.enter()?;
        report.sends = 1;
        report.enter_presses = 1;
        report.detail = format!("no screen model for engine {engine:?}; bytes written only");
        report.elapsed_ms = target.now_ms() - started;
        return Ok(report);
    }
    // From here the (possibly sniffed) family's screen model verifies.
    let engine: &str = &verify_engine;

    // 1. ready
    let mut screen = wait_until(target, policy.ready_timeout_ms, policy.poll_ms, |s| {
        is_ready(engine, s)
    });
    report.ready_wait_ms = target.now_ms() - started;
    let ready = screen.as_deref().is_some_and(|s| is_ready(engine, s));
    if !ready {
        screen = target.capture();
    }
    report.ready = screen
        .as_deref()
        .and_then(|s| parse_screen(engine, s))
        .map(|_| ready);
    let baseline = screen.unwrap_or_default();
    report.queued = is_busy(engine, &baseline);
    let baseline_view = parse_screen(engine, &baseline);
    if let Some(Composer::Holds(other)) = baseline_view.as_ref().map(|v| &v.composer) {
        if !holds_payload(engine, other, payload) {
            report.status = DeliveryStatus::Failed;
            report.detail =
                "the input box already holds other unsent text; refusing to append to it".into();
            report.elapsed_ms = target.now_ms() - started;
            return Ok(report);
        }
    }
    let baseline_above = baseline_view.map(|v| v.above);
    let baseline_echoes = echo_count(engine, &baseline, payload);

    let since_wall = target.now_ms();
    loop {
        // 2. landed
        target.paste()?;
        report.sends += 1;
        // An unreadable layout cannot show the paste: one look, no wait.
        let landing_ms = if report.ready.is_some() {
            policy.landing_timeout_ms
        } else {
            0
        };
        let landed = wait_until(target, landing_ms, policy.poll_ms, |s| {
            pending_payload(engine, s, payload).is_some()
        });
        let after_paste = landed
            .clone()
            .or_else(|| target.capture())
            .unwrap_or_default();
        let view = parse_screen(engine, &after_paste);
        let fingerprint = pending_payload(engine, &after_paste, payload);
        report.landed = match (&fingerprint, &view) {
            (Some(_), _) => Some(true),
            (None, Some(v)) if v.composer == Composer::Empty => Some(false),
            // Other text or an unknown layout: no evidence either way.
            _ => None,
        };
        if report.landed == Some(false) {
            // The composer is visibly empty: the paste was dropped (typically
            // a TUI still wiring up its input after the first paint).
            if report.sends <= policy.max_resends {
                // Positive evidence of a drop justifies a readiness wait
                // even when the caller skipped the one before the paste.
                wait_until(target, RESEND_READY_TIMEOUT_MS, policy.poll_ms, |s| {
                    is_ready(engine, s)
                });
                continue;
            }
            report.status = DeliveryStatus::Failed;
            report.detail = format!(
                "text never appeared in the input box after {} send(s); nothing was submitted",
                report.sends
            );
            report.elapsed_ms = target.now_ms() - started;
            return Ok(report);
        }

        // 3. submitted
        target.enter()?;
        report.enter_presses += 1;
        let mut pending = fingerprint;
        if report.landed.is_none() {
            // The input box was unreadable when the text went in (still
            // booting, a dialog). Look again once the engine draws it: text
            // stuck in the box gets Enter retries, and an empty box with no
            // echo is a drop the working phase resends.
            let Some(late) = wait_until(target, policy.late_look_ms, policy.poll_ms, |s| {
                parse_screen(engine, s).is_some()
            }) else {
                report.detail =
                    "the input box could not be read; text and one Enter were written".into();
                report.elapsed_ms = target.now_ms() - started;
                return Ok(report);
            };
            pending = pending_payload(engine, &late, payload);
        }
        while let Some(expected) = pending.clone() {
            let cleared = wait_until(target, policy.submit_timeout_ms, policy.poll_ms, |s| {
                pending_payload(engine, s, payload).as_deref() != Some(expected.as_str())
            });
            if cleared.is_some() {
                report.submitted = Some(true);
                pending = None;
            } else if report.enter_presses >= policy.max_enters {
                report.submitted = Some(false);
                break;
            } else {
                target.enter()?;
                report.enter_presses += 1;
            }
        }
        if report.submitted == Some(false) {
            report.status = DeliveryStatus::Failed;
            report.detail = format!(
                "input is still pending in the input box after {} Enter press(es); text was not resent",
                report.enter_presses
            );
            report.elapsed_ms = target.now_ms() - started;
            return Ok(report);
        }

        // 4. working. With no ack window, still wait (bounded) for the
        // prompt's echo so a send that vanished on Enter is caught.
        let arrival_ms = 3_000;
        let deadline = target.now_ms() + policy.ack_timeout_ms.max(arrival_ms);
        let mut echo_screen: Option<Vec<String>> = None;
        let mut last = String::new();
        loop {
            if target.reported_working_since(since_wall) {
                report.working_evidence = Some("harness_working");
            }
            if let Some(s) = target.capture() {
                if !report.queued && is_busy(engine, &s) {
                    report.working_evidence.get_or_insert("busy_indicator");
                }
                if echo_count(engine, &s, payload) > baseline_echoes {
                    report.echoed = true;
                }
                if report.echoed {
                    match (&echo_screen, parse_screen(engine, &s).map(|v| v.above)) {
                        (None, Some(above)) => echo_screen = Some(above),
                        (Some(first), Some(above)) if *first != above => {
                            report.working_evidence.get_or_insert("transcript_activity");
                        }
                        _ => {}
                    }
                }
                last = s;
            }
            let now = target.now_ms();
            let quiet = !report.echoed
                && report.working_evidence.is_none()
                && untouched(engine, &last, &baseline_above);
            if report.working_evidence.is_some()
                || (policy.ack_timeout_ms == 0 && report.echoed)
                || (quiet && now >= since_wall + arrival_ms && report.sends <= policy.max_resends)
                || now >= deadline
            {
                break;
            }
            target.sleep_ms(policy.poll_ms);
        }
        // Nothing arrived at all: the input box is empty and the screen
        // above it is exactly as it was before the send. A moved transcript
        // is never resent into.
        let vanished = !report.echoed
            && report.working_evidence.is_none()
            && untouched(engine, &last, &baseline_above);
        if vanished && report.sends <= policy.max_resends {
            report.submitted = None;
            report.landed = None;
            continue;
        }
        report.status = if report.working_evidence.is_some() {
            DeliveryStatus::DeliveredWorking
        } else if vanished {
            DeliveryStatus::Failed
        } else if policy.ack_timeout_ms == 0 {
            DeliveryStatus::Submitted
        } else {
            DeliveryStatus::DeliveredIdle
        };
        report.detail = match report.status {
            DeliveryStatus::DeliveredWorking if report.queued => format!(
                "submitted while the agent was busy; queued behind its current turn ({})",
                report.working_evidence.unwrap_or_default()
            ),
            DeliveryStatus::DeliveredWorking => format!(
                "submitted; agent started working ({})",
                report.working_evidence.unwrap_or_default()
            ),
            DeliveryStatus::Submitted => "submitted; working check skipped".into(),
            DeliveryStatus::Failed => format!(
                "the input box cleared but the prompt never reached the transcript after {} send(s)",
                report.sends
            ),
            _ => format!(
                "submitted{}, but no sign of work within {}ms",
                if report.echoed { " and echoed" } else { "" },
                policy.ack_timeout_ms
            ),
        };
        report.elapsed_ms = target.now_ms() - started;
        return Ok(report);
    }
}

/// The input box is empty and everything above it matches the pre-send
/// screen (when that screen was readable at all; the caller also requires
/// that no echo showed).
fn untouched(engine: &str, screen: &str, baseline_above: &Option<Vec<String>>) -> bool {
    parse_screen(engine, screen).is_some_and(|view| {
        view.composer == Composer::Empty
            && baseline_above
                .as_ref()
                .is_none_or(|baseline| *baseline == view.above)
    })
}

/// Poll `capture` until `done` holds or `timeout_ms` passes; returns the
/// matching screen. Capture failures count as "not yet".
fn wait_until(
    target: &mut dyn AckTarget,
    timeout_ms: u64,
    poll_ms: u64,
    mut done: impl FnMut(&str) -> bool,
) -> Option<String> {
    let deadline = target.now_ms() + timeout_ms;
    loop {
        if let Some(screen) = target.capture() {
            if done(&screen) {
                return Some(screen);
            }
        }
        if target.now_ms() >= deadline {
            return None;
        }
        target.sleep_ms(poll_ms.max(1));
    }
}
