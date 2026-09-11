//! Core-owned workspace decoration (CTX-0292; unified CTX-0333).
//!
//! The Core-owned workspace-compositor specification owns `gaps_in`,
//! `gaps_out`, `border`, `radius`, and `content_inset` in **logical pixels**.
//! CTX-0333 unified the sibling/container gap defaults at `6 / 6` (the older
//! accepted spec had `gaps_in = 4`, which read as too tight next to a
//! terminal) and added `content_inset` (inner padding between the border and
//! the painted content). Current defaults are `6 / 6 / 2 / 6 / 6` with the
//! ranges `0..=32 / 0..=32 / 0..=8 / 0..=16 / 0..=32`. Decoration is never
//! part of a `LayoutTree`, never proposed by a `LayoutProvider`, and never
//! carried by a `View`; it is a parameter of the Core composition step, so no
//! plugin mutation path exists.
//!
//! [`LayoutNode::layout_with_decoration`] applies the contract after the tree
//! shape is known: `gaps_out` insets the workspace area, `gaps_in` reserves a
//! band between siblings at every `Split`, `border` and `content_inset` are
//! drawn inside the View frame (the content rect is the frame inset by
//! `border + content_inset`), and `radius` is carried as clip metadata for the
//! frame. `content_inset` and `gaps_out` share a default so a sibling gap and
//! a container gap read as the same spacing out of the box. The solver is
//! total and deterministic: identical trees, bounds, and decoration always
//! produce identical frames, and oversized decoration saturates instead of
//! panicking.
//!
//! The existing cell-unit [`Gaps`] path (CTX-0177) is untouched; decoration is
//! a separate px surface. Units are the caller's: the same integer algebra is
//! valid for logical pixels (this module) and cells (CTX-0177).

use crate::geometry::{Gaps, Rect};
use crate::layout::LayoutNode;
use crate::view::ViewId;

/// Default inner gap (`gaps_in`) in logical pixels.
///
/// CTX-0333 unified this with [`DEFAULT_GAPS_OUT_PX`] (both `6`) so a
/// panel-to-panel sibling gap and a panel-to-terminal/container gap read as
/// one coherent spacing by default.
pub const DEFAULT_GAPS_IN_PX: u16 = 6;

/// Default outer gap (`gaps_out`) in logical pixels.
pub const DEFAULT_GAPS_OUT_PX: u16 = 6;

/// Default border thickness in logical pixels (CTX-0292).
pub const DEFAULT_BORDER_PX: u16 = 2;

/// Default View frame corner radius in logical pixels (CTX-0292).
pub const DEFAULT_RADIUS_PX: u16 = 6;

/// Default content inset in logical pixels (CTX-0333).
///
/// Inner padding between the frame border and the painted content on every
/// side, so text never sits flush against the panel margin line. Shares the
/// unified `6` spacing unit with the gap defaults.
pub const DEFAULT_CONTENT_INSET_PX: u16 = 6;

/// Maximum `gaps_in`/`gaps_out` in logical pixels (CTX-0292).
pub const MAX_GAP_PX: u16 = 32;

/// Maximum `border` thickness in logical pixels (CTX-0292).
pub const MAX_BORDER_PX: u16 = 8;

/// Maximum View frame `radius` in logical pixels (CTX-0292).
pub const MAX_RADIUS_PX: u16 = 16;

/// Maximum `content_inset` in logical pixels (CTX-0333).
pub const MAX_CONTENT_INSET_PX: u16 = 32;

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
    /// Inner padding between the border and the painted content, logical px
    /// (CTX-0333). Applied on every side; `0` reproduces the pre-CTX-0333
    /// border-only content geometry.
    pub content_inset: u16,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            gaps_in: DEFAULT_GAPS_IN_PX,
            gaps_out: DEFAULT_GAPS_OUT_PX,
            border: DEFAULT_BORDER_PX,
            radius: DEFAULT_RADIUS_PX,
            content_inset: DEFAULT_CONTENT_INSET_PX,
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
        content_inset: 0,
    };

    /// Safe-mode decoration (`bitty --safe`): `0/0/1/0/0` regardless of user
    /// configuration (accepted spec rule 5; content inset stays zero so safe
    /// mode reproduces the legacy border-only geometry).
    pub const SAFE: Self = Self {
        gaps_in: 0,
        gaps_out: 0,
        border: 1,
        radius: 0,
        content_inset: 0,
    };

    /// Creates decoration from the five logical-pixel values.
    #[must_use]
    pub const fn new(
        gaps_in: u16,
        gaps_out: u16,
        border: u16,
        radius: u16,
        content_inset: u16,
    ) -> Self {
        Self {
            gaps_in,
            gaps_out,
            border,
            radius,
            content_inset,
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
        if self.content_inset > MAX_CONTENT_INSET_PX {
            return Err(DecorationError::ContentInset(self.content_inset));
        }
        Ok(())
    }

    /// True when every property is zero (undecorated fast path).
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.gaps_in == 0
            && self.gaps_out == 0
            && self.border == 0
            && self.radius == 0
            && self.content_inset == 0
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
            "gaps_in={} gaps_out={} border={} radius={} content_inset={}",
            self.gaps_in, self.gaps_out, self.border, self.radius, self.content_inset
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
    /// `content_inset` outside `0..=32` (CTX-0333).
    ContentInset(u16),
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
            Self::ContentInset(v) => {
                write!(
                    f,
                    "decoration.content_inset {v} is outside [0, {MAX_CONTENT_INSET_PX}]"
                )
            }
        }
    }
}

