//! Unit tests for the submit timing gap (`enter_timing`) and its
//! per-engine configuration surface.

use super::*;

fn load_config_text(text: &str) -> Result<Config> {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    fs::write(&paths.config_file, text).unwrap();
    Config::load(&paths)
}

#[test]
fn builtin_delays_come_from_the_engine_family() {
    // The shipped floor stays the historical constant: a build must never
    // silently regress the gap every engine gets without measurement.
    assert_eq!(
        builtin_enter_delay_ms("mystery"),
        DEFAULT_ENTER_DELAY_MS,
        "unknown families fall back to the shipped default"
    );
    // Variant engines ride the family: zcodex is the codex fork alias.
    assert_eq!(
        builtin_enter_delay_ms(engine_family("zcodex")),
        builtin_enter_delay_ms("codex")
    );
    // Settle ships on for the codex family only.
    assert!(builtin_enter_settle("codex"));
    assert!(builtin_enter_settle(engine_family("zcodex")));
    assert!(!builtin_enter_settle("claude"));
    // Every measured family is capped: a typo in the table must fail here
    // rather than stall every submitted send.
    for family in [
        "codex",
        "claude",
        "gemini",
        "antigravity",
        "grok",
        "opencode",
    ] {
        assert!(
            builtin_enter_delay_ms(family) <= MAX_ENTER_DELAY_MS,
            "family {family} default exceeds the configured maximum"
        );
        assert!(
            builtin_enter_delay_ms(family) > 0,
            "family {family} default must not disable the gap entirely"
        );
    }
}

#[test]
fn resolve_prefers_the_configured_values_over_the_family_builtin() {
    // An explicit config entry overrides both builtin fields.
    let configured = SubmissionTiming::resolve("codex", Some(50), Some(false));
    assert_eq!(configured.delay_ms, 50);
    assert!(
        !configured.settle,
        "explicit false must win over the builtin"
    );

    let settled = SubmissionTiming::resolve("claude", None, Some(true));
    assert_eq!(settled.delay_ms, builtin_enter_delay_ms("claude"));
    assert!(settled.settle, "explicit true must win over the builtin");

    // Unset entries take the family builtin: codex ships settle on.
    let unset = SubmissionTiming::resolve("codex", None, None);
    assert_eq!(unset.delay_ms, builtin_enter_delay_ms("codex"));
    assert!(unset.settle);

    let claude = SubmissionTiming::resolve("claude", None, None);
    assert_eq!(claude.delay_ms, builtin_enter_delay_ms("claude"));
    assert!(!claude.settle);

    // Builtin helper: no config at all.
    let builtin = SubmissionTiming::builtin("claude");
    assert_eq!(
        builtin,
        SubmissionTiming {
            delay_ms: builtin_enter_delay_ms("claude"),
            settle: builtin_enter_settle("claude"),
        }
    );
}

#[test]
fn config_entry_resolves_over_the_family_default() {
    let config = load_config_text(
        "[engines.codex]\ncommand = [\"codex\"]\nenter_delay_ms = 450\nenter_settle = true\n",
    )
    .unwrap();
    let timing = config.submission_timing("codex");
    assert_eq!(timing.delay_ms, 450);
    assert!(timing.settle);
    // Engines without an entry keep the builtin, and unknown engines do
    // not require a config entry at all.
    let untouched = config.submission_timing("claude");
    assert_eq!(untouched.delay_ms, builtin_enter_delay_ms("claude"));
    assert!(!untouched.settle);
    // An explicit false turns settle off even in the codex family.
    let explicit_off =
        load_config_text("[engines.zcodex]\ncommand = [\"zcodex\"]\nenter_settle = false\n")
            .unwrap();
    assert!(!explicit_off.submission_timing("zcodex").settle);
}

#[test]
fn config_rejects_improbable_enter_delays() {
    let error =
        load_config_text("[engines.codex]\ncommand = [\"codex\"]\nenter_delay_ms = 60000\n")
            .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("enter_delay_ms"),
        "rejection must name the offending key: {message}"
    );
    // One under the cap is accepted.
    load_config_text(&format!(
        "[engines.codex]\ncommand = [\"codex\"]\nenter_delay_ms = {}\n",
        MAX_ENTER_DELAY_MS
    ))
    .unwrap();
}

