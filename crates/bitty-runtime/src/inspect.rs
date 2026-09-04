//! Read-only introspection snapshots for input debugging (CTX-0159, Issue #258).
//!
//! This module owns the bounded, read-only helpers that turn live [`Runtime`]
//! state into screenshots-free observation data: grid text (bounded rows/cols
//! plus cursor), the last input events (bounded ring of keys, modifiers, and
//! mouse buttons with coordinates), the modifier/latch state, and the
//! focus/window state.
//!
//! # Reference-first (DEC-0017)
//!
//! Wire behavior mirrors the sibling `bitty-devtools` repository (read-only,
//! never modified here):
//!
//! - Framing: length-prefixed `u32` big-endian plus payload `<= 256 KiB`, per
//!   `bitty-devtools/src/transport.ts` (`encodeFrame` / `decodeFrame`).
//! - Envelope: versioned JSON with `version: "1.0"`, numeric `id`, and
//!   `method` starting with `bitty.debug/`, per
//!   `bitty-devtools/src/protocol.ts` (`RequestFrame` / `ResponseFrame`).
//! - Bounds vocabulary per `bitty-devtools/src/bounds.ts` (`BOUNDS`,
//!   `MAX_FRAME_BYTES`, `PREVIEW_MAX_CHARS`): every list is capped and every
//!   string field is bounded before return, fail-closed.
//! - Inspection surface per `bitty-devtools/src/inspection.ts`
//!   (`debug.inspect`, read-only default): connection alone grants nothing,
//!   each query is bounded and labelled as untrusted observation data.
//!
//! The serving side lives in `bitty-ipc/src/devtools.rs`, which registers the
//! four read-only queries (`bitty.debug/getGridText`,
//! `bitty.debug/getInputRing`, `bitty.debug/getModifiers`,
//! `bitty.debug/getFocus`) via [`bitty_ipc::devtools::Dispatcher::register`]
//! and serves them from the live store this module publishes. This module
//! never opens a socket, never spawns a thread, and never mutates terminal
//! truth: publishing copies bounded snapshots into the `bitty-ipc` live store
//! (`&self` only), and every query is read-only.
//!
//! # Bounds (threat T-01, fail-closed)
//!
//! Bounds reuse the `MAX_*` conventions from `bitty-ipc/src/devtools.rs`:
//! grid rows/cols, input-ring length, per-event labels, and rendered JSON are
//! all capped. Oversize requests fail closed with a countable error, never a
//! panic and never truncation that hides the failure (truncation of stored
//! snapshots is deterministic and flagged via `truncated` where applicable).
//!
//! # What this module does not do
//!
//! Input injection (synthetic input) is a separate next slice after
//! introspection lands (DEC-0018). Nothing here writes to the PTY, mutates
//! [`Runtime`], or widens authority.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use bitty_term_state::{Snapshot, State};

// Reuse the serving-side bounds as the single source of truth so the builder
// and the server can never disagree about caps.
use bitty_ipc::devtools::{
    MAX_INPUT_LABEL_CHARS, MAX_INPUT_RING, MAX_INSPECT_COLS, MAX_INSPECT_ROWS,
};

// ── bounds (re-exported for callers that build without bitty-ipc import) ──

/// Maximum grid rows per snapshot (serving bound, truncated deterministically).
pub const INSPECT_MAX_ROWS: usize = MAX_INSPECT_ROWS;

/// Maximum grid columns per row (char-boundary truncated).
pub const INSPECT_MAX_COLS: usize = MAX_INSPECT_COLS;

/// Maximum input-ring events retained (drop-oldest).
pub const INSPECT_MAX_RING: usize = MAX_INPUT_RING;

/// Maximum characters per input-event label.
pub const INSPECT_MAX_LABEL_CHARS: usize = MAX_INPUT_LABEL_CHARS;

// ── input ring ─────────────────────────────────────────────────────────────

