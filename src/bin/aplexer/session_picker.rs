use super::*;

/// The `Ctrl-b s` and `Ctrl-b w` pickers: a box over the screen listing --
/// for `s` -- this workspace's sessions under the same 1-9 numbering the
/// status bar prints, or -- for `w` -- every workspace under the `[N]`
/// numbering `a list` prints. A digit attaches (the ordinary switch path,
/// exactly the bare chord), Esc cancels, and until one of those arrives no
/// keystroke reaches the workload.
///
/// They are modals like the which-key overlay, not status-row prompts like
/// the rename prompt: a list is inherently multi-row, and the row the
/// prompt owns cannot hold one. That means they go through the same
/// machinery -- `KeyOverlay::active` suspends the relay behind the box, the
/// box renders above the reserved status row, dismissal is a full repaint
/// from the live screen model -- with one addition: `KeyOverlay::kind` says
/// which of the three boxes is up, because the resize thread repaints
/// whichever modal the user is actually looking at and must not swap one
/// for another mid-glance.
///
/// The rows are the same formatters the surfaces the digits key off print
/// -- `numbered_session_label` (the status bar's sibling segment) for
/// sessions, `numbered_workspace_label` (`a list`'s `[N]` badges) for
/// workspaces. That is the whole point of a picker: the number on a row is
/// *guaranteed* to be the number the digit attaches, because both walk the
/// same registry order through the same formatter.
/// What one input byte asks the picker loop to do.
pub(crate) enum PickerKey {
    /// A byte the picker does not answer. Ignored outright -- unlike the
    /// rename prompt there is nothing to edit, so an unbound key must
    /// neither reach the workload nor look like it did.
    Ignore,
    /// A digit with no entry behind it. The picker stays up and the loop
    /// flashes the same refusal the bare chord would have flashed.
    OutOfRange(usize),
    /// Attach the numbered entry and close.
    Select(usize),
    /// Esc or Ctrl-c: close, attach nothing.
    Cancel,
    /// Ctrl-b: close *and* re-arm the prefix scanner, so
    /// `Ctrl-b s Ctrl-b d` detaches instead of typing `d` into the
    /// workload -- the same courtesy the rename prompt extends.
    PrefixThenCancel,
}

/// The rows a picker answers digits within: 1..=count, 1-based. The picker
/// never re-lists between open and pick, so an entry that dies in that
/// window is caught by `perform_switch`'s own `check_attachable`, not by a
/// shrinking list here.
pub(crate) struct PickerState {
    pub(crate) count: usize,
}

impl PickerState {
    pub(crate) fn key(&self, byte: u8) -> PickerKey {
        match byte {
            b'1'..=b'9' => {
                let n = (byte - b'0') as usize;
                if n <= self.count {
                    PickerKey::Select(n)
                } else {
                    PickerKey::OutOfRange(n)
                }
            }
            // A bare Esc cancels. An arrow chord also starts with Esc, and
            // the picker has no cursor for an arrow to move: it cancels
            // too, and the rest of the sequence falls through to the
            // workload like any unbound input once the box is gone -- the
            // same call the rename prompt makes.
            0x1b | 0x03 => PickerKey::Cancel,
            0x02 => PickerKey::PrefixThenCancel,
            _ => PickerKey::Ignore,
        }
    }
}

/// The same group the status bar's sibling segment and `Ctrl-b 1-9` walk:
/// every session of the attached session's workspace, in `list_records`
/// order -- the order `a list` prints, which is where the numbers come
/// from. All of them, including dead ones: the status bar numbers them,
/// so the picker must too, and picking a dead one names it in the error
/// (the `Index` arm's no-skipping contract) rather than silently hopping
/// past it.
pub(crate) fn list_workspace_sessions(
    paths: &Paths,
    record: &SessionRecord,
) -> Result<Vec<SessionRecord>> {
    Ok(list_records(paths)?
        .into_iter()
        .filter(|r| r.workspace == record.workspace)
        .collect())
}

/// `{i}:{name}[*]` for one workspace at position `i` (0-based) of the
/// `list_workspace_groups` order -- the `Ctrl-b w` picker's row label,
/// numbered exactly as `a list` badges its groups and spelled exactly as
/// the list spells them (`display_workspace`, so `~/...` where the list
/// says `~/...`). `*` marks the workspace the user is attached in, where
/// `a list` prints `← here`. No state column, unlike a session label: a
/// workspace is a place, not a process, and the digit's promise is where
/// you land, not what you will find running there.
pub(crate) fn numbered_workspace_label(workspace: &Path, index: usize, current: &Path) -> String {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut label = format!(
        "{}:{}",
        index + 1,
        display_workspace(workspace, home.as_deref())
    );
    if workspace == current {
        label.push('*');
    }
    label
}

