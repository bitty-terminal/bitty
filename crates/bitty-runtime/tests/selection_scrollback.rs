#![forbid(unsafe_code)]
//! Scrolled-viewport mouse selection (CTX-1021, issue #1807).
//!
//! The pointer-to-grid mapping must apply the scrollback/viewport offset:
//! after scrolling up with the wheel, a left-drag must select exactly the
//! visible text under the cursor at any scroll position (not live-bottom
//! text as if the viewport were at the bottom), and a wheel scroll during
//! an active drag must keep the anchor pinned to its buffer line (tracking).
//!
//! All pointer positions derive from public geometry
//! (`present_frames`, `live_cell_size`, `window_padding_physical`) — no
//! hard-coded pixel, padding, or decoration constants. Headless only.

use bitty_platform::{CursorPosition, MouseButton, MouseEvent, PressState, ScrollDelta};
use bitty_runtime::{PresentFrame, Runtime, RuntimeConfig, ViewId};
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn feed_lines(rt: &mut Runtime, count: usize) {
    for i in 0..count {
        let line = format!("srow{i:02} txt\r\n");
        rt.handle_pty_bytes(line.as_bytes());
    }
}

fn view_id(rt: &Runtime) -> ViewId {
    rt.focused_view()
        .or(rt.primary_view())
        .expect("a focused or primary view")
}

fn scroll_offset(rt: &Runtime) -> usize {
    let id = view_id(rt);
    rt.layout()
        .find_leaf(id)
        .map(|view| view.scroll_offset())
        .unwrap_or(usize::MAX)
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Physical cursor position at a vertical cell edge, row centre.
///
/// Issue #2480: pointer drags are caret-to-caret horizontally, so tests that
/// mean "select this exact cell range" press on the leading edge and release
/// on the trailing edge instead of relying on inclusive cell rounding.
fn cell_edge(rt: &Runtime, view: ViewId, row: u16, col_edge: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + f64::from(col_edge) * f64::from(cw),
        y: pad + f64::from(frame.content.y) + f64::from(row) * f64::from(ch) + f64::from(ch) / 2.0,
    }
}

/// Physical cursor position at the centre of frame-local cell `(row, col)`.
fn cell_center(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + f64::from(col) * f64::from(cw) + f64::from(cw) / 2.0,
        y: pad + f64::from(frame.content.y) + f64::from(row) * f64::from(ch) + f64::from(ch) / 2.0,
    }
}

fn press(rt: &mut Runtime) {
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Pressed));
}

fn release(rt: &mut Runtime) {
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Released));
}

fn visible_rows(rt: &Runtime) -> Vec<String> {
    let id = view_id(rt);
    let view = rt.layout().find_leaf(id).expect("focused leaf").clone();
    view.visible_text_rows(rt.state())
}

#[test]
fn scrolled_viewport_drag_selects_visible_text() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 60);
    assert!(
        rt.state().scrollback_len() > 10,
        "need scrollback, got {}",
        rt.state().scrollback_len()
    );
    // Scroll up 6 lines (two default notches of 3) into history.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 2.0));
    assert_eq!(scroll_offset(&rt), 6, "wheel must scroll into history");

    let id = view_id(&rt);
    let visible = visible_rows(&rt);
    assert!(!visible.is_empty(), "viewport must show history");
    // Drag the first five columns of the top visible row: leading edge of
    // column 0 through the trailing edge of column 4.
    let start = cell_edge(&rt, id, 0, 0);
    let end = cell_edge(&rt, id, 0, 5);
    rt.handle_cursor_moved(start);
    press(&mut rt);
    rt.handle_cursor_moved(end);
    release(&mut rt);

    assert!(!rt.is_selection_dragging(), "release commits the drag");
    let sel = rt.selection().expect("scrolled drag must select");
    assert_eq!(
        sel.normalized().start,
        CellPos::new(0, 0),
        "press maps to the viewport top-left, not the live bottom"
    );
    assert_eq!(
        sel.normalized().end,
        CellPos::new(0, 4),
        "release maps to the viewport cell under the cursor"
    );
    let expected: String = visible[0].chars().take(5).collect();
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(expected.as_str()),
        "must copy visible history text, got {:?} vs visible row {:?}",
        rt.selection_text(),
        visible[0]
    );
    // The live bottom shows newer lines: selecting there would be different.
    let live_bottom = {
        let snap = rt.state().snapshot();
        let row = snap.height.saturating_sub(1);
        let mut line = String::new();
        for col in 0..5 {
            let idx = row * snap.width + col;
            if let Some(cell) = snap.cells.get(idx) {
                if !cell.spacer {
                    line.push(cell.glyph);
                }
            }
        }
        line
    };
    assert_ne!(
        rt.selection_text().as_deref(),
        Some(live_bottom.as_str()),
        "must not select live-bottom text while scrolled"
    );
    // The highlight must actually paint while scrolled (previously suppressed).
    let stats = rt.tick().expect("selection must force a present");
    assert!(stats.fills > 0, "scrolled highlight must paint fills");
}

