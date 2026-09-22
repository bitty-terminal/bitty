//! Beacon target providers (UX-33 + UX-29, U-8 Beacon family).
//!
//! [`CommandBlockProvider`] exposes the semantic terminal as one provider:
//! the terminal surface enumerates its scrollback command blocks and nothing
//! else. Providers are ranked into tiers ([`ProviderTier`]: `Core`,
//! `Plugin`, `Derived`); the [`ProviderMediator`] collects them in tier
//! priority (Core first, then Plugin, then Derived) with registration order
//! inside each tier.
//!
//! Collection is a cold path (UX-29): the mediator gathers every offer once
//! per hint-session entry into an immutable [`TargetSnapshot`]. The
//! compositor never polls providers per frame; a new session entry collects
//! a new snapshot. Each collection is a new epoch: inserting an id issues a
//! fresh [`TargetRegistry`](crate::beacon_target::TargetRegistry)
//! generation, so handles captured by an older snapshot resolve
//! [`ProviderError::StaleSnapshot`] — fail closed, never refreshed in place.
//! There is deliberately no `refresh`/`poll` API on [`TargetSnapshot`].
//!
//! # Mediator-only security
//!
//! Providers receive no registry handle, mutable or shared. [`TargetProvider`]
//! is a pure enumeration (`collect` is a pure function of provider state:
//! same inputs yield the same offers); only the mediator inserts offers into
//! the live registry. Plugin and Derived providers register behind an
//! explicit capability grant ([`TARGET_PROVIDER_CAPABILITY`]); the Core tier
//! is not self-claimable — external Core registrations need the grant too,
//! and the compiled-in terminal provider enters only through
//! [`ProviderMediator::with_core`]. The reserved name
//! ([`CORE_TERMINAL_PROVIDER_NAME`]) stays with the Core tier.
//!
//! Derived providers compose rather than source: [`DerivedProvider`] builds
//! only from [`TargetSnapshot::offered`] data (typically a filtered lens
//! over a parent snapshot, e.g. command-blocks-only), never from raw
//! terminal state.
//!
//! All types are bounded ([`MAX_TARGET_PROVIDERS`] providers,
//! [`MAX_SNAPSHOT_TARGETS`] entries per snapshot), `#![forbid(unsafe_code)]`,
//! deterministic, and headless: no I/O, wall-clock, randomness, or render
//! coupling.

#![forbid(unsafe_code)]

use crate::beacon_target::{
    CommandBlockId, LinkId, TargetError, TargetRef, TargetRegistry, WorkspaceId,
};
use crate::panel::PanelId;
use crate::uitree::UiNodeId;

/// Capability that gates target-provider registration.
///
/// Mirrors the `layout.provider` discipline in [`crate::provider`]: the
/// mediator takes an explicit grant flag instead of depending on the plugin
/// host. The Core terminal provider enters through
/// [`ProviderMediator::with_core`], which carries the grant internally.
pub const TARGET_PROVIDER_CAPABILITY: &str = "beacon.target-provider";

/// Reserved provider name for the Core semantic-terminal provider.
pub const CORE_TERMINAL_PROVIDER_NAME: &str = "terminal";

/// Maximum provider name length, mirroring the layout-provider bound.
pub const MAX_TARGET_PROVIDER_NAME_LEN: usize = 32;

/// Maximum providers per mediator.
pub const MAX_TARGET_PROVIDERS: usize = 64;

/// Maximum targets per cold-path snapshot. Aligns with
/// [`crate::beacon_target::MAX_TARGETS_PER_KIND`] and
/// [`crate::beacon_dispatch::MAX_BEACON_BINDINGS`]: a snapshot never feeds
/// more than the downstream tables hold.
pub const MAX_SNAPSHOT_TARGETS: usize = 1024;

// ---------------------------------------------------------------------------
// Tiers and targets
// ---------------------------------------------------------------------------

