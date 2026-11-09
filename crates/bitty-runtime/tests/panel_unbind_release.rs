#![forbid(unsafe_code)]
//! OQ-084 A.2/D.4: the unbind edge releases every generation-owned family.
//!
//! `release_generation_scope` used to release only the `(PanelId, N)` grant
//! row. A.2 requires the edge to release, for that controller generation:
//!
//! - the panel-scoped grant rows (still copied into the retired-ceiling
//!   tombstone first, so the tombstone behaviour is unchanged);
//! - the panel's queued bus events;
//! - the panel commands `N` registered;
//! - `N`'s overlay attribution.
//!
//! It also requires a per-family release record that names the ended
//! generation and is created even when every family releases nothing, so
//! "released `∅`" stays distinguishable from "the release never ran".
//!
//! These tests drive the public registry path end to end. They fail if any
//! family is left behind or the record is not written.

use bitty_runtime::{
    Generation, UiRect, ViewId,
    registry::{
        BoundedPayload, OverlayKind, PanelError, PanelId, PanelRegistry, PanelRegistryConfig,
        PanelType,
    },
};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const TOPIC: &str = "xuepoo.owner:status";

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds")
}

fn mounted_panel(reg: &mut PanelRegistry, view: u64) -> (PanelId, Generation) {
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");
    reg.mount_panel(handle.id, handle.generation, ViewId::new(view))
        .expect("mount panel");
    (handle.id, handle.generation)
}

fn rect() -> UiRect {
    UiRect::new(0, 0, 20, 10)
}

/// The edge releases grants, events, commands, and overlays in one call, and
/// the retired-ceiling tombstone still holds the departed grant set.
#[test]
fn the_edge_releases_every_generation_family() {
    let mut reg = registry();
    let (panel, generation) = mounted_panel(&mut reg, 1);
    reg.grant_panel_capability(panel, generation, CREATE)
        .expect("grant create");
    reg.grant_panel_capability(panel, generation, FOCUS)
        .expect("grant focus");
    reg.register_command(panel, generation, "helper.run:go")
        .expect("register command");

    let topic = reg.declare_topic(TOPIC).expect("declare topic");
    reg.subscribe(panel, generation, &topic)
        .expect("subscribe panel");
    for payload in ["one", "two"] {
        reg.publish(&topic, BoundedPayload::try_new(payload).expect("payload"))
            .expect("publish");
    }
    let first = reg
        .create_panel_overlay(
            panel,
            generation,
            OverlayKind::NonModal,
            rect(),
            "first",
            None,
        )
        .expect("attributed overlay");
    let second = reg
        .create_panel_overlay(
            panel,
            generation,
            OverlayKind::Tooltip,
            rect(),
            "second",
            None,
        )
        .expect("attributed overlay");

    // Preconditions: every family is actually populated before the edge.
    assert_eq!(reg.bus_events_for_panel(panel), 2);
    assert_eq!(reg.command_owner("helper.run:go"), Some(panel));
    assert_eq!(reg.overlay_len(), 2);
    assert_eq!(reg.attributed_overlay_count(), 2);
    assert_eq!(reg.overlay_attribution_of(first), Some((panel, generation)));
    assert_eq!(
        reg.overlay_attribution_of(second),
        Some((panel, generation))
    );
    assert_eq!(reg.unbind_release_count(panel), None);

    assert!(
        reg.release_generation_scope(panel, generation)
            .expect("edge release"),
        "the edge reports the live grant row it removed"
    );

    // Every family is released.
    assert!(!reg.is_panel_capability_granted(panel, generation, CREATE));
    assert_eq!(reg.bus_events_for_panel(panel), 0, "bus queues drained");
    assert_eq!(reg.command_owner("helper.run:go"), None, "commands freed");
    assert_eq!(reg.overlay_len(), 0, "attributed overlays dismissed");
    assert_eq!(reg.attributed_overlay_count(), 0);

    // The per-family record names the ended generation and counts each family.
    let record = reg
        .unbind_release_record(panel)
        .expect("the edge writes a release record");
    assert_eq!(record.generation, generation);
    assert_eq!(record.grants, 2);
    assert_eq!(record.events, 2);
    assert_eq!(record.commands, 1);
    assert_eq!(record.overlays, 2);
    assert_eq!(record.total(), 7);
    assert!(record.released_any());
    assert_eq!(reg.unbind_release_count(panel), Some(7));

    // The tombstone behaviour is unchanged: the ceiling is a copy of the
    // departed grant set and the record still names the ended controller.
    let ceiling = reg
        .retained_grant_ceiling(panel)
        .expect("the edge still writes the retired-ceiling tombstone");
    assert_eq!(ceiling.ended_generation, generation);
    assert_eq!(ceiling.grants.len(), 2);
    assert_eq!(reg.last_controller_generation(panel), Some(generation));
    assert!(
        reg.panel_state(panel, generation).is_ok(),
        "the edge keeps the container and its record"
    );
}

