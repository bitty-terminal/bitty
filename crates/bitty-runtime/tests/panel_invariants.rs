//! CTX-0405: Workspace/View/Panel/Terminal/Focus/Session invariants (headless).
//!
//! Pins the candidate invariant set in
//! `bitty-terminal-docs/specifications/workspace-panel-invariants.md`
//! (`WS-INV-*`) with deterministic pseudo-random state-machine sequences over
//! the live runtime layout/workspace surface. No PTY is spawned; live-session
//! ownership and grid coherence are covered by
//! `panel_session_invariants.rs`.
//!
//! Invariants exercised after every operation:
//!
//! - WS-INV-1: `ViewId` is unique among every live leaf of the active layout
//!   and every stashed workspace slot.
//! - WS-INV-5/6: a workspace never strands with zero leaves; the stashed slot
//!   focus is a live member of that slot's layout.
//! - WS-INV-12: a split installs a freshly allocated id through the single
//!   global allocator (`next_view_id_global`).
//! - WS-INV-13: close refuses the last leaf; closing the last workspace resets
//!   to a fresh idle leaf.
//! - WS-INV-14: a leaf moved between workspaces keeps its id and appears in
//!   exactly one slot.
//! - WS-INV-15: zoom/restore is a presentation swap over the same ids.
//! - WS-INV-18/19: focus is always a live member of its layout; focusing a
//!   non-member fails closed without mutating focus.
//!
//! The PRNG is a deterministic xorshift64 (no new dependency): a failing seed
//! reproduces exactly across machines.

use bitty_runtime::{
    FocusDirection, LayoutNode, MAX_WORKSPACES, Runtime, SplitAxis, View, ViewId, WsCloseRequest,
};

/// Deterministic xorshift64 PRNG for reproducible randomized sequences.
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
        assert!(n > 0, "modulus must be positive");
        (self.next() % n as u64) as usize
    }
}

