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

use bitty_vt::{EnhancedKeyboardSetMode, MouseCoordinateEncoding, MouseTrackingMode};

/// Maximum depth of the Kitty keyboard-protocol push/pop stack (CTX-0575).
///
/// The spec requires terminals to bound the stack against denial-of-service;
/// eight entries matches the reference implementations (kitty, ghostty) and
/// the RFC "bounded parser" rule.
pub const ENHANCED_KEYBOARD_STACK_MAX: usize = 8;

/// Mask of the five defined Kitty keyboard progressive-enhancement bits:
/// disambiguate, report events, report alternates, report all keys, report
/// associated text. Unknown wire bits are ignored.
pub const ENHANCED_KEYBOARD_FLAG_MASK: u32 = 0x1F;

/// Live Kitty keyboard-protocol flag register plus its bounded push/pop stack
/// (CTX-0575).
///
/// `entries` is a stack whose top is the current flag register. `len == 0`
/// means no register exists yet and the live flags are zero, matching
/// [`EnhancedKeyboardSetMode`] assignment semantics: a pop that empties the
/// stack resets all flags (spec "progressive enhancement"), and a later set
/// re-creates the base entry.
///
/// Per-screen independence (the spec's "terminal must maintain separate
/// stacks for the main and alternate screens") is provided by the existing
/// alternate-screen save/restore: entering the alternate screen clones the
/// whole mode register (including this stack) into `primary_save`, so
/// alt-screen negotiation never mutates the main register and exit restores
/// it. This matches how every other mode in this register is handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnhancedKeyboardState {
    entries: [u8; ENHANCED_KEYBOARD_STACK_MAX],
    len: u8,
}

impl Default for EnhancedKeyboardState {
    fn default() -> Self {
        Self {
            entries: [0; ENHANCED_KEYBOARD_STACK_MAX],
            len: 0,
        }
    }
}

impl EnhancedKeyboardState {
    /// Current active flags (`0` when no register exists).
    #[must_use]
    pub fn flags(&self) -> u32 {
        if self.len == 0 {
            0
        } else {
            u32::from(self.entries[self.len as usize - 1]) & ENHANCED_KEYBOARD_FLAG_MASK
        }
    }

    /// Number of entries currently on the stack.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.len as usize
    }

    /// Stack contents, oldest first (top of stack last).
    #[must_use]
    pub fn entries(&self) -> &[u8] {
        &self.entries[..self.len as usize]
    }

    /// Applies one `CSI = flags ; mode u` assignment (five-bit bounded).
    pub fn set(&mut self, flags: u32, mode: EnhancedKeyboardSetMode) {
        let flags = (flags & ENHANCED_KEYBOARD_FLAG_MASK) as u8;
        if self.len == 0 {
            // Re-create the base entry (post-pop or initial assignment).
            self.entries[0] = 0;
            self.len = 1;
        }
        let top = self.len as usize - 1;
        match mode {
            EnhancedKeyboardSetMode::Assign => self.entries[top] = flags,
            EnhancedKeyboardSetMode::Set => self.entries[top] |= flags,
            EnhancedKeyboardSetMode::Reset => self.entries[top] &= !flags,
        }
    }

    /// Pushes `flags` as the new current register; when full, the oldest entry
    /// is evicted (spec), never growing past [`ENHANCED_KEYBOARD_STACK_MAX`].
    pub fn push(&mut self, flags: u32) {
        if self.len as usize == ENHANCED_KEYBOARD_STACK_MAX {
            self.entries.copy_within(1..ENHANCED_KEYBOARD_STACK_MAX, 0);
            self.len -= 1;
        }
        self.entries[self.len as usize] = (flags & ENHANCED_KEYBOARD_FLAG_MASK) as u8;
        self.len += 1;
    }

    /// Pops `n` entries (spec default 1 when the wire omits the count).
    ///
    /// A pop that empties the stack resets all flags; popping an empty stack
    /// is a no-op. Never underflows, and a hostile huge `n` costs one clear
    /// rather than an `n`-iteration loop.
    pub fn pop(&mut self, n: u32) {
        let n = n as usize;
        if n >= self.len as usize {
            self.entries = [0; ENHANCED_KEYBOARD_STACK_MAX];
            self.len = 0;
            return;
        }
        for _ in 0..n {
            self.len -= 1;
            self.entries[self.len as usize] = 0;
        }
    }
}

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
    /// Alternate scroll (`?1007`): wheel events in the alternate screen
    /// translate to cursor up/down keys. Defaults off, matching the xterm
    /// `alternateScroll` resource (the runtime only enters alternate scroll
    /// while the alternate screen is active).
    pub alternate_scroll: bool,
    /// Synchronized updates (`?2026`, CTX-0380): while set, presentation
    /// defers committing frames so an application can redraw atomically.
    /// The runtime bounds the deferral with a timeout so a hung application
    /// cannot stall presentation indefinitely.
    pub synchronized_update: bool,
    /// Kitty keyboard progressive flag register and bounded push/pop stack
    /// (CTX-0575). The live bitmask is [`EnhancedKeyboardState::flags`].
    pub enhanced_keyboard: EnhancedKeyboardState,
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
            alternate_scroll: false,
            synchronized_update: false,
            enhanced_keyboard: EnhancedKeyboardState::default(),
            mouse_tracking: None,
            mouse_coordinate_encoding: None,
        }
    }
}

/// Which alternate-screen entry variant is active, if any.
///
/// RFC invariant 5: entry saves and exit restores the primary-screen
/// mode/charset set. The cursor is only saved/restored by the `?1049` pair
/// (`srm_OPT_ALTBUF_CURSOR`); the legacy `?47` (`srm_ALTBUF`) leaves the
/// cursor in place. The variant is recorded so exit handling stays
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
