//! Filesystem authorization: sensitive-path policy plus secret detection
//! (research 045 section 4).
//!
//! Prompt rules cannot protect `.env`, `.ssh`, and credential stores: a later
//! turn can still run `cat .env` or `open(".env").read()`. This module is the
//! real authorization layer in the filesystem path that the task requires:
//! every FS request (agent tools, plugin capability calls, execution
//! requests) evaluates `FilesystemScope` + `SensitivePathPolicy` + secret
//! detection, default-denying sensitive locations with an explicit
//! user-consent path rather than silent access.
//!
//! # Composition (conforms, never duplicates)
//!
//! - `manifest::is_hostile_fs_pattern` (CTX-0465/0489/0495 waves): the
//!   grant-shape predicate for capability *patterns* (absolute paths,
//!   `..` traversal, overbroad home roots, credential-location segments).
//!   This module never re-implements it: [`FilesystemScope::allows`] calls it
//!   to reject hostile patterns before scope matching.
//! - `effective::authorize` (CTX-0524): the six-layer capability
//!   intersection. This module never re-implements it: the host seam
//!   (`PluginHost::authorize_fs`) authorizes through `authorize_effective`
//!   first, then applies the sensitive-path and content layers below.
//! - `secrets::looks_like_secret_token` / `is_sensitive_env_name` (CTX-0521):
//!   the fail-closed secret-shape heuristics. This module reuses them for
//!   content-based detection so renamed secrets (`secret.txt`,
//!   `credentials.json`, `prod-config.yaml`) are still caught, and routes
//!   file bytes through `scrub_against_store`-compatible redaction before
//!   any agent-visible boundary.
//! - Threat T-03 (deny-by-default resource loader and safe-path policy):
//!   this layer is the FS-authorization half; real-path/symlink/device
//!   resolution stays with the host I/O boundary.
//!
//! # What this module owns
//!
//! - [`FilesystemScope`]: the granted path set for one request (exact
//!   capability-shaped patterns; deny-by-default, Lua cannot widen it).
//! - [`SensitivePathPolicy`]: the default-deny sensitive-location set
//!   (`.env` and `.env.*` variants, `~/.ssh/**`, `~/.gnupg/**`,
//!   `~/.aws/credentials`, token stores, browser credential stores) plus
//!   explicit per-path user consent (the `secret://`-style escape hatch is
//!   consent keyed by path, never a value).
//! - [`FsDecision`]: the typed outcome — `Allow`, `Deny` (typed
//!   [`FsDenialKind`], path quoted, value never quoted), or
//!   [`FsDecision::ConsentRequired`] (explicit user-consent path).
//! - [`FsAuditLedger`]: bounded drop-oldest audit of every decision (names
//!   and paths only, never file values).
//! - Content-based detection ([`content_looks_secret`]): fail-closed
//!   over-reject on read paths, consistent with the CTX-0521 heuristics.
//!
//! The exact default pattern set and detection heuristics need security
//! review (045 open item) and are not fixed here: the defaults below are the
//! reviewable starting set, and [`SensitivePathPolicy::default`] documents
//! that status.
//!
//! There is no `unsafe`, no new dependency (`std` only), no I/O, and no wall
//! clock: consent grants carry host monotonic `u64` timestamps like the
//! secret store. No host path is hardcoded: callers supply every root.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::manifest::is_hostile_fs_pattern;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum path bytes accepted by the FS authorization path.
///
/// Precedent: `bitty-runtime` `PROJECT_PATH_MAX_BYTES` (`4096`, cwd-style
/// parser bound). Over-bound paths fail closed.
pub const MAX_FS_PATH_BYTES: usize = 4096;

/// Maximum bytes of one read payload scanned for secret shapes.
///
/// Precedent: `secrets::MAX_SECRET_FILE_LINE_BYTES` (`4096 + 128`);
/// content scanning is line-oriented and bounded per line, so a whole-file
/// cap is not needed — the per-call line cap below bounds the work.
pub const MAX_FS_CONTENT_SCAN_BYTES: usize = 64 * 1024;

/// Maximum content lines scanned per read (fail-closed: over-bound content
/// is treated as secret-shaped rather than passed through).
pub const MAX_FS_CONTENT_SCAN_LINES: usize = 1024;

/// Maximum bytes per scanned content line.
pub const MAX_FS_CONTENT_LINE_BYTES: usize = 4096 + 128;

/// Maximum audit entries retained ([`FsAuditLedger`], drop-oldest).
pub const MAX_FS_AUDIT_ENTRIES: usize = 1024;

/// Maximum items named in one audit entry's path list (remainder counted).
pub const MAX_FS_AUDIT_ITEMS: usize = 8;

/// Maximum sensitive-path consent grants tracked by one policy.
pub const MAX_FS_CONSENTS: usize = 256;

// ── path normalization (compose with the 0489/0495 waves) ─────────────────

/// Split `path` on either separator (`/` or `\`) preserving empties.
fn split_segments(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).collect()
}

/// Normalize `.`/empty segments the same way the manifest predicate does.
///
/// A leading run of no-op segments is preserved (it keeps the path
/// relative: `./~/.config/gh/x` names a literal `~` directory, not the
/// credential prefix); interior no-ops collapse. This is the same rule as
/// `is_hostile_fs_pattern` so request-path evaluation cannot drift from
/// grant-pattern evaluation.
fn normalize_segments<'a>(segments: &'a [&'a str]) -> Vec<&'a str> {
    match segments
        .iter()
        .position(|segment| !segment.is_empty() && *segment != ".")
    {
        Some(root) => segments[..root]
            .iter()
            .copied()
            .chain(
                segments[root..]
                    .iter()
                    .copied()
                    .filter(|segment| !segment.is_empty() && *segment != "."),
            )
            .collect(),
        None => Vec::new(),
    }
}

/// Whether `path` is an absolute path on either platform.
///
/// Leading `/`, Windows drive (`C:/`, `C:\`), UNC (`\\`). Absolute paths
/// are not representable in the capability grammar (the manifest predicate
/// denies them as patterns); at request time they fail closed as
/// [`FsDenialKind::OutsideScope`] unless a scope pattern explicitly covers
/// them — and no default scope does.
fn is_absolute_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with("\\\\") {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Whether a scope `pattern` text is a literal pin (not a glob matcher).
///
/// Mirrors the manifest predicate: a pattern containing glob syntax matches
/// an unknown set and cannot certify a sensitive-adjacent grant. Used by
/// the host seam to reject glob-widened consent requests.
#[must_use]
pub fn is_literal_scope_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.split(['/', '\\']).all(|segment| {
            segment.is_empty()
                || *segment == *"."
                || !segment.contains(['*', '?', '[', ']', '{', '}'])
        })
}

// ── FilesystemScope ───────────────────────────────────────────────────────

/// The granted path set for one FS request.
///
/// Patterns are capability-shaped (`fs.read:`/`fs.write:` parameter text):
/// `~`-rooted globs, project-relative globs, or bare relative paths.
/// Deny-by-default: an empty scope allows nothing. Hostile patterns (per
/// [`is_hostile_fs_pattern`]) never enter the scope — construction fails
/// closed so a hostile grant cannot be smuggled through this type.
///
/// Lua and agent requests supply candidate patterns; the host intersects
/// them with the effective capability set (CTX-0524) before building this
/// scope, so this type only narrows. There is deliberately no `allow_all`
/// constructor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilesystemScope {
    patterns: BTreeSet<String>,
}

impl FilesystemScope {
    /// Empty scope (allows nothing).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a scope from candidate patterns, fail-closed.
    ///
    /// Every pattern is validated: empty, over-bound (`> 512` bytes, the
    /// manifest pattern bound), control/whitespace-bearing, and hostile
    /// (per [`is_hostile_fs_pattern`]) patterns are rejected with a typed
    /// [`FsError`]. No pattern is silently dropped.
    pub fn from_patterns(patterns: &[String]) -> Result<Self, FsError> {
        let mut set = BTreeSet::new();
        for pattern in patterns {
            validate_scope_pattern(pattern)?;
            set.insert(pattern.clone());
        }
        Ok(Self { patterns: set })
    }

