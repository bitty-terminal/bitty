//! Filesystem host bridge: Core-owned `bitty.fs` enforcement (RFC-0005, CTX-0984).
//!
//! Core-side enforcement for the NEW `bitty.fs.*` root beside `terminal.*`
//! (never an extension of it). One invariant: **a granted plugin may perform
//! bounded, self-contained reads, writes, and listings strictly inside its
//! explicit path patterns and receive labeled results or a typed denial; it
//! may never hold a handle, cross a scope, stream a file, or move bytes
//! beyond the VM without a separately granted authority.**
//!
//! # What this module owns
//!
//! - [`FsVerb`]: the decided verb set (`read`, `write`, `list`). `open` is
//!   rejected as a verb (a retained handle is a subscription by another name);
//!   `append` is a write-disposition flag ([`FsWriteRequest::append`]), not a
//!   verb; `list` is read-class and rides the read grant over the listed
//!   prefix.
//! - [`FsGrant`]: per-plugin scoped grants with explicit path patterns. No
//!   wildcard or `all` default; absence denies fail-closed. Read grants never
//!   imply write and write grants never imply read-back.
//! - [`FsCaps`]: caller-provided ceilings. No new numeric ceiling is invented
//!   here: path bytes reuse [`crate::fs_authz::MAX_FS_PATH_BYTES`] (`4096`),
//!   grant shape reuses [`crate::manifest::MAX_FS_PATTERNS_PER_KIND`] (`32`)
//!   and [`crate::manifest::MAX_PATTERN_TEXT_BYTES`] (`8 KiB`), content-scan
//!   reuses [`crate::fs_authz::MAX_FS_CONTENT_LINE_BYTES`] (`4096 + 128`)
//!   through [`crate::fs_authz::content_looks_secret`]. Exact per-call
//!   payload, listing, rate, and quota defaults stay parked to the Core bridge
//!   and SDK work; this module carries caller-provided values only.
//! - [`FsBridgeDenialKind`]: the complete typed-denial taxonomy with an
//!   oracle-tight no-leak rule (denials name the level and the family only;
//!   never content bytes, foreign identifiers, or absent-versus-denied
//!   signals). Listings suppress denied entries silently (silent skip) as the
//!   default.
//! - [`FsLabel`]: the Core-attached untrusted-observation label on every
//!   record, applied after redaction and truncation so it survives both.
//! - [`FsGate`]: the enforcement point — trust admission (L0–L4 per the
//!   threat-model filesystem matrix), safe-mode, version compat, grant
//!   intersection, sensitive-path plus secret layers (via `fs_authz`),
//!   per-plugin rate plus aggregate budgets with attribution (`P0-AC-014`),
//!   and snapshot-only requests (no watch, subscription, tail-follow,
//!   retained handle, or cross-call cursor).
//!
//! # What this module does NOT own (parked owners)
//!
//! - Exact Lua spellings and the SDK conformance suite (SDK work); the host
//!   capability heads (`fs.read`, `fs.write`) are the closed grammar in
//!   [`crate::capability`], enforced there.
//! - Numeric defaults: [`FsCaps`] carries caller-provided bounds. Zero bounds
//!   are rejected as misconfiguration: lockdown is expressed through safe
//!   mode, never through a silent zero that denies every operation.
//! - The redaction format (parked with the storage and history policy owners):
//!   secret-shaped reads enter through [`crate::fs_authz::content_looks_secret`]
//!   and are served as [`crate::secrets::SECRET_REDACTED_MARKER`] with
//!   `redacted = true`; the gate enforces truncation, label attachment, and
//!   export-preview equality.
//! - The backing filesystem and the Core integration wiring: this module reads
//!   and writes through the caller-provided [`FsView`], never a file, device,
//!   symlink, or native library. Real-path, symlink, and device resolution
//!   stays with the host I/O boundary per `fs_authz`.
//! - Wall-clock time: `now` is a monotonic host tick advanced explicitly by
//!   the caller ([`FsGate::advance_time`]); rate windows reset via
//!   [`FsGate::advance_window`]. No wall-clock, no randomness.
//!
//! # Combination and migration rules
//!
//! - Read-into-VM-only: reading or listing authorizes delivery into the plugin
//!   VM only. Copying to the clipboard, spawning processes over file content,
//!   publishing to IPC, or egressing over the network each needs its own
//!   separately granted authority; the family grant never implies them.
//! - Argv-first: process invocation over file content stays argv-first with
//!   validated arguments and no shell-string construction (`P0-AC-009`). File
//!   bytes are data, never instructions; see [`argv_first_ok`].
//! - No migration: a `terminal.*` grant never implies a grant in this family
//!   and vice versa. The gate never consults terminal grants and the terminal
//!   surface never consults this gate.
//!
//! # Non-goals
//!
//! No `unsafe`, no I/O, no new dependency (`std` plus sibling host types
//! only). Pure data plus validation, headlessly testable.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::capability::CapabilityFamily;
use crate::fs_authz::{MAX_FS_PATH_BYTES, SensitivePathPolicy, content_looks_secret};
use crate::manifest::{MAX_FS_PATTERNS_PER_KIND, MAX_PATTERN_TEXT_BYTES, PluginId};
use crate::trust_levels::TrustLevel;

// ── version / family / verbs ────────────────────────────────────────────────

/// Host surface version of the filesystem bridge.
///
/// Capability-registry stable from acceptance: identifier choice is a
/// compatibility decision, so a version mismatch disables the surface with a
/// diagnostic instead of serving operations under unknown semantics.
pub const FS_BRIDGE_VERSION: u32 = 1;

/// Capability family label for this surface (never under `terminal.*`).
pub const FS_FAMILY: &str = "fs";

/// Decided verb set for `bitty.fs.*` (RFC-0005 reconciliation).
pub const FS_VERBS: &[&str] = &["read", "write", "list"];

/// Maximum bytes of one grant pattern (precedent: manifest
/// `FilesystemRequest` per-pattern bound, `512`).
pub const FS_PATTERN_MAX_BYTES: usize = 512;

/// Decided filesystem verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsVerb {
    /// Bounded reads of file bytes under the read grant.
    Read,
    /// Bounded writes of file bytes under the write grant, with the
    /// disposition (create, overwrite, append-mode) as a flag, not a verb.
    Write,
    /// Bounded directory listings (names plus file-kind metadata, never file
    /// bytes) authorized by the read grant over the listed prefix.
    List,
}

impl FsVerb {
    /// Parse a verb label; `open`, `append`, `watch`, `subscribe`,
    /// `tail`, `stream`, `handle`, and every other spelling fail closed.
    pub fn parse(s: &str) -> Result<Self, FsBridgeError> {
        match s {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "list" => Ok(Self::List),
            "open" | "append" | "watch" | "subscribe" | "tail" | "stream" | "handle"
            | "handles" | "cursor" | "follow" => Err(FsBridgeError::Disabled {
                diagnostic: format!(
                    "fs verb '{s}' is not a decided verb (decided: read, write, list; \
                     open rejected, append is a write-disposition flag, \
                     no watch/handles/streaming)"
                ),
            }),
            _ => Err(FsBridgeError::Disabled {
                diagnostic: format!("unknown fs verb '{s}' (decided: read, write, list)"),
            }),
        }
    }

    /// Stable verb label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
        }
    }

    /// Whether this verb is authorized by the read grant (`read` and `list`)
    /// or the write grant (`write`).
    #[must_use]
    pub const fn is_read_class(self) -> bool {
        match self {
            Self::Read | Self::List => true,
            Self::Write => false,
        }
    }
}

// ── grants ──────────────────────────────────────────────────────────────────

/// Grant shape: standing per-plugin grants versus single-use per-request
/// (L3) / per-invocation (L4) grants issued by Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsGrantKind {
    /// Explicit per-plugin grant carrying explicit path patterns (L1/L2).
    Standing,
    /// Single-use Core-issued grant (L3 native sidecar per-request, L4
    /// external-tool per-invocation). Consumed by one successful operation.
    PerRequest {
        /// Remaining uses; decremented on success, never replenished here.
        uses_left: u32,
    },
}

/// Per-plugin scoped grant recorded by the Core gate.
///
/// Grants never bundle modes: a read grant never implies write and a write
/// grant never implies read-back. Listing rides the read grant. Absence of a
/// grant, or an operation outside the grant scope, denies fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsGrant {
    plugin: PluginId,
    read: bool,
    patterns: BTreeSet<String>,
    kind: FsGrantKind,
    revoked: bool,
    /// Monotonic host tick at which the grant expires (`None` = no expiry).
    expires_at: Option<u64>,
}

impl FsGrant {
    /// Record a standing grant (L1/L2 path) with explicit path patterns.
    ///
    /// Fails closed on unscoped (empty, `*`, `**`, `all`), over-bound
    /// (`> 32` patterns, `> 8 KiB` total text, `> 512` per pattern),
    /// control/whitespace-bearing, or hostile patterns (absolute escape,
    /// traversal, overbroad home, credential-location, Windows device).
    pub fn standing(
        plugin: PluginId,
        read: bool,
        patterns: &[String],
    ) -> Result<Self, FsBridgeError> {
        Self::new(plugin, read, patterns, FsGrantKind::Standing)
    }

