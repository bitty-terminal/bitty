#![forbid(unsafe_code)]
//! View-bound search and selection (CTX-0805, issue #1478; policy retired by
//! W-144, CTX-0937).
//!
//! Copy mode and scrollback search used to read the primary grid for motion,
//! matching, and text, and consulted the focused View only for its scroll
//! offset. With a non-primary pane focused, copy mode walked and yanked the
//! primary grid and search matched primary content. Output on any pane
//! refreshed the search against that pane's grid, and a `clear` in any pane
//! dropped a selection owned by another.
//!
//! Pinned here with a real split (primary grid plus a pane session):
//!
//! - `search_set` binds to the focused View, matches its grid only, and is
//!   refreshed only by output on that grid;
//! - a grid erase drops only a selection owned by the erased grid;
//! - the persistent-selection API follows the keyboard View.
//!
//! The overlay plus copy-mode lifecycle tests retired with the Core policy
//! (W-144, CTX-0937); the search/copy-mode plugins re-prove that behavior
//! over the public host ops (CTX-0004).
//!
//! Unix-only: the second grid is a real pane session (mirrors
//! `focused_input_modes.rs`).

#![cfg(unix)]

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};
use bitty_term_state::search::SearchOptions;
use bitty_ui::{CellPos, Selection};

const PRIMARY: ViewId = ViewId::new(1);
const PANE: ViewId = ViewId::new(2);

const PRIMARY_TEXT: &str = "alpha bravo";
const PANE_TEXT: &str = "gamma delta";

fn split_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.force_headless_clipboard();
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == PANE)
        .expect("pane is presented");
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
        .expect("spawn pane shell");
    rt.handle_pty_bytes(PRIMARY_TEXT.as_bytes());
    rt.handle_pane_bytes(PANE, PANE_TEXT.as_bytes());
    rt
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[test]
fn search_set_binds_to_the_focused_view() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));

    rt.search_set("delta", SearchOptions::default());
    assert_eq!(rt.search_view(), Some(PANE));
    assert_eq!(rt.search_match_count(), 1);
    assert!(
        rt.search_case_sensitive("alpha").is_empty(),
        "the stateless search reads the keyboard View's grid"
    );

    rt.search_clear();
    assert_eq!(rt.search_view(), None, "clearing releases the binding");
}

#[test]
fn output_on_another_grid_does_not_refresh_a_bound_search() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));
    rt.search_set("gamma", SearchOptions::default());
    assert_eq!(rt.search_match_count(), 1);

    // Primary output containing the pattern must not leak into the pane's
    // matches (it used to refresh against whichever grid was fed).
    rt.handle_pty_bytes(b"\r\ngamma gamma gamma");
    assert_eq!(
        rt.search_match_count(),
        1,
        "only output on the bound grid refreshes its matches"
    );

    // Output on the bound grid does refresh.
    rt.handle_pane_bytes(PANE, b"\r\ngamma");
    assert_eq!(rt.search_match_count(), 2);
}

// ---------------------------------------------------------------------------
// Grid erase and persistent selection
// ---------------------------------------------------------------------------

#[test]
fn an_erase_drops_only_a_selection_owned_by_the_erased_grid() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    assert!(rt.set_view_selection(PRIMARY, range));

    rt.handle_pane_bytes(PANE, b"\x1b[2J");
    assert_eq!(
        rt.selection_owner(),
        Some(PRIMARY),
        "a clear in another pane keeps this pane's selection"
    );

    rt.handle_pty_bytes(b"\x1b[2J");
    assert_eq!(
        rt.selection_owner(),
        None,
        "a clear on the owner's grid drops it"
    );
}

#[test]
fn persistent_selection_follows_the_keyboard_view() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));
    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    assert!(rt.set_view_selection(PANE, range));

    let pers = rt
        .persistent_selection()
        .expect("a selection on the keyboard View persists");
    assert_eq!(
        rt.persistent_selection_text(&pers).as_deref(),
        Some("gamma")
    );

    rt.clear_selection();
    assert!(rt.restore_persistent_selection(pers));
    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "restore installs into the keyboard View's grid"
    );
    assert_eq!(rt.selection_text().as_deref(), Some("gamma"));
}

#[test]
fn primary_drain_after_a_rehome_onto_a_pane_leaf_leaves_its_bindings_alone() {
    bitty_test_support::require_pty!();
    // Closing the workspace that holds the primary owner re-homes primary
    // ownership onto the loaded workspace's focused leaf, which already runs
    // its own pane session. That leaf reads its pane grid, so output the
    // still-running primary shell writes into the (now unattributed) primary
    // grid must neither erase the leaf's selection nor refresh its search.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.force_headless_clipboard();
    rt.workspace_new().expect("a second workspace");
    let leaf = rt
        .focused_view()
        .expect("the new workspace has a focused leaf");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == leaf)
        .expect("the new leaf is presented");
    rt.spawn_shell_for_view(leaf, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
        .expect("spawn the leaf's pane session");
    rt.handle_pane_bytes(leaf, PANE_TEXT.as_bytes());
    assert!(rt.workspace_switch(0), "back to the primary's workspace");
    let _ = rt.workspace_close_request();
    assert_eq!(
        rt.primary_view(),
        Some(leaf),
        "primary ownership re-homed onto the pane leaf"
    );

    let range = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
    assert!(rt.set_view_selection(leaf, range));
    rt.search_set("gamma", SearchOptions::default());
    assert_eq!(rt.search_match_count(), 1, "the leaf's own text matches");

    rt.handle_pty_bytes(b"\x1b[2J");
    rt.handle_pty_bytes(b"gamma gamma");
    assert_eq!(
        rt.selection_owner(),
        Some(leaf),
        "an erase on the unattributed primary grid keeps the leaf's selection"
    );
    assert_eq!(rt.selection_text().as_deref(), Some("gamma"));
    assert_eq!(
        rt.search_match_count(),
        1,
        "primary output must not refresh the leaf's search against another grid"
    );
}
