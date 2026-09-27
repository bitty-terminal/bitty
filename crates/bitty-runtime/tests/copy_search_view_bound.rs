#![forbid(unsafe_code)]
//! Copy mode and search are View-bound (CTX-0805, issue #1478).
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
//! - copy mode binds to the focused View at entry, walks and yanks that
//!   View's grid, stays bound when focus moves, and ends when its View goes;
//! - the search overlay and `search_set` bind to the focused View, match its
//!   grid only, and are refreshed only by output on that grid;
//! - a grid erase drops only a selection owned by the erased grid;
//! - the persistent-selection API follows the keyboard View.
//!
//! Unix-only: the second grid is a real pane session (mirrors
//! `focused_input_modes.rs`).

#![cfg(unix)]

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, PressState};
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

fn key(ch: &str) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: Some(ch.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

/// Copy-mode keys: line start, visual, then `extend` steps right.
fn visual_from_line_start(rt: &mut Runtime, extend: usize) {
    rt.handle_key_event(key("0"));
    rt.handle_key_event(key("v"));
    for _ in 0..extend {
        rt.handle_key_event(key("l"));
    }
}

// ---------------------------------------------------------------------------
// Copy mode
// ---------------------------------------------------------------------------

#[test]
fn copy_mode_walks_and_yanks_the_focused_pane() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));

    rt.enter_copy_mode();
    assert!(rt.is_copy_mode());
    assert_eq!(
        rt.copy_mode_cursor(),
        Some(CellPos::new(
            0,
            u16::try_from(PANE_TEXT.len()).expect("fits")
        )),
        "the copy cursor starts at the pane's own terminal cursor"
    );
    visual_from_line_start(&mut rt, 4);
    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "the visual is owned by the pane copy mode walks"
    );
    assert_eq!(
        rt.copy_mode_yank().as_deref(),
        Some("gamma"),
        "yank reads the pane's grid, not the primary grid"
    );
    assert!(!rt.is_copy_mode(), "yank exits copy mode");
}

#[test]
fn copy_mode_stays_bound_when_focus_moves() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));
    rt.enter_copy_mode();

    assert!(rt.set_focus(PRIMARY));
    visual_from_line_start(&mut rt, 4);

    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "a focus change mid-session never retargets the copy cursor"
    );
    assert_eq!(rt.copy_mode_yank().as_deref(), Some("gamma"));
}

#[test]
fn copy_mode_ends_when_its_pane_closes() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));
    rt.enter_copy_mode();
    visual_from_line_start(&mut rt, 2);

    assert!(rt.close_pane_session(&PANE));
    rt.set_layout_closing(LayoutNode::leaf(View::new(PRIMARY, 80, 24)), PANE);

    assert!(!rt.is_copy_mode(), "copy mode cannot outlive its grid");
    assert_eq!(rt.selection_owner(), None);
}

#[test]
fn copy_mode_in_the_primary_pane_is_unchanged() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PRIMARY));
    rt.enter_copy_mode();
    visual_from_line_start(&mut rt, 4);
    assert_eq!(rt.selection_owner(), Some(PRIMARY));
    assert_eq!(rt.copy_mode_yank().as_deref(), Some("alpha"));
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[test]
fn search_overlay_matches_the_focused_pane_grid() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime();
    assert!(rt.set_focus(PANE));

    rt.enter_search_mode();
    assert_eq!(rt.search_view(), Some(PANE), "the overlay binds on open");
    rt.search_set_overlay_query("gamma");
    assert_eq!(rt.search_match_count(), 1, "the pane's own text matches");
    assert_eq!(rt.selection_owner(), Some(PANE));
    assert_eq!(rt.selection_text().as_deref(), Some("gamma"));

    rt.search_set_overlay_query("alpha");
    assert_eq!(
        rt.search_match_count(),
        0,
        "primary-grid text is not part of the pane's search"
    );
}

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
