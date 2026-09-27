//! `Runtime` — Scrollback search over persistent selections.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
//!
//! CTX-0805 (#1478): search is View-bound. A search starts on the focused
//! View (`search_view`), its matches are buffer rows of that View's own grid,
//! and every refresh, reveal, highlight, and live selection it drives
//! addresses that grid, never the primary grid by assumption. The
//! persistent-selection API addresses the same keyboard View
//! ([`Runtime::keyboard_view`]).
use super::selection::grid_of;
use super::*;

impl Runtime {
    /// Grid the current search's matches belong to (CTX-0805).
    ///
    /// The bound View's live grid; an unbound search (only reachable through
    /// the `search_state_mut` test seam) reads the keyboard View's grid.
    /// `None` when the bound View no longer resolves to a live grid.
    fn search_grid(&self) -> Option<&State> {
        match self.search_view {
            Some(view) => self.live_view_state(view),
            None => self.keyboard_grid(),
        }
    }

    /// Whether `view` is the View the current search addresses.
    fn search_targets(&self, view: ViewId) -> bool {
        self.search_view.or_else(|| self.keyboard_view()) == Some(view)
    }

    /// Recomputes matches for `pattern` over the bound grid without
    /// rebinding (CTX-0805). A bound View that lost its grid ends the
    /// search (fail closed).
    pub(super) fn search_set_bound(&mut self, pattern: &str, options: SearchOptions) {
        let grid = self
            .search_view
            .filter(|view| self.layout.find_leaf(*view).is_some())
            .and_then(|view| grid_of(&self.pane_sessions, self.primary_view, &self.state, view));
        match grid {
            Some(grid) => self.search_state.set_search(grid, pattern, options),
            None => self.search_clear(),
        }
    }

