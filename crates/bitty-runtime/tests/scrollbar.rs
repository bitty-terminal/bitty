//! Overlay scrollbar integration tests (headless, no display).
//!
//! Proves the CTX-0181 slice without a window server:
//!
//! - `hidden` (default) paints zero pixels and keeps grid geometry
//!   untouched, even while hovering/pressing the track area.
//! - `always` paints exactly one thumb fill whenever scrollback exists
//!   (and none while history is empty).
//! - `auto` hides until mouse proximity/hover/drag, then hides again on
//!   leave — with exactly-once repaints, never per-motion presents.
//! - Press+move on the thumb drags the viewport through the existing
//!   scroll path (no selection starts, no scroll-semantics change).
//! - Hit-testing accounts for panel gaps (CTX-0177) and padding (CTX-0223):
//!   stale coordinates miss after the track moves.
//! - A release over the painted thumb never activates hyperlinks.
//!
//! Default headless geometry throughout: 80x24 grid, 9x19 cells, 8px
//! window padding — so the no-gap track is `(720, 8, 8x456)` in physical
//! pixels (right edge inside the leaf, grid columns untouched).

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PlatformEvent, PressState, WindowEventKind, WindowId,
};
use bitty_runtime::{Runtime, RuntimeConfig, ScrollbarMode};
use bitty_ui::scrollbar::{TrackRect, TrackSpec, track_rect};

/// Default no-gap track in physical pixels (80x24 cells of 9x19px + 8px pad).
const TRACK_X: f64 = 720.0;
const TRACK_Y: f64 = 8.0;
const TRACK_H: f64 = 456.0;

fn window() -> WindowId {
    WindowId::from_raw_public(1)
}

fn runtime_with(mode: ScrollbarMode) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig {
        scrollbar_mode: mode,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn runtime_with_config(cfg: RuntimeConfig) -> Runtime {
    let mut rt = Runtime::new(cfg).expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

/// Feeds lines until scrollback exists (bounded: at most 200 lines).
fn feed_scrollback(rt: &mut Runtime) -> usize {
    for i in 0..200 {
        rt.handle_pty_bytes(format!("scrollback filler line {i:03}\r\n").as_bytes());
        if rt.scrollback_len() > 0 {
            break;
        }
    }
    rt.scrollback_len()
}

fn move_to(rt: &mut Runtime, x: f64, y: f64) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: window(),
        kind: WindowEventKind::CursorMoved(CursorPosition { x, y }),
    });
}

fn press(rt: &mut Runtime) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: window(),
        kind: WindowEventKind::MouseInput(MouseEvent {
            button: MouseButton::Left,
            state: PressState::Pressed,
        }),
    });
}

fn release(rt: &mut Runtime) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: window(),
        kind: WindowEventKind::MouseInput(MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
}

fn cursor_left(rt: &mut Runtime) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: window(),
        kind: WindowEventKind::CursorLeft,
    });
}

fn scroll_offset(rt: &Runtime) -> usize {
    let fid = rt.focused_view().expect("focused leaf");
    rt.layout()
        .find_leaf(fid)
        .expect("focused view")
        .scroll_offset()
}

#[test]
fn hidden_default_paints_nothing_and_keeps_geometry() {
    // Hidden-by-default must NOT change grid geometry: hover and press
    // over the track area add zero fills and zero layout delta.
    let mut rt = runtime_with(ScrollbarMode::Hidden);
    let sb = feed_scrollback(&mut rt);
    assert!(sb > 0, "need scrollback for the test");
    let cells_before = rt.snapshot().cells;
    let allocs_before = rt.layout_allocations();
    let stats = rt.tick().expect("first frame presents");
    assert!(!rt.scrollbar_is_visible());
    // Hover the track: no repaint (idle), no visibility.
    move_to(&mut rt, TRACK_X + 4.0, 200.0);
    assert!(rt.tick().is_none(), "hidden hover must not present");
    assert!(!rt.scrollbar_is_visible());
    // Press on the track: no scrollbar drag (the selection path keeps it —
    // hidden chrome never steals input).
    press(&mut rt);
    assert!(!rt.is_scrollbar_dragging());
    release(&mut rt);
    assert!(!rt.is_scrollbar_dragging());
    // Geometry untouched: same allocations, same grid cells.
    assert_eq!(rt.layout_allocations(), allocs_before);
    assert_eq!(rt.snapshot().cells, cells_before);
    assert!(!rt.scrollbar_is_visible());
    let _ = stats;
}

