//! Cell content and resolved text styles for the Terminal Truth grid.
//!
//! Every visible position holds exactly one [`Cell`] with a defined glyph,
//! style, width, and hyperlink reference (RFC "Grid and state invariants",
//! invariant 2 "Cell totality"). Wide characters occupy two cells: the
//! leading cell carries the glyph and `width == 2`; the trailing half is a
//! [`Cell::wide_spacer`]. No orphan spacers may exist.
//!
//! Cell-width resolution uses the generated UAX #11 tables in
//! [`crate::width_tables`] (Unicode 18.0.0 `EastAsianWidth.txt`, W/F wide,
//! A resolved by the configured [`AmbiguousWidth`] policy, N/Na/H narrow),
//! the crate zero-width tables, and the UAX #29 cluster rules in
//! [`crate::grapheme`]. The remainder of an extended grapheme cluster
//! (combining marks, ZWJ, regional-indicator pairs, emoji modifiers,
//! spacing marks) is stored on the leading cell's bounded
//! [`Cell::zerowidth`] buffer, so one cluster is one layout unit. Shaping,
//! fallback fonts, and cluster paint for the unshaped path follow the text
//! RFC named by ADR-0004 and are still open.

use bitty_vt::{Attribute, Color, UnderlineStyle};

use crate::width_tables::{EawClass, eaw_class};

/// Maximum cluster-continuation scalars retained on one cell.
///
/// The cap bounds untrusted-input memory per threat T-01: the scalars after
/// the base glyph of an extended grapheme cluster are stored inline and the
/// excess degrades deterministically (see [`Cell::push_zerowidth`]). Six
/// covers decomposed accents, keycaps, variant selectors, skin tones, and the
/// canonical four-person ZWJ family without a heap allocation while keeping
/// [`Cell`] inside the 64-byte scrollback budget.
pub const MAX_CLUSTER_SCALARS: usize = 6;

/// Historical name for [`MAX_CLUSTER_SCALARS`] kept on the public surface.
///
/// The buffer originally held only zero-width combining marks
/// (Alacritty zerowidth parity); it now holds every scalar of the cluster
/// tail, including regional indicators and emoji modifiers. Both names
/// describe the same bound.
pub const MAX_ZEROWIDTH_CHARS: usize = MAX_CLUSTER_SCALARS;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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

/// Bounded inline cluster-tail buffer replacing the heap `Vec<char>`.
///
/// Stores up to [`MAX_CLUSTER_SCALARS`] scalars inline as `[char; 6] + len`
/// (CTX-0389, issue #645): no heap allocation per cell, so cloning a
/// scrollback snapshot is a `memcpy` instead of per-cell `malloc` + cache
/// thrash. `Copy` so [`Cell`] can be `Copy` and snapshots clone fast.
///
/// The historical name reflects the original zero-width-only payload; the
/// buffer now holds every scalar of the extended grapheme cluster that
/// follows [`Cell::glyph`] — combining marks, ZWJ, variation selectors,
/// regional indicators, emoji modifiers, and spacing marks (CTX-1609).
///
/// Fail-closed on overflow: [`Zerowidth::push`] returns `false` without
/// mutating when full, never panics. Unused tail slots stay `'\0'` and are
/// excluded from equality and hashing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Zerowidth {
    chars: [char; MAX_CLUSTER_SCALARS],
    len: u8,
}

impl Zerowidth {
    /// Empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            chars: ['\0'; MAX_CLUSTER_SCALARS],
            len: 0,
        }
    }

    /// Number of retained marks.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether no marks are retained.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Retained marks in arrival order.
    #[must_use]
    pub fn as_slice(&self) -> &[char] {
        &self.chars[..self.len as usize]
    }

    /// Iterate retained marks in arrival order.
    pub fn iter(&self) -> std::slice::Iter<'_, char> {
        self.as_slice().iter()
    }

    /// Whether `mark` is retained (mirrors `Vec::contains`).
    #[must_use]
    pub fn contains(&self, mark: &char) -> bool {
        self.as_slice().contains(mark)
    }

    /// Append one cluster-tail scalar, fail-closed when full.
    ///
    /// Returns `false` without mutating when already holding
    /// [`MAX_CLUSTER_SCALARS`] scalars; the caller decides the documented
    /// degradation (a zero-width mark is dropped, a printable continuation
    /// starts a fresh cell).
    pub fn push(&mut self, mark: char) -> bool {
        if self.len as usize >= MAX_CLUSTER_SCALARS {
            return false;
        }
        self.chars[self.len as usize] = mark;
        self.len += 1;
        true
    }
}

