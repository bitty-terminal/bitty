#![forbid(unsafe_code)]
//! OQ-084 B-2: the presented workspace resolves to a real `WorkspaceId`.
//!
//! The decision's create path reads "the active workspace index and that
//! workspace's focused view" and passes "the resulting `WorkspaceId`" to
//! `create_panel`. Until this seam existed there was no id to pass: `Runtime`
//! held an index, `WorkspaceSlot` held a positional `seq`, and every production
//! `create_panel` site passed `None`. These tests hold the seam to the three
//! properties the decision needs:
//!
//! 1. the presented workspace answers a stable `WorkspaceId` that survives
//!    index shifts and `seq` reuse,
//! 2. a presented workspace with no focused leaf answers `None`, which is the
//!    `E_UI_UNAVAILABLE` branch rather than a fallback pointer,
//! 3. the answered id is the panel registry's bucket key — one bucket, not two.

use bitty_runtime::{
    Runtime, RuntimeConfig,
    registry::{PanelRegistry, PanelRegistryConfig, PanelType, WorkspaceId},
};
use bitty_ui::ViewId;

fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless runtime builds")
}

fn panels() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds")
}

#[test]
fn focused_workspace_id_resolves_the_presented_workspace() {
    let rt = runtime();
    let resolved = rt
        .focused_workspace_id()
        .expect("a fresh runtime presents one focused leaf");
    assert_eq!(resolved, rt.active_workspace_id());
}

#[test]
fn every_workspace_slot_mints_a_distinct_id() {
    let mut rt = runtime();
    let first = rt.active_workspace_id();
    rt.workspace_new().expect("second workspace");
    let second = rt.active_workspace_id();
    assert_ne!(first, second);
    rt.workspace_new().expect("third workspace");
    let third = rt.active_workspace_id();
    assert_ne!(third, first);
    assert_ne!(third, second);
}

#[test]
fn a_slot_keeps_its_id_across_index_shifts_and_switching() {
    let mut rt = runtime();
    let first = rt.active_workspace_id();
    rt.workspace_new().expect("second workspace");
    let second = rt.active_workspace_id();
    assert_eq!(rt.active_workspace_index(), 1);

    // Switching reorders nothing but must not rename either slot.
    assert!(rt.workspace_switch(0));
    assert_eq!(rt.active_workspace_id(), first);
    assert!(rt.workspace_switch(1));
    assert_eq!(rt.active_workspace_id(), second);

    // Closing the first slot shifts the survivor's index down; its id is
    // unaffected because identity is not positional.
    rt.workspace_close_at(0).expect("close the first workspace");
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(
        rt.active_workspace_id(),
        second,
        "the survivor keeps the id it was minted"
    );

    // A closed slot's id is never reissued to a later workspace.
    rt.workspace_new().expect("replacement workspace");
    let replacement = rt.active_workspace_id();
    assert_ne!(replacement, first);
    assert_ne!(replacement, second);
}

#[test]
fn no_focused_leaf_answers_none() {
    let mut rt = runtime();
    let attributed = rt.active_workspace_id();
    rt.focus_mut().clear();
    assert_eq!(
        rt.focused_workspace_id(),
        None,
        "an unfocused presented workspace has no workspace to attribute a create to"
    );
    assert_eq!(
        rt.active_workspace_id(),
        attributed,
        "the slot itself still answers; only the create-path resolution fails closed"
    );
}

#[test]
fn a_focused_id_outside_the_presented_layout_answers_none() {
    let mut rt = runtime();
    // A focus id that no presented leaf carries is drift, not attribution:
    // resolving it would hand the create a workspace the leaf does not occupy.
    rt.focus_mut().set(ViewId::new(u64::MAX));
    assert_eq!(rt.focused_workspace_id(), None);
}

#[test]
fn the_resolved_id_is_the_panel_registry_bucket_key() {
    let rt = runtime();
    let workspace = rt
        .focused_workspace_id()
        .expect("focused workspace resolves");
    let mut panels = panels();
    let handle = panels
        .create_panel(PanelType::Helper, Some(workspace))
        .expect("create attributed to the presented workspace");
    panels
        .mount_panel(handle.id, handle.generation, ViewId::new(1))
        .expect("mount");
    panels
        .focus_panel(handle.id, handle.generation, workspace)
        .expect("focus within the same workspace bucket");
    assert_eq!(panels.focused_panel(workspace), Some(handle.id));

    // A guessed id is an empty bucket: the registry keys membership by the
    // resolved id, never by the positional index the id was derived from.
    let guessed = WorkspaceId::new(workspace.get() + 1_000);
    assert_eq!(panels.focused_panel(guessed), None);
    assert!(
        panels
            .focus_panel(handle.id, handle.generation, guessed)
            .is_err(),
        "a panel attributed to one workspace cannot be focused through another"
    );
}
