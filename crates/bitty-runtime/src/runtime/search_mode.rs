//! `Runtime` — scrollback search overlay UI (CTX-0383, issue #639).
//!
//! Keyboard-first modal search overlay over the bounded CTX-0060/0061
//! seams (`State::search`, `SearchState`). Reuses without reimplementing:
//! - CTX-0385 vocabulary: [`SelectionKind`] tagging, `PersistentSelection`
//!   kind preservation through scrollback/prune, `text()`/`block_text`
//!   dispatch via [`Self::search_apply_selection`];
//! - CTX-0384 modal-machine patterns: total key handling (unknown keys are
//!   consumed no-ops), no PTY leaks while modal, `Esc` exits, mouse
//!   selection plus SGR wheel forwarding suppressed, viewport paging still
//!   scrolls for history viewing.
//!
//! Design (scoped, fail-closed, bounded `O(1)` flag plus `<=256`-byte query
//! and `<=1000` matches owned by `SearchState`):
//! - `search_mode: bool` marks the overlay open. The query lives in the
//!   existing `search_state` (`SearchState::set_search` truncates, caps,
//!   orders oldest-first, wraps navigation deterministically).
//! - While open the runtime consumes all non-modifier key presses (no PTY
//!   bytes, no viewport snap-to-live, no selection clearing outside the
//!   search reveal path). Typing appends to the bounded query, `Backspace`
//!   deletes one char, `Enter` advances (`Shift+Enter` goes back),
//!   `Up`/`PageUp` go back and `Down`/`PageDown` advance; every step
//!   reveals the current match in the focused viewport and syncs the live
//!   selection via the existing `search_apply_selection` seam (history
//!   matches keep a highlight-persistent `PersistentSelection` while the
//!   live selection clears).
//! - `Esc` (or [`Self::exit_search_mode`]) exits and clears the search so
//!   no stale highlight lingers. Entering search exits copy mode and
//!   entering copy mode exits search (modals stay exclusive).
//! - No new dependencies, `forbid(unsafe_code)` via the crate root.

use super::*;
use bitty_term_state::search::{SEARCH_MAX_PATTERN_LEN, SearchOptions};

impl Runtime {
    /// Whether the search overlay is open.
    #[must_use]
    pub fn is_search_mode(&self) -> bool {
        self.search_mode
    }

    /// Status indicator for the chrome layer (`None` when closed).
    ///
    /// - Empty query: `SEARCH: type to find`.
    /// - No matches: `SEARCH: no matches: <pattern>`.
    /// - Matches: `SEARCH <current+1>/<count>: <pattern>` (1-based).
    #[must_use]
    pub fn search_mode_label(&self) -> Option<String> {
        if !self.search_mode {
            return None;
        }
        let pattern = self.search_state.pattern();
        if pattern.is_empty() {
            return Some("SEARCH: type to find".to_string());
        }
        let count = self.search_state.match_count();
        if count == 0 {
            return Some(format!("SEARCH: no matches: {pattern}"));
        }
        let cur = self
            .search_state
            .current_index()
            .map(|i| i + 1)
            .unwrap_or(0);
        Some(format!("SEARCH {cur}/{count}: {pattern}"))
    }

    /// Opens the search overlay (fail-closed: no-op when already open).
    ///
    /// Exits copy mode first so modals stay exclusive, clears any mouse
    /// selection so the search highlight owns the selection path, clears
    /// the query so the overlay starts empty, and requests a redraw. No
    /// PTY bytes are produced.
    pub fn enter_search_mode(&mut self) {
        if self.search_mode {
            return;
        }
        if self.copy_mode.is_some() {
            self.exit_copy_mode();
        }
        self.clear_selection();
        self.search_state.clear();
        self.search_mode = true;
        self.pending_full_redraw = true;
    }

    /// Closes the overlay and clears the search (fail-closed no-op when
    /// already closed).
    ///
    /// Clears the query, matches, live selection, and the open flag so no
    /// stale highlight lingers. Plain exit (`Esc`) copies nothing.
    pub fn exit_search_mode(&mut self) {
        if !self.search_mode {
            return;
        }
        self.search_mode = false;
        self.search_state.clear();
        self.selection = None;
        self.selection_dragging = false;
        self.selection_anchor_press = None;
        self.pending_full_redraw = true;
    }

