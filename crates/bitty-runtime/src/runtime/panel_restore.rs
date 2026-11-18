//! Panel restore on the session-restore path (bitty#3239 row 14).
//!
//! [`plan_restore`] decided how a persisted panel record is re-attached, but
//! until this module it had no production caller: the session restore rebuilt
//! workspaces and panes only, so a panel whose driver is missing, denied, or
//! older could never reach its bounded outcome. The restore now applies panel
//! records the same way it applies panes — inside
//! [`Runtime::apply_session_snapshot`], after validation and before the first
//! painted frame.
//!
//! # What the step does
//!
//! 1. **Plans before any mutation.** Every configured record is pushed into a
//!    [`RestartManifest`] and passed through [`plan_restore`] under the
//!    configured [`PersistencePolicy`]. `StableIdentity` is refused there
//!    ([`ManifestError::StableIdentityUnsupported`]) and the whole restore is
//!    refused before a layout, a focus, or a pane session is touched — the
//!    atomicity contract the session path already keeps. A record set over
//!    [`bitty_ui::MAX_PERSIST_PANELS`] refuses the same way.
//! 2. **Mints a fresh [`PanelId`] per record.** The restored container is
//!    allocated by [`PanelRuntime::create_panel`], so identity comes from the
//!    registry and never from the record. The record's `id` field is advisory
//!    (it is the id the panel had before the restart): it is reported back so a
//!    caller can correlate, and it is never used to re-attach. The record's
//!    [`PanelRestoreKey`] and title travel into the report unchanged.
//! 3. **Binds, or presents the bounded outcome.** A record whose driver is
//!    installed takes a first bind — no predecessor, no ceiling, the whole
//!    resolution granted, because `panel.create` is the authorization. A record
//!    whose driver is missing, denied, or older keeps its container and presents
//!    the Core-owned placeholder for that reason (`unloaded`, `denied`,
//!    `incompatible`). A driver that answers installed while the provider
//!    manifest it registered declares a contract newer than this runtime reads
//!    is treated as older: the bind is refused here, so the container presents
//!    `incompatible` rather than a grant set nobody can interpret.
//! 4. **Bounds the rest.** A record whose workspace names no slot in the
//!    restored session, or that the registry refuses to allocate or bind, is
//!    dropped with a typed reason instead of being retried or silently
//!    orphaned: a container allocated for a record that then failed is disposed
//!    again, so no record leaves half a panel behind.
//!
//! # What this module does not decide
//!
//! The driver answer is an input, not a probe. Which plugin is installed, and
//! which capability a permission denies, are facts the plugin host owns; Core
//! owns only the bounded outcome. The one skew this module reads for itself is
//! the provider contract version: a registered manifest that declares a schema
//! newer than the runtime reads is visible through
//! [`PanelRuntime::provider_manifest`], and a record driven by it never binds.
//! The persisted record still carries no creating owner, no panel type, and no
//! provider contract version (bitty#3239 row 16 stays open, and the record's
//! field set is `RFC-OQ-9`'s pending persistence contract), so this module asks
//! for exactly the facts the record cannot carry and decides nothing else about
//! them.
//!
//! # Reachability of the skew gate
//!
//! Row 13's restore half has a producer chain for both of its convergence
//! inputs:
//!
//! - **The records.** They have a production producer:
//!   [`Runtime::save_session_to_path`](Runtime::save_session_to_path) encodes a
//!   [`RestartManifest`] beside the session file, and
//!   [`Runtime::load_session_from_path`](Runtime::load_session_from_path)
//!   decodes it and configures the records before the restore plans them
//!   (`crate::runtime::panel_persist`, bitty#3243).
//! - **The provider manifest.** The composition root registers one per plugin
//!   it observed driving a container with `panel.provider` granted, at the host
//!   install ([`Runtime::install_shared_scene_host`], see
//!   `Runtime::register_live_panel_providers`), and installs the host-backed
//!   [`HostProviderDriverSource`] for the restore to ask. A record is therefore
//!   matched against a descriptor this process registered for its container
//!   kind, and a descriptor declaring a contract newer than this runtime reads
//!   presents `incompatible` instead of being treated as absent. What no in-tree
//!   declaration source can do yet is *declare* a version above
//!   [`PROVIDER_MANIFEST_VERSION_V1`]:
//!   the descriptors minted here attest the contract the container was created
//!   under, and the plugin package grammar carries no provider-contract field
//!   (`crate::plugin_runtime::manifest_toml`). That declaration is the remaining
//!   residual of bitty#3243.
//! - **The panel host at restore time.** A restore that finds no host no
//!   longer drops its records: the planned step is deferred and committed when
//!   the composition root installs the host
//!   ([`Runtime::install_shared_scene_host`]), which in the shipped order
//!   happens once plugin activation has built it and before the first painted
//!   frame. `bitty-terminal` therefore needs no reordering. The
//!   [`PanelRestoreDrop::NoPanelHost`] reason stays in the report as the total
//!   answer for a plan committed with no host installed, which the deferred
//!   step cannot produce.
//!
//! What the restore does with a record whose driver is too new — retain the
//! container, present `incompatible`, grant nothing — is proven on the restore
//! chain in `tests/panel_restore_path.rs`, and the save→disk→fresh-load→restore
//! chain that reaches it with a real manifest is proven in
//! `tests/panel_restore_producers.rs`.
//!
//! The workspace side resolves through the persisted workspace ordinal. A
//! record's [`PanelRestoreKey`] names the workspace the panel belonged to by
//! the ordinal that workspace occupied in the persisted session (1-based — the
//! `ws:N` index the window presents, and the origin the slot minter starts
//! from), while [`Runtime`] mints a fresh [`WorkspaceId`] for every restored
//! slot: a slot id is per-window attribution and no restored slot ever keeps
//! the id it had. The step therefore resolves the ordinal against the restored
//! slot list — `ordinal - 1`, bounded by the number of slots the snapshot
//! restores — and restores the container into that slot's fresh id. Resolution
//! is never by raw value against the live slots: after a restart the persisted
//! value names no live id, so every record would drop on every window but the
//! one that happened to mint that number. A record whose ordinal falls outside
//! the restored session is dropped, never re-homed onto another workspace.
//!
//! # The outgoing world
//!
//! A restore replaces the whole slot table, so the ids of the world it replaces
//! die with it. The step releases the containers attributed to those outgoing
//! slots before the restored world is installed, so no container is left
//! attributed to a workspace that no longer exists — an attribution no later
//! workspace edge could reach and no later close sweep would ever visit.
//! Containers created without a workspace attribution are window-global and are
//! left alone.
//!
//! # Lifetime of the records
//!
//! The configured records are consumed by the restore that applies them
//! ([`Runtime::set_panel_restore_records`]), so replaying a restore cannot mint
//! a second container for a record that already has one. A restore that finds
//! no panel host yet does not consume them: it defers the step
//! ([`Runtime::panel_restore_pending`]) and the host install commits it. A
//! refused restore leaves them in place, and the per-record outcome stays
//! readable through [`Runtime::panel_restore_report`] afterwards.

