//! Core-owned workspace decoration (CTX-0292; accepted spec CTX-0118).
//!
//! The accepted workspace-compositor specification owns `gaps_in`, `gaps_out`,
//! `border`, and `radius` in Core, in **logical pixels**, with the defaults
//! `4 / 6 / 2 / 6` and the ranges `0..=32 / 0..=32 / 0..=8 / 0..=16`. Decoration
//! is never part of a `LayoutTree`, never proposed by a `LayoutProvider`, and
//! never carried by a `View`; it is a parameter of the Core composition step,
//! so no plugin mutation path exists.
//!
//! [`LayoutNode::layout_with_decoration`] applies the contract after the tree
//! shape is known: `gaps_out` insets the workspace area, `gaps_in` reserves a
//! band between siblings at every `Split`, `border` is drawn inside the View
//! frame (the content rect is the frame inset by the border), and `radius` is
//! carried as clip metadata for the frame. The solver is total and
//! deterministic: identical trees, bounds, and decoration always produce
//! identical frames, and oversized decoration saturates instead of panicking.
//!
//! The existing cell-unit [`Gaps`] path (CTX-0177) is untouched; decoration is
//! a separate px surface. Units are the caller's: the same integer algebra is
//! valid for logical pixels (this module) and cells (CTX-0177).

use crate::geometry::{Gaps, Rect};
use crate::layout::LayoutNode;
use crate::view::ViewId;

/// Default inner gap (`gaps_in`) in logical pixels (accepted spec CTX-0118).
pub const DEFAULT_GAPS_IN_PX: u16 = 4;

/// Default outer gap (`gaps_out`) in logical pixels (accepted spec CTX-0118).
pub const DEFAULT_GAPS_OUT_PX: u16 = 6;

/// Default border thickness in logical pixels (accepted spec CTX-0118).
pub const DEFAULT_BORDER_PX: u16 = 2;

/// Default View frame corner radius in logical pixels (accepted spec CTX-0118).
pub const DEFAULT_RADIUS_PX: u16 = 6;

/// Maximum `gaps_in`/`gaps_out` in logical pixels (accepted spec CTX-0118).
pub const MAX_GAP_PX: u16 = 32;

/// Maximum `border` thickness in logical pixels (accepted spec CTX-0118).
pub const MAX_BORDER_PX: u16 = 8;

/// Maximum View frame `radius` in logical pixels (accepted spec CTX-0118).
pub const MAX_RADIUS_PX: u16 = 16;

/// Core-owned View/Workspace decoration in logical pixels.
///
/// Values are integers in logical pixels; DPI scaling happens only at render
/// time. [`Self::validate`] fails closed on out-of-range values; construction
/// through [`LayoutNode::layout_with_decoration`] never falls back silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decoration {
    /// Gap between adjacent views inside one workspace, logical px.
    pub gaps_in: u16,
    /// Gap between the workspace tiling area and the window edge, logical px.
    pub gaps_out: u16,
    /// Border thickness drawn inside each View frame, logical px.
    pub border: u16,
    /// Corner radius for View frames, logical px.
    pub radius: u16,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            gaps_in: DEFAULT_GAPS_IN_PX,
            gaps_out: DEFAULT_GAPS_OUT_PX,
            border: DEFAULT_BORDER_PX,
            radius: DEFAULT_RADIUS_PX,
        }
    }
}

impl Decoration {
    /// No decoration at all; bit-identical to the undecorated solver.
    pub const ZERO: Self = Self {
        gaps_in: 0,
        gaps_out: 0,
        border: 0,
        radius: 0,
    };

    /// Safe-mode decoration (`bitty --safe`): `0/0/1/0` regardless of user
    /// configuration (accepted spec CTX-0118 rule 5).
    pub const SAFE: Self = Self {
        gaps_in: 0,
        gaps_out: 0,
        border: 1,
        radius: 0,
    };

    /// Creates decoration from the four logical-pixel values.
    #[must_use]
    pub const fn new(gaps_in: u16, gaps_out: u16, border: u16, radius: u16) -> Self {
        Self {
            gaps_in,
            gaps_out,
            border,
            radius,
        }
    }

