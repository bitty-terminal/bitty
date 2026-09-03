//! Bootstrap user config-file loading: `config.toml` under the XDG config root.
//!
//! # Status — bootstrap ahead of `init.lua`
//!
//! The canonical long-term direction remains `init.lua` plus `lua/` modules
//! under `$XDG_CONFIG_HOME/bitty/` (see
//! `bitty-docs/docs/configuration/lua-and-xdg.md`, draft, and DEC-0010).
//! A full Lua runtime (vendored Lua 5.4 evaluation, module resolution,
//! sandboxing) is a much larger slice, so this module implements the minimal
//! bootstrap: a **data-only TOML subset** at the same XDG root
//! (`$XDG_CONFIG_HOME/bitty/config.toml`, fallback `~/.config/bitty/`).
//! TOML is never executed as code; it parses to plain [`ConfigPlan`] data and
//! flows through the existing validate/migrate/merge machinery unchanged. A
//! future Lua loader will produce the same [`LayerKind::User`] layer, so the
//! migration path is structural, not a flag day.
//!
//! # Format (bootstrap subset)
//!
//! ```toml
//! # Top-level alias for the common case (matches the CTX-0148 example).
//! theme = "dark"
//!
//! [appearance]
//! theme = "bitty-dark"
//!
//! [font]
//! family = "JetBrains Mono"
//! size = 13.0
//!
//! [window]
//! opacity = 0.95
//! padding = 8
//!
//! [terminal]
//! scrollback = 10000
//! shell = "/bin/fish"
//! ```
//!
//! - Tables [`appearance`], [`font`], [`window`], [`terminal`] are optional.
//! - Top-level `theme` is a convenience alias for `[appearance] theme`; when
//!   both are present the table value wins (documented, deterministic).
//! - `[font]` must set **both** `family` and `size` together; `[window]` must
//!   set **both** `opacity` and `padding` together; `[terminal]` must set
//!   `scrollback` (`shell` is optional). Partial tables are rejected
//!   fail-closed rather than silently filling defaults (which would corrupt
//!   attribution against lower layers).
//! - Keymaps, plugins, profiles, `extends`, distributions, and system policy
//!   remain Lua-track and are rejected as undeclared here.
//! - Values are double-quoted basic strings (with `\"`, `\\`, `\n`, `\t`,
//!   `\r`), single-quoted literal strings (no escapes), integers, or floats.
//!   Arrays, inline tables, booleans, and dates are rejected fail-closed.
//! - `#` starts a comment outside quotes; blank lines are ignored.
//! - Duplicate keys, unknown tables/keys, malformed lines, oversize files,
//!   and values that fail typed validation all fail closed with a
//!   [`ConfigError`] (never a panic, never a silent ignore).
//!
//! # Bounds (threat T-01)
//!
//! - File bytes `<= MAX_CONFIG_FILE_BYTES` (64 KiB).
//! - Lines `<= MAX_CONFIG_LINES` (2048), each `<= MAX_LINE_BYTES` (4096).
//! - All string/number bounds from [`crate::types`] still apply via
//!   [`ConfigPlan::validate`].
//!
//! # Precedence
//!
//! `CLI > file > defaults`: the file yields a [`LayerKind::User`] plan, an
//! explicit `--theme` CLI flag yields a [`LayerKind::Cli`] plan, and
//! [`merge_layers`](crate::merge_layers) sorts by precedence so the CLI wins.
//! Missing files are not errors for the default path (bare `bitty` keeps
//! working); a missing **explicit** `--config` path is an error.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::migration::CURRENT_SCHEMA_VERSION;
use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
use crate::types::{AppearanceConfig, FontConfig, TerminalConfig, WindowConfig};

/// Config directory name under the XDG config root.
pub const CONFIG_DIR_NAME: &str = "bitty";

/// Bootstrap config file name (TOML). `init.lua` remains the long-term name.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Maximum config file size in bytes (fail-closed).
pub const MAX_CONFIG_FILE_BYTES: usize = 64 * 1024;

