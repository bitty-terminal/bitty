//! Built-in theme presets: the single source of truth for terminal colors.
//!
//! The designed default is [`BITTY_DARK`] (name [`DEFAULT_THEME_NAME`]), a
//! dark-first preset in the `#1e1e2e` family. It resolves with zero config
//! files: [`resolve_theme`] maps `None`/empty/unknown `appearance.theme`
//! values to the default preset without any file I/O, so bare `bitty` looks
//! designed out of the box. Unknown names fall back to the default and are
//! logged to stderr (see [`resolve_theme`]); [`resolve_theme_with_status`]
//! exposes the same decision as data for headless tests.
//!
//! # Bitty Dark palette — role table
//!
//! | Role | Swatch | Value | Serves |
//! |------|--------|-------|--------|
//! | Background | `#1e1e2e` | `[0x1E, 0x1E, 0x2E]` | Window clear color and default cell background. Dark indigo-gray: avoids pure-black harshness and keeps colored text legible under 1.6x Hyprland scaling. |
//! | Foreground | `#cdd6f4` | `[0xCD, 0xD6, 0xF4]` | Default glyph color and prompt text. Soft lavender-white: avoids pure-white glare against the dark background. |
//! | Cursor | `#f5e0dc` | `[0xF5, 0xE0, 0xDC]` | Block cursor fill. Warm rosewater, distinct from both foreground and selection so the cursor stays findable on a busy line. |
//! | Selection | `#313244` | `[0x31, 0x32, 0x44]` | Selection background fill. One step above the background: visible without shouting, and dark enough that foreground-colored text stays readable on top. |
//! | ANSI 0 (black) | `#45475a` | `[0x45, 0x47, 0x5A]` | Muted surface tone, not pure black, so "black" text and dim UI chrome remain visible on the dark background. |
//! | ANSI 1 (red) | `#f38ba8` | `[0xF3, 0x8B, 0xA8]` | Errors, failures, `ls` archives/special flags. Soft red: urgent without vibrating. |
//! | ANSI 2 (green) | `#a6e3a1` | `[0xA6, 0xE3, 0xA1]` | Success, `+` diffs, executable green in `ls --color`. This is the green the synthetic demo pump (`\x1b[32m`) resolves to — no hardcoded green remains in render. |
//! | ANSI 3 (yellow) | `#f9e2af` | `[0xF9, 0xE2, 0xAF]` | Warnings, pending states, `ls` device/special files. Warm and readable on dark. |
//! | ANSI 4 (blue) | `#89b4fa` | `[0x89, 0xB4, 0xFA]` | Directories in `ls --color`, links, info. Periwinkle blue tuned for dark backgrounds. |
//! | ANSI 5 (magenta) | `#f5c2e7` | `[0xF5, 0xC2, 0xE7]` | Symlinks, prompts accents, highlights. Soft pink. |
//! | ANSI 6 (cyan) | `#94e2d5` | `[0x94, 0xE2, 0xD5]` | Teal for accents, diagnostics, `ls` multimedia. |
//! | ANSI 7 (white) | `#bac2de` | `[0xBA, 0xC2, 0xDE]` | Secondary text, `ls` regular files. Subtext tone: deliberately dimmer than the foreground. |
//! | ANSI 8 (bright black) | `#585b70` | `[0x58, 0x5B, 0x70]` | Bright-black / gray comments and dim decorations. Lighter than ANSI 0 so the two stay distinguishable. |
//! | ANSI 9–14 (bright hues) | same as 1–6 | — | Bright red/green/yellow/blue/magenta/cyan reuse the base hues so intent survives bold/bright styling without introducing six more tints. |
//! | ANSI 15 (bright white) | `#cdd6f4` | `[0xCD, 0xD6, 0xF4]` | Brightest text; equals the foreground so emphasized text matches the default glyph tone. |
//!
//! Indices 16–231 (6x6x6 cube) and 232–255 (grayscale ramp) stay
//! xterm-compatible and are owned by `bitty-render`, not by this preset.
//!
//! Taste reference (read-only, never copied): the hue choices were informed
//! by glancing at the default themes shipped in
//! `recording/references/{alacritty,kitty,ghostty,wezterm}` per DEC-0004.
//! Every value above is written out explicitly here and owned by this module.
//!
//! Config-file loading is out of scope (CTX-0148): this module performs no
//! file I/O and knows no config paths. It maps an already-parsed
//! `appearance.theme` identifier to a preset.

