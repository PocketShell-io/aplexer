use super::*;

/// Byte-level routing of stdin while mouse capture or the pager is active.
/// Wheel reports page through ordinary output. An alternate-screen workload
/// that requested mouse reporting gets its wheel reports instead: its own
/// transcript lives in its repaintable viewport, not in terminal scrollback.
/// Pager keys never reach the workload.
///
/// Returns the bytes that may still be forwarded. In scroll mode that is
/// empty except while type-through has handed the keyboard to the workload
/// (`i`): every byte is otherwise consumed, navigation or not.
///
/// `pending` exists because a mouse report can be split across two `read()`s
/// exactly like the `Ctrl-b` prefix can. Outside scroll mode it is only ever
/// allowed to hold a buffer that has already produced a full mouse
/// introducer (`\x1b[<` or `\x1b[M`) -- no keyboard emits either, so nothing
/// a user types can be delayed by it. A bare `ESC` or `ESC [` at the end of a chunk
/// is forwarded immediately rather than held, because holding it would make
/// the Escape key in the user's editor wait for the next keystroke.
#[derive(Default)]
pub(crate) struct ScrollInput {
    pub(crate) pending: Vec<u8>,
}

/// What one step of `ScrollInput::route` made of the bytes at the cursor.
enum Routed {
    /// This many bytes were forwarded or swallowed; carry on after them.
    Consumed(usize),
    /// A mode change ran and `pending` was drained through it; carry on
    /// from the front of what is left.
    Rebased,
    /// A sequence has begun but not finished; keep the bytes for the next
    /// `read()`.
    Wait,
    /// A mode change ran and everything after it was typed against the
    /// mode that just ended: drop it, and stop.
    Discard,
}

