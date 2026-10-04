//! Read-only history/search/selection host surface (RFC-0004, CTX-0955, W-139).
//!
//! Core-side enforcement for the NEW `history` capability family beside
//! `terminal.*` (never an extension of it). One invariant: **a granted plugin
//! may ask bounded snapshot questions of the three queryable sources and
//! receive redacted, truncated, attributed answers, or a typed denial; it
//! may never open a stream, poll its way into one, widen a scope, touch
//! Terminal Truth, or read what was never persisted.**
//!
//! # What this module owns
//!
//! - [`HistorySource`]: the three queryable sources (segmented transcript,
//!   command history, own per-plugin KV). Session snapshots are NOT a source:
//!   save/restore stays Core-only per W-137 ([`HistorySource::parse`]
//!   rejects `session`/`snapshot` spellings fail-closed).
//! - [`HistoryGrant`]: per-plugin, per-source scoped grants. No wildcards, no
//!   bundled sources, no migration to or from `terminal.*`.
//! - [`SnapshotQuery`]: explicit row ranges with explicit count and size
//!   caps. Over-bound requests deny; Core never clamps silently.
//! - [`HistoryDenialKind`]: the complete 8-category typed-denial taxonomy with an
//!   oracle-tight no-leak rule (denials name the level and the family only;
//!   never content bytes, foreign identifiers, or absent-versus-denied
//!   signals).
//! - [`UntrustedLabel`]: the Core-attached untrusted-observation label on
//!   every record, surviving redaction and truncation.
//! - [`HistoryGate`]: the enforcement point — trust admission (L0–L4 per the
//!   threat-model matrix), safe-mode, version compat, grant intersection,
//!   capture opt-in, per-plugin rate plus aggregate budgets with attribution
//!   (`P0-AC-014`), and snapshot-only reads (no streaming, subscription,
//!   watch, or tail-follow; no freshness guarantee).
//!
//! # What this module does NOT own (parked owners)
//!
//! - Exact Lua spellings and the manifest grammar that declares them (W-139,
//!   SDK); the host capability heads (`history.transcript.read`,
//!   `history.commands.read`, `history.kv.read`) are the closed grammar in
//!   [`crate::capability`], enforced there.
//! - Numeric ceilings: [`HistoryCaps`] carries caller-provided bounds. No new
//!   numeric ceiling is invented here; exact defaults stay parked to W-137,
//!   W-139, and W-146, which reuse the accepted W-131/W-137 bounds.
//! - The redaction format (parked to W-137): rows enter through
//!   [`HistorySnapshot`] already redacted; the gate enforces truncation,
//!   label attachment, and export-preview equality.
//! - The backing stores and the Core integration wiring (W-146): this module
//!   reads through the caller-provided [`HistorySnapshot`] view, never a
//!   database, segment file, PTY hook, or native library.
//! - Wall-clock time: `now` is a monotonic host tick advanced explicitly by
//!   the caller ([`HistoryGate::advance_time`]); rate windows reset via
//!   [`HistoryGate::advance_window`]. No wall-clock, no randomness.
//!
//! # Non-goals
//!
//! No `unsafe`, no I/O, no new dependency (`std` plus sibling host types
//! only). Pure data plus validation, headlessly testable.

use std::collections::BTreeMap;

use crate::capability::CapabilityFamily;
use crate::manifest::PluginId;
use crate::trust_levels::TrustLevel;

// ── version / compat ────────────────────────────────────────────────────────

/// Host surface version of the history-read gate.
///
/// Capability-registry stable from acceptance: identifier choice is a
/// compatibility decision, so a version mismatch disables the surface with a
/// diagnostic instead of serving reads under unknown semantics.
pub const HISTORY_READ_VERSION: u32 = 1;

/// Capability family label for this surface (never under `terminal.*`).
pub const HISTORY_FAMILY: &str = "history";

/// Maximum bytes of one scope identifier (panel or workspace id).
///
/// A validation bound mirroring the capability-grammar segment bounds, not a
/// query ceiling: it keeps scope parsing bounded and allocation-safe.
pub const MAX_SCOPE_ID_BYTES: usize = 128;

/// Maximum bytes of one search needle.
pub const MAX_NEEDLE_BYTES: usize = 256;

// ── sources ─────────────────────────────────────────────────────────────────

/// Queryable history source (RFC-0004 source table).
///
/// Exactly the three W-131 objects this family may read. Session snapshots
/// are absent deliberately: save/restore is a Core-only mechanism per W-137.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HistorySource {
    /// Segmented transcript: scoped, bounded snapshot reads over sealed
    /// segments while opt-in capture holds.
    Transcript,
    /// Command history: scoped list, search, get, and tail reads.
    CommandHistory,
    /// Per-plugin KV: reads of the plugin's own namespace only.
    PluginKv,
}

impl HistorySource {
    /// Parse a source label; unknown labels (including `session` and
    /// `snapshot`) fail closed.
    pub fn parse(s: &str) -> Result<Self, HistoryError> {
        match s {
            "transcript" => Ok(Self::Transcript),
            "commands" | "command-history" => Ok(Self::CommandHistory),
            "kv" | "plugin-kv" => Ok(Self::PluginKv),
            "session" | "snapshot" | "session-snapshot" => Err(HistoryError::forbidden_source(s)),
            _ => Err(HistoryError::forbidden_source(s)),
        }
    }

    /// Stable source label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::CommandHistory => "commands",
            Self::PluginKv => "kv",
        }
    }

    /// Closed capability head granting reads of this source.
    #[must_use]
    pub const fn capability_head(self) -> &'static str {
        match self {
            Self::Transcript => "history.transcript.read",
            Self::CommandHistory => "history.commands.read",
            Self::PluginKv => "history.kv.read",
        }
    }

    /// Whether reads of this source require opt-in capture to hold.
    ///
    /// Transcript and command history are terminal-derived and persist only
    /// on the opt-in history path (W-131). Per-plugin KV is plugin-authored
    /// state, not terminal capture, so its reads need a grant but no
    /// capture opt-in.
    #[must_use]
    pub const fn requires_capture(self) -> bool {
        match self {
            Self::Transcript | Self::CommandHistory => true,
            Self::PluginKv => false,
        }
    }
}

// ── scopes ──────────────────────────────────────────────────────────────────

/// Explicit grant/query scope: a source plus a panel/workspace extent.
///
/// There is no wildcard and no `all` default. For transcript and command
/// history, at least one of `panel`/`workspace` must be set (an unscoped
/// grant request is rejected; an unscoped query denies). For own-namespace
/// KV the extent is the caller identity itself, so `panel` and `workspace`
/// must both be unset — KV has no panel extent and cross-plugin reads are
/// unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryScope {
    source: HistorySource,
    panel: Option<String>,
    workspace: Option<String>,
}

impl HistoryScope {
    /// Build a scope, fail-closed on unscoped or wildcard extents.
    pub fn new(
        source: HistorySource,
        panel: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Self, HistoryError> {
        let panel = validate_scope_id("panel", panel)?;
        let workspace = validate_scope_id("workspace", workspace)?;
        match source {
            HistorySource::Transcript | HistorySource::CommandHistory => {
                if panel.is_none() && workspace.is_none() {
                    return Err(HistoryError::unscoped(source));
                }
            }
            HistorySource::PluginKv => {
                if panel.is_some() || workspace.is_some() {
                    return Err(HistoryError::Disabled {
                        diagnostic:
                            "kv scope is the caller namespace; panel/workspace extents are rejected"
                                .to_string(),
                    });
                }
            }
        }
        Ok(Self {
            source,
            panel,
            workspace,
        })
    }

    /// Source this scope covers.
    #[must_use]
    pub const fn source(&self) -> HistorySource {
        self.source
    }

    /// Intersect-or-deny: a query scope is authorized only when it
    /// intersects the grant scope on the same source with no wildcard
    /// widening. `None` intersects only `None`; a set extent intersects
    /// only the equal extent.
    #[must_use]
    pub fn intersects(&self, grant: &HistoryScope) -> bool {
        if self.source != grant.source {
            return false;
        }
        axis_intersects(&self.panel, &grant.panel)
            && axis_intersects(&self.workspace, &grant.workspace)
    }
}

fn axis_intersects(query: &Option<String>, grant: &Option<String>) -> bool {
    match (query, grant) {
        (Some(q), Some(g)) => q == g,
        (None, None) => true,
        _ => false,
    }
}

fn validate_scope_id(axis: &str, value: Option<&str>) -> Result<Option<String>, HistoryError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > MAX_SCOPE_ID_BYTES {
        return Err(HistoryError::bad_scope(axis));
    }
    if value == "*" || value.eq_ignore_ascii_case("all") {
        return Err(HistoryError::bad_scope(axis));
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(HistoryError::bad_scope(axis));
    }
    Ok(Some(value.to_string()))
}