/// Splits `target`'s leaf in place, keeping it and adding `new_id` as the
/// sibling. Mirrors the app keymap/ctl split shape (`chrome_keys::split_focused_leaf`).
fn split_leaf(
    node: &mut LayoutNode,
    target: ViewId,
    new_id: ViewId,
    axis: SplitAxis,
    place_new_first: bool,
) -> bool {
    match node {
        LayoutNode::Leaf(view) => {
            if view.id() != target {
                return false;
            }
            let old = view.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            let (first, second) = if place_new_first {
                (LayoutNode::leaf(fresh), LayoutNode::leaf(old))
            } else {
                (LayoutNode::leaf(old), LayoutNode::leaf(fresh))
            };
            *node = LayoutNode::split(axis, 0.5, first, second);
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_leaf(first, target, new_id, axis, place_new_first)
                || split_leaf(second, target, new_id, axis, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|child| split_leaf(child, target, new_id, axis, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_leaf(base, target, new_id, axis, place_new_first)
                || split_leaf(overlay, target, new_id, axis, place_new_first)
        }
    }
}

/// Removes `target`'s leaf, promoting its sibling; refuses the last leaf.
/// Mirrors `chrome_keys::close_focused_leaf`.
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

/// One workspace slot as observed through the public switch surface:
/// live leaf ids plus the focus that was loaded with the slot.
struct SlotView {
    leaves: Vec<ViewId>,
    focused: Option<ViewId>,
}

/// Reads every workspace slot by switching to it, then restores the original
/// active index. This is the only public way to observe stashed slots.
fn all_slots(rt: &mut Runtime) -> Vec<SlotView> {
    let original = rt.active_workspace_index();
    let mut slots = Vec::with_capacity(rt.workspace_count());
    for index in 0..rt.workspace_count() {
        assert!(
            rt.workspace_switch(index),
            "every live workspace index must be switchable"
        );
        slots.push(SlotView {
            leaves: rt.layout().leaf_ids(),
            focused: rt.focused_view(),
        });
    }
    assert!(
        rt.workspace_switch(original),
        "the original workspace must be restorable"
    );
    slots
}

/// Asserts every frozen WS-INV* structural rule, returning the live id count
/// and raw ids for the caller. `context` names the operation that just ran.
fn check_invariants(rt: &mut Runtime, context: &str) -> usize {
    let slots = all_slots(rt);
    assert_eq!(
        slots.len(),
        rt.workspace_count(),
        "{context}: slot scan must cover every workspace"
    );

    let mut seen: Vec<(u64, usize)> = Vec::new();
    let mut total = 0usize;
    for (index, slot) in slots.iter().enumerate() {
        assert!(
            !slot.leaves.is_empty(),
            "{context}: workspace {index} must never strand with zero leaves"
        );
        let focused = slot
            .focused
            .unwrap_or_else(|| panic!("{context}: workspace {index} must have focus loaded"));
        assert!(
            slot.leaves.contains(&focused),
            "{context}: workspace {index} focus {focused:?} must be a live leaf"
        );
        for id in &slot.leaves {
            assert!(id.0 >= 1, "{context}: ViewId raw 0 is never allocated");
            if let Some((_, other)) = seen.iter().find(|(raw, _)| *raw == id.0) {
                panic!(
                    "{context}: live ViewId {id:?} collides across workspace \
                     {other} and {index} (WS-INV-1)"
                );
            }
            seen.push((id.0, index));
        }
        total += slot.leaves.len();
    }
    assert_eq!(
        seen.len(),
        total,
        "{context}: leaf count must match id count"
    );

    // WS-INV-12: the allocator never re-hands a live id, including ids held by
    // inactive slots; WS-INV-18: focusing a non-live id fails closed.
    let candidate = rt.next_view_id_global();
    assert!(
        !seen.iter().any(|(raw, _)| *raw == candidate.0),
        "{context}: next_view_id_global handed out live id {candidate:?}"
    );
    let before_focus = rt.focused_view();
    assert!(
        !rt.set_focus(candidate),
        "{context}: set_focus must reject a non-member id {candidate:?}"
    );
    assert_eq!(
        rt.focused_view(),
        before_focus,
        "{context}: rejected focus must not mutate focus state"
    );

    total
}

/// The focused leaf of the active workspace (public ops keep one).
fn focused(rt: &Runtime) -> ViewId {
    rt.focused_view().expect("active layout always has focus")
}

/// Installs a fresh split of the focused leaf through the global allocator,
/// returning the new id. Mirrors the keymap/ctl split funnel: build the tree,
/// then commit it in one `set_layout` step (WS-INV-12).
fn op_split(rt: &mut Runtime, rng: &mut Rng) -> ViewId {
    let new_id = rt.next_view_id_global();
    let target = focused(rt);
    let axis = if rng.next() & 1 == 0 {
        SplitAxis::Horizontal
    } else {
        SplitAxis::Vertical
    };
    let place_first = rng.next() & 1 == 0;
    let mut layout = rt.layout().clone();
    assert!(
        split_leaf(&mut layout, target, new_id, axis, place_first),
        "focused leaf must be splittable"
    );
    rt.set_layout(layout);
    assert!(
        rt.layout().leaf_ids().contains(&new_id),
        "a committed split installs the allocated id"
    );
    new_id
}

/// Closes the focused leaf (refusing the last leaf) and routes the removal
/// through `set_layout_closing` so primary-owner re-homing stays exercised.
fn op_close(rt: &mut Runtime) -> ViewId {
    let closing = focused(rt);
    assert!(rt.layout().leaf_count() > 1, "close needs a sibling");
    let mut layout = rt.layout().clone();
    assert!(
        remove_leaf(&mut layout, closing),
        "focused leaf must be closable with a sibling"
    );
    rt.set_layout_closing(layout, closing);
    closing
}

/// Runs one randomized op; returns a label for diagnostics.
fn random_op(
    rt: &mut Runtime,
    rng: &mut Rng,
    leaves: usize,
    zoom_backup: &mut Option<LayoutNode>,
) -> String {
    // While zoomed, only focus/restore are legal: a zoom is a presentation
    // swap over a single leaf, and tree-mutating actions restore first (the
    // app calls `restore_zoom` before any tree edit).
    if zoom_backup.is_some() {
        return match rng.below(10) {
            0..=5 => {
                let ids = rt.layout().leaf_ids();
                let target = ids[rng.below(ids.len())];
                assert!(rt.set_focus(target), "zoom focus target must be live");
                String::from("set_focus(zoomed)")
            }
            6..=7 => {
                let before = focused(rt);
                let _ = rt.move_focus(
                    [
                        FocusDirection::Next,
                        FocusDirection::Prev,
                        FocusDirection::Up,
                        FocusDirection::Down,
                    ][rng.below(4)],
                );
                format!("move_focus(zoomed) {before:?}")
            }
            _ => {
                let backup = zoom_backup.take().expect("zoomed state");
                let zoomed = focused(rt);
                rt.set_layout(backup);
                assert!(
                    rt.layout().leaf_ids().contains(&zoomed),
                    "zoom restore keeps the zoomed leaf live"
                );
                assert_eq!(focused(rt), zoomed, "zoom restore keeps focus");
                String::from("zoom_restore")
            }
        };
    }

    match rng.below(100) {
        0..=21 => {
            let id = op_split(rt, rng);
            format!("split {id:?}")
        }
        22..=37 => {
            if rt.layout().leaf_count() > 1 {
                let id = op_close(rt);
                format!("close {id:?}")
            } else {
                let target = focused(rt);
                assert!(rt.set_focus(target), "singleton focus must stay valid");
                String::from("close(refused: singleton)")
            }
        }
        38..=53 => {
            let ids = rt.layout().leaf_ids();
            let target = ids[rng.below(ids.len())];
            assert!(rt.set_focus(target), "leaf focus target must be live");
            format!("set_focus {target:?}")
        }
        54..=63 => {
            let dir = [
                FocusDirection::Next,
                FocusDirection::Prev,
                FocusDirection::Up,
                FocusDirection::Down,
            ][rng.below(4)];
            let moved = rt.move_focus(dir);
            if let Some(id) = moved {
                assert!(rt.layout().leaf_ids().contains(&id));
            }
            format!("move_focus {moved:?}")
        }
        64..=74 => {
            if rt.workspace_count() < MAX_WORKSPACES {
                let index = rt.workspace_new().expect("workspace below capacity");
                format!("workspace_new {index}")
            } else {
                String::from("workspace_new(capacity)")
            }
        }
        75..=87 => {
            let index = rng.below(rt.workspace_count());
            assert!(rt.workspace_switch(index), "live workspace must switch");
            format!("workspace_switch {index}")
        }
        88..=92 => {
            let index = rt.workspace_next();
            format!("workspace_next {index}")
        }
        93..=96 => {
            if rt.workspace_count() >= 2 {
                let mut target = rng.below(rt.workspace_count() - 1);
                if target >= rt.active_workspace_index() {
                    target += 1;
                }
                let moved = rt
                    .workspace_move_focused_to(target)
                    .expect("moving to a live other workspace must succeed");
                assert!(
                    rt.layout().find_leaf(moved).is_none(),
                    "moved leaf left the source"
                );
                format!("workspace_move {moved:?} -> {target}")
            } else {
                String::from("workspace_move(single)")
            }
        }
        97..=99 => {
            if rt.workspace_count() >= 2 {
                let closed = match rt.workspace_close_request() {
                    WsCloseRequest::Closed { killed } => {
                        assert_eq!(killed, 0, "headless runtimes own no sessions");
                        true
                    }
                    WsCloseRequest::Pending { .. } => {
                        assert!(rt.confirm_pending_ws_close());
                        true
                    }
                };
                assert!(closed);
                assert!(
                    rt.workspace_count() >= 1,
                    "last workspace resets, never empties"
                );
                String::from("workspace_close")
            } else {
                String::from("workspace_close(single)")
            }
        }
        _ => {
            // Zoom the focused leaf; restore is handled on later iterations.
            if leaves > 1 {
                let zoomed = focused(rt);
                let view = rt
                    .layout()
                    .find_leaf(zoomed)
                    .cloned()
                    .expect("focused leaf exists");
                let backup = rt.layout().clone();
                rt.set_layout(LayoutNode::leaf(view));
                assert_eq!(rt.layout().leaf_ids(), vec![zoomed]);
                *zoom_backup = Some(backup);
                String::from("zoom")
            } else {
                String::from("zoom(single)")
            }
        }
    }
}

/// Runs a full randomized sequence for one seed.
fn run_sequence(seed: u64, steps: usize) {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let mut rng = Rng::new(seed);
    let mut zoom_backup: Option<LayoutNode> = None;
    check_invariants(&mut rt, "initial");

    for step in 0..steps {
        let leaves = check_invariants(&mut rt, "pre-op");
        let label = random_op(&mut rt, &mut rng, leaves, &mut zoom_backup);
        let context = format!("seed {seed} step {step} op `{label}`");
        check_invariants(&mut rt, &context);
    }

    // Deterministic finish: leave the last zoomed state restored.
    if let Some(backup) = zoom_backup.take() {
        rt.set_layout(backup);
        check_invariants(&mut rt, "final restore");
    }
}

#[test]
fn randomized_layout_ops_keep_view_identity_unique() {
    for seed in [0x5EED_0405, 0x0BAD_F00D, 0xC0FF_EE00] {
        run_sequence(seed, 240);
    }
}

#[test]
fn split_allocator_never_reuses_a_live_id() {
    // Deterministic companion to the randomized scan: ids held by inactive
    // slots are live, so a split in the active workspace must skip them.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    assert_eq!(rt.next_view_id_global(), ViewId::new(2));
    rt.workspace_new().expect("ws2");
    let inactive = rt.focused_view().expect("ws2 focus");
    assert!(rt.workspace_switch(0));
    let candidate = rt.next_view_id_global();
    assert_ne!(candidate, inactive, "inactive-slot ids are still live");
    assert!(
        !rt.layout().leaf_ids().contains(&candidate),
        "allocator must skip ids held by the active layout"
    );
}

#[test]
fn zoom_round_trip_preserves_ids_and_focus() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let mut rng = Rng::new(0x2007_0405);
    let second = op_split(&mut rt, &mut rng);
    let focus = focused(&rt);
    let backup = rt.layout().clone();

    let view = rt.layout().find_leaf(focus).cloned().expect("leaf");
    rt.set_layout(LayoutNode::leaf(view));
    assert_eq!(rt.layout().leaf_ids(), vec![focus], "zoom shows one leaf");
    assert_eq!(focused(&rt), focus, "zoom keeps focus");
    assert!(
        !rt.layout().leaf_ids().contains(&second),
        "zoom hides the sibling without destroying it"
    );

    rt.set_layout(backup);
    assert!(
        rt.layout().leaf_ids().contains(&second),
        "restore brings back the sibling"
    );
    assert_eq!(focused(&rt), focus, "restore keeps focus");
    check_invariants(&mut rt, "zoom round trip");
}

#[test]
fn close_last_leaf_refuses_and_move_replaces_source_leaf() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let only = focused(&rt);
    let mut layout = rt.layout().clone();
    assert!(
        !remove_leaf(&mut layout, only),
        "the last leaf is never removable"
    );
    assert!(!rt.set_focus(ViewId::new(99)), "unknown ids are rejected");

    let ws2 = rt.workspace_new().expect("ws2");
    rt.workspace_switch(0);
    let moved = rt.workspace_move_focused_to(ws2).expect("move");
    assert_eq!(moved, only, "move preserves the ViewId");
    assert_eq!(
        rt.layout().leaf_count(),
        1,
        "single-leaf source gets a fresh leaf"
    );
    assert_ne!(focused(&rt), moved, "the moved leaf left the source layout");
    check_invariants(&mut rt, "move singles");
}
