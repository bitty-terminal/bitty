//! Legacy per-plugin TOML user files (CTX-1195, issue #2010).
//!
//! This module owns the bounded flat-key TOML subset and its path helpers.
//! It is the **legacy data-only fallback** for per-plugin configuration; the
//! canonical format is Lua, resolved by `bitty-runtime`'s plugin settings
//! loader. The runtime reads, in order, the canonical
//! `<config-dir>/plugins/<owner>.<name>.lua`, an unambiguous `<name>.lua`
//! short alias, this module's `<config-dir>/plugins/<owner>.<name>.toml`, and
//! finally the manifest `[settings]` defaults. The first present file wins,
//! so a plugin with a Lua file never consults its TOML file.
//!
//! # File layout (legacy fallback)
//!
//! A plugin may own one optional TOML user file next to the managed
//! manifest:
//!
//! ```text
//! $XDG_CONFIG_HOME/bitty/plugins/<owner>.<name>.toml
//! ```
//!
//! (fallback `~/.config/bitty/plugins/<owner>.<name>.toml`; Windows
//! `%APPDATA%` / `%LOCALAPPDATA%` participate through the same platform
//! precedence as [`crate::file::config_dir_with_platform_env`].) The file
//! name derives from the validated plugin id only
//! (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*$`, mirroring
//! `bitty-plugin-host` `PluginId`, one dot, no separators), so a validated
//! id can never escape the `plugins/` directory: path traversal fails
//! closed with no file (`None`). Every root comes from the caller (env or
//! parameters); no home directory is ever hard-coded.
//!
//! An absent file (or an absent `plugins/` directory) is not an error: the
//! next lower layer wins (the manifest's shipped built-in defaults when no
//! other user file exists — Waybar parity: ships working, user overrides).
//! Present files override the defaults **per key**.
//!
//! # Subset grammar (v1, data only)
//!
//! The TOML file is parsed as data through this strict bounded subset.
//! Unlike the canonical Lua file it is never evaluated as code and never
//! executes anything: there is no `include`, no `exec`, no shell, no nesting.
//! (The canonical Lua file is likewise host-free, but it is evaluated as
//! plain data in the `bitty-lua` configuration VM, not parsed here.)
//!
//! ```text
//! # flat keys only (no [sections], no dotted keys)
//! show = true                 # boolean
//! name_max_chars = 32         # bare integer (optional leading `-`)
//! edge = "bottom"             # double-quoted string (" \\ \n \t \r escapes)
//! modules = ["a", "b"]        # array of double-quoted strings (one line)
//! ```
//!
//! - Keys are bare `[A-Za-z0-9_.-]+` or double-quoted; anything else fails
//!   the whole file closed (fall back to defaults).
//! - Values are booleans, bare integers, double-quoted strings, or
//!   single-line string arrays. Floats, quoted numbers, inline tables,
//!   nested arrays, and multi-line values are **not** in the subset: the
//!   offending key falls back to its default while the rest of the file
//!   still applies (never abort the plugin for one bad key).
//! - Unknown keys (keys the manifest never declared) are kept by the parser
//!   and dropped at merge time, recorded as skipped: they change nothing.
//! - Duplicate keys fail the whole file closed (silent shadowing would be a
//!   side channel); structural garbage (a `[section]`, a line without `=`,
//!   a malformed key) fails the whole file closed too. Either way the
//!   plugin keeps running on defaults. An unterminated string or array is a
//!   value-shape problem, not structural: the key is skipped while the rest
//!   of the file still applies.
//! - Tables are not in the v1 grammar: a table value fails to the default
//!   for that key. Bar's `modules` (a string array) is the first customer;
//!   no declared customer needs tables, so the hostile surface stays small.
//!
//! # Bounds (fail closed before any value is trusted)
//!
//! - File bytes `<= MAX_PLUGIN_FILE_BYTES` (16 KiB), lines
//!   `<= MAX_PLUGIN_FILE_LINES` (512), each `<= MAX_PLUGIN_LINE_BYTES`.
//! - Keys per file `<= MAX_PLUGIN_FILE_KEYS` (64); duplicate keys fail.
//! - String values `<= MAX_PLUGIN_STRING_BYTES` (512) with no NUL.
//! - Arrays hold `<= MAX_PLUGIN_ARRAY_ITEMS` (32) string items of
//!   `<= MAX_PLUGIN_ARRAY_ITEM_BYTES` (128) each; non-string items fail the
//!   key to its default.
//! - Non-UTF-8 files fail closed (no lossy reads: a plugin must never
//!   observe mojibake it could mistake for configuration).
//!
//! # Defaults rule
//!
//! This module supplies only the legacy TOML layer. Full per-plugin
//! resolution belongs to the runtime: canonical Lua, unambiguous short
//! alias, this TOML file, then manifest defaults; the first present file
//! wins, so this file applies only when neither Lua file exists. Within a
//! file, precedence per declared key, lowest first:
//!
//! ```text
//! manifest shipped default < old home (shared host source) < this TOML file
//! ```
//!
//! A wrong-typed value at any layer falls back to the next lower layer for
//! that key only (garbage in, fail-to-default out). Undeclared keys are
//! never readable through `bitty.settings.get`, whatever layer carries
//! them. The machine-managed manifest (`bitty-plugins.toml`) is untouched:
//! it stays machine-only and hand edits there keep today's fail-closed
//! behavior.
//!
//! # Responsibility split (quoted from issue #2010, not paraphrased)
//!
//! > - **Values → user.** Format strings, thresholds, module order, icons,
//! >   intervals: garbage in, fail-to-default out (the `read_settings`
//! >   pattern). A bad value can hide a module, never crash the host,
//! >   never widen authority. This half of "the user bears responsibility"
//! >   is endorsed.
//! >
//! > - **Authority → system, non-delegable.** No config value, however
//! >   explicit, can grant authority: grants come only from manifest
//! >   declaration + consent, bound to the manifest hash in the
//! >   machine-managed manifest. Rationale: configs flow (shared, copied),
//! >   risk flows with them, but informedness does not — nobody may assume
//! >   risk on behalf of a future copier. This half of "user bears
//! >   responsibility" is rejected by design (deny-by-default is P0).
//!
//! Files are data, never code. A `[capabilities]`-looking key in a plugin
//! file is an unknown key like any other: dropped, recorded, granting
//! nothing.
//!
//! # Ownership rules
//!
//! Pure `std`, no workspace-crate dependencies, `#![forbid(unsafe_code)]`
//! at the crate level. Headlessly testable on Linux CI and the
//! `windows-latest` job (paths only, no I/O here: the caller reads the
//! bytes).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Directory under the config root holding per-plugin user files
/// (`$XDG_CONFIG_HOME/bitty/plugins/`, CTX-1195).
pub const PLUGINS_DIR_NAME: &str = "plugins";

