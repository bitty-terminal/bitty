#![forbid(unsafe_code)]
//! OQ-084 Decision F: the `PGO` matrix for panel generation ownership, on the
//! production rebind seam.
//!
//! The decision record is
//! `bitty-terminal-docs/specifications/panel-generation-ownership-decision.md`
//! (§"Decision F: required test matrix", a draft record, cited by repo-qualified
//! path because it does not live in this tree); bitty#2476 records the same
//! matrix. This file is the row home the record names for the rows the
//! **production** bind owns: residual 2 landed `PanelRegistry::rebind_panel` and
//! `PanelRuntime::rebind_panel`, so the adversarial rows below are driven
//! through the same entry points a generation swap will use, not through a
//! test-only binder.
//!
//! The `..._generation_edge_...` rows at the end drive the **plugin runtime's**
//! generation edge (`PluginRuntime::reload` / `PluginRuntime::dispose`,
//! residuals 2 and 4) with
//! [`PanelGenerationPort`](bitty_runtime::plugin_runtime::PanelGenerationPort)
//! installed, so the caller of the production bind is the edge and not the
//! test. Their port is the same eight-line delegation the composition root
//! installs (`crates/bitty-terminal/src/plugin_runtime.rs`), so what they pin is
//! the wiring: an edge that never calls the bind, or one that presents a
//! placeholder without the release, fails them.
//!
//! The two `pgo_4_...` / `pgo_20_...` rows drive the fifth edge instead: a
//! callback inside a live generation exhausts its own budget, the host records
//! the terminal verdict, and `PluginRuntime::rollback` -- the function a failed
//! activation also reaches -- ends the generation. That path is where a crash
//! is released, and it reaches the same installed port.
//!
//! # Coverage map
//!
//! This file does not restate rows another file already owns. The rows are:
//!
//! | Row | Home |
//! | --- | --- |
//! | PGO-1, PGO-22(b) | `tests/panel_rebind.rs`, `a_reload_with_a_mounted_panel_keeps_the_container_and_rebinds_the_controller` |
//! | PGO-2 | `tests/panel_rebind.rs`, `a_reload_with_a_suspended_panel_rebinds_without_a_mount_and_without_focus` |
//! | PGO-7 | `tests/panel_rebind.rs`, `an_active_overlay_and_queue_do_not_survive_the_rebind` |
//! | PGO-8 | `tests/panel_rebind.rs`, `a_racing_generation_is_refused_and_changes_nothing` |
//! | PGO-10, PGO-19 | `tests/panel_rebind.rs`, the denied-capability reload |
//! | PGO-12 | `tests/panel_rebind.rs`, `a_restored_panel_is_a_first_bind_that_grants_its_whole_resolution` |
//! | PGO-14 | `tests/panel_placeholder_bounds.rs:65` |
//! | PGO-16 | `tests/panel_rebind.rs` (the fail-closed tombstone write) and `registry::panel::rebind_panel_tests` |
//! | PGO-17, PGO-18 | `tests/panel_rebind.rs`, the consent / narrowing reload |
//! | PGO-22(a) | `tests/panel_grant_ceiling.rs:347` (still-bound half) |
//! | PGO-22(c) | `tests/panel_rebind.rs`, `an_empty_tombstone_is_a_real_ceiling` |
//! | PGO-22(d), generation mismatch | `tests/panel_grant_ceiling.rs` and `registry::panel::grant_ceiling_tests` |
//! | PGO-22(d), owner mismatch | here — `a_bind_by_another_owner_is_refused_as_a_lost_ceiling`; the tombstone-side twin (a tombstone stamped under a foreign owner while the record names another) can only be built by violating the table invariant, so it stays in `registry::panel::grant_ceiling_tests` |
//! | PGO-23 | `tests/panel_grant_ceiling.rs:182` (disposal drops the tombstone), `tests/panel_unbind_release.rs` (the four released families), `crates/bitty-ui/src/panel_recovery.rs` (the placeholder reasons) |
//! | PGO-23, edge-driven (bitty#2476 residual 4) | here — `a_generation_edge_with_no_successor_presents_the_placeholder_after_the_release` |
//! | bitty#2476 AC-3, capture family at the edge | here — `the_generation_edge_records_the_capture_session_it_revoked` (the plugin runtime's own `dispose_inner` revokes the session) and `a_generation_edge_without_a_capture_records_none` (the negative half); the registry-side row is `tests/panel_unbind_release.rs` |
//! | PGO-4, crash-driven (bitty#2476 residual 4) | here — `pgo_4_a_crash_inside_a_callback_presents_crashed_and_contains_the_damage`; the crash is a callback stopped by the generation's own RC-1 instruction budget, so the producer of `crashed` is `PluginRuntime::rollback` (`src/plugin_runtime/mod.rs:5109`, port call at `:5155`), not the test |
//! | PGO-20, crash arm (bitty#2476 residual 4) | here — `pgo_20_a_crashed_generation_is_bounded_and_loses_its_authority_at_the_next_bind`; the disable arm is **not** homed, see the blocking note below |
//! | PGO-1/PGO-2, edge-driven (bitty#2476 residual 2) | here — `the_generation_edge_rebinds_the_container_and_moves_the_controller` |
//! | Residual 7, per-owner retention | here — `a_tombstone_is_retained_for_the_owner_that_stamped_it` |
//! | Residual 7(a), no owner on an epoch edge (bitty#2476) | here — `an_allocation_epoch_edge_retains_no_owner_from_the_previous_edge`, `a_bind_after_an_allocation_epoch_edge_is_not_measured_against_the_previous_owner` |
//! | PGO-22(d) through the edge, no widening | here — `a_refused_edge_bind_leaves_the_container_unbound_without_widening_its_grants` |
//! | PGO-6, reload racing a mount (bitty#2476 residual 9) | here — `a_mount_that_races_a_reload_commits_nothing_and_the_successor_mounts_cleanly` |
//! | PGO-9, capability gate on a guessed `PanelId` (bitty#2476 residual 9) | here — `a_guessed_panel_id_is_denied_at_the_grant_gate_before_any_state_access`; the deny-by-default half is pinned by `tests/panel_rebind.rs:412`, `a_denied_plugin_presents_unavailable_and_stops_after_one_record` (a capability outside the committed resolution is refused at `require_panel_capability`) |
//! | PGO-13, disposal reclaim (bitty#2476 residual 9) | here — `a_disposal_reclaims_every_registry_owned_family_to_the_pre_creation_baseline` (queue, commands, MRU and focus, overlays, tombstone, counts, read with a second container left in the workspace so the MRU entry is observable) and `a_disposal_releases_the_lease_entry_and_the_placement_mirror` (lease, attachment). The live **grant row** is not covered by either: no accessor reads it once the record is gone — see the first row's rustdoc for what that would take |
//! | PGO-15, refusal half (bitty#2476 residual 9) | here — `pgo_15_a_verb_from_a_displaced_generation_is_refused_and_moves_nothing`; the read half is **not** homed, see the blocking note below |
//! | PGO-1, container-fact half through the reload edge (bitty#2476 residual 9) | here — `pgo_1_container_facts_survive_the_rebind_edge` (attachment, lifecycle state, title across the plugin-runtime edge); the controller half is `tests/panel_rebind.rs`. The `PresentationMode` clause is **not** homed, see the blocking note below |
//! | `N -> N+1 -> N` retired generation | here — `a_retired_generation_cannot_be_rebound_over_its_successor` |
//!
//! Still without a row in this crate: PGO-5, the disable arm of PGO-20
//! (`disabled` on a live generation, then a re-enable that rebinds), PGO-11
//! (provider version records), PGO-15's read half (a displaced generation
//! reading its successor's queued events), and PGO-1's `PresentationMode` clause
//! (a panel's presentation mode surviving the swap). Each is named here with the
//! seam that blocks it rather than implied covered:
//!
//! - **PGO-5, PGO-20 disable arm** (`disabled` placeholder, then a re-enable
//!   that rebinds): the reason token has exactly one production producer —
//!   `dispose_inner` called from `recover_wall_failed_plugins`
//!   (`src/plugin_runtime/mod.rs:3934`, branch at `:3970`, dispose at `:3974`)
//!   — and that pass only selects a generation that is `Failed` with
//!   `FailureClass::WallSliceTimeout`. Nothing else turns a *live* generation
//!   into a `disabled` one: `PluginRuntime` has no disable entry (`suspend`
//!   parks a generation and asks the port for nothing, and `dispose` names
//!   `unloaded`), `package::set_enabled` (`src/plugin_runtime/package.rs:1441`) only writes
//!   the store index, `discover_inner` skips a disabled record without touching
//!   a live entry (`mod.rs:2852`), and the application classes a plugin-set
//!   change as `restart-required` because "the plugin set is not part of the
//!   config plan and has no live apply path"
//!   (`crates/bitty-terminal/src/config_reload.rs:128`). A test that produced
//!   `disabled` therefore has to invent the disable, and the only route that
//!   exists (crash first, then disable the index) reaches the release with the
//!   containers already unbound by the crash edge, so `disabled` cannot be
//!   presented on a container this generation still holds. Homing the row needs
//!   a live disable entry and a live enable/rebind path in the plugin host, not
//!   a test-side reason token; `PGO-4`/`PGO-20`'s crash arm cover the fault
//!   direction that *is* reachable.
//! - **PGO-11** (provider version records): no production path presents
//!   `incompatible` — `PlaceholderReason::Incompatible` has no producer
//!   (`dispose_inner` passes `Unloaded`/`Disabled`/`Reloaded`, `rollback` passes
//!   `Crashed`, and `PanelRegistry::present_placeholder` takes the reason token
//!   from its caller), and the only versioned provider record,
//!   `PanelProviderManifest`, refuses every version other than 1 at
//!   `registry/provider.rs:138`, so there is no newer-provider-over-older-record
//!   direction and no tolerant reader to drive. The persisted panel manifest
//!   (`bitty-ui/src/panel_persist.rs:367`) has a version gate and unknown-field
//!   tolerance, but it carries identity, not provider content.
//! - **PGO-15's read half** (a displaced generation reading its successor's
//!   state): the refusal half is homed above, but the read half has no seam.
//!   The panel-scoped bus (`PanelEventBus::queued_events_for_panel`,
//!   `registry/panel.rs:1224`) and the panel scene slot
//!   (`PanelRuntime::panel_scene`, `registry/host.rs:1217`) are keyed by panel
//!   alone, so no accessor distinguishes "the queued events generation `N` may
//!   read" from "the ones `N+1` may read" and no refusal exists to test. The
//!   row's "refused with `ControllerConflict`" wording is also already known to
//!   be narrower than the tree: the displaced-but-released generation is
//!   answered `ControllerReleased`, which the record carries as finding (d)
//!   rather than as a decided rule. Homing the read half needs a generation key
//!   on those families, not a test.
//! - **PGO-1's `PresentationMode` clause** (a panel's presentation mode survives
//!   the generation swap): the mode lives on the leaf `View`
//!   (`crates/bitty-ui/src/view.rs:227`) inside the layout `Runtime` holds, and
//!   the panel host cannot reach that layout. `PanelRuntime` owns a `Placement`
//!   mirror and a lease table, no `LayoutNode`
//!   (`crates/bitty-runtime/src/registry/host.rs:115`); its record carries a
//!   `ViewId` attachment (`registry/panel.rs:607`), not a leaf. The production
//!   rebind writes only the controller binding, the lease tenure, and the
//!   retired-ceiling tombstone (`registry/host.rs:452`), which is why its own
//!   contract states the presentation facts survive *because nothing there
//!   presents* (`:435`). The rebind's only production caller,
//!   `AppPanelGenerationPort` (`crates/bitty-terminal/src/plugin_runtime.rs:3548`,
//!   `:3660`), reads back `placement_view_of`, never a mode; and the rule mapper
//!   `presentation_for` (`runtime/panel_rules.rs:50`) has no production caller
//!   either — the spawn path stamps the rule's mode inline
//!   (`runtime/panes.rs:251`, `:733`). A mode read after a reload is therefore
//!   unchanged by construction: a row over a test-built leaf stays green with
//!   `runtime.reload` deleted, which is what a reviewer measured before this
//!   note replaced the claim. Homing the clause needs a production consumer of a
//!   panel leaf's mode across the rebind — a re-present step in the edge, or an
//!   accessor answering panel → leaf mode — not a test.
//!
//! Naming them here keeps the matrix honest about what it covers instead of
//! implying all twenty-three rows live in one file.
//!
//! # What these rows pin
//!
//! A retired-ceiling tombstone is keyed `(owner, PanelId)` (OQ-084 A.2, `N-13`):
//! two plugins can carry the same controller generation number, so the owner is
//! part of the key, the record retains the predecessor owner beside the
//! predecessor generation, and a tombstone whose owner does not match the
//! predecessor is refused exactly like a missing one (PGO-22(d)). A generation
//! whose binding ended is not resurrected, whether or not its successor is
//! bound.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PanelEdgeBind, PanelEdgeRelease, PanelEdgeReport, PanelGenerationPort,
    PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource, controller_generation,
};
use bitty_runtime::{
    Generation, UiRect, ViewId,
    registry::{
        BoundedPayload, LeaseHolder, LeaseState, OverlayKind, PanelError, PanelId, PanelRegistry,
        PanelRegistryConfig, PanelRuntime, PanelType, PlaceholderReason, WorkspaceId,
    },
};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const OVERLAY: &str = "panel.overlay";

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

