//! `bitty extension`: CLI-first native binary extension management (issue #1651).
//!
//! Renamed from `bitty component` by issue #1959: `component` stays as a
//! hidden deprecated alias (same handlers, stderr deprecation note, kept at
//! least through 0.1.0) and never appears in help or shell completions.
//!
//! Source transparency (issue #1986): `show-source` prints the exact pinned
//! bytes of the installed extension (fail-closed on missing installs and
//! hash mismatches, provenance on stderr so stdout stays byte-identical for
//! piping/hashing); `install` renders an update-diff preview when the fetch
//! would change the TOFU pin (preview only, existing consent/`--force`
//! semantics unchanged, diff computed locally from fetched-then-pinned
//! bytes, no network); dist validation refuses minified/obfuscated Lua and
//! descriptors via the documented readability heuristic (max 1000 bytes per
//! line, 200 average; see `is_obfuscated_source_text` and
//! `scripts/make-component-dist.sh`).
//!
//! Search priority: user `$XDG_DATA_HOME/bitty/extensions/` wins over system
//! `/usr/lib/bitty/extensions/` (or platform equivalent). The user tier is
//! writable without privilege; the system tier is read-only for this command.
//!
//! # Contract (implemented, v1 local-path only)
//!
//! - Shape: `bitty extension list|add|install|remove|show-source` with `list` accepting
//!   `--format table|json|jsonl` (default table) and `--no-color`.
//! - `list`: read-only enumeration across both tiers (merged, user shadows
//!   system on the same version). Shows name, version, active marker, source,
//!   ABI compat, path, and the last activation transition the broker recorded
//!   for that component name (`installed`, `active`, `denied:<class>`,
//!   `crashed`, `stopped`; `installed` when no record exists). The label is
//!   diagnostics, not a liveness probe: `active` means a spawn was recorded.
//!   It is printed once per component, on the active-version row. The state is
//!   read through the runtime's single activation reader, never re-derived
//!   here. Empty when nothing is installed; never fails on a missing root.
//! - `add <path> [--version <semver>]`: stage a pre-built binary from a local
//!   path into the user tier only. A directory must hold a source
//!   `bitty-component.toml` (name, version, protocol, executable, optional
//!   `sha256` that must match) plus the executable; a bare `bitty-<name>`
//!   executable takes its version from required `--version` and the protocol
//!   range Core supports. The digest is computed, ABI compat is verified
//!   before any mutation, and the executable is published atomically to
//!   `<user>/<name>/<version>/` with an installed descriptor plus `current`
//!   (each destination via temp write, fsync, and rename like the
//!   session/KV backends; `current` only after the executable and the
//!   descriptor are in place, so a crash mid-publish never exposes a
//!   partial component).
//!   Re-adding an installed version with a different digest fails. No network
//!   fetch, no registry download: URL operands fail closed.
//! - `remove <name> [<version>]`: remove one user-installed version, or every
//!   user version when none is named; removing the version named by `current`
//!   also removes `current`. Never touches the system tier (needs a package
//!   manager); a system-only component fails with an actionable diagnostic.
//! - `show-source <name> [<version>]`: print the exact pinned bytes of the
//!   installed executable to stdout (the version defaults to the active
//!   `current` pointer). The executable hash is re-verified against the
//!   installed descriptor before anything is printed; a missing install or
//!   a hash mismatch fails closed (exit 4) with nothing on stdout.
//!   Provenance (name, version, source tier, path, sha256) goes to stderr so
//!   stdout stays byte-identical for piping and hashing.
//! - `install <name> [--version <X.Y.Z>] [--digest <hex>] [--force] [--yes]`:
//!   fetch one prebuilt release from R2 through the minimal install seed
//!   (`crate::component_seed`, issue #1791 option A): fixed-argv system
//!   `curl` 7.68.0 or newer (preflighted, pinned to `https://cdn.bitty.run`,
//!   `SHA256SUMS` hash verify fail-closed with all-available-must-match
//!   against the registry catalog digest and `--digest` when present (#1906,
//!   #2111, constant-shape compare, exit 4 on any mismatch), exact member
//!   audit, TOFU pin of the verified CDN digest into
//!   `<user>/<name>/tarball.sha256` (later differing digests fail closed as
//!   pin-mismatch unless `--force` re-pins, never auto-updated), then the
//!   same atomic publish path as `add`.
//!
//!   Version selection (#2381): `--version X.Y.Z` pins exactly; omitting it
//!   resolves the *latest* release for the exact `(name, host target)` from
//!   the bounded `extensions/catalog.v1` metadata, choosing the highest
//!   strict `X.Y.Z` and never a prerelease (the pack grammar publishes none).
//!   The catalog is fetched after consent, the resolved version is printed
//!   before the release download, and existing consent/digest/TOFU rules are
//!   unchanged. Catalog unavailable (offline, or 404 while the publication
//!   jobs catch up) or a missing entry fails closed with a `--version` hint;
//!   a version is never guessed from directory names. Latest is a
//!   convenience: the trust decision stays the TLS catalog plus the digest
//!   chain, not the absence of a pin.
//! - Name aliases (#2381): `list`, `install`, `remove`, and `show-source`
//!   accept one repo-style `bitty-` prefix (`bitty-net` resolves to
//!   `net`) because extension repositories carry the workspace prefix
//!   while stored component names do not. Resolution is display/selection
//!   only: stored names, directories, and TOFU pins are never rewritten, and
//!   an alias whose both spellings are known fails closed as ambiguous.
//! - Class: local-only (no instance, no IPC, no component code is ever
//!   loaded or executed; safe-mode clean).
//!
//! # Install-source decision (v1)
//!
//! `add` stages from local paths only. Network fetch and registry/download
//! sources are a follow-up (DIR-030 D6: no automatic download in v1). A URL
//! operand fails closed naming the local-path form. This keeps Core
//! network-free (package-manager-boundary DIR-016/DIR-017) and install
//! offline-capable. The narrow exception is `install`: the minimal seed
//! fetches the catalog plus exactly one allowlisted R2 release (fixed-argv
//! `curl`, no shell, hash-verified before unpack), and nothing else in Core
//! touches the network.
//!
//! # Exit codes (stable taxonomy, cli-contract-rfc.md)
//!
//! - `0` success (including idempotent re-add and empty list).
//! - `1` filesystem failure after validation (copy/write/remove I/O).
//! - `2` usage error (missing/unknown verb, missing operand, unknown flag,
//!   bad `--format`/`--version`/`--digest`, stray `--`, `--version`/`--format`/
//!   `--digest`/`--force`/`--yes` on the wrong verb).
//! - `4` component error (unknown/invalid name, missing component, ABI
//!   mismatch, digest mismatch, pin-mismatch, system-only remove, remote source).
//!
//! # Bounds (fail closed before any filesystem mutation)
//!
//! - Raw tokens: 1..=[`MAX_COMPONENT_TOKEN_BYTES`] bytes, no NUL.
//! - `--format`: 1..=[`MAX_COMPONENT_FORMAT_BYTES`] bytes.
//! - `--version`: 1..=[`MAX_COMPONENT_VERSION_BYTES`] bytes.
//! - `--digest`: 1..=[`MAX_COMPONENT_DIGEST_BYTES`] bytes, no NUL, plus 64 hex shape.
//! - Source path: 1..=[`MAX_COMPONENT_PATH_BYTES`] bytes, no NUL.
//! - Descriptor: [`COMPONENT_DESCRIPTOR_MAX_BYTES`] bytes (via runtime).
//! - Executable: [`COMPONENT_EXECUTABLE_MAX_BYTES`] bytes (via runtime).
//! - TOFU pin file: [`MAX_PIN_FILE_BYTES`] bytes (`tarball.sha256`).

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bitty_runtime::component::{
    ActivationState, COMPONENT_DESCRIPTOR_FILE, COMPONENT_EXECUTABLE_PREFIX, ComponentDescriptor,
    ComponentSummary, component_search_roots_for, component_state_root_for, data_home_for,
    discover_components, executable_file_name, incompatible_component_hint,
    read_component_activation_with_state, remove_activation_artifacts, validate_component_name,
};

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success (including idempotent no-ops and empty list).
pub const EXIT_OK: i32 = 0;
/// Filesystem failure after validation.
pub const EXIT_GENERIC: i32 = 1;
/// Usage error (`cli-contract-rfc.md` class `UsageError`).
pub const EXIT_USAGE: i32 = 2;
/// Component-level failure (missing, incompatible, digest, system-only).
pub const EXIT_COMPONENT: i32 = 4;

/// Stderr deprecation note for the hidden `component` alias (issue #1959).
///
/// The dispatch prints this to stderr (never stdout, so `--format json`
/// stays parseable) whenever the subcommand was invoked as
/// `bitty component ...`. The alias keeps full behavior at least through
/// 0.1.0; new scripts should use `bitty extension ...`.
#[must_use]
pub fn component_deprecated_note() -> String {
    "warning: `bitty component` is deprecated, use `bitty extension` instead (hidden alias kept through 0.1.0)".to_string()
}

/// Maximum bytes for one raw token (verb, name, flag value).
pub const MAX_COMPONENT_TOKEN_BYTES: usize = 256;
/// Maximum bytes for a `--format` value.
pub const MAX_COMPONENT_FORMAT_BYTES: usize = 16;
/// Maximum bytes for a `--version` value.
pub const MAX_COMPONENT_VERSION_BYTES: usize = 64;
/// Maximum bytes for a source path operand.
pub const MAX_COMPONENT_PATH_BYTES: usize = 4096;
/// Maximum bytes read from one install-consent answer line (mirrors
/// `plugin.rs` `MAX_CONSENT_LINE_BYTES`).
pub const MAX_COMPONENT_CONSENT_LINE_BYTES: usize = 64;
/// Install-consent attempts before aborting (mirrors `plugin.rs`
/// `MAX_CONSENT_ATTEMPTS`).
pub const MAX_COMPONENT_CONSENT_ATTEMPTS: usize = 3;
/// Maximum bytes for a `--digest` value (64 hex chars, bounded like tokens).
pub const MAX_COMPONENT_DIGEST_BYTES: usize = 256;
/// Maximum bytes for a `--distro` value (mirrors the WSL module bound).
pub const MAX_COMPONENT_DISTRO_BYTES: usize = 64;
/// TOFU pin file name inside `<user>/<name>/` holding the verified CDN
/// tarball digest (`<hex>\n`, written atomically like `current`).
pub const TARBALL_PIN_FILE: &str = "tarball.sha256";
/// Maximum bytes read from a TOFU pin file (64 hex plus newline and slack).
pub const MAX_PIN_FILE_BYTES: u64 = 128;

/// Repo-style prefix accepted as an extension-name alias (#2381).
///
/// Extension repositories carry the workspace prefix (`bitty-net`) while
/// stored component names do not (`graphics`). The alias is accepted wherever
/// a name operand is taken; it never rewrites a stored name or TOFU pin.
pub const COMPONENT_ALIAS_PREFIX: &str = "bitty-";

/// First-party extension names known without a registry index (#2381 hints).
///
/// These seed the typo suggestion for install/remove/show-source and for the
/// list filter when nothing is installed locally. The catalog stays
/// authoritative for releases; this list is only a hint.
pub const KNOWN_BUILTIN_EXTENSIONS: [&str; 3] = ["graphics", "net", "ai"];

/// One-line pointer shown after a usage error instead of the full usage block.
///
/// Full usage stays behind `--help` (owner directive, #2381): errors keep a
/// targeted fix plus this pointer, never the whole wall of text.
pub const COMPONENT_USAGE_POINTER: &str =
    "hint: `bitty extension --help` lists the full usage, exit codes, and examples";

// ---------------------------------------------------------------------------
// Parsed request (headless, bounded, fail-closed)
// ---------------------------------------------------------------------------

/// Output shape for `list` (`--format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentFormat {
    /// Human table (default).
    Table,
    /// Versioned JSON envelope on stdout.
    Json,
    /// Same envelope, one line (identical for this command).
    Jsonl,
}

impl ComponentFormat {
    /// Parse `--format` (`None` means the table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(value) => {
                if value.len() > MAX_COMPONENT_FORMAT_BYTES || value.contains('\0') {
                    return Err(format!(
                        "bitty extension: unknown --format {value:?} (want table|json|jsonl)"
                    ));
                }
                match value.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty extension: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

/// One `bitty extension` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentVerb {
    /// `list`: enumerate installed components across both tiers.
    List,
    /// `add <path> [--version <semver>]`: stage a local binary.
    Add,
    /// `install <name> --version <X.Y.Z>`: fetch one R2 release via seed.
    Install,
    /// `remove <name> [<version>]`: drop a user-installed component.
    Remove,
    /// `show-source <name> [<version>]`: print the pinned executable bytes.
    ShowSource,
}

impl ComponentVerb {
    /// Every extension verb in help order (single source for parsing and
    /// shell completion; never duplicate this list).
    pub const VERBS: &[(&str, Self)] = &[
        ("list", Self::List),
        ("add", Self::Add),
        ("install", Self::Install),
        ("remove", Self::Remove),
        ("show-source", Self::ShowSource),
    ];

    /// Parse a verb token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::VERBS
            .iter()
            .find(|(name, _)| *name == token)
            .map(|(_, verb)| *verb)
    }

    /// Canonical verb name for output/errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Add => "add",
            Self::Install => "install",
            Self::Remove => "remove",
            Self::ShowSource => "show-source",
        }
    }
}

/// Validated request for one `bitty extension` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRequest {
    /// Requested verb.
    pub verb: ComponentVerb,
    /// First operand: source path for `add`, component name for
    /// `install`/`remove`/`show-source`.
    pub operand: Option<String>,
    /// Second operand: version for `remove`/`show-source` (no flag form).
    pub version_operand: Option<String>,
    /// `--version <semver>` for `add` from a bare executable; required
    /// `X.Y.Z` for `install`.
    pub version_flag: Option<String>,
    /// Output shape for `list`.
    pub format: ComponentFormat,
    /// `--no-color` (accepted for parity; tables are plain text).
    pub no_color: bool,
    /// `install --yes`: approve the download consent non-interactively.
    pub yes: bool,
    /// `install --digest <hex>`: caller-supplied expected tarball digest.
    /// All-available-must-match with CDN `SHA256SUMS` and the registry
    /// catalog; malformed fails as usage.
    pub digest: Option<String>,
    /// `install --force`: re-pin the TOFU pin to the new verified digest.
    /// Never bypasses digest verification; only overwrites the pin after
    /// all-available-must-match passes.
    pub force: bool,
    /// `add --distro <name>`: WSL2 guest distro for component execution on
    /// Windows (explicit selection; `default` means the `wsl.exe` default
    /// distro). Parsed for `install` only to reject it with a usage error
    /// naming `add` (install fetches the host triple and cannot stage a guest
    /// helper). Windows-only in effect: Unix accepts and validates the value
    /// but stays native (no-op).
    pub distro: Option<String>,
}

/// Why parsing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentParseError {
    /// `-h`/`--help` was requested.
    Help,
    /// Usage-level failure (message already user-facing).
    Usage(String),
}

impl ComponentParseError {
    /// User-facing message (without usage text).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Help => String::new(),
            Self::Usage(message) => message.clone(),
        }
    }
}

fn bare_token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_COMPONENT_TOKEN_BYTES && !token.contains('\0')
}

fn version_token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_COMPONENT_VERSION_BYTES && !token.contains('\0')
}

fn path_token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_COMPONENT_PATH_BYTES && !token.contains('\0')
}

fn digest_token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_COMPONENT_DIGEST_BYTES && !token.contains('\0')
}

/// Validate a `--digest` value: bounded token plus 64 hex chars (fail-closed,
/// usage error). Case-insensitive (upper/lower both accepted, normalized at
/// compare time via constant-shape compare).
fn valid_digest_flag(value: &str) -> bool {
    digest_token_ok(value) && crate::component_seed::is_hex_digest(value)
}

/// Validate a `--distro` value: bounded token plus the WSL distro grammar.
///
/// The literal `default` selects the `wsl.exe` default distro; every other
/// value must pass the WSL distro grammar. Fail-closed as usage (exit 2).
fn valid_distro_flag(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_COMPONENT_DISTRO_BYTES || value.contains('\0') {
        return false;
    }
    if value == "default" {
        return true;
    }
    bitty_runtime::component::valid_wsl_distro(value)
}

/// Known extension names for hints and alias collision checks (#2381).
///
/// Built-in names plus anything installed in either tier. This is a hint set,
/// not an authority: installs still require the catalog to list the release.
fn known_extension_names(user_root: Option<&Path>, system_root: Option<&Path>) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = KNOWN_BUILTIN_EXTENSIONS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    for summary in discover_components(user_root, system_root) {
        names.insert(summary.name);
    }
    names
}

/// Resolve one user-supplied extension name to the stored component name.
///
/// One `bitty-` prefix is an accepted alias (`bitty-net` -> `net`);
/// the resolved name is only used for selection, so stored names, directories,
/// and TOFU pins never change. An exact known name always wins; when both the
/// prefixed spelling and the unprefixed name are known the alias is ambiguous
/// and fails closed (exit 4) instead of guessing.
fn resolve_component_name(raw: &str, known: &BTreeSet<String>) -> Result<String, ComponentFailure> {
    let Some(stripped) = raw.strip_prefix(COMPONENT_ALIAS_PREFIX) else {
        return Ok(raw.to_string());
    };
    if stripped.is_empty() {
        return Ok(raw.to_string());
    }
    let raw_known = known.contains(raw);
    let stripped_known = known.contains(stripped);
    if raw_known && stripped_known {
        return Err(ComponentFailure::component(format!(
            "bitty extension: ambiguous extension name '{raw}': both '{raw}' and '{stripped}' are known; use '{stripped}' (aliases never rewrite stored names)"
        )));
    }
    if raw_known {
        return Ok(raw.to_string());
    }
    Ok(stripped.to_string())
}

/// Levenshtein distance over ASCII tokens (bounded candidate names).
fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<u8> = left.bytes().collect();
    let right: Vec<u8> = right.bytes().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, left_byte) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, right_byte) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_byte != right_byte);
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// Closest candidate within a small typo budget, if any.
fn closest_known_name<'a>(
    needle: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let budget = 2.max(needle.len() / 3);
    let mut best: Option<(&str, usize)> = None;
    for candidate in candidates {
        if candidate == needle {
            continue;
        }
        let distance = edit_distance(needle, candidate);
        if distance <= budget && best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((candidate, distance));
        }
    }
    best.map(|(candidate, _)| candidate)
}

/// Exact suggestion line for one verb, e.g. `bitty extension install graphics`.
fn extension_command(verb: &str, name: &str) -> String {
    format!("bitty extension {verb} {name}")
}

/// Actionable diagnostic for an unknown extension name (#2381).
///
/// Names the closest known extension and the exact command, never the full
/// usage block. `verb` is the canonical verb (`install`, `remove`, ...).
fn unknown_component_message(
    verb: &str,
    name: &str,
    user_root: Option<&Path>,
    system_root: Option<&Path>,
) -> String {
    let known = known_extension_names(user_root, system_root);
    if known.contains(name) {
        return format!(
            "bitty extension: component '{name}' is not installed; install it with `bitty extension install {name}` (latest catalog release) or stage a local build with `bitty extension add <path>`"
        );
    }
    let candidates: Vec<&str> = known.iter().map(String::as_str).collect();
    let suggestion = closest_known_name(name, candidates.iter().copied());
    let mut message = match suggestion {
        Some(closest) => format!(
            "bitty extension: unknown extension '{name}'; did you mean '{closest}'?\ntry: {}",
            extension_command(verb, closest)
        ),
        None => format!(
            "bitty extension: unknown extension '{name}' (known: {})",
            candidates.join(", ")
        ),
    };
    if verb == "install" {
        message.push_str("\nnote: a bare install resolves the latest catalog release; pin one with `--version <X.Y.Z>`");
    }
    message
}

