//! `Runtime` — Keyboard, mouse, wheel, IME, and input-byte routing.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::*;

/// Maximum preedit scalars kept from one IME composition (CTX-0367).
///
/// Matches the `input-pointer-rfc.md` preedit overlay bound (128 chars) and
/// the platform seam's own truncation; the overlay additionally clips to the
/// pane width, so no composition can overdraw or grow the frame.
pub const IME_PREEDIT_MAX_CHARS: usize = 128;

/// Maximum characters committed from one IME commit (CTX-0367).
///
/// Matches `text-rendering-rfc.md` TXT-11 and the platform seam truncation
/// (256 chars / 1024 UTF-8 bytes).
pub const IME_COMMIT_MAX_CHARS: usize = 256;

/// Maximum UTF-8 bytes committed from one IME commit (CTX-0367).
pub const IME_COMMIT_MAX_BYTES: usize = 1024;

/// Truncates `text` to at most `max` scalars at a char boundary.
///
/// Pure and allocation-minimal: returns the input unchanged when it already
/// fits, otherwise copies only the kept prefix.
fn truncate_chars(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut out = String::new();
    for ch in text.chars().take(max) {
        out.push(ch);
    }
    out
}

/// Bounded human-readable label for a [`KeyEvent`] (CTX-0159 input ring).
///
/// Prefers the layout-dependent `text` when present (so `wtype` probes show
/// the typed character), else the logical key name. Truncated to 32
/// characters here; the ring applies the final [`crate::inspect`] bound.
pub(super) fn key_inspect_label(event: &KeyEvent) -> String {
    if let Some(text) = event.text.as_deref() {
        let first = text.chars().next().unwrap_or('?');
        if !text.is_empty() && text.chars().count() == 1 && !first.is_control() {
            return format!("key:{text}");
        }
    }
    match &event.logical_key {
        bitty_platform::LogicalKey::Character(s) => {
            let short: String = s.chars().take(8).collect();
            format!("key:{short}")
        }
        bitty_platform::LogicalKey::Named(named) => format!("key:{named:?}"),
        _ => "key:Unidentified".to_string(),
    }
}

impl Runtime {
    /// Current IME preedit overlay, if any (presentation only).
    ///
    /// Non-`None` exactly while a composition is active; the inline overlay
    /// paints from this model and the key path suppresses raw input while it
    /// is `Some` (CTX-0367).
    #[must_use]
    pub fn ime_preedit(&self) -> Option<&str> {
        self.ime_preedit.as_deref()
    }

    /// Physical-pixel caret rect for the platform IME candidate window.
    ///
    /// `Some` while the focused leaf painted a visible cursor in the last
    /// presented frame; the embedder forwards it through
    /// `bitty_platform::WindowHandle::set_ime_cursor_area` when it changes
    /// (CTX-0367). `None` before the first present, while the cursor is
    /// hidden, or while the window is unfocused.
    #[must_use]
    pub fn ime_cursor_area(&self) -> Option<ImeCursorArea> {
        self.ime_caret.map(|caret| caret.area)
    }

    /// Encodes a [`KeyEvent`] into the terminal input bytes (legacy xterm).
    ///
    /// Pure, headless, and deterministic: delegates to
    /// [`bitty_platform::encode_key_event`] which owns the xterm legacy table
    /// (M1 required baseline; Kitty protocol is deferred). Returns `None` for
    /// release/synthetic/modifier-only/unmapped inputs. This entry point
    /// assumes no modifiers are held; the live input path
    /// ([`Self::handle_key_event`]) applies the tracked modifier snapshot via
    /// [`encode_key_with_kitty`](Self::encode_key_with_kitty) instead, so
    /// `Ctrl+letter` synthesizes C0 bytes on Wayland where winit reports
    /// `text=None` (CTX-0154).
    #[must_use]
    pub fn encode_key_event(event: &KeyEvent) -> Option<Vec<u8>> {
        bitty_platform::encode_key_event(event)
    }

    /// Snapshots the tracked modifier flags for the legacy encoder.
    ///
    /// winit delivers modifiers (`ModifiersChanged`, modifier key presses)
    /// separately from the key press itself, and on Wayland the press carries
    /// `text=None` (or the bare letter) for `Ctrl+letter`. The legacy encoder
    /// therefore cannot rely on `text` and synthesizes C0/ESC bytes from this
    /// snapshot instead (CTX-0154).
    pub(super) fn modifier_snapshot(&self) -> bitty_platform::ModifiersState {
        bitty_platform::ModifiersState {
            shift: self.shift_pressed,
            control: self.control_pressed,
            alt: self.alt_pressed,
            super_pressed: false,
        }
    }

