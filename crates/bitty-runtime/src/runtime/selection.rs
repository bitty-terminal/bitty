//! `Runtime` — Selection, clipboard, paste gating, and truncation helpers.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
//!
//! CTX-0803 (DEC-0078 D1, issues #1476/#1433): the selection is View-owned.
//! [`SelectionState`] binds the range, the drag pin, and the drag flag to the
//! one [`ViewId`] whose grid they address, and every reader resolves that
//! owner's live grid before touching a snapshot. There is at most one live
//! selection; pressing in another `View` replaces it.
use super::input::key_inspect_label;
use super::*;

/// The single live selection plus the `View` that owns it (CTX-0803, D1;
/// CTX-1021 scrolled viewport for issue #1807).
///
/// Held as `Option<SelectionState>` on [`Runtime`], so an owner-less
/// selection is not representable. When the owner is live
/// (`scroll_offset == 0`), `selection` addresses cells of `owner`'s grid
/// (the pane session's grid, or the primary grid when `owner` is
/// [`Runtime::primary_view`]), never the runtime-global primary grid by
/// assumption. When the owner is scrolled into history
/// (`is_viewport_scrolled`), `selection` addresses *viewport* cells of the
/// `visible_cells` composite (`0..view.rows-1`, `0..view.cols-1`), so a drag
/// addresses exactly the visible text under the cursor at any scroll
/// position; `selection_text` and the highlight paint read the same
/// composite, and wheel scrolls during a drag shift the stored viewport
/// anchor to keep its buffer line pinned (see `shift_selection_for_scroll`).
///
/// Keyed by owner on purpose: moving to per-`View` persistent selections
/// (deferred) is a mechanical change from `Option<SelectionState>` to a
/// `ViewId`-keyed map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionState {
    /// `View` whose grid (live) or viewport (scrolled) `selection` addresses.
    pub(crate) owner: ViewId,
    /// Live range in the owner's grid coordinates, or viewport range into
    /// the scrolled composite (see above).
    pub(crate) selection: Selection,
    /// Raw press cell that started the current drag (CTX-0385).
    ///
    /// Pins word/line drag extension direction (`word_drag`/`line_drag`
    /// compare the live pointer against this, not against the expanded
    /// range). `None` once the drag is committed. Viewport coordinates when
    /// scrolled, live-grid coordinates when live (same space as `selection`).
    pub(crate) anchor_press: Option<CellPos>,
    /// Whether a pointer drag is currently extending `selection`.
    pub(crate) dragging: bool,
}

/// Grid backing `view` from disjoint field borrows (CTX-0805).
///
/// The free-function twin of [`Runtime::session_state_for`]: the pane
/// session's grid, else the primary grid when `view` is the primary owner.
/// Taking the fields separately lets a caller hold this grid while it
/// mutably borrows another field (the search state, a layout leaf).
pub(super) fn grid_of<'a>(
    pane_sessions: &'a BTreeMap<ViewId, super::panes::PaneSession>,
    primary_view: Option<ViewId>,
    primary: &'a State,
    view: ViewId,
) -> Option<&'a State> {
    if let Some(session) = pane_sessions.get(&view) {
        return Some(&session.state);
    }
    (primary_view == Some(view)).then_some(primary)
}

pub(super) fn clamp_cell_pos(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    let max_row = snapshot.height.saturating_sub(1) as u16;
    let max_col = snapshot.width.saturating_sub(1) as u16;
    CellPos::new(pos.row.min(max_row), pos.col.min(max_col))
}