/// Parse the tokens captured after the `component` word (bounded, fail-closed).
pub fn parse_component_request(
    raw: &[String],
    pre_format: Option<&str>,
) -> Result<ComponentRequest, ComponentParseError> {
    let mut verb: Option<ComponentVerb> = None;
    let mut operand: Option<String> = None;
    let mut version_operand: Option<String> = None;
    let mut version_flag: Option<String> = None;
    let mut format: Option<String> = None;
    let mut no_color = false;
    let mut yes = false;
    let mut digest: Option<String> = None;
    let mut force = false;
    let mut distro: Option<String> = None;

    let mut index = 0usize;
    while index < raw.len() {
        let token = raw[index].as_str();
        match token {
            "-h" | "--help" => return Err(ComponentParseError::Help),
            "--no-color" => {
                no_color = true;
                index += 1;
                continue;
            }
            "--yes" => {
                yes = true;
                index += 1;
                continue;
            }
            "--force" => {
                force = true;
                index += 1;
                continue;
            }
            "--" => {
                return Err(ComponentParseError::Usage(
                    "bitty extension: stray `--` separator (component takes no child argv)"
                        .to_string(),
                ));
            }
            _ => {}
        }
        if let Some(value) = token.strip_prefix("--format=") {
            format = Some(value.to_string());
            index += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--version=") {
            if !version_token_ok(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --version value is empty, contains NUL, or exceeds {MAX_COMPONENT_VERSION_BYTES} bytes"
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: duplicate --version".to_string(),
                ));
            }
            version_flag = Some(value.to_string());
            index += 1;
            continue;
        }
        if token == "--format" {
            let Some(value) = raw.get(index + 1) else {
                return Err(ComponentParseError::Usage(
                    "bitty extension: --format needs a value (table|json|jsonl)".to_string(),
                ));
            };
            format = Some(value.clone());
            index += 2;
            continue;
        }
        if token == "--version" {
            let Some(value) = raw.get(index + 1) else {
                return Err(ComponentParseError::Usage(
                    "bitty extension: --version needs a value (strict semver)".to_string(),
                ));
            };
            if !version_token_ok(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --version value is empty, contains NUL, or exceeds {MAX_COMPONENT_VERSION_BYTES} bytes"
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: duplicate --version".to_string(),
                ));
            }
            version_flag = Some(value.clone());
            index += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--digest=") {
            if !valid_digest_flag(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: invalid --digest {value:?} (want 64 hex characters, SHA-256)"
                )));
            }
            if digest.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: duplicate --digest".to_string(),
                ));
            }
            digest = Some(value.to_string());
            index += 1;
            continue;
        }
        if token == "--digest" {
            let Some(value) = raw.get(index + 1) else {
                return Err(ComponentParseError::Usage(
                    "bitty extension: --digest needs a value (64 hex characters, SHA-256)"
                        .to_string(),
                ));
            };
            if !valid_digest_flag(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: invalid --digest {value:?} (want 64 hex characters, SHA-256)"
                )));
            }
            if digest.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: duplicate --digest".to_string(),
                ));
            }
            digest = Some(value.clone());
            index += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--distro=") {
            if !valid_distro_flag(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: invalid --distro {value:?} (want 1..={MAX_COMPONENT_DISTRO_BYTES} bytes of [A-Za-z0-9._-], or the literal `default`)"
                )));
            }
            if distro.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: duplicate --distro".to_string(),
                ));
            }
            distro = Some(value.to_string());
            index += 1;
            continue;
        }
        if token == "--distro" {
            let Some(value) = raw.get(index + 1) else {
                return Err(ComponentParseError::Usage(
                    "bitty extension: --distro needs a value (a WSL distro name, or `default`)"
                        .to_string(),
                ));
            };
            if !valid_distro_flag(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: invalid --distro {value:?} (want 1..={MAX_COMPONENT_DISTRO_BYTES} bytes of [A-Za-z0-9._-], or the literal `default`)"
                )));
            }
            if distro.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: duplicate --distro".to_string(),
                ));
            }
            distro = Some(value.clone());
            index += 2;
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            return Err(ComponentParseError::Usage(format!(
                "bitty extension: unknown flag {token:?}"
            )));
        }
        // Positional operand: length bound depends on the verb under
        // construction (paths allow longer values than names).
        let looks_like_path = verb.is_some_and(|verb| verb == ComponentVerb::Add);
        if looks_like_path {
            if !path_token_ok(token) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: argument is empty, contains NUL, or exceeds {MAX_COMPONENT_PATH_BYTES} bytes"
                )));
            }
        } else if !bare_token_ok(token) {
            return Err(ComponentParseError::Usage(format!(
                "bitty extension: argument is empty, contains NUL, or exceeds {MAX_COMPONENT_TOKEN_BYTES} bytes"
            )));
        }
        if verb.is_none() {
            match ComponentVerb::parse(token) {
                Some(parsed) => verb = Some(parsed),
                None => {
                    return Err(ComponentParseError::Usage(format!(
                        "bitty extension: unknown verb {token:?} (want list|add|install|remove|show-source)"
                    )));
                }
            }
            index += 1;
            continue;
        }
        if operand.is_none() {
            operand = Some(token.to_string());
            index += 1;
            continue;
        }
        if version_operand.is_none() {
            version_operand = Some(token.to_string());
            index += 1;
            continue;
        }
        return Err(ComponentParseError::Usage(format!(
            "bitty extension: unexpected extra argument {token:?}"
        )));
    }

    let verb = verb.ok_or_else(|| {
        ComponentParseError::Usage(
            "bitty extension: missing verb (want list|add|install|remove|show-source)".to_string(),
        )
    })?;

    let format = match format {
        Some(value) => ComponentFormat::parse(Some(&value)).map_err(ComponentParseError::Usage)?,
        None => ComponentFormat::parse(pre_format).map_err(ComponentParseError::Usage)?,
    };

    match verb {
        ComponentVerb::List => {
            if version_operand.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: `list` takes at most one name filter (e.g. `bitty extension list bitty-graphics`)"
                        .to_string(),
                ));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --version only applies to `add` and `install` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --yes only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if digest.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --digest only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if force {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --force only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if distro.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --distro only applies to `add` (got `{}`)",
                    verb.name()
                )));
            }
        }
        ComponentVerb::Add => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: `add` needs a source path (local directory or `bitty-<name>` executable)"
                        .to_string(),
                ));
            }
            if version_operand.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: `add` takes one path operand (the version comes from `--version <semver>` for bare executables)"
                        .to_string(),
                ));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --yes only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if digest.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --digest only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if force {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --force only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
        }
        ComponentVerb::Install => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: `install` needs a component name (e.g. `bitty extension install graphics` for the latest release, or `bitty extension install graphics --version 0.0.24` to pin)"
                        .to_string(),
                ));
            }
            if version_operand.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: `install` takes the version from `--version <X.Y.Z>`, not a second operand"
                        .to_string(),
                ));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
            // Fail-closed scope: `install` fetches the host triple (a Windows
            // PE on Windows hosts) and cannot stage a Linux guest helper, so a
            // WSL placement for it would re-launch the PE via interop, never a
            // guest helper. Stage guest helpers with `add` instead.
            if distro.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: --distro only applies to `add` (guest-helper staging); `install` fetches the host triple and cannot stage a Linux guest helper — stage one with `bitty extension add <path> --distro <name>` (e.g. from inside the guest)".to_string(),
                ));
            }
        }
        ComponentVerb::Remove => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: `{}` needs a component name",
                    verb.name()
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: `{}` takes an optional version operand, not `--version` (got `{}`)",
                    verb.name(),
                    verb.name()
                )));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --yes only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if digest.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --digest only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if force {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --force only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if distro.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --distro only applies to `add` (got `{}`)",
                    verb.name()
                )));
            }
        }
        ComponentVerb::ShowSource => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(
                    "bitty extension: `show-source` needs a component name (e.g. `bitty extension show-source net`)"
                        .to_string(),
                ));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: `{}` takes an optional version operand, not `--version` (got `{}`)",
                    verb.name(),
                    verb.name()
                )));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --yes only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if digest.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --digest only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if force {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --force only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
            if distro.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty extension: --distro only applies to `add` (got `{}`)",
                    verb.name()
                )));
            }
        }
    }

    Ok(ComponentRequest {
        verb,
        operand,
        version_operand,
        version_flag,
        format,
        no_color,
        yes,
        digest,
        force,
        distro,
    })
}

/// Long help (`bitty extension --help`).
#[must_use]
pub fn component_help_text() -> String {
    concat!(
        "bitty extension — CLI-first native binary extension management (local class, no execution)\n\
     \n\
     usage: bitty extension <verb> [args] [flags]\n\
     \n\
     verbs:\n\
     \x20 list [<name>]               Show installed components across both tiers:\n\
     \x20                             name, version, active marker, source\n\
     \x20                             (user|system), ABI compat, path, and the\n\
     \x20                             last activation transition the broker\n\
     \x20                             recorded for that component name\n\
     \x20                             (installed|active|denied:<class>|crashed|\n\
     \x20                             stopped; installed when never activated).\n\
     \x20                             This is last-recorded diagnostics, not a\n\
     \x20                             liveness probe: active means a spawn was\n\
     \x20                             recorded, not that the process is running\n\
     \x20                             now. The label is shown once per component,\n\
     \x20                             on the active-version row, and covers both\n\
     \x20                             install tiers via $XDG_STATE_HOME. An\n\
     \x20                             optional name filters the table (aliases\n\
     \x20                             accepted: `bitty extension list bitty-graphics`).\n\
     \x20 add <path> [--version V]    Stage a pre-built binary into the user tier\n\
     \x20                             ($XDG_DATA_HOME/bitty/extensions/, no root).\n\
     \x20                             A directory holds bitty-component.toml plus\n\
     \x20                             the executable; a bare bitty-<name> file\n\
     \x20                             needs --version <semver>. The digest is\n\
     \x20                             computed and ABI compat verified before\n\
     \x20                             anything is written. URLs fail closed (add\n\
     \x20                             accepts local paths only).\n\
     \x20 install <name> [--version V] Fetch the bounded extension catalog plus one\n\
     \x20 [--digest H] [--force]      prebuilt release from R2\n\
     \x20 [--yes]                     (https://cdn.bitty.run; SHA256SUMS and catalog\n\
     \x20                             digest verified, TOFU-pinned) into the user\n\
     \x20                             tier, then hand off to the full manager.\n\
     \x20                             Omit --version to resolve the LATEST release\n\
     \x20                             for the exact name and host target from the\n\
     \x20                             catalog: the highest strict X.Y.Z wins, and\n\
     \x20                             the resolved version is printed before the\n\
     \x20                             download. `--version X.Y.Z` pins exactly.\n\
     \x20                             Prereleases are never published by the pack\n\
     \x20                             grammar, so latest never selects one. A\n\
     \x20                             catalog with no entry for this name/target\n\
     \x20                             fails closed; an unavailable catalog fails a\n\
     \x20                             latest install with a --version hint (a\n\
     \x20                             pinned install falls back to SHA256SUMS-only).\n\
     \x20 remove <name> [<version>]   Remove one user-installed version, or every\n\
     \x20                             user version when none is named. Removing\n\
     \x20                             the active version also drops `current`.\n\
     \x20                             Never touches the system tier.\n\
     \x20 show-source <name> [V]     Print the exact pinned executable bytes to\n\
     \x20                             stdout (version defaults to the active\n\
     \x20                             `current` pointer). The hash is re-verified\n\
     \x20                             against the installed descriptor first; a\n\
     \x20                             missing install or mismatch fails closed\n\
     \x20                             (exit 4) with nothing on stdout. Provenance\n\
     \x20                             (name, version, tier, path, sha256) goes to\n\
     \x20                             stderr so stdout stays byte-identical.\n\
     \n\
     name aliases (#2381):\n\
     \x20 `list`, `install`, `remove`, and `show-source` accept one repo-style\n\
     \x20 `bitty-` prefix (`bitty-graphics` resolves to `graphics`) because\n\
     \x20 extension repositories carry the workspace prefix while stored\n\
     \x20 component names do not. Resolution only selects the target: stored\n\
     \x20 names, directories, and TOFU pins are never rewritten. An exact\n\
     \x20 known name wins; when both `bitty-<name>` and `<name>` are known the\n\
     \x20 alias is ambiguous and fails closed (exit 4) instead of guessing.\n\
     \n\
     latest resolution (#2381):\n\
     \x20 A bare `install <name>` selects the highest strict X.Y.Z entry for\n\
     \x20 the exact (name, host target) pair in the catalog. Host target is\n\
     \x20 detected, never a flag, so a foreign target can never be confused\n\
     \x20 with this one. Numeric comparison is per component, so 0.0.10 beats\n\
     \x20 0.0.9. Prereleases are excluded by the pack grammar (no prerelease\n\
     \x20 is ever published); an explicit `--version` must be X.Y.Z too.\n\
     \x20 Trust boundary: the catalog is fetched over TLS from the single\n\
     \x20 builtin egress, and its digest must match the downloaded bytes and\n\
     \x20 the release SHA256SUMS (all-available-must-match), then the TOFU pin\n\
     \x20 records first contact. `latest` is a convenience selector, NOT a\n\
     \x20 security guarantee: it tracks a mutable catalog, so pin --version for\n\
     \x20 reproducible automation. If the catalog is unavailable (offline, or a\n\
     \x20 404 before publication catches up) or has no entry, a latest install\n\
     \x20 fails closed with a concrete --version example; a version is never\n\
     \x20 guessed from GitHub tags or R2 directory names.\n\
     \n\
     source transparency (issue #1986):\n\
     \x20 `show-source` prints pinned bytes (stdout) with provenance on\n\
     \x20                             stderr; `install` renders an update-diff\n\
     \x20                             preview when the fetch would change the TOFU\n\
     \x20                             pin (preview only, consent and --force still\n\
     \x20                             apply; diff is local, no network); dist\n\
     \x20                             validation refuses minified/obfuscated Lua\n\
     \x20                             and descriptors (readability rule: max 1000\n\
     \x20                             bytes per line, 200 average).\n\
     \n\
     flags:\n\
     \x20 --format table|json|jsonl   list output shape (default table).\n\
     \x20 --no-color                  Accepted for parity (tables are plain text).\n\
     \x20 --version <semver>          add: version for a bare executable.\n\
     \x20                             install: pin this exact X.Y.Z release;\n\
     \x20                             omit it to resolve the highest published\n\
     \x20                             catalog version for the host target.\n\
     \x20 --digest <hex>              install only: caller-supplied expected tarball\n\
     \x20                             digest (64 hex, SHA-256). All-available-must-\n\
     \x20                             match with CDN SHA256SUMS and the registry\n\
     \x20                             catalog when published; any mismatch is an\n\
     \x20                             integrity failure (exit 4), never a silent pick.\n\
     \x20                             Malformed values fail as usage (exit 2).\n\
     \x20 --force                     install only: re-pin the TOFU pin to the new\n\
     \x20                             verified digest after all-available-must-match\n\
     \x20                             passes. Does NOT bypass digest verification.\n\
     \x20 --yes                       install only: approve the download consent\n\
     \x20                             non-interactively (no prompt).\n\
     \x20 --distro <name|default>     add only: WSL2 guest distro for component\n\
     \x20                             execution on Windows. `default` uses the\n\
     \x20                             wsl.exe default distro. `install --distro`\n\
     \x20                             is rejected (install fetches the host triple\n\
     \x20                             and cannot stage a guest helper — use `add`).\n\
     \x20                             Windows-only in effect: Unix validates the\n\
     \x20                             value but stays native (no-op). See `WSL2\n\
     \x20                             trampoline` below.\n\
     \n\
     WSL2 trampoline (Windows only, issue #1908):\n\
     \x20 Windows component helpers that must run inside a WSL2 guest do so\n\
     \x20 through wsl.exe [--distribution D] --exec <helper> <fixed-argv>\n\
     \x20 (PATH-resolved wsl.exe, fixed argv, no shell on either side).\n\
     \x20 `add` never executes the component here: it stages only and persists\n\
     \x20 the resolved selection per-component to <user>/<name>/wsl_distro for\n\
     \x20 later runs (omission preserves any previous placement). `install`\n\
     \x20 always fetches the host triple (a Windows PE on Windows hosts) and\n\
     \x20 takes no --distro (flag or env): any selection for it fails as usage\n\
     \x20 (exit 2) naming `add`, before any fetch or staging. Cross-target\n\
     \x20 Linux fetching for `install --distro` is a #1908 follow-up, not in\n\
     \x20 this change. Guest distro selection (`add`): explicit --distro wins,\n\
     \x20 else BITTY_WSL_DISTRO config wins, else native execution (no\n\
     \x20 trampoline); pass the literal `default` for the wsl.exe default\n\
     \x20 distro (no --distribution flag). A selected distro with absent WSL\n\
     \x20 fails closed before any staging (exit 1): install WSL2 from\n\
     \x20 the Store (inbox builds use --status fallback, unversioned) or run\n\
     \x20 without --distro. Path mapping (Windows to guest for spawn):\n",
        // scratch-paths-exempt: intentional WSL help contract path example, not a host path.
        "\x20 drive-absolute X:\\... maps to /mnt/<lower-x>/... and WSL UNC\n\
     \x20 \\\\wsl$\\<distro>\\... maps to guest-absolute /...; UNC paths bind\n\
     \x20 to their launch distro (cross-distro exec fails closed); device\n\
     \x20 paths, drive-relative, relative, and other UNC spellings fail closed\n\
     \x20 (exit 2, no silent mangling). Backslash rewriting happens only inside\n\
     \x20 path mapping; general argv elements pass through discrete with spaces\n",
        // scratch-paths-exempt: intentional WSL help contract path example, not a host path.
        "\x20 preserved and metacharacters inert. Reverse (/mnt/... to Windows)\n\
     \x20 exists for diagnostics only; other guest paths have no Windows\n\
     \x20 spelling. Guest env is WSLENV-gated (forwarded proxy/TLS/locale\n\
     \x20 names only, secrets never, otherwise clean by design). Stdio and\n\
     \x20 signals: stdin/stdout/stderr pipes pass through wsl.exe unchanged\n\
     \x20 (binary-safe, no CRLF translation); Ctrl-C propagates via wsl.exe to\n\
     \x20 the guest helper; the wsl.exe exit status is the helper exit status\n\
     \x20 (1:1 fidelity, no remapping). Fetch stays native (Windows\n\
     \x20 curl.exe/tar.exe); the trampoline applies to component execution,\n\
     \x20 never to downloads.\n\
     \n\
     installer egress:\n\
     \x20 cdn.bitty.run:443 is the core-built-in installer egress: strict\n\
     \x20 configurations allow the pinned first-party host with no extra\n\
     \x20 grant. Every other host stays consent-gated and fails closed\n\
     \x20 without explicit consent. Fetch uses fixed-argv system curl\n\
     \x20 (pinned to https://cdn.bitty.run, --proto =https, no shell) plus\n\
     \x20 system tar; no Rust network stack.\n\
     \n\
     digest arbitration:\n\
     \x20 All-available-must-match: CDN SHA256SUMS, the extension catalog\n\
     \x20 digest when the release is published there, and the caller-supplied\n\
     \x20 --digest when provided must EACH match the downloaded bytes. Any\n\
     \x20 mismatch exits 4, never a silent pick. Hex compare is constant-shape\n\
     \x20 (pass/fail only, no position oracle).\n\
     \n\
     registry catalog:\n\
     \x20 `install` fetches https://cdn.bitty.run/extensions/catalog.v1 after\n\
     \x20 consent. The bounded line format is `schema 1` plus\n\
     \x20 `extension <name> <version> <target> <sha256>` records. A pinned\n\
     \x20 install uses the exact requested version; a bare install selects the\n\
     \x20 highest strict X.Y.Z for the exact (name, host target). A fetched\n\
     \x20 catalog that is malformed, oversize, or missing the requested\n\
     \x20 name/version/host-target fails closed (exit 4) before the tarball;\n\
     \x20 there is no allow-all fallback and no directory-name guessing. For a\n\
     \x20 *pinned* install, a catalog *transport* failure (offline, 404, 5xx)\n\
     \x20 falls back to the single-release SHA256SUMS path with the same\n\
     \x20 explicit consent, TOFU pin, and exact-two-member audit; a latest\n\
     \x20 install cannot fall back (there is no version) and prints a --version\n\
     \x20 example instead.\n\
     \n\
     TOFU pinning:\n\
     \x20 First install of a component records (pins) the verified CDN\n\
     \x20 tarball digest to <user>/<name>/tarball.sha256. Later installs\n\
     \x20 (reinstall, upgrade, downgrade) with a differing digest fail closed\n\
     \x20 as pin-mismatch (exit 4) naming the component, versions, and digests;\n\
     \x20 use --force to re-pin after verification. The pin never auto-updates.\n\
     \n\
     download consent:\n\
     \x20 Consent precedes every transport contact, so a decline or EOF exits\n\
     \x20 1 with nothing fetched or staged. Unsigned 0.1.0 therefore always\n\
     \x20 takes the explicit-confirm path for executable downloads; --digest\n\
     \x20 and the catalog digest do NOT grant silent. Without --yes the\n\
     \x20 installer prompts for the exact pin (`Grant download\n\
     \x20 <name> <version>? [y/N]`) or, for a bare install, for resolving and\n\
     \x20 downloading the latest release for the host target (up to 3 attempts,\n\
     \x20 64 bytes per answer); `y`/`yes` approves, `n`/`no`/empty declines, EOF\n\
     \x20 aborts. The resolved version is printed before the release download.\n\
     \n\
     fetch environment:\n\
     \x20 `install` preflights `curl --version` once per invocation and needs\n\
     \x20 curl 7.68.0 or newer (for --proto =https, --connect-timeout,\n\
     \x20 --max-time, --max-filesize; `curl` resolves via PATH, curl.exe on\n\
     \x20 Windows). A missing curl fails closed: install curl, or place the\n\
     \x20 release manually under $XDG_DATA_HOME/bitty/extensions/<name>/\n\
     \x20 <version>/; nothing is fetched or staged (exit 1). A too-old curl\n\
     \x20 fails the same way naming the found version, the floor, and the\n\
     \x20 upgrade hint (exit 1); a missing tar fails likewise (exit 1).\n\
     \x20 Fetch uses the system CA store only (no --cacert override) and\n\
     \x20 honors the system proxy env (https_proxy/HTTPS_PROXY/all_proxy\n\
     \x20 plus no_proxy) by inheritance; no *_TOKEN/*_KEY/*_SECRET variable\n\
     \x20 ever crosses into a fetch child, and Core carries no custom TLS\n\
     \x20 flags.\n\
     \n\
     resolution:\n\
     \x20 User $XDG_DATA_HOME/bitty/extensions/ wins over system\n\
     \x20 /usr/lib/bitty/extensions/ (or platform equivalent) on collision.\n\
     \x20 A tampered user install fails closed without system fallback. Every\n\
     \x20 spawn re-verifies name, version, executable name, protocol range,\n\
     \x20 and SHA-256; a protocol range excluding Core fails closed.\n\
     \n\
     authority:\n\
     \x20 No component code ever runs during any `bitty extension` operation.\n\
     \x20 Missing optional components fail soft: resolution reports the name,\n\
     \x20 the `bitty extension add` command to run, and never a stack trace.\n\
     \n\
     exit codes:\n\
     \x20 0 success | 1 declined consent, aborted prompt, filesystem\n\
     \x20 failure, fetch-environment failure (missing/too-old curl,\n\
     \x20 missing tar), or WSL absent when --distro was requested | 2 usage\n\
     \x20 (including malformed --digest/--distro and unmappable WSL paths) | 4\n\
     \x20 component error (integrity, ABI, digest, pin-mismatch)\n\
     \n\
     examples:\n\
     \x20 bitty extension list\n\
     \x20 bitty extension list --format json\n\
     \x20 bitty extension list bitty-graphics\n\
     \x20 bitty extension add ./dist/net --version 0.0.1\n\
     \x20 bitty extension add ./dist/net --distro Ubuntu\n\
     \x20 bitty extension install graphics            # latest published release\n\
     \x20 bitty extension install graphics --version 0.0.24\n\
     \x20 bitty extension install net --version 0.0.1\n\
     \x20 bitty extension install bitty-net --version 0.0.1   # repo-style alias\n\
     \x20 bitty extension install net --version 0.0.1 --digest <hex>\n\
     \x20 bitty extension install net --version 0.0.1 --force --yes\n\
     \x20 bitty extension remove net 0.0.1\n\
     \x20 bitty extension show-source net\n\
     \x20 bitty extension show-source bitty-net 0.0.1\n\
     \x20 bitty extension show-source net 0.0.1"
    )
        .to_string()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Injected environment for one `bitty extension` dispatch (hermetic).
#[derive(Debug, Clone, Default)]
pub struct ComponentContext<'a> {
    /// `XDG_DATA_HOME` environment value (user tier parent).
    pub xdg_data_home: Option<&'a str>,
    /// `HOME` environment value (fallback data-home parent).
    pub home: Option<&'a str>,
    /// `BITTY_COMPONENTS_DIR` override (tests; wins over XDG).
    pub components_dir: Option<&'a str>,
    /// `BITTY_SYSTEM_COMPONENTS_DIR` override (tests; default is the
    /// platform system path).
    pub system_components_dir: Option<&'a str>,
    /// `XDG_STATE_HOME` environment value (activation record parent).
    pub xdg_state_home: Option<&'a str>,
    /// `BITTY_COMPONENT_STATE_DIR` override (tests; wins over XDG state).
    pub component_state_dir: Option<&'a str>,
    /// `BITTY_WSL_DISTRO` environment/config value for guest distro
    /// selection (tests pass values in; production reads the environment).
    pub wsl_distro_env: Option<&'a str>,
    /// Pre-word global `--format` fallback.
    pub pre_format: Option<&'a str>,
    /// Pre-word `--no-color`.
    pub pre_no_color: bool,
}

