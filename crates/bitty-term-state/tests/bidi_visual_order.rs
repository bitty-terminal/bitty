//! Visual-order mapping over real grid cells (CTX-1612, #1766).
//!
//! `BidiGrid` is a pure projection of logical cells: it never mutates
//! Terminal Truth, and a line without strong RTL resolves to no maps at
//! all (the renderer's LTR fast path). These tests pin the column maps, the
//! wide-pair atomicity, cursor arithmetic (logical), explicit marks, and the
//! bounded over-cap degradation.

use bitty_term_state::{
    BidiGrid, Cell, MAX_BIDI_PARAGRAPH_CELLS, ParaDirection, Snapshot, State, TerminalAction,
    resolve_text,
};
use bitty_vt::{ControlChar, Count, Direction, GraphemeCell};

fn print_text(state: &mut State, text: &str) {
    for scalar in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(scalar)));
    }
}

fn state_with_text(text: &str) -> State {
    let mut state = State::new();
    print_text(&mut state, text);
    state
}

fn grid_for(snapshot: &Snapshot) -> BidiGrid {
    BidiGrid::resolve(
        &snapshot.cells,
        snapshot.width,
        snapshot.height,
        None,
        ParaDirection::Auto,
    )
}

fn row_text(snapshot: &Snapshot, row: usize, cols: usize) -> String {
    let start = row * snapshot.width;
    snapshot.cells[start..start + cols]
        .iter()
        .map(|cell| cell.glyph)
        .collect()
}

#[test]
fn pure_ltr_line_has_no_maps_and_identity_columns() {
    let state = state_with_text("plain ascii");
    let snapshot = state.snapshot();
    let grid = grid_for(&snapshot);
    assert!(grid.is_empty());
    for col in 0..snapshot.width {
        assert_eq!(grid.logical_col_for_visual(0, col), col);
        assert_eq!(grid.visual_col_for_logical(0, col), col);
    }
}

#[test]
fn mixed_ltr_rtl_line_reverses_only_the_rtl_run() {
    let state = state_with_text("abc אבג");
    let snapshot = state.snapshot();
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL run");
    assert_eq!(line.paragraph_level(), 0, "first strong is LTR");
    assert!(line.has_rtl());
    // Logical: a b c space א ב ג. Visual: a b c space ג ב א.
    let expected: [(usize, usize); 7] = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 6), (5, 5), (6, 4)];
    for (logical, visual) in expected {
        assert_eq!(
            line.visual_col_for_logical(logical),
            visual,
            "logical {logical}"
        );
        assert_eq!(
            line.logical_col_for_visual(visual),
            logical,
            "visual {visual}"
        );
    }
    // The map is a permutation of the row.
    let mut seen = vec![false; snapshot.width];
    for visual in 0..snapshot.width {
        seen[line.logical_col_for_visual(visual)] = true;
    }
    assert!(seen.iter().all(|seen| *seen));
    assert_eq!(row_text(&snapshot, 0, 7), "abc אבג");
    // Edge inverse: visual edge 5 is the boundary before א (logical 6),
    // not cell 5; the line end is visual 4 (logical 7).
    assert_eq!(line.logical_caret_edge_for_visual(5), 6);
    assert_eq!(line.logical_caret_edge_for_visual(4), 7);
    assert_eq!(line.logical_caret_edge_for_visual(3), 3);
}

#[test]
fn rtl_paragraph_keeps_the_latin_run_ltr_and_reverses_the_hebrew_run() {
    let state = state_with_text("אבג abc");
    let snapshot = state.snapshot();
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL paragraph");
    assert_eq!(line.paragraph_level(), 1);
    // Logical: א ב ג space a b c. Visual: a b c space ג ב א.
    assert_eq!(line.visual_col_for_logical(0), 6);
    assert_eq!(line.visual_col_for_logical(1), 5);
    assert_eq!(line.visual_col_for_logical(2), 4);
    assert_eq!(line.visual_col_for_logical(3), 3);
    assert_eq!(line.visual_col_for_logical(4), 0);
    assert_eq!(line.visual_col_for_logical(5), 1);
    assert_eq!(line.visual_col_for_logical(6), 2);
    assert_eq!(line.logical_col_for_visual(0), 4);
    assert_eq!(line.logical_col_for_visual(6), 0);
    // Caret edges: before the last logical cell sits at its left edge; the
    // end-of-line caret follows the paragraph direction.
    assert_eq!(line.visual_caret_col(0), 7);
    assert_eq!(line.visual_caret_col(4), 0);
    assert_eq!(line.visual_caret_col(7), 3);
    assert_eq!(
        line.visual_caret_col(9),
        9,
        "padding beyond end is identity"
    );
    // Edge inverse for the RTL paragraph: visual edge 5 is the boundary
    // between ג and ב (logical 2); visual edge 3 is the line end (7).
    assert_eq!(line.logical_caret_edge_for_visual(5), 2);
    assert_eq!(line.logical_caret_edge_for_visual(4), 3);
    assert_eq!(line.logical_caret_edge_for_visual(3), 7);
    // Outside content stays identity.
    assert_eq!(line.logical_caret_edge_for_visual(12), 12);
}

