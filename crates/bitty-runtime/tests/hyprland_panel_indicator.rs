#![forbid(unsafe_code)]
//! CTX-0838 (#1441) retirement leg (W-104/CTX-0956): Hyprland-style panel
//! creation plus the no-Core-bar default.
//!
//! Headless regression (no window, adapter, or display server):
//!
//! - `Runtime::panel_split_axis` follows the focused leaf's cell allocation
//!   via the `smart_split_axis` heuristic: wide splits side-by-side
//!   (`SplitAxis::Horizontal`), tall stacks (`SplitAxis::Vertical`), square
//!   ties break side-by-side — mirroring Hyprland's
//!   `splitTop = height * split_width_multiplier > width` at the default
//!   multiplier `1.0`. Explicit `new_split:<dir>` keeps its fixed axis;
//!   Niri-ribbon ordering is out of scope.
//! - CTX-0979: Core draws no workspace display (Hyprland-style). Workspace
//!   state is memory-only; the bar plugin owns presentation via the query
//!   commands. A lone workspace and multi-workspace both reserve no Core
//!   row; queries (`workspace_names`, `workspace_summaries`) serve the
//!   indicator.

use bitty_runtime::{Runtime, SplitAxis, UiRect};

fn fresh() -> Runtime {
    Runtime::with_defaults().expect("default headless runtime must build")
}

#[test]
fn panel_axis_wide_splits_side_by_side() {
    let rt = fresh();
    let focused = rt.focused_view().expect("live runtime always has focus");
    // Default container is wide (80x24-class): Hyprland dwindle splits
    // side-by-side.
    assert_eq!(
        rt.panel_split_axis(focused),
        SplitAxis::Horizontal,
        "wide focused leaf must split side-by-side"
    );
}

#[test]
fn panel_axis_tall_stacks() {
    let mut rt = fresh();
    rt.set_container(UiRect::new(0, 0, 24, 80));
    let focused = rt.focused_view().expect("live runtime always has focus");
    assert_eq!(
        rt.panel_split_axis(focused),
        SplitAxis::Vertical,
        "tall focused leaf must stack"
    );
}

#[test]
fn panel_axis_square_tie_breaks_side_by_side() {
    let mut rt = fresh();
    // In terminal cell coordinates, a physically square container is cols == rows * 2.0.
    rt.set_container(UiRect::new(0, 0, 80, 40));
    let focused = rt.focused_view().expect("live runtime always has focus");
    assert_eq!(
        rt.panel_split_axis(focused),
        SplitAxis::Horizontal,
        "square tie must break side-by-side (Hyprland splitTop=false)"
    );
}

#[test]
fn panel_axis_unknown_focus_falls_back_to_container() {
    // Fail-closed: an unknown ViewId never panics; it resolves against the
    // container (wide default -> side-by-side).
    let rt = fresh();
    assert_eq!(
        rt.panel_split_axis(bitty_runtime::ViewId::new(999_999)),
        SplitAxis::Horizontal,
        "unknown focus must fall back to the container, not panic"
    );
}

#[test]
fn lone_workspace_queries_serve_the_indicator() {
    let rt = fresh();
    // Queries serve the bar plugin and ctl; Core reserves no row.
    assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(rt.container(), rt.window_cells());
}

#[test]
fn two_workspaces_queries_serve_the_indicator() {
    let mut rt = fresh();
    rt.workspace_new().expect("ws2");
    assert_eq!(
        rt.workspace_names(),
        vec![String::from("ws1"), String::from("ws2")]
    );
    assert_eq!(rt.active_workspace_index(), 1);
    // CTX-0979: no Core bar is reserved even with many workspaces.
    assert_eq!(rt.container(), rt.window_cells());
    // Closing back to one keeps queries consistent.
    assert!(rt.workspace_switch(0));
    rt.workspace_close_index(2).expect("close ws2");
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
}
