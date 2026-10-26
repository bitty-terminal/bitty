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
//! rather than upstream palette data, derived per preset so the accepted
//! contrast rules hold (RFC-0001/OQ-039): **AC-1** focused >= 3:1 against the
//! preset background (enforced), **AC-2** focused >= 3:1 against idle
//! (enforced), and **AC-3** idle >= 1.5:1 against the background (advisory,
//! reported by `bitty config check`). The rule:
//!
//! 1. Try ANSI accents as the focused outline, preferring the blue/cyan
//!    family (ANSI 4/6/12/14, highest contrast first), then the remaining
//!    ANSI 1–6/9–14 by descending contrast. The first accent that clears AC-1
//!    and admits an idle that clears AC-2 wins.
//! 2. If no accent qualifies, fall back to the preset foreground.
//! 3. Take the idle outline from ANSI 8 (bright black), then the palette
//!    selection color, then ANSI 0: the first surface that clears AC-2 and
//!    the advisory AC-3.
//! 4. If no surface clears both, mix the palette selection color toward the
//!    focused outline to the midpoint of the window where AC-2 and AC-3 both
//!    hold (CTX-0354). The idle stays a Bitty-owned blend of two preset
//!    tokens rather than an invented hue.
//!
//! Five presets use the step-4 blend — solarized-dark `#274F58`, one-dark
//! `#3B4E64`, rose-pine-dawn `#B7C1C6`, everforest-dark `#5B4E53`, and
//! everforest-light `#BCC2AC` (was `#EAEDC8`, 1.00:1). `tokyo-night-day` is
//! the one documented AC-3 exemption: against its focused `#007197` every
//! palette surface that clears AC-2 (the selection `#B7C1E3`, 3.09:1) sits at
//! only 1.38:1 against the background, and no selection->focused blend clears
//! both floors; retaining that visible 1.38:1 idle is preferred to a
//! near-black token outside the light palette's tone, and `bitty config
//! check` keeps reporting the advisory.
//!
//! [`BITTY_DARK`] keeps the originally ratified `#33CCFF`/`#595959AA` pair.
//! No preset needs an AC-1/AC-2 exemption: every catalog pair satisfies both
//! enforced rules, and the catalog tests below pin that.
//!
//! # Readability policy (CTX-1418, issue #2370)
//!
//! Presets are matched case- and space-insensitively to Ghostty's shipped
//! theme corpus where a canonical file exists; matching presets reproduce the
//! Ghostty palette rather than an older iTerm2 export. `bitty-dark` has no
//! Ghostty counterpart and keeps its designed values. Where a canonical
//! Ghostty value is unreadable under this policy the value is repaired
//! minimally (lightness only, hue preserved) and the deviation is recorded in
//! `bitty-terminal-docs`; the full per-preset table, including Ghostty
//! provenance, lives in the configuration reference.
//!
//! The enforced floors are [`MIN_TEXT_CONTRAST`] (4.5:1) for the default
//! foreground and the chromatic ANSI slots TUIs use for syntax highlighting
//! (1–6 and 9–14), [`MIN_TEXT_CONTRAST`] for the cursor fill against the
//! background, and [`MIN_SELECTION_CONTRAST`] (1.3:1) for the selection fill
//! while selected text keeps [`MIN_TEXT_CONTRAST`]. ANSI 0/8 are surface
//! tokens and may legally sit near the background; in light presets 7/15 are
//! surface tokens too. They are reported by `bitty config check` as
//! `theme.contrast.advisory.*` when they fall below [`MIN_SURFACE_CONTRAST`],
//! but they are never gated and never treated as text.
//! [`TEXT_CONTRAST_EXEMPTIONS`] and
//! [`SELECTION_CONTRAST_EXEMPTIONS`] name the only accepted exceptions
//! (Solarized, designed low-contrast); the catalog tests require each
//! exemption to still measure below its floor, so exemptions cannot go dead.
//!
//! Derived chrome uses the same measurements and the same theme-owned
//! colors: the IME preedit underline (cursor on selection) clears
//! [`MIN_UI_CONTRAST`], the help-panel ring is the theme's accepted outline
//! pair (`border.idle` / `border.focused`, CTX-1477), the legacy accent probe
//! ([`help_ring_source`]) still measures the first of ANSI 12/4/6 or the
//! foreground that clears [`MIN_UI_CONTRAST`] on the selection ground, and the
//! pending-paste banner picks black or white for [`MIN_TEXT_CONTRAST`] on
//! ANSI 3 ([`banner_text`]). No chrome role invents a color outside the
//! resolved palette.
//!
//! Contrast alone can hide a palette regression: two families may end up at
//! the same hue and similar lightness after a repair. [`hue_checks`] measures
//! every cross-family ANSI pair in CIE76 `ΔE*ab` and HSL hue separation,
//! requiring [`MIN_ANSI_FAMILY_DELTA_E`] *or*
//! [`MIN_ANSI_FAMILY_HUE_DEGREES`]. Normal/bright aliases inside one family
//! are intentional and skipped.
//! [`HUE_COLLISION_EXEMPTIONS`] documents Solarized's neutral bright ramp,
//! and the catalog test requires that exemption to stay measurable.
//!
//! Cursor-vs-foreground contrast is deliberately not a floor: with a
//! foreground/background pair at or above 4.5:1 no block fill can clear 3:1
//! against both sides (the usable luminance interval is too narrow). The
//! cursor is a block cue measured against the background; the glyph under it
//! is a separate reverse-video rendering concern.
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

// -- Contrast policy (CTX-1418, issue #2370) --------------------------------
//
// Built-in presets are user-facing defaults, so their readability is a
// product contract, not a cosmetic preference. The policy below is the one
// `bitty config check` reports and the catalog test enforces:
//
// - **Text**: the default foreground and every chromatic ANSI slot used by
//   syntax-highlighting TUIs (indices 1–6 and 9–14) must clear WCAG AA
//   normal-text contrast (>= 4.5:1) against the preset background.
// - **Neutral ANSI**: `0`/`8` (black/bright black) are surface tokens, and in
//   light presets `7`/`15` (white/bright white) are surface tokens too. They
//   are exempt from the text floor but must stay visible against the
//   background (>= 1.2:1) or be an explicit background alias.
// - **Cursor**: a block-cursor fill must clear the normal-text floor against
//   the background. A fill that also had to clear 3:1 against the foreground
//   cannot exist whenever foreground/background is >= 4.5:1: the interval
//   between the two luminances is too narrow. The cursor is therefore a
//   block cue judged against the background (the glyph under it is a
//   separate reverse-video concern), not a foreground-colored glyph.
// - **Selection**: the selected text keeps the preset foreground, so
//   foreground-on-selection must clear 4.5:1, and the selection fill itself
//   must be visible against the background (>= 1.3:1).
//
// Exemptions are explicit and pinned by tests: a preset listed as exempt
// must still measure below the floor, so an exemption cannot silently become
// dead policy. Solarized is the only family with exemptions because its
// upstream design is deliberately low-contrast (fg/bg 4.13 light, 4.75 dark)
// and its visible selection necessarily eats part of that headroom.

/// WCAG 2.1 AA contrast floor for normal text (4.5:1).
pub const MIN_TEXT_CONTRAST: f64 = 4.5;

/// WCAG 2.1 AA contrast floor for large text and non-text UI (3:1).
pub const MIN_UI_CONTRAST: f64 = 3.0;

/// Minimum visibility of the selection fill against the background (1.3:1).
pub const MIN_SELECTION_CONTRAST: f64 = 1.3;

/// Advisory visibility of a neutral surface token against the background
/// (1.2:1). Surface tokens are never gated: the row is reported by
/// `bitty config check` as `theme.contrast.advisory.*` and never treated as
/// text.
pub const MIN_SURFACE_CONTRAST: f64 = 1.2;

/// Perceptual distance (CIE76 `ΔE*ab`) that keeps two different ANSI color
/// families visually distinct even when their hues are close (for example
/// Nord's frost blue and cyan). A palette pair that fails this floor must
/// separate by at least [`MIN_ANSI_FAMILY_HUE_DEGREES`] instead.
pub const MIN_ANSI_FAMILY_DELTA_E: f64 = 14.0;

/// Hue separation (degrees, HSL) that keeps two different ANSI color
/// families distinguishable when their lightness is close.
pub const MIN_ANSI_FAMILY_HUE_DEGREES: f64 = 15.0;

/// Presets whose default foreground is an accepted upstream low-contrast
/// value, with the reason. The catalog test requires each entry to still be
/// below [`MIN_TEXT_CONTRAST`] so the exemption stays live.
pub const TEXT_CONTRAST_EXEMPTIONS: &[(&str, &str)] = &[(
    "solarized-light",
    "Solarized is designed as a low-contrast scheme; foreground #657b83 on \
     background #fdf6e3 is 4.13:1 upstream and is retained verbatim",
)];

/// Presets whose selection cannot hold both the text floor and the visibility
/// floor because their foreground/background pair leaves no headroom, with
/// the reason. Measured values are reported by `bitty config check`.
pub const SELECTION_CONTRAST_EXEMPTIONS: &[(&str, &str)] = &[
    (
        "solarized-dark",
        "Solarized is designed low-contrast (fg/bg 4.75:1); a visible \
         selection necessarily drops selected text below 4.5:1",
    ),
    (
        "solarized-light",
        "Solarized is designed low-contrast (fg/bg 4.13:1, itself exempt); \
         the selection tracks that documented ceiling",
    ),
];

/// Role of one contrast check row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastRole {
    /// Default foreground on the preset background.
    Foreground,
    /// A chromatic ANSI slot used as foreground text.
    AnsiText(u8),
    /// A neutral ANSI slot used as a surface tone.
    AnsiSurface(u8),
    /// Block-cursor fill against the background.
    Cursor,
    /// Preset foreground on the selection fill.
    SelectionText,
    /// Selection fill against the background.
    SelectionSurface,
    /// Cursor fill against the selection fill (IME preedit underline over
    /// the preedit ground).
    CursorSelection,
    /// Derived help-panel ring against the help-panel ground (selection).
    HelpRing,
    /// Derived pending-paste banner text against the ANSI 3 ground.
    BannerText,
}

impl ContrastRole {
    /// Stable diagnostic label (`foreground`, `ansi2.text`, ...).
    #[must_use]
    pub fn label(self) -> &'static str {
        const TEXT: [&str; 16] = [
            "ansi0.text",
            "ansi1.text",
            "ansi2.text",
            "ansi3.text",
            "ansi4.text",
            "ansi5.text",
            "ansi6.text",
            "ansi7.text",
            "ansi8.text",
            "ansi9.text",
            "ansi10.text",
            "ansi11.text",
            "ansi12.text",
            "ansi13.text",
            "ansi14.text",
            "ansi15.text",
        ];
        const SURFACE: [&str; 16] = [
            "ansi0.surface",
            "ansi1.surface",
            "ansi2.surface",
            "ansi3.surface",
            "ansi4.surface",
            "ansi5.surface",
            "ansi6.surface",
            "ansi7.surface",
            "ansi8.surface",
            "ansi9.surface",
            "ansi10.surface",
            "ansi11.surface",
            "ansi12.surface",
            "ansi13.surface",
            "ansi14.surface",
            "ansi15.surface",
        ];
        match self {
            Self::Foreground => "foreground",
            Self::AnsiText(index) => TEXT[usize::from(index % 16)],
            Self::AnsiSurface(index) => SURFACE[usize::from(index % 16)],
            Self::Cursor => "cursor",
            Self::SelectionText => "selection.text",
            Self::SelectionSurface => "selection.surface",
            Self::CursorSelection => "cursor.on-selection",
            Self::HelpRing => "chrome.help-ring",
            Self::BannerText => "chrome.banner-text",
        }
    }
}