impl std::error::Error for DecorationError {}

/// One decorated View frame produced by
/// [`LayoutNode::layout_with_decoration`].
///
/// `frame` is the View's rectangle after gaps_out/gaps_in; `content` is the
/// rectangle inside the frame after the `border + content_inset` inset (equal
/// to `frame` when both are zero); `radius` is the frame's corner radius clip
/// metadata. Hit testing uses `frame` (`content` only shifts painting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoratedView {
    /// View frame after gaps and before the border/content inset, logical px.
    pub frame: Rect,
    /// Content rectangle inside the `border + content_inset` inset, logical px.
    pub content: Rect,
    /// Border thickness applied between `frame` and `content`, logical px.
    pub border: u16,
    /// Frame corner radius for clipping, logical px (no hit-test effect
    /// beyond `frame`).
    pub radius: u16,
}

/// Per-axis decoration bands in one integer unit (CTX-0294).
///
/// Core-owned decoration is symmetric (`gaps_in`, `gaps_out`, `border`,
/// `radius`, `content_inset` are scalars), but the render-time composition
/// with the CTX-0177 cell gaps needs per-axis outer insets and sibling bands:
/// a cell gap of `n` cells converts to `n * cell_width` physical px
/// horizontally and `n * cell_height` vertically.
/// [`LayoutNode::layout_with_decoration`] constructs this with equal axes so
/// the solver stays bit-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bands {
    outer_x: u16,
    outer_y: u16,
    inner_x: u16,
    inner_y: u16,
    border: u16,
    content_inset: u16,
    radius: u16,
}

/// Round-half-away-from-zero conversion of a logical-px decoration value to
/// physical px at `scale`, saturating at `u16::MAX` (CTX-0294).
///
/// Zero stays zero (no 1px floor) so `Decoration::ZERO` is an exact
/// undecorated fast path at every scale.
fn scaled_px(value: u16, scale: f64) -> u16 {
    if value == 0 {
        return 0;
    }
    let scaled = f64::from(value) * scale;
    if !scaled.is_finite() || scaled <= 0.0 {
        return 0;
    }
    if scaled >= f64::from(u16::MAX) {
        u16::MAX
    } else {
        scaled.round() as u16
    }
}

