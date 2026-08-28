//! Scrollback search UI integration (CTX-0061) — headless.
//!
//! Proves the search UI wiring over the scrollback search primitive and
//! selection persistence:
//!
//! - `SearchState` bounded truncation, capped matches, case-sensitive/insensitive,
//!   wide-char col mapping, deterministic ordering, next/prev wrap, refresh
//!   clamping, clear/inactive.
//! - `SearchState` → `PersistentSelection` conversion (single-row, line_id-anchored)
//!   and view-aware highlight mapping (`visible_highlights`, `visible_match_indices`,
//!   `scroll_to_current`).
//! - `Runtime` search UI integration: `search_set`/`clear`/`refresh`/`next`/`prev`,
//!   `search_visible_highlights`, `search_scroll_view_to_current`,
//!   `search_apply_selection` (live-grid selection sync), auto-refresh after
//!   `handle_pty_bytes` and `handle_resize`, headless bounded deterministic.
//! - Headless determinism: same `(State, pattern, options)` yields same matches
//!   and highlights across runtimes; no window/GPU/PTY spawn.

#![forbid(unsafe_code)]

use bitty_platform::PhysicalSize;
use bitty_runtime::Runtime;
use bitty_term_state::search::{SEARCH_MAX_PATTERN_LEN, SEARCH_MAX_RESULTS, SearchOptions};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::{SearchState, View, ViewId};
use bitty_vt::{ControlChar, GraphemeCell};

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

fn feed_line(state: &mut State, text: &str) {
    prints(state, text);
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
}

fn feed_runtime(rt: &mut Runtime, text: &str) {
    rt.handle_pty_bytes(text.as_bytes());
}

#[test]
fn search_ui_set_and_navigate_headless() {
    let mut rt = make_runtime();
    for i in 0..(rt.state().height() + 4) {
        feed_runtime(&mut rt, &format!("line{i:02} needle\n"));
    }
    feed_runtime(&mut rt, "live needle here");

    // Bounded case-sensitive search via Runtime SearchState
    rt.search_set("needle", SearchOptions::default());
    assert!(rt.search_is_active());
    assert!(rt.search_match_count() >= 5);
    assert_eq!(rt.search_current_index(), Some(0));
    // Ordered oldest first
    for w in rt.search_matches().windows(2) {
        assert!(w[0].buffer_row <= w[1].buffer_row);
    }
    // Next/prev wrap deterministically
    let first_row = rt.search_current_match().unwrap().buffer_row;
    rt.search_next();
    let second_row = rt.search_current_match().unwrap().buffer_row;
    assert!(second_row >= first_row);
    rt.search_prev();
    assert_eq!(rt.search_current_match().unwrap().buffer_row, first_row);
    // Wrap to last
    rt.search_prev();
    let last_row = rt.search_current_match().unwrap().buffer_row;
    assert!(last_row >= second_row);
    rt.search_next();
    assert_eq!(rt.search_current_match().unwrap().buffer_row, first_row);
    // Advance with delta
    rt.search_advance(2);
    let idx2 = rt.search_current_index().unwrap();
    rt.search_advance(-2);
    assert_eq!(rt.search_current_index(), Some(0));
    let _ = idx2;
}

