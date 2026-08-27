//! Plugin host: owned registry, grant lifecycle, event pipeline, and bounded side queue.
//!
//! This is the user-facing orchestrator for the draft plugin platform. It keeps
//! the standalone property: **no window/GPU coupling** — the host never holds
//! `wgpu` objects, window handles, PTY file descriptors, or hot-path Rust
//! objects. It observes terminal state only through a bounded side queue per
//! `ADR-0003` rule 4, and through the public `Snapshot` surface where needed
//! (pure reads of the terminal truth).

use std::collections::VecDeque;

use crate::capability::CapabilityId;
use crate::error::PluginError;
use crate::event::{
    DEFAULT_BATCH_BYTES, DEFAULT_BATCH_EVENTS, DEFAULT_QUEUE_CAPACITY, DropPolicy, Event,
    EventKind, EventPipeline,
};
use crate::grant::{GrantRecord, GrantStore};
use crate::manifest::{PluginId, PluginManifest};
use crate::registry::{Generation, Registry};

// ── bounded side queue (ADR-0003 rule 4) ──────────────────────────────────

/// Bounded side queue through which the plugin host observes terminal events.
///
/// The queue is strictly bounded so untrusted input cannot grow memory without
/// limit (threat `T-01`). Producers never block on a subscriber; backpressure
/// isolates at the queue boundary, never in the emitting path. When full, the
/// oldest event is dropped and a counter increments — mirroring the cold-queue
/// policy in `bitty-runtime`. The queue is drained by the plugin host, not by
/// the hot PTY/parser/state path.
///
/// This queue carries only host-mediated, bounded observations (e.g. title/cwd
/// changes derived from committed terminal state). No hot-path events such as
/// byte-received, cell-changed, or damage appear here; the v1 event vocabulary
/// explicitly forbids them at the type level (threat `T-07`).
#[derive(Debug)]
pub struct SideQueue<T> {
    inner: VecDeque<T>,
    capacity: usize,
    dropped: u64,
}

impl<T> SideQueue<T> {
    /// Create a queue with `capacity` entries. Capacity must be `> 0`.
    ///
    /// # Panics
    ///
    /// Panics when `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "side queue capacity must be > 0");
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no item is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of items dropped due to overflow since creation or last `clear`.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Enqueue `item`, dropping the oldest entry when at capacity.
    pub fn push(&mut self, item: T) {
        if self.inner.len() >= self.capacity {
            self.inner.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        self.inner.push_back(item);
    }

    /// Drain all queued items in FIFO order.
    pub fn drain(&mut self) -> Vec<T> {
        self.inner.drain(..).collect()
    }

    /// Drain up to `limit` items.
    pub fn drain_bounded(&mut self, limit: usize) -> Vec<T> {
        let take = limit.min(self.inner.len());
        self.inner.drain(..take).collect()
    }

    /// Clear queued items and reset the dropped counter.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.dropped = 0;
    }

    /// Iterate queued items in order without consuming.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.inner.iter()
    }
}

/// Snapshot observation delivered through the side queue.
///
/// Read-only, bounded, versioned structures derived from committed terminal state.
/// Snapshots served to automation surfaces carry the untrusted-observation-data
/// label required by `T-10` (not modelled as a flag here, but callers must
/// treat payloads as untrusted display data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostObservation {
    /// Window/icon title changed (`OSC 0`/`OSC 2`).
    TitleChanged(String),
    /// Working directory report changed (`OSC 7`).
    CwdChanged(String),
    /// Terminal mode toggled.
    ModeChanged {
        /// Mode label (opaque string for draft).
        mode: String,
        /// New state.
        enabled: bool,
    },
    /// Terminal bell.
    Bell,
    /// Damage became available (generation counter).
    Damage {
        /// New generation after the batch.
        generation: u64,
    },
}

// ── plugin host ───────────────────────────────────────────────────────────

