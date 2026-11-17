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
//! - `N`'s overlay attribution;
//! - the capture session `N` lost at the edge, which the plugin runtime revokes
//!   and reports to this registry before the release (bitty#2476 AC-3, OQ-084
//!   D.4) — the one family the registry cannot release itself.
//!
//! It also requires a per-family release record that names the ended
//! generation and is created even when every family releases nothing, so
//! "released `∅`" stays distinguishable from "the release never ran".
//!
//! These tests drive the public registry path end to end. They fail if any
//! family is left behind or the record is not written.

use std::collections::BTreeSet;

use bitty_plugin_host::PluginId;
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

fn owner(name: &str) -> PluginId {
    PluginId::new(name).expect("valid plugin id")
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn empty() -> BTreeSet<String> {
    BTreeSet::new()
}

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

/// One mounted panel on its first bind: `panel` is bound to `generation`, a
/// controller generation (never the allocation epoch), so the edge these rows
/// drive is a real generation edge. The allocation epoch is returned too,
/// because the epoch-keyed writers (`register_command`, overlays) fence on it.
fn bound_panel(reg: &mut PanelRegistry, view: u64) -> (PanelId, Generation, Generation) {
    let (panel, epoch) = mounted_panel(reg, view);
    let generation = epoch.next().expect("controller generation");
    reg.rebind_panel(
        panel,
        generation,
        &owner("acme.notes"),
        &set(&[CREATE]),
        &empty(),
    )
    .expect("first bind");
    (panel, epoch, generation)
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

/// AC-3 (bitty#2476): the capture family releases at the same edge and is named
/// by the same record as the four the registry releases itself.
///
/// The session is **not** this registry's — `OverlayCapture::revoke_plugin` runs
/// in the plugin runtime's generation edge, which cannot name a
/// `PanelRegistry` — so the edge reports it through
/// [`PanelRegistry::note_capture_release`] before the release, and the release
/// folds the report into `GenerationReleaseRecord::capture_sessions`. That makes
/// the fifth family observable where the other four already were: one record,
/// one generation, one total.
///
/// The negative half is the discrimination: without a report the same edge
/// records `0`, so a record that always reads `1` cannot pass this row.
#[test]
fn the_edge_records_the_capture_family_the_generation_edge_revoked() {
    let mut reg = registry();
    let (panel, epoch, generation) = bound_panel(&mut reg, 6);
    reg.register_command(panel, epoch, "helper.run:go")
        .expect("register command");

    // The plugin runtime revokes the generation's capture session before the
    // panel edge, and reports it here.
    reg.note_capture_release(panel, generation)
        .expect("the generation still binds the container");

    assert!(
        reg.release_generation_scope(panel, generation)
            .expect("edge release"),
        "the edge reports the live grant row it removed"
    );

    let record = reg
        .unbind_release_record(panel)
        .expect("the edge writes a release record");
    assert_eq!(record.generation, generation);
    assert_eq!(record.grants, 1);
    assert_eq!(record.commands, 1);
    assert_eq!(
        record.capture_sessions, 1,
        "the capture family is counted with the families the registry released"
    );
    assert_eq!(record.total(), 3);
    assert_eq!(reg.unbind_release_count(panel), Some(3));

    // An edge with no report counts nothing: the field names what the edge
    // revoked, not what every record always claims.
    let (second, _second_epoch, second_generation) = bound_panel(&mut reg, 7);
    reg.release_generation_scope(second, second_generation)
        .expect("edge release");
    let unreported = reg
        .unbind_release_record(second)
        .expect("a record exists even when nothing was reported");
    assert_eq!(unreported.capture_sessions, 0);
    assert_eq!(unreported.total(), 1, "the grant row alone");
}

/// A report is refused for a generation the container does not bind, and it is
/// never carried into a later generation's record.
///
/// The report is per container and names one generation, so a late report (after
/// the release) and a report for another container's generation must fail closed
/// instead of landing on whatever edge runs next. [`PanelError::StaleHandle`] is
/// that refusal, counted like every other stale handle.
#[test]
fn a_capture_report_for_another_generation_is_refused_and_never_carried() {
    let mut reg = registry();
    let (panel, _epoch, first) = bound_panel(&mut reg, 8);
    reg.note_capture_release(panel, first)
        .expect("the binding generation is accepted");
    reg.release_generation_scope(panel, first)
        .expect("edge release");
    assert_eq!(
        reg.unbind_release_record(panel)
            .expect("record")
            .capture_sessions,
        1
    );

    // The successor binds; a report naming the *ended* generation is stale.
    let successor = first.next().expect("successor generation");
    reg.rebind_panel(
        panel,
        successor,
        &owner("acme.notes"),
        &set(&[CREATE]),
        &empty(),
    )
    .expect("rebind");
    let error = reg
        .note_capture_release(panel, first)
        .expect_err("a report for the ended generation is refused");
    assert_eq!(
        error,
        PanelError::StaleHandle {
            expected_generation: successor,
            found_generation: first,
            id_raw: panel.0,
        }
    );
    assert_eq!(reg.error_count("StaleHandle"), 1);

    // The refusal stored nothing, so the successor's edge records no capture
    // family it never lost.
    reg.note_capture_release(panel, successor)
        .expect("the successor's own edge reports its own session");
    reg.release_generation_scope(panel, successor)
        .expect("successor edge");
    let record = reg
        .unbind_release_record(panel)
        .expect("the successor's record replaced the predecessor's");
    assert_eq!(record.generation, successor);
    assert_eq!(
        record.capture_sessions, 1,
        "one report, one session: the stale report was not counted on top"
    );
}
