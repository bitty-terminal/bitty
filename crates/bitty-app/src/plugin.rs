//! `bitty plugin`: CLI-first plugin management (CTX-0150, issue #244).
//!
//! Canonical direction: `bitty-docs/docs/product/plugin-roadmap.md` owner
//! direction 2026-09-03 (DEC-0007): management UX is subcommand-first and the
//! durable state is a managed manifest, never hand-edited Lua. Candidate
//! layout and semantics from
//! `bitty-docs/docs/extensibility/package-management.md` and
//! `default-distribution-rfc.md` § "Managed manifest".
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty plugin list|install|remove|enable|disable|info`
//!   with `list`/`info` accepting `--format table|json|jsonl` (default
//!   table) and `--no-color` (accepted; tables are plain text).
//! - `install <id>` resolves a **bundled** plugin manifest, pins its
//!   `PluginManifest::manifest_hash()`, and grants the requested capability
//!   set only after explicit consent (`--yes`, or an interactive `[y/N]`
//!   prompt that fails closed on EOF). A capability increase on an existing
//!   record blocks until approved; unchanged or narrowed sets carry forward
//!   silently (the P0-AC-030 pattern over `bitty-plugin-host` grants).
//! - `remove <id>` is destructive and requires `--force`; the previous
//!   managed manifest is copied to `bitty-plugins.toml.bak` before the
//!   rewrite. `enable`/`disable` are idempotent toggles; `enable` re-checks
//!   the hash pin and the grant coverage and fails closed on a mismatch.
//! - `list` shows every bundled plugin plus any recorded extra, with state
//!   (`enabled`/`disabled`/`available`), pin status, and granted/requested
//!   capability counts. `info` shows one plugin's static manifest plus the
//!   recorded grants and their plain-language effect statements.
//! - Class: local-only (no instance, no IPC, **no plugin VM and no plugin
//!   code is ever loaded or executed**; safe-mode clean). v1 sources are the
//!   bundled catalog (`bitty-terminal.*`); registry/Git/local-path sources
//!   are deferred with the package manager and fail closed with a clear
//!   error.
//!
//! # Durable state
//!
//! Exactly one machine-managed manifest lives beside the config file:
//! `$XDG_CONFIG_HOME/bitty/bitty-plugins.toml` (fallback `~/.config/...`),
//! or the directory of an explicit `--config`/`BITTY_CONFIG` path. The
//! format is a strict, bounded TOML **subset** owned by this module: unknown
//! sections, unknown keys, duplicate keys, malformed values, over-limit
//! files, and unknown version numbers all fail closed before any mutation.
//! Only this module writes it; `bitty-config`'s Lua `init.lua` is never
//! rewritten (package-management.md: arbitrary Lua rewrite is unsafe).
//!
//! # Authority
//!
//! - Every capability identifier is validated through
//!   `bitty-plugin-host`'s closed capability grammar
//!   ([`CapabilityId`]); filesystem requests expand to
//!   `fs.read:PARAM`/`fs.write:PARAM` exactly as the host does.
//! - Nothing is granted implicitly: the recorded grant set is exactly the
//!   manifest's requested set at approval time, bound to the manifest hash.
//! - No ambient authority is created by the CLI: it cannot load plugins, it
//!   cannot widen a grant without a fresh consent, and it never executes
//!   package code (P0-AC-027).
//!
//! # Exit codes (stable taxonomy, cli-contract-rfc.md)
//!
//! - `0` success (including idempotent no-op toggles).
//! - `1` aborted consent (declined/EOF), filesystem failure after approval.
//! - `2` usage error (missing/unknown verb, missing `<id>`, unknown flag,
//!   bad `--format`, stray `--`, malformed plugin id, destructive `remove`
//!   without `--force`, `--yes`/`--force`/`--format` on the wrong verb).
//! - `4` plugin error (unknown/non-bundled id, invalid or corrupt managed
//!   manifest, pin mismatch, uncovered grant, blocked capability increase).
//!
//! # Bounds (fail closed before any filesystem mutation)
//!
//! - Raw tokens: 1..=[`MAX_PLUGIN_TOKEN_BYTES`] bytes, no NUL.
//! - `--format`: 1..=[`MAX_PLUGIN_FORMAT_BYTES`] bytes.
//! - Managed manifest: [`MAX_STATE_FILE_BYTES`] bytes,
//!   [`MAX_STATE_PLUGINS`] records, [`MAX_STATE_GRANTS`] grants per record.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bitty_plugin_host::bundled::{all_bundled_manifests, bundled_manifest_for};
use bitty_plugin_host::capability::{CapabilityId, effect_statement};
use bitty_plugin_host::manifest::{PluginId, PluginManifest};

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success (including idempotent no-ops).
pub const EXIT_OK: i32 = 0;
/// Aborted consent or filesystem failure after approval.
pub const EXIT_GENERIC: i32 = 1;
/// Usage error (`cli-contract-rfc.md` class `UsageError`).
pub const EXIT_USAGE: i32 = 2;
/// Plugin-level failure (unknown id, pin mismatch, blocked increase).
pub const EXIT_PLUGIN: i32 = 4;

/// Maximum bytes for one raw token (verb, id, flag value).
pub const MAX_PLUGIN_TOKEN_BYTES: usize = 256;
/// Maximum bytes for a `--format` value.
pub const MAX_PLUGIN_FORMAT_BYTES: usize = 16;
/// Managed-manifest file name beside `init.lua`.
pub const STATE_FILE_NAME: &str = "bitty-plugins.toml";
/// Suffix appended to the previous managed manifest on every rewrite.
pub const STATE_BACKUP_SUFFIX: &str = ".bak";
/// Managed-manifest format version (this module owns the codec).
pub const STATE_VERSION: u32 = 1;
/// Maximum managed-manifest file size.
pub const MAX_STATE_FILE_BYTES: usize = 64 * 1024;
/// Maximum records in one managed manifest.
pub const MAX_STATE_PLUGINS: usize = 256;
/// Maximum granted capabilities recorded per plugin.
pub const MAX_STATE_GRANTS: usize = 256;
/// Maximum managed-manifest lines.
pub const MAX_STATE_LINES: usize = 4096;
/// Hex digest length (`Sha256`, as produced by `PluginManifest::manifest_hash`).
pub const HEX_HASH_LEN: usize = 64;
/// Maximum bytes read from one consent answer line.
pub const MAX_CONSENT_LINE_BYTES: usize = 64;
/// Consent attempts before aborting (mirrors `bitty init`).
pub const MAX_CONSENT_ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Parsed request (headless, bounded, fail-closed)
// ---------------------------------------------------------------------------

/// Output shape for `list`/`info` (`--format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFormat {
    /// Human table (default).
    Table,
    /// Versioned JSON envelope on stdout.
    Json,
    /// Same envelope, one line (identical for this command).
    Jsonl,
}

impl PluginFormat {
    /// Parse `--format` (`None` means the table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(value) => {
                if value.len() > MAX_PLUGIN_FORMAT_BYTES || value.contains('\0') {
                    return Err(format!(
                        "bitty plugin: unknown --format {value:?} (want table|json|jsonl)"
                    ));
                }
                match value.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty plugin: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

/// One `bitty plugin` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginVerb {
    /// `list`: enumerate bundled + recorded plugins.
    List,
    /// `install <id>`: pin + grant a bundled plugin.
    Install,
    /// `remove <id> --force`: drop a record (backup kept).
    Remove,
    /// `enable <id>`: re-enable a recorded plugin.
    Enable,
    /// `disable <id>`: disable a recorded plugin.
    Disable,
    /// `info <id>`: explain one plugin's static manifest + state.
    Info,
}

impl PluginVerb {
    /// Parse a verb token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "list" => Some(Self::List),
            "install" => Some(Self::Install),
            "remove" => Some(Self::Remove),
            "enable" => Some(Self::Enable),
            "disable" => Some(Self::Disable),
            "info" => Some(Self::Info),
            _ => None,
        }
    }

    /// Canonical verb name for output/errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Info => "info",
        }
    }

    /// Whether the verb takes a plugin id operand.
    #[must_use]
    pub fn needs_id(self) -> bool {
        !matches!(self, Self::List)
    }
}

