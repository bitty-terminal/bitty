//! Manifest and identity model (OQ-012, part 1).
//!
//! Candidate `bitty-plugin.toml` schema per the proposed plugin-platform RFC.
//! Validation is total, side-effect free, and headless: no file I/O, no VM,
//! no network. The manifest is attacker-controlled input (cloned repo,
//! typo-squat) so every field is treated as untrusted display data and is
//! bounded before use.

use std::collections::BTreeSet;

use crate::capability::CapabilityId;
use crate::error::PluginError;

// ── hard limits (proposed, tunable only by reviewed change) ─────────────

/// Maximum manifest size in bytes (256 KiB).
pub const MANIFEST_MAX_BYTES: usize = 256 * 1024;
/// Maximum declared commands.
pub const MAX_COMMANDS: usize = 128;
/// Maximum subscribed event types.
pub const MAX_EVENT_TYPES: usize = 256;
/// Maximum filesystem patterns per access kind.
pub const MAX_FS_PATTERNS_PER_KIND: usize = 32;
/// Maximum provided services.
pub const MAX_PROVIDED_SERVICES: usize = 16;
/// Maximum plugin dependencies.
pub const MAX_DEPENDENCIES: usize = 8;
/// Maximum total pattern text in bytes (8 KiB).
pub const MAX_PATTERN_TEXT_BYTES: usize = 8 * 1024;
/// Maximum plugin ID length.
pub const MAX_PLUGIN_ID_LEN: usize = 128;
/// Maximum display name length.
pub const MAX_NAME_LEN: usize = 128;
/// Maximum description length.
pub const MAX_DESCRIPTION_LEN: usize = 1024;
/// Maximum license expression length.
pub const MAX_LICENSE_LEN: usize = 256;

// ── plugin id ────────────────────────────────────────────────────────────

/// Owner-qualified stable plugin identifier, `owner.name`, e.g. `xuepoo.markdown`.
///
/// Validation: `^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*$`, bounded length, globally unique
/// (package layer verifies publisher binding per source type — not done here).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(String);

impl PluginId {
    /// Parse and validate a plugin id.
    pub fn new(raw: &str) -> Result<Self, PluginError> {
        validate_plugin_id(raw)?;
        Ok(Self(raw.to_string()))
    }

