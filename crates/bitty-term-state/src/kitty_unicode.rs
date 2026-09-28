//! Kitty Unicode placeholder decoding (`U+10EEEE`, issue #1400).
//!
//! The Kitty graphics protocol reserves one private-use scalar as a
//! text-grid placeholder for image content. A client first transmits an
//! image and creates a virtual placement (`U=1` with `c=`/`r=` cell spans),
//! then prints runs of `U+10EEEE` cells whose combining diacritics name the
//! tile row/column inside that span, whose foreground color names the image
//! id, and whose underline color optionally names the placement id.
//!
//! This module is the headless, bounded decode half: given one placeholder
//! cell's style colors plus its retained combining marks, it resolves the
//! `(image id, placement id, tile row, tile column)` tuple the cell names.
//! It owns no grid, no scrollback, and no store: callers ([`crate::State`]
//! for grid linkage, `bitty-rich` for placement sizing) supply the context.
//!
//! # Wire rules (kitty graphics protocol, Unicode placeholders)
//!
//! - Base scalar: exactly `U+10EEEE`. The `U+10EEEE..=U+10EEFF` range naming
//!   in the issue title is the reservation window; only `U+10EEEE` is the
//!   defined placeholder cell. Neighboring scalars print as ordinary text.
//! - Diacritics: the first mark names the tile row, the second the tile
//!   column, the optional third the most significant image-id byte. Each is
//!   looked up in the frozen kitty `rowcolumn-diacritics` table (a sorted
//!   256-entry list; the index is the value). A mark outside the table
//!   (or a missing mark) means "unspecified" and is inherited leftwards
//!   (see below), never an error.
//! - Image id: 24 low bits from the cell foreground color (indexed palette
//!   entry value, or RGB packed `r << 16 | g << 8 | b`), plus the optional
//!   third-diacritic byte shifted up 24. No foreground color means id `0`.
//! - Placement id: cell underline color by the same 24-bit packing; absent
//!   or `0` means "unspecified" (any virtual placement of the image may
//!   serve — kitty fallback, needed because TUI hosts often cannot emit
//!   SGR 58 underline colors).
//! - Leftward inheritance (applied left-to-right across a run): a cell with
//!   no marks copies row/column/high-byte from the previous placeholder
//!   cell when colors match; a cell with only a row mark copies the column
//!   and high byte; a cell with row+column marks copies only the high byte.
//!   In this headless API the caller performs the chaining via
//!   [`KittyUnicodeRun::push`]; per-cell decode without context is
//!   [`decode_cell`].
//!
//! # Bounds (threat T-01/T-02)
//!
//! Everything here is total over untrusted input: table lookup is a binary
//! search over the frozen 297-entry list (no allocation), row/column values
//! are `u8` (indexes past `u8::MAX` decode to `None`/unspecified),
//! image/placement ids are `u32`, and runs are caller-bounded (grid rows
//! are at most [`crate::state::MAX_GRID_DIM`] cells). Excess diacritics
//! past the third are ignored.

use bitty_vt::{Color, Rgb};

/// The Kitty Unicode placeholder base scalar.
pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// Start of the Kitty placeholder reservation window (issue #1400 title).
pub const KITTY_PLACEHOLDER_RANGE_START: u32 = 0x10EEEE;

/// End of the Kitty placeholder reservation window.
pub const KITTY_PLACEHOLDER_RANGE_END: u32 = 0x10EEFF;

/// Whether `ch` is the defined Kitty Unicode placeholder scalar.
///
/// Only `U+10EEEE` carries image content. Neighboring reservation-window
/// scalars are ordinary text (fail closed to glyphs, never to images).
#[must_use]
pub const fn is_kitty_placeholder(ch: char) -> bool {
    ch as u32 == KITTY_PLACEHOLDER as u32
}

/// Decoded identity of one placeholder cell: which stored image's which
/// tile this grid cell displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyUnicodeId {
    /// 32-bit image id: 24 low bits from the foreground color plus the
    /// optional third-diacritic high byte.
    pub image_id: u32,
    /// Placement id from the underline color; `None` means unspecified
    /// (any virtual placement of `image_id` may serve).
    pub placement_id: Option<u32>,
    /// Tile row inside the virtual placement (0-based).
    pub row: u8,
    /// Tile column inside the virtual placement (0-based).
    pub col: u8,
}

/// One decoded placeholder cell: grid position plus identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyUnicodeCell {
    /// Grid row of the cell.
    pub grid_row: usize,
    /// Grid column of the cell.
    pub grid_col: usize,
    /// Decoded image/tile identity.
    pub id: KittyUnicodeId,
}

/// One decoded placeholder run: the cells plus their shared key.
pub type KittyUnicodeRunCells = (Vec<KittyUnicodeCell>, (u32, Option<u32>));

