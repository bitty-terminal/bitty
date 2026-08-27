//! Project trust mechanics.
//!
//! Implements the candidate contract from RFC section “Project trust”,
//! inheriting the normative T-08 defense from the security corpus:
//!
//! 1. Project configuration is declarative-data-only; project-scope Lua
//!    execution is not a configuration-model feature. If a `.bitty.lua`-style
//!    file is ever honored, its content is data validated against a restricted
//!    project schema.
//! 2. Consent is bound to canonical path plus content hash; any content change
//!    invalidates prior approval.
//! 3. Proposed consent lifecycle per untrusted project config: ask once, ask
//!    always-on-entry, or deny, with deny as the default when origin detection
//!    is not positively local (R-020's restrictive `Unknown` rule).
//!
//! Open mechanics left to review (DB location/format, invalidation on
//! rename/move, expiry, prompt UX) are not fixed here; this module exposes
//! the smallest headless-testable surface that captures the hash binding
//! without claiming a final storage location.
//!
//! # Drift note
//!
//! The restricted project schema here is intentionally narrow. Expanding it
//! without a review would weaken the T-08 mitigation and must be gated on
//! an accepted RFC update.

use std::collections::HashMap;

use crate::error::ConfigError;
use crate::plan::ConfigPlan;

/// Consent lifecycle for a single project's declarative config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustDecision {
    /// Trust this content hash for this canonical path until the hash changes.
    TrustOnce,
    /// Trust this path persistently but still re-check on each entry; any hash
    /// change still invalidates and re-prompts.
    TrustAlways,
    /// Deny execution/use of this project's config.
    Deny,
}

impl std::fmt::Display for TrustDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::TrustOnce => "trust-once",
            Self::TrustAlways => "trust-always",
            Self::Deny => "deny",
        };
        f.write_str(s)
    }
}

/// A single grant bound to canonical path plus content hash.
///
/// Any content change invalidates prior approval (normative already).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    /// Canonical absolute path of the project root (e.g. `/home/alice/proj`).
    pub canonical_path: String,
    /// Hex digest (or opaque string) of the project's config content.
    pub content_hash: String,
    /// Decision for this binding.
    pub decision: TrustDecision,
}

impl TrustRecord {
    /// Create a trust record.
    pub fn new(
        canonical_path: impl Into<String>,
        content_hash: impl Into<String>,
        decision: TrustDecision,
    ) -> Self {
        Self {
            canonical_path: canonical_path.into(),
            content_hash: content_hash.into(),
            decision,
        }
    }

    /// Whether this record matches the given path and hash exactly.
    #[must_use]
    pub fn matches(&self, canonical_path: &str, content_hash: &str) -> bool {
        self.canonical_path == canonical_path && self.content_hash == content_hash
    }

    /// Whether this record *covers* the path but the hash has changed (stale).
    #[must_use]
    pub fn is_stale(&self, canonical_path: &str, content_hash: &str) -> bool {
        self.canonical_path == canonical_path && self.content_hash != content_hash
    }
}

/// In-memory trust store (headless). The durable location and format are an
/// RFC open item; this type is the pure-data shape so validation and trust
/// checks are testable without a filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrustStore {
    /// Records keyed by canonical path.
    records: HashMap<String, TrustRecord>,
}

impl TrustStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a record. Returns the previous record if any.
    pub fn insert(&mut self, record: TrustRecord) -> Option<TrustRecord> {
        self.records.insert(record.canonical_path.clone(), record)
    }

    /// Remove a record for a path.
    pub fn remove(&mut self, canonical_path: &str) -> Option<TrustRecord> {
        self.records.remove(canonical_path)
    }

    /// Look up a record for a path.
    #[must_use]
    pub fn get(&self, canonical_path: &str) -> Option<&TrustRecord> {
        self.records.get(canonical_path)
    }

    /// Check whether the given path+hash is trusted.
    ///
    /// Returns `true` only when a record exists with exactly this path and
    /// hash and the decision is `TrustOnce` or `TrustAlways`.
    /// `Deny` and missing records are untrusted. This preserves the RFC's
    /// "deny as default when origin is not positively local" — the caller
    /// should synthesize `Deny` for unknown origins before calling this, and
    /// this function correctly treats absence as untrusted.
    #[must_use]
    pub fn is_trusted(&self, canonical_path: &str, content_hash: &str) -> bool {
        match self.get(canonical_path) {
            Some(r) if r.matches(canonical_path, content_hash) => {
                matches!(
                    r.decision,
                    TrustDecision::TrustOnce | TrustDecision::TrustAlways
                )
            }
            _ => false,
        }
    }

    /// Whether the record for this path is stale due to a content change.
    #[must_use]
    pub fn is_stale(&self, canonical_path: &str, content_hash: &str) -> bool {
        match self.get(canonical_path) {
            Some(r) => r.is_stale(canonical_path, content_hash),
            None => false,
        }
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Iterate over records.
    pub fn iter(&self) -> impl Iterator<Item = &TrustRecord> {
        self.records.values()
    }
}

