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
//!     layout = { gaps_in = 1, gaps_out = 2 }, -- Hyprland-like panel gaps in cells, 0 = edge-to-edge (CTX-0177, default 0/0)
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
//!   `layout` follows the same fully-optional pattern: absent table/key means
//!   "this layer says nothing"; when the table is present, omitted keys
//!   default to [`LayoutConfig`](crate::types::LayoutConfig) defaults (`0`,
//!   edge-to-edge), and out-of-range values fail closed with the field path.
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
//! `CLI > file > profile > defaults`: the file yields a [`LayerKind::User`]
//! plan, the named profile (`--profile` / `BITTY_PROFILE`, CTX-0169) yields a
//! [`LayerKind::Profile`] plan UNDER the file (`init.lua` still wins over
//! it), explicit CLI appearance flags (`--theme`, `--font-family`,
//! `--font-size`, `--opacity`, CTX-0180) yield one [`LayerKind::Cli`] plan,
//! and [`merge_layers`](crate::merge_layers) sorts by precedence so the CLI wins.
//! `BITTY_CONFIG` (path) and `BITTY_PROFILE` (name) env overrides sit between
//! CLI flags and files (CLI wins over env). Missing files are not errors for
//! the default probe (bare `bitty` keeps working); a missing **explicit**
//! `--config`/`BITTY_CONFIG` path or a requested-but-missing profile is an
//! error (fail-closed, no fallback guessing).

use std::path::{Path, PathBuf};

use bitty_lua::config::ConfigEval;

use crate::error::ConfigError;
use crate::migration::CURRENT_SCHEMA_VERSION;
use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
use crate::types::{
    AppearanceConfig, FontConfig, KeymapEntry, LayoutConfig, MAX_FONT_FAMILY_LEN, SelectionConfig,
    TerminalConfig, WindowConfig,
};

/// Config directory name under the XDG config root.
pub const CONFIG_DIR_NAME: &str = "bitty";

/// Canonical user config file name (matches the draft spec).
pub const INIT_LUA_NAME: &str = "init.lua";

/// Fallback alias accepted when `init.lua` is absent (wezterm-style name).
pub const FALLBACK_LUA_NAME: &str = "config.lua";

/// Directory under the config root holding named profiles
/// (`$XDG_CONFIG_HOME/bitty/profiles/<name>.lua`, CTX-0169 / #271).
pub const PROFILES_DIR_NAME: &str = "profiles";

/// Maximum profile name length (fail-closed; keeps paths bounded).
pub const MAX_PROFILE_NAME_LEN: usize = 64;

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
///
/// Theme-only helper (CTX-0169 contract, unchanged by CTX-0180): full CLI
/// overrides (`--theme` + `--font-family`/`--font-size`/`--opacity`) go
/// through [`CliOverrides::to_layer_with_base`] via [`resolve_effective_full`].
#[must_use]
pub fn cli_theme_layer(theme: Option<&str>) -> Option<LayeredPlan> {
    let overrides = CliOverrides {
        theme: theme
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        ..Default::default()
    };
    overrides.to_layer()
}

/// CLI overrides collected from flags (CTX-0169; appearance extended by
/// CTX-0180 / #279).
///
/// `theme` comes from `--theme`; `font_family` / `font_size` / `opacity` come
/// from `--font-family` / `--font-size` / `--opacity`. The numeric raws stay
/// strings so invalid values fail closed at merge time (never silently
/// dropped, never warn-ignored). Pure data; no env/filesystem access.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliOverrides {
    /// CLI theme override (`--theme`).
    pub theme: Option<String>,
    /// CLI font-family override (`--font-family`); blank means absent.
    pub font_family: Option<String>,
    /// CLI font-size override (`--font-size`); raw text, parsed at merge
    /// time (fail-closed, range `(0, 128]`).
    pub font_size: Option<String>,
    /// CLI window-opacity override (`--opacity`); raw text, parsed at merge
    /// time (fail-closed, range `[0.0, 1.0]`).
    pub opacity: Option<String>,
}

/// Trims a CLI raw: `None`/empty/whitespace means "no override".
fn cli_present(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Parses one CLI numeric raw (already trimmed, non-empty) into a finite
/// `f32`.
///
/// # Errors
///
/// [`ConfigError::validation`] at `field` when the raw is unparseable or
/// non-finite. The message names the field and quotes the trimmed raw (the
/// caller's own argv, never file bytes).
fn parse_cli_float(field: &str, raw: &str) -> Result<f32, ConfigError> {
    match raw.parse::<f32>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(ConfigError::validation(
            field,
            format!("CLI value {raw:?} is not a finite number"),
        )),
    }
}

