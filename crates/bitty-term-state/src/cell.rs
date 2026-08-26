//! Cell content and resolved text styles for the Terminal Truth grid.
//!
//! Every visible position holds exactly one [`Cell`] with a defined glyph,
//! style, width, and hyperlink reference (RFC "Grid and state invariants",
//! invariant 2 "Cell totality"). Wide characters occupy two cells: the
//! leading cell carries the glyph and `width == 2`; the trailing half is a
//! [`Cell::wide_spacer`]. No orphan spacers may exist.
//!
//! Cell-width resolution uses a compact East Asian Width approximation in
//! [`char_cell_width`]. The full text domain (grapheme clusters, combining
//! marks, fallback, shaping) follows the text RFC named by ADR-0004 and is
//! still open; until it lands, zero-width scalars are dropped by terminal
//! state (see `state::State` print handling).

use bitty_vt::{Attribute, Color, UnderlineStyle};

/// A unique reference from cells into the state's hyperlink table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(u32);

impl HyperlinkId {
    /// Numeric identifier; stable for the lifetime of the owning state.
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }

    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }
}

/// Resolved text emphasis attributes carried by a cell or the cursor pen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Attributes {
    /// Bold intensity (`SGR 1`/`22`).
    pub bold: bool,
    /// Faint intensity (`SGR 2`/`22`).
    pub faint: bool,
    /// Italic (`SGR 3`/`23`).
    pub italic: bool,
    /// Underline shape (`SGR 4`, `21`, `24`, `4:x`).
    pub underline: UnderlineStyle,
    /// Blink (`SGR 5`/`25`).
    pub blink: bool,
    /// Inverse video (`SGR 7`/`27`).
    pub inverse: bool,
    /// Concealed text (`SGR 8`/`28`).
    pub invisible: bool,
    /// Strikethrough (`SGR 9`/`29`).
    pub strikethrough: bool,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            bold: false,
            faint: false,
            italic: false,
            underline: UnderlineStyle::None,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
        }
    }
}

impl Attributes {
    /// Applies one ordered SGR change to this attribute set.
    pub(crate) fn apply_change(&mut self, change: &AttributeChangeKind) {
        match *change {
            AttributeChangeKind::Reset => {
                *self = Self::default();
            }
            AttributeChangeKind::Set(attribute, enabled) => {
                match attribute {
                    Attribute::Bold => self.bold = enabled,
                    Attribute::Faint => self.faint = enabled,
                    Attribute::Italic => self.italic = enabled,
                    Attribute::Underline(style) => {
                        // Disabling any underline collapses to no underline;
                        // enabling replaces the shape wholesale.
                        if enabled {
                            self.underline = style;
                        } else {
                            self.underline = UnderlineStyle::None;
                        }
                    }
                    Attribute::Blink => self.blink = enabled,
                    Attribute::Inverse => self.inverse = enabled,
                    Attribute::Invisible => self.invisible = enabled,
                    Attribute::Strikethrough => self.strikethrough = enabled,
                }
            }
        }
    }
}

/// Internal normalized form of an SGR attribute operation.
///
/// Terminal state collapses the parser's ordered
/// [`bitty_vt::AttributeChange`] list into these kinds while replaying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttributeChangeKind {
    Reset,
    Set(Attribute, bool),
}

/// Foreground, background, underline color, and emphasis of one cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Style {
    /// Foreground color; `None` is the default palette entry.
    pub foreground: Option<Color>,
    /// Background color; `None` is the default palette entry.
    pub background: Option<Color>,
    /// Decorative underline color (`SGR 58`/`59`).
    pub underline_color: Option<Color>,
    /// Emphasis attributes.
    pub attributes: Attributes,
}

/// One grid cell: total by construction (RFC invariant 2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Leading Unicode scalar displayed in this cell; `' '` when erased.
    ///
    /// Grapheme-cluster composition beyond a single scalar awaits the text
    /// RFC (ADR-0004 open item); see module docs.
    pub glyph: char,
    /// Resolved style of this cell.
    pub style: Style,
    /// Display width: exactly `1` or `2`.
    pub width: u8,
    /// `true` only for the trailing half of a wide character whose leading
    /// half lives at the previous column of the same row.
    pub spacer: bool,
    /// Hyperlink span reference, if any.
    pub hyperlink: Option<HyperlinkId>,
}