#[test]
fn always_paints_exactly_one_thumb_fill_with_scrollback() {
    // Same bytes through hidden vs always: the only delta is one thumb fill.
    let mut hidden = runtime_with(ScrollbarMode::Hidden);
    feed_scrollback(&mut hidden);
    let base = hidden.tick().expect("hidden presents");
    let mut shown = runtime_with(ScrollbarMode::Always);
    feed_scrollback(&mut shown);
    let stats = shown.tick().expect("always presents");
    assert!(shown.scrollbar_is_visible());
    assert!(!hidden.scrollbar_is_visible());
    assert_eq!(stats.fills, base.fills + 1, "exactly one thumb fill");
    assert_eq!(stats.glyphs, base.glyphs, "thumb adds no glyphs");
    // Track hugs the right edge inside the leaf at default geometry.
    assert_eq!(
        shown.scrollbar_track(),
        Some(TrackRect {
            x: 720,
            y: 8,
            width: 8,
            height: 456,
        })
    );
}

#[test]
fn always_paints_nothing_while_history_is_empty() {
    // `always` with no scrollback paints nothing: hidden and always agree
    // pixel-for-pixel on a fresh grid.
    let mut hidden = runtime_with(ScrollbarMode::Hidden);
    let base = hidden.tick().expect("hidden presents");
    let mut shown = runtime_with(ScrollbarMode::Always);
    let stats = shown.tick().expect("always presents");
    assert!(!shown.scrollbar_is_visible());
    assert_eq!(stats.fills, base.fills);
    assert_eq!(stats.glyphs, base.glyphs);
    assert_eq!(shown.scrollbar_track(), None);
}

#[test]
fn auto_hides_until_proximity_then_hides_on_leave() {
    let mut rt = runtime_with(ScrollbarMode::Auto);
    feed_scrollback(&mut rt);
    rt.tick().expect("first frame presents");
    assert!(!rt.scrollbar_is_visible(), "auto starts hidden");
    // Far motion: still hidden, still idle (no per-motion present).
    move_to(&mut rt, 100.0, 100.0);
    assert!(rt.tick().is_none(), "far hover must not present");
    assert!(!rt.scrollbar_is_visible());
    // Proximity (within 16px of the track): exactly one repaint, visible.
    move_to(&mut rt, TRACK_X - 6.0, 100.0);
    rt.tick().expect("proximity transition presents");
    assert!(rt.scrollbar_is_visible());
    assert!(rt.tick().is_none(), "steady hover must idle");
    // Leaving the window hides again with one repaint, then idles.
    cursor_left(&mut rt);
    rt.tick().expect("leave transition presents");
    assert!(!rt.scrollbar_is_visible());
    assert!(rt.tick().is_none(), "must idle once hidden");
}

#[test]
fn drag_thumb_scrolls_viewport_without_selection() {
    let mut rt = runtime_with(ScrollbarMode::Always);
    let sb = feed_scrollback(&mut rt);
    assert!(sb > 0, "need scrollback for the test");
    rt.tick().expect("first frame presents");
    assert!(rt.scrollbar_is_visible());
    let track = rt.scrollbar_track().expect("track");
    // Press on the live thumb (bottom of the track): starts a drag, and
    // the selection path never sees the event.
    move_to(
        &mut rt,
        f64::from(track.x) + 2.0,
        f64::from(track.y) + f64::from(track.height) - 4.0,
    );
    press(&mut rt);
    assert!(rt.is_scrollbar_dragging());
    assert!(
        rt.selection().is_none(),
        "thumb press must not start a selection"
    );
    // Drag to the top: viewport scrolls fully into history.
    move_to(&mut rt, f64::from(track.x) + 2.0, f64::from(track.y) + 2.0);
    assert!(rt.is_scrollbar_dragging());
    assert_eq!(
        scroll_offset(&rt),
        sb,
        "drag to top must reach oldest history"
    );
    // Drag back to the bottom: viewport returns to live.
    move_to(
        &mut rt,
        f64::from(track.x) + 2.0,
        f64::from(track.y) + f64::from(track.height) - 4.0,
    );
    assert_eq!(scroll_offset(&rt), 0, "drag to bottom must go live");
    release(&mut rt);
    assert!(!rt.is_scrollbar_dragging());
    assert_eq!(scroll_offset(&rt), 0);
}

