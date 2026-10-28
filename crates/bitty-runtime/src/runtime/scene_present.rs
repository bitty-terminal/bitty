#![forbid(unsafe_code)]
//! Scene composition in the shipped present loop (CTX-1630 / bitty#2760).
//!
//! The present loop owns the only live consumer of the OQ-051 scene paint
//! path. For every visible leaf the loop asks the installed
//! [`PanelRuntime`] whether the leaf is a
//! panel with an attached [`Scene`](bitty_rich::scene::Scene); if so it
//! derives the bounded, leaf-clipped paint list through
//! [`PanelRuntime::scene_paint_for`]
//! with the compositor-owned content rect and the live cell metrics, and
//! composes the result into the frame:
//!
//! - fills join the normal leaf fill layer, painted after that leaf's cell
//!   backgrounds and before later leaves;
//! - glyphs go through the grid renderer's overlay text seam
//!   ([`ScenePaint::append_into`] → `GridRenderer::overlay_text_glyphs`), so
//!   scene text and cell text share one rasterizer and one atlas epoch;
//! - image placements are counted and reported, never decoded or blitted
//!   here: the admitted image plane keeps owning that;
//! - damage is the paint list's bounded invalidation rectangle, clipped to
//!   the leaf content rect and handed to the frame descriptor through
//!   `SnapshotDamage::with_extra_regions`.
//!
//! # Fail-closed
//!
//! A typed [`ScenePaintError`] paints nothing for that leaf. The previously
//! presented scene primitives are re-emitted verbatim when the leaf surface,
//! clip, and fade still match, so a refused revision never replaces the
//! visible scene with a partial or clamped one. The refusal is attributed to
//! the runtime counters and diagnostic. A scene text projection that exceeds
//! the accessibility bounds is refused the same way: nothing paints and
//! nothing is projected, so the accessibility tree cannot diverge from the
//! frame.
//!
//! # Accessibility
//!
//! The painted lines are exactly the paint list's own
//! [`ScenePaint::text_lines`], pushed through
//! [`UiSnapshotHost::push_scene_text`]. A panel scene is composed vertically
//! into one [`SceneKind::Column`] node whose identity is the panel id, so
//! the projection carries the scene kind/identity plus the clipped bytes the
//! frame paints, in paint order. Existing host bounds apply unchanged; the
//! whole contribution is refused when it would exceed them.
//!
//! No GPU object, decode seam, or renderer handle crosses this module: it
//! consumes owned paint data and the shared CPU text seam only.

use bitty_render::geometry::RectPx;
use bitty_render::grid::{FillRect, GlyphInstance, RoundedClip, scale_alpha};
use bitty_render::scene::{ScenePaint, ScenePaintError, SceneSurface};
use bitty_ui::ViewId;
use bitty_ui::a11y_host::{SceneKind, UiSnapshotHost};

use super::Runtime;
use crate::cw_present::CW_PRESENT_MAX_SCENE_BLOCKS;
use crate::registry::{PanelRuntime, ViewContent};

/// Accessibility scene kind for a composed panel scene.
///
/// A panel's attached scene is a vertical composition of admitted blocks, so
/// the accessibility node reads as a column and its text runs follow paint
/// order.
pub const SCENE_A11Y_KIND: SceneKind = SceneKind::Column;

/// One painted scene's accessibility projection (CTX-1630).
///
/// `lines` is exactly the paint list's own text, in paint order, after the
/// [`UiSnapshotHost::push_scene_text`] bounds were applied. `identity` is the
/// producer-assigned panel id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneA11yRun {
    /// Accessibility scene kind.
    pub kind: SceneKind,
    /// Producer-assigned scene identity (the panel id).
    pub identity: u64,
    /// Painted lines: leaf-local row ordinal plus the clipped text.
    pub lines: Vec<(u32, String)>,
}

/// Bounded scene diagnostics for the last presented frame (CTX-1630).
///
/// Per-frame only: a frame that presents nothing keeps the previous report,
/// matching the frame still on screen. Cumulative refusal counters live on
/// [`Runtime::scene_paint_refusals`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SceneFrameReport {
    /// Panel leaves that painted (or retained) scene content.
    pub views: usize,
    /// Paint ops composed into the frame.
    pub ops: usize,
    /// Image placement references reserved but not blitted here.
    pub images: usize,
    /// Bounded invalidation rectangles, one per painted scene.
    pub damage: Vec<RectPx>,
    /// Typed paint refusals observed while composing this frame.
    pub refusals: usize,
    /// Accessibility projection of the same painted scenes.
    pub a11y: Vec<SceneA11yRun>,
}

