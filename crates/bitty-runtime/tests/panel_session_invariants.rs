//! CTX-0405: live pane-session ownership and grid coherence invariants.
//!
//! Companion to `panel_invariants.rs`: these tests drive real POSIX pane
//! shells so the PTY/session side of the frozen invariant set is exercised:
//!
//! - WS-INV-7: the primary grid has exactly one owner leaf; a pane session is
//!   owned by exactly one leaf id; a leaf never owns two sessions.
//! - WS-INV-9: a session's id is a live leaf id (never orphaned); closing a
//!   leaf tears the session down; layout-only operations never kill one.
//! - WS-INV-19: focus changes never re-home the primary owner; an explicit
//!   close of the owner re-homes it to the surviving focused leaf.
//! - WS-INV-22: every visible pane session's grid and PTY winsize equal its
//!   decorated content frame after any layout/container/workspace change.
//!
//! - WS-INV-13/16 (F-6): closing an inactive workspace preserves the active
//!   workspace's live layout, focus, and primary owner. The randomized live
//!   sequence interleaves inactive closes, and every step re-checks geometry
//!   (F-5) plus session liveness across all slots.
//!
//! Unix-only: spawning needs a POSIX shell plus PTY master semantics.

#![cfg(unix)]

use bitty_platform::PhysicalSize;
use bitty_runtime::{
    AnimationPolicy, LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, View, ViewId,
};

/// RFC-0002 animations off: these tests pin structural invariants, not
/// presentation transitions, so a change presents exactly one committed frame.
fn instant_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("instant runtime must build")
}

fn frame_of(rt: &Runtime, id: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have a present frame"))
}

/// Splits `target`'s leaf in place, keeping it and adding `new_id` as the
/// sibling (same shape as the keymap/ctl split funnel).
fn split_leaf(node: &mut LayoutNode, target: ViewId, new_id: ViewId, axis: SplitAxis) -> bool {
    match node {
        LayoutNode::Leaf(view) => {
            if view.id() != target {
                return false;
            }
            let old = view.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            *node = LayoutNode::split(axis, 0.5, LayoutNode::leaf(old), LayoutNode::leaf(fresh));
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_leaf(first, target, new_id, axis) || split_leaf(second, target, new_id, axis)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|child| split_leaf(child, target, new_id, axis)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_leaf(base, target, new_id, axis) || split_leaf(overlay, target, new_id, axis)
        }
    }
}

/// Removes `target`'s leaf, promoting its sibling; refuses the last leaf.
fn remove_leaf(node: &mut LayoutNode, target: ViewId) -> bool {
    match node {
        LayoutNode::Leaf(_) => false,
        LayoutNode::Split { first, second, .. } => {
            if matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == target) {
                *node = (**second).clone();
                true
            } else if matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == target) {
                *node = (**first).clone();
                true
            } else {
                remove_leaf(first, target) || remove_leaf(second, target)
            }
        }
        LayoutNode::Stack(children) => {
            if let Some(pos) = children
                .iter()
                .position(|child| matches!(child, LayoutNode::Leaf(v) if v.id() == target))
            {
                if children.len() <= 1 {
                    return false;
                }
                children.remove(pos);
                true
            } else {
                children.iter_mut().any(|child| remove_leaf(child, target))
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            remove_leaf(base, target) || remove_leaf(overlay, target)
        }
    }
}

/// Two-leaf workspace: leaf 1 is the primary owner, leaf 2 owns a private
/// `sleep` shell. The pane is spawned at its committed content frame so the
/// initial state satisfies WS-INV-22 before any operation runs.
fn live_two_pane_runtime() -> (Runtime, ViewId) {
    let mut rt = instant_runtime();
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("primary shell must spawn");
    assert_eq!(
        rt.primary_view(),
        Some(ViewId::new(1)),
        "the focused startup leaf owns the primary grid"
    );

    let new_id = rt.next_view_id_global();
    assert_eq!(new_id, ViewId::new(2));
    let mut layout = rt.layout().clone();
    assert!(split_leaf(
        &mut layout,
        ViewId::new(1),
        new_id,
        SplitAxis::Horizontal
    ));
    rt.set_layout(layout);
    let frame = frame_of(&rt, new_id);
    rt.spawn_shell_for_view(
        new_id,
        "/bin/sh",
        &["-c", "sleep 30"],
        frame.cols,
        frame.rows,
    )
    .expect("pane shell must spawn");
    (rt, new_id)
}

