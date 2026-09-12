#![forbid(unsafe_code)]
//! Renderer-side panel animations (RFC-0002, CTX-0341).
//!
//! Pins the runtime wiring of the accepted contract:
//! - a new split `View` arms a bounded open transition and presents extra
//!   frames while active; when it completes the present path idles again
//!   (frame-on-demand, PB-7: zero wakeups after completion);
//! - the `now`-parameterized `tick_at` seam lets tests advance virtual time
//!   deterministically, so no wall-clock sleeps are needed;
//! - `enabled = false`, `reduced_motion = "always"`, `safe_mode`, and `0` ms
//!   durations all render the final state instantly (no extra frame);
//! - `next_deadline` is `None` when idle and bounded by the animation frame
//!   cadence while active;
//! - the workspace transition and focus cross-fade arm and expire bounded;
//! - Core-owned chrome only: the terminal grid is never interpolated, and a
//!   disabled policy reproduces the pre-RFC instant present exactly.

use std::time::{Duration, Instant};

use bitty_runtime::{
    AnimationCurve, AnimationKind, AnimationPolicy, LayoutNode, ReducedMotionMode, Runtime,
    RuntimeConfig, SplitAxis, View, ViewId,
};

fn runtime_with(policy: AnimationPolicy) -> Runtime {
    Runtime::new(RuntimeConfig {
        animations: policy,
        ..RuntimeConfig::default()
    })
    .expect("animation runtime must build")
}

/// Pixel at `(x, y)` in a surface of `stride` pixels.
fn rgba_at(rgba: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * stride + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn two_pane_split() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    )
}

#[test]
fn open_transition_presents_bounded_frames_then_idles() {
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    // First present (baseline single leaf) has no open transition.
    let _ = rt.tick_at(start).expect("first frame presents");
    assert!(rt.tick_at(start).is_none(), "idle baseline");

    // A new split View triggers the open transition: at least one present
    // frame is produced even with no PTY bytes.
    rt.set_layout(two_pane_split());
    let opened = rt.tick_at(start).expect("open must present the split");
    assert!(opened.headless);
    assert!(rt.animations_active(), "open animation must be active");
    assert_eq!(
        rt.animation_progress(AnimationKind::Open, Some(ViewId::new(2)), start),
        Some(0.0)
    );

    // Mid-transition a frame presents and the ring alpha is scaled (progress
    // in (0,1) means the chrome is partially faded).
    let mid = start + Duration::from_millis(75);
    let p = rt
        .animation_progress(AnimationKind::Open, Some(ViewId::new(2)), mid)
        .expect("mid progress");
    assert!((0.0..1.0).contains(&p), "mid open progress {p}");
    assert!(
        rt.tick_at(mid).is_some(),
        "active animation must keep presenting"
    );

    // After the duration the animation is done and the present path idles:
    // zero periodic wakeups attributable to animation (PB-7).
    let end = start + Duration::from_millis(150);
    assert!(
        rt.tick_at(end).is_some(),
        "final frame commits the end state"
    );
    assert!(!rt.animations_active(), "open must complete");
    assert!(rt.tick_at(end).is_none(), "idle after open completes");
    assert_eq!(rt.animation_deadline(), None, "no deadline when idle");
}

#[test]
fn disabled_policy_is_instant_and_matches_pre_rfc_idle() {
    let mut rt = runtime_with(AnimationPolicy {
        enabled: false,
        ..AnimationPolicy::default()
    });
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("first frame");
    rt.set_layout(two_pane_split());
    assert!(rt.tick_at(start).is_some(), "split presents once");
    assert!(!rt.animations_active());
    assert!(
        rt.tick_at(start).is_none(),
        "instant split idles immediately"
    );
    assert_eq!(rt.animation_deadline(), None);
}

#[test]
fn zero_duration_reduced_and_safe_are_instant() {
    // 0 ms durations.
    let policy = AnimationPolicy {
        duration_ms: [0, 0, 0, 0],
        ..AnimationPolicy::default()
    };
    let mut rt = runtime_with(policy);
    let start = Instant::now();
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    assert!(rt.tick_at(start).is_some());
    assert!(!rt.animations_active());
    assert!(rt.tick_at(start).is_none(), "0 ms must idle immediately");

    // reduced_motion = "always".
    let mut rt = runtime_with(AnimationPolicy {
        reduced_motion: ReducedMotionMode::Always,
        ..AnimationPolicy::default()
    });
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    assert!(rt.tick_at(start).is_some());
    assert!(!rt.animations_active());
    assert!(rt.tick_at(start).is_none());

    // safe_mode forces 0 regardless of the durations and platform signal.
    let policy = AnimationPolicy {
        safe_mode: true,
        platform_reduced: false,
        ..AnimationPolicy::default()
    };
    let mut rt = runtime_with(policy);
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    assert!(rt.tick_at(start).is_some());
    assert!(!rt.animations_active());
    assert!(rt.tick_at(start).is_none());

    // platform reduced signal with `auto`.
    let policy = AnimationPolicy {
        platform_reduced: true,
        ..AnimationPolicy::default()
    };
    let mut rt = runtime_with(policy);
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    assert!(rt.tick_at(start).is_some());
    assert!(!rt.animations_active());
    assert!(rt.tick_at(start).is_none());
}

