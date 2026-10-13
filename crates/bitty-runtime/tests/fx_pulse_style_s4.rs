#![forbid(unsafe_code)]
//! Style-driven fx pulse envelopes (CTX-1163 S4, DEC-0111 section 4 as
//! corrected by DEC-0113).
//!
//! Proves headlessly that `bitty.fx.pulse` reads the RESOLVED per-`View`
//! style instead of hardcoded values: a styled leaf (glow color plus width)
//! pulses its style halo while a default leaf pulses one ring-derived
//! fallback halo (the leaf's committed ring color and width, never a
//! constant), so styled and default envelopes differ exactly by their style
//! contribution. Grant gating (`panel.focus` reuse, deny-by-default), the
//! shared 8-surface budget, the queue discipline, and the fixed
//! 150 ms `EaseInOut` timing are unchanged.
//!
//! Envelope style (per the frame-capture harness precedent): virtual-clock
//! ticks into the deterministic headless surface, geometry keys and grid
//! digests pin the content-metrics invariant, raw RGBA probes pin paint
//! presence, and `rounded_fills` deltas pin the halo primitive count.
//!
//! Deterministic on all platforms: virtual clock only, no sleeps, no PTY
//! spawn, no window. Grid bytes enter through `handle_pty_bytes` exactly
//! like the existing grid-truth tests.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bitty_runtime::plugin_runtime::{
    EmptySettings, NotificationQueue, PluginServices, PluginStore, UnavailableSnapshot,
};
use bitty_runtime::{
    AnimationCurve, AnimationKind, AnimationPolicy, FX_PULSE_DURATION_MS, FxPulseQueue,
    MAX_CONCURRENT_ANIMATIONS, PresentStats, ReducedMotionMode, Runtime, RuntimeConfig, UiRect,
    View, ViewId,
};
use bitty_runtime::{LayoutNode, PresentFrame};

/// Comparable geometry key for one presented leaf: any style-driven layout
/// motion changes at least one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GeomKey {
    view: u64,
    frame_x: i32,
    frame_y: i32,
    frame_w: u32,
    frame_h: u32,
    content_x: i32,
    content_y: i32,
    content_w: u32,
    content_h: u32,
    cols: u16,
    rows: u16,
    border: u16,
    radius: u16,
}