/// Registry identifier of the designed default preset.
pub const DEFAULT_THEME_NAME: &str = "bitty-dark";

/// Alias accepted for the default preset (convenience; CLI-first naming per DEC-0007).
pub const DARK_THEME_ALIAS: &str = "dark";

/// A built-in color preset: background, foreground, cursor, selection, and
/// the 16 ANSI colors. All channels are unpremultiplied `sRGB` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Registry name used in `appearance.theme` (e.g. `"bitty-dark"`).
    pub name: &'static str,
    /// Window clear color and default cell background.
    pub background: [u8; 3],
    /// Default glyph color.
    pub foreground: [u8; 3],
    /// Block cursor fill.
    pub cursor: [u8; 3],
    /// Selection background fill.
    pub selection: [u8; 3],
    /// The 16 ANSI colors, indices 0–15 (8 normal + 8 bright).
    pub ansi: [[u8; 3]; 16],
}

impl Theme {
    /// ANSI entry `index` (0–15) as RGB bytes.
    #[must_use]
    pub const fn ansi_entry(self, index: u8) -> [u8; 3] {
        self.ansi[index as usize % 16]
    }
}

/// The designed default preset: Bitty Dark.
///
/// Values are documented in the module-level role table.
pub static BITTY_DARK: Theme = Theme {
    name: DEFAULT_THEME_NAME,
    background: [0x1E, 0x1E, 0x2E],
    foreground: [0xCD, 0xD6, 0xF4],
    cursor: [0xF5, 0xE0, 0xDC],
    selection: [0x31, 0x32, 0x44],
    ansi: [
        [0x45, 0x47, 0x5A], // 0 black
        [0xF3, 0x8B, 0xA8], // 1 red
        [0xA6, 0xE3, 0xA1], // 2 green
        [0xF9, 0xE2, 0xAF], // 3 yellow
        [0x89, 0xB4, 0xFA], // 4 blue
        [0xF5, 0xC2, 0xE7], // 5 magenta
        [0x94, 0xE2, 0xD5], // 6 cyan
        [0xBA, 0xC2, 0xDE], // 7 white
        [0x58, 0x5B, 0x70], // 8 bright black
        [0xF3, 0x8B, 0xA8], // 9 bright red
        [0xA6, 0xE3, 0xA1], // 10 bright green
        [0xF9, 0xE2, 0xAF], // 11 bright yellow
        [0x89, 0xB4, 0xFA], // 12 bright blue
        [0xF5, 0xC2, 0xE7], // 13 bright magenta
        [0x94, 0xE2, 0xD5], // 14 bright cyan
        [0xCD, 0xD6, 0xF4], // 15 bright white (= foreground)
    ],
};

/// How [`resolve_theme_with_status`] reached its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeResolution {
    /// No name was given (or it was empty/whitespace): the default preset.
    Default,
    /// A known preset name resolved to its exact values.
    Named,
    /// An unknown name fell back to the default preset (and was logged).
    FallbackUnknown,
}

/// Returns the designed default preset.
#[must_use]
pub const fn default_theme() -> &'static Theme {
    &BITTY_DARK
}

/// Normalizes a raw `appearance.theme` value: trims surrounding whitespace
/// and lowercases it for comparison. Returns `None` for `None`, empty, or
/// whitespace-only input.
#[must_use]
pub fn normalize_theme_name(name: Option<&str>) -> Option<String> {
    let raw = name?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > super::types::MAX_THEME_LEN {
        return Some(trimmed.to_lowercase());
    }
    Some(trimmed.to_lowercase())
}