    /// Record a single-use per-request/per-invocation grant (L3/L4 path).
    pub fn per_request(
        plugin: PluginId,
        read: bool,
        patterns: &[String],
    ) -> Result<Self, FsBridgeError> {
        Self::new(
            plugin,
            read,
            patterns,
            FsGrantKind::PerRequest { uses_left: 1 },
        )
    }

    fn new(
        plugin: PluginId,
        read: bool,
        patterns: &[String],
        kind: FsGrantKind,
    ) -> Result<Self, FsBridgeError> {
        validate_grant_patterns(patterns)?;
        let mut set = BTreeSet::new();
        for pattern in patterns {
            set.insert(pattern.clone());
        }
        Ok(Self {
            plugin,
            read,
            patterns: set,
            kind,
            revoked: false,
            expires_at: None,
        })
    }

    /// Owning plugin.
    #[must_use]
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// Whether this is a read grant (`true`) or a write grant (`false`).
    #[must_use]
    pub const fn is_read(&self) -> bool {
        self.read
    }

    /// Granted patterns (sorted, deduplicated).
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        self.patterns.iter().cloned().collect()
    }

    /// Whether this grant is live (not revoked and not expired at `now`).
    #[must_use]
    pub fn is_live(&self, now: u64) -> bool {
        if self.revoked {
            return false;
        }
        match self.expires_at {
            Some(deadline) => now < deadline,
            None => true,
        }
    }

    /// Whether this grant authorizes `verb` (read-class verbs need a read
    /// grant; write needs a write grant).
    #[must_use]
    pub const fn authorizes_verb(&self, verb: FsVerb) -> bool {
        match verb {
            FsVerb::Read | FsVerb::List => self.read,
            FsVerb::Write => !self.read,
        }
    }
}

/// Validate grant patterns fail-closed (no wildcard default, reuse bounds).
fn validate_grant_patterns(patterns: &[String]) -> Result<(), FsBridgeError> {
    if patterns.is_empty() {
        return Err(FsBridgeError::Disabled {
            diagnostic: "fs grant names no path patterns (no wildcard default)".to_string(),
        });
    }
    if patterns.len() > MAX_FS_PATTERNS_PER_KIND {
        return Err(FsBridgeError::Disabled {
            diagnostic: format!(
                "fs grant exceeds {} patterns (actual {})",
                MAX_FS_PATTERNS_PER_KIND,
                patterns.len()
            ),
        });
    }
    let mut total = 0usize;
    for pattern in patterns {
        if pattern.is_empty() {
            return Err(FsBridgeError::Disabled {
                diagnostic: "fs grant pattern must not be empty".to_string(),
            });
        }
        if pattern == "*" || pattern == "**" || pattern.eq_ignore_ascii_case("all") {
            return Err(FsBridgeError::Disabled {
                diagnostic: format!(
                    "fs grant pattern '{pattern}' is an unscoped wildcard (no allow-all)"
                ),
            });
        }
        if pattern.len() > FS_PATTERN_MAX_BYTES {
            return Err(FsBridgeError::Disabled {
                diagnostic: format!("fs grant pattern exceeds {FS_PATTERN_MAX_BYTES} bytes"),
            });
        }
        if pattern
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err(FsBridgeError::Disabled {
                diagnostic: "fs grant pattern must not contain control characters or whitespace"
                    .to_string(),
            });
        }
        if crate::manifest::is_hostile_fs_pattern(pattern) {
            return Err(FsBridgeError::Disabled {
                diagnostic: format!("fs grant pattern '{pattern}' is hostile"),
            });
        }
        total += pattern.len();
    }
    if total > MAX_PATTERN_TEXT_BYTES {
        return Err(FsBridgeError::Disabled {
            diagnostic: format!(
                "fs grant exceeds {MAX_PATTERN_TEXT_BYTES} total pattern bytes (actual {total})"
            ),
        });
    }
    Ok(())
}

// ── typed denials ───────────────────────────────────────────────────────────

/// Complete typed-denial taxonomy (RFC-0005, normative).
///
/// Denials are catchable and fail closed. They are oracle-tight: a denial
/// carries the category code plus the trust level and the family only —
/// never content bytes, foreign identifiers, or any signal distinguishing
/// absent files from denied files. Listings suppress denied entries silently
/// instead of denying the whole listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsBridgeDenialKind {
    /// No per-plugin grant is recorded (deny-by-default).
    MissingGrant,
    /// A grant was recorded but is revoked or expired.
    RevokedOrExpired,
    /// The operation path does not intersect the grant scope (including
    /// upward traversal and cross-root operations).
    ScopeMismatch,
    /// The request path itself is hostile (absolute escape, traversal,
    /// overbroad home, credential-location shape, Windows device).
    HostilePattern,
    /// The path names a sensitive location without active per-path consent.
    SensitivePath,
    /// Secret-shaped content is refused (write path fail-closed).
    SecretContent,
    /// The request exceeds its path, payload, or listing bound, or the
    /// per-plugin operation rate or aggregate byte budget (polling that
    /// reconstitutes a watch denies here).
    OverBoundOrRate,
    /// `bitty --safe` performs no filesystem operations.
    SafeMode,
    /// The trust level or domain admits no standing access here (L4 and
    /// unknown levels/domains deny rather than default).
    UnknownTrustOrDomain,
}

impl FsBridgeDenialKind {
    /// Stable machine-readable code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingGrant => "E_FS_MISSING_GRANT",
            Self::RevokedOrExpired => "E_FS_REVOKED_GRANT",
            Self::ScopeMismatch => "E_FS_SCOPE_MISMATCH",
            Self::HostilePattern => "E_FS_HOSTILE_PATTERN",
            Self::SensitivePath => "E_FS_SENSITIVE_PATH",
            Self::SecretContent => "E_FS_SECRET_CONTENT",
            Self::OverBoundOrRate => "E_FS_OVER_BOUND",
            Self::SafeMode => "E_FS_SAFE_MODE",
            Self::UnknownTrustOrDomain => "E_FS_TRUST_DENIED",
        }
    }

    /// All nine categories, for taxonomy-completeness tests.
    #[must_use]
    pub const fn all() -> &'static [FsBridgeDenialKind] {
        &[
            Self::MissingGrant,
            Self::RevokedOrExpired,
            Self::ScopeMismatch,
            Self::HostilePattern,
            Self::SensitivePath,
            Self::SecretContent,
            Self::OverBoundOrRate,
            Self::SafeMode,
            Self::UnknownTrustOrDomain,
        ]
    }
}

/// A typed, catchable denial: category plus level and family only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsBridgeDenial {
    kind: FsBridgeDenialKind,
    level: TrustLevel,
}

impl FsBridgeDenial {
    /// Denial category.
    #[must_use]
    pub const fn kind(self) -> FsBridgeDenialKind {
        self.kind
    }

    /// Oracle-tight rendering: category code plus level and family only.
    #[must_use]
    pub fn message(self) -> String {
        format!(
            "{} denied for level '{}' (family '{}')",
            self.kind.code(),
            self.level.as_str(),
            FS_FAMILY
        )
    }
}

// ── errors ──────────────────────────────────────────────────────────────────

/// Gate outcome error: either the surface is disabled (version compat or
/// malformed shape) or the operation is denied with a typed denial.
///
/// Version mismatch and malformed shapes disable with a diagnostic and are
/// NOT denials: they carry no denial category and must not be mistaken for
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsBridgeError {
    /// The surface is disabled: client/host version mismatch or a malformed
    /// verb, path, or grant shape. Carries a diagnostic, never a denial.
    Disabled {
        /// Human-readable diagnostic (versions, offending shape).
        diagnostic: String,
    },
    /// The operation is denied with a typed, catchable denial.
    Denied(FsBridgeDenial),
}

impl FsBridgeError {
    fn denied(kind: FsBridgeDenialKind, level: TrustLevel, _why: &str) -> Self {
        // `_why` is intentionally dropped: denial text must stay oracle-tight
        // (level plus family only). It exists so call sites document the
        // failing gate inline for reviewers.
        Self::Denied(FsBridgeDenial { kind, level })
    }

    /// Stable rendering: diagnostics for disabled, oracle-tight denial text
    /// for denied.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Disabled { diagnostic } => format!("fs surface disabled: {diagnostic}"),
            Self::Denied(denial) => denial.message(),
        }
    }

    /// Denial category when denied; `None` when disabled.
    #[must_use]
    pub const fn denial_kind(&self) -> Option<FsBridgeDenialKind> {
        match self {
            Self::Disabled { .. } => None,
            Self::Denied(denial) => Some(denial.kind),
        }
    }
}

impl std::fmt::Display for FsBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for FsBridgeError {}

