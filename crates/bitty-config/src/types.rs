//! Typed configuration structs for the draft `ConfigPlan` pipeline.
//!
//! These types are **candidate** shapes derived from
//! `bitty-docs/docs/configuration/lua-and-xdg.md` and the proposed
//! configuration-model RFC. They are pure data, cloneable, comparable, and
//! validated without I/O.
//!
//! # Draft status
//!
//! The RFC is still `Proposed` and may change. Field sets, defaults, and
//! validation thresholds are **not normative** and will track the accepted
//! contract once an RFC is accepted. See crate-level docs for the mapping
//! table.

use crate::error::ConfigError;

/// Upper bounds that keep every structure bounded against untrusted input
/// (threat T-01).
pub const MAX_FONT_FAMILY_LEN: usize = 128;
pub const MAX_THEME_LEN: usize = 64;
pub const MAX_PLUGIN_ID_LEN: usize = 128;
pub const MAX_KEYMAPS: usize = 1024;
pub const MAX_PLUGINS: usize = 1024;
pub const MAX_SHELL_LEN: usize = 1024;

/// Default lines scrolled per wheel notch (LineDelta unit 1.0).
/// Matches alacritty/ghostty-class `3` lines per tick.
pub const DEFAULT_SCROLL_LINES_PER_NOTCH: u32 = 3;

/// Maximum lines per wheel notch (per-frame SGR/viewport cap is 32).
pub const MAX_SCROLL_LINES_PER_NOTCH: u32 = 32;

/// Default smooth-scroll (PixelDelta) pixels per wheel notch.
/// Matches the default 8x16 cell height (one cell per notch before the
/// lines multiplier).
pub const DEFAULT_SCROLL_PIXELS_PER_NOTCH: u32 = 16;

/// Maximum smooth-scroll pixels per wheel notch.
pub const MAX_SCROLL_PIXELS_PER_NOTCH: u32 = 256;

/// Font configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct FontConfig {
    /// Font family, trimmed, non-empty.
    pub family: String,
    /// Point size, finite, `> 0` and `<= 128`.
    pub size: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".to_string(),
            size: 12.0,
        }
    }
}

impl FontConfig {
    /// Validate this font config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let fam = self.family.trim();
        if fam.is_empty() {
            return Err(ConfigError::validation(
                "font.family",
                "must not be empty or whitespace",
            ));
        }
        if fam.len() > MAX_FONT_FAMILY_LEN {
            return Err(ConfigError::validation(
                "font.family",
                format!("must be <= {MAX_FONT_FAMILY_LEN} bytes"),
            ));
        }
        if !(self.size.is_finite() && self.size > 0.0 && self.size <= 128.0) {
            return Err(ConfigError::validation(
                "font.size",
                "must be finite within (0, 128]",
            ));
        }
        Ok(())
    }
}

/// Window presentation configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowConfig {
    /// Opacity `0.0..=1.0`, finite.
    pub opacity: f32,
    /// Padding in logical pixels `0..=64`.
    pub padding: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            padding: 8,
        }
    }
}

impl WindowConfig {
    /// Validate window config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(self.opacity.is_finite() && (0.0..=1.0).contains(&self.opacity)) {
            return Err(ConfigError::validation(
                "window.opacity",
                "must be finite within [0.0, 1.0]",
            ));
        }
        if self.padding > 64 {
            return Err(ConfigError::validation("window.padding", "must be <= 64"));
        }
        Ok(())
    }
}

/// Terminal behavior configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConfig {
    /// Scrollback lines, `0..=100000` (bounded memory).
    pub scrollback: u32,
    /// Preferred shell argv[0], if overridden; trimmed non-empty when present.
    pub shell: Option<String>,
    /// Lines scrolled per wheel notch, `1..=32` (CTX-0185; default 3,
    /// ghostty/alacritty-class throughput).
    pub scroll_lines_per_notch: u32,
    /// Smooth-scroll pixels per wheel notch, `1..=256` (CTX-0185; default 16,
    /// one default cell height per notch before the lines multiplier).
    pub scroll_pixels_per_notch: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            shell: None,
            scroll_lines_per_notch: DEFAULT_SCROLL_LINES_PER_NOTCH,
            scroll_pixels_per_notch: DEFAULT_SCROLL_PIXELS_PER_NOTCH,
        }
    }
}