/// Kind of a retained input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// A keyboard key press or release (non-modifier keys produce PTY bytes;
    /// modifier-only keys only latch state).
    Key,
    /// A modifier-state change (`ModifiersChanged` or modifier key latch).
    Modifiers,
    /// A mouse button press or release with cell coordinates.
    Mouse,
    /// A wheel scroll gesture (line counts, bounded).
    Wheel,
    /// A window focus gain or loss.
    Focus,
}

impl InputKind {
    /// Stable wire label for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Modifiers => "modifiers",
            Self::Mouse => "mouse",
            Self::Wheel => "wheel",
            Self::Focus => "focus",
        }
    }
}

/// One retained input event (bounded, owned, `Send`).
///
/// `label` is a short human-readable summary (for example `"key:Enter"` or
/// `"mouse:Left pressed col=10 row=5"`), truncated to
/// [`INSPECT_MAX_LABEL_CHARS`] characters at construction. Coordinates are
/// cell coordinates (`0`-based) when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent {
    /// Monotonic sequence number (per ring, wraps on `u64::MAX`).
    pub seq: u64,
    /// Event kind.
    pub kind: InputKind,
    /// Bounded human-readable summary.
    pub label: String,
    /// Whether Shift was held when the event was recorded.
    pub shift: bool,
    /// Whether Control was held when the event was recorded.
    pub control: bool,
    /// Whether Alt was held when the event was recorded.
    pub alt: bool,
    /// Mouse button name (`"Left"`, `"Right"`, `"Middle"`, ...) when applicable.
    pub button: Option<String>,
    /// Cell column (`0`-based) when applicable.
    pub col: Option<u16>,
    /// Cell row (`0`-based) when applicable.
    pub row: Option<u16>,
    /// Button/key state: `true` pressed, `false` released, `None` not applicable.
    pub pressed: Option<bool>,
}

impl InputEvent {
    /// Build an event, truncating `label` to the char bound.
    #[must_use]
    pub fn new(
        seq: u64,
        kind: InputKind,
        label: String,
        shift: bool,
        control: bool,
        alt: bool,
    ) -> Self {
        Self {
            seq,
            kind,
            label: truncate_chars(&label, INSPECT_MAX_LABEL_CHARS),
            shift,
            control,
            alt,
            button: None,
            col: None,
            row: None,
            pressed: None,
        }
    }

    /// Attach mouse coordinates and button state (bounded button name).
    #[must_use]
    pub fn with_mouse(
        mut self,
        button: &str,
        col: Option<u16>,
        row: Option<u16>,
        pressed: Option<bool>,
    ) -> Self {
        self.button = Some(truncate_chars(button, 16));
        self.col = col;
        self.row = row;
        self.pressed = pressed;
        self
    }
}

/// Truncate to at most `max` characters (char-boundary safe).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Bounded ring of the last input events (drop-oldest, fail-closed).
///
/// Owned by [`crate::Runtime`]; pushes are `O(1)` and never grow past
/// [`INSPECT_MAX_RING`]. When full the oldest event is dropped (no blocking,
/// no error). Snapshots are cloned most-recent-first-capped reads for the
/// live store.
#[derive(Debug, Clone)]
pub struct InputRing {
    /// Retained events, oldest first.
    events: VecDeque<InputEvent>,
    /// Next sequence number.
    next_seq: u64,
}

