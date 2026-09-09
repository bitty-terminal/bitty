//! PresentationMode: per-leaf display mode for workspace views (CTX-0276).
//!
//! Unifies the zoom/overlay/visibility special-cases toward one mode field:
//!
//! - tiled tree: [`PresentationMode::Tiled`] leaves participate in the
//!   [`LayoutNode`](crate::layout::LayoutNode) solver directly (status quo).
//! - app-level zoom (`zoom_backup` backup/restore in `bitty-app`): conceptually
//!   [`PresentationMode::Fullscreen`] full-bleed base. The existing
//!   backup/restore mechanics are reused as-is; this crate adds no second
//!   zoom path.
//! - [`OverlayTier`](crate::layout::OverlayTier) paint order
//!   (`Editor < Float < Popup < Messages`): overlay-like modes map onto it
//!   via [`PresentationMode::overlay_tier`].
//! - 4+1 overlay manager (`OverlayManager` in [`crate::panel`]): unchanged,
//!   presentation-only; never mutates the grid.
//! - `Visibility::{..., ScratchpadHidden}` (`bitty-runtime` registry):
//!   deliberately DISTINCT from this enum. `Visibility` is a computed display
//!   state (inactive workspace, zero-area, overlay-occluded, scratchpad
//!   hidden); `PresentationMode` is the requested per-leaf display mode.
//!   Scratchpad-hidden stays a `Visibility` state — this enum is never
//!   flattened with it (assessment correction to 009 §5).
//!
//! # Current scope (explicit decision)
//!
//! Only [`PresentationMode::Tiled`] is live: every [`View`](crate::view::View)
//! starts `Tiled` and the layout solver ignores the field, so existing
//! zoom/overlay behavior is preserved byte-identically. `Floating`,
//! `Fullscreen`, and `Scratchpad` parse (see [`PresentationMode::parse`]) but
//! their transitions are gated: [`PresentationMode::can_transition`] rejects
//! every cross-mode transition for now. They are follow-ups (future Panel
//! Rules prerequisite consumers) — never half-wired here. Re-affirming the
//! current mode (`from == to`) is always allowed.
//!
//! This crate (`bitty-ui`) depends only on `bitty-term-state`; this module
//! adds no new crate dependency.

#![forbid(unsafe_code)]

use crate::layout::OverlayTier;

/// Requested per-leaf display mode on a workspace [`View`](crate::view::View).
///
/// Orthogonal to `Visibility` (computed display state owned by the
/// `bitty-runtime` registry): do NOT convert between the two, and do NOT add
/// `Visibility` variants here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PresentationMode {
    /// Normal tiled leaf: participates in the `LayoutNode` solver directly.
    /// Default; the only live mode in this slice.
    Tiled = 0,
    /// Floating leaf painted above the base at [`OverlayTier::Float`].
    /// Parseable but gated (follow-up).
    Floating = 1,
    /// Full-bleed base leaf (zoom-like; reuses the app-level `zoom_backup`
    /// mechanics conceptually, not as an overlay). Parseable but gated.
    Fullscreen = 2,
    /// Floating leaf that may be hidden via `Visibility::ScratchpadHidden`
    /// (hidden state stays `Visibility` — distinct). Parseable but gated.
    Scratchpad = 3,
}

impl Default for PresentationMode {
    /// Default: [`PresentationMode::Tiled`] (status-quo tiling).
    fn default() -> Self {
        Self::Tiled
    }
}