#[test]
fn scroll_while_dragging_pins_the_press_endpoint_pointer_driven() {
    // Issue #2470 + #2480 review: a real pointer drag, wheel-scrolled while
    // held. Only the press-side endpoint moves with the viewport; the
    // pointer-side endpoint stays under the cursor, and the range keeps
    // growing from the original anchor.
    let mut rt = make_runtime();
    feed_lines(&mut rt, 60);
    assert_eq!(scroll_offset(&rt), 0, "must start live");
    let id = view_id(&rt);

    let press_pos = cell_edge(&rt, id, 10, 2);
    rt.handle_cursor_moved(press_pos);
    press(&mut rt);
    rt.handle_cursor_moved(cell_edge(&rt, id, 12, 4));
    assert!(rt.is_selection_dragging(), "press arms a drag");
    let before = rt.selection().expect("drag selects").normalized();
    assert_eq!(
        before.end.row,
        before.start.row + 2,
        "two rows of pointer travel, before {before:?}"
    );

    // Wheel up one notch (3 lines) mid-drag with no cursor motion.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(scroll_offset(&rt), 3, "wheel must scroll mid-drag");
    assert!(
        rt.is_selection_dragging(),
        "scrolling must not end the drag"
    );
    let sel = rt.selection().expect("drag survives scroll").normalized();
    // The press line is pinned in the buffer, so it sits 3 viewport rows
    // lower; the pointer line stays at its screen row.
    assert_eq!(
        sel.end.row,
        before.start.row + 3,
        "press endpoint tracks its buffer line, before {before:?} got {sel:?}"
    );
    assert_eq!(
        sel.start.row, 12,
        "pointer endpoint stays on its screen row, before {before:?} got {sel:?}"
    );

    // Extend after the scroll, then release: the press endpoint stays pinned
    // while the pointer endpoint follows the new cursor cell.
    let press_row = sel.end.row;
    rt.handle_cursor_moved(cell_edge(&rt, id, 14, 5));
    release(&mut rt);
    assert!(!rt.is_selection_dragging());
    let sel = rt
        .selection()
        .expect("scroll-tracked drag commits")
        .normalized();
    assert_eq!(
        sel.end.row, press_row,
        "press anchor stays pinned through the post-scroll motion, got {sel:?}"
    );
    assert_eq!(
        sel.start.row, 14,
        "pointer endpoint follows the scrolled viewport cursor, got {sel:?}"
    );
    assert!(
        rt.selection_text().is_some(),
        "tracked drag must yield text across the scroll"
    );
}