use std::collections::{BTreeMap, BTreeSet};

use bitty_plugin_host::PluginId;
use bitty_ui::panel_recovery::PlaceholderReason;
use bitty_ui::{
    ManifestError, PanelRestoreKey, PersistedPanel, PersistencePolicy, RestartManifest,
    plan_restore,
};

use crate::registry::{
    PANEL_PROVIDER_CAPABILITY, PROVIDER_MANIFEST_VERSION_V1, PanelError, PanelHandle, PanelId,
    PanelProviderManifest, PanelRuntime, PanelState, PanelType, WorkspaceId,
};

use super::Runtime;
use super::panel_persist::panel_type_of_persistent_id;

// ---------------------------------------------------------------------------
// Inputs: the record plus the facts it cannot carry
// ---------------------------------------------------------------------------

/// What the plugin host answers about a persisted record's driver at restore.
///
/// The answer is the host's; this module only turns it into the bounded
/// outcome Decision E names. It is not a probe of plugin state and carries no
/// plugin text: an `Installed` answer brings the driver's own resolution of the
/// `panel.*` capabilities, which is what a first bind commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelDriverAnswer {
    /// A driver is installed and available for this container.
    Installed {
        /// The plugin that will drive the container.
        owner: PluginId,
        /// The driver's own resolution of the `panel.*` capabilities. A first
        /// bind commits it whole: there is no predecessor ceiling.
        resolution: BTreeSet<String>,
        /// Capabilities a permission-diff consent admits for this binding only.
        consented: BTreeSet<String>,
    },
    /// Nothing installed serves the record: the container keeps an `unloaded`
    /// placeholder (Decision E rule 4 — unloaded, not deleted).
    Missing,
    /// A driver is installed but a required capability is denied (safe mode
    /// included): the container presents `denied` and is `unavailable`.
    Denied,
    /// A driver is installed but its panel contract cannot be read by this
    /// runtime: nothing is downgraded or partially applied, and the container
    /// presents `incompatible`.
    ///
    /// Two skews answer this way. The driver's own contract may be older than
    /// the persisted record, which the host decides; and the provider manifest
    /// the driver registered may declare a schema newer than this runtime reads,
    /// which the restore observes for itself
    /// ([`PROVIDER_MANIFEST_VERSION_V1`])
    /// and refuses to bind through.
    Older,
}

/// One persisted panel record plus the restore-time facts it cannot carry.
///
/// The composition root configures these on the runtime
/// ([`Runtime::set_panel_restore_records`]) before the session restore runs;
/// the restore path applies them in the order they are given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRestoreInput {
    /// The durable record: advisory [`PanelId`], `(WorkspaceId, PersistentId)`
    /// restore key, title.
    pub record: PersistedPanel,
    /// Container kind the record restores as. The record carries no type
    /// (bitty#3239 row 16 keeps the field set open), so the driver answer
    /// brings the one thing the registry needs to allocate the container.
    pub panel_type: PanelType,
    /// What the plugin host resolves the record's driver to.
    pub driver: PanelDriverAnswer,
}

impl PanelRestoreInput {
    /// Builds one restore input.
    #[must_use]
    pub fn new(record: PersistedPanel, panel_type: PanelType, driver: PanelDriverAnswer) -> Self {
        Self {
            record,
            panel_type,
            driver,
        }
    }
}

/// What a persisted record restores as: the container kind plus what drives it.
///
/// One value, never two fields, for the same reason the registry keeps an
/// owner and its controller generation together: a record restored with one
/// half from the durable document and the other from a different resolution is
/// a mismatch no later edge could detect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRestoreBinding {
    /// Container kind the record restores as.
    pub panel_type: PanelType,
    /// What the plugin host answers about the record's driver.
    pub driver: PanelDriverAnswer,
}

impl PanelRestoreBinding {
    /// Builds one binding.
    #[must_use]
    pub fn new(panel_type: PanelType, driver: PanelDriverAnswer) -> Self {
        Self { panel_type, driver }
    }
}