impl ScrollInput {
    pub(crate) fn route(&mut self, ctx: &StatusBarCtx, bytes: &[u8]) -> Vec<u8> {
        let client_mouse = matches!(
            *ctx.mouse_owned
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Some(true)
        );
        if !ctx.scroll.is_active() && !client_mouse && !ctx.mouse_capture {
            // Mouse capture is disabled: leave the workload's input alone.
            let mut out = std::mem::take(&mut self.pending);
            out.extend_from_slice(bytes);
            return out;
        }
        self.pending.extend_from_slice(bytes);
        let mut out: Vec<u8> = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            // Re-read every byte: a step can change the mode for the rest
            // of the buffer (`i` into type-through, `q` out of the pager, a
            // wheel roll into it).
            let routed = if ctx.scroll.is_typing() {
                self.route_typing(ctx, at, &mut out)
            } else if ctx.scroll.is_active() {
                self.route_pager(ctx, at)
            } else {
                self.route_live_mouse(ctx, at, &mut out, client_mouse)
            };
            match routed {
                Routed::Consumed(n) => at += n,
                Routed::Rebased => at = 0,
                Routed::Wait => break,
                Routed::Discard => {
                    self.pending.clear();
                    return out;
                }
            }
        }
        self.pending.drain(..at);
        out
    }

    /// Type-through (`i`): the keyboard belongs to the workload now, so
    /// bytes forward verbatim. Mouse reports are swallowed while the pager
    /// owns the host mouse, and a lone-ESC chunk takes the keyboard back.
    /// Anything else starting with ESC -- arrows, Home, a sequence split across reads --
    /// is somebody's key, not text, and forwards whole.
    fn route_typing(&mut self, ctx: &StatusBarCtx, at: usize, out: &mut Vec<u8>) -> Routed {
        let rest = &self.pending[at..];
        if rest[0] == 0x1b {
            if rest.starts_with(b"\x1b[<") {
                match parse_sgr_mouse(rest) {
                    MouseParse::Complete(_, consumed) => return Routed::Consumed(consumed),
                    MouseParse::Incomplete => return Routed::Wait,
                    MouseParse::NotMouse => {}
                }
            } else if rest.starts_with(b"\x1b[M") {
                match parse_x10_mouse(rest, self.workload_mouse_uses_utf8(ctx)) {
                    MouseParse::Complete(_, consumed) => return Routed::Consumed(consumed),
                    MouseParse::Incomplete => return Routed::Wait,
                    MouseParse::NotMouse => {}
                }
            } else if rest.len() == 1 {
                exit_typing(ctx);
                // Whatever else was typed into this same chunk was typed
                // blind against a pager that is back in charge: discard it,
                // exactly like the bytes that trail a pager Exit.
                return Routed::Discard;
            }
        }
        out.push(self.pending[at]);
        Routed::Consumed(1)
    }

    /// The pager has the keyboard: every byte is a navigation key or is
    /// swallowed, and nothing reaches the workload.
    fn route_pager(&mut self, ctx: &StatusBarCtx, at: usize) -> Routed {
        if self.pending[at..].starts_with(b"\x1b[M") {
            match parse_x10_mouse(&self.pending[at..], self.workload_mouse_uses_utf8(ctx)) {
                MouseParse::Complete(report, consumed) => {
                    if let Some(command) = wheel_direction(report.button) {
                        self.pending.drain(..at + consumed);
                        apply_scroll_command(ctx, command);
                        return if ctx.scroll.is_active() {
                            Routed::Rebased
                        } else {
                            Routed::Discard
                        };
                    }
                    return Routed::Consumed(consumed);
                }
                MouseParse::Incomplete => return Routed::Wait,
                MouseParse::NotMouse => {}
            }
        }
        match scroll_keys(&self.pending[at..]) {
            ScrollKey::Command(command, n) => {
                // The buffer is advanced *before* the command runs, so an
                // Exit landing mid-buffer leaves the bytes after it to be
                // routed by the mode that follows rather than swallowed
                // with it.
                self.pending.drain(..at + n);
                apply_scroll_command(ctx, command);
                if ctx.scroll.is_active() {
                    Routed::Rebased
                } else {
                    // The command closed the pager. Everything left in this
                    // buffer was typed while the pager still had the
                    // keyboard, so it is discarded rather than forwarded:
                    // the user meant it for the pager, and "the rest of the
                    // keystroke you were reading with lands in your agent's
                    // prompt" is the exact failure this mode exists to
                    // prevent. Bytes from the next read() go to the workload
                    // normally.
                    Routed::Discard
                }
            }
            ScrollKey::Ignored(n) => Routed::Consumed(n),
            ScrollKey::Incomplete => Routed::Wait,
        }
    }

    /// Route the wheel to the pane for ordinary output, or to a full-screen
    /// workload that requested mouse input. Clicks and motion still reach it.
    fn route_live_mouse(
        &mut self,
        ctx: &StatusBarCtx,
        at: usize,
        out: &mut Vec<u8>,
        client_mouse: bool,
    ) -> Routed {
        let rest = &self.pending[at..];
        if rest.starts_with(b"\x1b[<") || rest.starts_with(b"\x1b[M") {
            let parsed = if rest.starts_with(b"\x1b[<") {
                parse_sgr_mouse(rest)
            } else {
                parse_x10_mouse(rest, self.workload_mouse_uses_utf8(ctx))
            };
            match parsed {
                MouseParse::Complete(report, consumed) => {
                    if let Some(command) = wheel_direction(report.button) {
                        // Full-screen TUIs such as Codex and OpenCode keep
                        // their transcript in application state and repaint
                        // the alternate grid when they receive a wheel
                        // event. The grid has no scrollback for our pager.
                        // Only forward when the workload actually requested
                        // mouse input and owns the host's mouse protocol;
                        // otherwise the report was generated for us.
                        let workload_scrolls = !client_mouse && {
                            let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
                            screen.alternate_screen() && screen.workload_wants_mouse()
                        };
                        if workload_scrolls {
                            out.extend_from_slice(&rest[..consumed]);
                            return Routed::Consumed(consumed);
                        }
                        if report.press && matches!(command, ScrollCommand::Up(_)) {
                            self.pending.drain(..at + consumed);
                            enter_scroll_mode(ctx, command);
                            return Routed::Rebased;
                        }
                        // Down at the live bottom and wheel releases have
                        // nowhere to go; neither belongs to the workload.
                        return Routed::Consumed(consumed);
                    }
                    if !client_mouse
                        && ctx
                            .screen
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .workload_wants_mouse()
                    {
                        out.extend_from_slice(&rest[..consumed]);
                    }
                    return Routed::Consumed(consumed);
                }
                MouseParse::Incomplete => return Routed::Wait,
                MouseParse::NotMouse => {}
            }
        }
        out.push(self.pending[at]);
        Routed::Consumed(1)
    }

    fn workload_mouse_uses_utf8(&self, ctx: &StatusBarCtx) -> bool {
        ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .workload_mouse_encoding()
            == vt100::MouseProtocolEncoding::Utf8
    }
}