/// WS-INV-7/9: every session id is a live leaf of exactly one workspace slot;
/// session ids are unique; the primary owner (if any) is a live leaf.
fn assert_session_ids_live(rt: &mut Runtime, context: &str) {
    let original = rt.active_workspace_index();
    let mut live: Vec<(ViewId, usize)> = Vec::new();
    let mut focus_ok = true;
    for index in 0..rt.workspace_count() {
        assert!(rt.workspace_switch(index));
        let focused = rt
            .focused_view()
            .unwrap_or_else(|| panic!("{context}: workspace {index} must have focus"));
        focus_ok &= rt.layout().leaf_ids().contains(&focused);
        for id in rt.layout().leaf_ids() {
            assert!(
                !live.iter().any(|(other, _)| *other == id),
                "{context}: live ViewId {id:?} appears in two slots"
            );
            live.push((id, index));
        }
    }
    assert!(rt.workspace_switch(original));
    assert!(focus_ok, "{context}: every slot focus must be a live leaf");

    if let Some(owner) = rt.primary_view() {
        assert!(
            live.iter().any(|(id, _)| *id == owner),
            "{context}: primary owner {owner:?} must be a live leaf (WS-INV-7)"
        );
    }
    let sessions = rt.pane_session_ids();
    let mut unique = sessions.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        sessions.len(),
        "{context}: duplicate session ids"
    );
    for id in sessions {
        assert!(
            live.iter().any(|(leaf, _)| *leaf == id),
            "{context}: pane session {id:?} outlived its leaf (WS-INV-9)"
        );
    }
}

/// WS-INV-22: for every session visible in the active layout, the grid and
/// PTY winsize equal the decorated content frame the present path paints.
fn assert_visible_geometry_coherent(rt: &Runtime, context: &str) {
    let frames = rt.present_frames();
    for id in rt.pane_session_ids() {
        let Some(frame) = frames.iter().find(|frame| frame.view == id) else {
            continue;
        };
        let snapshot = rt
            .pane_snapshot(&id)
            .unwrap_or_else(|| panic!("{context}: session {id:?} must have a grid"));
        assert_eq!(
            (snapshot.width, snapshot.height),
            (usize::from(frame.cols), usize::from(frame.rows)),
            "{context}: pane {id:?} grid must match its content frame"
        );
        if let Some(size) = rt.pane_pty_size(&id) {
            assert_eq!(
                size,
                (frame.cols, frame.rows),
                "{context}: pane {id:?} PTY winsize must match its content frame"
            );
        }
    }
}

fn tick_and_check(rt: &mut Runtime, context: &str) {
    let _ = rt.tick();
    assert_visible_geometry_coherent(rt, context);
}

#[test]
fn pane_grid_follows_resize_then_workspace_switch() {
    bitty_test_support::require_pty!();
    let (mut rt, pane) = live_two_pane_runtime();
    tick_and_check(&mut rt, "initial");

    // A second workspace whose leaf replays the primary shell recipe.
    let ws2 = rt.workspace_new().expect("ws2");
    let fresh = rt.focused_view().expect("ws2 focus");
    assert!(
        rt.has_pane_session(&fresh),
        "workspace_new replays the primary recipe"
    );
    assert!(rt.workspace_switch(0));
    tick_and_check(&mut rt, "back on ws1");

    // Resize while ws2 is inactive: only the active slot reflows.
    rt.handle_resize(PhysicalSize::new(1024, 768))
        .expect("valid resize");
    tick_and_check(&mut rt, "ws1 after resize");

    // WS-INV-22: switching to the stashed slot must re-sync the pane grid and
    // PTY to the new container before/at the next presented frame.
    assert!(rt.workspace_switch(ws2), "ws2 must switch");
    tick_and_check(&mut rt, "ws2 after resize + switch");
    assert!(rt.workspace_switch(0));
    tick_and_check(&mut rt, "ws1 after round trip");
    let _ = pane;
}

