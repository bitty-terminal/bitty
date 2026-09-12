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
//! # Preset catalog
//!
//! [`ALL_PRESETS`] is the curated built-in catalog. Each entry records its
//! [`ThemeCategory`] (`Dark`/`Light`), the upstream project URL, and the
//! upstream license. Selection still happens purely by name (or alias, see
//! [`Theme::aliases`]); the category and provenance are metadata used by
//! documentation, `bitty list themes`, and the fail-closed catalog tests
//! below. Presets are reproduced faithfully from their upstream palettes;
//! they are not derived from one another.
//!
//! Provenance is per preset: the `source` field is the project that owns the
//! palette, and the doc comment records the exact export used. Where a
//! canonical Alacritty/kitty export exists for the project it is preferred;
//! otherwise the MIT-licensed `iTerm2-Color-Schemes` export of that project
//! is used and the owning project is still `source`. No non-permissive
//! palette is included, and no value is invented to fill a gap.
//!
//! Outline tokens (`border_focused`/`border_idle`, CTX-0340) are Bitty-owned
//! rather than upstream palette data: each preset's focused outline is the
//! highest-contrast ANSI accent that clears the 3:1 non-text contrast floor
//! against its background (preferring blue/cyan tints, falling back to the
//! full ANSI set when none of those clears it), and the idle outline is ANSI
//! 8 (bright black). [`BITTY_DARK`] keeps the originally ratified values.
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
//! | Border focused | `#33CCFF` | `[0x33, 0xCC, 0xFF, 0xFF]` | Focused View outline (CTX-0340 `border.focused`). Saturated cyan accent, >= 3:1 against the background and the idle outline. |
//! | Border idle | `#595959AA` | `[0x59, 0x59, 0x59, 0xAA]` | Idle View outline (CTX-0340 `border.idle`). Desaturated translucent gray: subtle by design while still clearing the advisory 1.5:1 floor. |
//!
//! Indices 16–231 (6x6x6 cube) and 232–255 (grayscale ramp) stay
//! xterm-compatible and are owned by `bitty-render`, not by this preset.
//!
//! Config-file loading is out of scope (CTX-0148): this module performs no
//! file I/O and knows no config paths. It maps an already-parsed
//! `appearance.theme` identifier to a preset.

use crate::types::OutlineColor;

/// Registry identifier of the designed default preset.
pub const DEFAULT_THEME_NAME: &str = "bitty-dark";

/// Alias accepted for the default preset (convenience; CLI-first naming per DEC-0007).
pub const DARK_THEME_ALIAS: &str = "dark";

/// Light/dark classification of a [`Theme`] preset.
///
/// Metadata only: selection is always by name (or alias). The category drives
/// documentation, `bitty list themes`, and the catalog's "both a dark and a
/// light preset exist" invariant; it is never consulted by color resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeCategory {
    /// A dark-background preset.
    Dark,
    /// A light-background preset.
    Light,
}

impl ThemeCategory {
    /// Stable lowercase label (for docs and `bitty list themes` output).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// A built-in color preset: background, foreground, cursor, selection, and
/// the 16 ANSI colors. All channels are unpremultiplied `sRGB` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Registry name used in `appearance.theme` (e.g. `"bitty-dark"`).
    pub name: &'static str,
    /// Dark/light classification (metadata; not used for selection).
    pub category: ThemeCategory,
    /// Accepted alternate names, normalized lowercase (e.g. `["dark"]`).
    /// Empty when the preset has no alias.
    pub aliases: &'static [&'static str],
    /// Upstream project URL that owns this palette (or the Bitty repository
    /// for the original default preset).
    pub source: &'static str,
    /// Upstream license identifier (SPDX where known).
    pub license: &'static str,
    /// Window clear color and default cell background.
    pub background: [u8; 3],
    /// Default glyph color.
    pub foreground: [u8; 3],
    /// Block cursor fill.
    pub cursor: [u8; 3],
    /// Selection background fill.
    pub selection: [u8; 3],
    /// Focused View outline token (CTX-0340 RFC-0001 `border.focused`).
    pub border_focused: OutlineColor,
    /// Idle View outline token (CTX-0340 RFC-0001 `border.idle`).
    pub border_idle: OutlineColor,
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
    category: ThemeCategory::Dark,
    aliases: &[DARK_THEME_ALIAS],
    source: "https://github.com/bitty-terminal/bitty",
    license: "MIT OR Apache-2.0",
    background: [0x1E, 0x1E, 0x2E],
    foreground: [0xCD, 0xD6, 0xF4],
    cursor: [0xF5, 0xE0, 0xDC],
    selection: [0x31, 0x32, 0x44],
    border_focused: crate::types::DEFAULT_DECORATION_BORDER_FOCUSED,
    border_idle: crate::types::DEFAULT_DECORATION_BORDER_IDLE,
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

/// Preset `tokyo-night` (dark).
///
/// Origin: Tokyo Night (folke). Values reproduced from <https://github.com/folke/tokyonight.nvim> (Apache-2.0),
/// canonical well-known export (kitty).
pub static TOKYO_NIGHT: Theme = Theme {
    name: "tokyo-night",
    category: ThemeCategory::Dark,
    aliases: &["tokyonight"],
    source: "https://github.com/folke/tokyonight.nvim",
    license: "Apache-2.0",
    background: [0x1A, 0x1B, 0x26],
    foreground: [0xC0, 0xCA, 0xF5],
    cursor: [0xC0, 0xCA, 0xF5],
    selection: [0x28, 0x34, 0x57],
    border_focused: OutlineColor([0xA4, 0xDA, 0xFF, 0xFF]),
    border_idle: OutlineColor([0x41, 0x48, 0x68, 0xFF]),
    ansi: [
        [0x15, 0x16, 0x1E], // 0 black
        [0xF7, 0x76, 0x8E], // 1 red
        [0x9E, 0xCE, 0x6A], // 2 green
        [0xE0, 0xAF, 0x68], // 3 yellow
        [0x7A, 0xA2, 0xF7], // 4 blue
        [0xBB, 0x9A, 0xF7], // 5 magenta
        [0x7D, 0xCF, 0xFF], // 6 cyan
        [0xA9, 0xB1, 0xD6], // 7 white
        [0x41, 0x48, 0x68], // 8 black
        [0xFF, 0x89, 0x9D], // 9 red
        [0x9F, 0xE0, 0x44], // 10 green
        [0xFA, 0xBA, 0x4A], // 11 yellow
        [0x8D, 0xB0, 0xFF], // 12 blue
        [0xC7, 0xA9, 0xFF], // 13 magenta
        [0xA4, 0xDA, 0xFF], // 14 cyan
        [0xC0, 0xCA, 0xF5], // 15 white
    ],
};

/// Preset `tokyo-night-storm` (dark).
///
/// Origin: Tokyo Night Storm (folke). Values reproduced from <https://github.com/folke/tokyonight.nvim> (Apache-2.0),
/// canonical well-known export (kitty).
pub static TOKYO_NIGHT_STORM: Theme = Theme {
    name: "tokyo-night-storm",
    category: ThemeCategory::Dark,
    aliases: &["tokyonight-storm"],
    source: "https://github.com/folke/tokyonight.nvim",
    license: "Apache-2.0",
    background: [0x24, 0x28, 0x3B],
    foreground: [0xC0, 0xCA, 0xF5],
    cursor: [0xC0, 0xCA, 0xF5],
    selection: [0x2E, 0x3C, 0x64],
    border_focused: OutlineColor([0xA4, 0xDA, 0xFF, 0xFF]),
    border_idle: OutlineColor([0x41, 0x48, 0x68, 0xFF]),
    ansi: [
        [0x1D, 0x20, 0x2F], // 0 black
        [0xF7, 0x76, 0x8E], // 1 red
        [0x9E, 0xCE, 0x6A], // 2 green
        [0xE0, 0xAF, 0x68], // 3 yellow
        [0x7A, 0xA2, 0xF7], // 4 blue
        [0xBB, 0x9A, 0xF7], // 5 magenta
        [0x7D, 0xCF, 0xFF], // 6 cyan
        [0xA9, 0xB1, 0xD6], // 7 white
        [0x41, 0x48, 0x68], // 8 black
        [0xFF, 0x89, 0x9D], // 9 red
        [0x9F, 0xE0, 0x44], // 10 green
        [0xFA, 0xBA, 0x4A], // 11 yellow
        [0x8D, 0xB0, 0xFF], // 12 blue
        [0xC7, 0xA9, 0xFF], // 13 magenta
        [0xA4, 0xDA, 0xFF], // 14 cyan
        [0xC0, 0xCA, 0xF5], // 15 white
    ],
};