/// Validated request for one `bitty plugin` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRequest {
    /// Requested verb.
    pub verb: PluginVerb,
    /// Plugin id operand (validated as a token; grammar checked at resolve).
    pub id: Option<String>,
    /// Output shape for `list`/`info`.
    pub format: PluginFormat,
    /// `install --yes`: approve capability consent non-interactively.
    pub yes: bool,
    /// `remove --force`: required for the destructive removal.
    pub force: bool,
    /// `--no-color` (accepted for parity; tables are plain text).
    pub no_color: bool,
}

/// Why parsing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginParseError {
    /// `-h`/`--help` was requested.
    Help,
    /// Usage-level failure (message already user-facing).
    Usage(String),
}

impl PluginParseError {
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
    !token.is_empty() && token.len() <= MAX_PLUGIN_TOKEN_BYTES && !token.contains('\0')
}

/// Parse the tokens captured after the `plugin` word (bounded, fail-closed).
///
/// `pre_format` carries a pre-word global `--format` value (mirrors
/// `bitty dev`); an explicit `--format` after the verb wins.
pub fn parse_plugin_request(
    raw: &[String],
    pre_format: Option<&str>,
) -> Result<PluginRequest, PluginParseError> {
    let mut verb: Option<PluginVerb> = None;
    let mut id: Option<String> = None;
    let mut format: Option<String> = None;
    let mut yes = false;
    let mut force = false;
    let mut no_color = false;

    let mut index = 0usize;
    while index < raw.len() {
        let token = raw[index].as_str();
        match token {
            "-h" | "--help" => return Err(PluginParseError::Help),
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
            "--no-color" => {
                no_color = true;
                index += 1;
                continue;
            }
            "--" => {
                return Err(PluginParseError::Usage(
                    "bitty plugin: stray `--` separator (plugin takes no child argv)".to_string(),
                ));
            }
            _ => {}
        }
        if let Some(value) = token.strip_prefix("--format=") {
            format = Some(value.to_string());
            index += 1;
            continue;
        }
        if token == "--format" {
            // Pair form: consume the next token as the value; a missing
            // value is usage (fail closed before dispatch).
            let Some(value) = raw.get(index + 1) else {
                return Err(PluginParseError::Usage(
                    "bitty plugin: --format needs a value (table|json|jsonl)".to_string(),
                ));
            };
            format = Some(value.clone());
            index += 2;
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            return Err(PluginParseError::Usage(format!(
                "bitty plugin: unknown flag {token:?}"
            )));
        }
        if !bare_token_ok(token) {
            return Err(PluginParseError::Usage(format!(
                "bitty plugin: argument is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
            )));
        }
        if verb.is_none() {
            match PluginVerb::parse(token) {
                Some(parsed) => verb = Some(parsed),
                None => {
                    return Err(PluginParseError::Usage(format!(
                        "bitty plugin: unknown verb {token:?} \
                         (want list|install|remove|enable|disable|info)"
                    )));
                }
            }
            index += 1;
            continue;
        }
        if id.is_none() {
            id = Some(token.to_string());
            index += 1;
            continue;
        }
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: unexpected extra argument {token:?}"
        )));
    }

    let verb = verb.ok_or_else(|| {
        PluginParseError::Usage(
            "bitty plugin: missing verb (want list|install|remove|enable|disable|info)".to_string(),
        )
    })?;

    let format = match format {
        Some(value) => PluginFormat::parse(Some(&value)).map_err(PluginParseError::Usage)?,
        None => PluginFormat::parse(pre_format).map_err(PluginParseError::Usage)?,
    };

    if verb.needs_id() && id.is_none() {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: `{}` needs a plugin id (owner.name)",
            verb.name()
        )));
    }
    if !verb.needs_id() && id.is_some() {
        return Err(PluginParseError::Usage(
            "bitty plugin: `list` takes no plugin id".to_string(),
        ));
    }
    if yes && verb != PluginVerb::Install {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --yes only applies to `install` (got `{}`)",
            verb.name()
        )));
    }
    if force && verb != PluginVerb::Remove {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --force only applies to `remove` (got `{}`)",
            verb.name()
        )));
    }
    if (format != PluginFormat::Table || no_color)
        && !matches!(verb, PluginVerb::List | PluginVerb::Info)
    {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --format/--no-color only apply to `list` and `info` (got `{}`)",
            verb.name()
        )));
    }

    Ok(PluginRequest {
        verb,
        id,
        format,
        yes,
        force,
        no_color,
    })
}

/// Short usage block (`stderr` on usage failures).
#[must_use]
pub fn plugin_usage() -> String {
    "usage: bitty plugin list [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty plugin install <id> [--yes]\n\
     \x20      bitty plugin remove <id> --force\n\
     \x20      bitty plugin enable <id>\n\
     \x20      bitty plugin disable <id>\n\
     \x20      bitty plugin info <id> [--format table|json|jsonl] [--no-color]\n\
     \n\
     Managed manifest: $XDG_CONFIG_HOME/bitty/bitty-plugins.toml (fallback\n\
     ~/.config/bitty/bitty-plugins.toml), or beside an explicit --config path.\n\
     v1 installs bundled plugins only (`bitty-terminal.*`); no plugin code runs.\n\
     `bitty plugin --help` explains capabilities, consent, and exit codes."
        .to_string()
}

/// Long help (`bitty plugin --help`).
#[must_use]
pub fn plugin_help_text() -> String {
    "bitty plugin — CLI-first plugin management (local class, no VM)\n\
     \n\
     usage: bitty plugin <verb> [args] [flags]\n\
     \n\
     verbs:\n\
     \x20 list                        Show bundled + recorded plugins: state,\n\
     \x20                             pin status, granted/requested capabilities.\n\
     \x20 install <id> [--yes]        Resolve a bundled manifest, pin its hash,\n\
     \x20                             and grant requested capabilities after\n\
     \x20                             explicit consent. --yes approves without a\n\
     \x20                             prompt; without it an interactive [y/N]\n\
     \x20                             prompt lists every capability and effect\n\
     \x20                             (EOF/decline aborts, nothing is written).\n\
     \x20 remove <id> --force         Remove a record; the previous managed\n\
     \x20                             manifest is copied to <file>.bak first.\n\
     \x20 enable <id>                 Re-enable a recorded plugin. The manifest\n\
     \x20                             hash pin and grant coverage are re-checked\n\
     \x20                             and a mismatch fails closed (re-install).\n\
     \x20 disable <id>                Disable without dropping the grant record.\n\
     \x20 info <id>                   Static manifest + recorded state, with the\n\
     \x20                             plain-language effect per capability.\n\
     \n\
     flags:\n\
     \x20 --format table|json|jsonl   list/info output shape (default table).\n\
     \x20 --no-color                  Accepted for parity (tables are plain text).\n\
     \x20 --yes                       install only: approve capability consent.\n\
     \x20 --force                     remove only: confirm the destructive drop.\n\
     \n\
     authority:\n\
     \x20 Plugin code is never executed by any `bitty plugin` operation. A\n\
     \x20 capability is granted only when the manifest requests it and consent\n\
     \x20 is explicit; grants are bound to the exact manifest hash, so a later\n\
     \x20 manifest that adds a capability blocks until re-approved. The\n\
     \x20 managed manifest is strict machine state: unknown keys or malformed\n\
     \x20 values fail closed; edit it through this CLI, not by hand.\n\
     \n\
     exit codes:\n\
     \x20 0 success | 1 aborted/io | 2 usage | 4 plugin error\n\
     \n\
     examples:\n\
     \x20 bitty plugin list\n\
     \x20 bitty plugin list --format json\n\
     \x20 bitty plugin install bitty-terminal.tabs --yes\n\
     \x20 bitty plugin disable bitty-terminal.tabs\n\
     \x20 bitty plugin info bitty-terminal.tabs\n\
     \x20 bitty plugin remove bitty-terminal.tabs --force"
        .to_string()
}

// ---------------------------------------------------------------------------
// Managed manifest (strict bounded TOML subset, owned by this module)
// ---------------------------------------------------------------------------

/// One recorded plugin: source, pinned manifest hash, enabled flag, and the
/// granted capability set bound to that hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRecord {
    /// Provenance (`bundled` today; closed set).
    pub source: String,
    /// Hex-encoded `PluginManifest::manifest_hash()` this grant is bound to.
    pub manifest_hash: String,
    /// Whether the plugin is enabled in the recorded desired state.
    pub enabled: bool,
    /// Granted capabilities for this exact manifest hash.
    pub granted: BTreeSet<CapabilityId>,
}

