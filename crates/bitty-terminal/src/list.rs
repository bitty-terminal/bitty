//! `bitty list`: enumerate resources (CTX-0172).
//!
//! Canonical: `docs/interfaces/cli.md` (`list` introspection
//! section) as refined by `docs/specifications/cli-contract-rfc.md`
//! (`bitty list` mixed class, output envelope v1, exit codes 0-8, `ls` alias).
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty list <kind> [--format table|json|jsonl] [--socket PATH]
//!   [--instance ID] [--no-color]` and alias `bitty ls <kind> ...` (same
//!   executable, envelope `command` names the invoked spelling).
//! - Kinds (task-required): `themes`, `plugins`, `instances`, `actions`,
//!   `keymaps` (singular `theme|plugin|instance|action|keymap` accepted as
//!   convenience, same result). Unknown kinds are `UsageError` (exit 2)
//!   naming the valid set. RFC candidates (`fonts|commands|protocols`) are
//!   not invented here: they fail closed as unknown kinds until their owning
//!   slice lands.
//! - Class: `themes`, `plugins`, `actions`, and `keymaps` are local (no
//!   instance, safe-mode clean, no plugin VM loaded). `instances` is runtime
//!   discovery (registry scan, live endpoint probe) but fetches no terminal
//!   content.
//!   Unix scans `$XDG_RUNTIME_DIR/bitty/*.sock` (plus the `BITTY_SOCKET`
//!   parent) and probes each socket with a `connect`; Windows scans the same
//!   registry inputs for `<instance>.sock` markers and probes each
//!   `\\.\pipe\bitty-<instance>` named pipe, merging live pipe-namespace
//!   enumeration (CTX-0196, transport since CTX-1405/#2244). A probe verifies
//!   the serving process token SID before reporting live, so a foreign-owner
//!   pipe is never listed. Only discovery metadata is reported
//! - `--format table` (default) is human output, not a machine contract.
//!   `--format json` / `--format jsonl` emit the versioned envelope (`v: 1`,
//!   `command: "list"|"ls"`, `ok`, `result`, plus `error` on failure) on
//!   stdout; diagnostics go to stderr so JSON is never corrupted.
//! - `--` anywhere in `list` mode is `UsageError` (exit 2): stray separators
//!   are never silently ignored (RFC pass-through rule).
//! - `bitty -- list ...` runs a program named `list` (escape hatch); the word
//!   `list`/`ls` as first positional is always this subcommand. Same for a
//!   program named `ls`: use `bitty run -- ls ...` or `bitty -- ls ...`.
//! - `--help` never requires an instance and never loads a plugin VM.
//!
//! # Read-only surface (no new authority)
//!
//! - Themes reuse `bitty-config::theme` registry including built-in dark from
//!   CTX-0147 (`BITTY_DARK`, `DEFAULT_THEME_NAME`, alias `dark`). No file I/O.
//! - Plugins reuse `bitty-plugin-host::bundled` static catalog (staged but
//!   disabled by default per Default Distribution RFC). No VM is loaded; help
//!   and listing come from static manifests only.
//! - Instances reuse `bitty-ipc::devtools` endpoint discovery (`BITTY_SOCKET`,
//!   `XDG_RUNTIME_DIR`, `BITTY_INSTANCE_ID` advisory identifiers, portable
//!   `AF_UNIX` bound on Unix, `\\.\pipe\bitty-<instance>` naming on Windows,
//!   instance grammar). Only discovery metadata is reported
//!   (`instance`, `socket`, `live`, `detail`); terminal content is never
//!   fetched. Detailed snapshots require `debug.inspect` via `ctl`/DevTools
//!   and are out of scope here, which is how bearer scoping is respected: no
//!   privileged read occurs, and explicit `--socket` paths that fail OS
//!   authentication surface as exit 7 rather than data.
//! - Actions reuse the canonical [`ChromeAction::description`](bitty_config::keymap::ChromeAction::description)
//!   source (same strings `bitty inspect key` shows, never a parallel copy)
//!   plus the `inspect` core-command registry summaries (no duplicated
//!   strings). Manual-bind-only is derived from `DEFAULT_KEYMAPS` absence.
//!   No file I/O.
//! - Keymaps reuse the shared shipped resolver
//!   ([`shipped_resolved_keymaps`](bitty_config::keymap::shipped_resolved_keymaps))
//!   that `bitty inspect key` reads (same code path, no fork). No file I/O.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success (including empty `instances` when no sockets exist).
//! - `2` usage error (missing/unknown kind, extra positional, unknown flag,
//!   bad `--format`, bad `--socket`/`--instance` shape, stray `--`).
//! - `6` IPC/runtime unavailable (socket base cannot be derived, directory
//!   unreadable, explicit socket missing/unreachable for reasons other than
//!   permission).
//! - `7` permission denied (socket directory or explicit socket fails OS
//!   authentication: bad mode/owner or `EACCES` on connect).
//! - `1` generic (unexpected failure after parsing; never used for the three
//!   usage/runtime paths above).
//!
//! # Bounds (T-01 parity, fail closed with exit 2 before any I/O)
//!
//! - Kind token: 1..=`MAX_LIST_KIND_LEN` bytes, ASCII alphanumeric/`-`/`_`
//!   after lowercasing (longer or NUL-containing rejected as unknown kind).
//! - `--format` value: at most `MAX_LIST_FORMAT_LEN` bytes.
//! - `--socket`: 1..=`MAX_SOCKET_PATH_BYTES` (reused from `bitty-ipc`),
//!   no NUL.
//! - `--instance`: 1..=`MAX_INSTANCE_ID_LEN` (reused), `^[a-z0-9_-]+$`
//!   case-insensitive.
//! - Socket-dir scan caps entries at `MAX_LIST_INSTANCES` (shed oldest
//!   lexicographically past the cap, counted in `detail`); each file name
//!   capped at 108 bytes (portable `sun_path` ceiling). Windows merges live
//!   `\\.\pipe\bitty-*` enumeration into the same bound via
//!   [`merge_instance_rows`].

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// Generic failure (unexpected post-parse failure).
/// Kept as stable taxonomy API; the current three kinds never emit it.
#[allow(dead_code)]
pub const EXIT_GENERIC: i32 = 1;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;
/// IPC/runtime unavailable.
pub const EXIT_RUNTIME: i32 = 6;
/// Permission denied.
/// Allowed dead on non-Unix stubs where OS authentication cannot be probed;
/// still part of the stable taxonomy.
#[allow(dead_code)]
pub const EXIT_PERM: i32 = 7;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum bytes for a kind token (plenty for `themes|plugins|instances|actions|keymaps|services`).
pub const MAX_LIST_KIND_LEN: usize = 32;
/// Maximum bytes for a `--format` value.
pub const MAX_LIST_FORMAT_LEN: usize = 16;
/// Maximum socket value bytes (reused portable `AF_UNIX` bound).
pub const MAX_LIST_SOCKET_LEN: usize = bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES;
/// Maximum instance id bytes (reused).
pub const MAX_LIST_INSTANCE_LEN: usize = bitty_ipc::devtools::MAX_INSTANCE_ID_LEN;
/// Maximum instances reported from one directory scan (bounded table/JSON).
pub const MAX_LIST_INSTANCES: usize = 128;

// ---------------------------------------------------------------------------
// Kind and format
// ---------------------------------------------------------------------------

/// Resource kind enumerated by `bitty list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// Built-in theme presets (`bitty-config::theme`).
    Themes,
    /// Static bundled plugin catalog (no VM).
    Plugins,
    /// Live socket discovery (no content fetch).
    Instances,
    /// Machine-readable chrome-action + core-command catalog (local).
    Actions,
    /// Resolved chord to action keymaps (local, shipped defaults).
    Keymaps,
    /// Registered extension interfaces (static registration observed
    /// without loading any plugin VM; dynapi observability, bitty#2002).
    Services,
}

impl ListKind {
    /// Canonical plural names in help order (single source for shell
    /// completion).
    pub const NAMES: &[&str] = &[
        "themes",
        "plugins",
        "instances",
        "actions",
        "keymaps",
        "services",
    ];

    /// Parse a kind token (case-insensitive, singular accepted).
    /// Returns `None` for unknown kinds (caller maps to exit 2).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_LIST_KIND_LEN || raw.contains('\0') {
            return None;
        }
        match raw.trim().to_ascii_lowercase().as_str() {
            "themes" | "theme" => Some(Self::Themes),
            "plugins" | "plugin" => Some(Self::Plugins),
            "instances" | "instance" => Some(Self::Instances),
            "actions" | "action" => Some(Self::Actions),
            "keymaps" | "keymap" => Some(Self::Keymaps),
            "services" | "service" => Some(Self::Services),
            _ => None,
        }
    }

    /// Canonical plural name used in output `result.kind`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Themes => "themes",
            Self::Plugins => "plugins",
            Self::Instances => "instances",
            Self::Actions => "actions",
            Self::Keymaps => "keymaps",
            Self::Services => "services",
        }
    }

    /// Registry-style command id fragment for envelope diagnostics.
    /// Stable contract API for generators; the envelope currently carries the
    /// invoked spelling (`list`/`ls`), this names the executable.
    #[allow(dead_code)]
    #[must_use]
    pub fn command_id(self) -> &'static str {
        match self {
            Self::Themes => "core.list.themes",
            Self::Plugins => "core.list.plugins",
            Self::Instances => "core.list.instances",
            Self::Actions => "core.list.actions",
            Self::Keymaps => "core.list.keymaps",
            Self::Services => "core.list.services",
        }
    }
}

/// Output shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListFormat {
    /// Human table (not a machine contract).
    Table,
    /// Single versioned JSON envelope.
    Json,
    /// Same envelope, single line.
    Jsonl,
}

impl ListFormat {
    /// Parse `--format` (`None` means table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(v) => {
                if v.len() > MAX_LIST_FORMAT_LEN || v.contains('\0') {
                    return Err(format!(
                        "bitty list: unknown --format {v:?} (want table|json|jsonl)"
                    ));
                }
                match v.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty list: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Owned request (parsed in main.rs, validated here)
// ---------------------------------------------------------------------------

/// Validated `bitty list` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRequest {
    /// Resource kind.
    pub kind: ListKind,
    /// Output shape.
    pub format: ListFormat,
    /// Explicit `--socket` override (advisory path, still authenticated by OS).
    pub socket: Option<String>,
    /// Explicit `--instance` override (resolved via discovery file).
    pub instance: Option<String>,
    /// Disable ANSI coloring in table output.
    pub no_color: bool,
    /// Invoked spelling for envelope (`list` or `ls`).
    pub spelling: String,
}

impl ListRequest {
    /// Validate raw fields into a request. All failures are usage errors
    /// (exit 2, stderr only, no stdout envelope).
    pub fn validate(
        kind_raw: Option<&str>,
        format_raw: Option<&str>,
        socket_raw: Option<&str>,
        instance_raw: Option<&str>,
        no_color: bool,
        spelling: &str,
    ) -> Result<Self, String> {
        let kind_token = kind_raw.ok_or_else(|| {
            format!(
                "{}\n{}",
                "bitty list: missing <kind> (want themes|plugins|instances|actions|keymaps|services)",
                list_usage()
            )
        })?;
        let kind = ListKind::parse(kind_token).ok_or_else(|| {
            format!(
                "bitty list: unknown kind {kind_token:?} (want themes|plugins|instances|actions|keymaps|services)\n{}",
                list_usage()
            )
        })?;
        let format = ListFormat::parse(format_raw).map_err(|m| format!("{m}\n{}", list_usage()))?;
        if let Some(s) = socket_raw {
            validate_socket_value(s).map_err(|m| format!("{m}\n{}", list_usage()))?;
        }
        if let Some(id) = instance_raw {
            validate_instance_value(id).map_err(|m| format!("{m}\n{}", list_usage()))?;
        }
        if socket_raw.is_some() && instance_raw.is_some() {
            return Err(format!(
                "bitty list: --socket and --instance are mutually exclusive (explicit socket bypasses discovery)\n{}",
                list_usage()
            ));
        }
        if (socket_raw.is_some() || instance_raw.is_some()) && kind != ListKind::Instances {
            return Err(format!(
                "bitty list: --socket/--instance apply only to `instances` (kind is {:?})\n{}",
                kind.name(),
                list_usage()
            ));
        }
        Ok(Self {
            kind,
            format,
            socket: socket_raw.map(str::to_string),
            instance: instance_raw.map(str::to_string),
            no_color,
            spelling: spelling.to_string(),
        })
    }
}

/// Validate an explicit `--socket` value (shape only; OS auth at probe time).
fn validate_socket_value(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_LIST_SOCKET_LEN {
        return Err(format!(
            "bitty list: --socket must be 1..={MAX_LIST_SOCKET_LEN} bytes (got {})",
            value.len()
        ));
    }
    if value.contains('\0') {
        return Err("bitty list: --socket must not contain NUL".to_string());
    }
    Ok(())
}