    /// Encodes with Kitty protocol when `kitty_flags != 0` (opt-in 7727).
    /// Bounded to 64 bytes per spec; progressive flags are honored with fallback to legacy when flag not set.
    pub(super) fn encode_key_with_kitty(&self, event: &KeyEvent) -> Option<Vec<u8>> {
        if self.kitty_flags == 0 {
            return bitty_platform::keyboard::encode_key_event_with_modifiers(
                event,
                &self.modifier_snapshot(),
            );
        }
        // Kitty active: produce CSI u for disambiguated keys, else legacy with fallback.
        // For vertical slice, handle Tab vs Ctrl-I disambiguation and Enter vs Ctrl-M.
        // When event is Tab with Ctrl modifier (text is \t but logical is Tab), encode Kitty distinct.
        // Simplistic: if logical is Named(Tab) and control_pressed, encode Kitty 9;1:1 etc.
        // General: encode any character key as CSI <codepoint> ; mods u
        // Bounded: single key ≤64 bytes, checked.
        let mods: u8 = (if self.shift_pressed { 1 } else { 0 })
            | (if self.alt_pressed { 2 } else { 0 })
            | (if self.control_pressed { 4 } else { 0 });
        // For named keys with CSI u equivalents, use codepoint of logical char if available
        let codepoint_opt = match &event.logical_key {
            bitty_platform::LogicalKey::Character(s) => s.chars().next().map(|c| c as u32),
            bitty_platform::LogicalKey::Named(named) => match named {
                bitty_platform::NamedKey::Enter => Some(13),
                bitty_platform::NamedKey::Tab => Some(9),
                bitty_platform::NamedKey::Backspace => Some(127),
                bitty_platform::NamedKey::Escape => Some(27),
                _ => None,
            },
            _ => None,
        };
        if let Some(cp) = codepoint_opt {
            // Kitty CSI u: ESC [ <codepoint> ; <mods+1> u  (mods+1 per Kitty spec where 1 = no mods)
            // Event type 1 = press, 2 = repeat, 3 = release (only when requested flag bit 1 set)
            let event_type = if event.repeat { 2 } else { 1 };
            // Only emit Kitty when flag for disambiguation wants it; for slice, always when Kitty active and mods non-zero or named.
            let is_named = matches!(&event.logical_key, bitty_platform::LogicalKey::Named(_));
            if mods != 0 || is_named {
                let seq = format!("\x1b[{cp};{}:{}u", mods + 1, event_type);
                if seq.len() <= 64 {
                    return Some(seq.into_bytes());
                }
            }
        }
        // Fallback to legacy for keys not disambiguated, still honoring the
        // tracked modifier snapshot (CTX-0154: Ctrl+letter synthesis).
        bitty_platform::keyboard::encode_key_event_with_modifiers(event, &self.modifier_snapshot())
    }

    /// Handles a decoded keyboard event: encodes to bytes and routes to the PTY.
    ///
    /// When a live PTY writer exists the bytes are written directly (best
    /// effort; errors are ignored so a transient write failure never panics
    /// the loop). Otherwise the bytes are buffered in a bounded
    /// `pending_input` queue (`MAX_PENDING_INPUT` = 8 KiB) so headless
    /// synthetic tests can observe them via [`Self::drain_pending_input`].
    /// When the buffer would overflow the oldest bytes are dropped and
    /// [`Self::pending_input_dropped`] increments (drop-oldest).
    ///
    /// Returns the encoded bytes when the event produced input, `None`
    /// otherwise (release, synthetic, modifier-only, etc.). Headless
    /// callers may synthesize [`KeyEvent`]s without a window and drive this
    /// path deterministically.
    ///
    /// While an IME composition is active (`ime_preedit.is_some()`) raw key
    /// presses are consumed here: winit already suppresses `KeyboardInput`
    /// during the preedit phase on every backend, and this guard keeps the
    /// single-commit invariant even if a platform quirk delivers both — the
    /// raw Latin key must never insert alongside the eventual
    /// `Ime::Commit` (CTX-0367).
    pub fn handle_key_event(&mut self, event: KeyEvent) -> Option<Vec<u8>> {
        let is_modifier = matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(
                bitty_platform::NamedKey::Shift
                    | bitty_platform::NamedKey::Control
                    | bitty_platform::NamedKey::Alt
                    | bitty_platform::NamedKey::AltGraph
                    | bitty_platform::NamedKey::Super
                    | bitty_platform::NamedKey::Meta
            )
        );
        self.track_modifiers_from_key(&event);
        // CTX-0367 IME composition guard: consume raw presses while a
        // preedit is active. winit suppresses `KeyboardInput` during the
        // preedit phase on every backend; if a platform quirk ever delivers
        // one anyway, inserting it would double-input the composition
        // (preedit/commit already carries the text). Modifier tracking stays
        // above so chord state never desyncs; releases produce no bytes.
        if self.ime_preedit.is_some()
            && event.state == PressState::Pressed
            && !event.is_synthetic
            && !is_modifier
        {
            return None;
        }
        // CTX-0243: any non-modifier press snaps to live (covers Esc-cancel
        // with no bytes and unmapped keys with no encoding; normal typing
        // also snaps via `push_input_bytes` below — idempotent).
        if event.state == PressState::Pressed && !is_modifier {
            self.snap_focused_to_live();
        }
        // CTX-0166: any real non-modifier key press clears the selection
        // highlight (left-click/Esc/typing dismiss). Clearing uses
        // `clear_selection` so `pending_full_redraw` forces the next tick to
        // present without the highlight — never a frame late. Modifier-only
        // and synthetic events never clear; releases never clear.
        if event.state == PressState::Pressed
            && !event.is_synthetic
            && !is_modifier
            && self.selection.is_some()
        {
            self.clear_selection();
        }
        // CTX-0186: Esc while a paste is pending cancels the confirmation
        // dialog. The Esc is consumed (never reaches the PTY) so a dismissal
        // cannot also drive shell/vim state.
        if self.cancel_pending_on_escape(&event) {
            return None;
        }
        // CTX-0159: retain a bounded input trace for screenshots-free probes.
        let pressed = Some(event.state == PressState::Pressed);
        if is_modifier {
            // Modifier-only keys produce no PTY input but still update state.
            self.inspect_ring.push_modifiers(
                self.shift_pressed,
                self.control_pressed,
                self.alt_pressed,
            );
            self.publish_inspect_snapshot();
            return None;
        }
        self.inspect_ring.push_key(
            &key_inspect_label(&event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            pressed,
        );
        let bytes = self.encode_key_with_kitty(&event)?;
        // Bounded encoding already ≤64; push respects MAX_PENDING_INPUT.
        self.push_input_bytes(&bytes);
        self.publish_inspect_snapshot();
        Some(bytes)
    }

