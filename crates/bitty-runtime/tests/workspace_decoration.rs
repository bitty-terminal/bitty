#![forbid(unsafe_code)]
//! Core-owned workspace decoration carry + application (CTX-0292).
//!
//! Pins the accepted workspace-compositor contract (spec CTX-0118):
//! - defaults `gaps_in = 4`, `gaps_out = 6`, `border = 2`, `radius = 6`
//!   logical px, carried from `bitty-ui` into `RuntimeConfig` unchanged;
//! - ranges `0..=32 / 0..=32 / 0..=8 / 0..=16` fail closed in
//!   [`Runtime::set_decoration`] and `RuntimeConfig::validate`;
//! - `decorated_allocations` applies `gaps_out` as a workspace-area inset,
//!   `gaps_in` as a band between siblings, `border` inside each View frame
//!   (content inset), and carries `radius` for clipping;
//! - [`bitty_ui::Decoration::ZERO`] frames are bit-identical to the plain
//!   cell allocations at the live cell metrics;
//! - the application is deterministic and never panics on saturation.

use bitty_runtime::{Decoration, LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

fn runtime_with_decoration(decoration: Decoration) -> Runtime {
    Runtime::new(RuntimeConfig {
        decoration,
        ..RuntimeConfig::default()
    })
    .expect("decorated config must build")
}

fn two_pane_split() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn cell_metrics(rt: &Runtime) -> (u16, u16) {
    let cfg = rt.config();
    (
        u16::try_from(cfg.cell_width).expect("cell width fits"),
        u16::try_from(cfg.cell_height).expect("cell height fits"),
    )
}

#[test]
fn runtime_config_carries_accepted_defaults() {
    // CTX-0292: the runtime mirror equals the accepted CTX-0118 defaults and
    // is the exact bitty-ui solver type (no value mapping).
    let rt = Runtime::new(RuntimeConfig::default()).expect("default runtime");
    assert_eq!(
        rt.decoration(),
        Decoration::new(4, 6, 2, 6),
        "runtime must carry the accepted defaults"
    );
    assert_eq!(rt.decoration(), Decoration::default());
}

#[test]
fn set_decoration_fails_closed_out_of_range() {
    let mut rt = runtime_with_decoration(Decoration::default());
    let cases = [
        (Decoration::new(33, 6, 2, 6), "gaps_in"),
        (Decoration::new(4, 33, 2, 6), "gaps_out"),
        (Decoration::new(4, 6, 9, 6), "border"),
        (Decoration::new(4, 6, 2, 17), "radius"),
    ];
    for (bad, field) in cases {
        let err = rt.set_decoration(bad).expect_err("out-of-range must fail");
        assert!(
            err.to_string().contains(field),
            "{field}: unexpected error {err}"
        );
        // Failed validation must not change the stored value.
        assert_eq!(rt.decoration(), Decoration::default());
    }
    // Boundary values are accepted.
    rt.set_decoration(Decoration::new(32, 32, 8, 16))
        .expect("boundary decoration valid");
    assert_eq!(rt.decoration(), Decoration::new(32, 32, 8, 16));
}

#[test]
fn set_decoration_adopts_live_and_repaints_once() {
    let mut rt = runtime_with_decoration(Decoration::default());
    let _ = rt.tick().expect("first tick presents");
    assert_eq!(rt.tick(), None, "idle on unchanged frame");
    let safe = Decoration::SAFE;
    rt.set_decoration(safe).expect("safe decoration valid");
    assert_eq!(rt.decoration(), safe);
    let stats = rt.tick().expect("live decoration change must repaint");
    assert!(stats.headless);
    assert_eq!(rt.tick(), None, "must idle after one repaint");
    // A no-op set does not force another frame.
    rt.set_decoration(safe).expect("same value valid");
    assert_eq!(rt.tick(), None, "no-op set must not repaint");
}

#[test]
fn decorated_allocations_apply_outer_gap_border_and_radius() {
    // Single leaf: gaps_out insets the workspace area; border insets the
    // content inside the frame; radius is carried unchanged.
    let mut rt = runtime_with_decoration(Decoration::new(0, 6, 2, 6));
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let (cw, ch) = cell_metrics(&rt);
    let area_w = 80 * cw;
    let area_h = 24 * ch;
    let out = rt.decorated_allocations();
    assert_eq!(out.len(), 1);
    let view = out[0].1;
    assert_eq!(
        view.frame,
        bitty_runtime::UiRect::new(6, 6, area_w - 12, area_h - 12)
    );
    assert_eq!(
        view.content,
        bitty_runtime::UiRect::new(8, 8, area_w - 16, area_h - 16)
    );
    assert_eq!(view.border, 2);
    assert_eq!(view.radius, 6);
}

#[test]
fn decorated_allocations_reserve_inner_gap_band() {
    // Two panes: gaps_in reserves a logical-px band between siblings; the
    // ratio applies to the gap-subtracted space.
    let mut rt = runtime_with_decoration(Decoration::new(10, 0, 0, 0));
    rt.set_layout(two_pane_split());
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let (cw, ch) = cell_metrics(&rt);
    let area_w = 80 * cw;
    let area_h = 24 * ch;
    let out = rt.decorated_allocations();
    assert_eq!(out.len(), 2);
    let (a, b) = (out[0].1.frame, out[1].1.frame);
    let avail = area_w - 10;
    let first = avail / 2;
    assert_eq!(a, bitty_runtime::UiRect::new(0, 0, first, area_h));
    assert_eq!(
        b,
        bitty_runtime::UiRect::new(first + 10, 0, avail - first, area_h)
    );
    // The band is exactly the configured px and sits between the frames.
    assert_eq!(b.x - a.right() as u16, 10);
    // Content equals frame when border is zero.
    assert_eq!(out[0].1.content, a);
    assert_eq!(out[1].1.content, b);
}

#[test]
fn zero_decoration_frames_match_plain_allocations_px() {
    // Decoration::ZERO is the undecorated fast path: frames are the plain
    // cell allocations scaled by the base (logical, scale-1.0) cell
    // metrics, content == frame.
    let mut rt = runtime_with_decoration(Decoration::ZERO);
    rt.set_layout(two_pane_split());
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let (cw, ch) = cell_metrics(&rt);
    let plain = rt.layout_allocations();
    let decorated = rt.decorated_allocations();
    assert_eq!(plain.len(), decorated.len());
    for ((pid, prect), (did, dview)) in plain.iter().zip(decorated.iter()) {
        assert_eq!(pid, did);
        let expected = bitty_runtime::UiRect::new(
            prect.x * cw,
            prect.y * ch,
            prect.width * cw,
            prect.height * ch,
        );
        assert_eq!(dview.frame, expected);
        assert_eq!(dview.content, expected);
    }
}

#[test]
fn decorated_allocations_are_deterministic_and_bounded() {
    let mut rt = runtime_with_decoration(Decoration::new(3, 5, 2, 6));
    rt.set_layout(two_pane_split());
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let a = rt.decorated_allocations();
    let b = rt.decorated_allocations();
    assert_eq!(a, b, "same state must yield identical frames");
    // Oversized decoration on a tiny area saturates without panicking.
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 1, 1));
    let tiny = rt.decorated_allocations();
    assert_eq!(tiny.len(), 2);
    for (_, view) in &tiny {
        assert!(view.content.width <= view.frame.width);
        assert!(view.content.height <= view.frame.height);
    }
}
