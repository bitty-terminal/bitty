#![forbid(unsafe_code)]
//! OQ-084 B-4: the predecessor grant ceiling survives the unbind edge.
//!
//! The third review's finding: the edge release (`A.2`, routed through `C.2`)
//! removes `(PanelId, N)`'s grant rows before `N+1` binds, so phase 0 read an
//! empty table and `grant(n+1) = (R ∩ ∅) ∪ C = C` — a reloaded generation that
//! either holds nothing or (worse) is read as having no ceiling at all. The fix
//! is one answer for where `P` lives: the edge writes the departing generation's
//! set into a retained per-panel record, and phase 0 reads that record, never
//! the live table.
//!
//! Every test here fails if the retention is removed, if the derivation reads
//! the live table, or if an empty ceiling is conflated with an absent
//! predecessor.

use bitty_runtime::registry::{
    Generation, MAX_RETAINED_GRANT_CEILINGS, PanelId, PanelRegistry, PanelRegistryConfig, PanelType,
};
use bitty_ui::ViewId;
use std::collections::BTreeSet;

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const OVERLAY: &str = "panel.overlay";

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds")
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
fn the_disposal_edge_retains_the_generation_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 1);
    reg.dispose_panel(panel, generation).expect("dispose");

    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the terminal edge retains the ceiling it releases");
    assert_eq!(ceiling.ended_generation, generation);
    assert_eq!(ceiling.grants, set(&[CREATE, FOCUS]));
    assert!(
        !reg.is_panel_capability_granted(panel, generation, CREATE),
        "the live row is still released: retention is a copy, not a lease"
    );
}

#[test]
fn a_rebind_after_the_edge_is_bounded_by_the_retained_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE, FOCUS], 2);

    // C.5 order: generation N is disposed before N+1 activates, so the edge
    // release runs first and the live row is already gone when the bind starts.
    assert!(
        reg.release_generation_scope(panel, generation)
            .expect("edge release"),
        "an edge with a live row reports the removal"
    );
    assert!(!reg.is_panel_capability_granted(panel, generation, CREATE));

    // The successor resolves three capabilities and consents to none. Reading
    // the drained table would commit `∅` here.
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
        "P still bounds the successor after the edge"
    );
    assert!(reg.is_panel_capability_granted(panel, generation, CREATE));
    assert!(!reg.is_panel_capability_granted(panel, generation, OVERLAY));
}

#[test]
fn an_absent_predecessor_is_not_an_empty_ceiling() {
    let mut reg = registry();
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");

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
}

#[test]
fn a_bound_predecessor_that_held_nothing_is_a_ceiling_of_emptiness() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[], 3);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");

    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("a predecessor that existed is a predecessor");
    assert!(ceiling.grants.is_empty());

    let granted = reg
        .install_rebound_grants(panel, generation, &set(&[CREATE]), &BTreeSet::new())
        .expect("rebind install");
    assert!(
        granted.is_empty(),
        "existed-and-held-nothing must narrow to nothing, not widen to R"
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

#[test]
fn a_successful_install_retires_the_ceiling() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 5);
    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    assert!(reg.retained_grant_ceiling(panel).is_some());

    reg.install_rebound_grants(panel, generation, &set(&[CREATE]), &BTreeSet::new())
        .expect("rebind install");
    assert!(
        reg.retained_grant_ceiling(panel).is_none(),
        "retention ends at the panel's next successful bind"
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
    reg.dispose_panel(handle.id, handle.generation)
        .expect("dispose");
    let ceiling = reg
        .retained_grant_ceiling(handle.id)
        .expect("a granted generation is a predecessor");
    assert_eq!(ceiling.grants, set(&[CREATE]));
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
}

#[test]
fn retained_ceilings_stay_bounded() {
    let mut reg = registry();
    for index in 0..MAX_RETAINED_GRANT_CEILINGS + 5 {
        let (panel, generation) = bound_panel(&mut reg, &[CREATE], 100 + index as u64);
        reg.dispose_panel(panel, generation).expect("dispose");
    }
    assert_eq!(
        reg.retained_grant_ceiling_count(),
        MAX_RETAINED_GRANT_CEILINGS,
        "a long-lived window accumulates no more than the bound"
    );
}

#[test]
fn whole_registry_disposal_drops_retained_ceilings() {
    let mut reg = registry();
    let (panel, generation) = bound_panel(&mut reg, &[CREATE], 6);
    reg.dispose_panel(panel, generation).expect("dispose");
    assert_eq!(reg.retained_grant_ceiling_count(), 1);
    reg.dispose();
    assert_eq!(reg.retained_grant_ceiling_count(), 0);
}