/// One measured contrast row for a preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeContrastCheck {
    /// What was measured.
    pub role: ContrastRole,
    /// Measured WCAG contrast ratio.
    pub ratio: f64,
    /// Floor that applies to this role.
    pub floor: f64,
    /// Whether the measurement clears the floor or carries an exemption.
    pub passed: bool,
    /// Explicit reason when an upstream value is retained below the floor.
    pub exemption: Option<&'static str>,
    /// Advisory rows are reported but never gate a preset. Neutral surface
    /// tokens below [`MIN_SURFACE_CONTRAST`] are the only advisory rows: an
    /// upstream palette may deliberately keep ANSI 0/8 (or light 7/15) close
    /// to its background, and that is not a text-readability failure.
    pub advisory: bool,
}

impl ThemeContrastCheck {
    fn new(role: ContrastRole, ratio: f64, floor: f64, exemption: Option<&'static str>) -> Self {
        Self {
            role,
            ratio,
            floor,
            passed: ratio >= floor || exemption.is_some(),
            exemption,
            advisory: false,
        }
    }

    fn advisory(role: ContrastRole, ratio: f64, floor: f64) -> Self {
        Self {
            role,
            ratio,
            floor,
            passed: true,
            exemption: None,
            advisory: true,
        }
    }
}

/// Returns the documented text-contrast exemption for `theme`, if any.
#[must_use]
pub fn text_contrast_exemption(theme: &Theme) -> Option<&'static str> {
    TEXT_CONTRAST_EXEMPTIONS
        .iter()
        .find(|(name, _)| *name == theme.name)
        .map(|(_, reason)| *reason)
}

/// Returns the documented selection-contrast exemption for `theme`, if any.
#[must_use]
pub fn selection_contrast_exemption(theme: &Theme) -> Option<&'static str> {
    SELECTION_CONTRAST_EXEMPTIONS
        .iter()
        .find(|(name, _)| *name == theme.name)
        .map(|(_, reason)| *reason)
}

/// First candidate color that clears `floor` against `background`.
///
/// Derived chrome (help-panel ring, IME accent) must stay legible without
/// inventing a color: the caller supplies theme-owned candidates in
/// preference order. Returns the index of the winning candidate so
/// diagnostics can name the role that won.
#[must_use]
pub fn readable_accent(candidates: &[[u8; 3]], background: [u8; 3], floor: f64) -> Option<usize> {
    candidates
        .iter()
        .position(|candidate| crate::types::contrast_ratio(*candidate, background) >= floor)
}

/// Legacy help-panel accent probe for `theme`: the first of ANSI 12 (the
/// primary accent), ANSI 4, ANSI 6, or the foreground that clears
/// [`MIN_UI_CONTRAST`] against the help-panel ground (the selection fill).
///
/// CTX-1477 moved production chrome to the theme outline pair; this source
/// remains the measured accent-family probe used by the contrast report, so a
/// preset's ANSI accent fallback chain stays covered. It mirrors the live
/// candidate order (including OSC 4 overrides) but is no longer the colour the
/// help panel paints.
#[must_use]
pub fn help_ring_source(theme: &Theme) -> usize {
    let candidates = [
        theme.ansi_entry(12),
        theme.ansi_entry(4),
        theme.ansi_entry(6),
        theme.foreground,
    ];
    // The foreground is always a candidate and selected text clears 4.5:1
    // (or its documented Solarized exemption), so this cannot be `None`.
    readable_accent(&candidates, theme.selection, MIN_UI_CONTRAST).unwrap_or(3)
}

/// Black or white, whichever reads better on `background` (the pending-paste
/// banner's derived text role in `bitty-render`).
#[must_use]
pub fn banner_text(background: [u8; 3]) -> [u8; 3] {
    use crate::types::contrast_ratio;
    let black = contrast_ratio([0, 0, 0], background);
    let white = contrast_ratio([255, 255, 255], background);
    if black >= white {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    }
}

/// One measured ANSI hue/lightness collision for a preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeHueCheck {
    /// Lower ANSI index of the pair (0–15).
    pub first: u8,
    /// Higher ANSI index of the pair (0–15).
    pub second: u8,
    /// CIE76 `ΔE*ab` between the two colors.
    pub delta_e: f64,
    /// HSL hue separation in degrees (0–180).
    pub hue_delta: f64,
    /// Whether the pair clears [`MIN_ANSI_FAMILY_DELTA_E`] or
    /// [`MIN_ANSI_FAMILY_HUE_DEGREES`].
    pub passed: bool,
}

/// Chromatic ANSI indices: the six normal families (1–6) and their bright
/// counterparts (9–14).
const CHROMATIC_ANSI: [u8; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];

/// ANSI family of one chromatic index (0 = red … 5 = cyan).
const fn ansi_family(index: u8) -> Option<u8> {
    match index {
        1 | 9 => Some(0),
        2 | 10 => Some(1),
        3 | 11 => Some(2),
        4 | 12 => Some(3),
        5 | 13 => Some(4),
        6 | 14 => Some(5),
        _ => None,
    }
}

/// `sRGB` byte to CIE L*a*b* (D65).
fn lab(rgb: [u8; 3]) -> [f64; 3] {
    fn linear(channel: u8) -> f64 {
        let c = f64::from(channel) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let (r, g, b) = (linear(rgb[0]), linear(rgb[1]), linear(rgb[2]));
    let x = r * 0.412_456_4 + g * 0.357_576_1 + b * 0.180_437_5;
    let y = r * 0.212_672_9 + g * 0.715_152_2 + b * 0.072_175_0;
    let z = r * 0.019_333_9 + g * 0.119_192_0 + b * 0.950_304_1;
    let f = |t: f64| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / 0.950_47), f(y), f(z / 1.088_83));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIE76 `ΔE*ab` between two opaque colors.
#[must_use]
pub fn delta_e(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (lab(a), lab(b));
    ((la[0] - lb[0]).powi(2) + (la[1] - lb[1]).powi(2) + (la[2] - lb[2]).powi(2)).sqrt()
}

/// HSL hue in degrees (0–360) for `rgb`; `None` for an achromatic color.
fn hue_degrees(rgb: [u8; 3]) -> Option<f64> {
    let (r, g, b) = (
        f64::from(rgb[0]) / 255.0,
        f64::from(rgb[1]) / 255.0,
        f64::from(rgb[2]) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    if chroma == 0.0 {
        return None;
    }
    let hue = if max == r {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    };
    Some(hue * 60.0)
}

/// Smallest separation between two hues, in degrees (0–180).
fn hue_separation(a: [u8; 3], b: [u8; 3]) -> f64 {
    match (hue_degrees(a), hue_degrees(b)) {
        (Some(first), Some(second)) => {
            let delta = (first - second).abs();
            delta.min(360.0 - delta)
        }
        // An achromatic pair has no hue to collide; its lightness difference
        // is measured by `delta_e` instead.
        _ => 180.0,
    }
}

/// Measures every cross-family ANSI pair of `theme` for hue/lightness
/// collisions.
///
/// Normal (1–6) and bright (9–14) slots of the same family are skipped:
/// aliasing a bright slot to its normal slot is an intentional design choice
/// in several upstream palettes and is not a collision. Every pair of
/// *different* families must separate by [`MIN_ANSI_FAMILY_DELTA_E`] or
/// [`MIN_ANSI_FAMILY_HUE_DEGREES`].
#[must_use]
pub fn hue_checks(theme: &Theme) -> Vec<ThemeHueCheck> {
    let mut rows = Vec::with_capacity(60);
    for (offset, &first) in CHROMATIC_ANSI.iter().enumerate() {
        for &second in &CHROMATIC_ANSI[offset + 1..] {
            if ansi_family(first) == ansi_family(second) {
                continue;
            }
            let a = theme.ansi_entry(first);
            let b = theme.ansi_entry(second);
            let delta_e = delta_e(a, b);
            let hue_delta = hue_separation(a, b);
            rows.push(ThemeHueCheck {
                first,
                second,
                delta_e,
                hue_delta,
                passed: delta_e >= MIN_ANSI_FAMILY_DELTA_E
                    || hue_delta >= MIN_ANSI_FAMILY_HUE_DEGREES,
            });
        }
    }
    rows
}

/// Presets whose bright ANSI slots are deliberately neutral `base0`–`base3`
/// tones rather than distinct hues, with the reason. The catalog test
/// requires an exempt preset to still measure at least one collision, so the
/// exemption cannot become dead policy.
pub const HUE_COLLISION_EXEMPTIONS: &[(&str, &str)] = &[
    (
        "solarized-dark",
        "Solarized's bright slots 9–14 are the canonical base0/base1/orange \
         tones, not six distinguishable hues; changing them would discard the \
         scheme's defining neutral bright ramp",
    ),
    (
        "solarized-light",
        "Solarized's bright slots 9–14 are the canonical base0/base1/orange \
         tones, not six distinguishable hues; changing them would discard the \
         scheme's defining neutral bright ramp",
    ),
];

/// Returns the documented hue-collision exemption for `theme`, if any.
#[must_use]
pub fn hue_collision_exemption(theme: &Theme) -> Option<&'static str> {
    HUE_COLLISION_EXEMPTIONS
        .iter()
        .find(|(name, _)| *name == theme.name)
        .map(|(_, reason)| *reason)
}

/// ANSI pairs that collapse by both hue and lightness. Presets with a
/// documented [`HUE_COLLISION_EXEMPTIONS`] entry report no violations (the
/// exemption is measured by the catalog test).
#[must_use]
pub fn hue_violations(theme: &Theme) -> Vec<ThemeHueCheck> {
    if hue_collision_exemption(theme).is_some() {
        return Vec::new();
    }
    hue_checks(theme)
        .into_iter()
        .filter(|row| !row.passed)
        .collect()
}

