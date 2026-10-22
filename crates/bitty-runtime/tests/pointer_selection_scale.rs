//! Pointer→cell mapping and caret selection at DPI scale 1 and 2 (issue #2480).
//!
//! The reported dogfooding symptom was a two-cell selection stride over
//! scaled terminal text, which looked like a physical/logical pixel mix. The
//! tests here prove the mapping end to end instead of assuming it:
//!
//! - adjacent cell centres resolve to adjacent columns at both scales;
//! - one cell of pointer travel selects exactly one cell (forward and
//!   backward), so a single character is selectable;
//! - a drag within one cell also selects that single cell;
//! - a plain click still clears (no accidental one-cell highlight).
//!
//! Coordinates are derived from `present_frames` (`content` origin),
//! `window_padding_physical`, and `live_cell_size`, never from hard-coded
//! pixel constants, so padding, decoration, and DPI rounding stay covered.

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PhysicalSize, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{PresentFrame, Runtime, RuntimeConfig};
use bitty_ui::CellPos;

fn runtime_at(scale: f64) -> Runtime {
    runtime_with_text(scale, b"abcdef")
}

fn runtime_with_text(scale: f64, text: &[u8]) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime builds");
    let physical = PhysicalSize::new((800.0 * scale) as u32, (600.0 * scale) as u32);
    rt.apply_dpi_scale(scale, Some(physical));
    rt.handle_pty_bytes(text);
    rt
}

fn primary_frame(rt: &Runtime) -> PresentFrame {
    *rt.present_frames().first().expect("primary frame")
}

/// Absolute physical x of a cell edge (`col == 0` is the content left edge).
fn cell_edge_x(rt: &Runtime, frame: &PresentFrame, col: u16) -> f64 {
    let (cell_w, _) = rt.live_cell_size();
    f64::from(rt.window_padding_physical())
        + f64::from(frame.content.x)
        + f64::from(col) * f64::from(cell_w)
}

/// Absolute physical y of a cell edge (`row == 0` is the content top edge).
fn cell_edge_y(rt: &Runtime, frame: &PresentFrame, row: u16) -> f64 {
    let (_, cell_h) = rt.live_cell_size();
    f64::from(rt.window_padding_physical())
        + f64::from(frame.content.y)
        + f64::from(row) * f64::from(cell_h)
}

fn at(rt: &Runtime, frame: &PresentFrame, col: u16, row: u16) -> CursorPosition {
    let (cell_w, cell_h) = rt.live_cell_size();
    CursorPosition {
        x: cell_edge_x(rt, frame, col) + 0.5 * f64::from(cell_w),
        y: cell_edge_y(rt, frame, row) + 0.5 * f64::from(cell_h),
    }
}

/// A point on the left edge of `col`, inside the top quarter of `row`.
///
/// The quarter-row keeps the caret in the row above the midpoint, so a
/// horizontal drag stays on one row while the x edge stays exact.
fn left_edge(rt: &Runtime, frame: &PresentFrame, col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: cell_edge_x(rt, frame, col),
        y: cell_edge_y(rt, frame, row) + 0.25 * f64::from(rt.live_cell_size().1),
    }
}

/// The centre of `col`, inside the top quarter of `row`.
fn center_in_row(rt: &Runtime, frame: &PresentFrame, col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: cell_edge_x(rt, frame, col) + 0.5 * f64::from(rt.live_cell_size().0),
        y: cell_edge_y(rt, frame, row) + 0.25 * f64::from(rt.live_cell_size().1),
    }
}

fn window_event(kind: WindowEventKind) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind,
    }
}

fn drag(rt: &mut Runtime, start: CursorPosition, waypoints: &[CursorPosition]) {
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(start)));
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
        MouseButton::Left,
        PressState::Pressed,
    ))));
    for pos in waypoints {
        rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(*pos)));
    }
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
        MouseButton::Left,
        PressState::Released,
    ))));
}

#[test]
fn adjacent_cells_map_to_adjacent_columns_at_scale_one() {
    adjacent_cells_map_to_adjacent_columns(1.0);
}

#[test]
fn adjacent_cells_map_to_adjacent_columns_at_scale_two() {
    adjacent_cells_map_to_adjacent_columns(2.0);
}