#[test]
fn press_outside_track_stays_on_selection_path() {
    // Chrome is exclusive: grid presses still select, never drag.
    let mut rt = runtime_with(ScrollbarMode::Always);
    feed_scrollback(&mut rt);
    rt.tick().expect("first frame presents");
    move_to(&mut rt, 100.0, 100.0);
    press(&mut rt);
    assert!(!rt.is_scrollbar_dragging());
    assert!(rt.is_selection_dragging());
    release(&mut rt);
    assert!(!rt.is_scrollbar_dragging());
}

#[test]
fn gaps_and_padding_shift_hit_testing() {
    // CTX-0177: an outer gap moves the track, so stale no-gap coordinates
    // miss the scrollbar while the resolved track still drags.
    let mut rt = runtime_with_config(RuntimeConfig {
        scrollbar_mode: ScrollbarMode::Always,
        gaps_out: 1,
        ..RuntimeConfig::default()
    });
    let sb = feed_scrollback(&mut rt);
    assert!(sb > 0, "need scrollback for the test");
    rt.tick().expect("first frame presents");
    let track = rt.scrollbar_track().expect("track");
    assert_ne!(
        (track.x, track.y),
        (TRACK_X as i32, TRACK_Y as i32),
        "outer gap must move the track"
    );
    // The resolved track matches pure math on the gapped allocation.
    let fid = rt.focused_view().expect("focused leaf");
    let (_, rect) = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == fid)
        .expect("focused allocation");
    assert_eq!(
        Some(track),
        track_rect(TrackSpec {
            leaf: rect,
            cell_w_px: 9,
            cell_h_px: 19,
            pad_px: 8,
            width_px: 8,
        }),
        "track must follow the gapped allocation plus padding"
    );
    // Stale no-gap press misses the scrollbar (selection keeps it).
    move_to(&mut rt, TRACK_X + 4.0, TRACK_Y + TRACK_H - 4.0);
    press(&mut rt);
    assert!(
        !rt.is_scrollbar_dragging(),
        "stale coordinates must miss the gapped track"
    );
    release(&mut rt);
    // Live thumb press on the resolved track drags.
    move_to(
        &mut rt,
        f64::from(track.x) + 2.0,
        f64::from(track.y) + f64::from(track.height) - 4.0,
    );
    press(&mut rt);
    assert!(rt.is_scrollbar_dragging());
    move_to(&mut rt, f64::from(track.x) + 2.0, f64::from(track.y) + 2.0);
    assert_eq!(scroll_offset(&rt), sb);
    release(&mut rt);

    // CTX-0223: zero padding puts the track flush at the window edge.
    let mut bare = runtime_with_config(RuntimeConfig {
        scrollbar_mode: ScrollbarMode::Always,
        window_padding: 0,
        ..RuntimeConfig::default()
    });
    feed_scrollback(&mut bare);
    bare.tick().expect("presents");
    assert_eq!(
        bare.scrollbar_track(),
        Some(TrackRect {
            x: 712,
            y: 0,
            width: 8,
            height: 456,
        })
    );
}

#[test]
fn release_over_thumb_never_activates_hyperlinks() {
    // A full-width hyperlink row sits on the live bottom row (col 79 maps
    // from the thumb strip); a thumb-gesture release must not open it,
    // while the same click with hidden chrome still does (test validity).
    fn feed_link_row(rt: &mut Runtime) {
        let row = "x".repeat(80);
        rt.handle_pty_bytes(
            format!("\x1b]8;;https://example.test\x07{row}\x1b]8;;\x07").as_bytes(),
        );
    }
    let mut rt = runtime_with(ScrollbarMode::Always);
    feed_scrollback(&mut rt);
    feed_link_row(&mut rt);
    rt.tick().expect("presents");
    let track = rt.scrollbar_track().expect("track");
    move_to(
        &mut rt,
        f64::from(track.x) + 2.0,
        f64::from(track.y) + f64::from(track.height) - 4.0,
    );
    press(&mut rt);
    assert!(rt.is_scrollbar_dragging());
    release(&mut rt);
    assert!(
        rt.take_activation_gesture().is_none(),
        "thumb release must not activate the link beneath"
    );

    // Validity: the same coordinates with hidden chrome DO activate.
    let mut plain = runtime_with(ScrollbarMode::Hidden);
    feed_scrollback(&mut plain);
    feed_link_row(&mut plain);
    plain.tick().expect("presents");
    move_to(
        &mut plain,
        f64::from(track.x) + 2.0,
        f64::from(track.y) + f64::from(track.height) - 4.0,
    );
    press(&mut plain);
    release(&mut plain);
    assert!(
        plain.take_activation_gesture().is_some(),
        "hidden-chrome click must still activate (test validity)"
    );
}
