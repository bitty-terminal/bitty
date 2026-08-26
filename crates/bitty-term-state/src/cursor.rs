//! Cursor position and saved-cursor state.
//!
//! RFC "Grid and state invariants", invariant 3 "Cursor integrity": the
//! cursor always addresses a leading cell and never the trailing half of a
//! wide character. The snap rule lives in terminal state; this module holds
//! the data.

use crate::cell::Style;
use crate::charsets::Charsets;

/// Zero-based cursor position on the active grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CursorPosition {
    /// Row within `[0, height)`.
    pub row: u16,
    /// Column within `[0, width)`.
    pub col: u16,
}

/// The live cursor: position plus the pen it prints with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Position on the active screen.
    pub position: CursorPosition,
    /// Current print style (SGR state).
    pub style: Style,
    /// Whether the cursor is rendered (`DECTCEM`).
    pub visible: bool,
    /// Selected cursor rendering shape (`DECSCUSR`).
    pub cursor_style: bitty_vt::CursorStyle,
    /// Deferred-wrap latch: set after printing into the last column while
    /// auto-wrap is enabled; consumed by the next print.
    pub pending_wrap: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            position: CursorPosition::default(),
            style: Style::default(),
            visible: true,
            cursor_style: bitty_vt::CursorStyle::Default,
            pending_wrap: false,
        }
    }
}

/// Snapshot captured by `DECSC` / alt-screen entry.
///
/// Per RFC invariant 5 the save is complete enough that restore reproduces
/// position, pen, wrap latches, origin handling, and charset designation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedCursor {
    pub position: CursorPosition,
    pub pending_wrap: bool,
    pub style: Style,
    pub origin_mode: bool,
    pub auto_wrap: bool,
    pub charsets: Charsets,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cursor_is_home_visible_plain() {
        let c = Cursor::default();
        assert_eq!(c.position, CursorPosition { row: 0, col: 0 });
        assert!(c.visible);
        assert_eq!(c.style, Style::default());
        assert!(!c.pending_wrap);
        assert_eq!(c.cursor_style, bitty_vt::CursorStyle::Default);
    }
}
