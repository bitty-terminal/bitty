#![forbid(unsafe_code)]
//! Renderer-side panel animations (RFC-0002, CTX-0341; CTX-0967 move/resize/drag;
//! CTX-1078 splits/zoom/workspaces; CTX-1335 paint geometry).
//!
//! Pins the runtime wiring of the accepted contract:
//! - a new split `View` arms a bounded open transition and presents extra
//!   frames while active; when it completes the present path idles again
//!   (frame-on-demand, PB-7: zero wakeups after completion);
//! - single-pane zoom engage/disengage arms bounded close/open transitions
//!   through the same chrome-ring fade; the logical layout, PTY size,
//!   content, and hit-test frames commit immediately (CTX-1335 adds
//!   paint-only bounds interpolation);
//! - workspace switches (new/switch/prev/next/last) arm the bounded
//!   workspace cross-fade; the committed panel bounds and terminal grid stay
//!   fixed throughout the chrome-only transition;
//! - panel move (same-workspace reposition, cross-workspace reparent),
//!   panel resize (border-drag divider, keyboard step), and panel drag
//!   (Alt+drag float move) arm their own bounded transitions from the
//!   gesture paths; the layout commits immediately and only Core-owned
//!   chrome fades plus the painted bounds interpolate (terminal content is
//!   never interpolated);
//! - CTX-1335 geometry interpolation is paint-only: start/mid/end painted
//!   bounds run on the virtual clock while `present_frames()` stays final;
//!   instant/reduced/safe policies paint the end bounds on the first frame,
//!   and float detach/restore (tier changes) fail closed to instant bounds
//!   so the opaque float cover never bleeds the base beneath it;
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

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{
    AnimationCurve, AnimationKind, AnimationPolicy, LayoutNode, MAX_CONCURRENT_ANIMATIONS,
    OverlayTier, ReducedMotionMode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
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
    // The virtual clock, not `Instant::now()`: a loaded runner stalling the
    // thread between the tick and the assert must not read the transition as
    // already expired (bitty#2781).
    assert!(
        rt.animations_active_at(start),
        "open animation must be active"
    );
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
    assert!(!rt.animations_active_at(end), "open must complete");
    assert!(rt.tick_at(end).is_none(), "idle after open completes");
    assert_eq!(rt.animation_deadline_at(end), None, "no deadline when idle");
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
        duration_ms: [0; AnimationKind::COUNT],
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
        duration_ms: [500, 120, 100, 200, 150, 120, 150],
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
        rt.animations_active_at(past_default),
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
        duration_ms: [500, 120, 100, 200, 150, 120, 150],
        curves: [
            AnimationCurve::Linear,
            AnimationCurve::EaseIn,
            AnimationCurve::EaseInOut,
            AnimationCurve::EaseInOut,
            AnimationCurve::EaseInOut,
            AnimationCurve::EaseInOut,
            AnimationCurve::EaseOut,
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
    let positive = [500; AnimationKind::COUNT];
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
        duration_ms: [500, 120, 100, 200, 150, 120, 150],
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
    // Query on the same virtual clock `tick_at` armed the transition with.
    // The wall-clock `animation_deadline()` folds in "now + frame interval"
    // and reads as beyond the accepted duration when a loaded CI scheduler
    // stalls this thread between arming and the assertion (bitty#2781).
    let deadline = rt.animation_deadline_at(start).expect("active deadline");
    assert!(
        deadline <= start + Duration::from_millis(150),
        "deadline must never exceed the accepted duration"
    );
    let end = start + Duration::from_millis(150);
    rt.tick_at(end);
    assert_eq!(
        rt.animation_deadline_at(end),
        None,
        "idle clears the deadline"
    );
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
    // Query the arming on the same virtual clock `tick_at` uses. The
    // wall-clock `animations_active()` reads the cross-fade as already
    // expired when a loaded CI scheduler stalls this thread for more than
    // the 100 ms focus duration between arming and the assertion (CTX-0408).
    assert!(
        rt.animation_progress(AnimationKind::Focus, Some(ViewId::new(2)), start)
            .is_some(),
        "focus cross-fade must be active"
    );
    // Focus is 100 ms in the accepted contract.
    let end = start + Duration::from_millis(100);
    rt.tick_at(end);
    assert!(
        rt.animation_progress(AnimationKind::Focus, Some(ViewId::new(2)), end)
            .is_none(),
        "focus must expire by 100 ms"
    );
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
    assert!(rt.animations_active_at(start));
    // Let the open transition finish before collapsing so this test isolates
    // the close transition timing.
    let open_end = start + Duration::from_millis(150);
    rt.tick_at(open_end);
    assert!(!rt.animations_active_at(open_end), "open completes first");
    // Collapse back to a single leaf: the removed View starts a close.
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    let _ = rt.tick_at(open_end).expect("close presents");
    assert!(rt.animations_active_at(open_end), "close must be active");
    let end = open_end + Duration::from_millis(120);
    rt.tick_at(end);
    assert!(!rt.animations_active_at(end), "close must expire by 120 ms");
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
        rt.animations_active_at(start),
        "workspace transition must be active"
    );
    let end = start + Duration::from_millis(200);
    rt.tick_at(end);
    assert!(
        !rt.animations_active_at(end),
        "workspace must expire by 200 ms"
    );
}

#[test]
fn workspace_switch_fades_no_ring_for_stashed_views() {
    // #2134: switching workspaces stashes the old panels instead of
    // closing them, so no close fade may arm for them — retained rings
    // fading out over the new workspace read as ghosting. The workspace
    // transition itself still arms.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("first frame");
    assert!(rt.tick_at(start).is_none());
    let old = rt.focused_view().expect("single leaf focused");
    rt.workspace_new().expect("new workspace");
    let _ = rt.tick_at(start).expect("workspace switch presents");
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, start)
            .is_some(),
        "workspace transition must be active"
    );
    assert!(
        rt.animation_progress(AnimationKind::Close, Some(old), start)
            .is_none(),
        "stashed views must not fade out on switch"
    );
}