/// File extension of legacy per-plugin TOML user files.
pub const PLUGIN_FILE_EXTENSION: &str = "toml";

/// Maximum per-plugin file size in bytes (fail-closed).
pub const MAX_PLUGIN_FILE_BYTES: usize = 16 * 1024;

/// Maximum per-plugin file lines (fail-closed).
pub const MAX_PLUGIN_FILE_LINES: usize = 512;

/// Maximum bytes per line (fail-closed).
pub const MAX_PLUGIN_LINE_BYTES: usize = 1024;

/// Maximum keys per file (fail-closed).
pub const MAX_PLUGIN_FILE_KEYS: usize = 64;

/// Maximum bytes per string value (fail-closed).
pub const MAX_PLUGIN_STRING_BYTES: usize = 512;

/// Maximum items per string-array value (fail-closed).
pub const MAX_PLUGIN_ARRAY_ITEMS: usize = 32;

/// Maximum bytes per string-array item (fail-closed).
pub const MAX_PLUGIN_ARRAY_ITEM_BYTES: usize = 128;

/// Maximum bytes of one id segment (mirrors the `PluginId` 64-byte bound).
pub const MAX_PLUGIN_ID_SEGMENT_BYTES: usize = 64;

// ---------------------------------------------------------------------------
// Paths (pure, no I/O)
// ---------------------------------------------------------------------------

