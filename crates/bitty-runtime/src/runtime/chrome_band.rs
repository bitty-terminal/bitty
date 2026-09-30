//! Core-owned window chrome band (CTX-0873, issue #1431).
//!
//! The workspace bar used to be painted into the last row of every leaf's
//! present snapshot, occluding terminal content. It now owns a dedicated
//! band carved out of the window grid *before* layout: the layout container
//! is the window minus the band, so every leaf, the primary grid, and every
//! PTY winsize are sized by the normal reflow path and no terminal cell is
//! ever painted under the bar.
//!
//! The geometry is four-sided ([`ChromeInsets`]) so left/right bands can be
//! added later without reshaping callers; only [`BarEdge::Top`] and
//! [`BarEdge::Bottom`] exist today. Everything here is pure, total, and
//! headless: hostile or tiny windows hide the band instead of producing a
//! zero or negative content extent.

use crate::config::BarEdge;
use bitty_term_state::Rgb;
use bitty_ui::Rect as UiRect;

/// Minimum terminal content rows a leaf must keep once a band is reserved,
/// before outer gaps and decoration (see [`min_container_rows`]). A window
/// shorter than the band plus the effective minimum hides the band
/// (fail-open for content: the terminal always keeps at least this much).
pub const MIN_CONTENT_ROWS: u16 = 1;

/// Minimum terminal content columns required before any band is reserved.
pub const MIN_CONTENT_COLS: u16 = 1;

/// Cells reserved on each window edge by Core chrome (window cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromeInsets {
    /// Rows reserved at the top edge.
    pub top: u16,
    /// Rows reserved at the bottom edge.
    pub bottom: u16,
    /// Columns reserved at the left edge (always `0` today).
    pub left: u16,
    /// Columns reserved at the right edge (always `0` today).
    pub right: u16,
}

impl ChromeInsets {
    /// No reserved chrome.
    pub const NONE: Self = Self {
        top: 0,
        bottom: 0,
        left: 0,
        right: 0,
    };

    /// Insets reserving `thickness` cells on `edge`.
    #[must_use]
    pub const fn for_edge(edge: BarEdge, thickness: u16) -> Self {
        match edge {
            BarEdge::Top => Self {
                top: thickness,
                ..Self::NONE
            },
            BarEdge::Bottom => Self {
                bottom: thickness,
                ..Self::NONE
            },
        }
    }

    /// Content rectangle left after removing the insets from `window`, or
    /// `None` when the remainder would fall below [`MIN_CONTENT_COLS`]
    /// columns or `min_rows` rows (the caller then hides the band;
    /// `min_rows` is floored at [`MIN_CONTENT_ROWS`]). Saturating: never
    /// wraps, never yields a zero extent.
    #[must_use]
    pub fn content_of(self, window: UiRect, min_rows: u16) -> Option<UiRect> {
        let vertical = u32::from(self.top) + u32::from(self.bottom);
        let horizontal = u32::from(self.left) + u32::from(self.right);
        let rows = u32::from(window.height).checked_sub(vertical)?;
        let cols = u32::from(window.width).checked_sub(horizontal)?;
        let min_rows = u32::from(min_rows.max(MIN_CONTENT_ROWS));
        if rows < min_rows || cols < u32::from(MIN_CONTENT_COLS) {
            return None;
        }
        // Bounded by the window extent above, so the narrowing is lossless;
        // the origin shift saturates at the u16 edge for hostile origins.
        Some(UiRect::new(
            window.x.saturating_add(self.left),
            window.y.saturating_add(self.top),
            saturate_u16(cols),
            saturate_u16(rows),
        ))
    }
}

