#![forbid(unsafe_code)]
//! `PanelRuntime` host abstraction (CW-20, issue #998).
//!
//! Contract source: the accepted Panel Runtime RFC names `PanelRuntime`
//! as the Core-owned host that creates, mounts, suspends, resumes, and
//! disposes panels, validates `(PanelId, Generation)`, and mediates bus
//! traffic. The owner ruling of 2026-09-23 accepts `RFC-OQ-3` Option A
//! (Panel as typed `View` content, `ViewContent::Panel(PanelId)`) and
//! `OQ-058` `SMO-1..SMO-4` including delivery semantics, with terms from
//! `ADR-0013` (`bitty-docs` #367). This module records the spelling
//! decision:
//!
//! - [`PanelRegistry`] stays the identity/attachment/budget store: id
//!   allocation, `(PanelId, Generation)` validation, the `PanelId -> ViewId`
//!   map, focus MRU, overlays, command registry, capability grants, and
//!   the three-level bus budgets.
//! - [`PanelEventBus`] stays the queue store: per-subscription FIFOs,
//!   coalescing, drop policy, and aggregate eviction.
//! - [`PanelRuntime`] is the Core-owned host facade over both. Every
//!   cross-component call enters through the runtime, which validates the
//!   handle generation first (`StaleHandle` before any grid or PTY access)
//!   and gates bus mediation on the v1 capability ledger (`panel.provider`,
//!   deny-by-default; see `event_bus_v1`).
//! - Routable delivery (`OQ-058`, issue #1001) rides the
//!   [`RoutableLedger`](super::routable::RoutableLedger): the host checks
//!   sender/recipient registration against [`PanelProviderRegistry`] so a
//!   missing or stale route fails closed (`SMO-3`), then admits the
//!   validated [`AgentMessage`](super::routable::AgentMessage) envelope
//!   (identity, attribution, deadline, priority, dedup, cancel, expiry).
//!
//! The runtime holds no PTY descriptor, GPU object, or OS window handle;
//! those remain with `bitty-pty`, `bitty-render`, and `bitty-platform`.
//! Every failure is fail-closed and typed: the previous valid state is
//! retained and a bounded diagnostic counter advances in the registry.
//!
//! Live adoption (CTX-0700, extended CTX-0721): the host mirrors every
//! mount/unmount/dispose into the [`bitty_ui::placement::Placement`]
//! binding map, gates provider-backed creation through
//! [`PanelProviderRegistry`], mints v1 taxonomy topics via
//! [`core_topic`](super::event_bus_v1::core_topic), resolves cross-window
//! scope via [`CrossWindowRoute`](super::event_bus_v1::CrossWindowRoute),
//! and routes [`AgentMessage`](super::routable::AgentMessage) envelopes
//! through the routable ledger, so the accepted contracts have live
//! consumers outside unit tests.

use bitty_ui::ViewId;
use bitty_ui::placement::{Placement, PlacementError};

use std::collections::HashMap;

use crate::execution::{
    LeaseError, LeaseEvent, LeaseHolder, LeaseState, PanelLease, validate_description,
    validate_title,
};

use super::event_bus_v1::{
    BUS_PUBLISH_CAPABILITY, BUS_SUBSCRIBE_CAPABILITY, BusTopicFamily, CrossWindowRoute,
    RoutingError, RoutingScope, core_topic,
};
use super::panel::PanelType;
use super::panel::{
    BoundedPayload, BusEvent, EventTopic, PanelError, PanelHandle, PanelRegistry,
    PanelRegistryConfig, PanelState, ViewContent,
};
use super::provider::{PanelProviderManifest, PanelProviderRegistry, ProviderContractError};
use super::routable::{AgentMessage, RoutableError, RoutableLedger};
use super::{Generation, WorkspaceId};

// ---------------------------------------------------------------------------
// Host facade
// ---------------------------------------------------------------------------