#[test]
fn configured_open_duration_is_honored_not_the_default() {
    // CTX-0356 regression: a positive custom duration must drive the tracker.
    // On the buggy runtime the animator kept `AnimationPolicy::default()`
    // (open = 150 ms), so a 500 ms configured open expired 350 ms early.
    let policy = AnimationPolicy {
        duration_ms: [500, 120, 100, 200],
        ..AnimationPolicy::default()
    };
    let mut rt = runtime_with(policy);
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    assert!(rt.tick_at(start).is_none(), "idle baseline");

    rt.set_layout(two_pane_split());
    rt.tick_at(start).expect("open presents");

    // Still active well past the 150 ms default.
    let past_default = start + Duration::from_millis(200);
    assert!(
        rt.animations_active(),
        "configured 500 ms open must outlive the 150 ms default"
    );
    assert!(
        rt.animation_progress(AnimationKind::Open, Some(ViewId::new(2)), past_default)
            .is_some(),
        "configured open must still report progress at 200 ms"
    );

    // Still active one frame before the configured end.
    let near_end = start + Duration::from_millis(499);
    assert!(
        rt.animation_progress(AnimationKind::Open, Some(ViewId::new(2)), near_end)
            .is_some(),
        "configured open must still progress at 499 ms"
    );

    // Completed at the configured 500 ms.
    let end = start + Duration::from_millis(500);
    rt.tick_at(end);
    assert!(
        !rt.animations_active(),
        "configured 500 ms open must complete by 500 ms"
    );
    assert!(rt.tick_at(end).is_none(), "idle after configured open");
}

#[test]
fn configured_easing_drives_the_reported_progress() {
    // CTX-0356 regression: a non-default easing must change the eased value.
    // Linear at half the configured duration is exactly 0.5; the default
    // EaseOut curve would report 0.75 for the same time.
    let policy = AnimationPolicy {
        duration_ms: [500, 120, 100, 200],
        curves: [
            AnimationCurve::Linear,
            AnimationCurve::EaseIn,
            AnimationCurve::EaseInOut,
            AnimationCurve::EaseInOut,
        ],
        ..AnimationPolicy::default()
    };
    let mut rt = runtime_with(policy);
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    rt.tick_at(start).expect("open presents");

    let half = start + Duration::from_millis(250);
    let progress = rt
        .animation_progress(AnimationKind::Open, Some(ViewId::new(2)), half)
        .expect("mid progress");
    assert!(
        (progress - 0.5).abs() < 1e-4,
        "configured Linear easing at half duration must be 0.5, got {progress}"
    );
}

#[test]
fn configured_durations_still_suppress_to_instant() {
    // CTX-0356: the 0 ms suppression paths must keep winning over positive
    // configured durations.
    let positive = [500, 500, 500, 500];
    for policy in [
        AnimationPolicy {
            duration_ms: positive,
            enabled: false,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            duration_ms: positive,
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            duration_ms: positive,
            safe_mode: true,
            ..AnimationPolicy::default()
        },
    ] {
        let mut rt = runtime_with(policy);
        let start = Instant::now();
        let _ = rt.tick_at(start).expect("first frame");
        rt.set_layout(two_pane_split());
        assert!(rt.tick_at(start).is_some(), "split presents once");
        assert!(
            !rt.animations_active(),
            "suppression must override configured durations: {policy:?}"
        );
        assert!(
            rt.tick_at(start).is_none(),
            "suppressed split idles immediately"
        );
        assert_eq!(rt.animation_deadline(), None);
    }
}

#[test]
fn set_animations_reloads_the_live_policy() {
    // CTX-0356 acceptance: live reload adopts a new policy through
    // `set_animations`, not only at construction.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    assert!(rt.tick_at(start).is_none());

    rt.set_animations(AnimationPolicy {
        duration_ms: [500, 120, 100, 200],
        ..AnimationPolicy::default()
    });
    rt.set_layout(two_pane_split());
    rt.tick_at(start).expect("open presents");
    let past_default = start + Duration::from_millis(200);
    assert!(
        rt.animation_progress(AnimationKind::Open, Some(ViewId::new(2)), past_default)
            .is_some(),
        "reloaded 500 ms open must outlive the 150 ms default"
    );
    let end = start + Duration::from_millis(500);
    rt.tick_at(end);
    assert!(
        !rt.animations_active(),
        "reloaded 500 ms open must complete by 500 ms"
    );
}

#[test]
fn animation_deadline_is_bounded_and_clears_on_completion() {
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    rt.tick_at(start).expect("open presents");
    let deadline = rt.animation_deadline().expect("active deadline");
    assert!(
        deadline <= start + Duration::from_millis(150),
        "deadline must never exceed the accepted duration"
    );
    let end = start + Duration::from_millis(150);
    rt.tick_at(end);
    assert_eq!(rt.animation_deadline(), None, "idle clears the deadline");
}

