#![forbid(unsafe_code)]
//! `PanelRuntime` host abstraction (CW-20, issue #998).
//!
//! Contract source: the accepted Panel Runtime RFC names `PanelRuntime`
//! as the Core-owned host that creates, mounts, suspends, resumes, and
//! disposes panels, validates `(PanelId, Generation)`, and mediates bus
//! traffic — but no `PanelRuntime` type exists; `PanelRegistry` is the
//! orchestration. This module records the spelling decision:
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
//!
//! The runtime holds no PTY descriptor, GPU object, or OS window handle;
//! those remain with `bitty-pty`, `bitty-render`, and `bitty-platform`.
//! Every failure is fail-closed and typed: the previous valid state is
//! retained and a bounded diagnostic counter advances in the registry.

use bitty_ui::ViewId;

use super::event_bus_v1::{BUS_PUBLISH_CAPABILITY, BUS_SUBSCRIBE_CAPABILITY};
use super::panel::PanelType;
use super::panel::{
    BoundedPayload, BusEvent, EventTopic, PanelError, PanelHandle, PanelRegistry,
    PanelRegistryConfig, PanelState,
};
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
#[derive(Debug)]
pub struct PanelRuntime {
    registry: PanelRegistry,
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
        })
    }

    /// Borrows the underlying identity/attachment store.
    #[must_use]
    pub fn registry(&self) -> &PanelRegistry {
        &self.registry
    }

    /// Creates a panel of `panel_type` without mounting it.
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
        self.registry.create_panel(panel_type, workspace)
    }

    /// Mounts a created panel onto an empty view (`Created -> Mounted`).
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
        self.registry.mount_panel(id, generation, view)
    }

    /// Unmounts a panel, returning its former view (suspend, not destroy).
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
        self.registry.unmount_panel(id, generation)
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
    /// # Errors
    ///
    /// [`PanelError::StaleHandle`] or [`PanelError::NotFound`].
    pub fn dispose_panel(
        &mut self,
        id: super::panel::PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.registry.dispose_panel(id, generation)
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
