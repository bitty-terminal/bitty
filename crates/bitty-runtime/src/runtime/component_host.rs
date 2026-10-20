//! Production wiring for installed native components (CTX-1361, bitty#2254).
//!
//! [`Runtime`] owns at most one [`ComponentHost`](crate::component::ComponentHost).
//! The composition root installs it during startup; `None` is fail-closed.
//! Requests are authorized against the accepted plugin capability model
//! (`PluginHost` registry + grant store + manifest `[[network.egress]]`) and
//! then handed to the host, which resolves and digest-verifies the installed
//! component before every spawn.
//!
//! Authorization inputs (`plugin_id`, `manifest_hash`) are caller-supplied and
//! acceptable only because every caller is Core-internal today. The plugin
//! request surface (`bitty.component.*`, CTX-2254) derives both host-side from
//! the generation's `PluginServices` (plugin id and activation-time manifest
//! hash), never from plugin-supplied strings, and submits through
//! [`Runtime::submit_component_request`].

use std::time::Instant;

use bitty_plugin_host::{PluginId, PluginState};

use crate::component::{
    BrokerEvent, ComponentHost, ComponentHostError, ComponentRequest, ComponentResponse,
    ComponentStatus, PluginGrant, RequestId,
};

use super::Runtime;

/// Maximum component events retained between `tick_at` and the embedder's
/// next [`Runtime::poll_component_events`] / [`Runtime::take_component_events`].
pub const COMPONENT_EVENT_QUEUE_CAPACITY: usize = 256;

impl Runtime {
    /// Install the production component host, returning the previous one.
    ///
    /// Installed hosts own every recorded coprocess; callers that replace a
    /// host should shut the old one down (or let it drop, which kills only
    /// recorded children).
    #[doc(hidden)]
    pub fn install_component_host(&mut self, host: ComponentHost) -> Option<ComponentHost> {
        self.component_host.replace(host)
    }

    /// Installed component host, if the composition root provided one.
    #[must_use]
    #[doc(hidden)]
    pub fn component_host(&self) -> Option<&ComponentHost> {
        self.component_host.as_ref()
    }

    /// Mutable component host for non-blocking submit/poll callers.
    ///
    /// Mixing a synchronous `request` with this handle is safe: the host
    /// retains events for other ids and returns them from `poll`.
    #[doc(hidden)]
    pub fn component_host_mut(&mut self) -> Option<&mut ComponentHost> {
        self.component_host.as_mut()
    }

    /// Authorize `plugin_id` for a native-component request under
    /// `manifest_hash`.
    ///
    /// Reuses the accepted model: the plugin must be `Activated`, the caller's
    /// hash must equal the manifest hash, a non-denied grant record must be
    /// bound to that hash, and each capability must pass
    /// [`bitty_plugin_host::GrantStore::is_granted`] (which also enforces
    /// per-capability revocations / `denied_caps`). The resulting
    /// [`PluginGrant`] is the network authority intersection; an empty
    /// (offline) grant is denied before any spawn.
    #[doc(hidden)]
    pub fn component_grant(
        &self,
        plugin_id: &str,
        manifest_hash: &str,
    ) -> Result<PluginGrant, ComponentHostError> {
        let denied = |reason: &str| ComponentHostError::CapabilityDenied {
            plugin: plugin_id.to_owned(),
            reason: reason.to_owned(),
        };
        let id = PluginId::new(plugin_id).map_err(|_| denied("invalid plugin id"))?;
        let Some(entry) = self.plugin_host.registry().get(&id) else {
            return Err(denied("plugin is not registered"));
        };
        if entry.state != PluginState::Activated {
            return Err(ComponentHostError::PluginNotActive {
                plugin: plugin_id.to_owned(),
                state: entry.state.to_string(),
            });
        }
        let actual_hash = entry.manifest.manifest_hash();
        if actual_hash != manifest_hash {
            return Err(denied("manifest hash is not the registered generation"));
        }
        let Some(record) = self.plugin_host.grants().get(&id) else {
            return Err(denied("plugin has no capability grant"));
        };
        if record.denied {
            return Err(denied("plugin capability grant is denied"));
        }
        if record.manifest_hash != actual_hash {
            return Err(denied("capability grant is bound to another manifest hash"));
        }
        // Canonical boundary: `is_granted` checks the declared set, the
        // manifest hash, the record, and per-capability denials (`denied_caps`
        // from an explicit revoke). A bare re-insert never resurrects a
        // revoked capability.
        let granted: Vec<_> = record
            .granted
            .iter()
            .filter(|capability| {
                self.plugin_host.grants().is_granted(
                    &id,
                    &actual_hash,
                    capability,
                    &entry.manifest.capabilities,
                )
            })
            .cloned()
            .collect();
        let grant = PluginGrant::compute(id.as_str(), granted.iter(), &entry.manifest.network)?;
        if grant.is_offline() {
            return Err(denied(
                "no granted network.connect capability covers a declared egress host",
            ));
        }
        Ok(grant)
    }