#[test]
fn search_ui_case_sensitivity_and_bounds_headless() {
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "Hello hello HELLO");

    rt.search_set("hello", SearchOptions::new(true, 100));
    assert_eq!(rt.search_match_count(), 1);
    assert_eq!(rt.search_current_match().unwrap().matched_text, "hello");

    rt.search_set("hello", SearchOptions::new(false, 100));
    assert_eq!(rt.search_match_count(), 3);

    // Max results cap
    let mut rt2 = make_runtime();
    for _ in 0..5 {
        feed_runtime(&mut rt2, "xxx xxx xxx\n");
    }
    rt2.search_set("xxx", SearchOptions::new(true, 2));
    assert_eq!(rt2.search_match_count(), 2);
    assert!(rt2.search_match_count() <= SEARCH_MAX_RESULTS);

    // Empty pattern -> inactive, no matches
    rt2.search_set("", SearchOptions::default());
    assert!(!rt2.search_is_active());
    assert_eq!(rt2.search_match_count(), 0);
    assert_eq!(rt2.search_current_index(), None);

    // Zero max_results -> inactive
    rt2.search_set("xxx", SearchOptions::new(true, 0));
    assert!(!rt2.search_is_active());
    assert_eq!(rt2.search_match_count(), 0);

    // Overlong pattern truncated at char boundary, still headless
    let long = "a".repeat(SEARCH_MAX_PATTERN_LEN + 100);
    rt.search_set(&long, SearchOptions::default());
    assert!(rt.search_pattern().len() <= SEARCH_MAX_PATTERN_LEN);
    assert!(
        rt.search_pattern()
            .is_char_boundary(rt.search_pattern().len())
    );
    assert!(rt.search_is_active());
    // Overlong pattern longer than line => no matches but still active (pattern non-empty)
    assert_eq!(rt.search_match_count(), 0);
    assert_eq!(rt.search_current_index(), None);

    // Multi-byte emoji truncation at char boundary
    let emoji = "😀".repeat(100); // 400 bytes
    rt.search_set(&emoji, SearchOptions::default());
    assert!(rt.search_pattern().len() <= SEARCH_MAX_PATTERN_LEN);
    assert!(
        rt.search_pattern()
            .is_char_boundary(rt.search_pattern().len())
    );
}

#[test]
fn search_ui_wide_char_and_highlight_headless() {
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "A\u{4e2d}B live");
    rt.search_set("\u{4e2d}", SearchOptions::default());
    assert_eq!(rt.search_match_count(), 1);
    let m = rt.search_current_match().unwrap();
    assert_eq!(m.matched_text, "\u{4e2d}");
    assert_eq!(m.col_start, 1);
    assert_eq!(m.col_end, 2);

    // Visible highlight mapping for live viewport
    let view = View::new(ViewId::new(1), rt.state().width(), rt.state().height());
    let hls = rt.search_visible_highlights(&view);
    assert_eq!(hls.len(), 1);
    assert_eq!(hls[0].view_col_start, 1);
    assert_eq!(hls[0].view_col_end, 2);
    assert!(hls[0].is_current);

    // Current persistent selection is exact and live
    let ps = rt
        .search_current_persistent_selection()
        .expect("current ps");
    assert_eq!(ps.text(rt.state()).as_deref(), Some("\u{4e2d}"));
    assert!(ps.is_valid(rt.state()));
    // All persistent selections bounded
    let all = rt.search_all_persistent_selections();
    assert_eq!(all.len(), 1);
}

