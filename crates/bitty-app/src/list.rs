//! `bitty list`: enumerate resources (CTX-0172).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`list` introspection
//! section) as refined by `docs/specifications/cli-contract-rfc.md`
//! (`bitty list` mixed class, output envelope v1, exit codes 0-8, `ls` alias).
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty list <kind> [--format table|json|jsonl] [--socket PATH]
//!   [--instance ID] [--no-color]` and alias `bitty ls <kind> ...` (same
//!   executable, envelope `command` names the invoked spelling).
//! - Kinds (task-required): `themes`, `plugins`, `instances` (singular
//!   `theme|plugin|instance` accepted as convenience, same result).
//!   Unknown kinds are `UsageError` (exit 2) naming the valid set. RFC
//!   candidates (`fonts|keymaps|actions|commands|protocols`) are not invented
//!   here: they fail closed as unknown kinds until their owning slice lands.
//! - Class: `themes` and `plugins` are local (no instance, safe-mode clean,
//!   no plugin VM loaded). `instances` is runtime discovery (registry scan,
//!   live endpoint probe) but fetches no terminal content.
//!   Unix scans `$XDG_RUNTIME_DIR/bitty/*.sock` (plus the `BITTY_SOCKET`
//!   parent) and probes each socket with a `connect`; Windows scans the same
//!   registry inputs for `<instance>.sock` markers and probes each
//!   `\\.\pipe\bitty-<instance>` named pipe, merging live pipe-namespace
//!   enumeration (CTX-0196). Only discovery metadata is reported
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

use std::path::{Path, PathBuf};

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

/// Maximum bytes for a kind token (plenty for `themes|plugins|instances`).
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
}

