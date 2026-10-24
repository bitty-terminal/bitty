#![forbid(unsafe_code)]
//! Offscreen frame-capture harness for RFC-0002 panel transitions (CTX-1138).
//!
//! Pixel evidence without headed timing flakes: the harness drives present on
//! virtual-clock ticks into the deterministic headless surface
//! (`Runtime::with_deterministic_rasterizer` + `tick_at` + `headless_rgba`),
//! captures one record per tick (presented flag, eased progress, FNV-1a frame
//! hash, geometry key, grid digest), and checks transition envelopes:
//! fade alpha follows the configured easing curve monotonically, geometry and
//! grid stay static mid-transition, the 8-concurrent cap cuts instantly, and
//! reduced-motion/disabled policies stay instant.
//!
//! Hashes are evidence, not the assertion: every captured frame records its
//! FNV-1a digest (stable across toolchains, same rationale as
//! `present_golden`) plus the raw RGBA bytes for pixel probes, all from a
//! single forward virtual-clock drive (never re-ticked, never backwards in
//! time). The assertions themselves are envelope checks, deliberately not pinned golden hashes. A golden hash
//! churns on every present-path change (see the re-record history in
//! `present_golden`), while the envelope pins the RFC-0002 contract that must
//! survive refactors: monotone easing-matched fade, static geometry and grid,
//! bounded completion with frame-on-demand idle after it.
//!
//! Coverage: split-open, zoom engage/disengage, workspace switch, the
//! concurrent-surface budget, and the instant policies. A deliberately broken
//! envelope (interpolated geometry, non-monotone progress, curve mismatch, grid
//! motion) is fed to the same checkers to prove they reject it.
//!
//! Deterministic on all platforms: virtual clock only, no sleeps, no PTY
//! spawn, no window. Grid bytes enter through `handle_pty_bytes` (byte
//! injection, not a PTY) exactly like the existing grid-truth tests.

use std::time::{Duration, Instant};