/// Parsed managed manifest (`bitty-plugins.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginState {
    records: BTreeMap<String, PluginRecord>,
}

/// Managed-manifest parse/validation failure (owned message incl. line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError {
    message: String,
}

impl StateError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            message: format!("line {line}: {}", message.into()),
        }
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StateError {}

impl PluginState {
    /// Empty state (no records).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a state from records (test constructor).
    #[cfg(test)]
    #[must_use]
    pub fn from_records(records: BTreeMap<String, PluginRecord>) -> Self {
        Self { records }
    }

    /// Record for `id`, if any.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&PluginRecord> {
        self.records.get(id)
    }

    /// All recorded ids in deterministic (sorted) order.
    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.records.keys()
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut PluginRecord> {
        self.records.get_mut(id)
    }

    fn insert(&mut self, id: String, record: PluginRecord) {
        self.records.insert(id, record);
    }

    fn remove(&mut self, id: &str) -> Option<PluginRecord> {
        self.records.remove(id)
    }

    /// Parse a managed manifest, fail-closed on anything outside the strict
    /// bounded subset this module writes.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] for over-limit files, unknown/duplicate
    /// sections or keys, malformed values, missing required keys, an
    /// unsupported `state_version`, or invalid plugin/capability identifiers.
    pub fn parse(text: &str) -> Result<Self, StateError> {
        if text.len() > MAX_STATE_FILE_BYTES {
            return Err(StateError::new(
                1,
                format!("managed manifest exceeds {MAX_STATE_FILE_BYTES} bytes"),
            ));
        }
        let mut state = Self::new();
        let mut version_seen = false;
        let mut section_id: Option<String> = None;
        let mut section_line = 0usize;
        let mut source: Option<String> = None;
        let mut manifest_hash: Option<String> = None;
        let mut enabled: Option<bool> = None;
        let mut granted: Option<BTreeSet<CapabilityId>> = None;

        for (index, raw_line) in text.lines().enumerate() {
            let line = index + 1;
            if line > MAX_STATE_LINES {
                return Err(StateError::new(
                    line,
                    format!("managed manifest exceeds {MAX_STATE_LINES} lines"),
                ));
            }
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(header) = trimmed.strip_prefix('[') {
                let header = header.strip_suffix(']').ok_or_else(|| {
                    StateError::new(line, "section header is missing the closing `]`")
                })?;
                if section_id.is_some() {
                    flush_section(
                        &mut state,
                        section_id.take(),
                        section_line,
                        &mut source,
                        &mut manifest_hash,
                        &mut enabled,
                        &mut granted,
                    )?;
                }
                let id = parse_section_header(header, line)?;
                if state.records.contains_key(&id) {
                    return Err(StateError::new(
                        line,
                        format!("duplicate section for plugin '{id}'"),
                    ));
                }
                section_id = Some(id);
                section_line = line;
                continue;
            }
            if section_id.is_none() {
                // Only `state_version = 1` is allowed before the first section.
                let (key, value) = split_assignment(trimmed, line)?;
                if key != "state_version" {
                    return Err(StateError::new(
                        line,
                        format!("expected `state_version` before any section, found '{key}'"),
                    ));
                }
                if version_seen {
                    return Err(StateError::new(line, "duplicate `state_version`"));
                }
                if value != STATE_VERSION.to_string() {
                    return Err(StateError::new(
                        line,
                        format!("unsupported state_version '{value}' (want {STATE_VERSION})"),
                    ));
                }
                version_seen = true;
                continue;
            }
            let (key, value) = split_assignment(trimmed, line)?;
            match key {
                "source" => {
                    reject_duplicate(source.is_some(), "source", line)?;
                    let parsed = parse_quoted(value, line)?;
                    if parsed != "bundled" {
                        return Err(StateError::new(
                            line,
                            format!("unsupported source '{parsed}' (want bundled)"),
                        ));
                    }
                    source = Some(parsed);
                }
                "manifest_hash" => {
                    reject_duplicate(manifest_hash.is_some(), "manifest_hash", line)?;
                    let parsed = parse_quoted(value, line)?;
                    if !is_lower_hex(&parsed, HEX_HASH_LEN) {
                        return Err(StateError::new(
                            line,
                            format!(
                                "manifest_hash must be {HEX_HASH_LEN} lowercase hex characters"
                            ),
                        ));
                    }
                    manifest_hash = Some(parsed);
                }
                "enabled" => {
                    reject_duplicate(enabled.is_some(), "enabled", line)?;
                    enabled = Some(match value {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(StateError::new(
                                line,
                                format!("enabled must be true|false, found '{other}'"),
                            ));
                        }
                    });
                }
                "granted" => {
                    reject_duplicate(granted.is_some(), "granted", line)?;
                    granted = Some(parse_capability_array(value, line)?);
                }
                other => {
                    return Err(StateError::new(
                        line,
                        format!("unknown key '{other}' in plugin section"),
                    ));
                }
            }
        }
        if section_id.is_some() {
            flush_section(
                &mut state,
                section_id.take(),
                section_line,
                &mut source,
                &mut manifest_hash,
                &mut enabled,
                &mut granted,
            )?;
        }
        if !version_seen {
            return Err(StateError::new(1, "missing `state_version = 1`"));
        }
        if state.records.len() > MAX_STATE_PLUGINS {
            return Err(StateError::new(
                1,
                format!("managed manifest exceeds {MAX_STATE_PLUGINS} plugins"),
            ));
        }
        Ok(state)
    }

    /// Render the managed manifest deterministically (sorted records, fixed
    /// field order, sorted grants, trailing newline).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# bitty managed plugin manifest — machine-generated by `bitty plugin`.\n\
             # Do not edit by hand; use bitty plugin list|install|remove|enable|disable.\n",
        );
        let _ = writeln!(out, "state_version = {STATE_VERSION}");
        for (id, record) in &self.records {
            out.push('\n');
            let _ = writeln!(out, "[plugins.\"{id}\"]");
            let _ = writeln!(out, "source = \"{}\"", record.source);
            let _ = writeln!(out, "manifest_hash = \"{}\"", record.manifest_hash);
            let _ = writeln!(out, "enabled = {}", record.enabled);
            out.push_str("granted = [");
            for (index, capability) in record.granted.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "\"{}\"", capability.as_str());
            }
            out.push_str("]\n");
        }
        out
    }
}

fn flush_section(
    state: &mut PluginState,
    id: Option<String>,
    line: usize,
    source: &mut Option<String>,
    manifest_hash: &mut Option<String>,
    enabled: &mut Option<bool>,
    granted: &mut Option<BTreeSet<CapabilityId>>,
) -> Result<(), StateError> {
    let Some(id) = id else {
        return Ok(());
    };
    let source = source
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `source`")))?;
    let manifest_hash = manifest_hash.take().ok_or_else(|| {
        StateError::new(line, format!("plugin '{id}' is missing `manifest_hash`"))
    })?;
    let enabled = enabled
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `enabled`")))?;
    let granted = granted
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `granted`")))?;
    if granted.len() > MAX_STATE_GRANTS {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' exceeds {MAX_STATE_GRANTS} granted capabilities"),
        ));
    }
    state.insert(
        id,
        PluginRecord {
            source,
            manifest_hash,
            enabled,
            granted,
        },
    );
    Ok(())
}

fn parse_section_header(header: &str, line: usize) -> Result<String, StateError> {
    let header = header.trim();
    let inner = header
        .strip_prefix("plugins.")
        .ok_or_else(|| StateError::new(line, "expected section `[plugins.\"<id>\"]`"))?;
    let id = parse_quoted(inner, line)?;
    PluginId::new(&id)
        .map_err(|error| StateError::new(line, format!("invalid plugin id '{id}': {error}")))?;
    Ok(id)
}

fn split_assignment(line: &str, line_no: usize) -> Result<(&str, &str), StateError> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| StateError::new(line_no, "expected `key = value`"))?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(StateError::new(line_no, "expected `key = value`"));
    }
    Ok((key, value))
}

fn reject_duplicate(seen: bool, key: &str, line: usize) -> Result<(), StateError> {
    if seen {
        return Err(StateError::new(line, format!("duplicate key '{key}'")));
    }
    Ok(())
}

fn parse_quoted(value: &str, line: usize) -> Result<String, StateError> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| StateError::new(line, "expected a double-quoted value"))?;
    if inner.contains(['"', '\\']) || inner.chars().any(char::is_control) {
        return Err(StateError::new(
            line,
            "quoted value must not contain quotes, escapes, or control characters",
        ));
    }
    Ok(inner.to_string())
}

