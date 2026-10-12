//! Chrome-style S2 carry (CTX-1161).
//!
//! Proves headlessly, without a display server or GPU, that
//! `resolve_view_style` plus the [`RuntimeViewStyle`] carry is
//! accepted-but-inert: values resolve per `View` (global base plus per-field
//! per-tier `views` overrides), are stored on every [`PresentFrame`] at
//! present build, and have zero render effect until S3.
//!
//! Later stages (glow/shadow/tint/fade paint in the present path plus the
//! `set_chrome_style` adopter) are separate tasks and must not change this
//! file's inertness expectation: S2 adds no DrawList/present consumer for
//! the stored value.

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
fn styled_and_unstyled_configs_render_identically() {
    // Same bytes through two runtimes differing only in the stored style
    // surface (max globals plus a per-View override): surface extent, frame
    // geometry, and composited RGBA must all agree, proving the carry is
    // stored but unread by render.
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

    let a = present_bytes(&mut plain, b"HELLO style S2");
    let b = present_bytes(&mut styled, b"HELLO style S2");
    assert_eq!(a.len(), b.len());
    assert_eq!(
        a, b,
        "max style surface must render byte-identically (S2 no-op)"
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
fn present_path_has_no_style_consumer() {
    // Grep-level proof that S2 stores but never reads: the production half
    // of the present path (before any `#[cfg(test)]` helper) must not name
    // any chrome-style resolver, value type, or leaf. The `#[cfg(test)]`
    // region is excluded so this test cannot satisfy itself, following the
    // `effective_validate_calls_every_section_validator` precedent.
    let src = include_str!("../src/runtime/present.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
    for token in [
        "view_style_for",
        "resolve_view_style",
        "RuntimeViewStyle",
        "style_glow",
        "style_shadow",
        "style_fade",
        "fade_unfocused",
        "background_tint",
        "glow_color",
        "glow_width",
    ] {
        assert!(
            !prod.contains(token),
            "present path must not consume style: found {token}"
        );
    }
    // The population site stays exactly one stored (not read) carry in the
    // layout build: the frame construction stores the resolved value.
    let layout = include_str!("../src/runtime/layout_focus.rs");
    let layout_prod = layout.split("#[cfg(test)]").next().unwrap_or(layout);
    assert!(
        layout_prod.contains("style: self.view_style_for(view)"),
        "present build must populate the per-View style carry"
    );
}