/// Whether `plugin_id` is a safe file stem (`owner.name`, one dot, no
/// separators).
///
/// Mirrors the `bitty-plugin-host` `PluginId` grammar without depending on
/// it: exactly `owner.name`, each segment `1..=64` bytes of
/// `[a-z][a-z0-9_-]` starting with a lowercase letter. Anything else —
/// traversal (`..`, `/`, `\`), whitespace, quoting — fails closed with no
/// file.
#[must_use]
pub fn valid_plugin_file_id(plugin_id: &str) -> bool {
    if plugin_id.is_empty() || plugin_id.len() > 2 * MAX_PLUGIN_ID_SEGMENT_BYTES + 1 {
        return false;
    }
    let Some((owner, name)) = plugin_id.split_once('.') else {
        return false;
    };
    // Exactly one dot: the name half must not contain another.
    if name.contains('.') {
        return false;
    }
    for segment in [owner, name] {
        if segment.is_empty() || segment.len() > MAX_PLUGIN_ID_SEGMENT_BYTES {
            return false;
        }
        let mut bytes = segment.bytes();
        match bytes.next() {
            Some(first) if first.is_ascii_lowercase() => {}
            _ => return false,
        }
        if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return false;
        }
    }
    true
}

/// Pure legacy per-plugin TOML file path with injected environment values.
///
/// `<config-dir>/plugins/<plugin_id>.toml` under
/// [`crate::file::config_dir_with_env`]. Returns `None` when no config root
/// is usable or when `plugin_id` fails [`valid_plugin_file_id`]
/// (fail-closed, no file). No I/O, no environment reads.
#[must_use]
pub fn plugin_file_path_with_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
    plugin_id: &str,
) -> Option<PathBuf> {
    plugin_file_path_with_platform_env(xdg_config_home, home, None, None, plugin_id)
}

/// Platform-aware legacy per-plugin TOML file path with Windows `%APPDATA%` /
/// `%LOCALAPPDATA%` injected (same precedence as
/// [`crate::file::config_dir_with_platform_env`]).
///
/// Returns `None` under the same fail-closed conditions as
/// [`plugin_file_path_with_env`].
#[must_use]
pub fn plugin_file_path_with_platform_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
    appdata: Option<&str>,
    localappdata: Option<&str>,
    plugin_id: &str,
) -> Option<PathBuf> {
    if !valid_plugin_file_id(plugin_id) {
        return None;
    }
    let dir =
        crate::file::config_dir_with_platform_env(xdg_config_home, home, appdata, localappdata)?;
    let file_name = format!("{plugin_id}.{PLUGIN_FILE_EXTENSION}");
    // Defense in depth: the grammar above already excludes separators, but
    // a file name must never carry one even if the grammar ever widens.
    if file_name.contains('/') || file_name.contains('\\') {
        return None;
    }
    Some(dir.join(PLUGINS_DIR_NAME).join(file_name))
}

/// Reads the live environment for the legacy per-plugin TOML file path.
/// Thin wrapper so unit tests stay hermetic via
/// [`plugin_file_path_with_platform_env`].
#[must_use]
pub fn plugin_file_path(plugin_id: &str) -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let appdata = std::env::var("APPDATA").ok();
    let localappdata = std::env::var("LOCALAPPDATA").ok();
    plugin_file_path_with_platform_env(
        xdg.as_deref(),
        home.as_deref(),
        appdata.as_deref(),
        localappdata.as_deref(),
        plugin_id,
    )
}

/// Pure plugins-directory path with injected environment values
/// (`<config-dir>/plugins`). `None` when no config root is usable.
#[must_use]
pub fn plugins_dir_with_env(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    crate::file::config_dir_with_env(xdg_config_home, home).map(|dir| dir.join(PLUGINS_DIR_NAME))
}

// ---------------------------------------------------------------------------
// Values and errors (plain data)
// ---------------------------------------------------------------------------

/// One parsed per-plugin file value (subset shapes only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginFileValue {
    /// Boolean (`true` / `false`).
    Bool(bool),
    /// Bare integer.
    Int(i64),
    /// Double-quoted string.
    String(String),
    /// Array of double-quoted strings.
    Strings(Vec<String>),
}

/// A parsed per-plugin file: accepted values plus the skipped keys.
///
/// `skipped` names keys that were present but unusable (wrong shape,
/// overlong, NUL-bearing, or otherwise outside the subset), sorted for
/// deterministic diagnostics. The merge layer falls back to the default for
/// each skipped key while the rest of the file still applies.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedPluginFile {
    /// Accepted `key -> value` pairs.
    pub values: BTreeMap<String, PluginFileValue>,
    /// Skipped key names (fail-to-default, per key).
    pub skipped: Vec<String>,
}

