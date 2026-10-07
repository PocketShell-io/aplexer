//! SVG rendering of a session's live screen for `a capture --screen --svg`.
//!
//! The renderer is entirely client-side: the worker's existing
//! `CaptureScreen` snapshot (vt100's state dump, absolute-addressed per
//! row) is replayed into an oversized local parser, so no protocol or
//! worker change is needed and the flag works against workers already
//! running an older binary. The oversized parse grid only bounds memory;
//! the drawing is trimmed to the content's bounding box (plus the cursor
//! cell), because the client never learns the session's true geometry --
//! a full-screen TUI fills its grid, so the trim recovers it, and sparse
//! screens are padded to [`MIN_COLS`] so a bare-shell capture still reads
//! like a terminal rather than a sliver.
//!
//! The bottom row reproduces the attach status bar (`status_bar_text`'s
//! layout: workspace:tag, branch, state, engine, siblings, key hint) from
//! a one-shot fetch of the same facts the status thread caches, so a
//! capture looks like the attached view it is a snapshot of. The bar is
//! drawn with fixed colors (dark strip, light text) instead of the
//! attach's reverse-video-on-defaults, because the SVG names colors
//! directly rather than borrowing a terminal theme. Keep this composition
//! in step with `status_bar_text` when the bar's layout changes.

use super::*;

/// Local parse-grid bound. Generous against every grid the worker's own
/// `MAX_SCREEN_CELLS` cap permits in practice; content beyond it is
/// clipped rather than grown into, since the bound is what keeps a hostile
/// snapshot from allocating without limit client-side.
const PARSE_ROWS: u16 = 256;
const PARSE_COLS: u16 = 512;

/// Captures of sparse screens never render narrower than this many cell
/// columns, so a capture of a fresh shell prompt still looks like a
/// terminal. tmux's historical default width.
const MIN_COLS: u16 = 80;

const CELL_W: f64 = 9.0;
const CELL_H: f64 = 24.0;
const FONT_SIZE: f64 = 15.0;
const PAD_X: f64 = 14.0;
const PAD_Y: f64 = 12.0;
/// Distance from a row's top to its text baseline, inside [`CELL_H`].
const BASELINE: f64 = 17.0;

const DEFAULT_BG_RGB: (u8, u8, u8) = (0x0c, 0x0c, 0x0c);
const DEFAULT_FG_RGB: (u8, u8, u8) = (0xd4, 0xd4, 0xd4);
const BAR_BG: &str = "#1b1b1b";
const BAR_FG: &str = "#c8cdd4";
const FONT_FAMILY: &str = "DejaVu Sans Mono,Menlo,Consolas,monospace";

/// The 16 ANSI colors, in a dark-theme palette close to what the agent
/// TUIs this feature exists to capture assume. Indexed colors 16-255 go
/// through the standard xterm cube in [`indexed`]; truecolor arrives
/// pre-resolved.
fn palette(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (0x28, 0x2c, 0x34),
        1 => (0xe0, 0x6c, 0x75),
        2 => (0x98, 0xc3, 0x79),
        3 => (0xe5, 0xc0, 0x7b),
        4 => (0x61, 0xaf, 0xef),
        5 => (0xc6, 0x78, 0xdd),
        6 => (0x56, 0xb6, 0xc2),
        7 => (0xab, 0xb2, 0xbf),
        8 => (0x5c, 0x63, 0x70),
        9 => (0xff, 0x7b, 0x86),
        10 => (0xa9, 0xd0, 0x85),
        11 => (0xf0, 0xcf, 0x8d),
        12 => (0x7c, 0xbf, 0xf2),
        13 => (0xd5, 0x8a, 0xe8),
        14 => (0x6c, 0xc4, 0xd1),
        _ => (0xff, 0xff, 0xff),
    }
}

/// The standard xterm 256-color extension: a 6x6x6 cube for 16-231, then
/// 24 grayscale steps. Keeping this exact means a TUI's theme colors
/// survive the capture unchanged.
fn indexed(idx: u8) -> (u8, u8, u8) {
    if idx < 16 {
        return palette(idx);
    }
    if idx < 232 {
        let n = u16::from(idx - 16);
        let levels = [0u8, 95, 135, 175, 215, 255];
        let (r, g, b) = (n / 36, (n / 6) % 6, n % 6);
        return (levels[r as usize], levels[g as usize], levels[b as usize]);
    }
    let gray = 8 + u16::from(idx - 232) * 10;
    (gray as u8, gray as u8, gray as u8)
}

fn color_rgb(color: vt100::Color) -> Option<(u8, u8, u8)> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(idx) => Some(indexed(idx)),
        vt100::Color::Rgb(r, g, b) => Some((r, g, b)),
    }
}

