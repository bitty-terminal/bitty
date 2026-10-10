//! Minimal component-install seed (issue #1791, option A).
//!
//! Like cargo ships with rustc, bitty ships a minimal install seed so the
//! full `bitty-plugin-manager` itself can be installed on first use: `bitty
//! component install <name> --version <semver>` fetches the prebuilt R2
//! distribution packed by `scripts/make-component-dist.sh` (issue #1792),
//! verifies it fail-closed, and stages it into the user tier through the
//! same atomic publish path as `component add`. The full manager takes over
//! after: the seed does nothing else (no registry index, no version solving,
//! no UI).
//!
//! # Contract (implemented, seed only)
//!
//! - Shape: `bitty component install <name> --version <X.Y.Z>`. The version
//!   is required and exact: components publish no `latest` pointer, and
//!   version solving arrives with the full manager, not the seed.
//! - Fetch: fixed-argv system `curl` (pinned to `https://cdn.bitty.run`,
//!   `--proto =https`, no shell interpolation) plus system `tar` for
//!   extraction plus Rust-side SHA-256 verification. No Rust network stack
//!   in bitty (DIR-030 D6 / DIR-016 / DIR-017 stay intact).
//! - Layout: `bitty/components/<name>/<version>/<target>.tar.gz` plus the
//!   aggregate `SHA256SUMS` manifest (the authoritative checksum record per
//!   `packaging/README.md`); the tarball holds exactly `bitty-component.toml`
//!   plus `bitty-<name>` at the archive root.
//! - Trust: unsigned 0.1.0 verifies by hash only (Windows ships unsigned per
//!   #1810, so hash-only is the uniform story). Sigstore/cosign stays a
//!   follow-up, not a silent addition.
//! - Environment: `curl` 7.68.0 or newer (preflighted once per invocation;
//!   absent or too-old curl fails closed with a manual-placement
//!   diagnostic, exit `1`). `curl`/`tar` inherit the caller's environment
//!   for proxy and TLS store discovery (mirroring `scripts/install.sh`),
//!   minus secret-shaped variables (`*_TOKEN`/`*_KEY`/`*_SECRET`, which are
//!   scrubbed from every child). Argv is fixed and audited; system CA store
//!   only (no `--cacert` override), system proxy env honored, no custom TLS
//!   flags, and no secret material crosses the boundary.
//! - Class: local-only (no instance, no IPC, no component code is ever
//!   loaded or executed; safe-mode clean).
//!
//! # Exit-code mapping (stable taxonomy, cli-contract-rfc.md)
//!
//! [`SeedError::exit_code`] maps onto the component command taxonomy: `2`
//! for CLI-derived input failures (bad name/version), `1` for environmental
//! failures (unsupported host, missing or too-old `curl`, missing `tar`,
//! download or filesystem I/O), `4` for artifact-integrity failures
//! (manifest, digest, member, or descriptor mismatch).
//!
//! # Bounds (fail closed before any filesystem mutation of the user tier)
//!
//! - Name: `[a-z][a-z0-9-]{0,31}` ([`validate_component_name`]).
//! - Version: strict `X.Y.Z` numeric core (pack policy, narrower than the
//!   installed parser's full semver, mirroring `make-component-dist.sh`).
//! - Target: `[a-z0-9_]+(-[a-z0-9_]+)+`, at most [`SEED_TARGET_MAX_BYTES`]
//!   bytes; only the detected host triple is ever requested (there is no
//!   `--target` flag, so foreign-target confusion is unrepresentable).
//! - URLs: at most [`SEED_URL_MAX_BYTES`] bytes, `https` on
//!   [`SEED_CDN_HOST`] only, audited after construction.
//! - Manifest: at most [`SEED_MANIFEST_MAX_BYTES`] bytes via curl
//!   `--max-filesize`.
//! - Tarball: at most [`SEED_TARBALL_MAX_BYTES`] bytes via curl
//!   `--max-filesize` (executable ceiling plus descriptor/tar/gzip slack).
//! - Extracted members: exactly the two expected files, sizes bounded,
//!   never symlinks, never outside the fresh staging directory.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use bitty_runtime::component::{
    COMPONENT_DESCRIPTOR_FILE, COMPONENT_DESCRIPTOR_MAX_BYTES, COMPONENT_EXECUTABLE_MAX_BYTES,
    COMPONENT_EXECUTABLE_PREFIX, ComponentDescriptor, validate_component_name,
};

/// CDN host the seed ever contacts (allowlist, single entry).
pub const SEED_CDN_HOST: &str = "cdn.bitty.run";

/// CDN base URL the seed ever contacts (derived from [`SEED_CDN_HOST`]).
pub const SEED_CDN_BASE: &str = "https://cdn.bitty.run";

/// CDN port the seed ever contacts (HTTPS; never appears in the URL, which
/// relies on the `https` default, but pins the egress `host:port`).
pub const SEED_CDN_PORT: u16 = 443;

/// Core-built-in installer egress host (first-party CDN, issue #1905).
///
/// Alias of [`SEED_CDN_HOST`]: strict configurations allow this host without
/// a per-plugin `[[network.egress]]` declaration or per-source consent.
/// Every other host stays consent-gated (fail closed without explicit
/// consent). Must stay equal to
/// `bitty_runtime::component::BUILTIN_INSTALLER_EGRESS_HOST` (pinned by test).
pub const BUILTIN_INSTALLER_EGRESS_HOST: &str = SEED_CDN_HOST;

/// Core-built-in installer egress port (HTTPS).
pub const BUILTIN_INSTALLER_EGRESS_PORT: u16 = SEED_CDN_PORT;

/// Core-built-in installer egress in `host:port` form for strict-config
/// allowlists (`cdn.bitty.run:443`).
pub const BUILTIN_INSTALLER_EGRESS: &str = "cdn.bitty.run:443";

/// Whether `host` is the core-built-in installer egress host.
///
/// Exact, case-sensitive equality with [`BUILTIN_INSTALLER_EGRESS_HOST`].
/// Suffix tricks (`cdn.bitty.run.evil.com`), userinfo splits
/// (`cdn.bitty.run@evil`), case tricks (`CDN.BITTY.RUN`), and empty hosts
/// all return `false`.
#[must_use]
pub fn is_builtin_installer_host(host: &str) -> bool {
    host == BUILTIN_INSTALLER_EGRESS_HOST
}

/// Whether `url` addresses the core-built-in installer egress.
///
/// Strict, case-sensitive: the URL must start with exactly
/// `https://cdn.bitty.run/` (implicit [`BUILTIN_INSTALLER_EGRESS_PORT`])
/// or `https://cdn.bitty.run:443/` (explicit port). Anything else — suffix
/// hosts (`https://cdn.bitty.run.evil.com/`), userinfo
/// (`https://cdn.bitty.run@evil/`), uppercase schemes/hosts
/// (`HTTPS://`/`CDN.BITTY.RUN`), or port swaps (`:8443`, `:80`) — returns
/// `false`. Path contents are irrelevant here (egress is host-level); the
/// URL-shape audit ([`audit_seed_url`]) still applies to constructed URLs.
#[must_use]
pub fn is_builtin_installer_url(url: &str) -> bool {
    if !(url.starts_with("https://cdn.bitty.run/") || url.starts_with("https://cdn.bitty.run:443/"))
    {
        return false;
    }
    // Belt and braces: the literal prefixes above already pin the host, but
    // route through the host check so `is_builtin_installer_host`,
    // `BUILTIN_INSTALLER_EGRESS_HOST`, and `BUILTIN_INSTALLER_EGRESS_PORT`
    // stay live in non-test builds (no dead-code drift between the URL gate
    // and the host/port gate).
    let after_scheme = &url["https://".len()..];
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = authority.split(':').next().unwrap_or("");
    if !is_builtin_installer_host(host) {
        return false;
    }
    if let Some(port_text) = authority.split(':').nth(1) {
        return port_text.parse::<u16>().ok() == Some(BUILTIN_INSTALLER_EGRESS_PORT);
    }
    // No explicit port: `https` implies `BUILTIN_INSTALLER_EGRESS_PORT`.
    // Reference the port constant so the implicit-443 invariant is checked
    // against the egress definition, not a magic number.
    debug_assert_eq!(BUILTIN_INSTALLER_EGRESS_PORT, SEED_CDN_PORT);
    true
}

/// Auto-download consent level for one installer fetch (issue #1905,
/// delta 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerConsentLevel {
    /// No prompt: the fetch targets the built-in CDN host and both digests
    /// verify (registry/manifest digest and CDN `SHA256SUMS` both match the
    /// fetched bytes, per the #1906 dual-digest direction).
    Silent,
    /// Explicit confirm required: third-party host, single-digest-only, or
    /// any prior digest mismatch history. The caller prompts (`--yes`
    /// approves non-interactively, interactive `[y/N]`, decline/EOF exits
    /// 1 with no staging).
    Confirm,
}

/// Consent level for a fetch: silent iff the host is the built-in installer
/// egress **and** the payload is dual-digest verified; otherwise confirm.
///
/// Pure (no I/O): `is_builtin` should come from
/// [`is_builtin_installer_url`] and `dual_digest_verified` from
/// [`is_dual_digest_verified`].
#[must_use]
pub fn installer_consent_level(
    is_builtin: bool,
    dual_digest_verified: bool,
) -> InstallerConsentLevel {
    if is_builtin && dual_digest_verified {
        InstallerConsentLevel::Silent
    } else {
        InstallerConsentLevel::Confirm
    }
}