/// Fail-closed parse error for a per-plugin file.
///
/// A whole-file error means the plugin runs on defaults for every key (plus
/// the old home below the file); per-key problems never surface here — they
/// land in [`ParsedPluginFile::skipped`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginFileError {
    /// File exceeds [`MAX_PLUGIN_FILE_BYTES`].
    Oversize {
        /// Actual byte length.
        actual: usize,
    },
    /// File exceeds [`MAX_PLUGIN_FILE_LINES`].
    TooManyLines {
        /// Actual line count (at least the ceiling + 1).
        actual: usize,
    },
    /// One line exceeds [`MAX_PLUGIN_LINE_BYTES`].
    LineTooLong {
        /// 1-based line number.
        line: usize,
    },
    /// File is not valid UTF-8 (no lossy reads, fail closed).
    InvalidUtf8,
    /// Structural garbage: a `[section]`, a line without `key = value`, an
    /// unterminated string/array, a malformed key, duplicate keys, or more
    /// than [`MAX_PLUGIN_FILE_KEYS`] keys.
    Malformed {
        /// 1-based line number (0 when the error is not line-attributable,
        /// e.g. duplicate keys or key-count overflow found at the end).
        line: usize,
        /// Bounded reason (never echoes file bytes beyond the key name).
        reason: String,
    },
}

