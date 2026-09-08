//! Window corner radius S0 parsed no-op (CTX-0241).
//!
//! Proves headlessly, without a display server or GPU, that
//! `window.radius_px` (physical px, `0..=24`, default 0) is accepted,
//! stored, and reported through the config pipeline with **zero render
//! effect**: a runtime with radius unset (default 0), an explicit 0, and a
//! positive radius all present byte-identical frames from the same input.
//!
//! Later stages (pane radius, window-level rounding) are separate tasks and
//! must not change this file's expectation: S0 has no DrawList/present
//! consumer for the stored value.

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};

fn runtime_with_radius(radius_px: u32) -> Runtime {
    let cfg = RuntimeConfig {
        window_radius_px: radius_px,
        ..RuntimeConfig::default()
    };
    cfg.validate().expect("radius in range must validate");
    Runtime::new(cfg).expect("runtime with radius must build")
}

fn present_bytes(rt: &mut Runtime, input: &[u8]) -> Vec<u8> {
    rt.handle_pty_bytes(input);
    rt.tick().expect("first present must draw");
    rt.headless_rgba().expect("rgba after tick")
}

#[test]
fn default_radius_is_zero_square_noop() {
    let rt = Runtime::with_defaults().expect("default runtime builds");
    assert_eq!(rt.window_radius_px(), 0);
    assert_eq!(rt.config().window_radius_px, 0);
    assert_eq!(
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX,
        0,
        "S0 default is square (zero-cost fast path)"
    );
}

#[test]
fn unset_explicit_zero_and_positive_render_identically() {
    // Same bytes through three runtimes differing only in the stored radius:
    // unset (default 0), explicit 0, and a positive value. All must agree
    // on surface extent, grid geometry, and composited RGBA.
    let mut unset = Runtime::with_defaults().expect("default builds");
    let mut zero = runtime_with_radius(0);
    let mut rounded = runtime_with_radius(12);

    assert_eq!(unset.window_radius_px(), 0);
    assert_eq!(zero.window_radius_px(), 0);
    assert_eq!(rounded.window_radius_px(), 12);

    assert_eq!(unset.surface_extent(), zero.surface_extent());
    assert_eq!(unset.surface_extent(), rounded.surface_extent());
    assert_eq!(unset.present_plan_extent(), zero.present_plan_extent());
    assert_eq!(unset.present_plan_extent(), rounded.present_plan_extent());

    let a = present_bytes(&mut unset, b"HELLO radius S0");
    let b = present_bytes(&mut zero, b"HELLO radius S0");
    let c = present_bytes(&mut rounded, b"HELLO radius S0");
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), c.len());
    assert_eq!(a, b, "explicit 0 must match unset default");
    assert_eq!(a, c, "positive radius must render identically (S0 no-op)");
}

#[test]
fn set_radius_is_live_store_without_geometry_or_repaint_effect() {
    // The live setter stores the value (Live reload class) without touching
    // geometry or forcing work: no resize, no grid change, no mandatory
    // redraw. A tick with no other damage stays idle after the set.
    let mut rt = Runtime::with_defaults().expect("default builds");
    // Drain the creation-time full redraw first so the radius-only change
    // is the sole pending state.
    rt.handle_pty_bytes(b"HELLO");
    rt.tick().expect("first present must draw");
    let extent_before = rt.surface_extent();
    let plan_before = rt.present_plan_extent();
    let (cols_before, rows_before) = (rt.snapshot().width, rt.snapshot().height);

    rt.set_window_radius_px(12)
        .expect("valid radius adopts live");
    assert_eq!(rt.window_radius_px(), 12);
    assert_eq!(rt.surface_extent(), extent_before);
    assert_eq!(rt.present_plan_extent(), plan_before);
    assert_eq!(
        (rt.snapshot().width, rt.snapshot().height),
        (cols_before, rows_before)
    );

    // No damage besides the no-op store: tick stays idle (None), proving the
    // setter forced no full redraw on its own.
    assert!(
        rt.tick().is_none(),
        "radius-only change must not force a present"
    );

    // Out-of-range values fail closed and retain the previous value.
    assert!(rt.set_window_radius_px(25).is_err());
    assert!(rt.set_window_radius_px(u32::MAX).is_err());
    assert_eq!(rt.window_radius_px(), 12);
}
