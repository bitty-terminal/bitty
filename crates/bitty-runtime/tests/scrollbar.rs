//! Overlay scrollbar integration tests (headless, no display).
//!
//! Proves the CTX-0181 slice without a window server:
//!
//! - `auto` (default, CTX-0362) paints nothing at rest — zero fills and
//!   zero grid/layout delta — then reveals the thumb on mouse proximity,
//!   reveals it on hover, and hides it again when the cursor leaves; with
//!   exactly-once repaints, never per-motion presents.
//! - `hidden` paints zero pixels and keeps grid geometry untouched, even
//!   while hovering/pressing the track area (explicit opt-out).
//! - `always` paints exactly one thumb fill whenever scrollback exists
//!   (and none while history is empty).
//! - Press+move on the thumb drags the viewport through the existing
//!   scroll path (no selection starts, no scroll-semantics change).
//! - Hit-testing accounts for panel gaps (CTX-0177) and padding (CTX-0223):
//!   stale coordinates miss after the track moves.
//! - A release over the painted thumb never activates hyperlinks.
//!
//! Default headless geometry throughout: 80x24 grid, 9x19 cells, 8px
//! window padding, and the unified CTX-0333 decoration `6/6/2/6/6` — so the
//! track is `(706, 22, 8x428)` in physical pixels, hugging the decorated
//! content frame right edge (CTX-0294 present frame, not the raw cell
//! allocation; grid columns untouched).

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PlatformEvent, PressState, WindowEventKind, WindowId,
};
use bitty_runtime::{
    Decoration, LayoutNode, Runtime, RuntimeConfig, ScrollbarMode, SplitAxis, View, ViewId,
};
use bitty_ui::scrollbar::TrackRect;

