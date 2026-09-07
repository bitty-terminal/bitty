//! `Runtime` — Scrollback search over persistent selections.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::*;

impl Runtime {
    /// Searches scrollback and live grid for `pattern`.
    ///
    /// Bounded by [`bitty_term_state::search::SEARCH_MAX_PATTERN_LEN`] and
    /// [`bitty_term_state::search::SEARCH_MAX_RESULTS`]; headless and deterministic;
    /// no I/O. Delegates to [`State::search`].
    #[must_use]
    pub fn search(&self, pattern: &str, options: SearchOptions) -> Vec<SearchMatch> {
        self.state.search(pattern, options)
    }

    /// Convenience: case-sensitive search with default limits.
    #[must_use]
    pub fn search_case_sensitive(&self, pattern: &str) -> Vec<SearchMatch> {
        self.search(pattern, SearchOptions::default())
    }

    /// Lifts the current live-grid selection to a buffer-anchored persistent
    /// selection, if any. The returned value survives scroll (lines moving
    /// from grid into scrollback), `View` scroll offset changes, and resize
    /// (clamped). Returns `None` when no selection exists.
    #[must_use]
    pub fn persistent_selection(&self) -> Option<PersistentSelection> {
        let sel = self.selection?;
        Some(PersistentSelection::from_grid_selection(sel, &self.state))
    }

    /// Attempts to restore a persistent selection into the live-grid selection.
    ///
    /// Returns `true` when the persistent buffer rows still map into the
    /// current live grid window (and survive pruning); `false` when the
    /// selection has moved into history or been pruned. On `false` the live
    /// selection is cleared to keep invariants (empty pruned selections never
    /// linger as stale grid coords). Headless and bounded.
    pub fn restore_persistent_selection(&mut self, pers: PersistentSelection) -> bool {
        if let Some(sel) = pers.to_grid_selection(&self.state) {
            self.selection = Some(sel);
            self.selection_dragging = sel.active;
            true
        } else {
            // Buffer is either pruned or now in history: clear live selection.
            // Caller may still use `pers.text(&state)` for history highlight.
            self.selection = None;
            self.selection_dragging = false;
            false
        }
    }

    /// Returns the buffer text for a persistent selection, if still valid.
    ///
    /// This reads from scrollback + live grid according to the persistent
    /// buffer rows, so a selection that has scrolled into history still yields
    /// its original text (unless pruned). Headless.
    #[must_use]
    pub fn persistent_selection_text(&self, pers: &PersistentSelection) -> Option<String> {
        pers.text(&self.state)
    }

    /// Whether a persistent selection is still valid against the current state
    /// (not pruned, buffer rows in bounds).
    #[must_use]
    pub fn is_persistent_selection_valid(&self, pers: &PersistentSelection) -> bool {
        pers.is_valid(&self.state)
    }

    /// View-aware persistence: lifts a viewport `Selection` (viewport rows) to
    /// a persistent selection anchored to the combined buffer (respects `View`
    /// scroll offset). Headless.
    #[must_use]
    pub fn persistent_selection_from_view(
        &self,
        sel: Selection,
        view: &View,
    ) -> PersistentSelection {
        PersistentSelection::from_view_selection(sel, view, &self.state)
    }

    /// View-aware restore: attempts to map a persistent selection back into a
    /// viewport `Selection` for the given `View`. Returns `None` when the
    /// selection is outside the current viewport window or pruned.
    #[must_use]
    pub fn persistent_to_view_selection(
        &self,
        pers: &PersistentSelection,
        view: &View,
    ) -> Option<Selection> {
        pers.to_view_selection(view, &self.state)
    }

    /// Owned search UI state (read-only).
    ///
    /// `SearchState` owns the bounded query (`≤256` bytes), options, bounded
    /// matches (`≤1000`), and the current navigation index. All operations are
    /// headless, bounded, and deterministic: `search_set` truncates the
    /// pattern, `State::search` caps results, navigation wraps, and view
    /// highlight mapping is pure arithmetic.
    #[must_use]
    pub fn search_state(&self) -> &SearchState {
        &self.search_state
    }

    /// Owned search UI state (mutable, for tests).
    #[must_use]
    pub fn search_state_mut(&mut self) -> &mut SearchState {
        &mut self.search_state
    }

    /// Sets the search query and recomputes bounded matches against the live state.
    ///
    /// Bounded by [`bitty_term_state::search::SEARCH_MAX_PATTERN_LEN`] and
    /// [`bitty_term_state::search::SEARCH_MAX_RESULTS`]; headless and deterministic.
    /// The UI becomes active iff the truncated pattern is non-empty and
    /// `options.max_results != 0`. When matches are non-empty `current` is set to
    /// `Some(0)`, otherwise cleared. Does not touch `selection` automatically;
    /// call [`Self::search_apply_selection`] to move the live selection to the
    /// current match when desired (selection-persistence integration).
    pub fn search_set(&mut self, pattern: &str, options: SearchOptions) {
        self.search_state.set_search(&self.state, pattern, options);
    }

    /// Clears the search UI (pattern empty, matches cleared, inactive).
    pub fn search_clear(&mut self) {
        self.search_state.clear();
    }

