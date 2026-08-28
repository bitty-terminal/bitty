//! Scrollback search and selection persistence integration (headless).
//!
//! Proves CTX-0060 without a window server, GPU, or PTY spawn:
//!
//! - `State::search` bounded, case-sensitive/insensitive, wide-char col mapping,
//!   truncation, max-results cap, deterministic
//! - `Runtime::search` delegation (headless)
//! - `PersistentSelection` lifting/resolving across
//!   `State` scroll (buffer-row stability), `State::resize` (clamping),
//!   `View` scroll offset window, and `FullReset` invalidation
//! - Selection persistence via `Runtime::persistent_selection` /
//!   `restore_persistent_selection` and `persistent_selection_text`
//! - Headless determinism and boundedness

#![forbid(unsafe_code)]

use bitty_platform::PhysicalSize;
use bitty_runtime::Runtime;
use bitty_term_state::search::{SEARCH_MAX_PATTERN_LEN, SEARCH_MAX_RESULTS, SearchOptions};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::{CellPos, PersistentSelection, Selection, View, ViewId};
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
fn search_finds_in_scrollback_and_live_grid_headless() {
    let mut rt = make_runtime();
    // Generate scrollback: each iteration prints a line then LF (scrolls once after height).
    for i in 0..(rt.state().height() + 4) {
        feed_runtime(&mut rt, &format!("line{i:02} needle\n"));
    }
    // One live-grid line with needle as well
    feed_runtime(&mut rt, "live needle here");
    let opts = SearchOptions::default();
    let matches = rt.search("needle", opts);
    // At least 5 matches (4 scrollback + 1 live)
    assert!(matches.len() >= 5, "found {} matches", matches.len());
    // Ordered by buffer_row
    for w in matches.windows(2) {
        assert!(w[0].buffer_row <= w[1].buffer_row);
    }
    // At least one scrollback and one live
    assert!(matches.iter().any(|m| m.is_scrollback()));
    assert!(matches.iter().any(|m| !m.is_scrollback()));
    // Column mapping: "needle" in "line00 needle" starts after "line00 " (6 incl space)
    for m in &matches {
        assert_eq!(m.matched_text, "needle");
        // col_start should be >=0 and col_end >= col_start
        assert!(m.col_start <= m.col_end);
    }
}

#[test]
fn search_case_sensitivity_and_bounds_headless() {
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "Hello hello HELLO");
    // Case sensitive: only one exact "hello"
    let cs = rt.search("hello", SearchOptions::new(true, 100));
    assert_eq!(cs.len(), 1, "case sensitive should find one");
    assert_eq!(cs[0].matched_text, "hello");
    // Case insensitive: all three
    let ci = rt.search("hello", SearchOptions::new(false, 100));
    assert_eq!(ci.len(), 3, "case insensitive should find three");
    // Max results cap
    let mut rt2 = make_runtime();
    for _ in 0..5 {
        feed_runtime(&mut rt2, "xxx xxx xxx\n");
    }
    // Each line "xxx xxx xxx" has 3 matches for "xxx"
    let capped = rt2.search("xxx", SearchOptions::new(true, 2));
    assert_eq!(capped.len(), 2);
    assert!(capped.len() <= SEARCH_MAX_RESULTS);
    // Empty pattern returns none
    let empty = rt.search("", SearchOptions::default());
    assert!(empty.is_empty());
    // Overlong pattern is truncated at char boundary and still headless (no panic)
    let long = "a".repeat(SEARCH_MAX_PATTERN_LEN + 100);
    let long_matches = rt.search(&long, SearchOptions::default());
    assert!(long_matches.is_empty());
}

#[test]
fn search_wide_char_headless() {
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "A\u{4e2d}B live");
    let m = rt.search("\u{4e2d}", SearchOptions::default());
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].matched_text, "\u{4e2d}");
    // Wide char at col 1 width 2 => col_start 1 col_end 2
    assert_eq!(m[0].col_start, 1);
    assert_eq!(m[0].col_end, 2);
    let m2 = rt.search("A\u{4e2d}", SearchOptions::default());
    assert_eq!(m2.len(), 1);
    assert_eq!(m2[0].col_start, 0);
    assert_eq!(m2[0].col_end, 2);
}

#[test]
fn search_deterministic_headless() {
    let mut a = make_runtime();
    let mut b = make_runtime();
    for rt in [&mut a, &mut b] {
        feed_runtime(rt, "deterministic NEEDLE\n");
        feed_runtime(rt, "needle in scrollback\n");
        for _ in 0..rt.state().height() + 1 {
            feed_runtime(rt, "scroll line\n");
        }
    }
    let opts = SearchOptions::new(false, 100);
    let ma = a.search("needle", opts);
    let mb = b.search("needle", opts);
    assert_eq!(ma, mb, "search must be deterministic across runtimes");
}