/// Per-leaf primitives one scene contributes to the combined frame.
pub(super) struct LeafScene {
    /// Cell-aligned fills (window pixels).
    pub fills: Vec<FillRect>,
    /// Glyph instances placed through the overlay text seam (window pixels).
    pub glyphs: Vec<GlyphInstance>,
    /// Bounded invalidation rectangles, clipped to the leaf surface.
    pub damage: Vec<RectPx>,
    /// Paint ops composed.
    pub ops: usize,
    /// Image placement references reserved.
    pub images: usize,
    /// Accessibility projection of the same lines.
    pub run: SceneA11yRun,
}

/// Last successfully composed scene primitives for one leaf.
///
/// Reused verbatim when a later paint for the same leaf is refused, so the
/// refused revision keeps the previous visible scene instead of clearing it.
/// Entries are valid only for the exact surface, clip, fade, and atlas epoch
/// they were composed against; any drift drops them (the caller re-renders
/// the leaf normally on the next accepted revision).
#[derive(Debug, Clone)]
pub(super) struct RetainedScene {
    surface: SceneSurface,
    clip: Option<RoundedClip>,
    fade: u32,
    atlas_epoch: u64,
    fills: Vec<FillRect>,
    glyphs: Vec<GlyphInstance>,
    damage: Vec<RectPx>,
    ops: usize,
    images: usize,
    run: SceneA11yRun,
}

impl RetainedScene {
    /// Rebuilds the leaf contribution for a refusal at the same geometry.
    fn primitive(&self) -> LeafScene {
        LeafScene {
            fills: self.fills.clone(),
            glyphs: self.glyphs.clone(),
            damage: self.damage.clone(),
            ops: self.ops,
            images: self.images,
            run: self.run.clone(),
        }
    }
}

impl Runtime {
    /// Installs the panel host the present loop consults for scenes.
    ///
    /// The composition root installs the same [`PanelRuntime`] that owns the
    /// panel slots (OQ-051); the present loop never creates a second scene
    /// store. Installing a host forces the next frame so an already painted
    /// window picks up newly attached scenes.
    pub fn install_scene_host(&mut self, host: PanelRuntime) {
        self.scene_host = Some(host);
        self.pending_full_redraw = true;
    }

    /// Borrows the installed panel host, if any.
    #[must_use]
    pub fn scene_host(&self) -> Option<&PanelRuntime> {
        self.scene_host.as_ref()
    }

    /// Borrows the installed panel host mutably (attach/detach scenes).
    ///
    /// A mutable borrow is treated as a possible scene revision: the next
    /// tick presents a fresh frame even when no PTY or layout state changed.
    #[must_use]
    pub fn scene_host_mut(&mut self) -> Option<&mut PanelRuntime> {
        if self.scene_host.is_some() {
            self.pending_full_redraw = true;
        }
        self.scene_host.as_mut()
    }

    /// Removes the installed panel host and drops every retained scene.
    pub fn take_scene_host(&mut self) -> Option<PanelRuntime> {
        self.presented_scene_frames.clear();
        let host = self.scene_host.take();
        if host.is_some() {
            self.pending_full_redraw = true;
        }
        host
    }

    /// Scene diagnostics for the last presented frame (CTX-1630).
    #[must_use]
    pub const fn scene_frame(&self) -> &SceneFrameReport {
        &self.scene_frame
    }

    /// Cumulative typed scene paint refusals (CTX-1630).
    ///
    /// A refusal paints nothing and keeps the previous retained scene; this
    /// counter is the attribution an operator or test reads.
    #[must_use]
    pub const fn scene_paint_refusals(&self) -> u64 {
        self.scene_paint_refusals
    }

    /// The last typed scene paint refusal, if any (CTX-1630).
    #[must_use]
    pub const fn last_scene_paint_refusal(&self) -> Option<&ScenePaintError> {
        self.last_scene_paint_refusal.as_ref()
    }

    /// Cumulative accessibility refusals for scene text (CTX-1630).
    #[must_use]
    pub const fn scene_a11y_refusals(&self) -> u64 {
        self.scene_a11y_refusals
    }

