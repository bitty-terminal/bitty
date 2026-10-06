//! Core-owned capability contribution catalog (CTX-0916 slice S1, DEC-0102).
//!
//! S1 is purely additive with zero behavior change. The closed Core tables
//! still live in [`crate::manifest`] ([`CAPABILITY_FAMILIES`],
//! [`CLOSED_CAPABILITY_HEADS`], [`capability_requires_param`]) and every
//! pre-existing validation entry point delegates to
//! [`CapabilityCatalog::core`], which is seeded from exactly those tables.
//! Loaded extensions will contribute additional families, heads, and
//! parameter rules through [`CapabilityCatalog::register`] (later slices wire
//! the load-time call sites; S4 performs the single breaking removal of AI
//! families from the Core seed).
//!
//! Fail-closed rules for [`CapabilityCatalog::register`]:
//!
//! - Additive only: re-registering an existing head is a
//!   [`PackageError::Duplicate`] error, never an overwrite (a conflicting
//!   parameter rule is therefore also rejected, not merged).
//! - Every family and head is shape-validated exactly like manifest-time
//!   validation (segments, lengths, character classes, family prefix).
//! - Registered heads are bare identifiers: a `:` parameter is rejected.
//! - The whole call is validated before any mutation, so a failed call
//!   leaves the catalog unchanged.
//! - Per-call and total bounds ([`MAX_REGISTER_HEADS_PER_CALL`],
//!   [`MAX_CATALOG_HEADS`]) keep extension input bounded (Invariant 7).
//!
//! Enforcement math (grant intersection, delegation, trust passthrough) is
//! untouched: this catalog only owns the contribution tables. Shape rules
//! stay Core-owned in [`crate::manifest`].
//!
//! ```rust
//! use bitty_package::{CapabilityCatalog, CapabilityId};
//!
//! let core = CapabilityCatalog::core();
//! assert!(CapabilityId::parse_with(&core, "fs.read:/data/**").is_ok());
//!
//! let mut extended = CapabilityCatalog::core();
//! extended
//!     .register("acme", &[("acme.widget", false)])
//!     .unwrap();
//! assert!(CapabilityId::parse_with(&extended, "acme.widget").is_ok());
//! // The static paths still fail closed on unregistered contributions.
//! assert!(CapabilityId::new("acme.widget").is_err());
//! ```

use std::collections::{BTreeMap, BTreeSet};

use crate::error::PackageError;
use crate::manifest::{
    CAPABILITY_FAMILIES, CLOSED_CAPABILITY_HEADS, ClosedCapabilityViolation, MAX_CAPABILITY_LEN,
    capability_requires_param,
};

/// Maximum total capability heads one catalog may hold (Core seed plus
/// extension contributions). Bounds extension input (Invariant 7); raise only
/// by reviewed change.
pub const MAX_CATALOG_HEADS: usize = 256;
/// Maximum heads accepted by a single [`CapabilityCatalog::register`] call.
/// Mirrors [`crate::manifest::MAX_CAPABILITIES`] (one package requests at most
/// this many); extensions contributing more split across calls.
pub const MAX_REGISTER_HEADS_PER_CALL: usize = 64;
/// Maximum family label length. Mirrors the capability segment bound enforced
/// by manifest shape validation.
pub const MAX_FAMILY_LEN: usize = 64;
/// Maximum capability segment length for registered heads. Mirrors the segment
/// bound enforced by manifest shape validation.
pub const MAX_CAPABILITY_SEGMENT_LEN: usize = 64;

/// Core-owned contribution catalog for capability families.
///
/// Owns family membership, head membership, and parameter-presence rules.
/// Enforcement math stays with the callers; shape rules stay Core-owned in
/// [`crate::manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityCatalog {
    families: BTreeSet<String>,
    /// Head identifier -> whether a `:PARAMETER` is required.
    heads: BTreeMap<String, bool>,
}

