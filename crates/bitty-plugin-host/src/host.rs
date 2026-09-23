//! Plugin host: owned registry, grant lifecycle, event pipeline, and bounded side queue.
//!
//! This is the user-facing orchestrator for the draft plugin platform. It keeps
//! the standalone property: **no window/GPU coupling** — the host never holds
//! `wgpu` objects, window handles, PTY file descriptors, or hot-path Rust
//! objects. It observes terminal state only through a bounded side queue per
//! `ADR-0003` rule 4, and through the public `Snapshot` surface where needed
//! (pure reads of the terminal truth).

use std::collections::{BTreeSet, VecDeque};

use crate::capability::CapabilityId;
use crate::error::PluginError;
use crate::event::{
    BudgetSnapshot, DEFAULT_BATCH_BYTES, DEFAULT_BATCH_EVENTS, DEFAULT_QUEUE_CAPACITY, DropPolicy,
    Event, EventKind, EventPipeline, GLOBAL_QUEUED_BYTES_LIMIT, GLOBAL_QUEUED_EVENT_LIMIT,
    PER_PLUGIN_QUEUED_BYTES_LIMIT, PER_PLUGIN_QUEUED_EVENT_LIMIT, PER_SUBSCRIPTION_QUEUE_LIMIT,
};
use crate::grant::{GrantRecord, GrantStore};
use crate::manifest::{FsAccess, PluginId, PluginManifest};
use crate::registry::{Generation, PluginState, Registry};

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

/// Owned host for the accepted plugin platform (accepted Plugin Platform RFC 2026-08-27).
///
/// Composition root of the accepted contracts (closed OQ-011/OQ-012/OQ-013; bitty-docs open-questions register):
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
    audit: crate::effective::AuditLedger,
    secrets: crate::secrets::SecretStore,
    fs_policy: crate::fs_authz::SensitivePathPolicy,
    fs_audit: crate::fs_authz::FsAuditLedger,
}

impl PluginHost {
    /// Create a new host.
    ///
    /// `drop_policy` is the shared overflow policy for every per-subscriber queue.
    /// It must be chosen explicitly (see [`DropPolicy`] and `event::DropPolicy`
    /// docs). There is no implicit settling; `DropOldest` is the accepted v1
    /// default, `DropNewest` remains available via explicit opt-in.
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
            audit: crate::effective::AuditLedger::new(),
            secrets: crate::secrets::SecretStore::new(),
            fs_policy: crate::fs_authz::SensitivePathPolicy::default_policy(),
            fs_audit: crate::fs_authz::FsAuditLedger::new(),
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
            audit: crate::effective::AuditLedger::new(),
            secrets: crate::secrets::SecretStore::new(),
            fs_policy: crate::fs_authz::SensitivePathPolicy::default_policy(),
            fs_audit: crate::fs_authz::FsAuditLedger::new(),
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

