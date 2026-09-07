//! `Runtime` — Plugin host, grants, interception, and URL activation.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::*;

/// Map a [`ColdEvent`] to a [`HostObservation`] where semantics overlap.
///
/// Only post-state, bounded observations cross the queue; hot-path payloads
/// (bytes, cells, per-frame damage beyond generation) never cross. Returns `None`
/// when there is no direct observation mapping (e.g. `ZoneMarked`, `HyperlinkChanged`).
pub(super) fn cold_to_observation(event: &ColdEvent) -> Option<HostObservation> {
    match event {
        ColdEvent::TitleChanged(s) => Some(HostObservation::TitleChanged(s.clone())),
        ColdEvent::CwdChanged(s) => Some(HostObservation::CwdChanged(s.clone())),
        ColdEvent::Bell => Some(HostObservation::Bell),
        ColdEvent::ModeChanged { mode, enabled } => Some(HostObservation::ModeChanged {
            mode: format!("{mode:?}"),
            enabled: *enabled,
        }),
        ColdEvent::Damage { generation } => Some(HostObservation::Damage {
            generation: *generation,
        }),
        ColdEvent::ZoneMarked(_)
        | ColdEvent::HyperlinkChanged(_)
        | ColdEvent::UnknownSequence(_) => None,
    }
}

impl Runtime {
    /// Owned plugin host (read-only).
    #[must_use]
    pub fn plugin_host(&self) -> &PluginHost {
        &self.plugin_host
    }

    /// Owned plugin host (mutable).
    #[must_use]
    pub fn plugin_host_mut(&mut self) -> &mut PluginHost {
        &mut self.plugin_host
    }

    /// Drop policy for the plugin host's event pipeline (accepted v1 default `DropOldest`, OQ-013 closed).
    #[must_use]
    pub fn plugin_drop_policy(&self) -> DropPolicy {
        self.plugin_host.pipeline().drop_policy()
    }

    /// Per-queue capacity for the plugin pipeline (candidate, `OQ-014`; budget not yet normative).
    #[must_use]
    pub fn plugin_pipeline_capacity(&self) -> usize {
        self.plugin_host.pipeline().default_capacity()
    }

    /// Side-queue capacity for [`HostObservation`] (ADR-0003 rule 4).
    #[must_use]
    pub fn plugin_side_capacity(&self) -> usize {
        self.plugin_host.side_queue().capacity()
    }

    /// Number of queued [`HostObservation`]s in the side queue.
    #[must_use]
    pub fn plugin_side_len(&self) -> usize {
        self.plugin_host.side_queue().len()
    }

    /// How many side-queue observations have been dropped (bounded, counted for `bitty plugin doctor`).
    #[must_use]
    pub fn plugin_side_dropped(&self) -> u64 {
        self.plugin_host.side_queue().dropped()
    }

    /// Total dropped events across all per-subscriber pipeline queues (for `bitty plugin doctor`).
    #[must_use]
    pub fn plugin_total_dropped(&self) -> u64 {
        self.plugin_host.pipeline().total_dropped()
    }

    /// Per-queue dropped counts `(plugin_id, event_kind) -> dropped`.
    #[must_use]
    pub fn plugin_dropped_per_queue(&self) -> std::collections::BTreeMap<(String, String), u64> {
        self.plugin_host.pipeline().dropped_per_queue()
    }

    /// Whether the host is in safe mode (`bitty --safe` skips third-party plugins).
    #[must_use]
    pub fn plugin_safe_mode(&self) -> bool {
        self.plugin_host.is_safe_mode()
    }

    /// Enable or disable safe mode.
    pub fn set_plugin_safe_mode(&mut self, safe: bool) {
        self.plugin_host.set_safe_mode(safe);
    }

    /// Register a plugin from its already-parsed manifest.
    ///
    /// Validates the manifest, inserts as `Declared`, resolves dependencies
    /// against the current registry, and reserves commands/event subscriptions
    /// at graph construction time (duplicate qualified names are rejected here,
    /// not shadowed). On success the entry is `Registered`; caller may then
    /// [`Self::activate_plugin`] or let the lazy loader activate on first
    /// command/event.
    ///
    /// Headless-testable: no file I/O, no VM, no window/GPU.
    pub fn register_plugin(&mut self, manifest: PluginManifest) -> Result<(), RuntimeError> {
        let id = manifest.id().clone();
        self.plugin_host.declare(manifest)?;
        // Resolve may fail if dependencies missing; we keep the `Declared` entry
        // and surface the error for the caller to inspect `plugin_host.registry()`.
        self.plugin_host.resolve(&id)?;
        self.plugin_host.register(&id)?;
        Ok(())
    }