/// Insets `rect` by independent horizontal/vertical amounts, saturating.
fn inset_axes(rect: Rect, x: u16, y: u16) -> Rect {
    Rect::new(
        rect.x.saturating_add(x),
        rect.y.saturating_add(y),
        rect.width.saturating_sub(x.saturating_mul(2)),
        rect.height.saturating_sub(y.saturating_mul(2)),
    )
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
    /// 3. `decoration.border` and `decoration.content_inset` are drawn inside
    ///    each View frame; `content` is the frame inset by `border +
    ///    content_inset` on every side (CTX-0333, so text never sits flush
    ///    against the frame edge).
    /// 4. `decoration.radius` is carried for frame clipping.
    ///
    /// `Stack` children share the (gapped) bounds, so inner gaps never appear
    /// inside a stack; `Overlay` follows the base/overlay behavior of
    /// [`LayoutNode::layout_with_gaps`] with the overlay clipped to the
    /// inset area. With [`Decoration::ZERO`] the frames are bit-identical to
    /// [`LayoutNode::layout`] and `content == frame`; with `content_inset = 0`
    /// the content geometry reproduces the pre-CTX-0333 border-only frames.
    ///
    /// Total and deterministic: oversized gaps, borders, or insets saturate to
    /// zero-size rects, never panic.
    #[must_use]
    pub fn layout_with_decoration(
        &self,
        bounds: Rect,
        decoration: Decoration,
    ) -> Vec<(ViewId, DecoratedView)> {
        let bands = Bands {
            outer_x: decoration.gaps_out,
            outer_y: decoration.gaps_out,
            inner_x: decoration.gaps_in,
            inner_y: decoration.gaps_in,
            border: decoration.border,
            content_inset: decoration.content_inset,
            radius: decoration.radius,
        };
        let area = inset_axes(bounds, bands.outer_x, bands.outer_y);
        let mut out = Vec::new();
        self.layout_bands_inner(area, bands, &mut out);
        out
    }

    /// Decorated composition at render scale, composed with CTX-0177 cell
    /// gaps (CTX-0294 live-present wiring).
    ///
    /// The accepted decoration stays in **logical pixels**; this solver is the
    /// render-time step the spec rule 1 allows, converting each property to
    /// **physical pixels** with the Window DPI `scale` (`round`, saturating,
    /// zero stays zero). The CTX-0177 panel gaps are cell-unit and convert
    /// with the live physical cell metrics:
    ///
    /// - outer inset axis: `scaled(gaps_out) + gaps.outer * cell_axis`;
    /// - sibling band axis: `scaled(gaps_in) + gaps.inner * cell_axis`;
    /// - content inset and radius: `scaled(...)`; the leaf content rectangle
    ///   is inset by `scaled(border) + scaled(content_inset)` (CTX-0333).
    ///
    /// With `gaps == Gaps::ZERO` and `scale == 1.0` this is bit-identical to
    /// [`Self::layout_with_decoration`] (same integer unit); with
    /// `decoration == Decoration::ZERO` it is the CTX-0177 cell-gap solver
    /// expressed in physical pixels. `bounds` is the workspace area in
    /// physical pixels (window padding is Window chrome, not decoration).
    #[must_use]
    pub fn layout_with_decoration_scaled(
        &self,
        bounds: Rect,
        decoration: Decoration,
        scale: f64,
        cell: (u16, u16),
        gaps: Gaps,
    ) -> Vec<(ViewId, DecoratedView)> {
        let border = scaled_px(decoration.border, scale);
        let radius = scaled_px(decoration.radius, scale);
        let outer = scaled_px(decoration.gaps_out, scale);
        let inner = scaled_px(decoration.gaps_in, scale);
        let content_inset = scaled_px(decoration.content_inset, scale);
        let bands = Bands {
            outer_x: outer.saturating_add(gaps.outer.saturating_mul(cell.0)),
            outer_y: outer.saturating_add(gaps.outer.saturating_mul(cell.1)),
            inner_x: inner.saturating_add(gaps.inner.saturating_mul(cell.0)),
            inner_y: inner.saturating_add(gaps.inner.saturating_mul(cell.1)),
            border,
            content_inset,
            radius,
        };
        let area = inset_axes(bounds, bands.outer_x, bands.outer_y);
        let mut out = Vec::new();
        self.layout_bands_inner(area, bands, &mut out);
        out
    }

    fn layout_bands_inner(
        &self,
        bounds: Rect,
        bands: Bands,
        out: &mut Vec<(ViewId, DecoratedView)>,
    ) {
        match self {
            Self::Leaf(v) => {
                let frame = bounds;
                let content = inset_rect(frame, bands.border.saturating_add(bands.content_inset));
                out.push((
                    v.id(),
                    DecoratedView {
                        frame,
                        content,
                        border: bands.border,
                        radius: bands.radius,
                    },
                ));
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let band = match axis {
                    crate::geometry::SplitAxis::Horizontal => bands.inner_x,
                    crate::geometry::SplitAxis::Vertical => bands.inner_y,
                };
                let (a, b) = crate::layout::split_rect_with_gap(bounds, *axis, *ratio, band);
                first.layout_bands_inner(a, bands, out);
                second.layout_bands_inner(b, bands, out);
            }
            Self::Stack(children) => {
                for child in children {
                    child.layout_bands_inner(bounds, bands, out);
                }
            }
            Self::Overlay {
                base,
                overlay,
                bounds: overlay_bounds,
                ..
            } => {
                base.layout_bands_inner(bounds, bands, out);
                let clipped = if let Some(inter) = overlay_bounds.clip_to(bounds) {
                    inter
                } else {
                    Rect::zero()
                };
                overlay.layout_bands_inner(clipped, bands, out);
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
    fn defaults_match_unified_spec() {
        // CTX-0333: unified sibling/container gap default (6/6) plus a 6px
        // content inset so text never sits flush against the frame.
        let d = Decoration::default();
        assert_eq!(d.gaps_in, 6);
        assert_eq!(d.gaps_out, 6);
        assert_eq!(d.border, 2);
        assert_eq!(d.radius, 6);
        assert_eq!(d.content_inset, 6);
        // The sibling and container defaults match, so panel-panel and
        // panel-terminal gaps read as one spacing by default.
        assert_eq!(d.gaps_in, d.gaps_out);
    }

    #[test]
    fn safe_mode_is_zero_zero_one_zero_zero() {
        assert_eq!(Decoration::SAFE.gaps_in, 0);
        assert_eq!(Decoration::SAFE.gaps_out, 0);
        assert_eq!(Decoration::SAFE.border, 1);
        assert_eq!(Decoration::SAFE.radius, 0);
        assert_eq!(Decoration::SAFE.content_inset, 0);
        assert!(Decoration::SAFE.validate().is_ok());
    }

    #[test]
    fn validate_bounds_fail_closed() {
        assert!(Decoration::new(32, 32, 8, 16, 32).validate().is_ok());
        assert_eq!(
            Decoration::new(33, 0, 0, 0, 0).validate(),
            Err(DecorationError::GapsIn(33))
        );
        assert_eq!(
            Decoration::new(0, 33, 0, 0, 0).validate(),
            Err(DecorationError::GapsOut(33))
        );
        assert_eq!(
            Decoration::new(0, 0, 9, 0, 0).validate(),
            Err(DecorationError::Border(9))
        );
        assert_eq!(
            Decoration::new(0, 0, 0, 17, 0).validate(),
            Err(DecorationError::Radius(17))
        );
        assert_eq!(
            Decoration::new(0, 0, 0, 0, 33).validate(),
            Err(DecorationError::ContentInset(33))
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
        let out = node.layout_with_decoration(bounds, Decoration::new(0, 6, 0, 0, 0));
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
        let out = node.layout_with_decoration(bounds, Decoration::new(10, 0, 0, 0, 0));
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
        let out = node.layout_with_decoration(bounds, Decoration::new(0, 0, 2, 6, 0));
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
        let out = node.layout_with_decoration(bounds, Decoration::new(4, 2, 0, 0, 0));
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
            node.layout_with_decoration(Rect::new(0, 0, 3, 3), Decoration::new(32, 32, 8, 16, 32));
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
        let d = Decoration::new(3, 5, 2, 6, 0);
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
        let _ =
            node.layout_with_decoration(Rect::new(0, 0, 80, 24), Decoration::new(3, 5, 2, 6, 0));
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
        assert!(
            DecorationError::ContentInset(40)
                .to_string()
                .contains("content_inset")
        );
    }

    #[test]
    fn scaled_zero_decoration_matches_cell_gap_solver() {
        // CTX-0294: with no px decoration the render-scale solver is the
        // CTX-0177 cell-gap solver expressed in physical pixels.
        let node = split(
            0.5,
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::leaf(view(2, 4, 2)),
        );
        let cell = (9u16, 19u16);
        let gaps = Gaps::new(2, 1);
        let cell_bounds = Rect::new(0, 0, 80, 24);
        let px_bounds = Rect::new(0, 0, 80 * 9, 24 * 19);
        let px = node.layout_with_decoration_scaled(px_bounds, Decoration::ZERO, 1.75, cell, gaps);
        let plain = node.layout_with_gaps(cell_bounds, gaps);
        assert_eq!(px.len(), plain.len());
        for ((pid, prect), (did, dview)) in plain.iter().zip(px.iter()) {
            assert_eq!(pid, did);
            assert_eq!(
                dview.frame,
                Rect::new(
                    prect.x * 9,
                    prect.y * 19,
                    prect.width * 9,
                    prect.height * 19
                )
            );
            assert_eq!(dview.content, dview.frame);
        }
    }

    #[test]
    fn scaled_zero_scale_matches_logical_decoration() {
        // CTX-0294: scale 1.0 with zero cell gaps is the accepted logical
        // solver bit-for-bit.
        let node = split(
            0.37,
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::leaf(view(2, 4, 2)),
        );
        let bounds = Rect::new(1, 2, 101, 51);
        let d = Decoration::new(3, 5, 2, 6, 0);
        assert_eq!(
            node.layout_with_decoration(bounds, d),
            node.layout_with_decoration_scaled(bounds, d, 1.0, (9, 19), Gaps::ZERO)
        );
    }

    #[test]
    fn scaled_decoration_scales_insets_hidpi() {
        // CTX-0294/CTX-0333: logical values double at 2x DPI while cell gaps
        // stay per-axis physical. Content inset scales with the border.
        let node = LayoutNode::leaf(view(1, 4, 2));
        let bounds = Rect::new(0, 0, 200, 100);
        let out = node.layout_with_decoration_scaled(
            bounds,
            Decoration::new(0, 6, 2, 6, 3),
            2.0,
            (9, 19),
            Gaps::new(0, 0),
        );
        assert_eq!(out[0].1.frame, Rect::new(12, 12, 176, 76));
        // border 2*2 + content inset 3*2 = 10 px per side.
        assert_eq!(out[0].1.content, Rect::new(22, 22, 156, 56));
        assert_eq!(out[0].1.border, 4);
        assert_eq!(out[0].1.radius, 12);
    }

    #[test]
    fn content_inset_insets_content_inside_border() {
        // CTX-0333: the content rectangle is inset by `border + inset`.
        let node = LayoutNode::leaf(view(1, 4, 2));
        let bounds = Rect::new(0, 0, 40, 20);
        let out = node.layout_with_decoration(bounds, Decoration::new(0, 0, 2, 0, 3));
        assert_eq!(out[0].1.frame, bounds);
        assert_eq!(out[0].1.content, Rect::new(5, 5, 30, 10));
        assert_eq!(out[0].1.border, 2);
    }

    #[test]
    fn zero_content_inset_reproduces_border_only_geometry() {
        // CTX-0333: an explicit zero content inset is the pre-CTX-0333
        // border-only content rectangle, so the old geometry stays reachable.
        let node = LayoutNode::leaf(view(1, 4, 2));
        let bounds = Rect::new(0, 0, 40, 20);
        let zero = node.layout_with_decoration(bounds, Decoration::new(0, 0, 2, 6, 0));
        assert_eq!(zero[0].1.frame, Rect::new(0, 0, 40, 20));
        assert_eq!(zero[0].1.content, Rect::new(2, 2, 36, 16));
        // A non-zero inset pushes the content further inside the border.
        let inset = node.layout_with_decoration(bounds, Decoration::new(0, 0, 2, 6, 3));
        assert_eq!(inset[0].1.content, Rect::new(5, 5, 30, 10));
    }

    #[test]
    fn scaled_content_inset_saturates_without_panic() {
        // Oversized content inset on a tiny frame collapses content to zero,
        // never inverts it.
        let node = LayoutNode::leaf(view(1, 1, 1));
        let out =
            node.layout_with_decoration(Rect::new(0, 0, 5, 5), Decoration::new(0, 0, 1, 0, 32));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.content.width, 0);
        assert_eq!(out[0].1.content.height, 0);
    }

    #[test]
    fn scaled_composes_cell_gaps_with_px_decoration() {
        // CTX-0294: outer inset = 1 cell (9/19 px) + 6 px; the sibling band
        // = 2 cells (18 px wide) + 4 px; border/radius scale too.
        let node = split(
            0.5,
            LayoutNode::leaf(view(1, 4, 2)),
            LayoutNode::leaf(view(2, 4, 2)),
        );
        let bounds = Rect::new(0, 0, 720, 456);
        let out = node.layout_with_decoration_scaled(
            bounds,
            Decoration::new(4, 6, 2, 6, 0),
            1.0,
            (9, 19),
            Gaps::new(2, 1),
        );
        let (a, b) = (out[0].1.frame, out[1].1.frame);
        assert_eq!(a.x, 15);
        assert_eq!(a.y, 25);
        assert_eq!(u32::from(b.x) - a.right(), 22);
        assert_eq!(u32::from(a.width) + 22 + u32::from(b.width), 720 - 30);
        assert_eq!(out[0].1.border, 2);
        assert_eq!(out[0].1.radius, 6);
    }

    #[test]
    fn scaled_hostile_scale_is_total() {
        let node = LayoutNode::leaf(view(1, 4, 2));
        let bounds = Rect::new(0, 0, 40, 20);
        for scale in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e9] {
            let out = node.layout_with_decoration_scaled(
                bounds,
                Decoration::new(32, 32, 8, 16, 32),
                scale,
                (9, 19),
                Gaps::new(16, 16),
            );
            assert_eq!(out.len(), 1);
            let view = out[0].1;
            assert!(view.content.width <= view.frame.width);
            assert!(view.content.height <= view.frame.height);
        }
    }
}