/// Trust tier of a target provider.
///
/// - `Core` ships with the binary (the semantic terminal). Collected first.
/// - `Plugin` is third-party and capability-gated. Collected second.
/// - `Derived` composes a parent snapshot (see [`DerivedProvider`]).
///   Collected last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProviderTier {
    /// Built-in, host-owned source.
    Core,
    /// Third-party, capability-gated source.
    Plugin,
    /// Snapshot-composed lens, capability-gated.
    Derived,
}

impl ProviderTier {
    /// Collection priority: lower collects first.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Core => 0,
            Self::Plugin => 1,
            Self::Derived => 2,
        }
    }
}

impl std::fmt::Display for ProviderTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core => f.write_str("core"),
            Self::Plugin => f.write_str("plugin"),
            Self::Derived => f.write_str("derived"),
        }
    }
}

/// Addressable surface offered by a provider. Mirrors the five
/// [`TargetRef`] variants as plain ids: the mediator issues generations on
/// insert, so offers carry no generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderTarget {
    /// A hosted panel.
    Panel(PanelId),
    /// A workspace.
    Workspace(WorkspaceId),
    /// A scrollback command block.
    CommandBlock(CommandBlockId),
    /// A chrome/UI-tree node.
    UiNode(UiNodeId),
    /// A hyperlink.
    Link(LinkId),
}

impl ProviderTarget {
    /// Surface kind of this offer.
    #[must_use]
    pub const fn kind(self) -> TargetKind {
        match self {
            Self::Panel(_) => TargetKind::Panel,
            Self::Workspace(_) => TargetKind::Workspace,
            Self::CommandBlock(_) => TargetKind::CommandBlock,
            Self::UiNode(_) => TargetKind::UiNode,
            Self::Link(_) => TargetKind::Link,
        }
    }

    /// Rebuilds the offer side of a resolved [`TargetRef`] (drops the
    /// generation). Used by [`TargetSnapshot::offered`] for derivation.
    fn from_ref(target: &TargetRef) -> Self {
        match target {
            TargetRef::Panel(r) => Self::Panel(r.id),
            TargetRef::Workspace(r) => Self::Workspace(r.id),
            TargetRef::CommandBlock(r) => Self::CommandBlock(r.id),
            TargetRef::UiNode(r) => Self::UiNode(r.id),
            TargetRef::Link(r) => Self::Link(r.id),
        }
    }
}

impl std::fmt::Display for ProviderTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panel(id) => write!(f, "Panel({id})"),
            Self::Workspace(id) => write!(f, "Workspace({id})"),
            Self::CommandBlock(id) => write!(f, "CommandBlock({id})"),
            Self::UiNode(id) => write!(f, "UiNode({id})"),
            Self::Link(id) => write!(f, "Link({id})"),
        }
    }
}

/// Surface kind for filtering snapshot offers into derived lenses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// Hosted panels.
    Panel,
    /// Workspaces.
    Workspace,
    /// Scrollback command blocks.
    CommandBlock,
    /// Chrome/UI-tree nodes.
    UiNode,
    /// Hyperlinks.
    Link,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed failure for the provider path. Every rejection is attributed and
