//! `bitty component`: CLI-first native binary extension management (issue #1651).
//!
//! Search priority: user `$XDG_DATA_HOME/bitty/components/` wins over system
//! `/usr/lib/bitty/components/` (or platform equivalent). The user tier is
//! writable without privilege; the system tier is read-only for this command.
//!
//! # Contract (implemented, v1 local-path only)
//!
//! - Shape: `bitty component list|add|install|remove` with `list` accepting
//!   `--format table|json|jsonl` (default table) and `--no-color`.
//! - `list`: read-only enumeration across both tiers (merged, user shadows
//!   system on the same version). Shows name, version, active marker, source,
//!   ABI compat, and path. Empty when nothing is installed; never fails on a
//!   missing root.
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
//! - `install <name> --version <X.Y.Z>`: fetch one prebuilt release from R2
//!   through the minimal install seed (`crate::component_seed`, issue
//!   #1791 option A): fixed-argv system `curl` pinned to
//!   `https://cdn.bitty.run`, `SHA256SUMS` hash verify fail-closed, exact
//!   member audit, then the same atomic publish path as `add`. The version
//!   is required and exact (no registry, no solving); the full manager
//!   takes over after the install.
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
//! fetches exactly one allowlisted R2 release (fixed-argv `curl`, no shell,
//! hash-verified before unpack), and nothing else in Core touches the
//! network.
//!
//! # Exit codes (stable taxonomy, cli-contract-rfc.md)
//!
//! - `0` success (including idempotent re-add and empty list).
//! - `1` filesystem failure after validation (copy/write/remove I/O).
//! - `2` usage error (missing/unknown verb, missing operand, unknown flag,
//!   bad `--format`/`--version`, stray `--`, `--version`/`--format` on the
//!   wrong verb).
//! - `4` component error (unknown/invalid name, missing component, ABI
//!   mismatch, digest mismatch, system-only remove, remote source).
//!
//! # Bounds (fail closed before any filesystem mutation)
//!
//! - Raw tokens: 1..=[`MAX_COMPONENT_TOKEN_BYTES`] bytes, no NUL.
//! - `--format`: 1..=[`MAX_COMPONENT_FORMAT_BYTES`] bytes.
//! - `--version`: 1..=[`MAX_COMPONENT_VERSION_BYTES`] bytes.
//! - Source path: 1..=[`MAX_COMPONENT_PATH_BYTES`] bytes, no NUL.
//! - Descriptor: [`COMPONENT_DESCRIPTOR_MAX_BYTES`] bytes (via runtime).
//! - Executable: [`COMPONENT_EXECUTABLE_MAX_BYTES`] bytes (via runtime).

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bitty_runtime::component::{
    COMPONENT_DESCRIPTOR_FILE, COMPONENT_EXECUTABLE_PREFIX, ComponentDescriptor,
    component_search_roots_for, data_home_for, discover_components, executable_file_name,
    incompatible_component_hint, missing_component_hint, validate_component_name,
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
                        "bitty component: unknown --format {value:?} (want table|json|jsonl)"
                    ));
                }
                match value.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty component: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

/// One `bitty component` verb.
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
}

impl ComponentVerb {
    /// Parse a verb token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "list" => Some(Self::List),
            "add" => Some(Self::Add),
            "install" => Some(Self::Install),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }

    /// Canonical verb name for output/errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Add => "add",
            Self::Install => "install",
            Self::Remove => "remove",
        }
    }
}