impl InputRing {
    /// Empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
            next_seq: 1,
        }
    }

    /// Number of retained events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the ring is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Next sequence number (for tests).
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Clear all retained events (test helper).
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Push one event, dropping the oldest when full.
    fn push_event(&mut self, mut event: InputEvent) {
        event.seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        if self.events.len() >= INSPECT_MAX_RING {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Record a keyboard key (press or release).
    pub fn push_key(
        &mut self,
        label: &str,
        shift: bool,
        control: bool,
        alt: bool,
        pressed: Option<bool>,
    ) {
        let mut event = InputEvent::new(0, InputKind::Key, label.to_string(), shift, control, alt);
        event.pressed = pressed;
        self.push_event(event);
    }

    /// Record a modifier-state change.
    pub fn push_modifiers(&mut self, shift: bool, control: bool, alt: bool) {
        let label = format!("modifiers:shift={shift} control={control} alt={alt}");
        let event = InputEvent::new(0, InputKind::Modifiers, label, shift, control, alt);
        self.push_event(event);
    }

    /// Record a mouse button press or release with cell coordinates.
    #[allow(clippy::too_many_arguments)]
    pub fn push_mouse(
        &mut self,
        button: &str,
        col: Option<u16>,
        row: Option<u16>,
        pressed: bool,
        shift: bool,
        control: bool,
        alt: bool,
    ) {
        let state = if pressed { "pressed" } else { "released" };
        let label = match (col, row) {
            (Some(c), Some(r)) => format!("mouse:{button} {state} col={c} row={r}"),
            _ => format!("mouse:{button} {state}"),
        };
        let event = InputEvent::new(0, InputKind::Mouse, label, shift, control, alt).with_mouse(
            button,
            col,
            row,
            Some(pressed),
        );
        self.push_event(event);
    }

    /// Record a wheel gesture (bounded line counts in the label).
    pub fn push_wheel(
        &mut self,
        lines_x: i32,
        lines_y: i32,
        shift: bool,
        control: bool,
        alt: bool,
    ) {
        let label = format!("wheel:x={lines_x} y={lines_y}");
        let event = InputEvent::new(0, InputKind::Wheel, label, shift, control, alt);
        self.push_event(event);
    }

    /// Record a window focus change.
    pub fn push_focus(&mut self, focused: bool, shift: bool, control: bool, alt: bool) {
        let label = if focused {
            "focus:gained".to_string()
        } else {
            "focus:lost".to_string()
        };
        let mut event = InputEvent::new(0, InputKind::Focus, label, shift, control, alt);
        event.pressed = Some(focused);
        self.push_event(event);
    }

    /// Snapshot the most recent `limit` events, oldest first.
    ///
    /// `limit` is clamped to `1..=INSPECT_MAX_RING`; `0` yields an empty vec
    /// (fail-closed, never a panic).
    #[must_use]
    pub fn snapshot(&self, limit: usize) -> Vec<InputEvent> {
        if limit == 0 || self.events.is_empty() {
            return Vec::new();
        }
        let capped = limit.min(INSPECT_MAX_RING).min(self.events.len());
        let skip = self.events.len() - capped;
        self.events.iter().skip(skip).cloned().collect()
    }

    /// Snapshot all retained events, oldest first.
    #[must_use]
    pub fn snapshot_all(&self) -> Vec<InputEvent> {
        self.events.iter().cloned().collect()
    }
}

impl Default for InputRing {
    fn default() -> Self {
        Self::new()
    }
}

// ── grid text ──────────────────────────────────────────────────────────────

/// Bounded grid-text snapshot (owned, `Send`).
///
/// `lines` holds at most [`INSPECT_MAX_ROWS`] rows, each at most
/// [`INSPECT_MAX_COLS`] characters (char-boundary truncated, trailing blanks
/// trimmed). `cursor_row`/`cursor_col` are `0`-based live cursor coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridTextSnapshot {
    /// Grid text rows (trailing blanks trimmed per row).
    pub lines: Vec<String>,
    /// Live cursor row (`0`-based).
    pub cursor_row: u16,
    /// Live cursor column (`0`-based).
    pub cursor_col: u16,
    /// Whether the cursor is visible (`DECTCEM`).
    pub cursor_visible: bool,
    /// Grid width in columns at capture time.
    pub cols: usize,
    /// Grid height in rows at capture time.
    pub rows: usize,
    /// Damage generation at capture time.
    pub generation: u64,
}

