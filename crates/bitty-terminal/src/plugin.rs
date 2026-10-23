//! `bitty plugin`: CLI-first plugin management (CTX-0150, issue #244).
//!
//! Canonical direction: `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/product/plugin-roadmap.md` owner
//! direction 2026-09-03 (DEC-0007): management UX is subcommand-first and the
//! durable state is a managed manifest, never hand-edited Lua. Candidate
//! layout and semantics from
//! `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/extensibility/package-management.md` and
//! `default-distribution-rfc.md` § "Managed manifest".
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty plugin list|install|remove|enable|disable|revoke|grant|info|config`
//!   with `list`/`info`/`config show` accepting `--format table|json|jsonl`
//!   (default table) and `--no-color`; `doctor` also accepts `--no-color`.
//! - `install <id>` resolves an exact id against the **bundled** catalog or
//!   the build-time **official** catalog embedded from bitty-plugins
//!   (`bitty_plugin_host::official`). Bundled ids pin
//!   `PluginManifest::manifest_hash()`; official ids carry the registry
//!   repository, pinned commit, raw `bitty-plugin.toml` `sha256`, and declared
//!   compatibility, so resolution needs no network metadata request. An
//!   official entry that excludes this host is refused with the exact range
//!   before any fetch; an installable one fetches exactly the pinned commit,
//!   verifies the raw manifest hash, then runs the package pipeline with
//!   `registry` provenance and the same capability consent. A capability
//!   increase on an existing record blocks until approved; unchanged or
//!   narrowed sets carry forward silently (the P0-AC-030 pattern over
//!   `bitty-plugin-host` grants).
//! - `install <path>` installs (or updates) an **external** package from a
//!   local directory into the XDG data store
//!   (`$XDG_DATA_HOME/bitty/plugins/packages/<id>/<version>/`): manifest,
//!   schema, entry point, module-tree bounds, and `compat` ranges are
//!   verified before anything is staged, added capabilities need the same
//!   consent, and the atomic `current.json` pointer is switched last (the
//!   commit point). A failed commit rolls the version tree back; an
//!   interrupted/partial tree is surfaced by `list` and repaired on the next
//!   install after a `[y/N]` prompt (`--force` skips it; identical bytes are
//!   adopted without a prompt). Reinstalling the same manifest version with
//!   different content replaces the tree after the same prompt, and
//!   `--force` does it non-interactively. The runtime then discovers and
//!   activates the package as a visibly unverified third-party
//!   (`local-path`) source. `install <git-url>`
//!   clones a `https://<host>/<path>.git`, `git+https://…`, or
//!   `git@<host>:<path>` source with fixed-argv system `git` into an
//!   exclusive temp staging dir and then runs the same local-path pipeline
//!   unchanged. `install <archive>` unpacks a local `.tar.gz`/`.tgz`/
//!   `.tar.zst`/`.zip` file with fixed-argv system `tar`/`unzip` into the
//!   same kind of staging and then runs the same pipeline. Unsupported
//!   schemes (`file://`, `http://`, `ssh://`, …) fail closed with a naming
//!   diagnostic.
//! - `remove <id> --force` is destructive: bundled records lose their managed
//!   entry with `bitty-plugins.toml.bak` kept; installed packages delete their
//!   store record and staged tree. It also deletes an unindexed orphan tree
//!   when an operator explicitly chooses to discard it; the repair hint shown
//!   by `list` is the non-destructive `doctor --repair` path.
//!   `enable`/`disable` are idempotent toggles
//!   in the managed manifest or the store index respectively; `enable`
//!   re-checks the hash pin and the grant coverage and fails closed on a
//!   mismatch. `revoke <id> [--cap <capability>]` drops grants without
//!   dropping the record: a full revoke clears the granted set and disables,
//!   a `--cap` revoke drops one capability and remembers it as an explicit
//!   denial (no silent re-prompt); re-grant goes through `install` consent.
//! - `install <bundled-id> --profile <name>` and
//!   `grant <id> --profile <name>` apply a versioned category preselect
//!   (`minimal`, `editor-like`, `panel-like`, `agent-like`). A profile is
//!   only a set of individual grants drawn from the manifest's requested
//!   set, never a new authority layer: the diff/review screen runs before
//!   apply, every grant stays visible and revocable one by one, sensitive
//!   heads (exec, network, agent/MCP/AI, clipboard read, environment,
//!   terminal input/raw read, debug, browser, device control) are excluded
//!   and need explicit `grant --cap` consent, and an unknown/removed head
//!   fails closed against the live catalog. Store packages reject `--profile`
//!   because their install transaction owns the full grant set.
//! - `grant <id> --cap <capability>` is the explicit per-item consent path;
//!   it cannot exceed the manifest's requested set and never clears an
//!   explicit denial (`install` re-consent does).
//! - `list` shows every bundled plugin, every embedded official catalog entry
//!   (`available`, or `incompatible` when the declared range excludes this
//!   host), and any recorded or installed extra, with source, state
//!   (`enabled`/`disabled`/`available`/`orphaned`/`broken`), pin status,
//!   granted/requested capability counts, and a repair hint for unhealthy
//!   rows. `info` shows one plugin's manifest plus the recorded grants and
//!   their plain-language effect statements.
//! - `install` and `enable` success prints post-install next steps (CTX-1268,
//!   issue #2144; issue #2374): the enable command, where the plugin landed,
//!   the canonical Lua settings path (`<config-root>/plugins/<owner>.<name>.lua`),
//!   the `config show`/`config scaffold` pointers, manifest-provided `[hints]`
//!   lines when declared, and the `info` pointer. Legacy TOML is never
//!   advertised here; it stays a runtime compatibility fallback documented in
//!   the manual. Display-only: no grants, no state, no plugin code runs.
//! - `config show <id>` prints the declared `[settings]` schema (key, type,
//!   default, description when the schema carries one), the canonical/alias/
//!   legacy paths and which file is selected, and the effective value plus
//!   serving layer (`manifest-default`, `host`, `user-file`) per key. The
//!   bounded plain settings VM resolves the file exactly as activation does;
//!   the plugin VM is never created or executed. Undeclared/wrong-typed keys
//!   and rejected files fail closed with exit 4 after the report prints.
//! - `config scaffold <id>` writes a commented canonical
//!   `<config-dir>/plugins/<owner>.<name>.lua` template from the manifest
//!   `[settings]` defaults only (no invented values). It refuses to overwrite
//!   without `--force`, always refuses symlinked targets/directories, keeps
//!   the write inside the environment-derived config root, and commits
//!   atomically (temp file with `create_new`; hard-link no-clobber or rename
//!   with `--force`). `--init` is the issue-shape alias for `scaffold`.
//! - `doctor [--repair] [--no-color]` diagnoses the package store and the
//!   managed ledger without creating a VM: corrupt `current.json`,
//!   orphaned/incomplete/duplicate version dirs, dangling records, stale
//!   staging, malformed or stale ledger records. `--repair` moves broken/extra
//!   trees into `<store>/quarantine/` (never deletes them), restores or
//!   quarantines a `.tmp-replaced-*` tree left by an interrupted commit,
//!   removes stale staging, moves a corrupt index/ledger aside, and rebuilds
//!   `current.json` from complete package dirs with the index write last and
//!   atomic. The authoritative rebuild is never truncated: over the safe
//!   repair bound it refuses with a typed error and changes nothing. Rebuilt
//!   records keep grants only when the manifest hash still matches; otherwise
//!   they start disabled with no grants and need a fresh `install` consent.
//! - `explain <id> <capability> [--format table|json|jsonl]` prints the
//!   read-only capability decision chain for one declared capability: every
//!   layer in authority order (`host-ceiling`, `user-policy`,
//!   `project-policy`, `parent-delegation`, `task-grant`, `agent-request`)
//!   with allow/deny/empty, the first denying layer, `DenialKind`, the
//!   engine's bounded reason chain, resolved `TrustLevel`, the enforcement
//!   record, pins/conflicts, the effective grant, a static budget envelope,
//!   and minimal non-applying remediation hints. User/project policy files
//!   are read (bounded, regular files only) and marked advisory until the
//!   host wires them; the task grant and trust gate are enforced today. A
//!   denied capability exits 4 after the report; unknown ids/capabilities and
//!   malformed input fail closed with actionable messages, never a panic.
//! - Class: local-only (no instance, no IPC, **no plugin VM and no plugin
//!   code is ever loaded or executed**; safe-mode clean).
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
//! - Nothing is granted implicitly: the recorded grant set is always a
//!   subset of the manifest's requested set at approval time, bound to the
//!   manifest hash. A profile only preselects that subset; it cannot add
//!   authority, bypass the six-layer intersection, or resurrect a denial.
//! - No ambient authority is created by the CLI: it cannot load plugins, it
//!   cannot widen a grant without a fresh consent, and it never executes
//!   package code (P0-AC-027).
//!
//! # Exit codes (stable taxonomy, cli-contract-rfc.md)
//!
//! - `0` success (including idempotent no-op toggles).
//! - `1` aborted consent (declined/EOF), filesystem failure after approval,
//!   missing fetch helper (`git`/`tar`/`unzip`), `git clone` failure, or
//!   archive unpack failure (nothing staged).
//! - `2` usage error (missing/unknown verb, missing `<id>`, unknown flag,
//!   bad `--format`, stray `--`, malformed plugin id, destructive `remove`
//!   without `--force`, `--yes`/`--force`/`--format` on the wrong verb).
//! - `4` plugin error (unknown/non-bundled id, invalid or corrupt managed
//!   manifest, pin mismatch, uncovered grant, blocked capability increase,
//!   rejected fetch scheme such as `file://`/`http://`/`ssh://`, malformed
//!   git URL, hostile archive member).
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
use std::io::Read as _;
use std::path::{Path, PathBuf};

use bitty_config::plugin_file::PLUGINS_DIR_NAME;
use bitty_plugin_host::bundled::{all_bundled_manifests, bundled_ids_sorted, bundled_manifest_for};
use bitty_plugin_host::capability::{CapabilityId, effect_statement};
use bitty_plugin_host::consent_profile::{
    CONSENT_PROFILE_VERSION, ConsentProfile, ProfileSelection,
};
use bitty_plugin_host::effective::{
    AgentRequest, CapabilityExplanation, CapabilityScope, DenialKind, ENFORCEMENT_MAP,
    EffectiveLayer, EffectiveStack, EnforcementEntry, LayerState, MAX_POLICY_FILE_BYTES,
    PolicyProvenance, RequestKind, explain_capability, parse_policy, project_policy_path,
    user_policy_path_with_env,
};
use bitty_plugin_host::grant::{GrantRecord as HostGrantRecord, GrantStore};
use bitty_plugin_host::manifest::{PluginId, PluginManifest, SettingDecl, SettingDefault};
use bitty_plugin_host::official::{OfficialPlugin, official_plugin_for, official_plugins};
use bitty_plugin_host::trust_levels::TrustLevel;
use bitty_runtime::plugin_runtime::plugin_settings::{
    self, LoadedPluginSettings, PluginConfigKind, PluginConfigPaths, SelectedPluginFile,
    SettingLayer,
};
use bitty_runtime::plugin_runtime::{
    PLUGIN_MANIFEST_MAX_BYTES, SourceClass, load_index, manifest_toml,
    package::{self, LocalInstallOptions, PackageOpError},
};

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
/// Prefix for the scaffold temp file created beside the target (atomic write).
pub const SCAFFOLD_TEMP_PREFIX: &str = ".bitty-plugin-config-";
/// Maximum rendered scaffold bytes (declared defaults are already bounded).
pub const MAX_SCAFFOLD_BYTES: usize = 64 * 1024;
/// Largest single value rendered in a `config show` table cell.
pub const CONFIG_TABLE_CELL_BYTES: usize = 48;

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
    /// `explain <id> <capability>`: read-only capability decision chain.
    Explain,
    /// `revoke <id> [--cap <capability>]`: drop grants (durable denial).
    Revoke,
    /// `grant <id> --profile <name> | --cap <capability>`: add reviewed grants.
    Grant,
    /// `config [show|scaffold] <id>`: inspect or scaffold per-plugin settings.
    Config,
    /// `doctor [--repair]`: diagnose store/ledger health and optionally repair.
    Doctor,
}

/// Which `bitty plugin config` action to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigAction {
    /// Print the declared schema, selected file, and effective values.
    Show,
    /// Write a commented settings template at the canonical path.
    Scaffold,
}

impl ConfigAction {
    /// Every plugin-config action in help order.
    pub const ACTIONS: &[(&str, Self)] = &[("show", Self::Show), ("scaffold", Self::Scaffold)];

    /// Parse an explicit action token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ACTIONS
            .iter()
            .find(|(name, _)| *name == token)
            .map(|(_, action)| *action)
    }
}

impl PluginVerb {
    /// Every plugin verb in help order (single source for parsing and shell
    /// completion; never duplicate this list).
    pub const VERBS: &[(&str, Self)] = &[
        ("list", Self::List),
        ("install", Self::Install),
        ("remove", Self::Remove),
        ("enable", Self::Enable),
        ("disable", Self::Disable),
        ("info", Self::Info),
        ("explain", Self::Explain),
        ("revoke", Self::Revoke),
        ("grant", Self::Grant),
        ("config", Self::Config),
        ("doctor", Self::Doctor),
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
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Info => "info",
            Self::Explain => "explain",
            Self::Revoke => "revoke",
            Self::Grant => "grant",
            Self::Config => "config",
            Self::Doctor => "doctor",
        }
    }

    /// Whether the verb takes a plugin id operand.
    #[must_use]
    pub fn needs_id(self) -> bool {
        !matches!(self, Self::List | Self::Doctor)
    }
}

/// Validated request for one `bitty plugin` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRequest {
    /// Requested verb.
    pub verb: PluginVerb,
    /// Plugin id operand (validated as a token; grammar checked at resolve).
    pub id: Option<String>,
    /// Capability operand for `explain` (validated as a closed id at dispatch).
    pub capability: Option<String>,
    /// Output shape for `list`/`info`/`explain`/`config show`.
    pub format: PluginFormat,
    /// `install --yes`: approve capability consent non-interactively.
    pub yes: bool,
    /// `remove --force`: required for the destructive removal.
    pub force: bool,
    /// `revoke --cap <capability>`: revoke one capability instead of all.
    pub cap: Option<String>,
    /// `install`/`grant --profile <name>`: reviewed capability preselect.
    pub profile: Option<String>,
    /// `--no-color` (accepted for parity; tables are plain text).
    pub no_color: bool,
    /// `config` action (`show` default, `scaffold` via the verb or `--init`).
    pub config_action: Option<ConfigAction>,
    /// `doctor --repair`: quarantine broken trees and rebuild `current.json`.
    pub repair: bool,
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
    let mut capability: Option<String> = None;
    let mut format: Option<String> = None;
    let mut yes = false;
    let mut force = false;
    let mut cap: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut no_color = false;
    let mut config_action: Option<ConfigAction> = None;
    let mut init = false;
    let mut repair = false;

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
            "--repair" => {
                repair = true;
                index += 1;
                continue;
            }
            "--init" => {
                init = true;
                index += 1;
                continue;
            }
            "--json" => {
                format = Some("json".to_string());
                index += 1;
                continue;
            }
            "--cap" => {
                // Pair form: consume the next token as the capability value;
                // a missing value is usage (fail closed before dispatch).
                let Some(value) = raw.get(index + 1) else {
                    return Err(PluginParseError::Usage(
                        "bitty plugin: --cap needs a value (a capability id)".to_string(),
                    ));
                };
                if !bare_token_ok(value) {
                    return Err(PluginParseError::Usage(format!(
                        "bitty plugin: --cap value is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
                    )));
                }
                if cap.is_some() {
                    return Err(PluginParseError::Usage(
                        "bitty plugin: duplicate --cap".to_string(),
                    ));
                }
                cap = Some(value.clone());
                index += 2;
                continue;
            }
            "--profile" => {
                let Some(value) = raw.get(index + 1) else {
                    return Err(PluginParseError::Usage(
                        "bitty plugin: --profile needs a value (minimal|editor-like|panel-like|agent-like)"
                            .to_string(),
                    ));
                };
                if !bare_token_ok(value) {
                    return Err(PluginParseError::Usage(format!(
                        "bitty plugin: --profile value is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
                    )));
                }
                if profile.is_some() {
                    return Err(PluginParseError::Usage(
                        "bitty plugin: duplicate --profile".to_string(),
                    ));
                }
                profile = Some(value.clone());
                index += 2;
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
        if let Some(value) = token.strip_prefix("--cap=") {
            if !bare_token_ok(value) {
                return Err(PluginParseError::Usage(format!(
                    "bitty plugin: --cap value is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
                )));
            }
            if cap.is_some() {
                return Err(PluginParseError::Usage(
                    "bitty plugin: duplicate --cap".to_string(),
                ));
            }
            cap = Some(value.to_string());
            index += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--profile=") {
            if !bare_token_ok(value) {
                return Err(PluginParseError::Usage(format!(
                    "bitty plugin: --profile value is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
                )));
            }
            if profile.is_some() {
                return Err(PluginParseError::Usage(
                    "bitty plugin: duplicate --profile".to_string(),
                ));
            }
            profile = Some(value.to_string());
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
                         (want list|install|remove|enable|disable|info|explain|revoke|grant|config|doctor)"
                    )));
                }
            }
            index += 1;
            continue;
        }
        if verb == Some(PluginVerb::Config) && config_action.is_none() {
            if let Some(action) = ConfigAction::parse(token) {
                config_action = Some(action);
                index += 1;
                continue;
            }
        }
        if id.is_none() {
            id = Some(token.to_string());
            index += 1;
            continue;
        }
        if verb == Some(PluginVerb::Explain) && capability.is_none() {
            capability = Some(token.to_string());
            index += 1;
            continue;
        }
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: unexpected extra argument {token:?}"
        )));
    }

    let verb = verb.ok_or_else(|| {
        PluginParseError::Usage(
            "bitty plugin: missing verb (want list|install|remove|enable|disable|info|explain|revoke|grant|config|doctor)"
                .to_string(),
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
    if verb == PluginVerb::Explain && capability.is_none() {
        return Err(PluginParseError::Usage(
            "bitty plugin: `explain` needs a capability id after the plugin id \
             (for example `bitty plugin explain bitty-terminal.shell-integration \
             terminal.semantic-read`)"
                .to_string(),
        ));
    }
    if verb != PluginVerb::Explain && capability.is_some() {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: `{}` takes no capability operand",
            verb.name()
        )));
    }
    if !verb.needs_id() && id.is_some() {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: `{}` takes no plugin id",
            verb.name()
        )));
    }
    if yes && !matches!(verb, PluginVerb::Install | PluginVerb::Grant) {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --yes only applies to `install` and `grant` (got `{}`)",
            verb.name()
        )));
    }
    if cap.is_some() && !matches!(verb, PluginVerb::Revoke | PluginVerb::Grant) {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --cap only applies to `revoke` and `grant` (got `{}`)",
            verb.name()
        )));
    }
    if profile.is_some() && !matches!(verb, PluginVerb::Install | PluginVerb::Grant) {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --profile only applies to `install` and `grant` (got `{}`)",
            verb.name()
        )));
    }
    if let Some(name) = profile.as_deref() {
        if ConsentProfile::parse(name).is_none() {
            return Err(PluginParseError::Usage(format!(
                "bitty plugin: unknown --profile {name:?} (want minimal|editor-like|panel-like|agent-like)"
            )));
        }
    }
    if verb == PluginVerb::Grant {
        match (profile.is_some(), cap.is_some()) {
            (true, true) => {
                return Err(PluginParseError::Usage(
                    "bitty plugin: `grant` takes one of --profile or --cap, not both".to_string(),
                ));
            }
            (false, false) => {
                return Err(PluginParseError::Usage(
                    "bitty plugin: `grant` needs --profile <name> or --cap <capability>"
                        .to_string(),
                ));
            }
            _ => {}
        }
    }
    if format != PluginFormat::Table
        && !matches!(
            verb,
            PluginVerb::List | PluginVerb::Info | PluginVerb::Config | PluginVerb::Explain
        )
    {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --format/--json only apply to `list`, `info`, `explain`, and `config` (got `{}`)",
            verb.name()
        )));
    }
    if no_color
        && !matches!(
            verb,
            PluginVerb::List
                | PluginVerb::Info
                | PluginVerb::Config
                | PluginVerb::Explain
                | PluginVerb::Doctor
        )
    {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --no-color only applies to `list`, `info`, `explain`, `config`, and `doctor` (got `{}`)",
            verb.name()
        )));
    }
    if config_action.is_some() && verb != PluginVerb::Config {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: `show`/`scaffold` only apply to `config` (got `{}`)",
            verb.name()
        )));
    }
    if init && verb != PluginVerb::Config {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --init only applies to `config` (got `{}`)",
            verb.name()
        )));
    }
    let config_action = if verb == PluginVerb::Config {
        match (config_action, init) {
            (Some(ConfigAction::Show), true) => {
                return Err(PluginParseError::Usage(
                    "bitty plugin: `config show` conflicts with --init (scaffold)".to_string(),
                ));
            }
            (Some(action), _) => Some(action),
            (None, true) => Some(ConfigAction::Scaffold),
            (None, false) => Some(ConfigAction::Show),
        }
    } else {
        None
    };
    if force
        && !matches!(
            verb,
            PluginVerb::Install | PluginVerb::Remove | PluginVerb::Config
        )
    {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --force only applies to `install`, `remove`, and `config scaffold` (got `{}`)",
            verb.name()
        )));
    }
    if force && config_action == Some(ConfigAction::Show) {
        return Err(PluginParseError::Usage(
            "bitty plugin: --force only applies to `config scaffold`".to_string(),
        ));
    }
    if config_action == Some(ConfigAction::Scaffold) && format != PluginFormat::Table {
        return Err(PluginParseError::Usage(
            "bitty plugin: --format/--json only apply to `config show`".to_string(),
        ));
    }
    if repair && verb != PluginVerb::Doctor {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --repair only applies to `doctor` (got `{}`)",
            verb.name()
        )));
    }

    Ok(PluginRequest {
        verb,
        id,
        capability,
        format,
        yes,
        force,
        cap,
        profile,
        no_color,
        config_action,
        repair,
    })
}

/// Short usage block (`stderr` on usage failures).
#[must_use]
pub fn plugin_usage() -> String {
    "usage: bitty plugin list [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty plugin install <id> [--yes] [--profile <name>]   (bundled id)\n\
     \x20      bitty plugin install <id> [--yes]   (official catalog id)\n\
     \x20      bitty plugin install <path> [--yes] [--force]   (local directory package)\n\
     \x20      bitty plugin install <git-url> [--yes] [--force]   (https://<host>/<path>.git | git+https://... | git@<host>:<path>)\n\
     \x20      bitty plugin install <archive> [--yes] [--force]   (.tar.gz/.tgz/.tar.zst/.zip file)\n\
     \x20      bitty plugin remove <id> --force\n\
     \x20      bitty plugin doctor [--repair] [--no-color]\n\
     \x20      bitty plugin enable <id>\n\
     \x20      bitty plugin disable <id>\n\
     \x20      bitty plugin revoke <id> [--cap <capability>]\n\
     \x20      bitty plugin grant <id> --profile <name> [--yes]\n\
     \x20      bitty plugin grant <id> --cap <capability> [--yes]\n\
     \x20      bitty plugin info <id> [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty plugin explain <id> <capability> [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty plugin config show <id> [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty plugin config scaffold <id> [--force]\n\
     \x20      profiles (preselect only; review before apply): minimal | editor-like |\n\
     \x20      panel-like | agent-like. Sensitive caps stay explicit per-item consent.\n\
     \n\
     Bundled plugins are recorded in $XDG_CONFIG_HOME/bitty/bitty-plugins.toml\n\
     (fallback ~/.config/bitty/...), or beside an explicit --config path.\n\
     External packages install into $XDG_DATA_HOME/bitty/plugins (fallback\n\
     ~/.local/share/bitty/plugins). No plugin code runs during any operation.\n\
     Exit codes: 0 success | 1 aborted/io (consent, helper, clone, unpack,\n\
     repair needed) | 2 usage | 4 plugin error (id, scheme, validation).\n\
     `bitty plugin doctor` diagnoses orphaned/partial packages and a broken\n\
     index; `--repair` quarantines broken trees and rebuilds current.json.\n\
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
     \x20 list                        Show bundled, official-catalog, and installed\n\
     \x20                             plugins: source, state, pin status,\n\
     \x20                             granted/requested capabilities.\n\
     \x20 install <id> [--yes]        Resolve an exact id against the bundled\n\
     \x20                             catalog or the build-time official catalog.\n\
     \x20                             Official ids come from a pinned, hash-\n\
     \x20                             verified catalog generated from bitty-\n\
     \x20                             plugins (no runtime metadata fetch), and\n\
     \x20                             are installed from the pinned commit.\n\
     \x20                             Incompatible entries are refused with the\n\
     \x20                             exact declared range before any fetch.\n\
     \x20                             Capabilities need explicit consent; --yes\n\
     \x20                             approves without a prompt. Add\n\
     \x20                             --profile <name> for a bundled id to\n\
     \x20                             preselect a reviewed subset (see flags).\n\
     \x20 install <path> [--yes]      Install or update an external package from\n\
     \x20                             a local directory containing\n\
     \x20                             bitty-plugin.toml (lua/ module root). The\n\
     \x20                             manifest, entry point, bounds, and compat\n\
     \x20                             ranges are verified before anything is\n\
     \x20                             staged; added capabilities need consent.\n\
     \x20                             The package tree and current.json are\n\
     \x20                             committed together: a failed install\n\
     \x20                             rolls the tree back, so no version dir\n\
     \x20                             is left without an index entry. Re-running\n\
     \x20                             an install with the same version but\n\
     \x20                             different content (a rebuilt checkout)\n\
     \x20                             replaces the tree after a `[y/N]` prompt,\n\
     \x20                             or non-interactively with --force. An\n\
     \x20                             orphaned/partial tree left by an\n\
     \x20                             interrupted install takes the same path,\n\
     \x20                             and identical bytes are adopted without a\n\
     \x20                             prompt.\n\
     \x20 install <git-url> [--yes]  Install from a git source: exactly\n\
     \x20                             https://<host>/<path>.git,\n\
     \x20                             git+https://<host>/<path>.git (the git+\n\
     \x20                             prefix is stripped before cloning), or\n\
     \x20                             git@<host>:<path>. Fixed-argv system\n\
     \x20                             `git clone --filter=blob:none --depth=1\n\
     \x20                             <url> <staging-dir>` into an exclusive\n\
     \x20                             temp dir (caller env inherited for\n\
     \x20                             gitconfig/SSH-agent/proxy/CA), then the\n\
     \x20                             same local-path pipeline. file://,\n\
     \x20                             http://, ssh://, and other schemes fail\n\
     \x20                             closed with a naming diagnostic.\n\
     \x20 install <archive> [--yes]  Install from a local archive file:\n\
     \x20                             .tar.gz/.tgz (tar -xzf), .tar.zst (tar\n\
     \x20                             --use-compress-program=unzstd -xf), or\n\
     \x20                             .zip (unzip -q -d). Fixed-argv system\n\
     \x20                             helpers unpack into an exclusive temp\n\
     \x20                             staging dir (members audited for ../,\n\
     \x20                             absolute paths, and symlinks; declared\n\
     \x20                             unpacked content bounded at 64 MiB\n\
     \x20                             before extraction), then the\n\
     \x20                             same local-path pipeline. A missing\n\
     \x20                             helper or a bad/hostile archive fails\n\
     \x20                             closed with nothing staged.\n\
     \x20 remove <id> --force         Remove a record: bundled records drop from\n\
     \x20                             the managed manifest (previous copy kept as\n\
     \x20                             .bak); installed packages delete their store\n\
     \x20                             record and package tree.\n\
     \x20 enable <id>                 Re-enable a recorded plugin. Bundled pins\n\
     \x20                             and grants are re-checked; installed records\n\
     \x20                             are switched atomically in the store index.\n\
     \x20 disable <id>                Disable without dropping the record or grant.\n\
     \x20 revoke <id> [--cap <cap>]   Revoke grants without dropping the record:\n\
     \x20                             without --cap every granted capability is\n\
     \x20                             dropped and the plugin disabled; with --cap\n\
     \x20                             one capability is dropped and remembered\n\
     \x20                             as explicitly denied (no silent re-prompt).\n\
     \x20                             Re-grant with `install` (explicit consent).\n\
     \x20 grant <id> --profile <name> [--yes]\n\
     \x20                             Apply one category profile to a bundled\n\
     \x20                             record. The profile only preselects\n\
     \x20                             individual grants the manifest already\n\
     \x20                             requests; a diff/review screen lists\n\
     \x20                             additions, already-granted, non-selected,\n\
     \x20                             explicitly denied, and sensitive heads\n\
     \x20                             that need per-item consent. It never\n\
     \x20                             enables the plugin, revokes, or clears a\n\
     \x20                             denial. Profiles: minimal | editor-like |\n\
     \x20                             panel-like | agent-like.\n\
     \x20 grant <id> --cap <capability> [--yes]\n\
     \x20                             Explicitly grant one requested\n\
     \x20                             capability. This is the per-item path for\n\
     \x20                             exec/network/agent/clipboard-read/env and\n\
     \x20                             every other sensitive head; naming it is\n\
     \x20                             the consent action. It cannot add a\n\
     \x20                             capability the manifest does not request,\n\
     \x20                             and it never clears an explicit denial\n\
     \x20                             (re-run `install` to re-consent).\n\
     \x20 info <id>                   Manifest + recorded state, with the plain-\
     \x20                             language effect per capability.\n\
     \x20 explain <id> <capability>    Read-only capability decision chain for one\n\
     \x20                             declared capability: every layer\n\
     \x20                             (host/user/project/parent/task/request)\n\
     \x20                             with allow/deny/empty, the first denying\n\
     \x20                             layer, DenialKind, reason chain, trust\n\
     \x20                             level, enforcement record, effective\n\
     \x20                             grant, and a remediation hint. Exits 4\n\
     \x20                             when the capability is denied. No plugin\n\
     \x20                             code runs and nothing is mutated.\n\
     \x20 config show <id>            Print the declared [settings] schema, the\n\
     \x20                             selected user file, and the effective\n\
     \x20                             value plus serving layer per key. No\n\
     \x20                             plugin VM is created.\n\
     \x20 config scaffold <id>        Write a commented Lua settings template\n\
     \x20                             (canonical <owner>.<name>.lua path) from\n\
     \x20                             the manifest [settings] defaults. Refuses\n\
     \x20                             to overwrite without --force.\n\
     \x20 doctor [--repair] [--no-color]\n\
     \x20                             Diagnose the package store and the\n\
     \x20                             bitty-plugins.toml ledger. Reports\n\
     \x20                             orphaned/partial version dirs, dangling\n\
     \x20                             index records, a corrupt current.json,\n\
     \x20                             stale staging dirs, and stale ledger\n\
     \x20                             records. --repair moves broken/extra\n\
     \x20                             trees to <store>/quarantine/, removes\n\
     \x20                             stale staging, rebuilds current.json\n\
     \x20                             from complete trees (index last, atomic),\n\
     \x20                             and quarantines a malformed ledger.\n\
     \x20                             Rebuilt records with a changed manifest\n\
     \x20                             hash start disabled with no grants; re-run\n\
     \x20                             `install <source> --force` to re-consent.\n\
     \n\
     flags:\n\
     \x20 --format table|json|jsonl   list/info/explain/config show output (default table).\n\
     \x20 --json                      Alias for --format json.\n\
     \x20 --no-color                  list/info/explain/config/doctor output (tables\n\
     \x20                             are plain text; this also honors NO_COLOR).\n\
     \x20 --yes                       install/grant: approve capability consent and\n\
     \x20                             an orphan/same-version repair prompt. With\n\
     \x20                             --profile it approves only the reviewed\n\
     \x20                             preselected set, never sensitive heads.\n\
     \x20 --force                     install/remove/config scaffold: install\n\
     \x20                             replaces an orphaned or same-version tree\n\
     \x20                             without the repair prompt; remove confirms\n\
     \x20                             the destructive drop; config scaffold\n\
     \x20                             confirms overwrite.\n\
     \x20 --repair                    doctor only: quarantine/rebuild the store\n\
     \x20                             and ledger.\n\
     \x20 --init                      config only: alias for `config scaffold`.\n\
     \x20 --cap <capability>          revoke/grant only: revoke one granted\n\
     \x20                             capability, or explicitly grant one\n\
     \x20                             manifest-requested capability.\n\
     \x20 --profile <name>            install (bundled ids)/grant only: apply a\n\
     \x20                             versioned category preselect (minimal,\n\
     \x20                             editor-like, panel-like, agent-like).\n\
     \x20                             Profiles never include exec, network,\n\
     \x20                             agent/MCP/AI, clipboard read, environment,\n\
     \x20                             terminal input/raw read, debug, browser,\n\
     \x20                             or device control; grant those one at a\n\
     \x20                             time with --cap. Store packages reject\n\
     \x20                             --profile: their install transaction owns\n\
     \x20                             the full grant set.\n\
     \n\
     download consent:\n\
     \x20 Capability grants use explicit confirm: --yes approves\n\
     \x20 non-interactively, otherwise the installer prompts\n\
     \x20 `Grant these capabilities? [y/N]` (up to 3 attempts, 64 bytes per\n\
     \x20 answer); `y`/`yes` approves, `n`/`no`/empty declines, EOF aborts.\n\
     \x20 A decline or EOF exits 1 with nothing changed. Unchanged or\n\
     \x20 narrowed capability sets carry forward silently; added\n\
     \x20 capabilities always need fresh consent. Capability profiles are\n\
     \x20 only a preselection over individual grants: the review/diff screen\n\
     \x20 runs before apply, every selected capability is recorded and\n\
     \x20 revocable one by one, and the six-layer intersection plus every\n\
     \x20 existing denial path is unchanged. Native extension downloads\n\
     \x20 (`bitty extension install`) follow the same levels: the builtin\n\
     \x20 installer egress cdn.bitty.run:443 needs no extra grant, while\n\
     \x20 third-party hosts stay consent-gated and fail closed.\n\
     \n\
     authority:\n\
     \x20 Plugin code is never executed by any `bitty plugin` operation. A\n\
     \x20 capability is granted only when the manifest requests it and consent\n\
     \x20 is explicit; grants are bound to the exact manifest hash, so a later\n\
     \x20 manifest that adds a capability blocks until re-approved. Installed\n\
     \x20 packages come from local directories, git checkouts, local archives\n\
     \x20 (source class `local-path`), or the embedded official catalog\n\
     \x20 (source class `registry`, pinned commit + raw manifest hash verified),\n\
     \x20 are stored under packages/<id>/<version>/ with an owner-only mode, and\n\
     \x20 load as third-party packages. The official catalog is build-time data:\n\
     \x20 Core never fetches registry metadata at runtime.\n\
     \n\
     store recovery (issue #2439):\n\
     \x20 `bitty plugin list` shows orphaned/partial package directories with\n\
     \x20 state `orphaned` or `broken` and a repair hint instead of hiding\n\
     \x20 them. Safe recovery ladder:\n\
     \x20   bitty plugin doctor                 # diagnose, change nothing\n\
     \x20   bitty plugin doctor --repair        # quarantine broken trees,\n\
     \x20                                       # rebuild current.json/ledger\n\
     \x20   bitty plugin remove <id> --force    # discard one orphaned tree\n\
     \x20   bitty plugin install <source> --force  # reinstall/replace content\n\
     \x20 A corrupt current.json is never treated as an empty store: list and\n\
     \x20 install fail closed with the doctor repair command. `doctor --repair`\n\
     \x20 moves the old file to <store>/quarantine/ first. Rebuilt records keep\n\
     \x20 existing grants only when the manifest hash still matches; otherwise\n\
     \x20 they are disabled with no grants and must be re-approved.\n\
     \n\
     wiring (where things land, what to edit next):\n\
     \x20 The managed manifest lives beside the config file:\n\
     \x20 $XDG_CONFIG_HOME/bitty/bitty-plugins.toml (fallback\n\
     \x20 ~/.config/bitty/...; an explicit --config path selects its own\n\
     \x20 directory). Never hand-edit it: it is the grant ledger, and a\n\
     \x20 malformed file fails closed.\n\
     \x20 Per-plugin user files live next to it, under plugins/.\n\
     \x20 The canonical file is\n\
     \x20 $XDG_CONFIG_HOME/bitty/plugins/<owner>.<name>.lua: it returns a\n\
     \x20 table and runs in a host-free, budgeted configuration VM. A\n\
     \x20 <name>.lua short alias also works when exactly one discovered\n\
     \x20 plugin claims that name. The legacy data-only form\n\
     \x20 $XDG_CONFIG_HOME/bitty/plugins/<owner>.<name>.toml (flat\n\
     \x20 `key = value` lines only) applies only when no Lua file exists.\n\
     \x20 Unknown keys are ignored, bad values fall back to the plugin's\n\
     \x20 shipped defaults, and edits apply on the next activation, not\n\
     \x20 live. Installed packages\n\
     \x20 land under $XDG_DATA_HOME/bitty/plugins/packages/<id>/<version>/\n\
     \x20 (fallback ~/.local/share/...). Inspect with\n\
     \x20 `bitty plugin config show <id>`; scaffold with\n\
     \x20 `bitty plugin config scaffold <id>`.\n\
     \x20 After a fresh local, Git, or archive install, the plugin is staged and\n\
     \x20 disabled; updates preserve the existing enabled/disabled state. Enable\n\
     \x20 it explicitly with:\n\
     \x20 bitty plugin enable <id>\n\
     \x20 bitty plugin info <id>     # manifest, grants, and settings\n\
     \n\
     fetch:\n\
     \x20 Git and archive installs stage under an exclusive temp dir and run\n\
     \x20 zero plugin code. `git`/`tar`/`unzip` resolve via PATH (git.exe/\n\
     \x20 tar.exe on Windows); a missing helper fails closed. `unzip` may be\n\
     \x20 absent on Windows: .zip installs then fail closed with a diagnostic.\n\
     \x20 Helpers inherit the system proxy env and the system CA store (no\n\
     \x20 bundled certs, no custom TLS flags in Core).\n\
     \n\
     exit codes:\n\
     \x20 0 success (including idempotent no-ops).\n\
     \x20 1 aborted/io: declined/EOF consent, filesystem failure, missing\n\
     \x20    fetch helper, `git clone` failure, archive unpack failure.\n\
     \x20 2 usage: bad verb/flag/format/id, stray `--`, wrong-verb flags.\n\
     \x20 4 plugin error: unknown id, bad manifest/pin/grant, rejected fetch\n\
     \x20    scheme (file://, http://, ssh://, ...), malformed git URL,\n\
     \x20    hostile archive member, or a denied `plugin explain` capability.\n\
     \n\
     examples:\n\
     \x20 bitty plugin list\n\
     \x20 bitty plugin list --format json\n\
     \x20 bitty plugin install bitty-terminal.shell-integration --yes\n\
     \x20 bitty plugin install bitty-terminal.shell-integration --profile editor-like --yes\n\
     \x20 bitty plugin grant xuepoo.hello --profile panel-like\n\
     \x20 bitty plugin grant xuepoo.hello --cap network.connect:example.com:443\n\
     \x20 bitty plugin install bitty-terminal.bar --yes   (official catalog id)\n\
     \x20 bitty plugin install ./my-plugin --yes\n\
     \x20 bitty plugin install https://example.com/my-plugin.git --yes\n\
     \x20 bitty plugin install git+https://example.com/my-plugin.git --yes\n\
     \x20 bitty plugin install git@example.com:my-plugin.git --yes\n\
     \x20 bitty plugin install ./my-plugin.tar.gz --yes\n\
     \x20 bitty plugin install ./my-plugin.zip --yes\n\
     \x20 bitty plugin info xuepoo.hello\n\
     \x20 bitty plugin explain bitty-terminal.shell-integration terminal.semantic-read\n\
     \x20 bitty plugin explain bitty-terminal.shell-integration terminal.semantic-read --json\n\
     \x20 bitty plugin config show xuepoo.hello\n\
     \x20 bitty plugin config scaffold xuepoo.hello\n\
     \x20 bitty plugin disable bitty-terminal.shell-integration\n\
     \x20 bitty plugin remove xuepoo.hello --force"
        .to_string()
}