impl std::fmt::Display for PluginFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversize { actual } => {
                write!(
                    f,
                    "plugin config exceeds {MAX_PLUGIN_FILE_BYTES} bytes ({actual} bytes)"
                )
            }
            Self::TooManyLines { actual } => {
                write!(
                    f,
                    "plugin config exceeds {MAX_PLUGIN_FILE_LINES} lines ({actual} lines)"
                )
            }
            Self::LineTooLong { line } => {
                write!(
                    f,
                    "plugin config line {line} exceeds {MAX_PLUGIN_LINE_BYTES} bytes"
                )
            }
            Self::InvalidUtf8 => write!(f, "plugin config must be valid UTF-8"),
            Self::Malformed { line, reason } => {
                if *line == 0 {
                    write!(f, "plugin config malformed: {reason}")
                } else {
                    write!(f, "plugin config malformed at line {line}: {reason}")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parser (bounded TOML subset, data only)
// ---------------------------------------------------------------------------

/// Parse per-plugin file bytes into [`ParsedPluginFile`].
///
/// Pure and headless: the caller supplies the already-read bytes (no I/O,
/// no environment). Whole-file problems return [`PluginFileError`] (the
/// caller falls back to defaults for every key); per-key problems land in
/// [`ParsedPluginFile::skipped`] while the rest of the file applies.
pub fn parse_plugin_file(bytes: &[u8]) -> Result<ParsedPluginFile, PluginFileError> {
    if bytes.len() > MAX_PLUGIN_FILE_BYTES {
        return Err(PluginFileError::Oversize {
            actual: bytes.len(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| PluginFileError::InvalidUtf8)?;
    let mut parsed = ParsedPluginFile::default();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line_number > MAX_PLUGIN_FILE_LINES {
            return Err(PluginFileError::TooManyLines {
                actual: line_number,
            });
        }
        if raw_line.len() > MAX_PLUGIN_LINE_BYTES {
            return Err(PluginFileError::LineTooLong { line: line_number });
        }
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        // Flat keys only: sections are structural garbage in v1.
        if line.starts_with('[') {
            return Err(PluginFileError::Malformed {
                line: line_number,
                reason: "sections are not supported (flat keys only)".to_string(),
            });
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(PluginFileError::Malformed {
                line: line_number,
                reason: "expected 'key = value'".to_string(),
            });
        };
        let key = parse_file_key(raw_key.trim()).map_err(|reason| PluginFileError::Malformed {
            line: line_number,
            reason,
        })?;
        if let Some(first) = seen.insert(key.clone(), line_number) {
            let _ = first;
            return Err(PluginFileError::Malformed {
                line: 0,
                reason: format!("duplicate key '{key}'"),
            });
        }
        if seen.len() > MAX_PLUGIN_FILE_KEYS {
            return Err(PluginFileError::Malformed {
                line: 0,
                reason: format!("more than {MAX_PLUGIN_FILE_KEYS} keys"),
            });
        }
        match parse_file_value(raw_value.trim(), &key) {
            Ok(value) => {
                parsed.values.insert(key, value);
            }
            Err(_reason) => {
                parsed.skipped.push(key);
            }
        }
    }
    parsed.skipped.sort();
    Ok(parsed)
}

/// Parse one file key: bare `[A-Za-z0-9_.-]+` or double-quoted.
///
/// Quoted keys allow dots and dashes explicitly; the merge layer still
/// matches keys against the declared `^[a-z][a-z0-9_]*$` grammar, so a key
/// no declaration names is dropped there. Overlong keys fail here.
fn parse_file_key(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("expected a key".to_string());
    }
    // Declared-shaped keys have a tighter bound at the manifest layer; the
    // file layer caps raw key bytes so a hostile key cannot smuggle an
    // unbounded string into diagnostics.
    if raw.len() > 256 {
        return Err("key exceeds 256 bytes".to_string());
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        return parse_quoted(raw).map_err(|_| "malformed quoted key".to_string());
    }
    if raw.contains('"') || raw.contains('\'') || raw.chars().any(char::is_whitespace) {
        return Err(format!("malformed key '{raw}'"));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        return Err(format!("malformed key '{raw}'"));
    }
    Ok(raw.to_string())
}

/// Parse one file value; `Err` carries a bounded reason for the skipped-key
/// record (the key name travels separately in [`ParsedPluginFile::skipped`]).
fn parse_file_value(raw: &str, _key: &str) -> Result<PluginFileValue, String> {
    match raw {
        "true" => return Ok(PluginFileValue::Bool(true)),
        "false" => return Ok(PluginFileValue::Bool(false)),
        _ => {}
    }
    if looks_like_int(raw) {
        return parse_file_int(raw).map(PluginFileValue::Int);
    }
    if raw.starts_with('"') {
        let value = parse_quoted_bounded(raw, MAX_PLUGIN_STRING_BYTES)
            .map_err(|_| "bad string".to_string())?;
        if value.contains('\0') {
            return Err("NUL in string".to_string());
        }
        return Ok(PluginFileValue::String(value));
    }
    if raw.starts_with('[') {
        return parse_string_array(raw).map(PluginFileValue::Strings);
    }
    Err("unsupported value shape".to_string())
}

/// Whether `raw` has integer shape: optional `-` plus ASCII digits.
fn looks_like_int(raw: &str) -> bool {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Parse a bare integer (fail-closed on overflow and absurd digit runs).
fn parse_file_int(raw: &str) -> Result<i64, String> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.len() > 19 {
        return Err("integer too large".to_string());
    }
    raw.parse::<i64>()
        .map_err(|_| "integer too large".to_string())
}

/// Parse a double-quoted string with `\" \\ \n \t \r` escapes.
fn parse_quoted(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
        return Err("expected a quoted string".to_string());
    }
    // A lone `"` (len 1) cannot be a string; len >= 2 with both quotes but
    // unbalanced interior (e.g. `"a`) fails here via the ends check above
    // only when the raw itself is the whole token — interior quotes are
    // literal content unless escaped, matching the manifest subset.
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(_) => return Err("unsupported escape".to_string()),
                None => return Err("truncated escape".to_string()),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

/// Parse a double-quoted string with a byte bound (fail-closed).
fn parse_quoted_bounded(raw: &str, max_bytes: usize) -> Result<String, String> {
    // Bound the raw token first so an absurd literal fails fast.
    if raw.len() > max_bytes + 2 {
        return Err("string too long".to_string());
    }
    let value = parse_quoted(raw)?;
    if value.len() > max_bytes {
        return Err("string too long".to_string());
    }
    Ok(value)
}

/// Parse a single-line `["a", "b"]` string array.
///
/// Items must be double-quoted strings of `<= MAX_PLUGIN_ARRAY_ITEM_BYTES`
/// bytes with no NUL; at most [`MAX_PLUGIN_ARRAY_ITEMS`] items. Anything
/// else fails the key (never the file).
fn parse_string_array(raw: &str) -> Result<Vec<String>, String> {
    let value = raw.trim();
    if !(value.starts_with('[') && value.ends_with(']')) {
        return Err("unterminated array".to_string());
    }
    let inner = &value[1..value.len() - 1];
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let mut current: Option<String> = None;
    let mut in_string = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if in_string {
            let buf = current.as_mut().ok_or("array parser state")?;
            if escaped {
                match ch {
                    '"' => buf.push('"'),
                    '\\' => buf.push('\\'),
                    'n' => buf.push('\n'),
                    't' => buf.push('\t'),
                    'r' => buf.push('\r'),
                    _ => return Err("unsupported escape in array".to_string()),
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            } else {
                buf.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current = Some(String::new());
            }
            ',' | ' ' | '\t' | '\n' | '\r' => {
                if let Some(item) = current.take() {
                    push_array_item(&mut items, item)?;
                }
            }
            _ => return Err("unexpected character in array".to_string()),
        }
    }
    if in_string {
        return Err("unterminated string in array".to_string());
    }
    if let Some(item) = current.take() {
        push_array_item(&mut items, item)?;
    }
    Ok(items)
}

/// Push one validated array item (bounds + NUL, fail-closed per key).
fn push_array_item(items: &mut Vec<String>, item: String) -> Result<(), String> {
    if items.len() >= MAX_PLUGIN_ARRAY_ITEMS {
        return Err("too many array items".to_string());
    }
    if item.len() > MAX_PLUGIN_ARRAY_ITEM_BYTES {
        return Err("array item too long".to_string());
    }
    if item.contains('\0') {
        return Err("NUL in array item".to_string());
    }
    items.push(item);
    Ok(())
}

/// Strip a `#` comment, respecting double-quoted strings (with `\`
/// escapes). Mirrors the manifest-subset reader.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if *byte == b'"' {
            in_string = true;
        } else if *byte == b'#' {
            return &line[..index];
        }
    }
    line
}