// ── records, labels, attribution ────────────────────────────────────────────

/// Core-attached untrusted-observation label.
///
/// Every record carries this label as a separate typed field applied AFTER
/// redaction and truncation, so it survives both. Consumers (plugins,
/// agents, tools) must treat labeled content as observation data under the
/// prompt-injection rule, never as instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FsLabel;

impl FsLabel {
    /// Stable label value.
    pub const VALUE: &'static str = "untrusted-observation";

    /// Stable label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        Self::VALUE
    }
}

/// Row-level attribution carried by every result (plugin, path, verb,
/// timing, byte counts where the operation records them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsAttribution {
    /// Requesting plugin id.
    pub plugin: String,
    /// Operation path or prefix (diagnostic, never a value).
    pub path: String,
    /// Verb label (`read`, `write`, `list`).
    pub verb: String,
    /// Monotonic host tick of the operation.
    pub recorded_at: u64,
    /// Actor trust level label.
    pub actor: String,
}

/// Caller-provided view over filesystem state.
///
/// The gate never opens a file, device, symlink, or native library; Core
/// builds this view from the mediated host boundary. Tests use small
/// fixtures. Paths are stored as supplied (validated at insert); matching
/// uses [`crate::fs_authz::FilesystemScope`] so grant evaluation cannot drift
/// from the accepted authorization layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsView {
    files: BTreeMap<String, String>,
}

impl FsView {
    /// Empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a file with content (test and host wiring only).
    ///
    /// Fails closed on malformed paths (empty, over-bound, NUL/controls);
    /// hostile shapes are rejected here as well so fixtures cannot smuggle a
    /// grant-shape attack through the view.
    pub fn insert(&mut self, path: &str, content: &str) -> Result<(), FsBridgeError> {
        validate_request_path(path).map_err(|diagnostic| FsBridgeError::Disabled { diagnostic })?;
        if crate::manifest::is_hostile_fs_pattern(path) {
            return Err(FsBridgeError::Disabled {
                diagnostic: format!("fs view path '{path}' is hostile"),
            });
        }
        self.files.insert(path.to_string(), content.to_string());
        Ok(())
    }

    fn get(&self, path: &str) -> Option<&String> {
        self.files.get(path)
    }

    /// Immediate children of `prefix` (one level, sorted).
    fn children_of(&self, prefix: &str) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let base = prefix.trim_end_matches(['/', '\\']);
        for path in self.files.keys() {
            if base.is_empty() {
                continue;
            }
            if path == base {
                continue;
            }
            let rest = path
                .strip_prefix(base)
                .and_then(|r| r.strip_prefix('/').or_else(|| r.strip_prefix('\\')));
            let Some(rest) = rest else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            let first = rest.split(['/', '\\']).next().unwrap_or(rest);
            if first.is_empty() {
                continue;
            }
            let child = format!("{base}/{first}");
            if !seen.insert(child.clone()) {
                continue;
            }
            let is_dir = self.files.keys().any(|p| {
                p != &child && p.starts_with(&format!("{child}/"))
                    || p.starts_with(&format!("{}/", child.replace('/', "\\")))
            });
            // A child is a dir when any stored path nests below it on either
            // separator; otherwise it names a file present in the view.
            let nested = self.files.keys().any(|p| {
                p.len() > child.len() && p.starts_with(&child) && {
                    let tail = &p[child.len()..];
                    tail.starts_with('/') || tail.starts_with('\\')
                }
            });
            let _ = is_dir;
            out.push((child, nested));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

// ── requests and results ────────────────────────────────────────────────────

/// Bounded read request over current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsReadRequest {
    /// Client surface version; must equal [`FS_BRIDGE_VERSION`].
    pub client_version: u32,
    /// File path to read.
    pub path: String,
    /// Explicit result byte bound (`> 0`).
    pub max_bytes: u32,
}

/// Bounded write request over current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWriteRequest {
    /// Client surface version; must equal [`FS_BRIDGE_VERSION`].
    pub client_version: u32,
    /// File path to write.
    pub path: String,
    /// File bytes to persist.
    pub content: String,
    /// Write disposition flag (candidate, not a verb): `false` creates or
    /// overwrites, `true` appends. Whether the write grant subdivides by
    /// disposition stays parked; the default is a single `write` verb with
    /// this flag.
    pub append: bool,
}

/// Bounded listing request over current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsListRequest {
    /// Client surface version; must equal [`FS_BRIDGE_VERSION`].
    pub client_version: u32,
    /// Directory prefix to list.
    pub prefix: String,
    /// Explicit entry bound (`> 0`).
    pub max_entries: u32,
    /// Explicit result byte bound for entry names (`> 0`).
    pub max_bytes: u32,
}

/// One redacted, truncated, attributed, labeled file record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsReadResult {
    /// Redacted and truncated body (truncation marked, never silent).
    pub body: String,
    /// True when the body was truncated to the byte cap.
    pub truncated: bool,
    /// True: the body passed secret-shape redaction before reaching the gate
    /// (or was served as the redaction marker).
    pub redacted: bool,
    /// Operation attribution.
    pub attribution: FsAttribution,
    /// Core-attached untrusted-observation label (survives truncation).
    pub label: FsLabel,
    /// Export bytes: identical truncation to the preview path, so an export
    /// preview equals the actual export byte-for-byte.
    pub export_preview_equal: bool,
}

impl FsReadResult {
    /// Export bytes: the served body.
    #[must_use]
    pub fn export_bytes(&self) -> &str {
        &self.body
    }

    /// Preview bytes: same pipeline as export.
    #[must_use]
    pub fn preview_bytes(&self) -> &str {
        &self.body
    }
}

/// Write receipt (no content bytes cross back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWriteReceipt {
    /// Path written (diagnostic, never a value).
    pub path: String,
    /// Bytes persisted for this operation.
    pub bytes_written: u64,
    /// Whether the write appended (`true`) or created/overwrote (`false`).
    pub appended: bool,
    /// Operation attribution.
    pub attribution: FsAttribution,
    /// Core-attached label (receipts carry the label so downstream
    /// combination sites treat the receipt as observation data).
    pub label: FsLabel,
}

/// One listing entry (name plus file-kind metadata, never file bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsListEntry {
    /// Entry path (diagnostic, never bytes).
    pub name: String,
    /// Entry kind: `file` or `dir`.
    pub kind: String,
    /// Entry attribution (plugin, prefix, timing).
    pub attribution: FsAttribution,
    /// Core-attached label (listings carry the label so consumers treat
    /// names as observation data).
    pub label: FsLabel,
}

/// Bounded listing page (cursor-free, snapshot-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsListPage {
    /// Visible entries in sorted order (denied entries suppressed silently).
    pub entries: Vec<FsListEntry>,
    /// Entries in scope at snapshot time (may exceed the returned page).
    pub total_in_scope: u64,
    /// Always point-in-time with no freshness promise.
    pub point_in_time: bool,
}

// ── budgets ─────────────────────────────────────────────────────────────────

/// Caller-provided operation and budget ceilings.
///
/// All bounds arrive from Core configuration; this module mints no numeric
/// ceiling. Zero bounds are rejected as misconfiguration: lockdown is
/// expressed through safe mode, never through a silent zero that denies
/// every operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsCaps {
    /// Maximum bytes per read result.
    pub max_bytes_per_read: u32,
    /// Maximum bytes per write payload (and per stored file).
    pub max_bytes_per_write: u32,
    /// Maximum entries per listing.
    pub max_entries_per_list: u32,
    /// Maximum entry-name bytes per listing.
    pub max_bytes_per_list: u32,
    /// Maximum successful operations per plugin per window.
    pub max_ops_per_window: u64,
    /// Maximum successful result bytes per plugin per window.
    pub max_bytes_per_window: u64,
}

impl FsCaps {
    /// Build ceilings, rejecting zero bounds fail-closed as misconfiguration.
    pub fn new(
        max_bytes_per_read: u32,
        max_bytes_per_write: u32,
        max_entries_per_list: u32,
        max_bytes_per_list: u32,
        max_ops_per_window: u64,
        max_bytes_per_window: u64,
    ) -> Result<Self, FsBridgeError> {
        if max_bytes_per_read == 0
            || max_bytes_per_write == 0
            || max_entries_per_list == 0
            || max_bytes_per_list == 0
            || max_ops_per_window == 0
            || max_bytes_per_window == 0
        {
            return Err(FsBridgeError::Disabled {
                diagnostic: "fs caps must all be nonzero (lockdown uses safe mode, not zero caps)"
                    .to_string(),
            });
        }
        Ok(Self {
            max_bytes_per_read,
            max_bytes_per_write,
            max_entries_per_list,
            max_bytes_per_list,
            max_ops_per_window,
            max_bytes_per_window,
        })
    }
}

/// Per-plugin window usage with per-plugin attribution (`P0-AC-014`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WindowUsage {
    ops: u64,
    bytes: u64,
}

// ── gate ────────────────────────────────────────────────────────────────────

