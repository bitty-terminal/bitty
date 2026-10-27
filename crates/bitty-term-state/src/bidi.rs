//! UAX #9 BiDi resolution for paint-time line reordering (CTX-1612, #1766).
//!
//! Terminal Truth stays logical: [`Cell`], [`Snapshot`](crate::Snapshot), the
//! canonical hash, and the snapshot codec never carry a paragraph level or a
//! visual position. This module resolves the Unicode Bidirectional Algorithm
//! over a *logical line* reconstructed from grid cells and exposes the two
//! column maps a consumer needs to paint and hit-test in visual order:
//!
//! - [`BidiGrid::visual_col_for_logical`] (paint: where a logical cell lands),
//! - [`BidiGrid::logical_col_for_visual`] (hit test: which logical cell is
//!   displayed at a visual column).
//!
//! Storage contract: with BiDi disabled (the default, see
//! `terminal.bidi = off`) no resolution runs and the renderer paints the
//! logical order exactly as before, so rendered output and hashes are
//! unchanged.
//!
//! Paragraph extent (OQ-091 point 1): one *logical line*. Soft-wrap flags
//! are supplied out of band ([`logical_line_ranges`]) because the snapshot
//! schema stays unchanged; a hard line break starts a new paragraph. The
//! viewport edge is a paragraph boundary for callers without wrap flags.
//! Lines longer than [`MAX_BIDI_PARAGRAPH_CELLS`] resolve in independent
//! chunks (neutrals at a split resolve as `L`), reusing the bounded split
//! mechanics this module has carried since M1-21.
//!
//! Unicode data: resolution uses `unicode-bidi` 0.3.18, whose generated
//! tables are UCD 16.0.0, while this crate's width/cluster tables are
//! UCD 18.0.0. Scalars assigned after 16.0.0 therefore fall back to that
//! crate's default bidi class. [`UNICODE_BIDI_VERSION`] names the expected
//! version and the conformance fixture asserts it (with an asserted count of
//! cases skipped for scalars outside UCD 16.0.0), so a dependency bump is
//! visible in the diff instead of silently changing coverage.
//!
//! Security: reordering is a display-spoofing capability (Trojan-Source
//! class, risk register R-004 covers paste inspection). Reordering is
//! opt-in via `terminal.bidi = on` until the owner accepts the risk.

use std::ops::Range;

use unicode_bidi::{BidiClass, Level, ParagraphBidiInfo};

use crate::Cell;

/// Maximum cells of one logical paragraph processed as a single BiDi unit.
///
/// Candidate bound from the Text and Rendering RFC BiDi section (one very
/// wide logical line). Longer paragraphs are split with
/// [`bidi_paragraph_splits`]; see `OQ-091` for the still-open paragraph
/// extent across soft wraps and scrollback.
pub const MAX_BIDI_PARAGRAPH_CELLS: usize = 4096;

/// The UCD version whose bidi classes `unicode-bidi` 0.3.18 ships.
///
/// The workspace's generated width/cluster tables are UCD 18.0.0; the skew
/// is deliberate (the dependency is the reviewed UBA implementation) and is
/// asserted by the conformance fixture. Bump this constant together with the
/// dependency and regenerate the fixture.
pub const UNICODE_BIDI_VERSION: (u64, u64, u64) = (16, 0, 0);

/// Base paragraph direction for [`resolve_text`] / [`BidiGrid::resolve`].
///
/// `Auto` is UAX #9 P2/P3 (first strong scalar); `Ltr`/`Rtl` force the
/// paragraph embedding level. `terminal.bidi_direction` is deferred (S0
/// resolves `Auto` only), but the engine already carries the explicit forms
/// so the config mapping is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParaDirection {
    /// First-strong paragraph detection ([`ParaDirection`] default).
    #[default]
    Auto,
    /// Force a left-to-right paragraph embedding level.
    Ltr,
    /// Force a right-to-left paragraph embedding level.
    Rtl,
}

impl ParaDirection {
    fn level(self) -> Option<Level> {
        match self {
            Self::Auto => None,
            Self::Ltr => Some(Level::ltr()),
            Self::Rtl => Some(Level::rtl()),
        }
    }
}

