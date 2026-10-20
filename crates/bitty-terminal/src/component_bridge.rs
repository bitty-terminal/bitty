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
//!   A hot reload with a new manifest hash replaces the mirrored predecessor,
//!   so the policy host follows the live generation rather than the startup
//!   snapshot.
//! - [`pump_component_requests`] (re-exported from `bitty-runtime`) is the
//!   per-tick bridge between queued `bitty.component.request` submissions and
//!   the existing [`ComponentBroker`](bitty_runtime::component::ComponentBroker).
//!
//! Neither function spawns a component, opens a socket for it, or invents
//! authority: spawn happens only inside the broker, and only for a plugin that
//! is `Activated`, hash-matched, and granted.

use std::collections::BTreeSet;
use std::time::Instant;

use bitty_plugin_host::{CapabilityId, GrantRecord, PluginId, PluginState};
use bitty_runtime::Runtime;
use bitty_runtime::plugin_runtime::{LifecycleState, PluginRuntime, broker_request};

/// Remove one mirrored policy-host generation, reporting a refusal.
///
/// Returns whether the identity is gone afterwards. A failed removal is
/// reported and treated like a refusal so the caller leaves the plugin
/// unmirrored (fail closed).
fn remove_mirrored(
    runtime: &mut Runtime,
    id: &PluginId,
    warn: &mut impl FnMut(&str, &str),
) -> bool {
    match runtime.plugin_host_mut().remove(id) {
        Ok(()) => true,
        Err(error) => {
            warn(id.as_str(), &error.to_string());
            false
        }
    }
}

/// Mirror every active plugin generation's manifest and grant into the
/// runtime's policy host.
///
/// Returns the number of generations mirrored. Idempotent for the current
/// generation: an `Activated` id whose manifest hash already matches the
/// plugin runtime's active package is left as is. A matching hash alone is not
/// enough: if `register_plugin` succeeded but `activate_plugin` failed (for
/// example an explicit capability denial), the entry stays `Registered` and a
/// later mirror retries the grant and activation instead of skipping the id
/// forever. When the hash changed (a hot reload, `PluginRuntime::reload`), the
/// predecessor is removed and the new generation is registered from scratch,
/// so component requests carry the new manifest hash instead of being denied
/// against a stale mirror. A plugin the runtime refuses is reported through
/// `warn` and stays unmirrored (or loses its predecessor), so its component
/// requests fail closed instead of inheriting another generation's authority.
///
/// Two security invariants hold across a refresh:
///
/// - an explicit plugin-level denial in the policy host is sticky: the
///   reloaded generation is neither re-granted nor activated;
/// - an id the plugin runtime no longer keeps `Active` (disposed, failed, or
///   otherwise not live) loses its mirrored policy-host entry, so the
///   predecessor's grant cannot outlive its generation.
pub fn mirror_plugin_authority(
    runtime: &mut Runtime,
    plugins: &PluginRuntime,
    mut warn: impl FnMut(&str, &str),
) -> usize {
    let discovered = plugins.discovered_ids();
    let active: BTreeSet<PluginId> = discovered
        .iter()
        .filter(|id| plugins.state(id) == Some(&LifecycleState::Active))
        .cloned()
        .collect();

    // A generation that is no longer live has no component authority.
    // `discovered_ids` retains disposed and failed entries, so this covers
    // every id the plugin runtime still knows; unrelated policy-host ids are
    // left untouched.
    for id in &discovered {
        if active.contains(id) || runtime.plugin_host().registry().get(id).is_none() {
            continue;
        }
        let _ = remove_mirrored(runtime, id, &mut warn);
    }

    // Snapshot first, then register every generation before activating any:
    // `register_plugin` resolves dependencies against the runtime registry, so
    // a dependent plugin must not lose the race against its dependency's id
    // order.
    let mut pending = Vec::new();
    let mut retries = Vec::new();
    for id in &discovered {
        if !active.contains(id) {
            continue;
        }
        let Some(package) = plugins.discovered_package(id) else {
            continue;
        };
        let manifest = package.manifest.clone();
        let hash = manifest.manifest_hash();
        let current = runtime.plugin_host().registry().get(id);
        let registered_hash = current.map(|entry| entry.manifest.manifest_hash());
        let registered_state = current.map(|entry| entry.state);
        let same_hash = registered_hash.as_deref() == Some(hash.as_str());
        if same_hash && registered_state == Some(PluginState::Activated) {
            continue;
        }
        // A host denial is an explicit decision that a reload cannot clear.
        // Remove any predecessor so its authority cannot survive by accident,
        // then leave this generation unmirrored (fail closed). This check runs
        // before the retry path below: inserting a grant record clears the
        // plugin-level denial marker, which would turn a denied generation
        // into an authorized one.
        let denied = runtime.plugin_host().grants().is_denied(id)
            || runtime
                .plugin_host()
                .grants()
                .get(id)
                .is_some_and(|record| record.denied);
        if denied {
            if registered_hash.is_some() {
                let _ = remove_mirrored(runtime, id, &mut warn);
            }
            warn(
                id.as_str(),
                "explicit policy-host denial is retained; the generation is not mirrored",
            );
            continue;
        }
        let granted: BTreeSet<CapabilityId> = match package.granted.as_ref() {
            Some(granted) => granted
                .iter()
                .filter_map(|capability| CapabilityId::parse(capability).ok())
                .collect(),
            None => match manifest.capabilities.all_ids() {
                Ok(ids) => ids,
                Err(error) => {
                    if registered_hash.is_some() {
                        let _ = remove_mirrored(runtime, id, &mut warn);
                    }
                    warn(id.as_str(), &error.to_string());
                    continue;
                }
            },
        };
        if same_hash && registered_state == Some(PluginState::Registered) {
            // Registration succeeded on an earlier mirror but activation did
            // not. Re-registering would fail in `Registered`, so keep the
            // entry and retry the grant/activation below.
            retries.push((id.clone(), hash, granted));
        } else {
            // A new generation, or a policy-host entry that is not in a state
            // activation can start from: replace it from scratch.
            pending.push((
                id.clone(),
                manifest,
                hash,
                granted,
                registered_hash.is_some(),
            ));
        }
    }

    // A reloaded generation replaces its predecessor's identity. `remove`
    // (rather than `dispose`) purges the id so the new manifest can be
    // declared; a removal failure leaves the plugin unmirrored and therefore
    // fail-closed.
    let mut replacements = Vec::with_capacity(pending.len());
    for (id, manifest, hash, granted, stale) in pending {
        if stale && !remove_mirrored(runtime, &id, &mut warn) {
            continue;
        }
        replacements.push((id, manifest, hash, granted));
    }

    let mut registered = Vec::with_capacity(replacements.len());
    for (id, manifest, hash, granted) in replacements {
        match runtime.register_plugin(manifest) {
            Ok(()) => registered.push((id, hash, granted)),
            Err(error) => warn(id.as_str(), &error.to_string()),
        }
    }

    // Retried generations are already `Registered`; queue them after the
    // fresh registrations so every dependency is registered before any
    // activation runs.
    registered.extend(retries);

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
