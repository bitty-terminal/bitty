//! Resize and scrollback for the runtime: state resize via `handle_resize` and
//! headless view composition. Proves the deferred reflow is now live and the
//! runtime remains headless after geometry changes.

#![forbid(unsafe_code)]

use bitty_platform::PhysicalSize;
use bitty_runtime::Runtime;
use bitty_ui::ViewId;

#[cfg(unix)]
use bitty_runtime::{LayoutNode, SplitAxis, View};

#[test]
fn runtime_handle_resize_actually_resizes_state_and_is_headless() {
    let mut rt = Runtime::with_defaults().expect("defaults");
    assert_eq!(rt.snapshot().width, 80);
    assert_eq!(rt.snapshot().height, 24);
    let gen_before = rt.snapshot().generation;

    // Resize physics: 800x600 at 9x19 readable cell minus the default 8px
    // window padding inset (CTX-0223) and the 13px per-side decoration
    // inset (CTX-0375) => 84x28. The layout container keeps the window cell
    // count (87x30) so the decoration lands inside the Window.
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("resize");
    assert_eq!(rt.snapshot().width, 84);
    assert_eq!(rt.snapshot().height, 28);
    assert_eq!(rt.container(), bitty_ui::Rect::new(0, 0, 87, 30));
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
    assert_eq!(rt.snapshot().width, 84);
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

    // Resize wider: scrollback lines must be padded (CTX-0223/CTX-0375:
    // 800x600 minus the 8px padding and 13px decoration insets => 84 cols).
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("resize wide");
    assert_eq!(rt.snapshot().width, 84);
    for line in rt.state().scrollback() {
        assert_eq!(line.cells.len(), 84);
    }
    // Narrower: truncate with repair.
    rt.handle_resize(PhysicalSize::new(320, 240))
        .expect("resize narrow");
    // (320-16-26)/9=30 cols, rows stay 9.
    assert_eq!(rt.snapshot().width, 30);
    assert_eq!(rt.snapshot().height, 9);
    for line in rt.state().scrollback() {
        assert_eq!(line.cells.len(), 30);
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
        // CTX-0603: `layout_mut` returns a guard by value, so bind it
        // before borrowing a leaf through it (a chained temporary would
        // drop while `view` is still borrowed).
        let mut borrowed = rt.layout_mut();
        let view = borrowed.find_leaf_mut(view_id).expect("view 1");
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
        PhysicalSize::new(80, 24), // small physical yields 80/9=8, 24/19=1 => 8x1
        PhysicalSize::new(800, 600),
    ] {
        rt.handle_resize(size).expect("resize");
        rt.handle_pty_bytes(b"hello after resize\r\n");
        let snap = rt.snapshot();
        // CTX-0375: the grid follows the primary owner leaf's decorated
        // content frame, not the window cell container.
        let frame = rt.present_frames();
        let primary = frame
            .iter()
            .find(|f| Some(f.view) == rt.primary_view())
            .or_else(|| frame.first())
            .expect("single-leaf primary frame");
        assert_eq!(snap.width, usize::from(primary.cols));
        assert_eq!(snap.height, usize::from(primary.rows));
        assert!(rt.state().check_invariants().is_ok());
        let _ = rt.tick();
        assert!(
            rt.headless_rgba().is_some(),
            "headless rgba must exist after resize"
        );
    }
}

/// #2529 multi-panel repro: dragging a horizontal divider changes a pane's
/// height (and nothing else). The pane's grid must keep its logical lines and
/// round-trip them through scrollback instead of collapsing/squashing them.
#[cfg(unix)]
#[test]
fn two_pane_height_resize_roundtrips_pane_content() {
    const SHELL: &str = "/bin/sh";
    fn two_pane_vertical(ratio: f32) -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Vertical,
            ratio,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        )
    }
    fn pane_rows(rt: &Runtime) -> Vec<String> {
        let snap = rt.pane_snapshot(&ViewId::new(2)).expect("pane 2 frame");
        (0..snap.height)
            .map(|row| {
                let start = row * snap.width;
                snap.cells[start..start + snap.width]
                    .iter()
                    .map(|cell| if cell.spacer { ' ' } else { cell.glyph })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    let mut rt = Runtime::with_defaults().expect("defaults");
    rt.set_layout(two_pane_vertical(0.5));
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("bottom pane frame");
    rt.spawn_shell_for_view(
        ViewId::new(2),
        SHELL,
        &["-c", "exec sleep 30"],
        frame.cols,
        frame.rows,
    )
    .expect("spawn pane shell");

    // Short `ls`-style rows plus a long Markdown paragraph that soft-wraps.
    for i in 0..4 {
        rt.handle_pane_bytes(
            ViewId::new(2),
            format!("file-{i:02}.md  README.md\r\n").as_bytes(),
        );
    }
    rt.handle_pane_bytes(
        ViewId::new(2),
        b"# Heading\r\n\r\nA paragraph that is long enough to wrap across more than one physical row in this pane.\r\n",
    );

    let before = pane_rows(&rt);
    let joined = before.join("\n");
    assert!(joined.contains("file-03.md"), "fixture content: {joined:?}");
    assert!(
        joined.contains("more than one physical row"),
        "fixture must show the wrapped paragraph: {joined:?}"
    );

    // Shrink the bottom pane hard (divider drag), then restore it. At one
    // row the visible row can be the blank cursor row; the contract is that
    // nothing is lost and the round-trip restores the grid.
    rt.set_layout(two_pane_vertical(0.9));
    let _ = pane_rows(&rt);
    rt.set_layout(two_pane_vertical(0.5));

    let restored = pane_rows(&rt);
    let content = |rows: &[String]| -> Vec<String> {
        rows.iter()
            .filter(|row| !row.trim().is_empty())
            .cloned()
            .collect()
    };
    assert_eq!(
        content(&restored),
        content(&before),
        "height round-trip must restore every pane line without merging or loss"
    );
    assert!(rt.state().check_invariants().is_ok());
}
