#![forbid(unsafe_code)]
//! View-owned text selection (CTX-0803, DEC-0078 D1, issues #1476/#1433).
//!
//! The runtime used to keep one global selection operated entirely against
//! the primary grid: the pointer mapped through the primary-global
//! `cursor_to_cell` (which subtracts no leaf origin), text came from the
//! primary grid, and the highlight painted at the *focused* View's frame. In
//! a split that leaked a drag across panels (#1433), copied the wrong pane's
//! text, and painted the highlight in the wrong frame (#1476).
//!
//! The accepted model is one live selection owned by exactly one `ViewId`.
//! This file pins that contract end to end with a real split:
//!
//! - a cross-panel drag clamps at the owner's edge and never names a cell of
//!   the other panel, on both axes and in both directions;
//! - `selection_text` comes from the owner's grid for every selection kind;
//! - `Shift`+drag in a non-focused View owns that View without moving focus;
//! - the highlight paints only inside the owner's content frame;
//! - closing the owner, respawning its session, and switching workspace each
//!   clear the selection (fail closed);
//! - a press on the window-padding band still falls back to the focused View.
//!
//! Every pointer position is derived from public geometry
//! ([`Runtime::present_frames`], [`Runtime::live_cell_size`],
//! [`Runtime::window_padding_physical`]) — no hard-coded pixel, padding, or
//! decoration constants.
//!
//! Unix-only: a second real grid needs a POSIX shell plus PTY master
//! semantics (mirrors `focused_input_modes.rs`). The file is still compiled on
//! Windows through the `cfg` gate.

#![cfg(unix)]

use bitty_platform::{
    CursorPosition, ModifiersState, MouseButton, MouseEvent, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{
    LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};
use bitty_term_state::search::SearchOptions;
use bitty_ui::{CellPos, Selection, SelectionKind};

const PRIMARY: ViewId = ViewId::new(1);
const PANE: ViewId = ViewId::new(2);

/// Distinct per-grid content so a wrong-grid read is unmistakable.
const PRIMARY_TEXT: &str = "alpha bravo";
const PANE_TEXT: &str = "gamma delta";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Two-pane runtime split on `axis`, with the primary grid on [`PRIMARY`] and
/// a real shell session on [`PANE`], each carrying its own distinct text.
///
/// The pane session is spawned at its own present-frame dimensions so both
/// grids match their painted frames (the row-window translation is then the
/// identity, which keeps the assertions about *ownership* rather than about
/// scroll windows).
fn split_runtime(axis: SplitAxis) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::split(
        axis,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.force_headless_clipboard();
    assert_eq!(
        rt.primary_view(),
        Some(PRIMARY),
        "the headless runtime pins the primary grid to the first leaf"
    );

    let pane_frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(
        PANE,
        "/bin/sh",
        &["-c", "sleep 30"],
        pane_frame.cols,
        pane_frame.rows,
    )
    .expect("spawn pane shell");

    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    rt.handle_pane_bytes(PANE, PANE_TEXT.as_bytes());
    rt
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

// ---------------------------------------------------------------------------
// Geometry derived from public seams only
// ---------------------------------------------------------------------------

/// Physical cursor position at the centre of frame-local cell `(row, col)` of
/// `view`'s content frame.
///
/// Composed exactly like the present path composes a leaf origin: the window
/// padding inset plus the decorated content rectangle plus whole cells at the
/// live cell size. No literal pixel, padding, gap, or border value appears.
fn cell_center(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + f64::from(col) * f64::from(cw) + f64::from(cw) / 2.0,
        y: pad + f64::from(frame.content.y) + f64::from(row) * f64::from(ch) + f64::from(ch) / 2.0,
    }
}

/// Physical cursor position inside the window-padding band (top-left corner),
/// which belongs to no present frame.
fn padding_band(rt: &Runtime) -> CursorPosition {
    let pad = f64::from(rt.window_padding_physical());
    assert!(pad > 0.0, "the default window padding must be non-zero");
    CursorPosition {
        x: pad / 2.0,
        y: pad / 2.0,
    }
}

// ---------------------------------------------------------------------------
// Input helpers
// ---------------------------------------------------------------------------

fn press(rt: &mut Runtime) {
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Pressed));
}

