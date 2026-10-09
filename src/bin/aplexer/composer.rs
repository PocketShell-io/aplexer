//! Observed agent composer state, for submitting input without guessing.
//!
//! Claude Code and Codex park the real terminal cursor at the caret of their
//! input composer, and both draw an empty composer as a lone prompt glyph
//! (`❯` / `›`) with the cursor right after it -- any placeholder hint is
//! painted *after* the cursor. That is the whole observation contract: the
//! composer is the run of rows around the cursor bounded by a blank row or a
//! horizontal rule, and it is empty exactly when nothing but the glyph sits
//! left of the cursor on the composer's first row.
//!
//! `submit_observed` uses it to replace the old wall-clock pause before
//! Enter: refuse to touch a composer that already holds someone's draft,
//! write the text, wait until the composer shows it, press Enter once, and
//! report success only after the draft is observed leaving the composer. A
//! draft still sitting there is an error, never a second Enter.

use super::*;

/// Grid the absolute-addressed screen snapshot is replayed into; larger than
/// any real session so no row or column is clipped (as in `capture_svg`).
const PARSE_ROWS: u16 = 256;
const PARSE_COLS: u16 = 512;
/// Upper bound on each observed transition (text rendered, draft cleared).
/// Polling stops as soon as the condition holds; a heavily loaded agent can
/// stall its event loop for seconds, so the bound is generous.
/// `APLEXER_SUBMIT_TIMEOUT_MS` overrides it.
const OBSERVE_DEADLINE: Duration = Duration::from_secs(15);
const OBSERVE_POLL: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerView {
    pub(crate) bracketed_paste: bool,
    /// The cursor row and its neighbours up to a blank row or a rule.
    pub(crate) region: Vec<String>,
    pub(crate) empty: bool,
}

fn is_rule(row: &str) -> bool {
    let trimmed = row.trim();
    trimmed.chars().count() >= 8 && trimmed.chars().all(|c| matches!(c, '─' | '━' | '═' | '-'))
}

/// Claude Code's `❯` and Codex's `›`, and nothing else: a shell's `$`/`#`/`>`
/// or a menu's punctuation must never read as an agent's empty composer.
const PROMPT_GLYPHS: [char; 2] = ['❯', '›'];

fn is_prompt_glyph(prefix: &str) -> bool {
    let mut chars = prefix.trim().chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if PROMPT_GLYPHS.contains(&c))
}

impl ComposerView {
    pub(crate) fn parse(snapshot: &[u8]) -> Self {
        let mut parser = vt100::Parser::new(PARSE_ROWS, PARSE_COLS, 0);
        parser.process(snapshot);
        let screen = parser.screen();
        let rows: Vec<String> = screen.rows(0, PARSE_COLS).collect();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let cursor_row = usize::from(cursor_row);
        let bounded = |row: &str| row.trim().is_empty() || is_rule(row);
        let mut first = cursor_row;
        while first > 0 && !bounded(&rows[first - 1]) {
            first -= 1;
        }
        let mut last = cursor_row;
        while last + 1 < rows.len() && !bounded(&rows[last + 1]) {
            last += 1;
        }
        let prefix: String = screen
            .rows(0, cursor_col)
            .nth(cursor_row)
            .unwrap_or_default();
        Self {
            bracketed_paste: screen.bracketed_paste(),
            region: rows[first..=last].to_vec(),
            empty: first == cursor_row && is_prompt_glyph(&prefix),
        }
    }
}

/// Engines whose composer follows the contract above. Anything else (shells,
/// TUIs we have not pinned) cannot be observed and keeps a blind write.
pub(crate) fn observable_composer(record: &SessionRecord) -> bool {
    matches!(aplexer::engine_family(&record.engine), "claude" | "codex")
}

pub(crate) fn capture_composer(record: &SessionRecord) -> Result<ComposerView> {
    Ok(ComposerView::parse(&rpc_capture_screen(record, false)?))
}

