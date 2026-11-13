#![forbid(unsafe_code)]
//! OQ-084 C.9: a workspace close disposes every panel attributed to it.
//!
//! C.9 makes a workspace close a *disposal path*: closing a workspace disposes
//! every panel attributed to it, ordered by MRU with the focused panel first,
//! through the release obligations of C.8, and emits one FS-4-style record per
//! panel with owner, controller generation, reason `quit`, and action
//! `disposed`.
//!
//! These tests drive the path that now exists: the ordered read accessor, the
//! sweep that releases the families per panel, the per-panel report, and the
//! production workspace-close seam that reaches it.
//!
//! Each test is written to fail on the pre-change tree: there was no
//! `panels_for_workspace`, no `dispose_workspace_panels` on either the registry
//! or the host, and no workspace-close path reached `dispose_panel` at all.

use std::cell::RefCell;
use std::rc::Rc;

use bitty_rich::scene::Scene;
use bitty_runtime::{
    Generation, Runtime, RuntimeConfig, UiRect, ViewId,
    registry::{
        BoundedPayload, OverlayKind, PanelDisposalAction, PanelDisposalReason, PanelId,
        PanelRegistry, PanelRegistryConfig, PanelRuntime, PanelType, WorkspaceDisposeReport,
        WorkspaceId,
    },
};

const CREATE: &str = "panel.create";
const COMMAND: &str = "helper.run:go";
const TOPIC: &str = "xuepoo.owner:status";

fn workspace(raw: u64) -> WorkspaceId {
    WorkspaceId::new(raw)
}

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds")
}

fn host() -> PanelRuntime {
    PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds")
}

fn rect() -> UiRect {
    UiRect::new(0, 0, 20, 10)
}

/// The accessor answers every attributed panel, not just the focused ones, and
/// the focused panel comes first.
///
/// The three orderings the sweep depends on, in one test:
/// 1. the focused panel is first even though it was created last,
/// 2. the rest of the workspace MRU follows in MRU order,
/// 3. a `Created` panel that never took focus still appears, after the MRU —
///    `PanelFocus` alone would lose it and the container would leak.
#[test]
fn the_accessor_orders_the_focused_panel_first_then_mru_then_never_focused() {
    let mut reg = registry();
    let ws = workspace(1);
    let first = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("first panel");
    let second = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("second panel");
    let third = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("third panel");

    // `second` then `third` take focus, so the MRU is [third, second] and
    // `first` never enters it.
    for handle in [second, third] {
        reg.mount_panel(handle.id, handle.generation, ViewId::new(handle.id.0 + 100))
            .expect("mount panel");
        reg.focus_panel(handle.id, handle.generation, ws)
            .expect("focus panel");
    }

    assert_eq!(
        reg.panels_for_workspace(ws),
        vec![third.id, second.id, first.id],
        "focused first, then MRU, then the never-focused panel"
    );
    assert_eq!(
        reg.mru_order(ws),
        vec![third.id, second.id],
        "the MRU itself holds only the panels that took focus"
    );
    assert_eq!(reg.focused_panel(ws), Some(third.id));
    // An unattributed workspace answers nothing rather than guessing.
    assert!(reg.panels_for_workspace(workspace(99)).is_empty());
}