#[test]
fn search_ui_visible_highlights_and_scroll_to_current_headless() {
    let mut rt = make_runtime();
    // Generate enough scrollback that oldest match is off-screen when live
    for i in 0..(rt.state().height() + 8) {
        feed_runtime(&mut rt, &format!("item{i:02} findme\n"));
    }
    feed_runtime(&mut rt, "live findme");
    rt.search_set("findme", SearchOptions::default());
    assert!(rt.search_match_count() >= 5);
    // Current is oldest (buffer_row 0) when live window shows bottom rows
    let first_row = rt.search_current_match().unwrap().buffer_row;
    let mut view_live = View::new(ViewId::new(1), rt.state().width(), rt.state().height());
    // Live window start = total - rows
    let total = rt.state().scrollback_len() + rt.state().height();
    let rows = view_live.rows() as usize;
    let start = total.saturating_sub(rows);
    assert!(
        first_row < start,
        "oldest match should be off-screen when live"
    );
    // No highlight for off-screen current
    let live_hls = rt.search_visible_highlights(&view_live);
    let is_first_visible = live_hls.iter().any(|h| h.buffer_row == first_row);
    assert!(
        !is_first_visible,
        "oldest off-screen should not be in live highlights"
    );

    // Visible indices for live window should be at least the live match
    let vis = rt.search_visible_match_indices(&view_live);
    assert!(!vis.is_empty());
    assert!(
        vis.iter()
            .any(|&i| rt.search_matches()[i].buffer_row >= start)
    );

    // scroll_to_current should bring oldest into view
    let changed = rt.search_scroll_view_to_current(&mut view_live);
    assert!(changed);
    let total2 = rt.state().scrollback_len() + rt.state().height();
    let start2 = total2
        .saturating_sub(view_live.rows() as usize)
        .saturating_sub(view_live.scroll_offset());
    let end2 = start2 + view_live.rows() as usize;
    assert!(
        first_row >= start2 && first_row < end2,
        "current must be visible after scroll"
    );
    // After scroll, highlight appears
    let hls2 = rt.search_visible_highlights(&view_live);
    assert!(
        hls2.iter()
            .any(|h| h.buffer_row == first_row && h.is_current)
    );

    // Second call when already visible should not change
    let unchanged = rt.search_scroll_view_to_current(&mut view_live);
    assert!(!unchanged);

    // Navigate to last (live) match, scroll from scrolled position back to live
    while rt.search_current_index().unwrap() + 1 < rt.search_match_count() {
        rt.search_next();
    }
    let live_row = rt.search_current_match().unwrap().buffer_row;
    // View still scrolled to oldest, live is off-screen below
    let (s3, e3) = {
        let tot = rt.state().scrollback_len() + rt.state().height();
        let st = tot
            .saturating_sub(view_live.rows() as usize)
            .saturating_sub(view_live.scroll_offset());
        (st, st + view_live.rows() as usize)
    };
    if live_row < s3 || live_row >= e3 {
        let ch = rt.search_scroll_view_to_current(&mut view_live);
        assert!(ch);
        let tot = rt.state().scrollback_len() + rt.state().height();
        let st4 = tot
            .saturating_sub(view_live.rows() as usize)
            .saturating_sub(view_live.scroll_offset());
        assert!(live_row >= st4 && live_row < st4 + view_live.rows() as usize);
    }
}

#[test]
fn search_ui_integrates_with_selection_persistence_headless() {
    let mut rt = make_runtime();
    // Scrollback with needle, live also has needle
    for i in 0..(rt.state().height() + 2) {
        feed_runtime(&mut rt, &format!("row{i:02} needle\n"));
    }
    feed_runtime(&mut rt, "live needle here");
    rt.search_set("needle", SearchOptions::default());
    assert!(rt.search_match_count() >= 3);

    // Current is oldest scrollback needle -> not live, apply should clear live selection
    assert!(rt.search_current_match().unwrap().is_scrollback());
    let applied = rt.search_apply_selection();
    assert!(!applied, "history match should not become live selection");
    assert!(
        rt.selection().is_none(),
        "live selection cleared for history highlight"
    );
    // But persistent selection is still valid and readable as history
    let ps = rt.search_current_persistent_selection().unwrap();
    assert!(ps.is_valid(rt.state()));
    assert_eq!(ps.text(rt.state()).as_deref(), Some("needle"));

    // Navigate to live match (last)
    while rt.search_current_match().unwrap().is_scrollback() {
        rt.search_next();
    }
    assert!(!rt.search_current_match().unwrap().is_scrollback());
    let applied_live = rt.search_apply_selection();
    assert!(applied_live, "live match should become live selection");
    assert!(rt.selection().is_some());
    assert_eq!(rt.selection_text().as_deref(), Some("needle"));
    // Lifting the live selection to persistent should equal the current search persistent
    let pers_from_sel = rt.persistent_selection().unwrap();
    let pers_from_search = rt.search_current_persistent_selection().unwrap();
    assert_eq!(pers_from_sel.anchor, pers_from_search.anchor);
    assert_eq!(pers_from_sel.focus, pers_from_search.focus);
    assert_eq!(
        pers_from_sel.text(rt.state()),
        pers_from_search.text(rt.state())
    );
}

