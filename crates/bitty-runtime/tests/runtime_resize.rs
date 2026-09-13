//! Runtime Resize, DPI, and platform-event tests.
//!
//! Moved verbatim from the inline `runtime.rs` unit tests as part of
//! the CTX-0232 pure-move split. Adaptations are wiring only:
//! `super::*` became explicit imports and the private `layout` field
//! reads became the public `layout()` getter (identical semantics).
use bitty_platform::{PhysicalSize, PlatformEvent, ScaleFactor, WindowEventKind};
use bitty_runtime::{Runtime, RuntimeConfig};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

#[test]
fn handle_resize_reconfigures_surface_and_keeps_grid_pending_full_redraw() {
    let mut rt = make_runtime();
    let before = rt.surface_extent().expect("surface must have extent");
    // CTX-0223: the surface spans the window (grid + padding inset).
    assert_eq!(before, RuntimeConfig::default().window_extent());
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("valid resize");
    assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
    assert!(rt.tick().is_some(), "resize forces full redraw");
}

#[test]
fn zero_resize_is_skipped_honestly() {
    let mut rt = make_runtime();
    rt.handle_resize(PhysicalSize::new(0, 0))
        .expect("zero resize must not error");
    assert_eq!(
        rt.surface_extent(),
        Some(RuntimeConfig::default().window_extent()),
        "zero resize must not reconfigure"
    );
}

#[test]
fn handle_platform_event_close_semantics() {
    let mut rt = make_runtime();
    let close = rt.handle_platform_event(PlatformEvent::Exiting);
    assert!(close);
    assert!(!rt.handle_platform_event(PlatformEvent::Resumed));
    assert!(!rt.handle_platform_event(PlatformEvent::AboutToWait));
    assert!(!rt.handle_platform_event(PlatformEvent::Suspended));
}

#[test]
fn handle_platform_event_resize_via_handle_resize() {
    let mut rt = make_runtime();
    rt.handle_resize(PhysicalSize::new(320, 240))
        .expect("valid resize");
    assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(320, 240)));
    assert!(rt.tick().is_some(), "resize forces full redraw");
}

#[test]
fn dpi_adoption_derives_grid_from_physical_over_scaled_cells() {
    let mut rt = make_runtime();
    assert_eq!(rt.dpi_scale(), 1.0);
    // Hyprland scale 1.6, tiled physical extent 2506x1496: scaled cells
    // are 14x30 (CTX-0157 readable 9x19 base) and the physical padding
    // is round(8 * 1.6) = 13px per side (CTX-0223). CTX-0375: the primary
    // grid follows the decorated content frame, so the Core-owned
    // decoration (gaps_out 6 + border 2 + content_inset 6 = 14 logical px
    // per side, round(14 * 1.6) = 23 physical px) is removed too:
    // (2506-26-46)/14 x (1496-26-46)/30 = 173x47 (a window-sized 177x49
    // grid would be cropped to the content frame).
    rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(2506, 1496)));
    assert_eq!(rt.dpi_scale(), 1.6);
    let snap = rt.snapshot();
    assert_eq!((snap.width, snap.height), (173, 47));
    assert_eq!(
        rt.surface_extent(),
        Some(PhysicalSize::new(2506, 1496)),
        "surface must be reconfigured to the physical extent"
    );
    assert!(rt.tick().is_some(), "adoption forces full redraw");
}

#[test]
fn dpi_rescale_without_extent_keeps_grid_for_following_resized() {
    let mut rt = make_runtime();
    let before = rt.snapshot();
    rt.apply_dpi_scale(1.6, None);
    assert_eq!(rt.dpi_scale(), 1.6, "renderer rescales even without extent");
    let kept = rt.snapshot();
    assert_eq!(
        (kept.width, kept.height),
        (before.width, before.height),
        "grid waits for the physical extent"
    );
    // The following Resized takes precedence and derives from the same
    // scaled cells (proves Resized-after-scale consistency).
    rt.handle_resize(PhysicalSize::new(2506, 1496))
        .expect("valid resize");
    let snap = rt.snapshot();
    assert_eq!((snap.width, snap.height), (173, 47));
}

#[test]
fn invalid_dpi_scales_are_sanitized_fail_safe() {
    for invalid in [0.0, -1.6, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut rt = make_runtime();
        // Must never panic or strand the window.
        rt.apply_dpi_scale(invalid, Some(PhysicalSize::new(800, 600)));
        assert_eq!(
            rt.dpi_scale(),
            1.0,
            "invalid scale {invalid:?} must sanitize to 1.0"
        );
        let snap = rt.snapshot();
        assert_eq!(
            (snap.width, snap.height),
            (83, 28),
            "unscaled 9x19 cells over 800x600 minus the 8px padding inset \
             and the 14px per-side decoration inset (CTX-0375)"
        );
        assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
        assert!(rt.tick().is_some(), "window stays drawable");
    }
    // Hostile magnitudes clamp instead of exploding geometry.
    let mut rt = make_runtime();
    rt.apply_dpi_scale(100.0, Some(PhysicalSize::new(800, 600)));
    assert_eq!(rt.dpi_scale(), 4.0);
    let snap = rt.snapshot();
    assert!(snap.width >= 1 && snap.height >= 1);
    let mut rt = make_runtime();
    rt.apply_dpi_scale(0.01, Some(PhysicalSize::new(800, 600)));
    assert_eq!(rt.dpi_scale(), 0.25);
}

