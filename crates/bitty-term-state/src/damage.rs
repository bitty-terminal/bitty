//! Damage tracking model (RFC "Damage tracking model").
//!
//! Each processed action batch records a damaged rectangle set over grid
//! coordinates plus scrollback line ranges. Print produces per-cell marks
//! that are coalesced into runs; erase and scroll produce coarse rectangles.
//! Damage is tagged with the state's monotonically increasing `generation`,
//! which increments exactly once per processed batch, and snapshots embed
//! that generation so renderers can consume `snapshot + damage-since(gen)`
//! without reading grid internals (presentation boundary).
//!
//! Coalescing is deterministic: primitives are sorted by `(top, left,
//! bottom, right)` and merged at a fixed point when row spans overlap or
//! touch and column spans overlap or touch. Over-damage (a union covering
//! undamaged cells) is safe; under-damage is impossible because every merge
//! covers both inputs entirely. Batches are bounded by
//! [`DAMAGE_MAX_REGIONS_PER_BATCH`]; exceeding it falls back to one coarse
//! full-grid rectangle.

/// Hard cap on regions retained per damage batch before falling back to a
/// full-grid rectangle (bounded memory per threat T-01).
pub const DAMAGE_MAX_REGIONS_PER_BATCH: usize = 256;

/// Number of recent batches retained for `damage_since` queries.
pub const DAMAGE_HISTORY_BATCHES: usize = 64;

/// Inclusive grid-coordinate rectangle (both corners damaged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DamageRect {
    /// First damaged row.
    pub top: u16,
    /// First damaged column.
    pub left: u16,
    /// Last damaged row.
    pub bottom: u16,
    /// Last damaged column.
    pub right: u16,
}

impl DamageRect {
    /// The full-extent rectangle for a `rows x cols` grid.
    #[must_use]
    pub fn full(rows: u16, cols: u16) -> Self {
        Self {
            top: 0,
            left: 0,
            bottom: rows.saturating_sub(1),
            right: cols.saturating_sub(1),
        }
    }

    fn overlaps_or_touches_rows(&self, other: &DamageRect) -> bool {
        self.top <= other.bottom.saturating_add(1) && other.top <= self.bottom.saturating_add(1)
    }

    fn overlaps_or_touches_cols(&self, other: &DamageRect) -> bool {
        self.left <= other.right.saturating_add(1) && other.left <= self.right.saturating_add(1)
    }

    fn union(&self, other: &DamageRect) -> DamageRect {
        DamageRect {
            top: self.top.min(other.top),
            left: self.left.min(other.left),
            bottom: self.bottom.max(other.bottom),
            right: self.right.max(other.right),
        }
    }
}

/// One damaged region of the terminal presentation surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DamagedRegion {
    /// A rectangle of the visible grid.
    Grid(DamageRect),
    /// Scrollback lines with ids in `[first_line_id, first_line_id + count)`.
    Scrollback {
        /// First affected line id.
        first_line_id: u64,
        /// Number of consecutive affected lines (never zero).
        count: u64,
    },
}

/// The damage produced by one processed action batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Damage {
    /// Generation counter value after the batch was applied; snapshots
    /// embed the same number.
    pub generation: u64,
    /// Coalesced damaged regions in deterministic order (sorted, then
    /// fixed-point merged; scrollback ranges after grid rectangles).
    pub regions: Box<[DamagedRegion]>,
}

impl Damage {
    /// An empty batch (the action touched nothing observable).
    #[must_use]
    pub fn empty(generation: u64) -> Self {
        Self {
            generation,
            regions: Box::new([]),
        }
    }
}

