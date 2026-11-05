//! Pointer hit-testing, pane focus, and painted-grid row fit at fractional
//! DPI (CTX-1707 / issue #2896).
//!
//! The owner's HiDPI session reported two symptoms that shared one root:
//! a pointer over the right tiled pane never moved focus, and a full-screen
//! TUI lost its top rows because its PTY was sized larger than the painted
//! grid. The invariant is that grading, painting, and hit-testing address
//! the same physical pixel space and the same decorated content cells:
//!
//! - grid sizing (`Runtime::grid_from_physical`),
//! - frame geometry and the pointer resolver (`present_frames` /
//!   `cursor_to_present_cell`),
//! - PTY winsize (`frame.cols` / `frame.rows`),
//! - the GPU sampling factor (`present_plan_extent` must equal the surface,
//!   so `bitty_render::batch::derive_scale` is exactly 1.0).
//!
//! Coordinates are derived from the runtime's own cell metrics, padding,
//! and frames; no host path, pixel constant, or scale-1.6 special case.

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PhysicalSize, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

/// Base cell/padding used by the owner's profile: a 12pt JetBrains Mono
/// design cell and the default 8 logical px window padding.
fn owner_config() -> RuntimeConfig {
    RuntimeConfig {
        cell_width: 10,
        cell_height: 22,
        window_padding: 8,
        ..RuntimeConfig::default()
    }
}

fn pane(id: u64, cols: u16, rows: u16) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), cols as usize, rows as usize))
}

/// Two tiled panes at the owner's reported split (`69x41 + 86x41`) on a
/// 155x41 container at scale 1.6.
fn two_pane_runtime(config: RuntimeConfig) -> Runtime {
    let mut rt = Runtime::new(config).expect("headless runtime builds");
    // Window physical extent: 155 live columns * 16 px + 2 * 13 px padding.
    let extent = PhysicalSize::new(155 * 16 + 2 * 13, 41 * 35 + 2 * 13);
    rt.apply_dpi_scale(1.6, Some(extent));
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        69.0 / 155.0,
        pane(2, 69, 41),
        pane(4, 86, 41),
    ));
    rt.set_window_cells(bitty_runtime::UiRect::new(0, 0, 155, 41));
    rt.reflow_layout();
    rt
}

fn owner_two_pane_runtime() -> Runtime {
    two_pane_runtime(owner_config())
}

fn frames(rt: &Runtime) -> (PresentFrame, PresentFrame) {
    let mut frames = rt.present_frames();
    assert_eq!(frames.len(), 2, "both panes must present");
    let right = frames.remove(1);
    let left = frames.remove(0);
    (left, right)
}

fn at_content(rt: &Runtime, frame: &PresentFrame, col: u16, row: u16) -> CursorPosition {
    let (cell_w, cell_h) = rt.live_cell_size();
    CursorPosition {
        x: f64::from(rt.window_padding_physical())
            + f64::from(frame.content.x)
            + f64::from(col) * f64::from(cell_w)
            + 0.5 * f64::from(cell_w),
        y: f64::from(rt.window_padding_physical())
            + f64::from(frame.content.y)
            + f64::from(row) * f64::from(cell_h)
            + 0.5 * f64::from(cell_h),
    }
}

fn window_event(kind: WindowEventKind) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind,
    }
}

#[test]
fn two_pane_hit_testing_at_scale_1_6_resolves_both_panes() {
    let mut rt = owner_two_pane_runtime();
    assert_eq!(rt.dpi_scale(), 1.6);
    assert_eq!(rt.live_cell_size(), (16, 35));
    assert_eq!(rt.window_padding_physical(), 13);
    let (left, right) = frames(&rt);
    assert_eq!(left.view, ViewId::new(2));
    assert_eq!(right.view, ViewId::new(4));

    // Clearly inside each pane: the middle of its content grid.
    let left_mid = at_content(&rt, &left, left.cols / 2, left.rows / 2);
    let right_mid = at_content(&rt, &right, right.cols / 2, right.rows / 2);
    assert_eq!(
        rt.cursor_to_present_cell(left_mid).map(|(view, _)| view),
        Some(left.view),
        "left pane centre"
    );
    assert_eq!(
        rt.cursor_to_present_cell(right_mid).map(|(view, _)| view),
        Some(right.view),
        "right pane centre"
    );
    // The last painted cell of the right pane is still the right pane.
    let right_last = at_content(
        &rt,
        &right,
        right.cols.saturating_sub(1),
        right.rows.saturating_sub(1),
    );
    assert_eq!(
        rt.cursor_to_present_cell(right_last).map(|(view, _)| view),
        Some(right.view),
        "right pane last cell"
    );

    // The sibling band between the two frames owns no pane.
    let gap_x = f64::from(rt.window_padding_physical())
        + 0.5 * (f64::from(left.frame.x + left.frame.width as i32) + f64::from(right.frame.x));
    let gap = CursorPosition {
        x: gap_x,
        y: right_mid.y,
    };
    assert_eq!(
        rt.cursor_to_present_cell(gap),
        None,
        "the decoration band between panes owns no leaf"
    );

    // Outside the window (window padding and beyond the surface) maps to none.
    assert_eq!(
        rt.cursor_to_present_cell(CursorPosition { x: 1.0, y: 1.0 }),
        None,
        "window padding is chrome, not a pane"
    );
    assert_eq!(
        rt.cursor_to_present_cell(CursorPosition {
            x: f64::from(rt.surface_extent().expect("surface").width()) + 64.0,
            y: right_mid.y,
        }),
        None,
        "a point outside the surface owns no pane"
    );

    // Hover path: a dwell over the right pane commits focus to it, and a
    // move back over the left pane commits back.
    rt.set_focus(left.view);
    let now = std::time::Instant::now();
    rt.handle_cursor_moved_at(right_mid, now);
    rt.tick_at(now + std::time::Duration::from_millis(200));
    assert_eq!(rt.focused_view(), Some(right.view), "hover right commits");
    rt.handle_cursor_moved_at(left_mid, now + std::time::Duration::from_millis(250));
    rt.tick_at(now + std::time::Duration::from_millis(450));
    assert_eq!(rt.focused_view(), Some(left.view), "hover left commits");
}

