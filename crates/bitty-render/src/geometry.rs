//! Pixel-domain rectangle and extent algebra used by frame planning.
//!
//! All arithmetic is overflow-safe: coordinates combine through `i64`
//! intermediates with saturating conversion back to the field types, so no
//! input can panic or wrap. Clipping only ever shrinks rectangles, which
//! keeps the damage invariant "over-damage is safe; under-damage is
//! impossible" (mirroring the `bitty-term-state` damage model).

/// Logical pixel extent of a render target (width x height).
///
/// Both fields are plain `u32`; an all-zero extent describes a not-yet-sized
/// target, for which [`crate::frame::plan_frame`] yields a clean plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ExtentPx {
    /// Width in logical pixels.
    pub width: u32,
    /// Height in logical pixels.
    pub height: u32,
}

impl ExtentPx {
    /// Builds an extent from width and height.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// True when either dimension is zero (nothing can be drawn).
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// True when the rectangle lies fully inside this extent (zero-area
    /// rectangles at the origin edge count as inside).
    #[must_use]
    pub fn contains_rect(&self, rect: &RectPx) -> bool {
        rect.x >= 0
            && rect.y >= 0
            && rect.right_exclusive() <= i64::from(self.width)
            && rect.bottom_exclusive() <= i64::from(self.height)
    }
}

/// Axis-aligned rectangle in logical pixels with a top-left origin.
///
/// `x`/`y` are inclusive offsets and may be negative (glyph bitmaps carry
/// negative bearings); `width`/`height` are unsigned spans. A rectangle with
/// a zero span is *empty*: it intersects nothing and clips away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RectPx {
    /// Left edge offset; may be negative.
    pub x: i32,
    /// Top edge offset; may be negative.
    pub y: i32,
    /// Horizontal span in pixels.
    pub width: u32,
    /// Vertical span in pixels.
    pub height: u32,
}

impl RectPx {
    /// Builds a rectangle from an offset and a span.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The full-extent rectangle for a target of the given size.
    #[must_use]
    pub fn full(extent: &ExtentPx) -> Self {
        Self {
            x: 0,
            y: 0,
            width: extent.width,
            height: extent.height,
        }
    }

