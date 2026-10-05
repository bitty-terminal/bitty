//! `terminal.submit` host operation (`bitty.terminal.submit(text) -> outcome`),
//! phase B (CTX-0929, W-103 S-1b).
//!
//! Accepted contract:
//! `bitty-terminal-docs/specifications/composer-architecture.md` (`W-82`).
//! Submit writes the accepted buffer to the focused panel's PTY through the
//! capability-gated terminal input API, attributed to the extension and gated
//! by the panel lease write rule (P0-AC-039). The extension never writes a
//! raw PTY handle; the byte-exact bracketed-paste frame stays host-side.
//!
//! Enforcement shape (phase B):
//!
//! - the caller supplies the focused panel's lease kernel, the submitting
//!   holder tag, and the host tick; the operation evaluates
//!   [`PanelLease::check_write`](crate::registry::PanelLease::check_write)
//!   itself and fails closed on refusal. Focused-panel lease *resolution*
//!   (placement view -> panel -> kernel) and holder *attribution* (which
//!   holder belongs to which plugin) arrive at cutover with the capability
//!   grant lane; this operation never trusts a bare boolean for the gate.
//! - the per-plugin byte window ([`SubmitBudget`](bitty_rich::host::SubmitBudget))
//!   is charged only on success; denials leave it untouched (P0-AC-014).
//! - over-cap submissions fail closed before framing and emit nothing.
//!
//! On success the frame is written through the single input router
//! ([`Runtime::push_input_bytes`]); the outcome reports the accepted byte
//! count. This module adds the host operation only: the retired composer
//! modal is gone (E-CUT, CTX-0968); the plugin owns editing UX via host ops.

use bitty_rich::host::{SubmitBudget, SubmitDeny, check_terminal_submit};

use super::Runtime;
use crate::registry::{LeaseHolder, PanelLease};

/// Why `terminal.submit` found no submission target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSubmitUnavailable {
    /// No focused leaf to route the frame to.
    NoFocusedView,
    /// The frame reached no live PTY (session-less non-primary leaf, or no
    /// writer live at all): it was only buffered headless, so the budget
    /// is untouched.
    BufferedOnly,
}

impl std::fmt::Display for TerminalSubmitUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFocusedView => f.write_str("submit unavailable: no focused panel"),
            Self::BufferedOnly => f.write_str("submit unavailable: no live PTY accepted the frame"),
        }
    }
}

/// Typed `terminal.submit` outcome (W-82 G-4 at the submit boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSubmitOutcome {
    /// The frame was written to the focused panel's PTY in one write.
    Accepted {
        /// Framed bytes delivered.
        bytes: usize,
    },
    /// The submission was refused; nothing was emitted and the budget is
    /// untouched.
    Denied(SubmitDeny),
    /// No submission target; nothing was emitted.
    Unavailable(TerminalSubmitUnavailable),
}

impl std::fmt::Display for TerminalSubmitOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accepted { bytes } => write!(f, "submit accepted ({bytes} bytes)"),
            Self::Denied(d) => write!(f, "submit denied: {d}"),
            Self::Unavailable(u) => write!(f, "{u}"),
        }
    }
}