#[test]
fn drag_can_span_multiple_scrollback_pages_while_held() {
    // Issue #2470/#2523: hold a drag, wheel up several notches, then extend
    // into the revealed page and release. The committed range must contain
    // more rows than one viewport and copy every line in between.
    let mut rt = make_runtime();
    feed_lines(&mut rt, 80);
    let id = view_id(&rt);
    let rows = usize::from(rt.layout().find_leaf(id).expect("leaf").rows());

    // Press near the bottom of the live viewport.
    let press_pos = cell_edge(&rt, id, 20, 0);
    rt.handle_cursor_moved(press_pos);
    press(&mut rt);
    let press_row = rt.selection().expect("press").anchor.row;
    // Scroll up two notches (6 lines) with the button still held.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 2.0));
    assert_eq!(scroll_offset(&rt), 6);
    assert!(rt.is_selection_dragging());
    // Extend to the top visible row, now in history.
    rt.handle_cursor_moved(cell_edge(&rt, id, 0, 0));
    release(&mut rt);

    let sel = rt.selection().expect("multi-page drag commits");
    assert!(!rt.is_selection_dragging());
    assert_eq!(
        sel.normalized().start.row,
        0,
        "pointer endpoint is the visible top row"
    );
    assert_eq!(
        sel.normalized().end.row,
        (press_row + 6).min(rows as u16 - 1),
        "press endpoint stays pinned to its buffer line"
    );
    let text = rt.selection_text().expect("multi-page selection text");
    assert!(
        text.lines().count() > rows,
        "selection must span more than one viewport page, got {} lines for {rows} rows",
        text.lines().count()
    );
    assert!(
        text.starts_with("srow"),
        "history text must be the lines under the drag, got {:?}",
        text.chars().take(8).collect::<String>()
    );

    // Scrolling further after the commit keeps the copied text stable.
    let before = text;
    rt.handle_wheel(ScrollDelta::Lines(0.0, 3.0));
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(before.as_str()),
        "buffer-anchored text survives later scrolling"
    );
}

#[test]
fn page_scroll_selection_follows_actual_delta() {
    // Issue #2475: paging at a custom percentage shifts a committed
    // selection by the actual scroll delta, not the viewport height.
    let mut rt = Runtime::new(RuntimeConfig {
        rows: 40,
        page_scroll_percent: 50,
        ..RuntimeConfig::default()
    })
    .expect("page-scroll runtime must build");
    rt.force_headless_clipboard();
    feed_lines(&mut rt, 200);
    assert_eq!(scroll_offset(&rt), 0, "must start live");
    let id = view_id(&rt);
    let rows = usize::from(rt.layout().find_leaf(id).expect("leaf").rows());
    assert_eq!(rows, 40);

    // Commit a single-row selection in the live viewport.
    let start = cell_center(&rt, id, 10, 0);
    let end = cell_center(&rt, id, 10, 4);
    rt.handle_cursor_moved(start);
    press(&mut rt);
    rt.handle_cursor_moved(end);
    release(&mut rt);
    let before_offset = scroll_offset(&rt);
    let before = rt.selection().expect("committed selection");

    assert!(rt.scroll_focused_page(true), "page up must scroll");
    let actual_delta = scroll_offset(&rt) - before_offset;
    assert_eq!(actual_delta, rows * 50 / 100, "custom page distance");

    let after = rt.selection().expect("selection survives page scroll");
    assert_eq!(
        after.anchor.row,
        before.anchor.row + actual_delta as u16,
        "anchor tracks its buffer line by the actual delta"
    );
    assert_eq!(
        after.focus.row,
        before.focus.row + actual_delta as u16,
        "focus tracks its buffer line by the actual delta"
    );
}

#[test]
fn committed_scrolled_selection_tracks_on_wheel() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 60);
    rt.handle_wheel(ScrollDelta::Lines(0.0, 2.0));
    assert_eq!(scroll_offset(&rt), 6);
    let id = view_id(&rt);

    // Commit a single-row selection at the viewport top.
    let start = cell_center(&rt, id, 0, 0);
    let end = cell_center(&rt, id, 0, 4);
    rt.handle_cursor_moved(start);
    press(&mut rt);
    rt.handle_cursor_moved(end);
    release(&mut rt);
    let before_text = rt.selection_text().expect("committed text");
    let before = rt.selection().expect("committed range");

    // Scroll up one more notch: both endpoints shift so the highlight stays
    // on the same buffer line, and the text is unchanged.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(scroll_offset(&rt), 9);
    let after = rt.selection().expect("committed selection survives scroll");
    assert_eq!(
        after.anchor.row,
        before.anchor.row + 3,
        "anchor scrolls with its content, before {before:?} after {after:?}"
    );
    assert_eq!(
        after.focus.row,
        before.focus.row + 3,
        "focus scrolls with its content, before {before:?} after {after:?}"
    );
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(before_text.as_str()),
        "buffer-pinned text survives the scroll"
    );
}