/// One dispatch failure with its stable exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComponentFailure {
    exit: i32,
    message: String,
}

impl ComponentFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_USAGE,
            message: message.into(),
        }
    }

    fn component(message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_COMPONENT,
            message: message.into(),
        }
    }

    fn generic(message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_GENERIC,
            message: message.into(),
        }
    }
}

/// Validate WSL2 distro selection for `add`/`install` (fail-closed before mutation).
///
/// Returns the resolved selection (`None` means native, no trampoline) for the
/// caller to persist after successful staging; `Err(exit_code)` means exit
/// early. Invalid selection fails as usage (exit 2) on every platform; on
/// Windows a selected distro with absent WSL fails as environmental (exit 1).
/// Unix never probes (no-op, native execution even when selected).
fn check_wsl_for_store_verb(
    explicit_distro: Option<&str>,
    env_distro: Option<&str>,
) -> Result<Option<bitty_runtime::component::WslDistro>, i32> {
    let selection = match bitty_runtime::component::select_wsl_distro(explicit_distro, env_distro) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("bitty extension: {}", error.message());
            return Err(EXIT_USAGE);
        }
    };
    // Unix: selection validated above, execution stays native (no probe).
    #[cfg(not(windows))]
    {
        Ok(selection)
    }
    // Windows: probe only when a distro was selected; native otherwise.
    #[cfg(windows)]
    {
        if selection.is_some() {
            if let Err(error) = bitty_runtime::component::detect_wsl_context() {
                eprintln!("bitty extension: {}", error.message());
                return Err(error.exit_code());
            }
        }
        Ok(selection)
    }
}

/// Persist a resolved WSL distro selection for one installed component.
///
/// `add`/`install` are staging/fetch operations only (they never execute the
/// component); this records the execution placement for later runs (broker
/// reads `<user>/<name>/wsl_distro` when no explicit override is set).
/// `None` preserves any previous placement (never deletes); `Some` overwrites
/// atomically. Failures propagate as generic (exit 1) without rolling back
/// the already-staged component.
fn persist_wsl_selection(
    user: &std::path::Path,
    name: &str,
    selection: Option<&bitty_runtime::component::WslDistro>,
) -> Result<(), ComponentFailure> {
    let Some(distro) = selection else {
        return Ok(());
    };
    let content = match distro {
        bitty_runtime::component::WslDistro::Default => "default\n".to_string(),
        bitty_runtime::component::WslDistro::Named(distro) => format!("{distro}\n"),
    };
    let path = user
        .join(name)
        .join(bitty_runtime::component::WSL_DISTRO_FILE);
    publish_bytes_atomic(&path, content.as_bytes(), None)
}

