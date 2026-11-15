//! Host placement policy for the frozen v1 UI slot set (CTX-0923, OQ-056).
//!
//! [`bitty_lua::ui::UiSlot`] owns the slot *names*; this module owns the
//! single host decision of what each slot *does* in this Core build. The
//! `ui.mount` gate ([`crate::plugin_runtime::PluginServices`]) and the chrome
//! band routing ([`ChromeBands::from_mounts`]) both call
//! [`ui_slot_placement`], so a slot can never be admitted at mount time and
//! then dropped at render time.
//!
//! Mapping (candidate Chrome Surface API, "L0 Surfaces"; candidate Chrome
//! Band Contract, "Lua-mounted chrome"):
//!
//! | Slot         | Placement                                              |
//! |--------------|--------------------------------------------------------|
//! | `top`        | top edge band (one row, full width)                    |
//! | `bottom`     | bottom edge band (one row, full width)                 |
//! | `left`       | left edge band (column-shaped, see below)              |
//! | `right`      | right edge band (column-shaped, see below)             |
//! | `tabline`    | rejected: a tab strip is Core tab-projection state (issue #3166), not a band row |
//! | `overlay`    | Core focusable-overlay host (CTX-0941): retained, not a band surface |
//! | `terminal`   | v2 rich scene host only; the v1 band path is rejected  |
//!
//! Rejected slots fail closed at `ui.mount` with the existing v1 code
//! [`E_UI_UNAVAILABLE`] (class `runtime`, "host has no surface"), after the
//! capability and claim gates, so a plugin learns the slot is unhosted
//! instead of rendering nothing. The message names the slot and the reason.
//! No new code is added to the OQ-056-frozen v1 error vocabulary.
//!
//! Band axes (issue #3165): a **row** band (`top`/`bottom`) is one window row
//! wide across the window and stacks its content left to right; a **column**
//! band (`left`/`right`) owns a column range on its edge, spans the container
//! rows, and stacks one content row per direct child of its root, top-down.
//! A column band's width is the widest direct child of its root in display
//! cells (clamped to the window width), so the scene decides the band
//! thickness and the exclusive zone it reserves. A `Column`/`List` root is
//! column-shaped, an axis-neutral `Text` root is a single row, and a `Row`
//! root is row-shaped: mounting a row-shaped root into `left`/`right` fails
//! closed with the accepted `E_UI_COMPONENT_INVALID` component check naming
//! the slot and the root kind, instead of painting a horizontally composed
//! row down a vertical band. Row bands keep their pre-#3165 flattening (a
//! `Column`/`List` root flows left-to-right in them), so `top`/`bottom`
//! behaviour is unchanged.
//!
//! `terminal` is the one slot with two readers: the v2 rich reader
//! ([`bitty_lua::ui_rich::read_rich_component`]) and the Core scene host
//! (`crate::scene_host::SceneHost`, bitty#2785), which attach an admitted
//! `Scene` to the focused leaf. It is not a band, so this placement table (and
//! the band routing that consumes it) keeps rejecting the v1 shape.
//!
//! Stacking: within one edge, surfaces stack from the window edge inward
//! (index `0` is outermost) in plugin id byte order; mount order never
//! affects placement. The `chrome.<edge>.order` key is not yet wired into the runtime.
//!
//! CTX-0979: Core draws no workspace display (Hyprland-style). Plugin bands
//! stack from the window edge inward with no Core reservation; see
//! [`crate::Runtime::plugin_band_row`] for row bands and
//! [`crate::Runtime::visible_band_column`] for column bands.
//!
//! Exclusive zone (CTX-0946 C3, closed): visible plugin bands shrink the
//! layout container through [`Runtime::band_exclusive_container`]
//! (see [`super::band_host`]), so no terminal cell is ever painted under a
//! band. Horizontal bands reserve rows and, since issue #3165, vertical bands
//! reserve columns. The reservation flows through the normal reflow path,
//! resizing grids and PTY winsizes.
//!
//! Known gaps (tracked follow-ups, not implemented here):
//!
//! - Column bands paint, reserve, and route input (issue #3171), but a hover
//!   tooltip anchored to a `left`/`right` band item still falls back to the
//!   centered modal placement: [`crate::BandOverlayAnchor`] resolves its row
//!   against a row band only.
//! - Known divergence from the candidate rule "one plugin may hold at most
//!   one surface per edge; a second mount on the same edge fails": several
//!   mounts from one plugin on one edge are all kept and stack in mount
//!   order. Enforcing the candidate rule later is a deliberate behavior
//!   change, not a regression.

