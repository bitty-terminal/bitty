//! Manifest and identity model (OQ-012, part 1, closed by the accepted Plugin Platform RFC).
//!
//! Accepted `bitty-plugin.toml` schema per the accepted plugin-platform RFC (2026-08-27,
//! frontmatter `status: accepted`; bitty-docs open-questions register).
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
/// Maximum required services.
pub const MAX_REQUIRED_SERVICES: usize = 16;
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
/// Maximum declared Layer-2 system-CLI tools.
pub const MAX_TOOLS: usize = 8;
/// Maximum tool name length (policy bound, mirrors id-segment bound).
pub const MAX_TOOL_NAME_LEN: usize = 64;
/// Maximum tool version-requirement length (mirrors compat bound).
pub const MAX_TOOL_VERSION_REQ_LEN: usize = 128;
/// The only accepted Layer-2 tool (CTX-0425 v1).
///
/// Any other `[tools.*]` table fails closed until its own slice is accepted.
pub const ACCEPTED_TOOLS: &[&str] = &["git"];

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

/// Home-scoped dot-directories that always hold secrets (CTX-0465).
///
/// A filesystem pattern naming one of these — as a `~/`/`~\` prefix or as any
/// segment on either separator (e.g. `**/.ssh/**`) — fails closed. Matching is
/// ASCII case-insensitive because the target filesystem may be
/// case-insensitive (Windows/macOS); bare relative patterns that merely pass
/// *through* a project-local directory with the same name are collateral: name
/// grants precisely instead.
const FS_SENSITIVE_HOME_NAMES: &[&str] = &[".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker"];

/// Sensitive two-level prefixes under `~/` (credential helpers), matched as
/// case-insensitive segments on either separator.
const FS_SENSITIVE_HOME_PREFIXES: &[&[&str]] =
    &[&["~", ".config", "gh"], &["~", ".config", "gcloud"]];

/// Whether a glob segment is a literal name rather than a wildcard matcher.
///
/// The sensitive denylist can only certify a pattern whose leading home child
/// is a concrete name: a first segment containing glob syntax matches an
/// unknown set of home children, which includes the sensitive dot-directories.
/// The `.` identity segment names the containing directory, so it pins nothing
/// either (`~/.` is the bare home root).
fn is_literal_fs_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && !segment.contains(['*', '?', '[', ']', '{', '}'])
}