#[test]
fn pure_rtl_edges_invert() {
    let state = state_with_text("אבג");
    let snapshot = state.snapshot();
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL line");
    // Logical edges 0..3 map to visual edges 3,2,1,0.
    assert_eq!(line.logical_caret_edge_for_visual(3), 0);
    assert_eq!(line.logical_caret_edge_for_visual(2), 1);
    assert_eq!(line.logical_caret_edge_for_visual(1), 2);
    assert_eq!(line.logical_caret_edge_for_visual(0), 3);
}

#[test]
fn wide_pair_moves_as_one_unit_in_rtl_text() {
    // א (1 cell) 中 (2 cells, spacer at 2) ב (1 cell): visual order
    // reverses the leads, and the pair keeps its halves adjacent.
    let state = state_with_text("א中ב");
    let snapshot = state.snapshot();
    let cells = &snapshot.cells[..4];
    assert_eq!(cells[0].glyph, 'א');
    assert_eq!(cells[1].glyph, '中');
    assert_eq!(cells[1].width, 2);
    assert!(cells[2].spacer, "wide spacer follows the CJK lead");
    assert_eq!(cells[3].glyph, 'ב');
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL line");
    // Leads 0,1,3 become visual positions 2,1,0; the spacer is 3.
    assert_eq!(line.visual_col_for_logical(1), 1);
    assert_eq!(
        line.visual_col_for_logical(2),
        2,
        "the spacer stays to the right of its lead"
    );
    assert_eq!(line.logical_col_for_visual(1), 1);
    assert_eq!(line.logical_col_for_visual(2), 2);
    assert_eq!(line.visual_span_start(1, 2), 1);
}

#[test]
fn trailing_wide_cell_caret_uses_the_writing_direction_edge() {
    // אב then a wide 中 at the logical end: visual is 中 ב א, and the
    // end-of-line caret sits at the left edge of the wide pair.
    let state = state_with_text("אב中");
    let snapshot = state.snapshot();
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL line");
    assert_eq!(line.visual_col_for_logical(2), 0);
    assert_eq!(line.visual_col_for_logical(3), 1, "spacer half");
    // 中 resolved level 2 (LTR inside the RTL paragraph), so the caret after
    // it sits at its right visual edge.
    assert_eq!(line.visual_caret_col(4), 2);
}

#[test]
fn explicit_isolate_is_zero_width_and_does_not_consume_a_cell() {
    let state = state_with_text("x\u{2067}אבג\u{2069}y");
    let snapshot = state.snapshot();
    let cells = &snapshot.cells[..5];
    assert_eq!(cells[0].glyph, 'x');
    assert_eq!(cells[0].zerowidth.as_slice(), &['\u{2067}']);
    assert_eq!(cells[1].glyph, 'א', "the isolate consumes no cell");
    assert_eq!(cells[3].glyph, 'ג');
    assert_eq!(cells[3].zerowidth.as_slice(), &['\u{2069}']);
    assert_eq!(cells[4].glyph, 'y');
    assert_eq!(state.snapshot().cursor.position.col, 5);
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("isolated RTL run");
    // Visual: x ג ב א y; only the isolated Hebrew run reverses.
    assert_eq!(line.visual_col_for_logical(0), 0);
    assert_eq!(line.visual_col_for_logical(1), 3);
    assert_eq!(line.visual_col_for_logical(2), 2);
    assert_eq!(line.visual_col_for_logical(3), 1);
    assert_eq!(line.visual_col_for_logical(4), 4);
}

