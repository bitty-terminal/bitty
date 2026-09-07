//! CTX-0228 regression: split/zoom (and every geometry-only layout or
//! focus change) forces a full present with no PTY bytes.
//!
//! Live evidence (`recording/live-dogfood/` shots 24/25/26/29): the layout
//! tree split instantly (`leafs=2`) but the presented frame stayed
//! bit-identical until the next PTY-output damage repainted; same staleness
//! after zoom-off. Root cause: geometry-only changes must force a full
//! redraw — damage tracking over PTY generations alone misses them.
//!
//! All tests are headless and deterministic: they assert `tick` presents
//! (`Some` with a non-empty draw list) without feeding PTY bytes and
//! without wall-clock waits, then assert the frame returns to idle.

use bitty_runtime::{FocusDirection, LayoutNode, Runtime, SplitAxis, View, ViewId};

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    )
}

fn assert_full_present(stats: Option<bitty_runtime::PresentStats>, what: &str) {
    let stats = stats.unwrap_or_else(|| panic!("{what} without PTY bytes must present"));
    assert!(stats.headless, "{what} must present headlessly");
    assert!(
        stats.fills > 0,
        "{what} draw list must carry fills (full-dirty)"
    );
}

#[test]
fn split_geometry_only_forces_full_present() {
    let mut rt = Runtime::with_defaults().expect("build");
    let _ = rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None, "must idle before split");
    rt.set_layout(two_pane());
    assert_eq!(rt.leaf_count(), 2);
    let stats = rt.tick();
    assert_full_present(stats, "split");
    assert!(
        rt.headless_rgba().is_some_and(|b| !b.is_empty()),
        "split must composite a frame"
    );
    assert_eq!(rt.tick(), None, "must return to idle after split present");
}

#[test]
fn zoom_on_and_off_force_full_present() {
    let mut rt = Runtime::with_defaults().expect("build");
    let _ = rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None);
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split before zoom");
    assert_eq!(rt.tick(), None);
    // Zoom on: collapse to the focused leaf.
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    assert_eq!(rt.leaf_count(), 1);
    assert_full_present(rt.tick(), "zoom-on");
    assert_eq!(rt.tick(), None);
    // Zoom off: restore the split tree.
    rt.set_layout(two_pane());
    assert_eq!(rt.leaf_count(), 2);
    assert_full_present(rt.tick(), "zoom-off");
    assert_eq!(rt.tick(), None, "must idle after zoom-off present");
}

#[test]
fn reflow_geometry_only_forces_full_present() {
    let mut rt = Runtime::with_defaults().expect("build");
    let _ = rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None);
    let _ = rt.reflow_layout();
    assert_full_present(rt.tick(), "reflow_layout");
    assert_eq!(rt.tick(), None);
}

#[test]
fn focus_moves_force_full_present() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split before focus");
    assert_eq!(rt.tick(), None);
    let before = rt.focused_view();
    let next = rt.move_focus(FocusDirection::Next);
    assert!(next.is_some(), "two panes must have a next focus");
    if next != before {
        assert_full_present(rt.tick(), "move_focus");
        assert_eq!(rt.tick(), None);
    }
    // Re-selecting the already-focused pane is a no-op: must stay idle
    // (no over-damage spin).
    let focused = rt.focused_view().expect("focused");
    assert!(rt.set_focus(focused));
    assert_eq!(rt.tick(), None, "re-selecting focus must not present");
}

#[test]
fn layout_mut_borrow_without_explicit_dirty_still_presents() {
    let mut rt = Runtime::with_defaults().expect("build");
    let _ = rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None);
    // Mutate through the borrow without calling `mark_layout_dirty`:
    // tick's allocation comparison must still force a full present.
    *rt.layout_mut() = two_pane();
    assert_eq!(rt.leaf_count(), 2);
    assert_full_present(rt.tick(), "layout_mut tree replacement");
    assert_eq!(rt.tick(), None);
}

#[test]
fn set_focus_change_forces_full_present() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split before set_focus");
    assert_eq!(rt.tick(), None);
    let target = ViewId::new(2);
    assert!(rt.set_focus(target));
    assert_eq!(rt.focused_view(), Some(target));
    assert_full_present(rt.tick(), "set_focus");
    assert_eq!(rt.tick(), None);
}
