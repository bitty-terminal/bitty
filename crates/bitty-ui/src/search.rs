//! Scrollback search UI integration (CTX-0061).
//!
//! Headless, bounded, deterministic search UI that binds [`bitty_term_state::State`]
//! search results ([`SearchMatch`]) to the selection/view layer. This module is
//! pure: no I/O, no wall-clock, no platform, no render coupling. It owns the
//! current query, options, bounded matches, and the current navigation index,
//! and provides view-aware highlight mapping and `PersistentSelection`
//! conversion so the current match survives scroll (including history) and
//! resize (clamped). The UI is rendered by the runtime's draw path; this crate
//! only computes deterministic highlight coordinates.
//!
//! # Bounds (T-01)
//!
//! - Pattern is truncated to [`SEARCH_MAX_PATTERN_LEN`] bytes at a char boundary
//!   before search, mirroring `State::search`.
//! - Matches are capped to [`SEARCH_MAX_RESULTS`]; `SearchOptions::max_results`
//!   is clamped, and the stored `matches` vec never exceeds the hard cap.
//! - No unbounded heap growth: the only heap is `pattern` (≤256 bytes) and
//!   `matches` (≤1000 entries).
//!
//! # Determinism
//!
//! All operations are pure functions of `(State, pattern, options, current)`:
//! search ordering is oldest-scrollback-first via `State::search`, navigation
//! wraps deterministically, view mapping is arithmetic on `buffer_row` and
//! `View::scroll_offset`, and highlight coordinates are stable across platforms.
//!
//! # Headless
//!
//! No window system is required. Tests construct `State`/`View` headlessly and
//! drive `SearchState` without a `Clipboard` or `winit` event loop.

#![forbid(unsafe_code)]

use bitty_term_state::{
    State,
    search::{SEARCH_MAX_PATTERN_LEN, SEARCH_MAX_RESULTS, SearchMatch, SearchOptions},
};

use crate::selection::{BufferPos, PersistentSelection};
use crate::view::View;

/// Truncates `pattern` to [`SEARCH_MAX_PATTERN_LEN`] bytes at a char boundary.
///
/// Mirrors `bitty_term_state::search` truncation so the UI query and the
/// state's query agree headlessly.
fn truncate_pattern(pattern: &str) -> String {
    if pattern.len() <= SEARCH_MAX_PATTERN_LEN {
        return pattern.to_string();
    }
    let mut end = SEARCH_MAX_PATTERN_LEN;
    while end > 0 && !pattern.is_char_boundary(end) {
        end -= 1;
    }
    pattern[..end].to_string()
}

/// One highlight for rendering inside a `View` viewport.
///
/// Coordinates are view-local (0-based within the viewport) and already
/// clipped to the viewport's column window (`[col_offset, col_offset+cols)`).
/// `is_current` marks the active navigation entry (brighter style); all other
/// highlights are dim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHighlight {
    /// Index into the owning [`SearchState::matches`] slice.
    pub match_index: usize,
    /// View-local row (0 = top of viewport).
    pub view_row: u16,
    /// View-local inclusive start column after `col_offset` adjustment.
    pub view_col_start: u16,
    /// View-local inclusive end column after `col_offset` adjustment.
    pub view_col_end: u16,
    /// Original combined buffer row (`0 = oldest scrollback`).
    pub buffer_row: usize,
    /// Whether this highlight is the current navigated match.
    pub is_current: bool,
    /// Scrollback line id when match is in history, else `None`.
    pub line_id: Option<u64>,
    /// Matched substring (bounded by pattern length).
    pub matched_text: String,
}

