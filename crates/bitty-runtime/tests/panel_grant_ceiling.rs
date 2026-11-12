#![forbid(unsafe_code)]
//! OQ-084 B-4/D.2: the predecessor grant ceiling survives the unbind edge.
//!
//! The edge release (`A.2`, routed through `C.2`) removes `(PanelId, N)`'s
//! grant row before `N+1` binds, so phase 0 must not read the live table. The
//! release writes a retired-ceiling tombstone first, even when the generation
//! held nothing, and phase 0 reads that tombstone on the reload path or the live
//! row when it displaces a still-bound predecessor.
//!
//! The contract these tests pin:
//!
//! - a tombstone for the record's last controller generation bounds the
//!   successor; an empty tombstone is a real empty ceiling;
//! - a panel record that names a previous controller but has no matching
//!   tombstone refuses the bind (`PanelError::CeilingLost`) instead of granting
//!   the whole resolution;
//! - panel disposal drops the tombstone with the container, so a later panel is
//!   a first bind and not a reload of the retired identity;
//! - the table holds one entry per panel and is bounded by the live-panel
//!   budget, so another panel's edge can never evict a protected ceiling.
//!
//! The lost-tombstone refusal is also unit-tested inside `registry::panel`,
//! where the invariant can be violated deliberately; the public path never
//! produces that state.

use bitty_runtime::registry::{
    Generation, MAX_PANELS_PER_WINDOW, MAX_RETAINED_GRANT_CEILINGS, PanelError, PanelId,
    PanelRegistry, PanelRegistryConfig, PanelType,
};
use bitty_ui::ViewId;
use std::collections::BTreeSet;

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const OVERLAY: &str = "panel.overlay";

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds")
}

fn registry_with_max_panels(max_panels_per_window: usize) -> PanelRegistry {
    let config = PanelRegistryConfig {
        max_panels_per_window,
        ..PanelRegistryConfig::default()
    };
    PanelRegistry::new(config).expect("panel registry builds")
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

/// Creates, mounts, and grants `grants` to a panel, returning its handle.
fn bound_panel(reg: &mut PanelRegistry, grants: &[&str], view: u64) -> (PanelId, Generation) {
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    reg.mount_panel(handle.id, handle.generation, ViewId::new(view))
        .expect("mount panel");
    for capability in grants {
        reg.grant_panel_capability(handle.id, handle.generation, capability)
            .expect("grant panel capability");
    }
    (handle.id, handle.generation)
}

#[test]
fn an_edge_release_retains_the_generation_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 1);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");

    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the terminal edge writes the retired-ceiling tombstone");
    assert_eq!(ceiling.ended_generation, generation);
    assert_eq!(ceiling.grants, set(&[CREATE, FOCUS]));
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(generation),
        "the record retains the ended controller across the unbind"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, generation, CREATE),
        "the live row is still released: retention is a copy, not a lease"
    );
    assert!(
        reg.panel_state(panel, generation).is_ok(),
        "the unbind release keeps the panel container and its record"
    );
}

#[test]
fn a_bind_after_an_edge_release_reads_the_retired_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 2);

    // This exercises the release/derive/install seam on one live panel record:
    // the edge release runs explicitly, then the bind installs against the
    // tombstone. It is not a reload to a new allocation generation; the
    // registry has no path that re-binds the same `PanelId` under a new
    // generation, so the seam is what can honestly be claimed here.
    assert!(
        reg.release_generation_scope(panel, generation)
            .expect("edge release"),
        "an edge with a live row reports the removal"
    );
    assert!(!reg.is_panel_capability_granted(panel, generation, CREATE));

    // The bind resolves three capabilities and consents to none. Reading the
    // drained table would commit `∅` here; reading no tombstone as "no
    // ceiling" would commit all of `R`.
    let granted = reg
        .install_rebound_grants(
            panel,
            generation,
            &set(&[CREATE, FOCUS, OVERLAY]),
            &BTreeSet::new(),
        )
        .expect("rebind install");
    assert_eq!(
        granted,
        set(&[CREATE, FOCUS]),
        "P still bounds the install after the release"
    );
    assert!(reg.is_panel_capability_granted(panel, generation, CREATE));
    assert!(!reg.is_panel_capability_granted(panel, generation, OVERLAY));
}