/// One logical line resolved by UAX #9, exposed per Unicode scalar.
///
/// This is the conformance/test surface; paint and hit testing consume
/// [`BidiGrid`], which maps the same levels onto grid cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidiText {
    /// Paragraph embedding level (P2/P3 or the forced direction).
    pub paragraph_level: u8,
    /// Resolved level per scalar (`text.chars()` order), with rule L1
    /// applied. X9-removed formatting scalars keep the crate's retained
    /// level; they are absent from [`Self::visual`].
    pub levels: Vec<u8>,
    /// Scalar indices in visual order, excluding X9-removed formatting
    /// scalars (the `BidiCharacterTest.txt` convention).
    pub visual: Vec<usize>,
    /// True when any resolved level is odd (or the paragraph level is RTL).
    pub has_rtl: bool,
}

/// Resolves one paragraph of text with the Unicode Bidirectional Algorithm.
///
/// Returns `None` when `text` is empty. The result is total over arbitrary
/// UTF-8: `unicode-bidi` is safe Rust and its tables cover every scalar
/// (unassigned scalars resolve as `L`).
#[must_use]
pub fn resolve_text(text: &str, direction: ParaDirection) -> Option<BidiText> {
    if text.is_empty() {
        return None;
    }
    let info = ParagraphBidiInfo::new(text, direction.level());
    let per_char = info.reordered_levels_per_char(0..text.len());
    let mut levels = Vec::with_capacity(per_char.len());
    let mut visual_levels = Vec::with_capacity(per_char.len());
    let mut visual = Vec::with_capacity(per_char.len());
    let mut has_rtl = info.paragraph_level.is_rtl();
    for (index, (byte, _)) in text.char_indices().enumerate() {
        let level = per_char.get(index).copied().unwrap_or(info.paragraph_level);
        levels.push(level.number());
        has_rtl |= level.is_rtl();
        // X9: explicit embedding/override/isolate controls and BN are
        // removed before reordering; keep the remaining scalar levels for
        // L2. `original_classes` is byte-indexed; every byte of a scalar
        // carries the same class.
        let class = info
            .original_classes
            .get(byte)
            .copied()
            .unwrap_or(BidiClass::L);
        if !removed_by_x9(class) {
            visual_levels.push(level);
            visual.push(index);
        }
    }
    let order = ParagraphBidiInfo::reorder_visual(&visual_levels);
    let visual = order
        .into_iter()
        .filter_map(|pos| visual.get(pos).copied())
        .collect();
    Some(BidiText {
        paragraph_level: info.paragraph_level.number(),
        levels,
        visual,
        has_rtl,
    })
}

/// True when `class` is removed by UAX #9 rule X9 before reordering.
fn removed_by_x9(class: BidiClass) -> bool {
    matches!(
        class,
        BidiClass::RLE
            | BidiClass::LRE
            | BidiClass::RLO
            | BidiClass::LRO
            | BidiClass::PDF
            | BidiClass::BN
    )
}

/// True when `scalar` is a Unicode bidi format control (marks, embeddings,
/// overrides, isolates).
///
/// These scalars are zero-width (see `char_cell_width`) and reorder text
/// without representing content. The print path keeps them in the preceding
/// cell's cluster tail; a control at the start of a line has no preceding
/// cell and is dropped (the honest limit of storing truth in fixed cells).
#[must_use]
pub const fn is_bidi_format_scalar(scalar: char) -> bool {
    matches!(
        scalar as u32,
        0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
    )
}

/// One physical row resolved for paint-time reordering.
///
/// All vectors are indexed by *grid column* (spacers included), so a
/// consumer never re-derives a column from a scalar index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidiLine {
    paragraph_level: u8,
    levels: Vec<u8>,
    visual_to_logical: Vec<u16>,
    logical_to_visual: Vec<u16>,
    /// Lead width per column (`2` on a wide lead, `1` otherwise).
    widths: Vec<u8>,
    /// True on the trailing half of a wide lead.
    spacers: Vec<bool>,
    /// Visual caret edge -> logical caret edge (inverse of
    /// [`Self::visual_caret_col`], length `columns + 1`).
    visual_edge_to_logical: Vec<u16>,
    /// Content columns; erased layout padding outside it keeps identity.
    content: Range<usize>,
    has_rtl: bool,
}

impl BidiLine {
    /// Paragraph embedding level this row was resolved under.
    #[must_use]
    pub const fn paragraph_level(&self) -> u8 {
        self.paragraph_level
    }

    /// Per-column resolved level (spacers inherit their lead's level).
    #[must_use]
    pub fn levels(&self) -> &[u8] {
        &self.levels
    }

