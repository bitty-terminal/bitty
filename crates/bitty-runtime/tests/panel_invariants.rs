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
use std::collections::BTreeSet;

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

#[test]
fn inactive_workspace_close_preserves_active_layout_focus_and_owner() {
    // CTX-0414 F1 (issue #668): closing an inactive workspace used to reload
    // the active slot from its stale stash, reverting the active workspace's
    // live leaf edits (issue repro: 2 leaves -> 1). The active workspace,
    // its geometry, focus, and primary owner must survive both an index
    // above and an index below the active slot.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let mut rng = Rng::new(0x0414_0668);
    let second = op_split(&mut rt, &mut rng);
    let owner = rt
        .primary_view()
        .expect("the startup leaf owns the primary grid");
    let focus = focused(&rt);

    assert_eq!(rt.workspace_new().expect("ws2"), 1);
    assert_eq!(rt.workspace_new().expect("ws3"), 2);
    assert!(rt.workspace_switch(0), "back to ws1");
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(rt.layout().leaf_count(), 2, "round trip keeps the split");
    assert!(rt.layout().leaf_ids().contains(&second));

    let layout_before = rt.layout().clone();
    let frames_before = rt.present_frames();
    let leaves_before = rt.workspace_count();

    // Close the inactive workspace above the active slot (buggy code also
    // switched the active workspace here via `index.min(len - 1)`).
    let killed = rt.workspace_close_at(2).expect("close inactive ws3");
    assert_eq!(killed, 0, "headless workspace owns no sessions");
    assert_eq!(rt.workspace_count(), leaves_before - 1);
    assert_eq!(rt.active_workspace_index(), 0, "active workspace survives");
    assert_eq!(rt.layout().leaf_count(), 2, "live leaf edits preserved");
    assert_eq!(rt.layout(), &layout_before, "tree geometry preserved");
    assert_eq!(rt.present_frames(), frames_before, "frames preserved");
    assert_eq!(rt.focused_view(), Some(focus), "focus preserved");
    assert_eq!(rt.primary_view(), Some(owner), "primary owner preserved");

    // Close the inactive workspace below the active slot: removal shifts
    // the active index down but must keep the same live workspace loaded.
    let killed = rt.workspace_close_at(1).expect("close inactive ws2");
    assert_eq!(killed, 0);
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(rt.layout(), &layout_before);
    assert_eq!(rt.present_frames(), frames_before);
    assert_eq!(rt.focused_view(), Some(focus));
    assert_eq!(rt.primary_view(), Some(owner));
    check_invariants(&mut rt, "after inactive closes");
}

#[test]
fn closing_workspace_rehomes_moved_primary_owner() {
    // CTX-0414 F2 (CTX-0405 review follow-up): a workspace close that
    // destroys the primary owner leaf must re-home `primary_view` to a live
    // surviving leaf, never leave it dangling on a dead id.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let owner = rt
        .primary_view()
        .expect("the startup leaf owns the primary grid");
    assert_eq!(focused(&rt), owner, "startup focus owns the primary grid");

    // Move the owner leaf out of ws1 into ws2.
    let ws2 = rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    assert!(rt.set_focus(owner));
    let moved = rt.workspace_move_focused_to(ws2).expect("move the owner");
    assert_eq!(moved, owner);
    assert_eq!(
        rt.primary_view(),
        Some(owner),
        "a move never re-homes the owner"
    );

    // Closing ws2 destroys the moved owner: re-home to the focused
    // survivor of the active workspace.
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(
        rt.workspace_close_at(ws2).expect("close owner workspace"),
        0
    );
    let rehomed = rt
        .primary_view()
        .expect("primary owner must never dangle on a dead id");
    assert!(
        rt.layout().leaf_ids().contains(&rehomed),
        "re-homed owner must be a live leaf (WS-INV-7)"
    );
    assert_eq!(rehomed, focused(&rt), "re-homed to the focused survivor");
    assert!(rt.is_primary_view(&rehomed));
    assert_eq!(rt.workspace_count(), 1);
    check_invariants(&mut rt, "after owner-workspace close");
}

