//! Close confirmation for views and windows with running jobs (CTX-0370).
//!
//! User report (m0466): closing when work is still running must prompt whether
//! to close. This module owns the gate decision and the pending arm; the
//! embedder owns the actual layout mutation (app close chord) and the window
//! lifetime.
//!
//! # Busy definition (bounded, never on the input hot path)
//!
//! A pane is **busy** when its PTY reports a kernel foreground process group
//! that differs from the spawned child pid (the shell itself). An idle shell
//! at its prompt is the foreground group leader, so it is not a job; a
//! foreground pipeline/program is. Undetectable states (Windows ConPTY has no
//! process-group surface, dead PTYs) count as *not busy* rather than
//! inventing a guess; the check runs only on a close gesture, and
//! [`Runtime::close_confirm_banner_text`] is the only per-frame consumer.
//!
//! # Gate contract (reuses the accepted modal contract)
//!
//! Mirrors the paste gate (CTX-0186/CTX-0369) and the workspace kill-confirm
//! (CTX-0257): the first close gesture arms a bounded, never-silent overlay
//! confirmation; **repeating the same close gesture confirms**; `Esc`
//! cancels (`cancel_pending_on_escape`). No new keybinding is introduced.
//! Modes come from the top-level `close_confirm` key
//! ([`CloseConfirmMode`]): `always`, `when_busy` (default), `never`.
//!
//! `view_close_request` returns [`ViewCloseRequest::Proceed`] when the caller
//! may run its existing close path and [`ViewCloseRequest::Pending`] when an
//! arm was set; `window_close_request` returns `true` when the application
//! loop may exit. The workspace kill-confirm gate is a separate accepted
//! control and is not governed by `close_confirm`.

use super::*;

use crate::config::CloseConfirmMode;

/// Hard bound on the rendered confirmation text (chars).
pub const CLOSE_CONFIRM_BANNER_MAX_CHARS: usize = 200;

/// Bound on the foreground-job name shown in the confirmation (chars).
pub const JOB_DISPLAY_MAX_CHARS: usize = 48;

/// Outcome of [`Runtime::view_close_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewCloseRequest {
    /// No confirmation required (or the repeat gesture confirmed): the
    /// caller proceeds with its existing close path.
    Proceed,
    /// A bounded confirmation was armed; `summary` is the overlay text.
    /// Repeating the close gesture confirms, `Esc` cancels.
    Pending {
        /// Bounded one-line banner text.
        summary: String,
    },
}

/// What a pending close confirmation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseTarget {
    /// Closing a single pane/view.
    View(ViewId),
    /// Closing the window (quit).
    Window,
}

/// A close awaiting explicit confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingCloseConfirm {
    pub(super) target: CloseTarget,
    pub(super) summary: String,
}

/// Bounded display label for one foreground job.
fn job_display_label(job: &bitty_pty::ForegroundJob) -> String {
    let cleaned: String = job
        .name
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(JOB_DISPLAY_MAX_CHARS)
        .collect();
    if cleaned.trim().is_empty() {
        format!("pid {}", job.pid)
    } else {
        cleaned
    }
}

/// Final display bound (chars) applied to every summary, fail-safe against a
/// future unbounded label source.
fn bound_summary(text: String) -> String {
    if text.chars().count() <= CLOSE_CONFIRM_BANNER_MAX_CHARS {
        return text;
    }
    text.chars().take(CLOSE_CONFIRM_BANNER_MAX_CHARS).collect()
}

/// One-line pane-close confirmation text.
fn view_close_summary(job: Option<&bitty_pty::ForegroundJob>) -> String {
    match job {
        Some(job) => bound_summary(format!(
            "Running job: {} — close pane anyway? (close again to confirm, Esc cancels)",
            job_display_label(job)
        )),
        None => String::from("Close pane? (close again to confirm, Esc cancels)"),
    }
}

