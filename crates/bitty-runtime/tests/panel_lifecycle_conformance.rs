#![forbid(unsafe_code)]
//! CTX-0648 (issue #999, CW-21): Panel lifecycle conformance tests.
//!
//! Issue #999: "CW-21: Panel lifecycle conformance tests (RFC verification
//! plan)". Scope: identity/lifecycle/focus/overlay/headless composition test
//! suite per accepted `panel-runtime-rfc.md` items 2-8. Sub-issue of #971,
//! backlog item CW-21, size L / P1, no dependencies.
//!
//! Composition context from the just-merged lanes:
//! - CTX-0651 (#991): tiered overlay stacking in the present path
//!   (`OverlayTier` order `Editor < Float < Popup < Messages`).
//! - CTX-0652/0653/0654 (#987/#989/#988): presentation transitions gated by
//!   `PresentationMode::can_transition`, scratchpad, drag/resize.
//! - CTX-0655 (#986): `LayoutProvider` plugin path (`provider.rs`).
//!
//! What this file pins (all headless, deterministic, no PTY):
//! - Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed
//!   ordering with view attachment asserted at every step (RFC lifecycle).
//! - Mount atomicity: a failed mount (`PanelAlreadyMounted` /
//!   `AlreadyMounted`) / stale generation leaves both mappings and both
//!   records untouched (RFC single-owner + fail-closed rules).
//! - Focus re-home: unmount/dispose of the focused panel moves focus to the
//!   next MRU entry before the detach commits; the last removal yields
//!   `None` (RFC focus routing rule 4).
//! - No-focus rules: `Created`/viewless/wrong-workspace panels refuse focus;
//!   `route_input` never targets a non-focused panel (RFC focus rules 4-5).
//! - Overlay/focus-retention composition: overlays never mutate panel state,
//!   view attachment, or focus; modal exclusivity keeps the first modal in
//!   place (RFC overlay rules 2-4).
//! - Presentation orthogonality: stamping `PresentationMode` never moves the
//!   layout solver output or the panel registry state (CTX-0652 contract).
//!
//! Non-overlap: unit coverage for the registry lives in
//! `src/registry/panel_tests.rs` (single-op errors, bus budgets,
//! capabilities) and layout/focus property coverage in
//! `tests/panel_invariants.rs` / `tests/panel_session_invariants.rs` (live
//! `Runtime` geometry). This file asserts the end-to-end conformance
//! sequences those suites do not: full ordering in one registry, atomicity
//! snapshots around failed mounts, and multi-panel re-home chains.

use bitty_runtime::{
    Generation, LayoutNode, SplitAxis, UiRect, View, ViewId, WorkspaceId,
    registry::{PanelError, PanelId, PanelRegistry, PanelRegistryConfig, PanelState, PanelType},
};
use bitty_ui::{InputTarget, OverlayKind, PresentationMode, route_input};

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("default panel registry")
}

fn workspace(n: u64) -> WorkspaceId {
    WorkspaceId::new(n)
}

fn view(n: u64) -> ViewId {
    ViewId::new(n)
}

fn rect() -> UiRect {
    UiRect::new(0, 0, 20, 10)
}

// ---------------------------------------------------------------------------
// Lifecycle ordering
// ---------------------------------------------------------------------------

/// RFC lifecycle model: `Declared` is entered only via `Created`; no direct
/// edge from `Declared` to any later state exists.
#[test]
fn declared_entry_is_only_via_created() {
    use PanelState as S;
    assert!(S::can_transition(S::Declared, S::Created));
    for forbidden in [S::Mounted, S::Focused, S::Suspended, S::Disposed] {
        assert!(
            !S::can_transition(S::Declared, forbidden),
            "Declared -> {forbidden} must be refused"
        );
    }
    // The full forward chain stays reachable through the single gate.
    assert!(S::can_transition(S::Created, S::Mounted));
    assert!(S::can_transition(S::Mounted, S::Focused));
    assert!(S::can_transition(S::Focused, S::Suspended));
    assert!(S::can_transition(S::Suspended, S::Mounted));
    assert!(S::can_transition(S::Suspended, S::Focused));
    assert!(S::can_transition(S::Mounted, S::Disposed));
    assert!(S::can_transition(S::Focused, S::Disposed));
    assert!(S::can_transition(S::Suspended, S::Disposed));
    assert!(S::can_transition(S::Created, S::Disposed));
}

