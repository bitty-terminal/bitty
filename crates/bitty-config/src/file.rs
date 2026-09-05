//! User config-file loading: `init.lua` under the XDG config root.
//!
//! # Status — Lua-only (DEC-0011, owner-mandated, supersedes DEC-0010)
//!
//! Bitty configuration is Lua, wezterm-style. The canonical file is
//! `$XDG_CONFIG_HOME/bitty/init.lua` (fallback `~/.config/bitty/`); a
//! sibling `config.lua` is accepted as a fallback alias when `init.lua` is
//! absent, and `--config PATH` overrides both. There is no TOML loader: the
//! earlier TOML-subset bootstrap was rejected in review.
//!
//! The chunk is evaluated in a fresh `bitty-lua` [`LuaVm`](bitty_lua::LuaVm)
//! with the **same** RC-1/RC-2 budgets as a plugin — config never executes
//! with more authority than a plugin, and the sandbox denies `io`/`os`/
//! `debug`/`package`/`require`/`load*` (see `bitty-lua` `config` module for
//! the exact inventory). The chunk must return a plain-data table
//! (wezterm-style `return {...}`) mapping 1:1 to [`ConfigPlan`] fields; the
//! result becomes the [`LayerKind::User`] layer through the UNCHANGED
//! validate/migrate/merge machinery.
//!
//! # Shape (wezterm-style return table)
//!
//! ```lua
//! -- $XDG_CONFIG_HOME/bitty/init.lua
//! return {
//!     theme = "dark", -- alias for appearance.theme
//!     appearance = { theme = "bitty-dark" }, -- wins over the alias
//!     font = { family = "JetBrainsMono Nerd Font", size = 12.0 },
//!     -- Optional breathing room (defaults 1.2 / 1.0 give effective 9x19
//!     -- from the legacy 8x16 base; see FontConfig docs):
//!     -- font = { family = "JetBrainsMono Nerd Font", size = 12.0,
//!     --          line_height = 1.2, letter_spacing = 1.0 },
//!     window = { opacity = 0.95, padding = 8 },
//!     terminal = { scrollback = 10000, shell = "/bin/fish", scroll_lines_per_notch = 3, scroll_pixels_per_notch = 16 },
//!     selection = { auto_copy = true }, -- false opts out of copy-on-select (CTX-0191, default true)
//!     keymaps = {
//!         { chord = "alt+h", action = "goto_split:left", context = "global" },
//!     },
//! }
//! ```
//!
//! - `[font]`-equivalent tables are atomic for `family`+`size`: `font` needs
//!   both (partial tables fail closed rather than silently filling defaults,
//!   which would corrupt attribution). `line_height`/`letter_spacing` are
//!   optional and default to [`crate::types::DEFAULT_LINE_HEIGHT`]/
//!   [`crate::types::DEFAULT_LETTER_SPACING`] when omitted, so existing
//!   `{ family, size }` tables keep working.
//! - `window` needs both `opacity` and `padding`, `terminal` needs
//!   `scrollback` (`shell`, `scroll_lines_per_notch`,
//!   `scroll_pixels_per_notch` optional, defaulting to
//!   [`TerminalConfig`](crate::types::TerminalConfig) defaults when absent).
//!   `selection` is fully optional (absent table/key means "this layer says
//!   nothing"); when present, `auto_copy` defaults to
//!   [`SelectionConfig`](crate::types::SelectionConfig) default `true` when
//!   omitted, so existing configs without `selection` keep working unchanged.
//!   Partial tables fail closed rather than
//!   silently filling defaults (which would corrupt attribution).
//! - `plugins`, `extends`, and profile names remain non-user layers and are
//!   rejected as undeclared here.
//! - Empty chunk / no return / `return nil` means "no user overrides".
//!
//! # Bounds and failure posture (threat T-01)
//!
//! - File bytes `<= MAX_CONFIG_FILE_BYTES` (64 KiB), lines
//!   `<= MAX_CONFIG_LINES`, each `<= MAX_LINE_BYTES`.
//! - VM budgets enforced per evaluation; budget exceed, syntax errors,
//!   runtime errors, wrong shapes, and undeclared keys all fail closed with
//!   [`ConfigError`] (never a panic, never a silent ignore).
//! - Error messages carry key paths and Lua line info only — never file
//!   contents beyond the offending line.
//!
//! # Precedence
//!
//! `CLI > file > defaults`: the file yields a [`LayerKind::User`] plan, an
//! explicit `--theme` CLI flag yields a [`LayerKind::Cli`] plan, and
//! [`merge_layers`](crate::merge_layers) sorts by precedence so the CLI wins.
//! Missing files are not errors for the default probe (bare `bitty` keeps
//! working); a missing **explicit** `--config` path is an error.