/// The sweep releases every panel of the workspace, focused panel first, and
/// writes exactly one FS-4-style record per disposed panel.
///
/// The record is the C.9 evidence: owner (none while no controller binds),
/// the released controller generation, the state at the edge (which names the
/// C.8 arm that ran), reason `quit`, and action `disposed`.
#[test]
fn a_sweep_disposes_focused_first_and_emits_one_record_per_panel() {
    let mut reg = registry();
    let ws = workspace(2);
    let created = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("created panel");
    let mounted = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("mounted panel");
    let focused = reg
        .create_panel(PanelType::Rich, Some(ws))
        .expect("focused panel");
    for handle in [mounted, focused] {
        reg.mount_panel(handle.id, handle.generation, ViewId::new(handle.id.0 + 100))
            .expect("mount panel");
    }
    reg.focus_panel(focused.id, focused.generation, ws)
        .expect("focus panel");

    let report: WorkspaceDisposeReport = reg.dispose_workspace_panels(ws);

    assert!(
        report.is_complete(),
        "a clean sweep reports itself complete"
    );
    assert!(report.refused().is_none());
    assert!(report.remaining.is_empty());
    assert_eq!(
        report.disposed(),
        vec![focused.id, created.id, mounted.id],
        "the focused panel goes first, then the panels that never took focus in creation order"
    );
    assert_eq!(report.disposed_count(), 3);
    assert_eq!(reg.panel_count(), 0, "every container was retired");

    // Exactly one record per panel, in the disposal order, all reason `quit`
    // and action `disposed`.
    let recorded: Vec<PanelId> = report.records.iter().map(|record| record.panel).collect();
    assert_eq!(recorded, vec![focused.id, created.id, mounted.id]);
    for record in &report.records {
        assert_eq!(record.workspace, ws);
        assert_eq!(record.reason, PanelDisposalReason::Quit);
        assert_eq!(record.reason.as_str(), "quit");
        assert_eq!(record.action, PanelDisposalAction::Disposed);
        assert_eq!(record.action.as_str(), "disposed");
        assert!(
            record.owner.is_none(),
            "a container that never bound a controller carries no owner (OQ-084 A.2)"
        );
    }
    // The record names the state at the edge, so the two C.8 arms stay
    // distinguishable in the evidence: `Focused -> Disposed` and
    // `Created -> Disposed`.
    assert_eq!(
        report.records[0].state,
        bitty_ui::panel::PanelState::Focused
    );
    assert_eq!(
        report.records[1].state,
        bitty_ui::panel::PanelState::Created
    );
    assert_eq!(
        report.records[2].state,
        bitty_ui::panel::PanelState::Mounted
    );
    // The released generation is the allocation epoch while no controller is
    // bound, and it matches the handle the panel was created with.
    assert_eq!(report.records[0].controller_generation, focused.generation);

    // Focus routing and the workspace bookkeeping went with the containers.
    assert_eq!(reg.focused_panel(ws), None);
    assert!(reg.mru_order(ws).is_empty());
    assert!(reg.panels_for_workspace(ws).is_empty());
    assert_eq!(
        reg.attributed_workspace_count(),
        0,
        "the last panel drops the workspace bucket, which can never be reclaimed otherwise"
    );
}

/// C.8's `Focused -> Disposed` arm: input routing, the view attachment, the MRU
/// entry, the scene slot, panel commands, bus queues, grants, and overlay
/// attribution all go with the panel.
///
/// `Focused -> Disposed` must release input routing first — focus moves to the
/// next MRU entry — so the sweep leaves focus on a live panel until that panel
/// is disposed too, and never on a retired one.
#[test]
fn a_focused_disposal_releases_every_rule_8_family() {
    let mut reg = registry();
    let ws = workspace(3);
    let survivor = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("survivor panel");
    let doomed = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("doomed panel");
    for handle in [survivor, doomed] {
        reg.mount_panel(handle.id, handle.generation, ViewId::new(handle.id.0 + 100))
            .expect("mount panel");
    }
    reg.focus_panel(survivor.id, survivor.generation, ws)
        .expect("focus survivor");
    reg.focus_panel(doomed.id, doomed.generation, ws)
        .expect("focus doomed");

    reg.grant_panel_capability(doomed.id, doomed.generation, CREATE)
        .expect("grant create");
    reg.register_command(doomed.id, doomed.generation, COMMAND)
        .expect("register command");
    let topic = reg.declare_topic(TOPIC).expect("declare topic");
    reg.subscribe(doomed.id, doomed.generation, &topic)
        .expect("subscribe");
    reg.publish(&topic, BoundedPayload::try_new("one").expect("payload"))
        .expect("publish");
    reg.publish(&topic, BoundedPayload::try_new("two").expect("payload"))
        .expect("publish");
    reg.create_panel_overlay(
        doomed.id,
        doomed.generation,
        OverlayKind::NonModal,
        rect(),
        "attributed",
        None,
    )
    .expect("attributed overlay");
    assert_eq!(reg.bus_events_for_panel(doomed.id), 2);
    assert_eq!(reg.overlay_len(), 1);
    assert_eq!(reg.attributed_overlay_count(), 1);

    // Dispose only the focused panel: the survivor must keep its focus entry
    // until its own turn, which is what "focus moves to the next MRU entry"
    // means when the next entry is disposed immediately afterwards.
    reg.dispose_panel(doomed.id, doomed.generation)
        .expect("dispose focused panel");
    assert_eq!(
        reg.focused_panel(ws),
        Some(survivor.id),
        "input routing moved to the next MRU entry, not to none"
    );
    assert!(
        reg.panel_view(survivor.id, survivor.generation)
            .expect("survivor record")
            .is_some(),
        "the survivor keeps its attachment"
    );
    assert_eq!(reg.bus_events_for_panel(doomed.id), 0, "queues drained");
    assert_eq!(reg.overlay_len(), 0, "attributed overlay dismissed");
    assert_eq!(reg.attributed_overlay_count(), 0);
    assert!(
        reg.command_owner(COMMAND).is_none(),
        "the panel commands went with the container"
    );
    assert!(!reg.is_panel_capability_granted(doomed.id, doomed.generation, CREATE));

    // Disposing the survivor retires the pointer entirely: a workspace with no
    // other panel moves focus to none (C.8).
    reg.dispose_panel(survivor.id, survivor.generation)
        .expect("dispose survivor");
    assert_eq!(reg.focused_panel(ws), None);
}