impl TerminalConfig {
    /// Validate terminal config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.scrollback > 100_000 {
            return Err(ConfigError::validation(
                "terminal.scrollback",
                "must be <= 100000",
            ));
        }
        if !(1..=MAX_SCROLL_LINES_PER_NOTCH).contains(&self.scroll_lines_per_notch) {
            return Err(ConfigError::validation(
                "terminal.scroll_lines_per_notch",
                format!("must be within [1, {MAX_SCROLL_LINES_PER_NOTCH}]"),
            ));
        }
        if !(1..=MAX_SCROLL_PIXELS_PER_NOTCH).contains(&self.scroll_pixels_per_notch) {
            return Err(ConfigError::validation(
                "terminal.scroll_pixels_per_notch",
                format!("must be within [1, {MAX_SCROLL_PIXELS_PER_NOTCH}]"),
            ));
        }
        if let Some(s) = &self.shell {
            let t = s.trim();
            if t.is_empty() {
                return Err(ConfigError::validation(
                    "terminal.shell",
                    "when present must be non-empty after trimming",
                ));
            }
            if t.len() > MAX_SHELL_LEN {
                return Err(ConfigError::validation(
                    "terminal.shell",
                    format!("must be <= {MAX_SHELL_LEN} bytes"),
                ));
            }
        }
        Ok(())
    }
}

/// Appearance configuration.
///
/// The optional theme identifier resolves through the built-in preset
/// registry ([`crate::theme`]): `None`/empty means the designed default
/// preset ([`crate::theme::DEFAULT_THEME_NAME`]), a known name resolves to
/// its exact values, and an unknown name falls back to the default (logged).
/// No config-file I/O happens here; the identifier is already-parsed data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppearanceConfig {
    /// Optional theme identifier.
    pub theme: Option<String>,
}

impl AppearanceConfig {
    /// Validate appearance config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(t) = &self.theme {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                return Err(ConfigError::validation(
                    "appearance.theme",
                    "when present must be non-empty after trimming",
                ));
            }
            if trimmed.len() > MAX_THEME_LEN {
                return Err(ConfigError::validation(
                    "appearance.theme",
                    format!("must be <= {MAX_THEME_LEN} bytes"),
                ));
            }
        }
        Ok(())
    }
}

/// A single key mapping — data describing a chord, action, and context.
///
/// RFC merge rule: key mappings merge by `context + chord` identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeymapEntry {
    /// Chord string, e.g. `"ctrl+shift+p"`.
    pub chord: String,
    /// Action/command identifier.
    pub action: String,
    /// Context in which the mapping applies (e.g. `"global"`).
    pub context: String,
}

impl KeymapEntry {
    /// Validate this entry: shape bounds plus semantic chord/action/context
    /// checks ([`crate::keymap`]); unknown actions or keys fail closed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.chord.trim().is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].chord",
                "must not be empty",
            ));
        }
        if self.action.trim().is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].action",
                "must not be empty",
            ));
        }
        if self.context.trim().is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].context",
                "must not be empty",
            ));
        }
        if self.chord.len() > 128 || self.action.len() > 256 || self.context.len() > 128 {
            return Err(ConfigError::validation(
                "keymaps[]",
                "chord/action/context exceed length bounds",
            ));
        }
        crate::keymap::validate_entry(self)
    }

    /// Identity key for set-by-identifier merging.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}::{}", self.context, self.chord)
    }
}

/// A plugin declaration.
///
/// RFC merge rule: plugin set merges by globally unique plugin `id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginSpec {
    /// Globally unique plugin identifier, e.g. `"xuepoo/bitty-markdown"`.
    pub id: String,
    /// Whether the plugin is enabled.
    pub enabled: bool,
}

impl PluginSpec {
    /// Validate this spec.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err(ConfigError::validation("plugins[].id", "must not be empty"));
        }
        if id.len() > MAX_PLUGIN_ID_LEN {
            return Err(ConfigError::validation(
                "plugins[].id",
                format!("must be <= {MAX_PLUGIN_ID_LEN} bytes"),
            ));
        }
        Ok(())
    }
}

/// Fully resolved effective configuration after merge — every field has a
/// concrete value, never `Option`, derived from core defaults plus layers.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    /// Font config.
    pub font: FontConfig,
    /// Window config.
    pub window: WindowConfig,
    /// Terminal config.
    pub terminal: TerminalConfig,
    /// Appearance config (theme defaults to `None` if unset).
    pub appearance: AppearanceConfig,
    /// Keymaps, possibly empty.
    pub keymaps: Vec<KeymapEntry>,
    /// Plugins, possibly empty.
    pub plugins: Vec<PluginSpec>,
    /// Profile name that produced this config, if any.
    pub profile: Option<String>,
    /// Schema version of the source plan that produced this config.
    pub schema_version: u32,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            window: WindowConfig::default(),
            terminal: TerminalConfig::default(),
            appearance: AppearanceConfig::default(),
            keymaps: Vec::new(),
            plugins: Vec::new(),
            profile: None,
            schema_version: crate::migration::CURRENT_SCHEMA_VERSION,
        }
    }
}