fn release(rt: &mut Runtime) {
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Released));
}

fn set_shift(rt: &mut Runtime, shift: bool) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ModifiersChanged(ModifiersState {
            shift,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });
}

/// Press at `(from_row, from_col)` of `from`, drag to `(to_row, to_col)` of
/// `to`, and release there.
fn drag_between(rt: &mut Runtime, from: (ViewId, u16, u16), to: (ViewId, u16, u16)) {
    let start = cell_center(rt, from.0, from.1, from.2);
    let end = cell_center(rt, to.0, to.1, to.2);
    rt.handle_cursor_moved(start);
    press(rt);
    rt.handle_cursor_moved(end);
    release(rt);
}

// ---------------------------------------------------------------------------
// Cross-panel drag confinement (#1433)
// ---------------------------------------------------------------------------

#[test]
fn horizontal_drag_from_primary_into_pane_clamps_at_the_primary_edge() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    let owner_frame = frame_of(&rt, PRIMARY);

    drag_between(&mut rt, (PRIMARY, 0, 0), (PANE, 0, 3));

    assert_eq!(
        rt.selection_owner(),
        Some(PRIMARY),
        "the owner is the pane the press landed in"
    );
    let sel = rt.selection().expect("the drag must leave a selection");
    assert_eq!(
        sel.normalized().end.col,
        owner_frame.cols - 1,
        "a drag into the sibling panel clamps at the owner's last column"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(
        text.contains("alpha"),
        "owner text must come from the primary grid, got {text:?}"
    );
    assert!(
        !text.contains("gamma"),
        "no cell of the other panel may be selected, got {text:?}"
    );
}

#[test]
fn horizontal_drag_from_pane_into_primary_clamps_at_the_pane_edge() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);

    drag_between(&mut rt, (PANE, 0, 6), (PRIMARY, 0, 1));

    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "the owner is the pane the press landed in"
    );
    let sel = rt.selection().expect("the drag must leave a selection");
    assert_eq!(
        sel.normalized().start.col,
        0,
        "a leftward drag out of the owner clamps at its first column"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(
        text.contains("gamma"),
        "owner text must come from the pane session grid, got {text:?}"
    );
    assert!(
        !text.contains("alpha"),
        "no cell of the primary grid may be selected, got {text:?}"
    );
}

/// Two-pane runtime with the primary grid on the **second** leaf, so the
/// pane that owns `self.state` does not start at the container origin.
///
/// This is what separates a leaf-origin-aware mapping from the old
/// primary-global one: the pre-CTX-0803 `cursor_to_cell` subtracted no leaf
/// origin, so a press at the right-hand pane's own first cell landed near that
/// grid's *last* column instead of its first.
fn primary_on_the_right() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PANE, 80, 24)),
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
    ));
    rt.force_headless_clipboard();
    let pane_frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(
        PANE,
        "/bin/sh",
        &["-c", "sleep 30"],
        pane_frame.cols,
        pane_frame.rows,
    )
    .expect("spawn pane shell");
    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    rt.handle_pane_bytes(PANE, PANE_TEXT.as_bytes());
    rt
}

#[test]
fn press_in_a_non_origin_pane_anchors_at_that_pane_own_origin() {
    bitty_test_support::require_pty!();
    let mut rt = primary_on_the_right();

    let pos = cell_center(&rt, PRIMARY, 0, 0);
    rt.handle_cursor_moved(pos);
    press(&mut rt);
    rt.handle_cursor_moved(cell_center(&rt, PRIMARY, 0, 4));
    release(&mut rt);

    assert_eq!(rt.selection_owner(), Some(PRIMARY));
    let sel = rt.selection().expect("the drag must leave a selection");
    assert_eq!(
        sel.normalized().start,
        CellPos::new(0, 0),
        "the press maps to the owner's own first cell, not to a global column"
    );
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("alpha"),
        "the owner's own leading word, proving the leaf origin was subtracted"
    );
    // `cursor_to_cell` stays public and primary-global for its other callers
    // (alt-drag chrome, the inspect trace, and the mouse-report fallback for a
    // focused leaf without a frame). It maps this very position far from the
    // owner's first cell, which is what the selection path used to do and no
    // longer does.
    assert_ne!(
        rt.cursor_to_cell(pos),
        CellPos::new(0, 0),
        "the primary-global mapping is unchanged and is no longer used for selection"
    );
}

