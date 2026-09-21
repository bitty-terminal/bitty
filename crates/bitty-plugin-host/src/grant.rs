//! Grant lifecycle stubs (OQ-012, part 2).
//!
//! Persisted grant records bind `(plugin-id, manifest-hash)` to the set of
//! granted capabilities. This module provides headless, in-memory stubs for
//! the full lifecycle: request, consent, persistence, update, revocation,
//! re-grant, and workspace narrowing. No file I/O is performed yet; storage
//! under the configuration state directory and the CLI/plugin-manager
//! revocation surface are deferred behind these owned stubs.

use std::collections::{BTreeMap, BTreeSet};

use crate::capability::CapabilityId;
use crate::error::PluginError;
use crate::manifest::{CapabilityRequests, PluginId};

/// How the grant decision was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrantOrigin {
    /// User consented via the host consent UX (one dialog per capability family group).
    ConsentUi,
    /// Revoked explicitly via CLI or plugin manager.
    Revoked,
    /// Denied and persisted as a denial record (prevents re-prompt loops).
    Denied,
    /// Carried forward silently because the update narrowed or kept the set.
    CarriedForward,
}

/// A persisted grant decision for one plugin plus manifest hash.
///
/// Content-addressed to the manifest hash: any manifest change recomputes the
/// hash, added capabilities block automatic update and require diff approval,
/// unchanged or narrowed sets carry forward silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    /// Plugin id this record belongs to.
    pub plugin_id: PluginId,
    /// Hex-encoded hash of the manifest (opaque to this crate; caller computes it).
    pub manifest_hash: String,
    /// Granted capability set (closed, validated identifiers only).
    pub granted: BTreeSet<CapabilityId>,
    /// When the decision was made (monotonic host time, opaque u64 for stub).
    pub decided_at: u64,
    /// Origin of the decision.
    pub origin: GrantOrigin,
    /// Whether this record represents a denial (re-prompts only after explicit revocation).
    pub denied: bool,
}

impl GrantRecord {
    /// Create a new granted record.
    #[must_use]
    pub fn granted(
        plugin_id: PluginId,
        manifest_hash: impl Into<String>,
        granted: BTreeSet<CapabilityId>,
        decided_at: u64,
    ) -> Self {
        Self {
            plugin_id,
            manifest_hash: manifest_hash.into(),
            granted,
            decided_at,
            origin: GrantOrigin::ConsentUi,
            denied: false,
        }
    }

    /// Create a denial record.
    #[must_use]
    pub fn denied(plugin_id: PluginId, manifest_hash: impl Into<String>, decided_at: u64) -> Self {
        Self {
            plugin_id,
            manifest_hash: manifest_hash.into(),
            granted: BTreeSet::new(),
            decided_at,
            origin: GrantOrigin::Denied,
            denied: true,
        }
    }

    /// Whether `capability` is granted by this record.
    #[must_use]
    pub fn is_granted(&self, capability: &CapabilityId) -> bool {
        !self.denied && self.granted.contains(capability)
    }
}

/// Outcome of a granted plugin update ([`GrantStore::apply_update`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// Plugin id the update applied to.
    pub plugin_id: PluginId,
    /// Manifest hash the update moved away from.
    pub old_hash: String,
    /// Manifest hash the update moved to.
    pub new_hash: String,
    /// Capabilities in the new set absent from the prior grant (sorted).
    /// Empty when the update narrowed or kept the set.
    pub added: Vec<CapabilityId>,
    /// True when no capability was added: the grant carried forward silently
    /// (narrowed, equal, or idempotent same-hash re-apply).
    pub carried_forward: bool,
}

