//! `Runtime` — copy-modal containment (CTX-0937, W-144 policy retirement).
//!
//! The vi-style modal copy-mode interaction policy retired to the copy-mode
//! plugin (CTX-0003, blocked on W-139/W-138): cursor/visual state machine,
//! all motions, the modal keymap, the yank gesture, and the status label.
//! The duplicated word/wide-motion helpers retired in favor of the Core
//! algebra (`Selection::snapped`, `snap_to_leading`, `word_at`,
//! `word_drag`). Core keeps only the mechanism: bound-View grid resolution,
//! selection install through the selection-drive bridge host op, the
//! snapshot-projection (`CopySpace` compositing), and the clipboard plus
//! primary write paths with the permission gate (`runtime::search_host`,
//! `runtime::selection`).
//!
//! Parked until W-01 input capture lands (owner: W-01): the `copy_mode`
//! state plus its transient readouts, the release hook
//! ([`Runtime::exit_copy_mode`], kept so the `selection` lifecycle funnels
//! still release the binding), and modal containment in
//! [`Runtime::handle_copy_mode_key`] (consume-all while active, so no
//! keystroke can leak to the PTY from a half-moved modal). Copy mode can no
//! longer activate (entry retired with the policy), so the containment is
//! unreachable in practice and `input.rs` modal routing stays byte-identical.
//! No new dependencies, `forbid(unsafe_code)` via the crate root.
use super::*;

/// Bounded copy-mode state (`O(1)`: three small values, no heap).
///
/// W-144 parked state shape (owner: W-01): the cursor/visual state machine
/// that drove this retired to the copy-mode plugin; the struct stays as the
/// `copy_mode` field type plus the transient readouts below until input
/// capture lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyModeState {
    /// Cursor in cursor-space coordinates (live grid while the focused
    /// view is live, focused-viewport rows while it is scrolled into
    /// history). Always clamped plus wide-snapped.
    pub cursor: CellPos,
    /// Visual anchor when a visual selection is active.
    pub anchor: Option<CellPos>,
    /// Visual kind when active (`None` means cursor-only, no selection).
    pub visual_kind: Option<SelectionKind>,
}

impl CopyModeState {
    /// New cursor-only state at `cursor`.
    #[must_use]
    pub const fn new(cursor: CellPos) -> Self {
        Self {
            cursor,
            anchor: None,
            visual_kind: None,
        }
    }

    /// Whether a visual selection is active.
    #[must_use]
    pub const fn is_visual(self) -> bool {
        self.anchor.is_some() && self.visual_kind.is_some()
    }
}

/// Cursor-space snapshot plus the buffer row of its row 0.
///
/// M1-15 history support (CTX-0665): while the focused view is scrolled
/// into history the copy cursor addresses viewport rows, so motions read
/// the composited viewport (`View::visible_cells`: history plus live
/// grid) instead of the live snapshot. While live this is exactly the
/// live snapshot at buffer origin `scrollback_len`, preserving the
/// pre-history behavior bit-for-bit.
///
/// W-144 parked (owner: W-01/W-139): uncalled until input capture plus SDK
/// bindings land; kept so the plugin's snapshot reads assemble on the exact
/// Core compositing instead of recomputing grid internals.
#[allow(dead_code)]
struct CopySpace {
    snapshot: Snapshot,
    /// Combined buffer row shown at snapshot row 0.
    origin: usize,
    /// Grid column shown at snapshot column 0 (viewport `col_offset`).
    col_origin: usize,
}

// W-144 parked (owner: W-01/W-139): uncalled until capture plus SDK
// bindings land, except `copy_binding_live` (used by modal containment);
// kept as the selection-install plus snapshot-projection seams the plugins
// assemble on.
#[allow(dead_code)]
impl Runtime {
    /// Grid the active copy mode walks (CTX-0805, #1478): the live grid of
    /// the View copy mode is bound to, never the primary grid by assumption.
    ///
    /// W-144 parked binding invariant (owner: W-01): copy mode can no longer
    /// activate (entry retired with the policy), so the binding only ever
    /// clears through the release hook. The primary fallback is therefore
    /// unreachable in practice; it only keeps the projection total.
    fn copy_state(&self) -> &State {
        self.copy_mode_view
            .and_then(|view| self.live_view_state(view))
            .unwrap_or(&self.state)
    }

    /// Leaf of the View copy mode is bound to, if still in the layout.
    fn copy_view(&self) -> Option<&View> {
        self.copy_mode_view.and_then(|id| self.layout.find_leaf(id))
    }

    /// Whether the bound copy-mode View still resolves to a live grid.
    fn copy_binding_live(&self) -> bool {
        self.copy_mode_view
            .is_some_and(|view| self.live_view_state(view).is_some())
    }