/// End-to-end ordering in ONE registry with the view attachment asserted at
/// every step: Created(viewless) -> Mounted(view) -> Focused(view) ->
/// Suspended(view retained) -> Mounted -> Focused -> Suspended(viewless via
/// unmount) -> Created(viewless via resume) -> Mounted -> Disposed(gone).
#[test]
fn full_lifecycle_ordering_with_view_attachment() {
    let mut reg = registry();
    let ws = workspace(1);
    let h = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();

    // Created: allocated with (PanelId, Generation), viewless.
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Created
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), None);

    // Mounted: bound to an empty view.
    let v1 = view(1);
    reg.mount_panel(h.id, h.generation, v1).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Mounted
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), Some(v1));

    // Focused: owns routing within its workspace.
    reg.focus_panel(h.id, h.generation, ws).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Focused
    );
    assert_eq!(reg.focused_panel(ws), Some(h.id));

    // Suspended via suspend: invisible, attachment retained.
    reg.suspend_panel(h.id, h.generation).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Suspended
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), Some(v1));

    // Resume with retained view returns to Mounted with the same view.
    reg.resume_panel(h.id, h.generation).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Mounted
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), Some(v1));

    // Focus again, then unmount: Suspended WITHOUT a view.
    reg.focus_panel(h.id, h.generation, ws).unwrap();
    let removed = reg.unmount_panel(h.id, h.generation).unwrap();
    assert_eq!(removed, v1);
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Suspended
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), None);

    // Resume viewless returns to Created (never Mounted without a view).
    reg.resume_panel(h.id, h.generation).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Created
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), None);

    // Re-mount under a new view preserves identity.
    let v2 = view(2);
    reg.mount_panel(h.id, h.generation, v2).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Mounted
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), Some(v2));

    // Disposed: the handle is retired; further reads fail.
    reg.dispose_panel(h.id, h.generation).unwrap();
    assert!(reg.panel_state(h.id, h.generation).is_err());
    assert!(reg.panel_view(h.id, h.generation).is_err());
}

// ---------------------------------------------------------------------------
// Mount atomicity
// ---------------------------------------------------------------------------

/// `PanelAlreadyMounted`: re-mounting an already-mounted panel fails with
/// `current_view` pointing at the original view, and NEITHER mapping moves:
/// the panel stays Mounted at the first view and the target view stays free.
#[test]
fn mount_atomicity_on_panel_already_mounted() {
    let mut reg = registry();
    let h1 = reg.create_panel(PanelType::Terminal, None).unwrap();
    let h2 = reg.create_panel(PanelType::Rich, None).unwrap();
    let v1 = view(1);
    let v2 = view(2);
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();

    let err = reg.mount_panel(h1.id, h1.generation, v2).unwrap_err();
    match err {
        PanelError::PanelAlreadyMounted {
            panel_id,
            current_view,
        } => {
            assert_eq!(panel_id, h1.id);
            assert_eq!(current_view, v1);
        }
        other => panic!("expected PanelAlreadyMounted, got {other:?}"),
    }

    // Atomicity snapshot: original mapping intact, target view untouched.
    assert_eq!(
        reg.panel_state(h1.id, h1.generation).unwrap(),
        PanelState::Mounted
    );
    assert_eq!(reg.panel_view(h1.id, h1.generation).unwrap(), Some(v1));
    // The refused target view still accepts a fresh mount.
    reg.mount_panel(h2.id, h2.generation, v2).unwrap();
    assert_eq!(reg.panel_view(h2.id, h2.generation).unwrap(), Some(v2));
    assert_eq!(reg.panel_view(h1.id, h1.generation).unwrap(), Some(v1));
}