#[test]
fn a_repeated_edge_release_leaves_the_retired_ceiling_untouched() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 7);

    assert!(
        reg.release_generation_scope(panel, generation)
            .expect("first release"),
        "an edge with a live row reports the removal"
    );
    // `A.2`: a bind repeats these releases in phase 1 as defence in depth, so
    // the second call is a normal occurrence, not an error path.
    assert!(
        !reg.release_generation_scope(panel, generation)
            .expect("repeat release"),
        "the repeat finds no live row"
    );

    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the first release retained the ceiling");
    assert_eq!(ceiling.ended_generation, generation);
    assert_eq!(
        ceiling.grants,
        set(&[CREATE, FOCUS]),
        "a repeat with no live row must not rewrite the ceiling as the empty set"
    );

    let granted = reg
        .install_rebound_grants(
            panel,
            generation,
            &set(&[CREATE, FOCUS, OVERLAY]),
            &BTreeSet::new(),
        )
        .expect("install after the repeat");
    assert_eq!(
        granted,
        set(&[CREATE, FOCUS]),
        "the ceiling the first release wrote still bounds the install"
    );
}

#[test]
fn panel_disposal_drops_the_retired_ceiling_with_the_container() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 8);
    reg.dispose_panel(panel, generation).expect("dispose");

    assert!(
        reg.retained_grant_ceiling(panel).is_none(),
        "A.2/PGO-23: the tombstone is dropped on panel disposal"
    );
    assert_eq!(reg.retained_grant_ceiling_count(), 0);
    assert_eq!(
        reg.last_controller_generation(panel),
        None,
        "a disposed record names no controller"
    );
    assert!(!reg.is_panel_capability_granted(panel, generation, CREATE));
    assert!(
        matches!(
            reg.install_rebound_grants(panel, generation, &set(&[CREATE]), &BTreeSet::new()),
            Err(PanelError::NotFound { kind: "panel", .. })
        ),
        "a disposed identity has no record to bind"
    );
}

#[test]
fn a_new_panel_after_disposal_is_a_first_bind_not_a_reload() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 9);
    reg.dispose_panel(panel, generation).expect("dispose");

    // The old gap kept the tombstone for the disposed identity: no path could
    // bind that identity again, and the entry could never be reclaimed. The
    // contract drops it with the container, so nothing of the predecessor is
    // readable by any successor.
    assert!(reg.retained_grant_ceiling(panel).is_none());
    assert_eq!(reg.retained_grant_ceiling_count(), 0);

    let successor = reg
        .create_panel(PanelType::Helper, None)
        .expect("create successor");
    assert_ne!(
        successor.id, panel,
        "the successor a new create returns is a new identity"
    );
    assert_eq!(
        reg.last_controller_generation(successor.id),
        None,
        "a new container has no previous controller"
    );
    assert!(
        reg.retained_grant_ceiling(successor.id).is_none(),
        "no ceiling is keyed under the new identity"
    );

    // A panel with no previous controller is the absent-`P` case: the first
    // bind commits `R` because `panel.create` is the authorization. This is not
    // authority carry-over — the disposed ceiling was dropped, so the decision
    // record's reload path has no predecessor to read.
    let granted = reg
        .install_rebound_grants(
            successor.id,
            successor.generation,
            &set(&[CREATE, FOCUS, OVERLAY]),
            &BTreeSet::new(),
        )
        .expect("first bind");
    assert_eq!(granted, set(&[CREATE, FOCUS, OVERLAY]));
    assert_eq!(
        reg.retained_grant_ceiling_count(),
        0,
        "no tombstone survived the disposal"
    );
    assert_eq!(reg.error_count("CeilingLost"), 0);
}

#[test]
fn an_absent_predecessor_is_not_an_empty_ceiling() {
    let mut reg = registry();
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    assert_eq!(reg.last_controller_generation(handle.id), None);

    // A first bind has no predecessor, so the create verb is the authorization
    // and the whole resolution is granted without a permission diff.
    let granted = reg
        .install_rebound_grants(
            handle.id,
            handle.generation,
            &set(&[CREATE, FOCUS]),
            &BTreeSet::new(),
        )
        .expect("first bind install");
    assert_eq!(granted, set(&[CREATE, FOCUS]));
    assert_eq!(reg.error_count("CeilingLost"), 0);
}