/// Port that answers what drives a persisted record (bitty#3243).
///
/// The durable record carries identity only — a restore key, an advisory
/// [`PanelId`], and a title — because its field set is `RFC-OQ-9`'s pending
/// persistence contract (bitty#3239 row 16). Which plugin is installed for a
/// record, which container kind it restores as, and which `panel.*` resolution
/// that driver brings are facts the plugin host owns, so the composition root
/// supplies them through this port instead of Core guessing them from a title
/// or a key.
///
/// `panel_type` is Core's own derivation from the durable key (the save side
/// mints `<panel type>-<allocation id>`), so a source that knows better may
/// correct it; the answer carries both halves together, because a record
/// restored with its kind from one resolution and its driver from another is a
/// mismatch no later edge could detect.
///
/// The port is asked once per decoded record whose key names a container kind
/// ([`Runtime::load_session_from_path`](Runtime::load_session_from_path)), and
/// again every time a restore plans those records
/// (`Runtime::plan_panel_restore`) — including the deferred commit a host
/// install runs, which is the first moment the shipped order has a panel host
/// to answer from. A runtime without a port restores every record as
/// [`PanelDriverAnswer::Missing`], which retains the container with the
/// `unloaded` placeholder rather than resurrecting a panel no installed driver
/// can serve.
///
/// [`HostProviderDriverSource`] is the production implementation: the
/// composition root installs it when it installs the panel host, so a record is
/// matched against the provider descriptor this process registered for its
/// container kind.
pub trait PanelRestoreDriverSource: std::fmt::Debug {
    /// What `record`, which the durable key names as `panel_type`, restores as
    /// in this process.
    fn resolve(&self, record: &PersistedPanel, panel_type: PanelType) -> PanelRestoreBinding;
}

/// The production [`PanelRestoreDriverSource`]: answers from the provider
/// descriptors installed on the panel host (bitty#3243).
///
/// A record's container kind is matched against
/// [`PanelRuntime::providers_for_type`] — the descriptors the composition root
/// registered for the plugins this process observed driving containers of that
/// kind (`Runtime::register_live_panel_providers`) — and the answer is:
///
/// - [`PanelDriverAnswer::Missing`] when no provider declares the kind, or when
///   more than one does: an ambiguity is not a fact a restore may resolve by
///   picking one.
/// - [`PanelDriverAnswer::Older`] when the one provider that declares the kind
///   declares a contract newer than this runtime reads
///   ([`PROVIDER_MANIFEST_VERSION_V1`]).
///   The container is retained and presents `incompatible` (bitty#3239 row 13).
/// - [`PanelDriverAnswer::Installed`] with the resolution this process already
///   committed for a live container of the same owner and kind. That is the
///   driver's own `panel.*` set, read back from the registry, so a first bind on
///   the restored container commits exactly what the runtime granted the same
///   owner here: nothing is widened, and nothing is invented from the record.
/// - [`PanelDriverAnswer::Missing`] when the owner is not the only owner driving
///   live containers of the record's kind. The durable record carries no owner
///   (`RFC-OQ-9`, bitty#3239 row 16), so a kind that several plugins drive names
///   no single creator for the record: binding it to whichever of them happens
///   to hold a container would attribute one plugin's panel to another plugin's
///   provider. The record keeps the `unloaded` placeholder instead.
/// - [`PanelDriverAnswer::Missing`] when a readable descriptor exists but this
///   process holds no live container of that owner and kind to read a resolution
///   from. A first bind has no predecessor ceiling and would have to commit a
///   grant set out of thin air; `unloaded` is the honest answer instead.
/// - [`PanelDriverAnswer::Missing`] when the owner holds several live containers
///   of the kind with different committed resolutions. Choosing one of them
///   would make the answer depend on which container the registry enumerated
///   first; a restore may not pick one, so it binds none.
///
/// The answer is a function of the world, never of allocation order: it is
/// assembled from sets of owners and grant sets rather than from the first
/// container that matches, so the same world answers the same thing whatever
/// order its containers were created in. See
/// `tests/panel_provider_registration.rs` for the row that asserts it.
pub struct HostProviderDriverSource {
    host: std::rc::Rc<std::cell::RefCell<PanelRuntime>>,
}

impl HostProviderDriverSource {
    /// Answers from `host`'s provider registry and live containers.
    #[must_use]
    pub fn new(host: std::rc::Rc<std::cell::RefCell<PanelRuntime>>) -> Self {
        Self { host }
    }
}

impl std::fmt::Debug for HostProviderDriverSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostProviderDriverSource").finish()
    }
}

impl PanelRestoreDriverSource for HostProviderDriverSource {
    fn resolve(&self, _record: &PersistedPanel, panel_type: PanelType) -> PanelRestoreBinding {
        let missing = || PanelRestoreBinding::new(panel_type, PanelDriverAnswer::Missing);
        // A host borrowed elsewhere cannot be read: the record keeps the
        // `unloaded` answer rather than waiting on a borrow a restore may not
        // take (the commit itself drops the record as `PanelHostBusy`).
        let Ok(host) = self.host.try_borrow() else {
            return missing();
        };
        let owners = host.providers_for_type(panel_type);
        let [owner] = owners.as_slice() else {
            // None declares the kind, or several do: no single driver to name.
            return missing();
        };
        let Some(manifest) = host.provider_manifest(owner) else {
            return missing();
        };
        if manifest.requires_newer_runtime() {
            // The declaration is admitted (that is what makes the skew
            // observable) and nothing may bind through it.
            return PanelRestoreBinding::new(panel_type, PanelDriverAnswer::Older);
        }
        let Some(resolution) = live_resolution(&host, owner, panel_type) else {
            return missing();
        };
        let Ok(owner) = PluginId::new(owner) else {
            return missing();
        };
        PanelRestoreBinding::new(
            panel_type,
            PanelDriverAnswer::Installed {
                owner,
                resolution,
                consented: BTreeSet::new(),
            },
        )
    }
}