/// C.8's `Created -> Disposed` arm: the workspace and window slot charges are
/// released, so a fresh panel fits in the same workspace afterwards.
///
/// A refused create would prove the charge leaked, so this test also proves the
/// sweep is not merely emptying visible lists.
#[test]
fn a_sweep_releases_the_workspace_and_window_slot_charges() {
    let config = PanelRegistryConfig {
        max_panels_per_workspace: 2,
        max_panels_per_window: 2,
        ..PanelRegistryConfig::default()
    };
    let mut reg = PanelRegistry::new(config).expect("bounded registry");
    let ws = workspace(4);
    for _ in 0..2 {
        reg.create_panel(PanelType::Helper, Some(ws))
            .expect("panel fits the workspace budget");
    }
    assert!(
        reg.create_panel(PanelType::Helper, Some(ws)).is_err(),
        "the workspace budget is full before the sweep"
    );

    let report = reg.dispose_workspace_panels(ws);
    assert!(report.is_complete());
    assert_eq!(report.records.len(), 2, "one record per disposed panel");

    for _ in 0..2 {
        reg.create_panel(PanelType::Helper, Some(ws))
            .expect("the workspace charge was released by the sweep");
    }
    assert_eq!(reg.panels_for_workspace(ws).len(), 2);
}

/// The sweep is scoped: an unrelated workspace keeps its panels, its bucket,
/// and its focus.
#[test]
fn a_sweep_leaves_another_workspace_untouched() {
    let mut reg = registry();
    let closing = workspace(5);
    let surviving = workspace(6);
    let closed_panel = reg
        .create_panel(PanelType::Helper, Some(closing))
        .expect("closing workspace panel");
    let surviving_panel = reg
        .create_panel(PanelType::Helper, Some(surviving))
        .expect("surviving workspace panel");

    let report = reg.dispose_workspace_panels(closing);
    assert!(report.is_complete());
    assert_eq!(report.records.len(), 1);
    assert_eq!(report.records[0].panel, closed_panel.id);

    assert!(reg.panels_for_workspace(closing).is_empty());
    assert_eq!(
        reg.panels_for_workspace(surviving),
        vec![surviving_panel.id]
    );
    assert_eq!(reg.panel_count(), 1);
    assert_eq!(reg.attributed_workspace_count(), 1);
}