/// Key identifying one placeholder run: the image (and placement, when
/// specified) whose tiles the run displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyUnicodeRun {
    /// Image id shared by every cell of the run.
    pub image_id: u32,
    /// Placement id when specified by underline color.
    pub placement_id: Option<u32>,
    /// Tile row of the run's first cell.
    pub row: u8,
    /// Tile column of the run's first cell.
    pub col: u8,
    /// Grid row of the run's first cell.
    pub grid_row: usize,
    /// Grid column of the run's first cell.
    pub grid_col: usize,
    /// Run width in cells (one grid row; multi-row spans form one run
    /// per row, mirroring the host reflow unit).
    pub width: usize,
}

/// Low 24 bits of a style color as a Kitty image/placement id fragment.
///
/// - `None` (default color): `0` (unspecified).
/// - `Indexed(i)`: `i`.
/// - `Rgb`: packed `r << 16 | g << 8 | b`.
///
/// This mirrors Ghostty's `colorToId`: palette and RGB colors share one
/// 24-bit id space (the 24 most significant bits of the color).
#[must_use]
pub const fn color_id_fragment(color: Option<Color>) -> u32 {
    match color {
        None | Some(Color::Default) => 0,
        Some(Color::Indexed(i)) => i as u32,
        Some(Color::Rgb(Rgb { r, g, b })) => (r as u32) << 16 | (g as u32) << 8 | (b as u32),
    }
}

/// Full 32-bit image id: 24 low bits from `foreground` plus the optional
/// third-diacritic high byte.
#[must_use]
pub fn kitty_image_id(foreground: Option<Color>, high: Option<u8>) -> u32 {
    let low = color_id_fragment(foreground) & 0x00FF_FFFF;
    low | ((high.unwrap_or(0) as u32) << 24)
}

/// Placement id from the underline color: absent or `0` means
/// unspecified (`None`); any other fragment is the explicit id.
#[must_use]
pub fn kitty_placement_id(underline_color: Option<Color>) -> Option<u32> {
    let fragment = color_id_fragment(underline_color);
    if fragment == 0 { None } else { Some(fragment) }
}

/// Row/column value of a diacritic scalar: its index in the frozen kitty
/// `rowcolumn-diacritics` table, or `None` when the scalar is not a table
/// member (treated as "unspecified", never an error).
#[must_use]
pub fn diacritic_index(mark: char) -> Option<u8> {
    let cp = mark as u32;
    ROWCOLUMN_DIACRITICS
        .binary_search(&cp)
        .ok()
        .and_then(|index| u8::try_from(index).ok())
}

/// Decodes one placeholder cell's marks into `(row, col, high)` with no
/// leftward context: `None` per position means "unspecified".
#[must_use]
pub fn decode_marks(marks: &[char]) -> (Option<u8>, Option<u8>, Option<u8>) {
    let row = marks.first().and_then(|mark| diacritic_index(*mark));
    let col = marks.get(1).and_then(|mark| diacritic_index(*mark));
    let high = marks.get(2).and_then(|mark| diacritic_index(*mark));
    (row, col, high)
}

/// Decodes one placeholder cell with full leftward context.
///
/// `prev` carries the previous cell's raw `(low fragment, placement id,
/// row, col, high)` tuple (`None` for a run start). Callers that chain
/// cells pass the previous decode back; see [`KittyRunBuilder`], which is
/// the only production caller and keeps colors exact. The direct
/// `(foreground, underline_color, prev_id)` form below is the testable
/// core: unspecified row/column/high-byte positions inherit from `prev`
/// when the image-low fragment and placement id match (the kitty "same
/// colors" rule); otherwise they default to `0`.
#[must_use]
pub fn decode_cell(
    foreground: Option<Color>,
    underline_color: Option<Color>,
    marks: &[char],
    prev: Option<&KittyUnicodeCell>,
) -> KittyUnicodeId {
    decode_cell_with_prev(
        foreground,
        underline_color,
        marks,
        prev.map(|p| {
            (
                p.id.image_id & 0x00FF_FFFF,
                p.id.placement_id,
                p.id.row,
                p.id.col,
                high_of(p),
            )
        }),
    )
}

/// Previous-cell context for [`decode_cell_with_prev`]: image-id low 24
/// bits, placement id, tile row, tile column, image-id high byte.
pub type KittyUnicodePrev = (u32, Option<u32>, u8, u8, Option<u8>);

