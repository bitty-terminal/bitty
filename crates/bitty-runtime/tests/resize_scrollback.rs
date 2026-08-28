//! Resize and scrollback for the runtime: state resize via `handle_resize` and
//! headless view composition. Proves the deferred reflow is now live and the
//! runtime remains headless after geometry changes.

#![forbid(unsafe_code)]

use bitty_platform::PhysicalSize;
use bitty_runtime::Runtime;
use bitty_ui::ViewId;

#[test]
fn runtime_handle_resize_actually_resizes_state_and_is_headless() {
    let mut rt = Runtime::with_defaults().expect("defaults");
    assert_eq!(rt.snapshot().width, 80);
    assert_eq!(rt.snapshot().height, 24);
    let gen_before = rt.snapshot().generation;

    // Resize physics: 800x600 at 8x16 cell => 100x37.
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("resize");
    assert_eq!(rt.snapshot().width, 100);
    assert_eq!(rt.snapshot().height, 37);
    assert_eq!(rt.container(), bitty_ui::Rect::new(0, 0, 100, 37));
    assert!(
        rt.snapshot().generation > gen_before,
        "resize must bump generation"
    );
    // State invariants held (implicitly via resize check), but also runtime tick still headless.
    let stats = rt.tick().expect("tick after resize must present");
    assert!(stats.headless);
    assert!(stats.fills > 0);
    assert!(rt.headless_rgba().is_some());

    // Second resize deterministically identical to first on a fresh runtime.
    let mut rt2 = Runtime::with_defaults().unwrap();
    rt2.handle_resize(PhysicalSize::new(800, 600)).unwrap();
    assert_eq!(rt.snapshot().width, rt2.snapshot().width);
    assert_eq!(rt.snapshot().height, rt2.snapshot().height);

    // Zero resize is still honest no-op.
    let extent_before = rt.surface_extent();
    rt.handle_resize(PhysicalSize::new(0, 0))
        .expect("zero no-op");
    assert_eq!(rt.surface_extent(), extent_before);
    assert_eq!(rt.snapshot().width, 100);
}

#[test]
fn runtime_resize_updates_scrollback_width_and_views() {
    let mut rt = Runtime::with_defaults().expect("defaults");
    // Create scrollback: feed 30 lines causing scroll.
    for i in 0..30 {
        let line = format!("line{i:02}\r\n");
        rt.handle_pty_bytes(line.as_bytes());
    }
    let sb_before = rt.state().scrollback_len();
    assert!(sb_before > 0);

    // Resize wider: scrollback lines must be padded.
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("resize wide");
    assert_eq!(rt.snapshot().width, 100);
    for line in rt.state().scrollback() {
        assert_eq!(line.cells.len(), 100);
    }
    // Narrower: truncate with repair.
    rt.handle_resize(PhysicalSize::new(320, 240))
        .expect("resize narrow");
    // 320/8=40 cols, 240/16=15 rows
    assert_eq!(rt.snapshot().width, 40);
    assert_eq!(rt.snapshot().height, 15);
    for line in rt.state().scrollback() {
        assert_eq!(line.cells.len(), 40);
    }
    assert!(rt.state().check_invariants().is_ok());
    // Headless tick still works after multiple resizes.
    assert!(rt.tick().is_some());
}

#[test]
fn runtime_view_scroll_offset_survives_resize_headlessly() {
    let mut rt = Runtime::with_defaults().unwrap();
    for i in 0..40 {
        rt.handle_pty_bytes(format!("L{i:02}\r\n").as_bytes());
    }
    let sb_len = rt.state().scrollback_len();
    assert!(sb_len >= 10);

    // Use layout with single leaf view sized to container.
    // View id 1 is default leaf.
    let view_id = ViewId::new(1);
    {
        let view = rt.layout_mut().find_leaf_mut(view_id).expect("view 1");
        view.set_scroll_offset(5, sb_len);
        assert_eq!(view.scroll_offset(), 5);
    }

    // Resize and ensure view was clamped and still composes deterministically.
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("resize");
    let after_view = rt.layout().find_leaf(view_id).unwrap();
    assert!(after_view.scroll_offset() <= rt.state().scrollback_len());
    let after_cells = after_view.visible_cells(rt.state());
    assert_eq!(
        after_cells.len(),
        (after_view.cols() as usize) * (after_view.rows() as usize)
    );
    // Not strictly equal because width changed, but deterministic across replay.
    let mut rt2 = Runtime::with_defaults().unwrap();
    for i in 0..40 {
        rt2.handle_pty_bytes(format!("L{i:02}\r\n").as_bytes());
    }
    rt2.handle_resize(PhysicalSize::new(800, 600)).unwrap();
    let v2 = rt2.layout().find_leaf(view_id).unwrap();
    assert_eq!(after_view.cols(), v2.cols());
    assert_eq!(after_view.rows(), v2.rows());

    // Headless present after scroll+resize still works.
    assert!(rt.tick().is_some());
    assert!(rt.headless_rgba().is_some());
}

#[test]
fn headless_still_works_after_multiple_resizes_and_prints() {
    let mut rt = Runtime::with_defaults().unwrap();
    rt.tick().expect("first present headless must work");
    for size in [
        PhysicalSize::new(640, 480),
        PhysicalSize::new(1024, 768),
        PhysicalSize::new(80, 24), // small physical yields fallback cols=10, rows=1? Actually 80/8=10, 24/16=1 => 10x1
        PhysicalSize::new(800, 600),
    ] {
        rt.handle_resize(size).expect("resize");
        rt.handle_pty_bytes(b"hello after resize\r\n");
        let snap = rt.snapshot();
        assert_eq!(snap.width, rt.container().width as usize);
        assert_eq!(snap.height, rt.container().height as usize);
        assert!(rt.state().check_invariants().is_ok());
        let _ = rt.tick();
        assert!(
            rt.headless_rgba().is_some(),
            "headless rgba must exist after resize"
        );
    }
}
