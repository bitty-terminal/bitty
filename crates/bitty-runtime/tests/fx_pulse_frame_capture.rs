#![forbid(unsafe_code)]
//! Offscreen frame-capture for the plugin-requested ring pulse (CTX-1139).
//!
//! Same evidence style as the CTX-1138 transition harness: virtual-clock
//! ticks into the deterministic headless surface, one record per tick
//! (presented flag, eased progress, geometry key, grid digest), and envelope
//! checks — the pulse follows the closed `EaseInOut` curve monotonically,
//! geometry and grid stay static mid-pulse, the shared 8-surface budget cuts
//! instantly in both directions, and instant policies never arm.
//!
//! Deterministic on all platforms: virtual clock only, no sleeps, no PTY
//! spawn, no window. Grid bytes enter through `handle_pty_bytes` exactly
//! like the existing grid-truth tests.

use std::time::{Duration, Instant};

use bitty_runtime::{
    AnimationCurve, AnimationKind, AnimationPolicy, FX_PULSE_DURATION_MS,
    MAX_CONCURRENT_ANIMATIONS, PresentStats, ReducedMotionMode, Runtime, RuntimeConfig, UiRect,
    View, ViewId,
};
use bitty_runtime::{LayoutNode, PresentFrame};

/// Comparable geometry key for one presented leaf: any interpolation of
/// geometry mid-pulse changes at least one field.
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
    offset_ms: u64,
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
    fn new(policy: AnimationPolicy, start: Instant) -> Self {
        let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig {
            animations: policy,
            ..RuntimeConfig::default()
        })
        .expect("headless harness runtime must build");
        assert!(
            !rt.is_crossfont(),
            "harness requires the deterministic headless rasterizer"
        );
        rt.set_container(UiRect::new(0, 0, 80, 24));
        Self { rt, start }
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
            offset_ms,
            presented: stats.is_some(),
            progress,
            rgba,
            geometry: self.rt.present_frames().iter().map(geom_key).collect(),
            grid: grid_digest(&self.rt),
            stats,
        }
    }

    fn drive(&mut self, surface: ViewId, offsets_ms: &[u64]) -> Vec<CapturedFrame> {
        offsets_ms
            .iter()
            .map(|&offset| self.capture(offset, surface))
            .collect()
    }
}

fn rgba_at(rgba: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * stride + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn check_progress_envelope(samples: &[(u64, Option<f32>)], duration_ms: u64) -> Result<(), String> {
    if samples.is_empty() {
        return Err("envelope needs at least one sample".to_string());
    }
    let (first_offset, first_progress) = samples[0];
    if first_offset != 0 {
        return Err(format!(
            "first sample must sit at the pulse start, got {first_offset} ms"
        ));
    }
    let first = first_progress.ok_or_else(|| "pulse must arm at the start".to_string())?;
    if first.abs() > 1e-4 {
        return Err(format!("pulse must start at 0, got {first}"));
    }
    let mut prev = first;
    let mut advanced = false;
    for &(offset, progress) in &samples[1..] {
        let Some(value) = progress else {
            if offset < duration_ms {
                return Err(format!("pulse dropped early at {offset} ms"));
            }
            continue;
        };
        if offset >= duration_ms {
            return Err(format!(
                "pulse still reports {value} at {offset} ms past its {duration_ms} ms duration"
            ));
        }
        let raw = (offset as f32 / duration_ms as f32).clamp(0.0, 1.0);
        let expected = AnimationCurve::EaseInOut.eval(raw);
        if (value - expected).abs() > 1e-4 {
            return Err(format!(
                "progress at {offset} ms must follow EaseInOut ({expected}), got {value}"
            ));
        }
        if value < prev - 1e-6 {
            return Err(format!(
                "progress must never run backwards: {prev} then {value} at {offset} ms"
            ));
        }
        if value > prev + 1e-6 {
            advanced = true;
        }
        prev = value;
    }
    if !advanced {
        return Err("pulse never advanced: a flat envelope is an instant cut".to_string());
    }
    let (last_offset, last_progress) = samples[samples.len() - 1];
    if last_offset >= duration_ms && last_progress.is_some() {
        return Err(format!(
            "pulse must complete by {duration_ms} ms, still active at {last_offset} ms"
        ));
    }
    Ok(())
}

fn check_geometry_static(frames: &[CapturedFrame]) -> Result<(), String> {
    let Some(first) = frames.first() else {
        return Err("geometry check needs at least one frame".to_string());
    };
    if first.geometry.is_empty() {
        return Err("geometry check needs at least one presented leaf".to_string());
    }
    for frame in &frames[1..] {
        if frame.geometry != first.geometry {
            return Err(format!(
                "geometry must stay static mid-pulse: start {:?} vs {} ms {:?}",
                first.geometry, frame.offset_ms, frame.geometry
            ));
        }
    }
    Ok(())
}

fn check_grid_static(frames: &[CapturedFrame]) -> Result<(), String> {
    let Some(first) = frames.first() else {
        return Err("grid check needs at least one frame".to_string());
    };
    for frame in &frames[1..] {
        if frame.grid != first.grid {
            return Err(format!(
                "grid must stay static mid-pulse: digest changed by {} ms",
                frame.offset_ms
            ));
        }
    }
    Ok(())
}

fn single_leaf() -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(1), 80, 24))
}

fn ring_probe(rt: &Runtime, view: ViewId) -> (usize, usize, usize) {
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let stride = usize::try_from(rt.config().window_extent().width()).expect("stride fits");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|f| f.view == view)
        .expect("probed view must have a frame");
    let x = pad + usize::try_from(frame.frame.x).expect("frame x fits");
    let y = pad + usize::try_from(frame.frame.y).expect("frame y fits") + 50;
    (x, y, stride)
}

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
    assert_ne!(mid_px, final_px, "mid ring must differ from the final ring");
}

