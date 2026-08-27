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
    /// Deterministic canonical bytes for hash binding (draft `bitty-manifest-v1`).
    ///
    /// Sorted, cross-platform, no wall-clock. Covers identity, compat, resolved
    /// capability set (including filesystem `fs.read:PARAM`/`fs.write:PARAM` expansion),
    /// dependencies, and services. Used to bind grant records to the exact manifest
    /// that was approved (`hash(manifest) == record.manifest_hash`).
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = String::new();
        buf.push_str("bitty-manifest-v1\n");
        buf.push_str(self.identity.id.as_str());
        buf.push('|');
        buf.push_str(&self.identity.version);
        buf.push('|');
        // Sorted capability ids (canonical).
        let mut caps: Vec<&str> = self.capabilities.ids.iter().map(|c| c.as_str()).collect();
        caps.sort_unstable();
        for c in caps {
            buf.push_str(c);
            buf.push(',');
        }
        buf.push('|');
        // Filesystem expansions `fs.read:pat` / `fs.write:pat` sorted.
        let mut fs: Vec<String> = Vec::new();
        for req in &self.capabilities.filesystem {
            let prefix = match req.access {
                FsAccess::Read => "fs.read:",
                FsAccess::Write => "fs.write:",
            };
            for p in &req.paths {
                fs.push(format!("{prefix}{p}"));
            }
        }
        fs.sort_unstable();
        for p in fs {
            buf.push_str(&p);
            buf.push(',');
        }
        buf.push('|');
        // Compat (empty if None)
        buf.push_str(self.compat.bitty.as_deref().unwrap_or(""));
        buf.push('|');
        buf.push_str(self.compat.plugin_api.as_deref().unwrap_or(""));
        buf.push('|');
        // Dependencies sorted.
        let mut deps: Vec<String> = self
            .dependencies
            .iter()
            .map(|(id, req)| format!("{}={}", id.as_str(), req))
            .collect();
        deps.sort_unstable();
        for d in deps {
            buf.push_str(&d);
            buf.push(',');
        }
        buf.push('|');
        // Services sorted.
        let mut svcs: Vec<String> = self
            .provided_services
            .iter()
            .map(|(iface, ver)| format!("{iface}={ver}"))
            .collect();
        svcs.sort_unstable();
        for s in svcs {
            buf.push_str(&s);
            buf.push(',');
        }
        buf.into_bytes()
    }

    /// Canonical manifest hash (hex SHA-256 over [`Self::canonical_bytes`]).
    ///
    /// Deterministic, cross-platform. Used as the opaque `manifest_hash` stored
    /// in [`crate::grant::GrantRecord`] and checked by [`crate::host::PluginHost::activate`].
    #[must_use]
    pub fn manifest_hash(&self) -> String {
        sha256_hex(&self.canonical_bytes())
    }

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

    #[test]
    fn manifest_hash_deterministic_and_sensitive() {
        let m1 = minimal_manifest("xuepoo.hash");
        let m2 = minimal_manifest("xuepoo.hash");
        assert_eq!(m1.manifest_hash(), m2.manifest_hash());
        assert_eq!(m1.manifest_hash().len(), 64);
        assert!(m1.manifest_hash().chars().all(|c| c.is_ascii_hexdigit()));
        // Changing version changes hash.
        let mut m3 = m1.clone();
        m3.identity.version = "0.2.0".to_string();
        assert_ne!(m1.manifest_hash(), m3.manifest_hash());
        // Adding capability changes hash.
        let mut m4 = m1.clone();
        m4.capabilities
            .ids
            .insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        assert_ne!(m1.manifest_hash(), m4.manifest_hash());
    }
}

// ── sha256 helper (vendored, pure Rust, no unsafe, deterministic cross-platform) ──

#[allow(clippy::items_after_test_module)]
fn sha256_hex(bytes: &[u8]) -> String {
    let hash = sha256(bytes);
    let mut s = String::with_capacity(64);
    for b in hash {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    out
}