impl Cell {
    /// An erased cell carrying the given style (background-color erase).
    #[must_use]
    pub fn erased(style: Style) -> Self {
        Self {
            glyph: ' ',
            style,
            width: 1,
            spacer: false,
            hyperlink: None,
        }
    }

    /// The trailing half of a wide character rooted at the previous column.
    #[must_use]
    pub fn wide_spacer(style: Style) -> Self {
        Self {
            glyph: ' ',
            style,
            width: 2,
            spacer: true,
            hyperlink: None,
        }
    }

    /// Whether this cell displays no content (an erased blank).
    #[must_use]
    pub fn is_blank(&self) -> bool {
        !self.spacer && self.glyph == ' '
    }
}

/// Compact display-width resolution for one Unicode scalar.
///
/// Returns `0` for zero-width scalars (combining marks, variation selectors,
/// zero-width direction marks), `2` for East Asian Wide/Fullwidth ranges and
/// wide emoji blocks, else `1`. This is a deliberate compact approximation:
/// the authoritative width tables follow the text RFC named by ADR-0004
/// (open item under OQ-007), after which this function becomes the single
/// implementation behind that decision.
#[must_use]
pub fn char_cell_width(c: char) -> u8 {
    let cp = c as u32;
    if cp < 0x0300 {
        return 1;
    }
    if is_zero_width(cp) {
        return 0;
    }
    if is_wide(cp) { 2 } else { 1 }
}

fn is_zero_width(cp: u32) -> bool {
    matches!(cp,
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x05BF
        | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5
        | 0x05C7
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED
        | 0x0711
        | 0x0730..=0x074A
        | 0x07A6..=0x07B0
        | 0x0816..=0x0819
        | 0x081B..=0x0823
        | 0x0825..=0x0827
        | 0x0829..=0x082D
        | 0x0900..=0x0902
        | 0x093A
        | 0x093C
        | 0x0941..=0x0948
        | 0x094D
        | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x0E31
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E
        | 0x200B..=0x200F
        | 0x202A..=0x202E
        | 0x2060..=0x2064
        | 0xFE00..=0xFE0F
        | 0xFEFF
        | 0xE0100..=0xE01EF
    )
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_ascii_is_one_cell() {
        assert_eq!(char_cell_width('a'), 1);
        assert_eq!(char_cell_width('~'), 1);
        assert_eq!(char_cell_width(' '), 1);
    }

    #[test]
    fn cjk_and_fullwidth_are_two_cells() {
        assert_eq!(char_cell_width('\u{4E2D}'), 2);
        assert_eq!(char_cell_width('\u{AC00}'), 2);
        assert_eq!(char_cell_width('\u{FF21}'), 2);
        assert_eq!(char_cell_width('\u{1F600}'), 2);
    }

    #[test]
    fn combining_marks_are_zero_cells() {
        assert_eq!(char_cell_width('\u{0301}'), 0);
        assert_eq!(char_cell_width('\u{200D}'), 0);
        assert_eq!(char_cell_width('\u{FE0F}'), 0);
    }

    #[test]
    fn erased_cell_is_total_and_blank() {
        let cell = Cell::erased(Style::default());
        assert!(cell.is_blank());
        assert_eq!(cell.width, 1);
        assert!(!cell.spacer);
        assert_eq!(cell.glyph, ' ');
    }

    #[test]
    fn spacer_cell_marks_trailing_half() {
        let cell = Cell::wide_spacer(Style::default());
        assert_eq!(cell.width, 2);
        assert!(cell.spacer);
    }

    #[test]
    fn attributes_reset_restores_defaults() {
        let mut attrs = Attributes {
            bold: true,
            italic: true,
            ..Attributes::default()
        };
        attrs.apply_change(&AttributeChangeKind::Reset);
        assert_eq!(attrs, Attributes::default());
    }

    #[test]
    fn underline_disable_collapses_to_none() {
        let mut attrs = Attributes::default();
        attrs.apply_change(&AttributeChangeKind::Set(
            Attribute::Underline(UnderlineStyle::Curly),
            true,
        ));
        assert_eq!(attrs.underline, UnderlineStyle::Curly);
        attrs.apply_change(&AttributeChangeKind::Set(
            Attribute::Underline(UnderlineStyle::Single),
            false,
        ));
        assert_eq!(attrs.underline, UnderlineStyle::None);
    }
}