/// Validate an explicit `--instance` value (grammar only; resolution at probe).
fn validate_instance_value(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_LIST_INSTANCE_LEN {
        return Err(format!(
            "bitty list: --instance must be 1..={MAX_LIST_INSTANCE_LEN} (got {})",
            value.len()
        ));
    }
    let ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(
            "bitty list: --instance must match ^[a-z0-9_-]+$ (case-insensitive)".to_string(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Usage and help
// ---------------------------------------------------------------------------

/// Short usage for stderr (fail-closed exit 2 trailer).
#[must_use]
pub fn list_usage() -> String {
    "usage: bitty list <themes|plugins|instances|actions|keymaps|services> [--format table|json|jsonl] [--socket PATH | --instance ID] [--no-color]\n       bitty ls <themes|plugins|instances|actions|keymaps|services> [--format table|json|jsonl] [--no-color]\n\nkinds:\n  themes     built-in presets plus local theme files (no network)\n  plugins    static bundled plugin catalog, no VM (local, safe-mode clean)\n  instances  live instance discovery: instance, socket, live (runtime, no content fetch)\n  actions    chrome actions + core commands catalog (local, safe-mode clean)\n  keymaps    resolved chord to action table, shipped defaults (local, safe-mode clean)\n  services   registered extension interfaces, no VM (local, safe-mode clean)"
        .to_string()
}

/// Full help for `bitty list --help` (stdout, exit 0).
#[must_use]
pub fn list_help_text(invoked_as: &str) -> String {
    format!(
        "bitty {invoked_as} — enumerate resources (local or runtime)\n\
         \n\
          Usage: bitty {invoked_as} <themes|plugins|instances|actions|keymaps|services> [--format table|json|jsonl] [--socket PATH | --instance ID] [--no-color]\n\
         \n\
         Kinds:\n  \
           themes     Built-in theme preset catalog from bitty-config::theme (e.g. bitty-dark,\n  \
                      tokyo-night, github-dark, catppuccin-mocha; aliases like dark) plus\n  \
                      accepted local files from <config>/bitty/themes (*.toml), selectable\n  \
                      as local:<name>. Local class: no instance, no network; reads local\n  \
                      theme files unless --safe keeps the catalog built-in-only.\n  \
           plugins    Static bundled plugin catalog (bitty-terminal.*), staged disabled by default.\n  \
                      Local class: manifest metadata only, no plugin VM loaded, safe-mode clean.\n  \
            instances  Live instance discovery (instance, socket, live). Runtime class:\n  \
                       Unix scans the socket directory and probes each socket with a connect;\n  \
                       Windows scans the instance registry and probes each named pipe, plus\n  \
                       live pipe-namespace enumeration; every platform reports discovery\n  \
                       metadata only. Windows serves the same instance-derived named pipe\n  \
                       (CTX-1405) and probes verify the serving process identity before a\n  \
                       row reports live. Terminal content is never fetched;\n  \
                       detailed snapshots need debug.inspect via ctl/DevTools (bearer scoping\n  \
                       respected by not reading privileged data here).\n  \
           actions    Machine-readable action catalog for users and doc tooling\n  \
                      (consumer: bitty-manual#15 drift guard). Every ChromeAction variant\n  \
                      plus every registered core.* command. Local class: no instance;\n  \
                      rows needing live state carry requires_runtime true (chrome)\n  \
                      or class runtime (core). Each chrome row carries its effective\n  \
                      binding (user-configured chord first, default fallback, null\n  \
                      only for manual-bind-only); core rows always report null binding.\n  \
            keymaps    Resolved chord to action table (config overrides plus defaults,\n  \
                       same resolver `list actions` bindings read: shared code path,\n  \
                       no fork). Local class: no instance; user file read best-effort\n  \
                       with shipped-default fallback. Header and JSON meta carry the\n  \
                       mod_key context (alt default, super when rebound).\n  \
            services   Registered extension interfaces (dynapi observability): one\n  \
                       row per backing head of each registered provision, computed\n  \
                       from validated manifests plus install records — never\n  \
                       provider-reported strings, no plugin VM loaded. Local class:\n  \
                       bundled catalog plus discovery roots; invalid manifests\n  \
                       are skipped, never fatal; `--safe` skips third-party\n  \
                       sources like startup.\n\
         \n\
         Options:\n  \
           --format SHAPE  table (default, human, not a contract) | json | jsonl (envelope v1)\n  \
            --socket PATH   Explicit socket for `instances` (bypasses directory scan;\n  \
                            still authenticated by the OS at probe).\n  \
           --instance ID   Explicit instance for `instances` (resolved via BITTY_SOCKET /\n  \
                           XDG_RUNTIME_DIR discovery, then probed like --socket).\n  \
           --no-color      Disable ANSI coloring in table output (also honours NO_COLOR).\n  \
           -h, --help      Print this help and exit (never needs an instance or VM).\n\
         \n\
         Output contract:\n  \
           Stdout carries the result; stderr carries diagnostics. JSON/JSONL use\n  \
           envelope {{\"v\":1,\"command\":\"{invoked_as}\",\"ok\":true,\"result\":{{\"kind\":...}}}}.\n  \
           Usage errors (exit 2) go to stderr with no stdout envelope. Runtime\n  \
           failures for `instances` emit ok:false envelopes (exit 6/7).\n  \
           themes result keys: kind, count, themes (array of {{\"name\",\"aliases\",\"category\",\n  \
           \"source\",\"provenance\",\"license\",\"background\",\"foreground\",\"cursor\",\"selection\"}});\n  \
           source is builtin|local and provenance is the optional upstream URL.\n  \
           actions result keys: kind, count (actions plus commands), actions\n  \
           (array of {{\"id\",\"description\",\"manual_bind_only\",\"requires_runtime\",\"binding\",\"kind\":\"chrome\"}}),\n  \
           commands (array of {{\"id\",\"description\",\"class\",\"requires_runtime\",\"binding\",\"kind\",\"scopes\",\"owner\"}}).\n  \
           Binding is the effective chord (user-configured first, default fallback,\n  \
           null only for manual-bind-only chrome actions; always null for core).\n  \
           Action descriptions reuse the canonical ChromeAction::description source\n  \
           (same strings inspect key shows); core descriptions reuse the\n  \
           inspect command registry summaries. Manual-bind-only is derived from\n  \
           DEFAULT_KEYMAPS (absent means manual).\n  \
            keymaps result keys: kind, count, mod_key, source, keymaps\n  \
            (array of {{\"chord\",\"action\",\"description\",\"context\",\"source\"}}).\n  \
            The table header names mod_key and source; JSON meta carries both.\n  \
            Both actions bindings and keymaps rows come from the same resolved\n  \
            table (config overrides plus defaults, no fork).\n  \
            services result keys: kind, count, services\n  \
            (array of {{\"interface\",\"owner\",\"version\",\"provider\",\"backing_head\",\"grant_source\",\"status\"}}).\n  \
            Every value is a required string (v1 never emits null); the table\n  \
            renders absent cells as `-` (reserved null marker, currently\n  \
            unreachable) and scrubs control characters so hostile provider\n  \
            strings render safely.\n\
         \n\
         Exit codes:\n  \
           0 success (empty instances is success with count 0)\n  \
           2 usage error (missing/unknown kind, extra arg, bad --format/--socket/--instance, stray --)\n  \
           6 IPC/runtime unavailable (no socket base, unreadable dir, missing socket)\n  \
           7 permission denied (directory/socket fails OS authentication)\n\
         \n\
         Examples:\n  \
           bitty list themes\n  \
           bitty list plugins --format json\n  \
           bitty list instances\n  \
           bitty list instances --format jsonl\n  \
           bitty list actions --format json\n  \
           bitty list keymaps --format table\n  \
           bitty list services --format json\n  \
           bitty ls themes --format table\n"
    )
}

// ---------------------------------------------------------------------------
// Theme enumeration (reuses bitty-config::theme, CTX-0147)
// ---------------------------------------------------------------------------

/// One theme row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    /// Registry name (e.g. `bitty-dark`).
    pub name: String,
    /// Dark/light classification (`dark` or `light`).
    pub category: String,
    /// Accepted aliases (e.g. `dark`).
    pub aliases: Vec<String>,
    /// Row origin: `builtin` or `local` (never a URL).
    pub source: String,
    /// Upstream project URL for a built-in preset, or the optional local
    /// file `source` value (empty when the file omits it).
    pub provenance: String,
    /// Upstream license identifier.
    pub license: String,
    /// Background as `#rrggbb`.
    pub background: String,
    /// Foreground as `#rrggbb`.
    pub foreground: String,
    /// Cursor as `#rrggbb`.
    pub cursor: String,
    /// Selection as `#rrggbb`.
    pub selection: String,
}

/// Format `[r,g,b]` bytes as `#rrggbb`.
#[must_use]
pub fn rgb_to_hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// Enumerate every built-in theme preset, default first.
///
/// Pure, no I/O: the single source of truth is
/// `bitty-config::theme::ALL_PRESETS`. Callers must handle empty
/// (defensive) as an empty table, not an error. `bitty list themes` adds
/// local files through [`list_themes_with_local`].
#[must_use]
pub fn list_themes() -> Vec<ThemeInfo> {
    bitty_config::theme::list_presets()
        .iter()
        .map(|theme| ThemeInfo {
            name: theme.name.to_string(),
            category: theme.category.as_str().to_string(),
            aliases: theme.aliases.iter().map(|a| (*a).to_string()).collect(),
            source: String::from("builtin"),
            provenance: theme.source.to_string(),
            license: theme.license.to_string(),
            background: rgb_to_hex(theme.background),
            foreground: rgb_to_hex(theme.foreground),
            cursor: rgb_to_hex(theme.cursor),
            selection: rgb_to_hex(theme.selection),
        })
        .collect()
}

/// Enumerate built-ins plus accepted local theme files.
///
/// Local rows carry the explicit selectable name `local:<name>` and
/// `source = "local"`; their optional upstream URL travels in `provenance`,
/// never in the table's origin column. Rejected files are returned as
/// per-file diagnostics so the caller can print them without hiding the
/// catalog. The built-in prefix of the result is byte-identical to
/// [`list_themes`], and the whole function is filesystem-only (no network).
#[must_use]
pub fn list_themes_with_local() -> (Vec<ThemeInfo>, Vec<bitty_config::LocalThemeDiagnostic>) {
    let catalog = bitty_config::local_theme::load_local_themes();
    let mut rows = list_themes();
    for theme in &catalog.themes {
        rows.push(ThemeInfo {
            name: format!("{}{}", bitty_config::theme::LOCAL_THEME_PREFIX, theme.name),
            category: theme.category.as_str().to_string(),
            aliases: theme
                .aliases
                .iter()
                .map(|alias| format!("{}{}", bitty_config::theme::LOCAL_THEME_PREFIX, alias))
                .collect(),
            source: String::from("local"),
            provenance: theme.provenance.clone(),
            license: theme.license.clone(),
            background: rgb_to_hex(theme.palette.background),
            foreground: rgb_to_hex(theme.palette.foreground),
            cursor: rgb_to_hex(theme.palette.cursor),
            selection: rgb_to_hex(theme.palette.selection),
        });
    }
    (rows, catalog.diagnostics)
}

// ---------------------------------------------------------------------------
// Plugin enumeration (reuses bitty-plugin-host::bundled, no VM)
// ---------------------------------------------------------------------------

/// One plugin row (static manifest metadata only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// Fully qualified id (e.g. `bitty-terminal.shell-integration`).
    pub id: String,
    /// Human name.
    pub name: String,
    /// Manifest version.
    pub version: String,
    /// One-line description.
    pub description: String,
    /// Whether this id is in the bundled catalog.
    pub bundled: bool,
    /// Whether enabled (always `false` here: bundled is staged disabled by
    /// default; enabling is explicit config + consent, read by another slice).
    pub enabled: bool,
    /// Qualified commands (`plugin-id:command`) from static manifest.
    pub commands: Vec<String>,
}

/// Enumerate the static bundled catalog (no VM, safe-mode clean).
///
/// Pure: reads `bundled::all_bundled_manifests()` only. Every row reports
/// `enabled: false` because v1 bundled is staged disabled by default; this
/// function never reads user config, so it cannot claim otherwise.
#[must_use]
pub fn list_plugins() -> Vec<PluginInfo> {
    list_plugins_from_manifests(&bitty_plugin_host::bundled::all_bundled_manifests())
}

/// Test hook: render plugin rows from an explicit manifest slice (covers the
/// empty-registry state without touching the global catalog).
#[must_use]
pub fn list_plugins_from_manifests(
    manifests: &[bitty_plugin_host::PluginManifest],
) -> Vec<PluginInfo> {
    let mut out: Vec<PluginInfo> = manifests
        .iter()
        .map(|m| PluginInfo {
            id: m.id().to_string(),
            name: m.identity.name.clone(),
            version: m.identity.version.clone(),
            description: m.identity.description.clone(),
            bundled: bitty_plugin_host::bundled::is_bundled(m.id()),
            enabled: false,
            commands: m
                .lazy
                .commands
                .iter()
                .map(|c| c.id.as_str().to_string())
                .collect(),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ---------------------------------------------------------------------------
// Action catalog (CTX-1127, issue #1934; consumer bitty-manual#15)
// ---------------------------------------------------------------------------

/// One chrome-action row.
///
/// `id` is the canonical spelling, `description` reuses the canonical
/// [`ChromeAction::description`](bitty_config::keymap::ChromeAction::description)
/// source (same strings `bitty inspect key` shows — never a parallel copy),
/// `manual_bind_only` is derived from `DEFAULT_KEYMAPS` absence,
/// `requires_runtime` marks entries needing live state (all chrome actions
/// need the live window; `list` itself stays local), and `binding` is the
/// effective keybinding (user-configured chord first, default fallback, null
/// only for manual-bind-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeActionInfo {
    /// Canonical id spelling (e.g. `close_view`, `goto_split:left`).
    pub id: String,
    /// One-line English description (canonical source).
    pub description: String,
    /// True when no shipped default binds this variant.
    pub manual_bind_only: bool,
    /// True when execution needs live runtime state.
    pub requires_runtime: bool,
    /// Effective chord (e.g. `alt+d`) or `None` when manual-bind-only.
    pub binding: Option<String>,
}

/// One core-command row (reuses the `inspect` registry summaries directly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreCommandInfo {
    /// Stable registry id (e.g. `core.terminal.text`).
    pub id: String,
    /// One-line description (registry summary, no copy).
    pub description: String,
    /// Entry class (`local`, `runtime`, or `extension`).
    pub class: String,
    /// True when `class` is `runtime`.
    pub requires_runtime: bool,
    /// Entry kind (always `command` today).
    pub kind: String,
    /// Required scopes.
    pub scopes: Vec<String>,
    /// Owning service (`core`).
    pub owner: String,
    /// Core commands have no chord; always `None` (keeps the `binding` key
    /// stable across both arrays).
    pub binding: Option<String>,
}

/// Enumerate every chrome-action variant (local, no I/O).
///
/// Pure: builds from
/// [`all_chrome_actions`](bitty_config::keymap::all_chrome_actions) and the
/// canonical description/manual/runtime accessors, with `binding` resolved
/// through the shared
/// [`effective_binding_for_action`](bitty_config::keymap::effective_binding_for_action)
/// helper over the shipped defaults. Sorted by `id` for stable output.
/// See [`list_chrome_actions_with`] for the override-aware variant used by
/// the live `list actions` dispatch.
#[must_use]
#[allow(dead_code)]
pub fn list_chrome_actions() -> Vec<ChromeActionInfo> {
    list_chrome_actions_with(&bitty_config::keymap::shipped_resolved_keymaps())
}

/// Enumerate every chrome-action variant against any resolved table.
///
/// Shared code path (no fork): `list actions` passes the effective table
/// (config overrides + defaults, same table `list keymaps` reads); tests pass
/// synthetic tables to prove override-wins, default-fallback, and
/// manual-only-null.
#[must_use]
pub fn list_chrome_actions_with(
    resolved: &[bitty_config::keymap::ResolvedKeymap],
) -> Vec<ChromeActionInfo> {
    let mut out: Vec<ChromeActionInfo> = bitty_config::keymap::all_chrome_actions()
        .iter()
        .map(|action| ChromeActionInfo {
            id: action.canonical(),
            description: action.description().to_string(),
            manual_bind_only: action.is_manual_bind_only(),
            requires_runtime: action.requires_runtime(),
            binding: bitty_config::keymap::effective_binding_for_action(resolved, action),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Enumerate every registered core command (local, no I/O).
///
/// Pure: reuses [`crate::inspect::CORE_COMMANDS`] summaries directly, so the
/// `inspect command` and `list actions` surfaces can never disagree silently.
/// Sorted by `id` for stable output. `binding` is always `None`: core
/// commands run via `bitty cmd`/`ctl`, never via a chord.
#[must_use]
pub fn list_core_commands() -> Vec<CoreCommandInfo> {
    let mut out: Vec<CoreCommandInfo> = crate::inspect::CORE_COMMANDS
        .iter()
        .map(|c| CoreCommandInfo {
            id: c.id.to_string(),
            description: c.summary.to_string(),
            class: c.class.to_string(),
            requires_runtime: c.class == "runtime",
            kind: c.kind.to_string(),
            scopes: c.scopes.iter().map(|s| (*s).to_string()).collect(),
            owner: c.owner.to_string(),
            binding: None,
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ---------------------------------------------------------------------------
// Keymap catalog (CTX-1127, issue #1934)
// ---------------------------------------------------------------------------

/// One resolved keymap row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapInfo {
    /// Canonical chord (e.g. `alt+h`).
    pub chord: String,
    /// Canonical action (e.g. `goto_split:left`).
    pub action: String,
    /// One-line action description (canonical source).
    pub description: String,
    /// Keymap context (always `global` today).
    pub context: String,
    /// Owning layer (`default` for shipped entries, `user` for overrides).
    pub source: String,
}

/// Shipped `mod_key` plus its provenance (local, no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapMeta {
    /// Canonical `mod_key` (`alt` default, `super` when rebound).
    pub mod_key: String,
    /// Provenance (`default` for built-in defaults, `user` when the effective
    /// config carries overrides).
    pub source: String,
}

/// Effective `mod_key` for the shipped table (local, no I/O).
#[must_use]
#[allow(dead_code)]
pub fn keymap_meta() -> KeymapMeta {
    keymap_meta_for(&bitty_config::EffectiveConfig::default())
}

/// `mod_key` meta for any effective config.
///
/// Shared helper (no fork): the live `list keymaps` dispatch passes the
/// best-effort effective config (config overrides + defaults); tests pass
/// synthetic configs.
#[must_use]
pub fn keymap_meta_for(effective: &bitty_config::EffectiveConfig) -> KeymapMeta {
    let is_default = *effective == bitty_config::EffectiveConfig::default();
    KeymapMeta {
        mod_key: effective.mod_key.canonical().to_string(),
        source: if is_default {
            "default".to_string()
        } else {
            "user".to_string()
        },
    }
}

/// Enumerate the resolved chord to action table (local, no I/O).
///
/// Shared code path with `bitty inspect key` for the shipped case; the live
/// dispatch uses [`list_keymaps_with`] over the effective config (config
/// overrides + defaults, same [`resolve_keymaps`](bitty_config::keymap::resolve_keymaps)
/// resolver, no fork). Sorted by chord for stable output.
#[must_use]
#[allow(dead_code)]
pub fn list_keymaps() -> Vec<KeymapInfo> {
    list_keymaps_with(&bitty_config::keymap::shipped_resolved_keymaps())
}

/// Enumerate any resolved table (effective or shipped).
///
/// Shared code path (no fork): `list keymaps` passes the effective table and
/// `list actions` reuses the same table for `binding` lookup.
#[must_use]
pub fn list_keymaps_with(resolved: &[bitty_config::keymap::ResolvedKeymap]) -> Vec<KeymapInfo> {
    let mut out: Vec<KeymapInfo> = resolved
        .iter()
        .map(|entry| KeymapInfo {
            chord: entry.chord.canonical(),
            action: entry.action.canonical(),
            description: entry.action.description().to_string(),
            context: entry.context.clone(),
            source: if entry.from_default {
                "default".to_string()
            } else {
                "user".to_string()
            },
        })
        .collect();
    out.sort_by(|a, b| a.chord.cmp(&b.chord));
    out
}

/// Best-effort effective config for the catalog (local, no instance).
///
/// Tries the full merged config (CLI > file > profile > defaults, honors
/// `--safe`/`--config`/`BITTY_CONFIG`/profiles like the app startup); on any
/// load failure falls back to built-in defaults so `list actions` and `list
/// keymaps` always succeed (exit 0, safe-mode-clean fallback). The load error
/// is reported on stderr only, so JSON on stdout is never corrupted and the
/// exit code is unchanged.
#[must_use]
pub fn effective_for_catalog(args: &crate::cli::Args) -> bitty_config::EffectiveConfig {
    match crate::config_cli::load_merged_config(args) {
        Ok(loaded) => loaded.merged.effective,
        Err(err) => {
            eprintln!("bitty list: invalid config ({err}); using shipped defaults");
            bitty_config::EffectiveConfig::default()
        }
    }
}

/// Resolved keymap table for the catalog dispatch.
///
/// Same [`resolve_keymaps`](bitty_config::keymap::resolve_keymaps) resolver
/// for both `list actions` (`binding`) and `list keymaps` (rows): config
/// overrides + defaults. When the effective table fails to resolve (invalid
/// user keymaps), fall back to the defaults for the effective modifier so the
/// chords stay consistent with the reported `mod_key`; invalid user keymaps
/// never break the catalog (silent-success fallback, exit 0).
#[must_use]
pub fn resolved_for_catalog(
    effective: &bitty_config::EffectiveConfig,
) -> Vec<bitty_config::keymap::ResolvedKeymap> {
    match bitty_config::keymap::resolve_keymaps(effective) {
        Ok(table) => table,
        Err(_) => bitty_config::keymap::default_keymaps_with_mod(effective.mod_key)
            .unwrap_or_else(|_| bitty_config::keymap::shipped_resolved_keymaps()),
    }
}

// ---------------------------------------------------------------------------
// Service registration catalog (dynapi observability, bitty#2002)
// ---------------------------------------------------------------------------

/// One registered extension-interface row: Core-computed from validated
/// registration state, never provider-reported strings.
///
/// A row is one attachment: one backing head projected by one interface
/// version of one provider. Multi-head provisions expand to one row per
/// head, so every JSON value stays a scalar string and the keys stay
/// frozen (the #1934 catalog discipline: stable keys, documented
/// nullables, shared envelope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    /// Short interface name as declared (e.g. `calc.add`).
    pub interface: String,
    /// Owner segment of the providing plugin id (e.g. `xuepoo`).
    pub owner: String,
    /// Declared exact interface version (the negotiation candidate set is
    /// exactly these declarations; live per-attachment negotiation stays
    /// runtime-side).
    pub version: String,
    /// Providing plugin id (e.g. `xuepoo.calc`).
    pub provider: String,
    /// Single backing Core capability head this row projects.
    pub backing_head: String,
    /// Where the covering grant comes from: `manifest` (declared
    /// capabilities back it directly) or `install-record` (persisted
    /// consent backs it).
    pub grant_source: String,
    /// Registration status observed without loading any plugin VM:
    /// `active` (registered + grant-covered). Frozen as part of the row
    /// contract; reserved for future live states behind a live transport.
    pub status: String,
}

/// Registration status for listed rows (frozen contract value).
pub const SERVICE_STATUS_ACTIVE: &str = "active";

/// Grant provenance: declared capabilities back the provision directly
/// (bundled and development sources carry no persisted grant record).
pub const SERVICE_GRANT_MANIFEST: &str = "manifest";

/// Grant provenance: persisted install-record consent backs the provision
/// (store-resolved packages).
pub const SERVICE_GRANT_INSTALL_RECORD: &str = "install-record";

/// Table null marker for the services table.
///
/// v1 rows carry no nulls (every field is a required string; unbacked and
/// ungranted provisions never list), and JSON never emits null in v1.
/// `-` stays frozen as the marker an absent cell renders, following the
/// plugins table precedent, so tables can never disagree with JSON
/// silently.
pub const SERVICES_NULL: &str = "-";

/// Scrub one table cell: empty renders as the null marker, control
/// characters (terminal-injection vectors) render as the replacement
/// character. Defense in depth over manifest validation (which already
/// constrains every row charset): a hostile provider string can never
/// reach the terminal raw through this table.
#[must_use]
pub fn sanitize_service_cell(raw: &str) -> String {
    if raw.is_empty() {
        return SERVICES_NULL.to_string();
    }
    raw.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Owned provision source for the services catalog: one validated
/// manifest entry plus its provider identity and effective grant set.
struct ServiceProvisionSource {
    provider: String,
    owner: String,
    iface: String,
    version: String,
    heads: Vec<String>,
    grants: BTreeSet<String>,
    grant_source: &'static str,
}

/// One input provision for the services catalog: a validated manifest
/// entry plus its provider identity and effective grant set.
///
/// The builder below is generic over these inputs — no per-extension
/// branches exist anywhere on this path, so registering a new extension
/// needs no Core change (a regression test pins exactly that by varying
/// the provider count over the same builder).
pub struct ServiceListingInput<'a> {
    /// Providing plugin id.
    pub provider: &'a str,
    /// Owner segment of the provider id.
    pub owner: &'a str,
    /// Short interface name as declared.
    pub iface: &'a str,
    /// Declared exact interface version.
    pub version: &'a str,
    /// Declared backing Core capability heads.
    pub heads: &'a [String],
    /// Effective grant set backing this provider (recorded consent, else
    /// declared capabilities).
    pub grants: &'a BTreeSet<String>,
    /// Grant provenance (`SERVICE_GRANT_MANIFEST` / `SERVICE_GRANT_INSTALL_RECORD`).
    pub grant_source: &'static str,
}

/// Build service rows from validated inputs (pure, no I/O, no VM).
///
/// One row per (provision, head), sorted by (provider, interface, version,
/// backing head) for stable output. Unbacked provisions never record so
/// they never list (fail-closed); grant-uncovered provisions are skipped
/// as absent — indistinguishable from undeclared, the same posture as
/// `services.get` (a malicious provider can neither hide a registered
/// row nor inject display text: every field renders from validated
/// records through the escaping formatters).
#[must_use]
pub fn list_services_from_inputs(inputs: &[ServiceListingInput<'_>]) -> Vec<ServiceInfo> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<(&str, &str, &str, &str)> = BTreeSet::new();
    for input in inputs {
        for head in input.heads {
            if !input.grants.contains(head.as_str()) {
                continue;
            }
            let key = (input.provider, input.iface, input.version, head.as_str());
            if !seen.insert(key) {
                continue;
            }
            out.push(ServiceInfo {
                interface: input.iface.to_string(),
                owner: input.owner.to_string(),
                version: input.version.to_string(),
                provider: input.provider.to_string(),
                backing_head: head.clone(),
                grant_source: input.grant_source.to_string(),
                status: SERVICE_STATUS_ACTIVE.to_string(),
            });
        }
    }
    out.sort_by(|a, b| {
        (&a.provider, &a.interface, &a.version, &a.backing_head).cmp(&(
            &b.provider,
            &b.interface,
            &b.version,
            &b.backing_head,
        ))
    });
    out
}

/// Effective grant set plus provenance for one discovered package.
///
/// Store-resolved packages carry persisted consent (`install-record`);
/// bundled and development sources carry none, so their declared
/// capabilities back provisions directly (`manifest`).
fn service_effective_grants(
    manifest: &bitty_plugin_host::PluginManifest,
    recorded: Option<&[String]>,
) -> (BTreeSet<String>, &'static str) {
    match recorded {
        Some(granted) => (
            granted.iter().cloned().collect(),
            SERVICE_GRANT_INSTALL_RECORD,
        ),
        None => (
            manifest
                .capabilities
                .all_ids()
                .unwrap_or_default()
                .iter()
                .map(|id| id.as_str().to_string())
                .collect(),
            SERVICE_GRANT_MANIFEST,
        ),
    }
}

/// Enumerate registered extension interfaces (no plugin VM loaded).
///
/// Sources: the compiled-in bundled catalog plus a headless discovery
/// pass over development roots (`BITTY_PLUGIN_DIR`) and the install
/// store — manifests parse and validate, nothing activates, so an
/// invalid manifest fails its root closed (gone), never fatal, and never
/// executes. Safe-mode discovery skips third-party sources exactly like
/// startup, so `bitty --safe list services` stays safe-mode clean.
/// Every row is a registered, grant-covered provision; unbacked and
/// ungranted provisions are absent, never denied.
#[must_use]
pub fn list_services(safe_mode: bool) -> Vec<ServiceInfo> {
    let mut sources: Vec<ServiceProvisionSource> = Vec::new();
    for manifest in bitty_plugin_host::bundled::all_bundled_manifests() {
        let (grants, source) = service_effective_grants(&manifest, None);
        let id = manifest.id().clone();
        for svc in &manifest.provided_services {
            sources.push(ServiceProvisionSource {
                provider: id.to_string(),
                owner: id.owner().to_string(),
                iface: svc.iface.clone(),
                version: svc.version.clone(),
                heads: svc.heads.clone(),
                grants: grants.clone(),
                grant_source: source,
            });
        }
    }
    if !safe_mode {
        let mut runtime = bitty_runtime::plugin_runtime::PluginRuntime::new(
            bitty_runtime::plugin_runtime::PluginRuntimeConfig {
                safe_mode,
                data_dir: None,
                store_root: crate::plugin_runtime::store_root(),
                bundled_roots: Vec::new(),
                third_party_roots: crate::plugin_runtime::dev_roots(),
                settings: Rc::new(bitty_runtime::plugin_runtime::EmptySettings),
                snapshot: Rc::new(bitty_runtime::plugin_runtime::UnavailableSnapshot),
            },
        );
        let _ = runtime.discover();
        for id in runtime.discovered_ids() {
            let Some(package) = runtime.discovered_package(&id) else {
                continue;
            };
            let (grants, source) =
                service_effective_grants(&package.manifest, package.granted.as_deref());
            for svc in &package.manifest.provided_services {
                sources.push(ServiceProvisionSource {
                    provider: id.to_string(),
                    owner: id.owner().to_string(),
                    iface: svc.iface.clone(),
                    version: svc.version.clone(),
                    heads: svc.heads.clone(),
                    grants: grants.clone(),
                    grant_source: source,
                });
            }
        }
    }
    let borrowed: Vec<ServiceListingInput<'_>> = sources
        .iter()
        .map(|source| ServiceListingInput {
            provider: &source.provider,
            owner: &source.owner,
            iface: &source.iface,
            version: &source.version,
            heads: &source.heads,
            grants: &source.grants,
            grant_source: source.grant_source,
        })
        .collect();
    list_services_from_inputs(&borrowed)
}

// ---------------------------------------------------------------------------
// Instance discovery (reuses bitty-ipc::devtools discovery, no content fetch)
// ---------------------------------------------------------------------------

/// One discovered instance (discovery metadata only, never terminal content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInfo {
    /// Instance id (socket file stem, e.g. `default`).
    pub instance: String,
    /// Full socket path probed.
    pub socket: String,
    /// Whether a connect probe succeeded (live server on the other end).
    pub live: bool,
    /// Short detail (`live`, `stale: connection refused`, ...).
    pub detail: String,
}

/// Discovery failure (maps to exit 6/7 with an ok:false envelope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceError {
    /// Exit code (6 or 7).
    pub code: i32,
    /// Envelope error class (`Unavailable` or `Denied`).
    pub class: &'static str,
    /// Envelope error code (stable short token).
    pub error_code: &'static str,
    /// Human message (also stderr).
    pub message: String,
}

impl InstanceError {
    fn unavailable(message: String) -> Self {
        Self {
            code: EXIT_RUNTIME,
            class: "Unavailable",
            error_code: "InstanceUnavailable",
            message,
        }
    }

    /// Permission failure (Unix directory/socket authentication).
    /// Unused on non-Unix stubs; kept for taxonomy parity.
    #[allow(dead_code)]
    fn denied(message: String) -> Self {
        Self {
            code: EXIT_PERM,
            class: "Denied",
            error_code: "ScopeViolation",
            message,
        }
    }
}

/// Candidate socket directories to scan (injected for tests).
///
/// Production derives this from the advisory environment: the parent of
/// `BITTY_SOCKET` (when set) plus `$XDG_RUNTIME_DIR/bitty` (when set).
/// Missing env yields an empty vec (empty list, not an error).
#[must_use]
pub fn candidate_socket_dirs(
    bitty_socket: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(sock) = bitty_socket {
        if !sock.is_empty() && !sock.contains('\0') {
            if let Some(parent) = Path::new(sock).parent() {
                if !parent.as_os_str().is_empty() {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
    }
    if let Some(base) = xdg_runtime_dir {
        if !base.is_empty() && !base.contains('\0') {
            dirs.push(Path::new(base).join(bitty_ipc::devtools::SOCKET_LEAF_DIR));
        }
    }
    // De-duplicate while preserving order.
    let mut seen = std::collections::BTreeSet::new();
    dirs.into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Unix POSIX mode gate for the socket directory (fail-closed).
///
/// The directory must be `0700`: enumerating a directory another user could
/// have planted entries in is refused with `Denied` (exit 7). Missing
/// directories pass the gate (the scan core reports empty success); every
/// other violation fails here before any entry is read.
#[cfg(unix)]
fn unix_socket_dir_mode_gate(dir: &Path) -> Result<(), InstanceError> {
    use std::os::unix::fs::MetadataExt;

    if !dir.exists() {
        return Ok(());
    }
    let meta = std::fs::metadata(dir).map_err(|err| {
        let msg = err.to_string();
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            InstanceError::denied(format!(
                "bitty list: socket directory '{}' not accessible: {msg}",
                dir.display()
            ))
        } else {
            InstanceError::unavailable(format!(
                "bitty list: cannot read socket directory '{}': {msg}",
                dir.display()
            ))
        }
    })?;
    if !meta.is_dir() {
        return Err(InstanceError::unavailable(format!(
            "bitty list: socket path '{}' is not a directory",
            dir.display()
        )));
    }
    let mode = meta.mode() & 0o777;
    if mode != 0o700 {
        return Err(InstanceError::denied(format!(
            "bitty list: socket directory '{}' mode {mode:o} != 700 (refusing to enumerate; fix with chmod 700)",
            dir.display()
        )));
    }
    Ok(())
}

/// Scan one socket directory for `*.sock` entries (bounded).
///
/// - Missing directory: empty (no instances yet), not an error.
/// - Directory present but unreadable: `Unavailable` (exit 6).
/// - Directory mode `!= 0700` (Unix): `Denied` (exit 7, fail-closed: refuse
///   to enumerate a directory another user could have planted entries in).
/// - Past `MAX_LIST_INSTANCES` entries: keep the first N lexicographically,
///   note the shed count in the last row's `detail` (bounded output).
#[cfg(unix)]
pub fn scan_socket_dir(dir: &Path) -> Result<Vec<InstanceInfo>, InstanceError> {
    unix_socket_dir_mode_gate(dir)?;
    scan_registry_files_with_probe(dir, &probe_socket_live)
}

/// Shared registry-file scan core with an injected liveness probe (CTX-0196).
///
/// `probe` receives each `*.sock` entry path and returns `(live, detail)`.
/// Unix passes its `connect` probe; Windows passes its named-pipe probe;
/// tests pass fakes. Missing directory is empty success (no instances yet);
/// a non-directory or unreadable directory is `Unavailable`/`Denied`
/// fail-closed. There is deliberately no POSIX mode gate here: Windows ACLs
/// carry no `0700` bits, so ownership is enforced at the endpoint probe
/// (pipe ACL / `Denied` on access-class failures), not at the directory.
pub fn scan_registry_files_with_probe(
    dir: &Path,
    probe: &dyn Fn(&Path) -> (bool, String),
) -> Result<Vec<InstanceInfo>, InstanceError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let meta = std::fs::metadata(dir).map_err(|err| {
        let msg = err.to_string();
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            InstanceError::denied(format!(
                "bitty list: socket directory '{}' not accessible: {msg}",
                dir.display()
            ))
        } else {
            InstanceError::unavailable(format!(
                "bitty list: cannot read socket directory '{}': {msg}",
                dir.display()
            ))
        }
    })?;
    if !meta.is_dir() {
        return Err(InstanceError::unavailable(format!(
            "bitty list: socket path '{}' is not a directory",
            dir.display()
        )));
    }
    let entries = std::fs::read_dir(dir).map_err(|err| {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            InstanceError::denied(format!(
                "bitty list: socket directory '{}' not accessible: {err}",
                dir.display()
            ))
        } else {
            InstanceError::unavailable(format!(
                "bitty list: cannot read socket directory '{}': {err}",
                dir.display()
            ))
        }
    })?;
    let mut sockets: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "sock") {
            sockets.push(path);
        }
    }
    sockets.sort();
    let total = sockets.len();
    let capped: Vec<PathBuf> = sockets.into_iter().take(MAX_LIST_INSTANCES).collect();
    let mut out = Vec::with_capacity(capped.len());
    for path in capped {
        let instance = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let (live, detail) = probe(&path);
        out.push(InstanceInfo {
            instance,
            socket: path.to_string_lossy().to_string(),
            live,
            detail,
        });
    }
    if total > MAX_LIST_INSTANCES {
        if let Some(last) = out.last_mut() {
            last.detail = format!(
                "{} (shed {} of {total} entries past cap {MAX_LIST_INSTANCES})",
                last.detail,
                total - MAX_LIST_INSTANCES
            );
        }
    }
    Ok(out)
}

/// Windows instance discovery (CTX-0196): registry scan with named-pipe
/// liveness probes.
///
/// Each `<instance>.sock` registry marker maps to
/// `\\.\pipe\bitty-<instance>` (see
/// [`bitty_ipc::devtools::windows_pipe_name`]); the marker file itself
/// carries no liveness, the pipe open does. Live pipe-namespace enumeration
/// is merged separately in [`discover_instances`].
#[cfg(windows)]
pub fn scan_socket_dir(dir: &Path) -> Result<Vec<InstanceInfo>, InstanceError> {
    scan_registry_files_with_probe(dir, &|path| {
        let instance = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let pipe = bitty_ipc::devtools::windows_pipe_name(&instance);
        probe_windows_pipe(&pipe)
    })
}

/// Non-Unix, non-Windows stub: no registry mechanism exists on this platform
/// (same fail-soft shape as the IPC servo, but explicit rather than silent
/// for a query command).
#[cfg(not(any(unix, windows)))]
pub fn scan_socket_dir(dir: &Path) -> Result<Vec<InstanceInfo>, InstanceError> {
    let _ = dir;
    Err(InstanceError::unavailable(
        "bitty list: instance discovery is unavailable on this platform".to_string(),
    ))
}

/// Probe one socket path with endpoint verification plus connect
/// (bounded, no content exchange).
///
/// Returns `(live, detail)`. `live == true` only when the endpoint first
/// verifies (`0700` dir + `0600` socket, owned by the probing euid, no
/// symlinks — CTX-0528/IPC-001: a foreign-owned or tampered endpoint never
/// reaches `connect`) and `connect` then succeeds; every other outcome is
/// `live == false` with a short, non-sensitive detail (error kind only,
/// never OS handles or peer bytes).
#[cfg(unix)]
fn probe_socket_live(path: &Path) -> (bool, String) {
    // CTX-0528 (IPC-001): verify before connect — a planted entry pointing
    // at another owner's socket fails closed here, never reaching I/O.
    // Portable uid: `/proc/self` metadata is Linux-only (absent on macOS).
    let euid = bitty_ipc::current_unix_uid();
    if let Ok(uid) = euid {
        let path_str = path.to_string_lossy();
        if bitty_ipc::devtools::verify_socket_endpoint_for_connect(path_str.as_ref(), uid).is_err()
        {
            return (
                false,
                "unreachable: endpoint verification failed".to_string(),
            );
        }
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => (true, "live".to_string()),
        Err(err) => {
            let detail = match err.kind() {
                std::io::ErrorKind::NotFound => "stale: socket file missing".to_string(),
                std::io::ErrorKind::PermissionDenied => {
                    "unreachable: permission denied".to_string()
                }
                std::io::ErrorKind::ConnectionRefused => "stale: connection refused".to_string(),
                std::io::ErrorKind::TimedOut => "unreachable: timed out".to_string(),
                _ => format!("unreachable: {}", short_io_kind(err.kind())),
            };
            (false, detail)
        }
    }
}

/// Short stable token for an `io::ErrorKind` (no OS message bytes leaked).
#[cfg(any(unix, windows))]
fn short_io_kind(kind: std::io::ErrorKind) -> String {
    format!("{kind:?}")
        .chars()
        .take(48)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Bounded wait for a free pipe instance during discovery probes.
#[cfg(windows)]
const WINDOWS_PROBE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Probe one Windows named pipe with a bounded authenticated open (CTX-0196,
/// transport CTX-1405/#2244).
///
/// The probe uses the shared pipe client, which verifies the serving process
/// token SID against the current user before reporting success; a foreign
/// server the ACL let us reach is still not `live`. A missing pipe fails
/// `NotFound` (stale registry entry); a present-but-inaccessible pipe fails
/// `PermissionDenied`; a busy pipe waits at most 50 ms. The connect never
/// blocks unboundedly, mirroring the fail-fast Unix probe.
#[cfg(windows)]
fn probe_windows_pipe(pipe_path: &str) -> (bool, String) {
    match bitty_ipc::winpipe::connect(pipe_path, WINDOWS_PROBE_WAIT) {
        Ok(_) => (true, "live".to_string()),
        Err(err) => {
            let detail = match err.kind() {
                std::io::ErrorKind::NotFound => "stale: pipe not present".to_string(),
                std::io::ErrorKind::PermissionDenied => {
                    "unreachable: permission denied".to_string()
                }
                std::io::ErrorKind::TimedOut => "unreachable: pipe busy".to_string(),
                _ => format!("unreachable: {}", short_io_kind(err.kind())),
            };
            (false, detail)
        }
    }
}

/// Whether an explicit `--socket` value addresses the Windows pipe namespace
/// directly (device/UNC paths such as `\\.\pipe\bitty-default`).
#[cfg(windows)]
fn is_windows_pipe_path(value: &str) -> bool {
    value.starts_with(r"\\")
}

/// Label an explicitly addressed pipe for the instance column (CTX-0196).
///
/// Grammar-checked pipe names yield their instance id; anything else falls
/// back to the trailing path segment so diagnostics echo what was probed.
#[cfg(windows)]
fn windows_explicit_instance_label(pipe_path: &str) -> String {
    let file = pipe_path.rsplit('\\').next().unwrap_or(pipe_path);
    bitty_ipc::devtools::windows_instance_from_pipe_name(file).unwrap_or_else(|| file.to_string())
}

/// Convert listed pipe-namespace file names into live instance rows
/// (CTX-0196).
///
/// Pure transform over bare names as listed from `\\.\pipe\` (e.g.
/// `bitty-default`): foreign or malformed names are skipped, every kept row
/// is `live: true` — a listed pipe has a server holding a handle, and pipes
/// (unlike socket files) vanish with their server, so existence implies
/// liveness. Sorted by socket and capped at [`MAX_LIST_INSTANCES`] with the
/// shed count folded into the last row's `detail` (same bound shape as the
/// directory scan).
/// Map bare pipe-namespace names to candidate rows without applying the list
/// cap, so ownership probing filters before the bound is spent.
fn pipe_namespace_candidates(pipe_names: &[String]) -> Vec<InstanceInfo> {
    pipe_names
        .iter()
        .filter_map(|name| {
            let instance = bitty_ipc::devtools::windows_instance_from_pipe_name(name)?;
            let socket = format!("{}{}", bitty_ipc::devtools::WINDOWS_PIPE_NAMESPACE, name);
            Some(InstanceInfo {
                instance,
                socket,
                live: true,
                detail: "live".to_string(),
            })
        })
        .collect()
}

/// Sort rows by endpoint and truncate past [`MAX_LIST_INSTANCES`], folding the
/// shed count into the last surviving row.
fn cap_instance_rows(mut rows: Vec<InstanceInfo>) -> Vec<InstanceInfo> {
    rows.sort_by(|a, b| a.socket.cmp(&b.socket));
    let total = rows.len();
    rows.truncate(MAX_LIST_INSTANCES);
    if total > MAX_LIST_INSTANCES {
        if let Some(last) = rows.last_mut() {
            last.detail = format!(
                "{} (shed {} of {total} entries past cap {MAX_LIST_INSTANCES})",
                last.detail,
                total - MAX_LIST_INSTANCES
            );
        }
    }
    rows
}

/// Enumerate live bitty pipes from the Windows pipe namespace (CTX-0196).
///
/// Best-effort augmentation to the registry scan: a listing failure yields
/// empty (the registry scan stays authoritative) rather than failing the
/// whole command, since sandboxes may hide the namespace while the registry
/// directory remains readable.
#[cfg(windows)]
pub fn scan_pipe_namespace() -> Vec<InstanceInfo> {
    let names: Vec<String> = match std::fs::read_dir(bitty_ipc::devtools::WINDOWS_PIPE_NAMESPACE) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return Vec::new(),
    };
    // Namespace existence alone is not ownership: the authenticated probe
    // filters candidates before the `MAX_LIST_INSTANCES` cap, so a foreign
    // `bitty-*` pipe never consumes a surviving slot (CTX-1405, #2244).
    let live: Vec<InstanceInfo> = pipe_namespace_candidates(&names)
        .into_iter()
        .filter(|row| probe_windows_pipe(&row.socket).0)
        .collect();
    cap_instance_rows(live)
}

/// Non-Windows: no pipe namespace exists; the registry scan is authoritative.
///
/// Routes through the shared candidate transform so the
/// [`discover_instances`] merge stays uniform across platforms.
#[cfg(not(windows))]
pub fn scan_pipe_namespace() -> Vec<InstanceInfo> {
    cap_instance_rows(pipe_namespace_candidates(&[]))
}

/// Merge registry rows with live pipe-namespace rows (CTX-0196).
///
/// Dedups by `socket`, sorts by `socket`, truncates past
/// [`MAX_LIST_INSTANCES`]. A pipe row and a registry row for the same
/// instance carry different `socket` spellings (pipe path vs registry file),
/// so both survive the merge; byte-identical paths dedup exactly like
/// repeated candidate directories do in [`discover_instances`].
pub fn merge_instance_rows(
    mut primary: Vec<InstanceInfo>,
    secondary: Vec<InstanceInfo>,
) -> Vec<InstanceInfo> {
    let mut seen: std::collections::BTreeSet<String> =
        primary.iter().map(|row| row.socket.clone()).collect();
    for row in secondary {
        if seen.insert(row.socket.clone()) {
            primary.push(row);
        }
    }
    primary.sort_by(|a, b| a.socket.cmp(&b.socket));
    if primary.len() > MAX_LIST_INSTANCES {
        primary.truncate(MAX_LIST_INSTANCES);
    }
    primary
}
/// Discover instances honoring explicit overrides.
///
/// - `socket = Some(path)`: probe exactly that path. Missing file is
///   `Unavailable` (6); `PermissionDenied` on connect is `Denied` (7);
///   any other connect failure is reported as a single `live: false` row
///   (stale socket still conveys useful state, exit 0).
/// - `instance = Some(id)`: resolve via `devtools::resolve_socket_path`
///   (advisory env + portable bound) then probe like `socket`.
/// - Neither: scan every [`candidate_socket_dirs`] entry, concatenating rows
///   (dedup by socket path, sorted). On Windows, live pipe-namespace rows
///   from [`scan_pipe_namespace`] are merged in as well. Directory-level
///   auth failures abort fail-closed (first error wins); per-socket probe
///   failures are rows.
pub fn discover_instances(
    socket: Option<&str>,
    instance: Option<&str>,
) -> Result<Vec<InstanceInfo>, InstanceError> {
    if let Some(path) = socket {
        return probe_explicit_socket(path);
    }
    if let Some(id) = instance {
        let resolved = resolve_instance_socket(id)?;
        return probe_explicit_socket(&resolved);
    }
    let env = bitty_ipc::devtools::SocketEnv::from_process_env();
    let dirs = candidate_socket_dirs(env.bitty_socket.as_deref(), env.xdg_runtime_dir.as_deref());
    let mut merged: Vec<InstanceInfo> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for dir in dirs {
        let rows = scan_socket_dir(&dir)?;
        for row in rows {
            if seen.insert(row.socket.clone()) {
                merged.push(row);
            }
        }
    }
    // Windows contributes live pipe-namespace rows; elsewhere this is empty
    // and the merge is a no-op sort/truncate (uniform bound enforcement).
    merged = merge_instance_rows(merged, scan_pipe_namespace());
    merged.sort_by(|a, b| a.socket.cmp(&b.socket));
    if merged.len() > MAX_LIST_INSTANCES {
        merged.truncate(MAX_LIST_INSTANCES);
    }
    Ok(merged)
}

/// Resolve an explicit `--instance` id to a socket path via the shared
/// discovery (advisory env, portable bound). Shape already validated; this
/// maps resolution failures to exit 6.
fn resolve_instance_socket(id: &str) -> Result<String, InstanceError> {
    let env = bitty_ipc::devtools::SocketEnv::from_process_env();
    // Prefer the caller's explicit id over the inherited one.
    let with_id = bitty_ipc::devtools::SocketEnv {
        bitty_socket: env.bitty_socket.clone(),
        xdg_runtime_dir: env.xdg_runtime_dir.clone(),
        instance_id: Some(id.to_string()),
    };
    bitty_ipc::devtools::resolve_socket_path_from_env(&with_id, None)
        .map(|(path, _)| path)
        .map_err(|err| {
            InstanceError::unavailable(format!("bitty list: cannot resolve instance {id:?}: {err}"))
        })
}

/// Probe one explicit socket path (shared by `--socket`/`--instance`).
#[cfg(unix)]
fn probe_explicit_socket(path_str: &str) -> Result<Vec<InstanceInfo>, InstanceError> {
    use std::os::unix::fs::MetadataExt;

    let path = Path::new(path_str);
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            // Socket file must be a socket; a regular file at that path is a
            // stale/misplaced entry (report, don't error), except permission
            // failures which are auth errors.
            let _ = meta.mode();
            let instance = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.to_string());
            // CTX-0528 (IPC-001): verify the endpoint before connect — a
            // forged `--socket`/planted entry pointing at another owner's
            // endpoint fails closed here, never reaching I/O.
            // Portable uid: `/proc/self` metadata is Linux-only (absent on
            // macOS, where it fell back to 0).
            let uid = bitty_ipc::current_unix_uid().unwrap_or(0);
            if bitty_ipc::devtools::verify_socket_endpoint_for_connect(path_str, uid).is_err() {
                return Ok(vec![InstanceInfo {
                    instance,
                    socket: path_str.to_string(),
                    live: false,
                    detail: "unreachable: endpoint verification failed".to_string(),
                }]);
            }
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => Ok(vec![InstanceInfo {
                    instance,
                    socket: path_str.to_string(),
                    live: true,
                    detail: "live".to_string(),
                }]),
                Err(err) => match err.kind() {
                    std::io::ErrorKind::PermissionDenied => Err(InstanceError::denied(format!(
                        "bitty list: socket '{path_str}' not accessible: permission denied"
                    ))),
                    std::io::ErrorKind::NotFound => Err(InstanceError::unavailable(format!(
                        "bitty list: socket '{path_str}' not found"
                    ))),
                    _ => Ok(vec![InstanceInfo {
                        instance,
                        socket: path_str.to_string(),
                        live: false,
                        detail: format!("stale: {}", short_io_kind(err.kind())),
                    }]),
                },
            }
        }
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => Err(InstanceError::unavailable(format!(
                "bitty list: socket '{path_str}' not found"
            ))),
            std::io::ErrorKind::PermissionDenied => Err(InstanceError::denied(format!(
                "bitty list: socket '{path_str}' not accessible: permission denied"
            ))),
            _ => Err(InstanceError::unavailable(format!(
                "bitty list: cannot stat socket '{path_str}': {err}"
            ))),
        },
    }
}

