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
//! | `top`        | top edge band                                          |
//! | `bottom`     | bottom edge band                                       |
//! | `left`       | rejected: no vertical band host in this build          |
//! | `right`      | rejected: no vertical band host in this build          |
//! | `tabline`    | rejected: reserved for PW-10 panel tabs, not a band surface |
//! | `overlay`    | Core focusable-overlay host (CTX-0941): retained, not a band surface |
//! | `terminal`   | v2 rich scene host only; the v1 band path is rejected  |
//!
//! Rejected slots fail closed at `ui.mount` with the existing v1 code
//! [`E_UI_UNAVAILABLE`] (class `runtime`, "host has no surface"), after the
//! capability and claim gates, so a plugin learns the slot is unhosted
//! instead of rendering nothing. The message names the slot and the reason.
//! No new code is added to the OQ-056-frozen v1 error vocabulary.
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
//! [`crate::Runtime::plugin_band_row`].
//!
//! Exclusive zone (CTX-0946 C3, closed): visible plugin bands shrink the
//! layout container through [`Runtime::band_exclusive_container`]
//! (see [`super::band_host`]), so no terminal cell is ever painted under a
//! band. The reservation flows through the normal reflow path, resizing
//! grids and PTY winsizes.
//!
//! Known gaps (tracked follow-ups, not implemented here):
//!
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
    /// Top edge (full window width).
    Top,
    /// Bottom edge (full window width).
    Bottom,
    /// Left edge (no vertical band host in this build; mounts fail closed).
    Left,
    /// Right edge (no vertical band host in this build; mounts fail closed).
    Right,
}

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
        // CTX-1341: no vertical band host exists yet. Admitting these mounts
        // would return a handle the renderer can never paint, reserve, or
        // hit-test, so they fail closed at mount time instead.
        UiSlot::Left | UiSlot::Right => {
            UiSlotPlacement::Unsupported("has no vertical band host in this build yet")
        }
        UiSlot::Tabline => UiSlotPlacement::Unsupported(
            "is reserved for panel tabs (PW-10) and has no host surface yet",
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
        for edge in [
            BandEdge::Top,
            BandEdge::Bottom,
            BandEdge::Left,
            BandEdge::Right,
        ] {
            bands
                .edge_mut(edge)
                .sort_by(|a, b| a.plugin_id.as_bytes().cmp(b.plugin_id.as_bytes()));
        }
        (bands, unplaced)
    }

    /// Window row (cells) painted by horizontal band `index` on `edge` in a
    /// window `window_rows` tall, stacking from the edge inward and starting
    /// inward of `reserved` rows already held on that edge; `None` for a
    /// vertical edge or a band that does not fit.
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
                UiSlot::Overlay => UiSlotPlacement::Overlay,
                UiSlot::Left | UiSlot::Right | UiSlot::Tabline | UiSlot::Terminal => {
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
    fn from_mounts_routes_each_band_slot_to_its_edge() {
        let (bands, unplaced) = ChromeBands::from_mounts([
            block("a", UiSlot::Top),
            block("b", UiSlot::Bottom),
            block("c", UiSlot::Bottom),
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
        assert!(ids(BandEdge::Left).is_empty());
        assert!(ids(BandEdge::Right).is_empty());
    }

    #[test]
    fn vertical_slots_are_unsupported_and_never_routed_to_a_band() {
        // CTX-1341: left/right used to map to a band that nothing painted.
        // The placement policy now fails closed at mount, and any block that
        // somehow reaches routing is counted as unplaced rather than stored.
        for slot in [UiSlot::Left, UiSlot::Right] {
            let UiSlotPlacement::Unsupported(reason) = ui_slot_placement(slot) else {
                panic!("{slot} must be unsupported");
            };
            assert!(reason.contains("vertical band"));
        }
        let (bands, unplaced) =
            ChromeBands::from_mounts([block("l", UiSlot::Left), block("r", UiSlot::Right)]);
        assert_eq!(unplaced, 2);
        assert!(bands.top.is_empty() && bands.bottom.is_empty());
        assert!(bands.left.is_empty() && bands.right.is_empty());
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
    fn unsupported_error_is_typed() {
        for slot in [UiSlot::Tabline, UiSlot::Left, UiSlot::Right] {
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