    /// Validates the accepted ranges, failing closed on the first violation.
    ///
    /// # Errors
    ///
    /// [`DecorationError`] naming the out-of-range property.
    pub fn validate(&self) -> Result<(), DecorationError> {
        if self.gaps_in > MAX_GAP_PX {
            return Err(DecorationError::GapsIn(self.gaps_in));
        }
        if self.gaps_out > MAX_GAP_PX {
            return Err(DecorationError::GapsOut(self.gaps_out));
        }
        if self.border > MAX_BORDER_PX {
            return Err(DecorationError::Border(self.border));
        }
        if self.radius > MAX_RADIUS_PX {
            return Err(DecorationError::Radius(self.radius));
        }
        Ok(())
    }

    /// True when every property is zero (undecorated fast path).
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.gaps_in == 0 && self.gaps_out == 0 && self.border == 0 && self.radius == 0
    }

    /// The gap component as the shared integer-algebra [`Gaps`] type.
    ///
    /// Units follow the caller: this module's solver treats them as logical
    /// pixels, the CTX-0177 path as cells.
    #[must_use]
    pub const fn gaps(self) -> Gaps {
        Gaps::new(self.gaps_in, self.gaps_out)
    }
}

impl std::fmt::Display for Decoration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gaps_in={} gaps_out={} border={} radius={}",
            self.gaps_in, self.gaps_out, self.border, self.radius
        )
    }
}

/// Fail-closed validation error for [`Decoration`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationError {
    /// `gaps_in` outside `0..=32`.
    GapsIn(u16),
    /// `gaps_out` outside `0..=32`.
    GapsOut(u16),
    /// `border` outside `0..=8`.
    Border(u16),
    /// `radius` outside `0..=16`.
    Radius(u16),
}

impl std::fmt::Display for DecorationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GapsIn(v) => {
                write!(f, "decoration.gaps_in {v} is outside [0, {MAX_GAP_PX}]")
            }
            Self::GapsOut(v) => {
                write!(f, "decoration.gaps_out {v} is outside [0, {MAX_GAP_PX}]")
            }
            Self::Border(v) => {
                write!(f, "decoration.border {v} is outside [0, {MAX_BORDER_PX}]")
            }
            Self::Radius(v) => {
                write!(f, "decoration.radius {v} is outside [0, {MAX_RADIUS_PX}]")
            }
        }
    }
}

impl std::error::Error for DecorationError {}

/// One decorated View frame produced by
/// [`LayoutNode::layout_with_decoration`].
///
/// `frame` is the View's rectangle after gaps_out/gaps_in; `content` is the
/// rectangle inside the frame after the border inset (equal to `frame` when
/// `border` is zero); `radius` is the frame's corner radius clip metadata.
/// Hit testing uses `frame` (`content` only shifts painting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoratedView {
    /// View frame after gaps and before the border inset, logical px.
    pub frame: Rect,
    /// Content rectangle inside the border inset, logical px.
    pub content: Rect,
    /// Border thickness applied between `frame` and `content`, logical px.
    pub border: u16,
    /// Frame corner radius for clipping, logical px (no hit-test effect
    /// beyond `frame`).
    pub radius: u16,
}

impl LayoutNode {
    /// Decorated composition: the accepted Core-owned decoration application.
    ///
    /// Walks `self` exactly like [`LayoutNode::layout`], but in logical
    /// pixels and with the accepted decorations:
    ///
    /// 1. `decoration.gaps_out` insets the whole workspace area (once).
    /// 2. `decoration.gaps_in` reserves a band between siblings at every
    ///    `Split`; the ratio applies to the gap-subtracted space, so a 50/50
    ///    split stays symmetric.
    /// 3. `decoration.border` is drawn inside each View frame; `content` is
    ///    the frame inset by the border on every side.
    /// 4. `decoration.radius` is carried for frame clipping.
    ///
    /// `Stack` children share the (gapped) bounds, so inner gaps never appear
    /// inside a stack; `Overlay` follows the base/overlay behavior of
    /// [`LayoutNode::layout_with_gaps`] with the overlay clipped to the
    /// inset area. With [`Decoration::ZERO`] the frames are bit-identical to
    /// [`LayoutNode::layout`] and `content == frame`.
    ///
    /// Total and deterministic: oversized gaps or borders saturate to
    /// zero-size rects, never panic.
    #[must_use]
    pub fn layout_with_decoration(
        &self,
        bounds: Rect,
        decoration: Decoration,
    ) -> Vec<(ViewId, DecoratedView)> {
        let area = if decoration.gaps_out == 0 {
            bounds
        } else {
            decoration.gaps().inset_outer(bounds)
        };
        let mut out = Vec::new();
        self.layout_decorated_inner(area, decoration, &mut out);
        out
    }

