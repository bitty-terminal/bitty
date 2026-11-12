#![forbid(unsafe_code)]
//! OQ-084 D.2: the production `rebind_panel` across a plugin generation swap.
//!
//! The decision record makes a bind three phases: phase 0 captures the
//! predecessor ceiling and derives the successor's set from that snapshot, five
//! ordered phase-1 steps invalidate the predecessor, and three phase-2 steps
//! commit. This file drives the shipped path with the six cases the issue names
//! — reload with a mounted panel, reload with a suspended panel, an active
//! overlay/capture at bind time, the plugin removed, the plugin denied, and
//! restore after restart — plus the adversarial rows the matrix adds: a racing
//! generation, a retired generation, a stale tombstone, an empty tombstone, and
//! the lease tenure.
//!
//! What a successful rebind must never do is inherit authority: every
//! capability the successor holds is one its own resolution declared and the
//! predecessor's ceiling allowed, or one a permission-diff consent admitted for
//! this binding only. What it must never do either is orphan the container: a
//! plugin that is gone or denied leaves the panel in place with a placeholder,
//! never deleted and never silently hidden.

use std::collections::BTreeSet;

use bitty_plugin_host::PluginId;
use bitty_runtime::{
    Generation, UiRect, ViewId,
    registry::{
        BoundedPayload, LeaseHolder, LeaseState, OverlayKind, PanelError, PanelId, PanelRegistry,
        PanelRegistryConfig, PanelRuntime, PanelState, PanelType, PlaceholderReason,
        RecoveryOutcome, WorkspaceId,
    },
};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const OVERLAY: &str = "panel.overlay";
const TOPIC: &str = "acme.owner:status";

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds")
}

fn host() -> PanelRuntime {
    PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds")
}

fn owner(name: &str) -> PluginId {
    PluginId::new(name).expect("valid plugin id")
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn empty() -> BTreeSet<String> {
    BTreeSet::new()
}

fn rect() -> UiRect {
    UiRect::new(0, 0, 20, 10)
}

/// One mounted panel with its recovery state in place, before any controller
/// has bound. Returns the panel id and the allocation epoch its handle carries
/// — the only generation a caller can mint before a controller binds.
fn mounted_panel(
    reg: &mut PanelRegistry,
    workspace: Option<WorkspaceId>,
    view: u64,
) -> (PanelId, Generation) {
    let handle = reg
        .create_panel(PanelType::Helper, workspace)
        .expect("create panel");
    reg.mount_panel(handle.id, handle.generation, ViewId::new(view))
        .expect("mount panel");
    (handle.id, handle.generation)
}

/// `PGO-1` / `PGO-22(b)`: a reload with a mounted panel keeps the container
/// (identity, attachment, state) and replaces the controller binding, reading
/// the predecessor's ceiling from the tombstone its generation edge wrote.
#[test]
fn a_reload_with_a_mounted_panel_keeps_the_container_and_rebinds_the_controller() {
    let mut reg = registry();
    let ws = WorkspaceId::new(3);
    let (panel, epoch) = mounted_panel(&mut reg, Some(ws), 11);

    // The panel's first bind is the `panel.create`-authorized one.
    let n = epoch.next().expect("controller generation");
    let first = reg
        .rebind_panel(
            panel,
            n,
            &owner("acme.notes"),
            &set(&[CREATE, FOCUS]),
            &empty(),
        )
        .expect("first bind");
    assert_eq!(first.predecessor_generation(), None);
    assert_eq!(first.grants(), &set(&[CREATE, FOCUS]));

    // Generation N ends: the reload edge releases its families and writes the
    // retired-ceiling tombstone before N+1 can bind.
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    assert_eq!(
        reg.retained_grant_ceiling(panel)
            .expect("the edge retains N's ceiling")
            .grants,
        set(&[CREATE, FOCUS])
    );

    // N+1 resolves one capability N never held. Without a consent the bind
    // commits `R ∩ P`, so `panel.overlay` is denied and nothing is inherited.
    let n1 = n.next().expect("successor generation");
    let committed = reg
        .rebind_panel(
            panel,
            n1,
            &owner("acme.notes"),
            &set(&[CREATE, FOCUS, OVERLAY]),
            &empty(),
        )
        .expect("rebind");

    assert_eq!(committed.predecessor_generation(), Some(n));
    assert_eq!(committed.controller_generation(), n1);
    assert_eq!(
        committed.grants(),
        &set(&[CREATE, FOCUS]),
        "the ceiling bounds the successor; the widening needs a consent"
    );
    assert_eq!(reg.controller_generation(panel), Some(n1));
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(n),
        "the displaced generation is retained for the next capture"
    );

    // Container facts survive: same id, same attachment, same state (D.3).
    assert_eq!(
        reg.panel_view(panel, epoch).expect("panel"),
        Some(ViewId::new(11))
    );
    assert_eq!(
        reg.panel_state(panel, epoch).expect("panel"),
        PanelState::Mounted
    );
    assert!(
        reg.retained_grant_ceiling(panel).is_none(),
        "a committed bind retires the tombstone it consumed"
    );
    assert_eq!(
        reg.error_count("StaleHandle"),
        0,
        "the panel itself never produces a stale handle across a reload"
    );

    // The predecessor's row is gone and the successor's is keyed by it.
    assert!(!reg.is_panel_capability_granted(panel, n, CREATE));
    assert!(reg.is_panel_capability_granted(panel, n1, CREATE));
    assert!(!reg.is_panel_capability_granted(panel, n1, OVERLAY));
}