fn parse_capability_array(value: &str, line: usize) -> Result<BTreeSet<CapabilityId>, StateError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| StateError::new(line, "granted must be an array of quoted capabilities"))?;
    let inner = inner.trim();
    let mut out = BTreeSet::new();
    if inner.is_empty() {
        return Ok(out);
    }
    for element in inner.split(',') {
        let element = element.trim();
        let capability_text = parse_quoted(element, line)?;
        let capability = CapabilityId::parse(&capability_text).map_err(|error| {
            StateError::new(
                line,
                format!("invalid capability '{capability_text}': {error}"),
            )
        })?;
        if !out.insert(capability) {
            return Err(StateError::new(
                line,
                format!("duplicate capability '{capability_text}'"),
            ));
        }
        if out.len() > MAX_STATE_GRANTS {
            return Err(StateError::new(
                line,
                format!("granted exceeds {MAX_STATE_GRANTS} capabilities"),
            ));
        }
    }
    Ok(out)
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// Managed-manifest location and IO
// ---------------------------------------------------------------------------

/// One dispatch failure with its stable exit code and envelope class.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginFailure {
    exit: i32,
    class: &'static str,
    code: &'static str,
    message: String,
}

impl PluginFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_USAGE,
            class: "UsageError",
            code: "Usage",
            message: message.into(),
        }
    }

    fn plugin(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_PLUGIN,
            class: "PluginError",
            code,
            message: message.into(),
        }
    }

    fn generic(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_GENERIC,
            class: "Internal",
            code,
            message: message.into(),
        }
    }
}

/// Resolve the managed-manifest path.
///
/// - An explicit `--config`/`BITTY_CONFIG` path selects its parent directory.
/// - Otherwise `$XDG_CONFIG_HOME/bitty`, falling back to `~/.config/bitty`.
fn resolve_state_path(
    config_path: Option<&str>,
    bitty_config_env: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, PluginFailure> {
    if let Some(explicit) =
        bitty_config::file::resolve_config_explicit(config_path, bitty_config_env)
    {
        let candidate = Path::new(&explicit);
        // An explicit directory is the config root; a file path selects its
        // parent directory (mirrors `bitty init`'s `--config` semantics).
        let dir = if candidate.is_dir() {
            candidate
        } else {
            candidate
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| {
                    PluginFailure::usage(format!(
                        "bitty plugin: cannot derive a config directory from --config '{explicit}'"
                    ))
                })?
        };
        return Ok(dir.join(STATE_FILE_NAME));
    }
    bitty_config::file::config_dir_with_env(xdg_config_home, home)
        .map(|dir| dir.join(STATE_FILE_NAME))
        .ok_or_else(|| {
            PluginFailure::usage(
                "bitty plugin: no config root ($XDG_CONFIG_HOME or $HOME unset)".to_string(),
            )
        })
}

fn load_state(path: &Path) -> Result<PluginState, PluginFailure> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginState::new());
        }
        Err(error) => {
            return Err(PluginFailure::generic(
                "IoError",
                format!("bitty plugin: cannot read '{}': {error}", path.display()),
            ));
        }
    };
    if bytes.len() > MAX_STATE_FILE_BYTES {
        return Err(PluginFailure::plugin(
            "StateInvalid",
            format!(
                "bitty plugin: '{}' exceeds {MAX_STATE_FILE_BYTES} bytes",
                path.display()
            ),
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        PluginFailure::plugin(
            "StateInvalid",
            format!("bitty plugin: '{}' is not UTF-8: {error}", path.display()),
        )
    })?;
    PluginState::parse(text).map_err(|error| {
        PluginFailure::plugin(
            "StateInvalid",
            format!(
                "bitty plugin: invalid managed manifest '{}': {error}",
                path.display()
            ),
        )
    })
}

/// Write the state, keeping the previous bytes at `<file>.bak` first.
fn save_state(path: &Path, state: &PluginState) -> Result<Option<PathBuf>, PluginFailure> {
    let rendered = state.render();
    // Never write bytes this module cannot read back (defense in depth).
    PluginState::parse(&rendered).map_err(|error| {
        PluginFailure::generic(
            "Internal",
            format!("bitty plugin: internal error: rendered manifest invalid: {error}"),
        )
    })?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PluginFailure::generic(
                    "IoError",
                    format!(
                        "bitty plugin: cannot create '{}': {error}",
                        parent.display()
                    ),
                )
            })?;
        }
    }
    let mut backup = None;
    if path.exists() {
        let mut backup_name = path.as_os_str().to_os_string();
        backup_name.push(STATE_BACKUP_SUFFIX);
        let backup_path = PathBuf::from(backup_name);
        let previous = std::fs::read(path).map_err(|error| {
            PluginFailure::generic(
                "IoError",
                format!("bitty plugin: cannot read '{}': {error}", path.display()),
            )
        })?;
        std::fs::write(&backup_path, previous).map_err(|error| {
            PluginFailure::generic(
                "IoError",
                format!(
                    "bitty plugin: cannot write backup '{}': {error}",
                    backup_path.display()
                ),
            )
        })?;
        backup = Some(backup_path);
    }
    std::fs::write(path, rendered).map_err(|error| {
        PluginFailure::generic(
            "IoError",
            format!("bitty plugin: cannot write '{}': {error}", path.display()),
        )
    })?;
    Ok(backup)
}

// ---------------------------------------------------------------------------
// Rows (static manifest + recorded state) and rendering
// ---------------------------------------------------------------------------

/// One plugin row for `list`/`info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// Plugin id.
    pub id: String,
    /// Display name (manifest, else id).
    pub name: String,
    /// Manifest version (`-` for records without a bundled manifest).
    pub version: String,
    /// One-line description.
    pub description: String,
    /// Whether the id is in the bundled catalog.
    pub bundled: bool,
    /// `enabled`/`disabled` when recorded, else `available`.
    pub state: &'static str,
    /// Recorded enabled flag.
    pub enabled: bool,
    /// Hash pin status: `Some(true)` verified, `Some(false)` stale, `None`
    /// when there is no manifest to verify against.
    pub pin_ok: Option<bool>,
    /// Capabilities the manifest requests (recorded set when no manifest).
    pub requested: BTreeSet<CapabilityId>,
    /// Capabilities recorded as granted.
    pub granted: BTreeSet<CapabilityId>,
    /// Qualified commands from the static manifest.
    pub commands: Vec<String>,
    /// Pinned manifest hash when recorded.
    pub manifest_hash: Option<String>,
}

impl PluginRow {
    /// Capability rows for `info`/envelopes: requested, granted, effect, risk.
    #[must_use]
    pub fn capability_rows(&self) -> Vec<CapabilityRow> {
        let mut ids: BTreeSet<CapabilityId> = self.requested.clone();
        ids.extend(self.granted.iter().cloned());
        ids.into_iter()
            .map(|capability| CapabilityRow {
                effect: effect_statement(&capability).to_string(),
                high_risk: capability.is_high_risk(),
                requested: self.requested.contains(&capability),
                granted: self.granted.contains(&capability),
                id: capability.as_str().to_string(),
            })
            .collect()
    }
}

/// One capability row for `info` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow {
    /// Capability identifier.
    pub id: String,
    /// Plain-language effect statement.
    pub effect: String,
    /// Whether the identifier is high risk.
    pub high_risk: bool,
    /// Whether the manifest requests it.
    pub requested: bool,
    /// Whether it is recorded as granted.
    pub granted: bool,
}