impl EffectiveConfig {
    /// Validate all fields of the effective config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.font.validate()?;
        self.window.validate()?;
        self.terminal.validate()?;
        self.appearance.validate()?;
        if self.keymaps.len() > MAX_KEYMAPS {
            return Err(ConfigError::validation(
                "keymaps",
                format!("must contain <= {MAX_KEYMAPS} entries"),
            ));
        }
        if self.plugins.len() > MAX_PLUGINS {
            return Err(ConfigError::validation(
                "plugins",
                format!("must contain <= {MAX_PLUGINS} entries"),
            ));
        }
        for km in &self.keymaps {
            km.validate()?;
        }
        for p in &self.plugins {
            p.validate()?;
        }
        // Keymap and plugin IDs must be unique per merge contract.
        let mut seen_km = std::collections::HashSet::new();
        for km in &self.keymaps {
            let id = km.id();
            if !seen_km.insert(id.clone()) {
                return Err(ConfigError::validation(
                    "keymaps",
                    format!("duplicate keymap id '{id}'"),
                ));
            }
        }
        let mut seen_pl = std::collections::HashSet::new();
        for p in &self.plugins {
            let id = p.id.trim().to_string();
            if !seen_pl.insert(id.clone()) {
                return Err(ConfigError::validation(
                    "plugins",
                    format!("duplicate plugin id '{id}'"),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_validation() {
        FontConfig {
            family: String::new(),
            size: 12.0,
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "JetBrains Mono".into(),
            size: f32::NAN,
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 0.0,
        }
        .validate()
        .unwrap_err();
        FontConfig::default().validate().expect("default valid");
    }

    #[test]
    fn window_validation() {
        WindowConfig {
            opacity: 2.0,
            padding: 8,
        }
        .validate()
        .unwrap_err();
        WindowConfig {
            opacity: 0.5,
            padding: 100,
        }
        .validate()
        .unwrap_err();
        WindowConfig::default().validate().expect("default valid");
    }

    #[test]
    fn terminal_validation() {
        TerminalConfig {
            scrollback: 200_000,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        TerminalConfig {
            scrollback: 10,
            shell: Some("   ".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        TerminalConfig::default().validate().expect("default valid");
    }

    #[test]
    fn terminal_scroll_speed_validation() {
        // CTX-0185: lines/pixels per notch are validated fail-closed.
        for bad_lines in [0, MAX_SCROLL_LINES_PER_NOTCH + 1] {
            TerminalConfig {
                scroll_lines_per_notch: bad_lines,
                ..Default::default()
            }
            .validate()
            .unwrap_err();
        }
        for bad_pixels in [0, MAX_SCROLL_PIXELS_PER_NOTCH + 1] {
            TerminalConfig {
                scroll_pixels_per_notch: bad_pixels,
                ..Default::default()
            }
            .validate()
            .unwrap_err();
        }
        for (lines, pixels) in [(1, 1), (3, 16), (32, 256)] {
            TerminalConfig {
                scroll_lines_per_notch: lines,
                scroll_pixels_per_notch: pixels,
                ..Default::default()
            }
            .validate()
            .expect("boundary scroll speed must be valid");
        }
    }

    #[test]
    fn keymap_id_uniqueness_enforced() {
        let cfg = EffectiveConfig {
            keymaps: vec![
                KeymapEntry {
                    chord: "ctrl+p".into(),
                    action: "focus_next".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "ctrl+p".into(),
                    action: "focus_prev".into(),
                    context: "global".into(),
                },
            ],
            ..Default::default()
        };
        cfg.validate().unwrap_err();
    }

    #[test]
    fn plugin_id_uniqueness_enforced() {
        let cfg = EffectiveConfig {
            plugins: vec![
                PluginSpec {
                    id: "a/b".into(),
                    enabled: true,
                },
                PluginSpec {
                    id: "a/b".into(),
                    enabled: false,
                },
            ],
            ..Default::default()
        };
        cfg.validate().unwrap_err();
    }

    #[test]
    fn effective_default_valid() {
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
    }
}