/// `PGO-2`: a suspended panel stays suspended with its attachment, and the
/// successor rebinds without a mount and without moving focus.
#[test]
fn a_reload_with_a_suspended_panel_rebinds_without_a_mount_and_without_focus() {
    let mut reg = registry();
    let ws = WorkspaceId::new(4);
    let (panel, epoch) = mounted_panel(&mut reg, Some(ws), 12);
    reg.focus_panel(panel, epoch, ws).expect("focus panel");
    let n = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        n,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("first bind");

    reg.suspend_panel(panel, epoch).expect("suspend panel");
    let focus_before = reg.focused_panel(ws);
    let mru_before = reg.mru_order(ws);
    let view_before = reg.panel_view(panel, epoch).expect("panel");
    assert_eq!(
        reg.panel_state(panel, epoch).expect("panel"),
        PanelState::Suspended
    );

    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    let n1 = n.next().expect("successor generation");
    reg.rebind_panel(panel, n1, &owner("acme.notes"), &set(&[CREATE]), &empty())
        .expect("rebind");

    assert_eq!(
        reg.panel_state(panel, epoch).expect("panel"),
        PanelState::Suspended,
        "a rebind does not resume a panel the workspace suspended"
    );
    assert_eq!(
        reg.panel_view(panel, epoch).expect("panel"),
        view_before,
        "the attachment survives the rebind untouched"
    );
    assert_eq!(reg.focused_panel(ws), focus_before, "no focus change");
    assert_eq!(reg.mru_order(ws), mru_before, "no MRU change");
    assert!(reg.is_panel_capability_granted(panel, n1, CREATE));
}

/// `PGO-7`: at the instant the bind returns, the predecessor's overlay
/// attribution, queued events, and commands are gone and the successor's
/// surface is observably empty.
///
/// Capture handles have no registry-owned table — the content/capture family is
/// generation-scoped in the plugin host and released by `clear_ui_blocks` (D.4)
/// — so what this row can observe on this seam is the overlay attribution, the
/// panel queue, and the panel commands. The bind must not replay any of them,
/// which is what "observably empty rather than inferred" means here.
#[test]
fn an_active_overlay_and_queue_do_not_survive_the_rebind() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 13);
    let n = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        n,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("first bind");

    // An overlay, a queue, and a command owned by the generation that is about
    // to be displaced. The overlay is attributed under the allocation epoch
    // because that is the only key `create_panel_overlay` fences on.
    let overlay = reg
        .create_panel_overlay(panel, epoch, OverlayKind::NonModal, rect(), "live", None)
        .expect("attributed overlay");
    let topic = reg.declare_topic(TOPIC).expect("declare topic");
    reg.subscribe(panel, epoch, &topic).expect("subscribe");
    for payload in ["one", "two"] {
        reg.publish(&topic, BoundedPayload::try_new(payload).expect("payload"))
            .expect("publish");
    }
    reg.register_command(panel, epoch, "helper.run:go")
        .expect("register command");
    assert_eq!(reg.overlay_attribution_of(overlay), Some((panel, epoch)));
    assert_eq!(reg.bus_events_for_panel(panel), 2);
    assert_eq!(reg.command_owner("helper.run:go"), Some(panel));

    // A still-bound displacement: the bind itself must invalidate the
    // predecessor, because its generation edge has not run.
    let n1 = n.next().expect("successor generation");
    reg.rebind_panel(
        panel,
        n1,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("rebind");

    assert_eq!(reg.overlay_len(), 0, "no overlay survives a rebind");
    assert_eq!(reg.attributed_overlay_count(), 0);
    assert_eq!(
        reg.bus_events_for_panel(panel),
        0,
        "queues are not replayed"
    );
    assert_eq!(reg.command_owner("helper.run:go"), None);
    let released = reg
        .unbind_release_record(panel)
        .expect("phase 1 ran the release");
    assert_eq!(
        released.generation, n,
        "the record names the displaced generation"
    );
    assert_eq!(released.grants, 2);
    assert_eq!(released.events, 2);
    assert_eq!(released.commands, 1);
    // The record counts the generation-scoped pass only. The overlay above was
    // attributed under the allocation epoch — the only key the writer fences on
    // — so the bind's container-wide pass dismissed it and the generation's own
    // count does not claim it.
    assert_eq!(released.overlays, 0);
    assert_eq!(released.total(), 5);
}