#[test]
fn fx_pulse_envelope_fades_ring_with_static_geometry_and_grid() {
    assert_eq!(
        u64::from(FX_PULSE_DURATION_MS),
        150,
        "default pulse duration pins the sample grid"
    );
    let duration_ms = u64::from(FX_PULSE_DURATION_MS);
    let start = Instant::now();
    let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
    let view = ViewId::new(1);

    harness.rt.handle_pty_bytes(b"PULSE-TRUTH");
    harness.rt.set_layout(single_leaf());
    let baseline = harness.capture(0, view);
    assert!(baseline.presented, "baseline frame must present the grid");

    // A granted plugin intent arms the Core-owned pulse on the virtual clock.
    assert!(
        harness.rt.trigger_fx_pulse(view, start),
        "pulse must arm on a live leaf"
    );
    let series = harness.drive(view, &[0, 25, 50, 75, 100, 125]);
    for frame in &series {
        assert!(frame.presented, "active pulse must keep presenting");
        let rounded = frame.stats.map(|s| s.rounded_fills).unwrap_or(0);
        assert!(
            rounded >= 1,
            "every pulse frame must paint Core-owned chrome at {} ms",
            frame.offset_ms
        );
    }
    let end_at = start + Duration::from_millis(duration_ms);
    let end_stats = harness.rt.tick_at(end_at);
    assert!(end_stats.is_some(), "final frame must commit the end state");
    assert_eq!(
        harness.rt.fx_progress(view, end_at),
        None,
        "pulse must complete by its duration"
    );
    let mut samples: Vec<(u64, Option<f32>)> =
        series.iter().map(|f| (f.offset_ms, f.progress)).collect();
    samples.push((duration_ms, None));
    check_progress_envelope(&samples, duration_ms).expect("pulse must follow its easing envelope");
    check_geometry_static(&series).expect("geometry must commit immediately and never move");
    let mut with_end = series.clone();
    with_end.push(CapturedFrame {
        offset_ms: duration_ms,
        presented: true,
        progress: None,
        rgba: harness.rt.headless_rgba(),
        geometry: harness.rt.present_frames().iter().map(geom_key).collect(),
        grid: grid_digest(&harness.rt),
        stats: end_stats,
    });
    check_grid_static(&with_end).expect("grid must never interpolate during a pulse");
    let snap = harness.rt.snapshot();
    let row: String = snap
        .cells
        .chunks(snap.width)
        .next()
        .expect("row 1")
        .iter()
        .take(11)
        .map(|c| c.glyph)
        .collect();
    assert_eq!(row, "PULSE-TRUTH", "grid content survives verbatim");

    // Pixel evidence: the ring fades in from transparent, so a mid-pulse
    // ring pixel sits strictly between the background and the committed
    // color. Both bytes come from the single forward drive above.
    let (ring_x, ring_y, stride) = ring_probe(&harness.rt, view);
    let mid_rgba = series[3].rgba.as_deref().expect("mid frame carries bytes");
    let mid_px = rgba_at(mid_rgba, stride, ring_x, ring_y);
    let final_rgba = with_end
        .last()
        .expect("end frame")
        .rgba
        .as_deref()
        .expect("end frame carries bytes");
    let final_px = rgba_at(final_rgba, stride, ring_x, ring_y);
    assert_fade_between(mid_px, final_px);

    // Frame-on-demand: after the end state commits, the path idles with no
    // deadline (zero periodic wakeups attributable to the pulse).
    assert!(harness.rt.tick_at(end_at).is_none(), "idle after pulse");
    assert_eq!(harness.rt.animation_deadline(), None);
}

#[test]
fn fx_pulse_shares_the_eight_surface_budget_both_ways() {
    assert_eq!(
        MAX_CONCURRENT_ANIMATIONS, 8,
        "harness assumes the accepted 8-surface budget"
    );
    let start = Instant::now();
    let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
    let _ = harness.rt.tick_at(start);
    // Eight live core transitions refuse the next pulse (end state holds).
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
        "pulse past a full core set must cut instantly"
    );
    assert_eq!(
        harness.rt.fx_progress(ViewId::new(999), start),
        None,
        "refused pulse must never report progress"
    );

    // The reverse: seven core transitions leave room for exactly one pulse,
    // and that full shared set refuses the next core trigger.
    let start2 = Instant::now();
    let mut second = OffscreenHarness::new(AnimationPolicy::default(), start2);
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
        "one pulse fits beside seven core transitions"
    );
    assert!(
        !second
            .rt
            .trigger_animation(AnimationKind::Open, Some(ViewId::new(200)), start2),
        "core past the shared set must cut instantly"
    );
    // Capacity returns after completion on both sides.
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
fn fx_pulse_instant_policies_never_arm() {
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
        let mut harness = OffscreenHarness::new(policy, start);
        let view = ViewId::new(1);
        harness.rt.set_layout(single_leaf());
        let _ = harness.rt.tick_at(start);
        assert!(
            !harness.rt.trigger_fx_pulse(view, start),
            "instant pulse must not arm: {policy:?}"
        );
        assert!(
            harness.rt.fx_progress(view, start).is_none(),
            "instant pulse must never report progress: {policy:?}"
        );
        assert!(
            harness.rt.tick_at(start).is_none(),
            "instant pulse idles immediately: {policy:?}"
        );
        assert_eq!(harness.rt.animation_deadline(), None);
    }
}
