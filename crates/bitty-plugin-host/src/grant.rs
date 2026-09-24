//! Grant lifecycle (OQ-012, part 2).
//!
//! Persisted grant records bind `(plugin-id, manifest-hash)` to the set of
//! granted capabilities. This module implements the full lifecycle: request,
//! consent, persistence, update, revocation, re-grant, and workspace
//! narrowing. Records persist as a versioned, bounded text file under the
//! host state directory ([`GrantStore::save`] / [`GrantStore::load`]);
//! hostile file contents fail closed and change nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

/// Explicit consent evidence for [`GrantStore::insert_with_consent`].
///
/// A value of this type is the call-site's attestation that the grant record
/// passed through an explicit user decision (host consent UX for first
/// installs, explicit re-grant action afterwards). It is deliberately not
/// `Default`: callers must write `GrantConsent::explicit(..)`, so no implicit
/// or ambient insert compiles without acknowledging consent. The attested
/// `decided_at` must match the record's `decided_at`; mismatched evidence is
/// rejected fail-closed so one consent cannot be replayed onto another record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantConsent {
    decided_at: u64,
}

impl GrantConsent {
    /// Attest an explicit user decision made at `decided_at` (monotonic host
    /// time, same clock as [`GrantRecord::decided_at`]).
    #[must_use]
    pub fn explicit(decided_at: u64) -> Self {
        Self { decided_at }
    }
}

impl GrantOrigin {
    /// Codec token for this origin (stable wire value in the persisted file).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConsentUi => "consent-ui",
            Self::Revoked => "revoked",
            Self::Denied => "denied",
            Self::CarriedForward => "carried-forward",
        }
    }

    /// Parse a codec token produced by [`GrantOrigin::as_str`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "consent-ui" => Some(Self::ConsentUi),
            "revoked" => Some(Self::Revoked),
            "denied" => Some(Self::Denied),
            "carried-forward" => Some(Self::CarriedForward),
            _ => None,
        }
    }
}

/// Codec version of the persisted grant-store file (the only version accepted).
pub const GRANTS_STATE_VERSION: u32 = 1;
/// File name of the persisted grant store inside the host state directory.
pub const GRANTS_FILE_NAME: &str = "grants.toml";
/// Maximum persisted grant-store file size.
pub const MAX_GRANTS_FILE_BYTES: usize = 64 * 1024;
/// Maximum plugin sections in one persisted file.
pub const MAX_GRANTS_PLUGINS: usize = 256;
/// Maximum capabilities in one persisted capability list.
pub const MAX_GRANTS_CAPS: usize = 256;
/// Maximum persisted file lines.
pub const MAX_GRANTS_LINES: usize = 4096;
/// Maximum manifest-hash characters (the hash is opaque to this crate, bounded here).
pub const MAX_GRANT_HASH_CHARS: usize = 512;

