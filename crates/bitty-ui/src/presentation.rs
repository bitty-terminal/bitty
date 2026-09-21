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
//! # Transitions (CW-08, issue #987)
//!
//! Every [`View`](crate::view::View) starts `Tiled` and the layout solver
//! ignores the field, so existing zoom/overlay behavior is preserved
//! byte-identically. All cross-mode transitions are permitted:
//! [`PresentationMode::can_transition`] is the single gate and it currently
//! allows every `from -> to` pair (identity included). Mode changes route
//! through the workspace command registry (`PRESENTATION_CMD_*` names via
//! [`apply_presentation_command`]), which resolves the command, enforces the
//! gate, and stamps the leaf. Future restrictions (if any) flow through
//! [`PresentationMode::can_transition`] only — never a parallel gate.
//!
//! This crate (`bitty-ui`) depends only on `bitty-term-state`; this module
//! adds no new crate dependency.
//!
//! # Overlay ownership (accepted decision, CTX-0482 / issue #763)
//!
//! Three overlay systems exist and must not be conflated:
//! [`LayoutNode::Overlay`](crate::layout::LayoutNode::Overlay) owns geometry
//! and paint order, [`OverlayManager`](crate::panel::OverlayManager) owns
//! presentation stacking plus the single panel-modal authority, and this
//! enum owns the requested per-leaf display mode (transitions gated, solver
//! ignores it, never paints). A new overlay feature extends exactly one
//! owner; see `OverlayManager` for the full decision text.

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

    /// Whether transition `from -> to` is allowed (CW-08: the single gate).
    ///
    /// Currently every `from -> to` pair is permitted (identity re-affirm
    /// included — it is a no-state-change no-op). Mode changes route through
    /// the workspace command registry ([`apply_presentation_command`]), which
    /// enforces this gate before stamping the leaf, so every transition (and
    /// every rejection, once restrictions exist) flows through exactly here.
    /// The layout solver keeps ignoring the field, so solver output stays
    /// byte-identical to the tiled-only path.
    #[must_use]
    pub fn can_transition(_from: Self, _to: Self) -> bool {
        true
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

    /// Requests the mode change `leaf -> mode` through the single gate.
    ///
    /// Returns `true` and stamps the leaf when
    /// [`Self::can_transition`](PresentationMode::can_transition) allows it
    /// (currently every pair); returns `false` with the leaf untouched when
    /// the gate rejects. Identity requests are allowed no-ops.
    pub fn request_transition(leaf: &mut crate::view::View, mode: Self) -> bool {
        if !Self::can_transition(leaf.presentation(), mode) {
            return false;
        }
        leaf.set_presentation(mode);
        true
    }
}

/// Workspace command names carrying a mode change (CW-08).
///
/// `<owner>.<name>:<command>` grammar (see [`QualifiedCommand`](crate::panel::QualifiedCommand)):
/// one presentation command per target mode, routed by
/// [`apply_presentation_command`].
pub const PRESENTATION_CMD_FLOATING: &str = "bitty.workspace:presentation-floating";
pub const PRESENTATION_CMD_FULLSCREEN: &str = "bitty.workspace:presentation-fullscreen";
pub const PRESENTATION_CMD_SCRATCHPAD: &str = "bitty.workspace:presentation-scratchpad";
pub const PRESENTATION_CMD_TILED: &str = "bitty.workspace:presentation-tiled";

/// Error for [`apply_presentation_command`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresentationCommandError {
    /// `command` is not one of the `PRESENTATION_CMD_*` names.
    UnknownCommand(String),
    /// The [`PresentationMode::can_transition`] gate rejected the change.
    Rejected {
        from: PresentationMode,
        to: PresentationMode,
    },
    /// No leaf with `target` id exists in `tree`.
    LeafNotFound(crate::view::ViewId),
}

impl std::fmt::Display for PresentationCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCommand(cmd) => write!(f, "unknown presentation command: {cmd}"),
            Self::Rejected { from, to } => {
                write!(f, "presentation transition rejected: {from} -> {to}")
            }
            Self::LeafNotFound(id) => write!(f, "presentation target not found: {id}"),
        }
    }
}

impl std::error::Error for PresentationCommandError {}

/// Resolves a mode name to its workspace command name, or `None` for
/// unknown/aliased input (mirrors [`PresentationMode::parse`] strictness).
#[must_use]
pub fn presentation_command_for_mode(mode: PresentationMode) -> &'static str {
    match mode {
        PresentationMode::Tiled => PRESENTATION_CMD_TILED,
        PresentationMode::Floating => PRESENTATION_CMD_FLOATING,
        PresentationMode::Fullscreen => PRESENTATION_CMD_FULLSCREEN,
        PresentationMode::Scratchpad => PRESENTATION_CMD_SCRATCHPAD,
    }
}

/// Parses a workspace presentation command name to its target mode, or
/// `None` when `command` is not a `PRESENTATION_CMD_*` name.
#[must_use]
pub fn presentation_mode_for_command(command: &str) -> Option<PresentationMode> {
    match command {
        PRESENTATION_CMD_TILED => Some(PresentationMode::Tiled),
        PRESENTATION_CMD_FLOATING => Some(PresentationMode::Floating),
        PRESENTATION_CMD_FULLSCREEN => Some(PresentationMode::Fullscreen),
        PRESENTATION_CMD_SCRATCHPAD => Some(PresentationMode::Scratchpad),
        _ => None,
    }
}