/// Maximum config file lines (fail-closed).
pub const MAX_CONFIG_LINES: usize = 2048;

/// Maximum bytes per line (fail-closed).
pub const MAX_LINE_BYTES: usize = 4096;

/// Allowed tables in the bootstrap subset.
const ALLOWED_TABLES: &[&str] = &["appearance", "font", "window", "terminal"];

/// Pure, testable XDG resolution with injected environment values.
///
/// - `xdg_config_home`: value of `$XDG_CONFIG_HOME` (if set).
/// - `home`: value of `$HOME` (fallback root).
///
/// Returns `None` only when neither yields a usable root (no panic).
#[must_use]
pub fn default_config_path_with_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(
                Path::new(trimmed)
                    .join(CONFIG_DIR_NAME)
                    .join(CONFIG_FILE_NAME),
            );
        }
    }
    if let Some(h) = home {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return Some(
                Path::new(trimmed)
                    .join(".config")
                    .join(CONFIG_DIR_NAME)
                    .join(CONFIG_FILE_NAME),
            );
        }
    }
    None
}

/// Reads the live environment (`$XDG_CONFIG_HOME`, `$HOME`) for the default
/// config path. Thin wrapper over [`default_config_path_with_env`] so unit
/// tests stay hermetic (no env mutation).
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    default_config_path_with_env(xdg.as_deref(), home.as_deref())
}

/// Pure path resolution: explicit `--config` wins verbatim, else the default
/// path from injected env values. Returns `None` when no path can be formed.
#[must_use]
pub fn resolve_config_path_with(
    explicit: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.trim().is_empty() {
            return default_config_path_with_env(xdg_config_home, home);
        }
        return Some(PathBuf::from(p));
    }
    default_config_path_with_env(xdg_config_home, home)
}

/// Live-environment path resolution for the app startup path.
#[must_use]
pub fn resolve_config_path(explicit: Option<&str>) -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    resolve_config_path_with(explicit, xdg.as_deref(), home.as_deref())
}

/// Builds the CLI override layer for an explicit `--theme` value.
///
/// Returns `None` when `theme` is `None`/empty/whitespace (no override, so
/// the file/default wins). Otherwise returns a [`LayerKind::Cli`] plan that
/// [`crate::merge_layers`] orders above the file layer. Validation happens at
/// merge time (overlong names fail closed there).
#[must_use]
pub fn cli_theme_layer(theme: Option<&str>) -> Option<LayeredPlan> {
    let raw = theme?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let plan = ConfigPlan {
        appearance: Some(AppearanceConfig {
            theme: Some(trimmed.to_string()),
        }),
        schema_version: Some(CURRENT_SCHEMA_VERSION),
        ..Default::default()
    };
    Some(LayeredPlan::new(
        ConfigSource::new(LayerKind::Cli, Some("cli:--theme")),
        plan,
    ))
}

/// Merges an optional file (User) layer with an optional `--theme` CLI layer
/// into the effective config. Pure and headless: precedence is `CLI > file >
/// defaults` via [`crate::merge_layers`] (empty layers merge to core
/// defaults with attribution).
///
/// # Errors
///
/// Returns the first [`ConfigError`] from validation or merge (including
/// policy violations, which remain hard errors here).
pub fn resolve_effective(
    file: Option<LayeredPlan>,
    cli_theme: Option<&str>,
) -> Result<crate::merge::MergedConfig, ConfigError> {
    let mut layers = Vec::new();
    if let Some(f) = file {
        layers.push(f);
    }
    if let Some(cli) = cli_theme_layer(cli_theme) {
        layers.push(cli);
    }
    crate::merge::merge_layers(layers)
}

// ---------------------------------------------------------------------------
// TOML subset parser
// ---------------------------------------------------------------------------

/// Parsed scalar value in the bootstrap subset.
#[derive(Debug, Clone, PartialEq)]
enum TomlValue {
    /// Quoted string (basic or literal).
    Str(String),
    /// Integer literal.
    Int(i64),
    /// Float literal.
    Float(f64),
}