use bitty_lua::host::BridgeError;
pub use bitty_lua::host::E_UI_UNAVAILABLE;
use bitty_lua::ui::{UiNode, UiSlot};

use super::{BandContent, ChromeBands, Runtime};

/// Window edge carrying a chrome band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BandEdge {
    /// Top edge (row band, full window width).
    Top,
    /// Bottom edge (row band, full window width).
    Bottom,
    /// Left edge (column band, spans the container rows).
    Left,
    /// Right edge (column band, spans the container rows).
    Right,
}

/// Axis a band stacks its content on (issue #3165).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BandAxis {
    /// One window row, content flowing left to right (`top`, `bottom`).
    Row,
    /// A column range, one content row per root child flowing top-down
    /// (`left`, `right`).
    Column,
}

impl BandAxis {
    /// Host spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Column => "column",
        }
    }
}

impl BandEdge {
    /// Axis this edge's band stacks on (issue #3165).
    #[must_use]
    pub const fn axis(self) -> BandAxis {
        match self {
            Self::Top | Self::Bottom => BandAxis::Row,
            Self::Left | Self::Right => BandAxis::Column,
        }
    }
}

/// Every band edge, in construction order (`top`, `bottom`, `left`, `right`).
pub const BAND_EDGES: [BandEdge; 4] = [
    BandEdge::Top,
    BandEdge::Bottom,
    BandEdge::Left,
    BandEdge::Right,
];

/// Host placement of one accepted UI slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiSlotPlacement {
    /// Mounted content joins the band on this edge.
    Band(BandEdge),
    /// The slot is served by the Core focusable-overlay surface (CTX-0941):
    /// the block is retained in the generation registry and may own the
    /// transient input capture, but it is not routed to a chrome band. The
    /// overlay presentation host (not this band renderer) consumes it.
    Overlay,
    /// The slot is accepted by the v1 contract but not hosted; mounts fail
    /// closed with [`E_UI_UNAVAILABLE`] and this reason.
    Unsupported(&'static str),
}

/// The single host placement decision for `slot` (see module docs).
#[must_use]
pub const fn ui_slot_placement(slot: UiSlot) -> UiSlotPlacement {
    match slot {
        UiSlot::Top => UiSlotPlacement::Band(BandEdge::Top),
        UiSlot::Bottom => UiSlotPlacement::Band(BandEdge::Bottom),
        // Issue #3165: the vertical band host serves these slots. The scene
        // axis is checked separately (`band_scene_matches`), so a row-shaped
        // component is refused at mount instead of painted down a column.
        UiSlot::Left => UiSlotPlacement::Band(BandEdge::Left),
        UiSlot::Right => UiSlotPlacement::Band(BandEdge::Right),
        UiSlot::Tabline => UiSlotPlacement::Unsupported(
            "is Core tab-projection state (issue #3166 `core.tab:*` verbs) and has no host surface yet",
        ),
        // CTX-0941: the `overlay` slot is a Core-hosted focusable overlay. The
        // block is retained (not a band) and may own the transient input
        // capture through `bitty.ui.overlay.*`.
        UiSlot::Overlay => UiSlotPlacement::Overlay,
        // bitty#2785: the v2 rich reader + Core scene host serve `terminal`;
        // the v1 band tree still has no terminal-attached band host.
        UiSlot::Terminal => {
            UiSlotPlacement::Unsupported("has no v1 terminal-attached band host in this build")
        }
    }
}

