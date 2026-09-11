//! Plugin registry and lifecycle generations (OQ-011).
//!
//! Every resource (command, handler, timer, task, UI node, store handle) is
//! owned by `(PluginId, generation)`. Reload disposes all generation N
//! resources before activating N+1; the old generation cannot observe or
//! cancel N+1 except through host-mediated handoff of persisted state.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::PluginError;
use crate::manifest::{PluginId, PluginManifest, QualifiedName};

/// Monotonic instance counter per plugin id.
///
/// All runtime resources are owned by one generation; reload increments it.
pub type Generation = u64;

/// Whether a provided service version satisfies a version requirement.
///
/// Uses the canonical package evaluator (`bitty_package::VersionReq`, the same
/// grammar family as plugin dependencies): caret/tilde expansion plus
/// comparator intersection. Fail-closed: unparseable versions or requirements
/// never satisfy. Prerelease candidates require an explicit prerelease opt-in
/// in the requirement text (same rule as the package resolver); a stable-only
/// requirement never matches a prerelease build.
fn service_version_satisfies(provided: &str, requirement: &str) -> bool {
    let (Ok(version), Ok(req)) = (
        bitty_package::Version::parse(provided),
        bitty_package::VersionReq::parse(requirement),
    ) else {
        return false;
    };
    req.matches(&version) && req.allows_prerelease_for(&version)
}

/// Render a cycle path with stable quoting (`'a' -> 'b' -> 'a'`).
fn quoted_path(nodes: &[String]) -> String {
    nodes
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// Lifecycle state per
/// `Declared -> Resolved -> Registered -> Activated -> (Suspended) -> Disposed`
/// with reload creating generation `N+1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginState {
    /// Manifest declared but not yet resolved.
    Declared,
    /// Dependencies resolved, graph consistent (no cycles, compatible constraints).
    Resolved,
    /// Commands, services, event subscriptions reserved at graph construction.
    Registered,
    /// Host has created the VM and activated handlers (generation active).
    Activated,
    /// Detached handlers, retained grants and stored state.
    Suspended,
    /// All resources released (grants and state may still be retained until explicit clear).
    Disposed,
}

impl std::fmt::Display for PluginState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Declared => "Declared",
            Self::Resolved => "Resolved",
            Self::Registered => "Registered",
            Self::Activated => "Activated",
            Self::Suspended => "Suspended",
            Self::Disposed => "Disposed",
        };
        f.write_str(s)
    }
}

/// Entry for one plugin across its current generation.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    /// The declared manifest for this generation.
    pub manifest: PluginManifest,
    /// Current lifecycle state.
    pub state: PluginState,
    /// Current generation (monotonic per plugin id).
    pub generation: Generation,
    /// Qualified commands registered by this plugin in this generation.
    pub commands: Vec<QualifiedName>,
    /// Event subscriptions declared by this generation.
    pub subscribed_events: Vec<String>,
}

impl RegistryEntry {
    /// Create a Declared entry at generation 1.
    fn declared(manifest: PluginManifest) -> Self {
        let commands = manifest.lazy.commands.clone();
        let subscribed_events = manifest.lazy.events.clone();
        Self {
            manifest,
            state: PluginState::Declared,
            generation: 1,
            commands,
            subscribed_events,
        }
    }
}

/// Owned plugin registry.
///
/// Single-responsibility: map from `PluginId` to the entry for its current
/// generation and enforce the ownership rules of the plugin-platform RFC:
/// - duplicate qualified names are rejected at graph construction, not shadowed,
/// - cycles are rejected, incompatible constraints are resolver errors,
/// - every resource is owned by `(PluginId, generation)`.
#[derive(Debug, Default)]
pub struct Registry {
    plugins: BTreeMap<String, RegistryEntry>,
    /// Qualified command -> owning plugin id (to reject duplicates across plugins).
    command_owners: BTreeMap<String, String>,
}