#[test]
fn pane_grid_follows_move_promoted_sibling() {
    bitty_test_support::require_pty!();
    let (mut rt, pane) = live_two_pane_runtime();
    tick_and_check(&mut rt, "initial");

    // A target workspace, then move the primary leaf out of the two-leaf
    // source: the surviving pane leaf is promoted to the full container, so
    // its grid must follow the enlarged content frame.
    let ws2 = rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    assert!(rt.set_focus(ViewId::new(1)));
    let moved = rt
        .workspace_move_focused_to(ws2)
        .expect("moving the primary leaf out must succeed");
    assert_eq!(moved, ViewId::new(1));
    assert_eq!(rt.layout().leaf_ids(), vec![pane], "the pane leaf survives");
    tick_and_check(&mut rt, "source after move");

    assert!(rt.workspace_switch(ws2));
    tick_and_check(&mut rt, "target after move");
}

/// CW-27 (F-5 x F-6): resize with a stashed slot, then close the inactive
/// workspace. The active workspace's live layout, focus, and primary owner
/// must survive, and every visible pane grid/PTY must still match its
/// content frame (WS-INV-13/16/22).
#[test]
fn inactive_close_after_resize_keeps_active_geometry_coherent() {
    bitty_test_support::require_pty!();
    let (mut rt, _pane) = live_two_pane_runtime();
    tick_and_check(&mut rt, "initial");

    let ws2 = rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    let focus_before = rt.focused_view();
    let owner_before = rt.primary_view();

    rt.handle_resize(PhysicalSize::new(1024, 768))
        .expect("valid resize");
    tick_and_check(&mut rt, "ws1 after resize");
    let layout_resized = rt.layout().clone();

    rt.workspace_close_at(ws2).expect("close inactive ws2");
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(
        rt.layout(),
        &layout_resized,
        "live leaf edits survive inactive close"
    );
    assert_eq!(
        rt.focused_view(),
        focus_before,
        "focus survives inactive close"
    );
    assert_eq!(
        rt.primary_view(),
        owner_before,
        "primary owner survives inactive close"
    );
    tick_and_check(&mut rt, "ws1 after inactive close");
    assert_session_ids_live(&mut rt, "ws1 after inactive close");
}

#[test]
fn primary_owner_rehomes_only_on_close() {
    bitty_test_support::require_pty!();
    let (mut rt, pane) = live_two_pane_runtime();
    tick_and_check(&mut rt, "initial");
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));

    // Focus moves never re-home the primary owner.
    assert!(rt.set_focus(pane));
    assert_eq!(
        rt.primary_view(),
        Some(ViewId::new(1)),
        "focus must not move primary ownership (WS-INV-19)"
    );

    // Closing the owner re-homes it to the surviving focused leaf.
    let mut layout = rt.layout().clone();
    assert!(remove_leaf(&mut layout, ViewId::new(1)));
    rt.close_pane_session(&ViewId::new(1));
    rt.set_layout_closing(layout, ViewId::new(1));
    assert_eq!(
        rt.primary_view(),
        Some(pane),
        "closing the owner re-homes the primary grid to the survivor"
    );
    tick_and_check(&mut rt, "after owner close");
    assert_session_ids_live(&mut rt, "after owner close");
}

#[test]
fn zoom_hides_panes_without_killing_sessions() {
    bitty_test_support::require_pty!();
    let (mut rt, pane) = live_two_pane_runtime();
    tick_and_check(&mut rt, "initial");

    let focus = rt.focused_view().expect("focus");
    let view = rt.layout().find_leaf(focus).cloned().expect("leaf");
    let backup = rt.layout().clone();
    rt.set_layout(LayoutNode::leaf(view));
    let _ = rt.tick();
    assert_eq!(rt.pane_count(), 1, "zoom must not kill hidden sessions");
    assert!(
        rt.has_pane_session(&pane),
        "the hidden pane session survives zoom"
    );
    assert!(!rt.present_frames().iter().any(|f| f.view == pane));

    rt.set_layout(backup);
    tick_and_check(&mut rt, "after zoom restore");
    assert_session_ids_live(&mut rt, "after zoom restore");
}

