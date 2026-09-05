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
/// Default font family: Nerd-Font-patched JetBrains Mono.
///
/// Matches the CTX-0157 acceptance probe (`JetBrainsMono Nerd Font 12pt`
/// side-by-side vs ghostty must show no material difference) and renders
/// starship/opencode Nerd glyphs out of the box. System `monospace` remains
/// the ultimate fallback via [`FONT_FALLBACK_CHAIN`], so bare installs
/// without the Nerd font still start (headless fallback path in
/// `bitty-runtime`).
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";

/// Default point size: 12pt on Linux.
///
/// Matches ghostty `font-size = 12` (Linux; 13 on macOS) and keeps the
/// "smaller than normal" complaint closed: 12.0 was already the prior
/// default and stays. Kitty defaults to 11.0; bitty stays at ghostty parity.
pub const DEFAULT_FONT_SIZE: f32 = 12.0;

/// Default line-height multiplier: 1.2x.
///
/// Legacy design cell was `8x16` with no breathing room. Measured
/// `JetBrainsMonoNerdFont-Regular.ttf` (`fontTools`, UPM 1000,
/// ascent 1020 / descent -300): at 12pt (16px em) the true advance is
/// `0.6 * 16 = 9.6px` and the true line is `1320/1000 * 16 = 21.1px`.
/// Ghostty defaults to `adjust-cell-height = null` (pure font metrics) and
/// kitty to no `modify_font` adjustment; bitty's legacy `8x16` is ~20%
/// too narrow and ~32% too short vs those metrics. `1.2` gives
/// `round(16 * 1.2) = 19px` — a conservative "slight" breathing room
/// between legacy 16 and true 21, matching kitty 11pt line (~19.4px).
pub const DEFAULT_LINE_HEIGHT: f32 = 1.2;

/// Default letter-spacing: 1.0px.
///
/// Legacy advance 8px vs true 9.6px at 12pt: `+1px` gives effective width 9,
/// matching kitty 11pt advance (~8.8px -> 9) and moving toward the true 9.6
/// without jumping straight to 10. Ghostty `adjust-cell-width = null`;
/// the `+1` is justified only because the legacy base is cramped.
pub const DEFAULT_LETTER_SPACING: f32 = 1.0;

/// Legacy design cell (pre-CTX-0157): the compiled base that spacing
/// applies to. Kept explicit so [`FontConfig::effective_cell`] stays
/// deterministic and testable.
pub const BASE_CELL_WIDTH: u32 = 8;
/// Legacy design cell height (see [`BASE_CELL_WIDTH`]).
pub const BASE_CELL_HEIGHT: u32 = 16;

/// Documented monospace/Nerd fallback stack.
///
/// Order: configured primary (Nerd-patched by default) -> unpatched
/// `JetBrains Mono` -> system `monospace` (fontconfig/WC) ->
/// `DejaVu Sans Mono` (widely available). Mirrors ghostty (embedded
/// JetBrains Mono variable + symbols-only Nerd fallback, always present)
/// and kitty (`font_family = "monospace"` + builtin Nerd font,
/// `set_font_family(..., add_builtin_nerd_font=True)`).
///
/// Per-glyph fallback shaping stays deferred to the text RFC (ADR-0004
/// "Wrap" row); this chain is family-level attempt order for embedders:
/// try each in order until `load_font` succeeds, ending in headless.
/// [`FontConfig::fallback_chain`] builds the configured-first variant.
pub const FONT_FALLBACK_CHAIN: [&str; 4] = [
    DEFAULT_FONT_FAMILY,
    "JetBrains Mono",
    "monospace",
    "DejaVu Sans Mono",
];