#[test]
fn click_focus_moves_to_the_right_pane_at_scale_1_6() {
    // Click-to-focus must work both with hover activation on (the owner's
    // config) and off (the stock click-only mode).
    for follow in [true, false] {
        let mut rt = two_pane_runtime(RuntimeConfig {
            focus_follows_mouse: follow,
            ..owner_config()
        });
        let (left, right) = frames(&rt);
        rt.set_focus(left.view);

        rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(at_content(
            &rt, &right, 4, 0,
        ))));
        rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
            MouseButton::Left,
            PressState::Pressed,
        ))));
        assert_eq!(
            rt.focused_view(),
            Some(right.view),
            "a press on the right pane must focus it (focus_follows_mouse={follow})"
        );
        rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
            MouseButton::Left,
            PressState::Released,
        ))));
    }
}

#[test]
fn present_plan_is_exactly_the_surface_at_fractional_scales() {
    // A window that is not an exact multiple of the cell: the cell-aligned
    // grid stops short of the surface. The plan must still be the surface,
    // otherwise the GPU path treats the leftover strip as a sampling scale.
    let extents = [
        (1.0, PhysicalSize::new(1601, 907)),
        (1.25, PhysicalSize::new(1337, 733)),
        (1.5, PhysicalSize::new(1599, 899)),
        (1.6, PhysicalSize::new(2505, 1496)),
    ];
    for (scale, extent) in extents {
        let mut rt = Runtime::new(owner_config()).expect("runtime builds");
        rt.apply_dpi_scale(scale, Some(extent));
        let surface = rt.surface_extent().expect("surface configured");
        let plan = rt.present_plan_extent();
        assert_eq!(
            (plan.width, plan.height),
            (surface.width(), surface.height()),
            "scale {scale}: plan must be the surface pixel space"
        );
        let sampled = bitty_render::batch::derive_scale(surface.width(), surface.height(), plan);
        assert_eq!(
            sampled, 1.0,
            "scale {scale}: painting must not apply a second cell metric"
        );

        // A scale adopted without a physical extent (the runtime's own
        // `ScaleFactorChanged` path) leaves the grid on the previous cell
        // count; the plan must still be the live surface, never the stale
        // grid, or painting shrinks while hit-testing stays put.
        rt.apply_dpi_scale(scale * 1.25, None);
        let surface = rt.surface_extent().expect("surface still configured");
        let plan = rt.present_plan_extent();
        assert_eq!(
            (plan.width, plan.height),
            (surface.width(), surface.height()),
            "scale {scale}: a stale grid must not become the plan extent"
        );
        let sampled = bitty_render::batch::derive_scale(surface.width(), surface.height(), plan);
        assert_eq!(
            sampled, 1.0,
            "scale {scale}: a stale grid must not shrink the painted pane"
        );
    }
}

#[test]
fn painted_right_pane_resolves_right_after_a_scale_without_extent() {
    // The runtime's own `ScaleFactorChanged` path adopts new cell metrics
    // without a physical extent; the grid/frames stay on the old cell count
    // until the embedder's follow-up or the next `Resized`. During that
    // window, painting must NOT derive a sampling scale from the stale
    // grid: the painted pane and its hit frame have to stay in one space.
    let mut rt = owner_two_pane_runtime();
    rt.apply_dpi_scale(2.0, None);
    let (_, right) = frames(&rt);

    let surface = rt.surface_extent().expect("surface configured");
    let plan = rt.present_plan_extent();
    let sampled = bitty_render::batch::derive_scale(surface.width(), surface.height(), plan);
    assert_eq!(sampled, 1.0, "plan must equal the surface after scale-only");

    // Where the GPU actually paints the right pane's first content column:
    // the draw-list plan is mapped onto the surface by `sampled`, so the
    // painted origin is `sampled * draw origin`. With one shared pixel
    // space that is exactly the frame content origin.
    let painted = CursorPosition {
        x: sampled as f64 * (f64::from(rt.window_padding_physical()) + f64::from(right.content.x))
            + 0.5,
        y: sampled as f64 * (f64::from(rt.window_padding_physical()) + f64::from(right.content.y))
            + 0.5,
    };
    assert_eq!(
        rt.cursor_to_present_cell(painted).map(|(view, _)| view),
        Some(right.view),
        "a pointer at the painted right pane must resolve to the right pane"
    );
}