/// How `ui.state` reports a block mounted in one slot (issue #3166).
///
/// Keeps the presentation rule of each slot in one place, next to the placement
/// decision, so `UiBlockState`
/// ([`crate::plugin_runtime::PluginServices::ui_block_state`]) can never report
/// a rule that contradicts [`ui_slot_placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStateRule {
    /// One-row band: presented when the block paints visible text
    /// (`band_is_visible`), hidden when its text is empty.
    BandVisibility,
    /// Core focusable overlay (CTX-0941): presented when the generation owns
    /// the transient input capture or an active presentation session.
    OverlayCapture,
    /// No host surface in this build; never presented.
    NeverPresented,
}

/// The presentation rule for `slot`.
///
/// Only `top`/`bottom` are one-row bands hidden by empty text. `tabline` is
/// deliberately **not** in that set: a tab strip is a Core tab projection
/// ([`bitty_ui::tabs`]) with several entries, so the band rule would be wrong
/// even once a host exists — and its placement is
/// [`UiSlotPlacement::Unsupported`], which [`SlotStateRule::NeverPresented`]
/// matches.
#[must_use]
pub const fn slot_state_rule(slot: UiSlot) -> SlotStateRule {
    match slot {
        UiSlot::Top | UiSlot::Bottom => SlotStateRule::BandVisibility,
        UiSlot::Overlay => SlotStateRule::OverlayCapture,
        UiSlot::Tabline | UiSlot::Left | UiSlot::Right | UiSlot::Terminal => {
            SlotStateRule::NeverPresented
        }
    }
}

/// Whether `root` may paint in the band on `edge` (issue #3165).
///
/// A row band accepts every v1 root: its flattening walk has always laid a
/// `Column`/`List` root out left to right (documented pre-#3165 degradation,
/// pinned by the parity suite), so `top`/`bottom` behaviour is unchanged.
///
/// A column band stacks one content row per direct child of a `Column`/`List`
/// root, and admits the axis-neutral `Text` root as a single row. A `Row` root
/// is a horizontal composition and cannot be stacked down a column, so it is
/// refused at mount through [`band_scene_direction_error`].
#[must_use]
pub fn band_scene_matches(edge: BandEdge, root: &UiNode) -> bool {
    match edge.axis() {
        BandAxis::Row => true,
        BandAxis::Column => !matches!(root, UiNode::Row { .. }),
    }
}

/// Typed refusal for a scene whose axis does not fit the band's axis
/// (issue #3165).
///
/// Keeps the accepted `bitty.ui.mount` component-error vocabulary: the class
/// is `validation` and the code `E_UI_COMPONENT_INVALID`, the same pair the
/// bridge uses for a component that cannot be served. The mount gate calls
/// this instead of storing a block the paint path would have to clip or
/// transpose, so a direction mismatch is a typed error rather than garbage.
#[must_use]
pub fn band_scene_direction_error(slot: UiSlot, edge: BandEdge, root: &UiNode) -> BridgeError {
    bitty_lua::ui::component_invalid(format!(
        "UI slot '{slot}' is a {} band and cannot stack a '{}' root: a row-shaped scene \
         would paint horizontally inside a column band, so mount a Column or List root",
        edge.axis().as_str(),
        root.kind()
    ))
}

/// Typed `E_UI_UNAVAILABLE` error for an unhosted accepted slot.
#[must_use]
pub fn unsupported_slot_error(slot: UiSlot, reason: &str) -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_UNAVAILABLE,
        format!("UI slot '{slot}' {reason}"),
    )
}

impl ChromeBands {
    /// Bands on `edge`, index `0` outermost (at the window edge).
    #[must_use]
    pub fn edge(&self, edge: BandEdge) -> &[BandContent] {
        match edge {
            BandEdge::Top => &self.top,
            BandEdge::Bottom => &self.bottom,
            BandEdge::Left => &self.left,
            BandEdge::Right => &self.right,
        }
    }

    fn edge_mut(&mut self, edge: BandEdge) -> &mut Vec<BandContent> {
        match edge {
            BandEdge::Top => &mut self.top,
            BandEdge::Bottom => &mut self.bottom,
            BandEdge::Left => &mut self.left,
            BandEdge::Right => &mut self.right,
        }
    }