/// Windows explicit probe for `--socket`/`--instance` (CTX-0196).
///
/// Pipe-namespace paths (`\\...`) are probed directly; filesystem paths
/// resolve their file stem to the matching `bitty-<instance>` pipe (the
/// registry file itself carries no liveness, mirroring how the directory
/// scan probes pipes rather than files). Missing paths are `Unavailable`
/// (exit 6, parity with Unix); `PermissionDenied` at stat or open is
/// `Denied` (exit 7); any other open failure is a single `live: false`
/// stale row (exit 0, parity with Unix).
#[cfg(windows)]
fn probe_explicit_socket(path_str: &str) -> Result<Vec<InstanceInfo>, InstanceError> {
    if is_windows_pipe_path(path_str) {
        let instance = windows_explicit_instance_label(path_str);
        return match bitty_ipc::winpipe::connect(path_str, WINDOWS_PROBE_WAIT) {
            Ok(_) => Ok(vec![InstanceInfo {
                instance,
                socket: path_str.to_string(),
                live: true,
                detail: "live".to_string(),
            }]),
            Err(err) => match err.kind() {
                std::io::ErrorKind::NotFound => Err(InstanceError::unavailable(format!(
                    "bitty list: socket '{path_str}' not found"
                ))),
                std::io::ErrorKind::PermissionDenied => Err(InstanceError::denied(format!(
                    "bitty list: socket '{path_str}' not accessible: permission denied"
                ))),
                _ => Ok(vec![InstanceInfo {
                    instance,
                    socket: path_str.to_string(),
                    live: false,
                    detail: format!("stale: {}", short_io_kind(err.kind())),
                }]),
            },
        };
    }
    let path = Path::new(path_str);
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let instance = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.to_string());
            let pipe = bitty_ipc::devtools::windows_pipe_name(&instance);
            match bitty_ipc::winpipe::connect(&pipe, WINDOWS_PROBE_WAIT) {
                Ok(_) => Ok(vec![InstanceInfo {
                    instance,
                    socket: path_str.to_string(),
                    live: true,
                    detail: "live".to_string(),
                }]),
                Err(err) => match err.kind() {
                    std::io::ErrorKind::PermissionDenied => Err(InstanceError::denied(format!(
                        "bitty list: socket '{path_str}' not accessible: permission denied"
                    ))),
                    std::io::ErrorKind::NotFound => Ok(vec![InstanceInfo {
                        instance,
                        socket: path_str.to_string(),
                        live: false,
                        detail: "stale: pipe not present".to_string(),
                    }]),
                    _ => Ok(vec![InstanceInfo {
                        instance,
                        socket: path_str.to_string(),
                        live: false,
                        detail: format!("stale: {}", short_io_kind(err.kind())),
                    }]),
                },
            }
        }
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => Err(InstanceError::unavailable(format!(
                "bitty list: socket '{path_str}' not found"
            ))),
            std::io::ErrorKind::PermissionDenied => Err(InstanceError::denied(format!(
                "bitty list: socket '{path_str}' not accessible: permission denied"
            ))),
            _ => Err(InstanceError::unavailable(format!(
                "bitty list: cannot stat socket '{path_str}': {err}"
            ))),
        },
    }
}