use bitty_runtime::{
    AnimationCurve, AnimationKind, AnimationPolicy, LayoutNode, MAX_CONCURRENT_ANIMATIONS,
    PresentStats, ReducedMotionMode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

/// FNV-1a 64-bit over the bytes; fixed constants, stable across toolchains
/// (same rationale as the `present_golden` digests).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Comparable geometry key for one presented leaf: the full decorated frame
/// the present path composites. Any interpolation of geometry mid-transition
/// changes at least one field, so equality across the captured frames proves
/// the layout committed immediately and never moved while chrome faded.
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

fn geom_key(frame: &bitty_runtime::PresentFrame) -> GeomKey {
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

/// Digest of the primary grid glyphs: Terminal Truth must never move while
/// chrome fades.
fn grid_digest(rt: &Runtime) -> u64 {
    let snap = rt.snapshot();
    let mut bytes = Vec::with_capacity(snap.cells.len() * 2);
    for cell in snap.cells.iter() {
        let mut buf = [0u8; 4];
        bytes.extend_from_slice(cell.glyph.encode_utf8(&mut buf).as_bytes());
    }
    fnv1a(&bytes)
}

/// One offscreen frame captured on a virtual-clock tick.
#[derive(Debug, Clone)]
struct CapturedFrame {
    /// Milliseconds after the transition start.
    offset_ms: u64,
    /// Whether the tick presented (frame-on-demand: idle ticks present
    /// nothing).
    presented: bool,
    /// Eased progress reported for the transition at this tick (`None` once
    /// complete, when callers must use the final committed state).
    progress: Option<f32>,
    /// FNV-1a digest of the presented RGBA (`None` when idle).
    hash: Option<u64>,
    /// Raw presented RGBA (`None` when idle). Hashes are the routine
    /// evidence; raw bytes stay available for pixel probes without
    /// re-ticking (notably never backwards in virtual time).
    rgba: Option<Vec<u8>>,
    /// Presented-leaf geometry at this tick.
    geometry: Vec<GeomKey>,
    /// Grid digest at this tick.
    grid: u64,
    /// Present counters for the frame, when one presented.
    stats: Option<PresentStats>,
}

/// Offscreen harness: owns a deterministic headless runtime and the captured
/// frame log. The transition start is the virtual instant the test passes as
/// `start`; every sample is an explicit offset from it.
struct OffscreenHarness {
    rt: Runtime,
    start: Instant,
    frames: Vec<CapturedFrame>,
}

impl OffscreenHarness {
    /// Builds the harness on the deterministic headless rasterizer: bitmaps
    /// derive from the scalar and point size alone, so pixels are
    /// bit-identical on every platform. The container is fixed so ring-probe
    /// offsets are stable.
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
        Self {
            rt,
            start,
            frames: Vec::new(),
        }
    }

    /// Ticks at `start + offset` and captures the frame for `kind`/`surface`.
    fn capture(
        &mut self,
        offset_ms: u64,
        kind: AnimationKind,
        surface: Option<ViewId>,
    ) -> CapturedFrame {
        let at = self.start + Duration::from_millis(offset_ms);
        let stats = self.rt.tick_at(at);
        let progress = self.rt.animation_progress(kind, surface, at);
        let rgba = if stats.is_some() {
            self.rt.headless_rgba()
        } else {
            None
        };
        let frame = CapturedFrame {
            offset_ms,
            presented: stats.is_some(),
            progress,
            hash: rgba.as_deref().map(fnv1a),
            rgba,
            geometry: self.rt.present_frames().iter().map(geom_key).collect(),
            grid: grid_digest(&self.rt),
            stats,
        };
        self.frames.push(frame.clone());
        frame
    }

    /// Captures one frame per offset in order; returns the series.
    fn drive(
        &mut self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        offsets_ms: &[u64],
    ) -> Vec<CapturedFrame> {
        offsets_ms
            .iter()
            .map(|&offset| self.capture(offset, kind, surface))
            .collect()
    }

    /// Ticks exactly at the transition end: commits the final frame, then
    /// reports completion. Returns the captured end frame so the caller can
    /// close the envelope sample series and probe final pixels.
    fn tick_end(
        &mut self,
        duration_ms: u64,
        kind: AnimationKind,
        surface: Option<ViewId>,
    ) -> CapturedFrame {
        let at = self.start + Duration::from_millis(duration_ms);
        let stats = self.rt.tick_at(at);
        assert!(
            stats.is_some(),
            "final frame must commit the end state at {duration_ms} ms"
        );
        let progress = self.rt.animation_progress(kind, surface, at);
        assert_eq!(
            progress, None,
            "transition must complete by {duration_ms} ms"
        );
        let rgba = self.rt.headless_rgba();
        let frame = CapturedFrame {
            offset_ms: duration_ms,
            presented: true,
            progress,
            hash: rgba.as_deref().map(fnv1a),
            rgba,
            geometry: self.rt.present_frames().iter().map(geom_key).collect(),
            grid: grid_digest(&self.rt),
            stats,
        };
        self.frames.push(frame.clone());
        frame
    }

    /// Hashes of the presented frames in capture order (idle ticks excluded).
    fn presented_hashes(&self) -> Vec<u64> {
        self.frames.iter().filter_map(|f| f.hash).collect()
    }
}

/// Pixel at `(x, y)` in a surface `stride` pixels wide.
fn rgba_at(rgba: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * stride + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

/// Checks the eased progress envelope of one transition sampled at virtual
/// offsets: starts exactly at zero, tracks the configured curve within float
/// noise, never runs backwards, moves strictly at least once (a cut would
/// stay flat), and reports completion exactly at the duration end.
fn check_progress_envelope(
    samples: &[(u64, Option<f32>)],
    duration_ms: u64,
    curve: AnimationCurve,
) -> Result<(), String> {
    if samples.is_empty() {
        return Err("envelope needs at least one sample".to_string());
    }
    let (first_offset, first_progress) = samples[0];
    if first_offset != 0 {
        return Err(format!(
            "first sample must sit at the transition start, got {first_offset} ms"
        ));
    }
    let first = first_progress.ok_or_else(|| "transition must arm at the start".to_string())?;
    if first.abs() > 1e-4 {
        return Err(format!("transition must start at 0, got {first}"));
    }
    let mut prev = first;
    let mut advanced = false;
    for &(offset, progress) in &samples[1..] {
        let Some(value) = progress else {
            if offset < duration_ms {
                return Err(format!("transition dropped early at {offset} ms"));
            }
            continue;
        };
        if offset >= duration_ms {
            return Err(format!(
                "transition still reports {value} at {offset} ms past its {duration_ms} ms duration"
            ));
        }
        let raw = (offset as f32 / duration_ms as f32).clamp(0.0, 1.0);
        let expected = curve.eval(raw);
        if (value - expected).abs() > 1e-4 {
            return Err(format!(
                "progress at {offset} ms must follow {curve:?} ({expected}), got {value}"
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
        return Err("transition never advanced: a flat envelope is an instant cut".to_string());
    }
    let (last_offset, last_progress) = samples[samples.len() - 1];
    if last_offset >= duration_ms && last_progress.is_some() {
        return Err(format!(
            "transition must complete by {duration_ms} ms, still active at {last_offset} ms"
        ));
    }
    Ok(())
}

/// Checks that every captured frame carries identical presented geometry:
/// the layout committed immediately and nothing interpolated while chrome
/// faded.
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
                "geometry must stay static mid-transition: start {:?} vs {} ms {:?}",
                first.geometry, frame.offset_ms, frame.geometry
            ));
        }
    }
    Ok(())
}