#[test]
fn explicit_close_still_fades_despite_stale_active_slot() {
    // CodeRabbit on #2134: the active slot refreshes only on switch-away,
    // so after an explicit close it still names the destroyed leaf. The
    // close gate must exclude the active slot, or the destroyed view would
    // wrongly skip its fade.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("first frame presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    // Round-trip through a second workspace so the active slot holds a
    // fresh copy of the two-pane layout (stale after the close below).
    rt.workspace_new().expect("new workspace");
    let _ = rt.tick_at(start).expect("switch presents");
    assert!(rt.workspace_switch(0), "switch back must succeed");
    let _ = rt.tick_at(start).expect("switch back presents");
    // Explicitly close leaf v:2; the stashed active slot still names it.
    rt.set_layout_closing(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        ViewId::new(2),
    );
    let _ = rt.tick_at(start).expect("close presents");
    assert!(
        rt.animation_progress(AnimationKind::Close, Some(ViewId::new(2)), start)
            .is_some(),
        "destroyed views must still fade out"
    );
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
    let ring_x = pad + usize::try_from(frame.frame.x).unwrap();
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
    assert!(rt.animations_active_at(start));
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

// ── CTX-1335: paint-only panel-bounds interpolation ──────────────────────

fn frame_for(frames: &[bitty_runtime::PresentFrame], view: ViewId) -> bitty_runtime::PresentFrame {
    *frames
        .iter()
        .find(|frame| frame.view == view)
        .expect("view must be present")
}

#[test]
fn split_resizes_interpolate_pane_bounds_on_the_virtual_clock() {
    // A split commits the final layout, PTY size, and hit-test geometry on
    // the first frame; only the painted bounds move from the committed start
    // to the committed end over the accepted Resize duration.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    assert!(rt.tick_at(start).is_none());
    let before = frame_for(&rt.present_frames(), ViewId::new(1));

    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let logical = rt.present_frames();
    let target = frame_for(&logical, ViewId::new(1));
    assert_ne!(before.frame, target.frame, "split resizes the kept pane");

    // Frame zero paints the committed start bounds; hit-testing already sees
    // the committed end bounds.
    let painted_start = frame_for(&rt.paint_frames_at(start), ViewId::new(1));
    assert_eq!(painted_start.frame, before.frame);
    assert_eq!(painted_start.content, before.content);
    assert_eq!(
        rt.present_frames(),
        logical,
        "logical/hit-test frames never animate"
    );

    let mid = start + Duration::from_millis(60);
    rt.tick_at(mid).expect("mid frame presents");
    let painted_mid = frame_for(&rt.paint_frames_at(mid), ViewId::new(1));
    assert_ne!(painted_mid.frame, before.frame, "mid must have moved");
    assert_ne!(painted_mid.frame, target.frame, "mid must not be final");
    // The kept pane shrinks: its width interpolates monotonically between the
    // committed start and end spans.
    let lo = before.frame.width.min(target.frame.width);
    let hi = before.frame.width.max(target.frame.width);
    assert!(
        (lo..=hi).contains(&painted_mid.frame.width),
        "mid width {} must lie between {lo} and {hi}",
        painted_mid.frame.width
    );
    assert_eq!(
        rt.present_frames(),
        logical,
        "hit-testing stays final during the animation"
    );

    // Open is 150 ms, so the whole split settles by then; the final frame is
    // the exact committed end state and the present path idles.
    let end = start + Duration::from_millis(150);
    rt.tick_at(end).expect("final frame commits");
    let painted_end = frame_for(&rt.paint_frames_at(end), ViewId::new(1));
    assert_eq!(painted_end.frame, target.frame);
    assert_eq!(painted_end.content, target.content);
    assert!(!rt.animations_active());
    assert!(rt.tick_at(end).is_none());
    assert_eq!(rt.animation_deadline(), None);
}

#[test]
fn instant_reduced_and_safe_paths_skip_bounds_interpolation() {
    // First-class instant paths: disabled, reduced motion, safe mode, and
    // zero durations all paint the committed end bounds on the first frame
    // and produce no extra animation wakeups.
    for policy in [
        AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            duration_ms: [0; AnimationKind::COUNT],
            ..AnimationPolicy::default()
        },
    ] {
        let mut rt = runtime_with(policy);
        let start = Instant::now();
        rt.set_container(UiRect::new(0, 0, 80, 24));
        let _ = rt.tick_at(start).expect("baseline presents");
        rt.set_layout(two_pane_split());
        let _ = rt.tick_at(start).expect("split presents");
        let logical = rt.present_frames();
        let paint = rt.paint_frames_at(start);
        assert_eq!(
            paint, logical,
            "instant path must use final bounds: {policy:?}"
        );
        assert!(!rt.animations_active(), "{policy:?}");
        assert!(rt.tick_at(start).is_none(), "{policy:?}");
        assert_eq!(rt.animation_deadline(), None, "{policy:?}");
    }
}