    /// Granted patterns (sorted, deduplicated).
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        self.patterns.iter().cloned().collect()
    }

    /// Whether any pattern is granted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Number of granted patterns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether `path` is covered by at least one granted pattern.
    ///
    /// Pure, bounded, fail-closed: empty, over-bound, NUL/control-bearing,
    /// and `..`-traversal paths are never covered. Matching composes with
    /// the grant waves: a `~`-rooted pattern covers its literal subtree
    /// with `**` recursion; a bare relative pattern covers its own subtree
    /// only (it never reaches `~` or `/` roots). Matching is ASCII
    /// case-insensitive per segment (Windows/macOS filesystems) on either
    /// separator, consistent with the manifest predicate.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        if path.is_empty() || path.len() > MAX_FS_PATH_BYTES || path.contains('\0') {
            return false;
        }
        if path.chars().any(|c| c.is_control()) {
            return false;
        }
        let segments = split_segments(path);
        if segments.contains(&"..") {
            return false;
        }
        let normalized = normalize_segments(&segments);
        if normalized.is_empty() {
            return false;
        }
        // A leading run of no-op segments pins the request as relative: it
        // names a literal directory, never `~` or a sensitive root.
        let leading_noop = segments
            .iter()
            .take_while(|s| s.is_empty() || **s == ".")
            .count();
        let rooted_home = leading_noop == 0 && normalized.first() == Some(&"~");
        self.patterns
            .iter()
            .any(|pattern| pattern_covers(pattern, &normalized, rooted_home))
    }
}

/// Validate one scope pattern (fail-closed, no side effects).
fn validate_scope_pattern(pattern: &str) -> Result<(), FsError> {
    if pattern.is_empty() {
        return Err(FsError::invalid_request("scope pattern must not be empty"));
    }
    if pattern.len() > 512 {
        return Err(FsError::limit_exceeded("scope pattern", 512, pattern.len()));
    }
    if pattern
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(FsError::invalid_request(
            "scope pattern must not contain control characters or whitespace",
        ));
    }
    if is_hostile_fs_pattern(pattern) {
        return Err(FsError::hostile_pattern(pattern));
    }
    Ok(())
}

/// Whether a validated scope `pattern` covers a normalized request path.
///
/// `normalized` is the request path after `.`/empty normalization;
/// `rooted_home` records whether the raw request was `~`-rooted (a
/// leading no-op run pins it relative instead). Both pattern and request
/// match ASCII case-insensitively per segment on either separator.
fn pattern_covers(pattern: &str, normalized: &[&str], rooted_home: bool) -> bool {
    let pattern_segments = split_segments(pattern);
    let pattern_normalized = normalize_segments(&pattern_segments);
    if pattern_normalized.is_empty() {
        return false;
    }
    let pattern_home = pattern_normalized.first() == Some(&"~");
    if pattern_home != rooted_home {
        // A `~`-rooted grant never covers a relative request and vice
        // versa; absolute requests are never `~`-rooted either.
        return false;
    }
    glob_segments_cover(&pattern_normalized, normalized)
}

/// Whether `pattern` segments (with `*`/`**` globs) cover `path` segments.
///
/// `*` matches within one segment, `**` crosses segments; any other
/// segment matches ASCII case-insensitively as a literal-or-`?`/`[...]`
/// glob. A trailing `/**` covers the subtree root itself.
fn glob_segments_cover(pattern: &[&str], path: &[&str]) -> bool {
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si: Option<usize> = None;
    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == "**" {
            // Trailing `**` covers the subtree root itself and everything below.
            if pi + 1 == pattern.len() {
                return true;
            }
            star_pi = Some(pi);
            star_si = Some(si.saturating_add(1));
            pi += 1;
            continue;
        }
        if pi < pattern.len() && segment_glob_match(pattern[pi], path[si]) {
            pi += 1;
            si += 1;
            continue;
        }
        if let (Some(sp), Some(ss)) = (star_pi, star_si) {
            if ss <= path.len() {
                pi = sp + 1;
                si = ss;
                star_si = Some(ss + 1);
                continue;
            }
        }
        return false;
    }
    while pi < pattern.len() && pattern[pi] == "**" {
        pi += 1;
    }
    pi == pattern.len()
}

/// Whether one glob `pattern` segment matches one path `segment`.
///
/// `*` matches any run within the segment, `?` matches one char,
/// `[...]` matches one char class (with `^`/`!` negation and `-` ranges);
/// every other byte matches ASCII case-insensitively. `{...}` alternation
/// is not expanded here: a literal `{` only matches itself, so brace
/// spellings never widen coverage (fail-closed).
fn segment_glob_match(pattern: &str, segment: &str) -> bool {
    let pat: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let text: Vec<u8> = segment.bytes().map(|b| b.to_ascii_lowercase()).collect();
    segment_match(&pat, &text)
}

fn segment_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while si < text.len() {
        if pi < pat.len() && pat[pi] == b'[' {
            if let Some(consumed) = match_class(&pat[pi..], text[si]) {
                pi += consumed;
                si += 1;
                continue;
            }
            if let Some(s) = star {
                pi = s + 1;
                mark += 1;
                si = mark;
                continue;
            }
            return false;
        }
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == text[si]) {
            pi += 1;
            si += 1;
            continue;
        }
        if pi < pat.len() && pat[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
            continue;
        }
        if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            si = mark;
            continue;
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Match one `[...]` class against `byte`; returns consumed pattern bytes.
fn match_class(pat: &[u8], byte: u8) -> Option<usize> {
    let mut i = 1usize;
    let mut negate = false;
    if i < pat.len() && (pat[i] == b'^' || pat[i] == b'!') {
        negate = true;
        i += 1;
    }
    let mut matched = false;
    let mut consumed_any = false;
    while i < pat.len() && pat[i] != b']' {
        consumed_any = true;
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let (lo, hi) = (pat[i], pat[i + 2]);
            if lo <= byte && byte <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if pat[i] == byte {
                matched = true;
            }
            i += 1;
        }
    }
    if !consumed_any || i >= pat.len() || pat[i] != b']' {
        return None;
    }
    if matched != negate { Some(i + 1) } else { None }
}

// ── SensitivePathPolicy ───────────────────────────────────────────────────

/// Default-deny sensitive locations plus explicit per-path user consent.
///
/// The default set covers the task corpus: `.env` and `.env.*` variants
/// (any directory), `~/.ssh/**`, `~/.gnupg/**`, `~/.aws/credentials`,
/// token stores, and browser credential stores. Matching composes with the
/// grant waves: case-insensitive per segment, either separator, with
/// `.`/empty segments normalized away first (so `~/.config/./gh/...`
/// cannot bypass).
///
/// Consent is keyed by normalized path with an explicit user action
/// ([`SensitivePathPolicy::grant_consent`]); a grant moves exactly that
/// path from `Deny` to `ConsentRequired`-cleared `Allow` (when the scope
/// also covers it). Consent names paths only — the `secret://` escape
/// hatch convention from CTX-0521 applies to *values*, while here the
/// hatch is per-path consent, never silent access.
///
/// The exact default set needs security review (045 open item): treat
/// [`SensitivePathPolicy::default`] as the reviewable starting set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitivePathPolicy {
    /// Extra deny prefixes beyond the built-in defaults (normalized form).
    extra_deny: BTreeSet<String>,
    /// Per-path consent grants (normalized path text).
    consents: BTreeMap<String, FsConsent>,
}

impl SensitivePathPolicy {
    /// Policy with the built-in default-deny set and no consents.
    ///
    /// Named `default_policy` (not `default`) so the constructor cannot be
    /// confused with `Default::default`; a `Default` impl below forwards
    /// to it.
    #[must_use]
    pub fn default_policy() -> Self {
        Self {
            extra_deny: BTreeSet::new(),
            consents: BTreeMap::new(),
        }
    }

