/// `Ctrl-b s` opens the session picker -- consumed like every bound chord
/// (no byte reaches the workload), split-proof across reads, with the
/// surrounding bytes still flowing, and distinct from its capital
/// neighbour, which stays unbound and falls through.
#[test]
fn scan_ctrl_b_s_opens_the_session_picker() {
    let mut s = InputScanner::default();
    assert!(matches!(
        s.scan(&[0x02, b's']).as_slice(),
        [InputAction::Sessions]
    ));
    // Split across reads, like every other chord.
    let mut split = InputScanner::default();
    assert!(split.scan(&[0x02]).is_empty());
    assert!(matches!(split.scan(b"s").as_slice(), [InputAction::Sessions]));
    // Ordinary bytes around the chord keep flowing.
    let mut mixed = InputScanner::default();
    let actions = mixed.scan(&[b'a', 0x02, b's', b'b']);
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, b"a"),
        _ => panic!("expected the pre-chord byte forwarded"),
    }
    assert!(matches!(actions[1], InputAction::Sessions));
    match &actions[2] {
        InputAction::Forward(b) => assert_eq!(b, b"b"),
        _ => panic!("expected the post-chord byte forwarded"),
    }
    // Capital S is a different key and remains unbound.
    let mut upper = InputScanner::default();
    assert_eq!(bytes(&upper.scan(&[0x02, b'S'])), vec![0x02, b'S']);
}

/// `Ctrl-b w` opens the workspace picker -- consumed like every bound
/// chord (no byte reaches the workload), split-proof across reads, with
/// the surrounding bytes still flowing. Capital W stays unbound and falls
/// through, exactly like the session picker's capital S.
#[test]
fn scan_ctrl_b_w_opens_the_workspace_picker() {
    let mut s = InputScanner::default();
    assert!(matches!(
        s.scan(&[0x02, b'w']).as_slice(),
        [InputAction::Workspaces]
    ));
    // Split across reads, like every other chord.
    let mut split = InputScanner::default();
    assert!(split.scan(&[0x02]).is_empty());
    assert!(matches!(
        split.scan(b"w").as_slice(),
        [InputAction::Workspaces]
    ));
    // Ordinary bytes around the chord keep flowing.
    let mut mixed = InputScanner::default();
    let actions = mixed.scan(&[b'a', 0x02, b'w', b'b']);
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, b"a"),
        _ => panic!("expected the pre-chord byte forwarded"),
    }
    assert!(matches!(actions[1], InputAction::Workspaces));
    match &actions[2] {
        InputAction::Forward(b) => assert_eq!(b, b"b"),
        _ => panic!("expected the post-chord byte forwarded"),
    }
    // Capital W is a different key and remains unbound.
    let mut upper = InputScanner::default();
    assert_eq!(bytes(&upper.scan(&[0x02, b'W'])), vec![0x02, b'W']);
}

/// The picker's key routing: digits attach while the number exists, a
/// digit past the end is a refusal that keeps the box up, Esc/Ctrl-c
/// cancel, Ctrl-b re-arms the prefix, and anything else is ignored
/// outright -- the picker has nothing to edit, so an unbound key must
/// neither reach the workload nor look like it did.
#[test]
fn picker_keys_route_by_byte() {
    let state = PickerState { count: 3 };
    assert!(matches!(state.key(b'1'), PickerKey::Select(1)));
    assert!(matches!(state.key(b'3'), PickerKey::Select(3)));
    assert!(matches!(state.key(b'4'), PickerKey::OutOfRange(4)));
    assert!(matches!(state.key(b'9'), PickerKey::OutOfRange(9)));
    assert!(matches!(state.key(b'0'), PickerKey::Ignore));
    assert!(matches!(state.key(0x1b), PickerKey::Cancel));
    assert!(matches!(state.key(0x03), PickerKey::Cancel));
    assert!(matches!(state.key(0x02), PickerKey::PrefixThenCancel));
    assert!(matches!(state.key(b'q'), PickerKey::Ignore));
    assert!(matches!(state.key(b' '), PickerKey::Ignore));
    // A one-session workspace still answers its own number (the switch is
    // a silent no-op onto itself) and refuses every other digit.
    let solo = PickerState { count: 1 };
    assert!(matches!(solo.key(b'1'), PickerKey::Select(1)));
    assert!(matches!(solo.key(b'2'), PickerKey::OutOfRange(2)));
}

