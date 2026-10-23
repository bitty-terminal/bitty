#![forbid(unsafe_code)]
//! Issue #2522: a floating panel must not erase the outlines of the tiled
//! panels behind it, and the pink profile (`decoration = { gaps_in = 4,
//! gaps_out = 2, border = 1, radius = 4 }`, ayu-mirage) must keep all four
//! outlines on every panel.
//!
//! History: CTX-1280 (#2168) suppressed every background-tier leaf's
//! Core-owned chrome (shadow/glow/ring) while a detached float was presented
//! ("clean backdrop"). That erased *all* outlines of tiled panels, including
//! panels the float never covered, which the owner reported as #2522. The
//! float's opaque frame cover (CTX-1271, #2138) already occludes exactly the
//! area the float covers, so the backdrop suppression is retired: tiled
//! chrome always paints, the stable tier sort keeps floats above it, and the
//! cover still hides the tiled chrome under the float.
//!
//! Every assertion below is deterministic headless RGBA (software
//! compositor) or pure `PresentFrame` geometry: no smoke tests.

use bitty_runtime::config::{DEFAULT_OUTLINE_FOCUSED, DEFAULT_OUTLINE_IDLE};
use bitty_runtime::{
    AnimationPolicy, Decoration, LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis,
    ThemePalette, UiRect, View, ViewId,
};
use bitty_ui::PresentationMode;

/// Theme background of the deterministic harness (Bitty Dark).
const THEME_BG: [u8; 4] = [0x1E, 0x1E, 0x2E, 0xFF];

/// The owner's pink profile decoration, in logical pixels.
fn pink() -> Decoration {
    Decoration::new(4, 2, 1, 4, 6)
}

fn make_runtime(decoration: Decoration) -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        decoration,
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless harness runtime must build")
}

fn three_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        ),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn pixel(rgba: &[u8], width: u32, x: i32, y: i32) -> [u8; 4] {
    assert!(x >= 0 && y >= 0, "probe must be on-surface");
    let base = (y as usize * width as usize + x as usize) * 4;
    [rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]]
}