/// The `panel.*` grant set `host` already committed for the live containers of
/// `owner` and `kind`, if the world names exactly one such resolution.
///
/// Read back from the registry ([`PanelRuntime::panel_granted_capabilities`]),
/// never assembled from a catalog: the set is exactly what a bind of the same
/// owner and kind earned in this process.
///
/// Owner **and** kind are one predicate, and the answer is assembled from sets
/// rather than from the first matching container, for two reasons that are both
/// about not naming an owner the world does not name:
///
/// * The durable record carries no owner (`RFC-OQ-9`, bitty#3239 row 16), so the
///   only facts that can attribute a container to a plugin are the live ones.
///   Taking the first container of the kind and comparing owners *afterwards*
///   makes both the attribution and the answer a function of allocation order:
///   when two plugins drive one [`PanelType`] the record was either bound to the
///   plugin that created it, bound to the other plugin, or `unloaded`, depending
///   only on which container the registry enumerated first. `PanelType` is a
///   closed set of generic kinds shared by every plugin, so several drivers per
///   kind is an ordinary world, not an edge.
/// * A resolution is authority: handing a record a set that belongs to a
///   different owner than the one that created its container widens that
///   plugin's reach on a panel it never had. So when the kind is driven by more
///   than one owner, or by one owner that committed two different resolutions,
///   there is no single answer and this returns `None` (the caller answers
///   [`PanelDriverAnswer::Missing`]) instead of picking one.
///
/// An unbound container (no owner) is not a candidate: it holds no controller
/// generation to read grants from and attributes nothing.
fn live_resolution(
    host: &PanelRuntime,
    owner: &str,
    panel_type: PanelType,
) -> Option<BTreeSet<String>> {
    let mut owners: BTreeSet<String> = BTreeSet::new();
    let mut resolutions: BTreeSet<BTreeSet<String>> = BTreeSet::new();
    for record in host.live_panel_records() {
        if record.panel_type != panel_type {
            continue;
        }
        let Some(live_owner) = record.owner else {
            continue;
        };
        owners.insert(live_owner.as_str().to_string());
        if live_owner.as_str() != owner {
            continue;
        }
        if let Some(grants) = host.panel_granted_capabilities(record.id, record.grant_generation) {
            resolutions.insert(grants.clone());
        }
    }
    // Exactly one owner drives the kind, and it is the one whose descriptor this
    // answer is for: otherwise the record has no single creator to restore.
    if owners.len() != 1 || !owners.contains(owner) {
        return None;
    }
    // Exactly one resolution among that owner's containers: several distinct
    // ones would make "the" resolution an enumeration-order fact.
    let mut resolutions = resolutions.into_iter();
    let resolution = resolutions.next()?;
    if resolutions.next().is_some() {
        return None;
    }
    Some(resolution)
}

// ---------------------------------------------------------------------------
// Report: what one restore did with each record
// ---------------------------------------------------------------------------

/// Why a persisted panel did not come back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelRestoreDrop {
    /// No panel host is installed in this runtime, so there is no registry to
    /// restore a container into.
    ///
    /// A restore no longer reaches this: it defers its panel step until a host
    /// is installed ([`Runtime::panel_restore_pending`]). The reason stays in
    /// the report as the total answer a plan committed with no host installed
    /// would carry, and it is what a caller sees if it commits one directly.
    NoPanelHost,
    /// The installed panel host was already borrowed for the whole step, so
    /// nothing could be allocated. The record is not retried inside the restore;
    /// the next restore that finds the host free applies it.
    PanelHostBusy,
    /// The record's workspace names no slot in the restored session. The
    /// record is not re-homed: an unresolvable workspace is a persistence
    /// question, not something a restore may guess.
    WorkspaceGone,
    /// The registry refused the allocation or the bind (capacity, generation
    /// exhaustion, a refused fence). A container allocated for the record is
    /// disposed again before the drop is reported.
    RegistryRefused(PanelError),
}

/// What one persisted record restored to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelRestoreOutcome {
    /// A first bind committed on a fresh container.
    Bound {
        /// Freshly minted container id.
        panel: PanelId,
        /// Restored workspace the container belongs to.
        workspace: WorkspaceId,
        /// The committed grant set: the driver's whole resolution.
        grants: BTreeSet<String>,
    },
    /// The container is retained and presents the Core-owned placeholder.
    Placeholder {
        /// Freshly minted container id.
        panel: PanelId,
        /// Restored workspace the container belongs to.
        workspace: WorkspaceId,
        /// The reason the container presents.
        reason: PlaceholderReason,
    },
    /// Nothing came back for this record.
    Dropped {
        /// Why the record was bounded out.
        reason: PanelRestoreDrop,
    },
}

/// One record's restore result: the durable identity carried across plus what
/// the restore did with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredPanel {
    /// The id the record carried. Advisory: never used to re-attach, reported
    /// only so a caller can correlate a record with the container it became.
    pub persisted_id: PanelId,
    /// The `(WorkspaceId, PersistentId)` restore key, carried verbatim.
    pub key: PanelRestoreKey,
    /// Display title, carried verbatim.
    pub title: String,
    /// What the restore did with the record.
    pub outcome: PanelRestoreOutcome,
}

/// Outcome of the panel step of one session restore (content-free: ids, keys,
/// titles, and the bounded outcome, never panel content).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelRestoreReport {
    panels: Vec<RestoredPanel>,
}

impl PanelRestoreReport {
    /// One entry per configured record, in configuration order.
    #[must_use]
    pub fn panels(&self) -> &[RestoredPanel] {
        &self.panels
    }