/// Decision E rule 4: a removed plugin is `unloaded`, not deleted. The
/// container, its attachment, and its last identity facts stay, and a reinstall
/// rebinds from the ceiling the edge retained.
#[test]
fn a_removed_plugin_leaves_the_container_with_an_unloaded_placeholder() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 14);
    let n = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        n,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("first bind");

    // No successor generation exists: the plugin was uninstalled.
    let presented = reg
        .present_placeholder(panel, n, "unloaded")
        .expect("the unload edge");

    assert_eq!(
        presented.outcome,
        RecoveryOutcome::Placeholder(PlaceholderReason::Unloaded)
    );
    assert_eq!(
        reg.placeholder(panel).expect("placeholder").reason(),
        PlaceholderReason::Unloaded
    );
    assert_eq!(
        reg.panel_state(panel, epoch).expect("panel"),
        PanelState::Mounted,
        "the container is retained, not retired"
    );
    assert_eq!(
        reg.panel_view(panel, epoch).expect("panel"),
        Some(ViewId::new(14))
    );
    assert_eq!(
        reg.controller_generation(panel),
        None,
        "the panel is unbound"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, n, CREATE),
        "the unloaded generation's grants are released"
    );
    assert_eq!(
        reg.retained_grant_ceiling(panel)
            .expect("the unload edge retains the ceiling")
            .grants,
        set(&[CREATE, FOCUS])
    );

    // A reinstall rebinds from the retained ceiling and clears the placeholder.
    let n1 = n.next().expect("successor generation");
    reg.rebind_panel(
        panel,
        n1,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS, OVERLAY]),
        &empty(),
    )
    .expect("reinstall rebind");
    assert!(
        reg.placeholder(panel).is_none(),
        "the placeholder is cleared"
    );
    assert!(reg.is_panel_capability_granted(panel, n1, CREATE));
    assert!(
        !reg.is_panel_capability_granted(panel, n1, OVERLAY),
        "the reinstall inherits only what it resolves and the ceiling allows"
    );
}

