#![forbid(unsafe_code)]
//! Issue #2993: paging a floating panel must move its painted viewport.
//!
//! Owner report on 0.0.35: "Floating panels still have many problems, for
//! example they cannot page."
//!
//! `scroll_focused_page` (`Mod+u`/`Mod+i`) already resolved the focused float
//! and mutated its `View` scroll offset (issue #2139), and the wheel did the
//! same (issue #2945). The paint pass never saw it: it resolved each frame's
//! `View` through the live layout only, so a detached float fell back to a
//! synthesized `View` whose scroll offset is always zero. The offset moved in
//! the store while the panel kept painting its live grid, which is exactly
//! "nothing pages".
//!
//! RED on origin/main: `page_scroll_repaints_the_floating_panel_viewport` and
//! `wheel_scroll_repaints_the_floating_panel_viewport`.
//! Guards (green before and after): the offset arithmetic, the end clamps,
//! and the tiled pane's paging.

use bitty_platform::ScrollDelta;
use bitty_runtime::{
    AnimationPolicy, LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View,
    ViewId,
};

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless harness runtime must build")
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

fn offset(rt: &Runtime, view: ViewId) -> usize {
    rt.presented_scroll_offset(view)
        .unwrap_or_else(|| panic!("{view:?} must be live"))
}

/// Runtime with one detached float holding real scrollback.
///
/// The float is the primary owner (detach keeps primary-grid ownership on the
/// panel, CTX-1977), so the history fed before the detach is the panel's own
/// scrollback — the same shape a user sees after `Mod+a` on a busy pane.
fn floating_with_history() -> (Runtime, ViewId) {
    let mut rt = make_runtime();
    install(
        &mut rt,
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ),
    );
    for line in 0..200 {
        rt.handle_pty_bytes(format!("history line {line:03}\r\n").as_bytes());
    }
    // A still frame: hide the cursor so a blink phase can never be mistaken
    // for painted content motion.
    rt.handle_pty_bytes(b"\x1b[?25l");
    let float = ViewId::new(1);
    assert_eq!(rt.primary_view(), Some(float), "primary seed");
    rt.float_detach(float).expect("detach the primary pane");
    assert!(rt.set_focus(float), "the detached panel stays focusable");
    assert!(rt.tick().is_some(), "the panel's first frame presents");
    assert!(
        rt.state().scrollback_len() > 0,
        "the panel must own scrollback to page through"
    );
    assert_eq!(offset(&rt, float), 0, "a fresh panel shows live output");
    (rt, float)
}

/// Painted content pixels of `view`, excluding a two-pixel margin so the
/// decoration ring and the content inset never contribute.
fn content_pixels(rt: &Runtime, view: ViewId) -> Vec<u8> {
    let frame = frame_of(rt, view);
    let rgba = rt.headless_rgba().expect("deterministic RGBA frame");
    let width = usize::try_from(rt.config().window_extent().width()).expect("width fits");
    let pad = usize::try_from(rt.window_padding_physical()).expect("padding fits");
    let x0 = pad + usize::try_from(frame.content.x).expect("content x fits") + 2;
    let y0 = pad + usize::try_from(frame.content.y).expect("content y fits") + 2;
    let x1 = pad
        + usize::try_from(frame.content.x)
            .expect("content x fits")
            .saturating_add(usize::try_from(frame.content.width).expect("content width fits"))
        - 2;
    let y1 = pad
        + usize::try_from(frame.content.y)
            .expect("content y fits")
            .saturating_add(usize::try_from(frame.content.height).expect("content height fits"))
        - 2;
    assert!(
        x1 > x0 && y1 > y0,
        "panel content must have a paintable area"
    );
    let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
    for y in y0..y1 {
        let row = (y * width + x0) * 4;
        out.extend_from_slice(&rgba[row..row + (x1 - x0) * 4]);
    }
    out
}