#[test]
fn leading_bidi_control_is_dropped_instead_of_crossing_a_line_break() {
    let mut state = state_with_text("abc");
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    print_text(&mut state, "\u{2067}אבג");
    let snapshot = state.snapshot();
    let first: String = snapshot.cells[..3].iter().map(|cell| cell.glyph).collect();
    assert_eq!(first, "abc");
    assert!(
        !snapshot.cells[..snapshot.width]
            .iter()
            .any(|cell| cell.zerowidth.as_slice().contains(&'\u{2067}')),
        "a line-leading isolate must not reorder the previous line"
    );
    // The Hebrew still occupies three cells on row 1.
    let row1 = &snapshot.cells[snapshot.width..snapshot.width + 3];
    assert_eq!(row1[0].glyph, 'א');
    assert_eq!(row1[1].glyph, 'ב');
    assert_eq!(row1[2].glyph, 'ג');
}

#[test]
fn cursor_movement_stays_logical_across_visual_runs() {
    let mut state = state_with_text("abc אבג");
    assert_eq!(state.snapshot().cursor.position.col, 7);
    // CUB moves one logical column even though it may jump a visual run.
    for _ in 0..3 {
        state.apply(&TerminalAction::PrintControl(ControlChar(0x08)));
    }
    assert_eq!(state.snapshot().cursor.position.col, 4);
    let snapshot = state.snapshot();
    let grid = grid_for(&snapshot);
    assert_eq!(
        grid.visual_col_for_logical(0, 4),
        6,
        "logical first Hebrew letter paints rightmost"
    );
    // CUF moves right in logical order again.
    state.apply(&TerminalAction::CursorMove {
        dir: Direction::Right,
        n: Count(1),
    });
    assert_eq!(state.snapshot().cursor.position.col, 5);
}

#[test]
fn over_cap_paragraph_resolves_in_bounded_chunks() {
    // 3 chunks of Hebrew: no panic, maps stay a permutation per row.
    let text: String = std::iter::repeat_n('א', MAX_BIDI_PARAGRAPH_CELLS * 3 + 1).collect();
    let cells: Vec<Cell> = text
        .chars()
        .map(|glyph| {
            let mut cell = Cell::erased(bitty_term_state::Style::default());
            cell.glyph = glyph;
            cell.padding = false;
            cell
        })
        .collect();
    let width = cells.len();
    let grid = BidiGrid::resolve(&cells, width, 1, None, ParaDirection::Auto);
    let line = grid.line(0).expect("RTL row");
    let mut seen = vec![false; width];
    for visual in 0..width {
        let logical = line.logical_col_for_visual(visual);
        assert!(logical < width);
        assert!(!seen[logical], "permutation");
        seen[logical] = true;
    }
    assert!(seen.into_iter().all(|seen| seen));
}

#[test]
fn soft_wraps_resolve_one_logical_line_but_reorder_per_physical_row() {
    // A soft-wrapped RTL paragraph: levels come from the whole line, while
    // each physical row is reordered independently.
    let mut state = State::new();
    let text: String = std::iter::repeat_n('א', 100).collect();
    print_text(&mut state, &text);
    let snapshot = state.snapshot();
    let wraps: Vec<bool> = (0..snapshot.height)
        .map(|row| state.row_soft_wrapped(row))
        .collect();
    assert!(wraps[0], "row 0 soft-wraps into row 1");
    let grid = BidiGrid::resolve(
        &snapshot.cells,
        snapshot.width,
        snapshot.height,
        Some(&wraps),
        ParaDirection::Auto,
    );
    let row0 = grid.line(0).expect("row 0 reorders");
    let row1 = grid.line(1).expect("row 1 reorders");
    assert!(row0.has_rtl() && row1.has_rtl());
    assert_eq!(row0.paragraph_level(), 1);
    assert_eq!(row1.paragraph_level(), 1);
    // Each physical row reverses its own content span: row 0 is full, row
    // 1 holds the remaining 20 cells of the same logical line.
    assert_eq!(row0.visual_col_for_logical(0), snapshot.width - 1);
    assert_eq!(row1.visual_col_for_logical(0), 19);
}

#[test]
fn text_level_resolution_matches_known_uax9_examples() {
    // RTL paragraph with a Latin run: the run stays LTR inside the RTL line.
    let resolved = resolve_text("אבג abc", ParaDirection::Rtl).expect("text");
    assert_eq!(resolved.paragraph_level, 1);
    assert_eq!(resolved.visual, vec![4, 5, 6, 3, 2, 1, 0]);
    // Forced LTR on the same text puts the Hebrew run at the right.
    let forced = resolve_text("אבג abc", ParaDirection::Ltr).expect("text");
    assert_eq!(forced.paragraph_level, 0);
    assert_eq!(forced.visual, vec![2, 1, 0, 3, 4, 5, 6]);
}