/// Default decorated track in physical pixels: 80x24 cells of 9x19px, 8px
/// padding, unified decoration `6/6/2/6/6` (gaps_out 6 + border 2 + content
/// inset 6 inset the content frame to `(14, 14, 692, 428)`; right edge
/// 14 + 692 = 706, thumb 8 wide).
const TRACK_X: f64 = 706.0;
const TRACK_Y: f64 = 22.0;
const TRACK_H: f64 = 428.0;

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
fn hidden_mode_paints_nothing_and_keeps_geometry() {
    // Explicit `hidden` must NOT change grid geometry: hover and press
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
fn default_mode_is_auto_overlay_and_reveals_on_hover() {
    // CTX-0362: the shipped default is the overlay `auto` mode. At rest it
    // is transparent and geometry-neutral (no fill, no layout delta); a
    // cursor near/over the right-edge track reveals the thumb; leaving the
    // proximity zone hides it again — each transition one repaint, steady
    // state idle.
    assert_eq!(
        RuntimeConfig::default().scrollbar_mode,
        ScrollbarMode::Auto,
        "shipped default must be the auto overlay"
    );
    let mut rt = runtime_with_config(RuntimeConfig::default());
    let sb = feed_scrollback(&mut rt);
    assert!(sb > 0, "need scrollback for the test");
    let cells_before = rt.snapshot().cells;
    let allocs_before = rt.layout_allocations();
    // At rest (no cursor entered the window): nothing paints.
    rt.tick().expect("first frame presents");
    assert!(!rt.scrollbar_is_visible(), "auto must start hidden");
    // Hover the right edge: exactly one repaint reveals the thumb.
    move_to(&mut rt, TRACK_X + 2.0, 200.0);
    rt.tick().expect("hover transition presents");
    assert!(rt.scrollbar_is_visible(), "hover must reveal the thumb");
    assert!(rt.tick().is_none(), "steady hover must idle");
    // The overlay never changes grid or layout geometry.
    assert_eq!(rt.layout_allocations(), allocs_before);
    assert_eq!(rt.snapshot().cells, cells_before);
    // Moving away from the track hides it again with one repaint.
    move_to(&mut rt, 100.0, 100.0);
    rt.tick().expect("leave-proximity transition presents");
    assert!(!rt.scrollbar_is_visible(), "leaving proximity must hide");
    assert!(rt.tick().is_none(), "must idle once hidden again");
}

#[test]
fn default_auto_hover_then_drag_scrolls_without_selection() {
    // CTX-0362: under the shipped `auto` default the thumb is draggable
    // once hover engages it; the press routes exclusively to the drag and
    // never starts a selection. Leaving the window ends the drag and hides.
    let mut rt = runtime_with_config(RuntimeConfig::default());
    let sb = feed_scrollback(&mut rt);
    assert!(sb > 0, "need scrollback for the test");
    rt.tick().expect("first frame presents");
    let track = rt.scrollbar_track().expect("track geometry resolves");
    // Engage by hovering the live (bottom) thumb strip, then press.
    move_to(
        &mut rt,
        f64::from(track.x) + 2.0,
        f64::from(track.y) + f64::from(track.height) - 4.0,
    );
    rt.tick().expect("hover transition presents");
    assert!(rt.scrollbar_is_visible());
    press(&mut rt);
    assert!(rt.is_scrollbar_dragging(), "hovered thumb press must drag");
    assert!(rt.selection().is_none(), "chrome press must not select");
    // Drag to the top: the viewport reaches oldest history.
    move_to(&mut rt, f64::from(track.x) + 2.0, f64::from(track.y) + 2.0);
    assert_eq!(scroll_offset(&rt), sb, "drag to top reaches oldest history");
    release(&mut rt);
    assert!(!rt.is_scrollbar_dragging());
    // Leaving the window hides the revealed thumb again.
    cursor_left(&mut rt);
    assert!(rt.tick().is_some(), "leave must clear the painted thumb");
    assert!(!rt.scrollbar_is_visible());
}

#[test]
fn hidden_and_always_modes_remain_selectable() {
    // CTX-0362: `auto` is only the default; explicit `hidden` opts out and
    // `always` pins the overlay visible. Hovering never reveals `hidden`.
    let mut hidden = runtime_with(ScrollbarMode::Hidden);
    let sb = feed_scrollback(&mut hidden);
    assert!(sb > 0, "need scrollback for the test");
    hidden.tick().expect("first frame presents");
    move_to(&mut hidden, TRACK_X + 2.0, 200.0);
    assert!(hidden.tick().is_none(), "hidden hover must not present");
    assert!(!hidden.scrollbar_is_visible());

    let mut always = runtime_with(ScrollbarMode::Always);
    feed_scrollback(&mut always);
    always.tick().expect("first frame presents");
    assert!(always.scrollbar_is_visible(), "always paints without hover");
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
    // Track hugs the decorated content frame right edge at default geometry.
    assert_eq!(
        shown.scrollbar_track(),
        Some(TrackRect {
            x: 706,
            y: 22,
            width: 8,
            height: 428,
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
    // The resolved track hugs the decorated content frame (outer cell gap
    // plus decoration gaps_out 6, border 2, content inset 6) translated by
    // the 8px pad: content (23, 33, 674, 390) -> right edge
    // 8 + 23 + 674 - 8 = 697.
    assert_eq!(
        track,
        TrackRect {
            x: 697,
            y: 41,
            width: 8,
            height: 390,
        },
        "track must follow the decorated content frame plus padding"
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

    // CTX-0223: zero padding puts the track flush at the decorated content
    // edge (default decoration: content (14, 14, 692, 428) -> x 698, y 14).
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
            x: 698,
            y: 14,
            width: 8,
            height: 428,
        })
    );
}

#[test]
fn release_over_thumb_never_activates_hyperlinks() {
    // A hyperlink sits on exactly the cell under the thumb's bottom strip;
    // a thumb-gesture release must not open it, while the same click with
    // hidden chrome still does (test validity). Setting the link on the
    // mapped cell keeps the test independent of the decorated content
    // geometry.
    let mut rt = runtime_with(ScrollbarMode::Always);
    feed_scrollback(&mut rt);
    rt.tick().expect("presents");
    let track = rt.scrollbar_track().expect("track");
    let click = CursorPosition {
        x: f64::from(track.x) + 2.0,
        y: f64::from(track.y) + f64::from(track.height) - 2.0,
    };
    let cell = rt.cursor_to_cell(click);
    let link = format!(
        "\x1b[{};{}H\x1b]8;;https://example.test\x07x\x1b]8;;\x07",
        cell.row + 1,
        cell.col + 1
    );
    rt.handle_pty_bytes(link.as_bytes());
    rt.tick().expect("link presents");
    move_to(&mut rt, click.x, click.y);
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
    plain.handle_pty_bytes(link.as_bytes());
    plain.tick().expect("presents");
    move_to(&mut plain, click.x, click.y);
    press(&mut plain);
    release(&mut plain);
    assert!(
        plain.take_activation_gesture().is_some(),
        "hidden-chrome click must still activate (test validity)"
    );
}

#[test]
fn scrollbar_width_scales_track_and_thumb() {
    // CTX-0295: `scrollbar.width` (default 8) is the paint width — a
    // non-default logical width must move the track edge and shrink/grow the
    // painted thumb footprint. Every earlier runtime test used the default 8,
    // so the width knob had no effect evidence.
    for width in [12_u32, 32] {
        let mut rt = runtime_with_config(RuntimeConfig {
            scrollbar_mode: ScrollbarMode::Always,
            scrollbar_width: width,
            ..RuntimeConfig::default()
        });
        let sb = feed_scrollback(&mut rt);
        assert!(sb > 0, "need scrollback for the test");
        assert_eq!(rt.scrollbar_width_physical(), width);
        rt.tick().expect("always presents");
        assert!(rt.scrollbar_is_visible());
        let track = rt.scrollbar_track().expect("track");
        assert_eq!(track.width, width, "track carries the configured width");
        // Right edge hugs the decorated content frame at default padding:
        // x = 8px pad + 14px content origin + 692px content - width.
        assert_eq!(track.x, 714 - width as i32);
        assert_eq!(track.y, 22);
        assert_eq!(track.height, 428);

        // The painted thumb is exactly `width` physical pixels wide: sample
        // one row inside the thumb near the live (bottom) position and count
        // non-background pixels across a window around the track. Filler
        // text is left-aligned, so the right-edge window holds only the fill.
        let rgba = rt.headless_rgba().expect("rgba after tick");
        let surface = rt.surface_extent().expect("surface extent");
        let stride = usize::try_from(surface.width()).expect("surface width fits");
        let rows = usize::from(
            rt.layout()
                .find_leaf(rt.focused_view().expect("focused leaf"))
                .expect("focused view")
                .rows(),
        );
        let thumb = bitty_ui::scrollbar::thumb_geometry(track.height, rows, rt.scrollback_len(), 0)
            .expect("thumb exists with scrollback");
        let y = usize::try_from(track.y).expect("track y fits")
            + usize::try_from(thumb.y + thumb.height / 2).expect("thumb y fits");
        // Window: 8px left of the track up to the content right edge
        // (exclusive) so the adjacent decoration border ring, which is also
        // non-background, is never counted.
        let x0 = usize::try_from(track.x - 8).expect("window x0 fits");
        let x1 = usize::try_from(track.x + width as i32).expect("window x1 fits");
        let painted = (x0..x1)
            .filter(|&x| {
                let i = (y * stride + x) * 4;
                [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]] != bitty_render::grid::DEFAULT_BG
            })
            .count();
        assert_eq!(
            painted, width as usize,
            "thumb fill must span exactly the configured width"
        );
    }

    // DPI scaling rides the same path as padding: logical 12 at 2x is 24
    // physical, and the resolved track doubles with the live cell metrics.
    let mut rt = runtime_with_config(RuntimeConfig {
        scrollbar_mode: ScrollbarMode::Always,
        scrollbar_width: 12,
        ..RuntimeConfig::default()
    });
    feed_scrollback(&mut rt);
    rt.tick().expect("presents");
    rt.apply_dpi_scale(2.0, None);
    assert_eq!(rt.scrollbar_width_physical(), 24);
    assert_eq!(rt.scrollbar_track().expect("track at 2x").width, 24);
}

#[test]
fn track_follows_decorated_content_frame_defaults_and_zero() {
    // CTX-0313: the track hugs the decorated content frame the live present
    // paints (CTX-0294), not the raw cell allocation. Unified decoration
    // `6/6/2/6/6` insets the content frame to `(14, 14, 692, 428)` before the
    // 8px padding: track top 22, right edge 8 + 14 + 692 - 8 = 706.
    for (decoration, expected) in [
        (
            Decoration::default(),
            TrackRect {
                x: 706,
                y: 22,
                width: 8,
                height: 428,
            },
        ),
        (
            Decoration::ZERO,
            TrackRect {
                x: 720,
                y: 8,
                width: 8,
                height: 456,
            },
        ),
    ] {
        let mut rt = runtime_with_config(RuntimeConfig {
            scrollbar_mode: ScrollbarMode::Always,
            decoration,
            ..RuntimeConfig::default()
        });
        let sb = feed_scrollback(&mut rt);
        assert!(sb > 0, "need scrollback for {decoration:?}");
        rt.tick().expect("presents");
        let track = rt.scrollbar_track().expect("track");
        assert_eq!(track, expected, "decoration {decoration:?}");
        // Independent relation: the track is exactly the present-painted
        // content frame plus the window padding inset.
        let fid = rt.focused_view().expect("focused leaf");
        let content = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == fid)
            .expect("focused present frame")
            .content;
        assert_eq!(
            track,
            TrackRect {
                x: 8 + content.x + content.width as i32 - 8,
                y: 8 + content.y,
                width: 8,
                height: content.height,
            },
            "decoration {decoration:?}"
        );
    }
}