/// Checks that the primary grid never moved across the captured frames.
fn check_grid_static(frames: &[CapturedFrame]) -> Result<(), String> {
    let Some(first) = frames.first() else {
        return Err("grid check needs at least one frame".to_string());
    };
    for frame in &frames[1..] {
        if frame.grid != first.grid {
            return Err(format!(
                "grid must stay static mid-transition: digest changed by {} ms",
                frame.offset_ms
            ));
        }
    }
    Ok(())
}

fn two_pane_split() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    )
}

fn single_leaf() -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(1), 80, 24))
}

/// Left-edge ring pixel of `view` plus the surface stride, following the same
/// probe convention as the existing ring-fade test.
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

/// Asserts a mid-transition ring pixel sits strictly between the background
/// and the committed final color on every channel: the software compositor
/// blends straight-alpha chrome over the background, so a partial alpha lands
/// between the two endpoints.
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
fn split_open_envelope_fades_ring_with_static_geometry_and_grid() {
    let start = Instant::now();
    let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
    let policy = harness.rt.config().animations;
    let duration_ms = policy.duration(AnimationKind::Open).as_millis() as u64;
    assert_eq!(
        duration_ms, 150,
        "default open duration pins the sample grid"
    );
    let curve = policy.curve(AnimationKind::Open);

    harness.rt.handle_pty_bytes(b"GRID-TRUTH");
    let baseline = harness.capture(0, AnimationKind::Open, Some(ViewId::new(2)));
    assert!(baseline.presented, "baseline frame must present the grid");

    // A new split View arms the open transition on the virtual clock.
    harness.rt.set_layout(two_pane_split());
    let series = harness.drive(
        AnimationKind::Open,
        Some(ViewId::new(2)),
        &[0, 25, 50, 75, 100, 125],
    );
    assert!(series[0].presented, "open must present the split");
    for frame in &series {
        assert!(frame.presented, "active open must keep presenting");
        let rounded = frame.stats.map(|s| s.rounded_fills).unwrap_or(0);
        assert!(
            rounded >= 1,
            "every open frame must paint Core-owned chrome at {} ms",
            frame.offset_ms
        );
    }
    let end_frame = harness.tick_end(duration_ms, AnimationKind::Open, Some(ViewId::new(2)));
    let mut samples: Vec<(u64, Option<f32>)> =
        series.iter().map(|f| (f.offset_ms, f.progress)).collect();
    samples.push((duration_ms, end_frame.progress));
    check_progress_envelope(&samples, duration_ms, curve)
        .expect("open must follow its easing envelope");
    check_geometry_static(&series).expect("split geometry must commit immediately");
    // The split commit reflows the grid once (80-wide to the owner leaf's
    // width); after that commit nothing may move while chrome fades. The
    // pre-split baseline is excluded: its digest predates the one-time
    // reflow, which is a commit, not interpolation.
    let mut transition_frames = series.clone();
    transition_frames.push(end_frame.clone());
    check_grid_static(&transition_frames).expect("grid must never interpolate during open");
    let snap = harness.rt.snapshot();
    let row: String = snap
        .cells
        .chunks(snap.width)
        .next()
        .expect("row 1")
        .iter()
        .take(10)
        .map(|c| c.glyph)
        .collect();
    assert_eq!(
        row, "GRID-TRUTH",
        "grid content survives the commit verbatim"
    );

    // Pixel evidence: the ring fades in gradually, so a mid-transition ring
    // pixel sits strictly between the background and the committed color.
    // Both bytes come from the single forward drive above.
    let (ring_x, ring_y, stride) = ring_probe(&harness.rt, ViewId::new(2));
    let mid_rgba = series[3]
        .rgba
        .as_deref()
        .expect("mid frame must carry raw bytes");
    let mid_px = rgba_at(mid_rgba, stride, ring_x, ring_y);
    let final_rgba = end_frame
        .rgba
        .as_deref()
        .expect("end frame must carry raw bytes");
    let final_px = rgba_at(final_rgba, stride, ring_x, ring_y);
    assert_fade_between(mid_px, final_px);

    // Frame-on-demand: after the end state commits, the path idles with no
    // deadline (zero periodic wakeups attributable to animation).
    let end_at = start + Duration::from_millis(duration_ms);
    assert!(
        harness.rt.tick_at(end_at).is_none(),
        "idle after open completes"
    );
    assert_eq!(harness.rt.animation_deadline(), None);
}

