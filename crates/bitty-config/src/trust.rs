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
//! Open mechanics left to review (trust DB install location, invalidation
//! on rename/move, grant expiry, prompt UX, single-use consumption of
//! `TrustOnce`) are not fixed here; [`admit_project_layer`] is the single
//! enforcement point callers must route project plans through, and the
//! store exposes the smallest headless-testable surface that captures the
//! hash binding without claiming a final storage location.
//!
//! # Drift note
//!
//! The restricted project schema here is intentionally narrow. Expanding it
//! without a review would weaken the T-08 mitigation and must be gated on
//! an accepted RFC update.

use std::collections::HashMap;

use crate::error::ConfigError;
use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};

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

    /// Normalize a canonical path for comparison: trim ASCII whitespace and
    /// strip trailing `/` or `\` separators (except a lone root). Empty
    /// after trimming normalizes to empty (which never matches).
    #[must_use]
    pub fn normalize_path(canonical_path: &str) -> String {
        let trimmed = canonical_path.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let stripped = trimmed.trim_end_matches(['/', '\\']);
        if stripped.is_empty() {
            // Input was all separators (e.g. `/` root): keep one separator.
            trimmed[..1].to_string()
        } else {
            stripped.to_string()
        }
    }

    /// Normalize a content hash for comparison: trim whitespace and
    /// lowercase ASCII hex. Empty after trimming normalizes to empty
    /// (which never matches — an empty hash must not grant trust).
    #[must_use]
    pub fn normalize_hash(content_hash: &str) -> String {
        content_hash.trim().to_ascii_lowercase()
    }

    /// Whether this record matches the given path and hash under normalized
    /// comparison. Empty path or hash on either side never matches
    /// (fail-closed: an empty hash must not grant trust).
    #[must_use]
    pub fn matches(&self, canonical_path: &str, content_hash: &str) -> bool {
        let want_path = Self::normalize_path(canonical_path);
        let want_hash = Self::normalize_hash(content_hash);
        if want_path.is_empty() || want_hash.is_empty() {
            return false;
        }
        let have_path = Self::normalize_path(&self.canonical_path);
        let have_hash = Self::normalize_hash(&self.content_hash);
        if have_path.is_empty() || have_hash.is_empty() {
            return false;
        }
        have_path == want_path && have_hash == want_hash
    }

    /// Whether this record *covers* the path but the hash has changed (stale).
    ///
    /// Path comparison is normalized like [`Self::matches`]; an empty path
    /// never counts as covered. A hash difference — including an empty
    /// candidate or stored hash — is stale rather than trusted.
    #[must_use]
    pub fn is_stale(&self, canonical_path: &str, content_hash: &str) -> bool {
        let want_path = Self::normalize_path(canonical_path);
        if want_path.is_empty() {
            return false;
        }
        let have_path = Self::normalize_path(&self.canonical_path);
        if have_path.is_empty() || have_path != want_path {
            return false;
        }
        let want_hash = Self::normalize_hash(content_hash);
        let have_hash = Self::normalize_hash(&self.content_hash);
        have_hash != want_hash
    }
}

/// In-memory trust store (headless) with an explicit durable file form.
pub const MAX_TRUST_RECORDS: usize = 1024;
/// Maximum bytes for one persisted trust file (fail-closed).
pub const MAX_TRUST_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrustStore {
    /// Records keyed by normalized canonical path.
    records: HashMap<String, TrustRecord>,
}

