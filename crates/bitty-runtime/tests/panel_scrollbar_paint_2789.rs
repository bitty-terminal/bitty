//! Issue #2789: the painted scrollbar follows the painted frames.
//!
//! A paint-only geometry transition (an accepted open/close/workspace tween)
//! reuses the committed grid while moving the painted rectangle. The overlay
//! thumb must sample that painted rectangle, not the committed one, or the
//! handle tears away from the visible panel edge for the whole transition.
//!
//! `Runtime::scrollbar_geometry_at` is the headless twin of the present
//! path's resolution (the present layer resolves the fill from the frames it
//! paints). [`bitty_runtime::Runtime::scrollbar_track`] keeps reporting the
//! committed geometry that input routes against, so this contract cannot be
//! observed through it and needs the seam this fix adds.

use std::time::{Duration, Instant};

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

fn two_pane_horizontal() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

/// The geometry the present layer paints must resolve from the painted
/// frames: same content rect, and a thumb derived from that track plus the
/// committed viewport snapshot (rows, scrollback, scroll offset).
fn assert_scrollbar_matches_painted_viewport(
    rt: &Runtime,
    now: Instant,
    painted: &[bitty_runtime::PresentFrame],
) {
    let Some((track, thumb)) = rt.scrollbar_geometry_at(now) else {
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
        "the painted track must hug the painted content, not the committed one"
    );
    let view = rt
        .layout()
        .find_leaf(focused)
        .expect("focused leaf is live");
    let expected = thumb_geometry(
        track.height,
        usize::from(view.rows()),
        rt.scrollback_len(),
        view.scroll_offset(),
    )
    .expect("scrollback geometry resolves");
    assert_eq!(
        thumb, expected,
        "the painted thumb must derive from the painted viewport"
    );
    assert!(
        thumb.y.saturating_add(thumb.height) <= track.height,
        "the thumb must stay inside its track"
    );
}

#[test]
fn split_resize_paints_its_scrollbar_from_the_painted_frames() {
    // The invariant outside a live gesture: a split resize interpolates the
    // kept pane's painted bounds by design (CTX-1335), so the scrollbar must
    // follow the painted rect during the transition, not the committed one.
    let mut rt = runtime_with_scrollbar();
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    for i in 0..40 {
        rt.handle_pty_bytes(format!("scrollback line {i:02}\r\n").as_bytes());
    }
    let _ = rt.tick_at(start).expect("content presents");

    rt.set_layout(two_pane_horizontal());
    let _ = rt.tick_at(start).expect("split presents");
    let mid = start + Duration::from_millis(30);
    rt.tick_at(mid).expect("mid transition presents");
    let painted = rt.paint_frames_at(mid);
    assert_ne!(
        painted,
        rt.present_frames(),
        "fixture needs an in-flight bounds tween"
    );
    assert_scrollbar_matches_painted_viewport(&rt, mid, &painted);
}