/// WS-INV-4 / F-1 (CTX-0567): a `ViewId` that was ever installed in a layout
/// must never be reissued after retirement. The high-water mark must record
/// the *outgoing* layout at every replacement, not only the incoming one.
///
/// The raw [`Runtime::layout_mut`] escape installs leaf ids without going
/// through an allocation funnel, so an id can be live without the allocator
/// having observed it. Retiring that id by replacing the layout with a lower
/// id must still quarantine it: retirement is tracked explicitly, never
/// inferred from the survivor's maximum.
#[test]
fn retired_view_ids_survive_lower_id_replacement_and_workspace_close() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let mut retired: BTreeSet<u64> = BTreeSet::new();

    // ws1: install high ids through the `layout_mut` escape and retire each by
    // replacing the live layout with a lower id. The escape is the only public
    // install path that does not itself raise the high-water mark.
    for raw in [20_u64, 30, 40] {
        let id = ViewId::new(raw);
        *rt.layout_mut() = LayoutNode::leaf(View::new(id, 80, 24));
        assert_eq!(rt.layout().leaf_ids(), vec![id]);
        retired.insert(raw);
        rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(4), 80, 24)));
    }

    // Retire escaped ids across several workspaces by closing each *active*
    // escaped slot without a prior switch: `remove_workspace` must fold the
    // outgoing live layout into the high-water before dropping it. The final
    // close resets the last workspace (single-leaf reallocation).
    for _ in 0..4 {
        let index = rt.workspace_new().expect("new workspace below capacity");
        assert_eq!(index, rt.active_workspace_index(), "new slot is active");
        // `workspace_new` committed a real allocated id; overwriting the slot
        // through the escape retires it as well.
        retired.insert(rt.focused_view().expect("new slot focus").0);
        let escaped = ViewId::new(100 + index as u64);
        *rt.layout_mut() = LayoutNode::leaf(View::new(escaped, 80, 24));
        retired.insert(escaped.0);
        rt.workspace_close_at(index).expect("close escaped slot");
        assert!(
            rt.workspace_count() >= 1,
            "last close resets, never empties"
        );
    }

    // Close the remaining last workspace: its live id is retired too.
    let last_view = rt.focused_view().expect("sole workspace focus");
    retired.insert(last_view.0);
    rt.workspace_close_at(0).expect("close last workspace");
    assert_eq!(
        rt.workspace_count(),
        1,
        "last close resets to one workspace"
    );
    assert_ne!(
        rt.focused_view(),
        Some(last_view),
        "the last-workspace reset allocates a fresh id"
    );

    // Allocate many new views, committing each (the split/close shape): none
    // may reuse a retired id, allocation stays strictly monotonic, and every
    // committed live id is unique.
    let mut last = 0_u64;
    for step in 0..128 {
        let id = rt.next_view_id_global();
        assert!(
            !retired.contains(&id.0),
            "retired ViewId {id:?} was reissued at step {step} (WS-INV-4/F-1)"
        );
        assert!(
            id.0 > last,
            "allocation must be strictly monotonic: {id:?} after {last}"
        );
        last = id.0;
        rt.set_layout(LayoutNode::leaf(View::new(id, 80, 24)));
        let live = rt.layout().leaf_ids();
        let unique: BTreeSet<u64> = live.iter().map(|leaf| leaf.0).collect();
        assert_eq!(unique.len(), live.len(), "live ViewIds must stay unique");
    }

    check_invariants(&mut rt, "after retirement stress");
}

/// WS-INV-4 / F-1 (CTX-0567): two consecutive `layout_mut` writes retire a
/// high id with no install funnel in between. The escape guard must fold the
/// id it is about to overwrite even when no `set_layout` follows; otherwise
/// the retired id is never observed and the allocator reissues it once
/// allocation climbs past it.
#[test]
fn consecutive_layout_mut_writes_quarantine_the_retired_id() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let escaped = ViewId::new(9);
    *rt.layout_mut() = LayoutNode::leaf(View::new(escaped, 80, 24));
    // Second escape retires `escaped` without any allocator-visible funnel.
    *rt.layout_mut() = LayoutNode::leaf(View::new(ViewId::new(2), 80, 24));
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(2)]);

    // Allocation climbs toward `escaped`; it must skip the retired id rather
    // than reissuing it (without the escape guard this loop hits 9).
    for step in 0..32 {
        let id = rt.next_view_id_global();
        assert_ne!(
            id, escaped,
            "ViewId retired by a bare layout_mut overwrite was reissued at step {step}"
        );
        rt.set_layout(LayoutNode::leaf(View::new(id, 80, 24)));
    }
}
