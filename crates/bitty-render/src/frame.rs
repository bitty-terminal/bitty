//! Damage-aware frame planning.
//!
//! [`plan_frame`] turns a [`DamageDescriptor`] — a generic, pixel-domain view
//! of "what changed" — into an owned [`FramePlan`] that a renderer backend can
//! execute without knowing anything about terminal grids.
//!
//! # Descriptor contract
//!
//! The descriptor is deliberately generic: this slice does not depend on
//! `bitty-term-state`. The grid-integration slice will implement
//! [`DamageDescriptor`] for (or in front of) the public render snapshot types,
//! converting grid-coordinate damage into pixel rectangles using the cell
//! metrics of the loaded font. Until then the trait keeps the planner fully
//! unit-testable on headless CI.
//!
//! # Decision rules (deterministic)
//!
//! 1. A zero extent or an empty region list yields [`FrameMode::Clean`]
//!    (nothing to draw), unless a full-redraw hint is set.
//! 2. A full-redraw hint, any region covering the whole extent, or an input
//!    region count at or above [`MAX_FRAME_REGIONS`] yields
//!    [`FrameMode::Full`] with exactly one full-extent rectangle.
//! 3. Otherwise the plan is [`FrameMode::Partial`]: regions are clipped to
//!    the extent, empty survivors dropped, and overlapping-or-touching
//!    rectangles coalesced left-to-right, top-to-bottom. If coalescing still
//!    leaves more than [`MAX_FRAME_REGIONS`] rectangles (bounded output), the
//!    plan degrades to [`FrameMode::Full`].
//!
//! Like the term-state damage model, over-damage is safe and under-damage is
//! impossible: clipping only shrinks, and every fallback covers everything.

use crate::geometry::{ExtentPx, RectPx};

/// Hard cap on damage regions considered per frame before falling back to a
/// full redraw. Mirrors the per-batch bound of the term-state damage model so
/// both sides of the presentation boundary stay bounded by construction.
pub const MAX_FRAME_REGIONS: usize = 256;

/// Generic, pixel-domain description of what changed since the last frame.
///
/// Implementors must report damage conservatively: every changed pixel must
/// lie inside at least one reported region. Regions may overlap; the planner
/// normalizes them.
pub trait DamageDescriptor {
    /// Logical pixel extent of the target surface for this frame.
    fn extent(&self) -> ExtentPx;

    /// Damaged rectangles in logical pixels. An empty slice means nothing
    /// changed.
    fn damaged_regions(&self) -> &[RectPx];

    /// Optional hint that this frame must be drawn from scratch regardless of
    /// the region list (first frame after realization, resize, device-loss
    /// recovery). Defaults to `false`.
    fn full_redraw_hint(&self) -> bool {
        false
    }
}

/// How much of the surface the frame needs to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    /// Nothing changed; skip drawing entirely.
    Clean,
    /// Only the listed dirty rectangles need drawing.
    Partial,
    /// Draw everything (`dirty_rects` contains one full-extent rectangle).
    Full,
}

/// Owned plan for one frame, produced by [`plan_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePlan {
    /// The extent the plan was computed against.
    pub extent: ExtentPx,
    /// Decision outcome.
    pub mode: FrameMode,
    /// Rectangles to draw. Empty for [`FrameMode::Clean`]; exactly one
    /// full-extent rectangle for [`FrameMode::Full`]; clipped-and-coalesced
    /// regions for [`FrameMode::Partial`]. Sorted deterministically in
    /// `(x, y, width, height)` field order.
    pub dirty_rects: Vec<RectPx>,
}

impl FramePlan {
    /// True when any drawing work exists for this frame.
    #[must_use]
    pub fn needs_draw(&self) -> bool {
        self.mode != FrameMode::Clean && !self.dirty_rects.is_empty()
    }
}