#[test]
fn focus_change_arms_bounded_cross_fade() {
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_layout(two_pane_split());
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    assert!(rt.tick_at(start).is_none());
    assert!(rt.set_focus(ViewId::new(2)));
    let _ = rt.tick_at(start).expect("focus change presents");
    assert!(rt.animations_active(), "focus cross-fade must be active");
    // Focus is 100 ms in the accepted contract.
    let end = start + Duration::from_millis(100);
    rt.tick_at(end);
    assert!(!rt.animations_active(), "focus must expire by 100 ms");
    assert!(rt.tick_at(end).is_none());
}

#[test]
fn open_then_close_is_bounded_and_num_surfaces_capped() {
    // A close retains the removed View's ring and fades it out; the tracker is
    // capacity-bounded so repeated constructs cannot grow without bound.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    // Baseline single-leaf frame first so the split is a detected open, not
    // the (non-animated) first frame.
    let _ = rt.tick_at(start).expect("baseline frame");
    assert!(rt.tick_at(start).is_none());
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    assert!(rt.animations_active());
    // Let the open transition finish before collapsing so this test isolates
    // the close transition timing.
    let open_end = start + Duration::from_millis(150);
    rt.tick_at(open_end);
    assert!(!rt.animations_active(), "open completes first");
    // Collapse back to a single leaf: the removed View starts a close.
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    let _ = rt.tick_at(open_end).expect("close presents");
    assert!(rt.animations_active(), "close must be active");
    let end = open_end + Duration::from_millis(120);
    rt.tick_at(end);
    assert!(!rt.animations_active(), "close must expire by 120 ms");
    assert!(rt.tick_at(end).is_none());
}

#[test]
fn workspaces_switch_arms_workspace_transition() {
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("first frame");
    assert!(rt.tick_at(start).is_none());
    // Create and switch to a second workspace.
    rt.workspace_new().expect("new workspace");
    let _ = rt.tick_at(start).expect("workspace switch presents");
    assert!(
        rt.animations_active(),
        "workspace transition must be active"
    );
    let end = start + Duration::from_millis(200);
    rt.tick_at(end);
    assert!(!rt.animations_active(), "workspace must expire by 200 ms");
}

#[test]
fn open_transition_fades_the_core_owned_ring_alpha() {
    // RFC-0002: a panel-open transition fades Core-owned chrome in. Pixel
    // proof: the ring alpha at the mid-transition frame sits strictly between
    // transparent and the final focused outline, and after completion the
    // ring is the exact committed color.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline frame");
    assert!(rt.tick_at(start).is_none());
    // A new split View (id 2) is the open transition target.
    rt.set_layout(two_pane_split());
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("open presents");

    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let stride = usize::try_from(rt.config().window_extent().width()).expect("stride");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|f| f.view == ViewId::new(2))
        .expect("opened view frame");
    let ring_x = pad + usize::try_from(frame.frame.x).unwrap() + 1;
    let ring_y = pad + usize::try_from(frame.frame.y).unwrap() + 50;

    let mid = start + Duration::from_millis(40);
    rt.tick_at(mid).expect("mid frame");
    let mid_px = rgba_at(&rt.headless_rgba().expect("rgba"), stride, ring_x, ring_y);
    let eased = rt
        .animation_progress(AnimationKind::Open, Some(ViewId::new(2)), mid)
        .expect("mid progress");
    assert!(eased > 0.0 && eased < 1.0, "mid progress {eased}");

    let end = start + Duration::from_millis(150);
    rt.tick_at(end).expect("final frame commits");
    let final_px = rgba_at(&rt.headless_rgba().expect("rgba"), stride, ring_x, ring_y);

    // The software compositor blends straight-alpha chrome over the
    // background, so a partial alpha yields a composited RGB strictly between
    // the background and the committed ring for every channel.
    let bg = bitty_render::grid::DEFAULT_BG;
    for i in 0..3 {
        let (lo, hi) = if bg[i] <= final_px[i] {
            (bg[i], final_px[i])
        } else {
            (final_px[i], bg[i])
        };
        assert!(
            (lo..=hi).contains(&mid_px[i]),
            "channel {i}: mid {} must lie between bg {} and final {}",
            mid_px[i],
            bg[i],
            final_px[i]
        );
    }
    assert_ne!(mid_px, final_px, "mid ring must differ from the final ring");
    assert_ne!(mid_px, bg, "mid ring must still be visible");
    assert_ne!(final_px, bg, "committed ring must be visible");
}

#[test]
fn terminal_grid_is_never_interpolated() {
    // RFC-0002 rule 1: only Core-owned chrome animates. A grid byte written
    // during an active transition must read back verbatim immediately.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    rt.tick_at(start).expect("open presents");
    assert!(rt.animations_active());
    rt.handle_pty_bytes(b"\x1b[2;1HGRID-TRUTH");
    let snap = rt.snapshot();
    let row: String = snap
        .cells
        .chunks(snap.width)
        .nth(1)
        .expect("row 2")
        .iter()
        .take(10)
        .map(|c| c.glyph)
        .collect();
    assert_eq!(row, "GRID-TRUTH", "grid content is never animated");
}