// ---------------------------------------------------------------------------
// Managed manifest (strict bounded TOML subset, owned by this module)
// ---------------------------------------------------------------------------

/// One recorded plugin: source, pinned manifest hash, enabled flag, the
/// granted capability set bound to that hash, and explicitly revoked
/// (denied) capabilities that must not be silently re-prompted.
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
    /// Explicitly revoked capabilities (durable deny-loop guard; only
    /// `install` consent clears entries that are re-granted).
    pub denied: BTreeSet<CapabilityId>,
}

/// Parsed managed manifest (`bitty-plugins.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginState {
    records: BTreeMap<String, PluginRecord>,
}

/// Per-section key accumulators during [`PluginState::parse`].
#[derive(Default)]
struct SectionAccum {
    source: Option<String>,
    manifest_hash: Option<String>,
    enabled: Option<bool>,
    granted: Option<BTreeSet<CapabilityId>>,
    denied: Option<BTreeSet<CapabilityId>>,
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
        let mut accum = SectionAccum::default();

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
                    flush_section(&mut state, section_id.take(), section_line, &mut accum)?;
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
                    reject_duplicate(accum.source.is_some(), "source", line)?;
                    let parsed = parse_quoted(value, line)?;
                    if parsed != "bundled" {
                        return Err(StateError::new(
                            line,
                            format!("unsupported source '{parsed}' (want bundled)"),
                        ));
                    }
                    accum.source = Some(parsed);
                }
                "manifest_hash" => {
                    reject_duplicate(accum.manifest_hash.is_some(), "manifest_hash", line)?;
                    let parsed = parse_quoted(value, line)?;
                    if !is_lower_hex(&parsed, HEX_HASH_LEN) {
                        return Err(StateError::new(
                            line,
                            format!(
                                "manifest_hash must be {HEX_HASH_LEN} lowercase hex characters"
                            ),
                        ));
                    }
                    accum.manifest_hash = Some(parsed);
                }
                "enabled" => {
                    reject_duplicate(accum.enabled.is_some(), "enabled", line)?;
                    accum.enabled = Some(match value {
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
                    reject_duplicate(accum.granted.is_some(), "granted", line)?;
                    accum.granted = Some(parse_capability_array(value, line)?);
                }
                "denied" => {
                    reject_duplicate(accum.denied.is_some(), "denied", line)?;
                    accum.denied = Some(parse_capability_array(value, line)?);
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
            flush_section(&mut state, section_id.take(), section_line, &mut accum)?;
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
             # Do not edit by hand; use bitty plugin list|install|remove|enable|disable|revoke|grant.\n",
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
            out.push_str("denied = [");
            for (index, capability) in record.denied.iter().enumerate() {
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
    accum: &mut SectionAccum,
) -> Result<(), StateError> {
    let Some(id) = id else {
        return Ok(());
    };
    let source = accum
        .source
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `source`")))?;
    let manifest_hash = accum.manifest_hash.take().ok_or_else(|| {
        StateError::new(line, format!("plugin '{id}' is missing `manifest_hash`"))
    })?;
    let enabled = accum
        .enabled
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `enabled`")))?;
    let granted = accum
        .granted
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `granted`")))?;
    if granted.len() > MAX_STATE_GRANTS {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' exceeds {MAX_STATE_GRANTS} granted capabilities"),
        ));
    }
    // `denied` is optional (absent in manifests written before revocation
    // existed); missing means no explicit denials.
    let denied = accum.denied.take().unwrap_or_default();
    if denied.len() > MAX_STATE_GRANTS {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' exceeds {MAX_STATE_GRANTS} denied capabilities"),
        ));
    }
    if !granted.is_disjoint(&denied) {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' lists a capability as both granted and denied"),
        ));
    }
    state.insert(
        id,
        PluginRecord {
            source,
            manifest_hash,
            enabled,
            granted,
            denied,
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

#[derive(Debug)]
enum BoundedReadError {
    NotFound,
    Unsafe(String),
    Io(String),
    TooLarge,
}

#[cfg(target_os = "macos")]
fn path_for_bounded_traversal(path: &Path) -> PathBuf {
    let temp_dir = std::env::temp_dir();
    if path.is_absolute() && path.starts_with(&temp_dir) {
        if let (Ok(canonical_temp), Ok(relative)) =
            (temp_dir.canonicalize(), path.strip_prefix(&temp_dir))
        {
            return canonical_temp.join(relative);
        }
    }
    path.to_path_buf()
}

#[cfg(unix)]
fn open_bounded_file(path: &Path) -> Result<std::fs::File, BoundedReadError> {
    #[cfg(target_os = "macos")]
    let path = path_for_bounded_traversal(path);
    let mut directory = std::fs::File::open(if path.is_absolute() { "/" } else { "." })
        .map_err(|error| BoundedReadError::Io(error.to_string()))?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            if matches!(
                component,
                std::path::Component::RootDir | std::path::Component::CurDir
            ) {
                continue;
            }
            return Err(BoundedReadError::Unsafe(
                "parent or prefix path component".to_string(),
            ));
        };
        let final_component = components.peek().is_none();
        let flags = nix::fcntl::OFlag::O_RDONLY
            | nix::fcntl::OFlag::O_NOFOLLOW
            | nix::fcntl::OFlag::O_NONBLOCK
            | nix::fcntl::OFlag::O_CLOEXEC;
        let fd = nix::fcntl::openat(&directory, name, flags, nix::sys::stat::Mode::empty())
            .map_err(|error| {
                if error == nix::errno::Errno::ENOENT {
                    BoundedReadError::NotFound
                } else if error == nix::errno::Errno::ELOOP {
                    BoundedReadError::Unsafe("symlink path component".to_string())
                } else {
                    BoundedReadError::Io(error.to_string())
                }
            })?;
        directory = std::fs::File::from(fd);
        let metadata = directory
            .metadata()
            .map_err(|error| BoundedReadError::Io(error.to_string()))?;
        if final_component {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(BoundedReadError::Unsafe("not a regular file".to_string()));
            }
        } else if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(BoundedReadError::Unsafe("not a directory".to_string()));
        }
        if final_component {
            return Ok(directory);
        }
    }
    Err(BoundedReadError::Unsafe(
        "path has no file name".to_string(),
    ))
}

/// Read a regular file from a stable descriptor, refusing symlinked path
/// components and special files. The descriptor is capped before allocation.
fn read_bounded_regular_file(
    path: &Path,
    limit: usize,
) -> Result<Option<Vec<u8>>, BoundedReadError> {
    #[cfg(unix)]
    let file = open_bounded_file(path)?;
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    BoundedReadError::NotFound
                } else {
                    BoundedReadError::Io(error.to_string())
                }
            })?
    };
    #[cfg(not(any(unix, windows)))]
    let file =
        std::fs::File::open(path).map_err(|error| BoundedReadError::Io(error.to_string()))?;

    let metadata = file
        .metadata()
        .map_err(|error| BoundedReadError::Io(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(BoundedReadError::Unsafe("not a regular file".to_string()));
    }
    if metadata.len() > limit as u64 {
        return Err(BoundedReadError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| BoundedReadError::Io(error.to_string()))?;
    if bytes.len() > limit || bytes.len() as u64 != metadata.len() {
        return Err(BoundedReadError::TooLarge);
    }
    Ok(Some(bytes))
}

fn load_state(path: &Path) -> Result<PluginState, PluginFailure> {
    let bytes = match read_bounded_regular_file(path, MAX_STATE_FILE_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) | Err(BoundedReadError::NotFound) => return Ok(PluginState::new()),
        Err(BoundedReadError::TooLarge) => {
            return Err(PluginFailure::plugin(
                "StateInvalid",
                format!(
                    "bitty plugin: '{}' exceeds {MAX_STATE_FILE_BYTES} bytes",
                    path.display()
                ),
            ));
        }
        Err(BoundedReadError::Unsafe(reason)) | Err(BoundedReadError::Io(reason)) => {
            return Err(PluginFailure::generic(
                "IoError",
                format!("bitty plugin: cannot read '{}': {reason}", path.display()),
            ));
        }
    };
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

/// Owner-only mode for the managed manifest and its `.bak` (Unix, CTX-0293).
#[cfg(unix)]
const STATE_FILE_MODE: u32 = 0o600;

/// Owner-only mode for the directory holding the managed manifest (CTX-0293).
#[cfg(unix)]
const STATE_DIR_MODE: u32 = 0o700;

/// Force owner-only permissions on the managed manifest or its `.bak`.
///
/// The manifest is a capability-grant ledger; `std::fs::write` inherits the
/// process umask, so the mode is set explicitly (P0-AC-026 defense in depth,
/// CTX-0293). Non-Unix targets have no equivalent mode bits here and no-op.
#[cfg(unix)]
fn harden_managed_file(path: &Path) -> Result<(), PluginFailure> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(STATE_FILE_MODE)).map_err(
        |error| {
            PluginFailure::generic(
                "IoError",
                format!(
                    "bitty plugin: cannot set mode {STATE_FILE_MODE:03o} on '{}': {error}",
                    path.display()
                ),
            )
        },
    )
}

#[cfg(not(unix))]
fn harden_managed_file(_path: &Path) -> Result<(), PluginFailure> {
    Ok(())
}

/// Force owner-only permissions on the directory holding the manifest.
#[cfg(unix)]
fn harden_managed_dir(path: &Path) -> Result<(), PluginFailure> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(STATE_DIR_MODE)).map_err(
        |error| {
            PluginFailure::generic(
                "IoError",
                format!(
                    "bitty plugin: cannot set mode {STATE_DIR_MODE:03o} on '{}': {error}",
                    path.display()
                ),
            )
        },
    )
}

#[cfg(not(unix))]
fn harden_managed_dir(_path: &Path) -> Result<(), PluginFailure> {
    Ok(())
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
            harden_managed_dir(parent)?;
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
        harden_managed_file(&backup_path)?;
        backup = Some(backup_path);
    }
    std::fs::write(path, rendered).map_err(|error| {
        PluginFailure::generic(
            "IoError",
            format!("bitty plugin: cannot write '{}': {error}", path.display()),
        )
    })?;
    harden_managed_file(path)?;
    Ok(backup)
}

// ---------------------------------------------------------------------------
// Init follow-ups (CTX-1132, #1913): offline-first batch recording
// ---------------------------------------------------------------------------

/// Resolve the managed-manifest path for `bitty init` follow-ups.
///
/// Mirrors [`resolve_state_path`] (explicit `--config`/`BITTY_CONFIG` parent,
/// else XDG config root): the init wizard records into the same
/// `bitty-plugins.toml` `plugin install <id>` uses, so a recorded queue is
/// consumable later by `plugin install`. Returns a plain string error for the
/// init dispatch (English-only, no secrets).
pub(crate) fn init_plugin_state_path(
    config_path: Option<&str>,
    bitty_config_env: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    resolve_state_path(config_path, bitty_config_env, xdg_config_home, home)
        .map_err(|failure| failure.message)
}

/// Record first-batch bundled ids offline without fetching or consent.
///
/// For each id in `ids` (validated against the same bundled source
/// `plugin install <id>` uses via [`bundled_manifest_for`], no duplicate
/// list): when absent, inserts a fresh disabled record (`source = "bundled"`,
/// pinned `manifest_hash`, `enabled = false`, empty grants/denials) so no
/// capability is granted without explicit consent.
///
/// Merge rules (never clobber user state): an id that already has a record is
/// always skipped, whatever its stored `manifest_hash`. The existing
/// `enabled` flag, `granted` set, and durable `denied` set (the deny-loop
/// guard only consent clears) are preserved untouched, and a stale hash is
/// never silently refreshed here — hash transitions (including the
/// `GrantStore::apply_update` gate) belong to the install consent flow via
/// [`init_install_bundled_with_consent`] or a later `plugin install <id>`.
/// Only ids with no record at all write fresh disabled records.
///
/// Offline-only: filesystem only, no network, no secrets. Fails closed on an
/// unknown id, an over-limit state, or any filesystem error (nothing partially
/// written: the state is saved once at the end with a `.bak` backup).
/// Returns the number of newly recorded ids.
pub(crate) fn init_record_bundled_offline(
    state_path: &Path,
    ids: &[String],
) -> Result<usize, String> {
    let mut state = load_state(state_path).map_err(|failure| failure.message)?;
    let mut recorded = 0usize;
    for id in ids {
        let manifest = bundled_manifest_for(id.trim()).ok_or_else(|| {
            let valid = bitty_plugin_host::bundled::bundled_ids_sorted().join(", ");
            format!(
                "bitty init: unknown plugin '{}' (bundled ids: {})",
                id.trim(),
                if valid.is_empty() {
                    "(none)".to_string()
                } else {
                    valid
                }
            )
        })?;
        manifest
            .validate()
            .map_err(|error| format!("bitty init: bundled manifest '{id}' is invalid: {error}"))?;
        let manifest_hash = manifest.manifest_hash();
        // Never touch an existing record: grants, denials, and hash
        // transitions belong to the install consent flow.
        if state.get(id.trim()).is_some() {
            continue;
        }
        if state.records.len() >= MAX_STATE_PLUGINS {
            return Err(format!(
                "bitty init: too many plugins (limit {MAX_STATE_PLUGINS}) — nothing changed"
            ));
        }
        state.insert(
            id.trim().to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash,
                enabled: false,
                granted: BTreeSet::new(),
                denied: BTreeSet::new(),
            },
        );
        recorded += 1;
    }
    if recorded > 0 {
        save_state(state_path, &state).map_err(|failure| failure.message)?;
    }
    Ok(recorded)
}