impl CapabilityCatalog {
    /// Core seed: exactly the closed tables in [`crate::manifest`].
    ///
    /// Seeded from [`CLOSED_CAPABILITY_HEADS`] plus
    /// [`capability_requires_param`], so the static validation paths and this
    /// catalog accept exactly the same set (pinned by tests on both crates).
    #[must_use]
    pub fn core() -> Self {
        let mut catalog = Self {
            families: BTreeSet::new(),
            heads: BTreeMap::new(),
        };
        for family in CAPABILITY_FAMILIES {
            catalog.families.insert((*family).to_string());
        }
        for head in CLOSED_CAPABILITY_HEADS {
            catalog
                .heads
                .insert((*head).to_string(), capability_requires_param(head));
        }
        catalog
    }

    /// Families currently in the catalog, sorted.
    #[must_use]
    pub fn families(&self) -> Vec<&str> {
        self.families.iter().map(String::as_str).collect()
    }

    /// Capability heads currently in the catalog, sorted.
    #[must_use]
    pub fn heads(&self) -> Vec<&str> {
        self.heads.keys().map(String::as_str).collect()
    }

    /// Whether a family is known to this catalog.
    #[must_use]
    pub fn contains_family(&self, family: &str) -> bool {
        self.families.contains(family)
    }

    /// Whether a head is known to this catalog.
    #[must_use]
    pub fn contains_head(&self, head: &str) -> bool {
        self.heads.contains_key(head)
    }

    /// Parameter rule for a head, or `None` when the head is unknown
    /// (fail-closed lookup: callers must deny on `None`).
    #[must_use]
    pub fn requires_param(&self, head: &str) -> Option<bool> {
        self.heads.get(head).copied()
    }

    /// Number of heads currently in the catalog.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heads.len()
    }

    /// Whether the catalog holds no heads (the Core seed never is).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// Closed-set check against this catalog's tables.
    ///
    /// Unknown heads fail; known heads additionally enforce their parameter
    /// presence rule. [`CapabilityCatalog::core`] reproduces
    /// [`crate::manifest::check_closed_capability`] exactly.
    pub fn check(&self, head: &str, has_param: bool) -> Result<(), ClosedCapabilityViolation> {
        match self.heads.get(head) {
            None => Err(ClosedCapabilityViolation::UnknownHead),
            Some(true) if !has_param => Err(ClosedCapabilityViolation::ParamRequired),
            Some(false) if has_param => Err(ClosedCapabilityViolation::ParamForbidden),
            Some(_) => Ok(()),
        }
    }

    /// Additively register one family's heads (CTX-0916 extension hook).
    ///
    /// `family` names the contributing family; each `(head, requires_param)`
    /// entry declares one bare head (no `:PARAMETER`) belonging to that
    /// family. Fail-closed: anything invalid rejects the whole call and the
    /// catalog is left unchanged (see module docs).
    pub fn register(&mut self, family: &str, heads: &[(&str, bool)]) -> Result<(), PackageError> {
        validate_family_shape(family)?;
        if heads.is_empty() {
            return Err(PackageError::manifest(
                "capabilities",
                "registration must declare at least one head",
            ));
        }
        if heads.len() > MAX_REGISTER_HEADS_PER_CALL {
            return Err(PackageError::LimitExceeded {
                field: "capabilities.register".to_string(),
                limit: MAX_REGISTER_HEADS_PER_CALL,
                actual: heads.len(),
            });
        }
        if self.heads.len() + heads.len() > MAX_CATALOG_HEADS {
            return Err(PackageError::LimitExceeded {
                field: "capabilities.catalog".to_string(),
                limit: MAX_CATALOG_HEADS,
                actual: self.heads.len() + heads.len(),
            });
        }
        // Validate everything before mutating so a failed call leaves the
        // catalog unchanged.
        let mut seen_in_call = BTreeSet::new();
        for (head, _) in heads {
            validate_registered_head(family, head)?;
            if self.heads.contains_key(*head) {
                return Err(PackageError::Duplicate {
                    kind: "capability".to_string(),
                    value: (*head).to_string(),
                });
            }
            if !seen_in_call.insert(*head) {
                return Err(PackageError::Duplicate {
                    kind: "capability".to_string(),
                    value: (*head).to_string(),
                });
            }
        }
        self.families.insert(family.to_string());
        for (head, requires_param) in heads {
            self.heads.insert((*head).to_string(), *requires_param);
        }
        Ok(())
    }
}