/// Whether a hex digest string is a 64-character SHA-256 hex value.
#[must_use]
pub fn is_hex_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether the dual-digest rule holds (issue #1906 direction, checked
/// locally): both the registry/manifest digest and the CDN `SHA256SUMS`
/// digest are present, well-formed, and both equal the fetched bytes'
/// digest.
///
/// - `registry_digest`: the out-of-CDN pin (`None` means single-source-only,
///   which returns `false` so the caller takes the confirm path).
/// - `cdn_digest` / `actual`: the `SHA256SUMS` entry and the `sha256_hex` of
///   the fetched bytes. Comparison is ASCII case-insensitive (both sides
///   are lowercased, mirroring [`find_tarball_digest`]).
/// - Any malformed digest or any mismatch returns `false` (never a silent
///   pick: mismatch is an integrity failure upstream, single-source is a
///   confirm, per #1905/#1906).
#[must_use]
pub fn is_dual_digest_verified(
    registry_digest: Option<&str>,
    cdn_digest: &str,
    actual: &str,
) -> bool {
    let Some(registry) = registry_digest else {
        return false;
    };
    if !is_hex_digest(registry) || !is_hex_digest(cdn_digest) || !is_hex_digest(actual) {
        return false;
    }
    registry.eq_ignore_ascii_case(actual) && cdn_digest.eq_ignore_ascii_case(actual)
}

/// R2 key prefix for prebuilt component distributions.
pub const SEED_KEY_PREFIX: &str = "bitty/components";

/// Maximum bytes of a `--version` value (mirrors the component CLI bound).
pub const SEED_VERSION_MAX_BYTES: usize = 64;

/// Maximum bytes of a target triple.
pub const SEED_TARGET_MAX_BYTES: usize = 64;

/// Maximum bytes of a constructed seed URL.
pub const SEED_URL_MAX_BYTES: usize = 512;

/// Maximum bytes of a fetched `SHA256SUMS` manifest (one line per target;
/// a handful of targets fit in well under one KiB).
pub const SEED_MANIFEST_MAX_BYTES: u64 = 64 * 1024;

/// Maximum bytes of a fetched component tarball: the executable ceiling
/// plus 1 MiB of slack for the descriptor, tar headers, and gzip framing
/// (gzip of incompressible bytes can exceed its input slightly).
pub const SEED_TARBALL_MAX_BYTES: u64 = COMPONENT_EXECUTABLE_MAX_BYTES + 1024 * 1024;

/// Maximum manifest lines parsed (bounded iteration over bounded bytes).
pub const SEED_MANIFEST_MAX_LINES: usize = 4096;

/// Maximum bytes of one manifest line.
pub const SEED_MANIFEST_LINE_MAX_BYTES: usize = 1024;

/// Maximum bytes of `curl` stderr kept for a fetch diagnostic.
pub const SEED_FETCH_STDERR_MAX_BYTES: usize = 512;

/// Connect timeout in seconds for seed `curl` fetches (`--connect-timeout`).
/// Bounds TCP/TLS/proxy setup so a stalled network fails closed instead of
/// hanging `install` silently.
pub const SEED_CURL_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Total timeout in seconds for seed `curl` fetches (`--max-time`). Bounds
/// the whole transfer so `Command::output()` always returns.
pub const SEED_CURL_MAX_TIME_SECS: u64 = 120;

/// Minimum supported `curl` major version (fetch-environment floor).
pub const SEED_CURL_FLOOR_MAJOR: u64 = 7;

/// Minimum supported `curl` minor version (fetch-environment floor).
pub const SEED_CURL_FLOOR_MINOR: u64 = 68;

/// Minimum supported `curl` patch version (fetch-environment floor).
pub const SEED_CURL_FLOOR_PATCH: u64 = 0;

/// Minimum supported `curl` version in `major.minor.patch` form.
///
/// Driver flags in [`SystemTransport::curl_argv`] and the curl release that
/// introduced each (per `curl.se/docs/optionsall.html` and the curl
/// changelog): `--max-time` since 4.0, `--connect-timeout` since 7.7,
/// `--max-filesize` since 7.10.8, `--proto` (including the `=https`
/// allow-only modifier, part of the original `--proto` design) since
/// 7.20.2/7.21.0. The bare minimum would be 7.21.0; the floor is raised
/// conservatively to 7.68.0 (January 2020, the Ubuntu 20.04 LTS baseline)
/// for stable `--proto` redirect semantics and modern TLS/proxy handling on
/// every platform bitty ships (Linux, macOS, Windows), while staying old
/// enough not to break LTS installs gratuitously. Must stay equal to the
/// `major.minor.patch` triple above (pinned by test).
pub const SEED_CURL_FLOOR: &str = "7.68.0";

/// Maximum bytes of `tar -tzf` output parsed for the member audit.
pub const SEED_TAR_LIST_MAX_BYTES: usize = 8192;

/// Fetch the tarball and manifest URLs for one component release.
///
/// [`SeedUrls`] is pure (no spawn, no filesystem writes): inputs are
/// validated against the allowlist first, URLs are built from the validated
/// parts only, and the result is audited before return. Hostile inputs fail
/// here with zero spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedUrls {
    /// `https://cdn.bitty.run/bitty/components/<name>/<version>/<target>.tar.gz`.
    pub tarball_url: String,
    /// `https://cdn.bitty.run/bitty/components/<name>/<version>/SHA256SUMS`.
    pub manifest_url: String,
    /// Tarball file name (`<target>.tar.gz`, the manifest lookup key).
    pub tarball_file: String,
}

/// Why the seed refused (all variants carry owned, user-facing context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedError {
    /// The component name violates `[a-z][a-z0-9-]{0,31}` (CLI input).
    InvalidName(String),
    /// The version is not a strict `X.Y.Z` numeric core (CLI input).
    InvalidVersion(String),
    /// The target triple violates the grammar (internal; no CLI flag feeds
    /// it, so reaching this means a corrupt host mapping or caller bug).
    InvalidTarget(String),
    /// The host OS/arch maps to no published target triple.
    UnsupportedHost(String),
    /// A required system tool is missing (`curl` or `tar`).
    ToolMissing(String),
    /// The system `curl` is present but older than [`SEED_CURL_FLOOR`].
    CurlTooOld(String),
    /// The download failed (URL plus a bounded reason).
    Fetch(String),
    /// The `SHA256SUMS` manifest is missing, malformed, or has no entry.
    Manifest(String),
    /// The fetched bytes do not match the manifest digest (tamper).
    DigestMismatch(String),
    /// The extracted payload is not exactly the two expected members.
    MemberViolation(String),
    /// The extracted descriptor disagrees with the request or its binary
    /// (substitution or downgrade).
    DescriptorMismatch(String),
    /// A filesystem operation failed after validation (I/O context).
    Io(String),
}

impl SeedError {
    /// Stable exit code on the component taxonomy (`0` never: success has no
    /// error; `2` usage, `1` environmental, `4` artifact integrity).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidName(_) | Self::InvalidVersion(_) => 2,
            Self::InvalidTarget(_)
            | Self::UnsupportedHost(_)
            | Self::ToolMissing(_)
            | Self::CurlTooOld(_)
            | Self::Fetch(_)
            | Self::Io(_) => 1,
            Self::Manifest(_)
            | Self::DigestMismatch(_)
            | Self::MemberViolation(_)
            | Self::DescriptorMismatch(_) => 4,
        }
    }

    /// User-facing message (already complete; the caller prefixes context).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::InvalidName(name) => {
                format!("invalid component name {name:?} (want [a-z][a-z0-9-]{{0,31}}, e.g. `net`)")
            }
            Self::InvalidVersion(version) => {
                format!("invalid version {version:?} (want strict X.Y.Z numeric core, e.g. 0.0.23)")
            }
            Self::InvalidTarget(target) => {
                format!("invalid target triple {target:?} (internal error: refusing to fetch)")
            }
            Self::UnsupportedHost(detail) => detail.clone(),
            Self::ToolMissing(tool) => tool.clone(),
            Self::CurlTooOld(detail) => detail.clone(),
            Self::Fetch(detail) => detail.clone(),
            Self::Manifest(detail) => detail.clone(),
            Self::DigestMismatch(detail) => detail.clone(),
            Self::MemberViolation(detail) => detail.clone(),
            Self::DescriptorMismatch(detail) => detail.clone(),
            Self::Io(detail) => detail.clone(),
        }
    }
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SeedError {}

