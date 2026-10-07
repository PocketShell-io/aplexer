//! Self-wakeup jobs: an agent that is waiting for something (a message, a
//! build, another agent) registers a job on its own session; the session's
//! worker then types a short wake prompt into the PTY on a schedule, but only
//! while the harness reports the agent as idle/waiting -- never mid-turn.
//!
//! The job lives in `SessionRecord::wake`, so it is durable (same atomic
//! record write as every other session fact), per-session, and shows up in
//! `a list --json`, `a status`, and the attach UI for free. The worker's
//! periodic tick is the only thing that fires it; the agent (or anyone) turns
//! it off with `a wake off`. This is the aplexer-native successor of
//! tmuxctl's `t jobs` pings.

use crate::SessionRecord;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Shortest allowed interval/delay: a wake is a nudge, not a busy loop.
pub const WAKE_MIN_INTERVAL_MS: u64 = 10_000;
/// Safety cap when `--max-lifetime` is not given (visible as `expires_at_ms`).
pub const WAKE_DEFAULT_MAX_LIFETIME_MS: u64 = 6 * 60 * 60 * 1000;
pub const WAKE_DEFAULT_TEXT: &str = "[aplexer wake] Self-wakeup: check for the message or event you are waiting for (`a message inbox`). If you are no longer waiting, run `a wake off`.";
pub const WAKE_MAX_TEXT_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeMode {
    /// Fire once after the delay, then remove itself.
    Once,
    /// Fire every interval until turned off, expired, or (opt-in) a message.
    Every,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeJob {
    pub mode: WakeMode,
    pub interval_ms: u64,
    pub text: String,
    /// Opt-in: a message addressed to this session (or its ack) removes the job.
    #[serde(default)]
    pub until_message: bool,
    pub created_at_ms: u64,
    pub next_due_ms: u64,
    /// Hard stop: the job removes itself at this time even if never turned off.
    pub expires_at_ms: u64,
    #[serde(default)]
    pub fires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_ms: Option<u64>,
}

impl WakeJob {
    /// Validate and build a job. `delay_ms` is the first-fire delay (for
    /// `Every` it defaults to one interval).
    pub fn new(
        mode: WakeMode,
        interval_ms: u64,
        text: Option<String>,
        until_message: bool,
        max_lifetime_ms: Option<u64>,
        now: u64,
    ) -> Result<Self> {
        if interval_ms < WAKE_MIN_INTERVAL_MS {
            bail!(
                "wake interval/delay must be at least {}s",
                WAKE_MIN_INTERVAL_MS / 1000
            );
        }
        let text = text.unwrap_or_else(|| WAKE_DEFAULT_TEXT.to_string());
        validate_text(&text)?;
        let life = max_lifetime_ms.unwrap_or(WAKE_DEFAULT_MAX_LIFETIME_MS);
        if life < interval_ms {
            bail!("--max-lifetime must be at least one interval/delay");
        }
        Ok(Self {
            mode,
            interval_ms,
            text,
            until_message,
            created_at_ms: now,
            next_due_ms: now + interval_ms,
            expires_at_ms: now.saturating_add(life),
            fires: 0,
            last_fired_ms: None,
        })
    }
}

/// The wake text is typed into a live prompt: one printable line only.
pub fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("wake text must not be empty");
    }
    if text.len() > WAKE_MAX_TEXT_BYTES {
        bail!("wake text is limited to {WAKE_MAX_TEXT_BYTES} bytes");
    }
    if text.chars().any(|c| c.is_control()) {
        bail!("wake text must be a single line without control characters");
    }
    Ok(())
}

/// Parse `90s`, `2m`, `1h30m`, or a bare number of seconds.
pub fn parse_duration_ms(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = raw.parse::<u64>() {
        return Ok(secs.saturating_mul(1000));
    }
    let mut total = 0u64;
    let mut digits = String::new();
    for c in raw.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: u64 = digits
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid duration {raw:?} (try 90s, 2m, 1h30m)"))?;
        digits.clear();
        let unit = match c {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => bail!("invalid duration {raw:?} (try 90s, 2m, 1h30m)"),
        };
        total = total.saturating_add(n.saturating_mul(unit));
    }
    if !digits.is_empty() {
        bail!("invalid duration {raw:?}: trailing number needs a unit (s, m, h, d)");
    }
    Ok(total)
}