    /// Add an extra deny prefix (fail-closed on hostile input).
    ///
    /// The prefix is stored normalized (see [`normalize_request_path`]);
    /// hostile-shaped prefixes (absolute paths are allowed here — they
    /// name deny roots, not grants — but empty, over-bound, and
    /// control-bearing text is rejected).
    pub fn deny_prefix(&mut self, prefix: &str) -> Result<(), FsError> {
        let normalized = normalize_request_path(prefix)
            .ok_or_else(|| FsError::invalid_request("deny prefix must be a representable path"))?;
        if self.extra_deny.len() >= MAX_FS_CONSENTS {
            return Err(FsError::limit_exceeded(
                "sensitive deny prefixes",
                MAX_FS_CONSENTS,
                self.extra_deny.len() + 1,
            ));
        }
        self.extra_deny.insert(normalized);
        Ok(())
    }

    /// Grant explicit user consent for exactly one path.
    ///
    /// The path is stored normalized; consent is session-scoped unless
    /// `expires_at_ms` bounds it. Audited by the caller (the host seam
    /// records the consent entry in the [`FsAuditLedger`]).
    pub fn grant_consent(
        &mut self,
        path: &str,
        granted_at_ms: u64,
        expires_at_ms: Option<u64>,
    ) -> Result<(), FsError> {
        let normalized = normalize_request_path(path)
            .ok_or_else(|| FsError::invalid_request("consent path must be representable"))?;
        if !self.consents.contains_key(&normalized) && self.consents.len() >= MAX_FS_CONSENTS {
            return Err(FsError::limit_exceeded(
                "sensitive-path consents",
                MAX_FS_CONSENTS,
                self.consents.len() + 1,
            ));
        }
        self.consents.insert(
            normalized,
            FsConsent {
                granted_at_ms,
                expires_at_ms,
            },
        );
        Ok(())
    }

    /// Revoke consent for one path (missing grants are a no-op).
    pub fn revoke_consent(&mut self, path: &str) {
        if let Some(normalized) = normalize_request_path(path) {
            self.consents.remove(&normalized);
        }
    }

    /// Whether active (unexpired) consent covers `normalized_path`.
    #[must_use]
    pub fn consent_active(&self, normalized_path: &str, now_ms: u64) -> bool {
        self.consents
            .get(normalized_path)
            .is_some_and(|grant| grant.is_active(now_ms))
    }

    /// Whether `path` names a sensitive location (default set + extras).
    ///
    /// Pure, fail-closed: unrepresentable paths (empty, over-bound,
    /// NUL/control-bearing) report sensitive so callers deny rather than
    /// pass bytes through.
    #[must_use]
    pub fn is_sensitive(&self, path: &str) -> bool {
        let Some(normalized) = normalize_request_path(path) else {
            return true;
        };
        if is_default_sensitive(&normalized) {
            return true;
        }
        let segments = key_segments(&normalized);
        self.extra_deny.iter().any(|prefix| {
            let prefix_segments = key_segments(prefix);
            segments.len() >= prefix_segments.len()
                && segments[..prefix_segments.len()] == prefix_segments[..]
        })
    }
}

impl Default for SensitivePathPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// One per-path consent grant (host monotonic clock, opaque `u64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsConsent {
    /// When the grant was recorded (host monotonic ms).
    pub granted_at_ms: u64,
    /// When the grant expires (`None` = session-scoped).
    pub expires_at_ms: Option<u64>,
}

impl FsConsent {
    /// Whether this grant is active at `now_ms`.
    #[must_use]
    pub const fn is_active(&self, now_ms: u64) -> bool {
        match self.expires_at_ms {
            Some(expiry) => now_ms < expiry,
            None => true,
        }
    }
}

/// Normalize a request path to a canonical lowercase segment key.
///
/// Segments join on `|` with `\` and `|` backslash-escaped inside each
/// segment, so the key stays unambiguous even though both bytes are legal
/// POSIX file name bytes (`~/.npmrc|foo` and `~/.npmrc/foo` never share a
/// consent or deny key). [`key_segments`] is the exact inverse.
///
/// Returns `None` when the path is unrepresentable (empty, over-bound,
/// NUL/control-bearing, or normalizing to nothing). Both separators fold
/// to segments; `.`/empty segments collapse per the wave rule (leading
/// run preserved as relative markers so `./~/.config/gh/x` never equals
/// the `~/.config/gh` credential prefix); `..` segments are preserved
/// verbatim so traversal stays visible to the caller (scope checks reject
/// it). Absolute paths keep a leading empty root marker so they never
/// collide with `~`-rooted or relative keys.
fn normalize_request_path(path: &str) -> Option<String> {
    if path.is_empty() || path.len() > MAX_FS_PATH_BYTES || path.contains('\0') {
        return None;
    }
    if path.chars().any(|c| c.is_control()) {
        return None;
    }
    let segments = split_segments(path);
    let leading_noop = segments
        .iter()
        .take_while(|s| s.is_empty() || **s == ".")
        .count();
    let normalized = normalize_segments(&segments);
    if normalized.is_empty() {
        return None;
    }
    let mut key_parts: Vec<String> = Vec::with_capacity(normalized.len() + 1);
    if leading_noop > 0 {
        key_parts.push("rel".to_string());
        for segment in &segments[..leading_noop] {
            if segment.is_empty() {
                key_parts.push(String::new());
            } else {
                key_parts.push(".".to_string());
            }
        }
    } else if is_absolute_path(path) {
        key_parts.push(String::new());
    }
    for segment in normalized {
        key_parts.push(escape_key_segment(&segment.to_ascii_lowercase()));
    }
    Some(key_parts.join("|"))
}