fn adjacent_cells_map_to_adjacent_columns(scale: f64) {
    let rt = runtime_at(scale);
    let frame = primary_frame(&rt);
    let view = frame.view;
    let mut previous = None;
    for col in 0..10u16 {
        let pos = at(&rt, &frame, col, 0);
        let (hit_view, cell) = rt
            .cursor_to_present_cell(pos)
            .unwrap_or_else(|| panic!("scale {scale}: col {col} must hit the grid"));
        assert_eq!(hit_view, view, "scale {scale}: col {col} owner");
        assert_eq!(cell, CellPos::new(0, col), "scale {scale}: col {col}");
        if let Some(prev) = previous {
            assert_eq!(
                cell.col,
                prev + 1,
                "scale {scale}: adjacent cells must map to adjacent columns"
            );
        }
        previous = Some(cell.col);
    }
}

#[test]
fn adjacent_cells_map_to_adjacent_columns_at_fractional_scales() {
    // The issue acceptance names 125%/150% as well; both round the scaled
    // cell to whole physical pixels, which is where a stride bug would hide.
    for scale in [1.25, 1.5] {
        adjacent_cells_map_to_adjacent_columns(scale);
    }
}

#[test]
fn one_cell_drag_selects_one_cell_at_scale_one() {
    one_cell_drag_selects_one_cell(1.0);
}

#[test]
fn one_cell_drag_selects_one_cell_at_scale_two() {
    one_cell_drag_selects_one_cell(2.0);
}

#[test]
fn one_cell_drag_selects_one_cell_at_fractional_scales() {
    for scale in [1.25, 1.5] {
        one_cell_drag_selects_one_cell(scale);
    }
}

fn one_cell_drag_selects_one_cell(scale: f64) {
    // One full cell of pointer travel, left edge to left edge: exactly the
    // first cell is selected (previously the inclusive-cell mapping selected
    // two cells for the same travel).
    let mut rt = runtime_at(scale);
    let frame = primary_frame(&rt);
    let start = left_edge(&rt, &frame, 1, 0);
    let end = left_edge(&rt, &frame, 2, 0);
    drag(&mut rt, start, &[end]);
    assert_eq!(
        rt.selection().map(|sel| sel.normalized()),
        Some(bitty_ui::SelectionRange::new(
            CellPos::new(0, 1),
            CellPos::new(0, 1)
        )),
        "scale {scale}: a one-cell drag must keep a one-cell selection"
    );
    assert!(rt.has_selection(), "scale {scale}: one cell is a selection");
    assert!(
        rt.selection_is_single_cell(),
        "scale {scale}: the collapsed inclusive range is deliberate"
    );
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("b"),
        "scale {scale}: exactly one character"
    );
    // The one-cell selection must paint (the inclusive range is collapsed,
    // so a plain `is_empty` guard would have skipped the highlight).
    let stats = rt.tick().expect("selection must force a present");
    assert!(
        stats.fills > 0,
        "scale {scale}: the single-cell highlight must paint"
    );

    // The same travel backwards selects the cell under the start edge too.
    let mut rt = runtime_at(scale);
    let frame = primary_frame(&rt);
    let start = left_edge(&rt, &frame, 2, 0);
    let end = left_edge(&rt, &frame, 1, 0);
    drag(&mut rt, start, &[end]);
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("b"),
        "scale {scale}: backward one-cell drag"
    );
}

#[test]
fn drag_within_one_cell_selects_that_character() {
    // A drag inside one cell (left half to right half) is a real one-cell
    // selection: this is the "single character cannot be selected" fix.
    let mut rt = runtime_at(1.0);
    let frame = primary_frame(&rt);
    let start = left_edge(&rt, &frame, 1, 0);
    let end = center_in_row(&rt, &frame, 1, 0);
    drag(&mut rt, start, &[end]);
    assert_eq!(rt.selection_text().as_deref(), Some("b"));
    assert!(rt.selection_is_single_cell());
}