impl PartialEq for Zerowidth {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Zerowidth {}

impl std::hash::Hash for Zerowidth {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<'a> IntoIterator for &'a Zerowidth {
    type Item = &'a char;
    type IntoIter = std::slice::Iter<'a, char>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// One grid cell: total by construction (RFC invariant 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Leading Unicode scalar displayed in this cell; `' '` when erased.
    ///
    /// Zero-width scalars that follow this scalar (combining marks, ZWJ,
    /// variation selectors) accumulate in [`Cell::zerowidth`] instead of
    /// occupying their own cell.
    pub glyph: char,
    /// Resolved style of this cell.
    pub style: Style,
    /// Display width: exactly `1` or `2`.
    pub width: u8,
    /// `true` only for the trailing half of a wide character whose leading
    /// half lives at the previous column of the same row.
    pub spacer: bool,
    /// `true` when this cell is layout/erase padding rather than printed
    /// content: blank cells synthesized by erase, grid growth, or rewrap
    /// fill. Reflow drops trailing padding instead of materializing it as
    /// spaces, while a space printed by the application (`padding == false`)
    /// stays content (#2529).
    pub padding: bool,
    /// Hyperlink span reference, if any.
    pub hyperlink: Option<HyperlinkId>,
    /// Continuation scalars of this cell's extended grapheme cluster, in
    /// arrival order: combining marks, ZWJ, regional indicators, emoji
    /// modifiers, spacing marks, variation selectors, and keycap marks.
    ///
    /// Bounded by [`MAX_CLUSTER_SCALARS`] (threat T-01); spacers never
    /// carry continuation scalars. Empty for erased and spacer cells.
    pub zerowidth: Zerowidth,
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
            padding: true,
            hyperlink: None,
            zerowidth: Zerowidth::new(),
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
            padding: true,
            hyperlink: None,
            zerowidth: Zerowidth::new(),
        }
    }

    /// Whether this cell displays no content (an erased blank).
    #[must_use]
    pub fn is_blank(&self) -> bool {
        !self.spacer && self.glyph == ' ' && self.zerowidth.is_empty()
    }

    /// Appends this cell's textual content to `out`: the leading
    /// [`Cell::glyph`] followed by every retained cluster-tail scalar, in
    /// arrival order (CTX-1367, CTX-1609).
    ///
    /// Text extraction (selection copy, accessibility runs, inspection,
    /// URL detection) uses this so combining marks, ZWJ, variation
    /// selectors, regional indicators, and emoji modifiers survive. Paint
    /// does not call it. Callers iterate a row and skip [`Cell::spacer`]
    /// trailing halves before invoking this.
    pub fn push_text_into(&self, out: &mut String) {
        out.push(self.glyph);
        for mark in &self.zerowidth {
            out.push(*mark);
        }
    }

    /// Appends a cluster-continuation scalar to this cell's inline buffer.
    ///
    /// Returns `false` without mutating when the buffer already holds
    /// [`MAX_CLUSTER_SCALARS`] scalars, so untrusted cluster runs cannot
    /// grow memory (threat T-01). The caller drops a zero-width excess or
    /// places a printable excess as its own layout unit. Spacers reject
    /// every scalar (`false`) because the cluster belongs to the leading
    /// half.
    pub fn push_zerowidth(&mut self, mark: char) -> bool {
        if self.spacer {
            return false;
        }
        self.zerowidth.push(mark)
    }
}

/// Ambiguous-width policy for East Asian Ambiguous (`A`) scalars (UAX #11).
///
/// Ambiguous scalars (Greek and Cyrillic letters, CJK-adjacent
/// punctuation, arrows, math symbols, box drawing) render wide
/// in East Asian legacy contexts and narrow elsewhere, so the grid must pick
/// one policy per deployment. [`AmbiguousWidth::Narrow`] (default; matches
/// `unicode-width::width`) resolves every ambiguous scalar to one cell;
/// [`AmbiguousWidth::Wide`] resolves them to two cells (matches
/// `unicode-width::width_cjk`). The policy is configuration
/// (`terminal.ambiguous_width`) carried by [`crate::State`]; call sites that
/// measure text outside a state use [`char_cell_width`] (narrow) or
/// [`char_cell_width_in`] explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AmbiguousWidth {
    /// Ambiguous scalars occupy one cell (default).
    #[default]
    Narrow,
    /// Ambiguous scalars occupy two cells (East Asian legacy context).
    Wide,
}