/// Measures every contrast role the readability policy covers for `theme`.
///
/// The returned rows are stable in order: foreground, ANSI 0–15, cursor,
/// selection text, selection surface, cursor-on-selection, help-panel ring,
/// banner text. `bitty config check` prints them and the catalog test fails
/// on any row with `passed == false`.
#[must_use]
pub fn contrast_checks(theme: &Theme) -> Vec<ThemeContrastCheck> {
    use crate::types::contrast_ratio;

    let bg = theme.background;
    let mut rows = Vec::with_capacity(23);
    rows.push(ThemeContrastCheck::new(
        ContrastRole::Foreground,
        contrast_ratio(theme.foreground, bg),
        MIN_TEXT_CONTRAST,
        text_contrast_exemption(theme),
    ));
    let selection_exempt = selection_contrast_exemption(theme);
    for index in 0u8..16 {
        let rgb = theme.ansi_entry(index);
        let ratio = contrast_ratio(rgb, bg);
        if (1..=6).contains(&index) || (9..=14).contains(&index) {
            rows.push(ThemeContrastCheck::new(
                ContrastRole::AnsiText(index),
                ratio,
                MIN_TEXT_CONTRAST,
                None,
            ));
        } else if (index == 7 || index == 15) && theme.category == ThemeCategory::Dark {
            // Dark presets use white/bright-white as text.
            rows.push(ThemeContrastCheck::new(
                ContrastRole::AnsiText(index),
                ratio,
                MIN_TEXT_CONTRAST,
                None,
            ));
        } else {
            // Neutral surface token: ANSI 0/8, plus 7/15 on light presets.
            // These are reported, never gated: canonical palettes
            // deliberately place black near a dark background (and white
            // near a light one), which is not a text failure.
            if ratio >= MIN_SURFACE_CONTRAST {
                rows.push(ThemeContrastCheck::new(
                    ContrastRole::AnsiSurface(index),
                    ratio,
                    MIN_SURFACE_CONTRAST,
                    None,
                ));
            } else {
                rows.push(ThemeContrastCheck::advisory(
                    ContrastRole::AnsiSurface(index),
                    ratio,
                    MIN_SURFACE_CONTRAST,
                ));
            }
        }
    }
    rows.push(ThemeContrastCheck::new(
        ContrastRole::Cursor,
        contrast_ratio(theme.cursor, bg),
        MIN_TEXT_CONTRAST,
        None,
    ));
    rows.push(ThemeContrastCheck::new(
        ContrastRole::SelectionText,
        contrast_ratio(theme.foreground, theme.selection),
        MIN_TEXT_CONTRAST,
        selection_exempt,
    ));
    rows.push(ThemeContrastCheck::new(
        ContrastRole::SelectionSurface,
        contrast_ratio(theme.selection, bg),
        MIN_SELECTION_CONTRAST,
        selection_exempt,
    ));
    // Derived chrome roles. These are the same expressions the renderer
    // uses, so a preset cannot pass the policy and still ship an unreadable
    // banner, IME underline, or help-panel ring.
    rows.push(ThemeContrastCheck::new(
        ContrastRole::CursorSelection,
        contrast_ratio(theme.cursor, theme.selection),
        MIN_UI_CONTRAST,
        None,
    ));
    let ring = [
        theme.ansi_entry(12),
        theme.ansi_entry(4),
        theme.ansi_entry(6),
        theme.foreground,
    ][help_ring_source(theme)];
    rows.push(ThemeContrastCheck::new(
        ContrastRole::HelpRing,
        contrast_ratio(ring, theme.selection),
        MIN_UI_CONTRAST,
        None,
    ));
    rows.push(ThemeContrastCheck::new(
        ContrastRole::BannerText,
        contrast_ratio(banner_text(theme.ansi_entry(3)), theme.ansi_entry(3)),
        MIN_TEXT_CONTRAST,
        None,
    ));
    rows
}

/// Returns only the rows that fail the policy. Empty means the preset is
/// readable under the documented floors (exempt rows count as passing).
#[must_use]
pub fn contrast_violations(theme: &Theme) -> Vec<ThemeContrastCheck> {
    contrast_checks(theme)
        .into_iter()
        .filter(|row| !row.passed)
        .collect()
}