// ── grants ──────────────────────────────────────────────────────────────────

/// Grant shape: standing per-plugin grants versus single-use per-request
/// (L3) / per-invocation (L4) grants issued by Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// Explicit per-plugin, per-source grant recorded by Core (L1/L2 path).
    Standing,
    /// Single-use Core-issued grant (L3 native sidecar per-request, L4
    /// external-tool per-invocation). Consumed by one successful read.
    PerRequest {
        /// Remaining uses; decremented on success, never replenished here.
        uses_left: u32,
    },
}

/// Per-plugin, per-source scoped grant recorded by the Core gate.
///
/// Grants never bundle sources (one grant names exactly one source), never
/// migrate to or from `terminal.*`, and never widen at runtime. Absence of
/// a grant, or a query scope outside the grant scope, denies fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryGrant {
    plugin: PluginId,
    scope: HistoryScope,
    kind: GrantKind,
    revoked: bool,
    /// Monotonic host tick at which the grant expires (`None` = no expiry).
    expires_at: Option<u64>,
}

impl HistoryGrant {
    /// Record a standing grant (L1/L2 path).
    pub fn standing(plugin: PluginId, scope: HistoryScope) -> Self {
        Self {
            plugin,
            scope,
            kind: GrantKind::Standing,
            revoked: false,
            expires_at: None,
        }
    }

    /// Record a single-use per-request/per-invocation grant (L3/L4 path).
    pub fn per_request(plugin: PluginId, scope: HistoryScope) -> Self {
        Self {
            plugin,
            scope,
            kind: GrantKind::PerRequest { uses_left: 1 },
            revoked: false,
            expires_at: None,
        }
    }

    /// Owning plugin.
    #[must_use]
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// Grant scope.
    #[must_use]
    pub const fn scope(&self) -> &HistoryScope {
        &self.scope
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
}

// ── typed denials ───────────────────────────────────────────────────────────

/// Complete typed-denial taxonomy (RFC-0004, normative).
///
/// Denials are catchable and fail closed. They are oracle-tight: a denial
/// carries the category code plus the trust level and the family only —
/// never content bytes, foreign identifiers, or any signal distinguishing
/// absent content from denied content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HistoryDenialKind {
    /// No per-plugin, per-source grant is recorded (deny-by-default).
    MissingGrant,
    /// A grant was recorded but is revoked or expired.
    RevokedOrExpired,
    /// The query scope does not intersect the grant scope (including
    /// cross-panel, cross-workspace, and cross-plugin reads).
    ScopeMismatch,
    /// The request exceeds its row/byte caps or the per-plugin rate or
    /// aggregate budget.
    OverBoundOrRate,
    /// The source's opt-in capture is off; nothing was persisted to read.
    CaptureDisabled,
    /// `bitty --safe` reads nothing: no transcript, history, snapshot, or KV.
    SafeMode,
    /// The trust level or domain admits no standing access here (L4 and
    /// unknown levels/domains deny rather than default).
    UnknownTrustOrDomain,
    /// The content was purged or expired: typed unavailability, never a
    /// silent gap and never resurrected from a derived index.
    PurgedOrExpired,
}

impl HistoryDenialKind {
    /// Stable machine-readable code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingGrant => "E_HISTORY_MISSING_GRANT",
            Self::RevokedOrExpired => "E_HISTORY_REVOKED_GRANT",
            Self::ScopeMismatch => "E_HISTORY_SCOPE_MISMATCH",
            Self::OverBoundOrRate => "E_HISTORY_OVER_BOUND",
            Self::CaptureDisabled => "E_HISTORY_CAPTURE_DISABLED",
            Self::SafeMode => "E_HISTORY_SAFE_MODE",
            Self::UnknownTrustOrDomain => "E_HISTORY_TRUST_DENIED",
            Self::PurgedOrExpired => "E_HISTORY_UNAVAILABLE",
        }
    }

    /// All eight categories, for taxonomy-completeness tests.
    #[must_use]
    pub const fn all() -> &'static [HistoryDenialKind] {
        &[
            Self::MissingGrant,
            Self::RevokedOrExpired,
            Self::ScopeMismatch,
            Self::OverBoundOrRate,
            Self::CaptureDisabled,
            Self::SafeMode,
            Self::UnknownTrustOrDomain,
            Self::PurgedOrExpired,
        ]
    }
}

/// A typed, catchable denial: category plus level and family only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryDenial {
    kind: HistoryDenialKind,
    level: TrustLevel,
}

impl HistoryDenial {
    /// Denial category.
    #[must_use]
    pub const fn kind(self) -> HistoryDenialKind {
        self.kind
    }

    /// Oracle-tight rendering: category code plus level and family only.
    #[must_use]
    pub fn message(self) -> String {
        format!(
            "{} denied for level '{}' (family '{}')",
            self.kind.code(),
            self.level.as_str(),
            HISTORY_FAMILY
        )
    }
}

// ── errors ──────────────────────────────────────────────────────────────────

/// Gate outcome error: either the surface is disabled (version compat) or
/// the query is denied with a typed denial.
///
/// Version mismatch disables with a diagnostic and is NOT a denial: it
/// carries no denial category and must not be mistaken for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    /// The surface is disabled: client/host version mismatch or a malformed
    /// scope or source spelling. Carries a diagnostic, never a denial.
    Disabled {
        /// Human-readable diagnostic (versions, offending axis).
        diagnostic: String,
    },
    /// The query is denied with a typed, catchable denial.
    Denied(HistoryDenial),
}

impl HistoryError {
    fn denied(kind: HistoryDenialKind, level: TrustLevel, _why: &str) -> Self {
        // `_why` is intentionally dropped: denial text must stay oracle-tight
        // (level plus family only). It exists so call sites document the
        // failing gate inline for reviewers.
        Self::Denied(HistoryDenial { kind, level })
    }

    fn forbidden_source(spelling: &str) -> Self {
        Self::Disabled {
            diagnostic: format!(
                "history source '{spelling}' is not queryable (queryable: transcript, commands, kv; session snapshots are Core-only)"
            ),
        }
    }

    fn unscoped(source: HistorySource) -> Self {
        Self::Disabled {
            diagnostic: format!(
                "history scope for '{}' names no panel or workspace extent (no wildcard default)",
                source.as_str()
            ),
        }
    }

    fn bad_scope(axis: &str) -> Self {
        Self::Disabled {
            diagnostic: format!(
                "history scope {axis} must be 1..={MAX_SCOPE_ID_BYTES} non-blank bytes, never '*' or 'all'"
            ),
        }
    }

    /// Stable rendering: diagnostics for disabled, oracle-tight denial text
    /// for denied.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Disabled { diagnostic } => format!("history surface disabled: {diagnostic}"),
            Self::Denied(denial) => denial.message(),
        }
    }

    /// Denial category when denied; `None` when disabled.
    #[must_use]
    pub const fn denial_kind(&self) -> Option<HistoryDenialKind> {
        match self {
            Self::Disabled { .. } => None,
            Self::Denied(denial) => Some(denial.kind),
        }
    }
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for HistoryError {}

// ── records, labels, pages ──────────────────────────────────────────────────

/// Core-attached untrusted-observation label.
///
/// Every record carries this label as a separate typed field applied AFTER
/// redaction and truncation, so it survives both. Consumers (plugins,
/// agents, tools) must treat labeled content as observation data under the
/// prompt-injection rule, never as instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UntrustedLabel;

impl UntrustedLabel {
    /// Stable label value.
    pub const VALUE: &'static str = "untrusted-observation";

    /// Stable label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        Self::VALUE
    }
}