/// Whether a `Path` looks like a per-plugin file name (`<id>.toml` with a
/// valid id stem). Used by hosts that scan the directory: a hostile file
/// name never resolves to a plugin.
#[must_use]
pub fn valid_plugin_file_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".toml") else {
        return false;
    };
    // The id grammar already excludes separators, so the stem is the id.
    valid_plugin_file_id(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_derive_from_env_without_hardcoded_home() {
        let path = plugin_file_path_with_env(
            Some("/tmp/bitty-xdg"),
            Some("/home/alice"),
            "bitty-terminal.bar",
        )
        .expect("xdg root wins");
        assert_eq!(
            path,
            PathBuf::from("/tmp/bitty-xdg")
                .join("bitty")
                .join("plugins")
                .join("bitty-terminal.bar.toml")
        );
        // HOME fallback.
        let path = plugin_file_path_with_env(None, Some("/home/alice"), "xuepoo.demo")
            .expect("home fallback");
        assert_eq!(
            path,
            PathBuf::from("/home/alice")
                .join(".config")
                .join("bitty")
                .join("plugins")
                .join("xuepoo.demo.toml")
        );
        // Windows APPDATA participates.
        let path = plugin_file_path_with_platform_env(
            None,
            None,
            Some("C:\\Users\\alice\\AppData\\Roaming"),
            None,
            "xuepoo.demo",
        )
        .expect("appdata fallback");
        assert_eq!(
            path,
            PathBuf::from("C:\\Users\\alice\\AppData\\Roaming")
                .join("bitty")
                .join("plugins")
                .join("xuepoo.demo.toml")
        );
        // No root: no file.
        assert!(plugin_file_path_with_env(None, None, "xuepoo.demo").is_none());
        assert!(plugins_dir_with_env(None, None).is_none());
    }

    #[test]
    fn hostile_ids_never_escape_the_plugins_dir() {
        for bad in [
            "",
            "owner",
            "a.b.c",
            "Owner.name",
            "owner.Name",
            "../escape",
            "owner/../x",
            "owner\\name",
            "owner.name.toml",
            ".hidden",
            "owner.",
            ".name",
            "own er.name",
            "owner.na me",
            "owner.name\n",
            "-owner.name",
            "owner.-name",
            "0wner.name",
        ] {
            assert!(!valid_plugin_file_id(bad), "'{bad}' must be rejected");
            assert!(
                plugin_file_path_with_env(Some("/tmp/x"), None, bad).is_none(),
                "'{bad}' must yield no path"
            );
        }
        for good in ["a.b", "bitty-terminal.bar", "xuepoo.demo-2_x"] {
            assert!(valid_plugin_file_id(good), "'{good}' must be accepted");
        }
        // File-name gate agrees with the id gate.
        assert!(valid_plugin_file_name(Path::new("bitty-terminal.bar.toml")));
        assert!(!valid_plugin_file_name(Path::new("../evil.toml")));
        assert!(!valid_plugin_file_name(Path::new("evil.lua")));
    }

    #[test]
    fn bar_shaped_file_parses() {
        let body = b"show = true\nedge = \"bottom\"\nshow_single = false\nname_max_chars = 32\nmodules = [\"workspaces\", \"clock\"]\n";
        let parsed = parse_plugin_file(body).expect("bar shape must parse");
        assert_eq!(
            parsed.values.get("show"),
            Some(&PluginFileValue::Bool(true))
        );
        assert_eq!(
            parsed.values.get("edge"),
            Some(&PluginFileValue::String("bottom".to_string()))
        );
        assert_eq!(
            parsed.values.get("name_max_chars"),
            Some(&PluginFileValue::Int(32))
        );
        assert_eq!(
            parsed.values.get("modules"),
            Some(&PluginFileValue::Strings(vec![
                "workspaces".to_string(),
                "clock".to_string()
            ]))
        );
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn comments_blank_lines_and_negative_ints() {
        let body =
            b"# leading comment\n\nshow = false # trailing comment\noffset = -12\nempty = []\n";
        let parsed = parse_plugin_file(body).expect("comments must parse");
        assert_eq!(
            parsed.values.get("show"),
            Some(&PluginFileValue::Bool(false))
        );
        assert_eq!(
            parsed.values.get("offset"),
            Some(&PluginFileValue::Int(-12))
        );
        assert_eq!(
            parsed.values.get("empty"),
            Some(&PluginFileValue::Strings(Vec::new()))
        );
        // A `#` inside a string is content, not a comment.
        let body = b"edge = \"a#b\"\n";
        let parsed = parse_plugin_file(body).expect("hash in string");
        assert_eq!(
            parsed.values.get("edge"),
            Some(&PluginFileValue::String("a#b".to_string()))
        );
    }

    #[test]
    fn hostile_files_fail_to_defaults_whole_file() {
        // Oversize.
        let big = vec![b'x'; MAX_PLUGIN_FILE_BYTES + 1];
        assert!(matches!(
            parse_plugin_file(&big),
            Err(PluginFileError::Oversize { .. })
        ));
        // Too many lines (blank/comment lines count: the bound is on read
        // effort, and distinct keys would trip the key cap first).
        let many = "# pad\n".repeat(MAX_PLUGIN_FILE_LINES + 1);
        assert!(matches!(
            parse_plugin_file(many.as_bytes()),
            Err(PluginFileError::TooManyLines { .. })
        ));
        // Overlong line.
        let long = format!("show = true #{}\n", "x".repeat(MAX_PLUGIN_LINE_BYTES));
        assert!(matches!(
            parse_plugin_file(long.as_bytes()),
            Err(PluginFileError::LineTooLong { .. })
        ));
        // Non-UTF-8 fails closed (no lossy reads).
        assert_eq!(
            parse_plugin_file(&[0x73, 0x3d, 0xff, 0x0a]),
            Err(PluginFileError::InvalidUtf8)
        );
        // Sections are structural garbage.
        assert!(matches!(
            parse_plugin_file(b"[capabilities]\nui.rich = true\n"),
            Err(PluginFileError::Malformed { .. })
        ));
        // A capabilities-looking key as a section grants nothing: the file
        // fails closed wholesale.
        assert!(parse_plugin_file(b"[capabilities]\n").is_err());
        // Line without `=`.
        assert!(matches!(
            parse_plugin_file(b"show\n"),
            Err(PluginFileError::Malformed { .. })
        ));
        // Duplicate keys fail closed (no silent shadowing).
        assert!(matches!(
            parse_plugin_file(b"show = true\nshow = false\n"),
            Err(PluginFileError::Malformed { .. })
        ));
        // Unterminated string / array fails the key, never the file: lines
        // are independent, so there is nothing to mask past the newline.
        let parsed =
            parse_plugin_file(b"edge = \"bottom\n").expect("unterminated string skips per key");
        assert!(!parsed.values.contains_key("edge"));
        assert!(parsed.skipped.contains(&"edge".to_string()));
        let parsed =
            parse_plugin_file(b"modules = [\"a\"\n").expect("unterminated array skips per key");
        assert!(!parsed.values.contains_key("modules"));
        assert!(parsed.skipped.contains(&"modules".to_string()));
        // Absurd key count.
        let mut crowded = String::new();
        for n in 0..MAX_PLUGIN_FILE_KEYS + 1 {
            crowded.push_str(&format!("key{n} = true\n"));
        }
        assert!(parse_plugin_file(crowded.as_bytes()).is_err());
    }

    #[test]
    fn hostile_values_fail_per_key_rest_applies() {
        let body = b"show = true\nedge = 3.5\nmodules = [\"a\", 7]\nfloaty = 1e3\ntable = { a = 1 }\nsingle = 'x'\nplus = +42\n";
        let parsed = parse_plugin_file(body).expect("per-key failures must not abort");
        // The good key still applies.
        assert_eq!(
            parsed.values.get("show"),
            Some(&PluginFileValue::Bool(true))
        );
        // Every bad shape is skipped, never trusted.
        for key in ["edge", "modules", "floaty", "table", "single", "plus"] {
            assert!(
                !parsed.values.contains_key(key),
                "'{key}' must not be trusted"
            );
            assert!(
                parsed.skipped.contains(&key.to_string()),
                "'{key}' recorded"
            );
        }
        // A quoted `"32"` IS a valid file-layer string: the parser keeps it
        // and the merge fails it to the default for an int-declared key.
        let parsed = parse_plugin_file(b"name_max_chars = \"32\"\n").expect("string shape parses");
        assert_eq!(
            parsed.values.get("name_max_chars"),
            Some(&PluginFileValue::String("32".to_string()))
        );
    }

    #[test]
    fn overlong_and_nul_values_skipped_per_key() {
        // Overlong string.
        let body = format!(
            "edge = \"{}\"\nshow = true\n",
            "e".repeat(MAX_PLUGIN_STRING_BYTES + 1)
        );
        let parsed = parse_plugin_file(body.as_bytes()).expect("must not abort");
        assert!(!parsed.values.contains_key("edge"));
        assert_eq!(
            parsed.values.get("show"),
            Some(&PluginFileValue::Bool(true))
        );
        // NUL in a string.
        let body = b"edge = \"a\\0b\"\nshow = true\n";
        let parsed = parse_plugin_file(body).expect("must not abort");
        // `\0` is an unsupported escape, so the key is skipped either way.
        assert!(!parsed.values.contains_key("edge"));
        // Overlong array item.
        let body = format!(
            "modules = [\"{}\"]\n",
            "m".repeat(MAX_PLUGIN_ARRAY_ITEM_BYTES + 1)
        );
        let parsed = parse_plugin_file(body.as_bytes()).expect("must not abort");
        assert!(!parsed.values.contains_key("modules"));
        // Too many items.
        let mut body = String::from("modules = [");
        for n in 0..MAX_PLUGIN_ARRAY_ITEMS + 1 {
            if n > 0 {
                body.push_str(", ");
            }
            body.push_str(&format!("\"m{n}\""));
        }
        body.push_str("]\n");
        let parsed = parse_plugin_file(body.as_bytes()).expect("must not abort");
        assert!(!parsed.values.contains_key("modules"));
        // Integer overflow.
        let parsed = parse_plugin_file(b"name_max_chars = 99999999999999999999999\nshow = true\n")
            .expect("must not abort");
        assert!(!parsed.values.contains_key("name_max_chars"));
        assert_eq!(
            parsed.values.get("show"),
            Some(&PluginFileValue::Bool(true))
        );
    }

    #[test]
    fn unknown_keys_kept_for_merge_to_drop() {
        // The parser keeps unknown keys as data; the merge drops them per
        // the manifest declaration (never aborts the plugin).
        let body = b"show = true\nfrobnicate = 7\ncapabilities = true\n";
        let parsed = parse_plugin_file(body).expect("unknown keys must not abort");
        assert_eq!(
            parsed.values.get("show"),
            Some(&PluginFileValue::Bool(true))
        );
        assert!(parsed.values.contains_key("frobnicate"));
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn utf8_edge_cases() {
        // Valid multi-byte content passes through untouched.
        let body = "edge = \"caf\u{e9}\"\nmodules = [\"\u{2014}\"]\n".as_bytes();
        let parsed = parse_plugin_file(body).expect("valid UTF-8 must parse");
        assert_eq!(
            parsed.values.get("edge"),
            Some(&PluginFileValue::String("caf\u{e9}".to_string()))
        );
        // Lone continuation byte is invalid UTF-8: whole file fails closed.
        assert_eq!(
            parse_plugin_file(b"edge = \"\x80\"\n"),
            Err(PluginFileError::InvalidUtf8)
        );
    }
}
