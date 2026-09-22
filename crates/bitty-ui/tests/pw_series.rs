//! PW panel series batch (UX-06/UX-07/UX-08/UX-10/UX-11/UX-12,
//! CTX-0684, issues #1012 #1013 #1014 #1016 #1017 #1018).
//!
//! Candidate behavior: stable identity display (`panel_identity`),
//! restart manifests (`panel_persist`), the never-empty workspace guard
//! (`workspace_guard`), drag-to-Bar drops (`drag_bar`), Lua API
//! descriptors (`panel_lua`), and the tab strip projection (`tab_strip`).
//! Cross-module checks the unit tests inside those modules do not cover
//! alone: identity-to-tab joins, manifest restore planning, guarded
//! last-panel closes, registry-gated Bar commits with undo, capability
//! gating, and strip snapshot round-trips.

#![forbid(unsafe_code)]

use bitty_ui::{
    ApiScope, BAR_MOVE_CMD, BAR_SPLIT_CMD, BarDropSession, BarError, BarOutcome, BarUndoStack,
    BarZone, CapabilityGate, CloseRequest, CloseResolution, CommandRegistry, FocusResolution,
    GuardError, IdentityRegistry, LastPanelPolicy, LuaCapability, ManifestError, PanelId,
    PersistedPanel, PersistencePolicy, RestartManifest, SlotNumber, TabError, TabScope, TabStrip,
    WorkspaceGuard, classify_bar_drop, decode_manifest, encode_manifest, lookup_command,
    plan_restore, tab_label, undo_bar_drop, validate_spellings,
};

fn slot(n: u8) -> SlotNumber {
    SlotNumber::new(n).unwrap()
}

fn bound_registry() -> (IdentityRegistry, PanelId, PanelId) {
    let mut reg = IdentityRegistry::new();
    let a = PanelId::new(7);
    let b = PanelId::new(9);
    reg.bind(a, slot(1), "editor").unwrap();
    reg.bind(b, slot(2), "logs").unwrap();
    (reg, a, b)
}

// ---------------------------------------------------------------------------
// UX-06 (#1012): Mod+Number changes the physical slot only, never identity
// ---------------------------------------------------------------------------

#[test]
fn mod_number_reseat_swaps_slots_with_identity_stable() {
    let (mut reg, a, b) = bound_registry();
    let ha = reg.handle_of(a).unwrap();
    reg.reseat(a, slot(2)).unwrap();
    assert_eq!(reg.slot_of(a).unwrap(), slot(2));
    assert_eq!(reg.slot_of(b).unwrap(), slot(1));
    assert_eq!(reg.title_of(a).unwrap(), "editor");
    assert_eq!(reg.resolve(ha), Some(a));
    assert_eq!(
        tab_label(reg.slot_of(a).unwrap(), reg.title_of(a).unwrap()),
        "2:editor"
    );
}

#[test]
fn opaque_handle_never_exposes_the_raw_id() {
    let (reg, a, _) = bound_registry();
    let handle = reg.handle_of(a).unwrap();
    assert_eq!(handle.to_string(), "PanelHandle(opaque)");
    assert_eq!(reg.resolve(handle), Some(a));
}

#[test]
fn identity_bind_fails_closed_on_occupied_and_bad_slots() {
    let (mut reg, a, _) = bound_registry();
    assert!(reg.bind(PanelId::new(11), slot(1), "dup").is_err());
    assert_eq!(reg.slot_of(a).unwrap(), slot(1));
    assert!(SlotNumber::new(0).is_err());
}

// ---------------------------------------------------------------------------
// UX-07 (#1013): restart manifest round-trip and restore planning
// ---------------------------------------------------------------------------

#[test]
fn manifest_round_trip_and_stable_restore_order() {
    let mut manifest = RestartManifest::new();
    manifest
        .push(PersistedPanel::new(PanelId::new(9), 1, "logs").unwrap())
        .unwrap();
    manifest
        .push(PersistedPanel::new(PanelId::new(3), 0, "editor").unwrap())
        .unwrap();
    let back = decode_manifest(&encode_manifest(&manifest)).unwrap();
    assert_eq!(back, manifest);
    let stable = plan_restore(&back, PersistencePolicy::StableIdentity);
    assert_eq!(stable[0].id, PanelId::new(3));
    assert_eq!(stable[1].id, PanelId::new(9));
    let ephemeral = plan_restore(&back, PersistencePolicy::Ephemeral);
    assert_eq!(ephemeral[0].id, PanelId::new(9));
}

