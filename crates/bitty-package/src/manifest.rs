//! Package manifest types — draft per package-lifecycle RFC.
//!
//! The manifest is attacker-controlled input: cloned repository, typo-squat,
//! or tampered artifact. Every field is bounded before use (Invariant 7) and
//! unknown fields must be rejected.

use std::collections::BTreeSet;

use crate::error::PackageError;
use crate::integrity::{is_valid_hex_digest, sha256_hex};

// ── hard limits (proposed, tunable only by reviewed change) ─────────────

/// Maximum manifest size in bytes (256 KiB).
pub const MANIFEST_MAX_BYTES: usize = 256 * 1024;
/// Maximum package dependencies.
pub const MAX_DEPENDENCIES: usize = 32;
/// Maximum requested capabilities per package.
pub const MAX_CAPABILITIES: usize = 64;
/// Maximum total capability identifier text in bytes (8 KiB).
pub const MAX_CAPABILITY_TEXT_BYTES: usize = 8 * 1024;
/// Maximum plugin dependencies is reused from plugin-host but keep package-specific.
pub const MAX_PLUGIN_DEPS: usize = 8;
/// Maximum package id length.
pub const MAX_PACKAGE_ID_LEN: usize = 128;
/// Maximum display name length.
pub const MAX_NAME_LEN: usize = 128;
/// Maximum description length.
pub const MAX_DESCRIPTION_LEN: usize = 1024;
/// Maximum license expression length.
pub const MAX_LICENSE_LEN: usize = 256;
/// Maximum capability identifier length.
pub const MAX_CAPABILITY_LEN: usize = 256;

// ── package id ───────────────────────────────────────────────────────────

/// Owner-qualified stable package identifier, `owner.name`, e.g. `xuepoo.theme`.
///
/// Validation: `^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*$`, bounded length.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId(String);

impl PackageId {
    /// Parse and validate a package id.
    pub fn new(raw: &str) -> Result<Self, PackageError> {
        validate_package_id(raw)?;
        Ok(Self(raw.to_string()))
    }

    /// Raw id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Owner segment.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.split_once('.').map(|(a, _)| a).unwrap_or(&self.0)
    }

    /// Name segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.split_once('.').map(|(_, b)| b).unwrap_or("")
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for PackageId {
    type Err = PackageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

fn validate_package_id(raw: &str) -> Result<(), PackageError> {
    if raw.is_empty() {
        return Err(PackageError::manifest(
            "package.id",
            "package id must not be empty",
        ));
    }
    if raw.len() > MAX_PACKAGE_ID_LEN {
        return Err(PackageError::LimitExceeded {
            field: "package.id".to_string(),
            limit: MAX_PACKAGE_ID_LEN,
            actual: raw.len(),
        });
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return Err(PackageError::manifest(
            "package.id",
            "package id must not contain whitespace",
        ));
    }
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 2 {
        return Err(PackageError::manifest(
            "package.id",
            "package id must be exactly owner.name (one dot)",
        ));
    }
    for seg in &parts {
        if seg.is_empty() {
            return Err(PackageError::manifest(
                "package.id",
                "package id segment must not be empty",
            ));
        }
        if seg.len() > 64 {
            return Err(PackageError::LimitExceeded {
                field: "package.id.segment".to_string(),
                limit: 64,
                actual: seg.len(),
            });
        }
        let first = seg.as_bytes()[0];
        if !first.is_ascii_lowercase() {
            return Err(PackageError::manifest(
                "package.id",
                "segment must start with lowercase letter",
            ));
        }
        for b in seg.bytes() {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
                return Err(PackageError::manifest(
                    "package.id",
                    "segment must be [a-z0-9_-]",
                ));
            }
        }
    }
    Ok(())
}

// ── semver helpers ───────────────────────────────────────────────────────