#[test]
fn multi_cell_drag_selects_one_cell_per_cell_of_travel() {
    // Three cells of travel select exactly three cells, at both scales.
    for scale in [1.0, 2.0] {
        let mut rt = runtime_at(scale);
        let frame = primary_frame(&rt);
        let start = left_edge(&rt, &frame, 1, 0);
        let end = left_edge(&rt, &frame, 4, 0);
        drag(&mut rt, start, &[end]);
        assert_eq!(
            rt.selection_text().as_deref(),
            Some("bcd"),
            "scale {scale}: three cells of travel, three characters"
        );
        assert!(!rt.selection_is_single_cell());
    }
}

#[test]
fn wide_glyph_still_selects_as_one_character() {
    // Wide cells remain leader + spacer; the caret model must snap the
    // spacer endpoint back to the leader so one character is selected
    // without splitting the pair (issue #2480 acceptance).
    let mut rt = runtime_with_text(2.0, "A\u{4e2d}B".as_bytes());
    let frame = primary_frame(&rt);
    let start = left_edge(&rt, &frame, 1, 0);
    let end = left_edge(&rt, &frame, 3, 0);
    drag(&mut rt, start, &[end]);
    assert_eq!(rt.selection_text().as_deref(), Some("\u{4e2d}"));
    assert!(rt.selection_is_single_cell());
}

#[test]
fn vertical_block_drag_keeps_one_column() {
    // A block drag straight down must still select a one-column rectangle
    // (caret columns are equal, so the zero-width edge case is deliberate).
    let mut rt = runtime_with_text(1.0, b"abcd\r\nefgh\r\nijkl");
    let frame = primary_frame(&rt);
    rt.handle_platform_event(window_event(WindowEventKind::ModifiersChanged(
        bitty_platform::ModifiersState {
            shift: false,
            control: false,
            alt: true,
            super_pressed: false,
        },
    )));
    let start = left_edge(&rt, &frame, 1, 0);
    let end = left_edge(&rt, &frame, 1, 2);
    drag(&mut rt, start, &[end]);
    assert_eq!(rt.selection_text().as_deref(), Some("b\nf\nj"));
}

#[test]
fn plain_click_still_clears_the_selection() {
    let mut rt = runtime_at(2.0);
    let frame = primary_frame(&rt);
    let pos = left_edge(&rt, &frame, 2, 0);
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(pos)));
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
        MouseButton::Left,
        PressState::Pressed,
    ))));
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
        MouseButton::Left,
        PressState::Released,
    ))));
    assert!(!rt.has_selection(), "a click without travel clears");
}

#[test]
fn pointer_over_selectable_text_tracks_grid_and_chrome() {
    let mut rt = runtime_at(1.0);
    let frame = primary_frame(&rt);
    let inside = at(&rt, &frame, 2, 1);
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(inside)));
    assert!(rt.pointer_over_selectable_text());

    // The window padding is chrome, not text.
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(CursorPosition {
        x: 1.0,
        y: 1.0,
    })));
    assert!(!rt.pointer_over_selectable_text());

    // A modal panel overlay owns the pointer: the grid beneath is not
    // selectable text.
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(inside)));
    rt.set_overlay_modal_active(true);
    assert!(!rt.pointer_over_selectable_text());
    rt.set_overlay_modal_active(false);
    assert!(rt.pointer_over_selectable_text());

    // Leaving clears the predicate until the next motion.
    rt.handle_platform_event(window_event(WindowEventKind::CursorLeft));
    assert!(!rt.cursor_inside_window());
    assert!(!rt.pointer_over_selectable_text());
}

#[test]
fn pointer_over_text_is_arrow_while_a_pane_captures_the_mouse() {
    let mut rt = runtime_at(1.0);
    let frame = primary_frame(&rt);
    let inside = at(&rt, &frame, 2, 1);
    // Enable mouse tracking (`?1000h` + SGR `?1006h`): the app owns the
    // pointer, so selectable-text styling must not apply.
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(inside)));
    assert!(!rt.pointer_over_selectable_text());

    // Shift is the accessibility escape: text is selectable again.
    rt.handle_platform_event(window_event(WindowEventKind::ModifiersChanged(
        bitty_platform::ModifiersState {
            shift: true,
            control: false,
            alt: false,
            super_pressed: false,
        },
    )));
    assert!(rt.pointer_over_selectable_text());
}