#[test]
fn an_empty_predecessor_ceiling_is_a_real_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[], 3);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");

    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("a predecessor that existed is a predecessor");
    assert!(ceiling.grants.is_empty());
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(generation),
        "the record remembers that a controller ended even with no grants"
    );

    let granted = reg
        .install_rebound_grants(panel, generation, &set(&[CREATE]), &BTreeSet::new())
        .expect("an empty ceiling is real, not missing: the bind is not refused");
    assert!(
        granted.is_empty(),
        "existed-and-held-nothing must narrow to nothing, not widen to R"
    );
    assert_eq!(
        reg.error_count("CeilingLost"),
        0,
        "an empty tombstone never produces a lost-record refusal"
    );
}

#[test]
fn an_empty_predecessor_ceiling_admits_only_consented_widening() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[], 13);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");

    let granted = reg
        .install_rebound_grants(panel, generation, &set(&[CREATE, FOCUS]), &set(&[FOCUS]))
        .expect("an empty ceiling is real, not missing: the bind is not refused");
    assert_eq!(
        granted,
        set(&[FOCUS]),
        "(R ∩ ∅) ∪ C = C: only the consented widening survives"
    );
}

#[test]
fn consent_widens_only_inside_the_successor_resolution() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 4);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");

    // `OVERLAY` is consented but never requested, so it cannot enter through C.
    let granted = reg
        .install_rebound_grants(
            panel,
            generation,
            &set(&[CREATE, FOCUS]),
            &set(&[FOCUS, OVERLAY]),
        )
        .expect("rebind install");
    assert_eq!(granted, set(&[CREATE, FOCUS]));
}

/// A live row on a record that binds nothing is not a ceiling (OQ-084 D.2
/// phase 0, `N-15`).
///
/// The state is what an edge release leaves behind when its tombstone write did
/// not complete: the predecessor ended, the release did not, and the row is
/// diagnostic evidence. It used to be read as the ceiling — which handed the
/// successor a capability the predecessor's released set never contained. The
/// still-bound half of `PGO-22(a)` (the live row wins over a stale tombstone)
/// lives in `registry::panel::rebind_panel_tests`, where the tombstone can be
/// held beside a binding.
#[test]
fn a_live_row_on_an_unbound_record_is_refused_not_consumed() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 10);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    // The failed-release signature: the writers fence on the allocation epoch,
    // so a row can appear under it after the binding is gone.
    reg.grant_panel_capability(panel, generation, FOCUS)
        .expect("orphan row");

    let error = reg
        .install_rebound_grants(
            panel,
            generation,
            &set(&[CREATE, FOCUS, OVERLAY]),
            &BTreeSet::new(),
        )
        .expect_err("the orphan row is not a source");
    assert!(
        matches!(error, PanelError::CeilingLost { .. }),
        "the refusal is the lost-ceiling class, got {error:?}"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, generation, CREATE),
        "nothing is installed from the orphan row"
    );
    assert!(
        reg.is_panel_capability_granted(panel, generation, FOCUS),
        "the orphan row is left untouched as evidence"
    );
}

#[test]
fn a_successful_install_keeps_the_ceiling_until_the_bind_commits() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 5);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    assert!(reg.retained_grant_ceiling(panel).is_some());

    reg.install_rebound_grants(panel, generation, &set(&[CREATE]), &BTreeSet::new())
        .expect("rebind install");
    assert!(
        reg.retained_grant_ceiling(panel).is_some(),
        "D.2 phase 2 step 3 retires the ceiling, not step 1: an install that \
         never became a binding must not lose it"
    );
}

#[test]
fn the_last_controller_generation_survives_the_unbind_edge() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 11);
    assert_eq!(reg.last_controller_generation(panel), None);

    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(generation),
        "the field is retained across the unbind"
    );

    reg.install_rebound_grants(panel, generation, &set(&[CREATE]), &BTreeSet::new())
        .expect("rebind install");
    assert_eq!(
        reg.last_controller_generation(panel),
        Some(generation),
        "a successor bind does not erase the retained predecessor"
    );
}

#[test]
fn a_granted_panel_that_never_mounted_still_retains_its_ceiling() {
    let mut reg = registry();
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    reg.grant_panel_capability(handle.id, handle.generation, CREATE)
        .expect("grant");
    // Holding a grant makes the generation a controller even without a mount,
    // so its edge must not silently drop the ceiling.
    reg.release_generation_scope(handle.id, handle.generation)
        .expect("edge release");
    let ceiling = reg
        .retained_grant_ceiling(handle.id)
        .expect("a granted generation is a predecessor");
    assert_eq!(ceiling.grants, set(&[CREATE]));

    reg.dispose_panel(handle.id, handle.generation)
        .expect("dispose");
    assert!(
        reg.retained_grant_ceiling(handle.id).is_none(),
        "the later panel close reclaims the entry"
    );
}