impl CliOverrides {
    /// True when no CLI field is set (no `Cli` layer is built).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        cli_present(self.theme.as_deref()).is_none()
            && cli_present(self.font_family.as_deref()).is_none()
            && cli_present(self.font_size.as_deref()).is_none()
            && cli_present(self.opacity.as_deref()).is_none()
    }

    /// True when this overrides `field` (a dotted merge field path).
    ///
    /// Only the four CLI-owned scalars report true: `appearance.theme`,
    /// `font.family`, `font.size`, `window.opacity`. Used after a successful
    /// [`CliOverrides::to_layer_with_base`] to restore base attribution for
    /// inherited siblings.
    #[must_use]
    pub fn overrides_field(&self, field: &str) -> bool {
        match field {
            "appearance.theme" => cli_present(self.theme.as_deref()).is_some(),
            "font.family" => cli_present(self.font_family.as_deref()).is_some(),
            "font.size" => cli_present(self.font_size.as_deref()).is_some(),
            "window.opacity" => cli_present(self.opacity.as_deref()).is_some(),
            _ => false,
        }
    }

    /// Validates the CTX-0180 appearance raws without needing layers.
    ///
    /// Checks `--font-family` length and parses `--font-size`/`--opacity`
    /// through the same typed validators the merge uses, so range violations
    /// fail closed with the dotted field path. `theme` is intentionally NOT
    /// checked here (CTX-0169 merge-time behavior stays untouched).
    ///
    /// # Errors
    ///
    /// [`ConfigError::validation`] at `font.family` / `font.size` /
    /// `window.opacity` for overlong/unparseable/out-of-range values.
    pub fn validate_appearance_overrides(&self) -> Result<(), ConfigError> {
        if let Some(family) = cli_present(self.font_family.as_deref()) {
            if family.len() > MAX_FONT_FAMILY_LEN {
                return Err(ConfigError::validation(
                    "font.family",
                    format!("CLI value must be <= {MAX_FONT_FAMILY_LEN} bytes"),
                ));
            }
        }
        if let Some(raw) = cli_present(self.font_size.as_deref()) {
            let size = parse_cli_float("font.size", &raw)?;
            FontConfig {
                family: String::from("cli"),
                size,
                ..Default::default()
            }
            .validate()?;
        }
        if let Some(raw) = cli_present(self.opacity.as_deref()) {
            let opacity = parse_cli_float("window.opacity", &raw)?;
            WindowConfig {
                opacity,
                ..Default::default()
            }
            .validate()?;
        }
        Ok(())
    }

    /// Builds the single [`LayerKind::Cli`] plan for the `--theme` field only.
    ///
    /// Returns `None` when empty (no override). Validation happens at merge
    /// time (overlong names fail closed there). Kept theme-only so CTX-0169
    /// callers and [`cli_theme_layer`] behave exactly as before; full CLI
    /// overrides go through [`CliOverrides::to_layer_with_base`].
    #[must_use]
    pub fn to_layer(&self) -> Option<LayeredPlan> {
        let trimmed = self.theme.as_deref()?.trim();
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

    /// Builds the single [`LayerKind::Cli`] plan for every present CLI field
    /// (CTX-0180 / #279).
    ///
    /// Font/window siblings the CLI did not override are inherited from
    /// `base` (the already-merged lower layers), so one flag never clobbers
    /// its siblings: `--font-family X` keeps the file's size, spacing, and
    /// padding. Tables are included only when at least one of their fields
    /// is overridden. The source label names the active flags (`cli:--theme`
    /// is preserved verbatim for theme-only overrides).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] from [`CliOverrides::validate_appearance_overrides`]
    /// (fail-closed raws) or from the final plan validation (fail-closed
    /// theme length, same as [`CliOverrides::to_layer`] at merge).
    pub fn to_layer_with_base(
        &self,
        base: &crate::types::EffectiveConfig,
    ) -> Result<Option<LayeredPlan>, ConfigError> {
        self.validate_appearance_overrides()?;
        let theme = cli_present(self.theme.as_deref());
        let family = cli_present(self.font_family.as_deref());
        let size = match cli_present(self.font_size.as_deref()) {
            Some(raw) => Some(parse_cli_float("font.size", &raw)?),
            None => None,
        };
        let opacity = match cli_present(self.opacity.as_deref()) {
            Some(raw) => Some(parse_cli_float("window.opacity", &raw)?),
            None => None,
        };
        if theme.is_none() && family.is_none() && size.is_none() && opacity.is_none() {
            return Ok(None);
        }
        let font = match (family, size) {
            (None, None) => None,
            (f, s) => {
                let cfg = FontConfig {
                    family: f.unwrap_or_else(|| base.font.family.clone()),
                    size: s.unwrap_or(base.font.size),
                    line_height: base.font.line_height,
                    letter_spacing: base.font.letter_spacing,
                };
                cfg.validate()?;
                Some(cfg)
            }
        };
        let window = match opacity {
            None => None,
            Some(o) => {
                let cfg = WindowConfig {
                    opacity: o,
                    padding: base.window.padding,
                };
                cfg.validate()?;
                Some(cfg)
            }
        };
        let appearance = theme.as_deref().map(|t| AppearanceConfig {
            theme: Some(t.to_string()),
        });
        let plan = ConfigPlan {
            appearance,
            font,
            window,
            schema_version: Some(CURRENT_SCHEMA_VERSION),
            ..Default::default()
        };
        plan.validate()?;
        let mut flags = Vec::new();
        if theme.is_some() {
            flags.push("--theme");
        }
        if self.overrides_field("font.family") {
            flags.push("--font-family");
        }
        if self.overrides_field("font.size") {
            flags.push("--font-size");
        }
        if self.overrides_field("window.opacity") {
            flags.push("--opacity");
        }
        let label = if flags.is_empty() {
            String::from("cli")
        } else {
            format!("cli:{}", flags.join(","))
        };
        Ok(Some(LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some(label)),
            plan,
        )))
    }
}