/// The box, as text rows already padded to a uniform display width -- or
/// `None` when this terminal cannot hold one, which the caller answers
/// with the one-line flash and no suspension (the which-key overlay's
/// honest degradation).
///
/// The shared body both pickers render through: a title naming the mode,
/// one row per label (`shown` of them -- a terminal too short for the whole
/// list gets the first entries, which are the ones the list prints first,
/// and the footer says how many it is not seeing; their digits still
/// attach, since the digits address the list's order, not the box's rows),
/// a footer naming the two ways out, and the borders.
///
/// Labels and the footer wider than what fits are ellipsized by
/// `fit_overlay_cell`, the same call the keymap's rows go through.
pub(crate) fn picker_box_lines(
    title: &str,
    labels: Vec<String>,
    footer: &str,
    rows: usize,
    cols: usize,
) -> Option<Vec<String>> {
    let capacity = rows.checked_sub(KEY_OVERLAY_CHROME_ROWS)?;
    // The top border inlines the title, so the content column can never be
    // narrower than one cell under the title (`inner >= title width + 1`);
    // below that the border arithmetic underflows, and the honest answer is
    // the one-line flash. The session title fits inside PICKER_MIN_CONTENT;
    // the wider workspace title is what raises the floor.
    let floor = PICKER_MIN_CONTENT.max(terminal_display_width(title).saturating_sub(1));
    if capacity == 0 || cols < PICKER_CHROME_COLS + floor {
        return None;
    }
    let shown = labels.len().min(capacity);
    let hidden = labels.len() - shown;
    let mut footer = footer.to_string();
    if hidden > 0 {
        footer = format!("{hidden} more \u{b7} {footer}");
    }
    // Hug the content, but never spill past the terminal.
    let content = labels
        .iter()
        .map(|l| terminal_display_width(l))
        .max()
        .unwrap_or(0)
        .max(terminal_display_width(&footer))
        .max(floor)
        .min(cols - PICKER_CHROME_COLS);
    let width = PICKER_CHROME_COLS + content;
    let inner = width - 2;

    let mut lines = Vec::with_capacity(shown + KEY_OVERLAY_CHROME_ROWS);
    let title_cells = terminal_display_width(title) + 1;
    lines.push(format!(
        "\u{250c}\u{2500}{title}{}\u{2510}",
        "\u{2500}".repeat(inner - title_cells)
    ));
    for label in &labels[..shown] {
        lines.push(format!(
            "\u{2502} {} \u{2502}",
            fit_overlay_cell(label, inner - 2)
        ));
    }
    lines.push(format!(
        "\u{2502} {} \u{2502}",
        fit_overlay_cell(&footer, inner - 2)
    ));
    lines.push(format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner)));
    Some(lines)
}

/// The session picker's box: `numbered_session_label` rows -- the same
/// formatter the status bar's sibling segment prints, so a row's number is
/// the number `Ctrl-b <digit>` (and the bar) mean.
pub(crate) fn session_picker_lines(
    entries: &[SessionRecord],
    current: Uuid,
    rows: usize,
    cols: usize,
) -> Option<Vec<String>> {
    let labels = entries
        .iter()
        .enumerate()
        .map(|(i, r)| numbered_session_label(r, i, current))
        .collect();
    picker_box_lines(
        " sessions ",
        labels,
        "1-9 attach \u{b7} Esc cancel",
        rows,
        cols,
    )
}

/// The workspace picker's box: `numbered_workspace_label` rows over the
/// same groups `a list` badges, so a row's number is the number the list
/// prints in brackets and the digit attaches through `SwitchTarget::
/// Workspace`.
pub(crate) fn workspace_picker_lines(
    groups: &[(PathBuf, Vec<SessionRecord>)],
    current: &Path,
    rows: usize,
    cols: usize,
) -> Option<Vec<String>> {
    let labels = groups
        .iter()
        .enumerate()
        .map(|(i, (workspace, _))| numbered_workspace_label(workspace, i, current))
        .collect();
    picker_box_lines(
        " workspaces ",
        labels,
        "1-9 enter \u{b7} Esc cancel",
        rows,
        cols,
    )
}

/// The one-line answer for a terminal too small for a box: the same rows
/// the box would have shown, joined -- the status bar's own sibling
/// numbering, so `Ctrl-b 1-9` keys off it unchanged.
pub(crate) fn session_picker_flash_line(entries: &[SessionRecord], current: Uuid) -> String {
    let joined = entries
        .iter()
        .enumerate()
        .map(|(i, r)| numbered_session_label(r, i, current))
        .collect::<Vec<_>>()
        .join(" ");
    format!("sessions: {joined}")
}