    /// Activate a registered plugin (fail-closed capability gate).
    ///
    /// Before `registry.activate`, validates:
    /// - manifest hash (`PluginManifest::manifest_hash()`) matches the stored
    ///   [`GrantRecord::manifest_hash`] for this plugin,
    /// - every declared capability (flat `capability.ids` plus expanded
    ///   `fs.read:PARAM`/`fs.write:PARAM` from `capabilities.filesystem`)
    ///   is present in the grant record for that hash (deny-by-default),
    /// - the grant record exists and is not denied, and no denial marker exists.
    ///
    /// If the manifest declares no capabilities, no grant record is required
    /// and activation proceeds directly (least authority). Otherwise failure
    /// is fail-closed with an owned [`PluginError::Grant`] describing the
    /// missing manifest hash or missing capabilities. No partially activated
    /// state is produced on failure.
    pub fn activate(&mut self, id: &PluginId) -> Result<(), PluginError> {
        // Snapshot entry without holding borrow across grant checks.
        let entry = self
            .registry
            .get(id)
            .cloned()
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if entry.state != PluginState::Registered {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: PluginState::Registered.to_string(),
            });
        }
        // Collect required capabilities: flat ids + filesystem expansions.
        let required: BTreeSet<CapabilityId> = {
            let mut set = entry.manifest.capabilities.ids.clone();
            for req in &entry.manifest.capabilities.filesystem {
                for pat in &req.paths {
                    let cap_str = match req.access {
                        FsAccess::Read => format!("fs.read:{pat}"),
                        FsAccess::Write => format!("fs.write:{pat}"),
                    };
                    let cap = CapabilityId::parse(&cap_str).map_err(|e| {
                        PluginError::grant(format!(
                            "invalid filesystem capability '{cap_str}': {e}"
                        ))
                    })?;
                    set.insert(cap);
                }
            }
            set
        };
        // No declared authority: activation does not require a grant record.
        if required.is_empty() {
            return self.registry.activate(id);
        }
        let hash = entry.manifest.manifest_hash();
        // Grant record must exist and match hash.
        let record = self.grants.get(id).ok_or_else(|| {
            PluginError::grant(format!(
                "missing grant record for '{}' (manifest hash {})",
                id.as_str(),
                hash
            ))
        })?;
        if record.denied {
            return Err(PluginError::grant(format!(
                "plugin '{}' is denied; explicit re-grant required (hash {})",
                id.as_str(),
                hash
            )));
        }
        if self.grants.is_denied(id) {
            return Err(PluginError::grant(format!(
                "plugin '{}' has denial marker (hash {})",
                id.as_str(),
                hash
            )));
        }
        if record.manifest_hash != hash {
            return Err(PluginError::grant(format!(
                "manifest hash mismatch for '{}': expected {}, got {}",
                id.as_str(),
                hash,
                record.manifest_hash
            )));
        }
        // Explicit per-capability denials (single-capability revocation,
        // CTX-0465) fail closed here as an explicit re-grant error, not a
        // generic missing grant: re-prompting a revoked capability requires
        // explicit user action (`clear_cap_denial`), never a silent
        // re-request.
        let explicitly_denied: Vec<String> = required
            .iter()
            .filter(|cap| self.grants.is_cap_denied(id, cap))
            .map(|cap| cap.as_str().to_string())
            .collect();
        if !explicitly_denied.is_empty() {
            return Err(PluginError::grant(format!(
                "explicitly denied capabilities for '{}' (hash {}): {}; explicit re-grant required",
                id.as_str(),
                hash,
                explicitly_denied.join(", ")
            )));
        }
        // Every declared capability must be granted for this hash.
        let mut missing: Vec<String> = Vec::new();
        for cap in &required {
            if !self
                .grants
                .is_granted(id, &hash, cap, &entry.manifest.capabilities)
            {
                missing.push(cap.as_str().to_string());
            }
        }
        if !missing.is_empty() {
            return Err(PluginError::grant(format!(
                "missing grants for '{}' (hash {}): {}",
                id.as_str(),
                hash,
                missing.join(", ")
            )));
        }
        self.registry.activate(id)
    }

    /// Activate without capability gating (test-only).
    ///
    /// Exposed `pub(crate)` so unit tests can exercise registry lifecycle
    /// without wiring grant records, while the public [`Self::activate`]
    /// remains strictly gated. Production callers must not use this.
    #[allow(dead_code)]
    pub(crate) fn activate_unchecked_for_test(&mut self, id: &PluginId) -> Result<(), PluginError> {
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
        let events = self
            .registry
            .get(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?
            .subscribed_events
            .clone();
        let result = self.registry.dispose(id);
        if result.is_ok() {
            // Drop all generation-owned queues only after the lifecycle transition
            // succeeds. This is the reclaim boundary for PB-3.
            for event in events {
                if let Ok(kind) = EventKind::parse(&event) {
                    let _ = self.pipeline.unsubscribe(id, &kind);
                }
            }
        }
        result
    }

    /// Fully remove a plugin generation, releasing its identity and all owned
    /// resources (command ownership and per-generation event queues).
    ///
    /// Unlike [`PluginHost::dispose`], the registry identity is purged so the
    /// same plugin id can be declared again. This is the rollback primitive
    /// that guarantees a failed activation leaves no partial activation and
    /// allows a clean retry (RFC `plugin-host-runtime-rfc` A.4 rule 4).
    ///
    /// # Errors
    ///
    /// [`PluginError::NotFound`] when `id` has no registry entry. Queue
    /// unsubscribe failures are ignored; the registry entry is still purged.
    pub fn remove(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let events = self
            .registry
            .get(id)
            .map(|entry| entry.subscribed_events.clone())
            .unwrap_or_default();
        self.registry.remove(id)?;
        for event in events {
            if let Ok(kind) = EventKind::parse(&event) {
                let _ = self.pipeline.unsubscribe(id, &kind);
            }
        }
        Ok(())
    }

    /// Reload: dispose generation `N` resources before activating `N+1` atomically.
    pub fn reload(
        &mut self,
        id: &PluginId,
        new_manifest: PluginManifest,
    ) -> Result<Generation, PluginError> {
        new_manifest.validate()?;
        if new_manifest.id() != id {
            return Err(PluginError::registry(format!(
                "replacement manifest id '{}' does not match requested plugin '{}'",
                new_manifest.id(),
                id
            )));
        }
        if self.safe_mode && !id.as_str().starts_with("bitty.") {
            return Err(PluginError::registry(format!(
                "safe mode: plugin '{}' is not a built-in plugin",
                id
            )));
        }

        // Validate all replacement authority before the registry can release the
        // current generation. This keeps reload fail-closed and transactional.
        let required = Self::required_capabilities(&new_manifest)?;
        if !required.is_empty() {
            let hash = new_manifest.manifest_hash();
            let record = self.grants.get(id).ok_or_else(|| {
                PluginError::grant(format!(
                    "missing grant record for '{}' (manifest hash {})",
                    id.as_str(),
                    hash
                ))
            })?;
            if record.denied || self.grants.is_denied(id) {
                return Err(PluginError::grant(format!(
                    "plugin '{}' is denied (manifest hash {})",
                    id.as_str(),
                    hash
                )));
            }
            if record.manifest_hash != hash {
                return Err(PluginError::grant(format!(
                    "manifest hash mismatch for '{}': expected {}, got {}",
                    id.as_str(),
                    hash,
                    record.manifest_hash
                )));
            }
            let missing: Vec<String> = required
                .iter()
                .filter(|cap| {
                    !self
                        .grants
                        .is_granted(id, &hash, cap, &new_manifest.capabilities)
                })
                .map(|cap| cap.as_str().to_string())
                .collect();
            // Explicit per-capability denials (CTX-0465) surface as an
            // explicit re-grant error even when the grant is also missing.
            let explicitly_denied: Vec<String> = required
                .iter()
                .filter(|cap| self.grants.is_cap_denied(id, cap))
                .map(|cap| cap.as_str().to_string())
                .collect();
            if !explicitly_denied.is_empty() {
                return Err(PluginError::grant(format!(
                    "explicitly denied capabilities for '{}' (manifest hash {}): {}; explicit re-grant required",
                    id.as_str(),
                    hash,
                    explicitly_denied.join(", ")
                )));
            }
            if !missing.is_empty() {
                return Err(PluginError::grant(format!(
                    "missing grants for '{}' (hash {}): {}",
                    id.as_str(),
                    hash,
                    missing.join(", ")
                )));
            }
        }

        let old_events = self
            .registry
            .get(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?
            .subscribed_events
            .clone();
        let generation = self.registry.reload(id, new_manifest)?;
        // A generation owns its subscriptions. Reclaim the old queues only after
        // registry reload commits, so a failed reload leaves the old generation intact.
        for event in old_events {
            if let Ok(kind) = EventKind::parse(&event) {
                let _ = self.pipeline.unsubscribe(id, &kind);
            }
        }
        Ok(generation)
    }

    fn required_capabilities(
        manifest: &PluginManifest,
    ) -> Result<BTreeSet<CapabilityId>, PluginError> {
        manifest.capabilities.all_ids()
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
        let Some(entry) = self.registry.get(plugin_id) else {
            return false;
        };
        self.grants.is_granted(
            plugin_id,
            manifest_hash,
            capability,
            &entry.manifest.capabilities,
        )
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

    // ── effective-capability authorization (research 045 §8 §12 §13) ────

    /// Authorize a privileged request through the adopted gates plus the
    /// six-layer intersection.
    ///
    /// `stack` carries host/user/project/parent/task voices; `request` is the
    /// agent/plugin/Lua voice. The adopted trust-level admission gate
    /// (`level`, OQ-085) and role contract gate (`role`, OQ-057) run first
    /// and fail closed with [`crate::effective::DenialKind::PolicyConflict`];
    /// only a request both gates admit reaches the grant intersection.
    /// Wide declarations fail closed as self-grant, excess over the
    /// intersection denies with a reason chain, and success returns exactly
    /// what may be exercised. Outcomes are appended to the
    /// host [`crate::effective::AuditLedger`] (drop-oldest when full). The
    /// pre-existing [`PluginHost::activate`] grant gate is unchanged: this is
    /// an additional seam for privileged requests, never a bypass.
    pub fn authorize_effective(
        &mut self,
        stack: &crate::effective::EffectiveStack,
        request: &crate::effective::AgentRequest,
        kind: crate::effective::RequestKind,
        project_trusted: bool,
        level: crate::trust_levels::TrustLevel,
        role: crate::roles::AgentRole,
    ) -> Result<crate::effective::EffectiveCapability, crate::effective::EffectiveDenial> {
        let requested: Vec<String> = request
            .scope
            .caps
            .iter()
            .map(|cap| cap.as_str().to_string())
            .collect();
        match crate::effective::authorize_with_trust_and_role(
            stack,
            request,
            kind,
            project_trusted,
            level,
            role,
        ) {
            Ok(effective) => {
                let granted: Vec<String> = effective
                    .caps
                    .iter()
                    .map(|cap| cap.as_str().to_string())
                    .collect();
                self.audit.push_allow(kind, &requested, &granted);
                Ok(effective)
            }
            Err(denial) => {
                self.audit.push_deny(kind, &requested, denial.kind);
                Err(denial)
            }
        }
    }

    /// Attenuate a child request under an already-computed parent set.
    ///
    /// Children narrow, never widen: excess denies with
    /// [`crate::effective::DenialKind::SelfGrant`]. Audited like
    /// [`PluginHost::authorize_effective`].
    pub fn delegate_effective(
        &mut self,
        parent: &crate::effective::EffectiveCapability,
        request: &crate::effective::AgentRequest,
        kind: crate::effective::RequestKind,
    ) -> Result<crate::effective::EffectiveCapability, crate::effective::EffectiveDenial> {
        let requested: Vec<String> = request
            .scope
            .caps
            .iter()
            .map(|cap| cap.as_str().to_string())
            .collect();
        match crate::effective::delegate(parent, request, kind) {
            Ok(child) => {
                debug_assert!(child.is_subset_of(parent));
                let granted: Vec<String> = child
                    .caps
                    .iter()
                    .map(|cap| cap.as_str().to_string())
                    .collect();
                self.audit.push_allow(kind, &requested, &granted);
                Ok(child)
            }
            Err(denial) => {
                self.audit.push_deny(kind, &requested, denial.kind);
                Err(denial)
            }
        }
    }

    /// Attenuate a child request under `parent` after the adopted role
    /// dispatch gate (OQ-057).
    ///
    /// `role` must admit a dispatch of `child_count` subagents at chain
    /// `depth` before [`crate::effective::delegate`] runs; the gate denies
    /// fail-closed with [`crate::effective::DenialKind::PolicyConflict`] and
    /// audits like [`PluginHost::authorize_effective`]. On success the child
    /// still narrows under the parent: the role gate never grants.
    pub fn delegate_effective_with_role(
        &mut self,
        parent: &crate::effective::EffectiveCapability,
        request: &crate::effective::AgentRequest,
        kind: crate::effective::RequestKind,
        role: crate::roles::AgentRole,
        child_count: u32,
        depth: u32,
    ) -> Result<crate::effective::EffectiveCapability, crate::effective::EffectiveDenial> {
        let requested: Vec<String> = request
            .scope
            .caps
            .iter()
            .map(|cap| cap.as_str().to_string())
            .collect();
        match crate::effective::delegate_with_role(parent, request, kind, role, child_count, depth)
        {
            Ok(child) => {
                debug_assert!(child.is_subset_of(parent));
                let granted: Vec<String> = child
                    .caps
                    .iter()
                    .map(|cap| cap.as_str().to_string())
                    .collect();
                self.audit.push_allow(kind, &requested, &granted);
                Ok(child)
            }
            Err(denial) => {
                self.audit.push_deny(kind, &requested, denial.kind);
                Err(denial)
            }
        }
    }

    /// Audit ledger of effective grants and denials (oldest first).
    #[must_use]
    pub fn audit(&self) -> &crate::effective::AuditLedger {
        &self.audit
    }

    // ── filesystem authorization (research 045 §4; CTX-0523) ─────────────

    /// Access the sensitive-path policy (read-only).
    ///
    /// The policy carries the default-deny sensitive set plus explicit
    /// per-path user consent. Mutate through [`Self::grant_fs_consent`] /
    /// [`Self::revoke_fs_consent`] so consent changes audit.
    #[must_use]
    pub fn fs_policy(&self) -> &crate::fs_authz::SensitivePathPolicy {
        &self.fs_policy
    }

    /// FS authorization audit ledger (paths only, never values).
    #[must_use]
    pub fn fs_audit(&self) -> &crate::fs_authz::FsAuditLedger {
        &self.fs_audit
    }

    /// Grant explicit user consent for exactly one sensitive path.
    ///
    /// Consent is keyed by normalized path and audited; it never widens
    /// the [`crate::fs_authz::FilesystemScope`] — the scope check still
    /// applies on every request.
    pub fn grant_fs_consent(
        &mut self,
        path: &str,
        granted_at_ms: u64,
        expires_at_ms: Option<u64>,
    ) -> Result<(), PluginError> {
        self.fs_policy
            .grant_consent(path, granted_at_ms, expires_at_ms)
            .map_err(PluginError::from)?;
        self.fs_audit
            .push_consent(&[path.to_string()], format!("consent granted for '{path}'"));
        Ok(())
    }

    /// Revoke per-path consent (audited; missing grants are a no-op).
    pub fn revoke_fs_consent(&mut self, path: &str) {
        self.fs_policy.revoke_consent(path);
        self.fs_audit
            .push_consent(&[path.to_string()], format!("consent revoked for '{path}'"));
    }

    /// Authorize one FS request path through the full FS authorization path.
    ///
    /// Composition seam (conforms, never duplicates):
    ///
    /// - `stack`/`request` authorize first through the CTX-0524 six-layer
    ///   intersection ([`Self::authorize_effective`] with `FsRead`/`FsWrite`
    ///   by `is_write`); only an allowed request reaches the
    ///   [`crate::fs_authz`] layers (`FilesystemScope` +
    ///   `SensitivePathPolicy` + content detection).
    /// - Every outcome (capability denial, scope denial, consent-required,
    ///   redacted, allow) appends to both the effective ledger (via
    ///   `authorize_effective`) and the FS audit ledger (paths only).
    /// - Diagnostics quote the path, never the value.
    ///
    /// `scope_patterns` are the capability-shaped grant patterns the
    /// effective set confers for this request (e.g. the `fs.read:PARAM`
    /// params); hostile entries fail the whole request closed. `content`
    /// carries read bytes when available (write requests pass `None`);
    /// secret-shaped reads authorize as redacted. Lua, agent-tool, and
    /// execution-request surfaces must all enter here: there is no
    /// bypass path around this seam.
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_fs(
        &mut self,
        stack: &crate::effective::EffectiveStack,
        request: &crate::effective::AgentRequest,
        project_trusted: bool,
        level: crate::trust_levels::TrustLevel,
        role: crate::roles::AgentRole,
        scope_patterns: &[String],
        path: &str,
        is_write: bool,
        content: Option<&str>,
        now_ms: u64,
    ) -> Result<crate::fs_authz::FsAuthorized, PluginError> {
        let kind = if is_write {
            crate::effective::RequestKind::FsWrite
        } else {
            crate::effective::RequestKind::FsRead
        };
        if let Err(denial) =
            self.authorize_effective(stack, request, kind, project_trusted, level, role)
        {
            // Capability denial before the FS layers are contacted: record
            // a scope-denial marker in the FS ledger (path only) so every
            // FS decision audits exactly once per ledger.
            let evaluated = crate::fs_authz::FsAuthorized {
                decision: crate::fs_authz::FsDecision::Deny {
                    kind: crate::fs_authz::FsDenialKind::OutsideScope,
                    path: path.to_string(),
                },
                sensitive: self.fs_policy.is_sensitive(path),
                secret_shaped: false,
            };
            let _ = denial;
            self.fs_audit.push_decision(&evaluated, is_write);
            return Err(PluginError::registry(format!(
                "fs denied (outside-scope) '{}'",
                evaluated.decision.path()
            )));
        }
        let scope = match crate::fs_authz::FilesystemScope::from_patterns(scope_patterns) {
            Ok(scope) => scope,
            Err(error) => {
                // Hostile/invalid grant patterns fail the whole request
                // closed *and* audit: the construction refusal is itself a
                // decision, so it cannot return before the FS ledger push
                // (the "every decision appends" contract).
                let evaluated = crate::fs_authz::FsAuthorized {
                    decision: crate::fs_authz::FsDecision::Deny {
                        kind: error.denial_kind(),
                        path: path.to_string(),
                    },
                    sensitive: true,
                    secret_shaped: false,
                };
                self.fs_audit.push_decision(&evaluated, is_write);
                return Err(PluginError::from(error));
            }
        };
        let evaluated =
            crate::fs_authz::authorize_fs(&scope, &self.fs_policy, path, is_write, content, now_ms);
        self.fs_audit.push_decision(&evaluated, is_write);
        match &evaluated.decision {
            crate::fs_authz::FsDecision::Allow { .. }
            | crate::fs_authz::FsDecision::Redacted { .. } => Ok(evaluated),
            crate::fs_authz::FsDecision::ConsentRequired { path } => Err(PluginError::registry(
                format!("fs consent required for '{path}'"),
            )),
            crate::fs_authz::FsDecision::Deny { kind, path } => Err(PluginError::registry(
                format!("fs denied ({kind}) '{path}'"),
            )),
        }
    }

    /// Scrub read bytes against the live secret store (P0-AC-026).
    ///
    /// Redacted decisions must serve these bytes — never the raw content —
    /// before any agent-visible boundary (agent context, Lua logs,
    /// execution logs, panel history, traces, diagnostics).
    #[must_use]
    pub fn scrub_fs_content(&self, content: &str) -> String {
        crate::secrets::scrub_against_store(content, &self.secrets)
    }

    // ── host secret store (research 045 §5; CTX-0521) ────────────────────

    /// Access the host secret store (read-only).
    ///
    /// Values never leave through this reference: use
    /// [`Self::resolve_secret_for_spawn`] at spawn/request time, or the
    /// redacting views ([`crate::secrets::SecretDescriptor`],
    /// [`crate::secrets::SanitizedEnvView`]) for doctor/list surfaces.
    #[must_use]
    pub fn secrets(&self) -> &crate::secrets::SecretStore {
        &self.secrets
    }

    /// Access the host secret store mutably (provisioning, consent, tests).
    #[must_use]
    pub fn secrets_mut(&mut self) -> &mut crate::secrets::SecretStore {
        &mut self.secrets
    }

    /// Resolve `(env_name, handle)` bindings into child-env entries.
    ///
    /// Composition seam (conforms, never duplicates): `stack`/`request` are
    /// authorized first through the CTX-0524 six-layer intersection
    /// ([`Self::authorize_effective`] with `RequestKind::ExecutionRun`); only
    /// an allowed request reaches the store. Resolution then injects values
    /// only into the returned child-env pairs (never argv); the values must
    /// never enter agent context, prompts, Lua logs, execution logs, panel
    /// history, traces, diagnostics, or error messages.
    ///
    /// # Errors
    ///
    /// - [`crate::effective::EffectiveDenial`] (as `PluginError`) when the
    ///   capability stack denies the spawn.
    /// - [`crate::secrets::SecretError::MissingHandle`] /
    ///   [`crate::secrets::SecretError::ConsentRequired`] (as `PluginError`)
    ///   when a handle is unknown or lacks active per-handle consent.
    /// - [`crate::secrets::SecretError`] for malformed/over-bound bindings.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_secret_for_spawn(
        &mut self,
        stack: &crate::effective::EffectiveStack,
        request: &crate::effective::AgentRequest,
        project_trusted: bool,
        level: crate::trust_levels::TrustLevel,
        role: crate::roles::AgentRole,
        bindings: &[(String, crate::secrets::SecretHandle)],
        now_ms: u64,
    ) -> Result<Vec<(String, String)>, PluginError> {
        if let Err(denial) = self.authorize_effective(
            stack,
            request,
            crate::effective::RequestKind::ExecutionRun,
            project_trusted,
            level,
            role,
        ) {
            // Authorization denied before the store is contacted: resolve in
            // unauthorized mode so failures stay typed but leave no
            // secret-audit trace (the effective ledger records the denial).
            let _ = self
                .secrets
                .resolve_env_for_spawn_authorized(bindings, now_ms, false);
            return Err(PluginError::registry(denial.to_string()));
        }
        self.secrets
            .resolve_env_for_spawn(bindings, now_ms)
            .map_err(PluginError::from)
    }

    /// Resolve `(env_name, handle)` bindings with a secret-tier consent gate
    /// (CTX-0330).
    ///
    /// The tier consent half runs first
    /// ([`crate::secret_tiers::check_tier_access`]): tiers whose policy
    /// requires an explicit grant fail closed here when `tier_consent` is
    /// false, and the denial is recorded in the secret audit ledger (names
    /// only) via [`crate::secrets::SecretStore::audit_tier_deny`] before the
    /// store is contacted. `HostEnv`-tier reads pass the gate — their
    /// per-key allowlist is enforced at the `bitty.env` boundary — and then
    /// follow the same capability authorization and per-handle consent path
    /// as [`Self::resolve_secret_for_spawn`]. Values flow only into the
    /// returned child-env pairs, never into diagnostics.
    ///
    /// # Errors
    ///
    /// [`PluginError::Grant`] when the tier gate denies; otherwise the same
    /// errors as [`Self::resolve_secret_for_spawn`].
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_secret_with_tier(
        &mut self,
        stack: &crate::effective::EffectiveStack,
        request: &crate::effective::AgentRequest,
        project_trusted: bool,
        level: crate::trust_levels::TrustLevel,
        role: crate::roles::AgentRole,
        bindings: &[(String, crate::secrets::SecretHandle)],
        now_ms: u64,
        access: crate::secret_tiers::TierAccess,
    ) -> Result<Vec<(String, String)>, PluginError> {
        if let Err(error) = access.check() {
            let names: Vec<String> = bindings
                .iter()
                .map(|(_, handle)| handle.name().to_string())
                .collect();
            self.secrets.audit_tier_deny(access.tier, &names);
            return Err(error);
        }
        self.resolve_secret_for_spawn(
            stack,
            request,
            project_trusted,
            level,
            role,
            bindings,
            now_ms,
        )
    }

    /// Agent-visible sanitized view over explicit env entries.
    ///
    /// Presence plus non-secret values only (panel-environment Agent View
    /// direction): secret values surface as presence markers, never raw.
    #[must_use]
    pub fn sanitized_env_view(&self, env: &[(String, String)]) -> crate::secrets::SanitizedEnvView {
        crate::secrets::SanitizedEnvView::sanitize(env, &self.secrets)
    }

    /// Scrub text against the live store's values (P0-AC-026).
    ///
    /// Handle references survive; every stored value is replaced with
    /// `[redacted]`. Use before logs, diagnostics, traces, and snapshots.
    #[must_use]
    pub fn scrub_against_secrets(&self, input: &str) -> String {
        crate::secrets::scrub_against_store(input, &self.secrets)
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
    ///
    /// Admission is fail-closed at the `EventPipeline` boundary: per-plugin
    /// `1024`/`256 KiB` and global `8192`/`2 MiB` aggregates are enforced via
    /// the shared `DropPolicy` (same as `EventPipeline::publish`). Global
    /// `invariant_global_bounds` is strict after this call.
    pub fn publish(&mut self, event: Event) {
        self.pipeline.publish(event);
    }

    /// Publish to a specific subscriber (lifecycle).
    ///
    /// Admission is fail-closed at the `EventPipeline` boundary with the same
    /// per-plugin/global enforcement as `publish`.
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

    /// Total queued events across all queues (global, RC-5 global limit 8192).
    #[must_use]
    pub fn total_queued_events(&self) -> usize {
        self.pipeline.total_queued_events()
    }

    /// Total queued payload bytes across all queues (global, RC-5 global limit 2 MiB).
    #[must_use]
    pub fn total_queued_bytes(&self) -> usize {
        self.pipeline.total_queued_bytes()
    }

    /// Queued events for one plugin (aggregate, RC-5 per-plugin limit 1024).
    #[must_use]
    pub fn queued_events_for_plugin(&self, plugin_id: &PluginId) -> usize {
        self.pipeline.queued_events_for_plugin(plugin_id.as_str())
    }

    /// Queued payload bytes for one plugin (RC-5 per-plugin limit 256 KiB).
    #[must_use]
    pub fn queued_bytes_for_plugin(&self, plugin_id: &PluginId) -> usize {
        self.pipeline.queued_bytes_for_plugin(plugin_id.as_str())
    }

    /// Total dropped events across all queues (attributed, for `bitty plugin doctor`).
    #[must_use]
    pub fn total_dropped(&self) -> u64 {
        self.pipeline.total_dropped()
    }

    /// Per-queue dropped counts `(plugin_id, event_kind) -> dropped`.
    #[must_use]
    pub fn dropped_per_queue(&self) -> std::collections::BTreeMap<(String, String), u64> {
        self.pipeline.dropped_per_queue()
    }

    /// Total `publish` / `publish_to` calls observed (perf counter).
    #[must_use]
    pub fn publish_count(&self) -> u64 {
        self.pipeline.publish_count()
    }

    /// Headless budget adherence snapshot (perf counters for `tests/measurement.rs`).
    #[must_use]
    pub fn budget_snapshot(&self) -> BudgetSnapshot {
        self.pipeline.budget_snapshot()
    }

    /// Whether per-subscription, per-plugin, and global invariants all hold.
    #[must_use]
    pub fn invariant_queue_bounds(&self) -> bool {
        self.pipeline.invariant_queue_bounds()
    }

    /// Whether global bounds hold (strict, fail-closed, RC-5 global 8192 / 2 MiB).
    ///
    /// Enforced at the `publish` / `publish_to` admission boundary via
    /// `DropPolicy`; always holds after any publish (fail-closed).
    #[must_use]
    pub fn invariant_global_bounds(&self) -> bool {
        self.pipeline.invariant_global_bounds()
    }

    /// Per-subscription queue limit (64).
    #[must_use]
    pub const fn per_subscription_limit(&self) -> usize {
        PER_SUBSCRIPTION_QUEUE_LIMIT
    }

    /// Per-plugin queued event limit (1024).
    #[must_use]
    pub const fn per_plugin_event_limit(&self) -> usize {
        PER_PLUGIN_QUEUED_EVENT_LIMIT
    }

    /// Per-plugin queued bytes limit (256 KiB).
    #[must_use]
    pub const fn per_plugin_bytes_limit(&self) -> usize {
        PER_PLUGIN_QUEUED_BYTES_LIMIT
    }

    /// Global queued event limit (8192).
    #[must_use]
    pub const fn global_event_limit(&self) -> usize {
        GLOBAL_QUEUED_EVENT_LIMIT
    }

    /// Global queued bytes limit (2 MiB).
    #[must_use]
    pub const fn global_bytes_limit(&self) -> usize {
        GLOBAL_QUEUED_BYTES_LIMIT
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
    use crate::grant::GrantRecord;
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
            required_services: Vec::new(),
            capabilities: CapabilityRequests::default(),
            tools: Vec::new(),
            lazy: LazyTriggers {
                commands: Vec::new(),
                events: events.into_iter().map(|s| s.to_string()).collect(),
                claims: Vec::new(),
            },
            raw_bytes_len: 256,
        }
    }

    fn manifest_with_caps(id: &str, caps: Vec<&str>) -> PluginManifest {
        let mut m = minimal_manifest(id, vec![]);
        for c in caps {
            m.capabilities.ids.insert(CapabilityId::parse(c).unwrap());
        }
        m
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
    fn host_remove_purges_identity_allowing_redeclare() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let id = PluginId::new("xuepoo.test").unwrap();
        host.declare(minimal_manifest("xuepoo.test", vec![]))
            .unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        host.activate(&id).unwrap();

        host.remove(&id).unwrap();

        assert!(
            host.registry().get(&id).is_none(),
            "identity must be purged after remove"
        );
        // The same id can be declared and driven through the lifecycle again.
        host.declare(minimal_manifest("xuepoo.test", vec![]))
            .unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        assert!(host.activate(&id).is_ok());
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
    fn host_activate_gate_requires_grant_and_hash() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = manifest_with_caps("xuepoo.gated", vec!["terminal.semantic-read"]);
        // Lifecycle to Registered without grants.
        host.declare(m.clone()).unwrap();
        host.resolve(&PluginId::new("xuepoo.gated").unwrap())
            .unwrap();
        host.register(&PluginId::new("xuepoo.gated").unwrap())
            .unwrap();
        // Activate without grant must fail-closed.
        assert!(
            host.activate(&PluginId::new("xuepoo.gated").unwrap())
                .is_err()
        );
        // Insert correct grant.
        let hash = m.manifest_hash();
        let mut granted = std::collections::BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        host.insert_grant(GrantRecord::granted(
            PluginId::new("xuepoo.gated").unwrap(),
            hash.clone(),
            granted,
            1,
        ));
        assert!(
            host.activate(&PluginId::new("xuepoo.gated").unwrap())
                .is_ok()
        );
        assert_eq!(
            host.registry()
                .get(&PluginId::new("xuepoo.gated").unwrap())
                .unwrap()
                .state,
            crate::registry::PluginState::Activated
        );
        // Hash mismatch should be rejected (new manifest version).
        let mut host2 = PluginHost::new(DropPolicy::DropOldest, 8);
        let mut m2 = manifest_with_caps("xuepoo.gated2", vec!["terminal.semantic-read"]);
        m2.identity.version = "0.2.0".to_string();
        host2.declare(m2.clone()).unwrap();
        host2
            .resolve(&PluginId::new("xuepoo.gated2").unwrap())
            .unwrap();
        host2
            .register(&PluginId::new("xuepoo.gated2").unwrap())
            .unwrap();
        // Grant with old hash.
        let mut granted2 = std::collections::BTreeSet::new();
        granted2.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        host2.insert_grant(GrantRecord::granted(
            PluginId::new("xuepoo.gated2").unwrap(),
            "oldhash".to_string(),
            granted2,
            1,
        ));
        assert!(
            host2
                .activate(&PluginId::new("xuepoo.gated2").unwrap())
                .is_err()
        );
    }

    #[test]
    fn host_activate_no_caps_needs_no_grant() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = minimal_manifest("xuepoo.nocap", vec![]);
        host.declare(m).unwrap();
        host.resolve(&PluginId::new("xuepoo.nocap").unwrap())
            .unwrap();
        host.register(&PluginId::new("xuepoo.nocap").unwrap())
            .unwrap();
        assert!(
            host.activate(&PluginId::new("xuepoo.nocap").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn host_grant_check_requires_current_manifest_declaration() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = manifest_with_caps("xuepoo.declared", vec!["terminal.semantic-read"]);
        let id = PluginId::new("xuepoo.declared").unwrap();
        let hash = m.manifest_hash();
        host.declare(m).unwrap();
        host.insert_grant(GrantRecord::granted(
            id.clone(),
            hash.clone(),
            [CapabilityId::parse("clipboard.read").unwrap()]
                .into_iter()
                .collect(),
            1,
        ));

        assert!(!host.is_granted(&id, &hash, &CapabilityId::parse("clipboard.read").unwrap()));
        assert!(!host.is_granted(
            &id,
            &hash,
            &CapabilityId::parse("terminal.semantic-read").unwrap()
        ));
    }

    #[test]
    fn host_activate_missing_one_cap_fails() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let mut m = minimal_manifest("xuepoo.partial", vec![]);
        m.capabilities
            .ids
            .insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        m.capabilities
            .ids
            .insert(CapabilityId::parse("ui.rich").unwrap());
        host.declare(m.clone()).unwrap();
        host.resolve(&PluginId::new("xuepoo.partial").unwrap())
            .unwrap();
        host.register(&PluginId::new("xuepoo.partial").unwrap())
            .unwrap();
        let hash = m.manifest_hash();
        let mut granted = std::collections::BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        // missing ui.rich
        host.insert_grant(GrantRecord::granted(
            PluginId::new("xuepoo.partial").unwrap(),
            hash,
            granted,
            1,
        ));
        assert!(
            host.activate(&PluginId::new("xuepoo.partial").unwrap())
                .is_err()
        );
    }

    #[test]
    fn host_activate_unchecked_bypasses_gate() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = manifest_with_caps("xuepoo.bypass", vec!["terminal.semantic-read"]);
        host.declare(m).unwrap();
        host.resolve(&PluginId::new("xuepoo.bypass").unwrap())
            .unwrap();
        host.register(&PluginId::new("xuepoo.bypass").unwrap())
            .unwrap();
        // Unchecked succeeds without grant.
        assert!(
            host.activate_unchecked_for_test(&PluginId::new("xuepoo.bypass").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn host_activate_filesystem_caps_expanded() {
        use crate::manifest::{FilesystemRequest, FsAccess};
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let mut m = minimal_manifest("xuepoo.fs", vec![]);
        m.capabilities.filesystem.push(FilesystemRequest {
            access: FsAccess::Read,
            paths: vec!["~/docs/*.md".to_string()],
        });
        host.declare(m.clone()).unwrap();
        host.resolve(&PluginId::new("xuepoo.fs").unwrap()).unwrap();
        host.register(&PluginId::new("xuepoo.fs").unwrap()).unwrap();
        // Without fs.read grant, activate fails.
        assert!(host.activate(&PluginId::new("xuepoo.fs").unwrap()).is_err());
        let hash = m.manifest_hash();
        let mut granted = std::collections::BTreeSet::new();
        granted.insert(CapabilityId::parse("fs.read:~/docs/*.md").unwrap());
        host.insert_grant(GrantRecord::granted(
            PluginId::new("xuepoo.fs").unwrap(),
            hash,
            granted,
            1,
        ));
        assert!(host.activate(&PluginId::new("xuepoo.fs").unwrap()).is_ok());
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

    #[test]
    fn dispose_reclaims_generation_queues() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = minimal_manifest("xuepoo.reclaim", vec!["terminal.bell"]);
        let id = m.identity.id.clone();
        host.declare(m).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        host.subscribe(&id, EventKind::TerminalBell).unwrap();
        host.publish_to(
            &id,
            Event::new(EventKind::TerminalBell, EventPayload::Empty, 1),
        )
        .unwrap();
        assert_eq!(host.total_queued_events(), 1);
        host.dispose(&id).unwrap();
        assert_eq!(host.total_queued_events(), 0);
        assert_eq!(host.pipeline().queue_count(), 0);
        assert!(host.invariant_queue_bounds());
    }

    #[test]
    fn failed_reload_preserves_current_generation_and_queues() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = minimal_manifest("xuepoo.atomic", vec!["terminal.bell"]);
        let id = m.identity.id.clone();
        host.declare(m.clone()).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        host.subscribe(&id, EventKind::TerminalBell).unwrap();
        host.publish_to(
            &id,
            Event::new(EventKind::TerminalBell, EventPayload::Empty, 7),
        )
        .unwrap();

        let mut invalid = minimal_manifest("xuepoo.atomic", vec!["terminal.bell"]);
        invalid.raw_bytes_len = crate::manifest::MANIFEST_MAX_BYTES + 1;
        let err = host.reload(&id, invalid).unwrap_err();
        assert!(format!("{err}").contains("raw_bytes_len") || format!("{err}").contains("limit"));
        assert_eq!(host.registry().get(&id).unwrap().generation, 1);
        assert_eq!(host.total_queued_events(), 1);
        assert_eq!(
            host.drain(&id, &EventKind::TerminalBell).unwrap()[0].sequence,
            7
        );
    }

    #[test]
    fn successful_reload_reclaims_old_queues_before_new_subscription() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let m = minimal_manifest("xuepoo.reload", vec!["terminal.bell"]);
        let id = m.identity.id.clone();
        host.declare(m).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        host.subscribe(&id, EventKind::TerminalBell).unwrap();
        host.publish_to(
            &id,
            Event::new(EventKind::TerminalBell, EventPayload::Empty, 11),
        )
        .unwrap();
        assert_eq!(host.queued_events_for_plugin(&id), 1);

        let generation = host
            .reload(
                &id,
                minimal_manifest("xuepoo.reload", vec!["terminal.bell"]),
            )
            .unwrap();
        assert_eq!(generation, 2);
        assert_eq!(host.total_queued_events(), 0);
        assert_eq!(host.pipeline().queue_count(), 0);
        // Reload keeps the replacement event declaration, but subscriptions are
        // generation-owned and must be explicitly recreated by the host.
        assert_eq!(
            host.registry().get(&id).unwrap().subscribed_events,
            vec!["terminal.bell".to_string()]
        );
        assert!(host.subscribe(&id, EventKind::TerminalBell).is_ok());
        assert_eq!(host.pipeline().queue_count(), 1);
        assert!(host.invariant_queue_bounds());
    }

    #[test]
    fn reload_rejects_identity_mismatch_without_mutation() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let manifest = minimal_manifest("xuepoo.identity", vec![]);
        let id = manifest.identity.id.clone();
        host.declare(manifest).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();

        let replacement = minimal_manifest("xuepoo.other", vec![]);
        assert!(
            host.reload(&id, replacement)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert_eq!(host.registry().get(&id).unwrap().generation, 1);
        assert_eq!(
            host.registry().get(&id).unwrap().state,
            PluginState::Registered
        );
    }

    #[test]
    fn reload_rejects_new_or_undeclared_capability_before_commit() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let current = manifest_with_caps("xuepoo.capreload", vec!["terminal.semantic-read"]);
        let id = current.identity.id.clone();
        host.declare(current.clone()).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();

        let replacement = manifest_with_caps("xuepoo.capreload", vec!["ui.rich"]);
        let hash = replacement.manifest_hash();
        let mut grants = std::collections::BTreeSet::new();
        grants.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        host.insert_grant(GrantRecord::granted(id.clone(), hash, grants, 1));

        assert!(
            host.reload(&id, replacement)
                .unwrap_err()
                .to_string()
                .contains("missing grants")
        );
        assert_eq!(host.registry().get(&id).unwrap().generation, 1);
        assert_eq!(host.registry().get(&id).unwrap().manifest, current);
    }

    #[test]
    fn reload_rejects_hash_and_denial_before_commit() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let current = manifest_with_caps("xuepoo.grantreload", vec!["ui.rich"]);
        let id = current.identity.id.clone();
        host.declare(current).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        let replacement = manifest_with_caps("xuepoo.grantreload", vec!["ui.rich"]);
        host.insert_grant(GrantRecord::granted(
            id.clone(),
            "wrong-hash",
            std::collections::BTreeSet::new(),
            1,
        ));
        assert!(host.reload(&id, replacement.clone()).is_err());
        host.insert_grant(GrantRecord::denied(
            id.clone(),
            replacement.manifest_hash(),
            2,
        ));
        assert!(
            host.reload(&id, replacement)
                .unwrap_err()
                .to_string()
                .contains("denied")
        );
        assert_eq!(host.registry().get(&id).unwrap().generation, 1);
    }

    #[test]
    fn reload_matches_declare_safe_mode_policy() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let manifest = minimal_manifest("xuepoo.safe", vec![]);
        let id = manifest.identity.id.clone();
        host.declare(manifest).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        host.set_safe_mode(true);
        assert!(
            host.reload(&id, minimal_manifest("xuepoo.safe", vec![]))
                .unwrap_err()
                .to_string()
                .contains("safe mode")
        );
        assert_eq!(host.registry().get(&id).unwrap().generation, 1);
    }
}

