//! CTX-0723: present-path live wiring integration (issues #980-#984).
//!
//! Each test drives the live [`Runtime`] owner — Leader-armed hint
//! sessions, latest-block fold verbs, composer submit-to-PTY, provider
//! unregister symmetry, and fold-ordinal persistence — rather than the
//! candidate modules directly, so removing the wiring fails these tests.
//!
//! - #980 (CW-01): `cw_latest_command_id` / `cw_fold_latest` resolve the
//!   focused view's latest `OSC 133` command block for the
//!   `fold_toggle` / `fold_expand` / `fold_collapse` keymap actions.
//! - #981 (CW-02): `cw_hint_arm` / `cw_hint_push_key` / `cw_hint_disarm`
//!   own the live hint session behind the operator-conflict gate with
//!   prefix-completion dispatch over sequential keystrokes;
//!   `cw_hint_dispatch_armed` dispatches against the armed batch only (no
//!   caller-supplied batch), and `cw_present_plan` carries the live batch
//!   as a single zero-slot overlay payload.
//! - #982 (CW-03): submit frames from `cw_composer_feed` reach the focused
//!   PTY through the single input router; the external-editor request
//!   stays a routing flag.
//! - #983 (CW-04): `cw_hint_unregister` is the dispose symmetric of
//!   `cw_hint_register` on the single live engine.
//! - #984 (CW-05): `cw_fold_snapshot_ordinals` /
//!   `cw_fold_restore_ordinals` own fold persistence as anchor ordinals.
//!
//! Headless only: no PTY, window, GPU, wall-clock, or filesystem.

use bitty_rich::composer::{ComposerKeyEvent, frame_submit};
use bitty_runtime::cw_present::{
    CwComposerFeed, CwFoldAction, CwHintProvider, CwInputRoute, HintKeyOutcome,
};
use bitty_runtime::{DispatchOutcome, HintScope, Runtime, RuntimeConfig};

fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless runtime must build")
}

/// Feeds one full `OSC 133` command cycle (prompt / input / output / done)
/// so the live state derives one semantic command block.
fn mark_command(rt: &mut Runtime) {
    rt.handle_pty_bytes(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07output\x1b]133;D;0\x07");
}

// -- #980: latest-block fold verbs ------------------------------------------

#[test]
fn cw980_fold_latest_needs_shell_marks() {
    let mut rt = runtime();
    assert_eq!(rt.cw_latest_command_id(), None);
    for action in [
        CwFoldAction::Toggle,
        CwFoldAction::Expand,
        CwFoldAction::Collapse,
    ] {
        assert_eq!(rt.cw_fold_latest(action), None, "no invented target");
    }
}

#[test]
fn cw980_fold_latest_toggle_expand_collapse_live() {
    let mut rt = runtime();
    mark_command(&mut rt);
    mark_command(&mut rt);
    let latest = rt.cw_latest_command_id().expect("two marked commands");
    assert!(!rt.cw_fold_is_folded(latest));

    // Toggle flips membership and reports the post-flip end state.
    assert_eq!(rt.cw_fold_latest(CwFoldAction::Toggle), Some(true));
    assert!(rt.cw_fold_is_folded(latest));
    assert_eq!(rt.cw_fold_latest(CwFoldAction::Toggle), Some(false));
    assert!(!rt.cw_fold_is_folded(latest));

    // Expand is idempotent; collapse re-hides.
    assert_eq!(rt.cw_fold_latest(CwFoldAction::Expand), Some(true));
    assert!(!rt.cw_fold_is_folded(latest));
    assert_eq!(rt.cw_fold_latest(CwFoldAction::Collapse), Some(true));
    assert!(rt.cw_fold_is_folded(latest));
}

// -- #981: Leader-armed hint session -----------------------------------------

#[test]
fn cw981_push_key_fails_closed_while_disarmed() {
    let mut rt = runtime();
    assert!(!rt.cw_hint_is_armed());
    assert_eq!(
        rt.cw_hint_push_key('z'),
        HintKeyOutcome::Invalid { key: 'z' }
    );
    assert!(!rt.cw_hint_is_armed(), "rejection never arms");
}