    /// Refreshes the current search against the live state after scrollback
    /// growth, resize, or new input. Preserves `current` clamped to the new
    /// match count (or `None` when empty). No-op when search is inactive.
    pub fn search_refresh(&mut self) {
        self.search_state.refresh(&self.state);
    }

    /// Advances to the next match (wraps deterministically).
    pub fn search_next(&mut self) {
        self.search_state.next();
    }

    /// Advances to the previous match (wraps deterministically).
    pub fn search_prev(&mut self) {
        self.search_state.prev();
    }

    /// Advances the search by `delta` with wrapping.
    pub fn search_advance(&mut self, delta: isize) {
        self.search_state.advance(delta);
    }

    /// Number of matches for the current query (≤ [`bitty_term_state::search::SEARCH_MAX_RESULTS`]).
    #[must_use]
    pub fn search_match_count(&self) -> usize {
        self.search_state.match_count()
    }

    /// Whether the search UI is active (non-empty pattern and max_results > 0).
    #[must_use]
    pub fn search_is_active(&self) -> bool {
        self.search_state.is_active()
    }

    /// Current query pattern (truncated to `SEARCH_MAX_PATTERN_LEN`).
    #[must_use]
    pub fn search_pattern(&self) -> &str {
        self.search_state.pattern()
    }

    /// Current search options (clamped).
    #[must_use]
    pub fn search_options(&self) -> SearchOptions {
        self.search_state.options()
    }

    /// Current match index, if any.
    #[must_use]
    pub fn search_current_index(&self) -> Option<usize> {
        self.search_state.current_index()
    }

    /// Current match, if any.
    #[must_use]
    pub fn search_current_match(&self) -> Option<&SearchMatch> {
        self.search_state.current_match()
    }

    /// Bounded matches for the current query (ordered oldest-scrollback-first).
    #[must_use]
    pub fn search_matches(&self) -> &[SearchMatch] {
        self.search_state.matches()
    }

    /// Persistent selection that exactly spans the current match, if any.
    ///
    /// This is the selection-persistence integration point: the returned
    /// `PersistentSelection` survives scroll (including history) and resize
    /// (clamped), and its `is_valid` tracks pruning. When the match is in the
    /// live grid `to_grid_selection` succeeds; when in history the text is
    /// still readable via `pers.text(&state)`.
    #[must_use]
    pub fn search_current_persistent_selection(&self) -> Option<PersistentSelection> {
        self.search_state.current_persistent_selection(&self.state)
    }

    /// Persistent selection for match `idx`, if in bounds and still valid.
    #[must_use]
    pub fn search_match_persistent_selection(&self, idx: usize) -> Option<PersistentSelection> {
        self.search_state
            .match_persistent_selection(&self.state, idx)
    }

    /// All current matches as bounded persistent selections (≤ `SEARCH_MAX_RESULTS`).
    ///
    /// Each entry is `is_valid`-filtered, so pruned history matches are dropped
    /// deterministically.
    #[must_use]
    pub fn search_all_persistent_selections(&self) -> Vec<PersistentSelection> {
        self.search_state.all_persistent_selections(&self.state)
    }

    /// Indices of matches whose `buffer_row` is currently visible in `view`.
    #[must_use]
    pub fn search_visible_match_indices(&self, view: &View) -> Vec<usize> {
        self.search_state.visible_match_indices(view, &self.state)
    }

    /// Highlights for matches currently visible in `view`, with view-local
    /// coordinates and `is_current` flag.
    ///
    /// Headless helper for the renderer: maps each visible `SearchMatch` to its
    /// `view_row`, `view_col_start..view_col_end`, and whether it is the
    /// current navigated target.
    #[must_use]
    pub fn search_visible_highlights(&self, view: &View) -> Vec<SearchHighlight> {
        self.search_state.visible_highlights(view, &self.state)
    }

    /// Scrolls `view` vertically (and horizontally when needed) to bring the
    /// current match into the viewport. Returns `true` when the view's
    /// `scroll_offset` or `col_offset` changed.
    ///
    /// Deterministic and bounded: the target offset is the minimal adjustment
    /// that makes `current.buffer_row` visible. No-op when no current match
    /// or already visible.
    pub fn search_scroll_view_to_current(&self, view: &mut View) -> bool {
        self.search_state.scroll_to_current(view, &self.state)
    }

    /// Moves the live `selection` to exactly cover the current search match,
    /// if the match is currently in the live grid window; otherwise clears the
    /// live selection while keeping the search highlight (history matches are
    /// not live-selectable but remain highlight-persistent via
    /// `search_current_persistent_selection`).
    ///
    /// Returns `true` when the live selection was set to the match; `false`
    /// when the match is in history or pruned (live selection cleared).
    /// Headless: `selection_text` will then equal `matched_text` for live matches.
    pub fn search_apply_selection(&mut self) -> bool {
        let Some(pers) = self.search_state.current_persistent_selection(&self.state) else {
            return false;
        };
        // Try to restore as live-grid selection.
        if let Some(sel) = pers.to_grid_selection(&self.state) {
            self.selection = Some(sel);
            self.selection_dragging = sel.active;
            true
        } else {
            // In history or pruned: leave a history highlight but clear live selection.
            self.selection = None;
            self.selection_dragging = false;
            false
        }
    }
}