/// In-memory grant store (stub for the state-directory persistence).
///
/// Invariants enforced:
/// - Undeclared authority cannot be exercised even if a stale grant record exists
///   (callers must intersect requested capabilities with grants; see [`GrantStore::is_granted`]).
/// - Workspace configuration may narrow grants but may never add any.
/// - Revocation takes effect at the next dispatch boundary (host detaches handlers).
#[derive(Debug, Default, Clone)]
pub struct GrantStore {
    /// Records keyed by plugin id.
    records: BTreeMap<String, GrantRecord>,
    /// Denial markers to prevent re-prompt loops (per plugin id).
    denials: BTreeSet<String>,
    /// Per-capability denials from single-capability revocation (CTX-0465).
    ///
    /// A single-capability revoke persists here so the revoked capability
    /// cannot be silently re-prompted or re-granted: `is_granted` fails
    /// closed on denied entries, and re-grant requires explicit
    /// [`GrantStore::clear_cap_denial`] (user action), never a bare
    /// `insert`. Keyed by plugin id, then the denied capability set.
    denied_caps: BTreeMap<String, BTreeSet<CapabilityId>>,
}

impl GrantStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored grant records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no records are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Insert or replace a grant record.
    pub fn insert(&mut self, record: GrantRecord) {
        let key = record.plugin_id.as_str().to_string();
        if record.denied {
            self.denials.insert(key.clone());
        } else {
            self.denials.remove(&key);
        }
        self.records.insert(key, record);
    }

    /// Retrieve a record for `plugin_id`.
    #[must_use]
    pub fn get(&self, plugin_id: &PluginId) -> Option<&GrantRecord> {
        self.records.get(plugin_id.as_str())
    }

    /// Whether `capability` is granted for `plugin_id` under the current manifest hash.
    ///
    /// Returns `false` if no record exists, if the stored hash does not match
    /// `manifest_hash`, or if the capability is not in the granted set
    /// (deny-by-default). Callers should have already validated that the
    /// capability is among the manifest's declared requests.
    #[must_use]
    pub fn is_granted(
        &self,
        plugin_id: &PluginId,
        manifest_hash: &str,
        capability: &CapabilityId,
        declared: &CapabilityRequests,
    ) -> bool {
        if !declared.contains(capability) {
            return false;
        }
        if self.is_cap_denied(plugin_id, capability) {
            return false;
        }
        let Some(rec) = self.get(plugin_id) else {
            return false;
        };
        if rec.manifest_hash != manifest_hash {
            return false;
        }
        rec.is_granted(capability)
    }

    /// Decide whether an update from `old_hash` to `new_hash` with capability sets
    /// `old_caps` -> `new_caps` can be carried forward without re-prompt.
    ///
    /// - If `new_caps` is subset of `old_caps` (narrowed or equal), grants carry silently.
    /// - If `new_caps` adds capabilities, approval is required (returns `false`).
    #[must_use]
    pub fn can_carry_forward(
        &self,
        old_caps: &BTreeSet<CapabilityId>,
        new_caps: &BTreeSet<CapabilityId>,
    ) -> bool {
        new_caps.is_subset(old_caps)
    }

    /// Check whether the manifest hash changed between the stored record and a candidate.
    #[must_use]
    pub fn hash_changed(&self, plugin_id: &PluginId, candidate_hash: &str) -> bool {
        self.get(plugin_id)
            .map(|r| r.manifest_hash != candidate_hash)
            .unwrap_or(false)
    }

    /// Apply a plugin update to the stored grant (R-016 / P0-AC-030).
    ///
    /// The install pipeline (`install::verify_install`) blocks artifact-level
    /// updates; this is the grant-lifecycle counterpart: it binds the new
    /// capability set to the new manifest hash so a silently broadened update
    /// can never take effect through a bare `insert`.
    ///
    /// - Unknown plugin fails closed (`NotFound`): first installs go through
    ///   consent, never through update.
    /// - `old_hash` must equal the stored hash: stale or confused updates
    ///   (including downgrade/rollback confusion) fail closed with no state change.
    /// - Added capabilities (`new_caps - granted`) without `approved` fail
    ///   closed with no state change: the update blocks pending an explicit
    ///   permission diff and approval.
    /// - Narrowed or equal sets carry forward silently (`CarriedForward`);
    ///   this is the downgrade path. Rollback to a broader set is possible
    ///   but requires `approved`, exactly like any other expansion.
    /// - A denied plugin is never revived by an update, even an approved one:
    ///   explicit re-grant is required.
    /// - Per-capability denials (CTX-0465) survive updates: even an approved
    ///   expansion cannot resurrect a revoked capability without explicit
    ///   [`GrantStore::clear_cap_denial`].
    /// - Same-hash grant changes are not updates: `new_hash == old_hash`
    ///   requires `new_caps == granted` (idempotent no-op success), otherwise
    ///   fails closed.
    pub fn apply_update(
        &mut self,
        plugin_id: &PluginId,
        old_hash: &str,
        new_hash: &str,
        new_caps: &BTreeSet<CapabilityId>,
        approved: bool,
        decided_at: u64,
    ) -> Result<UpdateOutcome, PluginError> {
        let key = plugin_id.as_str().to_string();
        // Denied plugins are never revived by an update, even an approved one:
        // explicit re-grant is required. Checked first because a full revoke
        // removes the record while the denial marker persists.
        if self.denials.contains(&key) {
            return Err(PluginError::grant(format!(
                "plugin '{}' is denied; explicit re-grant required (update cannot revive a denial)",
                plugin_id.as_str()
            )));
        }
        let current = self
            .records
            .get(&key)
            .ok_or_else(|| PluginError::NotFound {
                id: plugin_id.to_string(),
            })?;
        if current.denied {
            return Err(PluginError::grant(format!(
                "plugin '{}' is denied; explicit re-grant required (update cannot revive a denial)",
                plugin_id.as_str()
            )));
        }
        if current.manifest_hash != old_hash {
            return Err(PluginError::grant(format!(
                "stale update for '{}': expected current hash '{}', got '{}'",
                plugin_id.as_str(),
                current.manifest_hash,
                old_hash
            )));
        }
        if current.denied || self.denials.contains(&key) {
            return Err(PluginError::grant(format!(
                "plugin '{}' is denied; explicit re-grant required (update cannot revive a denial)",
                plugin_id.as_str()
            )));
        }
        if new_hash == old_hash {
            if *new_caps != current.granted {
                return Err(PluginError::grant(format!(
                    "same-manifest grant change for '{}' is not an update (hash '{}')",
                    plugin_id.as_str(),
                    old_hash
                )));
            }
            return Ok(UpdateOutcome {
                plugin_id: plugin_id.clone(),
                old_hash: old_hash.to_string(),
                new_hash: new_hash.to_string(),
                added: Vec::new(),
                carried_forward: true,
            });
        }
        let added: Vec<CapabilityId> = new_caps.difference(&current.granted).cloned().collect();
        if !added.is_empty() && !approved {
            let names: Vec<&str> = added.iter().map(CapabilityId::as_str).collect();
            return Err(PluginError::grant(format!(
                "update for '{}' adds {} capabilit{} ({}); blocked pending explicit diff approval",
                plugin_id.as_str(),
                added.len(),
                if added.len() == 1 { "y" } else { "ies" },
                names.join(", ")
            )));
        }
        if let Some(denied_set) = self.denied_caps.get(&key) {
            let resurrected: Vec<&str> = new_caps
                .iter()
                .filter(|cap| denied_set.contains(*cap))
                .map(CapabilityId::as_str)
                .collect();
            if !resurrected.is_empty() {
                return Err(PluginError::grant(format!(
                    "update for '{}' resurrects explicitly denied capabilit{} ({}); explicit re-grant required",
                    plugin_id.as_str(),
                    if resurrected.len() == 1 { "y" } else { "ies" },
                    resurrected.join(", ")
                )));
            }
        }
        let carried_forward = added.is_empty();
        let origin = if carried_forward {
            GrantOrigin::CarriedForward
        } else {
            GrantOrigin::ConsentUi
        };
        self.records.insert(
            key,
            GrantRecord {
                plugin_id: plugin_id.clone(),
                manifest_hash: new_hash.to_string(),
                granted: new_caps.clone(),
                decided_at,
                origin,
                denied: false,
            },
        );
        Ok(UpdateOutcome {
            plugin_id: plugin_id.clone(),
            old_hash: old_hash.to_string(),
            new_hash: new_hash.to_string(),
            added,
            carried_forward,
        })
    }

    /// Revoke grants for `plugin_id`.
    ///
    /// If `capability` is `Some`, remove only that capability and persist a
    /// per-capability denial (CTX-0465): re-prompting or re-granting the
    /// revoked capability requires explicit [`GrantStore::clear_cap_denial`].
    /// When the last granted capability is revoked this way, the record is
    /// removed and a plugin-level denial marker is set, exactly as for a
    /// full revoke. Otherwise remove the whole grant record. The host must
    /// detach affected handlers at the next dispatch boundary and report
    /// what was revoked.
    pub fn revoke(
        &mut self,
        plugin_id: &PluginId,
        capability: Option<&CapabilityId>,
    ) -> Result<RevokeReport, PluginError> {
        let key = plugin_id.as_str().to_string();
        let rec = self
            .records
            .get_mut(&key)
            .ok_or_else(|| PluginError::NotFound {
                id: plugin_id.to_string(),
            })?;

        if let Some(cap) = capability {
            let removed = rec.granted.remove(cap);
            if !removed {
                return Err(PluginError::grant(format!(
                    "capability '{cap}' not granted for '{}'",
                    plugin_id.as_str()
                )));
            }
            // Persist the denial before any escalation so the explicit deny
            // state survives even when the record itself is removed below.
            self.denied_caps
                .entry(key.clone())
                .or_default()
                .insert(cap.clone());
            if rec.granted.is_empty() {
                // Last capability out: escalate to a full revoke (remove the
                // record, persist the plugin-level denial marker).
                self.records.remove(&key);
                self.denials.insert(key);
                return Ok(RevokeReport {
                    plugin_id: plugin_id.clone(),
                    revoked: vec![cap.clone()],
                    fully_revoked: true,
                });
            }
            Ok(RevokeReport {
                plugin_id: plugin_id.clone(),
                revoked: vec![cap.clone()],
                fully_revoked: false,
            })
        } else {
            let revoked: Vec<CapabilityId> = rec.granted.iter().cloned().collect();
            self.records.remove(&key);
            // Persist denial marker so hostile packages cannot re-prompt in a loop;
            // re-grant requires explicit user action.
            self.denials.insert(key);
            Ok(RevokeReport {
                plugin_id: plugin_id.clone(),
                revoked,
                fully_revoked: true,
            })
        }
    }

    /// Revoke all grants for `plugin_id` and report what was removed.
    pub fn revoke_all(&mut self, plugin_id: &PluginId) -> Result<RevokeReport, PluginError> {
        self.revoke(plugin_id, None)
    }

    /// Whether `plugin_id` has a denial marker (re-prompts blocked until explicit action).
    #[must_use]
    pub fn is_denied(&self, plugin_id: &PluginId) -> bool {
        self.denials.contains(plugin_id.as_str())
    }

    /// Whether `capability` was individually revoked for `plugin_id` (CTX-0465).
    ///
    /// Re-prompting or re-granting a denied capability requires explicit
    /// [`GrantStore::clear_cap_denial`]; a bare `insert` never clears it.
    #[must_use]
    pub fn is_cap_denied(&self, plugin_id: &PluginId, capability: &CapabilityId) -> bool {
        self.denied_caps
            .get(plugin_id.as_str())
            .is_some_and(|set| set.contains(capability))
    }

    /// Clear a per-capability denial after explicit user action (CTX-0465).
    ///
    /// Returns `true` when a denial was present and removed. Re-granting the
    /// capability afterwards (via `insert`) then works; without this call the
    /// denial persists across `insert` calls so hostile packages cannot
    /// re-prompt revoked capabilities back into the grant in a loop.
    pub fn clear_cap_denial(&mut self, plugin_id: &PluginId, capability: &CapabilityId) -> bool {
        let key = plugin_id.as_str();
        let removed = self
            .denied_caps
            .get_mut(key)
            .is_some_and(|set| set.remove(capability));
        if removed && self.denied_caps.get(key).is_some_and(|set| set.is_empty()) {
            self.denied_caps.remove(key);
        }
        removed
    }

    /// Workspace narrowing: intersect `granted` with `workspace_allowed`, rejecting any addition.
    ///
    /// System policy cannot be weakened by user configuration, and workspace trust
    /// is weaker than user consent, so workspace configuration may narrow but never add.
    /// Returns error if `workspace_allowed` would add a capability not already in the grant.
    pub fn apply_workspace_narrowing(
        &self,
        plugin_id: &PluginId,
        workspace_allowed: &BTreeSet<CapabilityId>,
    ) -> Result<BTreeSet<CapabilityId>, PluginError> {
        let rec = self.get(plugin_id).ok_or_else(|| PluginError::NotFound {
            id: plugin_id.to_string(),
        })?;
        // Adding means workspace_allowed contains something not in granted.
        if !workspace_allowed.is_subset(&rec.granted) {
            return Err(PluginError::grant(
                "workspace may narrow grants but never add any",
            ));
        }
        Ok(workspace_allowed.clone())
    }

    /// Effective granted set after workspace narrowing (subset of stored grants).
    #[must_use]
    pub fn effective_grants(
        &self,
        plugin_id: &PluginId,
        workspace_allowed: Option<&BTreeSet<CapabilityId>>,
    ) -> BTreeSet<CapabilityId> {
        let base = self
            .get(plugin_id)
            .map(|r| r.granted.clone())
            .unwrap_or_default();
        if let Some(allowed) = workspace_allowed {
            base.intersection(allowed).cloned().collect()
        } else {
            base
        }
    }

    /// Clear all records (for tests; disposal path in real host).
    pub fn clear(&mut self) {
        self.records.clear();
        self.denials.clear();
        self.denied_caps.clear();
    }
}