/// Resolves the explicit config-file override: CLI `--config` wins over
/// `BITTY_CONFIG` env (CTX-0169 / #271). Pure over injected values so tests
/// stay hermetic: trims, treats `None`/empty/whitespace as absent.
///
/// The caller probes the returned path verbatim (explicit, fail-closed when
/// missing); `None` means run the default XDG probe.
#[must_use]
pub fn resolve_config_explicit(cli: Option<&str>, env: Option<&str>) -> Option<String> {
    for raw in [cli, env].into_iter().flatten() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Resolves the requested profile name: CLI `--profile` wins over
/// `BITTY_PROFILE` env (CTX-0169 / #271). Pure over injected values:
/// trims, treats `None`/empty/whitespace as absent (no profile).
///
/// Validation (allowed charset, fail-closed missing file) happens in
/// [`validate_profile_name`] / [`profile_file_path_with_env`].
#[must_use]
pub fn resolve_profile_request(cli: Option<&str>, env: Option<&str>) -> Option<String> {
    for raw in [cli, env].into_iter().flatten() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Validates a `--profile` / `BITTY_PROFILE` name (CTX-0169, fail-closed).
///
/// Allowed: `1..=MAX_PROFILE_NAME_LEN` chars of `[A-Za-z0-9_-]` only.
/// Rejects empty/whitespace, path separators, dots, and traversal (`..`,
/// `/`, `\`) so the name always maps to exactly one file
/// `profiles/<name>.lua` under the config root. Returns the trimmed name.
///
/// # Errors
///
/// [`ConfigError::InvalidInput`] naming the problem without echoing more
/// than the offending name.
pub fn validate_profile_name(name: &str) -> Result<String, ConfigError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidInput {
            message: "profile name must be non-empty".to_string(),
        });
    }
    if trimmed.len() > MAX_PROFILE_NAME_LEN {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "profile name exceeds {MAX_PROFILE_NAME_LEN} chars ({} chars)",
                trimmed.len()
            ),
        });
    }
    let ok = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "invalid profile name '{trimmed}': use [A-Za-z0-9_-] only (no paths, no extensions)"
            ),
        });
    }
    Ok(trimmed.to_string())
}

/// Config root directory (`$XDG_CONFIG_HOME/bitty` or `~/.config/bitty`)
/// with injected environment values. Returns `None` only when neither
/// yields a usable root (no panic).
#[must_use]
pub fn config_dir_with_env(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(Path::new(trimmed).join(CONFIG_DIR_NAME));
        }
    }
    if let Some(h) = home {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return Some(Path::new(trimmed).join(".config").join(CONFIG_DIR_NAME));
        }
    }
    None
}

/// Pure profile path for `name` with injected environment values
/// (CTX-0169): `<config-dir>/profiles/<name>.lua`.
///
/// `XDG_CONFIG_DIRS` (system-wide) is never consulted: profiles are
/// user-level (`$XDG_CONFIG_HOME`, fallback `~/.config`).
///
/// # Errors
///
/// - Invalid name from [`validate_profile_name`] (fail-closed).
/// - No usable config root (neither `$XDG_CONFIG_HOME` nor `$HOME` set).
pub fn profile_file_path_with_env(
    name: &str,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    let valid = validate_profile_name(name)?;
    let dir =
        config_dir_with_env(xdg_config_home, home).ok_or_else(|| ConfigError::InvalidInput {
            message: "no config root ($XDG_CONFIG_HOME or $HOME unset)".to_string(),
        })?;
    Ok(dir.join(PROFILES_DIR_NAME).join(format!("{valid}.lua")))
}

/// Reads the live environment (`$XDG_CONFIG_HOME`, `$HOME`) for the profile
/// path. Thin wrapper so unit tests stay hermetic (see
/// [`profile_file_path_with_env`]).
///
/// # Errors
///
/// Same as [`profile_file_path_with_env`].
pub fn profile_file_path(name: &str) -> Result<PathBuf, ConfigError> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    profile_file_path_with_env(name, xdg.as_deref(), home.as_deref())
}

