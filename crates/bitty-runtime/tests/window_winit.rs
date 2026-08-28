//! Window creation and resize integration via winit-owned events and Runtime.
//!
//! This suite proves the winit 0.30 window lifecycle is wired through
//! `bitty-platform` into `bitty-runtime` without a display server or GPU:
//!
//! - Window creation is modeled as `WindowConfig` -> `LogicalSize` -> `PhysicalSize`
//!   -> `Runtime::handle_resize` / `handle_platform_event(Resized)`.
//! - Resize reconfigures the headless surface extent, container, and layout allocations
//!   deterministically; zero-sized resizes are skipped per `map_resize_to_surface_extent`.
//! - Close semantics (`CloseRequested`/`Closed` and `Exiting`) are owned and return
//!   `true` for loop exit, while `RedrawRequested` and `AboutToWait` do not.
//! - Headless still works: the suite never calls `App::run` or `GpuContext`; it
//!   proves the same byte -> snapshot -> present path that `bitty-app --headless`
//!   uses, and would remain green with `cargo test --features headless`.

#![forbid(unsafe_code)]

use bitty_platform::{LogicalSize, PhysicalSize, PlatformEvent, WindowEventKind, WindowId};
use bitty_runtime::{Runtime, RuntimeConfig};
use bitty_ui::{LayoutNode, Rect as UiRect, SplitAxis, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("default headless runtime must build")
}

#[test]
fn window_creation_config_maps_to_physical_extent_and_runtime_surface() {
    // WindowConfig-like logical size (what winit WindowAttributes would carry)
    // maps to a physical extent via DPI scale, then into Runtime's headless surface.
    let logical = LogicalSize::new(320.0, 240.0).expect("valid");
    let scale = bitty_platform::ScaleFactor::new(1.0).expect("valid");
    let physical = logical.to_physical(scale);
    assert_eq!(physical, PhysicalSize::new(320, 240));

    let mut rt = make_runtime();
    let before = rt.surface_extent().expect("extent");
    assert_eq!(before, RuntimeConfig::default().pixel_extent());

    // Simulate winit Resumed -> create_window success -> runtime surface already exists;
    // the first post-creation step in a real app would be a resize to the window size.
    rt.handle_resize(physical).expect("valid resize");
    assert_eq!(rt.surface_extent(), Some(physical));
}

#[test]
fn resize_via_platform_event_reconfigures_runtime_and_layout() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
    );
    rt.set_layout(split);
    rt.tick().expect("first present");

    // Simulate winit WindowEvent::Resized -> PlatformEvent::Window { Resized }.
    let physical = PhysicalSize::new(800, 600);
    // Direct handle_resize and via handle_platform_event must agree.
    let mut rt2 = make_runtime();
    rt2.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
    ));
    let via_direct = {
        let mut r = make_runtime();
        r.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
            LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
        ));
        r.handle_resize(physical).expect("direct");
        r.layout_allocations()
    };
    let _ = rt2.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::Resized(physical),
    });
    assert_eq!(rt2.surface_extent(), Some(physical));
    assert_eq!(rt2.layout_allocations(), via_direct);

    // Also drive the first runtime via owned event to prove determinism across both paths.
    let _ = rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::Resized(physical),
    });
    assert_eq!(rt.surface_extent(), Some(physical));
    assert_eq!(rt.layout_allocations(), via_direct);
    // View sizes were reflowed to match allocations.
    assert_eq!(rt.layout().find_leaf(ViewId::new(1)).unwrap().cols(), 50);
    // Container recomputed from pixels via RuntimeConfig::grid_from_pixels (8x16 cells).
    assert_eq!(rt.container(), UiRect::new(0, 0, 100, 37));
    assert!(rt.tick().is_some(), "resize forces full redraw");
}

#[test]
fn zero_sized_resize_is_skipped_both_via_direct_and_platform_event() {
    let mut rt = make_runtime();
    let before = rt.surface_extent();
    rt.handle_resize(PhysicalSize::new(0, 0))
        .expect("zero not error");
    assert_eq!(rt.surface_extent(), before);
    let _ = rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::Resized(PhysicalSize::new(0, 480)),
    });
    assert_eq!(rt.surface_extent(), before);
    assert_eq!(
        bitty_platform::map_resize_to_surface_extent(PhysicalSize::new(0, 0)),
        None
    );
}

#[test]
fn close_request_and_closed_ask_loop_to_exit() {
    let mut rt = make_runtime();
    assert!(rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(7),
        kind: WindowEventKind::CloseRequested,
    }));
    assert!(rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(7),
        kind: WindowEventKind::Closed,
    }));
    assert!(rt.handle_platform_event(PlatformEvent::Exiting));
    assert!(!rt.handle_platform_event(PlatformEvent::Resumed));
    assert!(!rt.handle_platform_event(PlatformEvent::AboutToWait));
}

#[test]
fn redraw_requested_is_frame_on_demand_and_does_not_exit() {
    let mut rt = make_runtime();
    rt.tick().expect("first present");
    // RedrawRequested itself does not exit and does not present eagerly;
    // the embedder ticks on AboutToWait / RedrawRequested.
    assert!(!rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(2),
        kind: WindowEventKind::RedrawRequested,
    }));
    // Without new bytes, tick is idle (frame-on-demand).
    assert_eq!(rt.tick(), None);
    // Feed bytes then redraw must present.
    rt.handle_pty_bytes(b"redraw test");
    assert!(!rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(2),
        kind: WindowEventKind::RedrawRequested,
    }));
    assert!(rt.tick().is_some());
}

#[test]
fn scale_factor_changed_alone_does_not_resize_but_following_resize_does() {
    let mut rt = make_runtime();
    let before = rt.surface_extent();
    // ScaleFactorChanged alone is a DPI hint; runtime waits for Resized.
    assert!(!rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ScaleFactorChanged(
            bitty_platform::ScaleFactor::new(2.0).expect("valid"),
        ),
    }));
    assert_eq!(rt.surface_extent(), before);
    // Following resize takes precedence.
    let physical = PhysicalSize::new(640, 480);
    assert!(!rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::Resized(physical),
    }));
    assert_eq!(rt.surface_extent(), Some(physical));
}

#[test]
fn winit_window_creation_headless_still_ticks_deterministically() {
    // Proves headless still works when no winit window exists (CI path).
    // Two runtimes with identical layout + bytes must produce identical RGBA
    // even without ever calling App::run.
    let payload = b"headless winit fallback still ticks";
    let mut a = make_runtime();
    let mut b = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    a.set_layout(split.clone());
    b.set_layout(split);
    a.handle_pty_bytes(payload);
    b.handle_pty_bytes(payload);
    let stats_a = a.tick().expect("present");
    let stats_b = b.tick().expect("present");
    assert_eq!(stats_a.generation, stats_b.generation);
    assert_eq!(a.headless_rgba(), b.headless_rgba());
}
