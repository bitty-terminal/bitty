//! U-8 provider-batch integration coverage (UX-33/UX-29, CTX-0675).
//!
//! Candidate behavior: [`CommandBlockProvider`](bitty_ui::CommandBlockProvider)
//! exposes the semantic terminal as the single Core-tier target source,
//! [`ProviderMediator`](bitty_ui::ProviderMediator) gates Plugin/Derived
//! registration behind the capability grant and collects in tier priority,
//! and [`TargetSnapshot`](bitty_ui::TargetSnapshot) freezes one cold-path
//! collection per hint-session entry with fail-closed epoch revalidation.
//! Cross-module checks the unit tests inside `beacon_provider` do not cover
//! alone: provenance ordering across tiers, capability gating end to end,
//! epoch staleness against the live [`TargetRegistry`](bitty_ui::TargetRegistry),
//! retirement fail-closed through snapshots, and atomic oversized-collection
//! rejection.

#![forbid(unsafe_code)]

use bitty_ui::{
    CommandBlockId, CommandBlockProvider, DerivedProvider, LinkId, ProviderError, ProviderMediator,
    ProviderTarget, ProviderTier, TargetProvider, TargetRegistry, TargetSnapshot,
};

/// A third-party Plugin-tier fixture: offers a fixed set of targets.
struct FixturePlugin {
    name: String,
    targets: Vec<ProviderTarget>,
}

impl FixturePlugin {
    fn links(name: &str, raw: &[u64]) -> Self {
        Self {
            name: name.to_string(),
            targets: raw
                .iter()
                .map(|id| ProviderTarget::Link(LinkId::new(*id)))
                .collect(),
        }
    }
}

impl TargetProvider for FixturePlugin {
    fn tier(&self) -> ProviderTier {
        ProviderTier::Plugin
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn collect(&self) -> Vec<ProviderTarget> {
        self.targets.clone()
    }
}

fn blocks(raw: &[u64]) -> Vec<CommandBlockId> {
    raw.iter().map(|id| CommandBlockId::new(*id)).collect()
}

fn mediator_with_plugin(block_ids: &[u64], plugin: FixturePlugin) -> ProviderMediator {
    let mut mediator = ProviderMediator::with_core(blocks(block_ids));
    mediator
        .register(Box::new(plugin), true)
        .expect("plugin registers with grant");
    mediator
}

// ---------------------------------------------------------------------------
// Session entry collects the terminal's blocks (UX-33 + UX-29)
// ---------------------------------------------------------------------------

/// One session entry freezes the terminal's command blocks with Core
/// provenance, and every entry resolves live against the registry.
#[test]
fn session_entry_collects_terminal_blocks_with_core_provenance() {
    let mediator = ProviderMediator::with_core(blocks(&[11, 12, 13]));
    let mut registry = TargetRegistry::new();
    let snapshot = mediator
        .collect(&mut registry)
        .expect("session entry collects");
    assert_eq!(snapshot.len(), 3);
    assert!(!snapshot.is_empty());
    for entry in snapshot.entries() {
        assert_eq!(entry.tier(), ProviderTier::Core);
        assert_eq!(entry.provider(), "terminal");
    }
    let live = snapshot
        .resolve_all(&registry)
        .expect("fresh snapshot resolves");
    assert_eq!(live, snapshot.targets());
}

/// The snapshot carries no polling handle: entries stay frozen while the
/// registry retires underneath, and revalidation fails closed instead.
#[test]
fn frozen_snapshot_never_refreshes_itself() {
    let mediator = ProviderMediator::with_core(blocks(&[21]));
    let mut registry = TargetRegistry::new();
    let snapshot = mediator.collect(&mut registry).expect("collects");
    let before = snapshot.targets();
    assert!(registry.retire_block(CommandBlockId::new(21)));
    // The snapshot still holds the same frozen handle — nothing re-polled.
    assert_eq!(snapshot.targets(), before);
    assert!(matches!(
        snapshot.resolve_all(&registry),
        Err(ProviderError::StaleSnapshot(_))
    ));
}

// ---------------------------------------------------------------------------
// Tier priority and provenance (UX-33)
// ---------------------------------------------------------------------------

/// Collection order is tier priority (Core, Plugin, Derived) regardless of
/// registration order, and every entry records its provenance.
#[test]
fn snapshot_orders_core_before_plugin_before_derived() {
    // Register out of priority order on purpose: derived first, plugin next.
    // Ids stay disjoint across providers: re-offering one id inside a single
    // collect opens a newer generation for it, staling the earlier entry by
    // construction (see the epoch test below).
    let mut mediator = ProviderMediator::new();
    let parent = ProviderMediator::with_core(blocks(&[33, 34]));
    let mut registry = TargetRegistry::new();
    let parent_snapshot = parent.collect(&mut registry).expect("parent collects");
    let derived =
        DerivedProvider::new("session.blocks", parent_snapshot.offered()).expect("lens builds");
    mediator
        .register(Box::new(derived), true)
        .expect("derived registers");
    mediator
        .register(
            Box::new(FixturePlugin::links("acme.links:extra", &[7])),
            true,
        )
        .expect("plugin registers");
    mediator
        .register(Box::new(CommandBlockProvider::new(blocks(&[31, 32]))), true)
        .expect("core registers");

    let snapshot = mediator.collect(&mut registry).expect("collects");
    let tiers: Vec<ProviderTier> = snapshot.entries().iter().map(|e| e.tier()).collect();
    assert_eq!(
        tiers,
        vec![
            ProviderTier::Core,
            ProviderTier::Core,
            ProviderTier::Plugin,
            ProviderTier::Derived,
            ProviderTier::Derived,
        ]
    );
    assert_eq!(snapshot.targets_of_tier(ProviderTier::Core).len(), 2);
    assert_eq!(snapshot.targets_of_tier(ProviderTier::Plugin).len(), 1);
    assert_eq!(snapshot.targets_of_tier(ProviderTier::Derived).len(), 2);
    snapshot
        .resolve_all(&registry)
        .expect("fresh snapshot resolves");
}

/// Derived lenses compose from parent snapshots and re-expose only the
/// selected kinds.
#[test]
fn derived_lens_reexposes_filtered_parent_offers() {
    let parent = ProviderMediator::with_core(blocks(&[41]));
    let mut registry = TargetRegistry::new();
    let parent_snapshot = parent.collect(&mut registry).expect("parent collects");
    let only_blocks: Vec<ProviderTarget> = parent_snapshot
        .offered()
        .into_iter()
        .filter(|offer| offer.kind() == bitty_ui::TargetKind::CommandBlock)
        .collect();
    assert_eq!(only_blocks.len(), 1);
    let lens = DerivedProvider::new("session.blocks-only", only_blocks).expect("lens builds");
    assert_eq!(lens.len(), 1);
    assert!(!lens.is_empty());

    let mut mediator = ProviderMediator::new();
    mediator
        .register(Box::new(lens), true)
        .expect("derived registers");
    let snapshot = mediator.collect(&mut registry).expect("lens collects");
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot.entries()[0].tier(), ProviderTier::Derived);
    snapshot
        .resolve_all(&registry)
        .expect("lens snapshot resolves");
}