    /// True when at least one column resolved to an odd level.
    #[must_use]
    pub const fn has_rtl(&self) -> bool {
        self.has_rtl
    }

    /// Logical grid column displayed at `visual_col`.
    ///
    /// The shared hit-test inverse. Out-of-range or padding columns are
    /// identity (total, never panics).
    #[must_use]
    pub fn logical_col_for_visual(&self, visual_col: usize) -> usize {
        if visual_col >= self.visual_to_logical.len() {
            return visual_col;
        }
        if !self.content.contains(&visual_col) {
            return visual_col;
        }
        usize::from(self.visual_to_logical.get(visual_col).copied().unwrap_or(0))
    }

    /// Visual grid column where `logical_col` is painted.
    ///
    /// The shared paint map. Out-of-range or padding columns are identity.
    #[must_use]
    pub fn visual_col_for_logical(&self, logical_col: usize) -> usize {
        if logical_col >= self.logical_to_visual.len() {
            return logical_col;
        }
        if !self.content.contains(&logical_col) {
            return logical_col;
        }
        usize::from(
            self.logical_to_visual
                .get(logical_col)
                .copied()
                .unwrap_or(0),
        )
    }

    /// Visual start of a logical span `[logical_col, logical_col + width)`.
    ///
    /// A wide cluster (and any shaped cluster spanning several cells) moves
    /// as one unit in RTL order, so its paint origin is the *minimum* visual
    /// position of its member cells, not the visual position of the leading
    /// logical cell. Clamps the span to the row.
    #[must_use]
    pub fn visual_span_start(&self, logical_col: usize, width: usize) -> usize {
        if self.logical_to_visual.is_empty() || !self.content.contains(&logical_col) {
            return logical_col;
        }
        let end = logical_col
            .saturating_add(width.max(1))
            .min(self.content.end);
        let start = logical_col.max(self.content.start);
        self.logical_to_visual[start..end]
            .iter()
            .copied()
            .min()
            .map_or(logical_col, usize::from)
    }

    /// Content column range this line reorders (layout padding excluded).
    #[must_use]
    pub fn content(&self) -> Range<usize> {
        self.content.clone()
    }

    /// Visual column of the caret at logical insertion point `logical_col`.
    ///
    /// A caret is an *edge*, not a cell: inside an RTL run the insertion
    /// point before a logical cell is that cell's right visual edge; the
    /// end-of-line caret (`logical_col == content.end`) sits at the leading
    /// edge of the last logical cell for an RTL tail and at the trailing
    /// edge for an LTR tail. Values below the content start and beyond the
    /// content end are identity (padding), never projected into the run.
    #[must_use]
    pub fn visual_caret_col(&self, logical_col: usize) -> usize {
        if self.content.is_empty() || logical_col < self.content.start {
            return logical_col;
        }
        if logical_col > self.content.end {
            return logical_col;
        }
        if logical_col == self.content.end {
            let mut last = self.content.end - 1;
            // A trailing spacer names the wide lead it belongs to; the caret
            // edge is the lead's, not the spacer's half-column.
            if self.spacers.get(last).copied().unwrap_or(false) && last > self.content.start {
                last -= 1;
            }
            let visual = self.visual_col_for_logical(last);
            return if self.levels.get(last).copied().unwrap_or(0) % 2 == 1 {
                visual
            } else {
                visual + usize::from(self.widths.get(last).copied().unwrap_or(1).max(1))
            };
        }
        let visual = self.visual_col_for_logical(logical_col);
        if self.levels.get(logical_col).copied().unwrap_or(0) % 2 == 1 {
            visual + usize::from(self.widths.get(logical_col).copied().unwrap_or(1).max(1))
        } else {
            visual
        }
    }

    /// Logical caret edge (0..=columns) displayed at visual caret edge.
    ///
    /// The edge inverse for drag selection: [`Self::logical_col_for_visual`]
    /// maps a *cell*, so using it for an edge shifts a right-boundary drag by
    /// one logical cell. Visual edges outside the content span are identity;
    /// content edges win inside it (the left edge of an RTL run is its
    /// logical end). Two logical edges can share one visual edge at a level
    /// run boundary; the later logical edge (the line end) wins, so a
    /// right-edge drag stays at the end instead of jumping into the run.
    #[must_use]
    pub fn logical_caret_edge_for_visual(&self, visual_edge: usize) -> usize {
        if visual_edge >= self.visual_edge_to_logical.len() {
            return visual_edge;
        }
        let in_content = (self.content.start..=self.content.end).contains(&visual_edge);
        if !in_content {
            return visual_edge;
        }
        usize::from(self.visual_edge_to_logical[visual_edge])
    }