use std::path::{Path, PathBuf};

use bitty_lua::config::ConfigEval;

use crate::error::ConfigError;
use crate::migration::CURRENT_SCHEMA_VERSION;
use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
use crate::types::{
    AppearanceConfig, FontConfig, KeymapEntry, SelectionConfig, TerminalConfig, WindowConfig,
};

/// Config directory name under the XDG config root.
pub const CONFIG_DIR_NAME: &str = "bitty";

/// Canonical user config file name (matches the draft spec).
pub const INIT_LUA_NAME: &str = "init.lua";

/// Fallback alias accepted when `init.lua` is absent (wezterm-style name).
pub const FALLBACK_LUA_NAME: &str = "config.lua";

/// Maximum config file size in bytes (fail-closed).
pub const MAX_CONFIG_FILE_BYTES: usize = 64 * 1024;

/// Maximum config file lines (fail-closed).
pub const MAX_CONFIG_LINES: usize = 2048;

/// Maximum bytes per line (fail-closed).
pub const MAX_LINE_BYTES: usize = 4096;

/// Pure XDG path for one file name with injected environment values.
///
/// - `xdg_config_home`: value of `$XDG_CONFIG_HOME` (if set).
/// - `home`: value of `$HOME` (fallback root).
///
/// Returns `None` only when neither yields a usable root (no panic).
#[must_use]
pub fn config_file_path_with_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
    file_name: &str,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(Path::new(trimmed).join(CONFIG_DIR_NAME).join(file_name));
        }
    }
    if let Some(h) = home {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return Some(
                Path::new(trimmed)
                    .join(".config")
                    .join(CONFIG_DIR_NAME)
                    .join(file_name),
            );
        }
    }
    None
}

/// Pure canonical path (`init.lua`) with injected environment values.
#[must_use]
pub fn default_config_path_with_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    config_file_path_with_env(xdg_config_home, home, INIT_LUA_NAME)
}

/// Reads the live environment (`$XDG_CONFIG_HOME`, `$HOME`) for the
/// canonical config path. Thin wrapper so unit tests stay hermetic.
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    default_config_path_with_env(xdg.as_deref(), home.as_deref())
}

/// How [`probe_config_path`] resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedConfig {
    /// File to load (or to display/create when absent).
    pub path: PathBuf,
    /// True when the caller passed an explicit `--config` path.
    pub explicit: bool,
    /// True when the fallback `config.lua` name was selected (never set for
    /// explicit paths).
    pub fallback_name: bool,
}

/// Probe the user config file: explicit `--config` wins verbatim, else
/// `init.lua` when it exists, else `config.lua` when it exists, else the
/// canonical `init.lua` path (for `config path` display and `config edit`
/// creation). `exists` is injected so tests stay hermetic (no fs).
#[must_use]
pub fn probe_config_path_with_env(
    explicit: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<ProbedConfig> {
    if let Some(p) = explicit {
        if !p.trim().is_empty() {
            return Some(ProbedConfig {
                path: PathBuf::from(p),
                explicit: true,
                fallback_name: false,
            });
        }
    }
    let canonical = config_file_path_with_env(xdg_config_home, home, INIT_LUA_NAME)?;
    if exists(&canonical) {
        return Some(ProbedConfig {
            path: canonical,
            explicit: false,
            fallback_name: false,
        });
    }
    // MSRV 1.85: no let-chains; nest instead of `if let ... && ...`.
    if let Some(fallback) = config_file_path_with_env(xdg_config_home, home, FALLBACK_LUA_NAME) {
        if exists(&fallback) {
            return Some(ProbedConfig {
                path: fallback,
                explicit: false,
                fallback_name: true,
            });
        }
    }
    Some(ProbedConfig {
        path: canonical,
        explicit: false,
        fallback_name: false,
    })
}

/// Live-environment probe for the app startup path.
#[must_use]
pub fn probe_config_path(explicit: Option<&str>) -> Option<ProbedConfig> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    probe_config_path_with_env(explicit, xdg.as_deref(), home.as_deref(), &|p| p.exists())
}