/// Strips a trailing `#` comment outside quotes.
///
/// Handles `"` basic strings (with `\` escapes) and `'` literal strings.
/// Returns the line truncated before the comment (still needs trimming).
fn strip_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        match ch {
            '"' => in_double = true,
            '\'' => in_single = true,
            '#' => return &line[..idx],
            _ => {}
        }
    }
    line
}

/// Parses a double-quoted basic string (including the quotes).
///
/// Supports `\"`, `\\`, `\n`, `\t`, `\r`. Any other escape, control
/// character, or unterminated string is an error.
fn parse_basic_string(raw: &str, line_no: usize) -> Result<String, ConfigError> {
    debug_assert!(raw.starts_with('"'));
    let mut out = String::new();
    let mut chars = raw[1..].chars();
    loop {
        let Some(ch) = chars.next() else {
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: unterminated string"),
            ));
        };
        match ch {
            '"' => {
                // Trailing characters after the closing quote are rejected by
                // the caller (value must be exactly one scalar).
                return Ok(out);
            }
            '\\' => {
                let Some(esc) = chars.next() else {
                    return Err(ConfigError::validation(
                        "config",
                        format!("line {line_no}: unterminated escape"),
                    ));
                };
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    _ => {
                        return Err(ConfigError::validation(
                            "config",
                            format!("line {line_no}: unsupported escape '\\{esc}'"),
                        ));
                    }
                }
            }
            c if (c as u32) < 0x20 => {
                return Err(ConfigError::validation(
                    "config",
                    format!("line {line_no}: control character in string"),
                ));
            }
            c => out.push(c),
        }
    }
}

/// Parses a single scalar value (already comment-stripped and trimmed).
fn parse_value(raw: &str, line_no: usize) -> Result<(TomlValue, usize), ConfigError> {
    if let Some(rest) = raw.strip_prefix('"') {
        let _ = rest;
        let value = parse_basic_string(raw, line_no)?;
        // Ensure nothing but whitespace follows the closing quote. The
        // parser finds the closing quote by re-scanning with escapes.
        let mut consumed = 1usize; // opening quote
        let mut chars = raw[1..].chars().peekable();
        let mut closed = false;
        while let Some(ch) = chars.next() {
            consumed += ch.len_utf8();
            if ch == '\\' {
                if let Some(esc) = chars.next() {
                    consumed += esc.len_utf8();
                }
                continue;
            }
            if ch == '"' {
                closed = true;
                break;
            }
        }
        if !closed {
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: unterminated string"),
            ));
        }
        let trailing = raw[consumed..].trim();
        if !trailing.is_empty() {
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: trailing characters after string"),
            ));
        }
        return Ok((TomlValue::Str(value), consumed));
    }
    if raw.starts_with('\'') {
        if raw.len() >= 2 && raw.ends_with('\'') && raw.len() > 1 {
            let inner = &raw[1..raw.len() - 1];
            if inner.contains('\'') || inner.contains('\n') {
                return Err(ConfigError::validation(
                    "config",
                    format!("line {line_no}: invalid literal string"),
                ));
            }
            return Ok((TomlValue::Str(inner.to_string()), raw.len()));
        }
        return Err(ConfigError::validation(
            "config",
            format!("line {line_no}: unterminated literal string"),
        ));
    }
    // Bare scalars: reject tables/arrays/bools/dates fail-closed; accept
    // integers and floats only.
    if raw.starts_with('[') || raw.starts_with('{') {
        return Err(ConfigError::validation(
            "config",
            format!("line {line_no}: arrays and inline tables are not supported in the bootstrap"),
        ));
    }
    if raw == "true" || raw == "false" {
        return Err(ConfigError::validation(
            "config",
            format!("line {line_no}: booleans are not supported in the bootstrap"),
        ));
    }
    // Try integer first (rejects `+`/hex/octal/bin fail-closed via parse).
    if let Ok(i) = raw.parse::<i64>() {
        // Reject float-looking ints with exponent/point that parsed as int?
        // `parse::<i64>` already rejects `.`/`e`, so this is exact.
        return Ok((TomlValue::Int(i), raw.len()));
    }
    if let Ok(f) = raw.parse::<f64>() {
        if !f.is_finite() {
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: non-finite number"),
            ));
        }
        return Ok((TomlValue::Float(f), raw.len()));
    }
    Err(ConfigError::validation(
        "config",
        format!("line {line_no}: invalid value {raw:?} (quote strings)"),
    ))
}