#[test]
fn workspace_switch_keeps_committed_bounds_and_hits() {
    // Workspace transitions are chrome-only. A lower-index switch must not
    // translate incoming panels from a negative x origin, and the painted
    // frames must remain exactly equal to the committed hit-test frames while
    // the global workspace cross-fade is active.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    assert!(rt.tick_at(start).is_none());

    // Build and settle the two-panel workspace at index 0, retaining focus on
    // its second panel so the switch-back also proves workspace focus state.
    rt.set_layout(two_pane_split());
    rt.tick_at(start).expect("split presents");
    let split_end = start + Duration::from_millis(200);
    rt.tick_at(split_end).expect("split settles");
    assert!(rt.tick_at(split_end).is_none(), "split is idle");
    assert!(rt.set_focus(ViewId::new(2)));
    rt.tick_at(split_end).expect("focus presents");
    let focus_end = split_end + Duration::from_millis(100);
    rt.tick_at(focus_end).expect("focus settles");
    assert!(rt.tick_at(focus_end).is_none(), "focus is idle");

    // Create workspace 1, then give it two different panels and settle that
    // layout. Switching back to workspace 0 therefore exercises a lower-index
    // multi-panel transition rather than the single-leaf fast path.
    rt.workspace_new().expect("new workspace");
    let workspace_one_start = focus_end + Duration::from_millis(200);
    rt.tick_at(workspace_one_start)
        .expect("new workspace presents");
    let workspace_one_end = workspace_one_start + Duration::from_millis(200);
    rt.tick_at(workspace_one_end)
        .expect("new workspace settles");
    assert!(rt.tick_at(workspace_one_end).is_none());
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(4), 40, 24)),
    ));
    let workspace_one_split = workspace_one_end + Duration::from_millis(200);
    rt.tick_at(workspace_one_split)
        .expect("workspace one split presents");
    let workspace_one_split_end = workspace_one_split + Duration::from_millis(200);
    rt.tick_at(workspace_one_split_end)
        .expect("workspace one split settles");
    assert!(rt.tick_at(workspace_one_split_end).is_none());

    assert!(rt.workspace_switch(0), "switch to lower workspace");
    let switch_start = workspace_one_split_end + Duration::from_millis(200);
    rt.tick_at(switch_start)
        .expect("lower workspace switch presents");
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, switch_start)
            .is_some(),
        "workspace chrome transition remains active"
    );

    let assert_committed_paint_and_hits = |rt: &Runtime, now: Instant| {
        let committed = rt.present_frames();
        let painted = rt.paint_frames_at(now);
        assert_eq!(
            painted, committed,
            "workspace switch must not animate panels"
        );
        let viewport_right = i64::from(rt.config().window_extent().width());
        for frame in &committed {
            let right = i64::from(frame.frame.x) + i64::from(frame.frame.width);
            assert!(
                frame.frame.x >= 0,
                "panel {} entered from x<0",
                frame.view.0
            );
            assert!(
                right <= viewport_right,
                "panel {} extends outside the viewport",
                frame.view.0
            );
            let hit = rt
                .cursor_to_present_cell(point_in_frame(rt, frame.view, 0.5, 0.5))
                .map(|(view, _)| view);
            assert_eq!(
                hit,
                Some(frame.view),
                "committed hit-test for {}",
                frame.view.0
            );
        }
    };
    assert_committed_paint_and_hits(&rt, switch_start);

    let mid = switch_start + Duration::from_millis(100);
    rt.tick_at(mid).expect("mid workspace frame presents");
    assert_committed_paint_and_hits(&rt, mid);

    let end = switch_start + Duration::from_millis(200);
    rt.tick_at(end).expect("final workspace frame commits");
    assert_committed_paint_and_hits(&rt, end);
    assert!(!rt.animations_active());
    assert!(rt.tick_at(end).is_none());
}

#[test]
fn workspace_switch_is_instant_under_reduced_motion() {
    for policy in [
        AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
    ] {
        let mut rt = runtime_with(policy);
        let start = Instant::now();
        rt.set_container(UiRect::new(0, 0, 80, 24));
        let _ = rt.tick_at(start).expect("baseline presents");
        rt.workspace_new().expect("new workspace");
        let _ = rt.tick_at(start).expect("switch presents");
        assert_eq!(
            rt.paint_frames_at(start),
            rt.present_frames(),
            "reduced/instant workspace transition must paint final bounds: {policy:?}"
        );
        assert!(!rt.animations_active(), "{policy:?}");
        assert!(rt.tick_at(start).is_none(), "{policy:?}");
    }
}

/// Physical cursor position at fractional (`fx`, `fy`) offsets inside
/// `view`'s committed present frame, derived from public geometry only.
fn point_in_frame(rt: &Runtime, view: ViewId, fx: f64, fy: f64) -> CursorPosition {
    let frame = frame_for(&rt.present_frames(), view);
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.frame.x) + f64::from(frame.frame.width) * fx,
        y: pad + f64::from(frame.frame.y) + f64::from(frame.frame.height) * fy,
    }
}

#[test]
fn float_tier_changes_cut_a_mid_tween_bounds_tween() {
    // CTX-1335 F1: a detach or restore during an in-flight geometry tween
    // must cancel it, not merely decline to re-arm. The painted frame is the
    // committed rect on the first present after the tier flips, so the float
    // cover/occluders can never own an obsolete rectangle.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let mid = start + Duration::from_millis(30);
    rt.tick_at(mid).expect("mid tween frame presents");
    assert!(
        rt.paint_frames_at(mid) != rt.present_frames(),
        "a split resize tween is actually in flight"
    );

    rt.toggle_floating(Some(ViewId::new(1)))
        .expect("detach view 1");
    let after = start + Duration::from_millis(40);
    let _ = rt.tick_at(after);
    let target = frame_for(&rt.present_frames(), ViewId::new(1));
    assert_eq!(target.tier, Some(OverlayTier::Float));
    let painted = frame_for(&rt.paint_frames_at(after), ViewId::new(1));
    assert_eq!(painted.tier, target.tier, "float tier committed");
    assert_eq!(
        painted.frame, target.frame,
        "detach must cut the stale tween and paint the committed float frame"
    );
    assert_eq!(painted.content, target.content);
    assert_eq!(painted.cols, target.cols);
    assert_eq!(painted.rows, target.rows);

    // Restore direction: move the detached float (arming a live drag/move
    // bounds tween), then restore mid-tween.
    let settled = after + Duration::from_millis(200);
    rt.tick_at(settled);
    let grab = point_in_frame(&rt, ViewId::new(1), 0.5, 0.5);
    rt.handle_cursor_moved(grab);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.alt_drag_active(),
        "Alt+press on the detached float grabs"
    );
    let t = Instant::now();
    let moved = CursorPosition {
        x: grab.x + 18.0,
        y: grab.y + 38.0,
    };
    assert!(rt.update_alt_drag_at(moved, t), "drag motion must apply");
    let _ = rt.tick_at(t).expect("drag present attaches bounds");
    let drag_mid = t + Duration::from_millis(30);
    rt.tick_at(drag_mid).expect("drag mid frame presents");
    assert!(
        rt.paint_frames_at(drag_mid) != rt.present_frames(),
        "float move tween is in flight before restore"
    );

    rt.toggle_floating(Some(ViewId::new(1)))
        .expect("restore view 1");
    let restored_at = drag_mid + Duration::from_millis(5);
    let _ = rt.tick_at(restored_at);
    let target = frame_for(&rt.present_frames(), ViewId::new(1));
    assert_eq!(
        target.tier, None,
        "restore returns the leaf to the tiled base"
    );
    let painted = frame_for(&rt.paint_frames_at(restored_at), ViewId::new(1));
    assert_eq!(painted.tier, target.tier, "tiled tier committed");
    assert_eq!(
        painted.frame, target.frame,
        "restore must cut the stale float tween and paint the committed tiled frame"
    );
    assert_eq!(painted.content, target.content);
    assert_eq!(painted.cols, target.cols);
    assert_eq!(painted.rows, target.rows);
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn non_animating_kind_cancels_a_different_kind_bounds_tween() {
    // CTX-1335 F2: with move animated and resize instant, a move tween in
    // flight followed by a size-only delta must cut the stale move tween and
    // paint the committed end bounds.
    let mut rt = runtime_with(AnimationPolicy {
        duration_ms: [150, 120, 100, 200, 150, 0, 150],
        ..AnimationPolicy::default()
    });
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "settled baseline");

    let t = Instant::now();
    rt.workspace_move_focused_to_position_at(1, t)
        .expect("reposition must succeed");
    let _ = rt.tick_at(t).expect("move present attaches bounds");
    let mid = t + Duration::from_millis(30);
    rt.tick_at(mid).expect("move mid frame presents");
    assert_ne!(
        frame_for(&rt.paint_frames_at(mid), ViewId::new(1)).frame,
        frame_for(&rt.present_frames(), ViewId::new(1)).frame,
        "move tween is painting"
    );

    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.6,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    ));
    let after = t + Duration::from_millis(40);
    let _ = rt.tick_at(after);
    let target = frame_for(&rt.present_frames(), ViewId::new(1));
    let painted = frame_for(&rt.paint_frames_at(after), ViewId::new(1));
    assert_eq!(
        painted.frame, target.frame,
        "non-animating resize must cut the stale move tween"
    );
    assert_eq!(painted.content, target.content);
}