/// Host target triple for this binary (`None` on unmapped OS/arch).
///
/// Covers exactly the published bootstrap rows (`packaging/README.md`):
/// Linux glibc/musl x86_64 plus aarch64 glibc, macOS arm64/x86_64, Windows
/// x64/arm64. Musl detection is compile-time (`target_env`), so a musl
/// host never requests a glibc build.
#[must_use]
pub fn host_target_triple() -> Option<&'static str> {
    const OS: &str = std::env::consts::OS;
    const ARCH: &str = std::env::consts::ARCH;
    // Musl hosts can only run musl builds; the only published musl target
    // is Alpine x86_64, so any other musl host is unsupported (returning
    // the glibc triple would fetch an unrunnable binary).
    if cfg!(target_env = "musl") {
        return match (OS, ARCH) {
            ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
            _ => None,
        };
    }
    match (OS, ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

/// Strict `X.Y.Z` numeric core (pack policy, mirroring
/// `make-component-dist.sh valid_version`): no leading zeros, no prerelease
/// or build metadata, each part fitting `u32`. Narrower than the installed
/// parser's full semver by intent: R2 keys only ever hold packed `X.Y.Z`
/// releases, so anything else fails before any URL is built.
#[must_use]
pub fn valid_seed_version(version: &str) -> bool {
    if version.is_empty() || version.len() > SEED_VERSION_MAX_BYTES {
        return false;
    }
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    for part in parts {
        if part.is_empty() || part.len() > 10 {
            return false;
        }
        if part.len() > 1 && part.starts_with('0') {
            return false;
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if part.len() == 10 && part > "4294967295" {
            return false;
        }
    }
    true
}

/// Target-triple grammar (mirroring `make-component-dist.sh valid_target`):
/// lowercase alphanumerics/underscores in at least two dash-separated
/// fields. Applied to the detected host triple as defense in depth (the
/// value is a compiled-in constant, but the URL builder trusts no input).
#[must_use]
pub fn valid_seed_target(target: &str) -> bool {
    if target.is_empty() || target.len() > SEED_TARGET_MAX_BYTES {
        return false;
    }
    let fields: Vec<&str> = target.split('-').collect();
    if fields.len() < 2 {
        return false;
    }
    fields.iter().all(|field| {
        !field.is_empty()
            && field
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// Final audit over a constructed URL: exact `https://cdn.bitty.run` base
/// (built from [`SEED_CDN_HOST`], the single allowlisted host), exact
/// `bitty/components/` prefix, bounded length, and no byte outside the
/// URL-safe set the validated parts can produce. Belt and braces after part
/// validation: the audit pins the invariant for review even though
/// validated parts cannot violate it.
fn audit_seed_url(url: &str) -> bool {
    if url.len() > SEED_URL_MAX_BYTES {
        return false;
    }
    let prefix = format!("https://{SEED_CDN_HOST}/{SEED_KEY_PREFIX}/");
    if !url.starts_with(&prefix) {
        return false;
    }
    if url.contains("..") {
        return false;
    }
    url.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'/' | b'.' | b'-' | b'_' | b'~'))
}

/// Build the fetch URLs for one component release (pure: zero spawn).
///
/// Validates name, version, and target against the allowlist, formats the
/// two URLs from the validated parts only, then audits the result.
pub fn seed_urls(name: &str, version: &str, target: &str) -> Result<SeedUrls, SeedError> {
    if let Err(error) = validate_component_name(name) {
        return Err(SeedError::InvalidName(format!("{name:?} ({error})")));
    }
    if !valid_seed_version(version) {
        return Err(SeedError::InvalidVersion(version.to_owned()));
    }
    if !valid_seed_target(target) {
        return Err(SeedError::InvalidTarget(target.to_owned()));
    }
    let tarball_file = format!("{target}.tar.gz");
    let tarball_url = format!("{SEED_CDN_BASE}/{SEED_KEY_PREFIX}/{name}/{version}/{tarball_file}");
    let manifest_url = format!("{SEED_CDN_BASE}/{SEED_KEY_PREFIX}/{name}/{version}/SHA256SUMS");
    if !audit_seed_url(&tarball_url) || !audit_seed_url(&manifest_url) {
        return Err(SeedError::InvalidTarget(format!(
            "constructed URL failed audit (name={name:?} version={version:?} target={target:?})"
        )));
    }
    Ok(SeedUrls {
        tarball_url,
        manifest_url,
        tarball_file,
    })
}

// ---------------------------------------------------------------------------
// Transport (fixed-argv system tools; injectable for hermetic tests)
// ---------------------------------------------------------------------------

/// Seed transport: fetch a URL to a file, extract named members.
///
/// Production is [`SystemTransport`] (fixed-argv `curl`/`tar`, no shell).
/// Tests inject a stub that records argv (zero-spawn assertions) and serves
/// fixture bytes or writes fixture members (tamper/downgrade corpora).
pub trait SeedTransport {
    /// Fetch `url` to `dest` (creating it), refusing bodies over
    /// `max_bytes`.
    fn fetch(&mut self, url: &str, dest: &Path, max_bytes: u64) -> Result<(), SeedError>;
    /// List the member names of `archive` (no extraction).
    fn list_members(&mut self, archive: &Path) -> Result<Vec<String>, SeedError>;
    /// Extract exactly `members` from `archive` into existing `dest`.
    fn extract(&mut self, archive: &Path, dest: &Path, members: &[String])
    -> Result<(), SeedError>;
}

/// Production transport: fixed-argv `curl` + `tar`, never a shell.
///
/// Metacharacters in every argument are inert data: argv arrays go directly
/// to [`Command`] (`sh -c` is never constructed on any platform).
/// Children inherit the caller's environment for system proxy
/// (`https_proxy`/`HTTPS_PROXY`/`all_proxy` plus `no_proxy`) and system
/// CA-store discovery (like `scripts/install.sh`), minus secret-shaped
/// variables ([`is_secret_env_name`]); argv is fixed and audited, so no
/// secret material crosses the boundary. `curl` resolves via `PATH`
/// (`curl.exe` on Windows through `PATHEXT`), same for `tar`/`tar.exe`.
pub struct SystemTransport;

/// Parsed `curl --version` first-line version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CurlVersion {
    /// Major version (before the first `.`).
    pub major: u64,
    /// Minor version (between the dots).
    pub minor: u64,
    /// Patch version (after the second `.`; `0` when absent).
    pub patch: u64,
}

impl CurlVersion {
    /// The enforced floor ([`SEED_CURL_FLOOR_MAJOR`]..[`SEED_CURL_FLOOR_PATCH`]).
    #[must_use]
    pub fn floor() -> Self {
        Self {
            major: SEED_CURL_FLOOR_MAJOR,
            minor: SEED_CURL_FLOOR_MINOR,
            patch: SEED_CURL_FLOOR_PATCH,
        }
    }

    /// Whether this version meets the floor (lexicographic `>=`).
    #[must_use]
    pub fn meets_floor(self) -> bool {
        self >= Self::floor()
    }
}

impl std::fmt::Display for CurlVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One numeric version part: leading ASCII digits, no sign, no whitespace.
///
/// Returns `None` for an empty digit run or an unparsable (overflowing)
/// number, so hostile version strings fail closed upstream.
fn parse_version_part(text: &str) -> Option<u64> {
    let end = text.bytes().take_while(|b| b.is_ascii_digit()).count();
    if end == 0 {
        return None;
    }
    text[..end].parse::<u64>().ok()
}

/// Parse the version from `curl --version` output.
///
/// Reads the first line only, expecting `curl X.Y.Z ...` where the version
/// token is the second whitespace field. Handles the Windows suffix form
/// (`curl 8.4.0 (Windows) ...`) because the parenthesized marker is a later
/// field, plus trailing per-part suffixes (`8.4.0-DEV` parses the leading
/// digits of each part). Major and minor are required; a missing patch
/// defaults to `0`. Anything else returns `None` (fail closed upstream).
#[must_use]
pub fn parse_curl_version(output: &str) -> Option<CurlVersion> {
    let first = output.lines().next()?.trim();
    let mut fields = first.split_whitespace();
    if fields.next() != Some("curl") {
        return None;
    }
    let token = fields.next()?;
    let mut parts = token.split('.');
    let major = parse_version_part(parts.next()?)?;
    let minor = parse_version_part(parts.next()?)?;
    let patch = match parts.next() {
        Some(text) => parse_version_part(text)?,
        None => 0,
    };
    Some(CurlVersion {
        major,
        minor,
        patch,
    })
}

/// Enforce the floor against one `curl --version` output.
///
/// `Ok(version)` when the output parses and meets the floor; otherwise a
/// fail-closed [`SeedError`]: a parsed-but-old version surfaces as
/// [`SeedError::CurlTooOld`] (exit `1`) naming the found version, the floor,
/// and the upgrade hint, while unparsable output surfaces as
/// [`SeedError::Fetch`] (environmental, exit `1`) naming the floor.
pub fn check_curl_version_output(output: &str) -> Result<CurlVersion, SeedError> {
    match parse_curl_version(output) {
        Some(version) if version.meets_floor() => Ok(version),
        Some(version) => Err(SeedError::CurlTooOld(format!(
            "bitty component: curl {version} is too old (need {SEED_CURL_FLOOR} or newer for --proto =https, --connect-timeout, --max-time, --max-filesize; upgrade curl and retry, or place the release manually under $XDG_DATA_HOME/bitty/components/<name>/<version>/)"
        ))),
        None => Err(SeedError::Fetch(format!(
            "bitty component: cannot parse `curl --version` output (need curl {SEED_CURL_FLOOR} or newer; upgrade curl and retry)"
        ))),
    }
}

/// Whether an environment variable name is secret-shaped and must never
/// cross into a fetch child.
///
/// Case-insensitive suffix match on `_TOKEN`, `_KEY`, `_SECRET`: covers
/// `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, `CARGO_REGISTRY_TOKEN`, and
/// similar without enumerating providers. Proxy variables (`https_proxy`,
/// `HTTPS_PROXY`, `all_proxy`, `no_proxy`, ...) never match (no such
/// suffix), so proxy inheritance is unaffected.
#[must_use]
pub fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_TOKEN") || upper.ends_with("_KEY") || upper.ends_with("_SECRET")
}

/// Live secret-shaped variable names to scrub from one child spawn.
///
/// Enumerated at spawn time (the process environment can change between
/// invocations, and tests poison it); the caller removes each from the
/// child [`Command`] while inheriting everything else (proxy/CA discovery
/// stays intact).
fn secret_env_removals() -> Vec<String> {
    secret_removals_in(std::env::vars_os().filter_map(|(key, _)| key.into_string().ok()))
}

/// Pure core of [`secret_env_removals`]: the secret-shaped names among
/// `names`. Pure so the hostile-env audit is provable without mutating the
/// process environment (see the `hostile_env_*` tests).
fn secret_removals_in(names: impl Iterator<Item = String>) -> Vec<String> {
    names.filter(|name| is_secret_env_name(name)).collect()
}

/// Child [`Command`] for one fixed-argv tool spawn.
///
/// Resolves `tool` via `PATH` (`curl` finds `curl.exe` on Windows through
/// `PATHEXT`, same for `tar`/`tar.exe`); inherits the caller environment
/// for system proxy and system CA-store discovery, minus secret-shaped
/// variables ([`is_secret_env_name`]). The process environment is never
/// mutated: removals apply to the child only.
fn scrubbed_command(tool: &str) -> Command {
    let mut command = Command::new(tool);
    for name in secret_env_removals() {
        command.env_remove(name);
    }
    command
}

/// Actionable diagnostic for absent `curl`: names the floor, the manual
/// placement path, and that no staging was attempted (exit `1`).
fn curl_missing_diagnostic() -> String {
    format!(
        "bitty component: `curl` not found (need curl {SEED_CURL_FLOOR} or newer; install curl first, or place the release manually under $XDG_DATA_HOME/bitty/components/<name>/<version>/; nothing was fetched or staged)"
    )
}

/// Actionable diagnostic for absent `tar` (same fail-closed shape).
fn tar_missing_diagnostic() -> String {
    "bitty component: `tar` not found (install tar first, or place the release manually under $XDG_DATA_HOME/bitty/components/<name>/<version>/; nothing was fetched or staged)".to_string()
}

/// Result of the once-per-invocation `curl --version` preflight.
static CURL_PREFLIGHT: std::sync::OnceLock<Result<CurlVersion, SeedError>> =
    std::sync::OnceLock::new();

/// How many times the preflight actually spawned `curl --version`
/// (cache misses; hits never spawn). Observed by tests.
static CURL_PREFLIGHT_SPAWNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run the cached preflight: `curl --version` parsed against the floor.
///
/// The first call in the process spawns `curl --version` (via
/// [`scrubbed_command`], so secrets are already scrubbed here) and caches
/// the outcome; later calls return the cached result without spawning, so
/// one CLI invocation preflights once no matter how many fetches follow.
/// Absent curl fails closed with the manual-placement diagnostic (exit `1`);
/// too-old curl fails closed naming found/floor/upgrade (exit `1`).
pub fn preflight_curl() -> Result<CurlVersion, SeedError> {
    CURL_PREFLIGHT
        .get_or_init(|| {
            CURL_PREFLIGHT_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let output = scrubbed_command("curl")
                .arg("--version")
                .output()
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        SeedError::ToolMissing(curl_missing_diagnostic())
                    } else {
                        SeedError::Io(format!(
                            "bitty component: cannot run `curl --version`: {error}"
                        ))
                    }
                })?;
            if !output.status.success() {
                return Err(SeedError::Fetch(format!(
                    "bitty component: `curl --version` failed ({})",
                    output.status
                )));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            check_curl_version_output(&text)
        })
        .clone()
}

impl SystemTransport {
    /// Fixed curl argv (mirrors `scripts/install.sh -fsSL` plus the seed
    /// pins): `--fail` (HTTP errors are failures, never staged error
    /// pages), `--silent --show-error` (quiet unless failing), `--location`
    /// (CDN redirects), `--proto =https` (redirects stay on https even if
    /// the URL audit were ever bypassed), `--connect-timeout` (bounded
    /// TCP/TLS/proxy setup) and `--max-time` (bounded total transfer, so
    /// `run_tool` cannot wait indefinitely on a stalled network),
    /// `--max-filesize` (client-side
    /// bound), `--output` (no stdout pipe, no truncation surprises).
    fn curl_argv(url: &str, dest: &Path, max_bytes: u64) -> Vec<String> {
        let argv = vec![
            "--fail".to_string(),
            "--silent".to_string(),
            "--show-error".to_string(),
            "--location".to_string(),
            "--proto".to_string(),
            "=https".to_string(),
            "--connect-timeout".to_string(),
            SEED_CURL_CONNECT_TIMEOUT_SECS.to_string(),
            "--max-time".to_string(),
            SEED_CURL_MAX_TIME_SECS.to_string(),
            "--max-filesize".to_string(),
            max_bytes.to_string(),
            "--output".to_string(),
            dest.to_string_lossy().into_owned(),
            url.to_string(),
        ];
        // Posture pin (issue #1907): system CA store only (no `--cacert` /
        // `--capath` override), system proxy via inherited env (no `--proxy`
        // flag), no custom TLS version/cipher flags. The unit test
        // `curl_argv_pins_ca_proxy_tls_posture` audits the full argv.
        debug_assert!(
            !argv.iter().any(|arg| matches!(
                arg.as_str(),
                "--cacert" | "--capath" | "--ciphers" | "--proxy" | "--tls-max"
            ) || arg.starts_with("--tlsv")),
            "curl argv must not override CA, proxy, or TLS posture"
        );
        argv
    }

    /// Fixed tar argv: explicit member list only (no wildcards), extraction
    /// confined with `-C`; the member-list audit plus the post-extraction
    /// audit hold regardless of tar implementation quirks.
    fn tar_argv(archive: &Path, dest: &Path, members: &[String]) -> Vec<String> {
        let mut argv = vec![
            "-xzf".to_string(),
            archive.to_string_lossy().into_owned(),
            "-C".to_string(),
            dest.to_string_lossy().into_owned(),
        ];
        argv.extend(members.iter().cloned());
        argv
    }

    /// Fixed tar list argv: names only, never extracted here.
    fn tar_list_argv(archive: &Path) -> Vec<String> {
        vec!["-tzf".to_string(), archive.to_string_lossy().into_owned()]
    }

    /// Run one fixed-argv tool; map every failure to [`SeedError`].
    ///
    /// The child is spawned via [`scrubbed_command`] (secret-shaped env vars
    /// removed, proxy/CA inheritance intact); absent tools fail closed with
    /// the manual-placement diagnostic (exit `1`, no staging attempted).
    fn run_tool(tool: &str, argv: &[String]) -> Result<(), SeedError> {
        let output = scrubbed_command(tool)
            .args(argv)
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    SeedError::ToolMissing(if tool == "curl" {
                        curl_missing_diagnostic()
                    } else if tool == "tar" {
                        tar_missing_diagnostic()
                    } else {
                        format!(
                            "bitty component: `{tool}` not found (install {tool} first; nothing was staged)"
                        )
                    })
                } else {
                    SeedError::Io(format!("bitty component: cannot run `{tool}`: {error}"))
                }
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = stderr
                .chars()
                .rev()
                .take(SEED_FETCH_STDERR_MAX_BYTES)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let tail = tail
                .chars()
                .map(|c| {
                    if c.is_control() && c != '\n' && c != '\t' {
                        '\u{fffd}'
                    } else {
                        c
                    }
                })
                .collect::<String>();
            return Err(SeedError::Fetch(format!(
                "bitty component: `{tool}` failed ({}; {tail})",
                output.status,
                tail = tail.trim(),
            )));
        }
        Ok(())
    }
}

impl SeedTransport for SystemTransport {
    fn fetch(&mut self, url: &str, dest: &Path, max_bytes: u64) -> Result<(), SeedError> {
        // Preflight once per invocation (cached): absent or too-old curl
        // fails here, before any fetch or staging.
        preflight_curl()?;
        let argv = Self::curl_argv(url, dest, max_bytes);
        Self::run_tool("curl", &argv)
    }