    /// Number of records the step considered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
    }

    /// Whether the step considered no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    /// Records that took a first bind.
    #[must_use]
    pub fn bound(&self) -> usize {
        self.count(|outcome| matches!(outcome, PanelRestoreOutcome::Bound { .. }))
    }

    fn count(&self, predicate: impl Fn(&PanelRestoreOutcome) -> bool) -> usize {
        self.panels
            .iter()
            .filter(|panel| predicate(&panel.outcome))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Planning (pure, pre-mutation)
// ---------------------------------------------------------------------------

/// One record after planning: the record itself plus the slot it lands in.
///
/// Reachable at `pub(in crate::runtime)` so the session-restore path
/// ([`Runtime::apply_session_snapshot`]) can carry a plan from the pre-mutation
/// half of the step to the commit; its fields stay private to this module, so
/// nothing outside can forge or reorder a plan.
pub(super) struct PlannedPanel {
    record: PersistedPanel,
    panel_type: PanelType,
    driver: PanelDriverAnswer,
    /// Index into the restored slot list, or `None` when the record's
    /// workspace names no slot being restored.
    slot: Option<usize>,
}

impl Runtime {
    /// Selects the [`PersistencePolicy`] panel records are planned under at the
    /// next session restore (bitty#3239 row 14).
    ///
    /// The default is [`PersistencePolicy::Ephemeral`], the fail-safe outcome
    /// the persistence module documents while `RFC-OQ-9` is open: restart
    /// re-mints identity rather than resurrecting a persisted one. Selecting
    /// [`PersistencePolicy::StableIdentity`] makes every subsequent restore
    /// refuse, because a [`PanelId`] is not a restore key.
    pub fn set_panel_persistence_policy(&mut self, policy: PersistencePolicy) {
        self.panel_restore_policy = policy;
    }

    /// Configures the panel records the next session restore applies.
    ///
    /// Records are applied in the order given and are consumed by the restore
    /// that applies them: after a successful restore this list is empty again,
    /// so replaying a restore cannot mint a second container for a record that
    /// already has one. A refused restore leaves them in place, so a caller can
    /// correct the policy and retry. A restore that finds no panel host yet
    /// leaves them in place too and defers the step
    /// ([`Self::panel_restore_pending`]): the records are the composition root's
    /// and this module never drops them for a host that arrives later.
    ///
    /// They are not persisted by this call and the restore never writes them:
    /// the durable store stays the session backend's, and the record encoding is
    /// untouched by this slice.
    pub fn set_panel_restore_records(&mut self, records: Vec<PanelRestoreInput>) {
        self.panel_restore_inputs = records;
        self.panel_restore_pending = false;
    }

    /// Installs (or clears) the port that answers what drives a persisted record.
    ///
    /// Installed by the composition root when it installs the panel host
    /// ([`Self::install_shared_scene_host`]), so the records a durable manifest
    /// decodes into carry the driver facts the record itself cannot (see
    /// [`PanelRestoreDriverSource`]). A composition root that installs its own
    /// port keeps it: the host install only supplies the default
    /// [`HostProviderDriverSource`] when none is installed.
    pub fn set_panel_restore_driver_source(
        &mut self,
        source: Option<std::rc::Rc<dyn PanelRestoreDriverSource>>,
    ) {
        self.panel_restore_driver_source = source;
    }

    /// The installed driver source, if any.
    #[must_use]
    pub fn panel_restore_driver_source(&self) -> Option<std::rc::Rc<dyn PanelRestoreDriverSource>> {
        self.panel_restore_driver_source.clone()
    }

    /// Registers one provider descriptor per plugin this process observed
    /// driving a live container of the installed panel host (bitty#3243,
    /// provider-registration half).
    ///
    /// Called by [`Self::install_shared_scene_host`], which is the first moment
    /// this process holds both a panel host and the containers plugins created
    /// in it, and which runs before the deferred panel step commits — so a
    /// restored record is matched against a descriptor that exists.
    ///
    /// Every fact comes from the host: the owner a container is bound to
    /// ([`PanelRuntime::live_panel_records`]), the kinds that owner drives, and
    /// whether the container's committed grant set holds
    /// [`PANEL_PROVIDER_CAPABILITY`] ([`PanelRuntime::panel_granted_capabilities`]).
    /// A plugin that drives a container without the provider capability is not
    /// declaring itself a provider, so it mints no descriptor; an owner that
    /// already has one keeps it, because this is an observation, not a reload.
    ///
    /// The descriptor declares [`PROVIDER_MANIFEST_VERSION_V1`]: this runtime
    /// can attest the contract it read a container under, and nothing stronger.
    /// A descriptor declaring a newer contract is admitted by the registry and
    /// still presents `incompatible` at the bind edge; no in-tree declaration
    /// source can mint one yet (bitty#3243 residual, the package grammar carries
    /// no provider-contract field).
    ///
    /// Returns how many descriptors were registered. A host borrowed elsewhere is
    /// not observed (`0`): a save or a restore holding the host is not a world
    /// this call may describe.
    pub(super) fn register_live_panel_providers(&mut self) -> usize {
        let Some(host) = self.scene_host.clone() else {
            return 0;
        };
        // Grouped by owner: a descriptor declares the whole observed kind set at
        // once, because a second registration for the same owner is refused.
        let mut declared: BTreeMap<String, Vec<PanelType>> = BTreeMap::new();
        {
            let Ok(host) = host.try_borrow() else {
                return 0;
            };
            for record in host.live_panel_records() {
                let Some(owner) = record.owner else {
                    continue;
                };
                // Terminal containers are the PTY path's, never a provider's.
                if record.panel_type == PanelType::Terminal {
                    continue;
                }
                let declares = host
                    .panel_granted_capabilities(record.id, record.grant_generation)
                    .is_some_and(|grants| grants.contains(PANEL_PROVIDER_CAPABILITY));
                if !declares {
                    continue;
                }
                let kinds = declared.entry(owner.as_str().to_string()).or_default();
                if !kinds.contains(&record.panel_type) {
                    kinds.push(record.panel_type);
                }
            }
        }
        let mut registered = 0;
        for (owner, kinds) in declared {
            let Ok(manifest) =
                PanelProviderManifest::parse(&owner, kinds, PROVIDER_MANIFEST_VERSION_V1)
            else {
                // An owner id the provider grammar refuses declares nothing;
                // its containers still restore, as `unloaded`.
                continue;
            };
            let outcome =
                self.with_scene_host_mut(|host| host.register_panel_provider(manifest, true));
            if matches!(outcome, Some(Ok(_))) {
                registered += 1;
            }
        }
        registered
    }

    /// Whether a restore's panel records are waiting for a panel host.
    ///
    /// `true` between a session restore that found no panel host and the host
    /// install that commits the deferred step; the records stay readable through
    /// [`Self::panel_restore_records`] meanwhile, and
    /// [`Self::panel_restore_report`] stays empty because nothing was decided
    /// yet.
    #[must_use]
    pub const fn panel_restore_pending(&self) -> bool {
        self.panel_restore_pending
    }

    /// The panel records the next session restore will apply (empty once a
    /// restore consumed them).
    #[must_use]
    pub fn panel_restore_records(&self) -> &[PanelRestoreInput] {
        &self.panel_restore_inputs
    }

    /// What the last successful session restore did with the configured panel
    /// records (empty before any restore, and reset by a refused one).
    #[must_use]
    pub fn panel_restore_report(&self) -> &PanelRestoreReport {
        &self.panel_restore_report
    }

    /// Plans the panel step of a restore that is about to rebuild
    /// `restored_slots` workspaces.
    ///
    /// Pure: nothing here touches the layout, the focus, a pane session, or the
    /// panel registry. It is the pre-mutation half of the step, so a refusal
    /// ([`ManifestError`]) leaves the runtime exactly as it was.
    ///
    /// The driver bindings are resolved here rather than taken from the answers
    /// recorded at load: in the shipped order the load runs before the panel
    /// host exists, so the facts a binding needs arrive later, and a deferred
    /// commit planned against the load-time answers would restore every record
    /// as if no driver were installed.
    pub(super) fn plan_panel_restore(
        &self,
        restored_slots: usize,
    ) -> Result<Vec<PlannedPanel>, ManifestError> {
        let inputs = self.resolved_panel_restore_inputs();
        let mut manifest = RestartManifest::new();
        for input in &inputs {
            // Over-cap record sets fail closed here, before any mutation, with
            // the same typed error a decoded document would produce.
            manifest.push(input.record.clone())?;
        }
        // The restore entry point: `StableIdentity` is refused here.
        let records = plan_restore(&manifest, self.panel_restore_policy)?;
        let mut planned = Vec::with_capacity(records.len());
        for (index, record) in records.into_iter().enumerate() {
            let Some(input) = inputs.get(index) else {
                continue;
            };
            planned.push(PlannedPanel {
                slot: restored_slot_of(record.key.workspace.get(), restored_slots),
                record,
                panel_type: input.panel_type,
                driver: input.driver.clone(),
            });
        }
        Ok(planned)
    }

    /// The configured records with their driver bindings resolved through the
    /// installed port.
    ///
    /// The load side resolves the bindings of the records it decodes, and this
    /// resolves them again because the port's answer is a function of the state
    /// at the moment it is asked: a deferred commit runs after the composition
    /// root installed the panel host, which is when the provider descriptors
    /// exist. A record whose durable key names no container kind is left
    /// exactly as configured — the load side already refuses to configure one,
    /// and a source may not invent a kind for it.
    fn resolved_panel_restore_inputs(&self) -> Vec<PanelRestoreInput> {
        let Some(source) = self.panel_restore_driver_source.clone() else {
            return self.panel_restore_inputs.clone();
        };
        self.panel_restore_inputs
            .iter()
            .map(
                |input| match panel_type_of_persistent_id(&input.record.key.persistent) {
                    Some(panel_type) => {
                        let binding = source.resolve(&input.record, panel_type);
                        PanelRestoreInput::new(
                            input.record.clone(),
                            binding.panel_type,
                            binding.driver,
                        )
                    }
                    None => input.clone(),
                },
            )
            .collect()
    }

    /// Releases every container attributed to a workspace of the outgoing world.
    ///
    /// A restore replaces the whole slot table, and a slot [`WorkspaceId`] is a
    /// per-window attribution the minter never reuses, so the ids of the world
    /// being replaced die with it. A container still attributed to one of them
    /// would be unreachable: no restored workspace resolves to that id, and
    /// every later sweep (the one a workspace close runs) walks live slots
    /// only, so nothing would ever dispose it and the registry would keep
    /// counting it. The step therefore runs the same per-workspace disposal a
    /// workspace close runs, for every outgoing slot, before the restored world
    /// is installed.
    ///
    /// Containers created without a workspace attribution are window-global and
    /// are left alone: no dead slot id is involved, so nothing orphans them. A
    /// host the composition root is holding across the restore cannot be
    /// borrowed, and the release is skipped for that restore — the same bounded
    /// borrow the panel step itself takes (`PanelHostBusy`), never a panic
    /// inside Core.
    pub(super) fn release_panels_of_outgoing_world(&mut self) {
        // Collected first: the sweep needs the host mutably, and the ids must
        // be the outgoing ones (the caller replaces the table right after).
        let outgoing: Vec<WorkspaceId> = self.workspaces.iter().map(|slot| slot.id).collect();
        self.with_scene_host_mut(|host| {
            for workspace in outgoing {
                // The sweep is fail-fast and reports what it could not release.
                // A refusal there is a registry invariant violation (the sweep
                // disposes each container at its own allocation epoch, so a
                // stale handle cannot arise), and the world it belonged to is
                // already gone from the layout domain, so there is no second
                // path to retry it on.
                let _ = host.dispose_workspace_panels(workspace);
            }
        });
    }

    /// Applies the planned panel step, or defers it when no panel host exists.
    ///
    /// The shipped order restores the session before the composition root has
    /// built the panel host (`bitty-terminal`) and before plugin activation has
    /// registered anything, so a restore that ran the step immediately would
    /// bound out every record with [`PanelRestoreDrop::NoPanelHost`]. The step
    /// is therefore deferred rather than dropped: the records stay configured,
    /// nothing is planned against, and
    /// [`Self::apply_deferred_panel_restore`] commits them when the host is
    /// installed (before the first painted frame).
    ///
    /// Deferring is bounded: a second restore before the host arrives replaces
    /// the configured records (the load side re-configures them), and the
    /// deferred plan is re-planned against the world installed at commit time,
    /// so a record can never land in a slot table a later restore replaced.
    pub(super) fn apply_panel_restore(&mut self, planned: Vec<PlannedPanel>) {
        if self.scene_host.is_none() {
            self.panel_restore_report = PanelRestoreReport::default();
            self.panel_restore_pending = !self.panel_restore_inputs.is_empty();
            return;
        }
        self.panel_restore_report = self.commit_panel_restore(planned);
        self.panel_restore_inputs.clear();
        self.panel_restore_pending = false;
    }

    /// Commits a panel step that was deferred for want of a panel host.
    ///
    /// Called by [`Self::install_shared_scene_host`], so the host exists by the
    /// time it runs. The records are planned again against the world installed
    /// now (never against the plan a restore would have made before the world
    /// it names was replaced); a refused plan
    /// ([`ManifestError::StableIdentityUnsupported`] after a policy change, or
    /// an over-cap record set) leaves an empty report and consumes the records,
    /// because there is nothing a second attempt could do differently.
    pub(super) fn apply_deferred_panel_restore(&mut self) {
        if !self.panel_restore_pending {
            return;
        }
        self.panel_restore_pending = false;
        let planned = match self.plan_panel_restore(self.workspaces.len()) {
            Ok(planned) => planned,
            Err(_) => {
                self.panel_restore_report = PanelRestoreReport::default();
                self.panel_restore_inputs.clear();
                return;
            }
        };
        self.panel_restore_report = self.commit_panel_restore(planned);
        self.panel_restore_inputs.clear();
    }

    /// Commits the planned panel step after the restored world is installed.
    ///
    /// Bounded and total: every planned record yields exactly one report entry,
    /// and a record that cannot be restored is dropped with a typed reason
    /// (after disposing the container it may have allocated) rather than
    /// aborting a restore whose layout, focus, and sessions are already in
    /// place.
    pub(super) fn commit_panel_restore(
        &mut self,
        planned: Vec<PlannedPanel>,
    ) -> PanelRestoreReport {
        let host = self.scene_host.clone();
        let mut panels = Vec::with_capacity(planned.len());
        for item in planned {
            let outcome = self.restore_one_panel(host.as_ref(), &item);
            panels.push(RestoredPanel {
                persisted_id: item.record.id,
                key: item.record.key,
                title: item.record.title,
                outcome,
            });
        }
        PanelRestoreReport { panels }
    }

    /// Restores one planned record: allocate a fresh container, then bind it or
    /// present its placeholder.
    fn restore_one_panel(
        &self,
        host: Option<&std::rc::Rc<std::cell::RefCell<PanelRuntime>>>,
        item: &PlannedPanel,
    ) -> PanelRestoreOutcome {
        let Some(slot) = item.slot else {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::WorkspaceGone,
            };
        };
        let Some(slot) = self.workspaces.get(slot) else {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::WorkspaceGone,
            };
        };
        let Some(host) = host else {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::NoPanelHost,
            };
        };
        let workspace = slot.id;
        // `try_borrow_mut`: a caller holding the shared handle across the
        // restore is a bounded drop, never a panic inside Core.
        let mut host = match host.try_borrow_mut() {
            Ok(host) => host,
            Err(_) => {
                return PanelRestoreOutcome::Dropped {
                    reason: PanelRestoreDrop::PanelHostBusy,
                };
            }
        };
        // The container is allocated here, so its `PanelId` is fresh: the
        // record's advisory id was never a restore key and is never passed in.
        let handle = match host.create_panel(item.panel_type, Some(workspace)) {
            Ok(handle) => handle,
            Err(error) => {
                return PanelRestoreOutcome::Dropped {
                    reason: PanelRestoreDrop::RegistryRefused(error),
                };
            }
        };
        let outcome = match &item.driver {
            PanelDriverAnswer::Installed {
                owner,
                resolution,
                consented,
            } => {
                // The one skew this step can observe for itself: the driver is
                // installed, but the provider manifest it registered declares a
                // contract newer than this runtime reads. Binding would commit
                // a resolution this runtime cannot interpret, so the container
                // keeps its placeholder instead (bitty#3239 row 13).
                if provider_contract_outruns_runtime(&host, owner) {
                    present_restored_placeholder(&mut host, handle, workspace, "incompatible")
                } else {
                    bind_restored_panel(&mut host, handle, workspace, owner, resolution, consented)
                }
            }
            PanelDriverAnswer::Missing => {
                present_restored_placeholder(&mut host, handle, workspace, "unloaded")
            }
            PanelDriverAnswer::Denied => {
                present_restored_placeholder(&mut host, handle, workspace, "denied")
            }
            PanelDriverAnswer::Older => {
                present_restored_placeholder(&mut host, handle, workspace, "incompatible")
            }
        };
        if matches!(outcome, PanelRestoreOutcome::Dropped { .. }) {
            // Never leave a container the record did not earn: the join between
            // allocation and presentation is where a half-restored panel could
            // otherwise survive as an orphan.
            let _ = host.dispose_panel(handle.id, handle.generation);
        }
        outcome
    }
}