// ---------------------------------------------------------------------------
// Capability gate and naming (UX-33 mediator-only security)
// ---------------------------------------------------------------------------

/// Plugin registration without the capability grant is denied; the same
/// provider registers with the grant and resolves by name.
#[test]
fn plugin_registration_requires_capability_grant() {
    let mut mediator = ProviderMediator::with_core(blocks(&[51]));
    let denied = mediator.register(
        Box::new(FixturePlugin::links("acme.links:extra", &[9])),
        false,
    );
    assert_eq!(
        denied,
        Err(ProviderError::CapabilityDenied {
            name: "acme.links:extra".to_string()
        })
    );
    assert!(mediator.resolve("acme.links:extra").is_none());
    mediator
        .register(
            Box::new(FixturePlugin::links("acme.links:extra", &[9])),
            true,
        )
        .expect("grant registers");
    assert_eq!(
        mediator.names(),
        vec!["terminal".to_string(), "acme.links:extra".to_string()]
    );
    assert!(mediator.require("acme.links:extra").is_ok());
    assert!(matches!(
        mediator.require("acme.links:ghost"),
        Err(ProviderError::UnknownProvider { .. })
    ));
}

/// The reserved `terminal` name stays with the Core tier, and duplicate
/// names are rejected even with the grant.
#[test]
fn reserved_and_duplicate_names_fail_closed() {
    let mut mediator = ProviderMediator::with_core(blocks(&[61]));
    assert_eq!(
        mediator.register(Box::new(FixturePlugin::links("terminal", &[1])), true),
        Err(ProviderError::ReservedName {
            name: "terminal".to_string()
        })
    );
    mediator
        .register(
            Box::new(FixturePlugin::links("acme.links:extra", &[1])),
            true,
        )
        .expect("first registers");
    assert_eq!(
        mediator.register(
            Box::new(FixturePlugin::links("acme.links:extra", &[2])),
            true
        ),
        Err(ProviderError::DuplicateName {
            name: "acme.links:extra".to_string()
        })
    );
    assert_eq!(mediator.len(), 2);
}

// ---------------------------------------------------------------------------
// Epochs and fail-closed revalidation (UX-29)
// ---------------------------------------------------------------------------

/// A new session entry opens a new epoch: the prior snapshot's handles go
/// stale while the new snapshot resolves.
#[test]
fn new_session_epoch_stales_prior_snapshot() {
    let mediator = mediator_with_plugin(&[71, 72], FixturePlugin::links("acme.links:extra", &[5]));
    let mut registry = TargetRegistry::new();
    let first = mediator
        .collect(&mut registry)
        .expect("first session collects");
    first
        .resolve_all(&registry)
        .expect("first resolves while current");
    let second = mediator
        .collect(&mut registry)
        .expect("second session collects");
    assert!(matches!(
        first.resolve_all(&registry),
        Err(ProviderError::StaleSnapshot(_))
    ));
    let live = second.resolve_all(&registry).expect("second resolves");
    assert_eq!(live, second.targets());
    assert_eq!(
        TargetSnapshot::default()
            .resolve_all(&registry)
            .expect("empty"),
        Vec::new()
    );
}

/// An oversized collection fails atomically: no partial epoch reaches the
/// registry.
#[test]
fn oversized_collection_commits_no_partial_epoch() {
    let wide: Vec<u64> = (1..=bitty_ui::MAX_SNAPSHOT_TARGETS as u64 + 1).collect();
    let mediator = ProviderMediator::with_core(blocks(&wide));
    let mut registry = TargetRegistry::new();
    let err = mediator.collect(&mut registry).expect_err("cap must hold");
    assert_eq!(
        err,
        ProviderError::TooManyTargets {
            max: bitty_ui::MAX_SNAPSHOT_TARGETS,
            current: bitty_ui::MAX_SNAPSHOT_TARGETS + 1,
        }
    );
    assert!(registry.is_empty());
}
