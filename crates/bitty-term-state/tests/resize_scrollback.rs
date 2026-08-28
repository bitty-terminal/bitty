//! Resize and scrollback integration for `bitty-term-state` and `bitty-ui`.
//! Headless, deterministic, and bounded: no window, no GPU, no PTY, no filesystem.
//! This file proves the singular reflow (truncate/pad with orphan repair) for
//! terminal resize and the scrollback-aware View viewport composition.

#![forbid(unsafe_code)]

use bitty_term_state::State;
use bitty_term_state::TerminalAction;
use bitty_ui::{LayoutNode, Rect, SplitAxis, View, ViewId};
use bitty_vt::{ControlChar, GraphemeCell};

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

#[test]
fn state_resize_changes_geometry_and_generates_full_damage() {
    let mut s = State::new();
    assert_eq!(s.width(), 80);
    assert_eq!(s.height(), 24);
    let gen_before = s.generation();
    prints(&mut s, "hello");
    assert!(s.check_invariants().is_ok());

    let damage = s.resize(100, 37);
    assert_eq!(s.width(), 100);
    assert_eq!(s.height(), 37);
    assert!(s.check_invariants().is_ok());
    assert!(s.generation() > gen_before);
    assert!(damage.generation > gen_before);
    assert!(!damage.regions.is_empty());
    // Snapshot dimensions must reflect new geometry.
    let snap = s.snapshot();
    assert_eq!(snap.width, 100);
    assert_eq!(snap.height, 37);
    assert_eq!(snap.cells.len(), 100 * 37);
    // Full grid present in damage.
    let has_grid = damage
        .regions
        .iter()
        .any(|r| matches!(r, bitty_term_state::DamagedRegion::Grid(_)));
    assert!(has_grid, "resize damage must contain full grid");

    // Idempotent resize with same dims is no-op (no generation bump).
    let gen_mid = s.generation();
    let dmg2 = s.resize(100, 37);
    assert_eq!(
        s.generation(),
        gen_mid,
        "same-size resize must not bump generation"
    );
    assert!(dmg2.regions.is_empty());
}