impl AmbiguousWidth {
    /// Parses a config spelling (exact lowercase; fail-closed).
    ///
    /// Returns `None` for anything but `"narrow"` and `"wide"`; the
    /// integration layer maps the configuration enum by value.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "narrow" => Some(Self::Narrow),
            "wide" => Some(Self::Wide),
            _ => None,
        }
    }

    /// Canonical config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Narrow => "narrow",
            Self::Wide => "wide",
        }
    }
}

impl std::fmt::Display for AmbiguousWidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Display-width resolution for one Unicode scalar (UAX #11, UCD 18.0.0).
///
/// Returns `0` for zero-width scalars (combining marks, ZWJ, variation
/// selectors, bidi controls, tags), `2` for East Asian Wide/Fullwidth
/// scalars, else `1`. Ambiguous (`A`) scalars resolve narrow. C0/C1 controls
/// resolve to `1`: control interpretation belongs to the action stream
/// (`PrintControl`), never to width, and changing that here would silently
/// reflow every caller.
#[must_use]
pub fn char_cell_width(c: char) -> u8 {
    char_cell_width_in(c, AmbiguousWidth::Narrow)
}

/// [`char_cell_width`] with an explicit [`AmbiguousWidth`] policy.
///
/// Only East Asian Ambiguous scalars observe the policy; every other scalar
/// resolves identically under both policies. The authoritative
/// `East_Asian_Width` table lives in [`crate::width_tables`] (generated from
/// the pinned UCD snapshot); the crate zero-width table takes precedence so
/// combining marks in wide blocks still occupy no cell.
#[must_use]
pub fn char_cell_width_in(c: char, ambiguous: AmbiguousWidth) -> u8 {
    let cp = c as u32;
    if cp < 0x00A1 {
        // Below the first ambiguous scalar (U+00A1) nothing is zero-width,
        // wide, or ambiguous; controls intentionally resolve to 1 (see
        // [`char_cell_width`]).
        return 1;
    }
    if is_zero_width(cp) {
        return 0;
    }
    match eaw_class(cp) {
        EawClass::W | EawClass::F => 2,
        EawClass::A => match ambiguous {
            AmbiguousWidth::Narrow => 1,
            AmbiguousWidth::Wide => 2,
        },
        EawClass::N | EawClass::Na | EawClass::H => 1,
    }
}

/// Whether `cp` is a zero-width combining or format scalar in the crate
/// tables.
///
/// The tables cover the combining-mark blocks, implicit marks, variation
/// selectors, and the format controls the terminal must not advance over
/// (CTX-1609 leaves this curated slice unchanged; the authoritative
/// `Grapheme_Extend` set is a separate follow-up). Width resolution orders
/// this check before the East_Asian_Width table so a mark stays zero-width
/// even when its block is Wide or Ambiguous.
pub(crate) fn is_zero_width(cp: u32) -> bool {
    matches!(cp,
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x05BF
        | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5
        | 0x05C7
        | 0x0600..=0x0605
        | 0x0610..=0x061A
        | 0x061C
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DD
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED
        | 0x070F
        | 0x0711
        | 0x0730..=0x074A
        | 0x07A6..=0x07B0
        | 0x0816..=0x0819
        | 0x081B..=0x0823
        | 0x0825..=0x0827
        | 0x0829..=0x082D
        | 0x0890..=0x0891
        | 0x0900..=0x0902
        | 0x093A
        | 0x093C
        | 0x0941..=0x0948
        | 0x094D
        | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x09CD
        | 0x0ACD
        | 0x0AFB
        | 0x0B4D
        | 0x0C4D
        | 0x0D4D
        | 0x0E31
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E
        | 0x0F72..=0x0F84
        | 0x1036..=0x1037
        | 0x1039..=0x103A
        | 0x135D..=0x135F
        | 0x17B7..=0x17BD
        | 0x17D0
        | 0x17D2
        | 0x180B..=0x180D
        | 0x1A60
        | 0x1AB0..=0x1AFF
        | 0x1B00..=0x1B03
        | 0x1B34..=0x1B38
        | 0x1B44
        | 0x1BAB
        | 0x1D165..=0x1D169
        | 0x1DC0..=0x1DFF
        | 0x200B..=0x200F
        | 0x202A..=0x202E
        | 0x2060..=0x2064
        | 0x20D0..=0x20FF
        | 0x302A..=0x302D
        | 0x3099..=0x309A
        | 0xA66F
        | 0xA674..=0xA67D
        | 0xA8E0..=0xA8F1
        | 0xA9C0
        | 0xAAF6
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0xFEFF
        | 0x10A3F
        | 0x11133
        | 0x113D0
        | 0x1193E
        | 0x11A47
        | 0x11A99
        | 0x11F42
        | 0xE0000..=0xE007F
        | 0xE0100..=0xE01EF
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
    fn emoji_presentation_set_is_two_cells() {
        // UAX #11 emoji-presentation ranges terminals render wide.
        assert_eq!(char_cell_width('\u{231A}'), 2);
        assert_eq!(char_cell_width('\u{23E9}'), 2);
        assert_eq!(char_cell_width('\u{25FD}'), 2);
        assert_eq!(char_cell_width('\u{2614}'), 2);
        assert_eq!(char_cell_width('\u{26AA}'), 2);
        assert_eq!(char_cell_width('\u{2705}'), 2);
        assert_eq!(char_cell_width('\u{2B50}'), 2);
        assert_eq!(char_cell_width('\u{1B000}'), 2);
    }

    #[test]
    fn extended_zero_width_ranges_attach() {
        // Combining and format ranges added by the M1 tables.
        assert_eq!(char_cell_width('\u{20D0}'), 0);
        assert_eq!(char_cell_width('\u{FE20}'), 0);
        assert_eq!(char_cell_width('\u{1AB0}'), 0);
        assert_eq!(char_cell_width('\u{1DC0}'), 0);
        assert_eq!(char_cell_width('\u{302A}'), 0);
        assert_eq!(char_cell_width('\u{3099}'), 0);
        assert_eq!(char_cell_width('\u{0F72}'), 0);
        assert_eq!(char_cell_width('\u{0600}'), 0);
        assert_eq!(char_cell_width('\u{070F}'), 0);
    }

    #[test]
    fn ambiguous_defaults_narrow_with_explicit_wide_policy() {
        for scalar in [
            '\u{00A1}', '\u{0391}', '\u{0410}', '\u{2014}', '\u{20AC}', '\u{2192}', '\u{2200}',
            '\u{2260}', '\u{2500}',
        ] {
            assert_eq!(char_cell_width(scalar), 1, "{scalar:?} defaults narrow");
            assert_eq!(
                char_cell_width_in(scalar, AmbiguousWidth::Narrow),
                1,
                "{scalar:?} explicit narrow"
            );
            assert_eq!(
                char_cell_width_in(scalar, AmbiguousWidth::Wide),
                2,
                "{scalar:?} explicit wide"
            );
        }
    }

    #[test]
    fn policy_only_moves_ambiguous_scalars() {
        // Non-ambiguous scalars resolve identically under both policies
        // (U+00DF sharp-s and U+2500 box drawing are Ambiguous per UAX #11,
        // so they are absent here; the authoritative table moved box
        // drawing from the old curated narrow gap into the policy).
        for scalar in [
            'a',
            '\u{4E2D}',
            '\u{1F600}',
            '\u{0301}',
            '\u{00E7}',
            '\u{FF61}',
        ] {
            assert_eq!(
                char_cell_width_in(scalar, AmbiguousWidth::Narrow),
                char_cell_width_in(scalar, AmbiguousWidth::Wide),
                "{scalar:?} must be policy-independent"
            );
        }
        assert_eq!(AmbiguousWidth::default(), AmbiguousWidth::Narrow);
    }

    #[test]
    fn erased_cell_is_total_and_blank() {
        let cell = Cell::erased(Style::default());
        assert!(cell.is_blank());
        assert_eq!(cell.width, 1);
        assert!(!cell.spacer);
        assert_eq!(cell.glyph, ' ');
        assert!(cell.zerowidth.is_empty());
    }

    #[test]
    fn spacer_cell_marks_trailing_half() {
        let cell = Cell::wide_spacer(Style::default());
        assert_eq!(cell.width, 2);
        assert!(cell.spacer);
        assert!(cell.zerowidth.is_empty());
    }

    #[test]
    fn zerowidth_buffer_accumulates_in_order_up_to_cap() {
        let mut cell = Cell::erased(Style::default());
        cell.glyph = 'e';
        assert!(cell.push_zerowidth('\u{0301}'));
        assert!(cell.push_zerowidth('\u{200D}'));
        assert_eq!(cell.zerowidth.as_slice(), &['\u{0301}', '\u{200D}']);
        for _ in 0..MAX_ZEROWIDTH_CHARS {
            cell.push_zerowidth('\u{0300}');
        }
        assert_eq!(cell.zerowidth.len(), MAX_ZEROWIDTH_CHARS);
        // Full buffer drops without growth, never panics (fail-closed).
        assert!(!cell.push_zerowidth('\u{0302}'));
        assert_eq!(cell.zerowidth.len(), MAX_ZEROWIDTH_CHARS);
    }

    #[test]
    fn zerowidth_marks_cell_non_blank_and_spacer_rejects() {
        let mut cell = Cell::erased(Style::default());
        assert!(cell.is_blank());
        assert!(cell.push_zerowidth('\u{0301}'));
        assert!(!cell.is_blank());
        let mut spacer = Cell::wide_spacer(Style::default());
        assert!(!spacer.push_zerowidth('\u{0301}'));
        assert!(spacer.zerowidth.is_empty());
    }

    #[test]
    fn push_text_into_emits_glyph_then_marks_in_order() {
        let mut cell = Cell::erased(Style::default());
        cell.glyph = 'e';
        assert!(cell.push_zerowidth('\u{0301}'));
        assert!(cell.push_zerowidth('\u{200D}'));
        let mut out = String::from("prefix:");
        cell.push_text_into(&mut out);
        assert_eq!(out, "prefix:e\u{0301}\u{200D}");
        // A spacer never carries marks, so it contributes only its blank.
        let spacer = Cell::wide_spacer(Style::default());
        let mut blank = String::new();
        spacer.push_text_into(&mut blank);
        assert_eq!(blank, " ");
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

    #[test]
    fn zerowidth_is_inline_bounded_and_copy() {
        // CTX-0389 (#645): no heap Vec per cell; inline [char; 6] + len.
        // 6 chars (24 bytes) + len byte padded to char alignment = 28 bytes,
        // same stack size class as a Vec header but with no heap allocation
        // and Copy memcpy clone for snapshots (CTX-1609 widened the payload
        // from 5 to 6 cluster-tail scalars).
        assert_eq!(std::mem::size_of::<Zerowidth>(), 28);
        assert!(std::mem::size_of::<Zerowidth>() <= 28);
        // Cell stays sane for a 10k x 100 scrollback (1M cells); the win is
        // no per-cell heap, not a smaller header.
        assert!(std::mem::size_of::<Cell>() <= 96);
        // Copy semantics: copies are independent values, not shared heap.
        let mut first = Zerowidth::new();
        assert!(first.push('\u{0301}'));
        let second = first;
        assert_eq!(first.as_slice(), second.as_slice());
        assert!(second.contains(&'\u{0301}'));
        assert_eq!(second.len(), 1);
        // Equality and hashing ignore unused tail slots.
        let mut third = Zerowidth::new();
        assert!(third.push('\u{0301}'));
        assert_eq!(second, third);
        assert!(second.iter().eq(third.iter()));
    }

    #[test]
    fn zerowidth_overflow_truncates_fail_closed_never_panics() {
        // Fill exactly to cap, then overflow must return false without
        // mutating and without panicking.
        let mut buffer = Zerowidth::new();
        for index in 0..MAX_ZEROWIDTH_CHARS {
            assert!(buffer.push(char::from_u32(0x0300 + index as u32).unwrap_or('\u{0300}')));
        }
        assert_eq!(buffer.len(), MAX_ZEROWIDTH_CHARS);
        let before = buffer;
        assert!(!buffer.push('\u{0302}'));
        assert_eq!(buffer.len(), MAX_ZEROWIDTH_CHARS);
        assert_eq!(buffer, before);
        // Cell-level overflow also fails closed (spacer already covered).
        let mut cell = Cell::erased(Style::default());
        for _ in 0..MAX_ZEROWIDTH_CHARS {
            cell.push_zerowidth('\u{0300}');
        }
        assert!(!cell.push_zerowidth('\u{0302}'));
        assert_eq!(cell.zerowidth.len(), MAX_ZEROWIDTH_CHARS);
    }

    #[test]
    fn snapshot_clone_is_bounded_no_heap_thrash() {
        // Targeted timing assertion for the hot path (no criterion bench for
        // Cell alone; the `bitty-perf` suite's benches/terminal_state.rs
        // covers State::apply).
        // Cloning 100k cells (10k-line x 100-col scrollback is 1M; 100k keeps
        // CI fast) must stay well under a generous bound; a Vec-per-cell
        // clone with heap churn would blow this on debug builds.
        let cell = Cell::erased(Style::default());
        let row = vec![cell; 1000];
        let start = std::time::Instant::now();
        let mut total = 0usize;
        for _ in 0..100 {
            let cloned = row.clone();
            total += cloned.len();
        }
        let elapsed = start.elapsed();
        assert_eq!(total, 100_000);
        assert!(
            elapsed.as_millis() < 500,
            "100x clone of 1k cells took {elapsed:?}, expected < 500ms"
        );
    }
}