fn observe(
    record: &SessionRecord,
    mut done: impl FnMut(&ComposerView) -> bool,
) -> Result<Option<ComposerView>> {
    let limit = env::var("APLEXER_SUBMIT_TIMEOUT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(OBSERVE_DEADLINE, Duration::from_millis);
    let deadline = Instant::now() + limit;
    loop {
        let view = capture_composer(record)?;
        if done(&view) {
            return Ok(Some(view));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(OBSERVE_POLL);
    }
}

/// `text` then one Enter into an observable composer. `paste` brackets the
/// text so the Enter can never be read as part of it, however the agent's
/// reads split. Either way Enter is sent only on positive evidence -- the
/// composer showing the text -- because an empty composer after Enter proves
/// nothing if the text was never seen arriving.
pub(crate) fn submit_observed(
    record: &SessionRecord,
    before: &ComposerView,
    text: &[u8],
    paste: bool,
) -> Result<()> {
    let tag = &record.tag;
    if !before.empty {
        bail!(
            "session {tag:?} is not at an empty input prompt (an unsent draft or a dialog is \
             showing); nothing was written. Composer: {:?}",
            before.region
        );
    }
    write_text(record, text, paste)?;
    let Some(shown) = observe(record, |view| view.region != before.region && !view.empty)? else {
        bail!(
            "input injected into session {tag:?} but never appeared in its composer; Enter \
             was not sent, so the text may remain as an unsent draft"
        );
    };
    rpc_send(record, b"\r")?;
    // Gone from the composer: emptied, or replaced by something else (a
    // permission dialog) that no longer holds the draft's first row. An Enter
    // read as a newline keeps that row and moves the cursor below it.
    let draft_head = shown.region.first().cloned().unwrap_or_default();
    let cleared = |view: &ComposerView| view.empty || !view.region.contains(&draft_head);
    match observe(record, cleared)? {
        Some(_) => Ok(()),
        None => {
            let draft = capture_composer(record)
                .map(|view| view.region)
                .unwrap_or_default();
            bail!(
                "input injected and Enter sent, but session {tag:?} still shows it as an unsent \
                 draft (not submitted; no further Enter was sent). Composer: {draft:?}"
            )
        }
    }
}

pub(crate) fn write_text(record: &SessionRecord, text: &[u8], paste: bool) -> Result<()> {
    let paste = paste && !text.is_empty();
    if paste {
        rpc_send(record, b"\x1b[200~")?;
    }
    for chunk in text.chunks(MAX_FRAME_BYTES) {
        rpc_send(record, chunk)?;
    }
    if paste {
        rpc_send(record, b"\x1b[201~")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(screen: &str) -> ComposerView {
        ComposerView::parse(screen.as_bytes())
    }

    const RULE: &str = "────────────────────────────────────────";

    #[test]
    fn claude_empty_composer_with_placeholder_is_empty() {
        let v = view(&format!(
            "\x1b[?2004h\x1b[1;1H● done\x1b[3;1H{RULE}\x1b[4;1H❯ Try \"refactor\"\x1b[5;1H{RULE}\x1b[4;3H"
        ));
        assert!(v.bracketed_paste);
        assert!(v.empty, "{v:?}");
        assert_eq!(v.region.len(), 1);
    }

    #[test]
    fn claude_single_and_multiline_drafts_are_not_empty() {
        let single = view(&format!(
            "\x1b[3;1H{RULE}\x1b[4;1H❯ [Pasted text #2 +61 lines]\x1b[5;1H{RULE}\x1b[4;30H"
        ));
        assert!(!single.empty, "{single:?}");
        let multi = view(&format!(
            "\x1b[3;1H{RULE}\x1b[4;1H❯ line one\x1b[5;1H  line two\x1b[6;1H  \x1b[7;1H{RULE}\x1b[6;3H"
        ));
        assert!(
            !multi.empty,
            "Enter read as newline leaves the cursor on a later row"
        );
    }

    #[test]
    fn codex_composer_is_bounded_by_blank_rows() {
        let empty =
            view("\x1b[1;1HWorked\x1b[3;1H› Ask Codex to do anything\x1b[5;1H  model\x1b[3;3H");
        assert!(empty.empty, "{empty:?}");
        assert_eq!(empty.region, vec!["› Ask Codex to do anything".to_string()]);
        let draft = view("\x1b[3;1H› hello\x1b[3;8H");
        assert!(!draft.empty);
    }

    #[test]
    fn cursor_at_column_zero_or_after_words_is_not_a_prompt() {
        assert!(!view("READY\r\n").empty);
        assert!(!view("user@host:~$ ").empty);
    }

    #[test]
    fn only_claude_and_codex_glyphs_are_prompts() {
        for glyph in ["❯", "›"] {
            assert!(
                view(&format!("\x1b[2;1H{glyph} \x1b[2;3H")).empty,
                "{glyph}"
            );
        }
        for glyph in ["$", "#", ">", "%", ":", "?", "*", "-", "»", "→", "•"] {
            let v = view(&format!("\x1b[2;1H{glyph} \x1b[2;3H"));
            assert!(!v.empty, "{glyph:?} must not read as an agent composer");
        }
    }
}