#[test]
fn track_offsets_track_decoration_properties() {
    // (decoration, expected (x, y, height)) for 80x24 9x19 cells, 8px pad,
    // 8px thumb: unified defaults, ZERO, and each property in isolation.
    // `gaps_in` and `radius` leave a single leaf's frame rects unchanged;
    // `gaps_out`, `border`, and `content_inset` inset the content frame the
    // track hugs.
    let cases = [
        (Decoration::new(6, 6, 2, 6, 6), (706, 22, 428)),
        (Decoration::new(0, 0, 0, 0, 0), (720, 8, 456)),
        (Decoration::new(0, 0, 4, 0, 0), (716, 12, 448)),
        (Decoration::new(0, 10, 0, 0, 0), (710, 18, 436)),
        (Decoration::new(8, 0, 0, 0, 0), (720, 8, 456)),
        (Decoration::new(0, 0, 0, 16, 0), (720, 8, 456)),
        (Decoration::new(0, 0, 0, 0, 4), (716, 12, 448)),
    ];
    for (decoration, (x, y, height)) in cases {
        let mut rt = runtime_with_config(RuntimeConfig {
            scrollbar_mode: ScrollbarMode::Always,
            decoration,
            ..RuntimeConfig::default()
        });
        let sb = feed_scrollback(&mut rt);
        assert!(sb > 0, "need scrollback for {decoration:?}");
        rt.tick().expect("presents");
        let track = rt.scrollbar_track().expect("track");
        assert_eq!(
            (track.x, track.y, track.width, track.height),
            (x, y, 8, height),
            "decoration {decoration:?}"
        );
    }
}

#[test]
fn split_inner_gap_moves_track_to_focused_content_frame() {
    // `gaps_in` only enters through a split: each leaf's decorated content
    // frame excludes the shared inner band, and the track follows focus.
    let mut rt = runtime_with(ScrollbarMode::Always);
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    let sb = feed_scrollback(&mut rt);
    assert!(sb > 0, "need scrollback for the test");
    rt.tick().expect("presents");
    // Outer gaps_out 6 splits the 720px area into frames inset 6, then the
    // 6px inner band leaves two 351px frames; border 2 + content inset 6
    // shrink each content to (14, 14, 335, 428) and (371, 14, 335, 428);
    // track right edge = content + 8px pad.
    assert_eq!(
        rt.scrollbar_track(),
        Some(TrackRect {
            x: 349,
            y: 22,
            width: 8,
            height: 428,
        }),
        "focused first leaf"
    );
    assert!(rt.set_focus(ViewId::new(2)), "second leaf exists");
    assert_eq!(
        rt.scrollbar_track(),
        Some(TrackRect {
            x: 706,
            y: 22,
            width: 8,
            height: 428,
        }),
        "focused second leaf"
    );
}
