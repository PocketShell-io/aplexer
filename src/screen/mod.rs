//! Server-side live terminal state (docs/terminal-state-design.md).
//!
//! `ScreenTracker` feeds every PTY byte through a `vt100::Parser` --
//! continuously, whether or not a client is attached -- and can render the
//! *current screen* on demand for reattach (`snapshot()`), instead of
//! replaying raw byte history. This is the aplexer equivalent of tmux's
//! per-pane virtual terminal; see the design doc section 4-6 for the full
//! rationale and section 5.4 for why `MarginTracker` exists alongside it.

use anyhow::{bail, Result};

mod boundary;
mod client;
mod host_alt;
mod margins;
#[cfg(test)]
mod tests;
mod tracker;

pub use boundary::StreamBoundary;
pub use client::ClientScreen;
pub use margins::{CsiEvent, MarginTracker};
pub use tracker::{LayoutChange, ScreenTracker};

/// Maximum number of cells retained by the worker's live terminal model.
///
/// `vt100` keeps both normal and alternate grids and each cell carries
/// formatting state, so accepting the protocol's full `u16 * u16` range
/// would let a local client request multiple gigabytes of allocation. This
/// still permits unusually large terminals (for example 512x512) while
/// keeping each session's model within a defensible fixed bound.
pub const MAX_SCREEN_CELLS: usize = 256 * 1024;

/// Ceiling on the *scrollback* cells a client-side model may retain, on top
/// of the visible grid `MAX_SCREEN_CELLS` bounds.
///
/// Scrollback is the one part of the model whose size the user configures
/// (`APLEXER_HISTORY_LIMIT`, tmux's `history-limit`), so it needs its own
/// bound rather than borrowing the screen's: a `vt100` cell is 32 bytes, so
/// a naive "100000 lines" on a 200-column terminal would ask for 640 MB.
/// One million cells is 32 MB at that cell size -- generous next to the
/// 2000-line default (12.8 MB at 200 columns, 5 MB at 80) and still a hard
/// ceiling a typo cannot blow past.
pub const MAX_SCROLLBACK_CELLS: usize = 1024 * 1024;

/// Default retained scrollback, in lines. Deliberately tmux's own
/// `history-limit` default, because that is the number the user is
/// measuring aplexer against.
pub const DEFAULT_SCROLLBACK_LINES: usize = 2000;

/// The scrollback line count actually usable at `cols` columns: the request,
/// clamped so `lines * cols` stays inside `MAX_SCROLLBACK_CELLS`. Returns 0
/// for a request of 0 (the worker's model, which retains no history -- see
/// `ScreenTracker::try_new`).
pub fn scrollback_lines_for(cols: u16, lines: usize) -> usize {
    if lines == 0 {
        return 0;
    }
    let cols = usize::from(cols.max(1));
    lines.min(MAX_SCROLLBACK_CELLS / cols).max(1)
}

/// Conventional geometry used when a PTY exists but its kernel winsize has
/// not been initialized. Linux reports that state as `0x0`; feeding the
/// zeros to vt100 leaves several parser operations with a degenerate grid.
pub const DEFAULT_TERMINAL_ROWS: u16 = 24;
pub const DEFAULT_TERMINAL_COLS: u16 = 80;

/// vt100's automatic-wrap path cannot safely scroll a one-row grid: when the
/// cursor wraps, the previous row scrolls off before vt100 marks it wrapped.
/// The worker's PTY must match the parser's minimum, including while a small
/// viewport or the on-screen keyboard reports only one row.
pub const MIN_TERMINAL_ROWS: u16 = 2;

/// Normalize the protocol's zero dimensions and reject grids that would
/// exceed the worker's fixed cell budget. A nonzero one-row request stays
/// one row; worker PTY callers use [`validate_worker_size`] and vt100 model
/// constructors use [`validate_screen_size`] when they need the parser-safe
/// minimum.
pub fn validate_size(rows: u16, cols: u16) -> Result<(u16, u16)> {
    validate_size_with_minimum(rows, cols, 1)
}

/// Normalize geometry for the aplexer-owned workload PTY and its matching
/// screen model. An outer SSH PTY can accept 37x1 while the nested workload
/// sees 37x2; columns stay unchanged, but a one-row viewport can have one
/// extra row of workload output beyond its visible height. Keep this floor at
/// the worker boundary so a requested host geometry remains distinguishable
/// from the worker's safe effective geometry.
pub(crate) fn validate_worker_size(rows: u16, cols: u16) -> Result<(u16, u16)> {
    validate_size_with_minimum(rows, cols, MIN_TERMINAL_ROWS)
}

/// Normalize the geometry of any vt100-backed model. `ClientScreen` also
/// needs this minimum: it parses the same wrapped PTY byte stream and would
/// otherwise hit vt100's one-row wrap panic even when its physical terminal
/// remains one row tall.
pub(crate) fn validate_screen_size(rows: u16, cols: u16) -> Result<(u16, u16)> {
    validate_size_with_minimum(rows, cols, MIN_TERMINAL_ROWS)
}

fn validate_size_with_minimum(rows: u16, cols: u16, min_rows: u16) -> Result<(u16, u16)> {
    let rows = if rows == 0 {
        DEFAULT_TERMINAL_ROWS
    } else {
        rows.max(min_rows)
    };
    let cols = if cols == 0 {
        DEFAULT_TERMINAL_COLS
    } else {
        cols
    };
    let cells = usize::from(rows)
        .checked_mul(usize::from(cols))
        .ok_or_else(|| anyhow::anyhow!("terminal dimensions overflow"))?;
    if cells > MAX_SCREEN_CELLS {
        bail!("terminal size {rows}x{cols} exceeds the maximum of {MAX_SCREEN_CELLS} cells");
    }
    Ok((rows, cols))
}