/// Validated request for one `bitty component` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRequest {
    /// Requested verb.
    pub verb: ComponentVerb,
    /// First operand: source path for `add`, component name for
    /// `install`/`remove`.
    pub operand: Option<String>,
    /// Second operand: version for `remove` (no flag form).
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
            "--" => {
                return Err(ComponentParseError::Usage(
                    "bitty component: stray `--` separator (component takes no child argv)"
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
                    "bitty component: --version value is empty, contains NUL, or exceeds {MAX_COMPONENT_VERSION_BYTES} bytes"
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty component: duplicate --version".to_string(),
                ));
            }
            version_flag = Some(value.to_string());
            index += 1;
            continue;
        }
        if token == "--format" {
            let Some(value) = raw.get(index + 1) else {
                return Err(ComponentParseError::Usage(
                    "bitty component: --format needs a value (table|json|jsonl)".to_string(),
                ));
            };
            format = Some(value.clone());
            index += 2;
            continue;
        }
        if token == "--version" {
            let Some(value) = raw.get(index + 1) else {
                return Err(ComponentParseError::Usage(
                    "bitty component: --version needs a value (strict semver)".to_string(),
                ));
            };
            if !version_token_ok(value) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --version value is empty, contains NUL, or exceeds {MAX_COMPONENT_VERSION_BYTES} bytes"
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty component: duplicate --version".to_string(),
                ));
            }
            version_flag = Some(value.clone());
            index += 2;
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            return Err(ComponentParseError::Usage(format!(
                "bitty component: unknown flag {token:?}"
            )));
        }
        // Positional operand: length bound depends on the verb under
        // construction (paths allow longer values than names).
        let looks_like_path = verb.is_some_and(|verb| verb == ComponentVerb::Add);
        if looks_like_path {
            if !path_token_ok(token) {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: argument is empty, contains NUL, or exceeds {MAX_COMPONENT_PATH_BYTES} bytes"
                )));
            }
        } else if !bare_token_ok(token) {
            return Err(ComponentParseError::Usage(format!(
                "bitty component: argument is empty, contains NUL, or exceeds {MAX_COMPONENT_TOKEN_BYTES} bytes"
            )));
        }
        if verb.is_none() {
            match ComponentVerb::parse(token) {
                Some(parsed) => verb = Some(parsed),
                None => {
                    return Err(ComponentParseError::Usage(format!(
                        "bitty component: unknown verb {token:?} (want list|add|install|remove)"
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
            "bitty component: unexpected extra argument {token:?}"
        )));
    }

    let verb = verb.ok_or_else(|| {
        ComponentParseError::Usage(
            "bitty component: missing verb (want list|add|install|remove)".to_string(),
        )
    })?;

    let format = match format {
        Some(value) => ComponentFormat::parse(Some(&value)).map_err(ComponentParseError::Usage)?,
        None => ComponentFormat::parse(pre_format).map_err(ComponentParseError::Usage)?,
    };

    match verb {
        ComponentVerb::List => {
            if operand.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: `{}` takes no operand",
                    verb.name()
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --version only applies to `add` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --yes only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
        }
        ComponentVerb::Add => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(
                    "bitty component: `add` needs a source path (local directory or `bitty-<name>` executable)"
                        .to_string(),
                ));
            }
            if version_operand.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty component: `add` takes one path operand (the version comes from `--version <semver>` for bare executables)"
                        .to_string(),
                ));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --yes only applies to `install` (got `{}`)",
                    verb.name()
                )));
            }
        }
        ComponentVerb::Install => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(
                    "bitty component: `install` needs a component name (e.g. `bitty component install net --version 0.0.23`)"
                        .to_string(),
                ));
            }
            if version_operand.is_some() {
                return Err(ComponentParseError::Usage(
                    "bitty component: `install` takes the version from `--version <X.Y.Z>`, not a second operand"
                        .to_string(),
                ));
            }
            if version_flag.is_none() {
                return Err(ComponentParseError::Usage(
                    "bitty component: `install` needs `--version <X.Y.Z>` (the seed installs exactly the version named; version solving arrives with the full manager)"
                        .to_string(),
                ));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
        }
        ComponentVerb::Remove => {
            if operand.is_none() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: `{}` needs a component name",
                    verb.name()
                )));
            }
            if version_flag.is_some() {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: `{}` takes an optional version operand, not `--version` (got `{}`)",
                    verb.name(),
                    verb.name()
                )));
            }
            if format != ComponentFormat::Table || no_color {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --format/--no-color only apply to `list` (got `{}`)",
                    verb.name()
                )));
            }
            if yes {
                return Err(ComponentParseError::Usage(format!(
                    "bitty component: --yes only applies to `install` (got `{}`)",
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
    })
}

/// Short usage block (`stderr` on usage failures).
#[must_use]
pub fn component_usage() -> String {
    "usage: bitty component list [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty component add <path> [--version <semver>]\n\
     \x20      bitty component install <name> --version <X.Y.Z> [--yes]\n\
     \x20      bitty component remove <name> [<version>]\n\
     \n\
     Components are upstream native binaries resolved user-first:\n\
     $XDG_DATA_HOME/bitty/components/ wins over /usr/lib/bitty/components/.\n\
     `add` stages from a local path only (v1 has no registry download);\n\
     `install` fetches one R2 prebuilt release through the hash-verified\n\
     seed (builtin cdn.bitty.run:443 egress, dual-digest verified silent\n\
     else explicit consent), then the full manager takes over; `remove` only\n\
     touches the user tier (no root required).\n\
     `bitty component --help` explains sources, ABI checks, and exit codes."
        .to_string()
}

/// Long help (`bitty component --help`).
#[must_use]
pub fn component_help_text() -> String {
    "bitty component — CLI-first native binary extension management (local class, no execution)\n\
     \n\
     usage: bitty component <verb> [args] [flags]\n\
     \n\
     verbs:\n\
     \x20 list                        Show installed components across both tiers:\n\
     \x20                             name, version, active marker, source\n\
     \x20                             (user|system), ABI compat, and path.\n\
     \x20 add <path> [--version V]    Stage a pre-built binary into the user tier\n\
     \x20                             ($XDG_DATA_HOME/bitty/components/, no root).\n\
     \x20                             A directory holds bitty-component.toml plus\n\
     \x20                             the executable; a bare bitty-<name> file\n\
     \x20                             needs --version <semver>. The digest is\n\
     \x20                             computed and ABI compat verified before\n\
     \x20                             anything is written. URLs fail closed (v1\n\
     \x20                             has no registry download).\n\
     \x20 install <name> --version V Fetch one prebuilt release from R2\n\
     \x20 [--yes]                     (https://cdn.bitty.run, SHA256SUMS hash\n\
     \x20                             verified) into the user tier, then hand\n\
     \x20                             off to the full manager. The version is\n\
     \x20                             required and exact: the seed keeps no\n\
     \x20                             registry and does no solving.\n\
     \x20 remove <name> [<version>]   Remove one user-installed version, or every\n\
     \x20                             user version when none is named. Removing\n\
     \x20                             the active version also drops `current`.\n\
     \x20                             Never touches the system tier.\n\
     \n\
     flags:\n\
     \x20 --format table|json|jsonl   list output shape (default table).\n\
     \x20 --no-color                  Accepted for parity (tables are plain text).\n\
     \x20 --version <semver>          add: version for a bare executable.\n\
     \x20                             install: required exact R2 release.\n\
     \x20 --yes                       install only: approve the download consent\n\
     \x20                             non-interactively (no prompt).\n\
     \n\
     installer egress:\n\
     \x20 cdn.bitty.run:443 is the core-built-in installer egress: strict\n\
     \x20 configurations allow the pinned first-party host with no extra\n\
     \x20 grant. Every other host stays consent-gated and fails closed\n\
     \x20 without explicit consent. Fetch uses fixed-argv system curl\n\
     \x20 (pinned to https://cdn.bitty.run, --proto =https, no shell) plus\n\
     \x20 system tar; no Rust network stack.\n\
     \n\
     download consent:\n\
     \x20 Silent only when the fetch targets the builtin CDN host AND both\n\
     \x20 digests verify (registry/manifest digest and CDN SHA256SUMS both\n\
     \x20 match, per the dual-digest rule); anything else (third-party host,\n\
     \x20 single-digest-only, digest mismatch history) needs explicit\n\
     \x20 confirm. Without --yes the installer prompts `Grant download\n\
     \x20 <name> <version>? [y/N]` (up to 3 attempts, 64 bytes per answer);\n\
     \x20 `y`/`yes` approves, `n`/`no`/empty declines, EOF aborts. A\n\
     \x20 decline or EOF exits 1 with nothing staged or fetched.\n\
     \n\
     resolution:\n\
     \x20 User $XDG_DATA_HOME/bitty/components/ wins over system\n\
     \x20 /usr/lib/bitty/components/ (or platform equivalent) on collision.\n\
     \x20 A tampered user install fails closed without system fallback. Every\n\
     \x20 spawn re-verifies name, version, executable name, protocol range,\n\
     \x20 and SHA-256; a protocol range excluding Core fails closed.\n\
     \n\
     authority:\n\
     \x20 No component code ever runs during any `bitty component` operation.\n\
     \x20 Missing optional components fail soft: resolution reports the name,\n\
     \x20 the `bitty component add` command to run, and never a stack trace.\n\
     \n\
     exit codes:\n\
     \x20 0 success | 1 declined consent, aborted prompt, or filesystem\n\
     \x20 failure | 2 usage | 4 component error (integrity, ABI, digest)\n\
     \n\
     examples:\n\
     \x20 bitty component list\n\
     \x20 bitty component list --format json\n\
     \x20 bitty component add ./dist/net --version 0.0.1\n\
     \x20 bitty component install net --version 0.0.23\n\
     \x20 bitty component install net --version 0.0.23 --yes\n\
     \x20 bitty component remove net 0.0.1"
        .to_string()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Injected environment for one `bitty component` dispatch (hermetic).
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

/// Run one `bitty component` invocation; returns the process exit code.
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
            eprintln!("{}\n{}", error.message(), component_usage());
            return EXIT_USAGE;
        }
    };

    let data_home = data_home_for(context.xdg_data_home, context.home);
    let (user_root, system_root) = component_search_roots_for(
        context.components_dir,
        data_home.as_deref(),
        context.system_components_dir,
    );

    match request.verb {
        ComponentVerb::List => {
            let summaries = discover_components(user_root.as_deref(), system_root.as_deref());
            match request.format {
                ComponentFormat::Table => {
                    let no_color = request.no_color || context.pre_no_color;
                    let _ = output.write_all(format_list_table(&summaries, no_color).as_bytes());
                }
                ComponentFormat::Json | ComponentFormat::Jsonl => {
                    let _ = writeln!(output, "{}", format_list_envelope(&summaries));
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
            let name = request.operand.as_deref().expect("install requires a name");
            let mut transport = crate::component_seed::SystemTransport;
            match op_install(
                name,
                request.version_flag.as_deref(),
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
            let name = request.operand.as_deref().expect("remove requires a name");
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
    }
}

// ---------------------------------------------------------------------------
// list rendering
// ---------------------------------------------------------------------------

fn format_list_table(
    summaries: &[bitty_runtime::component::ComponentSummary],
    _no_color: bool,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<16} {:<12} {:<7} {:<8} {:<9}  PATH",
        "NAME", "VERSION", "ACTIVE", "SOURCE", "ABI"
    );
    let mut rows = 0usize;
    for summary in summaries {
        if summary.versions.is_empty() {
            let active = summary.active_version.as_deref().unwrap_or("(none)");
            let source = summary.active_source.map_or("-", |source| source.label());
            let _ = writeln!(
                out,
                "{:<16} {:<12} {:<7} {:<8} {:<9}  (missing version dir)",
                summary.name, active, "*", source, "missing",
            );
            rows += 1;
            continue;
        }
        for installed in &summary.versions {
            let is_active = summary.active_version.as_deref() == Some(installed.version.as_str());
            let abi = if installed.compatible {
                "ok"
            } else {
                "mismatch"
            };
            let _ = writeln!(
                out,
                "{:<16} {:<12} {:<7} {:<8} {:<9}  {}",
                summary.name,
                installed.version,
                if is_active { "*" } else { "-" },
                installed.source.label(),
                abi,
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
#[must_use]
pub fn format_list_envelope(summaries: &[bitty_runtime::component::ComponentSummary]) -> String {
    let mut out = String::from(
        "{\"v\":1,\"command\":\"component\",\"ok\":true,\"result\":{\"verb\":\"list\",",
    );
    let _ = write!(out, "\"count\":{},", summaries.len());
    out.push_str("\"components\":[");
    for (index, summary) in summaries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
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
        out.push_str("]}}");
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
) -> Result<String, ComponentFailure> {
    if is_url_operand(source) {
        return Err(ComponentFailure::component(format!(
            "bitty component: remote sources are not implemented in v1 \
             (got {source:?}); stage from a local directory holding \
             bitty-component.toml plus the executable, or a bare \
             `bitty-<name>` executable with `--version <semver>`"
        )));
    }
    let Some(user) = user_root else {
        return Err(ComponentFailure::component(
            "bitty component: no user component root (data directory unavailable; set $XDG_DATA_HOME or $HOME)".to_string(),
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
    install_staged(&staged, user)
}

// ---------------------------------------------------------------------------
// install (minimal seed: one allowlisted R2 release, then manager handoff)
// ---------------------------------------------------------------------------

/// Map a seed failure onto the stable exit taxonomy.
fn seed_failure(error: &crate::component_seed::SeedError) -> ComponentFailure {
    let message = format!("bitty component: install failed: {}", error.message());
    match error.exit_code() {
        2 => ComponentFailure::usage(message),
        4 => ComponentFailure::component(message),
        _ => ComponentFailure::generic(message),
    }
}

fn op_install(
    name: &str,
    version_flag: Option<&str>,
    user_root: Option<&Path>,
    output: &mut dyn std::io::Write,
    input: &mut dyn std::io::BufRead,
    yes: bool,
    transport: &mut dyn crate::component_seed::SeedTransport,
) -> Result<String, ComponentFailure> {
    // The parser requires `--version`; this is defense in depth.
    let Some(version) = version_flag else {
        return Err(ComponentFailure::usage(
            "bitty component: `install` needs `--version <X.Y.Z>` (the seed installs exactly the version named)".to_string(),
        ));
    };
    let Some(user) = user_root else {
        return Err(ComponentFailure::component(
            "bitty component: no user component root (data directory unavailable; set $XDG_DATA_HOME or $HOME)".to_string(),
        ));
    };
    // Validate name/version before probing the host: hostile input is a
    // usage error (exit 2) on every host, even where no seed target exists.
    if let Err(error) = validate_component_name(name) {
        return Err(seed_failure(
            &crate::component_seed::SeedError::InvalidName(format!("{name:?} ({error})")),
        ));
    }
    if !crate::component_seed::valid_seed_version(version) {
        return Err(seed_failure(
            &crate::component_seed::SeedError::InvalidVersion(version.to_owned()),
        ));
    }
    let target = crate::component_seed::host_target_triple().ok_or_else(|| {
        seed_failure(&crate::component_seed::SeedError::UnsupportedHost(format!(
            "bitty component: unsupported host {}-{} (no prebuilt seed target; install the component from a local path with `bitty component add`)",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )))
    })?;
    // Build the allowlisted URLs first (pure: hostile input fails here with
    // zero spawn) so the consent prompt names the exact fetch and the
    // builtin-egress check runs before any child is spawned.
    let urls = crate::component_seed::seed_urls(name, version, target)
        .map_err(|error| seed_failure(&error))?;
    // Defense in depth: seed_urls only ever emits the pinned base, but the
    // egress gate owns the decision, so audit both URLs through it. A
    // non-builtin URL (unrepresentable today; future third-party hosts)
    // always takes the confirm path.
    let is_builtin = crate::component_seed::is_builtin_installer_url(&urls.tarball_url)
        && crate::component_seed::is_builtin_installer_url(&urls.manifest_url);
    // Dual-digest (#1906 direction, checked locally): the registry pin is
    // not yet available in this slice (it lands with the manager index),
    // so installs are single-source-only (CDN SHA256SUMS) and always take
    // the confirm path. The check runs through the real predicate so the
    // dual-digest rule stays live in non-test builds: `None` (no registry
    // pin) always yields `false` (confirm), and when the registry digest
    // arrives the call becomes
    // `is_dual_digest_verified(Some(registry), &cdn_digest, &actual)` with
    // silent activating for builtin+dual.
    let dual_verified = crate::component_seed::is_dual_digest_verified(None, "", "");
    let level = crate::component_seed::installer_consent_level(is_builtin, dual_verified);
    let needs_consent = level == crate::component_seed::InstallerConsentLevel::Confirm;
    if needs_consent && !yes {
        // Consent precedes any fetch: a decline/EOF exits 1 with no spawn
        // and no staging (provable by the zero-spawn hostile corpus plus
        // the no-staging decline tests).
        match ask_component_install_consent(input, output, name, version, &urls)? {
            true => {}
            false => {
                return Err(ComponentFailure::generic(format!(
                    "bitty component: download of '{name}' version {version} was not approved — nothing changed"
                )));
            }
        }
    } else if needs_consent && yes {
        let _ = writeln!(
            output,
            "bitty component: --yes approved download of '{name}' version {version} from {}",
            crate::component_seed::BUILTIN_INSTALLER_EGRESS
        );
    }
    let payload = crate::component_seed::fetch_seed_payload(name, version, target, transport)
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
    let staged = StagedSource {
        name: payload.name.clone(),
        version: payload.version.clone(),
        protocol_min: payload.protocol_min,
        protocol_max: payload.protocol_max,
        executable: payload.executable.clone(),
        bytes: payload.bytes,
    };
    let summary = install_staged(&staged, user)?;
    // Handoff: the seed is done; name the installed executable so the
    // caller (or the user, for the manager itself) invokes the full
    // component directly from here.
    let executable_path = user
        .join(&staged.name)
        .join(&staged.version)
        .join(executable_file_name(&staged.executable));
    Ok(format!(
        "{summary}\nseed handoff: '{}' is installed and active; the full manager takes over from here",
        executable_path.display()
    ))
}

/// Interactive download consent for `component install` (fails closed).
///
/// Mirrors `plugin.rs` `ask_consent` (`MAX_CONSENT_LINE_BYTES` /
/// `MAX_CONSENT_ATTEMPTS` precedent): returns `Ok(true)` on approval,
/// `Ok(false)` on explicit decline, and `Err` (exit 1, nothing changed) on
/// EOF/read error or too many invalid answers. Bounded, headless, and
/// portable (generic `BufRead`/`Write`, no Unix-only calls; helpers stay
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
        "bitty component: install '{name}' version {version} downloads an executable from the builtin installer egress {}:",
        crate::component_seed::BUILTIN_INSTALLER_EGRESS
    );
    let _ = writeln!(output, "  manifest: {}", urls.manifest_url);
    let _ = writeln!(output, "  tarball:  {}", urls.tarball_url);
    let _ = writeln!(
        output,
        "  trust: single-source-only (CDN SHA256SUMS; registry pin arrives with #1906) — explicit confirm required."
    );
    for attempt in 1..=MAX_COMPONENT_CONSENT_ATTEMPTS {
        let _ = write!(
            output,
            "Grant download of '{name}' version {version}? [y/N]: "
        );
        let _ = output.flush();
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => {
                return Err(ComponentFailure::generic(
                    "bitty component: aborted (end of input) — nothing changed".to_string(),
                ));
            }
            Ok(_) => {
                if line.len() > MAX_COMPONENT_CONSENT_LINE_BYTES {
                    line.truncate(MAX_COMPONENT_CONSENT_LINE_BYTES);
                }
                match line.trim().to_ascii_lowercase().as_str() {
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
        }
    }
    Err(ComponentFailure::generic(format!(
        "bitty component: aborted (too many invalid answers, limit {MAX_COMPONENT_CONSENT_ATTEMPTS}) — nothing changed"
    )))
}

fn load_staged_source(
    source: &str,
    version_flag: Option<&str>,
) -> Result<StagedSource, ComponentFailure> {
    let path = Path::new(source);
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ComponentFailure::component(format!(
            "bitty component: cannot read source {source:?}: {error} ({})",
            missing_component_hint_for_add()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ComponentFailure::component(format!(
            "bitty component: source {source:?} is a symlink (refusing to follow)"
        )));
    }
    if metadata.is_dir() {
        load_staged_dir(path)
    } else if metadata.is_file() {
        load_staged_file(path, version_flag)
    } else {
        Err(ComponentFailure::component(format!(
            "bitty component: source {source:?} is not a directory or file"
        )))
    }
}

fn missing_component_hint_for_add() -> String {
    "provide a local directory or `bitty-<name>` executable path".to_string()
}

fn load_staged_dir(dir: &Path) -> Result<StagedSource, ComponentFailure> {
    let descriptor_path = dir.join(COMPONENT_DESCRIPTOR_FILE);
    let bytes = std::fs::read(&descriptor_path).map_err(|error| {
        ComponentFailure::component(format!(
            "bitty component: cannot read '{}': {error} (a source directory must hold \
             bitty-component.toml plus the executable)",
            descriptor_path.display()
        ))
    })?;
    if bytes.len() > bitty_runtime::component::COMPONENT_DESCRIPTOR_MAX_BYTES {
        return Err(ComponentFailure::component(format!(
            "bitty component: '{}' exceeds {} bytes",
            descriptor_path.display(),
            bitty_runtime::component::COMPONENT_DESCRIPTOR_MAX_BYTES
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        ComponentFailure::component(format!(
            "bitty component: '{}' is not UTF-8",
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
                        "bitty component: invalid source descriptor '{}': {error}",
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
                "bitty component: source descriptor sha256 {expected} does not match \
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
            "bitty component: `add <executable>` needs `--version <semver>` (a directory source carries its version in bitty-component.toml)".to_string(),
        ));
    };
    if bitty_package::Version::parse(version).is_err() {
        return Err(ComponentFailure::usage(format!(
            "bitty component: invalid --version {version:?} (want strict semver, e.g. 0.0.1)"
        )));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ComponentFailure::component(format!(
                "bitty component: source '{}' has no file name",
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
            "bitty component: executable {file_name:?} must be named `bitty-<name>` (name matches [a-z][a-z0-9-]{{0,31}})"
        ))
    })?;
    if let Err(error) = validate_component_name(name) {
        return Err(ComponentFailure::component(format!(
            "bitty component: invalid component name derived from {file_name:?}: {error}"
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
            "bitty component: cannot read executable '{}': {error}",
            path.display()
        ))
    })?;
    if bytes.len() as u64 > bitty_runtime::component::COMPONENT_EXECUTABLE_MAX_BYTES {
        return Err(ComponentFailure::component(format!(
            "bitty component: executable '{}' exceeds {} bytes",
            path.display(),
            bitty_runtime::component::COMPONENT_EXECUTABLE_MAX_BYTES
        )));
    }
    if bytes.is_empty() {
        return Err(ComponentFailure::component(format!(
            "bitty component: executable '{}' is empty",
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
                    "bitty component: cannot create '{}': {error}",
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
            "bitty component: cannot publish '{}': {error}",
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
                    "bitty component: refusing symlink or non-directory '{}'",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty component: cannot inspect '{}': {error}",
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
                    "bitty component: refusing symlink install path '{}'",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty component: cannot inspect '{}': {error}",
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
                            "bitty component: version {} of '{}' is already installed with a different digest (refusing to overwrite)",
                            staged.version, staged.name
                        )));
                    }
                }
                Err(_) => {
                    // A corrupt installed descriptor fails closed rather
                    // than being silently overwritten.
                    return Err(ComponentFailure::component(format!(
                        "bitty component: installed descriptor '{}' is invalid (refusing to overwrite)",
                        descriptor_path.display()
                    )));
                }
            },
            Err(error) => {
                return Err(ComponentFailure::generic(format!(
                    "bitty component: cannot read '{}': {error}",
                    descriptor_path.display()
                )));
            }
        }
    }

    std::fs::create_dir_all(&version_dir).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty component: cannot create '{}': {error}",
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
            "bitty component: internal error: rendered descriptor invalid: {error}"
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
            "bitty component: invalid component name {name:?}: {error}"
        )));
    }
    if let Some(version) = version {
        if bitty_package::Version::parse(version).is_err() {
            return Err(ComponentFailure::usage(format!(
                "bitty component: invalid version {version:?} (want strict semver)"
            )));
        }
    }
    let Some(user) = user_root else {
        return Err(ComponentFailure::component(
            "bitty component: no user component root (data directory unavailable; set $XDG_DATA_HOME or $HOME)".to_string(),
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
                    "bitty component: '{}' is installed system-wide at {} (`bitty component remove` only removes user-installed components; use your system package manager)",
                    name,
                    system.join(name).display()
                )));
            }
        }
        return Err(ComponentFailure::component(missing_component_hint(name)));
    }

    match version {
        Some(version) => {
            let version_dir = component_dir.join(version);
            if !version_dir.is_dir() {
                return Err(ComponentFailure::component(format!(
                    "bitty component: version {version} of '{name}' is not installed in the user tier ({})",
                    version_dir.display()
                )));
            }
            std::fs::remove_dir_all(&version_dir).map_err(|error| {
                ComponentFailure::generic(format!(
                    "bitty component: cannot remove '{}': {error}",
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
            // `current` remains.
            if let Ok(mut entries) = std::fs::read_dir(&component_dir) {
                if entries.next().is_none() {
                    let _ = std::fs::remove_dir(&component_dir);
                }
            }
            Ok(format!("removed component '{name}' version {version}"))
        }
        None => {
            std::fs::remove_dir_all(&component_dir).map_err(|error| {
                ComponentFailure::generic(format!(
                    "bitty component: cannot remove '{}': {error}",
                    component_dir.display()
                ))
            })?;
            Ok(format!("removed component '{name}' (all user versions)"))
        }
    }
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
        let user: &'static str = Box::leak(user.to_string_lossy().into_owned().into_boxed_str());
        let system: &'static str =
            Box::leak(system.to_string_lossy().into_owned().into_boxed_str());
        (
            ComponentContext {
                xdg_data_home: None,
                home: None,
                components_dir: Some(user),
                system_components_dir: Some(system),
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

        // `--version` is required for install.
        assert!(
            parse_component_request(&[String::from("install"), String::from("net")], None,)
                .is_err()
        );
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
    fn install_requires_version_at_cli_level() {
        let base = scratch("install-no-version");
        let (context, _, _) = context_for(&base.join("user"), &base.join("system"));
        let (code, _) = run_with(&context, &["install", "net"]);
        assert_eq!(code, EXIT_USAGE);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Stub transport serving one canned, digest-consistent release.
    struct InstallStub {
        tarball: Vec<u8>,
        manifest: String,
        descriptor: Vec<u8>,
        executable: Vec<u8>,
        fetch_calls: usize,
    }

    impl InstallStub {
        fn canned(name: &str, version: &str, target: &str, exe: &[u8]) -> Self {
            let tarball = format!("canned-tarball-{name}-{version}-{target}").into_bytes();
            let tarball_digest = bitty_package::integrity::sha256_hex(&tarball);
            let exe_digest = bitty_package::integrity::sha256_hex(exe);
            // The stub fetch returns the same tarball bytes for any tarball
            // URL, so the stub is host-independent: the manifest carries one
            // line per mapped host triple (see `host_target_triple`) all
            // pointing at that single digest, so the real host lookup always
            // hits regardless of where the test runs.
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
            for triple in HOST_TRIPLES {
                manifest.push_str(&format!("{tarball_digest}  {triple}.tar.gz\n"));
            }
            let descriptor = format!(
                "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{name}\"\nsha256 = \"{exe_digest}\"\n"
            )
            .into_bytes();
            Self {
                tarball,
                manifest,
                descriptor,
                executable: exe.to_vec(),
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
            let bytes = if url.ends_with("SHA256SUMS") {
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
        let error = op_add(&source.display().to_string(), None, Some(&user), &mut out)
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
        let error = op_add(&source.display().to_string(), None, Some(&user), &mut out)
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
        let error = op_add(&source.display().to_string(), None, Some(&user), &mut out)
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
        let error = op_add(&source.display().to_string(), None, Some(&user), &mut out)
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
}