#[test]
fn capacity_refused_bounds_paint_the_committed_end() {
    // CTX-1335 F2/F4: with the shared budget full, a surface with no
    // reusable slot refuses its bounds tween and paints the committed end
    // bounds on that frame.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "settled baseline");

    let t = Instant::now();
    rt.handle_cursor_moved(border_cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active(), "border press must grab");
    assert!(rt.update_border_drag_at(border_cell_pixels(45, 12), t));
    for i in 0..(MAX_CONCURRENT_ANIMATIONS - 1) {
        assert!(rt.trigger_animation(AnimationKind::Open, Some(ViewId::new(100 + i as u64)), t));
    }
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.7,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    ));
    let after = t + Duration::from_millis(20);
    let _ = rt.tick_at(after);
    assert_eq!(
        rt.paint_frames_at(after),
        rt.present_frames(),
        "capacity-refused bounds must paint the committed geometry"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn retargeting_starts_from_the_currently_painted_bounds() {
    // CTX-1335 F4: a live update retargets from the painted frame at that
    // instant, so the next present is continuous (no jump back to the
    // previous committed start). Pinned through the reparent path: issue
    // #2789 makes a divider drag paint its per-step commit instead of
    // interpolating, so resize no longer owns this contract.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.34,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
        ),
    ));
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "settled baseline");

    let t = Instant::now();
    assert_eq!(
        rt.workspace_move_focused_to_position_at(2, t)
            .expect("first reposition"),
        ViewId::new(1)
    );
    let _ = rt.tick_at(t).expect("first move present");
    let first_target = frame_for(&rt.present_frames(), ViewId::new(1)).frame;
    let mid = t + Duration::from_millis(40);
    rt.tick_at(mid).expect("move mid frame presents");
    assert_ne!(
        frame_for(&rt.paint_frames_at(mid), ViewId::new(1)).frame,
        first_target,
        "mid tween is not at its target"
    );

    // A second reposition re-arms the Move entry. Its carried start must be
    // the painted mid frame, so the present at the same instant is continuous.
    let t2 = mid + Duration::from_millis(1);
    let continuity = frame_for(&rt.paint_frames_at(t2), ViewId::new(1)).frame;
    assert_eq!(
        rt.workspace_move_focused_to_position_at(0, t2)
            .expect("second reposition"),
        ViewId::new(1)
    );
    let target = frame_for(&rt.present_frames(), ViewId::new(1)).frame;
    assert_ne!(target, first_target, "the second move names another slot");
    let _ = rt.tick_at(t2).expect("retarget present");
    let retargeted = frame_for(&rt.paint_frames_at(t2), ViewId::new(1)).frame;
    assert_eq!(
        retargeted, continuity,
        "retarget must restart from the currently painted bounds"
    );
    assert_ne!(
        retargeted, target,
        "retargeted tween has not already settled"
    );
}