/// Owned search UI state: query, options, bounded matches, current index.
///
/// `active` is true when a non-empty pattern is set (search bar visible);
/// `current` is `Some(0)` when matches are non-empty, otherwise `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pattern: String,
    options: SearchOptions,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    active: bool,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchState {
    /// Creates an inactive, empty search state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pattern: String::new(),
            options: SearchOptions::default(),
            matches: Vec::new(),
            current: None,
            active: false,
        }
    }

    /// Whether the search UI is active (non-empty pattern set).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Current truncated pattern (≤ [`SEARCH_MAX_PATTERN_LEN`] bytes).
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Current search options (clamped).
    #[must_use]
    pub fn options(&self) -> SearchOptions {
        self.options
    }

    /// Bounded matches for the current pattern against the last `set_search`
    /// state snapshot. Ordered oldest-scrollback-first (delegates to `State::search`).
    #[must_use]
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// Number of matches (≤ [`SEARCH_MAX_RESULTS`]).
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Current navigation index, if any.
    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// Current match, if any.
    #[must_use]
    pub fn current_match(&self) -> Option<&SearchMatch> {
        self.current.and_then(|i| self.matches.get(i))
    }

    /// Clears the search: pattern empty, matches cleared, current cleared, inactive.
    pub fn clear(&mut self) {
        self.pattern.clear();
        self.matches.clear();
        self.current = None;
        self.active = false;
    }

    /// Sets the search query and re-runs `State::search` boundedly.
    ///
    /// `pattern` is truncated to [`SEARCH_MAX_PATTERN_LEN`] at a char boundary;
    /// an empty (or all-truncated-empty) pattern clears matches and deactivates
    /// the UI (still headless, no panic). `options.max_results` is clamped to
    /// [`SEARCH_MAX_RESULTS`] inside `State::search` and `SearchOptions::new`.
    /// On success `current` is `Some(0)` when matches non-empty, else `None`;
    /// the UI becomes `active` iff the truncated pattern is non-empty and
    /// `options.max_results != 0`.
    pub fn set_search(&mut self, state: &State, pattern: &str, options: SearchOptions) {
        let pat = truncate_pattern(pattern);
        let max_results = options.max_results.min(SEARCH_MAX_RESULTS);
        // Clamp options explicitly so stored options reflect the bound.
        let opts = SearchOptions::new(options.case_sensitive, max_results);
        self.pattern = pat.clone();
        self.options = opts;
        if pat.is_empty() || max_results == 0 {
            self.matches.clear();
            self.current = None;
            self.active = false;
            return;
        }
        self.active = true;
        self.matches = state.search(&pat, opts);
        if self.matches.is_empty() {
            self.current = None;
        } else {
            self.current = Some(0);
        }
    }

    /// Re-runs the current query against a new `State` (after scrollback growth,
    /// resize, or new input) and clamps `current` into the refreshed match
    /// window. Deterministic: same `(State, pattern, options)` yields same matches.
    ///
    /// If the pattern is empty/inactive this is a no-op. When matches shrink
    /// the current index is clamped to the new last index; when matches become
    /// empty it is cleared. `active` is preserved for non-empty patterns.
    pub fn refresh(&mut self, state: &State) {
        if self.pattern.is_empty() {
            self.matches.clear();
            self.current = None;
            // Keep active false for empty pattern.
            return;
        }
        let prev_current = self.current;
        self.matches = state.search(&self.pattern, self.options);
        if self.matches.is_empty() {
            self.current = None;
        } else if let Some(idx) = prev_current {
            // Clamp to last valid index; preserves logical position when list shrinks.
            let clamped = idx.min(self.matches.len().saturating_sub(1));
            self.current = Some(clamped);
        } else {
            // Was None but now has matches -> select first.
            self.current = Some(0);
        }
    }

    /// Advances the current index by `delta` with wrapping (deterministic).
    ///
    /// `delta > 0` is forward (next), `delta < 0` is backward (prev). Wraps
    /// around `0..len`. No-op when no matches or current is `None`.
    pub fn advance(&mut self, delta: isize) {
        let len = self.matches.len();
        if len == 0 || self.current.is_none() {
            return;
        }
        let cur = self.current.unwrap() as isize;
        let raw = cur + delta;
        // Euclidean remainder for negative wrapping.
        let wrapped = ((raw % len as isize) + len as isize) % len as isize;
        self.current = Some(wrapped as usize);
    }

    /// Next match (wraps).
    pub fn next(&mut self) {
        self.advance(1);
    }

    /// Previous match (wraps).
    pub fn prev(&mut self) {
        self.advance(-1);
    }

    /// Returns a `PersistentSelection` that exactly spans the match at `idx`,
    /// if `idx` is in bounds and the resulting buffer rows are still valid
    /// against `state`.
    ///
    /// The selection is single-row: anchor and focus share `buffer_row`, cols
    /// `col_start..col_end`. For scrollback rows `line_id` is preserved so
    /// `PersistentSelection::is_valid` can detect pruning; for live-grid rows
    /// it is `None`.
    #[must_use]
    pub fn match_persistent_selection(
        &self,
        state: &State,
        idx: usize,
    ) -> Option<PersistentSelection> {
        let m = self.matches.get(idx)?;
        Some(search_match_to_persistent(state, m))
    }

    /// Persistent selection for the current match, if any.
    #[must_use]
    pub fn current_persistent_selection(&self, state: &State) -> Option<PersistentSelection> {
        let idx = self.current?;
        self.match_persistent_selection(state, idx)
    }

    /// All matches as bounded persistent selections (≤ [`SEARCH_MAX_RESULTS`]).
    ///
    /// Each entry is derived via [`search_match_to_persistent`] and is therefore
    /// `is_valid`-checked against `state` at creation time; pruned matches are
    /// filtered out (still bounded). This is the selection-persistence
    /// integration point: callers can highlight every match via persistent
    /// buffer rows that survive scroll.
    #[must_use]
    pub fn all_persistent_selections(&self, state: &State) -> Vec<PersistentSelection> {
        let mut out = Vec::with_capacity(self.matches.len());
        for m in &self.matches {
            let ps = search_match_to_persistent(state, m);
            // Filter to valid only (defensive: line_id may have been pruned between
            // set_search and now; `is_valid` catches drift).
            if ps.is_valid(state) {
                out.push(ps);
            }
        }
        out
    }

    /// Indices of matches whose `buffer_row` is currently visible in `view`.
    ///
    /// Visible means the match's combined buffer row lies inside the view's
    /// viewport window `[start, start+rows)` derived from `State` total
    /// (`sb_len + height`) and `view.scroll_offset`. Headless and deterministic.
    #[must_use]
    pub fn visible_match_indices(&self, view: &View, state: &State) -> Vec<usize> {
        let Some((start, end)) = visible_window(view, state) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (idx, m) in self.matches.iter().enumerate() {
            if m.buffer_row >= start && m.buffer_row < end {
                out.push(idx);
            }
        }
        out
    }

    /// Highlights for matches currently visible in `view`, with view-local
    /// coordinates and `is_current` flag.
    ///
    /// Each highlight maps `buffer_row -> view_row` (`view_row = buffer_row - start`)
    /// and clamps column range to the viewport's column window
    /// `[col_offset, col_offset+cols)`. A match whose column range is entirely
    /// before the viewport or entirely after it is still considered in the
    /// row window but filtered when columns do not overlap; such matches are
    /// excluded from the returned highlights (they would be off-screen
    /// horizontally). `is_current` is true exactly for `current_index`.
    #[must_use]
    pub fn visible_highlights(&self, view: &View, state: &State) -> Vec<SearchHighlight> {
        let Some((start, _end)) = visible_window(view, state) else {
            return Vec::new();
        };
        let cols = view.cols() as usize;
        let col_off = view.col_offset() as usize;
        let col_win_start = col_off;
        let col_win_end = col_off + cols; // exclusive
        let cur = self.current;
        let mut out = Vec::new();
        for (idx, m) in self.matches.iter().enumerate() {
            if m.buffer_row < start || m.buffer_row >= start + view.rows() as usize {
                continue;
            }
            // Horizontal overlap: match cols [col_start, col_end] inclusive must
            // intersect viewport cols [col_off, col_off+cols-1].
            if m.col_end < col_win_start || m.col_start >= col_win_end {
                continue;
            }
            let view_row = (m.buffer_row - start) as u16;
            // Convert match columns to view-local by subtracting col_offset, clamping.
            let vc_start = (m.col_start.max(col_win_start) - col_win_start) as u16;
            let vc_end_exclusive = (m.col_end + 1).min(col_win_end);
            let vc_start_raw = m.col_start.max(col_win_start);
            let vc_end_raw = (m.col_end + 1).min(col_win_end);
            if vc_start_raw >= vc_end_raw {
                continue;
            }
            let view_col_start = vc_start;
            let view_col_end = (vc_end_exclusive - col_win_start - 1) as u16;
            // Guard view_col_end >= view_col_start
            let view_col_end = view_col_end.min(view.cols().saturating_sub(1));
            out.push(SearchHighlight {
                match_index: idx,
                view_row,
                view_col_start,
                view_col_end,
                buffer_row: m.buffer_row,
                is_current: cur == Some(idx),
                line_id: m.line_id,
                matched_text: m.matched_text.clone(),
            });
        }
        out
    }

    /// Scrolls `view` vertically (and optionally horizontally) to bring the
    /// current match into the viewport. Returns `true` when the view's
    /// `scroll_offset` or `col_offset` changed.
    ///
    /// The target offset is the minimal vertical adjustment that makes
    /// `current.buffer_row` visible: if `current.buffer_row < start` the view
    /// scrolls up; if `>= end` it scrolls down. When `current` is `None` or
    /// the match's row is already visible, no scroll occurs. Column
    /// adjustment is performed only when the match is horizontally off-screen:
    /// `col_offset` is set to `col_start` when the match starts to the right
    /// of the window, or to `col_end+1-cols` when it ends to the left, else
    /// unchanged. Bounded and deterministic.
    pub fn scroll_to_current(&self, view: &mut View, state: &State) -> bool {
        let Some(m) = self.current_match() else {
            return false;
        };
        let mut changed = false;
        // Vertical scroll.
        if let Some((start, end)) = visible_window(view, state) {
            if m.buffer_row < start || m.buffer_row >= end {
                // Desired start that puts target at top when above, or bottom when below.
                let total = state.scrollback_len() + state.height();
                let rows = view.rows() as usize;
                let sb_len = state.scrollback_len();
                let target = m.buffer_row;
                // We want smallest offset that makes target visible. Derive offset:
                // start = total - rows - offset => offset = total - rows - start
                // For target < start: want start = target => offset = total - rows - target
                // For target >= end: want end = target+1 => start = target+1 - rows => offset = total - rows - (target+1 - rows) = total - target -1
                let desired_offset = if target < start {
                    total.saturating_sub(rows).saturating_sub(target)
                } else {
                    // target >= end
                    // bring target to bottom: start = target+1 - rows
                    let new_start = target + 1 - rows;
                    total.saturating_sub(rows).saturating_sub(new_start)
                };
                let clamped = desired_offset.min(sb_len);
                if clamped != view.scroll_offset() {
                    view.set_scroll_offset(clamped, sb_len);
                    changed = true;
                }
            }
        }
        // Horizontal col_offset adjustment (only when match off-screen horizontally).
        let cols = view.cols() as usize;
        let col_off = view.col_offset() as usize;
        let win_start = col_off;
        let win_end = col_off + cols; // exclusive
        if m.col_start >= win_end || m.col_end < win_start {
            let state_width = state.width();
            let max_col_off = state_width.saturating_sub(cols);
            let desired_col_off = if m.col_start >= win_end {
                // Match to the right: show its start at left edge.
                m.col_start
            } else {
                // Match to the left: show its end at right edge.
                m.col_end + 1 - cols
            };
            let clamped = desired_col_off.min(max_col_off);
            if clamped != col_off {
                view.set_col_offset(clamped as u16);
                changed = true;
            }
        }
        changed
    }
}

