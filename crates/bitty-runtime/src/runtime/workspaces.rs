//! App workspaces: named layout slots with MRU order and kill-confirm close.
//!
//! CTX-0257 workspace ops entry per DEC-0034 (tiling-first, no float mode).
//! CTX-0259 move-focused-window-to-workspace-N per DEC-0034 follow-through
//! (`Mod+Shift+Number` / `ctl workspace move ws:N`).
//! A workspace is a named [`LayoutNode`] + [`Focus`](bitty_ui::Focus) slot.
//! The live `Runtime::layout`/`Runtime::focus` always mirror the active slot;
//! every switch stashes the live pair into the outgoing slot first, so
//! inactive slots are always fresh and no per-mutation sync hook is needed
//! (`layout_mut` escapes make hook-based sync unsound).
//!
//! Invariants: at least one slot always exists (closing the last workspace
//! resets it to a fresh idle leaf, never strands empty); `active_workspace`
//! is always in range; `workspace_mru` holds each live index exactly once
//! with the active index fronted. Pane sessions stay keyed globally by
//! [`ViewId`](bitty_ui::ViewId), so [`ViewId`]s are unique across slots
//! (fresh ids take the max over every slot + 1).
//!
//! Close discipline (never silent kill): a workspace whose leaves own pane
//! sessions is "live" and never closes on one gesture. The first
//! [`Runtime::workspace_close_request`] arms a pending confirm (loud
//! summary + overlay banner); repeating the chord confirms the kill,
//! `Esc` cancels, switching workspaces keeps the pending arm (it names its
//! workspace). Idle workspaces (no pane sessions — a fresh shell on the
//! runtime-global primary counts as idle) close immediately. The primary
//! PTY is runtime-global, not workspace-owned, so closing never tears it
//! down; primary-shell teardown on workspace close is a follow-up. A close
//! that destroys the primary owner re-homes the grid onto the loaded slot's
//! focused leaf and drains that leaf's pending restore into the primary grid
//! (CTX-0501), so no pending entry is stranded without a spawn path.
//!
//! Move discipline (CTX-0259, never a kill): [`Runtime::workspace_move_focused_to`]
//! reparents the focused leaf (with its pane session, untouched) into the
//! target slot. Same-workspace is a no-op; unknown targets fail closed with
//! state untouched. A single-leaf source leaves a fresh idle leaf behind so
//! no workspace ever strands empty and the `>= 1` workspace invariant holds
//! without removing a slot. The pending close arm (if any) is preserved
//! untouched — kill-confirm stays with close, not move. The primary PTY is
//! never touched.
//!
//! Resize/focus-follows-mouse/Alt-drag are follow-ups, not this task.
//! All bounds mirror the registry (`MAX_WORKSPACES_PER_WINDOW` = 16);
//! rendering is the pure [`Runtime::workspaceline_text`] overlay string,
//! never grid truth.

use super::*;

use std::collections::VecDeque;

/// Maximum workspaces (mirrors `registry::MAX_WORKSPACES_PER_WINDOW`).
pub const MAX_WORKSPACES: usize = 16;

/// Maximum workspace name characters shown in the workspaceline.
pub const WORKSPACE_NAME_MAX_CHARS: usize = 32;

/// Hard bound on the rendered workspaceline string.
pub const WORKSPACELINE_MAX_CHARS: usize = 1024;

/// Height of the in-grid status bar in terminal rows (issue #1349).
///
/// The bar occupies exactly the last content row of each rendered leaf.
/// Terminal content behind it is occluded, never mutated: the overlay
/// applies to the owned present snapshot copy, never to grid truth.
pub const STATUS_BAR_ROWS: usize = 1;

/// One workspace: name plus stashed layout + focus.
#[derive(Debug, Clone)]
pub struct WorkspaceSlot {
    /// Creation sequence (stable identity across index shifts).
    pub seq: u64,
    /// Display name (`ws{seq}` until rename lands as a follow-up).
    pub name: String,
    /// Stashed layout (fresh while this slot is inactive).
    pub layout: LayoutNode,
    /// Stashed focus (fresh while this slot is inactive).
    pub focus: Focus,
}

/// A close awaiting explicit confirmation (kill-confirm gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWsClose {
    /// Slot index the arm belongs to.
    pub index: usize,
    /// Workspace name at arm time (banner stays truthful).
    pub name: String,
    /// Live pane sessions counted at arm time.
    pub live: usize,
}

/// Outcome of [`Runtime::workspace_close_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsCloseRequest {
    /// Closed immediately (`killed` pane sessions torn down, 0 when idle).
    Closed {
        /// Pane sessions killed by this close.
        killed: usize,
    },
    /// Armed a pending confirm (workspace is live); `summary` is the loud
    /// banner text. Repeat the chord to confirm, `Esc` to cancel.
    Pending {
        /// Loud one-line banner (`Close workspace ... confirm ...`).
        summary: String,
    },
}

/// Truncate a workspace name to [`WORKSPACE_NAME_MAX_CHARS`] at a char boundary.
fn truncate_ws_name(name: &str) -> String {
    if name.chars().count() <= WORKSPACE_NAME_MAX_CHARS {
        return name.to_string();
    }
    name.chars().take(WORKSPACE_NAME_MAX_CHARS).collect()
}

/// Remove leaf `id` from `node`, promoting its sibling (tiling-first close
/// semantics, mirroring the chrome helper). Returns the removed
/// [`View`](bitty_ui::View) on success, `None` when missing or when `node`
/// is a single leaf (the caller owns the empty-source policy).
fn remove_leaf_view(node: &mut LayoutNode, id: ViewId) -> Option<View> {
    match node {
        LayoutNode::Leaf(_) => None,
        LayoutNode::Split { first, second, .. } => {
            if matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == id) {
                let removed = match first.as_ref() {
                    LayoutNode::Leaf(v) => v.clone(),
                    _ => return None,
                };
                let sibling = (**second).clone();
                *node = sibling;
                Some(removed)
            } else if matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == id) {
                let removed = match second.as_ref() {
                    LayoutNode::Leaf(v) => v.clone(),
                    _ => return None,
                };
                let sibling = (**first).clone();
                *node = sibling;
                Some(removed)
            } else if let Some(v) = remove_leaf_view(first, id) {
                Some(v)
            } else {
                remove_leaf_view(second, id)
            }
        }
        LayoutNode::Stack(children) => {
            if let Some(pos) = children
                .iter()
                .position(|c| matches!(c, LayoutNode::Leaf(v) if v.id() == id))
            {
                if children.len() <= 1 {
                    return None;
                }
                match children.remove(pos) {
                    LayoutNode::Leaf(v) => Some(v),
                    _ => None,
                }
            } else {
                children.iter_mut().find_map(|c| remove_leaf_view(c, id))
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            remove_leaf_view(base, id).or_else(|| remove_leaf_view(overlay, id))
        }
    }
}

impl Runtime {
    /// Seed the slot table from the constructed live layout + focus.
    ///
    /// Called once by each constructor: the fresh single-leaf layout becomes
    /// `ws1` (active). Total; never fails.
    pub(super) fn init_workspaces(&mut self) {
        // Seed from the live pair (clone: the live layout/focus stay
        // authoritative for the active slot until the first switch-away).
        self.workspaces = vec![WorkspaceSlot {
            seq: 1,
            name: String::from("ws1"),
            layout: self.layout.clone(),
            focus: self.focus.clone(),
        }];
        self.active_workspace = 0;
        self.workspace_mru = VecDeque::from([0]);
        self.pending_ws_close = None;
        self.next_workspace_seq = 2;
        // CTX-0536 (#923): seed the monotonic id high-water from the initial
        // leaf so the very first allocation can never reuse id 1.
        self.raise_view_id_high_water();
    }

    /// Number of workspaces (`>= 1` by invariant).
    #[must_use]
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    /// Active workspace index (0-based; display is 1-based).
    #[must_use]
    pub fn active_workspace_index(&self) -> usize {
        self.active_workspace
    }

    /// Workspace names in index order.
    #[must_use]
    pub fn workspace_names(&self) -> Vec<String> {
        self.workspaces.iter().map(|s| s.name.clone()).collect()
    }

    /// Stable creation sequence (`ws{seq}` identity) of slot `index`.
    ///
    /// CTX-0322: the `ctl` surface addresses a workspace by this stable
    /// sequence, never by the positional (1-based) display index, so an id
    /// reported by `workspace new`/`list` stays valid through close+new.
    #[must_use]
    pub fn workspace_seq_at(&self, index: usize) -> Option<u64> {
        self.workspaces.get(index).map(|s| s.seq)
    }

    /// Resolve a stable workspace sequence to its current slot index.
    ///
    /// CTX-0322: `None` when no slot carries `seq`; callers map that to
    /// `NotFound` rather than addressing an unrelated workspace.
    #[must_use]
    pub fn workspace_index_by_seq(&self, seq: u64) -> Option<usize> {
        self.workspaces.iter().position(|s| s.seq == seq)
    }

    /// Minimal tabline render: names + indices + focused marker + count.
    ///
    /// Pure overlay string, never grid truth: `1:ws1* 2:ws2 (2)` — each
    /// slot renders as `{1-based}:{name}` with `*` on the active one, plus
    /// ` ({count})`. Bounded ([`WORKSPACELINE_MAX_CHARS`]), deterministic,
    /// headless-pinned. Updates are trivial: it reads live state, so every
    /// switch/new/close is reflected on the next call.
    #[must_use]
    pub fn workspaceline_text(&self) -> String {
        let mut out = self.workspaceline_tokens().join(" ");
        out.push_str(&format!(" ({})", self.workspaces.len()));
        if out.len() <= WORKSPACELINE_MAX_CHARS {
            return out;
        }
        let mut end = WORKSPACELINE_MAX_CHARS;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out
    }

