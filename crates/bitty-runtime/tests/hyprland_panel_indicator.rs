#![forbid(unsafe_code)]
//! CTX-0838 (#1441): Hyprland-style panel creation + single-workspace
//! indicator suppression.
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
//! - The workspace indicator is a single source: `workspaceline_present`
//!   delegates to `status_bar_text`. A lone workspace never presents
//!   (the compositor already shows it); the data path
//!   `workspaceline_text` still renders for ctl/tabline. Multi-workspace
//!   presents, hides with the same opt-out, and reserves no row / blinds
//!   hit-testing when hidden. The full Ghostty-style tabs strip stays in
//!   #1431.

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
fn lone_workspace_hides_the_merged_indicator() {
    let rt = fresh();
    // Data still renders for ctl/tabline ...
    assert_eq!(rt.workspaceline_text(), "1:ws1* (1)");
    // ... but chrome presents nothing: single source, no duplication with
    // the compositor bar.
    assert_eq!(rt.workspaceline_present(), None);
    assert_eq!(rt.status_bar_text(), None);
    assert_eq!(
        rt.workspaceline_present(),
        rt.status_bar_text(),
        "merged single source must agree"
    );
    assert_eq!(rt.status_bar_row(24), None, "no reserved row when hidden");
    assert_eq!(rt.workspaceline_hit_test(0), None, "no bar to click");
}

#[test]
fn two_workspaces_present_the_merged_indicator() {
    let mut rt = fresh();
    rt.workspace_new().expect("ws2");
    let text = rt.workspaceline_text();
    assert_eq!(text, "1:ws1 2:ws2* (2)");
    assert_eq!(
        rt.workspaceline_present().as_deref(),
        Some("1:ws1 2:ws2* (2)")
    );
    assert_eq!(
        rt.status_bar_text().as_deref(),
        Some("1:ws1 2:ws2* (2)"),
        "merged single source must agree"
    );
    assert_eq!(rt.status_bar_row(24), Some(23));
    assert_eq!(rt.workspaceline_hit_test(0), Some(0));
    // Closing back to one hides again.
    assert!(rt.workspace_switch(0));
    rt.workspace_close_index(2).expect("close ws2");
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.workspaceline_present(), None);
    assert_eq!(rt.status_bar_text(), None);
}