/// Converts a [`SearchMatch`] to a single-row [`PersistentSelection`] that
/// exactly covers the matched columns.
///
/// For scrollback matches `line_id` is preserved for prune detection; for live
/// matches it is `None`. Column clamping is deferred to `PersistentSelection`
/// resolution (`clamped`/`to_grid_selection`).
#[must_use]
pub fn search_match_to_persistent(_state: &State, m: &SearchMatch) -> PersistentSelection {
    let anchor_line_id = m.line_id;
    let focus_line_id = m.line_id;
    let anchor = BufferPos::new(m.buffer_row, m.col_start as u16);
    let focus = BufferPos::new(m.buffer_row, m.col_end as u16);
    // Bypass private fields via construction helper: PersistentSelection is a plain
    // record with public fields, so we can construct directly. For future-proofing
    // we keep this function in the `selection` crate friendship boundary (both
    // in `bitty-ui`).
    PersistentSelection {
        anchor,
        anchor_line_id,
        focus,
        focus_line_id,
        kind: crate::selection::SelectionKind::Simple,
        active: false,
    }
}

/// Computes the combined-buffer viewport window `[start, end)` for `view` against
/// `state`. Returns `None` when rows==0 or state has zero width/height edge
/// (still headless, no panic). `end` is exclusive.
fn visible_window(view: &View, state: &State) -> Option<(usize, usize)> {
    let rows = view.rows() as usize;
    if rows == 0 {
        return None;
    }
    let sb_len = state.scrollback_len();
    let total = sb_len + state.height();
    let offset = view.scroll_offset().min(sb_len);
    let start = total.saturating_sub(rows).saturating_sub(offset);
    let end = start + rows;
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{View, ViewId};
    use bitty_term_state::search::SearchOptions;
    use bitty_term_state::{State, TerminalAction};
    use bitty_vt::{ControlChar, GraphemeCell};

    fn prints(state: &mut State, text: &str) {
        for ch in text.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }

    fn feed_line(state: &mut State, text: &str) {
        prints(state, text);
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }

    #[test]
    fn search_state_is_headless_and_bounded() {
        let state = State::new();
        let mut ui = SearchState::new();
        assert!(!ui.is_active());
        assert_eq!(ui.match_count(), 0);
        assert_eq!(ui.current_index(), None);
        // Empty pattern -> inactive, no matches
        ui.set_search(&state, "", SearchOptions::default());
        assert!(!ui.is_active());
        assert_eq!(ui.match_count(), 0);
        // Overlong pattern truncated, still bounded (deterministic, no panic)
        let long = "a".repeat(SEARCH_MAX_PATTERN_LEN + 50);
        let mut s2 = State::new();
        prints(&mut s2, "aaaaaaaaaa");
        ui.set_search(&s2, &long, SearchOptions::default());
        // Long pattern (257+ chars) truncated but longer than line => no matches
        assert!(ui.is_active());
        assert_eq!(ui.match_count(), 0);
        // Max results clamp
        let mut s3 = State::new();
        for _ in 0..5 {
            feed_line(&mut s3, "xxx xxx xxx");
        }
        prints(&mut s3, "xxx xxx");
        ui.set_search(&s3, "xxx", SearchOptions::new(true, 2));
        assert_eq!(ui.match_count(), 2);
        assert_eq!(ui.options().max_results, 2);
        // Zero max_results -> inactive
        ui.set_search(&s3, "xxx", SearchOptions::new(true, 0));
        assert!(!ui.is_active());
        assert_eq!(ui.match_count(), 0);
    }

    #[test]
    fn search_state_set_and_navigate_deterministic() {
        let mut state = State::new();
        for i in 0..(state.height() + 3) {
            feed_line(&mut state, &format!("line{i:02} needle"));
        }
        prints(&mut state, "live needle");
        let mut ui = SearchState::new();
        ui.set_search(&state, "needle", SearchOptions::default());
        assert!(ui.is_active());
        assert!(ui.match_count() >= 4);
        // Current starts at 0
        assert_eq!(ui.current_index(), Some(0));
        // Ordered by buffer_row
        for w in ui.matches().windows(2) {
            assert!(w[0].buffer_row <= w[1].buffer_row);
        }
        // Deterministic navigate
        let first = ui.current_match().unwrap().buffer_row;
        ui.next();
        let second = ui.current_match().unwrap().buffer_row;
        assert!(second >= first);
        ui.prev();
        assert_eq!(ui.current_match().unwrap().buffer_row, first);
        // Wrap
        ui.prev();
        let last = ui.current_match().unwrap().buffer_row;
        assert!(last >= second);
        ui.next();
        assert_eq!(ui.current_match().unwrap().buffer_row, first);
        // Advance with delta
        ui.advance(2);
        let third = ui.current_index().unwrap();
        ui.advance(-2);
        assert_eq!(ui.current_index(), Some(first - first)); // wraps to 0 logically
        let _ = third;
    }

    #[test]
    fn search_match_to_persistent_and_visible_highlights() {
        let mut state = State::new();
        // Fill scrollback with 4 lines each "hi needle"
        for _ in 0..(state.height() + 4) {
            feed_line(&mut state, "hi needle");
        }
        prints(&mut state, "hi needle live");
        let mut view = View::new(ViewId::new(1), state.width(), state.height());
        let mut ui = SearchState::new();
        ui.set_search(&state, "needle", SearchOptions::default());
        assert!(ui.match_count() >= 5);
        // All persistent selections are valid and single-row
        let pss = ui.all_persistent_selections(&state);
        assert_eq!(pss.len(), ui.match_count());
        for ps in &pss {
            assert!(ps.is_valid(&state));
            assert_eq!(ps.anchor.buffer_row, ps.focus.buffer_row);
        }
        // Current persistent selection exists and its buffer text is "needle"
        let cur_ps = ui.current_persistent_selection(&state).unwrap();
        assert_eq!(cur_ps.text(&state).as_deref(), Some("needle"));
        // Visible highlights when live (offset 0) should contain at least the live match
        let live_hls = ui.visible_highlights(&view, &state);
        assert!(
            !live_hls.is_empty(),
            "live viewport must show at least one match"
        );
        for hl in &live_hls {
            assert!(hl.view_row < view.rows());
            assert_eq!(hl.is_current, hl.match_index == ui.current_index().unwrap());
        }
        // Scroll up by 2 lines -> highlights shift but count remains bounded
        view.set_scroll_offset(2, state.scrollback_len());
        let scrolled_hls = ui.visible_highlights(&view, &state);
        // Visible window still yields deterministic highlights
        let again = ui.visible_highlights(&view, &state);
        assert_eq!(scrolled_hls, again);
        // Visible indices deterministic
        let vis = ui.visible_match_indices(&view, &state);
        let vis2 = ui.visible_match_indices(&view, &state);
        assert_eq!(vis, vis2);
        // At least one match should be visible after scroll as well
        assert!(!vis.is_empty());
    }

    #[test]
    fn search_scroll_to_current_brings_offscreen_into_view() {
        let mut state = State::new();
        for i in 0..(state.height() + 8) {
            feed_line(&mut state, &format!("item{i:02} findme"));
        }
        prints(&mut state, "live findme");
        let mut ui = SearchState::new();
        ui.set_search(&state, "findme", SearchOptions::default());
        // Current is first (oldest) scrollback match, which is off-screen when view is live.
        let mut view_live = View::new(ViewId::new(1), state.width(), state.height());
        assert_eq!(view_live.scroll_offset(), 0);
        // Current buffer_row is 0 (oldest), live window start = total - rows
        // That start is >0, so current is off-screen.
        let cur_row = ui.current_match().unwrap().buffer_row;
        let (start, end) = visible_window(&view_live, &state).unwrap();
        assert!(
            cur_row < start || cur_row >= end,
            "oldest match should be off-screen when live"
        );
        // scroll_to_current should move offset so current becomes visible
        let changed = ui.scroll_to_current(&mut view_live, &state);
        assert!(changed, "scroll should have changed");
        let (start2, end2) = visible_window(&view_live, &state).unwrap();
        assert!(
            cur_row >= start2 && cur_row < end2,
            "current must be visible after scroll_to_current"
        );
        // Second call when already visible should not move
        let unchanged = ui.scroll_to_current(&mut view_live, &state);
        assert!(!unchanged);
        // Navigate to last (live) match, then scroll from scrolled position back to live.
        while ui.current_index().unwrap() + 1 < ui.match_count() {
            ui.next();
        }
        let live_row = ui.current_match().unwrap().buffer_row;
        assert!(live_row >= start2); // live match is near total
        // If view remains scrolled up, live may be off-screen below. Ensure scroll brings it.
        let mut view_scrolled = View::new(ViewId::new(1), state.width(), state.height());
        view_scrolled.set_scroll_offset(state.scrollback_len(), state.scrollback_len());
        let (s3, e3) = visible_window(&view_scrolled, &state).unwrap();
        if live_row < s3 || live_row >= e3 {
            let ch = ui.scroll_to_current(&mut view_scrolled, &state);
            assert!(ch);
            let (s4, e4) = visible_window(&view_scrolled, &state).unwrap();
            assert!(live_row >= s4 && live_row < e4);
        }
        // Prune resilience: after scrolling many lines, refresh still headless
        for _ in 0..40 {
            feed_line(&mut state, "prune filler");
        }
        ui.refresh(&state);
        assert!(ui.match_count() <= SEARCH_MAX_RESULTS);
    }

    #[test]
    fn search_refresh_preserves_current_clamped() {
        let mut state = State::new();
        feed_line(&mut state, "needle one");
        feed_line(&mut state, "needle two");
        // Force scrollback
        for _ in 0..state.height() {
            feed_line(&mut state, "filler");
        }
        let mut ui = SearchState::new();
        ui.set_search(&state, "needle", SearchOptions::default());
        assert!(ui.match_count() >= 2);
        ui.next(); // go to index 1
        assert_eq!(ui.current_index(), Some(1));
        // Feed many filler lines to prune? Scrollback cap is 10000, not prune yet, but refresh should keep current clamped.
        for _ in 0..2 {
            feed_line(&mut state, "needle three");
        }
        ui.refresh(&state);
        assert!(ui.current_index().unwrap() < ui.match_count());
        // Clear then refresh stays empty
        ui.clear();
        assert!(!ui.is_active());
        ui.refresh(&state);
        assert_eq!(ui.match_count(), 0);
    }

    #[test]
    fn search_case_insensitive_and_wide_char_mapping() {
        let mut state = State::new();
        prints(&mut state, "Hello");
        let mut ui = SearchState::new();
        ui.set_search(&state, "hello", SearchOptions::new(true, 10));
        assert_eq!(ui.match_count(), 0);
        ui.set_search(&state, "hello", SearchOptions::new(false, 10));
        assert_eq!(ui.match_count(), 1);
        assert_eq!(ui.current_match().unwrap().matched_text, "Hello");
        // Wide char
        let mut s2 = State::new();
        prints(&mut s2, "A\u{4e2d}B");
        let mut ui2 = SearchState::new();
        ui2.set_search(&s2, "\u{4e2d}", SearchOptions::default());
        assert_eq!(ui2.match_count(), 1);
        let m = ui2.current_match().unwrap();
        assert_eq!(m.col_start, 1);
        assert_eq!(m.col_end, 2);
        // Scrollback + live with wide char and view highlight
        let view = View::new(ViewId::new(1), s2.width(), s2.height());
        let hls = ui2.visible_highlights(&view, &s2);
        assert_eq!(hls.len(), 1);
        assert_eq!(hls[0].view_col_start, 1);
        assert_eq!(hls[0].view_col_end, 2);
    }

    #[test]
    fn search_clear_and_truncate_at_char_boundary() {
        let state = State::new();
        let mut ui = SearchState::new();
        // Multi-byte emoji: each 4 bytes, ensure truncation at char boundary
        let emoji = "😀".repeat(100); // 400 bytes
        ui.set_search(&state, &emoji, SearchOptions::default());
        // Truncated pattern should be at char boundary and length <=256
        assert!(ui.pattern().len() <= SEARCH_MAX_PATTERN_LEN);
        assert!(ui.pattern().is_char_boundary(ui.pattern().len()));
        ui.clear();
        assert!(!ui.is_active());
        assert_eq!(ui.match_count(), 0);
        assert_eq!(ui.current_index(), None);
        assert!(ui.pattern().is_empty());
    }

    #[test]
    fn search_deterministic_across_states() {
        let mut a = State::new();
        let mut b = State::new();
        for st in [&mut a, &mut b] {
            for i in 0..6 {
                feed_line(st, &format!("row{i} needle"));
            }
            prints(st, "live needle");
        }
        let mut ua = SearchState::new();
        let mut ub = SearchState::new();
        ua.set_search(&a, "needle", SearchOptions::new(false, 100));
        ub.set_search(&b, "needle", SearchOptions::new(false, 100));
        assert_eq!(ua.matches(), ub.matches());
        assert_eq!(ua.current_index(), ub.current_index());
        // Highlights deterministic
        let view = View::new(ViewId::new(1), a.width(), a.height());
        assert_eq!(
            ua.visible_highlights(&view, &a),
            ub.visible_highlights(&view, &b)
        );
    }
}