/// Enforcement point for the filesystem surface.
///
/// Holds grants, budgets, safe mode, the compat version, per-plugin window
/// usage, and the sensitive-path policy. Check order is fail-closed and
/// oracle-aware: safe mode, trust admission, version compat, syntactic shape
/// (invalid, hostile), grant presence, revocation/expiry, scope intersection,
/// sensitive-path consent, secret treatment, static bounds plus window
/// budgets — then bytes. Safe mode first so `bitty --safe` performs nothing
/// identically for every level and version. Scope is checked before
/// sensitive state so out-of-scope callers learn nothing about sensitive or
/// secret state; static bounds and budgets are checked only for authorized
/// callers so denials never oracle bound or budget facts.
#[derive(Debug, Clone)]
pub struct FsGate {
    grants: Vec<FsGrant>,
    caps: FsCaps,
    policy: SensitivePathPolicy,
    safe_mode: bool,
    host_version: u32,
    now: u64,
    usage: BTreeMap<String, WindowUsage>,
}

impl FsGate {
    /// Create a gate with caller-provided ceilings.
    pub fn new(caps: FsCaps) -> Self {
        Self {
            grants: Vec::new(),
            caps,
            policy: SensitivePathPolicy::default_policy(),
            safe_mode: false,
            host_version: FS_BRIDGE_VERSION,
            now: 0,
            usage: BTreeMap::new(),
        }
    }

    /// Record a grant (Core consent path).
    pub fn issue_grant(&mut self, grant: FsGrant) {
        self.grants.push(grant);
    }

    /// Revoke every grant for `plugin` with this access (`true` = read,
    /// `false` = write).
    pub fn revoke(&mut self, plugin: &PluginId, read: bool) {
        for grant in &mut self.grants {
            if grant.plugin == *plugin && grant.read == read {
                grant.revoked = true;
            }
        }
    }

    /// Enter or leave safe mode (`bitty --safe` performs nothing).
    pub fn set_safe_mode(&mut self, safe: bool) {
        self.safe_mode = safe;
    }

    /// Override the host surface version (compat tests only; production
    /// stays at [`FS_BRIDGE_VERSION`]).
    pub fn set_host_version(&mut self, version: u32) {
        self.host_version = version;
    }

    /// Advance the monotonic host tick (grant expiry).
    pub fn advance_time(&mut self, delta: u64) {
        self.now = self.now.saturating_add(delta);
    }

    /// Open a new rate window (resets per-plugin usage).
    pub fn advance_window(&mut self) {
        self.usage.clear();
    }

    /// Grant explicit user consent for exactly one sensitive path.
    pub fn grant_consent(&mut self, path: &str) -> Result<(), FsBridgeError> {
        self.policy
            .grant_consent(path, self.now, None)
            .map_err(|e| FsBridgeError::Disabled {
                diagnostic: format!("fs consent malformed: {e}"),
            })
    }

    /// Revoke per-path consent (missing grants are a no-op).
    pub fn revoke_consent(&mut self, path: &str) {
        self.policy.revoke_consent(path);
    }

    /// Per-plugin window usage (attribution evidence for `P0-AC-014`).
    #[must_use]
    pub fn window_usage(&self, plugin: &PluginId) -> (u64, u64) {
        self.usage
            .get(plugin.as_str())
            .map(|usage| (usage.ops, usage.bytes))
            .unwrap_or((0, 0))
    }

    /// Whether `grant` is eligible for `level` right now: owned by
    /// `plugin`, covering `verb`, live, and of the kind the level admits
    /// (standing for L1/L2, single-use per-request for L3/L4).
    fn grant_is_usable(
        &self,
        grant: &FsGrant,
        plugin: &PluginId,
        verb: FsVerb,
        level: TrustLevel,
    ) -> bool {
        grant.plugin == *plugin
            && grant.authorizes_verb(verb)
            && grant.is_live(self.now)
            && match level {
                TrustLevel::BundledLua | TrustLevel::ThirdPartyLua => {
                    matches!(grant.kind, FsGrantKind::Standing)
                }
                TrustLevel::NativeSidecar | TrustLevel::ExternalTool => {
                    matches!(grant.kind, FsGrantKind::PerRequest { uses_left: 1.. })
                }
                TrustLevel::Core => false,
            }
    }

    /// Whether `path` is covered by at least one of `patterns` (grant-side
    /// scope matching through the accepted authorization layer).
    fn scope_covers(patterns: &[String], path: &str) -> bool {
        let scope = match crate::fs_authz::FilesystemScope::from_patterns(patterns) {
            Ok(scope) => scope,
            Err(_) => return false,
        };
        scope.allows(path)
    }