    /// Advances to the next match, reveals it, and syncs the live
    /// selection. Fail-closed no-op when the overlay is closed or empty.
    pub fn search_goto_next(&mut self) {
        if !self.search_mode || self.search_state.match_count() == 0 {
            return;
        }
        self.search_state.next();
        self.search_reveal_current();
        let _ = self.search_apply_selection();
        self.pending_full_redraw = true;
    }

    /// Goes back to the previous match, reveals it, and syncs the live
    /// selection. Fail-closed no-op when the overlay is closed or empty.
    pub fn search_goto_prev(&mut self) {
        if !self.search_mode || self.search_state.match_count() == 0 {
            return;
        }
        self.search_state.prev();
        self.search_reveal_current();
        let _ = self.search_apply_selection();
        self.pending_full_redraw = true;
    }

    /// Reveals the current match in the focused viewport.
    ///
    /// Scrolls the focused view minimally so the current match becomes
    /// visible (no-op when closed, empty, or already visible). Returns
    /// `true` when the viewport moved.
    pub fn search_reveal_current(&mut self) -> bool {
        if !self.search_mode || self.search_state.current_match().is_none() {
            return false;
        }
        let Some(focused) = self.focused_view() else {
            return false;
        };
        // Disjoint field borrows in a tight scope: `layout` mutably plus
        // `search_state`/`state` immutably (avoids a whole-`self` borrow
        // while the view is live).
        let changed = {
            let (search_state, state, layout) = (&self.search_state, &self.state, &mut self.layout);
            let Some(view) = layout.find_leaf_mut(focused) else {
                return false;
            };
            search_state.scroll_to_current(view, state)
        };
        if changed {
            self.pending_full_redraw = true;
        }
        changed
    }

    /// Appends one char to the overlay query (bounded, char-boundary safe).
    ///
    /// Fail-closed no-op when closed or when the query already fills
    /// [`SEARCH_MAX_PATTERN_LEN`] bytes. Re-runs the bounded search,
    /// reveals the first match, and syncs the live selection.
    pub fn search_push_query_char(&mut self, ch: char) {
        if !self.search_mode {
            return;
        }
        let mut pattern = self.search_state.pattern().to_string();
        if pattern.len() + ch.len_utf8() > SEARCH_MAX_PATTERN_LEN {
            return;
        }
        pattern.push(ch);
        let opts = self.search_state.options();
        self.search_state.set_search(&self.state, &pattern, opts);
        self.search_reveal_current();
        let _ = self.search_apply_selection();
        self.pending_full_redraw = true;
    }

    /// Deletes the last char of the overlay query (char-boundary safe).
    ///
    /// Fail-closed no-op when closed or already empty. Re-runs the bounded
    /// search and syncs the live selection (empty query clears matches and
    /// the live highlight but keeps the overlay open).
    pub fn search_backspace(&mut self) {
        if !self.search_mode {
            return;
        }
        let pattern = self.search_state.pattern().to_string();
        if pattern.is_empty() {
            return;
        }
        // Pop one char at a boundary.
        let mut next = pattern;
        next.pop();
        let opts = self.search_state.options();
        self.search_state.set_search(&self.state, &next, opts);
        // Empty query: clear the live highlight but stay open.
        if next.is_empty() {
            self.selection = None;
            self.selection_dragging = false;
        } else {
            self.search_reveal_current();
            let _ = self.search_apply_selection();
        }
        self.pending_full_redraw = true;
    }

    /// Sets the overlay query directly (bounded via `SearchState`).
    ///
    /// Test and chrome-action helper: no-op when the overlay is closed so
    /// background code can never arm a hidden search. Uses
    /// [`SearchOptions::default`] parity with the headless seams (wrapping,
    /// oldest-first, capped). Reveals the first match and syncs selection.
    pub fn search_set_overlay_query(&mut self, pattern: &str) {
        if !self.search_mode {
            return;
        }
        let opts = SearchOptions::default();
        self.search_state.set_search(&self.state, pattern, opts);
        self.search_reveal_current();
        let _ = self.search_apply_selection();
        self.pending_full_redraw = true;
    }