fn validate_semver(raw: &str, field: &str) -> Result<(), PackageError> {
    if raw.is_empty() {
        return Err(PackageError::manifest(field, "version must not be empty"));
    }
    if raw.len() > 64 {
        return Err(PackageError::LimitExceeded {
            field: field.to_string(),
            limit: 64,
            actual: raw.len(),
        });
    }
    let core = raw.split(['-', '+']).next().unwrap_or(raw);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(PackageError::manifest(
            field,
            format!("version '{raw}' must be SemVer X.Y.Z"),
        ));
    }
    for part in parts {
        if part.is_empty() {
            return Err(PackageError::manifest(
                field,
                format!("version '{raw}' has empty numeric component"),
            ));
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PackageError::manifest(
                field,
                format!("version '{raw}' numeric components must be digits"),
            ));
        }
        if part.len() > 1 && part.starts_with('0') {
            return Err(PackageError::manifest(
                field,
                format!("version '{raw}' must not have leading zeros"),
            ));
        }
    }
    for b in raw.bytes() {
        if !(b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+' || b == b'_') {
            return Err(PackageError::manifest(
                field,
                format!("version '{raw}' contains invalid character"),
            ));
        }
    }
    Ok(())
}

fn validate_version_req(raw: &str, field: &str) -> Result<(), PackageError> {
    if raw.is_empty() {
        return Err(PackageError::manifest(
            field,
            "version requirement must not be empty",
        ));
    }
    if raw.len() > 128 {
        return Err(PackageError::LimitExceeded {
            field: field.to_string(),
            limit: 128,
            actual: raw.len(),
        });
    }
    // Closed grammar: allow only alphanumeric, whitespace, ., -, +, ,, <, >, =, ^, ~
    // Explicitly deny *, |, & and other shell characters.
    for b in raw.bytes() {
        if !(b.is_ascii_alphanumeric()
            || b.is_ascii_whitespace()
            || matches!(
                b,
                b'.' | b'-' | b'+' | b',' | b'<' | b'>' | b'=' | b'^' | b'~'
            ))
        {
            return Err(PackageError::manifest(
                field,
                format!("version requirement '{raw}' contains invalid character"),
            ));
        }
    }
    // Additional closed-grammar denials with explicit messages for audit.
    if raw.contains('*') {
        return Err(PackageError::manifest(
            field,
            "wildcard '*' is not allowed in v1",
        ));
    }
    if raw.contains('|') {
        return Err(PackageError::manifest(
            field,
            "disjunction '||' is not allowed in v1",
        ));
    }
    if raw.contains('&') {
        return Err(PackageError::manifest(field, "operator '&' is not allowed"));
    }
    Ok(())
}

// ── capability id ────────────────────────────────────────────────────────

/// Validated package capability identifier.
///
/// Grammar: `family.resource[.scope]` plus optional `:parameter` for
/// path/network constraints. Closed identifier set; unknown families or
/// malformed identifiers fail validation (deny-by-default, no wildcards in
/// head).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityId(String);

impl CapabilityId {
    /// Parse and validate a capability identifier.
    pub fn new(raw: &str) -> Result<Self, PackageError> {
        validate_capability(raw)?;
        Ok(Self(raw.to_string()))
    }