/// Resolves a record's persisted workspace ordinal to the restored slot it
/// lands in.
///
/// A record's key carries [`bitty_ui::WorkspaceId`] — the targeting newtype —
/// while the runtime names slots with [`WorkspaceId`]. The two are distinct
/// types over the same number, but the number is not the same fact in each: the
/// record's value is the *persisted workspace ordinal* (1-based, the `ws:N`
/// index the window presents and the origin the slot minter starts from), and a
/// live slot id is per-window attribution that the minter mints fresh for every
/// restored slot. Matching the raw value against the live slots — what this
/// step used to do — resolves at most the one workspace the starting window
/// happened to mint, so every record from a second or later workspace of the
/// persisted session dropped as `WorkspaceGone`; in the production shape (one
/// workspace at startup) that is every record but the first workspace's.
///
/// Resolution is therefore by the ordinal, against the slots the snapshot is
/// about to restore: `ordinal - 1` for the zero-based slot list, bounded by
/// `restored_slots`. `None` when the ordinal falls outside that range (a
/// zeroed key, or one past the persisted session's last workspace); the record
/// is then dropped rather than re-homed onto another workspace, because a
/// restore may not guess which workspace a record meant.
fn restored_slot_of(persisted_workspace: u64, restored_slots: usize) -> Option<usize> {
    let slot = usize::try_from(persisted_workspace).ok()?.checked_sub(1)?;
    (slot < restored_slots).then_some(slot)
}