    fn list_members(&mut self, archive: &Path) -> Result<Vec<String>, SeedError> {
        let argv = Self::tar_list_argv(archive);
        let output = scrubbed_command("tar")
            .args(&argv)
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    SeedError::ToolMissing(tar_missing_diagnostic())
                } else {
                    SeedError::Io(format!("bitty component: cannot run `tar`: {error}"))
                }
            })?;
        if !output.status.success() {
            return Err(SeedError::Fetch(format!(
                "bitty component: `tar -tzf` failed ({})",
                output.status
            )));
        }
        if output.stdout.len() > SEED_TAR_LIST_MAX_BYTES {
            return Err(SeedError::MemberViolation(format!(
                "bitty component: archive member list exceeds {SEED_TAR_LIST_MAX_BYTES} bytes"
            )));
        }
        let text = std::str::from_utf8(&output.stdout).map_err(|_| {
            SeedError::MemberViolation(
                "bitty component: archive member names are not UTF-8".to_string(),
            )
        })?;
        Ok(text.lines().map(str::to_string).collect())
    }

    fn extract(
        &mut self,
        archive: &Path,
        dest: &Path,
        members: &[String],
    ) -> Result<(), SeedError> {
        let argv = Self::tar_argv(archive, dest, members);
        Self::run_tool("tar", &argv)
    }
}

// ---------------------------------------------------------------------------
// Pipeline (fetch, verify, unpack; staging stays in `component.rs`)
// ---------------------------------------------------------------------------

/// Verified seed payload ready for the shared atomic publish path.
///
/// Field-for-field compatible with the `add` staging shape: the caller runs
/// the same ABI check and `install_staged`, so seed installs and local
/// installs share every downstream guarantee (digest-idempotent re-add,
/// symlink refusal, atomic publish, `current` last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedPayload {
    /// Validated component name.
    pub name: String,
    /// Requested version (must equal the descriptor version).
    pub version: String,
    /// Lowest supported wire protocol version.
    pub protocol_min: u16,
    /// Highest supported wire protocol version.
    pub protocol_max: u16,
    /// Executable base name (`bitty-<name>`).
    pub executable: String,
    /// Raw executable bytes (digest-verified).
    pub bytes: Vec<u8>,
}

/// Staging directory guard: a fresh temp dir removed on drop (success moves
/// the payload out first, then disarms by forgetting nothing — the payload
/// bytes live in memory, so dropping the dir is always safe).
struct ScopedDir {
    path: PathBuf,
    disarm: bool,
}

impl ScopedDir {
    fn create(tag: &str) -> Result<Self, SeedError> {
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt as _;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // Each attempt names a fresh unpredictable directory (pid + sequence
        // + nanos) and creates it exclusively: no delete/reuse, so a
        // pre-created /tmp sibling (sticky-bit takeover) fails with
        // AlreadyExists and the next suffix is tried instead.
        for _ in 0..100 {
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "bitty-seed-{tag}-{}-{sequence}-{nanos}",
                std::process::id()
            ));
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        disarm: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(SeedError::Io(format!(
                        "bitty component: cannot create staging dir '{}': {error}",
                        path.display()
                    )));
                }
            }
        }
        Err(SeedError::Io(
            "bitty component: cannot create staging dir (no fresh name after retries)".to_string(),
        ))
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        if !self.disarm {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Parse one `SHA256SUMS` manifest and return the digest pinned for
/// `tarball_file` (GNU `sha256sum` format: `<hex><space><space|*><name>`).
///
/// Requires exactly one matching entry; zero matches (unknown file),
/// duplicates, malformed lines, or a non-hex digest all fail closed.
fn find_tarball_digest(manifest: &str, tarball_file: &str) -> Result<String, SeedError> {
    let refuse = |why: &str| {
        SeedError::Manifest(format!(
            "bitty component: invalid SHA256SUMS manifest ({why})"
        ))
    };
    let mut matches = 0usize;
    let mut digest = String::new();
    for (index, line) in manifest.lines().enumerate() {
        if index >= SEED_MANIFEST_MAX_LINES {
            return Err(refuse("too many lines"));
        }
        // Tolerate one trailing carriage return (CRLF mirrors); anything
        // longer than the line bound fails closed.
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() > SEED_MANIFEST_LINE_MAX_BYTES {
            return Err(refuse("line too long"));
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let hex = parts.next().ok_or_else(|| refuse("malformed line"))?;
        let rest = parts.next().ok_or_else(|| refuse("malformed line"))?;
        let file = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*'));
        let Some(file) = file else {
            return Err(refuse("malformed line"));
        };
        let hex_ok = hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex_ok {
            return Err(refuse("digest is not 64 hex characters"));
        }
        // Reject path-bearing entries outright (no basename games).
        if file.contains('/') || file.contains('\\') || file.is_empty() {
            return Err(refuse("entry carries a path"));
        }
        if file == tarball_file {
            matches += 1;
            digest = hex.to_ascii_lowercase();
        }
    }
    if matches == 1 {
        Ok(digest)
    } else if matches == 0 {
        Err(SeedError::Manifest(format!(
            "bitty component: SHA256SUMS has no entry for {tarball_file:?} (refusing to guess)"
        )))
    } else {
        Err(SeedError::Manifest(format!(
            "bitty component: SHA256SUMS has {matches} entries for {tarball_file:?} (ambiguous)"
        )))
    }
}

/// Fetch the tarball plus its manifest and verify the digest (fail closed).
///
/// Returns the verified tarball bytes. The manifest is the authoritative
/// checksum record; a missing entry, a malformed manifest, or a digest
/// mismatch refuses the install before anything is unpacked or staged.
fn fetch_verified_tarball(
    urls: &SeedUrls,
    transport: &mut dyn SeedTransport,
) -> Result<Vec<u8>, SeedError> {
    let stage = ScopedDir::create("fetch")?;
    let manifest_path = stage.path.join("SHA256SUMS");
    transport.fetch(&urls.manifest_url, &manifest_path, SEED_MANIFEST_MAX_BYTES)?;
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|error| {
        SeedError::Io(format!(
            "bitty component: cannot read fetched manifest: {error}"
        ))
    })?;
    if manifest_bytes.len() as u64 > SEED_MANIFEST_MAX_BYTES {
        return Err(SeedError::Manifest(format!(
            "bitty component: fetched manifest exceeds {SEED_MANIFEST_MAX_BYTES} bytes"
        )));
    }
    let manifest = std::str::from_utf8(&manifest_bytes).map_err(|_| {
        SeedError::Manifest("bitty component: fetched manifest is not UTF-8".to_string())
    })?;
    let expected = find_tarball_digest(manifest, &urls.tarball_file)?;

    let tarball_path = stage.path.join(&urls.tarball_file);
    transport.fetch(&urls.tarball_url, &tarball_path, SEED_TARBALL_MAX_BYTES)?;
    let bytes = std::fs::read(&tarball_path).map_err(|error| {
        SeedError::Io(format!(
            "bitty component: cannot read fetched tarball: {error}"
        ))
    })?;
    if bytes.len() as u64 > SEED_TARBALL_MAX_BYTES {
        return Err(SeedError::Fetch(format!(
            "bitty component: fetched tarball exceeds {SEED_TARBALL_MAX_BYTES} bytes"
        )));
    }
    if bytes.is_empty() {
        return Err(SeedError::Fetch(
            "bitty component: fetched tarball is empty".to_string(),
        ));
    }
    let actual = bitty_package::integrity::sha256_hex(&bytes);
    if actual != expected {
        return Err(SeedError::DigestMismatch(format!(
            "bitty component: tarball digest {actual} does not match SHA256SUMS entry {expected} (tampered or corrupt download; refusing to install)"
        )));
    }
    Ok(bytes)
}

/// Audit the extracted staging dir: exactly the two expected members, both
/// regular files within size bounds, never symlinks, nothing else.
///
/// The audit holds regardless of tar implementation quirks (absolute
/// members, `..` members, stray files, links): anything but the exact
/// payload fails closed before the descriptor is even read.
fn audit_extracted(
    dir: &Path,
    descriptor_name: &str,
    executable_name: &str,
) -> Result<(Vec<u8>, Vec<u8>), SeedError> {
    let refuse = |why: String| SeedError::MemberViolation(format!("bitty component: {why}"));
    let entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| {
            SeedError::Io(format!(
                "bitty component: cannot inspect staging dir '{}': {error}",
                dir.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            SeedError::Io(format!(
                "bitty component: cannot inspect staging dir '{}': {error}",
                dir.display()
            ))
        })?;
    if entries.len() != 2 {
        return Err(refuse(format!(
            "unexpected member count {} in component payload (want exactly {descriptor_name} + {executable_name})",
            entries.len()
        )));
    }
    let mut descriptor_bytes: Option<Vec<u8>> = None;
    let mut executable_bytes: Option<Vec<u8>> = None;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let expected_kind = if name == descriptor_name {
            Some("descriptor")
        } else if name == executable_name {
            Some("executable")
        } else {
            None
        };
        let Some(kind) = expected_kind else {
            return Err(refuse(format!(
                "unexpected member {name:?} in component payload (want exactly {descriptor_name} + {executable_name})"
            )));
        };
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
            SeedError::Io(format!(
                "bitty component: cannot inspect member {name:?}: {error}"
            ))
        })?;
        // No-follow stat first: a link must fail before any byte is read
        // through it.
        if metadata.is_symlink() || !metadata.is_file() {
            return Err(refuse(format!(
                "member {name:?} is not a regular file (refusing to follow)"
            )));
        }
        let bound = if kind == "descriptor" {
            COMPONENT_DESCRIPTOR_MAX_BYTES as u64
        } else {
            COMPONENT_EXECUTABLE_MAX_BYTES
        };
        if metadata.len() > bound {
            return Err(refuse(format!("member {name:?} exceeds {bound} bytes")));
        }
        if metadata.len() == 0 {
            return Err(refuse(format!("member {name:?} is empty")));
        }
        let bytes = std::fs::read(entry.path()).map_err(|error| {
            SeedError::Io(format!(
                "bitty component: cannot read member {name:?}: {error}"
            ))
        })?;
        if kind == "descriptor" {
            descriptor_bytes = Some(bytes);
        } else {
            executable_bytes = Some(bytes);
        }
    }
    match (descriptor_bytes, executable_bytes) {
        (Some(descriptor), Some(executable)) => Ok((descriptor, executable)),
        _ => Err(refuse(format!(
            "component payload is missing {descriptor_name} or {executable_name}"
        ))),
    }
}

