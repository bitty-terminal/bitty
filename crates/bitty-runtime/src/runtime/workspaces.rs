//! App workspaces: named layout slots with MRU order and kill-confirm close.
//!
//! CTX-0257 workspace ops entry per DEC-0034 (tiling-first, no float mode).
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
//! down; primary-shell teardown on workspace close is a follow-up.
//!
//! Move/resize/focus-follows-mouse/Alt-drag are follow-ups, not this task.
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

    /// Minimal tabline render: names + indices + focused marker + count.
    ///
    /// Pure overlay string, never grid truth: `1:ws1* 2:ws2 (2)` — each
    /// slot renders as `{1-based}:{name}` with `*` on the active one, plus
    /// ` ({count})`. Bounded ([`WORKSPACELINE_MAX_CHARS`]), deterministic,
    /// headless-pinned. Updates are trivial: it reads live state, so every
    /// switch/new/close is reflected on the next call.
    #[must_use]
    pub fn workspaceline_text(&self) -> String {
        let mut parts = Vec::with_capacity(self.workspaces.len());
        for (idx, slot) in self.workspaces.iter().enumerate() {
            let mark = if idx == self.active_workspace {
                "*"
            } else {
                ""
            };
            parts.push(format!(
                "{}:{}{}",
                idx + 1,
                truncate_ws_name(&slot.name),
                mark
            ));
        }
        let mut out = parts.join(" ");
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

    /// Fresh [`ViewId`] unique across every slot and the live layout.
    fn next_view_id_global(&self) -> ViewId {
        let mut max = self
            .layout
            .leaf_ids()
            .iter()
            .map(|id| id.0)
            .max()
            .unwrap_or(0);
        for slot in &self.workspaces {
            if let Some(m) = slot.layout.leaf_ids().iter().map(|id| id.0).max() {
                max = max.max(m);
            }
        }
        ViewId::new(max.saturating_add(1).max(1))
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
        if let Some(slot) = self.workspaces.get(index) {
            self.layout = slot.layout.clone();
            self.focus = slot.focus.clone();
        }
    }

    /// Front an index in the MRU (each live index exactly once).
    fn mru_front(&mut self, index: usize) {
        self.workspace_mru.retain(|&i| i != index);
        self.workspace_mru.push_front(index);
    }

    /// Create a fresh workspace and switch to it.
    ///
    /// The new workspace starts as a single idle leaf (no shell spawns;
    /// lazy spawn is a follow-up). Fail-closed when at capacity
    /// ([`MAX_WORKSPACES`]).
    pub fn workspace_new(&mut self) -> Result<usize, String> {
        if self.workspaces.len() >= MAX_WORKSPACES {
            return Err(format!(
                "too many workspaces: max {MAX_WORKSPACES}, current {}",
                self.workspaces.len()
            ));
        }
        self.stash_active_slot();
        let seq = self.next_workspace_seq;
        self.next_workspace_seq = seq.wrapping_add(1).max(1);
        let fresh_id = self.next_view_id_global();
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
    /// never strands empty). The caller owns session teardown.
    fn remove_workspace(&mut self, index: usize) {
        if index >= self.workspaces.len() {
            return;
        }
        self.workspaces.remove(index);
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
        let active = index.min(self.workspaces.len().saturating_sub(1));
        self.active_workspace = active;
        self.load_slot(active);
        self.mru_front(active);
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

    /// Close workspace `one_based` (1-based, `ctl` path): immediate, no
    /// confirm gate (elevation is the gate there). Returns sessions killed.
    /// Unknown indices fail closed.
    pub fn workspace_close_index(&mut self, one_based: u64) -> Result<usize, String> {
        if one_based == 0 {
            return Err(String::from("workspace index starts at 1 (e.g. ws:1)"));
        }
        let index = (one_based - 1) as usize;
        if index >= self.workspaces.len() {
            return Err(format!("no such workspace ws:{one_based}"));
        }
        let killed = self.kill_workspace_sessions(index);
        self.remove_workspace(index);
        Ok(killed)
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

    // Live-spawn: runs a real shell; skips (not fails) where no PTY backend
    // exists (Windows ConPTY unimplemented per ADR-0002; CTX-0267).
    #[test]
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

    // Live-spawn: runs a real shell; skips (not fails) where no PTY backend
    // exists (Windows ConPTY unimplemented per ADR-0002; CTX-0267).
    #[test]
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
}