/// The box numbers its rows through the same formatter the status bar's
/// sibling segment prints -- that is the contract: the number on a picker
/// row is the number `Ctrl-b <n>` attaches, so the current session is
/// starred where the bar stars it and a dead session carries its state
/// where the bar shows it.
#[test]
fn picker_lines_number_like_the_status_bar() {
    let mine = mk_record("/ws/a", "main", Phase::Running);
    let review = mk_record("/ws/a", "review", Phase::Running);
    let corpse = mk_record("/ws/a", "build", Phase::Exited);
    let entries = vec![mine.clone(), review.clone(), corpse.clone()];
    let lines =
        session_picker_lines(&entries, mine.id, 40, 80).expect("40x80 fits three rows");
    assert_eq!(
        lines.len(),
        entries.len() + KEY_OVERLAY_CHROME_ROWS,
        "one row per session, plus two borders and the footer: {lines:#?}"
    );
    assert!(
        lines[0].contains("sessions"),
        "the box names the mode: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("1:main*"),
        "the current session is starred, exactly as the bar stars it: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("2:review"),
        "numbering follows the workspace's list order: {}",
        lines[2]
    );
    assert!(
        lines[3].contains("3:build(exited)"),
        "a dead session shows its state, exactly as the bar shows it: {}",
        lines[3]
    );
    let footer = &lines[lines.len() - 2];
    assert!(
        footer.contains("1-9 attach") && footer.contains("Esc cancel"),
        "the footer names both ways out: {footer}"
    );
}

/// Honest degradation, picker edition: a terminal too short for the whole
/// list gets the first entries -- the ones `a list` prints first -- and a
/// footer that says how much it is not seeing (their digits still attach,
/// because the digits address the workspace's order, not the box's rows).
#[test]
fn picker_lines_trim_from_the_end_and_say_how_much_they_dropped() {
    let entries: Vec<SessionRecord> = (0..6)
        .map(|i| mk_record("/ws/a", &format!("s{i}"), Phase::Running))
        .collect();
    let current = entries[0].id;
    let rows = KEY_OVERLAY_CHROME_ROWS + 2;
    let lines = session_picker_lines(&entries, current, rows, 80).expect("two rows still fit");
    assert_eq!(lines.len(), rows);
    let footer = &lines[lines.len() - 2];
    assert!(
        footer.contains("4 more"),
        "a trimmed box must say how many entries it dropped: {footer}"
    );
    assert!(
        lines[1].contains("1:s0*") && lines[2].contains("2:s1"),
        "the first entries are the ones kept: {lines:#?}"
    );
    let full =
        session_picker_lines(&entries, current, 40, 80).expect("40 rows fit everything");
    assert!(
        !full[full.len() - 2].contains("more"),
        "an untrimmed box does not claim entries are missing: {}",
        full[full.len() - 2]
    );
}

/// Below a box worth drawing there is no box: `run_session_picker` reads
/// this `None` as "flash the one-line list instead", which is the whole of
/// the small-terminal story -- no half-drawn border, nothing suspended.
#[test]
fn picker_lines_refuse_a_terminal_they_cannot_fit() {
    let entry = mk_record("/ws/a", "main", Phase::Running);
    let entries = vec![entry.clone()];
    // Too short: chrome alone, no row for the session itself.
    assert!(
        session_picker_lines(&entries, entry.id, KEY_OVERLAY_CHROME_ROWS, 80).is_none(),
        "chrome with no label row is not a picker"
    );
    assert!(
        session_picker_lines(&entries, entry.id, KEY_OVERLAY_CHROME_ROWS + 1, 80).is_some()
    );
    // Too narrow: the label column would stop being `1:tag` and become
    // ellipses.
    let narrowest = PICKER_CHROME_COLS + PICKER_MIN_CONTENT;
    assert!(session_picker_lines(&entries, entry.id, 40, narrowest - 1).is_none());
    assert!(session_picker_lines(&entries, entry.id, 40, narrowest).is_some());
    assert!(session_picker_lines(&entries, entry.id, 40, 0).is_none());
}