/// Preset `tokyo-night-day` (light).
///
/// Origin: Tokyo Night Day (folke). Values reproduced from <https://github.com/folke/tokyonight.nvim> (Apache-2.0),
/// canonical well-known export (kitty).
pub static TOKYO_NIGHT_DAY: Theme = Theme {
    name: "tokyo-night-day",
    category: ThemeCategory::Light,
    aliases: &["tokyonight-day"],
    source: "https://github.com/folke/tokyonight.nvim",
    license: "Apache-2.0",
    background: [0xE1, 0xE2, 0xE7],
    foreground: [0x37, 0x60, 0xBF],
    cursor: [0x37, 0x60, 0xBF],
    selection: [0xB7, 0xC1, 0xE3],
    border_focused: OutlineColor([0x00, 0x71, 0x97, 0xFF]),
    border_idle: OutlineColor([0xA1, 0xA6, 0xC5, 0xFF]),
    ansi: [
        [0xB4, 0xB5, 0xB9], // 0 black
        [0xF5, 0x2A, 0x65], // 1 red
        [0x58, 0x75, 0x39], // 2 green
        [0x8C, 0x6C, 0x3E], // 3 yellow
        [0x2E, 0x7D, 0xE9], // 4 blue
        [0x98, 0x54, 0xF1], // 5 magenta
        [0x00, 0x71, 0x97], // 6 cyan
        [0x61, 0x72, 0xB0], // 7 white
        [0xA1, 0xA6, 0xC5], // 8 black
        [0xFF, 0x47, 0x74], // 9 red
        [0x5C, 0x85, 0x24], // 10 green
        [0xA2, 0x76, 0x29], // 11 yellow
        [0x35, 0x8A, 0xFF], // 12 blue
        [0xA4, 0x63, 0xFF], // 13 magenta
        [0x00, 0x7E, 0xA8], // 14 cyan
        [0x37, 0x60, 0xBF], // 15 white
    ],
};

/// Preset `catppuccin-mocha` (dark).
///
/// Origin: Catppuccin Mocha. Values reproduced from <https://github.com/catppuccin/alacritty> (MIT),
/// canonical alacritty export.
pub static CATPPUCCIN_MOCHA: Theme = Theme {
    name: "catppuccin-mocha",
    category: ThemeCategory::Dark,
    aliases: &["catppuccin"],
    source: "https://github.com/catppuccin/alacritty",
    license: "MIT",
    background: [0x1E, 0x1E, 0x2E],
    foreground: [0xCD, 0xD6, 0xF4],
    cursor: [0xF5, 0xE0, 0xDC],
    selection: [0xF5, 0xE0, 0xDC],
    border_focused: OutlineColor([0xA6, 0xE3, 0xA1, 0xFF]),
    border_idle: OutlineColor([0x58, 0x5B, 0x70, 0xFF]),
    ansi: [
        [0x45, 0x47, 0x5A], // 0 black
        [0xF3, 0x8B, 0xA8], // 1 red
        [0xA6, 0xE3, 0xA1], // 2 green
        [0xF9, 0xE2, 0xAF], // 3 yellow
        [0x89, 0xB4, 0xFA], // 4 blue
        [0xF5, 0xC2, 0xE7], // 5 magenta
        [0x94, 0xE2, 0xD5], // 6 cyan
        [0xBA, 0xC2, 0xDE], // 7 white
        [0x58, 0x5B, 0x70], // 8 black
        [0xF3, 0x8B, 0xA8], // 9 red
        [0xA6, 0xE3, 0xA1], // 10 green
        [0xF9, 0xE2, 0xAF], // 11 yellow
        [0x89, 0xB4, 0xFA], // 12 blue
        [0xF5, 0xC2, 0xE7], // 13 magenta
        [0x94, 0xE2, 0xD5], // 14 cyan
        [0xA6, 0xAD, 0xC8], // 15 white
    ],
};

/// Preset `catppuccin-macchiato` (dark).
///
/// Origin: Catppuccin Macchiato. Values reproduced from <https://github.com/catppuccin/alacritty> (MIT),
/// canonical alacritty export.
pub static CATPPUCCIN_MACCHIATO: Theme = Theme {
    name: "catppuccin-macchiato",
    category: ThemeCategory::Dark,
    aliases: &[],
    source: "https://github.com/catppuccin/alacritty",
    license: "MIT",
    background: [0x24, 0x27, 0x3A],
    foreground: [0xCA, 0xD3, 0xF5],
    cursor: [0xF4, 0xDB, 0xD6],
    selection: [0xF4, 0xDB, 0xD6],
    border_focused: OutlineColor([0xF5, 0xBD, 0xE6, 0xFF]),
    border_idle: OutlineColor([0x5B, 0x60, 0x78, 0xFF]),
    ansi: [
        [0x49, 0x4D, 0x64], // 0 black
        [0xED, 0x87, 0x96], // 1 red
        [0xA6, 0xDA, 0x95], // 2 green
        [0xEE, 0xD4, 0x9F], // 3 yellow
        [0x8A, 0xAD, 0xF4], // 4 blue
        [0xF5, 0xBD, 0xE6], // 5 magenta
        [0x8B, 0xD5, 0xCA], // 6 cyan
        [0xB8, 0xC0, 0xE0], // 7 white
        [0x5B, 0x60, 0x78], // 8 black
        [0xED, 0x87, 0x96], // 9 red
        [0xA6, 0xDA, 0x95], // 10 green
        [0xEE, 0xD4, 0x9F], // 11 yellow
        [0x8A, 0xAD, 0xF4], // 12 blue
        [0xF5, 0xBD, 0xE6], // 13 magenta
        [0x8B, 0xD5, 0xCA], // 14 cyan
        [0xA5, 0xAD, 0xCB], // 15 white
    ],
};

/// Preset `catppuccin-frappe` (dark).
///
/// Origin: Catppuccin Frappe. Values reproduced from <https://github.com/catppuccin/alacritty> (MIT),
/// canonical alacritty export.
pub static CATPPUCCIN_FRAPPE: Theme = Theme {
    name: "catppuccin-frappe",
    category: ThemeCategory::Dark,
    aliases: &[],
    source: "https://github.com/catppuccin/alacritty",
    license: "MIT",
    background: [0x30, 0x34, 0x46],
    foreground: [0xC6, 0xD0, 0xF5],
    cursor: [0xF2, 0xD5, 0xCF],
    selection: [0xF2, 0xD5, 0xCF],
    border_focused: OutlineColor([0xF4, 0xB8, 0xE4, 0xFF]),
    border_idle: OutlineColor([0x62, 0x68, 0x80, 0xFF]),
    ansi: [
        [0x51, 0x57, 0x6D], // 0 black
        [0xE7, 0x82, 0x84], // 1 red
        [0xA6, 0xD1, 0x89], // 2 green
        [0xE5, 0xC8, 0x90], // 3 yellow
        [0x8C, 0xAA, 0xEE], // 4 blue
        [0xF4, 0xB8, 0xE4], // 5 magenta
        [0x81, 0xC8, 0xBE], // 6 cyan
        [0xB5, 0xBF, 0xE2], // 7 white
        [0x62, 0x68, 0x80], // 8 black
        [0xE7, 0x82, 0x84], // 9 red
        [0xA6, 0xD1, 0x89], // 10 green
        [0xE5, 0xC8, 0x90], // 11 yellow
        [0x8C, 0xAA, 0xEE], // 12 blue
        [0xF4, 0xB8, 0xE4], // 13 magenta
        [0x81, 0xC8, 0xBE], // 14 cyan
        [0xA5, 0xAD, 0xCE], // 15 white
    ],
};

/// Preset `catppuccin-latte` (light).
///
/// Origin: Catppuccin Latte. Values reproduced from <https://github.com/catppuccin/alacritty> (MIT),
/// canonical alacritty export.
pub static CATPPUCCIN_LATTE: Theme = Theme {
    name: "catppuccin-latte",
    category: ThemeCategory::Light,
    aliases: &[],
    source: "https://github.com/catppuccin/alacritty",
    license: "MIT",
    background: [0xEF, 0xF1, 0xF5],
    foreground: [0x4C, 0x4F, 0x69],
    cursor: [0xDC, 0x8A, 0x78],
    selection: [0xDC, 0x8A, 0x78],
    border_focused: OutlineColor([0x1E, 0x66, 0xF5, 0xFF]),
    border_idle: OutlineColor([0xAC, 0xB0, 0xBE, 0xFF]),
    ansi: [
        [0xBC, 0xC0, 0xCC], // 0 black
        [0xD2, 0x0F, 0x39], // 1 red
        [0x40, 0xA0, 0x2B], // 2 green
        [0xDF, 0x8E, 0x1D], // 3 yellow
        [0x1E, 0x66, 0xF5], // 4 blue
        [0xEA, 0x76, 0xCB], // 5 magenta
        [0x17, 0x92, 0x99], // 6 cyan
        [0x5C, 0x5F, 0x77], // 7 white
        [0xAC, 0xB0, 0xBE], // 8 black
        [0xD2, 0x0F, 0x39], // 9 red
        [0x40, 0xA0, 0x2B], // 10 green
        [0xDF, 0x8E, 0x1D], // 11 yellow
        [0x1E, 0x66, 0xF5], // 12 blue
        [0xEA, 0x76, 0xCB], // 13 magenta
        [0x17, 0x92, 0x99], // 14 cyan
        [0x6C, 0x6F, 0x85], // 15 white
    ],
};