/// Escape `\` and `|` inside one normalized key segment.
///
/// `|` separates key segments and `\` introduces an escape; both are legal
/// POSIX file name bytes, so a raw join would let `a|b` (one segment) and
/// `a/b` (two segments) produce the same key. Escaping keeps the joined
/// form injective over segment vectors.
fn escape_key_segment(segment: &str) -> String {
    let mut escaped = String::with_capacity(segment.len());
    for ch in segment.chars() {
        if ch == '\\' || ch == '|' {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Split a normalized key back into its segments (inverse of
/// [`escape_key_segment`] plus the `|` join).
///
/// Keys are produced only by [`normalize_request_path`], so escape bytes
/// always come in pairs; a malformed trailing `\` is kept as a literal so
/// the parse stays total and fail-closed.
fn key_segments(key: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in key.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            segments.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    segments.push(current);
    segments
}

/// Whether a normalized (`|`-joined lowercase) path is in the default
/// sensitive set.
///
/// Default-deny corpus (reviewable starting set, 045 open item):
/// `.env` / `.env.*` file names in any directory; `~/.ssh/**`;
/// `~/.gnupg/**`; `~/.aws/credentials`; `~/.aws/config` (credential-adjacent);
/// `~/.config/gh/**` and `~/.config/gcloud/**` (token stores, matching the
/// manifest credential prefixes); `~/.docker/config.json` (credential
/// store); `~/.kube/config` (cluster credentials); `~/.azure/**` (token
/// store); npm/pypi/cargo token stores (`~/.npmrc`, `~/.pypirc`,
/// `~/.cargo/credentials.toml`); shell-history-adjacent secrets are
/// deliberately excluded (history is not a credential store); browser
/// credential stores (Chromium `Login Data`/`Cookies`/`Local State`,
/// Firefox `logins.json`/`key4.db`/`cert9.db`, Safari `Keychains/**`).
fn is_default_sensitive(normalized: &str) -> bool {
    let lower = key_segments(normalized);
    // Strip the absolute-root marker for file-name checks: `.env` file
    // names are sensitive in any directory, relative or absolute.
    let file_name = lower.last().map(String::as_str).unwrap_or("");
    if file_name == ".env" || file_name.starts_with(".env.") {
        return true;
    }
    // `~`-rooted credential locations (case already folded, either
    // separator already split).
    if lower.first().is_some_and(|segment| segment == "~") {
        if lower.len() >= 2 {
            match lower[1].as_str() {
                ".ssh" | ".gnupg" | ".azure" => return true,
                ".aws" => {
                    // `~/.aws/credentials` and `~/.aws/config` carry
                    // secrets; other `~/.aws/**` children (e.g. `cli/`,
                    // `sso/`) are credential-adjacent and stay denied by
                    // the manifest wave already — deny the whole subtree
                    // here for the default-deny posture.
                    return true;
                }
                ".kube" => return true,
                ".docker" => {
                    if lower.len() >= 3 && lower[2] == "config.json" {
                        return true;
                    }
                    // Other docker children are config, not credential
                    // stores — leave them to the scope check.
                    if lower.len() == 2 {
                        return false;
                    }
                }
                ".npmrc" => return true,
                ".pypirc" => return true,
                ".netrc" => return true,
                ".config" => {
                    if lower.len() >= 3 {
                        match lower[2].as_str() {
                            "gh" | "gcloud" => return true,
                            "google-chrome" | "chromium" | "brave-browser" | "microsoft-edge"
                            | "firefox" => return true,
                            _ => {}
                        }
                    }
                }
                ".mozilla" => return true,
                ".cargo" => {
                    if lower.len() >= 3 && lower[2] == "credentials.toml" {
                        return true;
                    }
                }
                "library" => {
                    // macOS `~/Library/Keychains/**` and
                    // `~/Library/Application Support/<browser>/**`.
                    if lower.len() >= 3 && lower[2] == "keychains" {
                        return true;
                    }
                    if lower.len() >= 4
                        && lower[2] == "application support"
                        && matches!(
                            lower[3].as_str(),
                            "google-chrome"
                                | "chromium"
                                | "brave-browser"
                                | "microsoft-edge"
                                | "firefox"
                        )
                    {
                        return true;
                    }
                }
                "appdata" => {
                    // Windows `%APPDATA%`-adjacent `~/AppData/**` browser stores.
                    return true;
                }
                _ => {}
            }
        }
        // Token-store file names under `~` regardless of depth.
        if lower.iter().any(|s| {
            matches!(
                s.as_str(),
                ".npmrc" | ".pypirc" | ".netrc" | "credentials.toml" | "logins.json" | "key4.db"
            )
        }) {
            return true;
        }
    }
    // Browser credential file names in any directory (renamed-profile and
    // copied-store shapes): Chromium `Login Data` / `Cookies` / `Local
    // State` / `Web Data`; Firefox `logins.json` / `key4.db` /
    // `cert9.db`; Safari `keychain` names. `Cookies` alone is a common
    // word — only deny it adjacent to a browser directory marker.
    if matches!(
        file_name,
        "login data"
            | "login data-journal"
            | "local state"
            | "web data"
            | "logins.json"
            | "key4.db"
            | "cert9.db"
            | "key3.db"
    ) {
        return true;
    }
    if file_name == "cookies" && lower.iter().any(|s| is_browser_dir(s)) {
        return true;
    }
    if file_name.contains("keychain") {
        return true;
    }
    // `id_rsa`/`id_ed25519`/`.pem` private-key file names in any directory.
    if file_name == "id_rsa"
        || file_name == "id_ed25519"
        || file_name == "id_ecdsa"
        || file_name == "id_dsa"
        || file_name.ends_with(".pem")
        || file_name == ".pem"
    {
        return true;
    }
    false
}

/// Whether a normalized segment names a browser profile directory.
fn is_browser_dir(segment: &str) -> bool {
    matches!(
        segment,
        "google-chrome"
            | "chromium"
            | "brave-browser"
            | "microsoft-edge"
            | "firefox"
            | "safari"
            | "default"
            | "profile"
    )
}

// ── secret-shaped content detection (compose with CTX-0521) ───────────────

/// Whether read `content` looks like it carries secrets (fail-closed).
///
/// Composes with the CTX-0521 heuristics without duplicating them:
/// sensitive env/key names (`is_sensitive_env_name`) in `key=value` /
/// `key: value` / JSON `"key": "value"` shapes, or free-text secret-token
/// shapes (`looks_like_secret_token`: PEM blocks, known token prefixes,
/// JWT, bearer). Over-bound content (too many bytes/lines, over-long
/// lines) reports secret-shaped rather than passing bytes through —
/// fail-closed over-reject. `secret://` handle references are safe and
/// never count as secrets (consistent with the secrets scrubber).
///
/// Callers must route secret-shaped reads to [`FsDecision::Redacted`] (or
/// deny) and scrub bytes with the secrets redactor before any
/// agent-visible boundary.
#[must_use]
pub fn content_looks_secret(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }
    if content.len() > MAX_FS_CONTENT_SCAN_BYTES {
        return true;
    }
    let mut lines = 0usize;
    for line in content.lines() {
        lines += 1;
        if lines > MAX_FS_CONTENT_SCAN_LINES {
            return true;
        }
        if line.len() > MAX_FS_CONTENT_LINE_BYTES {
            return true;
        }
        if line_looks_secret(line) {
            return true;
        }
    }
    false
}

/// Whether one content line is secret-shaped.
fn line_looks_secret(line: &str) -> bool {
    if crate::secrets::SecretHandle::is_handle_ref(line.trim()) {
        return false;
    }
    // `key=value` / `key: value` / `"key": "value"` under a sensitive key
    // name: the value is secret-shaped regardless of its own shape
    // (fail-closed over-reject, mirroring `reject_literal_secret`).
    for sep in ["=", ":"] {
        if let Some(pos) = line.find(sep) {
            if sep == ":" && !colon_is_kv_separator(line, pos) {
                continue;
            }
            let (left, right) = line.split_at(pos);
            let key = left
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .rsplit([' ', '\t', '"', '\'', '{', ',', '('])
                .next()
                .unwrap_or("")
                .trim_matches('"')
                .trim_matches('\'')
                .trim_end_matches(':');
            if !key.is_empty() && crate::secrets::is_sensitive_env_name(key) {
                let value = right[sep.len()..].trim();
                if value.is_empty() {
                    continue;
                }
                let bare = value.trim_matches(['"', '\'']);
                if bare.is_empty() {
                    continue;
                }
                if crate::secrets::SecretHandle::is_handle_ref(bare) {
                    continue;
                }
                return true;
            }
        }
    }
    // Free-text token shapes (whitespace-delimited scan, handle refs safe).
    for token in line.split(|c: char| c.is_whitespace()) {
        let stripped = token.trim_matches(|c: char| {
            c == '"'
                || c == '\''
                || c == ','
                || c == ';'
                || c == '('
                || c == ')'
                || c == '['
                || c == ']'
                || c == '{'
                || c == '}'
                || c == '='
                || c == ':'
        });
        if stripped.is_empty() {
            continue;
        }
        if crate::secrets::SecretHandle::is_handle_ref(stripped) {
            continue;
        }
        if crate::secrets::looks_like_secret_token(stripped) {
            return true;
        }
    }
    false
}

/// Whether a `:` at `pos` is a key/value separator (not a URI scheme or clock).
///
/// Same rule as the secrets scrubber: the text after `:` must start with
/// whitespace or a quote (`key: value`, JSON `"k": "v"`); otherwise it is
/// likely a URI scheme (`secret://`), a clock time, or prose.
fn colon_is_kv_separator(line: &str, pos: usize) -> bool {
    let after = &line[pos + 1..];
    matches!(
        after.chars().next(),
        Some(c) if c.is_whitespace() || c == '"' || c == '\''
    )
}

// ── typed outcome ─────────────────────────────────────────────────────────

/// Why an FS request was refused.
///
/// Diagnostics quote the path only — never file values (CTX-0521 /
/// P0-AC-026 rule). `Display` output is safe to embed in logs, consent
/// prompts, traces, and audit entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsDenialKind {
    /// The path is outside the granted [`FilesystemScope`].
    OutsideScope,
    /// The path names a sensitive location without active consent.
    SensitivePath,
    /// The path pattern itself is hostile (absolute escape, traversal,
    /// overbroad home, credential-location grant shape).
    HostilePattern,
    /// The request itself is malformed (empty, over bounds, NUL/controls).
    InvalidRequest,
    /// Read content is secret-shaped and no redacted path was accepted.
    SecretContent,
}

impl FsDenialKind {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OutsideScope => "outside-scope",
            Self::SensitivePath => "sensitive-path",
            Self::HostilePattern => "hostile-pattern",
            Self::InvalidRequest => "invalid-request",
            Self::SecretContent => "secret-content",
        }
    }
}