    /// Builds the column maps for one physical row, or `None` when every
    /// lead level is even (logical order == visual order).
    ///
    /// `spacers[col]` marks the trailing half of a wide lead; a spacer is
    /// never a reorder unit and follows its lead in both maps.
    fn from_levels(
        paragraph_level: u8,
        levels: &[u8],
        spacers: &[bool],
        content: Range<usize>,
    ) -> Option<Self> {
        if levels.len() != spacers.len() || levels.is_empty() || content.is_empty() {
            return None;
        }
        // The maps are `u16` per column; a viewport wider than that fails
        // closed to identity rather than silently aliasing columns.
        if levels.len() > usize::from(u16::MAX) {
            return None;
        }
        let content = content.start.min(levels.len())..content.end.min(levels.len());
        // Lead cells only, in logical order. A leading orphan spacer is its
        // own unit (invalid grid truth, but the mapping stays total).
        let mut leads: Vec<usize> = Vec::with_capacity(content.len());
        let mut lead_levels: Vec<Level> = Vec::with_capacity(content.len());
        for col in content.clone() {
            let spacer = spacers[col] && col > 0;
            if spacer {
                continue;
            }
            leads.push(col);
            lead_levels.push(Level::new(levels[col]).unwrap_or(Level::ltr()));
        }
        if lead_levels.iter().all(Level::is_ltr) {
            return None;
        }
        let order = ParagraphBidiInfo::reorder_visual(&lead_levels);
        let mut visual_to_logical: Vec<u16> = (0..levels.len())
            .map(|col| u16::try_from(col).unwrap_or(u16::MAX))
            .collect();
        let mut logical_to_visual = visual_to_logical.clone();
        let mut visual_col = content.start;
        for lead_index in order {
            let Some(&logical) = leads.get(lead_index) else {
                continue;
            };
            // A wide lead owns its spacer column in the same visual unit.
            let width = if spacers.get(logical + 1).copied().unwrap_or(false) {
                2
            } else {
                1
            };
            for offset in 0..width {
                let v = u16::try_from(visual_col + offset).unwrap_or(u16::MAX);
                let l = u16::try_from(logical + offset).unwrap_or(u16::MAX);
                if let Some(slot) = logical_to_visual.get_mut(logical + offset) {
                    *slot = v;
                }
                if let Some(slot) = visual_to_logical.get_mut(visual_col + offset) {
                    *slot = l;
                }
            }
            visual_col += width;
        }
        let mut line = Self {
            paragraph_level,
            levels: levels.to_vec(),
            visual_to_logical,
            logical_to_visual,
            spacers: spacers.to_vec(),
            visual_edge_to_logical: (0..=levels.len())
                .map(|edge| u16::try_from(edge).unwrap_or(u16::MAX))
                .collect(),
            widths: spacers
                .iter()
                .enumerate()
                .map(|(col, spacer)| {
                    if *spacer {
                        1
                    } else if spacers.get(col + 1).copied().unwrap_or(false) {
                        2
                    } else {
                        1
                    }
                })
                .collect(),
            content: content.clone(),
            has_rtl: true,
        };
        for logical in content.start..=content.end {
            let visual = line.visual_caret_col(logical);
            if let Some(slot) = line.visual_edge_to_logical.get_mut(visual) {
                *slot = u16::try_from(logical).unwrap_or(u16::MAX);
            }
        }
        Some(line)
    }
}

/// Resolved reordering maps for every row of a grid snapshot.
///
/// Construction resolves each logical line once (soft-wrap groups when the
/// caller supplies flags, row-local paragraphs otherwise) and stores only
/// rows that actually reorder; pure-LTR rows cost no allocation. With BiDi
/// disabled callers simply do not construct a grid and all paint/hit-test
/// paths keep their logical arithmetic.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BidiGrid {
    rows: Vec<Option<BidiLine>>,
}