#[test]
fn cw981_arm_collects_live_batch_and_feeds_operator_then_label() {
    let mut rt = runtime();
    assert!(rt.cw_hint_register(CwHintProvider::new(7)));
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("no conflicts");
    assert_eq!(count, 1, "one panel target, no command zones yet");
    assert!(rt.cw_hint_is_armed());

    // Unknown operator rejects but keeps the window open for a retry.
    assert_eq!(
        rt.cw_hint_push_key('q'),
        HintKeyOutcome::Invalid { key: 'q' }
    );
    assert!(rt.cw_hint_is_armed());

    // `p` (Focus) then the single label `a` dispatches and disarms.
    assert_eq!(rt.cw_hint_push_key('p'), HintKeyOutcome::NeedMore);
    assert!(rt.cw_hint_is_armed());
    assert_eq!(
        rt.cw_hint_push_key('a'),
        HintKeyOutcome::Dispatched(DispatchOutcome::FocusPanel { panel: 7 })
    );
    assert!(!rt.cw_hint_is_armed());
}

#[test]
fn cw981_dead_label_clears_buffer_and_stays_armed() {
    let mut rt = runtime();
    assert!(rt.cw_hint_register(CwHintProvider::new(7)));
    rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(rt.cw_hint_push_key('p'), HintKeyOutcome::NeedMore);
    // `z` extends no label in the single-target batch: rejected, buffer
    // cleared, operator kept, window still open.
    assert_eq!(
        rt.cw_hint_push_key('z'),
        HintKeyOutcome::Invalid { key: 'z' }
    );
    assert!(rt.cw_hint_is_armed());
    // Retry inside the same window feeds the label directly: the operator
    // survived the dead buffer.
    assert_eq!(
        rt.cw_hint_push_key('a'),
        HintKeyOutcome::Dispatched(DispatchOutcome::FocusPanel { panel: 7 })
    );
}

#[test]
fn cw981_operator_conflict_gate_refuses_arming() {
    let mut rt = runtime();
    assert!(rt.cw_hint_register(CwHintProvider::new(7)));
    let err = rt
        .cw_hint_arm(HintScope(1), 1, &['j', 'z', 'p', 'y', 'e', 'c'])
        .expect_err("shadowing operators must refuse");
    assert_eq!(err.keys.len(), 6);
    assert!(!rt.cw_hint_is_armed(), "refused arm keeps no batch");
}

#[test]
fn cw981_disarm_is_idempotent_and_safe() {
    let mut rt = runtime();
    rt.cw_hint_disarm();
    assert!(!rt.cw_hint_is_armed());
    assert!(rt.cw_hint_register(CwHintProvider::new(7)));
    rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    rt.cw_hint_disarm();
    rt.cw_hint_disarm();
    assert!(!rt.cw_hint_is_armed());
}

#[test]
fn cw981_fold_toggle_dispatches_through_armed_session() {
    let mut rt = runtime();
    mark_command(&mut rt);
    let latest = rt.cw_latest_command_id().expect("marked command");
    // The live engine includes command targets: one zone, one label.
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(count, 1);
    assert_eq!(rt.cw_hint_push_key('z'), HintKeyOutcome::NeedMore);
    assert_eq!(
        rt.cw_hint_push_key('a'),
        HintKeyOutcome::Dispatched(DispatchOutcome::FoldToggled {
            id: latest,
            folded: true,
        })
    );
    assert!(rt.cw_fold_is_folded(latest));
    assert!(!rt.cw_hint_is_armed());
}

