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
//! | Residual 7, per-owner retention | here — `a_tombstone_is_retained_for_the_owner_that_stamped_it` |
//! | `N -> N+1 -> N` retired generation | here — `a_retired_generation_cannot_be_rebound_over_its_successor` |
//!
//! Still without a row in this crate: PGO-4/PGO-5/PGO-20 (crash and disable
//! recovery driven by the plugin host), PGO-6 (reload racing a mount), PGO-9
//! (`PanelId` guessing across every verb), PGO-11 (provider version records),
//! PGO-13 (disposal reclaim against the PB-3 tolerance), and PGO-15 (verbs from
//! a displaced generation, whose refusal variant the record and the fence do not
//! yet spell the same way). Naming them here keeps the matrix honest about what
//! it covers instead of implying all twenty-three rows live in one file.
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

use std::collections::BTreeSet;

use bitty_plugin_host::PluginId;
use bitty_runtime::{
    Generation, ViewId,
    registry::{PanelError, PanelId, PanelRegistry, PanelRegistryConfig, PanelRuntime, PanelType},
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