/// Grant store: capability grants bound to manifest hash, with state-directory
/// persistence.
///
/// Invariants enforced:
///
/// - Undeclared authority cannot be exercised even if a stale grant record exists
///   (callers must intersect requested capabilities with grants; see [`GrantStore::is_granted`]).
/// - Direct [`GrantStore::insert`] is the explicit-consent path (first installs
///   and explicit re-grants only): production callers present [`GrantConsent`]
///   via [`GrantStore::insert_with_consent`], and updates go through
///   [`GrantStore::apply_update`], never a bare `insert`.
/// - Workspace configuration may narrow grants but may never add any.
/// - Revocation takes effect at the next dispatch boundary (host detaches handlers).
/// - Persistence ([`GrantStore::save`] / [`GrantStore::load`]) is fail-closed:
///   hostile file contents are rejected with a typed error and change nothing.
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

    /// Insert or replace a grant record (explicit-consent path).
    ///
    /// Scope: first installs and explicit re-grants only — the caller attests
    /// the record passed through the host consent UX. Updates to an existing
    /// grant must go through [`GrantStore::apply_update`] (which fails closed
    /// on unapproved additions), never a bare `insert`; production callers
    /// should prefer [`GrantStore::insert_with_consent`], which rejects
    /// missing consent evidence with a typed error. A bare `insert` never
    /// clears per-capability denials for enforcement: [`GrantStore::is_granted`]
    /// still fails closed until explicit [`GrantStore::clear_cap_denial`].
    pub fn insert(&mut self, record: GrantRecord) {
        let key = record.plugin_id.as_str().to_string();
        if record.denied {
            self.denials.insert(key.clone());
        } else {
            self.denials.remove(&key);
        }
        self.records.insert(key, record);
    }

    /// Insert or replace a grant record with explicit consent evidence.
    ///
    /// Consent-only fail-closed entry point: `None` (missing evidence) or
    /// evidence whose `decided_at` does not match the record is rejected with
    /// a typed [`PluginError::Grant`] and changes no state. On success this
    /// stores exactly what [`GrantStore::insert`] would store.
    pub fn insert_with_consent(
        &mut self,
        record: GrantRecord,
        consent: Option<GrantConsent>,
    ) -> Result<(), PluginError> {
        let Some(evidence) = consent else {
            return Err(PluginError::grant(format!(
                "explicit consent required to grant '{}'; ambient inserts are rejected",
                record.plugin_id.as_str()
            )));
        };
        if evidence.decided_at != record.decided_at {
            return Err(PluginError::grant(format!(
                "consent evidence mismatch for '{}': consent decided_at {} != record decided_at {}",
                record.plugin_id.as_str(),
                evidence.decided_at,
                record.decided_at
            )));
        }
        self.insert(record);
        Ok(())
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
    /// can never take effect through [`GrantStore::apply_update`] without
    /// explicit diff approval. A bare [`GrantStore::insert`] is the
    /// explicit-consent path for first installs and re-grants and must never
    /// carry an update.
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
        // (Denied plugins were already rejected above: `denials` before the
        // fetch, `current.denied` right after. No second check needed.)
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

    /// Render the store deterministically (sorted sections, fixed field
    /// order, sorted capability lists, trailing newline).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Grant`] when a stored value cannot be rendered
    /// safely (quotes, escapes, or control characters in an opaque hash).
    /// Validated ids and capabilities never contain those; the error is
    /// fail-closed defense in depth.
    pub fn render(&self) -> Result<String, PluginError> {
        let mut out = String::from(
            "# bitty grant store — machine-generated by the plugin host; do not edit by hand.\n",
        );
        out.push_str(&format!("grants_version = {GRANTS_STATE_VERSION}\n"));
        let mut ids: BTreeSet<&str> = self.records.keys().map(String::as_str).collect();
        ids.extend(self.denials.iter().map(String::as_str));
        ids.extend(self.denied_caps.keys().map(String::as_str));
        for id in ids {
            out.push('\n');
            writeln_quoted_section(&mut out, id)?;
            if let Some(record) = self.records.get(id) {
                writeln_quoted(&mut out, "manifest_hash", &record.manifest_hash)?;
                out.push_str(&format!("decided_at = {}\n", record.decided_at));
                writeln_quoted(&mut out, "origin", record.origin.as_str())?;
                out.push_str(&format!("denied = {}\n", record.denied));
                write_cap_list(&mut out, "granted", &record.granted)?;
            }
            let plugin_denied = self.denials.contains(id);
            out.push_str(&format!("plugin_denied = {plugin_denied}\n"));
            let empty = BTreeSet::new();
            let cap_denied = self.denied_caps.get(id).unwrap_or(&empty);
            write_cap_list(&mut out, "cap_denied", cap_denied)?;
        }
        return Ok(out);

        fn writeln_quoted_section(out: &mut String, id: &str) -> Result<(), PluginError> {
            check_renderable("plugin id", id)?;
            use std::fmt::Write as _;
            let _ = writeln!(out, "[grants.\"{id}\"]");
            Ok(())
        }

        fn writeln_quoted(out: &mut String, key: &str, value: &str) -> Result<(), PluginError> {
            check_renderable(key, value)?;
            use std::fmt::Write as _;
            let _ = writeln!(out, "{key} = \"{value}\"");
            Ok(())
        }

        fn write_cap_list(
            out: &mut String,
            key: &str,
            caps: &BTreeSet<CapabilityId>,
        ) -> Result<(), PluginError> {
            use std::fmt::Write as _;
            out.push_str(key);
            out.push_str(" = [");
            for (index, capability) in caps.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                check_renderable(key, capability.as_str())?;
                let _ = write!(out, "\"{}\"", capability.as_str());
            }
            out.push_str("]\n");
            Ok(())
        }

        fn check_renderable(what: &str, value: &str) -> Result<(), PluginError> {
            if value.contains(['"', '\\']) || value.chars().any(char::is_control) {
                return Err(PluginError::grant(format!(
                    "grant store: {what} cannot be rendered safely (quotes, escapes, or control characters)"
                )));
            }
            Ok(())
        }
    }

    /// Parse persisted text, fail-closed on anything outside the strict
    /// bounded subset [`GrantStore::render`] writes.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Grant`] for over-limit files, unknown/duplicate
    /// sections or keys, malformed values, missing required keys, an
    /// unsupported `grants_version`, or invalid plugin/capability identifiers.
    /// A failed parse changes nothing: the caller keeps its current store.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        if text.len() > MAX_GRANTS_FILE_BYTES {
            return Err(PluginError::grant(format!(
                "grant store exceeds {MAX_GRANTS_FILE_BYTES} bytes"
            )));
        }
        let mut store = Self::new();
        let mut version_seen = false;
        let mut section: Option<SectionBuilder> = None;
        for (index, raw_line) in text.lines().enumerate() {
            let line = index + 1;
            if line > MAX_GRANTS_LINES {
                return Err(PluginError::grant(format!(
                    "line {line}: grant store exceeds {MAX_GRANTS_LINES} lines"
                )));
            }
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(header) = trimmed.strip_prefix('[') {
                if let Some(open) = section.take() {
                    open.flush(&mut store)?;
                }
                let header = header.strip_suffix(']').ok_or_else(|| {
                    PluginError::grant(format!(
                        "line {line}: section header is missing the closing `]`"
                    ))
                })?;
                section = Some(SectionBuilder::open(header, line)?);
                continue;
            }
            let Some(open) = section.as_mut() else {
                let (key, value) = split_assignment(trimmed, line)?;
                if key != "grants_version" {
                    return Err(PluginError::grant(format!(
                        "line {line}: expected `grants_version` before any section, found '{key}'"
                    )));
                }
                if version_seen {
                    return Err(PluginError::grant(format!(
                        "line {line}: duplicate `grants_version`"
                    )));
                }
                if value != GRANTS_STATE_VERSION.to_string() {
                    return Err(PluginError::grant(format!(
                        "line {line}: unsupported grants_version '{value}' (want {GRANTS_STATE_VERSION})"
                    )));
                }
                version_seen = true;
                continue;
            };
            let (key, value) = split_assignment(trimmed, line)?;
            open.set(key, value, line)?;
        }
        if let Some(open) = section.take() {
            open.flush(&mut store)?;
        }
        if !version_seen {
            return Err(PluginError::grant(format!(
                "line 1: missing `grants_version = {GRANTS_STATE_VERSION}`"
            )));
        }
        let sections = store.records.len() + store.extra_sections();
        if sections > MAX_GRANTS_PLUGINS {
            return Err(PluginError::grant(format!(
                "line 1: grant store exceeds {MAX_GRANTS_PLUGINS} plugins"
            )));
        }
        Ok(store)
    }

    /// Load the store from `path`, verifying every record before replacing
    /// anything in memory.
    ///
    /// A missing file loads an empty store (first run); any other I/O
    /// failure, over-limit file, non-UTF-8 bytes, or hostile contents fail
    /// closed with a typed error.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Grant`] when the file cannot be read (other
    /// than not-found) or its contents are rejected by [`GrantStore::parse`].
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::new());
            }
            Err(error) => {
                return Err(PluginError::grant(format!(
                    "grant store: cannot read '{}': {error}",
                    path.display()
                )));
            }
        };
        if bytes.len() > MAX_GRANTS_FILE_BYTES {
            return Err(PluginError::grant(format!(
                "grant store: '{}' exceeds {MAX_GRANTS_FILE_BYTES} bytes",
                path.display()
            )));
        }
        let text = std::str::from_utf8(&bytes).map_err(|error| {
            PluginError::grant(format!(
                "grant store: '{}' is not UTF-8: {error}",
                path.display()
            ))
        })?;
        Self::parse(text).map_err(|error| {
            PluginError::grant(format!(
                "grant store: invalid grant store '{}': {error}",
                path.display()
            ))
        })
    }

    /// Persist the store to `path` atomically (temp file in the same
    /// directory, owner-only mode on Unix, rename last).
    ///
    /// The rendered bytes are re-parsed before any write, so this module
    /// never writes bytes it cannot read back. Rename-over-existing is
    /// Windows-safe (destination removed first).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Grant`] when rendering fails, the parent
    /// directory cannot be created, or the write/rename fails.
    pub fn save(&self, path: &Path) -> Result<(), PluginError> {
        let rendered = self.render()?;
        // Never write bytes this module cannot read back (defense in depth).
        Self::parse(&rendered).map_err(|error| {
            PluginError::grant(format!(
                "grant store: internal error: rendered store invalid: {error}"
            ))
        })?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    PluginError::grant(format!(
                        "grant store: cannot create '{}': {error}",
                        parent.display()
                    ))
                })?;
                harden_grants_dir(parent)?;
            }
        }
        let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&temp_path, rendered.as_bytes()).map_err(|error| {
            PluginError::grant(format!(
                "grant store: cannot write '{}': {error}",
                temp_path.display()
            ))
        })?;
        harden_grants_file(&temp_path)?;
        // Rename-over-existing fails on Windows; remove first (Windows-safe).
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(PluginError::grant(format!(
                    "grant store: cannot replace '{}': {error}",
                    path.display()
                )));
            }
        }
        std::fs::rename(&temp_path, path).map_err(|error| {
            let _ = std::fs::remove_file(&temp_path);
            PluginError::grant(format!(
                "grant store: cannot rename '{}' to '{}': {error}",
                temp_path.display(),
                path.display()
            ))
        })
    }

    /// Number of plugin sections a load produced beyond stored records
    /// (denial-only sections); used for the section-count bound.
    fn extra_sections(&self) -> usize {
        let mut extra = 0usize;
        for id in self.denials.iter().chain(self.denied_caps.keys()) {
            if !self.records.contains_key(id) {
                extra += 1;
            }
        }
        extra
    }
}