fn saturate_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Effective minimum container rows before a band may be reserved: the
/// [`MIN_CONTENT_ROWS`] content floor plus the outer cell gap on both
/// sides (`layout.gaps_out`) plus the vertical pixel decoration
/// (`decoration_px`, both sides summed, at the live DPI scale) rounded up
/// to whole rows at the live cell height `cell_h`.
///
/// Without this, a window that keeps [`MIN_CONTENT_ROWS`] container rows
/// could still lose every content row to gaps and the decoration ring.
/// Total and saturating; a zero `cell_h` counts the decoration as no rows.
#[must_use]
pub fn min_container_rows(gaps_out_cells: u16, decoration_px: f64, cell_h: u32) -> u16 {
    let gaps = u32::from(gaps_out_cells).saturating_mul(2);
    let deco_rows = if cell_h == 0 || !decoration_px.is_finite() || decoration_px <= 0.0 {
        0
    } else {
        // Bounded by the u16 clamp below; the float is finite and positive.
        let rows = (decoration_px / f64::from(cell_h)).ceil();
        if rows >= f64::from(u16::MAX) {
            u32::from(u16::MAX)
        } else {
            rows as u32
        }
    };
    saturate_u16(
        u32::from(MIN_CONTENT_ROWS)
            .saturating_add(gaps)
            .saturating_add(deco_rows),
    )
}

/// Solved chrome geometry for one window grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeLayout {
    /// Layout container (window minus every reserved band).
    pub container: UiRect,
    /// The workspace bar band in window cells, when reserved.
    pub bar: Option<UiRect>,
}

/// Solves the layout container and the bar band for `window`.
///
/// `present` is the bar presence predicate (visible and more than one
/// workspace). An absent bar, a zero `thickness`, or a window too small to
/// keep `min_rows` container rows (see [`min_container_rows`]) reserves
/// nothing: the container is the full window. Pure and deterministic.
#[must_use]
pub fn solve(
    window: UiRect,
    edge: BarEdge,
    thickness: u16,
    present: bool,
    min_rows: u16,
) -> ChromeLayout {
    let full = ChromeLayout {
        container: window,
        bar: None,
    };
    if !present || thickness == 0 {
        return full;
    }
    let Some(container) = ChromeInsets::for_edge(edge, thickness).content_of(window, min_rows)
    else {
        return full;
    };
    let bar_y = match edge {
        BarEdge::Top => window.y,
        BarEdge::Bottom => container.y.saturating_add(container.height),
    };
    ChromeLayout {
        container,
        bar: Some(UiRect::new(window.x, bar_y, window.width, thickness)),
    }
}

/// Cells left empty before the first workspace pill (CTX-0874).
pub const BAR_LEFT_MARGIN: u16 = 1;

/// Blank cells between two adjacent workspace pills (CTX-0874).
pub const PILL_GAP: u16 = 1;

/// Background-filled cells on each side of a pill label (CTX-0874).
pub const PILL_PAD: u16 = 1;

/// Theme token the active pill uses when `workspace.bar.colors.active` is
/// unset (CTX-0874).
pub const DEFAULT_ACTIVE_TOKEN: &str = "accent";

/// Theme token inactive pills use when `workspace.bar.colors.inactive` is
/// unset (CTX-0874).
pub const DEFAULT_INACTIVE_TOKEN: &str = "surface.1";

/// Placeholder RGB for the theme tokens the bar accepts (CTX-0874).
///
/// Theme token resolution against the active theme is a follow-up; until
/// it lands these fixed values (matching the default dark palette) stand
/// in. Unknown tokens resolve to `None` and the caller falls back to the
/// default token.
const PLACEHOLDER_TOKENS: &[(&str, [u8; 3])] = &[
    ("accent", [0x89, 0xB4, 0xFA]),
    ("surface.0", [0x31, 0x32, 0x44]),
    ("surface.1", [0x45, 0x47, 0x5A]),
    ("surface.2", [0x58, 0x5B, 0x70]),
    ("muted", [0x6C, 0x70, 0x86]),
];

/// Dark label color used on light pill backgrounds.
const LABEL_DARK: [u8; 3] = [0x1E, 0x1E, 0x2E];

/// Light label color used on dark pill backgrounds.
const LABEL_LIGHT: [u8; 3] = [0xCD, 0xD6, 0xF4];