/// Install one bundled plugin via the install consent flow (CTX-1132, #1913).
///
/// Reuses [`op_install`] (per-plugin `Grant these capabilities? [y/N]`
/// consent, `--yes` honors) then persists with the same backup/hardening as
/// `plugin install <id>`. Call only after explicit opt-in (interactive `y` or
/// `--yes`); offline-safe for bundled ids (no network fetch, capability
/// consent only). Returns the install summary line.
pub(crate) fn init_install_bundled_with_consent(
    state_path: &Path,
    id: &str,
    yes: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<String, String> {
    let mut state = load_state(state_path).map_err(|failure| failure.message)?;
    let result = op_install(&mut state, id.trim(), yes, None, input, output)
        .map_err(|failure| failure.message)?;
    let summary = result.summary.clone();
    if result.changed {
        save_state(state_path, &state).map_err(|failure| failure.message)?;
    }
    Ok(summary)
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
    /// Provenance label (`bundled`, `registry`, `git`, `local-path`).
    pub source: String,
    /// `enabled`/`disabled` when recorded, else `available`.
    pub state: &'static str,
    /// Recorded enabled flag.
    pub enabled: bool,
    /// Whether the managed `bitty-plugins.toml` ledger records this id.
    ///
    /// Store-index rows are **not** ledger records: `state` describes their
    /// desired store state, not a managed record. Runtime inspection reports
    /// this field verbatim so a dashboard can tell "installed and enabled"
    /// from "recorded but not yet installed".
    pub recorded: bool,
    /// Hash pin status: `Some(true)` verified, `Some(false)` stale, `None`
    /// when there is no manifest to verify against.
    pub pin_ok: Option<bool>,
    /// Capabilities the manifest requests (recorded set when no manifest).
    pub requested: BTreeSet<CapabilityId>,
    /// Capabilities recorded as granted.
    pub granted: BTreeSet<CapabilityId>,
    /// Capabilities explicitly revoked (durable denials; managed records only).
    pub denied: BTreeSet<CapabilityId>,
    /// Qualified commands from the static manifest.
    pub commands: Vec<String>,
    /// Pinned manifest hash when recorded.
    pub manifest_hash: Option<String>,
    /// Declared settings (issue #2396 read-only inspection only).
    ///
    /// Declarations carry their shipped default and kind; values are resolved
    /// separately from the per-plugin user file. Empty when no manifest body
    /// is available (a recorded id without a bundled/staged manifest).
    pub settings: Vec<bitty_plugin_host::manifest::SettingDecl>,
    /// Actionable repair command when the row is orphaned or broken.
    ///
    /// `None` for healthy bundled/installed rows. `list` prints these in a
    /// repair footer and `info` as a `repair:` line so an interrupted install
    /// is never silently hidden.
    pub repair: Option<String>,
}

impl PluginRow {
    /// Capability rows for `info`/envelopes: requested, granted, effect, risk.
    #[must_use]
    pub fn capability_rows(&self) -> Vec<CapabilityRow> {
        let mut ids: BTreeSet<CapabilityId> = self.requested.clone();
        ids.extend(self.granted.iter().cloned());
        ids.extend(self.denied.iter().cloned());
        ids.into_iter()
            .map(|capability| CapabilityRow {
                effect: effect_statement(&capability).to_string(),
                high_risk: capability.is_high_risk(),
                requested: self.requested.contains(&capability),
                granted: self.granted.contains(&capability),
                denied: self.denied.contains(&capability),
                id: capability.as_str().to_string(),
            })
            .collect()
    }

    /// Explicitly revoked capabilities in sorted order (display only).
    #[must_use]
    pub fn denied_ids(&self) -> Vec<String> {
        self.denied
            .iter()
            .map(|capability| capability.as_str().to_string())
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
    /// Whether it was explicitly revoked (re-grant needs `install` consent).
    pub denied: bool,
}

fn rows_from_state(
    state: &PluginState,
    store_root: Option<&Path>,
) -> Result<Vec<PluginRow>, PluginFailure> {
    rows_from_state_with_catalog(state, store_root, true)
}

/// Build plugin rows, optionally appending the embedded official catalog.
///
/// `bitty plugin list`/`info` include not-yet-installed official entries;
/// read-only runtime inspection keeps its existing bundled + recorded +
/// installed contract and never advertises catalog entries a plugin panel
/// cannot install.
fn rows_from_state_with_catalog(
    state: &PluginState,
    store_root: Option<&Path>,
    include_official: bool,
) -> Result<Vec<PluginRow>, PluginFailure> {
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
        let recorded = record.is_some();
        let (state_label, enabled, pin_ok, granted, denied, manifest_hash) = match record {
            Some(record) => (
                if record.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                record.enabled,
                Some(record.manifest_hash == manifest.manifest_hash()),
                record.granted.clone(),
                record.denied.clone(),
                Some(record.manifest_hash.clone()),
            ),
            None => (
                "available",
                false,
                None,
                BTreeSet::new(),
                BTreeSet::new(),
                None,
            ),
        };
        rows.push(PluginRow {
            id,
            name: manifest.identity.name.clone(),
            version: manifest.identity.version.clone(),
            description: manifest.identity.description.clone(),
            bundled: true,
            source: "bundled".to_string(),
            state: state_label,
            enabled,
            recorded,
            pin_ok,
            requested,
            granted,
            denied,
            commands: manifest
                .lazy
                .commands
                .iter()
                .map(|command| command.id.as_str().to_string())
                .collect(),
            manifest_hash,
            settings: manifest.settings.clone(),
            repair: None,
        });
    }

    for id in state.ids() {
        if seen.contains(id) {
            continue;
        }
        seen.insert(id.clone());
        let record = state.get(id).expect("iterated id exists");
        rows.push(PluginRow {
            id: id.clone(),
            name: id.clone(),
            version: "-".to_string(),
            description: "recorded plugin without a bundled manifest".to_string(),
            bundled: false,
            source: record.source.clone(),
            state: if record.enabled {
                "enabled"
            } else {
                "disabled"
            },
            enabled: record.enabled,
            recorded: true,
            pin_ok: None,
            requested: record.granted.clone(),
            granted: record.granted.clone(),
            denied: record.denied.clone(),
            commands: Vec::new(),
            manifest_hash: Some(record.manifest_hash.clone()),
            settings: Vec::new(),
            repair: None,
        });
    }

    if let Some(store_root) = store_root {
        for record in load_store_index(store_root)? {
            if seen.contains(&record.plugin_id) {
                continue;
            }
            seen.insert(record.plugin_id.clone());
            rows.push(store_row(store_root, &record));
        }

        // Issue #2439: an interrupted/partial install leaves a version
        // directory with no index record. Never hide it: list it with an
        // actionable repair command. A plugin that already has a row (record
        // or bundled) gets the hint attached to that row instead of a
        // duplicate id.
        let orphans = package::scan_orphans(store_root).map_err(|error| {
            PluginFailure::plugin(
                "StoreInvalid",
                format!(
                    "bitty plugin: plugin store '{}' is unreadable: {error}; run \
                     `bitty plugin doctor --repair` to rebuild the store index",
                    store_root.display()
                ),
            )
        })?;
        let mut by_id: BTreeMap<
            String,
            Vec<bitty_runtime::plugin_runtime::package::OrphanPackage>,
        > = BTreeMap::new();
        for orphan in orphans {
            by_id
                .entry(orphan.plugin_id.clone())
                .or_default()
                .push(orphan);
        }
        for (id, mut orphans) in by_id {
            orphans.sort_by(|a, b| a.version.cmp(&b.version));
            let versions = orphans
                .iter()
                .map(|orphan| {
                    format!(
                        "{} ({})",
                        orphan.version,
                        if orphan.complete {
                            "complete"
                        } else {
                            "incomplete"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            // Non-destructive repair path only: `remove --force` deletes the
            // staged tree (including a working version), so it is never a
            // suggested repair for an extra directory.
            let hint = "bitty plugin doctor --repair   # rebuild the index from complete \
                        trees and quarantine incomplete ones"
                .to_string();
            if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
                // The index record (or bundled/recorded state) stays
                // authoritative. Extra unreferenced directories are reported
                // in the description without flipping a healthy row to
                // `orphaned` while it is still enabled.
                row.repair = Some(hint);
                let _ = write!(
                    row.description,
                    " (unreferenced package directories: {versions})"
                );
                continue;
            }
            if seen.contains(&id) {
                continue;
            }
            seen.insert(id.clone());
            rows.push(PluginRow {
                id: id.clone(),
                name: id.clone(),
                version: versions.clone(),
                description: format!(
                    "orphaned package directory with no store index record ({versions}); \
                     complete trees can be re-indexed and incomplete ones quarantined by \
                     `bitty plugin doctor --repair`"
                ),
                bundled: false,
                source: "unknown".to_string(),
                state: "orphaned",
                enabled: false,
                recorded: false,
                pin_ok: None,
                requested: BTreeSet::new(),
                granted: BTreeSet::new(),
                denied: BTreeSet::new(),
                commands: Vec::new(),
                manifest_hash: None,
                settings: Vec::new(),
                repair: Some(hint),
            });
        }
    }

    // Official catalog entries that are neither installed nor recorded are
    // listed as available (or incompatible) so `plugin list` and `info` show
    // the installable set without a network metadata fetch. The catalog is
    // compiled-in data; compatibility is evaluated against the running host.
    if include_official {
        for entry in official_plugins() {
            if seen.contains(entry.id) {
                continue;
            }
            seen.insert(entry.id.to_string());
            rows.push(official_row(entry));
        }
    }

    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

/// Evaluate one official catalog entry against this host.
///
/// Shared by the `list`/`info` row label and the install pre-flight, so a
/// listed entry can never disagree with the install refusal. Returns the
/// structured [`PackageOpError::Incompatible`] carrying the exact declared
/// range and the evaluated host version; no network access is involved.
fn official_entry_compat(entry: &OfficialPlugin) -> Result<(), PackageOpError> {
    package::check_declared_compat(entry.id, entry.compat_bitty, entry.compat_plugin_api)
}

/// One `PluginRow` for a not-yet-installed official catalog entry.
///
/// The catalog is compiled-in data, so compatibility is evaluated against the
/// running host here; an entry outside its declared range is still listed,
/// marked `incompatible`, and annotated with the exact range.
fn official_row(entry: &OfficialPlugin) -> PluginRow {
    let compatible = official_entry_compat(entry).is_ok();
    let description = if compatible {
        entry.description.to_string()
    } else {
        format!(
            "{} [not installable on this host: compat.bitty '{}']",
            entry.description,
            entry.compat_bitty.unwrap_or("-")
        )
    };
    PluginRow {
        id: entry.id.to_string(),
        name: entry.name.to_string(),
        version: "-".to_string(),
        description,
        bundled: false,
        source: "registry".to_string(),
        state: if compatible {
            "available"
        } else {
            "incompatible"
        },
        enabled: false,
        recorded: false,
        pin_ok: None,
        requested: BTreeSet::new(),
        granted: BTreeSet::new(),
        denied: BTreeSet::new(),
        commands: Vec::new(),
        manifest_hash: Some(entry.manifest_hash.to_string()),
        settings: Vec::new(),
        repair: None,
    }
}

/// Load the managed ledger for read-only runtime inspection (issue #2396).
///
/// String error keeps the private CLI failure type out of the inspection
/// module; the message is already host-authored and bounded by the caller.
pub(crate) fn load_state_for_inspect(path: &Path) -> Result<PluginState, String> {
    load_state(path).map_err(|failure| failure.message)
}

/// Build plugin rows for read-only runtime inspection (issue #2396).
pub(crate) fn rows_for_inspect(
    state: &PluginState,
    store_root: Option<&Path>,
) -> Result<Vec<PluginRow>, String> {
    rows_from_state_with_catalog(state, store_root, false).map_err(|failure| failure.message)
}

/// Read the store index, mapping any failure to a read-only plugin error.
fn load_store_index(
    store_root: &Path,
) -> Result<Vec<bitty_runtime::plugin_runtime::PluginRecord>, PluginFailure> {
    load_index(store_root).map_err(|error| {
        PluginFailure::plugin(
            "StoreInvalid",
            format!(
                "bitty plugin: plugin store '{}' is unreadable: {error}; run \
                 `bitty plugin doctor --repair` to rebuild the store index",
                store_root.display()
            ),
        )
    })
}

/// Build a row for one installed (non-bundled) store record.
///
/// The staged manifest body is read best-effort for display only; a record
/// whose body is unreadable still lists, with the recorded grant standing in
/// for the requested set. Loading re-verifies the hashes and fails closed.
fn store_row(store_root: &Path, record: &bitty_runtime::plugin_runtime::PluginRecord) -> PluginRow {
    let root = bitty_runtime::plugin_runtime::resolution::store_package_root(store_root, record);
    let manifest = root.as_deref().and_then(|root| {
        let bytes = read_bounded_regular_file(
            &root.join(package::MANIFEST_FILE_NAME),
            PLUGIN_MANIFEST_MAX_BYTES,
        )
        .ok()
        .flatten()?;
        manifest_toml::parse_manifest(&bytes).ok()
    });
    let broken = manifest.is_none()
        || match (root.as_deref(), manifest.as_ref()) {
            (Some(root), Some(manifest)) => !store_entry_point_exists(root, manifest.id().as_str()),
            _ => true,
        };
    let granted: BTreeSet<CapabilityId> = record
        .granted
        .iter()
        .filter_map(|raw| CapabilityId::parse(raw).ok())
        .collect();
    let requested = manifest
        .as_ref()
        .and_then(|manifest| manifest.capabilities.all_ids().ok())
        .unwrap_or_else(|| granted.clone());
    PluginRow {
        id: record.plugin_id.clone(),
        name: manifest
            .as_ref()
            .map(|manifest| manifest.identity.name.clone())
            .unwrap_or_else(|| record.plugin_id.clone()),
        version: record.version.clone(),
        description: if broken {
            format!(
                "store record points at an unusable package directory ({}); \
                 repair: bitty plugin doctor --repair",
                record.root
            )
        } else {
            manifest.as_ref().map_or_else(
                || format!("installed from a {} source", record.source_class.as_str()),
                |manifest| manifest.identity.description.clone(),
            )
        },
        bundled: false,
        source: record.source_class.as_str().to_string(),
        state: if broken {
            "broken"
        } else if record.enabled {
            "enabled"
        } else {
            "disabled"
        },
        enabled: record.enabled,
        recorded: false,
        pin_ok: None,
        requested,
        granted,
        denied: BTreeSet::new(),
        commands: manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .lazy
                    .commands
                    .iter()
                    .map(|command| command.id.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default(),
        manifest_hash: Some(record.manifest_hash.clone()),
        settings: manifest
            .as_ref()
            .map(|manifest| manifest.settings.clone())
            .unwrap_or_default(),
        repair: broken.then(|| "bitty plugin doctor --repair".to_string()),
    }
}

/// Best-effort read of one staged manifest body (display only, never authority).
///
/// The record root is validated with the same store-escape guard the runtime
/// uses, so a crafted `current.json` can never make a read-only command open a
/// file outside the store.
fn read_store_manifest(
    store_root: &Path,
    record: &bitty_runtime::plugin_runtime::PluginRecord,
) -> Option<PluginManifest> {
    let root = bitty_runtime::plugin_runtime::resolution::store_package_root(store_root, record)?;
    let bytes = read_bounded_regular_file(
        &root.join(package::MANIFEST_FILE_NAME),
        PLUGIN_MANIFEST_MAX_BYTES,
    )
    .ok()
    .flatten()?;
    manifest_toml::parse_manifest(&bytes).ok()
}

/// Whether a staged package root carries its Lua entry point.
///
/// Mirrors the runtime's `entry_point` rule without exposing it across the
/// crate boundary: `lua/init.lua`, `lua/<module>/init.lua`, or the same shapes
/// directly under the package root when there is no `lua/` directory.
fn store_entry_point_exists(root: &Path, id: &str) -> bool {
    let module = id.rsplit('.').next().unwrap_or_default();
    let lua = root.join("lua");
    if lua.is_dir() {
        lua.join("init.lua").is_file() || lua.join(module).join("init.lua").is_file()
    } else {
        root.join("init.lua").is_file() || root.join(module).join("init.lua").is_file()
    }
}

fn row_for_id(
    state: &PluginState,
    store_root: Option<&Path>,
    id: &str,
) -> Result<PluginRow, PluginFailure> {
    rows_from_state(state, store_root)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| {
            PluginFailure::plugin(
                "PluginNotFound",
                format!("bitty plugin: unknown plugin '{id}' (see `bitty plugin list`)"),
            )
        })
}

/// Resolve explain input from an actual manifest, never from a display row.
/// Store and ledger records are evidence only after identity, version, hash,
/// and entry-point checks agree with the runtime resolver.
fn explain_row_for_id(
    state: &PluginState,
    store_root: Option<&Path>,
    id: &str,
) -> Result<PluginRow, PluginFailure> {
    let row = row_for_id(state, store_root, id)?;
    if row.bundled {
        return Ok(row);
    }
    let Some(store_root) = store_root else {
        return Err(PluginFailure::plugin(
            "PluginUnusable",
            format!("bitty plugin: '{id}' has no installed manifest"),
        ));
    };
    let record = load_store_index(store_root)?
        .into_iter()
        .find(|record| record.plugin_id == id)
        .ok_or_else(|| {
            PluginFailure::plugin(
                "PluginUnusable",
                format!("bitty plugin: '{id}' has no installed manifest"),
            )
        })?;
    let package = bitty_runtime::plugin_runtime::resolution::resolve_record(store_root, &record)
        .map_err(|error| {
            PluginFailure::plugin(
                "PluginUnusable",
                format!(
                    "bitty plugin: installed record for '{id}' failed runtime resolution: {error}"
                ),
            )
        })?;
    let requested = package.manifest.capabilities.all_ids().map_err(|error| {
        PluginFailure::plugin(
            "PluginUnusable",
            format!("bitty plugin: installed manifest for '{id}' is invalid: {error}"),
        )
    })?;
    let granted = record
        .granted
        .iter()
        .map(|raw| {
            CapabilityId::parse(raw).map_err(|error| {
                PluginFailure::plugin(
                    "PluginUnusable",
                    format!("bitty plugin: installed grant for '{id}' is invalid: {error}"),
                )
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(PluginRow {
        id: package.manifest.id().as_str().to_string(),
        name: package.manifest.identity.name.clone(),
        version: package.manifest.identity.version.clone(),
        description: package.manifest.identity.description.clone(),
        bundled: false,
        source: record.source_class.as_str().to_string(),
        state: if record.enabled {
            "enabled"
        } else {
            "disabled"
        },
        enabled: record.enabled,
        recorded: false,
        pin_ok: Some(true),
        requested,
        granted,
        denied: BTreeSet::new(),
        commands: package
            .manifest
            .lazy
            .commands
            .iter()
            .map(|command| command.id.as_str().to_string())
            .collect(),
        manifest_hash: Some(record.manifest_hash.clone()),
        settings: package.manifest.settings.clone(),
        repair: None,
    })
}

fn pin_label(pin_ok: Option<bool>) -> &'static str {
    match pin_ok {
        Some(true) => "ok",
        Some(false) => "stale",
        None => "-",
    }
}

/// Whether ANSI emphasis is allowed (unified CLI gate: `--no-color`,
/// `NO_COLOR`, `TERM=dumb`, stdout is a terminal).
fn color_enabled(no_color: bool) -> bool {
    crate::color::cli_color_enabled(no_color)
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
        "{:<32} {:<10} {:<12} {:<6} {:>8}  {}",
        "ID", "SOURCE", "STATE", "PIN", "GRANTED", "VERSION"
    );
    let _ = writeln!(out, "{}", bold(&header, color));
    for row in rows {
        let granted = format!("{}/{}", row.granted.len(), row.requested.len());
        let _ = writeln!(
            out,
            "{:<32} {:<10} {:<12} {:<6} {:>8}  {}",
            row.id,
            row.source,
            row.state,
            pin_label(row.pin_ok),
            granted,
            row.version
        );
    }
    if rows.is_empty() {
        out.push_str("(no plugins)\n");
    }
    // Issue #2439: orphaned/broken entries are shown as rows, and their
    // actionable repair command is repeated here so it is never truncated by
    // the table columns.
    let repairs: Vec<&PluginRow> = rows.iter().filter(|row| row.repair.is_some()).collect();
    if !repairs.is_empty() {
        out.push('\n');
        out.push_str("repair:\n");
        for row in repairs {
            let _ = writeln!(
                out,
                "  {}: {}",
                row.id,
                row.repair
                    .as_deref()
                    .unwrap_or("bitty plugin doctor --repair")
            );
        }
    }
    out
}

fn format_info_table(row: &PluginRow, no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let _ = writeln!(out, "{}", bold(&format!("plugin {}", row.id), color));
    let _ = writeln!(out, "  name:         {}", row.name);
    let _ = writeln!(out, "  version:      {}", row.version);
    let _ = writeln!(out, "  source:       {}", row.source);
    let _ = writeln!(out, "  state:        {}", row.state);
    let _ = writeln!(
        out,
        "  manifest:     {}",
        row.manifest_hash.as_deref().unwrap_or("-")
    );
    let _ = writeln!(out, "  pin:          {}", pin_label(row.pin_ok));
    let _ = writeln!(out, "  description:  {}", row.description);
    if let Some(repair) = &row.repair {
        let _ = writeln!(out, "  repair:       {repair}");
    }
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
        let marker = if capability.denied {
            "revoked"
        } else if capability.granted {
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
    let revoked = row.denied_ids();
    if !revoked.is_empty() {
        let _ = writeln!(
            out,
            "  revoked ({}): {} — re-grant with `bitty plugin install {}`",
            revoked.len(),
            revoked.join(", "),
            row.id
        );
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
            "{{\"id\":\"{}\",\"effect\":\"{}\",\"high_risk\":{},\"requested\":{},\"granted\":{},\"denied\":{}}}",
            json_escape(&capability.id),
            json_escape(&capability.effect),
            capability.high_risk,
            capability.requested,
            capability.granted,
            capability.denied
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
        json_escape(&row.source),
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
    let _ = write!(
        out,
        ",\"repair\":{}",
        match &row.repair {
            Some(repair) => format!("\"{}\"", json_escape(repair)),
            None => "null".to_string(),
        }
    );
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
// `bitty plugin explain` (issue #2471): read-only capability decision chain
// ---------------------------------------------------------------------------

/// Budget limits named in the explanation. Live usage is not observable from
/// a separate CLI process, so the report states limits only (never invents a
/// live counter).
const EXPLAIN_BUDGET_NOTE: &str =
    "limits only; live usage is not observable from a read-only CLI process";

/// Advisory marker for policy layers that are not yet enforced by the host.
const EXPLAIN_POLICY_NOTE: &str = "user/project policy layers are advisory in this build; the running host enforces the \
     task grant and trust gate; a policy-layer denial is fail-closed guidance, not a mutation";

/// Report built for one `bitty plugin explain` invocation.
struct ExplainReport {
    id: String,
    capability: String,
    effect: &'static str,
    high_risk: bool,
    source: String,
    state: &'static str,
    trust: TrustLevel,
    explanation: CapabilityExplanation,
    enforcement: Option<EnforcementEntry>,
    remediation: Vec<String>,
    policy_advisory: bool,
}

/// Load one bounded policy file when present.
///
/// Read-only and fail-closed: symlinks and non-regular files are rejected,
/// at most [`MAX_POLICY_FILE_BYTES`] are read, and malformed syntax is a
/// plugin error naming the file and the bounded parse reason. Parsing stays
/// in [`bitty_plugin_host::effective::parse_policy`]; this function only
/// performs the bounded read.
fn load_policy_scope(
    path: &Path,
    provenance: PolicyProvenance,
) -> Result<Option<CapabilityScope>, PluginFailure> {
    let bytes = match read_bounded_regular_file(path, MAX_POLICY_FILE_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) | Err(BoundedReadError::NotFound) => return Ok(None),
        Err(BoundedReadError::TooLarge) => {
            return Err(PluginFailure::plugin(
                "PolicyTooLarge",
                format!(
                    "bitty plugin: policy file '{}' exceeds {MAX_POLICY_FILE_BYTES} bytes",
                    path.display()
                ),
            ));
        }
        Err(BoundedReadError::Unsafe(reason)) => {
            return Err(PluginFailure::plugin(
                "PolicyUnsafe",
                format!(
                    "bitty plugin: policy file '{}' is unsafe ({reason})",
                    path.display()
                ),
            ));
        }
        Err(BoundedReadError::Io(reason)) => {
            return Err(PluginFailure::generic(
                "PolicyUnreadable",
                format!(
                    "bitty plugin: policy file '{}' could not be read: {reason}",
                    path.display()
                ),
            ));
        }
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        PluginFailure::plugin(
            "PolicyInvalid",
            format!(
                "bitty plugin: policy file '{}' is not valid UTF-8; fix it and re-run",
                path.display()
            ),
        )
    })?;
    parse_policy(text, provenance, &path.display().to_string())
        .map(Some)
        .map_err(|denial| {
            PluginFailure::plugin(
                "PolicyInvalid",
                format!(
                    "bitty plugin: policy file '{}' is invalid: {}; fix it and re-run",
                    path.display(),
                    denial.reason_chain().join("; ")
                ),
            )
        })
}

/// Build the read-only explanation report for one declared capability.
fn build_explain_report(
    row: &PluginRow,
    raw_capability: &str,
    context: &PluginContext<'_>,
) -> Result<ExplainReport, PluginFailure> {
    if matches!(row.state, "broken" | "orphaned") {
        return Err(PluginFailure::plugin(
            "PluginUnusable",
            format!(
                "bitty plugin: '{}' is {} ({}); repair it with `bitty plugin doctor --repair` \
                 or reinstall before explaining capabilities",
                row.id, row.state, row.description
            ),
        ));
    }
    let capability = CapabilityId::parse(raw_capability).map_err(|error| {
        PluginFailure::usage(format!(
            "bitty plugin: invalid capability '{raw_capability}': {error}"
        ))
    })?;
    if !row.requested.contains(&capability) {
        if row.granted.contains(&capability) {
            return Err(PluginFailure::plugin(
                "CapabilityNotDeclared",
                format!(
                    "bitty plugin: '{capability}' is recorded as granted for '{}' but the \
                     manifest no longer declares it (stale grant); re-install to re-consent \
                     with `bitty plugin install {} --yes`",
                    row.id, row.id
                ),
            ));
        }
        let declared = if row.requested.is_empty() {
            "(none)".to_string()
        } else {
            row.requested
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(PluginFailure::plugin(
            "UnknownCapability",
            format!(
                "bitty plugin: '{}' does not declare capability '{raw_capability}'; \
                 declared capabilities: {declared}",
                row.id
            ),
        ));
    }

    let user_path = user_policy_path_with_env(context.xdg_config_home, context.home);
    let user_scope = match user_path.as_deref() {
        Some(path) => load_policy_scope(Path::new(path), PolicyProvenance::User)?,
        None => None,
    };
    let project_path = context.cwd.map(project_policy_path);
    let project_scope = match project_path.as_deref() {
        Some(path) => load_policy_scope(Path::new(path), PolicyProvenance::Project)?,
        None => None,
    };

    let stale_pin = row.pin_ok == Some(false);
    let hash_hint: String = row
        .manifest_hash
        .as_deref()
        .map(|hash| hash.chars().take(12).collect())
        .unwrap_or_else(|| "unrecorded".to_string());
    let mut task = CapabilityScope::unconstrained(format!("task-grant:{}@{hash_hint}", row.id));
    task.closed = true;
    if !stale_pin {
        task.caps = row.granted.clone();
        task.denied = row.denied.clone();
    }
    let stack = EffectiveStack {
        host: CapabilityScope::unconstrained("host-ceiling:built-in"),
        user: user_scope
            .clone()
            .unwrap_or_else(|| CapabilityScope::unconstrained("user:absent")),
        project: project_scope.clone(),
        parent: CapabilityScope::unconstrained("parent-delegation:not-applicable"),
        task,
    };
    let mut request_scope =
        CapabilityScope::unconstrained(format!("agent-request:{} manifest", row.id));
    request_scope.caps.insert(capability.clone());
    let request = AgentRequest {
        scope: request_scope,
        raw_wide: Vec::new(),
    };
    let trust = if row.bundled {
        TrustLevel::BundledLua
    } else {
        TrustLevel::ThirdPartyLua
    };
    let mut explanation = explain_capability(
        &stack,
        &request,
        RequestKind::PluginLifecycle,
        true,
        Some(trust),
        &capability,
    )
    .map_err(|denial| {
        PluginFailure::plugin(
            "ExplanationInvalid",
            format!(
                "bitty plugin: could not explain '{}' for '{}': {}",
                capability.as_str(),
                row.id,
                denial.reason_chain().join("; ")
            ),
        )
    })?;

    if stale_pin {
        // A grant bound to a different manifest hash never contributes
        // authority. Surface it as the conflict it is (PolicyConflict), not as
        // a generic excess.
        if let Some(task_row) = explanation
            .layers
            .iter_mut()
            .find(|layer| layer.layer == EffectiveLayer::TaskGrant)
        {
            task_row.state = LayerState::Deny;
            task_row.blocks = true;
            task_row.detail = "grant bound to a different manifest hash".to_string();
        }
        let stale_reason = format!(
            "task-grant: manifest hash pin mismatch (recorded {}, current manifest differs); \
             re-install with `bitty plugin install {} --yes` to re-consent",
            row.manifest_hash.as_deref().unwrap_or("unrecorded"),
            row.id
        );
        let prior_first_layer = explanation.first_denying_layer;
        explanation.allowed = false;
        if prior_first_layer.is_none() {
            explanation.first_denying_layer = Some(EffectiveLayer::TaskGrant);
            explanation.denial_kind = Some(DenialKind::PolicyConflict);
        } else if prior_first_layer == Some(EffectiveLayer::TaskGrant) {
            // The empty task scope above is only the stale-pin guard. Report
            // its typed conflict rather than the generic missing-grant denial.
            explanation.denial_kind = Some(DenialKind::PolicyConflict);
        }
        explanation.reason_chain.push(stale_reason);
        explanation.effective.caps.clear();
    }

    let enforcement = ENFORCEMENT_MAP
        .iter()
        .copied()
        .find(|entry| entry.kind == RequestKind::PluginLifecycle);
    // Name the policy file a denial came from (or say it is absent) without
    // changing the decision. Paths are environment-derived, never literals.
    for layer in &mut explanation.layers {
        match layer.layer {
            EffectiveLayer::UserPolicy if user_scope.is_none() => {
                layer.detail = match user_path.as_deref() {
                    Some(path) => format!("file not present: {path}"),
                    None => "no config root (set XDG_CONFIG_HOME or HOME)".to_string(),
                };
            }
            EffectiveLayer::ProjectPolicy if project_scope.is_none() => {
                layer.detail = match project_path.as_deref() {
                    Some(path) => format!("file not present: {path}"),
                    None => "current directory unavailable".to_string(),
                };
            }
            _ => {}
        }
    }
    let remediation = explain_remediation(
        row,
        &capability,
        trust,
        &explanation,
        user_path.as_deref(),
        project_path.as_deref(),
        stale_pin,
    );
    let policy_advisory = user_scope.is_some() || project_scope.is_some();

    Ok(ExplainReport {
        id: row.id.clone(),
        capability: capability.as_str().to_string(),
        effect: effect_statement(&capability),
        high_risk: capability.is_high_risk(),
        source: row.source.clone(),
        state: row.state,
        trust,
        explanation,
        enforcement,
        remediation,
        policy_advisory,
    })
}

/// Minimal, non-applying remediation hints for a denied explanation.
fn explain_remediation(
    row: &PluginRow,
    capability: &CapabilityId,
    trust: TrustLevel,
    explanation: &CapabilityExplanation,
    user_path: Option<&str>,
    project_path: Option<&str>,
    stale_pin: bool,
) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    if stale_pin && explanation.first_denying_layer == Some(EffectiveLayer::TaskGrant) {
        hints.push(format!(
            "Re-consent to the current manifest: bitty plugin install {} --yes",
            row.id
        ));
    } else if !explanation.allowed {
        match explanation.first_denying_layer {
            Some(EffectiveLayer::TaskGrant) => {
                if row.denied.contains(capability) {
                    hints.push(format!(
                        "Re-grant the revoked capability: bitty plugin install {} --yes \
                         (revocation is durable until explicit re-consent)",
                        row.id
                    ));
                } else {
                    hints.push(format!(
                        "Grant the declared capability: bitty plugin install {} --yes",
                        row.id
                    ));
                }
            }
            Some(EffectiveLayer::UserPolicy) => {
                if let Some(path) = user_path {
                    hints.push(format!(
                        "Add `allow {}` to '{path}' (user policy layer; advisory in this build)",
                        capability.as_str()
                    ));
                }
            }
            Some(EffectiveLayer::ProjectPolicy) => {
                if let Some(path) = project_path {
                    hints.push(format!(
                        "Add `allow {}` to '{path}' (project policy layer; advisory in this build)",
                        capability.as_str()
                    ));
                }
            }
            Some(EffectiveLayer::HostCeiling) => {
                hints.push(format!(
                    "Host admission denies '{capability}' for trust level '{}'; \
                     no local override exists",
                    trust.as_str()
                ));
            }
            Some(EffectiveLayer::ParentDelegation) => hints.push(
                "No parent delegation applies to a plugin invocation; inspect the delegating \
                 agent's grant instead"
                    .to_string(),
            ),
            Some(EffectiveLayer::AgentRequest) | None => {}
        }
    }
    if !row.enabled && row.state != "available" {
        hints.push(format!(
            "Enable the plugin after the grant is present: bitty plugin enable {}",
            row.id
        ));
    }
    hints.push(format!(
        "Inspect the full record: bitty plugin info {}",
        row.id
    ));
    hints.truncate(4);
    hints
}

/// Render the explain report as a bounded human table.
fn table_cell(raw: &str) -> String {
    const MAX_TABLE_CELL_CHARS: usize = 256;
    let mut result = String::new();
    for character in raw.chars().take(MAX_TABLE_CELL_CHARS) {
        match character {
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character if character.is_control() => {
                let _ = write!(result, "\\u{{{:x}}}", character as u32);
            }
            character => result.push(character),
        }
    }
    if raw.chars().count() > MAX_TABLE_CELL_CHARS {
        result.push_str("...");
    }
    result
}

fn format_explain_table(report: &ExplainReport, no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        bold(&format!("plugin {}", table_cell(&report.id)), color)
    );
    let risk = if report.high_risk { " (high risk)" } else { "" };
    let _ = writeln!(
        out,
        "  capability:   {}{risk} — {}",
        table_cell(&report.capability),
        table_cell(report.effect)
    );
    let _ = writeln!(out, "  source:       {}", table_cell(&report.source));
    let _ = writeln!(out, "  state:        {}", report.state);
    let _ = writeln!(
        out,
        "  trust:        {} (level {})",
        report.trust.as_str(),
        report.trust.level_number()
    );
    if let Some(enforcement) = report.enforcement {
        let _ = writeln!(
            out,
            "  enforcement:  {} ({}) — {}",
            enforcement.kind.as_str(),
            enforcement.class.as_str(),
            table_cell(enforcement.owner)
        );
    }
    let _ = writeln!(
        out,
        "  decision:     {}",
        if report.explanation.allowed {
            "allow"
        } else {
            "deny"
        }
    );
    let _ = writeln!(
        out,
        "  first denying layer: {}",
        report
            .explanation
            .first_denying_layer
            .map_or("none", EffectiveLayer::label)
    );
    let _ = writeln!(
        out,
        "  denial kind:  {}",
        report
            .explanation
            .denial_kind
            .map_or("none", DenialKind::as_str)
    );
    let _ = writeln!(
        out,
        "  effective:    {} ({} of 1 requested capability)",
        if report.explanation.allowed {
            "allow"
        } else {
            "deny"
        },
        report.explanation.effective.caps.len()
    );
    let _ = writeln!(
        out,
        "  pins:         {}",
        if report.explanation.pins.is_empty() {
            "none".to_string()
        } else {
            report
                .explanation
                .pins
                .iter()
                .map(|pin| table_cell(pin))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    let _ = writeln!(
        out,
        "  budget:       {EXPLAIN_BUDGET_NOTE} (instructions={}, wall_clock_ms={}, \
         memory_bytes={}, queued_events={}, queued_bytes={}, file_descriptors={})",
        bitty_plugin_host::event::RC1_INSTRUCTION_BUDGET,
        bitty_plugin_host::event::RC1_WALL_CLOCK_BUDGET_MS,
        bitty_plugin_host::event::RC2_MEMORY_PER_PLUGIN_BYTES,
        bitty_plugin_host::event::PER_PLUGIN_QUEUED_EVENT_LIMIT,
        bitty_plugin_host::event::PER_PLUGIN_QUEUED_BYTES_LIMIT,
        bitty_plugin_host::event::RC6_FD_PER_PLUGIN,
    );
    out.push_str("  layers (highest authority first):\n");
    for layer in &report.explanation.layers {
        let _ = writeln!(
            out,
            "    {:<18} {:<6} {} — {}",
            layer.layer.label(),
            layer.state.as_str(),
            table_cell(&layer.source),
            table_cell(&layer.detail)
        );
    }
    if !report.explanation.reason_chain.is_empty() {
        out.push_str("  reason chain:\n");
        for step in &report.explanation.reason_chain {
            let _ = writeln!(out, "    - {}", table_cell(step));
        }
    }
    if report.policy_advisory {
        let _ = writeln!(out, "  note:         {EXPLAIN_POLICY_NOTE}");
    }
    if !report.remediation.is_empty() {
        out.push_str("  remediation:\n");
        for hint in &report.remediation {
            let _ = writeln!(out, "    - {}", table_cell(hint));
        }
    }
    out
}

/// Render the explain report as a versioned JSON envelope.
fn format_explain_envelope(report: &ExplainReport) -> String {
    let explanation = &report.explanation;
    let mut out = String::from(
        "{\"v\":1,\"command\":\"plugin\",\"ok\":true,\"result\":{\"verb\":\"explain\",",
    );
    let _ = write!(
        out,
        "\"plugin\":\"{}\",\"capability\":\"{}\",\"effect\":\"{}\",\"high_risk\":{},\
         \"source\":\"{}\",\"state\":\"{}\",",
        json_escape(&report.id),
        json_escape(&report.capability),
        json_escape(report.effect),
        report.high_risk,
        json_escape(&report.source),
        report.state
    );
    let _ = write!(
        out,
        "\"request_kind\":\"{}\",\"decision\":\"{}\",",
        explanation.request_kind.as_str(),
        if explanation.allowed { "allow" } else { "deny" }
    );
    let _ = write!(
        out,
        "\"first_denying_layer\":{},\"denial_kind\":{},",
        match explanation.first_denying_layer {
            Some(layer) => format!("\"{}\"", layer.label()),
            None => "null".to_string(),
        },
        match explanation.denial_kind {
            Some(kind) => format!("\"{}\"", kind.as_str()),
            None => "null".to_string(),
        }
    );
    let _ = write!(
        out,
        "\"trust\":{{\"level\":\"{}\",\"number\":{},\"family\":\"{}\",\"admits_family\":{}}},",
        report.trust.as_str(),
        report.trust.level_number(),
        explanation.capability.family().as_str(),
        report
            .trust
            .check_family(explanation.capability.family())
            .is_ok()
    );
    out.push_str("\"layers\":[");
    for (index, layer) in explanation.layers.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"layer\":\"{}\",\"config_layer\":\"{}\",\"state\":\"{}\",\
             \"constrains\":{},\"blocks\":{},\"source\":\"{}\",\"detail\":\"{}\"}}",
            layer.layer.label(),
            layer.layer.config_layer(),
            layer.state.as_str(),
            layer.constrains,
            layer.blocks,
            json_escape(&layer.source),
            json_escape(&layer.detail)
        );
    }
    out.push_str("],\"reason_chain\":[");
    for (index, step) in explanation.reason_chain.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(step));
    }
    out.push_str("],\"pins\":[");
    for (index, pin) in explanation.pins.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(pin));
    }
    out.push_str("],\"effective\":{");
    let _ = write!(out, "\"allowed\":{},\"caps\":[", explanation.allowed);
    for (index, cap) in explanation.effective.caps.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(cap.as_str()));
    }
    let _ = write!(
        out,
        "],\"max_agents\":{},\"allow_root\":{}}}",
        explanation.effective.max_agents, explanation.effective.allow_root
    );
    out.push_str(",\"budget\":{\"observable\":false");
    let _ = write!(out, ",\"note\":\"{}\"", json_escape(EXPLAIN_BUDGET_NOTE));
    let _ = write!(
        out,
        ",\"limits\":{{\"instructions\":{},\"wall_clock_ms\":{},\"memory_bytes\":{},\
         \"queued_events\":{},\"queued_bytes\":{},\"file_descriptors\":{}}}}}",
        bitty_plugin_host::event::RC1_INSTRUCTION_BUDGET,
        bitty_plugin_host::event::RC1_WALL_CLOCK_BUDGET_MS,
        bitty_plugin_host::event::RC2_MEMORY_PER_PLUGIN_BYTES,
        bitty_plugin_host::event::PER_PLUGIN_QUEUED_EVENT_LIMIT,
        bitty_plugin_host::event::PER_PLUGIN_QUEUED_BYTES_LIMIT,
        bitty_plugin_host::event::RC6_FD_PER_PLUGIN,
    );
    let _ = write!(
        out,
        ",\"enforcement\":{}",
        match report.enforcement {
            Some(enforcement) => format!(
                "{{\"request_kind\":\"{}\",\"class\":\"{}\",\"owner\":\"{}\",\"note\":\"{}\"}}",
                enforcement.kind.as_str(),
                enforcement.class.as_str(),
                json_escape(enforcement.owner),
                json_escape(enforcement.note)
            ),
            None => "null".to_string(),
        }
    );
    out.push_str(",\"remediation\":[");
    for (index, hint) in report.remediation.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(hint));
    }
    let _ = write!(
        out,
        "],\"policy_advisory\":{},\"policy_note\":{}}}}}",
        report.policy_advisory,
        if report.policy_advisory {
            format!("\"{}\"", json_escape(EXPLAIN_POLICY_NOTE))
        } else {
            "null".to_string()
        }
    );
    out
}

fn finish_explain(
    report: &ExplainReport,
    format: PluginFormat,
    no_color: bool,
    output: &mut dyn std::io::Write,
) -> i32 {
    match format {
        PluginFormat::Table => {
            let _ = output.write_all(format_explain_table(report, no_color).as_bytes());
        }
        PluginFormat::Json | PluginFormat::Jsonl => {
            let _ = writeln!(output, "{}", format_explain_envelope(report));
        }
    }
    if report.explanation.allowed {
        EXIT_OK
    } else {
        eprintln!(
            "bitty plugin: capability '{}' denied for '{}' by {} ({}); \
             see the decision chain above",
            report.capability,
            report.id,
            report
                .explanation
                .first_denying_layer
                .map_or("an unknown layer", EffectiveLayer::label),
            report
                .explanation
                .denial_kind
                .map_or("unknown", DenialKind::as_str)
        );
        EXIT_PLUGIN
    }
}

// ---------------------------------------------------------------------------
// Per-plugin config inspection and scaffolding (`bitty plugin config`, #2246)
// ---------------------------------------------------------------------------

/// One declared setting in a `config show` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSettingRow {
    /// Declared key.
    pub key: String,
    /// Declared type spelling (`bool`, `int`, `string`, `string-array`).
    pub kind: &'static str,
    /// Rendered shipped manifest default.
    pub default: String,
    /// Declared description, when the manifest schema carries one.
    ///
    /// v1 `[settings]` declarations have no description field, so this is
    /// `None`; the table renders `-` and JSON renders `null`. No copy is
    /// invented for it.
    pub description: Option<String>,
    /// Rendered effective value after resolution.
    pub value: String,
    /// Layer that served `value`.
    pub layer: SettingLayer,
}

/// Read-only inspection result for one plugin's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfigReport {
    /// Plugin id.
    pub id: String,
    /// Environment-derived config root (parent of `bitty-plugins.toml`).
    pub config_dir: PathBuf,
    /// Candidate user-file paths.
    pub paths: PluginConfigPaths,
    /// File selected by resolution, when one was read.
    pub selected: Option<SelectedPluginFile>,
    /// Whether the canonical Lua file exists (symlinks count as present but
    /// are rejected by the loader).
    pub canonical_exists: bool,
    /// Whether the unambiguous short-name alias exists.
    pub alias_exists: bool,
    /// Whether the legacy TOML fallback exists.
    pub legacy_exists: bool,
    /// One row per declared setting, sorted by key.
    pub rows: Vec<ConfigSettingRow>,
    /// File keys that changed nothing (undeclared, wrong-typed, skipped).
    pub dropped: Vec<String>,
    /// Whole-file rejection diagnostic, if any.
    pub file_error: Option<String>,
}

/// Render a resolved/declared plain value as a single bounded token.
fn render_lua_value(value: &bitty_runtime::plugin_runtime::LuaValue) -> String {
    use bitty_runtime::plugin_runtime::LuaValue;
    match value {
        LuaValue::Nil => "nil".to_string(),
        LuaValue::Bool(flag) => flag.to_string(),
        LuaValue::Integer(number) => number.to_string(),
        LuaValue::Number(number) => number.to_string(),
        LuaValue::String(text) => format!("\"{}\"", escape_toml_basic(text)),
        LuaValue::Table(pairs) => {
            let items: Vec<String> = pairs
                .iter()
                .map(|(_, item)| render_lua_value(item))
                .collect();
            format!("[{}]", items.join(", "))
        }
    }
}

/// Bound one table cell so a hostile default cannot flood the terminal.
fn bounded_cell(text: &str) -> String {
    if text.chars().count() <= CONFIG_TABLE_CELL_BYTES {
        return text.to_string();
    }
    let mut out: String = text.chars().take(CONFIG_TABLE_CELL_BYTES).collect();
    out.push_str("...");
    out
}

fn exists_no_follow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Build the inspection report (no plugin VM; the bounded plain settings VM
/// resolves the selected user file exactly as activation does).
fn build_config_report(
    state: &PluginState,
    store_root: Option<&Path>,
    config_dir: &Path,
    id: &str,
    manifest: &PluginManifest,
) -> PluginConfigReport {
    let discovered = known_plugin_ids(state, store_root);
    let paths = plugin_settings::plugin_config_paths(config_dir, id, Some(&discovered))
        .expect("plugin id validated before report construction");
    let loaded: LoadedPluginSettings = plugin_settings::load_plugin_settings_with_plugins(
        Some(config_dir),
        id,
        &manifest.settings,
        None,
        Some(&discovered),
    );
    let mut rows: Vec<ConfigSettingRow> = manifest
        .settings
        .iter()
        .map(|decl| ConfigSettingRow {
            key: decl.key.clone(),
            kind: decl.kind.as_str(),
            default: render_lua_value(&plugin_settings::default_to_lua(&decl.default)),
            description: None,
            value: loaded
                .source
                .values()
                .get(&decl.key)
                .map_or_else(|| "-".to_string(), render_lua_value),
            layer: loaded
                .layers
                .get(&decl.key)
                .copied()
                .unwrap_or(SettingLayer::ManifestDefault),
        })
        .collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    PluginConfigReport {
        id: id.to_string(),
        config_dir: config_dir.to_path_buf(),
        canonical_exists: exists_no_follow(&paths.canonical_lua),
        alias_exists: paths.alias_lua.as_deref().is_some_and(exists_no_follow),
        legacy_exists: exists_no_follow(&paths.legacy_toml),
        selected: loaded.selected_file,
        paths,
        rows,
        dropped: loaded.dropped,
        file_error: loaded.file_error,
    }
}

fn presence(exists: bool) -> &'static str {
    if exists { "present" } else { "absent" }
}

/// Human table for `config show`.
fn format_config_table(report: &PluginConfigReport, no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let _ = writeln!(out, "{}", bold(&format!("plugin {}", report.id), color));
    let _ = writeln!(out, "  config dir:     {}", report.config_dir.display());
    let _ = writeln!(
        out,
        "  canonical lua:  {} ({})",
        report.paths.canonical_lua.display(),
        presence(report.canonical_exists)
    );
    match &report.paths.alias_lua {
        Some(alias) => {
            let _ = writeln!(
                out,
                "  alias lua:      {} ({})",
                alias.display(),
                presence(report.alias_exists)
            );
        }
        None => {
            let _ = writeln!(out, "  alias lua:      - (short name not unambiguous)");
        }
    }
    let _ = writeln!(
        out,
        "  legacy toml:    {} ({})",
        report.paths.legacy_toml.display(),
        presence(report.legacy_exists)
    );
    match &report.selected {
        Some(selected) => {
            let _ = writeln!(
                out,
                "  selected:       {} ({})",
                selected.path.display(),
                selected.kind.as_str()
            );
        }
        None => {
            let _ = writeln!(out, "  selected:       - (manifest defaults only)");
        }
    }
    if report
        .selected
        .as_ref()
        .is_some_and(|selected| selected.kind == PluginConfigKind::LegacyToml)
    {
        out.push_str(
            "  note:           legacy TOML is the active source; the canonical surface is \
             <owner>.<name>.lua\n",
        );
    }
    out.push('\n');
    let header = format!(
        "{:<24} {:<12} {:<20} {:<20} {:<16} {}",
        "KEY", "TYPE", "DEFAULT", "VALUE", "SOURCE", "DESCRIPTION"
    );
    let _ = writeln!(out, "{}", bold(&header, color));
    if report.rows.is_empty() {
        out.push_str("(no settings declared)\n");
    }
    for row in &report.rows {
        let _ = writeln!(
            out,
            "{:<24} {:<12} {:<20} {:<20} {:<16} {}",
            bounded_cell(&row.key),
            row.kind,
            bounded_cell(&row.default),
            bounded_cell(&row.value),
            row.layer.as_str(),
            row.description.as_deref().unwrap_or("-")
        );
    }
    if !report.dropped.is_empty() {
        let _ = writeln!(
            out,
            "\n  dropped (undeclared or wrong-typed) keys: {}",
            report.dropped.join(", ")
        );
    }
    if let Some(error) = &report.file_error {
        let _ = writeln!(out, "\n  file error: {error}");
    }
    out
}