fn rows_from_state(state: &PluginState) -> Result<Vec<PluginRow>, PluginFailure> {
    let mut rows: Vec<PluginRow> = Vec::new();
    let mut manifests = all_bundled_manifests();
    manifests.sort_by_key(|manifest| manifest.id().as_str().to_string());
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for manifest in &manifests {
        let id = manifest.id().as_str().to_string();
        seen.insert(id.clone());
        let requested = manifest.capabilities.all_ids().map_err(|error| {
            PluginFailure::plugin(
                "ManifestInvalid",
                format!("bitty plugin: bundled manifest '{id}' is invalid: {error}"),
            )
        })?;
        let record = state.get(&id);
        let (state_label, enabled, pin_ok, granted, manifest_hash) = match record {
            Some(record) => (
                if record.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                record.enabled,
                Some(record.manifest_hash == manifest.manifest_hash()),
                record.granted.clone(),
                Some(record.manifest_hash.clone()),
            ),
            None => ("available", false, None, BTreeSet::new(), None),
        };
        rows.push(PluginRow {
            id,
            name: manifest.identity.name.clone(),
            version: manifest.identity.version.clone(),
            description: manifest.identity.description.clone(),
            bundled: true,
            state: state_label,
            enabled,
            pin_ok,
            requested,
            granted,
            commands: manifest
                .lazy
                .commands
                .iter()
                .map(|command| command.as_str().to_string())
                .collect(),
            manifest_hash,
        });
    }

    for id in state.ids() {
        if seen.contains(id) {
            continue;
        }
        let record = state.get(id).expect("iterated id exists");
        rows.push(PluginRow {
            id: id.clone(),
            name: id.clone(),
            version: "-".to_string(),
            description: "recorded plugin without a bundled manifest".to_string(),
            bundled: false,
            state: if record.enabled {
                "enabled"
            } else {
                "disabled"
            },
            enabled: record.enabled,
            pin_ok: None,
            requested: record.granted.clone(),
            granted: record.granted.clone(),
            commands: Vec::new(),
            manifest_hash: Some(record.manifest_hash.clone()),
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

fn row_for_id(state: &PluginState, id: &str) -> Result<PluginRow, PluginFailure> {
    rows_from_state(state)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| {
            PluginFailure::plugin(
                "PluginNotFound",
                format!("bitty plugin: unknown plugin '{id}' (see `bitty plugin list`)"),
            )
        })
}

fn pin_label(pin_ok: Option<bool>) -> &'static str {
    match pin_ok {
        Some(true) => "ok",
        Some(false) => "stale",
        None => "-",
    }
}

/// Whether ANSI emphasis is allowed (`--no-color`, `NO_COLOR`, `TERM=dumb`).
fn color_enabled(no_color: bool) -> bool {
    if no_color {
        return false;
    }
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    !matches!(std::env::var("TERM"), Ok(term) if term.trim().eq_ignore_ascii_case("dumb"))
}

fn bold(text: &str, color: bool) -> String {
    if color {
        format!("\u{1b}[1m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

fn format_list_table(rows: &[PluginRow], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let header = format!(
        "{:<32} {:<10} {:<6} {:>8}  {}",
        "ID", "STATE", "PIN", "GRANTED", "VERSION"
    );
    let _ = writeln!(out, "{}", bold(&header, color));
    for row in rows {
        let granted = format!("{}/{}", row.granted.len(), row.requested.len());
        let _ = writeln!(
            out,
            "{:<32} {:<10} {:<6} {:>8}  {}",
            row.id,
            row.state,
            pin_label(row.pin_ok),
            granted,
            row.version
        );
    }
    if rows.is_empty() {
        out.push_str("(no plugins)\n");
    }
    out
}

fn format_info_table(row: &PluginRow, no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let _ = writeln!(out, "{}", bold(&format!("plugin {}", row.id), color));
    let _ = writeln!(out, "  name:         {}", row.name);
    let _ = writeln!(out, "  version:      {}", row.version);
    let _ = writeln!(
        out,
        "  source:       {}",
        if row.bundled { "bundled" } else { "recorded" }
    );
    let _ = writeln!(out, "  state:        {}", row.state);
    let _ = writeln!(
        out,
        "  manifest:     {}",
        row.manifest_hash.as_deref().unwrap_or("-")
    );
    let _ = writeln!(out, "  pin:          {}", pin_label(row.pin_ok));
    let _ = writeln!(out, "  description:  {}", row.description);
    let _ = writeln!(
        out,
        "  commands:     {}",
        if row.commands.is_empty() {
            "-".to_string()
        } else {
            row.commands.join(", ")
        }
    );
    let capabilities = row.capability_rows();
    let _ = writeln!(out, "  capabilities ({}):", capabilities.len());
    for capability in &capabilities {
        let marker = if capability.granted {
            "granted"
        } else if capability.requested {
            "requested, not granted"
        } else {
            "not requested"
        };
        let risk = if capability.high_risk {
            " (high risk)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "    [{marker}] {}{risk} — {}",
            capability.id, capability.effect
        );
    }
    if capabilities.is_empty() {
        out.push_str("    (none requested)\n");
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

fn write_capability_array(out: &mut String, capabilities: &[CapabilityRow]) {
    out.push('[');
    for (index, capability) in capabilities.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"id\":\"{}\",\"effect\":\"{}\",\"high_risk\":{},\"requested\":{},\"granted\":{}}}",
            json_escape(&capability.id),
            json_escape(&capability.effect),
            capability.high_risk,
            capability.requested,
            capability.granted
        );
    }
    out.push(']');
}

fn write_plugin_object(out: &mut String, row: &PluginRow, include_manifest: bool) {
    let _ = write!(
        out,
        "{{\"id\":\"{}\",\"name\":\"{}\",\"version\":\"{}\",\"description\":\"{}\",\"bundled\":{},\"source\":\"{}\",\"state\":\"{}\",\"enabled\":{},\"pin_ok\":{}",
        json_escape(&row.id),
        json_escape(&row.name),
        json_escape(&row.version),
        json_escape(&row.description),
        row.bundled,
        if row.bundled { "bundled" } else { "recorded" },
        row.state,
        row.enabled,
        match row.pin_ok {
            Some(value) => value.to_string(),
            None => "null".to_string(),
        }
    );
    if include_manifest {
        let _ = write!(
            out,
            ",\"manifest_hash\":{}",
            match &row.manifest_hash {
                Some(hash) => format!("\"{}\"", json_escape(hash)),
                None => "null".to_string(),
            }
        );
    }
    out.push_str(",\"capabilities\":");
    write_capability_array(out, &row.capability_rows());
    out.push_str(",\"commands\":[");
    for (index, command) in row.commands.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(command));
    }
    out.push_str("]}");
}

/// Success envelope for `list`.
#[must_use]
pub fn format_list_envelope(rows: &[PluginRow]) -> String {
    let mut out =
        String::from("{\"v\":1,\"command\":\"plugin\",\"ok\":true,\"result\":{\"verb\":\"list\",");
    let _ = write!(out, "\"count\":{},", rows.len());
    out.push_str("\"plugins\":[");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_plugin_object(&mut out, row, false);
    }
    out.push_str("]}}");
    out
}

/// Success envelope for `info`.
#[must_use]
pub fn format_info_envelope(row: &PluginRow) -> String {
    let mut out = String::from(
        "{\"v\":1,\"command\":\"plugin\",\"ok\":true,\"result\":{\"verb\":\"info\",\"plugin\":",
    );
    write_plugin_object(&mut out, row, true);
    out.push_str("}}");
    out
}

/// Failure envelope (`ok:false`) for json/jsonl read-only output.
#[must_use]
pub fn format_error_envelope(verb: &str, class: &str, code: &str, message: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"plugin\",\"ok\":false,\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}},\"result\":{{\"verb\":\"{}\"}}}}",
        json_escape(class),
        json_escape(code),
        json_escape(message),
        json_escape(verb)
    )
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Injected environment for one `bitty plugin` dispatch (hermetic in tests).
#[derive(Debug, Clone, Default)]
pub struct PluginContext<'a> {
    /// Explicit `--config PATH` value.
    pub config_path: Option<&'a str>,
    /// `BITTY_CONFIG` environment value.
    pub bitty_config_env: Option<&'a str>,
    /// `XDG_CONFIG_HOME` environment value.
    pub xdg_config_home: Option<&'a str>,
    /// `HOME` environment value.
    pub home: Option<&'a str>,
    /// Pre-word global `--format` fallback (mirrors `bitty dev`).
    pub pre_format: Option<&'a str>,
    /// Pre-word `--no-color`.
    pub pre_no_color: bool,
}

struct OpOutput {
    summary: String,
    changed: bool,
}

/// Run one `bitty plugin` invocation; returns the process exit code.
pub fn run_plugin_subcommand(
    raw: &[String],
    context: &PluginContext<'_>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let request = match parse_plugin_request(raw, context.pre_format) {
        Ok(request) => request,
        Err(PluginParseError::Help) => {
            let _ = output.write_all(plugin_help_text().as_bytes());
            let _ = output.write_all(b"\n");
            return EXIT_OK;
        }
        Err(error) => {
            eprintln!("{}\n{}", error.message(), plugin_usage());
            return EXIT_USAGE;
        }
    };

    // Post-word `--no-color` wins; a pre-word global flag is honored too.
    let no_color = request.no_color || context.pre_no_color;
    let path = match resolve_state_path(
        context.config_path,
        context.bitty_config_env,
        context.xdg_config_home,
        context.home,
    ) {
        Ok(path) => path,
        Err(failure) => {
            eprintln!("{}", failure.message);
            return failure.exit;
        }
    };
    let mut state = match load_state(&path) {
        Ok(state) => state,
        Err(failure) => {
            eprintln!("{}", failure.message);
            return failure.exit;
        }
    };

    match request.verb {
        PluginVerb::List => match rows_from_state(&state) {
            Ok(rows) => {
                match request.format {
                    PluginFormat::Table => {
                        let _ = output.write_all(format_list_table(&rows, no_color).as_bytes());
                    }
                    PluginFormat::Json | PluginFormat::Jsonl => {
                        let _ = writeln!(output, "{}", format_list_envelope(&rows));
                    }
                }
                EXIT_OK
            }
            Err(failure) => fail_read_only(&request, &failure, output),
        },
        PluginVerb::Info => {
            let id = request.id.as_deref().expect("info requires an id");
            match resolve_plugin_id(id) {
                Ok(_) => {}
                Err(failure) => return fail_read_only(&request, &failure, output),
            }
            match row_for_id(&state, id) {
                Ok(row) => {
                    match request.format {
                        PluginFormat::Table => {
                            let _ = output.write_all(format_info_table(&row, no_color).as_bytes());
                        }
                        PluginFormat::Json | PluginFormat::Jsonl => {
                            let _ = writeln!(output, "{}", format_info_envelope(&row));
                        }
                    }
                    EXIT_OK
                }
                Err(failure) => fail_read_only(&request, &failure, output),
            }
        }
        PluginVerb::Install => {
            let id = request.id.as_deref().expect("install requires an id");
            match op_install(&mut state, id, request.yes, input, output) {
                Ok(result) => finish_mutation(&path, state, result, output),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Enable => {
            let id = request.id.as_deref().expect("enable requires an id");
            match op_enable(&mut state, id) {
                Ok(result) => finish_mutation(&path, state, result, output),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Disable => {
            let id = request.id.as_deref().expect("disable requires an id");
            match op_disable(&mut state, id) {
                Ok(result) => finish_mutation(&path, state, result, output),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Remove => {
            let id = request.id.as_deref().expect("remove requires an id");
            match op_remove(&mut state, id, request.force) {
                Ok(result) => finish_mutation(&path, state, result, output),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
    }
}

fn fail_read_only(
    request: &PluginRequest,
    failure: &PluginFailure,
    output: &mut dyn std::io::Write,
) -> i32 {
    if request.format != PluginFormat::Table {
        let _ = writeln!(
            output,
            "{}",
            format_error_envelope(
                request.verb.name(),
                failure.class,
                failure.code,
                &failure.message
            )
        );
    }
    eprintln!("{}", failure.message);
    failure.exit
}

fn finish_mutation(
    path: &Path,
    state: PluginState,
    result: OpOutput,
    output: &mut dyn std::io::Write,
) -> i32 {
    if result.changed {
        match save_state(path, &state) {
            Ok(backup) => {
                if let Some(backup) = backup {
                    let _ = writeln!(
                        output,
                        "bitty plugin: backed up previous manifest to '{}'",
                        backup.display()
                    );
                }
            }
            Err(failure) => {
                eprintln!("{}", failure.message);
                return failure.exit;
            }
        }
    }
    let _ = writeln!(output, "{}", result.summary);
    EXIT_OK
}

fn resolve_plugin_id(raw: &str) -> Result<PluginId, PluginFailure> {
    if raw.len() > MAX_PLUGIN_TOKEN_BYTES || raw.contains('\0') {
        return Err(PluginFailure::usage(
            "bitty plugin: plugin id is empty, contains NUL, or is too long".to_string(),
        ));
    }
    PluginId::new(raw).map_err(|error| {
        PluginFailure::usage(format!(
            "bitty plugin: malformed plugin id '{raw}': {error}"
        ))
    })
}

fn bundled_manifest(id: &str) -> Result<PluginManifest, PluginFailure> {
    let manifest = bundled_manifest_for(id).ok_or_else(|| {
        PluginFailure::plugin(
            "UnknownPlugin",
            format!(
                "bitty plugin: '{id}' is not a bundled plugin; v1 installs bundled ids \
                 only (`bitty-terminal.*`) — registry/Git/local sources land with the \
                 package manager (see `bitty plugin list`)"
            ),
        )
    })?;
    manifest.validate().map_err(|error| {
        PluginFailure::plugin(
            "ManifestInvalid",
            format!("bitty plugin: bundled manifest '{id}' is invalid: {error}"),
        )
    })?;
    Ok(manifest)
}

fn requested_capabilities(
    manifest: &PluginManifest,
) -> Result<BTreeSet<CapabilityId>, PluginFailure> {
    manifest.capabilities.all_ids().map_err(|error| {
        PluginFailure::plugin(
            "ManifestInvalid",
            format!(
                "bitty plugin: manifest '{}' expands to an invalid capability: {error}",
                manifest.id()
            ),
        )
    })
}

fn not_installed(id: &str) -> PluginFailure {
    PluginFailure::plugin(
        "PluginNotFound",
        format!("bitty plugin: '{id}' is not installed (see `bitty plugin list`)"),
    )
}

fn op_install(
    state: &mut PluginState,
    id: &str,
    yes: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let requested = requested_capabilities(&manifest)?;
    let existing = state.get(id).cloned();

    // P0-AC-030 pattern: only capabilities absent from the recorded grant
    // need consent; unchanged/narrowed sets carry forward silently.
    let needed: BTreeSet<CapabilityId> = match &existing {
        Some(record) => requested.difference(&record.granted).cloned().collect(),
        None => requested.clone(),
    };

    let approved = if needed.is_empty() {
        true
    } else if yes {
        let _ = writeln!(
            output,
            "bitty plugin: --yes approved {} capabilit{} for '{id}' (manifest {})",
            needed.len(),
            if needed.len() == 1 { "y" } else { "ies" },
            short_hash(&manifest_hash)
        );
        true
    } else {
        ask_consent(input, output, id, &manifest, &needed, existing.is_some())?
    };
    if !approved {
        return Err(PluginFailure::generic(
            "ConsentDeclined",
            format!("bitty plugin: capability grant for '{id}' was not approved — nothing changed"),
        ));
    }

    let changed = match &existing {
        Some(record) => {
            record.manifest_hash != manifest_hash || !record.enabled || record.granted != requested
        }
        None => true,
    };
    if changed {
        state.insert(
            id.to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: manifest_hash.clone(),
                enabled: true,
                granted: requested.clone(),
            },
        );
    }
    let action = match &existing {
        None => "installed",
        Some(_) if !changed => "already installed",
        Some(_) => "updated",
    };
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: {action} '{id}' (version {}, manifest {}, {} capabilit{} granted, enabled)",
            manifest.identity.version,
            short_hash(&manifest_hash),
            requested.len(),
            if requested.len() == 1 { "y" } else { "ies" }
        ),
        changed,
    })
}

fn op_enable(state: &mut PluginState, id: &str) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let record = state.get(id).expect("checked");
    if record.manifest_hash != manifest_hash {
        return Err(PluginFailure::plugin(
            "PinMismatch",
            format!(
                "bitty plugin: '{id}' manifest changed since consent (pin {}, current {}); \
                 review with `bitty plugin info {id}` and re-run `bitty plugin install {id}`",
                short_hash(&record.manifest_hash),
                short_hash(&manifest_hash)
            ),
        ));
    }
    let requested = requested_capabilities(&manifest)?;
    if !requested.is_subset(&record.granted) {
        return Err(PluginFailure::plugin(
            "CapabilityBlocked",
            format!(
                "bitty plugin: '{id}' requests capabilities that are not granted; \
                 re-run `bitty plugin install {id}` to review and approve"
            ),
        ));
    }
    if record.enabled {
        return Ok(OpOutput {
            summary: format!("bitty plugin: '{id}' already enabled"),
            changed: false,
        });
    }
    let granted_len = record.granted.len();
    state.get_mut(id).expect("checked").enabled = true;
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: enabled '{id}' (manifest {}, {granted_len} capabilit{})",
            short_hash(&manifest_hash),
            if granted_len == 1 {
                "y granted"
            } else {
                "ies granted"
            }
        ),
        changed: true,
    })
}