/// Preset `dracula` (dark).
///
/// Origin: Dracula. Values reproduced from <https://github.com/dracula/alacritty> (MIT),
/// canonical alacritty export.
pub static DRACULA: Theme = Theme {
    name: "dracula",
    category: ThemeCategory::Dark,
    aliases: &[],
    source: "https://github.com/dracula/alacritty",
    license: "MIT",
    background: [0x28, 0x2A, 0x36],
    foreground: [0xF8, 0xF8, 0xF2],
    cursor: [0xF8, 0xF8, 0xF2],
    selection: [0x44, 0x47, 0x5A],
    border_focused: OutlineColor([0xA4, 0xFF, 0xFF, 0xFF]),
    border_idle: OutlineColor([0x62, 0x72, 0xA4, 0xFF]),
    ansi: [
        [0x21, 0x22, 0x2C], // 0 black
        [0xFF, 0x55, 0x55], // 1 red
        [0x50, 0xFA, 0x7B], // 2 green
        [0xF1, 0xFA, 0x8C], // 3 yellow
        [0xBD, 0x93, 0xF9], // 4 blue
        [0xFF, 0x79, 0xC6], // 5 magenta
        [0x8B, 0xE9, 0xFD], // 6 cyan
        [0xF8, 0xF8, 0xF2], // 7 white
        [0x62, 0x72, 0xA4], // 8 black
        [0xFF, 0x6E, 0x6E], // 9 red
        [0x69, 0xFF, 0x94], // 10 green
        [0xFF, 0xFF, 0xA5], // 11 yellow
        [0xD6, 0xAC, 0xFF], // 12 blue
        [0xFF, 0x92, 0xDF], // 13 magenta
        [0xA4, 0xFF, 0xFF], // 14 cyan
        [0xFF, 0xFF, 0xFF], // 15 white
    ],
};

/// Preset `nord` (dark).
///
/// Origin: Nord (Arctic Ice Studio). Values reproduced from <https://github.com/nordtheme/nord> (MIT),
/// iTerm2-Color-Schemes export.
pub static NORD: Theme = Theme {
    name: "nord",
    category: ThemeCategory::Dark,
    aliases: &[],
    source: "https://github.com/nordtheme/nord",
    license: "MIT",
    background: [0x2E, 0x34, 0x40],
    foreground: [0xD8, 0xDE, 0xE9],
    cursor: [0xEC, 0xEF, 0xF4],
    selection: [0xEC, 0xEF, 0xF4],
    border_focused: OutlineColor([0x88, 0xC0, 0xD0, 0xFF]),
    border_idle: OutlineColor([0x59, 0x63, 0x77, 0xFF]),
    ansi: [
        [0x3B, 0x42, 0x52], // 0 black
        [0xBF, 0x61, 0x6A], // 1 red
        [0xA3, 0xBE, 0x8C], // 2 green
        [0xEB, 0xCB, 0x8B], // 3 yellow
        [0x81, 0xA1, 0xC1], // 4 blue
        [0xB4, 0x8E, 0xAD], // 5 magenta
        [0x88, 0xC0, 0xD0], // 6 cyan
        [0xE5, 0xE9, 0xF0], // 7 white
        [0x59, 0x63, 0x77], // 8 black
        [0xBF, 0x61, 0x6A], // 9 red
        [0xA3, 0xBE, 0x8C], // 10 green
        [0xEB, 0xCB, 0x8B], // 11 yellow
        [0x81, 0xA1, 0xC1], // 12 blue
        [0xB4, 0x8E, 0xAD], // 13 magenta
        [0x8F, 0xBC, 0xBB], // 14 cyan
        [0xEC, 0xEF, 0xF4], // 15 white
    ],
};

/// Preset `gruvbox-dark` (dark).
///
/// Origin: Gruvbox Dark (Pavel Pertsev). Values reproduced from <https://github.com/morhetz/gruvbox> (MIT),
/// iTerm2-Color-Schemes export.
pub static GRUVBOX_DARK: Theme = Theme {
    name: "gruvbox-dark",
    category: ThemeCategory::Dark,
    aliases: &["gruvbox"],
    source: "https://github.com/morhetz/gruvbox",
    license: "MIT",
    background: [0x28, 0x28, 0x28],
    foreground: [0xEB, 0xDB, 0xB2],
    cursor: [0xEB, 0xDB, 0xB2],
    selection: [0x66, 0x5C, 0x54],
    border_focused: OutlineColor([0xB8, 0xBB, 0x26, 0xFF]),
    border_idle: OutlineColor([0x92, 0x83, 0x74, 0xFF]),
    ansi: [
        [0x28, 0x28, 0x28], // 0 black
        [0xCC, 0x24, 0x1D], // 1 red
        [0x98, 0x97, 0x1A], // 2 green
        [0xD7, 0x99, 0x21], // 3 yellow
        [0x45, 0x85, 0x88], // 4 blue
        [0xB1, 0x62, 0x86], // 5 magenta
        [0x68, 0x9D, 0x6A], // 6 cyan
        [0xA8, 0x99, 0x84], // 7 white
        [0x92, 0x83, 0x74], // 8 black
        [0xFB, 0x49, 0x34], // 9 red
        [0xB8, 0xBB, 0x26], // 10 green
        [0xFA, 0xBD, 0x2F], // 11 yellow
        [0x83, 0xA5, 0x98], // 12 blue
        [0xD3, 0x86, 0x9B], // 13 magenta
        [0x8E, 0xC0, 0x7C], // 14 cyan
        [0xEB, 0xDB, 0xB2], // 15 white
    ],
};

/// Preset `gruvbox-light` (light).
///
/// Origin: Gruvbox Light (Pavel Pertsev). Values reproduced from <https://github.com/morhetz/gruvbox> (MIT),
/// iTerm2-Color-Schemes export.
pub static GRUVBOX_LIGHT: Theme = Theme {
    name: "gruvbox-light",
    category: ThemeCategory::Light,
    aliases: &[],
    source: "https://github.com/morhetz/gruvbox",
    license: "MIT",
    background: [0xFB, 0xF1, 0xC7],
    foreground: [0x3C, 0x38, 0x36],
    cursor: [0x3C, 0x38, 0x36],
    selection: [0x3C, 0x38, 0x36],
    border_focused: OutlineColor([0x8F, 0x3F, 0x71, 0xFF]),
    border_idle: OutlineColor([0x92, 0x83, 0x74, 0xFF]),
    ansi: [
        [0xFB, 0xF1, 0xC7], // 0 black
        [0xCC, 0x24, 0x1D], // 1 red
        [0x98, 0x97, 0x1A], // 2 green
        [0xD7, 0x99, 0x21], // 3 yellow
        [0x45, 0x85, 0x88], // 4 blue
        [0xB1, 0x62, 0x86], // 5 magenta
        [0x68, 0x9D, 0x6A], // 6 cyan
        [0x7C, 0x6F, 0x64], // 7 white
        [0x92, 0x83, 0x74], // 8 black
        [0x9D, 0x00, 0x06], // 9 red
        [0x79, 0x74, 0x0E], // 10 green
        [0xB5, 0x76, 0x14], // 11 yellow
        [0x07, 0x66, 0x78], // 12 blue
        [0x8F, 0x3F, 0x71], // 13 magenta
        [0x42, 0x7B, 0x58], // 14 cyan
        [0x3C, 0x38, 0x36], // 15 white
    ],
};

/// Preset `solarized-dark` (dark).
///
/// Origin: Solarized Dark (Ethan Schoonover). Values reproduced from <https://github.com/altercation/solarized> (MIT),
/// iTerm2-Color-Schemes export.
pub static SOLARIZED_DARK: Theme = Theme {
    name: "solarized-dark",
    category: ThemeCategory::Dark,
    aliases: &["solarized"],
    source: "https://github.com/altercation/solarized",
    license: "MIT",
    background: [0x00, 0x2B, 0x36],
    foreground: [0x83, 0x94, 0x96],
    cursor: [0x83, 0x94, 0x96],
    selection: [0x07, 0x36, 0x42],
    border_focused: OutlineColor([0x93, 0xA1, 0xA1, 0xFF]),
    border_idle: OutlineColor([0x33, 0x5E, 0x69, 0xFF]),
    ansi: [
        [0x07, 0x36, 0x42], // 0 black
        [0xDC, 0x32, 0x2F], // 1 red
        [0x85, 0x99, 0x00], // 2 green
        [0xB5, 0x89, 0x00], // 3 yellow
        [0x26, 0x8B, 0xD2], // 4 blue
        [0xD3, 0x36, 0x82], // 5 magenta
        [0x2A, 0xA1, 0x98], // 6 cyan
        [0xEE, 0xE8, 0xD5], // 7 white
        [0x33, 0x5E, 0x69], // 8 black
        [0xCB, 0x4B, 0x16], // 9 red
        [0x58, 0x6E, 0x75], // 10 green
        [0x65, 0x7B, 0x83], // 11 yellow
        [0x83, 0x94, 0x96], // 12 blue
        [0x6C, 0x71, 0xC4], // 13 magenta
        [0x93, 0xA1, 0xA1], // 14 cyan
        [0xFD, 0xF6, 0xE3], // 15 white
    ],
};