#[test]
fn alt_drag_float_keeps_one_animation_entry_under_a_full_budget() {
    // CTX-1335 F3: an Alt-drag float holds the Drag ring; the per-view move
    // bounds ride that entry instead of consuming a second budget slot. With
    // the shared budget otherwise full, interpolation still works and the
    // ring alpha is multiplied once.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    rt.toggle_floating(Some(ViewId::new(1)))
        .expect("detach view 1");
    let detach_settled = settled + Duration::from_millis(200);
    rt.tick_at(detach_settled);
    let detach_idle = detach_settled + Duration::from_millis(200);
    rt.tick_at(detach_idle);
    assert!(rt.tick_at(detach_idle).is_none(), "detach settled");

    let grab = point_in_frame(&rt, ViewId::new(1), 0.5, 0.5);
    rt.handle_cursor_moved(grab);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.alt_drag_active(),
        "Alt+press on the detached float grabs"
    );
    // Commit the pointer focus change before filling the budget so this
    // present does not also arm a focus fade that would legitimately evict a
    // bounds entry at the shared cap.
    let focus_settled = Instant::now();
    rt.tick_at(focus_settled);
    let focus_idle = focus_settled + Duration::from_millis(200);
    rt.tick_at(focus_idle);
    assert!(rt.tick_at(focus_idle).is_none(), "focus settled");
    let old_frame = frame_for(&rt.present_frames(), ViewId::new(1)).frame;
    let t = Instant::now();
    let moved = CursorPosition {
        x: grab.x + 18.0,
        y: grab.y + 38.0,
    };
    assert!(rt.update_alt_drag_at(moved, t));
    let target = frame_for(&rt.present_frames(), ViewId::new(1)).frame;
    assert_ne!(old_frame, target, "drag moved the committed float frame");
    for i in 0..(MAX_CONCURRENT_ANIMATIONS - 1) {
        assert!(rt.trigger_animation(AnimationKind::Open, Some(ViewId::new(200 + i as u64)), t));
    }
    let _ = rt.tick_at(t).expect("drag present attaches bounds");
    assert_eq!(
        frame_for(&rt.paint_frames_at(t), ViewId::new(1)).frame,
        old_frame,
        "attached tween starts at the committed previous bounds"
    );
    let mid = t + Duration::from_millis(75);
    rt.tick_at(mid).expect("drag mid frame presents");
    let painted = frame_for(&rt.paint_frames_at(mid), ViewId::new(1)).frame;
    assert_ne!(painted, old_frame, "mid drag must have moved");
    assert_ne!(
        painted, target,
        "a full budget must not refuse the drag's own bounds slot"
    );
    assert!(
        rt.animation_progress(AnimationKind::Move, Some(ViewId::new(1)), mid)
            .is_none(),
        "bounds ride the drag entry, not a second move entry"
    );
    assert!(
        rt.animation_progress(AnimationKind::Drag, Some(ViewId::new(1)), mid)
            .is_some()
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn consecutive_alt_drag_updates_carry_painted_bounds_forward() {
    // CodeRabbit PR #2231 finding 1: re-arming the Drag ring on a second
    // motion must start from the currently painted frame, not from the last
    // committed target, so continuous Alt+drag never snaps.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    rt.toggle_floating(Some(ViewId::new(1)))
        .expect("detach view 1");
    let detach_settled = settled + Duration::from_millis(200);
    rt.tick_at(detach_settled);
    let detach_idle = detach_settled + Duration::from_millis(200);
    rt.tick_at(detach_idle);
    assert!(rt.tick_at(detach_idle).is_none(), "detach settled");

    let grab = point_in_frame(&rt, ViewId::new(1), 0.5, 0.5);
    rt.handle_cursor_moved(grab);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.alt_drag_active(),
        "Alt+press on the detached float grabs"
    );
    let focus_settled = Instant::now();
    rt.tick_at(focus_settled);
    let focus_idle = focus_settled + Duration::from_millis(200);
    rt.tick_at(focus_idle);
    assert!(rt.tick_at(focus_idle).is_none(), "focus settled");

    let t0 = Instant::now();
    let first = CursorPosition {
        x: grab.x + 18.0,
        y: grab.y + 38.0,
    };
    assert!(rt.update_alt_drag_at(first, t0));
    let _ = rt.tick_at(t0).expect("first drag present attaches bounds");
    let first_target = frame_for(&rt.present_frames(), ViewId::new(1)).frame;

    let mid = t0 + Duration::from_millis(40);
    rt.tick_at(mid).expect("mid drag frame presents");
    let continuity = frame_for(&rt.paint_frames_at(mid), ViewId::new(1)).frame;
    assert_ne!(continuity, first_target, "mid tween is not at its target");

    // A second motion re-arms the same Drag entry. Its carried start must be
    // the painted mid frame, so the present at the same instant is continuous.
    let second = CursorPosition {
        x: first.x + 18.0,
        y: first.y + 38.0,
    };
    assert!(rt.update_alt_drag_at(second, mid));
    let second_target = frame_for(&rt.present_frames(), ViewId::new(1)).frame;
    assert_ne!(second_target, first_target, "second motion moved further");
    let _ = rt.tick_at(mid).expect("second drag present retargets");
    let retargeted = frame_for(&rt.paint_frames_at(mid), ViewId::new(1)).frame;
    assert_eq!(
        retargeted, continuity,
        "the retarget must not snap back to the previous committed target"
    );

    let later = mid + Duration::from_millis(40);
    rt.tick_at(later).expect("later drag frame presents");
    let later_paint = frame_for(&rt.paint_frames_at(later), ViewId::new(1)).frame;
    assert_ne!(later_paint, continuity, "the retargeted tween advances");
    assert_ne!(later_paint, second_target, "and has not already settled");
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn pinned_overlay_does_not_slide_with_the_workspace() {
    // CTX-1335 F5: pins are window-global chrome; the incoming workspace
    // transition runs beneath them instead of moving them off-screen.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    rt.pin_floating(ViewId::new(1)).expect("pin view 1");
    let pinned_settled = settled + Duration::from_millis(200);
    rt.tick_at(pinned_settled);

    let t = Instant::now();
    rt.workspace_new().expect("new workspace");
    let _ = rt.tick_at(t).expect("workspace switch presents");
    let target = frame_for(&rt.present_frames(), ViewId::new(1));
    let painted = frame_for(&rt.paint_frames_at(t), ViewId::new(1));
    assert_eq!(
        painted.frame, target.frame,
        "pinned chrome must stay anchored during the workspace slide"
    );
    assert_eq!(painted.content, target.content);
}

#[test]
fn unrelated_focus_fade_does_not_tween_an_appearance_reflow() {
    // CTX-1335 F8: geometry interpolation needs a causal accepted transition;
    // an unrelated focus fade must not turn a decoration/container reflow
    // into a bounds tween.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "settled baseline");

    let t = Instant::now();
    assert!(rt.trigger_animation(AnimationKind::Focus, Some(ViewId::new(1)), t));
    rt.set_container(UiRect::new(0, 0, 90, 24));
    let after = t + Duration::from_millis(20);
    let _ = rt.tick_at(after);
    assert_eq!(
        rt.paint_frames_at(after),
        rt.present_frames(),
        "appearance reflow stays instant while an unrelated focus fades"
    );
}

#[test]
fn zero_duration_trigger_does_not_evict_a_live_bounds_tween() {
    // CTX-1335 F7: a refused ring trigger must not drop another surface's
    // geometry before discovering its own duration is zero.
    let mut rt = runtime_with(AnimationPolicy {
        duration_ms: [150, 120, 0, 200, 150, 120, 150],
        ..AnimationPolicy::default()
    });
    let start = Instant::now();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    assert!(
        rt.animation_progress(AnimationKind::Resize, Some(ViewId::new(1)), start)
            .is_some(),
        "split armed a resize bounds slot"
    );
    for i in 0..(MAX_CONCURRENT_ANIMATIONS - 2) {
        assert!(rt.trigger_animation(
            AnimationKind::Open,
            Some(ViewId::new(300 + i as u64)),
            start
        ));
    }
    // Use the same logical instant as the armed tween: taking a fresh
    // wall-clock sample here made the assertion fail when CI spent >120 ms
    // in the loop (Linux Wayland flake on 39795616503).
    let now = start;
    assert!(
        !rt.trigger_animation(AnimationKind::Focus, Some(ViewId::new(999)), now),
        "zero-duration focus never arms"
    );
    assert!(
        rt.animation_progress(AnimationKind::Resize, Some(ViewId::new(1)), now)
            .is_some(),
        "the live bounds tween survives a refused zero-duration trigger"
    );
}

// ── CTX-0967: move/resize/drag transitions ───────────────────────────────

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry for the overlay path (8px padding, 14px decoration
/// inset, 9x19 cells).
fn overlay_cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 14.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 14.0 + f64::from(row) * 19.0 + 9.0,
    }
}

