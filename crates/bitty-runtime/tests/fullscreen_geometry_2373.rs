//! Issue #2373: fullscreen (zoom) must reflow the visible pane to the
//! full-bleed frame.
//!
//! Owner report: after repeated gap-drag resizes, making a panel fullscreen
//! left its content/PTY geometry at the old tiled size — btop stayed clipped,
//! the right side never repainted, and closing the panel appeared to hang.
//!
//! Root cause: the zoom lane installed its proxy layout through
//! `set_layout_preserve_grid`, which skipped the primary grid + PTY winsize
//! sync for *every* zoom. Zooming the primary owner therefore painted a small
//! grid inside the full frame; zooming a sibling still needs the owner's grid
//! preserved for the return trip (CTX-0790/#1435).
//!
//! # Differential vs contract coverage
//!
//! These tests drive `Runtime::set_layout` directly — the funnel the zoom
//! lane now uses. On pre-fix `main` that funnel already synced the primary
//! grid, so **these are contract/guard tests, not the differential proof**:
//! they pin the geometry/winsize/round-trip invariants for the layout funnel
//! (including the independent PTY-winsize resync).
//!
//! The genuinely differential regressions live in `bitty-terminal`'s
//! `chrome_keys` tests and fail on pre-fix `main`:
//!
//! - `fullscreen_after_gap_resize_fills_frame_and_close_is_bounded_2373`
//!   (direct `ToggleZoom` API);
//! - `mod_f_primary_fullscreen_resyncs_grid_and_winsize_2373` (the real
//!   Mod+F keybinding dispatch, primary owner);
//! - `mod_f_pane_session_fullscreen_resyncs_grid_and_winsize_2373`
//!   (Mod+F on a split pane running a TUI).
//!
//! Both arms are pinned here: the visible owner follows the full frame, the
//! hidden owner keeps its tiled grid.

use bitty_runtime::{LayoutNode, PresentFrame, Runtime, SplitAxis, View, ViewId};

const OWNER: ViewId = ViewId::new(1);
const SIBLING: ViewId = ViewId::new(2);

fn two_pane(ratio: f32) -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        ratio,
        LayoutNode::leaf(View::new(OWNER, 80, 24)),
        LayoutNode::leaf(View::new(SIBLING, 80, 24)),
    )
}

fn frame_of(rt: &Runtime, id: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have a present frame"))
}

/// Installs the single-leaf zoom proxy for `id` exactly like
/// `ZoomState::engage` / `disengage` do (same funnel).
fn zoom_to(rt: &mut Runtime, id: ViewId) {
    let leaf = rt
        .layout()
        .find_leaf(id)
        .expect("zoom target exists")
        .clone();
    rt.set_layout(LayoutNode::leaf(leaf));
}

/// Fresh single-leaf runtime at the same container: the exact full-bleed
/// frame, content rect, and grid the zoomed owner must match.
fn full_frame_reference() -> PresentFrame {
    let mut rt = Runtime::with_defaults().expect("reference runtime builds");
    let _ = rt.tick().expect("reference full redraw");
    frame_of(&rt, OWNER)
}

#[test]
fn fullscreen_owner_matches_full_frame_after_gap_resizes_2373() {
    let mut rt = Runtime::with_defaults().expect("runtime builds");
    let _ = rt.tick().expect("baseline full redraw");
    let full = full_frame_reference();
    assert!(full.cols > 1 && full.rows > 1, "reference is a real frame");

    // Repeated gap-drag shapes: shrink, widen, then shrink again.
    for ratio in [0.2_f32, 0.75, 0.35] {
        rt.set_layout(two_pane(ratio));
        let _ = rt.tick().expect("tiled reflow presents");
        let tiled = frame_of(&rt, OWNER);
        assert!(
            tiled.cols < full.cols,
            "ratio {ratio}: tiled frame must be narrower than full-bleed"
        );
        assert_eq!(
            (rt.state().width(), rt.state().height()),
            (usize::from(tiled.cols), usize::from(tiled.rows)),
            "ratio {ratio}: tiled primary grid matches the tiled frame"
        );

        // Fullscreen: grid, cell viewport, and presented rect follow the
        // full frame immediately (no PTY bytes needed in between).
        zoom_to(&mut rt, OWNER);
        let zoomed = frame_of(&rt, OWNER);
        assert_eq!(
            (zoomed.frame, zoomed.content),
            (full.frame, full.content),
            "ratio {ratio}: fullscreen frame/content rects must be full-bleed"
        );
        assert_eq!(
            (zoomed.cols, zoomed.rows),
            (full.cols, full.rows),
            "ratio {ratio}: fullscreen presented grid must be the full frame"
        );
        assert_eq!(
            (rt.state().width(), rt.state().height()),
            (usize::from(full.cols), usize::from(full.rows)),
            "ratio {ratio}: fullscreen primary grid must be the full frame"
        );
        let leaf = rt.layout().find_leaf(OWNER).expect("zoomed leaf");
        assert_eq!(
            (leaf.cols(), leaf.rows()),
            (full.cols, full.rows),
            "ratio {ratio}: zoomed leaf cell viewport must equal the full frame"
        );

        // Return to Tiled: the exact pre-fullscreen geometry and grid.
        rt.set_layout(two_pane(ratio));
        let _ = rt.tick().expect("tiled restore presents");
        let restored = frame_of(&rt, OWNER);
        assert_eq!(
            (restored.frame, restored.content),
            (tiled.frame, tiled.content),
            "ratio {ratio}: leaving fullscreen restores the tiled frame"
        );
        assert_eq!(
            (rt.state().width(), rt.state().height()),
            (usize::from(tiled.cols), usize::from(tiled.rows)),
            "ratio {ratio}: leaving fullscreen restores the tiled grid"
        );
    }
}