/// Every row is one uniform display width that fits inside the terminal,
/// and the box never claims more rows than it was given -- the same
/// "do not draw outside the screen" guarantee the keymap's box states,
/// for the same reason: the sequence addresses rows absolutely.
#[test]
fn picker_lines_are_uniform_and_stay_inside_the_terminal() {
    let entries: Vec<SessionRecord> = (0..7)
        .map(|i| {
            mk_record(
                "/ws/a",
                &format!("session-{i}"),
                if i == 3 { Phase::Exited } else { Phase::Running },
            )
        })
        .collect();
    let current = entries[0].id;
    for rows in [4usize, 6, 9, 20, 40] {
        for cols in [14usize, 20, 30, 80, 200] {
            let Some(lines) = session_picker_lines(&entries, current, rows, cols) else {
                continue;
            };
            assert!(
                lines.len() <= rows,
                "a {rows}x{cols} box claimed {} rows",
                lines.len()
            );
            let width = terminal_display_width(&lines[0]);
            assert!(width <= cols, "a {rows}x{cols} box is {width} cells wide");
            for line in &lines {
                assert_eq!(
                    terminal_display_width(line),
                    width,
                    "ragged row in a {rows}x{cols} box: {line}"
                );
            }
        }
    }
}

/// The too-small fallback is the same rows the box would have shown,
/// joined -- the status bar's own numbering with a `sessions:` lead, so
/// `Ctrl-b 1-9` keys off it unchanged.
#[test]
fn the_flash_line_is_the_box_without_the_borders() {
    let mine = mk_record("/ws/a", "main", Phase::Running);
    let review = mk_record("/ws/a", "review", Phase::Running);
    let entries = vec![mine.clone(), review.clone()];
    let flash = session_picker_flash_line(&entries, mine.id);
    assert_eq!(flash, "sessions: 1:main* 2:review");
    let lines =
        session_picker_lines(&entries, mine.id, 40, 80).expect("40x80 fits two rows");
    for label in ["1:main*", "2:review"] {
        assert!(
            lines.iter().any(|line| line.contains(label)),
            "the box and the flash must show the same numbering: {label}"
        );
    }
}

/// The workspace box numbers its rows exactly as `a list` badges its
/// groups -- the whole contract: the digit attaches through
/// `SwitchTarget::Workspace` over the same `list_workspace_groups` order
/// the row numbers are printed from -- and stars the workspace the user is
/// attached in, where the list prints `← here`.
#[test]
fn workspace_picker_lines_number_like_a_list_and_star_the_current_one() {
    let groups: Vec<(PathBuf, Vec<SessionRecord>)> = vec![
        (PathBuf::from("/ws/a"), vec![]),
        (PathBuf::from("/ws/b"), vec![]),
    ];
    let lines = workspace_picker_lines(&groups, Path::new("/ws/a"), 40, 80)
        .expect("40x80 fits two rows");
    assert_eq!(
        lines.len(),
        groups.len() + KEY_OVERLAY_CHROME_ROWS,
        "one row per workspace, plus two borders and the footer: {lines:#?}"
    );
    assert!(
        lines[0].contains("workspaces"),
        "the box names the mode: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("1:/ws/a*"),
        "the attached workspace is starred, where `a list` prints `← here`: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("2:/ws/b"),
        "numbering follows the list's group order: {}",
        lines[2]
    );
    let footer = &lines[lines.len() - 2];
    assert!(
        footer.contains("1-9 enter") && footer.contains("Esc cancel"),
        "the footer names both ways out: {footer}"
    );
}