/// Perceived-brightness threshold (0..=255) above which a pill background
/// takes the dark label color.
const LIGHT_BG_THRESHOLD: u32 = 128;

const fn rgb(c: [u8; 3]) -> Rgb {
    Rgb {
        r: c[0],
        g: c[1],
        b: c[2],
    }
}

/// Placeholder RGB for a bar theme token, or `None` when unknown.
#[must_use]
pub fn resolve_bar_token(token: &str) -> Option<Rgb> {
    PLACEHOLDER_TOKENS
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, c)| rgb(*c))
}

/// Label color that contrasts with `bg` (dark text on light fills).
#[must_use]
pub fn contrasting_label(bg: Rgb) -> Rgb {
    let brightness = (u32::from(bg.r) * 299 + u32::from(bg.g) * 587 + u32::from(bg.b) * 114) / 1000;
    if brightness >= LIGHT_BG_THRESHOLD {
        rgb(LABEL_DARK)
    } else {
        rgb(LABEL_LIGHT)
    }
}

/// Resolved workspace pill colors (CTX-0874).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarPalette {
    /// Active pill background.
    pub active_bg: Rgb,
    /// Active pill label.
    pub active_fg: Rgb,
    /// Inactive pill background.
    pub inactive_bg: Rgb,
    /// Inactive pill label.
    pub inactive_fg: Rgb,
}

impl BarPalette {
    /// Resolves the configured tokens (`workspace.bar.colors.*`); an unset
    /// or unknown token falls back to its default token.
    #[must_use]
    pub fn from_tokens(active: Option<&str>, inactive: Option<&str>) -> Self {
        let pick = |token: Option<&str>, default: &str| {
            token
                .and_then(resolve_bar_token)
                .or_else(|| resolve_bar_token(default))
                .unwrap_or(rgb(LABEL_LIGHT))
        };
        let active_bg = pick(active, DEFAULT_ACTIVE_TOKEN);
        let inactive_bg = pick(inactive, DEFAULT_INACTIVE_TOKEN);
        Self {
            active_bg,
            active_fg: contrasting_label(active_bg),
            inactive_bg,
            inactive_fg: contrasting_label(inactive_bg),
        }
    }
}

impl Default for BarPalette {
    fn default() -> Self {
        Self::from_tokens(None, None)
    }
}

/// Pill horizontal alignment (CTX-0874).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillAlign {
    /// Pills start at [`BAR_LEFT_MARGIN`].
    Left,
    /// Pills are centered in the band width.
    Center,
}

impl PillAlign {
    /// Parses from config string (`workspace.bar.pill_align`).
    #[must_use]
    pub fn from_config(s: Option<&str>) -> Self {
        match s {
            Some("center") => Self::Center,
            _ => Self::Left,
        }
    }
}

