//! Terminal mode register (RFC invariant 5: single authoritative
//! definitions referenced by the mode enum).
//!
//! Each supported [`bitty_vt::Mode`] has exactly one storage slot here.
//! Two documented deferrals, both tied to RFC open items:
//!
//! - `DECCOLM` (`?3`) side effects (screen clear, cursor home, margin
//!   reset) are applied, but the actual column-dimension change awaits the
//!   resize environment input and the singular reflow algorithm that the
//!   Terminal State RFC lists under "Open items remaining under OQ-007".
//! - Mouse tracking and coordinate encoding store `Option<..>` because the
//!   parser's closed `Mode` enum has no explicit "off" variant; disabling
//!   clears the option.

use bitty_vt::{MouseCoordinateEncoding, MouseTrackingMode};

/// The full mode set of one screen context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modes {
    /// `IRM` (ANSI 4): printed characters insert instead of overwrite.
    pub insert: bool,
    /// `LNM` (ANSI 20): linefeed implies carriage return.
    pub line_feed_new_line: bool,
    /// Application keypad (`DECCKPAM`/`DECKPNM`).
    pub application_keypad: bool,
    /// Application cursor keys (`DECCKM`).
    pub application_cursor_keys: bool,
    /// `DECCOLM` request flag; see module docs for the deferred dimension
    /// change.
    pub column_132_requested: bool,
    /// Reverse video (`DECSCNM`); presentation interprets this flag.
    pub reverse_video: bool,
    /// Origin mode (`DECOM`): addressing is relative to the scroll region.
    pub origin: bool,
    /// Automatic wrapping (`DECAWM`); enabled in the default state.
    pub auto_wrap: bool,
    /// Cursor blinking (`ATT610`).
    pub cursor_blinking: bool,
    /// Bracketed paste (`?2004`).
    pub bracketed_paste: bool,
    /// Focus reporting (`?1004`).
    pub focus_events: bool,
    /// Active mouse-tracking protocol level (`None`: off).
    pub mouse_tracking: Option<MouseTrackingMode>,
    /// Extended mouse coordinate encoding (`None`: legacy default).
    pub mouse_coordinate_encoding: Option<MouseCoordinateEncoding>,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            insert: false,
            line_feed_new_line: false,
            application_keypad: false,
            application_cursor_keys: false,
            column_132_requested: false,
            reverse_video: false,
            origin: false,
            auto_wrap: true,
            cursor_blinking: false,
            bracketed_paste: false,
            focus_events: false,
            mouse_tracking: None,
            mouse_coordinate_encoding: None,
        }
    }
}

/// Which alternate-screen entry variant is active, if any.
///
/// RFC invariant 5: entry saves and exit restores the full primary-screen
/// cursor/style/mode set. The variant is recorded so exit handling stays
/// deterministic regardless of which disable sequence arrives first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AltScreen {
    #[default]
    Off,
    Via47,
    Via1049,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_modes_match_power_on() {
        let m = Modes::default();
        assert!(m.auto_wrap, "DECAWM defaults to on per DEC baseline");
        assert!(!m.origin);
        assert!(!m.insert);
        assert_eq!(m.mouse_tracking, None);
        assert_eq!(m.mouse_coordinate_encoding, None);
    }
}