/// Non-Unix, non-Windows stub for explicit probes.
#[cfg(not(any(unix, windows)))]
fn probe_explicit_socket(path_str: &str) -> Result<Vec<InstanceInfo>, InstanceError> {
    let _ = path_str;
    Err(InstanceError::unavailable(
        "bitty list: instance discovery is unavailable on this platform".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Escape a string for embedding in JSON output (control bytes safe).
#[must_use]
pub fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

use std::fmt::Write as _;

/// Render the success envelope for a fully materialized result.
#[must_use]
pub fn format_success_envelope(
    spelling: &str,
    kind: ListKind,
    themes: &[ThemeInfo],
    plugins: &[PluginInfo],
    instances: &[InstanceInfo],
) -> String {
    let mut out = String::with_capacity(1024);
    let _ = write!(
        out,
        "{{\"v\":1,\"command\":\"{}\",\"ok\":true,\"result\":{{\"kind\":\"{}\"",
        json_escape(spelling),
        kind.name()
    );
    match kind {
        ListKind::Themes => {
            let _ = write!(out, ",\"count\":{},\"themes\":[", themes.len());
            for (i, t) in themes.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{{\"name\":\"{}\",\"aliases\":[", json_escape(&t.name));
                for (j, a) in t.aliases.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "\"{}\"", json_escape(a));
                }
                let _ = write!(
                    out,
                    "],\"category\":\"{}\",\"source\":\"{}\",\"provenance\":\"{}\",\"license\":\"{}\",\"background\":\"{}\",\"foreground\":\"{}\",\"cursor\":\"{}\",\"selection\":\"{}\"}}",
                    json_escape(&t.category),
                    json_escape(&t.source),
                    json_escape(&t.provenance),
                    json_escape(&t.license),
                    json_escape(&t.background),
                    json_escape(&t.foreground),
                    json_escape(&t.cursor),
                    json_escape(&t.selection)
                );
            }
            out.push(']');
        }
        ListKind::Plugins => {
            let _ = write!(out, ",\"count\":{},\"plugins\":[", plugins.len());
            for (i, p) in plugins.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "{{\"id\":\"{}\",\"name\":\"{}\",\"version\":\"{}\",\"description\":\"{}\",\"bundled\":{},\"enabled\":{}",
                    json_escape(&p.id),
                    json_escape(&p.name),
                    json_escape(&p.version),
                    json_escape(&p.description),
                    p.bundled,
                    p.enabled
                );
                out.push_str(",\"commands\":[");
                for (j, c) in p.commands.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "\"{}\"", json_escape(c));
                }
                out.push_str("]}");
            }
            out.push(']');
        }
        ListKind::Instances => {
            let _ = write!(out, ",\"count\":{},\"instances\":[", instances.len());
            for (i, inst) in instances.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "{{\"instance\":\"{}\",\"socket\":\"{}\",\"live\":{},\"detail\":\"{}\"}}",
                    json_escape(&inst.instance),
                    json_escape(&inst.socket),
                    inst.live,
                    json_escape(&inst.detail)
                );
            }
            out.push(']');
        }
        ListKind::Actions => {
            // Legacy entry point carries no action rows; the `actions` dispatch
            // uses `format_actions_envelope`. Emit the stable empty shape so
            // the envelope stays total over every kind.
            out.push_str(",\"count\":0,\"actions\":[],\"commands\":[]");
        }
        ListKind::Keymaps => {
            // Same total-shape note as `Actions`: the `keymaps` dispatch uses
            // `format_keymaps_envelope`.
            let meta = keymap_meta();
            let _ = write!(
                out,
                ",\"count\":0,\"mod_key\":\"{}\",\"source\":\"{}\",\"keymaps\":[]",
                json_escape(&meta.mod_key),
                json_escape(&meta.source)
            );
        }
        ListKind::Services => {
            // Same total-shape note as `Actions`: the `services` dispatch uses
            // `format_services_envelope`.
            out.push_str(",\"count\":0,\"services\":[]");
        }
    }
    out.push_str("}}");
    out
}