/// Legacy resolution returning the path only: explicit `--config` wins
/// verbatim, else the default probe. Kept for callers that only need the
/// path; prefer [`probe_config_path`] when the fallback distinction matters.
#[must_use]
pub fn resolve_config_path_with(
    explicit: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    probe_config_path_with_env(explicit, xdg_config_home, home, &|p| p.exists()).map(|p| p.path)
}

/// Live-environment path resolution for callers that only need the path.
#[must_use]
pub fn resolve_config_path(explicit: Option<&str>) -> Option<PathBuf> {
    probe_config_path(explicit).map(|p| p.path)
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
// Lua loader (wezterm-style return table -> ConfigPlan)
// ---------------------------------------------------------------------------

/// Parses Lua config content into a [`ConfigPlan`] via the shared
/// `bitty-lua` sandbox (same budgets as plugins).
///
/// `source` is used for error attribution. The returned plan has
/// `schema_version: None` (implicit 0); callers run [`crate::migrate`]
/// before merging (see [`load_user_layer`]).
///
/// # Errors
///
/// Fail-closed [`ConfigError`]: oversize input, budget exceed, Lua
/// load/runtime errors, wrong shapes, partial tables, undeclared keys, and
/// typed validation failures. Messages carry key paths and Lua line info
/// only — never file contents.
pub fn parse_lua_config(content: &str, source: &ConfigSource) -> Result<ConfigPlan, ConfigError> {
    if content.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "config file exceeds {MAX_CONFIG_FILE_BYTES} bytes ({} bytes)",
                content.len()
            ),
        });
    }
    let mut lines = 0usize;
    for line in content.lines() {
        lines += 1;
        if lines > MAX_CONFIG_LINES {
            return Err(ConfigError::InvalidInput {
                message: format!(
                    "config file exceeds {MAX_CONFIG_LINES} lines ({} lines)",
                    lines + content.lines().count().saturating_sub(lines)
                ),
            });
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(ConfigError::InvalidInput {
                message: format!("config line exceeds {MAX_LINE_BYTES} bytes"),
            });
        }
    }

    let src_desc = source.describe();
    let mut vm = bitty_lua::LuaVm::new("bitty-config");
    let data = match vm
        .eval_config(content)
        .map_err(|e| ConfigError::InvalidInput {
            message: format!("config evaluation refused: {e}"),
        })? {
        bitty_lua::ConfigOutcome::Completed { data, .. } => *data,
        bitty_lua::ConfigOutcome::Suspended { reason, .. } => {
            return Err(ConfigError::InvalidInput {
                message: format!(
                    "config exceeded Lua budgets (suspended: {reason:?}); simplify init.lua"
                ),
            });
        }
        bitty_lua::ConfigOutcome::LuaError { message } => {
            return Err(ConfigError::validation("config", message));
        }
        bitty_lua::ConfigOutcome::ShapeError { message } => {
            return Err(shape_error_to_config_error(&message, &src_desc));
        }
    };

    if let Some(first) = data.undeclared.first() {
        return Err(ConfigError::UndeclaredField {
            field: first.clone(),
            source: Some(src_desc),
        });
    }

    // Table-wins for the theme alias.
    let theme = data.appearance_theme.or(data.theme);

    let appearance = theme.map(|t| AppearanceConfig { theme: Some(t) });
    let font = match data.font {
        None => None,
        Some(f) => match (f.family, f.size) {
            (Some(family), Some(size)) => {
                let line_height = f.line_height.unwrap_or(crate::types::DEFAULT_LINE_HEIGHT);
                let letter_spacing = f
                    .letter_spacing
                    .unwrap_or(crate::types::DEFAULT_LETTER_SPACING);
                let cfg = FontConfig {
                    family,
                    size: size as f32,
                    line_height,
                    letter_spacing,
                };
                // Fail closed on out-of-range spacing (same as typed validation).
                cfg.validate().map_err(|e| {
                    ConfigError::validation(e.field().unwrap_or("font"), e.to_string())
                })?;
                Some(cfg)
            }
            _ => {
                return Err(ConfigError::validation(
                    "font",
                    "table 'font' requires both 'family' (string) and 'size' (number)",
                ));
            }
        },
    };
    let window = match data.window {
        None => None,
        Some(w) => match (w.opacity, w.padding) {
            (Some(opacity), Some(padding)) => {
                if !(0..=64).contains(&padding) {
                    return Err(ConfigError::validation(
                        "window.padding",
                        format!("must be within [0, 64] (found {padding})"),
                    ));
                }
                Some(WindowConfig {
                    opacity: opacity as f32,
                    padding: padding as u32,
                })
            }
            _ => {
                return Err(ConfigError::validation(
                    "window",
                    "table 'window' requires both 'opacity' (number) and 'padding' (integer)",
                ));
            }
        },
    };
    let terminal = match data.terminal {
        None => None,
        Some(t) => match t.scrollback {
            Some(scrollback) => {
                if !(0..=100_000).contains(&scrollback) {
                    return Err(ConfigError::validation(
                        "terminal.scrollback",
                        format!("must be within [0, 100000] (found {scrollback})"),
                    ));
                }
                // CTX-0185: scroll speed keys are optional extras (like
                // `shell`); absent means "this layer says nothing" so merge
                // keeps the lower-precedence value. Present values are
                // range-checked here (fail-closed) and again by
                // `TerminalConfig::validate` via `plan.validate()`.
                let defaults = TerminalConfig::default();
                let scroll_lines_per_notch = match t.scroll_lines_per_notch {
                    None => defaults.scroll_lines_per_notch,
                    Some(v) => {
                        if !(1..=crate::types::MAX_SCROLL_LINES_PER_NOTCH as i64).contains(&v) {
                            return Err(ConfigError::validation(
                                "terminal.scroll_lines_per_notch",
                                format!(
                                    "must be within [1, {}] (found {v})",
                                    crate::types::MAX_SCROLL_LINES_PER_NOTCH
                                ),
                            ));
                        }
                        v as u32
                    }
                };
                let scroll_pixels_per_notch = match t.scroll_pixels_per_notch {
                    None => defaults.scroll_pixels_per_notch,
                    Some(v) => {
                        if !(1..=crate::types::MAX_SCROLL_PIXELS_PER_NOTCH as i64).contains(&v) {
                            return Err(ConfigError::validation(
                                "terminal.scroll_pixels_per_notch",
                                format!(
                                    "must be within [1, {}] (found {v})",
                                    crate::types::MAX_SCROLL_PIXELS_PER_NOTCH
                                ),
                            ));
                        }
                        v as u32
                    }
                };
                Some(TerminalConfig {
                    scrollback: scrollback as u32,
                    shell: t.shell,
                    scroll_lines_per_notch,
                    scroll_pixels_per_notch,
                })
            }
            None => {
                return Err(ConfigError::validation(
                    "terminal.scrollback",
                    "table 'terminal' requires 'scrollback' (integer)",
                ));
            }
        },
    };
    let keymaps = match data.keymaps {
        None => None,
        Some(list) => {
            let mut out = Vec::with_capacity(list.len());
            for k in list {
                out.push(KeymapEntry {
                    chord: k.chord,
                    action: k.action,
                    context: k.context,
                });
            }
            Some(out)
        }
    };
    // CTX-0191: `selection` is fully optional (absent table means "this layer
    // says nothing" so merge keeps the lower-precedence value). When the
    // table is present but `auto_copy` is omitted, default to `true` (like
    // the CTX-0185 scroll extras inside `terminal`): existing configs without
    // `selection` keep working unchanged, and `selection = { auto_copy =
    // false }` is the explicit opt-out. Wrong types already failed closed as
    // `ShapeError` in `bitty-lua` (never coerced, never echoed).
    let selection = data.selection.map(|s| SelectionConfig {
        auto_copy: s.auto_copy.unwrap_or(SelectionConfig::default().auto_copy),
    });

    let plan = ConfigPlan {
        schema_version: None,
        font,
        window,
        terminal,
        selection,
        appearance,
        keymaps,
        plugins: None,
        profile_name: None,
        extends: None,
        undeclared_fields: Vec::new(),
    };
    plan.validate()?;
    Ok(plan)
}