/// Core-owned panel host: the single entry point for panel lifecycle and
/// host-mediated bus traffic within one window/process.
///
/// Spelling decision (CW-20): the host is a facade, not a second store.
/// Identity, attachment, budgets, and capability grants live in the wrapped
/// [`PanelRegistry`]; queues live in its [`PanelEventBus`](super::panel::PanelEventBus).
/// The host additionally mirrors attachment into the live
/// [`Placement`] map and provider declarations into the live
/// [`PanelProviderRegistry`]; both mirrors are updated only after the
/// registry commits, and any mirror failure rolls the registry back, so the
/// three views never desynchronize.
#[derive(Debug)]
pub struct PanelRuntime {
    registry: PanelRegistry,
    placement: Placement,
    providers: PanelProviderRegistry,
    routable: RoutableLedger,
    /// Panel lease table (RUN-21, #1052): one pure [`PanelLease`] kernel
    /// per live panel plus its chrome-facing title/description. Issued at
    /// [`PanelRuntime::create_panel`] (fresh `Idle`), moved only through
    /// the acquire/release/handoff entries below, and cleared at
    /// [`PanelRuntime::dispose_panel`]. The kernel owns the transition;
    /// this table owns the per-panel binding. Lease vocabulary stays a UX
    /// metaphor: the lease records who may drive a panel, never what is
    /// true, and holder tags are opaque [`LeaseHolder`] values the caller
    /// assigns.
    leases: HashMap<super::panel::PanelId, PanelLeaseEntry>,
}

/// Per-panel lease binding: the transition kernel plus the validated
/// human/agent orientation text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelLeaseEntry {
    lease: PanelLease,
    title: Option<String>,
    description: Option<String>,
}

impl PanelLeaseEntry {
    fn idle() -> Self {
        Self {
            lease: PanelLease::idle(),
            title: None,
            description: None,
        }
    }
}

/// Maps a refused kernel transition to the typed host denial, keeping the
/// stable kernel audit name (`already_occupied` / `not_occupied` /
/// `not_holder`) at the front of the reason.
fn map_lease_error(panel: super::panel::PanelId, error: LeaseError) -> PanelError {
    PanelError::LeaseDenied {
        panel_id: panel,
        reason: format!("{}: {error}", error.as_str()),
    }
}

impl PanelRuntime {
    /// Creates a host over a validated registry config.
    ///
    /// # Errors
    ///
    /// [`PanelError::ResourceExhausted`] for bad bounds,
    /// [`PanelError::GenerationExhausted`] if the generation reserve holds.
    pub fn new(config: PanelRegistryConfig) -> Result<Self, PanelError> {
        Ok(Self {
            registry: PanelRegistry::new(config)?,
            placement: Placement::new(),
            providers: PanelProviderRegistry::new(),
            routable: RoutableLedger::new(),
            leases: HashMap::new(),
        })
    }

    /// Borrows the underlying identity/attachment store.
    #[must_use]
    pub fn registry(&self) -> &PanelRegistry {
        &self.registry
    }

    /// Creates a panel of `panel_type` without mounting it.
    ///
    /// Issues the panel's lease on success (RUN-21): a fresh `Idle` kernel
    /// binding, so every live panel has exactly one lease from birth.
    ///
    /// # Errors
    ///
    /// [`PanelError::TooManyPanels`], [`PanelError::UnknownPanelType`],
    /// [`PanelError::GenerationExhausted`], or
    /// [`PanelError::RegistryDisposed`].
    pub fn create_panel(
        &mut self,
        panel_type: PanelType,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        let handle = self.registry.create_panel(panel_type, workspace)?;
        self.leases
            .entry(handle.id)
            .or_insert_with(PanelLeaseEntry::idle);
        Ok(handle)
    }