/// Render the success envelope for `bitty list actions` (CTX-1127).
///
/// Stable keys: `kind`, `count` (actions plus commands), `actions` (array of
/// `{"id","description","manual_bind_only","requires_runtime","binding","kind":"chrome"}`),
/// `commands` (array of
/// `{"id","description","class","requires_runtime","binding","kind","scopes","owner"}`).
/// `binding` is the effective chord (user override first, default fallback,
/// null only for manual-bind-only chrome actions; always null for core
/// commands). Documented in `list --help` output contract.
#[must_use]
pub fn format_actions_envelope(
    spelling: &str,
    actions: &[ChromeActionInfo],
    commands: &[CoreCommandInfo],
) -> String {
    let mut out = String::with_capacity(2048);
    let _ = write!(
        out,
        "{{\"v\":1,\"command\":\"{}\",\"ok\":true,\"result\":{{\"kind\":\"actions\",\"count\":{}",
        json_escape(spelling),
        actions.len() + commands.len()
    );
    out.push_str(",\"actions\":[");
    for (i, a) in actions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let binding = a
            .binding
            .as_deref()
            .map_or_else(|| "null".to_string(), |b| format!("\"{}\"", json_escape(b)));
        let _ = write!(
            out,
            "{{\"id\":\"{}\",\"description\":\"{}\",\"manual_bind_only\":{},\"requires_runtime\":{},\"binding\":{binding},\"kind\":\"chrome\"}}",
            json_escape(&a.id),
            json_escape(&a.description),
            a.manual_bind_only,
            a.requires_runtime
        );
    }
    out.push_str("],\"commands\":[");
    for (i, c) in commands.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let mut scopes = String::from("[");
        for (j, s) in c.scopes.iter().enumerate() {
            if j > 0 {
                scopes.push(',');
            }
            let _ = write!(scopes, "\"{}\"", json_escape(s));
        }
        scopes.push(']');
        let binding = c
            .binding
            .as_deref()
            .map_or_else(|| "null".to_string(), |b| format!("\"{}\"", json_escape(b)));
        let _ = write!(
            out,
            "{{\"id\":\"{}\",\"description\":\"{}\",\"class\":\"{}\",\"requires_runtime\":{},\"binding\":{binding},\"kind\":\"{}\",\"scopes\":{scopes},\"owner\":\"{}\"}}",
            json_escape(&c.id),
            json_escape(&c.description),
            json_escape(&c.class),
            c.requires_runtime,
            json_escape(&c.kind),
            json_escape(&c.owner),
        );
    }
    out.push_str("]}}");
    out
}

/// Render the success envelope for `bitty list keymaps` (CTX-1127).
///
/// Stable keys: `kind`, `count`, `mod_key`, `source`, `keymaps` (array of
/// `{"chord","action","description","context","source"}`). Documented in
/// `list --help` output contract.
#[must_use]
pub fn format_keymaps_envelope(
    spelling: &str,
    meta: &KeymapMeta,
    keymaps: &[KeymapInfo],
) -> String {
    let mut out = String::with_capacity(2048);
    let _ = write!(
        out,
        "{{\"v\":1,\"command\":\"{}\",\"ok\":true,\"result\":{{\"kind\":\"keymaps\",\"count\":{},\"mod_key\":\"{}\",\"source\":\"{}\",\"keymaps\":[",
        json_escape(spelling),
        keymaps.len(),
        json_escape(&meta.mod_key),
        json_escape(&meta.source)
    );
    for (i, k) in keymaps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"chord\":\"{}\",\"action\":\"{}\",\"description\":\"{}\",\"context\":\"{}\",\"source\":\"{}\"}}",
            json_escape(&k.chord),
            json_escape(&k.action),
            json_escape(&k.description),
            json_escape(&k.context),
            json_escape(&k.source)
        );
    }
    out.push_str("]}}");
    out
}

/// Render the success envelope for `bitty list services` (dynapi
/// observability, bitty#2002).
///
/// Frozen keys: `kind`, `count`, `services` (array of
/// `{"interface","owner","version","provider","backing_head","grant_source","status"}`).
/// Every value is a required string — v1 never emits null. Documented in
/// `list --help` output contract.
#[must_use]
pub fn format_services_envelope(spelling: &str, services: &[ServiceInfo]) -> String {
    let mut out = String::with_capacity(2048);
    let _ = write!(
        out,
        "{{\"v\":1,\"command\":\"{}\",\"ok\":true,\"result\":{{\"kind\":\"services\",\"count\":{},\"services\":[",
        json_escape(spelling),
        services.len()
    );
    for (i, s) in services.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"interface\":\"{}\",\"owner\":\"{}\",\"version\":\"{}\",\"provider\":\"{}\",\"backing_head\":\"{}\",\"grant_source\":\"{}\",\"status\":\"{}\"}}",
            json_escape(&s.interface),
            json_escape(&s.owner),
            json_escape(&s.version),
            json_escape(&s.provider),
            json_escape(&s.backing_head),
            json_escape(&s.grant_source),
            json_escape(&s.status)
        );
    }
    out.push_str("]}}");
    out
}

/// Render services as a human table.
///
/// Columns mirror the frozen JSON keys; absent cells render the reserved
/// null marker (`-`, currently unreachable — v1 rows are total) and
/// control characters are scrubbed so hostile provider strings render
/// safely.
#[must_use]
pub fn format_services_table(services: &[ServiceInfo], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    out.push_str(&format!(
        "{}\n",
        bold(
            "INTERFACE OWNER VERSION PROVIDER BACKING_HEAD GRANT_SOURCE STATUS",
            color
        )
    ));
    if services.is_empty() {
        out.push_str("(no services)\n");
        return out;
    }
    for s in services {
        out.push_str(&format!(
            "{} {} {} {} {} {} {}\n",
            sanitize_service_cell(&s.interface),
            sanitize_service_cell(&s.owner),
            sanitize_service_cell(&s.version),
            sanitize_service_cell(&s.provider),
            sanitize_service_cell(&s.backing_head),
            sanitize_service_cell(&s.grant_source),
            sanitize_service_cell(&s.status)
        ));
    }
    out
}

/// Render the failure envelope for `instances` runtime/permission errors.
///
/// `result` carries the empty instance list so machine clients always see
/// `kind`/`count` even on failure.
#[must_use]
pub fn format_error_envelope(spelling: &str, kind: ListKind, err: &InstanceError) -> String {
    format!(
        "{{\"v\":1,\"command\":\"{}\",\"ok\":false,\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}},\"result\":{{\"kind\":\"{}\",\"count\":0,\"instances\":[]}}}}",
        json_escape(spelling),
        err.class,
        json_escape(err.error_code),
        json_escape(&err.message),
        kind.name()
    )
}

// ---------------------------------------------------------------------------
// Table rendering (human, not a machine contract)
// ---------------------------------------------------------------------------

/// Whether ANSI color is enabled (explicit flag plus unified CLI gate).
///
/// Delegates to [`crate::color::cli_color_enabled`]: no `--no-color`, no
/// `NO_COLOR`, `TERM` is not `dumb`, and stdout is a terminal. Piped output
/// stays plain so headless runs carry no ANSI.
#[must_use]
pub fn color_enabled(no_color: bool) -> bool {
    crate::color::cli_color_enabled(no_color)
}

fn bold(s: &str, color: bool) -> String {
    if color {
        format!("\u{1b}[1m{s}\u{1b}[0m")
    } else {
        s.to_string()
    }
}

/// Parse `#rrggbb` into RGB bytes.
///
/// Accepts exactly the shape [`rgb_to_hex`] emits (lowercase or uppercase
/// hex, leading `#`). Returns `None` for any other shape so callers can
/// fall back to plain hex rather than emitting a wrong swatch.
#[must_use]
pub fn parse_hex_rgb(hex: &str) -> Option<[u8; 3]> {
    let digits = hex.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let g = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let b = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Render a background cell: swatch block in the bg color plus plain hex.
///
/// The hex stays verbatim so the row remains greppable and existing
/// `contains("#rrggbb")` assertions keep passing. Unparseable input falls
/// back to the plain hex (never a wrong color).
#[must_use]
pub fn swatch_bg(hex: &str) -> String {
    match parse_hex_rgb(hex) {
        Some([r, g, b]) => format!("\u{1b}[48;2;{r};{g};{b}m  \u{1b}[0m {hex}"),
        None => hex.to_string(),
    }
}

/// Render a foreground cell: hex sample text in the fg color.
///
/// The hex stays verbatim (colored, not stripped) so the row remains
/// greppable. Unparseable input falls back to the plain hex.
#[must_use]
pub fn swatch_fg(hex: &str) -> String {
    match parse_hex_rgb(hex) {
        Some([r, g, b]) => format!("\u{1b}[38;2;{r};{g};{b}m{hex}\u{1b}[0m"),
        None => hex.to_string(),
    }
}

/// Render themes as a human table.
///
/// When color is enabled (see [`color_enabled`]: no `--no-color`,
/// no `NO_COLOR`, `TERM` is not `dumb`, stdout is a terminal), the
/// BACKGROUND column carries a
/// truecolor swatch block (`48;2`) plus the hex, and the FOREGROUND column
/// carries the hex as sample text in the fg color (`38;2`). JSON output is
/// untouched (plain hex). Sibling `plugins`/`instances` tables print no
/// color values, so they keep their plain-text cells unchanged.
#[must_use]
pub fn format_themes_table(themes: &[ThemeInfo], no_color: bool) -> String {
    format_themes_table_with_color(themes, color_enabled(no_color))
}

/// Test hook: render the themes table with an explicit color decision.
///
/// `format_themes_table` resolves `color` via [`color_enabled`] (which reads
/// process env plus `stdout().is_terminal()`); this hook takes it directly
/// so unit tests stay deterministic without touching `NO_COLOR`/`TERM` and
/// so forced color still renders when piped.
#[must_use]
pub fn format_themes_table_with_color(themes: &[ThemeInfo], color: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}\n",
        bold(
            "NAME CATEGORY ALIASES PROVENANCE BACKGROUND FOREGROUND",
            color
        )
    ));
    if themes.is_empty() {
        out.push_str("(no themes)\n");
        return out;
    }
    for t in themes {
        let background = if color {
            swatch_bg(&t.background)
        } else {
            t.background.clone()
        };
        let foreground = if color {
            swatch_fg(&t.foreground)
        } else {
            t.foreground.clone()
        };
        out.push_str(&format!(
            "{} {} {} {} {} {}\n",
            t.name,
            t.category,
            if t.aliases.is_empty() {
                "-".to_string()
            } else {
                t.aliases.join(",")
            },
            t.source,
            background,
            foreground
        ));
    }
    out
}