/// Merges an optional file (User) layer with an optional `--theme` CLI layer
/// into the effective config. Pure and headless: precedence is `CLI > file >
/// defaults` via [`crate::merge_layers`] (empty layers merge to core
/// defaults with attribution).
///
/// Kept for back-compat (CTX-0148 callers + CTX-0180 rebase): delegates to
/// [`resolve_effective_full`] with no profile layer. New code should call
/// [`resolve_effective_full`] directly.
///
/// # Errors
///
/// Returns the first [`ConfigError`] from validation or merge (including
/// policy violations, which remain hard errors here).
pub fn resolve_effective(
    file: Option<LayeredPlan>,
    cli_theme: Option<&str>,
) -> Result<crate::merge::MergedConfig, ConfigError> {
    let cli = CliOverrides {
        theme: cli_present(cli_theme),
        ..Default::default()
    };
    resolve_effective_full(file, None, &cli)
}

/// Merges optional profile + file (User) layers with CLI overrides into the
/// effective config (CTX-0169; appearance overrides extended by CTX-0180).
///
/// Precedence (per `lua-and-xdg.md` §Layers and #271): `CLI > user file >
/// profile > defaults` via [`crate::merge_layers`] — the merge sorts by
/// [`LayerKind::precedence`] (`Cli 70 > User 50 > Profile 40`), so input
/// order never matters. The profile composes the base; `init.lua` (or the
/// explicit `--config`/`BITTY_CONFIG` file) still wins over it; CLI
/// appearance flags (`--theme`, `--font-family`, `--font-size`, `--opacity`)
/// win over both, each flag overriding only its own field (siblings inherit
/// the merged lower layers, never defaults). Empty layers merge to core
/// defaults with attribution.
///
/// # Errors
///
/// Returns the first [`ConfigError`] from validation or merge (including
/// policy violations, which remain hard errors here). Invalid CLI raws fail
/// closed with their dotted field path (`font.family` / `font.size` /
/// `window.opacity`).
pub fn resolve_effective_full(
    file: Option<LayeredPlan>,
    profile: Option<LayeredPlan>,
    cli: &CliOverrides,
) -> Result<crate::merge::MergedConfig, ConfigError> {
    let mut lower = Vec::new();
    if let Some(p) = profile {
        debug_assert_eq!(
            p.source.layer,
            LayerKind::Profile,
            "profile layer must use LayerKind::Profile"
        );
        lower.push(p);
    }
    if let Some(f) = file {
        lower.push(f);
    }
    // Fast path: no CLI override stays a single merge (pre-0180 behavior).
    if cli.is_empty() {
        return crate::merge::merge_layers(lower);
    }
    // Base merge first so profile/file failures surface before CLI enters.
    let base = crate::merge::merge_layers(lower.clone())?;
    // Base-aware CLI layer: validates CLI raws fail-closed and inherits
    // non-overridden font/window siblings from the base.
    let cli_layer = cli.to_layer_with_base(&base.effective)?;
    let mut layers = lower;
    if let Some(c) = cli_layer {
        layers.push(c);
    }
    let mut merged = crate::merge::merge_layers(layers)?;
    // The CLI tables are atomic at merge, so inherited siblings were
    // re-attributed to `Cli` with identical values: restore the base source
    // per non-overridden field and prune the spurious CLI conflicts so
    // `config check` keeps exact per-field sources.
    for field in [
        "font.family",
        "font.size",
        "font.line_height",
        "font.letter_spacing",
        "window.opacity",
        "window.padding",
    ] {
        if !cli.overrides_field(field) {
            if let Some(src) = base.attribution.get(field) {
                merged.attribution.insert(field.to_string(), src.clone());
            }
        }
    }
    merged
        .conflicts
        .retain(|c| !(c.new_source.layer == LayerKind::Cli && !cli.overrides_field(&c.field)));
    Ok(merged)
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
    // CTX-0177: `layout` follows the same fully-optional pattern: absent
    // table means "this layer says nothing" (plan.layout None so merge keeps
    // the lower-precedence value). When the table is present, omitted keys
    // default to `LayoutConfig` defaults (0 = edge-to-edge) so
    // `layout = { gaps_in = 1 }` keeps working without forcing `gaps_out`.
    // Present values are range-checked here (fail-closed with the field path)
    // and again by `LayoutConfig::validate` via `plan.validate()`.
    let layout = match data.layout {
        None => None,
        Some(l) => {
            let defaults = LayoutConfig::default();
            let gaps_in = match l.gaps_in {
                None => defaults.gaps_in,
                Some(v) => {
                    if !(0..=crate::types::MAX_LAYOUT_GAP_CELLS as i64).contains(&v) {
                        return Err(ConfigError::validation(
                            "layout.gaps_in",
                            format!(
                                "must be within [0, {}] (found {v})",
                                crate::types::MAX_LAYOUT_GAP_CELLS
                            ),
                        ));
                    }
                    v as u32
                }
            };
            let gaps_out = match l.gaps_out {
                None => defaults.gaps_out,
                Some(v) => {
                    if !(0..=crate::types::MAX_LAYOUT_GAP_CELLS as i64).contains(&v) {
                        return Err(ConfigError::validation(
                            "layout.gaps_out",
                            format!(
                                "must be within [0, {}] (found {v})",
                                crate::types::MAX_LAYOUT_GAP_CELLS
                            ),
                        ));
                    }
                    v as u32
                }
            };
            Some(LayoutConfig { gaps_in, gaps_out })
        }
    };

    let plan = ConfigPlan {
        schema_version: None,
        font,
        window,
        terminal,
        selection,
        layout,
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

/// Loads, evaluates, validates, and migrates a Lua config file into a
/// [`LayerKind::User`] plan.
///
/// The file **must** exist; missing files are the caller's decision (default
/// probe skips, explicit `--config`/`BITTY_CONFIG` fails). Oversize/
/// unreadable/budget- or shape-invalid files fail closed with [`ConfigError`]
/// (no panic).
///
/// # Errors
///
/// - [`ConfigError::InvalidInput`] for oversize, unreadable, or
///   budget-exceeding files.
/// - Parser/validation errors from [`parse_lua_config`].
pub fn load_user_layer(path: &Path) -> Result<LayeredPlan, ConfigError> {
    load_layer_with_kind(path, LayerKind::User)
}

/// Loads, evaluates, validates, and migrates a named-profile file into a
/// [`LayerKind::Profile`] plan (CTX-0169).
///
/// Same bounds and fail-closed posture as [`load_user_layer`]; the only
/// difference is the layer kind (precedence `Profile 40 < User 50`, so
/// `init.lua` still wins over the profile per `lua-and-xdg.md` §Layers).
/// The file **must** exist; a missing profile fails closed at the caller
/// (no silent fallback, exit 2).
///
/// # Errors
///
/// Same as [`load_user_layer`].
pub fn load_profile_layer(path: &Path) -> Result<LayeredPlan, ConfigError> {
    load_layer_with_kind(path, LayerKind::Profile)
}

/// Shared Lua-file loader behind [`load_user_layer`] and
/// [`load_profile_layer`]: size-bounded read, sandboxed [`parse_lua_config`],
/// [`crate::migration::migrate`], tagged with `kind`.
fn load_layer_with_kind(path: &Path, kind: LayerKind) -> Result<LayeredPlan, ConfigError> {
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
    let source = ConfigSource::new(kind, Some(path.display().to_string()));
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
    fn lua_layout_gaps_parse_and_validate() {
        // CTX-0177: explicit gaps parse; absent table means "says nothing"
        // (plan.layout None so merge keeps lower); present-but-partial
        // defaults omitted keys to 0; wrong types and out-of-range fail
        // closed naming the field (never the value beyond the line).
        let plan = parse_lua_config(
            r#"return { layout = { gaps_in = 1, gaps_out = 2 } }"#,
            &test_source(),
        )
        .expect("gaps parse");
        let layout = plan.layout.expect("layout present");
        assert_eq!(layout.gaps_in, 1);
        assert_eq!(layout.gaps_out, 2);
        let plan = parse_lua_config(r#"return { layout = { gaps_in = 1 } }"#, &test_source())
            .expect("partial layout parses");
        let layout = plan.layout.expect("layout present");
        assert_eq!(layout.gaps_in, 1);
        assert_eq!(layout.gaps_out, 0);
        let plan = parse_lua_config(r#"return { layout = {} }"#, &test_source())
            .expect("empty layout defaults");
        let layout = plan.layout.expect("layout present");
        assert_eq!(layout.gaps_in, 0);
        assert_eq!(layout.gaps_out, 0);
        let plan = parse_lua_config(
            r#"return { terminal = { scrollback = 10000 } }"#,
            &test_source(),
        )
        .expect("no layout table");
        assert!(plan.layout.is_none());
        for bad in [
            r#"return { layout = { gaps_in = -1 } }"#,
            r#"return { layout = { gaps_in = 17 } }"#,
            r#"return { layout = { gaps_out = 100 } }"#,
            r#"return { layout = { gaps_in = "1" } }"#,
            r#"return { layout = { gaps_in = 1.5 } }"#,
            r#"return { layout = "wide" }"#,
            r#"return { layout = { gaps_in = 1, bogus = 2 } }"#,
        ] {
            let err = parse_lua_config(bad, &test_source()).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("layout"),
                "must name the field: {bad} -> {msg}"
            );
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

    // CTX-0169 named profiles: precedence CLI > user file > profile >
    // defaults, plus fail-closed validation.

    #[test]
    fn profile_name_validation_accepts_simple_names() {
        assert_eq!(validate_profile_name("work").unwrap(), "work");
        assert_eq!(
            validate_profile_name("  coding-2_x ").unwrap(),
            "coding-2_x"
        );
    }

    #[test]
    fn profile_name_validation_rejects_paths_and_empty() {
        for bad in [
            "",
            "   ",
            "work.lua",
            "a/b",
            "a\\b",
            "..",
            "../work",
            "work..",
            "a.b",
            ".hidden",
            "with space",
            "semi;colon",
        ] {
            assert!(validate_profile_name(bad).is_err(), "must reject {bad:?}");
        }
        let long = "x".repeat(MAX_PROFILE_NAME_LEN + 1);
        assert!(validate_profile_name(&long).is_err());
    }

    #[test]
    fn profile_path_resolves_under_config_root() {
        let p = profile_file_path_with_env("work", Some("/xdg"), Some("/h")).expect("xdg wins");
        assert_eq!(p, PathBuf::from("/xdg/bitty/profiles/work.lua"));
        let p =
            profile_file_path_with_env("work", Some("  "), Some("/home/u")).expect("home fallback");
        assert_eq!(p, PathBuf::from("/home/u/.config/bitty/profiles/work.lua"));
        assert!(profile_file_path_with_env("work", None, None).is_err());
        assert!(profile_file_path_with_env("../evil", Some("/xdg"), Some("/h")).is_err());
    }

    #[test]
    fn resolve_explicit_prefers_cli_over_env() {
        assert_eq!(
            resolve_config_explicit(Some("/cli.lua"), Some("/env.lua")).as_deref(),
            Some("/cli.lua")
        );
        assert_eq!(
            resolve_config_explicit(None, Some("/env.lua")).as_deref(),
            Some("/env.lua")
        );
        assert_eq!(
            resolve_config_explicit(Some("  "), Some("/env.lua")).as_deref(),
            Some("/env.lua")
        );
        assert_eq!(resolve_config_explicit(None, None), None);
        assert_eq!(resolve_config_explicit(Some("  "), Some(" ")), None);
    }

    #[test]
    fn resolve_profile_request_prefers_cli_over_env() {
        assert_eq!(
            resolve_profile_request(Some("cli"), Some("env")).as_deref(),
            Some("cli")
        );
        assert_eq!(
            resolve_profile_request(None, Some("env")).as_deref(),
            Some("env")
        );
        assert_eq!(
            resolve_profile_request(Some("  "), Some("env")).as_deref(),
            Some("env")
        );
        assert_eq!(resolve_profile_request(None, None), None);
        assert_eq!(resolve_profile_request(Some(" "), Some("  ")), None);
    }

    #[test]
    fn cli_overrides_build_single_cli_layer() {
        let empty = CliOverrides::default();
        assert!(empty.is_empty());
        assert!(empty.to_layer().is_none());
        let cli = CliOverrides {
            theme: Some("dark".to_string()),
            ..Default::default()
        };
        assert!(!cli.is_empty());
        let layer = cli.to_layer().expect("layer");
        assert_eq!(layer.source.layer, LayerKind::Cli);
    }

    fn profile_layer_for(content: &str) -> LayeredPlan {
        let src = ConfigSource::new(LayerKind::Profile, Some("profiles/work.lua"));
        let plan = parse_lua_config(content, &src).expect("profile parses");
        LayeredPlan::new(src, plan)
    }

    #[test]
    fn profile_precedence_user_over_profile_over_default() {
        // Profile alone beats defaults.
        let profile = profile_layer_for(r#"return { theme = "dark" }"#);
        let cli = CliOverrides::default();
        let merged =
            resolve_effective_full(None, Some(profile.clone()), &cli).expect("profile>default");
        assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::Profile
        );
        // User file wins over the profile base.
        let user_src = test_source();
        let user_plan =
            parse_lua_config(r#"return { theme = "bitty-dark" }"#, &user_src).expect("user");
        let user = LayeredPlan::new(user_src, user_plan);
        let merged =
            resolve_effective_full(Some(user), Some(profile.clone()), &cli).expect("user>profile");
        assert_eq!(
            merged.effective.appearance.theme.as_deref(),
            Some("bitty-dark")
        );
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::User
        );
        // CLI wins over both.
        let cli = CliOverrides {
            theme: Some("cli-theme".to_string()),
            ..Default::default()
        };
        // Overlong CLI themes fail closed at merge (same as --theme today).
        let long = CliOverrides {
            theme: Some("x".repeat(65)),
            ..Default::default()
        };
        assert!(resolve_effective_full(None, Some(profile), &long).is_err());
        let user_src2 = test_source();
        let user_plan2 =
            parse_lua_config(r#"return { theme = "dark" }"#, &user_src2).expect("user2");
        let user2 = LayeredPlan::new(user_src2, user_plan2);
        let profile2 = profile_layer_for(r#"return { theme = "dark" }"#);
        let merged = resolve_effective_full(Some(user2), Some(profile2), &cli).expect("cli wins");
        assert_eq!(
            merged.effective.appearance.theme.as_deref(),
            Some("cli-theme")
        );
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::Cli
        );
    }

    #[test]
    fn profile_deep_merge_composes_with_user() {
        // Profile sets the font, user sets the theme: both survive (deep
        // merge across layers, not whole-file replacement).
        let profile = profile_layer_for(r#"return { font = { family = "Mono", size = 12 } }"#);
        let user_src = test_source();
        let user_plan = parse_lua_config(r#"return { theme = "dark" }"#, &user_src).expect("user");
        let user = LayeredPlan::new(user_src, user_plan);
        let merged = resolve_effective_full(Some(user), Some(profile), &CliOverrides::default())
            .expect("compose");
        assert_eq!(merged.effective.font.family, "Mono");
        assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::Profile
        );
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::User
        );
    }

    #[test]
    fn load_profile_layer_tags_profile_kind() {
        let dir = std::env::temp_dir().join(format!("bitty-ctx0169-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("work.lua");
        std::fs::write(&path, r#"return { theme = "dark" }"#).expect("write temp profile");
        let layer = load_profile_layer(&path).expect("load");
        assert_eq!(layer.source.layer, LayerKind::Profile);
        assert_eq!(
            layer.plan.appearance.unwrap().theme.as_deref(),
            Some("dark")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_profile_fails_closed() {
        let path = Path::new("/nonexistent-bitty-ctx0169/profiles/ghost.lua");
        assert!(load_profile_layer(path).is_err());
    }

    // CTX-0180 CLI appearance overrides: each flag overrides only its own
    // field for one launch (siblings inherit the merged lower layers),
    // invalid raws fail closed with the dotted field path, precedence stays
    // CLI > file > profile > defaults with exact per-field attribution.

    fn file_layer_with_appearance() -> LayeredPlan {
        let src = test_source();
        let plan = parse_lua_config(
            r#"return {
                font = { family = "File Mono", size = 12.0 },
                window = { opacity = 1.0, padding = 8 },
            }"#,
            &src,
        )
        .expect("file parses");
        LayeredPlan::new(src, plan)
    }

    #[test]
    fn cli_font_family_overrides_only_family() {
        let cli = CliOverrides {
            font_family: Some("Cli Mono".to_string()),
            ..Default::default()
        };
        assert!(!cli.is_empty());
        let merged =
            resolve_effective_full(Some(file_layer_with_appearance()), None, &cli).expect("merge");
        assert_eq!(merged.effective.font.family, "Cli Mono");
        // Siblings inherit the file, never defaults-clobber.
        assert!((merged.effective.font.size - 12.0).abs() < f32::EPSILON);
        assert!((merged.effective.window.opacity - 1.0).abs() < f32::EPSILON);
        assert_eq!(merged.effective.window.padding, 8);
        // Attribution stays exact per field.
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::Cli
        );
        assert_eq!(
            merged.source_of("font.size").unwrap().layer,
            LayerKind::User
        );
        assert_eq!(
            merged.source_of("window.opacity").unwrap().layer,
            LayerKind::User
        );
        // Genuine conflict kept for the overridden field; no spurious CLI
        // conflict for inherited siblings.
        assert!(
            merged
                .conflicts
                .iter()
                .any(|c| c.field == "font.family" && c.new_source.layer == LayerKind::Cli)
        );
        assert!(
            !merged
                .conflicts
                .iter()
                .any(|c| c.new_source.layer == LayerKind::Cli && c.field != "font.family")
        );
    }

    #[test]
    fn cli_font_size_overrides_only_size() {
        let cli = CliOverrides {
            font_size: Some("16".to_string()),
            ..Default::default()
        };
        let merged =
            resolve_effective_full(Some(file_layer_with_appearance()), None, &cli).expect("merge");
        assert_eq!(merged.effective.font.family, "File Mono");
        assert!((merged.effective.font.size - 16.0).abs() < f32::EPSILON);
        assert_eq!(merged.source_of("font.size").unwrap().layer, LayerKind::Cli);
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::User
        );
    }

    #[test]
    fn cli_opacity_overrides_only_opacity() {
        let cli = CliOverrides {
            opacity: Some("0.9".to_string()),
            ..Default::default()
        };
        let merged =
            resolve_effective_full(Some(file_layer_with_appearance()), None, &cli).expect("merge");
        assert!((merged.effective.window.opacity - 0.9).abs() < f32::EPSILON);
        assert_eq!(merged.effective.window.padding, 8);
        assert_eq!(
            merged.source_of("window.opacity").unwrap().layer,
            LayerKind::Cli
        );
        assert_eq!(
            merged.source_of("window.padding").unwrap().layer,
            LayerKind::User
        );
    }

    #[test]
    fn cli_appearance_all_flags_override_together() {
        let cli = CliOverrides {
            theme: Some("cli-theme".to_string()),
            font_family: Some("Cli Mono".to_string()),
            font_size: Some("14.5".to_string()),
            opacity: Some("0.85".to_string()),
        };
        let merged =
            resolve_effective_full(Some(file_layer_with_appearance()), None, &cli).expect("merge");
        assert_eq!(
            merged.effective.appearance.theme.as_deref(),
            Some("cli-theme")
        );
        assert_eq!(merged.effective.font.family, "Cli Mono");
        assert!((merged.effective.font.size - 14.5).abs() < f32::EPSILON);
        assert!((merged.effective.window.opacity - 0.85).abs() < f32::EPSILON);
        for field in [
            "appearance.theme",
            "font.family",
            "font.size",
            "window.opacity",
        ] {
            assert_eq!(
                merged.source_of(field).unwrap().layer,
                LayerKind::Cli,
                "field {field} must attribute to cli"
            );
        }
        // Untouched siblings keep file sources.
        assert_eq!(
            merged.source_of("window.padding").unwrap().layer,
            LayerKind::User
        );
    }

    #[test]
    fn cli_appearance_beats_profile_when_no_file() {
        let profile = profile_layer_for(r#"return { font = { family = "Prof Mono", size = 11 } }"#);
        let cli = CliOverrides {
            font_size: Some("18".to_string()),
            ..Default::default()
        };
        let merged = resolve_effective_full(None, Some(profile), &cli).expect("cli over profile");
        assert!((merged.effective.font.size - 18.0).abs() < f32::EPSILON);
        assert_eq!(merged.effective.font.family, "Prof Mono");
        assert_eq!(merged.source_of("font.size").unwrap().layer, LayerKind::Cli);
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::Profile
        );
    }

    #[test]
    fn cli_appearance_blank_means_no_override() {
        let cli = CliOverrides {
            font_family: Some("   ".to_string()),
            font_size: Some(String::new()),
            opacity: None,
            ..Default::default()
        };
        assert!(cli.is_empty());
        let merged =
            resolve_effective_full(Some(file_layer_with_appearance()), None, &cli).expect("merge");
        assert_eq!(merged.effective.font.family, "File Mono");
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::User
        );
    }

    #[test]
    fn cli_appearance_invalid_raws_fail_closed_with_field() {
        // Each invalid raw names its dotted field (never a silent ignore).
        for cli in [
            CliOverrides {
                font_size: Some("abc".to_string()),
                ..Default::default()
            },
            CliOverrides {
                font_size: Some("0".to_string()),
                ..Default::default()
            },
            CliOverrides {
                font_size: Some("-5".to_string()),
                ..Default::default()
            },
            CliOverrides {
                font_size: Some("129".to_string()),
                ..Default::default()
            },
            CliOverrides {
                font_size: Some("NaN".to_string()),
                ..Default::default()
            },
            CliOverrides {
                font_size: Some("inf".to_string()),
                ..Default::default()
            },
            CliOverrides {
                opacity: Some("abc".to_string()),
                ..Default::default()
            },
            CliOverrides {
                opacity: Some("1.5".to_string()),
                ..Default::default()
            },
            CliOverrides {
                opacity: Some("-0.1".to_string()),
                ..Default::default()
            },
            CliOverrides {
                opacity: Some("NaN".to_string()),
                ..Default::default()
            },
            CliOverrides {
                font_family: Some("x".repeat(MAX_FONT_FAMILY_LEN + 1)),
                ..Default::default()
            },
        ] {
            assert!(!cli.is_empty());
            let err = resolve_effective_full(Some(file_layer_with_appearance()), None, &cli)
                .expect_err("invalid CLI raw must fail closed");
            let msg = err.to_string();
            assert!(
                msg.contains("font.size")
                    || msg.contains("window.opacity")
                    || msg.contains("font.family"),
                "must name the field: {msg}"
            );
        }
    }

    #[test]
    fn cli_appearance_boundary_values_merge() {
        for (size, opacity) in [("0.5", "0.0"), ("128", "1.0"), ("12.0", "0.95")] {
            let cli = CliOverrides {
                font_size: Some(size.to_string()),
                opacity: Some(opacity.to_string()),
                ..Default::default()
            };
            let merged = resolve_effective_full(Some(file_layer_with_appearance()), None, &cli)
                .expect("boundary values must merge");
            assert!(
                (merged.effective.font.size - size.parse::<f32>().unwrap()).abs() < f32::EPSILON
            );
            assert!(
                (merged.effective.window.opacity - opacity.parse::<f32>().unwrap()).abs()
                    < f32::EPSILON
            );
        }
    }

    #[test]
    fn cli_layer_with_base_labels_active_flags() {
        let base = crate::types::EffectiveConfig::default();
        let cli = CliOverrides {
            font_family: Some("Cli Mono".to_string()),
            opacity: Some("0.9".to_string()),
            ..Default::default()
        };
        let layer = cli
            .to_layer_with_base(&base)
            .expect("valid")
            .expect("non-empty");
        assert_eq!(layer.source.layer, LayerKind::Cli);
        assert_eq!(
            layer.source.path.as_deref(),
            Some("cli:--font-family,--opacity")
        );
        // Theme-only keeps the historical label verbatim.
        let theme_only = CliOverrides {
            theme: Some("dark".to_string()),
            ..Default::default()
        };
        let layer = theme_only
            .to_layer_with_base(&base)
            .expect("valid")
            .expect("non-empty");
        assert_eq!(layer.source.path.as_deref(), Some("cli:--theme"));
        // Empty overrides build no layer.
        assert!(
            CliOverrides::default()
                .to_layer_with_base(&base)
                .expect("empty valid")
                .is_none()
        );
    }
}
