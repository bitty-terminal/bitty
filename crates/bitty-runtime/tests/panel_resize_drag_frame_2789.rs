//! Issue #2789: a drag-resize must paint committed geometry every step.
//!
//! Reported on 0.0.30: resizing a panel was no longer smooth — every drag
//! step flashed and the scrollbar could fall out of sync. A border-drag
//! step commits the split ratio immediately; content reflows once, and the
//! frame that paints that commit must show the committed bounds. When the
//! present path instead keeps interpolating per-panel bounds, each step
//! paints the reflowed grid into the previous step's rectangle: the strip
//! beside the moved divider belongs to no painted leaf (an unpainted
//! flash) and the overlay scrollbar, resolved from the committed frames,
//! floats off the painted edge.
//!
//! The tests pin the contract on a virtual clock using only APIs that
//! predate the fix, so this file compiles and fails on the parent revision:
//! - every drag step paints exactly the committed frames (no stale or
//!   unpainted band) and the panes still tile the container;
//! - the committed scrollbar track input routes against hugs the painted
//!   content after every step, after a release that races the final
//!   commit's present, and through an oscillating resize storm.

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, MouseButton, PressState};
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, ScrollbarMode, SplitAxis, View, ViewId};
use bitty_ui::scrollbar::{TrackRect, thumb_geometry};