    /// Installs a copy-mode visual as the live selection, owned by the View
    /// copy mode walks (CTX-0805).
    ///
    /// CTX-0936 (W-143b dogfood): installs through the selection-drive
    /// bridge host op. Fail-closed on a lost grid (the live selection is
    /// dropped rather than installed against a dead owner); unreachable in
    /// practice because the lifecycle funnels end copy mode when its grid
    /// dies.
    fn set_copy_selection(&mut self, selection: Selection) {
        match self.copy_mode_view {
            Some(view) => {
                if self
                    .search_host_install_selection(view, selection, None, false)
                    .is_err()
                {
                    self.drop_selection();
                }
            }
            None => self.drop_selection(),
        }
    }

    /// Whether the copy cursor currently addresses scrolled history
    /// (bound view offset nonzero and resolvable).
    fn is_copy_space_scrolled(&self) -> bool {
        self.copy_view()
            .is_some_and(|view| view.scroll_offset() != 0)
    }

    /// Builds the cursor-space snapshot for copy motions.
    fn copy_space(&self) -> CopySpace {
        let state = self.copy_state();
        if let Some(view) = self.copy_view() {
            if view.scroll_offset() != 0 {
                let rows = view.rows() as usize;
                let cols = view.cols() as usize;
                if rows > 0 && cols > 0 {
                    let sb_len = state.scrollback_len();
                    let total = sb_len + state.height();
                    let offset = view.scroll_offset().min(sb_len);
                    let origin = total.saturating_sub(rows).saturating_sub(offset);
                    let mut snapshot = state.snapshot();
                    snapshot.width = cols;
                    snapshot.height = rows;
                    snapshot.cells = view.visible_cells(state);
                    return CopySpace {
                        snapshot,
                        origin,
                        col_origin: view.col_offset() as usize,
                    };
                }
            }
        }
        CopySpace {
            snapshot: state.snapshot(),
            origin: state.scrollback_len(),
            col_origin: 0,
        }
    }
}

impl Runtime {
    /// Whether keyboard copy mode is active.
    ///
    /// W-144 parked readout (owner: W-01): the `chrome_keys` modal guards
    /// and `input` routing branch on this until input capture lands. Copy
    /// mode can no longer activate, so this reads `false` in practice.
    #[must_use]
    pub fn is_copy_mode(&self) -> bool {
        self.copy_mode.is_some()
    }

    /// Copy-mode cursor in cursor-space coordinates, if active.
    ///
    /// W-144 parked readout (owner: W-01): live-grid rows while the focused
    /// view is live, focused-viewport rows while it is scrolled into history
    /// (M1-15). Always `None` in practice until capture lands.
    #[must_use]
    pub fn copy_mode_cursor(&self) -> Option<CellPos> {
        self.copy_mode.map(|c| c.cursor)
    }

    /// Visual kind when a visual selection is active (`None` otherwise).
    ///
    /// W-144 parked readout (owner: W-01): reuses the CTX-0385
    /// [`SelectionKind`] vocabulary so copy-mode visuals stay typed without
    /// new selection algebra. Always `None` in practice until capture lands.
    #[must_use]
    pub fn copy_mode_visual_kind(&self) -> Option<SelectionKind> {
        self.copy_mode.and_then(|c| c.visual_kind)
    }

    /// Releases the copy-mode binding when its View loses its grid.
    ///
    /// W-144 parked release hook (owner: W-01): the `selection` lifecycle
    /// funnels call this so a dead binding still tears down state, view,
    /// live selection, and redraw exactly as before. Plugin-owned teardown
    /// arrives with W-01 capture release. Plain exit copies nothing; the
    /// yank gesture retired to the copy-mode plugin with the clipboard
    /// writes staying Core-side (`runtime::search_host`).
    pub fn exit_copy_mode(&mut self) {
        if self.copy_mode.is_none() {
            return;
        }
        self.copy_mode = None;
        self.copy_mode_view = None;
        self.drop_selection();
        self.pending_full_redraw = true;
    }

    /// Modal containment while copy mode is active.
    ///
    /// W-144 parked capture-dispatch (owner: W-01): consumes every key while
    /// active so no keystroke can leak to the PTY from a half-moved modal.
    /// The copy-mode keymap itself (hjkl/arrows/pages, visuals, yank, word
    /// motions) moved to the copy-mode plugin; `input.rs` routing calls this
    /// unchanged until capture lands. The binding-liveness check stays: a
    /// grid that dies without a lifecycle funnel observing it still ends the
    /// session instead of stranding the modal.
    pub(super) fn handle_copy_mode_key(&mut self, event: &KeyEvent) -> bool {
        if self.copy_mode.is_none() {
            return false;
        }
        if !self.copy_binding_live() {
            self.exit_copy_mode();
            return true;
        }
        let _ = event;
        true
    }
}