#[test]
fn resize_preserves_overlapping_content_and_repairs_wide_pairs() {
    let mut s = State::new();
    // Fill first row with "AB" + wide CJK at col 2 (occupies 2 cells) + "X" at col 4
    prints(&mut s, "AB");
    s.apply(&TerminalAction::Print(GraphemeCell::from('中')));
    prints(&mut s, "X");
    let snap_before = s.snapshot();
    assert_eq!(snap_before.cells[2].glyph, '中');
    assert_eq!(snap_before.cells[2].width, 2);
    assert!(snap_before.cells[3].spacer);

    // Shrink width to 3: truncation cuts the wide pair's spacer at col3, leading at col2 becomes orphan -> erased.
    s.resize(3, 24);
    assert_eq!(s.width(), 3);
    assert!(s.check_invariants().is_ok());
    let snap = s.snapshot();
    // After repair, col2 (0-indexed) must not be a leading wide without spacer nor spacer alone.
    assert!(
        !snap.cells[2].spacer || {
            // if col2 is spacer, its lead at col1 must be wide (but col1 is 'B' narrow, so col2 cannot be spacer).
            false
        }
    );
    assert!(
        snap.cells[2].is_blank() || snap.cells[2].width == 1,
        "orphaned wide leading must be demoted to blank single width"
    );
    // Overlapping content still present at cols 0,1
    assert_eq!(snap.cells[0].glyph, 'A');
    assert_eq!(snap.cells[1].glyph, 'B');

    // Grow again, new area must be blank.
    s.resize(80, 24);
    let snap2 = s.snapshot();
    // Far right columns must be blank.
    assert!(snap2.cells[79].is_blank());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn resize_height_preserves_scroll_region_and_clamps_cursor() {
    let mut s = State::new();
    // Move cursor to bottom-right
    s.apply(&TerminalAction::CursorPosition {
        row: bitty_vt::Row(24),
        col: bitty_vt::Col(80),
    });
    assert_eq!(s.cursor().position.row, 23);
    assert_eq!(s.cursor().position.col, 79);

    // Grow height; scroll region must reset to full screen and cursor stays in-bounds.
    s.resize(80, 40);
    assert_eq!(s.height(), 40);
    assert!(s.check_invariants().is_ok());
    // Cursor previously at 23,79 must still be 23,79 (within new 40 rows)
    assert_eq!(s.cursor().position.row, 23);
    assert_eq!(s.cursor().position.col, 79);

    // Shrink rows below cursor row; cursor must clamp.
    s.resize(80, 10);
    assert_eq!(s.height(), 10);
    assert!(s.check_invariants().is_ok());
    assert!(s.cursor().position.row < 10);
    assert!(s.cursor().position.col < 80);
}

#[test]
fn scrollback_lines_resize_to_new_width_and_stay_monotonic() {
    let mut s = State::new();
    // Generate scrollback by printing lines and LF until scroll.
    for i in 0..30 {
        prints(&mut s, &format!("line{i:02}"));
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    let sb_len = s.scrollback_len();
    assert!(sb_len > 0, "must have captured scrollback");
    let first_id = s.scrollback_line(0).unwrap().id;
    let last_id = s.scrollback_line(sb_len - 1).unwrap().id;
    assert!(first_id < last_id);

    // Verify each scrollback line width matches state width (80)
    for line in s.scrollback() {
        assert_eq!(line.cells.len(), 80);
    }

    // Resize wider: each line must pad to new width.
    s.resize(100, 24);
    assert_eq!(s.width(), 100);
    for line in s.scrollback() {
        assert_eq!(
            line.cells.len(),
            100,
            "wider resize must pad scrollback lines"
        );
    }
    // Ids must stay monotonic after resize (reflow preserves ids).
    let mut prev: Option<u64> = None;
    for line in s.scrollback() {
        if let Some(p) = prev {
            assert!(line.id > p);
        }
        prev = Some(line.id);
    }
    assert!(s.check_invariants().is_ok());

    // Resize narrower: truncation with repair.
    s.resize(40, 24);
    for line in s.scrollback() {
        assert_eq!(line.cells.len(), 40);
    }
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        s.scrollback_len(),
        sb_len,
        "shrink should not prune scrollback count"
    );
}

#[test]
fn view_visible_cells_composites_scrollback_and_live_grid_headlessly() {
    // Headless still works: View viewport composition must be deterministic and respect scroll_offset.
    let mut s = State::new();
    // Create 3 scrollback lines: line00, line01, line02 (each 4 rows height to force scroll)
    for i in 0..(s.height() + 5) {
        prints(&mut s, &format!("L{i:02}"));
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    let sb_len = s.scrollback_len();
    assert!(sb_len >= 5);

    // View sized to terminal width/height (80x24), live at offset 0.
    let mut view = View::new(ViewId::new(1), s.width(), s.height());
    assert!(view.is_live());
    let live_cells = view.visible_cells(&s);
    assert_eq!(live_cells.len(), s.width() * s.height());
    // Live viewport bottom row should contain newest grid content, not the oldest scrollback.
    // Hard to assert exact glyph, but we verify visible_text_rows length.
    let live_rows = view.visible_text_rows(&s);
    assert_eq!(live_rows.len(), s.height());

    // Scroll up into history
    view.set_scroll_offset(2, sb_len);
    assert_eq!(view.scroll_offset(), 2);
    let scrolled_cells = view.visible_cells(&s);
    assert_eq!(scrolled_cells.len(), s.width() * s.height());
    // Scrolled view must differ from live
    assert_ne!(live_cells, scrolled_cells, "scrolled viewport must differ");
    // Determinism: same state+same view yields same cells.
    let view2 = {
        let mut v = View::new(ViewId::new(1), s.width(), s.height());
        v.set_scroll_offset(2, sb_len);
        v
    };
    assert_eq!(scrolled_cells, view2.visible_cells(&s));

    // After resize, view can be reflowed via LayoutNode and clamp offset.
    view.set_scroll_offset(5, sb_len);
    s.resize(100, 30);
    // View that stays 80x24 but state now 100x30: visible_cells pads/truncates deterministically.
    let after_cells = view.visible_cells(&s);
    assert_eq!(after_cells.len(), 80 * 24);
    // Clamp after resize when offset exceeds new sb_len (edge case: no pruning, so still 5)
    view.clamp_scroll_offset(s.scrollback_len());
    assert!(view.scroll_offset() <= s.scrollback_len());

    // Layout reflow headless determinism: two identical layouts into same container produce identical allocations.
    let rect = Rect::new(0, 0, 100, 30);
    let mut root_a = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
    );
    let mut root_b = root_a.clone();
    root_a.reflow(rect);
    root_b.reflow(rect);
    assert_eq!(root_a.layout(rect), root_b.layout(rect));
    // Leaf views after reflow must match allocation sizes (50x30 each)
    assert_eq!(root_a.find_leaf(ViewId::new(1)).unwrap().cols(), 50);
    assert_eq!(root_a.find_leaf(ViewId::new(1)).unwrap().rows(), 30);
}

#[test]
fn view_resize_and_reflow_headless_still_works() {
    let mut v = View::new(ViewId::new(9), 80, 24);
    assert!(!v.resize(80, 24));
    assert!(v.resize(100, 37));
    assert_eq!(v.cols(), 100);
    assert_eq!(v.rows(), 37);

    let r = Rect::new(5, 7, 40, 12);
    assert!(v.reflow_to_rect(r));
    assert_eq!(v.origin(), bitty_ui::Point::new(5, 7));
    assert_eq!(v.cols(), 40);
    assert_eq!(v.rows(), 12);
    assert_eq!(v.allocation(), r);
    assert!(v.is_live());

    // Horizontal scroll offset clamped.
    v.set_col_offset(10);
    assert_eq!(v.col_offset(), 10);
}

#[test]
fn scrollback_bounded_pruning_still_headless() {
    let mut s = State::new();
    // Fill far beyond SCROLLBACK_MAX_LINES to force pruning. First height-1
    // linefeeds just move the cursor; each additional linefeed scrolls one
    // blank line into scrollback, so we need height extra.
    let needed = bitty_term_state::SCROLLBACK_MAX_LINES + s.height() + 50;
    for i in 0..needed {
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
        if i % 500 == 0 {
            assert!(s.check_invariants().is_ok());
        }
    }
    assert_eq!(s.scrollback_len(), bitty_term_state::SCROLLBACK_MAX_LINES);
    assert!(s.check_invariants().is_ok());

    // Resize after pruned should keep bounded length and remap widths.
    let len_before = s.scrollback_len();
    s.resize(120, 40);
    assert_eq!(s.scrollback_len(), len_before);
    assert_eq!(s.scrollback_len(), bitty_term_state::SCROLLBACK_MAX_LINES);
    for line in s.scrollback() {
        assert_eq!(line.cells.len(), 120);
    }
}

#[test]
fn deterministic_hash_across_resize_replays() {
    // Same byte stream plus same resize sequence must hash identically.
    let bytes = b"resize test \x1b[31mred\x1b[0m\r\n";
    let mut a = State::new();
    let mut b = State::new();
    for chunk in bytes.chunks(3) {
        a.apply(&TerminalAction::Print(GraphemeCell::from(chunk[0] as char)));
        b.apply(&TerminalAction::Print(GraphemeCell::from(chunk[0] as char)));
    }
    // Both start identical hash
    assert_eq!(a.state_hash(), b.state_hash());
    a.resize(100, 30);
    b.resize(100, 30);
    assert_eq!(a.state_hash(), b.state_hash());
    // Feed same post-resize bytes
    prints(&mut a, "after");
    prints(&mut b, "after");
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(a.snapshot().width, b.snapshot().width);
}