/// Versioned JSON envelope for `config show`.
#[must_use]
pub fn format_config_envelope(report: &PluginConfigReport) -> String {
    let mut out = String::from(
        "{\"v\":1,\"command\":\"plugin\",\"ok\":true,\"result\":{\"verb\":\"config\",\"action\":\"show\",",
    );
    let _ = write!(out, "\"plugin\":\"{}\",", json_escape(&report.id));
    let _ = write!(
        out,
        "\"config_dir\":\"{}\",",
        json_escape(&report.config_dir.display().to_string())
    );
    let _ = write!(
        out,
        "\"candidates\":{{\"canonical_lua\":\"{}\",\"canonical_exists\":{},\"alias_lua\":{},\"alias_exists\":{},\"legacy_toml\":\"{}\",\"legacy_exists\":{}}},",
        json_escape(&report.paths.canonical_lua.display().to_string()),
        report.canonical_exists,
        match &report.paths.alias_lua {
            Some(path) => format!("\"{}\"", json_escape(&path.display().to_string())),
            None => "null".to_string(),
        },
        report.alias_exists,
        json_escape(&report.paths.legacy_toml.display().to_string()),
        report.legacy_exists
    );
    out.push_str("\"selected\":");
    match &report.selected {
        Some(selected) => {
            let _ = write!(
                out,
                "{{\"path\":\"{}\",\"kind\":\"{}\"}}",
                json_escape(&selected.path.display().to_string()),
                selected.kind.as_str()
            );
        }
        None => out.push_str("null"),
    }
    out.push_str(",\"settings\":[");
    for (index, row) in report.rows.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"key\":\"{}\",\"type\":\"{}\",\"default\":{},\"description\":{},\"value\":{},\"source\":\"{}\"}}",
            json_escape(&row.key),
            row.kind,
            row.default,
            match &row.description {
                Some(description) => format!("\"{}\"", json_escape(description)),
                None => "null".to_string(),
            },
            row.value,
            row.layer.as_str()
        );
    }
    out.push_str("],\"dropped\":[");
    for (index, key) in report.dropped.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(key));
    }
    out.push_str("],\"file_error\":");
    match &report.file_error {
        Some(error) => {
            let _ = write!(out, "\"{}\"", json_escape(error));
        }
        None => out.push_str("null"),
    }
    out.push_str("}}");
    out
}

/// Escape one string for a Lua short string literal in scaffold templates.
///
/// Manifest string defaults are bounded but attacker-controlled; control
/// characters must never reach the file raw. Multi-byte UTF-8 passes through;
/// C0/C1 controls and DEL become decimal escapes, which every Lua version
/// accepts and which reproduce the exact original bytes.
fn escape_lua_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                for byte in c.to_string().as_bytes() {
                    let _ = write!(out, "\\{byte:03}");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Render the canonical Lua settings template from manifest declarations only.
fn render_config_scaffold(id: &str, settings: &[SettingDecl]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "-- Per-plugin settings for {id}.");
    out.push_str("-- Generated by `bitty plugin config scaffold`; edit values freely.\n");
    out.push_str(
        "-- Values are data only: no setting grants capability, authority, or plugin code.\n",
    );
    out.push_str("-- Settings load on the next activation of this plugin.\n");
    out.push_str("--\n");
    out.push_str("-- Declared keys and shipped defaults:\n");
    for decl in settings {
        let _ = writeln!(
            out,
            "--   {}: {} = {}",
            decl.key,
            decl.kind.as_str(),
            render_scaffold_default(&decl.default)
        );
    }
    out.push_str("--\n");
    out.push_str("-- Delete a line to keep its shipped default.\n");
    out.push_str("return {\n");
    for decl in settings {
        let _ = writeln!(
            out,
            "    {} = {},",
            decl.key,
            render_scaffold_default(&decl.default)
        );
    }
    out.push_str("}\n");
    out
}

/// Render one default as a Lua literal (`true`, `32`, `"x"`, `{ "a" }`).
fn render_scaffold_default(default: &SettingDefault) -> String {
    match default {
        SettingDefault::Bool(value) => value.to_string(),
        SettingDefault::Int(value) => value.to_string(),
        SettingDefault::String(value) => format!("\"{}\"", escape_lua_string(value)),
        SettingDefault::Strings(items) => {
            let quoted: Vec<String> = items
                .iter()
                .map(|item| format!("\"{}\"", escape_lua_string(item)))
                .collect();
            format!("{{ {} }}", quoted.join(", "))
        }
    }
}

/// Every statically known plugin id (bundled catalog plus installed store).
fn known_plugin_ids(state: &PluginState, store_root: Option<&Path>) -> Vec<String> {
    let mut ids: BTreeSet<String> = all_bundled_manifests()
        .iter()
        .map(|manifest| manifest.id().as_str().to_string())
        .collect();
    ids.extend(state.ids().cloned());
    if let Some(root) = store_root {
        if let Ok(records) = load_index(root) {
            ids.extend(records.into_iter().map(|record| record.plugin_id));
        }
    }
    ids.into_iter().collect()
}

fn unknown_plugin_failure(id: &str) -> PluginFailure {
    PluginFailure::plugin(
        "UnknownPlugin",
        format!("bitty plugin: unknown plugin '{id}' (see `bitty plugin list`)"),
    )
}

/// Resolve the static manifest for any known plugin, failing closed on an
/// unknown id or an unreadable stored manifest body.
fn config_manifest_for_id(
    store_root: Option<&Path>,
    id: &str,
) -> Result<PluginManifest, PluginFailure> {
    if let Some(manifest) = bundled_manifest_for(id) {
        manifest.validate().map_err(|error| {
            PluginFailure::plugin(
                "SchemaUnreadable",
                format!("bitty plugin: bundled settings schema for '{id}' is invalid: {error}"),
            )
        })?;
        return Ok(manifest);
    }
    let Some(root) = store_root else {
        return Err(unknown_plugin_failure(id));
    };
    let records = load_store_index(root)?;
    let Some(record) = records.iter().find(|record| record.plugin_id == id) else {
        return Err(unknown_plugin_failure(id));
    };
    let Some(package_root) =
        bitty_runtime::plugin_runtime::resolution::store_package_root(root, record)
    else {
        return Err(PluginFailure::plugin(
            "SchemaUnreadable",
            format!("bitty plugin: package root for '{id}' is not a safe store path"),
        ));
    };
    let path = package_root.join(package::MANIFEST_FILE_NAME);
    let bytes = std::fs::read(&path).map_err(|error| {
        PluginFailure::plugin(
            "SchemaUnreadable",
            format!(
                "bitty plugin: cannot read the manifest for '{id}' at '{}': {error}",
                path.display()
            ),
        )
    })?;
    manifest_toml::parse_manifest(&bytes).map_err(|error| {
        PluginFailure::plugin(
            "SchemaUnreadable",
            format!("bitty plugin: settings schema for '{id}' is unreadable: {error}"),
        )
    })
}

/// Create or verify `<config-dir>/plugins` without ever following a symlink
/// out of the user config root.
fn ensure_plugins_dir(config_dir: &Path) -> Result<PathBuf, PluginFailure> {
    std::fs::create_dir_all(config_dir).map_err(|error| {
        PluginFailure::plugin(
            "IoError",
            format!(
                "bitty plugin: cannot create config dir '{}': {error}",
                config_dir.display()
            ),
        )
    })?;
    let root = std::fs::canonicalize(config_dir).map_err(|error| {
        PluginFailure::plugin(
            "IoError",
            format!(
                "bitty plugin: cannot resolve config dir '{}': {error}",
                config_dir.display()
            ),
        )
    })?;
    let plugins = config_dir.join(PLUGINS_DIR_NAME);
    if let Ok(metadata) = std::fs::symlink_metadata(&plugins) {
        if metadata.file_type().is_symlink() {
            return Err(PluginFailure::plugin(
                "SymlinkTarget",
                format!(
                    "bitty plugin: refusing to write through symlinked plugins dir '{}'",
                    plugins.display()
                ),
            ));
        }
    }
    std::fs::create_dir_all(&plugins).map_err(|error| {
        PluginFailure::plugin(
            "IoError",
            format!(
                "bitty plugin: cannot create plugins dir '{}': {error}",
                plugins.display()
            ),
        )
    })?;
    let canonical = std::fs::canonicalize(&plugins).map_err(|error| {
        PluginFailure::plugin(
            "IoError",
            format!(
                "bitty plugin: cannot resolve plugins dir '{}': {error}",
                plugins.display()
            ),
        )
    })?;
    if !canonical.starts_with(&root) {
        return Err(PluginFailure::plugin(
            "UnsafePath",
            format!(
                "bitty plugin: plugins dir '{}' resolves outside the config root '{}'",
                canonical.display(),
                root.display()
            ),
        ));
    }
    Ok(plugins)
}

/// Write the scaffold atomically (temp + no-clobber commit or rename).
///
/// `--force` replaces an existing regular file via an atomic rename. Without
/// `--force` the commit uses a hard link, which fails atomically if the target
/// appeared after the pre-check. Symlinked targets and directories are always
/// refused, even with `--force`.
fn write_config_scaffold(
    config_dir: &Path,
    id: &str,
    settings: &[SettingDecl],
    force: bool,
) -> Result<PathBuf, PluginFailure> {
    if settings.is_empty() {
        return Err(PluginFailure::plugin(
            "NoSettings",
            format!("bitty plugin: '{id}' declares no [settings]; nothing to scaffold"),
        ));
    }
    let paths = plugin_settings::plugin_config_paths(config_dir, id, None).ok_or_else(|| {
        PluginFailure::plugin(
            "UnsafePath",
            format!("bitty plugin: '{id}' cannot name a per-plugin config file"),
        )
    })?;
    let target = paths.canonical_lua;
    let plugins = ensure_plugins_dir(config_dir)?;
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(PluginFailure::plugin(
                "SymlinkTarget",
                format!(
                    "bitty plugin: refusing to write '{}': it is a symlink",
                    target.display()
                ),
            ));
        }
        Ok(metadata) if metadata.is_dir() => {
            return Err(PluginFailure::plugin(
                "InvalidTarget",
                format!(
                    "bitty plugin: refusing to write '{}': it is a directory",
                    target.display()
                ),
            ));
        }
        Ok(_) if !force => {
            return Err(PluginFailure::plugin(
                "AlreadyExists",
                format!(
                    "bitty plugin: '{}' already exists; pass --force to overwrite",
                    target.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(PluginFailure::plugin(
                "IoError",
                format!(
                    "bitty plugin: cannot inspect '{}': {error}",
                    target.display()
                ),
            ));
        }
    }

    let rendered = render_config_scaffold(id, settings);
    if rendered.len() > MAX_SCAFFOLD_BYTES {
        return Err(PluginFailure::plugin(
            "ScaffoldTooLarge",
            format!("bitty plugin: rendered scaffold exceeds {MAX_SCAFFOLD_BYTES} bytes"),
        ));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp = plugins.join(format!(
        "{SCAFFOLD_TEMP_PREFIX}{}-{nonce}.tmp",
        std::process::id()
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::io::Write::write_all(&mut file, rendered.as_bytes())?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(PluginFailure::plugin(
            "IoError",
            format!(
                "bitty plugin: cannot write scaffold temp file '{}': {error}",
                temp.display()
            ),
        ));
    }
    let commit = if force {
        std::fs::rename(&temp, &target)
    } else {
        std::fs::hard_link(&temp, &target).and_then(|()| std::fs::remove_file(&temp))
    };
    if let Err(error) = commit {
        let _ = std::fs::remove_file(&temp);
        let code = if error.kind() == std::io::ErrorKind::AlreadyExists {
            "AlreadyExists"
        } else {
            "IoError"
        };
        return Err(PluginFailure::plugin(
            code,
            format!(
                "bitty plugin: cannot create '{}': {error}{}",
                target.display(),
                if code == "AlreadyExists" {
                    "; pass --force to overwrite"
                } else {
                    ""
                }
            ),
        ));
    }
    Ok(target)
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
    /// `XDG_DATA_HOME` environment value (plugin store root parent).
    pub xdg_data_home: Option<&'a str>,
    /// `HOME` environment value.
    pub home: Option<&'a str>,
    /// Current working directory for the project policy layer
    /// (`<cwd>/.bitty/capabilities.conf`); `None` when undeterminable.
    pub cwd: Option<&'a str>,
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
    // Resolved once from injected environment values (never process env here).
    let store_root = crate::plugin_runtime::store_root_for(context.xdg_data_home, context.home);
    // Doctor must diagnose a malformed ledger instead of failing before it can
    // report it, so it runs before the strict state load.
    if request.verb == PluginVerb::Doctor {
        return run_plugin_doctor(
            request.repair,
            &path,
            store_root.as_deref(),
            no_color,
            output,
        );
    }
    let mut state = match load_state(&path) {
        Ok(state) => state,
        Err(failure) => {
            eprintln!("{}", failure.message);
            return failure.exit;
        }
    };
    // Config dir for next-steps display (CTX-1268): the parent of the
    // managed manifest, so an explicit `--config`/`BITTY_CONFIG` selects
    // its own `plugins/` directory for the copy-paste path.
    let config_dir: Option<PathBuf> = path.parent().map(Path::to_path_buf);

    match request.verb {
        PluginVerb::List => match rows_from_state(&state, store_root.as_deref()) {
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
            match row_for_id(&state, store_root.as_deref(), id) {
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
        PluginVerb::Explain => {
            let id = request.id.as_deref().expect("explain requires an id");
            if let Err(failure) = resolve_plugin_id(id) {
                return fail_read_only(&request, &failure, output);
            }
            let capability = request
                .capability
                .as_deref()
                .expect("explain requires a capability");
            match explain_row_for_id(&state, store_root.as_deref(), id) {
                Ok(row) => match build_explain_report(&row, capability, context) {
                    Ok(report) => finish_explain(&report, request.format, no_color, output),
                    Err(failure) => fail_read_only(&request, &failure, output),
                },
                Err(failure) => fail_read_only(&request, &failure, output),
            }
        }
        PluginVerb::Install => {
            let operand = request.id.as_deref().expect("install requires an operand");
            // Exact catalog ids win over path heuristics so a same-named
            // directory in the cwd cannot shadow an official id; a local
            // package with that name is still reachable as `./<name>`.
            if let Some(entry) = official_plugin_for(operand) {
                if request.profile.is_some() {
                    return reject_store_profile(operand, output);
                }
                return install_official(
                    &request,
                    entry,
                    store_root.as_deref(),
                    config_dir.as_deref(),
                    input,
                    output,
                );
            }
            if is_source_operand(operand) {
                if request.profile.is_some() {
                    return reject_store_profile(operand, output);
                }
                return install_source(
                    &request,
                    operand,
                    store_root.as_deref(),
                    config_dir.as_deref(),
                    input,
                    output,
                );
            }
            let profile = request
                .profile
                .as_deref()
                .map(|name| ConsentProfile::parse(name).expect("parser validates --profile"));
            match op_install(&mut state, operand, request.yes, profile, input, output) {
                Ok(result) => {
                    let code = finish_mutation(&path, state, result, output);
                    if code == EXIT_OK {
                        // Bundled success: the manifest is in the catalog
                        // (op_install just used it), so a lookup miss falls
                        // back to generic next steps, never to an error.
                        let manifest = bundled_manifest(operand).ok();
                        write_next_steps(
                            output,
                            operand,
                            manifest.as_ref(),
                            config_dir.as_deref(),
                            None,
                        );
                    }
                    code
                }
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Enable => {
            let id = request.id.as_deref().expect("enable requires an id");
            if is_managed_operand(&state, id) {
                match op_enable(&mut state, id) {
                    Ok(result) => {
                        let code = finish_mutation(&path, state, result, output);
                        if code == EXIT_OK {
                            let manifest = bundled_manifest(id).ok();
                            write_next_steps(
                                output,
                                id,
                                manifest.as_ref(),
                                config_dir.as_deref(),
                                None,
                            );
                        }
                        code
                    }
                    Err(failure) => {
                        eprintln!("{}", failure.message);
                        failure.exit
                    }
                }
            } else {
                set_store_enabled(&store_root, id, true, config_dir.as_deref(), output)
            }
        }
        PluginVerb::Disable => {
            let id = request.id.as_deref().expect("disable requires an id");
            if is_managed_operand(&state, id) {
                match op_disable(&mut state, id) {
                    Ok(result) => finish_mutation(&path, state, result, output),
                    Err(failure) => {
                        eprintln!("{}", failure.message);
                        failure.exit
                    }
                }
            } else {
                set_store_enabled(&store_root, id, false, config_dir.as_deref(), output)
            }
        }
        PluginVerb::Remove => {
            let id = request.id.as_deref().expect("remove requires an id");
            if is_managed_operand(&state, id) {
                match op_remove(&mut state, id, request.force) {
                    Ok(result) => finish_mutation(&path, state, result, output),
                    Err(failure) => {
                        eprintln!("{}", failure.message);
                        failure.exit
                    }
                }
            } else {
                uninstall_store(&store_root, id, request.force, output)
            }
        }
        PluginVerb::Revoke => {
            let id = request.id.as_deref().expect("revoke requires an id");
            if !is_managed_operand(&state, id) {
                eprintln!(
                    "bitty plugin: `revoke {id}` needs a managed record (bundled plugin); \
                     installed packages revoke through `bitty plugin remove {id} --force` \
                     followed by `bitty plugin install <path>` to re-grant"
                );
                return EXIT_PLUGIN;
            }
            match op_revoke(&mut state, id, request.cap.as_deref()) {
                Ok(result) => finish_mutation(&path, state, result, output),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Grant => {
            let id = request.id.as_deref().expect("grant requires an id");
            if !is_managed_operand(&state, id) {
                eprintln!(
                    "bitty plugin: `grant {id}` needs a managed record (bundled plugin); \
                     installed packages change grants only by `bitty plugin remove {id} --force` \
                     followed by a reviewed `bitty plugin install <source>`"
                );
                return EXIT_PLUGIN;
            }
            let profile = request
                .profile
                .as_deref()
                .map(|name| ConsentProfile::parse(name).expect("parser validates --profile"));
            match op_grant(
                &mut state,
                id,
                profile,
                request.cap.as_deref(),
                request.yes,
                input,
                output,
            ) {
                Ok(result) => {
                    let already_enabled = state.get(id).is_some_and(|record| record.enabled);
                    let code = finish_mutation(&path, state, result, output);
                    if code == EXIT_OK && !already_enabled {
                        let _ = writeln!(
                            output,
                            "bitty plugin: '{id}' remains disabled; review `bitty plugin info {id}` \
                             and run `bitty plugin enable {id}` once every requested capability is granted"
                        );
                    }
                    code
                }
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Doctor => {
            // Dispatched before the state load; unreachable here.
            EXIT_OK
        }
        PluginVerb::Config => {
            let id = request.id.as_deref().expect("config requires an id");
            if let Err(failure) = resolve_plugin_id(id) {
                return fail_read_only(&request, &failure, output);
            }
            let manifest = match config_manifest_for_id(store_root.as_deref(), id) {
                Ok(manifest) => manifest,
                Err(failure) => return fail_read_only(&request, &failure, output),
            };
            let config_dir = config_dir.as_deref().expect("state path has a parent");
            match request.config_action.unwrap_or(ConfigAction::Show) {
                ConfigAction::Show => {
                    let report = build_config_report(
                        &state,
                        store_root.as_deref(),
                        config_dir,
                        id,
                        &manifest,
                    );
                    match request.format {
                        PluginFormat::Table => {
                            let _ =
                                output.write_all(format_config_table(&report, no_color).as_bytes());
                        }
                        PluginFormat::Json | PluginFormat::Jsonl => {
                            let _ = writeln!(output, "{}", format_config_envelope(&report));
                        }
                    }
                    // Fail closed after reporting: an unreadable file or a
                    // key the schema does not declare is a hard error for an
                    // inspection command, never a silent success.
                    if let Some(error) = &report.file_error {
                        eprintln!(
                            "bitty plugin: ConfigRejected: settings file for '{id}' was \
                             rejected: {error}"
                        );
                        return EXIT_PLUGIN;
                    }
                    if !report.dropped.is_empty() {
                        eprintln!(
                            "bitty plugin: UndeclaredKey: settings file for '{id}' carries \
                             undeclared or wrong-typed keys: {}",
                            report.dropped.join(", ")
                        );
                        return EXIT_PLUGIN;
                    }
                    EXIT_OK
                }
                ConfigAction::Scaffold => {
                    match write_config_scaffold(config_dir, id, &manifest.settings, request.force) {
                        Ok(target) => {
                            let _ = writeln!(
                                output,
                                "bitty plugin: scaffolded '{}' (edit values, then restart bitty \
                                 to apply)",
                                target.display()
                            );
                            EXIT_OK
                        }
                        Err(failure) => {
                            eprintln!("{}: {}", failure.code, failure.message);
                            failure.exit
                        }
                    }
                }
            }
        }
    }
}

/// Whether an operand addresses the managed (bundled) catalog rather than the
/// installed package store.
fn is_managed_operand(state: &PluginState, id: &str) -> bool {
    state.get(id).is_some() || bundled_manifest_for(id).is_some()
}

/// Whether an `install` operand names a filesystem source rather than a
/// bundled plugin id.
///
/// Path separators, leading `.`/`~`, an existing directory or file, an
/// archive suffix, and any git/URL spelling all select the package-manager
/// path. A bare dotted token is a bundled id; the id grammar cannot contain
/// separators, so the two never overlap.
fn is_source_operand(token: &str) -> bool {
    if classify_archive_kind(token).is_some() {
        return true;
    }
    if normalize_git_source(token).is_some() || rejected_scheme(token).is_some() {
        return true;
    }
    token.contains('/')
        || token.contains('\\')
        || token.starts_with('.')
        || token.starts_with('~')
        || Path::new(token).is_dir()
        || Path::new(token).is_file()
}

/// Naming diagnostic for a rejected fetch scheme (fail-closed, exit 4).
///
/// Names the scheme so `file://`, `http://`, `ssh://`, `git://`, `git:`,
/// `ftp://`, and other non-git spellings are distinguishable from a malformed
/// git URL. Returns `None` when the token carries no URL scheme at all.
fn rejected_scheme(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    // Order matters: check the longest distinctive prefixes first.
    for scheme in [
        "file://", "http://", "https://", "ssh://", "git://", "ftp://", "ftps://", "git:",
    ] {
        if lower.starts_with(scheme) {
            // `https://` and `git@` have allowed git spellings; only report
            // them here when they fail the git allowlist (the caller checks
            // `normalize_git_source` first).
            return Some(scheme.to_string());
        }
    }
    None
}

/// Local archive kind selected by file-name suffix (case-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    /// `.tar.gz` / `.tgz` (gzip-compressed tar).
    TarGz,
    /// `.tar.zst` (zstd-compressed tar).
    TarZst,
    /// `.zip`.
    Zip,
}

/// Classify an `install` operand as a local archive by suffix.
///
/// Returns `None` for non-archive spellings. The file itself is validated
/// later (must exist, must be a regular file, size-bounded); the suffix alone
/// selects the unpack helper.
fn classify_archive_kind(token: &str) -> Option<ArchiveKind> {
    let lower = token.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(ArchiveKind::TarGz)
    } else if lower.ends_with(".tar.zst") {
        Some(ArchiveKind::TarZst)
    } else if lower.ends_with(".zip") {
        Some(ArchiveKind::Zip)
    } else {
        None
    }
}

/// Normalize an `install` operand to a cloneable git URL.
///
/// Accepts exactly three spellings (case-insensitive scheme where applicable):
/// `https://<host>/<path>.git`, `git+https://<host>/<path>.git` (the `git+`
/// prefix is stripped before cloning; git itself does not understand it),
/// and scp-like `git@<host>:<path>`. Returns the URL to pass to
/// `git clone` (stripped for `git+https`), or `None` when the token is not an
/// allowed git spelling. Malformed git-looking tokens fail closed in the
/// caller with a naming diagnostic; this function only recognizes the
/// allowlist.
fn normalize_git_source(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    if let Some(inner) = token
        .strip_prefix("git+")
        .or_else(|| token.strip_prefix("GIT+"))
        .or_else(|| {
            if lower.starts_with("git+") {
                Some(&token[4..])
            } else {
                None
            }
        })
    {
        // `git+https://…` only: `git+` plus any other scheme is rejected.
        // The scheme is normalized to lowercase: git picks its transport
        // from the literal scheme text, and a mixed-case `Https://` would
        // make it look for a nonexistent `git-remote-Https` helper.
        if inner.to_ascii_lowercase().starts_with("https://") && is_valid_https_git_url(inner) {
            return Some(format!("https://{}", &inner[8..]));
        }
        return None;
    }
    if lower.starts_with("https://") {
        if is_valid_https_git_url(token) {
            return Some(format!("https://{}", &token[8..]));
        }
        return None;
    }
    if token.starts_with("git@") {
        if is_valid_scp_git_source(token) {
            return Some(token.to_string());
        }
        return None;
    }
    None
}

/// Whether `token` is a valid `https://` git URL for cloning.
///
/// Strict allowlist: `https://` host (`[A-Za-z0-9.-]`, leading alnum,
/// optional `:port`) `/` path (`[A-Za-z0-9._/-]`, non-empty) ending in
/// `.git`. Anything else (whitespace, control, backslash, quotes, shell
/// metacharacters, `=` so `--config`/`--upload-pack=` cannot hide in the
/// path, `@` so credentials cannot hide in the URL) fails closed. The length
/// is already bounded by [`MAX_PLUGIN_TOKEN_BYTES`] at parse time.
fn is_valid_https_git_url(token: &str) -> bool {
    let Some(rest) = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("HTTPS://"))
        .or_else(|| {
            if token.len() >= 8 && token[..8].eq_ignore_ascii_case("https://") {
                Some(&token[8..])
            } else {
                None
            }
        })
    else {
        return false;
    };
    if rest.is_empty() || rest.len() > MAX_PLUGIN_TOKEN_BYTES {
        return false;
    }
    if rest
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
    {
        return false;
    }
    // No shell metacharacters, no backslash, no credential `@`, no `=` (which
    // would let `--config`/`--upload-pack=` hide in the path as one argv).
    if rest.contains([
        '\\', '"', '\'', '`', '$', '|', ';', '&', '<', '>', '(', ')', '{', '}', '*', '?', '#', '@',
        '=', '!', '+', ',', '%',
    ]) {
        return false;
    }
    // A leading dash would let the URL be parsed as a git flag without the
    // end-of-options separator; reject regardless (defense in depth: the
    // clone argv also carries `--`).
    if rest.starts_with('-') {
        return false;
    }
    let Some(slash) = rest.find('/') else {
        return false;
    };
    let (host_port, path) = (&rest[..slash], &rest[slash + 1..]);
    if host_port.is_empty() || path.is_empty() {
        return false;
    }
    if !host_port
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
    {
        return false;
    }
    let host = host_port.split(':').next().unwrap_or("");
    if host.is_empty()
        || !host
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
    {
        return false;
    }
    if let Some(port) = host_port.split(':').nth(1) {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if host_port.split(':').count() != 2 {
            return false;
        }
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'/')
    {
        return false;
    }
    if path.contains("//") {
        return false;
    }
    path.ends_with(".git") && path.len() > ".git".len()
}

/// Whether `token` is a valid scp-like `git@<host>:<path>` source.
///
/// Strict allowlist: literal `git@`, host (`[A-Za-z0-9.-]`, leading alnum),
/// `:`, path (`[A-Za-z0-9._/-]`, non-empty, no leading dash). The `.git`
/// suffix is conventional but not required for this spelling. The same
/// metacharacter denylist as [`is_valid_https_git_url`] applies.
fn is_valid_scp_git_source(token: &str) -> bool {
    let Some(rest) = token.strip_prefix("git@") else {
        return false;
    };
    if rest.is_empty() || rest.len() > MAX_PLUGIN_TOKEN_BYTES {
        return false;
    }
    if rest
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
    {
        return false;
    }
    if rest.contains([
        '\\', '"', '\'', '`', '$', '|', ';', '&', '<', '>', '(', ')', '{', '}', '*', '?', '#', '@',
        '=', '!', '+', ',', '%',
    ]) {
        return false;
    }
    if rest.starts_with('-') {
        return false;
    }
    let Some(colon) = rest.find(':') else {
        return false;
    };
    let (host, path) = (&rest[..colon], &rest[colon + 1..]);
    if host.is_empty() || path.is_empty() {
        return false;
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return false;
    }
    if !host
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
    {
        return false;
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'/')
    {
        return false;
    }
    if path.contains("//") || path.starts_with('-') {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Fetch staging (CTX-1103, #1901): exclusive temp dirs, fixed-argv helpers
// ---------------------------------------------------------------------------

/// Maximum archive file size staged (64 MiB; the declared unpacked content is
/// bounded separately by [`MAX_FETCH_UNPACKED_BYTES`], and the module-tree
/// ceilings still bound the committed content to 16 MiB / 4096 files).
const MAX_FETCH_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum total declared unpacked bytes one archive may hold (64 MiB, same
/// as the compressed-file cap).
///
/// Zip-bomb defense: a highly compressible archive can declare far more
/// temporary disk than its file size suggests, so the declared total from
/// the pre-extraction listing (`tar -tv` / `unzip -l` sizes) is rejected
/// above this limit before anything is extracted, and the same total is
/// enforced again while walking the extracted tree (defense in depth).
const MAX_FETCH_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum bytes of one helper listing (`tar -tf` / `unzip -Z1`) parsed.
const MAX_FETCH_LIST_BYTES: usize = 256 * 1024;
/// Maximum archive members listed (2x the module-tree file ceiling + slack
/// for directory entries).
const MAX_FETCH_MEMBERS: usize = 8192;
/// Maximum bytes kept from a failed helper's stderr (bounded detail).
const MAX_FETCH_STDERR_BYTES: usize = 2048;
/// Maximum bytes for one archive member path (the ratified 1024-byte ceiling).
const MAX_FETCH_MEMBER_PATH_BYTES: usize = 1024;

/// Exclusive temp staging dir for one git/archive fetch (removed on drop
/// unless disarmed by forgetting — the caller moves the payload out through
/// the package manager first, then drops the guard to clean the temp copy).
struct ScopedFetchDir {
    path: PathBuf,
}

impl ScopedFetchDir {
    fn create(tag: &str) -> Result<Self, String> {
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt as _;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..100 {
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "bitty-plugin-fetch-{tag}-{}-{sequence}-{nanos}",
                std::process::id()
            ));
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "bitty plugin: cannot create staging dir '{}': {error}",
                        path.display()
                    ));
                }
            }
        }
        Err("bitty plugin: cannot create staging dir (no fresh name after retries)".to_string())
    }
}

impl Drop for ScopedFetchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Bounded, sanitized tail of a failed helper's stderr (no control bytes).
fn fetch_stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let tail: String = text
        .chars()
        .rev()
        .take(MAX_FETCH_STDERR_BYTES)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    tail.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Repository-location variables never inherited by a spawned helper.
///
/// `git` (and hooks that export these, e.g. `pre-commit`) lets `GIT_DIR` /
/// `GIT_WORK_TREE` / `GIT_INDEX_FILE` override the command line, so a clone
/// running under such a parent could lock and overwrite the caller's index.
/// Only these location variables are scrubbed: gitconfig, SSH-agent, proxy,
/// and CA handling still come free from the inherited environment.
const GIT_REPO_ENV_VARS: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
];

/// Run one fixed-argv helper; map every failure to a user-facing message.
///
/// `tool` resolves via `PATH` (`Command::new` finds `git.exe`/`tar.exe` on
/// Windows). The child inherits the caller's environment (system
/// gitconfig/SSH-agent/proxy/CA handling comes free, mirroring
/// `component_seed.rs` `SystemTransport` posture). No shell is ever
/// constructed: every element of `argv` is inert data.
fn run_fetch_tool(tool: &str, argv: &[String]) -> Result<std::process::Output, String> {
    run_fetch_tool_full(tool, argv, &[], false)
}

/// [`run_fetch_tool`] with a scoped environment override.
///
/// `extra_env` sets additional variables for this invocation only (the
/// process environment is never mutated); `scrub_git_repo_env` removes the
/// [`GIT_REPO_ENV_VARS`] location variables so a hooked parent cannot
/// redirect the child into the caller's repository.
fn run_fetch_tool_full(
    tool: &str,
    argv: &[String],
    extra_env: &[(&str, &str)],
    scrub_git_repo_env: bool,
) -> Result<std::process::Output, String> {
    let mut command = std::process::Command::new(tool);
    command.args(argv);
    command.envs(extra_env.iter().copied());
    if scrub_git_repo_env {
        for var in GIT_REPO_ENV_VARS {
            command.env_remove(var);
        }
    }
    command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            match tool {
                "git" => "bitty plugin: `git` not found (install git first, or install from a local directory or archive)".to_string(),
                "tar" => "bitty plugin: `tar` not found (install tar first, or install from a local directory)".to_string(),
                "unzip" => "bitty plugin: `unzip` not found (install unzip first, or repack as .tar.gz; .zip installs fail closed without it)".to_string(),
                _ => format!("bitty plugin: `{tool}` not found (install {tool} first)"),
            }
        } else {
            format!("bitty plugin: cannot run `{tool}`: {error}")
        }
    })
}

/// Fixed-argv `git clone --filter=blob:none --depth=1 -- <url> <dest>`.
///
/// The `--` end-of-options separator plus the leading-dash rejection in
/// [`is_valid_https_git_url`]/[`is_valid_scp_git_source`] closes
/// `--upload-pack=`/`--config` smuggling via the URL: the URL is always one
/// inert argv element, never a flag. Repository-location variables are
/// scrubbed ([`GIT_REPO_ENV_VARS`]) so a clone under a git hook cannot lock
/// the caller's index; everything else is inherited.
fn run_git_clone(url: &str, dest: &Path) -> Result<(), String> {
    let argv = vec![
        "clone".to_string(),
        "--filter=blob:none".to_string(),
        "--depth=1".to_string(),
        "--".to_string(),
        url.to_string(),
        dest.to_string_lossy().into_owned(),
    ];
    let output = run_fetch_tool_full("git", &argv, &[], true)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: `git clone` failed ({}; {tail})",
            output.status
        ));
    }
    Ok(())
}

/// Run one fixed-argv git command with repository-location scrubbing.
fn run_git_checked(argv: &[String], what: &str) -> Result<std::process::Output, String> {
    let output = run_fetch_tool_full("git", argv, &[("GIT_TERMINAL_PROMPT", "0")], true)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: {what} failed ({}; {tail})",
            output.status
        ));
    }
    Ok(output)
}

