//! Production wiring for installed native components (CTX-1361, bitty#2254).
//!
//! [`Runtime`] owns at most one [`ComponentHost`](crate::component::ComponentHost).
//! The composition root installs it during startup; `None` is fail-closed.
//! Requests are authorized against the accepted plugin capability model
//! (`PluginHost` registry + grant store + manifest `[[network.egress]]`) and
//! then handed to the host, which resolves and digest-verifies the installed
//! component before every spawn.

use std::time::Instant;

use bitty_plugin_host::PluginId;

use crate::component::{
    ComponentHost, ComponentHostError, ComponentRequest, ComponentResponse, ComponentStatus,
    PluginGrant, RequestId,
};

use super::Runtime;

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
    #[doc(hidden)]
    pub fn component_host_mut(&mut self) -> Option<&mut ComponentHost> {
        self.component_host.as_mut()
    }

    /// Authorize `plugin_id` for a native-component request under
    /// `manifest_hash`.
    ///
    /// Reuses the accepted model: the plugin must be registered, the caller's
    /// hash must equal the manifest hash, a non-denied grant record must be
    /// bound to that hash, and only declared capabilities count. The
    /// resulting [`PluginGrant`] is the network authority intersection; an
    /// empty (offline) grant is denied before any spawn.
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
        let granted: Vec<_> = record
            .granted
            .iter()
            .filter(|capability| entry.manifest.capabilities.contains(capability))
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
    /// Never blocks on the component; responses arrive from
    /// [`Self::poll_component_events`].
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
        let grant = self.component_grant(plugin_id, manifest_hash)?;
        let host = self
            .component_host
            .as_mut()
            .ok_or(ComponentHostError::Unavailable)?;
        host.submit(Instant::now(), component, &grant, request)
    }

    /// Submit and wait (bounded) for one component request after capability
    /// authorization.
    ///
    /// This is the synchronous composition/test entry point; embedders with
    /// an event loop should use [`Self::submit_component_request`] and
    /// [`Self::poll_component_events`].
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
        let grant = self.component_grant(plugin_id, manifest_hash)?;
        let host = self
            .component_host
            .as_mut()
            .ok_or(ComponentHostError::Unavailable)?;
        host.request(component, &grant, request)
    }

    /// Drain pending component events and advance lifecycle policy.
    #[doc(hidden)]
    pub fn poll_component_events(&mut self, now: Instant) -> Vec<crate::component::BrokerEvent> {
        match self.component_host.as_mut() {
            Some(host) => host.poll(now),
            None => Vec::new(),
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
}