fn truncate_paste_text(text: String) -> String {
    const MAX_BYTES: usize = bitty_platform::clipboard::CLIPBOARD_MAX_BYTES;
    if text.len() <= MAX_BYTES {
        return text;
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Truncate `s` to at most `max_bytes` at a char boundary (CTX-0186 summary).
fn truncate_str_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Runtime {
    /// Live grid backing `view`: its pane session, else the primary grid when
    /// `view` owns it, else `None` (CTX-0803 promoted from `session.rs`).
    ///
    /// Presence-only: it does not check that `view` is a leaf of the active
    /// layout. Selection readers use [`Self::selection_owner_state`], which
    /// adds that check and fails closed.
    pub(super) fn session_state_for(&self, view: ViewId) -> Option<&State> {
        grid_of(&self.pane_sessions, self.primary_view, &self.state, view)
    }

    /// Live grid backing `view` **and** its membership in the active layout.
    ///
    /// `None` when `view` is not a leaf of the current tree (closed, on
    /// another workspace, hidden by a zoom) or owns no grid. This is the
    /// fail-closed guard every selection reader goes through: a stale owner
    /// yields no grid, so it yields no selection.
    pub(super) fn live_view_state(&self, view: ViewId) -> Option<&State> {
        self.layout.find_leaf(view)?;
        self.session_state_for(view)
    }

    /// The live selection together with its owner's grid, fail-closed.
    ///
    /// `None` when no selection exists or the owner no longer resolves to a
    /// live grid in the active layout. Readers must not reach for
    /// `self.state` (the primary grid) on their own: that is the #1476 bug.
    pub(super) fn selection_owner_state(&self) -> Option<(&SelectionState, &State)> {
        let sel = self.selection_state.as_ref()?;
        let state = self.live_view_state(sel.owner)?;
        Some((sel, state))
    }

    /// Drops the live selection unconditionally (single clear funnel).
    ///
    /// Every clear — pointer, keyboard, copy mode, search, lifecycle
    /// invalidation — goes through here so the owner, the range, the drag
    /// pin, and the drag flag can never be cleared apart.
    pub(super) fn drop_selection(&mut self) {
        if self.selection_state.is_some() {
            self.selection_state = None;
        }
        self.pending_full_redraw = true;
    }

    /// Installs `selection` owned by `owner`, replacing any live selection.
    ///
    /// The caller has already clamped and snapped against `owner`'s grid
    /// (live) or viewport composite (scrolled, CTX-1021). D1: at most one
    /// live selection, so a press in another `View` replaces the previous
    /// one rather than stacking.
    pub(super) fn install_selection(
        &mut self,
        owner: ViewId,
        selection: Selection,
        anchor_press: Option<CellPos>,
        dragging: bool,
    ) {
        self.selection_state = Some(SelectionState {
            owner,
            selection,
            anchor_press,
            dragging,
        });
        self.pending_full_redraw = true;
    }

    /// Shifts a scrolled viewport selection by `delta_rows` viewport rows to
    /// keep its buffer lines pinned across a scroll offset change (CTX-1021,
    /// issue #1807 scroll-while-selecting tracking).
    ///
    /// `delta_rows` is `new_offset - old_offset` for a pure offset scroll
    /// (wheel, scrollbar, page; total unchanged): the same buffer line sits
    /// `delta_rows` lower in the new viewport, so the stored viewport anchor
    /// moves down by `delta_rows` to stay on its buffer line. Positive is
    /// scrolling up into history. Rows are shifted as `i32` and clamped to
    /// `0..=u16::MAX` (an anchor that scrolls out of the viewport clamps for
    /// display; its buffer text stays available through the viewport snapshot
    /// while any endpoint remains in bounds — fully out-of-window anchors are
    /// a deferred buffer-persistence follow-up).
    ///
    /// When `dragging`, only the anchor (and its press pin) shifts: the focus
    /// stays at the cursor's frame-local cell, so the selection tracks the
    /// pointer as the viewport slides. Word/line drags additionally re-expand
    /// around the shifted press pin in the new viewport composite, so the
    /// focus word/line follows visible text. When committed, both endpoints
    /// shift so the highlight scrolls with its content. No-op when no
    /// selection exists or `owner` does not own it.
    pub(super) fn shift_selection_for_scroll(&mut self, owner: ViewId, delta_rows: isize) {
        if delta_rows == 0 {
            return;
        }
        let Some(sel) = self.selection_state else {
            return;
        };
        if sel.owner != owner {
            return;
        }
        // Only viewport selections shift: a live-grid selection addresses the
        // live grid, not a scroll window. `is_viewport_scrolled` is checked
        // against the *new* offset (post-scroll); a selection that was live
        // before the scroll and is now scrolled (or vice versa) still shifts
        // by the same delta to keep its buffer line when the sizes match
        // (the common grid == viewport case). A fully general live<->scrolled
        // buffer remap is deferred; the delta keeps the in-viewport tests
        // exact.
        let shift_row = |row: u16| -> u16 {
            let next = row as i32 + delta_rows as i32;
            next.clamp(0, u16::MAX as i32) as u16
        };
        if sel.dragging {
            match sel.selection.kind {
                SelectionKind::Simple | SelectionKind::Block => {
                    let mut next = sel.selection;
                    next.anchor.row = shift_row(next.anchor.row);
                    let pin = sel.anchor_press.map(|pin| CellPos {
                        row: shift_row(pin.row),
                        col: pin.col,
                    });
                    // Focus stays at the cursor: do not shift it.
                    self.install_selection(sel.owner, next, pin.or(sel.anchor_press), true);
                }
                SelectionKind::Word | SelectionKind::Line => {
                    // Shift the press pin, then re-expand in the new viewport
                    // so the focus word/line follows visible text.
                    let Some(viewport) = self.viewport_snapshot_for(sel.owner) else {
                        // Fell back to live (offset 0 with stale state?):
                        // shift the pin against the live snapshot instead.
                        let Some(state) = self.live_view_state(sel.owner) else {
                            self.drop_selection();
                            return;
                        };
                        let snap = state.snapshot();
                        let press = sel.anchor_press.unwrap_or(sel.selection.anchor);
                        let shifted = CellPos::new(
                            shift_row(press.row),
                            press.col.min(snap.width.saturating_sub(1) as u16),
                        );
                        let current = sel.selection.focus;
                        let next = match sel.selection.kind {
                            SelectionKind::Word => Selection::word_drag(&snap, shifted, current),
                            _ => Selection::line_drag(&snap, shifted, current),
                        };
                        self.install_selection(sel.owner, next, Some(shifted), true);
                        return;
                    };
                    let press = sel.anchor_press.unwrap_or(sel.selection.anchor);
                    let shifted = CellPos::new(
                        shift_row(press.row),
                        press.col.min(viewport.width.saturating_sub(1) as u16),
                    );
                    // Focus stays at the cursor's frame-local cell: re-read it
                    // from the last known cursor through the new mapping when
                    // available, else keep the stored focus.
                    let current = self
                        .last_cursor
                        .and_then(|pos| self.cursor_to_owner_cell(sel.owner, pos))
                        .unwrap_or(sel.selection.focus);
                    let next = match sel.selection.kind {
                        SelectionKind::Word => Selection::word_drag(&viewport, shifted, current),
                        _ => Selection::line_drag(&viewport, shifted, current),
                    };
                    self.install_selection(sel.owner, next, Some(shifted), true);
                }
            }
        } else {
            let mut next = sel.selection;
            next.anchor.row = shift_row(next.anchor.row);
            next.focus.row = shift_row(next.focus.row);
            let pin = sel.anchor_press.map(|pin| CellPos {
                row: shift_row(pin.row),
                col: pin.col,
            });
            self.install_selection(sel.owner, next, pin, false);
        }
    }

    /// Drops the live selection when `view` owns it (CTX-0803 lifecycle).
    ///
    /// Used where `view`'s grid itself is replaced or torn down while the leaf
    /// may still exist in the layout (pane close, pane respawn): the range
    /// addresses a grid that is gone, so the stale-owner guard alone would not
    /// catch it.
    pub(super) fn drop_selection_owned_by(&mut self, view: ViewId) {
        if self
            .selection_state
            .as_ref()
            .is_some_and(|sel| sel.owner == view)
        {
            self.drop_selection();
        }
    }

    /// Drops the live selection when its owner is no longer a live grid in
    /// the active layout (CTX-0803 lifecycle funnel).
    ///
    /// Called from the layout-install funnel (`replace_layout`, which covers
    /// close, zoom, and workspace switch), the pane-session remove/respawn
    /// sites, and the primary re-home sites. Backed by the same fail-closed
    /// read guard, so a missed call degrades to "no selection" rather than to
    /// a selection painted against the wrong grid.
    pub(super) fn invalidate_selection_if_owner_stale(&mut self) {
        let Some(sel) = self.selection_state.as_ref() else {
            return;
        };
        if self.live_view_state(sel.owner).is_none() {
            self.drop_selection();
        }
    }

    /// Lifecycle funnel for every View-bound consumer (CTX-0803/CTX-0805).
    ///
    /// Drops the selection, ends copy mode, and ends the search when the
    /// View each is bound to no longer resolves to a live grid in the active
    /// layout. Called from the same funnels as the selection invalidation
    /// (layout install, primary re-home, session restore).
    pub(super) fn invalidate_stale_view_bindings(&mut self) {
        self.invalidate_selection_if_owner_stale();
        if self
            .copy_mode_view
            .is_some_and(|view| self.live_view_state(view).is_none())
        {
            self.exit_copy_mode();
        }
        if self
            .search_view
            .is_some_and(|view| self.live_view_state(view).is_none())
        {
            self.end_search_binding();
        }
    }

    /// Drops every consumer bound to `view` because its grid was replaced
    /// or torn down while the leaf may still be in the layout (pane close,
    /// pane respawn): selection, copy mode, and search (CTX-0803/CTX-0805).
    pub(super) fn drop_view_bindings_for(&mut self, view: ViewId) {
        self.drop_selection_owned_by(view);
        if self.copy_mode_view == Some(view) {
            self.exit_copy_mode();
        }
        if self.search_view == Some(view) {
            self.end_search_binding();
        }
    }

    /// Ends the search bound to a View that lost its grid: closes the
    /// overlay when open, else clears the non-modal search state.
    fn end_search_binding(&mut self) {
        if self.search_mode {
            self.exit_search_mode();
        } else {
            self.search_clear();
        }
    }

    /// Drops every View-bound consumer unconditionally (session restore
    /// installs a new world whose grids none of them addressed).
    pub(super) fn drop_all_view_bindings(&mut self) {
        self.drop_selection();
        self.exit_copy_mode();
        self.end_search_binding();
    }

    /// View the keyboard consumers address (CTX-0805, #1478): the View an
    /// active search is bound to, else the focused View.
    ///
    /// The persistent-selection API and a fresh search read this View's
    /// grid instead of assuming the primary grid.
    pub(super) fn keyboard_view(&self) -> Option<ViewId> {
        self.search_view.or_else(|| self.focused_view())
    }

    /// Live grid of [`Self::keyboard_view`], fail-closed.
    pub(super) fn keyboard_grid(&self) -> Option<&State> {
        self.live_view_state(self.keyboard_view()?)
    }

    /// Current selection, if any (read-only).
    ///
    /// CTX-0803: fails closed when the owning `View` is no longer a live leaf
    /// of the active layout — a stale owner reports no selection instead of
    /// coordinates into someone else's grid.
    #[must_use]
    pub fn selection(&self) -> Option<Selection> {
        self.selection_owner_state().map(|(sel, _)| sel.selection)
    }

    /// `View` that owns the live selection, if any (CTX-0803).
    ///
    /// Fails closed like [`Self::selection`]: a selection whose owner left
    /// the active layout reports `None`.
    #[must_use]
    pub fn selection_owner(&self) -> Option<ViewId> {
        self.selection_owner_state().map(|(sel, _)| sel.owner)
    }

    /// Whether a drag is in progress.
    #[must_use]
    pub fn is_selection_dragging(&self) -> bool {
        self.selection_owner_state()
            .is_some_and(|(sel, _)| sel.dragging)
    }

    /// Whether a selection currently exists and is non-empty.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection().is_some_and(|s| !s.is_empty())
    }

    /// Clears the current selection.
    pub fn clear_selection(&mut self) {
        self.drop_selection();
    }

    /// Directly sets the selection on the primary grid (headless test seam).
    ///
    /// CTX-0803: the selection is owned by [`Self::primary_view`], which
    /// keeps the historic single-pane semantics. A runtime with no primary
    /// owner installs nothing (fail closed) — use
    /// [`Self::set_view_selection`] to target a specific `View`.
    pub fn set_selection(&mut self, selection: Selection) {
        if let Some(primary) = self.primary_view {
            let _ = self.set_view_selection(primary, selection);
        } else {
            self.drop_selection();
        }
    }

    /// Directly sets the selection owned by `view` (headless test seam).
    ///
    /// Clamps and snaps against `view`'s own grid. Returns `false` when
    /// `view` is not a live leaf of the active layout or owns no grid, in
    /// which case no selection is installed and any previous one is dropped
    /// (fail closed).
    pub fn set_view_selection(&mut self, view: ViewId, selection: Selection) -> bool {
        let Some(state) = self.live_view_state(view) else {
            self.drop_selection();
            return false;
        };
        let snap = state.snapshot();
        let clamped = selection.clamped(&snap).snapped(Some(&snap));
        let active = clamped.active;
        self.install_selection(view, clamped, Some(clamped.anchor), active);
        true
    }

    /// Current selection kind, if a selection exists (CTX-0385).
    ///
    /// Extension point for keyboard copy mode (CTX-0384) and scrollback
    /// search UI (CTX-0383): they branch on the typed kind without touching
    /// the click tracker.
    #[must_use]
    pub fn selection_kind(&self) -> Option<SelectionKind> {
        self.selection().map(|s| s.kind)
    }

    /// Click count of the last left press (`1..=3`, CTX-0385).
    #[must_use]
    pub fn last_click_count(&self) -> u8 {
        self.last_click_count
    }

    /// Starts a new selection at `pos` in the primary grid (mouse down).
    ///
    /// CTX-0803: the primary-grid entry point, kept for its existing
    /// callers and single-pane semantics. The pointer path uses
    /// [`Self::start_selection_in`] with the hit-tested owner.
    pub fn start_selection(&mut self, pos: CellPos) {
        self.start_selection_in(SelectionKind::Simple, pos);
    }

    /// Starts a word selection at `pos` in the primary grid (double-click,
    /// CTX-0385).
    ///
    /// Expands to the containing word via [`Selection::word_at`]; a press on
    /// a delimiter yields a collapsed single cell (no selection on release,
    /// matching stream semantics). Drag after this extends word-wise via
    /// [`Self::update_selection`].
    pub fn start_word_selection(&mut self, pos: CellPos) {
        self.start_selection_in(SelectionKind::Word, pos);
    }

    /// Starts a line selection at `pos` row in the primary grid
    /// (triple-click, CTX-0385).
    ///
    /// Covers the whole row via [`Selection::line_at`]; drag after this
    /// extends line-wise. Columns of `pos` are ignored.
    pub fn start_line_selection(&mut self, pos: CellPos) {
        self.start_selection_in(SelectionKind::Line, pos);
    }

    /// Starts a rectangular block selection at `pos` in the primary grid
    /// (`Alt` modifier, CTX-0385).
    ///
    /// The anchor and focus start collapsed; drag extends the rectangle.
    /// Text extraction stays rectangular via [`Selection::block_text`].
    pub fn start_block_selection(&mut self, pos: CellPos) {
        self.start_selection_in(SelectionKind::Block, pos);
    }

    /// Starts a selection of `kind` at primary-grid cell `pos`.
    ///
    /// Owner is [`Self::primary_view`]; without one, nothing is installed
    /// (fail closed).
    fn start_selection_in(&mut self, kind: SelectionKind, pos: CellPos) {
        let Some(primary) = self.primary_view else {
            self.drop_selection();
            return;
        };
        self.start_view_selection(primary, kind, pos);
    }

    /// Starts a selection of `kind` owned by `owner` at `pos` (CTX-0803
    /// pointer-press entry point; CTX-1021 scrolled viewport for #1807).
    ///
    /// `pos` is a live-grid cell when the owner is live and a viewport cell
    /// into the `visible_cells` composite when scrolled (see
    /// `frame_cell_to_owner_cell`): clamped and snapped against the matching
    /// snapshot (live vs composite), with word/line expansion in the same
    /// space. A press whose owner resolves to no live grid drops the
    /// selection instead of installing one against the primary grid.
    pub(super) fn start_view_selection(
        &mut self,
        owner: ViewId,
        kind: SelectionKind,
        pos: CellPos,
    ) {
        if self.is_viewport_scrolled(owner) {
            let Some(viewport) = self.viewport_snapshot_for(owner) else {
                self.drop_selection();
                return;
            };
            let clamped = clamp_cell_pos(&viewport, pos);
            let snapped = bitty_ui::snap_to_leading(&viewport, clamped);
            let selection = match kind {
                SelectionKind::Simple | SelectionKind::Block => Selection {
                    anchor: snapped,
                    focus: snapped,
                    kind,
                    active: true,
                },
                SelectionKind::Word => {
                    let range = Selection::word_at(&viewport, snapped);
                    Selection {
                        anchor: range.start,
                        focus: range.end,
                        kind,
                        active: true,
                    }
                }
                SelectionKind::Line => {
                    let range = Selection::line_at(&viewport, snapped.row);
                    Selection {
                        anchor: range.start,
                        focus: range.end,
                        kind,
                        active: true,
                    }
                }
            };
            self.install_selection(owner, selection, Some(snapped), true);
            return;
        }
        let Some(state) = self.live_view_state(owner) else {
            self.drop_selection();
            return;
        };
        let snap = state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        let selection = match kind {
            SelectionKind::Simple | SelectionKind::Block => Selection {
                anchor: snapped,
                focus: snapped,
                kind,
                active: true,
            },
            SelectionKind::Word => {
                let range = Selection::word_at(&snap, snapped);
                Selection {
                    anchor: range.start,
                    focus: range.end,
                    kind,
                    active: true,
                }
            }
            SelectionKind::Line => {
                let range = Selection::line_at(&snap, snapped.row);
                Selection {
                    anchor: range.start,
                    focus: range.end,
                    kind,
                    active: true,
                }
            }
        };
        self.install_selection(owner, selection, Some(snapped), true);
    }

    /// Updates the current selection's focus to `pos` (mouse drag).
    ///
    /// `pos` is a cell in the **owner's** grid when live and a viewport cell
    /// into the scrolled composite when scrolled (CTX-0803 pointer path maps
    /// into the owner's content frame first, so a drag that leaves the
    /// owner's panel clamps at its edge instead of leaking into a sibling
    /// panel (#1433); CTX-1021 maps through the scrollback offset for #1807).
    ///
    /// Kind-aware (CTX-0385): `Simple`/`Block` move the focus; `Word`
    /// re-expands word-wise around the pinned press cell
    /// ([`Selection::word_drag`]); `Line` covers whole lines between the
    /// press row and `pos` ([`Selection::line_drag`]). Word/line expansion
    /// reads the same snapshot the press snapped against (live vs viewport
    /// composite), so a scrolled word drag expands in visible text.
    pub fn update_selection(&mut self, pos: CellPos) {
        let Some(sel) = self.selection_state else {
            return;
        };
        if !sel.dragging {
            return;
        }
        if self.is_viewport_scrolled(sel.owner) {
            let Some(viewport) = self.viewport_snapshot_for(sel.owner) else {
                self.drop_selection();
                return;
            };
            let clamped = clamp_cell_pos(&viewport, pos);
            let snapped = bitty_ui::snap_to_leading(&viewport, clamped);
            let anchor_press = sel.anchor_press.unwrap_or(sel.selection.anchor);
            let next = match sel.selection.kind {
                SelectionKind::Simple | SelectionKind::Block => {
                    let mut next = sel.selection;
                    next.focus = snapped;
                    next.active = true;
                    next
                }
                SelectionKind::Word => Selection::word_drag(&viewport, anchor_press, snapped),
                SelectionKind::Line => Selection::line_drag(&viewport, anchor_press, snapped),
            };
            self.install_selection(sel.owner, next, Some(anchor_press), true);
            return;
        }
        let Some(state) = self.live_view_state(sel.owner) else {
            // Owner lost its grid mid-drag: fail closed rather than extend a
            // range into a grid that is no longer there.
            self.drop_selection();
            return;
        };
        let snap = state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        let anchor_press = sel.anchor_press.unwrap_or(sel.selection.anchor);
        let next = match sel.selection.kind {
            SelectionKind::Simple | SelectionKind::Block => {
                let mut next = sel.selection;
                next.focus = snapped;
                next.active = true;
                next
            }
            SelectionKind::Word => Selection::word_drag(&snap, anchor_press, snapped),
            SelectionKind::Line => Selection::line_drag(&snap, anchor_press, snapped),
        };
        self.install_selection(sel.owner, next, Some(anchor_press), true);
    }

    /// Ends the selection at `pos` (mouse up) and leaves it active for copy.
    ///
    /// `pos` is a cell in the **owner's** grid when live and a viewport cell
    /// when scrolled, like [`Self::update_selection`] (CTX-1021).
    ///
    /// Kind-aware like [`Self::update_selection`]: a press+release without
    /// motion keeps the word/line expansion from the press, while a drag
    /// re-expands to the release cell.
    ///
    /// A release far from the press cell (beyond
    /// [`super::click::MULTI_CLICK_MAX_CELL_DISTANCE`]) was a drag, not a
    /// click: the click chain resets so the next press starts at single
    /// (standard click-vs-drag classification; a double-click word with no
    /// pointer motion still chains to triple).
    pub fn end_selection(&mut self, pos: CellPos) {
        let Some(sel) = self.selection_state else {
            return;
        };
        if !sel.dragging {
            // No drag in flight: the press was consumed by chrome (status
            // bar, scrollbar) or the selection was already committed. A
            // release must not move a committed selection's focus (nor
            // auto-copy the changed text).
            return;
        }
        if self.is_viewport_scrolled(sel.owner) {
            let Some(viewport) = self.viewport_snapshot_for(sel.owner) else {
                self.drop_selection();
                self.click_tracker.reset();
                return;
            };
            let clamped = clamp_cell_pos(&viewport, pos);
            let snapped = bitty_ui::snap_to_leading(&viewport, clamped);
            let anchor_press = sel.anchor_press.unwrap_or(sel.selection.anchor);
            let was_drag = super::click::cell_distance(anchor_press, snapped)
                > super::click::MULTI_CLICK_MAX_CELL_DISTANCE;
            let mut finished = match sel.selection.kind {
                SelectionKind::Simple | SelectionKind::Block => {
                    let mut next = sel.selection;
                    next.focus = snapped;
                    next
                }
                SelectionKind::Word => Selection::word_drag(&viewport, anchor_press, snapped),
                SelectionKind::Line => Selection::line_drag(&viewport, anchor_press, snapped),
            };
            finished.active = false;
            if was_drag {
                self.click_tracker.reset();
            }
            if finished.anchor == finished.focus {
                self.drop_selection();
            } else {
                self.install_selection(sel.owner, finished, None, false);
            }
            return;
        }
        let Some(state) = self.live_view_state(sel.owner) else {
            // Owner lost its grid before the release: fail closed.
            self.drop_selection();
            self.click_tracker.reset();
            return;
        };
        let snap = state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        let anchor_press = sel.anchor_press.unwrap_or(sel.selection.anchor);
        let was_drag = super::click::cell_distance(anchor_press, snapped)
            > super::click::MULTI_CLICK_MAX_CELL_DISTANCE;
        let mut finished = match sel.selection.kind {
            SelectionKind::Simple | SelectionKind::Block => {
                let mut next = sel.selection;
                next.focus = snapped;
                next
            }
            SelectionKind::Word => Selection::word_drag(&snap, anchor_press, snapped),
            SelectionKind::Line => Selection::line_drag(&snap, anchor_press, snapped),
        };
        finished.active = false;
        if was_drag {
            self.click_tracker.reset();
        }
        // Keep zero-length selections as None to avoid empty copies.
        // Note: a double-click word of length > 1 survives (anchor != focus);
        // a delimiter press collapses and clears, matching stream semantics.
        if finished.anchor == finished.focus {
            self.drop_selection();
        } else {
            self.install_selection(sel.owner, finished, None, false);
        }
    }

    /// Ends any in-flight drag without moving the focus cell.
    ///
    /// Used by the pointer paths that lose the pointer (cursor left the
    /// window, no tracked cursor, owner lost its frame): the range stays as
    /// last extended and becomes inactive, exactly like a release in place.
    pub(super) fn end_selection_drag_in_place(&mut self) {
        let Some(sel) = self.selection_state else {
            return;
        };
        if !sel.dragging {
            return;
        }
        let mut selection = sel.selection;
        selection.active = false;
        self.click_tracker.reset();
        self.install_selection(sel.owner, selection, None, false);
    }

    /// Returns selected text for the current selection, if any.
    ///
    /// CTX-0803: read from the **owner's** grid, so a selection made in a
    /// split pane copies that pane's text, never the primary grid's.
    /// CTX-1021 (#1807): when the owner is scrolled into history, read from
    /// the `visible_cells` composite viewport instead of the live grid, so a
    /// drag copies exactly the visible text under the cursor at any scroll
    /// position.
    #[must_use]
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection_state.as_ref()?;
        if self.is_viewport_scrolled(sel.owner) {
            let viewport = self.viewport_snapshot_for(sel.owner)?;
            // Fail closed like the live path when the owner left the layout:
            // `viewport_snapshot_for` already requires a live leaf + grid.
            let text = sel.selection.text(&viewport);
            return if text.is_empty() { None } else { Some(text) };
        }
        let (sel, state) = self.selection_owner_state()?;
        let snap = state.snapshot();
        let text = sel.selection.text(&snap);
        if text.is_empty() { None } else { Some(text) }
    }

    /// Copies the current selection to the system clipboard (via the
    /// Wayland-first platform backend with headless fallback, which
    /// best-effort syncs the primary selection on Linux). Returns the copied
    /// text on success, `None` when no selection exists.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the OS reports an error,
    /// returns `PlatformError::ClipboardOperation` but still updates the
    /// headless buffer so headless tests can observe the value.
    pub fn copy_selection_to_clipboard(
        &mut self,
    ) -> Result<Option<String>, bitty_platform::PlatformError> {
        let Some(text) = self.selection_text() else {
            return Ok(None);
        };
        self.clipboard.set_text(text.clone())?;
        Ok(Some(text))
    }

    /// Best-effort copy that never returns an error (drops system errors).
    pub fn copy_selection_lossy(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        self.clipboard.set_text_lossy(text.clone());
        Some(text)
    }

    /// Current contents of the platform primary (selection) clipboard buffer.
    ///
    /// Headless-first observation seam for tests: on a live Wayland/X11
    /// desktop this mirrors the last primary write through
    /// `bitty-platform::Clipboard`, so unit tests stay deterministic by
    /// forcing the headless clipboard first (`force_headless_clipboard`).
    #[must_use]
    pub fn primary_contents(&self) -> &str {
        self.clipboard.primary_contents()
    }

    /// Last clipboard failure observed on the mouse-paste path, if any.
    ///
    /// Mouse paste stays fail-soft (no bytes, no panic), but read/write
    /// failures from the platform clipboard are recorded here instead of
    /// swallowed, so the embedder can surface them (PR #259 review). A
    /// subsequent successful clipboard operation clears the slot. Cloned
    /// because [`Runtime`] is not `Sync`-friendly to borrow across frames.
    #[must_use]
    pub fn last_clipboard_error(&self) -> Option<bitty_platform::PlatformError> {
        self.last_clipboard_error.clone()
    }

    /// Records a platform clipboard failure for later surfacing.
    pub(super) fn record_clipboard_error(&mut self, err: bitty_platform::PlatformError) {
        self.last_clipboard_error = Some(err);
    }

    /// Clears the recorded clipboard failure after a successful operation.
    pub(super) fn clear_clipboard_error(&mut self) {
        self.last_clipboard_error = None;
    }

    /// Directly sets the platform primary clipboard (headless test seam).
    ///
    /// Routes through `bitty-platform::Clipboard::set_primary` (Wayland
    /// primary selection where supported, headless buffer otherwise).
    /// Fail-soft: a system error is recorded for
    /// [`Self::last_clipboard_error`] but the headless buffer is still
    /// updated by the platform layer, so headless tests stay deterministic.
    pub fn set_primary_text(&mut self, text: String) {
        if let Err(err) = self.clipboard.set_primary(text) {
            self.record_clipboard_error(err);
        } else {
            self.clear_clipboard_error();
        }
    }

    /// Copies the current selection to the platform primary clipboard.
    /// Returns the copied text, or `None` when no selection exists.
    /// Fail-soft: a system error is recorded for
    /// [`Self::last_clipboard_error`] while the void return keeps the
    /// historic call shape.
    pub fn copy_selection_to_primary(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        if let Err(err) = self.clipboard.set_primary(text.clone()) {
            self.record_clipboard_error(err);
        } else {
            self.clear_clipboard_error();
        }
        Some(text)
    }

    /// Copy-on-select path (CTX-0158): copies the
    /// current selection to the standard clipboard, which the platform layer
    /// best-effort syncs to the primary selection on Linux (CTX-0160).
    /// Returns the copied text, or `None` when no selection exists.
    /// Fail-soft: a system clipboard error is recorded for
    /// [`Self::last_clipboard_error`] while the headless buffers always
    /// update, and headless tests never touch the real clipboard.
    ///
    /// Called automatically on left-release only when
    /// `RuntimeConfig::selection_auto_copy` is `true` (CTX-0191); that opt-in
    /// defaults to `false` (CTX-0371, matching kitty/ghostty). The explicit
    /// `copy_to_clipboard` chord calls the same path regardless of the toggle.
    pub fn auto_copy_selection(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        match self.clipboard.set_text(text.clone()) {
            Ok(()) => self.clear_clipboard_error(),
            Err(err) => self.record_clipboard_error(err),
        }
        Some(text)
    }

    /// Pastes from the platform primary selection (middle-click /
    /// `wl-paste --primary`) through the same suspicious-paste inspection
    /// gate as clipboard input. Returns `None` when the primary selection is
    /// empty, otherwise `Some(true)` when the paste requires confirmation or
    /// `Some(false)` when delivered immediately.
    ///
    /// Fail-soft with a surfaced error: a platform read failure pastes
    /// nothing but is recorded for [`Self::last_clipboard_error`] instead of
    /// swallowed (PR #259 review); a successful read clears the slot.
    ///
    /// An over-limit primary selection is read through the bounded accessor,
    /// so it pastes its clipped prefix instead of failing closed
    /// (CTX-0478 review).
    pub fn paste_from_primary(&mut self) -> Option<bool> {
        let text = match self.clipboard.get_primary_bounded() {
            Ok(text) => {
                self.clear_clipboard_error();
                if self.clipboard.last_bounded_read_truncated() {
                    // Same exactly-once attribution as `paste_from_clipboard`.
                    self.paste_truncated_pastes = self.paste_truncated_pastes.wrapping_add(1);
                }
                text
            }
            Err(err) => {
                self.record_clipboard_error(err);
                return None;
            }
        };
        if text.is_empty() {
            return None;
        }
        Some(self.request_paste(text))
    }

    /// Whether the pending paste requires confirmation, if one exists.
    #[must_use]
    pub fn pending_paste_inspection(&self) -> Option<bool> {
        self.pending_paste
            .as_ref()
            .map(|p| p.inspection.needs_confirmation())
    }

    /// Whether a paste is awaiting confirmation.
    #[must_use]
    pub fn has_pending_paste(&self) -> bool {
        self.pending_paste.is_some()
    }

    /// Current pending paste text, if any.
    #[must_use]
    pub fn pending_paste_text(&self) -> Option<&str> {
        self.pending_paste.as_ref().map(|p| p.text.as_str())
    }

    /// Bounded human-readable summary of the pending paste, if any (CTX-0186,
    /// compacted CTX-0192).
    ///
    /// A gated paste is never silent: while [`Self::has_pending_paste`] holds,
    /// this returns `Some` single line of the form
    /// `Paste 2 lines, 11B [newline] "line1\nline2" (paste again to confirm, Esc cancels)`.
    ///
    /// Multi-line pastes report `newline` (`[newline]`), not the generic `C0`
    /// control class, because LF is the expected multi-line trigger under the
    /// kitty/ghostty safety default (CTX-0369). Only genuinely adversarial
    /// classes name themselves as controls (`NUL`, `ESC`, `CR`, `C0`, ...).
    ///
    /// Bounded and deterministic: the input is already capped at
    /// `CLIPBOARD_MAX_BYTES` (8192), reasons are at most 7 static tokens, and
    /// the preview keeps the first 32 chars escaped (`escape_debug`) and cut
    /// to 48 bytes at a char boundary. Total length stays well under 256
    /// bytes, single-line (no raw `\n`). `O(n)` with `n ≤ 8192`.
    #[must_use]
    pub fn pending_paste_summary(&self) -> Option<String> {
        let pending = self.pending_paste.as_ref()?;
        let lines = pending.text.bytes().filter(|&b| b == b'\n').count() + 1;
        let bytes = pending.text.len();
        let reasons = pending.inspection.reasons().join(", ");
        let preview: String = pending.text.chars().take(32).collect();
        let preview = preview.escape_debug().to_string();
        let preview = truncate_str_to_bytes(&preview, 48);
        Some(format!(
            "Paste {lines} lines, {bytes}B [{reasons}] \"{preview}\" (paste again to confirm, Esc cancels)"
        ))
    }

    /// Whether the banner has collapsed to the minimal flash at `now`
    /// (CTX-0192). `None` when no paste pends.
    #[must_use]
    pub fn paste_banner_collapsed_at(&self, now: std::time::Instant) -> Option<bool> {
        self.pending_paste.as_ref()?;
        let since = self.pending_paste_since?;
        Some(now.saturating_duration_since(since) >= PASTE_BANNER_FULL_DURATION)
    }

    /// Visible banner text at `now` (CTX-0192): compact summary while fresh,
    /// [`PASTE_BANNER_FLASH_TEXT`] after [`PASTE_BANNER_FULL_DURATION`].
    /// Always `Some` while [`Self::has_pending_paste`] holds (never-silent),
    /// bounded, single-line, overlay-only.
    #[must_use]
    pub fn paste_banner_text_at(&self, now: std::time::Instant) -> Option<String> {
        if !self.has_pending_paste() {
            return None;
        }
        match self.paste_banner_collapsed_at(now) {
            Some(true) => Some(PASTE_BANNER_FLASH_TEXT.to_string()),
            _ => self.pending_paste_summary(),
        }
    }

    /// Visible banner text now (CTX-0192). See [`Self::paste_banner_text_at`].
    #[must_use]
    pub fn paste_banner_text(&self) -> Option<String> {
        self.paste_banner_text_at(std::time::Instant::now())
    }

    /// Whether the pending paste should be auto-cancelled at `now` (issue #1438).
    ///
    /// Returns `true` when a paste is pending and has exceeded the configured
    /// `paste_confirm_timeout` duration without confirmation or cancellation.
    /// `None` when no paste pends.
    #[must_use]
    pub fn paste_should_auto_cancel_at(&self, now: std::time::Instant) -> Option<bool> {
        self.pending_paste.as_ref()?;
        let since = self.pending_paste_since?;
        let timeout = self.config.paste_confirm_timeout;
        Some(now.saturating_duration_since(since) >= timeout)
    }

    /// Auto-cancel the pending paste if the timeout has expired at `now` (issue #1438).
    ///
    /// Uses the caller-supplied clock (not `Instant::now()`) so virtual ticks
    /// enforce the deadline consistently with `paste_should_auto_cancel_at`.
    /// Timer expiry clears pending without delivery or the PTY input it would
    /// imply; unlike user cancellation it must not move the viewport (no
    /// `snap_focused_to_live`), so a user reading scrollback stays put.
    /// Returns `true` when a paste was auto-cancelled.
    /// This should be called during presentation/tick to enforce bounded paste-pending state.
    pub fn check_and_auto_cancel_paste_at(&mut self, now: std::time::Instant) -> bool {
        if self.paste_should_auto_cancel_at(now) == Some(true) {
            self.pending_paste = None;
            self.pending_paste_since = None;
            self.paste_banner_collapsed = false;
            self.pending_full_redraw = true;
            true
        } else {
            false
        }
    }

    /// Auto-cancel the pending paste if the timeout has expired (issue #1438).
    ///
    /// Wall-clock shorthand for [`Self::check_and_auto_cancel_paste_at`].
    /// This should be called during presentation/tick to enforce bounded paste-pending state.
    pub fn check_and_auto_cancel_paste(&mut self) -> bool {
        self.check_and_auto_cancel_paste_at(std::time::Instant::now())
    }

    /// Pastes text from the system clipboard (or headless buffer) and routes
    /// it as terminal input via the bounded pending path. Returns
    /// `Err(PlatformError)` when clipboard acquisition fails, `Ok(None)` when
    /// the clipboard is empty, and `Ok(Some(true))` when confirmation is
    /// required or `Ok(Some(false))` when the text is delivered immediately.
    ///
    /// The right-click mouse path records `Err` for
    /// [`Self::last_clipboard_error`] instead of dropping it (PR #259
    /// review); direct callers match on the `Result` themselves.
    ///
    /// Paste inspection (P0-AC-008): every paste is inspected. Multi-line
    /// text (LF) and adversarial controls (NUL/ESC/CR/other C0/C1/Unicode
    /// BiDi and zero-width) require confirmation; other text is delivered
    /// immediately. Text needing confirmation is stored as a pending paste —
    /// `confirm_pending_paste(true)`, repeating the identical paste while
    /// pending (CTX-0186 second chord/right-click press with unchanged
    /// clipboard), or `Esc` to cancel. The pending paste stays visible via
    /// [`Self::pending_paste_summary`]: there is no silent delivery path and
    /// no silent drop. Bracketed paste (`?2004`) is defense-in-depth only and
    /// wraps confirmed delivery when enabled in terminal state.
    ///
    /// Paste is bounded to `CLIPBOARD_MAX_BYTES` (8192) before the scan
    /// (T-01), so untrusted clipboard content cannot grow the heap without
    /// limit. The clipboard read is bounded on purpose: an over-limit system
    /// clipboard is clipped at a UTF-8 char boundary and pastes its prefix
    /// instead of propagating `ClipboardPayloadTooLarge` and pasting nothing
    /// (CTX-0478 review); the inspection gate re-applies the same
    /// char-boundary bound via `truncate_paste_text`.
    pub fn paste_from_clipboard(&mut self) -> Result<Option<bool>, bitty_platform::PlatformError> {
        let text = self.clipboard.get_text_bounded()?;
        if self.clipboard.last_bounded_read_truncated() {
            // The platform layer clipped an over-limit system value: attribute
            // the truncation here (`request_paste` sees an in-cap string and
            // must not count again).
            self.paste_truncated_pastes = self.paste_truncated_pastes.wrapping_add(1);
        }
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.request_paste(text)))
    }

    /// Pastes from a given string via the inspection gate (headless helper).
    /// Returns `true` when the submitted paste requires confirmation and
    /// `false` when it is delivered immediately. Re-submitting the identical
    /// pending text confirms and delivers (CTX-0186); different
    /// confirmation-requiring content while pending preserves the first paste
    /// and returns `true`.
    pub fn paste_text_via_gate(&mut self, text: String) -> bool {
        self.request_paste(text)
    }

    /// Pastes the given text through the suspicious-paste inspection gate.
    ///
    /// This string-input seam is safe for production callers because it uses
    /// the same pending confirmation path as clipboard input.
    pub fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.request_paste(text.to_owned());
    }

    /// Core paste entry: bounds and inspects `text`, stores a pending paste
    /// when confirmation is required (multi-line or adversarial controls),
    /// otherwise delivers immediately. Returns `true` when confirmation is
    /// required and `false` when delivery is immediate. A different
    /// confirmation-requiring request while another paste is pending is
    /// rejected, which preserves the first pending paste for explicit
    /// confirmation or cancel.
    ///
    /// CTX-0186 explicit repeat-to-confirm: re-submitting the identical
    /// (post-truncation) text while it is pending is the user's confirmation
    /// gesture — the second chord/right-click press with an unchanged
    /// clipboard delivers (bracketed when `?2004` is on) and clears pending.
    /// Different content while pending preserves the first paste (TOCTOU-safe:
    /// a swapped clipboard cannot smuggle new bytes through confirmation).
    ///
    /// No silent delivery path exists for `needs_confirmation() == true`.
    pub fn request_paste(&mut self, text: String) -> bool {
        // Issue #1438: an expired pending paste must never deliver. A stale
        // confirmation (repeat chord hours later, or a confirm after the
        // deadline passed with no tick in between) clears first so neither
        // the repeat-paste path below nor `confirm_pending_paste(true)` can
        // write terminal input from an expired gate.
        self.check_and_auto_cancel_paste();
        // CTX-0243: paste is typing — snap to live so the pending banner and
        // the eventual echo land on the visible window (delivery via
        // `write_input` also snaps; explicit here for the pending-confirm path
        // with no bytes yet).
        self.snap_focused_to_live();
        // R-004 truncated-paste telemetry (CTX-0641): string seams reach the
        // gate unclipped, so a clip here is the paste's truncation. Clipboard
        // seams arrive pre-clipped from the bounded read (already counted by
        // the caller), so this stays exactly-once per paste.
        let original_len = text.len();
        let text = truncate_paste_text(text);
        if text.len() < original_len {
            self.paste_truncated_pastes = self.paste_truncated_pastes.wrapping_add(1);
        }
        // Explicit confirmation: identical re-paste while pending delivers.
        if let Some(pending) = self.pending_paste.as_ref() {
            if pending.text == text {
                let pending = self.pending_paste.take().expect("checked above");
                self.deliver_paste_bytes_bracketed(&pending.text);
                self.pending_paste_since = None;
                self.paste_banner_collapsed = false;
                self.pending_full_redraw = true;
                return false;
            }
        }
        let inspection = crate::paste::inspect_paste(&text);
        if inspection.needs_confirmation() {
            if self.pending_paste.is_some() {
                return true;
            }
            self.pending_paste = Some(crate::paste::PendingPaste::new(text, inspection.clone()));
            // CTX-0192 transient banner starts full now.
            self.pending_paste_since = Some(std::time::Instant::now());
            self.paste_banner_collapsed = false;
            self.pending_full_redraw = true;
            return true;
        }
        self.deliver_paste_bytes(text.as_bytes());
        false
    }

    /// Confirm or cancel the pending paste. `confirm == true` delivers the
    /// pending text (bracketed when `?2004` is enabled); `false` drops it.
    ///
    /// Returns `true` when a pending paste existed and was handled.
    pub fn confirm_pending_paste(&mut self, confirm: bool) -> bool {
        // Issue #1438: never deliver an expired gate — clear it first.
        if confirm {
            self.check_and_auto_cancel_paste();
        }
        // CTX-0243: confirming/cancelling is user intent — snap to live
        // (confirm delivers via `write_input` which also snaps; cancel has
        // no bytes so needs the explicit snap).
        self.snap_focused_to_live();
        let Some(pending) = self.pending_paste.take() else {
            return false;
        };
        if confirm {
            self.deliver_paste_bytes_bracketed(&pending.text);
        }
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        true
    }

    /// Cancel any pending paste without delivery.
    pub fn cancel_pending_paste(&mut self) -> bool {
        self.confirm_pending_paste(false)
    }

    /// Scoped `Esc` routing: consume the press only for a real confirmation
    /// gate (CTX-0186 paste, CTX-0257 workspace close, CTX-0370 view/window
    /// close) or an active pointer gesture with advisory-only state
    /// (CTX-1070 Mod+drag tiled move).
    ///
    /// Returns `true` when an `Esc` press cancelled at least one pending
    /// **confirmation gate** or the active tiled-drag gesture: the gate or
    /// gesture is dropped without delivery, a redraw
    /// is requested so any pending indicator clears, and the caller must not
    /// forward the key to the PTY (a dismissal must not also drive shell/vim
    /// state on the gate it just aborted). Returns `false` otherwise (not
    /// `Esc`, not a press, or only the informational CTX-0265 help overlay).
    ///
    /// CTX-0475 (issue #756): the help popup is informational, not modal, so
    /// its `Esc` dismissal no longer consumes the press — a fullscreen app
    /// (vim/less) keeps receiving `Esc` instead of having its mode state
    /// desynced by the overlay. All pending gates still drop together when
    /// several pend (loud, no partial state).
    pub(super) fn cancel_pending_on_escape(&mut self, event: &KeyEvent) -> bool {
        if event.state != PressState::Pressed {
            return false;
        }
        if !matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(bitty_platform::NamedKey::Escape)
        ) {
            return false;
        }
        let mut gate_cancelled = false;
        // CTX-1070 (issue #1811): Esc cancels an active Mod+drag tiled move
        // without committing. The preview never mutates the tree, so the
        // cancel restores the pre-drag layout byte-identically (no
        // re-parent, no selection, no residue) and the press is consumed —
        // the gesture owned the pointer, so the dismissal must not also
        // drive shell/vim state. Live-mutating gestures (Alt+drag float
        // moves, border-drag resizes) commit continuously and have no
        // no-op cancel; only the advisory-preview tiled drag cancels here.
        // The transient pointer gesture owns Esc before the confirmation
        // gates below (a second Esc press reaches them).
        if self.tiled_drag.is_some() {
            self.cancel_tiled_drag();
            return true;
        }
        // CTX-0370: Esc cancels a pending view/window close confirmation
        // (the close itself is aborted; nothing is torn down).
        if self.pending_close_confirm.is_some() {
            self.snap_focused_to_live();
            gate_cancelled = self.cancel_pending_close_confirm();
        }
        if self.pending_ws_close.is_some() {
            // CTX-0243: Esc-cancel is user intent — snap to live (key handler
            // already snapped; idempotent).
            self.snap_focused_to_live();
            gate_cancelled = self.cancel_pending_ws_close() || gate_cancelled;
        }
        // CTX-0475: the help popup is informational, not a confirmation gate.
        // Dismiss it, but never consume the `Esc`: the press still routes to
        // the focused PTY so a fullscreen app's mode state stays in sync.
        if self.help_visible {
            self.snap_focused_to_live();
            self.dismiss_help();
        }
        if self.pending_paste.is_none() {
            return gate_cancelled;
        }
        // CTX-0186/CTX-0243: the paste gate is a real confirmation gate;
        // cancelling it consumes the press (the `Esc` is never delivered).
        self.snap_focused_to_live();
        self.pending_paste = None;
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        self.inspect_ring.push_key(
            &key_inspect_label(event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            Some(true),
        );
        self.publish_inspect_snapshot();
        true
    }

    /// Scoped `Ctrl+D` routing (issue #1336): dismiss a pending paste
    /// without delivery, mirroring [`Self::cancel_pending_on_escape`].
    ///
    /// `Ctrl+D` normally exits (shell EOF via `0x04`); while a paste pends
    /// the byte must not reach the PTY and the shell must not exit behind
    /// the banner — the press drops the pending paste and is consumed.
    /// Only the paste gate is affected: close-confirm arms are untouched
    /// (`Esc` remains their cancel gesture), and with no paste pending the
    /// key encodes normally. Any `Ctrl+D` shape dismisses (shift/alt
    /// variants included): the exit-intent gesture wins while the banner
    /// shows. Copy/search modes and IME preedit own the keyboard above the
    /// caller, so this never fires there.
    ///
    /// Returns `true` when a pending paste was dropped (caller consumes the
    /// press); `false` otherwise.
    pub(super) fn cancel_pending_paste_on_ctrl_d(&mut self, event: &KeyEvent) -> bool {
        if event.state != PressState::Pressed {
            return false;
        }
        if self.pending_paste.is_none() {
            return false;
        }
        if !self.control_pressed {
            return false;
        }
        if !matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Character(text) if text.eq_ignore_ascii_case("d")
        ) {
            return false;
        }
        self.snap_focused_to_live();
        self.pending_paste = None;
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        self.inspect_ring.push_key(
            &key_inspect_label(event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            Some(true),
        );
        self.publish_inspect_snapshot();
        true
    }

    pub(super) fn deliver_paste_bytes(&mut self, bytes: &[u8]) {
        self.write_input(bytes);
    }

    pub(super) fn deliver_paste_bytes_bracketed(&mut self, text: &str) {
        // CTX-0532: bracketed-paste wrapping follows the focused pane's own
        // mode register (primary fallback for session-less leaves) — the
        // same context the bytes route to, so a focus change with no pump
        // between panes can never wrap with the previous pane's mode.
        let bracketed = self.focused_modes().bracketed_paste;
        let bytes = crate::paste::bracketed_wrap(text, bracketed);
        self.write_input(&bytes);
    }

    /// Selects all cells of the focused View's grid (Ctrl+Shift+A /
    /// triple-click equivalent).
    ///
    /// CTX-0803: the selection is owned by the focused View and covers that
    /// View's own grid, so select-all in a split pane selects that pane, not
    /// the primary grid. A focused leaf that owns no grid (session-less,
    /// non-primary) selects nothing (fail closed).
    pub fn select_all(&mut self) {
        let Some(owner) = self.focused_view() else {
            self.drop_selection();
            return;
        };
        let Some(state) = self.live_view_state(owner) else {
            self.drop_selection();
            return;
        };
        let snap = state.snapshot();
        if snap.width == 0 || snap.height == 0 {
            self.drop_selection();
            return;
        }
        let start = CellPos::new(0, 0);
        let end = CellPos::new((snap.height - 1) as u16, (snap.width - 1) as u16);
        let sel = Selection {
            anchor: start,
            focus: bitty_ui::snap_to_leading(&snap, end),
            kind: SelectionKind::Simple,
            active: false,
        };
        self.install_selection(owner, sel, None, false);
    }

    /// Owned clipboard handle (mutable) for advanced use (e.g. OSC 52 tests).
    pub fn clipboard_mut(&mut self) -> &mut Clipboard {
        &mut self.clipboard
    }

    /// Owned clipboard handle (read-only).
    #[must_use]
    pub fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// Forces the clipboard into headless mode (test helper, deterministic).
    ///
    /// Replaces the handle (clearing both the standard and primary headless
    /// buffers) and drops any recorded clipboard error, so tests start from
    /// a clean seam and never touch the real clipboard or primary.
    pub fn force_headless_clipboard(&mut self) {
        self.clipboard = Clipboard::new_headless();
        self.last_clipboard_error = None;
    }

    /// Allow or deny OSC 52 clipboard writes (capability-gated, default false).
    pub fn set_osc_clipboard_write_allowed(&mut self, allowed: bool) {
        self.osc_clipboard_write_allowed = allowed;
    }

    /// Allow or deny OSC 52 clipboard reads / queries (consent-gated, default false).
    pub fn set_osc_clipboard_read_allowed(&mut self, allowed: bool) {
        self.osc_clipboard_read_allowed = allowed;
    }

    /// Whether OSC 52 writes are currently allowed.
    #[must_use]
    pub fn osc_clipboard_write_allowed(&self) -> bool {
        self.osc_clipboard_write_allowed
    }

    /// Whether OSC 52 reads are currently allowed (consent-gated).
    #[must_use]
    pub fn osc_clipboard_read_allowed(&self) -> bool {
        self.osc_clipboard_read_allowed
    }

    /// Count of OSC 52 writes rejected for invalid base64 (CTX-0212).
    ///
    /// Monotonic (wrapping); each rejection leaves the clipboard unchanged
    /// and emits a loud `eprintln!` warn.
    #[must_use]
    pub fn osc52_rejected_writes(&self) -> u64 {
        self.osc52_rejected_writes
    }

    /// Count of pastes clipped to `CLIPBOARD_MAX_BYTES` (R-004, CTX-0641).
    ///
    /// Truncated-paste telemetry: exactly one increment per paste whose
    /// payload exceeded the 8192-byte post-acquisition bound — counted at
    /// the platform bounded read for clipboard seams and at the inspection
    /// gate for string seams. Monotonic (wrapping); confirm/cancel never
    /// change it.
    #[must_use]
    pub fn paste_truncated_pastes(&self) -> u64 {
        self.paste_truncated_pastes
    }
}