    /// Charge one successful operation to the per-plugin window.
    fn charge(
        &mut self,
        plugin: &PluginId,
        bytes: u64,
        level: TrustLevel,
        slot: Option<usize>,
    ) -> Result<(), FsBridgeError> {
        let key = plugin.as_str().to_string();
        let used = self.usage.get(&key).copied().unwrap_or_default();
        if used.ops >= self.caps.max_ops_per_window
            || used.bytes >= self.caps.max_bytes_per_window
            || used.bytes.saturating_add(bytes) > self.caps.max_bytes_per_window
        {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over window budget",
            ));
        }
        let entry = self.usage.entry(key).or_default();
        entry.ops += 1;
        entry.bytes += bytes;
        if matches!(level, TrustLevel::NativeSidecar | TrustLevel::ExternalTool) {
            if let Some(idx) = slot {
                if let FsGrantKind::PerRequest { uses_left } = &mut self.grants[idx].kind {
                    *uses_left = uses_left.saturating_sub(1);
                }
            }
        }
        Ok(())
    }

    fn check_common(
        &self,
        plugin: &PluginId,
        level: TrustLevel,
        verb: FsVerb,
        client_version: u32,
        path: &str,
    ) -> Result<Option<usize>, FsBridgeError> {
        // 1. Safe mode performs nothing, identically for every level,
        //    version, grant, and store content.
        if self.safe_mode {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::SafeMode,
                level,
                "safe mode performs no filesystem operations",
            ));
        }

        // 2. Trust admission before grant intersection (P0-AC-035): the
        //    family maps to the `filesystem` domain. L4 admits nothing at the
        //    domain gate; a live per-request/per-invocation grant is the only
        //    exception (checked at step 4).
        let per_request_live = self.grants.iter().any(|grant| {
            grant.plugin == *plugin
                && grant.is_live(self.now)
                && matches!(grant.kind, FsGrantKind::PerRequest { uses_left: 1.. })
        });
        if level.check_family(CapabilityFamily::Fs).is_err() && !per_request_live {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::UnknownTrustOrDomain,
                level,
                "level admits no standing filesystem access",
            ));
        }

        // 3. Version compat: mismatch disables with a diagnostic (never a
        //    denial category).
        if client_version != self.host_version {
            return Err(FsBridgeError::Disabled {
                diagnostic: format!(
                    "fs surface version mismatch (client {client_version}, host {}); surface disabled",
                    self.host_version
                ),
            });
        }

        // 4. Syntactic shape: malformed paths disable (diagnostic, not a
        //    denial); hostile shapes deny as hostile-pattern.
        validate_request_path(path).map_err(|diagnostic| FsBridgeError::Disabled { diagnostic })?;
        if crate::manifest::is_hostile_fs_pattern(path) {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::HostilePattern,
                level,
                "hostile path shape",
            ));
        }

        // 5. Grant presence (deny-by-default) with the L1/L2 vs L3/L4 kind
        //    split. L0 (Core enforcement itself) needs no plugin grant and
        //    skips grant/scope checks; bounds, budgets, and sensitive layers
        //    below still apply. A plugin may hold several grants: the gate
        //    selects a live grant of the right kind whose scope covers the
        //    path, and reports ScopeMismatch only when eligible live grants
        //    exist but none of them covers it.
        if level == TrustLevel::Core {
            return Ok(None);
        }
        let any_usable = self
            .grants
            .iter()
            .any(|grant| self.grant_is_usable(grant, plugin, verb, level));
        let matching_idx = self.grants.iter().position(|grant| {
            self.grant_is_usable(grant, plugin, verb, level)
                && Self::scope_covers(&grant.patterns(), path)
        });
        if any_usable && matching_idx.is_none() {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::ScopeMismatch,
                level,
                "operation path outside grant scope",
            ));
        }
        let recorded = self
            .grants
            .iter()
            .any(|grant| grant.plugin == *plugin && grant.authorizes_verb(verb));
        let Some(idx) = matching_idx else {
            if recorded
                && self.grants.iter().any(|grant| {
                    grant.plugin == *plugin
                        && grant.authorizes_verb(verb)
                        && (!grant.is_live(self.now) || grant.revoked)
                })
            {
                return Err(FsBridgeError::denied(
                    FsBridgeDenialKind::RevokedOrExpired,
                    level,
                    "grant revoked or expired",
                ));
            }
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::MissingGrant,
                level,
                "no usable grant",
            ));
        };
        Ok(Some(idx))
    }

    fn check_sensitive(&self, level: TrustLevel, path: &str) -> Result<(), FsBridgeError> {
        if self.policy.is_sensitive(path) && !self.consented(path) {
            // Consent is keyed by normalized path; without active consent the
            // operation denies with the sensitive-path category (oracle-tight:
            // level plus family only, never the consent state).
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::SensitivePath,
                level,
                "sensitive path without consent",
            ));
        }
        Ok(())
    }

    /// Whether active per-path user consent covers `path` right now.
    ///
    /// Consent is keyed by normalized path
    /// ([`crate::fs_authz::SensitivePathPolicy::grant_consent`]); hostile or
    /// otherwise unrepresentable paths never normalize, so they never carry
    /// consent and stay denied.
    fn consented(&self, path: &str) -> bool {
        crate::fs_authz::normalize_request_path(path)
            .is_some_and(|normalized| self.policy.consent_active(&normalized, self.now))
    }

    /// Bounded read of file bytes under the read grant.
    pub fn read(
        &mut self,
        plugin: &PluginId,
        level: TrustLevel,
        req: &FsReadRequest,
        view: &FsView,
    ) -> Result<FsReadResult, FsBridgeError> {
        let slot = self.check_common(plugin, level, FsVerb::Read, req.client_version, &req.path)?;

        // Sensitive gate before content: out-of-scope callers already denied
        // above, so this denial never oracles sensitive state to them.
        // Consent-aware callers pass here only with active per-path consent;
        // the policy check below still denies without it.
        self.check_sensitive(level, &req.path)?;

        // Static bounds: explicit byte bound must be within the cap; never
        // clamp silently.
        if req.max_bytes == 0 || req.max_bytes > self.caps.max_bytes_per_read {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over per-read byte bound",
            ));
        }
        let key = plugin.as_str().to_string();
        let used = self.usage.get(&key).copied().unwrap_or_default();
        if used.ops >= self.caps.max_ops_per_window || used.bytes >= self.caps.max_bytes_per_window
        {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over window budget",
            ));
        }

        // Absent and denied are indistinguishable (no absent-vs-denied
        // signal): a path in scope but absent from the view denies exactly
        // like an out-of-scope path.
        let Some(content) = view.get(&req.path) else {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::ScopeMismatch,
                level,
                "path not visible under grant",
            ));
        };

        // Secret treatment: secret-shaped reads are served redacted with the
        // label, never raw. Over-bound scan content reports secret-shaped
        // (fail-closed over-reject) and follows the same redacted path.
        let secret = content_looks_secret(content);
        let mut body = if secret {
            crate::secrets::SECRET_REDACTED_MARKER.to_string()
        } else {
            content.clone()
        };
        let mut truncated = false;
        let cap = (req.max_bytes.min(self.caps.max_bytes_per_read)) as usize;
        if body.len() > cap {
            let mut end = cap;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            truncated = true;
        }
        let bytes = body.len() as u64;
        self.charge(plugin, bytes, level, slot)?;
        Ok(FsReadResult {
            body,
            truncated,
            redacted: true,
            attribution: FsAttribution {
                plugin: plugin.as_str().to_string(),
                path: req.path.clone(),
                verb: FsVerb::Read.as_str().to_string(),
                recorded_at: self.now,
                actor: level.as_str().to_string(),
            },
            label: FsLabel,
            export_preview_equal: true,
        })
    }

    /// Bounded write of file bytes under the write grant.
    pub fn write(
        &mut self,
        plugin: &PluginId,
        level: TrustLevel,
        req: &FsWriteRequest,
        view: &mut FsView,
    ) -> Result<FsWriteReceipt, FsBridgeError> {
        let slot =
            self.check_common(plugin, level, FsVerb::Write, req.client_version, &req.path)?;
        self.check_sensitive(level, &req.path)?;

        // Secret-shaped writes refuse fail-closed (no redacted write path):
        // persisting secret-shaped bytes without an explicit secret flow
        // would launder them into the granted scope. Check the bytes that
        // will actually be stored, not only the payload, so split appends
        // cannot assemble a secret-shaped file from innocent halves.
        let existing = view.get(&req.path).cloned().unwrap_or_default();
        let next = if req.append {
            format!("{existing}{}", req.content)
        } else {
            req.content.clone()
        };
        if content_looks_secret(&next) {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::SecretContent,
                level,
                "secret-shaped write refused",
            ));
        }

        // Static bounds: payload must fit the per-write cap; append must fit
        // the resulting file within the same cap.
        if req.content.len() > self.caps.max_bytes_per_write as usize {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over per-write byte bound",
            ));
        }
        if next.len() > self.caps.max_bytes_per_write as usize {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over per-file byte bound",
            ));
        }
        let bytes = req.content.len() as u64;
        self.charge(plugin, bytes, level, slot)?;
        view.files.insert(req.path.clone(), next);
        Ok(FsWriteReceipt {
            path: req.path.clone(),
            bytes_written: bytes,
            appended: req.append,
            attribution: FsAttribution {
                plugin: plugin.as_str().to_string(),
                path: req.path.clone(),
                verb: FsVerb::Write.as_str().to_string(),
                recorded_at: self.now,
                actor: level.as_str().to_string(),
            },
            label: FsLabel,
        })
    }

    /// Bounded directory listing under the read grant over the listed prefix.
    ///
    /// Silent-skip default: entries outside the grant scope or naming
    /// sensitive locations without consent are suppressed silently (omitted,
    /// never marked), so the page leaks no absent-vs-denied signal. The
    /// prefix itself is checked explicitly: a hostile, out-of-scope, or
    /// sensitive prefix denies with the typed category.
    pub fn list(
        &mut self,
        plugin: &PluginId,
        level: TrustLevel,
        req: &FsListRequest,
        view: &FsView,
    ) -> Result<FsListPage, FsBridgeError> {
        let slot =
            self.check_common(plugin, level, FsVerb::List, req.client_version, &req.prefix)?;
        self.check_sensitive(level, &req.prefix)?;

        if req.max_entries == 0 || req.max_entries > self.caps.max_entries_per_list {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over per-listing entry bound",
            ));
        }
        if req.max_bytes == 0 || req.max_bytes > self.caps.max_bytes_per_list {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over per-listing byte bound",
            ));
        }
        let key = plugin.as_str().to_string();
        let used = self.usage.get(&key).copied().unwrap_or_default();
        if used.ops >= self.caps.max_ops_per_window || used.bytes >= self.caps.max_bytes_per_window
        {
            return Err(FsBridgeError::denied(
                FsBridgeDenialKind::OverBoundOrRate,
                level,
                "over window budget",
            ));
        }

        // Collect immediate children, then silent-skip denied entries: an
        // entry is visible only when a live read grant of the right kind
        // covers it and the sensitive policy clears it. Denied entries are
        // omitted without marking; the page never names them.
        let mut visible: Vec<(String, bool)> = Vec::new();
        for (child, is_dir) in view.children_of(&req.prefix) {
            let covered = self.grants.iter().any(|grant| {
                self.grant_is_usable(grant, plugin, FsVerb::List, level)
                    && Self::scope_covers(&grant.patterns(), &child)
            });
            if level == TrustLevel::Core
                && self.policy.is_sensitive(&child)
                && !self.consented(&child)
            {
                continue;
            }
            if level != TrustLevel::Core && !covered {
                continue;
            }
            if self.policy.is_sensitive(&child) && !self.consented(&child) {
                continue;
            }
            visible.push((child, is_dir));
        }
        visible.sort_by(|a, b| a.0.cmp(&b.0));
        let total_in_scope = visible.len() as u64;

        // Bound the page explicitly (snapshot-only, no cursor): excess rows
        // stay counted in `total_in_scope` but are not served.
        let mut entries: Vec<FsListEntry> = Vec::new();
        let mut bytes: u64 = 0;
        for (name, is_dir) in visible.into_iter().take(req.max_entries as usize) {
            let name_bytes = name.len() as u64;
            if bytes.saturating_add(name_bytes) > req.max_bytes as u64
                || bytes.saturating_add(name_bytes) > self.caps.max_bytes_per_list as u64
            {
                break;
            }
            bytes += name_bytes;
            entries.push(FsListEntry {
                name: name.clone(),
                kind: if is_dir {
                    "dir".to_string()
                } else {
                    "file".to_string()
                },
                attribution: FsAttribution {
                    plugin: plugin.as_str().to_string(),
                    path: name,
                    verb: FsVerb::List.as_str().to_string(),
                    recorded_at: self.now,
                    actor: level.as_str().to_string(),
                },
                label: FsLabel,
            });
        }
        self.charge(plugin, bytes, level, slot)?;
        Ok(FsListPage {
            entries,
            total_in_scope,
            point_in_time: true,
        })
    }
}

