//! Bounded BiDi paragraph-split mechanics (M1-21, #1147).
//!
//! Candidate split half of the text-domain bidi contract: how a logical
//! paragraph longer than [`MAX_BIDI_PARAGRAPH_CELLS`] is divided into visual
//! chunks. This module implements the split mechanics only — chunk ranges
//! over `[0, total)` — and decides no open point: paragraph extent across
//! soft wraps and scrollback stays owner-pending under `OQ-091`, there is no
//! UAX #9 reorder algorithm here, no `Snapshot` change, and no renderer
//! consumption. See the BiDi Scope Decision record
//! (`docs/specifications/bidi-scope-decision.md` in bitty-terminal-docs).
//!
//! Split rules (pinned from the candidate Text and Rendering RFC BiDi
//! section): chunks break at cell boundaries, each chunk holds at most the
//! bound, and neutrals at a split resolve as `L` when the chunks are
//! eventually reordered independently. The helper consumes cell counts
//! only, never scalar values, so bidi control smuggling cannot influence
//! splits. The function is total: degenerate bounds saturate to a single
//! chunk rather than panicking.

/// Maximum cells of one logical paragraph processed as a single BiDi unit.
///
/// Candidate bound from the Text and Rendering RFC BiDi section (one very
/// wide logical line). Longer paragraphs are split with
/// [`bidi_paragraph_splits`]; see `OQ-091` for the still-open paragraph
/// extent across soft wraps and scrollback.
pub const MAX_BIDI_PARAGRAPH_CELLS: usize = 4096;

/// Split a `total_cells`-cell logical paragraph into visual chunks.
///
/// Returns half-open `[start, end)` cell ranges covering `[0, total_cells)`
/// in order, each holding at most `max_cells` cells. An empty paragraph
/// yields no chunks. A zero `max_cells` saturates to one chunk covering the
/// whole paragraph (fail-open, never panic).
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
}