/// Unpack verified tarball bytes and validate the payload.
///
/// Checks, in order: exact member set (audit), descriptor parse (strict
/// installed rules), name/version binding to the request (substitution and
/// downgrade fail here), executable-name binding, and descriptor-digest
/// equality against the extracted binary (tamper fails here).
fn unpack_and_validate(
    tarball: &[u8],
    name: &str,
    version: &str,
    transport: &mut dyn SeedTransport,
) -> Result<SeedPayload, SeedError> {
    let stage = ScopedDir::create("unpack")?;
    let archive_path = stage.path.join("payload.tar.gz");
    std::fs::write(&archive_path, tarball).map_err(|error| {
        SeedError::Io(format!("bitty component: cannot stage tarball: {error}"))
    })?;
    let executable_name = format!("{COMPONENT_EXECUTABLE_PREFIX}{name}");
    let members = vec![
        COMPONENT_DESCRIPTOR_FILE.to_string(),
        executable_name.clone(),
    ];
    // Contract audit first: the archive must hold exactly the two expected
    // members (exact multiset compare, no normalization — normalization is
    // where traversal bugs hide). A stray, absolute, or `..` member fails
    // here even though extraction would never materialize it.
    let mut listed = transport.list_members(&archive_path)?;
    listed.sort();
    let mut expected = members.clone();
    expected.sort();
    if listed != expected {
        return Err(SeedError::MemberViolation(format!(
            "bitty component: unexpected archive members {listed:?} (want exactly {expected:?})"
        )));
    }
    let extract_dir = stage.path.join("extract");
    // Exclusive create inside our own 0700 staging dir: no delete/reuse,
    // so a pre-existing entry fails closed instead of being adopted.
    {
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt as _;
        #[cfg(unix)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&extract_dir).map_err(|error| {
            SeedError::Io(format!(
                "bitty component: cannot create extract dir: {error}"
            ))
        })?;
    }
    transport.extract(&archive_path, &extract_dir, &members)?;
    let (descriptor_bytes, executable_bytes) =
        audit_extracted(&extract_dir, COMPONENT_DESCRIPTOR_FILE, &executable_name)?;
    let text = std::str::from_utf8(&descriptor_bytes).map_err(|_| {
        SeedError::DescriptorMismatch(
            "bitty component: extracted descriptor is not UTF-8".to_string(),
        )
    })?;
    let descriptor = ComponentDescriptor::parse(text).map_err(|error| {
        SeedError::DescriptorMismatch(format!(
            "bitty component: extracted descriptor is invalid: {error}"
        ))
    })?;
    // Substitution/downgrade: the payload must BE the requested release.
    if descriptor.name != name {
        return Err(SeedError::DescriptorMismatch(format!(
            "bitty component: payload is for '{}', not {name:?} (refusing to install)",
            descriptor.name
        )));
    }
    if descriptor.version != version {
        return Err(SeedError::DescriptorMismatch(format!(
            "bitty component: payload is version {}, not {version} (refusing to install)",
            descriptor.version
        )));
    }
    if descriptor.executable != executable_name {
        return Err(SeedError::DescriptorMismatch(format!(
            "bitty component: payload executable is {:?}, not {executable_name:?} (refusing to install)",
            descriptor.executable
        )));
    }
    // Tamper: the descriptor digest must match the extracted binary.
    let actual = bitty_package::integrity::sha256_hex(&executable_bytes);
    if descriptor.sha256 != actual {
        return Err(SeedError::DescriptorMismatch(format!(
            "bitty component: payload descriptor sha256 {} does not match extracted binary digest {actual} (tampered payload; refusing to install)",
            descriptor.sha256
        )));
    }
    Ok(SeedPayload {
        name: name.to_owned(),
        version: version.to_owned(),
        protocol_min: descriptor.protocol_min,
        protocol_max: descriptor.protocol_max,
        executable: executable_name,
        bytes: executable_bytes,
    })
}