impl BidiGrid {
    /// Resolves every row of `cells` (`width * height`, row-major).
    ///
    /// `wraps[r] == true` means physical row `r` continues on row `r + 1`
    /// (a soft wrap), exactly like `State::row_soft_wrapped`. Pass `None`
    /// when the caller has no wrap hint: each row is then its own paragraph
    /// (the viewport edge is a paragraph boundary).
    #[must_use]
    pub fn resolve(
        cells: &[Cell],
        width: usize,
        height: usize,
        wraps: Option<&[bool]>,
        direction: ParaDirection,
    ) -> Self {
        if width == 0 || height == 0 || cells.len() < width.saturating_mul(height) {
            return Self::default();
        }
        let mut rows: Vec<Option<BidiLine>> = vec![None; height];
        for range in logical_line_ranges(wraps, height) {
            let start = range.start * width;
            let end = range.end * width;
            let Some(line_cells) = cells.get(start..end) else {
                continue;
            };
            let resolved = resolve_cell_levels(line_cells, width, direction);
            let Some((paragraph_level, levels, content)) = resolved else {
                continue;
            };
            if !levels.iter().any(|level| level % 2 == 1) {
                continue;
            }
            for row in range.clone() {
                let offset = (row - range.start) * width;
                let row_levels = &levels[offset..offset + width];
                let row_cells = &line_cells[offset..offset + width];
                // Intersect the paragraph content span with this physical
                // row; erased layout padding outside it keeps identity.
                let row_start = content.start.saturating_sub(offset).min(width);
                let row_end = content.end.saturating_sub(offset).min(width);
                if row_end <= row_start {
                    continue;
                }
                rows[row] = BidiLine::from_levels(
                    paragraph_level,
                    row_levels,
                    &spacers(row_cells),
                    row_start..row_end,
                );
            }
        }
        Self { rows }
    }

    /// The row's reordering maps, or `None` for logical (LTR) order.
    #[must_use]
    pub fn line(&self, row: usize) -> Option<&BidiLine> {
        self.rows.get(row).and_then(Option::as_ref)
    }

    /// True when no row needs reordering.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(Option::is_none)
    }

    /// Shared hit-test inverse: visual column -> logical column.
    ///
    /// Identity when the row is not reordered, so every consumer can call
    /// this unconditionally without a second branch.
    #[must_use]
    pub fn logical_col_for_visual(&self, row: usize, visual_col: usize) -> usize {
        self.line(row)
            .map_or(visual_col, |line| line.logical_col_for_visual(visual_col))
    }

    /// Shared paint map: logical column -> visual column.
    ///
    /// Identity when the row is not reordered.
    #[must_use]
    pub fn visual_col_for_logical(&self, row: usize, logical_col: usize) -> usize {
        self.line(row)
            .map_or(logical_col, |line| line.visual_col_for_logical(logical_col))
    }

    /// Shared paint map for a logical span; identity when not reordered.
    #[must_use]
    pub fn visual_span_start(&self, row: usize, logical_col: usize, width: usize) -> usize {
        self.line(row).map_or(logical_col, |line| {
            line.visual_span_start(logical_col, width)
        })
    }

    /// Shared caret map (cursor/IME): logical insertion point -> visual edge.
    ///
    /// Identity when not reordered.
    #[must_use]
    pub fn visual_caret_col(&self, row: usize, logical_col: usize) -> usize {
        self.line(row)
            .map_or(logical_col, |line| line.visual_caret_col(logical_col))
    }

    /// Shared drag-caret inverse: visual edge -> logical edge.
    ///
    /// Identity when not reordered. This is the edge inverse, not
    /// [`Self::logical_col_for_visual`] (which maps a cell).
    #[must_use]
    pub fn logical_caret_edge_for_visual(&self, row: usize, visual_edge: usize) -> usize {
        self.line(row).map_or(visual_edge, |line| {
            line.logical_caret_edge_for_visual(visual_edge)
        })
    }
}

/// Splits soft-wrap flags into logical-line row ranges.
///
/// `wraps[r] == true` continues the line on the next row; a missing flag
/// (out-of-range) ends the line. Always covers `[0, rows)` without gaps.
#[must_use]
pub fn logical_line_ranges(wraps: Option<&[bool]>, rows: usize) -> Vec<Range<usize>> {
    if rows == 0 {
        return Vec::new();
    }
    let Some(wraps) = wraps else {
        return (0..rows).map(|row| row..row + 1).collect();
    };
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for row in 0..rows {
        if !wraps.get(row).copied().unwrap_or(false) {
            ranges.push(start..row + 1);
            start = row + 1;
        }
    }
    if start < rows {
        ranges.push(start..rows);
    }
    ranges
}