    /// Route mounted blocks `(plugin_id, slot, root, version)` into bands
    /// through [`ui_slot_placement`] and order every edge deterministically
    /// (plugin id byte order, stable for one plugin's own mounts).
    ///
    /// Blocks in an unsupported slot cannot exist (the mount gate rejects
    /// them); one reaching here anyway is skipped and counted in the second
    /// return value so the caller can emit a bounded diagnostic.
    pub fn from_mounts<I>(blocks: I) -> (Self, usize)
    where
        I: IntoIterator<Item = (String, UiSlot, UiNode, u32)>,
    {
        let mut bands = Self::default();
        let mut unplaced = 0usize;
        for (plugin_id, slot, root, version) in blocks {
            match ui_slot_placement(slot) {
                UiSlotPlacement::Band(edge) => bands.edge_mut(edge).push(BandContent {
                    plugin_id,
                    slot,
                    root,
                    version,
                }),
                // Hosted by the focusable-overlay surface, not by a band;
                // skipping it here is a placement, not a silent drop.
                UiSlotPlacement::Overlay => {}
                UiSlotPlacement::Unsupported(_) => unplaced = unplaced.saturating_add(1),
            }
        }
        for edge in BAND_EDGES {
            bands
                .edge_mut(edge)
                .sort_by(|a, b| a.plugin_id.as_bytes().cmp(b.plugin_id.as_bytes()));
        }
        (bands, unplaced)
    }

    /// Window row (cells) painted by horizontal band `index` on `edge` in a
    /// window `window_rows` tall, stacking from the edge inward and starting
    /// inward of `reserved` rows already held on that edge; `None` for a
    /// vertical (column) edge or a band that does not fit.
    #[must_use]
    pub fn band_row(edge: BandEdge, index: usize, window_rows: u16, reserved: u16) -> Option<u16> {
        let offset = reserved.checked_add(u16::try_from(index).ok()?)?;
        if offset >= window_rows {
            return None;
        }
        match edge {
            BandEdge::Top => Some(offset),
            BandEdge::Bottom => Some(window_rows - 1 - offset),
            BandEdge::Left | BandEdge::Right => None,
        }
    }

    /// Leftmost window column of vertical band `index` on `edge`, which
    /// occupies `width` columns starting `offset` columns inward of the edge
    /// (the edge-mirror of [`Self::band_row`], issue #3165).
    ///
    /// `offset` is the sum of the widths of the visible bands held outward of
    /// `index` on the same edge (see
    /// [`Runtime::visible_band_column`](crate::Runtime::visible_band_column)),
    /// so the band never overlaps its outward neighbours. `None` for a
    /// horizontal (row) edge, a zero width, or a band that does not fit the
    /// window width — the caller then paints nothing rather than clipping a
    /// band into the content area.
    #[must_use]
    pub fn band_column(edge: BandEdge, offset: u16, width: u16, window_cols: u16) -> Option<u16> {
        if width == 0 {
            return None;
        }
        let end = offset.checked_add(width)?;
        if end > window_cols {
            return None;
        }
        match edge {
            BandEdge::Left => Some(offset),
            BandEdge::Right => Some(window_cols - end),
            BandEdge::Top | BandEdge::Bottom => None,
        }
    }
}

impl Runtime {
    /// Window row painted by visible plugin band `index` on `edge`
    /// (CTX-0979: no Core reservation; see [`ChromeBands::band_row`]);
    /// `None` when it does not fit.
    ///
    /// `index` counts visible (non-empty-text) bands only: hidden bands
    /// take no stacking row (CTX-0946 C3, CTX-0925 item 3), so `index >=
    /// visible count` is out of range even when more bands are mounted.
    #[must_use]
    pub fn plugin_band_row(&self, edge: BandEdge, index: usize) -> Option<u16> {
        if u64::try_from(index).unwrap_or(u64::MAX) >= u64::from(self.visible_band_count(edge)) {
            return None;
        }
        let window = self.window_cells();
        ChromeBands::band_row(edge, index, window.height, 0).map(|row| row.saturating_add(window.y))
    }