#[test]
fn cw981_prefix_completion_waits_for_disambiguation() {
    let mut rt = runtime();
    // 27 panel targets overflow single letters: `a`..`z` plus `aa`.
    for panel in 1..=27u64 {
        assert!(rt.cw_hint_register(CwHintProvider::new(panel)));
    }
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(count, 27);
    assert_eq!(rt.cw_hint_push_key('p'), HintKeyOutcome::NeedMore);
    // `a` resolves panel 1 but `aa` extends it: no dispatch yet.
    assert_eq!(rt.cw_hint_push_key('a'), HintKeyOutcome::NeedMore);
    assert!(rt.cw_hint_is_armed());
    // The second `a` completes `aa` uniquely and dispatches panel 27.
    assert_eq!(
        rt.cw_hint_push_key('a'),
        HintKeyOutcome::Dispatched(DispatchOutcome::FocusPanel { panel: 27 })
    );
    assert!(!rt.cw_hint_is_armed());
}

// -- #982: composer submit reaches the PTY -----------------------------------

#[test]
fn cw981_armed_dispatch_uses_session_batch_only() {
    // CTX-0735 (#981 dispatch authority): the armed batch is the single
    // dispatch authority — `cw_hint_dispatch_armed` takes no batch, so a
    // stale or foreign batch can never be smuggled through the live path.
    use bitty_rich::hints::HintAction;
    use bitty_runtime::HintFeedError;
    let mut rt = runtime();
    assert!(rt.cw_hint_register(CwHintProvider::new(7)));
    // Disarmed: no authority, keys belong to the shell.
    assert_eq!(
        rt.cw_hint_dispatch_armed("a", HintAction::Focus),
        Err(HintFeedError::NotArmed)
    );
    assert!(!rt.cw_hint_is_armed());
    rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    // Unknown label rejects but keeps the window open for a retry.
    assert!(
        matches!(
            rt.cw_hint_dispatch_armed("zzz", HintAction::Focus),
            Err(HintFeedError::Dispatch(_))
        ),
        "unknown label dispatches nothing"
    );
    assert!(rt.cw_hint_is_armed(), "failed dispatch keeps the session");
    // The armed batch dispatches and disarms (chrome is ephemeral).
    assert_eq!(
        rt.cw_hint_dispatch_armed("a", HintAction::Focus),
        Ok(DispatchOutcome::FocusPanel { panel: 7 })
    );
    assert!(!rt.cw_hint_is_armed(), "dispatch disarms");
    // A second call finds no authority: the window is gone.
    assert_eq!(
        rt.cw_hint_dispatch_armed("a", HintAction::Focus),
        Err(HintFeedError::NotArmed)
    );
}

#[test]
fn cw981_present_plan_carries_hint_overlay() {
    // CTX-0735 (#981 overlay): the present plan carries the live batch as
    // a single zero-slot annotation payload — paint shows exactly what
    // dispatch can resolve.
    use bitty_rich::scene::Scene;
    use bitty_ui::panel::{PanelId, ViewContent};
    use bitty_ui::uitree::UiNodeId;
    use bitty_ui::view::ViewId;
    let mut rt = runtime();
    assert!(rt.cw_hint_register(CwHintProvider::new(7)));
    let batch = rt.cw_hint_collect(HintScope(1), 9);
    assert_eq!(batch.len(), 1);
    let scene = Scene::new();
    let plan = rt.cw_present_plan(
        ViewId::new(1),
        9,
        &[],
        &batch,
        &scene,
        ViewContent::Panel(PanelId::new(7)),
        UiNodeId::new(3),
    );
    assert_eq!(plan.hint_labels, 1);
    assert_eq!(plan.hint_shed, 0);
    assert_eq!(plan.hint_overlay_cost, 0);
    assert_eq!(plan.hint_overlay.len(), 1);
    assert_eq!(plan.hint_overlay.overlay_cost(), 0);
    assert_eq!(plan.hint_overlay.shed, 0);
    assert_eq!(
        plan.hint_overlay.entries[0].label,
        batch.labels()[0].label,
        "overlay paints the allocated label"
    );
}