/// Cursor pixels for the split-handle path (8px padding, 6px decoration
/// outer at scale 1, 9x19 cells, zero gaps): column 40 is the zero-gap
/// boundary line of a 40|40 split.
fn border_cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 6.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 6.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

fn named_key(named: NamedKey, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn overlay_tree() -> LayoutNode {
    LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 5)),
        UiRect::new(50, 10, 10, 5),
    )
}

#[test]
fn same_workspace_reposition_arms_move_then_idles() {
    // CTX-0967: reparenting the focused leaf inside its workspace arms the
    // move transition on the moved panel, presents extra frames while
    // active, and idles with zero wakeups after the 150 ms default.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(150);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "idle baseline");

    let t = Instant::now();
    let moved = rt
        .workspace_move_focused_to_position_at(1, t)
        .expect("reposition must succeed");
    assert_eq!(moved, ViewId::new(1));
    assert_eq!(
        rt.animation_progress(AnimationKind::Move, Some(moved), t),
        Some(0.0),
        "move must arm at the gesture time"
    );
    let mid = t + Duration::from_millis(75);
    let p = rt
        .animation_progress(AnimationKind::Move, Some(moved), mid)
        .expect("mid progress");
    assert!((0.0..1.0).contains(&p), "mid move progress {p}");
    assert!(rt.tick_at(mid).is_some(), "active move must present");
    let end = t + Duration::from_millis(150);
    assert!(rt.tick_at(end).is_some(), "final frame commits");
    assert!(!rt.animations_active(), "move must complete");
    assert!(rt.tick_at(end).is_none(), "idle after move completes");
    assert_eq!(rt.animation_deadline(), None, "no deadline when idle");
}