    /// One rendered token per workspace slot (`{1-based}:{name}[*]`), in
    /// index order. Shared by [`Self::workspaceline_text`] and
    /// [`Self::workspaceline_hit_test`] so the bar text and its click
    /// columns can never drift apart.
    fn workspaceline_tokens(&self) -> Vec<String> {
        self.workspaces
            .iter()
            .enumerate()
            .map(|(idx, slot)| {
                let mark = if idx == self.active_workspace {
                    "*"
                } else {
                    ""
                };
                format!("{}:{}{}", idx + 1, truncate_ws_name(&slot.name), mark)
            })
            .collect()
    }

    /// Whether the workspace switcher bar presents (issue #1333).
    ///
    /// Default-on: seeded from [`crate::config::RuntimeConfig::workspaceline_visible`]
    /// at construction. Presentation-only; toggling changes no workspace,
    /// focus, or session state.
    #[must_use]
    pub fn workspaceline_visible(&self) -> bool {
        self.workspaceline_visible
    }

    /// Live-toggle the switcher bar (opt-out path for `workspace.show_bar`).
    /// Presentation-only; always succeeds.
    pub fn set_workspaceline_visible(&mut self, visible: bool) {
        self.workspaceline_visible = visible;
        self.pending_full_redraw = true;
    }

    /// The bar string as chrome should present it, or `None` when opted
    /// out. `Some` on the default config (bar renders by default).
    #[must_use]
    pub fn workspaceline_present(&self) -> Option<String> {
        if self.workspaceline_visible {
            Some(self.workspaceline_text())
        } else {
            None
        }
    }

    /// StatusBar text as chrome should present it, or `None` when opted
    /// out (issue #1349).
    ///
    /// v1 composes the `workspace` module only (status-system design:
    /// event-driven, fail-closed em-dash when no workspace slot exists).
    /// The `cwd`/`git`/`clock`/metrics slots compose here once their
    /// snapshots exist; `--safe` needs no stripping because no
    /// configuration-dependent module is composed yet.
    #[must_use]
    pub fn status_bar_text(&self) -> Option<String> {
        if !self.workspaceline_visible {
            return None;
        }
        if self.workspaces.is_empty() {
            return Some(String::from("\u{2014}"));
        }
        Some(self.workspaceline_text())
    }

    /// In-grid bar row (0-based) inside a leaf content frame `height_rows`
    /// tall, or `None` when the bar is hidden or the frame has no bar row.
    ///
    /// Shared by the present overlay, the mouse routing, and headless
    /// tests so the drawn row and its click geometry can never drift
    /// apart.
    #[must_use]
    pub fn status_bar_row(&self, height_rows: usize) -> Option<usize> {
        if !self.workspaceline_visible || height_rows < STATUS_BAR_ROWS {
            return None;
        }
        Some(height_rows - STATUS_BAR_ROWS)
    }

    /// Maps a bar column (0-based, in characters of
    /// [`Self::workspaceline_text`]) to a workspace index. `None` when the
    /// bar is hidden, when the column lands on a separator or the trailing
    /// ` (count)` suffix, or when out of range — every unknown target fails
    /// closed with no state change.
    #[must_use]
    pub fn workspaceline_hit_test(&self, column: usize) -> Option<usize> {
        if !self.workspaceline_visible {
            return None;
        }
        let mut start = 0usize;
        for (idx, token) in self.workspaceline_tokens().iter().enumerate() {
            let width = token.chars().count();
            if column >= start && column < start + width {
                return Some(idx);
            }
            // Single-space separator between tokens.
            start += width + 1;
        }
        None
    }

    /// Mouse switching (issue #1333): a click at bar `column` switches to
    /// the hit workspace. Returns `false` (no state change) when the bar is
    /// hidden, the column hits no workspace, or the column names the
    /// already-active workspace — including the single-workspace case, so a
    /// lone workspace can never switch away from itself.
    pub fn workspaceline_click(&mut self, column: usize) -> bool {
        let Some(target) = self.workspaceline_hit_test(column) else {
            return false;
        };
        if target == self.active_workspace {
            return false;
        }
        self.workspace_switch(target)
    }

