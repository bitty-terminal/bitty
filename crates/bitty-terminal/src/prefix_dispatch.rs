//! Prefix-sequence dispatch: `<Leader> <second>` pending-state routing
//! (CTX-1002 / issue #1650, RFC OQ-056).
//!
//! Pure, headless decision half of the Core prefix mechanism: given the
//! configured [`ResolvedLeader`](bitty_config::ResolvedLeader) plus the
//! resolved [`PrefixBinding`](bitty_config::PrefixBinding) table, classify
//! one press while the Leader window is pending. The caller owns the clock
//! (the shared [`LeaderState`](bitty_config::LeaderState) window: expiry is
//! reaped via [`LeaderState::poll`](bitty_config::LeaderState::poll) before
//! this runs) and owns key routing (consumed presses never reach the PTY;
//! fall-through presses keep their normal owner).
//!
//! Nothing is hardcoded here: the arming chord, the pending budget, and
//! every follow-up binding arrive from configuration (`input.leader` /
//! `leader_key` plus `keymaps` `"leader <second>"` entries, or the same
//! spelling through `bitty.keymaps.suggest`). An empty binding table
//! classifies every press [`PrefixPress::Ignored`], so a config without
//! prefix sequences keeps single-chord dispatch byte-identical.
//!
//! Precedence while pending (first match wins):
//! 1. Bare `Esc` cancels ([`PrefixPress::Cancel`], consumed).
//! 2. A Leader re-press re-arms ([`PrefixPress::Rearm`], consumed); a held
//!    Leader repeat is swallowed without restarting the window
//!    ([`PrefixPress::Repeat`], consumed).
//! 3. A [`match_prefix`](bitty_config::match_prefix) hit dispatches
//!    ([`PrefixPress::Dispatch`], consumed).
//! 4. Anything else while a hint session is armed defers to it
//!    ([`PrefixPress::DeferToHint`], not consumed here).
//! 5. Anything else falls through ([`PrefixPress::Fallthrough`], not
//!    consumed: the press keeps its normal owner and the window disarms).
//!
//! Time/Space: classification is a bounded linear scan over the binding
//! table (`O(n)` in user entries, capped at `MAX_KEYMAPS`); no allocation,
//! no I/O, no clock reads.

#![forbid(unsafe_code)]

use bitty_config::{ChromeAction, KeyName, KeyRef, PrefixBinding, ResolvedLeader, match_prefix};

/// Outcome of classifying one press for the prefix dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixPress {
    /// Idle and not a Leader press, or no prefix bindings configured: the
    /// caller continues with hint/normal dispatch untouched.
    Ignored,
    /// Idle Leader press: arm the pending window (consumed).
    Arm,
    /// Pending Leader re-press: restart the window (consumed).
    Rearm,
    /// Pending Leader repeat (held key): swallowed, window unchanged
    /// (consumed, so holding the Leader can neither extend the window
    /// forever nor leak repeats to the PTY).
    Repeat,
    /// Pending follow-up match: dispatch the bound action (consumed).
    Dispatch(ChromeAction),
    /// Pending `Esc`: cancel the window (consumed).
    Cancel,
    /// Pending, unmatched, hint session armed: let the hint path decide
    /// (not consumed here; the window stays armed).
    DeferToHint,
    /// Pending, unmatched, no hint session: disarm and route normally
    /// (not consumed; the press keeps its normal owner).
    Fallthrough,
}

/// True for a bare `Esc` (no modifiers): the pending-window cancel gesture.
///
/// Mirrors the hint path's cancel spelling exactly, so one `Esc` cancels
/// both halves of the Leader interaction.
#[must_use]
pub(crate) fn is_prefix_cancel(key: KeyRef) -> bool {
    key.key == KeyName::Escape && !key.ctrl && !key.alt && !key.super_held
}