impl fmt::Display for FsDenialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed FS authorization outcome.
///
/// `Allow` means the scope covers the path, the sensitive policy clears
/// it (not sensitive, or active per-path consent), and read content (when
/// supplied) is not secret-shaped. `Redacted` means the path is allowed
/// but the content is secret-shaped: the caller must serve the scrubbed
/// bytes, never the raw value. `ConsentRequired` is the explicit
/// user-consent path (never silent access). `Deny` is fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsDecision {
    /// The request is authorized as-is.
    Allow {
        /// Normalized path key (diagnostic, never a value).
        path: String,
    },
    /// The path is authorized but content is secret-shaped: serve
    /// redacted bytes only.
    Redacted {
        /// Normalized path key (diagnostic, never a value).
        path: String,
    },
    /// Sensitive location: explicit user consent is required first.
    ConsentRequired {
        /// Normalized path key (diagnostic, never a value).
        path: String,
    },
    /// The request is refused (typed reason, path only).
    Deny {
        /// Why the request was refused.
        kind: FsDenialKind,
        /// Normalized path key, or the raw length summary when the path
        /// itself is unrepresentable (never a value).
        path: String,
    },
}

impl FsDecision {
    /// Whether the decision authorizes any access (allow or redacted).
    #[must_use]
    pub const fn is_authorized(&self) -> bool {
        matches!(self, Self::Allow { .. } | Self::Redacted { .. })
    }

    /// Normalized path this decision is about (diagnostic only).
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Allow { path }
            | Self::Redacted { path }
            | Self::ConsentRequired { path }
            | Self::Deny { path, .. } => path,
        }
    }
}

impl fmt::Display for FsDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow { path } => write!(f, "fs allow '{path}'"),
            Self::Redacted { path } => {
                write!(f, "fs allow redacted '{path}' (secret-shaped content)")
            }
            Self::ConsentRequired { path } => {
                write!(f, "fs consent required for '{path}'")
            }
            Self::Deny { kind, path } => write!(f, "fs denied ({kind}) '{path}'"),
        }
    }
}

/// Owned, headless-testable error for FS authorization misuse.
///
/// Values never appear here: only paths (bounded) and reasons. `Display`
/// output is log-safe by construction (P0-AC-026).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// The path is outside the granted scope.
    OutsideScope {
        /// Path (bounded, never a value).
        path: String,
    },
    /// The path names a sensitive location.
    SensitivePath {
        /// Path (bounded, never a value).
        path: String,
    },
    /// A grant pattern is hostile (absolute escape, traversal, overbroad
    /// home, credential-location grant shape).
    HostilePattern {
        /// Pattern (bounded, never a value).
        pattern: String,
    },
    /// Malformed request (bad shape, over bounds).
    InvalidRequest {
        /// Bounded reason (names only, never values).
        reason: String,
    },
    /// A hard limit was exceeded.
    LimitExceeded {
        /// Field or resource.
        field: String,
        /// Configured limit.
        limit: usize,
        /// Actual value.
        actual: usize,
    },
}

impl FsError {
    /// Outside-scope error (quotes the path only).
    #[must_use]
    pub fn outside_scope(path: impl Into<String>) -> Self {
        Self::OutsideScope {
            path: bounded_path(path.into()),
        }
    }

    /// Sensitive-path error (quotes the path only).
    #[must_use]
    pub fn sensitive_path(path: impl Into<String>) -> Self {
        Self::SensitivePath {
            path: bounded_path(path.into()),
        }
    }

    /// Hostile grant-pattern error (quotes the pattern only).
    #[must_use]
    pub fn hostile_pattern(pattern: impl Into<String>) -> Self {
        Self::HostilePattern {
            pattern: bounded_path(pattern.into()),
        }
    }

    /// Malformed-request error (bounded reason, names only).
    #[must_use]
    pub fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: bounded_path(reason.into()),
        }
    }

    /// Limit error.
    #[must_use]
    pub fn limit_exceeded(field: impl Into<String>, limit: usize, actual: usize) -> Self {
        Self::LimitExceeded {
            field: bounded_path(field.into()),
            limit,
            actual,
        }
    }

    /// Stable denial kind for audit attribution.
    #[must_use]
    pub const fn denial_kind(&self) -> FsDenialKind {
        match self {
            Self::OutsideScope { .. } => FsDenialKind::OutsideScope,
            Self::SensitivePath { .. } => FsDenialKind::SensitivePath,
            Self::HostilePattern { .. } => FsDenialKind::HostilePattern,
            Self::InvalidRequest { .. } | Self::LimitExceeded { .. } => {
                FsDenialKind::InvalidRequest
            }
        }
    }
}

/// Clamp a path/reason to a bounded length (fail-closed at the call site).
fn bounded_path(path: String) -> String {
    if path.len() > MAX_FS_PATH_BYTES {
        format!("<{} bytes>", path.len())
    } else {
        path
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideScope { path } => write!(f, "path '{path}' is outside the granted scope"),
            Self::SensitivePath { path } => {
                write!(f, "sensitive path '{path}': explicit user consent required")
            }
            Self::HostilePattern { pattern } => {
                write!(f, "hostile fs pattern '{pattern}'")
            }
            Self::InvalidRequest { reason } => write!(f, "invalid fs request: {reason}"),
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(f, "{field}: limit {limit} exceeded (actual {actual})"),
        }
    }
}

impl std::error::Error for FsError {}

impl From<FsError> for crate::error::PluginError {
    fn from(error: FsError) -> Self {
        crate::error::PluginError::registry(error.to_string())
    }
}

// ── authorization entry point ─────────────────────────────────────────────

/// Authorize one FS request path.
///
/// Fail-closed evaluation order (every refusal is typed, path-only):
///
/// 1. Unrepresentable paths (empty, over-bound, NUL/controls) deny as
///    [`FsDenialKind::InvalidRequest`].
/// 2. Hostile patterns (per [`is_hostile_fs_pattern`]: absolute escape,
///    `..` traversal, overbroad home, credential-location grant shapes)
///    deny as [`FsDenialKind::HostilePattern`] — the wave predicates run
///    first so grant-shape attacks never reach scope matching.
/// 3. Paths outside [`FilesystemScope`] deny as
///    [`FsDenialKind::OutsideScope`] (deny-by-default, Lua cannot widen).
/// 4. Sensitive paths (per [`SensitivePathPolicy`]) without active
///    per-path consent yield [`FsDecision::ConsentRequired`] — the
///    explicit user-consent path, never silent access.
/// 5. When `content` is supplied and [`content_looks_secret`] holds, the
///    decision is [`FsDecision::Redacted`]: serve scrubbed bytes only.
/// 6. Otherwise [`FsDecision::Allow`].
///
/// `is_write` is recorded for audit attribution only: reads and writes
/// share the same sensitive-path boundary (a credential must not be
/// overwritten either). `now_ms` is the host monotonic clock for consent
/// expiry.
#[must_use]
pub fn authorize_fs(
    scope: &FilesystemScope,
    policy: &SensitivePathPolicy,
    path: &str,
    is_write: bool,
    content: Option<&str>,
    now_ms: u64,
) -> FsAuthorized {
    let _ = is_write;
    if path.is_empty() || path.len() > MAX_FS_PATH_BYTES || path.contains('\0') {
        return FsAuthorized {
            decision: FsDecision::Deny {
                kind: FsDenialKind::InvalidRequest,
                path: bounded_path(path.to_string()),
            },
            sensitive: true,
            secret_shaped: false,
        };
    }
    if path.chars().any(|c| c.is_control()) {
        return FsAuthorized {
            decision: FsDecision::Deny {
                kind: FsDenialKind::InvalidRequest,
                path: bounded_path(path.to_string()),
            },
            sensitive: true,
            secret_shaped: false,
        };
    }
    if is_hostile_fs_pattern(path) {
        return FsAuthorized {
            decision: FsDecision::Deny {
                kind: FsDenialKind::HostilePattern,
                path: bounded_path(path.to_string()),
            },
            sensitive: true,
            secret_shaped: false,
        };
    }
    if !scope.allows(path) {
        return FsAuthorized {
            decision: FsDecision::Deny {
                kind: FsDenialKind::OutsideScope,
                path: bounded_path(path.to_string()),
            },
            sensitive: policy.is_sensitive(path),
            secret_shaped: false,
        };
    }
    let sensitive = policy.is_sensitive(path);
    if sensitive {
        let Some(normalized) = normalize_request_path(path) else {
            return FsAuthorized {
                decision: FsDecision::Deny {
                    kind: FsDenialKind::InvalidRequest,
                    path: bounded_path(path.to_string()),
                },
                sensitive: true,
                secret_shaped: false,
            };
        };
        if !policy.consent_active(&normalized, now_ms) {
            return FsAuthorized {
                decision: FsDecision::ConsentRequired { path: normalized },
                sensitive: true,
                secret_shaped: false,
            };
        }
        // Active consent clears the sensitive gate; content still scans.
        let secret_shaped = content.is_some_and(content_looks_secret);
        let label = normalize_request_path(path).unwrap_or_else(|| path.to_string());
        if secret_shaped {
            return FsAuthorized {
                decision: FsDecision::Redacted { path: label },
                sensitive: true,
                secret_shaped: true,
            };
        }
        return FsAuthorized {
            decision: FsDecision::Allow { path: label },
            sensitive: true,
            secret_shaped: false,
        };
    }
    let secret_shaped = content.is_some_and(content_looks_secret);
    let label = normalize_request_path(path).unwrap_or_else(|| path.to_string());
    if secret_shaped {
        return FsAuthorized {
            decision: FsDecision::Redacted { path: label },
            sensitive: false,
            secret_shaped: true,
        };
    }
    FsAuthorized {
        decision: FsDecision::Allow { path: label },
        sensitive: false,
        secret_shaped: false,
    }
}