/// Render plugins as a human table.
#[must_use]
pub fn format_plugins_table(plugins: &[PluginInfo], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    out.push_str(&format!(
        "{}\n",
        bold("ID VERSION BUNDLED ENABLED COMMANDS", color)
    ));
    if plugins.is_empty() {
        out.push_str("(no plugins)\n");
        return out;
    }
    for p in plugins {
        out.push_str(&format!(
            "{} {} {} {} {}\n",
            p.id,
            p.version,
            if p.bundled { "yes" } else { "no" },
            if p.enabled { "yes" } else { "no" },
            if p.commands.is_empty() {
                "-".to_string()
            } else {
                p.commands.join(",")
            }
        ));
    }
    out
}

/// Render instances as a human table.
#[must_use]
pub fn format_instances_table(instances: &[InstanceInfo], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    out.push_str(&format!("{}\n", bold("INSTANCE SOCKET LIVE DETAIL", color)));
    if instances.is_empty() {
        out.push_str(
            "(no instances — start bitty to serve an endpoint, or set BITTY_SOCKET/XDG_RUNTIME_DIR)\n",
        );
        return out;
    }
    for inst in instances {
        out.push_str(&format!(
            "{} {} {} {}\n",
            inst.instance,
            inst.socket,
            if inst.live { "yes" } else { "no" },
            inst.detail
        ));
    }
    out
}

/// Render actions as a human table (CTX-1127).
///
/// Two sections: chrome actions (`KIND chrome`) then core commands (`KIND
/// command`). Descriptions reuse the canonical sources; binding shows the
/// effective chord (user override first, default fallback, `-` when
/// manual-bind-only); manual-bind-only and runtime columns mark entries
/// needing explicit binds or live state.
#[must_use]
pub fn format_actions_table(
    actions: &[ChromeActionInfo],
    commands: &[CoreCommandInfo],
    no_color: bool,
) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    out.push_str(&format!(
        "{}\n",
        bold("KIND ID BINDING DESCRIPTION MANUAL_BIND RUNTIME", color)
    ));
    if actions.is_empty() && commands.is_empty() {
        out.push_str("(no actions)\n");
        return out;
    }
    for a in actions {
        out.push_str(&format!(
            "chrome {} {} {} {} {}\n",
            a.id,
            a.binding.as_deref().unwrap_or("-"),
            a.description,
            if a.manual_bind_only { "yes" } else { "no" },
            if a.requires_runtime {
                "runtime"
            } else {
                "local"
            }
        ));
    }
    for c in commands {
        out.push_str(&format!(
            "command {} - {} - {}\n",
            c.id, c.description, c.class
        ));
    }
    out
}