impl TrustStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a record. Returns the previous record if any.
    ///
    /// The map key is the normalized path so `…/proj` and `…/proj/`
    /// alias; the stored record keeps its original spelling for
    /// diagnostics while comparison always normalizes.
    pub fn insert(&mut self, record: TrustRecord) -> Option<TrustRecord> {
        let key = TrustRecord::normalize_path(&record.canonical_path);
        self.records.insert(key, record)
    }

    /// Remove a record for a path (normalized lookup).
    pub fn remove(&mut self, canonical_path: &str) -> Option<TrustRecord> {
        let key = TrustRecord::normalize_path(canonical_path);
        if key.is_empty() {
            return None;
        }
        self.records.remove(&key)
    }

    /// Look up a record for a path (normalized lookup).
    #[must_use]
    pub fn get(&self, canonical_path: &str) -> Option<&TrustRecord> {
        let key = TrustRecord::normalize_path(canonical_path);
        if key.is_empty() {
            return None;
        }
        self.records.get(&key)
    }

    /// Check whether the given path+hash is trusted.
    ///
    /// Returns `true` only when a record exists with normalized path and
    /// hash equality and the decision is `TrustOnce` or `TrustAlways`.
    /// Empty path or hash never grants trust (fail-closed); `Deny` and
    /// missing records are untrusted. This preserves the RFC's
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

    /// Serialize to the durable line form (`path \t hash \t decision` per
    /// line). Paths/hashes must not contain `\n`, `\r`, or `\t`; decisions
    /// are the [`TrustDecision`] display spellings. Fail-closed on
    /// oversized stores.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the store exceeds
    /// [`MAX_TRUST_RECORDS`] or a field contains a separator.
    pub fn serialize(&self) -> Result<String, ConfigError> {
        if self.records.len() > MAX_TRUST_RECORDS {
            return Err(ConfigError::validation(
                "trust",
                format!("must contain <= {MAX_TRUST_RECORDS} records"),
            ));
        }
        let mut records: Vec<&TrustRecord> = self.records.values().collect();
        records.sort_by(|a, b| {
            TrustRecord::normalize_path(&a.canonical_path)
                .cmp(&TrustRecord::normalize_path(&b.canonical_path))
        });
        let mut out = String::new();
        for r in records {
            for field in [&r.canonical_path, &r.content_hash] {
                if field.contains(['\n', '\r', '\t']) {
                    return Err(ConfigError::validation(
                        "trust",
                        "path and hash must not contain tab or newline",
                    ));
                }
            }
            out.push_str(&r.canonical_path);
            out.push('\t');
            out.push_str(&r.content_hash);
            out.push('\t');
            out.push_str(&r.decision.to_string());
            out.push('\n');
        }
        Ok(out)
    }

    /// Parse the durable line form produced by [`Self::serialize`].
    /// Unknown decisions, malformed lines, empty paths/hashes, and
    /// oversized inputs fail closed. Later lines win on duplicate
    /// normalized paths.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on malformed input or bound violations.
    pub fn deserialize(text: &str) -> Result<Self, ConfigError> {
        if text.len() > MAX_TRUST_FILE_BYTES {
            return Err(ConfigError::validation(
                "trust",
                format!("trust file must be <= {MAX_TRUST_FILE_BYTES} bytes"),
            ));
        }
        let mut store = Self::new();
        if text.trim().is_empty() {
            return Ok(store);
        }
        for (idx, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() != 3 {
                return Err(ConfigError::validation(
                    "trust",
                    format!("malformed trust record on line {}", idx + 1),
                ));
            }
            let decision = match parts[2].trim() {
                "trust-once" => TrustDecision::TrustOnce,
                "trust-always" => TrustDecision::TrustAlways,
                "deny" => TrustDecision::Deny,
                other => {
                    return Err(ConfigError::validation(
                        "trust",
                        format!("unknown trust decision '{other}' on line {}", idx + 1),
                    ));
                }
            };
            if store.len() >= MAX_TRUST_RECORDS {
                return Err(ConfigError::validation(
                    "trust",
                    format!("must contain <= {MAX_TRUST_RECORDS} records"),
                ));
            }
            // CTX-0628: a grant for an empty path or hash can never match
            // (fail-closed at lookup), so persisting one is either corrupt
            // or hostile — reject it at parse time instead of storing junk.
            if TrustRecord::normalize_path(parts[0]).is_empty()
                || TrustRecord::normalize_hash(parts[1]).is_empty()
            {
                return Err(ConfigError::validation(
                    "trust",
                    format!("trust record on line {} has empty path or hash", idx + 1),
                ));
            }
            store.insert(TrustRecord::new(
                parts[0].to_string(),
                parts[1].to_string(),
                decision,
            ));
        }
        Ok(store)
    }

    /// Persist to `path` (creates parent dirs, fail-closed on I/O).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on serialization or filesystem failure.
    pub fn save_to_path(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        let text = self.serialize()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| ConfigError::InvalidInput {
                    message: format!("cannot create trust dir: {e}"),
                })?;
            }
        }
        std::fs::write(path, text).map_err(|e| ConfigError::InvalidInput {
            message: format!("cannot write trust file: {e}"),
        })
    }

    /// Load from `path`. A missing file yields an empty store (no trust);
    /// corrupt or oversized files fail closed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on I/O (other than missing file) or parse
    /// failure.
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, ConfigError> {
        // CTX-0628: pre-check size via metadata so a hostile oversized file
        // fails closed without allocating the read buffer first. The
        // post-read check in `deserialize` stays as the binding limit
        // (metadata is advisory under symlink races).
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > MAX_TRUST_FILE_BYTES as u64 {
                return Err(ConfigError::validation(
                    "trust",
                    format!("trust file must be <= {MAX_TRUST_FILE_BYTES} bytes"),
                ));
            }
        }
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(ConfigError::InvalidInput {
                message: format!("cannot read trust file: {e}"),
            }),
            Ok(text) => Self::deserialize(&text),
        }
    }
}