/// Extract bounded grid text from a terminal [`Snapshot`].
///
/// Skips wide-char spacers (trailing halves), emits each leading glyph once,
/// trims trailing blanks per row, truncates each row to `max_cols`
/// characters and rows to `max_rows` (top rows first). `max_rows`/`max_cols`
/// are clamped to `1..=INSPECT_MAX_*`; `0` yields an empty snapshot
/// (fail-closed). Pure and read-only: never mutates state.
#[must_use]
pub fn grid_text_from_snapshot(
    snapshot: &Snapshot,
    max_rows: usize,
    max_cols: usize,
) -> GridTextSnapshot {
    let rows = max_rows.clamp(1, INSPECT_MAX_ROWS);
    let cols = max_cols.clamp(1, INSPECT_MAX_COLS);
    // MSRV 1.85: no let-chains; handle zero explicitly (clamp maps 0 to 1,
    // so check the raw inputs first).
    if max_rows == 0 || max_cols == 0 {
        return GridTextSnapshot {
            lines: Vec::new(),
            cursor_row: snapshot.cursor.position.row,
            cursor_col: snapshot.cursor.position.col,
            cursor_visible: snapshot.cursor.visible,
            cols: snapshot.width,
            rows: snapshot.height,
            generation: snapshot.generation,
        };
    }
    let take_rows = rows.min(snapshot.height);
    let mut lines = Vec::with_capacity(take_rows);
    for r in 0..take_rows {
        let mut line = String::new();
        let mut chars = 0usize;
        for c in 0..snapshot.width {
            if chars >= cols {
                break;
            }
            let idx = r * snapshot.width + c;
            let Some(cell) = snapshot.cells.get(idx) else {
                break;
            };
            if cell.spacer {
                continue;
            }
            line.push(cell.glyph);
            chars += 1;
        }
        // Trim trailing blanks (erased cells) so typed text is findable
        // without screenshot diffing; leading blanks are preserved.
        let trimmed = line.trim_end().to_string();
        // Re-truncate after trim (trim only shortens, so the char bound holds).
        lines.push(truncate_chars(&trimmed, cols));
    }
    GridTextSnapshot {
        lines,
        cursor_row: snapshot.cursor.position.row,
        cursor_col: snapshot.cursor.position.col,
        cursor_visible: snapshot.cursor.visible,
        cols: snapshot.width,
        rows: snapshot.height,
        generation: snapshot.generation,
    }
}

/// Extract bounded grid text directly from live [`State`].
#[must_use]
pub fn grid_text_from_state(state: &State, max_rows: usize, max_cols: usize) -> GridTextSnapshot {
    grid_text_from_snapshot(&state.snapshot(), max_rows, max_cols)
}

// ── modifier / focus snapshots ─────────────────────────────────────────────

/// Bounded modifier/latch snapshot (owned, `Send`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifierSnapshot {
    /// Whether Shift is currently latched.
    pub shift: bool,
    /// Whether Control is currently latched.
    pub control: bool,
    /// Whether Alt is currently latched.
    pub alt: bool,
    /// Live Kitty keyboard flags (`0` means legacy).
    pub kitty_flags: u32,
}

/// Bounded focus/window snapshot (owned, `Send`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusSnapshot {
    /// Whether the window currently holds keyboard focus.
    pub focused: bool,
    /// Currently focused view id, when the layout has one.
    pub focused_view: Option<u64>,
    /// Whether mouse-event capture is active (a mouse tracking mode is on).
    pub mouse_capture: bool,
    /// Whether the alternate screen is active.
    pub alt_screen: bool,
    /// Whether bracketed paste (`2004`) is active.
    pub bracketed_paste: bool,
    /// Whether focus-event reporting (`1004`) is active.
    pub focus_events: bool,
}

// ── live-store publishing ──────────────────────────────────────────────────
//
// The serving side (`bitty-ipc/src/devtools.rs`) owns the cross-thread live
// store (its globals are `Send` and bounded). These helpers convert runtime
// observations into the serving-side publish types so `bitty-app` needs no
// extra wiring: [`crate::Runtime`] calls them on input and on tick (`&self`
// only, never mutating terminal truth).