impl std::fmt::Display for PresentationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PresentationMode {
    /// Canonical lowercase name (`"tiled"`, `"floating"`, `"fullscreen"`,
    /// `"scratchpad"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tiled => "tiled",
            Self::Floating => "floating",
            Self::Fullscreen => "fullscreen",
            Self::Scratchpad => "scratchpad",
        }
    }

    /// Parses a canonical [`Self::as_str`] name. Case-sensitive; rejects
    /// empty, whitespace-padded, and unknown inputs (including legacy
    /// `"zoom"`/`"overlay"`/`"panel"` aliases — no silent aliasing).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tiled" => Some(Self::Tiled),
            "floating" => Some(Self::Floating),
            "fullscreen" => Some(Self::Fullscreen),
            "scratchpad" => Some(Self::Scratchpad),
            _ => None,
        }
    }

    /// Whether transition `from -> to` is allowed.
    ///
    /// Currently only identity (`from == to`, re-affirming the current mode).
    /// All cross-mode transitions are gated follow-ups: `Floating`,
    /// `Fullscreen`, and `Scratchpad` entry/exit is parseable but NOT
    /// transitionable yet, so no half-wired path exists. Existing zoom
    /// (`zoom_backup`) and overlay behavior keep their own mechanics
    /// byte-identically in the meantime.
    #[must_use]
    pub fn can_transition(from: Self, to: Self) -> bool {
        from == to
    }

    /// Overlay paint tier for overlay-like modes, reusing the existing
    /// [`OverlayTier`] order.
    ///
    /// - [`PresentationMode::Tiled`] and [`PresentationMode::Fullscreen`]
    ///   are base content (solver / full-bleed), not overlays: `None`.
    /// - [`PresentationMode::Floating`] and [`PresentationMode::Scratchpad`]
    ///   (when shown) float at [`OverlayTier::Float`], the legacy default
    ///   tier, so they paint above `Editor` chrome and below `Popup` /
    ///   `Messages`.
    #[must_use]
    pub fn overlay_tier(self) -> Option<OverlayTier> {
        match self {
            Self::Tiled | Self::Fullscreen => None,
            Self::Floating | Self::Scratchpad => Some(OverlayTier::Float),
        }
    }

    /// True for the live tiled mode.
    #[must_use]
    pub fn is_tiled(self) -> bool {
        matches!(self, Self::Tiled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_tiled() {
        assert_eq!(PresentationMode::default(), PresentationMode::Tiled);
        assert!(PresentationMode::default().is_tiled());
    }

    #[test]
    fn discriminants_stable_and_tier_aligned() {
        assert_eq!(PresentationMode::Tiled as u8, 0);
        assert_eq!(PresentationMode::Floating as u8, 1);
        assert_eq!(PresentationMode::Fullscreen as u8, 2);
        assert_eq!(PresentationMode::Scratchpad as u8, 3);
        // Overlay-like modes reuse the legacy Float tier position.
        assert_eq!(OverlayTier::Float as u8, 1);
        assert_eq!(OverlayTier::BOTTOM, OverlayTier::Editor);
        assert_eq!(OverlayTier::TOP, OverlayTier::Messages);
    }

    #[test]
    fn parse_roundtrip_all_modes() {
        for mode in [
            PresentationMode::Tiled,
            PresentationMode::Floating,
            PresentationMode::Fullscreen,
            PresentationMode::Scratchpad,
        ] {
            assert_eq!(PresentationMode::parse(mode.as_str()), Some(mode));
            assert_eq!(mode.to_string(), mode.as_str());
        }
    }

    #[test]
    fn parse_rejects_unknown_and_aliases() {
        for bad in [
            "",
            " ",
            "TILED",
            "Tiled",
            " tiled",
            "tiled ",
            "zoom",
            "overlay",
            "panel",
            "float",
            "full-screen",
            "scratchpad_hidden",
        ] {
            assert_eq!(PresentationMode::parse(bad), None, "input {bad:?}");
        }
    }

    #[test]
    fn can_transition_only_identity_until_follow_ups() {
        use PresentationMode as M;
        // Re-affirming any current mode is always safe (no state change).
        for mode in [M::Tiled, M::Floating, M::Fullscreen, M::Scratchpad] {
            assert!(M::can_transition(mode, mode), "{mode:?} -> {mode:?}");
        }
        // Every cross-mode transition is gated: Floating / Fullscreen /
        // Scratchpad entry and exit land as follow-ups, never half-wired.
        let all = [M::Tiled, M::Floating, M::Fullscreen, M::Scratchpad];
        for from in all {
            for to in all {
                if from != to {
                    assert!(!M::can_transition(from, to), "{from:?} -> {to:?}");
                }
            }
        }
    }

    #[test]
    fn overlay_tier_reuses_float_for_overlay_like_modes() {
        assert_eq!(PresentationMode::Tiled.overlay_tier(), None);
        assert_eq!(PresentationMode::Fullscreen.overlay_tier(), None);
        assert_eq!(
            PresentationMode::Floating.overlay_tier(),
            Some(OverlayTier::Float)
        );
        assert_eq!(
            PresentationMode::Scratchpad.overlay_tier(),
            Some(OverlayTier::Float)
        );
    }

    #[test]
    fn layout_allocations_ignore_presentation_mode() {
        // Byte-identical guarantee: the solver reads only leaf ids/bounds,
        // so stamping non-default modes on leaves must not move a single
        // allocation (existing zoom/overlay behavior preserved).
        use crate::geometry::{Rect, SplitAxis};
        use crate::layout::LayoutNode;
        use crate::view::{View, ViewId};

        let bounds = Rect::new(0, 0, 100, 40);
        let base = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            ),
            LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        );
        let mut stamped = base.clone();
        for (id, mode) in [
            (ViewId::new(1), PresentationMode::Floating),
            (ViewId::new(2), PresentationMode::Fullscreen),
            (ViewId::new(3), PresentationMode::Scratchpad),
        ] {
            let leaf = stamped.find_leaf_mut(id).expect("leaf present");
            leaf.set_presentation(mode);
        }
        assert_eq!(base.layout(bounds), stamped.layout(bounds));
        assert_eq!(
            base.layout_with_gaps(bounds, crate::geometry::Gaps::ZERO),
            stamped.layout_with_gaps(bounds, crate::geometry::Gaps::ZERO)
        );
    }
}