#[test]
fn cw982_composer_submit_writes_single_frame_to_pty() {
    let mut rt = runtime();
    rt.cw_composer_open();
    assert_eq!(rt.cw_input_route(), CwInputRoute::Composer);
    assert_eq!(
        rt.cw_composer_feed(ComposerKeyEvent::printable('h')),
        CwComposerFeed::Inserted
    );
    assert_eq!(
        rt.cw_composer_feed(ComposerKeyEvent::printable('i')),
        CwComposerFeed::Inserted
    );
    let frame = match rt.cw_composer_feed(ComposerKeyEvent::ctrl_enter()) {
        CwComposerFeed::Submitted(frame) => frame,
        other => panic!("submit must frame, got {other:?}"),
    };
    assert_eq!(frame, frame_submit("hi").expect("fits composer cap"));
    assert!(!rt.cw_composer_is_open(), "submit auto-closes");
    assert_eq!(rt.cw_input_route(), CwInputRoute::Pty);

    // The single input router carries the frame headless (no writer live).
    rt.push_input_bytes(&frame);
    assert_eq!(rt.drain_pending_input(), frame);
}

#[test]
fn cw982_editor_request_stays_a_routing_flag() {
    let mut rt = runtime();
    rt.cw_composer_open();
    assert_eq!(
        rt.cw_composer_feed(ComposerKeyEvent::alt_e()),
        CwComposerFeed::EditorRequested
    );
    assert!(
        rt.cw_composer_is_open(),
        "editor request preserves the draft session"
    );
    assert_eq!(rt.cw_input_route(), CwInputRoute::Composer);
}

// -- #983: provider unregister symmetry ---------------------------------------

#[test]
fn cw983_unregister_stops_provider_targets() {
    let mut rt = runtime();
    assert!(rt.cw_hint_register(CwHintProvider::new(1)));
    assert!(rt.cw_hint_register(CwHintProvider::new(2)));
    assert_eq!(rt.cw_hint_provider_count(), 2);

    assert!(rt.cw_hint_unregister(1));
    assert_eq!(rt.cw_hint_provider_count(), 1);
    assert!(!rt.cw_hint_unregister(1), "second removal is a no-op");
    assert!(!rt.cw_hint_unregister(9), "unknown panel is a no-op");

    // The disposed panel contributes no labels to the next collection.
    let batch = rt.cw_hint_collect(HintScope(4), 3);
    assert_eq!(batch.len(), 1);
    assert_eq!(rt.cw_hint_arm(HintScope(4), 4, &[]).expect("arms"), 1);
    rt.cw_hint_disarm();
}

// -- #984: fold-ordinal persistence --------------------------------------------

#[test]
fn cw984_fold_snapshot_restore_round_trip() {
    let mut rt = runtime();
    mark_command(&mut rt);
    mark_command(&mut rt);
    let latest = rt.cw_latest_command_id().expect("marked commands");
    assert_eq!(rt.cw_fold_latest(CwFoldAction::Collapse), Some(true));

    let snapshot = rt.cw_fold_snapshot_ordinals();
    assert_eq!(snapshot, vec![latest.get()]);

    // A fresh runtime rehydrates the ordinals without any blocks present:
    // unknown ordinals are admitted harmlessly and project nothing.
    let mut fresh = runtime();
    assert!(fresh.cw_fold_snapshot_ordinals().is_empty());
    assert_eq!(fresh.cw_fold_restore_ordinals(&snapshot), 1);
    assert_eq!(fresh.cw_fold_snapshot_ordinals(), snapshot);
    assert!(fresh.cw_fold_is_folded(latest));

    // Restoring twice is idempotent.
    assert_eq!(fresh.cw_fold_restore_ordinals(&snapshot), 1);
    assert_eq!(fresh.cw_fold_snapshot_ordinals(), snapshot);
}

#[test]
fn cw984_restore_fails_closed_past_the_fold_cap() {
    let mut rt = runtime();
    let ordinals: Vec<u64> = (1..=300).collect();
    let admitted = rt.cw_fold_restore_ordinals(&ordinals);
    assert_eq!(admitted, bitty_rich::blocks::FOLD_MAX);
    assert_eq!(
        rt.cw_fold_snapshot_ordinals().len(),
        bitty_rich::blocks::FOLD_MAX
    );
}