    /// Activate a previously registered plugin (moves `Registered -> Activated`).
    pub fn activate_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.activate(id).map_err(RuntimeError::from)
    }

    /// Suspend a plugin.
    pub fn suspend_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.suspend(id).map_err(RuntimeError::from)
    }

    /// Resume a suspended plugin (`Suspended -> Registered`, caller may `activate` again).
    pub fn resume_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.resume(id).map_err(RuntimeError::from)
    }

    /// Dispose a plugin (releases generation resources).
    pub fn dispose_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.dispose(id).map_err(RuntimeError::from)
    }

    /// Subscribe `plugin_id` to `kind` (requires the event was declared in the manifest).
    pub fn subscribe_plugin_event(
        &mut self,
        plugin_id: &PluginId,
        kind: EventKind,
    ) -> Result<(), RuntimeError> {
        self.plugin_host
            .subscribe(plugin_id, kind)
            .map_err(RuntimeError::from)
    }

    /// Publish an event to all subscribers of its kind (observation/lifecycle).
    ///
    /// Bounded, never blocks the producer; drops are counted per queue under `DropPolicy`.
    pub fn publish_plugin_event(&mut self, event: Event) {
        self.plugin_host.publish(event);
    }

    /// Publish to a specific subscriber (lifecycle, owning plugin only).
    pub fn publish_plugin_event_to(
        &mut self,
        plugin_id: &PluginId,
        event: Event,
    ) -> Result<(), RuntimeError> {
        self.plugin_host
            .publish_to(plugin_id, event)
            .map_err(RuntimeError::from)
    }

    /// Drain a bounded batch for `plugin_id` + `kind` (FIFO, bounded by count/bytes).
    pub fn drain_plugin_events(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<Event>, RuntimeError> {
        self.plugin_host
            .drain_batch(plugin_id, kind, max_events, max_bytes)
            .map_err(RuntimeError::from)
    }

    /// Drain all queued events for `plugin_id` + `kind`.
    pub fn drain_plugin_events_all(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
    ) -> Result<Vec<Event>, RuntimeError> {
        self.plugin_host
            .drain(plugin_id, kind)
            .map_err(RuntimeError::from)
    }

    /// Drain side-queue observations (bounded host-mediated `HostObservation`s).
    pub fn drain_plugin_observations(&mut self) -> Vec<HostObservation> {
        self.plugin_host.drain_observations()
    }

    /// Drain side-queue observations up to `limit` (bounded batch).
    pub fn drain_plugin_observations_bounded(&mut self, limit: usize) -> Vec<HostObservation> {
        self.plugin_host.drain_observations_bounded(limit)
    }

    /// Push a [`HostObservation`] into the side queue (producer never blocks, bounded drops).
    ///
    /// Exposed for headless tests that drive observations without going through `handle_pty_bytes`.
    pub fn push_plugin_observation(&mut self, obs: HostObservation) {
        self.plugin_host.push_observation(obs);
    }

    /// Bridge all currently queued [`ColdEvent`]s into the side queue where a direct
    /// [`HostObservation`] mapping exists. This drains the `ColdQueue` and pushes
    /// corresponding observations without blocking; the side queue's bounded drop
    /// counter increments on overflow (visible via `plugin_side_dropped` for doctor).
    ///
    /// In steady state, `handle_pty_bytes` already bridges overlapping events automatically;
    /// this helper is for callers that have drained or synthesized cold events and want
    /// to observe them through the plugin host side queue headlessly.
    pub fn bridge_cold_to_side_queue(&mut self) {
        let drained = self.cold_queue.drain();
        for ev in drained {
            if let Some(obs) = cold_to_observation(&ev) {
                self.plugin_host.push_observation(obs);
            }
            // Keep the original cold event re-queued? No — draining is consuming.
            // For the cold+side dual accounting mode used by `handle_pty_bytes`,
            // we re-push the cold event so `cold_queue` remains observable.
            // But this drain-into-side is explicit; caller has already drained.
            // To preserve cold observability, we do not re-enqueue here — the
            // caller can decide to handle cold events separately. Documented honestly.
        }
    }

    /// Whether `capability` is granted for `plugin_id` under `manifest_hash`.
    #[must_use]
    pub fn is_capability_granted(
        &self,
        plugin_id: &PluginId,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> bool {
        self.plugin_host
            .is_granted(plugin_id, manifest_hash, capability)
    }

    /// Insert a grant record (headless helper; persistence deferred).
    pub fn insert_grant(&mut self, record: GrantRecord) {
        self.plugin_host.insert_grant(record);
    }

    /// Revoke a capability or all grants for `plugin_id`.
    pub fn revoke_grant(
        &mut self,
        plugin_id: &PluginId,
        capability: Option<&CapabilityId>,
    ) -> Result<bitty_plugin_host::RevokeReport, RuntimeError> {
        self.plugin_host
            .revoke(plugin_id, capability)
            .map_err(RuntimeError::from)
    }

    /// Stub: check whether `plugin_id` may dispatch `command` under `manifest_hash` and `capability`.
    ///
    /// The full dispatch will run the command via the Lua VM with the plugin's grants;
    /// here we only intersect the requested capability with the grant store (deny-by-default,
    /// hash-bound). Returns `Ok(())` when granted, `Err(RuntimeError::Plugin)` otherwise.
    ///
    /// High-risk identifiers (`terminal.raw-read`, `ui.protocol-register`, etc.) are already
    /// flagged by `CapabilityId::is_high_risk`; revocation and workspace narrowing are
    /// enforced by the underlying `GrantStore`.
    pub fn check_command_grant(
        &self,
        plugin_id: &PluginId,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> Result<(), RuntimeError> {
        if self.is_capability_granted(plugin_id, manifest_hash, capability) {
            Ok(())
        } else {
            Err(RuntimeError::Plugin(format!(
                "command dispatch denied: plugin '{}' lacks capability '{capability}' for hash '{manifest_hash}' (deny-by-default)",
                plugin_id.as_str()
            )))
        }
    }

    /// Stub: grant-checked command dispatch.
    ///
    /// Validates that `qualified` is owned by `plugin_id` (via the registry) and that
    /// the required `capability` is granted. On success returns `Ok(())` as a
    /// placeholder for the future VM invocation; actual execution remains deferred.
    pub fn dispatch_command(
        &self,
        plugin_id: &PluginId,
        qualified: &QualifiedName,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> Result<(), RuntimeError> {
        // Qualified name must be owned by this plugin (registry invariant: duplicates rejected at graph construction).
        let entry = self.plugin_host.registry().get(plugin_id).ok_or_else(|| {
            RuntimeError::Plugin(format!("plugin not found: '{}'", plugin_id.as_str()))
        })?;
        if !entry
            .commands
            .iter()
            .any(|c| c.as_str() == qualified.as_str())
        {
            return Err(RuntimeError::Plugin(format!(
                "command '{}' not owned by plugin '{}'",
                qualified.as_str(),
                plugin_id.as_str()
            )));
        }
        self.check_command_grant(plugin_id, manifest_hash, capability)
    }

    /// Accumulate interceptor decisions for a single user action (veto-wins, deterministic).
    ///
    /// This mirrors the RFC fail-open, veto-wins policy: a single `Veto` vetoes
    /// regardless of handler order; otherwise the action proceeds.
    #[must_use]
    pub fn accumulate_interceptions(decisions: &[InterceptionDecision]) -> InterceptionDecision {
        bitty_plugin_host::accumulate_interceptions(decisions)
    }

    /// Whether an intercepted action should proceed (`true`) or be vetoed (`false`) under fail-open.
    ///
    /// Timeouts are treated as abstention: the host proceeds without the plugin, records a
    /// violation, and disables the handler after repeated violations (threshold deferred to `OQ-014`).
    #[must_use]
    pub fn should_proceed_for_intercept(decision: InterceptionDecision, timed_out: bool) -> bool {
        bitty_plugin_host::should_proceed(decision, timed_out)
    }

    /// Convenience wrapper: `accumulate_interceptions` then `should_proceed`.
    #[must_use]
    pub fn should_proceed_after_interceptions(
        decisions: &[InterceptionDecision],
        timed_out: bool,
    ) -> bool {
        let acc = Self::accumulate_interceptions(decisions);
        Self::should_proceed_for_intercept(acc, timed_out)
    }

    /// Interception helper for `intercept.command-dispatch` (v1 of four points).
    ///
    /// Callers collect per-handler [`InterceptionDecision`]s (e.g. from future VM invocations)
    /// and pass them here; the host applies veto-wins and fail-open semantics.
    /// Reentrancy (a handler triggering another interception on the same thread) is rejected
    /// by the caller — nested interception is not defined behavior (RFC).
    #[must_use]
    pub fn intercept_command_dispatch(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    /// `intercept.terminal-spawn` stub — same fail-open, veto-wins policy.
    #[must_use]
    pub fn intercept_terminal_spawn(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    /// `intercept.paste` stub — bounded metadata path (no clipboard text without `clipboard.read`).
    #[must_use]
    pub fn intercept_paste(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    /// `intercept.open-url` stub.
    #[must_use]
    pub fn intercept_open_url(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        if timed_out {
            return false;
        }
        Self::should_proceed_after_interceptions(decisions, false)
    }

    /// Authorizes a non-local URL activation and binds it to the exact URI.
    /// The gesture must have been issued by the runtime's platform-event path;
    /// terminal output and caller-supplied booleans cannot satisfy this gate.
    pub fn authorize_url_activation(
        &mut self,
        uri: &str,
        gesture: ActivationGesture,
        decisions: &[InterceptionDecision],
        timed_out: bool,
    ) -> Result<UrlActivation, bitty_platform::PlatformError> {
        if self.pending_activation_gesture.as_ref() != Some(&gesture)
            || !Self::intercept_open_url(decisions, timed_out)
        {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        self.pending_activation_gesture = None;
        let validated = bitty_platform::validate_url(uri)?;
        if validated.as_str().starts_with("file:") {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        Ok(UrlActivation {
            uri: validated.as_str().to_owned(),
        })
    }

    /// Authorizes a local-file URL through a separate explicit approval path.
    pub fn authorize_file_url_activation(
        &mut self,
        uri: &str,
        gesture: ActivationGesture,
        decisions: &[InterceptionDecision],
        timed_out: bool,
    ) -> Result<FileUrlActivation, bitty_platform::PlatformError> {
        if self.pending_activation_gesture.as_ref() != Some(&gesture)
            || !Self::intercept_open_url(decisions, timed_out)
        {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        self.pending_activation_gesture = None;
        let validated = bitty_platform::validate_file_url(uri)?;
        Ok(FileUrlActivation {
            uri: validated.as_str().to_owned(),
        })
    }

    /// Takes the one-use gesture minted by a real primary mouse activation.
    /// Terminal output and synthetic API calls never mint this proof.
    pub fn take_activation_gesture(&self) -> Option<ActivationGesture> {
        self.pending_activation_gesture.clone()
    }

    /// Opens a URL using only a runtime-issued, URI-bound authorization.
    pub fn open_url(&self, activation: UrlActivation) -> Result<(), bitty_platform::PlatformError> {
        let validated = bitty_platform::validate_url(&activation.uri)?;
        if validated.as_str().starts_with("file:") {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        Self::spawn_validated_url(validated.as_str())
    }

    /// Opens a local-file URL using only its distinct runtime-issued approval.
    pub fn open_file_url(
        &self,
        activation: FileUrlActivation,
    ) -> Result<(), bitty_platform::PlatformError> {
        let validated = bitty_platform::validate_file_url(&activation.uri)?;
        Self::spawn_validated_url(validated.as_str())
    }

    pub(super) fn spawn_validated_url(uri: &str) -> Result<(), bitty_platform::PlatformError> {
        use std::process::Command;
        let (program, prefix) = Self::url_dispatch();
        if cfg!(target_os = "linux") && !Self::handler_available(program) {
            return Err(bitty_platform::PlatformError::UrlLaunch(format!(
                "URL handler is unavailable: {program}"
            )));
        }
        let mut command = Command::new(program);
        if let Some(argument) = prefix {
            command.arg(argument);
        }
        command.arg(uri);
        command
            .spawn()
            .map(|_| ())
            .map_err(|error| bitty_platform::PlatformError::UrlLaunch(error.to_string()))
    }

    pub(super) fn handler_available(program: &str) -> bool {
        std::path::Path::new(program).is_file()
    }

    pub(super) fn url_handler() -> &'static str {
        if cfg!(target_os = "windows") {
            r"C:\Windows\System32\explorer.exe"
        } else if cfg!(target_os = "macos") {
            "/usr/bin/open"
        } else {
            "/usr/bin/gio"
        }
    }

    pub(super) fn url_dispatch() -> (&'static str, Option<&'static str>) {
        if cfg!(target_os = "linux") {
            (Self::url_handler(), Some("open"))
        } else {
            (Self::url_handler(), None)
        }
    }
}