    /// Raw id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Owner segment (before dot).
    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.split_once('.').map(|(a, _)| a).unwrap_or(&self.0)
    }

    /// Name segment (after dot).
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.split_once('.').map(|(_, b)| b).unwrap_or("")
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for PluginId {
    type Err = PluginError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

fn validate_plugin_id(raw: &str) -> Result<(), PluginError> {
    if raw.is_empty() {
        return Err(PluginError::InvalidPluginId {
            id: raw.to_string(),
            reason: "plugin id must not be empty".to_string(),
        });
    }
    if raw.len() > MAX_PLUGIN_ID_LEN {
        return Err(PluginError::InvalidPluginId {
            id: raw.to_string(),
            reason: format!("plugin id too long (max {MAX_PLUGIN_ID_LEN})"),
        });
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return Err(PluginError::InvalidPluginId {
            id: raw.to_string(),
            reason: "plugin id must not contain whitespace".to_string(),
        });
    }
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 2 {
        return Err(PluginError::InvalidPluginId {
            id: raw.to_string(),
            reason: "plugin id must be exactly owner.name (one dot)".to_string(),
        });
    }
    for seg in &parts {
        if seg.is_empty() {
            return Err(PluginError::InvalidPluginId {
                id: raw.to_string(),
                reason: "plugin id segment must not be empty".to_string(),
            });
        }
        if seg.len() > 64 {
            return Err(PluginError::InvalidPluginId {
                id: raw.to_string(),
                reason: "plugin id segment too long (max 64)".to_string(),
            });
        }
        let first = seg.as_bytes()[0];
        if !first.is_ascii_lowercase() {
            return Err(PluginError::InvalidPluginId {
                id: raw.to_string(),
                reason: "segment must start with lowercase letter".to_string(),
            });
        }
        for b in seg.bytes() {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
                return Err(PluginError::InvalidPluginId {
                    id: raw.to_string(),
                    reason: "segment must be [a-z0-9_-]".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Qualified resource name `plugin-id:resource`, e.g. `xuepoo.markdown:toggle`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QualifiedName(String);

impl QualifiedName {
    /// Parse and validate a qualified name.
    pub fn new(raw: &str) -> Result<Self, PluginError> {
        if raw.is_empty() {
            return Err(PluginError::manifest(
                "qualified_name",
                "qualified name must not be empty",
            ));
        }
        if raw.len() > 256 {
            return Err(PluginError::manifest(
                "qualified_name",
                "qualified name too long (max 256)",
            ));
        }
        let (plugin_part, resource) = raw.split_once(':').ok_or_else(|| {
            PluginError::manifest(
                "qualified_name",
                format!("qualified name '{raw}' must be plugin-id:resource"),
            )
        })?;
        validate_plugin_id(plugin_part).map_err(|_| {
            PluginError::manifest(
                "qualified_name",
                format!("invalid plugin id in qualified name '{raw}'"),
            )
        })?;
        if resource.is_empty() {
            return Err(PluginError::manifest(
                "qualified_name",
                "resource part must not be empty",
            ));
        }
        if resource.len() > 128 {
            return Err(PluginError::manifest(
                "qualified_name",
                "resource part too long (max 128)",
            ));
        }
        // Resource: start with lowercase, alphanumeric + - _ . (no spaces)
        let first = resource.as_bytes()[0];
        if !first.is_ascii_lowercase() {
            return Err(PluginError::manifest(
                "qualified_name",
                "resource must start with lowercase letter",
            ));
        }
        for b in resource.bytes() {
            if !(b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'-'
                || b == b'_'
                || b == b'.')
            {
                return Err(PluginError::manifest(
                    "qualified_name",
                    "resource must be [a-z0-9._-]",
                ));
            }
        }
        Ok(Self(raw.to_string()))
    }

    /// Raw string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Plugin id part.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        self.0.split_once(':').map(|(a, _)| a).unwrap_or(&self.0)
    }
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── semver (minimal) ────────────────────────────────────────────────────

fn validate_semver(raw: &str, field: &str) -> Result<(), PluginError> {
    if raw.is_empty() {
        return Err(PluginError::manifest(field, "version must not be empty"));
    }
    if raw.len() > 64 {
        return Err(PluginError::manifest(field, "version too long (max 64)"));
    }
    // Minimal SemVer: X.Y.Z with optional pre-release/build (ascii, no spaces).
    let core = raw.split(['-', '+']).next().unwrap_or(raw);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(PluginError::manifest(
            field,
            format!("version '{raw}' must be SemVer X.Y.Z"),
        ));
    }
    for part in parts {
        if part.is_empty() {
            return Err(PluginError::manifest(
                field,
                format!("version '{raw}' has empty numeric component"),
            ));
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PluginError::manifest(
                field,
                format!("version '{raw}' numeric components must be digits"),
            ));
        }
        // Reject leading zeros (except single zero).
        if part.len() > 1 && part.starts_with('0') {
            return Err(PluginError::manifest(
                field,
                format!("version '{raw}' must not have leading zeros"),
            ));
        }
    }
    // Remaining chars (pre-release/build) must be ascii alnum + . - + _ if present.
    for b in raw.bytes() {
        if !(b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+' || b == b'_') {
            return Err(PluginError::manifest(
                field,
                format!("version '{raw}' contains invalid character"),
            ));
        }
    }
    Ok(())
}

fn validate_version_req(raw: &str, field: &str) -> Result<(), PluginError> {
    if raw.is_empty() {
        return Err(PluginError::manifest(
            field,
            "version requirement must not be empty",
        ));
    }
    if raw.len() > 128 {
        return Err(PluginError::manifest(
            field,
            "version requirement too long (max 128)",
        ));
    }
    // Candidate: allow common operators; no need for full semver range evaluation yet.
    // Check that string contains only reasonable chars.
    for b in raw.bytes() {
        if !(b.is_ascii_alphanumeric()
            || b.is_ascii_whitespace()
            || matches!(
                b,
                b'.' | b'-' | b'+' | b',' | b'<' | b'>' | b'=' | b'^' | b'~' | b'*' | b'|' | b'&'
            ))
        {
            return Err(PluginError::manifest(
                field,
                format!("version requirement '{raw}' contains invalid character"),
            ));
        }
    }
    Ok(())
}

// ── capability set (manifest section) ────────────────────────────────────

/// Filesystem access kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FsAccess {
    Read,
    Write,
}

/// A filesystem capability request carrying explicit patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemRequest {
    /// Access kind.
    pub access: FsAccess,
    /// Glob patterns (bounded).
    pub paths: Vec<String>,
}

impl FilesystemRequest {
    /// Validate this request.
    pub fn validate(&self) -> Result<(), PluginError> {
        if self.paths.is_empty() {
            return Err(PluginError::manifest(
                "capabilities.filesystem",
                "filesystem request must have at least one path",
            ));
        }
        if self.paths.len() > MAX_FS_PATTERNS_PER_KIND {
            return Err(PluginError::LimitExceeded {
                field: "capabilities.filesystem.paths".to_string(),
                limit: MAX_FS_PATTERNS_PER_KIND,
                actual: self.paths.len(),
            });
        }
        let mut total = 0usize;
        for p in &self.paths {
            if p.is_empty() {
                return Err(PluginError::manifest(
                    "capabilities.filesystem.paths",
                    "path pattern must not be empty",
                ));
            }
            if p.len() > 512 {
                return Err(PluginError::manifest(
                    "capabilities.filesystem.paths",
                    "path pattern too long (max 512)",
                ));
            }
            if p.contains('\0') {
                return Err(PluginError::manifest(
                    "capabilities.filesystem.paths",
                    "path pattern must not contain NUL",
                ));
            }
            total += p.len();
        }
        if total > MAX_PATTERN_TEXT_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "capabilities.filesystem.pattern_text".to_string(),
                limit: MAX_PATTERN_TEXT_BYTES,
                actual: total,
            });
        }
        Ok(())
    }
}