/// Validates a bare key (`[A-Za-z0-9_-]+`, non-empty).
fn check_key(key: &str, line_no: usize) -> Result<(), ConfigError> {
    if key.is_empty() {
        return Err(ConfigError::validation(
            "config",
            format!("line {line_no}: empty key"),
        ));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ConfigError::validation(
            "config",
            format!("line {line_no}: invalid key {key:?}"),
        ));
    }
    Ok(())
}

/// Parses TOML bootstrap content into a [`ConfigPlan`].
///
/// `source` is used for error attribution (`UndeclaredField.source`). The
/// returned plan has `schema_version: None` (implicit 0); callers run
/// [`crate::migrate`] before merging (see [`load_user_layer`]).
///
/// # Errors
///
/// Fail-closed [`ConfigError`] on oversize input, malformed lines, duplicate
/// keys, unknown tables/keys, type mismatches, partial tables, or typed
/// validation failures (with line numbers in the message).
pub fn parse_toml_config(content: &str, source: &ConfigSource) -> Result<ConfigPlan, ConfigError> {
    if content.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "config file exceeds {MAX_CONFIG_FILE_BYTES} bytes ({} bytes)",
                content.len()
            ),
        });
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > MAX_CONFIG_LINES {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "config file exceeds {MAX_CONFIG_LINES} lines ({} lines)",
                lines.len()
            ),
        });
    }

    let src_desc = source.describe();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut table: Option<String> = None;

    let mut top_theme: Option<(String, usize)> = None;
    let mut appearance_theme: Option<(String, usize)> = None;
    let mut font_family: Option<(String, usize)> = None;
    let mut font_size: Option<(f64, usize)> = None;
    let mut font_size_is_float = false;
    let mut window_opacity: Option<(f64, usize)> = None;
    let mut window_padding: Option<(i64, usize)> = None;
    let mut terminal_scrollback: Option<(i64, usize)> = None;
    let mut terminal_shell: Option<(String, usize)> = None;
    // Track which tables were explicitly opened (for partial-table errors).
    let mut opened_tables: HashSet<String> = HashSet::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        if raw_line.len() > MAX_LINE_BYTES {
            return Err(ConfigError::InvalidInput {
                message: format!("line {line_no}: exceeds {MAX_LINE_BYTES} bytes"),
            });
        }
        let stripped = strip_comment(raw_line).trim();
        if stripped.is_empty() {
            continue;
        }
        if let Some(inner) = stripped.strip_prefix('[') {
            let Some(close) = inner.find(']') else {
                return Err(ConfigError::validation(
                    "config",
                    format!("line {line_no}: malformed table header (missing ']')"),
                ));
            };
            let name = inner[..close].trim();
            let trailing = inner[close + 1..].trim();
            if !trailing.is_empty() {
                return Err(ConfigError::validation(
                    "config",
                    format!("line {line_no}: trailing characters after table header"),
                ));
            }
            if name.is_empty() || !ALLOWED_TABLES.contains(&name) {
                return Err(ConfigError::UndeclaredField {
                    field: format!("[{name}]"),
                    source: Some(src_desc.clone()),
                });
            }
            table = Some(name.to_string());
            opened_tables.insert(name.to_string());
            continue;
        }
        let Some(eq) = stripped.find('=') else {
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: expected 'key = value'"),
            ));
        };
        let key = stripped[..eq].trim();
        let raw_value = stripped[eq + 1..].trim();
        check_key(key, line_no)?;
        if raw_value.is_empty() {
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: missing value for key {key:?}"),
            ));
        }
        let table_name = table.clone().unwrap_or_default();
        if !seen.insert((table_name.clone(), key.to_string())) {
            let field = if table_name.is_empty() {
                key.to_string()
            } else {
                format!("{table_name}.{key}")
            };
            return Err(ConfigError::validation(
                "config",
                format!("line {line_no}: duplicate key '{field}'"),
            ));
        }
        let (value, _) = parse_value(raw_value, line_no)?;
        let field = if table_name.is_empty() {
            key.to_string()
        } else {
            format!("{table_name}.{key}")
        };

        match (table_name.as_str(), key) {
            ("", "theme") => match value {
                TomlValue::Str(s) => top_theme = Some((s, line_no)),
                _ => {
                    return Err(ConfigError::validation(
                        "appearance.theme",
                        format!("line {line_no}: expected string for '{field}'"),
                    ));
                }
            },
            ("appearance", "theme") => match value {
                TomlValue::Str(s) => appearance_theme = Some((s, line_no)),
                _ => {
                    return Err(ConfigError::validation(
                        "appearance.theme",
                        format!("line {line_no}: expected string for '{field}'"),
                    ));
                }
            },
            ("font", "family") => match value {
                TomlValue::Str(s) => font_family = Some((s, line_no)),
                _ => {
                    return Err(ConfigError::validation(
                        "font.family",
                        format!("line {line_no}: expected string for '{field}'"),
                    ));
                }
            },
            ("font", "size") => match value {
                TomlValue::Float(f) => {
                    font_size = Some((f, line_no));
                    font_size_is_float = true;
                }
                TomlValue::Int(i) => {
                    font_size = Some((i as f64, line_no));
                    font_size_is_float = false;
                }
                TomlValue::Str(_) => {
                    return Err(ConfigError::validation(
                        "font.size",
                        format!("line {line_no}: expected number for '{field}'"),
                    ));
                }
            },
            ("window", "opacity") => match value {
                TomlValue::Float(f) => window_opacity = Some((f, line_no)),
                TomlValue::Int(i) => window_opacity = Some((i as f64, line_no)),
                TomlValue::Str(_) => {
                    return Err(ConfigError::validation(
                        "window.opacity",
                        format!("line {line_no}: expected number for '{field}'"),
                    ));
                }
            },
            ("window", "padding") => match value {
                TomlValue::Int(i) => window_padding = Some((i, line_no)),
                _ => {
                    return Err(ConfigError::validation(
                        "window.padding",
                        format!("line {line_no}: expected integer for '{field}'"),
                    ));
                }
            },
            ("terminal", "scrollback") => match value {
                TomlValue::Int(i) => terminal_scrollback = Some((i, line_no)),
                _ => {
                    return Err(ConfigError::validation(
                        "terminal.scrollback",
                        format!("line {line_no}: expected integer for '{field}'"),
                    ));
                }
            },
            ("terminal", "shell") => match value {
                TomlValue::Str(s) => terminal_shell = Some((s, line_no)),
                _ => {
                    return Err(ConfigError::validation(
                        "terminal.shell",
                        format!("line {line_no}: expected string for '{field}'"),
                    ));
                }
            },
            _ => {
                return Err(ConfigError::UndeclaredField {
                    field,
                    source: Some(src_desc.clone()),
                });
            }
        }
        let _ = font_size_is_float;
    }

    // Table-wins for theme alias.
    let theme_choice = appearance_theme.or(top_theme);
    let mut field_lines: HashMap<String, usize> = HashMap::new();
    if let Some((_, ln)) = &theme_choice {
        field_lines.insert("appearance.theme".to_string(), *ln);
    }
    if let Some((_, ln)) = &font_family {
        field_lines.insert("font.family".to_string(), *ln);
    }
    if let Some((_, ln)) = &font_size {
        field_lines.insert("font.size".to_string(), *ln);
    }
    if let Some((_, ln)) = &window_opacity {
        field_lines.insert("window.opacity".to_string(), *ln);
    }
    if let Some((_, ln)) = &window_padding {
        field_lines.insert("window.padding".to_string(), *ln);
    }
    if let Some((_, ln)) = &terminal_scrollback {
        field_lines.insert("terminal.scrollback".to_string(), *ln);
    }
    if let Some((_, ln)) = &terminal_shell {
        field_lines.insert("terminal.shell".to_string(), *ln);
    }

    // Atomic-table checks (fail-closed, no silent defaults).
    if opened_tables.contains("font") && (font_family.is_none() || font_size.is_none()) {
        return Err(ConfigError::validation(
            "font",
            "table [font] requires both 'family' (string) and 'size' (number)",
        ));
    }
    if opened_tables.contains("window") && (window_opacity.is_none() || window_padding.is_none()) {
        return Err(ConfigError::validation(
            "window",
            "table [window] requires both 'opacity' (number) and 'padding' (integer)",
        ));
    }
    if opened_tables.contains("terminal") && terminal_scrollback.is_none() {
        return Err(ConfigError::validation(
            "terminal.scrollback",
            "table [terminal] requires 'scrollback' (integer)",
        ));
    }

    // Range-check integers before narrowing (typed validation also checks,
    // but here we attach line numbers and avoid lossy casts).
    if let Some((v, ln)) = terminal_scrollback {
        if !(0..=100_000).contains(&v) {
            return Err(ConfigError::validation(
                "terminal.scrollback",
                format!("line {ln}: must be within [0, 100000] (found {v})"),
            ));
        }
    }
    if let Some((v, ln)) = window_padding {
        if !(0..=64).contains(&v) {
            return Err(ConfigError::validation(
                "window.padding",
                format!("line {ln}: must be within [0, 64] (found {v})"),
            ));
        }
    }

    let appearance = theme_choice.map(|(t, _)| AppearanceConfig { theme: Some(t) });
    let font = match (font_family, font_size) {
        (Some((family, _)), Some((size, _))) => Some(FontConfig {
            family,
            size: size as f32,
        }),
        (None, None) => None,
        _ => unreachable!("atomic [font] check above guarantees both-or-neither"),
    };
    let window = match (window_opacity, window_padding) {
        (Some((opacity, _)), Some((padding, _))) => Some(WindowConfig {
            opacity: opacity as f32,
            padding: padding as u32,
        }),
        (None, None) => None,
        _ => unreachable!("atomic [window] check above guarantees both-or-neither"),
    };
    let terminal = match (terminal_scrollback, terminal_shell) {
        (Some((scrollback, _)), shell) => Some(TerminalConfig {
            scrollback: scrollback as u32,
            shell: shell.map(|(s, _)| s),
        }),
        (None, None) => None,
        (None, Some(_)) => unreachable!("atomic [terminal] check guarantees scrollback"),
    };

    let plan = ConfigPlan {
        schema_version: None,
        font,
        window,
        terminal,
        appearance,
        keymaps: None,
        plugins: None,
        profile_name: None,
        extends: None,
        undeclared_fields: Vec::new(),
    };
    if let Err(err) = plan.validate() {
        // Attach the offending line when the field came from this file.
        let with_line = match &err {
            ConfigError::Validation { field, message } => {
                if let Some(ln) = field_lines.get(field) {
                    ConfigError::validation(field.clone(), format!("line {ln}: {message}"))
                } else {
                    err
                }
            }
            _ => err,
        };
        return Err(with_line);
    }
    Ok(plan)
}