    /// Handles one key event while the search overlay is open.
    ///
    /// Returns `true` when the key was consumed (the caller must not
    /// forward to the PTY). Total over all inputs: unknown keys are
    /// consumed as no-ops so typing can never leak into the shell while
    /// modal.
    pub(super) fn handle_search_mode_key(&mut self, event: &KeyEvent) -> bool {
        use bitty_platform::{LogicalKey, NamedKey, PressState};
        if !self.search_mode {
            return false;
        }
        // Releases and synthetic events produce no PTY bytes anyway; consume
        // them so the overlay stays modal until an explicit exit.
        if event.state != PressState::Pressed || event.is_synthetic {
            return true;
        }
        // Modifier-only keys never edit the query; consume without effect
        // (modifier tracking already ran in the caller).
        if matches!(
            &event.logical_key,
            LogicalKey::Named(
                NamedKey::Shift
                    | NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::AltGraph
                    | NamedKey::Super
                    | NamedKey::Meta
            )
        ) {
            return true;
        }
        match &event.logical_key {
            LogicalKey::Named(NamedKey::Escape) => {
                self.exit_search_mode();
                true
            }
            LogicalKey::Named(NamedKey::Enter) => {
                // Shift+Enter goes back (less-like reverse); plain Enter
                // advances with wrap.
                if self.shift_pressed {
                    self.search_goto_prev();
                } else {
                    self.search_goto_next();
                }
                true
            }
            LogicalKey::Named(named) => {
                self.handle_search_mode_named(*named);
                true
            }
            LogicalKey::Character(text) => {
                self.handle_search_mode_char(text);
                true
            }
            LogicalKey::Dead(_) | LogicalKey::Unidentified => true,
        }
    }

    /// Named-key dispatch for the search overlay.
    fn handle_search_mode_named(&mut self, named: bitty_platform::NamedKey) {
        use bitty_platform::NamedKey;
        // Alt-held named keys are chrome-owned (or unbound chrome
        // candidates): consume without effect so `Alt+F` never edits the
        // query while modal (the app captures bound Alt chords first; this
        // guards headless/direct runtime callers).
        if self.alt_pressed {
            return;
        }
        match named {
            NamedKey::Backspace => self.search_backspace(),
            NamedKey::ArrowUp | NamedKey::PageUp => self.search_goto_prev(),
            NamedKey::ArrowDown | NamedKey::PageDown => self.search_goto_next(),
            // Query editing is char-based with no caret: arrows/home/end
            // otherwise stay consumed no-ops so the overlay stays total.
            NamedKey::ArrowLeft
            | NamedKey::ArrowRight
            | NamedKey::Home
            | NamedKey::End
            | NamedKey::Delete
            | NamedKey::Insert
            | NamedKey::Tab => {}
            _ => {}
        }
    }

    /// Character-key dispatch for the search overlay.
    ///
    /// Single printable chars (no Ctrl/Alt) append to the bounded query.
    /// `Ctrl`-held chords are consumed without effect so shell control
    /// bytes (SIGINT, etc.) never leak while modal. `Alt`-held chords are
    /// chrome-owned and consumed without effect. Multi-char compositions
    /// (IME shape) are no-ops while modal.
    fn handle_search_mode_char(&mut self, text: &str) {
        // Alt-held chords are chrome-owned: consume without editing.
        if self.alt_pressed {
            return;
        }
        // Any Ctrl-held chord is consumed without effect (no control bytes
        // to the PTY while modal).
        if self.control_pressed {
            return;
        }
        let mut chars = text.chars();
        let Some(first) = chars.next() else {
            return;
        };
        if chars.next().is_some() {
            // Multi-char composition (IME shape): no-op while modal.
            return;
        }
        // Control pictures (`\x00..\x1f`, DEL) never enter the query.
        if first.is_control() {
            return;
        }
        self.search_push_query_char(first);
    }
}
