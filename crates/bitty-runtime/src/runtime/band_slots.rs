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
//! | `overlay`    | Core focusable-overlay host (CTX-0941): retained, not a band surface |
//! | `terminal`   | rejected: no terminal-attached block host yet          |
//!
//! Rejected slots fail closed at `ui.mount` with the existing v1 code
//! [`E_UI_UNAVAILABLE`] (class `runtime`, "host has no surface"), after the
//! capability and claim gates, so a plugin learns the slot is unhosted
//! instead of rendering nothing. The message names the slot and the reason.
//! No new code is added to the OQ-056-frozen v1 error vocabulary.
//!
//! Stacking: within one edge, surfaces stack from the window edge inward
//! (index `0` is outermost) in plugin id byte order; mount order never
//! affects placement, and `statusline` and `bottom` surfaces share one
//! ordering. The `chrome.<edge>.order` key is not yet wired into the runtime.
//!
//! Core reservation: plugin bands start inward of the rows the Core
//! workspaceline band reserves on the same edge
//! ([`crate::Runtime::status_bar_band`], solved once by
//! `chrome_band::solve`), so with `workspace.bar.edge = bottom` (default) and
//! two or more workspaces bottom band `0` sits on row `H-2`, never on the
//! Core bar row `H-1`; the same holds for top bands with `edge = top`. See
//! [`crate::Runtime::plugin_band_row`].
//!
//! Exclusive zone (CTX-0946 C3, closed): visible plugin bands shrink the
//! layout container through [`Runtime::band_exclusive_container`]
//! (see [`super::band_host`]), so no terminal cell is ever painted under a
//! band. The reservation flows through the normal reflow path, resizing
//! grids and PTY winsizes exactly like the Core workspaceline band.
//!
//! Known gaps (tracked follow-ups, not implemented here):
//!
//! - Known divergence from the candidate rule "one plugin may hold at most
//!   one surface per edge; a second mount on the same edge fails": several
//!   mounts from one plugin on one edge (for example `statusline` and
//!   `bottom`) are all kept and stack in mount order. Enforcing the candidate
//!   rule later is a deliberate behavior change, not a regression.

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
        UiSlot::Bottom | UiSlot::Statusline => UiSlotPlacement::Band(BandEdge::Bottom),
        UiSlot::Left => UiSlotPlacement::Band(BandEdge::Left),
        UiSlot::Right => UiSlotPlacement::Band(BandEdge::Right),
        UiSlot::Tabline => UiSlotPlacement::Unsupported(
            "is reserved for panel tabs (PW-10) and has no host surface yet",
        ),
        // CTX-0941: the `overlay` slot is a Core-hosted focusable overlay. The
        // block is retained (not a band) and may own the transient input
        // capture through `bitty.ui.overlay.*`.
        UiSlot::Overlay => UiSlotPlacement::Overlay,
        UiSlot::Terminal => {
            UiSlotPlacement::Unsupported("has no terminal-attached block host in this build yet")
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
    /// inward of the `core_reserved` rows the Core workspaceline band holds
    /// on that edge; `None` for a vertical edge or a band that does not fit.
    #[must_use]
    pub fn band_row(
        edge: BandEdge,
        index: usize,
        window_rows: u16,
        core_reserved: u16,
    ) -> Option<u16> {
        let offset = core_reserved.checked_add(u16::try_from(index).ok()?)?;
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
    /// Rows the Core workspaceline band reserves on horizontal `edge`
    /// (`0` when no band is reserved there or for a vertical edge).
    ///
    /// Derived from the single chrome solve ([`Self::status_bar_band`]), so
    /// plugin band stacking can never drift from the Core reservation.
    #[must_use]
    pub fn core_reserved_rows(&self, edge: BandEdge) -> u16 {
        let window = self.window_cells();
        let Some(bar) = self.status_bar_band() else {
            return 0;
        };
        let bar_end = bar.y.saturating_add(bar.height);
        match edge {
            BandEdge::Top if bar.y == window.y => bar.height,
            BandEdge::Bottom if bar_end == window.y.saturating_add(window.height) => bar.height,
            _ => 0,
        }
    }

    /// Window row painted by visible plugin band `index` on `edge`, offset
    /// inward of the Core workspaceline band on that edge (see
    /// [`ChromeBands::band_row`]); `None` when it does not fit.
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
        ChromeBands::band_row(edge, index, window.height, self.core_reserved_rows(edge))
            .map(|row| row.saturating_add(window.y))
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
            block("zeta", UiSlot::Statusline),
            block("alpha", UiSlot::Bottom),
            block("mid", UiSlot::Statusline),
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
    fn band_rows_start_inward_of_the_core_reservation() {
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
        let UiSlotPlacement::Unsupported(reason) = ui_slot_placement(UiSlot::Tabline) else {
            panic!("tabline must be unsupported");
        };
        let error = unsupported_slot_error(UiSlot::Tabline, reason);
        assert_eq!(error.code, E_UI_UNAVAILABLE);
        assert_eq!(error.class, "runtime");
        assert!(error.message.contains("'tabline'"));
        assert!(error.message.contains(reason));
    }
}