/// Classify one press for prefix dispatch (pure; the caller reaps timeout
/// expiry first via `LeaderState::poll`).
///
/// `pending` is the shared Leader window state (armed by a Leader press);
/// `hint_armed` is whether the hint session currently owns letters;
/// `is_repeat` marks auto-repeat presses (repeats never arm or re-arm:
/// holding the Leader must not extend the window forever).
pub(crate) fn classify_prefix_press(
    leader: &ResolvedLeader,
    bindings: &[PrefixBinding],
    key: KeyRef,
    pending: bool,
    hint_armed: bool,
    is_repeat: bool,
) -> PrefixPress {
    if bindings.is_empty() {
        return PrefixPress::Ignored;
    }
    if !pending {
        if !is_repeat && leader.arms(key) {
            return PrefixPress::Arm;
        }
        return PrefixPress::Ignored;
    }
    if is_prefix_cancel(key) {
        return PrefixPress::Cancel;
    }
    if leader.arms(key) {
        if !is_repeat {
            return PrefixPress::Rearm;
        }
        return PrefixPress::Repeat;
    }
    if let Some(action) = match_prefix(bindings, key) {
        return PrefixPress::Dispatch(action);
    }
    if hint_armed {
        return PrefixPress::DeferToHint;
    }
    PrefixPress::Fallthrough
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_config::{Chord, ChromeAction, LeaderPlatform, SplitDir, resolve_leader};

    fn leader() -> ResolvedLeader {
        resolve_leader(None, None, LeaderPlatform::Other).expect("default leader resolves")
    }

    fn bindings() -> Vec<PrefixBinding> {
        bitty_config::resolve_prefix_bindings(&[
            bitty_config::KeymapEntry {
                chord: "leader w".into(),
                action: "new_split:right".into(),
                context: "global".into(),
            },
            bitty_config::KeymapEntry {
                chord: "leader c".into(),
                action: "workspace_new".into(),
                context: "global".into(),
            },
        ])
        .expect("test bindings resolve")
    }

    fn key_for(chord_raw: &str) -> KeyRef {
        let chord = Chord::parse(chord_raw).expect("test chord parses");
        KeyRef {
            key: chord.key,
            ctrl: chord.ctrl,
            alt: chord.alt,
            shift: chord.shift,
            super_held: chord.super_held,
        }
    }

    fn bare(c: char) -> KeyRef {
        KeyRef {
            key: KeyName::Char(c),
            ctrl: false,
            alt: false,
            shift: false,
            super_held: false,
        }
    }

    #[test]
    fn single_chords_ignore_the_prefix_path() {
        // CTX-1002: with no pending Leader, ordinary chords (bound or
        // shell) never enter the prefix path; unrelated input is untouched.
        let leader = leader();
        let bindings = bindings();
        for raw in ["alt+h", "ctrl+tab", "ctrl+shift+c"] {
            assert_eq!(
                classify_prefix_press(&leader, &bindings, key_for(raw), false, false, false),
                PrefixPress::Ignored,
                "single chord {raw} ignored"
            );
        }
        // And with no bindings configured, even the Leader press ignores.
        let empty: Vec<PrefixBinding> = Vec::new();
        assert_eq!(
            classify_prefix_press(&leader, &empty, key_for("alt+space"), false, false, false),
            PrefixPress::Ignored,
            "leader ignored without bindings"
        );
    }

    #[test]
    fn leader_arms_and_followup_dispatches() {
        // CTX-1002: `<Leader> w` dispatches deterministically; `<Leader> c`
        // dispatches its own binding from the same window.
        let leader = leader();
        let bindings = bindings();
        assert_eq!(
            classify_prefix_press(
                &leader,
                &bindings,
                key_for("alt+space"),
                false,
                false,
                false
            ),
            PrefixPress::Arm,
            "leader arms from idle"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('w'), true, false, false),
            PrefixPress::Dispatch(ChromeAction::NewSplit(SplitDir::Right)),
            "follow-up w dispatches"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('c'), true, false, false),
            PrefixPress::Dispatch(ChromeAction::WorkspaceNew),
            "follow-up c dispatches"
        );
    }

    #[test]
    fn unmatched_followup_falls_through_with_normal_owner() {
        // CTX-1002: an unbound follow-up is NOT swallowed: the window
        // disarms and the press keeps its normal owner (shell/chrome).
        let leader = leader();
        let bindings = bindings();
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('z'), true, false, false),
            PrefixPress::Fallthrough,
            "unmatched z falls through"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, key_for("alt+h"), true, false, false),
            PrefixPress::Fallthrough,
            "bound single chord as follow-up falls through"
        );
    }

    #[test]
    fn hint_armed_defers_unmatched_to_hint_letters() {
        // CTX-1002: prefix bindings win on a hit, but an unmatched press
        // while the hint session is armed defers to it (operator/label
        // letters keep working); without a hint session it falls through.
        let leader = leader();
        let bindings = bindings();
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('w'), true, true, false),
            PrefixPress::Dispatch(ChromeAction::NewSplit(SplitDir::Right)),
            "prefix hit wins over hint"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('z'), true, true, false),
            PrefixPress::DeferToHint,
            "unmatched defers while hint armed"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('z'), true, false, false),
            PrefixPress::Fallthrough,
            "unmatched falls through with no hint"
        );
    }

    #[test]
    fn esc_cancels_and_leader_repress_rearms() {
        // CTX-1002: bare `Esc` cancels the window (consumed); a Leader
        // re-press restarts it (consumed); repeats never extend it.
        let leader = leader();
        let bindings = bindings();
        assert_eq!(
            classify_prefix_press(&leader, &bindings, key_for("escape"), true, false, false),
            PrefixPress::Cancel,
            "esc cancels"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, key_for("escape"), false, false, false),
            PrefixPress::Ignored,
            "esc idle keeps its normal owner"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, key_for("alt+space"), true, false, false),
            PrefixPress::Rearm,
            "leader re-press re-arms"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, key_for("alt+space"), false, false, true),
            PrefixPress::Ignored,
            "held leader repeat never arms"
        );
        assert_eq!(
            classify_prefix_press(&leader, &bindings, key_for("alt+space"), true, false, true),
            PrefixPress::Repeat,
            "held leader repeat swallowed without re-arming"
        );
    }

    #[test]
    fn timeout_expiry_resets_without_dropping_input() {
        // CTX-1002: the shared Leader window expires fail-open on the
        // caller clock: after the deadline the state is idle, the expired
        // press routes normally, and unrelated input never entered the
        // prefix path at all.
        use bitty_config::{LeaderPoll, LeaderState};
        let mut window = LeaderState::Idle;
        assert_eq!(window.poll(0), LeaderPoll::Idle);
        window.arm(10_000, bitty_config::LEADER_TIMEOUT_MS_DEFAULT);
        assert!(window.is_armed());
        assert_eq!(window.poll(10_999), LeaderPoll::Armed);
        assert_eq!(
            window.poll(10_000 + bitty_config::LEADER_TIMEOUT_MS_DEFAULT),
            LeaderPoll::Expired,
            "deadline expires fail-open"
        );
        assert!(!window.is_armed(), "expiry resets to idle");
        // Post-expiry presses classify idle: nothing pending, nothing
        // swallowed.
        let leader = leader();
        let bindings = bindings();
        assert_eq!(
            classify_prefix_press(&leader, &bindings, bare('w'), false, false, false),
            PrefixPress::Ignored,
            "post-expiry follow-up keeps its normal owner"
        );
    }

    #[test]
    fn leader_chord_comes_from_config_never_hardcoded() {
        // CTX-1002 / DEC-W144-1: re-chording the Leader moves the arming
        // press with it; the old default no longer arms.
        let custom = resolve_leader(Chord::parse("ctrl+b").ok(), None, LeaderPlatform::Other)
            .expect("custom leader resolves");
        let bindings = bindings();
        assert_eq!(
            classify_prefix_press(&custom, &bindings, key_for("ctrl+b"), false, false, false),
            PrefixPress::Arm,
            "configured leader arms"
        );
        assert_eq!(
            classify_prefix_press(
                &custom,
                &bindings,
                key_for("alt+space"),
                false,
                false,
                false
            ),
            PrefixPress::Ignored,
            "old default no longer arms"
        );
    }
}