/// fail-closed: the caller drops the hint session, never falls back to a
/// default target or a silently truncated snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// Provider name is empty, too long, or outside the name grammar.
    InvalidName {
        /// Rejected name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The reserved Core name was claimed by a non-Core tier.
    ReservedName {
        /// Rejected name.
        name: String,
    },
    /// No provider is registered under this name.
    UnknownProvider {
        /// Requested name.
        name: String,
    },
    /// Registration without the `beacon.target-provider` capability grant.
    CapabilityDenied {
        /// Rejected name.
        name: String,
    },
    /// A provider under this name is already registered.
    DuplicateName {
        /// Rejected name.
        name: String,
    },
    /// The mediator is at [`MAX_TARGET_PROVIDERS`].
    TooManyProviders {
        /// Enforced cap.
        max: usize,
        /// Live providers.
        current: usize,
    },
    /// Collection exceeds [`MAX_SNAPSHOT_TARGETS`] (or the live registry
    /// kind cap on insert). Nothing is partially committed.
    TooManyTargets {
        /// Enforced cap.
        max: usize,
        /// Offered or live entries.
        current: usize,
    },
    /// A snapshot entry no longer resolves against the live registry
    /// (retired, re-registered by a newer epoch, or never registered).
    StaleSnapshot(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName { name, reason } => {
                write!(f, "invalid target provider name '{name}': {reason}")
            }
            Self::ReservedName { name } => {
                write!(
                    f,
                    "target provider name '{name}' is reserved for the Core terminal provider"
                )
            }
            Self::UnknownProvider { name } => {
                write!(f, "unknown target provider '{name}'")
            }
            Self::CapabilityDenied { name } => {
                write!(
                    f,
                    "registering target provider '{name}' requires the '{TARGET_PROVIDER_CAPABILITY}' capability"
                )
            }
            Self::DuplicateName { name } => {
                write!(f, "target provider '{name}' is already registered")
            }
            Self::TooManyProviders { max, current } => {
                write!(f, "too many target providers: max {max}, current {current}")
            }
            Self::TooManyTargets { max, current } => {
                write!(f, "too many snapshot targets: max {max}, current {current}")
            }
            Self::StaleSnapshot(detail) => write!(f, "stale snapshot target: {detail}"),
        }
    }
}