/// The `incompatible` placeholder for a driver whose registered provider
/// contract is newer than this runtime reads.
///
/// The answer comes from the provider registry the panel host owns, never from
/// the restore input: a driver that answers
/// [`PanelDriverAnswer::Installed`] for a container whose provider manifest
/// declares a schema above
/// [`PROVIDER_MANIFEST_VERSION_V1`]
/// cannot be bound — this runtime cannot read what the provider would commit —
/// so the container keeps a `incompatible` placeholder rather than a grant set
/// nobody can interpret (bitty#3239 row 13, Decision E rule 3).
///
/// An owner that registered no provider manifest is not a skew: most panels are
/// driven by a plugin that never declares `panel.provider`, and their first
/// bind is unchanged.
///
/// Evaluated for every restored record whose driver answers `Installed`. The
/// descriptors this process mints at the host install
/// (`Runtime::register_live_panel_providers`) declare the version this runtime
/// reads, so they answer `false`; a descriptor declaring a newer contract is
/// admitted by the registry, and this predicate is the gate that keeps the
/// container away from a bind this runtime could not interpret.
fn provider_contract_outruns_runtime(host: &PanelRuntime, owner: &PluginId) -> bool {
    host.provider_manifest(owner.as_str())
        .is_some_and(PanelProviderManifest::requires_newer_runtime)
}