impl Registry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of registered plugins (any state).
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True when no plugin is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Retrieve an entry by plugin id.
    #[must_use]
    pub fn get(&self, id: &PluginId) -> Option<&RegistryEntry> {
        self.plugins.get(id.as_str())
    }

    /// Retrieve a mutable entry (for lifecycle transitions).
    fn get_mut(&mut self, id: &PluginId) -> Option<&mut RegistryEntry> {
        self.plugins.get_mut(id.as_str())
    }

    /// Declare a plugin from its manifest.
    ///
    /// Validates the manifest, inserts as `Declared`, and reserves nothing yet.
    pub fn declare(&mut self, manifest: PluginManifest) -> Result<(), PluginError> {
        manifest.validate()?;
        let id = manifest.id().clone();
        if self.plugins.contains_key(id.as_str()) {
            return Err(PluginError::Duplicate {
                kind: "plugin".to_string(),
                value: id.to_string(),
            });
        }
        let entry = RegistryEntry::declared(manifest);
        self.plugins.insert(id.as_str().to_string(), entry);
        Ok(())
    }

    /// Resolve dependencies for one plugin.
    ///
    /// Checks that the dependency graph remains acyclic and that constraints
    /// are structurally consistent (detailed resolver evaluation is deferred,
    /// but arity and duplicate detection happen here). Transitions
    /// `Declared -> Resolved`.
    ///
    /// Service requirements (`required_services`) are NOT evaluated here:
    /// providers may be declared after the requirer, so satisfaction,
    /// missing-service, and service-cycle checks run at graph scope in
    /// `resolve_all` (mirroring how missing plugin dependencies defer to
    /// `resolve_all`).
    pub fn resolve(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if entry.state != PluginState::Declared {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: PluginState::Declared.to_string(),
            });
        }
        // Cycle detection stub: self-dependency is rejected immediately.
        for (dep_id, _) in &entry.manifest.dependencies {
            if dep_id == id {
                return Err(PluginError::registry(format!(
                    "plugin '{}' cannot depend on itself",
                    id.as_str()
                )));
            }
        }
        // If any dependency is not yet declared, we keep as Declared? For the stub,
        // resolution succeeds; missing deps become resolver errors when the full
        // graph is checked via `resolve_all`.
        entry.state = PluginState::Resolved;
        Ok(())
    }

    /// Resolve all declared plugins (graph-level check: cycles, missing deps,
    /// service requirements).
    ///
    /// In addition to the plugin-dependency checks, every live
    /// (non-Disposed) plugin's `required_services` must be satisfied by the
    /// provides side of the declared graph: the plugin itself (self-provision
    /// needs no edge) or another live plugin whose provided version matches
    /// the requirement under the canonical package evaluator. Anything else
    /// fails closed with no state mutation: unknown interfaces, version
    /// mismatches, unparseable versions/requirements, and requirement cycles
    /// (over the combined dependency + service graph). Service
    /// lookup/invocation at runtime is explicitly out of scope.
    ///
    /// Note the requirement grammar is the closed comparator grammar (same
    /// family as plugin dependencies): wildcard `*` and disjunction `||`
    /// pass manifest syntax but never satisfy at resolve time (fail-closed).
    pub fn resolve_all(&mut self) -> Result<(), PluginError> {
        // Collect ids to avoid borrow issues.
        let ids: Vec<PluginId> = self
            .plugins
            .values()
            .filter(|e| e.state == PluginState::Declared)
            .map(|e| e.manifest.id().clone())
            .collect();

        // Simple cycle / missing detection: DFS over declared deps.
        let known: BTreeSet<String> = self.plugins.keys().cloned().collect();
        for id in &ids {
            let entry = self.plugins.get(id.as_str()).unwrap();
            for (dep, _) in &entry.manifest.dependencies {
                if !known.contains(dep.as_str()) {
                    return Err(PluginError::registry(format!(
                        "plugin '{}' depends on unknown plugin '{}'",
                        id.as_str(),
                        dep.as_str()
                    )));
                }
            }
        }

        // Naive cycle detection via visited set per root.
        for root in &ids {
            let mut stack = vec![root.as_str().to_string()];
            let mut visiting = BTreeSet::new();
            while let Some(cur) = stack.pop() {
                if !visiting.insert(cur.clone()) {
                    return Err(PluginError::registry(format!(
                        "dependency cycle involving '{}'",
                        cur
                    )));
                }
                if let Some(entry) = self.plugins.get(&cur) {
                    for (dep, _) in &entry.manifest.dependencies {
                        // Only follow edges among the declared set; resolved plugins are already acyclic.
                        if self
                            .plugins
                            .get(dep.as_str())
                            .map(|e| e.state == PluginState::Declared)
                            .unwrap_or(false)
                        {
                            if visiting.contains(dep.as_str()) {
                                return Err(PluginError::registry(format!(
                                    "dependency cycle: '{}' -> '{}'",
                                    cur,
                                    dep.as_str()
                                )));
                            }
                            stack.push(dep.as_str().to_string());
                        }
                    }
                }
            }
        }

        // Service requirement satisfaction (requires-side of the loop).
        //
        // Provider index over live (non-Disposed) plugins: interface ->
        // (plugin id, provided version). Checked for every live plugin with
        // requirements (not just Declared), so a singly-resolved requirer
        // cannot dodge the graph gate: `resolve_all` is the backstop.
        let mut providers: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (pid, entry) in &self.plugins {
            if entry.state == PluginState::Disposed {
                continue;
            }
            for (iface, ver) in &entry.manifest.provided_services {
                providers
                    .entry(iface.clone())
                    .or_default()
                    .push((pid.clone(), ver.clone()));
            }
        }
        // Requirer -> satisfying cross-plugin providers. Self-provision with
        // a satisfying version needs no edge (no self-loop).
        let mut service_edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // Deterministic plugin order (BTreeMap key order).
        let live_ids: Vec<String> = self
            .plugins
            .iter()
            .filter(|(_, e)| e.state != PluginState::Disposed)
            .map(|(pid, _)| pid.clone())
            .collect();
        for pid in &live_ids {
            let entry = self.plugins.get(pid).unwrap();
            if entry.manifest.required_services.is_empty() {
                continue;
            }
            for (iface, req) in &entry.manifest.required_services {
                if entry
                    .manifest
                    .provided_services
                    .iter()
                    .any(|(p_iface, p_ver)| {
                        p_iface == iface && service_version_satisfies(p_ver, req)
                    })
                {
                    continue;
                }
                let mut satisfied_by = Vec::new();
                let mut seen_versions = Vec::new();
                if let Some(candidates) = providers.get(iface) {
                    for (provider_pid, p_ver) in candidates {
                        if provider_pid == pid {
                            // Self provides the interface but not at a
                            // satisfying version (checked above): report the
                            // version without adding a self-loop edge.
                            seen_versions.push(format!("'{provider_pid}'={p_ver}"));
                            continue;
                        }
                        seen_versions.push(format!("'{provider_pid}'={p_ver}"));
                        if service_version_satisfies(p_ver, req) {
                            satisfied_by.push(provider_pid.clone());
                        }
                    }
                }
                if satisfied_by.is_empty() {
                    if seen_versions.is_empty() {
                        return Err(PluginError::registry(format!(
                            "plugin '{pid}' requires unknown service '{iface}' ('{req}')"
                        )));
                    }
                    return Err(PluginError::registry(format!(
                        "plugin '{pid}' requires service '{iface}' ('{req}'): no provider satisfies (saw {})",
                        seen_versions.join(", ")
                    )));
                }
                service_edges
                    .entry(pid.clone())
                    .or_default()
                    .extend(satisfied_by);
            }
        }
        // Cycle detection over the combined dependency + service graph.
        // Dependency-only cycles among Declared plugins are already rejected
        // above with their stable message; this pass only runs when service
        // edges exist, so pure dependency graphs keep byte-identical behavior.
        if !service_edges.is_empty() {
            // Combined adjacency over live plugins (sorted, deduplicated).
            let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (pid, entry) in &self.plugins {
                if entry.state == PluginState::Disposed {
                    continue;
                }
                let mut outs = Vec::new();
                for (dep, _) in &entry.manifest.dependencies {
                    if self.plugins.contains_key(dep.as_str()) {
                        outs.push(dep.as_str().to_string());
                    }
                }
                if let Some(svcs) = service_edges.get(pid) {
                    outs.extend(svcs.iter().cloned());
                }
                outs.sort();
                outs.dedup();
                adjacency.insert(pid.clone(), outs);
            }
            // Iterative 3-color DFS from every service requirer (WHITE = 0,
            // GRAY = 1, BLACK = 2). Deterministic: BTreeMap order + sorted edges.
            let mut color: BTreeMap<String, u8> = BTreeMap::new();
            let mut roots: Vec<String> = service_edges.keys().cloned().collect();
            roots.sort();
            for root in &roots {
                // Stack frames: (node, next child index). Index 0 = first visit.
                let mut stack: Vec<(String, usize)> = vec![(root.clone(), 0)];
                let mut path: Vec<String> = Vec::new();
                while let Some((node, next_idx)) = stack.pop() {
                    if next_idx == 0 {
                        match color.get(&node).copied().unwrap_or(0) {
                            // Already fully explored via another root.
                            2 => continue,
                            // Ancestor on the current path: back edge.
                            1 => {
                                let pos = path.iter().position(|n| n == &node).unwrap_or(0);
                                let mut cyc = path[pos..].to_vec();
                                cyc.push(node.clone());
                                return Err(PluginError::registry(format!(
                                    "dependency cycle: {}",
                                    quoted_path(&cyc)
                                )));
                            }
                            _ => {
                                color.insert(node.clone(), 1);
                                path.push(node.clone());
                            }
                        }
                    }
                    let neighbors = adjacency.get(&node).cloned().unwrap_or_default();
                    if next_idx < neighbors.len() {
                        stack.push((node.clone(), next_idx + 1));
                        let child = neighbors[next_idx].clone();
                        match color.get(&child).copied().unwrap_or(0) {
                            2 => {}
                            1 => {
                                let pos = path.iter().position(|n| n == &child).unwrap_or(0);
                                let mut cyc = path[pos..].to_vec();
                                cyc.push(child);
                                return Err(PluginError::registry(format!(
                                    "dependency cycle: {}",
                                    quoted_path(&cyc)
                                )));
                            }
                            _ => stack.push((child, 0)),
                        }
                    } else {
                        color.insert(node.clone(), 2);
                        path.pop();
                    }
                }
            }
        }

        for id in ids {
            // Each still Declared becomes Resolved.
            if let Some(entry) = self.plugins.get_mut(id.as_str()) {
                if entry.state == PluginState::Declared {
                    entry.state = PluginState::Resolved;
                }
            }
        }
        Ok(())
    }
    /// Register a resolved plugin: reserve commands, event subscriptions, claims,
    /// and service provisions so conflicts cannot appear at event time.
    ///
    /// Transitions `Resolved -> Registered`. Duplicate qualified names across
    /// plugins are rejected here, not shadowed.
    pub fn register(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .get(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if entry.state != PluginState::Resolved {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: PluginState::Resolved.to_string(),
            });
        }
        // Check command collisions.
        for q in &entry.commands {
            if let Some(owner) = self.command_owners.get(q.as_str()) {
                return Err(PluginError::Duplicate {
                    kind: "command".to_string(),
                    value: format!("'{q}' already owned by '{owner}'"),
                });
            }
        }
        // Also check event subscription duplicates? The RFC reserves them during graph
        // construction so conflicts cannot appear at event time (not a per-entry error
        // in the stub, but we record the reservation).
        let commands = entry.commands.clone();
        let entry_mut = self.plugins.get_mut(id.as_str()).unwrap();
        for q in &commands {
            self.command_owners
                .insert(q.as_str().to_string(), id.as_str().to_string());
        }
        entry_mut.state = PluginState::Registered;
        Ok(())
    }

    /// Activate a registered plugin (generation becomes live).
    ///
    /// In the full host this creates the VM and completes event subscriptions
    /// and claims, then replays the triggering command once (lazy load). Failure
    /// during activation rejects the invocation with no partially activated state.
    pub fn activate(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if entry.state != PluginState::Registered {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: PluginState::Registered.to_string(),
            });
        }
        entry.state = PluginState::Activated;
        Ok(())
    }

    /// Suspend an activated or registered plugin.
    ///
    /// Detaches handlers and releases CPU tasks while retaining grants and
    /// stored state. Suspended plugins are still registered in the graph.
    pub fn suspend(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if !matches!(
            entry.state,
            PluginState::Activated | PluginState::Registered
        ) {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: format!("{} or {}", PluginState::Activated, PluginState::Registered),
            });
        }
        entry.state = PluginState::Suspended;
        Ok(())
    }

    /// Dispose a plugin: release all generation `N` resources before a reload can create `N+1`.
    ///
    /// The old generation cannot observe or cancel `N+1` except via host-mediated
    /// handoff of persisted state.
    pub fn dispose(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let commands = {
            let entry = self
                .get(id)
                .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
            if entry.state == PluginState::Disposed {
                return Err(PluginError::InvalidState {
                    id: id.to_string(),
                    current: entry.state.to_string(),
                    expected: "non-Disposed".to_string(),
                });
            }
            entry.commands.clone()
        };
        // Release command ownership for this generation.
        for q in &commands {
            self.command_owners.remove(q.as_str());
        }
        let entry = self.plugins.get_mut(id.as_str()).unwrap();
        entry.state = PluginState::Disposed;
        Ok(())
    }

    /// Fully remove a plugin identity and release its command ownership.
    ///
    /// Unlike [`Registry::dispose`], which retains the identity in the
    /// `Disposed` state, `remove` deletes the entry so the same plugin id can
    /// be declared again. It is the rollback primitive for a failed
    /// activation: no partially activated generation survives (RFC
    /// `plugin-host-runtime-rfc` A.4 rule 4).
    ///
    /// # Errors
    ///
    /// [`PluginError::NotFound`] when `id` has no entry.
    pub fn remove(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .plugins
            .remove(id.as_str())
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        for q in &entry.commands {
            self.command_owners.remove(q.as_str());
        }
        Ok(())
    }

    /// Reload: dispose generation `N` and activate generation `N+1` atomically.
    ///
    /// The caller supplies the new manifest for `N+1`; reservations made at
    /// construction are released or retained atomically. If `new_manifest`
    /// fails validation, no disposal occurs (no partially activated state).
    pub fn reload(
        &mut self,
        id: &PluginId,
        new_manifest: PluginManifest,
    ) -> Result<Generation, PluginError> {
        new_manifest.validate()?;
        let entry = self
            .get(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if entry.manifest.id() != id {
            return Err(PluginError::registry(format!(
                "registered manifest identity '{}' does not match requested plugin '{}'",
                entry.manifest.id(),
                id
            )));
        }
        if entry.state == PluginState::Disposed {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: "non-Disposed (cannot reload a disposed plugin)".to_string(),
            });
        }
        // Validate the replacement before releasing the current generation.
        let old_gen = entry.generation;
        // Prepare new entry at generation old+1, state Declared -> Resolved -> Registered -> Activated.
        let new_gen = old_gen.checked_add(1).ok_or_else(|| {
            PluginError::registry(format!("generation overflow for plugin '{}'", id))
        })?;
        let mut new_entry = RegistryEntry::declared(new_manifest);
        if new_entry.manifest.id() != id {
            return Err(PluginError::registry(format!(
                "replacement manifest id '{}' does not match requested plugin '{}'",
                new_entry.manifest.id(),
                id
            )));
        }
        new_entry.generation = new_gen;
        // Validate no duplicate commands with remaining plugins.
        for q in &new_entry.commands {
            if let Some(owner) = self.command_owners.get(q.as_str()) {
                if owner != id.as_str() {
                    return Err(PluginError::Duplicate {
                        kind: "command".to_string(),
                        value: format!("'{q}' already owned by '{owner}'"),
                    });
                }
            }
        }
        let old_commands = entry.commands.clone();
        for q in &old_commands {
            self.command_owners.remove(q.as_str());
        }
        // Commit: replace entry and advance through lifecycle stub (Declared->Resolved->Registered->Activated).
        // For reload we bypass separate steps and mark as Activated directly (all reservations validated).
        new_entry.state = PluginState::Activated;
        for q in &new_entry.commands {
            self.command_owners
                .insert(q.as_str().to_string(), id.as_str().to_string());
        }
        self.plugins.insert(id.as_str().to_string(), new_entry);
        Ok(new_gen)
    }

    /// Resume a suspended plugin.
    pub fn resume(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| PluginError::NotFound { id: id.to_string() })?;
        if entry.state != PluginState::Suspended {
            return Err(PluginError::InvalidState {
                id: id.to_string(),
                current: entry.state.to_string(),
                expected: PluginState::Suspended.to_string(),
            });
        }
        entry.state = PluginState::Registered;
        // Caller may then `activate` again.
        Ok(())
    }

    /// List all plugin ids.
    #[must_use]
    pub fn plugin_ids(&self) -> Vec<PluginId> {
        self.plugins
            .keys()
            .filter_map(|k| PluginId::new(k).ok())
            .collect()
    }

    /// Whether a qualified command is already owned.
    #[must_use]
    pub fn is_command_owned(&self, qualified: &str) -> bool {
        self.command_owners.contains_key(qualified)
    }

    /// Handler-violation isolation stub: first violations log, sustained violations
    /// suspend the handler and surface via `bitty plugin doctor`. Only a stub counter
    /// is kept here; thresholds belong to OQ-014.
    #[must_use]
    pub fn violation_counts(&self) -> BTreeMap<String, u64> {
        // Stub: no per-handler counters retained yet; return empty map.
        // The shape is provided so future isolation work has a stable API.
        BTreeMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        CapabilityRequests, Compat, LazyTriggers, PluginIdentity, PluginManifest,
    };

    fn minimal_manifest(id: &str, commands: Vec<&str>) -> PluginManifest {
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
            lazy: LazyTriggers {
                commands: commands
                    .into_iter()
                    .map(|c| QualifiedName::new(c).unwrap())
                    .collect(),
                events: Vec::new(),
                claims: Vec::new(),
            },
            raw_bytes_len: 256,
        }
    }

    #[test]
    fn lifecycle_happy_path() {
        let mut reg = Registry::new();
        let m = minimal_manifest("xuepoo.markdown", vec!["xuepoo.markdown:toggle"]);
        reg.declare(m).unwrap();
        reg.resolve(&PluginId::new("xuepoo.markdown").unwrap())
            .unwrap();
        reg.register(&PluginId::new("xuepoo.markdown").unwrap())
            .unwrap();
        reg.activate(&PluginId::new("xuepoo.markdown").unwrap())
            .unwrap();
        assert_eq!(
            reg.get(&PluginId::new("xuepoo.markdown").unwrap())
                .unwrap()
                .state,
            PluginState::Activated
        );
    }

    #[test]
    fn duplicate_command_rejected() {
        let mut reg = Registry::new();
        let m1 = minimal_manifest("xuepoo.a", vec!["xuepoo.a:cmd"]);
        let m2 = minimal_manifest("xuepoo.b", vec!["xuepoo.a:cmd"]);
        reg.declare(m1).unwrap();
        reg.declare(m2).unwrap();
        reg.resolve(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        reg.resolve(&PluginId::new("xuepoo.b").unwrap()).unwrap();
        reg.register(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        let err = reg
            .register(&PluginId::new("xuepoo.b").unwrap())
            .unwrap_err();
        assert!(format!("{err}").contains("already owned"));
    }

    #[test]
    fn duplicate_plugin_rejected() {
        let mut reg = Registry::new();
        let m = minimal_manifest("xuepoo.a", vec![]);
        reg.declare(m.clone()).unwrap();
        assert!(reg.declare(m).is_err());
    }

    #[test]
    fn self_dependency_rejected() {
        let mut reg = Registry::new();
        let mut m = minimal_manifest("xuepoo.a", vec![]);
        m.dependencies
            .push((PluginId::new("xuepoo.a").unwrap(), ">=1.0".to_string()));
        reg.declare(m).unwrap();
        assert!(reg.resolve(&PluginId::new("xuepoo.a").unwrap()).is_err());
    }

    #[test]
    fn reload_increments_generation_and_disposes_old() {
        let mut reg = Registry::new();
        let m = minimal_manifest("xuepoo.a", vec!["xuepoo.a:cmd"]);
        reg.declare(m).unwrap();
        reg.resolve(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        reg.register(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        reg.activate(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        let gen_before = reg
            .get(&PluginId::new("xuepoo.a").unwrap())
            .unwrap()
            .generation;
        let new_m = minimal_manifest("xuepoo.a", vec!["xuepoo.a:cmd2"]);
        let new_gen = reg
            .reload(&PluginId::new("xuepoo.a").unwrap(), new_m)
            .unwrap();
        assert_eq!(new_gen, gen_before + 1);
        let entry = reg.get(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        assert_eq!(entry.generation, new_gen);
        assert_eq!(entry.state, PluginState::Activated);
        assert!(reg.is_command_owned("xuepoo.a:cmd2"));
        assert!(!reg.is_command_owned("xuepoo.a:cmd"));
    }

    #[test]
    fn reload_generation_overflow_fails_closed_without_mutation() {
        let mut reg = Registry::new();
        let m = minimal_manifest("xuepoo.overflow", vec!["xuepoo.overflow:cmd"]);
        let id = m.identity.id.clone();
        reg.declare(m).unwrap();
        reg.resolve(&id).unwrap();
        reg.register(&id).unwrap();
        reg.activate(&id).unwrap();
        reg.plugins.get_mut(id.as_str()).unwrap().generation = Generation::MAX;

        let replacement = minimal_manifest("xuepoo.overflow", vec!["xuepoo.overflow:new"]);
        let err = reg.reload(&id, replacement).unwrap_err();

        assert!(format!("{err}").contains("generation overflow"));
        let entry = reg.get(&id).unwrap();
        assert_eq!(entry.generation, Generation::MAX);
        assert_eq!(entry.state, PluginState::Activated);
        assert!(reg.is_command_owned("xuepoo.overflow:cmd"));
        assert!(!reg.is_command_owned("xuepoo.overflow:new"));
    }

    #[test]
    fn dispose_releases_commands() {
        let mut reg = Registry::new();
        let m = minimal_manifest("xuepoo.a", vec!["xuepoo.a:cmd"]);
        reg.declare(m).unwrap();
        reg.resolve(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        reg.register(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        reg.dispose(&PluginId::new("xuepoo.a").unwrap()).unwrap();
        assert!(!reg.is_command_owned("xuepoo.a:cmd"));
    }

    #[test]
    fn remove_releases_identity_and_commands_allowing_redeclare() {
        let mut reg = Registry::new();
        let id = PluginId::new("xuepoo.a").unwrap();
        reg.declare(minimal_manifest("xuepoo.a", vec!["xuepoo.a:cmd"]))
            .unwrap();
        reg.resolve(&id).unwrap();
        reg.register(&id).unwrap();
        assert!(reg.is_command_owned("xuepoo.a:cmd"));

        reg.remove(&id).unwrap();

        assert!(reg.get(&id).is_none(), "identity must be purged");
        assert!(
            !reg.is_command_owned("xuepoo.a:cmd"),
            "command ownership must be released"
        );
        // A fresh declaration of the same id succeeds after removal.
        reg.declare(minimal_manifest("xuepoo.a", vec!["xuepoo.a:cmd"]))
            .unwrap();
        reg.resolve(&id).unwrap();
        reg.register(&id).unwrap();
        assert_eq!(state_of(&reg, "xuepoo.a"), PluginState::Registered);
    }

    #[test]
    fn remove_unknown_id_is_not_found() {
        let mut reg = Registry::new();
        let id = PluginId::new("xuepoo.missing").unwrap();
        assert!(matches!(reg.remove(&id), Err(PluginError::NotFound { .. })));
    }

    fn manifest_with_services(
        id: &str,
        provided: Vec<(&str, &str)>,
        required: Vec<(&str, &str)>,
    ) -> PluginManifest {
        let mut m = minimal_manifest(id, vec![]);
        m.provided_services = provided
            .into_iter()
            .map(|(iface, ver)| (iface.to_string(), ver.to_string()))
            .collect();
        m.required_services = required
            .into_iter()
            .map(|(iface, req)| (iface.to_string(), req.to_string()))
            .collect();
        m
    }

    fn state_of(reg: &Registry, id: &str) -> PluginState {
        reg.get(&PluginId::new(id).unwrap()).unwrap().state
    }

    #[test]
    fn service_requirements_valid_graph_resolves() {
        let mut reg = Registry::new();
        reg.declare(manifest_with_services(
            "xuepoo.provider",
            vec![("markdown.render", "1.2.0")],
            vec![],
        ))
        .unwrap();
        reg.declare(manifest_with_services(
            "xuepoo.consumer",
            vec![],
            vec![("markdown.render", "^1.0")],
        ))
        .unwrap();
        reg.resolve_all().unwrap();
        assert_eq!(state_of(&reg, "xuepoo.provider"), PluginState::Resolved);
        assert_eq!(state_of(&reg, "xuepoo.consumer"), PluginState::Resolved);
    }

    #[test]
    fn service_requirements_missing_service_fails_closed() {
        let mut reg = Registry::new();
        reg.declare(manifest_with_services(
            "xuepoo.consumer",
            vec![],
            vec![("nosuch.svc", "^1.0")],
        ))
        .unwrap();
        let err = reg.resolve_all().unwrap_err();
        assert!(format!("{err}").contains("unknown service"));
        // Fail-closed: no state mutation.
        assert_eq!(state_of(&reg, "xuepoo.consumer"), PluginState::Declared);
    }

    #[test]
    fn service_requirements_version_mismatch_fails_closed() {
        let mut reg = Registry::new();
        reg.declare(manifest_with_services(
            "xuepoo.provider",
            vec![("markdown.render", "1.0.0")],
            vec![],
        ))
        .unwrap();
        reg.declare(manifest_with_services(
            "xuepoo.consumer",
            vec![],
            vec![("markdown.render", "^2.0")],
        ))
        .unwrap();
        let err = reg.resolve_all().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no provider satisfies"), "{msg}");
        assert!(msg.contains("xuepoo.provider"), "{msg}");
        assert_eq!(state_of(&reg, "xuepoo.provider"), PluginState::Declared);
        assert_eq!(state_of(&reg, "xuepoo.consumer"), PluginState::Declared);
    }

    #[test]
    fn service_requirements_cycle_fails_closed() {
        let mut reg = Registry::new();
        reg.declare(manifest_with_services(
            "xuepoo.a",
            vec![("svc.a", "1.0.0")],
            vec![("svc.b", "^1.0")],
        ))
        .unwrap();
        reg.declare(manifest_with_services(
            "xuepoo.b",
            vec![("svc.b", "1.0.0")],
            vec![("svc.a", "^1.0")],
        ))
        .unwrap();
        let err = reg.resolve_all().unwrap_err();
        assert!(format!("{err}").contains("cycle"));
        assert_eq!(state_of(&reg, "xuepoo.a"), PluginState::Declared);
        assert_eq!(state_of(&reg, "xuepoo.b"), PluginState::Declared);
    }

    #[test]
    fn service_requirements_self_provision_resolves() {
        let mut reg = Registry::new();
        reg.declare(manifest_with_services(
            "xuepoo.solo",
            vec![("svc.solo", "1.0.0")],
            vec![("svc.solo", "^1.0")],
        ))
        .unwrap();
        reg.resolve_all().unwrap();
        assert_eq!(state_of(&reg, "xuepoo.solo"), PluginState::Resolved);
    }

    #[test]
    fn service_requirements_single_resolve_defers_to_graph() {
        // Single `resolve` stays lenient (providers may be declared later),
        // mirroring missing plugin dependencies; `resolve_all` is the backstop.
        let mut reg = Registry::new();
        reg.declare(manifest_with_services(
            "xuepoo.consumer",
            vec![],
            vec![("markdown.render", "^1.0")],
        ))
        .unwrap();
        reg.resolve(&PluginId::new("xuepoo.consumer").unwrap())
            .unwrap();
        assert_eq!(state_of(&reg, "xuepoo.consumer"), PluginState::Resolved);
        // Backstop still enforces: missing service fails even for Resolved entries.
        let err = reg.resolve_all().unwrap_err();
        assert!(format!("{err}").contains("unknown service"));
        // Late provider satisfies the graph.
        reg.declare(manifest_with_services(
            "xuepoo.provider",
            vec![("markdown.render", "1.2.0")],
            vec![],
        ))
        .unwrap();
        reg.resolve_all().unwrap();
        assert_eq!(state_of(&reg, "xuepoo.provider"), PluginState::Resolved);
        assert_eq!(state_of(&reg, "xuepoo.consumer"), PluginState::Resolved);
    }
}