/// Run one `bitty extension` invocation; returns the process exit code.
pub fn run_component_subcommand(
    raw: &[String],
    context: &ComponentContext<'_>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let request = match parse_component_request(raw, context.pre_format) {
        Ok(request) => request,
        Err(ComponentParseError::Help) => {
            let _ = output.write_all(component_help_text().as_bytes());
            let _ = output.write_all(b"\n");
            return EXIT_OK;
        }
        Err(error) => {
            // #2381: errors stay targeted. Full usage lives behind --help;
            // the pointer keeps discoverability without the wall of text.
            eprintln!("{}", error.message());
            eprintln!("{COMPONENT_USAGE_POINTER}");
            return EXIT_USAGE;
        }
    };

    let data_home = data_home_for(context.xdg_data_home, context.home);
    let (user_root, system_root) = component_search_roots_for(
        context.components_dir,
        data_home.as_deref(),
        context.system_components_dir,
    );
    let state_root = component_state_root_for(
        context.component_state_dir,
        context.xdg_state_home,
        context.home,
    );

    // Name aliases (#2381): resolve one `bitty-` prefix for every verb that
    // takes a name. `add` keeps its path operand untouched.
    let known = known_extension_names(user_root.as_deref(), system_root.as_deref());
    let resolved_name = match request.verb {
        ComponentVerb::Install
        | ComponentVerb::Remove
        | ComponentVerb::ShowSource
        | ComponentVerb::List => match request.operand.as_deref() {
            Some(raw) => match resolve_component_name(raw, &known) {
                Ok(name) => Some(name),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    return failure.exit;
                }
            },
            None => None,
        },
        ComponentVerb::Add => None,
    };

    // WSL2 trampoline gate (#1908): `add` stages guest helpers (validates,
    // probes on Windows, persists placement); `install` fetches the host
    // triple and cannot stage a guest helper, so any distro selection for it
    // (flag or env) fails as usage (exit 2) before any fetch or staging.
    // Unix never probes (no-op). `list`/`remove` never consult the trampoline.
    // Neither verb executes the component here; `add` persists the resolved
    // selection per-component for later runs.
    let wsl_selection: Option<bitty_runtime::component::WslDistro> = match request.verb {
        ComponentVerb::Add => {
            match check_wsl_for_store_verb(request.distro.as_deref(), context.wsl_distro_env) {
                Ok(selection) => selection,
                Err(exit) => return exit,
            }
        }
        ComponentVerb::Install => {
            match bitty_runtime::component::select_wsl_distro(
                request.distro.as_deref(),
                context.wsl_distro_env,
            ) {
                Ok(None) => None,
                Ok(Some(_)) => {
                    eprintln!(
                        "bitty extension: --distro only applies to `add` (guest-helper staging); `install` fetches the host triple and cannot stage a Linux guest helper — stage one with `bitty extension add <path> --distro <name>` (e.g. from inside the guest; cross-target Linux fetching is a #1908 follow-up, not in this change)"
                    );
                    return EXIT_USAGE;
                }
                Err(error) => {
                    eprintln!("bitty extension: {}", error.message());
                    return EXIT_USAGE;
                }
            }
        }
        _ => None,
    };

    match request.verb {
        ComponentVerb::List => {
            let mut summaries = discover_components(user_root.as_deref(), system_root.as_deref());
            if let Some(name) = resolved_name.as_deref() {
                summaries.retain(|summary| summary.name == name);
                if summaries.is_empty() {
                    eprintln!(
                        "{}",
                        unknown_component_message(
                            "list",
                            name,
                            user_root.as_deref(),
                            system_root.as_deref()
                        )
                    );
                    return EXIT_COMPONENT;
                }
            }
            let listed = summarize_list(
                &summaries,
                user_root.as_deref(),
                system_root.as_deref(),
                state_root.as_deref(),
            );
            match request.format {
                ComponentFormat::Table => {
                    let no_color = request.no_color || context.pre_no_color;
                    let _ = output.write_all(format_list_table(&listed, no_color).as_bytes());
                }
                ComponentFormat::Json | ComponentFormat::Jsonl => {
                    let _ = writeln!(output, "{}", format_list_envelope(&listed));
                }
            }
            EXIT_OK
        }
        ComponentVerb::Add => {
            let source = request.operand.as_deref().expect("add requires a path");
            match op_add(
                source,
                request.version_flag.as_deref(),
                user_root.as_deref(),
                output,
                wsl_selection.as_ref(),
            ) {
                Ok(summary) => {
                    let _ = writeln!(output, "{summary}");
                    EXIT_OK
                }
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        ComponentVerb::Install => {
            let name = resolved_name.as_deref().expect("install requires a name");
            let mut transport = crate::component_seed::SystemTransport;
            match op_install(
                name,
                request.version_flag.as_deref(),
                request.digest.as_deref(),
                request.force,
                user_root.as_deref(),
                output,
                input,
                request.yes,
                &mut transport,
            ) {
                Ok(summary) => {
                    let _ = writeln!(output, "{summary}");
                    EXIT_OK
                }
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        ComponentVerb::Remove => {
            let name = resolved_name.as_deref().expect("remove requires a name");
            match op_remove(
                name,
                request.version_operand.as_deref(),
                user_root.as_deref(),
                system_root.as_deref(),
                output,
            ) {
                Ok(summary) => {
                    let _ = writeln!(output, "{summary}");
                    EXIT_OK
                }
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        ComponentVerb::ShowSource => {
            let name = resolved_name
                .as_deref()
                .expect("show-source requires a name");
            match op_show_source(
                name,
                request.version_operand.as_deref(),
                user_root.as_deref(),
                system_root.as_deref(),
                output,
            ) {
                // `op_show_source` already wrote the exact pinned bytes to
                // `output` and provenance to stderr: no summary line follows,
                // so stdout stays byte-identical for piping/hashing.
                Ok(()) => EXIT_OK,
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// list rendering
// ---------------------------------------------------------------------------

/// One `list` entry: the installed inventory plus the activation state the
/// broker last recorded for that component name (#2254).
///
/// `activation` is read through the runtime's single reader
/// ([`read_component_activation`]); `list` never derives a state of its own.
#[derive(Debug, Clone)]
pub struct ListedComponent {
    /// Discovered inventory for the component name.
    pub summary: ComponentSummary,
    /// Recorded activation (`installed` when nothing was recorded).
    pub activation: ActivationState,
}

fn summarize_list(
    summaries: &[ComponentSummary],
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    state_root: Option<&Path>,
) -> Vec<ListedComponent> {
    summaries
        .iter()
        .map(|summary| ListedComponent {
            summary: summary.clone(),
            activation: read_component_activation_with_state(
                user_root,
                system_root,
                state_root,
                &summary.name,
            ),
        })
        .collect()
}

fn format_list_table(listed: &[ListedComponent], _no_color: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<16} {:<12} {:<7} {:<8} {:<9} {:<30}  PATH",
        "NAME", "VERSION", "ACTIVE", "SOURCE", "ABI", "LAST ACTIVATION"
    );
    let mut rows = 0usize;
    for entry in listed {
        let summary = &entry.summary;
        let activation = entry.activation.label();
        if summary.versions.is_empty() {
            let active = summary.active_version.as_deref().unwrap_or("(none)");
            let source = summary.active_source.map_or("-", |source| source.label());
            let _ = writeln!(
                out,
                "{:<16} {:<12} {:<7} {:<8} {:<9} {:<30}  (missing version dir)",
                summary.name, active, "*", source, "missing", activation,
            );
            rows += 1;
            continue;
        }
        // The record is name-scoped: print it once, on the active-version
        // row. If `current` names no installed row, the first row carries it
        // so the label is never duplicated or silently dropped.
        let active_row = summary.versions.iter().position(|installed| {
            summary.active_version.as_deref() == Some(installed.version.as_str())
        });
        for (index, installed) in summary.versions.iter().enumerate() {
            let is_active = summary.active_version.as_deref() == Some(installed.version.as_str());
            let abi = if installed.compatible {
                "ok"
            } else {
                "mismatch"
            };
            let activation = if is_active || (active_row.is_none() && index == 0) {
                activation.as_str()
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "{:<16} {:<12} {:<7} {:<8} {:<9} {:<30}  {}",
                summary.name,
                installed.version,
                if is_active { "*" } else { "-" },
                installed.source.label(),
                abi,
                activation,
                installed.executable_path.display(),
            );
            rows += 1;
        }
    }
    if rows == 0 {
        out.push_str("(no components)\n");
    }
    out
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Success envelope for `list`.
///
/// The `"command":"component"` field is deliberately stable across the #1959
/// verb rename: both spellings share this handler, so stdout stays
/// byte-identical and machine consumers keep parsing without a flag day.
/// Only the stderr deprecation note distinguishes the alias.
///
/// `activation` / `activation_class` were added for #2254 at the end of each
/// component object: existing fields keep their order and meaning, and
/// `activation` is the same `installed|active|denied:<class>|crashed|stopped`
/// last-recorded transition the table shows. It appears once per component
/// (the table puts it on the active-version row), never once per version.
#[must_use]
pub fn format_list_envelope(listed: &[ListedComponent]) -> String {
    let mut out = String::from(
        "{\"v\":1,\"command\":\"component\",\"ok\":true,\"result\":{\"verb\":\"list\",",
    );
    let _ = write!(out, "\"count\":{},", listed.len());
    out.push_str("\"components\":[");
    for (index, entry) in listed.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let summary = &entry.summary;
        let _ = write!(
            out,
            "{{\"name\":\"{}\",\"active_version\":{},\"active_source\":{},\"active_path\":{},\"versions\":[",
            json_escape(&summary.name),
            match &summary.active_version {
                Some(version) => format!("\"{}\"", json_escape(version)),
                None => "null".to_string(),
            },
            match &summary.active_source {
                Some(source) => format!("\"{}\"", source.label()),
                None => "null".to_string(),
            },
            match &summary.active_path {
                Some(path) => format!("\"{}\"", json_escape(&path.display().to_string())),
                None => "null".to_string(),
            },
        );
        for (version_index, installed) in summary.versions.iter().enumerate() {
            if version_index > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"version\":\"{}\",\"source\":\"{}\",\"compatible\":{},\"protocol_min\":{},\"protocol_max\":{},\"path\":\"{}\"}}",
                json_escape(&installed.version),
                installed.source.label(),
                installed.compatible,
                installed.protocol_min,
                installed.protocol_max,
                json_escape(&installed.executable_path.display().to_string()),
            );
        }
        let _ = write!(
            out,
            "],\"activation\":\"{}\",\"activation_class\":{}}}",
            json_escape(&entry.activation.label()),
            match entry.activation.class() {
                Some(class) => format!("\"{}\"", json_escape(class)),
                None => "null".to_string(),
            },
        );
    }
    out.push_str("]}}");
    out
}

// ---------------------------------------------------------------------------
// add (local path only, user tier only)
// ---------------------------------------------------------------------------

/// Staged source: validated name/version/protocol/executable plus the raw
/// executable bytes for the digest.
///
/// Shared by `add` (local path) and the install seed (verified R2 payload):
/// both run the same ABI check and `install_staged` downstream.
pub(crate) struct StagedSource {
    /// Validated component name.
    pub(crate) name: String,
    /// Validated version.
    pub(crate) version: String,
    /// Lowest supported wire protocol version.
    pub(crate) protocol_min: u16,
    /// Highest supported wire protocol version.
    pub(crate) protocol_max: u16,
    /// Executable base name (`bitty-<name>`).
    pub(crate) executable: String,
    /// Raw executable bytes.
    pub(crate) bytes: Vec<u8>,
}

fn is_url_operand(source: &str) -> bool {
    let lower = source.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("git@")
        || lower.starts_with("ssh://")
        || lower.starts_with("registry:")
}

fn op_add(
    source: &str,
    version_flag: Option<&str>,
    user_root: Option<&Path>,
    _output: &mut dyn std::io::Write,
    wsl_selection: Option<&bitty_runtime::component::WslDistro>,
) -> Result<String, ComponentFailure> {
    if is_url_operand(source) {
        return Err(ComponentFailure::component(format!(
            "bitty extension: remote sources are not implemented in v1 \
             (got {source:?}); stage from a local directory holding \
             bitty-component.toml plus the executable, or a bare \
             `bitty-<name>` executable with `--version <semver>`"
        )));
    }
    let Some(user) = user_root else {
        return Err(ComponentFailure::component(
            "bitty extension: no user component root (data directory unavailable; set $XDG_DATA_HOME or $HOME)".to_string(),
        ));
    };
    let staged = load_staged_source(source, version_flag)?;
    // ABI-compat check before any mutation: the protocol range must include
    // Core's wire version, else fail closed without writing files.
    let core = bitty_runtime::component::PROTOCOL_VERSION;
    if !(staged.protocol_min..=staged.protocol_max).contains(&core) {
        return Err(ComponentFailure::component(incompatible_component_hint(
            &staged.name,
            staged.protocol_min,
            staged.protocol_max,
            core,
        )));
    }
    let summary = install_staged(&staged, user)?;
    // Persist execution placement for later runs (staging only; never executes
    // here). `None` preserves any previous placement.
    persist_wsl_selection(user, &staged.name, wsl_selection)?;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// install (minimal seed: one allowlisted R2 release, then manager handoff)
// ---------------------------------------------------------------------------

/// Map a seed failure onto the stable exit taxonomy.
fn seed_failure(error: &crate::component_seed::SeedError) -> ComponentFailure {
    let message = format!("bitty extension: install failed: {}", error.message());
    match error.exit_code() {
        2 => ComponentFailure::usage(message),
        4 => ComponentFailure::component(message),
        _ => ComponentFailure::generic(message),
    }
}

/// TOFU pin path for one component (`<user>/<name>/tarball.sha256`).
fn tofu_pin_path(user: &Path, name: &str) -> PathBuf {
    user.join(name).join(TARBALL_PIN_FILE)
}

/// Read the recorded TOFU pin for `name` (`None` means first contact).
///
/// Fail-closed: a symlinked pin refuses with exit 1 (like other install
/// paths); an oversize, non-UTF-8, or non-hex pin fails as pin-mismatch
/// (exit 4) requiring `--force` to re-pin. Comparison by callers is
/// constant-shape via [`crate::component_seed::digests_equal_ct`].
fn read_tofu_pin(user: &Path, name: &str) -> Result<Option<String>, ComponentFailure> {
    let path = tofu_pin_path(user, name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ComponentFailure::generic(format!(
                "bitty extension: refusing symlink pin file '{}'",
                path.display()
            )));
        }
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() > MAX_PIN_FILE_BYTES {
                return Err(ComponentFailure::component(format!(
                    "bitty extension: pin-mismatch for '{name}': pin file '{}' exceeds {MAX_PIN_FILE_BYTES} bytes (corrupt pin; use --force to re-pin)",
                    path.display()
                )));
            }
        }
        Ok(_) => {
            return Err(ComponentFailure::generic(format!(
                "bitty extension: refusing non-file pin path '{}'",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ComponentFailure::generic(format!(
                "bitty extension: cannot inspect '{}': {error}",
                path.display()
            )));
        }
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            let Ok(text) = String::from_utf8(bytes) else {
                return Err(ComponentFailure::component(format!(
                    "bitty extension: pin-mismatch for '{name}': pin file '{}' is not UTF-8 (corrupt pin; use --force to re-pin)",
                    path.display()
                )));
            };
            let digest = text.trim().to_ascii_lowercase();
            if !crate::component_seed::is_hex_digest(&digest) {
                return Err(ComponentFailure::component(format!(
                    "bitty extension: pin-mismatch for '{name}': pin file '{}' is invalid (want 64 hex characters; use --force to re-pin)",
                    path.display()
                )));
            }
            Ok(Some(digest))
        }
        Err(error) => Err(ComponentFailure::generic(format!(
            "bitty extension: cannot read '{}': {error}",
            path.display()
        ))),
    }
}

/// Record (first contact) or re-pin (`--force`) the TOFU pin atomically.
fn write_tofu_pin(user: &Path, name: &str, digest: &str) -> Result<(), ComponentFailure> {
    let path = tofu_pin_path(user, name);
    publish_bytes_atomic(&path, format!("{digest}\n").as_bytes(), None)
}

// ---------------------------------------------------------------------------
// latest-version resolution (#2381)
// ---------------------------------------------------------------------------

/// Typed failure while resolving a bare `install` from the catalog.
///
/// Kept separate from the rendered message so tests can pin the class (and
/// so the "never guess a version" rule is structural, not prose).
#[derive(Debug, Clone, PartialEq, Eq)]
enum LatestResolveError {
    /// Catalog transport failure (offline, DNS, TLS, HTTP 4xx/5xx).
    CatalogUnavailable { reason: String },
    /// Catalog fetched, but no release exists for this exact name/target.
    NoEntry {
        name: String,
        target: String,
        known: Vec<String>,
    },
}

/// Resolve `(version, digest)` for the latest exact `(name, target)` entry.
///
/// Highest strict `X.Y.Z` wins; a catalog without an entry is `NoEntry`,
/// never a guess. The digest comes from the same entry, so selection and
/// integrity cannot disagree.
fn resolve_latest_entry(
    catalog: &crate::component_seed::ExtensionCatalog,
    name: &str,
    target: &str,
) -> Result<(String, String), LatestResolveError> {
    match catalog.latest_version_for(name, target) {
        Some(version) => {
            let digest = catalog
                .digest_for(name, version, target)
                .expect("latest version implies a catalog entry");
            Ok((version.to_owned(), digest.to_owned()))
        }
        None => Err(LatestResolveError::NoEntry {
            name: name.to_owned(),
            target: target.to_owned(),
            known: catalog
                .names_for_target(target)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }),
    }
}

/// Real seed-release examples used only in pin hints (#2381).
///
/// These are the published artifacts recorded by the catalog evidence
/// (graphics 0.0.24, net 0.0.1); they are hints, never selection input.
fn example_version_for(name: &str) -> Option<&'static str> {
    match name {
        "graphics" => Some("0.0.24"),
        "net" => Some("0.0.1"),
        _ => None,
    }
}

/// Locally installed versions for `name` in the user tier, newest first.
fn local_installed_versions(user: Option<&Path>, name: &str) -> Vec<String> {
    let Some(user) = user else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(user.join(name)) else {
        return Vec::new();
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|version| crate::component_seed::valid_seed_version(version))
        .collect();
    versions.sort_by_key(|version| std::cmp::Reverse(version_sort_key(version)));
    versions
}

/// Numeric sort key for a strict `X.Y.Z` version (grammar-checked first).
fn version_sort_key(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.');
    let next = |parts: &mut std::str::Split<'_, char>| {
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0)
    };
    (next(&mut parts), next(&mut parts), next(&mut parts))
}

/// Concrete pinned-install command for a hint, using a real version when one
/// is known locally or the release is a published example.
fn pinned_install_hint(name: &str, user: Option<&Path>) -> String {
    if let Some(version) = local_installed_versions(user, name).first() {
        return format!("bitty extension install {name} --version {version}");
    }
    match example_version_for(name) {
        Some(version) => format!("bitty extension install {name} --version {version}"),
        None => format!("bitty extension install {name} --version <X.Y.Z>"),
    }
}

/// Render the typed latest-resolution failure with an actionable hint.
fn latest_failure(
    error: LatestResolveError,
    requested_name: &str,
    user: Option<&Path>,
) -> ComponentFailure {
    match error {
        LatestResolveError::CatalogUnavailable { reason } => ComponentFailure::component(format!(
            "bitty extension: install failed: extension catalog unavailable ({reason}); \
                 cannot resolve the latest version without guessing. \
                 Pin an exact release instead: `{}`",
            pinned_install_hint(requested_name, user)
        )),
        LatestResolveError::NoEntry {
            name,
            target,
            known,
        } => {
            let suggestion = closest_known_name(&name, known.iter().map(String::as_str));
            let mut message = match suggestion {
                Some(closest) => format!(
                    "bitty extension: install failed: the extension catalog has no release for '{name}' ({target}); did you mean '{closest}'?\ntry: bitty extension install {closest}"
                ),
                None => format!(
                    "bitty extension: install failed: the extension catalog has no release for '{name}' ({target}) (published here: {})",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                ),
            };
            message.push_str(&format!(
                "\npin an exact release: `{}`",
                pinned_install_hint(&name, user)
            ));
            ComponentFailure::component(message)
        }
    }
}

// Nine args: install needs name/version/digest/force/roots/stdio/consent/
// transport together (all bounded, fail-closed before mutation). A params
// struct would obscure the call-site audit, so allow the lint here.
//
// Note: `install` takes no WSL selection (it fetches the host triple and
// cannot stage a guest helper; `--distro`/env for it is rejected at dispatch
// before this runs, so there is nothing to persist here).
#[allow(clippy::too_many_arguments)]
fn op_install(
    name: &str,
    version_flag: Option<&str>,
    digest_flag: Option<&str>,
    force: bool,
    user_root: Option<&Path>,
    output: &mut dyn std::io::Write,
    input: &mut dyn std::io::BufRead,
    yes: bool,
    transport: &mut dyn crate::component_seed::SeedTransport,
) -> Result<String, ComponentFailure> {
    let Some(user) = user_root else {
        return Err(ComponentFailure::component(
            "bitty extension: no user component root (data directory unavailable; set $XDG_DATA_HOME or $HOME)".to_string(),
        ));
    };
    // Validate name/version before probing the host: hostile input is a
    // usage error (exit 2) on every host, even where no seed target exists.
    if let Err(error) = validate_component_name(name) {
        return Err(seed_failure(
            &crate::component_seed::SeedError::InvalidName(format!("{name:?} ({error})")),
        ));
    }
    if let Some(version) = version_flag {
        if !crate::component_seed::valid_seed_version(version) {
            return Err(seed_failure(
                &crate::component_seed::SeedError::InvalidVersion(version.to_owned()),
            ));
        }
    }
    // Defense in depth: the parser already rejects malformed `--digest` as
    // usage (exit 2); a malformed value reaching here fails the same way
    // before any fetch or staging.
    if let Some(digest) = digest_flag {
        if !crate::component_seed::is_hex_digest(digest) {
            return Err(seed_failure(
                &crate::component_seed::SeedError::InvalidDigest(digest.to_owned()),
            ));
        }
    }
    let target = crate::component_seed::host_target_triple().ok_or_else(|| {
        seed_failure(&crate::component_seed::SeedError::UnsupportedHost(format!(
            "bitty extension: unsupported host {}-{} (no prebuilt seed target; install the component from a local path with `bitty extension add`)",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )))
    })?;
    // A pinned install builds its allowlisted URLs first (pure: hostile input
    // fails here with zero spawn) so consent can name the exact fetch. A bare
    // install has no version yet; its prompt names the catalog and target,
    // and the resolved version is printed after resolution, before download.
    let pinned_urls = match version_flag {
        Some(version) => Some(
            crate::component_seed::seed_urls(name, version, target)
                .map_err(|error| seed_failure(&error))?,
        ),
        None => None,
    };
    // Defense in depth: seed_urls only ever emits the pinned base, but the
    // egress gate owns the decision, so audit both URLs through it. A
    // non-builtin URL (unrepresentable today; future third-party hosts)
    // always takes the confirm path. A bare install only contacts the same
    // builtin base (catalog plus one release).
    let is_builtin = pinned_urls.as_ref().is_none_or(|urls| {
        crate::component_seed::is_builtin_installer_url(&urls.tarball_url)
            && crate::component_seed::is_builtin_installer_url(&urls.manifest_url)
    });
    // Consent precedes *every* transport contact: a decline/EOF exits 1 with
    // zero spawn and no staging (pinned by the no-fetch decline tests). The
    // registry catalog therefore cannot inform the prompt, and unsigned
    // 0.1.0 keeps the explicit-confirm path for every executable download.
    // The predicate is still evaluated so the builtin+dual-digest rule
    // cannot drift silently into an accidental silent path.
    let dual_verified = crate::component_seed::is_dual_digest_verified(None, "", "");
    let level = crate::component_seed::installer_consent_level(is_builtin, dual_verified);
    let needs_consent = level == crate::component_seed::InstallerConsentLevel::Confirm;
    if needs_consent && !yes {
        let approved = match (version_flag, pinned_urls.as_ref()) {
            (Some(version), Some(urls)) => {
                ask_component_install_consent(input, output, name, version, urls)?
            }
            _ => ask_latest_component_install_consent(input, output, name, target)?,
        };
        if !approved {
            return Err(ComponentFailure::generic(format!(
                "bitty extension: download of '{name}' was not approved — nothing changed"
            )));
        }
    } else if needs_consent && yes {
        let _ = match version_flag {
            Some(version) => writeln!(
                output,
                "bitty extension: --yes approved download of '{name}' version {version} from {}",
                crate::component_seed::BUILTIN_INSTALLER_EGRESS
            ),
            None => writeln!(
                output,
                "bitty extension: --yes approved latest-resolution and download of '{name}' ({target}) from {}",
                crate::component_seed::BUILTIN_INSTALLER_EGRESS
            ),
        };
    }
    // Registry backend (#2111, latest #2381): fetch the bounded catalog after
    // consent. A pinned install resolves exactly (name, version, target); a
    // bare install resolves the highest strict X.Y.Z for (name, target) and
    // prints it before the release fetch. A transport failure is the explicit
    // offline fallback for a pinned install only; a bare install cannot fall
    // back without a version and fails closed with a --version hint.
    let catalog_fetch =
        crate::component_seed::fetch_catalog(transport).map_err(|error| seed_failure(&error))?;
    let (version, registry_digest) = match version_flag {
        Some(version) => {
            let digest = match catalog_fetch {
                crate::component_seed::CatalogFetch::Available(catalog) => {
                    match catalog.digest_for(name, version, target) {
                        Some(digest) => {
                            let _ = writeln!(
                                output,
                                "bitty extension: catalog pins tarball digest {digest} for '{name}' {version} ({target})"
                            );
                            Some(digest.to_owned())
                        }
                        None => {
                            let known = catalog.names_for_target(target);
                            let suggestion = closest_known_name(name, known.iter().copied());
                            let mut message = format!(
                                "bitty extension: install failed: extension catalog has no entry for '{name}' version {version} target {target} (refusing to guess; publish the release in {} first)",
                                crate::component_seed::SEED_CATALOG_URL,
                            );
                            if let Some(closest) = suggestion {
                                message
                                    .push_str(&format!("\ntry: bitty extension install {closest}"));
                            }
                            return Err(ComponentFailure::component(message));
                        }
                    }
                }
                crate::component_seed::CatalogFetch::Unavailable(reason) => {
                    let _ = writeln!(
                        output,
                        "bitty extension: extension catalog unavailable ({reason}); falling back to single-release SHA256SUMS verification (no registry pin; explicit consent already required)"
                    );
                    None
                }
            };
            (version.to_owned(), digest)
        }
        None => {
            let catalog = match catalog_fetch {
                crate::component_seed::CatalogFetch::Available(catalog) => catalog,
                crate::component_seed::CatalogFetch::Unavailable(reason) => {
                    return Err(latest_failure(
                        LatestResolveError::CatalogUnavailable { reason },
                        name,
                        Some(user),
                    ));
                }
            };
            let (version, digest) = resolve_latest_entry(&catalog, name, target)
                .map_err(|error| latest_failure(error, name, Some(user)))?;
            let _ = writeln!(
                output,
                "bitty extension: resolved latest version for '{name}' ({target}): {version} (from {})",
                crate::component_seed::SEED_CATALOG_URL
            );
            (version, Some(digest))
        }
    };
    let payload = if let Some(registry_digest) = registry_digest.as_deref() {
        crate::component_seed::fetch_seed_payload_with_registry(
            name,
            &version,
            target,
            digest_flag,
            Some(registry_digest),
            transport,
        )
    } else {
        // Explicit offline fallback: the original single-release pipeline,
        // unchanged (manifest + tarball + exact-two-member audit + TOFU).
        crate::component_seed::fetch_seed_payload(name, &version, target, digest_flag, transport)
    }
    .map_err(|error| seed_failure(&error))?;
    // ABI-compat check before any mutation (same gate as `add`).
    let core = bitty_runtime::component::PROTOCOL_VERSION;
    if !(payload.protocol_min..=payload.protocol_max).contains(&core) {
        return Err(ComponentFailure::component(incompatible_component_hint(
            &payload.name,
            payload.protocol_min,
            payload.protocol_max,
            core,
        )));
    }
    let verified_digest = payload.tarball_digest.clone();
    let staged = StagedSource {
        name: payload.name.clone(),
        version: payload.version.clone(),
        protocol_min: payload.protocol_min,
        protocol_max: payload.protocol_max,
        executable: payload.executable.clone(),
        bytes: payload.bytes,
    };
    // Update-diff preview (issue #1986): when the fetch would change the
    // TOFU pin, render a unified diff old-vs-new BEFORE applying. Preview
    // only — consent and `--force` still apply exactly as before, and the
    // diff is computed locally from fetched-then-pinned bytes (no network).
    // First contact (no pin) renders nothing, so existing flows are
    // byte-identical when there is nothing to preview.
    {
        let exe_digest = bitty_package::integrity::sha256_hex(&staged.bytes);
        let new_descriptor = format!(
            "[component]\nname = \"{}\"\nversion = \"{}\"\nprotocol = [{}, {}]\nexecutable = \"{}\"\nsha256 = \"{exe_digest}\"\n",
            staged.name,
            staged.version,
            staged.protocol_min,
            staged.protocol_max,
            staged.executable,
        );
        let previous = load_update_diff_context(user, &staged.name, &staged.version);
        let pin_would_change = match read_tofu_pin(user, &staged.name) {
            Ok(Some(pinned)) => !crate::component_seed::digests_equal_ct(&pinned, &verified_digest),
            Ok(None) => false,
            // Corrupt pins re-pin only under `--force`; still preview when
            // prior bytes exist so the operator sees the change.
            Err(failure) if failure.exit == EXIT_COMPONENT => previous.is_some(),
            Err(_) => false,
        };
        if pin_would_change {
            render_update_diff_preview(
                output,
                &staged.name,
                &staged.version,
                &new_descriptor,
                &verified_digest,
                previous.as_ref(),
            );
        }
    }
    // TOFU-with-pin (#1906): first contact records the verified CDN digest;
    // later differing digests fail closed as pin-mismatch unless `--force`
    // re-pins. Never auto-updates. Check runs before any mutation so a
    // pin-mismatch stages nothing. A corrupt pin (exit 4) is a mismatch that
    // `--force` may overwrite after verification; filesystem refusals
    // (symlink, non-file, I/O; exit 1) always propagate.
    let (existing_pin, pin_corrupt) = match read_tofu_pin(user, &staged.name) {
        Ok(pin) => (pin, false),
        Err(failure) if failure.exit == EXIT_COMPONENT && force => (None, true),
        Err(failure) => return Err(failure),
    };
    let pin_matches = match existing_pin.as_deref() {
        None => !pin_corrupt,
        Some(pinned) => crate::component_seed::digests_equal_ct(pinned, &verified_digest),
    };
    if !pin_matches && !force {
        let pinned = existing_pin.as_deref().unwrap_or("?");
        return Err(ComponentFailure::component(format!(
            "bitty extension: pin-mismatch for '{}' version {}: pinned tarball digest {pinned} differs from verified CDN digest {verified_digest} (TOFU pin; refusing to auto-update; use --force to re-pin)",
            staged.name, staged.version,
        )));
    }
    let summary = install_staged(&staged, user)?;
    // Record or re-pin the TOFU pin after successful staging (never before,
    // never on failure). First contact writes; `--force` overwrites a
    // differing pin; a matching pin needs no write.
    let needs_pin_write = match existing_pin.as_deref() {
        // First contact and corrupt-pin recovery (force-only) both write.
        None => true,
        Some(_) if force && !pin_matches => true,
        Some(_) => false,
    };
    let pin_note = if needs_pin_write {
        write_tofu_pin(user, &staged.name, &verified_digest)?;
        if existing_pin.is_none() && !pin_corrupt {
            format!(
                "\ntofupin: pinned tarball digest {verified_digest} for '{}' (first contact)",
                staged.name,
            )
        } else {
            format!(
                "\ntofupin: re-pinned tarball digest {verified_digest} for '{}' (--force)",
                staged.name,
            )
        }
    } else {
        String::new()
    };
    // No WSL persistence here by design: `install` fetches the host triple
    // (a Windows PE on Windows hosts) and cannot stage a Linux guest helper.
    // Handoff: the seed is done; name the installed executable so the
    // caller (or the user, for the manager itself) invokes the full
    // component directly from here.
    let executable_path = user
        .join(&staged.name)
        .join(&staged.version)
        .join(executable_file_name(&staged.executable));
    Ok(format!(
        "{summary}\nseed handoff: '{}' is installed and active; the full manager takes over from here{pin_note}",
        executable_path.display()
    ))
}

/// Interactive download consent for `component install` (fails closed).
///
/// Mirrors `plugin.rs` `ask_consent` (`MAX_CONSENT_LINE_BYTES` /
/// `MAX_CONSENT_ATTEMPTS` precedent) for prompts, matching, attempts, and
/// exit codes: returns `Ok(true)` on approval, `Ok(false)` on explicit
/// decline, and `Err` (exit 1, nothing changed) on EOF/read error or too
/// many invalid answers. Unlike the `plugin.rs` precedent, the read itself
/// is bounded (CodeRabbit 4238843895): at most
/// `MAX_COMPONENT_CONSENT_LINE_BYTES + 2` bytes are buffered per answer
/// (room for a 64-byte answer plus `\r\n`); an unterminated longer line is
/// rejected as overlong after its remainder is discarded through bounded
/// `fill_buf` takes, so a large piped answer cannot grow memory without
/// bound and no byte-index truncation can split a UTF-8 character. Portable (generic
/// `BufRead`/`Write`, byte-oriented reads, no Unix-only calls; helpers stay
/// PATH-resolved `curl`/`tar`).
fn ask_component_install_consent(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    name: &str,
    version: &str,
    urls: &crate::component_seed::SeedUrls,
) -> Result<bool, ComponentFailure> {
    let _ = writeln!(
        output,
        "bitty extension: install '{name}' version {version} downloads an executable from the builtin installer egress {}:",
        crate::component_seed::BUILTIN_INSTALLER_EGRESS
    );
    let _ = writeln!(output, "  manifest: {}", urls.manifest_url);
    let _ = writeln!(output, "  tarball:  {}", urls.tarball_url);
    let _ = writeln!(
        output,
        "  catalog:  {} (optional registry metadata)",
        crate::component_seed::SEED_CATALOG_URL
    );
    let _ = writeln!(
        output,
        "  trust: CDN SHA256SUMS plus the extension catalog digest when published (all-available-must-match); TOFU-pinned, unsigned 0.1.0 — explicit confirm required."
    );
    confirm_component_download(
        input,
        output,
        &format!("Grant download of '{name}' version {version}? [y/N]: "),
    )
}

/// Interactive consent for a bare `install` (latest resolution, #2381).
///
/// Consent must precede every transport contact, so the catalog has not been
/// fetched yet and the version is unknown at prompt time. The prompt names
/// the catalog, the exact host target, and the trust chain; the resolved
/// version is printed immediately after resolution and always before the
/// release download. Decline/EOF behave exactly like the pinned prompt.
fn ask_latest_component_install_consent(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    name: &str,
    target: &str,
) -> Result<bool, ComponentFailure> {
    let _ = writeln!(
        output,
        "bitty extension: install '{name}' resolves the latest published release for {target} from the extension catalog:"
    );
    let _ = writeln!(
        output,
        "  catalog:  {}",
        crate::component_seed::SEED_CATALOG_URL
    );
    let _ = writeln!(
        output,
        "  egress:   {} (same host as the release download)",
        crate::component_seed::BUILTIN_INSTALLER_EGRESS
    );
    let _ = writeln!(
        output,
        "  trust: the catalog is fetched over TLS and must pin the release digest (all-available-must-match with CDN SHA256SUMS); latest is convenience, not a security guarantee. TOFU-pinned, unsigned 0.1.0 — explicit confirm required."
    );
    confirm_component_download(
        input,
        output,
        &format!("Resolve and download the latest '{name}' release for {target}? [y/N]: "),
    )
}

/// Bounded y/n prompt shared by both install consent shapes.
///
/// Returns `Ok(true)` on approval, `Ok(false)` on explicit decline, and
/// `Err` (exit 1, nothing changed) on EOF/read error or too many invalid
/// answers.
fn confirm_component_download(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    question: &str,
) -> Result<bool, ComponentFailure> {
    for attempt in 1..=MAX_COMPONENT_CONSENT_ATTEMPTS {
        let _ = write!(output, "{question}");
        let _ = output.flush();
        let Some(answer) = read_consent_answer(input, output, attempt) else {
            continue;
        };
        let answer = answer?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => {
                let _ = writeln!(
                    output,
                    "  (answer y or n — try again [{attempt}/{MAX_COMPONENT_CONSENT_ATTEMPTS}])"
                );
            }
        }
    }
    Err(ComponentFailure::generic(format!(
        "bitty extension: aborted (too many invalid answers, limit {MAX_COMPONENT_CONSENT_ATTEMPTS}) — nothing changed"
    )))
}

/// Read one consent answer with a bounded buffer.
///
/// Returns `None` when the answer was overlong (already reported; the
/// caller retries without consuming an extra attempt branch), otherwise
/// `Some(Ok(line))` for a complete line or `Some(Err(..))` on EOF/read
/// error/invalid UTF-8 (fail closed, nothing changed).
fn read_consent_answer(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    attempt: usize,
) -> Option<Result<String, ComponentFailure>> {
    // Window: a 64-byte answer plus a `\r\n` terminator. A line that does
    // not terminate inside the window is overlong and rejected. Bytes are
    // taken via `fill_buf`/`consume` (never a wide `read`) so a single
    // answer never swallows the start of the next line.
    const WINDOW: usize = MAX_COMPONENT_CONSENT_LINE_BYTES + 2;
    let mut answer: Vec<u8> = Vec::with_capacity(WINDOW);
    let mut terminated = false;
    let mut failed = false;
    while answer.len() < WINDOW {
        let ate: usize = match input.fill_buf() {
            Ok(&[]) => break, // EOF: line ends here, if any.
            Err(_) => {
                failed = true;
                break;
            }
            Ok(buffered) => {
                let mut end = buffered.len().min(WINDOW - answer.len());
                if let Some(pos) = buffered[..end].iter().position(|&b| b == b'\n') {
                    end = pos + 1;
                    terminated = true;
                }
                answer.extend_from_slice(&buffered[..end]);
                end
            }
        };
        input.consume(ate);
        if terminated {
            break;
        }
    }
    if failed || (answer.is_empty() && !terminated) {
        return Some(Err(ComponentFailure::generic(
            "bitty extension: aborted (end of input) — nothing changed".to_string(),
        )));
    }
    // A short EOF-terminated line (window not filled) is a complete
    // answer; only a filled window without a newline is overlong.
    if !terminated && answer.len() < WINDOW {
        terminated = true;
    }
    if !terminated {
        // Overlong: discard the line remainder (never past its newline,
        // so a queued next answer survives) so the next attempt starts
        // fresh. Fixed-window takes keep memory bounded no matter how
        // long the piped line is.
        loop {
            match input.fill_buf() {
                Ok(&[]) | Err(_) => break,
                Ok(buffered) => {
                    if let Some(pos) = buffered.iter().position(|&b| b == b'\n') {
                        let ate = pos + 1;
                        input.consume(ate);
                        break;
                    }
                    let ate = buffered.len();
                    input.consume(ate);
                }
            }
        }
        let _ = writeln!(
            output,
            "  (answer too long — at most {MAX_COMPONENT_CONSENT_LINE_BYTES} bytes — try again [{attempt}/{MAX_COMPONENT_CONSENT_ATTEMPTS}])"
        );
        return None;
    }
    match String::from_utf8(answer) {
        Ok(line) => Some(Ok(line)),
        Err(_) => Some(Err(ComponentFailure::generic(
            "bitty extension: aborted (end of input) — nothing changed".to_string(),
        ))),
    }
}

fn load_staged_source(
    source: &str,
    version_flag: Option<&str>,
) -> Result<StagedSource, ComponentFailure> {
    let path = Path::new(source);
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ComponentFailure::component(format!(
            "bitty extension: cannot read source {source:?}: {error} ({})",
            missing_component_hint_for_add()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ComponentFailure::component(format!(
            "bitty extension: source {source:?} is a symlink (refusing to follow)"
        )));
    }
    if metadata.is_dir() {
        load_staged_dir(path)
    } else if metadata.is_file() {
        load_staged_file(path, version_flag)
    } else {
        Err(ComponentFailure::component(format!(
            "bitty extension: source {source:?} is not a directory or file"
        )))
    }
}

fn missing_component_hint_for_add() -> String {
    "provide a local directory or `bitty-<name>` executable path".to_string()
}

fn load_staged_dir(dir: &Path) -> Result<StagedSource, ComponentFailure> {
    // No-obfuscation rule (issue #1986): refuse minified Lua in the source
    // tree before reading the descriptor, so obfuscated sources never stage
    // silently. Native-binary-only sources (no Lua files) are unaffected.
    check_staged_lua_readability(dir)?;
    let descriptor_path = dir.join(COMPONENT_DESCRIPTOR_FILE);
    let bytes = std::fs::read(&descriptor_path).map_err(|error| {
        ComponentFailure::component(format!(
            "bitty extension: cannot read '{}': {error} (a source directory must hold \
             bitty-component.toml plus the executable)",
            descriptor_path.display()
        ))
    })?;
    if bytes.len() > bitty_runtime::component::COMPONENT_DESCRIPTOR_MAX_BYTES {
        return Err(ComponentFailure::component(format!(
            "bitty extension: '{}' exceeds {} bytes",
            descriptor_path.display(),
            bitty_runtime::component::COMPONENT_DESCRIPTOR_MAX_BYTES
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        ComponentFailure::component(format!(
            "bitty extension: '{}' is not UTF-8",
            descriptor_path.display()
        ))
    })?;
    // Source descriptors carry (name, version, protocol, executable) and an
    // optional `sha256` that must match the computed digest. Reuse the
    // installed parser when `sha256` is present; otherwise inject a dummy
    // digest to validate the remaining fields with the same strict rules.
    let with_digest = ComponentDescriptor::parse(text).ok();
    let (name, version, protocol_min, protocol_max, executable, expected_digest) = match with_digest
    {
        Some(descriptor) => (
            descriptor.name,
            descriptor.version,
            descriptor.protocol_min,
            descriptor.protocol_max,
            descriptor.executable,
            Some(descriptor.sha256),
        ),
        None => {
            // Distinguish "missing sha256" (tolerated) from other errors.
            let dummy = "0".repeat(64);
            let padded = format!("{text}\nsha256 = \"{dummy}\"\n");
            match ComponentDescriptor::parse(&padded) {
                Ok(descriptor) => (
                    descriptor.name,
                    descriptor.version,
                    descriptor.protocol_min,
                    descriptor.protocol_max,
                    descriptor.executable,
                    None,
                ),
                Err(error) => {
                    return Err(ComponentFailure::component(format!(
                        "bitty extension: invalid source descriptor '{}': {error}",
                        descriptor_path.display()
                    )));
                }
            }
        }
    };
    let executable_path = dir.join(executable_file_name(&executable));
    let executable_bytes = read_executable(&executable_path)?;
    let actual = bitty_package::integrity::sha256_hex(&executable_bytes);
    if let Some(expected) = expected_digest {
        if expected != actual {
            return Err(ComponentFailure::component(format!(
                "bitty extension: source descriptor sha256 {expected} does not match \
                 computed digest {actual} for '{}'",
                executable_path.display()
            )));
        }
    }
    Ok(StagedSource {
        name,
        version,
        protocol_min,
        protocol_max,
        executable,
        bytes: executable_bytes,
    })
}

fn load_staged_file(
    path: &Path,
    version_flag: Option<&str>,
) -> Result<StagedSource, ComponentFailure> {
    let Some(version) = version_flag else {
        return Err(ComponentFailure::usage(
            "bitty extension: `add <executable>` needs `--version <semver>` (a directory source carries its version in bitty-component.toml)".to_string(),
        ));
    };
    if bitty_package::Version::parse(version).is_err() {
        return Err(ComponentFailure::usage(format!(
            "bitty extension: invalid --version {version:?} (want strict semver, e.g. 0.0.1)"
        )));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ComponentFailure::component(format!(
                "bitty extension: source '{}' has no file name",
                path.display()
            ))
        })?;
    // Strip the platform suffix (`bitty-<name>.exe` on Windows) before the
    // `bitty-<name>` grammar check.
    let stem = file_name
        .strip_suffix(std::env::consts::EXE_SUFFIX)
        .filter(|_| !std::env::consts::EXE_SUFFIX.is_empty())
        .unwrap_or(file_name);
    let name = stem.strip_prefix(COMPONENT_EXECUTABLE_PREFIX).ok_or_else(|| {
        ComponentFailure::component(format!(
            "bitty extension: executable {file_name:?} must be named `bitty-<name>` (name matches [a-z][a-z0-9-]{{0,31}})"
        ))
    })?;
    if let Err(error) = validate_component_name(name) {
        return Err(ComponentFailure::component(format!(
            "bitty extension: invalid component name derived from {file_name:?}: {error}"
        )));
    }
    let executable = format!("{COMPONENT_EXECUTABLE_PREFIX}{name}");
    let executable_bytes = read_executable(path)?;
    let core = bitty_runtime::component::PROTOCOL_VERSION;
    Ok(StagedSource {
        name: name.to_owned(),
        version: version.to_owned(),
        protocol_min: core,
        protocol_max: core,
        executable,
        bytes: executable_bytes,
    })
}

fn read_executable(path: &Path) -> Result<Vec<u8>, ComponentFailure> {
    let bytes = std::fs::read(path).map_err(|error| {
        ComponentFailure::component(format!(
            "bitty extension: cannot read executable '{}': {error}",
            path.display()
        ))
    })?;
    if bytes.len() as u64 > bitty_runtime::component::COMPONENT_EXECUTABLE_MAX_BYTES {
        return Err(ComponentFailure::component(format!(
            "bitty extension: executable '{}' exceeds {} bytes",
            path.display(),
            bitty_runtime::component::COMPONENT_EXECUTABLE_MAX_BYTES
        )));
    }
    if bytes.is_empty() {
        return Err(ComponentFailure::component(format!(
            "bitty extension: executable '{}' is empty",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Monotonic suffix for atomic-publish temp siblings of one destination.
static PUBLISH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Unique temp sibling for an atomic publish (`<file>.tmp-<pid>-<seq>`).
///
/// The sibling lives in the destination directory so the final rename stays
/// on one filesystem (atomic). Process id plus a process-global counter
/// keeps concurrent publishers of the same path from sharing a temp file.
fn temp_sibling_for(destination: &Path) -> PathBuf {
    let file_name = destination.file_name().map_or_else(
        || "component".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let temp_name = format!(
        "{file_name}.tmp-{}-{}",
        std::process::id(),
        PUBLISH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(temp_name),
        _ => PathBuf::from(temp_name),
    }
}

/// Atomically publishes `bytes` to `destination` (temp write, fsync, rename).
///
/// Mirrors the session/KV backends: the parent directory is created, bytes
/// go to a unique temp sibling in the same directory, the file is fsynced,
/// then the temp is renamed onto the destination (atomic on one filesystem)
/// and the parent directory is synced. On unix `mode` is applied to the temp
/// before the rename so the published file is immediately correct; `None`
/// keeps the default creation mode (0666 masked by umask). On any failure
/// the temp is removed and the previous destination is left untouched, so a
/// crash mid-publish never exposes a partial file.
fn publish_bytes_atomic(
    destination: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<(), ComponentFailure> {
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ComponentFailure::generic(format!(
                    "bitty extension: cannot create '{}': {error}",
                    parent.display()
                ))
            })?;
        }
    }
    let temp = temp_sibling_for(destination);
    // A pid-reusing predecessor could have left a twin under the same name;
    // dropping it keeps our unique temp truly ours.
    let _ = std::fs::remove_file(&temp);
    let outcome = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(mode.unwrap_or(0o666))
                .open(&temp)?
        };
        #[cfg(not(unix))]
        let mut file = {
            let _ = mode;
            std::fs::File::create(&temp)?
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, destination)?;
        if let Some(parent) = destination.parent() {
            if !parent.as_os_str().is_empty() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        let _ = std::fs::remove_file(&temp);
        return Err(ComponentFailure::generic(format!(
            "bitty extension: cannot publish '{}': {error}",
            destination.display()
        )));
    }
    Ok(())
}

pub(crate) fn install_staged(
    staged: &StagedSource,
    user: &Path,
) -> Result<String, ComponentFailure> {
    let component_dir = user.join(&staged.name);
    let version_dir = component_dir.join(&staged.version);
    // Fail closed on symlinked install dirs: a symlinked component or
    // version dir would make `create_dir_all`/`write` below escape the user
    // component root, so a pre-existing symlink (or any non-directory)
    // refuses the install before any mutation.
    for path in [&component_dir, &version_dir] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty extension: refusing symlink or non-directory '{}'",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty extension: cannot inspect '{}': {error}",
                    path.display()
                )));
            }
        }
    }
    let descriptor_path = version_dir.join(COMPONENT_DESCRIPTOR_FILE);
    let executable_path = version_dir.join(executable_file_name(&staged.executable));
    let current_path = component_dir.join("current");
    // Fail closed on symlinked install files: `write` follows symlinks, so
    // a pre-existing symlinked executable/descriptor/`current` would escape
    // the version dir. Missing paths are fine (fresh install).
    for path in [&executable_path, &descriptor_path, &current_path] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ComponentFailure::generic(format!(
                    "bitty extension: refusing symlink install path '{}'",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty extension: cannot inspect '{}': {error}",
                    path.display()
                )));
            }
        }
    }
    let digest = bitty_package::integrity::sha256_hex(&staged.bytes);

    // Re-adding an installed version with a different digest fails; the
    // same digest is idempotent (refresh `current`).
    if descriptor_path.is_file() {
        match std::fs::read_to_string(&descriptor_path) {
            Ok(installed_text) => match ComponentDescriptor::parse(&installed_text) {
                Ok(installed) => {
                    if installed.sha256 != digest {
                        return Err(ComponentFailure::component(format!(
                            "bitty extension: version {} of '{}' is already installed with a different digest (refusing to overwrite)",
                            staged.version, staged.name
                        )));
                    }
                }
                Err(_) => {
                    // A corrupt installed descriptor fails closed rather
                    // than being silently overwritten.
                    return Err(ComponentFailure::component(format!(
                        "bitty extension: installed descriptor '{}' is invalid (refusing to overwrite)",
                        descriptor_path.display()
                    )));
                }
            },
            Err(error) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty extension: cannot read '{}': {error}",
                    descriptor_path.display()
                )));
            }
        }
    }

    std::fs::create_dir_all(&version_dir).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty extension: cannot create '{}': {error}",
            version_dir.display()
        ))
    })?;
    // Atomic publish: each destination is written through a sibling temp
    // file and renamed into place, so a crash mid-publish never exposes a
    // partial executable, descriptor, or `current`. `current` is published
    // only after the executable and descriptor are in place.
    publish_bytes_atomic(&executable_path, &staged.bytes, Some(0o755))?;
    let descriptor_text = format!(
        "[component]\nname = \"{}\"\nversion = \"{}\"\nprotocol = [{}, {}]\nexecutable = \"{}\"\nsha256 = \"{digest}\"\n",
        staged.name, staged.version, staged.protocol_min, staged.protocol_max, staged.executable,
    );
    // Defense in depth: never write bytes this module cannot read back.
    ComponentDescriptor::parse(&descriptor_text).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty extension: internal error: rendered descriptor invalid: {error}"
        ))
    })?;
    publish_bytes_atomic(&descriptor_path, descriptor_text.as_bytes(), None)?;
    publish_bytes_atomic(
        &current_path,
        format!("{}\n", staged.version).as_bytes(),
        None,
    )?;
    Ok(format!(
        "installed component '{}' version {} to {} (active)",
        staged.name,
        staged.version,
        version_dir.display()
    ))
}