/// Evaluated FS authorization: the typed decision plus the two layer flags.
///
/// `sensitive` records whether the sensitive-path policy fired;
/// `secret_shaped` records whether content detection fired. Both are
/// audit attribution; neither carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsAuthorized {
    /// Typed outcome (path only, never values).
    pub decision: FsDecision,
    /// Whether the sensitive-path layer classified this path.
    pub sensitive: bool,
    /// Whether content detection classified the read bytes.
    pub secret_shaped: bool,
}

impl FsAuthorized {
    /// Whether the decision authorizes any access (allow or redacted).
    #[must_use]
    pub const fn is_authorized(&self) -> bool {
        matches!(
            self.decision,
            FsDecision::Allow { .. } | FsDecision::Redacted { .. }
        )
    }
}

// ── audit ledger ──────────────────────────────────────────────────────────

/// Allow/deny outcome recorded in the [`FsAuditLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsAuditDecision {
    /// The request was allowed (as-is or redacted).
    Allow,
    /// The request was denied (typed denial).
    Deny,
    /// Sensitive location: consent is required first.
    ConsentRequired,
    /// Consent was granted or revoked.
    Consent,
}

impl FsAuditDecision {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::ConsentRequired => "consent-required",
            Self::Consent => "consent",
        }
    }
}

impl fmt::Display for FsAuditDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One FS-audit entry: what was asked, what was decided, and why.
///
/// Paths only — never file values. Bounded and sequence-numbered (no
/// wall-clock), mirroring the effective-capability and secret audit
/// ledgers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsAuditEntry {
    /// Monotonic sequence (no wall-clock).
    pub seq: u64,
    /// Whether this was a read or write request.
    pub is_write: bool,
    /// Outcome.
    pub decision: FsAuditDecision,
    /// Paths involved (capped at [`MAX_FS_AUDIT_ITEMS`]).
    pub paths: Vec<String>,
    /// Denial kind on deny (`None` otherwise).
    pub denial: Option<FsDenialKind>,
    /// Bounded detail (paths only, never values).
    pub detail: String,
}

/// Bounded append-only ledger of FS authorization decisions.
///
/// Drop-oldest when full (accepted v1 default, mirroring the event
/// pipeline and the secret audit ledger); `dropped` counts evicted
/// entries for `bitty plugin doctor`.
#[derive(Debug, Clone, Default)]
pub struct FsAuditLedger {
    entries: VecDeque<FsAuditEntry>,
    dropped: u64,
    next_seq: u64,
}

impl FsAuditLedger {
    /// Empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an evaluated authorization (paths only).
    ///
    /// CodeQL models any call to these `push_*` entry points as
    /// log-tainted on the grounds that the ledger can hold file bytes;
    /// the payload is paths-only by construction (see [`Self::push`] and
    /// the `*_names_never_values` tests), which is the documented barrier
    /// the analyzer cannot see. Treat new `push_*` call sites as
    /// log-tainted until they carry the same names-only proof.
    ///
    /// # Logging
    ///
    /// Log-tainted: arguments must be paths, never file values.
    pub fn push_decision(&mut self, evaluated: &FsAuthorized, is_write: bool) {
        let (decision, denial) = match &evaluated.decision {
            FsDecision::Allow { .. } => (FsAuditDecision::Allow, None),
            FsDecision::Redacted { .. } => (FsAuditDecision::Allow, None),
            FsDecision::ConsentRequired { .. } => (FsAuditDecision::ConsentRequired, None),
            FsDecision::Deny { kind, .. } => (FsAuditDecision::Deny, Some(*kind)),
        };
        let detail = evaluated.decision.to_string();
        self.push(
            decision,
            is_write,
            std::slice::from_ref(&evaluated.decision.path().to_string()),
            denial,
            detail,
        );
    }

    /// Record a consent grant or revocation (paths only).
    ///
    /// # Logging
    ///
    /// Log-tainted: arguments must be paths, never file values.
    pub fn push_consent(&mut self, paths: &[String], detail: impl Into<String>) {
        self.push(FsAuditDecision::Consent, false, paths, None, detail.into());
    }

    /// Bounded append shared by all outcomes.
    fn push(
        &mut self,
        decision: FsAuditDecision,
        is_write: bool,
        paths: &[String],
        denial: Option<FsDenialKind>,
        detail: String,
    ) {
        if self.entries.len() >= MAX_FS_AUDIT_ENTRIES {
            self.entries.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.entries.push_back(FsAuditEntry {
            seq,
            is_write,
            decision,
            paths: paths.iter().take(MAX_FS_AUDIT_ITEMS).cloned().collect(),
            denial,
            detail: bounded_path(detail),
        });
    }

    /// Retained entries, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &FsAuditEntry> + '_ {
        self.entries.iter()
    }

    /// Number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries evicted by the bound so far.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_A: &str = "ghp_seededFsFixtureAAAA1111";
    const SEED_B: &str = "sk-live-seededFsFixtureBBBB2222";
    const SEED_C: &str = "AKIASEEDEDFSDTFIXTURE3333";

    fn scope(patterns: &[&str]) -> FilesystemScope {
        FilesystemScope::from_patterns(
            &patterns
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        )
        .expect("test scope must validate")
    }

    fn test_policy() -> SensitivePathPolicy {
        SensitivePathPolicy::default_policy()
    }

    #[test]
    fn seeded_sensitive_path_corpus_denied() {
        // Task corpus: `.env` / `.env.*`, `~/.ssh/**`, `~/.gnupg/**`,
        // `~/.aws/credentials`, token stores, browser credential stores.
        //
        // Sensitive corpus paths need no scope grant to prove the denial:
        // use a broad legit scope plus direct policy assertions, since
        // hostile-shaped grants (e.g. `~/.ssh/**`) correctly fail closed
        // at scope construction (wave predicate, never duplicated here).
        let scope = scope(&["~/projects/**", "~/.config/**"]);
        let policy = test_policy();
        for path in [
            "~/.ssh/id_rsa",
            "~/.ssh",
            "~/.gnupg/pubring.kbx",
            "~/.aws/credentials",
            "~/.config/gh/hosts.yml",
            "~/.config/gcloud/credentials.db",
            "~/.docker/config.json",
            "~/.kube/config",
            "~/.npmrc",
            "~/.cargo/credentials.toml",
        ] {
            assert!(
                policy.is_sensitive(path),
                "sensitive path {path:?} must classify sensitive"
            );
        }
        for path in [
            "~/projects/.env",
            "~/projects/.env.local",
            "~/projects/.env.production",
            "~/.config/gh/hosts.yml",
            "~/.config/gcloud/credentials.db",
        ] {
            let evaluated = authorize_fs(&scope, &policy, path, false, None, 0);
            assert!(
                !evaluated.is_authorized(),
                "sensitive path {path:?} must not authorize"
            );
            assert!(
                matches!(
                    evaluated.decision,
                    FsDecision::ConsentRequired { .. } | FsDecision::Deny { .. }
                ),
                "sensitive path {path:?} must deny or require consent"
            );
        }
        // Scope-hostile sensitive grants fail closed at construction.
        assert!(FilesystemScope::from_patterns(&["~/.ssh/**".to_string()]).is_err());
    }