#[test]
fn off_layout_primary_keeps_its_grid_instead_of_the_container() {
    let mut rt = Runtime::new(owner_config()).expect("runtime builds");
    let extent = PhysicalSize::new(155 * 16 + 2 * 13, 41 * 35 + 2 * 13);
    rt.apply_dpi_scale(1.6, Some(extent));
    let before = (rt.snapshot().width, rt.snapshot().height);
    assert_ne!(
        before,
        (155, 41),
        "the fixture needs a decorated primary grid"
    );
    // Install a layout that does not contain the primary owner (a workspace
    // slot / zoom-away shape): the owner-absence rule keeps its grid instead
    // of resizing it to the undecorated container.
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        pane(2, 69, 41),
        pane(4, 86, 41),
    ));
    rt.handle_resize(extent).expect("resize reflows");
    assert_eq!(
        (rt.snapshot().width, rt.snapshot().height),
        before,
        "an off-layout primary must keep its committed grid, not the container"
    );
}

/// Content rows for the numbered fixture: enough lines to fill the grid
/// exactly, with the last line unterminated so nothing scrolls.
fn numbered_fixture(rows: u16) -> String {
    let mut out = String::new();
    for row in 0..rows {
        if row > 0 {
            out.push_str("\r\n");
        }
        out.push_str(&format!("{:02}", row + 1));
    }
    out
}

fn first_row_text(rt: &Runtime) -> String {
    let snap = rt.snapshot();
    let mut text = String::new();
    for cell in snap.cells.iter().take(snap.width) {
        text.push(cell.glyph);
    }
    text.trim_end().to_string()
}

#[test]
fn rows_fit_the_painted_grid_at_fractional_scales() {
    let extents = [
        (1.0, PhysicalSize::new(1601, 907)),
        (1.25, PhysicalSize::new(1337, 733)),
        (1.5, PhysicalSize::new(1599, 899)),
        (1.6, PhysicalSize::new(2505, 1496)),
    ];
    for (scale, extent) in extents {
        let mut rt = Runtime::new(owner_config()).expect("runtime builds");
        #[cfg(unix)]
        bitty_test_support::require_pty!();
        #[cfg(unix)]
        rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 5"])
            .expect("shell spawns");
        rt.apply_dpi_scale(scale, Some(extent));
        let frame = *rt.present_frames().first().expect("primary frame");
        let surface = rt.surface_extent().expect("surface configured");
        assert_eq!((frame.view, rt.present_frames().len()), (ViewId::new(1), 1));
        // The frame (with its decoration) sits inside the surface.
        assert!(
            frame.content.y >= 0
                && frame.content.y as u32 + frame.content.height <= surface.height(),
            "scale {scale}: content frame must fit the surface height"
        );
        assert!(
            frame.content.x >= 0 && frame.content.x as u32 + frame.content.width <= surface.width(),
            "scale {scale}: content frame must fit the surface width"
        );
        // Maximal fit: one more row would not fit.
        let (cell_w, cell_h) = rt.live_cell_size();
        assert!(
            u32::from(frame.rows) * cell_h <= frame.content.height,
            "scale {scale}: every painted row fits"
        );
        assert!(
            (u32::from(frame.rows) + 1) * cell_h > frame.content.height,
            "scale {scale}: no unused row inside the content frame"
        );
        assert!(
            u32::from(frame.cols) * cell_w <= frame.content.width,
            "scale {scale}: every painted column fits"
        );
        // PTY winsize == painted grid == backing state grid.
        assert_eq!(
            (rt.snapshot().height as u16),
            frame.rows,
            "scale {scale}: state rows must equal the painted rows"
        );
        assert_eq!(
            (rt.snapshot().width as u16),
            frame.cols,
            "scale {scale}: state cols must equal the painted cols"
        );
        #[cfg(unix)]
        assert_eq!(
            rt.pty_size(),
            Some((frame.cols, frame.rows)),
            "scale {scale}: PTY winsize must equal the painted grid"
        );

        // Numbered fixture: the first painted row is grid row 1 (`01`), not
        // a clipped top row from a taller-than-painted PTY grid.
        rt.handle_pty_bytes(numbered_fixture(frame.rows).as_bytes());
        assert_eq!(
            first_row_text(&rt),
            "01",
            "scale {scale}: the first painted row must be the first fixture row"
        );
        let first_painted_row = CursorPosition {
            x: f64::from(rt.window_padding_physical()) + f64::from(frame.content.x) + 1.0,
            y: f64::from(rt.window_padding_physical()) + f64::from(frame.content.y) + 1.0,
        };
        let (view, local) = rt
            .cursor_to_present_cell(first_painted_row)
            .expect("first painted row hits the primary");
        assert_eq!(view, ViewId::new(1));
        assert_eq!(local.row, 0, "scale {scale}: painted row 0 is grid row 0");
    }
}
