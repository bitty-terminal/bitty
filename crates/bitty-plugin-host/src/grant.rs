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
use crate::manifest::PluginId;

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
    ) -> bool {
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

    /// Revoke grants for `plugin_id`.
    ///
    /// If `capability` is `Some`, remove only that capability; otherwise remove
    /// the whole grant record. The host must detach affected handlers at the
    /// next dispatch boundary and report what was revoked.
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

        assert!(store.is_granted(&pid, hash, &cap("terminal.semantic-read")));
        assert!(!store.is_granted(&pid, hash, &cap("clipboard.read")));
        // Wrong hash denies.
        assert!(!store.is_granted(&pid, "other", &cap("terminal.semantic-read")));
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

        let report = store.revoke(&pid, Some(&cap("ui.rich"))).unwrap();
        assert_eq!(report.revoked.len(), 1);
        assert!(!report.fully_revoked);
        assert!(!store.is_granted(&pid, "h", &cap("ui.rich")));
        assert!(store.is_granted(&pid, "h", &cap("terminal.semantic-read")));
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
}