fn op_disable(state: &mut PluginState, id: &str) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let record = state.get(id).ok_or_else(|| not_installed(id))?;
    let manifest_hash = record.manifest_hash.clone();
    let granted = record.granted.len();
    if !record.enabled {
        return Ok(OpOutput {
            summary: format!("bitty plugin: '{id}' already disabled"),
            changed: false,
        });
    }
    state.get_mut(id).expect("checked").enabled = false;
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: disabled '{id}' (manifest {}, {} grant{} kept)",
            short_hash(&manifest_hash),
            granted,
            if granted == 1 { "" } else { "s" }
        ),
        changed: true,
    })
}

fn op_remove(state: &mut PluginState, id: &str, force: bool) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    if !force {
        return Err(PluginFailure::usage(format!(
            "bitty plugin: `remove {id}` is destructive; re-run with --force \
             (the previous manifest is backed up to {STATE_FILE_NAME}{STATE_BACKUP_SUFFIX})"
        )));
    }
    let record = state.remove(id).ok_or_else(|| not_installed(id))?;
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: removed '{id}' (manifest {}, {} grant{} dropped)",
            short_hash(&record.manifest_hash),
            record.granted.len(),
            if record.granted.len() == 1 { "" } else { "s" }
        ),
        changed: true,
    })
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// Ask the interactive capability-consent question (fails closed).
///
/// Returns `Ok(true)` when the user approves, `Ok(false)` on an explicit
/// decline, and `Err` when input ends before an answer (aborted).
fn ask_consent(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    id: &str,
    manifest: &PluginManifest,
    needed: &BTreeSet<CapabilityId>,
    update: bool,
) -> Result<bool, PluginFailure> {
    let noun = if needed.len() == 1 {
        "capability"
    } else {
        "capabilities"
    };
    let _ = writeln!(
        output,
        "bitty plugin: '{id}' {} {} new {noun} (manifest {}):",
        if update { "requests" } else { "requires" },
        needed.len(),
        short_hash(&manifest.manifest_hash())
    );
    for capability in needed {
        let risk = if capability.is_high_risk() {
            " (high risk)"
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "  - {}{risk} — {}",
            capability.as_str(),
            effect_statement(capability)
        );
    }
    for attempt in 1..=MAX_CONSENT_ATTEMPTS {
        let _ = write!(output, "Grant these capabilities? [y/N]: ");
        let _ = output.flush();
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => {
                return Err(PluginFailure::generic(
                    "ConsentAborted",
                    "bitty plugin: aborted (end of input) — nothing changed".to_string(),
                ));
            }
            Ok(_) => {
                if line.len() > MAX_CONSENT_LINE_BYTES {
                    line.truncate(MAX_CONSENT_LINE_BYTES);
                }
                match line.trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" => return Ok(true),
                    "" | "n" | "no" => return Ok(false),
                    _ => {
                        let _ = writeln!(
                            output,
                            "  (answer y or n — try again [{attempt}/{MAX_CONSENT_ATTEMPTS}])"
                        );
                    }
                }
            }
        }
    }
    Err(PluginFailure::generic(
        "ConsentAborted",
        format!(
            "bitty plugin: aborted (too many invalid answers, limit {MAX_CONSENT_ATTEMPTS}) — nothing changed"
        ),
    ))
}