/// First-binds a freshly allocated container: no predecessor, no ceiling, and
/// the driver's whole resolution as the grant set.
fn bind_restored_panel(
    host: &mut PanelRuntime,
    handle: PanelHandle,
    workspace: WorkspaceId,
    owner: &PluginId,
    resolution: &BTreeSet<String>,
    consented: &BTreeSet<String>,
) -> PanelRestoreOutcome {
    // A controller generation is a different value from the allocation epoch by
    // construction (`rebind_panel` refuses the epoch), so a first bind takes the
    // epoch's successor.
    let controller = match handle.generation.next() {
        Ok(generation) => generation,
        Err(_) => {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::RegistryRefused(PanelError::GenerationExhausted {
                    current: handle.generation,
                }),
            };
        }
    };
    match host.rebind_panel(handle.id, controller, owner, resolution, consented) {
        Ok(committed) => PanelRestoreOutcome::Bound {
            panel: handle.id,
            workspace,
            grants: committed.grants().clone(),
        },
        Err(error) => PanelRestoreOutcome::Dropped {
            reason: PanelRestoreDrop::RegistryRefused(error),
        },
    }
}

/// Presents the Decision E placeholder a driver answer names on a freshly
/// allocated, never-bound container.
fn present_restored_placeholder(
    host: &mut PanelRuntime,
    handle: PanelHandle,
    workspace: WorkspaceId,
    token: &'static str,
) -> PanelRestoreOutcome {
    match host.present_placeholder(handle.id, handle.generation, token) {
        Ok(presented) => match presented.placeholder {
            Some(placeholder) => PanelRestoreOutcome::Placeholder {
                panel: handle.id,
                workspace,
                reason: placeholder.reason(),
            },
            // Unreachable: every token used here selects `Placeholder` or
            // `Unavailable`, and both write the record's placeholder. A `Drop`
            // token (a user close) is never one of them.
            None => PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::RegistryRefused(PanelError::InvalidState {
                    current: PanelState::Created,
                    expected: "a placeholder outcome",
                }),
            },
        },
        Err(error) => PanelRestoreOutcome::Dropped {
            reason: PanelRestoreDrop::RegistryRefused(error),
        },
    }
}