/// `AlreadyMounted`: mounting onto an occupied view fails, and the incoming
/// panel stays `Created`/viewless while the occupant is undisturbed; the
/// incoming panel remains mountable elsewhere afterwards.
#[test]
fn mount_atomicity_on_view_occupied() {
    let mut reg = registry();
    let h1 = reg.create_panel(PanelType::Terminal, None).unwrap();
    let h2 = reg.create_panel(PanelType::Rich, None).unwrap();
    let v1 = view(1);
    let v2 = view(2);
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();

    let err = reg.mount_panel(h2.id, h2.generation, v1).unwrap_err();
    assert!(
        matches!(err, PanelError::AlreadyMounted { .. }),
        "expected AlreadyMounted, got {err:?}"
    );

    // Atomicity snapshot: occupant intact, incoming panel still Created.
    assert_eq!(
        reg.panel_state(h1.id, h1.generation).unwrap(),
        PanelState::Mounted
    );
    assert_eq!(reg.panel_view(h1.id, h1.generation).unwrap(), Some(v1));
    assert_eq!(
        reg.panel_state(h2.id, h2.generation).unwrap(),
        PanelState::Created
    );
    assert_eq!(reg.panel_view(h2.id, h2.generation).unwrap(), None);

    // The refused panel mounts cleanly on a free view.
    reg.mount_panel(h2.id, h2.generation, v2).unwrap();
    assert_eq!(
        reg.panel_state(h2.id, h2.generation).unwrap(),
        PanelState::Mounted
    );
}

/// RFC rule 3: a stale `(PanelId, Generation)` fails with `StaleHandle`
/// BEFORE any mapping is installed; the record stays `Created`/viewless.
#[test]
fn stale_mount_fails_before_any_mapping() {
    let mut reg = registry();
    let h = reg.create_panel(PanelType::Browser, None).unwrap();
    let stale = Generation(h.generation.get().wrapping_add(7));
    let v = view(9);

    let err = reg.mount_panel(h.id, stale, v).unwrap_err();
    match err {
        PanelError::StaleHandle {
            expected_generation,
            found_generation,
            id_raw,
        } => {
            assert_eq!(expected_generation, h.generation);
            assert_eq!(found_generation, stale);
            assert_eq!(id_raw, h.id.0);
        }
        other => panic!("expected StaleHandle, got {other:?}"),
    }
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Created
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), None);
    // The live handle still mounts afterwards: nothing was half-installed.
    reg.mount_panel(h.id, h.generation, v).unwrap();
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), Some(v));
}

// ---------------------------------------------------------------------------
// Focus re-home
// ---------------------------------------------------------------------------

/// RFC focus rule 4: detaching or destroying the focused panel re-homes
/// focus to the next MRU entry in the same workspace; removing the last
/// target yields `None`. Survivors keep their views throughout.
#[test]
fn focus_rehome_chain_on_unmount_and_dispose() {
    let mut reg = registry();
    let ws = workspace(42);
    let h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
    let h2 = reg.create_panel(PanelType::Rich, Some(ws)).unwrap();
    let h3 = reg.create_panel(PanelType::Canvas, Some(ws)).unwrap();
    let (v1, v2, v3) = (view(1), view(2), view(3));
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();
    reg.mount_panel(h2.id, h2.generation, v2).unwrap();
    reg.mount_panel(h3.id, h3.generation, v3).unwrap();
    reg.focus_panel(h1.id, h1.generation, ws).unwrap();
    reg.focus_panel(h2.id, h2.generation, ws).unwrap();
    reg.focus_panel(h3.id, h3.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h3.id));
    assert_eq!(reg.mru_order(ws), vec![h3.id, h2.id, h1.id]);

    // Unmount the focused panel: focus re-homes to the next MRU entry.
    let removed = reg.unmount_panel(h3.id, h3.generation).unwrap();
    assert_eq!(removed, v3);
    assert_eq!(reg.focused_panel(ws), Some(h2.id));
    assert_eq!(reg.mru_order(ws), vec![h2.id, h1.id]);
    // Survivors keep state and views; the removed panel is viewless.
    assert_eq!(reg.panel_view(h2.id, h2.generation).unwrap(), Some(v2));
    assert_eq!(reg.panel_view(h1.id, h1.generation).unwrap(), Some(v1));
    assert_eq!(reg.panel_view(h3.id, h3.generation).unwrap(), None);

    // Dispose the newly focused panel: re-home again before the record goes.
    reg.dispose_panel(h2.id, h2.generation).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h1.id));
    assert_eq!(reg.mru_order(ws), vec![h1.id]);

    // Dispose the last panel: focus becomes None (never a stale target).
    reg.dispose_panel(h1.id, h1.generation).unwrap();
    assert_eq!(reg.focused_panel(ws), None);
    assert!(reg.mru_order(ws).is_empty());
}