#[test]
fn manifest_defects_reject_the_whole_document() {
    assert_eq!(
        decode_manifest("v=99;id=1;ws=0;title=x").unwrap_err(),
        ManifestError::UnsupportedVersion {
            found: 99,
            supported: bitty_ui::PERSIST_MANIFEST_VERSION,
        }
    );
    assert!(decode_manifest("v=1;id=1;ws=0").is_err());
    assert!(decode_manifest("not-a-record").is_err());
}

// ---------------------------------------------------------------------------
// UX-08 (#1014): never-empty closes and zero-focus reconciliation
// ---------------------------------------------------------------------------

#[test]
fn last_panel_park_and_merge_keep_the_invariant() {
    let mut guard = WorkspaceGuard::new();
    guard.add_workspace(0).unwrap();
    guard.add_workspace(1).unwrap();
    guard.open_panel(0, PanelId::new(1)).unwrap();
    guard.open_panel(1, PanelId::new(2)).unwrap();
    let out = guard
        .close_panel(
            0,
            CloseRequest {
                panel: PanelId::new(1),
                policy: LastPanelPolicy::Park,
                donor: None,
                target: None,
            },
        )
        .unwrap();
    assert_eq!(out, CloseResolution::Parked);
    let parked = guard.get(0).unwrap();
    assert!(parked.is_empty() && parked.parked());
    assert_eq!(guard.reconcile_focus(0).unwrap(), FocusResolution::Cleared);

    // Reopen, then merge workspace 0 away into workspace 1.
    guard.open_panel(0, PanelId::new(3)).unwrap();
    let out = guard
        .close_panel(
            0,
            CloseRequest {
                panel: PanelId::new(3),
                policy: LastPanelPolicy::Merge,
                donor: None,
                target: Some(1),
            },
        )
        .unwrap();
    assert_eq!(out, CloseResolution::Merged { into: 1 });
    assert!(guard.get(0).is_none());
    assert!(guard.get(1).unwrap().panels().contains(&PanelId::new(2)));
}