/// The host facade releases the families the registry cannot own — the
/// placement mirror of the attachment, the lease entry, and the render scene
/// slot — for exactly the panels it disposed.
#[test]
fn the_host_releases_placement_lease_and_scene_slots() {
    let mut host = host();
    let ws = workspace(7);
    let handle = host.create_panel(PanelType::Rich, Some(ws)).expect("panel");
    host.mount_panel(handle.id, handle.generation, ViewId::new(41))
        .expect("mount");
    host.focus_panel(handle.id, handle.generation, ws)
        .expect("focus");
    host.attach_panel_scene(handle, Scene::new())
        .expect("attach scene");
    assert_eq!(host.placement_len(), 1);
    assert_eq!(host.panel_scene_len(), 1);
    let revision = host.scene_revision();

    let report = host.dispose_workspace_panels(ws);
    assert!(report.is_complete());
    assert_eq!(report.disposed(), vec![handle.id]);
    assert_eq!(host.panel_count(), 0);
    assert_eq!(host.placement_len(), 0, "attachment mirror released");
    assert_eq!(host.panel_scene_len(), 0, "scene slot released");
    assert!(
        host.panel_scene(handle.id).is_none(),
        "the scene cannot outlive its leaf"
    );
    assert!(
        host.scene_revision() > revision,
        "the present loop is told the scene store changed"
    );
    assert!(
        host.panel_lease_state(handle.id, handle.generation)
            .is_err(),
        "the lease entry went with the container"
    );
}

/// The production seam: closing a workspace disposes the panels attributed to
/// that slot's minted `WorkspaceId`, and the runtime retains the report.
///
/// This is the wiring C.9 asks for. Before it, `dispose_panel` was reachable
/// only from the scene host, so a workspace close left every panel container
/// behind with no owner.
#[test]
fn closing_a_workspace_disposes_its_panels() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime");
    let shared = Rc::new(RefCell::new(host()));
    rt.install_shared_scene_host(Rc::clone(&shared));
    let ws = rt.active_workspace_id();

    {
        let mut host = shared.borrow_mut();
        host.create_panel(PanelType::Helper, Some(ws))
            .expect("first panel in the closing workspace");
        host.create_panel(PanelType::Rich, Some(ws))
            .expect("second panel in the closing workspace");
    }
    assert_eq!(shared.borrow().panel_count(), 2);

    rt.workspace_close_at(0).expect("close the only workspace");

    let report = rt
        .last_workspace_panel_disposal()
        .expect("the close ran a panel sweep");
    assert!(report.is_complete());
    assert_eq!(report.workspace, ws);
    assert_eq!(report.records.len(), 2);
    assert!(report.records.iter().all(|record| record.workspace == ws));
    assert!(
        report
            .records
            .iter()
            .all(|record| record.action == PanelDisposalAction::Disposed)
    );
    assert_eq!(
        shared.borrow().panel_count(),
        0,
        "the closed slot's panels are gone from the shared host"
    );
}

/// The last slot is replaced rather than left empty, and a panel attributed to
/// the retired id is not resurrected into the fresh slot.
#[test]
fn the_fresh_workspace_after_a_close_starts_with_no_panels() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime");
    let shared = Rc::new(RefCell::new(host()));
    rt.install_shared_scene_host(Rc::clone(&shared));
    let retired = rt.active_workspace_id();
    {
        let mut host = shared.borrow_mut();
        host.create_panel(PanelType::Helper, Some(retired))
            .expect("panel in the retired workspace");
    }

    rt.workspace_close_at(0).expect("close the only workspace");
    let fresh = rt.active_workspace_id();
    assert_ne!(fresh, retired, "a slot id is never reused (A.7)");
    assert!(
        shared
            .borrow()
            .registry()
            .panels_for_workspace(retired)
            .is_empty()
    );
    let report = rt.last_workspace_panel_disposal().expect("sweep report");
    assert!(report.is_complete());
    assert_eq!(report.disposed_count(), 1);
    assert_eq!(shared.borrow().registry().attributed_workspace_count(), 0);
}

/// A runtime with no installed panel host — every headless runtime — closes a
/// workspace without inventing a report and without panicking.
#[test]
fn a_close_without_a_panel_host_reports_nothing() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime");
    rt.workspace_close_at(0).expect("close the only workspace");
    assert!(rt.last_workspace_panel_disposal().is_none());
}

/// `Generation` is part of the record's evidence, so the sweep must not invent
/// one: the report carries the panel's own allocation epoch.
#[test]
fn the_record_names_the_panels_own_generation() {
    let mut reg = registry();
    let ws = workspace(8);
    let handle = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("panel");
    let report = reg.dispose_workspace_panels(ws);
    assert_eq!(report.records.len(), 1);
    assert_eq!(
        report.records[0].controller_generation,
        Generation(handle.generation.0),
        "the released generation is the one `create_panel` minted"
    );
}