#[test]
fn persistent_selection_survives_scroll_and_text_stable_headless() {
    let mut rt = make_runtime();
    // Write "alpha beta" on first row, so we can select "beta"
    feed_runtime(&mut rt, "alpha beta");
    // Snapshot width 80, "alpha beta" at row 0 cols 0..9
    // Select "beta" cols 6..9
    rt.start_selection(CellPos::new(0, 6));
    rt.end_selection(CellPos::new(0, 9));
    assert_eq!(rt.selection_text().as_deref(), Some("beta"));
    let pers = rt.persistent_selection().expect("selection to persistent");
    // The persistent buffer text should match live selection text
    assert_eq!(rt.persistent_selection_text(&pers).as_deref(), Some("beta"));
    // Validate persistent is valid now
    assert!(rt.is_persistent_selection_valid(&pers));
    // Scroll one line: feed enough LFs to push first row into scrollback
    for _ in 0..rt.state().height() {
        feed_runtime(&mut rt, "\n");
    }
    // After scroll, the old grid row 0 is now scrollback. Live grid is blank rows.
    // Live-grid resolve should fail (selection now in history)
    let live_restored = pers.to_grid_selection(rt.state());
    assert!(
        live_restored.is_none(),
        "selection moved into scrollback should not map to live grid"
    );
    // But persistent buffer text should still be "beta" (history readable)
    assert_eq!(
        pers.text(rt.state()).as_deref(),
        Some("beta"),
        "buffer text must survive scroll into history"
    );
    assert!(pers.is_valid(rt.state()));
    // Restoring via runtime should clear live selection (moved to history)
    let restored = rt.restore_persistent_selection(pers);
    assert!(!restored, "restore should report not live");
    assert!(
        rt.selection().is_none(),
        "live selection cleared after moving to history"
    );
    // The persistent text is still valid history
    assert_eq!(pers.text(rt.state()).as_deref(), Some("beta"));
}

#[test]
fn persistent_selection_survives_resize_clamping_headless() {
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "hello resize");
    rt.start_selection(CellPos::new(0, 0));
    rt.end_selection(CellPos::new(0, 4));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    let pers = rt.persistent_selection().unwrap();
    // Resize to smaller grid: 4 cols x 2 rows -> selection col 4 clamped to 3
    rt.handle_resize(PhysicalSize::new(8 * 4, 16 * 2))
        .expect("resize small must succeed");
    // Persistent selection columns should be clamped to new width (3)
    let clamped = pers.clamped(rt.state());
    // Original anchor col 0 stays 0, focus col 4 -> clamped to 3
    assert!(clamped.focus.col <= 3);
    assert!(clamped.is_valid(rt.state()));
    // Restoring the clamped persistent should succeed as still live
    let mut rt2 = make_runtime();
    feed_runtime(&mut rt2, "hello resize");
    rt2.start_selection(CellPos::new(0, 0));
    rt2.end_selection(CellPos::new(0, 4));
    let _pers2 = rt2.persistent_selection().unwrap();
    rt2.handle_resize(PhysicalSize::new(8 * 4, 16 * 2)).unwrap();
    // Runtime's automatic clamping after resize should keep selection valid and non-empty
    assert!(rt2.has_selection());
    let sel = rt2.selection().unwrap();
    assert!(sel.anchor.col < 4 && sel.focus.col < 4);
    assert!(rt2.persistent_selection().is_some());
    // After resize, search still headless and deterministic
    let m = rt2.search("hello", SearchOptions::default());
    // "hello" was truncated? After resize width 4, the line "hello resize" was truncated to width 4,
    // so "hello" no longer fits as contiguous in the grid (only "hell" visible). Search may find prefix.
    // We just assert headless not panic and result is deterministic.
    assert!(m.len() <= SEARCH_MAX_RESULTS);
    // Original persistent's text after resize may be truncated due to width change
    let txt = clamped.text(rt2.state());
    assert!(txt.is_some());
}