/// Resolves one logical line's cells into per-cell levels (`width` columns
/// per physical row), or `None` when the line is empty.
fn resolve_cell_levels(
    line_cells: &[Cell],
    _width: usize,
    direction: ParaDirection,
) -> Option<(u8, Vec<u8>, Range<usize>)> {
    if line_cells.is_empty() {
        return None;
    }
    // Erased padding (never printed) is not part of the paragraph: trailing
    // padding at paragraph level RTL would otherwise right-align a short
    // RTL line, and leading padding would shift it. A space the application
    // printed (`padding == false`) stays content.
    let is_padding = |cell: &Cell| cell.padding && cell.is_blank();
    let first = line_cells.iter().position(|cell| !is_padding(cell))?;
    let last = line_cells.iter().rposition(|cell| !is_padding(cell))? + 1;
    if first >= last {
        return None;
    }
    // One entry per lead cell, in logical order: (cell index, start byte).
    let mut text = String::new();
    let mut leads: Vec<(usize, usize)> = Vec::with_capacity(last - first);
    for (index, cell) in line_cells.iter().enumerate().take(last).skip(first) {
        if cell.spacer {
            continue;
        }
        leads.push((index, text.len()));
        cell.push_text_into(&mut text);
    }
    if leads.is_empty() || text.is_empty() {
        return None;
    }
    let mut levels = vec![0u8; line_cells.len()];
    let mut paragraph_level = 0u8;
    // Bound the work per paragraph: resolve independent chunks of at most
    // MAX_BIDI_PARAGRAPH_CELLS lead cells. Neutrals at a split resolve as L
    // in each chunk, matching the historical split contract. The split
    // helper is the single source of the chunk arithmetic.
    for (chunk_start, chunk_end) in bidi_paragraph_splits(leads.len(), MAX_BIDI_PARAGRAPH_CELLS) {
        let Some(chunk) = leads.get(chunk_start..chunk_end) else {
            continue;
        };
        let first_byte = chunk.first().map_or(0, |(_, byte)| *byte);
        let end_byte = leads.get(chunk_end).map_or(text.len(), |(_, byte)| *byte);
        let chunk_text = text.get(first_byte..end_byte).unwrap_or("");
        let Some(resolved) = resolve_text(chunk_text, direction) else {
            continue;
        };
        if chunk_start == 0 {
            paragraph_level = resolved.paragraph_level;
        }
        // Merge the chunk's scalar levels with its lead offsets; both lists
        // are ascending, so one pass suffices.
        let mut char_levels = resolved
            .levels
            .iter()
            .copied()
            .zip(chunk_text.char_indices().map(|(byte, _)| first_byte + byte))
            .peekable();
        for (cell_index, byte) in chunk.iter().copied() {
            if byte >= end_byte {
                continue;
            }
            while char_levels.peek().is_some_and(|(_, offset)| *offset < byte) {
                let _ = char_levels.next();
            }
            let level = match char_levels.peek() {
                Some((level, offset)) if *offset == byte => *level,
                _ => 0,
            };
            levels[cell_index] = level;
        }
    }
    // Spacers inherit their lead's level; a wide pair is one reorder unit.
    for index in 0..line_cells.len() {
        if line_cells[index].spacer && index > 0 {
            levels[index] = levels[index - 1];
        }
    }
    Some((paragraph_level, levels, first..last))
}

/// Per-column spacer flags for [`BidiLine::from_levels`].
fn spacers(cells: &[Cell]) -> Vec<bool> {
    cells.iter().map(|cell| cell.spacer).collect()
}