    /// Submit one component request after capability authorization.
    ///
    /// Never blocks on the component; responses are delivered by
    /// [`Self::poll_component_events`] / [`Self::take_component_events`].
    #[doc(hidden)]
    pub fn submit_component_request(
        &mut self,
        plugin_id: &str,
        manifest_hash: &str,
        component: &str,
        request: ComponentRequest,
    ) -> Result<RequestId, ComponentHostError> {
        if self.component_host.is_none() {
            return Err(ComponentHostError::Unavailable);
        }
        let grant = self.authorize_component(plugin_id, manifest_hash, component)?;
        let host = self
            .component_host
            .as_mut()
            .ok_or(ComponentHostError::Unavailable)?;
        host.submit(Instant::now(), component, &grant, request)
    }

    /// Submit and wait (bounded, up to 310 s) for one component request after
    /// capability authorization.
    ///
    /// This blocks and must never run on the runtime owner/UI thread. It is
    /// kept for headless callers and tests; the production plugin surface
    /// (`bitty.component.request` + `poll`, CTX-2254) uses
    /// [`Self::submit_component_request`] and delivers responses on a later
    /// tick instead.
    #[doc(hidden)]
    pub fn component_request(
        &mut self,
        plugin_id: &str,
        manifest_hash: &str,
        component: &str,
        request: ComponentRequest,
    ) -> Result<ComponentResponse, ComponentHostError> {
        if self.component_host.is_none() {
            return Err(ComponentHostError::Unavailable);
        }
        let grant = self.authorize_component(plugin_id, manifest_hash, component)?;
        let host = self
            .component_host
            .as_mut()
            .ok_or(ComponentHostError::Unavailable)?;
        host.request(component, &grant, request)
    }

    /// [`Self::component_grant`], recording a fail-closed denial so
    /// `bitty extension list` reports `denied:<class>` (#2254).
    fn authorize_component(
        &mut self,
        plugin_id: &str,
        manifest_hash: &str,
        component: &str,
    ) -> Result<PluginGrant, ComponentHostError> {
        match self.component_grant(plugin_id, manifest_hash) {
            Ok(grant) => Ok(grant),
            Err(error) => {
                if let Some(host) = self.component_host.as_mut() {
                    host.record_denial(component, &error);
                }
                Err(error)
            }
        }
    }

    /// Drain pending component events and advance lifecycle policy.
    ///
    /// Equivalent to [`Self::take_component_events`] after pumping the broker
    /// at `now`; events already polled by `tick_at` are returned too.
    #[doc(hidden)]
    pub fn poll_component_events(&mut self, now: Instant) -> Vec<BrokerEvent> {
        self.pump_component_events(now);
        self.take_component_events()
    }

    /// Take events already delivered by `tick_at` without polling again.
    #[doc(hidden)]
    pub fn take_component_events(&mut self) -> Vec<BrokerEvent> {
        self.component_events.drain(..).collect()
    }

    /// Events dropped from the bounded delivery queue.
    #[must_use]
    #[doc(hidden)]
    pub fn component_events_dropped(&self) -> u64 {
        self.component_events_dropped
    }

    /// Poll the broker and retain events for the embedder (bounded, oldest
    /// dropped first). Called by `tick_at`; never discards responses.
    pub(super) fn pump_component_events(&mut self, now: Instant) {
        let events = match self.component_host.as_mut() {
            Some(host) => host.poll(now),
            None => Vec::new(),
        };
        for event in events {
            if self.component_events.len() >= COMPONENT_EVENT_QUEUE_CAPACITY {
                let _ = self.component_events.pop_front();
                self.component_events_dropped = self.component_events_dropped.saturating_add(1);
            }
            self.component_events.push_back(event);
        }
    }

    /// Diagnostic snapshot for a component this runtime has used.
    #[must_use]
    #[doc(hidden)]
    pub fn component_status(&self, component: &str) -> Option<ComponentStatus> {
        self.component_host
            .as_ref()
            .and_then(|host| host.status(component))
    }

    /// Cancel one in-flight component request (CTX-2254, bitty#2254).
    ///
    /// The plugin-facing request surface calls this when a ticket's deadline
    /// or response budget closed first, so the broker sends `Cancel` and drops
    /// late frames instead of accumulating work nobody will read.
    #[doc(hidden)]
    pub fn cancel_component_request(&mut self, id: RequestId) {
        if let Some(host) = self.component_host.as_mut() {
            host.cancel(id);
        }
    }
}