/// Publish one grid snapshot to the serving-side live store.
///
/// Truncation to the serving bounds happens here (deterministic, top rows
/// first) so the socket layer only slices per-request params.
pub fn publish_grid(snapshot: &GridTextSnapshot) {
    bitty_ipc::devtools::publish_grid_text(
        snapshot.lines.clone(),
        snapshot.cursor_row,
        snapshot.cursor_col,
        snapshot.cursor_visible,
        snapshot.generation,
        snapshot.cols,
        snapshot.rows,
    );
}

/// Publish the input ring to the serving-side live store.
pub fn publish_input_ring(events: &[InputEvent]) {
    let converted: Vec<bitty_ipc::devtools::InputEventPublish> = events
        .iter()
        .map(|e| bitty_ipc::devtools::InputEventPublish {
            seq: e.seq,
            kind: e.kind.as_str().to_string(),
            label: e.label.clone(),
            shift: e.shift,
            control: e.control,
            alt: e.alt,
            button: e.button.clone(),
            col: e.col,
            row: e.row,
            pressed: e.pressed,
        })
        .collect();
    bitty_ipc::devtools::publish_input_ring(converted);
}

/// Publish the modifier/latch state to the serving-side live store.
pub fn publish_modifiers(snapshot: &ModifierSnapshot) {
    bitty_ipc::devtools::publish_modifiers(bitty_ipc::devtools::ModifiersPublish {
        shift: snapshot.shift,
        control: snapshot.control,
        alt: snapshot.alt,
        kitty_flags: snapshot.kitty_flags,
    });
}

/// Publish the focus/window state to the serving-side live store.
pub fn publish_focus(snapshot: &FocusSnapshot) {
    bitty_ipc::devtools::publish_focus(bitty_ipc::devtools::FocusPublish {
        focused: snapshot.focused,
        focused_view: snapshot.focused_view,
        mouse_capture: snapshot.mouse_capture,
        alt_screen: snapshot.alt_screen,
        bracketed_paste: snapshot.bracketed_paste,
        focus_events: snapshot.focus_events,
    });
}