/// Testable decode core: `prev` is the previous cell's raw tuple.
#[must_use]
pub fn decode_cell_with_prev(
    foreground: Option<Color>,
    underline_color: Option<Color>,
    marks: &[char],
    prev: Option<KittyUnicodePrev>,
) -> KittyUnicodeId {
    let low = color_id_fragment(foreground) & 0x00FF_FFFF;
    let placement_id = kitty_placement_id(underline_color);
    let (row_mark, col_mark, high_mark) = decode_marks(marks);
    let fallback = prev.filter(|(prev_low, prev_placement, _, _, _)| {
        *prev_low == low && *prev_placement == placement_id
    });
    let row = row_mark
        .or_else(|| fallback.map(|(_, _, row, _, _)| row))
        .unwrap_or(0);
    let mut col = col_mark
        .or_else(|| fallback.map(|(_, _, _, col, _)| col))
        .unwrap_or(0);
    // Bare continuation (no marks at all): the column advances one past
    // the previous cell. Explicit marks name absolute tile positions.
    if marks.is_empty() && fallback.is_some() {
        col = col.saturating_add(1);
    }
    let high = high_mark.or_else(|| fallback.and_then(|(_, _, _, _, high)| high));
    KittyUnicodeId {
        image_id: low | ((high.unwrap_or(0) as u32) << 24),
        placement_id,
        row,
        col,
    }
}

/// Accumulates one grid row's placeholder run left-to-right, applying the
/// kitty inheritance rules across adjacent cells.
///
/// Push returns the decoded cell, or `None` when the cell breaks the run
/// (different image id, different placement id, a new tile row, or an
/// explicit column that is not the expected continuation). Row breaks
/// always start a new run: multi-row spans form one run per row.
#[derive(Debug, Clone, Default)]
pub struct KittyRunBuilder {
    prev: Option<(KittyUnicodeCell, u32, Option<u32>)>,
    start: Option<KittyUnicodeRun>,
    width: usize,
    broken: bool,
}

impl KittyRunBuilder {
    /// An empty builder for one grid row.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one cell at `(grid_row, grid_col)` and either extends the
    /// run (returning `Some(cell)`) or reports a run break (`None`; the
    /// caller starts a new builder for the next run). Non-placeholder
    /// content always breaks the run.
    pub fn push(
        &mut self,
        grid_row: usize,
        grid_col: usize,
        is_placeholder: bool,
        foreground: Option<Color>,
        underline_color: Option<Color>,
        marks: &[char],
    ) -> Option<KittyUnicodeCell> {
        if self.broken || !is_placeholder {
            self.broken = true;
            return None;
        }
        let low = color_id_fragment(foreground) & 0x00FF_FFFF;
        let placement_id = kitty_placement_id(underline_color);
        let prev_tuple = self.prev.as_ref().map(|(cell, low, placement)| {
            (*low, *placement, cell.id.row, cell.id.col, high_of(cell))
        });
        let id = decode_cell_with_prev(foreground, underline_color, marks, prev_tuple);
        if let Some((prev, _prev_low, _prev_placement)) = &self.prev {
            let continues = prev.id.image_id == id.image_id
                && prev.id.placement_id == id.placement_id
                && prev.id.row == id.row
                && id.col == prev.id.col.saturating_add(1);
            // Absolute re-statement of the same tile (explicit marks that
            // repeat the previous cell's position) also continues: some
            // emitters always print full diacritics.
            let restated = prev.id == id;
            if !continues && !restated {
                self.broken = true;
                return None;
            }
            if restated {
                let cell = KittyUnicodeCell {
                    grid_row,
                    grid_col,
                    id: KittyUnicodeId {
                        col: prev.id.col.saturating_add(1),
                        ..id
                    },
                };
                self.prev = Some((cell, low, placement_id));
                self.width += 1;
                if let Some(start) = &mut self.start {
                    start.width = self.width;
                }
                return Some(cell);
            }
        }
        let cell = KittyUnicodeCell {
            grid_row,
            grid_col,
            id,
        };
        if self.start.is_none() {
            self.start = Some(KittyUnicodeRun {
                image_id: id.image_id,
                placement_id: id.placement_id,
                row: id.row,
                col: id.col,
                grid_row,
                grid_col,
                width: 1,
            });
        }
        self.prev = Some((cell, low, placement_id));
        self.width += 1;
        if let Some(start) = &mut self.start {
            start.width = self.width;
        }
        Some(cell)
    }

    /// The run accumulated so far, if any cell was accepted.
    #[must_use]
    pub fn run(&self) -> Option<KittyUnicodeRun> {
        self.start
    }

    /// Run key of the accumulated run, if any.
    #[must_use]
    pub fn run_key(&self) -> Option<(u32, Option<u32>)> {
        self.start.map(run_key_of)
    }
}

/// Run key of one decoded cell: `(image_id, placement_id)`.
#[must_use]
pub fn run_key(cell: &KittyUnicodeCell) -> (u32, Option<u32>) {
    (cell.id.image_id, cell.id.placement_id)
}

fn run_key_of(run: KittyUnicodeRun) -> (u32, Option<u32>) {
    (run.image_id, run.placement_id)
}

fn high_of(cell: &KittyUnicodeCell) -> Option<u8> {
    let high = (cell.id.image_id >> 24) as u8;
    if high == 0 { None } else { Some(high) }
}