/// One mounted panel and the controller generation its first bind will use. The
/// panel is created and mounted, so the container facts (id, attachment, epoch)
/// exist before any controller does — the state a generation swap starts from.
fn mounted_panel(reg: &mut PanelRegistry, view: u64) -> (PanelId, Generation, Generation) {
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    reg.mount_panel(handle.id, handle.generation, ViewId::new(view))
        .expect("mount panel");
    let controller = handle.generation.next().expect("controller generation");
    (handle.id, handle.generation, controller)
}

/// Residual 7 (OQ-084 A.2, `N-13`): the tombstone is stamped with the owner of
/// the binding the edge ends, retained with the generation, and is the ceiling
/// the *same* owner's successor reads.
///
/// Fails for an implementation that stamps the owner after the binding is
/// released (it can only read `None` then), for one that keys the table by panel
/// alone, and for one whose successor ignores the retained set.
#[test]
fn a_tombstone_is_retained_for_the_owner_that_stamped_it() {
    let mut reg = registry();
    let (panel, _epoch, n) = mounted_panel(&mut reg, 31);
    let acme = owner("acme.notes");
    reg.rebind_panel(panel, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("first bind");

    reg.release_generation_scope(panel, n)
        .expect("generation edge");

    // The pair the record retains is what phase 0 will later compare a
    // tombstone against, so both halves must survive the binding release.
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(n),
        "the ended generation is retained"
    );
    assert_eq!(
        reg.last_controller_owner(panel),
        Some(&acme),
        "the ended generation's owner is retained beside it, not read from the \
         binding the release just cleared"
    );
    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the edge writes the tombstone");
    assert_eq!(
        ceiling.owner,
        Some(acme.clone()),
        "A.2: the tombstone is keyed by the binding pair, so it carries the owner"
    );
    assert_eq!(ceiling.ended_generation, n);
    assert_eq!(ceiling.grants, set(&[CREATE, FOCUS]));

    // The same owner's successor consumes exactly that ceiling: `P` can only
    // subtract, so the family the predecessor never held needs a consent.
    let n1 = n.next().expect("successor generation");
    let capture = reg
        .capture_rebind_source(panel, n1)
        .expect("phase 0 reads the retained pair");
    assert_eq!(capture.predecessor_generation(), Some(n));
    assert_eq!(
        capture.predecessor_owner(),
        Some(&acme),
        "provenance names the owner the tombstone was stamped with"
    );
    let committed = reg
        .rebind_panel(panel, n1, &acme, &set(&[CREATE, OVERLAY]), &empty())
        .expect("same-owner successor bind");
    assert_eq!(
        committed.grants(),
        &set(&[CREATE]),
        "the retained per-owner ceiling bounds the successor"
    );
    assert_eq!(reg.retained_grant_ceiling_count(), 1);

    // D.2 phase 2 step 3 retires it, so the table holds no row per reload.
    assert!(
        reg.retire_rebound_ceiling(panel),
        "a committed bind retires the ceiling it read"
    );
    assert!(reg.retained_grant_ceiling(panel).is_none());
    assert_eq!(reg.retained_grant_ceiling_count(), 0);
}

