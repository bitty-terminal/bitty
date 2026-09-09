//! Per-window font zoom (CTX-0263): Ctrl+Plus/Minus/0 adjust live size.
//!
//! Proves headlessly, without a display server or GPU:
//!
//! - Zoom means per-window live `font_size` (not pane `toggle_zoom`, not a
//!   config-file write, not DPI): `zoom_in`/`zoom_out` step `1pt`,
//!   `reset_zoom` restores the startup value, `set_font_size` is the
//!   fail-closed seam (`[6.0, 32.0]`, non-finite rejected, no mutation).
//! - Zoom survives reflow/resize: a resize re-derives the grid from the
//!   zoomed size (font size itself is untouched), and a later DPI adoption
//!   keeps the zoomed base (renderer rescales from the new base).
//! - Zoom is per-window: two headless runtimes diverge independently.

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("default headless runtime must build")
}

#[test]
fn zoom_starts_at_startup_size() {
    let rt = make_runtime();
    assert!((rt.font_size() - 12.0).abs() < f32::EPSILON);
    assert!((rt.base_font_size() - 12.0).abs() < f32::EPSILON);
    assert!((rt.config().font_size - 12.0).abs() < f32::EPSILON);
}

#[test]
fn zoom_steps_and_resets() {
    let mut rt = make_runtime();
    rt.zoom_in().expect("zoom in from 12");
    assert!((rt.font_size() - 13.0).abs() < f32::EPSILON);
    rt.zoom_in().expect("zoom in again");
    assert!((rt.font_size() - 14.0).abs() < f32::EPSILON);
    rt.zoom_out().expect("zoom out");
    assert!((rt.font_size() - 13.0).abs() < f32::EPSILON);
    rt.reset_zoom();
    assert!((rt.font_size() - 12.0).abs() < f32::EPSILON);
}

#[test]
fn zoom_bounds_fail_closed_without_mutation() {
    let mut rt = make_runtime();
    // Drive to the top bound exactly: 12 -> 32 is 20 steps.
    for _ in 0..20 {
        rt.zoom_in().expect("step to max");
    }
    assert!((rt.font_size() - 32.0).abs() < f32::EPSILON);
    let err = rt.zoom_in().unwrap_err();
    assert!(err.to_string().contains("maximum"));
    assert!((rt.font_size() - 32.0).abs() < f32::EPSILON);
    // Drive to the bottom bound exactly: 32 -> 6 is 26 steps.
    for _ in 0..26 {
        rt.zoom_out().expect("step to min");
    }
    assert!((rt.font_size() - 6.0).abs() < f32::EPSILON);
    let err = rt.zoom_out().unwrap_err();
    assert!(err.to_string().contains("minimum"));
    assert!((rt.font_size() - 6.0).abs() < f32::EPSILON);
    // Direct sets outside the window fail closed with no mutation.
    for bad in [f32::NAN, f32::INFINITY, 0.0, 5.9, 32.1, 3999.0] {
        let before = rt.font_size();
        assert!(rt.set_font_size(bad).is_err(), "must reject {bad}");
        assert!((rt.font_size() - before).abs() < f32::EPSILON);
    }
    // Reset still restores the startup size after bound excursions.
    rt.reset_zoom();
    assert!((rt.font_size() - 12.0).abs() < f32::EPSILON);
}

#[test]
fn zoom_survives_reflow_and_resize() {
    let mut rt = make_runtime();
    rt.zoom_in().expect("12 -> 13");
    rt.zoom_in().expect("13 -> 14");
    assert!((rt.font_size() - 14.0).abs() < f32::EPSILON);
    // A live resize re-derives the grid but never touches the zoomed size.
    let extent = rt.surface_extent().expect("headless extent");
    rt.handle_resize(extent).expect("resize at same extent");
    assert!((rt.font_size() - 14.0).abs() < f32::EPSILON);
    // DPI adoption rescales the renderer from the zoomed base and keeps it.
    rt.apply_dpi_scale(1.6, None);
    assert!((rt.font_size() - 14.0).abs() < f32::EPSILON);
    assert!((rt.dpi_scale() - 1.6).abs() < 1e-9);
    rt.apply_dpi_scale(1.0, None);
    assert!((rt.font_size() - 14.0).abs() < f32::EPSILON);
    // And zoom still steps after DPI round-trips.
    rt.zoom_out().expect("14 -> 13 after DPI");
    assert!((rt.font_size() - 13.0).abs() < f32::EPSILON);
}

#[test]
fn zoom_is_per_window_not_global() {
    let mut a = make_runtime();
    let b = make_runtime();
    a.zoom_in().expect("a zooms");
    a.zoom_in().expect("a zooms again");
    assert!((a.font_size() - 14.0).abs() < f32::EPSILON);
    assert!((b.font_size() - 12.0).abs() < f32::EPSILON);
    assert!((RuntimeConfig::default().font_size - 12.0).abs() < f32::EPSILON);
    a.reset_zoom();
    assert!((a.font_size() - 12.0).abs() < f32::EPSILON);
    assert!((b.font_size() - 12.0).abs() < f32::EPSILON);
}