/// Whether a filesystem glob `pattern` escapes its sandbox or names secrets.
///
/// Denies (fail-closed):
/// - absolute paths: leading `/`, Windows drive (`C:/`, `C:\`), UNC (`\\`);
/// - `~user/` homes: only a `~`-rooted pattern stays expressible, and it must
///   name a literal first child;
/// - overbroad home roots: bare `~`, `~/`, `~\`, and any `~`-rooted pattern
///   whose first component is not a literal name (`~/**`, `~/*`, `~/.*/**`,
///   `~/.`) — these match unknown home children, sensitive dot-directories
///   included;
/// - `..` segments on either separator (`../`, `..\\`, embedded, trailing);
/// - sensitive credential locations, case-insensitively and on either
///   separator (`~/.ssh/...`, `~\.SSH\...`, any `.ssh`/`.gnupg`/`.aws`/
///   `.azure`/`.kube`/`.docker` segment, `~/.config/gh|gcloud/...`), with
///   empty and `.` segments normalized away first so spelling variants like
///   `~/.config/./gh/...` or `~//.config/gh/...` still match.
#[must_use]
pub fn is_hostile_fs_pattern(pattern: &str) -> bool {
    if pattern.starts_with('/') || pattern.starts_with("\\\\") {
        return true;
    }
    let bytes = pattern.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return true;
    }
    let segments: Vec<&str> = pattern.split(['/', '\\']).collect();
    let home_scoped = segments.first() == Some(&"~");
    if pattern.starts_with('~') && !home_scoped {
        // `~user` names a foreign home; only `~` itself may root a pattern.
        return true;
    }
    if segments.contains(&"..") {
        return true;
    }
    if home_scoped
        && !segments
            .iter()
            .skip(1)
            .find(|segment| !segment.is_empty())
            .is_some_and(|segment| is_literal_fs_segment(segment))
    {
        // Overbroad home root: no literal first child pins the grant.
        return true;
    }
    // Positional scans run on a normalized view: empty and `.` segments are
    // no-ops on the target filesystem, so `~/.config/./gh/...` and
    // `~/.config//gh/...` must match `~/.config/gh/...` exactly; otherwise a
    // trivial spelling variant bypasses the credential-prefix guard.
    let normalized: Vec<&str> = segments
        .iter()
        .copied()
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect();
    if normalized.iter().any(|segment| {
        FS_SENSITIVE_HOME_NAMES
            .iter()
            .any(|name| segment.eq_ignore_ascii_case(name))
    }) {
        return true;
    }
    for prefix in FS_SENSITIVE_HOME_PREFIXES {
        if normalized.len() >= prefix.len()
            && normalized[..prefix.len()]
                .iter()
                .zip(prefix.iter())
                .all(|(segment, expected)| segment.eq_ignore_ascii_case(expected))
        {
            return true;
        }
    }
    false
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
            if p.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
                return Err(PluginError::manifest(
                    "capabilities.filesystem.paths",
                    "path pattern must not contain control characters or whitespace",
                ));
            }
            if is_hostile_fs_pattern(p) {
                return Err(PluginError::manifest(
                    "capabilities.filesystem.paths",
                    format!(
                        "path pattern '{p}' is hostile (absolute path, '..' traversal, foreign/overbroad home, or sensitive credential location)"
                    ),
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

// ── Layer-2 system-CLI tools (accepted `[tools.git]` v1, CTX-0425) ────────

/// One Layer-2 system-CLI tool declaration (`[tools.<name>]`).
///
/// Only the accepted slice passes: `tool` must be `git` (see
/// [`ACCEPTED_TOOLS`]), `required` pins fail-closed activation when the tool
/// is missing or mismatched, and `version_req` pins the version constraint
/// re-checked by `bitty plugin doctor`. Any other tool table fails closed
/// until its own slice is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDeclaration {
    /// Tool name (e.g. `git`).
    pub tool: String,
    /// Whether activation fails closed when the tool is missing/mismatched.
    pub required: bool,
    /// Version constraint (e.g. `>=2.30`).
    pub version_req: String,
}

impl ToolDeclaration {
    /// Validate this declaration as untrusted input (fail-closed).
    pub fn validate(&self) -> Result<(), PluginError> {
        if !crate::tools::is_valid_tool_name(&self.tool) {
            return Err(PluginError::manifest(
                "tools",
                format!(
                    "tool name '{}' must match [a-z0-9]+(-[a-z0-9]+)* (max {MAX_TOOL_NAME_LEN})",
                    self.tool
                ),
            ));
        }
        if !ACCEPTED_TOOLS.contains(&self.tool.as_str()) {
            return Err(PluginError::manifest(
                "tools",
                format!(
                    "unknown tool '{}' (only {} accepted until its own slice is accepted)",
                    self.tool,
                    ACCEPTED_TOOLS.join(", ")
                ),
            ));
        }
        validate_version_req(&self.version_req, "tools.version")?;
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
    /// Whether this manifest explicitly requests `capability`.
    #[must_use]
    pub fn contains(&self, capability: &CapabilityId) -> bool {
        if self.ids.contains(capability) {
            return true;
        }
        self.filesystem.iter().any(|request| {
            let name = match request.access {
                FsAccess::Read => "fs.read",
                FsAccess::Write => "fs.write",
            };
            request.paths.iter().any(|path| {
                CapabilityId::parse(&format!("{name}:{path}"))
                    .is_ok_and(|expanded| expanded == *capability)
            })
        })
    }

    /// The full requested capability set (flat ids plus filesystem requests
    /// expanded to `fs.read:PARAM` / `fs.write:PARAM`).
    ///
    /// This is the exact set a grant must cover before activation; it is the
    /// single expansion shared by [`crate::host::PluginHost::activate`] and
    /// the `bitty plugin` CLI consent surface (CTX-0150). Sorted and
    /// deduplicated (`BTreeSet`), deny-by-default: an empty set means no
    /// authority requested.
    pub fn all_ids(&self) -> Result<BTreeSet<CapabilityId>, PluginError> {
        let mut required = self.ids.clone();
        for request in &self.filesystem {
            for path in &request.paths {
                let capability = match request.access {
                    FsAccess::Read => format!("fs.read:{path}"),
                    FsAccess::Write => format!("fs.write:{path}"),
                };
                required.insert(CapabilityId::parse(&capability).map_err(|error| {
                    PluginError::grant(format!(
                        "invalid filesystem capability '{capability}': {error}"
                    ))
                })?);
            }
        }
        Ok(required)
    }

    /// Validate all capability requests.
    pub fn validate(&self) -> Result<(), PluginError> {
        // Already validated via CapabilityId::parse at insertion; re-validate invariants.
        for id in &self.ids {
            // Re-parse to ensure no bypass via direct construction.
            CapabilityId::parse(id.as_str())?;
            // Filesystem authority must use `[[capabilities.filesystem]]`
            // (structured requests), never a bool `fs.read:*` / `fs.write:*`
            // key (fail-closed; mirrors the transitional validator).
            let raw = id.as_str();
            let head = raw.split_once(':').map(|(h, _)| h).unwrap_or(raw);
            if head == "fs.read" || head == "fs.write" {
                return Err(PluginError::manifest(
                    "capabilities.filesystem",
                    format!(
                        "capability '{raw}' must use '[[capabilities.filesystem]]', not a boolean key"
                    ),
                ));
            }
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
    /// UI claim names that load the plugin (e.g. `workspaceline`; `tabline` is a deprecated alias).
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

/// Validate one service interface name (shared provides/requires grammar).
///
/// Dot-separated segments (e.g. `markdown.render`), 1..128 bytes total,
/// 1..64 bytes per segment, no NUL or space. The manifest is
/// attacker-controlled input so every interface string is bounded before use.
fn validate_service_iface(iface: &str, field: &str) -> Result<(), PluginError> {
    if iface.is_empty() || iface.len() > 128 {
        return Err(PluginError::manifest(
            field,
            "interface name must be 1..128 bytes",
        ));
    }
    if iface.contains('\0') || iface.contains(' ') {
        return Err(PluginError::manifest(
            field,
            "interface name must not contain NUL or space",
        ));
    }
    // Interface naming: allow dot-separated lowercase (e.g. `markdown.render`).
    for seg in iface.split('.') {
        if seg.is_empty() || seg.len() > 64 {
            return Err(PluginError::manifest(
                field,
                "interface segment must be 1..64 bytes",
            ));
        }
    }
    Ok(())
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
    /// Optional required services `interface -> version requirement`.
    ///
    /// The consumes side of the provide/require loop: each entry names a
    /// service interface (same dot-separated grammar as provided services)
    /// plus a version requirement in the closed comparator grammar (same
    /// syntax class as plugin dependencies). Satisfaction is checked at
    /// resolve time against the provides side of the declared graph; service
    /// lookup/invocation at runtime is explicitly out of scope.
    pub required_services: Vec<(String, String)>,
    /// Requested capabilities.
    pub capabilities: CapabilityRequests,
    /// Layer-2 system-CLI tool declarations (`[tools.*]`, accepted v1: only `git`).
    pub tools: Vec<ToolDeclaration>,
    /// Lazy trigger declarations.
    pub lazy: LazyTriggers,
    /// Raw manifest byte length (for the 256 KiB size check).
    pub raw_bytes_len: usize,
}

impl PluginManifest {
    /// Deterministic canonical bytes for hash binding (draft `bitty-manifest-v3`).
    ///
    /// Sorted, cross-platform, no wall-clock. Covers identity, compat, resolved
    /// capability set (including filesystem `fs.read:PARAM`/`fs.write:PARAM` expansion),
    /// dependencies, provided services, required services, and Layer-2 `tools`
    /// declarations. Used to bind grant records to the exact manifest
    /// that was approved (`hash(manifest) == record.manifest_hash`).
    /// Raising `tools.<name>.required` from `false` to `true` changes the hash
    /// and is a capability increase whose grant must be re-confirmed.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = String::new();
        buf.push_str("bitty-manifest-v3\n");
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
        buf.push('|');
        // Required services sorted (v2 segment: requires-side of the loop).
        let mut reqs: Vec<String> = self
            .required_services
            .iter()
            .map(|(iface, req)| format!("{iface}={req}"))
            .collect();
        reqs.sort_unstable();
        for s in reqs {
            buf.push_str(&s);
            buf.push(',');
        }
        buf.push('|');
        // Layer-2 tools sorted (v3 segment: `tool=required:version_req`).
        let mut tools: Vec<String> = self
            .tools
            .iter()
            .map(|t| format!("{}={}:{}", t.tool, t.required, t.version_req))
            .collect();
        tools.sort_unstable();
        for t in tools {
            buf.push_str(&t);
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
    /// - required services count, identifier syntax, and requirement syntax
    ///   (16 max, duplicate interfaces rejected; satisfaction is resolve-time),
    /// - capability closed-set validation (unknown identifiers fail, no wildcards),
    /// - filesystem pattern bounds,
    /// - Layer-2 `tools` declarations (accepted v1: only `git`; unknown tools
    ///   fail closed; `process.spawn:<tool>` requires `[tools.<tool>]` and vice
    ///   versa),
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
            validate_service_iface(iface, "services.provided")?;
            validate_semver(ver, "services.provided")?;
        }

        if self.required_services.len() > MAX_REQUIRED_SERVICES {
            return Err(PluginError::LimitExceeded {
                field: "services.required".to_string(),
                limit: MAX_REQUIRED_SERVICES,
                actual: self.required_services.len(),
            });
        }
        for (iface, req) in &self.required_services {
            validate_service_iface(iface, "services.required")?;
            validate_version_req(req, "services.required")?;
        }
        // Duplicate required interfaces rejected (would be silent shadowing otherwise).
        {
            let mut seen = BTreeSet::new();
            for (iface, _) in &self.required_services {
                if !seen.insert(iface.clone()) {
                    return Err(PluginError::Duplicate {
                        kind: "required-service".to_string(),
                        value: iface.clone(),
                    });
                }
            }
        }

        self.capabilities.validate()?;
        self.lazy.validate()?;

        if self.tools.len() > MAX_TOOLS {
            return Err(PluginError::LimitExceeded {
                field: "tools".to_string(),
                limit: MAX_TOOLS,
                actual: self.tools.len(),
            });
        }
        for decl in &self.tools {
            decl.validate()?;
        }
        {
            let mut seen = BTreeSet::new();
            for decl in &self.tools {
                if !seen.insert(decl.tool.clone()) {
                    return Err(PluginError::Duplicate {
                        kind: "tool".to_string(),
                        value: decl.tool.clone(),
                    });
                }
            }
        }
        // Pairing: `process.spawn:<tool>` requires `[tools.<tool>]` and vice
        // versa (fail-closed; prevents spawn authority without a versioned
        // tool declaration and tool declarations without capability consent).
        {
            let mut spawn_tools = BTreeSet::new();
            for id in &self.capabilities.ids {
                let raw = id.as_str();
                if let Some((head, param)) = raw.split_once(':') {
                    if head == "process.spawn" {
                        spawn_tools.insert(param.to_string());
                    }
                }
            }
            for tool in &spawn_tools {
                if !self.tools.iter().any(|t| t.tool == *tool) {
                    return Err(PluginError::manifest(
                        "tools",
                        format!(
                            "capability 'process.spawn:{tool}' requires a '[tools.{tool}]' declaration"
                        ),
                    ));
                }
            }
            for decl in &self.tools {
                let expected = format!("process.spawn:{}", decl.tool);
                if !self
                    .capabilities
                    .ids
                    .iter()
                    .any(|id| id.as_str() == expected)
                {
                    return Err(PluginError::manifest(
                        "capabilities",
                        format!("'[tools.{}]' requires capability '{expected}'", decl.tool),
                    ));
                }
            }
        }

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
            required_services: Vec::new(),
            capabilities: CapabilityRequests::default(),
            tools: Vec::new(),
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
    fn filesystem_patterns_reject_controls_and_unicode_whitespace() {
        for path in ["path\0name", "path\u{0007}name", "path\u{2003}name"] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(req.validate().is_err(), "should reject {path:?}");

            let mut manifest = minimal_manifest("xuepoo.test");
            manifest.capabilities.filesystem.push(req);
            assert!(
                manifest.validate().is_err(),
                "manifest should reject {path:?}"
            );
        }
    }

    #[test]
    fn probe_filesystem_rejects_absolute_traversal_and_sensitive() {
        // CTX-0465 hostile probes: absolute paths, `..` segments (both
        // separators), `~user` homes, and sensitive-prefix patterns must
        // fail closed at manifest validation.
        for path in [
            "/etc/passwd",
            "/etc/**",
            "/etc/../etc/passwd",
            "../secret",
            "a/../../b",
            "~/../etc/passwd",
            "~root/.ssh/id_rsa",
            "~/.ssh/id_rsa",
            "~/.ssh",
            "~/.ssh/**",
            "~/.gnupg/**",
            "~/.aws/credentials",
            "~/.azure/**",
            "**/.ssh/**",
            "docs/../../.ssh/id_rsa",
            "C:/Windows/System32/**",
            "C:\\Windows\\System32\\**",
            "\\\\server\\share\\**",
            "/proc/self/environ",
            "/sys/**",
        ] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(
                req.validate().is_err(),
                "hostile fs pattern {path:?} must be denied"
            );
            let mut manifest = minimal_manifest("xuepoo.test");
            manifest.capabilities.filesystem.push(FilesystemRequest {
                access: FsAccess::Write,
                paths: vec![path.to_string()],
            });
            assert!(
                manifest.validate().is_err(),
                "manifest must reject hostile fs pattern {path:?}"
            );
        }
        // Legit panel use keeps working.
        for path in [
            "~/projects/**",
            "~/mail/**",
            "~/Documents/**/*.md",
            "~/docs/*.md",
            "notes/**",
            "docs/**/*.md",
        ] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(
                req.validate().is_ok(),
                "legit fs pattern {path:?} must stay allowed"
            );
        }
    }

    #[test]
    fn probe_filesystem_rejects_overbroad_home_and_case_separator_variants() {
        // CTX-0489 (follow-up of #772 residual 2/3): bare/overbroad home
        // patterns match the whole home — sensitive dot-directories included —
        // without ever naming them, and exact-case `/`-only segment matching
        // misses `.SSH`-style and `~\.ssh\...` variants on case-insensitive
        // Windows/macOS filesystems.
        for path in [
            "~",
            "~/",
            "~\\",
            "~//",
            "~/**",
            "~\\**",
            "~/*",
            "~/.*",
            "~/.*/**",
            "~/[.]ssh/**",
            "~/.SSH",
            "~/.SSH/**",
            "~/.Aws/credentials",
            "~/.GNUPG/**",
            "~/.AZURE/**",
            "~/.KUBE/config",
            "~/.DOCKER/config.json",
            "~/.config/GH/hosts.yml",
            "~/.CONFIG/gcloud/credentials.db",
            "~\\.ssh\\id_rsa",
            "~\\.AWS\\credentials",
            "~\\projects\\.SSH\\id_rsa",
            "**/.AWS/**",
            "docs/.SSH/id_rsa",
            "~root\\",
        ] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(
                req.validate().is_err(),
                "overbroad/variant fs pattern {path:?} must be denied"
            );
            let mut manifest = minimal_manifest("xuepoo.test");
            manifest.capabilities.filesystem.push(FilesystemRequest {
                access: FsAccess::Write,
                paths: vec![path.to_string()],
            });
            assert!(
                manifest.validate().is_err(),
                "manifest must reject overbroad/variant fs pattern {path:?}"
            );
        }
        // Legit patterns survive: a literal first home child keeps working on
        // either separator, and near-miss segment names are not collateral.
        for path in [
            "~/projects/**",
            "~/mail/**",
            "~/Documents/**/*.md",
            "~/docs/*.md",
            "notes/**",
            "docs/**/*.md",
            "~/.config",
            "~/.config/foo/**",
            "~/.config/ghost/**",
            "~/.sshrc",
            "~/.awsome/notes.md",
            "~\\.config\\foo\\**",
            "~\\projects\\**",
        ] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(
                req.validate().is_ok(),
                "legit fs pattern {path:?} must stay allowed"
            );
        }
    }

    #[test]
    fn probe_filesystem_rejects_dot_and_empty_segment_variants() {
        // CTX-0489 F1: `.` and empty path segments are no-ops on the target
        // filesystem, so `~/.` collapses to the bare home root and
        // `~/.config/./gh/...` / `~/.config//gh/...` to the sensitive
        // credential prefix. The overbroad-home guard must treat `.` as a
        // non-literal child and the positional sensitive scans must run on a
        // normalized segment view, or these spellings bypass both.
        for path in [
            "~/.",
            "~/./",
            "~/./**",
            "~/.//**",
            "~/./.config/gh/hosts.yml",
            "~/./.config/gcloud/credentials.db",
            "~/.config/./gh/hosts.yml",
            "~/.config//gh/hosts.yml",
            "~//.config/gh/hosts.yml",
        ] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(
                req.validate().is_err(),
                "dot/empty-segment fs pattern {path:?} must be denied"
            );
            let mut manifest = minimal_manifest("xuepoo.test");
            manifest.capabilities.filesystem.push(FilesystemRequest {
                access: FsAccess::Write,
                paths: vec![path.to_string()],
            });
            assert!(
                manifest.validate().is_err(),
                "manifest must reject dot/empty-segment fs pattern {path:?}"
            );
        }
        // Identifier grammar boundary (reviewer probe): `CapabilityId::parse`
        // accepts the expanded form — it validates the closed identifier
        // grammar, not fs-pattern content. Manifests cannot smuggle it that
        // way: `CapabilityRequests::validate` rejects `fs.read:*`/`fs.write:*`
        // ids outright, and structured requests deny the pattern above.
        assert!(
            CapabilityId::parse("fs.read:~/./**").is_ok(),
            "identifier grammar accepts fs.read:~/./** (manifest path denies it)"
        );
        // Legit patterns survive: literal home children, near-miss segment
        // names, and `.`-prefixed relative paths through a literal directory.
        for path in ["~/projects/**", "~/.config/ghost/**", "./~/x"] {
            let req = FilesystemRequest {
                access: FsAccess::Read,
                paths: vec![path.to_string()],
            };
            assert!(
                req.validate().is_ok(),
                "legit fs pattern {path:?} must stay allowed"
            );
        }
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
    fn required_services_valid() {
        let mut m = minimal_manifest("xuepoo.test");
        m.required_services
            .push(("markdown.render".to_string(), "^1.0".to_string()));
        assert!(m.validate().is_ok());
    }

    #[test]
    fn required_services_limit() {
        let mut m = minimal_manifest("xuepoo.test");
        for i in 0..(MAX_REQUIRED_SERVICES + 1) {
            m.required_services
                .push((format!("svc.iface{i}"), ">=1.0,<2.0".to_string()));
        }
        let err = m.validate().unwrap_err();
        assert!(format!("{err}").contains("services.required"));
    }

    #[test]
    fn required_services_reject_bad_iface_and_req() {
        let mut m = minimal_manifest("xuepoo.test");
        m.required_services
            .push(("".to_string(), "^1.0".to_string()));
        assert!(m.validate().is_err());

        let mut m = minimal_manifest("xuepoo.test");
        m.required_services
            .push(("bad iface".to_string(), "^1.0".to_string()));
        assert!(m.validate().is_err());

        let mut m = minimal_manifest("xuepoo.test");
        m.required_services
            .push(("markdown.render".to_string(), String::new()));
        assert!(m.validate().is_err());
    }

    #[test]
    fn required_services_reject_duplicates() {
        let mut m = minimal_manifest("xuepoo.test");
        m.required_services
            .push(("markdown.render".to_string(), "^1.0".to_string()));
        m.required_services
            .push(("markdown.render".to_string(), "^2.0".to_string()));
        let err = m.validate().unwrap_err();
        assert!(format!("{err}").contains("required-service"));
    }

    #[test]
    fn manifest_hash_covers_required_services() {
        let m1 = minimal_manifest("xuepoo.hash");
        let mut m2 = minimal_manifest("xuepoo.hash");
        m2.required_services
            .push(("markdown.render".to_string(), "^1.0".to_string()));
        assert_ne!(m1.manifest_hash(), m2.manifest_hash());
        assert_eq!(m2.manifest_hash(), m2.clone().manifest_hash());
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

    fn manifest_with_git_tool(id: &str) -> PluginManifest {
        let mut m = minimal_manifest(id);
        m.capabilities
            .ids
            .insert(CapabilityId::parse("process.spawn:git").unwrap());
        m.tools.push(ToolDeclaration {
            tool: "git".to_string(),
            required: true,
            version_req: ">=2.30".to_string(),
        });
        m
    }

    #[test]
    fn tools_git_accepted_pairing_validates() {
        let m = manifest_with_git_tool("xuepoo.tools");
        assert!(m.validate().is_ok());
    }

    #[test]
    fn tools_reject_unknown_tool() {
        let mut m = minimal_manifest("xuepoo.tools");
        m.capabilities
            .ids
            .insert(CapabilityId::parse("process.spawn:rg").unwrap());
        m.tools.push(ToolDeclaration {
            tool: "rg".to_string(),
            required: true,
            version_req: ">=13".to_string(),
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn tools_reject_path_manipulation_names() {
        for evil in [
            "/usr/bin/git",
            "./git",
            "../evil",
            "git.exe",
            "git;evil",
            "git evil",
            "",
        ] {
            let decl = ToolDeclaration {
                tool: evil.to_string(),
                required: true,
                version_req: ">=2.30".to_string(),
            };
            assert!(decl.validate().is_err(), "must reject {evil:?}");
        }
    }

    #[test]
    fn tools_require_capability_pairing_both_directions() {
        // Spawn capability without declaration fails closed.
        let mut m = minimal_manifest("xuepoo.pair");
        m.capabilities
            .ids
            .insert(CapabilityId::parse("process.spawn:git").unwrap());
        assert!(m.validate().is_err());

        // Declaration without capability fails closed.
        let mut m = minimal_manifest("xuepoo.pair");
        m.tools.push(ToolDeclaration {
            tool: "git".to_string(),
            required: true,
            version_req: ">=2.30".to_string(),
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn filesystem_bool_keys_fail_closed_require_table() {
        // `fs.read:~/x = true` must fail (use `[[capabilities.filesystem]]`).
        let mut m = minimal_manifest("xuepoo.fsbool");
        m.capabilities
            .ids
            .insert(CapabilityId::parse("fs.read:~/projects/**").unwrap());
        assert!(m.validate().is_err());

        let mut m = minimal_manifest("xuepoo.fsbool");
        m.capabilities
            .ids
            .insert(CapabilityId::parse("fs.write:~/projects/**").unwrap());
        assert!(m.validate().is_err());
    }

    #[test]
    fn tools_reject_duplicates_and_bad_version() {
        let mut m = manifest_with_git_tool("xuepoo.dup");
        m.tools.push(ToolDeclaration {
            tool: "git".to_string(),
            required: false,
            version_req: ">=2.30".to_string(),
        });
        assert!(m.validate().is_err());

        let mut m = manifest_with_git_tool("xuepoo.badver");
        m.tools[0].version_req = String::new();
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_hash_covers_tools() {
        let m1 = minimal_manifest("xuepoo.thash");
        let m2 = manifest_with_git_tool("xuepoo.thash");
        assert_ne!(m1.manifest_hash(), m2.manifest_hash());
        // Flipping `required` is a capability increase (hash must change).
        let mut m3 = m2.clone();
        m3.tools[0].required = false;
        assert_ne!(m2.manifest_hash(), m3.manifest_hash());
        assert_eq!(m3.manifest_hash(), m3.clone().manifest_hash());
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
