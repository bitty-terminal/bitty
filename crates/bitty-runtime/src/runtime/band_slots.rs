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
//! | `statusline` | bottom edge band ("Core places on the bottom band")    |
//! | `left`       | left edge band — stored, not painted until vertical bands ship |
//! | `right`      | right edge band — stored, not painted until vertical bands ship |
//! | `tabline`    | rejected: reserved for PW-10 panel tabs, not a band surface |
//! | `overlay`    | rejected: no plugin overlay host in the band renderer yet |
//! | `terminal`   | rejected: no terminal-attached block host yet          |
//!
//! Rejected slots fail closed at `ui.mount` with [`E_UI_SLOT_UNSUPPORTED`]
//! (class `runtime`), after the capability and claim gates, so a plugin
//! learns the slot is unhosted instead of rendering nothing.
//!
//! Stacking: within one edge, surfaces stack from the window edge inward
//! (index `0` is outermost) in plugin id byte order; mount order never
//! affects placement, and `statusline` and `bottom` surfaces share one
//! ordering. Several mounts from one plugin on one edge keep mount order.
//! The `chrome.<edge>.order` key is not yet wired into the runtime.

use bitty_lua::host::BridgeError;
use bitty_lua::ui::{UiNode, UiSlot};

use super::{BandContent, ChromeBands};

/// Stable code for an accepted v1 slot this host does not present.
pub const E_UI_SLOT_UNSUPPORTED: &str = "E_UI_SLOT_UNSUPPORTED";

/// Window edge carrying a chrome band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BandEdge {
    /// Top edge (full window width).
    Top,
    /// Bottom edge (full window width).
    Bottom,
    /// Left edge (vertical bands are not painted yet).
    Left,
    /// Right edge (vertical bands are not painted yet).
    Right,
}

/// Host placement of one accepted UI slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiSlotPlacement {
    /// Mounted content joins the band on this edge.
    Band(BandEdge),
    /// The slot is accepted by the v1 contract but not hosted; mounts fail
    /// closed with [`E_UI_SLOT_UNSUPPORTED`] and this reason.
    Unsupported(&'static str),
}

/// The single host placement decision for `slot` (see module docs).
#[must_use]
pub const fn ui_slot_placement(slot: UiSlot) -> UiSlotPlacement {
    match slot {
        UiSlot::Top => UiSlotPlacement::Band(BandEdge::Top),
        UiSlot::Bottom | UiSlot::Statusline => UiSlotPlacement::Band(BandEdge::Bottom),
        UiSlot::Left => UiSlotPlacement::Band(BandEdge::Left),
        UiSlot::Right => UiSlotPlacement::Band(BandEdge::Right),
        UiSlot::Tabline => UiSlotPlacement::Unsupported(
            "is reserved for panel tabs (PW-10) and has no host surface yet",
        ),
        UiSlot::Overlay => {
            UiSlotPlacement::Unsupported("has no plugin overlay host in this build yet")
        }
        UiSlot::Terminal => {
            UiSlotPlacement::Unsupported("has no terminal-attached block host in this build yet")
        }
    }
}

/// Typed `E_UI_SLOT_UNSUPPORTED` error for an unhosted accepted slot.
#[must_use]
pub fn unsupported_slot_error(slot: UiSlot, reason: &str) -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_SLOT_UNSUPPORTED,
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
    /// window `window_rows` tall, stacking from the edge inward; `None` for a
    /// vertical edge or a band that does not fit.
    #[must_use]
    pub fn band_row(edge: BandEdge, index: usize, window_rows: u16) -> Option<u16> {
        let index = u16::try_from(index).ok()?;
        if index >= window_rows {
            return None;
        }
        match edge {
            BandEdge::Top => Some(index),
            BandEdge::Bottom => Some(window_rows - 1 - index),
            BandEdge::Left | BandEdge::Right => None,
        }
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
                UiSlot::Bottom | UiSlot::Statusline => UiSlotPlacement::Band(BandEdge::Bottom),
                UiSlot::Left => UiSlotPlacement::Band(BandEdge::Left),
                UiSlot::Right => UiSlotPlacement::Band(BandEdge::Right),
                UiSlot::Tabline | UiSlot::Overlay | UiSlot::Terminal => {
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
            block("c", UiSlot::Statusline),
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
            [("b", UiSlot::Bottom), ("c", UiSlot::Statusline)]
        );
        assert_eq!(ids(BandEdge::Left), [("d", UiSlot::Left)]);
        assert_eq!(ids(BandEdge::Right), [("e", UiSlot::Right)]);
    }

    #[test]
    fn from_mounts_counts_unsupported_slots_instead_of_dropping_silently() {
        let (bands, unplaced) = ChromeBands::from_mounts([
            block("t", UiSlot::Tabline),
            block("o", UiSlot::Overlay),
            block("x", UiSlot::Terminal),
        ]);
        assert_eq!(unplaced, 3);
        assert!(bands.top.is_empty() && bands.bottom.is_empty());
    }

    #[test]
    fn stacking_is_plugin_id_order_independent_of_mount_order() {
        let (bands, _) = ChromeBands::from_mounts([
            block("zeta", UiSlot::Statusline),
            block("alpha", UiSlot::Bottom),
            block("mid", UiSlot::Statusline),
        ]);
        let order: Vec<_> = bands.bottom.iter().map(|b| b.plugin_id.as_str()).collect();
        assert_eq!(order, ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn band_rows_stack_from_the_window_edge_inward() {
        assert_eq!(ChromeBands::band_row(BandEdge::Top, 0, 24), Some(0));
        assert_eq!(ChromeBands::band_row(BandEdge::Top, 1, 24), Some(1));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 0, 24), Some(23));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 1, 24), Some(22));
        assert_eq!(ChromeBands::band_row(BandEdge::Bottom, 24, 24), None);
        assert_eq!(ChromeBands::band_row(BandEdge::Left, 0, 24), None);
    }

    #[test]
    fn unsupported_error_is_typed() {
        let UiSlotPlacement::Unsupported(reason) = ui_slot_placement(UiSlot::Tabline) else {
            panic!("tabline must be unsupported");
        };
        let error = unsupported_slot_error(UiSlot::Tabline, reason);
        assert_eq!(error.code, E_UI_SLOT_UNSUPPORTED);
        assert_eq!(error.class, "runtime");
        assert!(error.message.contains("'tabline'"));
    }
}