/// `PGO-10`/`PGO-19`: a denied capability leaves the panel `unavailable` with
/// the denial named, accepts no verb, and produces exactly one record — the
/// second transition is refused instead of re-emitting.
#[test]
fn a_denied_plugin_presents_unavailable_and_stops_after_one_record() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 15);
    let n = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        n,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("first bind");
    reg.release_generation_scope(panel, n)
        .expect("generation edge");

    // The successor's own resolution no longer declares `panel.create`: the
    // bind succeeds on the intersection, and every content verb is then denied.
    let n1 = n.next().expect("successor generation");
    let committed = reg
        .rebind_panel(panel, n1, &owner("acme.notes"), &set(&[FOCUS]), &empty())
        .expect("rebind");
    assert_eq!(committed.grants(), &set(&[FOCUS]));
    assert!(matches!(
        reg.require_panel_capability(panel, n1, CREATE),
        Err(PanelError::CapabilityDenied { .. })
    ));

    let presented = reg
        .present_placeholder(panel, n1, "denied")
        .expect("the denial transition");
    assert_eq!(presented.outcome, RecoveryOutcome::Unavailable);
    assert_eq!(
        reg.placeholder(panel).expect("placeholder").reason(),
        PlaceholderReason::Denied
    );
    assert_eq!(reg.recovery_records(panel).len(), 1, "one FS-4 record");
    assert_eq!(
        reg.panel_state(panel, epoch).expect("panel"),
        PanelState::Mounted,
        "the container stays"
    );

    // Bounded recovery: the denied generation's own binding ended with that
    // transition, so a repeat is refused rather than counted again.
    assert!(matches!(
        reg.present_placeholder(panel, n1, "denied"),
        Err(PanelError::ControllerReleased { .. })
    ));
    assert_eq!(
        reg.recovery_records(panel).len(),
        1,
        "a refused repeat emits no second record and never respawns"
    );
    assert_eq!(reg.controller_generation(panel), None);
}

/// Restore after restart is a first bind: a fresh `PanelId` (PGO-12 refuses a
/// persisted id), no predecessor, no ceiling, and the whole resolution granted
/// because `panel.create` is the authorization. A pre-bind allocation-epoch row
/// is not a source and not an authority.
#[test]
fn a_restored_panel_is_a_first_bind_that_grants_its_whole_resolution() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 16);
    // A row written before any controller bound — the shape a restored session
    // that re-applied its grants would leave.
    reg.grant_panel_capability(panel, epoch, OVERLAY)
        .expect("pre-bind grant");

    let n = epoch.next().expect("controller generation");
    let capture = reg
        .capture_rebind_source(panel, n)
        .expect("phase 0 capture");
    assert!(capture.is_first_bind(), "no predecessor, no ceiling");
    assert!(capture.ceiling().is_none());
    assert_eq!(capture.attempt(), n);
    assert_eq!(capture.panel_id(), panel);

    let committed = reg
        .rebind_panel(
            panel,
            n,
            &owner("acme.notes"),
            &set(&[CREATE, FOCUS]),
            &empty(),
        )
        .expect("first bind");
    assert_eq!(
        committed.grants(),
        &set(&[CREATE, FOCUS]),
        "the first bind commits R whole with no permission-diff consent"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, n, OVERLAY),
        "the pre-bind row is not a source for the successor"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, epoch, OVERLAY),
        "the epoch row is unreadable once a controller generation keys the panel"
    );
    assert!(reg.retained_grant_ceiling(panel).is_none());

    // The next generation narrows silently and widens only with a consent.
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    let n1 = n.next().expect("successor generation");
    let narrowed = reg
        .rebind_panel(panel, n1, &owner("acme.notes"), &set(&[CREATE]), &empty())
        .expect("narrowing bind");
    assert_eq!(
        narrowed.grants(),
        &set(&[CREATE]),
        "a dropped capability is simply absent from R: no prompt, no refusal"
    );

    reg.release_generation_scope(panel, n1)
        .expect("generation edge");
    let n2 = n1.next().expect("successor generation");
    let widened = reg
        .rebind_panel(
            panel,
            n2,
            &owner("acme.notes"),
            &set(&[CREATE, OVERLAY]),
            &empty(),
        )
        .expect("unconsented widening");
    assert_eq!(
        widened.grants(),
        &set(&[CREATE]),
        "a widening without a permission-diff consent is denied"
    );

    reg.release_generation_scope(panel, n2)
        .expect("generation edge");
    let n3 = n2.next().expect("successor generation");
    let consented = reg
        .rebind_panel(
            panel,
            n3,
            &owner("acme.notes"),
            &set(&[CREATE, OVERLAY]),
            &set(&[OVERLAY]),
        )
        .expect("consented widening");
    assert_eq!(
        consented.grants(),
        &set(&[CREATE, OVERLAY]),
        "the consent admits the widening for this binding"
    );
    assert_eq!(reg.error_count("CeilingLost"), 0);
    assert_eq!(reg.error_count("CapabilityDenied"), 0);
}