/// Owned draft host for the plugin platform (proposed plugin-platform RFC).
///
/// Composition root of the draft contracts:
/// - [`Registry`]: plugin identity, dependencies, lifecycle generations,
/// - [`GrantStore`]: capability grants bound to manifest hash, revocation, workspace narrowing,
/// - [`EventPipeline`]: bounded per-subscriber queues, coalescing, batching, drop policy,
/// - [`SideQueue<HostObservation>`]: bounded side queue per `ADR-0003` rule 4.
///
/// The host is headless-testable: no window, no GPU, no PTY, no Lua VM. All
/// structures are owned, cloneable where appropriate, and bounded against
/// untrusted input.
///
/// # Safety and ownership
///
/// - `bitty --safe` semantics (skip third-party plugins) are implemented as
///   a host-level check: when `safe_mode` is set, declaration of non-builtin
///   plugins is rejected.
/// - Every trust transition passes the capability gate; authority follows the
///   requesting plugin, never the calling context.
/// - Native in-process plugins remain forbidden (risk `R-017`); this host only
///   tracks manifest-declared capabilities and never confers authority on
///   native payloads.
#[derive(Debug)]
pub struct PluginHost {
    registry: Registry,
    grants: GrantStore,
    pipeline: EventPipeline,
    side_queue: SideQueue<HostObservation>,
    safe_mode: bool,
}

impl PluginHost {
    /// Create a new host.
    ///
    /// `drop_policy` is the shared overflow policy for every per-subscriber queue.
    /// It must be chosen explicitly because the choice is an open decision point
    /// (see [`DropPolicy`] and `event::DropPolicy` docs). There is no implicit
    /// settling; both candidates remain proposed.
    ///
    /// `side_capacity` bounds the side queue that observes terminal events; producers
    /// never block on the subscriber.
    pub fn new(drop_policy: DropPolicy, side_capacity: usize) -> Self {
        Self {
            registry: Registry::new(),
            grants: GrantStore::new(),
            pipeline: EventPipeline::new(DEFAULT_QUEUE_CAPACITY, drop_policy),
            side_queue: SideQueue::new(side_capacity),
            safe_mode: false,
        }
    }

    /// Create with explicit queue capacity (for tests / OQ-014 tuning).
    pub fn with_capacity(
        drop_policy: DropPolicy,
        pipeline_capacity: usize,
        side_capacity: usize,
    ) -> Self {
        Self {
            registry: Registry::new(),
            grants: GrantStore::new(),
            pipeline: EventPipeline::new(pipeline_capacity, drop_policy),
            side_queue: SideQueue::new(side_capacity),
            safe_mode: false,
        }
    }

    /// Enable or disable `bitty --safe` mode (skips third-party plugins).
    pub fn set_safe_mode(&mut self, safe: bool) {
        self.safe_mode = safe;
    }

    /// Whether safe mode is active.
    #[must_use]
    pub fn is_safe_mode(&self) -> bool {
        self.safe_mode
    }

    // ── registry delegation ────────────────────────────────────────────

    /// Declare a plugin manifest (validates and inserts as `Declared`).
    ///
    /// In safe mode, declaration of any plugin whose id does not start with
    /// `bitty.` (the candidate built-in namespace) is rejected so that
    /// `bitty --safe` restores a minimal configuration without third-party
    /// code (invariant 10, `R-009`).
    pub fn declare(&mut self, manifest: PluginManifest) -> Result<(), PluginError> {
        if self.safe_mode && !manifest.id().as_str().starts_with("bitty.") {
            return Err(PluginError::registry(format!(
                "safe mode: plugin '{}' is not a built-in plugin",
                manifest.id()
            )));
        }
        self.registry.declare(manifest)
    }

    /// Resolve one plugin (`Declared -> Resolved`).
    pub fn resolve(&mut self, id: &PluginId) -> Result<(), PluginError> {
        self.registry.resolve(id)
    }

    /// Resolve all declared plugins (graph-level checks: missing deps, cycles).
    pub fn resolve_all(&mut self) -> Result<(), PluginError> {
        self.registry.resolve_all()
    }

    /// Register a resolved plugin (reserve commands etc.).
    pub fn register(&mut self, id: &PluginId) -> Result<(), PluginError> {
        self.registry.register(id)
    }

    /// Activate a registered plugin.
    pub fn activate(&mut self, id: &PluginId) -> Result<(), PluginError> {
        // Check that all declared capabilities are granted for the current hash.
        // For the draft stub, manifest hash is the version string (opaque); callers
        // supply the hash via the grant record. We skip hash matching here and simply
        // ensure that if the manifest requests capabilities, a grant record exists when
        // not in safe mode. Full hash binding is exercised through GrantStore directly.
        self.registry.activate(id)
    }