/// Default grant-store path under an XDG state home.
///
/// Resolves `$XDG_STATE_HOME/bitty/grants.toml` (empty values ignored),
/// falling back to `$HOME/.local/state/bitty/grants.toml`. Takes explicit
/// environment values so callers stay hermetic (tests pass values in);
/// returns `None` when neither root is available.
#[must_use]
pub fn grants_path_for(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_state_home {
        Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let home = home.filter(|home| !home.trim().is_empty())?;
            PathBuf::from(home).join(".local").join("state")
        }
    };
    Some(base.join("bitty").join(GRANTS_FILE_NAME))
}

/// Owner-only mode for the grant-store file (Unix).
#[cfg(unix)]
const GRANTS_FILE_MODE: u32 = 0o600;

/// Owner-only mode for the directory holding the grant store (Unix).
#[cfg(unix)]
const GRANTS_DIR_MODE: u32 = 0o700;

/// Force owner-only permissions on the grant-store file.
///
/// The store is a capability-grant ledger; `std::fs::write` inherits the
/// process umask, so the mode is set explicitly (defense in depth, mirroring
/// the CLI managed manifest). Non-Unix targets have no equivalent mode bits
/// here and no-op.
#[cfg(unix)]
fn harden_grants_file(path: &Path) -> Result<(), PluginError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(GRANTS_FILE_MODE)).map_err(
        |error| {
            PluginError::grant(format!(
                "grant store: cannot set mode {GRANTS_FILE_MODE:03o} on '{}': {error}",
                path.display()
            ))
        },
    )
}