/// Fetch, verify, and unpack one component release (no staging).
///
/// Pure pipeline over `transport`: builds allowlisted URLs (zero spawn on
/// hostile input), fetches manifest + tarball, verifies the digest, audits
/// the members, and binds the descriptor to the request. The caller stages
/// the payload through the shared `install_staged` path.
pub fn fetch_seed_payload(
    name: &str,
    version: &str,
    target: &str,
    transport: &mut dyn SeedTransport,
) -> Result<SeedPayload, SeedError> {
    let urls = seed_urls(name, version, target)?;
    let tarball = fetch_verified_tarball(&urls, transport)?;
    unpack_and_validate(&tarball, name, version, transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Stub transport: records every invocation (argv evidence + zero-spawn
    /// assertions) and serves canned bytes or writes canned members.
    #[derive(Debug, Default)]
    struct StubTransport {
        /// `(tool, argv)` per spawn, in order.
        spawns: Vec<(String, Vec<String>)>,
        /// URL -> bytes served by `fetch`.
        fetches: HashMap<String, Vec<u8>>,
        /// Files the stub `extract` writes into the dest dir.
        extract_files: Vec<(String, Vec<u8>)>,
        /// Symlinks the stub `extract` plants into the dest dir
        /// (`link_name -> target`), unix only.
        #[cfg(unix)]
        extract_links: Vec<(String, String)>,
        /// When true, `fetch` fails instead of serving.
        fetch_fails: bool,
        /// Canned `list_members` output; defaults to the extract file names.
        listed_members: Option<Vec<String>>,
    }

    impl StubTransport {
        fn serving(tarball_url: &str, tarball: &[u8], manifest_url: &str, manifest: &str) -> Self {
            let mut fetches = HashMap::new();
            fetches.insert(tarball_url.to_string(), tarball.to_vec());
            fetches.insert(manifest_url.to_string(), manifest.as_bytes().to_vec());
            Self {
                fetches,
                ..Self::default()
            }
        }

        fn spawn_count(&self, tool: &str) -> usize {
            self.spawns.iter().filter(|(name, _)| name == tool).count()
        }

        fn argv_for(&self, tool: &str) -> Vec<Vec<String>> {
            self.spawns
                .iter()
                .filter(|(name, _)| name == tool)
                .map(|(_, argv)| argv.clone())
                .collect()
        }
    }

    impl SeedTransport for StubTransport {
        fn fetch(&mut self, url: &str, dest: &Path, max_bytes: u64) -> Result<(), SeedError> {
            let argv = SystemTransport::curl_argv(url, dest, max_bytes);
            self.spawns.push(("curl".to_string(), argv));
            if self.fetch_fails {
                return Err(SeedError::Fetch("stub fetch failed".to_string()));
            }
            match self.fetches.get(url) {
                Some(bytes) => {
                    if let Some(parent) = dest.parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).expect("stub parent");
                        }
                    }
                    std::fs::write(dest, bytes).expect("stub write");
                    Ok(())
                }
                None => Err(SeedError::Fetch(format!("stub has no bytes for {url}"))),
            }
        }

        fn extract(
            &mut self,
            archive: &Path,
            dest: &Path,
            members: &[String],
        ) -> Result<(), SeedError> {
            let argv = SystemTransport::tar_argv(archive, dest, members);
            self.spawns.push(("tar".to_string(), argv));
            for (name, bytes) in &self.extract_files {
                std::fs::write(dest.join(name), bytes).expect("stub member");
            }
            #[cfg(unix)]
            for (link, target) in &self.extract_links {
                std::os::unix::fs::symlink(target, dest.join(link)).expect("stub link");
            }
            Ok(())
        }

        fn list_members(&mut self, archive: &Path) -> Result<Vec<String>, SeedError> {
            let argv = SystemTransport::tar_list_argv(archive);
            self.spawns.push(("tar".to_string(), argv));
            Ok(match self.listed_members.clone() {
                Some(listed) => listed,
                None => self
                    .extract_files
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect(),
            })
        }
    }

    /// One valid descriptor binding name/version/executable to `exe_digest`.
    fn descriptor_for(name: &str, version: &str, exe_digest: &str) -> String {
        format!(
            "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{name}\"\nsha256 = \"{exe_digest}\"\n"
        )
    }

    /// Manifest pinning `tarball_file` to `digest`.
    fn manifest_for(tarball_file: &str, digest: &str) -> String {
        format!("{digest}  {tarball_file}\n")
    }

    /// Stub serving a fully valid release for (`name`, `version`, `target`)
    /// with executable bytes `exe`; members unpack to the matching payload.
    fn valid_stub(
        name: &str,
        version: &str,
        target: &str,
        exe: &[u8],
    ) -> (StubTransport, SeedUrls) {
        let urls = seed_urls(name, version, target).expect("valid urls");
        let digest = bitty_package::integrity::sha256_hex(exe);
        // The tarball bytes are opaque to the stub extractor (it writes
        // members directly); any digest-consistent bytes do.
        let tarball = format!("fake-tarball-for-{name}-{version}-{target}").into_bytes();
        let tarball_digest = bitty_package::integrity::sha256_hex(&tarball);
        let manifest = manifest_for(&urls.tarball_file, &tarball_digest);
        let mut stub =
            StubTransport::serving(&urls.tarball_url, &tarball, &urls.manifest_url, &manifest);
        stub.extract_files = vec![
            (
                COMPONENT_DESCRIPTOR_FILE.to_string(),
                descriptor_for(name, version, &digest).into_bytes(),
            ),
            (format!("bitty-{name}"), exe.to_vec()),
        ];
        (stub, urls)
    }

    // --- host triple ----------------------------------------------------

    #[test]
    fn host_triple_is_a_valid_seed_target_when_mapped() {
        if let Some(triple) = host_target_triple() {
            assert!(valid_seed_target(triple), "{triple}");
        }
    }

    // --- version grammar (pack-policy mirror) -----------------------------

    #[test]
    fn seed_version_accepts_strict_core() {
        for valid in ["0.0.1", "0.0.23", "1.2.3", "10.20.30", "4294967295.0.0"] {
            assert!(valid_seed_version(valid), "{valid}");
        }
    }

    #[test]
    fn seed_version_rejects_everything_else() {
        for invalid in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.3-alpha",
            "1.2.3+build",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.x",
            "1.2.3/evil",
            "1.2.3/../../evil",
            "4294967296.0.0",
            "1.2.3 ",
            " 1.2.3",
            "1.2.3\n",
        ] {
            assert!(!valid_seed_version(invalid), "{invalid:?}");
        }
        assert!(!valid_seed_version(&"9".repeat(65)));
    }

    // --- target grammar (dist-script mirror) ------------------------------

    #[test]
    fn seed_target_accepts_published_triples() {
        for valid in [
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ] {
            assert!(valid_seed_target(valid), "{valid}");
        }
    }

    #[test]
    fn seed_target_rejects_hostile_shapes() {
        for invalid in [
            "",
            "..",
            "x86_64",
            "X86_64-unknown-linux-gnu",
            "x86_64|evil",
            "x86_64 evil",
            "x86_64/evil",
            "../evil",
            "x86_64-unknown-linux-gnu.tar.gz",
            "x86_64;evil",
            "x86_64$(evil)",
            "x86_64`evil`",
            "-leading-dash",
            "trailing-dash-",
            "double--dash",
        ] {
            // Each fails on case, separators, a single field, or an empty
            // field (`--` splits one empty field out).
            assert!(!valid_seed_target(invalid), "{invalid:?}");
        }
        assert!(!valid_seed_target(&"a".repeat(65)));
    }

    // --- URL builder: hostile corpus, zero spawn --------------------------

    #[test]
    fn seed_urls_builds_literal_templates() {
        let urls = seed_urls("net", "0.0.23", "x86_64-unknown-linux-gnu").expect("urls");
        assert_eq!(
            urls.tarball_url,
            "https://cdn.bitty.run/bitty/components/net/0.0.23/x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            urls.manifest_url,
            "https://cdn.bitty.run/bitty/components/net/0.0.23/SHA256SUMS"
        );
        assert_eq!(urls.tarball_file, "x86_64-unknown-linux-gnu.tar.gz");
    }

    /// Hostile corpus (mirrors the `make-component-dist.sh --print-url`
    /// hostile URLs): every input fails in the pure builder, so no spawn
    /// can follow. The stub records spawns; each case asserts zero.
    #[test]
    fn seed_urls_refuses_hostile_input_with_zero_spawn() {
        let hostile: &[(&str, &str, &str, &str)] = &[
            (
                "name with slash",
                "../evil",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name with scheme",
                "https://evil.example/x",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name absolute path",
                "/etc/passwd",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name uppercase",
                "Net",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            ("name empty", "", "0.0.23", "x86_64-unknown-linux-gnu"),
            (
                "name with space",
                "ne t",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name with semicolon",
                "net;evil",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name with backtick",
                "net`evil`",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name with dollar",
                "net$(evil)",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name with pipe",
                "net|evil",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "name with nul",
                "ne\0t",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "version with slash",
                "net",
                "0.0.23/../../evil",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "version with scheme",
                "net",
                "https://evil.example",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "version prerelease",
                "net",
                "0.0.23-alpha",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "version leading v",
                "net",
                "v0.0.23",
                "x86_64-unknown-linux-gnu",
            ),
            ("target with pipe", "net", "0.0.23", "x86_64|evil"),
            ("target with traversal", "net", "0.0.23", ".."),
            ("target with space", "net", "0.0.23", "x86_64 evil"),
            ("target with slash", "net", "0.0.23", "x86_64/evil"),
            (
                "target with suffix",
                "net",
                "0.0.23",
                "x86_64-unknown-linux-gnu.tar.gz",
            ),
            (
                "target uppercase",
                "net",
                "0.0.23",
                "X86_64-unknown-linux-gnu",
            ),
        ];
        for (why, name, version, target) in hostile {
            let mut stub = StubTransport::default();
            let error = seed_urls(name, version, target)
                .expect_err(&format!("hostile input must fail ({why})"));
            // The builder is pure: pipeline entry would fail identically,
            // and the stub proves nothing was spawned on the way there.
            let _ = fetch_seed_payload(name, version, target, &mut stub)
                .expect_err(&format!("hostile input must fail in pipeline ({why})"));
            assert!(
                stub.spawns.is_empty(),
                "hostile input spawned a child ({why}): {:?}",
                stub.spawns
            );
            let _ = error;
        }
    }

    #[test]
    fn seed_urls_are_always_cdn_pinned() {
        // No override surface exists (unlike the dist script's test-only
        // `--cdn-base`): every emitted URL carries the exact pinned base by
        // construction. A lookalike host is unrepresentable.
        for (name, version, target) in [
            ("net", "0.0.23", "x86_64-unknown-linux-gnu"),
            ("a-0-9-z", "10.20.30", "aarch64-apple-darwin"),
        ] {
            let urls = seed_urls(name, version, target).expect("urls");
            assert!(urls.tarball_url.starts_with(SEED_CDN_BASE));
            assert!(urls.manifest_url.starts_with(SEED_CDN_BASE));
            assert!(!urls.tarball_url.contains("evil"));
        }
    }

    // --- builtin installer egress (#1905 delta 2) -------------------------

    #[test]
    fn builtin_egress_constants_match_runtime_grant() {
        assert_eq!(SEED_CDN_HOST, "cdn.bitty.run");
        assert_eq!(SEED_CDN_PORT, 443);
        assert_eq!(BUILTIN_INSTALLER_EGRESS_HOST, SEED_CDN_HOST);
        assert_eq!(BUILTIN_INSTALLER_EGRESS_PORT, SEED_CDN_PORT);
        assert_eq!(BUILTIN_INSTALLER_EGRESS, "cdn.bitty.run:443");
        assert_eq!(
            BUILTIN_INSTALLER_EGRESS_HOST,
            bitty_runtime::component::BUILTIN_INSTALLER_EGRESS_HOST
        );
        assert_eq!(
            BUILTIN_INSTALLER_EGRESS_PORT,
            bitty_runtime::component::BUILTIN_INSTALLER_EGRESS_PORT
        );
        assert_eq!(
            BUILTIN_INSTALLER_EGRESS,
            bitty_runtime::component::BUILTIN_INSTALLER_EGRESS
        );
    }

    #[test]
    fn builtin_host_check_is_exact_and_case_sensitive() {
        assert!(is_builtin_installer_host("cdn.bitty.run"));
        for hostile in [
            "cdn.bitty.run.evil.com",
            "cdn.bitty.run@evil",
            "evil.com",
            "CDN.BITTY.RUN",
            "Cdn.BitTy.Run",
            "cdn.bitty.run ",
            " cdn.bitty.run",
            "",
            "cdn.bitty.run:443",
        ] {
            assert!(
                !is_builtin_installer_host(hostile),
                "hostile host must not be builtin: {hostile:?}"
            );
        }
    }

    #[test]
    fn builtin_url_check_rejects_hostile_posing() {
        for builtin in [
            "https://cdn.bitty.run/bitty/components/net/0.0.23/x.tar.gz",
            "https://cdn.bitty.run/bitty/components/net/0.0.23/SHA256SUMS",
            "https://cdn.bitty.run:443/bitty/components/net/0.0.23/x.tar.gz",
        ] {
            assert!(
                is_builtin_installer_url(builtin),
                "builtin URL must pass: {builtin}"
            );
        }
        for hostile in [
            "https://cdn.bitty.run.evil.com/bitty/components/net/0.0.23/x.tar.gz",
            "https://cdn.bitty.run@evil/bitty/components/net/0.0.23/x.tar.gz",
            "https://CDN.BITTY.RUN/bitty/components/net/0.0.23/x.tar.gz",
            "https://cdn.bitty.run:8443/bitty/components/net/0.0.23/x.tar.gz",
            "https://cdn.bitty.run:80/x",
            "http://cdn.bitty.run/x",
            "HTTPS://cdn.bitty.run/x",
            "https://evil.com/https://cdn.bitty.run/x",
            "https://evil.com/x",
            "",
        ] {
            assert!(
                !is_builtin_installer_url(hostile),
                "hostile URL must not be builtin: {hostile:?}"
            );
        }
    }

    #[test]
    fn seed_urls_are_builtin_egress() {
        let urls = seed_urls("net", "0.0.23", "x86_64-unknown-linux-gnu").expect("urls");
        assert!(is_builtin_installer_url(&urls.tarball_url));
        assert!(is_builtin_installer_url(&urls.manifest_url));
        assert!(is_builtin_installer_host(SEED_CDN_HOST));
        assert!(bitty_runtime::component::is_builtin_installer_egress(
            SEED_CDN_HOST,
            SEED_CDN_PORT
        ));
    }

    // --- dual-digest + consent levels (#1905 delta 6, #1906 direction) ----

    #[test]
    fn dual_digest_needs_both_sources_matching() {
        let digest = "a".repeat(64);
        let other = "b".repeat(64);
        // Both present and matching the bytes: verified.
        assert!(is_dual_digest_verified(Some(&digest), &digest, &digest));
        // Uppercase is normalized (mirrors manifest parsing).
        assert!(is_dual_digest_verified(
            Some(&digest.to_ascii_uppercase()),
            &digest.to_ascii_uppercase(),
            &digest
        ));
        // Single-source-only (no registry pin): confirm path, never silent.
        assert!(!is_dual_digest_verified(None, &digest, &digest));
        // Either side mismatching the bytes: not verified (mismatch is an
        // integrity failure upstream, never a silent pick).
        assert!(!is_dual_digest_verified(Some(&other), &digest, &digest));
        assert!(!is_dual_digest_verified(Some(&digest), &other, &digest));
        assert!(!is_dual_digest_verified(Some(&digest), &digest, &other));
        // Malformed digests never verify.
        assert!(!is_dual_digest_verified(Some("abc"), &digest, &digest));
        assert!(!is_dual_digest_verified(
            Some(&digest),
            "not-hex-at-all______________________________________________",
            &digest
        ));
    }

    #[test]
    fn consent_level_is_silent_only_for_builtin_and_dual() {
        assert_eq!(
            installer_consent_level(true, true),
            InstallerConsentLevel::Silent
        );
        // Third-party host, single-digest-only, or both: explicit confirm.
        assert_eq!(
            installer_consent_level(false, true),
            InstallerConsentLevel::Confirm
        );
        assert_eq!(
            installer_consent_level(true, false),
            InstallerConsentLevel::Confirm
        );
        assert_eq!(
            installer_consent_level(false, false),
            InstallerConsentLevel::Confirm
        );
    }

    // --- curl/tar argv shape ----------------------------------------------

    #[test]
    fn curl_argv_is_fixed_and_https_pinned() {
        let dest = Path::new("/tmp/out.tar.gz");
        let argv = SystemTransport::curl_argv("https://cdn.bitty.run/x", dest, 123);
        assert_eq!(
            argv,
            vec![
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--connect-timeout",
                "15",
                "--max-time",
                "120",
                "--max-filesize",
                "123",
                "--output",
                "/tmp/out.tar.gz",
                "https://cdn.bitty.run/x",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        // Bounded fetch: a stalled network/proxy fails closed instead of
        // hanging `install` silently in `Command::output()`.
        assert!(argv.contains(&"--connect-timeout".to_string()));
        assert!(argv.contains(&SEED_CURL_CONNECT_TIMEOUT_SECS.to_string()));
        assert!(argv.contains(&"--max-time".to_string()));
        assert!(argv.contains(&SEED_CURL_MAX_TIME_SECS.to_string()));
        // No shell anywhere: a single argv vector with no shell program
        // or `-c` flag, the audited URL as one final element.
        assert!(
            !argv
                .iter()
                .any(|arg| matches!(arg.as_str(), "sh" | "bash" | "cmd" | "-c"))
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("https://cdn.bitty.run/x")
        );
    }

    #[test]
    fn tar_argv_names_members_explicitly() {
        let argv = SystemTransport::tar_argv(
            Path::new("/tmp/a.tar.gz"),
            Path::new("/tmp/d"),
            &["bitty-component.toml".to_string(), "bitty-net".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "-xzf",
                "/tmp/a.tar.gz",
                "-C",
                "/tmp/d",
                "bitty-component.toml",
                "bitty-net",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }

    // --- curl floor preflight (issue #1907) --------------------------------

    #[test]
    fn curl_floor_consts_agree() {
        assert_eq!(
            SEED_CURL_FLOOR,
            format!("{SEED_CURL_FLOOR_MAJOR}.{SEED_CURL_FLOOR_MINOR}.{SEED_CURL_FLOOR_PATCH}")
        );
        assert_eq!(
            CurlVersion::floor(),
            CurlVersion {
                major: 7,
                minor: 68,
                patch: 0,
            }
        );
    }

    #[test]
    fn parse_curl_version_handles_unix_and_windows_forms() {
        // Unix forms.
        assert_eq!(
            parse_curl_version("curl 8.22.0 (x86_64-pc-linux-gnu) libcurl/8.22.0 OpenSSL/3.6.4"),
            Some(CurlVersion {
                major: 8,
                minor: 22,
                patch: 0,
            })
        );
        assert_eq!(
            parse_curl_version("curl 7.68.0 (x86_64-pc-linux-gnu) libcurl/7.68.0"),
            Some(CurlVersion {
                major: 7,
                minor: 68,
                patch: 0,
            })
        );
        // Windows suffix forms: the parenthesized marker is a later field.
        assert_eq!(
            parse_curl_version("curl 8.4.0 (Windows) libcurl/8.4.0 Schannel"),
            Some(CurlVersion {
                major: 8,
                minor: 4,
                patch: 0,
            })
        );
        assert_eq!(
            parse_curl_version("curl 7.55.1 (Windows) libcurl/7.55.1 WinSSL"),
            Some(CurlVersion {
                major: 7,
                minor: 55,
                patch: 1,
            })
        );
        // Trailing per-part suffixes parse the leading digits; a missing
        // patch defaults to zero.
        assert_eq!(
            parse_curl_version("curl 8.4.0-DEV (x86_64-pc-linux-gnu) libcurl/8.4.0-DEV"),
            Some(CurlVersion {
                major: 8,
                minor: 4,
                patch: 0,
            })
        );
        assert_eq!(
            parse_curl_version("curl 8.5 (x86_64-pc-linux-gnu) libcurl/8.5"),
            Some(CurlVersion {
                major: 8,
                minor: 5,
                patch: 0,
            })
        );
        // Hostile or malformed output fails closed (None).
        for bad in [
            "",
            "curl",
            "curl ",
            "wget 8.0.0 (x86_64-pc-linux-gnu)",
            "curl abc (x86_64-pc-linux-gnu)",
            "curl 8..0 (x86_64-pc-linux-gnu)",
            "curl 8 (x86_64-pc-linux-gnu)",
            "curl .8.0 (x86_64-pc-linux-gnu)",
        ] {
            assert!(parse_curl_version(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn curl_floor_gate_accepts_floor_and_newer_rejects_older() {
        assert!(CurlVersion::floor().meets_floor());
        for newer in [
            CurlVersion {
                major: 7,
                minor: 68,
                patch: 1,
            },
            CurlVersion {
                major: 7,
                minor: 69,
                patch: 0,
            },
            CurlVersion {
                major: 8,
                minor: 0,
                patch: 0,
            },
        ] {
            assert!(newer.meets_floor(), "{newer}");
        }
        for older in [
            CurlVersion {
                major: 7,
                minor: 67,
                patch: 9,
            },
            CurlVersion {
                major: 7,
                minor: 55,
                patch: 1,
            },
            CurlVersion {
                major: 6,
                minor: 0,
                patch: 0,
            },
        ] {
            assert!(!older.meets_floor(), "{older}");
        }
    }

    #[test]
    fn check_curl_version_output_names_found_floor_and_hint() {
        let version = check_curl_version_output("curl 8.22.0 (x86_64-pc-linux-gnu) libcurl/8.22.0")
            .expect("floor met");
        assert_eq!(
            version,
            CurlVersion {
                major: 8,
                minor: 22,
                patch: 0,
            }
        );
        let error = check_curl_version_output("curl 7.55.1 (Windows) libcurl/7.55.1 WinSSL")
            .expect_err("too old must fail");
        assert_eq!(error.exit_code(), 1);
        let message = error.message();
        assert!(message.contains("7.55.1"), "{message}");
        assert!(message.contains(SEED_CURL_FLOOR), "{message}");
        assert!(message.contains("upgrade"), "{message}");
        assert!(
            message.contains("$XDG_DATA_HOME/bitty/components/"),
            "{message}"
        );
        let error = check_curl_version_output("not curl output").expect_err("unparsable must fail");
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn preflight_curl_runs_once_per_invocation() {
        let before = CURL_PREFLIGHT_SPAWNS.load(std::sync::atomic::Ordering::Relaxed);
        let first = preflight_curl();
        let middle = CURL_PREFLIGHT_SPAWNS.load(std::sync::atomic::Ordering::Relaxed);
        let second = preflight_curl();
        let after = CURL_PREFLIGHT_SPAWNS.load(std::sync::atomic::Ordering::Relaxed);
        // Both calls agree (the second is a cache hit), and at most one
        // spawn happened here no matter how many fetches follow.
        assert_eq!(format!("{first:?}"), format!("{second:?}"));
        assert!(middle - before <= 1, "preflight must spawn at most once");
        assert_eq!(middle, after, "second preflight must be a cache hit");
    }

    // --- CA + proxy posture pin (issue #1907) -------------------------------

    #[test]
    fn curl_argv_pins_ca_proxy_tls_posture() {
        let dest = Path::new("/tmp/out.tar.gz");
        let argv = SystemTransport::curl_argv("https://cdn.bitty.run/x", dest, 123);
        // System CA store only: no `--cacert` / `--capath` override.
        for banned in ["--cacert", "--capath"] {
            assert!(
                !argv.iter().any(|arg| arg == banned),
                "argv must not contain {banned}"
            );
        }
        // No custom TLS in Core: no `--tlsv*` / `--tls-max` / `--ciphers`.
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--ciphers" || arg == "--tls-max" || arg.starts_with("--tlsv")),
            "argv must not carry custom TLS flags: {argv:?}"
        );
        // System proxy via inherited env only: no `--proxy` flag.
        assert!(
            !argv.iter().any(|arg| arg == "--proxy"),
            "argv must not pin a proxy flag: {argv:?}"
        );
        // The floor drivers are all present.
        for required in [
            "--proto",
            "=https",
            "--connect-timeout",
            "--max-time",
            "--max-filesize",
        ] {
            assert!(
                argv.iter().any(|arg| arg == required),
                "argv must contain {required}"
            );
        }
    }

    #[test]
    fn secret_env_names_cover_tokens_keys_secrets_not_proxies() {
        for secret in [
            "GITHUB_TOKEN",
            "github_token",
            "AWS_SECRET_ACCESS_KEY",
            "CARGO_REGISTRY_TOKEN",
            "BITTY_API_KEY",
            "DB_SECRET",
            "A_B_C_SECRET",
        ] {
            assert!(is_secret_env_name(secret), "{secret}");
        }
        for plain in [
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
            "PATH",
            "HOME",
            "SSL_CERT_FILE",
            "CURL_CA_BUNDLE",
            "TOKENIZER",
            "KEYBOARD",
            "SECRETARY",
        ] {
            assert!(!is_secret_env_name(plain), "{plain}");
        }
    }

    #[test]
    fn missing_tool_diagnostics_name_manual_placement_without_staging() {
        let curl = curl_missing_diagnostic();
        assert!(curl.contains("curl"), "{curl}");
        assert!(curl.contains(SEED_CURL_FLOOR), "{curl}");
        assert!(curl.contains("$XDG_DATA_HOME/bitty/components/"), "{curl}");
        assert!(curl.contains("nothing was fetched or staged"), "{curl}");
        let tar = tar_missing_diagnostic();
        assert!(tar.contains("tar"), "{tar}");
        assert!(tar.contains("$XDG_DATA_HOME/bitty/components/"), "{tar}");
        assert!(tar.contains("nothing was fetched or staged"), "{tar}");
        assert_eq!(SeedError::ToolMissing(curl).exit_code(), 1);
        assert_eq!(SeedError::CurlTooOld("x".to_string()).exit_code(), 1);
    }

    /// Serializes process-env observation (the environment is process-global;
    /// parallel tests must not race on it). This suite never mutates the
    /// process environment (the crate forbids `unsafe_code`, and
    /// `std::env::set_var` is `unsafe`): the live-poison proof runs in the
    /// `fetch_env` CLI suite, which poisons the child environment through
    /// `Command::env` instead.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn hostile_env_secrets_scrubbed_while_proxy_and_fetch_survive() {
        let _locked = ENV_TEST_LOCK.lock().expect("env lock");
        // Hostile map: secrets that must be scrubbed, proxy/CA/ordinary
        // vars that must survive, and a canary proving inheritance.
        let present = [
            "BITTY_CTX1112_GITHUB_TOKEN",
            "BITTY_CTX1112_AWS_SECRET_ACCESS_KEY",
            "BITTY_CTX1112_API_KEY",
            "GITHUB_TOKEN",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "no_proxy",
            "PATH",
            "SSL_CERT_FILE",
            "BITTY_CTX1112_CANARY",
        ];
        let removals = secret_removals_in(present.iter().map(|name| (*name).to_string()));
        for secret in [
            "BITTY_CTX1112_GITHUB_TOKEN",
            "BITTY_CTX1112_AWS_SECRET_ACCESS_KEY",
            "BITTY_CTX1112_API_KEY",
            "GITHUB_TOKEN",
        ] {
            assert!(
                removals.iter().any(|listed| listed == secret),
                "scrub must cover {secret}"
            );
        }
        for kept in [
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "no_proxy",
            "PATH",
            "SSL_CERT_FILE",
            "BITTY_CTX1112_CANARY",
        ] {
            assert!(
                !removals.iter().any(|listed| listed == kept),
                "scrub must keep {kept}"
            );
        }
        // The stub pipeline is unaffected by secret-shaped names existing
        // (scrubbing happens at the spawn layer, below).
        let exe = b"hostile-env-bytes";
        let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        let payload = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect("fetch works with secret-shaped names present");
        assert_eq!(payload.bytes, exe);
        // A real scrubbed child still runs (skip when curl is absent: the
        // fail-closed path is covered elsewhere).
        match scrubbed_command("curl").arg("--version").output() {
            Ok(output) => assert!(output.status.success()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("SKIP: curl not found (scrubbed-spawn proof needs curl)");
            }
            Err(error) => panic!("scrubbed `curl --version` failed: {error}"),
        }
    }

    // --- manifest: tamper corpus -------------------------------------------

    #[test]
    fn manifest_finds_the_pinned_entry() {
        let manifest = format!(
            "{a}  aarch64-unknown-linux-gnu.tar.gz\n{b}  x86_64-unknown-linux-gnu.tar.gz\n",
            a = "a".repeat(64),
            b = "b".repeat(64),
        );
        assert_eq!(
            find_tarball_digest(&manifest, "x86_64-unknown-linux-gnu.tar.gz").expect("entry"),
            "b".repeat(64)
        );
    }

    #[test]
    fn manifest_refuses_tamper_shapes() {
        let good_hex = "c".repeat(64);
        let cases: &[(&str, &str, &str)] = &[
            (
                "missing entry",
                &format!("{good_hex}  other.tar.gz\n"),
                "x86_64-unknown-linux-gnu.tar.gz",
            ),
            ("empty manifest", "", "x86_64-unknown-linux-gnu.tar.gz"),
            ("short digest", "abc  x.tar.gz\n", "x.tar.gz"),
            (
                "uppercase digest accepted but normalized",
                &format!("{}  x.tar.gz\n", "C".repeat(64)),
                "x.tar.gz",
            ),
            (
                "duplicate entries",
                &format!("{good_hex}  x.tar.gz\n{good_hex}  x.tar.gz\n"),
                "x.tar.gz",
            ),
            (
                "path entry",
                &format!("{good_hex}  subdir/x.tar.gz\n"),
                "x.tar.gz",
            ),
            ("no separator", &format!("{good_hex}x.tar.gz\n"), "x.tar.gz"),
        ];
        for (why, manifest, file) in cases {
            if *why == "uppercase digest accepted but normalized" {
                assert_eq!(
                    find_tarball_digest(manifest, file).expect("uppercase hex"),
                    good_hex,
                    "{why}"
                );
                continue;
            }
            assert!(
                find_tarball_digest(manifest, file).is_err(),
                "manifest must fail ({why})"
            );
        }
    }

    // --- pipeline: happy path + tamper/downgrade ----------------------------

    #[test]
    fn fetch_seed_payload_verifies_and_unpacks() {
        let exe = b"fake-net-bytes";
        let (mut stub, urls) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        let payload = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect("payload");
        assert_eq!(payload.name, "net");
        assert_eq!(payload.version, "0.0.1");
        assert_eq!(payload.executable, "bitty-net");
        assert_eq!(payload.bytes, exe);
        assert_eq!((payload.protocol_min, payload.protocol_max), (1, 1));
        // Exactly one manifest fetch plus one tarball fetch, one member
        // list, then one extraction: no registry chatter, no extra downloads.
        assert_eq!(stub.spawn_count("curl"), 2);
        assert_eq!(stub.spawn_count("tar"), 2);
        let curl_argvs = stub.argv_for("curl");
        assert!(curl_argvs[0].last().unwrap().ends_with("SHA256SUMS"));
        assert!(curl_argvs[1].last().unwrap().ends_with(&urls.tarball_file));
        let tar_argvs = stub.argv_for("tar");
        assert_eq!(tar_argvs[0][0], "-tzf");
        assert_eq!(tar_argvs[1][0], "-xzf");
        assert!(tar_argvs[1].contains(&"bitty-component.toml".to_string()));
        assert!(tar_argvs[1].contains(&"bitty-net".to_string()));
    }

    #[test]
    fn fetch_seed_payload_refuses_tampered_tarball() {
        let exe = b"fake-net-bytes";
        let (mut stub, urls) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        // Serve different bytes than the manifest pins.
        stub.fetches
            .insert(urls.tarball_url.clone(), b"tampered-bytes".to_vec());
        let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect_err("tamper must fail");
        assert_eq!(error.exit_code(), 4);
        assert!(error.message().contains("does not match"), "{error}");
        // Refused before unpacking: tar never ran.
        assert_eq!(stub.spawn_count("tar"), 0);
    }

    #[test]
    fn fetch_seed_payload_refuses_manifest_without_entry() {
        let exe = b"fake-net-bytes";
        let (mut stub, urls) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        stub.fetches.insert(
            urls.manifest_url.clone(),
            format!("{}  some-other.tar.gz\n", "d".repeat(64)).into_bytes(),
        );
        let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect_err("missing entry must fail");
        assert_eq!(error.exit_code(), 4);
        assert!(error.message().contains("no entry"), "{error}");
        assert_eq!(stub.spawn_count("tar"), 0);
    }

    #[test]
    fn fetch_seed_payload_refuses_substituted_descriptor() {
        // Downgrade/substitution: the payload claims a different version
        // than requested (or a different component entirely).
        for (why, desc_name, desc_version) in [
            ("older version", "net", "0.0.0"),
            ("newer version", "net", "0.0.2"),
            ("other component", "evil", "0.0.1"),
        ] {
            let exe = b"fake-net-bytes";
            let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
            let digest = bitty_package::integrity::sha256_hex(exe);
            stub.extract_files = vec![
                (
                    COMPONENT_DESCRIPTOR_FILE.to_string(),
                    descriptor_for(desc_name, desc_version, &digest).into_bytes(),
                ),
                ("bitty-net".to_string(), exe.to_vec()),
            ];
            let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
                .expect_err(&format!("substitution must fail ({why})"));
            assert_eq!(error.exit_code(), 4, "{why}: {error}");
            assert!(
                error.message().contains("refusing to install"),
                "{why}: {error}"
            );
        }
    }

    #[test]
    fn fetch_seed_payload_refuses_descriptor_digest_tamper() {
        let exe = b"fake-net-bytes";
        let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        // Descriptor pins a digest that is not the member's.
        stub.extract_files = vec![
            (
                COMPONENT_DESCRIPTOR_FILE.to_string(),
                descriptor_for("net", "0.0.1", &"e".repeat(64)).into_bytes(),
            ),
            ("bitty-net".to_string(), exe.to_vec()),
        ];
        let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect_err("digest tamper must fail");
        assert_eq!(error.exit_code(), 4);
        assert!(error.message().contains("tampered"), "{error}");
    }

    #[test]
    fn fetch_seed_payload_refuses_extra_member() {
        let exe = b"fake-net-bytes";
        let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        stub.extract_files
            .push(("evil.sh".to_string(), b"evil".to_vec()));
        let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect_err("extra member must fail");
        assert_eq!(error.exit_code(), 4);
        assert!(
            error.message().contains("unexpected archive members"),
            "{error}"
        );
        // Refused at the member-list audit: extraction never runs.
        assert!(
            stub.argv_for("tar").iter().all(|argv| argv[0] != "-xzf"),
            "extraction must not run after a failed member audit"
        );
    }

    #[test]
    fn fetch_seed_payload_refuses_hostile_member_lists() {
        // The member-list audit compares the exact multiset with no
        // normalization; every deviation fails before extraction.
        let hostile: &[(&str, Vec<&str>)] = &[
            (
                "absolute member",
                vec!["/tmp/evil", "bitty-component.toml", "bitty-net"],
            ),
            (
                "traversal member",
                vec!["../evil", "bitty-component.toml", "bitty-net"],
            ),
            (
                "dot-slash member",
                vec!["./bitty-net", "bitty-component.toml", "bitty-net"],
            ),
            ("missing member", vec!["bitty-component.toml"]),
            (
                "duplicate member",
                vec!["bitty-component.toml", "bitty-component.toml", "bitty-net"],
            ),
            ("empty list", vec![]),
            ("swapped names", vec!["bitty-component.toml", "bitty-evil"]),
        ];
        for (why, listed) in hostile {
            let exe = b"fake-net-bytes";
            let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
            stub.listed_members = Some(listed.iter().map(|name| (*name).to_string()).collect());
            let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
                .expect_err(&format!("hostile member list must fail ({why})"));
            assert_eq!(error.exit_code(), 4, "{why}: {error}");
            assert!(
                stub.argv_for("tar").iter().all(|argv| argv[0] != "-xzf"),
                "extraction must not run after a failed member audit ({why})"
            );
        }
    }

    #[test]
    fn fetch_seed_payload_refuses_missing_member() {
        let exe = b"fake-net-bytes";
        let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        stub.extract_files.retain(|(name, _)| name != "bitty-net");
        let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect_err("missing member must fail");
        assert_eq!(error.exit_code(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn fetch_seed_payload_refuses_symlinked_member() {
        let exe = b"fake-net-bytes";
        let (mut stub, _) = valid_stub("net", "0.0.1", "x86_64-unknown-linux-gnu", exe);
        // A tar-impl that follows links would plant these: the member list
        // still names the expected payload, so the failure lands in the
        // post-extraction no-follow audit.
        stub.listed_members = Some(vec![
            COMPONENT_DESCRIPTOR_FILE.to_string(),
            "bitty-net".to_string(),
        ]);
        stub.extract_files.clear();
        stub.extract_links = vec![
            ("bitty-net".to_string(), "/etc/passwd".to_string()),
            (
                COMPONENT_DESCRIPTOR_FILE.to_string(),
                "/etc/hostname".to_string(),
            ),
        ];
        let error = fetch_seed_payload("net", "0.0.1", "x86_64-unknown-linux-gnu", &mut stub)
            .expect_err("symlinked members must fail");
        assert_eq!(error.exit_code(), 4);
        assert!(error.message().contains("not a regular file"), "{error}");
    }

    #[test]
    fn seed_errors_map_to_stable_exit_codes() {
        assert_eq!(SeedError::InvalidName("x".into()).exit_code(), 2);
        assert_eq!(SeedError::InvalidVersion("x".into()).exit_code(), 2);
        assert_eq!(SeedError::InvalidTarget("x".into()).exit_code(), 1);
        assert_eq!(SeedError::UnsupportedHost("x".into()).exit_code(), 1);
        assert_eq!(SeedError::ToolMissing("x".into()).exit_code(), 1);
        assert_eq!(SeedError::CurlTooOld("x".into()).exit_code(), 1);
        assert_eq!(SeedError::Fetch("x".into()).exit_code(), 1);
        assert_eq!(SeedError::Io("x".into()).exit_code(), 1);
        assert_eq!(SeedError::Manifest("x".into()).exit_code(), 4);
        assert_eq!(SeedError::DigestMismatch("x".into()).exit_code(), 4);
        assert_eq!(SeedError::MemberViolation("x".into()).exit_code(), 4);
        assert_eq!(SeedError::DescriptorMismatch("x".into()).exit_code(), 4);
    }

    #[test]
    fn staging_dirs_are_removed() {
        let path = {
            let stage = ScopedDir::create("cleanup").expect("stage");
            assert!(stage.path.is_dir());
            stage.path.clone()
        };
        assert!(!path.exists(), "staging dir must be removed on drop");
    }

    #[test]
    fn staging_dirs_are_owner_only() {
        #[cfg(unix)]
        let stage = ScopedDir::create("mode").expect("stage");
        #[cfg(not(unix))]
        let _stage = ScopedDir::create("mode").expect("stage");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&stage.path)
                .expect("stage metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "staging dir must be owner-only");
        }
    }

    // --- real-tar interop ---------------------------------------------------
    //
    // The stub extractor above proves the audit logic; these tests prove the
    // production `tar` argv against genuine archives. `tar` is a production
    // dependency of `install`, and every CI OS ships one, so its absence
    // skips with a note (same convention as the shell fixture tests).

    /// Fetch stub + REAL system tar extraction.
    #[derive(Debug, Default)]
    struct RealTarTransport {
        fetches: HashMap<String, Vec<u8>>,
    }

    impl SeedTransport for RealTarTransport {
        fn fetch(&mut self, url: &str, dest: &Path, _max: u64) -> Result<(), SeedError> {
            match self.fetches.get(url) {
                Some(bytes) => {
                    if let Some(parent) = dest.parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).expect("stub parent");
                        }
                    }
                    std::fs::write(dest, bytes).expect("stub write");
                    Ok(())
                }
                None => Err(SeedError::Fetch(format!("stub has no bytes for {url}"))),
            }
        }

        fn extract(
            &mut self,
            archive: &Path,
            dest: &Path,
            members: &[String],
        ) -> Result<(), SeedError> {
            SystemTransport.extract(archive, dest, members)
        }

        fn list_members(&mut self, archive: &Path) -> Result<Vec<String>, SeedError> {
            SystemTransport.list_members(archive)
        }
    }

    fn system_tar_present() -> bool {
        std::process::Command::new("tar")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn seed_scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-seed-realtar-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn real_tar_roundtrip_installs_genuine_dist_layout() {
        if !system_tar_present() {
            eprintln!("SKIP: tar not found (production dependency of install)");
            return;
        }
        let base = seed_scratch("roundtrip");
        let target = "x86_64-unknown-linux-gnu";
        let exe = b"genuine-dist-bytes";
        let exe_digest = bitty_package::integrity::sha256_hex(exe);
        let src = base.join("src");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::write(src.join("bitty-net"), exe).expect("exe");
        std::fs::write(
            src.join(COMPONENT_DESCRIPTOR_FILE),
            descriptor_for("net", "0.0.23", &exe_digest),
        )
        .expect("descriptor");
        // Pack exactly like `make-component-dist.sh`: descriptor first,
        // then the executable, no wrapper directory.
        let tarball_path = base.join("payload.tar.gz");
        let pack = std::process::Command::new("tar")
            .args([
                "-czf",
                &tarball_path.to_string_lossy(),
                "-C",
                &src.to_string_lossy(),
                COMPONENT_DESCRIPTOR_FILE,
                "bitty-net",
            ])
            .output()
            .expect("pack");
        assert!(pack.status.success(), "pack failed: {pack:?}");
        let tarball = std::fs::read(&tarball_path).expect("tarball");
        let urls = seed_urls("net", "0.0.23", target).expect("urls");
        let manifest = manifest_for(
            &urls.tarball_file,
            &bitty_package::integrity::sha256_hex(&tarball),
        );
        let mut transport = RealTarTransport::default();
        transport.fetches.insert(urls.tarball_url.clone(), tarball);
        transport
            .fetches
            .insert(urls.manifest_url.clone(), manifest.into_bytes());

        let payload = fetch_seed_payload("net", "0.0.23", target, &mut transport).expect("payload");
        assert_eq!(payload.name, "net");
        assert_eq!(payload.version, "0.0.23");
        assert_eq!(payload.executable, "bitty-net");
        assert_eq!(payload.bytes, exe);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn real_tar_extra_member_is_refused() {
        if !system_tar_present() {
            eprintln!("SKIP: tar not found (production dependency of install)");
            return;
        }
        let base = seed_scratch("extra");
        let target = "x86_64-unknown-linux-gnu";
        let exe = b"genuine-dist-bytes";
        let exe_digest = bitty_package::integrity::sha256_hex(exe);
        let src = base.join("src");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::write(src.join("bitty-net"), exe).expect("exe");
        std::fs::write(
            src.join(COMPONENT_DESCRIPTOR_FILE),
            descriptor_for("net", "0.0.23", &exe_digest),
        )
        .expect("descriptor");
        std::fs::write(src.join("evil.sh"), b"evil").expect("stray");
        let tarball_path = base.join("payload.tar.gz");
        let pack = std::process::Command::new("tar")
            .args([
                "-czf",
                &tarball_path.to_string_lossy(),
                "-C",
                &src.to_string_lossy(),
                COMPONENT_DESCRIPTOR_FILE,
                "bitty-net",
                "evil.sh",
            ])
            .output()
            .expect("pack");
        assert!(pack.status.success(), "pack failed: {pack:?}");
        let tarball = std::fs::read(&tarball_path).expect("tarball");
        let urls = seed_urls("net", "0.0.23", target).expect("urls");
        let manifest = manifest_for(
            &urls.tarball_file,
            &bitty_package::integrity::sha256_hex(&tarball),
        );
        let mut transport = RealTarTransport::default();
        transport.fetches.insert(urls.tarball_url.clone(), tarball);
        transport
            .fetches
            .insert(urls.manifest_url.clone(), manifest.into_bytes());

        // The stray member fails the member-list audit even though the
        // explicit-member extraction would never materialize it: the seed
        // pins the exact dist contract end to end.
        let error = fetch_seed_payload("net", "0.0.23", target, &mut transport)
            .expect_err("stray member must fail");
        assert_eq!(error.exit_code(), 4);
        assert!(
            error.message().contains("unexpected archive members"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