/// A generation that holds nothing still gets a record: `Some(0)` is an edge
/// that released nothing, `None` is no edge at all.
#[test]
fn a_release_that_frees_nothing_still_writes_a_record() {
    let mut reg = registry();
    let handle = reg
        .create_panel(PanelType::Helper, None)
        .expect("create panel");

    assert!(
        !reg.release_generation_scope(handle.id, handle.generation)
            .expect("edge release"),
        "a generation with no live row reports no removal"
    );

    let record = reg
        .unbind_release_record(handle.id)
        .expect("the record exists even when every family releases nothing");
    assert_eq!(record.generation, handle.generation);
    assert_eq!(record.grants, 0);
    assert_eq!(record.events, 0);
    assert_eq!(record.commands, 0);
    assert_eq!(record.overlays, 0);
    assert!(!record.released_any());
    assert_eq!(reg.unbind_release_count(handle.id), Some(0));

    let ceiling = reg
        .retained_grant_ceiling(handle.id)
        .expect("an empty ceiling is still a ceiling");
    assert!(ceiling.grants.is_empty());
}

/// The phase-1 repeat is idempotent: it neither re-releases nor rewrites the
/// record the first edge wrote.
#[test]
fn a_repeated_release_does_not_double_count() {
    let mut reg = registry();
    let (panel, generation) = mounted_panel(&mut reg, 2);
    reg.grant_panel_capability(panel, generation, CREATE)
        .expect("grant create");
    reg.register_command(panel, generation, "helper.run:go")
        .expect("register command");

    assert!(
        reg.release_generation_scope(panel, generation)
            .expect("first release")
    );
    let first = *reg.unbind_release_record(panel).expect("record");
    assert_eq!(first.total(), 2);

    assert!(
        !reg.release_generation_scope(panel, generation)
            .expect("repeat release"),
        "the repeat removes no row"
    );
    let after = *reg.unbind_release_record(panel).expect("record");
    assert_eq!(after, first, "the repeat leaves the first record intact");
    assert_eq!(reg.unbind_release_count(panel), Some(2));
}

/// An overlay without attribution is not the generation's to release.
#[test]
fn an_unattributed_overlay_survives_the_edge() {
    let mut reg = registry();
    let (panel, generation) = mounted_panel(&mut reg, 3);
    let unattributed = reg
        .create_overlay(OverlayKind::NonModal, rect(), "global", None)
        .expect("unattributed overlay");
    let attributed = reg
        .create_panel_overlay(
            panel,
            generation,
            OverlayKind::Tooltip,
            rect(),
            "owned",
            None,
        )
        .expect("attributed overlay");
    assert_eq!(reg.overlay_len(), 2);

    reg.release_generation_scope(panel, generation)
        .expect("edge release");

    assert_eq!(reg.overlay_len(), 1, "only the attributed overlay is gone");
    assert_eq!(reg.overlay_attribution_of(attributed), None);
    assert_eq!(reg.overlay_attribution_of(unattributed), None);
    assert_eq!(reg.unbind_release_count(panel), Some(1));
}

/// Attribution is validated at creation: a stale generation cannot claim a
/// surface, and no overlay is allocated for the refused call.
#[test]
fn a_stale_generation_cannot_claim_an_overlay() {
    let mut reg = registry();
    let (panel, generation) = mounted_panel(&mut reg, 4);
    let stale = Generation(generation.get() + 1);

    assert!(matches!(
        reg.create_panel_overlay(panel, stale, OverlayKind::NonModal, rect(), "x", None),
        Err(PanelError::StaleHandle { .. })
    ));
    assert_eq!(reg.overlay_len(), 0);
    assert_eq!(reg.attributed_overlay_count(), 0);
    assert_eq!(reg.unbind_release_count(panel), None);
}

/// The record is a container fact: disposal drops it with the panel, exactly
/// like the tombstone.
#[test]
fn the_release_record_is_dropped_with_the_container() {
    let mut reg = registry();
    let (panel, generation) = mounted_panel(&mut reg, 5);
    reg.grant_panel_capability(panel, generation, CREATE)
        .expect("grant create");
    reg.release_generation_scope(panel, generation)
        .expect("edge release");
    assert!(reg.unbind_release_record(panel).is_some());

    reg.dispose_panel(panel, generation).expect("dispose panel");

    assert!(reg.unbind_release_record(panel).is_none());
    assert_eq!(reg.unbind_release_count(panel), None);
    assert_eq!(reg.unbind_release_record_count(), 0);
    assert!(reg.retained_grant_ceiling(panel).is_none());
}