/// `PGO-8`: a second generation racing for the same panel fails closed with the
/// typed conflict and changes nothing.
#[test]
fn a_racing_generation_is_refused_and_changes_nothing() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 17);
    let n = epoch.next().expect("controller generation");
    let won = reg
        .rebind_panel(
            panel,
            n,
            &owner("acme.notes"),
            &set(&[CREATE, FOCUS]),
            &empty(),
        )
        .expect("the winning bind");

    // The loser presents the same successor generation.
    let error = reg
        .rebind_panel(
            panel,
            n,
            &owner("evil.other"),
            &set(&[CREATE, FOCUS, OVERLAY]),
            &empty(),
        )
        .expect_err("a second bind of the same generation is refused");
    assert_eq!(
        error,
        PanelError::ControllerConflict {
            panel_id: panel,
            bound_generation: Some(n),
            presented_generation: n,
        }
    );

    // A loser that presents the allocation epoch — never a controller
    // generation (B.1/B.2) — is refused the same way.
    assert!(matches!(
        reg.rebind_panel(
            panel,
            epoch,
            &owner("evil.other"),
            &set(&[CREATE]),
            &empty()
        ),
        Err(PanelError::ControllerConflict { .. })
    ));

    assert_eq!(reg.controller_generation(panel), Some(n));
    assert!(
        reg.is_panel_capability_granted(panel, n, CREATE)
            && reg.is_panel_capability_granted(panel, n, FOCUS),
        "the recorded controller keeps the set its own bind committed"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, n, OVERLAY),
        "the refused widening never landed"
    );
    assert_eq!(won.grants(), &set(&[CREATE, FOCUS]));
}

/// Generation staleness fails closed before mutation: a generation whose own
/// binding ended cannot be rebound, and the record is left exactly as it was.
#[test]
fn a_retired_generation_cannot_be_rebound() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 18);
    let n = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        n,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("first bind");
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the edge retains the ceiling")
        .clone();

    let error = reg
        .rebind_panel(panel, n, &owner("acme.notes"), &set(&[CREATE]), &empty())
        .expect_err("a retired generation is not resurrected");
    assert_eq!(
        error,
        PanelError::ControllerReleased {
            panel_id: panel,
            released_generation: n,
        }
    );
    assert_eq!(reg.controller_generation(panel), None, "nothing was bound");
    assert_eq!(
        reg.retained_grant_ceiling(panel).cloned(),
        Some(ceiling),
        "the ceiling is untouched by the refusal"
    );
    assert_eq!(reg.error_count("ControllerReleased"), 1);
}

/// `PGO-22(c)`: an empty tombstone is a real ceiling, not an absent one.
#[test]
fn an_empty_tombstone_is_a_real_ceiling() {
    let mut reg = registry();
    let (panel, epoch) = mounted_panel(&mut reg, None, 19);
    let n = epoch.next().expect("controller generation");
    // N binds and holds nothing.
    let first = reg
        .rebind_panel(panel, n, &owner("acme.notes"), &empty(), &empty())
        .expect("first bind");
    assert!(first.grants().is_empty());
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("an ended generation that held nothing still leaves a ceiling");
    assert!(
        ceiling.grants.is_empty(),
        "the ceiling is empty, not absent"
    );

    let n1 = n.next().expect("successor generation");
    let committed = reg
        .rebind_panel(
            panel,
            n1,
            &owner("acme.notes"),
            &set(&[CREATE, FOCUS]),
            &empty(),
        )
        .expect("rebind");
    assert!(
        committed.grants().is_empty(),
        "empty P commits the empty set; a missing P would have granted R whole"
    );

    // The only way past an empty ceiling is a permission-diff consent.
    reg.release_generation_scope(panel, n1)
        .expect("generation edge");
    let n2 = n1.next().expect("successor generation");
    let consented = reg
        .rebind_panel(
            panel,
            n2,
            &owner("acme.notes"),
            &set(&[CREATE, FOCUS]),
            &set(&[CREATE]),
        )
        .expect("consented widening");
    assert_eq!(consented.grants(), &set(&[CREATE]), "C only");
}