/// The workspace box states the same honest-degradation contract as the
/// session box: trimmed from the end with a "N more" footer (the digits
/// address the registry order, not the box's rows), and no box at all
/// below the minimum -- the one-line flash answers instead.
#[test]
fn workspace_picker_lines_trim_and_refuse_like_the_session_box() {
    let groups: Vec<(PathBuf, Vec<SessionRecord>)> = (0..6)
        .map(|i| (PathBuf::from(format!("/ws/w{i}")), vec![]))
        .collect();
    let current = Path::new("/ws/w0");
    let rows = KEY_OVERLAY_CHROME_ROWS + 2;
    let lines =
        workspace_picker_lines(&groups, current, rows, 80).expect("two rows still fit");
    assert_eq!(lines.len(), rows);
    let footer = &lines[lines.len() - 2];
    assert!(
        footer.contains("4 more"),
        "a trimmed box must say how many workspaces it dropped: {footer}"
    );
    assert!(
        lines[1].contains("1:/ws/w0*") && lines[2].contains("2:/ws/w1"),
        "the first workspaces are the ones kept: {lines:#?}"
    );
    let full =
        workspace_picker_lines(&groups, current, 40, 80).expect("40 rows fit everything");
    assert!(
        !full[full.len() - 2].contains("more"),
        "an untrimmed box does not claim workspaces are missing: {}",
        full[full.len() - 2]
    );
    // Too short for chrome plus one row, or too narrow for the rows and
    // the inlined ` workspaces ` title: the caller's cue to flash instead
    // of drawing. The exact column floor is the title's to set (the
    // border inlines it), so find the boundary rather than name a magic
    // number -- what matters is that one exists.
    assert!(
        workspace_picker_lines(&groups, current, KEY_OVERLAY_CHROME_ROWS, 80).is_none(),
        "chrome with no label row is not a picker"
    );
    let narrowest = (PICKER_CHROME_COLS..=PICKER_CHROME_COLS + 40)
        .find(|&cols| workspace_picker_lines(&groups, current, 40, cols).is_some())
        .expect("a wide enough terminal must fit the box");
    assert!(workspace_picker_lines(&groups, current, 40, narrowest - 1).is_none());
}

/// The workspace fallback carries the same numbering the box would have:
/// a `workspaces:` lead, then the `a list` badges with the star where the
/// list prints `← here`.
#[test]
fn the_workspace_flash_line_is_the_box_without_the_borders() {
    let groups: Vec<(PathBuf, Vec<SessionRecord>)> = vec![
        (PathBuf::from("/ws/a"), vec![]),
        (PathBuf::from("/ws/b"), vec![]),
    ];
    let flash = workspace_picker_flash_line(&groups, Path::new("/ws/a"));
    assert_eq!(flash, "workspaces: 1:/ws/a* 2:/ws/b");
    let lines = workspace_picker_lines(&groups, Path::new("/ws/a"), 40, 80)
        .expect("40x80 fits two rows");
    for label in ["1:/ws/a*", "2:/ws/b"] {
        assert!(
            lines.iter().any(|line| line.contains(label)),
            "the box and the flash must show the same numbering: {label}"
        );
    }
}

/// Every workspace row is one uniform display width that fits inside the
/// terminal -- the same "do not draw outside the screen" guarantee the
/// session box states, for the same reason: the sequence addresses rows
/// absolutely, and long workspace paths are the thing most likely to
/// overflow.
#[test]
fn workspace_picker_lines_are_uniform_and_stay_inside_the_terminal() {
    let groups: Vec<(PathBuf, Vec<SessionRecord>)> = (0..7)
        .map(|i| {
            (
                PathBuf::from(format!("/ws/very-long-workspace-name-{i}")),
                vec![],
            )
        })
        .collect();
    let current = PathBuf::from("/ws/very-long-workspace-name-0");
    for rows in [4usize, 6, 9, 20, 40] {
        for cols in [14usize, 20, 30, 80, 200] {
            let Some(lines) = workspace_picker_lines(&groups, &current, rows, cols) else {
                continue;
            };
            assert!(
                lines.len() <= rows,
                "a {rows}x{cols} box claimed {} rows",
                lines.len()
            );
            let width = terminal_display_width(&lines[0]);
            assert!(width <= cols, "a {rows}x{cols} box is {width} cells wide");
            for line in &lines {
                assert_eq!(
                    terminal_display_width(line),
                    width,
                    "ragged row in a {rows}x{cols} box: {line}"
                );
            }
        }
    }
}