/// Residual 7 / PGO-22(d): a bind presenting another owner than the
/// predecessor's is refused as a lost ceiling — the tombstone the record names
/// was written for a different plugin, which is what `N-13` forbids consuming.
///
/// Fails for an implementation that keys the tombstone by panel alone (the
/// cross-owner bind would succeed and inherit `acme.notes`'s set) and for one
/// that refuses after phase 1 ran (the predecessor would be invalidated).
#[test]
fn a_bind_by_another_owner_is_refused_as_a_lost_ceiling() {
    let mut reg = registry();
    let (panel, _epoch, n) = mounted_panel(&mut reg, 32);
    let acme = owner("acme.notes");
    reg.rebind_panel(panel, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("first bind");
    reg.release_generation_scope(panel, n)
        .expect("generation edge");

    let n1 = n.next().expect("successor generation");
    let error = reg
        .rebind_panel(
            panel,
            n1,
            &owner("evil.other"),
            &set(&[CREATE, FOCUS]),
            &empty(),
        )
        .expect_err("another plugin's generation may not adopt this ceiling");

    assert_eq!(
        error,
        PanelError::CeilingLost {
            panel_id: panel,
            bind_generation: n1,
            last_controller_generation: Some(n),
            tombstone_generation: Some(n),
        },
        "the refusal names the predecessor the record retains"
    );
    // A.2: refused before phase 1 and before phase 2, so the container keeps
    // its binding-less state, installs nothing, and keeps the tombstone.
    assert_eq!(reg.controller_generation(panel), None);
    assert!(
        !reg.is_panel_capability_granted(panel, n1, CREATE),
        "no successor row is installed"
    );
    assert!(
        reg.retained_grant_ceiling(panel).is_some(),
        "the foreign bind does not consume the ceiling it may not read"
    );
    assert_eq!(reg.error_count("CeilingLost"), 1);

    // The owner that wrote it still binds, so the refusal is not a wedge.
    let adopted = reg
        .rebind_panel(panel, n1, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("the predecessor's own owner still binds");
    assert_eq!(adopted.grants(), &set(&[CREATE, FOCUS]));
}

/// Residual 7(a)/7(f) (bitty#2476): an edge that ends the allocation epoch ends
/// no bound controller, so it stamps neither half of the predecessor pair and
/// leaves the controller's pair — and the tombstone stamped with it — exactly as
/// that edge wrote them.
///
/// The sequence is the ordinary one on a container whose controller left: the
/// controller generation's edge stamps `acme.notes`, then a writer that holds no
/// controller (the epoch is the binding key while nothing is bound) reaches a
/// second edge under the allocation epoch. `PanelRecord::last_controller_owner`
/// belongs to "the plugin that owned the most recently ended controller
/// generation", and the epoch is never a controller generation (B.1/B.2), so the
/// second edge has no predecessor fact of its own: it writes neither half.
/// Everything else the epoch edge owes still runs — the epoch-keyed row, the
/// queued families, and the release record.
///
/// Fails for an implementation that copies the record's retained owner forward
/// onto its own generation (`bound_owner.or(last_ended_owner)`): the pair then
/// reads `(acme.notes, epoch)`, an owner from one edge beside a generation from
/// another, which is the phantom predecessor `N-13` keys the tombstone against.
/// Fails as well for one that overwrites the pair with `(None, epoch)` and
/// replaces the controller's tombstone with the epoch's: the panel becomes
/// nobody's ended panel and its own successor can no longer bind it.
#[test]
fn an_allocation_epoch_edge_keeps_the_controller_predecessor_pair() {
    let mut reg = registry();
    let (panel, epoch, n) = mounted_panel(&mut reg, 34);
    let acme = owner("acme.notes");
    reg.rebind_panel(panel, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("first bind");
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    assert_eq!(
        reg.last_controller_owner(panel),
        Some(&acme),
        "the controller's own edge stamps the owner of the binding it ends"
    );

    // While nothing is bound the record's binding key is the allocation epoch,
    // so this edge ends the epoch and no binding. The grant is the shape a
    // writer without a controller leaves: it is keyed by the epoch.
    reg.grant_panel_capability(panel, epoch, FOCUS)
        .expect("an unbound container's row is keyed by the epoch");
    assert!(
        reg.release_generation_scope(panel, epoch)
            .expect("allocation-epoch edge"),
        "the epoch's own row is still released"
    );

    assert_eq!(
        reg.last_controller_owner(panel),
        Some(&acme),
        "the pair still names the controller generation that ended a binding, \
         so both halves come from that one edge and never from the epoch edge"
    );
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(n),
        "the epoch is never a controller generation, so it does not replace the \
         ended generation"
    );
    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the controller's tombstone survives the epoch edge");
    assert_eq!(
        ceiling.ended_generation, n,
        "the retained ceiling still belongs to the controller generation the \
         pair names, not to the epoch the epoch edge released"
    );
    assert_eq!(
        ceiling.owner,
        Some(acme.clone()),
        "and it is keyed under that generation's owner"
    );
    assert_eq!(
        ceiling.grants,
        set(&[CREATE, FOCUS]),
        "the epoch edge's row is released, not copied over the controller's \
         ceiling: the epoch's set was never a controller's"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, epoch, FOCUS),
        "the epoch edge still releases the family it owns"
    );
    assert!(
        reg.unbind_release_count(panel).is_some(),
        "the epoch edge still writes its release record"
    );

    // A repeat of the epoch edge is the same edge, not a second one, and it
    // rewrites neither half: the release record the first pass wrote is what
    // recognises it, because a kept controller predecessor leaves no tombstone
    // of the epoch edge's own to name it.
    assert!(
        !reg.release_generation_scope(panel, epoch)
            .expect("the epoch edge's repeat"),
        "the repeat releases nothing"
    );
    assert_eq!(reg.last_controller_owner(panel), Some(&acme));
    assert_eq!(reg.last_controller_generation(panel), Some(n));
}

/// Residual 7(f) (bitty#2476, OQ-084 A.2/`N-13`): an edge that ends the
/// allocation epoch ends no controller binding, so the generation edge's
/// successor bind still finds — and can still rebind — the container through the
/// predecessor pair the controller edge left behind.
///
/// [`PanelRegistry::panels_ended_at`] is the only query that still names the
/// ended generation once the release cleared the binding, and the generation
/// edge's successor bind enumerates exactly that set (`bind_successor` in
/// `crates/bitty-terminal/src/plugin_runtime.rs`), so a container the epoch edge
/// drops from it is never rebound. The pair, the ceiling it is keyed with, and
/// the container's bindability are one fact: whatever survives has to be all
/// three.
///
/// Fails for the pre-fix code: the epoch edge overwrites the pair with
/// `(None, epoch)`, so the accessors name the epoch instead of the controller
/// generation, `panels_ended_at` answers nothing for that generation, and the
/// successor's bind is refused `CeilingLost` against the epoch tombstone the
/// edge wrote over the controller's.
#[test]
fn an_allocation_epoch_edge_leaves_the_container_enumerable_for_its_successor() {
    let mut reg = registry();
    let (panel, epoch, n) = mounted_panel(&mut reg, 36);
    let acme = owner("acme.notes");
    reg.rebind_panel(panel, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("first bind");
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    reg.grant_panel_capability(panel, epoch, FOCUS)
        .expect("an unbound container's row is keyed by the epoch");
    reg.release_generation_scope(panel, epoch)
        .expect("allocation-epoch edge");

    // The generation edge's enumeration: the binding is gone, so the retained
    // pair is the answer, and the two queries stay complementary.
    assert_eq!(
        reg.controller_generation(panel),
        None,
        "the release left the container unbound"
    );
    assert_eq!(
        reg.panels_bound_to(&acme, n),
        Vec::<PanelId>::new(),
        "a released binding is not a live binding"
    );
    assert_eq!(
        reg.panels_ended_at(&acme, n),
        vec![panel],
        "the container is still the ended controller generation's, so the \
         successor bind enumerates it"
    );

    // The successor's own bind, at the generation the edge would hand it.
    let n1 = n.next().expect("successor generation");
    let bound = reg
        .rebind_panel(panel, n1, &acme, &set(&[CREATE, OVERLAY]), &empty())
        .expect("the successor bind is accepted");
    assert_eq!(
        bound.predecessor_generation(),
        Some(n),
        "provenance names the controller generation that actually ended"
    );
    assert_eq!(
        bound.grants(),
        &set(&[CREATE]),
        "and the bind is bounded by that generation's ceiling"
    );
    assert_eq!(reg.controller_generation(panel), Some(n1));
    assert_eq!(
        reg.panels_bound_to(&acme, n1),
        vec![panel],
        "the container moved to the successor's live binding"
    );
    assert!(
        reg.panels_ended_at(&acme, n).is_empty(),
        "a bound container is no longer an ended one"
    );
}

/// Residual 7(a)/7(f) (bitty#2476): the pair an epoch edge leaves in place names
/// the controller that really ended, so the bind after it is measured against
/// that controller — its own owner's successor reads the ceiling the predecessor
/// held, and another plugin is refused exactly as it would be without the epoch
/// edge.
///
/// The pair is one fact (`N-13`): an owner no generation ever had is what `7(a)`
/// forbids, and it is what the pre-`7(f)` epoch edge wrote. The record named
/// `(None, epoch)`, so the successor's bind read the epoch's tombstone and
/// inherited a set no controller ever held, and the plugin whose controller
/// ended could not be handed its own predecessor's ceiling.
///
/// Fails for the pre-fix code: `last_controller_owner` no longer names
/// `acme.notes` after the epoch edge, so `beta.tasks`'s bind is accepted against
/// the epoch ceiling `{panel.focus}` while `acme.notes`'s successor inherits
/// that same set instead of `{panel.create, panel.focus}`.
#[test]
fn a_bind_after_an_allocation_epoch_edge_reads_the_controller_it_names() {
    let mut reg = registry();
    let (panel, epoch, n) = mounted_panel(&mut reg, 35);
    let acme = owner("acme.notes");
    reg.rebind_panel(panel, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("first bind");
    reg.release_generation_scope(panel, n)
        .expect("generation edge");
    reg.grant_panel_capability(panel, epoch, FOCUS)
        .expect("an unbound container's row is keyed by the epoch");
    reg.release_generation_scope(panel, epoch)
        .expect("allocation-epoch edge");

    let n1 = n.next().expect("successor generation");
    let capture = reg
        .capture_rebind_source(panel, n1)
        .expect("phase 0 reads the pair the epoch edge kept");
    assert_eq!(capture.predecessor_generation(), Some(n));
    assert_eq!(
        capture.predecessor_owner(),
        Some(&acme),
        "provenance names the owner of the pair the epoch edge left in place"
    );

    // Another plugin may not adopt a ceiling a controller's edge wrote, epoch
    // edge or not: the fence reads the retained pair, which names `acme.notes`.
    let beta = owner("beta.tasks");
    assert_eq!(
        reg.rebind_panel(panel, n1, &beta, &set(&[CREATE, FOCUS]), &empty())
            .expect_err("another plugin's generation may not adopt this ceiling"),
        PanelError::CeilingLost {
            panel_id: panel,
            bind_generation: n1,
            last_controller_generation: Some(n),
            tombstone_generation: Some(n),
        }
    );
    assert_eq!(
        reg.controller_generation(panel),
        None,
        "the refusal commits nothing"
    );
    assert_eq!(reg.error_count("CeilingLost"), 1);

    // The predecessor's own owner still binds, bounded by the ceiling its
    // predecessor held — not by the epoch row the epoch edge released.
    let bound = reg
        .rebind_panel(panel, n1, &acme, &set(&[CREATE, OVERLAY]), &empty())
        .expect("the predecessor's own owner still binds");
    assert_eq!(bound.predecessor_generation(), Some(n));
    assert_eq!(bound.grants(), &set(&[CREATE]));
}

/// `N -> N+1 -> N`: a retired generation is not rebound while its successor is
/// bound.
///
/// The generation whose binding ended had its scene, grants, capture handles,
/// and queued events released in phase 1; presenting it again must be refused
/// rather than re-own the container. Flagged by the review of bitty#3128, where
/// the fence was scoped to `bound.is_none()` and therefore missed exactly this
/// sequence. Driven through the facade, because D.2 phase 2 step 3 (lease reset,
/// then tombstone drop) is the facade's half of the bind.
///
/// Fails for the un-scoped fence (`bound.is_none() && last == Some(successor)`):
/// the third bind would succeed and the retired generation would own the panel
/// again.
#[test]
fn a_retired_generation_cannot_be_rebound_over_its_successor() {
    let mut runtime = host();
    let handle = runtime
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    runtime
        .mount_panel(handle.id, handle.generation, ViewId::new(33))
        .expect("mount panel");
    let epoch = handle.generation;
    let n = epoch.next().expect("controller generation");
    let acme = owner("acme.notes");

    runtime
        .rebind_panel(handle.id, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("first bind");
    let n1 = n.next().expect("successor generation");
    runtime
        .rebind_panel(handle.id, n1, &acme, &set(&[CREATE]), &empty())
        .expect("successor bind");

    // `N` ended when `N+1` displaced it: `last_controller_generation` names it
    // while `N+1` is the bound controller.
    assert_eq!(
        runtime.registry().last_controller_generation(handle.id),
        Some(n)
    );
    assert_eq!(
        runtime.registry().controller_generation(handle.id),
        Some(n1)
    );

    let error = runtime
        .rebind_panel(handle.id, n, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect_err("a retired generation is not resurrected");
    assert_eq!(
        error,
        PanelError::ControllerReleased {
            panel_id: handle.id,
            released_generation: n,
        }
    );

    // Nothing moved: the successor is still the controller and still holds the
    // set its own bind committed.
    assert_eq!(
        runtime.registry().controller_generation(handle.id),
        Some(n1)
    );
    assert!(
        runtime
            .registry()
            .is_panel_capability_granted(handle.id, n1, CREATE),
        "the bound successor keeps its row"
    );
    assert!(
        !runtime
            .registry()
            .is_panel_capability_granted(handle.id, n1, FOCUS),
        "the refused bind installs nothing for anyone"
    );

    // The refusal does not wedge the panel: the next generation binds normally.
    let n2 = n1.next().expect("third generation");
    let next = runtime
        .rebind_panel(handle.id, n2, &acme, &set(&[CREATE, FOCUS]), &empty())
        .expect("the next generation still binds");
    assert_eq!(next.predecessor_generation(), Some(n1));
    assert_eq!(
        next.grants(),
        &set(&[CREATE]),
        "the live row of the bound successor is the ceiling"
    );
}

// ---------------------------------------------------------------------------
// bitty#2476 residuals 2 and 4: the plugin generation edge
// ---------------------------------------------------------------------------
//
// Everything below runs the *edge*, not the bind: `PluginRuntime::reload` and
// `PluginRuntime::dispose` with a `PanelGenerationPort` installed, exactly as
// the composition root installs one. The port is the same delegation
// `crates/bitty-terminal/src/plugin_runtime.rs` performs — enumerate the
// containers the generation holds, then call the production entry point — so
// these rows fail if the runtime stops calling the port, or if the port is
// called with the wrong generation, or if a refused bind means "partially
// rebound" anywhere.

/// Empty settings; the channel exists so `PluginRuntimeConfig` can be built.
#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

/// Minimal committed-snapshot source.
struct StaticSnapshot;

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(LuaValue::table([("version", LuaValue::Integer(1))]))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-pgo-edge-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_plugin(root: &Path, id: &str, capabilities: &[&str]) {
    write_plugin_with(root, id, capabilities, "", "bitty = bitty or {}\n");
}

/// Write one development package.
///
/// `extra_manifest` is appended after the `[capabilities]` table (a trailing
/// section such as `[limits]`), and `init` becomes `lua/init.lua`. The plain
/// edge fixtures pass nothing and a one-line body, so their packages are the
/// ones they always were.
fn write_plugin_with(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    extra_manifest: &str,
    init: &str,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let caps = capabilities
        .iter()
        .map(|capability| format!("{capability} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Panel edge fixture"
version = "0.1.0"
description = "panel generation edge"

[compat]
plugin-api = "^1.0"

[capabilities]
{caps}
{extra_manifest}"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init).expect("init");
}

/// The port the composition root installs, reduced to the two delegations it
/// makes: enumerate the containers the generation holds, then call the
/// production panel entry point for each.
struct EdgePort {
    panels: Rc<RefCell<PanelRuntime>>,
}

impl PanelGenerationPort for EdgePort {
    fn release_generation(&self, edge: PanelEdgeRelease<'_>) -> PanelEdgeReport {
        let ended = controller_generation(edge.ended);
        let bound = {
            let host = self.panels.borrow();
            host.registry().panels_bound_to(edge.owner, ended)
        };
        let mut report = PanelEdgeReport::default();
        for panel in bound {
            report.attempted += 1;
            // The same report the composition root makes before it releases the
            // container (bitty#2476 AC-3): the capture family is the plugin
            // runtime's, so the port hands it to the registry here.
            if edge.capture_revoked {
                self.panels
                    .borrow_mut()
                    .note_capture_release(panel, ended)
                    .expect("the generation still binds the container it is releasing");
            }
            match self
                .panels
                .borrow_mut()
                .present_placeholder(panel, ended, edge.reason.as_str())
            {
                Ok(_) => report.committed.push(panel),
                Err(error) => report.refused.push((panel, error)),
            }
        }
        report
    }

    fn bind_successor(&self, edge: PanelEdgeBind<'_>) -> PanelEdgeReport {
        let predecessor = controller_generation(edge.predecessor);
        let successor = controller_generation(edge.successor);
        let held = {
            let host = self.panels.borrow();
            let mut held = host.registry().panels_bound_to(edge.owner, predecessor);
            held.extend(host.registry().panels_ended_at(edge.owner, predecessor));
            held
        };
        let mut report = PanelEdgeReport::default();
        for panel in held {
            report.attempted += 1;
            match self.panels.borrow_mut().rebind_panel(
                panel,
                successor,
                edge.owner,
                edge.requested,
                edge.consented,
            ) {
                Ok(committed) => report.committed.push(committed.panel_id()),
                Err(error) => report.refused.push((panel, error)),
            }
        }
        report
    }
}

/// The closed `panel` family, in vocabulary order (OQ-084 A.3).
const PANEL_FAMILY: [&str; 4] = [CREATE, FOCUS, "panel.provider", OVERLAY];

fn granted_set(
    registry: &PanelRegistry,
    panel: PanelId,
    generation: Generation,
) -> BTreeSet<String> {
    PANEL_FAMILY
        .iter()
        .filter(|capability| registry.is_panel_capability_granted(panel, generation, capability))
        .map(|capability| (*capability).to_string())
        .collect()
}

/// An activated local-path plugin plus its panel host and the installed edge
/// port. The plugin's first activation is generation 1, so the first controller
/// generation is [`controller_generation`]`(1)`.
fn edge_fixture(tag: &str, id: &str) -> (PluginRuntime, PluginId, Rc<RefCell<PanelRuntime>>) {
    let root = temp_dir(tag);
    write_plugin(&root, id, &["panel.create", "panel.focus"]);
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(temp_dir(&format!("{tag}-data"))),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    });
    runtime.discover();
    let plugin = owner(id);
    runtime.activate(&plugin).expect("activate fixture");
    let panels = Rc::new(RefCell::new(host()));
    runtime.set_panel_generation_port(Some(Rc::new(EdgePort {
        panels: Rc::clone(&panels),
    })));
    (runtime, plugin, panels)
}

/// Creates, mounts, describes, and first-binds one container owned by the
/// fixture's generation 1, returning its id and allocation epoch.
fn bound_container(
    panels: &Rc<RefCell<PanelRuntime>>,
    plugin: &PluginId,
    workspace: u64,
    view: u64,
) -> (PanelId, Generation) {
    let handle = panels
        .borrow_mut()
        .create_panel(PanelType::Rich, Some(WorkspaceId::new(workspace)))
        .expect("create panel");
    panels
        .borrow_mut()
        .mount_panel(handle.id, handle.generation, ViewId::new(view))
        .expect("mount panel");
    panels
        .borrow_mut()
        .set_panel_description(handle.id, handle.generation, "Notes", "acme notes")
        .expect("describe panel");
    panels
        .borrow_mut()
        .rebind_panel(
            handle.id,
            controller_generation(1),
            plugin,
            &set(&[CREATE, FOCUS]),
            &empty(),
        )
        .expect("first bind");
    (handle.id, handle.generation)
}

/// Residual 2 on the production edge: a reload keeps the container and moves the
/// controller.
///
/// The container's `PanelId`, its allocation epoch, its view attachment, its
/// workspace membership, and its title/description survive; the binding, the
/// grant row, and the lease tenure move to generation 2. What makes this row the
/// *edge's* is [`PluginRuntime::last_panel_edge`]: it shows the runtime, not the
/// test, issued the bind.
///
/// Fails for an edge that never calls the port (the binding stays at generation
/// 1 and the panel keeps the reload placeholder), for one that binds the
/// allocation epoch as the controller generation, and for one that rebinds by
/// disposing and re-creating the container (the `PanelId` and the epoch move).
#[test]
fn the_generation_edge_rebinds_the_container_and_moves_the_controller() {
    let (mut runtime, plugin, panels) = edge_fixture("rebind", "acme.edgerebind");
    let (panel, epoch) = bound_container(&panels, &plugin, 5, 11);
    let first = controller_generation(1);
    let successor = controller_generation(2);

    runtime.reload(&plugin).expect("reload");

    let host_ref = panels.borrow();
    let registry = host_ref.registry();
    assert_eq!(
        registry.controller_generation(panel),
        Some(successor),
        "the edge bound the successor generation, not the allocation epoch"
    );
    assert_eq!(
        registry.last_controller_generation(panel),
        Some(first),
        "the displaced predecessor is retained for the next bind's phase 0"
    );
    // Container identity and presentation facts (D.3).
    assert_eq!(
        registry.panels_for_workspace(WorkspaceId::new(5)),
        vec![panel],
        "the same PanelId stays attributed to the same workspace"
    );
    assert_eq!(
        host_ref.placement_view_of(panel),
        Some(ViewId::new(11)),
        "the view attachment survives a rebind"
    );
    assert_eq!(
        host_ref
            .panel_description(panel, epoch)
            .expect("live container"),
        (Some("Notes".to_string()), Some("acme notes".to_string())),
        "the title and description are container facts, not content"
    );
    // Authority moved with the binding and nothing else: the successor holds
    // exactly `(R ∩ P) ∪ C` = {panel.create, panel.focus}, and the predecessor's
    // row is gone.
    assert_eq!(
        granted_set(registry, panel, successor),
        set(&[CREATE, FOCUS])
    );
    assert_eq!(
        granted_set(registry, panel, first),
        empty(),
        "the predecessor's grant row was released at the edge"
    );
    assert_eq!(
        registry.placeholder(panel),
        None,
        "a bound controller owns the content again; the reload placeholder is cleared"
    );
    drop(host_ref);

    let edge = runtime.last_panel_edge().expect("the edge ran");
    assert_eq!(
        (edge.attempted, edge.refused_count()),
        (1, 0),
        "the runtime issued one bind and nothing was refused: {edge:?}"
    );
    assert_eq!(edge.committed, vec![panel]);
}

/// Residual 4 on the production edge: a generation that ends with no successor
/// must release its families before it leaves the placeholder.
///
/// The assertions are a snapshot taken after the edge returned, so this row
/// cannot observe wall-clock order: what it proves is that the placeholder, the
/// release record, the tombstone, and the container are all present together
/// once the edge is done. The placeholder names `unloaded` and the generation
/// its edge ended, the release record names that same generation and counts the
/// two released grants, the ended generation's grant row is gone, and the
/// retired-ceiling tombstone carries the released set — so the edge left
/// nothing half done: no silent orphan, the container kept rather than
/// disposed, and a placeholder carrying a closed reason.
///
/// Fails for an edge that presents the placeholder without running the release,
/// for one that releases but presents nothing (the silent orphan Decision E
/// forbids), and for one that disposes the container instead of keeping it.
#[test]
fn a_generation_edge_with_no_successor_presents_the_placeholder_after_the_release() {
    let (mut runtime, plugin, panels) = edge_fixture("placeholder", "acme.edgeplaceholder");
    let (panel, epoch) = bound_container(&panels, &plugin, 6, 12);
    let ended = controller_generation(1);

    runtime.dispose(&plugin).expect("dispose");

    let host_ref = panels.borrow();
    let registry = host_ref.registry();
    assert_eq!(
        registry.controller_generation(panel),
        None,
        "the container is unbound"
    );
    let placeholder = registry
        .placeholder(panel)
        .expect("an unbound container presents a placeholder, never a silent orphan");
    assert_eq!(placeholder.reason(), PlaceholderReason::Unloaded);
    assert_eq!(
        placeholder.unbound_generation(),
        Some(ended),
        "the placeholder names the generation its edge ended"
    );
    // The release ran, and it ran for the generation the placeholder names.
    let release = registry
        .unbind_release_record(panel)
        .expect("the release record exists when the placeholder is read");
    assert_eq!(release.generation, ended);
    assert_eq!(
        release.grants, 2,
        "both panel grants were released before the placeholder became visible"
    );
    assert_eq!(
        granted_set(registry, panel, ended),
        empty(),
        "no row of the ended generation survives the placeholder"
    );
    // The tombstone keeps the ceiling for a future bind, and the container keeps
    // its identity.
    let ceiling = registry
        .retained_grant_ceiling(panel)
        .expect("the edge writes the retired-ceiling tombstone");
    assert_eq!(ceiling.ended_generation, ended);
    assert_eq!(ceiling.owner, Some(plugin.clone()));
    assert_eq!(ceiling.grants, set(&[CREATE, FOCUS]));
    assert_eq!(
        host_ref.placement_view_of(panel),
        Some(ViewId::new(12)),
        "a placeholder keeps the attachment"
    );
    assert_eq!(
        host_ref
            .panel_description(panel, epoch)
            .expect("live container"),
        (Some("Notes".to_string()), Some("acme notes".to_string())),
        "a placeholder keeps the title and the description"
    );
    drop(host_ref);

    let edge = runtime.last_panel_edge().expect("the edge ran");
    assert_eq!((edge.attempted, edge.refused_count()), (1, 0), "{edge:?}");
    assert_eq!(edge.committed, vec![panel]);
}

/// A refused successor bind leaves the container unbound and widens nothing.
///
/// The container's edge has already run, so the ceiling the successor would read
/// is the retained tombstone. The bind is refused because it presents another
/// owner than the predecessor's (the cross-plugin adoption OQ-084 B.6 leaves
/// undecided), which is D.2 phase 0's `CeilingLost` — refused before phase 1 and
/// before any phase 2 step. The comparison is the whole closed `panel` family
/// before and after: the refused attempt installs nothing for the successor and
/// consumes nothing of the predecessor's ceiling.
///
/// Fails for a bind that installs the successor's resolution before validating
/// the ceiling (the successor would hold `{panel.create, panel.focus}` with no
/// predecessor to bound it), for one that consumes the tombstone on refusal (a
/// later legitimate bind would then be refused too), and for one that leaves the
/// panel bound to the predecessor after refusing.
#[test]
fn a_refused_edge_bind_leaves_the_container_unbound_without_widening_its_grants() {
    let (mut runtime, plugin, panels) = edge_fixture("refused", "acme.edgerefused");
    let (panel, _epoch) = bound_container(&panels, &plugin, 7, 13);
    let first = controller_generation(1);
    let successor = controller_generation(2);

    runtime.dispose(&plugin).expect("dispose");
    let before = granted_set(panels.borrow().registry(), panel, successor);
    let ceiling_before = panels
        .borrow()
        .registry()
        .retained_grant_ceiling(panel)
        .expect("tombstone")
        .grants
        .clone();

    let outcome = panels.borrow_mut().rebind_panel(
        panel,
        successor,
        &owner("evil.other"),
        &set(&[CREATE, FOCUS]),
        &empty(),
    );
    let error = outcome.expect_err("another owner may not adopt this ceiling");
    assert!(
        matches!(error, PanelError::CeilingLost { .. }),
        "D.2 phase 0 refuses a lost ceiling: {error:?}"
    );

    let host_ref = panels.borrow();
    let registry = host_ref.registry();
    let after = granted_set(registry, panel, successor);
    assert_eq!(
        after, before,
        "a refused bind installs no capability for the successor"
    );
    assert_eq!(
        after,
        empty(),
        "and the successor holds nothing at all, never a widened set"
    );
    assert_eq!(
        registry.controller_generation(panel),
        None,
        "the container stays unbound rather than half rebound"
    );
    assert_eq!(
        registry.last_controller_generation(panel),
        Some(first),
        "the refused bind did not move the predecessor"
    );
    assert_eq!(
        registry
            .retained_grant_ceiling(panel)
            .expect("tombstone")
            .grants,
        ceiling_before,
        "the refused bind consumes none of the ceiling a later bind needs"
    );
    assert!(
        registry.placeholder(panel).is_some(),
        "the placeholder the edge presented survives the refusal"
    );
    assert_eq!(
        granted_set(registry, panel, successor),
        empty(),
        "every verb from the successor generation is refused for want of a row"
    );
}

// ---------------------------------------------------------------------------
// bitty#2476 residuals 4 and 5: crash recovery driven by the plugin host
// ---------------------------------------------------------------------------
//
// The two rows below end a generation the way a fault does, not the way a user
// does: a callback inside a live generation exhausts its own budget, the VM
// stops it, and the host funnels the terminal verdict through
// `PluginRuntime::rollback`. That is the crash path -- the same function a
// generation that died in `init.lua` reaches -- and it is the one place the
// crash release is owed, because a crash is not a reload and has no successor.

/// Fuel the crashing fixture's timer body may burn before the host stops it.
///
/// Small enough that the crash costs the instruction ceiling rather than the
/// five seconds an RC-1 wall verdict needs, and well above what two closure
/// definitions cost `init.lua`.
const CRASH_FUEL_INSTRUCTIONS: u64 = 200_000;

/// A package whose activation arms one timer that never returns on its own.
///
/// The manifest carries the panel family plus a `[limits]` fuel ceiling, so the
/// crash is a real RC-1 budget stop inside a real callback: no test-side
/// `fail_runtime_*` call, no injected `SuspendReason`, no widened budget.
fn write_crashing_plugin(root: &Path, id: &str) {
    write_plugin_with(
        root,
        id,
        &["panel.create", "panel.focus"],
        &format!("[limits]\nfuel_instructions = {CRASH_FUEL_INSTRUCTIONS}\n\n"),
        r#"bitty = bitty or {}
bitty.timers.create(10, function()
  local n = 0
  while true do n = n + 1 end
end)
"#,
    );
}

/// The crash fixture: one plugin whose timer kills its own live generation, one
/// sibling plugin that is untouched by it, and the installed edge port.
///
/// The sibling exists for FS-P2 containment: a crash is supposed to reach the
/// owning generation and nothing else, so the row asserts on a second plugin's
/// binding and grants as well as on the crashed one's.
fn crash_fixture(
    tag: &str,
    id: &str,
    sibling_id: &str,
) -> (PluginRuntime, PluginId, PluginId, Rc<RefCell<PanelRuntime>>) {
    let root = temp_dir(tag);
    write_crashing_plugin(&root, id);
    write_plugin(&root, sibling_id, &["panel.create", "panel.focus"]);
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(temp_dir(&format!("{tag}-data"))),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    });
    runtime.discover();
    let plugin = owner(id);
    let sibling = owner(sibling_id);
    runtime
        .activate(&plugin)
        .expect("activate the crashing fixture");
    runtime
        .activate(&sibling)
        .expect("activate the sibling fixture");
    let panels = Rc::new(RefCell::new(host()));
    runtime.set_panel_generation_port(Some(Rc::new(EdgePort {
        panels: Rc::clone(&panels),
    })));
    (runtime, plugin, sibling, panels)
}

/// Kill the crashing fixture's live generation from inside a callback.
///
/// The fixture armed one timer whose body never returns; the pump drives it
/// under the generation's own `[limits]` fuel, the VM stops it with
/// `InstructionBudgetExceeded`, and `fail_runtime_on_suspend` hands that verdict
/// to `fail_runtime_classified`, which rolls the generation back. Nothing here
/// chooses the failure: the host does, from a callback it really ran.
fn crash_the_live_generation(runtime: &mut PluginRuntime, plugin: &PluginId) {
    let deadline = runtime
        .timer_deadline()
        .expect("activation armed the crashing timer");
    assert_eq!(
        runtime.pump_due_timers_at(deadline),
        1,
        "the crashing callback started and was counted"
    );
    match runtime.state(plugin) {
        Some(LifecycleState::Failed(reason)) => assert!(
            reason.contains("RC-1 instructions"),
            "the crash names the budget that stopped it: {reason}"
        ),
        other => panic!("a callback that exhausts its budget ends the generation, got {other:?}"),
    }
}

/// PGO-4: a crash inside a callback keeps the container, names the crash, and
/// reaches no other generation.
///
/// The generation dies the way a fault makes it die -- a callback that exhausts
/// its own RC-1 instruction budget -- and everything the row asks for is read
/// back through the registry the verbs use:
///
/// - the container is **unbound**, not disposed and not silently still owned by
///   the dead generation, and it presents the closed reason `crashed` naming the
///   generation the crash ended;
/// - the release ran at that edge (the tombstone carries the released set, the
///   release record names the same generation, the generation's grant row is
///   empty), so the placeholder became visible only after the four families
///   were released -- OQ-084 A.2's ordering, read as a snapshot because the row
///   cannot observe wall-clock order;
/// - one recovery record exists and the counter is `1`, not two: the edge is
///   the crash's only presenter, and no per-tick path re-presents it
///   (Decision E rule 1: one placeholder, one record, no respawn);
/// - the container keeps its `PanelId`, attachment, title, and description,
///   because a fault must not destroy the user's structure;
/// - the **sibling** plugin's container is still bound to its own generation
///   with its own grants, and the crash's edge report attempted exactly one
///   container -- FS-P2 containment, and "only the owning generation is
///   affected".
///
/// Fails for the pre-fix tree, where `rollback` released the generation's own
/// content but never asked the panel port: the crashed generation stayed bound,
/// kept `{panel.create, panel.focus}`, and presented no placeholder at all (a
/// silent orphan Decision E forbids). Fails as well for a release that presents
/// the placeholder before the tombstone/families, for one that disposes the
/// container instead of unbinding it, and for one that reaches a sibling.
#[test]
fn pgo_4_a_crash_inside_a_callback_presents_crashed_and_contains_the_damage() {
    let (mut runtime, plugin, sibling, panels) =
        crash_fixture("crash", "acme.edgecrash", "acme.edgecrashsibling");
    let (panel, epoch) = bound_container(&panels, &plugin, 9, 15);
    let (sibling_panel, _sibling_epoch) = bound_container(&panels, &sibling, 10, 16);
    let ended = controller_generation(1);
    let sibling_controller = controller_generation(1);

    crash_the_live_generation(&mut runtime, &plugin);

    let host_ref = panels.borrow();
    let registry = host_ref.registry();
    // Disposition: unbound, container kept.
    assert_eq!(
        registry.controller_generation(panel),
        None,
        "the crashed generation no longer binds its container"
    );
    let placeholder = registry
        .placeholder(panel)
        .expect("a crashed generation leaves a placeholder, never a silent orphan");
    assert_eq!(
        placeholder.reason(),
        PlaceholderReason::Crashed,
        "the reason token names the crash, not a user action nobody took"
    );
    assert_eq!(
        placeholder.unbound_generation(),
        Some(ended),
        "the placeholder names the generation the crash ended"
    );
    // Ordering, read as the state the edge left behind.
    let release = registry
        .unbind_release_record(panel)
        .expect("the crash edge ran the release before the placeholder");
    assert_eq!(release.generation, ended);
    assert_eq!(
        release.grants, 2,
        "both panel grants were released before the placeholder became visible"
    );
    assert_eq!(
        granted_set(registry, panel, ended),
        empty(),
        "no grant row of the crashed generation survives it"
    );
    let ceiling = registry
        .retained_grant_ceiling(panel)
        .expect("the crash edge writes the retired-ceiling tombstone");
    assert_eq!(ceiling.ended_generation, ended);
    assert_eq!(ceiling.owner, Some(plugin.clone()));
    assert_eq!(ceiling.grants, set(&[CREATE, FOCUS]));
    // Bounded recovery: one record, one count, for the generation that ended.
    assert_eq!(
        registry.recovery_attempts(panel, ended, PlaceholderReason::Crashed),
        1,
        "exactly one FS-4-style record for the crash"
    );
    assert_eq!(
        registry.recovery_records(panel).len(),
        1,
        "and the edge did not present the placeholder twice"
    );
    // The container keeps the facts that are not content.
    assert_eq!(
        host_ref.placement_view_of(panel),
        Some(ViewId::new(15)),
        "a crash keeps the view attachment"
    );
    assert_eq!(
        host_ref
            .panel_description(panel, epoch)
            .expect("live container"),
        (Some("Notes".to_string()), Some("acme notes".to_string())),
        "a crash keeps the title and the description"
    );
    // FS-P2 containment: the sibling generation is untouched.
    assert_eq!(
        registry.controller_generation(sibling_panel),
        Some(sibling_controller),
        "a crash reaches only the owning generation"
    );
    assert!(
        registry.placeholder(sibling_panel).is_none(),
        "the sibling panel is not unbound by another plugin's crash"
    );
    assert_eq!(
        granted_set(registry, sibling_panel, sibling_controller),
        set(&[CREATE, FOCUS]),
        "the sibling keeps its own authority"
    );
    drop(host_ref);

    let edge = runtime.last_panel_edge().expect("the crash ran an edge");
    assert_eq!(
        (edge.attempted, edge.refused_count()),
        (1, 0),
        "the crash edge attempted exactly the crashed generation's container: {edge:?}"
    );
    assert_eq!(edge.committed, vec![panel]);
}

/// PGO-20, crash arm: bounded recovery, then no authority from the crashed
/// generation survives the next bind.
///
/// The crash half of the row, on the same production path as `PGO-4`:
///
/// - the placeholder names `crashed`, and the registry holds **one** recovery
///   record for it, not one per tick;
/// - no respawn inside the driver's window: the timer the generation armed is
///   dropped with it (`timer_deadline` is empty), a recovery pass selects
///   nothing (an instruction stop is not the transient wall class
///   `recover_wall_failed_plugins` retries), and a second pass still selects
///   nothing while the placeholder stays `crashed` and the counter stays `1`;
/// - container, title, attachment, and focus history are intact: the focused
///   panel is still the focused one and the MRU order is unchanged;
/// - the next bind is a *new* generation's bind: the container is rebound to
///   the successor, the placeholder is gone, and the crashed generation holds
///   nothing -- its row is empty before and after the bind, so no authority of
///   generation `N` survives into `N+1`.
///
/// The disable half of `PGO-20` (`disabled` on a live generation, then a
/// re-enable that rebinds) is **not** in this row: no production path disables a
/// live generation that still binds containers, so the reason token has no
/// producer for this shape. See the file's rustdoc for the blocking symbols.
///
/// Fails for the pre-fix tree at the first assertion (`rollback` presented no
/// placeholder, so `registry.placeholder(panel)` is `None`), and for a fix that
/// re-presents on every tick, respawns the generation, or lets the crashed
/// generation's grant row survive the successor's bind.
#[test]
fn pgo_20_a_crashed_generation_is_bounded_and_loses_its_authority_at_the_next_bind() {
    let (mut runtime, plugin, _sibling, panels) =
        crash_fixture("crashbounded", "acme.edgecrashbounded", "acme.edgecrashsib");
    // Two containers in one workspace, so focus and the MRU order are facts the
    // crash has to preserve rather than leave unobserved.
    let workspace = WorkspaceId::new(11);
    let (first, first_epoch) = bound_container(&panels, &plugin, 11, 17);
    let (second, second_epoch) = bound_container(&panels, &plugin, 11, 18);
    panels
        .borrow_mut()
        .focus_panel(first, first_epoch, workspace)
        .expect("focus the first container");
    panels
        .borrow_mut()
        .focus_panel(second, second_epoch, workspace)
        .expect("focus the second container");
    let ended = controller_generation(1);
    let successor = controller_generation(2);

    crash_the_live_generation(&mut runtime, &plugin);

    {
        let host_ref = panels.borrow();
        let registry = host_ref.registry();
        for panel in [first, second] {
            let placeholder = registry
                .placeholder(panel)
                .expect("every container of the crashed generation presents a placeholder");
            assert_eq!(placeholder.reason(), PlaceholderReason::Crashed);
            assert_eq!(placeholder.unbound_generation(), Some(ended));
            assert_eq!(
                registry.recovery_attempts(panel, ended, PlaceholderReason::Crashed),
                1,
                "one record per container, not one per container per tick"
            );
            assert_eq!(registry.recovery_records(panel).len(), 1);
            assert_eq!(
                registry.controller_generation(panel),
                None,
                "the crashed generation keeps neither container"
            );
        }
        // The container facts a crash must not destroy.
        assert_eq!(
            registry.focused_panel(workspace),
            Some(second),
            "the focus history survives the crash"
        );
        let mut mru = registry.mru_order(workspace);
        mru.sort_unstable();
        let mut expected = vec![first, second];
        expected.sort_unstable();
        assert_eq!(mru, expected, "the MRU order keeps the containers it had");
        assert_eq!(
            host_ref.placement_view_of(first),
            Some(ViewId::new(17)),
            "the attachment survives the crash"
        );
    }

    // No respawn inside the driver's window.
    assert_eq!(
        runtime.timer_deadline(),
        None,
        "the crashed generation's timer is dropped with it"
    );
    assert_eq!(
        runtime.recover_wall_failed_plugins(),
        0,
        "an instruction stop is not a transient wall timeout, so nothing is respawned"
    );
    assert_eq!(
        runtime.recover_wall_failed_plugins(),
        0,
        "and a second pass still selects nothing"
    );
    {
        let host_ref = panels.borrow();
        let registry = host_ref.registry();
        assert_eq!(
            registry.placeholder(first).expect("placeholder").reason(),
            PlaceholderReason::Crashed,
            "the bounded recovery leaves the reason it started with"
        );
        assert_eq!(
            registry.recovery_attempts(first, ended, PlaceholderReason::Crashed),
            1,
            "and neither pass presented the placeholder again"
        );
        assert_eq!(
            granted_set(registry, first, ended),
            empty(),
            "the crashed generation holds nothing before the bind"
        );
    }

    // The next bind belongs to a new generation, and inherits nothing.
    runtime.reload(&plugin).expect("reload after the crash");
    {
        let host_ref = panels.borrow();
        let registry = host_ref.registry();
        for panel in [first, second] {
            assert_eq!(
                registry.controller_generation(panel),
                Some(successor),
                "the successor generation rebound the container"
            );
            assert!(
                registry.placeholder(panel).is_none(),
                "a bound container presents no placeholder"
            );
            assert_eq!(
                granted_set(registry, panel, ended),
                empty(),
                "no authority of the crashed generation survives the next bind"
            );
            assert_eq!(
                granted_set(registry, panel, successor),
                set(&[CREATE, FOCUS]),
                "the successor holds its own resolution bounded by the crashed ceiling"
            );
        }
        assert_eq!(
            registry.last_controller_generation(first),
            Some(ended),
            "the crashed generation is retained as the predecessor, not as a binding"
        );
    }
    let edge = runtime.last_panel_edge().expect("the reload ran an edge");
    assert_eq!(
        (edge.attempted, edge.refused_count()),
        (2, 0),
        "the successor's bind enumerated both containers: {edge:?}"
    );
}

// ---------------------------------------------------------------------------
// bitty#2476 residual 9: PGO-6, PGO-9, PGO-13
// ---------------------------------------------------------------------------
//
// The rows above drive the bind and the generation edge. These three drive the
// third actor in the ownership model: a caller that is *not* the edge — a mount
// that started before the edge ran, a plugin guessing another plugin's panel,
// and the disposal that ends a container. All three go through production entry
// points (`mount_panel`, `subscribe`/`publish`/`require_panel_capability`,
// `dispose_panel`), not a test-only binder.

/// PGO-6 (bitty#2476 residual 9): a mount that raced a reload commits nothing.
///
/// The reload disposes generation `N` and binds `N+1`; a mount issued while `N`
/// was still the controller arrives afterwards. Two presentations reach the
/// fence and both must fail closed:
///
/// - `N`, the generation the in-flight caller was driving — its binding ended,
///   so the fence answers `ControllerReleased`;
/// - the container's allocation epoch, the identity a caller held while the
///   reload was still disposing `N` — the record binds `N+1` now, so the fence
///   answers `ControllerConflict`.
///
/// The epoch arm is the one the pre-fence tree committed: `mount_panel`
/// validated against the allocation epoch alone, so a mount that started before
/// the edge succeeded after it and attached a view to a container another
/// generation had just bound. The row's remaining claims are what the refusal
/// owes: the racing view ends unbound, the panel keeps its pre-mount state, and
/// the successor mounts cleanly on that same view afterwards.
///
/// Fails for the pre-fence tree (the epoch mount commits and the successor's
/// mount then finds the view taken), for a fence that answers `StaleHandle` for
/// a released controller generation instead of `ControllerReleased`, and for a
/// refusal that writes the placement mirror or the panel state before refusing.
#[test]
fn a_mount_that_races_a_reload_commits_nothing_and_the_successor_mounts_cleanly() {
    let (mut runtime, plugin, panels) = edge_fixture("mountrace", "acme.mountrace");
    let (panel, epoch) = bound_container(&panels, &plugin, 8, 14);
    let ended = controller_generation(1);
    let successor = controller_generation(2);

    // The container goes back to `Suspended` before the reload, so the race is
    // decidable: a mount onto an attached panel is refused for being mounted,
    // not for the generation it presents.
    panels
        .borrow_mut()
        .unmount_panel(panel, epoch)
        .expect("unmount before the reload");
    let before = panels
        .borrow()
        .registry()
        .panel_state(panel, epoch)
        .expect("live container");

    runtime.reload(&plugin).expect("reload");
    assert_eq!(
        panels.borrow().registry().controller_generation(panel),
        Some(successor),
        "the edge moved the binding, so a racing mount is measured against it"
    );

    let race_view = ViewId::new(21);
    assert_eq!(
        panels
            .borrow_mut()
            .mount_panel(panel, ended, race_view)
            .expect_err("a mount from the generation the reload disposed is refused"),
        PanelError::ControllerReleased {
            panel_id: panel,
            released_generation: ended,
        },
        "the fence names the caller's own ended binding instead of a generic stale handle"
    );
    assert_eq!(
        panels
            .borrow_mut()
            .mount_panel(panel, epoch, race_view)
            .expect_err(
                "a mount carrying the allocation epoch is refused while a controller is bound"
            ),
        PanelError::ControllerConflict {
            panel_id: panel,
            bound_generation: Some(successor),
            presented_generation: epoch,
        },
        "the epoch was the accepted key before the edge; afterwards the record binds N+1"
    );

    let host_ref = panels.borrow();
    let registry = host_ref.registry();
    assert_eq!(
        registry.panel_state(panel, epoch).expect("live container"),
        before,
        "neither refused mount touched the panel state"
    );
    assert_eq!(
        registry.panel_view(panel, epoch).expect("live container"),
        None,
        "the container keeps its pre-mount, unattached state"
    );
    assert_eq!(
        host_ref.placement_panel_of(race_view),
        None,
        "the racing view ends unbound: no attachment map holds it"
    );
    assert_eq!(
        host_ref.placement_len(),
        0,
        "and the refusing path bound no other view"
    );
    assert_eq!(
        registry.controller_generation(panel),
        Some(successor),
        "the refusals moved no binding"
    );
    assert_eq!(
        registry.error_count("ControllerReleased"),
        1,
        "the ended-binding arm ran exactly once"
    );
    assert_eq!(
        registry.error_count("ControllerConflict"),
        1,
        "and the epoch arm ran exactly once"
    );
    drop(host_ref);

    panels
        .borrow_mut()
        .mount_panel(panel, successor, race_view)
        .expect("the successor generation mounts cleanly afterwards");
    let host_ref = panels.borrow();
    assert_eq!(
        host_ref.placement_panel_of(race_view),
        Some(panel),
        "the view the racing mount was refused is usable by the generation that owns the panel"
    );
    assert_eq!(
        host_ref.placement_view_of(panel),
        Some(race_view),
        "and the attachment belongs to the successor's mount"
    );
}

/// PGO-9 (bitty#2476 residual 9): a guessed `PanelId` is answered at the grant
/// gate, before any state is read.
///
/// Every capability-gated panel verb funnels through
/// [`PanelRegistry::require_panel_capability`] — the two bus verbs call it
/// directly, and `panel.create`, `panel.focus`, and `panel.overlay` are the same
/// row's families — and the row is keyed `(PanelId, accepted generation)`. A
/// caller that guesses a live `PanelId` therefore gets nothing:
///
/// - without a grant, every family is denied and the bus verbs queue nothing;
/// - the denial precedes the record read: the same call for a `PanelId` that
///   never existed answers `CapabilityDenied`, not `NotFound`, which a gate that
///   resolved the panel first could not do;
/// - with a grant, a verb naming *another generation* of the same panel is
///   still denied while the accepted generation works — the lookup is
///   generation-scoped, not panel-scoped.
///
/// Fails for a gate that resolves grants by panel alone (the foreign generation
/// would inherit the granted row), for one that reads the record before checking
/// the grant (a guessed id would answer `NotFound`), and for a denial that still
/// publishes.
#[test]
fn a_guessed_panel_id_is_denied_at_the_grant_gate_before_any_state_access() {
    let mut runtime = host();
    let handle = runtime
        .create_panel(PanelType::Helper, Some(WorkspaceId::new(9)))
        .expect("create panel");
    let epoch = handle.generation;
    let topic = runtime.declare_topic("acme.notes:status").expect("topic");
    let payload = BoundedPayload::try_new("busy").expect("payload");

    // No grant: every capability-gated family is denied for the live panel.
    assert!(matches!(
        runtime.subscribe(handle.id, epoch, &topic),
        Err(PanelError::CapabilityDenied { .. })
    ));
    assert!(matches!(
        runtime.publish(handle.id, epoch, &topic, payload.clone()),
        Err(PanelError::CapabilityDenied { .. })
    ));
    for capability in PANEL_FAMILY {
        assert!(
            matches!(
                runtime
                    .registry()
                    .require_panel_capability(handle.id, epoch, capability),
                Err(PanelError::CapabilityDenied { .. })
            ),
            "ungranted {capability} is denied"
        );
    }
    assert_eq!(
        runtime.bus_total_events(),
        0,
        "a denied verb queues nothing"
    );

    // Before any state access: an id that never existed is denied by the same
    // gate, so no record lookup can have run for it.
    let phantom = PanelId::new(handle.id.get() + 10_000);
    assert!(
        matches!(
            runtime.subscribe(phantom, epoch, &topic),
            Err(PanelError::CapabilityDenied { .. })
        ),
        "the gate answers before the record is resolved, so a guessed id is denied rather than NotFound"
    );
    assert!(matches!(
        runtime
            .registry()
            .require_panel_capability(phantom, epoch, OVERLAY),
        Err(PanelError::CapabilityDenied { .. })
    ));

    // Granted at the accepted key: the verb works there and nowhere else.
    runtime
        .grant_capability(handle.id, epoch, "panel.provider")
        .expect("grant the bus capability at the accepted key");
    runtime
        .subscribe(handle.id, epoch, &topic)
        .expect("the accepted generation subscribes");
    let foreign = epoch.next().expect("a generation this panel never held");
    assert!(
        matches!(
            runtime.subscribe(handle.id, foreign, &topic),
            Err(PanelError::CapabilityDenied { .. })
        ),
        "another generation of the same panel inherits nothing from the granted row"
    );
    assert!(matches!(
        runtime.publish(handle.id, foreign, &topic, payload.clone()),
        Err(PanelError::CapabilityDenied { .. })
    ));
    runtime
        .publish(handle.id, epoch, &topic, payload)
        .expect("the accepted generation publishes");
    assert_eq!(
        runtime.bus_total_events(),
        1,
        "exactly the granted publish queued, so neither denial emitted"
    );
}

/// PGO-13 (bitty#2476 residual 9): a disposal reclaims every registry-owned
/// family it can be observed to own.
///
/// This is FS-P4's reclaim criterion read structurally. PB-3's tolerance is a
/// wall-clock budget for a session that has recycled generations; the counts
/// asserted here are the state a disposal either releases or leaks, so exact
/// equality with the baseline is inside any non-negative tolerance. The
/// container is disposed while it still holds a queue, a command, a live grant
/// row, an MRU entry, and an attributed overlay — every registry-owned family
/// the row names — and each is asserted non-baseline before the disposal, so the
/// reclaimed assertion is not vacuous.
///
/// A second container shares the workspace, because the MRU entry is only
/// observable while the bucket outlives the disposal: disposing the only panel
/// empties the bucket, and `dispose_panel` then drops the whole focus tracker,
/// so a disposal that never calls `PanelFocus::on_panel_hidden` reads exactly
/// the same. Here the focused panel is disposed while a sibling stays in the
/// bucket, so the tracker survives with the disposed `PanelId` still in it
/// unless the call runs: without it `mru_order` answers `[disposed, sibling]`
/// and `focused_panel` keeps naming the disposed panel.
///
/// The container never binds a controller here: on a bound panel the public
/// verbs are addressed by the controller generation while `subscribe`, focus,
/// and command registration validate the allocation epoch, so this is the one
/// state in which all the families can coexist. The bound panel's disposal runs
/// the same release for the controller key, which
/// `tests/panel_grant_ceiling.rs:182` and `tests/panel_unbind_release.rs` cover.
///
/// Fails for a disposal that leaves any one family behind: the queue, the
/// command, the MRU entry, the focus that entry carried, the attributed overlay,
/// the workspace bucket, the dropped tombstone, or the panel count.
///
/// **Not covered here, and not claimed:** a live grant row that survives the
/// disposal. [`PanelRegistry::is_panel_capability_granted`] resolves its key
/// through the panel record (`resolved_grant_generation`), and `dispose_panel`
/// removes the record, so it answers `false` for a leaked row exactly as it does
/// for a released one. Deleting both removals of the row —
/// `release_generation_scope`'s `self.capabilities.remove(&(panel_id, generation))`
/// and the epoch sweep in `sweep_allocation_epoch_families` — leaves the row in
/// the table and this row green. The panel id is never reused
/// (`next_panel_raw` is monotonic), so no later container can expose it either.
/// Seeing that leak needs a public accessor for the live grant rows by
/// `PanelId` — a `live_grant_row_count(panel_id)` or a raw row read — which the
/// registry does not expose. The two `is_panel_capability_granted` assertions
/// below are the accessor's post-disposal answer only, never evidence that the
/// row went.
#[test]
fn a_disposal_reclaims_every_registry_owned_family_to_the_pre_creation_baseline() {
    let mut reg = registry();
    let ws = WorkspaceId::new(21);
    let view = ViewId::new(41);
    let sibling_view = ViewId::new(42);
    let command = "acme.notes:open";
    let topic = reg.declare_topic("acme.notes:status").expect("topic");

    // Baseline: what a session holds with no container of this kind.
    assert_eq!(reg.panel_count(), 0);
    assert_eq!(reg.attributed_workspace_count(), 0);
    assert_eq!(reg.bus_total_events(), 0);
    assert_eq!(reg.attributed_overlay_count(), 0);
    assert_eq!(reg.overlay_len(), 0);
    assert_eq!(reg.retained_grant_ceiling_count(), 0);
    assert!(reg.command_owner(command).is_none());

    let handle = reg
        .create_panel(PanelType::Rich, Some(ws))
        .expect("create panel");
    let epoch = handle.generation;
    reg.mount_panel(handle.id, epoch, view)
        .expect("mount panel");
    reg.grant_panel_capability(handle.id, epoch, CREATE)
        .expect("grant create");
    reg.grant_panel_capability(handle.id, epoch, "panel.provider")
        .expect("grant the bus capability");
    reg.register_command(handle.id, epoch, command)
        .expect("register command");
    reg.focus_panel(handle.id, epoch, ws).expect("focus panel");
    reg.subscribe(handle.id, epoch, &topic)
        .expect("subscribe the panel");
    reg.publish(&topic, BoundedPayload::try_new("busy").expect("payload"))
        .expect("publish to the subscriber");
    let overlay = reg
        .create_panel_overlay(
            handle.id,
            epoch,
            OverlayKind::NonModal,
            UiRect::new(0, 0, 30, 8),
            "note",
            None,
        )
        .expect("attributed overlay");

    // The sibling keeps the workspace bucket alive across the disposal, which is
    // what makes the MRU assertion below able to fail. Focus ends on `handle`,
    // so the disposed panel is the one the tracker has to forget and repoint.
    let sibling = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("create the sibling panel");
    let sibling_epoch = sibling.generation;
    reg.mount_panel(sibling.id, sibling_epoch, sibling_view)
        .expect("mount the sibling panel");
    reg.focus_panel(sibling.id, sibling_epoch, ws)
        .expect("focus the sibling");
    reg.focus_panel(handle.id, epoch, ws)
        .expect("focus the disposed panel again");

    // The container really holds every family before the disposal.
    assert_eq!(reg.panel_count(), 2, "the sibling is live too");
    assert_eq!(reg.attributed_workspace_count(), 1);
    assert_eq!(
        reg.mru_order(ws),
        vec![handle.id, sibling.id],
        "both containers hold an MRU entry, the disposed one first"
    );
    assert_eq!(reg.focused_panel(ws), Some(handle.id));
    assert_eq!(reg.panels_for_workspace(ws), vec![handle.id, sibling.id]);
    assert_eq!(reg.command_owner(command), Some(handle.id));
    assert_eq!(reg.bus_events_for_panel(handle.id), 1);
    assert_eq!(reg.attributed_overlay_count(), 1);
    assert!(reg.is_panel_capability_granted(handle.id, epoch, CREATE));

    reg.dispose_panel(handle.id, epoch).expect("dispose panel");

    assert_eq!(
        reg.panel_count(),
        1,
        "the disposed container count drops and the sibling stays"
    );
    assert_eq!(
        reg.attributed_workspace_count(),
        1,
        "the workspace bucket outlives the disposed panel while the sibling is in it"
    );
    assert_eq!(
        reg.mru_order(ws),
        vec![sibling.id],
        "the MRU entry goes with the container even though the bucket stays"
    );
    assert_eq!(
        reg.focused_panel(ws),
        Some(sibling.id),
        "focus moves to the next MRU entry instead of naming the disposed panel"
    );
    assert_eq!(
        reg.panels_for_workspace(ws),
        vec![sibling.id],
        "the workspace bucket no longer names the disposed panel"
    );
    assert_eq!(
        reg.command_owner(command),
        None,
        "the command the panel registered is unregistered"
    );
    assert_eq!(
        reg.bus_events_for_panel(handle.id),
        0,
        "the panel's queue is drained"
    );
    assert_eq!(reg.bus_total_events(), 0, "and nothing else is left queued");
    assert_eq!(
        reg.attributed_overlay_count(),
        0,
        "the attributed overlay is dismissed with its attribution"
    );
    assert!(reg.overlay_attribution_of(overlay).is_none());
    assert_eq!(reg.overlay_len(), 0, "no surface outlives the container");
    // The record is what resolves the key, so this is the accessor's
    // post-disposal answer, not a read of the grant table (see the rustdoc).
    assert!(
        !reg.is_panel_capability_granted(handle.id, epoch, CREATE),
        "the retired handle resolves no grant"
    );
    assert!(!reg.is_panel_capability_granted(handle.id, epoch, "panel.provider"));
    assert_eq!(
        reg.retained_grant_ceiling_count(),
        0,
        "the tombstone the disposal's own release wrote is dropped with the container"
    );
    assert!(reg.retained_grant_ceiling(handle.id).is_none());
    assert_eq!(
        reg.unbind_release_record_count(),
        0,
        "no release record outlives the container either"
    );
    assert!(reg.panel_disposal_facts(handle.id).is_none());
    assert!(
        matches!(
            reg.panel_state(handle.id, epoch),
            Err(PanelError::NotFound { .. })
        ),
        "the retired handle names nothing"
    );

    // The last panel of the workspace takes the bucket and the tracker with it,
    // so the pre-creation baseline is reached by the per-container reclaim and
    // not by the sibling's presence hiding a leak.
    reg.dispose_panel(sibling.id, sibling_epoch)
        .expect("dispose the sibling");
    assert_eq!(
        reg.panel_count(),
        0,
        "the container count returns to baseline"
    );
    assert_eq!(
        reg.attributed_workspace_count(),
        0,
        "the workspace bucket went with its last panel"
    );
    assert_eq!(reg.mru_order(ws), Vec::new(), "and the MRU with it");
    assert_eq!(reg.focused_panel(ws), None, "and it holds no focus");
}

/// PGO-13, the half the facade owns: the lease entry and the live placement
/// mirror of the view attachment are released with the container.
///
/// Neither family lives in the registry: the lease table and the `Placement`
/// mirror are `PanelRuntime` state, and C.8's `Focused -> Disposed` requires
/// both to go. A live handle cannot observe its own lease after disposal,
/// because the handle is retired, so the lease is read the one way a leak is
/// still visible: the sweep walks every entry the table holds. A tenure that
/// outlived its container comes back from [`PanelRuntime::sweep_expired_leases`]
/// — that is the reclaim the disposal owes, not a `NotFound` from a record that
/// is gone either way.
///
/// Fails for a disposal that retires the record but leaves the facade's mirrors:
/// `PanelRuntime::dispose_panel` must release them after the registry commit the
/// way the workspace sweep does, or the placement map keeps a view bound to a
/// container that no longer exists and the lapsed tenure is still there for the
/// next tick to sweep.
#[test]
fn a_disposal_releases_the_lease_entry_and_the_placement_mirror() {
    let mut runtime = host();
    let view = ViewId::new(42);
    let handle = runtime
        .create_panel(PanelType::Helper, Some(WorkspaceId::new(22)))
        .expect("create panel");
    let epoch = handle.generation;
    runtime
        .mount_panel(handle.id, epoch, view)
        .expect("mount panel");
    // A tenure that lapses after the disposal, so the sweep is the observer: an
    // entry the disposal should have removed is exactly what it would return.
    runtime
        .acquire_panel_lease(handle.id, epoch, LeaseHolder(7), 30, 1_000)
        .expect("acquire lease");

    assert_eq!(runtime.panel_count(), 1);
    assert_eq!(runtime.placement_len(), 1);
    assert_eq!(runtime.placement_view_of(handle.id), Some(view));
    assert!(
        matches!(
            runtime
                .panel_lease_state(handle.id, epoch)
                .expect("lease state"),
            LeaseState::Occupied { .. }
        ),
        "the tenure exists before the disposal"
    );

    runtime
        .dispose_panel(handle.id, epoch)
        .expect("dispose panel");

    assert_eq!(
        runtime.panel_count(),
        0,
        "the container count returns to baseline"
    );
    assert_eq!(
        runtime.placement_len(),
        0,
        "no placement mirror outlives the container"
    );
    assert_eq!(runtime.placement_view_of(handle.id), None);
    assert_eq!(
        runtime.placement_panel_of(view),
        None,
        "the view is unbound, not pointed at a retired container"
    );
    assert!(
        runtime.sweep_expired_leases(1_031).is_empty(),
        "the tenure the container held went with it: a leaked entry is what the first sweep would return"
    );
    assert!(
        matches!(
            runtime.panel_lease_state(handle.id, epoch),
            Err(PanelError::NotFound { .. })
        ),
        "and the retired handle cannot name a live tenure"
    );
}

/// PGO-15 (bitty#2476 residual 9): a verb a displaced generation still holds a
/// handle for is refused, and the refusal lands before the container moves.
///
/// The refusal symbol is not assumed here, it is read off the tree: the only
/// at-use revalidation in the registry is
/// `PanelRegistry::panel_state_at_controller_fence`
/// (`crates/bitty-runtime/src/registry/panel.rs:1711`). Two verbs reach it
/// before they commit — `mount_panel` (call at `:1964`) and
/// `present_placeholder` (call at `:2360`) — and it answers
/// `ControllerReleased` (`:1743`) when the presented generation is the
/// record's retained `last_controller_generation`, `ControllerConflict`
/// (`:1758`) when the record binds or has ended a controller and the presented
/// generation is neither, and the pre-existing `StaleHandle` while nothing ever
/// bound. Every other lifecycle verb resolves through `get_panel` (`:1766`) /
/// `get_panel_mut` (`:1788`), which stays fenced on the allocation epoch and
/// answers `StaleHandle`; `dispose_panel` raises the same variant explicitly
/// (`:2165`). All four are typed refusals taken before the commit.
///
/// Two limits of the row are stated rather than papered over, and neither is
/// fixed here because both need production work an owner decision gates:
///
/// - The row's literal claim, "every verb is refused with
///   `ControllerConflict`", does not hold on this tree. A generation whose own
///   binding ended is refused `ControllerReleased`, which the decision record
///   already carries as finding (d) ("the widened fence returns a variant D.2
///   does not define for that case",
///   `bitty-terminal-docs/specifications/panel-generation-ownership-decision.md`).
///   `ControllerConflict` is the arm for a generation that is neither bound nor
///   released, and this row pins both so the distinction cannot collapse.
/// - The row's read half stays unproven: the panel-scoped bus
///   (`PanelEventBus::queued_events_for_panel`) and the scene slot
///   (`PanelRuntime::panel_scene`) are keyed by panel alone, so there is no
///   generation dimension on which a displaced generation could be refused a
///   read of its successor's queued events. A test cannot create that key.
///
/// Fails for a fence that is dropped from either verb (the mount or the
/// placeholder commits and the container moves), for one that answers
/// `StaleHandle` or `ControllerConflict` where the released arm belongs (the
/// caller cannot tell its own ended binding from a foreign one), for a refusal
/// that writes the placeholder, the release record, the attachment, the
/// binding, or the grant row before refusing, and for a grant lookup that
/// resolves a released generation's row.
#[test]
fn pgo_15_a_verb_from_a_displaced_generation_is_refused_and_moves_nothing() {
    let (mut runtime, plugin, panels) = edge_fixture("displaced", "acme.displaced");
    let (panel, epoch) = bound_container(&panels, &plugin, 9, 17);
    let ended = controller_generation(1);
    let successor = controller_generation(2);

    // The attachment is released before the edge so the mount below asks the
    // generation question: a still-mounted container refuses a second mount with
    // `PanelAlreadyMounted` before the fence is reached, which is a different
    // answer than the one this row measures.
    panels
        .borrow_mut()
        .unmount_panel(panel, epoch)
        .expect("unmount before the reload");
    let state_before = panels
        .borrow()
        .panel_state(panel, epoch)
        .expect("live container");
    let described_before = panels
        .borrow()
        .panel_description(panel, epoch)
        .expect("the container's title and description");

    runtime.reload(&plugin).expect("reload");
    assert_eq!(
        panels.borrow().registry().controller_generation(panel),
        Some(successor),
        "the edge rebound the container, so the displaced generation is generation N"
    );
    let releases_after_edge = panels.borrow().registry().unbind_release_count(panel);
    assert_eq!(
        panels.borrow().registry().placeholder_reason(panel),
        None,
        "the bind cleared the reload placeholder and gave the container a controller again"
    );

    // 1. The controller fence: the two verbs that present a generation.
    assert_eq!(
        panels
            .borrow_mut()
            .mount_panel(panel, ended, ViewId::new(30))
            .expect_err("a mount from the generation the reload displaced is refused"),
        PanelError::ControllerReleased {
            panel_id: panel,
            released_generation: ended,
        },
        "the fence names the caller's own ended binding instead of a generic stale handle"
    );
    assert_eq!(
        panels
            .borrow_mut()
            .present_placeholder(panel, ended, "unloaded")
            .expect_err("a displaced generation cannot repaint the container as unloaded"),
        PanelError::ControllerReleased {
            panel_id: panel,
            released_generation: ended,
        },
        "the placeholder surface is fenced by the same arm"
    );
    assert_eq!(
        panels.borrow().registry().placeholder_reason(panel),
        None,
        "the refused placeholder wrote nothing over a container the successor drives"
    );
    assert_eq!(
        panels.borrow().registry().unbind_release_count(panel),
        releases_after_edge,
        "and the refusal ran no second release of the ended generation"
    );

    // 2. The allocation-epoch verbs: every lifecycle command a handle can issue.
    assert_eq!(
        panels
            .borrow_mut()
            .unmount_panel(panel, ended)
            .expect_err("a displaced generation cannot unmount the successor's panel"),
        PanelError::StaleHandle {
            expected_generation: epoch,
            found_generation: ended,
            id_raw: panel.0,
        },
        "the lifecycle verbs stay fenced on the allocation epoch, unchanged by a bind"
    );
    // Each borrow is its own statement: an array of three `borrow_mut()`
    // temporaries would keep the first `RefMut` alive until the loop ends.
    let suspended = panels
        .borrow_mut()
        .suspend_panel(panel, ended)
        .expect_err("a displaced generation cannot suspend the successor's panel");
    let resumed = panels
        .borrow_mut()
        .resume_panel(panel, ended)
        .expect_err("a displaced generation cannot resume the successor's panel");
    let disposed = panels
        .borrow_mut()
        .dispose_panel(panel, ended)
        .expect_err("a displaced generation cannot dispose the successor's panel");
    for (what, error) in [
        ("suspend", suspended),
        ("resume", resumed),
        ("dispose", disposed),
    ] {
        assert_eq!(
            error,
            PanelError::StaleHandle {
                expected_generation: epoch,
                found_generation: ended,
                id_raw: panel.0,
            },
            "the {what} verb is refused on the same handle check"
        );
    }
    assert_eq!(
        panels
            .borrow_mut()
            .focus_panel(panel, ended, WorkspaceId::new(9))
            .expect_err("a displaced generation cannot focus the successor's panel"),
        PanelError::StaleHandle {
            expected_generation: epoch,
            found_generation: ended,
            id_raw: panel.0,
        },
        "focus does not route a displaced generation's command either"
    );

    // 3. The control arm: a generation that never bound the container is a
    //    conflict, not a release. Without this the row would pass for a fence
    //    that answered one variant for every foreign generation.
    assert_eq!(
        panels
            .borrow_mut()
            .mount_panel(panel, controller_generation(9), ViewId::new(31))
            .expect_err("a generation that never bound the container is refused"),
        PanelError::ControllerConflict {
            panel_id: panel,
            bound_generation: Some(successor),
            presented_generation: controller_generation(9),
        },
        "the conflict arm names the binding the caller is not"
    );

    // 4. No refusal moved anything.
    let host = panels.borrow();
    let registry = host.registry();
    assert_eq!(
        host.panel_state(panel, epoch).expect("live container"),
        state_before,
        "the container's lifecycle state survived every refused verb"
    );
    assert_eq!(
        registry.panel_view(panel, epoch).expect("live container"),
        None,
        "no refused mount attached a view: the unmount's result stands"
    );
    assert_eq!(
        host.placement_len(),
        0,
        "and the facade's placement mirror bound no view either"
    );
    assert_eq!(
        host.panel_description(panel, epoch)
            .expect("the container's title and description"),
        described_before,
        "title and description are container facts no refusal touches"
    );
    assert_eq!(
        registry.controller_generation(panel),
        Some(successor),
        "the binding still names the successor"
    );
    assert!(
        registry.is_panel_capability_granted(panel, successor, CREATE),
        "the successor keeps the row its bind installed"
    );
    assert!(
        !registry.is_panel_capability_granted(panel, ended, CREATE),
        "and the released generation reads no grant, refused verb or not"
    );
    assert_eq!(
        registry.error_count("ControllerReleased"),
        2,
        "the released arm ran once per fenced verb"
    );
    assert_eq!(
        registry.error_count("ControllerConflict"),
        1,
        "and the conflict arm ran once, for the foreign generation"
    );
}

/// PGO-1, container-fact half (bitty#2476 residual 9): a reload-driven rebind
/// keeps the container attached to the same leaf, with its lifecycle state and
/// title intact.
///
/// The `PresentationMode` half of PGO-1 is deliberately **not** asserted here.
/// The mode is a workspace fact on the leaf `View`
/// (`crates/bitty-ui/src/view.rs:227`, stamped through the single gate at
/// `crates/bitty-ui/src/presentation.rs:157`), and the panel host owns no layout
/// to read it from: `PanelRuntime` holds a `Placement` mirror of the attachment
/// and a lease table, never a `LayoutNode`
/// (`crates/bitty-runtime/src/registry/host.rs:115`), and
/// `PanelRuntime::rebind_panel` (`:452`) writes only the controller binding, the
/// lease tenure, and the retired-ceiling tombstone — its own contract says the
/// presentation facts survive untouched because nothing at that seam presents
/// (`:435`). An assertion over a tree this test built would therefore stay green
/// with the reload edge deleted, which is the shape of a tautology. The header's
/// blocking note records what homing the clause needs.
///
/// Fails for an edge that does not rebind (the controller generation stays N),
/// one that retires the container (no attachment), one that unmounts it (the
/// lifecycle state leaves `Mounted`), or one that rewrites the title.
#[test]
fn pgo_1_container_facts_survive_the_rebind_edge() {
    let (mut runtime, plugin, panels) = edge_fixture("presentation", "acme.presentation");
    let view = ViewId::new(23);
    let (panel, epoch) = bound_container(&panels, &plugin, 10, view.0);

    let state_before = panels
        .borrow()
        .panel_state(panel, epoch)
        .expect("live container");

    runtime.reload(&plugin).expect("reload");

    let host = panels.borrow();
    let registry = host.registry();
    assert_eq!(
        registry.controller_generation(panel),
        Some(controller_generation(2)),
        "the edge replaced the controller generation"
    );
    assert_eq!(
        host.placement_view_of(panel),
        Some(view),
        "the container stays attached to the leaf it was mounted on"
    );
    assert_eq!(
        registry.panel_view(panel, epoch).expect("live container"),
        Some(view),
        "the record's attachment is the same view id"
    );
    assert_eq!(
        host.panel_state(panel, epoch).expect("live container"),
        state_before,
        "the container's lifecycle state survives the swap"
    );
    assert_eq!(
        host.panel_description(panel, epoch)
            .expect("the container's title and description"),
        (Some("Notes".to_string()), Some("acme notes".to_string())),
        "title and description are container facts the edge leaves alone"
    );
}

/// AC-3 on the production edge (bitty#2476, OQ-084 D.4): the generation's
/// overlay input-capture session is revoked by the plugin runtime, and the
/// container's release record names it with the four families the registry
/// released itself.
///
/// The capture table lives in the plugin runtime, so this is the only row where
/// the *producer* of the fact is the real `dispose_inner`: the test acquires the
/// session the way a focusable overlay does, disposes the generation, and reads
/// the record through the panel host. A revoke whose answer never reaches the
/// edge, or an edge that reports it but a registry that does not fold it in,
/// leaves `capture_sessions` at `0` and fails here.
#[test]
fn the_generation_edge_records_the_capture_session_it_revoked() {
    let (mut runtime, plugin, panels) = edge_fixture("capture", "acme.edgecapture");
    let (panel, _epoch) = bound_container(&panels, &plugin, 7, 13);
    let ended = controller_generation(1);

    // The generation holds a live input capture, exactly as a plugin that opened
    // a focusable overlay and acquired it would.
    runtime
        .overlay_capture()
        .borrow_mut()
        .acquire(
            plugin.as_str(),
            4_242,
            Instant::now() + Duration::from_secs(60),
        )
        .expect("the generation takes the capture");
    assert!(runtime.overlay_capture().borrow().is_active());

    runtime.dispose(&plugin).expect("dispose");

    let host_ref = panels.borrow();
    let release = host_ref
        .registry()
        .unbind_release_record(panel)
        .expect("the edge wrote the container's release record");
    assert_eq!(
        release.generation, ended,
        "the record names the ended generation"
    );
    assert_eq!(
        release.capture_sessions, 1,
        "the capture family the edge revoked is named with the other families"
    );
    assert_eq!(
        release.grants, 2,
        "the registry-released families are still counted"
    );
    assert_eq!(release.total(), 3);
}

/// The same edge for a generation that held no capture session: the record
/// counts `0`, so the field is the edge's answer rather than a constant.
///
/// This is the negative half of the row above. A record that always carried `1`
/// (or an implementation that reported a revoke it never made) passes the
/// capture row and fails this one.
#[test]
fn a_generation_edge_without_a_capture_records_none() {
    let (mut runtime, plugin, panels) = edge_fixture("nocapture", "acme.edgenocapture");
    let (panel, _epoch) = bound_container(&panels, &plugin, 8, 14);
    let ended = controller_generation(1);
    assert!(
        !runtime.overlay_capture().borrow().is_active(),
        "the fixture never acquired a capture"
    );

    runtime.dispose(&plugin).expect("dispose");

    let host_ref = panels.borrow();
    let release = host_ref
        .registry()
        .unbind_release_record(panel)
        .expect("the edge wrote the container's release record");
    assert_eq!(release.generation, ended);
    assert_eq!(
        release.capture_sessions, 0,
        "a generation that held no capture reports none"
    );
    assert_eq!(release.grants, 2);
    assert_eq!(release.total(), 2);
}