/// Lays out workspace pills in the band's left region (CTX-0874).
///
/// Pill `i` is `label_widths[i] + 2 * PILL_PAD` cells wide. Alignment:
/// - [`PillAlign::Left`]: first pill starts at [`BAR_LEFT_MARGIN`].
/// - [`PillAlign::Center`]: pills are centered as a group in `band_width`.
///
/// Each pill is [`PILL_GAP`] cells after the previous. The same layout serves
/// top and bottom bands (only the band origin differs). Pills that do not fit
/// whole in `band_width` are dropped together with every pill after them, so
/// painted pills and click targets always agree. Returns `(start, width)` in
/// band columns. The center and right regions are reserved for status-system
/// slots and stay empty.
#[must_use]
pub fn layout_pills(band_width: u16, label_widths: &[u16], align: PillAlign) -> Vec<(u16, u16)> {
    // First pass: measure total width of pills that fit.
    let mut total_width = 0u32;
    let mut count = 0usize;
    for &label in label_widths {
        let pill_width = u32::from(label) + 2 * u32::from(PILL_PAD);
        let gap = if count > 0 { u32::from(PILL_GAP) } else { 0 };
        let needed = total_width + gap + pill_width;
        let margin = if align == PillAlign::Left {
            u32::from(BAR_LEFT_MARGIN)
        } else {
            0
        };
        if margin + needed > u32::from(band_width) {
            break;
        }
        total_width = needed;
        count += 1;
    }

    if count == 0 {
        return Vec::new();
    }

    // Second pass: place pills at the computed start offset.
    let start_col = match align {
        PillAlign::Left => u32::from(BAR_LEFT_MARGIN),
        PillAlign::Center => {
            let available = u32::from(band_width);
            if total_width >= available {
                0
            } else {
                (available - total_width) / 2
            }
        }
    };

    let mut out = Vec::with_capacity(count);
    let mut col = start_col;
    for &label in &label_widths[..count] {
        let width = u32::from(label) + 2 * u32::from(PILL_PAD);
        out.push((saturate_u16(col), saturate_u16(width)));
        col += width + u32::from(PILL_GAP);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: UiRect = UiRect::new(0, 0, 80, 24);

    #[test]
    fn absent_bar_keeps_the_full_window() {
        let solved = solve(WINDOW, BarEdge::Bottom, 1, false, MIN_CONTENT_ROWS);
        assert_eq!(solved.container, WINDOW);
        assert_eq!(solved.bar, None);
        let zero = solve(WINDOW, BarEdge::Top, 0, true, MIN_CONTENT_ROWS);
        assert_eq!(zero.container, WINDOW);
        assert_eq!(zero.bar, None);
    }

    #[test]
    fn bottom_band_takes_the_last_row() {
        let solved = solve(WINDOW, BarEdge::Bottom, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container, UiRect::new(0, 0, 80, 23));
        assert_eq!(solved.bar, Some(UiRect::new(0, 23, 80, 1)));
    }

    #[test]
    fn top_band_takes_row_zero_and_shifts_content() {
        let solved = solve(WINDOW, BarEdge::Top, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container, UiRect::new(0, 1, 80, 23));
        assert_eq!(solved.bar, Some(UiRect::new(0, 0, 80, 1)));
    }

    #[test]
    fn tiny_window_hides_the_band_instead_of_zero_content() {
        for edge in [BarEdge::Top, BarEdge::Bottom] {
            let one_row = UiRect::new(0, 0, 80, 1);
            let solved = solve(one_row, edge, 1, true, MIN_CONTENT_ROWS);
            assert_eq!(solved.container, one_row, "{edge:?}");
            assert_eq!(solved.bar, None, "{edge:?}");
            let no_cols = UiRect::new(0, 0, 0, 24);
            assert_eq!(
                solve(no_cols, edge, 1, true, MIN_CONTENT_ROWS).bar,
                None,
                "{edge:?}"
            );
        }
        // Exactly band + MIN_CONTENT_ROWS still reserves.
        let two = UiRect::new(0, 0, 10, 1 + MIN_CONTENT_ROWS);
        let solved = solve(two, BarEdge::Bottom, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container.height, MIN_CONTENT_ROWS);
        assert!(solved.bar.is_some());
    }

    #[test]
    fn insets_saturate_on_hostile_extents() {
        let huge = ChromeInsets {
            top: u16::MAX,
            bottom: u16::MAX,
            ..ChromeInsets::NONE
        };
        assert_eq!(huge.content_of(WINDOW, MIN_CONTENT_ROWS), None);
        let edge = UiRect::new(u16::MAX, u16::MAX, 4, 4);
        let solved = solve(edge, BarEdge::Top, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container.y, u16::MAX, "origin saturates");
        assert_eq!(solved.container.height, 3);
    }

    #[test]
    fn min_container_rows_counts_gaps_and_decoration() {
        assert_eq!(min_container_rows(0, 0.0, 22), MIN_CONTENT_ROWS);
        // 2 * (6 + 2 + 6) px = 28 px at a 22 px cell rounds up to 2 rows.
        assert_eq!(min_container_rows(0, 28.0, 22), MIN_CONTENT_ROWS + 2);
        assert_eq!(min_container_rows(1, 0.0, 22), MIN_CONTENT_ROWS + 2);
        assert_eq!(min_container_rows(0, 28.0, 0), MIN_CONTENT_ROWS);
        assert_eq!(min_container_rows(0, f64::NAN, 22), MIN_CONTENT_ROWS);
        assert_eq!(min_container_rows(u16::MAX, 1e30, 1), u16::MAX);
    }

    #[test]
    fn effective_floor_hides_the_band_when_decoration_eats_content() {
        let floor = min_container_rows(0, 28.0, 22);
        let short = UiRect::new(0, 0, 80, floor);
        assert_eq!(solve(short, BarEdge::Bottom, 1, true, floor).bar, None);
        let fits = UiRect::new(0, 0, 80, floor + 1);
        let solved = solve(fits, BarEdge::Bottom, 1, true, floor);
        assert_eq!(solved.container.height, floor);
        assert!(solved.bar.is_some());
    }

    #[test]
    fn pills_left_align_with_margin_padding_and_gaps() {
        // Labels "ws1" and "ws2 (2)": widths 3 and 7.
        let pills = layout_pills(80, &[3, 7], PillAlign::Left);
        assert_eq!(pills, vec![(1, 5), (7, 9)]);
    }

    #[test]
    fn pills_center_align_as_a_group() {
        // Two pills: 5 + 1 gap + 9 = 15 total width.
        // Band width 80: (80 - 15) / 2 = 32.5 -> 32 start.
        let pills = layout_pills(80, &[3, 7], PillAlign::Center);
        assert_eq!(pills, vec![(32, 5), (38, 9)]);
        // Single pill: width 5, centered in 20 = (20-5)/2 = 7.
        assert_eq!(layout_pills(20, &[3], PillAlign::Center), vec![(7, 5)]);
        // Tight fit: no room to center, starts at 0.
        assert_eq!(layout_pills(5, &[3], PillAlign::Center), vec![(0, 5)]);
    }

    #[test]
    fn pills_that_do_not_fit_are_dropped_whole() {
        // Margin 1 + pill 5 = 6 fits exactly; the second pill would end at 12.
        assert_eq!(layout_pills(6, &[3, 3], PillAlign::Left), vec![(1, 5)]);
        assert_eq!(layout_pills(5, &[3], PillAlign::Left), Vec::new());
        assert_eq!(layout_pills(0, &[0], PillAlign::Left), Vec::new());
        // A huge label saturates instead of wrapping.
        assert_eq!(
            layout_pills(u16::MAX, &[u16::MAX], PillAlign::Left),
            Vec::new()
        );
        // Center alignment still drops pills that don't fit.
        assert_eq!(layout_pills(5, &[3], PillAlign::Center), vec![(0, 5)]);
        assert_eq!(layout_pills(4, &[3], PillAlign::Center), Vec::new());
    }

    #[test]
    fn palette_defaults_and_token_resolution() {
        let palette = BarPalette::default();
        assert_eq!(
            Some(palette.active_bg),
            resolve_bar_token(DEFAULT_ACTIVE_TOKEN)
        );
        assert_eq!(
            Some(palette.inactive_bg),
            resolve_bar_token(DEFAULT_INACTIVE_TOKEN)
        );
        assert_ne!(palette.active_bg, palette.inactive_bg, "active is distinct");
        // Light accent gets dark text, dark surface gets light text.
        assert_eq!(palette.active_fg, rgb(LABEL_DARK));
        assert_eq!(palette.inactive_fg, rgb(LABEL_LIGHT));
        let custom = BarPalette::from_tokens(Some("muted"), Some("surface.2"));
        assert_eq!(Some(custom.active_bg), resolve_bar_token("muted"));
        assert_eq!(Some(custom.inactive_bg), resolve_bar_token("surface.2"));
        // Unknown tokens fall back to the defaults.
        assert_eq!(BarPalette::from_tokens(Some("nope"), Some("x.9")), palette);
    }
}