    /// Composes one leaf's scene into the frame, if the leaf has one.
    ///
    /// Returns `None` when the leaf is not a panel, the panel has no
    /// attached scene, no host is installed, or the scene paints nothing.
    /// A refused paint returns the retained primitives for an unchanged
    /// surface, or `None` when the previous geometry no longer applies.
    ///
    /// `surface` is the compositor-owned leaf content rect in the same pixel
    /// space as the frame (`frame.content` translated by the window padding)
    /// plus the live cell metrics. `a11y` is the frame-local projection host;
    /// it is pushed only after the paint list exists, so the projection and
    /// the frame always describe the same lines.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compose_leaf_scene(
        &mut self,
        view: ViewId,
        surface: SceneSurface,
        clip: Option<RoundedClip>,
        fade: f32,
        occluders: &[RectPx],
        a11y: &mut UiSnapshotHost,
    ) -> Option<LeafScene> {
        let Self {
            scene_host,
            presented_scene_frames,
            renderer,
            scene_paint_refusals,
            last_scene_paint_refusal,
            scene_a11y_refusals,
            ..
        } = self;
        let host = scene_host.as_mut()?;
        let panel = host.placement_panel_of(view);
        let Some(panel) = panel.filter(|panel| host.panel_scene(*panel).is_some()) else {
            // A detached or scene-less leaf drops its retained primitives so
            // a later frame never resurrects them.
            presented_scene_frames.remove(&view);
            return None;
        };
        let identity = panel.get();
        let present = host.scene_paint_for(
            ViewContent::Panel(panel),
            Some(surface),
            CW_PRESENT_MAX_SCENE_BLOCKS,
        );
        let retained = presented_scene_frames.get(&view).filter(|retained| {
            retained.surface == surface
                && retained.clip == clip
                && retained.fade == fade.to_bits()
                && retained.atlas_epoch == renderer.atlas_epoch()
        });
        if let Some(error) = present.refusal {
            *scene_paint_refusals = scene_paint_refusals.saturating_add(1);
            *last_scene_paint_refusal = Some(error);
            return retain_presented(retained, a11y, scene_a11y_refusals);
        }
        if present.paint.is_empty() {
            presented_scene_frames.remove(&view);
            return None;
        }
        let paint: &ScenePaint = &present.paint;
        let before = a11y.scene_text().len();
        let lines: Vec<(u32, &str)> = paint
            .text_lines()
            .map(|line| (u32::from(line.row), line.text))
            .collect();
        if a11y
            .push_scene_text(SCENE_A11Y_KIND, identity, &lines)
            .is_err()
        {
            // Accessibility and paint are one contribution: an over-bound
            // projection is refused whole, never partially applied.
            *scene_a11y_refusals = scene_a11y_refusals.saturating_add(1);
            return retain_presented(retained, a11y, scene_a11y_refusals);
        }
        let run = SceneA11yRun {
            kind: SCENE_A11Y_KIND,
            identity,
            lines: a11y.scene_text()[before..].to_vec(),
        };
        let mut fills = Vec::new();
        let mut glyphs = Vec::new();
        let images = paint.append_into(renderer, &mut fills, &mut glyphs);
        let mut damage = Vec::with_capacity(paint.damage().len());
        for rect in paint.damage() {
            if let Some(clipped) = rect.intersection(&surface.rect) {
                damage.push(clipped);
            }
        }
        for fill in &mut fills {
            if fade < 1.0 {
                fill.color = scale_alpha(fill.color, fade);
            }
        }
        glyphs.retain(|glyph| {
            occluders.is_empty()
                || !super::present::glyph_occluded_by_floats(glyph.dest, glyph.size, occluders)
        });
        for glyph in &mut glyphs {
            glyph.clip = clip;
            if fade < 1.0 {
                glyph.color = scale_alpha(glyph.color, fade);
            }
        }
        let leaf = LeafScene {
            damage,
            ops: paint.ops().len(),
            images,
            fills,
            glyphs,
            run,
        };
        presented_scene_frames.insert(
            view,
            RetainedScene {
                surface,
                clip,
                fade: fade.to_bits(),
                atlas_epoch: renderer.atlas_epoch(),
                fills: leaf.fills.clone(),
                glyphs: leaf.glyphs.clone(),
                damage: leaf.damage.clone(),
                ops: leaf.ops,
                images: leaf.images,
                run: leaf.run.clone(),
            },
        );
        Some(leaf)
    }

    /// Drops retained scene primitives that are not visible this frame.
    pub(super) fn retain_visible_scenes(&mut self, visible: &[ViewId]) {
        self.presented_scene_frames
            .retain(|view, _| visible.contains(view));
    }
}

/// Re-emits the previously presented scene for a refused revision.
///
/// The retained lines re-enter the frame's accessibility projection first, so
/// the tree still describes exactly the retained pixels. If even the retained
/// contribution no longer fits the host bounds, nothing paints: the projection
/// and the frame stay identical.
fn retain_presented(
    retained: Option<&RetainedScene>,
    a11y: &mut UiSnapshotHost,
    a11y_refusals: &mut u64,
) -> Option<LeafScene> {
    let retained = retained?;
    let lines: Vec<(u32, &str)> = retained
        .run
        .lines
        .iter()
        .map(|(row, text)| (*row, text.as_str()))
        .collect();
    if a11y
        .push_scene_text(retained.run.kind, retained.run.identity, &lines)
        .is_err()
    {
        *a11y_refusals = a11y_refusals.saturating_add(1);
        return None;
    }
    Some(retained.primitive())
}