/// Maps a shape-error message (already `path: reason` or `undeclared field
/// 'path'`) to the matching [`ConfigError`] without echoing values.
fn shape_error_to_config_error(message: &str, src_desc: &str) -> ConfigError {
    if let Some(field) = message
        .strip_prefix("undeclared field '")
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return ConfigError::UndeclaredField {
            field: field.to_string(),
            source: Some(src_desc.to_string()),
        };
    }
    match message.split_once(": ") {
        Some((field, _)) if !field.is_empty() && !field.contains(' ') && !field.contains('\n') => {
            ConfigError::validation(field, message)
        }
        _ => ConfigError::validation("config", message),
    }
}

/// Loads, evaluates, validates, and migrates a user config file into a
/// [`LayerKind::User`] plan.
///
/// The file **must** exist; missing files are the caller's decision (default
/// probe skips, explicit `--config` fails). Oversize/unreadable/budget- or
/// shape-invalid files fail closed with [`ConfigError`] (no panic).
///
/// # Errors
///
/// - [`ConfigError::InvalidInput`] for oversize, unreadable, or
///   budget-exceeding files.
/// - Parser/validation errors from [`parse_lua_config`].
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
    let plan = parse_lua_config(&content, &source)?;
    let migrated = crate::migration::migrate(plan)?;
    Ok(LayeredPlan::new(source, migrated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_source() -> ConfigSource {
        ConfigSource::new(LayerKind::User, Some("init.lua"))
    }

    #[test]
    fn canonical_path_prefers_xdg_init_lua() {
        let p = default_config_path_with_env(Some("/xdg"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/bitty/init.lua"));
    }

    #[test]
    fn blank_xdg_falls_back_to_home_init_lua() {
        let p = default_config_path_with_env(Some("   "), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/bitty/init.lua"));
    }

    #[test]
    fn missing_roots_yield_none() {
        assert_eq!(default_config_path_with_env(None, None), None);
        assert_eq!(default_config_path_with_env(Some(""), Some("  ")), None);
    }

    #[test]
    fn probe_prefers_init_lua_then_config_lua() {
        let exists_init = |p: &Path| p.ends_with("init.lua");
        let probed =
            probe_config_path_with_env(None, Some("/xdg"), Some("/h"), &exists_init).unwrap();
        assert_eq!(probed.path, PathBuf::from("/xdg/bitty/init.lua"));
        assert!(!probed.explicit && !probed.fallback_name);

        let exists_fallback = |p: &Path| p.ends_with("config.lua");
        let probed =
            probe_config_path_with_env(None, Some("/xdg"), Some("/h"), &exists_fallback).unwrap();
        assert_eq!(probed.path, PathBuf::from("/xdg/bitty/config.lua"));
        assert!(probed.fallback_name);

        let exists_none = |_: &Path| false;
        let probed =
            probe_config_path_with_env(None, Some("/xdg"), Some("/h"), &exists_none).unwrap();
        assert_eq!(probed.path, PathBuf::from("/xdg/bitty/init.lua"));
        assert!(!probed.fallback_name);
    }

    #[test]
    fn probe_explicit_wins_verbatim() {
        let p = probe_config_path_with_env(
            Some("/tmp/custom.lua"),
            Some("/xdg"),
            Some("/h"),
            &|_: &Path| true,
        )
        .unwrap();
        assert_eq!(p.path, PathBuf::from("/tmp/custom.lua"));
        assert!(p.explicit && !p.fallback_name);
    }

    #[test]
    fn blank_explicit_falls_back_to_probe() {
        let p = probe_config_path_with_env(Some("  "), Some("/xdg"), Some("/h"), &|_: &Path| false)
            .unwrap();
        assert_eq!(p.path, PathBuf::from("/xdg/bitty/init.lua"));
        assert!(!p.explicit);
    }

    #[test]
    fn lua_theme_alias_parses() {
        let plan = parse_lua_config(r#"return { theme = "dark" }"#, &test_source()).expect("theme");
        assert_eq!(plan.appearance.unwrap().theme.as_deref(), Some("dark"));
    }

    #[test]
    fn lua_table_theme_wins_over_alias() {
        let content = r#"return { theme = "dark", appearance = { theme = "bitty-dark" } }"#;
        let plan = parse_lua_config(content, &test_source()).expect("table wins");
        assert_eq!(
            plan.appearance.unwrap().theme.as_deref(),
            Some("bitty-dark")
        );
    }

    #[test]
    fn lua_empty_chunk_yields_empty_plan() {
        for content in ["", "-- comment only\n", "return nil"] {
            let plan = parse_lua_config(content, &test_source()).expect("empty ok");
            assert!(plan.is_empty(), "content: {content:?}");
        }
    }

    #[test]
    fn lua_full_scalar_tables_and_keymaps_parse() {
        let content = r#"return {
            font = { family = "JetBrains Mono", size = 13.0 },
            window = { opacity = 0.95, padding = 8 },
            terminal = { scrollback = 10000, shell = "/bin/fish" },
            appearance = { theme = "dark" },
            keymaps = {
                { chord = "alt+h", action = "goto_split:left", context = "global" },
            },
        }"#;
        let plan = parse_lua_config(content, &test_source()).expect("full");
        let font = plan.font.unwrap();
        assert_eq!(font.family, "JetBrains Mono");
        assert!((font.size - 13.0).abs() < f32::EPSILON);
        let window = plan.window.unwrap();
        assert!((window.opacity - 0.95).abs() < f32::EPSILON);
        assert_eq!(window.padding, 8);
        let term = plan.terminal.unwrap();
        assert_eq!(term.scrollback, 10000);
        assert_eq!(term.shell.as_deref(), Some("/bin/fish"));
        // CTX-0185: scroll keys absent -> plan carries defaults (atomic
        // terminal table, like `shell = None`).
        assert_eq!(
            term.scroll_lines_per_notch,
            crate::types::DEFAULT_SCROLL_LINES_PER_NOTCH
        );
        assert_eq!(
            term.scroll_pixels_per_notch,
            crate::types::DEFAULT_SCROLL_PIXELS_PER_NOTCH
        );
        let maps = plan.keymaps.unwrap();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].chord, "alt+h");
    }

    #[test]
    fn lua_terminal_scroll_speed_parses_and_validates() {
        // CTX-0185: explicit scroll keys parse; out-of-range fails closed
        // with the field path (never the value beyond the offending line).
        let plan = parse_lua_config(
            r#"return { terminal = { scrollback = 10000, scroll_lines_per_notch = 5, scroll_pixels_per_notch = 24 } }"#,
            &test_source(),
        )
        .expect("scroll keys parse");
        let term = plan.terminal.unwrap();
        assert_eq!(term.scroll_lines_per_notch, 5);
        assert_eq!(term.scroll_pixels_per_notch, 24);
        for bad in [
            r#"return { terminal = { scrollback = 10000, scroll_lines_per_notch = 0 } }"#,
            r#"return { terminal = { scrollback = 10000, scroll_lines_per_notch = 33 } }"#,
            r#"return { terminal = { scrollback = 10000, scroll_pixels_per_notch = 0 } }"#,
            r#"return { terminal = { scrollback = 10000, scroll_pixels_per_notch = 257 } }"#,
        ] {
            let err = parse_lua_config(bad, &test_source()).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("scroll_lines_per_notch") || msg.contains("scroll_pixels_per_notch"),
                "must name the field: {msg}"
            );
        }
    }

    #[test]
    fn lua_selection_auto_copy_parses_and_validates() {
        // CTX-0191: explicit bool parses; absent table means "says nothing"
        // (plan.selection None so merge keeps lower); present-but-empty
        // defaults to true; wrong types fail closed naming the field.
        let plan = parse_lua_config(
            r#"return { selection = { auto_copy = false } }"#,
            &test_source(),
        )
        .expect("opt-out parses");
        assert!(!plan.selection.unwrap().auto_copy);
        let plan = parse_lua_config(
            r#"return { selection = { auto_copy = true } }"#,
            &test_source(),
        )
        .expect("opt-in parses");
        assert!(plan.selection.unwrap().auto_copy);
        let plan = parse_lua_config(
            r#"return { terminal = { scrollback = 10000 } }"#,
            &test_source(),
        )
        .expect("no selection table");
        assert!(plan.selection.is_none());
        let plan = parse_lua_config(r#"return { selection = {} }"#, &test_source())
            .expect("empty selection defaults");
        assert!(plan.selection.unwrap().auto_copy);
        for bad in [
            r#"return { selection = { auto_copy = 1 } }"#,
            r#"return { selection = { auto_copy = "false" } }"#,
            r#"return { selection = "false" }"#,
        ] {
            let err = parse_lua_config(bad, &test_source()).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("selection.auto_copy") || msg.contains("selection"),
                "must name the field: {msg}"
            );
            assert!(!msg.contains("\"false\""), "must not echo value: {msg}");
        }
    }

    #[test]
    fn lua_undeclared_key_fails_closed() {
        let err = parse_lua_config(r#"return { plugins = {} }"#, &test_source()).unwrap_err();
        assert!(matches!(err, ConfigError::UndeclaredField { .. }));
    }

    #[test]
    fn lua_syntax_error_fails_closed() {
        assert!(parse_lua_config("return { theme = }", &test_source()).is_err());
    }

    #[test]
    fn lua_wrong_types_fail_closed_without_values() {
        let err = parse_lua_config(r#"return { theme = 42 }"#, &test_source()).unwrap_err();
        assert!(!err.to_string().contains("42") || err.to_string().contains("theme"));
        let err = parse_lua_config(
            "return { window = { opacity = 2.0, padding = 8 } }",
            &test_source(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("window.opacity"));
    }

    #[test]
    fn lua_partial_tables_fail_closed() {
        let err = parse_lua_config(r#"return { font = { family = "Mono" } }"#, &test_source())
            .unwrap_err();
        assert!(err.to_string().contains("font"));
    }

    #[test]
    fn lua_font_spacing_optional_with_defaults() {
        use crate::types::{DEFAULT_LETTER_SPACING, DEFAULT_LINE_HEIGHT};
        let plan = parse_lua_config(
            r#"return { font = { family = "Mono", size = 12 } }"#,
            &test_source(),
        )
        .expect("legacy table works");
        let font = plan.font.unwrap();
        assert!((font.line_height - DEFAULT_LINE_HEIGHT).abs() < f32::EPSILON);
        assert!((font.letter_spacing - DEFAULT_LETTER_SPACING).abs() < f32::EPSILON);
        let plan = parse_lua_config(
            r#"return { font = { family = "Mono", size = 12, line_height = 1.0, letter_spacing = 0 } }"#,
            &test_source(),
        )
        .expect("spacing parses");
        let font = plan.font.unwrap();
        assert!((font.line_height - 1.0).abs() < f32::EPSILON);
        assert!((font.letter_spacing - 0.0).abs() < f32::EPSILON);
        // Out-of-range spacing fails closed.
        let err = parse_lua_config(
            r#"return { font = { family = "Mono", size = 12, line_height = 5.0 } }"#,
            &test_source(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("font"));
    }

    #[test]
    fn lua_denied_globals_fail_closed() {
        let err = parse_lua_config(r#"return { theme = os.getenv("HOME") }"#, &test_source())
            .unwrap_err();
        assert!(!err.to_string().contains("HOME="));
    }

    #[test]
    fn lua_infinite_loop_fails_closed_via_budgets() {
        let err = parse_lua_config("while true do end", &test_source()).unwrap_err();
        assert!(err.to_string().contains("budgets"));
    }

    #[test]
    fn lua_non_table_return_fails_closed() {
        assert!(parse_lua_config("return 42", &test_source()).is_err());
    }

    #[test]
    fn lua_oversize_fails_closed() {
        let big = "x".repeat(MAX_CONFIG_FILE_BYTES + 1);
        assert!(parse_lua_config(&big, &test_source()).is_err());
    }

    #[test]
    fn cli_theme_layer_precedence() {
        // CLI wins over file wins over default (string-level).
        let src = test_source();
        let file_plan = parse_lua_config(r#"return { theme = "dark" }"#, &src).expect("file");
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
    fn lua_keymaps_merge_by_chord() {
        let src = test_source();
        let plan = parse_lua_config(
            r#"return { keymaps = { { chord = "ctrl+p", action = "focus_next", context = "global" } } }"#,
            &src,
        )
        .expect("keymaps");
        let layer = LayeredPlan::new(src, plan);
        let merged = resolve_effective(Some(layer), None).expect("merge");
        assert_eq!(merged.effective.keymaps.len(), 1);
        assert_eq!(merged.effective.keymaps[0].action, "focus_next");
    }

    #[test]
    fn load_user_layer_round_trip_via_tempfile() {
        let dir = std::env::temp_dir().join(format!("bitty-ctx0148-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("init.lua");
        std::fs::write(&path, r#"return { theme = "dark" }"#).expect("write temp config");
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
        let path = Path::new("/nonexistent-bitty-ctx0148/init.lua");
        assert!(load_user_layer(path).is_err());
    }
}