#[test]
fn drag_from_the_non_origin_primary_into_the_left_pane_stays_in_the_primary() {
    bitty_test_support::require_pty!();
    let mut rt = primary_on_the_right();

    drag_between(&mut rt, (PRIMARY, 0, 4), (PANE, 0, 0));

    assert_eq!(rt.selection_owner(), Some(PRIMARY));
    assert_eq!(
        rt.selection().expect("selection").normalized().start.col,
        0,
        "a leftward drag out of the owner clamps at its own first column"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(
        text.contains("alpha") && !text.contains("gamma"),
        "the left pane's grid must not contribute, got {text:?}"
    );
}

#[test]
fn vertical_drag_clamps_on_the_row_axis_in_both_directions() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Vertical);
    let top = frame_of(&rt, PRIMARY);

    // Downward, out of the top panel: clamps at the owner's last row.
    drag_between(&mut rt, (PRIMARY, 0, 0), (PANE, 2, 0));
    assert_eq!(rt.selection_owner(), Some(PRIMARY));
    assert_eq!(
        rt.selection().expect("selection").normalized().end.row,
        top.rows - 1,
        "a downward drag out of the owner clamps at its last row"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(!text.contains("gamma"), "leaked the lower panel: {text:?}");

    // Upward, out of the bottom panel: clamps at the owner's first row.
    drag_between(&mut rt, (PANE, 1, 6), (PRIMARY, 0, 0));
    assert_eq!(rt.selection_owner(), Some(PANE));
    assert_eq!(
        rt.selection().expect("selection").normalized().start.row,
        0,
        "an upward drag out of the owner clamps at its first row"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(!text.contains("alpha"), "leaked the upper panel: {text:?}");
}

// ---------------------------------------------------------------------------
// Owner-grid text for every selection kind
// ---------------------------------------------------------------------------

#[test]
fn every_selection_kind_reads_the_owner_grid() {
    bitty_test_support::require_pty!();
    for kind in [
        SelectionKind::Simple,
        SelectionKind::Word,
        SelectionKind::Line,
        SelectionKind::Block,
    ] {
        let mut rt = split_runtime(SplitAxis::Horizontal);
        // Cover the first word of the pane's own line for every kind, so a
        // primary-grid read would show up as "alpha" instead of "gamma".
        let range = Selection {
            anchor: CellPos::new(0, 0),
            focus: CellPos::new(0, 4),
            kind,
            active: false,
        };
        assert!(
            rt.set_view_selection(PANE, range),
            "{kind:?}: the pane owns a live grid"
        );
        assert_eq!(rt.selection_owner(), Some(PANE), "{kind:?}");
        let text = rt
            .selection_text()
            .unwrap_or_else(|| panic!("{kind:?}: text"));
        assert!(
            text.contains("gamma"),
            "{kind:?}: text must come from the owner's grid, got {text:?}"
        );
        assert!(
            !text.contains("alpha"),
            "{kind:?}: primary-grid text leaked, got {text:?}"
        );
    }
}

#[test]
fn set_view_selection_refuses_a_view_without_a_grid() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    // `PANE` has no session and does not own the primary grid.
    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    assert!(
        !rt.set_view_selection(PANE, range),
        "a session-less, non-primary leaf owns no grid"
    );
    assert_eq!(rt.selection_owner(), None, "fail closed");
    assert!(rt.selection().is_none());
}

// ---------------------------------------------------------------------------
// Shift+drag in a non-focused View
// ---------------------------------------------------------------------------

#[test]
fn shift_drag_owns_the_hit_view_without_moving_focus() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    assert!(rt.set_focus(PRIMARY));
    set_shift(&mut rt, true);

    drag_between(&mut rt, (PANE, 0, 0), (PANE, 0, 6));

    assert_eq!(
        rt.focused_view(),
        Some(PRIMARY),
        "Shift is the accessibility escape: focus must not move"
    );
    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "the owner is the hit View, not the focused one"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(
        text.contains("gamma") && !text.contains("alpha"),
        "text must come from the hit View's grid, got {text:?}"
    );
}

// ---------------------------------------------------------------------------
// Paint confinement
// ---------------------------------------------------------------------------