#[test]
fn zoom_engage_and_disengage_envelopes() {
    let start = Instant::now();
    let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
    let policy = harness.rt.config().animations;
    let close_ms = policy.duration(AnimationKind::Close).as_millis() as u64;
    let open_ms = policy.duration(AnimationKind::Open).as_millis() as u64;
    assert_eq!((close_ms, open_ms), (120, 150));

    harness.rt.handle_pty_bytes(b"ZOOM-TRUTH");
    harness.capture(0, AnimationKind::Open, Some(ViewId::new(2)));
    harness.rt.set_layout(two_pane_split());
    harness.capture(0, AnimationKind::Open, Some(ViewId::new(2)));
    let settled = start + Duration::from_millis(open_ms);
    harness.rt.tick_at(settled);

    // Zoom engage collapses to one leaf through the layout funnel. The tree
    // commits at once. Issue #2577: the hidden leaf's retained ring is stale
    // chrome and must never paint over the zoomed owner, so the old rect
    // presents the owner from the first close frame.
    let closing_rect = harness
        .rt
        .present_frames()
        .into_iter()
        .find(|f| f.view == ViewId::new(2))
        .expect("split must show the second leaf")
        .frame;
    let probe_pad = usize::try_from(harness.rt.window_padding_physical()).expect("pad fits");
    let probe_x = probe_pad + usize::try_from(closing_rect.x).expect("rect x fits");
    let probe_y = probe_pad + usize::try_from(closing_rect.y).expect("rect y fits") + 50;
    let stride = usize::try_from(harness.rt.config().window_extent().width()).expect("stride");
    let pre_rgba = harness
        .rt
        .headless_rgba()
        .expect("pre-close frame must carry raw bytes");
    let pre_px = rgba_at(&pre_rgba, stride, probe_x, probe_y);
    harness.rt.set_layout(single_leaf());
    assert_eq!(
        harness.rt.layout().leaf_ids(),
        vec![ViewId::new(1)],
        "zoom geometry must commit immediately"
    );
    // Re-anchor the harness clock to the engage instant so the envelope is
    // sampled against the close duration from its own start.
    harness.start = settled;
    let close_series = harness.drive(AnimationKind::Close, Some(ViewId::new(2)), &[0, 30, 60, 90]);
    assert!(close_series[0].presented, "zoom engage must present");
    let close_end_frame = harness.tick_end(close_ms, AnimationKind::Close, Some(ViewId::new(2)));
    let mut close_samples: Vec<(u64, Option<f32>)> = close_series
        .iter()
        .map(|f| (f.offset_ms, f.progress))
        .collect();
    close_samples.push((close_ms, close_end_frame.progress));
    check_progress_envelope(&close_samples, close_ms, policy.curve(AnimationKind::Close))
        .expect("zoom engage must follow the close envelope");
    check_geometry_static(&close_series).expect("zoom geometry must stay committed");
    check_grid_static(&close_series).expect("grid must never interpolate during zoom");
    let zoom_snap = harness.rt.snapshot();
    let zoom_row: String = zoom_snap
        .cells
        .chunks(zoom_snap.width)
        .next()
        .expect("row 1")
        .iter()
        .take(10)
        .map(|c| c.glyph)
        .collect();
    assert_eq!(
        zoom_row, "ZOOM-TRUTH",
        "grid content survives zoom engage verbatim"
    );

    // Issue #2577 pixel evidence: the hidden leaf's old border probe now sits
    // inside the zoomed owner. The stale closing ring may not paint there, so
    // the probe shows the owner's surface from the first close frame instead
    // of a fading accent border.
    let background = harness.rt.config().theme.background;
    let mid_rgba = close_series[2]
        .rgba
        .as_deref()
        .expect("mid frame must carry raw bytes");
    let mid_px = rgba_at(mid_rgba, stride, probe_x, probe_y);
    let cleared_rgba = close_end_frame
        .rgba
        .as_deref()
        .expect("end frame must carry raw bytes");
    let cleared_px = rgba_at(cleared_rgba, stride, probe_x, probe_y);
    assert_ne!(
        pre_px, cleared_px,
        "the probe must see the removed leaf's ring before the zoom commits"
    );
    assert_eq!(
        mid_px, cleared_px,
        "no retained closing ring may paint over the zoomed owner (#2577)"
    );
    assert_eq!(
        mid_px, background,
        "the old rect presents the live owner's surface during the close"
    );

    // Zoom disengage restores the split; the restored leaf fades its ring in.
    harness.rt.set_layout(two_pane_split());
    assert_eq!(harness.rt.layout().leaf_ids().len(), 2);
    let close_end_at = settled + Duration::from_millis(close_ms);
    harness.start = close_end_at;
    let open_series = harness.drive(AnimationKind::Open, Some(ViewId::new(2)), &[0, 50, 100]);
    let open_end_frame = harness.tick_end(open_ms, AnimationKind::Open, Some(ViewId::new(2)));
    let mut open_samples: Vec<(u64, Option<f32>)> = open_series
        .iter()
        .map(|f| (f.offset_ms, f.progress))
        .collect();
    open_samples.push((open_ms, open_end_frame.progress));
    check_progress_envelope(&open_samples, open_ms, policy.curve(AnimationKind::Open))
        .expect("zoom release must follow the open envelope");
    check_geometry_static(&open_series).expect("restored geometry must stay committed");
    let open_end_at = close_end_at + Duration::from_millis(open_ms);
    assert!(
        harness.rt.tick_at(open_end_at).is_none(),
        "idle after zoom completes"
    );
}