impl Default for CapabilityCatalog {
    fn default() -> Self {
        Self::core()
    }
}

/// Family shape validation mirroring manifest segment rules: non-empty,
/// bounded, lowercase start, `[a-z0-9_-]` body.
fn validate_family_shape(family: &str) -> Result<(), PackageError> {
    if family.is_empty() {
        return Err(PackageError::manifest(
            "capabilities",
            "family must not be empty",
        ));
    }
    if family.len() > MAX_FAMILY_LEN {
        return Err(PackageError::LimitExceeded {
            field: "capabilities.family".to_string(),
            limit: MAX_FAMILY_LEN,
            actual: family.len(),
        });
    }
    let first = family.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return Err(PackageError::manifest(
            "capabilities",
            "family must start with lowercase letter",
        ));
    }
    for b in family.bytes() {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return Err(PackageError::manifest(
                "capabilities",
                "family must be [a-z0-9_-]",
            ));
        }
    }
    Ok(())
}

/// Registered-head shape validation mirroring manifest identifier rules, plus
/// the registration invariants: bare heads only, belonging to `family`.
fn validate_registered_head(family: &str, head: &str) -> Result<(), PackageError> {
    if head.is_empty() {
        return Err(PackageError::manifest(
            "capabilities",
            "capability head must not be empty",
        ));
    }
    if head.len() > MAX_CAPABILITY_LEN {
        return Err(PackageError::LimitExceeded {
            field: "capabilities".to_string(),
            limit: MAX_CAPABILITY_LEN,
            actual: head.len(),
        });
    }
    if head.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(PackageError::manifest(
            "capabilities",
            "capability head must not contain control characters or whitespace",
        ));
    }
    if head.contains(':') {
        return Err(PackageError::manifest(
            "capabilities",
            format!("registered head '{head}' must be bare (no ':PARAMETER')"),
        ));
    }
    if head.contains('*') {
        return Err(PackageError::manifest(
            "capabilities",
            "wildcards are not allowed in identifier head",
        ));
    }
    let parts: Vec<&str> = head.split('.').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(PackageError::manifest(
            "capabilities",
            "capability must be family.resource or family.resource.scope",
        ));
    }
    for seg in &parts {
        if seg.is_empty() {
            return Err(PackageError::manifest(
                "capabilities",
                "capability segment must not be empty",
            ));
        }
        if seg.len() > MAX_CAPABILITY_SEGMENT_LEN {
            return Err(PackageError::LimitExceeded {
                field: "capabilities.segment".to_string(),
                limit: MAX_CAPABILITY_SEGMENT_LEN,
                actual: seg.len(),
            });
        }
        let first = seg.as_bytes()[0];
        if !first.is_ascii_lowercase() {
            return Err(PackageError::manifest(
                "capabilities",
                "capability segment must start with lowercase letter",
            ));
        }
        for b in seg.bytes() {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
                return Err(PackageError::manifest(
                    "capabilities",
                    "capability segment must be [a-z0-9_-]",
                ));
            }
        }
    }
    if parts[0] != family {
        return Err(PackageError::manifest(
            "capabilities",
            format!("registered head '{head}' does not belong to family '{family}'"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        CAPABILITY_FAMILIES, CLOSED_CAPABILITY_HEADS, CapabilityId, check_closed_capability,
    };

    #[test]
    fn core_seed_matches_static_tables() {
        let core = CapabilityCatalog::core();
        assert!(!core.is_empty());

        let mut families = core.families();
        let mut expected_families = CAPABILITY_FAMILIES.to_vec();
        families.sort_unstable();
        expected_families.sort_unstable();
        assert_eq!(families, expected_families);

        let mut heads = core.heads();
        let mut expected_heads = CLOSED_CAPABILITY_HEADS.to_vec();
        heads.sort_unstable();
        expected_heads.sort_unstable();
        assert_eq!(heads, expected_heads);

        for head in CLOSED_CAPABILITY_HEADS {
            assert!(core.contains_head(head));
            assert_eq!(
                core.requires_param(head),
                Some(capability_requires_param(head)),
                "param rule diverged for '{head}'"
            );
        }
        assert_eq!(core.len(), CLOSED_CAPABILITY_HEADS.len());
        assert!(core.requires_param("terminal.unknown-thing").is_none());
    }

    #[test]
    fn old_and_new_agree_on_core_seed() {
        // Every closed head: old static path and catalog path both accept.
        let core = CapabilityCatalog::core();
        for head in CLOSED_CAPABILITY_HEADS {
            let raw = if capability_requires_param(head) {
                format!("{head}:param")
            } else {
                (*head).to_string()
            };
            assert!(
                CapabilityId::new(&raw).is_ok(),
                "old path must accept '{raw}'"
            );
            assert!(
                CapabilityId::parse_with(&core, &raw).is_ok(),
                "catalog path must accept '{raw}'"
            );
        }
        // Divergent identifiers: both reject, with the same violation kind.
        for (raw, head, has_param) in [
            ("terminal.unknown-thing", "terminal.unknown-thing", false),
            ("ui.unknown", "ui.unknown", false),
            ("agent.evil", "agent.evil", false),
            ("fs.read", "fs.read", false),
            ("env.read", "env.read", false),
            ("network.connect", "network.connect", false),
            (
                "terminal.semantic-read:param",
                "terminal.semantic-read",
                true,
            ),
            ("ui.rich:param", "ui.rich", true),
            ("ai.provider:extra", "ai.provider", true),
            ("mcp.invoke", "mcp.invoke", false),
        ] {
            assert!(
                CapabilityId::new(raw).is_err(),
                "old path must reject '{raw}'"
            );
            assert!(
                CapabilityId::parse_with(&core, raw).is_err(),
                "catalog path must reject '{raw}'"
            );
            assert_eq!(
                check_closed_capability(head, has_param),
                core.check(head, has_param),
                "violation kind diverged for '{raw}'"
            );
        }
    }

    #[test]
    fn register_adds_new_family_without_changing_static_paths() {
        // CTX-0916 S4 (DEC-0102): the AI families are the canonical additive
        // example — `ai.*`/`mcp.*`/`agent.*` validate ONLY through an
        // explicitly extended catalog (Core-only installs reject them
        // fail-closed; the static paths below still do).
        let mut catalog = CapabilityCatalog::core();
        assert!(!catalog.contains_family("ai"));
        catalog
            .register("ai", &[("ai.provider", false), ("ai.model", false)])
            .unwrap();
        catalog.register("mcp", &[("mcp.invoke", true)]).unwrap();
        catalog
            .register("agent", &[("agent.memory", true)])
            .unwrap();

        assert!(catalog.contains_family("ai"));
        assert!(catalog.contains_family("mcp"));
        assert!(catalog.contains_family("agent"));
        assert!(catalog.contains_head("ai.provider"));
        assert_eq!(catalog.requires_param("mcp.invoke"), Some(true));
        assert_eq!(catalog.requires_param("agent.memory"), Some(true));

        // New heads validate through the extended catalog ...
        assert!(CapabilityId::parse_with(&catalog, "ai.provider").is_ok());
        assert!(CapabilityId::parse_with(&catalog, "ai.model").is_ok());
        assert!(CapabilityId::parse_with(&catalog, "mcp.invoke:mail.list").is_ok());
        assert!(CapabilityId::parse_with(&catalog, "agent.memory:record-1").is_ok());
        assert!(CapabilityId::parse_with(&catalog, "mcp.invoke").is_err());
        assert!(CapabilityId::parse_with(&catalog, "agent.memory").is_err());
        assert!(CapabilityId::parse_with(&catalog, "ai.provider:param").is_err());

        // ... but the static paths still fail closed on them.
        assert!(CapabilityId::new("ai.provider").is_err());
        assert!(CapabilityId::new("mcp.invoke:mail.list").is_err());
        assert!(CapabilityId::new("agent.memory:record-1").is_err());
        assert!(check_closed_capability("ai.provider", false).is_err());
        assert!(check_closed_capability("mcp.invoke", true).is_err());

        // A fresh Core seed is unaffected by the extension.
        let fresh = CapabilityCatalog::core();
        assert!(!fresh.contains_family("ai"));
        assert!(!fresh.contains_family("mcp"));
        assert!(!fresh.contains_family("agent"));
        assert!(!fresh.contains_head("ai.provider"));
    }

    #[test]
    fn register_rejects_duplicates_without_mutation() {
        let mut catalog = CapabilityCatalog::core();
        let before = catalog.clone();

        // Re-registering a Core head is a duplicate, never an overwrite ...
        let err = catalog.register("fs", &[("fs.read", true)]).unwrap_err();
        assert!(matches!(err, PackageError::Duplicate { .. }));
        // ... even when the parameter rule disagrees (no silent merge).
        let err = catalog.register("fs", &[("fs.read", false)]).unwrap_err();
        assert!(matches!(err, PackageError::Duplicate { .. }));
        assert_eq!(catalog.requires_param("fs.read"), Some(true));

        // Duplicates within one call are rejected before any mutation.
        let err = catalog
            .register("acme", &[("acme.widget", false), ("acme.widget", false)])
            .unwrap_err();
        assert!(matches!(err, PackageError::Duplicate { .. }));
        assert!(!catalog.contains_family("acme"));

        assert_eq!(catalog, before);
    }

    #[test]
    fn register_rejects_bad_shapes_without_mutation() {
        let mut catalog = CapabilityCatalog::core();
        let before = catalog.clone();

        // Bad families.
        assert!(catalog.register("", &[("x.y", false)]).is_err());
        assert!(catalog.register("Acme", &[("Acme.widget", false)]).is_err());
        assert!(
            catalog
                .register("acme family", &[("acme.widget", false)])
                .is_err()
        );
        // Heads outside the named family.
        assert!(
            catalog
                .register("acme", &[("other.widget", false)])
                .is_err()
        );
        // Parameterized or malformed heads.
        assert!(
            catalog
                .register("acme", &[("acme.widget:param", false)])
                .is_err()
        );
        assert!(catalog.register("acme", &[("acme.*", false)]).is_err());
        assert!(catalog.register("acme", &[("acme", false)]).is_err());
        assert!(catalog.register("acme", &[("acme.Widget", false)]).is_err());
        assert!(
            catalog
                .register("acme", &[("acme.widget.extra.scope", false)])
                .is_err()
        );
        // Empty registration.
        assert!(catalog.register("acme", &[]).is_err());

        assert_eq!(catalog, before);
    }

    #[test]
    fn register_enforces_bounds() {
        let mut catalog = CapabilityCatalog::core();

        // Per-call bound.
        let owned: Vec<String> = (0..MAX_REGISTER_HEADS_PER_CALL + 1)
            .map(|i| format!("acme.head-{i}"))
            .collect();
        let refs: Vec<(&str, bool)> = owned.iter().map(|s| (s.as_str(), false)).collect();
        let err = catalog.register("acme", &refs).unwrap_err();
        assert!(matches!(err, PackageError::LimitExceeded { .. }));
        assert!(!catalog.contains_family("acme"));

        // Total bound: fill toward MAX_CATALOG_HEADS, then trip it.
        let mut n = 0;
        while catalog.len() + MAX_REGISTER_HEADS_PER_CALL <= MAX_CATALOG_HEADS {
            let family = format!("ext{n}");
            let owned: Vec<String> = (0..MAX_REGISTER_HEADS_PER_CALL)
                .map(|i| format!("ext{n}.head-{i}"))
                .collect();
            let refs: Vec<(&str, bool)> = owned.iter().map(|s| (s.as_str(), false)).collect();
            catalog.register(&family, &refs).unwrap();
            n += 1;
        }
        let remaining = MAX_CATALOG_HEADS - catalog.len() + 1;
        assert!(remaining > 1 && remaining <= MAX_REGISTER_HEADS_PER_CALL);
        let owned: Vec<String> = (0..remaining).map(|i| format!("final.head-{i}")).collect();
        let refs: Vec<(&str, bool)> = owned.iter().map(|s| (s.as_str(), false)).collect();
        let err = catalog.register("final", &refs).unwrap_err();
        assert!(matches!(err, PackageError::LimitExceeded { .. }));
        assert!(!catalog.contains_family("final"));
    }
}