    /// Window column painted by visible plugin band `index` on a vertical
    /// `edge` (issue #3165): the column mirror of [`Self::plugin_band_row`],
    /// translated into window cells.
    #[must_use]
    pub fn plugin_band_column(&self, edge: BandEdge, index: usize) -> Option<u16> {
        if u64::try_from(index).unwrap_or(u64::MAX)
            >= u64::from(self.visible_band_column_count(edge))
        {
            return None;
        }
        let window = self.window_cells();
        self.visible_band_column(edge, index)
            .map(|col| col.saturating_add(window.x))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(plugin: &str, slot: UiSlot) -> (String, UiSlot, UiNode, u32) {
        (plugin.to_string(), slot, UiNode::text(plugin), 1)
    }

    #[test]
    fn every_v1_slot_has_exactly_one_placement() {
        for slot in UiSlot::ALL {
            let expected = match slot {
                UiSlot::Top => UiSlotPlacement::Band(BandEdge::Top),
                UiSlot::Bottom => UiSlotPlacement::Band(BandEdge::Bottom),
                // Issue #3165: the vertical band host serves `left`/`right`.
                UiSlot::Left => UiSlotPlacement::Band(BandEdge::Left),
                UiSlot::Right => UiSlotPlacement::Band(BandEdge::Right),
                UiSlot::Overlay => UiSlotPlacement::Overlay,
                UiSlot::Tabline | UiSlot::Terminal => {
                    assert!(matches!(
                        ui_slot_placement(slot),
                        UiSlotPlacement::Unsupported(_)
                    ));
                    continue;
                }
            };
            assert_eq!(ui_slot_placement(slot), expected, "{slot}");
        }
    }

    #[test]
    fn band_edges_carry_their_axis() {
        // Issue #3165: the axis is what the mount gate compares a scene
        // against, so it must be exhaustive over the closed edge set.
        assert_eq!(BandEdge::Top.axis(), BandAxis::Row);
        assert_eq!(BandEdge::Bottom.axis(), BandAxis::Row);
        assert_eq!(BandEdge::Left.axis(), BandAxis::Column);
        assert_eq!(BandEdge::Right.axis(), BandAxis::Column);
        assert_eq!(BandAxis::Row.as_str(), "row");
        assert_eq!(BandAxis::Column.as_str(), "column");
        assert_eq!(
            BAND_EDGES,
            [
                BandEdge::Top,
                BandEdge::Bottom,
                BandEdge::Left,
                BandEdge::Right
            ]
        );
    }

    #[test]
    fn from_mounts_routes_each_band_slot_to_its_edge() {
        let (bands, unplaced) = ChromeBands::from_mounts([
            block("a", UiSlot::Top),
            block("b", UiSlot::Bottom),
            block("c", UiSlot::Bottom),
            block("d", UiSlot::Left),
            block("e", UiSlot::Right),
        ]);
        assert_eq!(unplaced, 0);
        let ids = |edge| {
            bands
                .edge(edge)
                .iter()
                .map(|b| (b.plugin_id.as_str(), b.slot))
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(BandEdge::Top), [("a", UiSlot::Top)]);
        assert_eq!(
            ids(BandEdge::Bottom),
            [("b", UiSlot::Bottom), ("c", UiSlot::Bottom)]
        );
        assert_eq!(ids(BandEdge::Left), [("d", UiSlot::Left)]);
        assert_eq!(ids(BandEdge::Right), [("e", UiSlot::Right)]);
    }

    #[test]
    fn vertical_slots_route_to_their_own_edge_and_never_to_a_row_band() {
        // Issue #3165: `left`/`right` used to be refused at mount (CTX-1341,
        // after #2234 replaced a silent no-op with a typed refusal). The
        // vertical band host now serves them, so routing must place each
        // block on its own edge and never leak it into a row band.
        for (slot, edge) in [
            (UiSlot::Left, BandEdge::Left),
            (UiSlot::Right, BandEdge::Right),
        ] {
            let (bands, unplaced) = ChromeBands::from_mounts([block("v", slot)]);
            assert_eq!(unplaced, 0, "{slot} is hosted");
            assert_eq!(bands.edge(edge).len(), 1, "{slot} lands on its edge");
            assert_eq!(bands.edge(edge)[0].slot, slot);
            assert!(bands.top.is_empty() && bands.bottom.is_empty());
            let other = if edge == BandEdge::Left {
                BandEdge::Right
            } else {
                BandEdge::Left
            };
            assert!(bands.edge(other).is_empty(), "{slot} never mirrors");
        }
    }

    #[test]
    fn from_mounts_counts_unsupported_slots_instead_of_dropping_silently() {
        let (bands, unplaced) =
            ChromeBands::from_mounts([block("t", UiSlot::Tabline), block("x", UiSlot::Terminal)]);
        assert_eq!(unplaced, 2);
        assert!(bands.top.is_empty() && bands.bottom.is_empty());
    }

    #[test]
    fn from_mounts_places_the_focusable_overlay_without_a_band() {
        // CTX-0941: the overlay is a hosted placement, so it is neither a
        // band nor counted as unplaced.
        let (bands, unplaced) = ChromeBands::from_mounts([block("o", UiSlot::Overlay)]);
        assert_eq!(unplaced, 0);
        assert!(bands.top.is_empty() && bands.bottom.is_empty());
    }

    #[test]
    fn stacking_is_plugin_id_order_independent_of_mount_order() {
        let (bands, _) = ChromeBands::from_mounts([
            block("zeta", UiSlot::Bottom),
            block("alpha", UiSlot::Bottom),
            block("mid", UiSlot::Bottom),
        ]);
        let order: Vec<_> = bands.bottom.iter().map(|b| b.plugin_id.as_str()).collect();
        assert_eq!(order, ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn band_rows_stack_from_the_window_edge_inward() {
        assert_eq!(ChromeBands::band_row(BandEdge::Top, 0, 24, 0), Some(0));
        assert_eq!(ChromeBands::band_row(BandEdge::Top, 1, 24, 0), Some(1));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 0, 24, 0), Some(23));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 1, 24, 0), Some(22));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 24, 24, 0), None);
        assert_eq!(ChromeBands::band_row(BandEdge::Left, 0, 24, 0), None);
    }

    #[test]
    fn band_rows_start_inward_of_reserved_rows() {
        assert_eq!(ChromeBands::band_row(BandEdge::Top, 0, 24, 1), Some(1));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 0, 24, 1), Some(22));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 1, 24, 1), Some(21));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 23, 24, 1), None);
        assert_eq!(
            ChromeBands::band_row(BandEdge::Top, usize::MAX, 24, 1),
            None
        );
        assert_eq!(ChromeBands::band_row(BandEdge::Top, 1, 24, u16::MAX), None);
    }

    #[test]
    fn band_columns_mirror_band_rows_from_the_window_edge() {
        // Issue #3165: the column mirror of `band_row`, addressed by the
        // band's inward offset and its content-derived width.
        assert_eq!(ChromeBands::band_column(BandEdge::Left, 0, 6, 80), Some(0));
        assert_eq!(ChromeBands::band_column(BandEdge::Left, 6, 4, 80), Some(6));
        assert_eq!(
            ChromeBands::band_column(BandEdge::Right, 0, 6, 80),
            Some(74)
        );
        assert_eq!(
            ChromeBands::band_column(BandEdge::Right, 6, 4, 80),
            Some(70)
        );
        // A band that does not fit never lands inside the content area.
        assert_eq!(ChromeBands::band_column(BandEdge::Left, 76, 6, 80), None);
        assert_eq!(ChromeBands::band_column(BandEdge::Right, 76, 6, 80), None);
        assert_eq!(ChromeBands::band_column(BandEdge::Left, 0, 0, 80), None);
        assert_eq!(ChromeBands::band_column(BandEdge::Left, 80, 1, 80), None);
        // Row edges have no column geometry.
        assert_eq!(ChromeBands::band_column(BandEdge::Top, 0, 6, 80), None);
        assert_eq!(ChromeBands::band_column(BandEdge::Bottom, 0, 6, 80), None);
        // Saturating: a hostile offset never wraps into the window.
        assert_eq!(
            ChromeBands::band_column(BandEdge::Left, u16::MAX, 2, 80),
            None
        );
    }

    #[test]
    fn band_scene_axis_matrix_admits_column_scenes_and_refuses_row_scenes() {
        // Issue #3165 acceptance 2: a column band stacks rows, so a
        // `Column`/`List` root (one row per child) and the axis-neutral
        // `Text` root are served, while a `Row` root is refused rather than
        // transposed or silently clipped.
        let column = UiNode::column(vec![UiNode::text("a"), UiNode::text("b")]);
        let list = UiNode::list(vec![UiNode::text("a")]);
        let text = UiNode::text("a");
        let row = UiNode::row(vec![UiNode::text("a"), UiNode::text("b")]);
        for edge in [BandEdge::Left, BandEdge::Right] {
            assert!(band_scene_matches(edge, &column), "{edge:?} column");
            assert!(band_scene_matches(edge, &list), "{edge:?} list");
            assert!(band_scene_matches(edge, &text), "{edge:?} text");
            assert!(!band_scene_matches(edge, &row), "{edge:?} row refused");
        }
        // Row bands keep serving every v1 root (pre-#3165 flattening).
        for edge in [BandEdge::Top, BandEdge::Bottom] {
            for root in [&column, &list, &text, &row] {
                assert!(band_scene_matches(edge, root), "{edge:?} {}", root.kind());
            }
        }
    }

    #[test]
    fn band_direction_error_is_typed_and_names_the_slot_and_kind() {
        // Acceptance 2: the refusal is typed and diagnosable, not a silent
        // clip. It keeps the accepted component-error vocabulary (no new v1
        // code) so a plugin can tell it apart from an unhosted slot.
        let row = UiNode::row(vec![UiNode::text("a")]);
        let error = band_scene_direction_error(UiSlot::Left, BandEdge::Left, &row);
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert!(error.message.contains("'left'"), "{}", error.message);
        assert!(error.message.contains("'Row'"), "{}", error.message);
        assert!(error.message.contains("column"), "{}", error.message);
        // Not the unhosted-slot code: `left` is hosted, the scene is wrong.
        assert_ne!(error.code, E_UI_UNAVAILABLE);
    }

    #[test]
    fn tabline_has_no_band_visibility_rule() {
        // Issue #3166 acceptance 6: the `ui.state` presentation rule and the
        // placement verdict must agree. A tab strip is Core tab-projection
        // state with several entries, so it is neither a one-row band gated by
        // `band_is_visible` nor a mountable slot.
        assert_eq!(
            slot_state_rule(UiSlot::Tabline),
            SlotStateRule::NeverPresented
        );
        assert!(matches!(
            ui_slot_placement(UiSlot::Tabline),
            UiSlotPlacement::Unsupported(_)
        ));
        // Every slot whose placement is unsupported reports no presentation.
        for slot in UiSlot::ALL {
            if matches!(ui_slot_placement(slot), UiSlotPlacement::Unsupported(_)) {
                assert_eq!(
                    slot_state_rule(slot),
                    SlotStateRule::NeverPresented,
                    "{slot} is unhosted, so it can never be presented"
                );
            }
            // Only the row bands use the one-row, empty-text rule.
            let band = matches!(ui_slot_placement(slot), UiSlotPlacement::Band(edge) if edge.axis() == BandAxis::Row);
            assert_eq!(
                slot_state_rule(slot) == SlotStateRule::BandVisibility,
                band,
                "{slot} presentation rule must match its band axis"
            );
        }
        // The overlay capture rule stays with the Core focusable overlay.
        assert_eq!(
            slot_state_rule(UiSlot::Overlay),
            SlotStateRule::OverlayCapture
        );
    }

    #[test]
    fn unsupported_error_is_typed() {
        for slot in [UiSlot::Tabline, UiSlot::Terminal] {
            let UiSlotPlacement::Unsupported(reason) = ui_slot_placement(slot) else {
                panic!("{slot} must be unsupported");
            };
            let error = unsupported_slot_error(slot, reason);
            assert_eq!(error.code, E_UI_UNAVAILABLE);
            assert_eq!(error.class, "runtime");
            assert!(error.message.contains(&format!("'{slot}'")));
            assert!(error.message.contains(reason));
        }
    }
}
