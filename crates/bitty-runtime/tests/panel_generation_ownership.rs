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
//! The three `..._generation_edge_...` rows at the end drive the **plugin
//! runtime's** generation edge (`PluginRuntime::reload` / `PluginRuntime::dispose`,
//! residuals 2 and 4) with
//! [`PanelGenerationPort`](bitty_runtime::plugin_runtime::PanelGenerationPort)
//! installed, so the caller of the production bind is the edge and not the
//! test. Their port is the same eight-line delegation the composition root
//! installs (`crates/bitty-terminal/src/plugin_runtime.rs`), so what they pin is
//! the wiring: an edge that never calls the bind, or one that presents a
//! placeholder without the release, fails them.
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
//! | PGO-1/PGO-2, edge-driven (bitty#2476 residual 2) | here — `the_generation_edge_rebinds_the_container_and_moves_the_controller` |
//! | Residual 7, per-owner retention | here — `a_tombstone_is_retained_for_the_owner_that_stamped_it` |
//! | Residual 7(a), no owner on an epoch edge (bitty#2476) | here — `an_allocation_epoch_edge_retains_no_owner_from_the_previous_edge`, `a_bind_after_an_allocation_epoch_edge_is_not_measured_against_the_previous_owner` |
//! | PGO-22(d) through the edge, no widening | here — `a_refused_edge_bind_leaves_the_container_unbound_without_widening_its_grants` |
//! | PGO-6, reload racing a mount (bitty#2476 residual 9) | here — `a_mount_that_races_a_reload_commits_nothing_and_the_successor_mounts_cleanly` |
//! | PGO-9, capability gate on a guessed `PanelId` (bitty#2476 residual 9) | here — `a_guessed_panel_id_is_denied_at_the_grant_gate_before_any_state_access`; the deny-by-default half is also pinned by `tests/cw_contracts.rs:127` |
//! | PGO-13, disposal reclaim (bitty#2476 residual 9) | here — `a_disposal_reclaims_every_registry_owned_family_to_the_pre_creation_baseline` (queue, commands, grants, MRU, overlays, counts) and `a_disposal_releases_the_lease_entry_and_the_placement_mirror` (lease, attachment) |
//! | `N -> N+1 -> N` retired generation | here — `a_retired_generation_cannot_be_rebound_over_its_successor` |
//!
//! Still without a row in this crate: PGO-4/PGO-5/PGO-20 (crash and disable
//! recovery driven by the plugin host). Two rows are named here with the seam
//! that blocks them rather than implied covered:
//!
//! - **PGO-11** (provider version records): no production path presents
//!   `incompatible` — `PlaceholderReason::Incompatible` has no producer
//!   (`dispose_inner` passes only `Unloaded`/`Disabled`/`Reloaded`, and
//!   `PanelRegistry::present_placeholder` takes the reason token from its
//!   caller), and the only versioned provider record,
//!   `PanelProviderManifest`, refuses every version other than 1 at
//!   `registry/provider.rs:138`, so there is no newer-provider-over-older-record
//!   direction and no tolerant reader to drive. The persisted panel manifest
//!   (`bitty-ui/src/panel_persist.rs:367`) has a version gate and unknown-field
//!   tolerance, but it carries identity, not provider content.
//! - **PGO-15** (verbs from a displaced generation): only `mount_panel`
//!   (`registry/panel.rs:1930`, fence call `:1952`) and `present_placeholder`
//!   (`:2294`, fence call `:2348`) revalidate at use through
//!   `panel_state_at_controller_fence` (`:1699`); every other verb resolves
//!   through `get_panel`/`get_panel_mut` (`:1754`/`:1776`), which compares the
//!   presented generation to the allocation epoch and answers `StaleHandle`, so
//!   the row's "refused with `ControllerConflict`" does not hold for the whole
//!   verb set. Its read half has no accessor either: queued events (`:1224`) and
//!   the panel scene slot (`registry/host.rs:1217`) are keyed by panel, not by
//!   generation.
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

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::PluginId;
use bitty_runtime::plugin_runtime::{
    PanelEdgeBind, PanelEdgeRelease, PanelEdgeReport, PanelGenerationPort, PluginRuntime,
    PluginRuntimeConfig, SettingsSource, SnapshotSource, controller_generation,
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
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), "bitty = bitty or {}\n").expect("init");
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
/// family and returns the panel counts to the pre-creation baseline.
///
/// This is FS-P4's reclaim criterion read structurally. PB-3's tolerance is a
/// wall-clock budget for a session that has recycled generations; the counts
/// asserted here are the state a disposal either releases or leaks, so exact
/// equality with the baseline is inside any non-negative tolerance. The
/// container is disposed while it still holds a queue, a command, a grant row,
/// an MRU entry, and an attributed overlay — every registry-owned family the row
/// names — and each is asserted non-baseline before the disposal, so the
/// reclaimed assertion is not vacuous.
///
/// The container never binds a controller here: on a bound panel the public
/// verbs are addressed by the controller generation while `subscribe`, focus,
/// and command registration validate the allocation epoch, so this is the one
/// state in which all the families can coexist. The bound panel's disposal runs
/// the same release for the controller key, which
/// `tests/panel_grant_ceiling.rs:182` and `tests/panel_unbind_release.rs` cover.
///
/// Fails for a disposal that leaves any one family behind: the queue, the
/// command, the grant row, the MRU entry, the attributed overlay, the workspace
/// bucket, the dropped tombstone, or the panel count.
#[test]
fn a_disposal_reclaims_every_registry_owned_family_to_the_pre_creation_baseline() {
    let mut reg = registry();
    let ws = WorkspaceId::new(21);
    let view = ViewId::new(41);
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

    // The container really holds every family before the disposal.
    assert_eq!(reg.panel_count(), 1);
    assert_eq!(reg.attributed_workspace_count(), 1);
    assert_eq!(reg.mru_order(ws), vec![handle.id]);
    assert_eq!(reg.focused_panel(ws), Some(handle.id));
    assert_eq!(reg.command_owner(command), Some(handle.id));
    assert_eq!(reg.bus_events_for_panel(handle.id), 1);
    assert_eq!(reg.attributed_overlay_count(), 1);
    assert!(reg.is_panel_capability_granted(handle.id, epoch, CREATE));

    reg.dispose_panel(handle.id, epoch).expect("dispose panel");

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
    assert_eq!(reg.mru_order(ws), Vec::new(), "the MRU entry is released");
    assert_eq!(reg.focused_panel(ws), None, "and it holds no focus");
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
    assert!(
        !reg.is_panel_capability_granted(handle.id, epoch, CREATE),
        "the grant row is released"
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