// ── manifest structs ─────────────────────────────────────────────────────

/// Identity block `[plugin]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginIdentity {
    /// Owner-qualified id.
    pub id: PluginId,
    /// Display name (bounded, untrusted display data — rendered with host-owned components, never as markup).
    pub name: String,
    /// SemVer 2 version.
    pub version: String,
    /// Short description (bounded).
    pub description: String,
    /// SPDX license expression, if present.
    pub license: Option<String>,
}

impl PluginIdentity {
    /// Validate this identity.
    pub fn validate(&self) -> Result<(), PluginError> {
        // id already validated via PluginId::new.
        if self.name.trim().is_empty() {
            return Err(PluginError::manifest("plugin.name", "must not be empty"));
        }
        if self.name.len() > MAX_NAME_LEN {
            return Err(PluginError::LimitExceeded {
                field: "plugin.name".to_string(),
                limit: MAX_NAME_LEN,
                actual: self.name.len(),
            });
        }
        if self.description.len() > MAX_DESCRIPTION_LEN {
            return Err(PluginError::LimitExceeded {
                field: "plugin.description".to_string(),
                limit: MAX_DESCRIPTION_LEN,
                actual: self.description.len(),
            });
        }
        validate_semver(&self.version, "plugin.version")?;
        if let Some(lic) = &self.license {
            if lic.len() > MAX_LICENSE_LEN {
                return Err(PluginError::LimitExceeded {
                    field: "plugin.license".to_string(),
                    limit: MAX_LICENSE_LEN,
                    actual: lic.len(),
                });
            }
            if lic.trim().is_empty() {
                return Err(PluginError::manifest(
                    "plugin.license",
                    "must not be empty if present",
                ));
            }
        }
        // Display strings are treated as untrusted: never interpreted as markup;
        // validation only bounds length and rejects control chars that could affect host rendering.
        for (field, value) in [
            ("plugin.name", &self.name),
            ("plugin.description", &self.description),
        ] {
            if value.contains('\0') || value.contains('\x1b') {
                return Err(PluginError::manifest(field, "must not contain NUL or ESC"));
            }
        }
        Ok(())
    }
}