// ---------------------------------------------------------------------------
// remove (user tier only, never needs root)
// ---------------------------------------------------------------------------

fn op_remove(
    name: &str,
    version: Option<&str>,
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    _output: &mut dyn std::io::Write,
) -> Result<String, ComponentFailure> {
    if let Err(error) = validate_component_name(name) {
        return Err(ComponentFailure::usage(format!(
            "bitty extension: invalid component name {name:?}: {error}"
        )));
    }
    if let Some(version) = version {
        if bitty_package::Version::parse(version).is_err() {
            return Err(ComponentFailure::usage(format!(
                "bitty extension: invalid version {version:?} (want strict semver)"
            )));
        }
    }
    let Some(user) = user_root else {
        return Err(ComponentFailure::component(
            "bitty extension: no user component root (data directory unavailable; set $XDG_DATA_HOME or $HOME)".to_string(),
        ));
    };
    let component_dir = user.join(name);
    let user_has_component = component_dir.is_dir();
    if !user_has_component {
        // Soft-fail with an actionable diagnostic; name the system tier
        // when the component lives there (removal needs a package manager).
        if let Some(system) = system_root {
            if system.join(name).is_dir() {
                return Err(ComponentFailure::component(format!(
                    "bitty extension: '{}' is installed system-wide at {} (`bitty extension remove` only removes user-installed components; use your system package manager)",
                    name,
                    system.join(name).display()
                )));
            }
        }
        return Err(ComponentFailure::component(unknown_component_message(
            "remove",
            name,
            user_root,
            system_root,
        )));
    }

    match version {
        Some(version) => {
            let version_dir = component_dir.join(version);
            if !version_dir.is_dir() {
                return Err(ComponentFailure::component(format!(
                    "bitty extension: version {version} of '{name}' is not installed in the user tier ({})",
                    version_dir.display()
                )));
            }
            std::fs::remove_dir_all(&version_dir).map_err(|error| {
                ComponentFailure::generic(format!(
                    "bitty extension: cannot remove '{}': {error}",
                    version_dir.display()
                ))
            })?;
            // Removing the version named by `current` also removes `current`.
            let current_path = component_dir.join("current");
            if let Ok(current) = std::fs::read_to_string(&current_path) {
                if current.trim_end_matches(['\n', '\r']).trim() == version {
                    let _ = std::fs::remove_file(&current_path);
                }
            }
            // Prune the component dir when the last version is gone and no
            // `current` remains. Activation records and stale writer temps
            // are not installed versions: sweep both so a removed component
            // cannot linger as "(missing version dir)".
            prune_component_dir(&component_dir);
            Ok(format!("removed component '{name}' version {version}"))
        }
        None => {
            std::fs::remove_dir_all(&component_dir).map_err(|error| {
                ComponentFailure::generic(format!(
                    "bitty extension: cannot remove '{}': {error}",
                    component_dir.display()
                ))
            })?;
            Ok(format!("removed component '{name}' (all user versions)"))
        }
    }
}

/// Drop the component directory once no version directory remains.
///
/// Activation records and `activation.tmp-*` leftovers are runtime
/// projections, not installed versions, so they must not keep a removed
/// component visible as a versionless name. New records live in the state
/// directory; the install-tree sweep covers legacy records and crashed
/// writers from older builds.
fn prune_component_dir(component_dir: &Path) {
    let has_versions = std::fs::read_dir(component_dir)
        .map(|entries| entries.flatten().any(|entry| entry.path().is_dir()))
        .unwrap_or(true);
    if has_versions {
        return;
    }
    remove_activation_artifacts(component_dir);
    if let Ok(mut entries) = std::fs::read_dir(component_dir) {
        if entries.next().is_none() {
            let _ = std::fs::remove_dir(component_dir);
        }
    }
}

// ---------------------------------------------------------------------------
// show-source (pinned-byte transparency, issue #1986)
// ---------------------------------------------------------------------------
/// Print the exact pinned bytes of one installed extension (issue #1986).
///
/// Resolution is user-first across both tiers (via `discover_components`):
/// an explicit `version` selects that version, otherwise the active
/// `current` pointer wins. The installed descriptor is parsed, the
/// executable bytes are hashed and compared against the descriptor `sha256`,
/// and only a match is printed — a missing install or a mismatch fails
/// closed (exit 4) with nothing on stdout (never print untrusted bytes
/// silently). Provenance (name, version, tier, path, sha256) goes to stderr
/// so stdout stays byte-identical for piping and hashing. No execution, no
/// network, no mutation.
fn op_show_source(
    name: &str,
    version: Option<&str>,
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    output: &mut dyn std::io::Write,
) -> Result<(), ComponentFailure> {
    if let Err(error) = validate_component_name(name) {
        return Err(ComponentFailure::usage(format!(
            "bitty extension: invalid component name {name:?}: {error}"
        )));
    }
    if let Some(version) = version {
        if bitty_package::Version::parse(version).is_err() {
            return Err(ComponentFailure::usage(format!(
                "bitty extension: invalid version {version:?} (want strict semver)"
            )));
        }
    }
    let summaries = bitty_runtime::component::discover_components(user_root, system_root);
    let Some(summary) = summaries.iter().find(|entry| entry.name == name) else {
        return Err(ComponentFailure::component(unknown_component_message(
            "show-source",
            name,
            user_root,
            system_root,
        )));
    };
    if summary.versions.is_empty() {
        return Err(ComponentFailure::component(unknown_component_message(
            "show-source",
            name,
            user_root,
            system_root,
        )));
    }
    let selected = match version {
        Some(requested) => summary
            .versions
            .iter()
            .find(|entry| entry.version == requested),
        None => match summary.active_version.as_deref() {
            Some(active) => summary
                .versions
                .iter()
                .find(|entry| entry.version == active),
            None => None,
        },
    };
    let Some(installed) = selected else {
        if let Some(requested) = version {
            return Err(ComponentFailure::component(format!(
                "bitty extension: version {requested} of '{name}' is not installed (installed: {})",
                summary
                    .versions
                    .iter()
                    .map(|entry| entry.version.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
        return Err(ComponentFailure::component(format!(
            "bitty extension: '{name}' has no active version (specify one: `bitty extension show-source {name} <version>`; installed: {})",
            summary
                .versions
                .iter()
                .map(|entry| entry.version.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )));
    };
    let descriptor_path = installed.version_dir.join(COMPONENT_DESCRIPTOR_FILE);
    let descriptor_bytes = std::fs::read(&descriptor_path).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty extension: cannot read '{}': {error}",
            descriptor_path.display()
        ))
    })?;
    let descriptor_text = std::str::from_utf8(&descriptor_bytes).map_err(|_| {
        ComponentFailure::component(format!(
            "bitty extension: installed descriptor '{}' is not UTF-8 (refusing to print untrusted bytes)",
            descriptor_path.display()
        ))
    })?;
    let descriptor = ComponentDescriptor::parse(descriptor_text).map_err(|error| {
        ComponentFailure::component(format!(
            "bitty extension: installed descriptor '{}' is invalid: {error} (refusing to print untrusted bytes)",
            descriptor_path.display()
        ))
    })?;
    if descriptor.name != name || descriptor.version != installed.version {
        return Err(ComponentFailure::component(format!(
            "bitty extension: installed descriptor '{}' names {} {} (expected {name} {} — refusing to print untrusted bytes)",
            descriptor_path.display(),
            descriptor.name,
            descriptor.version,
            installed.version,
        )));
    }
    let executable_bytes = std::fs::read(&installed.executable_path).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty extension: cannot read '{}': {error}",
            installed.executable_path.display()
        ))
    })?;
    let actual = bitty_package::integrity::sha256_hex(&executable_bytes);
    if actual != descriptor.sha256 {
        return Err(ComponentFailure::component(format!(
            "bitty extension: hash mismatch for '{name}' version {}: descriptor pins {} but the installed executable hashes {actual} (refusing to print untrusted bytes)",
            installed.version, descriptor.sha256,
        )));
    }
    eprintln!(
        "bitty extension: show-source '{}' version {} from {} tier at {} (sha256 {actual})",
        name,
        installed.version,
        installed.source.label(),
        installed.executable_path.display(),
    );
    output.write_all(&executable_bytes).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty extension: cannot write source bytes: {error}"
        ))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// update-diff preview (issue #1986)
// ---------------------------------------------------------------------------