/// The workspace picker's too-small fallback, likewise: the `a list`
/// numbering the digit addresses, on one line.
pub(crate) fn workspace_picker_flash_line(
    groups: &[(PathBuf, Vec<SessionRecord>)],
    current: &Path,
) -> String {
    let joined = groups
        .iter()
        .enumerate()
        .map(|(i, (workspace, _))| numbered_workspace_label(workspace, i, current))
        .collect::<Vec<_>>()
        .join(" ");
    format!("workspaces: {joined}")
}

/// The modal bring-up both pickers share: flags flipped under the stdout
/// lock -- the same lock `relay_to_terminal` reads them under, so a
/// workload chunk cannot be half-written across the box's first frame,
/// exactly `show_key_overlay`'s reasoning -- then the bar's dirty-check
/// cleared (a writer that is not the dirty-check's usual one is about to
/// draw it) and the box painted.
///
/// `false` means the box did not go up: another modal already owned the
/// screen and is not ours to take down (nothing is changed, like
/// `show_key_overlay`), or the paint failed and the overlay was dismissed
/// again -- never leave the relay suspended behind a box nobody can see.
fn show_picker_box(config: &InputThreadConfig, kind: OverlayKind, lines: &[String]) -> bool {
    {
        let _held = config
            .status
            .stdout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if config.status.overlay.active.swap(true, Ordering::SeqCst) {
            return false;
        }
        config.status.overlay.set_kind(kind);
    }
    *config
        .status
        .last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    if paint_overlay_lines(&config.status, lines) {
        true
    } else {
        dismiss_key_overlay(&config.status);
        false
    }
}

/// The key-reading loop both pickers share. Takes over stdin reads for the
/// duration -- the scanner is not consulted, so no keystroke reaches the
/// workload and none of the chords fire. Returns the picked number
/// (1-based) and whether a mid-picker `Ctrl-b` must re-arm the prefix, so
/// the key after it is read as a chord, not typed into anything.
///
/// `out_of_range` formats the refusal a digit past the end flashes -- the
/// same refusal the bare chord would have flashed -- and keeps the box up.
fn read_picker_choice(
    config: &InputThreadConfig,
    state: PickerState,
    out_of_range: impl Fn(usize) -> String,
) -> (Option<usize>, bool) {
    let mut input = io::stdin();
    let mut buffer = [0u8; 256];
    let mut selected: Option<usize> = None;
    let mut rearm_prefix = false;
    'picker: loop {
        let n = match input.read(&mut buffer) {
            Ok(0) => break 'picker,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break 'picker,
            Ok(n) => n,
        };
        for &byte in &buffer[..n] {
            match state.key(byte) {
                PickerKey::Ignore => {}
                PickerKey::OutOfRange(n) => flash_status(&config.status, out_of_range(n)),
                PickerKey::Select(n) => {
                    selected = Some(n);
                    break 'picker;
                }
                PickerKey::Cancel => break 'picker,
                PickerKey::PrefixThenCancel => {
                    rearm_prefix = true;
                    break 'picker;
                }
            }
        }
    }
    (selected, rearm_prefix)
}

/// Run the `Ctrl-b s` session picker to completion on the input thread, and
/// always leave the relay un-suspended and the live screen back on the host.
pub(crate) fn run_session_picker(config: &InputThreadConfig, scanner: &mut InputScanner) {
    let current = config
        .status
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let entries = match list_workspace_sessions(&config.status.paths, &current) {
        Ok(entries) => entries,
        Err(error) => {
            flash_status(&config.status, format!("{error:#}"));
            return;
        }
    };
    if entries.is_empty() {
        // Unreachable in practice -- the attached session is in the list --
        // but a box with no rows would be a modal that says nothing.
        flash_status(&config.status, "no sessions in this workspace");
        return;
    }
    let geom = match config.status.term.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    let Some(lines) = session_picker_lines(
        &entries,
        current.id,
        key_overlay_rows(geom) as usize,
        geom.cols as usize,
    ) else {
        // Too small for a box: the honest degradation -- a one-line answer
        // on the status bar, and nothing suspended.
        flash_status(
            &config.status,
            session_picker_flash_line(&entries, current.id),
        );
        return;
    };
    if !show_picker_box(config, OverlayKind::Sessions, &lines) {
        return;
    }

    let count = entries.len();
    let (selected, rearm_prefix) = read_picker_choice(config, PickerState { count }, |n| {
        format!("no session {n} here: this workspace has {count} session(s)")
    });
    // Take the box down before acting on the choice: the switch's replay
    // and any error flash both need the live screen, and the relay has to
    // resume. Every exit path goes through here -- including EOF/error,
    // where the outer loop is about to detach anyway but the box must not
    // keep covering a terminal nobody can dismiss it from.
    dismiss_key_overlay(&config.status);
    if rearm_prefix {
        scanner.pending_ctrl_b = true;
    }
    if let Some(n) = selected {
        // The same switch a bare `Ctrl-b <n>` performs, with the same
        // failure containment: nothing here has touched the live
        // attachment, so a target that died between opening the picker and
        // picking it is a status-bar flash and the user stays put.
        if let Err(error) = perform_switch(config, SwitchTarget::Index(n)) {
            flash_status(&config.status, format!("{error:#}"));
        }
    }
}

