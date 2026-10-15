use crate::delivery_ack::*;

/// A recorded `a capture --screen --plain` of a real engine TUI
/// (tests/fixtures/screens/<engine>/<name>.txt).
fn fixture(engine: &str, name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/screens/{engine}/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

const SHORT: &str = "Reply with exactly the single word ACKDONE and nothing else.";

fn long_payload() -> String {
    (0..30)
        .map(|i| {
            format!("Line {i}: the quick brown fox jumps over the lazy dog, ignore this filler.")
        })
        .chain(["Reply with exactly ACKLONG.".to_owned()])
        .collect::<Vec<_>>()
        .join("\n")
}

const ENGINES: [&str; 4] = ["claude", "codex", "zcodex", "opencode"];

#[test]
fn booting_screens_are_not_ready() {
    for (engine, name) in [
        ("claude", "boot-blank"),
        ("codex", "boot"),
        ("zcodex", "boot"),
        ("opencode", "boot-blank"),
    ] {
        assert!(!is_ready(engine, &fixture(engine, name)), "{engine}/{name}");
    }
}

#[test]
fn ready_screens_have_an_empty_idle_input_box() {
    for engine in ENGINES {
        let screen = fixture(engine, "ready");
        assert!(is_ready(engine, &screen), "{engine}");
        assert!(!is_busy(engine, &screen), "{engine}");
        let view = parse_screen(engine, &screen).unwrap();
        assert_eq!(
            view.composer,
            Composer::Empty,
            "{engine}: placeholder is not text"
        );
        assert_eq!(echo_count(engine, &screen, SHORT), 0, "{engine}");
    }
}

#[test]
fn pasted_text_is_seen_pending_in_the_input_box() {
    for engine in ENGINES {
        let screen = fixture(engine, "pending-short");
        assert!(
            pending_payload(engine, &screen, SHORT).is_some(),
            "{engine}"
        );
        assert!(!is_busy(engine, &screen), "{engine}");
        assert_eq!(
            pending_payload(engine, &screen, "an unrelated prompt"),
            None,
            "{engine}"
        );
        // Long pastes collapse into the engine's paste marker.
        let screen = fixture(engine, "pending-long");
        assert!(
            pending_payload(engine, &screen, &long_payload()).is_some(),
            "{engine}"
        );
    }
}

/// The Z.ai fork's footer is `glm-5.3-flash max · <cwd>`, without the
/// `Context` or `? for shortcuts` markers stock Codex draws; before the
/// shared screen model it was an unknown layout and Enter was never retried.
#[test]
fn zcodex_footer_is_recognized() {
    let screen = fixture("zcodex", "pending-short");
    assert!(pending_payload("zcodex", &screen, SHORT).is_some());
    assert!(crate::enter_timing::pending_composer("zcodex", &screen, SHORT).is_some());
}

#[test]
fn busy_indicators_are_detected_per_engine() {
    for engine in ENGINES {
        let screen = fixture(engine, "busy");
        assert!(is_busy(engine, &screen), "{engine}");
        assert_eq!(
            parse_screen(engine, &screen).unwrap().composer,
            Composer::Empty,
            "{engine}"
        );
    }
    // Claude's footer can show a paste hint instead of `esc to interrupt`;
    // the spinner verb alone is enough.
    assert!(is_busy("claude", &fixture("claude", "busy-long")));
}

#[test]
fn finished_turns_are_idle_and_show_the_prompt_echo() {
    for engine in ENGINES {
        let screen = fixture(engine, "done");
        assert!(!is_busy(engine, &screen), "{engine}");
        assert!(is_ready(engine, &screen), "{engine}");
        assert_eq!(echo_count(engine, &screen, SHORT), 1, "{engine}");
    }
    // Submitted and echoed, but the turn has not started yet.
    let screen = fixture("zcodex", "echo-only");
    assert!(!is_busy("zcodex", &screen));
    assert_eq!(echo_count("zcodex", &screen, SHORT), 1);
}

/// OpenCode drew its welcome screen but dropped a paste sent right after:
/// the input box is positively empty, the evidence that justifies a resend.
#[test]
fn a_dropped_paste_reads_as_an_empty_input_box() {
    let screen = fixture("opencode", "lost");
    let view = parse_screen("opencode", &screen).unwrap();
    assert_eq!(view.composer, Composer::Empty);
    assert_eq!(pending_payload("opencode", &screen, SHORT), None);
    assert_eq!(echo_count("opencode", &screen, SHORT), 0);
}

#[test]
fn transcript_text_cannot_fake_a_busy_indicator() {
    let quoted = "› explain esc to interrupt and ✶ Stewing…\n• ok\n\n\n\n\n\n\n› Ask Codex to do anything\n\n  GPT-6-Luna low · Context 99% left";
    assert!(!is_busy("codex", quoted));
    let done = "────\n❯ \n────\n  ⏵⏵ bypass permissions on";
    assert!(!is_busy(
        "claude",
        &format!("✻ Brewed for 2s · done 2:55 PM\n{done}")
    ));
    assert!(is_busy("claude", &format!("✶ Stewing… \n{done}")));
}

/// A scripted Codex-like TUI on a virtual clock.
#[derive(Default)]
struct FakeTui {
    now: u64,
    /// Draws nothing (and drops input) until then.
    blank_until: u64,
    boot_until: u64,
    drop_pastes: u32,
    vanish_enters: u32,
    swallow_enters: u32,
    works: bool,
    hook_works: bool,
    busy: bool,
    busy_since: u64,
    composer: String,
    transcript: Vec<String>,
    pastes: u32,
    enters: u32,
    worked_at: Option<u64>,
}

impl FakeTui {
    fn ready() -> Self {
        FakeTui {
            works: true,
            ..Default::default()
        }
    }
}

impl AckTarget for FakeTui {
    fn capture(&mut self) -> Option<String> {
        if self.now < self.blank_until {
            return Some("\n\n\n".into());
        }
        let mut lines = self.transcript.clone();
        if self.busy {
            lines.push(format!(
                "• Working ({}s • esc to interrupt)",
                (self.now - self.busy_since) / 1000
            ));
        }
        let input = if self.composer.is_empty() {
            "Ask Codex to do anything".to_owned()
        } else {
            self.composer.clone()
        };
        lines.push(format!("› {input}"));
        lines.push(String::new());
        if self.now < self.boot_until {
            lines.push("  ? for shortcuts".into());
        } else {
            lines.push("  GPT-6-Luna low · Context 100% left".into());
        }
        Some(lines.join("\n"))
    }
    fn paste(&mut self) -> anyhow::Result<()> {
        self.pastes += 1;
        if self.now < self.blank_until {
        } else if self.drop_pastes > 0 {
            self.drop_pastes -= 1;
        } else {
            self.composer.push_str("hello agent please start");
        }
        self.now += 400;
        Ok(())
    }
    fn enter(&mut self) -> anyhow::Result<()> {
        self.enters += 1;
        if self.now < self.blank_until {
            return Ok(());
        }
        if self.swallow_enters > 0 {
            self.swallow_enters -= 1;
            return Ok(());
        }
        if self.vanish_enters > 0 {
            self.vanish_enters -= 1;
            self.composer.clear();
            return Ok(());
        }
        if !self.composer.is_empty() {
            self.transcript.push(format!("› {}", self.composer));
            self.composer.clear();
            if self.works {
                self.busy = true;
                self.busy_since = self.now;
            }
            if self.hook_works {
                self.worked_at = Some(self.now);
            }
        }
        Ok(())
    }
    fn reported_working_since(&mut self, since_ms: u64) -> bool {
        self.worked_at.is_some_and(|at| at >= since_ms)
    }
    fn sleep_ms(&mut self, ms: u64) {
        self.now += ms;
    }
    fn now_ms(&mut self) -> u64 {
        self.now
    }
}

const PAYLOAD: &str = "hello agent please start";

fn run(tui: &mut FakeTui) -> DeliveryReport {
    deliver(tui, "codex", PAYLOAD, AckPolicy::for_engine("codex")).unwrap()
}

#[test]
fn healthy_delivery_is_working_after_one_paste_and_one_enter() {
    let mut tui = FakeTui::ready();
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert_eq!((r.sends, r.enter_presses), (1, 1));
    assert_eq!(r.working_evidence, Some("busy_indicator"));
    assert_eq!(
        (r.landed, r.submitted, r.echoed),
        (Some(true), Some(true), true)
    );
    assert!(r.elapsed_ms < 2_000, "{r:?}");
}

#[test]
fn a_booting_session_is_waited_for_before_pasting() {
    let mut tui = FakeTui {
        boot_until: 6_000,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert!(r.ready_wait_ms >= 6_000, "{r:?}");
    assert_eq!(r.sends, 1);
}

#[test]
fn a_dropped_paste_is_resent_once() {
    let mut tui = FakeTui {
        drop_pastes: 1,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert_eq!((r.sends, tui.pastes), (2, 2));
    assert_eq!(tui.transcript.len(), 1, "exactly one submitted prompt");
}

#[test]
fn a_paste_that_never_lands_fails_without_pressing_enter() {
    let mut tui = FakeTui {
        drop_pastes: 10,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::Failed, "{r:?}");
    assert_eq!(r.status.exit_code(), 1);
    assert_eq!((tui.pastes, tui.enters), (2, 0));
    assert!(r.detail.contains("never appeared"), "{}", r.detail);
}

#[test]
fn a_swallowed_enter_is_pressed_again() {
    let mut tui = FakeTui {
        swallow_enters: 2,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert_eq!((r.sends, r.enter_presses), (1, 3));
}

#[test]
fn text_stuck_after_every_enter_fails_without_resending() {
    let mut tui = FakeTui {
        swallow_enters: 99,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::Failed, "{r:?}");
    assert_eq!((tui.pastes, tui.enters), (1, 3));
    assert!(r.detail.contains("still pending"), "{}", r.detail);
}

#[test]
fn an_idle_agent_after_submission_is_delivered_idle() {
    let mut tui = FakeTui {
        works: false,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredIdle, "{r:?}");
    assert_eq!(r.status.exit_code(), 3);
    assert!(r.echoed);
    assert_eq!(tui.pastes, 1, "an echoed prompt is never resent");
    assert!(r.elapsed_ms >= 10_000);
}

#[test]
fn a_harness_working_report_counts_as_work() {
    let mut tui = FakeTui {
        works: false,
        hook_works: true,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert_eq!(r.working_evidence, Some("harness_working"));
}

#[test]
fn a_prompt_that_vanishes_on_enter_is_resent_once() {
    let mut tui = FakeTui {
        vanish_enters: 1,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert_eq!((tui.pastes, tui.transcript.len()), (2, 1));

    let mut tui = FakeTui {
        vanish_enters: 9,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::Failed, "{r:?}");
    assert_eq!(tui.pastes, 2);
}

#[test]
fn a_busy_agent_queues_the_prompt() {
    let mut tui = FakeTui {
        busy: true,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert!(r.queued);
    assert_eq!(r.working_evidence, Some("transcript_activity"));
}

#[test]
fn foreign_unsent_text_is_not_appended_to() {
    let mut tui = FakeTui {
        composer: "a human draft".into(),
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::Failed, "{r:?}");
    assert_eq!((tui.pastes, tui.enters), (0, 0));
}

#[test]
fn without_an_ack_window_submission_is_reported_promptly() {
    let mut tui = FakeTui {
        works: false,
        ..FakeTui::ready()
    };
    let policy = AckPolicy {
        ack_timeout_ms: 0,
        ..AckPolicy::for_engine("codex")
    };
    let r = deliver(&mut tui, "codex", PAYLOAD, policy).unwrap();
    assert_eq!(r.status, DeliveryStatus::Submitted, "{r:?}");
    assert!(r.elapsed_ms < 2_000, "{r:?}");

    // Without the working wait a prompt that keeps vanishing still fails.
    let mut tui = FakeTui {
        vanish_enters: 9,
        ..FakeTui::ready()
    };
    let r = deliver(&mut tui, "codex", PAYLOAD, policy).unwrap();
    assert_eq!(r.status, DeliveryStatus::Failed, "{r:?}");
}

#[test]
fn unknown_engines_and_layouts_are_unverified_single_sends() {
    let mut tui = FakeTui::ready();
    let r = deliver(&mut tui, "shell", PAYLOAD, AckPolicy::for_engine("shell")).unwrap();
    assert_eq!(r.status, DeliveryStatus::Unverified);
    assert_eq!((tui.pastes, tui.enters), (1, 1));
}

/// The handoff race with the readiness wait disabled: text and Enter go into
/// a TUI that has not drawn yet and are dropped. The second look after Enter
/// finds an empty input box and no echo, so the prompt is resent.
#[test]
fn a_prompt_dropped_by_an_undrawn_tui_is_resent_after_a_second_look() {
    let mut tui = FakeTui {
        blank_until: 2_000,
        ..FakeTui::ready()
    };
    let policy = AckPolicy {
        ready_timeout_ms: 0,
        ..AckPolicy::for_engine("codex")
    };
    let r = deliver(&mut tui, "codex", PAYLOAD, policy).unwrap();
    assert_eq!(r.status, DeliveryStatus::DeliveredWorking, "{r:?}");
    assert_eq!((tui.pastes, tui.transcript.len()), (2, 1));
}

#[test]
fn a_screen_that_never_becomes_readable_is_unverified() {
    let mut tui = FakeTui {
        blank_until: u64::MAX,
        ..FakeTui::ready()
    };
    let r = run(&mut tui);
    assert_eq!(r.status, DeliveryStatus::Unverified, "{r:?}");
    assert_eq!((tui.pastes, tui.enters), (1, 1));
}