    /// Convenience: handles a borrowed [`KeyEvent`] without moving it.
    pub fn handle_key_event_ref(&mut self, event: &KeyEvent) -> Option<Vec<u8>> {
        let is_modifier = matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(
                bitty_platform::NamedKey::Shift
                    | bitty_platform::NamedKey::Control
                    | bitty_platform::NamedKey::Alt
                    | bitty_platform::NamedKey::AltGraph
                    | bitty_platform::NamedKey::Super
                    | bitty_platform::NamedKey::Meta
            )
        );
        self.track_modifiers_from_key(event);
        // CTX-0367 IME composition guard (see the owned path): raw presses
        // are consumed while a preedit is active.
        if self.ime_preedit.is_some()
            && event.state == PressState::Pressed
            && !event.is_synthetic
            && !is_modifier
        {
            return None;
        }
        // CTX-0243: any non-modifier press snaps to live (see owned path).
        if event.state == PressState::Pressed && !is_modifier {
            self.snap_focused_to_live();
        }
        // CTX-0166: any real non-modifier key press clears the selection
        // highlight (see owned path). Additive only; range logic untouched.
        if event.state == PressState::Pressed
            && !event.is_synthetic
            && !is_modifier
            && self.selection.is_some()
        {
            self.clear_selection();
        }
        // CTX-0186: Esc while a paste is pending cancels (see owned path).
        if self.cancel_pending_on_escape(event) {
            return None;
        }
        let pressed = Some(event.state == PressState::Pressed);
        if is_modifier {
            self.inspect_ring.push_modifiers(
                self.shift_pressed,
                self.control_pressed,
                self.alt_pressed,
            );
            self.publish_inspect_snapshot();
            return None;
        }
        self.inspect_ring.push_key(
            &key_inspect_label(event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            pressed,
        );
        let bytes = self.encode_key_with_kitty(event)?;
        self.push_input_bytes(&bytes);
        self.publish_inspect_snapshot();
        Some(bytes)
    }

    /// Snaps the focused view to live (CTX-0243).
    ///
    /// Typing, IME, and paste must return the viewport to the live bottom:
    /// otherwise a scrolled viewport keeps showing history while new input
    /// echoes into the live grid, so the typed text is invisible and the
    /// screen looks frozen (the cursor gate also hides the cursor when
    /// scrolled). Output (`handle_pty_bytes`) deliberately does NOT snap —
    /// reading history while output continues must not yank.
    /// Idempotent: no-op when already live or when no focused leaf exists;
    /// sets `pending_full_redraw` when it actually moved so the live frame
    /// presents even before the echo.
    pub(super) fn snap_focused_to_live(&mut self) {
        let Some(fid) = self.focus.focused() else {
            return;
        };
        let Some(view) = self.layout.find_leaf_mut(fid) else {
            return;
        };
        if view.scroll_offset() != 0 {
            view.scroll_to_live();
            self.pending_full_redraw = true;
        }
    }