#[test]
fn zero_extent_dpi_adoption_rescales_but_skips_reflow() {
    let mut rt = make_runtime();
    let surface_before = rt.surface_extent();
    let grid_before = {
        let snap = rt.snapshot();
        (snap.width, snap.height)
    };
    rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(0, 0)));
    assert_eq!(rt.dpi_scale(), 1.6, "renderer still rescales");
    let snap = rt.snapshot();
    assert_eq!(
        (snap.width, snap.height),
        grid_before,
        "zero extent (minimized) must not reflow the grid"
    );
    assert_eq!(rt.surface_extent(), surface_before);
}

#[test]
fn scale_factor_changed_event_adopts_without_exit() {
    let mut rt = make_runtime();
    let exit = rt.handle_platform_event(PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(1),
        kind: WindowEventKind::ScaleFactorChanged(ScaleFactor::new(1.6).expect("valid")),
    });
    assert!(!exit, "scale change must not request exit");
    assert_eq!(rt.dpi_scale(), 1.6);
    assert!(rt.tick().is_some(), "scale change forces full redraw");
    // Invalid factors through the event path sanitize, never panic.
    let exit = rt.handle_platform_event(PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(1),
        kind: WindowEventKind::ScaleFactorChanged(ScaleFactor::new_sanitized(f64::NAN)),
    });
    assert!(!exit);
    assert_eq!(rt.dpi_scale(), 1.0);
}

#[test]
fn logical_recompute_matches_physical_resize() {
    // Embedders holding only cached logical geometry convert via
    // surface_extent_from_logical first: 1566x935 logical at 1.6x must
    // reach the identical grid as the physical Resized path (2506x1496).
    let logical = bitty_platform::LogicalSize::new(1566.0, 935.0).expect("valid");
    let scale = ScaleFactor::new(1.6).expect("valid");
    let physical =
        bitty_platform::surface_extent_from_logical(logical, scale).expect("non-zero extent");
    assert_eq!(physical, PhysicalSize::new(2506, 1496));
    let mut via_logical = make_runtime();
    via_logical.apply_dpi_scale(1.6, Some(physical));
    let mut via_physical = make_runtime();
    via_physical.apply_dpi_scale(1.6, None);
    via_physical.handle_resize(physical).expect("valid resize");
    let a = via_logical.snapshot();
    let b = via_physical.snapshot();
    assert_eq!((a.width, a.height), (173, 47));
    assert_eq!((b.width, b.height), (173, 47));
}

#[test]
fn repeated_rescales_start_from_design_base_without_drift() {
    let mut rt = make_runtime();
    rt.apply_dpi_scale(2.0, Some(PhysicalSize::new(1600, 1200)));
    let scaled = rt.snapshot();
    // 9x19 base at 2x -> 18x38 cells, physical padding round(8 * 2) =
    // 16px per side (CTX-0223) and decoration round(14 * 2) = 28px per
    // side (CTX-0375): (1600-32-56)/18=83, (1200-32-56)/38=28.
    assert_eq!((scaled.width, scaled.height), (83, 28));
    // Back to 1.0 must restore the exact base grid, not a rounded echo.
    // Padding is 8px per side and decoration 14px per side again:
    // (1600-16-28)/9=172, (1200-16-28)/19=60.
    rt.apply_dpi_scale(1.0, Some(PhysicalSize::new(1600, 1200)));
    let restored = rt.snapshot();
    assert_eq!((restored.width, restored.height), (172, 60));
    assert_eq!(rt.dpi_scale(), 1.0);
}

#[test]
fn resized_after_scale_uses_scaled_cells() {
    let mut rt = make_runtime();
    rt.apply_dpi_scale(2.0, None);
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("valid resize");
    let snap = rt.snapshot();
    // 18x38 scaled cells minus the 16px physical padding per side
    // (CTX-0223) and the 28px decoration inset per side (CTX-0375):
    // (800-32-56)/18=38, (600-32-56)/38=12.
    assert_eq!((snap.width, snap.height), (38, 12));
}

#[test]
fn present_plan_extent_matches_draw_list_pixels_so_gpu_scale_stays_near_one() {
    use bitty_render::batch::derive_scale;

    let mut rt = make_runtime();
    rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(2506, 1496)));
    let surface = rt.surface_extent().expect("adopted surface");
    let plan = rt.present_plan_extent();
    // 177x49 grid at 14x30 scaled cells plus the 13px physical padding
    // per side (CTX-0223) = 2504x1496 draw-list pixels.
    assert_eq!((plan.width, plan.height), (177 * 14 + 26, 49 * 30 + 26));
    let scale = derive_scale(surface.width(), surface.height(), plan);
    assert!(
        (scale - 1.0).abs() < 0.05,
        "adopted frame must present near 1.0, got {scale}"
    );
    // The stale 1-cell probe extent this replaced clamped to 4x
    // magnification (the dominant blur behind #232).
    let stale = bitty_render::geometry::ExtentPx::new(14, 30);
    assert_eq!(
        derive_scale(surface.width(), surface.height(), stale),
        4.0,
        "1-cell plan extent magnifies 4x"
    );
}

#[test]
fn present_plan_extent_tracks_scale_one_frames() {
    use bitty_render::batch::derive_scale;

    let rt = make_runtime();
    let surface = rt.surface_extent().expect("default surface");
    let plan = rt.present_plan_extent();
    // Default 80x24 grid at 9x19 readable cells plus the 8px window
    // padding per side (CTX-0223) = 736x472 window pixels.
    assert_eq!((plan.width, plan.height), (736, 472));
    assert_eq!(derive_scale(surface.width(), surface.height(), plan), 1.0);
}