/// Raw-RGBA evidence dump for review and screenshot evidence, same
/// convention as `decoration_present.rs`: set `BITTY_2522_EVIDENCE_DIR` to a
/// directory and the tests write `<name>.rgba` plus `<name>.dims`. Never
/// required for the assertions.
fn dump_evidence(name: &str, rt: &Runtime) {
    let Some(dir) = std::env::var_os("BITTY_2522_EVIDENCE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Some(rgba) = rt.headless_rgba() else {
        return;
    };
    let Some(extent) = rt.surface_extent() else {
        return;
    };
    let _ = std::fs::write(dir.join(format!("{name}.rgba")), &rgba);
    let _ = std::fs::write(
        dir.join(format!("{name}.dims")),
        format!("{}x{}\n", extent.width(), extent.height()),
    );
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Straight-alpha src-over of the ring colour onto the surface background.
fn blend_over(dst: [u8; 4], src: [u8; 4]) -> [u8; 4] {
    let a = u32::from(src[3]);
    let inv = 255 - a;
    [
        ((u32::from(src[0]) * a + u32::from(dst[0]) * inv) / 255) as u8,
        ((u32::from(src[1]) * a + u32::from(dst[1]) * inv) / 255) as u8,
        ((u32::from(src[2]) * a + u32::from(dst[2]) * inv) / 255) as u8,
        0xFF,
    ]
}

fn expected_ring(rt: &Runtime, view: ViewId) -> [u8; 4] {
    let outline = if rt.focused_view() == Some(view) {
        DEFAULT_OUTLINE_FOCUSED
    } else {
        DEFAULT_OUTLINE_IDLE
    };
    blend_over(THEME_BG, outline)
}

/// Resolved Core-owned ring width in physical px. These fixtures carry no
/// `views` overrides, so the per-`View` resolution defaults to the global
/// `decoration.border`; the elevated float geometry border additionally adds
/// `FLOAT_BORDER_EXTRA` px of content inset without widening the ring.
fn ring_width(rt: &Runtime) -> i32 {
    i32::from(rt.outline_width_physical(u32::from(rt.config().decoration.border)))
}

/// Window-space frame rectangle (padding included) as `(x0, y0, x1, y1)`.
fn window_rect(rt: &Runtime, frame: &PresentFrame) -> (i32, i32, i32, i32) {
    let pad = rt.window_padding_physical() as i32;
    let x0 = pad + frame.frame.x;
    let y0 = pad + frame.frame.y;
    (
        x0,
        y0,
        x0 + frame.frame.width as i32,
        y0 + frame.frame.height as i32,
    )
}

/// Asserts the full straight span of all four ring bands carries the
/// resolved outline colour for `frame`. Corner arcs (radius) are excluded
/// because they are legitimately partial coverage; the straight bands are
/// fully covered at `border >= 1` and must be exact.
fn assert_all_edges(rt: &Runtime, frame: &PresentFrame, label: &str) -> usize {
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = rt.surface_extent().expect("extent").width();
    let (x0, y0, x1, y1) = window_rect(rt, frame);
    let border = ring_width(rt);
    let radius = i32::from(frame.radius);
    assert!(border >= 1, "{label}: ring width must be >= 1");
    assert!(x1 - x0 > 2 * radius, "{label}: frame too narrow");
    assert!(y1 - y0 > 2 * radius, "{label}: frame too short");
    let want = expected_ring(rt, frame.view);
    let mut checked = 0usize;
    for dx in 0..border {
        for y in (y0 + radius + 1)..(y1 - radius - 1) {
            let left = pixel(&rgba, width, x0 + dx, y);
            assert_eq!(left, want, "{label}: left border at ({},{y})", x0 + dx);
            let right = pixel(&rgba, width, x1 - 1 - dx, y);
            assert_eq!(
                right,
                want,
                "{label}: right border at ({},{y})",
                x1 - 1 - dx
            );
            checked += 2;
        }
        for x in (x0 + radius + 1)..(x1 - radius - 1) {
            let top = pixel(&rgba, width, x, y0 + dx);
            assert_eq!(top, want, "{label}: top border at ({x},{})", y0 + dx);
            let bottom = pixel(&rgba, width, x, y1 - 1 - dx);
            assert_eq!(
                bottom,
                want,
                "{label}: bottom border at ({x},{})",
                y1 - 1 - dx
            );
            checked += 2;
        }
    }
    checked
}

/// Asserts the #2522 split between visible and occluded band pixels:
///
/// - every band pixel **outside** `occluder` must carry tiled chrome
///   (non-background); the retired CTX-1280 suppression left them empty;
/// - band pixels **inside** `occluder` are owned by the float frame cover and
///   are skipped here; the caller pins the opaque occlusion with an interior
///   probe so lower chrome cannot leak over the float.
///
/// Returns the number of outside samples checked (non-vacuity guard).
fn assert_band_visibility(
    rt: &Runtime,
    frame: &PresentFrame,
    occluder: (i32, i32, i32, i32),
    label: &str,
) -> usize {
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = rt.surface_extent().expect("extent").width();
    let (x0, y0, x1, y1) = window_rect(rt, frame);
    let radius = i32::from(frame.radius);
    let mut outside = 0usize;
    let probe = |x: i32, y: i32, outside: &mut usize| {
        let hidden = (occluder.0..occluder.2).contains(&x) && (occluder.1..occluder.3).contains(&y);
        if hidden {
            // Covered by the float frame: the cover owns these pixels (the
            // dedicated interior probe below pins the opaque occlusion).
            return;
        }
        *outside += 1;
        assert_ne!(
            pixel(&rgba, width, x, y),
            THEME_BG,
            "{label}: tiled band at ({x},{y}) was erased by the float"
        );
    };
    for y in (y0 + radius + 1)..(y1 - radius - 1) {
        probe(x0, y, &mut outside);
        probe(x1 - 1, y, &mut outside);
    }
    for x in (x0 + radius + 1)..(x1 - radius - 1) {
        probe(x, y0, &mut outside);
        probe(x, y1 - 1, &mut outside);
    }
    outside
}

#[test]
fn tiled_outlines_survive_behind_detached_float_issue_2522() {
    let mut rt = make_runtime(pink());
    install(&mut rt, three_pane());
    let baseline = rt.tick().expect("tiled baseline must present");
    assert!(
        baseline.rounded_fills >= 3,
        "tiled baseline paints one ring per pane, got {}",
        baseline.rounded_fills
    );
    for view in [ViewId::new(1), ViewId::new(2), ViewId::new(3)] {
        let frame = frame_of(&rt, view);
        assert!(assert_all_edges(&rt, &frame, "baseline") > 100);
    }
    dump_evidence("01-tiled-baseline", &rt);

    // Detach the left leaf: survivors refill the container and the float is
    // centered over both of them.
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    let tick = rt.tick().expect("detached float frame must present");
    // Float cover + float ring + both survivor rings; the retired suppression
    // would have left only the float's two rounded fills here.
    assert!(
        tick.rounded_fills >= 3,
        "float cover + float ring + survivor rings must paint, got {}",
        tick.rounded_fills
    );

    let float = frame_of(&rt, ViewId::new(1));
    assert_eq!(float.tier, Some(bitty_runtime::OverlayTier::Float));
    let float_rect = window_rect(&rt, &float);
    assert!(assert_all_edges(&rt, &float, "float") > 100);

    for view in [ViewId::new(2), ViewId::new(3)] {
        let frame = frame_of(&rt, view);
        assert_eq!(frame.tier, None, "survivor {view:?} stays tiled");
        let outside = assert_band_visibility(&rt, &frame, float_rect, "detached float");
        assert!(
            outside > 100,
            "{view:?}: the outside-float probe must be non-vacuous ({outside})"
        );

        // The float frame interior is opaque background where a hidden ring
        // would otherwise show (pane 2's right edge crosses the float).
        let rect = window_rect(&rt, &frame);
        let mid_y = (rect.1 + rect.3) / 2;
        if (float_rect.0..float_rect.2).contains(&(rect.2 - 1))
            && (float_rect.1..float_rect.3).contains(&mid_y)
        {
            let rgba = rt.headless_rgba().expect("rgba");
            let width = rt.surface_extent().expect("extent").width();
            assert_eq!(
                pixel(&rgba, width, rect.2 - 1, mid_y),
                THEME_BG,
                "the float cover must occlude the tiled ring under it"
            );
        }
    }

    dump_evidence("02-detached-float-outlines", &rt);

    // Restoring the float returns the plain tiled scene; outlines still paint.
    rt.float_restore(ViewId::new(1))
        .expect("restore must apply");
    let restored = rt.tick().expect("restored scene must present");
    assert!(
        restored.rounded_fills >= 3,
        "every pane ring returns after restore, got {}",
        restored.rounded_fills
    );
    for view in [ViewId::new(1), ViewId::new(2), ViewId::new(3)] {
        let frame = frame_of(&rt, view);
        assert!(assert_all_edges(&rt, &frame, "restored") > 100);
    }
}

#[test]
fn mode_float_keeps_tiled_outlines_outside_its_frame_issue_2522() {
    let mut rt = make_runtime(pink());
    let mut tree = three_pane();
    // Float the third pane (last in tree order) so it paints above the
    // siblings; the elevated frame is centered over the container.
    tree.find_leaf_mut(ViewId::new(3))
        .expect("leaf 3")
        .set_presentation(PresentationMode::Floating);
    install(&mut rt, tree);
    let tick = rt.tick().expect("mode-float frame must present");
    assert!(tick.rounded_fills >= 3);

    let float = frame_of(&rt, ViewId::new(3));
    assert_eq!(float.tier, Some(bitty_runtime::OverlayTier::Float));
    let float_rect = window_rect(&rt, &float);
    assert!(assert_all_edges(&rt, &float, "mode float") > 100);

    for view in [ViewId::new(1), ViewId::new(2)] {
        let frame = frame_of(&rt, view);
        let outside = assert_band_visibility(&rt, &frame, float_rect, "mode float");
        assert!(outside > 100, "{view:?}: probe must be non-vacuous");
    }
}

/// The reported profile: ayu-mirage, `border = 1`, `radius = 4`. Asserts the
/// exact right (and left/top/bottom) ring band on every pane at scale 1.0 and
/// at the owner's 1.6 HiDPI scale, so a half-pixel/rounding or clip-inset
/// regression on the right edge fails deterministically.
#[test]
fn pink_profile_keeps_right_border_at_border1_radius4_issue_2522() {
    let palette = ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("ayu-mirage")));
    for scale in [1.0, 1.6] {
        let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig {
            decoration: pink(),
            theme: palette,
            // ayu-mirage outline tokens (bitty-config theme table): the
            // resolved ring colours the profile actually paints.
            outline_idle: [0x68, 0x68, 0x68, 0xFF],
            outline_focused: [0x95, 0xE6, 0xCB, 0xFF],
            animations: AnimationPolicy {
                enabled: false,
                ..AnimationPolicy::default()
            },
            ..RuntimeConfig::default()
        })
        .expect("pink runtime must build");
        install(&mut rt, three_pane());
        if scale != 1.0 {
            rt.apply_dpi_scale(scale, Some(bitty_platform::PhysicalSize::new(2560, 1600)));
        }
        rt.tick().expect("pink frame must present");
        dump_evidence(&format!("03-pink-profile-scale-{scale}"), &rt);

        let bg = rt.config().theme.background;
        assert_eq!(bg, [0x1F, 0x24, 0x30, 0xFF], "ayu-mirage background");
        for view in [ViewId::new(1), ViewId::new(2), ViewId::new(3)] {
            let frame = frame_of(&rt, view);
            // Geometry: the content inset is symmetric on every edge, so the
            // right ring band can never be consumed by a lopsided clip.
            let right_inset = (frame.frame.x + frame.frame.width as i32)
                - (frame.content.x + frame.content.width as i32);
            let left_inset = frame.content.x - frame.frame.x;
            assert_eq!(
                left_inset, right_inset,
                "scale {scale} {view:?}: content inset must be symmetric"
            );
            assert!(
                left_inset >= i32::from(frame.border),
                "scale {scale} {view:?}: inset must at least clear the ring"
            );

            // Paint: all four straight bands carry the exact resolved ring
            // colour. `border = 1`/`radius = 4` at scale 1.6 resolves to
            // `border = 2`/`radius = 6`; both bands must be fully covered.
            let rgba = rt.headless_rgba().expect("rgba");
            let width = rt.surface_extent().expect("extent").width();
            let (x0, y0, x1, y1) = window_rect(&rt, &frame);
            let border = ring_width(&rt);
            let radius = i32::from(frame.radius);
            let outline = if rt.focused_view() == Some(view) {
                rt.config().outline_focused
            } else {
                rt.config().outline_idle
            };
            let want = blend_over(bg, outline);
            assert_eq!(want, outline, "ayu-mirage outlines are opaque");
            for dx in 0..border {
                for y in (y0 + radius + 1)..(y1 - radius - 1) {
                    assert_eq!(
                        pixel(&rgba, width, x1 - 1 - dx, y),
                        want,
                        "scale {scale} {view:?}: right border column {} missing at y={y}",
                        x1 - 1 - dx
                    );
                    assert_eq!(
                        pixel(&rgba, width, x0 + dx, y),
                        want,
                        "scale {scale} {view:?}: left border at ({},{y})",
                        x0 + dx
                    );
                }
                for x in (x0 + radius + 1)..(x1 - radius - 1) {
                    assert_eq!(
                        pixel(&rgba, width, x, y0 + dx),
                        want,
                        "scale {scale} {view:?}: top border at ({x},{})",
                        y0 + dx
                    );
                    assert_eq!(
                        pixel(&rgba, width, x, y1 - 1 - dx),
                        want,
                        "scale {scale} {view:?}: bottom border at ({x},{})",
                        y1 - 1 - dx
                    );
                }
            }
        }
    }
}