/// Loads, parses, validates, and migrates a user config file into a
/// [`LayerKind::User`] plan.
///
/// The file **must** exist; missing files are the caller's decision (default
/// path skips, explicit `--config` fails). Oversize/unreadable/malformed/
/// invalid files fail closed with [`ConfigError`] (no panic).
///
/// # Errors
///
/// - [`ConfigError::InvalidInput`] for oversize or unreadable files.
/// - Parser/validation errors from [`parse_toml_config`].
/// - [`ConfigError::SchemaVersionUnsupported`] via migration (unreachable for
///   the bootstrap, which emits implicit version 0).
pub fn load_user_layer(path: &Path) -> Result<LayeredPlan, ConfigError> {
    let meta = std::fs::metadata(path).map_err(|e| ConfigError::InvalidInput {
        message: format!("cannot read config '{}': {e}", path.display()),
    })?;
    if meta.len() > MAX_CONFIG_FILE_BYTES as u64 {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "config '{}' exceeds {} bytes ({} bytes)",
                path.display(),
                MAX_CONFIG_FILE_BYTES,
                meta.len()
            ),
        });
    }
    let content = std::fs::read_to_string(path).map_err(|e| ConfigError::InvalidInput {
        message: format!("cannot read config '{}': {e}", path.display()),
    })?;
    if content.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "config '{}' exceeds {} bytes ({} bytes)",
                path.display(),
                MAX_CONFIG_FILE_BYTES,
                content.len()
            ),
        });
    }
    let source = ConfigSource::new(LayerKind::User, Some(path.display().to_string()));
    let plan = parse_toml_config(&content, &source)?;
    let migrated = crate::migration::migrate(plan)?;
    Ok(LayeredPlan::new(source, migrated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_source() -> ConfigSource {
        ConfigSource::new(LayerKind::User, Some("test.toml"))
    }

    #[test]
    fn xdg_wins_over_home_fallback() {
        let p = default_config_path_with_env(Some("/xdg"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/bitty/config.toml"));
    }

    #[test]
    fn blank_xdg_falls_back_to_home() {
        let p = default_config_path_with_env(Some("   "), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/bitty/config.toml"));
    }

    #[test]
    fn missing_roots_yield_none() {
        assert_eq!(default_config_path_with_env(None, None), None);
        assert_eq!(default_config_path_with_env(Some(""), Some("  ")), None);
    }

    #[test]
    fn explicit_path_wins_verbatim() {
        let p =
            resolve_config_path_with(Some("/tmp/custom.toml"), Some("/xdg"), Some("/h")).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/custom.toml"));
    }

    #[test]
    fn blank_explicit_falls_back_to_default() {
        let p = resolve_config_path_with(Some("  "), Some("/xdg"), Some("/h")).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/bitty/config.toml"));
    }

    #[test]
    fn flat_theme_alias_parses() {
        let plan = parse_toml_config("theme = \"dark\"\n", &test_source()).expect("flat theme");
        assert_eq!(plan.appearance.unwrap().theme.as_deref(), Some("dark"));
    }

    #[test]
    fn table_theme_parses_and_wins_over_alias() {
        let content = "theme = \"dark\"\n[appearance]\ntheme = \"bitty-dark\"\n";
        let plan = parse_toml_config(content, &test_source()).expect("table wins");
        assert_eq!(
            plan.appearance.unwrap().theme.as_deref(),
            Some("bitty-dark")
        );
    }

    #[test]
    fn empty_file_yields_empty_plan() {
        let plan = parse_toml_config("# only a comment\n\n", &test_source()).expect("empty ok");
        assert!(plan.is_empty());
    }

    #[test]
    fn full_scalar_tables_parse() {
        let content = "[font]\nfamily = \"JetBrains Mono\"\nsize = 13.0\n[window]\nopacity = 0.95\npadding = 8\n[terminal]\nscrollback = 10000\nshell = \"/bin/fish\"\n[appearance]\ntheme = \"dark\"\n";
        let plan = parse_toml_config(content, &test_source()).expect("full scalars");
        let font = plan.font.unwrap();
        assert_eq!(font.family, "JetBrains Mono");
        assert!((font.size - 13.0).abs() < f32::EPSILON);
        let window = plan.window.unwrap();
        assert!((window.opacity - 0.95).abs() < f32::EPSILON);
        assert_eq!(window.padding, 8);
        let term = plan.terminal.unwrap();
        assert_eq!(term.scrollback, 10000);
        assert_eq!(term.shell.as_deref(), Some("/bin/fish"));
    }

    #[test]
    fn unknown_table_fails_closed() {
        let err = parse_toml_config("[plugins]\nfoo = \"bar\"\n", &test_source()).unwrap_err();
        assert!(matches!(err, ConfigError::UndeclaredField { .. }));
    }

    #[test]
    fn unknown_key_fails_closed() {
        let err = parse_toml_config("bogus = \"x\"\n", &test_source()).unwrap_err();
        assert!(matches!(err, ConfigError::UndeclaredField { .. }));
    }

    #[test]
    fn malformed_line_fails_closed() {
        assert!(parse_toml_config("not a kv line\n", &test_source()).is_err());
        assert!(parse_toml_config("[unclosed\n", &test_source()).is_err());
        assert!(parse_toml_config("theme = dark\n", &test_source()).is_err());
    }

    #[test]
    fn duplicate_key_fails_closed() {
        let err = parse_toml_config("theme = \"a\"\ntheme = \"b\"\n", &test_source()).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn partial_font_table_fails_closed() {
        let err = parse_toml_config("[font]\nfamily = \"Mono\"\n", &test_source()).unwrap_err();
        assert!(err.to_string().contains("[font]"));
    }

    #[test]
    fn invalid_values_fail_closed_with_line() {
        let err = parse_toml_config("theme = \"   \"\n", &test_source()).unwrap_err();
        assert!(err.to_string().contains("line 1"));
        let err = parse_toml_config("[window]\nopacity = 2.0\npadding = 8\n", &test_source())
            .unwrap_err();
        assert!(err.to_string().contains("line 2"));
    }

    #[test]
    fn arrays_and_bools_rejected() {
        assert!(parse_toml_config("theme = [\"a\"]\n", &test_source()).is_err());
        assert!(
            parse_toml_config(
                "[terminal]\nscrollback = 10\nshell = true\n",
                &test_source()
            )
            .is_err()
        );
    }

    #[test]
    fn comment_inside_string_survives() {
        let plan = parse_toml_config("theme = \"a#b\"\n", &test_source()).expect("hash in string");
        assert_eq!(plan.appearance.unwrap().theme.as_deref(), Some("a#b"));
    }

    #[test]
    fn oversize_and_too_many_lines_fail_closed() {
        let big = "x".repeat(MAX_CONFIG_FILE_BYTES + 1);
        assert!(parse_toml_config(&big, &test_source()).is_err());
        let many = "# c\n".repeat(MAX_CONFIG_LINES + 1);
        assert!(parse_toml_config(&many, &test_source()).is_err());
    }

    #[test]
    fn cli_theme_layer_precedence() {
        // CLI wins over file wins over default (string-level).
        let src = test_source();
        let file_plan = parse_toml_config("theme = \"dark\"\n", &src).expect("file");
        let file_layer = LayeredPlan::new(src, file_plan);
        let merged =
            resolve_effective(Some(file_layer.clone()), Some("bitty-dark")).expect("cli wins");
        assert_eq!(
            merged.effective.appearance.theme.as_deref(),
            Some("bitty-dark")
        );
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::Cli
        );
        let merged_file = resolve_effective(Some(file_layer), None).expect("file wins");
        assert_eq!(
            merged_file.effective.appearance.theme.as_deref(),
            Some("dark")
        );
        assert_eq!(
            merged_file.source_of("appearance.theme").unwrap().layer,
            LayerKind::User
        );
        let merged_default = resolve_effective(None, None).expect("defaults");
        assert_eq!(merged_default.effective.appearance.theme, None);
        assert_eq!(
            merged_default.source_of("appearance.theme").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn blank_cli_theme_means_no_override() {
        assert!(cli_theme_layer(None).is_none());
        assert!(cli_theme_layer(Some("   ")).is_none());
        assert!(cli_theme_layer(Some("dark")).is_some());
    }

    #[test]
    fn load_user_layer_round_trip_via_tempfile() {
        let dir = std::env::temp_dir().join(format!("bitty-ctx0148-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(&path, "theme = \"dark\"\n").expect("write temp config");
        let layer = load_user_layer(&path).expect("load");
        assert_eq!(layer.source.layer, LayerKind::User);
        assert_eq!(
            layer.plan.appearance.unwrap().theme.as_deref(),
            Some("dark")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_fails_closed() {
        let path = Path::new("/nonexistent-bitty-ctx0148/config.toml");
        assert!(load_user_layer(path).is_err());
    }
}