    /// Raw string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Family part.
    #[must_use]
    pub fn family(&self) -> &str {
        self.0.split(['.', ':']).next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_capability(raw: &str) -> Result<(), PackageError> {
    if raw.is_empty() {
        return Err(PackageError::manifest(
            "capabilities",
            "capability must not be empty",
        ));
    }
    if raw.len() > MAX_CAPABILITY_LEN {
        return Err(PackageError::LimitExceeded {
            field: "capabilities".to_string(),
            limit: MAX_CAPABILITY_LEN,
            actual: raw.len(),
        });
    }
    if raw.contains(' ') || raw.contains('\t') || raw.contains('\n') {
        return Err(PackageError::manifest(
            "capabilities",
            "capability must not contain whitespace",
        ));
    }
    // Split optional param.
    let (head, param) = match raw.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (raw, None),
    };
    if head.contains('*') {
        return Err(PackageError::manifest(
            "capabilities",
            "wildcards are not allowed in identifier head",
        ));
    }
    if let Some(p) = param {
        if p.is_empty() {
            return Err(PackageError::manifest(
                "capabilities",
                "parameter must not be empty",
            ));
        }
        if p.len() > 1024 {
            return Err(PackageError::LimitExceeded {
                field: "capabilities.param".to_string(),
                limit: 1024,
                actual: p.len(),
            });
        }
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
    let family = parts[0];
    const KNOWN_FAMILIES: &[&str] = &[
        "terminal",
        "ui",
        "clipboard",
        "fs",
        "process",
        "network",
        "runtime",
        "debug",
        "platform",
    ];
    if !KNOWN_FAMILIES.contains(&family) {
        return Err(PackageError::manifest(
            "capabilities",
            format!("unknown capability family '{family}'"),
        ));
    }
    Ok(())
}

// ── manifest structs ─────────────────────────────────────────────────────

/// Identity block `[package]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIdentity {
    /// Owner-qualified id.
    pub id: PackageId,
    /// Display name (bounded, untrusted).
    pub name: String,
    /// SemVer 2 version.
    pub version: String,
    /// Short description (bounded).
    pub description: String,
    /// SPDX license expression, if present.
    pub license: Option<String>,
}

impl PackageIdentity {
    /// Validate this identity.
    pub fn validate(&self) -> Result<(), PackageError> {
        // id already validated via PackageId::new.
        validate_semver(&self.version, "package.version")?;
        if self.name.trim().is_empty() {
            return Err(PackageError::manifest(
                "package.name",
                "must not be empty or whitespace",
            ));
        }
        if self.name.len() > MAX_NAME_LEN {
            return Err(PackageError::LimitExceeded {
                field: "package.name".to_string(),
                limit: MAX_NAME_LEN,
                actual: self.name.len(),
            });
        }
        if self.description.len() > MAX_DESCRIPTION_LEN {
            return Err(PackageError::LimitExceeded {
                field: "package.description".to_string(),
                limit: MAX_DESCRIPTION_LEN,
                actual: self.description.len(),
            });
        }
        if let Some(lic) = &self.license {
            if lic.len() > MAX_LICENSE_LEN {
                return Err(PackageError::LimitExceeded {
                    field: "package.license".to_string(),
                    limit: MAX_LICENSE_LEN,
                    actual: lic.len(),
                });
            }
            if lic.trim().is_empty() {
                return Err(PackageError::manifest(
                    "package.license",
                    "when present must not be empty",
                ));
            }
        }
        Ok(())
    }
}

/// Compatibility block `[compat]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Compat {
    /// Bitty version requirement, e.g. `>=0.5,<1.0`.
    pub bitty: Option<String>,
    /// Plugin API version requirement, e.g. `^1.0`.
    pub plugin_api: Option<String>,
}

impl Compat {
    /// Validate this compat block.
    pub fn validate(&self) -> Result<(), PackageError> {
        if let Some(b) = &self.bitty {
            validate_version_req(b, "compat.bitty")?;
        }
        if let Some(p) = &self.plugin_api {
            validate_version_req(p, "compat.plugin_api")?;
        }
        Ok(())
    }
}

/// A single package dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDependency {
    /// Required package id.
    pub id: PackageId,
    /// Version requirement.
    pub version_req: String,
    /// Whether this edge opts into prerelease selection (RFC §Prerelease policy).
    ///
    /// When false, prerelease candidates are excluded unless the requirement
    /// itself contains a prerelease identifier on the same X.Y.Z. When true,
    /// the edge explicitly allows prerelease.
    pub prerelease: bool,
}

impl PackageDependency {
    /// Validate this dependency.
    pub fn validate(&self) -> Result<(), PackageError> {
        validate_version_req(&self.version_req, "dependencies.version_req")?;
        Ok(())
    }

    /// Convenience constructor with `prerelease = false`.
    #[must_use]
    pub fn new(id: PackageId, version_req: impl Into<String>) -> Self {
        Self {
            id,
            version_req: version_req.into(),
            prerelease: false,
        }
    }

    /// Constructor with explicit prerelease flag.
    #[must_use]
    pub fn with_prerelease(
        id: PackageId,
        version_req: impl Into<String>,
        prerelease: bool,
    ) -> Self {
        Self {
            id,
            version_req: version_req.into(),
            prerelease,
        }
    }
}

/// Owned package manifest — draft candidate for `bitty-package.toml` or
/// `bitty.toml` `[package]`.
///
/// This is the semantic manifest that has already been parsed from bytes;
/// raw bytes length is retained for framing budgets, but the structure
/// itself is pure owned data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManifest {
    /// Identity.
    pub identity: PackageIdentity,
    /// Compatibility.
    pub compat: Compat,
    /// Dependencies (bounded).
    pub dependencies: Vec<PackageDependency>,
    /// Requested capabilities (bounded, closed grammar).
    pub capabilities: Vec<CapabilityId>,
    /// Optional plugin API version the package was built against.
    pub raw_bytes_len: usize,
    /// Unknown fields captured for rejection (must be empty to validate).
    pub undeclared_fields: Vec<String>,
}