/// Preset `solarized-light` (light).
///
/// Origin: Solarized Light (Ethan Schoonover). Values reproduced from <https://github.com/altercation/solarized> (MIT),
/// iTerm2-Color-Schemes export.
pub static SOLARIZED_LIGHT: Theme = Theme {
    name: "solarized-light",
    category: ThemeCategory::Light,
    aliases: &[],
    source: "https://github.com/altercation/solarized",
    license: "MIT",
    background: [0xFD, 0xF6, 0xE3],
    foreground: [0x65, 0x7B, 0x83],
    cursor: [0x65, 0x7B, 0x83],
    selection: [0xEE, 0xE8, 0xD5],
    border_focused: OutlineColor([0x58, 0x6E, 0x75, 0xFF]),
    border_idle: OutlineColor([0x00, 0x2B, 0x36, 0xFF]),
    ansi: [
        [0x07, 0x36, 0x42], // 0 black
        [0xDC, 0x32, 0x2F], // 1 red
        [0x85, 0x99, 0x00], // 2 green
        [0xB5, 0x89, 0x00], // 3 yellow
        [0x26, 0x8B, 0xD2], // 4 blue
        [0xD3, 0x36, 0x82], // 5 magenta
        [0x2A, 0xA1, 0x98], // 6 cyan
        [0xBB, 0xB5, 0xA2], // 7 white
        [0x00, 0x2B, 0x36], // 8 black
        [0xCB, 0x4B, 0x16], // 9 red
        [0x58, 0x6E, 0x75], // 10 green
        [0x65, 0x7B, 0x83], // 11 yellow
        [0x83, 0x94, 0x96], // 12 blue
        [0x6C, 0x71, 0xC4], // 13 magenta
        [0x93, 0xA1, 0xA1], // 14 cyan
        [0xFD, 0xF6, 0xE3], // 15 white
    ],
};

/// Preset `one-dark` (dark).
///
/// Origin: Atom One Dark. Values reproduced from <https://github.com/atom/one-dark-syntax> (MIT),
/// iTerm2-Color-Schemes export.
pub static ONE_DARK: Theme = Theme {
    name: "one-dark",
    category: ThemeCategory::Dark,
    aliases: &["onedark"],
    source: "https://github.com/atom/one-dark-syntax",
    license: "MIT",
    background: [0x21, 0x25, 0x2B],
    foreground: [0xAB, 0xB2, 0xBF],
    cursor: [0xAB, 0xB2, 0xBF],
    selection: [0x32, 0x38, 0x44],
    border_focused: OutlineColor([0x98, 0xC3, 0x79, 0xFF]),
    border_idle: OutlineColor([0x76, 0x76, 0x76, 0xFF]),
    ansi: [
        [0x21, 0x25, 0x2B], // 0 black
        [0xE0, 0x6C, 0x75], // 1 red
        [0x98, 0xC3, 0x79], // 2 green
        [0xE5, 0xC0, 0x7B], // 3 yellow
        [0x61, 0xAF, 0xEF], // 4 blue
        [0xC6, 0x78, 0xDD], // 5 magenta
        [0x56, 0xB6, 0xC2], // 6 cyan
        [0xAB, 0xB2, 0xBF], // 7 white
        [0x76, 0x76, 0x76], // 8 black
        [0xE0, 0x6C, 0x75], // 9 red
        [0x98, 0xC3, 0x79], // 10 green
        [0xE5, 0xC0, 0x7B], // 11 yellow
        [0x61, 0xAF, 0xEF], // 12 blue
        [0xC6, 0x78, 0xDD], // 13 magenta
        [0x56, 0xB6, 0xC2], // 14 cyan
        [0xAB, 0xB2, 0xBF], // 15 white
    ],
};

/// Preset `one-light` (light).
///
/// Origin: Atom One Light. Values reproduced from <https://github.com/atom/one-light-syntax> (MIT),
/// iTerm2-Color-Schemes export.
pub static ONE_LIGHT: Theme = Theme {
    name: "one-light",
    category: ThemeCategory::Light,
    aliases: &["onelight"],
    source: "https://github.com/atom/one-light-syntax",
    license: "MIT",
    background: [0xF9, 0xF9, 0xF9],
    foreground: [0x2A, 0x2C, 0x33],
    cursor: [0xBB, 0xBB, 0xBB],
    selection: [0xED, 0xED, 0xED],
    border_focused: OutlineColor([0x95, 0x00, 0x95, 0xFF]),
    border_idle: OutlineColor([0x00, 0x00, 0x00, 0xFF]),
    ansi: [
        [0x00, 0x00, 0x00], // 0 black
        [0xDE, 0x3E, 0x35], // 1 red
        [0x3F, 0x95, 0x3A], // 2 green
        [0xD2, 0xB6, 0x7C], // 3 yellow
        [0x2F, 0x5A, 0xF3], // 4 blue
        [0x95, 0x00, 0x95], // 5 magenta
        [0x3F, 0x95, 0x3A], // 6 cyan
        [0xBB, 0xBB, 0xBB], // 7 white
        [0x00, 0x00, 0x00], // 8 black
        [0xDE, 0x3E, 0x35], // 9 red
        [0x3F, 0x95, 0x3A], // 10 green
        [0xD2, 0xB6, 0x7C], // 11 yellow
        [0x2F, 0x5A, 0xF3], // 12 blue
        [0xA0, 0x00, 0x95], // 13 magenta
        [0x3F, 0x95, 0x3A], // 14 cyan
        [0xFF, 0xFF, 0xFF], // 15 white
    ],
};

/// Preset `ayu-dark` (dark).
///
/// Origin: Ayu Dark (Demizied). Values reproduced from <https://github.com/ayu-theme/ayu-colors> (MIT),
/// iTerm2-Color-Schemes export.
pub static AYU_DARK: Theme = Theme {
    name: "ayu-dark",
    category: ThemeCategory::Dark,
    aliases: &["ayu"],
    source: "https://github.com/ayu-theme/ayu-colors",
    license: "MIT",
    background: [0x0B, 0x0E, 0x14],
    foreground: [0xBF, 0xBD, 0xB6],
    cursor: [0xE6, 0xB4, 0x50],
    selection: [0x40, 0x9F, 0xFF],
    border_focused: OutlineColor([0x95, 0xE6, 0xCB, 0xFF]),
    border_idle: OutlineColor([0x68, 0x68, 0x68, 0xFF]),
    ansi: [
        [0x11, 0x15, 0x1C], // 0 black
        [0xEA, 0x6C, 0x73], // 1 red
        [0x7F, 0xD9, 0x62], // 2 green
        [0xF9, 0xAF, 0x4F], // 3 yellow
        [0x53, 0xBD, 0xFA], // 4 blue
        [0xCD, 0xA1, 0xFA], // 5 magenta
        [0x90, 0xE1, 0xC6], // 6 cyan
        [0xC7, 0xC7, 0xC7], // 7 white
        [0x68, 0x68, 0x68], // 8 black
        [0xF0, 0x71, 0x78], // 9 red
        [0xAA, 0xD9, 0x4C], // 10 green
        [0xFF, 0xB4, 0x54], // 11 yellow
        [0x59, 0xC2, 0xFF], // 12 blue
        [0xD2, 0xA6, 0xFF], // 13 magenta
        [0x95, 0xE6, 0xCB], // 14 cyan
        [0xFF, 0xFF, 0xFF], // 15 white
    ],
};

/// Preset `ayu-mirage` (dark).
///
/// Origin: Ayu Mirage (Demizied). Values reproduced from <https://github.com/ayu-theme/ayu-colors> (MIT),
/// iTerm2-Color-Schemes export.
pub static AYU_MIRAGE: Theme = Theme {
    name: "ayu-mirage",
    category: ThemeCategory::Dark,
    aliases: &[],
    source: "https://github.com/ayu-theme/ayu-colors",
    license: "MIT",
    background: [0x1F, 0x24, 0x30],
    foreground: [0xCC, 0xCA, 0xC2],
    cursor: [0xFF, 0xCC, 0x66],
    selection: [0x40, 0x9F, 0xFF],
    border_focused: OutlineColor([0xD5, 0xFF, 0x80, 0xFF]),
    border_idle: OutlineColor([0x68, 0x68, 0x68, 0xFF]),
    ansi: [
        [0x17, 0x1B, 0x24], // 0 black
        [0xED, 0x82, 0x74], // 1 red
        [0x87, 0xD9, 0x6C], // 2 green
        [0xFA, 0xCC, 0x6E], // 3 yellow
        [0x6D, 0xCB, 0xFA], // 4 blue
        [0xDA, 0xBA, 0xFA], // 5 magenta
        [0x90, 0xE1, 0xC6], // 6 cyan
        [0xC7, 0xC7, 0xC7], // 7 white
        [0x68, 0x68, 0x68], // 8 black
        [0xF2, 0x87, 0x79], // 9 red
        [0xD5, 0xFF, 0x80], // 10 green
        [0xFF, 0xD1, 0x73], // 11 yellow
        [0x73, 0xD0, 0xFF], // 12 blue
        [0xDF, 0xBF, 0xFF], // 13 magenta
        [0x95, 0xE6, 0xCB], // 14 cyan
        [0xFF, 0xFF, 0xFF], // 15 white
    ],
};