fn runtime_with_scrollbar() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig {
        scrollbar_mode: ScrollbarMode::Always,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (8px padding, 6px decoration outer inset, 9x19 cells,
/// zero gaps), matching the runtime's own cell mapping.
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 6.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 6.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn two_pane_horizontal() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// The committed scrollbar track (the geometry input routes against) must
/// hug the painted focused pane during a live resize gesture. Once the
/// gesture paints its commit, the track resolved for input and the rect
/// the frame paints describe the same viewport.
fn assert_scrollbar_matches_painted_viewport(
    rt: &Runtime,
    painted: &[bitty_runtime::PresentFrame],
) {
    let Some(track) = rt.scrollbar_track() else {
        panic!("scrollback exists, so the scrollbar must resolve");
    };
    let focused = rt
        .focused_view()
        .expect("a focused leaf owns the scrollbar");
    let frame = painted
        .iter()
        .find(|frame| frame.view == focused)
        .expect("the focused leaf must be painted");
    let pad = i32::try_from(rt.window_padding_physical()).expect("padding fits i32");
    let width = i32::try_from(rt.scrollbar_width_physical()).expect("width fits i32");
    assert_eq!(
        track,
        TrackRect {
            x: frame.content.x + pad + i32::try_from(frame.content.width).expect("content fits")
                - width,
            y: frame.content.y + pad,
            width: rt.scrollbar_width_physical(),
            height: frame.content.height,
        },
        "the scrollbar track must hug the painted content, not the last presented one"
    );
    let view = rt
        .layout()
        .find_leaf(focused)
        .expect("focused leaf is live");
    let thumb = thumb_geometry(
        track.height,
        usize::from(view.rows()),
        rt.scrollback_len(),
        view.scroll_offset(),
    )
    .expect("scrollback geometry resolves");
    assert!(
        thumb.y.saturating_add(thumb.height) <= track.height,
        "the thumb must stay inside its track"
    );
}

#[test]
fn resize_drag_steps_paint_committed_bounds_and_keep_the_scrollbar_in_sync() {
    let mut rt = runtime_with_scrollbar();
    rt.set_layout(two_pane_horizontal());
    // Enough output that the focused primary pane owns scrollback, so the
    // overlay scrollbar has a thumb to place.
    for i in 0..40 {
        rt.handle_pty_bytes(format!("scrollback line {i:02}\r\n").as_bytes());
    }
    assert!(rt.scrollback_len() > 0, "fixture needs scrollback");
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");

    // Grab the zero-gap divider between the two 40-column panes, then drag
    // it right one cell per step on the virtual clock.
    let grab = cell_pixels(39, 12);
    rt.handle_cursor_moved(grab);
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "border press must grab the divider"
    );

    // The decoration gap between the two frames is constant (gaps_out on
    // each side); a drag step must never widen it into an unpainted band.
    let divider_band = i64::from(frame_of(&rt, ViewId::new(2)).frame.x)
        - frame_of(&rt, ViewId::new(1)).frame.right_exclusive();
    assert!(divider_band >= 0, "the fixture tiles left-to-right");
    let mut previous_left_edge = frame_of(&rt, ViewId::new(1)).frame.right_exclusive();

    let mut now = start;
    for step in 1..=6 {
        now += Duration::from_millis(20);
        let pos = CursorPosition {
            x: grab.x + f64::from(step) * 9.0,
            y: grab.y,
        };
        assert!(
            rt.update_border_drag_at(pos, now),
            "step {step} is consumed"
        );
        rt.tick_at(now).expect("each drag step presents");
        let committed = rt.present_frames();
        let painted = rt.paint_frames_at(now);
        assert_eq!(
            painted, committed,
            "step {step}: the presented frame must paint the committed geometry"
        );

        // The two panes still tile the container: the left content ends
        // where the right content begins apart from the decoration bands,
        // so no strip is owned by neither leaf (the historical flash).
        let left = frame_of(&rt, ViewId::new(1));
        let right = frame_of(&rt, ViewId::new(2));
        let left_edge = left.frame.right_exclusive();
        assert!(
            left_edge > previous_left_edge,
            "step {step}: the divider must move right"
        );
        previous_left_edge = left_edge;
        assert!(
            left.content.right_exclusive() <= i64::from(right.content.x),
            "step {step}: panes overlap"
        );
        assert_eq!(
            i64::from(right.frame.x) - left.frame.right_exclusive(),
            divider_band,
            "step {step}: the divider band must stay the constant decoration gap"
        );
        assert_scrollbar_matches_painted_viewport(&rt, &painted);
    }

    // Issue #2789 (release race): the pointer can commit one final step and
    // release before the event loop ticks. The first present after the
    // release must still paint that commit directly — a settle tween from
    // the last presented step would paint the reflowed grid into the stale
    // rectangle one more time.
    now += Duration::from_millis(20);
    let final_pos = CursorPosition {
        x: grab.x + 7.0 * 9.0,
        y: grab.y,
    };
    assert!(
        rt.update_border_drag_at(final_pos, now),
        "the final step is consumed"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    let released = now + Duration::from_millis(10);
    rt.tick_at(released)
        .expect("the release settles with a present");
    let painted = rt.paint_frames_at(released);
    assert_eq!(
        painted,
        rt.present_frames(),
        "a release before the committed step presents must not interpolate a settle tween"
    );
    assert_scrollbar_matches_painted_viewport(&rt, &painted);

    // The settled frame keeps the same painted/committed/scrollbar agreement.
    let settled = released + Duration::from_millis(300);
    rt.tick_at(settled).expect("settled frame presents");
    let painted = rt.paint_frames_at(settled);
    assert_eq!(
        painted,
        rt.present_frames(),
        "after the commit the painted geometry is the committed geometry"
    );
    assert_scrollbar_matches_painted_viewport(&rt, &painted);
}

#[test]
fn resize_storm_keeps_painted_and_committed_in_lockstep() {
    // Issue #2789: an oscillating drag storms the commit path. Every step
    // reflows the grids, so every frame must paint its own commit; a step
    // that re-uses the previous rectangle leaves the flash behind.
    let mut rt = runtime_with_scrollbar();
    rt.set_layout(two_pane_horizontal());
    for i in 0..40 {
        rt.handle_pty_bytes(format!("scrollback line {i:02}\r\n").as_bytes());
    }
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");

    let grab = cell_pixels(39, 12);
    rt.handle_cursor_moved(grab);
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active(), "border press must grab");

    // Sweep right and back twice; no offset repeats back-to-back, so every
    // step commits a new split ratio.
    const SWEEP: [i32; 12] = [1, 2, 3, 4, 5, 6, 5, 4, 3, 2, 1, 0];
    let mut now = start;
    for step in 0..24 {
        now += Duration::from_millis(20);
        let offset = SWEEP[step % SWEEP.len()];
        let pos = CursorPosition {
            x: grab.x + f64::from(offset) * 9.0,
            y: grab.y,
        };
        assert!(rt.update_border_drag_at(pos, now), "storm step {step}");
        rt.tick_at(now).expect("storm step presents");
        let painted = rt.paint_frames_at(now);
        assert_eq!(
            painted,
            rt.present_frames(),
            "storm step {step}: the presented frame must paint the committed geometry"
        );
        assert_scrollbar_matches_painted_viewport(&rt, &painted);
    }

    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    let settled = now + Duration::from_millis(300);
    rt.tick_at(settled).expect("storm settles");
    assert_eq!(
        rt.paint_frames_at(settled),
        rt.present_frames(),
        "the storm's final commit must paint directly"
    );
}
