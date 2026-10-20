//! Production producer wiring for installed native components (CTX-2254,
//! bitty#2254).
//!
//! Two composition-root responsibilities live here so the binary and the
//! integration tests exercise exactly one implementation:
//!
//! - [`mirror_plugin_authority`] copies the plugin policy state the plugin
//!   runtime already decided (manifest, grant record, lifecycle) into the
//!   runtime's own `PluginHost`. The component broker's capability gate reads
//!   that host, and before this mirror it was empty in production, so every
//!   request would deny as "plugin is not registered". Nothing is granted
//!   here: the mirror reproduces the activated generation's existing grant.
//! - [`pump_component_requests`] (re-exported from `bitty-runtime`) is the
//!   per-tick bridge between queued `bitty.component.request` submissions and
//!   the existing [`ComponentBroker`](bitty_runtime::component::ComponentBroker).
//!
//! Neither function spawns a component, opens a socket for it, or invents
//! authority: spawn happens only inside the broker, and only for a plugin that
//! is `Activated`, hash-matched, and granted.

use std::collections::BTreeSet;
use std::time::Instant;

#[cfg(test)]
use bitty_plugin_host::PluginState;
use bitty_plugin_host::{CapabilityId, GrantRecord, PluginId};
use bitty_runtime::Runtime;
use bitty_runtime::plugin_runtime::{LifecycleState, PluginRuntime, broker_request};

/// Mirror every active plugin generation's manifest and grant into the
/// runtime's policy host.
///
/// Returns the number of generations mirrored. Idempotent: an already
/// registered id is skipped, and a grant already bound to the same manifest
/// generation is left as is. A plugin the runtime refuses is reported through
/// `warn` and stays unmirrored, so its component requests fail closed instead
/// of inheriting another generation's authority.
pub fn mirror_plugin_authority(
    runtime: &mut Runtime,
    plugins: &PluginRuntime,
    mut warn: impl FnMut(&str, &str),
) -> usize {
    // Snapshot first, then register every generation before activating any:
    // `register_plugin` resolves dependencies against the runtime registry, so
    // a dependent plugin must not lose the race against its dependency's id
    // order.
    let mut pending = Vec::new();
    for id in plugins.discovered_ids() {
        if plugins.state(&id) != Some(&LifecycleState::Active) {
            continue;
        }
        let Some(package) = plugins.discovered_package(&id) else {
            continue;
        };
        if runtime.plugin_host().registry().get(&id).is_some() {
            continue;
        }
        let manifest = package.manifest.clone();
        let hash = manifest.manifest_hash();
        let granted: BTreeSet<CapabilityId> = match package.granted.as_ref() {
            Some(granted) => granted
                .iter()
                .filter_map(|capability| CapabilityId::parse(capability).ok())
                .collect(),
            None => match manifest.capabilities.all_ids() {
                Ok(ids) => ids,
                Err(error) => {
                    warn(id.as_str(), &error.to_string());
                    continue;
                }
            },
        };
        pending.push((id, manifest, hash, granted));
    }

    let mut registered = Vec::with_capacity(pending.len());
    for (id, manifest, hash, granted) in pending {
        match runtime.register_plugin(manifest) {
            Ok(()) => registered.push((id, hash, granted)),
            Err(error) => warn(id.as_str(), &error.to_string()),
        }
    }

    let mut mirrored = 0usize;
    for (id, hash, granted) in registered {
        let plugin_id = PluginId::new(id.as_str()).expect("discovered ids are valid");
        runtime.insert_grant(GrantRecord::granted(plugin_id.clone(), hash, granted, 0));
        match runtime.activate_plugin(&plugin_id) {
            Ok(()) => mirrored += 1,
            Err(error) => warn(id.as_str(), &error.to_string()),
        }
    }
    mirrored
}

/// Submit queued plugin component requests and deliver broker events for one
/// application tick (CTX-2254, bitty#2254).
///
/// This is the shipped per-tick glue: `bitty.component.request` only enqueues
/// a bounded submission, and this function authorizes it through the runtime's
/// canonical capability model before the existing broker resolves and
/// digest-verifies the installed component. Returns the number of submissions
/// taken from the queue.
///
/// Bounded by the plugin runtime's queue capacities and one broker poll per
/// call. Nothing here waits on a component: responses arrive as broker events
/// and become `bitty.component.poll` results on a later tick.
pub fn pump_component_requests(
    runtime: &mut Runtime,
    plugins: &mut PluginRuntime,
    now: Instant,
) -> usize {
    let submissions = plugins.drain_component_requests();
    let submitted = submissions.len();
    for submission in submissions {
        let request = broker_request(&submission.request);
        match runtime.submit_component_request(
            &submission.plugin_id,
            &submission.manifest_hash,
            &submission.request.component,
            request,
        ) {
            Ok(id) => plugins.bind_component_request(
                submission.ticket,
                &submission.plugin_id,
                id,
                submission.deadline,
            ),
            Err(error) => plugins.fail_component_request(
                submission.ticket,
                &submission.plugin_id,
                error.class(),
                &error.to_string(),
            ),
        }
    }
    for id in plugins.expire_component_requests(now) {
        runtime.cancel_component_request(id);
    }
    let events = runtime.poll_component_events(now);
    plugins.deliver_component_events(&events);
    for id in plugins.take_cancelled_component_requests() {
        runtime.cancel_component_request(id);
    }
    submitted
}

/// Whether the runtime registry sees `plugin` as an `Activated` generation
/// (diagnostics for the surface's preconditions).
#[cfg(test)]
#[must_use]
pub fn plugin_is_active(runtime: &Runtime, plugin: &str) -> bool {
    let Ok(id) = PluginId::new(plugin) else {
        return false;
    };
    runtime
        .plugin_host()
        .registry()
        .get(&id)
        .is_some_and(|entry| entry.state == PluginState::Activated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_plugin_is_not_active() {
        let runtime =
            Runtime::with_deterministic_rasterizer(bitty_runtime::RuntimeConfig::default())
                .expect("runtime");
        assert!(!plugin_is_active(&runtime, "acme.missing"));
        assert!(!plugin_is_active(&runtime, "not a valid id"));
    }
}