fn geom_key(frame: &PresentFrame) -> GeomKey {
    GeomKey {
        view: frame.view.0,
        frame_x: frame.frame.x,
        frame_y: frame.frame.y,
        frame_w: frame.frame.width,
        frame_h: frame.frame.height,
        content_x: frame.content.x,
        content_y: frame.content.y,
        content_w: frame.content.width,
        content_h: frame.content.height,
        cols: frame.cols,
        rows: frame.rows,
        border: frame.border,
        radius: frame.radius,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn grid_digest(rt: &Runtime) -> u64 {
    let snap = rt.snapshot();
    let mut bytes = Vec::with_capacity(snap.cells.len() * 2);
    for cell in snap.cells.iter() {
        let mut buf = [0u8; 4];
        bytes.extend_from_slice(cell.glyph.encode_utf8(&mut buf).as_bytes());
    }
    fnv1a(&bytes)
}

#[derive(Debug, Clone)]
struct CapturedFrame {
    presented: bool,
    progress: Option<f32>,
    rgba: Option<Vec<u8>>,
    geometry: Vec<GeomKey>,
    grid: u64,
    stats: Option<PresentStats>,
}

struct OffscreenHarness {
    rt: Runtime,
    start: Instant,
}

impl OffscreenHarness {
    fn new(cfg: RuntimeConfig, start: Instant) -> Self {
        let mut rt = Runtime::with_deterministic_rasterizer(cfg)
            .expect("headless harness runtime must build");
        assert!(
            !rt.is_crossfont(),
            "harness requires the deterministic headless rasterizer"
        );
        rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
        rt.set_container(UiRect::new(0, 0, 80, 24));
        Self { rt, start }
    }

    fn plain(start: Instant) -> Self {
        Self::new(RuntimeConfig::default(), start)
    }

    fn styled(start: Instant) -> Self {
        Self::new(
            RuntimeConfig {
                style_glow_color: Some([0xFF, 0x00, 0x00, 0xFF]),
                style_glow_width: 8,
                ..RuntimeConfig::default()
            },
            start,
        )
    }

    fn capture(&mut self, offset_ms: u64, surface: ViewId) -> CapturedFrame {
        let at = self.start + Duration::from_millis(offset_ms);
        let stats = self.rt.tick_at(at);
        let progress = self.rt.fx_progress(surface, at);
        let rgba = if stats.is_some() {
            self.rt.headless_rgba()
        } else {
            None
        };
        CapturedFrame {
            presented: stats.is_some(),
            progress,
            rgba,
            geometry: self.rt.present_frames().iter().map(geom_key).collect(),
            grid: grid_digest(&self.rt),
            stats,
        }
    }
}

fn rgba_at(rgba: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * stride + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

/// Pixel probe one logical px outside the left frame edge at the vertical
/// middle: inside the 8 px styled halo and inside the ring-derived fallback
/// halo, far from the rounded corners and covered by no glyph.
fn halo_probe(rt: &Runtime, view: ViewId) -> (usize, usize, usize) {
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let stride = usize::try_from(rt.config().window_extent().width()).expect("stride fits");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|f| f.view == view)
        .expect("probed view must have a frame");
    let x = (pad + usize::try_from(frame.frame.x).expect("frame x fits")).saturating_sub(1);
    let y = pad
        + usize::try_from(frame.frame.y).expect("frame y fits")
        + usize::try_from(frame.frame.height / 2).expect("half height fits");
    (x, y, stride)
}

/// A mid-pulse halo pixel must lie strictly between the gap background and
/// the committed halo color on every channel (the S3 fade-between posture).
fn assert_fade_between(mid_px: [u8; 4], final_px: [u8; 4]) {
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
    assert_ne!(mid_px, final_px, "mid halo must differ from the final halo");
}

#[test]
fn fx_pulse_halo_reads_resolved_style() {
    assert_eq!(
        u64::from(FX_PULSE_DURATION_MS),
        150,
        "default pulse duration pins the sample grid"
    );
    let duration_ms = u64::from(FX_PULSE_DURATION_MS);
    let start = Instant::now();
    let mut styled = OffscreenHarness::styled(start);
    let mut plain = OffscreenHarness::plain(start);
    let view = ViewId::new(1);

    styled.rt.handle_pty_bytes(b"S4-STYLE-TRUTH");
    plain.rt.handle_pty_bytes(b"S4-STYLE-TRUTH");
    let styled_base = styled.capture(0, view);
    let plain_base = plain.capture(0, view);
    assert!(
        styled_base.presented && plain_base.presented,
        "baseline frames must present the grid"
    );
    // Style is paint-only: geometry and grid truth agree across harnesses.
    assert_eq!(styled_base.geometry, plain_base.geometry);
    assert_eq!(styled_base.grid, plain_base.grid);

    // Baselines: the styled leaf carries its opaque glow halo, the default
    // leaf carries none (S4 fallback paints only under a live pulse).
    let (hx, hy, stride) = halo_probe(&styled.rt, view);
    let bg = bitty_render::grid::DEFAULT_BG;
    let styled_rgba = styled_base.rgba.as_deref().expect("styled bytes");
    let plain_rgba = plain_base.rgba.as_deref().expect("plain bytes");
    assert_eq!(
        rgba_at(styled_rgba, stride, hx, hy),
        [0xFF, 0x00, 0x00, 0xFF],
        "styled baseline stamps the resolved glow color"
    );
    assert_eq!(
        rgba_at(plain_rgba, stride, hx, hy),
        bg,
        "default baseline keeps the gap background (no halo without a pulse)"
    );

    // A granted plugin intent arms the Core-owned pulse on the virtual clock.
    assert!(
        styled.rt.trigger_fx_pulse(view, start),
        "styled pulse must arm on a live leaf"
    );
    assert!(
        plain.rt.trigger_fx_pulse(view, start),
        "default pulse must arm on a live leaf"
    );
    let smid = styled.capture(duration_ms / 2, view);
    let pmid = plain.capture(duration_ms / 2, view);
    assert!(
        smid.presented && pmid.presented,
        "active pulses must keep presenting"
    );
    // Timing is unchanged: both follow the closed EaseInOut curve.
    for (label, frame) in [("styled", &smid), ("plain", &pmid)] {
        let value = frame.progress.expect("mid pulse must report");
        let expected = AnimationCurve::EaseInOut.eval(0.5);
        assert!(
            (value - expected).abs() < 1e-4,
            "{label} mid progress must follow EaseInOut ({expected}), got {value}"
        );
    }
    // The S4 fallback adds exactly one halo primitive on the default leaf;
    // the styled leaf keeps its single style halo (modulated, not doubled).
    let plain_base_n = plain_base.stats.map(|s| s.rounded_fills).unwrap_or(0);
    let plain_mid_n = pmid.stats.map(|s| s.rounded_fills).unwrap_or(0);
    assert_eq!(
        plain_mid_n,
        plain_base_n + 1,
        "default pulse must add exactly one ring-derived halo"
    );
    assert_eq!(
        smid.stats.map(|s| s.rounded_fills),
        styled_base.stats.map(|s| s.rounded_fills),
        "styled pulse must reuse its single style halo"
    );
    // Envelopes differ exactly by the style contribution: the styled halo
    // fades its resolved red, the default halo fades its resolved ring
    // color, and neither skips to its end state mid-pulse.
    let styled_mid = rgba_at(smid.rgba.as_deref().expect("mid bytes"), stride, hx, hy);
    let plain_mid = rgba_at(pmid.rgba.as_deref().expect("mid bytes"), stride, hx, hy);
    assert_fade_between(styled_mid, [0xFF, 0x00, 0x00, 0xFF]);
    assert_ne!(
        plain_mid, bg,
        "default mid pulse must paint its fallback halo"
    );
    assert_ne!(
        styled_mid, plain_mid,
        "styled vs default envelopes must differ by the style contribution"
    );
    // Geometry and grid truth never interpolate mid-pulse.
    assert_eq!(smid.geometry, styled_base.geometry);
    assert_eq!(pmid.geometry, plain_base.geometry);
    assert_eq!(smid.grid, styled_base.grid);
    assert_eq!(pmid.grid, plain_base.grid);

    // Completion commits the styled end states: the styled halo returns to
    // opaque red, the default halo is gone, and the path idles.
    let end_at = start + Duration::from_millis(duration_ms);
    let styled_end_stats = styled.rt.tick_at(end_at);
    let plain_end_stats = plain.rt.tick_at(end_at);
    assert!(
        styled_end_stats.is_some() && plain_end_stats.is_some(),
        "final frames must commit the end state"
    );
    assert_eq!(styled.rt.fx_progress(view, end_at), None);
    assert_eq!(plain.rt.fx_progress(view, end_at), None);
    assert_eq!(
        rgba_at(
            styled.rt.headless_rgba().expect("end bytes").as_slice(),
            stride,
            hx,
            hy
        ),
        [0xFF, 0x00, 0x00, 0xFF],
        "styled end state must restore the resolved glow color"
    );
    assert_eq!(
        rgba_at(
            plain.rt.headless_rgba().expect("end bytes").as_slice(),
            stride,
            hx,
            hy
        ),
        bg,
        "default end state must drop the fallback halo"
    );
    assert!(
        styled.rt.tick_at(end_at).is_none(),
        "idle after styled pulse"
    );
    assert!(
        plain.rt.tick_at(end_at).is_none(),
        "idle after default pulse"
    );
    assert_eq!(styled.rt.animation_deadline(), None);
    assert_eq!(plain.rt.animation_deadline(), None);
}

#[test]
fn fx_pulse_unknown_target_paints_nothing() {
    // The drain revalidates each intent against the live layout (the
    // application drops unknown ids with a warning and state untouched);
    // at the present layer an armed-but-unknown pulse paints no leaf.
    let start = Instant::now();
    let mut harness = OffscreenHarness::plain(start);
    let live = ViewId::new(1);
    let unknown = ViewId::new(404);
    harness.rt.handle_pty_bytes(b"S4-UNKNOWN-TRUTH");
    let baseline = harness.capture(0, live);
    assert!(baseline.presented, "baseline must present");
    assert!(
        harness.rt.trigger_fx_pulse(unknown, start),
        "tracker arms without layout validation (drain validates)"
    );
    let at = start + Duration::from_millis(25);
    let after = harness.capture(25, live);
    assert!(after.presented, "armed tracker still presents");
    assert_eq!(
        after.geometry, baseline.geometry,
        "unknown pulse must not move geometry"
    );
    assert_eq!(after.grid, baseline.grid, "grid truth must not move");
    assert_eq!(
        after.rgba, baseline.rgba,
        "live leaf pixels must not change for an unknown pulse"
    );
    assert_eq!(
        harness.rt.fx_progress(live, at),
        None,
        "live leaf must report no pulse"
    );
    assert_eq!(
        harness
            .rt
            .present_frames()
            .iter()
            .filter(|f| f.view == unknown)
            .count(),
        0,
        "unknown target must never own a frame"
    );
}

#[test]
fn fx_pulse_grant_gate_denies_without_focus() {
    // CTX-1139 gate, unchanged by S4: the verb reuses `panel.focus` and
    // denies by default, fails closed without a shared queue, and queues
    // (not applies) when granted.
    use bitty_lua::HostServices as _;

    fn services() -> PluginServices {
        PluginServices::new(
            "s4.test",
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(UnavailableSnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        )
    }

    let denied = services();
    let error = denied.fx_pulse(1).expect_err("denied");
    assert_eq!(error.code, "E_CAPABILITY_DENIED");
    assert_eq!(error.message, "capability 'panel.focus' is not granted");
    let create_only = services();
    create_only.set_panel_access(true, false);
    let error = create_only.fx_pulse(1).expect_err("focus denied");
    assert_eq!(error.code, "E_CAPABILITY_DENIED");

    let granted = services();
    granted.set_panel_access(false, true);
    let error = granted.fx_pulse(1).expect_err("unimplemented");
    assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    granted.set_fx_queue(Some(Rc::new(RefCell::new(FxPulseQueue::new(2)))));
    assert!(granted.fx_pulse(1).expect("first intent queues"));
    assert!(
        granted.fx_pulse(404).expect("unknown id still queues"),
        "unknown ids queue; the drain drops them against live layout"
    );
}

#[test]
fn fx_pulse_budget_unchanged_with_style() {
    // The single shared 8-surface cap spans core transitions and pulses
    // together under a styled config exactly as under the default one.
    assert_eq!(
        MAX_CONCURRENT_ANIMATIONS, 8,
        "harness assumes the accepted 8-surface budget"
    );
    let start = Instant::now();
    let mut harness = OffscreenHarness::styled(start);
    let _ = harness.rt.tick_at(start);
    for i in 0..MAX_CONCURRENT_ANIMATIONS {
        assert!(
            harness.rt.trigger_animation(
                AnimationKind::Open,
                Some(ViewId::new(i as u64 + 1)),
                start
            ),
            "first eight surfaces must arm"
        );
    }
    assert!(
        !harness.rt.trigger_fx_pulse(ViewId::new(999), start),
        "styled pulse past a full core set must cut instantly"
    );
    assert_eq!(harness.rt.fx_progress(ViewId::new(999), start), None);

    let start2 = Instant::now();
    let mut second = OffscreenHarness::styled(start2);
    let _ = second.rt.tick_at(start2);
    for i in 0..7 {
        assert!(second.rt.trigger_animation(
            AnimationKind::Open,
            Some(ViewId::new(i as u64 + 1)),
            start2
        ));
    }
    assert!(
        second.rt.trigger_fx_pulse(ViewId::new(100), start2),
        "one styled pulse fits beside seven core transitions"
    );
    assert!(
        !second
            .rt
            .trigger_animation(AnimationKind::Open, Some(ViewId::new(200)), start2),
        "core past the shared set must cut instantly"
    );
    let end = start2 + Duration::from_millis(200);
    assert!(second.rt.tick_at(end).is_some());
    assert!(second.rt.tick_at(end).is_none(), "idle after shared set");
    assert_eq!(second.rt.animation_deadline(), None);
    assert!(
        second.rt.trigger_fx_pulse(ViewId::new(100), end),
        "capacity returns after completion"
    );
}

#[test]
fn fx_pulse_instant_policies_never_arm_with_style() {
    // Safe mode, disabled animations, and reduced motion collapse every fx
    // verb to no-op even when a style halo is configured.
    let policies = [
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
            platform_reduced: true,
            ..AnimationPolicy::default()
        },
    ];
    for policy in policies {
        let start = Instant::now();
        let mut harness = OffscreenHarness::new(
            RuntimeConfig {
                style_glow_color: Some([0xFF, 0x00, 0x00, 0xFF]),
                style_glow_width: 8,
                animations: policy,
                ..RuntimeConfig::default()
            },
            start,
        );
        let view = ViewId::new(1);
        let _ = harness.rt.tick_at(start);
        assert!(
            !harness.rt.trigger_fx_pulse(view, start),
            "instant styled pulse must not arm: {policy:?}"
        );
        assert!(
            harness.rt.fx_progress(view, start).is_none(),
            "instant styled pulse must never report progress: {policy:?}"
        );
        assert!(
            harness.rt.tick_at(start).is_none(),
            "instant styled pulse idles immediately: {policy:?}"
        );
        assert_eq!(harness.rt.animation_deadline(), None);
    }
}