#[test]
fn cross_workspace_move_arms_move() {
    // CTX-0967: moving the focused leaf into another workspace arms the
    // move transition on the moved panel and expires bounded.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.workspace_new().expect("new workspace");
    let _ = rt.tick_at(start).expect("workspace switch presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "idle baseline");

    let t = Instant::now();
    let moved = rt
        .workspace_move_focused_to_at(0, t)
        .expect("cross-workspace move must succeed");
    assert!(
        rt.animation_progress(AnimationKind::Move, Some(moved), t)
            .is_some(),
        "move must arm on the moved panel"
    );
    let end = t + Duration::from_millis(150);
    rt.tick_at(end);
    assert!(
        rt.animation_progress(AnimationKind::Move, Some(moved), end)
            .is_none(),
        "move must expire by 150 ms"
    );
}

#[test]
fn border_drag_arms_whole_surface_resize() {
    // CTX-0967: a live divider move arms the whole-surface resize
    // transition (one gesture touches every adjacent panel), applies the
    // ratio live, and idles after the 120 ms default.
    let mut rt = runtime_with(AnimationPolicy::default());
    rt.set_layout(two_pane_split());
    rt.handle_cursor_moved(border_cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active(), "border press must grab");

    let t = Instant::now();
    assert!(
        rt.update_border_drag_at(border_cell_pixels(47, 12), t),
        "drag motion must apply"
    );
    let ratio = rt
        .layout()
        .split_ratio_at(&[])
        .expect("root must expose a ratio");
    assert!((ratio - 0.6).abs() < 1e-6, "ratio must move live");
    assert!(
        rt.animation_progress(AnimationKind::Resize, None, t)
            .is_some(),
        "resize must arm whole-surface at the gesture time"
    );
    let mid = t + Duration::from_millis(60);
    let p = rt
        .animation_progress(AnimationKind::Resize, None, mid)
        .expect("mid progress");
    assert!((0.0..1.0).contains(&p), "mid resize progress {p}");
    assert!(rt.tick_at(mid).is_some(), "active resize must present");
    let end = t + Duration::from_millis(120);
    assert!(rt.tick_at(end).is_some(), "final frame commits");
    assert!(!rt.animations_active(), "resize must complete");
    assert!(rt.tick_at(end).is_none(), "idle after resize completes");
    assert_eq!(rt.animation_deadline(), None, "no deadline when idle");
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn alt_drag_arms_drag_on_the_float() {
    // CTX-0967: moving a grabbed floating overlay arms the drag transition
    // on the dragged leaf; the bounds commit immediately and the chrome
    // settles after the 150 ms default.
    let mut rt = runtime_with(AnimationPolicy::default());
    rt.set_layout(overlay_tree());
    rt.handle_cursor_moved(overlay_cell_pixels(55, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.alt_drag_active(), "Alt+press on float must grab");

    let t = Instant::now();
    assert!(
        rt.update_alt_drag_at(overlay_cell_pixels(60, 14), t),
        "drag motion must apply"
    );
    let bounds = match rt.layout() {
        LayoutNode::Overlay { bounds, .. } => *bounds,
        other => panic!("expected overlay tree, got {other:?}"),
    };
    assert_eq!(bounds, UiRect::new(55, 12, 10, 5));
    let leaf = ViewId::new(2);
    assert_eq!(
        rt.animation_progress(AnimationKind::Drag, Some(leaf), t),
        Some(0.0),
        "drag must arm at the gesture time"
    );
    let mid = t + Duration::from_millis(75);
    let p = rt
        .animation_progress(AnimationKind::Drag, Some(leaf), mid)
        .expect("mid progress");
    assert!((0.0..1.0).contains(&p), "mid drag progress {p}");
    assert!(rt.tick_at(mid).is_some(), "active drag must present");
    let end = t + Duration::from_millis(150);
    assert!(rt.tick_at(end).is_some(), "final frame commits");
    assert!(!rt.animations_active(), "drag must complete");
    assert!(rt.tick_at(end).is_none(), "idle after drag completes");
    assert_eq!(rt.animation_deadline(), None, "no deadline when idle");
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn geometry_kinds_suppressed_when_instant() {
    // CTX-0967: disabled, reduced-motion-always, and safe-mode policies arm
    // no geometry transition, while the gestures themselves still apply.
    for policy in [
        AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        },
    ] {
        // Reparent still moves; it just does not animate.
        let mut rt = runtime_with(policy);
        let start = Instant::now();
        let _ = rt.tick_at(start);
        rt.set_layout(two_pane_split());
        let _ = rt.tick_at(start);
        let t = Instant::now();
        let moved = rt
            .workspace_move_focused_to_position_at(1, t)
            .expect("reposition must succeed");
        assert!(
            rt.animation_progress(AnimationKind::Move, Some(moved), t)
                .is_none(),
            "suppressed move must not arm: {policy:?}"
        );
        // Float drag still moves; it just does not animate.
        let mut rt = runtime_with(policy);
        rt.set_layout(overlay_tree());
        rt.handle_cursor_moved(overlay_cell_pixels(55, 12));
        rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
        rt.handle_mouse_input(press(MouseButton::Left));
        assert!(rt.alt_drag_active());
        let t = Instant::now();
        assert!(rt.update_alt_drag_at(overlay_cell_pixels(60, 14), t));
        assert!(
            rt.animation_progress(AnimationKind::Drag, Some(ViewId::new(2)), t)
                .is_none(),
            "suppressed drag must not arm: {policy:?}"
        );
        assert!(!rt.animations_active());
        assert_eq!(rt.animation_deadline(), None);
        rt.handle_mouse_input(release(MouseButton::Left));
        rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    }
}

#[test]
fn terminal_grid_is_never_interpolated_during_move() {
    // CTX-0967: a grid byte written mid-move reads back verbatim — only
    // Core-owned chrome fades, never terminal truth.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start);
    let t = Instant::now();
    rt.workspace_move_focused_to_position_at(1, t)
        .expect("reposition must succeed");
    assert!(rt.animations_active_at(t), "move must be active");
    rt.handle_pty_bytes(b"\x1b[2;1HMOVE-TRUTH");
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
    assert_eq!(row, "MOVE-TRUTH", "grid content is never animated");
}

// ── CTX-1078: splits/zoom/workspaces Bezier transitions ───────────────────

fn single_leaf() -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(1), 80, 24))
}

#[test]
fn zoom_engage_arms_close_and_disengage_arms_open() {
    // CTX-1078 (issue #1755): single-pane zoom rides the same chrome-ring
    // contract as splits. Engage collapses to one leaf (a close for the
    // hidden leaf); disengage restores the split (an open for the restored
    // leaf). Geometry commits immediately; only the ring fades, driven by
    // the virtual clock with no wall-clock sleeps.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("baseline presents");
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start).expect("split presents");
    let settled = start + Duration::from_millis(150);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "idle baseline");

    // Zoom engage: collapse to the focused leaf through the layout
    // path (the toggle_zoom lane). The tree commits at once.
    rt.set_layout(single_leaf());
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1)]);
    let _ = rt.tick_at(settled).expect("zoom engage presents");
    assert!(
        rt.animation_progress(AnimationKind::Close, Some(ViewId::new(2)), settled)
            .is_some(),
        "zoom engage must arm a close for the hidden leaf"
    );
    let mid = settled + Duration::from_millis(60);
    let p = rt
        .animation_progress(AnimationKind::Close, Some(ViewId::new(2)), mid)
        .expect("mid close progress");
    assert!((0.0..1.0).contains(&p), "mid zoom-close progress {p}");
    let close_end = settled + Duration::from_millis(120);
    rt.tick_at(close_end);
    assert!(
        rt.animation_progress(AnimationKind::Close, Some(ViewId::new(2)), close_end)
            .is_none(),
        "zoom close must expire by 120 ms"
    );

    // Zoom disengage: restore the split. The restored leaf opens.
    rt.set_layout(two_pane_split());
    assert_eq!(rt.layout().leaf_ids().len(), 2);
    let _ = rt.tick_at(close_end).expect("zoom release presents");
    assert!(
        rt.animation_progress(AnimationKind::Open, Some(ViewId::new(2)), close_end)
            .is_some(),
        "zoom release must arm an open for the restored leaf"
    );
    let open_end = close_end + Duration::from_millis(150);
    rt.tick_at(open_end);
    assert!(!rt.animations_active(), "zoom open must complete");
    assert!(rt.tick_at(open_end).is_none(), "idle after zoom completes");
}

#[test]
fn zoom_commits_grid_and_hit_geometry_immediately() {
    // CTX-1078 / CTX-1335: zoom commits the logical layout, PTY size, and
    // hit-test geometry at the mutation (not at animation end). Only the
    // paint bounds interpolate, and a grid byte written mid-zoom reads back
    // verbatim.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start);
    rt.set_layout(two_pane_split());
    let _ = rt.tick_at(start);
    rt.set_layout(single_leaf());
    assert_eq!(
        rt.layout().leaf_ids(),
        vec![ViewId::new(1)],
        "zoom geometry must commit immediately"
    );
    let _ = rt.tick_at(start).expect("zoom presents");
    // Query the arming on the same virtual clock `tick_at` uses. The
    // wall-clock `animations_active()` reads the close as already expired
    // when a loaded CI scheduler stalls this thread for more than the
    // 120 ms close duration between arming and the assertion (same
    // mechanism as the CTX-0408 focus cross-fade fix above).
    assert!(
        rt.animation_progress(AnimationKind::Close, Some(ViewId::new(2)), start)
            .is_some(),
        "zoom close must be active"
    );
    rt.handle_pty_bytes(b"\x1b[2;1HZOOM-TRUTH");
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
    assert_eq!(row, "ZOOM-TRUTH", "grid content is never animated by zoom");
}