fn css(rgb: Option<(u8, u8, u8)>, default: (u8, u8, u8)) -> String {
    let (r, g, b) = rgb.unwrap_or(default);
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The formatting a run of cells must agree on to share one `<text>`
/// element. Colors stay unresolved (`vt100::Color`) so `PartialEq` compares
/// the terminal's own vocabulary; only rendering resolves them.
#[derive(Clone, PartialEq)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

/// A cell contributes to the picture when it has a glyph, paints its own
/// background, or flips the default one -- cleared cells and plain spaces
/// are just the canvas. Formatting-only cells (bold/underline with no
/// contents and no colors) render nothing, so they don't count.
fn cell_used(cell: &vt100::Cell) -> bool {
    cell.has_contents() || cell.bgcolor() != vt100::Color::Default || cell.inverse()
}

/// Content extents (counts, not max indices), wide characters included:
/// the client cannot ask the worker for the session's geometry, so the
/// trim *is* the geometry recovery. The cursor cell joins the bounds when
/// visible, so a capture of an empty prompt still shows where typing goes.
fn used_extents(screen: &vt100::Screen) -> (u16, u16) {
    let (parse_rows, parse_cols) = screen.size();
    let mut rows = 0u16;
    let mut cols = 0u16;
    for r in 0..parse_rows {
        for c in 0..parse_cols {
            if let Some(cell) = screen.cell(r, c) {
                if cell_used(cell) {
                    if r >= rows {
                        rows = r + 1;
                    }
                    let span = if cell.is_wide() { 2 } else { 1 };
                    if c + span > cols {
                        cols = c + span;
                    }
                }
            }
        }
    }
    if !screen.hide_cursor() {
        let (cursor_row, cursor_col) = screen.cursor_position();
        if cursor_row < parse_rows && cursor_col < parse_cols {
            rows = rows.max(cursor_row + 1);
            cols = cols.max(cursor_col + 1);
        }
    }
    (rows.max(1), cols.max(1))
}

/// The status bar line exactly as the attach client would draw it right
/// now: same layout as `status_bar_text`'s body, from a one-shot fetch of
/// the same facts its status thread caches. Every fact degrades to
/// "omitted" on failure, exactly as a failed round-trip does there.
fn capture_status_bar_text(paths: &Paths, record: &SessionRecord, cols: usize) -> String {
    let raw = live_status(record);
    let agent = aplexer::api::record_detected(record);
    let siblings = workspace_summary(paths, record);
    let home = env::var_os("HOME").map(PathBuf::from);
    let ws = display_workspace(&record.workspace, home.as_deref());
    let engine_segment = format!("  {}", engine_label(record, agent.as_ref()));
    let mem = raw
        .as_ref()
        .and_then(|raw| memory_indicator(record, raw))
        .map(|mem| format!("  mem {mem}"))
        .unwrap_or_default();
    let branch_segment = git_branch_segment(session_git_branch(record).as_deref());
    let state_record = overlay_reported_state(record, raw.as_ref());
    let now = now_ms();
    let (state_word, state_source) = session_ui_state(&state_record, now);
    let (glyph, _) = state_glyph(state_word);
    let glyph = spinner_frame(state_word, state_source, now)
        .map(|frame| frame.to_string())
        .unwrap_or_else(|| glyph.to_string());
    let state = format!("{glyph} {}", state_word.to_uppercase());
    let tag = &record.tag;
    let elisions = sibling_elisions(&siblings);
    let full = |sibs: &str| {
        let sib = if sibs.is_empty() {
            String::new()
        } else {
            format!("  |  {sibs}")
        };
        format!("{ws}:{tag}{branch_segment}  {state}{engine_segment}{mem}{sib}  |  ^b ?")
    };
    let medium = |sibs: &str| {
        let sib = if sibs.is_empty() {
            String::new()
        } else {
            format!("  |  {sibs}")
        };
        format!("{tag}{branch_segment}  {state}{engine_segment}{sib}  |  ^b ?")
    };
    let compact = || format!("{tag}{branch_segment}  {state}{engine_segment}  ^b ?");
    let candidates = elisions
        .iter()
        .map(|sibs| full(sibs))
        .chain(elisions.iter().map(|sibs| medium(sibs)))
        .chain(std::iter::once(compact()));
    fit_bar_text(cols, candidates, &format!("{state}  ^b ?"))
}

fn render_screen(screen: &vt100::Screen, rows: u16, cols: u16) -> String {
    let mut body = String::new();
    for r in 0..rows {
        let mut c = 0u16;
        while c < cols {
            let Some(cell) = screen.cell(r, c) else {
                c += 1;
                continue;
            };
            if !cell_used(cell) {
                c += 1;
                continue;
            }
            let style = CellStyle::of(cell);
            let start = c;
            let mut text = String::new();
            while c < cols {
                match screen.cell(r, c) {
                    Some(next) if cell_used(next) && CellStyle::of(next) == style => {
                        text.push_str(next.contents());
                        // A wide glyph owns two cells but one `<text>`
                        // advance: skip its continuation cell so the run's
                        // `textLength` still covers exactly the cells it
                        // spans.
                        c += if next.is_wide() { 2 } else { 1 };
                    }
                    _ => break,
                }
            }
            let span = f64::from(c - start);
            let x = PAD_X + f64::from(start) * CELL_W;
            let y = PAD_Y + f64::from(r) * CELL_H;
            // Inverse video swaps the pen and the canvas; when either side
            // is the terminal default, the swap resolves against the theme
            // defaults rather than disappearing -- reverse-video-on-
            // defaults is how the status bar paints, so it must stay a
            // visible strip.
            let (fg_rgb, bg_rgb) = if style.inverse {
                (
                    Some(color_rgb(style.bg).unwrap_or(DEFAULT_BG_RGB)),
                    Some(color_rgb(style.fg).unwrap_or(DEFAULT_FG_RGB)),
                )
            } else {
                (color_rgb(style.fg), color_rgb(style.bg))
            };
            let fg = css(fg_rgb, DEFAULT_FG_RGB);
            if let Some(bg) = bg_rgb {
                body.push_str(&format!(
                    "<rect x=\"{x}\" y=\"{y}\" width=\"{}\" height=\"{CELL_H}\" fill=\"{}\"/>\n",
                    span * CELL_W,
                    css(Some(bg), DEFAULT_BG_RGB)
                ));
            }
            let mut attrs = format!("fill=\"{fg}\"");
            if style.bold {
                attrs.push_str(" font-weight=\"bold\"");
            }
            if style.italic {
                attrs.push_str(" font-style=\"italic\"");
            }
            if style.underline {
                attrs.push_str(" text-decoration=\"underline\"");
            }
            body.push_str(&format!(
                "<text x=\"{x}\" y=\"{y}\" {attrs} textLength=\"{}\">{}</text>\n",
                span * CELL_W,
                escape(&text)
            ));
        }
    }
    body
}

/// One-stop capture rendering: snapshot bytes in, standalone SVG document
/// out. The status bar adds one row below the captured grid, exactly as
/// the attach view reserves one below the workload's screen.
pub(crate) fn capture_svg_document(
    paths: &Paths,
    record: &SessionRecord,
    snapshot: &[u8],
) -> String {
    let mut parser = vt100::Parser::new(PARSE_ROWS, PARSE_COLS, 0);
    parser.process(snapshot);
    let screen = parser.screen();
    let (rows, cols) = used_extents(screen);
    let cols = cols.clamp(MIN_COLS, PARSE_COLS);
    let bar = capture_status_bar_text(paths, record, usize::from(cols));
    let width_px = PAD_X * 2.0 + f64::from(cols) * CELL_W;
    let grid_h = f64::from(rows) * CELL_H;
    let height_px = PAD_Y * 2.0 + grid_h + CELL_H;

    let mut body = render_screen(screen, rows, cols);
    let bar_y = PAD_Y + grid_h;
    body.push_str(&format!(
        "<rect x=\"0\" y=\"{bar_y}\" width=\"{width_px}\" height=\"{CELL_H}\" fill=\"{BAR_BG}\"/>\n\
         <text x=\"{PAD_X}\" y=\"{}\" fill=\"{BAR_FG}\">{}</text>\n",
        bar_y + BASELINE,
        escape(&bar)
    ));
    if !screen.hide_cursor() {
        let (cursor_row, cursor_col) = screen.cursor_position();
        if cursor_row < rows && cursor_col < cols {
            let x = PAD_X + f64::from(cursor_col) * CELL_W;
            let y = PAD_Y + f64::from(cursor_row) * CELL_H;
            body.push_str(&format!(
                "<rect x=\"{x}\" y=\"{y}\" width=\"{CELL_W}\" height=\"{CELL_H}\" \
                 fill=\"none\" stroke=\"{}\" stroke-width=\"1\"/>\n",
                css(None, DEFAULT_FG_RGB)
            ));
        }
    }
    let home = env::var_os("HOME").map(PathBuf::from);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width_px}\" height=\"{height_px}\" \
         viewBox=\"0 0 {width_px} {height_px}\" font-family=\"{FONT_FAMILY}\">\n\
         <title>aplexer {}:{}</title>\n\
         <rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n\
         <g font-size=\"{FONT_SIZE}\" xml:space=\"preserve\">\n{body}</g>\n</svg>\n",
        escape(&display_workspace(&record.workspace, home.as_deref())),
        escape(&record.tag),
        css(None, DEFAULT_BG_RGB),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_of(bytes: &[u8]) -> vt100::Screen {
        let mut parser = vt100::Parser::new(6, 40, 0);
        parser.process(bytes);
        parser.screen().clone()
    }

    fn svg_record() -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: 1,
            id: Uuid::nil(),
            workspace: PathBuf::from("/nonexistent"),
            tag: "svgcap".into(),
            engine: "shell".into(),
            profile: None,
            command: Vec::new(),
            cwd: PathBuf::from("/nonexistent"),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: aplexer::Limits::default(),
            history_bytes: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            wake: None,
            agent_override: None,
            phase: Phase::Exited,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: None,
            socket_path: PathBuf::from("/nonexistent"),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn styled_runs_carry_color_and_attributes() {
        let screen = screen_of(b"\x1b[2J\x1b[1;1H\x1b[1;31mHi\x1b[0m ok");
        let body = render_screen(&screen, 6, 40);
        assert!(body.contains("fill=\"#e06c75\""), "{body}");
        assert!(body.contains("font-weight=\"bold\""), "{body}");
        assert!(body.contains(">Hi</text>"), "{body}");
        assert!(body.contains("> ok</text>"), "{body}");
    }

    #[test]
    fn inverse_video_swaps_default_colors() {
        let screen = screen_of(b"\x1b[2J\x1b[1;1H\x1b[7mX\x1b[0m");
        let body = render_screen(&screen, 6, 40);
        // The cell paints the default foreground as its background and
        // writes in the default background color.
        assert!(body.contains("fill=\"#d4d4d4\""), "{body}");
        assert!(body.contains("fill=\"#0c0c0c\""), "{body}");
    }

    #[test]
    fn indexed_256_colors_resolve_through_the_xterm_cube() {
        let screen = screen_of(b"\x1b[2J\x1b[1;1H\x1b[38;5;196mR\x1b[48;5;17mg\x1b[0m");
        let body = render_screen(&screen, 6, 40);
        assert!(body.contains("fill=\"#ff0000\""), "{body}");
        assert!(body.contains("fill=\"#00005f\""), "{body}");
    }

    #[test]
    fn text_is_xml_escaped() {
        let screen = screen_of(b"\x1b[2J\x1b[1;1Ha<b>&");
        let body = render_screen(&screen, 6, 40);
        assert!(body.contains("a&lt;b&gt;&amp;"), "{body}");
        assert!(!body.contains("a<b>"), "{body}");
    }

    #[test]
    fn extents_trim_to_content_and_pad_the_bar_width() {
        let screen = screen_of(b"\x1b[2J\x1b[6;31Hend");
        let (rows, cols) = used_extents(&screen);
        assert_eq!((rows, cols), (6, 34));
        // The document never renders narrower than MIN_COLS even when the
        // content is short, and the bar is fitted to that width.
        let svg = capture_svg_document(
            &Paths {
                runtime_root: PathBuf::from("/nonexistent"),
                state_root: PathBuf::from("/nonexistent"),
                config_file: PathBuf::from("/nonexistent"),
            },
            &svg_record(),
            b"\x1b[2J\x1b[6;31Hend",
        );
        assert!(svg.contains(&format!(
            "width=\"{}\"",
            PAD_X * 2.0 + f64::from(MIN_COLS) * CELL_W
        )));
        assert!(svg.contains("svgcap"), "{svg}");
    }

    #[test]
    fn status_bar_mirrors_the_attach_layout() {
        let record = svg_record();
        let bar = capture_status_bar_text(
            &Paths {
                runtime_root: PathBuf::from("/nonexistent"),
                state_root: PathBuf::from("/nonexistent"),
                config_file: PathBuf::from("/nonexistent"),
            },
            &record,
            120,
        );
        // The degraded one-shot facts (no worker reachable from the
        // fixture) leave the identity and key-hint segments: tag and the
        // bar's trailing hint.
        assert!(bar.contains("svgcap"), "{bar}");
        assert!(bar.contains("^b ?"), "{bar}");
        assert!(bar.contains("RUNNING") || bar.contains("EXITED"), "{bar}");
    }

    #[test]
    fn wide_characters_span_two_cells() {
        // Cursor hidden so the visible cursor (parked after the glyph)
        // does not join the bounds; the glyph alone spans two cells.
        let screen = screen_of("\u{1b}[?25l\u{4e16}".as_bytes());
        let (_, cols) = used_extents(&screen);
        assert_eq!(cols, 2);
        let body = render_screen(&screen, 1, 2);
        assert!(body.contains("\u{4e16}"), "{body}");
        assert!(
            body.contains(&format!("textLength=\"{}\"", 2.0 * CELL_W)),
            "{body}"
        );
    }
}