/// Splits a `total_cells`-cell logical paragraph into visual chunks.
///
/// Returns half-open `[start, end)` cell ranges covering `[0, total_cells)`
/// in order, each holding at most `max_cells` cells. An empty paragraph
/// yields no chunks. A zero `max_cells` saturates to one chunk covering the
/// whole paragraph (fail-open, never panic).
#[must_use]
pub fn bidi_paragraph_splits(total_cells: usize, max_cells: usize) -> Vec<(usize, usize)> {
    if total_cells == 0 {
        return Vec::new();
    }
    let bound = if max_cells == 0 {
        total_cells
    } else {
        max_cells
    };
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < total_cells {
        let end = start.saturating_add(bound).min(total_cells);
        chunks.push((start, end));
        start = end;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_paragraph_yields_no_chunks() {
        assert!(bidi_paragraph_splits(0, MAX_BIDI_PARAGRAPH_CELLS).is_empty());
        assert!(bidi_paragraph_splits(0, 0).is_empty());
    }

    #[test]
    fn short_paragraph_is_one_chunk() {
        assert_eq!(
            bidi_paragraph_splits(120, MAX_BIDI_PARAGRAPH_CELLS),
            vec![(0, 120)]
        );
    }

    #[test]
    fn exact_bound_is_one_chunk() {
        assert_eq!(
            bidi_paragraph_splits(MAX_BIDI_PARAGRAPH_CELLS, MAX_BIDI_PARAGRAPH_CELLS),
            vec![(0, MAX_BIDI_PARAGRAPH_CELLS)]
        );
    }

    #[test]
    fn bound_plus_one_splits_at_cell_boundary() {
        assert_eq!(
            bidi_paragraph_splits(MAX_BIDI_PARAGRAPH_CELLS + 1, MAX_BIDI_PARAGRAPH_CELLS),
            vec![
                (0, MAX_BIDI_PARAGRAPH_CELLS),
                (MAX_BIDI_PARAGRAPH_CELLS, MAX_BIDI_PARAGRAPH_CELLS + 1)
            ]
        );
    }

    #[test]
    fn chunks_cover_without_gaps_or_overlap() {
        let total = 3 * MAX_BIDI_PARAGRAPH_CELLS + 7;
        let chunks = bidi_paragraph_splits(total, MAX_BIDI_PARAGRAPH_CELLS);
        assert_eq!(chunks.len(), 4);
        for (start, end) in &chunks {
            assert!(start < end);
            assert!(end - start <= MAX_BIDI_PARAGRAPH_CELLS);
        }
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks[chunks.len() - 1].1, total);
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn zero_bound_saturates_to_single_chunk() {
        assert_eq!(bidi_paragraph_splits(300, 0), vec![(0, 300)]);
    }

    #[test]
    fn crate_tables_are_the_asserted_ucd_version() {
        assert_eq!(unicode_bidi::UNICODE_VERSION, UNICODE_BIDI_VERSION);
        assert_eq!(UNICODE_BIDI_VERSION, (16, 0, 0));
        // Data-skew sentinel: U+1CCFA (SNAKE SYMBOL) was assigned in UCD
        // 17.0 as `ON`, but the crate's UCD 16.0.0 tables still classify it
        // as the unassigned default `L` (its block has no RTL default). The
        // repo's generated width/cluster tables are UCD 18.0.0. Arabic-block
        // additions (for example U+088F) still resolve `R` by block default,
        // which is why the conformance fixture needs 0 skipped cases today;
        // a dependency bump that changes that count is visible in the
        // fixture/constant diff.
        assert_eq!(unicode_bidi::bidi_class('\u{1CCFA}'), BidiClass::L);
    }

    #[test]
    fn pure_ltr_text_needs_no_reorder() {
        let resolved = resolve_text("hello world", ParaDirection::Auto).expect("text");
        assert_eq!(resolved.paragraph_level, 0);
        assert!(!resolved.has_rtl);
        assert_eq!(resolved.levels, vec![0; 11]);
        assert_eq!(resolved.visual, (0..11).collect::<Vec<_>>());
    }

    #[test]
    fn hebrew_text_reverses_in_visual_order() {
        let resolved = resolve_text("אבג", ParaDirection::Auto).expect("text");
        assert_eq!(resolved.paragraph_level, 1);
        assert_eq!(resolved.visual, vec![2, 1, 0]);
        assert!(resolved.has_rtl);
    }

    #[test]
    fn x9_controls_are_removed_from_the_visual_list() {
        // RLO ... PDF around Latin, then a plain scalar.
        let resolved = resolve_text("\u{202E}ab\u{202C}c", ParaDirection::Auto).expect("text");
        // `a` and `b` are removed from the reorder list; the retained
        // levels keep their crate value and `c` stays visible.
        assert_eq!(resolved.visual.len(), 3);
        assert_eq!(resolved.levels.len(), 5);
    }

    #[test]
    fn logical_line_ranges_respect_soft_wraps() {
        let wraps = [true, false, true, true, false];
        assert_eq!(logical_line_ranges(Some(&wraps), 5), vec![0..2, 2..5]);
        assert_eq!(logical_line_ranges(None, 3), vec![0..1, 1..2, 2..3]);
        assert!(logical_line_ranges(None, 0).is_empty());
    }
}