/// Preset `ayu-light` (light).
///
/// Origin: Ayu Light (Demizied). Values reproduced from <https://github.com/ayu-theme/ayu-colors> (MIT),
/// iTerm2-Color-Schemes export.
pub static AYU_LIGHT: Theme = Theme {
    name: "ayu-light",
    category: ThemeCategory::Light,
    aliases: &[],
    source: "https://github.com/ayu-theme/ayu-colors",
    license: "MIT",
    background: [0xF8, 0xF9, 0xFA],
    foreground: [0x5C, 0x61, 0x66],
    cursor: [0xFF, 0xAA, 0x33],
    selection: [0x03, 0x5B, 0xD6],
    border_focused: OutlineColor([0x9E, 0x75, 0xC7, 0xFF]),
    border_idle: OutlineColor([0x68, 0x68, 0x68, 0xFF]),
    ansi: [
        [0x00, 0x00, 0x00], // 0 black
        [0xEA, 0x6C, 0x6D], // 1 red
        [0x6C, 0xBF, 0x43], // 2 green
        [0xEC, 0xA9, 0x44], // 3 yellow
        [0x31, 0x99, 0xE1], // 4 blue
        [0x9E, 0x75, 0xC7], // 5 magenta
        [0x46, 0xBA, 0x94], // 6 cyan
        [0xBA, 0xBA, 0xBA], // 7 white
        [0x68, 0x68, 0x68], // 8 black
        [0xF0, 0x71, 0x71], // 9 red
        [0x86, 0xB3, 0x00], // 10 green
        [0xF2, 0xAE, 0x49], // 11 yellow
        [0x39, 0x9E, 0xE6], // 12 blue
        [0xA3, 0x7A, 0xCC], // 13 magenta
        [0x4C, 0xBF, 0x99], // 14 cyan
        [0xD1, 0xD1, 0xD1], // 15 white
    ],
};

/// Preset `kanagawa-wave` (dark).
///
/// Origin: Kanagawa Wave (Tommaso Laurenzi). Values reproduced from <https://github.com/rebelot/kanagawa.nvim> (MIT),
/// canonical well-known export (kitty).
pub static KANAGAWA_WAVE: Theme = Theme {
    name: "kanagawa-wave",
    category: ThemeCategory::Dark,
    aliases: &["kanagawa"],
    source: "https://github.com/rebelot/kanagawa.nvim",
    license: "MIT",
    background: [0x1F, 0x1F, 0x28],
    foreground: [0xDC, 0xD7, 0xBA],
    cursor: [0xC8, 0xC0, 0x93],
    selection: [0x2D, 0x4F, 0x67],
    border_focused: OutlineColor([0x98, 0xBB, 0x6C, 0xFF]),
    border_idle: OutlineColor([0x72, 0x71, 0x69, 0xFF]),
    ansi: [
        [0x16, 0x16, 0x1D], // 0 black
        [0xC3, 0x40, 0x43], // 1 red
        [0x76, 0x94, 0x6A], // 2 green
        [0xC0, 0xA3, 0x6E], // 3 yellow
        [0x7E, 0x9C, 0xD8], // 4 blue
        [0x95, 0x7F, 0xB8], // 5 magenta
        [0x6A, 0x95, 0x89], // 6 cyan
        [0xC8, 0xC0, 0x93], // 7 white
        [0x72, 0x71, 0x69], // 8 black
        [0xE8, 0x24, 0x24], // 9 red
        [0x98, 0xBB, 0x6C], // 10 green
        [0xE6, 0xC3, 0x84], // 11 yellow
        [0x7F, 0xB4, 0xCA], // 12 blue
        [0x93, 0x8A, 0xA9], // 13 magenta
        [0x7A, 0xA8, 0x9F], // 14 cyan
        [0xDC, 0xD7, 0xBA], // 15 white
    ],
};

/// Preset `kanagawa-lotus` (light).
///
/// Origin: Kanagawa Lotus (Tommaso Laurenzi). Values reproduced from <https://github.com/rebelot/kanagawa.nvim> (MIT),
/// canonical well-known export (kitty).
pub static KANAGAWA_LOTUS: Theme = Theme {
    name: "kanagawa-lotus",
    category: ThemeCategory::Light,
    aliases: &[],
    source: "https://github.com/rebelot/kanagawa.nvim",
    license: "MIT",
    background: [0xF2, 0xEC, 0xBC],
    foreground: [0x54, 0x54, 0x64],
    cursor: [0x43, 0x43, 0x6C],
    selection: [0xC9, 0xCB, 0xD1],
    border_focused: OutlineColor([0x62, 0x4C, 0x83, 0xFF]),
    border_idle: OutlineColor([0x8A, 0x89, 0x80, 0xFF]),
    ansi: [
        [0x1F, 0x1F, 0x28], // 0 black
        [0xC8, 0x40, 0x53], // 1 red
        [0x6F, 0x89, 0x4E], // 2 green
        [0x77, 0x71, 0x3F], // 3 yellow
        [0x4D, 0x69, 0x9B], // 4 blue
        [0xB3, 0x5B, 0x79], // 5 magenta
        [0x59, 0x7B, 0x75], // 6 cyan
        [0x54, 0x54, 0x64], // 7 white
        [0x8A, 0x89, 0x80], // 8 black
        [0xD7, 0x47, 0x4B], // 9 red
        [0x6E, 0x91, 0x5F], // 10 green
        [0x83, 0x6F, 0x4A], // 11 yellow
        [0x66, 0x93, 0xBF], // 12 blue
        [0x62, 0x4C, 0x83], // 13 magenta
        [0x5E, 0x85, 0x7A], // 14 cyan
        [0x43, 0x43, 0x6C], // 15 white
    ],
};

/// Preset `rose-pine` (dark).
///
/// Origin: Rose Pine. Values reproduced from <https://github.com/rose-pine/rose-pine-theme> (MIT),
/// rose-pine/alacritty export.
pub static ROSE_PINE: Theme = Theme {
    name: "rose-pine",
    category: ThemeCategory::Dark,
    aliases: &["rosepine", "rose-pine-main"],
    source: "https://github.com/rose-pine/rose-pine-theme",
    license: "MIT",
    background: [0x19, 0x17, 0x24],
    foreground: [0xE0, 0xDE, 0xF4],
    cursor: [0x52, 0x4F, 0x67],
    selection: [0x40, 0x3D, 0x52],
    border_focused: OutlineColor([0xEB, 0xBC, 0xBA, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x6A, 0x86, 0xFF]),
    ansi: [
        [0x26, 0x23, 0x3A], // 0 black
        [0xEB, 0x6F, 0x92], // 1 red
        [0x31, 0x74, 0x8F], // 2 green
        [0xF6, 0xC1, 0x77], // 3 yellow
        [0x9C, 0xCF, 0xD8], // 4 blue
        [0xC4, 0xA7, 0xE7], // 5 magenta
        [0xEB, 0xBC, 0xBA], // 6 cyan
        [0xE0, 0xDE, 0xF4], // 7 white
        [0x6E, 0x6A, 0x86], // 8 black
        [0xEB, 0x6F, 0x92], // 9 red
        [0x31, 0x74, 0x8F], // 10 green
        [0xF6, 0xC1, 0x77], // 11 yellow
        [0x9C, 0xCF, 0xD8], // 12 blue
        [0xC4, 0xA7, 0xE7], // 13 magenta
        [0xEB, 0xBC, 0xBA], // 14 cyan
        [0xE0, 0xDE, 0xF4], // 15 white
    ],
};

/// Preset `rose-pine-moon` (dark).
///
/// Origin: Rose Pine Moon. Values reproduced from <https://github.com/rose-pine/rose-pine-theme> (MIT),
/// rose-pine/alacritty export.
pub static ROSE_PINE_MOON: Theme = Theme {
    name: "rose-pine-moon",
    category: ThemeCategory::Dark,
    aliases: &["rosepine-moon"],
    source: "https://github.com/rose-pine/rose-pine-theme",
    license: "MIT",
    background: [0x23, 0x21, 0x36],
    foreground: [0xE0, 0xDE, 0xF4],
    cursor: [0x56, 0x52, 0x6E],
    selection: [0x44, 0x41, 0x5A],
    border_focused: OutlineColor([0x9C, 0xCF, 0xD8, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x6A, 0x86, 0xFF]),
    ansi: [
        [0x39, 0x35, 0x52], // 0 black
        [0xEB, 0x6F, 0x92], // 1 red
        [0x3E, 0x8F, 0xB0], // 2 green
        [0xF6, 0xC1, 0x77], // 3 yellow
        [0x9C, 0xCF, 0xD8], // 4 blue
        [0xC4, 0xA7, 0xE7], // 5 magenta
        [0xEA, 0x9A, 0x97], // 6 cyan
        [0xE0, 0xDE, 0xF4], // 7 white
        [0x6E, 0x6A, 0x86], // 8 black
        [0xEB, 0x6F, 0x92], // 9 red
        [0x3E, 0x8F, 0xB0], // 10 green
        [0xF6, 0xC1, 0x77], // 11 yellow
        [0x9C, 0xCF, 0xD8], // 12 blue
        [0xC4, 0xA7, 0xE7], // 13 magenta
        [0xEA, 0x9A, 0x97], // 14 cyan
        [0xE0, 0xDE, 0xF4], // 15 white
    ],
};