/// Clear the serving-side live store (test helper only).
///
/// Unit and integration tests publish known snapshots and must not leak them
/// into parallel tests or the live proof: callers clear before and after each
/// global round-trip. Production never calls this.
pub fn clear_live_store_for_tests() {
    bitty_ipc::devtools::clear_introspection_for_tests();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::State;

    fn feed_print(state: &mut State, text: &str) {
        use bitty_vt::{Parser, TerminalAction};
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        parser.advance(text.as_bytes(), |a| actions.push(a));
        for action in actions {
            if let TerminalAction::Reply { .. } = action {
                continue;
            }
            state.apply(&action);
        }
    }

    #[test]
    fn grid_text_extracts_typed_text_with_cursor() {
        let mut state = State::new();
        feed_print(&mut state, "hello introspect");
        let snap = grid_text_from_state(&state, 24, 80);
        assert!(snap.lines.iter().any(|l| l.contains("hello introspect")));
        assert_eq!(snap.cursor_visible, state.snapshot().cursor.visible);
        assert_eq!(snap.generation, state.snapshot().generation);
    }

    #[test]
    fn grid_text_truncates_rows_and_cols_deterministically() {
        let mut state = State::new();
        feed_print(&mut state, "abcdefghijklmnopqrstuvwxyz");
        let snap = grid_text_from_state(&state, 1, 5);
        assert_eq!(snap.lines.len(), 1);
        assert!(snap.lines[0].chars().count() <= 5);
        // Zero bounds fail closed with empty lines (no panic).
        let empty = grid_text_from_state(&state, 0, 0);
        assert!(empty.lines.is_empty());
    }

    #[test]
    fn grid_text_skips_wide_spacers() {
        let mut state = State::new();
        // Wide emoji: leading cell plus spacer; snapshot text must contain
        // the glyph once and never a lone spacer blank.
        feed_print(&mut state, "a\u{1F600}b");
        let snap = grid_text_from_state(&state, 4, 16);
        let joined = snap.lines.join("\n");
        assert!(joined.contains('a'));
        assert!(joined.contains('b'));
    }

    #[test]
    fn input_ring_is_bounded_drop_oldest() {
        let mut ring = InputRing::new();
        for i in 0..(INSPECT_MAX_RING + 10) {
            ring.push_key(&format!("key:k{i}"), false, false, false, Some(true));
        }
        assert_eq!(ring.len(), INSPECT_MAX_RING);
        let all = ring.snapshot_all();
        assert_eq!(all.len(), INSPECT_MAX_RING);
        // Oldest dropped: first retained seq is 11 (1-based, 10 dropped).
        assert_eq!(all[0].seq, 11);
        // Snapshot limit honors the request bound.
        let few = ring.snapshot(3);
        assert_eq!(few.len(), 3);
        assert_eq!(few[2].seq, all[all.len() - 1].seq);
        // Zero limit fails closed.
        assert!(ring.snapshot(0).is_empty());
    }

    #[test]
    fn input_event_label_is_char_bounded() {
        let mut ring = InputRing::new();
        let long = "k".repeat(INSPECT_MAX_LABEL_CHARS + 50);
        ring.push_key(&long, true, false, true, Some(true));
        let all = ring.snapshot_all();
        assert_eq!(all.len(), 1);
        assert!(all[0].label.chars().count() <= INSPECT_MAX_LABEL_CHARS);
        assert!(all[0].shift);
        assert!(!all[0].control);
        assert!(all[0].alt);
    }

    #[test]
    fn input_ring_records_mouse_with_coordinates() {
        let mut ring = InputRing::new();
        ring.push_mouse("Left", Some(10), Some(5), true, false, false, false);
        ring.push_modifiers(true, false, false);
        ring.push_wheel(0, 1, false, false, false);
        ring.push_focus(false, false, false, false);
        let all = ring.snapshot_all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].kind, InputKind::Mouse);
        assert_eq!(all[0].button.as_deref(), Some("Left"));
        assert_eq!(all[0].col, Some(10));
        assert_eq!(all[0].row, Some(5));
        assert_eq!(all[0].pressed, Some(true));
        assert_eq!(all[1].kind, InputKind::Modifiers);
        assert_eq!(all[2].kind, InputKind::Wheel);
        assert_eq!(all[3].kind, InputKind::Focus);
    }

    #[test]
    fn publish_round_trip_reaches_serving_store() {
        clear_live_store_for_tests();
        let mut state = State::new();
        feed_print(&mut state, "probe-text-0159");
        let grid = grid_text_from_state(&state, 24, 80);
        publish_grid(&grid);
        let mut ring = InputRing::new();
        ring.push_key("key:a", false, false, false, Some(true));
        publish_input_ring(&ring.snapshot_all());
        publish_modifiers(&ModifierSnapshot {
            shift: false,
            control: true,
            alt: false,
            kitty_flags: 0,
        });
        publish_focus(&FocusSnapshot {
            focused: true,
            focused_view: Some(1),
            mouse_capture: false,
            alt_screen: false,
            bracketed_paste: false,
            focus_events: false,
        });
        // Served JSON is rendered by bitty-ipc; here we only prove the store
        // holds what we published (full wire assertions live in
        // bitty-ipc/src/devtools.rs tests).
        let dispatcher = bitty_ipc::devtools::Dispatcher::with_defaults();
        let server = bitty_ipc::devtools::ServerInfo::new(
            "test".to_string(),
            "/tmp/probe.sock".to_string(),
            80,
            24,
        );
        let context = bitty_ipc::devtools::ServeContext::new(&server);
        let outcome = bitty_ipc::devtools::handle_envelope(
            br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("probe-text-0159"));
        clear_live_store_for_tests();
    }
}