#[test]
fn page_scroll_repaints_the_floating_panel_viewport() {
    let (mut rt, float) = floating_with_history();
    let live = content_pixels(&rt, float);

    assert!(
        rt.scroll_focused_page(true),
        "page up must reach the focused floating panel"
    );
    let paged_offset = offset(&rt, float);
    assert!(paged_offset > 0, "page up must enter scrollback");
    assert!(rt.tick().is_some(), "the paged frame presents");
    let paged = content_pixels(&rt, float);
    assert_ne!(
        live, paged,
        "page up must repaint the panel's viewport (offset {paged_offset})"
    );

    assert!(
        rt.scroll_focused_page(false),
        "page down must reach the focused floating panel"
    );
    assert_eq!(
        offset(&rt, float),
        0,
        "one page back returns to live output"
    );
    assert!(rt.tick().is_some(), "the live frame presents again");
    assert_eq!(
        live,
        content_pixels(&rt, float),
        "returning to live output must repaint exactly the live viewport"
    );
}

#[test]
fn wheel_scroll_repaints_the_floating_panel_viewport() {
    // Same paint path as paging (issue #2945 routes the wheel to the panel
    // under the pointer; a focused panel is the no-pointer fallback).
    let (mut rt, float) = floating_with_history();
    let live = content_pixels(&rt, float);

    rt.handle_wheel(ScrollDelta::Lines(0.0, 3.0));
    assert!(offset(&rt, float) > 0, "wheel up enters scrollback");
    assert!(rt.tick().is_some(), "the wheeled frame presents");
    assert_ne!(
        live,
        content_pixels(&rt, float),
        "wheel up must repaint the panel's viewport"
    );

    rt.handle_wheel(ScrollDelta::Lines(0.0, -3.0));
    assert_eq!(offset(&rt, float), 0, "wheel down returns to live output");
    assert!(rt.tick().is_some(), "the live frame presents again");
    assert_eq!(
        live,
        content_pixels(&rt, float),
        "wheel down must repaint exactly the live viewport"
    );
}

#[test]
fn page_scroll_uses_the_panel_row_count_and_clamps_at_the_ends() {
    let (mut rt, float) = floating_with_history();
    let rows = usize::from(frame_of(&rt, float).rows.max(1));
    let percent = rt.config().page_scroll_percent as usize;
    let page = rows.saturating_mul(percent).saturating_div(100).max(1);

    assert!(rt.scroll_focused_page(true));
    assert_eq!(
        offset(&rt, float),
        page,
        "a page is the configured share of the panel's own rows"
    );

    // Page to the top of history, then prove the clamp: no further motion.
    let mut guard = 0;
    while rt.scroll_focused_page(true) && guard < 64 {
        guard += 1;
    }
    let top = offset(&rt, float);
    assert!(top > page, "repeated pages walk into history");
    assert!(rt.scroll_focused_page(true), "paging stays callable");
    assert_eq!(
        offset(&rt, float),
        top,
        "paging past the oldest line must not move the viewport"
    );

    // Back to live output and prove the bottom clamp the same way.
    let mut guard = 0;
    while rt.scroll_focused_page(false) && offset(&rt, float) > 0 && guard < 64 {
        guard += 1;
    }
    assert_eq!(offset(&rt, float), 0, "paging down lands on live output");
    assert!(rt.scroll_focused_page(false), "paging stays callable");
    assert_eq!(
        offset(&rt, float),
        0,
        "paging below live output must not move the viewport"
    );
}

#[test]
fn tiled_pane_paging_is_unchanged() {
    // Guard: the shared paint lookup must keep the tiled path byte-identical.
    let mut rt = make_runtime();
    install(
        &mut rt,
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ),
    );
    for line in 0..200 {
        rt.handle_pty_bytes(format!("history line {line:03}\r\n").as_bytes());
    }
    rt.handle_pty_bytes(b"\x1b[?25l");
    let tiled = ViewId::new(1);
    assert!(rt.set_focus(tiled));
    assert!(rt.tick().is_some());
    let live = content_pixels(&rt, tiled);

    assert!(rt.scroll_focused_page(true));
    assert!(offset(&rt, tiled) > 0);
    assert!(rt.tick().is_some());
    assert_ne!(live, content_pixels(&rt, tiled), "a tiled pane still pages");
    assert!(rt.scroll_focused_page(false));
    assert!(rt.tick().is_some());
    assert_eq!(
        live,
        content_pixels(&rt, tiled),
        "a tiled pane still returns to the live viewport"
    );
}