#[test]
fn workspace_switch_envelope_with_static_chrome() {
    let start = Instant::now();
    let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
    let policy = harness.rt.config().animations;
    let duration_ms = policy.duration(AnimationKind::Workspace).as_millis() as u64;
    assert_eq!(
        duration_ms, 200,
        "default workspace duration pins the sample grid"
    );
    let curve = policy.curve(AnimationKind::Workspace);

    harness.capture(0, AnimationKind::Workspace, None);
    harness.rt.workspace_new().expect("second workspace");
    let series = harness.drive(AnimationKind::Workspace, None, &[0, 50, 100, 150]);
    assert!(series[0].presented, "workspace switch must present");
    let end_frame = harness.tick_end(duration_ms, AnimationKind::Workspace, None);
    let mut samples: Vec<(u64, Option<f32>)> =
        series.iter().map(|f| (f.offset_ms, f.progress)).collect();
    samples.push((duration_ms, end_frame.progress));
    check_progress_envelope(&samples, duration_ms, curve)
        .expect("workspace switch must follow its easing envelope");
    check_geometry_static(&series).expect("workspace geometry must not interpolate");
    check_grid_static(&series).expect("grid must never interpolate across a switch");

    // Pixel evidence: the whole-surface factor scales the focused ring, so a
    // mid-switch ring pixel sits strictly between background and final. Both
    // bytes come from the single forward drive above.
    let fresh = harness
        .rt
        .present_frames()
        .first()
        .expect("switched workspace must present a leaf")
        .view;
    let (ring_x, ring_y, stride) = ring_probe(&harness.rt, fresh);
    let mid_rgba = series[2]
        .rgba
        .as_deref()
        .expect("mid frame must carry raw bytes");
    let mid_px = rgba_at(mid_rgba, stride, ring_x, ring_y);
    let final_rgba = end_frame
        .rgba
        .as_deref()
        .expect("end frame must carry raw bytes");
    let final_px = rgba_at(final_rgba, stride, ring_x, ring_y);
    assert_fade_between(mid_px, final_px);

    let end_at = start + Duration::from_millis(duration_ms);
    assert!(
        harness.rt.tick_at(end_at).is_none(),
        "idle after switch completes"
    );
    assert_eq!(harness.rt.animation_deadline(), None);
}