impl Runtime {
    /// Submits `text` to the focused panel's PTY through the paste pipeline
    /// (`terminal.submit` / `bitty.terminal.submit`).
    ///
    /// `lease` is the focused panel's lease kernel, `holder` the submitting
    /// principal, and `now` the host tick: the lease write rule is evaluated
    /// here, fail-closed. `budget` is the submitting plugin's submit window,
    /// charged only on PTY delivery. The frame is byte-exact
    /// (`ESC[200~` content `ESC[201~` `CR`) and written in a single router
    /// call; any refusal emits nothing, and a frame that only buffers
    /// headless (session-less non-primary leaf, no live writer) reports
    /// [`TerminalSubmitUnavailable::BufferedOnly`] without charging.
    #[must_use]
    pub fn terminal_submit(
        &mut self,
        text: &str,
        lease: &PanelLease,
        holder: LeaseHolder,
        now: u64,
        budget: &mut SubmitBudget,
    ) -> TerminalSubmitOutcome {
        if self.focused_view().is_none() {
            return TerminalSubmitOutcome::Unavailable(TerminalSubmitUnavailable::NoFocusedView);
        }
        if lease.check_write(holder, now).is_err() {
            return TerminalSubmitOutcome::Denied(SubmitDeny::LeaseDenied);
        }
        // Validate (cap, framing) and price against a probe first so every
        // denial leaves the caller's budget untouched, exactly as
        // `check_terminal_submit` promises; the charge commits only after
        // the router confirms PTY delivery.
        let mut probe = budget.clone();
        let frame = match check_terminal_submit(text, true, &mut probe) {
            Ok(frame) => frame,
            Err(deny) => return TerminalSubmitOutcome::Denied(deny),
        };
        let bytes = frame.len();
        if !self.push_input_bytes(&frame) {
            return TerminalSubmitOutcome::Unavailable(TerminalSubmitUnavailable::BufferedOnly);
        }
        *budget = probe;
        TerminalSubmitOutcome::Accepted { bytes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayoutNode, SplitAxis, View, ViewId};
    // Only the live-spawn Accepted test below uses this (all `#[cfg(unix)]`);
    // without the gate the import is unused on Windows.
    #[cfg(unix)]
    use bitty_test_support::require_pty;

    fn test_budget(cap: u64) -> SubmitBudget {
        SubmitBudget::new("composer.test", cap)
    }

    fn granted_lease() -> (PanelLease, LeaseHolder) {
        let mut lease = PanelLease::idle();
        let holder = LeaseHolder(7);
        lease.acquire(holder, 100, 0).expect("acquire");
        (lease, holder)
    }

    fn split_with_sessionless_second_leaf(rt: &mut Runtime) -> ViewId {
        // Second leaf has no session and is not the primary owner: focusing
        // it reproduces the review case (buffered, never another shell).
        let second = ViewId::new(2);
        let focused = rt.focused_view().expect("default layout has focus");
        let old = rt
            .layout()
            .find_leaf(focused)
            .cloned()
            .expect("focused leaf exists");
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(second, 40, 24)),
        ));
        assert!(rt.set_focus(second));
        second
    }

    // Live-spawn: runs a real POSIX shell (`/bin/sh` has no Windows
    // equivalent). `#[cfg(unix)]` keeps it off Windows CI; `require_pty!()`
    // keeps the force-no-PTY simulation path.
    #[test]
    #[cfg(unix)]
    fn submit_accepted_writes_one_frame() {
        require_pty!();
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        let focused = rt.focused_view().expect("default layout has focus");
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        assert!(rt.set_focus(focused));
        let (lease, holder) = granted_lease();
        let mut budget = test_budget(1024 * 1024);
        let outcome = rt.terminal_submit("cargo test", &lease, holder, 1, &mut budget);
        match outcome {
            TerminalSubmitOutcome::Accepted { bytes } => {
                assert_eq!(bytes, b"\x1b[200~cargo test\x1b[201~\r".len());
                assert_eq!(budget.used(), bytes as u64);
            }
            other => panic!("expected accepted, got {other:?}"),
        }
        assert!(
            rt.pending_input().is_empty(),
            "PTY delivery must not buffer headless"
        );
    }

    #[test]
    fn submit_buffered_sessionless_leaf_is_unavailable_without_charge() {
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        split_with_sessionless_second_leaf(&mut rt);
        let (lease, holder) = granted_lease();
        let mut budget = test_budget(1024 * 1024);
        let outcome = rt.terminal_submit("cargo test", &lease, holder, 1, &mut budget);
        assert_eq!(
            outcome,
            TerminalSubmitOutcome::Unavailable(TerminalSubmitUnavailable::BufferedOnly)
        );
        assert_eq!(budget.used(), 0, "buffered frame must not charge");
        assert_eq!(
            rt.pending_input(),
            b"\x1b[200~cargo test\x1b[201~\r".as_slice(),
            "frame is buffered headless, never leaked to a shell"
        );
    }

    #[test]
    fn submit_headless_primary_without_writer_is_unavailable() {
        // Default headless runtime owns no writer: the primary leaf falls
        // through to the headless buffer, so submit reports Unavailable.
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        assert!(rt.focused_view().is_some(), "default layout has focus");
        let (lease, holder) = granted_lease();
        let mut budget = test_budget(1024 * 1024);
        let outcome = rt.terminal_submit("cargo test", &lease, holder, 1, &mut budget);
        assert_eq!(
            outcome,
            TerminalSubmitOutcome::Unavailable(TerminalSubmitUnavailable::BufferedOnly)
        );
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn submit_idle_lease_denies_and_emits_nothing() {
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        let lease = PanelLease::idle();
        let mut budget = test_budget(1024 * 1024);
        let outcome = rt.terminal_submit("echo hi", &lease, LeaseHolder(7), 1, &mut budget);
        assert_eq!(
            outcome,
            TerminalSubmitOutcome::Denied(SubmitDeny::LeaseDenied)
        );
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn submit_wrong_holder_and_lapsed_tenure_deny() {
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        let (lease, _) = granted_lease();
        let mut budget = test_budget(1024 * 1024);
        // Non-holder.
        let outcome = rt.terminal_submit("echo hi", &lease, LeaseHolder(9), 1, &mut budget);
        assert_eq!(
            outcome,
            TerminalSubmitOutcome::Denied(SubmitDeny::LeaseDenied)
        );
        // Lapsed tenure (expires_at = 100).
        let (lease, holder) = granted_lease();
        let outcome = rt.terminal_submit("echo hi", &lease, holder, 100, &mut budget);
        assert_eq!(
            outcome,
            TerminalSubmitOutcome::Denied(SubmitDeny::LeaseDenied)
        );
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn submit_over_cap_and_exhausted_budget_deny() {
        let mut rt = Runtime::with_defaults().expect("headless runtime");
        let (lease, holder) = granted_lease();
        let mut budget = test_budget(1024 * 1024);
        let big = "q".repeat(bitty_rich::host::COMPOSER_MAX_BYTES + 1);
        let outcome = rt.terminal_submit(&big, &lease, holder, 1, &mut budget);
        assert!(matches!(
            outcome,
            TerminalSubmitOutcome::Denied(SubmitDeny::TooLarge { .. })
        ));
        let mut small = test_budget(4);
        let outcome = rt.terminal_submit("echo hi", &lease, holder, 1, &mut small);
        assert!(matches!(
            outcome,
            TerminalSubmitOutcome::Denied(SubmitDeny::BudgetExceeded { .. })
        ));
        assert_eq!(budget.used(), 0);
        assert_eq!(small.used(), 0);
    }
}