/// Render keymaps as a human table (CTX-1127).
///
/// Header names the `mod_key` context and source so the table carries the
/// same meta as the JSON envelope. Rows are `chord action context source`.
#[must_use]
pub fn format_keymaps_table(meta: &KeymapMeta, keymaps: &[KeymapInfo], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    out.push_str(&format!(
        "# mod_key: {} (source: {})\n",
        meta.mod_key, meta.source
    ));
    out.push_str(&format!("{}\n", bold("CHORD ACTION CONTEXT SOURCE", color)));
    if keymaps.is_empty() {
        out.push_str("(no keymaps)\n");
        return out;
    }
    for k in keymaps {
        out.push_str(&format!(
            "{} {} {} {}\n",
            k.chord, k.action, k.context, k.source
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Dispatch (called by main.rs; returns process exit code)
// ---------------------------------------------------------------------------

/// Run `bitty list` from a validated request; prints to stdout, diagnostics
/// to stderr, and returns the process exit code.
///
/// - Themes/plugins/actions/keymaps/services always succeed (exit 0, even
///   when empty in tests). Actions and keymaps are local (no instance);
///   the catalog dispatch reads the user file best-effort with
///   shipped-default fallback (see [`run_list_with_effective`]).
///   Services discovers manifests without loading any plugin VM.
/// - Instances maps discovery failures to ok:false envelopes for json/jsonl
///   (exit 6/7) and to stderr + exit code for table (no partial table).
pub fn run_list(request: &ListRequest) -> i32 {
    run_list_with_effective(request, &bitty_config::EffectiveConfig::default(), false)
}

/// Run `bitty list` against any effective config.
///
/// Shared resolver (no fork): both `Actions` bindings and `Keymaps` rows come
/// from the same [`resolved_for_catalog`] table (config overrides + defaults).
/// `safe_mode` mirrors startup for the `themes` and `services` catalogs
/// (`--safe` skips local theme files and third-party sources); every other
/// kind ignores it.
pub fn run_list_with_effective(
    request: &ListRequest,
    effective: &bitty_config::EffectiveConfig,
    safe_mode: bool,
) -> i32 {
    match request.kind {
        ListKind::Themes => {
            // CTX-2464: normal listing includes accepted local files; `--safe`
            // keeps the catalog built-in-only (no external config reads), so
            // the safe-mode path stays byte-identical to the shipped catalog.
            let (themes, diagnostics) = if safe_mode {
                (list_themes(), Vec::new())
            } else {
                list_themes_with_local()
            };
            for diagnostic in &diagnostics {
                eprintln!(
                    "bitty list themes: {}: {}",
                    diagnostic.file, diagnostic.message
                );
            }
            match request.format {
                ListFormat::Table => {
                    print!("{}", format_themes_table(&themes, request.no_color));
                }
                ListFormat::Json | ListFormat::Jsonl => {
                    println!(
                        "{}",
                        format_success_envelope(&request.spelling, request.kind, &themes, &[], &[])
                    );
                }
            }
            EXIT_OK
        }
        ListKind::Plugins => {
            let plugins = list_plugins();
            match request.format {
                ListFormat::Table => {
                    print!("{}", format_plugins_table(&plugins, request.no_color));
                }
                ListFormat::Json | ListFormat::Jsonl => {
                    println!(
                        "{}",
                        format_success_envelope(
                            &request.spelling,
                            request.kind,
                            &[],
                            &plugins,
                            &[]
                        )
                    );
                }
            }
            EXIT_OK
        }
        ListKind::Instances => {
            match discover_instances(request.socket.as_deref(), request.instance.as_deref()) {
                Ok(instances) => {
                    match request.format {
                        ListFormat::Table => {
                            print!("{}", format_instances_table(&instances, request.no_color));
                        }
                        ListFormat::Json | ListFormat::Jsonl => {
                            println!(
                                "{}",
                                format_success_envelope(
                                    &request.spelling,
                                    request.kind,
                                    &[],
                                    &[],
                                    &instances
                                )
                            );
                        }
                    }
                    EXIT_OK
                }
                Err(err) => {
                    match request.format {
                        ListFormat::Table => {
                            eprintln!("{}", err.message);
                        }
                        ListFormat::Json | ListFormat::Jsonl => {
                            println!(
                                "{}",
                                format_error_envelope(&request.spelling, request.kind, &err)
                            );
                            eprintln!("{}", err.message);
                        }
                    }
                    err.code
                }
            }
        }
        ListKind::Actions => {
            let resolved = resolved_for_catalog(effective);
            let actions = list_chrome_actions_with(&resolved);
            let commands = list_core_commands();
            match request.format {
                ListFormat::Table => {
                    print!(
                        "{}",
                        format_actions_table(&actions, &commands, request.no_color)
                    );
                }
                ListFormat::Json | ListFormat::Jsonl => {
                    println!(
                        "{}",
                        format_actions_envelope(&request.spelling, &actions, &commands)
                    );
                }
            }
            EXIT_OK
        }
        ListKind::Keymaps => {
            let meta = keymap_meta_for(effective);
            let keymaps = list_keymaps_with(&resolved_for_catalog(effective));
            match request.format {
                ListFormat::Table => {
                    print!(
                        "{}",
                        format_keymaps_table(&meta, &keymaps, request.no_color)
                    );
                }
                ListFormat::Json | ListFormat::Jsonl => {
                    println!(
                        "{}",
                        format_keymaps_envelope(&request.spelling, &meta, &keymaps)
                    );
                }
            }
            EXIT_OK
        }
        ListKind::Services => run_list_services(request, safe_mode),
    }
}

/// Run `bitty list services` from a validated request (dynapi
/// observability, bitty#2002).
///
/// Discovery is VM-less (manifests parse and validate, nothing
/// activates); `safe_mode` mirrors startup (`--safe` skips third-party
/// sources exactly like the runtime). Always succeeds (exit 0, even when
/// empty): missing or invalid manifests and records are skipped, never
/// fatal.
pub fn run_list_services(request: &ListRequest, safe_mode: bool) -> i32 {
    let services = list_services(safe_mode);
    match request.format {
        ListFormat::Table => {
            print!("{}", format_services_table(&services, request.no_color));
        }
        ListFormat::Json | ListFormat::Jsonl => {
            println!("{}", format_services_envelope(&request.spelling, &services));
        }
    }
    EXIT_OK
}

// ---------------------------------------------------------------------------
// `bitty list` CLI entry point (relocated from `main.rs`, CTX-0305)
// ---------------------------------------------------------------------------

use crate::cli::Args;

/// Runs `bitty list <kind>`; returns the process exit code.
///
/// - Extra positionals, unknown kinds, bad `--format`/`--socket`/`--instance`,
///   and stray `--` fail closed (exit 2, stderr only, no stdout envelope).
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "list"|"ls"`) on stdout with diagnostics on stderr.
/// - `instances` runtime/permission failures emit ok:false envelopes for
///   json/jsonl (exit 6/7) and stderr-only for table.
/// - `actions`/`keymaps` resolve the effective config best-effort (config
///   overrides + defaults, shipped fallback) so user chords show without ever
///   failing the catalog on a broken file.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if !args.list_args.is_empty() {
        eprintln!(
            "bitty {}: unexpected argument '{}'\n{}",
            args.list_spelling,
            args.list_args[0],
            list_usage()
        );
        return EXIT_USAGE;
    }
    let request = match ListRequest::validate(
        args.list_kind.as_deref(),
        args.list_format.as_deref(),
        args.list_socket.as_deref(),
        args.list_instance.as_deref(),
        args.list_no_color,
        &args.list_spelling,
    ) {
        Ok(req) => req,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };
    // Only the catalog kinds need the effective config; the others stay
    // config-free (safe-mode clean, no file I/O). Services threads the
    // startup safe flag through so `--safe` skips third-party sources.
    match request.kind {
        ListKind::Themes => {
            // The theme catalog needs no effective config, but `--safe` must
            // still keep the listing built-in-only and read no local files.
            run_list_with_effective(
                &request,
                &bitty_config::EffectiveConfig::default(),
                args.safe,
            )
        }
        ListKind::Actions | ListKind::Keymaps => {
            let effective = effective_for_catalog(args);
            run_list_with_effective(&request, &effective, args.safe)
        }
        ListKind::Services => run_list_services(&request, args.safe),
        _ => run_list(&request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parses_plural_and_singular_case_insensitive() {
        assert_eq!(ListKind::parse("themes"), Some(ListKind::Themes));
        assert_eq!(ListKind::parse("Theme"), Some(ListKind::Themes));
        assert_eq!(ListKind::parse(" THEMES "), Some(ListKind::Themes));
        assert_eq!(ListKind::parse("plugins"), Some(ListKind::Plugins));
        assert_eq!(ListKind::parse("plugin"), Some(ListKind::Plugins));
        assert_eq!(ListKind::parse("instances"), Some(ListKind::Instances));
        assert_eq!(ListKind::parse("Instance"), Some(ListKind::Instances));
        assert_eq!(ListKind::parse("actions"), Some(ListKind::Actions));
        assert_eq!(ListKind::parse("Action"), Some(ListKind::Actions));
        assert_eq!(ListKind::parse("keymaps"), Some(ListKind::Keymaps));
        assert_eq!(ListKind::parse("Keymap"), Some(ListKind::Keymaps));
        assert_eq!(ListKind::parse("services"), Some(ListKind::Services));
        assert_eq!(ListKind::parse("Service"), Some(ListKind::Services));
        assert_eq!(ListKind::Services.name(), "services");
        assert_eq!(ListKind::parse("fonts"), None);
        assert_eq!(ListKind::parse(""), None);
        assert_eq!(ListKind::parse("themes;rm"), None);
    }

    #[test]
    fn format_parses_with_table_default() {
        assert_eq!(ListFormat::parse(None).unwrap(), ListFormat::Table);
        assert_eq!(ListFormat::parse(Some("json")).unwrap(), ListFormat::Json);
        assert_eq!(ListFormat::parse(Some("JSONL")).unwrap(), ListFormat::Jsonl);
        assert!(ListFormat::parse(Some("yaml")).is_err());
    }

    #[test]
    fn themes_lists_full_catalog_default_first() {
        let themes = list_themes();
        assert_eq!(themes.len(), 30, "curated catalog size");
        assert_eq!(themes[0].name, "bitty-dark");
        assert_eq!(themes[0].category, "dark");
        assert!(themes[0].aliases.contains(&"dark".to_string()));
        assert_eq!(themes[0].source, "builtin");
        assert_eq!(
            themes[0].provenance,
            "https://github.com/bitty-terminal/bitty"
        );
        assert_eq!(themes[0].background, "#1e1e2e");
        assert_eq!(themes[0].foreground, "#cdd6f4");
        // Both categories are represented.
        assert!(themes.iter().any(|t| t.category == "dark"));
        assert!(themes.iter().any(|t| t.category == "light"));
        // Aliases and names are unique across the whole table.
        let mut keys = std::collections::HashSet::new();
        for t in &themes {
            assert!(keys.insert(t.name.clone()), "duplicate name {}", t.name);
            for a in &t.aliases {
                assert!(keys.insert(a.clone()), "duplicate alias {a}");
            }
            assert_eq!(t.source, "builtin");
            assert!(t.provenance.starts_with("https://"));
            assert!(!t.license.is_empty());
        }
    }

    #[test]
    fn plugins_lists_bundled_sorted_and_disabled() {
        let plugins = list_plugins();
        // #1572 / CTX-0994: shell-integration only; workspace manifest retired.
        assert_eq!(plugins.len(), 1);
        let ids: Vec<&str> = plugins.iter().map(|p| p.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        for p in &plugins {
            assert!(p.bundled);
            assert!(!p.enabled);
        }
        assert!(ids.contains(&"bitty-terminal.shell-integration"));
        assert!(!ids.contains(&"bitty-terminal.workspace"));
        // Removed plugins are not bundled (CTX-0886: Unix philosophy, Core mechanism only)
        assert!(!ids.contains(&"bitty-terminal.ai-panel"));
        assert!(!ids.contains(&"bitty-terminal.mail-panel"));
        assert!(!ids.contains(&"bitty-terminal.browser-panel"));
        assert!(!ids.contains(&"bitty-terminal.project"));
        // Migrated to independent packages
        assert!(!ids.contains(&"bitty-terminal.palette"));
        assert!(!ids.contains(&"bitty-terminal.statusline"));
        assert!(!ids.contains(&"bitty-terminal.git-panel"));
        assert!(!ids.contains(&"bitty-terminal.file-manager"));
        // CTX-0974 (DEC-0100 waiver): tabs alias purged, not listed nor bundled.
        assert!(!ids.contains(&"bitty-terminal.tabs"));
        assert!(!bitty_plugin_host::bundled::is_bundled(
            &bitty_plugin_host::PluginId::new("bitty-terminal.tabs").unwrap()
        ));
    }

    #[test]
    fn plugins_empty_state_renders_empty_table_and_envelope() {
        let empty = list_plugins_from_manifests(&[]);
        assert!(empty.is_empty());
        let table = format_plugins_table(&empty, true);
        assert!(table.contains("(no plugins)"));
        let env = format_success_envelope("list", ListKind::Plugins, &[], &empty, &[]);
        assert!(env.contains("\"v\":1"));
        assert!(env.contains("\"command\":\"list\""));
        assert!(env.contains("\"ok\":true"));
        assert!(env.contains("\"count\":0"));
    }

    #[test]
    fn themes_envelope_is_machine_readable() {
        let themes = list_themes();
        let env = format_success_envelope("list", ListKind::Themes, &themes, &[], &[]);
        assert!(env.contains("\"v\":1"));
        assert!(env.contains("\"kind\":\"themes\""));
        assert!(env.contains("bitty-dark"));
        assert!(env.contains("#1e1e2e"));
        // Alias spelling propagates to the envelope command field.
        let alias = format_success_envelope("ls", ListKind::Themes, &themes, &[], &[]);
        assert!(alias.contains("\"command\":\"ls\""));
    }

    #[test]
    fn parse_hex_rgb_accepts_rrggbb_and_rejects_other_shapes() {
        assert_eq!(parse_hex_rgb("#1e1e2e"), Some([0x1e, 0x1e, 0x2e]));
        assert_eq!(parse_hex_rgb("#CDD6F4"), Some([0xcd, 0xd6, 0xf4]));
        assert_eq!(parse_hex_rgb("#000000"), Some([0, 0, 0]));
        assert_eq!(parse_hex_rgb("#ffffff"), Some([0xff, 0xff, 0xff]));
        assert_eq!(parse_hex_rgb("1e1e2e"), None);
        assert_eq!(parse_hex_rgb("#1e1e2"), None);
        assert_eq!(parse_hex_rgb("#1e1e2e00"), None);
        assert_eq!(parse_hex_rgb("#zzzzzz"), None);
        assert_eq!(parse_hex_rgb(""), None);
    }

    #[test]
    fn swatch_cells_emit_truecolor_escapes_with_hex_intact() {
        let bg = swatch_bg("#1e1e2e");
        assert!(
            bg.contains("\u{1b}[48;2;30;30;46m"),
            "bg swatch must set truecolor background, got {bg:?}"
        );
        assert!(bg.contains("\u{1b}[0m"), "bg swatch must reset, got {bg:?}");
        assert!(bg.contains("#1e1e2e"), "bg cell must keep hex, got {bg:?}");

        let fg = swatch_fg("#cdd6f4");
        assert!(
            fg.contains("\u{1b}[38;2;205;214;244m"),
            "fg swatch must set truecolor foreground, got {fg:?}"
        );
        assert!(fg.contains("\u{1b}[0m"), "fg swatch must reset, got {fg:?}");
        assert!(fg.contains("#cdd6f4"), "fg cell must keep hex, got {fg:?}");

        // Unparseable input never emits a wrong color.
        assert_eq!(swatch_bg("not-a-color"), "not-a-color");
        assert_eq!(swatch_fg("not-a-color"), "not-a-color");
    }

    #[test]
    fn themes_table_color_on_carries_swatches_with_hex_intact() {
        let themes = list_themes();
        let table = format_themes_table_with_color(&themes, true);
        assert!(
            table.contains("\u{1b}[48;2;"),
            "color-on table must carry bg swatches, got {:?}",
            &table[..table.len().min(512)]
        );
        assert!(
            table.contains("\u{1b}[38;2;"),
            "color-on table must carry fg sample text, got {:?}",
            &table[..table.len().min(512)]
        );
        // Hex stays greppable so existing assertions keep passing.
        assert!(table.contains("bitty-dark"));
        assert!(table.contains("#1e1e2e"));
        assert!(table.contains("#cdd6f4"));
    }

    #[test]
    fn themes_table_color_off_is_plain_hex_without_ansi() {
        let themes = list_themes();
        let plain = format_themes_table_with_color(&themes, false);
        assert!(
            !plain.contains("\u{1b}"),
            "color-off table must carry no ANSI, got {:?}",
            &plain[..plain.len().min(512)]
        );
        assert!(plain.contains("bitty-dark"));
        assert!(plain.contains("#1e1e2e"));
        assert!(plain.contains("#cdd6f4"));

        // `no_color=true` forces the same plain shape regardless of env.
        let via_flag = format_themes_table(&themes, true);
        assert_eq!(
            via_flag, plain,
            "no_color=true must equal the explicit color-off rendering"
        );
    }

    #[test]
    fn unified_gate_piped_stdout_carries_no_ansi() {
        // CTX-1065: headless (piped) output must carry no ANSI even with no
        // opt-outs. The harness stdout is piped, so the live gate resolves
        // to plain here.
        use std::io::IsTerminal as _;
        assert!(
            !std::io::stdout().is_terminal(),
            "test harness stdout must be piped for this assertion"
        );
        assert!(!color_enabled(false));
        let themes = list_themes();
        let table = format_themes_table(&themes, false);
        assert!(
            !table.contains("\u{1b}"),
            "piped themes table must carry no ANSI, got {:?}",
            &table[..table.len().min(512)]
        );
        let plugins = list_plugins();
        let plugins_table = format_plugins_table(&plugins, false);
        assert!(
            !plugins_table.contains("\u{1b}"),
            "piped plugins table must carry no ANSI"
        );
    }

    #[test]
    fn unified_gate_pure_decision_covers_dumb_and_tty() {
        // CTX-1065: TERM=dumb disables even on a tty; a tty with no
        // opt-outs allows color. Forced explicit color still renders via
        // the with_color hook (covered by themes_table_color_on).
        assert!(!crate::color::cli_color_enabled_impl(
            false, false, true, true
        ));
        assert!(crate::color::cli_color_enabled_impl(
            false, false, false, true
        ));
        assert!(!crate::color::cli_color_enabled_impl(
            false, false, false, false
        ));
        // Forced color hook still emits ANSI even when piped.
        let themes = list_themes();
        let forced = format_themes_table_with_color(&themes, true);
        assert!(
            forced.contains("\u{1b}[48;2;"),
            "forced color must still carry swatches"
        );
    }

    #[test]
    fn error_envelope_shape_for_instances() {
        let err = InstanceError::unavailable("socket base missing".to_string());
        let env = format_error_envelope("list", ListKind::Instances, &err);
        assert!(env.contains("\"ok\":false"));
        assert!(env.contains("\"class\":\"Unavailable\""));
        assert!(env.contains("\"count\":0"));
    }

    #[test]
    fn json_escapes_control_bytes() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\ny"), "x\\ny");
        assert_eq!(json_escape("\u{0}"), "\\u0000");
    }

    #[test]
    fn request_rejects_socket_for_non_instances() {
        let err = ListRequest::validate(
            Some("themes"),
            Some("table"),
            Some("/tmp/x.sock"),
            None,
            false,
            "list",
        )
        .unwrap_err();
        assert!(err.contains("--socket/--instance apply only"));
    }

    #[test]
    fn request_rejects_mutually_exclusive_targets() {
        let err = ListRequest::validate(
            Some("instances"),
            Some("json"),
            Some("/tmp/a.sock"),
            Some("default"),
            false,
            "list",
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn candidate_dirs_dedups_and_ignores_empty() {
        let dirs = candidate_socket_dirs(Some("/tmp/a/b.sock"), Some("/tmp/run"));
        assert_eq!(dirs.len(), 2);
        let empty = candidate_socket_dirs(None, None);
        assert!(empty.is_empty());
    }

    // ── shared registry core + Windows pipe merge (CTX-0196) ─────────────
    //
    // These run on every host: the registry scan core takes an injected
    // probe and the pipe-namespace mapping is a pure transform, so Linux
    // covers the shared logic while the Windows CI leg arbitrates the real
    // pipe open / `\\.\pipe\` listing shims.

    static REGISTRY_TEST_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    fn registry_test_dir() -> PathBuf {
        let uniq = REGISTRY_TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("blt-reg-{}-{uniq}", std::process::id()))
    }

    #[test]
    fn registry_scan_missing_dir_is_empty_success() {
        let dir = registry_test_dir().join("bitty");
        let _ = std::fs::remove_dir_all(registry_test_dir());
        let rows =
            scan_registry_files_with_probe(&dir, &|_| panic!("probe must not run with no entries"))
                .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn registry_scan_non_directory_is_unavailable() {
        let base = registry_test_dir();
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let err = scan_registry_files_with_probe(&file, &|_| (false, String::new())).unwrap_err();
        assert_eq!(err.code, EXIT_RUNTIME);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_scan_lists_entries_via_injected_probe() {
        let base = registry_test_dir();
        let dir = base.join("bitty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale.sock"), b"not a socket").unwrap();
        std::fs::write(dir.join("live.sock"), b"not a socket").unwrap();
        std::fs::write(dir.join("ignore.txt"), b"not a socket entry").unwrap();
        let rows = scan_registry_files_with_probe(&dir, &|path| {
            let live = path.to_string_lossy().contains("live");
            (live, if live { "live" } else { "stale" }.to_string())
        })
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].instance, "live");
        assert!(rows[0].live);
        assert_eq!(rows[1].instance, "stale");
        assert!(!rows[1].live);
        assert!(rows[0].socket < rows[1].socket);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_scan_caps_entries_with_shed_note() {
        let base = registry_test_dir();
        let dir = base.join("bitty");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(MAX_LIST_INSTANCES + 3) {
            std::fs::write(dir.join(format!("inst-{i:03}.sock")), b"x").unwrap();
        }
        let rows = scan_registry_files_with_probe(&dir, &|_| (false, "stale".to_string())).unwrap();
        assert_eq!(rows.len(), MAX_LIST_INSTANCES);
        let last = rows.last().unwrap();
        assert!(
            last.detail.contains("shed 3 of"),
            "detail: {:?}",
            last.detail
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pipe_namespace_candidates_map_live_and_skip_foreign() {
        let names = [
            "bitty-default".to_string(),
            "bitty-live_2".to_string(),
            "other-pipe".to_string(),
            "bitty-".to_string(),
            "bitty-has space".to_string(),
            "bitty-default.sock".to_string(),
            "BITTY-default".to_string(),
        ];
        let rows = cap_instance_rows(pipe_namespace_candidates(&names));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].instance, "default");
        assert_eq!(rows[1].instance, "live_2");
        for row in &rows {
            assert!(row.live);
            assert_eq!(row.detail, "live");
            assert!(row.socket.contains(r"\\.\pipe\bitty-"));
        }
        assert!(rows[0].socket < rows[1].socket);
    }

    #[test]
    fn pipe_namespace_candidates_cap_with_shed_note() {
        let names: Vec<String> = (0..(MAX_LIST_INSTANCES + 5))
            .map(|i| format!("bitty-inst-{i:03}"))
            .collect();
        let rows = cap_instance_rows(pipe_namespace_candidates(&names));
        assert_eq!(rows.len(), MAX_LIST_INSTANCES);
        assert!(
            rows.last().unwrap().detail.contains("shed 5 of"),
            "detail: {:?}",
            rows.last().unwrap().detail
        );
    }

    #[test]
    fn merge_instance_rows_dedups_sorts_and_caps() {
        let row = |instance: &str, socket: &str| InstanceInfo {
            instance: instance.to_string(),
            socket: socket.to_string(),
            live: true,
            detail: "live".to_string(),
        };
        let primary = vec![row("b", "b"), row("a", "a")];
        let secondary = vec![row("a-dup", "a"), row("c", "c")];
        let merged = merge_instance_rows(primary, secondary);
        let sockets: Vec<&str> = merged.iter().map(|r| r.socket.as_str()).collect();
        assert_eq!(sockets, vec!["a", "b", "c"]);
        // Identical pipe rows dedup; the surviving row keeps primary metadata.
        assert_eq!(merged[0].instance, "a");

        let big: Vec<InstanceInfo> = (0..(MAX_LIST_INSTANCES + 1))
            .map(|i| row(&format!("i{i:03}"), &format!("s{i:03}")))
            .collect();
        let merged = merge_instance_rows(big, vec![row("z", "z")]);
        assert_eq!(merged.len(), MAX_LIST_INSTANCES);
    }

    #[test]
    fn list_help_documents_windows_pipe_serving() {
        // CTX-1405 (#2244): `bitty list --help` must describe the real
        // Windows pipe transport and its identity-verified probe.
        let help = list_help_text("list");
        assert!(
            help.contains("Windows serves the same instance-derived named pipe"),
            "list help must name the Windows pipe transport, got {help:?}"
        );
        assert!(
            !help.contains("Windows serving is unavailable"),
            "list help must not keep the retired degradation, got {help:?}"
        );
    }

    #[test]
    fn degraded_merge_with_empty_pipe_namespace_preserves_registry() {
        // Issue #1800 degraded shape (Windows without serving, or any
        // platform without pipes): merging registry rows with an empty
        // pipe-namespace set is a sorted no-op, never a failure. The
        // empty namespace is passed explicitly so the test never depends
        // on the host pipe namespace (CodeRabbit, PR #1831).
        let row = |instance: &str, socket: &str| InstanceInfo {
            instance: instance.to_string(),
            socket: socket.to_string(),
            live: false,
            detail: "stale: pipe not present".to_string(),
        };
        let primary = vec![row("b", "b"), row("a", "a")];
        let merged = merge_instance_rows(primary, Vec::new());
        let sockets: Vec<&str> = merged.iter().map(|r| r.socket.as_str()).collect();
        assert_eq!(sockets, vec!["a", "b"]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_namespace_scan_stays_bounded() {
        // Live host scan, tested separately from the degraded merge
        // above: best-effort enumeration never exceeds the list bound,
        // however many pipes a busy host holds.
        assert!(scan_pipe_namespace().len() <= MAX_LIST_INSTANCES);
    }

    #[cfg(not(windows))]
    #[test]
    fn scan_pipe_namespace_stays_empty_without_pipe_namespace() {
        // Outside Windows there is no pipe namespace: the scan is empty
        // and the registry scan stays authoritative.
        assert!(scan_pipe_namespace().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_missing_dir_is_empty_success() {
        let dir =
            std::env::temp_dir().join(format!("bitty-list-test-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rows = scan_socket_dir(&dir).unwrap();
        assert!(rows.is_empty());
        let table = format_instances_table(&rows, true);
        assert!(table.contains("(no instances"));
    }

    #[cfg(unix)]
    #[test]
    fn scan_lists_stale_and_live_sockets() {
        use std::os::unix::net::UnixListener;
        // Keep socket paths well under SUN_LEN (104 B on macOS): short
        // prefix, pid + atomic counter only, no nanos timestamp.
        static TEST_DIR_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let uniq = TEST_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("blt-{}-{uniq}", std::process::id()));
        let dir = base.join("bitty");
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // Stale regular file with .sock suffix (connect fails -> live false).
        std::fs::write(dir.join("stale.sock"), b"not a socket").unwrap();
        // Live socket (0600 like the servo's attested endpoint, so the
        // CTX-0528/IPC-001 pre-connect verification passes).
        let live_path = dir.join("live.sock");
        let listener = UnixListener::bind(&live_path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&live_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        // Keep listener alive during scan.
        let rows = scan_socket_dir(&dir).unwrap();
        assert_eq!(rows.len(), 2);
        let live = rows.iter().find(|r| r.instance == "live").unwrap();
        assert!(live.live);
        let stale = rows.iter().find(|r| r.instance == "stale").unwrap();
        assert!(!stale.live);
        drop(listener);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// CTX-0528 (IPC-001): a foreign-owned/tampered endpoint never reports
    /// live — pre-connect verification fails closed before `connect`, so a
    /// planted entry cannot be mistaken for a live peer.
    #[cfg(unix)]
    #[test]
    fn ipc001_tampered_endpoint_never_reports_live() {
        use std::os::unix::fs::PermissionsExt;
        static TAMPER_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let uniq = TAMPER_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("blt-tamper-{}-{uniq}", std::process::id()));
        let dir = base.join("bitty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A world-writable leaf fails the endpoint verification gate even
        // though a socket inside could accept connections.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let rows = scan_socket_dir(&dir);
        assert!(
            rows.is_err(),
            "world-writable scan dir must fail closed, got: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── CTX-1127 actions/keymaps catalog (issue #1934) ─────────────────────

    #[test]
    fn actions_catalog_matches_registry_counts() {
        let chrome = list_chrome_actions();
        let core = list_core_commands();
        assert_eq!(
            chrome.len(),
            bitty_config::keymap::all_chrome_actions().len(),
            "chrome rows must match variant catalog"
        );
        assert_eq!(
            core.len(),
            crate::inspect::CORE_COMMANDS.len(),
            "core rows must match registry"
        );
        // Ids are unique across the combined catalog.
        let mut ids = std::collections::HashSet::new();
        for a in &chrome {
            assert!(!a.description.is_empty(), "description empty for {}", a.id);
            assert!(ids.insert(a.id.clone()), "duplicate action id {}", a.id);
        }
        for c in &core {
            assert!(!c.description.is_empty(), "description empty for {}", c.id);
            assert!(ids.insert(c.id.clone()), "duplicate command id {}", c.id);
        }
        // Chrome rows all mark runtime (local verb, runtime-dependent rows).
        for a in &chrome {
            assert!(a.requires_runtime, "chrome {} must mark runtime", a.id);
        }
        // Core runtime marking follows the registry class.
        for c in &core {
            assert_eq!(
                c.requires_runtime,
                c.class == "runtime",
                "core {} class/runtime mismatch",
                c.id
            );
        }
    }

    #[test]
    fn actions_descriptions_reuse_canonical_source() {
        // Chrome descriptions are exactly the canonical method strings (no
        // parallel copy): compare row by row against `all_chrome_actions`.
        let catalog = bitty_config::keymap::all_chrome_actions();
        let rows = list_chrome_actions();
        for action in &catalog {
            let id = action.canonical();
            let row = rows.iter().find(|r| r.id == id).unwrap_or_else(|| {
                panic!("catalog id {id:?} missing from list rows");
            });
            assert_eq!(row.description, action.description(), "drift for {id}");
            assert_eq!(
                row.manual_bind_only,
                action.is_manual_bind_only(),
                "manual flag drift for {id}"
            );
        }
        // Core descriptions are exactly the registry summaries.
        let core_rows = list_core_commands();
        for reg in crate::inspect::CORE_COMMANDS {
            let row = core_rows
                .iter()
                .find(|r| r.id == reg.id)
                .unwrap_or_else(|| panic!("registry id {:?} missing", reg.id));
            assert_eq!(row.description, reg.summary, "drift for {}", reg.id);
        }
    }

    #[test]
    fn actions_envelope_shape_is_stable() {
        let chrome = list_chrome_actions();
        let core = list_core_commands();
        let env = format_actions_envelope("list", &chrome, &core);
        assert!(env.contains("\"v\":1"), "envelope: {env:?}");
        assert!(env.contains("\"command\":\"list\""), "envelope: {env:?}");
        assert!(env.contains("\"ok\":true"), "envelope: {env:?}");
        assert!(env.contains("\"kind\":\"actions\""), "envelope: {env:?}");
        assert!(
            env.contains(&format!("\"count\":{}", chrome.len() + core.len())),
            "count: {env:?}"
        );
        assert!(env.contains("\"actions\":["), "envelope: {env:?}");
        assert!(env.contains("\"commands\":["), "envelope: {env:?}");
        assert!(env.contains("\"manual_bind_only\":"), "envelope: {env:?}");
        assert!(env.contains("\"requires_runtime\":"), "envelope: {env:?}");
        assert!(env.contains("\"binding\":"), "envelope: {env:?}");
        assert!(env.contains("close_view"), "envelope: {env:?}");
        assert!(env.contains("core.terminal.text"), "envelope: {env:?}");
        // Alias spelling propagates to the envelope command field.
        let alias = format_actions_envelope("ls", &chrome, &core);
        assert!(alias.contains("\"command\":\"ls\""));
    }

    #[test]
    fn actions_table_smoke() {
        let chrome = list_chrome_actions();
        let core = list_core_commands();
        let table = format_actions_table(&chrome, &core, true);
        assert!(
            table.contains("KIND ID BINDING DESCRIPTION"),
            "table: {table:?}"
        );
        assert!(table.contains("close_view"), "table: {table:?}");
        assert!(table.contains("core.terminal.text"), "table: {table:?}");
        assert!(table.contains("chrome"), "table: {table:?}");
        assert!(table.contains("command"), "table: {table:?}");
    }

    #[test]
    fn actions_binding_default_fallback_for_shipped() {
        // Shipped defaults: bound actions report a default chord, manual-only
        // report null. Same resolver the keymaps table reads.
        let resolved = bitty_config::keymap::shipped_resolved_keymaps();
        let rows = list_chrome_actions_with(&resolved);
        let close = rows
            .iter()
            .find(|r| r.id == "close_view")
            .expect("close_view row");
        assert!(!close.manual_bind_only);
        assert!(
            close.binding.is_some(),
            "shipped close_view must carry a default binding"
        );
        // close_view ships alt+d and alt+q; smallest wins deterministically.
        assert_eq!(close.binding.as_deref(), Some("alt+d"));
        let goto = rows
            .iter()
            .find(|r| r.id == "goto_split:left")
            .expect("goto row");
        assert_eq!(goto.binding.as_deref(), Some("alt+h"));
    }

    #[test]
    fn actions_binding_manual_only_is_null() {
        let resolved = bitty_config::keymap::shipped_resolved_keymaps();
        let rows = list_chrome_actions_with(&resolved);
        for id in [
            "open_composer",
            "fold_toggle",
            "fold_expand",
            "fold_collapse",
            "toggle_palette",
            "toggle_pinned",
            "focus:1",
        ] {
            let row = rows
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("manual row {id:?} missing"));
            assert!(row.manual_bind_only, "{id} must flag manual");
            assert_eq!(row.binding, None, "{id} must report null binding");
        }
        // Core commands never have chords: always null binding (stable key).
        for c in list_core_commands() {
            assert_eq!(c.binding, None, "core {} binding must be null", c.id);
        }
        // Envelope carries explicit nulls, never absent keys.
        let core = list_core_commands();
        let env = format_actions_envelope("list", &rows, &core);
        assert!(env.contains("\"binding\":null"), "envelope: {env:?}");
    }

    #[test]
    fn actions_binding_override_wins_over_default() {
        // Synthetic effective config with one user override: same resolver
        // (`resolve_keymaps`) the live dispatch uses, no fork.
        let mut effective = bitty_config::EffectiveConfig::default();
        effective.keymaps.push(bitty_config::KeymapEntry {
            context: "global".to_string(),
            chord: "alt+x".to_string(),
            action: "close_view".to_string(),
        });
        let resolved =
            bitty_config::keymap::resolve_keymaps(&effective).expect("override must resolve");
        // The keymaps view shares the same table (user source wins).
        let key_rows = list_keymaps_with(&resolved);
        let user_row = key_rows
            .iter()
            .find(|r| r.chord == "alt+x")
            .expect("user chord must appear");
        assert_eq!(user_row.action, "close_view");
        assert_eq!(user_row.source, "user");
        // The actions view prefers the user chord over the default.
        let action_rows = list_chrome_actions_with(&resolved);
        let close = action_rows
            .iter()
            .find(|r| r.id == "close_view")
            .expect("close_view row");
        assert_eq!(close.binding.as_deref(), Some("alt+x"));
        // Unrelated actions still fall back to defaults.
        let goto = action_rows
            .iter()
            .find(|r| r.id == "goto_split:left")
            .expect("goto row");
        assert_eq!(goto.binding.as_deref(), Some("alt+h"));
        // Meta flips to user when overrides exist.
        let meta = keymap_meta_for(&effective);
        assert_eq!(meta.source, "user");
    }

    #[test]
    fn keymaps_resolution_matches_inspect_on_sample_chords() {
        let rows = list_keymaps();
        let meta = keymap_meta();
        assert_eq!(meta.mod_key, "alt");
        assert_eq!(meta.source, "default");
        assert_eq!(
            rows.len(),
            bitty_config::keymap::DEFAULT_KEYMAPS.len(),
            "shipped table must cover every default"
        );
        // Bound chords resolve identically in both surfaces (shared path).
        for sample in ["alt+h", "alt+n", "ctrl+shift+c", "alt+tab"] {
            let info = crate::inspect::inspect_key(sample);
            assert!(info.bound, "sample {sample:?} must stay bound");
            let action = info.action.clone().expect("bound action");
            let row = rows
                .iter()
                .find(|r| r.chord == info.canonical)
                .unwrap_or_else(|| {
                    panic!(
                        "chord {sample:?} canonical {:?} missing from list",
                        info.canonical
                    );
                });
            assert_eq!(row.action, action, "mismatch for {sample:?}");
            assert_eq!(row.context, info.context.clone().unwrap_or_default());
            // Description reuses the same canonical source.
            let parsed =
                bitty_config::keymap::ChromeAction::parse(&action).expect("parse bound action");
            assert_eq!(row.description, parsed.description());
            assert_eq!(
                info.description.as_deref(),
                Some(parsed.description()),
                "inspect description drift for {sample:?}"
            );
        }
        // Unbound chords succeed in inspect with bound:false and never appear
        // in the shipped table (single-owner rule: they reach the shell).
        let free = crate::inspect::inspect_key("ctrl+shift+f9");
        assert!(!free.bound);
        assert!(rows.iter().all(|r| r.chord != free.canonical));
    }

    #[test]
    fn keymaps_envelope_and_table_shapes() {
        let meta = keymap_meta();
        let rows = list_keymaps();
        let env = format_keymaps_envelope("list", &meta, &rows);
        assert!(env.contains("\"v\":1"), "envelope: {env:?}");
        assert!(env.contains("\"kind\":\"keymaps\""), "envelope: {env:?}");
        assert!(env.contains("\"mod_key\":\"alt\""), "envelope: {env:?}");
        assert!(env.contains("\"source\":\"default\""), "envelope: {env:?}");
        assert!(
            env.contains(&format!("\"count\":{}", rows.len())),
            "count: {env:?}"
        );
        assert!(env.contains("\"chord\":\"alt+h\""), "envelope: {env:?}");
        assert!(env.contains("goto_split:left"), "envelope: {env:?}");
        let table = format_keymaps_table(&meta, &rows, true);
        assert!(table.contains("mod_key: alt"), "table: {table:?}");
        assert!(
            table.contains("CHORD ACTION CONTEXT SOURCE"),
            "table: {table:?}"
        );
        assert!(table.contains("alt+h"), "table: {table:?}");
    }

    #[test]
    fn list_help_documents_actions_keymaps_contract() {
        let help = list_help_text("list");
        assert!(
            help.contains("actions"),
            "help must name actions, got {help:?}"
        );
        assert!(
            help.contains("keymaps"),
            "help must name keymaps, got {help:?}"
        );
        assert!(
            help.contains("manual_bind_only"),
            "help must document actions JSON keys, got {help:?}"
        );
        assert!(
            help.contains("\"binding\"") || help.contains("binding"),
            "help must document the new binding field, got {help:?}"
        );
        assert!(
            help.contains("mod_key"),
            "help must document keymaps meta, got {help:?}"
        );
        assert!(
            help.contains("bitty list actions"),
            "help must carry examples, got {help:?}"
        );
    }

    fn service_input<'a>(
        provider: &'a str,
        owner: &'a str,
        iface: &'a str,
        version: &'a str,
        heads: &'a [String],
        grants: &'a BTreeSet<String>,
    ) -> ServiceListingInput<'a> {
        ServiceListingInput {
            provider,
            owner,
            iface,
            version,
            heads,
            grants,
            grant_source: SERVICE_GRANT_MANIFEST,
        }
    }

    fn grant_set(heads: &[&str]) -> BTreeSet<String> {
        heads.iter().map(|h| (*h).to_string()).collect()
    }

    #[test]
    fn services_builder_lists_one_row_per_head() {
        // Dynapi observability (bitty#2002): one row per (provision, head)
        // with frozen scalar fields, sorted for stable output.
        let grants = grant_set(&["runtime.inspect", "workspace.read"]);
        let heads = vec!["workspace.read".to_string(), "runtime.inspect".to_string()];
        let inputs = vec![service_input(
            "xuepoo.calc",
            "xuepoo",
            "calc.add",
            "1.0.0",
            &heads,
            &grants,
        )];
        let rows = list_services_from_inputs(&inputs);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].backing_head, "runtime.inspect");
        assert_eq!(rows[1].backing_head, "workspace.read");
        for row in &rows {
            assert_eq!(row.interface, "calc.add");
            assert_eq!(row.owner, "xuepoo");
            assert_eq!(row.version, "1.0.0");
            assert_eq!(row.provider, "xuepoo.calc");
            assert_eq!(row.grant_source, SERVICE_GRANT_MANIFEST);
            assert_eq!(row.status, SERVICE_STATUS_ACTIVE);
        }
    }

    #[test]
    fn services_builder_is_generic_over_providers() {
        // No Core change per extension: the same builder serves any
        // provider count with no per-extension branches.
        let grants = grant_set(&["runtime.inspect"]);
        for count in [1, 2, 5] {
            let providers: Vec<String> = (0..count).map(|i| format!("xuepoo.ext{i}")).collect();
            let heads = vec!["runtime.inspect".to_string()];
            let inputs: Vec<ServiceListingInput<'_>> = providers
                .iter()
                .map(|p| service_input(p, "xuepoo", "duet.play", "1.0.0", &heads, &grants))
                .collect();
            let rows = list_services_from_inputs(&inputs);
            assert_eq!(rows.len(), count, "one row per provider");
            let listed: Vec<&str> = rows.iter().map(|r| r.provider.as_str()).collect();
            let mut expected: Vec<&str> = providers.iter().map(|p| p.as_str()).collect();
            expected.sort();
            assert_eq!(listed, expected, "sorted by provider");
        }
    }

    #[test]
    fn services_builder_drops_unbacked_and_ungranted() {
        // Unbacked provisions never record (fail-closed); ungranted
        // provisions are absent — indistinguishable from undeclared, the
        // same posture as `services.get`.
        let grants = grant_set(&["runtime.inspect"]);
        let empty: Vec<String> = Vec::new();
        let spoofed = vec!["workspace.read".to_string()];
        let backed = vec!["runtime.inspect".to_string()];
        let inputs = vec![
            service_input(
                "xuepoo.solo",
                "xuepoo",
                "calc.solo",
                "1.0.0",
                &empty,
                &grants,
            ),
            service_input(
                "xuepoo.spoof",
                "xuepoo",
                "calc.spoof",
                "1.0.0",
                &spoofed,
                &grants,
            ),
            service_input(
                "xuepoo.calc",
                "xuepoo",
                "calc.add",
                "1.0.0",
                &backed,
                &grants,
            ),
        ];
        let rows = list_services_from_inputs(&inputs);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "xuepoo.calc");
    }

    #[test]
    fn services_envelope_freezes_json_keys_without_nulls() {
        // Frozen contract: exact keys, required strings, no nulls.
        let grants = grant_set(&["runtime.inspect"]);
        let heads = vec!["runtime.inspect".to_string()];
        let inputs = vec![service_input(
            "xuepoo.calc",
            "xuepoo",
            "calc.add",
            "1.0.0",
            &heads,
            &grants,
        )];
        let rows = list_services_from_inputs(&inputs);
        let envelope = format_services_envelope("list", &rows);
        assert!(
            envelope.contains("\"kind\":\"services\""),
            "kind: {envelope:?}"
        );
        assert!(envelope.contains("\"count\":1"), "count: {envelope:?}");
        for key in [
            "\"interface\"",
            "\"owner\"",
            "\"version\"",
            "\"provider\"",
            "\"backing_head\"",
            "\"grant_source\"",
            "\"status\"",
        ] {
            assert!(envelope.contains(key), "frozen key {key}: {envelope:?}");
        }
        assert!(
            !envelope.contains("null"),
            "v1 emits no nulls: {envelope:?}"
        );
        assert!(
            envelope.contains("\"interface\":\"calc.add\""),
            "values: {envelope:?}"
        );
    }

    #[test]
    fn services_table_renders_hostile_strings_safely() {
        // A hostile provider string (control characters, empty cells)
        // renders safely: controls scrubbed, empties show the reserved
        // null marker. Fed directly to prove the formatter is total even
        // if validation ever loosens.
        assert_eq!(SERVICES_NULL, "-");
        assert_eq!(sanitize_service_cell(""), SERVICES_NULL);
        assert_eq!(
            sanitize_service_cell("a\x00b\x1bc\x7f"),
            "a\u{FFFD}b\u{FFFD}c\u{FFFD}"
        );
        let hostile = ServiceInfo {
            interface: "calc\x01.add".to_string(),
            owner: "xuepoo".to_string(),
            version: "1.0.0".to_string(),
            provider: "xuepoo.calc".to_string(),
            backing_head: "runtime.inspect".to_string(),
            grant_source: SERVICE_GRANT_MANIFEST.to_string(),
            status: SERVICE_STATUS_ACTIVE.to_string(),
        };
        let table = format_services_table(std::slice::from_ref(&hostile), true);
        assert!(
            !table.chars().any(|c| c.is_control() && c != '\n'),
            "no raw control chars reach the terminal: {table:?}"
        );
        assert!(table.contains("calc"), "keeps readable text: {table:?}");
        let json = format_services_envelope("list", std::slice::from_ref(&hostile));
        assert!(
            json.contains("calc\\u0001.add"),
            "JSON escapes controls: {json:?}"
        );
    }

    #[test]
    fn services_table_empty_hint_and_header() {
        let table = format_services_table(&[], true);
        assert!(
            table.contains("INTERFACE OWNER VERSION PROVIDER BACKING_HEAD GRANT_SOURCE STATUS"),
            "header names every frozen key: {table:?}"
        );
        assert!(table.contains("(no services)"), "empty hint: {table:?}");
    }

    #[test]
    fn list_help_documents_services_contract() {
        let help = list_help_text("list");
        assert!(help.contains("services"), "help must name services");
        assert!(
            help.contains("backing_head"),
            "help must document services JSON keys, got {help:?}"
        );
        assert!(
            help.contains("bitty list services"),
            "help must carry an example, got {help:?}"
        );
        let usage = list_usage();
        assert!(usage.contains("services"), "usage must name services");
    }
}