/// Row-level attribution carried by every record (panel, workspace,
/// command, timing, actor where the source records it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// Panel the row was recorded in.
    pub panel: String,
    /// Workspace the panel belongs to.
    pub workspace: String,
    /// Command text for command-history rows (already redacted upstream).
    pub command: Option<String>,
    /// Monotonic host tick of recording.
    pub recorded_at: u64,
    /// Actor that produced the row, where the source records one.
    pub actor: Option<String>,
}

/// One persisted row in the caller-provided snapshot view.
///
/// Bodies enter already redacted (the redaction format stays parked to
/// W-137); the gate enforces truncation, label attachment, and
/// export-preview equality over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRow {
    /// Owning plugin namespace (KV rows only; `None` otherwise).
    pub owner: Option<PluginId>,
    /// Panel the row was recorded in.
    pub panel: String,
    /// Workspace the panel belongs to.
    pub workspace: String,
    /// Monotonic sequence within the source (snapshot ordering).
    pub seq: u64,
    /// Already-redacted body bytes.
    pub redacted_body: String,
    /// Attribution recorded with the row.
    pub attribution: Attribution,
    /// Purged rows are never resurrected: they are skipped, and a range
    /// covering only purged rows denies as typed unavailability.
    pub purged: bool,
}

/// Caller-provided view over already-persisted state.
///
/// The gate never opens a store file, database, PTY hook, or native
/// library; Core (W-146) builds this view from the mediated event and
/// storage surfaces. Tests use small fixtures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorySnapshot {
    transcript: Vec<StoredRow>,
    commands: Vec<StoredRow>,
    kv: Vec<StoredRow>,
}

impl HistorySnapshot {
    /// Empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a persisted row to a source view.
    pub fn push(&mut self, source: HistorySource, row: StoredRow) {
        match source {
            HistorySource::Transcript => self.transcript.push(row),
            HistorySource::CommandHistory => self.commands.push(row),
            HistorySource::PluginKv => self.kv.push(row),
        }
    }

    fn rows(&self, source: HistorySource) -> &[StoredRow] {
        match source {
            HistorySource::Transcript => &self.transcript,
            HistorySource::CommandHistory => &self.commands,
            HistorySource::PluginKv => &self.kv,
        }
    }
}

/// Snapshot freshness: queries carry no freshness guarantee.
///
/// Every result is a point-in-time snapshot and may be stale. No
/// live-ness, recency, or change-notification promise exists in this family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Freshness {
    /// Point-in-time snapshot; may be stale; never a live promise.
    PointInTimeNoGuarantee,
}

/// One redacted, truncated, attributed, labeled record in a result page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRecord {
    /// Source sequence number (snapshot ordering, not a cursor).
    pub seq: u64,
    /// Redacted and truncated body (truncation marked, never silent).
    pub body: String,
    /// True when the body was truncated to the row cap.
    pub truncated: bool,
    /// True: the body passed upstream redaction before reaching the gate.
    pub redacted: bool,
    /// Row attribution.
    pub attribution: Attribution,
    /// Core-attached untrusted-observation label (survives truncation).
    pub label: UntrustedLabel,
}

/// Bounded snapshot result page.
///
/// Deliberately cursor-free: there is no subscription, no watch, no
/// tail-follow, and no cursor held across calls. A caller that wants newer
/// state issues a new bounded query under its grant and budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPage {
    /// Redacted, truncated, labeled records in snapshot order.
    pub records: Vec<HistoryRecord>,
    /// Rows in scope at snapshot time (may exceed the returned page).
    pub total_in_scope: u64,
    /// Always point-in-time with no freshness promise.
    pub freshness: Freshness,
}

impl SnapshotPage {
    /// Export bytes: identical truncation to the preview path, so an export
    /// preview equals the actual export byte-for-byte.
    #[must_use]
    pub fn export_bytes(&self) -> Vec<String> {
        self.records.iter().map(|row| row.body.clone()).collect()
    }

    /// Preview bytes: same pipeline as [`SnapshotPage::export_bytes`].
    #[must_use]
    pub fn preview_bytes(&self) -> Vec<String> {
        self.export_bytes()
    }
}

// ── queries ─────────────────────────────────────────────────────────────────

/// Snapshot operation over an explicit row range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOp {
    /// Bounded list over `[row_start, row_start + row_count)`.
    List,
    /// Tail: the last `row_count` rows in scope (bounded like list).
    Tail,
    /// Substring search over redacted bodies (bounded needle, bounded rows).
    Search {
        /// Bounded search needle (matched against redacted bodies only).
        needle: String,
    },
}

/// Bounded snapshot query over already-persisted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotQuery {
    /// Client surface version; must equal [`HISTORY_READ_VERSION`].
    pub client_version: u32,
    /// Query scope (source plus panel/workspace extent).
    pub scope: HistoryScope,
    /// First row (snapshot sequence offset within the in-scope rows).
    pub row_start: u64,
    /// Explicit row bound (`> 0`).
    pub row_count: u32,
    /// Explicit result byte bound (`> 0`).
    pub max_bytes: u32,
    /// List, tail, or bounded search.
    pub op: QueryOp,
}

impl SnapshotQuery {
    /// Validate the needle bound for search ops.
    fn validated_needle(&self, level: TrustLevel) -> Result<Option<&str>, HistoryError> {
        match &self.op {
            QueryOp::List | QueryOp::Tail => Ok(None),
            QueryOp::Search { needle } => {
                if needle.is_empty() || needle.len() > MAX_NEEDLE_BYTES {
                    return Err(HistoryError::denied(
                        HistoryDenialKind::OverBoundOrRate,
                        level,
                        "search needle must be 1..=MAX_NEEDLE_BYTES bytes",
                    ));
                }
                Ok(Some(needle.as_str()))
            }
        }
    }
}

// ── budgets ─────────────────────────────────────────────────────────────────

/// Caller-provided query and budget ceilings.
///
/// All bounds arrive from Core configuration (owned by W-137/W-139/W-146
/// from the accepted W-131/W-137 bounds); this module mints no numeric
/// ceiling. Zero bounds are rejected as misconfiguration: lockdown is
/// expressed through safe mode and capture opt-in, never through a silent
/// zero that denies every read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCaps {
    /// Maximum rows per query.
    pub max_rows_per_query: u32,
    /// Maximum result bytes per query.
    pub max_bytes_per_query: u32,
    /// Maximum bytes per record (truncation cap; truncation is marked).
    pub max_bytes_per_row: u32,
    /// Maximum successful queries per plugin per window.
    pub max_queries_per_window: u64,
    /// Maximum successful result bytes per plugin per window.
    pub max_bytes_per_window: u64,
}

impl HistoryCaps {
    /// Build ceilings, rejecting zero bounds fail-closed as misconfiguration.
    pub fn new(
        max_rows_per_query: u32,
        max_bytes_per_query: u32,
        max_bytes_per_row: u32,
        max_queries_per_window: u64,
        max_bytes_per_window: u64,
    ) -> Result<Self, HistoryError> {
        if max_rows_per_query == 0
            || max_bytes_per_query == 0
            || max_bytes_per_row == 0
            || max_queries_per_window == 0
            || max_bytes_per_window == 0
        {
            return Err(HistoryError::Disabled {
                diagnostic:
                    "history caps must all be nonzero (lockdown uses safe mode, not zero caps)"
                        .to_string(),
            });
        }
        Ok(Self {
            max_rows_per_query,
            max_bytes_per_query,
            max_bytes_per_row,
            max_queries_per_window,
            max_bytes_per_window,
        })
    }
}

/// Per-plugin window usage with per-plugin attribution (`P0-AC-014`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WindowUsage {
    queries: u64,
    bytes: u64,
}

// ── gate ────────────────────────────────────────────────────────────────────

/// Enforcement point for the read-only history surface.
///
/// Holds grants, budgets, capture opt-in flags, safe mode, the compat
/// version, and per-plugin window usage. Check order is fail-closed and
/// oracle-aware: safe mode, trust admission, version compat, grant
/// presence, revocation/expiry, scope intersection, capture opt-in, static
/// bounds plus window budgets, purge unavailability — then rows. Safe mode
/// first so `bitty --safe` reads nothing identically for every level and
/// version. Scope is checked before capture so out-of-scope callers learn
/// nothing about capture state; static bounds and budgets are checked only
/// for authorized callers so denials never oracle bound or budget facts.
#[derive(Debug, Clone)]
pub struct HistoryGate {
    grants: Vec<HistoryGrant>,
    caps: HistoryCaps,
    capture: BTreeMap<HistorySource, bool>,
    safe_mode: bool,
    host_version: u32,
    now: u64,
    usage: BTreeMap<String, WindowUsage>,
}