/// Prepare `dest` as a shallow checkout of exactly `pin` from `url`.
///
/// `git init` + `remote add` + `fetch --depth=1 origin <pin>` + detached
/// `checkout` avoids cloning unrelated branch history while still accepting
/// a pin that is older than the default-branch tip. Every argv element is
/// inert data; the URL already passed the CLI allowlist and the pin is the
/// catalog's 40-hex gitlink. The caller re-reads `HEAD` and compares it to
/// `pin` before reading any manifest bytes.
fn run_git_fetch_pinned(url: &str, pin: &str, dest: &Path) -> Result<(), String> {
    let dest_arg = dest.to_string_lossy().into_owned();
    run_git_checked(
        &[
            "init".to_string(),
            "--quiet".to_string(),
            "--".to_string(),
            dest_arg.clone(),
        ],
        "`git init`",
    )?;
    run_git_checked(
        &[
            "-C".to_string(),
            dest_arg.clone(),
            "remote".to_string(),
            "add".to_string(),
            "origin".to_string(),
            url.to_string(),
        ],
        "`git remote add`",
    )?;
    run_git_checked(
        &[
            "-C".to_string(),
            dest_arg.clone(),
            "fetch".to_string(),
            "--quiet".to_string(),
            "--depth=1".to_string(),
            "--no-tags".to_string(),
            "origin".to_string(),
            pin.to_string(),
        ],
        "`git fetch` of the pinned commit",
    )?;
    run_git_checked(
        &[
            "-C".to_string(),
            dest_arg,
            "-c".to_string(),
            "advice.detachedHead=false".to_string(),
            "checkout".to_string(),
            "--quiet".to_string(),
            "--detach".to_string(),
            pin.to_string(),
        ],
        "`git checkout` of the pinned commit",
    )?;
    Ok(())
}

/// Read `HEAD` from a prepared checkout (`git rev-parse --verify HEAD`).
fn run_git_head(checkout: &Path) -> Result<String, String> {
    let argv = vec![
        "-C".to_string(),
        checkout.to_string_lossy().into_owned(),
        "rev-parse".to_string(),
        "--verify".to_string(),
        "HEAD".to_string(),
    ];
    let output = run_git_checked(&argv, "`git rev-parse`")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// List tar members (names only, never extracted here).
fn list_tar_members(archive: &Path, kind: ArchiveKind) -> Result<Vec<String>, String> {
    let archive_arg = archive.to_string_lossy().into_owned();
    let argv: Vec<String> = match kind {
        ArchiveKind::TarGz => vec!["-tzf".to_string(), archive_arg],
        ArchiveKind::TarZst => vec![
            "--use-compress-program=unzstd".to_string(),
            "-tf".to_string(),
            archive_arg,
        ],
        ArchiveKind::Zip => {
            return Err("bitty plugin: internal error: tar lister called for .zip".to_string());
        }
    };
    let output = run_fetch_tool("tar", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive members ({}; {tail})",
            output.status
        ));
    }
    parse_member_list(&output.stdout)
}

/// List zip members via `unzip -Z1` (bare names, one per line).
fn list_zip_members(archive: &Path) -> Result<Vec<String>, String> {
    let argv = vec!["-Z1".to_string(), archive.to_string_lossy().into_owned()];
    let output = run_fetch_tool("unzip", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive members ({}; {tail})",
            output.status
        ));
    }
    parse_member_list(&output.stdout)
}

/// Parse and bound one helper listing (UTF-8, member-count bounded).
fn parse_member_list(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.len() > MAX_FETCH_LIST_BYTES {
        return Err(format!(
            "bitty plugin: archive member list exceeds {MAX_FETCH_LIST_BYTES} bytes (refusing hostile archive)"
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        "bitty plugin: archive member names are not UTF-8 (refusing hostile archive)".to_string()
    })?;
    let members: Vec<String> = text.lines().map(str::to_string).collect();
    if members.len() > MAX_FETCH_MEMBERS {
        return Err(format!(
            "bitty plugin: archive holds {} members (limit {MAX_FETCH_MEMBERS}; refusing hostile archive)",
            members.len()
        ));
    }
    Ok(members)
}

/// Declared unpacked total for a tar archive via `tar -tv` sizes.
///
/// Never extracts: the verbose listing carries one size field per member.
/// Returns the entry count plus the summed declared bytes so the caller can
/// cross-check the count against the names listing (a mismatch fails closed).
fn declared_tar_unpacked_bytes(archive: &Path, kind: ArchiveKind) -> Result<(usize, u64), String> {
    let archive_arg = archive.to_string_lossy().into_owned();
    let argv: Vec<String> = match kind {
        ArchiveKind::TarGz => vec!["-tzvf".to_string(), archive_arg],
        ArchiveKind::TarZst => vec![
            "--use-compress-program=unzstd".to_string(),
            "-tvf".to_string(),
            archive_arg,
        ],
        ArchiveKind::Zip => {
            return Err(
                "bitty plugin: internal error: tar size lister called for .zip".to_string(),
            );
        }
    };
    // `bsdtar` localizes `%b` month names via the process locale; the parser
    // below only accepts the English `TAR_VERBOSE_MONTHS`, so this one
    // invocation runs under the C locale. The names listing keeps the user's
    // locale, and the process environment is never mutated.
    let output = run_fetch_tool_full("tar", &argv, &[("LC_ALL", "C")], false)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive sizes ({}; {tail})",
            output.status
        ));
    }
    parse_tar_verbose_total(&output.stdout)
}

/// BSD `tar -tv` month abbreviations (libarchive output spells the date as
/// `Mon DD HH:MM` or `Mon DD YYYY` where GNU spells `YYYY-MM-DD HH:MM`).
const TAR_VERBOSE_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Whether a field looks like a GNU `tar -tv` date (`YYYY-MM-DD`).
fn is_tar_verbose_date(field: &str) -> bool {
    field.len() == 10
        && field.as_bytes()[4] == b'-'
        && field.as_bytes()[7] == b'-'
        && field
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

/// Whether a field looks like a `tar -tv` time (`HH:MM`).
fn is_tar_verbose_time(field: &str) -> bool {
    field.len() == 5
        && field.as_bytes()[2] == b':'
        && field
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 2 || b.is_ascii_digit())
}

/// Parse `tar -tv` output into an entry count plus a declared byte total.
///
/// Accepts GNU (`PERMS OWNER SIZE YYYY-MM-DD HH:MM NAME`) and BSD/libarchive
/// (`PERMS NLINK USER GROUP SIZE Mon DD HH:MM|YYYY NAME`) shapes by anchoring
/// the size on the date field (the GNU date is searched first so a BSD month
/// name inside a GNU filename can never misalign the parse). Every line must
/// match one shape with a numeric size, a date, a time-or-year, and a name;
/// anything else fails closed rather than guessing.
fn parse_tar_verbose_total(bytes: &[u8]) -> Result<(usize, u64), String> {
    let refuse = |why: &str| {
        format!("bitty plugin: cannot parse archive sizes ({why}; refusing hostile archive)")
    };
    if bytes.len() > MAX_FETCH_LIST_BYTES {
        return Err(refuse("size listing too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| refuse("size listing is not UTF-8"))?;
    let mut count = 0usize;
    let mut total = 0u64;
    for line in text.lines() {
        if line.is_empty() {
            return Err(refuse("blank size entry"));
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // GNU shape first: the size sits immediately before the date field.
        let size = if let Some(date_at) = fields.iter().position(|f| is_tar_verbose_date(f)) {
            let Some(size_text) = date_at.checked_sub(1).and_then(|i| fields.get(i)) else {
                return Err(refuse("malformed size entry"));
            };
            // The date must be followed by a time and a name.
            let (Some(time), Some(_)) = (fields.get(date_at + 1), fields.get(date_at + 2)) else {
                return Err(refuse("malformed size entry"));
            };
            if !is_tar_verbose_time(time) {
                return Err(refuse("malformed size entry"));
            }
            size_text
        } else if let Some(month_at) = fields.iter().position(|f| TAR_VERBOSE_MONTHS.contains(f)) {
            // BSD shape: the size sits immediately before the month, followed
            // by a 1-2 digit day and a time (`HH:MM`) or year (`YYYY`).
            let Some(size_text) = month_at.checked_sub(1).and_then(|i| fields.get(i)) else {
                return Err(refuse("malformed size entry"));
            };
            let (Some(day), Some(clock)) = (fields.get(month_at + 1), fields.get(month_at + 2))
            else {
                return Err(refuse("malformed size entry"));
            };
            let day_ok =
                !day.is_empty() && day.len() <= 2 && day.bytes().all(|b| b.is_ascii_digit());
            let clock_ok = is_tar_verbose_time(clock)
                || (clock.len() == 4 && clock.bytes().all(|b| b.is_ascii_digit()));
            if !day_ok || !clock_ok || fields.get(month_at + 3).is_none() {
                return Err(refuse("malformed size entry"));
            }
            size_text
        } else {
            return Err(refuse("malformed size entry"));
        };
        let size: u64 = size.parse().map_err(|_| refuse("non-numeric size entry"))?;
        total = total.saturating_add(size);
        count += 1;
        if count > MAX_FETCH_MEMBERS {
            return Err(format!(
                "bitty plugin: archive holds {} members (limit {MAX_FETCH_MEMBERS}; refusing hostile archive)",
                count + 1
            ));
        }
        // Immediate refusal once the running total passes the enforcement
        // bound: returning here (instead of stopping early with a partial
        // count) keeps the declared-size diagnostic instead of surfacing a
        // misleading listing-count mismatch downstream.
        if total > MAX_FETCH_UNPACKED_BYTES {
            return Err(format!(
                "bitty plugin: archive declares {total} unpacked bytes (limit {MAX_FETCH_UNPACKED_BYTES}; refusing hostile archive)"
            ));
        }
    }
    Ok((count, total))
}

/// Declared unpacked total for a zip archive via `unzip -l` sizes.
///
/// Never extracts: the table carries one `Length` field per member row.
/// Returns the data-row count plus the summed lengths so the caller can
/// cross-check the count against the `-Z1` names listing.
fn declared_zip_unpacked_bytes(archive: &Path) -> Result<(usize, u64), String> {
    let argv = vec!["-l".to_string(), archive.to_string_lossy().into_owned()];
    let output = run_fetch_tool("unzip", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive sizes ({}; {tail})",
            output.status
        ));
    }
    parse_zip_list_total(&output.stdout)
}

/// Parse an `unzip -l` table into a data-row count plus a summed total.
///
/// Skips the `Archive:`/`Length` header and consumes rows after the first
/// `-----` separator until the footer separator; every row must lead with a
/// numeric length (names may contain spaces, which is why only the first
/// field is read). Anything else fails closed.
fn parse_zip_list_total(bytes: &[u8]) -> Result<(usize, u64), String> {
    let refuse = |why: &str| {
        format!("bitty plugin: cannot parse archive sizes ({why}; refusing hostile archive)")
    };
    if bytes.len() > MAX_FETCH_LIST_BYTES {
        return Err(refuse("size listing too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| refuse("size listing is not UTF-8"))?;
    let mut lines = text.lines();
    let mut in_rows = false;
    let mut count = 0usize;
    let mut total = 0u64;
    let mut closed = false;
    for line in &mut lines {
        if line.starts_with("-----") {
            if in_rows {
                closed = true;
                break;
            }
            in_rows = true;
            continue;
        }
        if !in_rows {
            continue;
        }
        if line.trim().is_empty() {
            return Err(refuse("blank size entry"));
        }
        let Some(first) = line.split_whitespace().next() else {
            return Err(refuse("malformed size entry"));
        };
        let size: u64 = first
            .parse()
            .map_err(|_| refuse("non-numeric size entry"))?;
        total = total.saturating_add(size);
        count += 1;
        if count > MAX_FETCH_MEMBERS {
            return Err(format!(
                "bitty plugin: archive holds {} members (limit {MAX_FETCH_MEMBERS}; refusing hostile archive)",
                count + 1
            ));
        }
    }
    if !closed {
        return Err(refuse("size table has no footer"));
    }
    Ok((count, total))
}

/// Validate one archive member name (fail closed on traversal).
///
/// Rejects absolute members (`/…`), Windows separators (`\`, `:` so drive
/// letters cannot hide), `..` path segments, over-long paths, and control
/// characters. Leading `./` and trailing `/` (directory entries) are
/// tolerated and stripped before the checks. An empty root entry (`./`)
/// is allowed.
fn validate_archive_member(raw: &str) -> Result<(), String> {
    if raw.is_empty() {
        return Err(
            "bitty plugin: archive holds an empty member name (refusing hostile archive)"
                .to_string(),
        );
    }
    if raw.len() > MAX_FETCH_MEMBER_PATH_BYTES {
        return Err(format!(
            "bitty plugin: archive member {raw:?} exceeds {MAX_FETCH_MEMBER_PATH_BYTES} bytes (refusing hostile archive)"
        ));
    }
    if raw.contains('\0') || raw.chars().any(|c| c.is_control() && c != '\t') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} carries control characters (refusing hostile archive)"
        ));
    }
    // Tolerate the `./` prefix tar emits for `tar -C src .` archives and the
    // trailing `/` on directory entries; everything else is checked literally
    // (no normalization — normalization is where traversal bugs hide).
    let mut name = raw;
    if let Some(stripped) = name.strip_prefix("./") {
        name = stripped;
    } else if name == "." {
        return Ok(());
    }
    name = name.strip_suffix('/').unwrap_or(name);
    if name.is_empty() {
        // `./` or `/` root entries (the latter is stripped to empty only when
        // the raw member was exactly `/`, which is absolute and rejected
        // below — but `./` legitimately reduces to empty here).
        if raw == "./" || raw == "." {
            return Ok(());
        }
        return Err(format!(
            "bitty plugin: archive member {raw:?} is not a usable path (refusing hostile archive)"
        ));
    }
    if raw.starts_with('/') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} is absolute (refusing hostile archive)"
        ));
    }
    if name.starts_with('/') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} is absolute (refusing hostile archive)"
        ));
    }
    if raw.contains('\\') || raw.contains(':') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} carries a Windows separator or drive (refusing hostile archive)"
        ));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err(format!(
                "bitty plugin: archive member {raw:?} has an empty path segment (refusing hostile archive)"
            ));
        }
        if component == ".." {
            return Err(format!(
                "bitty plugin: archive member {raw:?} escapes its directory (refusing hostile archive)"
            ));
        }
    }
    Ok(())
}

/// Audit every listed member before anything is extracted.
fn audit_archive_members(members: &[String]) -> Result<(), String> {
    if members.is_empty() {
        return Err("bitty plugin: archive holds no members (refusing empty archive)".to_string());
    }
    for member in members {
        validate_archive_member(member)?;
    }
    Ok(())
}

/// Extract a tar archive (fixed argv, confined with `-C`).
fn extract_tar(archive: &Path, dest: &Path, kind: ArchiveKind) -> Result<(), String> {
    let argv: Vec<String> = match kind {
        ArchiveKind::TarGz => vec![
            "-xzf".to_string(),
            archive.to_string_lossy().into_owned(),
            "-C".to_string(),
            dest.to_string_lossy().into_owned(),
        ],
        ArchiveKind::TarZst => vec![
            "--use-compress-program=unzstd".to_string(),
            "-xf".to_string(),
            archive.to_string_lossy().into_owned(),
            "-C".to_string(),
            dest.to_string_lossy().into_owned(),
        ],
        ArchiveKind::Zip => {
            return Err("bitty plugin: internal error: tar extractor called for .zip".to_string());
        }
    };
    let output = run_fetch_tool("tar", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot unpack archive ({}; {tail})",
            output.status
        ));
    }
    Ok(())
}

/// Extract a zip archive (`unzip -q <archive> -d <dest>`, fixed argv).
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let argv = vec![
        "-q".to_string(),
        archive.to_string_lossy().into_owned(),
        "-d".to_string(),
        dest.to_string_lossy().into_owned(),
    ];
    let output = run_fetch_tool("unzip", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot unpack archive ({}; {tail})",
            output.status
        ));
    }
    Ok(())
}

/// Audit the extracted tree: no symlinks, total regular-file bytes bounded.
///
/// Both GNU `tar` and `unzip` refuse to follow a just-created symlink dir
/// component during the same extraction (verified), so a symlink member lands
/// inside staging as a link and is caught here before the package manager
/// ever reads the tree. The walked byte total re-enforces
/// [`MAX_FETCH_UNPACKED_BYTES`] as defense in depth (the declared total was
/// already checked pre-extraction; the listing and the payload could still
/// disagree). The staging dir is removed by the caller on any failure.
fn audit_extracted_tree(root: &Path) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    let mut total = 0u64;
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            format!(
                "bitty plugin: cannot inspect unpacked archive '{}': {error}",
                dir.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("bitty plugin: cannot inspect unpacked archive entry: {error}")
            })?;
            seen += 1;
            if seen > MAX_FETCH_MEMBERS {
                return Err(format!(
                    "bitty plugin: unpacked archive exceeds {MAX_FETCH_MEMBERS} entries (refusing hostile archive)"
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                format!(
                    "bitty plugin: cannot inspect unpacked member '{}': {error}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "bitty plugin: archive member '{}' is a symlink (refusing hostile archive)",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
                if total > MAX_FETCH_UNPACKED_BYTES {
                    return Err(format!(
                        "bitty plugin: unpacked archive exceeds {MAX_FETCH_UNPACKED_BYTES} bytes (refusing hostile archive)"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Pick the package root inside an extracted archive.
///
/// Archives either lay the manifest at the extraction root
/// (`bitty-plugin.toml`) or under one top-level directory
/// (`my-plugin/bitty-plugin.toml`); both are accepted. Anything else falls
/// through to the extraction root so the existing manifest-missing diagnostic
/// names the problem.
fn find_plugin_root(extract_dir: &Path) -> PathBuf {
    if extract_dir.join("bitty-plugin.toml").is_file() {
        return extract_dir.to_path_buf();
    }
    let Ok(entries) = std::fs::read_dir(extract_dir) else {
        return extract_dir.to_path_buf();
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut has_files = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        if path.is_dir() {
            dirs.push(path);
        } else {
            has_files = true;
        }
    }
    if !has_files && dirs.len() == 1 && dirs[0].join("bitty-plugin.toml").is_file() {
        return dirs.remove(0);
    }
    extract_dir.to_path_buf()
}

/// Unpack one local archive file into `dest` (must exist, must be empty).
///
/// Lists members first (never extracted here), audits every name for
/// traversal, rejects declared unpacked totals above
/// [`MAX_FETCH_UNPACKED_BYTES`] before anything is extracted (zip-bomb
/// defense), extracts with the fixed-argv helper, then audits the tree for
/// symlinks and re-enforces the byte total. Any failure is fail-closed with
/// nothing trusted.
fn unpack_archive_into(archive: &Path, dest: &Path, kind: ArchiveKind) -> Result<(), String> {
    let members = match kind {
        ArchiveKind::TarGz | ArchiveKind::TarZst => list_tar_members(archive, kind)?,
        ArchiveKind::Zip => list_zip_members(archive)?,
    };
    audit_archive_members(&members)?;
    // Zip-bomb gate: the declared total must fit before extraction starts.
    // The entry count must agree with the names listing; a mismatch means
    // the two listing passes disagreed and fails closed.
    let (declared_count, declared_total) = match kind {
        ArchiveKind::TarGz | ArchiveKind::TarZst => declared_tar_unpacked_bytes(archive, kind)?,
        ArchiveKind::Zip => declared_zip_unpacked_bytes(archive)?,
    };
    if declared_count != members.len() {
        return Err(
            "bitty plugin: archive size listing disagrees with its member listing (refusing hostile archive)"
                .to_string(),
        );
    }
    if declared_total > MAX_FETCH_UNPACKED_BYTES {
        return Err(format!(
            "bitty plugin: archive declares {declared_total} unpacked bytes (limit {MAX_FETCH_UNPACKED_BYTES}; refusing hostile archive)"
        ));
    }
    match kind {
        ArchiveKind::TarGz | ArchiveKind::TarZst => extract_tar(archive, dest, kind)?,
        ArchiveKind::Zip => extract_zip(archive, dest)?,
    }
    audit_extracted_tree(dest)
}

/// Install or update one plugin from a local directory into the XDG store.
fn install_source(
    request: &PluginRequest,
    source: &str,
    store_root: Option<&Path>,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    // Git spellings (allowlist) clone first, then share the local pipeline.
    if let Some(url) = normalize_git_source(source) {
        return install_git_source(request, source, &url, store_root, config_dir, input, output);
    }
    // A URL-looking operand that is not an allowed git spelling fails closed
    // with a naming diagnostic (http downgrade, file escape, ssh without
    // git@, option-smuggling spellings that still look like URLs).
    if rejected_scheme(source).is_some() || looks_like_url(source) {
        return reject_fetch_scheme(source);
    }
    if let Some(kind) = classify_archive_kind(source) {
        return install_archive_source(
            request, source, kind, store_root, config_dir, input, output,
        );
    }
    install_local_path(
        request,
        source,
        Path::new(source),
        &LocalInstallOptions::staged_and_disabled(),
        store_root,
        config_dir,
        input,
        output,
    )
}

/// Whether a token looks like a URL even when its scheme is not allowlisted.
///
/// Catches `GIT+HTTPS://…` case variants, `ssh:…`, and leading-dash
/// smuggling spellings (`--upload-pack=…`, `--config …`) that reach this
/// layer as one argv element, so they fail closed as scheme errors (exit 4)
/// instead of masquerading as bundled ids.
fn looks_like_url(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("://")
        || lower.starts_with("git@")
        || lower.starts_with("git:")
        || lower.starts_with("ssh:")
        || token.starts_with('-')
}

/// Fail closed on a rejected fetch scheme with a naming diagnostic.
fn reject_fetch_scheme(source: &str) -> i32 {
    let lower = source.to_ascii_lowercase();
    let scheme = if lower.starts_with("file://") {
        "file://"
    } else if lower.starts_with("http://") {
        "http://"
    } else if lower.starts_with("ssh://") || lower.starts_with("ssh:") {
        "ssh://"
    } else if lower.starts_with("git://") || lower.starts_with("git:") {
        "git://"
    } else if lower.starts_with("ftp://") || lower.starts_with("ftps://") {
        "ftp://"
    } else if lower.contains("://") {
        "URL"
    } else if source.starts_with('-') {
        "flag-like"
    } else {
        "non-git"
    };
    if scheme == "http://" {
        eprintln!(
            "bitty plugin: refusing {scheme} source {source:?}: plugin git sources require https:// (or git+https:// / git@); \
             install from a local directory or archive instead"
        );
    } else if scheme == "flag-like" {
        eprintln!(
            "bitty plugin: refusing flag-like source {source:?}: plugin sources are bundled ids, local paths, \
             https://<host>/<path>.git, git+https://..., git@<host>:<path>, or local .tar.gz/.tgz/.tar.zst/.zip files"
        );
    } else {
        eprintln!(
            "bitty plugin: refusing {scheme} source {source:?}: plugin git sources are https://<host>/<path>.git, \
             git+https://..., or git@<host>:<path> only; install from a local directory or archive instead"
        );
    }
    EXIT_PLUGIN
}

/// Install one official plugin by exact catalog id.
///
/// Resolution is fully local: the embedded catalog supplies the repository,
/// pinned commit, registry manifest hash, and declared compatibility. The
/// flow is:
///
/// 1. evaluate `compat.bitty` / `compatibility.sdk` against this host before
///    any fetch and refuse with the exact declared range;
/// 2. fetch exactly the pinned commit into exclusive staging (no branch
///    drift);
/// 3. re-read `HEAD` and fail closed on a pin mismatch;
/// 4. hash the raw `bitty-plugin.toml` bytes and fail closed on a registry
///    manifest-hash mismatch;
/// 5. run the existing package pipeline (manifest/schema/entry-point bounds,
///    compat re-check, capability consent, atomic store transaction) with
///    [`SourceClass::Registry`] provenance.
///
/// No plugin code runs and no store/ledger byte is written before the hash,
/// pin, compat, and consent gates pass.
fn install_official(
    request: &PluginRequest,
    entry: &'static OfficialPlugin,
    store_root: Option<&Path>,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    // Catalog compatibility is known before any network access: an entry
    // that excludes this host must be refused with the exact declared range,
    // not a fetch or a manifest-parse diagnostic.
    if let Err(error) = official_entry_compat(entry) {
        eprintln!("bitty plugin: {error}");
        return EXIT_PLUGIN;
    }
    // The catalog is compiled in, but treat its strings as untrusted input:
    // the repository must pass the CLI git allowlist and the pin must be a
    // 40-hex commit before either reaches an argv.
    if !is_valid_https_git_url(entry.repository) {
        eprintln!(
            "bitty plugin: official catalog entry '{}' has an invalid repository URL; nothing installed",
            entry.id
        );
        return EXIT_PLUGIN;
    }
    if !is_lower_hex(entry.pin, 40) {
        eprintln!(
            "bitty plugin: official catalog entry '{}' has an invalid pin; nothing installed",
            entry.id
        );
        return EXIT_PLUGIN;
    }
    let staging = match ScopedFetchDir::create("official") {
        Ok(staging) => staging,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_GENERIC;
        }
    };
    let checkout = staging.path.join("src");
    if let Err(message) = run_git_fetch_pinned(entry.repository, entry.pin, &checkout) {
        eprintln!("{message}");
        return EXIT_GENERIC;
    }
    match run_git_head(&checkout) {
        Ok(head) if head == entry.pin => {}
        Ok(head) => {
            eprintln!(
                "bitty plugin: official catalog pin mismatch for '{}': catalog {}, checkout {}; nothing installed",
                entry.id,
                short_hash(entry.pin),
                short_hash(&head)
            );
            return EXIT_PLUGIN;
        }
        Err(message) => {
            eprintln!("{message}");
            return EXIT_GENERIC;
        }
    }
    let manifest_path = checkout.join(package::MANIFEST_FILE_NAME);
    match std::fs::metadata(&manifest_path) {
        Ok(metadata) if metadata.len() as usize <= PLUGIN_MANIFEST_MAX_BYTES => {}
        Ok(metadata) => {
            eprintln!(
                "bitty plugin: official '{}' manifest is {} bytes (limit {PLUGIN_MANIFEST_MAX_BYTES}); nothing installed",
                entry.id,
                metadata.len()
            );
            return EXIT_PLUGIN;
        }
        Err(error) => {
            eprintln!(
                "bitty plugin: official '{}' has no readable {}: {error}; nothing installed",
                entry.id,
                package::MANIFEST_FILE_NAME
            );
            return EXIT_PLUGIN;
        }
    }
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!(
                "bitty plugin: official '{}' cannot read {}: {error}; nothing installed",
                entry.id,
                package::MANIFEST_FILE_NAME
            );
            return EXIT_PLUGIN;
        }
    };
    let actual = bitty_package::integrity::sha256_hex(&bytes);
    if actual != entry.manifest_sha256() {
        eprintln!(
            "bitty plugin: official catalog manifest hash mismatch for '{}': registry sha256:{}, fetched sha256:{}; nothing installed",
            entry.id,
            entry.manifest_sha256(),
            actual
        );
        return EXIT_PLUGIN;
    }
    match manifest_toml::parse_manifest(&bytes) {
        Ok(manifest) if manifest.id().as_str() == entry.id => {}
        Ok(manifest) => {
            eprintln!(
                "bitty plugin: official '{}' checkout carries manifest id '{}'; nothing installed",
                entry.id,
                manifest.id().as_str()
            );
            return EXIT_PLUGIN;
        }
        Err(detail) => {
            eprintln!(
                "bitty plugin: official '{}' manifest is invalid: {detail}; nothing installed",
                entry.id
            );
            return EXIT_PLUGIN;
        }
    }
    let _ = writeln!(
        output,
        "bitty plugin: official '{}' from {} at {} (manifest sha256:{} verified)",
        entry.id,
        entry.repository,
        short_hash(entry.pin),
        short_hash(entry.manifest_sha256())
    );
    let options = LocalInstallOptions {
        source_class: SourceClass::Registry,
        ..LocalInstallOptions::staged_and_disabled()
    };
    install_local_path(
        request, entry.id, &checkout, &options, store_root, config_dir, input, output,
    )
}

/// Install from a git URL: clone into exclusive temp staging, then the
/// existing local-path pipeline unchanged (manifest/schema/compat, hash pin,
/// consent, atomic `current.json`). No plugin code runs at any step.
fn install_git_source(
    request: &PluginRequest,
    display: &str,
    url: &str,
    store_root: &Path,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let staging = match ScopedFetchDir::create("git") {
        Ok(staging) => staging,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_GENERIC;
        }
    };
    let checkout = staging.path.join("src");
    if let Err(message) = run_git_clone(url, &checkout) {
        eprintln!("{message}");
        return EXIT_GENERIC;
    }
    install_local_path(
        request,
        display,
        &checkout,
        &LocalInstallOptions::staged_and_disabled(),
        store_root,
        config_dir,
        input,
        output,
    )
}

/// Install from a local archive file: unpack into exclusive temp staging,
/// then the existing local-path pipeline unchanged.
fn install_archive_source(
    request: &PluginRequest,
    display: &str,
    kind: ArchiveKind,
    store_root: &Path,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let canonical = match std::fs::canonicalize(display) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("bitty plugin: invalid source: '{display}': {error}");
            return EXIT_GENERIC;
        }
    };
    let metadata = match std::fs::metadata(&canonical) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("bitty plugin: invalid source: '{display}': {error}");
            return EXIT_GENERIC;
        }
    };
    if !metadata.is_file() {
        eprintln!("bitty plugin: invalid source: '{display}' is not a file");
        return EXIT_GENERIC;
    }
    if metadata.len() > MAX_FETCH_ARCHIVE_BYTES {
        eprintln!(
            "bitty plugin: archive '{display}' exceeds {MAX_FETCH_ARCHIVE_BYTES} bytes (refusing hostile archive)"
        );
        return EXIT_PLUGIN;
    }
    // Snapshot the archive into staging first so a concurrent replace of the
    // user path cannot change the bytes between listing and extraction.
    let staging = match ScopedFetchDir::create("archive") {
        Ok(staging) => staging,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_GENERIC;
        }
    };
    let suffix = match kind {
        ArchiveKind::TarGz => "payload.tar.gz",
        ArchiveKind::TarZst => "payload.tar.zst",
        ArchiveKind::Zip => "payload.zip",
    };
    let staged_archive = staging.path.join(suffix);
    let bytes = match std::fs::read(&canonical) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("bitty plugin: invalid source: '{display}': {error}");
            return EXIT_GENERIC;
        }
    };
    if bytes.len() as u64 > MAX_FETCH_ARCHIVE_BYTES {
        eprintln!(
            "bitty plugin: archive '{display}' exceeds {MAX_FETCH_ARCHIVE_BYTES} bytes (refusing hostile archive)"
        );
        return EXIT_PLUGIN;
    }
    if let Err(error) = std::fs::write(&staged_archive, &bytes) {
        eprintln!("bitty plugin: cannot stage archive: {error}");
        return EXIT_GENERIC;
    }
    let extract_dir = staging.path.join("src");
    if let Err(error) = std::fs::create_dir(&extract_dir) {
        eprintln!("bitty plugin: cannot create staging dir: {error}");
        return EXIT_GENERIC;
    }
    if let Err(message) = unpack_archive_into(&staged_archive, &extract_dir, kind) {
        // Distinguish validation (plugin error, exit 4) from helper/io
        // failures (exit 1) by message shape: hostile-archive audits refuse
        // explicitly, everything else is an io/helper failure. `unpack` ends
        // with the extracted-tree audit, so no second walk is needed here.
        eprintln!("{message}");
        if message.contains("refusing hostile archive")
            || message.contains("no members")
            || message.contains("empty archive")
            || message.contains("disagrees with its member listing")
        {
            return EXIT_PLUGIN;
        }
        return EXIT_GENERIC;
    }
    let root = find_plugin_root(&extract_dir);
    install_local_path(
        request,
        display,
        &root,
        &LocalInstallOptions::staged_and_disabled(),
        store_root,
        config_dir,
        input,
        output,
    )
}

/// Shared local-path pipeline tail: consent bound to the staged snapshot,
/// then `package::install_local_dir` (manifest, entry point, bounds, compat,
/// hash pin, atomic `current.json`). The `source` argument selects the input
/// dir; `display` names the original operand in consent prompts; `options`
/// carries the fresh-install state policy and the recorded provenance class.
#[allow(clippy::too_many_arguments)] // one call site per source shape; bundling would hide the flow
fn install_local_path(
    request: &PluginRequest,
    display: &str,
    source: &Path,
    options: &LocalInstallOptions,
    store_root: &Path,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    // The caller-selected options carry the fresh-install state policy and the
    // recorded provenance class; explicit `enable` remains the authorization
    // boundary for first execution. `--force` approves replacing an
    // orphaned/partial or same-version directory without the repair prompt.
    // Capability consent is separate.
    let options = &LocalInstallOptions {
        force_repair: request.force,
        ..options.clone()
    };
    let approve_all = request.yes;
    // Consent is bound to the staged snapshot the installer reviewed: the
    // callback only decides, and the installer commits that same snapshot, so
    // a source swapped while the prompt is open can never widen the grant.
    let outcome = {
        let mut consent =
            |consent_request: &package::ConsentRequest| -> Result<bool, PackageOpError> {
                if approve_all {
                    return Ok(true);
                }
                match ask_source_consent(input, output, consent_request, display) {
                    Ok(approved) => Ok(approved),
                    Err(failure) => Err(PackageOpError::ConsentAborted(failure.message)),
                }
            };
        package::install_local_dir(store_root, source, options, &mut consent)
    };
    match outcome {
        Ok(Some(report)) => {
            report_install(&report, output);
            // Post-install next steps (CTX-1268): the installer verified
            // `source/bitty-plugin.toml`, so re-read that body best-effort
            // for settings/hints; a concurrent replace falls back to
            // generic next steps, never to an error.
            let installed_under = store_root.join(&report.root);
            let manifest = read_source_manifest_for_next_steps(source);
            write_next_steps(
                output,
                &report.plugin_id,
                manifest.as_ref(),
                config_dir,
                Some(&installed_under),
            );
            EXIT_OK
        }
        Ok(None) => {
            eprintln!(
                "bitty plugin: install was not approved (capability grant or package repair) \
                 — nothing changed"
            );
            EXIT_GENERIC
        }
        Err(error) => {
            eprintln!("bitty plugin: {error}");
            package_failure_exit(&error)
        }
    }
}

