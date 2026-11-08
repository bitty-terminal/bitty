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
//!
//! CTX-0952 (issue #1670) extends the same anchors with output selection:
//! [`Runtime::select_command_output`] covers exactly the last command's
//! output rows ([`State::last_command_output_rows`]) with a full-line
//! selection owned by the focused view, following the ghostty
//! `selectOutput` shape (output between the `C` mark and the next prompt,
//! empty output selects nothing). Reference decisions (read-only clones
//! under `recording/references/`): kitty `scroll_to_prompt` fails closed
//! at the history bounds and jumps prompt-by-prompt; ghostty
//! `jump_to_prompt(±1)` moves one prompt per gesture with viewport reveal;
//! wezterm exposes `ScrollToPrompt` as a bindable key assignment. Bitty
//! keeps one-prompt-per-gesture with the target at the viewport top.
use super::*;

/// Prompt-jump travel direction (CTX-0952, issue #1670).
///
/// The typed form of the `jump_to_prompt:<prev|next>` keymap action, so
/// chrome dispatch and embedders share one vocabulary instead of a bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptDirection {
    /// Toward older prompts (scrollback).
    Prev,
    /// Toward newer prompts (live).
    Next,
}

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

    /// Jumps the focused viewport one prompt in `direction` (CTX-0952,
    /// issue #1670: the `jump_to_prompt:<prev|next>` keymap action).
    ///
    /// Same target and viewport alignment as
    /// [`Self::shell_goto_prev_prompt`] / [`Self::shell_goto_next_prompt`]
    /// (one prompt per gesture, target lands at the viewport top);
    /// fail-closed (`false`) under the same conditions. No PTY bytes are
    /// produced.
    pub fn jump_to_prompt(&mut self, direction: PromptDirection) -> bool {
        self.shell_goto_prompt(matches!(direction, PromptDirection::Next))
    }

    /// Selects exactly the last command's output (CTX-0952, issue #1670:
    /// the `select_command_output` keymap action).
    ///
    /// The range comes from [`State::last_command_output_rows`]: the rows
    /// between the last `OutputStart` (`OSC 133;C`) mark and the next
    /// prompt, or up to the cursor for a still-running command. The
    /// selection covers whole lines ([`Selection::line_drag`]) and is
    /// owned by the focused view, so copy/yank read it through the usual
    /// owner-guarded seams. A successful select snaps the focused viewport
    /// back to live so the selection is visible (user intent, same
    /// precedent as the paste gate). Returns `true` when a selection was
    /// installed.
    ///
    /// Fail-closed (`false`, no selection touched): no focused view, no
    /// resolvable output range (absent/empty/pruned marks — never a panic,
    /// never a mis-select), or any endpoint scrolled out of the live grid
    /// (selections address live cells only; history output is left alone
    /// rather than clamped to the wrong lines).
    pub fn select_command_output(&mut self) -> bool {
        let Some(focused) = self.focused_view() else {
            return false;
        };
        let Some(grid) = self.live_view_state(focused) else {
            return false;
        };
        let Some((start_buf, end_buf)) = grid.last_command_output_rows() else {
            return false;
        };
        if end_buf < start_buf {
            return false;
        }
        let sb_len = grid.scrollback_len();
        let height = grid.height();
        let width = grid.width();
        if width == 0 || height == 0 {
            return false;
        }
        // Both endpoints must address live rows: `checked_sub` rejects
        // history rows, and the height bound rejects rows past the grid
        // (a stale anchor can never resolve there, but stay total).
        let (Some(start_live), Some(end_live)) =
            (start_buf.checked_sub(sb_len), end_buf.checked_sub(sb_len))
        else {
            return false;
        };
        if start_live >= height || end_live >= height {
            return false;
        }
        let snapshot = grid.snapshot();
        let selection = Selection::line_drag(
            &snapshot,
            CellPos::new(start_live as u16, 0),
            CellPos::new(end_live as u16, width.saturating_sub(1) as u16),
        );
        self.install_selection(focused, selection, None, false);
        self.snap_focused_to_live();
        true
    }

    /// Shared prompt-jump: scrolls the focused view to reveal the nearest
    /// resolvable prompt mark in the travel direction.
    fn shell_goto_prompt(&mut self, forward: bool) -> bool {
        let Some(focused) = self.focused_view() else {
            return false;
        };
        let Some(grid) = self.live_view_state(focused) else {
            return false;
        };
        let sb_len = grid.scrollback_len();
        let total = sb_len + grid.height();
        let Some((start, rows)) = self.focused_view_window(focused, total, sb_len) else {
            return false;
        };
        let target = if forward {
            grid.next_prompt_buffer_row(start)
        } else {
            grid.prev_prompt_buffer_row(start)
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
            let Some(view) = self.find_view_mut(focused) else {
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
            self.capture_view_scroll_anchor(focused);
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
        let leaf = self.find_view(view)?;
        let rows = leaf.rows() as usize;
        if rows == 0 {
            return None;
        }
        let offset = leaf.scroll_offset().min(sb_len);
        let start = total.saturating_sub(rows).saturating_sub(offset);
        Some((start, rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_command(rt: &mut Runtime, view: Option<ViewId>, cmd: &str, output: &[&str]) {
        let mut seq = String::new();
        seq.push_str("\u{1b}]133;A\u{7}prompt$ \u{1b}]133;B\u{7}");
        seq.push_str(cmd);
        seq.push_str("\r\n\u{1b}]133;C\u{7}");
        for line in output {
            seq.push_str(line);
            seq.push_str("\r\n");
        }
        seq.push_str("\u{1b}]133;D;0\u{7}\u{1b}]133;A\u{7}prompt$ ");
        match view {
            Some(v) => rt.handle_pane_bytes(v, seq.as_bytes()),
            None => rt.handle_pty_bytes(seq.as_bytes()),
        }
    }

    #[test]
    fn sessionless_non_primary_leaf_fails_closed_3060() {
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        rt.force_headless_clipboard();
        let primary = ViewId::new(1);
        let other = ViewId::new(2);
        rt.set_layout(LayoutNode::split(
            crate::SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(primary, 80, 24)),
            LayoutNode::leaf(View::new(other, 80, 24)),
        ));
        let h = rt.state().height();
        for i in 0..(h + 4) {
            feed_command(
                &mut rt,
                None,
                &format!("cmd{i}"),
                &[&format!("primary-{i}")],
            );
        }

        assert!(rt.set_focus(other));
        assert!(
            !rt.select_command_output(),
            "session-less non-primary leaf must not select primary grid output"
        );
        assert!(
            !rt.jump_to_prompt(PromptDirection::Prev),
            "session-less non-primary leaf must not jump using primary grid prompts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn split_pane_session_uses_focused_pane_state_3060() {
        bitty_test_support::require_pty!();
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        rt.force_headless_clipboard();
        let primary = ViewId::new(1);
        let pane = ViewId::new(2);
        rt.set_layout(LayoutNode::split(
            crate::SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(primary, 80, 24)),
            LayoutNode::leaf(View::new(pane, 80, 24)),
        ));
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|f| f.view == pane)
            .expect("pane frame");
        rt.spawn_shell_for_view(pane, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
            .expect("spawn pane shell");

        feed_command(&mut rt, None, "primary-cmd", &["primary-out"]);
        for i in 0..30 {
            feed_command(
                &mut rt,
                Some(pane),
                &format!("pane-cmd-{i}"),
                &[&format!("pane-out-{i}")],
            );
        }

        assert!(rt.set_focus(pane));
        assert!(rt.select_command_output());
        assert_eq!(rt.selection_owner(), Some(pane));
        assert_eq!(rt.selection_text().as_deref(), Some("pane-out-29"));

        assert!(rt.jump_to_prompt(PromptDirection::Prev));
        let pane_offset = rt
            .layout()
            .find_leaf(pane)
            .expect("pane leaf")
            .scroll_offset();
        assert!(
            pane_offset > 0,
            "focused split pane must scroll to its own prompt"
        );
    }
}