/// Preset `rose-pine-dawn` (light).
///
/// Origin: Rose Pine Dawn. Values reproduced from <https://github.com/rose-pine/rose-pine-theme> (MIT),
/// rose-pine/alacritty export.
pub static ROSE_PINE_DAWN: Theme = Theme {
    name: "rose-pine-dawn",
    category: ThemeCategory::Light,
    aliases: &["rosepine-dawn"],
    source: "https://github.com/rose-pine/rose-pine-theme",
    license: "MIT",
    background: [0xFA, 0xF4, 0xED],
    foreground: [0x57, 0x52, 0x79],
    cursor: [0xCE, 0xCA, 0xCD],
    selection: [0xDF, 0xDA, 0xD9],
    border_focused: OutlineColor([0x28, 0x69, 0x83, 0xFF]),
    border_idle: OutlineColor([0x98, 0x93, 0xA5, 0xFF]),
    ansi: [
        [0xF2, 0xE9, 0xE1], // 0 black
        [0xB4, 0x63, 0x7A], // 1 red
        [0x28, 0x69, 0x83], // 2 green
        [0xEA, 0x9D, 0x34], // 3 yellow
        [0x56, 0x94, 0x9F], // 4 blue
        [0x90, 0x7A, 0xA9], // 5 magenta
        [0xD7, 0x82, 0x7E], // 6 cyan
        [0x57, 0x52, 0x79], // 7 white
        [0x98, 0x93, 0xA5], // 8 black
        [0xB4, 0x63, 0x7A], // 9 red
        [0x28, 0x69, 0x83], // 10 green
        [0xEA, 0x9D, 0x34], // 11 yellow
        [0x56, 0x94, 0x9F], // 12 blue
        [0x90, 0x7A, 0xA9], // 13 magenta
        [0xD7, 0x82, 0x7E], // 14 cyan
        [0x57, 0x52, 0x79], // 15 white
    ],
};

/// Preset `everforest-dark` (dark).
///
/// Origin: Everforest Dark Medium (sainnhe). Values reproduced from <https://github.com/sainnhe/everforest> (MIT),
/// iTerm2-Color-Schemes export.
pub static EVERFOREST_DARK: Theme = Theme {
    name: "everforest-dark",
    category: ThemeCategory::Dark,
    aliases: &["everforest"],
    source: "https://github.com/sainnhe/everforest",
    license: "MIT",
    background: [0x23, 0x2A, 0x2E],
    foreground: [0xD3, 0xC6, 0xAA],
    cursor: [0xE6, 0x98, 0x75],
    selection: [0x54, 0x3A, 0x48],
    border_focused: OutlineColor([0xA7, 0xC0, 0x80, 0xFF]),
    border_idle: OutlineColor([0xA6, 0xB0, 0xA0, 0xFF]),
    ansi: [
        [0x7A, 0x84, 0x78], // 0 black
        [0xE6, 0x7E, 0x80], // 1 red
        [0xA7, 0xC0, 0x80], // 2 green
        [0xDB, 0xBC, 0x7F], // 3 yellow
        [0x7F, 0xBB, 0xB3], // 4 blue
        [0xD6, 0x99, 0xB6], // 5 magenta
        [0x83, 0xC0, 0x92], // 6 cyan
        [0xF2, 0xEF, 0xDF], // 7 white
        [0xA6, 0xB0, 0xA0], // 8 black
        [0xF8, 0x55, 0x52], // 9 red
        [0x8D, 0xA1, 0x01], // 10 green
        [0xDF, 0xA0, 0x00], // 11 yellow
        [0x3A, 0x94, 0xC5], // 12 blue
        [0xDF, 0x69, 0xBA], // 13 magenta
        [0x35, 0xA7, 0x7C], // 14 cyan
        [0xFF, 0xFB, 0xEF], // 15 white
    ],
};

/// Preset `everforest-light` (light).
///
/// Origin: Everforest Light Medium (sainnhe). Values reproduced from <https://github.com/sainnhe/everforest> (MIT),
/// iTerm2-Color-Schemes export.
pub static EVERFOREST_LIGHT: Theme = Theme {
    name: "everforest-light",
    category: ThemeCategory::Light,
    aliases: &[],
    source: "https://github.com/sainnhe/everforest",
    license: "MIT",
    background: [0xEF, 0xEB, 0xD4],
    foreground: [0x5C, 0x6A, 0x72],
    cursor: [0xF5, 0x7D, 0x26],
    selection: [0xEA, 0xED, 0xC8],
    border_focused: OutlineColor([0x7A, 0x84, 0x78, 0xFF]),
    border_idle: OutlineColor([0xA6, 0xB0, 0xA0, 0xFF]),
    ansi: [
        [0x7A, 0x84, 0x78], // 0 black
        [0xE6, 0x7E, 0x80], // 1 red
        [0x9A, 0xB3, 0x73], // 2 green
        [0xC1, 0xA2, 0x66], // 3 yellow
        [0x7F, 0xBB, 0xB3], // 4 blue
        [0xD6, 0x99, 0xB6], // 5 magenta
        [0x83, 0xC0, 0x92], // 6 cyan
        [0xB2, 0xAF, 0x9F], // 7 white
        [0xA6, 0xB0, 0xA0], // 8 black
        [0xF8, 0x55, 0x52], // 9 red
        [0x8D, 0xA1, 0x01], // 10 green
        [0xDF, 0xA0, 0x00], // 11 yellow
        [0x3A, 0x94, 0xC5], // 12 blue
        [0xDF, 0x69, 0xBA], // 13 magenta
        [0x35, 0xA7, 0x7C], // 14 cyan
        [0xFF, 0xFB, 0xEF], // 15 white
    ],
};

/// Preset `monokai` (dark).
///
/// Origin: Monokai Classic (Wimer Hazenberg). Values reproduced from <https://github.com/mbadolato/iTerm2-Color-Schemes> (MIT),
/// original Monokai by Wimer Hazenberg; export via iTerm2-Color-Schemes.
pub static MONOKAI: Theme = Theme {
    name: "monokai",
    category: ThemeCategory::Dark,
    aliases: &["monokai-classic"],
    source: "https://github.com/mbadolato/iTerm2-Color-Schemes",
    license: "MIT",
    background: [0x27, 0x28, 0x22],
    foreground: [0xFD, 0xFF, 0xF1],
    cursor: [0xC0, 0xC1, 0xB5],
    selection: [0x57, 0x58, 0x4F],
    border_focused: OutlineColor([0xA6, 0xE2, 0x2E, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x70, 0x66, 0xFF]),
    ansi: [
        [0x27, 0x28, 0x22], // 0 black
        [0xF9, 0x26, 0x72], // 1 red
        [0xA6, 0xE2, 0x2E], // 2 green
        [0xE6, 0xDB, 0x74], // 3 yellow
        [0xFD, 0x97, 0x1F], // 4 blue
        [0xAE, 0x81, 0xFF], // 5 magenta
        [0x66, 0xD9, 0xEF], // 6 cyan
        [0xFD, 0xFF, 0xF1], // 7 white
        [0x6E, 0x70, 0x66], // 8 black
        [0xF9, 0x26, 0x72], // 9 red
        [0xA6, 0xE2, 0x2E], // 10 green
        [0xE6, 0xDB, 0x74], // 11 yellow
        [0xFD, 0x97, 0x1F], // 12 blue
        [0xAE, 0x81, 0xFF], // 13 magenta
        [0x66, 0xD9, 0xEF], // 14 cyan
        [0xFD, 0xFF, 0xF1], // 15 white
    ],
};

/// Preset `night-owl` (dark).
///
/// Origin: Night Owl (Sarah Drasner). Values reproduced from <https://github.com/sdras/night-owl-vscode-theme> (MIT),
/// iTerm2-Color-Schemes export.
pub static NIGHT_OWL: Theme = Theme {
    name: "night-owl",
    category: ThemeCategory::Dark,
    aliases: &["nightowl"],
    source: "https://github.com/sdras/night-owl-vscode-theme",
    license: "MIT",
    background: [0x01, 0x16, 0x27],
    foreground: [0xD6, 0xDE, 0xEB],
    cursor: [0x7E, 0x57, 0xC2],
    selection: [0x5F, 0x7E, 0x97],
    border_focused: OutlineColor([0x7F, 0xDB, 0xCA, 0xFF]),
    border_idle: OutlineColor([0x57, 0x56, 0x56, 0xFF]),
    ansi: [
        [0x01, 0x16, 0x27], // 0 black
        [0xEF, 0x53, 0x50], // 1 red
        [0x22, 0xDA, 0x6E], // 2 green
        [0xAD, 0xDB, 0x67], // 3 yellow
        [0x82, 0xAA, 0xFF], // 4 blue
        [0xC7, 0x92, 0xEA], // 5 magenta
        [0x21, 0xC7, 0xA8], // 6 cyan
        [0xFF, 0xFF, 0xFF], // 7 white
        [0x57, 0x56, 0x56], // 8 black
        [0xEF, 0x53, 0x50], // 9 red
        [0x22, 0xDA, 0x6E], // 10 green
        [0xFF, 0xEB, 0x95], // 11 yellow
        [0x82, 0xAA, 0xFF], // 12 blue
        [0xC7, 0x92, 0xEA], // 13 magenta
        [0x7F, 0xDB, 0xCA], // 14 cyan
        [0xFF, 0xFF, 0xFF], // 15 white
    ],
};

