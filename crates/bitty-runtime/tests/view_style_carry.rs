//! Chrome-style S2 carry (CTX-1161), consumed by the S3 present path
//! (CTX-1162).
//!
//! Proves headlessly, without a display server or GPU, that
//! `resolve_view_style` plus the [`RuntimeViewStyle`] carry resolves per
//! `View` (global base plus per-field per-tier `views` overrides) and is
//! stored on every [`PresentFrame`] at present build.
//!
//! S3 owns the present-path paint plus the `set_chrome_style` adopter: the
//! carry-population tests below still hold, while the byte-identical and
//! no-consumer expectations that S2 pinned for its inert stage moved to the
//! S3 suite (`style_present_s3`), where styled configs paint and unstyled
//! ones stay byte-identical.

#![forbid(unsafe_code)]

use bitty_runtime::{PresentFrame, Runtime, RuntimeConfig, RuntimeViewStyle, ViewId};

/// A runtime whose config carries the maximum accepted style surface plus
/// one per-`View` override for view 1.
fn styled_runtime() -> Runtime {
    let cfg = RuntimeConfig {
        style_glow_color: Some([0xFF, 0x00, 0x00, 0xFF]),
        style_glow_width: bitty_runtime::config::MAX_STYLE_GLOW_WIDTH_PX,
        style_shadow_color: Some([0x00, 0x00, 0x00, 0x80]),
        style_shadow_offset_x: bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        style_shadow_offset_y: -bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        style_fade_unfocused: 0.0,
        background_tint: Some([0x11, 0x22, 0x33, 0xFF]),
        background_tint_strength: 1.0,
        view_appearance: vec![bitty_runtime::ViewAppearanceRule {
            selector: String::from("view:1"),
            glow_width: Some(3),
            background_tint_strength: Some(0.5),
            ..Default::default()
        }],
        ..RuntimeConfig::default()
    };
    cfg.validate().expect("max style surface must validate");
    Runtime::new(cfg).expect("styled runtime must build")
}

fn present_bytes(rt: &mut Runtime, input: &[u8]) -> Vec<u8> {
    rt.handle_pty_bytes(input);
    rt.tick().expect("first present must draw");
    rt.headless_rgba().expect("rgba after tick")
}

/// Geometry projection of one frame: every field except the inert `style`
/// carry. Two frame lists agree on geometry exactly when S2 changed no
/// layout, clip, or metric.
type FrameGeometry = (
    ViewId,
    (i32, i32, u32, u32),
    (i32, i32, u32, u32),
    u16,
    u16,
    u16,
    u16,
    String,
);

fn frame_geometry(frame: &PresentFrame) -> FrameGeometry {
    (
        frame.view,
        (
            frame.frame.x,
            frame.frame.y,
            frame.frame.width,
            frame.frame.height,
        ),
        (
            frame.content.x,
            frame.content.y,
            frame.content.width,
            frame.content.height,
        ),
        frame.cols,
        frame.rows,
        frame.border,
        frame.radius,
        format!("{:?}", frame.tier),
    )
}

#[test]
fn style_carry_is_populated_per_view() {
    let rt = styled_runtime();
    let frames = rt.present_frames();
    assert!(!frames.is_empty(), "one default leaf must present");
    let frame = frames
        .iter()
        .find(|f| f.view == ViewId::new(1))
        .expect("view 1 must present");
    // Global base plus the `view:1` per-field overrides.
    assert_eq!(frame.style.glow_color, Some([0xFF, 0x00, 0x00, 0xFF]));
    assert_eq!(frame.style.glow_width, 3);
    assert_eq!(frame.style.shadow_color, Some([0x00, 0x00, 0x00, 0x80]));
    assert_eq!(
        frame.style.shadow_offset_x,
        bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX
    );
    assert!((frame.style.fade_unfocused - 0.0).abs() < f32::EPSILON);
    assert_eq!(frame.style.background_tint, Some([0x11, 0x22, 0x33, 0xFF]));
    assert!((frame.style.background_tint_strength - 0.5).abs() < f32::EPSILON);
    assert!(!frame.style.is_inert(), "max style must not read as inert");
}

#[test]
fn default_carry_is_inert() {
    let rt = Runtime::with_defaults().expect("default runtime builds");
    for frame in rt.present_frames() {
        assert_eq!(frame.style, RuntimeViewStyle::default());
        assert!(frame.style.is_inert());
    }
}

#[test]
fn styled_config_paints_while_geometry_ignores_the_carry() {
    // S3 (CTX-1162): the stored style now paints, so the max-style surface
    // diverges in pixels — while frame geometry still ignores the carry
    // (style paint never moves content).
    let mut plain = Runtime::with_defaults().expect("default builds");
    let mut styled = styled_runtime();

    assert_eq!(plain.surface_extent(), styled.surface_extent());
    assert_eq!(plain.present_plan_extent(), styled.present_plan_extent());

    let plain_frames = plain.present_frames();
    let styled_frames = styled.present_frames();
    assert_eq!(plain_frames.len(), styled_frames.len());
    for (a, b) in plain_frames.iter().zip(styled_frames.iter()) {
        assert_eq!(
            frame_geometry(a),
            frame_geometry(b),
            "frame geometry must ignore the style carry"
        );
    }
    assert!(
        styled_frames.iter().any(|f| !f.style.is_inert()),
        "the styled run must actually carry style (else the comparison is vacuous)"
    );

    let a = present_bytes(&mut plain, b"HELLO style S3");
    let b = present_bytes(&mut styled, b"HELLO style S3");
    assert_eq!(a.len(), b.len());
    assert_ne!(
        a, b,
        "max style surface must paint in S3 (defaults still match: see style_present_s3)"
    );
}

#[test]
fn style_rules_do_not_break_view_first_match() {
    // CTX-0343 first-match path stays green under style-only `ws:`/`view:`
    // rules: style has no contrast contract, so a fresh `View` matching only
    // style selectors still commits.
    let rt = styled_runtime();
    rt.validate_new_view_appearance(ViewId::new(1))
        .expect("view 1 with a style rule must pass first-match");
    rt.validate_new_view_appearance(ViewId::new(999))
        .expect("an unmatched fresh view must pass first-match");
}

#[test]
fn present_path_consumes_style_through_the_s3_seam() {
    // S3 (CTX-1162) replaces the S2 no-consumer proof: the production half
    // of the present path must name the chrome-style consumers (the `S3`
    // push/fade helpers), and the layout build must keep populating the
    // per-View carry the consumers read. The `#[cfg(test)]` region is
    // excluded so this test cannot satisfy itself, following the
    // `effective_validate_calls_every_section_validator` precedent.
    let src = include_str!("../src/runtime/present.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
    for token in ["push_leaf_glow", "push_leaf_shadow", "leaf_fade_factor"] {
        assert!(
            prod.contains(token),
            "present path must consume style through the S3 seam: missing {token}"
        );
    }
    // The live adopter lives beside the consumers: the production half of
    // the live-config module must own `set_chrome_style`.
    let live = include_str!("../src/runtime/live_config.rs");
    let live_prod = live.split("#[cfg(test)]").next().unwrap_or(live);
    assert!(
        live_prod.contains("pub fn set_chrome_style"),
        "the S3 live adopter must exist"
    );
    // The population site stays exactly one stored carry in the layout
    // build: the frame construction stores the resolved value.
    let layout = include_str!("../src/runtime/layout_focus.rs");
    let layout_prod = layout.split("#[cfg(test)]").next().unwrap_or(layout);
    assert!(
        layout_prod.contains("style: self.view_style_for(view)"),
        "present build must populate the per-View style carry"
    );
}