#[test]
fn settle_watcher_counts_identical_observations_and_restarts_on_change() {
    let mut watcher = SettleWatcher::new();
    let a = ComposerSnapshot {
        cursor_row: 3,
        cursor_col: 10,
        cursor_line: "hello world".into(),
    };
    let mut draining = a.clone();
    draining.cursor_col = 12;
    assert_eq!(watcher.observe(a.clone()), 1);
    assert_eq!(watcher.observe(a.clone()), 2);
    assert_eq!(
        watcher.observe(draining),
        1,
        "any change restarts the count"
    );
    assert_eq!(watcher.observe(a.clone()), 1);
    assert_eq!(watcher.observe(a.clone()), 2);
    assert_eq!(watcher.observe(a), 3);
}

#[test]
fn settle_watcher_reports_stability_for_failed_captures() {
    // A capture failure degrades to default snapshots; repeated failures
    // must read as stable so the wait fails open instead of hanging.
    let mut watcher = SettleWatcher::new();
    let snapshot = ComposerSnapshot::default();
    for expected in 1..=SETTLE_STABLE_POLLS {
        assert_eq!(watcher.observe(snapshot.clone()), expected);
    }
}

#[test]
fn composer_snapshot_reads_the_cursor_row_from_a_screen() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    // Place text on row 2; the cursor parks after the last written cell.
    parser.process(b"\x1b[2;1Hcomposer: pasted text");
    let snapshot = ComposerSnapshot::from_screen(parser.screen());
    assert_eq!(snapshot.cursor_row, 1);
    assert!(
        snapshot.cursor_line.contains("composer: pasted text"),
        "snapshot line must be the cursor's row, got {:?}",
        snapshot.cursor_line
    );
    // A later cursor move changes the observed row.
    parser.process(b"\x1b[10;1Hbottom");
    let moved = ComposerSnapshot::from_screen(parser.screen());
    assert_eq!(moved.cursor_row, 9);
    assert_ne!(snapshot, moved, "cursor movement must change the snapshot");
}

#[test]
fn pending_input_recognizes_engine_composers_and_ignores_transcript_echoes() {
    let payload = "Dry run: reply with PROBEDONE";
    let codex = "› [Pasted Content 1234 chars]\n\n  GPT-6-Luna low · Context 100% left";
    assert!(pending_composer("codex", codex, payload).is_some());
    assert!(pending_composer("zcodex", codex, payload).is_some());
    let claude = "────────────────────\n❯ [Pasted text #1 +12 lines]\n────────────────────";
    assert!(pending_composer("claude", claude, payload).is_some());
    let open = format!("  ┃\n  ┃ {payload}\n  ┃\n  ┃ Build auto · Muse Spark\n  ╹▀▀▀▀▀▀▀");
    assert!(pending_composer("opencode", &open, payload).is_some());
    for (engine, screen) in [
        ("codex", format!("› {payload}\n• PROBEDONE\n› Ask Codex to do anything\n\n  GPT-6-Luna low · Context 99% left")),
        ("claude", format!("❯ {payload}\n● PROBEDONE\n────────────────────\n❯ \n────────────────────")),
        ("opencode", format!("  ┃ {payload}\nPROBEDONE\n\n  ┃\n  ┃\n  ┃ Build auto · Muse Spark\n  ╹▀▀▀▀▀▀▀")),
        ("shell", codex.to_owned()),
        ("claude", format!("❯ {payload}\n● Busy")),
    ] {
        assert_eq!(pending_composer(engine, &screen, payload), None, "{engine}: transcript or unknown layout must not trigger Enter");
    }
}

#[test]
fn pending_input_tracks_wrapped_payload_without_accepting_other_input() {
    let screen = "› Dry run: reply\n  with PROBEDONE\n\n  GPT-6-Luna low · Context 100% left";
    let pending = pending_composer("codex", screen, "Dry run: reply with PROBEDONE");
    assert!(pending.is_some());
    assert_eq!(
        pending,
        pending_composer(
            "codex",
            &screen.replace("\n\n", "\n \n\n"),
            "Dry run: reply with PROBEDONE"
        )
    );
    assert_eq!(
        pending_composer("codex", screen, "An unrelated payload"),
        None
    );
    assert_eq!(pending_composer("codex", screen, ""), None);
}

#[test]
fn empty_composer_hints_cannot_match_short_payloads() {
    assert_eq!(
        pending_composer(
            "codex",
            "› Ask Codex to do anything\n\n Context 100% left",
            "anything"
        ),
        None
    );
    assert_eq!(
        pending_composer(
            "claude",
            "────\n❯ Try \"refactor <filepath>\"\n────",
            "refactor"
        ),
        None
    );
    assert_eq!(
        pending_composer(
            "opencode",
            "┃\n┃ Ask anything… \"What is this?\"\n┃\n┃ Build auto · Muse Spark\n╹▀▀▀",
            "What"
        ),
        None
    );
}
