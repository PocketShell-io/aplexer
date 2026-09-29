use super::*;

/// One button press/release reported by xterm's SGR extended mouse mode
/// (`CSI ?1006h`, paired with `CSI ?1000h` click tracking) --
/// docs/clickable-status-bar-design.md section 2. `col`/`row` are 1-based,
/// matching the wire format, so a caller subtracts 1 before comparing
/// against `TermGeom.rows`. Decoded by `scroll_keys` (the pager's wheel)
/// and `ScrollInput::route` (the wheel that opens the pager, and the
/// reports swallowed while the client holds the mouse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseReport {
    pub(crate) button: u32,
    pub(crate) press: bool,
    pub(crate) col: u16,
    pub(crate) row: u16,
}

/// Result of attempting to parse an SGR mouse report off the front of a
/// buffer: a real hit (with the byte length consumed), "not this at all"
/// (any other byte sequence, including ordinary CSI sequences like arrow
/// keys -- `ESC [ <` is not a prefix any keyboard-generated input or other
/// terminal report uses, so this is an unambiguous, fast rejection), or
/// "looks like the start of one but the buffer ends before `M`/`m`" -- the
/// signal a live scanner needs to keep buffering across `read()` calls, the
/// same role `pending_ctrl_b` plays for the one-byte `Ctrl-b` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseParse {
    NotMouse,
    Incomplete,
    Complete(MouseReport, usize),
}

/// The longest run of bytes still read as a report that has not finished
/// arriving. A real report is at most `ESC [ <` plus three fields of a few
/// digits each, well under this; past it the bytes are a stray `ESC [ <`
/// followed by digits, and the same bound the generic CSI decoder applies
/// (`scroll_keys`) stops the caller buffering them forever.
pub(crate) const SGR_MOUSE_MAX_LEN: usize = 32;

/// Pure parser for `ESC [ < Cb ; Cx ; Cy [Mm]` at the start of `buf`
/// (docs/clickable-status-bar-design.md section 2/4.4). Never panics on
/// malformed input; malformed-but-prefix-matching input that can't
/// possibly resolve (non-digit where a number is expected, once the `<`
/// has been seen, or a run longer than `SGR_MOUSE_MAX_LEN` without a
/// terminator) is reported `NotMouse` rather than `Incomplete`, so a
/// caller doesn't buffer forever waiting for a `M`/`m` that will never
/// come.
pub(crate) fn parse_sgr_mouse(buf: &[u8]) -> MouseParse {
    const PREFIX: &[u8] = b"\x1b[<";
    if buf.len() < PREFIX.len() {
        if PREFIX.starts_with(buf) {
            return MouseParse::Incomplete;
        }
        return MouseParse::NotMouse;
    }
    if &buf[..PREFIX.len()] != PREFIX {
        return MouseParse::NotMouse;
    }
    // Three ';'-separated decimal fields, terminated by 'M' (press) or 'm'
    // (release). Parse by scanning for the terminator rather than
    // pre-splitting, so a genuinely truncated buffer (no terminator yet)
    // is correctly reported Incomplete instead of NotMouse.
    let rest = &buf[PREFIX.len()..];
    let mut fields: [u32; 3] = [0; 3];
    let mut field_idx = 0;
    let mut cur: u32 = 0;
    let mut have_digit = false;
    for (i, &b) in rest.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                have_digit = true;
                cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
            }
            b';' => {
                if !have_digit || field_idx >= 2 {
                    return MouseParse::NotMouse;
                }
                fields[field_idx] = cur;
                field_idx += 1;
                cur = 0;
                have_digit = false;
            }
            b'M' | b'm' => {
                if !have_digit || field_idx != 2 {
                    return MouseParse::NotMouse;
                }
                fields[2] = cur;
                let consumed = PREFIX.len() + i + 1;
                let row = u16::try_from(fields[2]).unwrap_or(u16::MAX);
                let col = u16::try_from(fields[1]).unwrap_or(u16::MAX);
                return MouseParse::Complete(
                    MouseReport {
                        button: fields[0],
                        press: b == b'M',
                        col,
                        row,
                    },
                    consumed,
                );
            }
            _ => return MouseParse::NotMouse,
        }
    }
    // Ran out of buffer with no terminator yet, but every byte seen so far
    // was a valid digit/`;` -- genuinely incomplete, keep buffering, up to
    // the bound.
    if buf.len() > SGR_MOUSE_MAX_LEN {
        MouseParse::NotMouse
    } else {
        MouseParse::Incomplete
    }
}

/// X10 mouse reports use three values after `ESC [ M`. DECSET 1005 encodes
/// each value as UTF-8; otherwise each is one byte. Keep the original bytes
/// for non-wheel events, since the workload may expect either encoding.
pub(crate) fn parse_x10_mouse(buf: &[u8], utf8: bool) -> MouseParse {
    const PREFIX: &[u8] = b"\x1b[M";
    if buf.len() < PREFIX.len() {
        return if PREFIX.starts_with(buf) {
            MouseParse::Incomplete
        } else {
            MouseParse::NotMouse
        };
    }
    if !buf.starts_with(PREFIX) {
        return MouseParse::NotMouse;
    }
    let mut at = PREFIX.len();
    let mut values = [0u32; 3];
    for value in &mut values {
        if at == buf.len() {
            return MouseParse::Incomplete;
        }
        if utf8 {
            let first = buf[at];
            let len = match first {
                0x00..=0x7f => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return MouseParse::NotMouse,
            };
            if buf.len() - at < len {
                return MouseParse::Incomplete;
            }
            let Ok(s) = std::str::from_utf8(&buf[at..at + len]) else {
                return MouseParse::NotMouse;
            };
            *value = s.chars().next().unwrap() as u32;
            at += len;
        } else {
            *value = u32::from(buf[at]);
            at += 1;
        }
        if *value < 32 {
            return MouseParse::NotMouse;
        }
        *value -= 32;
    }
    MouseParse::Complete(
        MouseReport {
            button: values[0],
            press: true,
            col: u16::try_from(values[1]).unwrap_or(u16::MAX),
            row: u16::try_from(values[2]).unwrap_or(u16::MAX),
        },
        at,
    )
}

/// Shift/Ctrl/Alt add modifier bits to a wheel button number.
pub(crate) fn wheel_direction(button: u32) -> Option<ScrollCommand> {
    match button & !0x1c {
        MOUSE_WHEEL_UP => Some(ScrollCommand::Up(WHEEL_LINES)),
        MOUSE_WHEEL_DOWN => Some(ScrollCommand::Down(WHEEL_LINES)),
        _ => None,
    }
}