/// Preset `github-dark` (dark).
///
/// Origin: GitHub Dark Default (GitHub Primer). Values reproduced from <https://github.com/primer/github-vscode-theme> (MIT),
/// iTerm2-Color-Schemes export.
pub static GITHUB_DARK: Theme = Theme {
    name: "github-dark",
    category: ThemeCategory::Dark,
    aliases: &["github", "github-dark-default"],
    source: "https://github.com/primer/github-vscode-theme",
    license: "MIT",
    background: [0x0D, 0x11, 0x17],
    foreground: [0xE6, 0xED, 0xF3],
    cursor: [0x2F, 0x81, 0xF7],
    selection: [0xE6, 0xED, 0xF3],
    border_focused: OutlineColor([0x56, 0xD4, 0xDD, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x76, 0x81, 0xFF]),
    ansi: [
        [0x48, 0x4F, 0x58], // 0 black
        [0xFF, 0x7B, 0x72], // 1 red
        [0x3F, 0xB9, 0x50], // 2 green
        [0xD2, 0x99, 0x22], // 3 yellow
        [0x58, 0xA6, 0xFF], // 4 blue
        [0xBC, 0x8C, 0xFF], // 5 magenta
        [0x39, 0xC5, 0xCF], // 6 cyan
        [0xB1, 0xBA, 0xC4], // 7 white
        [0x6E, 0x76, 0x81], // 8 black
        [0xFF, 0xA1, 0x98], // 9 red
        [0x56, 0xD3, 0x64], // 10 green
        [0xE3, 0xB3, 0x41], // 11 yellow
        [0x79, 0xC0, 0xFF], // 12 blue
        [0xD2, 0xA8, 0xFF], // 13 magenta
        [0x56, 0xD4, 0xDD], // 14 cyan
        [0xFF, 0xFF, 0xFF], // 15 white
    ],
};

/// Preset `github-light` (light).
///
/// Origin: GitHub Light Default (GitHub Primer). Values reproduced from <https://github.com/primer/github-vscode-theme> (MIT),
/// iTerm2-Color-Schemes export.
pub static GITHUB_LIGHT: Theme = Theme {
    name: "github-light",
    category: ThemeCategory::Light,
    aliases: &["github-light-default"],
    source: "https://github.com/primer/github-vscode-theme",
    license: "MIT",
    background: [0xFF, 0xFF, 0xFF],
    foreground: [0x1F, 0x23, 0x28],
    cursor: [0x09, 0x69, 0xDA],
    selection: [0x1F, 0x23, 0x28],
    border_focused: OutlineColor([0x11, 0x63, 0x29, 0xFF]),
    border_idle: OutlineColor([0x57, 0x60, 0x6A, 0xFF]),
    ansi: [
        [0x24, 0x29, 0x2F], // 0 black
        [0xCF, 0x22, 0x2E], // 1 red
        [0x11, 0x63, 0x29], // 2 green
        [0x4D, 0x2D, 0x00], // 3 yellow
        [0x09, 0x69, 0xDA], // 4 blue
        [0x82, 0x50, 0xDF], // 5 magenta
        [0x1B, 0x7C, 0x83], // 6 cyan
        [0x6E, 0x77, 0x81], // 7 white
        [0x57, 0x60, 0x6A], // 8 black
        [0xA4, 0x0E, 0x26], // 9 red
        [0x1A, 0x7F, 0x37], // 10 green
        [0x63, 0x3C, 0x01], // 11 yellow
        [0x21, 0x8B, 0xFF], // 12 blue
        [0xA4, 0x75, 0xF9], // 13 magenta
        [0x31, 0x92, 0xAA], // 14 cyan
        [0x8C, 0x95, 0x9F], // 15 white
    ],
};
/// The curated built-in preset catalog, default first.
///
/// Every entry is a `static` so callers get a `&'static Theme` with no
/// allocation. [`resolve_theme_with_status`] searches this slice by
/// normalized name and alias; [`list_presets`] exposes it for enumeration.
pub static ALL_PRESETS: &[&Theme] = &[
    &BITTY_DARK,
    &TOKYO_NIGHT,
    &TOKYO_NIGHT_STORM,
    &TOKYO_NIGHT_DAY,
    &CATPPUCCIN_MOCHA,
    &CATPPUCCIN_MACCHIATO,
    &CATPPUCCIN_FRAPPE,
    &CATPPUCCIN_LATTE,
    &DRACULA,
    &NORD,
    &GRUVBOX_DARK,
    &GRUVBOX_LIGHT,
    &SOLARIZED_DARK,
    &SOLARIZED_LIGHT,
    &ONE_DARK,
    &ONE_LIGHT,
    &AYU_DARK,
    &AYU_MIRAGE,
    &AYU_LIGHT,
    &KANAGAWA_WAVE,
    &KANAGAWA_LOTUS,
    &ROSE_PINE,
    &ROSE_PINE_MOON,
    &ROSE_PINE_DAWN,
    &EVERFOREST_DARK,
    &EVERFOREST_LIGHT,
    &MONOKAI,
    &NIGHT_OWL,
    &GITHUB_DARK,
    &GITHUB_LIGHT,
];

/// Returns the whole curated catalog (default first). Pure and allocation-free.
#[must_use]
pub const fn list_presets() -> &'static [&'static Theme] {
    ALL_PRESETS
}

/// Finds a preset by normalized name or alias. Pure linear scan over the
/// curated catalog (30 entries): the catalog is tiny and static, so a map
/// buys nothing and would need lazy initialization.
fn find_preset(normalized: &str) -> Option<&'static Theme> {
    ALL_PRESETS
        .iter()
        .copied()
        .find(|theme| theme.name == normalized || theme.aliases.contains(&normalized))
}

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
    if trimmed.len() > crate::types::MAX_THEME_LEN {
        return Some(trimmed.to_lowercase());
    }
    Some(trimmed.to_lowercase())
}