/// Force owner-only permissions on the directory holding the grant store.
#[cfg(unix)]
fn harden_grants_dir(path: &Path) -> Result<(), PluginError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(GRANTS_DIR_MODE)).map_err(
        |error| {
            PluginError::grant(format!(
                "grant store: cannot set mode {GRANTS_DIR_MODE:03o} on '{}': {error}",
                path.display()
            ))
        },
    )
}

#[cfg(not(unix))]
fn harden_grants_file(_path: &Path) -> Result<(), PluginError> {
    Ok(())
}

#[cfg(not(unix))]
fn harden_grants_dir(_path: &Path) -> Result<(), PluginError> {
    Ok(())
}

/// One `[grants."<id>"]` section under construction during [`GrantStore::parse`].
struct SectionBuilder {
    id: String,
    line: usize,
    manifest_hash: Option<String>,
    decided_at: Option<u64>,
    origin: Option<GrantOrigin>,
    denied: Option<bool>,
    granted: Option<BTreeSet<CapabilityId>>,
    plugin_denied: Option<bool>,
    cap_denied: Option<BTreeSet<CapabilityId>>,
}

impl SectionBuilder {
    fn open(header: &str, line: usize) -> Result<Self, PluginError> {
        let header = header.trim();
        let inner = header.strip_prefix("grants.").ok_or_else(|| {
            PluginError::grant(format!("line {line}: expected section `[grants.\"<id>\"]`"))
        })?;
        let id = parse_quoted(inner, line)?;
        PluginId::new(&id).map_err(|error| {
            PluginError::grant(format!("line {line}: invalid plugin id '{id}': {error}"))
        })?;
        Ok(Self {
            id,
            line,
            manifest_hash: None,
            decided_at: None,
            origin: None,
            denied: None,
            granted: None,
            plugin_denied: None,
            cap_denied: None,
        })
    }