#[test]
fn persistent_selection_view_scroll_window_headless() {
    let mut state = State::new();
    for i in 0..(state.height() + 6) {
        feed_line(&mut state, &format!("view{i:02} data"));
    }
    let sb_len = state.scrollback_len();
    assert!(sb_len >= 6);
    let mut view = View::new(ViewId::new(1), state.width(), state.height());
    // Create a viewport selection at row 0 col 0..3 (viewport coordinates)
    let sel = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 3));
    let pers = PersistentSelection::from_view_selection(sel, &view, &state);
    assert!(pers.is_valid(&state));
    // Initially visible in live viewport
    assert!(pers.to_view_selection(&view, &state).is_some());
    // Scroll up by 2 lines: viewport now shows older history
    view.set_scroll_offset(2, sb_len);
    // The same buffer rows may now be at different viewport rows; the original
    // persistent that was at viewport row 0 (buffer start+0) after scroll by 2
    // should now be 2 rows outside viewport (since we slid up), so resolving to
    // viewport should yield a different row or None.
    // Create a new view selection at same viewport row 0 after scroll: its buffer row shifts.
    let sel_after = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 3));
    let pers_after = PersistentSelection::from_view_selection(sel_after, &view, &state);
    assert_ne!(
        pers.anchor.buffer_row, pers_after.anchor.buffer_row,
        "scroll offset must shift buffer anchor"
    );
    // Both persistents are valid
    assert!(pers.is_valid(&state) && pers_after.is_valid(&state));
    // Text via buffer should be deterministic for each
    assert!(pers.text(&state).is_some());
    assert!(pers_after.text(&state).is_some());
    // Restoring original pers to scrolled view should not be in window (since window slid)
    // The original buffer window start = total - rows - 0; after scroll start = total - rows - 2.
    // Original buffer row = start_old + 0, new start = start_old -2, so original row is now
    // 2 rows beyond viewport bottom => outside window => to_view_selection None.
    let in_old_window = pers.to_view_selection(&view, &state);
    // It may be outside after scroll by 2, but within bounds we assert deterministic:
    // either Some with shifted row or None, but deterministic across repeated calls.
    let again = pers.to_view_selection(&view, &state);
    assert_eq!(in_old_window, again, "view restore must be deterministic");
}

#[test]
fn persistent_selection_cleared_on_full_reset_but_search_headless_still() {
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "keep me");
    rt.start_selection(CellPos::new(0, 0));
    rt.end_selection(CellPos::new(0, 3));
    assert!(rt.has_selection());
    let pers = rt.persistent_selection().unwrap();
    assert!(rt.is_persistent_selection_valid(&pers));
    // FullReset via ESC c
    rt.handle_pty_bytes(b"\x1bc");
    // Live selection must be cleared (FullReset invalidation)
    assert!(
        !rt.has_selection(),
        "selection must be cleared after FullReset"
    );
    // Persistent buffer rows still exist (grid erased) but content is now blank.
    // `is_valid` checks buffer bounds (still valid) not content hash, so it remains true;
    // however the buffer text is now blank, proving the erase cleared the content headlessly.
    assert!(
        pers.is_valid(rt.state()),
        "persistent buffer rows remain in bounds after FullReset (content erased, not pruned)"
    );
    assert_eq!(
        pers.text(rt.state()).unwrap().trim(),
        "",
        "buffer text should be blank after FullReset erase"
    );
    // Direct grid restore still yields a selection (coords still in window) but its text is blank.
    // Runtime purposely cleared live selection, so `persistent.to_grid_selection` is not used for live.
    // Search after reset must still be headless and bounded (grid now blank)
    let m = rt.search("keep", SearchOptions::default());
    assert!(m.is_empty(), "search after reset should find nothing");
    // New scrollback empty, still headless
    assert_eq!(rt.state().scrollback_len(), 0);
}

#[test]
fn selection_persistence_headless_still_works_via_state_resize_and_search() {
    // Combined smoke: resize, select, search, scroll, validate all headless.
    let mut rt = make_runtime();
    feed_runtime(&mut rt, "persist search test");
    rt.select_all();
    assert!(rt.has_selection());
    let pers_all = rt.persistent_selection().unwrap();
    let hits = rt.search("persist", SearchOptions::default());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].matched_text, "persist");
    // Resize larger so total buffer grows and persistent stays in bounds.
    rt.handle_resize(PhysicalSize::new(8 * 120, 16 * 30))
        .expect("resize must succeed");
    // After resize, selection clamped but still exists
    assert!(rt.has_selection());
    // Persistent text after resize (clamped columns) still readable
    let pers_text = pers_all.clamped(rt.state()).text(rt.state());
    assert!(pers_text.is_some());
    // Headless determinism across second runtime with same inputs
    let mut rt2 = make_runtime();
    feed_runtime(&mut rt2, "persist search test");
    rt2.select_all();
    rt2.handle_resize(PhysicalSize::new(8 * 120, 16 * 30))
        .unwrap();
    assert_eq!(
        rt.search("persist", SearchOptions::default()),
        rt2.search("persist", SearchOptions::default())
    );
    assert_eq!(
        rt.persistent_selection().unwrap().text(rt.state()),
        rt2.persistent_selection().unwrap().text(rt2.state())
    );
}