/// Render a unified diff preview of old-vs-new text (issue #1986).
///
/// Preview only: a whole-file hunk (`--- old` / `+++ new`, removed lines
/// prefixed `-`, added lines prefixed `+`). No LCS, no context collapsing —
/// descriptors are small (<4 KiB) so the full hunk stays readable. Equal
/// inputs render `(no changes)`. Computed locally from fetched-then-pinned
/// bytes; no network. Callers print the result before applying (or refusing)
/// the pin change without altering existing consent/`--force` semantics.
pub(crate) fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "--- {old_label}");
    let _ = writeln!(out, "+++ {new_label}");
    if old == new {
        out.push_str("(no changes)\n");
        return out;
    }
    out.push_str("@@ old vs new @@\n");
    for line in old.lines() {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Old-vs-new descriptor context for one install preview.
struct UpdateDiffContext {
    /// Previously installed version (from `current` or the requested version).
    old_version: String,
    /// Previously installed descriptor text.
    old_descriptor: String,
    /// Previously pinned tarball digest (TOFU pin).
    old_pin: String,
}

/// Load the previous install context for an update-diff preview.
///
/// Returns `None` on first contact (no prior install, nothing to diff) or
/// when the prior descriptor cannot be read (the caller then renders a
/// digest-only preview instead of failing the install on a read error).
fn load_update_diff_context(
    user: &Path,
    name: &str,
    requested_version: &str,
) -> Option<UpdateDiffContext> {
    let old_pin = std::fs::read_to_string(user.join(name).join(TARBALL_PIN_FILE))
        .ok()?
        .trim()
        .to_ascii_lowercase();
    if !crate::component_seed::is_hex_digest(&old_pin) {
        return None;
    }
    // Prefer the active version; fall back to the requested version when no
    // `current` pointer exists (reinstall of the same version path).
    let old_version = std::fs::read_to_string(user.join(name).join("current"))
        .ok()
        .map(|text| text.trim_end_matches(['\n', '\r']).trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| requested_version.to_owned());
    let old_descriptor = std::fs::read_to_string(
        user.join(name)
            .join(&old_version)
            .join(COMPONENT_DESCRIPTOR_FILE),
    )
    .ok()?;
    Some(UpdateDiffContext {
        old_version,
        old_descriptor,
        old_pin,
    })
}

/// Render an update-diff preview to `output` before any pin change.
///
/// Called from `op_install` after the fetch verifies and before the TOFU
/// check stages or refuses: prints `update-diff for '<name>': <old> ->
/// <new>`, the `pin:` digest line, and the descriptor unified diff. Preview
/// only — the caller still enforces consent and `--force` exactly as before.
fn render_update_diff_preview(
    output: &mut dyn std::io::Write,
    name: &str,
    new_version: &str,
    new_descriptor: &str,
    new_pin: &str,
    context: Option<&UpdateDiffContext>,
) {
    use std::fmt::Write as _;
    let Some(previous) = context else {
        return;
    };
    let mut preview = String::new();
    let _ = writeln!(
        preview,
        "update-diff for '{name}': {} -> {new_version} (preview only; consent and --force still apply)",
        previous.old_version,
    );
    let _ = writeln!(preview, "pin: {} -> {new_pin}", previous.old_pin);
    preview.push_str(&unified_diff(
        &previous.old_descriptor,
        new_descriptor,
        &format!(
            "a/{}/{}/{}",
            name, previous.old_version, COMPONENT_DESCRIPTOR_FILE
        ),
        &format!("b/{name}/{new_version}/{COMPONENT_DESCRIPTOR_FILE}"),
    ));
    let _ = output.write_all(preview.as_bytes());
}

/// Maximum Lua source files scanned per staged source dir (bounded walk).
const MAX_STAGED_LUA_FILES: usize = 64;

/// Maximum bytes read per Lua source file for the readability check.
const MAX_STAGED_LUA_FILE_BYTES: u64 = 1024 * 1024;

/// Refuse minified/obfuscated Lua in a staged source dir (issue #1986).
///
/// Scans `<dir>` non-recursively plus `<dir>/lua` one level deep for
/// `*.lua` files (at most [`MAX_STAGED_LUA_FILES`], each at most
/// [`MAX_STAGED_LUA_FILE_BYTES`] bytes). Readable or absent Lua passes;
/// minified Lua (see `is_obfuscated_source_text`) fails closed as a
/// component error. Native-binary-only sources (no Lua files) are unaffected.
fn check_staged_lua_readability(dir: &Path) -> Result<(), ComponentFailure> {
    use bitty_runtime::component::is_obfuscated_source_text;
    let mut candidates: Vec<PathBuf> = Vec::new();
    let collect_lua = |search: &Path, out: &mut Vec<PathBuf>| {
        let Ok(entries) = std::fs::read_dir(search) else {
            return;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_STAGED_LUA_FILES {
                break;
            }
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "lua")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                out.push(path);
            }
        }
    };
    collect_lua(dir, &mut candidates);
    collect_lua(&dir.join("lua"), &mut candidates);
    for path in &candidates {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            ComponentFailure::component(format!(
                "bitty extension: cannot inspect '{}': {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ComponentFailure::component(format!(
                "bitty extension: refusing symlink Lua source '{}'",
                path.display()
            )));
        }
        if metadata.len() > MAX_STAGED_LUA_FILE_BYTES {
            return Err(ComponentFailure::component(format!(
                "bitty extension: Lua source '{}' exceeds {MAX_STAGED_LUA_FILE_BYTES} bytes (refusing to stage)",
                path.display()
            )));
        }
        let bytes = std::fs::read(path).map_err(|error| {
            ComponentFailure::component(format!(
                "bitty extension: cannot read Lua source '{}': {error}",
                path.display()
            ))
        })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ComponentFailure::component(format!(
                "bitty extension: Lua source '{}' is not UTF-8 (refusing to stage)",
                path.display()
            ))
        })?;
        if is_obfuscated_source_text(text) {
            return Err(ComponentFailure::component(format!(
                "bitty extension: Lua source '{}' looks minified/obfuscated (readability rule: max 1000 bytes per line, 200 average; refusing to stage)",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use bitty_runtime::component::system_components_root_for;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-component-cli-{tag}-{}-{}",
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

    fn context_for(user: &Path, system: &Path) -> (ComponentContext<'static>, String, String) {
        // Leak the path strings so the context can borrow them with a
        // `'static` lifetime in tests (freed at process exit).
        let state_path = user
            .parent()
            .map_or_else(|| user.to_path_buf(), |base| base.join("state"));
        let user: &'static str = Box::leak(user.to_string_lossy().into_owned().into_boxed_str());
        let system: &'static str =
            Box::leak(system.to_string_lossy().into_owned().into_boxed_str());
        let state: &'static str =
            Box::leak(state_path.to_string_lossy().into_owned().into_boxed_str());
        (
            ComponentContext {
                xdg_data_home: None,
                home: None,
                components_dir: Some(user),
                system_components_dir: Some(system),
                xdg_state_home: None,
                component_state_dir: Some(state),
                wsl_distro_env: None,
                pre_format: None,
                pre_no_color: false,
            },
            user.to_owned(),
            system.to_owned(),
        )
    }

    fn write_source_dir(dir: &Path, name: &str, version: &str, protocol: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("source dir");
        let executable_name = format!("{COMPONENT_EXECUTABLE_PREFIX}{name}");
        // The descriptor stores the logical name; the file on disk carries
        // the platform suffix (`.exe` on Windows).
        let file_name = executable_file_name(&executable_name);
        std::fs::write(dir.join(&file_name), format!("{name}-{version}-bytes"))
            .expect("executable");
        std::fs::write(
            dir.join(COMPONENT_DESCRIPTOR_FILE),
            format!(
                "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = {protocol}\nexecutable = \"{executable_name}\"\n"
            ),
        )
        .expect("descriptor");
        dir.to_owned()
    }

    fn run_with(context: &ComponentContext<'_>, args: &[&str]) -> (i32, String) {
        run_with_input(context, args, "")
    }

    fn run_with_input(context: &ComponentContext<'_>, args: &[&str], stdin: &str) -> (i32, String) {
        let raw: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let mut buf = Vec::new();
        let code = run_component_subcommand(&raw, context, &mut input, &mut buf);
        (code, String::from_utf8_lossy(&buf).into_owned())
    }

    #[test]
    fn parses_list_add_remove() {
        let request = parse_component_request(&[String::from("list")], None).expect("list");
        assert_eq!(request.verb, ComponentVerb::List);

        let request = parse_component_request(&[String::from("add"), String::from("./dist")], None)
            .expect("add");
        assert_eq!(request.verb, ComponentVerb::Add);
        assert_eq!(request.operand.as_deref(), Some("./dist"));

        let request = parse_component_request(
            &[
                String::from("add"),
                String::from("./bin"),
                String::from("--version"),
                String::from("0.0.1"),
            ],
            None,
        )
        .expect("add with version");
        assert_eq!(request.version_flag.as_deref(), Some("0.0.1"));

        let request = parse_component_request(&[String::from("remove"), String::from("net")], None)
            .expect("remove");
        assert_eq!(request.verb, ComponentVerb::Remove);

        let request = parse_component_request(
            &[
                String::from("remove"),
                String::from("net"),
                String::from("0.0.1"),
            ],
            None,
        )
        .expect("remove with version");
        assert_eq!(request.version_operand.as_deref(), Some("0.0.1"));
    }

    #[test]
    fn parses_install() {
        let request = parse_component_request(
            &[
                String::from("install"),
                String::from("net"),
                String::from("--version"),
                String::from("0.0.23"),
            ],
            None,
        )
        .expect("install");
        assert_eq!(request.verb, ComponentVerb::Install);
        assert_eq!(request.operand.as_deref(), Some("net"));
        assert_eq!(request.version_flag.as_deref(), Some("0.0.23"));
        assert!(!request.yes);
        assert!(request.digest.is_none());
        assert!(!request.force);

        let request = parse_component_request(
            &[
                String::from("install"),
                String::from("net"),
                String::from("--version"),
                String::from("0.0.23"),
                String::from("--yes"),
            ],
            None,
        )
        .expect("install --yes");
        assert!(request.yes);

        // `--version` is optional: omitting it resolves the catalog latest.
        let request =
            parse_component_request(&[String::from("install"), String::from("net")], None)
                .expect("bare install parses");
        assert_eq!(request.version_flag, None);
        // The version comes from the flag, never a second operand.
        assert!(
            parse_component_request(
                &[
                    String::from("install"),
                    String::from("net"),
                    String::from("0.0.1"),
                    String::from("--version"),
                    String::from("0.0.1"),
                ],
                None,
            )
            .is_err()
        );
        // `--format`/`--no-color` only apply to `list`.
        assert!(
            parse_component_request(
                &[
                    String::from("install"),
                    String::from("net"),
                    String::from("--version"),
                    String::from("0.0.1"),
                    String::from("--format"),
                    String::from("json"),
                ],
                None,
            )
            .is_err()
        );
        // `--yes` only applies to `install`.
        assert!(
            parse_component_request(
                &[
                    String::from("add"),
                    String::from("x"),
                    String::from("--yes")
                ],
                None,
            )
            .is_err()
        );
        assert!(
            parse_component_request(&[String::from("list"), String::from("--yes")], None,).is_err()
        );
        assert!(
            parse_component_request(
                &[
                    String::from("remove"),
                    String::from("net"),
                    String::from("--yes")
                ],
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn parses_install_digest_and_force() {
        let digest = "a".repeat(64);
        let request = parse_component_request(
            &[
                String::from("install"),
                String::from("net"),
                String::from("--version"),
                String::from("0.0.23"),
                String::from("--digest"),
                digest.clone(),
                String::from("--force"),
            ],
            None,
        )
        .expect("install --digest --force");
        assert_eq!(request.digest.as_deref(), Some(digest.as_str()));
        assert!(request.force);

        // `--digest=` form plus uppercase hex.
        let upper = "B".repeat(64);
        let request = parse_component_request(
            &[
                String::from("install"),
                String::from("net"),
                String::from("--version"),
                String::from("0.0.23"),
                format!("--digest={upper}"),
            ],
            None,
        )
        .expect("install --digest=upper");
        assert_eq!(request.digest.as_deref(), Some(upper.as_str()));

        // Malformed digests fail as usage (exit 2 at dispatch).
        for bad in [
            "abc".to_string(),
            String::new(),
            "g".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
        ] {
            assert!(
                parse_component_request(
                    &[
                        String::from("install"),
                        String::from("net"),
                        String::from("--version"),
                        String::from("0.0.23"),
                        String::from("--digest"),
                        bad.clone(),
                    ],
                    None,
                )
                .is_err(),
                "malformed --digest must fail: {bad:?}"
            );
        }
        // Missing value, duplicate, and wrong-verb uses fail.
        assert!(
            parse_component_request(
                &[
                    String::from("install"),
                    String::from("net"),
                    String::from("--version"),
                    String::from("0.0.23"),
                    String::from("--digest"),
                ],
                None,
            )
            .is_err()
        );
        assert!(
            parse_component_request(
                &[
                    String::from("install"),
                    String::from("net"),
                    String::from("--version"),
                    String::from("0.0.23"),
                    String::from("--digest"),
                    digest.clone(),
                    format!("--digest={digest}"),
                ],
                None,
            )
            .is_err()
        );
        for verb in ["list", "add", "remove"] {
            let args: Vec<String> = if verb == "list" {
                vec![
                    String::from("list"),
                    String::from("--digest"),
                    digest.clone(),
                ]
            } else if verb == "add" {
                vec![
                    String::from("add"),
                    String::from("x"),
                    String::from("--digest"),
                    digest.clone(),
                ]
            } else {
                vec![
                    String::from("remove"),
                    String::from("net"),
                    String::from("--digest"),
                    digest.clone(),
                ]
            };
            assert!(
                parse_component_request(&args, None).is_err(),
                "--digest on {verb} must fail"
            );
            let force_args: Vec<String> = if verb == "list" {
                vec![String::from("list"), String::from("--force")]
            } else if verb == "add" {
                vec![
                    String::from("add"),
                    String::from("x"),
                    String::from("--force"),
                ]
            } else {
                vec![
                    String::from("remove"),
                    String::from("net"),
                    String::from("--force"),
                ]
            };
            assert!(
                parse_component_request(&force_args, None).is_err(),
                "--force on {verb} must fail"
            );
        }
    }

    #[test]
    fn parses_distro_for_add_only() {
        // `add --distro` parses; `default` selects the `wsl.exe` default
        // distro. Unix validates but stays native (no-op). `install --distro`
        // is rejected at parse (install fetches the host triple and cannot
        // stage a guest helper; cross-target fetching is a follow-up).
        let request = parse_component_request(
            &[
                String::from("add"),
                String::from("./dist"),
                String::from("--distro"),
                String::from("Debian"),
            ],
            None,
        )
        .expect("add --distro");
        assert_eq!(request.distro.as_deref(), Some("Debian"));

        let request = parse_component_request(
            &[
                String::from("add"),
                String::from("./dist"),
                String::from("--distro=default"),
            ],
            None,
        )
        .expect("add --distro=default");
        assert_eq!(request.distro.as_deref(), Some("default"));

        for distro_args in [
            vec![String::from("--distro"), String::from("Ubuntu")],
            vec![String::from("--distro=default")],
        ] {
            let mut args = vec![
                String::from("install"),
                String::from("net"),
                String::from("--version"),
                String::from("0.0.23"),
            ];
            args.extend(distro_args.clone());
            assert!(
                parse_component_request(&args, None).is_err(),
                "install --distro must fail at parse: {distro_args:?}"
            );
        }

        // The CLI bound mirrors the WSL module bound.
        assert_eq!(
            MAX_COMPONENT_DISTRO_BYTES,
            bitty_runtime::component::WSL_DISTRO_MAX_BYTES
        );

        // Malformed, missing, and duplicate values fail as usage (on `add`,
        // where the flag applies; `install` rejects the flag itself above).
        for bad in [
            String::new(),
            "../evil".to_string(),
            "Ubuntu/Debian".to_string(),
            "Ubuntu Debian".to_string(),
            "a".repeat(65),
        ] {
            assert!(
                parse_component_request(
                    &[
                        String::from("add"),
                        String::from("./dist"),
                        String::from("--distro"),
                        bad.clone(),
                    ],
                    None,
                )
                .is_err(),
                "malformed --distro must fail: {bad:?}"
            );
        }
        assert!(
            parse_component_request(
                &[
                    String::from("add"),
                    String::from("./dist"),
                    String::from("--distro")
                ],
                None,
            )
            .is_err()
        );
        assert!(
            parse_component_request(
                &[
                    String::from("add"),
                    String::from("./dist"),
                    String::from("--distro"),
                    String::from("Ubuntu"),
                    String::from("--distro=Debian"),
                ],
                None,
            )
            .is_err()
        );
        // `--distro` only applies to `add`.
        assert!(
            parse_component_request(
                &[
                    String::from("list"),
                    String::from("--distro"),
                    String::from("Ubuntu")
                ],
                None,
            )
            .is_err()
        );
        assert!(
            parse_component_request(
                &[
                    String::from("remove"),
                    String::from("net"),
                    String::from("--distro"),
                    String::from("Ubuntu"),
                ],
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn add_persists_distro_selection_for_later_runs() {
        // `add --distro` stages (never executes) and records placement;
        // omission preserves (never deletes).
        let base = scratch("distro-persist");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let (code, _) = run_with(
            &context,
            &["add", &source.display().to_string(), "--distro", "Ubuntu"],
        );
        assert_eq!(code, EXIT_OK);
        let user_root = Path::new(context.components_dir.expect("user"));
        assert_eq!(
            std::fs::read_to_string(user_root.join("net").join("wsl_distro")).expect("distro file"),
            "Ubuntu\n"
        );
        let persisted = bitty_runtime::component::read_persisted_wsl_distro(&user_root.join("net"))
            .expect("read")
            .expect("selected");
        assert_eq!(
            persisted,
            bitty_runtime::component::WslDistro::Named("Ubuntu".to_string())
        );
        let source2 = write_source_dir(&base.join("source2"), "net", "0.0.1", "[1, 1]");
        let (code, _) = run_with(&context, &["add", &source2.display().to_string()]);
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            std::fs::read_to_string(user_root.join("net").join("wsl_distro")).expect("preserved"),
            "Ubuntu\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_rejects_distro_with_usage_and_no_staging() {
        // `install` fetches the host triple (a Windows PE on Windows hosts)
        // and cannot stage a Linux guest helper: flag and env selections both
        // fail as usage (exit 2) before any fetch or staging, naming `add`.
        let base = scratch("install-distro-reject");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let (code, _) = run_with(
            &context,
            &[
                "install",
                "net",
                "--version",
                "0.0.23",
                "--distro",
                "Ubuntu",
            ],
        );
        assert_eq!(code, EXIT_USAGE);
        assert!(!base.join("user").join("net").exists());

        // Env-only selection (no flag) fails the same way at dispatch.
        let user: &'static str = Box::leak(
            base.join("user")
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        let system: &'static str = Box::leak(
            base.join("system")
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        let env_context = ComponentContext {
            xdg_data_home: None,
            home: None,
            components_dir: Some(user),
            system_components_dir: Some(system),
            xdg_state_home: None,
            component_state_dir: None,
            wsl_distro_env: Some("Ubuntu"),
            pre_format: None,
            pre_no_color: false,
        };
        let (code, _) = run_with(&env_context, &["install", "net", "--version", "0.0.23"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(!base.join("user").join("net").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_unknown_verb_and_flags() {
        assert!(parse_component_request(&[String::from("frobnicate")], None).is_err());
        assert!(
            parse_component_request(&[String::from("list"), String::from("--bogus")], None,)
                .is_err()
        );
        assert!(
            parse_component_request(
                &[String::from("add"), String::from("x"), String::from("--")],
                None,
            )
            .is_err()
        );
        // --format only applies to list.
        assert!(
            parse_component_request(
                &[
                    String::from("add"),
                    String::from("x"),
                    String::from("--format"),
                    String::from("json"),
                ],
                None,
            )
            .is_err()
        );
        // --version only applies to add.
        assert!(
            parse_component_request(&[String::from("remove"), String::from("net")], None,).is_ok()
        );
    }

    #[test]
    fn help_exits_zero() {
        let base = scratch("help");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let (code, stdout) = run_with(&context, &["--help"]);
        // `--help` without a verb is top-level help; component help needs the
        // verb token in this unit path.
        let _ = (code, stdout);
        let raw = vec![String::from("--help")];
        let buf: Vec<u8> = Vec::new();
        let error = parse_component_request(&raw, None).expect_err("help");
        assert_eq!(error, ComponentParseError::Help);
        let _ = buf;
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn help_path_mapping_lines_keep_original_bytes() {
        let help = component_help_text();
        let lines: Vec<&[u8]> = help
            .split_inclusive('\n')
            .filter(|line| {
                line.contains("drive-absolute X:")
                    || line.contains("\\\\wsl$\\")
                    || line.contains("preserved and metacharacters inert.")
            })
            .map(str::as_bytes)
            .collect();

        assert_eq!(
            lines,
            vec![
                b"\x20\x20drive-absolute X:\\... maps to /mnt/<lower-x>/... and WSL UNC\n"
                    .as_slice(),
                b"\x20\x20\\\\wsl$\\<distro>\\... maps to guest-absolute /...; UNC paths bind\n"
                    .as_slice(),
                b"\x20\x20preserved and metacharacters inert. Reverse (/mnt/... to Windows)\n"
                    .as_slice(),
            ]
        );
    }

    #[test]
    fn list_is_empty_without_roots_or_components() {
        let base = scratch("empty");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("(no components)"), "{stdout}");
        let (code, stdout) = run_with(&context, &["list", "--format", "json"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("\"command\":\"component\""), "{stdout}");
        assert!(stdout.contains("\"count\":0"), "{stdout}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_from_directory_then_list_and_remove() {
        let base = scratch("lifecycle");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");

        let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("installed"), "{stdout}");

        let user_root = Path::new(context.components_dir.expect("user"));
        assert!(
            user_root
                .join("net")
                .join("0.0.1")
                .join(COMPONENT_DESCRIPTOR_FILE)
                .is_file()
        );
        assert!(user_root.join("net").join("current").is_file());

        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("net"), "{stdout}");
        assert!(stdout.contains("0.0.1"), "{stdout}");
        assert!(stdout.contains("user"), "{stdout}");

        let (code, stdout) = run_with(&context, &["remove", "net", "0.0.1"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(!user_root.join("net").join("0.0.1").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn list_prints_activation_once_on_the_active_version_row() {
        let base = scratch("activation-per-version");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        for version in ["0.0.1", "0.0.2"] {
            let source = write_source_dir(
                &base.join(format!("source-{version}")),
                "net",
                version,
                "[1, 1]",
            );
            let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
            assert_eq!(code, EXIT_OK, "{stdout}");
        }
        // The record is name-scoped; it belongs to the active version only.
        let state_dir = base.join("state").join("net");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::write(state_dir.join("activation"), "v1 active\n").expect("record");

        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        let rows: Vec<&str> = stdout
            .lines()
            .filter(|line| line.starts_with("net "))
            .collect();
        assert_eq!(rows.len(), 2, "{stdout}");
        let row = |version: &str| {
            rows.iter()
                .find(|line| line.split_whitespace().nth(1) == Some(version))
                .copied()
                .unwrap_or_else(|| panic!("missing row {version}: {stdout}"))
        };
        assert_ne!(
            row("0.0.1").split_whitespace().nth(5),
            Some("active"),
            "superseded version must not repeat the name-scoped label: {stdout}"
        );
        assert_eq!(
            row("0.0.2").split_whitespace().nth(5),
            Some("active"),
            "active version must carry the label: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn list_reads_state_record_for_a_system_only_install() {
        let base = scratch("activation-system");
        let user = base.join("user");
        let system = base.join("system");
        write_source_dir(&system.join("net").join("0.0.1"), "net", "0.0.1", "[1, 1]");
        std::fs::write(system.join("net").join("current"), "0.0.1\n").expect("current");
        let (context, _, _) = context_for(&user, &system);
        // The read-only system install still reports the shared state record.
        let state_dir = base.join("state").join("net");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::write(state_dir.join("activation"), "v1 active\n").expect("record");

        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        let row = stdout
            .lines()
            .find(|line| line.starts_with("net "))
            .expect("system row");
        assert!(row.contains(" system "), "{stdout}");
        assert_eq!(
            row.split_whitespace().nth(5),
            Some("active"),
            "system-only install must report the state record: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn list_labels_activation_as_last_recorded_not_liveness() {
        let help = component_help_text();
        let normalized = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("last activation transition"),
            "help must describe the recorded transition: {help}"
        );
        assert!(
            normalized.contains("not a liveness probe"),
            "help must deny liveness semantics: {help}"
        );
        assert!(
            normalized.contains("active means a spawn was recorded"),
            "help must define active as a recorded spawn: {help}"
        );
        assert!(
            !normalized.contains("activation state"),
            "help must not call the projection a live state: {help}"
        );
        let header = format_list_table(&[], false);
        assert!(header.contains("LAST ACTIVATION"), "{header}");
    }

    #[test]
    fn last_version_removal_sweeps_stale_activation_temps() {
        let base = scratch("activation-temp-prune");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        let component_dir = Path::new(context.components_dir.expect("user")).join("net");
        // Simulate a writer killed between temp creation and rename.
        std::fs::write(
            component_dir.join("activation.tmp-999-0"),
            b"partial-record",
        )
        .expect("stale temp");

        let (code, stdout) = run_with(&context, &["remove", "net", "0.0.1"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(
            !component_dir.exists(),
            "stale activation temp must not keep the removed component visible"
        );
        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(!stdout.contains("net"), "{stdout}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_rejects_abi_mismatch_before_mutation() {
        let base = scratch("abi");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[99, 99]");

        let (code, _) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_COMPONENT);
        let user_root = Path::new(context.components_dir.expect("user"));
        assert!(
            !user_root.join("net").exists(),
            "ABI mismatch must not stage files"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_rejects_remote_source() {
        let base = scratch("remote");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let (code, _) = run_with(&context, &["add", "https://example.com/net.tar.gz"]);
        assert_eq!(code, EXIT_COMPONENT);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_rejects_hostile_name_before_any_fetch() {
        let base = scratch("install-hostile");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        // Hostile names fail as usage errors while building the allowlisted
        // URL, before any transport contact (zero-spawn is pinned with a
        // recording stub in the `component_seed` tests).
        for hostile in [
            "https://evil.example/net.tar.gz",
            "../evil",
            "net;evil",
            "net|evil",
            "Net",
        ] {
            let args = vec![
                "install".to_string(),
                hostile.to_string(),
                "--version".to_string(),
                "0.0.23".to_string(),
            ];
            let mut input = std::io::BufReader::new(&b""[..]);
            let mut buf = Vec::new();
            let code = run_component_subcommand(&args, &context, &mut input, &mut buf);
            assert_eq!(code, EXIT_USAGE, "hostile {hostile:?}");
        }
        let user_root = Path::new(context.components_dir.expect("user"));
        assert!(!user_root.join("net").exists());
        assert!(!user_root.join("evil").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_without_version_requires_consent_before_any_fetch() {
        let base = scratch("install-no-version");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        // Empty stdin is EOF: the consent prompt aborts (exit 1) before the
        // catalog or any release is contacted, and nothing is staged.
        let (code, _) = run_with(&context, &["install", "net"]);
        assert_eq!(code, EXIT_GENERIC);
        let user = Path::new(context.components_dir.expect("user"));
        assert!(!user.join("net").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Stub transport serving one canned, digest-consistent release.
    struct InstallStub {
        tarball: Vec<u8>,
        manifest: String,
        catalog: String,
        descriptor: Vec<u8>,
        executable: Vec<u8>,
        catalog_unavailable: bool,
        fetch_calls: usize,
    }

    impl InstallStub {
        fn canned(name: &str, version: &str, target: &str, exe: &[u8]) -> Self {
            let tarball = format!("canned-tarball-{name}-{version}-{target}").into_bytes();
            let tarball_digest = bitty_package::integrity::sha256_hex(&tarball);
            let exe_digest = bitty_package::integrity::sha256_hex(exe);
            // The stub fetch returns the same tarball bytes for any tarball
            // URL, so the stub is host-independent: the manifest and catalog
            // carry one line per mapped host triple (see `host_target_triple`)
            // all pointing at that single digest, so the real host lookup
            // always hits regardless of where the test runs.
            const HOST_TRIPLES: [&str; 7] = [
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-unknown-linux-musl",
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc",
            ];
            let mut manifest = String::new();
            let mut catalog = String::from("schema 1\n");
            for triple in HOST_TRIPLES {
                manifest.push_str(&format!("{tarball_digest}  {triple}.tar.gz\n"));
                catalog.push_str(&format!(
                    "extension {name} {version} {triple} {tarball_digest}\n"
                ));
            }
            let descriptor = format!(
                "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{name}\"\nsha256 = \"{exe_digest}\"\n"
            )
            .into_bytes();
            Self {
                tarball,
                manifest,
                catalog,
                descriptor,
                executable: exe.to_vec(),
                catalog_unavailable: false,
                fetch_calls: 0,
            }
        }
    }

    impl crate::component_seed::SeedTransport for InstallStub {
        fn fetch(
            &mut self,
            url: &str,
            dest: &Path,
            _max: u64,
        ) -> Result<(), crate::component_seed::SeedError> {
            self.fetch_calls += 1;
            if url == crate::component_seed::SEED_CATALOG_URL && self.catalog_unavailable {
                return Err(crate::component_seed::SeedError::Fetch(
                    "stub catalog offline".to_string(),
                ));
            }
            let bytes = if url == crate::component_seed::SEED_CATALOG_URL {
                self.catalog.as_bytes()
            } else if url.ends_with("SHA256SUMS") {
                self.manifest.as_bytes()
            } else {
                &self.tarball
            };
            if let Some(parent) = dest.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).expect("stub parent");
                }
            }
            std::fs::write(dest, bytes).expect("stub write");
            Ok(())
        }

        fn extract(
            &mut self,
            _archive: &Path,
            dest: &Path,
            _members: &[String],
        ) -> Result<(), crate::component_seed::SeedError> {
            std::fs::write(dest.join(COMPONENT_DESCRIPTOR_FILE), &self.descriptor)
                .expect("stub descriptor");
            // The member name is `bitty-<name>`; the one canned release here
            // is `net`.
            std::fs::write(dest.join("bitty-net"), &self.executable).expect("stub exe");
            Ok(())
        }

        fn list_members(
            &mut self,
            _archive: &Path,
        ) -> Result<Vec<String>, crate::component_seed::SeedError> {
            Ok(vec![
                COMPONENT_DESCRIPTOR_FILE.to_string(),
                "bitty-net".to_string(),
            ])
        }
    }

    #[test]
    fn install_stages_verified_seed_payload_end_to_end() {
        // Truly unmapped hosts (FreeBSD/musl-riscv/...) return
        // UnsupportedHost before the stub is reached, so there is nothing
        // end-to-end to exercise there.
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-e2e");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let user = Path::new(context.components_dir.expect("user")).to_path_buf();
        // Fixed triple keeps the canned tarball bytes stable; the manifest
        // above covers the real host lookup, so the test holds on any mapped
        // host.
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let summary = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("seed install");
        assert!(
            summary.contains("installed component 'net' version 0.0.23"),
            "{summary}"
        );
        assert!(summary.contains("seed handoff"), "{summary}");

        // The same three destinations `add` publishes, then `list` sees it.
        let version_dir = user.join("net").join("0.0.23");
        assert!(version_dir.join(COMPONENT_DESCRIPTOR_FILE).is_file());
        assert_eq!(
            std::fs::read(version_dir.join(executable_file_name(&format!(
                "{COMPONENT_EXECUTABLE_PREFIX}net"
            ))))
            .expect("installed exe"),
            b"seed-net-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(user.join("net").join("current")).expect("current"),
            "0.0.23\n"
        );
        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("net"), "{stdout}");

        // Re-installing the same bytes is idempotent, like `add`.
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("idempotent");

        // Different bytes for the same version fail closed (downgrade by
        // content swap), leaving the install untouched.
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"other-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("content swap must fail");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert_eq!(
            std::fs::read(version_dir.join(executable_file_name(&format!(
                "{COMPONENT_EXECUTABLE_PREFIX}net"
            ))))
            .expect("installed exe"),
            b"seed-net-bytes"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- #2381: aliases, latest resolution, targeted hints ----------------

    /// Parse a catalog for tests (schema + records).
    fn parsed_catalog(records: &[(&str, &str, &str)]) -> crate::component_seed::ExtensionCatalog {
        let mut text = String::from("schema 1\n");
        for (name, version, target) in records {
            text.push_str(&format!(
                "extension {name} {version} {target} {}\n",
                "a".repeat(64)
            ));
        }
        crate::component_seed::parse_catalog(&text).expect("test catalog")
    }

    #[test]
    fn alias_resolution_accepts_one_repo_prefix_and_rejects_ambiguity() {
        let known: BTreeSet<String> = KNOWN_BUILTIN_EXTENSIONS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        assert_eq!(
            resolve_component_name("bitty-graphics", &known).expect("alias"),
            "graphics"
        );
        assert_eq!(
            resolve_component_name("graphics", &known).expect("exact"),
            "graphics"
        );
        assert_eq!(
            resolve_component_name("bitty-net", &known).expect("alias"),
            "net"
        );
        // Only one prefix is stripped; nested prefixes keep the remainder as
        // the selected name and are validated later like any other name.
        assert_eq!(
            resolve_component_name("bitty-bitty-net", &known).expect("nested"),
            "bitty-net"
        );
        assert_eq!(
            resolve_component_name("bitty-", &known).expect("prefix only"),
            "bitty-"
        );
        // A literal known name wins when its unprefixed spelling is unknown.
        let literal: BTreeSet<String> = ["bitty-extra".to_string()].into_iter().collect();
        assert_eq!(
            resolve_component_name("bitty-extra", &literal).expect("literal"),
            "bitty-extra"
        );
        // Both spellings known: fail closed, never guess.
        let mut ambiguous = known.clone();
        ambiguous.insert("bitty-graphics".to_string());
        let failure = resolve_component_name("bitty-graphics", &ambiguous).expect_err("ambiguous");
        assert_eq!(failure.exit, EXIT_COMPONENT);
        assert!(failure.message.contains("ambiguous"), "{}", failure.message);
        assert!(
            failure.message.contains("bitty-graphics") && failure.message.contains("graphics"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn unknown_name_hint_suggests_closest_with_exact_command() {
        let base = scratch("unknown-hint");
        let user = base.join("user");
        let system = base.join("system");
        let message = unknown_component_message("remove", "grapics", Some(&user), Some(&system));
        assert!(message.contains("did you mean 'graphics'"), "{message}");
        assert!(
            message.contains("bitty extension remove graphics"),
            "{message}"
        );
        // A name with no close candidate still names the known set and never
        // dumps usage.
        let message =
            unknown_component_message("show-source", "zzzzzz", Some(&user), Some(&system));
        assert!(message.contains("unknown extension 'zzzzzz'"), "{message}");
        assert!(message.contains("graphics"), "{message}");
        assert!(!message.contains("usage: bitty extension"), "{message}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn latest_resolution_is_typed_and_never_guesses() {
        let target = "x86_64-unknown-linux-gnu";
        let catalog = parsed_catalog(&[
            ("graphics", "0.0.9", target),
            ("graphics", "0.0.10", target),
            ("graphics", "9.9.9", "aarch64-apple-darwin"),
            ("net", "8.0.0", target),
        ]);
        let (version, _) = resolve_latest_entry(&catalog, "graphics", target).expect("latest");
        assert_eq!(version, "0.0.10");

        let error = resolve_latest_entry(&catalog, "ai", target).expect_err("no entry");
        match error {
            LatestResolveError::NoEntry {
                name,
                target: got,
                known,
            } => {
                assert_eq!(name, "ai");
                assert_eq!(got, target);
                assert_eq!(known, vec!["graphics", "net"]);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // An empty (schema-only) catalog is still typed as no-entry.
        let empty = parsed_catalog(&[]);
        assert!(matches!(
            resolve_latest_entry(&empty, "graphics", target),
            Err(LatestResolveError::NoEntry { .. })
        ));
    }

    #[test]
    fn install_without_version_selects_highest_catalog_release() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("latest-e2e");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let host_target = crate::component_seed::host_target_triple().expect("mapped host target");
        // A target guaranteed to differ from the host, so the decoy cannot
        // accidentally become the host's latest on that platform.
        let foreign_target = if host_target == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-gnu"
        } else {
            "aarch64-apple-darwin"
        };
        let mut stub = InstallStub::canned("net", "0.0.10", target, b"latest-net-bytes");
        // Older releases plus foreign-target and other-name decoys: only the
        // highest strict X.Y.Z for the exact host target may win.
        let decoy = "b".repeat(64);
        stub.catalog.push_str(&format!(
            "extension net 0.0.9 {host_target} {decoy}\nextension net 0.0.2 {host_target} {decoy}\nextension net 9.9.9 {foreign_target} {decoy}\nextension graphics 8.0.0 {host_target} {decoy}\n"
        ));
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let summary = op_install(
            "net",
            None,
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("latest install");
        assert!(
            summary.contains("installed component 'net' version 0.0.10"),
            "{summary}"
        );
        let printed = String::from_utf8_lossy(&out).into_owned();
        let host_target = crate::component_seed::host_target_triple().expect("mapped host target");
        assert!(
            printed.contains(&format!(
                "resolved latest version for 'net' ({host_target}): 0.0.10"
            )),
            "resolved version must be printed before download: {printed}"
        );
        assert!(
            user.join("net").join("0.0.10").is_dir(),
            "latest must stage the selected release"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_without_version_decline_fetches_nothing() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("latest-decline");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let mut stub = InstallStub::canned(
            "net",
            "0.0.10",
            "x86_64-unknown-linux-gnu",
            b"latest-net-bytes",
        );
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b"n\n"[..]);
        let error = op_install(
            "net",
            None,
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            false,
            &mut stub,
        )
        .expect_err("decline must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert_eq!(stub.fetch_calls, 0, "decline must not touch the network");
        assert!(!user.join("net").exists(), "decline must stage nothing");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_without_version_catalog_unavailable_fails_with_pin_hint() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("latest-catalog-404");
        let user = base.join("user");
        std::fs::create_dir_all(user.join("net").join("0.0.7")).expect("local version");
        let mut stub = InstallStub::canned(
            "net",
            "0.0.10",
            "x86_64-unknown-linux-gnu",
            b"latest-net-bytes",
        );
        stub.catalog_unavailable = true;
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            None,
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("unavailable catalog cannot resolve a latest version");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(
            error.message.contains("catalog unavailable"),
            "{}",
            error.message
        );
        // A known local version wins the pin hint over the published example.
        assert!(
            error
                .message
                .contains("bitty extension install net --version 0.0.7"),
            "hint must use the known local version: {}",
            error.message
        );
        assert!(!user.join("net").join("0.0.10").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_without_version_empty_catalog_fails_with_pin_hint() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("latest-catalog-empty");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let mut stub = InstallStub::canned(
            "net",
            "0.0.10",
            "x86_64-unknown-linux-gnu",
            b"latest-net-bytes",
        );
        stub.catalog = String::from("schema 1\n");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            None,
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("empty catalog cannot resolve a latest version");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(
            error.message.contains("no release for 'net'"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("pin an exact release")
                && error
                    .message
                    .contains("bitty extension install net --version 0.0.1"),
            "{}",
            error.message
        );
        assert!(!user.join("net").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_pinned_catalog_miss_suggests_known_release() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("pinned-catalog-miss");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        // Catalog only publishes 0.0.10; 0.0.9 is a valid pin that the
        // catalog does not list, so the install fails closed with the
        // published alternatives named.
        let mut stub = InstallStub::canned(
            "net",
            "0.0.10",
            "x86_64-unknown-linux-gnu",
            b"latest-net-bytes",
        );
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.9"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("pinned miss must fail closed");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(
            error.message.contains("no entry for 'net'"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("publish the release"),
            "{}",
            error.message
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_consent_decline_exits_generic_with_no_staging_or_fetch() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-decline");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b"n\n"[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            false,
            &mut stub,
        )
        .expect_err("decline must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert!(
            error.message.contains("not approved"),
            "decline must name the refusal: {}",
            error.message
        );
        assert_eq!(stub.fetch_calls, 0, "decline must not fetch");
        assert!(!user.join("net").exists(), "decline must not stage files");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_consent_eof_exits_generic_with_no_staging_or_fetch() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-eof");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            false,
            &mut stub,
        )
        .expect_err("EOF must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert_eq!(stub.fetch_calls, 0, "EOF must not fetch");
        assert!(!user.join("net").exists(), "EOF must not stage files");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_consent_too_many_invalid_exits_generic_with_no_staging() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-invalid");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b"maybe\nperhaps\n???\n"[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            false,
            &mut stub,
        )
        .expect_err("invalid answers must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert_eq!(stub.fetch_calls, 0, "invalid answers must not fetch");
        assert!(!user.join("net").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_consent_approve_via_stdin_stages() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-approve");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b"y\n"[..]);
        let summary = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            false,
            &mut stub,
        )
        .expect("approval stages");
        assert!(summary.contains("installed component"), "{summary}");
        assert!(user.join("net").join("0.0.23").is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_yes_skips_prompt_and_stages() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-yes");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let user = Path::new(context.components_dir.expect("user")).to_path_buf();
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        // Empty stdin with `--yes` must not prompt (EOF would fail without it).
        let mut input = std::io::BufReader::new(&b""[..]);
        let summary = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("--yes stages without prompting");
        assert!(summary.contains("installed component"), "{summary}");
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- digest arbitration + TOFU pinning (#1906) --------------------------

    fn canned_tarball_digest(name: &str, version: &str, target: &str) -> String {
        let tarball = format!("canned-tarball-{name}-{version}-{target}").into_bytes();
        bitty_package::integrity::sha256_hex(&tarball)
    }

    #[test]
    fn install_digest_match_stages_and_pins() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-digest-ok");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let digest = canned_tarball_digest("net", "0.0.23", target);
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let summary = op_install(
            "net",
            Some("0.0.23"),
            Some(&digest),
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("matching --digest stages");
        assert!(summary.contains("installed component"), "{summary}");
        // First contact records the verified CDN digest.
        let pin =
            std::fs::read_to_string(user.join("net").join(TARBALL_PIN_FILE)).expect("pin file");
        assert_eq!(pin.trim(), digest);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_digest_mismatch_fails_closed() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-digest-mismatch");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let other = "e".repeat(64);
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            Some(&other),
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("expected mismatch must fail");
        // All-available-must-match: integrity failure (exit 4), never a
        // silent pick.
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(
            error.message.contains("does not match expected digest"),
            "{}",
            error.message
        );
        // Fetched (3 calls: catalog + manifest + tarball) but never unpacked
        // or staged.
        assert_eq!(stub.fetch_calls, 3);
        assert!(!user.join("net").exists(), "mismatch must not stage");
        assert!(
            !user.join("net").join(TARBALL_PIN_FILE).exists(),
            "mismatch must not pin"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_refuses_catalog_missing_entry_before_tarball_fetch() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-catalog-missing");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let mut stub = InstallStub::canned(
            "net",
            "0.0.23",
            "x86_64-unknown-linux-gnu",
            b"seed-net-bytes",
        );
        // Valid catalog, but the requested release is not listed: fail closed,
        // never guess from SHA256SUMS alone once the catalog answered.
        stub.catalog = "schema 1\n".to_string();
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("missing catalog entry must fail");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(error.message.contains("no entry"), "{}", error.message);
        assert_eq!(stub.fetch_calls, 1, "only the catalog may be fetched");
        assert!(!user.join("net").exists(), "must not stage");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_refuses_catalog_digest_mismatch_before_unpack() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-catalog-mismatch");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let mut stub = InstallStub::canned(
            "net",
            "0.0.23",
            "x86_64-unknown-linux-gnu",
            b"seed-net-bytes",
        );
        let digest = bitty_package::integrity::sha256_hex(&stub.tarball);
        stub.catalog = stub.catalog.replace(&digest, &"0".repeat(64));
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("catalog mismatch must fail");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(
            error.message.contains("extension catalog digest"),
            "{}",
            error.message
        );
        assert_eq!(stub.fetch_calls, 3);
        assert!(!user.join("net").exists(), "must not stage");
        assert!(
            !user.join("net").join(TARBALL_PIN_FILE).exists(),
            "must not pin"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_refuses_malformed_or_oversize_catalog_before_tarball_fetch() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let cases: &[(&str, String)] = &[
            ("malformed", "schema 1\nlatest net\n".to_string()),
            (
                "oversize",
                "x".repeat(crate::component_seed::SEED_CATALOG_MAX_BYTES as usize + 1),
            ),
        ];
        for (why, catalog) in cases {
            let base = scratch(&format!("install-catalog-{why}"));
            let user = base.join("user");
            std::fs::create_dir_all(&user).expect("user dir");
            let mut stub = InstallStub::canned(
                "net",
                "0.0.23",
                "x86_64-unknown-linux-gnu",
                b"seed-net-bytes",
            );
            stub.catalog = catalog.clone();
            let mut out = Vec::new();
            let mut input = std::io::BufReader::new(&b""[..]);
            let error = op_install(
                "net",
                Some("0.0.23"),
                None,
                false,
                Some(&user),
                &mut out,
                &mut input,
                true,
                &mut stub,
            )
            .expect_err(&format!("{why} catalog must fail"));
            assert_eq!(error.exit, EXIT_COMPONENT, "{why}: {}", error.message);
            assert!(
                error.message.contains("extension catalog"),
                "{why}: {}",
                error.message
            );
            assert_eq!(
                stub.fetch_calls, 1,
                "{why}: only the catalog may be fetched"
            );
            assert!(!user.join("net").exists(), "{why}: must not stage");
            assert!(
                !user.join("net").join(TARBALL_PIN_FILE).exists(),
                "{why}: must not pin"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn install_falls_back_when_catalog_transport_is_unavailable() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-catalog-offline");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let mut stub = InstallStub::canned(
            "net",
            "0.0.23",
            "x86_64-unknown-linux-gnu",
            b"seed-net-bytes",
        );
        stub.catalog_unavailable = true;
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let summary = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("offline fallback must still install");
        assert!(summary.contains("installed"), "{summary}");
        let note = String::from_utf8_lossy(&out);
        assert!(note.contains("catalog unavailable"), "{note}");
        assert!(note.contains("single-release SHA256SUMS"), "{note}");
        // Catalog + manifest + tarball: fallback never skips the release
        // manifest, member audit, or TOFU pin.
        assert_eq!(stub.fetch_calls, 3);
        assert!(
            user.join("net").join(TARBALL_PIN_FILE).is_file(),
            "fallback must pin"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_malformed_digest_fails_as_usage_before_fetch() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-digest-bad");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        for bad in [
            "abc".to_string(),
            String::new(),
            "g".repeat(64),
            "a".repeat(63),
        ] {
            let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
            let mut out = Vec::new();
            let mut input = std::io::BufReader::new(&b""[..]);
            let error = op_install(
                "net",
                Some("0.0.23"),
                Some(bad.as_str()),
                false,
                Some(&user),
                &mut out,
                &mut input,
                true,
                &mut stub,
            )
            .expect_err("malformed --digest must fail");
            assert_eq!(
                error.exit, EXIT_USAGE,
                "malformed {bad:?}: {}",
                error.message
            );
            assert_eq!(stub.fetch_calls, 0, "malformed must not fetch");
            assert!(!user.join("net").exists(), "malformed must not stage");
        }
        // CLI dispatch maps the same shape to exit 2.
        let (context, _, _) = context_for(&base.join("user2"), &base.join("system"));
        let (code, _) = run_with(
            &context,
            &["install", "net", "--version", "0.0.23", "--digest", "abc"],
        );
        assert_eq!(code, EXIT_USAGE);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_tofu_pin_mismatch_fails_closed_until_force() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-pin");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        // First contact pins.
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("first contact pins");
        let pin = std::fs::read_to_string(user.join("net").join(TARBALL_PIN_FILE)).expect("pin");
        assert!(crate::component_seed::is_hex_digest(pin.trim()));

        // Same bytes reinstall is idempotent (pin matches, no --force needed).
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("pin match is idempotent");

        // Different tarball bytes for the same version: pin-mismatch, fail closed.
        // (InstallStub cans the tarball per name/version/target, so mutate the
        // tarball plus its manifest to emulate a rotated release; the exe stays
        // identical so the failure is definitely the pin, not the executable.)
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        stub.tarball = b"rotated-tarball-bytes".to_vec();
        let rotated_digest = bitty_package::integrity::sha256_hex(&stub.tarball);
        {
            const HOST_TRIPLES: [&str; 7] = [
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-unknown-linux-musl",
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc",
            ];
            let mut manifest = String::new();
            let mut catalog = String::from("schema 1\n");
            for triple in HOST_TRIPLES {
                manifest.push_str(&format!("{rotated_digest}  {triple}.tar.gz\n"));
                catalog.push_str(&format!("extension net 0.0.23 {triple} {rotated_digest}\n"));
            }
            stub.manifest = manifest;
            stub.catalog = catalog;
        }
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("pin mismatch must fail");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(error.message.contains("pin-mismatch"), "{}", error.message);
        assert!(error.message.contains("--force"), "{}", error.message);
        // Installed bytes untouched; pin never auto-updated.
        let version_dir = user.join("net").join("0.0.23");
        assert_eq!(
            std::fs::read(version_dir.join(executable_file_name(&format!(
                "{COMPONENT_EXECUTABLE_PREFIX}net"
            ))))
            .expect("installed exe"),
            b"seed-net-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(user.join("net").join(TARBALL_PIN_FILE)).expect("pin"),
            pin,
            "pin must never auto-update"
        );

        // Explicit --force re-pins after verification.
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        stub.tarball = b"rotated-tarball-bytes".to_vec();
        {
            const HOST_TRIPLES: [&str; 7] = [
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-unknown-linux-musl",
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc",
            ];
            let mut manifest = String::new();
            let mut catalog = String::from("schema 1\n");
            for triple in HOST_TRIPLES {
                manifest.push_str(&format!("{rotated_digest}  {triple}.tar.gz\n"));
                catalog.push_str(&format!("extension net 0.0.23 {triple} {rotated_digest}\n"));
            }
            stub.manifest = manifest;
            stub.catalog = catalog;
        }
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let summary = op_install(
            "net",
            Some("0.0.23"),
            None,
            true,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("--force re-pins");
        assert!(summary.contains("re-pinned"), "{summary}");
        let new_pin =
            std::fs::read_to_string(user.join("net").join(TARBALL_PIN_FILE)).expect("pin");
        assert_ne!(new_pin, pin);
        assert_eq!(
            std::fs::read(version_dir.join(executable_file_name(&format!(
                "{COMPONENT_EXECUTABLE_PREFIX}net"
            ))))
            .expect("installed exe"),
            b"seed-net-bytes"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_downgrade_with_differing_digest_is_pin_mismatch() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-downgrade");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        // Install newer version first (pins its digest per component name).
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"newer-bytes");
        // InstallStub always serves the canned tarball for its own version;
        // emulate a newer release by installing 0.0.23 first.
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        // Direct op_install for 0.0.23 with distinct bytes.
        op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("newer pins");
        // Downgrade to an older version with a differing tarball digest:
        // per-name TOFU pin differs, so fail closed as pin-mismatch (not a
        // silent accept), requiring --force.
        let mut older = InstallStub::canned("net", "0.0.1", target, b"older-bytes");
        // The stub serves 0.0.1 bytes but the pin holds the 0.0.23 digest, so
        // the digests necessarily differ.
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        // Bypass version validation by calling the pin path directly: emulate
        // an older release whose tarball digest differs from the pin.
        // Here the stub's tarball digest for 0.0.1 differs from the pinned
        // 0.0.23 digest by construction (different canned bytes).
        let error = op_install(
            "net",
            Some("0.0.1"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut older,
        )
        .expect_err("downgrade with differing digest must fail");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        assert!(error.message.contains("pin-mismatch"), "{}", error.message);
        // With --force the downgrade re-pins explicitly.
        let mut older = InstallStub::canned("net", "0.0.1", target, b"older-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        op_install(
            "net",
            Some("0.0.1"),
            None,
            true,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut older,
        )
        .expect("--force downgrade re-pins");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_single_digest_only_takes_confirm_path() {
        // Single-source-only never verifies; unsigned 0.1.0 stays Confirm.
        let digest = "f".repeat(64);
        assert!(!crate::component_seed::is_dual_digest_verified(
            None, &digest, &digest
        ));
        assert_eq!(
            crate::component_seed::installer_consent_level(true, false),
            crate::component_seed::InstallerConsentLevel::Confirm
        );
        assert_eq!(
            crate::component_seed::installer_consent_level(false, false),
            crate::component_seed::InstallerConsentLevel::Confirm
        );
    }

    #[test]
    fn install_corrupt_pin_fails_closed_without_force_but_recovers_with_force() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        for tag in ["invalid-hex", "oversize", "non-utf8"] {
            let base = scratch(&format!("install-corrupt-pin-{tag}"));
            let user = base.join("user");
            std::fs::create_dir_all(user.join("net")).expect("component dir");
            // Plant a corrupt pin: non-hex text, an oversize blob, or
            // non-UTF-8 bytes (torn write / disk corruption).
            let corrupt: Vec<u8> = if tag == "oversize" {
                vec![b'x'; 200]
            } else if tag == "non-utf8" {
                vec![0xff, 0xfe, 0x00, 0x62]
            } else {
                b"not-a-hex-pin".to_vec()
            };
            std::fs::write(user.join("net").join(TARBALL_PIN_FILE), &corrupt).expect("corrupt pin");
            let target = "x86_64-unknown-linux-gnu";
            // Without --force: fail closed (exit 4) with no staging.
            let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
            let mut out = Vec::new();
            let mut input = std::io::BufReader::new(&b""[..]);
            let error = op_install(
                "net",
                Some("0.0.23"),
                None,
                false,
                Some(&user),
                &mut out,
                &mut input,
                true,
                &mut stub,
            )
            .expect_err("corrupt pin must fail without --force");
            assert_eq!(error.exit, EXIT_COMPONENT, "{tag}: {}", error.message);
            assert!(
                error.message.contains("pin-mismatch"),
                "{tag}: {}",
                error.message
            );
            assert!(
                !user.join("net").join("0.0.23").exists(),
                "{tag}: corrupt pin must not stage"
            );
            // With --force: verification passes, then the corrupt pin is
            // overwritten (re-pinned, not first contact).
            let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
            let mut out = Vec::new();
            let mut input = std::io::BufReader::new(&b""[..]);
            let summary = op_install(
                "net",
                Some("0.0.23"),
                None,
                true,
                Some(&user),
                &mut out,
                &mut input,
                true,
                &mut stub,
            )
            .expect("corrupt pin must recover with --force");
            assert!(summary.contains("re-pinned"), "{tag}: {summary}");
            let pin =
                std::fs::read_to_string(user.join("net").join(TARBALL_PIN_FILE)).expect("pin");
            assert!(
                crate::component_seed::is_hex_digest(pin.trim()),
                "{tag}: re-pinned value must be valid hex"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn install_consent_bounds_match_plugin_precedent() {
        assert_eq!(
            MAX_COMPONENT_CONSENT_LINE_BYTES,
            crate::plugin::MAX_CONSENT_LINE_BYTES
        );
        assert_eq!(
            MAX_COMPONENT_CONSENT_ATTEMPTS,
            crate::plugin::MAX_CONSENT_ATTEMPTS
        );
    }

    fn consent_urls() -> crate::component_seed::SeedUrls {
        crate::component_seed::seed_urls("net", "0.0.23", "x86_64-unknown-linux-gnu")
            .expect("consent urls")
    }

    #[test]
    fn install_consent_overlong_line_is_rejected_and_stream_resyncs() {
        // A 500-byte piped answer must not grow memory without bound: it
        // is rejected as overlong, its remainder discarded, and the next
        // queued answer (`y`) still applies.
        let urls = consent_urls();
        let mut out = Vec::new();
        let stdin = format!("{}\ny\n", "x".repeat(500));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_component_install_consent(&mut input, &mut out, "net", "0.0.23", &urls)
            .expect("resyncs to the queued approval");
        assert!(approved);
        let shown = String::from_utf8_lossy(&out);
        assert!(shown.contains("too long"), "{shown}");
    }

    #[test]
    fn install_consent_multibyte_at_boundary_does_not_panic() {
        // 63 ASCII bytes + `é` (2 bytes) put byte 64 mid-character: the old
        // `truncate(64)` panicked here. The bounded read evaluates the full
        // line instead (invalid answer, no panic), then the queued `n`
        // declines.
        let urls = consent_urls();
        let mut out = Vec::new();
        let stdin = format!("{}é\nn\n", "a".repeat(63));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_component_install_consent(&mut input, &mut out, "net", "0.0.23", &urls)
            .expect("boundary answer must not panic");
        assert!(!approved);
    }

    #[test]
    fn install_consent_overlong_multibyte_resyncs() {
        // 100 `é`s (200 bytes, unterminated in the window) are overlong:
        // discarded, then the queued `n` declines.
        let urls = consent_urls();
        let mut out = Vec::new();
        let stdin = format!("{}\nn\n", "é".repeat(100));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_component_install_consent(&mut input, &mut out, "net", "0.0.23", &urls)
            .expect("multibyte overlong must resync");
        assert!(!approved);
    }

    #[test]
    fn install_consent_overlong_then_eof_aborts_without_staging() {
        // Overlong line with no further input: the retry hits EOF and
        // aborts fail-closed (exit 1), never staging.
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-overlong-eof");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let target = "x86_64-unknown-linux-gnu";
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"seed-net-bytes");
        let mut out = Vec::new();
        let stdin = "x".repeat(500);
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let error = op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            false,
            &mut stub,
        )
        .expect_err("overlong-then-EOF must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert_eq!(stub.fetch_calls, 0, "abort must not fetch");
        assert!(!user.join("net").exists(), "abort must not stage files");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_consent_short_eof_terminated_line_still_counts() {
        // `printf 'n'` (no trailing newline) declines exactly like `n\n`:
        // a short EOF-terminated line is a complete answer, not overlong.
        let urls = consent_urls();
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b"n"[..]);
        let approved = ask_component_install_consent(&mut input, &mut out, "net", "0.0.23", &urls)
            .expect("short EOF-terminated answer counts");
        assert!(!approved);
        let shown = String::from_utf8_lossy(&out);
        assert!(!shown.contains("too long"), "{shown}");
    }

    #[test]
    fn remove_reports_system_only_with_actionable_error() {
        let base = scratch("system-only");
        let user = base.join("user");
        let system = base.join("system");
        std::fs::create_dir_all(user.join("empty")).expect("user dir");
        std::fs::create_dir_all(system.join("net").join("0.0.1")).expect("system component");
        let (context, _, _) = context_for(&user, &system);
        let raw = vec![String::from("remove"), String::from("net")];
        let mut input = std::io::BufReader::new(&b""[..]);
        let mut buf = Vec::new();
        let code = run_component_subcommand(&raw, &context, &mut input, &mut buf);
        assert_eq!(code, EXIT_COMPONENT);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_rejects_non_directory_component_dir() {
        let base = scratch("non-dir-component");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        // A regular file where the component dir belongs fails closed
        // before any mutation (same guard as symlinks, portable).
        std::fs::write(user.join("net"), "not-a-dir").expect("file");
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let mut out = Vec::new();
        let error = op_add(
            &source.display().to_string(),
            None,
            Some(&user),
            &mut out,
            None,
        )
        .expect_err("non-directory component dir must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert!(
            error.message.contains("refusing symlink or non-directory"),
            "{}",
            error.message
        );
        assert!(!user.join("net").join("0.0.1").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinked_component_dir() {
        let base = scratch("symlink-component-dir");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user dir");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");
        std::fs::write(outside.join("marker"), "outside").expect("marker");
        std::os::unix::fs::symlink(&outside, user.join("net")).expect("symlink");
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let mut out = Vec::new();
        let error = op_add(
            &source.display().to_string(),
            None,
            Some(&user),
            &mut out,
            None,
        )
        .expect_err("symlinked component dir must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert!(
            error.message.contains("refusing symlink"),
            "{}",
            error.message
        );
        // Nothing escaped through the link.
        assert!(!outside.join("0.0.1").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).expect("marker"),
            "outside"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinked_version_dir() {
        let base = scratch("symlink-version-dir");
        let user = base.join("user");
        std::fs::create_dir_all(user.join("net")).expect("component dir");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");
        std::fs::write(outside.join("marker"), "outside").expect("marker");
        std::os::unix::fs::symlink(&outside, user.join("net").join("0.0.1")).expect("symlink");
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let mut out = Vec::new();
        let error = op_add(
            &source.display().to_string(),
            None,
            Some(&user),
            &mut out,
            None,
        )
        .expect_err("symlinked version dir must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert!(
            error.message.contains("refusing symlink"),
            "{}",
            error.message
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).expect("marker"),
            "outside"
        );
        assert!(!outside.join(COMPONENT_DESCRIPTOR_FILE).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinked_install_file() {
        let base = scratch("symlink-install-file");
        let user = base.join("user");
        let version_dir = user.join("net").join("0.0.1");
        std::fs::create_dir_all(&version_dir).expect("version dir");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");
        std::fs::write(outside.join("secret"), "secret").expect("secret");
        let executable_name = format!("{COMPONENT_EXECUTABLE_PREFIX}net");
        std::os::unix::fs::symlink(
            outside.join("secret"),
            version_dir.join(executable_file_name(&executable_name)),
        )
        .expect("symlink");
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let mut out = Vec::new();
        let error = op_add(
            &source.display().to_string(),
            None,
            Some(&user),
            &mut out,
            None,
        )
        .expect_err("symlinked executable path must fail");
        assert_eq!(error.exit, EXIT_GENERIC, "{}", error.message);
        assert!(
            error.message.contains("refusing symlink install path"),
            "{}",
            error.message
        );
        // The link target is untouched.
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).expect("secret"),
            "secret"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_default_resolver_is_used_when_no_override() {
        assert_eq!(
            system_components_root_for(None).expect("default"),
            PathBuf::from(bitty_runtime::component::SYSTEM_COMPONENTS_DIR_DEFAULT)
        );
    }

    fn has_temp_sibling(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".tmp-") || name.contains(".tmp.") {
                return true;
            }
            if entry.path().is_dir() && has_temp_sibling(&entry.path()) {
                return true;
            }
        }
        false
    }

    #[test]
    fn publish_leaves_no_temp_siblings() {
        let base = scratch("atomic-no-litter");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");

        let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK, "{stdout}");

        let user_root = Path::new(context.components_dir.expect("user"));
        assert!(
            !has_temp_sibling(&user_root.join("net")),
            "atomic publish must not leave temp siblings behind"
        );
        // All three destinations are fully published.
        assert!(
            user_root
                .join("net")
                .join("0.0.1")
                .join(COMPONENT_DESCRIPTOR_FILE)
                .is_file()
        );
        assert_eq!(
            std::fs::read_to_string(user_root.join("net").join("current")).expect("current"),
            "0.0.1\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let executable = user_root
                .join("net")
                .join("0.0.1")
                .join(executable_file_name(&format!(
                    "{COMPONENT_EXECUTABLE_PREFIX}net"
                )));
            let mode = std::fs::metadata(&executable)
                .expect("mode")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "published executable must be 0755");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_temp_litter_is_invisible_to_list() {
        let base = scratch("atomic-litter-invisible");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK, "{stdout}");

        // Simulate crashed-publish litter: temp siblings next to each
        // destination must never become visible to discovery.
        let user_root = Path::new(context.components_dir.expect("user"));
        let version_dir = user_root.join("net").join("0.0.1");
        let executable_name = executable_file_name(&format!("{COMPONENT_EXECUTABLE_PREFIX}net"));
        std::fs::write(
            version_dir.join(format!("{executable_name}.tmp-999-0")),
            b"partial-bytes",
        )
        .expect("litter executable");
        std::fs::write(
            version_dir.join(format!("{COMPONENT_DESCRIPTOR_FILE}.tmp-999-1")),
            b"partial",
        )
        .expect("litter descriptor");
        std::fs::write(user_root.join("net").join("current.tmp-999-2"), b"0.")
            .expect("litter current");

        let (code, stdout) = run_with(&context, &["list"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("net"), "{stdout}");
        assert!(stdout.contains("0.0.1"), "{stdout}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn readd_with_different_digest_fails_without_overwrite() {
        let base = scratch("atomic-digest");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK, "{stdout}");

        let user_root = Path::new(context.components_dir.expect("user"));
        let executable_name = executable_file_name(&format!("{COMPONENT_EXECUTABLE_PREFIX}net"));
        let installed = version_dir_bytes(&user_root.join("net").join("0.0.1"), &executable_name);

        // Same name/version with different bytes: must fail closed and leave
        // the installed executable untouched (no partial overwrite).
        let source2 = write_source_dir(&base.join("source2"), "net", "0.0.1", "[1, 1]");
        std::fs::write(source2.join(&executable_name), b"different-bytes").expect("mutate");
        // Failure details go to stderr (not the captured stdout buffer), so
        // only the exit code is asserted here; the CLI integration test
        // below covers the diagnostic text.
        let (code, _) = run_with(&context, &["add", &source2.display().to_string()]);
        assert_eq!(code, EXIT_COMPONENT);
        assert_eq!(
            version_dir_bytes(&user_root.join("net").join("0.0.1"), &executable_name),
            installed,
            "failed re-add must not overwrite the installed executable"
        );
        assert!(
            !has_temp_sibling(&user_root.join("net")),
            "failed publish must not leave temp siblings behind"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn version_dir_bytes(version_dir: &Path, executable_name: &str) -> Vec<u8> {
        std::fs::read(version_dir.join(executable_name)).expect("installed executable")
    }

    #[test]
    fn parses_show_source() {
        let request =
            parse_component_request(&[String::from("show-source"), String::from("net")], None)
                .expect("show-source");
        assert_eq!(request.verb, ComponentVerb::ShowSource);
        assert_eq!(request.operand.as_deref(), Some("net"));
        assert!(request.version_operand.is_none());

        let request = parse_component_request(
            &[
                String::from("show-source"),
                String::from("net"),
                String::from("0.0.1"),
            ],
            None,
        )
        .expect("show-source with version");
        assert_eq!(request.version_operand.as_deref(), Some("0.0.1"));

        // Missing name fails.
        assert!(parse_component_request(&[String::from("show-source")], None).is_err());
        // `--version` flag form fails (operand form only, like `remove`).
        assert!(
            parse_component_request(
                &[
                    String::from("show-source"),
                    String::from("net"),
                    String::from("--version"),
                    String::from("0.0.1"),
                ],
                None,
            )
            .is_err()
        );
        // Install-only flags fail on `show-source`.
        for args in [
            vec![
                String::from("show-source"),
                String::from("net"),
                String::from("--yes"),
            ],
            vec![
                String::from("show-source"),
                String::from("net"),
                String::from("--force"),
            ],
            vec![
                String::from("show-source"),
                String::from("net"),
                String::from("--distro"),
                String::from("Ubuntu"),
            ],
            vec![
                String::from("show-source"),
                String::from("net"),
                String::from("--distro=Ubuntu"),
            ],
        ] {
            assert!(
                parse_component_request(&args, None).is_err(),
                "{args:?} on show-source must fail"
            );
        }
        // `--format`/`--no-color` only apply to `list`.
        assert!(
            parse_component_request(
                &[
                    String::from("show-source"),
                    String::from("net"),
                    String::from("--format"),
                    String::from("json"),
                ],
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn show_source_prints_pinned_bytes_and_fails_closed() {
        let base = scratch("show-source");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        let (code, _) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK);

        // Active version resolves without an explicit operand.
        let (code, stdout) = run_with(&context, &["show-source", "net"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert_eq!(
            stdout, "net-0.0.1-bytes",
            "stdout must be the exact pinned bytes"
        );

        // Explicit version agrees.
        let (code, stdout) = run_with(&context, &["show-source", "net", "0.0.1"]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert_eq!(stdout, "net-0.0.1-bytes");

        // Unknown component fails closed (exit 4).
        let (code, _) = run_with(&context, &["show-source", "missing"]);
        assert_eq!(code, EXIT_COMPONENT);
        // Unknown version fails closed.
        let (code, _) = run_with(&context, &["show-source", "net", "9.9.9"]);
        assert_eq!(code, EXIT_COMPONENT);

        // Tampered executable fails closed with nothing on stdout.
        let user_root = Path::new(context.components_dir.expect("user"));
        let executable_name = executable_file_name(&format!("{COMPONENT_EXECUTABLE_PREFIX}net"));
        std::fs::write(
            user_root.join("net").join("0.0.1").join(&executable_name),
            b"tampered-bytes",
        )
        .expect("tamper");
        let (code, stdout) = run_with(&context, &["show-source", "net"]);
        assert_eq!(code, EXIT_COMPONENT);
        assert!(
            stdout.is_empty(),
            "tampered bytes must never reach stdout: {stdout:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unified_diff_renders_old_vs_new() {
        let old = "[component]\nversion = \"0.0.1\"\n";
        let new = "[component]\nversion = \"0.0.2\"\n";
        let diff = unified_diff(old, new, "a/net", "b/net");
        assert!(diff.contains("--- a/net"), "{diff}");
        assert!(diff.contains("+++ b/net"), "{diff}");
        assert!(diff.contains("-version = \"0.0.1\""), "{diff}");
        assert!(diff.contains("+version = \"0.0.2\""), "{diff}");

        let same = unified_diff(old, old, "a/net", "b/net");
        assert!(same.contains("(no changes)"), "{same}");
    }

    #[test]
    fn install_renders_update_diff_preview_before_pin_refusal() {
        if crate::component_seed::host_target_triple().is_none() {
            return;
        }
        let base = scratch("install-diff-preview");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let user = Path::new(context.components_dir.expect("user")).to_path_buf();
        let target = "x86_64-unknown-linux-gnu";
        // First contact stages silently (no preview: nothing old to diff).
        let mut stub = InstallStub::canned("net", "0.0.23", target, b"v1-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        op_install(
            "net",
            Some("0.0.23"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("first install");
        assert!(
            !String::from_utf8_lossy(&out).contains("update-diff"),
            "first contact must not preview"
        );
        // Upgrading to a new version rotates the tarball digest, so the TOFU
        // pin would change: without `--force` the install still refuses
        // (exit 4) but previews the descriptor diff on stdout first.
        let mut stub = InstallStub::canned("net", "0.0.24", target, b"v2-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        let error = op_install(
            "net",
            Some("0.0.24"),
            None,
            false,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect_err("pin change without --force must fail");
        assert_eq!(error.exit, EXIT_COMPONENT, "{}", error.message);
        let preview = String::from_utf8_lossy(&out).into_owned();
        assert!(preview.contains("update-diff for 'net'"), "{preview}");
        assert!(preview.contains("0.0.23 -> 0.0.24"), "{preview}");
        assert!(preview.contains("pin: "), "{preview}");
        assert!(preview.contains("--- "), "{preview}");
        assert!(preview.contains("+++ "), "{preview}");
        // `--force` previews too, then stages the new version.
        let mut stub = InstallStub::canned("net", "0.0.24", target, b"v2-bytes");
        let mut out = Vec::new();
        let mut input = std::io::BufReader::new(&b""[..]);
        op_install(
            "net",
            Some("0.0.24"),
            None,
            true,
            Some(&user),
            &mut out,
            &mut input,
            true,
            &mut stub,
        )
        .expect("force re-pin");
        let preview = String::from_utf8_lossy(&out).into_owned();
        assert!(preview.contains("update-diff for 'net'"), "{preview}");
        assert_eq!(
            std::fs::read(
                user.join("net")
                    .join("0.0.24")
                    .join(executable_file_name(&format!(
                        "{COMPONENT_EXECUTABLE_PREFIX}net"
                    )))
            )
            .expect("installed exe"),
            b"v2-bytes"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = context;
    }

    #[test]
    fn add_refuses_obfuscated_lua_in_source_dir() {
        let base = scratch("lua-readability");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        // Readable Lua stages fine.
        let source = write_source_dir(&base.join("source"), "net", "0.0.1", "[1, 1]");
        std::fs::write(
            source.join("helper.lua"),
            "-- helper\nlocal M = {}\nfunction M.bump(n)\n  return n + 1\nend\nreturn M\n",
        )
        .expect("readable lua");
        let (code, stdout) = run_with(&context, &["add", &source.display().to_string()]);
        assert_eq!(code, EXIT_OK, "{stdout}");

        // Minified single-line Lua fails closed.
        let source2 = write_source_dir(&base.join("source2"), "net", "0.0.2", "[1, 1]");
        std::fs::write(
            source2.join("helper.lua"),
            format!("local x={};", "a".repeat(1200)),
        )
        .expect("minified lua");
        let (code, _) = run_with(&context, &["add", &source2.display().to_string()]);
        assert_eq!(code, EXIT_COMPONENT);

        // Minified Lua under `lua/` is also refused.
        let source3 = write_source_dir(&base.join("source3"), "net", "0.0.3", "[1, 1]");
        std::fs::create_dir_all(source3.join("lua")).expect("lua dir");
        std::fs::write(
            source3.join("lua").join("init.lua"),
            format!("local y={};", "b".repeat(1200)),
        )
        .expect("minified lua subdir");
        let (code, _) = run_with(&context, &["add", &source3.display().to_string()]);
        assert_eq!(code, EXIT_COMPONENT);
        let _ = std::fs::remove_dir_all(&base);
    }
}