    #[test]
    fn dot_empty_segment_and_case_variants_stay_denied() {
        // 0489/0495 wave parity at request time: `.`/empty spellings and
        // case/backslash variants collapse onto the same denials. Sensitive
        // corpus paths assert via the policy directly (their grants are
        // hostile-shaped); in-scope `.env` variants assert via authorize.
        let scope = scope(&["~/projects/**", "~/.config/**"]);
        let policy = test_policy();
        for path in [
            "~/.SSH/id_rsa",
            "~\\.ssh\\id_rsa",
            "~/./.ssh/id_rsa",
            "~//.aws/credentials",
            "~/.Aws/Credentials",
            "~/.config/./gh/hosts.yml",
            "~/.config//gcloud/credentials.db",
            "~\\.CONFIG\\gh\\hosts.yml",
        ] {
            assert!(
                policy.is_sensitive(path),
                "variant path {path:?} must classify sensitive"
            );
        }
        for path in [
            "~/projects/./.env",
            "~//projects//.env.local",
            "~/PROJECTS/.ENV",
            "~/.config/./gh/hosts.yml",
            "~/.config//gcloud/credentials.db",
        ] {
            let evaluated = authorize_fs(&scope, &policy, path, false, None, 0);
            assert!(
                !evaluated.is_authorized(),
                "variant path {path:?} must not authorize"
            );
        }
    }

    #[test]
    fn hostile_grant_shapes_deny_before_scope() {
        let scope = scope(&["~/projects/**"]);
        let policy = test_policy();
        for path in [
            "/etc/passwd",
            "../secret",
            "~/../etc/passwd",
            "C:/Windows/System32/drivers/etc/hosts",
            "\\\\server\\share\\secret",
            "~/projects/../../etc/passwd",
        ] {
            let evaluated = authorize_fs(&scope, &policy, path, false, None, 0);
            assert!(
                matches!(
                    evaluated.decision,
                    FsDecision::Deny {
                        kind: FsDenialKind::HostilePattern | FsDenialKind::InvalidRequest,
                        ..
                    }
                ),
                "hostile path {path:?} must deny as hostile/invalid"
            );
        }
    }

    #[test]
    fn outside_scope_denies_by_default() {
        let scope = scope(&["~/projects/**"]);
        let policy = test_policy();
        let evaluated = authorize_fs(&scope, &policy, "~/mail/inbox.md", false, None, 0);
        assert!(matches!(
            evaluated.decision,
            FsDecision::Deny {
                kind: FsDenialKind::OutsideScope,
                ..
            }
        ));
        // Empty scope allows nothing, even for innocuous paths.
        let empty = FilesystemScope::empty();
        let evaluated = authorize_fs(&empty, &policy, "~/projects/notes.md", false, None, 0);
        assert!(!evaluated.is_authorized());
    }

    #[test]
    fn legit_paths_allow() {
        let scope = scope(&["~/projects/**", "~/mail/**"]);
        let policy = test_policy();
        for path in [
            "~/projects/notes.md",
            "~/projects/src/main.rs",
            "~/mail/inbox.md",
        ] {
            let evaluated = authorize_fs(&scope, &policy, path, false, None, 0);
            assert_eq!(
                evaluated.decision,
                FsDecision::Allow {
                    path: normalize_request_path(path).unwrap(),
                },
                "legit path {path:?} must allow"
            );
        }
        // Writes share the same boundary (no silent overwrite channel).
        let evaluated = authorize_fs(&scope, &policy, "~/projects/notes.md", true, None, 0);
        assert!(evaluated.is_authorized());
    }

    #[test]
    fn content_detection_over_rejects_renamed_secrets() {
        // Renamed secrets (`secret.txt`, `credentials.json`,
        // `prod-config.yaml`) are caught by content, not by name.
        let scope = scope(&["~/projects/**"]);
        let policy = test_policy();
        assert!(!policy.is_sensitive("~/projects/secret.txt"));
        for content in [
            format!("token={SEED_A}"),
            format!("{{\"api_key\": \"{SEED_B}\"}}"),
            format!("aws_access_key_id = {SEED_C}"),
            "password: hunter2-hunter2".to_string(),
            "-----BEGIN PRIVATE KEY-----\nabc".to_string(),
            format!("Authorization: Bearer {SEED_B}"),
        ] {
            assert!(
                content_looks_secret(&content),
                "secret-shaped content must flag: {content:?}"
            );
            let evaluated = authorize_fs(
                &scope,
                &policy,
                "~/projects/secret.txt",
                false,
                Some(&content),
                0,
            );
            assert_eq!(
                evaluated.decision,
                FsDecision::Redacted {
                    path: normalize_request_path("~/projects/secret.txt").unwrap(),
                },
                "secret-shaped read must redact, never pass raw"
            );
        }
    }

    #[test]
    fn content_detection_passes_legit_text() {
        for content in [
            "hello world",
            "# notes\n- buy milk\n- write tests\n",
            "task-name cleanup",
            "mask-service --port 8080",
            "path=/tmp/x",
        ] {
            assert!(
                !content_looks_secret(content),
                "legit text must pass: {content:?}"
            );
        }
        assert!(!content_looks_secret(""));
        // `secret://` handle references are safe (CTX-0521 convention).
        assert!(!content_looks_secret("credential = secret://github"));
        assert!(!content_looks_secret("secret://github"));
    }