#[test]
fn bidi_control_at_column_zero_follows_the_break_kind() {
    // Soft wrap: the next row is still the same logical line, so a control
    // at its column 0 joins the previous row's last cell.
    let mut state = State::new();
    print_text(&mut state, &"a".repeat(81));
    assert!(state.row_soft_wrapped(0));
    state.apply(&TerminalAction::CursorPosition {
        row: bitty_vt::Row(2),
        col: bitty_vt::Col(1),
    });
    print_text(&mut state, "\u{2067}b");
    let snapshot = state.snapshot();
    assert_eq!(snapshot.cells[79].glyph, 'a');
    assert_eq!(snapshot.cells[79].zerowidth.as_slice(), &['\u{2067}']);
    // Hard break: the control is line-leading and dropped, never attached
    // to the previous line.
    let mut state = State::new();
    print_text(&mut state, &"a".repeat(80));
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    print_text(&mut state, "\u{2067}b");
    let snapshot = state.snapshot();
    assert!(snapshot.cells[79].zerowidth.is_empty());
    assert_eq!(snapshot.cells[80].glyph, 'b');
}

#[test]
fn nested_embeddings_and_isolates_stay_zero_width() {
    let state = state_with_text("x\u{202A}a\u{202E}b\u{202C}\u{2067}אב\u{2069}c\u{202C}y");
    let snapshot = state.snapshot();
    let text: String = snapshot.cells[..3].iter().map(|cell| cell.glyph).collect();
    assert_eq!(text, "xab", "controls never consume a cell");
    assert!(snapshot.cells[0].zerowidth.as_slice().contains(&'\u{202A}'));
    let before_isolate = snapshot
        .cells
        .iter()
        .find(|cell| cell.glyph == 'b')
        .expect("cell before the isolate");
    assert!(
        before_isolate.zerowidth.as_slice().contains(&'\u{2067}'),
        "RLI stays on the preceding cell tail"
    );
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL isolate");
    assert!(line.has_rtl());
}

#[test]
fn wide_and_combining_clusters_move_as_one_unit() {
    let state = state_with_text("א中\u{0301}ב");
    let snapshot = state.snapshot();
    let cells = &snapshot.cells[..4];
    assert_eq!(cells[0].glyph, 'א');
    assert_eq!(cells[1].glyph, '中');
    assert_eq!(cells[1].width, 2);
    assert!(cells[2].spacer);
    assert_eq!(cells[1].zerowidth.as_slice(), &['\u{0301}']);
    assert_eq!(cells[3].glyph, 'ב');
    let grid = grid_for(&snapshot);
    let line = grid.line(0).expect("RTL line");
    assert_eq!(line.visual_col_for_logical(1), 1);
    assert_eq!(line.visual_col_for_logical(2), 2);
    assert_eq!(
        line.visual_caret_col(4),
        0,
        "end caret before the wide pair"
    );
}

#[test]
fn over_width_rows_fail_closed_to_identity() {
    let width = usize::from(u16::MAX) + 1;
    let text: String = std::iter::repeat_n('א', width).collect();
    let cells: Vec<Cell> = text
        .chars()
        .map(|glyph| {
            let mut cell = Cell::erased(bitty_term_state::Style::default());
            cell.glyph = glyph;
            cell.padding = false;
            cell
        })
        .collect();
    let grid = BidiGrid::resolve(&cells, width, 1, None, ParaDirection::Auto);
    assert!(grid.line(0).is_none(), "u16 maps must fail closed");
    assert_eq!(grid.logical_col_for_visual(0, 12_345), 12_345);
}

#[test]
fn resolving_never_mutates_the_snapshot() {
    let state = state_with_text("abc אבג");
    let before = state.snapshot();
    let hash_before = state.state_hash();
    let _grid = grid_for(&before);
    let after = state.snapshot();
    assert_eq!(before.cells, after.cells);
    assert_eq!(hash_before, state.state_hash());
    assert_eq!(
        bitty_term_state::canonical_public::CANONICAL_HASH_VERSION,
        13
    );
}