fn report_install(report: &package::InstallReport, output: &mut dyn std::io::Write) {
    let action = if report.updated {
        "updated"
    } else {
        "installed"
    };
    let _ = writeln!(
        output,
        "bitty plugin: {action} '{}' (version {}, {} source, manifest {}, {} capabilit{} granted)",
        report.plugin_id,
        report.version,
        report.source_class.as_str(),
        short_hash(&report.manifest_hash),
        report.granted.len(),
        if report.granted.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    if report.content_replaced {
        let _ = writeln!(
            output,
            "bitty plugin: replaced the existing package content in place at {}",
            report.root
        );
    }
    if let Some(previous) = &report.previous_version {
        if previous != &report.version {
            let _ = writeln!(
                output,
                "bitty plugin: previous version {previous} retained for rollback under {}",
                report.root.rsplit_once('/').map_or("-", |(dir, _)| dir)
            );
        }
    }
    // CTX-0417: post-commit cleanup is non-fatal but must be visible: the
    // install already succeeded, so warn instead of failing.
    if let Some(warning) = &report.prune_warning {
        eprintln!("warning: bitty plugin: {warning}");
    }
}

// ---------------------------------------------------------------------------
// Post-install next steps (CTX-1268, bitty#2144)
// ---------------------------------------------------------------------------

/// Canonical per-plugin settings file extension (Lua is the canonical
/// surface; the flat-key TOML file is a legacy compatibility fallback).
const PLUGIN_LUA_FILE_EXTENSION: &str = "lua";

/// Escape one string for a TOML basic string in rendered reports.
///
/// Manifest string defaults are bounded but attacker-controlled; raw control
/// characters (including an ESC the settings grammar tolerates inside a
/// default) must never reach the terminal unescaped. Emits `\\`, `\"`, `\n`,
/// `\r`, `\t`, and `\uXXXX` for other C0/C1 controls and DEL; every other
/// character passes through unchanged.
///
/// These are TOML basic-string escapes and are **not** valid Lua literals
/// (`\uXXXX` is not a Lua escape); Lua-facing templates must use
/// [`escape_lua_string`] instead.
fn escape_toml_basic(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Render post-install next steps for one plugin (CTX-1268, bitty#2144).
///
/// Data-driven and display-only: the enable command, where the plugin landed,
/// the canonical Lua settings path, the `config show` pointer, and the
/// `bitty plugin info <id>` pointer always render; the `config scaffold`
/// pointer renders only when `has_settings` (the manifest declares at least
/// one `[settings]` entry), because scaffolding a settings-less manifest
/// exits without writing anything. Manifest-provided `[hints] next_steps`
/// lines append when the manifest declares them, otherwise the output stays
/// clean generic.
/// `installed_under` names the store package root for external installs
/// (`None` for bundled plugins, which ship with bitty). Nothing here grants
/// authority, writes state, or runs plugin code.
fn format_next_steps(
    id: &str,
    next_steps: &[String],
    has_settings: bool,
    config_file: Option<&Path>,
    installed_under: Option<&Path>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Next steps for '{id}':");
    let _ = writeln!(out, "  enable: bitty plugin enable {id}");
    match installed_under {
        Some(root) => {
            let _ = writeln!(out, "  installed: {}", root.display());
        }
        None => {
            let _ = writeln!(out, "  installed: bundled with bitty (no package tree)");
        }
    }
    match config_file {
        Some(path) => {
            let _ = writeln!(out, "  configure: {}", path.display());
        }
        None => {
            let _ = writeln!(
                out,
                "  configure: <config-root>/plugins/{id}.{PLUGIN_LUA_FILE_EXTENSION} \
                 (no config root resolved)"
            );
        }
    }
    let _ = writeln!(out, "  inspect: bitty plugin config show {id}");
    if has_settings {
        let _ = writeln!(
            out,
            "  scaffold: bitty plugin config scaffold {id} (writes a commented Lua template)"
        );
    } else {
        // `config scaffold` exits without writing for a manifest with no
        // `[settings]`; never advertise a command that cannot act.
        let _ = writeln!(out, "  scaffold: unavailable (no [settings] declared)");
    }
    if !next_steps.is_empty() {
        out.push_str("  hints:\n");
        for hint in next_steps {
            let _ = writeln!(out, "    - {hint}");
        }
    }
    let _ = writeln!(out, "  more: bitty plugin info {id}");
    out
}

/// Display path of one plugin's canonical Lua settings file
/// (`<config-dir>/plugins/<owner>.<name>.lua`; CTX-1195, issue #2374).
///
/// The id is already validated (`PluginId` grammar: one dot, no separators),
/// so the join cannot escape the `plugins/` directory; it is display-only
/// (no I/O happens here). Legacy `<owner>.<name>.toml` remains a runtime
/// compatibility fallback but is not advertised by the hint.
fn plugin_config_file_display(config_dir: Option<&Path>, id: &str) -> Option<PathBuf> {
    config_dir.map(|dir| {
        dir.join(PLUGINS_DIR_NAME)
            .join(format!("{id}.{PLUGIN_LUA_FILE_EXTENSION}"))
    })
}

/// Best-effort manifest read for next-steps rendering (store installs).
///
/// The installer already verified this body; a read/parse failure here (a
/// concurrent replace between commit and render) falls back to generic
/// next steps without hints, never to an error.
fn read_source_manifest_for_next_steps(source: &Path) -> Option<PluginManifest> {
    let bytes = std::fs::read(source.join(package::MANIFEST_FILE_NAME)).ok()?;
    manifest_toml::parse_manifest(&bytes).ok()
}

/// Write next steps for one successful install/enable (display-only).
///
/// `manifest` carries the verified manifest when at hand (bundled catalog or
/// the just-installed source body); `None` renders the clean generic shape
/// (no hints) without failing the command.
fn write_next_steps(
    output: &mut dyn std::io::Write,
    id: &str,
    manifest: Option<&PluginManifest>,
    config_dir: Option<&Path>,
    installed_under: Option<&Path>,
) {
    let config_file = plugin_config_file_display(config_dir, id);
    let next_steps: &[String] = manifest.map_or(&[], |manifest| &manifest.next_steps);
    let has_settings = manifest.is_some_and(|manifest| !manifest.settings.is_empty());
    let _ = writeln!(
        output,
        "{}",
        format_next_steps(
            id,
            next_steps,
            has_settings,
            config_file.as_deref(),
            installed_under
        )
    );
}

/// Toggle one installed store record.
fn set_store_enabled(
    store_root: &Option<PathBuf>,
    id: &str,
    enabled: bool,
    config_dir: Option<&Path>,
    output: &mut dyn std::io::Write,
) -> i32 {
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    match package::set_enabled(store_root, id, enabled) {
        Ok(changed) => {
            let state = if enabled { "enabled" } else { "disabled" };
            let _ = writeln!(
                output,
                "bitty plugin: '{id}' {}",
                if changed {
                    state.to_string()
                } else {
                    format!("already {state}")
                }
            );
            if enabled {
                // Best-effort store manifest for next steps (a missing index
                // entry or an unreadable body falls back to generic next
                // steps, never to an error).
                let record = load_index(store_root)
                    .ok()
                    .and_then(|records| records.into_iter().find(|record| record.plugin_id == id));
                let (manifest, installed_under) = match record.as_ref() {
                    Some(record) => (
                        read_store_manifest(store_root, record),
                        bitty_runtime::plugin_runtime::resolution::store_package_root(
                            store_root, record,
                        ),
                    ),
                    None => (None, None),
                };
                write_next_steps(
                    output,
                    id,
                    manifest.as_ref(),
                    config_dir,
                    installed_under.as_deref(),
                );
            }
            EXIT_OK
        }
        Err(error) => {
            eprintln!("bitty plugin: {error}");
            package_failure_exit(&error)
        }
    }
}

/// Ledger (`bitty-plugins.toml`) health for `bitty plugin doctor`.
struct LedgerInspection {
    present: bool,
    records: usize,
    error: Option<String>,
    unknown_ids: Vec<String>,
    stale_pins: Vec<String>,
    state: Option<PluginState>,
}

/// Inspect the managed ledger without failing the doctor on a bad file.
fn inspect_ledger(path: &Path) -> LedgerInspection {
    let mut inspection = LedgerInspection {
        present: false,
        records: 0,
        error: None,
        unknown_ids: Vec::new(),
        stale_pins: Vec::new(),
        state: None,
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return inspection,
        Err(error) => {
            inspection.error = Some(format!("cannot read '{}': {error}", path.display()));
            return inspection;
        }
    };
    inspection.present = true;
    if bytes.len() > MAX_STATE_FILE_BYTES {
        inspection.error = Some(format!("exceeds {MAX_STATE_FILE_BYTES} bytes"));
        return inspection;
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) => {
            inspection.error = Some(format!("not UTF-8: {error}"));
            return inspection;
        }
    };
    let state = match PluginState::parse(text) {
        Ok(state) => state,
        Err(error) => {
            inspection.error = Some(error.to_string());
            return inspection;
        }
    };
    inspection.records = state.ids().count();
    for id in state.ids() {
        match bundled_manifest_for(id) {
            None => inspection.unknown_ids.push(id.clone()),
            Some(manifest) => {
                if state
                    .get(id)
                    .is_some_and(|record| record.manifest_hash != manifest.manifest_hash())
                {
                    inspection.stale_pins.push(id.clone());
                }
            }
        }
    }
    inspection.state = Some(state);
    inspection
}

/// Whether a store issue is a hard failure (needs repair) rather than a warn.
fn store_issue_fails(kind: bitty_runtime::plugin_runtime::package::StoreIssueKind) -> bool {
    use bitty_runtime::plugin_runtime::package::StoreIssueKind;
    matches!(
        kind,
        StoreIssueKind::CorruptIndex | StoreIssueKind::DanglingRecord
    )
}

/// `bitty plugin doctor [--repair]`: diagnose store + ledger health.
///
/// Read-only by default. `--repair` moves incomplete/unreferenced package
/// trees into `<store>/quarantine/`, removes stale staging directories,
/// rebuilds `current.json` from complete package directories, and quarantines
/// a malformed ledger after moving it aside. It never invents capability
/// grants: a rebuilt record with a changed manifest hash starts disabled with
/// an empty grant set, and the report says so.
fn run_plugin_doctor(
    repair: bool,
    state_path: &Path,
    store_root: Option<&Path>,
    no_color: bool,
    output: &mut dyn std::io::Write,
) -> i32 {
    let color = color_enabled(no_color);
    let _ = writeln!(output, "{}", bold("bitty plugin doctor", color));
    let mut failures = 0usize;
    let mut warnings = 0usize;

    match store_root {
        Some(root) => {
            let _ = writeln!(output, "  store:  {}", root.display());
            match package::inspect_store(root) {
                Ok(inspection) => {
                    if let Some(error) = &inspection.index_error {
                        failures += 1;
                        let _ = writeln!(
                            output,
                            "  [fail] store index: current.json is unreadable ({error})"
                        );
                        let _ = writeln!(
                            output,
                            "         repair keeps the file: bitty plugin doctor --repair"
                        );
                    } else if inspection.index_present {
                        let _ = writeln!(
                            output,
                            "  [ok]   store index: current.json ({} record(s))",
                            inspection.record_count
                        );
                    } else {
                        let _ = writeln!(
                            output,
                            "  [ok]   store index: absent (empty store; rebuilt on demand)"
                        );
                    }
                    for issue in &inspection.issues {
                        let fail = store_issue_fails(issue.kind);
                        if fail {
                            failures += 1;
                        } else {
                            warnings += 1;
                        }
                        let _ = writeln!(
                            output,
                            "  [{}] {} {} {}: {}",
                            if fail { "fail" } else { "warn" },
                            issue.kind.as_str(),
                            issue.plugin_id,
                            issue.version,
                            issue.detail
                        );
                        if issue.kind.repairable() && !repair {
                            let _ =
                                writeln!(output, "         repair: bitty plugin doctor --repair");
                        }
                    }
                    // A store record for a bundled id can never be trusted:
                    // bundled provenance is first-party only. Repair
                    // quarantines the shadowing tree (never deletes it).
                    if let Ok(records) = load_index(root) {
                        for record in &records {
                            if bundled_manifest_for(&record.plugin_id).is_some() {
                                failures += 1;
                                let _ = writeln!(
                                    output,
                                    "  [fail] store/bundled mismatch {}: a store record shadows \
                                     a bundled plugin id; repair: bitty plugin doctor --repair \
                                     (quarantines the store tree)",
                                    record.plugin_id
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    failures += 1;
                    let _ = writeln!(output, "  [fail] store unreadable: {error}");
                    let _ = writeln!(
                        output,
                        "         check ownership/permissions; repair needs a writable store: \
                         bitty plugin doctor --repair"
                    );
                }
            }
        }
        None => {
            failures += 1;
            let _ = writeln!(
                output,
                "  [fail] store: no data root ($XDG_DATA_HOME or $HOME unset)"
            );
        }
    }

    let _ = writeln!(output, "  ledger: {}", state_path.display());
    let mut ledger = inspect_ledger(state_path);
    if let Some(error) = &ledger.error {
        failures += 1;
        let _ = writeln!(output, "  [fail] ledger unreadable: {error}");
        let _ = writeln!(
            output,
            "         repair quarantines the file and starts an empty valid ledger: \
             bitty plugin doctor --repair"
        );
    } else if ledger.present {
        let _ = writeln!(output, "  [ok]   ledger: {} record(s)", ledger.records);
    } else {
        let _ = writeln!(output, "  [ok]   ledger: absent (bundled defaults only)");
    }
    for id in &ledger.unknown_ids {
        warnings += 1;
        let _ = writeln!(
            output,
            "  [warn] ledger-record {id}: no bundled manifest for this id; \
             repair removes the stale record"
        );
    }
    for id in &ledger.stale_pins {
        warnings += 1;
        let _ = writeln!(
            output,
            "  [warn] ledger-pin {id}: recorded manifest hash differs from the bundled \
             catalog; re-run `bitty plugin install {id}` to re-pin and re-consent"
        );
    }

    if repair {
        let mut repair_failures = 0usize;
        let _ = writeln!(output, "\nrepair:");
        if let Some(root) = store_root {
            match package::repair_store(root) {
                Ok(report) => {
                    for restored in &report.restored {
                        let _ = writeln!(output, "  restored {restored}");
                    }
                    for (from, to) in &report.quarantined {
                        let _ = writeln!(output, "  quarantined {from} -> {to}");
                    }
                    for removed in &report.staging_removed {
                        let _ = writeln!(output, "  removed staging {removed}");
                    }
                    for warning in &report.warnings {
                        let _ = writeln!(output, "  warning: {warning}");
                    }
                    if report.index_rebuilt {
                        let _ = writeln!(
                            output,
                            "  wrote current.json ({} record(s))",
                            report.records_written
                        );
                    }
                    if report.quarantined.is_empty()
                        && report.restored.is_empty()
                        && report.staging_removed.is_empty()
                        && !report.index_rebuilt
                    {
                        let _ = writeln!(output, "  store already consistent");
                    }
                }
                Err(error) => {
                    repair_failures += 1;
                    let _ = writeln!(output, "  [fail] store repair failed: {error}");
                }
            }
        }
        if ledger.error.is_some() {
            let nonce = doctor_nonce();
            let quarantined = state_path.with_file_name(format!(
                "{}.corrupt-{nonce}",
                state_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "bitty-plugins.toml".to_string())
            ));
            match std::fs::rename(state_path, &quarantined) {
                Ok(()) => {
                    let _ = writeln!(output, "  quarantined ledger -> {}", quarantined.display());
                    match save_state(state_path, &PluginState::new()) {
                        Ok(_) => {
                            let _ = writeln!(output, "  wrote an empty valid ledger");
                        }
                        Err(failure) => {
                            repair_failures += 1;
                            let _ = writeln!(
                                output,
                                "  [fail] cannot write a fresh ledger: {}",
                                failure.message
                            );
                        }
                    }
                }
                Err(error) => {
                    repair_failures += 1;
                    let _ = writeln!(
                        output,
                        "  [fail] cannot quarantine '{}': {error} (check ownership/permissions)",
                        state_path.display()
                    );
                }
            }
        } else if let Some(mut state) = ledger.state.take() {
            let unknown = ledger.unknown_ids.clone();
            if !unknown.is_empty() {
                for id in &unknown {
                    state.remove(id);
                }
                match save_state(state_path, &state) {
                    Ok(backup) => {
                        let _ = writeln!(
                            output,
                            "  removed {} stale ledger record(s) ({})",
                            unknown.len(),
                            unknown.join(", ")
                        );
                        if let Some(backup) = backup {
                            let _ =
                                writeln!(output, "  previous ledger kept at {}", backup.display());
                        }
                    }
                    Err(failure) => {
                        repair_failures += 1;
                        let _ = writeln!(
                            output,
                            "  [fail] cannot rewrite ledger: {}",
                            failure.message
                        );
                    }
                }
            } else {
                let _ = writeln!(output, "  ledger already consistent");
            }
        }
        let _ = writeln!(
            output,
            "\nresult: {}",
            if repair_failures == 0 {
                format!(
                    "repair completed ({warnings} finding(s) were addressed); \
                     re-run `bitty plugin doctor` to confirm"
                )
            } else {
                format!("repair finished with {repair_failures} failure(s)")
            }
        );
        return if repair_failures == 0 {
            EXIT_OK
        } else {
            EXIT_GENERIC
        };
    }

    if failures + warnings == 0 {
        let _ = writeln!(output, "\nresult: healthy");
        EXIT_OK
    } else {
        let _ = writeln!(
            output,
            "\nresult: {} failure(s), {warnings} warning(s); run `bitty plugin doctor --repair`",
            failures
        );
        EXIT_GENERIC
    }
}

/// Monotonic nonce for ledger quarantine names.
fn doctor_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Uninstall one installed store record (destructive; requires `--force`).
fn uninstall_store(
    store_root: &Option<PathBuf>,
    id: &str,
    force: bool,
    output: &mut dyn std::io::Write,
) -> i32 {
    if !force {
        eprintln!(
            "bitty plugin: `remove {id}` is destructive; re-run with --force \
             (the installed package tree is deleted)"
        );
        return EXIT_USAGE;
    }
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    match package::uninstall(store_root, id) {
        Ok(report) => {
            if report.record_removed {
                let _ = writeln!(
                    output,
                    "bitty plugin: removed '{}' (version {}, {} package tree)",
                    report.plugin_id,
                    report.version,
                    if report.tree_removed {
                        "deleted"
                    } else {
                        "already absent"
                    }
                );
            } else {
                let _ = writeln!(
                    output,
                    "bitty plugin: removed orphaned package directory for '{}' (no store \
                     record; {} package tree) — reinstall from its source to restore it",
                    report.plugin_id,
                    if report.tree_removed {
                        "deleted"
                    } else {
                        "already absent"
                    }
                );
            }
            EXIT_OK
        }
        Err(error) => {
            eprintln!("bitty plugin: {error}");
            package_failure_exit(&error)
        }
    }
}

/// Interactive consent for package repair and newly requested capabilities.
///
/// A repair prompt comes first because replacing an existing version directory
/// is destructive; declining either prompt commits nothing. `--yes` approves
/// both non-interactively, while `--force` bypasses only the repair prompt.
fn ask_source_consent(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    request: &package::ConsentRequest,
    source: &str,
) -> Result<bool, PluginFailure> {
    if let Some(replacement) = &request.replacement {
        let _ = writeln!(
            output,
            "bitty plugin: existing package content for '{}' version {} conflicts with '{source}':",
            request.plugin_id, request.version
        );
        let _ = writeln!(output, "  {}", replacement.detail);
        let _ = writeln!(output, "  path: {}", replacement.root);
        if !ask_yes_no(
            input,
            output,
            "Replace it? (or re-run with --force; no plugin code runs) [y/N] ",
        )? {
            return Ok(false);
        }
    }
    if request.added.is_empty() {
        return Ok(true);
    }
    let _ = writeln!(
        output,
        "bitty plugin: '{}' (version {}, manifest {}) from '{source}' requests new capabilities:",
        request.plugin_id,
        request.version,
        short_hash(&request.manifest_hash)
    );
    for capability in &request.added {
        let effect = CapabilityId::parse(capability)
            .map(|capability| effect_statement(&capability).to_string())
            .unwrap_or_else(|_| "unknown capability".to_string());
        let _ = writeln!(output, "  - {capability} — {effect}");
    }
    ask_yes_no(input, output, "Grant these capabilities? [y/N] ")
}

/// Bounded yes/no read shared by repair and capability prompts.
///
/// Uses [`read_plugin_consent_answer`] so a large piped answer cannot grow
/// memory without bound (bitty#1919); EOF or too many invalid answers aborts
/// with nothing changed.
fn ask_yes_no(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    prompt: &str,
) -> Result<bool, PluginFailure> {
    for attempt in 1..=MAX_CONSENT_ATTEMPTS {
        let _ = write!(output, "{prompt}");
        let _ = output.flush();
        let Some(answer) = read_plugin_consent_answer(input, output, attempt) else {
            continue;
        };
        let answer = answer?;
        let answer = answer.trim();
        if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        if answer.is_empty()
            || answer.eq_ignore_ascii_case("n")
            || answer.eq_ignore_ascii_case("no")
        {
            return Ok(false);
        }
        let _ = writeln!(output, "bitty plugin: please answer 'y' or 'n'");
    }
    Err(PluginFailure::generic(
        "ConsentAborted",
        "bitty plugin: no valid consent answer after repeated attempts — nothing changed"
            .to_string(),
    ))
}

/// Stable exit code for a package-manager failure.
fn package_failure_exit(error: &PackageOpError) -> i32 {
    match error {
        PackageOpError::Manifest { .. }
        | PackageOpError::BundledIdReserved { .. }
        | PackageOpError::ModuleTree { .. }
        | PackageOpError::Incompatible { .. } => EXIT_PLUGIN,
        PackageOpError::InvalidSource(_)
        | PackageOpError::ConsentAborted(_)
        | PackageOpError::Store(_)
        | PackageOpError::StoreTooLarge { .. } => EXIT_GENERIC,
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
                "bitty plugin: unknown plugin '{id}' ({}); install an external package \
                 from a local directory (`bitty plugin install <path>`), a git URL \
                 (`https://<host>/<path>.git`, `git+https://...`, `git@<host>:<path>`), \
                 or a local archive (`.tar.gz`/`.tgz`/`.tar.zst`/`.zip`)",
                installable_ids_hint()
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

/// Bundled + installable official ids, with listed-but-incompatible entries
/// named separately (unknown-id diagnostics must never imply a network lookup
/// or hide a catalog entry).
fn installable_ids_hint() -> String {
    let bundled = bundled_ids_sorted();
    let mut official_installable: Vec<&str> = Vec::new();
    let mut incompatible: Vec<String> = Vec::new();
    for entry in official_plugins() {
        match official_entry_compat(entry) {
            Ok(()) => official_installable.push(entry.id),
            Err(_) => incompatible.push(format!(
                "{} (compat.bitty '{}', host {})",
                entry.id,
                entry.compat_bitty.unwrap_or("-"),
                env!("CARGO_PKG_VERSION")
            )),
        }
    }
    let bundled_text = if bundled.is_empty() {
        "none".to_string()
    } else {
        bundled.join(", ")
    };
    let official_text = if official_installable.is_empty() {
        "none".to_string()
    } else {
        official_installable.join(", ")
    };
    let mut hint =
        format!("bundled ids: {bundled_text}; official installable ids: {official_text}");
    if !incompatible.is_empty() {
        hint.push_str("; listed but incompatible with this host: ");
        hint.push_str(&incompatible.join(", "));
    }
    hint
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
    profile: Option<ConsentProfile>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<OpOutput, PluginFailure> {
    let plugin_id = resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let requested = requested_capabilities(&manifest)?;
    let existing = state.get(id).cloned();
    let now = now_unix_secs();
    let catalog = bitty_package::CapabilityCatalog::core();

    // P0-AC-030 pattern: only capabilities absent from the recorded grant
    // need consent; unchanged/narrowed sets carry forward silently.
    // Revoked-then-requested capabilities are absent from `granted`, so they
    // always need explicit re-grant consent (no silent re-prompt loops).
    let needed: BTreeSet<CapabilityId> = match &existing {
        Some(record) => requested.difference(&record.granted).cloned().collect(),
        None => requested.clone(),
    };

    let profile_plan = match profile {
        None => None,
        Some(profile) => {
            let empty = BTreeSet::new();
            let granted = existing
                .as_ref()
                .map(|record| record.granted.clone())
                .unwrap_or_else(|| empty.clone());
            let denied = existing
                .as_ref()
                .map(|record| record.denied.clone())
                .unwrap_or_else(|| empty.clone());
            let plan =
                ProfileSelection::resolve(profile, &catalog, &requested, &granted, &denied)
                    .map_err(|error| {
                        PluginFailure::plugin(
                            "CapabilityProfile",
                            format!(
                                "bitty plugin: capability profile '{}' failed validation: {error} — nothing changed",
                                profile.name()
                            ),
                        )
                    })?;
            print_profile_review(output, id, &manifest_hash, &plan);
            if !plan.selected.is_empty() {
                let approved = if yes {
                    let _ = writeln!(
                        output,
                        "bitty plugin: --yes applied profile '{}' v{CONSENT_PROFILE_VERSION} for '{id}': {} capabilit{}",
                        profile.name(),
                        plan.selected.len(),
                        if plan.selected.len() == 1 { "y" } else { "ies" }
                    );
                    true
                } else {
                    ask_yes_no(
                        input,
                        output,
                        &format!(
                            "Apply profile '{}' (grant {} capabilit{})? [y/N] ",
                            profile.name(),
                            plan.selected.len(),
                            if plan.selected.len() == 1 { "y" } else { "ies" }
                        ),
                    )?
                };
                if !approved {
                    return Err(PluginFailure::generic(
                        "ConsentDeclined",
                        format!(
                            "bitty plugin: capability profile '{}' for '{id}' was not approved — nothing changed",
                            profile.name()
                        ),
                    ));
                }
            }
            Some(plan)
        }
    };

    let approved = if profile_plan.is_some() || needed.is_empty() {
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

    // Explicit consent is the re-grant action: requested capabilities leave
    // the denial set. Only consent clears denials; enable/disable/revoke
    // never do (deny-loop guard). A profile only preselects additions, so it
    // never clears a denial: only a full `install` re-consent does.
    let denied: BTreeSet<CapabilityId> = match (&existing, &profile_plan) {
        (Some(record), Some(plan)) => record
            .denied
            .iter()
            .filter(|capability| {
                requested.contains(*capability) && !plan.selected.contains(*capability)
            })
            .cloned()
            .collect(),
        (Some(record), None) => record.denied.difference(&requested).cloned().collect(),
        (None, _) => BTreeSet::new(),
    };

    let (granted, enabled) = match &profile_plan {
        Some(plan) => {
            let mut base = existing
                .as_ref()
                .map(|record| record.granted.clone())
                .unwrap_or_default();
            // Never preserve a grant the current manifest no longer requests.
            base.retain(|capability| requested.contains(capability));
            let granted = plan.granted_after(&base);
            let enabled = requested.is_subset(&granted)
                && existing.as_ref().is_none_or(|record| record.enabled);
            (granted, enabled)
        }
        None => (requested.clone(), true),
    };

    // Manifest-hash changes commit through the grant engine so the P0-AC-030
    // update gate owns the transition (OQ-012 grant lifecycle): stale hashes
    // and unapproved additions fail closed with no state change.
    if let Some(record) = &existing {
        if record.manifest_hash != manifest_hash {
            let mut engine = GrantStore::new();
            engine.insert(HostGrantRecord::granted(
                plugin_id.clone(),
                record.manifest_hash.clone(),
                record.granted.clone(),
                now,
            ));
            engine
                .apply_update(
                    &plugin_id,
                    &record.manifest_hash,
                    &manifest_hash,
                    &granted,
                    true,
                    now,
                )
                .map_err(|error| {
                    PluginFailure::plugin(
                        "CapabilityBlocked",
                        format!("bitty plugin: '{id}' update rejected by the grant gate: {error}"),
                    )
                })?;
        }
    }

    let changed = match &existing {
        Some(record) => {
            record.manifest_hash != manifest_hash
                || record.enabled != enabled
                || record.granted != granted
                || record.denied != denied
        }
        None => true,
    };
    if changed {
        state.insert(
            id.to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: manifest_hash.clone(),
                enabled,
                granted: granted.clone(),
                denied,
            },
        );
    }
    let action = match &existing {
        None => "installed",
        Some(_) if !changed => "already installed",
        Some(_) => "updated",
    };
    let state_word = if enabled {
        "enabled"
    } else if requested.is_subset(&granted) {
        "disabled"
    } else {
        "disabled until every requested capability is explicitly granted"
    };
    let mut summary = format!(
        "bitty plugin: {action} '{id}' (version {}, manifest {}, {} of {} capabilit{} granted, {state_word})",
        manifest.identity.version,
        short_hash(&manifest_hash),
        granted.len(),
        requested.len(),
        if requested.len() == 1 { "y" } else { "ies" }
    );
    if let Some(plan) = &profile_plan {
        let _ = write!(
            summary,
            "; profile '{}' v{} selected {}",
            plan.profile.name(),
            plan.profile_version,
            plan.selected.len()
        );
    }
    Ok(OpOutput { summary, changed })
}

fn op_enable(state: &mut PluginState, id: &str) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let record = state.get(id).ok_or_else(|| not_installed(id))?;
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

/// Monotonic-ish host time for grant records (Unix seconds; 0 when the clock
/// is unavailable). Used only as opaque `decided_at` evidence, never ordering.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Revoke grants for a recorded plugin without dropping the record.
///
/// - Without `capability`: every granted capability is dropped and the plugin
///   disabled (mirrors the grant-store full revoke; the record stays so the
///   decision is durable and auditable).
/// - With `capability`: one capability is dropped and remembered as an
///   explicit denial (no silent re-prompt); when the last granted capability
///   goes, the plugin is disabled exactly like a full revoke. Re-granting a
///   denied capability requires explicit `install` consent.
fn op_revoke(
    state: &mut PluginState,
    id: &str,
    capability: Option<&str>,
) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let record = state.get(id).cloned().ok_or_else(|| not_installed(id))?;
    let Some(capability) = capability else {
        if record.granted.is_empty() && !record.enabled {
            return Ok(OpOutput {
                summary: format!("bitty plugin: '{id}' already revoked"),
                changed: false,
            });
        }
        let count = record.granted.len();
        let kept_denied = record.denied.len();
        let entry = state.get_mut(id).expect("checked");
        entry.granted.clear();
        entry.enabled = false;
        return Ok(OpOutput {
            summary: format!(
                "bitty plugin: revoked '{id}' ({} grant{} dropped{}; re-grant with `bitty plugin install {id}`)",
                count,
                if count == 1 { "" } else { "s" },
                if kept_denied == 0 {
                    String::new()
                } else {
                    format!(
                        ", {} explicit denial{} kept",
                        kept_denied,
                        if kept_denied == 1 { "" } else { "s" }
                    )
                },
            ),
            changed: true,
        });
    };
    let capability = CapabilityId::parse(capability).map_err(|error| {
        PluginFailure::usage(format!("bitty plugin: invalid capability: {error}"))
    })?;
    if !record.granted.contains(&capability) {
        return Err(PluginFailure::plugin(
            "CapabilityNotGranted",
            format!(
                "bitty plugin: '{}' is not granted for '{id}' (nothing revoked)",
                capability.as_str()
            ),
        ));
    }
    let entry = state.get_mut(id).expect("checked");
    entry.granted.remove(&capability);
    entry.denied.insert(capability.clone());
    let fully_revoked = entry.granted.is_empty();
    if fully_revoked {
        entry.enabled = false;
    }
    Ok(OpOutput {
        summary: if fully_revoked {
            format!(
                "bitty plugin: revoked '{}' for '{id}' (last grant; plugin disabled, denial kept; re-grant with `bitty plugin install {id}`)",
                capability.as_str()
            )
        } else {
            format!(
                "bitty plugin: revoked '{}' for '{id}' (denial kept; re-grant with `bitty plugin install {id}`)",
                capability.as_str()
            )
        },
        changed: true,
    })
}

/// Add reviewed grants to a recorded bundled plugin.
///
/// This is the apply path for `--profile`; `--cap` is the explicit per-item
/// path for sensitive capabilities. Neither path enables the plugin, removes
/// existing grants, clears an explicit per-capability denial, or widens the
/// manifest's requested set. Only a full `install` re-consent clears denials.
fn op_grant(
    state: &mut PluginState,
    id: &str,
    profile: Option<ConsentProfile>,
    capability: Option<&str>,
    yes: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let record = state.get(id).cloned().ok_or_else(|| not_installed(id))?;
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
    let catalog = bitty_package::CapabilityCatalog::core();

    if let Some(profile) = profile {
        let plan = ProfileSelection::resolve(
            profile,
            &catalog,
            &requested,
            &record.granted,
            &record.denied,
        )
        .map_err(|error| {
            PluginFailure::plugin(
                "CapabilityProfile",
                format!(
                    "bitty plugin: capability profile '{}' failed validation: {error} — nothing changed",
                    profile.name()
                ),
            )
        })?;
        print_profile_review(output, id, &manifest_hash, &plan);
        if plan.selected.is_empty() {
            return Ok(OpOutput {
                summary: format!(
                    "bitty plugin: profile '{}' v{} grants nothing new for '{id}' \
                     ({} already granted, {} require explicit per-item consent, {} not selected, {} denied)",
                    profile.name(),
                    plan.profile_version,
                    plan.already_granted.len(),
                    plan.excluded_sensitive.len(),
                    plan.not_selected.len(),
                    plan.denied.len()
                ),
                changed: false,
            });
        }
        let approved = if yes {
            let _ = writeln!(
                output,
                "bitty plugin: --yes applied profile '{}' v{CONSENT_PROFILE_VERSION} for '{id}': {} capabilit{}",
                profile.name(),
                plan.selected.len(),
                if plan.selected.len() == 1 { "y" } else { "ies" }
            );
            true
        } else {
            ask_yes_no(
                input,
                output,
                &format!(
                    "Apply profile '{}' (grant {} capabilit{})? [y/N] ",
                    profile.name(),
                    plan.selected.len(),
                    if plan.selected.len() == 1 { "y" } else { "ies" }
                ),
            )?
        };
        if !approved {
            return Err(PluginFailure::generic(
                "ConsentDeclined",
                format!(
                    "bitty plugin: capability profile '{}' for '{id}' was not approved — nothing changed",
                    profile.name()
                ),
            ));
        }
        let granted_after = plan.granted_after(&record.granted);
        let added = plan.selected.len();
        let entry = state.get_mut(id).expect("checked");
        entry.granted = granted_after;
        let complete = requested.is_subset(&entry.granted);
        return Ok(OpOutput {
            summary: format!(
                "bitty plugin: applied profile '{}' v{} to '{id}' (+{added} capabilit{}; {} of {} granted; {})",
                profile.name(),
                plan.profile_version,
                if added == 1 { "y" } else { "ies" },
                entry.granted.len(),
                requested.len(),
                if complete {
                    format!("re-run `bitty plugin enable {id}` to enable")
                } else {
                    format!(
                        "{} requested capabilit{} still need explicit per-item consent",
                        requested.difference(&entry.granted).count(),
                        if requested.difference(&entry.granted).count() == 1 {
                            "y"
                        } else {
                            "ies"
                        }
                    )
                }
            ),
            changed: true,
        });
    }

    let Some(raw) = capability else {
        return Err(PluginFailure::usage(
            "bitty plugin: `grant` needs --profile <name> or --cap <capability>".to_string(),
        ));
    };
    let capability = CapabilityId::parse(raw).map_err(|error| {
        PluginFailure::usage(format!("bitty plugin: invalid capability: {error}"))
    })?;
    if !requested.contains(&capability) {
        return Err(PluginFailure::plugin(
            "CapabilityNotRequested",
            format!(
                "bitty plugin: '{}' is not requested by '{id}' (grants never exceed the manifest; \
                 review `bitty plugin info {id}`)",
                capability.as_str()
            ),
        ));
    }
    if record.granted.contains(&capability) {
        return Ok(OpOutput {
            summary: format!(
                "bitty plugin: '{}' is already granted for '{id}'",
                capability.as_str()
            ),
            changed: false,
        });
    }
    // Preserve the existing denial path: only a full `install` re-consent
    // clears a durable per-capability denial. `grant --cap` is narrow and
    // must never become a deny-loop bypass.
    if record.denied.contains(&capability) {
        return Err(PluginFailure::plugin(
            "CapabilityDenied",
            format!(
                "bitty plugin: '{}' is explicitly denied for '{id}'; \
                 re-run `bitty plugin install {id}` and approve the capability to re-grant it",
                capability.as_str()
            ),
        ));
    }
    print_capability_review(output, id, &manifest_hash, &capability);
    let approved = if yes {
        let _ = writeln!(
            output,
            "bitty plugin: --yes granted '{}' for '{id}'",
            capability.as_str()
        );
        true
    } else {
        ask_yes_no(
            input,
            output,
            &format!("Grant '{}'? [y/N] ", capability.as_str()),
        )?
    };
    if !approved {
        return Err(PluginFailure::generic(
            "ConsentDeclined",
            format!(
                "bitty plugin: capability grant for '{}' was not approved — nothing changed",
                capability.as_str()
            ),
        ));
    }
    let entry = state.get_mut(id).expect("checked");
    entry.granted.insert(capability.clone());
    let complete = requested.is_subset(&entry.granted);
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: granted '{}' for '{id}' ({} of {} granted; {})",
            capability.as_str(),
            entry.granted.len(),
            requested.len(),
            if complete {
                format!("re-run `bitty plugin enable {id}` to enable")
            } else {
                "remaining requested capabilities still need explicit per-item consent".to_string()
            }
        ),
        changed: true,
    })
}

/// Render the profile diff/review screen before anything is applied.
fn print_profile_review(
    output: &mut dyn std::io::Write,
    id: &str,
    manifest_hash: &str,
    plan: &ProfileSelection,
) {
    let _ = writeln!(
        output,
        "bitty plugin: capability profile '{}' v{} for '{id}' — {} (manifest {}):",
        plan.profile.name(),
        plan.profile_version,
        plan.profile.summary(),
        short_hash(manifest_hash)
    );
    let _ = writeln!(output, "  -- review before apply --");
    if plan.selected.is_empty() {
        let _ = writeln!(output, "  + will grant: none");
    } else {
        let _ = writeln!(output, "  + will grant ({}):", plan.selected.len());
        for capability in &plan.selected {
            let _ = writeln!(
                output,
                "    + {} — {}",
                capability.as_str(),
                effect_statement(capability)
            );
        }
    }
    if !plan.already_granted.is_empty() {
        let ids: Vec<&str> = plan
            .already_granted
            .iter()
            .map(CapabilityId::as_str)
            .collect();
        let _ = writeln!(
            output,
            "  = already granted, unchanged ({}): {}",
            ids.len(),
            ids.join(", ")
        );
    }
    if !plan.excluded_sensitive.is_empty() {
        let _ = writeln!(
            output,
            "  ! explicit per-item consent required ({}):",
            plan.excluded_sensitive.len()
        );
        for capability in &plan.excluded_sensitive {
            let _ = writeln!(
                output,
                "    ! {} — {}; grant explicitly with `bitty plugin grant {id} --cap {}`",
                capability.as_str(),
                effect_statement(capability),
                capability.as_str()
            );
        }
    }
    if !plan.not_selected.is_empty() {
        let ids: Vec<&str> = plan.not_selected.iter().map(CapabilityId::as_str).collect();
        let _ = writeln!(
            output,
            "  - not selected by this profile ({}): {}",
            ids.len(),
            ids.join(", ")
        );
    }
    if !plan.denied.is_empty() {
        let ids: Vec<&str> = plan.denied.iter().map(CapabilityId::as_str).collect();
        let _ = writeln!(
            output,
            "  x explicitly denied, profile will not clear ({}): {}",
            ids.len(),
            ids.join(", ")
        );
    }
    if !plan.completes_requested() {
        let _ = writeln!(
            output,
            "  note: after apply the plugin stays disabled until every requested capability is granted"
        );
    }
}

/// Render the single-capability review screen used by explicit `--cap` grants.
fn print_capability_review(
    output: &mut dyn std::io::Write,
    id: &str,
    manifest_hash: &str,
    capability: &CapabilityId,
) {
    let risk = if capability.is_high_risk() {
        " (high risk)"
    } else {
        ""
    };
    let _ = writeln!(
        output,
        "bitty plugin: explicit capability grant for '{id}' (manifest {}):",
        short_hash(manifest_hash)
    );
    let _ = writeln!(
        output,
        "  + {}{risk} — {}",
        capability.as_str(),
        effect_statement(capability)
    );
}

/// Refuse `--profile` for store packages (official/local/git/archive installs).
///
/// The store package pipeline records the manifest's complete requested set
/// and owns its own consent/rollback transaction. Profiles are a preselection
/// over the managed bundled ledger only; pretending to narrow a store grant
/// here would either be a no-op or bypass that transaction, so it fails
/// closed instead.
fn reject_store_profile(operand: &str, output: &mut dyn std::io::Write) -> i32 {
    let _ = writeln!(
        output,
        "bitty plugin: --profile applies to bundled managed plugins only; '{operand}' is a store package"
    );
    eprintln!(
        "bitty plugin: install '{operand}' without --profile to review its full capability set, \
         or use `bitty plugin grant <bundled-id> --profile <name>` for bundled records"
    );
    EXIT_USAGE
}

/// Ask the interactive capability-consent question (fails closed).
///
/// Returns `Ok(true)` when the user approves, `Ok(false)` on an explicit
/// decline, and `Err` when input ends before an answer (aborted). The read
/// itself is bounded (bitty#1919, same class as #1916): at most
/// `MAX_CONSENT_LINE_BYTES + 2` bytes are buffered per answer (room for a
/// 64-byte answer plus `\r\n`); an unterminated longer line is rejected as
/// overlong after its remainder is discarded through bounded `fill_buf`
/// takes, so a large piped answer cannot grow memory without bound and no
/// byte-index truncation can split a UTF-8 character.
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
        let Some(answer) = read_plugin_consent_answer(input, output, attempt) else {
            continue;
        };
        let answer = answer?;
        match answer.trim().to_ascii_lowercase().as_str() {
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
    Err(PluginFailure::generic(
        "ConsentAborted",
        format!(
            "bitty plugin: aborted (too many invalid answers, limit {MAX_CONSENT_ATTEMPTS}) — nothing changed"
        ),
    ))
}

/// Read one consent answer with a bounded buffer.
///
/// Twin of `component.rs` `read_consent_answer` (bitty#1916); keep the two in
/// sync. Returns `None` when the answer was overlong (already reported; the
/// caller retries without consuming an extra attempt branch), otherwise
/// `Some(Ok(line))` for a complete line or `Some(Err(..))` on EOF/read
/// error/invalid UTF-8 (fail closed, nothing changed).
fn read_plugin_consent_answer(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    attempt: usize,
) -> Option<Result<String, PluginFailure>> {
    // Window: a 64-byte answer plus a `\r\n` terminator. A line that does
    // not terminate inside the window is overlong and rejected. Bytes are
    // taken via `fill_buf`/`consume` (never a wide `read`) so a single
    // answer never swallows the start of the next line.
    const WINDOW: usize = MAX_CONSENT_LINE_BYTES + 2;
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
        return Some(Err(PluginFailure::generic(
            "ConsentAborted",
            "bitty plugin: aborted (end of input) — nothing changed".to_string(),
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
            "  (answer too long — at most {MAX_CONSENT_LINE_BYTES} bytes — try again [{attempt}/{MAX_CONSENT_ATTEMPTS}])"
        );
        return None;
    }
    match String::from_utf8(answer) {
        Ok(line) => Some(Ok(line)),
        Err(_) => Some(Err(PluginFailure::generic(
            "ConsentAborted",
            "bitty plugin: aborted (end of input) — nothing changed".to_string(),
        ))),
    }
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

    /// [`run`] with injected XDG config and project cwd (policy-layer tests).
    fn run_with_env(
        raw: &[&str],
        config_path: &str,
        xdg_config_home: Option<&str>,
        cwd: Option<&str>,
        input: &str,
    ) -> (i32, String) {
        let context = PluginContext {
            config_path: Some(config_path),
            xdg_config_home,
            cwd,
            ..PluginContext::default()
        };
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        let mut input = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let code = run_plugin_subcommand(&owned, &context, &mut input, &mut output);
        (code, String::from_utf8(output).expect("utf-8"))
    }

    fn state_path(dir: &Path) -> PathBuf {
        dir.join(STATE_FILE_NAME)
    }

    // ── request parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_config_actions_and_aliases() {
        let request = parse_ok(&["config", "show", "xuepoo.hello"]);
        assert_eq!(request.verb, PluginVerb::Config);
        assert_eq!(request.config_action, Some(ConfigAction::Show));
        assert_eq!(request.id.as_deref(), Some("xuepoo.hello"));

        let request = parse_ok(&["config", "scaffold", "xuepoo.hello"]);
        assert_eq!(request.config_action, Some(ConfigAction::Scaffold));

        // Issue shape: `config <id>` inspects, `config <id> --init` scaffolds.
        let request = parse_ok(&["config", "xuepoo.hello"]);
        assert_eq!(request.config_action, Some(ConfigAction::Show));
        let request = parse_ok(&["config", "xuepoo.hello", "--init"]);
        assert_eq!(request.config_action, Some(ConfigAction::Scaffold));

        let request = parse_ok(&["config", "show", "xuepoo.hello", "--json"]);
        assert_eq!(request.format, PluginFormat::Json);
        let request = parse_ok(&["config", "scaffold", "xuepoo.hello", "--force"]);
        assert!(request.force);
    }

    #[test]
    fn parse_config_rejects_conflicting_flags() {
        assert!(parse_error(&["config", "show", "xuepoo.hello", "--init"]).contains("conflicts"));
        assert!(parse_error(&["config", "show", "xuepoo.hello", "--force"]).contains("--force"));
        assert!(parse_error(&["list", "--init"]).contains("--init"));
        assert!(
            parse_error(&["config", "scaffold", "xuepoo.hello", "--json"]).contains("--format")
        );
        assert!(parse_error(&["config"]).contains("needs a plugin id"));
        assert!(parse_error(&["config", "show"]).contains("needs a plugin id"));
    }

    fn config_decls() -> Vec<SettingDecl> {
        vec![
            SettingDecl::new(
                "show".to_string(),
                bitty_plugin_host::manifest::SettingKind::Bool,
                SettingDefault::Bool(true),
            )
            .expect("decl"),
            SettingDecl::new(
                "edge".to_string(),
                bitty_plugin_host::manifest::SettingKind::String,
                SettingDefault::String("bottom".to_string()),
            )
            .expect("decl"),
            SettingDecl::new(
                "modules".to_string(),
                bitty_plugin_host::manifest::SettingKind::StringArray,
                SettingDefault::Strings(vec!["workspaces".to_string(), "clock".to_string()]),
            )
            .expect("decl"),
        ]
    }

    #[test]
    fn config_scaffold_writes_canonical_lua_and_refuses_overwrite() {
        let dir = scratch_dir("config-scaffold");
        let target = write_config_scaffold(&dir, "xuepoo.cfg", &config_decls(), false)
            .expect("scaffold writes");
        assert_eq!(target, dir.join("plugins").join("xuepoo.cfg.lua"));
        let body = std::fs::read_to_string(&target).expect("scaffold body");
        assert!(body.contains("return {"), "{body}");
        assert!(body.contains("show = true,"), "{body}");
        assert!(body.contains("edge = \"bottom\","), "{body}");
        assert!(
            body.contains("modules = { \"workspaces\", \"clock\" },"),
            "{body}"
        );
        // The template is valid plain settings Lua and round-trips.
        let mut vm = bitty_lua::ConfigLuaVm::with_limits(
            "xuepoo.cfg",
            bitty_lua::PlainConfigLimits::default(),
        )
        .expect("vm");
        let parsed = vm.eval_plain_config(&body).expect("template parses");
        let bitty_lua::PlainConfigValue::Table(values) = parsed else {
            panic!("template must return a table");
        };
        assert_eq!(
            values.get("show"),
            Some(&bitty_lua::PlainConfigValue::Bool(true))
        );
        assert_eq!(
            values.get("edge"),
            Some(&bitty_lua::PlainConfigValue::String("bottom".to_string()))
        );

        // Refuse to clobber without --force; the bytes stay untouched.
        let failure = write_config_scaffold(&dir, "xuepoo.cfg", &config_decls(), false)
            .expect_err("overwrite refused");
        assert_eq!(failure.code, "AlreadyExists");
        assert_eq!(failure.exit, EXIT_PLUGIN);
        assert_eq!(std::fs::read_to_string(&target).expect("unchanged"), body);

        // --force rewrites atomically and leaves no temp files behind.
        let rewritten = write_config_scaffold(&dir, "xuepoo.cfg", &config_decls(), true)
            .expect("force overwrite");
        assert_eq!(rewritten, target);
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("plugins"))
            .expect("plugins dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SCAFFOLD_TEMP_PREFIX)
            })
            .collect();
        assert!(leftovers.is_empty(), "temp files must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_scaffold_refuses_no_settings_and_unsafe_ids() {
        let dir = scratch_dir("config-scaffold-empty");
        let failure =
            write_config_scaffold(&dir, "xuepoo.cfg", &[], false).expect_err("no settings refused");
        assert_eq!(failure.code, "NoSettings");
        let failure = write_config_scaffold(&dir, "../escape", &config_decls(), false)
            .expect_err("unsafe id refused");
        assert_eq!(failure.code, "UnsafePath");
        assert!(!dir.join("plugins").join("xuepoo.cfg.lua").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn config_scaffold_refuses_symlink_target_and_symlink_dir() {
        let dir = scratch_dir("config-scaffold-symlink");
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).expect("plugins dir");
        let outside = dir.join("outside.lua");
        std::fs::write(&outside, "return { show = false }\n").expect("outside target");
        let target = plugins.join("xuepoo.cfg.lua");
        std::os::unix::fs::symlink(&outside, &target).expect("symlink target");
        let failure = write_config_scaffold(&dir, "xuepoo.cfg", &config_decls(), true)
            .expect_err("symlink refused even with force");
        assert_eq!(failure.code, "SymlinkTarget");
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside unchanged"),
            "return { show = false }\n"
        );

        std::fs::remove_file(&target).expect("remove target symlink");
        let evil = scratch_dir("config-scaffold-evil");
        std::fs::remove_dir(&plugins).expect("remove plugins dir");
        std::os::unix::fs::symlink(&evil, &plugins).expect("symlink plugins dir");
        let failure = write_config_scaffold(&dir, "xuepoo.cfg", &config_decls(), false)
            .expect_err("symlinked plugins dir refused");
        assert_eq!(failure.code, "SymlinkTarget");
        assert!(!evil.join("xuepoo.cfg.lua").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&evil);
    }

    #[test]
    fn config_scaffold_escapes_hostile_string_defaults() {
        let decls = vec![
            SettingDecl::new(
                "text".to_string(),
                bitty_plugin_host::manifest::SettingKind::String,
                SettingDefault::String("a\"b\\c\nd\te\u{1b}".to_string()),
            )
            .expect("decl"),
        ];
        let rendered = render_config_scaffold("xuepoo.cfg", &decls);
        assert!(rendered.contains("\\\""), "{rendered}");
        assert!(rendered.contains("\\\\"), "{rendered}");
        assert!(rendered.contains("\\n"), "{rendered}");
        assert!(rendered.contains("\\t"), "{rendered}");
        assert!(rendered.contains("\\027"), "{rendered}");
        assert!(!rendered.contains('\u{1b}'), "{rendered}");
        let mut vm = bitty_lua::ConfigLuaVm::with_limits(
            "xuepoo.cfg",
            bitty_lua::PlainConfigLimits::default(),
        )
        .expect("vm");
        let parsed = vm
            .eval_plain_config(&rendered)
            .expect("escaped template parses");
        let bitty_lua::PlainConfigValue::Table(values) = parsed else {
            panic!("template must return a table");
        };
        assert_eq!(
            values.get("text"),
            Some(&bitty_lua::PlainConfigValue::String(
                "a\"b\\c\nd\te\u{1b}".to_string()
            ))
        );
    }

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
        let request = parse_ok(&["install", "bitty-terminal.workspace", "--yes"]);
        assert_eq!(request.verb, PluginVerb::Install);
        assert_eq!(request.id.as_deref(), Some("bitty-terminal.workspace"));
        assert!(request.yes);

        let request = parse_ok(&["list", "--format", "json"]);
        assert_eq!(request.format, PluginFormat::Json);
        let request = parse_ok(&["info", "bitty-terminal.workspace", "--format=jsonl"]);
        assert_eq!(request.format, PluginFormat::Jsonl);
        let request = parse_ok(&["remove", "bitty-terminal.workspace", "--force"]);
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
        assert!(parse_error(&["remove", "a", "--no-color"]).contains("--no-color only applies"));
        assert!(parse_error(&["list", "--repair"]).contains("--repair only applies"));
        assert!(parse_error(&["install", "a", "--repair"]).contains("--repair only applies"));
    }

    #[test]
    fn parse_doctor_and_install_force() {
        let doctor = parse_ok(&["doctor"]);
        assert_eq!(doctor.verb, PluginVerb::Doctor);
        assert!(!doctor.repair);
        let repair = parse_ok(&["doctor", "--repair"]);
        assert!(repair.repair);
        let plain = parse_ok(&["doctor", "--no-color"]);
        assert!(plain.no_color, "doctor accepts --no-color");
        let forced = parse_ok(&["install", "./plugin", "--force"]);
        assert!(forced.force);
        assert!(
            parse_error(&["doctor", "extra"]).contains("takes no plugin id"),
            "doctor takes no operand"
        );
    }

    #[test]
    fn parse_explain_takes_id_and_capability() {
        let request = parse_ok(&[
            "explain",
            "bitty-terminal.shell-integration",
            "terminal.semantic-read",
        ]);
        assert_eq!(request.verb, PluginVerb::Explain);
        assert_eq!(
            request.id.as_deref(),
            Some("bitty-terminal.shell-integration")
        );
        assert_eq!(
            request.capability.as_deref(),
            Some("terminal.semantic-read")
        );
        assert_eq!(request.format, PluginFormat::Table);

        let json = parse_ok(&["explain", "owner.name", "ui.rich", "--format=json"]);
        assert_eq!(json.format, PluginFormat::Json);
        assert_eq!(json.capability.as_deref(), Some("ui.rich"));

        assert!(parse_error(&["explain"]).contains("needs a plugin id"));
        assert!(parse_error(&["explain", "owner.name"]).contains("needs a capability"));
        assert!(
            parse_error(&["explain", "owner.name", "ui.rich", "extra"]).contains("extra argument")
        );
        // A second operand only exists for `explain`.
        assert!(parse_error(&["info", "owner.name", "ui.rich"]).contains("extra argument"));
    }

    #[test]
    fn help_and_usage_name_explain() {
        let help = plugin_help_text();
        assert!(help.contains("explain <id> <capability>"), "{help}");
        assert!(help.contains("capability decision chain"), "{help}");
        let usage = plugin_usage();
        assert!(usage.contains("explain <id> <capability>"), "{usage}");
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
            "bitty-terminal.workspace".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "ab".repeat(32),
                enabled: true,
                granted,
                denied: BTreeSet::new(),
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
        assert!(rendered.contains("[plugins.\"bitty-terminal.workspace\"]"));
        assert!(rendered.contains("granted = [\"terminal.semantic-read\"]"));
        // Empty state renders and parses.
        let empty = PluginState::new().render();
        assert_eq!(
            PluginState::parse(&empty).expect("empty parses"),
            PluginState::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_manifest_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch_dir("state-mode");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("seed permissive scratch dir");
        let path = state_path(&dir);

        let backup = save_state(&path, &sample_state()).expect("first save");
        assert!(backup.is_none(), "first save has nothing to back up");
        let manifest_mode = std::fs::metadata(&path)
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(manifest_mode, 0o600, "managed manifest must be owner-only");
        let dir_mode = std::fs::metadata(&dir)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "managed directory must be owner-only");

        let backup = save_state(&path, &sample_state())
            .expect("second save")
            .expect("second save backs up the previous manifest");
        let backup_mode = std::fs::metadata(&backup)
            .expect("backup metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(backup_mode, 0o600, "backup must be owner-only");
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

    // ── explain (issue #2471) ─────────────────────────────────────────────

    const EXPLAIN_CAP: &str = "terminal.semantic-read";
    const EXPLAIN_PLUGIN: &str = "bitty-terminal.shell-integration";

    #[test]
    fn explain_allow_path_prints_layers_and_effective_grant() {
        let dir = scratch_dir("explain-allow");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");

        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("decision:     allow"), "{out}");
        assert!(out.contains("first denying layer: none"), "{out}");
        assert!(out.contains("trust:        bundled-lua (level 1"), "{out}");
        assert!(out.contains("layers (highest authority first):"), "{out}");
        for layer in [
            "host-ceiling",
            "user-policy",
            "project-policy",
            "parent-delegation",
            "task-grant",
            "agent-request",
        ] {
            assert!(out.contains(layer), "missing {layer}: {out}");
        }
        assert!(out.contains("task-grant"), "{out}");
        assert!(out.contains("effective:    allow"), "{out}");
        assert!(out.contains("pins:         none"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_deny_path_selects_the_task_grant_and_prints_the_chain() {
        let dir = scratch_dir("explain-deny");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");
        let (code, out, _) = run(
            &["revoke", EXPLAIN_PLUGIN, "--cap", EXPLAIN_CAP],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");

        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("decision:     deny"), "{out}");
        assert!(out.contains("first denying layer: task-grant"), "{out}");
        assert!(out.contains("denial kind:  exceeds-policy"), "{out}");
        assert!(out.contains("reason chain:"), "{out}");
        assert!(out.contains("remediation:"), "{out}");
        assert!(out.contains("bitty plugin install"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_user_policy_is_the_first_denying_layer() {
        let dir = scratch_dir("explain-user-policy");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");

        // A constraining user policy that omits the capability denies by
        // omission before the task grant (which allows it) is considered.
        let xdg = scratch_dir("explain-user-policy-xdg");
        let policy_dir = xdg.join("bitty");
        std::fs::create_dir_all(&policy_dir).expect("policy dir");
        std::fs::write(policy_dir.join("capabilities.conf"), "allow ui.rich\n")
            .expect("policy file");

        let xdg_text = xdg.display().to_string();
        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP],
            &config,
            Some(&xdg_text),
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("first denying layer: user-policy"), "{out}");
        assert!(
            out.contains("capability not listed (deny-by-default)"),
            "{out}"
        );
        assert!(out.contains("policy layers are advisory"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&xdg);
    }

    #[test]
    fn explain_unknown_id_capability_and_malformed_input_fail_closed() {
        let dir = scratch_dir("explain-unknown");
        let config = dir.join("init.lua").display().to_string();

        // Unknown plugin: plugin error, never a panic. JSON keeps the
        // actionable message on stdout for this unit-level capture.
        let (code, out) = run_with_env(
            &["explain", "no.such-plugin", EXPLAIN_CAP, "--json"],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("\"code\":\"PluginNotFound\""), "{out}");
        assert!(out.contains("unknown plugin 'no.such-plugin'"), "{out}");

        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");

        // Valid capability grammar, not requested by this manifest.
        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, "ui.rich", "--json"],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("\"code\":\"UnknownCapability\""), "{out}");
        assert!(out.contains("does not declare capability"), "{out}");
        assert!(out.contains(EXPLAIN_CAP), "{out}");

        // Malformed capability grammar is a usage error.
        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, "FS-READ", "--json"],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_USAGE, "{out}");
        assert!(out.contains("\"class\":\"UsageError\""), "{out}");
        assert!(out.contains("invalid capability"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_json_envelope_has_stable_shape_and_escapes() {
        let dir = scratch_dir("explain-json");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");

        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP, "--format", "json"],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(
            out.starts_with("{\"v\":1,\"command\":\"plugin\",\"ok\":true"),
            "{out}"
        );
        assert!(out.contains("\"verb\":\"explain\""), "{out}");
        assert!(
            out.contains("\"capability\":\"terminal.semantic-read\""),
            "{out}"
        );
        assert!(out.contains("\"decision\":\"allow\""), "{out}");
        assert!(out.contains("\"first_denying_layer\":null"), "{out}");
        assert!(out.contains("\"denial_kind\":null"), "{out}");
        assert!(
            out.contains("\"trust\":{\"level\":\"bundled-lua\",\"number\":1"),
            "{out}"
        );
        assert!(out.contains("\"layers\":["), "{out}");
        assert!(
            out.contains(
                "\"layer\":\"task-grant\",\"config_layer\":\"runtime-task\",\"state\":\"allow\""
            ),
            "{out}"
        );
        assert!(out.contains("\"budget\":{\"observable\":false"), "{out}");
        assert!(out.contains("\"policy_note\":null"), "{out}");
        assert!(
            out.contains("\"enforcement\":{\"request_kind\":\"plugin.lifecycle\""),
            "{out}"
        );

        let (code, out, _) = run(
            &["revoke", EXPLAIN_PLUGIN, "--cap", EXPLAIN_CAP],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP, "--json"],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("\"decision\":\"deny\""), "{out}");
        assert!(
            out.contains("\"first_denying_layer\":\"task-grant\""),
            "{out}"
        );
        assert!(out.contains("\"denial_kind\":\"exceeds-policy\""), "{out}");
        assert!(out.contains("\"remediation\":["), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_reports_a_stale_grant_pin_as_policy_conflict() {
        let dir = scratch_dir("explain-pin");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");
        let file = state_path(&dir);
        let text = std::fs::read_to_string(&file).expect("state");
        // Rewrite the pin to a different (valid) hash: the grant is bound to
        // the old manifest, so the diagnostic must fail closed as a conflict.
        let stale = "0".repeat(64);
        let rewritten = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("manifest_hash = ") {
                    format!("\tmanifest_hash = \"{stale}\"")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file, rewritten).expect("rewrite");

        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP],
            &config,
            None,
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("first denying layer: task-grant"), "{out}");
        assert!(out.contains("denial kind:  policy-conflict"), "{out}");
        assert!(out.contains("manifest hash pin mismatch"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_table_escapes_hostile_dynamic_cells_without_color() {
        let escaped = table_cell("/tmp/evil\x1b[31m\r\nnext");
        assert!(!escaped.chars().any(char::is_control));
        assert!(escaped.contains("\\u{1b}"));
        assert!(escaped.contains("\\r\\n"));
    }

    #[test]
    fn long_unicode_policy_path_fails_closed_without_panicking() {
        let dir = scratch_dir("explain-unicode-policy");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");
        let tag = "é".repeat(90);
        let xdg = scratch_dir(&tag);
        let policy_dir = xdg.join("bitty");
        std::fs::create_dir_all(&policy_dir).expect("policy dir");
        std::fs::write(policy_dir.join("capabilities.conf"), "allow ui.rich\n").expect("policy");
        let xdg_text = xdg.display().to_string();
        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP],
            &config,
            Some(&xdg_text),
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("first denying layer: user-policy"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&xdg);
    }

    #[test]
    fn bounded_reader_rejects_symlinks_and_special_files() {
        let dir = scratch_dir("explain-bounded-reader");
        let target = dir.join("target");
        #[cfg(unix)]
        let link = dir.join("link");
        std::fs::write(&target, "allow ui.rich\n").expect("target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("link");
        #[cfg(unix)]
        assert!(matches!(
            read_bounded_regular_file(&link, MAX_POLICY_FILE_BYTES),
            Err(BoundedReadError::Unsafe(_))
        ));
        #[cfg(unix)]
        {
            let nested_link = dir.join("nested-link");
            let nested_target = dir.join("nested-target");
            std::fs::create_dir(&nested_target).expect("nested target");
            std::fs::write(nested_target.join("policy"), "allow ui.rich\n").expect("nested policy");
            std::os::unix::fs::symlink(&nested_target, &nested_link).expect("nested link");
            assert!(matches!(
                read_bounded_regular_file(&nested_link.join("policy"), MAX_POLICY_FILE_BYTES),
                Err(BoundedReadError::Unsafe(_))
            ));
            let fifo = dir.join("fifo");
            nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).expect("fifo");
            assert!(matches!(
                read_bounded_regular_file(&fifo, MAX_POLICY_FILE_BYTES),
                Err(BoundedReadError::Unsafe(_))
            ));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_reader_rejects_replaced_symlink_without_mutating_target() {
        let dir = scratch_dir("explain-state-symlink");
        let target = dir.join("real.toml");
        #[cfg(unix)]
        let link = dir.join(STATE_FILE_NAME);
        std::fs::write(&target, "state_version = 1\n").expect("target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("link");
        #[cfg(unix)]
        assert!(load_state(&link).is_err());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_to_string(&target).expect("target readable"),
            "state_version = 1\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_rejects_ledger_only_records_before_using_grants() {
        let capability = CapabilityId::parse(EXPLAIN_CAP).expect("capability");
        let state = PluginState::from_records(
            std::iter::once((
                "third.party".to_string(),
                PluginRecord {
                    source: "registry".to_string(),
                    manifest_hash: "a".repeat(64),
                    enabled: true,
                    granted: [capability].into_iter().collect(),
                    denied: BTreeSet::new(),
                },
            ))
            .collect(),
        );
        let error = explain_row_for_id(&state, None, "third.party")
            .expect_err("ledger-only grants cannot authorize explain");
        assert_eq!(error.code, "PluginUnusable");
    }

    #[test]
    fn stale_pin_does_not_replace_an_earlier_policy_denial() {
        let dir = scratch_dir("explain-stale-policy");
        let config = dir.join("init.lua").display().to_string();
        let (code, out, _) = run(&["install", EXPLAIN_PLUGIN, "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_OK, "{out}");
        let xdg = scratch_dir("explain-stale-policy-xdg");
        let policy_dir = xdg.join("bitty");
        std::fs::create_dir_all(&policy_dir).expect("policy dir");
        std::fs::write(policy_dir.join("capabilities.conf"), "allow ui.rich\n").expect("policy");
        let file = state_path(&dir);
        let text = std::fs::read_to_string(&file).expect("state");
        let stale = "0".repeat(64);
        std::fs::write(
            &file,
            text.lines()
                .map(|line| {
                    if line.trim_start().starts_with("manifest_hash = ") {
                        format!("\tmanifest_hash = \"{stale}\"")
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("stale state");
        let xdg_text = xdg.display().to_string();
        let (code, out) = run_with_env(
            &["explain", EXPLAIN_PLUGIN, EXPLAIN_CAP],
            &config,
            Some(&xdg_text),
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.contains("first denying layer: user-policy"), "{out}");
        assert!(out.contains("manifest hash pin mismatch"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&xdg);
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
    fn init_offline_never_clobbers_existing_records() {
        // CodeRabbit 4241676524: a pre-existing record with grants + a stale
        // hash must survive `init --plugins` untouched (no silent disable,
        // no grant/denial loss); hash transitions belong to the install
        // consent flow. Absent ids still get fresh disabled records.
        let dir = scratch_dir("init-noclobber");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        let path = crate::plugin::init_plugin_state_path(
            Some(&config),
            None,
            None,
            Some(&dir.display().to_string()),
        )
        .expect("state path resolves");
        let id = "bitty-terminal.shell-integration".to_string();

        // Seed a user record: enabled with a grant + denial under a stale hash.
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").expect("cap"));
        let mut denied = BTreeSet::new();
        denied.insert(CapabilityId::parse("ui.rich").expect("cap"));
        let mut records = BTreeMap::new();
        records.insert(
            id.clone(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "00".repeat(32),
                enabled: true,
                granted: granted.clone(),
                denied: denied.clone(),
            },
        );
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, PluginState::from_records(records).render()).expect("seed");
        let before = std::fs::read_to_string(&path).expect("seed readable");

        // Offline recording must skip the existing id (0 newly recorded).
        let recorded =
            init_record_bundled_offline(&path, std::slice::from_ref(&id)).expect("offline ok");
        assert_eq!(recorded, 0);
        let after = std::fs::read_to_string(&path).expect("state readable");
        assert_eq!(after, before, "existing record untouched");
        let state = PluginState::parse(&after).expect("parses");
        let kept = state.get(&id).expect("record kept");
        assert!(kept.enabled, "enabled preserved");
        assert_eq!(kept.granted, granted, "grants preserved");
        assert_eq!(kept.denied, denied, "denials preserved");
        assert_eq!(kept.manifest_hash, "00".repeat(32), "stale hash preserved");

        // Absent ids still get fresh disabled records as today.
        std::fs::remove_file(&path).expect("clear");
        let recorded =
            init_record_bundled_offline(&path, std::slice::from_ref(&id)).expect("offline ok");
        assert_eq!(recorded, 1);
        let state =
            PluginState::parse(&std::fs::read_to_string(&path).expect("readable")).expect("parses");
        let fresh = state.get(&id).expect("fresh record");
        assert!(!fresh.enabled, "fresh record disabled");
        assert!(fresh.granted.is_empty() && fresh.denied.is_empty());
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
                denied: BTreeSet::new(),
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

    fn consent_fixture() -> (PluginManifest, BTreeSet<CapabilityId>) {
        let manifest = bundled_manifest_for("bitty-terminal.shell-integration").expect("bundled");
        let mut needed = BTreeSet::new();
        needed.insert(CapabilityId::parse("terminal.raw-read").expect("known high risk"));
        (manifest, needed)
    }

    #[test]
    fn consent_overlong_line_is_rejected_and_stream_resyncs() {
        // A 500-byte piped answer must not grow memory without bound: it
        // is rejected as overlong, its remainder discarded, and the next
        // queued answer (`y`) still applies.
        let (manifest, needed) = consent_fixture();
        let mut output = Vec::new();
        let stdin = format!("{}\ny\n", "x".repeat(500));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("resyncs to the queued approval");
        assert!(approved);
        let shown = String::from_utf8(output).expect("utf-8");
        assert!(shown.contains("too long"), "{shown}");
    }

    #[test]
    fn consent_multibyte_at_boundary_does_not_panic() {
        // 63 ASCII bytes + `e` with acute (2 bytes) put byte 64
        // mid-character: the old `truncate(64)` panicked here. The bounded
        // read evaluates the full line instead (invalid answer, no panic),
        // then the queued `n` declines.
        let (manifest, needed) = consent_fixture();
        let mut output = Vec::new();
        let stdin = format!("{}é\nn\n", "a".repeat(63));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("boundary answer must not panic");
        assert!(!approved);
    }

    #[test]
    fn consent_short_eof_terminated_line_still_counts() {
        // `printf 'n'` (no trailing newline) declines exactly like `n\n`:
        // a short EOF-terminated line is a complete answer, not overlong.
        let (manifest, needed) = consent_fixture();
        let mut output = Vec::new();
        let mut input = std::io::BufReader::new(&b"n"[..]);
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("short EOF-terminated answer counts");
        assert!(!approved);
        let shown = String::from_utf8(output).expect("utf-8");
        assert!(!shown.contains("too long"), "{shown}");
    }

    // ── revocation (CTX-0765, #1381) ────────────────────────────────────────

    #[test]
    fn parse_revoke_with_cap_pair_and_equals_forms() {
        let request = parse_ok(&["revoke", "bitty-terminal.workspace", "--cap", "ui.rich"]);
        assert_eq!(request.verb, PluginVerb::Revoke);
        assert_eq!(request.id.as_deref(), Some("bitty-terminal.workspace"));
        assert_eq!(request.cap.as_deref(), Some("ui.rich"));
        let request = parse_ok(&["revoke", "bitty-terminal.workspace", "--cap=ui.rich"]);
        assert_eq!(request.cap.as_deref(), Some("ui.rich"));
        let request = parse_ok(&["revoke", "bitty-terminal.workspace"]);
        assert_eq!(request.cap, None);
    }

    #[test]
    fn parse_cap_is_revoke_only() {
        assert!(
            parse_error(&["install", "bitty-terminal.workspace", "--cap", "ui.rich"])
                .contains("--cap only applies to `revoke`")
        );
        assert!(
            parse_error(&["revoke", "bitty-terminal.workspace", "--cap"])
                .contains("--cap needs a value")
        );
        assert!(
            parse_error(&[
                "revoke",
                "bitty-terminal.workspace",
                "--cap",
                "ui.rich",
                "--cap",
                "ui.rich"
            ])
            .contains("duplicate --cap")
        );
        assert!(parse_error(&["revoke"]).contains("needs a plugin id"));
    }

    fn install_shell_integration(config: &str) {
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
    }

    // ── post-install next steps (CTX-1268, bitty#2144) ─────────────────────

    #[test]
    fn next_steps_generic_shape_is_stable() {
        // Golden: no hints, no settings, no config root, bundled install.
        let text = format_next_steps("xuepoo.demo", &[], false, None, None);
        assert!(text.contains("Next steps for 'xuepoo.demo':"), "{text}");
        assert!(
            text.contains("enable: bitty plugin enable xuepoo.demo"),
            "{text}"
        );
        assert!(
            text.contains("installed: bundled with bitty (no package tree)"),
            "{text}"
        );
        assert!(
            text.contains(
                "configure: <config-root>/plugins/xuepoo.demo.lua (no config root resolved)"
            ),
            "{text}"
        );
        assert!(
            text.contains("inspect: bitty plugin config show xuepoo.demo"),
            "{text}"
        );
        assert!(
            text.contains("scaffold: unavailable (no [settings] declared)"),
            "{text}"
        );
        // A settings-less manifest must never advertise a command that
        // cannot act (`config scaffold` exits without writing).
        assert!(
            !text.contains("bitty plugin config scaffold xuepoo.demo"),
            "{text}"
        );
        assert!(!text.contains(".toml"), "{text}");
        assert!(!text.contains("minimal example"), "{text}");
        assert!(!text.contains("hints:"), "{text}");
        assert!(
            text.contains("more: bitty plugin info xuepoo.demo"),
            "{text}"
        );
    }

    #[test]
    fn next_steps_point_at_canonical_lua_not_legacy_toml() {
        let text = format_next_steps(
            "bitty-terminal.bar",
            &[],
            true,
            Some(Path::new(
                "/home/u/.config/bitty/plugins/bitty-terminal.bar.lua",
            )),
            Some(Path::new(
                "/home/u/.local/share/bitty/plugins/packages/bitty-terminal.bar/0.1.0",
            )),
        );
        assert!(
            text.contains("configure: /home/u/.config/bitty/plugins/bitty-terminal.bar.lua"),
            "{text}"
        );
        assert!(
            text.contains(
                "installed: /home/u/.local/share/bitty/plugins/packages/bitty-terminal.bar/0.1.0"
            ),
            "{text}"
        );
        assert!(
            text.contains("inspect: bitty plugin config show bitty-terminal.bar"),
            "{text}"
        );
        assert!(
            text.contains("scaffold: bitty plugin config scaffold bitty-terminal.bar"),
            "{text}"
        );
        // The hint advertises Lua only: no legacy TOML path or example.
        assert!(!text.contains(".toml"), "{text}");
        assert!(!text.contains("minimal example"), "{text}");
        assert!(!text.contains("show ="), "{text}");
        assert!(!text.contains("(undeclared keys are ignored)"), "{text}");
    }

    #[test]
    fn next_steps_scaffold_pointer_follows_declared_settings() {
        // #2384: a settings-less manifest gets a softened line, never a
        // pointer at `config scaffold`, which exits 4 without writing.
        let without = format_next_steps("bitty-terminal.shell-integration", &[], false, None, None);
        assert!(
            without.contains("scaffold: unavailable (no [settings] declared)"),
            "{without}"
        );
        assert!(
            !without.contains("bitty plugin config scaffold bitty-terminal.shell-integration"),
            "{without}"
        );

        // A manifest that declares settings keeps the actionable pointer.
        let with_settings = format_next_steps("xuepoo.settings", &[], true, None, None);
        assert!(
            with_settings.contains(
                "scaffold: bitty plugin config scaffold xuepoo.settings \
                 (writes a commented Lua template)"
            ),
            "{with_settings}"
        );
        assert!(!with_settings.contains("unavailable"), "{with_settings}");
    }

    #[test]
    fn next_steps_hints_append_and_escape_helpers_hold() {
        // Manifest hints render under `hints:`; absent hints stay generic.
        let text = format_next_steps(
            "xuepoo.demo",
            &["Add `modules = [\"clock\"]` to your bar config.".to_string()],
            false,
            None,
            None,
        );
        assert!(text.contains("hints:"), "{text}");
        assert!(text.contains("- Add `modules = [\"clock\"]`"), "{text}");

        // Escaping never emits raw controls or bare quotes.
        assert_eq!(escape_toml_basic("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape_toml_basic("a\nb\rc\td"), "a\\nb\\rc\\td");
        assert_eq!(
            escape_toml_basic("nul\x00esc\x1bdel\x7f"),
            "nul\\u0000esc\\u001Bdel\\u007F"
        );
        assert_eq!(escape_toml_basic("caf\u{e9}\u{2014}"), "caf\u{e9}\u{2014}");
    }

    #[test]
    fn install_and_enable_print_next_steps() {
        let dir = scratch_dir("next-steps");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        // Install success prints the generic next steps (bundled manifest
        // declares no settings and no hints).
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("installed"), "{out}");
        assert!(
            out.contains("Next steps for 'bitty-terminal.shell-integration':"),
            "{out}"
        );
        assert!(
            out.contains("enable: bitty plugin enable bitty-terminal.shell-integration"),
            "{out}"
        );
        // Path separators are platform-native (`\` on Windows): normalize
        // before matching the managed `plugins/` layout. The hint advertises
        // the canonical Lua file, never the legacy TOML fallback.
        let portable = out.replace('\\', "/");
        assert!(
            portable.contains("plugins/bitty-terminal.shell-integration.lua"),
            "{out}"
        );
        assert!(!portable.contains(".toml"), "{out}");
        assert!(
            out.contains("inspect: bitty plugin config show bitty-terminal.shell-integration"),
            "{out}"
        );
        assert!(
            out.contains("scaffold: unavailable (no [settings] declared)"),
            "{out}"
        );
        // #2384: shell-integration declares no `[settings]`, so the hint must
        // not advertise `config scaffold` (it would exit without writing).
        assert!(
            !out.contains("scaffold: bitty plugin config scaffold"),
            "{out}"
        );
        assert!(
            out.contains("more: bitty plugin info bitty-terminal.shell-integration"),
            "{out}"
        );

        // Enable success (after a disable) prints them again.
        let (code, _, _) = run(
            &["disable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        let (code, out, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("enabled"), "{out}");
        assert!(
            out.contains("Next steps for 'bitty-terminal.shell-integration':"),
            "{out}"
        );
        assert!(
            out.contains("enable: bitty plugin enable bitty-terminal.shell-integration"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoke_full_clears_grants_disables_and_is_idempotent() {
        let dir = scratch_dir("revoke-full");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);

        let (code, out, _) = run(
            &["revoke", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("revoked"), "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(text.contains("granted = []"), "{text}");
        assert!(text.contains("enabled = false"), "{text}");
        assert!(text.contains("denied = []"), "{text}");

        // Second revoke is an idempotent no-op (no rewrite, still success).
        let (code, out, _) = run(
            &["revoke", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("already revoked"), "{out}");

        // list reflects the revoked grant count without touching other rows.
        let (code, out, _) = run(&["list"], Some(&config), "");
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("0/1"), "{out}");

        // enable fails closed: the manifest requests more than is granted.
        let (code, _, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoke_cap_records_denial_blocks_enable_and_shows_in_info() {
        let dir = scratch_dir("revoke-cap");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);

        // Single granted capability out: escalates to disabled, denial kept.
        let (code, out, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "terminal.semantic-read",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("denial kept"), "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(text.contains("granted = []"), "{text}");
        assert!(
            text.contains("denied = [\"terminal.semantic-read\"]"),
            "{text}"
        );
        assert!(text.contains("enabled = false"), "{text}");

        // info surfaces the revocation in table and json shapes.
        let (code, out, _) = run(
            &["info", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("[revoked] terminal.semantic-read"), "{out}");
        assert!(out.contains("revoked (1)"), "{out}");
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
        assert!(out.contains("\"denied\":true"), "{out}");

        // enable stays blocked while the denial stands.
        let (code, _, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoke_rejects_unknown_plugin_and_ungranted_capability() {
        let dir = scratch_dir("revoke-reject");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);
        let before = std::fs::read_to_string(&file).expect("state written");

        let (code, _, _) = run(&["revoke", "xuepoo.markdown"], Some(&config), "");
        assert_eq!(code, EXIT_PLUGIN);
        let (code, _, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "ui.rich",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let (code, _, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "not.a.cap!!",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_USAGE);
        assert_eq!(
            std::fs::read_to_string(&file).expect("kept"),
            before,
            "failed revokes change nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn regrant_after_revoke_needs_consent_and_clears_denial() {
        let dir = scratch_dir("regrant");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);
        let (code, _, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "terminal.semantic-read",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);

        // Declined consent changes nothing: the denial persists.
        let (code, _, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "n\n",
        );
        assert_eq!(code, EXIT_GENERIC);
        let text = std::fs::read_to_string(&file).expect("kept");
        assert!(
            text.contains("denied = [\"terminal.semantic-read\"]"),
            "{text}"
        );

        // Explicit consent re-grants and clears the denial (re-grant flow).
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(
            text.contains("granted = [\"terminal.semantic-read\"]"),
            "{text}"
        );
        assert!(text.contains("denied = []"), "{text}");
        assert!(text.contains("enabled = true"), "{text}");

        // enable works again after the re-grant.
        let (code, out, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn managed_manifest_denied_codec_round_trips_and_rejects_overlap() {
        let mut records = BTreeMap::new();
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").expect("cap"));
        let mut denied = BTreeSet::new();
        denied.insert(CapabilityId::parse("ui.rich").expect("cap"));
        records.insert(
            "bitty-terminal.shell-integration".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "ab".repeat(32),
                enabled: false,
                granted,
                denied,
            },
        );
        let rendered = PluginState::from_records(records).render();
        assert!(rendered.contains("denied = [\"ui.rich\"]"), "{rendered}");
        let parsed = PluginState::parse(&rendered).expect("parses");
        assert_eq!(parsed.render(), rendered);

        // A capability cannot be both granted and denied (hostile edit fails closed).
        let hostile = rendered.replace(
            "denied = [\"ui.rich\"]",
            "denied = [\"terminal.semantic-read\"]",
        );
        assert!(PluginState::parse(&hostile).is_err());

        // Unknown capabilities in `denied` fail closed like `granted`.
        let hostile = rendered.replace("denied = [\"ui.rich\"]", "denied = [\"nope.everything\"]");
        assert!(PluginState::parse(&hostile).is_err());
    }

    #[test]
    fn unified_gate_piped_plugin_tables_carry_no_ansi() {
        // CTX-1065: piped plugin list/info tables carry no ANSI. The
        // harness stdout is piped, so the live gate resolves to plain.
        use std::io::IsTerminal as _;
        assert!(
            !std::io::stdout().is_terminal(),
            "test harness stdout must be piped for this assertion"
        );
        assert!(!color_enabled(false));
        assert!(!color_enabled(true));
        let table = format_list_table(&[], false);
        assert!(
            !table.contains("\u{1b}"),
            "piped plugin list must carry no ANSI, got {table:?}"
        );
        assert!(table.contains("(no plugins)"));
        // Explicit opt-out stays plain as well.
        let plain = format_list_table(&[], true);
        assert!(!plain.contains("\u{1b}"));
    }

    // ── fetch sources (CTX-1103, #1901) ────────────────────────────────────

    #[test]
    fn git_spellings_normalize_and_reject_hostile_urls() {
        // Allowed spellings normalize (git+ stripped for cloning).
        assert_eq!(
            normalize_git_source("https://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );
        assert_eq!(
            normalize_git_source("git+https://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );
        assert_eq!(
            normalize_git_source("git@example.com:a/b.git").as_deref(),
            Some("git@example.com:a/b.git")
        );
        // Port in https URL is accepted.
        assert!(normalize_git_source("https://example.com:8443/a/b.git").is_some());
        // scp without .git is accepted for this spelling.
        assert!(normalize_git_source("git@example.com:a/b").is_some());
        // Mixed-case schemes normalize to a lowercase `https://` transport
        // (git would otherwise look for a `git-remote-Https` helper).
        assert_eq!(
            normalize_git_source("Https://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );
        assert_eq!(
            normalize_git_source("GIT+HTTPS://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );

        // Rejected: http downgrade, file escape, ssh without git@, git://.
        for hostile in [
            "http://example.com/a/b.git",
            "file:///tmp/a.git",
            "ssh://example.com/a/b.git",
            "git://example.com/a/b.git",
            "https://example.com/a/b",
            "https://example.com/a/b.git ",
            "https://example.com/--upload-pack=evil.git",
            "https://example.com/a/b.git--config",
            "--upload-pack=touch evil",
            "--config",
            "-u",
            "git+ssh://example.com/a.git",
            "git+file:///tmp/a.git",
            "GIT@example.com:a/b.git",
            "https://user@example.com/a/b.git",
            "https://example.com/a b.git",
        ] {
            assert!(
                normalize_git_source(hostile).is_none(),
                "must reject hostile git spelling: {hostile:?}"
            );
        }
        // Rejected schemes carry a naming diagnostic.
        assert_eq!(
            rejected_scheme("file:///tmp/a.git").as_deref(),
            Some("file://")
        );
        assert_eq!(
            rejected_scheme("http://example.com/a.git").as_deref(),
            Some("http://")
        );
        // Archive suffixes select helpers case-insensitively.
        assert_eq!(
            classify_archive_kind("pkg.TAR.GZ"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(classify_archive_kind("pkg.tgz"), Some(ArchiveKind::TarGz));
        assert_eq!(
            classify_archive_kind("pkg.tar.zst"),
            Some(ArchiveKind::TarZst)
        );
        assert_eq!(classify_archive_kind("pkg.ZIP"), Some(ArchiveKind::Zip));
        assert_eq!(classify_archive_kind("pkg.toml"), None);
        // Git and archive operands select the source path (even bare names).
        assert!(is_source_operand("https://example.com/a/b.git"));
        assert!(is_source_operand("git@example.com:a/b.git"));
        assert!(is_source_operand("pkg.zip"));
        assert!(is_source_operand("./my-plugin"));
        assert!(!is_source_operand("bitty-terminal.shell-integration"));
    }

    #[test]
    fn archive_members_reject_traversal_absolute_and_windows_paths() {
        for good in [
            "bitty-plugin.toml",
            "lua/init.lua",
            "lua/",
            "./",
            "./lua/init.lua",
        ] {
            validate_archive_member(good).expect("good member validates");
        }
        for hostile in [
            "",
            "/tmp/absolute.txt",
            "../evil.txt",
            "a/../../b.txt",
            "a/../b.txt",
            "..",
            "a//b.txt",
            "C:\\evil.txt",
            "a:b.txt",
            "a\\b.txt",
        ] {
            assert!(
                validate_archive_member(hostile).is_err(),
                "must reject hostile member: {hostile:?}"
            );
        }
    }
    /// Test-only `git` with the ambient repository location scrubbed.
    ///
    /// Cargo tests can run under `git hooks` (or any parent that exports
    /// `GIT_DIR`/`GIT_WORK_TREE`/…); without scrubbing, `GIT_DIR` overrides
    /// `-C` and the fixture commands would write into the developer's real
    /// repository. Signing is pinned off so `commit` works on machines with
    /// `commit.gpgsign=true`.
    fn test_git() -> std::process::Command {
        let mut command = std::process::Command::new("git");
        for var in super::GIT_REPO_ENV_VARS {
            command.env_remove(var);
        }
        command.args(["-c", "commit.gpgsign=false"]);
        command
    }

    #[test]
    fn git_clone_fixed_argv_succeeds_on_a_local_path() {
        // Transport-level proof (bypasses scheme validation): fixed-argv
        // `git clone --filter=blob:none --depth=1 -- <src> <dest>` works and
        // inherits env. Uses a local path as the repo (no network). Skips
        // (does not panic) when `git` is absent.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = scratch_dir("git-clone");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("lua")).expect("lua dir");
        std::fs::write(repo.join("bitty-plugin.toml"), "x = 1\n").expect("manifest");
        std::fs::write(repo.join("lua/init.lua"), "-- hi\n").expect("init");
        let repo_arg = repo.display().to_string();
        let status = test_git()
            .args(["init", "-q", &repo_arg])
            .status()
            .expect("git init");
        assert!(status.success());
        for (key, value) in [("user.email", "t@t"), ("user.name", "t")] {
            let status = test_git()
                .args(["-C", &repo_arg, "config", key, value])
                .status()
                .expect("git config");
            assert!(status.success());
        }
        let status = test_git()
            .args(["-C", &repo_arg, "add", "-A"])
            .status()
            .expect("git add");
        assert!(status.success());
        let status = test_git()
            .args(["-C", &repo_arg, "commit", "-qm", "init"])
            .status()
            .expect("git commit");
        assert!(status.success());
        let dest = base.join("clone");
        run_git_clone(&repo_arg, &dest).expect("clone succeeds");
        assert!(dest.join("bitty-plugin.toml").is_file());
        assert!(dest.join("lua/init.lua").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn archive_unpack_round_trips_and_rejects_symlinks() {
        // Skip when helpers are absent (Windows without tar/unzip): the
        // production path fails closed with a diagnostic instead.
        if std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = scratch_dir("archive-roundtrip");
        let src = base.join("src");
        std::fs::create_dir_all(src.join("lua")).expect("lua dir");
        std::fs::write(src.join("bitty-plugin.toml"), "x = 1\n").expect("manifest");
        std::fs::write(src.join("lua/init.lua"), "-- hi\n").expect("init");
        let archive = base.join("pkg.tar.gz");
        let status = std::process::Command::new("tar")
            .args([
                "-czf",
                &archive.display().to_string(),
                "-C",
                &src.display().to_string(),
                ".",
            ])
            .status()
            .expect("tar pack");
        assert!(status.success());
        let dest = base.join("out");
        std::fs::create_dir(&dest).expect("dest dir");
        unpack_archive_into(&archive, &dest, ArchiveKind::TarGz).expect("unpacks");
        audit_extracted_tree(&dest).expect("no symlinks and within byte bound");
        assert_eq!(find_plugin_root(&dest), dest);

        // A symlink member lists fine but fails closed at audit (never
        // trusted by the package manager).
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink as symlink_file;
            let link_src = base.join("link-src");
            std::fs::create_dir_all(&link_src).expect("link src");
            symlink_file("/tmp", link_src.join("evil-link")).expect("symlink");
            std::fs::write(link_src.join("bitty-plugin.toml"), "x = 1\n").expect("m");
            let link_archive = base.join("link.tar.gz");
            let status = std::process::Command::new("tar")
                .args([
                    "-czf",
                    &link_archive.display().to_string(),
                    "-C",
                    &link_src.display().to_string(),
                    ".",
                ])
                .status()
                .expect("tar pack link");
            assert!(status.success());
            let link_dest = base.join("link-out");
            std::fs::create_dir(&link_dest).expect("link dest");
            // Extraction itself lands the link inside staging (tools refuse
            // to follow it); the audit then fails closed.
            let _ = extract_tar(&link_archive, &link_dest, ArchiveKind::TarGz);
            assert!(
                audit_extracted_tree(&link_dest).is_err(),
                "symlink member must fail closed"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn archive_size_listings_parse_gnu_bsd_and_zip_shapes() {
        // GNU `tar -tv` (the local `tar --version` shape): size sits before
        // the `YYYY-MM-DD HH:MM` date, names may hold spaces.
        let gnu = b"drwxr-xr-x fuyu/fuyu         0 2026-10-11 01:20 ./\n\
            -rw-r--r-- fuyu/fuyu         6 2026-10-11 01:20 ./file with space.txt\n\
            lrwxrwxrwx fuyu/fuyu         4 2026-10-11 01:20 ./link -> /tmp\n";
        let (count, total) = parse_tar_verbose_total(gnu).expect("gnu parses");
        assert_eq!((count, total), (3, 10));
        // BSD/libarchive `tar -tv` (`Mon DD HH:MM` dates, link-count field).
        let bsd = b"drwxr-xr-x  2 user  group  96 Oct 11 01:20 dir/\n\
            -rw-r--r--  1 user  group  12 Oct 11 01:20 file\n";
        let (count, total) = parse_tar_verbose_total(bsd).expect("bsd parses");
        assert_eq!((count, total), (2, 108));
        // A line with no date shape fails closed instead of guessing a size.
        assert!(parse_tar_verbose_total(b"garbage line\n").is_err());
        // `unzip -l` tables: rows between the separators, first field sizes.
        let zipped = b"Archive:  /tmp/a.zip\n  Length      Date    Time    Name\n\
            ---------  ---------- -----   ----\n        6  2026-10-11 01:20   file with space.txt\n\
                  0  2026-10-11 01:20   lua/\n---------                     -------\n\
                  6                     2 files\n";
        let (count, total) = parse_zip_list_total(zipped).expect("zip parses");
        assert_eq!((count, total), (2, 6));
        assert!(parse_zip_list_total(b"Archive: x\nno table here\n").is_err());
    }

    // ── official catalog compatibility (CTX-1446, #2461) ───────────────────

    /// Synthetic catalog entry no supported host can install.
    ///
    /// The loopback repository and all-zero pin are deliberate: if the compat
    /// gate ever moved after the fetch, the install would fail with
    /// `EXIT_GENERIC` (transport) instead of `EXIT_PLUGIN` (incompatible), and
    /// the test would catch the ordering regression without external network.
    static HOST_INCOMPATIBLE_ENTRY: OfficialPlugin = OfficialPlugin {
        id: "bitty-terminal.synthetic-incompatible",
        name: "Synthetic Incompatible",
        repository: "https://127.0.0.1:9/never-fetched.git",
        pin: "0000000000000000000000000000000000000000",
        manifest_hash: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        compat_bitty: Some(">=99.0.0,<100.0.0"),
        compat_plugin_api: None,
        description: "synthetic entry that no supported host can install",
        license: Some("MIT"),
    };

    #[test]
    fn host_incompatible_official_entry_is_refused_before_fetch() {
        let entry = &HOST_INCOMPATIBLE_ENTRY;

        // `plugin list` / `info` advertise the entry as listed but
        // incompatible, annotated with the exact declared range.
        let row = official_row(entry);
        assert_eq!(row.id, entry.id);
        assert_eq!(row.source, "registry");
        assert_eq!(row.state, "incompatible");
        assert!(!row.recorded && !row.bundled);
        assert!(
            row.description.contains("compat.bitty '>=99.0.0,<100.0.0'"),
            "{}",
            row.description
        );

        // The shared pre-flight returns the structured incompatibility with
        // the exact range and evaluated host, never a generic error.
        match official_entry_compat(entry) {
            Err(PackageOpError::Incompatible {
                field,
                requested,
                host,
                ..
            }) => {
                assert_eq!(field, "compat.bitty");
                assert_eq!(requested, ">=99.0.0,<100.0.0");
                assert_eq!(host, env!("CARGO_PKG_VERSION"));
            }
            other => panic!("expected incompatible, got {other:?}"),
        }

        // End-to-end refusal happens before staging/fetch: exit code 4 is the
        // incompatibility class, the loopback repository is never contacted,
        // and the store and consent output stay untouched.
        let store = scratch_dir("official-incompatible-store");
        let request = parse_ok(&["install", entry.id, "--yes"]);
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        let code = install_official(&request, entry, Some(&store), None, &mut input, &mut output);
        assert_eq!(code, EXIT_PLUGIN);
        assert!(output.is_empty(), "no fetch/progress output expected");
        assert!(!store.join("current.json").exists());
        assert!(!store.join("packages").exists());
        let _ = std::fs::remove_dir_all(&store);
    }

    // ── capability consent profiles (CTX-2467, #2472) ─────────────────────

    const PROFILE_PLUGIN: &str = "bitty-terminal.shell-integration";

    fn profile_state(dir: &Path) -> Option<String> {
        std::fs::read_to_string(state_path(dir)).ok()
    }

    #[test]
    fn parse_profile_flows_and_rejections() {
        let install = parse_ok(&[
            "install",
            PROFILE_PLUGIN,
            "--profile",
            "editor-like",
            "--yes",
        ]);
        assert_eq!(install.profile.as_deref(), Some("editor-like"));
        assert!(install.yes);

        let grant = parse_ok(&["grant", PROFILE_PLUGIN, "--profile=panel-like"]);
        assert_eq!(grant.verb, PluginVerb::Grant);
        assert_eq!(grant.profile.as_deref(), Some("panel-like"));
        assert_eq!(grant.cap, None);

        let grant_cap = parse_ok(&["grant", PROFILE_PLUGIN, "--cap", "clipboard.read"]);
        assert_eq!(grant_cap.cap.as_deref(), Some("clipboard.read"));
        assert_eq!(grant_cap.profile, None);
        assert!(parse_ok(&["grant", PROFILE_PLUGIN, "--cap", "clipboard.read", "--yes"]).yes);

        assert!(
            parse_error(&["install", PROFILE_PLUGIN, "--profile", "nope"])
                .contains("unknown --profile")
        );
        assert!(parse_error(&["grant", PROFILE_PLUGIN]).contains("needs --profile"));
        assert!(
            parse_error(&[
                "grant",
                PROFILE_PLUGIN,
                "--profile",
                "minimal",
                "--cap",
                "clipboard.read"
            ])
            .contains("not both")
        );
        assert!(
            parse_error(&["revoke", PROFILE_PLUGIN, "--profile", "minimal"])
                .contains("only applies to `install` and `grant`")
        );
        assert!(
            parse_error(&["install", PROFILE_PLUGIN, "--cap", "clipboard.read"])
                .contains("only applies to `revoke` and `grant`")
        );
        assert!(
            parse_error(&["grant", PROFILE_PLUGIN, "--profile", "minimal", "--force"])
                .contains("--force only applies")
        );
    }

    #[test]
    fn install_profile_grants_only_the_preselected_set() {
        // editor-like preselects terminal.semantic-read, which the bundled
        // manifest requests: the fresh install is complete and enabled.
        let (code, out, dir) = run(
            &[
                "install",
                PROFILE_PLUGIN,
                "--profile",
                "editor-like",
                "--yes",
            ],
            None,
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("capability profile 'editor-like' v1"), "{out}");
        assert!(out.contains("-- review before apply --"), "{out}");
        assert!(out.contains("+ will grant (1)"), "{out}");
        let state = profile_state(&dir).expect("state written");
        assert!(
            state.contains("granted = [\"terminal.semantic-read\"]"),
            "{state}"
        );
        assert!(state.contains("enabled = true"), "{state}");
        assert!(
            out.contains("1 of 1 capabilit"),
            "summary must report coverage: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimal_profile_grants_nothing_and_keeps_the_plugin_disabled() {
        let (code, out, dir) = run(
            &["install", PROFILE_PLUGIN, "--profile", "minimal", "--yes"],
            None,
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("capability profile 'minimal' v1"), "{out}");
        assert!(out.contains("+ will grant: none"), "{out}");
        assert!(out.contains("stays disabled"), "{out}");
        let state = profile_state(&dir).expect("state written");
        assert!(state.contains("granted = []"), "{state}");
        assert!(state.contains("enabled = false"), "{state}");
        assert!(
            !state.contains("terminal.semantic-read"),
            "minimal must not grant the requested capability: {state}"
        );

        // Applying editor-like later adds exactly its preselected capability
        // and does not enable the plugin; no grant beyond the profile appears.
        let config = state_path(&dir).display().to_string();
        let (code, out, _) = run(
            &["grant", PROFILE_PLUGIN, "--profile", "editor-like", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("applied profile 'editor-like' v1"), "{out}");
        let state = profile_state(&dir).expect("state rewritten");
        assert_eq!(
            state.matches("terminal.semantic-read").count(),
            1,
            "{state}"
        );
        assert!(state.contains("enabled = false"), "{state}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_profile_decline_fails_closed() {
        let (code, out, dir) = run(
            &["install", PROFILE_PLUGIN, "--profile", "editor-like"],
            None,
            "n\n",
        );
        assert_eq!(code, EXIT_GENERIC, "{out}");
        assert!(out.contains("review before apply"), "{out}");
        assert!(!state_path(&dir).exists(), "decline must not write state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_cap_grant_is_idempotent_and_rejects_unrequested_caps() {
        let (code, out, dir) = run(
            &["install", PROFILE_PLUGIN, "--profile", "minimal", "--yes"],
            None,
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");

        let config = state_path(&dir).display().to_string();
        let (code, out, _) = run(
            &[
                "grant",
                PROFILE_PLUGIN,
                "--cap",
                "terminal.semantic-read",
                "--yes",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("granted 'terminal.semantic-read'"), "{out}");
        let state = profile_state(&dir).expect("state written");
        assert!(
            state.contains("granted = [\"terminal.semantic-read\"]"),
            "{state}"
        );
        assert!(state.contains("enabled = false"), "{state}");

        // Re-running the same explicit grant is an idempotent no-op.
        let (code, out, _) = run(
            &[
                "grant",
                PROFILE_PLUGIN,
                "--cap",
                "terminal.semantic-read",
                "--yes",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("already granted"), "{out}");

        // A manifest-unrequested capability can never be granted this way.
        let (code, out, _) = run(
            &[
                "grant",
                PROFILE_PLUGIN,
                "--cap",
                "network.connect:example.com:443",
                "--yes",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.is_empty(), "{out}");

        // Unknown capability names fail closed as usage errors.
        let (code, out, _) = run(
            &["grant", PROFILE_PLUGIN, "--cap", "made.up", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_USAGE, "{out}");
        assert!(out.is_empty(), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_never_clears_an_explicit_denial() {
        let dir = scratch_dir("grant-denied");
        let id = PROFILE_PLUGIN;
        let manifest = bundled_manifest(id).expect("bundled");
        let capability = CapabilityId::parse("terminal.semantic-read").expect("known");
        let mut denied = BTreeSet::new();
        denied.insert(capability.clone());
        let mut state = PluginState::new();
        state.insert(
            id.to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: manifest.manifest_hash(),
                enabled: false,
                granted: BTreeSet::new(),
                denied: denied.clone(),
            },
        );
        save_state(&state_path(&dir), &state).expect("state saved");
        let config = state_path(&dir).display().to_string();

        // Explicit `--cap` is narrow consent, not a deny-loop bypass.
        let (code, out, _) = run(
            &["grant", id, "--cap", "terminal.semantic-read", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.is_empty(), "{out}");
        let state = load_state(&state_path(&dir)).expect("state readable");
        let record = state.get(id).expect("record kept");
        assert!(record.denied.contains(&capability), "denial must persist");
        assert!(record.granted.is_empty(), "nothing granted");

        // A profile also reports and preserves the denial.
        let (code, out, _) = run(
            &["grant", id, "--profile", "editor-like", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("explicitly denied"), "{out}");
        let state = load_state(&state_path(&dir)).expect("state readable");
        let record = state.get(id).expect("record kept");
        assert!(record.denied.contains(&capability), "profile keeps denial");
        assert!(record.granted.is_empty(), "profile grants nothing denied");

        // `install --profile` is also a preselect, not a denial reset.
        let (code, out, _) = run(
            &["install", id, "--profile", "editor-like", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        let state = load_state(&state_path(&dir)).expect("state readable");
        let record = state.get(id).expect("record kept");
        assert!(
            record.denied.contains(&capability),
            "install --profile keeps the denial"
        );
        assert!(
            !record.granted.contains(&capability),
            "a profile cannot resurrect the denied capability"
        );
        assert!(
            !record.enabled,
            "an unmet denied capability keeps the plugin disabled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_packages_reject_profiles_and_grant() {
        let (code, out, dir) = run(
            &["install", "./my-plugin", "--profile", "editor-like"],
            None,
            "",
        );
        assert_eq!(code, EXIT_USAGE, "{out}");
        assert!(!state_path(&dir).exists());

        let (code, out, dir2) = run(
            &["grant", "xuepoo.hello", "--profile", "editor-like", "--yes"],
            None,
            "",
        );
        assert_eq!(code, EXIT_PLUGIN, "{out}");
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