/// Plans one frame from a damage descriptor (see module docs for rules).
#[must_use = "the produced FramePlan should drive the frame's draw decisions"]
pub fn plan_frame<D: DamageDescriptor + ?Sized>(damage: &D) -> FramePlan {
    let extent = damage.extent();
    let regions = damage.damaged_regions();

    if extent.is_zero() {
        return FramePlan {
            extent,
            mode: FrameMode::Clean,
            dirty_rects: Vec::new(),
        };
    }

    let full_rect = RectPx::full(&extent);

    if damage.full_redraw_hint()
        || regions.len() >= MAX_FRAME_REGIONS
        || regions.iter().any(|r| r.covers_extent(&extent))
    {
        return FramePlan {
            extent,
            mode: FrameMode::Full,
            dirty_rects: vec![full_rect],
        };
    }

    if regions.is_empty() {
        return FramePlan {
            extent,
            mode: FrameMode::Clean,
            dirty_rects: Vec::new(),
        };
    }

    let mut clipped: Vec<RectPx> = regions
        .iter()
        .filter_map(|r| r.clip_to_extent(&extent))
        .collect();
    if clipped.is_empty() {
        // All regions fell outside the (non-zero) extent: nothing to draw.
        return FramePlan {
            extent,
            mode: FrameMode::Clean,
            dirty_rects: Vec::new(),
        };
    }

    coalesce(&mut clipped);

    if clipped.len() > MAX_FRAME_REGIONS {
        return FramePlan {
            extent,
            mode: FrameMode::Full,
            dirty_rects: vec![full_rect],
        };
    }

    FramePlan {
        extent,
        mode: FrameMode::Partial,
        dirty_rects: clipped,
    }
}

/// Sorts by `(x, y)` and merges rectangles that overlap or touch until a
/// fixed point is reached. Deterministic: equal inputs produce equal outputs.
///
/// Complexity: each sweep is O(n²) comparisons plus removals, and sweeps
/// repeat only while merges occur; `n` is bounded by [`MAX_FRAME_REGIONS`] at
/// the call site, so the worst case stays small and bounded.
fn coalesce(rects: &mut Vec<RectPx>) {
    rects.sort();
    loop {
        let mut merged = false;
        for i in 0..rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                if overlaps_or_touches(&rects[i], &rects[j]) {
                    rects[i] = RectPx::union_bounding(&rects[i], &rects[j]);
                    rects.remove(j);
                    merged = true;
                } else {
                    j += 1;
                }
            }
        }
        if !merged {
            return;
        }
        rects.sort();
    }
}