// ---------------------------------------------------------------------------
// Tests (TDD surface: parsing, codec, dispatch; no VM, temp dirs only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_ok(raw: &[&str]) -> PluginRequest {
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        parse_plugin_request(&owned, None).expect("request parses")
    }

    fn parse_error(raw: &[&str]) -> String {
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        match parse_plugin_request(&owned, None) {
            Err(PluginParseError::Usage(message)) => message,
            other => panic!("expected usage error, got {other:?}"),
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("bitty-ctx0150-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn run(raw: &[&str], config_path: Option<&str>, input: &str) -> (i32, String, PathBuf) {
        let dir = scratch_dir("dispatch");
        let target = dir.join("init.lua");
        let owned_config = config_path
            .map(ToString::to_string)
            .unwrap_or_else(|| target.display().to_string());
        let context = PluginContext {
            config_path: Some(owned_config.as_str()),
            ..PluginContext::default()
        };
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        let mut input = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let code = run_plugin_subcommand(&owned, &context, &mut input, &mut output);
        (code, String::from_utf8(output).expect("utf-8"), dir)
    }

    fn state_path(dir: &Path) -> PathBuf {
        dir.join(STATE_FILE_NAME)
    }

    // ── request parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_list_defaults() {
        let request = parse_ok(&["list"]);
        assert_eq!(request.verb, PluginVerb::List);
        assert_eq!(request.format, PluginFormat::Table);
        assert_eq!(request.id, None);
        assert!(!request.yes && !request.force);
    }

    #[test]
    fn parse_install_id_yes_and_format_pairs() {
        let request = parse_ok(&["install", "bitty-terminal.tabs", "--yes"]);
        assert_eq!(request.verb, PluginVerb::Install);
        assert_eq!(request.id.as_deref(), Some("bitty-terminal.tabs"));
        assert!(request.yes);

        let request = parse_ok(&["list", "--format", "json"]);
        assert_eq!(request.format, PluginFormat::Json);
        let request = parse_ok(&["info", "bitty-terminal.tabs", "--format=jsonl"]);
        assert_eq!(request.format, PluginFormat::Jsonl);
        let request = parse_ok(&["remove", "bitty-terminal.tabs", "--force"]);
        assert!(request.force);
    }

    #[test]
    fn parse_uses_pre_word_format_fallback() {
        let owned = vec!["list".to_string()];
        let request = parse_plugin_request(&owned, Some("json")).expect("parses");
        assert_eq!(request.format, PluginFormat::Json);
        // Explicit post-word value wins.
        let owned = vec![
            "list".to_string(),
            "--format".to_string(),
            "table".to_string(),
        ];
        let request = parse_plugin_request(&owned, Some("json")).expect("parses");
        assert_eq!(request.format, PluginFormat::Table);
    }

    #[test]
    fn parse_rejects_unknown_verb_flags_and_extras() {
        assert!(parse_error(&["frobnicate"]).contains("unknown verb"));
        assert!(parse_error(&["list", "--bogus"]).contains("unknown flag"));
        assert!(parse_error(&["list", "extra"]).contains("takes no plugin id"));
        assert!(parse_error(&["install"]).contains("needs a plugin id"));
        assert!(parse_error(&["install", "a", "b"]).contains("extra argument"));
        assert!(parse_error(&["info", "a", "--format", "yaml"]).contains("unknown --format"));
        assert!(parse_error(&["list", "--format"]).contains("--format needs a value"));
        assert!(parse_error(&["list", "--"]).contains("stray `--`"));
    }

    #[test]
    fn parse_rejects_flags_on_the_wrong_verb() {
        assert!(parse_error(&["list", "--yes"]).contains("--yes only applies"));
        assert!(parse_error(&["enable", "a", "--force"]).contains("--force only applies"));
        assert!(
            parse_error(&["install", "a", "--format", "json"]).contains("only apply to `list`")
        );
        assert!(parse_error(&["remove", "a", "--no-color"]).contains("only apply to `list`"));
    }

    #[test]
    fn parse_bounds_tokens_and_help() {
        let long = "x".repeat(MAX_PLUGIN_TOKEN_BYTES + 1);
        assert!(parse_error(&["install", &long]).contains("exceeds"));
        let owned = vec!["--help".to_string()];
        assert_eq!(
            parse_plugin_request(&owned, None),
            Err(PluginParseError::Help)
        );
    }

    // ── managed-manifest codec ────────────────────────────────────────────

    fn sample_state() -> PluginState {
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").expect("cap"));
        let mut records = BTreeMap::new();
        records.insert(
            "bitty-terminal.tabs".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "ab".repeat(32),
                enabled: true,
                granted,
            },
        );
        PluginState::from_records(records)
    }

    #[test]
    fn state_round_trips_deterministically() {
        let rendered = sample_state().render();
        let parsed = PluginState::parse(&rendered).expect("parses");
        assert_eq!(parsed, sample_state());
        assert_eq!(parsed.render(), rendered);
        assert!(rendered.contains("state_version = 1"));
        assert!(rendered.contains("[plugins.\"bitty-terminal.tabs\"]"));
        assert!(rendered.contains("granted = [\"terminal.semantic-read\"]"));
        // Empty state renders and parses.
        let empty = PluginState::new().render();
        assert_eq!(
            PluginState::parse(&empty).expect("empty parses"),
            PluginState::new()
        );
    }

    #[test]
    fn state_parse_fails_closed_on_tampering() {
        let good = sample_state().render();
        for (bad, needle) in [
            (
                good.replace("state_version = 1", "state_version = 2"),
                "state_version",
            ),
            (
                good.replace("source = \"bundled\"", "source = \"git\""),
                "source",
            ),
            (
                good.replace(
                    "abababababababababababababababababababababababababababababababab",
                    "not-a-hash",
                ),
                "manifest_hash",
            ),
            (good.replace("enabled = true", "enabled = yes"), "enabled"),
            (
                good.replace(
                    "granted = [\"terminal.semantic-read\"]",
                    "granted = [\"nope\"]",
                ),
                "capability",
            ),
            (good.replace("manifest_hash", "bogus_key"), "unknown key"),
            (format!("{good}\n[plugins.\"x.y\"]\n"), "missing `source`"),
        ] {
            assert!(PluginState::parse(&bad).is_err(), "must reject: {needle}");
        }
        // Unknown top-level key before any section.
        assert!(PluginState::parse("nope = 1").is_err());
        // Missing version.
        assert!(PluginState::parse("# only a comment\n").is_err());
        // Duplicate plugin section.
        let dup = format!("{good}{good}");
        assert!(PluginState::parse(&dup).is_err());
        // Oversized file.
        let oversized = "a".repeat(MAX_STATE_FILE_BYTES + 1);
        assert!(PluginState::parse(&oversized).is_err());
        // Quoted value with an escape is not part of the subset.
        assert!(
            PluginState::parse("state_version = 1\n[plugins.\"a.b\"]\nsource = \"bun\\\"dled\"\n")
                .is_err()
        );
    }

    #[test]
    fn state_path_resolution_prefers_explicit_config_then_xdg() {
        let explicit = resolve_state_path(Some("/tmp/bitty-x/init.lua"), None, None, None)
            .expect("explicit path");
        assert_eq!(explicit, Path::new("/tmp/bitty-x").join(STATE_FILE_NAME));

        let env_config =
            resolve_state_path(None, Some("/env/bitty/init.lua"), None, None).expect("env path");
        assert_eq!(env_config, Path::new("/env/bitty").join(STATE_FILE_NAME));

        let xdg = resolve_state_path(None, None, Some("/xdg"), Some("/home/u")).expect("xdg");
        assert_eq!(xdg, Path::new("/xdg/bitty").join(STATE_FILE_NAME));

        let home = resolve_state_path(None, None, None, Some("/home/u")).expect("home");
        assert_eq!(
            home,
            Path::new("/home/u/.config/bitty").join(STATE_FILE_NAME)
        );

        assert!(resolve_state_path(None, None, None, None).is_err());
        assert!(resolve_state_path(Some("init.lua"), None, None, None).is_err());
    }

    // ── dispatch lifecycle ────────────────────────────────────────────────

    #[test]
    fn lifecycle_install_list_disable_enable_remove() {
        let dir = scratch_dir("lifecycle");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        // install --yes grants the manifest capabilities and pins the hash.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("installed"), "{out}");
        let file = state_path(&dir);
        let text = std::fs::read_to_string(&file).expect("state written");
        assert!(text.contains("[plugins.\"bitty-terminal.shell-integration\"]"));
        assert!(text.contains("granted = [\"terminal.semantic-read\"]"));
        assert!(text.contains("enabled = true"));

        // list shows enabled + pinned.
        let (code, out, _) = run(&["list"], Some(&config), "");
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("bitty-terminal.shell-integration"));
        assert!(out.contains("enabled"));
        assert!(out.contains("ok"));
        assert!(out.contains("1/1"));

        // disable is idempotent.
        let (code, out, _) = run(
            &["disable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("disabled"), "{out}");
        let (code, out, _) = run(
            &["disable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("already disabled"), "{out}");

        // enable restores it, keeping the grant.
        let (code, out, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("enabled"), "{out}");

        // remove without --force fails closed and keeps the record.
        let (code, out, _) = run(
            &["remove", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_USAGE);
        assert!(out.is_empty());
        assert!(
            std::fs::read_to_string(&file)
                .expect("kept")
                .contains("shell-integration")
        );

        // remove --force drops it and keeps a .bak of the previous state.
        let (code, out, _) = run(
            &["remove", "bitty-terminal.shell-integration", "--force"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("removed"), "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(!text.contains("shell-integration"));
        let backup = dir.join(format!("{STATE_FILE_NAME}{STATE_BACKUP_SUFFIX}"));
        assert!(backup.exists(), "backup kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_consent_fails_closed_on_eof_and_decline() {
        let dir = scratch_dir("consent");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        // EOF: aborted, no state file created.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_GENERIC, "{out}");
        assert!(!state_path(&dir).exists(), "no state on abort");

        // Explicit decline: same, with the prompt shown.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "n\n",
        );
        assert_eq!(code, EXIT_GENERIC, "{out}");
        assert!(out.contains("terminal.semantic-read"), "{out}");
        assert!(out.contains("Read structured terminal content"), "{out}");
        assert!(!state_path(&dir).exists(), "no state on decline");

        // Approval writes the record.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "y\n",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(state_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_rejects_unknown_and_non_bundled_ids() {
        let dir = scratch_dir("unknown");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        let (code, _, _) = run(&["install", "xuepoo.markdown", "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_PLUGIN);
        let (code, _, _) = run(&["install", "not_an_id", "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_USAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_fails_closed_on_pin_mismatch() {
        let dir = scratch_dir("pin");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        let file = state_path(&dir);
        let mut records = BTreeMap::new();
        records.insert(
            "bitty-terminal.shell-integration".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "00".repeat(32),
                enabled: false,
                granted: BTreeSet::new(),
            },
        );
        std::fs::write(&file, PluginState::from_records(records).render()).expect("seed state");

        let (code, _, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let text = std::fs::read_to_string(&file).expect("state kept");
        assert!(
            text.contains("enabled = false"),
            "no mutation on pin mismatch"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_fails_closed() {
        let dir = scratch_dir("corrupt");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        std::fs::write(state_path(&dir), "state_version = 1\nbogus = 1\n").expect("seed");
        let (code, _, _) = run(&["list"], Some(&config), "");
        assert_eq!(code, EXIT_PLUGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_info_json_envelopes_are_versioned() {
        let dir = scratch_dir("envelopes");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");

        let (code, out, _) = run(&["list", "--format", "json"], Some(&config), "");
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("\"v\":1"), "{out}");
        assert!(out.contains("\"command\":\"plugin\""), "{out}");
        assert!(out.contains("\"state\":\"enabled\""), "{out}");
        assert!(out.contains("\"pin_ok\":true"), "{out}");

        let (code, out, _) = run(
            &[
                "info",
                "bitty-terminal.shell-integration",
                "--format",
                "json",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("\"verb\":\"info\""), "{out}");
        assert!(out.contains("\"manifest_hash\""), "{out}");
        assert!(out.contains("Read structured terminal content"), "{out}");
        assert!(out.contains("\"granted\":true"), "{out}");

        // Unknown plugin fails closed with a failure envelope in json mode.
        let (code, out, _) = run(
            &["info", "xuepoo.markdown", "--format", "json"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        assert!(out.contains("\"ok\":false"), "{out}");
        assert!(out.contains("PluginNotFound"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consent_prompt_marks_high_risk_capabilities() {
        let mut output = Vec::new();
        let manifest = bundled_manifest_for("bitty-terminal.shell-integration").expect("bundled");
        let mut needed = BTreeSet::new();
        needed.insert(CapabilityId::parse("terminal.raw-read").expect("known high risk"));
        let mut input = Cursor::new(b"n\n".to_vec());
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("answered");
        assert!(!approved);
        let text = String::from_utf8(output).expect("utf-8");
        assert!(text.contains("(high risk)"), "{text}");
        assert!(text.contains("Read raw terminal bytes"), "{text}");
        assert!(text.contains("Grant these capabilities? [y/N]:"), "{text}");
    }
}