/// Resolves an `appearance.theme` identifier to a preset plus how it was
/// reached. Pure function: no I/O, no logging — use [`resolve_theme`] for
/// the logging variant used on the startup path.
///
/// - `None`/empty/whitespace → ([`BITTY_DARK`], [`ThemeResolution::Default`]).
/// - Any built-in registry name or alias, case-insensitive (e.g.
///   `"bitty-dark"`, `"dark"`, `"tokyo-night"`, `"catppuccin"`) →
///   (preset, [`ThemeResolution::Named`]).
/// - Anything else → ([`BITTY_DARK`], [`ThemeResolution::FallbackUnknown`]).
#[must_use]
pub fn resolve_theme_with_status(name: Option<&str>) -> (&'static Theme, ThemeResolution) {
    let Some(normalized) = normalize_theme_name(name) else {
        return (&BITTY_DARK, ThemeResolution::Default);
    };
    match find_preset(&normalized) {
        Some(theme) => (theme, ThemeResolution::Named),
        None => (&BITTY_DARK, ThemeResolution::FallbackUnknown),
    }
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

    /// WCAG relative luminance for one `sRGB` channel triplet.
    fn luminance(rgb: [u8; 3]) -> f64 {
        fn channel(value: u8) -> f64 {
            let scaled = f64::from(value) / 255.0;
            if scaled <= 0.040_45 {
                scaled / 12.92
            } else {
                ((scaled + 0.055) / 1.055).powf(2.4)
            }
        }
        0.212_6 * channel(rgb[0]) + 0.715_2 * channel(rgb[1]) + 0.072_2 * channel(rgb[2])
    }

    /// WCAG contrast ratio between two opaque colors (>= 1.0).
    fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Minimum WCAG AA contrast for normal text.
    const MIN_CONTRAST: f64 = 4.5;

    /// Presets whose upstream palette is intentionally low-contrast and is
    /// therefore exempt from [`MIN_CONTRAST`], with the reason. Keeping the
    /// upstream value faithful is preferable to silently editing a palette;
    /// the exemption is explicit so a new low-contrast preset cannot hide.
    const CONTRAST_EXEMPTIONS: &[(&str, &str)] = &[(
        "solarized-light",
        "Solarized is designed as a low-contrast scheme; foreground #657b83 on \
         background #fdf6e3 is 4.13:1 upstream and is retained verbatim",
    )];

    fn exemption(theme: &Theme) -> Option<&'static str> {
        CONTRAST_EXEMPTIONS
            .iter()
            .find(|(name, _)| *name == theme.name)
            .map(|(_, reason)| *reason)
    }

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
        let (theme, status) = resolve_theme_with_status(Some("definitely-not-a-theme"));
        assert_eq!(status, ThemeResolution::FallbackUnknown);
        assert_eq!(theme.name, DEFAULT_THEME_NAME);
        assert_eq!(theme.background, BITTY_DARK.background);
        // The logging variant agrees on the value.
        let logged = resolve_theme(Some("definitely-not-a-theme"));
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

    // -- catalog invariants (fail-closed) ---------------------------------

    #[test]
    fn catalog_has_expected_depth_and_both_categories() {
        assert_eq!(list_presets().len(), 30, "curated catalog size");
        assert!(
            list_presets()
                .iter()
                .any(|t| t.category == ThemeCategory::Dark),
            "catalog must ship dark presets"
        );
        assert!(
            list_presets()
                .iter()
                .any(|t| t.category == ThemeCategory::Light),
            "catalog must ship light presets"
        );
    }

    #[test]
    fn default_is_first_preset_and_unchanged() {
        let first = list_presets().first().copied().expect("catalog non-empty");
        assert!(std::ptr::eq(first, &BITTY_DARK));
        assert_eq!(first.name, "bitty-dark");
        assert_eq!(first.category, ThemeCategory::Dark);
        assert_eq!(first.aliases, &["dark"]);
        let (alias, status) = resolve_theme_with_status(Some("dark"));
        assert_eq!(status, ThemeResolution::Named);
        assert_eq!(alias.name, "bitty-dark");
    }

    #[test]
    fn names_and_aliases_are_unique_after_normalization() {
        use std::collections::HashMap;
        // `normalize_theme_name` lowercases and trims; apply the same rule so
        // a case/whitespace variant can never shadow an existing key.
        let normalize = |raw: &str| raw.trim().to_lowercase();
        let mut seen: HashMap<String, &str> = HashMap::new();
        for theme in list_presets() {
            for key in std::iter::once(theme.name).chain(theme.aliases.iter().copied()) {
                let key = normalize(key);
                assert!(!key.is_empty(), "preset {} has an empty key", theme.name);
                if key.len() > crate::types::MAX_THEME_LEN {
                    panic!("preset {} key '{key}' exceeds MAX_THEME_LEN", theme.name);
                }
                if let Some(previous) = seen.insert(key.clone(), theme.name) {
                    panic!(
                        "catalog key '{key}' collides: {previous} and {}",
                        theme.name
                    );
                }
            }
        }
        // Every registered key must resolve back to its owning preset.
        for theme in list_presets() {
            for key in std::iter::once(theme.name).chain(theme.aliases.iter().copied()) {
                let (resolved, status) = resolve_theme_with_status(Some(key));
                assert_eq!(status, ThemeResolution::Named, "key {key}");
                assert_eq!(resolved.name, theme.name, "key {key}");
            }
        }
    }

    #[test]
    fn every_preset_has_all_16_ansi_entries() {
        for theme in list_presets() {
            assert_eq!(theme.ansi.len(), 16, "preset {}", theme.name);
            // Touch every slot so a placeholder-filled array is still read.
            let mut seen = std::collections::HashSet::new();
            for index in 0..16u8 {
                let entry = theme.ansi_entry(index);
                assert_eq!(entry, theme.ansi[usize::from(index)]);
                seen.insert(entry);
            }
            // A palette that repeats one color 16 times is almost certainly a
            // transcription error; require at least 8 distinct ANSI swatches.
            assert!(
                seen.len() >= 8,
                "preset {} has only {} distinct ANSI colors",
                theme.name,
                seen.len()
            );
        }
    }

    #[test]
    fn foreground_background_contrast_meets_aa_or_is_exempt() {
        for theme in list_presets() {
            let ratio = contrast(theme.foreground, theme.background);
            if let Some(reason) = exemption(theme) {
                assert!(
                    ratio < MIN_CONTRAST,
                    "preset {} is listed as a contrast exemption but measures \
                     {ratio:.2}:1; remove the exemption",
                    theme.name
                );
                assert!(!reason.is_empty());
            } else {
                assert!(
                    ratio >= MIN_CONTRAST,
                    "preset {} contrast {ratio:.2}:1 is below {MIN_CONTRAST}:1",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn focused_outline_meets_non_text_contrast_floor() {
        // CTX-0340 AC-1: the focused outline must clear 3:1 against the
        // workspace background. The derivation above guarantees it for every
        // preset; this test pins the guarantee so a future edit cannot
        // silently regress it.
        for theme in list_presets() {
            let focused = [
                theme.border_focused.0[0],
                theme.border_focused.0[1],
                theme.border_focused.0[2],
            ];
            let ratio = contrast(focused, theme.background);
            assert!(
                ratio >= 3.0,
                "preset {} focused outline contrast {ratio:.2}:1 is below 3:1",
                theme.name
            );
        }
    }

    #[test]
    fn every_preset_declares_provenance() {
        for theme in list_presets() {
            assert!(
                theme.source.starts_with("https://"),
                "preset {} source is not a URL: {:?}",
                theme.name,
                theme.source
            );
            assert!(
                !theme.license.is_empty(),
                "preset {} has no license",
                theme.name
            );
        }
    }

    /// A preset's expected upstream RGB values:
    /// `(bg, fg, cursor, selection, ansi0, ansi1, ansi4, ansi15)`.
    type ExpectedColors = (
        [u8; 3],
        [u8; 3],
        [u8; 3],
        [u8; 3],
        [u8; 3],
        [u8; 3],
        [u8; 3],
        [u8; 3],
    );

    #[test]
    fn known_presets_match_upstream_values() {
        let checks: &[(&str, ExpectedColors)] = &[
            (
                "tokyo-night",
                (
                    [0x1A, 0x1B, 0x26],
                    [0xC0, 0xCA, 0xF5],
                    [0xC0, 0xCA, 0xF5],
                    [0x28, 0x34, 0x57],
                    [0x15, 0x16, 0x1E],
                    [0xF7, 0x76, 0x8E],
                    [0x7A, 0xA2, 0xF7],
                    [0xC0, 0xCA, 0xF5],
                ),
            ),
            (
                "catppuccin-mocha",
                (
                    [0x1E, 0x1E, 0x2E],
                    [0xCD, 0xD6, 0xF4],
                    [0xF5, 0xE0, 0xDC],
                    [0xF5, 0xE0, 0xDC],
                    [0x45, 0x47, 0x5A],
                    [0xF3, 0x8B, 0xA8],
                    [0x89, 0xB4, 0xFA],
                    [0xA6, 0xAD, 0xC8],
                ),
            ),
            (
                "dracula",
                (
                    [0x28, 0x2A, 0x36],
                    [0xF8, 0xF8, 0xF2],
                    [0xF8, 0xF8, 0xF2],
                    [0x44, 0x47, 0x5A],
                    [0x21, 0x22, 0x2C],
                    [0xFF, 0x55, 0x55],
                    [0xBD, 0x93, 0xF9],
                    [0xFF, 0xFF, 0xFF],
                ),
            ),
            (
                "github-dark",
                (
                    [0x0D, 0x11, 0x17],
                    [0xE6, 0xED, 0xF3],
                    [0x2F, 0x81, 0xF7],
                    [0xE6, 0xED, 0xF3],
                    [0x48, 0x4F, 0x58],
                    [0xFF, 0x7B, 0x72],
                    [0x58, 0xA6, 0xFF],
                    [0xFF, 0xFF, 0xFF],
                ),
            ),
            (
                "github-light",
                (
                    [0xFF, 0xFF, 0xFF],
                    [0x1F, 0x23, 0x28],
                    [0x09, 0x69, 0xDA],
                    [0x1F, 0x23, 0x28],
                    [0x24, 0x29, 0x2F],
                    [0xCF, 0x22, 0x2E],
                    [0x09, 0x69, 0xDA],
                    [0x8C, 0x95, 0x9F],
                ),
            ),
            (
                "nord",
                (
                    [0x2E, 0x34, 0x40],
                    [0xD8, 0xDE, 0xE9],
                    [0xEC, 0xEF, 0xF4],
                    [0xEC, 0xEF, 0xF4],
                    [0x3B, 0x42, 0x52],
                    [0xBF, 0x61, 0x6A],
                    [0x81, 0xA1, 0xC1],
                    [0xEC, 0xEF, 0xF4],
                ),
            ),
            (
                "monokai",
                (
                    [0x27, 0x28, 0x22],
                    [0xFD, 0xFF, 0xF1],
                    [0xC0, 0xC1, 0xB5],
                    [0x57, 0x58, 0x4F],
                    [0x27, 0x28, 0x22],
                    [0xF9, 0x26, 0x72],
                    [0xFD, 0x97, 0x1F],
                    [0xFD, 0xFF, 0xF1],
                ),
            ),
        ];
        for &(name, colors) in checks {
            let (theme, status) = resolve_theme_with_status(Some(name));
            assert_eq!(status, ThemeResolution::Named, "preset {name}");
            let (bg, fg, cursor, selection, a0, a1, a4, a15) = colors;
            assert_eq!(theme.background, bg, "{name} background");
            assert_eq!(theme.foreground, fg, "{name} foreground");
            assert_eq!(theme.cursor, cursor, "{name} cursor");
            assert_eq!(theme.selection, selection, "{name} selection");
            assert_eq!(theme.ansi[0], a0, "{name} ansi0");
            assert_eq!(theme.ansi[1], a1, "{name} ansi1");
            assert_eq!(theme.ansi[4], a4, "{name} ansi4");
            assert_eq!(theme.ansi[15], a15, "{name} ansi15");
        }
    }
}
