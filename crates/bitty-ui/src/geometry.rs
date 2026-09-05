//! Owned geometry primitives for the UI layout algebra.
//!
//! All types are integer cell coordinates (mirroring the terminal grid
//! domain of `bitty-term-state`). Arithmetic is saturating and deterministic;
//! no floating point is used except for split ratio storage in `layout`.

#![forbid(unsafe_code)]

/// Cell offset within a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Point {
    /// Column.
    pub x: u16,
    /// Row.
    pub y: u16,
}

impl Point {
    /// Creates a point.
    #[must_use]
    pub const fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

/// Cell extent (width x height).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Size {
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl Size {
    /// Creates a size. Zero spans are allowed (empty allocation) so the
    /// solver can stay total over all inputs.
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// True when either dimension is zero.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Axis-aligned rectangle in cell coordinates, top-left origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl Rect {
    /// Builds a rectangle.
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Zero rectangle.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    /// Right exclusive edge as u32 to avoid overflow.
    #[must_use]
    pub const fn right(self) -> u32 {
        self.x as u32 + self.width as u32
    }

    /// Bottom exclusive edge as u32.
    #[must_use]
    pub const fn bottom(self) -> u32 {
        self.y as u32 + self.height as u32
    }

    /// True when width or height is zero.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Returns true when `other` lies fully inside `self` (empty `other`
    /// vacuously true when edges inside bounds).
    #[must_use]
    pub fn contains(self, other: Rect) -> bool {
        if other.is_empty() {
            return other.x >= self.x
                && other.y >= self.y
                && (other.x as u32) <= self.right()
                && (other.y as u32) <= self.bottom();
        }
        self.x <= other.x
            && self.y <= other.y
            && self.right() >= other.right()
            && self.bottom() >= other.bottom()
    }

    /// Intersection by positive area overlap; `None` when disjoint or empty.
    #[must_use]
    pub fn intersection(self, other: Rect) -> Option<Rect> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = (self.right().min(other.right())) as u16;
        let y2 = (self.bottom().min(other.bottom())) as u16;
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        Some(Rect {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        })
    }

    /// Whether this rect overlaps or touches `other` on both axes (used for
    /// split adjacency detection).
    #[must_use]
    pub fn touches_or_overlaps(self, other: Rect) -> bool {
        self.right() >= other.x as u32
            && other.right() >= self.x as u32
            && self.bottom() >= other.y as u32
            && other.bottom() >= self.y as u32
    }

    /// Clip to `bounds` by intersection, returning `None` when outside.
    #[must_use]
    pub fn clip_to(self, bounds: Rect) -> Option<Rect> {
        self.intersection(bounds)
    }
}

/// Split axis for `LayoutNode::Split`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitAxis {
    /// Left / right.
    Horizontal,
    /// Top / bottom.
    Vertical,
}

/// Hyprland-like panel gaps in cell units (CTX-0177).
///
/// - `inner` (`gaps_in`): spacing between sibling panes at every `Split`,
///   in cells. The gap band shows the window background; no leaf paints it.
/// - `outer` (`gaps_out`): inset between the container edge and all leaves,
///   in cells.
///
/// Cells (not pixels): the layout algebra is integer cell-space and glyphs
/// must sit on the cell grid, so a pixel gap would quantize to cells anyway.
/// Hit-testing converts via the live cell metrics (`outer`/`inner` cells
/// times cell px). All arithmetic is saturating; oversized gaps collapse
/// leaves to zero-size (total solver) rather than panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Gaps {
    /// Spacing between sibling panes (`gaps_in`), in cells.
    pub inner: u16,
    /// Inset around the container edge (`gaps_out`), in cells.
    pub outer: u16,
}

impl Gaps {
    /// Zero gaps: edge-to-edge tiling (the pre-CTX-0177 behavior).
    pub const ZERO: Self = Self { inner: 0, outer: 0 };

    /// Creates gaps in cells.
    #[must_use]
    pub const fn new(inner: u16, outer: u16) -> Self {
        Self { inner, outer }
    }

    /// True when both gaps are zero (gapless fast path, bit-identical to the
    /// legacy solver).
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.inner == 0 && self.outer == 0
    }

    /// Insets `bounds` by `outer` on all four sides, saturating. When the
    /// inset exceeds the bounds, the result is a zero-size rect at the
    /// clamped origin (total, never panics).
    #[must_use]
    pub fn inset_outer(self, bounds: Rect) -> Rect {
        if self.outer == 0 {
            return bounds;
        }
        let o = self.outer;
        let x = bounds.x.saturating_add(o);
        let y = bounds.y.saturating_add(o);
        let width = bounds.width.saturating_sub(o.saturating_mul(2));
        let height = bounds.height.saturating_sub(o.saturating_mul(2));
        Rect::new(x, y, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains_and_intersection() {
        let outer = Rect::new(0, 0, 10, 10);
        let inner = Rect::new(2, 2, 3, 3);
        assert!(outer.contains(inner));
        assert_eq!(outer.intersection(inner), Some(Rect::new(2, 2, 3, 3)));
        let disjoint = Rect::new(20, 20, 2, 2);
        assert_eq!(outer.intersection(disjoint), None);
        let touching = Rect::new(10, 0, 2, 2);
        assert_eq!(outer.intersection(touching), None);
    }

    #[test]
    fn empty_handling() {
        let r = Rect::new(0, 0, 0, 5);
        assert!(r.is_empty());
        assert_eq!(r.intersection(Rect::new(0, 0, 5, 5)), None);
        assert!(Rect::new(0, 0, 5, 5).contains(Rect::new(1, 1, 0, 0)));
    }

    #[test]
    fn clip_to() {
        let bounds = Rect::new(0, 0, 10, 10);
        let r = Rect::new(8, 8, 5, 5);
        assert_eq!(r.clip_to(bounds), Some(Rect::new(8, 8, 2, 2)));
        assert_eq!(Rect::new(20, 20, 2, 2).clip_to(bounds), None);
    }

    #[test]
    fn gaps_zero_is_identity() {
        // CTX-0177: zero gaps preserve edge-to-edge tiling exactly.
        assert!(Gaps::ZERO.is_zero());
        assert!(!Gaps::new(1, 0).is_zero());
        let bounds = Rect::new(0, 0, 80, 24);
        assert_eq!(Gaps::ZERO.inset_outer(bounds), bounds);
    }

    #[test]
    fn gaps_outer_insets_all_sides() {
        // CTX-0177: gaps_out shrinks the container symmetrically.
        let gaps = Gaps::new(0, 2);
        assert_eq!(
            gaps.inset_outer(Rect::new(0, 0, 80, 24)),
            Rect::new(2, 2, 76, 20)
        );
        // Offset containers shift the origin too.
        assert_eq!(
            gaps.inset_outer(Rect::new(10, 10, 80, 24)),
            Rect::new(12, 12, 76, 20)
        );
    }

    #[test]
    fn gaps_outer_oversized_collapses_total() {
        // CTX-0177: an outer gap larger than the container saturates to a
        // zero-size rect instead of panicking.
        let gaps = Gaps::new(0, 50);
        let inset = gaps.inset_outer(Rect::new(0, 0, 80, 24));
        assert!(inset.is_empty());
        assert_eq!(inset.width, 0);
        assert_eq!(inset.height, 0);
    }
}