/// One-line window-close confirmation text.
fn window_close_summary(job: Option<&bitty_pty::ForegroundJob>) -> String {
    match job {
        Some(job) => bound_summary(format!(
            "Running job: {} — close window anyway? (close again to confirm, Esc cancels)",
            job_display_label(job)
        )),
        None => String::from("Close window? (close again to confirm, Esc cancels)"),
    }
}

impl Runtime {
    /// Foreground job of the runtime-global primary PTY, when one runs.
    #[must_use]
    pub fn primary_foreground_job(&self) -> Option<bitty_pty::ForegroundJob> {
        self.pty.as_ref()?.foreground_job()
    }

    /// Foreground job of leaf `view`'s private PTY, when one runs (no
    /// session, an idle shell, or an undetectable backend all report
    /// `None`).
    #[must_use]
    pub fn pane_foreground_job(&self, view: &ViewId) -> Option<bitty_pty::ForegroundJob> {
        self.pane_sessions.get(view)?.pty.foreground_job()
    }

    /// First running job across the window for the window-close prompt:
    /// focused pane first, then every pane in deterministic `ViewId` order,
    /// then the primary PTY. Bounded (one job observation per call).
    fn first_running_job(&self) -> Option<bitty_pty::ForegroundJob> {
        if let Some(id) = self.focused_view() {
            if let Some(job) = self.pane_foreground_job(&id) {
                return Some(job);
            }
        }
        for (id, sess) in &self.pane_sessions {
            let _ = id;
            if let Some(job) = sess.pty.foreground_job() {
                return Some(job);
            }
        }
        self.primary_foreground_job()
    }

    /// Request close of leaf `view` under the `close_confirm` gate.
    ///
    /// - A pending arm for this exact view is the repeat gesture: the arm
    ///   clears and the request returns [`ViewCloseRequest::Proceed`].
    /// - `never` proceeds immediately; `always` arms even when idle;
    ///   `when_busy` arms only when the pane's PTY has a foreground job.
    /// - A stale arm for a different target is replaced by this explicit
    ///   request (loud, never silent).
    pub fn view_close_request(&mut self, view: ViewId) -> ViewCloseRequest {
        if let Some(pending) = self.pending_close_confirm.take() {
            if pending.target == CloseTarget::View(view) {
                self.pending_full_redraw = true;
                return ViewCloseRequest::Proceed;
            }
            // Explicit request for another target supersedes the stale arm.
            self.pending_full_redraw = true;
        }
        let job = self.pane_foreground_job(&view);
        let needs_confirm = match self.config.close_confirm {
            CloseConfirmMode::Never => false,
            CloseConfirmMode::Always => true,
            CloseConfirmMode::WhenBusy => job.is_some(),
        };
        if !needs_confirm {
            return ViewCloseRequest::Proceed;
        }
        let summary = view_close_summary(job.as_ref());
        self.pending_close_confirm = Some(PendingCloseConfirm {
            target: CloseTarget::View(view),
            summary: summary.clone(),
        });
        self.pending_full_redraw = true;
        ViewCloseRequest::Pending { summary }
    }

    /// Request window close (quit) under the `close_confirm` gate. Returns
    /// `true` when the application loop may exit.
    ///
    /// A pending window arm is the repeat gesture (the OS close request
    /// arriving again) and confirms. Otherwise `never` exits immediately;
    /// `always` arms even when idle; `when_busy` arms only when some pane or
    /// the primary PTY has a running foreground job. An arm for a view is
    /// superseded by the window request and re-decided here.
    pub fn window_close_request(&mut self) -> bool {
        if let Some(pending) = self.pending_close_confirm.take() {
            if pending.target == CloseTarget::Window {
                self.pending_full_redraw = true;
                return true;
            }
            // View arm superseded by the window gesture; re-decide below.
            self.pending_full_redraw = true;
        }
        let job = self.first_running_job();
        let needs_confirm = match self.config.close_confirm {
            CloseConfirmMode::Never => false,
            CloseConfirmMode::Always => true,
            CloseConfirmMode::WhenBusy => job.is_some(),
        };
        if !needs_confirm {
            return true;
        }
        let summary = window_close_summary(job.as_ref());
        self.pending_close_confirm = Some(PendingCloseConfirm {
            target: CloseTarget::Window,
            summary,
        });
        self.pending_full_redraw = true;
        false
    }