/// Physical content rectangle of `view` including the window-padding inset,
/// as `(x, y, width, height)` in surface pixels.
fn content_rect_px(rt: &Runtime, view: ViewId) -> (i64, i64, i64, i64) {
    let frame = frame_of(rt, view);
    let pad = i64::from(rt.window_padding_physical());
    (
        pad + i64::from(frame.content.x),
        pad + i64::from(frame.content.y),
        i64::from(frame.content.width),
        i64::from(frame.content.height),
    )
}

#[test]
fn highlight_paints_only_inside_the_owner_content_frame() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    // A fixed clock keeps both frames comparable (no animation or timed
    // overlay drift between the baseline and the highlighted frame).
    let now = std::time::Instant::now();
    rt.tick_at(now).expect("the first frame presents");
    let baseline = rt.headless_rgba().expect("baseline rgba");

    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    assert!(rt.set_view_selection(PANE, range));
    rt.tick_at(now).expect("the highlight forces a present");
    let painted = rt.headless_rgba().expect("rgba with the highlight");

    assert_eq!(baseline.len(), painted.len(), "surface size is stable");
    let extent = rt.config().window_extent();
    let width = i64::from(extent.width());
    let (ox, oy, ow, oh) = content_rect_px(&rt, PANE);

    let mut inside_changed = 0usize;
    let mut outside_changed = Vec::new();
    for (index, (a, b)) in baseline.iter().zip(painted.iter()).enumerate() {
        if a == b {
            continue;
        }
        let pixel = (index / 4) as i64;
        let x = pixel % width;
        let y = pixel / width;
        if x >= ox && x < ox + ow && y >= oy && y < oy + oh {
            inside_changed += 1;
        } else if outside_changed.len() < 8 {
            outside_changed.push((x, y));
        }
    }
    assert!(
        outside_changed.is_empty(),
        "the highlight escaped the owner's content frame at {outside_changed:?}"
    );
    assert!(
        inside_changed > 0,
        "the highlight must actually paint inside the owner's frame"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle invalidation
// ---------------------------------------------------------------------------

/// Installs a pane-owned selection and asserts it is live.
fn armed_pane_selection(rt: &mut Runtime) {
    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    assert!(rt.set_view_selection(PANE, range));
    assert_eq!(rt.selection_owner(), Some(PANE));
}

#[test]
fn closing_the_owner_clears_the_selection() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    armed_pane_selection(&mut rt);

    assert!(rt.close_pane_session(&PANE), "the pane owned a session");
    rt.set_layout_closing(LayoutNode::leaf(View::new(PRIMARY, 80, 24)), PANE);

    assert_eq!(rt.selection_owner(), None, "a closed owner owns nothing");
    assert!(rt.selection().is_none());
    assert!(rt.selection_text().is_none());
}

#[test]
fn respawning_the_owner_session_clears_the_selection() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    armed_pane_selection(&mut rt);

    let frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
        .expect("respawn pane shell");

    assert_eq!(
        rt.selection_owner(),
        None,
        "a respawn replaced the grid the selection addressed"
    );
    assert!(rt.selection().is_none());
}

#[test]
fn switching_workspace_clears_the_selection() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    armed_pane_selection(&mut rt);

    let created = rt.workspace_new().expect("a second workspace");
    assert_ne!(created, 0, "the new slot is not the first one");
    assert_eq!(
        rt.selection_owner(),
        None,
        "the owner is not a leaf of the newly active layout"
    );
    assert!(rt.selection().is_none());
    // The slot install drops the state, not just hides it: switching back
    // must not resurrect the selection (CodeRabbit review on #1485).
    assert!(rt.workspace_switch(0), "back to the owner's workspace");
    assert_eq!(
        rt.selection_owner(),
        None,
        "a dropped selection stays dropped after switching back"
    );
}

#[test]
fn switching_workspace_ends_search_bound_to_a_hidden_view() {
    // W-144 (CTX-0937): the modal halves of this test retired with the Core
    // policy; the binding-lifecycle half stays, armed through the mechanism
    // (`search_set`) instead of the overlay. The plugins re-prove modal
    // teardown (CTX-0004).
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    assert!(rt.set_focus(PANE));
    rt.search_set("gamma", SearchOptions::default());
    assert_eq!(rt.search_view(), Some(PANE));
    assert_eq!(rt.search_match_count(), 1);

    rt.workspace_new().expect("a second workspace");
    assert_eq!(
        rt.search_view(),
        None,
        "a bound search cannot keep addressing a View the slot hides"
    );
    assert!(
        !rt.search_is_active(),
        "the search ends when its View is hidden"
    );
}