#[test]
fn reassign_pulls_a_donor_and_reconciles_both_sides() {
    let mut guard = WorkspaceGuard::new();
    guard.add_workspace(0).unwrap();
    guard.add_workspace(1).unwrap();
    guard.open_panel(0, PanelId::new(1)).unwrap();
    guard.open_panel(1, PanelId::new(2)).unwrap();
    guard.open_panel(1, PanelId::new(4)).unwrap();
    guard.focus_panel(1, PanelId::new(2)).unwrap();
    let out = guard
        .close_panel(
            0,
            CloseRequest {
                panel: PanelId::new(1),
                policy: LastPanelPolicy::Reassign,
                donor: Some((1, PanelId::new(2))),
                target: None,
            },
        )
        .unwrap();
    assert_eq!(
        out,
        CloseResolution::Reassigned {
            donor: PanelId::new(2)
        }
    );
    assert_eq!(guard.get(0).unwrap().focus(), Some(PanelId::new(2)));
    // Donor workspace kept its other panel and reconciled focus onto it.
    assert_eq!(guard.get(1).unwrap().panels(), vec![PanelId::new(4)]);
    assert_eq!(guard.get(1).unwrap().focus(), Some(PanelId::new(4)));
    // A donor-less reassign fails with both workspaces untouched.
    guard.add_workspace(2).unwrap();
    guard.open_panel(2, PanelId::new(8)).unwrap();
    let err = guard
        .close_panel(
            2,
            CloseRequest {
                panel: PanelId::new(8),
                policy: LastPanelPolicy::Reassign,
                donor: None,
                target: None,
            },
        )
        .unwrap_err();
    assert_eq!(err, GuardError::NeedsDonor);
    assert_eq!(guard.get(2).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// UX-10 (#1016): drag-to-Bar zones, gated commits, bounded undo
// ---------------------------------------------------------------------------

fn bar_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();
    registry.register(PanelId::new(1), BAR_MOVE_CMD).unwrap();
    registry.register(PanelId::new(1), BAR_SPLIT_CMD).unwrap();
    registry
}

#[test]
fn bar_center_commit_moves_and_undo_restores() {
    let registry = bar_registry();
    let mut order = vec![PanelId::new(1), PanelId::new(2)];
    let mut workspaces = 2_usize;
    let mut undo = BarUndoStack::new();
    let mut session = BarDropSession::lift(PanelId::new(1), true, BAR_MOVE_CMD).unwrap();
    session.preview_hover(20, 10, 0);
    assert_eq!(session.preview().unwrap().zone, BarZone::MoveSwitch);
    let out = session
        .commit(&registry, &mut order, &mut workspaces, &mut undo)
        .unwrap();
    assert_eq!(out, BarOutcome::Committed(BarZone::MoveSwitch));
    assert_eq!(order, vec![PanelId::new(2), PanelId::new(1)]);
    assert_eq!(workspaces, 2);
    undo_bar_drop(&mut undo, &mut order, &mut workspaces).unwrap();
    assert_eq!(order, vec![PanelId::new(1), PanelId::new(2)]);
}

#[test]
fn bar_edge_commit_splits_and_cap_fails_closed() {
    let registry = bar_registry();
    let mut order = vec![PanelId::new(1)];
    let mut workspaces = bitty_ui::MAX_BAR_WORKSPACES;
    let mut undo = BarUndoStack::new();
    let mut session = BarDropSession::lift(PanelId::new(1), true, BAR_SPLIT_CMD).unwrap();
    session.preview_hover(20, 0, workspaces as u32);
    assert_eq!(session.preview().unwrap().zone, BarZone::NewWorkspace);
    let before = order.clone();
    assert!(
        session
            .commit(&registry, &mut order, &mut workspaces, &mut undo)
            .is_err()
    );
    assert_eq!(order, before);
    assert!(undo.is_empty());
}

#[test]
fn bar_commit_without_registration_leaves_order_untouched() {
    let registry = CommandRegistry::new();
    let mut order = vec![PanelId::new(1), PanelId::new(2)];
    let mut workspaces = 1_usize;
    let mut undo = BarUndoStack::new();
    let mut session = BarDropSession::lift(PanelId::new(1), true, BAR_MOVE_CMD).unwrap();
    session.preview_hover(20, 10, 0);
    let err = session
        .commit(&registry, &mut order, &mut workspaces, &mut undo)
        .unwrap_err();
    assert!(matches!(err, BarError::UnregisteredCommand(_)));
    assert_eq!(order, vec![PanelId::new(1), PanelId::new(2)]);
    assert!(classify_bar_drop(20, 20).is_none());
}

// ---------------------------------------------------------------------------
// UX-11 (#1017): Lua descriptor table and capability gating
// ---------------------------------------------------------------------------

#[test]
fn lua_table_validates_and_gate_enforces_capabilities() {
    validate_spellings().unwrap();
    let gate = CapabilityGate::with_grants(&[LuaCapability::MovePanel]);
    let cmd = gate.allows("bitty.panel:move").unwrap();
    assert_eq!(cmd.capability, LuaCapability::MovePanel);
    assert_eq!(cmd.scope, ApiScope::Panel);
    assert!(gate.allows("bitty.panel:resize").is_err());
    assert!(gate.allows("bitty.panel:nope").is_err());
    assert_eq!(
        lookup_command("bitty.workspace:switch").unwrap().scope,
        ApiScope::Workspace
    );
}

// ---------------------------------------------------------------------------
// UX-12 (#1018): tab strip projection, reorder, snapshot round-trip
// ---------------------------------------------------------------------------

#[test]
fn tab_reorder_changes_slots_never_identity() {
    let mut strip = TabStrip::new(TabScope::PerWorkspace(0));
    for id in [11, 22, 33] {
        strip.open_tab(PanelId::new(id)).unwrap();
    }
    strip.reorder(0, 3).unwrap();
    assert_eq!(
        strip.order(),
        &[PanelId::new(22), PanelId::new(33), PanelId::new(11)]
    );
    let cells = strip.cells();
    assert_eq!(cells[0].slot, 1);
    assert_eq!(
        cells[2],
        bitty_ui::TabCell {
            panel: PanelId::new(11),
            slot: 3
        }
    );
    // Join with identity titles: the tab label follows the slot.
    let (reg, _, _) = bound_registry();
    let title = reg.title_of(PanelId::new(7)).unwrap();
    assert_eq!(tab_label(slot(1), title), "1:editor");
}

#[test]
fn tab_snapshot_restore_round_trip_and_move_scope() {
    let mut strip = TabStrip::new(TabScope::PerWorkspace(0));
    strip.open_tab(PanelId::new(5)).unwrap();
    strip.open_tab(PanelId::new(6)).unwrap();
    let snap = strip.snapshot();
    let mut other = TabStrip::new(TabScope::PerWindow);
    other.restore(&snap).unwrap();
    assert_eq!(other.order(), strip.order());
    assert_eq!(
        other
            .restore(&[PanelId::new(1), PanelId::new(1)])
            .unwrap_err(),
        TabError::DuplicateTab {
            panel: PanelId::new(1)
        }
    );
    strip.move_to(PanelId::new(5), &mut other).unwrap_err();
    let mut fresh = TabStrip::new(TabScope::PerWindow);
    strip.move_to(PanelId::new(5), &mut fresh).unwrap();
    assert_eq!(strip.order(), &[PanelId::new(6)]);
    assert_eq!(fresh.order(), &[PanelId::new(5)]);
}