    fn set(&mut self, key: &str, value: &str, line: usize) -> Result<(), PluginError> {
        match key {
            "manifest_hash" => {
                reject_duplicate(self.manifest_hash.is_some(), key, line)?;
                let parsed = parse_quoted(value, line)?;
                if parsed.is_empty()
                    || parsed.len() > MAX_GRANT_HASH_CHARS
                    || parsed
                        .chars()
                        .any(|ch| ch.is_whitespace() || ch.is_control())
                {
                    return Err(PluginError::grant(format!(
                        "line {line}: manifest_hash must be 1..={MAX_GRANT_HASH_CHARS} non-whitespace characters"
                    )));
                }
                self.manifest_hash = Some(parsed);
            }
            "decided_at" => {
                reject_duplicate(self.decided_at.is_some(), key, line)?;
                let parsed: u64 = value.parse().map_err(|_| {
                    PluginError::grant(format!(
                        "line {line}: decided_at must be an unsigned integer, found '{value}'"
                    ))
                })?;
                self.decided_at = Some(parsed);
            }
            "origin" => {
                reject_duplicate(self.origin.is_some(), key, line)?;
                let parsed = parse_quoted(value, line)?;
                let origin = GrantOrigin::parse(&parsed).ok_or_else(|| {
                    PluginError::grant(format!(
                        "line {line}: unknown origin '{parsed}' (want consent-ui|revoked|denied|carried-forward)"
                    ))
                })?;
                self.origin = Some(origin);
            }
            "denied" => {
                reject_duplicate(self.denied.is_some(), key, line)?;
                self.denied = Some(parse_bool(value, key, line)?);
            }
            "granted" => {
                reject_duplicate(self.granted.is_some(), key, line)?;
                self.granted = Some(parse_capability_array(value, line)?);
            }
            "plugin_denied" => {
                reject_duplicate(self.plugin_denied.is_some(), key, line)?;
                self.plugin_denied = Some(parse_bool(value, key, line)?);
            }
            "cap_denied" => {
                reject_duplicate(self.cap_denied.is_some(), key, line)?;
                self.cap_denied = Some(parse_capability_array(value, line)?);
            }
            other => {
                return Err(PluginError::grant(format!(
                    "line {line}: unknown key '{other}' in grants section"
                )));
            }
        }
        Ok(())
    }