/// Frozen kitty `rowcolumn-diacritics` table (sorted, 297 entries).
///
/// Derived from Unicode 6.0.0 combining class 230 marks with no
/// decomposition mapping; the index of a mark is its row/column value.
/// Vendored because the table never changes. Sorted for binary search.
/// Only indexes `0..=255` fit a `u8` tile coordinate; higher indexes
/// decode to `None` (unspecified, inherited leftwards).
#[rustfmt::skip]
const ROWCOLUMN_DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F,
    0x0346, 0x034A, 0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357,
    0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484,
    0x0485, 0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597,
    0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611,
    0x0612, 0x0613, 0x0614, 0x0615, 0x0616, 0x0617, 0x0657, 0x0658,
    0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2,
    0x06E4, 0x06E7, 0x06E8, 0x06EB, 0x06EC, 0x0730, 0x0732, 0x0733,
    0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE,
    0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817, 0x0818, 0x0819,
    0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C,
    0x082D, 0x0951, 0x0953, 0x0954, 0x0F82, 0x0F83, 0x0F86, 0x0F87,
    0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D,
    0x1B6E, 0x1B6F, 0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1,
    0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1,
    0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9,
    0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1,
    0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7,
    0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA,
    0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2,
    0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D,
    0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5,
    0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7,
    0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187,
    0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243,
    0x1D244,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_identity() {
        assert!(is_kitty_placeholder(KITTY_PLACEHOLDER));
        assert!(is_kitty_placeholder('\u{10EEEE}'));
        assert!(!is_kitty_placeholder('\u{10EEEF}'));
        assert!(!is_kitty_placeholder('A'));
        assert_eq!(KITTY_PLACEHOLDER_RANGE_START, 0x10EEEE);
        assert_eq!(KITTY_PLACEHOLDER_RANGE_END, 0x10EEFF);
    }

    #[test]
    fn diacritic_table_is_sorted_and_bounded() {
        assert_eq!(ROWCOLUMN_DIACRITICS.len(), 297);
        let mut sorted = ROWCOLUMN_DIACRITICS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 297, "table must hold 297 distinct marks");
        assert!(ROWCOLUMN_DIACRITICS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(diacritic_index('\u{0305}'), Some(0));
        assert_eq!(diacritic_index('\u{030D}'), Some(1));
        assert_eq!(diacritic_index('\u{030E}'), Some(2));
        assert_eq!(diacritic_index('\u{0483}'), Some(30));
        assert_eq!(diacritic_index('\u{A8E5}'), Some(255));
        // Indexes past u8::MAX are unspecified, never wrapped.
        assert_eq!(diacritic_index('\u{1D244}'), None);
        assert_eq!(diacritic_index('A'), None);
        assert_eq!(diacritic_index('\u{0301}'), None);
    }

    #[test]
    fn color_fragments_pack_like_ghostty() {
        assert_eq!(color_id_fragment(None), 0);
        assert_eq!(color_id_fragment(Some(Color::Default)), 0);
        assert_eq!(color_id_fragment(Some(Color::Indexed(42))), 42);
        assert_eq!(
            color_id_fragment(Some(Color::Rgb(Rgb { r: 0, g: 0, b: 42 }))),
            42
        );
        assert_eq!(
            color_id_fragment(Some(Color::Rgb(Rgb { r: 1, g: 2, b: 3 }))),
            0x010203
        );
        assert_eq!(kitty_image_id(Some(Color::Indexed(42)), None), 42);
        assert_eq!(
            kitty_image_id(Some(Color::Indexed(42)), Some(2)),
            42 + (2 << 24)
        );
        assert_eq!(kitty_placement_id(None), None);
        assert_eq!(kitty_placement_id(Some(Color::Default)), None);
        assert_eq!(kitty_placement_id(Some(Color::Indexed(0))), None);
        assert_eq!(kitty_placement_id(Some(Color::Indexed(21))), Some(21));
    }

    #[test]
    fn decode_cell_names_tiles() {
        let id = decode_cell(
            Some(Color::Indexed(42)),
            None,
            &['\u{0305}', '\u{030D}'],
            None,
        );
        assert_eq!(id.image_id, 42);
        assert_eq!(id.placement_id, None);
        assert_eq!((id.row, id.col), (0, 1));
        let id = decode_cell(
            Some(Color::Indexed(42)),
            None,
            &['\u{0305}', '\u{0305}', '\u{030E}'],
            None,
        );
        assert_eq!(id.image_id, 42 + (2 << 24));
        let id = decode_cell(
            Some(Color::Indexed(42)),
            Some(Color::Indexed(21)),
            &['\u{0305}', '\u{0305}'],
            None,
        );
        assert_eq!(id.placement_id, Some(21));
    }
}
