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
//!   no plugin VM loaded). `instances` is runtime discovery (socket-dir scan,
//!   live connect probe) but fetches no terminal content.
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
//! - Instances reuse `bitty-ipc::devtools` socket discovery (`BITTY_SOCKET`,
//!   `XDG_RUNTIME_DIR`, `BITTY_INSTANCE_ID` advisory identifiers, portable
//!   `AF_UNIX` bound, instance grammar). Only discovery metadata is reported
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
//!   capped at 108 bytes (portable `sun_path` ceiling).

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
    "usage: bitty list <themes|plugins|instances> [--format table|json|jsonl] [--socket PATH | --instance ID] [--no-color]\n       bitty ls <themes|plugins|instances> [--format table|json|jsonl] [--no-color]\n\nkinds:\n  themes     built-in theme presets (local, safe-mode clean)\n  plugins    static bundled plugin catalog, no VM (local, safe-mode clean)\n  instances  live socket discovery: instance, socket, live (runtime, no content fetch)"
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
           themes     Built-in theme presets from bitty-config::theme (e.g. bitty-dark, alias dark).\n  \
                      Local class: no instance, no file I/O, safe-mode clean.\n  \
           plugins    Static bundled plugin catalog (bitty-terminal.*), staged disabled by default.\n  \
                      Local class: manifest metadata only, no plugin VM loaded, safe-mode clean.\n  \
           instances  Live Unix-socket discovery (instance, socket, live). Runtime class:\n  \
                      scans the socket directory, probes each socket with a connect,\n  \
                      reports discovery metadata only. Terminal content is never fetched;\n  \
                      detailed snapshots need debug.inspect via ctl/DevTools (bearer scoping\n  \
                      respected by not reading privileged data here).\n\
         \n\
         Options:\n  \
           --format SHAPE  table (default, human, not a contract) | json | jsonl (envelope v1)\n  \
           --socket PATH   Explicit socket for `instances` (bypasses directory scan;\n  \
                           still authenticated by OS file modes/ownership at probe).\n  \
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
    /// Accepted aliases (e.g. `dark`).
    pub aliases: Vec<String>,
    /// Source label (always `built-in` today).
    pub source: String,
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

/// Enumerate built-in theme presets (today exactly one: Bitty Dark).
///
/// Pure, no I/O: the single source of truth stays
/// `bitty-config::theme::BITTY_DARK`. Future presets extend this vector;
/// callers must handle empty (defensive) as an empty table, not an error.
#[must_use]
pub fn list_themes() -> Vec<ThemeInfo> {
    let theme = &bitty_config::theme::BITTY_DARK;
    vec![ThemeInfo {
        name: theme.name.to_string(),
        aliases: vec![bitty_config::theme::DARK_THEME_ALIAS.to_string()],
        source: "built-in".to_string(),
        background: rgb_to_hex(theme.background),
        foreground: rgb_to_hex(theme.foreground),
        cursor: rgb_to_hex(theme.cursor),
        selection: rgb_to_hex(theme.selection),
    }]
}

// ---------------------------------------------------------------------------
// Plugin enumeration (reuses bitty-plugin-host::bundled, no VM)
// ---------------------------------------------------------------------------

/// One plugin row (static manifest metadata only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// Fully qualified id (e.g. `bitty-terminal.tabs`).
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
    use std::os::unix::fs::MetadataExt;

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
    let mode = meta.mode() & 0o777;
    if mode != 0o700 {
        return Err(InstanceError::denied(format!(
            "bitty list: socket directory '{}' mode {mode:o} != 700 (refusing to enumerate; fix with chmod 700)",
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
        let (live, detail) = probe_socket_live(&path);
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

/// Non-Unix stub: instance discovery is Unix-only (same fail-soft shape as
/// the IPC servo, but explicit rather than silent for a query command).
#[cfg(not(unix))]
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
#[cfg(unix)]
fn short_io_kind(kind: std::io::ErrorKind) -> String {
    format!("{kind:?}")
        .chars()
        .take(48)
        .collect::<String>()
        .to_ascii_lowercase()
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
///   (dedup by socket path, sorted). Directory-level auth failures abort
///   fail-closed (first error wins); per-socket probe failures are rows.
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

/// Non-Unix stub for explicit probes.
#[cfg(not(unix))]
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
                    "],\"source\":\"{}\",\"background\":\"{}\",\"foreground\":\"{}\",\"cursor\":\"{}\",\"selection\":\"{}\"}}",
                    json_escape(&t.source),
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
        bold("NAME ALIASES SOURCE BACKGROUND FOREGROUND", color)
    ));
    if themes.is_empty() {
        out.push_str("(no themes)\n");
        return out;
    }
    for t in themes {
        out.push_str(&format!(
            "{} {} {} {} {}\n",
            t.name,
            t.aliases.join(","),
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
            "(no instances — start bitty to serve a socket, or set BITTY_SOCKET/XDG_RUNTIME_DIR)\n",
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
    fn themes_lists_builtin_dark() {
        let themes = list_themes();
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].name, "bitty-dark");
        assert!(themes[0].aliases.contains(&"dark".to_string()));
        assert_eq!(themes[0].source, "built-in");
        assert_eq!(themes[0].background, "#1e1e2e");
        assert_eq!(themes[0].foreground, "#cdd6f4");
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
        assert!(ids.contains(&"bitty-terminal.tabs"));
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
