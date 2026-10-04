//! `Runtime` — search-modal containment (CTX-0937, W-144 policy retirement).
//!
//! The scrollback search overlay UI policy retired to the search plugin
//! (CTX-0003, blocked on W-139/W-138): overlay lifecycle, status label,
//! query editing, case toggle, and the total modal keymap. Core keeps only
//! the mechanism: bounded matching (`State::search`), `SearchState`, the
//! query-lifecycle plus navigation host ops (`runtime::search_host`), and
//! the clipboard permission gate.
//!
//! Parked until W-01 input capture lands (owner: W-01): the `search_mode`
//! flag, its transient readout, the release hook ([`Runtime::exit_search_mode`],
//! kept so the `selection` lifecycle funnels still release the binding), and
//! modal containment in [`Runtime::handle_search_mode_key`] (consume-all
//! while open, so no keystroke can leak to the PTY from a half-moved modal).
//! The overlay can no longer open (entry retired with the policy), so the
//! containment is unreachable in practice and `input.rs` modal routing stays
//! byte-identical. No new dependencies, `forbid(unsafe_code)` via the crate
//! root.

use super::*;

impl Runtime {
    /// Whether the search overlay is open.
    ///
    /// W-144 parked readout (owner: W-01): the `chrome_keys` modal guards
    /// branch on this until input capture lands. The overlay can no longer
    /// open, so this reads `false` in practice.
    #[must_use]
    pub fn is_search_mode(&self) -> bool {
        self.search_mode
    }

    /// Releases the search binding when its View loses its grid.
    ///
    /// W-144 parked release hook (owner: W-01): the `selection` lifecycle
    /// funnels call this so a dead binding still tears down flag, query,
    /// live selection, and redraw exactly as before. Plugin-owned teardown
    /// arrives with W-01 capture release.
    pub fn exit_search_mode(&mut self) {
        if !self.search_mode {
            return;
        }
        self.search_mode = false;
        self.search_clear();
        self.drop_selection();
        self.pending_full_redraw = true;
    }

    /// Modal containment while the search overlay is open.
    ///
    /// W-144 parked capture-dispatch (owner: W-01): consumes every key while
    /// open so no keystroke can leak to the PTY from a half-moved modal. The
    /// overlay keymap itself (query editing, `Enter` navigation, `Ctrl+T`
    /// case toggle, `Esc` exit) moved to the search plugin; `input.rs`
    /// routing calls this unchanged until capture lands.
    pub(super) fn handle_search_mode_key(&mut self, event: &KeyEvent) -> bool {
        if !self.search_mode {
            return false;
        }
        let _ = event;
        true
    }
}