/// Focus MRU is scoped per workspace: hiding the focus in one workspace
/// never disturbs the other workspace's focus.
#[test]
fn focus_rehome_is_scoped_per_workspace() {
    let mut reg = registry();
    let (ws_a, ws_b) = (workspace(1), workspace(2));
    let ha = reg.create_panel(PanelType::Terminal, Some(ws_a)).unwrap();
    let hb = reg.create_panel(PanelType::Terminal, Some(ws_b)).unwrap();
    reg.mount_panel(ha.id, ha.generation, view(11)).unwrap();
    reg.mount_panel(hb.id, hb.generation, view(21)).unwrap();
    reg.focus_panel(ha.id, ha.generation, ws_a).unwrap();
    reg.focus_panel(hb.id, hb.generation, ws_b).unwrap();

    reg.suspend_panel(ha.id, ha.generation).unwrap();
    assert_eq!(reg.focused_panel(ws_a), None);
    assert_eq!(reg.focused_panel(ws_b), Some(hb.id));
}

// ---------------------------------------------------------------------------
// No-focus rules
// ---------------------------------------------------------------------------

/// RFC focus rules 4-5: an empty workspace has no focus; `Created`,
/// viewless-`Suspended`, and wrong-workspace panels refuse focus without
/// moving focus; input routing never targets a non-focused panel.
#[test]
fn no_focus_rules_for_unmounted_and_foreign_panels() {
    let mut reg = registry();
    let (ws, other) = (workspace(5), workspace(6));

    // Empty workspace: zero focus, not a stale id.
    assert_eq!(reg.focused_panel(ws), None);

    // Created (never mounted) refuses focus and leaves focus at None.
    let h = reg.create_panel(PanelType::Helper, Some(ws)).unwrap();
    let err = reg.focus_panel(h.id, h.generation, ws).unwrap_err();
    assert!(matches!(err, PanelError::InvalidState { .. }));
    assert_eq!(reg.focused_panel(ws), None);

    // Mounted-but-foreign workspace refuses focus.
    let v = view(50);
    reg.mount_panel(h.id, h.generation, v).unwrap();
    let err = reg.focus_panel(h.id, h.generation, other).unwrap_err();
    assert!(matches!(err, PanelError::NotFound { .. }));
    assert_eq!(reg.focused_panel(ws), None);
    assert_eq!(reg.focused_panel(other), None);

    // Legal focus works, then unmount makes the record viewless and focus
    // returns to None rather than dangling.
    reg.focus_panel(h.id, h.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h.id));
    reg.unmount_panel(h.id, h.generation).unwrap();
    assert_eq!(reg.focused_panel(ws), None);
    reg.resume_panel(h.id, h.generation).unwrap();
    let err = reg.focus_panel(h.id, h.generation, ws).unwrap_err();
    assert!(matches!(err, PanelError::InvalidState { .. }));
    assert_eq!(reg.focused_panel(ws), None);
}

/// A non-focused panel never receives keyboard/IME/wheel events: routing
/// follows the focused panel, falls back to the view, and is `None` when
/// neither exists.
#[test]
fn input_routing_never_targets_non_focused_panel() {
    let panel = PanelId::new(42);
    let vid = ViewId::new(7);
    assert_eq!(
        route_input(Some(panel), Some(vid)),
        InputTarget::Panel(panel)
    );
    assert_eq!(route_input(None, Some(vid)), InputTarget::View(vid));
    assert_eq!(route_input(None, None), InputTarget::None);
}