    fn flush(self, store: &mut GrantStore) -> Result<(), PluginError> {
        if store.records.contains_key(&self.id)
            || store.denials.contains(&self.id)
            || store.denied_caps.contains_key(&self.id)
        {
            return Err(PluginError::grant(format!(
                "line {}: duplicate section for plugin '{}'",
                self.line, self.id
            )));
        }
        // The record fields are all-or-none: a section carries either a full
        // record (revocable grant) or only denial markers (fully revoked).
        let has_record = self.manifest_hash.is_some()
            || self.decided_at.is_some()
            || self.origin.is_some()
            || self.denied.is_some()
            || self.granted.is_some();
        if has_record {
            let manifest_hash = self.manifest_hash.ok_or_else(|| {
                PluginError::grant(format!(
                    "line {}: plugin '{}' is missing `manifest_hash`",
                    self.line, self.id
                ))
            })?;
            let decided_at = self.decided_at.ok_or_else(|| {
                PluginError::grant(format!(
                    "line {}: plugin '{}' is missing `decided_at`",
                    self.line, self.id
                ))
            })?;
            let origin = self.origin.ok_or_else(|| {
                PluginError::grant(format!(
                    "line {}: plugin '{}' is missing `origin`",
                    self.line, self.id
                ))
            })?;
            let denied = self.denied.ok_or_else(|| {
                PluginError::grant(format!(
                    "line {}: plugin '{}' is missing `denied`",
                    self.line, self.id
                ))
            })?;
            let granted = self.granted.ok_or_else(|| {
                PluginError::grant(format!(
                    "line {}: plugin '{}' is missing `granted`",
                    self.line, self.id
                ))
            })?;
            let plugin_id = PluginId::new(&self.id).map_err(|error| {
                PluginError::grant(format!(
                    "line {}: invalid plugin id '{}': {error}",
                    self.line, self.id
                ))
            })?;
            store.records.insert(
                self.id.clone(),
                GrantRecord {
                    plugin_id,
                    manifest_hash,
                    granted,
                    decided_at,
                    origin,
                    denied,
                },
            );
        }
        if self.plugin_denied.unwrap_or(false) {
            store.denials.insert(self.id.clone());
        }
        if let Some(cap_denied) = self.cap_denied {
            if !cap_denied.is_empty() {
                store.denied_caps.insert(self.id, cap_denied);
            }
        }
        Ok(())
    }
}

fn split_assignment(line: &str, line_no: usize) -> Result<(&str, &str), PluginError> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| PluginError::grant(format!("line {line_no}: expected `key = value`")))?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(PluginError::grant(format!(
            "line {line_no}: expected `key = value`"
        )));
    }
    Ok((key, value))
}

fn reject_duplicate(seen: bool, key: &str, line: usize) -> Result<(), PluginError> {
    if seen {
        return Err(PluginError::grant(format!(
            "line {line}: duplicate key '{key}'"
        )));
    }
    Ok(())
}

fn parse_quoted(value: &str, line: usize) -> Result<String, PluginError> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| {
            PluginError::grant(format!("line {line}: expected a double-quoted value"))
        })?;
    if inner.contains(['"', '\\']) || inner.chars().any(char::is_control) {
        return Err(PluginError::grant(format!(
            "line {line}: quoted value must not contain quotes, escapes, or control characters"
        )));
    }
    Ok(inner.to_string())
}

fn parse_bool(value: &str, key: &str, line: usize) -> Result<bool, PluginError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(PluginError::grant(format!(
            "line {line}: {key} must be true|false, found '{other}'"
        ))),
    }
}