    /// Exclusive right edge as `i64` (`x + width`, overflow-free).
    #[must_use]
    pub fn right_exclusive(&self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    /// Exclusive bottom edge as `i64` (`y + height`, overflow-free).
    #[must_use]
    pub fn bottom_exclusive(&self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    /// True when either span is zero. Empty rectangles never intersect.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Area in pixels as `u64` (cannot overflow: at most
    /// `(2^32 - 1)^2 < 2^64`). Widening casts are lossless here and
    /// const-stable, unlike trait-based conversions.
    #[must_use]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Intersection with `other` by positive-area overlap; `None` when the
    /// rectangles are disjoint or either is empty.
    #[must_use]
    pub fn intersection(&self, other: &RectPx) -> Option<RectPx> {
        let left = i64::from(self.x).max(i64::from(other.x));
        let top = i64::from(self.y).max(i64::from(other.y));
        let right = self.right_exclusive().min(other.right_exclusive());
        let bottom = self.bottom_exclusive().min(other.bottom_exclusive());
        if right <= left || bottom <= top {
            return None;
        }
        // An overlap can never exceed either input's span, so both spans fit
        // `u32`; a failure would be an internal invariant break.
        Some(RectPx {
            x: left.saturating_try_into_i32(),
            y: top.saturating_try_into_i32(),
            width: u32::try_from(right - left).ok()?,
            height: u32::try_from(bottom - top).ok()?,
        })
    }

    /// Clips this rectangle to `extent`. Returns `None` when nothing survives
    /// (fully outside, empty, or a zero extent). Never enlarges.
    #[must_use]
    pub fn clip_to_extent(&self, extent: &ExtentPx) -> Option<RectPx> {
        if extent.is_zero() || self.is_empty() {
            return None;
        }
        let full = RectPx::full(extent);
        // `full` has positive area for non-zero extents, so a surviving
        // intersection is always positive-area as well.
        self.intersection(&full)
    }

    /// Smallest rectangle covering both inputs. Always defined; empty inputs
    /// simply do not extend the bound beyond the other input's edges. If the
    /// union span exceeds `u32` (only reachable with extreme inputs), the
    /// span saturates to `u32::MAX`, which over-covers rather than
    /// under-covers — safe for damage semantics.
    #[must_use]
    pub fn union_bounding(a: &RectPx, b: &RectPx) -> RectPx {
        let x = i64::from(a.x).min(i64::from(b.x));
        let y = i64::from(a.y).min(i64::from(b.y));
        let right = a.right_exclusive().max(b.right_exclusive());
        let bottom = a.bottom_exclusive().max(b.bottom_exclusive());
        RectPx {
            x: x.saturating_try_into_i32(),
            y: y.saturating_try_into_i32(),
            width: saturating_u32(right - x),
            height: saturating_u32(bottom - y),
        }
    }

    /// True when this rectangle covers every drawable pixel of `extent`.
    #[must_use]
    pub fn covers_extent(&self, extent: &ExtentPx) -> bool {
        if extent.is_zero() {
            return false;
        }
        self.x <= 0
            && self.y <= 0
            && self.right_exclusive() >= i64::from(extent.width)
            && self.bottom_exclusive() >= i64::from(extent.height)
    }

    /// True when `other` lies entirely within `self` (empty `other` is
    /// vacuously contained when its edges fall on or inside `self`'s bounds).
    #[must_use]
    pub fn contains_rect(&self, other: &RectPx) -> bool {
        if other.is_empty() {
            return true;
        }
        i64::from(self.x) <= i64::from(other.x)
            && i64::from(self.y) <= i64::from(other.y)
            && self.right_exclusive() >= other.right_exclusive()
            && self.bottom_exclusive() >= other.bottom_exclusive()
    }
}

trait SaturatingI32 {
    fn saturating_try_into_i32(self) -> i32;
}

impl SaturatingI32 for i64 {
    fn saturating_try_into_i32(self) -> i32 {
        i32::try_from(self).unwrap_or(if self < 0 { i32::MIN } else { i32::MAX })
    }
}

/// Saturates a non-negative `i64` span into `u32`, clamping upward on
/// overflow (over-cover, never under-cover).
fn saturating_u32(span: i64) -> u32 {
    u32::try_from(span).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_of_overlapping_and_disjoint() {
        let a = RectPx::new(0, 0, 10, 10);
        let b = RectPx::new(5, 5, 10, 10);
        assert_eq!(a.intersection(&b), Some(RectPx::new(5, 5, 5, 5)));

        let c = RectPx::new(20, 20, 5, 5);
        assert_eq!(a.intersection(&c), None);

        // Touching-only edges share no positive area.
        let d = RectPx::new(10, 0, 5, 5);
        assert_eq!(a.intersection(&d), None);
    }

    #[test]
    fn intersection_with_negative_coordinates_is_exact() {
        let a = RectPx::new(-10, -10, 15, 15); // right/bottom at 5,5
        let b = RectPx::new(-5, -5, 30, 30);
        assert_eq!(a.intersection(&b), Some(RectPx::new(-5, -5, 10, 10)));
    }

    #[test]
    fn empty_rectangles_never_intersect() {
        let empty = RectPx::new(3, 3, 0, 8);
        let solid = RectPx::new(0, 0, 100, 100);
        assert_eq!(empty.intersection(&solid), None);
        assert!(empty.is_empty());
    }

    #[test]
    fn clip_to_extent_shrinks_and_drops_outsiders() {
        let extent = ExtentPx::new(100, 50);
        assert_eq!(
            RectPx::new(-5, 40, 20, 20).clip_to_extent(&extent),
            Some(RectPx::new(0, 40, 15, 10))
        );
        assert_eq!(RectPx::new(200, 200, 4, 4).clip_to_extent(&extent), None);
        assert_eq!(RectPx::new(0, 0, 0, 4).clip_to_extent(&extent), None);
        assert_eq!(
            RectPx::new(0, 0, 200, 200).clip_to_extent(&extent),
            Some(RectPx::new(0, 0, 100, 50))
        );
        // Zero extent clips everything away.
        assert_eq!(
            RectPx::new(0, 0, 1, 1).clip_to_extent(&ExtentPx::new(0, 9)),
            None
        );
    }

    #[test]
    fn union_bounding_covers_both_inputs() {
        let a = RectPx::new(-5, 0, 10, 10);
        let b = RectPx::new(20, 30, 5, 5);
        assert_eq!(RectPx::union_bounding(&a, &b), RectPx::new(-5, 0, 30, 35));
    }

    #[test]
    fn coverage_and_containment() {
        let extent = ExtentPx::new(80, 25);
        assert!(RectPx::full(&extent).covers_extent(&extent));
        assert!(RectPx::new(-1, -1, 90, 90).covers_extent(&extent));
        assert!(!RectPx::new(0, 0, 79, 25).covers_extent(&extent));
        assert!(!RectPx::new(0, 0, 80, 80).covers_extent(&ExtentPx::new(0, 0)));

        let outer = RectPx::new(0, 0, 10, 10);
        assert!(outer.contains_rect(&RectPx::new(2, 2, 3, 3)));
        assert!(!outer.contains_rect(&RectPx::new(9, 0, 2, 2)));
        assert!(outer.contains_rect(&RectPx::new(5, 5, 0, 0)));
    }

    #[test]
    fn extremes_do_not_panic_or_wrap() {
        let big = RectPx::new(i32::MAX - 1, i32::MIN / 2, u32::MAX, u32::MAX);
        let other = RectPx::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX);
        let _ = big.intersection(&other);
        let _ = RectPx::union_bounding(&big, &other);
        let _ = big.clip_to_extent(&ExtentPx::new(u32::MAX, u32::MAX));
        // `big` starts at x = i32::MAX - 1 but its top edge sits at
        // i32::MIN / 2, so its bottom edge cannot reach u32::MAX.
        assert!(!big.covers_extent(&ExtentPx::new(u32::MAX, u32::MAX)));
        // The exact full-extent rectangle always covers its own extent.
        let max_extent = ExtentPx::new(u32::MAX, u32::MAX);
        assert!(RectPx::full(&max_extent).covers_extent(&max_extent));
    }

    #[test]
    fn area_matches_span_product() {
        assert_eq!(RectPx::new(0, 0, 3, 7).area(), 21);
        assert_eq!(RectPx::new(0, 0, 0, 7).area(), 0);
        assert_eq!(
            RectPx::new(0, 0, u32::MAX, u32::MAX).area(),
            u64::from(u32::MAX) * u64::from(u32::MAX)
        );
    }
}