    /// Whether any close confirmation awaits a gesture.
    #[must_use]
    pub fn has_pending_close_confirm(&self) -> bool {
        self.pending_close_confirm.is_some()
    }

    /// The view a pending confirmation belongs to, when it is a view arm
    /// (the repeat confirm gesture is the close-view chord only for this
    /// view; a window arm reports `None`).
    #[must_use]
    pub fn pending_close_view(&self) -> Option<ViewId> {
        match self.pending_close_confirm.as_ref()?.target {
            CloseTarget::View(view) => Some(view),
            CloseTarget::Window => None,
        }
    }

    /// Overlay banner text for the pending close (steady while armed).
    ///
    /// Overlay-only via the present path (like the paste and workspace-close
    /// banners), never grid truth. `Some` exactly while an arm holds.
    #[must_use]
    pub fn close_confirm_banner_text(&self) -> Option<String> {
        Some(self.pending_close_confirm.as_ref()?.summary.clone())
    }

    /// Cancel a pending close without closing. `true` when one was armed.
    pub(super) fn cancel_pending_close_confirm(&mut self) -> bool {
        if self.pending_close_confirm.is_none() {
            return false;
        }
        self.pending_close_confirm = None;
        self.pending_full_redraw = true;
        true
    }

    /// Drop a pending arm that targets `view` (the pane/session is gone, so
    /// a stale arm must never be confirmed by an unrelated later close).
    pub(super) fn clear_pending_close_for_view(&mut self, view: ViewId) {
        if matches!(
            self.pending_close_confirm.as_ref().map(|p| p.target),
            Some(CloseTarget::View(v)) if v == view
        ) {
            self.pending_close_confirm = None;
            self.pending_full_redraw = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplitAxis;
    use crate::config::{CloseConfirmMode, RuntimeConfig};

    fn idle_runtime(mode: CloseConfirmMode) -> Runtime {
        let cfg = RuntimeConfig {
            close_confirm: mode,
            ..RuntimeConfig::default()
        };
        let mut rt = Runtime::new(cfg).expect("headless runtime builds");
        rt.force_headless_clipboard();
        rt
    }

    fn two_pane_layout() -> LayoutNode {
        let left = View::new(ViewId::new(1), 40, 12);
        let right = View::new(ViewId::new(2), 40, 12);
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(left),
            LayoutNode::leaf(right),
        )
    }

    #[test]
    fn idle_view_closes_without_confirm_under_when_busy() {
        let mut rt = idle_runtime(CloseConfirmMode::WhenBusy);
        rt.set_layout(two_pane_layout());
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        assert!(!rt.has_pending_close_confirm());
        assert_eq!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Proceed
        );
        assert!(!rt.has_pending_close_confirm(), "idle shell never arms");
    }

    #[test]
    fn never_proceeds_and_always_arms_even_when_idle() {
        let mut rt = idle_runtime(CloseConfirmMode::Never);
        rt.set_layout(two_pane_layout());
        assert_eq!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Proceed
        );
        assert!(!rt.has_pending_close_confirm());

