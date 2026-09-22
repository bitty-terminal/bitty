//! `Runtime` — shell prompt jump over anchored zone marks (M1-18, CTX-0665).
//!
//! Keyboard-driven prompt navigation built on the [`bitty_term_state`]
//! buffer anchors (`State::prev_prompt_buffer_row` /
//! `State::next_prompt_buffer_row`). The jump target is a resolved buffer
//! row, so prompts that scrolled into history stay reachable and prompts
//! whose lines were pruned, cleared, or reflowed are skipped, never
//! landed on. No PTY bytes are produced; the focused viewport scrolls so
//! the target lands at its top. Fail-closed (`false`) when there is no
//! focused view, no resolvable prompt in the direction of travel, or the
//! view is already showing the target.
use super::*;

impl Runtime {
    /// Jumps the focused viewport to the previous prompt block.
    ///
    /// The reference is the viewport's top row: the target is the nearest
    /// resolvable `PromptStart` strictly above it. From live this lands on
    /// the most recent prompt above the viewport; from the top of history
    /// it fails closed. Returns `true` when the viewport moved.
    pub fn shell_goto_prev_prompt(&mut self) -> bool {
        self.shell_goto_prompt(false)
    }

    /// Jumps the focused viewport to the next prompt block.
    ///
    /// Mirror of [`Self::shell_goto_prev_prompt`]: the target is the
    /// nearest resolvable `PromptStart` strictly below the viewport's top
    /// row. From live there is nothing below, so this fails closed;
    /// after paging up into history it hops toward live. Returns `true`
    /// when the viewport moved.
    pub fn shell_goto_next_prompt(&mut self) -> bool {
        self.shell_goto_prompt(true)
    }

    /// Shared prompt-jump: scrolls the focused view to reveal the nearest
    /// resolvable prompt mark in the travel direction.
    fn shell_goto_prompt(&mut self, forward: bool) -> bool {
        let Some(focused) = self.focused_view() else {
            return false;
        };
        let sb_len = self.state.scrollback_len();
        let total = sb_len + self.state.height();
        let Some((start, rows)) = self.focused_view_window(focused, total, sb_len) else {
            return false;
        };
        let target = if forward {
            self.state.next_prompt_buffer_row(start)
        } else {
            self.state.prev_prompt_buffer_row(start)
        };
        let Some(row) = target else {
            return false;
        };
        if rows == 0 {
            return false;
        }
        // Place the target at the viewport top: start == row.
        let desired = total.saturating_sub(rows).saturating_sub(row).min(sb_len);
        let changed = {
            let layout = &mut self.layout;
            let Some(view) = layout.find_leaf_mut(focused) else {
                return false;
            };
            if view.scroll_offset().min(sb_len) == desired {
                false
            } else {
                view.set_scroll_offset(desired, sb_len);
                true
            }
        };
        if changed {
            self.pending_full_redraw = true;
        }
        changed
    }

    /// Viewport window `(start, rows)` for `view`: `start` is the combined
    /// buffer row at the viewport top. `None` when the view is unknown or
    /// has zero rows.
    fn focused_view_window(
        &self,
        view: crate::ViewId,
        total: usize,
        sb_len: usize,
    ) -> Option<(usize, usize)> {
        let leaf = self.layout.find_leaf(view)?;
        let rows = leaf.rows() as usize;
        if rows == 0 {
            return None;
        }
        let offset = leaf.scroll_offset().min(sb_len);
        let start = total.saturating_sub(rows).saturating_sub(offset);
        Some((start, rows))
    }
}