#[cfg(test)]
mod effective_tests {
    use super::*;
    use crate::effective::{
        AgentRequest, CapabilityScope, DenialKind, EffectiveStack, RequestKind,
    };
    use crate::roles::AgentRole;
    use crate::trust_levels::TrustLevel;

    fn scope_with(source: &str, caps: &[&str]) -> CapabilityScope {
        let mut scope = CapabilityScope::unconstrained(source);
        for raw in caps {
            scope.caps.insert(CapabilityId::parse(raw).unwrap());
        }
        scope
    }

    #[test]
    fn host_authorize_effective_audits_allow_and_deny() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let stack = EffectiveStack {
            host: scope_with("host", &["terminal.semantic-read"]),
            user: scope_with("user", &["terminal.semantic-read"]),
            project: None,
            parent: scope_with("parent", &["terminal.semantic-read"]),
            task: scope_with("task", &["terminal.semantic-read"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["terminal.semantic-read"]),
            raw_wide: Vec::new(),
        };
        host.authorize_effective(
            &stack,
            &request,
            RequestKind::PluginLifecycle,
            true,
            TrustLevel::Core,
            AgentRole::Commander,
        )
        .unwrap();
        let wide = AgentRequest {
            scope: CapabilityScope::unconstrained("lua"),
            raw_wide: vec!["filesystem=all".to_string()],
        };
        let denial = host
            .authorize_effective(
                &stack,
                &wide,
                RequestKind::AgentSpawn,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
            )
            .unwrap_err();
        assert_eq!(denial.kind, DenialKind::SelfGrant);
        assert_eq!(host.audit().len(), 2);
    }