/// Deterministic xorshift64 PRNG (no new dependency; reproducible failures).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        assert!(n > 0);
        (self.next() % n as u64) as usize
    }
}

fn run_live_sequence(seed: u64, steps: usize) {
    let (mut rt, _pane) = live_two_pane_runtime();
    let mut rng = Rng::new(seed);
    let mut spawned_workspaces = 0usize;

    for step in 0..steps {
        let context = format!("seed {seed} step {step}");
        match rng.below(100) {
            0..=17 => {
                // Split the focused leaf: the new leaf is session-less until a
                // future spawn, exercising the "erased leaf" ownership path.
                let new_id = rt.next_view_id_global();
                let target = rt.focused_view().expect("focus");
                let axis = if rng.next() & 1 == 0 {
                    SplitAxis::Horizontal
                } else {
                    SplitAxis::Vertical
                };
                let mut layout = rt.layout().clone();
                assert!(split_leaf(&mut layout, target, new_id, axis));
                rt.set_layout(layout);
            }
            18..=32 => {
                if rt.layout().leaf_count() > 1 {
                    let closing = rt.focused_view().expect("focus");
                    let mut layout = rt.layout().clone();
                    assert!(remove_leaf(&mut layout, closing));
                    rt.close_pane_session(&closing);
                    rt.set_layout_closing(layout, closing);
                }
            }
            33..=47 => {
                let ids = rt.layout().leaf_ids();
                let target = ids[rng.below(ids.len())];
                assert!(rt.set_focus(target));
            }
            48..=62 => {
                let index = rng.below(rt.workspace_count());
                assert!(rt.workspace_switch(index));
            }
            63..=77 => {
                if spawned_workspaces < 2 {
                    rt.workspace_new().expect("workspace below cap");
                    spawned_workspaces += 1;
                }
            }
            78..=86 => {
                let width = [640u32, 800, 1024, 1280][rng.below(4)];
                let height = [480u32, 600, 768, 960][rng.below(4)];
                rt.handle_resize(PhysicalSize::new(width, height))
                    .expect("valid resize");
            }
            87..=93 => {
                if rt.workspace_count() >= 2 {
                    let mut target = rng.below(rt.workspace_count() - 1);
                    if target >= rt.active_workspace_index() {
                        target += 1;
                    }
                    let _ = rt
                        .workspace_move_focused_to(target)
                        .expect("move to a live other workspace");
                }
            }
            94..=96 => {
                // CW-27 (F-6): close an inactive slot. The active
                // workspace's live layout and focus must survive; session
                // liveness (WS-INV-7/9) and geometry coherence (F-5) are
                // re-checked for every slot after the op below.
                if rt.workspace_count() >= 2 {
                    let active = rt.active_workspace_index();
                    let layout_before = rt.layout().clone();
                    let focus_before = rt.focused_view();
                    let mut target = rng.below(rt.workspace_count() - 1);
                    if target >= active {
                        target += 1;
                    }
                    rt.workspace_close_at(target).expect("inactive close");
                    assert_eq!(
                        rt.layout(),
                        &layout_before,
                        "{context}: inactive close keeps the live layout"
                    );
                    assert_eq!(
                        rt.focused_view(),
                        focus_before,
                        "{context}: inactive close keeps focus"
                    );
                }
            }
            _ => {
                if rt.workspace_count() >= 2 {
                    match rt.workspace_close_request() {
                        bitty_runtime::WsCloseRequest::Closed { .. } => {}
                        bitty_runtime::WsCloseRequest::Pending { .. } => {
                            assert!(rt.confirm_pending_ws_close());
                        }
                    }
                }
            }
        }
        tick_and_check(&mut rt, &context);
        assert_session_ids_live(&mut rt, &context);
    }
}

#[test]
fn randomized_live_ops_preserve_session_ownership_and_geometry() {
    bitty_test_support::require_pty!();
    for seed in [0x51A7_E000, 0x5EED_5104, 0x0BAD_51A7] {
        run_live_sequence(seed, 48);
    }
}