#[test]
fn concurrent_surface_budget_cuts_instantly_past_eight() {
    assert_eq!(
        MAX_CONCURRENT_ANIMATIONS, 8,
        "harness assumes the accepted 8-surface budget"
    );
    let start = Instant::now();
    let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
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
    // Past capacity the trigger commits its end state immediately: no
    // animation arms, no progress ever reports, and the flat series is not
    // an envelope the harness would accept.
    assert!(
        !harness
            .rt
            .trigger_animation(AnimationKind::Workspace, None, start),
        "ninth trigger must cut instantly"
    );
    assert!(
        !harness
            .rt
            .trigger_animation(AnimationKind::Open, Some(ViewId::new(999)), start),
        "ninth split must cut instantly"
    );
    assert!(
        harness
            .rt
            .animation_progress(AnimationKind::Workspace, None, start)
            .is_none(),
        "refused switch must never report progress"
    );
    assert!(
        harness
            .rt
            .animation_progress(AnimationKind::Open, Some(ViewId::new(999)), start)
            .is_none(),
        "refused split must never report progress"
    );
    assert!(
        check_progress_envelope(&[(0, None)], 150, AnimationCurve::EaseOut).is_err(),
        "harness must reject the flat instant-cut series as an envelope"
    );

    // The bounded set still completes on time with frame-on-demand idle.
    let end = start + Duration::from_millis(200);
    assert!(
        harness.rt.tick_at(end).is_some(),
        "bounded set still presents its final frame"
    );
    assert!(harness.rt.tick_at(end).is_none(), "idle after bounded set");
    assert_eq!(harness.rt.animation_deadline(), None);
    assert!(
        harness
            .rt
            .trigger_animation(AnimationKind::Workspace, None, end),
        "capacity returns after completion"
    );
}

#[test]
fn reduced_motion_and_disabled_policies_cut_instantly() {
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
            duration_ms: [0; AnimationKind::COUNT],
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
        let _ = harness.rt.tick_at(start);
        harness.rt.set_layout(two_pane_split());
        assert!(
            harness.rt.tick_at(start).is_some(),
            "instant split still presents once: {policy:?}"
        );
        assert!(
            !harness.rt.animations_active(),
            "instant split must not arm: {policy:?}"
        );
        assert!(
            harness
                .rt
                .animation_progress(AnimationKind::Open, Some(ViewId::new(2)), start)
                .is_none(),
            "instant split must never report progress: {policy:?}"
        );
        assert!(
            harness.rt.tick_at(start).is_none(),
            "instant split idles immediately: {policy:?}"
        );
        assert_eq!(harness.rt.animation_deadline(), None);

        // Zoom still collapses without arming.
        harness.rt.set_layout(single_leaf());
        assert_eq!(harness.rt.layout().leaf_ids(), vec![ViewId::new(1)]);
        assert!(
            harness.rt.tick_at(start).is_some(),
            "instant zoom presents once"
        );
        assert!(
            harness
                .rt
                .animation_progress(AnimationKind::Close, Some(ViewId::new(2)), start)
                .is_none(),
            "instant zoom must not arm: {policy:?}"
        );

        // Workspace switch still moves without arming.
        let mut switched = OffscreenHarness::new(policy, start);
        let _ = switched.rt.tick_at(start);
        switched.rt.workspace_new().expect("new workspace");
        assert!(
            switched.rt.tick_at(start).is_some(),
            "instant switch presents once"
        );
        assert!(
            switched
                .rt
                .animation_progress(AnimationKind::Workspace, None, start)
                .is_none(),
            "instant switch must not arm: {policy:?}"
        );
        assert!(switched.rt.tick_at(start).is_none(), "instant switch idles");
    }
}