/// D.2 phase 2, no-orphan: a rebind re-owns the one container record. The
/// displaced generation's release record is written once for the panel, a
/// repeated edge is a no-op, and no second `PanelId` or record is minted for
/// the successor.
///
/// The container is the thing a reload must not lose: a bind that left the old
/// record behind (or allocated a new one beside it) would either orphan an
/// attachment and its placement, or let two records address one view.
#[test]
fn a_rebind_re_owns_the_container_and_releases_the_old_generation_once() {
    let mut reg = registry();
    let ws = WorkspaceId::new(5);
    let (panel, epoch) = mounted_panel(&mut reg, Some(ws), 21);
    let n = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        n,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("first bind");

    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    assert_eq!(reg.unbind_release_record_count(), 1, "one record per panel");
    assert_eq!(
        reg.unbind_release_record(panel).expect("record").generation,
        n
    );
    assert_eq!(reg.unbind_release_count(panel), Some(2), "both grants");

    // The edge already ran: a repeat releases nothing and writes no record.
    assert!(
        !reg.release_generation_scope(panel, n).expect("repeat edge"),
        "the repeat has no live row to release"
    );
    assert_eq!(reg.unbind_release_record_count(), 1);
    assert_eq!(
        reg.unbind_release_count(panel),
        Some(2),
        "the release is not counted twice"
    );

    let n1 = n.next().expect("successor generation");
    reg.rebind_panel(
        panel,
        n1,
        &owner("acme.notes"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    )
    .expect("rebind");

    // One record, re-owned: same id, same attachment, same workspace, and a
    // controller generation that moved exactly once.
    assert_eq!(reg.panel_count(), 1, "no second record is minted");
    assert_eq!(reg.panels_for_workspace(ws), vec![panel]);
    assert_eq!(reg.controller_generation(panel), Some(n1));
    assert_eq!(
        reg.panel_view(panel, epoch).expect("panel"),
        Some(ViewId::new(21)),
        "the attachment moves with the container"
    );
    assert_eq!(
        reg.unbind_release_record_count(),
        1,
        "the bind re-owns the record instead of adding one"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, n, CREATE),
        "the displaced generation keeps nothing"
    );
    assert!(
        reg.is_panel_capability_granted(panel, n1, CREATE),
        "the successor holds the container"
    );
}

/// D.2 phase 2 step 3, on the facade: the commit resets the lease to `Idle` and
/// reports the displaced holder; a refused bind leaves the tenure alone.
#[test]
fn the_lease_returns_to_idle_on_commit_and_survives_a_refusal() {
    let mut runtime = host();
    let handle = runtime
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    runtime
        .mount_panel(handle.id, handle.generation, ViewId::new(20))
        .expect("mount panel");
    let epoch = handle.generation;
    let n = epoch.next().expect("controller generation");
    runtime
        .rebind_panel(
            handle.id,
            n,
            &owner("acme.notes"),
            &set(&[CREATE]),
            &empty(),
        )
        .expect("first bind");

    let holder = LeaseHolder(77);
    runtime
        .acquire_panel_lease(handle.id, epoch, holder, 30, 1_000)
        .expect("acquire lease");
    assert!(
        matches!(
            runtime
                .panel_lease_state(handle.id, epoch)
                .expect("lease state"),
            LeaseState::Occupied { holder: held, .. } if held == holder
        ),
        "the predecessor's holder holds the tenure before the rebind"
    );

    // A refused bind must not touch the tenure.
    assert!(matches!(
        runtime.rebind_panel(
            handle.id,
            epoch,
            &owner("acme.notes"),
            &set(&[CREATE]),
            &empty()
        ),
        Err(PanelError::ControllerConflict { .. })
    ));
    assert!(
        matches!(
            runtime
                .panel_lease_state(handle.id, epoch)
                .expect("lease state"),
            LeaseState::Occupied { holder: held, .. } if held == holder
        ),
        "a refused bind leaves the tenure exactly as it was"
    );

    let n1 = n.next().expect("successor generation");
    runtime
        .rebind_panel(
            handle.id,
            n1,
            &owner("acme.notes"),
            &set(&[CREATE]),
            &empty(),
        )
        .expect("rebind");
    assert_eq!(
        runtime
            .panel_lease_state(handle.id, epoch)
            .expect("lease state"),
        LeaseState::Idle,
        "the predecessor's tenure ends with its binding"
    );
    // The successor acquires its own tenure; nothing of the old one carries.
    runtime
        .acquire_panel_lease(handle.id, epoch, LeaseHolder(78), 30, 1_010)
        .expect("successor acquires");
}