/// Font configuration.
///
/// `family`/`size` match ghostty Linux defaults (`JetBrainsMono Nerd Font`
/// 12pt for the acceptance probe); `line_height`/`letter_spacing` give the
/// slight breathing room the legacy `8x16` cell lacked. All four are
/// tunable via `init.lua` `font = { family, size, line_height,
/// letter_spacing }` (new keys optional, defaulted — existing
/// `{ family, size }` tables keep working).
#[derive(Debug, Clone, PartialEq)]
pub struct FontConfig {
    /// Font family, trimmed, non-empty.
    pub family: String,
    /// Point size, finite, `> 0` and `<= 128`.
    pub size: f32,
    /// Line-height multiplier, finite within `[1.0, 2.0]`.
    pub line_height: f32,
    /// Extra advance in px, finite within `[0.0, 8.0]`.
    pub letter_spacing: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: DEFAULT_FONT_FAMILY.to_string(),
            size: DEFAULT_FONT_SIZE,
            line_height: DEFAULT_LINE_HEIGHT,
            letter_spacing: DEFAULT_LETTER_SPACING,
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
        if !(self.line_height.is_finite() && (1.0..=2.0).contains(&self.line_height)) {
            return Err(ConfigError::validation(
                "font.line_height",
                "must be finite within [1.0, 2.0]",
            ));
        }
        if !(self.letter_spacing.is_finite() && (0.0..=8.0).contains(&self.letter_spacing)) {
            return Err(ConfigError::validation(
                "font.letter_spacing",
                "must be finite within [0.0, 8.0]",
            ));
        }
        Ok(())
    }

    /// Family-level fallback attempt order, configured family first.
    ///
    /// Starts with `self.family` (trimmed), then the documented
    /// [`FONT_FALLBACK_CHAIN`] entries not already covered (case-insensitive
    /// dedup), preserving order. Bounded: at most `1 + CHAIN.len()` entries,
    /// each `<= MAX_FONT_FAMILY_LEN`.
    #[must_use]
    pub fn fallback_chain(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(1 + FONT_FALLBACK_CHAIN.len());
        let primary = self.family.trim().to_string();
        out.push(primary.clone());
        let lower = primary.to_lowercase();
        for cand in FONT_FALLBACK_CHAIN {
            if cand.to_lowercase() != lower
                && !out.iter().any(|e| e.to_lowercase() == cand.to_lowercase())
            {
                out.push(cand.to_string());
            }
        }
        out
    }

    /// Effective cell `(width, height)` after breathing room.
    ///
    /// `width = base_width + round(letter_spacing)`,
    /// `height = round(base_height * line_height)`, each saturated to
    /// `>= 1`. Defaults give `(9, 19)` from the legacy `(8, 16)` base.
    #[must_use]
    pub fn effective_cell(&self, base_width: u32, base_height: u32) -> (u32, u32) {
        let extra_w = self.letter_spacing.round().clamp(0.0, 8.0) as u32;
        let width = base_width.saturating_add(extra_w).max(1);
        let scaled_h = (f64::from(base_height) * f64::from(self.line_height)).round();
        let height = u32::try_from(scaled_h as i64).unwrap_or(u32::MAX).max(1);
        (width, height)
    }

    /// Effective cell from the legacy [`BASE_CELL_WIDTH`]/[`BASE_CELL_HEIGHT`].
    #[must_use]
    pub fn default_effective_cell(&self) -> (u32, u32) {
        self.effective_cell(BASE_CELL_WIDTH, BASE_CELL_HEIGHT)
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
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "JetBrains Mono".into(),
            size: f32::NAN,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 0.0,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 12.0,
            line_height: 0.9,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 12.0,
            letter_spacing: 9.0,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig::default().validate().expect("default valid");
    }

    #[test]
    fn font_defaults_match_ghostty_linux_acceptance() {
        let d = FontConfig::default();
        assert_eq!(d.family, DEFAULT_FONT_FAMILY);
        assert_eq!(d.family, "JetBrainsMono Nerd Font");
        assert!((d.size - 12.0).abs() < f32::EPSILON);
        assert!((d.line_height - 1.2).abs() < f32::EPSILON);
        assert!((d.letter_spacing - 1.0).abs() < f32::EPSILON);
        // Effective cell from legacy 8x16 base gives breathing room 9x19.
        assert_eq!(d.default_effective_cell(), (9, 19));
        assert_eq!(d.effective_cell(8, 16), (9, 19));
    }

    #[test]
    fn font_fallback_chain_is_documented_order() {
        let d = FontConfig::default();
        let chain = d.fallback_chain();
        assert_eq!(
            chain,
            vec![
                "JetBrainsMono Nerd Font".to_string(),
                "JetBrains Mono".to_string(),
                "monospace".to_string(),
                "DejaVu Sans Mono".to_string(),
            ]
        );
        // Custom primary stays first, chain dedups case-insensitively.
        let custom = FontConfig {
            family: "monospace".into(),
            ..Default::default()
        };
        let chain = custom.fallback_chain();
        assert_eq!(chain[0], "monospace");
        assert_eq!(chain.len(), 4);
        // No duplicates when primary already in chain.
        let nerd = FontConfig {
            family: "  jetbrainsmono nerd font  ".into(),
            ..Default::default()
        };
        let chain = nerd.fallback_chain();
        assert_eq!(chain.len(), 4);
    }

    #[test]
    fn font_effective_cell_math() {
        let base = FontConfig {
            line_height: 1.0,
            letter_spacing: 0.0,
            ..Default::default()
        };
        assert_eq!(base.effective_cell(8, 16), (8, 16));
        let roomy = FontConfig::default();
        assert_eq!(roomy.effective_cell(8, 16), (9, 19));
        // Zero base still saturates to >= 1.
        assert_eq!(roomy.effective_cell(0, 0), (1, 1));
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