impl PackageManifest {
    /// Validate the manifest as untrusted input (stage 3 of the integrity chain).
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.raw_bytes_len > MANIFEST_MAX_BYTES {
            return Err(PackageError::LimitExceeded {
                field: "manifest.bytes".to_string(),
                limit: MANIFEST_MAX_BYTES,
                actual: self.raw_bytes_len,
            });
        }
        if !self.undeclared_fields.is_empty() {
            return Err(PackageError::manifest(
                "manifest",
                format!("unknown field '{}'", self.undeclared_fields[0]),
            ));
        }
        self.identity.validate()?;
        self.compat.validate()?;
        if self.dependencies.len() > MAX_DEPENDENCIES {
            return Err(PackageError::LimitExceeded {
                field: "dependencies".to_string(),
                limit: MAX_DEPENDENCIES,
                actual: self.dependencies.len(),
            });
        }
        // Duplicate dependency detection.
        let mut seen_deps = BTreeSet::new();
        for d in &self.dependencies {
            d.validate()?;
            if !seen_deps.insert(d.id.as_str().to_string()) {
                return Err(PackageError::Duplicate {
                    kind: "dependency".to_string(),
                    value: d.id.to_string(),
                });
            }
            if d.id == self.identity.id {
                return Err(PackageError::manifest(
                    "dependencies",
                    "package must not depend on itself",
                ));
            }
        }
        if self.capabilities.len() > MAX_CAPABILITIES {
            return Err(PackageError::LimitExceeded {
                field: "capabilities".to_string(),
                limit: MAX_CAPABILITIES,
                actual: self.capabilities.len(),
            });
        }
        let mut seen_caps = BTreeSet::new();
        let mut total_text = 0usize;
        for c in &self.capabilities {
            // Each CapabilityId already validated length.
            total_text += c.as_str().len();
            if !seen_caps.insert(c.as_str().to_string()) {
                return Err(PackageError::Duplicate {
                    kind: "capability".to_string(),
                    value: c.as_str().to_string(),
                });
            }
        }
        if total_text > MAX_CAPABILITY_TEXT_BYTES {
            return Err(PackageError::LimitExceeded {
                field: "capabilities.text".to_string(),
                limit: MAX_CAPABILITY_TEXT_BYTES,
                actual: total_text,
            });
        }
        Ok(())
    }

    /// Canonical encoding for H-B (manifest hash binding, stage 4).
    ///
    /// Deterministic across platforms: sorted dependencies by id,
    /// sorted capabilities, field-order stable, no whitespace variance.
    /// The encoding is versioned; changing it forces lock migration.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Versioned header so canonicalization is frozen per format version.
        out.extend_from_slice(b"bitty-manifest-v1\n");
        out.extend_from_slice(self.identity.id.as_str().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(self.identity.version.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(self.identity.name.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(self.identity.description.as_bytes());
        out.push(b'\n');
        if let Some(lic) = &self.identity.license {
            out.extend_from_slice(lic.as_bytes());
        }
        out.push(b'\n');
        if let Some(b) = &self.compat.bitty {
            out.extend_from_slice(b"bitty:");
            out.extend_from_slice(b.as_bytes());
        }
        out.push(b'\n');
        if let Some(p) = &self.compat.plugin_api {
            out.extend_from_slice(b"plugin_api:");
            out.extend_from_slice(p.as_bytes());
        }
        out.push(b'\n');
        // Dependencies sorted.
        let mut deps = self.dependencies.clone();
        deps.sort_by(|a, b| a.id.cmp(&b.id));
        for d in deps {
            out.extend_from_slice(b"dep:");
            out.extend_from_slice(d.id.as_str().as_bytes());
            out.push(b'@');
            out.extend_from_slice(d.version_req.as_bytes());
            if d.prerelease {
                out.extend_from_slice(b":pre");
            }
            out.push(b'\n');
        }
        // Capabilities sorted.
        let mut caps: Vec<&str> = self.capabilities.iter().map(|c| c.as_str()).collect();
        caps.sort_unstable();
        for c in caps {
            out.extend_from_slice(b"cap:");
            out.extend_from_slice(c.as_bytes());
            out.push(b'\n');
        }
        out
    }

    /// Canonical manifest digest (H-B) — SHA-256 hex over canonical bytes.
    #[must_use]
    pub fn canonical_digest(&self) -> String {
        sha256_hex(&self.canonical_bytes())
    }

    /// Check canonical digest against expected hex (stage 4).
    pub fn verify_canonical_digest(&self, expected_hex: &str) -> Result<(), PackageError> {
        if !is_valid_hex_digest(expected_hex) {
            return Err(PackageError::Integrity {
                stage: "manifest_hash_binding".to_string(),
                message: format!("expected digest '{expected_hex}' is not valid 64-hex"),
            });
        }
        let actual = self.canonical_digest();
        if !actual.eq_ignore_ascii_case(expected_hex) {
            return Err(PackageError::ManifestHashMismatch {
                expected: expected_hex.to_ascii_lowercase(),
                actual,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_manifest() -> PackageManifest {
        PackageManifest {
            identity: PackageIdentity {
                id: PackageId::new("xuepoo.theme").unwrap(),
                name: "Theme".to_string(),
                version: "0.1.0".to_string(),
                description: "A theme package".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        }
    }

    #[test]
    fn valid_manifest_passes() {
        minimal_manifest().validate().unwrap();
    }

    #[test]
    fn invalid_id_rejected() {
        assert!(PackageId::new("INVALID").is_err());
        assert!(PackageId::new("owner.").is_err());
        assert!(PackageId::new(".name").is_err());
        assert!(PackageId::new("owner.name.extra").is_err());
    }

    #[test]
    fn semver_validation() {
        assert!(validate_semver("1.0.0", "field").is_ok());
        assert!(validate_semver("1.0", "field").is_err());
        assert!(validate_semver("01.0.0", "field").is_err());
    }

    #[test]
    fn unknown_field_rejected() {
        let mut m = minimal_manifest();
        m.undeclared_fields.push("unknown_key".to_string());
        assert!(m.validate().is_err());
    }

    #[test]
    fn bytes_limit_enforced() {
        let mut m = minimal_manifest();
        m.raw_bytes_len = MANIFEST_MAX_BYTES + 1;
        assert!(m.validate().is_err());
    }

    #[test]
    fn duplicate_dependency_rejected() {
        let mut m = minimal_manifest();
        m.dependencies.push(PackageDependency {
            id: PackageId::new("xuepoo.dep").unwrap(),
            version_req: "^1.0".to_string(),
            prerelease: false,
        });
        m.dependencies.push(PackageDependency {
            id: PackageId::new("xuepoo.dep").unwrap(),
            version_req: "^2.0".to_string(),
            prerelease: false,
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn duplicate_capability_rejected() {
        let mut m = minimal_manifest();
        m.capabilities.push(CapabilityId::new("fs.read").unwrap());
        m.capabilities.push(CapabilityId::new("fs.read").unwrap());
        assert!(m.validate().is_err());
    }

    #[test]
    fn canonical_determinism() {
        let m1 = minimal_manifest();
        let mut m2 = minimal_manifest();
        // Different capability order should still produce same canonical digest (sorted).
        m2.capabilities = vec![
            CapabilityId::new("network.connect").unwrap(),
            CapabilityId::new("fs.read").unwrap(),
        ];
        let mut m3 = minimal_manifest();
        m3.capabilities = vec![
            CapabilityId::new("fs.read").unwrap(),
            CapabilityId::new("network.connect").unwrap(),
        ];
        assert_eq!(m2.canonical_digest(), m3.canonical_digest());
        assert_ne!(m1.canonical_digest(), m2.canonical_digest());
    }

    #[test]
    fn canonical_differs_on_semantic_edit() {
        let mut m1 = minimal_manifest();
        let mut m2 = minimal_manifest();
        m1.capabilities.push(CapabilityId::new("fs.read").unwrap());
        m2.capabilities.push(CapabilityId::new("fs.write").unwrap());
        assert_ne!(m1.canonical_digest(), m2.canonical_digest());
    }

    #[test]
    fn self_dependency_rejected() {
        let mut m = minimal_manifest();
        m.dependencies.push(PackageDependency {
            id: PackageId::new("xuepoo.theme").unwrap(),
            version_req: "^1.0".to_string(),
            prerelease: false,
        });
        assert!(m.validate().is_err());
    }
}