/// Validate one request path (malformed shapes disable, never deny).
fn validate_request_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("fs path must not be empty".to_string());
    }
    if path.len() > MAX_FS_PATH_BYTES {
        return Err(format!("fs path exceeds {MAX_FS_PATH_BYTES} bytes"));
    }
    if path.contains('\0') || path.chars().any(|c| c.is_control()) {
        return Err("fs path must not contain NUL or control characters".to_string());
    }
    Ok(())
}

/// Argv-first combination check for file content routed to process spawn.
///
/// File bytes are data, never instructions: a caller that spawns a process
/// over file content must pass the bytes as validated argv entries through
/// the allowlisted spawn surface, never through shell-string construction or
/// interpolation. This predicate mirrors the spawn bridge shape (non-empty,
/// bounded entries, no NUL) without inventing a new ceiling: the per-entry
/// bound reuses [`MAX_FS_PATH_BYTES`] (`4096`) as the loosest legitimate
/// argv entry, tighter per-tool bounds stay host-side.
#[must_use]
pub fn argv_first_ok(args: &[String]) -> bool {
    if args.is_empty() || args.len() > 64 {
        return false;
    }
    for arg in args {
        if arg.is_empty() || arg.len() > MAX_FS_PATH_BYTES || arg.contains('\0') {
            return false;
        }
        // Shell-string construction is never argv-first: an entry carrying
        // shell metacharacters as control syntax fails the combination. Data
        // entries that merely contain those bytes as data must be passed as
        // opaque argv entries, never interpolated — this predicate rejects
        // entries that would be interpreted by a shell, so callers keep them
        // as data.
        if arg.contains('\0') {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityId;

    fn pid(s: &str) -> PluginId {
        PluginId::new(s).unwrap()
    }

    /// Test-only ceilings (fixtures, not normative; exact defaults stay
    /// parked to the Core bridge and SDK work).
    fn caps() -> FsCaps {
        FsCaps::new(4096, 4096, 16, 8192, 4, 16384).unwrap()
    }

    fn read_grant(plugin: &PluginId, patterns: &[&str]) -> FsGrant {
        let owned: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        FsGrant::standing(plugin.clone(), true, &owned).unwrap()
    }

    fn write_grant(plugin: &PluginId, patterns: &[&str]) -> FsGrant {
        let owned: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        FsGrant::standing(plugin.clone(), false, &owned).unwrap()
    }

    fn read_req(path: &str) -> FsReadRequest {
        FsReadRequest {
            client_version: FS_BRIDGE_VERSION,
            path: path.to_string(),
            max_bytes: 4096,
        }
    }

    fn write_req(path: &str, content: &str, append: bool) -> FsWriteRequest {
        FsWriteRequest {
            client_version: FS_BRIDGE_VERSION,
            path: path.to_string(),
            content: content.to_string(),
            append,
        }
    }

    fn list_req(prefix: &str) -> FsListRequest {
        FsListRequest {
            client_version: FS_BRIDGE_VERSION,
            prefix: prefix.to_string(),
            max_entries: 16,
            max_bytes: 8192,
        }
    }

    fn seeded_view() -> FsView {
        let mut view = FsView::new();
        view.insert("~/docs/notes.txt", "hello notes").unwrap();
        view.insert("~/docs/todo.txt", "todo body").unwrap();
        view.insert("~/docs/sub/nested.txt", "nested body").unwrap();
        view
    }

    fn granted_gate(plugin: &PluginId) -> FsGate {
        let mut gate = FsGate::new(caps());
        gate.issue_grant(read_grant(plugin, &["~/docs/**"]));
        gate.issue_grant(write_grant(plugin, &["~/docs/out/**"]));
        gate
    }

    // ── verbs ────────────────────────────────────────────────────────────

    #[test]
    fn decided_verbs_parse_and_rejected_verbs_fail() {
        assert_eq!(FsVerb::parse("read").unwrap(), FsVerb::Read);
        assert_eq!(FsVerb::parse("write").unwrap(), FsVerb::Write);
        assert_eq!(FsVerb::parse("list").unwrap(), FsVerb::List);
        assert!(FsVerb::parse("open").is_err());
        assert!(FsVerb::parse("append").is_err());
        assert!(FsVerb::parse("watch").is_err());
        assert!(FsVerb::parse("subscribe").is_err());
        assert!(FsVerb::parse("tail").is_err());
        assert!(FsVerb::parse("stream").is_err());
        assert!(FsVerb::parse("handle").is_err());
        assert!(FsVerb::parse("cursor").is_err());
        assert_eq!(FS_VERBS, &["read", "write", "list"]);
        assert!(FsVerb::Read.is_read_class());
        assert!(FsVerb::List.is_read_class());
        assert!(!FsVerb::Write.is_read_class());
    }

    #[test]
    fn append_is_a_write_disposition_flag_not_a_verb() {
        let plugin = pid("xuepoo.files");
        let mut gate = FsGate::new(caps());
        gate.issue_grant(write_grant(&plugin, &["~/docs/out/**"]));
        let mut view = FsView::new();
        let receipt = gate
            .write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/out/a.txt", "one", false),
                &mut view,
            )
            .unwrap();
        assert!(!receipt.appended);
        let receipt = gate
            .write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/out/a.txt", "+two", true),
                &mut view,
            )
            .unwrap();
        assert!(receipt.appended);
        assert_eq!(view.get("~/docs/out/a.txt").unwrap(), "one+two");
        // No `append` verb exists to parse.
        assert!(FsVerb::parse("append").is_err());
    }

    #[test]
    fn list_rides_the_read_grant_never_the_write_grant() {
        let plugin = pid("xuepoo.files");
        let mut gate = FsGate::new(caps());
        gate.issue_grant(write_grant(&plugin, &["~/docs/**"]));
        let view = seeded_view();
        // A write grant alone never authorizes a listing.
        assert_eq!(
            gate.list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/docs"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );
        // A read grant does.
        gate.issue_grant(read_grant(&plugin, &["~/docs/**"]));
        let page = gate
            .list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/docs"),
                &view,
            )
            .unwrap();
        assert!(!page.entries.is_empty());
    }

    // ── grants ───────────────────────────────────────────────────────────

    #[test]
    fn scoped_grants_deny_without_wildcard() {
        let plugin = pid("xuepoo.files");
        // Empty, wildcard, and `all` grant requests fail closed at
        // construction (no allow-all).
        for patterns in [
            Vec::<String>::new(),
            vec!["*".to_string()],
            vec!["**".to_string()],
            vec!["all".to_string()],
            vec!["ALL".to_string()],
        ] {
            assert!(FsGrant::standing(plugin.clone(), true, &patterns).is_err());
        }
        // Read never implies write and write never implies read-back.
        let mut gate = FsGate::new(caps());
        gate.issue_grant(read_grant(&plugin, &["~/docs/**"]));
        let mut view = seeded_view();
        assert!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .is_ok()
        );
        assert_eq!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/notes.txt", "x", false),
                &mut view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );
        let mut gate = FsGate::new(caps());
        gate.issue_grant(write_grant(&plugin, &["~/docs/out/**"]));
        let mut view = FsView::new();
        assert!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/out/a.txt", "x", false),
                &mut view
            )
            .is_ok()
        );
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/out/a.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );
    }

    #[test]
    fn grant_matrix_l0_through_l4() {
        let plugin = pid("xuepoo.files");
        let view = seeded_view();
        // L0 is the enforcer and needs no plugin grant: it runs the remaining
        // layers (sensitive, bounds, budgets) without grant intersection.
        let mut gate = FsGate::new(caps());
        assert!(
            gate.read(
                &plugin,
                TrustLevel::Core,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .is_ok(),
            "L0 enforces without a plugin grant"
        );
        // L1/L2 with standing grants allow.
        for level in [TrustLevel::BundledLua, TrustLevel::ThirdPartyLua] {
            let mut gate = granted_gate(&plugin);
            assert!(
                gate.read(&plugin, level, &read_req("~/docs/notes.txt"), &view)
                    .is_ok(),
                "level {level}"
            );
        }
        // L3 with a standing grant denies (per-request only); a live
        // per-request grant allows exactly once.
        let mut gate = granted_gate(&plugin);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::NativeSidecar,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );
        gate.issue_grant(
            FsGrant::per_request(plugin.clone(), true, &["~/docs/**".to_string()]).unwrap(),
        );
        assert!(
            gate.read(
                &plugin,
                TrustLevel::NativeSidecar,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .is_ok()
        );
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::NativeSidecar,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );
        // L4 with a standing grant denies at the domain gate; a live
        // per-invocation grant allows exactly once.
        let mut gate = granted_gate(&plugin);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ExternalTool,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::UnknownTrustOrDomain)
        );
        gate.issue_grant(
            FsGrant::per_request(plugin.clone(), true, &["~/docs/**".to_string()]).unwrap(),
        );
        assert!(
            gate.read(
                &plugin,
                TrustLevel::ExternalTool,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .is_ok()
        );
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ExternalTool,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::UnknownTrustOrDomain)
        );
    }

    // ── bounds reuse ─────────────────────────────────────────────────────

    #[test]
    fn reuses_4096_32_8kib_bounds() {
        assert_eq!(MAX_FS_PATH_BYTES, 4096);
        assert_eq!(MAX_FS_PATTERNS_PER_KIND, 32);
        assert_eq!(MAX_PATTERN_TEXT_BYTES, 8 * 1024);
        // Over-bound paths disable (diagnostic, not a denial category).
        let plugin = pid("xuepoo.files");
        let mut gate = granted_gate(&plugin);
        let view = seeded_view();
        let long = format!("~/docs/{}", "x".repeat(4096));
        assert!(
            gate.read(&plugin, TrustLevel::ThirdPartyLua, &read_req(&long), &view)
                .unwrap_err()
                .denial_kind()
                .is_none()
        );
        // 32 patterns per kind enforced; 33rd fails closed.
        let many: Vec<String> = (0..33).map(|i| format!("~/docs/d{i}/**")).collect();
        assert!(FsGrant::standing(plugin.clone(), true, &many).is_err());
        let ok: Vec<String> = (0..32).map(|i| format!("~/docs/d{i}/**")).collect();
        assert!(FsGrant::standing(plugin.clone(), true, &ok).is_ok());
        // 8 KiB total pattern text enforced.
        let big: Vec<String> = vec!["~/docs/".to_string() + &"x".repeat(8192)];
        assert!(FsGrant::standing(plugin.clone(), true, &big).is_err());
    }

    // ── denials ──────────────────────────────────────────────────────────

    #[test]
    fn denial_taxonomy_is_complete_and_coded() {
        assert_eq!(FsBridgeDenialKind::all().len(), 9);
        let codes: Vec<&str> = FsBridgeDenialKind::all()
            .iter()
            .map(|kind| kind.code())
            .collect();
        assert_eq!(
            codes,
            vec![
                "E_FS_MISSING_GRANT",
                "E_FS_REVOKED_GRANT",
                "E_FS_SCOPE_MISMATCH",
                "E_FS_HOSTILE_PATTERN",
                "E_FS_SENSITIVE_PATH",
                "E_FS_SECRET_CONTENT",
                "E_FS_OVER_BOUND",
                "E_FS_SAFE_MODE",
                "E_FS_TRUST_DENIED",
            ]
        );
    }

    #[test]
    fn each_denial_category_is_reachable() {
        let plugin = pid("xuepoo.files");

        // 1. MissingGrant.
        let mut gate = FsGate::new(caps());
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );

        // 2. RevokedOrExpired.
        let mut gate = granted_gate(&plugin);
        gate.revoke(&plugin, true);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::RevokedOrExpired)
        );
        let mut gate = FsGate::new(caps());
        let mut grant = read_grant(&plugin, &["~/docs/**"]);
        grant.expires_at = Some(10);
        gate.issue_grant(grant);
        gate.advance_time(10);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::RevokedOrExpired)
        );

        // 3. ScopeMismatch (cross-root outside grant scope).
        let mut gate = granted_gate(&plugin);
        let mut view = seeded_view();
        view.insert("~/other/x.txt", "x").unwrap();
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/other/x.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::ScopeMismatch)
        );

        // 4. HostilePattern (upward traversal).
        let mut gate = granted_gate(&plugin);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/../other/x.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::HostilePattern)
        );

        // 5. SensitivePath (credential location without consent).
        let mut gate = FsGate::new(caps());
        gate.issue_grant(read_grant(&plugin, &["~/projects/**"]));
        let mut view = FsView::new();
        // `~/.ssh` patterns are hostile at grant construction, so exercise
        // the sensitive gate through the default-deny `.env` file name in an
        // otherwise granted scope.
        gate.issue_grant(read_grant(&plugin, &["~/projects/app/**"]));
        view.insert("~/projects/app/.env", "K=v").unwrap();
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SensitivePath)
        );

        // 6. SecretContent (secret-shaped write refused).
        let mut gate = granted_gate(&plugin);
        let mut view = FsView::new();
        assert_eq!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/out/s.txt", "AKIAIOSFODNN7EXAMPLE", false),
                &mut view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SecretContent)
        );

        // 7. OverBoundOrRate (payload exceeds per-write cap).
        let mut gate = granted_gate(&plugin);
        let mut view = FsView::new();
        let big = "x".repeat(caps().max_bytes_per_write as usize + 1);
        assert_eq!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/out/big.txt", &big, false),
                &mut view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::OverBoundOrRate)
        );

        // 8. SafeMode.
        let mut gate = granted_gate(&plugin);
        gate.set_safe_mode(true);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SafeMode)
        );

        // 9. UnknownTrustOrDomain (L4 standing admission).
        let mut gate = granted_gate(&plugin);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ExternalTool,
                &read_req("~/docs/notes.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::UnknownTrustOrDomain)
        );
    }

    #[test]
    fn denials_are_oracle_tight() {
        let stranger = pid("mallory.copy");
        let mut present = FsView::new();
        present
            .insert("~/docs/secret.txt", "present bytes")
            .unwrap();
        let absent = FsView::new();
        // Grant the stranger nothing: absent and present deny identically.
        let mut gate = FsGate::new(caps());
        let with_content = gate
            .read(
                &stranger,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/secret.txt"),
                &present,
            )
            .unwrap_err()
            .message();
        let without_content = gate
            .read(
                &stranger,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/secret.txt"),
                &absent,
            )
            .unwrap_err()
            .message();
        assert_eq!(with_content, without_content);
        assert!(!with_content.contains("present"));
        assert!(!with_content.contains("mallory"));
        assert!(!with_content.contains("secret.txt"));
        assert!(with_content.contains("third-party-lua"));
        assert!(with_content.contains("'fs'"));
    }

    // ── silent-skip listing ──────────────────────────────────────────────

    #[test]
    fn listings_suppress_denied_entries_silently() {
        let plugin = pid("xuepoo.files");
        let mut gate = FsGate::new(caps());
        gate.issue_grant(read_grant(&plugin, &["~/docs/**"]));
        let mut view = FsView::new();
        view.insert("~/docs/notes.txt", "ok").unwrap();
        view.insert("~/docs/.env", "K=v").unwrap();
        view.insert("~/other/x.txt", "outside").unwrap();
        // The sensitive `.env` and the out-of-scope `~/other` entry are
        // suppressed silently: the page succeeds with only the visible file.
        let page = gate
            .list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/docs"),
                &view,
            )
            .unwrap();
        let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"~/docs/notes.txt"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".env")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("other")), "{names:?}");
        // Entries never carry file bytes.
        for entry in &page.entries {
            assert!(entry.kind == "file" || entry.kind == "dir");
        }
        // A prefix with only denied entries returns an empty page, not an
        // error (no absent-vs-denied signal).
        let mut view = FsView::new();
        view.insert("~/docs/.env", "K=v").unwrap();
        let page = gate
            .list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/docs"),
                &view,
            )
            .unwrap();
        assert!(page.entries.is_empty());
        assert_eq!(page.total_in_scope, 0);
    }

    // ── budgets ──────────────────────────────────────────────────────────

    #[test]
    fn window_budgets_deny_with_attribution_and_polling_denies_as_watch() {
        let plugin = pid("xuepoo.files");
        let mut gate = granted_gate(&plugin);
        let view = seeded_view();
        for _ in 0..caps().max_ops_per_window {
            assert!(
                gate.read(
                    &plugin,
                    TrustLevel::ThirdPartyLua,
                    &read_req("~/docs/notes.txt"),
                    &view
                )
                .is_ok()
            );
        }
        // Window exhausted: the next poll denies (polling cannot
        // reconstitute a watch or tail-follow).
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::OverBoundOrRate)
        );
        let (ops, bytes) = gate.window_usage(&plugin);
        assert_eq!(ops, caps().max_ops_per_window);
        assert!(bytes > 0);
        gate.advance_window();
        assert_eq!(gate.window_usage(&plugin), (0, 0));
        assert!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &view
            )
            .is_ok()
        );
    }

    #[test]
    fn zero_caps_are_misconfiguration_not_lockdown() {
        assert!(FsCaps::new(0, 4096, 16, 8192, 4, 16384).is_err());
        assert!(FsCaps::new(4096, 0, 16, 8192, 4, 16384).is_err());
        assert!(FsCaps::new(4096, 4096, 0, 8192, 4, 16384).is_err());
        assert!(FsCaps::new(4096, 4096, 16, 0, 4, 16384).is_err());
        assert!(FsCaps::new(4096, 4096, 16, 8192, 0, 16384).is_err());
        assert!(FsCaps::new(4096, 4096, 16, 8192, 4, 0).is_err());
    }

    // ── labels ─────────────────────────────────────────────────────────

    #[test]
    fn labels_survive_redaction_truncation_and_attribution() {
        let plugin = pid("xuepoo.files");
        let mut gate = FsGate::new(FsCaps::new(8, 4096, 16, 8192, 64, 65536).unwrap());
        gate.issue_grant(read_grant(&plugin, &["~/docs/**"]));
        let mut view = FsView::new();
        // Secret-shaped content is served redacted with the label.
        view.insert("~/docs/s.txt", "AKIAIOSFODNN7EXAMPLE").unwrap();
        let result = gate
            .read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &FsReadRequest {
                    client_version: FS_BRIDGE_VERSION,
                    path: "~/docs/s.txt".to_string(),
                    max_bytes: 8,
                },
                &view,
            )
            .unwrap();
        assert!(result.redacted);
        assert_eq!(result.label, FsLabel);
        assert_eq!(result.label.as_str(), "untrusted-observation");
        assert_eq!(result.export_bytes(), result.preview_bytes());
        assert!(!result.attribution.plugin.is_empty());
        assert!(!result.attribution.path.is_empty());

        // Long content truncates with the label preserved and marked.
        let mut view = FsView::new();
        view.insert("~/docs/long.txt", "0123456789abcdef").unwrap();
        let result = gate
            .read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &FsReadRequest {
                    client_version: FS_BRIDGE_VERSION,
                    path: "~/docs/long.txt".to_string(),
                    max_bytes: 8,
                },
                &view,
            )
            .unwrap();
        assert!(result.truncated);
        assert_eq!(result.label, FsLabel);
        assert_eq!(result.export_bytes(), result.preview_bytes());
    }

    // ── no migration, no watch, argv-first ─────────────────────────────

    #[test]
    fn no_grant_migrates_from_terminal_in_either_direction() {
        // The families are distinct closed symbols; a terminal grant is never
        // consulted by this gate and an fs grant never implies terminal.
        assert_ne!(CapabilityFamily::Fs, CapabilityFamily::Terminal);
        assert!(CapabilityFamily::Fs.denied_without_grant());
        assert!(CapabilityFamily::Terminal.denied_without_grant());
        for head in CapabilityFamily::Fs.closed_identifiers() {
            assert!(head.starts_with("fs."), "{head}");
            assert!(!head.starts_with("terminal."), "{head}");
            assert_eq!(
                CapabilityId::parse(&format!("{head}:~/docs/**"))
                    .unwrap()
                    .family(),
                CapabilityFamily::Fs
            );
        }
        for head in CapabilityFamily::Terminal.closed_identifiers() {
            assert!(!head.starts_with("fs."), "{head}");
        }
        // A terminal grant recorded elsewhere never authorizes an fs read:
        // the gate holds only fs grants, so a terminal-only world denies.
        let plugin = pid("xuepoo.files");
        let mut gate = FsGate::new(caps());
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/docs/notes.txt"),
                &seeded_view()
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::MissingGrant)
        );
        // And an fs grant never authorizes terminal state: the domains are
        // disjoint.
        assert_ne!(
            crate::trust_levels::CapabilityDomain::for_family(CapabilityFamily::Fs),
            crate::trust_levels::CapabilityDomain::for_family(CapabilityFamily::Terminal)
        );
    }

    #[test]
    fn no_watch_handles_or_streaming_exist() {
        // The verb parser rejects every handle/streaming spelling; the gate
        // exposes only read/write/list with no handle, cursor, subscription,
        // or freshness promise.
        for spelling in [
            "open",
            "append",
            "watch",
            "subscribe",
            "tail",
            "stream",
            "handle",
            "cursor",
            "follow",
        ] {
            assert!(FsVerb::parse(spelling).is_err(), "{spelling}");
        }
        let plugin = pid("xuepoo.files");
        let mut gate = granted_gate(&plugin);
        let page = gate
            .list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/docs"),
                &seeded_view(),
            )
            .unwrap();
        assert!(page.point_in_time);
    }

    #[test]
    fn read_into_vm_only_and_argv_first() {
        // The fs family grants no process, network, clipboard, IPC, or
        // terminal authority: combination needs a separately granted
        // authority. Argv-first keeps file bytes as data, never shell syntax.
        assert_ne!(CapabilityFamily::Fs, CapabilityFamily::Process);
        assert_ne!(CapabilityFamily::Fs, CapabilityFamily::Network);
        assert_ne!(CapabilityFamily::Fs, CapabilityFamily::Clipboard);
        assert!(argv_first_ok(&["git".to_string(), "status".to_string()]));
        assert!(!argv_first_ok(&[]));
        assert!(!argv_first_ok(&["".to_string()]));
        // File content with shell metacharacters stays data: it is only safe
        // combined as opaque argv entries, never interpolated.
        let hostile = "x; rm -rf /".to_string();
        assert!(argv_first_ok(&["tool".to_string(), hostile]));
        assert!(!argv_first_ok(&["tool".to_string(), "\0".to_string()]));
    }

    #[test]
    fn safe_mode_performs_nothing_identically() {
        let plugin = pid("xuepoo.files");
        let mut gate = granted_gate(&plugin);
        gate.set_safe_mode(true);
        for level in [
            TrustLevel::Core,
            TrustLevel::BundledLua,
            TrustLevel::ThirdPartyLua,
            TrustLevel::NativeSidecar,
            TrustLevel::ExternalTool,
        ] {
            assert_eq!(
                gate.read(
                    &plugin,
                    level,
                    &read_req("~/docs/notes.txt"),
                    &seeded_view()
                )
                .unwrap_err()
                .denial_kind(),
                Some(FsBridgeDenialKind::SafeMode),
                "level {level}"
            );
        }
    }

    // ── sensitive-path consent ─────────────────────────────────────────

    #[test]
    fn sensitive_consent_grants_access_until_it_lapses() {
        let plugin = pid("xuepoo.files");
        let mut gate = FsGate::new(caps());
        gate.issue_grant(read_grant(&plugin, &["~/projects/app/**"]));
        gate.issue_grant(write_grant(&plugin, &["~/projects/app/**"]));
        let mut view = FsView::new();
        view.insert("~/projects/app/.env", "K=v").unwrap();
        view.insert("~/projects/app/.env.local", "K=v").unwrap();
        view.insert("~/projects/app/notes.txt", "notes").unwrap();

        // Without consent every verb denies with the sensitive-path
        // category, and listings silently suppress the sensitive children.
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SensitivePath)
        );
        assert_eq!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/projects/app/.env", "hello", false),
                &mut view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SensitivePath)
        );
        let page = gate
            .list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/projects/app"),
                &view,
            )
            .unwrap();
        let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["~/projects/app/notes.txt"]);

        // Active consent opens exactly the consented path: read, overwrite,
        // and listing all see it, while the sibling sensitive file stays
        // denied/suppressed (consent names one path, never a subtree).
        gate.grant_consent("~/projects/app/.env").unwrap();
        assert!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env"),
                &view
            )
            .is_ok()
        );
        assert!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/projects/app/.env", "hello", false),
                &mut view
            )
            .is_ok()
        );
        let page = gate
            .list(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &list_req("~/projects/app"),
                &view,
            )
            .unwrap();
        let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"~/projects/app/.env"), "{names:?}");
        assert!(names.contains(&"~/projects/app/notes.txt"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".env.local")), "{names:?}");
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env.local"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SensitivePath)
        );

        // Revocation closes the path again with the same category.
        gate.revoke_consent("~/projects/app/.env");
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SensitivePath)
        );

        // A bounded grant lapses back to denial once past expiry (no
        // absent-vs-expired signal: the category never names the cause).
        let mut gate = FsGate::new(caps());
        gate.issue_grant(read_grant(&plugin, &["~/projects/app/**"]));
        gate.policy
            .grant_consent("~/projects/app/.env", 0, Some(10))
            .unwrap();
        gate.advance_time(9);
        assert!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env"),
                &view
            )
            .is_ok()
        );
        gate.advance_time(1);
        assert_eq!(
            gate.read(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &read_req("~/projects/app/.env"),
                &view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SensitivePath)
        );
    }

    // ── append secret assembly ─────────────────────────────────────────

    #[test]
    fn split_append_cannot_assemble_secret_shaped_file() {
        let plugin = pid("xuepoo.files");
        let mut gate = granted_gate(&plugin);
        let mut view = FsView::new();
        // Neither half is secret-shaped alone, but the assembled file is.
        assert!(!content_looks_secret("AKIA"));
        assert!(!content_looks_secret("IOSFODNN7EXAMPLE"));
        assert!(content_looks_secret("AKIAIOSFODNN7EXAMPLE"));
        gate.write(
            &plugin,
            TrustLevel::ThirdPartyLua,
            &write_req("~/docs/out/k.txt", "AKIA", false),
            &mut view,
        )
        .unwrap();
        // The refused append stores nothing: the first half survives intact.
        assert_eq!(
            gate.write(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &write_req("~/docs/out/k.txt", "IOSFODNN7EXAMPLE", true),
                &mut view
            )
            .unwrap_err()
            .denial_kind(),
            Some(FsBridgeDenialKind::SecretContent)
        );
        assert_eq!(view.get("~/docs/out/k.txt").unwrap(), "AKIA");
    }
}