impl std::error::Error for ProviderError {}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Validates a provider name for `tier`.
///
/// Grammar (`1..=32` bytes, ASCII lowercase alphanumerics plus
/// `.`, `-`, `_`, `:`): spelling validation accepts any well-formed name;
/// registration membership is enforced by [`ProviderMediator`]. The reserved
/// [`CORE_TERMINAL_PROVIDER_NAME`] is accepted only for the Core tier.
///
/// # Errors
///
/// [`ProviderError::InvalidName`] or [`ProviderError::ReservedName`].
pub fn validate_provider_name(name: &str, tier: ProviderTier) -> Result<(), ProviderError> {
    if name.is_empty() {
        return Err(ProviderError::InvalidName {
            name: name.to_string(),
            reason: "provider name must not be empty",
        });
    }
    if name.len() > MAX_TARGET_PROVIDER_NAME_LEN {
        return Err(ProviderError::InvalidName {
            name: name.to_string(),
            reason: "provider name exceeds 32 bytes",
        });
    }
    let well_formed = name.bytes().all(|b| {
        b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || b == b'.'
            || b == b'-'
            || b == b'_'
            || b == b':'
    });
    if !well_formed {
        return Err(ProviderError::InvalidName {
            name: name.to_string(),
            reason: "provider name must be [a-z0-9._:-]*",
        });
    }
    if name == CORE_TERMINAL_PROVIDER_NAME && tier != ProviderTier::Core {
        return Err(ProviderError::ReservedName {
            name: name.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

/// Pure enumeration of beacon targets for one trust tier.
///
/// `collect` is a pure function of provider state: no registry handle, no
/// I/O, no wall-clock. Same state yields the same offers; nondeterminism is
/// a conformance violation. Only the [`ProviderMediator`] inserts offers
/// into the live registry.
pub trait TargetProvider: Send + Sync {
    /// Trust tier of this provider.
    fn tier(&self) -> ProviderTier;
    /// Validated registration name.
    fn name(&self) -> &str;
    /// Offers the current target ids (no generations; the mediator issues
    /// them on insert).
    fn collect(&self) -> Vec<ProviderTarget>;
}

// ---------------------------------------------------------------------------
// Core terminal provider (UX-33)
// ---------------------------------------------------------------------------

/// The semantic terminal as one provider (Core tier, reserved name
/// `terminal`).
///
/// Owns the terminal surface's scrollback command blocks and offers exactly
/// those — no panels, nodes, or links. Constructed with the current block
/// set; a changed scrollback set means a new provider (or a new session
/// entry), never mutation behind the mediator's back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandBlockProvider {
    blocks: Vec<CommandBlockId>,
}

impl CommandBlockProvider {
    /// Creates the Core terminal provider over `blocks`.
    #[must_use]
    pub fn new(blocks: Vec<CommandBlockId>) -> Self {
        Self { blocks }
    }

    /// Owned command blocks, in offer order.
    #[must_use]
    pub fn blocks(&self) -> &[CommandBlockId] {
        &self.blocks
    }

    /// Number of owned blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// True when no block is owned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl TargetProvider for CommandBlockProvider {
    fn tier(&self) -> ProviderTier {
        ProviderTier::Core
    }

    fn name(&self) -> &str {
        CORE_TERMINAL_PROVIDER_NAME
    }

    fn collect(&self) -> Vec<ProviderTarget> {
        self.blocks
            .iter()
            .map(|id| ProviderTarget::CommandBlock(*id))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Derived provider
// ---------------------------------------------------------------------------

/// A snapshot-composed lens (Derived tier).
///
/// Built only from [`TargetSnapshot::offered`] data — typically a
/// kind-filtered subset of a parent snapshot (e.g. command-blocks-only) —
/// never from raw terminal state. Collecting a derived provider re-inserts
/// its ids through the mediator, opening a new epoch whose generations
/// stale the parent snapshot's handles by construction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DerivedProvider {
    name: String,
    targets: Vec<ProviderTarget>,
}

impl DerivedProvider {
    /// Builds a derived lens over `targets` (usually filtered
    /// [`TargetSnapshot::offered`] output).
    ///
    /// # Errors
    ///
    /// [`ProviderError::InvalidName`] or [`ProviderError::ReservedName`]
    /// when `name` is not a valid Derived-tier name.
    pub fn new(name: &str, targets: Vec<ProviderTarget>) -> Result<Self, ProviderError> {
        validate_provider_name(name, ProviderTier::Derived)?;
        Ok(Self {
            name: name.to_string(),
            targets,
        })
    }

    /// Number of composed targets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// True when the lens is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

impl TargetProvider for DerivedProvider {
    fn tier(&self) -> ProviderTier {
        ProviderTier::Derived
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn collect(&self) -> Vec<ProviderTarget> {
        self.targets.clone()
    }
}

// ---------------------------------------------------------------------------
// Snapshot (UX-29 cold path)
// ---------------------------------------------------------------------------

/// One frozen snapshot entry: the resolved handle plus its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotEntry {
    target: TargetRef,
    tier: ProviderTier,
    provider: String,
}

impl SnapshotEntry {
    /// Resolved generation handle.
    #[must_use]
    pub const fn target(&self) -> TargetRef {
        self.target
    }

    /// Tier of the contributing provider.
    #[must_use]
    pub const fn tier(&self) -> ProviderTier {
        self.tier
    }

    /// Name of the contributing provider.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }
}

/// Immutable cold-path collection of one hint-session entry.
///
/// Built only by [`ProviderMediator::collect`]; entries keep collection
/// order (tier priority, then registration order, then provider offer
/// order). There is deliberately no `refresh`/`poll`: the compositor reuses
/// this frozen vector for the whole session, and the next session entry
/// collects a new snapshot. [`TargetSnapshot::resolve_all`] revalidates
/// every entry against the live registry and fails closed on the first
/// stale handle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetSnapshot {
    entries: Vec<SnapshotEntry>,
}

impl TargetSnapshot {
    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no target was collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Frozen entries in collection order.
    #[must_use]
    pub fn entries(&self) -> &[SnapshotEntry] {
        &self.entries
    }

    /// Resolved handles in collection order.
    #[must_use]
    pub fn targets(&self) -> Vec<TargetRef> {
        self.entries.iter().map(|entry| entry.target).collect()
    }

    /// Handles contributed by one tier, in collection order.
    #[must_use]
    pub fn targets_of_tier(&self, tier: ProviderTier) -> Vec<TargetRef> {
        self.entries
            .iter()
            .filter(|entry| entry.tier == tier)
            .map(|entry| entry.target)
            .collect()
    }

    /// Offer-side view of every entry (generations dropped), for building
    /// [`DerivedProvider`] lenses.
    #[must_use]
    pub fn offered(&self) -> Vec<ProviderTarget> {
        self.entries
            .iter()
            .map(|entry| ProviderTarget::from_ref(&entry.target))
            .collect()
    }

    /// Revalidates every entry against the live registry, failing closed on
    /// the first stale handle. Reads only: neither the snapshot nor the
    /// registry is mutated.
    ///
    /// # Errors
    ///
    /// [`ProviderError::StaleSnapshot`] when any entry retired,
    /// re-registered under a newer epoch, or never registered.
    pub fn resolve_all(&self, registry: &TargetRegistry) -> Result<Vec<TargetRef>, ProviderError> {
        let mut live = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            match registry.resolve(&entry.target) {
                Ok(target) => live.push(target),
                Err(err) => return Err(ProviderError::StaleSnapshot(err.to_string())),
            }
        }
        Ok(live)
    }
}

// ---------------------------------------------------------------------------
// Mediator
// ---------------------------------------------------------------------------

/// Name-keyed provider set and sole registry writer.
///
/// Registration carries an explicit capability grant
/// (`has_target_provider_capability`) so `bitty-ui` never depends on the
/// plugin host; [`ProviderMediator::with_core`] carries the grant internally
/// for the compiled-in terminal provider. Collection stages every offer
/// first (so an oversized collection fails before any insert), then inserts
/// in tier priority and freezes the resulting [`TargetSnapshot`].
pub struct ProviderMediator {
    providers: Vec<Box<dyn TargetProvider>>,
}

impl ProviderMediator {
    /// Creates an empty mediator (no providers).
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Creates a mediator with the Core terminal provider pre-registered
    /// over `blocks`.
    #[must_use]
    pub fn with_core(blocks: Vec<CommandBlockId>) -> Self {
        let mut mediator = Self::new();
        // Infallible by construction: the reserved name is valid for Core.
        mediator
            .register(Box::new(CommandBlockProvider::new(blocks)), true)
            .expect("core terminal provider registers");
        mediator
    }

    /// Registers a provider behind the capability grant. Core trust is not
    /// self-claimable: every external registration needs the grant,
    /// including Core-tier ones.
    ///
    /// # Errors
    ///
    /// [`ProviderError::InvalidName`], [`ProviderError::ReservedName`],
    /// [`ProviderError::CapabilityDenied`],
    /// [`ProviderError::DuplicateName`], or
    /// [`ProviderError::TooManyProviders`].
    pub fn register(
        &mut self,
        provider: Box<dyn TargetProvider>,
        has_target_provider_capability: bool,
    ) -> Result<(), ProviderError> {
        let tier = provider.tier();
        let name = provider.name().to_string();
        validate_provider_name(&name, tier)?;
        if !has_target_provider_capability {
            return Err(ProviderError::CapabilityDenied { name });
        }
        if self.providers.iter().any(|p| p.name() == name) {
            return Err(ProviderError::DuplicateName { name });
        }
        if self.providers.len() >= MAX_TARGET_PROVIDERS {
            return Err(ProviderError::TooManyProviders {
                max: MAX_TARGET_PROVIDERS,
                current: self.providers.len(),
            });
        }
        self.providers.push(provider);
        Ok(())
    }

    /// Looks up a provider by name (spelling unchecked).
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&dyn TargetProvider> {
        self.providers
            .iter()
            .find(|p| p.name() == name)
            .map(AsRef::as_ref)
    }

    /// Looks up a provider by name.
    ///
    /// # Errors
    ///
    /// [`ProviderError::UnknownProvider`] for unregistered names.
    pub fn require(&self, name: &str) -> Result<&dyn TargetProvider, ProviderError> {
        self.resolve(name)
            .ok_or_else(|| ProviderError::UnknownProvider {
                name: name.to_string(),
            })
    }

    /// Registered names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|p| p.name().to_string())
            .collect()
    }

    /// Number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no provider is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Collects one cold-path snapshot for a hint-session entry.
    ///
    /// Stages every offer first so an oversized collection fails before any
    /// registry insert (no partial epoch). Inserts in tier priority (Core,
    /// Plugin, Derived) with registration order inside each tier, freezing
    /// the issued handles and their provenance into the snapshot. Replays
    /// bump generations: handles from earlier snapshots go stale by
    /// construction.
    ///
    /// # Errors
    ///
    /// [`ProviderError::TooManyTargets`] when the staged offers exceed
    /// [`MAX_SNAPSHOT_TARGETS`] or a registry kind table is full.
    pub fn collect(&self, registry: &mut TargetRegistry) -> Result<TargetSnapshot, ProviderError> {
        let tiers = [
            ProviderTier::Core,
            ProviderTier::Plugin,
            ProviderTier::Derived,
        ];
        let mut staged: Vec<(ProviderTier, String, ProviderTarget)> = Vec::new();
        for tier in tiers {
            for provider in self.providers.iter().filter(|p| p.tier() == tier) {
                let name = provider.name().to_string();
                for offer in provider.collect() {
                    staged.push((tier, name.clone(), offer));
                }
            }
        }
        if staged.len() > MAX_SNAPSHOT_TARGETS {
            return Err(ProviderError::TooManyTargets {
                max: MAX_SNAPSHOT_TARGETS,
                current: staged.len(),
            });
        }
        let mut entries = Vec::with_capacity(staged.len());
        for (tier, name, offer) in staged {
            let target = insert_offer(registry, &offer)?;
            entries.push(SnapshotEntry {
                target,
                tier,
                provider: name,
            });
        }
        Ok(TargetSnapshot { entries })
    }
}

impl Default for ProviderMediator {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ProviderMediator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderMediator")
            .field("providers", &self.names())
            .finish()
    }
}

/// Inserts one offer into the live registry, issuing its generation.
fn insert_offer(
    registry: &mut TargetRegistry,
    offer: &ProviderTarget,
) -> Result<TargetRef, ProviderError> {
    let inserted = match offer {
        ProviderTarget::Panel(id) => registry.insert_panel(*id).map(TargetRef::Panel),
        ProviderTarget::Workspace(id) => registry.insert_workspace(*id).map(TargetRef::Workspace),
        ProviderTarget::CommandBlock(id) => registry.insert_block(*id).map(TargetRef::CommandBlock),
        ProviderTarget::UiNode(id) => registry.insert_node(*id).map(TargetRef::UiNode),
        ProviderTarget::Link(id) => registry.insert_link(*id).map(TargetRef::Link),
    };
    inserted.map_err(|err| match err {
        TargetError::TooManyTargets { max, current } => {
            ProviderError::TooManyTargets { max, current }
        }
        TargetError::UnknownTarget(detail) | TargetError::StaleTarget(detail) => {
            ProviderError::StaleSnapshot(detail)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(n: u64) -> Vec<CommandBlockId> {
        (1..=n).map(CommandBlockId::new).collect()
    }

    #[test]
    fn names_validate_per_tier() {
        assert!(validate_provider_name("terminal", ProviderTier::Core).is_ok());
        assert!(validate_provider_name("acme.blocks:links", ProviderTier::Plugin).is_ok());
        assert!(validate_provider_name("session.blocks", ProviderTier::Derived).is_ok());
        // Reserved name is Core-only.
        assert_eq!(
            validate_provider_name("terminal", ProviderTier::Plugin),
            Err(ProviderError::ReservedName {
                name: "terminal".to_string()
            })
        );
        assert_eq!(
            validate_provider_name("terminal", ProviderTier::Derived),
            Err(ProviderError::ReservedName {
                name: "terminal".to_string()
            })
        );
        // Grammar violations.
        for bad in ["", "Terminal", "has space", "shout!", "a/b"] {
            assert!(
                matches!(
                    validate_provider_name(bad, ProviderTier::Plugin),
                    Err(ProviderError::InvalidName { .. })
                ),
                "'{bad}' must be invalid"
            );
        }
        let long = "a".repeat(MAX_TARGET_PROVIDER_NAME_LEN + 1);
        assert!(validate_provider_name(&long, ProviderTier::Plugin).is_err());
        assert!(
            validate_provider_name(
                &"a".repeat(MAX_TARGET_PROVIDER_NAME_LEN),
                ProviderTier::Plugin
            )
            .is_ok()
        );
    }

    #[test]
    fn core_trust_is_not_self_claimable() {
        let mut mediator = ProviderMediator::new();
        let denied = mediator.register(Box::new(CommandBlockProvider::new(blocks(1))), false);
        assert_eq!(
            denied,
            Err(ProviderError::CapabilityDenied {
                name: "terminal".to_string()
            })
        );
        assert!(mediator.is_empty());
        assert!(
            mediator
                .register(Box::new(CommandBlockProvider::new(blocks(1))), true)
                .is_ok()
        );
        assert_eq!(mediator.names(), vec!["terminal".to_string()]);
    }

    #[test]
    fn terminal_provider_collects_owned_blocks_only() {
        let provider = CommandBlockProvider::new(blocks(2));
        assert_eq!(provider.tier(), ProviderTier::Core);
        assert_eq!(provider.name(), CORE_TERMINAL_PROVIDER_NAME);
        assert_eq!(provider.len(), 2);
        assert!(!provider.is_empty());
        assert_eq!(
            provider.collect(),
            vec![
                ProviderTarget::CommandBlock(CommandBlockId::new(1)),
                ProviderTarget::CommandBlock(CommandBlockId::new(2)),
            ]
        );
        assert!(CommandBlockProvider::new(Vec::new()).is_empty());
    }

    #[test]
    fn tier_ranks_order_core_plugin_derived() {
        assert!(ProviderTier::Core.rank() < ProviderTier::Plugin.rank());
        assert!(ProviderTier::Plugin.rank() < ProviderTier::Derived.rank());
        assert!(ProviderTier::Core < ProviderTier::Plugin);
    }

    #[test]
    fn empty_mediator_collects_empty_snapshot() {
        let mediator = ProviderMediator::new();
        assert!(mediator.is_empty());
        let mut registry = TargetRegistry::new();
        let snapshot = mediator.collect(&mut registry).expect("empty collects");
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.len(), 0);
        assert_eq!(
            snapshot.resolve_all(&registry).expect("empty resolves"),
            Vec::new()
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn oversized_collection_fails_before_any_insert() {
        let wide: Vec<CommandBlockId> = (1..=MAX_SNAPSHOT_TARGETS as u64 + 1)
            .map(CommandBlockId::new)
            .collect();
        let mediator = ProviderMediator::with_core(wide);
        let mut registry = TargetRegistry::new();
        let err = mediator.collect(&mut registry).expect_err("cap must hold");
        assert_eq!(
            err,
            ProviderError::TooManyTargets {
                max: MAX_SNAPSHOT_TARGETS,
                current: MAX_SNAPSHOT_TARGETS + 1,
            }
        );
        // No partial epoch was committed.
        assert!(registry.is_empty());
    }
}