    /// Pushes raw input bytes into the pending queue and, when a PTY writer
    /// is live, writes them through.
    pub fn push_input_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // CTX-0243: any input bytes snap the focused viewport to live so
        // the echo lands in the visible window (typing while scrolled must
        // not stay stuck showing history with invisible input).
        self.snap_focused_to_live();
        // CTX-0176: input routes to the focused leaf's own shell only —
        // never broadcast. CTX-0359: this is the single routing path (the
        // former `pane_sessions.is_empty()` shortcut wrote straight to the
        // global writer), so a session-less leaf that does not own the
        // primary can never reach another view's shell.
        self.push_input_bytes_multipane(bytes);
    }

    /// Focused-leaf input routing for split layouts (CTX-0176): the focused
    /// leaf's session writer wins; the shared writer serves only the primary
    /// owner leaf (CTX-0359); with no writer live, bytes fall back to the
    /// bounded headless buffer. Best-effort, never panics.
    pub(super) fn push_input_bytes_multipane(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        // CTX-0243: direct multipane sends must also snap (normally already
        // snapped by `push_input_bytes`; idempotent second snap is a no-op).
        self.snap_focused_to_live();
        match self.focus.focused() {
            Some(focused) => {
                if let Some(sess) = self.pane_sessions.get_mut(&focused) {
                    let _ = sess.writer.write_all(bytes);
                    let _ = sess.writer.flush();
                    return;
                }
                // CTX-0359: only the primary owner leaf may fall back to
                // the runtime-global writer. A session-less non-owner (fresh
                // workspace leaf, ctl split tile, spawn failure) has no
                // shell of its own; typing there must never reach another
                // view's shell (previous workspace primary included), so it
                // buffers headless instead.
                if Some(focused) != self.primary_view {
                    self.buffer_input_headless(bytes);
                    return;
                }
            }
            // No focused view: nothing owns input; never leak to a shell.
            None => {
                self.buffer_input_headless(bytes);
                return;
            }
        }
        if let Some(writer) = self.pty_writer.as_mut() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
            return;
        }
        self.buffer_input_headless(bytes);
    }

    /// Headless / no-writer input path: bounded buffer with drop-oldest.
    /// Shared by the single-pane and multipane routers so the bound
    /// (`MAX_PENDING_INPUT`, truncate-to-tail) stays identical on both.
    pub(super) fn buffer_input_headless(&mut self, bytes: &[u8]) {
        let overflow = self.pending_input.len() + bytes.len() > MAX_PENDING_INPUT;
        if overflow {
            // Make room by dropping oldest bytes.
            let needed = self.pending_input.len() + bytes.len() - MAX_PENDING_INPUT;
            let drop = needed.min(self.pending_input.len());
            self.pending_input.drain(0..drop);
            self.pending_input_dropped += drop as u64;
            // If the incoming chunk itself exceeds capacity, truncate to last
            // MAX_PENDING_INPUT bytes (still bounded).
            if bytes.len() > MAX_PENDING_INPUT {
                let start = bytes.len() - MAX_PENDING_INPUT;
                let dropped_extra = bytes.len() - MAX_PENDING_INPUT;
                self.pending_input_dropped += dropped_extra as u64;
                self.pending_input.extend_from_slice(&bytes[start..]);
                return;
            }
        }
        self.pending_input.extend_from_slice(bytes);
    }

    /// Number of bytes currently buffered for PTY input (headless observation).
    #[must_use]
    pub fn pending_input_len(&self) -> usize {
        self.pending_input.len()
    }

    /// How many input bytes have been dropped due to buffer overflow.
    #[must_use]
    pub fn pending_input_dropped(&self) -> u64 {
        self.pending_input_dropped
    }

    /// Views the pending input buffer without draining (headless helper).
    #[must_use]
    pub fn pending_input(&self) -> &[u8] {
        &self.pending_input
    }

    /// Drains and returns all pending input bytes (headless helper).
    pub fn drain_pending_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_input)
    }

    /// Whether a live PTY writer is currently owned.
    #[must_use]
    pub fn has_pty_writer(&self) -> bool {
        self.pty_writer.is_some()
    }

    /// Takes exclusive ownership of the PTY writer, if present (test helper).
    pub fn take_pty_writer(&mut self) -> Option<PtyWriter> {
        self.pty_writer.take()
    }

    /// Writes raw bytes as terminal input, same routing as keyboard (writer
    /// when live, bounded pending otherwise).
    pub fn write_input(&mut self, bytes: &[u8]) {
        self.push_input_bytes(bytes);
    }

    /// Handles a mouse button event for selection or terminal mouse tracking.
    ///
    /// Single-window vertical slice semantics (candidate Input RFC, ghostty
    /// reference `recording/references/ghostty/src/Surface.zig`):
    /// - When mouse tracking is enabled (`1000`/`1002`/`1003`) + SGR `1006` and
    ///   not in shift-override, every button event encodes to bounded SGR bytes
    ///   (`ESC[<b;x;yM/m`, ≤32 bytes) and is written to the PTY. Right/middle
    ///   paste never fires in this path.
    /// - Holding Shift bypasses capture unconditionally to force selection
    ///   (accessibility escape).
    /// - Otherwise the event drives presentation selection with ghostty
    ///   copy-on-select: left press starts a drag, left release commits it and
    ///   auto-copies to the platform clipboard (which best-effort syncs the
    ///   primary selection on Linux, CTX-0160) — unless
    ///   `RuntimeConfig::selection_auto_copy` is `false` (CTX-0191 opt-out:
    ///   the highlight stays and only the explicit `copy_to_clipboard` chord
    ///   copies); right press pastes the
    ///   standard clipboard (Wayland-first backend) and middle press pastes
    ///   the platform primary selection, both through the suspicious-paste
    ///   inspection gate. All three stay fail-soft (empty source means no
    ///   bytes, never a panic or block) but platform failures are recorded
    ///   for [`Self::last_clipboard_error`] instead of swallowed.
    pub fn handle_mouse_input(&mut self, event: bitty_platform::MouseEvent) {
        // CTX-0159: retain a bounded mouse trace for screenshots-free probes.
        // Coordinates come from the last known cursor position mapped to cell
        // space (clamped); `None` when the cursor never entered the window.
        {
            let cell = self.last_cursor.map(|pos| self.cursor_to_cell(pos));
            let button = match event.button {
                MouseButton::Left => "Left",
                MouseButton::Right => "Right",
                MouseButton::Middle => "Middle",
                MouseButton::Back => "Back",
                MouseButton::Forward => "Forward",
                MouseButton::Other(_) => "Other",
            };
            let pressed = event.state == PressState::Pressed;
            self.inspect_ring.push_mouse(
                button,
                cell.map(|c| c.col),
                cell.map(|c| c.row),
                pressed,
                self.shift_pressed,
                self.control_pressed,
                self.alt_pressed,
            );
            self.publish_inspect_snapshot();
        }
        // Shift override always forces selection path.
        let shift_override = self.shift_pressed;
        let capture = !shift_override
            && self.state.modes().mouse_tracking.is_some()
            && self.state.modes().mouse_coordinate_encoding
                == Some(bitty_vt::MouseCoordinateEncoding::Sgr)
            && self.should_capture_mouse();

        if capture {
            if let Some(pos) = self.last_cursor {
                let cell = self.cursor_to_cell(pos);
                // Bounded SGR encoding: ≤32 bytes per event, batch ≤4 KiB (candidate)
                // Coordinates are 1-based per SGR, clamped to [1,65535] then to grid.
                let col = (cell.col as u32 + 1).clamp(1, 65535) as u16;
                let row = (cell.row as u32 + 1).clamp(1, 65535) as u16;
                let mut code = match event.button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 9,
                    MouseButton::Other(n) => (n % 8) as u8,
                };
                // Modifier bits: shift 4, alt 8, ctrl 16
                if self.shift_pressed {
                    code |= 4;
                }
                if self.alt_pressed {
                    code |= 8;
                }
                if self.control_pressed {
                    code |= 16;
                }
                // Drag adds 32 for button-event tracking? For simplicity we map release vs press.
                let trailer = if event.state == PressState::Pressed {
                    'M'
                } else {
                    'm'
                };
                // For SGR, release is still reported with same button code but 'm'
                // Bounded <32 bytes: format "ESC[<code;col;rowM"
                let seq = format!("\x1b[<{code};{col};{row}{trailer}");
                // Enforce bound explicitly (candidate table: mouse encode ≤32 bytes)
                let bytes = if seq.len() > 32 {
                    &seq.as_bytes()[..32]
                } else {
                    seq.as_bytes()
                };
                self.push_input_bytes(bytes);
            }
            // Still update last_cursor tracking but do not start selection.
            // CTX-0166: a captured click must still dismiss any stale
            // highlight so the gray rect never lingers while a mouse-mode app
            // owns the pointer. Additive clearing only; range logic untouched.
            if self.selection.is_some() {
                self.clear_selection();
            }
            return;
        }

        // CTX-0181: overlay scrollbar chrome sits above selection. A left
        // press on the painted thumb/track starts a drag-to-scroll and
        // consumes the event (Shift still forces the selection path — the
        // accessibility escape wins over chrome too).
        if !shift_override
            && event.button == MouseButton::Left
            && event.state == PressState::Pressed
            && self.scrollbar_press()
        {
            return;
        }
        // A left release always ends a thumb drag; the selection release
        // path below then runs harmlessly (`end_selection` early-returns
        // with no selection, and no selection was started while dragging).
        // CTX-0260: a release also ends an Alt+drag move — but unlike the
        // thumb drag it returns early, skipping the selection commit/copy
        // (the grabbing press never started a selection, so there is
        // nothing to commit and stale highlights must not auto-copy).
        if event.button == MouseButton::Left && event.state == PressState::Released {
            self.scrollbar_release();
            if self.end_alt_drag() {
                return;
            }
        }
        // CTX-0260: Alt+Left-press on a floating overlay grabs it for an
        // Alt+drag move and consumes the event (no selection starts). The
        // grab fails soft on tiled layouts (no movable position) and under
        // Shift (which forces the selection path per the CTX-0181
        // precedent) — both fall through to selection below, so Alt+drag
        // never breaks selection.
        if !shift_override
            && event.button == MouseButton::Left
            && event.state == PressState::Pressed
            && self.alt_pressed
            && self.begin_alt_drag()
        {
            return;
        }

        // Selection path (including shift override)
        match (event.button, event.state) {
            (MouseButton::Left, PressState::Pressed) => {
                if let Some(pos) = self.last_cursor {
                    // CTX-0339: click-to-focus. A left press on a pane moves
                    // keyboard focus to the hit-tested leaf even when
                    // `focus_follows_mouse` is off (the default). Shift is
                    // the accessibility escape (CTX-0181): `click_focus_at`
                    // suppresses focus so Shift+click selects without
                    // stealing focus, coherent with the hover path.
                    self.click_focus_at(pos);
                    let cell = self.cursor_to_cell(pos);
                    self.start_selection(cell);
                } else if self.selection.is_some() {
                    // CTX-0166: click without cursor tracking still dismisses
                    // the highlight (no stale rect when `last_cursor` is None).
                    self.clear_selection();
                }
            }
            (MouseButton::Left, PressState::Released) => {
                if let Some(pos) = self.last_cursor {
                    let cell = self.cursor_to_cell(pos);
                    self.end_selection(cell);
                } else {
                    self.selection_dragging = false;
                    if let Some(mut sel) = self.selection {
                        sel.active = false;
                        self.selection = Some(sel);
                    }
                    self.pending_full_redraw = true;
                }
                // Ghostty copy-on-select: a committed drag auto-copies to
                // both selections via the platform clipboard (Wayland-first,
                // CTX-0160) — unless `selection.auto_copy` is false (CTX-0191
                // opt-out: the highlight stays and only the explicit
                // `copy_to_clipboard` chord copies). Fail-soft: empty
                // selection pastes nothing and a system clipboard error is
                // recorded for `last_clipboard_error` while the headless
                // buffers still update; the input path never blocks.
                if self.config.selection_auto_copy {
                    let _ = self.auto_copy_selection();
                }
            }
            (MouseButton::Right, PressState::Pressed) => {
                // Ghostty `paste` right-click action for the standard
                // clipboard (Wayland-first via the platform backend,
                // CTX-0160). Fail-soft: empty clipboards paste nothing and
                // suspicious text waits on the confirmation gate; a read
                // failure is recorded for `last_clipboard_error` instead of
                // swallowed (PR #259 review).
                match self.paste_from_clipboard() {
                    Ok(_) => self.clear_clipboard_error(),
                    Err(err) => self.record_clipboard_error(err),
                }
            }
            (MouseButton::Middle, PressState::Pressed) => {
                // Ghostty `primary-paste` middle-click action for the
                // platform primary selection (fail-soft like right-click;
                // read failures are recorded inside `paste_from_primary`).
                let _ = self.paste_from_primary();
            }
            _ => {}
        }
    }

    pub(super) fn should_capture_mouse(&self) -> bool {
        // Capture when a mouse mode is enabled; for single-window slice we
        // capture in any view (not only alternate screen) to prove 1000..1006
        // end-to-end, but we document that alternate-screen capture is the
        // normative owner. This keeps headless tests deterministic.
        self.state.modes().mouse_tracking.is_some()
    }

    /// Handles cursor movement for drag selection or mouse-tracking motion.
    pub fn handle_cursor_moved(&mut self, pos: CursorPosition) {
        self.handle_cursor_moved_at(pos, std::time::Instant::now());
    }

    /// [`Self::handle_cursor_moved`] with an explicit wall clock
    /// (CTX-0334 virtual-clock seam for the hover dwell delay).
    pub fn handle_cursor_moved_at(&mut self, pos: CursorPosition, now: std::time::Instant) {
        self.last_cursor = Some(pos);
        // CTX-0181: an active thumb drag consumes motion (a selection drag
        // cannot coexist — the press routed exclusively). Otherwise an
        // `auto` visibility transition repaints exactly once; steady
        // hover costs no present.
        self.scrollbar_cursor_left = false;
        if self.scrollbar_drag_to(pos) {
            self.clear_hover_pending();
            return;
        }
        if self.scrollbar_should_paint() != self.scrollbar_visible {
            self.pending_full_redraw = true;
        }
        if self.selection_dragging {
            self.clear_hover_pending();
            let cell = self.cursor_to_cell(pos);
            self.update_selection(cell);
            return;
        }
        // CTX-0260: an active Alt+drag consumes motion (it moves the
        // grabbed float; selection/hover/capture-motion all stay out).
        if self.update_alt_drag(pos) {
            self.clear_hover_pending();
            return;
        }
        // CTX-0260/CTX-0334: opt-in hover activation — hover moves keyboard
        // focus (gated by the config flag, default off; Shift suppresses it
        // so Shift+hover selection never steals focus). A positive dwell
        // delay arms a timed pending candidate instead of focusing eagerly.
        self.hover_focus_at_at(pos, now);
        // Motion reporting for 1003 (Any) or 1002 drag: encode as motion when capture active.
        let capture = !self.shift_pressed
            && self.state.modes().mouse_tracking == Some(bitty_vt::MouseTrackingMode::Any)
            && self.state.modes().mouse_coordinate_encoding
                == Some(bitty_vt::MouseCoordinateEncoding::Sgr);
        if capture {
            let cell = self.cursor_to_cell(pos);
            let col = (cell.col as u32 + 1).clamp(1, 65535) as u16;
            let row = (cell.row as u32 + 1).clamp(1, 65535) as u16;
            // Motion button code 32 (no button) plus modifiers, SGR uses 'M' for press/drag
            let mut code: u8 = 32;
            if self.shift_pressed {
                code |= 4;
            }
            if self.alt_pressed {
                code |= 8;
            }
            if self.control_pressed {
                code |= 16;
            }
            let seq = format!("\x1b[<{code};{col};{row}M");
            let bytes = if seq.len() > 32 {
                &seq.as_bytes()[..32]
            } else {
                seq.as_bytes()
            };
            // Bounded: drop if PTY queue full, never block
            self.push_input_bytes(bytes);
        }
    }

    /// Handles wheel scroll: accumulates line-notch and pixel deltas and
    /// emits lines or scrolls the viewport, scaled by the configured scroll
    /// speed (`RuntimeConfig::scroll_lines_per_notch` /
    /// `scroll_pixels_per_notch`).
    ///
    /// CTX-0185 profile (vs ghostty on the same machine): the lag was
    /// throughput, not redundant work. The `Lines` path moved exactly 1 line
    /// per notch (`y as isize`) while ghostty-class terminals move 3, and
    /// fractional `Lines` deltas (`|y| < 1.0` from high-resolution wheels)
    /// truncated to zero and were dropped outright. Scheduling was already
    /// coalesced (wheel events only set `pending_full_redraw`; one `tick`
    /// per event-loop pass presents, so an N-event fling costs one full
    /// redraw), and the full redraw itself is required — every viewport cell
    /// changes under scroll. The fix is speed plus a fractional accumulator,
    /// not fewer presents.
    ///
    /// Direction semantics (CTX-0155) are untouched: `y > 0` is up into
    /// history on both paths.
    /// Bounded: at most 32 lines per frame per path; the line-notch
    /// accumulator is clamped to one frame cap and the pixel accumulator to
    /// 4x the notch threshold, so a spinning wheel cannot bank unbounded
    /// drift.
    #[allow(clippy::unnecessary_cast)]
    pub fn handle_wheel(&mut self, delta: ScrollDelta) {
        // CTX-0159: retain a bounded wheel trace for screenshots-free probes.
        match delta {
            ScrollDelta::Lines(x, y) => {
                self.inspect_ring.push_wheel(
                    (x as i32).clamp(-32, 32),
                    (y as i32).clamp(-32, 32),
                    self.shift_pressed,
                    self.control_pressed,
                    self.alt_pressed,
                );
            }
            ScrollDelta::Pixels(px, py) => {
                self.inspect_ring.push_wheel(
                    (px as i32).clamp(-512, 512),
                    (py as i32).clamp(-512, 512),
                    self.shift_pressed,
                    self.control_pressed,
                    self.alt_pressed,
                );
            }
        }
        self.publish_inspect_snapshot();
        // Validated `1..=` at construction; `max(1)` keeps release builds
        // total even if a struct-literal config ever bypasses validation.
        let lines_per_notch = self.config.scroll_lines_per_notch.max(1) as f32;
        let pixels_per_notch = self.config.scroll_pixels_per_notch.max(1) as f32;
        match delta {
            ScrollDelta::Lines(x, y) => {
                // Scale notches into lines and bank the fraction so
                // sub-notch deltas survive across events instead of
                // truncating to zero.
                self.wheel_line_accum_y =
                    (self.wheel_line_accum_y + y * lines_per_notch).clamp(-32.0, 32.0);
                self.wheel_line_accum_x =
                    (self.wheel_line_accum_x + x * lines_per_notch).clamp(-32.0, 32.0);
                let lines_y = self.wheel_line_accum_y.trunc() as isize;
                let lines_x = self.wheel_line_accum_x.trunc() as isize;
                // Shift+wheel or no capture scrolls viewport; otherwise emit mouse wheel SGR when mouse mode active.
                let capture_scroll = !self.shift_pressed
                    && self.state.modes().mouse_tracking.is_some()
                    && self.state.modes().mouse_coordinate_encoding
                        == Some(bitty_vt::MouseCoordinateEncoding::Sgr);
                if capture_scroll {
                    // SGR wheel: buttons 64 (up) / 65 (down), horizontal 66/67
                    for _ in 0..lines_y.abs().min(32) {
                        let btn = if lines_y > 0 { 64 } else { 65 };
                        let seq = if let Some(pos) = self.last_cursor {
                            let cell = self.cursor_to_cell(pos);
                            let col = (cell.col as u32 + 1) as u16;
                            let row = (cell.row as u32 + 1) as u16;
                            format!("\x1b[<{btn};{col};{row}M")
                        } else {
                            format!("\x1b[<{btn};1;1M")
                        };
                        self.push_input_bytes(seq.as_bytes());
                    }
                    for _ in 0..lines_x.unsigned_abs().min(32) {
                        let btn = if lines_x > 0 { 66 } else { 67 };
                        let seq = format!("\x1b[<{btn};1;1M");
                        self.push_input_bytes(seq.as_bytes());
                    }
                    self.wheel_line_accum_y -= lines_y as f32;
                    self.wheel_line_accum_x -= lines_x as f32;
                } else {
                    // Horizontal has no viewport meaning; drain it so no
                    // stale fraction survives into a later capture session.
                    self.wheel_line_accum_x = 0.0;
                    // Viewport scroll
                    if lines_y != 0 {
                        // winit LineDelta y>0 = wheel up; View::scroll_by
                        // positive = up into history, so delta is +lines.
                        let max = self.state.scrollback_len();
                        if let Some(view_id) = self.focused_view() {
                            if let Some(view) = self.layout.find_leaf_mut(view_id) {
                                view.scroll_by(lines_y, max);
                            }
                        } else {
                            // Single-window fallback: find leaf 1
                            if let Some(view) = self.layout.find_leaf_mut(ViewId::new(1)) {
                                view.scroll_by(lines_y, max);
                            }
                        }
                        self.wheel_line_accum_y -= lines_y as f32;
                        self.pending_full_redraw = true;
                    }
                }
            }
            ScrollDelta::Pixels(px, py) => {
                // Accumulate pixel deltas; threshold = configured pixels
                // per notch (default 16 = one default cell height).
                self.wheel_accum_x += px as f32;
                self.wheel_accum_y += py as f32;
                let bound = 4.0 * pixels_per_notch;
                self.wheel_accum_y = self.wheel_accum_y.clamp(-bound, bound);
                self.wheel_accum_x = self.wheel_accum_x.clamp(-bound, bound);
                let notches_y = self.wheel_accum_y / pixels_per_notch;
                let notches_x = self.wheel_accum_x / pixels_per_notch;
                if notches_y.trunc() != 0.0 || notches_x.trunc() != 0.0 {
                    // Use lines path with coalescing; the lines multiplier
                    // applies once, inside the Lines path.
                    let clamped_y = notches_y.clamp(-32.0, 32.0);
                    let clamped_x = notches_x.clamp(-32.0, 32.0);
                    self.handle_wheel(ScrollDelta::Lines(clamped_x, clamped_y));
                    self.wheel_accum_y -= clamped_y * pixels_per_notch;
                    self.wheel_accum_x -= clamped_x * pixels_per_notch;
                }
            }
        }
    }

    /// Scrolls the focused pane by one viewport page (its row count): up
    /// into scrollback history or down toward live (CTX-0178 Alt+U/I,
    /// less-like paging behind the `scroll_page_up`/`scroll_page_down`
    /// chrome actions). Clamped to `[0, scrollback_len]`; bounded to one
    /// leaf mutation plus a redraw flag. Returns false only when no leaf
    /// holds the focused id (empty or stale layout).
    pub fn scroll_focused_page(&mut self, up: bool) -> bool {
        let max = self.state.scrollback_len();
        let target = match self.focused_view() {
            Some(id) => id,
            None => return false,
        };
        let scrolled = match self.layout.find_leaf_mut(target) {
            Some(view) => {
                let page = usize::from(view.rows()).max(1) as isize;
                view.scroll_by(if up { page } else { -page }, max);
                true
            }
            None => false,
        };
        if scrolled {
            self.pending_full_redraw = true;
        }
        scrolled
    }

    /// Tracks modifier state from keyboard events (Shift/Ctrl/Alt).
    pub fn track_modifiers_from_key(&mut self, event: &KeyEvent) {
        // Update shift/ctrl/alt pressed state based on named keys.
        // This keeps hot-path allocation-free (no HashMap) and bounded.
        if let bitty_platform::LogicalKey::Named(named) = &event.logical_key {
            match named {
                bitty_platform::NamedKey::Shift => {
                    self.shift_pressed = event.state == PressState::Pressed;
                }
                bitty_platform::NamedKey::Control => {
                    self.control_pressed = event.state == PressState::Pressed;
                }
                bitty_platform::NamedKey::Alt | bitty_platform::NamedKey::AltGraph => {
                    self.alt_pressed = event.state == PressState::Pressed;
                }
                _ => {}
            }
        }
    }

    /// Sets focus state and emits focus reports when mode 1004 is enabled.
    pub fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        let gained = focused;
        self.focused = focused;
        // CTX-0159: retain focus transitions for screenshots-free probes.
        self.inspect_ring.push_focus(
            focused,
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
        );
        if gained {
            self.pending_full_redraw = true;
        }
        if self.state.modes().focus_events {
            let seq = if focused { "\x1b[I" } else { "\x1b[O" };
            self.push_input_bytes(seq.as_bytes());
        }
        self.publish_inspect_snapshot();
    }

    /// Handles IME preedit (presentation overlay, not Terminal Truth).
    ///
    /// `cursor` is winit's byte-wise preedit cursor offset (the start of its
    /// `(begin, end)` range); the stored [`Self::ime_cursor`] is a
    /// **character index** used to place the composition caret, so a hostile
    /// offset inside a multi-byte scalar can never split a char. Text is
    /// truncated to [`IME_PREEDIT_MAX_CHARS`]
    /// scalars at a char boundary (TXT-10), and an empty preedit clears the
    /// overlay (winit sends one right before [`Self::handle_ime_commit`], per
    /// its `Ime::Commit` contract).
    pub fn handle_ime_preedit(&mut self, text: Option<String>, cursor: Option<usize>) {
        // CTX-0243: preedit is typing — snap to live so the overlay lands on
        // the visible window instead of a scrolled history viewport.
        self.snap_focused_to_live();
        if let Some(t) = text {
            let truncated = truncate_chars(t, IME_PREEDIT_MAX_CHARS);
            let char_len = truncated.chars().count();
            let cur = cursor
                .map(|start| {
                    truncated
                        .char_indices()
                        .take_while(|(byte, _)| *byte < start)
                        .count()
                })
                .unwrap_or(char_len)
                .min(char_len);
            self.ime_preedit = Some(truncated);
            self.ime_cursor = cur;
            self.pending_full_redraw = true;
        } else {
            self.ime_preedit = None;
            self.ime_cursor = 0;
            self.pending_full_redraw = true;
        }
    }

    /// Commits IME text: bounded ≤256 chars / ≤1024 bytes, then encoder path.
    ///
    /// The commit is UTF-8 `text.as_bytes()` pushed through the same bounded
    /// PTY write queue as raw keyboard input (`push_input_bytes`); bracketed
    /// paste framing is deliberately not applied (input-pointer RFC "IME
    /// composition and commit"). The preedit overlay clears first so a cancel
    /// of a stale composition can never paint after the commit.
    #[allow(clippy::explicit_counter_loop)]
    pub fn handle_ime_commit(&mut self, text: String) {
        // Bounded before allocation (TXT-11)
        let bytes_len = text.len();
        let char_count = text.chars().count();
        let bounded = if bytes_len > IME_COMMIT_MAX_BYTES || char_count > IME_COMMIT_MAX_CHARS {
            let mut out = String::new();
            let mut bytes = 0usize;
            let mut chars = 0usize;
            for ch in text.chars() {
                let clen = ch.len_utf8();
                if bytes + clen > IME_COMMIT_MAX_BYTES || chars + 1 > IME_COMMIT_MAX_CHARS {
                    break;
                }
                out.push(ch);
                bytes += clen;
                chars += 1;
            }
            out
        } else {
            text
        };
        self.ime_preedit = None;
        self.ime_cursor = 0;
        // CTX-0243: IME commit is typing — snap to live (also snaps via
        // `push_input_bytes` below; explicit for empty commits with no bytes).
        self.snap_focused_to_live();
        // CTX-0166: IME commit is typing — dismiss the highlight first so the
        // rect never lingers a frame past the state. `clear_selection` forces
        // the next tick to present; the final flag below keeps that promise.
        if self.selection.is_some() {
            self.clear_selection();
        }
        // IME commit shares PTY write queue with keyboard (bounded 8192)
        self.push_input_bytes(bounded.as_bytes());
        self.pending_full_redraw = true;
    }

    /// Current cursor position, if known.
    #[must_use]
    pub fn last_cursor(&self) -> Option<CursorPosition> {
        self.last_cursor
    }
}