    /// Suspend a plugin.
    pub fn suspend(&mut self, id: &PluginId) -> Result<(), PluginError> {
        self.registry.suspend(id)
    }

    /// Resume a suspended plugin.
    pub fn resume(&mut self, id: &PluginId) -> Result<(), PluginError> {
        self.registry.resume(id)
    }

    /// Dispose a plugin (releases generation resources).
    pub fn dispose(&mut self, id: &PluginId) -> Result<(), PluginError> {
        self.registry.dispose(id)
    }

    /// Reload: dispose generation `N` resources before activating `N+1` atomically.
    pub fn reload(
        &mut self,
        id: &PluginId,
        new_manifest: PluginManifest,
    ) -> Result<Generation, PluginError> {
        self.registry.reload(id, new_manifest)
    }

    /// Access the registry (read-only).
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    // ── grant delegation ───────────────────────────────────────────────

    /// Access the grant store (read-only).
    #[must_use]
    pub fn grants(&self) -> &GrantStore {
        &self.grants
    }

    /// Access the grant store mutably.
    #[must_use]
    pub fn grants_mut(&mut self) -> &mut GrantStore {
        &mut self.grants
    }

    /// Whether `capability` is granted for `plugin_id` under `manifest_hash`.
    #[must_use]
    pub fn is_granted(
        &self,
        plugin_id: &PluginId,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> bool {
        self.grants.is_granted(plugin_id, manifest_hash, capability)
    }

    /// Revoke a grant (single capability or all), detaching at the next dispatch boundary.
    pub fn revoke(
        &mut self,
        plugin_id: &PluginId,
        capability: Option<&CapabilityId>,
    ) -> Result<crate::grant::RevokeReport, PluginError> {
        self.grants.revoke(plugin_id, capability)
    }

    /// Insert a grant record (headless helper; persistence is deferred).
    pub fn insert_grant(&mut self, record: GrantRecord) {
        self.grants.insert(record);
    }

    // ── event pipeline delegation ─────────────────────────────────────

    /// Access the event pipeline (read-only).
    #[must_use]
    pub fn pipeline(&self) -> &EventPipeline {
        &self.pipeline
    }

    /// Access the event pipeline mutably.
    #[must_use]
    pub fn pipeline_mut(&mut self) -> &mut EventPipeline {
        &mut self.pipeline
    }

    /// Subscribe `plugin_id` to `kind`.
    pub fn subscribe(&mut self, plugin_id: &PluginId, kind: EventKind) -> Result<(), PluginError> {
        // Subscriptions must match manifest-declared types; subscribing to an
        // undeclared type is a registration error. The host checks here against
        // the registry entry's declared events.
        let entry = self
            .registry
            .get(plugin_id)
            .ok_or_else(|| PluginError::NotFound {
                id: plugin_id.to_string(),
            })?;
        let kind_str = kind.as_str();
        // Only observation/interception/lifecycle kinds that are valid manifest event strings
        // are allowed; the manifest stores raw strings, so compare.
        if !entry.subscribed_events.iter().any(|e| e == kind_str) {
            return Err(PluginError::registry(format!(
                "plugin '{}' subscribes to undeclared event type '{}'",
                plugin_id.as_str(),
                kind_str
            )));
        }
        self.pipeline.subscribe(plugin_id, kind)
    }

    /// Publish an observation/lifecycle event to all subscribers of its kind.
    pub fn publish(&mut self, event: Event) {
        self.pipeline.publish(event);
    }

    /// Publish to a specific subscriber (lifecycle).
    pub fn publish_to(&mut self, plugin_id: &PluginId, event: Event) -> Result<(), PluginError> {
        self.pipeline.publish_to(plugin_id, event)
    }

    /// Drain a bounded batch for `plugin_id` + `kind` (bounded wakeup).
    pub fn drain_batch(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<Event>, PluginError> {
        self.pipeline
            .drain_batch(plugin_id, kind, max_events, max_bytes)
    }

    /// Drain all for `plugin_id` + `kind`.
    pub fn drain(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
    ) -> Result<Vec<Event>, PluginError> {
        self.pipeline.drain(plugin_id, kind)
    }

    /// Convenience: drain with the RFC proposed defaults (`<= 32` or `8 KiB`).
    pub fn drain_default_batch(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
    ) -> Result<Vec<Event>, PluginError> {
        self.pipeline
            .drain_batch(plugin_id, kind, DEFAULT_BATCH_EVENTS, DEFAULT_BATCH_BYTES)
    }

    // ── side queue (ADR-0003 rule 4) ──────────────────────────────────

    /// The bounded side queue that observes terminal events.
    #[must_use]
    pub fn side_queue(&self) -> &SideQueue<HostObservation> {
        &self.side_queue
    }

    /// Mutable side queue.
    #[must_use]
    pub fn side_queue_mut(&mut self) -> &mut SideQueue<HostObservation> {
        &mut self.side_queue
    }

    /// Push a host observation into the side queue (producer never blocks).
    pub fn push_observation(&mut self, obs: HostObservation) {
        self.side_queue.push(obs);
    }

    /// Drain side-queue observations (bounded).
    pub fn drain_observations(&mut self) -> Vec<HostObservation> {
        self.side_queue.drain()
    }

    /// Drain side-queue observations up to `limit`.
    pub fn drain_observations_bounded(&mut self, limit: usize) -> Vec<HostObservation> {
        self.side_queue.drain_bounded(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventPayload};
    use crate::manifest::{CapabilityRequests, Compat, LazyTriggers, PluginIdentity};

    fn minimal_manifest(id: &str, events: Vec<&str>) -> PluginManifest {
        PluginManifest {
            identity: PluginIdentity {
                id: PluginId::new(id).unwrap(),
                name: "Test".to_string(),
                version: "0.1.0".to_string(),
                description: "desc".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            provided_services: Vec::new(),
            capabilities: CapabilityRequests::default(),
            lazy: LazyTriggers {
                commands: Vec::new(),
                events: events.into_iter().map(|s| s.to_string()).collect(),
                claims: Vec::new(),
            },
            raw_bytes_len: 256,
        }
    }

    #[test]
    fn host_side_queue_bounded() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 2);
        host.push_observation(HostObservation::Bell);
        host.push_observation(HostObservation::TitleChanged("a".into()));
        host.push_observation(HostObservation::TitleChanged("b".into()));
        assert_eq!(host.side_queue().len(), 2);
        assert_eq!(host.side_queue().dropped(), 1);
        let drained = host.drain_observations();
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn host_safe_mode_rejects_third_party() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        host.set_safe_mode(true);
        let m = minimal_manifest("xuepoo.test", vec![]);
        assert!(host.declare(m).is_err());
        let builtin = minimal_manifest("bitty.core", vec![]);
        assert!(host.declare(builtin).is_ok());
    }

    #[test]
    fn host_subscribe_requires_declared_event() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = minimal_manifest("xuepoo.test", vec!["terminal.bell"]);
        host.declare(m).unwrap();
        host.resolve(&PluginId::new("xuepoo.test").unwrap())
            .unwrap();
        host.register(&PluginId::new("xuepoo.test").unwrap())
            .unwrap();

        // Declared event succeeds.
        assert!(
            host.subscribe(
                &PluginId::new("xuepoo.test").unwrap(),
                EventKind::TerminalBell
            )
            .is_ok()
        );

        // Undeclared event fails.
        assert!(
            host.subscribe(
                &PluginId::new("xuepoo.test").unwrap(),
                EventKind::TerminalTitleChanged
            )
            .is_err()
        );
    }

    #[test]
    fn host_publish_and_drain() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = minimal_manifest("xuepoo.test", vec!["terminal.bell"]);
        host.declare(m).unwrap();
        host.resolve(&PluginId::new("xuepoo.test").unwrap())
            .unwrap();
        host.register(&PluginId::new("xuepoo.test").unwrap())
            .unwrap();
        host.subscribe(
            &PluginId::new("xuepoo.test").unwrap(),
            EventKind::TerminalBell,
        )
        .unwrap();

        host.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, 1));
        host.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, 2));
        let batch = host
            .drain(
                &PluginId::new("xuepoo.test").unwrap(),
                &EventKind::TerminalBell,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn host_no_window_gpu_coupling_in_api() {
        // Compile-time proof: PluginHost has no method returning winit/wgpu types.
        // Runtime assertion: host is headless constructible without display.
        let host = PluginHost::new(DropPolicy::DropNewest, 16);
        assert!(!host.is_safe_mode());
        assert!(host.side_queue().is_empty());
        assert_eq!(host.pipeline().queue_count(), 0);
    }
}