/// Applies workspace presentation command `command` to leaf `target` in
/// `tree` (CW-08 command-registry path).
///
/// Resolves the command to its target mode, enforces the single
/// [`PresentationMode::can_transition`] gate on the leaf's current mode, and
/// stamps the leaf on success. Unknown commands and missing leaves fail
/// without touching the tree.
///
/// # Errors
///
/// - [`PresentationCommandError::UnknownCommand`] for non-`PRESENTATION_CMD_*`
///   names.
/// - [`PresentationCommandError::LeafNotFound`] when `target` is absent.
/// - [`PresentationCommandError::Rejected`] when the gate rejects.
pub fn apply_presentation_command(
    tree: &mut crate::layout::LayoutNode,
    command: &str,
    target: crate::view::ViewId,
) -> Result<PresentationMode, PresentationCommandError> {
    let mode = presentation_mode_for_command(command)
        .ok_or_else(|| PresentationCommandError::UnknownCommand(command.to_owned()))?;
    let leaf = tree
        .find_leaf_mut(target)
        .ok_or(PresentationCommandError::LeafNotFound(target))?;
    let from = leaf.presentation();
    if !PresentationMode::can_transition(from, mode) {
        return Err(PresentationCommandError::Rejected { from, to: mode });
    }
    leaf.set_presentation(mode);
    Ok(mode)
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
    fn can_transition_allows_every_pair_cw08() {
        use PresentationMode as M;
        // CW-08 (issue #987): the single gate permits every transition;
        // mode changes route through the workspace command registry.
        let all = [M::Tiled, M::Floating, M::Fullscreen, M::Scratchpad];
        for from in all {
            for to in all {
                assert!(M::can_transition(from, to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn request_transition_stamps_leaf_through_gate() {
        use crate::view::{View, ViewId};
        let mut leaf = View::new(ViewId::new(1), 80, 24);
        assert_eq!(leaf.presentation(), PresentationMode::Tiled);
        assert!(PresentationMode::request_transition(
            &mut leaf,
            PresentationMode::Floating
        ));
        assert_eq!(leaf.presentation(), PresentationMode::Floating);
        // Identity re-affirm is an allowed no-op.
        assert!(PresentationMode::request_transition(
            &mut leaf,
            PresentationMode::Floating
        ));
        assert_eq!(leaf.presentation(), PresentationMode::Floating);
    }

    #[test]
    fn command_names_roundtrip_and_reject_unknown() {
        use PresentationMode as M;
        for mode in [M::Tiled, M::Floating, M::Fullscreen, M::Scratchpad] {
            let cmd = presentation_command_for_mode(mode);
            assert_eq!(presentation_mode_for_command(cmd), Some(mode));
        }
        assert_eq!(presentation_mode_for_command("bitty.workspace:zoom"), None);
        assert_eq!(presentation_mode_for_command(""), None);
        assert_eq!(presentation_mode_for_command("tiled"), None);
    }

    #[test]
    fn apply_presentation_command_routes_through_registry() {
        use crate::geometry::Rect;
        use crate::layout::LayoutNode;
        use crate::view::{View, ViewId};
        // Register the presentation commands through the workspace command
        // registry (owner.name:command grammar, duplicates rejected).
        let mut registry = crate::panel::CommandRegistry::new();
        let owner = crate::panel::PanelId::new(1);
        for mode in [
            PresentationMode::Tiled,
            PresentationMode::Floating,
            PresentationMode::Fullscreen,
            PresentationMode::Scratchpad,
        ] {
            registry
                .register(owner, presentation_command_for_mode(mode))
                .expect("presentation command registers");
        }
        let id = ViewId::new(9);
        let mut tree = LayoutNode::leaf(View::new(id, 80, 24));
        for (command, want) in [
            (PRESENTATION_CMD_FLOATING, PresentationMode::Floating),
            (PRESENTATION_CMD_FULLSCREEN, PresentationMode::Fullscreen),
            (PRESENTATION_CMD_SCRATCHPAD, PresentationMode::Scratchpad),
            (PRESENTATION_CMD_TILED, PresentationMode::Tiled),
        ] {
            assert_eq!(registry.owner_of(command), Some(owner));
            let got =
                apply_presentation_command(&mut tree, command, id).expect("transition routes");
            assert_eq!(got, want);
            assert_eq!(tree.find_leaf(id).expect("leaf").presentation(), want);
        }
        // Unknown command fails without touching the tree.
        let before = tree.clone();
        assert!(matches!(
            apply_presentation_command(&mut tree, "bitty.workspace:nope", id),
            Err(PresentationCommandError::UnknownCommand(_))
        ));
        assert_eq!(tree, before);
        // Missing leaf fails.
        assert!(matches!(
            apply_presentation_command(&mut tree, PRESENTATION_CMD_FLOATING, ViewId::new(404)),
            Err(PresentationCommandError::LeafNotFound(_))
        ));
        // Solver still ignores the stamped field (byte-identical).
        let bounds = Rect::new(0, 0, 100, 40);
        let plain = LayoutNode::leaf(View::new(id, 80, 24));
        assert_eq!(plain.layout(bounds), tree.layout(bounds));
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