impl HistoryGate {
    /// Create a gate with caller-provided ceilings.
    ///
    /// Capture defaults to off for terminal-derived sources (opt-in per
    /// W-131) and on for plugin-authored KV (no terminal capture involved).
    pub fn new(caps: HistoryCaps) -> Self {
        let mut capture = BTreeMap::new();
        capture.insert(HistorySource::Transcript, false);
        capture.insert(HistorySource::CommandHistory, false);
        capture.insert(HistorySource::PluginKv, true);
        Self {
            grants: Vec::new(),
            caps,
            capture,
            safe_mode: false,
            host_version: HISTORY_READ_VERSION,
            now: 0,
            usage: BTreeMap::new(),
        }
    }

    /// Record a grant (Core consent path).
    pub fn issue_grant(&mut self, grant: HistoryGrant) {
        self.grants.push(grant);
    }

    /// Revoke every grant for `plugin` on `source`.
    pub fn revoke(&mut self, plugin: &PluginId, source: HistorySource) {
        for grant in &mut self.grants {
            if grant.plugin == *plugin && grant.scope.source() == source {
                grant.revoked = true;
            }
        }
    }

    /// Set opt-in capture for a source (W-131: default off for
    /// terminal-derived content).
    pub fn set_capture(&mut self, source: HistorySource, enabled: bool) {
        self.capture.insert(source, enabled);
    }

    /// Enter or leave safe mode (`bitty --safe` reads nothing).
    pub fn set_safe_mode(&mut self, safe: bool) {
        self.safe_mode = safe;
    }

    /// Override the host surface version (compat tests only; production
    /// stays at [`HISTORY_READ_VERSION`]).
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

    /// Per-plugin window usage (attribution evidence for `P0-AC-014`).
    #[must_use]
    pub fn window_usage(&self, plugin: &PluginId) -> (u64, u64) {
        self.usage
            .get(plugin.as_str())
            .map(|usage| (usage.queries, usage.bytes))
            .unwrap_or((0, 0))
    }

    /// Whether `grant` is eligible for `level` right now: owned by
    /// `plugin`, covering `source`, live, and of the kind the level admits
    /// (standing for L1/L2, single-use per-request for L3/L4).
    fn grant_is_usable(
        &self,
        grant: &HistoryGrant,
        plugin: &PluginId,
        source: HistorySource,
        level: TrustLevel,
    ) -> bool {
        grant.plugin == *plugin
            && grant.scope.source() == source
            && grant.is_live(self.now)
            && match level {
                TrustLevel::BundledLua | TrustLevel::ThirdPartyLua => {
                    matches!(grant.kind, GrantKind::Standing)
                }
                TrustLevel::NativeSidecar | TrustLevel::ExternalTool => {
                    matches!(grant.kind, GrantKind::PerRequest { uses_left: 1.. })
                }
                TrustLevel::Core => false,
            }
    }

    /// Bounded snapshot read over already-persisted state.
    pub fn query(
        &mut self,
        plugin: &PluginId,
        level: TrustLevel,
        query: &SnapshotQuery,
        store: &HistorySnapshot,
    ) -> Result<SnapshotPage, HistoryError> {
        let scope = &query.scope;
        let source = scope.source();

        // 1. Safe mode reads nothing, identically for every level,
        //    version, grant, and store content.
        if self.safe_mode {
            return Err(HistoryError::denied(
                HistoryDenialKind::SafeMode,
                level,
                "safe mode reads no history",
            ));
        }

        // 2. Trust admission before grant intersection (P0-AC-035): the
        //    family maps to the `terminal output` domain. L4 admits nothing
        //    at the domain gate; a live per-request/per-invocation grant is
        //    the only exception (checked at step 4).
        let per_request_live = self.grants.iter().any(|grant| {
            grant.plugin == *plugin
                && grant.is_live(self.now)
                && matches!(grant.kind, GrantKind::PerRequest { uses_left: 1.. })
        });
        if level.check_family(CapabilityFamily::History).is_err() && !per_request_live {
            return Err(HistoryError::denied(
                HistoryDenialKind::UnknownTrustOrDomain,
                level,
                "level admits no standing history access",
            ));
        }

        // 3. Version compat: mismatch disables with a diagnostic (never a
        //    denial category).
        if query.client_version != self.host_version {
            return Err(HistoryError::Disabled {
                diagnostic: format!(
                    "history surface version mismatch (client {}, host {}); surface disabled",
                    query.client_version, self.host_version
                ),
            });
        }

        // 4. Grant presence (deny-by-default) with the L1/L2 vs L3/L4 kind
        //    split: standing grants authorize L1/L2 only; L3/L4 require a
        //    live per-request/per-invocation grant. L0 (Core enforcement
        //    itself) needs no plugin grant and skips grant/scope checks;
        //    KV namespace filtering below still applies structurally.
        //    A plugin may hold several grants on one source: the gate
        //    selects a live grant of the right kind whose scope INTERSECTS
        //    the query, and reports ScopeMismatch only when eligible live
        //    grants exist but none of them intersects.
        let mut grant_slot: Option<usize> = None;
        if level != TrustLevel::Core {
            let any_usable = self
                .grants
                .iter()
                .any(|grant| self.grant_is_usable(grant, plugin, source, level));
            let matching_idx = self.grants.iter().position(|grant| {
                self.grant_is_usable(grant, plugin, source, level) && scope.intersects(&grant.scope)
            });
            if any_usable && matching_idx.is_none() {
                // 5. Scope intersection (query ∩ grant, or deny). KV
                //    additionally restricts to the caller namespace
                //    structurally below.
                return Err(HistoryError::denied(
                    HistoryDenialKind::ScopeMismatch,
                    level,
                    "query scope outside grant scope",
                ));
            }
            // Any recorded-but-unusable grant (revoked/expired, or the wrong
            // kind for this level) denies as revoked/expired or missing
            // without distinguishing which — both are oracle-identical
            // denials.
            // L0 skips grant presence and scope intersection (Core owns every
            // scope); capture, bounds, budgets, and namespace filtering below
            // still apply.
            let recorded = self
                .grants
                .iter()
                .any(|grant| grant.plugin == *plugin && grant.scope.source() == source);
            let Some(idx) = matching_idx else {
                if recorded
                    && self.grants.iter().any(|grant| {
                        grant.plugin == *plugin
                            && grant.scope.source() == source
                            && (!grant.is_live(self.now) || grant.revoked)
                    })
                {
                    return Err(HistoryError::denied(
                        HistoryDenialKind::RevokedOrExpired,
                        level,
                        "grant revoked or expired",
                    ));
                }
                return Err(HistoryError::denied(
                    HistoryDenialKind::MissingGrant,
                    level,
                    "no usable grant",
                ));
            };
            grant_slot = Some(idx);
        }

        // 6. Capture opt-in: terminal-derived sources persist only while
        //    opt-in capture holds; opt-in off reads nothing.
        if source.requires_capture() && !self.capture.get(&source).copied().unwrap_or(false) {
            return Err(HistoryError::denied(
                HistoryDenialKind::CaptureDisabled,
                level,
                "capture opt-in off",
            ));
        }

        // 7. Static bounds (explicit row range plus caps; never clamp
        //    silently) and window budgets with per-plugin attribution.
        if query.row_count == 0 || query.max_bytes == 0 {
            return Err(HistoryError::denied(
                HistoryDenialKind::OverBoundOrRate,
                level,
                "empty range bound",
            ));
        }
        if query.row_count > self.caps.max_rows_per_query
            || u64::from(query.max_bytes) > u64::from(self.caps.max_bytes_per_query)
        {
            return Err(HistoryError::denied(
                HistoryDenialKind::OverBoundOrRate,
                level,
                "over per-query bound",
            ));
        }
        let needle = query.validated_needle(level)?;
        let key = plugin.as_str().to_string();
        let used = self.usage.get(&key).copied().unwrap_or_default();
        if used.queries >= self.caps.max_queries_per_window
            || used.bytes >= self.caps.max_bytes_per_window
        {
            return Err(HistoryError::denied(
                HistoryDenialKind::OverBoundOrRate,
                level,
                "over window budget",
            ));
        }

        // 8. Collect in-scope rows (snapshot order). KV is structurally
        //    restricted to the caller namespace: foreign rows are never
        //    collected, never counted, never named.
        let mut in_scope: Vec<&StoredRow> = store
            .rows(source)
            .iter()
            .filter(|row| {
                if source == HistorySource::PluginKv && row.owner.as_ref() != Some(plugin) {
                    return false;
                }
                scope_matches_row(scope, row)
            })
            .collect();
        in_scope.sort_by_key(|row| row.seq);
        let total_in_scope = in_scope.len() as u64;

        // Tail selects the last N rows; list/search slice from row_start.
        let selected: Vec<&StoredRow> = match query.op {
            QueryOp::Tail => {
                let count = usize::try_from(query.row_count).unwrap_or(usize::MAX);
                let skip = in_scope.len().saturating_sub(count);
                in_scope.into_iter().skip(skip).collect()
            }
            QueryOp::List => {
                let start = usize::try_from(query.row_start).unwrap_or(usize::MAX);
                let count = usize::try_from(query.row_count).unwrap_or(usize::MAX);
                in_scope.into_iter().skip(start).take(count).collect()
            }
            QueryOp::Search { .. } => {
                let needle = needle.unwrap_or_default();
                let start = usize::try_from(query.row_start).unwrap_or(usize::MAX);
                let count = usize::try_from(query.row_count).unwrap_or(usize::MAX);
                // Purged rows are invisible to content matching: letting the
                // needle match purged bodies would oracle purged-content
                // existence (unavailability versus empty page). Positional
                // list/tail selection keeps the typed-unavailability denial
                // below; search never does.
                in_scope
                    .into_iter()
                    .filter(|row| !row.purged)
                    .filter(|row| row.redacted_body.contains(needle))
                    .skip(start)
                    .take(count)
                    .collect()
            }
        };

        // 9. Purged or expired content: a range covering only purged rows
        //    denies as typed unavailability (never a silent gap, never
        //    resurrected). Live rows alongside purged ones return; purged
        //    bytes never cross.
        if !selected.is_empty() && selected.iter().all(|row| row.purged) {
            return Err(HistoryError::denied(
                HistoryDenialKind::PurgedOrExpired,
                level,
                "content purged or expired",
            ));
        }
        let live: Vec<&StoredRow> = selected.into_iter().filter(|row| !row.purged).collect();

        // 10. Truncate to the row cap (marked, never silent), attach the
        //     Core label, and charge the window budget on success only.
        let mut records = Vec::with_capacity(live.len());
        let mut page_bytes: u64 = 0;
        for row in live {
            let mut body = row.redacted_body.clone();
            let mut truncated = false;
            let cap = self.caps.max_bytes_per_row as usize;
            if body.len() > cap {
                // Walk back to a UTF-8 char boundary: `truncate` panics on
                // a mid-char split, and terminal text is routinely
                // multi-byte (CJK, emoji, box drawing).
                let mut end = cap;
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
                truncated = true;
            }
            page_bytes += body.len() as u64;
            records.push(HistoryRecord {
                seq: row.seq,
                body,
                truncated,
                redacted: true,
                attribution: row.attribution.clone(),
                label: UntrustedLabel,
            });
        }
        if page_bytes > u64::from(self.caps.max_bytes_per_query)
            || page_bytes > u64::from(query.max_bytes)
            || used.bytes.saturating_add(page_bytes) > self.caps.max_bytes_per_window
        {
            return Err(HistoryError::denied(
                HistoryDenialKind::OverBoundOrRate,
                level,
                "over result byte bound",
            ));
        }
        let entry = self.usage.entry(key).or_default();
        entry.queries += 1;
        entry.bytes += page_bytes;
        if level == TrustLevel::NativeSidecar || level == TrustLevel::ExternalTool {
            if let Some(idx) = grant_slot {
                if let GrantKind::PerRequest { uses_left } = &mut self.grants[idx].kind {
                    *uses_left = uses_left.saturating_sub(1);
                }
            }
        }

        Ok(SnapshotPage {
            records,
            total_in_scope,
            freshness: Freshness::PointInTimeNoGuarantee,
        })
    }
}