    #[test]
    fn over_bound_content_fails_closed() {
        let big = "x".repeat(MAX_FS_CONTENT_SCAN_BYTES + 1);
        assert!(content_looks_secret(&big));
        let many_lines = (0..MAX_FS_CONTENT_SCAN_LINES + 2)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content_looks_secret(&many_lines));
        let long_line = "y".repeat(MAX_FS_CONTENT_LINE_BYTES + 1);
        assert!(content_looks_secret(&long_line));
    }

    #[test]
    fn consent_path_clears_exactly_one_path() {
        let scope = scope(&["~/projects/**"]);
        let mut policy = test_policy();
        let before = authorize_fs(&scope, &policy, "~/projects/.env", false, None, 0);
        assert!(matches!(
            before.decision,
            FsDecision::ConsentRequired { .. }
        ));
        policy.grant_consent("~/projects/.env", 0, None).unwrap();
        let after = authorize_fs(&scope, &policy, "~/projects/.env", false, None, 0);
        assert!(after.is_authorized());
        // Sibling paths stay gated: consent never widens.
        let sibling = authorize_fs(&scope, &policy, "~/projects/.env.local", false, None, 0);
        assert!(!sibling.is_authorized());
        // Expired consent denies again.
        policy
            .grant_consent("~/projects/.env.local", 0, Some(10))
            .unwrap();
        let expired = authorize_fs(&scope, &policy, "~/projects/.env.local", false, None, 20);
        assert!(!expired.is_authorized());
        // Revocation re-gates.
        policy.revoke_consent("~/projects/.env");
        let revoked = authorize_fs(&scope, &policy, "~/projects/.env", false, None, 0);
        assert!(!revoked.is_authorized());
    }

    #[test]
    fn consent_keys_never_collide_on_pipe_separator_bytes() {
        // POSIX `|` is a legal file name byte. The normalized key must
        // keep `~/.npmrc|foo` (one file segment) and `~/.npmrc/foo` (a
        // nested path) distinct: consent for one never clears the other.
        let pipe_key = normalize_request_path("~/.npmrc|foo").unwrap();
        let nested_key = normalize_request_path("~/.npmrc/foo").unwrap();
        assert_ne!(pipe_key, nested_key);
        assert_eq!(
            key_segments(&pipe_key),
            vec!["~".to_string(), ".npmrc|foo".to_string()]
        );
        assert_eq!(
            key_segments(&nested_key),
            vec!["~".to_string(), ".npmrc".to_string(), "foo".to_string()]
        );

        // Granting consent for the pipe-named sibling never clears the
        // sensitive nested path...
        let mut policy = test_policy();
        policy.grant_consent("~/.npmrc|foo", 0, None).unwrap();
        assert!(policy.consent_active(&pipe_key, 0));
        assert!(!policy.consent_active(&nested_key, 0));
        // ...and the reverse direction holds too.
        let mut reverse = test_policy();
        reverse.grant_consent("~/.npmrc/foo", 0, None).unwrap();
        assert!(reverse.consent_active(&nested_key, 0));
        assert!(!reverse.consent_active(&pipe_key, 0));

        // End-to-end through `authorize_fs`: the nested path stays gated
        // after consent for the pipe-named sibling, and normal nested
        // consent still clears exactly its own path.
        let scope = scope(&["~/.npmrc/**"]);
        let mut authorized = test_policy();
        authorized.grant_consent("~/.npmrc|foo", 0, None).unwrap();
        let nested = authorize_fs(&scope, &authorized, "~/.npmrc/foo", false, None, 0);
        assert!(matches!(
            nested.decision,
            FsDecision::ConsentRequired { .. }
        ));
        authorized.grant_consent("~/.npmrc/foo", 0, None).unwrap();
        let cleared = authorize_fs(&scope, &authorized, "~/.npmrc/foo", false, None, 0);
        assert!(cleared.is_authorized());
        let sibling = authorize_fs(&scope, &authorized, "~/.npmrc/bar", false, None, 0);
        assert!(matches!(
            sibling.decision,
            FsDecision::ConsentRequired { .. }
        ));
    }

    #[test]
    fn diagnostics_carry_names_only_never_values() {
        let scope = scope(&["~/projects/**"]);
        let policy = test_policy();
        let content = format!("token={SEED_A}");
        let evaluated = authorize_fs(
            &scope,
            &policy,
            "~/projects/secret.txt",
            false,
            Some(&content),
            0,
        );
        let flat = format!(
            "{} {} {:?}",
            evaluated.decision,
            evaluated.decision.path(),
            evaluated.decision
        );
        assert!(!flat.contains(SEED_A));
        assert!(!flat.contains(SEED_B));
        assert!(!flat.contains(SEED_C));
        let denial = authorize_fs(&scope, &policy, "~/mail/x.md", false, None, 0);
        let flat = format!("{} {:?}", denial.decision, denial.decision);
        assert!(!flat.contains(SEED_A));
        for error in [
            FsError::outside_scope("~/mail/x.md"),
            FsError::sensitive_path("~/.ssh/id_rsa"),
            FsError::invalid_request("bad shape"),
        ] {
            let flat = format!("{error:?} {error}");
            assert!(!flat.contains(SEED_A));
            assert!(!flat.contains(SEED_B));
        }
        // Audit entries carry paths only.
        let mut ledger = FsAuditLedger::new();
        ledger.push_decision(&evaluated, false);
        ledger.push_decision(&denial, false);
        for entry in ledger.iter() {
            let flat = format!(
                "{} {} {:?}",
                entry.detail,
                entry.paths.join(","),
                entry.denial
            );
            assert!(!flat.contains(SEED_A));
            assert!(!flat.contains(SEED_B));
            assert!(!flat.contains(SEED_C));
        }
        // Debug snapshots never emit values either.
        assert!(!format!("{scope:?}").contains(SEED_A));
        assert!(!format!("{:?}", test_policy()).contains(SEED_A));
    }

    #[test]
    fn audit_records_every_decision() {
        let mut ledger = FsAuditLedger::new();
        let scope = scope(&["~/projects/**"]);
        let policy = test_policy();
        let allow = authorize_fs(&scope, &policy, "~/projects/a.md", false, None, 0);
        let deny = authorize_fs(&scope, &policy, "~/mail/a.md", false, None, 0);
        ledger.push_decision(&allow, false);
        ledger.push_decision(&deny, true);
        assert_eq!(ledger.len(), 2);
        let entries: Vec<_> = ledger.iter().collect();
        assert_eq!(entries[0].decision, FsAuditDecision::Allow);
        assert!(!entries[0].is_write);
        assert_eq!(entries[1].decision, FsAuditDecision::Deny);
        assert!(entries[1].is_write);
        assert_eq!(entries[1].denial, Some(FsDenialKind::OutsideScope));
        for pair in entries.windows(2) {
            assert!(pair[0].seq < pair[1].seq);
        }
        // Consent grants audit too.
        ledger.push_consent(
            &["~/projects/.env".to_string()],
            "consent granted for '~/projects/.env'",
        );
        assert_eq!(ledger.len(), 3);
        // Ledger is bounded drop-oldest.
        let mut bounded = FsAuditLedger::new();
        for _ in 0..MAX_FS_AUDIT_ENTRIES + 10 {
            bounded.push_decision(&allow, false);
        }
        assert_eq!(bounded.len(), MAX_FS_AUDIT_ENTRIES);
        assert_eq!(bounded.dropped(), 10);
    }

    #[test]
    fn no_bypass_through_case_separator_or_dot_spellings() {
        // The waves' bypass classes stay closed at request time: folded
        // case, backslash separators, and `.`/empty spellings of a
        // sensitive path never authorize without consent.
        let scope = scope(&["~/projects/**", "~/.config/**"]);
        let policy = test_policy();
        for path in [
            "~/.SSH/ID_RSA",
            "~\\.SSH\\ID_RSA",
            "~/./.ssh/id_rsa",
            "~//.ssh//id_rsa",
            "~/.AWS/credentials",
            "~\\.aws\\credentials",
        ] {
            assert!(
                policy.is_sensitive(path),
                "bypass spelling {path:?} must classify sensitive"
            );
        }
        for path in [
            "~/.config/./gh/hosts.yml",
            "~/.config//gcloud/credentials.db",
        ] {
            let evaluated = authorize_fs(&scope, &policy, path, false, None, 0);
            assert!(
                !evaluated.is_authorized(),
                "bypass spelling {path:?} must not authorize"
            );
        }
        // Relative `./~/.config/gh/x` names a literal directory, not the
        // credential prefix — scope decides, policy stays quiet.
        assert!(!policy.is_sensitive("./~/.config/gh/x"));
    }

    #[test]
    fn hostile_scope_patterns_fail_closed_at_construction() {
        for hostile in [
            "/etc/passwd",
            "../evil",
            "~",
            "~/",
            "~/**",
            "~/.ssh/id_rsa",
            "**/.ssh/**",
            "C:/Windows/System32/**",
        ] {
            assert!(
                FilesystemScope::from_patterns(&[hostile.to_string()]).is_err(),
                "hostile scope pattern {hostile:?} must fail closed"
            );
        }
        assert!(FilesystemScope::from_patterns(&[]).is_ok());
        assert!(FilesystemScope::empty().allows("~/projects/x").not());
    }

    #[test]
    fn backslash_and_case_scope_matching() {
        // Windows CI parity: grants match on either separator,
        // case-insensitively per segment.
        let scope = scope(&["~/projects/**"]);
        assert!(scope.allows("~\\projects\\notes.md"));
        assert!(scope.allows("~/PROJECTS/notes.md"));
        assert!(scope.allows("~\\PROJECTS\\Sub\\File.MD"));
        assert!(!scope.allows("~\\mail\\inbox.md"));
    }

    #[test]
    fn trailing_star_star_covers_subtree_root() {
        let scope = scope(&["~/projects/**"]);
        assert!(scope.allows("~/projects"));
        assert!(scope.allows("~/projects/"));
        assert!(scope.allows("~/projects/a/b/c.md"));
    }

    #[allow(clippy::nonminimal_bool)]
    trait Not {
        fn not(self) -> bool;
    }

    #[allow(clippy::nonminimal_bool)]
    impl Not for bool {
        fn not(self) -> bool {
            !self
        }
    }
}