    #[test]
    fn host_authorize_effective_denies_distrusted_level() {
        // #1314: every layer grants `fs.read`, so bare `authorize` would
        // allow — but the external-tool level admits no filesystem domain,
        // so the host seam must deny fail-closed and audit the denial.
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/docs/*.md"]),
            user: scope_with("user", &["fs.read:~/docs/*.md"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/docs/*.md"]),
            task: scope_with("task", &["fs.read:~/docs/*.md"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/docs/*.md"]),
            raw_wide: Vec::new(),
        };
        let denial = host
            .authorize_effective(
                &stack,
                &request,
                RequestKind::FsRead,
                true,
                TrustLevel::ExternalTool,
                AgentRole::Commander,
            )
            .expect_err("external tools admit no filesystem domain");
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
        assert_eq!(denial.request_kind, RequestKind::FsRead);
        let chain = denial.reason_chain().join("\n");
        assert!(chain.contains("external-tool"), "{chain}");
        assert_eq!(host.audit().len(), 1);
        assert_eq!(
            host.audit().iter().last().expect("audit entry").decision,
            crate::effective::AuditDecision::Deny
        );
    }

    #[test]
    fn host_authorize_effective_denies_unauthorized_role() {
        // #1314: grants allow `fs.read` and core admits the filesystem
        // domain, but reviewers invoke no tools — the role gate must deny
        // fail-closed on the host path too.
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/docs/*.md"]),
            user: scope_with("user", &["fs.read:~/docs/*.md"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/docs/*.md"]),
            task: scope_with("task", &["fs.read:~/docs/*.md"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/docs/*.md"]),
            raw_wide: Vec::new(),
        };
        let denial = host
            .authorize_effective(
                &stack,
                &request,
                RequestKind::FsRead,
                true,
                TrustLevel::Core,
                AgentRole::Reviewer,
            )
            .expect_err("reviewers invoke no tools");
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
        assert_eq!(denial.request_kind, RequestKind::FsRead);
        let chain = denial.reason_chain().join("\n");
        assert!(chain.contains("reviewer"), "{chain}");
        assert!(chain.contains("tool-call"), "{chain}");
        assert_eq!(host.audit().len(), 1);
        assert_eq!(
            host.audit().iter().last().expect("audit entry").decision,
            crate::effective::AuditDecision::Deny
        );
    }

    #[test]
    fn host_delegate_effective_narrows_only() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let parent = crate::effective::EffectiveCapability {
            caps: [CapabilityId::parse("terminal.semantic-read").unwrap()]
                .into_iter()
                .collect(),
            max_agents: 3,
            allow_root: false,
        };
        let narrow = AgentRequest {
            scope: scope_with("child", &["terminal.semantic-read"]),
            raw_wide: Vec::new(),
        };
        let child = host
            .delegate_effective(&parent, &narrow, RequestKind::PluginLifecycle)
            .unwrap();
        assert!(child.is_subset_of(&parent));
        let wide = AgentRequest {
            scope: scope_with("child", &["terminal.semantic-read", "ui.rich"]),
            raw_wide: Vec::new(),
        };
        assert!(
            host.delegate_effective(&parent, &wide, RequestKind::PluginLifecycle)
                .is_err()
        );
        assert_eq!(host.audit().len(), 2);
    }

    #[test]
    fn host_authorize_fs_denies_sensitive_without_consent() {
        // 045 §4: the FS authorization path evaluates capability
        // intersection first, then scope + sensitive policy + content.
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/projects/**"]),
            user: scope_with("user", &["fs.read:~/projects/**"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/projects/**"]),
            task: scope_with("task", &["fs.read:~/projects/**"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/projects/**"]),
            raw_wide: Vec::new(),
        };
        let patterns = vec!["~/projects/**".to_string()];
        // Legit read allows and audits.
        let allowed = host
            .authorize_fs(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &patterns,
                "~/projects/notes.md",
                false,
                None,
                0,
            )
            .unwrap();
        assert!(allowed.is_authorized());
        // Sensitive `.env` read requires consent (typed, path only).
        let err = host
            .authorize_fs(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &patterns,
                "~/projects/.env",
                false,
                None,
                0,
            )
            .unwrap_err();
        assert!(err.to_string().contains("consent required"));
        // Path-only diagnostic: the normalized key folds case/separators,
        // so assert the sensitive marker, never a value.
        assert!(err.to_string().contains("projects"));
        // Explicit user consent clears exactly that path.
        host.grant_fs_consent("~/projects/.env", 0, None).unwrap();
        let consented = host
            .authorize_fs(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &patterns,
                "~/projects/.env",
                false,
                None,
                0,
            )
            .unwrap();
        assert!(consented.is_authorized());
        // Secret-shaped content redacts; the scrubbed view leaks nothing.
        let seed = "ghp_seededFsHostFixtureAAAA1111";
        let secret_content = format!("token={seed}");
        let redacted = host
            .authorize_fs(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &patterns,
                "~/projects/secret.txt",
                false,
                Some(&secret_content),
                0,
            )
            .unwrap();
        assert!(matches!(
            redacted.decision,
            crate::fs_authz::FsDecision::Redacted { .. }
        ));
        // Every FS decision audited (allow + consent-required + consent +
        // allow + redacted).
        assert_eq!(host.fs_audit().len(), 5);
        // Capability denial audits too and never reaches the FS layers.
        let bad = AgentRequest {
            scope: scope_with("req", &["ui.rich"]),
            raw_wide: Vec::new(),
        };
        let err = host
            .authorize_fs(
                &stack,
                &bad,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &patterns,
                "~/projects/a.md",
                false,
                None,
                0,
            )
            .unwrap_err();
        assert!(err.to_string().contains("denied"));
        assert_eq!(host.fs_audit().len(), 6);
    }

    #[test]
    fn host_authorize_fs_audits_hostile_scope_pattern_refusal() {
        // A hostile grant pattern refuses the request at scope
        // construction; the refusal must still append to the FS ledger
        // (every decision audits).
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/projects/**"]),
            user: scope_with("user", &["fs.read:~/projects/**"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/projects/**"]),
            task: scope_with("task", &["fs.read:~/projects/**"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/projects/**"]),
            raw_wide: Vec::new(),
        };
        let hostile = vec!["~/.ssh/**".to_string()];
        let before = host.fs_audit().len();
        let err = host
            .authorize_fs(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &hostile,
                "~/projects/notes.md",
                false,
                None,
                0,
            )
            .unwrap_err();
        assert!(err.to_string().contains("hostile"));
        assert_eq!(host.fs_audit().len(), before + 1);
        let entry = host.fs_audit().iter().last().expect("audit entry");
        assert_eq!(entry.decision, crate::fs_authz::FsAuditDecision::Deny);
        assert_eq!(
            entry.denial,
            Some(crate::fs_authz::FsDenialKind::HostilePattern)
        );
    }

    #[test]
    fn host_authorize_fs_no_bypass_from_plugin_or_agent_request() {
        // Lua plugins and agent surfaces share this single seam: hostile
        // spellings and out-of-scope paths deny identically regardless of
        // which surface supplied the request voice.
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/projects/**"]),
            user: scope_with("user", &["fs.read:~/projects/**"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/projects/**"]),
            task: scope_with("task", &["fs.read:~/projects/**"]),
        };
        let lua_request = AgentRequest {
            scope: scope_with("lua", &["fs.read:~/projects/**"]),
            raw_wide: Vec::new(),
        };
        let agent_request = AgentRequest {
            scope: scope_with("agent", &["fs.read:~/projects/**"]),
            raw_wide: Vec::new(),
        };
        let patterns = vec!["~/projects/**".to_string()];
        for request in [&lua_request, &agent_request] {
            for path in [
                "~/projects/./.env",
                "~/PROJECTS/.ENV",
                "~/projects/../../etc/passwd",
                "/etc/passwd",
            ] {
                assert!(
                    host.authorize_fs(
                        &stack,
                        request,
                        true,
                        TrustLevel::Core,
                        AgentRole::Commander,
                        &patterns,
                        path,
                        false,
                        None,
                        0
                    )
                    .is_err(),
                    "surface request for {path:?} must not bypass"
                );
            }
            // Backslash/case bypass spellings of the scope itself still
            // resolve inside the grant (Windows parity) — but the
            // sensitive layer still fires for `.env`.
            let err = host
                .authorize_fs(
                    &stack,
                    request,
                    true,
                    TrustLevel::Core,
                    AgentRole::Commander,
                    &patterns,
                    "~\\PROJECTS\\.env",
                    false,
                    None,
                    0,
                )
                .unwrap_err();
            assert!(err.to_string().contains("consent required"));
        }
    }
}

#[cfg(test)]
mod secrets_tests {
    use super::*;
    use crate::effective::{AgentRequest, CapabilityScope, EffectiveStack};
    use crate::roles::AgentRole;
    use crate::secrets::{SecretError, SecretHandle};
    use crate::trust_levels::TrustLevel;

    const SEED: &str = "ghp_seededSecretFixtureAAAA1111";

    fn scope_with(source: &str, caps: &[&str]) -> CapabilityScope {
        let mut scope = CapabilityScope::unconstrained(source);
        for raw in caps {
            scope.caps.insert(CapabilityId::parse(raw).unwrap());
        }
        scope
    }

    fn authorized_stack() -> (EffectiveStack, AgentRequest) {
        let stack = EffectiveStack {
            host: scope_with("host", &["terminal.semantic-read"]),
            user: scope_with("user", &["terminal.semantic-read"]),
            project: None,
            parent: scope_with("parent", &["terminal.semantic-read"]),
            task: scope_with("task", &["terminal.semantic-read"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["terminal.semantic-read"]),
            raw_wide: Vec::new(),
        };
        (stack, request)
    }

    fn denied_request() -> AgentRequest {
        AgentRequest {
            scope: scope_with("req", &["ui.rich"]),
            raw_wide: Vec::new(),
        }
    }

    #[test]
    fn secret_spawn_requires_capability_authorization_first() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        host.secrets_mut().insert("github", SEED).unwrap();
        let secret_audit_before = host.secrets().audit().len();
        let bindings = vec![(
            "GITHUB_TOKEN".to_string(),
            SecretHandle::parse("secret://github").unwrap(),
        )];
        // An over-request against the intersection denies before the store
        // is contacted: no resolution, no new secret audit entry (the
        // consent grant above is the only entry so far).
        let denied = host
            .resolve_secret_for_spawn(
                &EffectiveStack::unconstrained(),
                &denied_request(),
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &bindings,
                10,
            )
            .unwrap_err();
        assert!(denied.to_string().contains("denied"));
        assert_eq!(host.secrets().audit().len(), secret_audit_before);
        // Authorized stack resolves into child env only.
        host.secrets_mut().grant_consent("github", 0, None);
        let allowed_before = host.secrets().audit().len();
        let (stack, request) = authorized_stack();
        let resolved = host
            .resolve_secret_for_spawn(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &bindings,
                10,
            )
            .unwrap();
        assert_eq!(
            resolved,
            vec![("GITHUB_TOKEN".to_string(), SEED.to_string())]
        );
        assert_eq!(host.secrets().audit().len(), allowed_before + 1);
    }

    #[test]
    fn tiered_secret_resolve_gates_consent_before_store() {
        use crate::secret_tiers::{SecretTier, TierAccess};

        let denied_access = TierAccess {
            tier: SecretTier::OsKeyring,
            consent: false,
        };
        let allowed_access = TierAccess {
            tier: SecretTier::OsKeyring,
            consent: true,
        };
        let host_env_access = TierAccess {
            tier: SecretTier::HostEnv,
            consent: false,
        };
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        host.secrets_mut().insert("github", SEED).unwrap();
        host.secrets_mut().grant_consent("github", 0, None);
        let (stack, request) = authorized_stack();
        let bindings = vec![(
            "GITHUB_TOKEN".to_string(),
            SecretHandle::parse("secret://github").unwrap(),
        )];
        // Grant tiers without tier consent deny before the store is
        // contacted, with a typed grant error and a names-only audit entry.
        let audit_before = host.secrets().audit().len();
        let denied = host
            .resolve_secret_with_tier(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &bindings,
                10,
                denied_access,
            )
            .expect_err("tier without consent must deny");
        assert!(denied.to_string().contains("explicit consent"));
        assert!(!denied.to_string().contains(SEED));
        assert_eq!(host.secrets().audit().len(), audit_before + 1);
        // With tier consent the same bindings resolve through the store path.
        let resolved = host
            .resolve_secret_with_tier(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &bindings,
                10,
                allowed_access,
            )
            .expect("tier with consent resolves");
        assert_eq!(
            resolved,
            vec![("GITHUB_TOKEN".to_string(), SEED.to_string())]
        );
        // Host-env tier needs no tier consent beyond the store path.
        let resolved = host
            .resolve_secret_with_tier(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &bindings,
                10,
                host_env_access,
            )
            .expect("host-env tier passes the gate");
        assert_eq!(
            resolved,
            vec![("GITHUB_TOKEN".to_string(), SEED.to_string())]
        );
    }

    #[test]
    fn secret_spawn_surfaces_typed_missing_and_denied() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        let (stack, request) = authorized_stack();
        // Unknown handle: missing (typed, name only).
        let missing = vec![(
            "GITHUB_TOKEN".to_string(),
            SecretHandle::parse("secret://nope").unwrap(),
        )];
        let err = host
            .resolve_secret_for_spawn(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &missing,
                10,
            )
            .unwrap_err();
        assert!(err.to_string().contains("missing secret handle 'nope'"));
        assert!(!err.to_string().contains(SEED));
        // Known handle without consent: denied (typed, name only).
        host.secrets_mut().insert("github", SEED).unwrap();
        let bindings = vec![(
            "GITHUB_TOKEN".to_string(),
            SecretHandle::parse("secret://github").unwrap(),
        )];
        let err = host
            .resolve_secret_for_spawn(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &bindings,
                10,
            )
            .unwrap_err();
        assert!(err.to_string().contains("consent required"));
        assert!(!err.to_string().contains(SEED));
    }

    #[test]
    fn secret_spawn_never_leaks_through_sanitized_or_scrubbed_views() {
        let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
        host.secrets_mut().insert("github", SEED).unwrap();
        host.secrets_mut().grant_consent("github", 0, None);
        let (stack, request) = authorized_stack();
        let resolved = host
            .resolve_secret_for_spawn(
                &stack,
                &request,
                true,
                TrustLevel::Core,
                AgentRole::Commander,
                &[(
                    "GITHUB_TOKEN".to_string(),
                    SecretHandle::parse("secret://github").unwrap(),
                )],
                10,
            )
            .unwrap();
        // Sanitized agent view withholds the value.
        let view = host.sanitized_env_view(&resolved);
        assert!(view.is_secret("GITHUB_TOKEN"));
        assert!(!format!("{view:?}").contains(SEED));
        // Scrubbing removes the value from mixed text; handles survive.
        let mixed = format!("out {SEED}\nref secret://github end");
        let scrubbed = host.scrub_against_secrets(&mixed);
        assert!(!scrubbed.contains(SEED));
        assert!(scrubbed.contains("secret://github"));
        let _ = SecretError::missing_handle("github");
    }
}