#[test]
fn search_ui_refresh_preserves_current_clamped_and_auto_refresh_headless() {
    let mut rt = make_runtime();
    for _ in 0..(rt.state().height() + 2) {
        feed_runtime(&mut rt, "needle one\n");
    }
    // One more needle later
    feed_runtime(&mut rt, "needle two");
    rt.search_set("needle", SearchOptions::default());
    let count_before = rt.search_match_count();
    assert!(count_before >= 2);
    rt.search_next();
    assert_eq!(rt.search_current_index(), Some(1));

    // Feed new state via handle_pty_bytes; auto-refresh should keep search active and update matches
    feed_runtime(&mut rt, "\nneedle three\n");
    assert!(rt.search_is_active());
    assert!(rt.search_match_count() >= count_before);
    // Current should still be clamped within bounds
    assert!(rt.search_current_index().unwrap() < rt.search_match_count());

    // Clear then refresh no-op
    rt.search_clear();
    assert!(!rt.search_is_active());
    assert_eq!(rt.search_match_count(), 0);
    rt.search_refresh();
    assert_eq!(rt.search_match_count(), 0);

    // Resize larger should also refresh (search inactive remains inactive, active would clamp)
    rt.search_set("needle", SearchOptions::default());
    let cnt = rt.search_match_count();
    rt.handle_resize(PhysicalSize::new(8 * 120, 16 * 30))
        .expect("resize must succeed");
    assert_eq!(rt.search_match_count(), cnt);
    // After resize, visible highlights still bounded
    let view = View::new(ViewId::new(1), rt.state().width(), rt.state().height());
    assert!(rt.search_visible_highlights(&view).len() <= SEARCH_MAX_RESULTS);
}

#[test]
fn search_ui_deterministic_headless() {
    let mut a = make_runtime();
    let mut b = make_runtime();
    for rt in [&mut a, &mut b] {
        for i in 0..6 {
            feed_runtime(rt, &format!("row{i} needle\n"));
        }
        feed_runtime(rt, "live needle");
    }
    let opts = SearchOptions::new(false, 100);
    a.search_set("needle", opts);
    b.search_set("needle", opts);
    assert_eq!(a.search_matches(), b.search_matches());
    assert_eq!(a.search_current_index(), b.search_current_index());
    let view = View::new(ViewId::new(1), a.state().width(), a.state().height());
    assert_eq!(
        a.search_visible_highlights(&view),
        b.search_visible_highlights(&view)
    );
    a.search_next();
    b.search_next();
    assert_eq!(a.search_current_index(), b.search_current_index());
    assert_eq!(
        a.search_current_persistent_selection().unwrap().anchor,
        b.search_current_persistent_selection().unwrap().anchor
    );
    // After same navigation, deterministic scroll
    let mut va = View::new(ViewId::new(1), a.state().width(), a.state().height());
    let mut vb = View::new(ViewId::new(1), b.state().width(), b.state().height());
    assert_eq!(
        a.search_scroll_view_to_current(&mut va),
        b.search_scroll_view_to_current(&mut vb)
    );
    assert_eq!(va.scroll_offset(), vb.scroll_offset());
}