    fn layout_decorated_inner(
        &self,
        bounds: Rect,
        decoration: Decoration,
        out: &mut Vec<(ViewId, DecoratedView)>,
    ) {
        match self {
            Self::Leaf(v) => {
                let frame = bounds;
                out.push((
                    v.id(),
                    DecoratedView {
                        frame,
                        content: inset_rect(frame, decoration.border),
                        border: decoration.border,
                        radius: decoration.radius,
                    },
                ));
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) =
                    crate::layout::split_rect_with_gap(bounds, *axis, *ratio, decoration.gaps_in);
                first.layout_decorated_inner(a, decoration, out);
                second.layout_decorated_inner(b, decoration, out);
            }
            Self::Stack(children) => {
                for child in children {
                    child.layout_decorated_inner(bounds, decoration, out);
                }
            }
            Self::Overlay {
                base,
                overlay,
                bounds: overlay_bounds,
                ..
            } => {
                base.layout_decorated_inner(bounds, decoration, out);
                let clipped = if let Some(inter) = overlay_bounds.clip_to(bounds) {
                    inter
                } else {
                    Rect::zero()
                };
                overlay.layout_decorated_inner(clipped, decoration, out);
            }
        }
    }
}

/// Insets `rect` by `amount` on all four sides, saturating to zero size.
fn inset_rect(rect: Rect, amount: u16) -> Rect {
    if amount == 0 {
        return rect;
    }
    Rect::new(
        rect.x.saturating_add(amount),
        rect.y.saturating_add(amount),
        rect.width.saturating_sub(amount.saturating_mul(2)),
        rect.height.saturating_sub(amount.saturating_mul(2)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::SplitAxis;
    use crate::view::View;

    fn view(id: u64, cols: usize, rows: usize) -> View {
        View::new(ViewId::new(id), cols, rows)
    }

    fn split(ratio: f32, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, ratio, first, second)
    }

    #[test]
    fn defaults_match_accepted_spec() {
        let d = Decoration::default();
        assert_eq!(d.gaps_in, 4);
        assert_eq!(d.gaps_out, 6);
        assert_eq!(d.border, 2);
        assert_eq!(d.radius, 6);
    }

    #[test]
    fn safe_mode_is_zero_zero_one_zero() {
        assert_eq!(Decoration::SAFE.gaps_in, 0);
        assert_eq!(Decoration::SAFE.gaps_out, 0);
        assert_eq!(Decoration::SAFE.border, 1);
        assert_eq!(Decoration::SAFE.radius, 0);
        assert!(Decoration::SAFE.validate().is_ok());
    }

    #[test]
    fn validate_bounds_fail_closed() {
        assert!(Decoration::new(32, 32, 8, 16).validate().is_ok());
        assert_eq!(
            Decoration::new(33, 0, 0, 0).validate(),
            Err(DecorationError::GapsIn(33))
        );
        assert_eq!(
            Decoration::new(0, 33, 0, 0).validate(),
            Err(DecorationError::GapsOut(33))
        );
        assert_eq!(
            Decoration::new(0, 0, 9, 0).validate(),
            Err(DecorationError::Border(9))
        );
        assert_eq!(
            Decoration::new(0, 0, 0, 17).validate(),
            Err(DecorationError::Radius(17))
        );
    }

    #[test]
    fn zero_decoration_matches_plain_layout() {
        let node = split(
            0.6,
            split(
                0.5,
                LayoutNode::leaf(view(1, 4, 2)),
                LayoutNode::leaf(view(2, 4, 2)),
            ),
            LayoutNode::leaf(view(3, 4, 2)),
        );
        let bounds = Rect::new(0, 0, 80, 24);
        let plain = node.layout(bounds);
        let decorated = node.layout_with_decoration(bounds, Decoration::ZERO);
        assert_eq!(plain.len(), decorated.len());
        for ((pid, prect), (did, dview)) in plain.iter().zip(decorated.iter()) {
            assert_eq!(pid, did);
            assert_eq!(*prect, dview.frame);
            assert_eq!(dview.frame, dview.content);
        }
    }

    #[test]
    fn gaps_out_insets_workspace_area_px() {
        let node = LayoutNode::leaf(view(1, 4, 2));
        let bounds = Rect::new(0, 0, 100, 50);
        let out = node.layout_with_decoration(bounds, Decoration::new(0, 6, 0, 0));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.frame, Rect::new(6, 6, 88, 38));
    }

    #[test]
    fn gaps_in_reserves_px_band_between_siblings() {
        let node = split(
            0.5,
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::leaf(view(2, 4, 2)),
        );
        let bounds = Rect::new(0, 0, 100, 20);
        let out = node.layout_with_decoration(bounds, Decoration::new(10, 0, 0, 0));
        assert_eq!(out[0].1.frame, Rect::new(0, 0, 45, 20));
        assert_eq!(out[1].1.frame, Rect::new(55, 0, 45, 20));
        // first + gap + second spans the bounds exactly.
        let (a, b) = (out[0].1.frame, out[1].1.frame);
        assert_eq!(a.right() + 10 + u32::from(b.width), bounds.right());
    }

    #[test]
    fn border_insets_content_inside_frame() {
        let node = LayoutNode::leaf(view(1, 4, 2));
        let bounds = Rect::new(0, 0, 40, 20);
        let out = node.layout_with_decoration(bounds, Decoration::new(0, 0, 2, 6));
        assert_eq!(out[0].1.frame, bounds);
        assert_eq!(out[0].1.content, Rect::new(2, 2, 36, 16));
        assert_eq!(out[0].1.border, 2);
        assert_eq!(out[0].1.radius, 6);
    }

    #[test]
    fn stacked_children_share_the_gapped_bounds() {
        let node = LayoutNode::stack(vec![
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::leaf(view(2, 4, 2)),
        ]);
        let bounds = Rect::new(0, 0, 30, 12);
        let out = node.layout_with_decoration(bounds, Decoration::new(4, 2, 0, 0));
        assert_eq!(out[0].1.frame, out[1].1.frame);
        assert_eq!(out[0].1.frame, Rect::new(2, 2, 26, 8));
    }

    #[test]
    fn oversized_decoration_saturates_without_panic() {
        let node = split(
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let out =
            node.layout_with_decoration(Rect::new(0, 0, 3, 3), Decoration::new(32, 32, 8, 16));
        assert_eq!(out.len(), 2);
        for (_, dview) in &out {
            assert!(dview.frame.width == 0 || dview.frame.width <= 3);
            assert!(dview.content.width <= dview.frame.width);
        }
    }

    #[test]
    fn decoration_is_deterministic() {
        let node = split(
            0.37,
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::stack(vec![
                LayoutNode::leaf(view(2, 4, 2)),
                LayoutNode::leaf(view(3, 4, 2)),
            ]),
        );
        let bounds = Rect::new(1, 2, 101, 51);
        let d = Decoration::new(3, 5, 2, 6);
        assert_eq!(
            node.layout_with_decoration(bounds, d),
            node.layout_with_decoration(bounds, d)
        );
    }

    #[test]
    fn decoration_never_mutates_the_tree() {
        // Spec rule 3: decoration is Core-owned and never carried by a
        // LayoutTree leaf/proposal (`Decoration` is not a `LayoutNode` or
        // `View` field); applying it is a pure post-computation pass.
        let node = split(
            0.5,
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::leaf(view(2, 4, 2)),
        );
        let before = node.clone();
        let _ = node.layout_with_decoration(Rect::new(0, 0, 80, 24), Decoration::new(3, 5, 2, 6));
        assert_eq!(node, before, "decoration application must not mutate");
    }

    #[test]
    fn errors_display_field_paths() {
        assert!(DecorationError::GapsIn(40).to_string().contains("gaps_in"));
        assert!(
            DecorationError::GapsOut(40)
                .to_string()
                .contains("gaps_out")
        );
        assert!(DecorationError::Border(9).to_string().contains("border"));
        assert!(DecorationError::Radius(17).to_string().contains("radius"));
    }
}