#[test]
fn fullscreen_hidden_owner_keeps_tiled_grid_2373() {
    // CTX-0790/#1435 guard: zooming a sibling hides the primary owner. Its
    // grid must stay at the tiled size for the return trip (the lossy-reflow
    // case the preserve path was introduced for); only the visible pane
    // follows the full frame.
    let mut rt = Runtime::with_defaults().expect("runtime builds");
    let _ = rt.tick().expect("baseline full redraw");
    rt.set_layout(two_pane(0.2));
    let tiled = frame_of(&rt, OWNER);
    let tiled_state = (rt.state().width(), rt.state().height());
    assert_eq!(
        tiled_state,
        (usize::from(tiled.cols), usize::from(tiled.rows))
    );

    zoom_to(&mut rt, SIBLING);
    let sibling = frame_of(&rt, SIBLING);
    assert!(
        sibling.cols > tiled.cols,
        "visible sibling must fill the full frame"
    );
    assert_eq!(
        (rt.state().width(), rt.state().height()),
        tiled_state,
        "hidden primary owner keeps its tiled grid while a sibling is zoomed"
    );

    rt.set_layout(two_pane(0.2));
    assert_eq!(
        (rt.state().width(), rt.state().height()),
        tiled_state,
        "owner grid is unchanged across the sibling-zoom round trip"
    );
}

#[cfg(unix)]
#[test]
fn fullscreen_owner_pty_winsize_follows_full_frame_2373() {
    let mut rt = Runtime::with_defaults().expect("runtime builds");
    let _ = rt.tick().expect("baseline full redraw");
    rt.spawn_shell("/bin/sh").expect("primary shell spawns");
    let full = full_frame_reference();
    let (cell_w, cell_h) = rt.live_cell_size();
    let pixels = |cols: u16, rows: u16| {
        (
            u16::try_from(u64::from(cols) * u64::from(cell_w)).unwrap_or(u16::MAX),
            u16::try_from(u64::from(rows) * u64::from(cell_h)).unwrap_or(u16::MAX),
        )
    };

    for ratio in [0.2_f32, 0.7] {
        rt.set_layout(two_pane(ratio));
        let tiled = frame_of(&rt, OWNER);
        assert_eq!(
            rt.pty_size(),
            Some((tiled.cols, tiled.rows)),
            "ratio {ratio}: primary winsize tracks the tiled frame"
        );
        let (pixel_w, pixel_h) = pixels(tiled.cols, tiled.rows);
        assert_eq!(
            rt.pty_pixel_size(),
            Some((tiled.cols, tiled.rows, pixel_w, pixel_h)),
            "ratio {ratio}: tiled pixel winsize tracks the frame"
        );

        zoom_to(&mut rt, OWNER);
        assert_eq!(
            rt.pty_size(),
            Some((full.cols, full.rows)),
            "ratio {ratio}: fullscreen must deliver SIGWINCH at the full frame size"
        );
        let (pixel_w, pixel_h) = pixels(full.cols, full.rows);
        assert_eq!(
            rt.pty_pixel_size(),
            Some((full.cols, full.rows, pixel_w, pixel_h)),
            "ratio {ratio}: fullscreen keeps the pixel winsize"
        );
        let _ = rt.tick().expect("fullscreen presents");
        assert_eq!(
            (rt.state().width(), rt.state().height()),
            (usize::from(full.cols), usize::from(full.rows))
        );

        rt.set_layout(two_pane(ratio));
        assert_eq!(
            rt.pty_size(),
            Some((tiled.cols, tiled.rows)),
            "ratio {ratio}: leaving fullscreen restores the tiled winsize"
        );
    }
}

#[cfg(unix)]
#[test]
fn fullscreen_pane_session_pty_and_grid_follow_full_frame_2373() {
    let mut rt = Runtime::with_defaults().expect("runtime builds");
    let _ = rt.tick().expect("baseline full redraw");
    rt.set_layout(two_pane(0.2));
    let tiled = frame_of(&rt, SIBLING);
    rt.spawn_shell_for_view(SIBLING, "/bin/sh", &[], tiled.cols, tiled.rows)
        .expect("pane shell spawns");
    assert_eq!(rt.pane_pty_size(&SIBLING), Some((tiled.cols, tiled.rows)));
    let full = full_frame_reference();

    for _ in 0..3 {
        zoom_to(&mut rt, SIBLING);
        let zoomed = frame_of(&rt, SIBLING);
        assert_eq!((zoomed.cols, zoomed.rows), (full.cols, full.rows));
        assert_eq!(
            rt.pane_pty_size(&SIBLING),
            Some((full.cols, full.rows)),
            "pane winsize follows the fullscreen frame"
        );
        let snapshot = rt.pane_snapshot(&SIBLING).expect("pane grid survives");
        assert_eq!(
            (snapshot.width, snapshot.height),
            (usize::from(full.cols), usize::from(full.rows)),
            "pane grid follows the fullscreen frame"
        );

        rt.set_layout(two_pane(0.2));
        assert_eq!(
            rt.pane_pty_size(&SIBLING),
            Some((tiled.cols, tiled.rows)),
            "pane winsize returns to the tiled frame"
        );
        let snapshot = rt.pane_snapshot(&SIBLING).expect("pane grid survives");
        assert_eq!(
            (snapshot.width, snapshot.height),
            (usize::from(tiled.cols), usize::from(tiled.rows))
        );
    }
}