#[test]
fn search_ui_survives_view_scroll_window_and_pruning_headless() {
    let mut state = State::new();
    for i in 0..(state.height() + 6) {
        feed_line(&mut state, &format!("view{i:02} findme"));
    }
    let sb_len = state.scrollback_len();
    let view = View::new(ViewId::new(1), state.width(), state.height());
    // Build SearchState headlessly against State + View
    let mut ui = SearchState::new();
    ui.set_search(&state, "findme", SearchOptions::default());
    assert!(ui.match_count() >= 5);
    let all_ps = ui.all_persistent_selections(&state);
    assert_eq!(all_ps.len(), ui.match_count());
    // Visible highlights when live
    let live_hls = ui.visible_highlights(&view, &state);
    assert!(!live_hls.is_empty());
    // Scroll view up by 2 lines: highlights shift deterministically
    let mut view_scrolled = view.clone();
    view_scrolled.set_scroll_offset(2, sb_len);
    let scrolled_hls = ui.visible_highlights(&view_scrolled, &state);
    let again = ui.visible_highlights(&view_scrolled, &state);
    assert_eq!(scrolled_hls, again);
    // Visible indices deterministic
    let vis = ui.visible_match_indices(&view_scrolled, &state);
    assert_eq!(vis, ui.visible_match_indices(&view_scrolled, &state));
    assert!(!vis.is_empty());
    // After scroll, current still maps to buffer row; scroll_to_current from live brings it
    let mut view_live = View::new(ViewId::new(1), state.width(), state.height());
    let first_row = ui.current_match().unwrap().buffer_row;
    let (start, end) = {
        let total = sb_len + state.height();
        let rows = view_live.rows() as usize;
        let st = total.saturating_sub(rows);
        (st, st + rows)
    };
    assert!(first_row < start || first_row >= end);
    let changed = ui.scroll_to_current(&mut view_live, &state);
    assert!(changed);
    // After many filler lines, refresh remains bounded and headless
    for _ in 0..40 {
        feed_line(&mut state, "prune filler");
    }
    ui.refresh(&state);
    assert!(ui.match_count() <= SEARCH_MAX_RESULTS);
    // History highlights still deterministic after refresh
    let _ = ui.visible_highlights(&view_live, &state);
}

#[test]
fn search_ui_integration_with_view_persistence_headless() {
    // Combined smoke: search, view scroll, persistent selection, resize, clear
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "persist search test");
    rt.search_set("persist", SearchOptions::default());
    assert_eq!(rt.search_match_count(), 1);
    let ps = rt.search_current_persistent_selection().unwrap();
    assert_eq!(ps.text(rt.state()).as_deref(), Some("persist"));
    // Apply as selection (live) and verify
    assert!(rt.search_apply_selection());
    assert_eq!(rt.selection_text().as_deref(), Some("persist"));
    // View highlight for live
    let view = View::new(ViewId::new(1), rt.state().width(), rt.state().height());
    let hls = rt.search_visible_highlights(&view);
    assert_eq!(hls.len(), 1);
    assert!(hls[0].is_current);
    // Resize larger, search should auto-refresh and remain valid
    rt.handle_resize(PhysicalSize::new(8 * 120, 16 * 30))
        .expect("resize");
    assert_eq!(rt.search_match_count(), 1);
    assert!(
        rt.search_current_persistent_selection()
            .unwrap()
            .is_valid(rt.state())
    );
    // Scroll simulation: create view with offset, highlight still deterministic
    let mut view2 = View::new(ViewId::new(1), rt.state().width(), rt.state().height());
    view2.set_scroll_offset(0, rt.state().scrollback_len());
    assert_eq!(
        rt.search_visible_highlights(&view2).len(),
        rt.search_visible_highlights(&view2).len()
    );
    // Clear search deactivates UI but does not clear selection automatically
    rt.search_clear();
    assert!(!rt.search_is_active());
    assert_eq!(rt.search_match_count(), 0);
    // Selection from previous apply remains until explicitly cleared
    assert!(rt.selection().is_some());
    rt.clear_selection();
    assert!(rt.selection().is_none());
    // New search after clear is fresh
    rt.search_set("search", SearchOptions::default());
    assert_eq!(rt.search_match_count(), 1);
    // Headless determinism across second runtime with same pattern
    let mut rt2 = make_runtime();
    feed_runtime(&mut rt2, "persist search test");
    rt2.search_set("search", SearchOptions::default());
    assert_eq!(rt.search_matches().len(), 1);
    assert_eq!(rt2.search_matches().len(), 1);
    assert_eq!(
        rt.search_current_match().unwrap().matched_text,
        rt2.search_current_match().unwrap().matched_text
    );
}