#[test]
fn a_release_without_a_drag_leaves_a_committed_selection_alone() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    drag_between(&mut rt, (PANE, 0, 0), (PANE, 0, 4));
    let committed = rt.selection().expect("a committed selection");
    assert!(!rt.is_selection_dragging());

    // A later release whose press was consumed by chrome (status bar,
    // scrollbar) reaches `end_selection` with no drag in flight.
    rt.end_selection(CellPos::new(0, 9));
    assert_eq!(
        rt.selection(),
        Some(committed),
        "a release without a drag must not move the committed focus"
    );
}

#[test]
fn owner_losing_its_frame_ends_the_drag_and_drops_the_selection() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    let start = cell_center(&rt, PANE, 0, 0);
    rt.handle_cursor_moved(start);
    press(&mut rt);
    assert!(rt.is_selection_dragging(), "the press armed a drag");
    assert_eq!(rt.selection_owner(), Some(PANE));

    // The owner leaves the layout mid-drag.
    rt.set_layout_closing(LayoutNode::leaf(View::new(PRIMARY, 80, 24)), PANE);
    assert!(
        !rt.is_selection_dragging(),
        "the drag must not survive its owner"
    );
    assert_eq!(rt.selection_owner(), None);

    // Further motion is inert and never selects in the survivor.
    let elsewhere = cell_center(&rt, PRIMARY, 0, 4);
    rt.handle_cursor_moved(elsewhere);
    release(&mut rt);
    assert!(rt.selection().is_none(), "no selection is resurrected");
}

// ---------------------------------------------------------------------------
// Padding-press fallback
// ---------------------------------------------------------------------------

#[test]
fn padding_press_falls_back_to_the_focused_view() {
    // Single pane: the historic padding-press behavior must be unchanged.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    let focused = rt.focused_view().expect("a focused leaf");
    assert_eq!(rt.primary_view(), Some(focused));

    let pos = padding_band(&rt);
    assert!(
        rt.cursor_to_present_cell(pos).is_none(),
        "the padding band belongs to no present frame"
    );
    rt.handle_cursor_moved(pos);
    press(&mut rt);

    assert_eq!(
        rt.selection_owner(),
        Some(focused),
        "a press outside every frame falls back to the focused View"
    );
    let sel = rt.selection().expect("the press armed a selection");
    assert_eq!(
        sel.anchor,
        CellPos::new(0, 0),
        "the fallback mapping clamps into the focused grid"
    );

    // A drag out of the window still extends inside the owner and clamps.
    rt.handle_cursor_moved(CursorPosition {
        x: -500.0,
        y: -500.0,
    });
    assert_eq!(rt.selection_owner(), Some(focused));
    release(&mut rt);
}

#[test]
fn padding_press_in_a_split_falls_back_to_the_focused_pane() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    assert!(rt.set_focus(PANE));

    let pos = padding_band(&rt);
    rt.handle_cursor_moved(pos);
    press(&mut rt);
    rt.handle_cursor_moved(cell_center(&rt, PANE, 0, 6));
    release(&mut rt);

    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "the fallback owner is the focused pane, not the primary one"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(
        text.contains("gamma") && !text.contains("alpha"),
        "the fallback reads the focused pane's grid, got {text:?}"
    );
}

// ---------------------------------------------------------------------------
// Keyboard select-all and the persistent form follow the owner
// ---------------------------------------------------------------------------

#[test]
fn select_all_selects_the_focused_pane_grid() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    assert!(rt.set_focus(PANE));

    rt.select_all();

    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "select-all is owned by the focused View"
    );
    let text = rt.selection_text().expect("owner text");
    assert!(
        text.contains("gamma") && !text.contains("alpha"),
        "select-all must cover the focused pane's grid only, got {text:?}"
    );
}