pub fn format_duration_ms(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=119 => format!("{s}s"),
        120..=7199 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// What the worker tick should do with the record's job at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAction {
    None,
    /// Remove the job (expired).
    Expire,
    /// Type the wake prompt now and advance the schedule.
    Fire,
}

/// True when the harness itself reports the agent resting (same evidence
/// rule as pane message delivery): a fresh reported idle/waiting state.
pub fn session_is_idle(record: &SessionRecord, now: u64) -> bool {
    let (state, source) = crate::watch::derive_agent_state_with_source(record, now);
    record.phase == crate::Phase::Running
        && source == "reported"
        && matches!(state, "idle" | "waiting")
}

pub fn decide(record: &SessionRecord, now: u64) -> WakeAction {
    let Some(job) = &record.wake else {
        return WakeAction::None;
    };
    if now >= job.expires_at_ms {
        return WakeAction::Expire;
    }
    if now >= job.next_due_ms && session_is_idle(record, now) {
        return WakeAction::Fire;
    }
    WakeAction::None
}

/// Apply a fire to the job: `Once` removes itself, `Every` schedules the next
/// slot one interval from now (never a catch-up burst after a busy stretch).
pub fn after_fire(job: &mut Option<WakeJob>, now: u64) {
    let Some(j) = job else { return };
    j.fires += 1;
    j.last_fired_ms = Some(now);
    match j.mode {
        WakeMode::Once => *job = None,
        WakeMode::Every => j.next_due_ms = now + j.interval_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: Option<&str>, now: u64) -> SessionRecord {
        let mut r = SessionRecord::fixture("/tmp/w", "t");
        r.reported_state = state.map(str::to_string);
        r.reported_state_at_ms = Some(now);
        r
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration_ms("90s").unwrap(), 90_000);
        assert_eq!(parse_duration_ms("2m").unwrap(), 120_000);
        assert_eq!(parse_duration_ms("1h30m").unwrap(), 5_400_000);
        assert_eq!(parse_duration_ms("45").unwrap(), 45_000);
        assert!(parse_duration_ms("5").is_ok());
        assert!(parse_duration_ms("m").is_err());
        assert!(parse_duration_ms("1h5").is_err());
    }

    #[test]
    fn job_validation() {
        assert!(WakeJob::new(WakeMode::Every, 1_000, None, false, None, 0).is_err());
        assert!(
            WakeJob::new(WakeMode::Every, 60_000, Some("a\nb".into()), false, None, 0).is_err()
        );
        assert!(WakeJob::new(WakeMode::Every, 60_000, None, false, Some(1_000), 0).is_err());
        let j = WakeJob::new(WakeMode::Every, 60_000, None, false, None, 100).unwrap();
        assert_eq!(j.next_due_ms, 60_100);
        assert_eq!(j.expires_at_ms, 100 + WAKE_DEFAULT_MAX_LIFETIME_MS);
    }

    #[test]
    fn fires_only_when_idle_and_due() {
        let now = 1_000_000;
        let mut r = record(Some("working"), now);
        r.wake =
            Some(WakeJob::new(WakeMode::Every, 60_000, None, false, None, now - 70_000).unwrap());
        assert_eq!(decide(&r, now), WakeAction::None, "never mid-turn");
        r.reported_state = Some("idle".into());
        assert_eq!(decide(&r, now), WakeAction::Fire);
        r.wake.as_mut().unwrap().next_due_ms = now + 1;
        assert_eq!(decide(&r, now), WakeAction::None, "not due yet");
        r.reported_state = None;
        r.reported_state_at_ms = None;
        r.wake.as_mut().unwrap().next_due_ms = 0;
        assert_eq!(
            decide(&r, now),
            WakeAction::None,
            "no harness evidence, no typing"
        );
    }

    #[test]
    fn expiry_wins_and_after_fire_semantics() {
        let now = 5_000_000;
        let mut r = record(Some("idle"), now);
        r.wake =
            Some(WakeJob::new(WakeMode::Every, 60_000, None, false, Some(120_000), 0).unwrap());
        assert_eq!(decide(&r, now), WakeAction::Expire);
        let mut every = Some(WakeJob::new(WakeMode::Every, 60_000, None, false, None, 0).unwrap());
        after_fire(&mut every, now);
        let j = every.unwrap();
        assert_eq!((j.fires, j.next_due_ms), (1, now + 60_000));
        let mut once = Some(WakeJob::new(WakeMode::Once, 60_000, None, false, None, 0).unwrap());
        after_fire(&mut once, now);
        assert!(once.is_none());
    }
}