        let mut rt = idle_runtime(CloseConfirmMode::Always);
        rt.set_layout(two_pane_layout());
        match rt.view_close_request(ViewId::new(2)) {
            ViewCloseRequest::Pending { summary } => {
                assert!(
                    summary.contains("close again to confirm"),
                    "names the confirm gesture: {summary}"
                );
                assert!(summary.contains("Esc cancels"), "names cancel: {summary}");
                assert!(
                    summary.chars().count() <= CLOSE_CONFIRM_BANNER_MAX_CHARS,
                    "bounded: {summary}"
                );
            }
            ViewCloseRequest::Proceed => panic!("always must arm on an idle pane"),
        }
        assert!(rt.has_pending_close_confirm());
        assert_eq!(rt.pending_close_view(), Some(ViewId::new(2)));
        // Repeat: the same view request confirms and clears the arm.
        assert_eq!(
            rt.view_close_request(ViewId::new(2)),
            ViewCloseRequest::Proceed
        );
        assert!(!rt.has_pending_close_confirm());
    }

    #[test]
    fn cancel_keeps_state_and_escalates_to_proceed_after_confirm() {
        let mut rt = idle_runtime(CloseConfirmMode::Always);
        rt.set_layout(two_pane_layout());
        assert!(matches!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Pending { .. }
        ));
        assert!(rt.cancel_pending_close_confirm());
        assert!(!rt.has_pending_close_confirm(), "cancel drops the arm");
        // Re-arm then confirm: Proceed is the only confirmation signal.
        assert!(matches!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Pending { .. }
        ));
        assert_eq!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Proceed
        );
        assert!(!rt.has_pending_close_confirm());
    }

    #[test]
    fn stale_view_arm_is_replaced_by_another_view_request() {
        let mut rt = idle_runtime(CloseConfirmMode::Always);
        rt.set_layout(two_pane_layout());
        assert!(matches!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Pending { .. }
        ));
        // A different view's close supersedes the stale arm and re-decides.
        match rt.view_close_request(ViewId::new(2)) {
            ViewCloseRequest::Pending { summary } => {
                assert!(summary.contains("Close pane?"), "{summary}");
            }
            ViewCloseRequest::Proceed => panic!("different view must re-decide, not confirm"),
        }
        assert_eq!(rt.pending_close_view(), Some(ViewId::new(2)));
    }

    #[test]
    fn closed_pane_session_clears_its_pending_arm() {
        let mut rt = idle_runtime(CloseConfirmMode::Always);
        rt.set_layout(two_pane_layout());
        assert!(matches!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Pending { .. }
        ));
        rt.clear_pending_close_for_view(ViewId::new(1));
        assert!(!rt.has_pending_close_confirm());
        assert_eq!(rt.close_confirm_banner_text(), None);
    }

    #[test]
    fn window_close_arms_and_confirm_exits_under_always() {
        let mut rt = idle_runtime(CloseConfirmMode::Always);
        rt.set_layout(two_pane_layout());
        assert!(
            !rt.window_close_request(),
            "first request must arm, not exit"
        );
        assert!(rt.has_pending_close_confirm());
        assert_eq!(rt.pending_close_view(), None, "window arm has no view");
        let banner = rt.close_confirm_banner_text().expect("banner while armed");
        assert!(banner.contains("Close window?"), "{banner}");
        // Repeat request confirms the exit.
        assert!(rt.window_close_request(), "repeat request must exit");
        assert!(!rt.has_pending_close_confirm());
    }

    #[test]
    fn window_close_proceeds_under_never_and_when_idle() {
        for mode in [CloseConfirmMode::Never, CloseConfirmMode::WhenBusy] {
            let mut rt = idle_runtime(mode);
            rt.set_layout(two_pane_layout());
            assert!(
                rt.window_close_request(),
                "{mode:?} must not trap an idle quit"
            );
            assert!(!rt.has_pending_close_confirm());
        }
    }

    #[test]
    fn view_arm_is_superseded_by_window_request() {
        let mut rt = idle_runtime(CloseConfirmMode::Always);
        rt.set_layout(two_pane_layout());
        assert!(matches!(
            rt.view_close_request(ViewId::new(1)),
            ViewCloseRequest::Pending { .. }
        ));
        // The window gesture re-decides: it arms a window prompt (always).
        assert!(!rt.window_close_request());
        assert_eq!(rt.pending_close_view(), None);
        assert!(
            rt.close_confirm_banner_text()
                .expect("window banner")
                .contains("Close window?")
        );
    }
}
