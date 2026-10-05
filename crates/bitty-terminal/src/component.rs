//! `bitty component`: CLI-first native binary extension management (issue #1651).
//!
//! Search priority: user `$XDG_DATA_HOME/bitty/components/` wins over system
//! `/usr/lib/bitty/components/` (or platform equivalent). The user tier is
//! writable without privilege; the system tier is read-only for this command.
//!
//! # Contract (implemented, v1 local-path only)
//!
//! - Shape: `bitty component list|add|remove` with `list` accepting
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
//!   before any mutation, and the executable is copied to
//!   `<user>/<name>/<version>/` with an installed descriptor plus `current`.
//!   Re-adding an installed version with a different digest fails. No network
//!   fetch, no registry download: URL operands fail closed.
//! - `remove <name> [<version>]`: remove one user-installed version, or every
//!   user version when none is named; removing the version named by `current`
//!   also removes `current`. Never touches the system tier (needs a package
//!   manager); a system-only component fails with an actionable diagnostic.
//! - Class: local-only (no instance, no IPC, no component code is ever
//!   loaded or executed; safe-mode clean).
//!
//! # Install-source decision (v1)
//!
//! `add` stages from local paths only. Network fetch and registry/download
//! sources are a follow-up (DIR-030 D6: no automatic download in v1). A URL
//! operand fails closed naming the local-path form. This keeps Core
//! network-free (package-manager-boundary DIR-016/DIR-017) and install
//! offline-capable.
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
use std::path::Path;

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
            Self::Remove => "remove",
        }
    }
}

/// Validated request for one `bitty component` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRequest {
    /// Requested verb.
    pub verb: ComponentVerb,
    /// First operand: source path for `add`, component name for `remove`.
    pub operand: Option<String>,
    /// Second operand: version for `remove` (no flag form).
    pub version_operand: Option<String>,
    /// `--version <semver>` for `add` from a bare executable.
    pub version_flag: Option<String>,
    /// Output shape for `list`.
    pub format: ComponentFormat,
    /// `--no-color` (accepted for parity; tables are plain text).
    pub no_color: bool,
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
                        "bitty component: unknown verb {token:?} (want list|add|remove)"
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
            "bitty component: missing verb (want list|add|remove)".to_string(),
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
        }
    }

    Ok(ComponentRequest {
        verb,
        operand,
        version_operand,
        version_flag,
        format,
        no_color,
    })
}

/// Short usage block (`stderr` on usage failures).
#[must_use]
pub fn component_usage() -> String {
    "usage: bitty component list [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty component add <path> [--version <semver>]\n\
     \x20      bitty component remove <name> [<version>]\n\
     \n\
     Components are upstream native binaries resolved user-first:\n\
     $XDG_DATA_HOME/bitty/components/ wins over /usr/lib/bitty/components/.\n\
     `add` stages from a local path only (v1 has no registry download);\n\
     `remove` only touches the user tier (no root required).\n\
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
     \x20 remove <name> [<version>]   Remove one user-installed version, or every\n\
     \x20                             user version when none is named. Removing\n\
     \x20                             the active version also drops `current`.\n\
     \x20                             Never touches the system tier.\n\
     \n\
     flags:\n\
     \x20 --format table|json|jsonl   list output shape (default table).\n\
     \x20 --no-color                  Accepted for parity (tables are plain text).\n\
     \x20 --version <semver>          add only: version for a bare executable.\n\
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
     \x20 0 success | 1 filesystem failure | 2 usage | 4 component error\n\
     \n\
     examples:\n\
     \x20 bitty component list\n\
     \x20 bitty component list --format json\n\
     \x20 bitty component add ./dist/net --version 0.0.1\n\
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
struct ComponentFailure {
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
struct StagedSource {
    name: String,
    version: String,
    protocol_min: u16,
    protocol_max: u16,
    executable: String,
    bytes: Vec<u8>,
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

fn install_staged(staged: &StagedSource, user: &Path) -> Result<String, ComponentFailure> {
    let version_dir = user.join(&staged.name).join(&staged.version);
    let descriptor_path = version_dir.join(COMPONENT_DESCRIPTOR_FILE);
    let executable_path = version_dir.join(executable_file_name(&staged.executable));
    let current_path = user.join(&staged.name).join("current");
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
    std::fs::write(&executable_path, &staged.bytes).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty component: cannot write '{}': {error}",
            executable_path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| {
                ComponentFailure::generic(format!(
                    "bitty component: cannot set mode 0755 on '{}': {error}",
                    executable_path.display()
                ))
            })?;
    }
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
    std::fs::write(&descriptor_path, descriptor_text).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty component: cannot write '{}': {error}",
            descriptor_path.display()
        ))
    })?;
    std::fs::write(&current_path, format!("{}\n", staged.version)).map_err(|error| {
        ComponentFailure::generic(format!(
            "bitty component: cannot write '{}': {error}",
            current_path.display()
        ))
    })?;
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
        let raw: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut buf = Vec::new();
        let code = run_component_subcommand(&raw, context, &mut buf);
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
    fn remove_reports_system_only_with_actionable_error() {
        let base = scratch("system-only");
        let user = base.join("user");
        let system = base.join("system");
        std::fs::create_dir_all(user.join("empty")).expect("user dir");
        std::fs::create_dir_all(system.join("net").join("0.0.1")).expect("system component");
        let (context, _, _) = context_for(&user, &system);
        let raw = vec![String::from("remove"), String::from("net")];
        let mut buf = Vec::new();
        let code = run_component_subcommand(&raw, &context, &mut buf);
        assert_eq!(code, EXIT_COMPONENT);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_default_resolver_is_used_when_no_override() {
        assert_eq!(
            system_components_root_for(None).expect("default"),
            PathBuf::from(bitty_runtime::component::SYSTEM_COMPONENTS_DIR_DEFAULT)
        );
    }
}