/// Restricted schema check for project configuration (declarative-only).
///
/// Project config is **not** allowed to introduce:
/// - `terminal.shell` (process authority),
/// - `plugins`, `keymaps` that could claim privileged actions,
/// - `extends` chains (to avoid confused-deputy profile loading),
/// - undeclared fields.
///
/// The allowed subset in this draft: `font`, `window`, `terminal.scrollback`,
/// `appearance`. Expanding this without review would weaken T-08 mitigation.
pub fn validate_project_plan(plan: &ConfigPlan) -> Result<(), ConfigError> {
    if plan.terminal.as_ref().is_some_and(|t| t.shell.is_some()) {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare terminal.shell".into(),
        });
    }
    if plan.plugins.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare plugins".into(),
        });
    }
    if plan.keymaps.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare keymaps".into(),
        });
    }
    if plan.extends.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare extends".into(),
        });
    }
    if !plan.undeclared_fields.is_empty() {
        return Err(ConfigError::UndeclaredField {
            field: plan.undeclared_fields[0].clone(),
            source: Some("project".into()),
        });
    }
    // Validate remaining allowed fields normally.
    if let Some(f) = &plan.font {
        f.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(w) = &plan.window {
        w.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(t) = &plan.terminal {
        // scrollback already bounded; just validate
        t.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(a) = &plan.appearance {
        a.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    Ok(())
}

/// Evaluate trust for a project path+hash against a store.
///
/// Returns `Ok(())` when trusted, `Err(TrustViolation)` otherwise, with a
/// diagnostic that distinguishes missing trust from stale hash.
pub fn check_trust(
    store: &TrustStore,
    canonical_path: &str,
    content_hash: &str,
) -> Result<(), ConfigError> {
    if store.is_trusted(canonical_path, content_hash) {
        Ok(())
    } else if store.is_stale(canonical_path, content_hash) {
        Err(ConfigError::TrustViolation {
            message: format!(
                "trust for '{canonical_path}' is stale: content hash changed, re-approve required"
            ),
        })
    } else {
        Err(ConfigError::TrustViolation {
            message: format!("no trust grant for '{canonical_path}'; explicit approval required"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ConfigPlan;
    use crate::types::TerminalConfig;

    #[test]
    fn trust_binding_hash_change_invalidates() {
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new(
            "/home/alice/proj",
            "abc123",
            TrustDecision::TrustOnce,
        ));
        assert!(store.is_trusted("/home/alice/proj", "abc123"));
        assert!(!store.is_trusted("/home/alice/proj", "deadbeef"));
        assert!(store.is_stale("/home/alice/proj", "deadbeef"));
        check_trust(&store, "/home/alice/proj", "abc123").expect("trusted");
        check_trust(&store, "/home/alice/proj", "deadbeef").unwrap_err();
    }

    #[test]
    fn deny_is_not_trusted() {
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new(
            "/home/alice/proj",
            "abc123",
            TrustDecision::Deny,
        ));
        assert!(!store.is_trusted("/home/alice/proj", "abc123"));
    }

    #[test]
    fn missing_is_not_trusted() {
        let store = TrustStore::new();
        assert!(!store.is_trusted("/unknown", "hash"));
        assert!(!store.is_stale("/unknown", "hash"));
    }

    #[test]
    fn project_plan_restricts_shell() {
        let plan = ConfigPlan {
            terminal: Some(TerminalConfig {
                scrollback: 5000,
                shell: Some("/bin/sh".into()),
            }),
            ..Default::default()
        };
        assert!(validate_project_plan(&plan).is_err());
    }

    #[test]
    fn project_plan_allows_font() {
        use crate::types::FontConfig;
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("font allowed in project");
    }

    #[test]
    fn project_plan_rejects_plugins() {
        use crate::types::PluginSpec;
        let plan = ConfigPlan {
            plugins: Some(vec![PluginSpec {
                id: "a/b".into(),
                enabled: true,
            }]),
            ..Default::default()
        };
        assert!(validate_project_plan(&plan).is_err());
    }

    #[test]
    fn project_plan_rejects_extends() {
        let plan = ConfigPlan {
            extends: Some("base".into()),
            ..Default::default()
        };
        assert!(validate_project_plan(&plan).is_err());
    }

    #[test]
    fn trust_store_insert_remove() {
        let mut s = TrustStore::new();
        assert!(s.is_empty());
        s.insert(TrustRecord::new("/a", "h1", TrustDecision::TrustAlways));
        assert_eq!(s.len(), 1);
        assert!(s.remove("/a").is_some());
        assert!(s.is_empty());
    }
}