#[test]
fn workspace_prev_next_last_arm_workspace_transition() {
    // CTX-1078: every workspace navigation path arms the same bounded
    // workspace transition with the virtual clock. Three workspaces give
    // prev/next/last distinct targets.
    let mut rt = runtime_with(AnimationPolicy::default());
    let start = Instant::now();
    let _ = rt.tick_at(start).expect("first frame");
    rt.workspace_new().expect("second workspace");
    let _ = rt.tick_at(start).expect("switch presents");
    let settled = start + Duration::from_millis(200);
    rt.tick_at(settled);
    assert!(rt.tick_at(settled).is_none(), "idle baseline");
    rt.workspace_new().expect("third workspace");
    let _ = rt.tick_at(settled).expect("switch presents");
    let base = settled + Duration::from_millis(200);
    rt.tick_at(base);
    assert!(rt.tick_at(base).is_none());
    assert_eq!(rt.workspace_count(), 3);

    // Prev wraps to the second workspace.
    rt.workspace_prev();
    let _ = rt.tick_at(base).expect("prev presents");
    assert_eq!(
        rt.animation_progress(AnimationKind::Workspace, None, base),
        Some(0.0),
        "prev must arm the workspace transition at the virtual time"
    );
    let end = base + Duration::from_millis(200);
    rt.tick_at(end);
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, end)
            .is_none(),
        "prev must expire by 200 ms"
    );

    // Next returns to the third workspace.
    rt.workspace_next();
    let _ = rt.tick_at(end).expect("next presents");
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, end)
            .is_some(),
        "next must arm the workspace transition"
    );
    let end2 = end + Duration::from_millis(200);
    rt.tick_at(end2);
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, end2)
            .is_none(),
        "next must expire by 200 ms"
    );

    // Last jumps to the most-recently-used workspace.
    rt.workspace_last();
    let _ = rt.tick_at(end2).expect("last presents");
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, end2)
            .is_some(),
        "last must arm the workspace transition"
    );
    let end3 = end2 + Duration::from_millis(200);
    rt.tick_at(end3);
    assert!(
        rt.animation_progress(AnimationKind::Workspace, None, end3)
            .is_none(),
        "last must expire by 200 ms"
    );
    assert!(rt.tick_at(end3).is_none(), "idle after nav completes");
}

#[test]
fn zoom_and_switch_suppressed_when_instant() {
    // CTX-1078: disabled, reduced-motion-always, safe-mode, and 0 ms
    // policies arm no zoom or workspace transition, while the state change
    // itself still commits (instant final geometry, never a degraded
    // intermediate). This is the headless no-arm gate: without a live
    // frame loop there is no interpolation, only the committed end state.
    for policy in [
        AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            duration_ms: [0; AnimationKind::COUNT],
            ..AnimationPolicy::default()
        },
    ] {
        // Zoom still collapses; it just does not animate.
        let mut rt = runtime_with(policy);
        let start = Instant::now();
        let _ = rt.tick_at(start);
        rt.set_layout(two_pane_split());
        let _ = rt.tick_at(start);
        rt.set_layout(single_leaf());
        assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1)]);
        assert!(rt.tick_at(start).is_some(), "zoom presents once");
        assert!(
            !rt.animations_active(),
            "suppressed zoom must not arm: {policy:?}"
        );
        assert!(
            rt.animation_progress(AnimationKind::Close, Some(ViewId::new(2)), start)
                .is_none(),
            "suppressed zoom close must not arm: {policy:?}"
        );
        assert!(rt.tick_at(start).is_none(), "suppressed zoom idles");

        // Workspace switch still moves; it just does not animate.
        let mut rt = runtime_with(policy);
        let _ = rt.tick_at(start);
        rt.workspace_new().expect("new workspace");
        assert!(rt.tick_at(start).is_some(), "switch presents once");
        assert!(
            !rt.animations_active(),
            "suppressed switch must not arm: {policy:?}"
        );
        assert!(
            rt.animation_progress(AnimationKind::Workspace, None, start)
                .is_none(),
            "suppressed workspace must not arm: {policy:?}"
        );
        assert_eq!(rt.animation_deadline(), None);
        assert!(rt.tick_at(start).is_none(), "suppressed switch idles");
    }
}

#[test]
fn split_zoom_switch_respect_concurrent_surface_budget() {
    // CTX-1078: the motion budget caps concurrently animating surfaces at
    // eight. Past capacity a split, zoom, or switch commits its end state
    // immediately (instant cut) instead of queueing unbounded work, and the
    // tracker idles with zero wakeups once the bounded set completes.
    let mut rt = runtime_with(AnimationPolicy::default());
    let now = Instant::now();
    // Fill the tracker directly through the same arm gate the gesture
    // paths use: eight distinct surfaces animate, the ninth cuts instantly.
    for i in 0..8 {
        assert!(rt.trigger_animation(AnimationKind::Open, Some(ViewId::new(i as u64 + 1)), now));
    }
    assert!(
        !rt.trigger_animation(AnimationKind::Workspace, None, now),
        "past capacity the workspace switch must cut instantly"
    );
    assert!(
        !rt.trigger_animation(AnimationKind::Open, Some(ViewId::new(999)), now),
        "past capacity a split must cut instantly"
    );
    let end = now + Duration::from_millis(200);
    assert!(
        rt.tick_at(end).is_some(),
        "bounded set still presents its final frame"
    );
    assert!(!rt.animations_active(), "bounded set must complete");
    assert_eq!(rt.animation_deadline(), None);
    // Capacity is available again after completion.
    assert!(rt.trigger_animation(AnimationKind::Workspace, None, end));
}