    /// Searches scrollback and live grid for `pattern`.
    ///
    /// Bounded by [`bitty_term_state::search::SEARCH_MAX_PATTERN_LEN`] and
    /// [`bitty_term_state::search::SEARCH_MAX_RESULTS`]; headless and deterministic;
    /// no I/O. Delegates to [`State::search`] on the keyboard View's grid
    /// (CTX-0805); a keyboard View without a grid yields no matches.
    #[must_use]
    pub fn search(&self, pattern: &str, options: SearchOptions) -> Vec<SearchMatch> {
        self.keyboard_grid()
            .map_or_else(Vec::new, |grid| grid.search(pattern, options))
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
    ///
    /// CTX-0805: `PersistentSelection` carries buffer rows, not a View, so
    /// it is expressed against the keyboard View's grid (the grid
    /// [`Self::restore_persistent_selection`] restores into). A selection
    /// owned by another View reports `None` instead of being re-expressed
    /// against the wrong grid.
    #[must_use]
    pub fn persistent_selection(&self) -> Option<PersistentSelection> {
        let (sel, state) = self.selection_owner_state()?;
        if Some(sel.owner) != self.keyboard_view() {
            return None;
        }
        Some(PersistentSelection::from_grid_selection(
            sel.selection,
            state,
        ))
    }

    /// Attempts to restore a persistent selection into the live-grid selection.
    ///
    /// Returns `true` when the persistent buffer rows still map into the
    /// current live grid window (and survive pruning); `false` when the
    /// selection has moved into history or been pruned. On `false` the live
    /// selection is cleared to keep invariants (empty pruned selections never
    /// linger as stale grid coords). Headless and bounded.
    ///
    /// CTX-0805: restores into the keyboard View's grid and installs the
    /// selection owned by that View.
    pub fn restore_persistent_selection(&mut self, pers: PersistentSelection) -> bool {
        let restored = self.keyboard_view().and_then(|view| {
            self.live_view_state(view)
                .and_then(|grid| pers.to_grid_selection(grid))
                .map(|sel| (view, sel))
        });
        if let Some((view, sel)) = restored {
            let pin = if sel.active { Some(sel.anchor) } else { None };
            self.install_selection(view, sel, pin, sel.active);
            true
        } else {
            // Buffer is either pruned or now in history: clear live selection.
            // Caller may still use `pers.text(&state)` for history highlight.
            self.drop_selection();
            false
        }
    }

    /// Returns the buffer text for a persistent selection, if still valid.
    ///
    /// This reads from scrollback + live grid according to the persistent
    /// buffer rows, so a selection that has scrolled into history still yields
    /// its original text (unless pruned). Headless. Reads the keyboard View's
    /// grid (CTX-0805).
    #[must_use]
    pub fn persistent_selection_text(&self, pers: &PersistentSelection) -> Option<String> {
        pers.text(self.keyboard_grid()?)
    }

    /// Whether a persistent selection is still valid against the current state
    /// (not pruned, buffer rows in bounds) of the keyboard View's grid.
    #[must_use]
    pub fn is_persistent_selection_valid(&self, pers: &PersistentSelection) -> bool {
        self.keyboard_grid().is_some_and(|grid| pers.is_valid(grid))
    }

    /// View-aware persistence: lifts a viewport `Selection` (viewport rows) to
    /// a persistent selection anchored to the combined buffer (respects `View`
    /// scroll offset). Headless.
    ///
    /// CTX-0805: anchored to `view`'s own grid. A View that owns no grid
    /// falls back to the primary grid only to keep this historic infallible
    /// signature; such a View presents no content to select.
    #[must_use]
    pub fn persistent_selection_from_view(
        &self,
        sel: Selection,
        view: &View,
    ) -> PersistentSelection {
        let grid = self.session_state_for(view.id()).unwrap_or(&self.state);
        PersistentSelection::from_view_selection(sel, view, grid)
    }

    /// View-aware restore: attempts to map a persistent selection back into a
    /// viewport `Selection` for the given `View`. Returns `None` when the
    /// selection is outside the current viewport window, pruned, or `view`
    /// owns no grid (CTX-0805).
    #[must_use]
    pub fn persistent_to_view_selection(
        &self,
        pers: &PersistentSelection,
        view: &View,
    ) -> Option<Selection> {
        pers.to_view_selection(view, self.session_state_for(view.id())?)
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

    /// View the current search is bound to, if any (CTX-0805).
    #[must_use]
    pub fn search_view(&self) -> Option<ViewId> {
        self.search_view
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
    ///
    /// CTX-0805: a new search binds to the focused View and searches its own
    /// grid; a focused leaf without a grid yields an inactive search.
    pub fn search_set(&mut self, pattern: &str, options: SearchOptions) {
        self.search_view = self.focused_view();
        self.search_set_bound(pattern, options);
    }

    /// Clears the search UI (pattern empty, matches cleared, inactive) and
    /// releases its View binding.
    pub fn search_clear(&mut self) {
        self.search_state.clear();
        self.search_view = None;
    }

    /// Refreshes the current search against the live state after scrollback
    /// growth, resize, or new input. Preserves `current` clamped to the new
    /// match count (or `None` when empty). No-op when search is inactive.
    ///
    /// CTX-0805: refreshes against the bound grid; a bound View that lost
    /// its grid ends the search. An unbound search (test seam) keeps the
    /// historic primary-grid refresh.
    pub fn search_refresh(&mut self) {
        let Some(view) = self.search_view else {
            self.search_state.refresh(&self.state);
            return;
        };
        let grid = self
            .layout
            .find_leaf(view)
            .and_then(|_| grid_of(&self.pane_sessions, self.primary_view, &self.state, view));
        match grid {
            Some(grid) => self.search_state.refresh(grid),
            None => self.search_clear(),
        }
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
        self.search_state
            .current_persistent_selection(self.search_grid()?)
    }

    /// Persistent selection for match `idx`, if in bounds and still valid.
    #[must_use]
    pub fn search_match_persistent_selection(&self, idx: usize) -> Option<PersistentSelection> {
        self.search_state
            .match_persistent_selection(self.search_grid()?, idx)
    }

    /// All current matches as bounded persistent selections (≤ `SEARCH_MAX_RESULTS`).
    ///
    /// Each entry is `is_valid`-filtered, so pruned history matches are dropped
    /// deterministically.
    #[must_use]
    pub fn search_all_persistent_selections(&self) -> Vec<PersistentSelection> {
        self.search_grid().map_or_else(Vec::new, |grid| {
            self.search_state.all_persistent_selections(grid)
        })
    }

    /// Indices of matches whose `buffer_row` is currently visible in `view`.
    ///
    /// CTX-0805: empty for any View other than the one the search is bound
    /// to (its matches are rows of that View's grid only).
    #[must_use]
    pub fn search_visible_match_indices(&self, view: &View) -> Vec<usize> {
        match self.search_grid() {
            Some(grid) if self.search_targets(view.id()) => {
                self.search_state.visible_match_indices(view, grid)
            }
            _ => Vec::new(),
        }
    }

    /// Highlights for matches currently visible in `view`, with view-local
    /// coordinates and `is_current` flag.
    ///
    /// Headless helper for the renderer: maps each visible `SearchMatch` to its
    /// `view_row`, `view_col_start..view_col_end`, and whether it is the
    /// current navigated target. Empty for any View other than the bound one
    /// (CTX-0805).
    #[must_use]
    pub fn search_visible_highlights(&self, view: &View) -> Vec<SearchHighlight> {
        match self.search_grid() {
            Some(grid) if self.search_targets(view.id()) => {
                self.search_state.visible_highlights(view, grid)
            }
            _ => Vec::new(),
        }
    }

    /// Scrolls `view` vertically (and horizontally when needed) to bring the
    /// current match into the viewport. Returns `true` when the view's
    /// `scroll_offset` or `col_offset` changed.
    ///
    /// Deterministic and bounded: the target offset is the minimal adjustment
    /// that makes `current.buffer_row` visible. No-op when no current match,
    /// already visible, or `view` is not the bound View (CTX-0805).
    pub fn search_scroll_view_to_current(&self, view: &mut View) -> bool {
        match self.search_grid() {
            Some(grid) if self.search_targets(view.id()) => {
                self.search_state.scroll_to_current(view, grid)
            }
            _ => false,
        }
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
    ///
    /// CTX-0805: the selection is owned by the View the search is bound to.
    pub fn search_apply_selection(&mut self) -> bool {
        let Some(view) = self.search_view.or_else(|| self.keyboard_view()) else {
            return false;
        };
        let Some(grid) = self.search_grid() else {
            return false;
        };
        let Some(pers) = self.search_state.current_persistent_selection(grid) else {
            return false;
        };
        // Try to restore as live-grid selection.
        if let Some(sel) = pers.to_grid_selection(grid) {
            let pin = if sel.active { Some(sel.anchor) } else { None };
            self.install_selection(view, sel, pin, sel.active);
            true
        } else {
            // In history or pruned: leave a history highlight but clear live selection.
            self.drop_selection();
            false
        }
    }
}