/// Minimum measured contrast across the chromatic ANSI text slots (1–6 and
/// 9–14), for the compact `bitty config check` summary.
#[must_use]
pub fn min_ansi_text_contrast(theme: &Theme) -> f64 {
    use crate::types::contrast_ratio;
    (1..=6)
        .chain(9..=14)
        .map(|index| contrast_ratio(theme.ansi_entry(index), theme.background))
        .fold(f64::INFINITY, f64::min)
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
    selection: [0x33, 0x46, 0x7C],
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
        [0xF7, 0x76, 0x8E], // 9 red
        [0x9E, 0xCE, 0x6A], // 10 green
        [0xE0, 0xAF, 0x68], // 11 yellow
        [0x7A, 0xA2, 0xF7], // 12 blue
        [0xBB, 0x9A, 0xF7], // 13 magenta
        [0x7D, 0xCF, 0xFF], // 14 cyan
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
    selection: [0x36, 0x4A, 0x82],
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
        [0x4E, 0x55, 0x75], // 8 black
        [0xF7, 0x76, 0x8E], // 9 red
        [0x9E, 0xCE, 0x6A], // 10 green
        [0xE0, 0xAF, 0x68], // 11 yellow
        [0x7A, 0xA2, 0xF7], // 12 blue
        [0xBB, 0x9A, 0xF7], // 13 magenta
        [0x7D, 0xCF, 0xFF], // 14 cyan
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
    foreground: [0x2A, 0x4C, 0x9B],
    cursor: [0x37, 0x60, 0xBF],
    selection: [0xBE, 0xC7, 0xDF],
    border_focused: OutlineColor([0x00, 0x71, 0x97, 0xFF]),
    border_idle: OutlineColor([0xB7, 0xC1, 0xE3, 0xFF]),
    ansi: [
        [0xE9, 0xE9, 0xED], // 0 black
        [0xC1, 0x21, 0x4F], // 1 red
        [0x52, 0x6D, 0x35], // 2 green
        [0x7C, 0x60, 0x37], // 3 yellow
        [0x25, 0x63, 0xB9], // 4 blue
        [0x7E, 0x46, 0xC8], // 5 magenta
        [0x00, 0x6D, 0x91], // 6 cyan
        [0x61, 0x72, 0xB0], // 7 white
        [0xA1, 0xA6, 0xC5], // 8 black
        [0xC1, 0x21, 0x4F], // 9 red
        [0x52, 0x6D, 0x35], // 10 green
        [0x7C, 0x60, 0x37], // 11 yellow
        [0x25, 0x63, 0xB9], // 12 blue
        [0x7E, 0x46, 0xC8], // 13 magenta
        [0x00, 0x6D, 0x91], // 14 cyan
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
    selection: [0x58, 0x5B, 0x70],
    border_focused: OutlineColor([0x94, 0xE2, 0xD5, 0xFF]),
    border_idle: OutlineColor([0x58, 0x5B, 0x70, 0xFF]),
    ansi: [
        [0x45, 0x47, 0x5A], // 0 black
        [0xF3, 0x8B, 0xA8], // 1 red
        [0xA6, 0xE3, 0xA1], // 2 green
        [0xF9, 0xE2, 0xAF], // 3 yellow
        [0x89, 0xB4, 0xFA], // 4 blue
        [0xF5, 0xC2, 0xE7], // 5 magenta
        [0x94, 0xE2, 0xD5], // 6 cyan
        [0xA6, 0xAD, 0xC8], // 7 white
        [0x58, 0x5B, 0x70], // 8 black
        [0xF3, 0x77, 0x99], // 9 red
        [0x89, 0xD8, 0x8B], // 10 green
        [0xEB, 0xD3, 0x91], // 11 yellow
        [0x74, 0xA8, 0xFC], // 12 blue
        [0xF2, 0xAE, 0xDE], // 13 magenta
        [0x6B, 0xD7, 0xCA], // 14 cyan
        [0xBA, 0xC2, 0xDE], // 15 white
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
    selection: [0x56, 0x5B, 0x72],
    border_focused: OutlineColor([0x8B, 0xD5, 0xCA, 0xFF]),
    border_idle: OutlineColor([0x5B, 0x60, 0x78, 0xFF]),
    ansi: [
        [0x49, 0x4D, 0x64], // 0 black
        [0xED, 0x87, 0x96], // 1 red
        [0xA6, 0xDA, 0x95], // 2 green
        [0xEE, 0xD4, 0x9F], // 3 yellow
        [0x8A, 0xAD, 0xF4], // 4 blue
        [0xF5, 0xBD, 0xE6], // 5 magenta
        [0x8B, 0xD5, 0xCA], // 6 cyan
        [0xA5, 0xAD, 0xCB], // 7 white
        [0x5B, 0x60, 0x78], // 8 black
        [0xEC, 0x74, 0x86], // 9 red
        [0x8C, 0xCF, 0x7F], // 10 green
        [0xE1, 0xC6, 0x82], // 11 yellow
        [0x78, 0xA1, 0xF6], // 12 blue
        [0xF2, 0xA9, 0xDD], // 13 magenta
        [0x63, 0xCB, 0xC0], // 14 cyan
        [0xB8, 0xC0, 0xE0], // 15 white
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
    selection: [0x54, 0x59, 0x70],
    border_focused: OutlineColor([0x81, 0xC8, 0xBE, 0xFF]),
    border_idle: OutlineColor([0x51, 0x57, 0x6D, 0xFF]),
    ansi: [
        [0x51, 0x57, 0x6D], // 0 black
        [0xE7, 0x82, 0x84], // 1 red
        [0xA6, 0xD1, 0x89], // 2 green
        [0xE5, 0xC8, 0x90], // 3 yellow
        [0x8C, 0xAA, 0xEE], // 4 blue
        [0xF4, 0xB8, 0xE4], // 5 magenta
        [0x81, 0xC8, 0xBE], // 6 cyan
        [0xA5, 0xAD, 0xCE], // 7 white
        [0x62, 0x68, 0x80], // 8 black
        [0xE8, 0x7E, 0x7E], // 9 red
        [0x8E, 0xC7, 0x72], // 10 green
        [0xD9, 0xBA, 0x73], // 11 yellow
        [0x7B, 0x9E, 0xF0], // 12 blue
        [0xF2, 0xA4, 0xDB], // 13 magenta
        [0x5A, 0xBF, 0xB5], // 14 cyan
        [0xB5, 0xBF, 0xE2], // 15 white
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
    cursor: [0x93, 0x5C, 0x50],
    selection: [0xBF, 0xC3, 0xCE],
    border_focused: OutlineColor([0x4C, 0x4F, 0x69, 0xFF]),
    border_idle: OutlineColor([0xAC, 0xB0, 0xBE, 0xFF]),
    ansi: [
        [0x5C, 0x5F, 0x77], // 0 black
        [0xD2, 0x0F, 0x39], // 1 red
        [0x32, 0x7D, 0x22], // 2 green
        [0x99, 0x62, 0x14], // 3 yellow
        [0x1D, 0x63, 0xEF], // 4 blue
        [0xA2, 0x51, 0x8C], // 5 magenta
        [0x13, 0x79, 0x7F], // 6 cyan
        [0xAC, 0xB0, 0xBE], // 7 white
        [0x6C, 0x6F, 0x85], // 8 black
        [0xD2, 0x27, 0x3B], // 9 red
        [0x34, 0x7D, 0x2C], // 10 green
        [0x95, 0x64, 0x1C], // 11 yellow
        [0x3E, 0x63, 0xE4], // 12 blue
        [0x9F, 0x54, 0x88], // 13 magenta
        [0x22, 0x79, 0x80], // 14 cyan
        [0xBC, 0xC0, 0xCC], // 15 white
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
    selection: [0x43, 0x4C, 0x5E],
    border_focused: OutlineColor([0x88, 0xC0, 0xD0, 0xFF]),
    border_idle: OutlineColor([0x59, 0x63, 0x77, 0xFF]),
    ansi: [
        [0x3B, 0x42, 0x52], // 0 black
        [0xCF, 0x89, 0x8F], // 1 red
        [0xA3, 0xBE, 0x8C], // 2 green
        [0xEB, 0xCB, 0x8B], // 3 yellow
        [0x81, 0xA1, 0xC1], // 4 blue
        [0xB6, 0x90, 0xAF], // 5 magenta
        [0x88, 0xC0, 0xD0], // 6 cyan
        [0xE5, 0xE9, 0xF0], // 7 white
        [0x59, 0x63, 0x77], // 8 black
        [0xCF, 0x89, 0x8F], // 9 red
        [0xA3, 0xBE, 0x8C], // 10 green
        [0xEB, 0xCB, 0x8B], // 11 yellow
        [0x81, 0xA1, 0xC1], // 12 blue
        [0xB6, 0x90, 0xAF], // 13 magenta
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
    border_focused: OutlineColor([0x8E, 0xC0, 0x7C, 0xFF]),
    border_idle: OutlineColor([0x66, 0x5C, 0x54, 0xFF]),
    ansi: [
        [0x28, 0x28, 0x28], // 0 black
        [0xDD, 0x6D, 0x68], // 1 red
        [0x98, 0x97, 0x1A], // 2 green
        [0xD7, 0x99, 0x21], // 3 yellow
        [0x61, 0x98, 0x9A], // 4 blue
        [0xBD, 0x7B, 0x99], // 5 magenta
        [0x68, 0x9D, 0x6A], // 6 cyan
        [0xA8, 0x99, 0x84], // 7 white
        [0x92, 0x83, 0x74], // 8 black
        [0xFB, 0x54, 0x40], // 9 red
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
    selection: [0xD5, 0xC4, 0xA1],
    border_focused: OutlineColor([0xB5, 0x76, 0x14, 0xFF]),
    border_idle: OutlineColor([0x3C, 0x38, 0x36, 0xFF]),
    ansi: [
        [0xFB, 0xF1, 0xC7], // 0 black
        [0xCC, 0x24, 0x1D], // 1 red
        [0x72, 0x71, 0x14], // 2 green
        [0x90, 0x66, 0x16], // 3 yellow
        [0x3D, 0x76, 0x79], // 4 blue
        [0x9E, 0x57, 0x78], // 5 magenta
        [0x4F, 0x77, 0x51], // 6 cyan
        [0x7C, 0x6F, 0x64], // 7 white
        [0x92, 0x83, 0x74], // 8 black
        [0x9D, 0x00, 0x06], // 9 red
        [0x75, 0x70, 0x0E], // 10 green
        [0x97, 0x62, 0x11], // 11 yellow
        [0x07, 0x66, 0x78], // 12 blue
        [0x8F, 0x3F, 0x71], // 13 magenta
        [0x41, 0x79, 0x57], // 14 cyan
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
    border_idle: OutlineColor([0x27, 0x4F, 0x58, 0xFF]),
    ansi: [
        [0x07, 0x36, 0x42], // 0 black
        [0xE5, 0x64, 0x62], // 1 red
        [0x85, 0x99, 0x00], // 2 green
        [0xB5, 0x89, 0x00], // 3 yellow
        [0x36, 0x94, 0xD5], // 4 blue
        [0xDD, 0x62, 0x9E], // 5 magenta
        [0x2A, 0xA1, 0x98], // 6 cyan
        [0xEE, 0xE8, 0xD5], // 7 white
        [0x33, 0x5E, 0x69], // 8 black
        [0xD6, 0x71, 0x47], // 9 red
        [0x7F, 0x90, 0x95], // 10 green
        [0x7E, 0x90, 0x97], // 11 yellow
        [0x83, 0x94, 0x96], // 12 blue
        [0x83, 0x87, 0xCD], // 13 magenta
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
    cursor: [0x58, 0x6E, 0x75],
    selection: [0xEE, 0xE8, 0xD5],
    border_focused: OutlineColor([0x26, 0x8B, 0xD2, 0xFF]),
    border_idle: OutlineColor([0x00, 0x2B, 0x36, 0xFF]),
    ansi: [
        [0x07, 0x36, 0x42], // 0 black
        [0xD5, 0x31, 0x2E], // 1 red
        [0x69, 0x78, 0x00], // 2 green
        [0x8F, 0x6C, 0x00], // 3 yellow
        [0x20, 0x76, 0xB3], // 4 blue
        [0xCA, 0x34, 0x7D], // 5 magenta
        [0x21, 0x7E, 0x77], // 6 cyan
        [0xBB, 0xB5, 0xA2], // 7 white
        [0x00, 0x2B, 0x36], // 8 black
        [0xC4, 0x49, 0x15], // 9 red
        [0x58, 0x6E, 0x75], // 10 green
        [0x60, 0x74, 0x7C], // 11 yellow
        [0x66, 0x73, 0x75], // 12 blue
        [0x65, 0x6A, 0xB8], // 13 magenta
        [0x69, 0x73, 0x73], // 14 cyan
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
    border_focused: OutlineColor([0x61, 0xAF, 0xEF, 0xFF]),
    border_idle: OutlineColor([0x3B, 0x4E, 0x64, 0xFF]),
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
    cursor: [0x72, 0x72, 0x72],
    selection: [0xD9, 0xD9, 0xD9],
    border_focused: OutlineColor([0x2F, 0x5A, 0xF3, 0xFF]),
    border_idle: OutlineColor([0x00, 0x00, 0x00, 0xFF]),
    ansi: [
        [0x00, 0x00, 0x00], // 0 black
        [0xD2, 0x3B, 0x32], // 1 red
        [0x37, 0x82, 0x33], // 2 green
        [0x82, 0x71, 0x4D], // 3 yellow
        [0x2F, 0x5A, 0xF3], // 4 blue
        [0x95, 0x00, 0x95], // 5 magenta
        [0x0B, 0x6E, 0x85], // 6 cyan
        [0xBB, 0xBB, 0xBB], // 7 white
        [0x00, 0x00, 0x00], // 8 black
        [0xD2, 0x3B, 0x32], // 9 red
        [0x37, 0x82, 0x33], // 10 green
        [0x82, 0x71, 0x4D], // 11 yellow
        [0x2F, 0x5A, 0xF3], // 12 blue
        [0xA0, 0x00, 0x95], // 13 magenta
        [0x0B, 0x6E, 0x85], // 14 cyan
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
    selection: [0x23, 0x4E, 0x7D],
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
    selection: [0x2D, 0x57, 0x87],
    border_focused: OutlineColor([0x95, 0xE6, 0xCB, 0xFF]),
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
    cursor: [0x9C, 0x68, 0x1F],
    selection: [0xCC, 0xDD, 0xF4],
    border_focused: OutlineColor([0x9E, 0x75, 0xC7, 0xFF]),
    border_idle: OutlineColor([0x00, 0x00, 0x00, 0xFF]),
    ansi: [
        [0x00, 0x00, 0x00], // 0 black
        [0xB7, 0x54, 0x55], // 1 red
        [0x49, 0x80, 0x2D], // 2 green
        [0x95, 0x6B, 0x2B], // 3 yellow
        [0x27, 0x78, 0xB1], // 4 blue
        [0x86, 0x64, 0xA9], // 5 magenta
        [0x30, 0x80, 0x66], // 6 cyan
        [0xBA, 0xBA, 0xBA], // 7 white
        [0x68, 0x68, 0x68], // 8 black
        [0xB5, 0x55, 0x55], // 9 red
        [0x5E, 0x7D, 0x00], // 10 green
        [0x94, 0x6B, 0x2D], // 11 yellow
        [0x2B, 0x78, 0xAF], // 12 blue
        [0x86, 0x64, 0xA8], // 13 magenta
        [0x33, 0x7F, 0x66], // 14 cyan
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
    cursor: [0xDC, 0xD7, 0xBA],
    selection: [0x2D, 0x4F, 0x67],
    border_focused: OutlineColor([0x7F, 0xB4, 0xCA, 0xFF]),
    border_idle: OutlineColor([0x2D, 0x4F, 0x67, 0xFF]),
    ansi: [
        [0x09, 0x06, 0x18], // 0 black
        [0xCF, 0x67, 0x69], // 1 red
        [0x76, 0x94, 0x6A], // 2 green
        [0xC0, 0xA3, 0x6E], // 3 yellow
        [0x7E, 0x9C, 0xD8], // 4 blue
        [0x95, 0x7F, 0xB8], // 5 magenta
        [0x6A, 0x95, 0x89], // 6 cyan
        [0xC8, 0xC0, 0x93], // 7 white
        [0x72, 0x71, 0x69], // 8 black
        [0xEC, 0x4F, 0x4F], // 9 red
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
    border_focused: OutlineColor([0x59, 0x7B, 0x75, 0xFF]),
    border_idle: OutlineColor([0x1F, 0x1F, 0x28, 0xFF]),
    ansi: [
        [0x1F, 0x1F, 0x28], // 0 black
        [0xBB, 0x3C, 0x4E], // 1 red
        [0x5B, 0x70, 0x40], // 2 green
        [0x71, 0x6B, 0x3C], // 3 yellow
        [0x4D, 0x69, 0x9B], // 4 blue
        [0x9F, 0x51, 0x6C], // 5 magenta
        [0x51, 0x70, 0x6B], // 6 cyan
        [0x54, 0x54, 0x64], // 7 white
        [0x8A, 0x89, 0x80], // 8 black
        [0xBB, 0x3E, 0x41], // 9 red
        [0x56, 0x71, 0x4A], // 10 green
        [0x7A, 0x67, 0x45], // 11 yellow
        [0x4C, 0x6D, 0x8D], // 12 blue
        [0x62, 0x4C, 0x83], // 13 magenta
        [0x50, 0x70, 0x67], // 14 cyan
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
    cursor: [0xE0, 0xDE, 0xF4],
    selection: [0x40, 0x3D, 0x52],
    border_focused: OutlineColor([0xEB, 0xBC, 0xBA, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x6A, 0x86, 0xFF]),
    ansi: [
        [0x26, 0x23, 0x3A], // 0 black
        [0xEB, 0x6F, 0x92], // 1 red
        [0x4F, 0x89, 0xA0], // 2 green
        [0xF6, 0xC1, 0x77], // 3 yellow
        [0x9C, 0xCF, 0xD8], // 4 blue
        [0xC4, 0xA7, 0xE7], // 5 magenta
        [0xEB, 0xBC, 0xBA], // 6 cyan
        [0xE0, 0xDE, 0xF4], // 7 white
        [0x6E, 0x6A, 0x86], // 8 black
        [0xEB, 0x6F, 0x92], // 9 red
        [0x4F, 0x89, 0xA0], // 10 green
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
    cursor: [0xE0, 0xDE, 0xF4],
    selection: [0x44, 0x41, 0x5A],
    border_focused: OutlineColor([0x9C, 0xCF, 0xD8, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x6A, 0x86, 0xFF]),
    ansi: [
        [0x39, 0x35, 0x52], // 0 black
        [0xEB, 0x6F, 0x92], // 1 red
        [0x44, 0x93, 0xB2], // 2 green
        [0xF6, 0xC1, 0x77], // 3 yellow
        [0x9C, 0xCF, 0xD8], // 4 blue
        [0xC4, 0xA7, 0xE7], // 5 magenta
        [0xEA, 0x9A, 0x97], // 6 cyan
        [0xE0, 0xDE, 0xF4], // 7 white
        [0x6E, 0x6A, 0x86], // 8 black
        [0xEB, 0x6F, 0x92], // 9 red
        [0x44, 0x93, 0xB2], // 10 green
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
    cursor: [0x57, 0x52, 0x79],
    selection: [0xDC, 0xD7, 0xD7],
    border_focused: OutlineColor([0x28, 0x69, 0x83, 0xFF]),
    border_idle: OutlineColor([0xB7, 0xC1, 0xC6, 0xFF]),
    ansi: [
        [0xF2, 0xE9, 0xE1], // 0 black
        [0xA3, 0x5A, 0x6F], // 1 red
        [0x20, 0x58, 0x70], // 2 green
        [0x98, 0x66, 0x22], // 3 yellow
        [0x46, 0x78, 0x80], // 4 blue
        [0x7B, 0x68, 0x91], // 5 magenta
        [0x9E, 0x5F, 0x5D], // 6 cyan
        [0x57, 0x52, 0x79], // 7 white
        [0x98, 0x93, 0xA5], // 8 black
        [0xA3, 0x5A, 0x6F], // 9 red
        [0x20, 0x58, 0x70], // 10 green
        [0x98, 0x66, 0x22], // 11 yellow
        [0x46, 0x78, 0x80], // 12 blue
        [0x7B, 0x68, 0x91], // 13 magenta
        [0x9E, 0x5F, 0x5D], // 14 cyan
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
    border_focused: OutlineColor([0x83, 0xC0, 0x92, 0xFF]),
    border_idle: OutlineColor([0x5B, 0x4E, 0x53, 0xFF]),
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
        [0xF8, 0x58, 0x55], // 9 red
        [0x8D, 0xA1, 0x01], // 10 green
        [0xDF, 0xA0, 0x00], // 11 yellow
        [0x40, 0x98, 0xC7], // 12 blue
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
    foreground: [0x4A, 0x56, 0x5D],
    cursor: [0xA4, 0x54, 0x1A],
    selection: [0xD0, 0xD0, 0xBF],
    border_focused: OutlineColor([0x5C, 0x6A, 0x72, 0xFF]),
    border_idle: OutlineColor([0xBC, 0xC2, 0xAC, 0xFF]),
    ansi: [
        [0x7A, 0x84, 0x78], // 0 black
        [0x9C, 0x55, 0x57], // 1 red
        [0x60, 0x6F, 0x48], // 2 green
        [0x7B, 0x67, 0x41], // 3 yellow
        [0x4C, 0x70, 0x6C], // 4 blue
        [0x85, 0x5F, 0x72], // 5 magenta
        [0x4E, 0x72, 0x57], // 6 cyan
        [0xB2, 0xAF, 0x9F], // 7 white
        [0xA6, 0xB0, 0xA0], // 8 black
        [0xB9, 0x40, 0x3D], // 9 red
        [0x63, 0x70, 0x01], // 10 green
        [0x8A, 0x63, 0x00], // 11 yellow
        [0x2C, 0x70, 0x96], // 12 blue
        [0xA0, 0x4C, 0x86], // 13 magenta
        [0x26, 0x77, 0x59], // 14 cyan
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
    border_focused: OutlineColor([0x66, 0xD9, 0xEF, 0xFF]),
    border_idle: OutlineColor([0x6E, 0x70, 0x66, 0xFF]),
    ansi: [
        [0x27, 0x28, 0x22], // 0 black
        [0xFA, 0x49, 0x88], // 1 red
        [0xA6, 0xE2, 0x2E], // 2 green
        [0xE6, 0xDB, 0x74], // 3 yellow
        [0xFD, 0x97, 0x1F], // 4 blue
        [0xAE, 0x81, 0xFF], // 5 magenta
        [0x66, 0xD9, 0xEF], // 6 cyan
        [0xFD, 0xFF, 0xF1], // 7 white
        [0x6E, 0x70, 0x66], // 8 black
        [0xFA, 0x49, 0x88], // 9 red
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
    cursor: [0xBD, 0xA8, 0xE0],
    selection: [0x40, 0x5F, 0x78],
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
    cursor: [0x5B, 0x9C, 0xF9],
    selection: [0x26, 0x4F, 0x78],
    border_focused: OutlineColor([0x56, 0xD4, 0xDD, 0xFF]),
    border_idle: OutlineColor([0x48, 0x4F, 0x58, 0xFF]),
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
    selection: [0xB6, 0xD7, 0xFF],
    border_focused: OutlineColor([0x09, 0x69, 0xDA, 0xFF]),
    border_idle: OutlineColor([0x1F, 0x23, 0x28, 0xFF]),
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
        [0x1C, 0x76, 0xD9], // 12 blue
        [0x88, 0x61, 0xCE], // 13 magenta
        [0x2B, 0x80, 0x96], // 14 cyan
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

/// Parsed `appearance.theme` selection (CTX-0951, issue #1669).
///
/// Two shapes, both plain strings, so the Lua extractor, CLI `--theme`,
/// the top-level `theme` alias, merge, and reload all pass the value
/// through unchanged (no new config syntax, no `bitty-lua` change):
///
/// - single: `"catppuccin-mocha"` (existing behavior; case-insensitive,
///   surrounding whitespace ignored).
/// - dual: `"light:<name>,dark:<name>"` (ghostty-compatible; either order,
///   whitespace around every token ignored, both halves required). The
///   runtime resolves the half matching the OS appearance and swaps the
///   live palette when it toggles; an unknown OS appearance resolves to
///   the dark half (dark-first default).
///
/// Names are stored normalized (trimmed, lowercased) exactly as
/// [`normalize_theme_name`] produces them, so `find_preset` matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSelection {
    /// One preset for every OS appearance.
    Single(String),
    /// Per-appearance presets swapped live on OS toggle.
    Dual {
        /// Preset used under a light OS appearance.
        light: String,
        /// Preset used under a dark (or unknown) OS appearance.
        dark: String,
    },
}

impl ThemeSelection {
    /// Every preset name this selection references (one for [`Self::Single`],
    /// two for [`Self::Dual`]).
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::Single(name) => vec![name.as_str()],
            Self::Dual { light, dark } => vec![light.as_str(), dark.as_str()],
        }
    }

    /// The preset name active under `prefer_light` (`true` = light OS
    /// appearance; `false` = dark or unknown, the dark-first default).
    #[must_use]
    pub fn select(&self, prefer_light: bool) -> &str {
        match self {
            Self::Single(name) => name.as_str(),
            Self::Dual { light, dark } => {
                if prefer_light {
                    light.as_str()
                } else {
                    dark.as_str()
                }
            }
        }
    }
}

/// Fail-closed parse error for [`parse_theme_selection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeSelectionError {
    message: String,
}

impl ThemeSelectionError {
    /// Human-readable diagnostic (naming the offending value and the valid set).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ThemeSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ThemeSelectionError {}

/// Parses a raw `appearance.theme` value into a [`ThemeSelection`].
///
/// A value containing `,` or starting with a `light:`/`dark:` side prefix
/// parses as the dual shape: every comma-separated entry must be
/// `light:<name>` or `dark:<name>` (side keys case-insensitive, values
/// trimmed, duplicates rejected) and both sides must be present. Anything
/// else parses as [`ThemeSelection::Single`]. Empty/whitespace-only input
/// and malformed dual values fail closed with a diagnostic naming the
/// offending value; preset membership is NOT checked here (see
/// [`unknown_theme_diagnostic`] and `AppearanceConfig::validate`).
pub fn parse_theme_selection(raw: &str) -> Result<ThemeSelection, ThemeSelectionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ThemeSelectionError {
            message: "appearance.theme must be non-empty after trimming".to_string(),
        });
    }
    let lowered_head = trimmed.to_lowercase();
    let looks_dual = trimmed.contains(',')
        || lowered_head.starts_with("light:")
        || lowered_head.starts_with("dark:");
    if !looks_dual {
        return Ok(ThemeSelection::Single(trimmed.to_lowercase()));
    }
    let malformed = |detail: &str| ThemeSelectionError {
        message: format!(
            "malformed appearance.theme '{trimmed}': {detail}; \
                 expected '<name>' or 'light:<name>,dark:<name>'"
        ),
    };
    let mut light: Option<String> = None;
    let mut dark: Option<String> = None;
    for part in trimmed.split(',') {
        let (key, value) = part
            .split_once(':')
            .ok_or_else(|| malformed("every entry must be 'light:<name>' or 'dark:<name>'"))?;
        let key = key.trim().to_lowercase();
        let value = value.trim();
        if value.is_empty() {
            return Err(malformed("preset names must be non-empty"));
        }
        let slot = match key.as_str() {
            "light" => &mut light,
            "dark" => &mut dark,
            _ => {
                return Err(malformed("side keys must be 'light' or 'dark'"));
            }
        };
        if slot.is_some() {
            return Err(malformed("duplicate side entry"));
        }
        *slot = Some(value.to_lowercase());
    }
    match (light, dark) {
        (Some(light), Some(dark)) => Ok(ThemeSelection::Dual { light, dark }),
        _ => Err(malformed(
            "both 'light:<name>' and 'dark:<name>' are required",
        )),
    }
}

/// Whether `raw` names a known registry preset or alias (case-insensitive,
/// whitespace-tolerant). `false` for empty, overlong, or unknown values.
#[must_use]
pub fn is_known_theme_name(raw: &str) -> bool {
    let Some(normalized) = normalize_theme_name(Some(raw)) else {
        return false;
    };
    find_preset(&normalized).is_some()
}

/// Explicit namespace prefix for a user-authored local theme file (OQ-047).
///
/// A local theme is selected only through this prefix
/// (`appearance.theme = "local:sunset"`), so a file can never change the
/// meaning of a built-in preset name and the default/built-in path stays
/// zero-I/O. The prefix is part of the accepted selection grammar and is
/// checked structurally by [`AppearanceConfig::validate`](crate::types::AppearanceConfig::validate);
/// whether the file exists is resolved by [`crate::local_theme`] on the load
/// path, which owns filesystem access.
pub const LOCAL_THEME_PREFIX: &str = "local:";

/// Neutral registry-style name used for an applied local theme.
///
/// The built-in catalog's contrast exemptions are keyed by preset name; a
/// local file must never inherit one, so its effective [`Theme`] carries this
/// non-catalog name.
pub const LOCAL_THEME_BASE_NAME: &str = "local-theme";

/// Maximum bytes of the name after [`LOCAL_THEME_PREFIX`].
///
/// Leaves room for the prefix inside [`crate::types::MAX_THEME_LEN`] and
/// bounds diagnostics and directory lookups.
pub const MAX_LOCAL_THEME_NAME_BYTES: usize =
    crate::types::MAX_THEME_LEN - LOCAL_THEME_PREFIX.len();

/// Whether `name` is a canonical local-theme slug: lowercase ASCII letters,
/// digits, `-`, or `_`, starting with a letter or digit, at most
/// [`MAX_LOCAL_THEME_NAME_BYTES`] bytes.
#[must_use]
pub fn is_valid_local_theme_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_LOCAL_THEME_NAME_BYTES {
        return false;
    }
    let mut bytes = name.bytes();
    let starts_well = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    starts_well
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Parses a `local:<name>` selection into its validated slug.
///
/// Returns `None` for any other spelling (including a built-in name, an
/// empty body, an overlong body, or a malformed slug). The caller owns the
/// filesystem lookup; this function is pure.
#[must_use]
pub fn parse_local_theme_reference(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix(LOCAL_THEME_PREFIX)
        .or_else(|| trimmed.strip_prefix("Local:"))?;
    is_valid_local_theme_name(body).then_some(body)
}

/// Whether `raw` is a syntactically valid local-theme selection.
///
/// Structural only: an existing `themes/<name>.toml` is not required here.
#[must_use]
pub fn is_local_theme_reference(raw: &str) -> bool {
    parse_local_theme_reference(raw).is_some()
}

/// Whether a parsed `appearance.theme` value (single or dual) references at
/// least one `local:<name>` selection.
///
/// Structural only; the load path owns file existence and parsing.
#[must_use]
pub fn is_local_theme_selection(raw: &str) -> bool {
    parse_theme_selection(raw).ok().is_some_and(|selection| {
        selection
            .names()
            .iter()
            .any(|name| is_local_theme_reference(name))
    })
}

/// Fail-closed diagnostic for an unknown preset name: names the offender
/// and the full valid set in registry order, plus the dual-shape syntax.
#[must_use]
pub fn unknown_theme_diagnostic(raw: &str) -> String {
    let mut names = String::new();
    for (i, preset) in ALL_PRESETS.iter().enumerate() {
        if i > 0 {
            names.push_str(", ");
        }
        names.push_str(preset.name);
    }
    format!(
        "unknown theme '{}'; expected one of: {names} (or 'local:<name>' for a user theme file; or 'light:<name>,dark:<name>' for OS auto-switching; run 'bitty list themes' for the catalog)",
        raw.trim()
    )
}

/// Resolves an `appearance.theme` value plus the OS appearance to a preset.
///
/// - `None`/empty/whitespace → [`BITTY_DARK`] (dark-first default).
/// - Single → [`resolve_theme`] on the name (unknown falls back + logs).
/// - Dual → [`resolve_theme`] on the half matching `prefer_light`
///   (`false` covers dark and unknown OS appearances).
///
/// Last-resort runtime safety only: config load/reload validation
/// (`AppearanceConfig::validate`) already rejects unknown names fail-closed,
/// so reaching the fallback here means the value bypassed validation.
#[must_use]
pub fn resolve_selection(name: Option<&str>, prefer_light: bool) -> &'static Theme {
    let raw = match name {
        Some(raw) if !raw.trim().is_empty() => raw,
        _ => return &BITTY_DARK,
    };
    match parse_theme_selection(raw) {
        Ok(selection) => resolve_theme(Some(selection.select(prefer_light))),
        Err(e) => {
            eprintln!(
                "bitty: {}; falling back to '{DEFAULT_THEME_NAME}'",
                e.message()
            );
            &BITTY_DARK
        }
    }
}

/// Resolves an `appearance.theme` value plus the OS appearance to a preset
/// with its [`ThemeResolution`] (CTX-0951 NEEDS-FIX, issue #1669).
///
/// Same selection rule as [`resolve_selection`] but status-aware for the
/// startup/title path (`load_app_config`): `None`/empty → `Default`, a known
/// single name or a dual with both halves known → `Named` on the active
/// half, anything else (unknown single, either dual half unknown, malformed
/// dual) → `FallbackUnknown` on [`BITTY_DARK`]. Pure (no logging): the
/// caller logs the fallback like [`resolve_theme_with_status`] does.
///
/// Validation already rejects unknown/malformed values fail-closed, so
/// `FallbackUnknown` here only fires for values that bypassed validation.
#[must_use]
pub fn resolve_selection_with_status(
    name: Option<&str>,
    prefer_light: bool,
) -> (&'static Theme, ThemeResolution) {
    let raw = match name {
        Some(raw) if !raw.trim().is_empty() => raw,
        _ => return (&BITTY_DARK, ThemeResolution::Default),
    };
    let selection = match parse_theme_selection(raw) {
        Ok(selection) => selection,
        Err(_) => return (&BITTY_DARK, ThemeResolution::FallbackUnknown),
    };
    // Both dual halves must be known (validation enforces this fail-closed);
    // an unknown inactive half still poisons the status so a later OS toggle
    // cannot silently land on a fallback.
    for half in selection.names() {
        if !is_known_theme_name(half) {
            return (&BITTY_DARK, ThemeResolution::FallbackUnknown);
        }
    }
    resolve_theme_with_status(Some(selection.select(prefer_light)))
}

/// Bounded user-supplied palette (CTX-0392, issue #648).
///
/// Two accepted sources share this shape:
///
/// - the inline `appearance.colors` table evaluated from the already-trusted
///   user config file (`init.lua`/`config.lua` via the `bitty-lua` sandbox);
/// - a bounded local theme file under
///   `$XDG_CONFIG_HOME/bitty/themes/*.toml` (CTX-2464, OQ-047), read by
///   [`crate::local_theme`] and selected explicitly as `local:<name>`.
///
/// The inline table never supplies outline tokens; a local theme file may
/// supply `border_focused`/`border_idle` explicitly. Both optional tokens
/// default to `None` (the selected built-in base's outline pair), so the
/// existing preset derivation is unchanged for inline palettes.
///
/// Shape (fail-closed, fixed, no growth):
/// `background`, `foreground`, `cursor`, `selection` plus exactly 16 ANSI
/// entries (`ansi[16]`). Every color leaf is a canonical `#RRGGBB` hex string
/// in config; this struct stores the parsed `sRGB` bytes. `#RGB`, `#RRGGBBAA`,
/// named colors, `rgb()` syntax, and any other spelling are rejected by
/// [`Self::parse_hex_rgb`] (never coerced, never clamped). Missing leaves
/// or a short/long `ansi` array reject the whole reload fail-closed, so a
/// malformed palette can never corrupt live state.
///
/// The AC-1/AC-2 outline invariants are enforced by
/// `EffectiveConfig::validate` against the custom background (same floors
/// as presets: focused `>= 3:1` vs background, focused `>= 3:1` vs idle
/// unless the width cue holds); this type owns only structural validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomPalette {
    /// Window clear color and default cell background.
    pub background: [u8; 3],
    /// Default glyph color.
    pub foreground: [u8; 3],
    /// Block cursor fill.
    pub cursor: [u8; 3],
    /// Selection background fill.
    pub selection: [u8; 3],
    /// The 16 ANSI colors, indices 0-15.
    pub ansi: [[u8; 3]; 16],
    /// Optional focused outline token (local theme files only).
    pub border_focused: Option<OutlineColor>,
    /// Optional idle outline token (local theme files only).
    pub border_idle: Option<OutlineColor>,
    /// Optional dark/light classification for a local theme file.
    ///
    /// Inline `appearance.colors` and plugin palettes leave this `None` and
    /// keep the selected preset's category; a local theme file carries its own
    /// category so the readability policy classifies ANSI 7/15 correctly.
    pub category: Option<ThemeCategory>,
}

/// Expected ANSI entry count for [`CustomPalette`].
pub const CUSTOM_PALETTE_ANSI_COUNT: usize = 16;

/// Typed [`CustomPalette`] construction failure (CTX-1395, issue #2333).
///
/// Every variant names the exact dotted field path the caller must fix, so a
/// rejected `appearance.colors` table produces one actionable diagnostic
/// instead of a generic "invalid palette" message. The path grammar is the
/// same one used by the rest of `bitty-config` (`appearance.colors.ansi[7]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomPaletteError {
    /// A required top-level role is absent from the table.
    MissingRole(&'static str),
    /// A leaf is not canonical `#RRGGBB` hex.
    InvalidHex {
        /// Dotted field path of the offending leaf.
        field: &'static str,
        /// ANSI index when the leaf is an `ansi` entry.
        index: Option<usize>,
    },
    /// The `ansi` array does not contain exactly [`CUSTOM_PALETTE_ANSI_COUNT`]
    /// entries.
    AnsiCount(usize),
}

impl CustomPaletteError {
    /// Dotted field path of the failure (stable diagnostic key).
    #[must_use]
    pub fn field_path(&self) -> String {
        match self {
            Self::MissingRole(role) => format!("appearance.colors.{role}"),
            Self::InvalidHex { field, index } => match index {
                Some(index) => format!("{field}[{index}]"),
                None => (*field).to_string(),
            },
            Self::AnsiCount(_) => "appearance.colors.ansi".to_string(),
        }
    }

    /// Developer-facing English reason (bounded, no config values echoed).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingRole("ansi") => {
                "table 'appearance.colors' requires 'ansi' as an array of 16 \"#RRGGBB\" strings"
                    .to_string()
            }
            Self::MissingRole(role) => {
                format!("table 'appearance.colors' requires '{role}' as a \"#RRGGBB\" string")
            }
            Self::InvalidHex { .. } => {
                "must be canonical \"#RRGGBB\" hex (7 characters, no alpha)".to_string()
            }
            Self::AnsiCount(found) => {
                format!("must contain exactly {CUSTOM_PALETTE_ANSI_COUNT} entries (found {found})")
            }
        }
    }
}

impl std::fmt::Display for CustomPaletteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field_path(), self.message())
    }
}

impl CustomPalette {
    /// Parses one canonical `#RRGGBB` spelling into `sRGB` bytes.
    ///
    /// Fail-closed: `None` for any other grammar (missing `#`, wrong digit
    /// count, non-hex bytes, `#RGB`, `#RRGGBBAA`, named colors). Trims
    /// surrounding whitespace like the outline parser; overlong input is
    /// rejected, never truncated.
    #[must_use]
    pub fn parse_hex_rgb(raw: &str) -> Option<[u8; 3]> {
        let trimmed = raw.trim();
        if trimmed.len() != 7 {
            return None;
        }
        let body = trimmed.strip_prefix('#')?;
        if body.len() != 6 {
            return None;
        }
        let bytes = body.as_bytes();
        let hex_byte = |start: usize| -> Option<u8> {
            let hi = (*bytes.get(start)? as char).to_digit(16)?;
            let lo = (*bytes.get(start + 1)? as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        };
        Some([hex_byte(0)?, hex_byte(2)?, hex_byte(4)?])
    }

    /// Builds a palette from already-split hex spellings.
    ///
    /// `ansi` must contain exactly [`CUSTOM_PALETTE_ANSI_COUNT`] entries.
    /// `None` on any malformed leaf or wrong `ansi` length (fail-closed).
    /// This compatibility wrapper discards the typed diagnostic; config
    /// loading uses [`Self::try_from_hex`] so the exact leaf is named.
    #[must_use]
    pub fn from_hex(
        background: &str,
        foreground: &str,
        cursor: &str,
        selection: &str,
        ansi: &[String],
    ) -> Option<Self> {
        Self::try_from_hex(
            Some(background),
            Some(foreground),
            Some(cursor),
            Some(selection),
            Some(ansi),
        )
        .ok()
    }

    /// Typed, fail-closed palette construction (CTX-1395, issue #2333).
    ///
    /// Every role is required: `background`, `foreground`, `cursor`,
    /// `selection`, and exactly [`CUSTOM_PALETTE_ANSI_COUNT`] `ansi` entries.
    /// The first missing role, wrong ANSI count, or non-canonical leaf wins and
    /// is reported as a [`CustomPaletteError`] naming the exact field path.
    /// Missing roles never inherit the previous theme's value: the whole table
    /// is rejected, so a partial palette is unrepresentable.
    ///
    /// # Errors
    ///
    /// [`CustomPaletteError::MissingRole`] when a role is `None`,
    /// [`CustomPaletteError::AnsiCount`] for a wrong-length array, and
    /// [`CustomPaletteError::InvalidHex`] for a leaf outside `#RRGGBB`.
    pub fn try_from_hex(
        background: Option<&str>,
        foreground: Option<&str>,
        cursor: Option<&str>,
        selection: Option<&str>,
        ansi: Option<&[String]>,
    ) -> Result<Self, CustomPaletteError> {
        let background = background.ok_or(CustomPaletteError::MissingRole("background"))?;
        let foreground = foreground.ok_or(CustomPaletteError::MissingRole("foreground"))?;
        let cursor = cursor.ok_or(CustomPaletteError::MissingRole("cursor"))?;
        let selection = selection.ok_or(CustomPaletteError::MissingRole("selection"))?;
        let ansi = ansi.ok_or(CustomPaletteError::MissingRole("ansi"))?;
        if ansi.len() != CUSTOM_PALETTE_ANSI_COUNT {
            return Err(CustomPaletteError::AnsiCount(ansi.len()));
        }
        let mut parsed = [[0u8; 3]; CUSTOM_PALETTE_ANSI_COUNT];
        for (index, (slot, raw)) in parsed.iter_mut().zip(ansi.iter()).enumerate() {
            *slot = Self::parse_hex_rgb(raw).ok_or(CustomPaletteError::InvalidHex {
                field: "appearance.colors.ansi",
                index: Some(index),
            })?;
        }
        let role = |name: &'static str, raw: &str| {
            Self::parse_hex_rgb(raw).ok_or(CustomPaletteError::InvalidHex {
                field: name,
                index: None,
            })
        };
        Ok(Self {
            background: role("appearance.colors.background", background)?,
            foreground: role("appearance.colors.foreground", foreground)?,
            cursor: role("appearance.colors.cursor", cursor)?,
            selection: role("appearance.colors.selection", selection)?,
            ansi: parsed,
            border_focused: None,
            border_idle: None,
            category: None,
        })
    }

    /// Canonical `#RRGGBB` spelling of one stored entry.
    #[must_use]
    pub fn hex_entry(rgb: [u8; 3]) -> String {
        format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
    }
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

    /// Presets whose palette cannot clear the advisory AC-3 floor
    /// (idle >= 1.5:1 against the background) while keeping the enforced
    /// AC-1/AC-2 invariants, with the reason. The exemption is explicit and
    /// measured: `bitty config check` still reports the advisory, so it can
    /// never become a silent runtime pass.
    const AC3_EXEMPTIONS: &[(&str, &str)] = &[(
        "tokyo-night-day",
        "no palette surface or selection->focused blend clears AC-3 while \
         holding AC-2 against focused #007197; the compliant near-black idle \
         would sit outside this light palette's tone, so the visible \
         selection idle #B7C1E3 (1.38:1) is retained",
    )];

    fn ac3_exemption(theme: &Theme) -> Option<&'static str> {
        AC3_EXEMPTIONS
            .iter()
            .find(|(name, _)| *name == theme.name)
            .map(|(_, reason)| *reason)
    }

    #[test]
    fn local_theme_reference_is_structural_and_explicit() {
        assert_eq!(parse_local_theme_reference("local:sunset"), Some("sunset"));
        assert_eq!(
            parse_local_theme_reference(" local:dark_mint "),
            Some("dark_mint")
        );
        assert_eq!(parse_local_theme_reference("Local:sunset"), Some("sunset"));
        for bad in [
            "",
            "sunset",
            "local:",
            "local:Sunset",
            "local:-sunset",
            "local:../escape",
            "light:local:sunset,dark:dracula",
        ] {
            assert_eq!(parse_local_theme_reference(bad), None, "{bad:?}");
        }
        assert!(is_local_theme_reference("local:sunset"));
        assert!(!is_local_theme_reference("local:../escape"));

        let mut appearance = crate::types::AppearanceConfig {
            theme: Some(String::from("local:sunset")),
            ..crate::types::AppearanceConfig::default()
        };
        appearance.validate().expect("structural local selection");
        appearance.theme = Some(String::from("local:../escape"));
        assert!(
            appearance.validate().is_err(),
            "malformed slug fails closed"
        );
        appearance.theme = Some(String::from("not-a-theme"));
        assert!(
            appearance.validate().is_err(),
            "unknown built-in fails closed"
        );
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

    // -- dual light/dark selection (CTX-0951) ----------------------------

    #[test]
    fn single_selection_parses_case_insensitive() {
        let selection = parse_theme_selection("  Catppuccin-Mocha ").expect("single parses");
        assert_eq!(
            selection,
            ThemeSelection::Single("catppuccin-mocha".to_string())
        );
        assert_eq!(selection.names(), vec!["catppuccin-mocha"]);
        assert_eq!(selection.select(true), "catppuccin-mocha");
        assert_eq!(selection.select(false), "catppuccin-mocha");
    }

    #[test]
    fn dual_selection_parses_either_order_with_whitespace() {
        for input in [
            "light:catppuccin-latte,dark:catppuccin-mocha",
            "dark:catppuccin-mocha,light:catppuccin-latte",
            "  LIGHT : Catppuccin-Latte , Dark : CATPPUCCIN-MOCHA  ",
        ] {
            let selection = parse_theme_selection(input).expect("dual parses");
            assert_eq!(
                selection,
                ThemeSelection::Dual {
                    light: "catppuccin-latte".to_string(),
                    dark: "catppuccin-mocha".to_string(),
                },
                "input: {input:?}"
            );
            assert_eq!(selection.select(true), "catppuccin-latte");
            assert_eq!(selection.select(false), "catppuccin-mocha");
        }
    }

    #[test]
    fn malformed_dual_values_fail_closed() {
        for input in [
            "",
            "   ",
            "light:catppuccin-latte",
            "dark:catppuccin-mocha",
            "light:,dark:catppuccin-mocha",
            "light:catppuccin-latte,dark:",
            "light:catppuccin-latte,light:catppuccin-mocha",
            "dawn:catppuccin-latte,dark:catppuccin-mocha",
            "catppuccin-latte,catppuccin-mocha",
            "light:catppuccin-latte,dark:catppuccin-mocha,light:github-light",
        ] {
            assert!(parse_theme_selection(input).is_err(), "input: {input:?}");
        }
    }

    #[test]
    fn resolve_selection_picks_half_by_appearance() {
        let dual = "light:github-light,dark:catppuccin-mocha";
        assert_eq!(resolve_selection(Some(dual), true).name, "github-light");
        assert_eq!(
            resolve_selection(Some(dual), false).name,
            "catppuccin-mocha"
        );
        // A single name ignores the flag; empty falls back to dark-first.
        assert_eq!(resolve_selection(Some("dracula"), true).name, "dracula");
        assert_eq!(resolve_selection(None, true).name, DEFAULT_THEME_NAME);
        assert_eq!(
            resolve_selection(Some("   "), false).name,
            DEFAULT_THEME_NAME
        );
    }

    #[test]
    fn unknown_theme_diagnostic_names_valid_set() {
        let message = unknown_theme_diagnostic("not-a-theme");
        assert!(message.contains("not-a-theme"), "{message}");
        assert!(message.contains("catppuccin-mocha"), "{message}");
        assert!(message.contains("light:<name>,dark:<name>"), "{message}");
        assert!(is_known_theme_name("catppuccin-mocha"));
        assert!(is_known_theme_name(" Dark "));
        assert!(!is_known_theme_name("not-a-theme"));
        assert!(!is_known_theme_name("   "));
    }

    #[test]
    fn selection_with_status_reports_active_half() {
        use crate::theme::ThemeResolution;
        // Dual resolves to the active half without falling back.
        let (light, status) =
            resolve_selection_with_status(Some("light:github-light,dark:dracula"), true);
        assert_eq!(
            (light.name, status),
            ("github-light", ThemeResolution::Named)
        );
        let (dark, status) =
            resolve_selection_with_status(Some("light:github-light,dark:dracula"), false);
        assert_eq!((dark.name, status), ("dracula", ThemeResolution::Named));
        // Single and default keep the existing statuses.
        let (_, status) = resolve_selection_with_status(Some("dracula"), false);
        assert_eq!(status, ThemeResolution::Named);
        let (_, status) = resolve_selection_with_status(None, false);
        assert_eq!(status, ThemeResolution::Default);
        // Unknown halves and malformed duals stay fail-closed observers.
        let (_, status) = resolve_selection_with_status(Some("not-a-theme"), false);
        assert_eq!(status, ThemeResolution::FallbackUnknown);
        let (_, status) =
            resolve_selection_with_status(Some("light:not-a-theme,dark:dracula"), false);
        assert_eq!(status, ThemeResolution::FallbackUnknown);
        let (_, status) = resolve_selection_with_status(Some("light:dracula"), false);
        assert_eq!(status, ThemeResolution::FallbackUnknown);
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
    fn every_preset_meets_the_readability_policy() {
        // CTX-1418 (#2370): the product policy behind `bitty config check`
        // and this test are the same rows. Any regression — a darkened
        // light-theme ANSI slot, a selection that matches the foreground, a
        // cursor that disappears into the background — fails here.
        for theme in list_presets() {
            let violations = contrast_violations(theme);
            let rendered: Vec<String> = violations
                .iter()
                .map(|row| {
                    format!(
                        "{} {:.2}:1 < {:.1}:1{}",
                        row.role.label(),
                        row.ratio,
                        row.floor,
                        row.exemption.map_or("", |_| " (exempt)")
                    )
                })
                .collect();
            assert!(
                violations.is_empty(),
                "preset {} fails the contrast policy: {}",
                theme.name,
                rendered.join(", ")
            );
        }
    }

    #[test]
    fn documented_exemptions_stay_live_and_measured() {
        // An exemption that no longer measures below its floor is dead
        // policy: remove it instead of letting it hide a future regression.
        for (name, reason) in TEXT_CONTRAST_EXEMPTIONS {
            let theme = resolve_theme_with_status(Some(name)).0;
            assert_eq!(theme.name, *name, "exemption names a catalog preset");
            assert!(
                contrast(theme.foreground, theme.background) < MIN_TEXT_CONTRAST,
                "preset {name} no longer needs its text exemption"
            );
            assert!(!reason.is_empty());
        }
        for (name, reason) in SELECTION_CONTRAST_EXEMPTIONS {
            let theme = resolve_theme_with_status(Some(name)).0;
            assert_eq!(theme.name, *name, "exemption names a catalog preset");
            assert!(
                contrast(theme.foreground, theme.selection) < MIN_TEXT_CONTRAST
                    || contrast(theme.selection, theme.background) < MIN_SELECTION_CONTRAST,
                "preset {name} no longer needs its selection exemption"
            );
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn contrast_checks_cover_every_policy_role_in_order() {
        let checks = contrast_checks(&BITTY_DARK);
        assert_eq!(checks.len(), 23);
        assert_eq!(checks[0].role, ContrastRole::Foreground);
        assert_eq!(checks[1].role, ContrastRole::AnsiSurface(0));
        assert_eq!(checks[8].role, ContrastRole::AnsiText(7));
        assert_eq!(checks[17].role, ContrastRole::Cursor);
        assert_eq!(checks[18].role, ContrastRole::SelectionText);
        assert_eq!(checks[19].role, ContrastRole::SelectionSurface);
        assert_eq!(checks[20].role, ContrastRole::CursorSelection);
        assert_eq!(checks[21].role, ContrastRole::HelpRing);
        assert_eq!(checks[22].role, ContrastRole::BannerText);
        assert!(checks.iter().all(|row| row.passed));
        assert!(min_ansi_text_contrast(&BITTY_DARK) >= MIN_TEXT_CONTRAST);
    }

    #[test]
    fn every_preset_keeps_ansi_families_distinguishable() {
        // CTX-1418 (#2370): a contrast repair must not collapse two ANSI
        // families into the same hue *and* lightness. Cross-family pairs must
        // clear the perceptual distance or the hue separation; normal/bright
        // aliases inside one family are an intentional upstream choice and
        // are skipped.
        for theme in list_presets() {
            let violations = hue_violations(theme);
            let rendered: Vec<String> = violations
                .iter()
                .map(|row| {
                    format!(
                        "ansi[{}]/ansi[{}] dE {:.1} < {:.1} and hue {:.0}deg < {:.0}deg",
                        row.first,
                        row.second,
                        row.delta_e,
                        MIN_ANSI_FAMILY_DELTA_E,
                        row.hue_delta,
                        MIN_ANSI_FAMILY_HUE_DEGREES
                    )
                })
                .collect();
            assert!(
                violations.is_empty(),
                "preset {} collapses ANSI families: {}",
                theme.name,
                rendered.join(", ")
            );
        }
    }

    #[test]
    fn hue_collision_exemptions_stay_live() {
        for (name, reason) in HUE_COLLISION_EXEMPTIONS {
            let theme = resolve_theme_with_status(Some(name)).0;
            assert_eq!(theme.name, *name, "exemption names a catalog preset");
            assert!(
                hue_checks(theme).iter().any(|row| !row.passed),
                "preset {name} no longer needs its hue-collision exemption"
            );
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn hue_checks_compare_every_cross_family_pair() {
        // 12 chromatic slots, minus the 6 normal/bright pairs inside one
        // family: 66 - 6 = 60 measured pairs.
        let checks = hue_checks(&BITTY_DARK);
        assert_eq!(checks.len(), 60);
        assert!(
            checks
                .iter()
                .all(|row| ansi_family(row.first) != ansi_family(row.second))
        );
        // Bitty Dark aliases every bright slot to its normal slot, which the
        // family filter skips rather than reporting as a collision.
        assert!(checks.iter().all(|row| row.passed));
    }

    #[test]
    fn ghostty_aligned_presets_pin_their_canonical_values() {
        // CTX-1418: presets with a canonical Ghostty theme file reproduce
        // that file's palette. Pin the values that historically drifted so a
        // later edit cannot silently undo the alignment. Deviations that the
        // contrast policy requires are pinned too (Catppuccin selection is
        // its upstream surface, not the cursor hue; GitHub's inverse
        // selection is adapted to Bitty's foreground-preserving contract).
        let latte = resolve_theme_with_status(Some("catppuccin-latte")).0;
        assert_eq!(latte.background, [0xEF, 0xF1, 0xF5]);
        assert_eq!(latte.foreground, [0x4C, 0x4F, 0x69]);
        assert_eq!(latte.ansi[0], [0x5C, 0x5F, 0x77]);
        assert_eq!(latte.ansi[15], [0xBC, 0xC0, 0xCC]);
        assert_eq!(latte.selection, [0xBF, 0xC3, 0xCE]);

        let mocha = resolve_theme_with_status(Some("catppuccin-mocha")).0;
        assert_eq!(mocha.selection, [0x58, 0x5B, 0x70]);
        assert_eq!(mocha.ansi[9], [0xF3, 0x77, 0x99]);

        let tokyo = resolve_theme_with_status(Some("tokyo-night")).0;
        assert_eq!(tokyo.selection, [0x33, 0x46, 0x7C]);
        assert_eq!(tokyo.ansi[14], [0x7D, 0xCF, 0xFF]);

        let github = resolve_theme_with_status(Some("github-light")).0;
        assert_eq!(github.selection, [0xB6, 0xD7, 0xFF]);
        assert_eq!(github.ansi[12], [0x1C, 0x76, 0xD9]);

        // Cursor and pine repairs that keep derived chrome readable while
        // preserving the upstream hue (CTX-1418).
        assert_eq!(latte.cursor, [0x93, 0x5C, 0x50]);
        assert_eq!(github.cursor, [0x09, 0x69, 0xDA]);
        let night_owl = resolve_theme_with_status(Some("night-owl")).0;
        assert_eq!(night_owl.cursor, [0xBD, 0xA8, 0xE0]);
        let github_dark = resolve_theme_with_status(Some("github-dark")).0;
        assert_eq!(github_dark.cursor, [0x5B, 0x9C, 0xF9]);
        let dawn = resolve_theme_with_status(Some("rose-pine-dawn")).0;
        assert_eq!(dawn.ansi[2], [0x20, 0x58, 0x70]);
    }

    #[test]
    fn focused_outline_meets_non_text_contrast_floor() {
        // CTX-0340 AC-1: the focused outline must clear 3:1 against the
        // workspace background. The derivation above guarantees it for every
        // preset; this test pins the guarantee so a future edit cannot
        // silently regress it. Contrast uses the runtime `OutlineColor`
        // compositing path, so the measured value is the one startup and
        // `bitty config check` see.
        for theme in list_presets() {
            let ratio = theme.border_focused.contrast_over(theme.background);
            assert!(
                ratio >= 3.0,
                "preset {} focused outline contrast {ratio:.2}:1 is below 3:1",
                theme.name
            );
        }
    }

    #[test]
    fn focused_idle_outline_meets_non_text_contrast_floor() {
        // CTX-0340 AC-2: the focused outline must clear 3:1 against the idle
        // outline. `EffectiveConfig::validate()` enforces this on the resolved
        // pair, so a catalog preset below the floor makes every selection of
        // that preset fail closed at startup. This is the catalog-level guard
        // the `EffectiveConfig` sweep below backs (AC-2, no exemptions).
        // Contrast uses the runtime `OutlineColor` compositing path so both
        // sides are measured exactly as they render over the background.
        for theme in list_presets() {
            let ratio = theme
                .border_focused
                .contrast_with(theme.border_idle, theme.background);
            assert!(
                ratio >= 3.0,
                "preset {} focused/idle outline contrast {ratio:.2}:1 is below 3:1 \
                 (AC-2); re-derive the pair",
                theme.name
            );
        }
    }

    #[test]
    fn idle_outline_clears_advisory_background_floor_or_is_documented() {
        // CTX-0340 AC-3 (idle >= 1.5:1 against the background) is advisory,
        // but after CTX-0354 every shipped preset must clear it unless it is
        // listed in `AC3_EXEMPTIONS`. An exempt preset must measure below the
        // floor (no stale exemption) and stay visible (never equal to the
        // background), and a fixed preset must clear the floor. Contrast is
        // computed exactly as the runtime does (`OutlineColor` composited
        // over the background), so an alpha idle cannot pass this test and
        // then fail at `bitty config check`.
        for theme in list_presets() {
            let ratio = theme.border_idle.contrast_over(theme.background);
            assert_ne!(
                theme.border_idle.composited_over(theme.background),
                theme.background,
                "preset {} idle outline is invisible against the background",
                theme.name
            );
            if let Some(reason) = ac3_exemption(theme) {
                assert!(
                    ratio < 1.5,
                    "preset {} is listed as an AC-3 exemption but measures \
                     {ratio:.2}:1; remove the exemption",
                    theme.name
                );
                assert!(!reason.is_empty());
            } else {
                assert!(
                    ratio >= 1.5,
                    "preset {} idle outline contrast {ratio:.2}:1 is below the \
                     advisory AC-3 floor 1.5:1; re-derive the idle or add a \
                     documented AC3_EXEMPTIONS entry",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn every_preset_passes_effective_config_validation() {
        // Integration-level guard: selecting a preset must not make the fully
        // resolved effective config fail closed. `EffectiveConfig::validate`
        // resolves the outline against `appearance.theme` and enforces the
        // CTX-0340 AC-1/AC-2 contract, so this sweep is exactly what would
        // have caught a catalog pair violating AC-2 at review time.
        use crate::types::EffectiveConfig;
        for theme in list_presets() {
            let effective = EffectiveConfig {
                appearance: crate::types::AppearanceConfig {
                    theme: Some(theme.name.to_owned()),
                    ..crate::types::AppearanceConfig::default()
                },
                ..EffectiveConfig::default()
            };
            if let Err(error) = effective.validate() {
                panic!(
                    "preset {} fails EffectiveConfig::validate(): {error}",
                    theme.name
                );
            }
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

    /// A preset's accepted RGB values:
    /// `(bg, fg, cursor, selection, ansi0, ansi1, ansi4, ansi15)`.
    ///
    /// These are the values the product ships after CTX-1418: Ghostty's
    /// canonical theme file where one exists, with the minimal readability
    /// repair recorded in `bitty-terminal-docs`. Updating a row is a palette
    /// change and needs the same contrast evidence as the original landed
    /// value.
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
    fn known_presets_pin_their_accepted_values() {
        let checks: &[(&str, ExpectedColors)] = &[
            (
                "tokyo-night",
                (
                    [0x1A, 0x1B, 0x26],
                    [0xC0, 0xCA, 0xF5],
                    [0xC0, 0xCA, 0xF5],
                    [0x33, 0x46, 0x7C],
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
                    [0x58, 0x5B, 0x70],
                    [0x45, 0x47, 0x5A],
                    [0xF3, 0x8B, 0xA8],
                    [0x89, 0xB4, 0xFA],
                    [0xBA, 0xC2, 0xDE],
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
                    [0x5B, 0x9C, 0xF9],
                    [0x26, 0x4F, 0x78],
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
                    [0xB6, 0xD7, 0xFF],
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
                    [0x43, 0x4C, 0x5E],
                    [0x3B, 0x42, 0x52],
                    [0xCF, 0x89, 0x8F],
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
                    [0xFA, 0x49, 0x88],
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

    // CTX-0392: bounded inline custom palette (fail-closed hex, 16 entries).
    #[test]
    fn custom_palette_hex_parses_canonical_only() {
        assert_eq!(
            CustomPalette::parse_hex_rgb("#1e1e2e"),
            Some([0x1E, 0x1E, 0x2E])
        );
        assert_eq!(
            CustomPalette::parse_hex_rgb("  #CDD6F4  "),
            Some([0xCD, 0xD6, 0xF4])
        );
        // Fail-closed: every non-canonical spelling rejects.
        for bad in [
            "",
            "1e1e2e",
            "#1e1",
            "#1e1e2eAA",
            "#gggggg",
            "red",
            "rgb(1,2,3)",
            "#12345",
            "#1234567",
            "# 1e1e2e",
        ] {
            assert_eq!(
                CustomPalette::parse_hex_rgb(bad),
                None,
                "must reject {bad:?}"
            );
        }
    }

    #[test]
    fn custom_palette_requires_16_ansi() {
        let ansi16: Vec<String> = (0..16).map(|_| "#112233".to_string()).collect();
        let ansi15: Vec<String> = (0..15).map(|_| "#112233".to_string()).collect();
        let ansi17: Vec<String> = (0..17).map(|_| "#112233".to_string()).collect();
        assert!(
            CustomPalette::from_hex("#1e1e2e", "#cdd6f4", "#f5e0dc", "#313244", &ansi16).is_some()
        );
        assert!(
            CustomPalette::from_hex("#1e1e2e", "#cdd6f4", "#f5e0dc", "#313244", &ansi15).is_none(),
            "15 entries must fail closed"
        );
        assert!(
            CustomPalette::from_hex("#1e1e2e", "#cdd6f4", "#f5e0dc", "#313244", &ansi17).is_none(),
            "17 entries must fail closed"
        );
        // Malformed leaf fails the whole palette (no partial state).
        assert!(
            CustomPalette::from_hex("#1e1e2e", "not-a-color", "#f5e0dc", "#313244", &ansi16)
                .is_none()
        );
    }

    // CTX-1395: every rejection names the exact missing/malformed leaf so
    // `appearance.colors` diagnostics stay actionable.
    #[test]
    fn custom_palette_typed_errors_name_exact_leaf() {
        let ansi16: Vec<String> = (0..16).map(|_| "#112233".to_string()).collect();
        let missing = CustomPalette::try_from_hex(
            Some("#1e1e2e"),
            None,
            Some("#f5e0dc"),
            Some("#313244"),
            Some(&ansi16),
        )
        .expect_err("missing foreground rejects");
        assert_eq!(missing.field_path(), "appearance.colors.foreground");
        assert!(missing.message().contains("requires 'foreground'"));

        let short = CustomPalette::try_from_hex(
            Some("#1e1e2e"),
            Some("#cdd6f4"),
            Some("#f5e0dc"),
            Some("#313244"),
            Some(&ansi16[..15]),
        )
        .expect_err("short ansi rejects");
        assert_eq!(short, CustomPaletteError::AnsiCount(15));
        assert_eq!(short.field_path(), "appearance.colors.ansi");
        assert!(short.message().contains("found 15"));

        let mut bad = ansi16.clone();
        bad[7] = "not-a-color".to_string();
        let invalid = CustomPalette::try_from_hex(
            Some("#1e1e2e"),
            Some("#cdd6f4"),
            Some("#f5e0dc"),
            Some("#313244"),
            Some(&bad),
        )
        .expect_err("bad ansi entry rejects");
        assert_eq!(invalid.field_path(), "appearance.colors.ansi[7]");
        assert!(invalid.message().contains("#RRGGBB"));

        let background = CustomPalette::try_from_hex(
            Some("#12345"),
            Some("#cdd6f4"),
            Some("#f5e0dc"),
            Some("#313244"),
            Some(&ansi16),
        )
        .expect_err("bad background rejects");
        assert_eq!(background.field_path(), "appearance.colors.background");
    }
}