/// Compatibility block `[compat]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compat {
    /// Bitty application version range (e.g. `">=0.5,<1.0"`).
    pub bitty: Option<String>,
    /// Plugin API range (e.g. `"^1.0"`).
    pub plugin_api: Option<String>,
}

impl Compat {
    /// Validate compat ranges (syntax only; resolver evaluates semantics).
    pub fn validate(&self) -> Result<(), PluginError> {
        if let Some(r) = &self.bitty {
            validate_version_req(r, "compat.bitty")?;
        }
        if let Some(r) = &self.plugin_api {
            validate_version_req(r, "compat.plugin-api")?;
        }
        Ok(())
    }
}

/// Capability requests for the manifest.
///
/// The draft keeps a flat list of parsed [`CapabilityId`]s plus the
/// structured `filesystem` requests that carry explicit path globs. Absent
/// means no authority (deny by default); unknown identifiers fail validation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilityRequests {
    /// Parsed capability identifiers (validated closed set, no wildcards).
    pub ids: BTreeSet<CapabilityId>,
    /// Structured filesystem requests (validated separately; they map to `fs.read:PARAM` / `fs.write:PARAM` checks).
    pub filesystem: Vec<FilesystemRequest>,
}

impl CapabilityRequests {
    /// Validate all capability requests.
    pub fn validate(&self) -> Result<(), PluginError> {
        // Already validated via CapabilityId::parse at insertion; re-validate invariants.
        for id in &self.ids {
            // Re-parse to ensure no bypass via direct construction.
            CapabilityId::parse(id.as_str())?;
        }

        // Filesystem requests: check per-kind bounds and total pattern text.
        let mut total_pattern_bytes = 0usize;
        for req in &self.filesystem {
            req.validate()?;
            for p in &req.paths {
                total_pattern_bytes += p.len();
            }
        }
        if total_pattern_bytes > MAX_PATTERN_TEXT_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "capabilities.pattern_text".to_string(),
                limit: MAX_PATTERN_TEXT_BYTES,
                actual: total_pattern_bytes,
            });
        }

        // Deny-by-default: no allow-all identifier exists. Enforcement is that
        // the closed set contains no wildcard and absence means denial (no
        // separate check needed beyond the forbidden `*` already rejected).
        Ok(())
    }
}

/// Lazy trigger declarations `[lazy]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LazyTriggers {
    /// Commands that load the plugin on first invocation.
    pub commands: Vec<QualifiedName>,
    /// Event types that load the plugin.
    pub events: Vec<String>,
    /// UI claim names that load the plugin (e.g. `tabline`).
    pub claims: Vec<String>,
}

impl LazyTriggers {
    /// Validate lazy triggers.
    pub fn validate(&self) -> Result<(), PluginError> {
        if self.commands.len() > MAX_COMMANDS {
            return Err(PluginError::LimitExceeded {
                field: "lazy.commands".to_string(),
                limit: MAX_COMMANDS,
                actual: self.commands.len(),
            });
        }
        if self.events.len() > MAX_EVENT_TYPES {
            return Err(PluginError::LimitExceeded {
                field: "lazy.events".to_string(),
                limit: MAX_EVENT_TYPES,
                actual: self.events.len(),
            });
        }
        for ev in &self.events {
            if ev.is_empty() || ev.len() > 128 {
                return Err(PluginError::manifest(
                    "lazy.events",
                    "event type must be 1..128 bytes",
                ));
            }
            if ev.contains('\0') || ev.contains(' ') {
                return Err(PluginError::manifest(
                    "lazy.events",
                    "event type must not contain NUL or space",
                ));
            }
        }
        for claim in &self.claims {
            if claim.is_empty() || claim.len() > 64 {
                return Err(PluginError::manifest(
                    "lazy.claims",
                    "claim must be 1..64 bytes",
                ));
            }
        }
        Ok(())
    }
}