/// Run the `Ctrl-b w` workspace picker: `run_session_picker`'s contract one
/// level up. The rows are `list_workspace_groups` -- the list `a list`
/// prints, `SwitchTarget::Workspace` resolves against, and this box
/// numbers by -- and a digit enters the workspace at
/// `workspace_entry_session`, the same place `Ctrl-b Down` would.
pub(crate) fn run_workspace_picker(config: &InputThreadConfig, scanner: &mut InputScanner) {
    let current = config
        .status
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let groups = match list_workspace_groups(&config.status.paths) {
        Ok(groups) => groups,
        Err(error) => {
            flash_status(&config.status, format!("{error:#}"));
            return;
        }
    };
    if groups.is_empty() {
        // Unreachable in practice -- the attached session's own workspace
        // is a group -- but a box with no rows would be a modal that says
        // nothing.
        flash_status(&config.status, "no workspaces to switch to");
        return;
    }
    let geom = match config.status.term.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    let Some(lines) = workspace_picker_lines(
        &groups,
        &current.workspace,
        key_overlay_rows(geom) as usize,
        geom.cols as usize,
    ) else {
        // Too small for a box: the honest degradation -- a one-line answer
        // on the status bar, and nothing suspended.
        flash_status(
            &config.status,
            workspace_picker_flash_line(&groups, &current.workspace),
        );
        return;
    };
    if !show_picker_box(config, OverlayKind::Workspaces, &lines) {
        return;
    }

    let count = groups.len();
    let (selected, rearm_prefix) = read_picker_choice(config, PickerState { count }, |n| {
        format!("no workspace {n} here: there are {count} workspace(s)")
    });
    dismiss_key_overlay(&config.status);
    if rearm_prefix {
        scanner.pending_ctrl_b = true;
    }
    if let Some(n) = selected {
        // The same containment as the session picker: the target is
        // re-resolved inside `perform_switch`, so a workspace whose last
        // live session died between opening the picker and picking it is a
        // status-bar flash naming it, and the user stays put.
        if let Err(error) = perform_switch(config, SwitchTarget::Workspace(n)) {
            flash_status(&config.status, format!("{error:#}"));
        }
    }
}

/// Repaint the session picker at the current geometry -- the resize
/// thread's half of the bargain. The list is re-derived from the registry
/// rather than parked anywhere, so a repaint can never show a stale box.
///
/// Returns `false` when it no longer fits (or the registry read failed),
/// which the caller answers by taking the overlay down rather than leaving
/// a stale box over a resumed relay.
pub(crate) fn repaint_session_picker(ctx: &StatusBarCtx) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let Ok(entries) = list_workspace_sessions(&ctx.paths, &record) else {
        return false;
    };
    let Some(lines) = session_picker_lines(
        &entries,
        record.id,
        key_overlay_rows(geom) as usize,
        geom.cols as usize,
    ) else {
        return false;
    };
    paint_overlay_lines(ctx, &lines)
}

/// Repaint the workspace picker, likewise: same geometry, same
/// re-derive-from-the-registry discipline, different rows.
pub(crate) fn repaint_workspace_picker(ctx: &StatusBarCtx) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let Ok(groups) = list_workspace_groups(&ctx.paths) else {
        return false;
    };
    let Some(lines) = workspace_picker_lines(
        &groups,
        &record.workspace,
        key_overlay_rows(geom) as usize,
        geom.cols as usize,
    ) else {
        return false;
    };
    paint_overlay_lines(ctx, &lines)
}

/// Columns the box spends on things that are not a label: its two borders
/// plus the padding either side.
pub(crate) const PICKER_CHROME_COLS: usize = 4;

/// The narrowest label column worth drawing a box for. Below it the rows
/// stop being `1:tag`/`1:~/path` entries and become ellipses, which is
/// again worse than the one-line flash.
pub(crate) const PICKER_MIN_CONTENT: usize = 10;