    /// Routes a left press on the drawn in-grid status bar band to the
    /// workspace hit-test (issue #1349).
    ///
    /// Returns `true` (consume the press) when the last-known cursor sits
    /// inside a drawn bar band: the band geometry reuses the same
    /// [`PresentFrame`](super::layout_focus::PresentFrame) content rects
    /// and [`Self::status_bar_row`] the present overlay paints, so a click
    /// can only land where the bar was drawn. The click itself still fails
    /// closed through [`Self::workspaceline_click`] (separators, the count
    /// suffix, and the active workspace switch nothing); the press is
    /// consumed regardless because the bar row is chrome — the terminal
    /// cells underneath must not start a selection while hidden behind
    /// the bar.
    pub(super) fn status_bar_press(&mut self) -> bool {
        if !self.workspaceline_visible {
            return false;
        }
        let Some(pos) = self.last_cursor else {
            return false;
        };
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return false;
        }
        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return false;
        }
        let pad = f64::from(self.window_padding_physical());
        let cell_w = f64::from(live.width);
        let cell_h = f64::from(live.height);
        for frame in self.present_frames() {
            if frame.cols == 0 || frame.rows == 0 {
                continue;
            }
            let Some(bar) = self.status_bar_row(usize::from(frame.rows)) else {
                continue;
            };
            let origin_x = pad + f64::from(frame.content.x.max(0));
            let origin_y = pad + f64::from(frame.content.y.max(0)) + (bar as f64) * cell_h;
            let band_w = f64::from(frame.cols) * cell_w;
            if pos.x >= origin_x
                && pos.x < origin_x + band_w
                && pos.y >= origin_y
                && pos.y < origin_y + cell_h
            {
                let col = ((pos.x - origin_x) / cell_w).floor() as usize;
                self.workspaceline_click(col);
                return true;
            }
        }
        false
    }

    /// Rename workspace `index` (0-based) to `name`.
    ///
    /// Fail-closed: unknown indices and blank names are refused with state
    /// untouched; overlong names truncate at a char boundary to
    /// [`WORKSPACE_NAME_MAX_CHARS`], mirroring the workspaceline display
    /// bound. Names live on the slots (never stashed), so renaming the
    /// active workspace takes effect on the next
    /// [`Self::workspaceline_text`] call.
    pub fn workspace_rename(&mut self, index: usize, name: &str) -> Result<(), String> {
        if index >= self.workspaces.len() {
            return Err(format!("no such workspace ws:{}", index.saturating_add(1)));
        }
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(String::from("workspace name must not be empty"));
        }
        if let Some(slot) = self.workspaces.get_mut(index) {
            slot.name = truncate_ws_name(trimmed);
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Reposition the focused panel at leaf-order `position` (0-based)
    /// within the active workspace (issue #1333).
    ///
    /// Reuses [`LayoutNode::reparent_leaf`](bitty_ui::LayoutNode::reparent_leaf):
    /// the focused leaf is detached and re-inserted beside the leaf
    /// currently at `position` (tiling-first horizontal 50/50 wrap, the same
    /// wrap [`Self::workspace_move_focused_to`] uses for cross-workspace
    /// moves). Focus stays on the moved panel; pane sessions stay keyed by
    /// [`ViewId`](bitty_ui::ViewId) and are never killed; the pending close
    /// arm (if any) is preserved untouched.
    ///
    /// Fail-closed with state untouched: unknown positions, no focused pane,
    /// a focused pane outside the live layout, or a refused reparent.
    /// Moving to the already-held position is a no-op success.
    pub fn workspace_move_focused_to_position(
        &mut self,
        position: usize,
    ) -> Result<ViewId, String> {
        let ids = self.layout.leaf_ids();
        let focused = self
            .focused_view()
            .ok_or_else(|| String::from("no focused pane to move"))?;
        let current = ids
            .iter()
            .position(|id| *id == focused)
            .ok_or_else(|| String::from("focused pane not in layout"))?;
        if position >= ids.len() {
            return Err(format!(
                "no such panel position {} (workspace holds {})",
                position.saturating_add(1),
                ids.len()
            ));
        }
        if position == current {
            return Ok(focused);
        }
        // CTX-0334: restructuring the live tree abandons any pending hover
        // dwell, mirroring the cross-workspace move.
        self.clear_hover_pending();
        let target = ids[position];
        let after = position > current;
        if !self
            .layout
            .reparent_leaf(focused, target, crate::SplitAxis::Horizontal, 0.5, after)
        {
            return Err(String::from("panel reposition refused"));
        }
        // CTX-0405: boundaries moved in place; re-sync before presenting.
        self.sync_primary_geometry();
        self.sync_pane_geometry();
        self.pending_full_redraw = true;
        Ok(focused)
    }

    /// Fresh [`ViewId`] unique across every slot and the live layout.
    ///
    /// This is the single live-allocation point for leaf ids (CTX-0378):
    /// `pane_sessions` is keyed globally by [`ViewId`], so a split inside one
    /// workspace must never reuse an id another slot owns. Every creation
    /// path (`workspace_new`, the last-slot reset, the ctl split/spawn verbs,
    /// and the keymap `new_split` action) takes its id here; a per-layout
    /// `max + 1` scan is forbidden because it aliases another workspace's
    /// shell.
    ///
    /// Bounded and alias-free: the scan covers the live layout plus at most
    /// [`MAX_WORKSPACES`] stashed slots. While an owner is live its id is
    /// never re-handed (`max + 1`). If the `u64` space were exhausted so that
    /// `max + 1` is impossible, the lowest free id is scanned instead — the
    /// allocator never wraps, saturates, or returns an id a live leaf owns.
    #[must_use]
    pub fn next_view_id_global(&self) -> ViewId {
        let live = self.live_view_raws();
        let live_max = live.iter().copied().max().unwrap_or(0);
        // CTX-0536 (#923): allocate above the monotonic high-water mark, not
        // just above the live maximum, so an id retired by a leaf or
        // workspace close is never re-handed to a fresh owner. `u64`
        // exhaustion is out of scope (the issue states it explicitly); the
        // allocator saturates at `u64::MAX` rather than wrapping or reusing.
        let floor = self.view_id_high_water.max(live_max);
        let id = match floor.checked_add(1) {
            Some(next) => ViewId::new(next),
            None => ViewId::new(u64::MAX),
        };
        debug_assert!(
            !live.contains(&id.0),
            "view id allocator must never hand out a live id"
        );
        id
    }

    /// Raise the monotonic view-id high-water mark to cover every id currently
    /// installed in the live layout or any stashed slot (CTX-0536, #923).
    ///
    /// Called at every layout install/removal funnel so a retired id stays
    /// below the mark and can never be re-issued. Bounded and cheap: it scans
    /// the same live set [`Self::live_view_raws`] already scans.
    pub(super) fn raise_view_id_high_water(&mut self) {
        if let Some(max) = self.live_view_raws().into_iter().max() {
            self.view_id_high_water = self.view_id_high_water.max(max);
        }
    }

    /// Raw ids of every live leaf: the live layout plus all stashed slots.
    fn live_view_raws(&self) -> Vec<u64> {
        let mut raws: Vec<u64> = self.layout.leaf_ids().iter().map(|id| id.0).collect();
        for slot in &self.workspaces {
            raws.extend(slot.layout.leaf_ids().iter().map(|id| id.0));
        }
        raws
    }

    /// Stash the live layout + focus into the active slot (fresh copy).
    fn stash_active_slot(&mut self) {
        if let Some(slot) = self.workspaces.get_mut(self.active_workspace) {
            slot.layout = self.layout.clone();
            slot.focus = self.focus.clone();
        }
    }

    /// Load slot `index` into the live layout + focus.
    fn load_slot(&mut self, index: usize) {
        // CTX-0334: a workspace switch is an explicit focus change; abandon
        // any pending hover dwell (the candidate is per-slot).
        self.clear_hover_pending();
        if let Some(slot) = self.workspaces.get(index) {
            self.layout = slot.layout.clone();
            self.focus = slot.focus.clone();
        }
        // CTX-0536 (#923): loading a slot is a layout install; cover ids that
        // entered through the workspace-new / empty-reset paths.
        self.raise_view_id_high_water();
        // CTX-0405: a slot swap is a layout install that bypasses
        // `replace_layout`; the loaded slot's leaf boundaries may have been
        // stashed before a window resize or reflow, so re-sync the primary
        // grid and every visible pane session to the loaded frames. Hidden
        // sessions in other slots are untouched.
        self.sync_primary_geometry();
        self.sync_pane_geometry();
        // CTX-0532: the loaded slot's focus is a focus transition; attribute
        // the input-mode caches to it before any input can arrive.
        self.sync_mode_caches_to_focus();
    }

    /// Front an index in the MRU (each live index exactly once).
    fn mru_front(&mut self, index: usize) {
        self.workspace_mru.retain(|&i| i != index);
        self.workspace_mru.push_front(index);
    }

    /// Create a fresh workspace and switch to it.
    ///
    /// The new workspace starts as a single leaf. CTX-0359: when the primary
    /// shell attached earlier, the fresh leaf replays that exact program
    /// recipe and owns its own shell immediately (best-effort); without a
    /// recipe it stays session-less, renders empty, and buffers input
    /// headlessly — it never paints or feeds another workspace's shell.
    /// Fail-closed when at capacity ([`MAX_WORKSPACES`]).
    pub fn workspace_new(&mut self) -> Result<usize, String> {
        if self.workspaces.len() >= MAX_WORKSPACES {
            return Err(format!(
                "too many workspaces: max {MAX_WORKSPACES}, current {}",
                self.workspaces.len()
            ));
        }
        // CTX-0343 first match: the fresh `View` lands in the new workspace
        // label as `empty` content. Resolve and check before any state
        // mutation; a violating pair fails the `View` creation closed.
        let fresh_id = self.next_view_id_global();
        let new_label = u8::try_from(self.workspaces.len() + 1)
            .unwrap_or(u8::MAX)
            .clamp(1, MAX_WORKSPACES as u8);
        if let Err(err) = self.validate_view_target_at("empty", new_label, fresh_id) {
            return Err(err.to_string());
        }
        // CTX-0334: creating/switching workspace is an explicit focus change.
        self.clear_hover_pending();
        self.stash_active_slot();
        let seq = self.next_workspace_seq;
        self.next_workspace_seq = seq.wrapping_add(1).max(1);
        let leaf = View::new(fresh_id, self.cols, self.rows);
        let layout = LayoutNode::leaf(leaf);
        let focus = Focus::with_focus(fresh_id);
        self.layout = layout.clone();
        self.focus = focus.clone();
        self.workspaces.push(WorkspaceSlot {
            seq,
            name: format!("ws{seq}"),
            layout,
            focus,
        });
        let index = self.workspaces.len() - 1;
        self.active_workspace = index;
        self.mru_front(index);
        // CTX-0536 (#923): record the fresh id so its later retirement can
        // never fall back below the monotonic high-water mark.
        self.raise_view_id_high_water();
        // CTX-0532: a brand-new slot's leaf starts focused; attribute the
        // input-mode caches to it before any pane spawn/output path runs.
        self.sync_mode_caches_to_focus();
        // CTX-0359: give the fresh workspace leaf a real shell of its own by
        // replaying the primary attach recipe, so its first typed byte can
        // never reach the previous workspace's shell. Best-effort, startup
        // parity: on spawn failure the leaf stays empty and input buffers
        // headless. Skipped when no primary ever attached (headless
        // runtimes): there is no recipe to replay, and the session-less
        // non-owner leaf never paints or feeds the primary.
        if let Some((program, args)) = self.primary_spawn.clone() {
            let (cols, rows) = self
                .present_frames()
                .iter()
                .find(|frame| frame.view == fresh_id)
                .map(|frame| (frame.cols.max(1), frame.rows.max(1)))
                .unwrap_or((
                    self.cols.min(u16::MAX as usize) as u16,
                    self.rows.min(u16::MAX as usize) as u16,
                ));
            let tail: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(err) = self.spawn_shell_for_view(fresh_id, &program, &tail, cols, rows) {
                // Rate-limited (CTX-0473): a broken recipe must not flood stderr
                // as workspaces are created in a loop.
                if let Some(suppressed) = self.spawn_log.admit_now() {
                    eprintln!(
                        "warning: workspace_new pane {fresh_id:?} shell spawn failed ({err}) — workspace {seq} starts empty{}",
                        log_throttle::suppressed_suffix(suppressed)
                    );
                }
            }
        }
        self.pending_full_redraw = true;
        Ok(index)
    }

    /// Switch to workspace `index` (0-based). Unknown indices fail closed
    /// (`false`, state untouched); the active index is a no-op `true`.
    pub fn workspace_switch(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() {
            return false;
        }
        if index == self.active_workspace {
            self.mru_front(index);
            return true;
        }
        self.stash_active_slot();
        self.active_workspace = index;
        self.load_slot(index);
        self.mru_front(index);
        // CTX-0393 (P2-2): lazily respawn restored panes that are still
        // pending — a restored inactive workspace arrives with layout plus
        // history but no live shells, so the first switch here gives each
        // pending leaf its fresh shell (best-effort, primary-recipe replay).
        // Covers every switch path: prev/next/last/focus all funnel through
        // this function. No-op without pending leaves.
        self.spawn_session_pending_for_active();
        self.pending_full_redraw = true;
        true
    }

    /// Switch to the previous workspace (wraps). Returns the new index.
    pub fn workspace_prev(&mut self) -> usize {
        let next = self
            .active_workspace
            .checked_sub(1)
            .unwrap_or_else(|| self.workspaces.len().saturating_sub(1));
        self.workspace_switch(next);
        self.active_workspace
    }

    /// Switch to the next workspace (wraps). Returns the new index.
    pub fn workspace_next(&mut self) -> usize {
        let next = (self.active_workspace + 1) % self.workspaces.len().max(1);
        self.workspace_switch(next);
        self.active_workspace
    }

    /// Switch to the last-used workspace (MRU). Single-workspace is a no-op.
    /// Returns the active index afterwards.
    pub fn workspace_last(&mut self) -> usize {
        let target = self
            .workspace_mru
            .iter()
            .find(|&&i| i != self.active_workspace && i < self.workspaces.len())
            .copied();
        if let Some(index) = target {
            self.workspace_switch(index);
        }
        self.active_workspace
    }

    /// Jump to workspace `one_based` (1-based display index, the `Alt+N`
    /// key path). Issue #1365: `N` beyond the live count clamps to the
    /// last workspace instead of failing; `0` (and an empty slot list,
    /// defended though the invariant keeps `>= 1`) fails closed with
    /// state untouched. Reuses [`Self::workspace_switch`], so every
    /// switch side effect (stash/load, MRU, pending-shell respawn,
    /// redraw) matches the other switch paths. Returns the active index
    /// afterwards, or `None` when the jump refused.
    pub fn workspace_focus_clamped(&mut self, one_based: u64) -> Option<usize> {
        if one_based == 0 || self.workspaces.is_empty() {
            return None;
        }
        let index = ((one_based - 1) as usize).min(self.workspaces.len() - 1);
        self.workspace_switch(index);
        Some(self.active_workspace)
    }

    /// Live pane sessions owned by workspace `index`'s leaves.
    ///
    /// The active index reads the live layout (the active slot copy is stale
    /// by design — stashed only on switch-away). A fresh primary-only shell
    /// counts 0 (idle baseline); the runtime-global primary is never counted
    /// and never killed here.
    #[must_use]
    pub fn workspace_live_count(&self, index: usize) -> usize {
        let ids: Vec<ViewId> = if index == self.active_workspace {
            self.layout.leaf_ids()
        } else {
            match self.workspaces.get(index) {
                Some(slot) => slot.layout.leaf_ids(),
                None => return 0,
            }
        };
        ids.iter()
            .filter(|id| self.pane_sessions.contains_key(id))
            .count()
    }

    /// Kill every pane session owned by workspace `index`'s leaves.
    /// Returns the number torn down.
    fn kill_workspace_sessions(&mut self, index: usize) -> usize {
        let ids: Vec<ViewId> = if index == self.active_workspace {
            self.layout.leaf_ids()
        } else {
            match self.workspaces.get(index) {
                Some(slot) => slot.layout.leaf_ids(),
                None => return 0,
            }
        };
        let mut killed = 0usize;
        for id in ids {
            if self.close_pane_session(&id) {
                killed += 1;
            }
        }
        killed
    }

    /// Remove slot `index`, remap MRU + pending arms, and load the neighbor.
    ///
    /// Removing the last slot resets it to a fresh idle leaf (the layout
    /// never strands empty). The active workspace survives an inactive close
    /// (shifted down when the removed slot sat below it); only removing the
    /// active slot itself loads its neighbor. The caller owns session
    /// teardown.
    fn remove_workspace(&mut self, index: usize) {
        if index >= self.workspaces.len() {
            return;
        }
        // CTX-0414: the active slot's stash is refreshed only on switch-away,
        // so persist the live layout/focus before any slot shuffle. Closing
        // an inactive workspace must not reload the active slot from a stale
        // stash (live leaf edits, geometry, focus, and owners would revert).
        self.stash_active_slot();
        // CTX-0567 (#992): fold every id currently held (live layout plus all
        // stashed slots, including the slot about to be removed) into the
        // monotonic high-water *before* the slot is dropped. Retirement is a
        // boundary: an id installed through the `layout_mut` escape never hit
        // an allocation funnel, so it must be captured on the way out or the
        // last-workspace reset below would reissue it.
        self.raise_view_id_high_water();
        // CTX-0461: leaves destroyed with the workspace can never respawn,
        // so capture them before the removal to drop any pending restores
        // they still hold (stale entries leak and misreport session state).
        let removed_leaves = self.workspaces[index].layout.leaf_ids();
        self.workspaces.remove(index);
        for view in removed_leaves {
            self.session_pending.remove(&view);
            // CTX-0585: a destroyed primary owner can never respawn; drop its
            // captured cwd so the slot does not outlive the leaf.
            if self.session_primary_cwd.as_ref().map(|(owner, _)| *owner) == Some(view) {
                self.session_primary_cwd = None;
            }
        }
        if self.workspaces.is_empty() {
            let fresh_id = self.next_view_id_global();
            let leaf = View::new(fresh_id, self.cols, self.rows);
            let layout = LayoutNode::leaf(leaf);
            let focus = Focus::with_focus(fresh_id);
            let seq = self.next_workspace_seq;
            self.next_workspace_seq = seq.wrapping_add(1).max(1);
            self.workspaces.push(WorkspaceSlot {
                seq,
                name: format!("ws{seq}"),
                layout,
                focus,
            });
        }
        // Remap: drop the removed index, shift higher ones down.
        self.workspace_mru.retain(|&i| i != index);
        for i in self.workspace_mru.iter_mut() {
            if *i > index {
                *i -= 1;
            }
        }
        // A pending arm for the removed workspace dies with it; arms above
        // shift down with their workspace.
        match self.pending_ws_close.take() {
            Some(pending) if pending.index == index => {}
            Some(mut pending) => {
                if pending.index > index {
                    pending.index -= 1;
                }
                self.pending_ws_close = Some(pending);
            }
            None => {}
        }
        // CTX-0414: an inactive close keeps the surviving active workspace
        // (index-shifted when the removed slot sat below it); only removing
        // the active slot itself loads its neighbor.
        let active = if index < self.active_workspace {
            self.active_workspace - 1
        } else if index == self.active_workspace {
            index.min(self.workspaces.len().saturating_sub(1))
        } else {
            self.active_workspace
        };
        self.active_workspace = active;
        self.load_slot(active);
        self.mru_front(active);
        // CTX-0405: removing a slot destroys every leaf it owns, including a
        // primary owner moved into it earlier. Re-home the primary grid to
        // the loaded slot's focused leaf so ownership never dangles on a
        // dead id; an owner still live in another slot is preserved.
        if let Some(owner) = self.primary_view {
            if !self.live_view_raws().contains(&owner.0) {
                self.primary_view = self.focus.focused();
                self.sync_primary_geometry();
                // CTX-0501: the re-homed leaf may still carry a pending
                // restore. Primary ownership supersedes the pane spawn — the
                // hook below skips the owner by design — so drain that
                // history into the primary grid (parity with
                // `rehydrate_pane`'s owner branch and the startup primary
                // attach). Otherwise the entry lingers with no path that can
                // ever spawn it and `session_pending_len` misreports.
                if let Some(new_owner) = self.primary_view {
                    let _ = self.hydrate_session_pending_for(new_owner);
                }
            }
        }
        // CTX-0461 (CTX-0393 P3 follow-up): the close path installs a slot
        // exactly like a switch, so the loaded workspace's still-pending
        // leaves respawn here too (CTX-0393 P2-2 parity). Without this,
        // closing the active workspace would land on a restored workspace
        // with empty panes, and `workspace_switch` early-returns on the
        // already-active index so no later switch could rescue them.
        self.spawn_session_pending_for_active();
        self.pending_full_redraw = true;
    }

    /// Close the active workspace with the kill-confirm gate.
    ///
    /// Idle workspaces (no pane sessions) close immediately. Live ones arm
    /// a pending confirm: repeating the call (second chord press) confirms
    /// the kill, [`Runtime::cancel_pending_ws_close`] / `Esc` cancels, and
    /// a pending arm for another workspace is replaced loudly. Never a
    /// silent kill.
    pub fn workspace_close_request(&mut self) -> WsCloseRequest {
        let index = self.active_workspace;
        // Repeat-to-confirm: a pending arm for THIS workspace confirms.
        if self
            .pending_ws_close
            .as_ref()
            .is_some_and(|p| p.index == index)
        {
            self.pending_ws_close = None;
            let killed = self.kill_workspace_sessions(index);
            self.remove_workspace(index);
            return WsCloseRequest::Closed { killed };
        }
        let live = self.workspace_live_count(index);
        if live == 0 {
            self.pending_ws_close = None;
            let killed = self.kill_workspace_sessions(index);
            self.remove_workspace(index);
            return WsCloseRequest::Closed { killed };
        }
        let name = self
            .workspaces
            .get(index)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let summary = format!(
            "Close workspace {}:{} with {live} live shells? (repeat Alt+W=confirm kill Esc=cancel)",
            index + 1,
            truncate_ws_name(&name),
        );
        self.pending_ws_close = Some(PendingWsClose { index, name, live });
        self.pending_full_redraw = true;
        WsCloseRequest::Pending { summary }
    }

    /// Close the workspace at slot `index` (0-based): immediate, no confirm
    /// gate. Returns sessions killed. Unknown slots fail closed.
    pub fn workspace_close_at(&mut self, index: usize) -> Result<usize, String> {
        if index >= self.workspaces.len() {
            return Err(String::from("no such workspace"));
        }
        let killed = self.kill_workspace_sessions(index);
        self.remove_workspace(index);
        Ok(killed)
    }

    /// Close workspace `one_based` by positional display index (key path).
    /// Unknown indices fail closed.
    pub fn workspace_close_index(&mut self, one_based: u64) -> Result<usize, String> {
        if one_based == 0 {
            return Err(String::from("workspace index starts at 1 (e.g. ws:1)"));
        }
        let index = (one_based - 1) as usize;
        if index >= self.workspaces.len() {
            return Err(format!("no such workspace ws:{one_based}"));
        }
        self.workspace_close_at(index)
    }

    /// Move the focused window (leaf) into workspace `index` (0-based).
    ///
    /// CTX-0259 DEC-0034 follow-through (`Mod+Shift+Number`): reparents the
    /// focused leaf — with its pane session untouched — into the target slot.
    /// Returns the moved [`ViewId`](bitty_ui::ViewId).
    ///
    /// - Same-workspace is a no-op `Ok` (focus preserved).
    /// - Unknown `index` fails closed (`Err`, state untouched).
    /// - No focused pane, or a focused id missing from the live layout,
    ///   fails closed.
    /// - A single-leaf source leaves a fresh idle leaf behind (new
    ///   [`ViewId`](bitty_ui::ViewId), no session), so no workspace ever
    ///   strands empty and the workspace count never drops.
    /// - The target slot gains the moved leaf alongside its existing tree
    ///   (tiling-first horizontal 50/50 wrap) and its focus moves to the
    ///   moved window; the source focus falls back to its first remaining
    ///   leaf. Pane sessions stay keyed globally by [`ViewId`](bitty_ui::ViewId)
    ///   and are never killed here — the runtime-global primary PTY is never
    ///   touched and kill-confirm stays with close (the pending close arm,
    ///   if any, is preserved untouched).
    pub fn workspace_move_focused_to(&mut self, index: usize) -> Result<ViewId, String> {
        if index >= self.workspaces.len() {
            return Err(format!("no such workspace ws:{}", index.saturating_add(1)));
        }
        // CTX-0334: reparenting the focused pane restructures focus; abandon
        // any pending hover dwell.
        self.clear_hover_pending();
        if index == self.active_workspace {
            let focused = self
                .focused_view()
                .ok_or_else(|| String::from("no focused pane to move"))?;
            if self.layout.find_leaf(focused).is_none() {
                return Err(String::from("focused pane not in layout"));
            }
            return Ok(focused);
        }
        let focused = self
            .focused_view()
            .ok_or_else(|| String::from("no focused pane to move"))?;
        let moved_view = self
            .layout
            .find_leaf(focused)
            .cloned()
            .ok_or_else(|| String::from("focused pane not in layout"))?;
        // CTX-0343 first match: the move brings the `View` under the target
        // workspace label; resolve and check before any state mutation.
        let content = self.view_content_kind(focused);
        let target_label = u8::try_from(index + 1)
            .unwrap_or(u8::MAX)
            .clamp(1, MAX_WORKSPACES as u8);
        if let Err(err) = self.validate_view_target_at(content, target_label, focused) {
            return Err(err.to_string());
        }
        // Remove from the live (source) layout. Single-leaf sources leave a
        // fresh idle leaf behind; multi-leaf sources promote the sibling.
        if self.layout.leaf_count() <= 1 {
            let fresh_id = self.next_view_id_global();
            debug_assert_ne!(fresh_id, focused, "fresh id must not collide");
            let fresh = View::new(fresh_id, self.cols, self.rows);
            self.layout = LayoutNode::leaf(fresh);
            self.focus = Focus::with_focus(fresh_id);
            // CTX-0536 (#923): the fresh source leaf is a new id; raise the
            // high-water before the moved leaf's old id can be recycled.
            self.raise_view_id_high_water();
        } else {
            let mut source = self.layout.clone();
            let removed = remove_leaf_view(&mut source, focused)
                .ok_or_else(|| String::from("focused pane not in layout"))?;
            debug_assert_eq!(removed.id(), focused, "removed id must match focus");
            self.layout = source;
            let first = self
                .layout
                .leaf_ids()
                .into_iter()
                .next()
                .ok_or_else(|| String::from("source workspace stranded empty after move"))?;
            self.focus = Focus::with_focus(first);
        }
        // CTX-0532: the source promotion is a focus transition; attribute
        // the input-mode caches before presenting again.
        self.sync_mode_caches_to_focus();
        // Mirror the live source into its slot so stashed copies never hold
        // a duplicate of the moved id (ids stay unique across slots).
        self.stash_active_slot();
        // Insert into the target slot (inactive by the early return above).
        let target_root = self
            .workspaces
            .get(index)
            .map(|s| s.layout.clone())
            .ok_or_else(|| String::from("target workspace vanished"))?;
        let wrapped = LayoutNode::split(
            crate::SplitAxis::Horizontal,
            0.5,
            target_root,
            LayoutNode::leaf(moved_view),
        );
        if let Some(slot) = self.workspaces.get_mut(index) {
            slot.layout = wrapped;
            slot.focus = Focus::with_focus(focused);
        }
        // CTX-0405: the source promotion changed surviving leaf boundaries in
        // place (the moved leaf's own session re-syncs when its target slot
        // is loaded), so re-sync the active source before presenting again.
        self.sync_primary_geometry();
        self.sync_pane_geometry();
        self.pending_full_redraw = true;
        Ok(focused)
    }

    /// Move the focused window to workspace `one_based` (1-based display
    /// index, `key` + `ctl` path). Returns `(moved_id, from_1based, to_1based)`.
    /// Unknown indices fail closed.
    pub fn workspace_move_focused_to_one_based(
        &mut self,
        one_based: u64,
    ) -> Result<(ViewId, usize, usize), String> {
        if one_based == 0 {
            return Err(String::from("workspace index starts at 1 (e.g. ws:1)"));
        }
        let index = (one_based - 1) as usize;
        if index >= self.workspaces.len() {
            return Err(format!("no such workspace ws:{one_based}"));
        }
        let from = self.active_workspace.saturating_add(1);
        let moved = self.workspace_move_focused_to(index)?;
        Ok((moved, from, index.saturating_add(1)))
    }

    /// Confirm a pending close (delivers the kill). `false` when none pends.
    pub fn confirm_pending_ws_close(&mut self) -> bool {
        let Some(pending) = self.pending_ws_close.take() else {
            return false;
        };
        // Confirm targets the armed workspace even if the user switched away
        // (the arm names its workspace; switching keeps it by design).
        let index = pending.index.min(self.workspaces.len().saturating_sub(1));
        let killed = self.kill_workspace_sessions(index);
        let _ = killed;
        self.remove_workspace(index);
        true
    }

    /// Cancel a pending close without killing. `true` when one pended.
    pub(super) fn cancel_pending_ws_close(&mut self) -> bool {
        if self.pending_ws_close.is_none() {
            return false;
        }
        self.pending_ws_close = None;
        self.pending_full_redraw = true;
        true
    }

    /// Whether a workspace close awaits confirmation.
    #[must_use]
    pub fn has_pending_ws_close(&self) -> bool {
        self.pending_ws_close.is_some()
    }

    /// Loud one-line summary of the pending close, if any (never-silent).
    #[must_use]
    pub fn pending_ws_close_summary(&self) -> Option<String> {
        let pending = self.pending_ws_close.as_ref()?;
        Some(format!(
            "Close workspace {}:{} with {} live shells? (repeat Alt+W=confirm kill Esc=cancel)",
            pending.index + 1,
            truncate_ws_name(&pending.name),
            pending.live,
        ))
    }

    /// Overlay banner text for the pending close (steady while pending).
    ///
    /// Overlay-only via the present path (like the paste banner), never grid
    /// truth. Always `Some` while [`Self::has_pending_ws_close`] holds.
    #[must_use]
    pub fn ws_close_banner_text(&self) -> Option<String> {
        if !self.has_pending_ws_close() {
            return None;
        }
        self.pending_ws_close_summary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplitAxis;
    // Only the POSIX-shell live-spawn tests below use this (all
    // `#[cfg(unix)]`); without the gate the import is unused on Windows.
    #[cfg(unix)]
    use bitty_test_support::require_pty;

    fn fresh() -> Runtime {
        Runtime::with_defaults().expect("defaults must build headless")
    }

    #[test]
    fn fresh_runtime_is_one_idle_workspace() {
        let rt = fresh();
        assert_eq!(rt.workspace_count(), 1);
        assert_eq!(rt.active_workspace_index(), 0);
        assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
        assert_eq!(rt.workspaceline_text(), "1:ws1* (1)");
        assert!(!rt.has_pending_ws_close());
        assert_eq!(rt.workspace_live_count(0), 0);
    }

    #[test]
    fn switcher_bar_renders_by_default_with_opt_out() {
        // Issue #1333: the bar is visible on the default config.
        let rt = fresh();
        assert!(rt.workspaceline_visible());
        let presented = rt.workspaceline_present().expect("bar presents by default");
        assert_eq!(presented, "1:ws1* (1)");
        assert_eq!(presented, rt.workspaceline_text());
        // Opt-out hides the present string and blinds hit-testing, with no
        // workspace, focus, or session state change.
        let mut rt = fresh();
        rt.set_workspaceline_visible(false);
        assert!(!rt.workspaceline_visible());
        assert_eq!(rt.workspaceline_present(), None);
        assert_eq!(rt.workspaceline_hit_test(0), None);
        assert_eq!(rt.workspace_count(), 1);
        assert_eq!(rt.active_workspace_index(), 0);
        // Re-enabling restores the bar.
        rt.set_workspaceline_visible(true);
        assert_eq!(rt.workspaceline_present().as_deref(), Some("1:ws1* (1)"));
    }

    #[test]
    fn status_bar_composes_workspace_module_with_shared_row_geometry() {
        // Issue #1349: the composer serves the workspace module by
        // default and hides with the same opt-out; the row helper names
        // the last content row so overlay, mouse, and tests agree.
        let rt = fresh();
        assert_eq!(
            rt.status_bar_text().as_deref(),
            Some("1:ws1* (1)"),
            "workspace module minimum"
        );
        assert_eq!(rt.status_bar_row(24), Some(23));
        assert_eq!(rt.status_bar_row(1), Some(0));
        assert_eq!(rt.status_bar_row(0), None, "no rows means no bar row");
        let mut hidden = fresh();
        hidden.set_workspaceline_visible(false);
        assert_eq!(hidden.status_bar_text(), None);
        assert_eq!(hidden.status_bar_row(24), None);
    }

    #[test]
    fn bar_hit_test_maps_columns_to_workspaces() {
        let mut rt = fresh();
        rt.workspace_new().expect("ws2");
        rt.workspace_new().expect("ws3");
        assert!(rt.workspace_switch(0));
        // Tokens: `1:ws1*` (0..6), sep, `2:ws2` (7..12), sep, `3:ws3`
        // (13..18), then the ` (3)` suffix.
        assert_eq!(rt.workspaceline_text(), "1:ws1* 2:ws2 3:ws3 (3)");
        assert_eq!(rt.workspaceline_hit_test(0), Some(0));
        assert_eq!(rt.workspaceline_hit_test(5), Some(0));
        assert_eq!(rt.workspaceline_hit_test(6), None, "separator fails closed");
        assert_eq!(rt.workspaceline_hit_test(7), Some(1));
        assert_eq!(rt.workspaceline_hit_test(11), Some(1));
        assert_eq!(
            rt.workspaceline_hit_test(12),
            None,
            "separator fails closed"
        );
        assert_eq!(rt.workspaceline_hit_test(13), Some(2));
        assert_eq!(rt.workspaceline_hit_test(17), Some(2));
        assert_eq!(rt.workspaceline_hit_test(18), None, "suffix fails closed");
        assert_eq!(
            rt.workspaceline_hit_test(999),
            None,
            "out of range fails closed"
        );
    }

    #[test]
    fn bar_click_switches_and_fails_closed() {
        let mut rt = fresh();
        rt.workspace_new().expect("ws2");
        assert!(rt.workspace_switch(0));
        // Click on ws2's token switches.
        assert!(rt.workspaceline_click(7));
        assert_eq!(rt.active_workspace_index(), 1);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
        // Clicking the active workspace is a no-op false.
        assert!(!rt.workspaceline_click(7));
        assert_eq!(rt.active_workspace_index(), 1);
        // Separators and out-of-range columns fail closed.
        assert!(!rt.workspaceline_click(6));
        assert!(!rt.workspaceline_click(999));
        assert_eq!(rt.active_workspace_index(), 1);
        // Hidden bar never switches.
        rt.set_workspaceline_visible(false);
        assert!(!rt.workspaceline_click(0));
        assert_eq!(rt.active_workspace_index(), 1);
        // Single workspace: the only column is the active one, so a click
        // can never switch away.
        let mut solo = fresh();
        assert!(!solo.workspaceline_click(0));
        assert_eq!(solo.active_workspace_index(), 0);
    }

    #[test]
    fn alt_n_jump_clamps_to_max_and_fails_closed_on_zero() {
        // Issue #1365: Alt+N jumps to workspace N; N beyond the live
        // count lands on the last workspace; 0 fails closed.
        let mut rt = fresh();
        rt.workspace_new().expect("ws2");
        rt.workspace_new().expect("ws3");
        assert!(rt.workspace_switch(0));
        // Exact jump within range.
        assert_eq!(rt.workspace_focus_clamped(2), Some(1));
        assert_eq!(rt.active_workspace_index(), 1);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* 3:ws3 (3)");
        // Clamp: N beyond the count goes last.
        assert_eq!(rt.workspace_focus_clamped(6), Some(2));
        assert_eq!(rt.active_workspace_index(), 2);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
        assert_eq!(rt.workspace_focus_clamped(9), Some(2));
        assert_eq!(rt.active_workspace_index(), 2);
        // Zero fails closed with state untouched.
        assert_eq!(rt.workspace_focus_clamped(0), None);
        assert_eq!(rt.active_workspace_index(), 2);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
    }

    #[test]
    fn workspace_rename_updates_bar_and_fails_closed() {
        let mut rt = fresh();
        rt.workspace_new().expect("ws2");
        rt.workspace_rename(1, "editor").expect("rename");
        assert_eq!(
            rt.workspace_names(),
            vec![String::from("ws1"), String::from("editor")]
        );
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:editor* (2)");
        // Renaming the active workspace reflects immediately.
        rt.workspace_rename(1, "  docs  ").expect("trims");
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:docs* (2)");
        // Overlong names truncate at the display bound.
        let long = "x".repeat(WORKSPACE_NAME_MAX_CHARS + 10);
        rt.workspace_rename(0, &long).expect("truncates");
        assert_eq!(
            rt.workspace_names()[0].chars().count(),
            WORKSPACE_NAME_MAX_CHARS
        );
        // Unknown index and blank names fail closed with state untouched.
        let before = rt.workspaceline_text();
        assert!(rt.workspace_rename(9, "nope").is_err());
        assert!(rt.workspace_rename(0, "").is_err());
        assert!(rt.workspace_rename(0, "   ").is_err());
        assert_eq!(rt.workspaceline_text(), before);
    }

    #[test]
    fn move_focused_within_reorders_panels() {
        let mut rt = fresh();
        // Deterministic three-pane order [first, second, third].
        let first = rt.focused_view().expect("focus");
        let second = ViewId::new(2);
        let third = ViewId::new(3);
        let leaf1 = rt.layout().find_leaf(first).cloned().expect("leaf1");
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(leaf1),
                LayoutNode::leaf(View::new(second, 40, 24)),
            ),
            LayoutNode::leaf(View::new(third, 40, 24)),
        ));
        assert!(rt.set_focus(first));
        assert_eq!(rt.layout().leaf_ids(), vec![first, second, third]);
        // Move first to position 3 (index 2): order becomes [2nd, 3rd, 1st].
        let moved = rt.workspace_move_focused_to_position(2).expect("move");
        assert_eq!(moved, first);
        assert_eq!(rt.focused_view(), Some(first));
        let ids = rt.layout().leaf_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&first) && ids.contains(&second) && ids.contains(&third));
        assert_eq!(ids[2], first, "focused panel lands at the target position");
        // Same-position move is a no-op success.
        assert_eq!(
            rt.workspace_move_focused_to_position(2).expect("noop"),
            first
        );
        // Unknown positions fail closed with the order untouched.
        let before = rt.layout().leaf_ids();
        assert!(rt.workspace_move_focused_to_position(3).is_err());
        assert!(rt.workspace_move_focused_to_position(99).is_err());
        assert_eq!(rt.layout().leaf_ids(), before);
        assert_eq!(rt.focused_view(), Some(first));
    }

    #[test]
    fn new_switch_prev_next_last_update_tabline() {
        let mut rt = fresh();
        let one = rt.workspace_new().expect("new");
        assert_eq!(one, 1);
        assert_eq!(rt.workspace_count(), 2);
        assert_eq!(rt.active_workspace_index(), 1);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
        // Prev wraps to ws1; next returns; last bounces between the two.
        assert_eq!(rt.workspace_prev(), 0);
        assert_eq!(rt.workspaceline_text(), "1:ws1* 2:ws2 (2)");
        assert_eq!(rt.workspace_next(), 1);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
        assert_eq!(rt.workspace_last(), 0);
        assert_eq!(rt.workspaceline_text(), "1:ws1* 2:ws2 (2)");
        assert_eq!(rt.workspace_last(), 1);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
        // Unknown index fails closed, state untouched.
        assert!(!rt.workspace_switch(9));
        assert_eq!(rt.active_workspace_index(), 1);
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
    }

    #[test]
    fn switch_preserves_per_workspace_layouts() {
        let mut rt = fresh();
        // Split ws1 into two panes, then open ws2 and switch back: the
        // stashed ws1 layout (2 leaves) must survive the round trip.
        let mut layout = rt.layout().clone();
        let focused = rt.focused_view().expect("focus");
        let new_id = ViewId::new(2);
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        rt.set_layout(layout);
        assert_eq!(rt.layout().leaf_count(), 2);
        rt.workspace_new().expect("new");
        assert_eq!(rt.layout().leaf_count(), 1);
        assert!(rt.workspace_switch(0));
        assert_eq!(rt.layout().leaf_count(), 2, "stashed ws1 layout restored");
        assert_eq!(rt.workspaceline_text(), "1:ws1* 2:ws2 (2)");
    }

    #[test]
    fn next_view_id_global_accounts_for_inactive_slots() {
        // CTX-0378: a split in ws1 must never reuse an id an inactive ws2
        // owns. The live ws1 layout is [v:1]; a per-layout max + 1 would
        // return v:2 (ws2's leaf). The global allocator must skip it.
        let mut rt = fresh();
        assert_eq!(rt.next_view_id_global(), ViewId::new(2));
        rt.workspace_new().expect("ws2");
        assert!(rt.layout().leaf_ids().contains(&ViewId::new(2)));
        assert!(rt.workspace_switch(0));
        assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1)]);
        assert_eq!(rt.next_view_id_global(), ViewId::new(3));
        assert!(!rt.layout().leaf_ids().contains(&ViewId::new(3)));
    }

    #[test]
    fn next_view_id_global_is_pure_until_a_leaf_commits() {
        // Allocation itself never mutates state: repeating it before the id
        // is installed in a layout returns the same fresh id (idempotent),
        // so a refused split cannot burn ids or alias later.
        let rt = fresh();
        assert_eq!(rt.next_view_id_global(), ViewId::new(2));
        assert_eq!(rt.next_view_id_global(), ViewId::new(2));
        assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1)]);
    }

    /// CTX-0536 (#923): a closed leaf's id must never be re-handed while a
    /// stale handle (session, snapshot, focus MRU, ctl reply) could still
    /// resolve it. A `max + 1` over *live* ids reuses the id of the highest
    /// leaf as soon as that leaf closes, aliasing the retired owner.
    #[test]
    fn retired_max_view_id_is_not_reissued() {
        let mut rt = fresh();
        // Install views 2 then 3 through the global allocator, splitting the
        // previous leaf each time (the keymap/ctl shape).
        let v2 = rt.next_view_id_global();
        assert_eq!(v2, ViewId::new(2));
        let mut layout = rt.layout().clone();
        let old = layout.find_leaf(ViewId::new(1)).cloned().expect("leaf 1");
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(v2, 80, 24)),
        );
        rt.set_layout(layout);

        let v3 = rt.next_view_id_global();
        assert_eq!(v3, ViewId::new(3));
        let mut layout = rt.layout().clone();
        let old = layout.find_leaf(v2).cloned().expect("leaf 2");
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(v3, 80, 24)),
        );
        rt.set_layout(layout);
        assert!(rt.layout().leaf_ids().contains(&v3));

        // Close v3 (the current max): the live max drops to 2, so a `max + 1`
        // allocator would hand out the retired id 3 again.
        let mut layout = rt.layout().clone();
        let removed = remove_leaf_view(&mut layout, v3).expect("v3 must be closable");
        assert_eq!(removed.id(), v3);
        rt.set_layout_closing(layout, v3);
        assert!(!rt.layout().leaf_ids().contains(&v3));

        let next = rt.next_view_id_global();
        assert_ne!(
            next, v3,
            "a retired view id must never be re-issued to a fresh owner"
        );
        assert_eq!(next, ViewId::new(4));
    }

    /// CTX-0536 (#923): closing an entire workspace retires its ids; a later
    /// allocation must not reuse them even though they are no longer live.
    #[test]
    fn retired_view_ids_across_workspaces_are_not_reissued() {
        let mut rt = fresh();
        rt.workspace_new().expect("ws2");
        let ws2_id = rt.focused_view().expect("ws2 focus");
        assert_eq!(ws2_id, ViewId::new(2));
        // ws2 is idle (no pane session): closing is immediate.
        assert_eq!(
            rt.workspace_close_request(),
            WsCloseRequest::Closed { killed: 0 }
        );
        assert!(!rt.layout().leaf_ids().contains(&ws2_id));
        let next = rt.next_view_id_global();
        assert_ne!(
            next, ws2_id,
            "an id retired by a workspace close must never be re-issued"
        );
        assert_eq!(next, ViewId::new(3));
    }

    #[test]
    fn idle_close_is_immediate_and_last_resets() {
        let mut rt = fresh();
        rt.workspace_new().expect("new");
        rt.workspace_new().expect("new3");
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
        // Active ws3 is idle: closes immediately, neighbor loads.
        assert_eq!(
            rt.workspace_close_request(),
            WsCloseRequest::Closed { killed: 0 }
        );
        assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
        assert!(!rt.has_pending_ws_close());
        // Closing the last workspace resets to a fresh idle leaf.
        assert!(rt.workspace_switch(0));
        assert_eq!(
            rt.workspace_close_request(),
            WsCloseRequest::Closed { killed: 0 }
        );
        assert_eq!(rt.workspace_count(), 1);
        assert_eq!(rt.layout().leaf_count(), 1);
        assert!(!rt.has_pending_ws_close());
    }

    // Issue #1333 live leg: click switching, rename, and within-move
    // with a real shell behind the focused pane. None of the switcher ops
    // may kill or detach the session; the bar follows every op.
    #[test]
    #[cfg(unix)]
    fn live_switcher_click_rename_and_move_preserve_session() {
        require_pty!();
        let mut rt = fresh();
        // Two panes in ws1; the second owns a live shell.
        let live_id = ViewId::new(2);
        let mut layout = rt.layout().clone();
        let focused = rt.focused_view().expect("focus");
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(live_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        rt.set_layout(layout);
        rt.spawn_shell_for_view(live_id, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        assert!(rt.set_focus(live_id));
        rt.workspace_new().expect("new ws2");
        assert!(rt.workspace_switch(0));
        assert!(rt.set_focus(live_id));
        assert_eq!(rt.workspaceline_text(), "1:ws1* 2:ws2 (2)");
        // Click ws2's token (column 7) switches away; the session survives.
        assert!(rt.workspaceline_click(7));
        assert_eq!(rt.active_workspace_index(), 1);
        assert!(
            rt.has_pane_session(&live_id),
            "click must not kill the session"
        );
        // Click back to ws1 (column 0) and rename it.
        assert!(rt.workspaceline_click(0));
        assert_eq!(rt.active_workspace_index(), 0);
        rt.workspace_rename(0, "live").expect("rename");
        assert_eq!(rt.workspaceline_text(), "1:live* 2:ws2 (2)");
        assert!(
            rt.has_pane_session(&live_id),
            "rename must not kill the session"
        );
        // Reposition the live pane within ws1; session and focus follow.
        let moved = rt
            .workspace_move_focused_to_position(0)
            .expect("move within");
        assert_eq!(moved, live_id);
        assert_eq!(rt.focused_view(), Some(live_id));
        assert!(
            rt.has_pane_session(&live_id),
            "move must not kill the session"
        );
        assert!(rt.layout().leaf_ids().contains(&live_id));
    }

    // Live-spawn: runs a real POSIX shell (`/bin/sh` has no Windows
    // equivalent). `#[cfg(unix)]` keeps it off Windows CI; `require_pty!()`
    // keeps the force-no-PTY simulation path. ConPTY coverage lives in
    // bitty-pty/tests/spawn_windows.rs (CTX-0268); porting this test to a
    // platform-neutral spawn is deferred follow-up.
    #[test]
    #[cfg(unix)]
    fn live_close_needs_repeat_confirm_and_kills() {
        require_pty!();
        let mut rt = fresh();
        // Live session: split ws1 and spawn a real shell in the new leaf
        // (headless-safe, same pattern as pane_sessions.rs).
        let new_id = ViewId::new(2);
        let mut layout = rt.layout().clone();
        let focused = rt.focused_view().expect("focus");
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        rt.set_layout(layout);
        rt.spawn_shell_for_view(new_id, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        assert_eq!(rt.workspace_live_count(0), 1);
        // First request arms pending (never silent kill).
        let req = rt.workspace_close_request();
        let summary = match req {
            WsCloseRequest::Pending { summary } => summary,
            WsCloseRequest::Closed { .. } => panic!("live workspace must not close silently"),
        };
        assert!(summary.contains("1 live shells"), "loud count: {summary}");
        assert!(
            summary.contains("Esc=cancel"),
            "cancel path named: {summary}"
        );
        assert!(rt.has_pending_ws_close());
        assert_eq!(rt.workspace_count(), 1, "nothing closed yet");
        assert_eq!(
            rt.pending_ws_close_summary().as_deref(),
            Some(summary.as_str())
        );
        assert_eq!(rt.ws_close_banner_text().as_deref(), Some(summary.as_str()));
        // Esc-cancel path drops the arm with no kill.
        assert!(rt.cancel_pending_ws_close());
        assert!(!rt.has_pending_ws_close());
        assert!(rt.has_pane_session(&new_id), "cancel must not kill");
        // Re-arm, then repeat-to-confirm kills and closes (last resets).
        assert!(matches!(
            rt.workspace_close_request(),
            WsCloseRequest::Pending { .. }
        ));
        assert_eq!(
            rt.workspace_close_request(),
            WsCloseRequest::Closed { killed: 1 }
        );
        assert!(!rt.has_pending_ws_close());
        assert!(
            !rt.has_pane_session(&new_id),
            "confirm must kill the session"
        );
        assert_eq!(rt.workspace_count(), 1);
    }

    // Live-spawn: runs a real POSIX shell (`/bin/sh` has no Windows
    // equivalent). `#[cfg(unix)]` keeps it off Windows CI; `require_pty!()`
    // keeps the force-no-PTY simulation path. ConPTY coverage lives in
    // bitty-pty/tests/spawn_windows.rs (CTX-0268); porting this test to a
    // platform-neutral spawn is deferred follow-up.
    #[test]
    #[cfg(unix)]
    fn pending_arm_survives_switch_and_dies_with_workspace() {
        require_pty!();
        let mut rt = fresh();
        rt.workspace_new().expect("new");
        // Live session in ws2 (active).
        let new_id = ViewId::new(3);
        let mut layout = rt.layout().clone();
        let focused = rt.focused_view().expect("focus");
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        rt.set_layout(layout);
        rt.spawn_shell_for_view(new_id, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        assert!(matches!(
            rt.workspace_close_request(),
            WsCloseRequest::Pending { .. }
        ));
        // Switch away: the arm survives (it names ws2).
        assert!(rt.workspace_switch(0));
        assert!(rt.has_pending_ws_close());
        assert!(
            rt.pending_ws_close_summary()
                .expect("summary")
                .contains("2:ws2")
        );
        // ctl-style immediate close of the armed workspace clears the arm.
        let killed = rt.workspace_close_index(2).expect("close ws2");
        assert_eq!(killed, 1);
        assert!(!rt.has_pending_ws_close());
        assert_eq!(rt.workspaceline_text(), "1:ws1* (1)");
    }

    #[test]
    fn new_fails_closed_at_capacity() {
        let mut rt = fresh();
        for _ in 1..MAX_WORKSPACES {
            rt.workspace_new().expect("new within capacity");
        }
        assert_eq!(rt.workspace_count(), MAX_WORKSPACES);
        assert!(rt.workspace_new().is_err(), "16 is max");
        assert!(rt.workspace_close_index(0).is_err(), "index starts at 1");
        assert!(rt.workspace_close_index(99).is_err(), "unknown index");
    }

    /// WS-INV-6 (follow-up F-4): a workspace slot keeps its stable creation
    /// sequence (`WorkspaceSlot::seq`) across index shifts, and the workspace
    /// MRU holds each live index exactly once with the active index fronted.
    ///
    /// Headless and session-free: every workspace here is idle, so closes
    /// are immediate and only slot/`seq`/MRU movement is pinned.
    #[test]
    fn workspace_seq_stable_across_index_shifts() {
        fn seqs(rt: &Runtime) -> Vec<u64> {
            (0..rt.workspace_count())
                .map(|i| rt.workspace_seq_at(i).expect("every slot carries a seq"))
                .collect()
        }
        fn assert_mru_valid(rt: &Runtime) {
            let n = rt.workspace_count();
            let mru: Vec<usize> = rt.workspace_mru.iter().copied().collect();
            assert_eq!(mru.len(), n, "MRU holds each live index exactly once");
            let mut seen = vec![false; n];
            for &i in &mru {
                assert!(i < n, "MRU index {i} is in range");
                assert!(!seen[i], "MRU index {i} appears exactly once");
                seen[i] = true;
            }
            assert_eq!(
                mru.first(),
                Some(&rt.active_workspace_index()),
                "MRU head is the active workspace"
            );
        }
        fn assert_seq_addrs(rt: &Runtime) {
            for (index, seq) in seqs(rt).iter().enumerate() {
                assert_eq!(rt.workspace_index_by_seq(*seq), Some(index));
                assert_eq!(
                    rt.workspace_names()[index],
                    format!("ws{seq}"),
                    "name tracks the stable seq, never the positional index"
                );
            }
            assert_eq!(rt.workspace_seq_at(rt.workspace_count()), None);
            assert_eq!(rt.workspace_index_by_seq(u64::MAX), None);
        }

        let mut rt = fresh();
        assert_eq!(seqs(&rt), vec![1]);
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // Grow to four slots: seqs allocate monotonically from 1.
        assert_eq!(rt.workspace_new().expect("ws2"), 1);
        assert_eq!(rt.workspace_new().expect("ws3"), 2);
        assert_eq!(rt.workspace_new().expect("ws4"), 3);
        assert_eq!(seqs(&rt), vec![1, 2, 3, 4]);
        assert_eq!(rt.active_workspace_index(), 3);
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // Switches (direct, prev/next, MRU-last) never mutate seqs.
        assert!(rt.workspace_switch(0));
        assert_eq!(seqs(&rt), vec![1, 2, 3, 4]);
        assert_mru_valid(&rt);
        assert_eq!(rt.workspace_next(), 1);
        assert_eq!(seqs(&rt), vec![1, 2, 3, 4]);
        assert_mru_valid(&rt);
        assert_eq!(rt.workspace_prev(), 0);
        assert_eq!(seqs(&rt), vec![1, 2, 3, 4]);
        assert_mru_valid(&rt);
        assert_eq!(rt.workspace_last(), 1);
        assert_eq!(seqs(&rt), vec![1, 2, 3, 4]);
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // Close an inactive slot above the active one: survivors keep their
        // seqs, higher slots shift down, the retired seq resolves nowhere.
        assert!(rt.workspace_switch(0));
        assert_eq!(rt.workspace_close_at(2).expect("close inactive ws3"), 0);
        assert_eq!(seqs(&rt), vec![1, 2, 4]);
        assert_eq!(rt.active_workspace_index(), 0);
        assert_eq!(rt.workspace_index_by_seq(3), None);
        assert_eq!(rt.workspace_index_by_seq(4), Some(2));
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // Close an inactive slot below the active one: the active index
        // shifts down with its slot while every surviving seq is untouched.
        assert!(rt.workspace_switch(2));
        assert_eq!(rt.active_workspace_index(), 2);
        assert_eq!(rt.workspace_close_at(1).expect("close inactive ws2"), 0);
        assert_eq!(seqs(&rt), vec![1, 4]);
        assert_eq!(rt.active_workspace_index(), 1);
        assert_eq!(rt.workspace_index_by_seq(2), None);
        assert_eq!(rt.workspace_index_by_seq(4), Some(1));
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // Close the active slot itself: the neighbor loads and surviving
        // seqs are preserved verbatim.
        assert_eq!(rt.workspace_close_at(1).expect("close active ws4"), 0);
        assert_eq!(seqs(&rt), vec![1]);
        assert_eq!(rt.active_workspace_index(), 0);
        assert_eq!(rt.workspace_index_by_seq(4), None);
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // A fresh workspace after closes takes a new monotonic seq; retired
        // seqs (2, 3, 4) are never reissued.
        let index = rt.workspace_new().expect("ws5");
        assert_eq!(index, 1);
        assert_eq!(seqs(&rt), vec![1, 5]);
        assert_eq!(rt.workspace_index_by_seq(5), Some(1));
        for retired in [2, 3, 4] {
            assert_eq!(rt.workspace_index_by_seq(retired), None);
        }
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);

        // Draining to the last workspace resets it with a fresh monotonic
        // seq rather than reusing a retired one.
        assert_eq!(rt.workspace_close_at(0).expect("close ws1"), 0);
        assert_eq!(seqs(&rt), vec![5]);
        assert_eq!(rt.workspace_close_at(0).expect("reset last ws5"), 0);
        assert_eq!(rt.workspace_count(), 1);
        let reset = seqs(&rt);
        assert_eq!(reset.len(), 1);
        assert!(
            reset[0] > 5,
            "reset seq {} must advance past every retired seq",
            reset[0]
        );
        assert_eq!(rt.workspace_names(), vec![format!("ws{}", reset[0])]);
        assert_mru_valid(&rt);
        assert_seq_addrs(&rt);
    }

    #[test]
    fn move_multi_leaf_preserves_focus_and_tabline() {
        // CTX-0259: two workspaces, ws1 split into two leaves; moving the
        // focused leaf to ws2 reparents it without killing or switching.
        let mut rt = fresh();
        let focused_ws1 = rt.focused_view().expect("focus");
        let moved_id = ViewId::new(2);
        let mut layout = rt.layout().clone();
        let old = layout.find_leaf(focused_ws1).cloned().expect("leaf");
        let fresh_leaf = View::new(moved_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        rt.set_layout(layout);
        assert!(rt.set_focus(moved_id));
        rt.workspace_new().expect("new ws2");
        assert!(rt.workspace_switch(0));
        assert!(rt.set_focus(moved_id));
        let count_before = rt.workspace_count();
        let moved = rt.workspace_move_focused_to(1).expect("move to ws2");
        assert_eq!(moved, moved_id);
        // Source (active ws1) lost one leaf; focus fell back to survivor.
        assert_eq!(rt.layout().leaf_count(), 1);
        assert_ne!(rt.focused_view(), Some(moved_id));
        assert_eq!(
            rt.workspace_count(),
            count_before,
            "move never removes a slot"
        );
        assert_eq!(rt.active_workspace_index(), 0, "move does not switch");
        assert!(!rt.has_pending_ws_close(), "move never arms close");
        // Target (inactive ws2) gained the leaf and focuses it.
        assert!(rt.workspace_switch(1));
        assert_eq!(rt.layout().leaf_count(), 2);
        assert!(rt.layout().leaf_ids().contains(&moved_id));
        assert_eq!(rt.focused_view(), Some(moved_id));
        // Same-workspace move is a no-op success.
        let same = rt
            .workspace_move_focused_to(1)
            .expect("same-workspace no-op");
        assert_eq!(same, moved_id);
        assert_eq!(rt.layout().leaf_count(), 2);
        // Invalid targets fail closed with state untouched.
        let tabline = rt.workspaceline_text();
        assert!(rt.workspace_move_focused_to(9).is_err());
        assert!(rt.workspace_move_focused_to_one_based(0).is_err());
        assert!(rt.workspace_move_focused_to_one_based(99).is_err());
        assert_eq!(rt.workspaceline_text(), tabline);
        assert_eq!(rt.layout().leaf_count(), 2);
    }

    #[test]
    fn move_single_leaf_source_leaves_fresh_idle() {
        // CTX-0259 last-workspace guard: moving the sole leaf out of a
        // workspace leaves a fresh idle leaf behind (no empty slot, no slot
        // removal, `>= 1` holds).
        let mut rt = fresh();
        let sole = rt.focused_view().expect("focus");
        rt.workspace_new().expect("new ws2");
        assert!(rt.workspace_switch(0));
        assert!(rt.set_focus(sole));
        let moved = rt.workspace_move_focused_to(1).expect("move sole leaf");
        assert_eq!(moved, sole);
        assert_eq!(rt.workspace_count(), 2);
        assert_eq!(rt.layout().leaf_count(), 1, "source keeps one leaf");
        let survivor = rt.focused_view().expect("source focus valid");
        assert_ne!(survivor, sole, "source focuses the fresh leaf");
        assert!(!rt.layout().leaf_ids().contains(&sole));
        assert!(rt.workspace_switch(1));
        assert_eq!(rt.layout().leaf_count(), 2);
        assert!(rt.layout().leaf_ids().contains(&sole));
        assert_eq!(rt.focused_view(), Some(sole), "target focuses moved window");
    }

    #[test]
    fn move_single_workspace_self_is_noop_and_invalid_fails_closed() {
        // With one workspace the only valid target is itself (no-op); any
        // other N fails closed and the layout is untouched.
        let mut rt = fresh();
        let sole = rt.focused_view().expect("focus");
        let same = rt.workspace_move_focused_to(0).expect("self move no-op");
        assert_eq!(same, sole);
        assert_eq!(rt.workspace_count(), 1);
        assert_eq!(rt.layout().leaf_count(), 1);
        assert!(rt.workspace_move_focused_to(1).is_err());
        assert!(rt.workspace_move_focused_to_one_based(2).is_err());
        assert_eq!(rt.workspace_count(), 1);
        assert_eq!(rt.layout().leaf_count(), 1);
        assert_eq!(rt.focused_view(), Some(sole));
    }

    // Live-spawn: runs a real POSIX shell (`/bin/sh` has no Windows
    // equivalent). `#[cfg(unix)]` keeps it off Windows CI; `require_pty!()`
    // keeps the force-no-PTY simulation path. ConPTY coverage lives in
    // bitty-pty/tests/spawn_windows.rs (CTX-0268); porting this test to a
    // platform-neutral spawn is deferred follow-up.
    #[test]
    #[cfg(unix)]
    fn live_move_preserves_session_without_kill() {
        require_pty!();
        let mut rt = fresh();
        // Live session in ws1's second leaf.
        let moved_id = ViewId::new(2);
        let mut layout = rt.layout().clone();
        let focused = rt.focused_view().expect("focus");
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(moved_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        rt.set_layout(layout);
        rt.spawn_shell_for_view(moved_id, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        assert!(rt.set_focus(moved_id));
        rt.workspace_new().expect("new ws2");
        assert!(rt.workspace_switch(0));
        assert!(rt.set_focus(moved_id));
        assert_eq!(rt.workspace_live_count(0), 1);
        let moved = rt.workspace_move_focused_to(1).expect("live move");
        assert_eq!(moved, moved_id);
        // Session survived (never killed); primary PTY untouched (move never
        // calls the kill path); source is idle, target holds the live leaf.
        assert!(rt.has_pane_session(&moved_id), "move preserves session");
        assert_eq!(rt.workspace_live_count(0), 0, "source drained");
        assert_eq!(rt.workspace_live_count(1), 1, "target gained live leaf");
        assert!(rt.workspace_switch(1));
        assert!(rt.has_pane_session(&moved_id));
        assert!(rt.layout().leaf_ids().contains(&moved_id));
        assert_eq!(rt.focused_view(), Some(moved_id));
    }
}