impl ListKind {
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
                "bitty list: missing <kind> (want themes|plugins|instances)",
                list_usage()
            )
        })?;
        let kind = ListKind::parse(kind_token).ok_or_else(|| {
            format!(
                "bitty list: unknown kind {kind_token:?} (want themes|plugins|instances)\n{}",
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
    "usage: bitty list <themes|plugins|instances> [--format table|json|jsonl] [--socket PATH | --instance ID] [--no-color]\n       bitty ls <themes|plugins|instances> [--format table|json|jsonl] [--no-color]\n\nkinds:\n  themes     built-in theme presets (local, safe-mode clean)\n  plugins    static bundled plugin catalog, no VM (local, safe-mode clean)\n  instances  live instance discovery: instance, socket, live (runtime, no content fetch)"
        .to_string()
}

/// Full help for `bitty list --help` (stdout, exit 0).
#[must_use]
pub fn list_help_text(invoked_as: &str) -> String {
    format!(
        "bitty {invoked_as} — enumerate resources (local or runtime)\n\
         \n\
         Usage: bitty {invoked_as} <themes|plugins|instances> [--format table|json|jsonl] [--socket PATH | --instance ID] [--no-color]\n\
         \n\
         Kinds:\n  \
           themes     Built-in theme preset catalog from bitty-config::theme (e.g. bitty-dark,\n  \
                      tokyo-night, github-dark, catppuccin-mocha; aliases like dark).\n  \
                      Local class: no instance, no file I/O, safe-mode clean.\n  \
           plugins    Static bundled plugin catalog (bitty-terminal.*), staged disabled by default.\n  \
                      Local class: manifest metadata only, no plugin VM loaded, safe-mode clean.\n  \
            instances  Live instance discovery (instance, socket, live). Runtime class:\n  \
                       Unix scans the socket directory and probes each socket with a connect;\n  \
                       Windows scans the instance registry and probes each named pipe, plus\n  \
                       live pipe-namespace enumeration; every platform reports discovery\n  \
                       metadata only. Terminal content is never fetched;\n  \
                      detailed snapshots need debug.inspect via ctl/DevTools (bearer scoping\n  \
                      respected by not reading privileged data here).\n\
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
           failures for `instances` emit ok:false envelopes (exit 6/7).\n\
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
    /// Upstream project URL that owns the palette.
    pub source: String,
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
/// (defensive) as an empty table, not an error.
#[must_use]
pub fn list_themes() -> Vec<ThemeInfo> {
    bitty_config::theme::list_presets()
        .iter()
        .map(|theme| ThemeInfo {
            name: theme.name.to_string(),
            category: theme.category.as_str().to_string(),
            aliases: theme.aliases.iter().map(|a| (*a).to_string()).collect(),
            source: theme.source.to_string(),
            license: theme.license.to_string(),
            background: rgb_to_hex(theme.background),
            foreground: rgb_to_hex(theme.foreground),
            cursor: rgb_to_hex(theme.cursor),
            selection: rgb_to_hex(theme.selection),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Plugin enumeration (reuses bitty-plugin-host::bundled, no VM)
// ---------------------------------------------------------------------------

/// One plugin row (static manifest metadata only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// Fully qualified id (e.g. `bitty-terminal.workspace`).
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
                .map(|c| c.as_str().to_string())
                .collect(),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
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

/// Probe one socket path with a connect (bounded, no content exchange).
///
/// Returns `(live, detail)`. `live == true` only when `connect` succeeds;
/// every other outcome is `live == false` with a short, non-sensitive
/// detail (error kind only, never OS handles or peer bytes).
#[cfg(unix)]
fn probe_socket_live(path: &Path) -> (bool, String) {
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

/// Probe one Windows named pipe with a bidirectional open (CTX-0196).
///
/// Safe-`std` only (no `unsafe` in this crate): a pipe with a listening
/// server opens; a missing pipe fails `NotFound` (stale registry entry); a
/// present-but-inaccessible pipe fails `PermissionDenied`. The open never
/// blocks: with no waiting listener the OS fails fast, mirroring the
/// fail-fast Unix `connect` probe. The serving side must expose duplex pipes
/// named `\\.\pipe\bitty-<instance>` for this probe to report `live`.
#[cfg(windows)]
fn probe_windows_pipe(pipe_path: &str) -> (bool, String) {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path)
    {
        Ok(_) => (true, "live".to_string()),
        Err(err) => {
            let detail = match err.kind() {
                std::io::ErrorKind::NotFound => "stale: pipe not present".to_string(),
                std::io::ErrorKind::PermissionDenied => {
                    "unreachable: permission denied".to_string()
                }
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
pub fn pipe_namespace_rows(pipe_names: &[String]) -> Vec<InstanceInfo> {
    let mut rows: Vec<InstanceInfo> = pipe_names
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
        .collect();
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
    pipe_namespace_rows(&names)
}

/// Non-Windows: no pipe namespace exists; the registry scan is authoritative.
///
/// Routes through the shared [`pipe_namespace_rows`] transform so the
/// [`discover_instances`] merge stays uniform across platforms.
#[cfg(not(windows))]
pub fn scan_pipe_namespace() -> Vec<InstanceInfo> {
    pipe_namespace_rows(&[])
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
        return match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path_str)
        {
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
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pipe)
            {
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
                    "],\"category\":\"{}\",\"source\":\"{}\",\"license\":\"{}\",\"background\":\"{}\",\"foreground\":\"{}\",\"cursor\":\"{}\",\"selection\":\"{}\"}}",
                    json_escape(&t.category),
                    json_escape(&t.source),
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
    }
    out.push_str("}}");
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

/// Whether ANSI color is enabled (explicit flag plus `NO_COLOR` honoring).
#[must_use]
pub fn color_enabled(no_color: bool) -> bool {
    if no_color {
        return false;
    }
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    // `TERM=dumb` disables color; otherwise assume color-capable.
    !matches!(std::env::var("TERM"), Ok(term) if term.trim().eq_ignore_ascii_case("dumb"))
}

fn bold(s: &str, color: bool) -> String {
    if color {
        format!("\u{1b}[1m{s}\u{1b}[0m")
    } else {
        s.to_string()
    }
}

/// Render themes as a human table.
#[must_use]
pub fn format_themes_table(themes: &[ThemeInfo], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    out.push_str(&format!(
        "{}\n",
        bold("NAME CATEGORY ALIASES SOURCE BACKGROUND FOREGROUND", color)
    ));
    if themes.is_empty() {
        out.push_str("(no themes)\n");
        return out;
    }
    for t in themes {
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
            t.background,
            t.foreground
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

// ---------------------------------------------------------------------------
// Dispatch (called by main.rs; returns process exit code)
// ---------------------------------------------------------------------------

/// Run `bitty list` from a validated request; prints to stdout, diagnostics
/// to stderr, and returns the process exit code.
///
/// - Themes/plugins always succeed (exit 0, even when empty in tests).
/// - Instances maps discovery failures to ok:false envelopes for json/jsonl
///   (exit 6/7) and to stderr + exit code for table (no partial table).
pub fn run_list(request: &ListRequest) -> i32 {
    match request.kind {
        ListKind::Themes => {
            let themes = list_themes();
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
    }
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
    run_list(&request)
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
        assert_eq!(themes[0].source, "https://github.com/bitty-terminal/bitty");
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
            assert!(t.source.starts_with("https://"));
            assert!(!t.license.is_empty());
        }
    }

    #[test]
    fn plugins_lists_bundled_sorted_and_disabled() {
        let plugins = list_plugins();
        assert_eq!(plugins.len(), 10);
        let ids: Vec<&str> = plugins.iter().map(|p| p.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        for p in &plugins {
            assert!(p.bundled);
            assert!(!p.enabled);
        }
        assert!(ids.contains(&"bitty-terminal.workspace"));
        // Deprecated alias is not a separate list row (canonical list only),
        // but still resolves as bundled.
        assert!(!ids.contains(&"bitty-terminal.tabs"));
        assert!(bitty_plugin_host::bundled::is_bundled(
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
    fn pipe_namespace_rows_maps_live_and_skips_foreign() {
        let names = [
            "bitty-default".to_string(),
            "bitty-live_2".to_string(),
            "other-pipe".to_string(),
            "bitty-".to_string(),
            "bitty-has space".to_string(),
            "bitty-default.sock".to_string(),
            "BITTY-default".to_string(),
        ];
        let rows = pipe_namespace_rows(&names);
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
    fn pipe_namespace_rows_caps_with_shed_note() {
        let names: Vec<String> = (0..(MAX_LIST_INSTANCES + 5))
            .map(|i| format!("bitty-inst-{i:03}"))
            .collect();
        let rows = pipe_namespace_rows(&names);
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
        // Live socket.
        let live_path = dir.join("live.sock");
        let listener = UnixListener::bind(&live_path).unwrap();
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
}
