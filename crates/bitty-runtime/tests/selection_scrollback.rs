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
use bitty_runtime::{PresentFrame, Runtime, ViewId};
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
    // Drag the first five columns of the top visible row.
    let start = cell_center(&rt, id, 0, 0);
    let end = cell_center(&rt, id, 0, 4);
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
fn scroll_while_selecting_tracks_anchor() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 60);
    let id = view_id(&rt);
    assert_eq!(scroll_offset(&rt), 0, "must start live");

    // Press at frame-local (5, 2) and hold (no release): anchor == focus.
    // (Live-grid rows include the cursor-follow window offset, so record the
    // actual press cell instead of assuming frame-local == grid.)
    let at = cell_center(&rt, id, 5, 2);
    rt.handle_cursor_moved(at);
    press(&mut rt);
    assert!(rt.is_selection_dragging(), "press arms a drag");
    let before = rt.selection().expect("press selects");
    assert_eq!(
        before.anchor, before.focus,
        "press without motion is collapsed, got {before:?}"
    );

    // Wheel up one notch (3 lines) mid-drag with no cursor motion.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(scroll_offset(&rt), 3, "wheel must scroll mid-drag");
    assert!(
        rt.is_selection_dragging(),
        "scrolling must not end the drag"
    );
    let sel = rt.selection().expect("drag survives scroll");
    // The anchor stays pinned to its buffer line, so it sits 3 viewport rows
    // lower; the focus stays at the cursor's frame-local cell.
    assert_eq!(
        sel.anchor.row,
        before.anchor.row + 3,
        "anchor tracks its buffer line across the scroll, before {before:?} got {sel:?}"
    );
    assert_eq!(
        sel.anchor.col, before.anchor.col,
        "anchor column is unchanged, before {before:?} got {sel:?}"
    );
    assert_eq!(
        sel.focus, before.focus,
        "focus stays at the cursor while scrolling, before {before:?} got {sel:?}"
    );
    // Extend after the scroll, then release: the anchor stays pinned while the
    // focus follows the new cursor viewport.
    let tracked_anchor = sel.anchor;
    let to = cell_center(&rt, id, 6, 4);
    rt.handle_cursor_moved(to);
    release(&mut rt);
    assert!(!rt.is_selection_dragging());
    let sel = rt.selection().expect("scroll-tracked drag commits");
    assert_eq!(
        sel.anchor, tracked_anchor,
        "anchor stays pinned through the post-scroll motion, got {sel:?}"
    );
    assert_eq!(
        sel.focus,
        CellPos::new(6, 4),
        "focus follows the scrolled viewport cursor, got {sel:?}"
    );
    assert!(
        rt.selection_text().is_some(),
        "tracked drag must yield visible text"
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