/// True when two rectangles overlap or touch on both axes. Touching counts
/// as mergeable so run-shaped damage collapses into single rectangles.
fn overlaps_or_touches(a: &RectPx, b: &RectPx) -> bool {
    let rows_touch =
        i64::from(a.y) <= b.bottom_exclusive() && i64::from(b.y) <= a.bottom_exclusive();
    let cols_touch = i64::from(a.x) <= b.right_exclusive() && i64::from(b.x) <= a.right_exclusive();
    rows_touch && cols_touch
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Desc {
        extent: ExtentPx,
        regions: Vec<RectPx>,
        hint: bool,
    }

    impl DamageDescriptor for Desc {
        fn extent(&self) -> ExtentPx {
            self.extent
        }
        fn damaged_regions(&self) -> &[RectPx] {
            &self.regions
        }
        fn full_redraw_hint(&self) -> bool {
            self.hint
        }
    }

    fn desc(extent: (u32, u32), regions: &[RectPx]) -> Desc {
        Desc {
            extent: ExtentPx::new(extent.0, extent.1),
            regions: regions.to_vec(),
            hint: false,
        }
    }

    #[test]
    fn zero_extent_is_clean() {
        let plan = plan_frame(&desc((0, 0), &[RectPx::new(0, 0, 5, 5)]));
        assert_eq!(plan.mode, FrameMode::Clean);
        assert!(!plan.needs_draw());
    }

    #[test]
    fn no_damage_is_clean_even_with_hint_off() {
        let plan = plan_frame(&desc((80, 24), &[]));
        assert_eq!(plan.mode, FrameMode::Clean);
        assert!(plan.dirty_rects.is_empty());
    }

    #[test]
    fn hint_forces_full_redraw() {
        let mut d = desc((80, 24), &[]);
        d.hint = true;
        let plan = plan_frame(&d);
        assert_eq!(plan.mode, FrameMode::Full);
        assert_eq!(plan.dirty_rects, vec![RectPx::new(0, 0, 80, 24)]);
        assert!(plan.needs_draw());
    }

    #[test]
    fn covering_region_promotes_to_full() {
        let plan = plan_frame(&desc((80, 24), &[RectPx::new(-10, -10, 200, 200)]));
        assert_eq!(plan.mode, FrameMode::Full);
    }

    #[test]
    fn partial_plan_clips_and_drops_outside() {
        let plan = plan_frame(&desc(
            (100, 100),
            &[RectPx::new(-5, -5, 20, 20), RectPx::new(500, 500, 4, 4)],
        ));
        assert_eq!(plan.mode, FrameMode::Partial);
        assert_eq!(plan.dirty_rects, vec![RectPx::new(0, 0, 15, 15)]);
    }

    #[test]
    fn touching_regions_coalesce_into_runs() {
        let plan = plan_frame(&desc(
            (800, 600),
            &[
                RectPx::new(0, 0, 10, 10),
                RectPx::new(5, 5, 10, 10),
                RectPx::new(30, 0, 5, 5),
            ],
        ));
        assert_eq!(plan.mode, FrameMode::Partial);
        assert_eq!(
            plan.dirty_rects,
            vec![RectPx::new(0, 0, 15, 15), RectPx::new(30, 0, 5, 5)]
        );
    }

    #[test]
    fn input_order_does_not_matter() {
        let a = plan_frame(&desc(
            (800, 600),
            &[RectPx::new(50, 50, 5, 5), RectPx::new(0, 0, 5, 5)],
        ));
        let b = plan_frame(&desc(
            (800, 600),
            &[RectPx::new(0, 0, 5, 5), RectPx::new(50, 50, 5, 5)],
        ));
        assert_eq!(a, b);
        assert_eq!(
            a.dirty_rects,
            vec![RectPx::new(0, 0, 5, 5), RectPx::new(50, 50, 5, 5)]
        );
    }

    #[test]
    fn region_cap_degrades_to_full() {
        let regions: Vec<RectPx> = (0..MAX_FRAME_REGIONS)
            .map(|i| RectPx::new(i as i32 * 2, 0, 1, 1))
            .collect();
        let plan = plan_frame(&desc((4096, 16), &regions));
        assert_eq!(plan.mode, FrameMode::Full);
        assert_eq!(
            plan.dirty_rects,
            vec![RectPx::full(&ExtentPx::new(4096, 16))]
        );
    }

    #[test]
    fn just_under_cap_stays_partial() {
        let regions: Vec<RectPx> = (0..MAX_FRAME_REGIONS - 1)
            .map(|i| RectPx::new(i as i32 * 3, 0, 1, 1))
            .collect();
        let plan = plan_frame(&desc((4096, 16), &regions));
        assert_eq!(plan.mode, FrameMode::Partial);
        assert_eq!(plan.dirty_rects.len(), MAX_FRAME_REGIONS - 1);
    }

    #[test]
    fn all_outside_extent_is_clean() {
        let plan = plan_frame(&desc((10, 10), &[RectPx::new(20, 20, 3, 3)]));
        assert_eq!(plan.mode, FrameMode::Clean);
    }

    #[test]
    fn works_through_trait_object() {
        let d = desc((32, 32), &[RectPx::new(0, 0, 8, 8)]);
        let dynamic: &dyn DamageDescriptor = &d;
        let plan = plan_frame(dynamic);
        assert_eq!(plan.mode, FrameMode::Partial);
        assert_eq!(plan.dirty_rects, vec![RectPx::new(0, 0, 8, 8)]);
    }
}