    /// Mounts a created panel onto an empty view (`Created -> Mounted`).
    ///
    /// Mirrors the attachment into the live [`Placement`] map after the
    /// registry commits; a mirror failure rolls the registry mount back so
    /// both views stay in sync.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`], [`PanelError::AlreadyMounted`],
    /// [`PanelError::PanelAlreadyMounted`], or [`PanelError::InvalidState`].
    pub fn mount_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        view: ViewId,
    ) -> Result<(), PanelError> {
        self.registry.mount_panel(id, generation, view)?;
        if let Err(place_err) = self.placement.bind(id, view) {
            let _ = self.registry.unmount_panel(id, generation);
            return Err(map_placement_bind(place_err, id, view, &self.placement));
        }
        Ok(())
    }

    /// Unmounts a panel, returning its former view (suspend, not destroy).
    ///
    /// Clears the live [`Placement`] mirror after the registry commits; a
    /// missing mirror entry is ignored because the registry is the source of
    /// truth.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`], [`PanelError::NotFound`], or
    /// [`PanelError::InvalidState`].
    pub fn unmount_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<ViewId, PanelError> {
        let view = self.registry.unmount_panel(id, generation)?;
        let _ = self.placement.unbind(id);
        Ok(view)
    }

    /// Focuses a mounted panel within `workspace`.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or focus-rule violations.
    pub fn focus_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        workspace: WorkspaceId,
    ) -> Result<(), PanelError> {
        self.registry.focus_panel(id, generation, workspace)
    }

    /// Suspends a panel without destroying its attachment.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::InvalidState`].
    pub fn suspend_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.registry.suspend_panel(id, generation)
    }

    /// Resumes a suspended panel.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::InvalidState`].
    pub fn resume_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.registry.resume_panel(id, generation)
    }

    /// Disposes a panel, retiring its `(PanelId, Generation)` and clearing
    /// its bus queues.
    ///
    /// Clears the live [`Placement`] mirror when the registry dispose
    /// commits.
    ///
    /// Also clears the panel's lease binding (RUN-21): a disposed panel
    /// holds no lease, and a later panel reusing the id is issued a fresh
    /// `Idle` lease at [`PanelRuntime::create_panel`].
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::NotFound`].
    pub fn dispose_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        let result = self.registry.dispose_panel(id, generation);
        if result.is_ok() {
            let _ = self.placement.unbind(id);
            self.leases.remove(&id);
        }
        result
    }

    /// Acquires an idle panel's lease for `holder` (RUN-21, #1052).
    ///
    /// The handle generation is validated first (`StaleHandle` before any
    /// lease access); the transition itself runs in the [`PanelLease`]
    /// kernel and a refusal maps to [`PanelError::LeaseDenied`] with the
    /// stable kernel audit name. A human takeover stays outside the lease:
    /// release back to `Idle`, never a holder value.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`], [`PanelError::NotFound`], or
    /// [`PanelError::LeaseDenied`] (`already_occupied`).
    pub fn acquire_panel_lease(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        holder: LeaseHolder,
    ) -> Result<LeaseEvent, PanelError> {
        self.registry.panel_state(id, generation)?;
        let entry = self.leases.entry(id).or_insert_with(PanelLeaseEntry::idle);
        entry
            .lease
            .acquire(holder)
            .map_err(|error| map_lease_error(id, error))
    }

    /// Releases an occupied panel's lease back to idle (RUN-21, #1052).
    ///
    /// Only the current occupant may release; anything else fails with
    /// [`PanelError::LeaseDenied`] and the lease is unchanged.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`], [`PanelError::NotFound`], or
    /// [`PanelError::LeaseDenied`] (`not_occupied` / `not_holder`).
    pub fn release_panel_lease(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        holder: LeaseHolder,
    ) -> Result<LeaseEvent, PanelError> {
        self.registry.panel_state(id, generation)?;
        let entry = self.leases.entry(id).or_insert_with(PanelLeaseEntry::idle);
        entry
            .lease
            .release(holder)
            .map_err(|error| map_lease_error(id, error))
    }

    /// Moves a panel's lease directly from `from` to `to` with no idle gap
    /// (RUN-21, #1052): a handoff cannot be intercepted mid-release by a
    /// third acquirer.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`], [`PanelError::NotFound`], or
    /// [`PanelError::LeaseDenied`] (`not_occupied` / `not_holder`).
    pub fn handoff_panel_lease(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        from: LeaseHolder,
        to: LeaseHolder,
    ) -> Result<LeaseEvent, PanelError> {
        self.registry.panel_state(id, generation)?;
        let entry = self.leases.entry(id).or_insert_with(PanelLeaseEntry::idle);
        entry
            .lease
            .handoff(from, to)
            .map_err(|error| map_lease_error(id, error))
    }

    /// Reads a panel's current lease state after handle validation
    /// (RUN-21): the check side of the lease entries above. A panel that
    /// was never issued a lease reads `Idle` — occupancy is never assumed.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::NotFound`].
    pub fn panel_lease_state(
        &self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<LeaseState, PanelError> {
        self.registry.panel_state(id, generation)?;
        Ok(self
            .leases
            .get(&id)
            .map_or(LeaseState::Idle, |entry| entry.lease.state()))
    }

    /// Stores a panel's chrome-facing title and description after
    /// handle validation (RUN-21, #1052).
    ///
    /// Titles render in workspace chrome: 1–128 chars, no control
    /// characters ([`validate_title`]). Descriptions are orientation text,
    /// not a data channel: up to 1024 chars, newline excepted
    /// ([`validate_description`]). A refusal stores nothing and maps to
    /// [`PanelError::InvalidDescription`].
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`], [`PanelError::NotFound`], or
    /// [`PanelError::InvalidDescription`].
    pub fn set_panel_description(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        title: &str,
        description: &str,
    ) -> Result<(), PanelError> {
        self.registry.panel_state(id, generation)?;
        if !validate_title(title) {
            return Err(PanelError::InvalidDescription {
                reason: format!(
                    "panel title must be 1-128 chars with no control characters (got {} chars)",
                    title.chars().count()
                ),
            });
        }
        if !validate_description(description) {
            return Err(PanelError::InvalidDescription {
                reason: format!(
                    "panel description must be at most 1024 chars with no control characters other than newline (got {} chars)",
                    description.chars().count()
                ),
            });
        }
        let entry = self.leases.entry(id).or_insert_with(PanelLeaseEntry::idle);
        entry.title = Some(title.to_owned());
        entry.description = Some(description.to_owned());
        Ok(())
    }

    /// Reads a panel's stored title and description after handle
    /// validation (RUN-21): `(title, description)`, each `None` until
    /// [`PanelRuntime::set_panel_description`] stores it.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::NotFound`].
    pub fn panel_description(
        &self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<(Option<String>, Option<String>), PanelError> {
        self.registry.panel_state(id, generation)?;
        Ok(self.leases.get(&id).map_or((None, None), |entry| {
            (entry.title.clone(), entry.description.clone())
        }))
    }

    /// Reads a panel's lifecycle state after handle validation.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::NotFound`].
    pub fn panel_state(
        &self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<PanelState, PanelError> {
        self.registry.panel_state(id, generation)
    }

    /// Declares a bus topic through the accepted topic grammar.
    ///
    /// # Errors
    ///
    /// [`PanelError::UnknownTopic`] or [`PanelError::TooManyTopics`].
    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        self.registry.declare_topic(raw)
    }

    /// Grants a `panel.*` capability to a panel handle (closed host set).
    ///
    /// # Errors
    ///
    /// [`PanelError::CapabilityDenied`], [`PanelError::StaleHandle`], or
    /// [`PanelError::NotFound`].
    pub fn grant_capability(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        self.registry
            .grant_panel_capability(id, generation, capability)
    }

    /// Subscribes a validated panel handle to a declared topic.
    /// Requires the v1 subscribe capability (deny-by-default).
    ///
    /// # Errors
    ///
    /// [`PanelError::CapabilityDenied`], [`PanelError::StaleHandle`],
    /// [`PanelError::UnknownTopic`], or
    /// [`PanelError::TooManySubscriptions`].
    pub fn subscribe(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        topic: &EventTopic,
    ) -> Result<(), PanelError> {
        self.registry
            .require_panel_capability(id, generation, BUS_SUBSCRIBE_CAPABILITY)?;
        self.registry.subscribe(id, generation, topic)
    }

    /// Publishes a bounded payload to a topic's subscribers.
    /// Requires the v1 publish capability (deny-by-default). The publisher
    /// handle is validated first so stale publishers cannot emit.
    ///
    /// # Errors
    ///
    /// [`PanelError::CapabilityDenied`], [`PanelError::StaleHandle`], or
    /// [`PanelError::PayloadTooLarge`].
    pub fn publish(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        self.registry
            .require_panel_capability(id, generation, BUS_PUBLISH_CAPABILITY)?;
        // Handle validation before any bus access: stale handles never emit.
        self.registry.panel_state(id, generation)?;
        self.registry.publish(topic, payload)
    }

    /// Drains up to `max_events`/`max_bytes` for a validated panel handle.
    ///
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::NotFound`]; an
    /// unsubscribed topic drains empty without error.
    pub fn drain_batch(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<BusEvent>, PanelError> {
        self.registry.panel_state(id, generation)?;
        Ok(self.registry.drain_batch(id, topic, max_events, max_bytes))
    }

    /// Total queued bus events host-wide.
    #[must_use]
    pub fn bus_total_events(&self) -> usize {
        self.registry.bus_total_events()
    }

    /// Total bus drops (per-subscription policy plus aggregate eviction).
    #[must_use]
    pub fn bus_total_dropped(&self) -> u64 {
        self.registry.bus_total_dropped()
    }

    /// Number of live panels.
    #[must_use]
    pub fn panel_count(&self) -> usize {
        self.registry.panel_count()
    }

    /// Returns the view the live [`Placement`] mirror binds `panel` to.
    ///
    /// Mirrors the registry attachment after every host mount/unmount/dispose,
    /// so panel focus and hit-testing can read placement without touching the
    /// identity store.
    #[must_use]
    pub fn placement_view_of(&self, panel: super::panel::PanelId) -> Option<ViewId> {
        self.placement.view_of(panel)
    }

    /// Returns the panel the live [`Placement`] mirror hosts on `view`.
    #[must_use]
    pub fn placement_panel_of(&self, view: ViewId) -> Option<super::panel::PanelId> {
        self.placement.panel_of(view)
    }

    /// Number of live placement bindings.
    #[must_use]
    pub fn placement_len(&self) -> usize {
        self.placement.len()
    }

    /// Registers a validated provider manifest behind the `panel.provider`
    /// capability gate (CW-23 surface, live through the host).
    ///
    /// Duplicate owners are rejected, not shadowed; each registration mints a
    /// fresh generation so reload swaps provider content atomically.
    ///
    /// # Errors
    ///
    /// [`ProviderContractError::CapabilityDenied`] or
    /// [`ProviderContractError::DuplicateOwner`]; state is unchanged.
    pub fn register_panel_provider(
        &mut self,
        manifest: PanelProviderManifest,
        capability_granted: bool,
    ) -> Result<u64, ProviderContractError> {
        self.providers.register(manifest, capability_granted)
    }

    /// Removes a provider; unload drops its contributed content without
    /// tearing down the host.
    ///
    /// # Errors
    ///
    /// [`ProviderContractError::UnknownOwner`]; state is unchanged.
    pub fn unregister_panel_provider(
        &mut self,
        owner: &str,
    ) -> Result<PanelProviderManifest, ProviderContractError> {
        self.providers.unregister(owner)
    }

    /// Owners declaring `panel_type`, in lexicographic order.
    #[must_use]
    pub fn providers_for_type(&self, panel_type: PanelType) -> Vec<String> {
        self.providers.providers_for_type(panel_type)
    }

    /// Registration generation for `owner`, if registered.
    #[must_use]
    pub fn provider_generation_of(&self, owner: &str) -> Option<u64> {
        self.providers.generation_of(owner)
    }

    /// Creates a panel gated on a registered provider declaration.
    ///
    /// The provider must be registered and must declare `panel_type`;
    /// otherwise creation fails closed without allocating. Existing
    /// [`PanelRuntime::create_panel`] behavior is unchanged.
    ///
    /// # Errors
    ///
    /// [`PanelError::UnknownPanelType`] when `owner` is unknown or does not
    /// declare `panel_type`, plus the [`PanelRuntime::create_panel`] errors.
    pub fn create_panel_for_provider(
        &mut self,
        owner: &str,
        panel_type: PanelType,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        let declares = self
            .providers
            .get(owner)
            .is_some_and(|manifest| manifest.declares(panel_type));
        if !declares {
            let value = if self.providers.get(owner).is_none() {
                owner.to_string()
            } else {
                panel_type.as_str().to_string()
            };
            return Err(PanelError::UnknownPanelType { value });
        }
        self.registry.create_panel(panel_type, workspace)
    }

    /// Declares a v1 Core topic `bitty.panel:<family>.<event>` through the
    /// accepted [`EventTopic`] grammar (issue #1000).
    ///
    /// Providers subscribe to these; only the host mints them.
    ///
    /// # Errors
    ///
    /// [`PanelError::UnknownTopic`] or [`PanelError::TooManyTopics`].
    pub fn declare_core_topic(
        &mut self,
        family: BusTopicFamily,
        event: &str,
    ) -> Result<EventTopic, PanelError> {
        let topic = core_topic(family, event)?;
        self.registry.declare_topic(topic.as_str())
    }

    /// Publishes a bounded payload on a freshly minted v1 Core topic through
    /// live host mediation (issue #1000).
    ///
    /// Mints `bitty.panel:<family>.<event>` via [`core_topic`], then routes
    /// through [`PanelRuntime::publish`] with the v1 publish-capability gate
    /// and stale-handle validation, so a real event type
    /// (for example `git.branch-changed`) exercises the taxonomy, ledger,
    /// and queue budgets in one call.
    ///
    /// # Errors
    ///
    /// [`PanelError::UnknownTopic`], [`PanelError::CapabilityDenied`],
    /// [`PanelError::StaleHandle`], or [`PanelError::PayloadTooLarge`].
    pub fn publish_core(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
        family: BusTopicFamily,
        event: &str,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        let topic = core_topic(family, event)?;
        self.publish(id, generation, &topic, payload)
    }

    /// Resolves the v1 cross-window routing decision for one process
    /// (issue #1000).
    ///
    /// Same-window traffic routes [`RoutingScope::InProcess`]; cross-window
    /// traffic fails closed with [`RoutingError::CrossProcessDeferred`]
    /// until an IPC framing follow-up lands.
    ///
    /// # Errors
    ///
    /// [`RoutingError::CrossProcessDeferred`] for differing windows.
    pub fn check_window_route(
        from_window: u64,
        to_window: u64,
    ) -> Result<RoutingScope, RoutingError> {
        CrossWindowRoute::new(from_window, to_window).resolve()
    }

    /// Routes a validated [`AgentMessage`] envelope through the routable
    /// ledger (issue #1001, `OQ-058` delivery semantics).
    ///
    /// The host checks sender/recipient registration against the live
    /// [`PanelProviderRegistry`] so a missing or stale route fails closed
    /// (`SMO-3`) instead of falling back to an ambient recipient, then
    /// admits the envelope into the bounded [`RoutableLedger`] (identity,
    /// deadline, priority, dedup, expiry). Ledger state is unchanged when
    /// attribution or admission fails.
    ///
    /// # Errors
    ///
    /// [`RoutableError::UnknownSender`], [`RoutableError::UnknownRecipient`],
    /// [`RoutableError::Expired`], [`RoutableError::DuplicateMessageId`],
    /// or [`RoutableError::TooManyInflight`].
    pub fn route_message(
        &mut self,
        message: AgentMessage,
        now_ticks: u64,
    ) -> Result<(), RoutableError> {
        if self.providers.get(message.sender()).is_none() {
            return Err(RoutableError::UnknownSender {
                sender: message.sender().to_string(),
            });
        }
        if self.providers.get(message.recipient()).is_none() {
            return Err(RoutableError::UnknownRecipient {
                recipient: message.recipient().to_string(),
            });
        }
        self.routable.send(message, now_ticks)
    }

    /// Cancels an inflight routable envelope by `message_id` (issue #1001).
    ///
    /// Explicit removal for cancellation: the envelope leaves the inflight
    /// set and is handed back for audit.
    ///
    /// # Errors
    ///
    /// [`RoutableError::UnknownMessageId`]; state is unchanged.
    pub fn cancel_message(&mut self, message_id: u64) -> Result<AgentMessage, RoutableError> {
        self.routable.cancel(message_id)
    }

    /// Drops every routable envelope expired at `now_ticks` (issue #1001).
    ///
    /// Returned in `message_id` order for deterministic audit.
    #[must_use]
    pub fn sweep_expired_messages(&mut self, now_ticks: u64) -> Vec<AgentMessage> {
        self.routable.sweep_expired(now_ticks)
    }

    /// Drains up to `max` routable envelopes, highest-priority first
    /// (issue #1001).
    #[must_use]
    pub fn drain_routable_by_priority(&mut self, max: usize) -> Vec<AgentMessage> {
        self.routable.drain_by_priority(max)
    }

    /// Number of inflight routable envelopes.
    #[must_use]
    pub fn routable_len(&self) -> usize {
        self.routable.len()
    }
}

/// Maps a [`PlacementError`] bind failure onto the host [`PanelError`]
/// vocabulary after rolling the registry mount back.
fn map_placement_bind(
    err: PlacementError,
    panel: super::panel::PanelId,
    view: ViewId,
    placement: &Placement,
) -> PanelError {
    match err {
        PlacementError::ViewOccupied { view: occupied } => {
            let existing = placement.panel_of(occupied).unwrap_or(panel);
            PanelError::AlreadyMounted {
                view_id: occupied,
                existing: ViewContent::Panel(existing),
            }
        }
        PlacementError::PanelAlreadyMounted { panel: mounted } => {
            let current_view = placement.view_of(mounted).unwrap_or(view);
            PanelError::PanelAlreadyMounted {
                panel_id: mounted,
                current_view,
            }
        }
        PlacementError::NotMounted { panel: missing } => PanelError::NotFound {
            kind: "panel",
            id_raw: missing.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::panel::{BUS_BATCH_MAX_BYTES, BUS_BATCH_MAX_EVENTS};
    use super::*;

    fn runtime() -> PanelRuntime {
        PanelRuntime::new(PanelRegistryConfig::default()).unwrap()
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId::new(1)
    }

    fn view(raw: u64) -> ViewId {
        ViewId::new(raw)
    }

    #[test]
    fn lifecycle_create_mount_focus_suspend_resume_dispose() {
        let mut host = runtime();
        let handle = host
            .create_panel(PanelType::Terminal, Some(workspace()))
            .unwrap();
        assert_eq!(
            host.panel_state(handle.id, handle.generation).unwrap(),
            PanelState::Created
        );
        host.mount_panel(handle.id, handle.generation, view(10))
            .unwrap();
        host.focus_panel(handle.id, handle.generation, workspace())
            .unwrap();
        host.suspend_panel(handle.id, handle.generation).unwrap();
        assert_eq!(
            host.panel_state(handle.id, handle.generation).unwrap(),
            PanelState::Suspended
        );
        host.resume_panel(handle.id, handle.generation).unwrap();
        host.dispose_panel(handle.id, handle.generation).unwrap();
        assert_eq!(host.panel_count(), 0);
    }

    #[test]
    fn stale_generation_rejected_before_any_access() {
        let mut host = runtime();
        let handle = host
            .create_panel(PanelType::Rich, Some(workspace()))
            .unwrap();
        let stale = Generation(handle.generation.0.wrapping_add(1000).max(1));
        assert!(matches!(
            host.mount_panel(handle.id, stale, view(10)),
            Err(PanelError::StaleHandle { .. })
        ));
        assert!(matches!(
            host.panel_state(handle.id, stale),
            Err(PanelError::StaleHandle { .. })
        ));
        // The valid handle still works afterwards: failure was fail-closed.
        host.mount_panel(handle.id, handle.generation, view(10))
            .unwrap();
    }

    #[test]
    fn bus_mediation_requires_capability() {
        let mut host = runtime();
        let publisher = host
            .create_panel(PanelType::Helper, Some(workspace()))
            .unwrap();
        let subscriber = host
            .create_panel(PanelType::Helper, Some(workspace()))
            .unwrap();
        let topic = host.declare_topic("example.git:branch-changed").unwrap();
        let payload = BoundedPayload::try_new("main").unwrap();
        // Deny-by-default on both directions.
        assert!(matches!(
            host.publish(publisher.id, publisher.generation, &topic, payload.clone()),
            Err(PanelError::CapabilityDenied { .. })
        ));
        assert!(matches!(
            host.subscribe(subscriber.id, subscriber.generation, &topic),
            Err(PanelError::CapabilityDenied { .. })
        ));
        assert_eq!(host.bus_total_events(), 0);
    }

    #[test]
    fn granted_panels_publish_and_drain_roundtrip() {
        let mut host = runtime();
        let publisher = host
            .create_panel(PanelType::Helper, Some(workspace()))
            .unwrap();
        let subscriber = host
            .create_panel(PanelType::Helper, Some(workspace()))
            .unwrap();
        host.grant_capability(publisher.id, publisher.generation, BUS_PUBLISH_CAPABILITY)
            .unwrap();
        host.grant_capability(
            subscriber.id,
            subscriber.generation,
            BUS_SUBSCRIBE_CAPABILITY,
        )
        .unwrap();
        let topic = host.declare_topic("example.git:branch-changed").unwrap();
        host.subscribe(subscriber.id, subscriber.generation, &topic)
            .unwrap();
        host.publish(
            publisher.id,
            publisher.generation,
            &topic,
            BoundedPayload::try_new("main").unwrap(),
        )
        .unwrap();
        let events = host
            .drain_batch(
                subscriber.id,
                subscriber.generation,
                topic.as_str(),
                BUS_BATCH_MAX_EVENTS,
                BUS_BATCH_MAX_BYTES,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload.as_str(), "main");
    }

    #[test]
    fn stale_publisher_cannot_emit() {
        let mut host = runtime();
        let handle = host
            .create_panel(PanelType::Helper, Some(workspace()))
            .unwrap();
        host.grant_capability(handle.id, handle.generation, BUS_PUBLISH_CAPABILITY)
            .unwrap();
        let topic = host.declare_topic("example.helper:exited").unwrap();
        let stale = Generation(handle.generation.0.wrapping_add(7).max(1));
        assert!(matches!(
            host.publish(
                handle.id,
                stale,
                &topic,
                BoundedPayload::try_new("x").unwrap()
            ),
            Err(PanelError::CapabilityDenied { .. }) | Err(PanelError::StaleHandle { .. })
        ));
    }

    #[test]
    fn disposed_panel_drains_closed() {
        let mut host = runtime();
        let handle = host
            .create_panel(PanelType::Helper, Some(workspace()))
            .unwrap();
        host.dispose_panel(handle.id, handle.generation).unwrap();
        assert!(matches!(
            host.panel_state(handle.id, handle.generation),
            Err(PanelError::NotFound { .. })
        ));
    }
}