#[test]
fn frame_hashes_are_deterministic_across_identical_drives() {
    // The headless seam must reproduce bit-identical frames for identical
    // virtual-clock drives: this is what makes the harness portable across
    // Linux, macOS, and Windows CI.
    fn drive_once() -> Vec<u64> {
        let start = Instant::now();
        let mut harness = OffscreenHarness::new(AnimationPolicy::default(), start);
        harness.rt.handle_pty_bytes(b"DETERMINISM");
        let _ = harness.rt.tick_at(start);
        harness.rt.set_layout(two_pane_split());
        let series = harness.drive(
            AnimationKind::Open,
            Some(ViewId::new(2)),
            &[0, 25, 50, 75, 100, 125, 150],
        );
        assert!(series.iter().all(|f| f.presented));
        harness.presented_hashes()
    }
    let first = drive_once();
    let second = drive_once();
    assert_eq!(first.len(), second.len());
    assert_eq!(first, second, "identical drives must hash identically");
    assert!(
        first.windows(2).any(|pair| pair[0] != pair[1]),
        "the envelope must actually move pixels across frames"
    );
}

#[test]
fn broken_envelopes_fail_the_harness_checks() {
    // A geometry series that interpolates (the forbidden behavior: layout
    // gliding between endpoints mid-transition) must fail the static check.
    let baseline = CapturedFrame {
        offset_ms: 0,
        presented: true,
        progress: Some(0.0),
        hash: Some(1),
        rgba: None,
        geometry: vec![GeomKey {
            view: 2,
            frame_x: 100,
            frame_y: 50,
            frame_w: 200,
            frame_h: 300,
            content_x: 102,
            content_y: 52,
            content_w: 196,
            content_h: 296,
            cols: 21,
            rows: 15,
            border: 1,
            radius: 6,
        }],
        grid: 7,
        stats: None,
    };
    let mut gliding = baseline.clone();
    gliding.offset_ms = 75;
    gliding.geometry[0].frame_x = 150;
    assert!(
        check_geometry_static(&[baseline.clone(), gliding]).is_err(),
        "interpolated geometry must fail the static check"
    );
    assert!(
        check_geometry_static(&[baseline.clone(), baseline.clone()]).is_ok(),
        "static geometry must pass"
    );

    // A non-monotone progress series (motion running backwards) must fail
    // the envelope check even when endpoints look right.
    let backwards = vec![
        (0u64, Some(0.0)),
        (50u64, Some(0.6)),
        (100u64, Some(0.4)),
        (150u64, None),
    ];
    assert!(
        check_progress_envelope(&backwards, 150, AnimationCurve::EaseOut).is_err(),
        "backwards motion must fail the envelope check"
    );

    // A curve mismatch (reporting linear timing on an ease-out contract)
    // must fail as well.
    let linear_on_ease_out = vec![(0u64, Some(0.0)), (75u64, Some(0.5)), (150u64, None)];
    assert!(
        check_progress_envelope(&linear_on_ease_out, 150, AnimationCurve::EaseOut).is_err(),
        "linear timing on an ease-out contract must fail"
    );

    // A grid digest that moves mid-transition must fail the grid check.
    let mut moved_grid = baseline.clone();
    moved_grid.offset_ms = 75;
    moved_grid.grid = 8;
    assert!(
        check_grid_static(&[baseline, moved_grid]).is_err(),
        "grid motion mid-transition must fail the grid check"
    );
}