#[test]
fn a_rolled_back_create_leaves_no_ceiling() {
    let mut reg = registry();
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    // A create that never bound (the scene host's rollback path) is not a
    // predecessor; retaining for it would narrow a later first bind.
    reg.dispose_panel(handle.id, handle.generation)
        .expect("rollback dispose");
    assert!(reg.retained_grant_ceiling(handle.id).is_none());
    assert_eq!(reg.retained_grant_ceiling_count(), 0);
    assert_eq!(reg.last_controller_generation(handle.id), None);
}

#[test]
fn retired_ceilings_are_bounded_by_live_panels_and_never_evicted() {
    let mut reg = registry_with_max_panels(MAX_PANELS_PER_WINDOW);
    let mut handles = Vec::with_capacity(MAX_PANELS_PER_WINDOW);
    for index in 0..MAX_PANELS_PER_WINDOW {
        let (panel, generation) = bound_panel(&mut reg, &[CREATE], 2_000 + index as u64);
        reg.release_generation_scope(panel, generation)
            .expect("edge release");
        handles.push((panel, generation));
    }

    // One entry per live panel, exactly at the configured live-panel budget.
    assert_eq!(reg.panel_count(), MAX_PANELS_PER_WINDOW);
    assert_eq!(reg.retained_grant_ceiling_count(), MAX_PANELS_PER_WINDOW);
    assert!(reg.retained_grant_ceiling_count() <= MAX_RETAINED_GRANT_CEILINGS);
    for (panel, _) in &handles {
        let ceiling = reg
            .retained_grant_ceiling(*panel)
            .expect("every live panel keeps its ceiling at the bound");
        assert_eq!(ceiling.grants, set(&[CREATE]));
    }
    assert!(
        matches!(
            reg.create_panel(PanelType::Helper, None),
            Err(PanelError::TooManyPanels { .. })
        ),
        "the live-panel budget, not the ceiling table, is the admission limit"
    );

    // Saturating the table with live panels did not evict the oldest entry.
    let (first_panel, first_generation) = handles[0];
    assert!(reg.retained_grant_ceiling(first_panel).is_some());
    let granted = reg
        .install_rebound_grants(
            first_panel,
            first_generation,
            &set(&[CREATE, FOCUS, OVERLAY]),
            &BTreeSet::new(),
        )
        .expect("rebind install");
    assert_eq!(granted, set(&[CREATE]));
}

#[test]
fn many_panel_edges_do_not_evict_a_long_lived_panels_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 12);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    assert_eq!(reg.retained_grant_ceiling_count(), 1);

    // The T-25 escalation: 64+ later panels reach their edges and are closed.
    // A disposal that kept its tombstone would fill the old 64-entry table and
    // evict the lowest `PanelId` — this long-lived panel — before its successor
    // bound. Closing each panel reclaims its entry instead.
    for index in 0..MAX_RETAINED_GRANT_CEILINGS + 1 {
        let (other, other_generation) = bound_panel(&mut reg, &[CREATE], 10_000 + index as u64);
        reg.release_generation_scope(other, other_generation)
            .expect("other edge release");
        reg.dispose_panel(other, other_generation)
            .expect("other panel close");
    }

    assert_eq!(
        reg.retained_grant_ceiling_count(),
        1,
        "only the long-lived panel still has a tombstone"
    );
    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the long-lived panel's ceiling was not evicted");
    assert_eq!(ceiling.ended_generation, generation);
    assert_eq!(ceiling.grants, set(&[CREATE]));

    let granted = reg
        .install_rebound_grants(
            panel,
            generation,
            &set(&[CREATE, FOCUS, OVERLAY]),
            &BTreeSet::new(),
        )
        .expect("rebind install");
    assert_eq!(
        granted,
        set(&[CREATE]),
        "the successor is still narrowed by the original ceiling"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, generation, FOCUS),
        "no consent-free widening survived the pressure"
    );
    assert!(
        !reg.is_panel_capability_granted(panel, generation, OVERLAY),
        "no consent-free widening survived the pressure"
    );
    assert_eq!(reg.error_count("CeilingLost"), 0);
}

#[test]
fn whole_registry_disposal_drops_retired_ceilings() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 6);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    assert_eq!(reg.retained_grant_ceiling_count(), 1);
    reg.dispose();
    assert_eq!(reg.retained_grant_ceiling_count(), 0);
}