#[test]
fn persistent_selection_skips_a_selection_off_the_keyboard_view() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(SplitAxis::Horizontal);
    assert!(
        rt.set_focus(PRIMARY),
        "the keyboard View is the primary pane"
    );
    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));

    assert!(rt.set_view_selection(PANE, range));
    assert!(
        rt.persistent_selection().is_none(),
        "a selection owned by another View has no persistent form on the \
         keyboard View's grid"
    );

    assert!(rt.set_view_selection(PRIMARY, range));
    let pers = rt
        .persistent_selection()
        .expect("a selection on the keyboard View persists");
    assert_eq!(
        rt.persistent_selection_text(&pers).as_deref(),
        Some("alpha"),
        "the persistent form addresses the keyboard View's grid"
    );
}

// ---------------------------------------------------------------------------
// Overlay hit testing: the visible float owns the press
// ---------------------------------------------------------------------------

#[test]
fn press_on_a_visible_float_selects_in_the_float() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    // Overlay bounds are cells (#1481 aligned the decorated present solver
    // with the cell path).
    rt.set_layout(LayoutNode::overlay(
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 30, 8)),
        UiRect::new(20, 6, 30, 8),
    ));
    rt.force_headless_clipboard();
    let float = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], float.cols, float.rows)
        .expect("spawn float shell");
    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    rt.handle_pane_bytes(PANE, PANE_TEXT.as_bytes());

    let pos = cell_center(&rt, PANE, 0, 0);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(PANE),
        "the hit test resolves the visible float, not the base leaf beneath it"
    );
    assert!(rt.set_focus(PRIMARY), "park focus on the base leaf");
    drag_between(&mut rt, (PANE, 0, 0), (PANE, 0, 4));

    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "click-to-focus agrees with the selection hit test: the visible float, \
         not the base leaf painted beneath it"
    );
    assert_eq!(rt.selection_owner(), Some(PANE));
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("gamma"),
        "the float's own grid supplies the text"
    );
}

#[test]
fn a_float_never_reaches_the_bar_band_and_owns_its_own_rows() {
    bitty_test_support::require_pty!();
    // CTX-0873 (#1431): the bar owns a Core-reserved band outside the layout
    // container, and overlay bounds clip to that container, so a float can
    // never paint over (or steal presses from) the bar. A press on the
    // float's last row, where the old in-grid bar sat, belongs to the float.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    // Install the float layout in workspace zero first: `workspace_new`
    // switches the active slot.
    rt.set_layout(LayoutNode::overlay(
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 40, 14)),
        UiRect::new(10, 10, 40, 14),
    ));
    rt.workspace_new().expect("second workspace for the bar");
    assert!(rt.workspace_switch(0), "checks run on the primary layout");
    rt.force_headless_clipboard();
    let float = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], float.cols, float.rows)
        .expect("spawn float shell");
    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    rt.handle_pane_bytes(PANE, b"\x1b[?1049h");
    rt.handle_pane_bytes(PANE, PANE_TEXT.as_bytes());

    let band = rt
        .status_bar_band()
        .expect("two workspaces reserve the band");
    let (_, ch) = rt.live_cell_size();
    let band_top = u32::from(band.y) * ch;
    for frame in rt.present_frames() {
        let bottom = u32::try_from(frame.frame.y.max(0)).expect("u32") + frame.frame.height;
        assert!(bottom <= band_top, "frame {frame:?} stays above the band");
    }

    let float = frame_of(&rt, PANE);
    let pos = cell_center(&rt, PANE, float.rows - 1, 1);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(PANE),
        "the float's last row is the float's"
    );
    rt.handle_cursor_moved(pos);
    press(&mut rt);
    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "the press reaches the float"
    );
    release(&mut rt);
}

// ---------------------------------------------------------------------------
// Single-pane API semantics are preserved
// ---------------------------------------------------------------------------

#[test]
fn set_selection_keeps_single_pane_semantics_and_names_the_primary_owner() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    rt.set_selection(range);
    assert_eq!(
        rt.selection_owner(),
        rt.primary_view(),
        "the headless seam owns the primary grid"
    );
    assert!(rt.has_selection());
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("alpha"),
        "single-pane text extraction is unchanged"
    );
    rt.clear_selection();
    assert_eq!(rt.selection_owner(), None);
    assert!(!rt.has_selection());
}