/// Resolves an `appearance.theme` identifier to a preset plus how it was
/// reached. Pure function: no I/O, no logging — use [`resolve_theme`] for
/// the logging variant used on the startup path.
///
/// - `None`/empty/whitespace → ([`BITTY_DARK`], [`ThemeResolution::Default`]).
/// - Known names (`"bitty-dark"`, alias `"dark"`, case-insensitive) →
///   (preset, [`ThemeResolution::Named`]).
/// - Anything else → ([`BITTY_DARK`], [`ThemeResolution::FallbackUnknown`]).
#[must_use]
pub fn resolve_theme_with_status(name: Option<&str>) -> (&'static Theme, ThemeResolution) {
    let Some(normalized) = normalize_theme_name(name) else {
        return (&BITTY_DARK, ThemeResolution::Default);
    };
    if normalized == DEFAULT_THEME_NAME || normalized == DARK_THEME_ALIAS {
        return (&BITTY_DARK, ThemeResolution::Named);
    }
    (&BITTY_DARK, ThemeResolution::FallbackUnknown)
}

/// Resolves an `appearance.theme` identifier to a preset for the startup
/// path. Unknown non-empty names fall back to the default preset and are
/// logged to stderr so a typo in config is visible instead of silent.
///
/// No file I/O is performed; the input is an already-parsed identifier.
#[must_use]
pub fn resolve_theme(name: Option<&str>) -> &'static Theme {
    let (theme, status) = resolve_theme_with_status(name);
    if status == ThemeResolution::FallbackUnknown {
        let raw = name.unwrap_or_default().trim();
        eprintln!("bitty: unknown theme '{raw}'; falling back to '{DEFAULT_THEME_NAME}'");
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_resolves_to_default() {
        let (theme, status) = resolve_theme_with_status(None);
        assert_eq!(status, ThemeResolution::Default);
        assert_eq!(theme.name, DEFAULT_THEME_NAME);
        assert!(std::ptr::eq(theme, &BITTY_DARK));
    }

    #[test]
    fn empty_and_whitespace_resolve_to_default() {
        for input in ["", "   ", "\t\n "] {
            let (theme, status) = resolve_theme_with_status(Some(input));
            assert_eq!(status, ThemeResolution::Default, "input: {input:?}");
            assert_eq!(theme.name, DEFAULT_THEME_NAME);
        }
    }

    #[test]
    fn known_name_resolves_to_exact_values() {
        for input in ["bitty-dark", "  Bitty-Dark ", "DARK", "dark"] {
            let (theme, status) = resolve_theme_with_status(Some(input));
            assert_eq!(status, ThemeResolution::Named, "input: {input:?}");
            assert_eq!(theme.background, [0x1E, 0x1E, 0x2E]);
            assert_eq!(theme.foreground, [0xCD, 0xD6, 0xF4]);
            assert_eq!(theme.cursor, [0xF5, 0xE0, 0xDC]);
            assert_eq!(theme.selection, [0x31, 0x32, 0x44]);
            assert_eq!(theme.ansi[2], [0xA6, 0xE3, 0xA1]);
            assert_eq!(theme.ansi[4], [0x89, 0xB4, 0xFA]);
        }
    }

    #[test]
    fn unknown_name_falls_back_to_default() {
        let (theme, status) = resolve_theme_with_status(Some("solarized-light"));
        assert_eq!(status, ThemeResolution::FallbackUnknown);
        assert_eq!(theme.name, DEFAULT_THEME_NAME);
        assert_eq!(theme.background, BITTY_DARK.background);
        // The logging variant agrees on the value.
        let logged = resolve_theme(Some("solarized-light"));
        assert_eq!(logged.background, BITTY_DARK.background);
    }

    #[test]
    fn default_theme_matches_preset_values() {
        let theme = default_theme();
        assert_eq!(theme.name, DEFAULT_THEME_NAME);
        assert_eq!(theme.ansi.len(), 16);
        assert_eq!(theme.ansi_entry(2), [0xA6, 0xE3, 0xA1]);
        assert_eq!(theme.ansi_entry(15), theme.foreground);
        // Background is dark-first but not pure black; foreground is
        // bright but not pure white (no harshness).
        assert_ne!(theme.background, [0, 0, 0]);
        assert_ne!(theme.foreground, [0xFF, 0xFF, 0xFF]);
    }
}