fn parse_capability_array(value: &str, line: usize) -> Result<BTreeSet<CapabilityId>, PluginError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| {
            PluginError::grant(format!(
                "line {line}: capability list must be an array of quoted capabilities"
            ))
        })?;
    let inner = inner.trim();
    let mut out = BTreeSet::new();
    if inner.is_empty() {
        return Ok(out);
    }
    for element in inner.split(',') {
        let element = element.trim();
        let capability_text = parse_quoted(element, line)?;
        let capability = CapabilityId::parse(&capability_text).map_err(|error| {
            PluginError::grant(format!(
                "line {line}: invalid capability '{capability_text}': {error}"
            ))
        })?;
        if !out.insert(capability) {
            return Err(PluginError::grant(format!(
                "line {line}: duplicate capability '{capability_text}'"
            )));
        }
        if out.len() > MAX_GRANTS_CAPS {
            return Err(PluginError::grant(format!(
                "line {line}: capability list exceeds {MAX_GRANTS_CAPS} entries"
            )));
        }
    }
    Ok(out)
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

    #[test]
    fn insert_without_consent_is_rejected() {
        // CTX-0657: the consent-gated path fails closed without evidence.
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        let err = store
            .insert_with_consent(GrantRecord::granted(pid.clone(), "h", granted, 1), None)
            .unwrap_err();
        assert!(matches!(err, PluginError::Grant { .. }));
        assert!(err.to_string().contains("explicit consent required"));
        assert!(store.get(&pid).is_none());
    }

    #[test]
    fn insert_with_consent_stores_grant() {
        // CTX-0657: matching evidence is the working consent path.
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        store
            .insert_with_consent(
                GrantRecord::granted(pid.clone(), "h", granted.clone(), 7),
                Some(GrantConsent::explicit(7)),
            )
            .unwrap();
        let declared = CapabilityRequests {
            ids: granted,
            ..CapabilityRequests::default()
        };
        assert!(store.is_granted(&pid, "h", &cap("ui.rich"), &declared));
    }

    #[test]
    fn insert_with_mismatched_consent_is_rejected() {
        // Evidence is bound to the record: one consent cannot be replayed
        // onto a different record.
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        let err = store
            .insert_with_consent(
                GrantRecord::granted(pid.clone(), "h", granted, 1),
                Some(GrantConsent::explicit(2)),
            )
            .unwrap_err();
        assert!(matches!(err, PluginError::Grant { .. }));
        assert!(err.to_string().contains("mismatch"));
        assert!(store.get(&pid).is_none());
    }

    #[test]
    fn bare_insert_remains_first_install_consent_path() {
        // CTX-0657 review constraint: `insert` stays pub because first-install
        // consent needs it; updates still go through `apply_update`.
        let mut store = GrantStore::new();
        let pid = PluginId::new("xuepoo.test").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(pid.clone(), "h1", granted.clone(), 1));
        let declared = CapabilityRequests {
            ids: granted,
            ..CapabilityRequests::default()
        };
        assert!(store.is_granted(&pid, "h1", &cap("ui.rich"), &declared));
    }

    fn persist_store() -> (GrantStore, PluginId) {
        // A store exercising every persisted shape: a live grant, a
        // per-capability denial, and a fully revoked (denial-only) plugin.
        let mut store = GrantStore::new();
        let live = PluginId::new("xuepoo.live").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(live.clone(), "deadbeef", granted, 7));
        let partial = PluginId::new("xuepoo.partial").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(
            partial.clone(),
            "cafef00d",
            granted,
            9,
        ));
        store.revoke(&partial, Some(&cap("ui.rich"))).unwrap();
        let gone = PluginId::new("xuepoo.gone").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("ui.rich"));
        store.insert(GrantRecord::granted(gone.clone(), "badc0de", granted, 3));
        store.revoke_all(&gone).unwrap();
        (store, live)
    }

    #[test]
    fn persist_round_trip_survives_restart() {
        // CTX-0765 (#1381): grants survive restart through the persisted file.
        let (store, live) = persist_store();
        let rendered = store.render().unwrap();
        let loaded = GrantStore::parse(&rendered).unwrap();
        assert_eq!(loaded.records, store.records);
        assert_eq!(loaded.denials, store.denials);
        assert_eq!(loaded.denied_caps, store.denied_caps);

        let live_declared = declared(&["terminal.semantic-read", "ui.rich"]);
        assert!(loaded.is_granted(&live, "deadbeef", &cap("ui.rich"), &live_declared));

        let partial = PluginId::new("xuepoo.partial").unwrap();
        let partial_declared = declared(&["terminal.semantic-read", "ui.rich"]);
        assert!(loaded.is_granted(
            &partial,
            "cafef00d",
            &cap("terminal.semantic-read"),
            &partial_declared
        ));
        assert!(!loaded.is_granted(&partial, "cafef00d", &cap("ui.rich"), &partial_declared));
        assert!(loaded.is_cap_denied(&partial, &cap("ui.rich")));

        let gone = PluginId::new("xuepoo.gone").unwrap();
        assert!(loaded.get(&gone).is_none());
        assert!(loaded.is_denied(&gone));
    }

    #[test]
    fn persist_denial_survives_bare_insert_after_reload() {
        // The deny-loop guard is durable: a reloaded denial still blocks a
        // bare re-grant until explicit clearance.
        let (store, _) = persist_store();
        let loaded = GrantStore::parse(&store.render().unwrap()).unwrap();
        let mut reloaded = loaded;
        let partial = PluginId::new("xuepoo.partial").unwrap();
        let mut granted = BTreeSet::new();
        granted.insert(cap("terminal.semantic-read"));
        granted.insert(cap("ui.rich"));
        reloaded.insert(GrantRecord::granted(
            partial.clone(),
            "cafef00d",
            granted,
            10,
        ));
        let declared = declared(&["terminal.semantic-read", "ui.rich"]);
        assert!(!reloaded.is_granted(&partial, "cafef00d", &cap("ui.rich"), &declared));
        assert!(reloaded.is_granted(
            &partial,
            "cafef00d",
            &cap("terminal.semantic-read"),
            &declared
        ));
    }

    #[test]
    fn persist_hostile_contents_fail_closed() {
        // CTX-0765 (#1381): hostile store contents are rejected and change nothing.
        let hostile = [
            "grants_version = 2\n",
            "state_version = 1\n",
            "grants_version = 1\ngrants_version = 1\n",
            "grants_version = 1\n[bogus.\"xuepoo.test\"]\n",
            "grants_version = 1\n[grants.\"nope-no-dot\"]\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\nunknown_key = true\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\nmanifest_hash = \"abc\"\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\nmanifest_hash = \"abc\"\nmanifest_hash = \"def\"\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\nmanifest_hash = \"\"\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\nmanifest_hash = \"a b\"\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\ndecided_at = -1\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\ndecided_at = 1.5\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\norigin = \"ambient\"\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\ndenied = yes\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\ngranted = [\"clipboard.everything\"]\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\ngranted = [\"ui.rich\", \"ui.rich\"]\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"]\nplugin_denied = true\n[grants.\"xuepoo.test\"]\nplugin_denied = false\n",
            "grants_version = 1\n[grants.\"xuepoo.test\"\n",
            "key without equals\n",
        ];
        for text in hostile {
            assert!(
                GrantStore::parse(text).is_err(),
                "hostile input must fail closed: {text:?}"
            );
        }
        // Missing version is rejected even with an otherwise valid section.
        let (store, _) = persist_store();
        let mut rendered = store.render().unwrap();
        rendered = rendered
            .lines()
            .filter(|line| !line.starts_with("grants_version"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(GrantStore::parse(&rendered).is_err());
    }

    #[test]
    fn persist_save_load_file_round_trip() {
        // Full lifecycle through the filesystem: save, reload into a fresh
        // store (restart), revoke, save again, reload, verify.
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx0765-grant-{}-save-load",
            std::process::id()
        ));
        let path = dir.join("grants.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let (store, live) = persist_store();
        store.save(&path).unwrap();

        let mut restarted = GrantStore::load(&path).unwrap();
        assert_eq!(restarted.records, store.records);
        assert_eq!(restarted.denials, store.denials);
        assert_eq!(restarted.denied_caps, store.denied_caps);

        restarted.revoke(&live, Some(&cap("ui.rich"))).unwrap();
        restarted.save(&path).unwrap();
        let reread = GrantStore::load(&path).unwrap();
        assert!(reread.is_cap_denied(&live, &cap("ui.rich")));
        let declared = declared(&["terminal.semantic-read", "ui.rich"]);
        assert!(!reread.is_granted(&live, "deadbeef", &cap("ui.rich"), &declared));
        assert!(reread.is_granted(&live, "deadbeef", &cap("terminal.semantic-read"), &declared));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_load_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx0765-grant-{}-missing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let loaded = GrantStore::load(&dir.join("grants.toml")).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn persist_load_rejects_non_utf8() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx0765-grant-{}-non-utf8",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.toml");
        std::fs::write(&path, [0xFF, 0xFE, b'x']).unwrap();
        assert!(GrantStore::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_grants_path_for() {
        assert_eq!(
            grants_path_for(Some("/state"), Some("/home/u")),
            Some(PathBuf::from("/state/bitty/grants.toml"))
        );
        assert_eq!(
            grants_path_for(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/bitty/grants.toml"))
        );
        assert_eq!(
            grants_path_for(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/bitty/grants.toml"))
        );
        assert_eq!(grants_path_for(None, None), None);
        assert_eq!(grants_path_for(Some(""), Some("")), None);
    }

    #[test]
    fn persist_origin_codec_round_trip() {
        for origin in [
            GrantOrigin::ConsentUi,
            GrantOrigin::Revoked,
            GrantOrigin::Denied,
            GrantOrigin::CarriedForward,
        ] {
            assert_eq!(GrantOrigin::parse(origin.as_str()), Some(origin));
        }
        assert_eq!(GrantOrigin::parse("ambient"), None);
    }
}