/// Deterministic coalescing pass over accumulated primitives plus
/// scrollback events.
///
/// Sorts, merges grid rectangles at a fixed point, applies the region cap,
/// then merges contiguous scrollback id ranges. Output ordering is part of
/// the replay contract: identical action sequences produce byte-identical
/// `regions` vectors.
pub(crate) fn coalesce(
    mut rects: Vec<DamageRect>,
    mut scroll_events: Vec<(u64, u64)>,
    grid_rows: u16,
    grid_cols: u16,
) -> Vec<DamagedRegion> {
    if rects.len() > 1 {
        rects.sort_unstable_by(|a, b| {
            (a.top, a.left, a.bottom, a.right).cmp(&(b.top, b.left, b.bottom, b.right))
        });
        loop {
            let mut merged_any = false;
            let mut i = 0;
            while i < rects.len() {
                let mut j = i + 1;
                while j < rects.len() {
                    if rects[i].overlaps_or_touches_rows(&rects[j])
                        && rects[i].overlaps_or_touches_cols(&rects[j])
                    {
                        let united = rects[i].union(&rects[j]);
                        rects.swap(i, j);
                        rects[i] = united;
                        rects.remove(j);
                        // Keep position i stable so chains merge fully.
                        j = i + 1;
                        merged_any = true;
                    } else {
                        j += 1;
                    }
                }
                i += 1;
            }
            if !merged_any {
                break;
            }
        }
    }
    let mut regions: Vec<DamagedRegion> = if rects.len() > DAMAGE_MAX_REGIONS_PER_BATCH {
        vec![DamagedRegion::Grid(DamageRect::full(grid_rows, grid_cols))]
    } else {
        rects.into_iter().map(DamagedRegion::Grid).collect()
    };
    scroll_events.retain(|&(_, count)| count > 0);
    if !scroll_events.is_empty() {
        scroll_events.sort_unstable();
        let mut merged_scroll: Vec<(u64, u64)> = Vec::with_capacity(scroll_events.len());
        for (first, count) in scroll_events {
            match merged_scroll.last_mut() {
                Some(last) if last.0 + last.1 == first => last.1 += count,
                _ => merged_scroll.push((first, count)),
            }
        }
        regions.extend(merged_scroll.into_iter().map(|(first_line_id, count)| {
            DamagedRegion::Scrollback {
                first_line_id,
                count,
            }
        }));
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(top: u16, left: u16, bottom: u16, right: u16) -> DamageRect {
        DamageRect {
            top,
            left,
            bottom,
            right,
        }
    }

    #[test]
    fn print_runs_merge_into_contiguous_rectangle() {
        // Three per-print marks on one row, adjacent columns.
        let out = coalesce(
            vec![rect(0, 0, 0, 0), rect(0, 1, 0, 1), rect(0, 2, 0, 2)],
            vec![],
            24,
            80,
        );
        assert_eq!(out, vec![DamagedRegion::Grid(rect(0, 0, 0, 2))]);
    }

    #[test]
    fn disjoint_marks_stay_separate_in_sorted_order() {
        let out = coalesce(vec![rect(5, 10, 5, 12), rect(0, 0, 0, 3)], vec![], 24, 80);
        assert_eq!(
            out,
            vec![
                DamagedRegion::Grid(rect(0, 0, 0, 3)),
                DamagedRegion::Grid(rect(5, 10, 5, 12))
            ]
        );
    }

    #[test]
    fn touching_rows_merge() {
        let out = coalesce(vec![rect(0, 0, 0, 79), rect(1, 0, 1, 79)], vec![], 24, 80);
        assert_eq!(out, vec![DamagedRegion::Grid(rect(0, 0, 1, 79))]);
    }

    #[test]
    fn scroll_ranges_coalesce_when_contiguous() {
        let out = coalesce(vec![], vec![(0, 2), (4, 1), (2, 2)], 24, 80);
        // (0..2) + (2..4) are contiguous and merge; (4..5) chains onto them.
        assert_eq!(
            out,
            vec![DamagedRegion::Scrollback {
                first_line_id: 0,
                count: 5
            }]
        );
    }

    #[test]
    fn oversized_batch_falls_back_to_full_grid() {
        // Even rows only: no two rectangles touch, so none merge and the
        // region cap forces the coarse full-grid fallback.
        let many: Vec<DamageRect> = (0..600u16).step_by(2).map(|r| rect(r, 0, r, 79)).collect();
        let out = coalesce(many, vec![], 24, 80);
        assert_eq!(out, vec![DamagedRegion::Grid(DamageRect::full(24, 80))]);
    }
}