/// Report of what was revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeReport {
    /// Plugin id.
    pub plugin_id: PluginId,
    /// Which capabilities were revoked.
    pub revoked: Vec<CapabilityId>,
    /// Whether the entire grant was removed.
    pub fully_revoked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityId;

    fn cap(s: &str) -> CapabilityId {
        CapabilityId::parse(s).unwrap()
    }

    #[test]
    fn grant_and_check() {
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let hash = "abc123";
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), hash, granted.clone(), 1));
        let declared = CapabilityRequests {
            ids: granted,
            ..CapabilityRequests::default()
        };

        assert!(store.is_granted(&pid, hash, &cap("terminal.semantic-read"), &declared));
        assert!(!store.is_granted(&pid, hash, &cap("clipboard.read"), &declared));
        // Wrong hash denies.
        assert!(!store.is_granted(&pid, "other", &cap("terminal.semantic-read"), &declared));
    }

    #[test]
    fn update_narrow_carries_forward() {
        let store = GrantStore::new();
        let old: BTreeSet<_> = [cap("terminal.semantic-read"), cap("ui.rich")]
            .into_iter()
            .collect();
        let narrowed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        let added: BTreeSet<_> = [cap("terminal.semantic-read"), cap("clipboard.read")]
            .into_iter()
            .collect();
        assert!(store.can_carry_forward(&old, &narrowed));
        assert!(!store.can_carry_forward(&old, &added));
        assert!(store.can_carry_forward(&old, &old));
    }

    #[test]
    fn revoke_single_capability() {
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h", granted, 1));
        let mut declared = CapabilityRequests::default();
        declared.ids.insert(cap("terminal.semantic-read"));

        let report = store.revoke(&pid, Some(&cap("ui.rich"))).unwrap();
        assert_eq!(report.revoked.len(), 1);
        assert!(!report.fully_revoked);
        assert!(!store.is_granted(&pid, "h", &cap("ui.rich"), &declared));
        assert!(store.is_granted(&pid, "h", &cap("terminal.semantic-read"), &declared));
    }

    #[test]
    fn revoke_single_capability_records_denial() {
        // CTX-0465: a single-capability revoke must persist an explicit
        // denial — no silent re-prompt / re-grant loop.
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h", granted.clone(), 1));
        let declared = CapabilityRequests {
            ids: granted.clone(),
            ..CapabilityRequests::default()
        };

        let report = store.revoke(&pid, Some(&cap("ui.rich"))).unwrap();
        assert_eq!(report.revoked, vec![cap("ui.rich")]);
        assert!(!report.fully_revoked);
        assert!(store.is_cap_denied(&pid, &cap("ui.rich")));
        assert!(!store.is_cap_denied(&pid, &cap("terminal.semantic-read")));
        assert!(!store.is_granted(&pid, "h", &cap("ui.rich"), &declared));
        assert!(store.is_granted(&pid, "h", &cap("terminal.semantic-read"), &declared));

        // A bare re-grant (no explicit clearance) must NOT resurrect the
        // revoked capability: the denial survives `insert`.
        store.insert(GrantRecord::granted(pid.clone(), "h", granted, 2));
        assert!(store.is_cap_denied(&pid, &cap("ui.rich")));
        assert!(!store.is_granted(&pid, "h", &cap("ui.rich"), &declared));

        // Explicit user action clears the denial; only then does re-grant work.
        assert!(store.clear_cap_denial(&pid, &cap("ui.rich")));
        assert!(!store.is_cap_denied(&pid, &cap("ui.rich")));
        assert!(!store.clear_cap_denial(&pid, &cap("ui.rich")));
        let mut granted2 = BTreeSet::new();
        granted2.insert(cap("terminal.semantic-read"));
        granted2.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h", granted2, 3));
        assert!(store.is_granted(&pid, "h", &cap("ui.rich"), &declared));
    }

    #[test]
    fn revoke_last_capability_escalates_to_full_denial() {
        // CTX-0465: revoking the final granted capability removes the record
        // and sets the plugin-level denial marker, exactly like a full revoke.
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h", granted, 1));

        let report = store.revoke(&pid, Some(&cap("ui.rich"))).unwrap();
        assert_eq!(report.revoked, vec![cap("ui.rich")]);
        assert!(report.fully_revoked);
        assert!(store.get(&pid).is_none());
        assert!(store.is_denied(&pid));
        assert!(store.is_cap_denied(&pid, &cap("ui.rich")));
    }

    #[test]
    fn revoke_all_sets_denial_marker() {
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h", granted, 1));
        store.revoke_all(&pid).unwrap();
        assert!(store.is_denied(&pid));
        assert!(store.get(&pid).is_none());
    }

    #[test]
    fn workspace_narrowing_never_adds() {
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        store.insert(GrantRecord::granted(pid.clone(), "h", granted, 1));

        // Narrowing to subset succeeds.
        let allowed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        assert!(store.apply_workspace_narrowing(&pid, &allowed).is_ok());

        // Adding via workspace fails.
        let adding: BTreeSet<_> = [cap("terminal.semantic-read"), cap("ui.rich")]
            .into_iter()
            .collect();
        assert!(store.apply_workspace_narrowing(&pid, &adding).is_err());

        // Empty narrowing succeeds (fully narrowed).
        let empty = BTreeSet::new();
        assert!(store.apply_workspace_narrowing(&pid, &empty).is_ok());
    }

    fn update_store() -> (GrantStore, PluginId) {
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h1", granted, 1));
        (store, pid)
    }

    fn declared(caps: &[&str]) -> CapabilityRequests {
        CapabilityRequests {
            ids: caps.iter().map(|s| cap(s)).collect(),
            ..CapabilityRequests::default()
        }
    }

    #[test]
    fn update_blocked_on_added_capability_without_approval() {
        // P0-AC-030: a silently broadened update blocks with no state change.
        let (mut store, pid) = update_store();
        let broadened: BTreeSet<_> = [
            cap("terminal.semantic-read"),
            cap("ui.rich"),
            cap("clipboard.read"),
        ]
        .into_iter()
        .collect();
        let err = store
            .apply_update(&pid, "h1", "h2", &broadened, false, 2)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("blocked pending explicit diff approval")
        );
        assert!(err.to_string().contains("clipboard.read"));
        // No state change: old record intact, new hash grants nothing.
        let rec = store.get(&pid).unwrap();
        assert_eq!(rec.manifest_hash, "h1");
        assert_eq!(rec.origin, GrantOrigin::ConsentUi);
        assert!(!store.is_granted(
            &pid,
            "h2",
            &cap("clipboard.read"),
            &declared(&["terminal.semantic-read", "ui.rich", "clipboard.read"])
        ));
    }

    #[test]
    fn update_approved_expansion_regrants() {
        let (mut store, pid) = update_store();
        let broadened: BTreeSet<_> = [
            cap("terminal.semantic-read"),
            cap("ui.rich"),
            cap("clipboard.read"),
        ]
        .into_iter()
        .collect();
        let outcome = store
            .apply_update(&pid, "h1", "h2", &broadened, true, 2)
            .unwrap();
        assert_eq!(outcome.added, vec![cap("clipboard.read")]);
        assert!(!outcome.carried_forward);
        let rec = store.get(&pid).unwrap();
        assert_eq!(rec.manifest_hash, "h2");
        assert_eq!(rec.origin, GrantOrigin::ConsentUi);
        assert!(!rec.denied);
        let new_declared = declared(&["terminal.semantic-read", "ui.rich", "clipboard.read"]);
        assert!(store.is_granted(&pid, "h2", &cap("clipboard.read"), &new_declared));
        // Old hash no longer grants.
        assert!(!store.is_granted(&pid, "h1", &cap("ui.rich"), &new_declared));
    }

    #[test]
    fn update_narrowing_carries_forward_silently() {
        // Downgrade path: fewer capabilities carry forward without approval.
        let (mut store, pid) = update_store();
        let narrowed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        let outcome = store
            .apply_update(&pid, "h1", "h2", &narrowed, false, 2)
            .unwrap();
        assert!(outcome.added.is_empty());
        assert!(outcome.carried_forward);
        let rec = store.get(&pid).unwrap();
        assert_eq!(rec.manifest_hash, "h2");
        assert_eq!(rec.origin, GrantOrigin::CarriedForward);
        let new_declared = declared(&["terminal.semantic-read"]);
        assert!(store.is_granted(&pid, "h2", &cap("terminal.semantic-read"), &new_declared));
        assert!(!store.is_granted(&pid, "h2", &cap("ui.rich"), &new_declared));
    }

    #[test]
    fn update_equal_set_carries_forward() {
        let (mut store, pid) = update_store();
        let same: BTreeSet<_> = [cap("terminal.semantic-read"), cap("ui.rich")]
            .into_iter()
            .collect();
        let outcome = store
            .apply_update(&pid, "h1", "h2", &same, false, 2)
            .unwrap();
        assert!(outcome.carried_forward);
        assert_eq!(store.get(&pid).unwrap().origin, GrantOrigin::CarriedForward);
    }

    #[test]
    fn update_stale_hash_fails_closed() {
        let (mut store, pid) = update_store();
        let narrowed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        let err = store
            .apply_update(&pid, "stale", "h2", &narrowed, true, 2)
            .unwrap_err();
        assert!(err.to_string().contains("stale update"));
        assert_eq!(store.get(&pid).unwrap().manifest_hash, "h1");
    }

    #[test]
    fn update_unknown_plugin_fails_closed() {
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.ghost").unwrap();
        let caps: BTreeSet<_> = [cap("ui.rich")].into_iter().collect();
        let err = store
            .apply_update(&pid, "h0", "h1", &caps, true, 1)
            .unwrap_err();
        assert!(matches!(err, PluginError::NotFound { .. }));
    }

    #[test]
    fn update_denied_plugin_requires_regrant() {
        // Even an approved update must not revive a denied plugin.
        let (mut store, pid) = update_store();
        store.revoke_all(&pid).unwrap();
        assert!(store.is_denied(&pid));
        let narrowed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        let err = store
            .apply_update(&pid, "h1", "h2", &narrowed, true, 2)
            .unwrap_err();
        assert!(err.to_string().contains("explicit re-grant required"));
        assert!(store.is_denied(&pid));
        assert!(store.get(&pid).is_none());
    }

    #[test]
    fn update_cannot_resurrect_cap_denial() {
        // CTX-0465 denials survive updates until explicitly cleared.
        let (mut store, pid) = update_store();
        store.revoke(&pid, Some(&cap("ui.rich"))).unwrap();
        assert!(store.is_cap_denied(&pid, &cap("ui.rich")));
        let with_revoked: BTreeSet<_> = [cap("terminal.semantic-read"), cap("ui.rich")]
            .into_iter()
            .collect();
        // Same-hash re-apply is not an update, and the hash moved anyway:
        // approved expansion carrying the revoked cap still fails.
        let err = store
            .apply_update(&pid, "h1", "h2", &with_revoked, true, 2)
            .unwrap_err();
        assert!(err.to_string().contains("explicit re-grant required"));
        // After explicit clearance the same approved update succeeds.
        assert!(store.clear_cap_denial(&pid, &cap("ui.rich")));
        let outcome = store
            .apply_update(&pid, "h1", "h2", &with_revoked, true, 3)
            .unwrap();
        assert!(!outcome.carried_forward);
        assert_eq!(outcome.added, vec![cap("ui.rich")]);
    }

    #[test]
    fn update_same_hash_requires_same_caps() {
        let (mut store, pid) = update_store();
        let same: BTreeSet<_> = [cap("terminal.semantic-read"), cap("ui.rich")]
            .into_iter()
            .collect();
        // Idempotent re-apply succeeds without rewriting the record.
        let outcome = store
            .apply_update(&pid, "h1", "h1", &same, false, 9)
            .unwrap();
        assert!(outcome.carried_forward);
        assert_eq!(store.get(&pid).unwrap().decided_at, 1);
        // Same-hash grant change is caller confusion: fail closed.
        let changed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        let err = store
            .apply_update(&pid, "h1", "h1", &changed, true, 9)
            .unwrap_err();
        assert!(err.to_string().contains("not an update"));
    }

    #[test]
    fn update_rollback_to_broader_set_requires_approval() {
        // Rollback is always possible, but a broader-than-current set is an
        // expansion no matter its history: it needs explicit approval.
        let (mut store, pid) = update_store();
        let narrowed: BTreeSet<_> = [cap("terminal.semantic-read")].into_iter().collect();
        store
            .apply_update(&pid, "h1", "h2", &narrowed, false, 2)
            .unwrap();
        let back: BTreeSet<_> = [cap("terminal.semantic-read"), cap("ui.rich")]
            .into_iter()
            .collect();
        // Unapproved rollback to the broader set blocks.
        assert!(
            store
                .apply_update(&pid, "h2", "h3", &back, false, 3)
                .is_err()
        );
        // Approved rollback restores.
        let outcome = store
            .apply_update(&pid, "h2", "h3", &back, true, 3)
            .unwrap();
        assert_eq!(outcome.added, vec![cap("ui.rich")]);
        let rollback_declared = declared(&["terminal.semantic-read", "ui.rich"]);
        assert!(store.is_granted(&pid, "h3", &cap("ui.rich"), &rollback_declared));
    }
}