// ---------------------------------------------------------------------------
// Overlay / headless composition
// ---------------------------------------------------------------------------

/// RFC overlay rules 2-4: overlays never mutate panel state, view
/// attachment, or focus; a second modal returns `OverlayBusy` and leaves
/// the first modal in place; dismissal restores the exact prior counts.
#[test]
fn overlay_activity_never_mutates_panel_lifecycle() {
    let mut reg = registry();
    let ws = workspace(7);
    let h = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
    let v = view(1);
    reg.mount_panel(h.id, h.generation, v).unwrap();
    reg.focus_panel(h.id, h.generation, ws).unwrap();

    let bounds = rect();
    for _ in 0..4 {
        reg.create_overlay(OverlayKind::NonModal, bounds, "note", None)
            .unwrap();
    }
    reg.create_overlay(OverlayKind::Modal, bounds, "modal", None)
        .unwrap();
    assert_eq!(reg.overlay_len(), 5);
    assert!(reg.overlay_modal_active());

    // Second modal fails closed and keeps the first modal in place.
    let err = reg
        .create_overlay(OverlayKind::Modal, bounds, "modal-2", None)
        .unwrap_err();
    assert_eq!(err, PanelError::OverlayBusy);
    assert_eq!(reg.overlay_len(), 5);
    assert!(reg.overlay_modal_active());

    // Panel lifecycle untouched by the whole overlay episode.
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Focused
    );
    assert_eq!(reg.panel_view(h.id, h.generation).unwrap(), Some(v));
    assert_eq!(reg.focused_panel(ws), Some(h.id));

    // Dismissing everything restores counts without touching the panel.
    reg.dismiss_overlay(1);
    reg.dismiss_overlay(2);
    reg.dismiss_overlay(3);
    reg.dismiss_overlay(4);
    reg.dismiss_overlay(5);
    assert_eq!(reg.overlay_len(), 0);
    assert!(!reg.overlay_modal_active());
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Focused
    );
    assert_eq!(reg.focused_panel(ws), Some(h.id));
}

// ---------------------------------------------------------------------------
// Presentation orthogonality (CTX-0652/0655 composition)
// ---------------------------------------------------------------------------

/// Presentation/stacking lanes stay orthogonal to lifecycle: stamping
/// `PresentationMode` on views leaves the solver output byte-identical and
/// never moves a panel record. Covers the CTX-0652 single-gate contract
/// (`can_transition` permits every pair; identity is a no-op) at the
/// composition boundary this task owns.
#[test]
fn presentation_stamps_leave_solver_and_lifecycle_untouched() {
    let bounds = UiRect::new(0, 0, 100, 40);
    let base = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    );
    let mut stamped = base.clone();
    for (id, mode) in [
        (ViewId::new(1), PresentationMode::Floating),
        (ViewId::new(2), PresentationMode::Scratchpad),
    ] {
        let leaf = stamped.find_leaf_mut(id).expect("leaf present");
        assert!(PresentationMode::request_transition(leaf, mode));
        // Identity re-affirm is an allowed no-op.
        assert!(PresentationMode::request_transition(leaf, mode));
    }
    assert_eq!(base.layout(bounds), stamped.layout(bounds));

    // Same episode at the registry boundary: presentation-only overlay
    // traffic does not advance lifecycle or focus.
    let mut reg = registry();
    let ws = workspace(9);
    let h = reg.create_panel(PanelType::Rich, Some(ws)).unwrap();
    reg.mount_panel(h.id, h.generation, view(31)).unwrap();
    reg.focus_panel(h.id, h.generation, ws).unwrap();
    reg.create_overlay(OverlayKind::NonModal, rect(), "hint", None)
        .unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Focused
    );
    assert_eq!(reg.focused_panel(ws), Some(h.id));
}