/// The full candidate manifest for `bitty-plugin.toml`.
///
/// This is the in-memory, already-parsed shape. TOML parsing itself is
/// outside this struct (caller supplies bytes/str), but validation of every
/// parsed field is owned here with the hard limits from the RFC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    /// Identity block.
    pub identity: PluginIdentity,
    /// Compatibility block.
    pub compat: Compat,
    /// Optional plugin dependencies by id -> version req.
    pub dependencies: Vec<(PluginId, String)>,
    /// Optional provided services `interface -> version`.
    pub provided_services: Vec<(String, String)>,
    /// Requested capabilities.
    pub capabilities: CapabilityRequests,
    /// Lazy trigger declarations.
    pub lazy: LazyTriggers,
    /// Raw manifest byte length (for the 256 KiB size check).
    pub raw_bytes_len: usize,
}

impl PluginManifest {
    /// Validate this manifest against every hard limit and grammar rule.
    ///
    /// Checks performed (all headless, no I/O):
    /// - `plugin` identity (id, semver, bounded display strings),
    /// - `compat` version requirement syntax,
    /// - dependency count and version req syntax (8 max, cycle check is in registry),
    /// - provided services count and identifier syntax (16 max),
    /// - capability closed-set validation (unknown identifiers fail, no wildcards),
    /// - filesystem pattern bounds,
    /// - lazy trigger bounds,
    /// - manifest size already supplied via `raw_bytes_len`.
    pub fn validate(&self) -> Result<(), PluginError> {
        if self.raw_bytes_len > MANIFEST_MAX_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "manifest".to_string(),
                limit: MANIFEST_MAX_BYTES,
                actual: self.raw_bytes_len,
            });
        }

        self.identity.validate()?;
        self.compat.validate()?;

        if self.dependencies.len() > MAX_DEPENDENCIES {
            return Err(PluginError::LimitExceeded {
                field: "dependencies".to_string(),
                limit: MAX_DEPENDENCIES,
                actual: self.dependencies.len(),
            });
        }
        for (id, req) in &self.dependencies {
            // id already validated
            let _ = id;
            validate_version_req(req, "dependencies")?;
        }
        // Duplicate dependency ids rejected (would be silent shadowing otherwise).
        {
            let mut seen = BTreeSet::new();
            for (id, _) in &self.dependencies {
                if !seen.insert(id.as_str().to_string()) {
                    return Err(PluginError::Duplicate {
                        kind: "dependency".to_string(),
                        value: id.to_string(),
                    });
                }
            }
        }

        if self.provided_services.len() > MAX_PROVIDED_SERVICES {
            return Err(PluginError::LimitExceeded {
                field: "services.provided".to_string(),
                limit: MAX_PROVIDED_SERVICES,
                actual: self.provided_services.len(),
            });
        }
        for (iface, ver) in &self.provided_services {
            if iface.is_empty() || iface.len() > 128 {
                return Err(PluginError::manifest(
                    "services.provided",
                    "interface name must be 1..128 bytes",
                ));
            }
            if iface.contains('\0') || iface.contains(' ') {
                return Err(PluginError::manifest(
                    "services.provided",
                    "interface name must not contain NUL or space",
                ));
            }
            // Interface naming: allow dot-separated lowercase (e.g. `markdown.render`).
            for seg in iface.split('.') {
                if seg.is_empty() || seg.len() > 64 {
                    return Err(PluginError::manifest(
                        "services.provided",
                        "interface segment must be 1..64 bytes",
                    ));
                }
            }
            validate_semver(ver, "services.provided")?;
        }

        self.capabilities.validate()?;
        self.lazy.validate()?;

        // Total pattern text is also checked inside capabilities; duplicate capability ids
        // would have been deduplicated in the BTreeSet (no error, just one grant check).

        // Every string field is bounded and treated as untrusted display data.
        // No additional handling is needed beyond the bounds already enforced;
        // callers must render names/descriptions with host-owned components.

        Ok(())
    }

    /// Convenience: the plugin id of this manifest.
    #[must_use]
    pub fn id(&self) -> &PluginId {
        &self.identity.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_identity(id: &str) -> PluginIdentity {
        PluginIdentity {
            id: PluginId::new(id).unwrap(),
            name: "Test".to_string(),
            version: "0.1.0".to_string(),
            description: "A test plugin".to_string(),
            license: Some("MIT".to_string()),
        }
    }

    fn minimal_manifest(id: &str) -> PluginManifest {
        PluginManifest {
            identity: minimal_identity(id),
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            provided_services: Vec::new(),
            capabilities: CapabilityRequests::default(),
            lazy: LazyTriggers::default(),
            raw_bytes_len: 512,
        }
    }

    #[test]
    fn valid_minimal_manifest() {
        let m = minimal_manifest("xuepoo.markdown");
        assert!(m.validate().is_ok());
    }

    #[test]
    fn invalid_plugin_id_rejected() {
        assert!(PluginId::new("bad id").is_err());
        assert!(PluginId::new("Bad.owner").is_err());
        assert!(PluginId::new("owner").is_err());
        assert!(PluginId::new("a.b.c").is_err());
    }

    #[test]
    fn qualified_name_validates() {
        assert!(QualifiedName::new("xuepoo.markdown:toggle").is_ok());
        assert!(QualifiedName::new("xuepoo.markdown:").is_err());
        assert!(QualifiedName::new("xuepoo.markdown").is_err());
        assert!(QualifiedName::new("bad:toggle").is_err());
    }

    #[test]
    fn dependency_limit() {
        let mut m = minimal_manifest("xuepoo.test");
        for i in 0..(MAX_DEPENDENCIES + 1) {
            m.dependencies.push((
                PluginId::new(&format!("xuepoo.dep{i}")).unwrap(),
                ">=1.0".to_string(),
            ));
        }
        assert!(m.validate().is_err());
    }

    #[test]
    fn filesystem_pattern_bounds() {
        let mut m = minimal_manifest("xuepoo.test");
        let req = FilesystemRequest {
            access: FsAccess::Read,
            paths: vec!["a".repeat(600)],
        };
        m.capabilities.filesystem.push(req);
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_size_limit() {
        let mut m = minimal_manifest("xuepoo.test");
        m.raw_bytes_len = MANIFEST_MAX_BYTES + 1;
        assert!(m.validate().is_err());
    }

    #[test]
    fn unknown_capability_rejected_via_closure() {
        let mut m = minimal_manifest("xuepoo.test");
        // Insert a valid capability first, then validate closed set.
        let cap = CapabilityId::parse("terminal.semantic-read").unwrap();
        m.capabilities.ids.insert(cap);
        assert!(m.validate().is_ok());

        // Unknown capability would have been rejected at parse time; verify directly.
        assert!(CapabilityId::parse("terminal.unknown-thing").is_err());
    }

    #[test]
    fn semver_validation() {
        assert!(validate_semver("1.0.0", "plugin.version").is_ok());
        assert!(validate_semver("0.9.0-alpha", "plugin.version").is_ok());
        assert!(validate_semver("1.0", "plugin.version").is_err());
        assert!(validate_semver("01.0.0", "plugin.version").is_err());
        assert!(validate_semver("", "plugin.version").is_err());
    }

    #[test]
    fn lazy_bounds() {
        let mut m = minimal_manifest("xuepoo.test");
        m.lazy.commands = (0..(MAX_COMMANDS + 1))
            .map(|i| QualifiedName::new(&format!("xuepoo.test:cmd{i}")).unwrap())
            .collect();
        assert!(m.validate().is_err());
    }
}