fn scope_matches_row(scope: &HistoryScope, row: &StoredRow) -> bool {
    match &scope.panel {
        Some(panel) if row.panel != *panel => return false,
        _ => {}
    }
    match &scope.workspace {
        Some(workspace) if row.workspace != *workspace => return false,
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityId;

    const SECRET: &str = "sk-live-SECRET-abc123";

    fn pid(s: &str) -> PluginId {
        PluginId::new(s).unwrap()
    }

    /// Test-only ceilings (fixtures, not normative; exact defaults stay
    /// parked to W-137/W-139/W-146).
    fn caps() -> HistoryCaps {
        HistoryCaps::new(16, 4096, 256, 4, 8192).unwrap()
    }

    fn ws_scope(source: HistorySource, workspace: &str) -> HistoryScope {
        HistoryScope::new(source, None, Some(workspace)).unwrap()
    }

    fn panel_scope(source: HistorySource, panel: &str, workspace: &str) -> HistoryScope {
        HistoryScope::new(source, Some(panel), Some(workspace)).unwrap()
    }

    fn row(panel: &str, workspace: &str, seq: u64, body: &str) -> StoredRow {
        StoredRow {
            owner: None,
            panel: panel.to_string(),
            workspace: workspace.to_string(),
            seq,
            redacted_body: body.to_string(),
            attribution: Attribution {
                panel: panel.to_string(),
                workspace: workspace.to_string(),
                command: None,
                recorded_at: seq,
                actor: None,
            },
            purged: false,
        }
    }

    fn kv_row(owner: &PluginId, seq: u64, body: &str) -> StoredRow {
        StoredRow {
            owner: Some(owner.clone()),
            panel: String::new(),
            workspace: String::new(),
            seq,
            redacted_body: body.to_string(),
            attribution: Attribution {
                panel: String::new(),
                workspace: String::new(),
                command: None,
                recorded_at: seq,
                actor: None,
            },
            purged: false,
        }
    }

    fn make_query(scope: HistoryScope, row_count: u32) -> SnapshotQuery {
        SnapshotQuery {
            client_version: HISTORY_READ_VERSION,
            scope,
            row_start: 0,
            row_count,
            max_bytes: 4096,
            op: QueryOp::List,
        }
    }

    fn transcript_store() -> HistorySnapshot {
        let mut store = HistorySnapshot::new();
        store.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 0, "redacted output line one"),
        );
        store.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 1, "redacted output line two"),
        );
        store
    }

    /// Gate with capture on plus a standing transcript grant over `ws-1`.
    fn granted_gate(plugin: &PluginId) -> HistoryGate {
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        gate
    }

    // ── grant matrix L0–L4 ───────────────────────────────────────────────

    #[test]
    fn grant_matrix_l0_through_l4() {
        let plugin = pid("xuepoo.search");
        let levels = [
            (
                TrustLevel::Core,
                true,
                "L0 enforces; no plugin grant needed",
            ),
            (TrustLevel::BundledLua, true, "L1 with standing grant"),
            (TrustLevel::ThirdPartyLua, true, "L2 with standing grant"),
        ];
        for (level, allowed, _why) in levels {
            let mut gate = granted_gate(&plugin);
            let scope = ws_scope(HistorySource::Transcript, "ws-1");
            let result = gate.query(&plugin, level, &make_query(scope, 4), &transcript_store());
            assert_eq!(result.is_ok(), allowed, "level {level}");
            if allowed {
                assert_eq!(result.unwrap().records.len(), 2);
            }
        }

        // L3 with a standing grant denies (per-request only); a live
        // per-request grant allows exactly once.
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let denied = gate.query(
            &plugin,
            TrustLevel::NativeSidecar,
            &make_query(scope.clone(), 4),
            &transcript_store(),
        );
        assert_eq!(
            denied.unwrap_err().denial_kind(),
            Some(HistoryDenialKind::MissingGrant)
        );
        gate.issue_grant(HistoryGrant::per_request(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        assert!(
            gate.query(
                &plugin,
                TrustLevel::NativeSidecar,
                &make_query(scope.clone(), 4),
                &transcript_store()
            )
            .is_ok()
        );
        // Single-use: the second read denies.
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::NativeSidecar,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::MissingGrant)
        );

        // L4 with a standing grant denies (per-invocation only); a live
        // per-invocation grant allows exactly once.
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let denied = gate.query(
            &plugin,
            TrustLevel::ExternalTool,
            &make_query(scope.clone(), 4),
            &transcript_store(),
        );
        assert_eq!(
            denied.unwrap_err().denial_kind(),
            Some(HistoryDenialKind::UnknownTrustOrDomain)
        );
        gate.issue_grant(HistoryGrant::per_request(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        assert!(
            gate.query(
                &plugin,
                TrustLevel::ExternalTool,
                &make_query(scope.clone(), 4),
                &transcript_store()
            )
            .is_ok()
        );
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ExternalTool,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::UnknownTrustOrDomain)
        );
    }

    #[test]
    fn default_denies_without_any_grant() {
        let plugin = pid("xuepoo.search");
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        for level in [
            TrustLevel::BundledLua,
            TrustLevel::ThirdPartyLua,
            TrustLevel::NativeSidecar,
            TrustLevel::ExternalTool,
        ] {
            let scope = ws_scope(HistorySource::Transcript, "ws-1");
            let error = gate
                .query(&plugin, level, &make_query(scope, 4), &transcript_store())
                .unwrap_err();
            assert!(
                matches!(
                    error.denial_kind(),
                    Some(HistoryDenialKind::MissingGrant)
                        | Some(HistoryDenialKind::UnknownTrustOrDomain)
                ),
                "level {level}: {error}"
            );
        }
        // A stranger plugin is denied even when another plugin holds a grant.
        let mut gate = granted_gate(&plugin);
        let stranger = pid("mallory.copy");
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &stranger,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::MissingGrant)
        );
    }

    // ── scope intersection ─────────────────────────────────────────────

    #[test]
    fn scope_intersection_or_deny() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);

        // Same workspace extent: allowed.
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .is_ok()
        );
        // Cross-workspace: denied.
        let scope = ws_scope(HistorySource::Transcript, "ws-2");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::ScopeMismatch)
        );
        // Cross-panel under a panel grant: denied.
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            panel_scope(HistorySource::Transcript, "pane-a", "ws-1"),
        ));
        let scope = panel_scope(HistorySource::Transcript, "pane-b", "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::ScopeMismatch)
        );
        // Matching panel extent: allowed.
        let scope = panel_scope(HistorySource::Transcript, "pane-a", "ws-1");
        assert!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .is_ok()
        );
        // Cross-source: a transcript grant never implies a commands grant.
        gate.set_capture(HistorySource::CommandHistory, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::CommandHistory, "ws-1"),
        ));
        let scope = ws_scope(HistorySource::CommandHistory, "ws-1");
        let mut commands = HistorySnapshot::new();
        commands.push(
            HistorySource::CommandHistory,
            row("pane-a", "ws-1", 0, "redacted command"),
        );
        assert!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &commands
            )
            .is_ok()
        );
    }

    #[test]
    fn scopes_reject_wildcards_and_unscoped() {
        // `*` and `all` are never extents.
        assert!(HistoryScope::new(HistorySource::Transcript, Some("*"), None).is_err());
        assert!(HistoryScope::new(HistorySource::Transcript, None, Some("all")).is_err());
        assert!(HistoryScope::new(HistorySource::Transcript, Some("ALL"), None).is_err());
        // Unscoped transcript/command grants are rejected at construction.
        assert!(HistoryScope::new(HistorySource::Transcript, None, None).is_err());
        assert!(HistoryScope::new(HistorySource::CommandHistory, None, None).is_err());
        // KV carries no panel/workspace extent (namespace-implicit).
        assert!(HistoryScope::new(HistorySource::PluginKv, None, None).is_ok());
        assert!(HistoryScope::new(HistorySource::PluginKv, Some("pane-a"), None).is_err());
        assert!(HistoryScope::new(HistorySource::PluginKv, None, Some("ws-1")).is_err());
    }

    // ── all eight denial categories ────────────────────────────────────

    #[test]
    fn denial_taxonomy_is_complete_and_coded() {
        assert_eq!(HistoryDenialKind::all().len(), 8);
        let codes: Vec<&str> = HistoryDenialKind::all()
            .iter()
            .map(|kind| kind.code())
            .collect();
        assert_eq!(
            codes,
            vec![
                "E_HISTORY_MISSING_GRANT",
                "E_HISTORY_REVOKED_GRANT",
                "E_HISTORY_SCOPE_MISMATCH",
                "E_HISTORY_OVER_BOUND",
                "E_HISTORY_CAPTURE_DISABLED",
                "E_HISTORY_SAFE_MODE",
                "E_HISTORY_TRUST_DENIED",
                "E_HISTORY_UNAVAILABLE",
            ]
        );
    }

    #[test]
    fn each_denial_category_is_reachable() {
        let plugin = pid("xuepoo.search");

        // 1. MissingGrant.
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::MissingGrant)
        );

        // 2. RevokedOrExpired (revoke path; expiry path below).
        let mut gate = granted_gate(&plugin);
        gate.revoke(&plugin, HistorySource::Transcript);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::RevokedOrExpired)
        );
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        let mut grant =
            HistoryGrant::standing(plugin.clone(), ws_scope(HistorySource::Transcript, "ws-1"));
        grant.expires_at = Some(10);
        gate.issue_grant(grant);
        gate.advance_time(10);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::RevokedOrExpired)
        );

        // 3. ScopeMismatch.
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-9");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::ScopeMismatch)
        );

        // 4. OverBoundOrRate (static bound here; window path below).
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let mut over = make_query(scope, caps().max_rows_per_query + 1);
        over.max_bytes = caps().max_bytes_per_query;
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &over,
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::OverBoundOrRate)
        );

        // 5. CaptureDisabled (opt-in off reads nothing).
        let mut gate = granted_gate(&plugin);
        gate.set_capture(HistorySource::Transcript, false);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::CaptureDisabled)
        );

        // 6. SafeMode.
        let mut gate = granted_gate(&plugin);
        gate.set_safe_mode(true);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::SafeMode)
        );

        // 7. UnknownTrustOrDomain (L4 standing admission).
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ExternalTool,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::UnknownTrustOrDomain)
        );

        // 8. PurgedOrExpired (range covering only purged rows).
        let mut gate = granted_gate(&plugin);
        let mut purged_store = HistorySnapshot::new();
        let mut purged_row = row("pane-a", "ws-1", 0, "purged bytes never return");
        purged_row.purged = true;
        purged_store.push(HistorySource::Transcript, purged_row);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &purged_store
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::PurgedOrExpired)
        );
    }

    // ── oracle-tightness ───────────────────────────────────────────────

    #[test]
    fn denials_are_oracle_tight() {
        let plugin = pid("xuepoo.search");
        let stranger = pid("mallory.copy");

        // Unauthorized queries deny identically whether matching content
        // exists or not: no absent-versus-denied signal.
        let mut secret_store = HistorySnapshot::new();
        secret_store.push(HistorySource::Transcript, row("pane-a", "ws-1", 0, SECRET));
        let empty_store = HistorySnapshot::new();
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let with_content = gate
            .query(
                &stranger,
                TrustLevel::ThirdPartyLua,
                &make_query(scope.clone(), 4),
                &secret_store,
            )
            .unwrap_err()
            .message();
        let without_content = gate
            .query(
                &stranger,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &empty_store,
            )
            .unwrap_err()
            .message();
        assert_eq!(with_content, without_content);
        assert!(!with_content.contains(SECRET));
        assert!(!with_content.contains("mallory"));
        assert!(!with_content.contains("pane-a"));

        // Every denial names the level and the family only.
        let mut gate = granted_gate(&plugin);
        gate.set_safe_mode(true);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let denial = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &secret_store,
            )
            .unwrap_err()
            .message();
        assert!(denial.contains("third-party-lua"), "{denial}");
        assert!(denial.contains("'history'"), "{denial}");
        assert!(!denial.contains(SECRET));
    }

    // ── budgets ────────────────────────────────────────────────────────

    #[test]
    fn window_budgets_deny_with_attribution() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");

        for _ in 0..caps().max_queries_per_window {
            assert!(
                gate.query(
                    &plugin,
                    TrustLevel::ThirdPartyLua,
                    &make_query(scope.clone(), 4),
                    &transcript_store()
                )
                .is_ok()
            );
        }
        // Window exhausted: the next poll denies (no tail-follow).
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope.clone(), 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::OverBoundOrRate)
        );
        // Attribution: the ledger names per-plugin usage.
        let (queries, bytes) = gate.window_usage(&plugin);
        assert_eq!(queries, caps().max_queries_per_window);
        assert!(bytes > 0);
        // A new window resets attribution.
        gate.advance_window();
        assert_eq!(gate.window_usage(&plugin), (0, 0));
        assert!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .is_ok()
        );
    }

    #[test]
    fn zero_caps_are_misconfiguration_not_lockdown() {
        assert!(HistoryCaps::new(0, 4096, 256, 4, 8192).is_err());
        assert!(HistoryCaps::new(16, 0, 256, 4, 8192).is_err());
        assert!(HistoryCaps::new(16, 4096, 0, 4, 8192).is_err());
        assert!(HistoryCaps::new(16, 4096, 256, 0, 8192).is_err());
        assert!(HistoryCaps::new(16, 4096, 256, 4, 0).is_err());
    }

    // ── labels ─────────────────────────────────────────────────────────

    #[test]
    fn labels_survive_redaction_and_truncation() {
        let plugin = pid("xuepoo.search");
        let mut gate = HistoryGate::new(HistoryCaps::new(16, 65536, 8, 64, 65536).unwrap());
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        let mut store = HistorySnapshot::new();
        store.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 0, "0123456789abcdef-too-long"),
        );
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &store,
            )
            .unwrap();
        assert_eq!(page.records.len(), 1);
        let record = &page.records[0];
        assert!(record.truncated, "over-cap bodies truncate, marked");
        assert!(record.redacted);
        assert_eq!(record.label, UntrustedLabel);
        assert_eq!(record.label.as_str(), UntrustedLabel::VALUE);
        assert_eq!(record.label.as_str(), "untrusted-observation");
    }

    #[test]
    fn export_preview_equality_holds() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store(),
            )
            .unwrap();
        assert_eq!(page.preview_bytes(), page.export_bytes());
    }

    // ── safe mode + opt-in ─────────────────────────────────────────────

    #[test]
    fn safe_mode_reads_nothing_identically() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        gate.set_safe_mode(true);
        // Every level denies in safe mode, including L0 Core itself.
        for level in [
            TrustLevel::Core,
            TrustLevel::BundledLua,
            TrustLevel::ThirdPartyLua,
            TrustLevel::NativeSidecar,
            TrustLevel::ExternalTool,
        ] {
            let scope = ws_scope(HistorySource::Transcript, "ws-1");
            let error = gate
                .query(&plugin, level, &make_query(scope, 4), &transcript_store())
                .unwrap_err();
            assert_eq!(
                error.denial_kind(),
                Some(HistoryDenialKind::SafeMode),
                "level {level}"
            );
        }
        gate.set_safe_mode(false);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .is_ok()
        );
    }

    #[test]
    fn capture_off_reads_nothing_but_kv_still_serves() {
        let plugin = pid("xuepoo.search");
        let mut gate = HistoryGate::new(caps());
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            HistoryScope::new(HistorySource::PluginKv, None, None).unwrap(),
        ));
        // Opt-in off: transcript denies, KV (plugin-authored) still serves.
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::CaptureDisabled)
        );
        let mut kv = HistorySnapshot::new();
        kv.push(HistorySource::PluginKv, kv_row(&plugin, 0, "own value"));
        let kv_scope = HistoryScope::new(HistorySource::PluginKv, None, None).unwrap();
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(kv_scope, 4),
                &kv,
            )
            .unwrap();
        assert_eq!(page.records.len(), 1);
    }

    // ── no streaming ───────────────────────────────────────────────────

    #[test]
    fn poll_loop_cannot_reconstitute_tail_follow() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");

        // Pages are point-in-time with no freshness promise and no cursor:
        // the type carries no continuation token by construction.
        let first = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope.clone(), 4),
                &transcript_store(),
            )
            .unwrap();
        assert_eq!(first.freshness, Freshness::PointInTimeNoGuarantee);
        assert_eq!(first.total_in_scope, 2);

        // A poll loop burns the window budget and then denies: polling
        // cannot reconstitute a live stream.
        let mut polls = 1u64; // first query above already charged one
        loop {
            let result = gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope.clone(), 4),
                &transcript_store(),
            );
            match result {
                Ok(_) => polls += 1,
                Err(error) => {
                    assert_eq!(
                        error.denial_kind(),
                        Some(HistoryDenialKind::OverBoundOrRate)
                    );
                    break;
                }
            }
            assert!(polls < 100, "budget must stop the loop");
        }
        assert_eq!(polls, caps().max_queries_per_window);

        // Rows appended after the snapshot do not rewrite the returned page.
        let mut later = transcript_store();
        later.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 2, "newer bytes"),
        );
        assert_eq!(first.records.len(), 2);
        assert!(!first.records.iter().any(|row| row.body.contains("newer")));
    }

    #[test]
    fn tail_and_search_are_bounded_snapshots() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        let mut tail_query = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 1);
        tail_query.op = QueryOp::Tail;
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &tail_query,
                &transcript_store(),
            )
            .unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].seq, 1);
        assert_eq!(page.total_in_scope, 2);

        let mut search = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 16);
        search.op = QueryOp::Search {
            needle: "line two".to_string(),
        };
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &search,
                &transcript_store(),
            )
            .unwrap();
        assert_eq!(page.records.len(), 1);
        assert!(page.records[0].body.contains("line two"));

        // Empty and oversize needles deny (bounded, never silent).
        let mut bad = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 16);
        bad.op = QueryOp::Search {
            needle: String::new(),
        };
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &bad,
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::OverBoundOrRate)
        );
        let mut bad = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 16);
        bad.op = QueryOp::Search {
            needle: "x".repeat(MAX_NEEDLE_BYTES + 1),
        };
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &bad,
                &transcript_store()
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::OverBoundOrRate)
        );
    }

    // ── session snapshots excluded ─────────────────────────────────────

    #[test]
    fn session_snapshots_are_never_queryable() {
        for spelling in [
            "session",
            "snapshot",
            "session-snapshot",
            "history.db",
            "segments",
        ] {
            assert!(
                HistorySource::parse(spelling).is_err(),
                "'{spelling}' must not parse as a queryable source"
            );
        }
        for spelling in [
            "transcript",
            "commands",
            "command-history",
            "kv",
            "plugin-kv",
        ] {
            assert!(HistorySource::parse(spelling).is_ok());
        }
        // No history head names a snapshot, and no head lives under
        // `terminal.*`.
        for source in [
            HistorySource::Transcript,
            HistorySource::CommandHistory,
            HistorySource::PluginKv,
        ] {
            let head = source.capability_head();
            assert!(head.starts_with("history."), "{head}");
            assert!(!head.starts_with("terminal."), "{head}");
            assert!(!head.contains("session"), "{head}");
            assert!(!head.contains("snapshot"), "{head}");
            assert_eq!(
                CapabilityId::parse(head).unwrap().family(),
                CapabilityFamily::History
            );
        }
    }

    // ── KV own-namespace ───────────────────────────────────────────────

    #[test]
    fn kv_reads_stay_in_the_caller_namespace() {
        let plugin = pid("xuepoo.search");
        let other = pid("mallory.copy");
        let mut gate = HistoryGate::new(caps());
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            HistoryScope::new(HistorySource::PluginKv, None, None).unwrap(),
        ));
        let mut kv = HistorySnapshot::new();
        kv.push(HistorySource::PluginKv, kv_row(&plugin, 0, "own value"));
        kv.push(HistorySource::PluginKv, kv_row(&other, 1, "foreign value"));
        let kv_scope = HistoryScope::new(HistorySource::PluginKv, None, None).unwrap();
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(kv_scope, 4),
                &kv,
            )
            .unwrap();
        // Foreign rows are never collected, never counted, never named.
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.total_in_scope, 1);
        assert_eq!(page.records[0].body, "own value");
    }

    // ── v1 frozen + no migration ───────────────────────────────────────

    #[test]
    fn no_grant_migration_either_direction() {
        // The closed sets are disjoint: a history grant never implies a
        // `terminal.*` grant and vice versa (structural: distinct families,
        // distinct heads, per-source grant records).
        let history_heads = [
            "history.transcript.read",
            "history.commands.read",
            "history.kv.read",
        ];
        let terminal_heads = CapabilityFamily::Terminal.closed_identifiers();
        for head in history_heads {
            assert!(!terminal_heads.contains(&head), "{head}");
            let id = CapabilityId::parse(head).unwrap();
            assert_eq!(id.family(), CapabilityFamily::History);
            assert_ne!(id.family(), CapabilityFamily::Terminal);
        }
        for head in terminal_heads {
            let id = CapabilityId::parse(head).unwrap();
            assert_eq!(id.family(), CapabilityFamily::Terminal);
            assert_ne!(id.family(), CapabilityFamily::History);
        }
        // A history read never confers clipboard/filesystem/process
        // authority: the gate returns data into the VM only, and no history
        // head shares a family with those authorities.
        for head in ["clipboard.write", "fs.read:~/docs/**", "process.spawn:git"] {
            let id = CapabilityId::parse(head).unwrap();
            assert_ne!(id.family(), CapabilityFamily::History);
        }
    }

    // ── version / compat ───────────────────────────────────────────────

    #[test]
    fn version_mismatch_disables_with_diagnostic() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let mut request = make_query(scope, 4);
        request.client_version = HISTORY_READ_VERSION + 1;
        let error = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &request,
                &transcript_store(),
            )
            .unwrap_err();
        // Disabled with a diagnostic — never a denial category.
        assert_eq!(error.denial_kind(), None);
        let message = error.message();
        assert!(message.contains("version mismatch"), "{message}");
        // Host-side mismatch disables identically.
        gate.set_host_version(HISTORY_READ_VERSION + 1);
        let scope = ws_scope(HistorySource::Transcript, "ws-1");
        let error = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(scope, 4),
                &transcript_store(),
            )
            .unwrap_err();
        assert_eq!(error.denial_kind(), None);
    }

    #[test]
    fn safe_mode_is_identical_across_versions() {
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        gate.set_safe_mode(true);
        // Safe mode denies identically whether versions match or not
        // (safe mode is checked before compat).
        for version in [HISTORY_READ_VERSION, HISTORY_READ_VERSION + 1] {
            let mut scoped = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 4);
            scoped.client_version = version;
            assert_eq!(
                gate.query(
                    &plugin,
                    TrustLevel::ThirdPartyLua,
                    &scoped,
                    &transcript_store()
                )
                .unwrap_err()
                .denial_kind(),
                Some(HistoryDenialKind::SafeMode),
                "version {version}"
            );
        }
    }

    // ── CodeRabbit review regressions (PR #1673) ────────────────────────

    #[test]
    fn second_grant_intersects_when_first_does_not() {
        // Standing grants for ws-1 then ws-2: a ws-2 query must select the
        // intersecting grant instead of denying against the first one.
        let plugin = pid("xuepoo.search");
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-2"),
        ));
        let mut store = HistorySnapshot::new();
        store.push(
            HistorySource::Transcript,
            row("pane-b", "ws-2", 0, "ws-two bytes"),
        );
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(ws_scope(HistorySource::Transcript, "ws-2"), 4),
                &store,
            )
            .expect("second grant intersects the query");
        assert_eq!(page.records.len(), 1);
        // ... and the first grant still serves its own scope.
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(ws_scope(HistorySource::Transcript, "ws-1"), 4),
                &transcript_store(),
            )
            .unwrap();
        assert_eq!(page.records.len(), 2);
    }

    #[test]
    fn per_request_use_charges_the_matching_grant() {
        // Two per-request grants, ws-1 then ws-2: a ws-2 query consumes the
        // second grant's single use, leaving the first intact.
        let plugin = pid("xuepoo.search");
        let mut gate = HistoryGate::new(caps());
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::per_request(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        gate.issue_grant(HistoryGrant::per_request(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-2"),
        ));
        let mut store = HistorySnapshot::new();
        store.push(
            HistorySource::Transcript,
            row("pane-b", "ws-2", 0, "ws-two bytes"),
        );
        assert!(
            gate.query(
                &plugin,
                TrustLevel::NativeSidecar,
                &make_query(ws_scope(HistorySource::Transcript, "ws-2"), 4),
                &store,
            )
            .is_ok()
        );
        // The ws-1 grant still holds its single use.
        assert!(
            gate.query(
                &plugin,
                TrustLevel::NativeSidecar,
                &make_query(ws_scope(HistorySource::Transcript, "ws-1"), 4),
                &transcript_store(),
            )
            .is_ok()
        );
        // Both uses are now spent: further reads deny.
        assert_eq!(
            gate.query(
                &plugin,
                TrustLevel::NativeSidecar,
                &make_query(ws_scope(HistorySource::Transcript, "ws-1"), 4),
                &transcript_store(),
            )
            .unwrap_err()
            .denial_kind(),
            Some(HistoryDenialKind::MissingGrant)
        );
    }

    #[test]
    fn window_byte_budget_covers_the_returned_page() {
        // Window of 100 bytes with two 60-byte rows: the first page fits
        // the remaining budget, the second page would overshoot and denies.
        let plugin = pid("xuepoo.search");
        let tight = HistoryCaps::new(16, 4096, 256, 64, 100).unwrap();
        let mut gate = HistoryGate::new(tight);
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        let mut store = HistorySnapshot::new();
        store.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 0, &"r".repeat(60)),
        );
        store.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 1, &"s".repeat(60)),
        );
        let mut one = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 1);
        let page = gate
            .query(&plugin, TrustLevel::ThirdPartyLua, &one, &store)
            .unwrap();
        assert_eq!(page.records.len(), 1);
        one.row_start = 1;
        assert_eq!(
            gate.query(&plugin, TrustLevel::ThirdPartyLua, &one, &store)
                .unwrap_err()
                .denial_kind(),
            Some(HistoryDenialKind::OverBoundOrRate)
        );
        // Attribution stops at the admitted page.
        assert_eq!(gate.window_usage(&plugin), (1, 60));
    }

    #[test]
    fn search_never_matches_purged_rows() {
        // A needle matching only purged content returns an empty page —
        // never an unavailability signal that would oracle purged-content
        // existence, and never the purged bytes.
        let plugin = pid("xuepoo.search");
        let mut gate = granted_gate(&plugin);
        let mut store = HistorySnapshot::new();
        let mut purged_row = row("pane-a", "ws-1", 0, "purged needle-holder bytes");
        purged_row.purged = true;
        store.push(HistorySource::Transcript, purged_row);
        store.push(
            HistorySource::Transcript,
            row("pane-a", "ws-1", 1, "live unrelated bytes"),
        );
        let mut search = make_query(ws_scope(HistorySource::Transcript, "ws-1"), 16);
        search.op = QueryOp::Search {
            needle: "needle-holder".to_string(),
        };
        let page = gate
            .query(&plugin, TrustLevel::ThirdPartyLua, &search, &store)
            .expect("purged-only matches stay invisible to search");
        assert!(page.records.is_empty());
        assert!(
            !page
                .preview_bytes()
                .iter()
                .any(|body| body.contains("purged"))
        );
    }

    #[test]
    fn truncation_stops_on_char_boundary() {
        // Two-byte é with an odd row cap: truncating at the raw byte cap
        // would split a char and panic; the gate walks back to the
        // boundary instead.
        let plugin = pid("xuepoo.search");
        let odd = HistoryCaps::new(16, 4096, 7, 64, 65536).unwrap();
        let mut gate = HistoryGate::new(odd);
        gate.set_capture(HistorySource::Transcript, true);
        gate.issue_grant(HistoryGrant::standing(
            plugin.clone(),
            ws_scope(HistorySource::Transcript, "ws-1"),
        ));
        let mut store = HistorySnapshot::new();
        store.push(HistorySource::Transcript, row("pane-a", "ws-1", 0, "ééééé"));
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &make_query(ws_scope(HistorySource::Transcript, "ws-1"), 4),
                &store,
            )
            .unwrap();
        assert_eq!(page.records.len(), 1);
        let record = &page.records[0];
        assert!(record.truncated);
        assert_eq!(record.body, "ééé");
        assert_eq!(record.body.len(), 6);
        assert_eq!(record.label.as_str(), UntrustedLabel::VALUE);
    }
}