/// Restricted schema check for project configuration (declarative-only).
///
/// Project config is **not** allowed to introduce:
/// - `terminal.shell` (process authority),
/// - `plugins`, `keymaps` that could claim privileged actions,
/// - `extends` chains (to avoid confused-deputy profile loading),
/// - `profile_name` (would spoof the active profile identity at merge),
/// - undeclared fields.
///
/// The allowed subset in this draft: `font`, `window`, `layout`,
/// `terminal.scrollback`, `terminal.scroll_lines_per_notch`,
/// `terminal.scroll_pixels_per_notch`, `terminal.cursor_style`,
/// `terminal.bell` (CTX-0756: presentation-only chrome like scroll speed;
/// `bell` never grants OSC 9/777 notification permission, which stays
/// deny-by-default), `selection.auto_copy`,
/// `decoration` (geometry and the CTX-0340 outline colors), `scrollbar`,
/// `mouse`, `appearance`, `views` (CTX-0343 per-View appearance; like
/// `decoration`, presentation-only chrome, still grammar/bounds checked).
/// Every allowed section is bounds-validated here — including `layout`
/// (CTX-0628: previously documented as allowed but never checked, so
/// out-of-range gaps passed the project gate).
/// Expanding this without review would weaken T-08 mitigation.
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
    // CTX-0236: the mod rebinds the chrome map, so it is keymap-adjacent
    // and stays out of project layers with the keymaps themselves.
    if plan.mod_key.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare mod_key".into(),
        });
    }
    // CTX-0715: the leader arms the overlay session and reroutes follow-up
    // keys, so it is keymap-adjacent like the mod: a project-local file
    // must not be able to hijack it (or stretch its armed window).
    if plan.leader_key.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare leader_key".into(),
        });
    }
    if plan.leader_timeout_ms.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare leader_timeout_ms".into(),
        });
    }
    // CTX-0735: `hints_enabled` gates the same hint session the leader
    // arms, so it stays out of project layers with the leader itself: a
    // project-local file must not be able to silence hint chrome (or
    // re-enable what the user disabled).
    if plan.hints_enabled.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare hints_enabled".into(),
        });
    }
    // CTX-0370: close confirmation is a data-loss guard; a project-local
    // file must not be able to disable or weaken it, so the key stays out of
    // project layers (like the keymaps and the mod).
    if plan.close_confirm.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare close_confirm".into(),
        });
    }
    if plan.extends.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare extends".into(),
        });
    }
    // CTX-0628: a project layer's `profile_name` overwrites
    // `effective.profile` at merge (ScalarReplace), so an untrusted clone
    // could spoof the active profile identity and its attribution. Like
    // `extends`, it stays out of project layers.
    if plan.profile_name.is_some() {
        return Err(ConfigError::TrustViolation {
            message: "project config must not declare profile_name".into(),
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
    if let Some(l) = &plan.layout {
        // CTX-0628: `layout` gaps are presentation-only geometry with no
        // process authority (like `window` radius), so project layers may
        // set them — but the bounds still fail closed at the project gate.
        l.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(t) = &plan.terminal {
        // scrollback already bounded; just validate
        t.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(s) = &plan.selection {
        // CTX-0191: auto-copy is presentation/clipboard behavior with no
        // process authority (like scroll speed), so project layers may set it.
        s.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(d) = &plan.decoration {
        // CTX-0292: Core-owned decoration is presentation-only chrome with
        // no process authority (like the scrollbar), so project layers may
        // set it; the accepted ranges still fail closed.
        d.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(b) = &plan.scrollbar {
        // CTX-0181: the scrollbar is presentation-only chrome (like
        // auto-copy), so project layers may set it.
        b.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(m) = &plan.mouse {
        // CTX-0260: hover-focus is presentation-only chrome (like
        // auto-copy), so project layers may set it.
        m.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(a) = &plan.appearance {
        a.validate().map_err(|e| ConfigError::TrustViolation {
            message: e.to_string(),
        })?;
    }
    if let Some(v) = &plan.views {
        // CTX-0343: per-`View` appearance is presentation-only chrome (like
        // `decoration`), so a project layer may set it — but only through
        // this declared allowlist entry, and every entry still fails closed
        // on grammar, selector, and field bounds.
        for entry in v {
            entry.validate().map_err(|e| ConfigError::TrustViolation {
                message: e.to_string(),
            })?;
        }
    }
    Ok(())
}

/// Admit a project plan as a [`LayerKind::TrustedLocal`] layer.
///
/// This is the single enforcement point for R-010 project-config trust:
/// consent first, schema second, layer construction last. A plan from an
/// untrusted clone reaches the merge stack only when **both** hold:
/// 1. [`check_trust`] passes — an explicit Once/Always grant binds this
///    canonical path to this content hash (missing grants and stale hashes
///    fail closed here, before any content is inspected);
/// 2. [`validate_project_plan`] passes — the content is declarative-only
///    with no process/network/fs-write/runtime-admin authority.
///
/// The returned layer carries [`LayerKind::TrustedLocal`] precedence
/// (above `User`, below `Cli`) with source attribution pointing at the
/// project root, so `config show --source` traces project fields back to
/// the consented directory.
///
/// # Errors
///
/// Returns [`ConfigError::TrustViolation`] when consent is missing/stale
/// or the plan declares out-of-allowlist content.
///
/// [R-010]: https://github.com/bitty-terminal/bitty-docs (risk-register.md)
#[must_use = "an unadmitted project plan must not reach the merge stack"]
pub fn admit_project_layer(
    plan: ConfigPlan,
    store: &TrustStore,
    canonical_path: &str,
    content_hash: &str,
) -> Result<LayeredPlan, ConfigError> {
    check_trust(store, canonical_path, content_hash)?;
    validate_project_plan(&plan)?;
    Ok(LayeredPlan::new(
        ConfigSource::new(LayerKind::TrustedLocal, Some(canonical_path)),
        plan,
    ))
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
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(validate_project_plan(&plan).is_err());
    }

    #[test]
    fn project_plan_allows_scroll_speed() {
        // CTX-0185: scroll speed is presentation-only (no process authority
        // like `shell`), so project layers may set it.
        let plan = ConfigPlan {
            terminal: Some(TerminalConfig {
                scroll_lines_per_notch: 5,
                scroll_pixels_per_notch: 24,
                ..Default::default()
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("scroll speed allowed in project");
    }

    #[test]
    fn project_plan_allows_selection_auto_copy() {
        // CTX-0191: auto-copy is presentation/clipboard behavior with no
        // process authority (like scroll speed), so project layers may set it.
        use crate::types::SelectionConfig;
        let plan = ConfigPlan {
            selection: Some(SelectionConfig { auto_copy: false }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("auto-copy allowed in project");
    }

    #[test]
    fn project_plan_allows_mouse_focus_follows_mouse() {
        // CTX-0260/CTX-0334: hover-focus and its dwell delay are
        // presentation-only chrome with no process authority (like
        // auto-copy), so project layers may set them (within the bounds).
        use crate::types::MouseConfig;
        let plan = ConfigPlan {
            mouse: Some(MouseConfig {
                focus_follows_mouse: true,
                focus_follows_mouse_delay_ms: 120,
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("hover-focus allowed in project");
    }

    #[test]
    fn project_plan_allows_font() {
        use crate::types::FontConfig;
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("font allowed in project");
    }

    #[test]
    fn project_plan_allows_views() {
        // CTX-0343: `views` joins the declared project allowlist as
        // presentation-only chrome (like `decoration`); entries still fail
        // closed on bounds.
        use crate::types::{ViewAppearanceOverride, ViewOverride, ViewSelector};
        let plan = ConfigPlan {
            views: Some(vec![ViewOverride {
                selector: ViewSelector::Wildcard,
                overrides: ViewAppearanceOverride {
                    border_width_focused: Some(2),
                    ..Default::default()
                },
            }]),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("views allowed in project");
    }

    #[test]
    fn project_plan_rejects_out_of_range_view_width() {
        use crate::types::{ViewAppearanceOverride, ViewOverride, ViewSelector};
        let plan = ConfigPlan {
            views: Some(vec![ViewOverride {
                selector: ViewSelector::Wildcard,
                overrides: ViewAppearanceOverride {
                    border_width: Some(crate::types::MAX_DECORATION_BORDER_WIDTH_PX + 1),
                    ..Default::default()
                },
            }]),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(matches!(err, ConfigError::TrustViolation { .. }), "{err:?}");
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
    fn project_plan_rejects_mod_key() {
        // CTX-0236: the mod rebinds the chrome map, so it stays out of
        // project layers with the keymaps themselves.
        use crate::keymap::ModKey;
        let plan = ConfigPlan {
            mod_key: Some(ModKey::Super),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("mod_key"));
    }

    #[test]
    fn project_plan_rejects_leader() {
        // CTX-0715: the leader reroutes follow-up keys (and its timeout
        // stretches the armed window), so both fields stay out of project
        // layers with the keymaps and the mod.
        use crate::keymap::Chord;
        let plan = ConfigPlan {
            leader_key: Some(Chord::parse("ctrl+q").expect("parses")),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("leader_key"));
        let plan = ConfigPlan {
            leader_timeout_ms: Some(2500),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("leader_timeout_ms"));
    }

    #[test]
    fn project_plan_rejects_hints_enabled() {
        // CTX-0735 (#981): the hint kill switch gates leader-armed chrome,
        // so it stays out of project layers with the leader fields.
        let plan = ConfigPlan {
            hints_enabled: Some(false),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("hints_enabled"));
    }

    #[test]
    fn project_plan_rejects_close_confirm() {
        // CTX-0370: close confirmation is a data-loss guard; a project-local
        // file must not be able to disable it.
        use crate::types::CloseConfirm;
        let plan = ConfigPlan {
            close_confirm: Some(CloseConfirm::Never),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("close_confirm"));
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
    fn project_plan_allows_window_radius_and_rejects_oob() {
        // CTX-0241 S0: `window` (incl. `radius_px`) is presentation-only
        // chrome with no process authority, so project layers may set it;
        // out-of-range values still fail closed via `WindowConfig::validate`.
        use crate::types::WindowConfig;
        let plan = ConfigPlan {
            window: Some(WindowConfig {
                opacity: 1.0,
                padding: 8,
                radius_px: 12,
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("radius allowed in project");
        let bad = ConfigPlan {
            window: Some(WindowConfig {
                opacity: 1.0,
                padding: 8,
                radius_px: crate::types::MAX_WINDOW_RADIUS_PX + 1,
            }),
            ..Default::default()
        };
        let err = validate_project_plan(&bad).unwrap_err();
        assert!(err.to_string().contains("window.radius_px"));
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

    #[test]
    fn trust_empty_hash_never_matches_hostile() {
        // CTX-0479: an empty stored or candidate hash must not grant
        // trust (previously pure `==` matched two empty hashes).
        let mut s = TrustStore::new();
        s.insert(TrustRecord::new("/proj", "", TrustDecision::TrustAlways));
        assert!(!s.is_trusted("/proj", ""));
        assert!(!s.is_trusted("/proj", "abc"));
        let mut s2 = TrustStore::new();
        s2.insert(TrustRecord::new("/proj", "abc", TrustDecision::TrustAlways));
        assert!(!s2.is_trusted("/proj", ""));
        assert!(!s2.is_trusted("", "abc"));
        assert!(!s2.is_trusted("   ", "abc"));
    }

    #[test]
    fn trust_normalized_comparison_hostile() {
        // CTX-0479: hash case/whitespace and path trailing separators
        // alias; forged casing must not bypass, and slash variants must
        // resolve to the same grant.
        let mut s = TrustStore::new();
        s.insert(TrustRecord::new(
            "/proj",
            "ABC123",
            TrustDecision::TrustOnce,
        ));
        assert!(s.is_trusted("/proj", "abc123"));
        assert!(s.is_trusted("  /proj  ", "  ABC123  "));
        assert!(s.is_trusted("/proj/", "abc123"));
        // Stale detection still fires on real content change.
        assert!(s.is_stale("/proj/", "deadbeef"));
        assert!(!s.is_trusted("/proj/", "deadbeef"));
    }

    #[test]
    fn trust_durable_roundtrip_and_hostile_reject() {
        // CTX-0479: the store survives a save/load cycle (memory-only
        // forgets); corrupt lines and unknown decisions fail closed.
        let mut s = TrustStore::new();
        s.insert(TrustRecord::new("/a", "h1", TrustDecision::TrustAlways));
        s.insert(TrustRecord::new("/b", "h2", TrustDecision::Deny));
        let text = s.serialize().expect("serialize");
        let back = TrustStore::deserialize(&text).expect("roundtrip");
        assert!(back.is_trusted("/a", "h1"));
        assert!(!back.is_trusted("/b", "h2"));
        assert!(TrustStore::deserialize("no-tabs-here").is_err());
        assert!(TrustStore::deserialize("/a\th1\tgrant-forever").is_err());
        assert!(TrustStore::deserialize("").expect("empty ok").is_empty());
    }

    #[test]
    fn admit_project_layer_grants_trusted_valid_plan() {
        // CTX-0628 (R-010 closure): the gate admits a consented,
        // declarative-only plan as a `TrustedLocal` layer with attribution
        // pointing back at the project root.
        use crate::types::FontConfig;
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new("/proj", "hash1", TrustDecision::TrustOnce));
        let layer = admit_project_layer(plan, &store, "/proj", "hash1").expect("admitted");
        assert_eq!(layer.source.layer, LayerKind::TrustedLocal);
        assert!(
            layer.source.describe().contains("/proj"),
            "attribution traces to project: {}",
            layer.source.describe()
        );
    }

    #[test]
    fn admit_project_layer_rejects_untrusted_clone() {
        // CTX-0628: no grant, no layer — an untrusted clone never reaches
        // the merge stack, even with perfectly valid content.
        use crate::types::FontConfig;
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let store = TrustStore::new();
        let err = admit_project_layer(plan, &store, "/clone", "hash1").unwrap_err();
        assert!(matches!(err, ConfigError::TrustViolation { .. }), "{err:?}");
    }

    #[test]
    fn admit_project_layer_rejects_stale_hash() {
        // CTX-0628: consent binds path PLUS hash — edited content
        // invalidates the grant and re-prompts instead of merging.
        use crate::types::FontConfig;
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new(
            "/proj",
            "hash1",
            TrustDecision::TrustAlways,
        ));
        let err = admit_project_layer(plan, &store, "/proj", "hash2").unwrap_err();
        assert!(err.to_string().contains("stale"), "{err:?}");
    }

    #[test]
    fn admit_project_layer_rejects_hostile_content_despite_grant() {
        // CTX-0628: consent does not launder authority — a granted path
        // serving `terminal.shell` is still denied at the schema gate.
        let plan = ConfigPlan {
            terminal: Some(TerminalConfig {
                scrollback: 5000,
                shell: Some("/bin/sh".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new(
            "/proj",
            "hash1",
            TrustDecision::TrustAlways,
        ));
        let err = admit_project_layer(plan, &store, "/proj", "hash1").unwrap_err();
        assert!(matches!(err, ConfigError::TrustViolation { .. }), "{err:?}");
    }

    #[test]
    fn admit_project_layer_rejects_deny() {
        // CTX-0628: an explicit Reject (Deny) grant is never admitted.
        use crate::types::FontConfig;
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new("/proj", "hash1", TrustDecision::Deny));
        let err = admit_project_layer(plan, &store, "/proj", "hash1").unwrap_err();
        assert!(matches!(err, ConfigError::TrustViolation { .. }), "{err:?}");
    }

    #[test]
    fn project_plan_rejects_profile_name() {
        // CTX-0628: a project layer's `profile_name` overwrites
        // `effective.profile` at merge, so it stays out like `extends`.
        let plan = ConfigPlan {
            profile_name: Some("work".into()),
            ..Default::default()
        };
        let err = validate_project_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("profile_name"), "{err:?}");
    }

    #[test]
    fn project_plan_layout_bounds_fail_closed() {
        // CTX-0628: `layout` was documented as allowed but never checked —
        // out-of-range gaps passed the project gate. Bounds now fail
        // closed here, not just downstream.
        use crate::types::{LayoutConfig, MAX_LAYOUT_GAP_CELLS};
        let plan = ConfigPlan {
            layout: Some(LayoutConfig {
                gaps_in: 2,
                gaps_out: 2,
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("in-range layout allowed in project");
        let bad = ConfigPlan {
            layout: Some(LayoutConfig {
                gaps_in: MAX_LAYOUT_GAP_CELLS + 1,
                gaps_out: 0,
            }),
            ..Default::default()
        };
        let err = validate_project_plan(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::TrustViolation { .. }), "{err:?}");
    }

    #[test]
    fn trust_deserialize_rejects_empty_path_or_hash() {
        // CTX-0628: a grant for an empty path or hash can never match at
        // lookup, so persisting one is corrupt/hostile — fail closed here.
        assert!(TrustStore::deserialize("/proj\t\ttrust-always").is_err());
        assert!(TrustStore::deserialize("/proj\t   \tdeny").is_err());
        assert!(TrustStore::deserialize("\th1\tdeny").is_err());
        assert!(TrustStore::deserialize("   \th1\tdeny").is_err());
    }

    #[test]
    fn trust_fs_roundtrip_and_oversized_fail_closed() {
        // CTX-0628: the store survives a save/load cycle through the
        // filesystem; missing files grant nothing; oversized files fail
        // closed before granting anything.
        let dir = std::env::temp_dir().join(format!(
            "bitty-trust-ctx0628-{}-roundtrip",
            std::process::id()
        ));
        let path = dir.join("trust.db");
        let mut s = TrustStore::new();
        s.insert(TrustRecord::new(
            "/proj",
            "abc123",
            TrustDecision::TrustAlways,
        ));
        s.save_to_path(&path).expect("save");
        let back = TrustStore::load_from_path(&path).expect("load");
        assert!(back.is_trusted("/proj", "abc123"));
        let missing = TrustStore::load_from_path(&dir.join("absent.db")).expect("missing ok");
        assert!(missing.is_empty());
        let big_path = dir.join("big.db");
        std::fs::write(&big_path, vec![b'x'; MAX_TRUST_FILE_BYTES + 1]).expect("write big");
        assert!(TrustStore::load_from_path(&big_path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
