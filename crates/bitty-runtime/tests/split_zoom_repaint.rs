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
//!
//! CTX-0234/CTX-0359 regression (same file: split-present mapping, live
//! evidence `recording/live-verify-0220/` shots 10/12): leaves WITHOUT a
//! pane shell session (ctl splits never spawn one; spawn failures) used to
//! render the shared primary top-left viewport, so one shell duplicated
//! across N tiles (three-column repeat + marker text in an unexpected tile
//! after zoom-off). Rule pinned here (CTX-0359): the runtime-global primary
//! grid belongs to exactly one leaf — the primary owner (the leaf focused
//! when the primary shell attached) — and is painted only there; every
//! other session-less leaf presents erased, whatever its focus or whether
//! any pane session exists. Pixel-asserted per tile through `headless_rgba`,
//! no wall clock, no PTY spawn except where noted.

use bitty_runtime::{
    AnimationPolicy, FocusDirection, LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId,
};

/// RFC-0002 (CTX-0341): animations default ON and a split/zoom/focus change
/// arms a bounded transition that presents extra frames. These tests pin the
/// CTX-0228 full-present-then-idle contract, not the animation feature, so
/// they build every runtime with animations disabled (instant present).
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

/// Pixel bounds of one leaf tile: the decorated content frame translated
/// exactly like the present layer (physical px + window padding inset).
/// CTX-0294: decoration (gaps/border) shrinks and offsets the content, so
/// the tile scan follows the present frames, not the raw cell allocation.
fn tile_pixels(rt: &Runtime, id: ViewId) -> (usize, usize, usize, usize) {
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have a present frame"));
    let x = usize::try_from(frame.content.x.max(0)).expect("content x fits") + pad;
    let y = usize::try_from(frame.content.y.max(0)).expect("content y fits") + pad;
    (
        x,
        y,
        frame.content.width as usize,
        frame.content.height as usize,
    )
}

/// Seam tolerance for cross-tile glyph overhang (CTX-0234 CI fix).
///
/// `Runtime::with_defaults` resolves the primary family through fontconfig,
/// which never fails (it substitutes) — so a host without the Nerd font
/// serves `M` from a proportional substitute (local probe: `Noto Sans CJK`
/// for a missing family) whose bitmap overhangs the 9px mono cell by up to
/// 3px rightwards into the adjacent tile. The present layer pushes per-leaf
/// glyphs with no per-tile scissor, so that overhang composites as a few
/// non-bg pixels inside the neighbour tile. Insetting by 8px (< one
/// 9x19 cell) ignores the seam while a true primary duplication still fills
/// the tile interior with thousands of ink pixels — the invariant is kept,
/// only the seam bleed is forgiven.
const SEAM_PX: usize = 8;

/// True when any pixel inside the leaf tile differs from the clear color.
///
/// The scan insets by [`SEAM_PX`] on every side so a neighbour tile's glyph
/// overhang (see above) never counts as duplication; interior content —
/// including the row-3 marker used below — is unaffected.
fn tile_has_ink(rt: &Runtime, id: ViewId) -> bool {
    tile_ink_pixels(rt, id) > 0
}

/// Ink pixel count inside the leaf tile (same seam-inset scan as
/// [`tile_has_ink`]). Lets a test separate "a lone cursor cell" from "a full
/// cloned marker row" without font introspection.
fn tile_ink_pixels(rt: &Runtime, id: ViewId) -> usize {
    let rgba = rt.headless_rgba().expect("rgba after present");
    let extent = rt.config().window_extent();
    let sw = usize::try_from(extent.width()).expect("surface width fits");
    let sh = usize::try_from(extent.height()).expect("surface height fits");
    assert_eq!(
        rgba.len(),
        sw * sh * 4,
        "headless surface must match the default window extent"
    );
    let bg = bitty_render::grid::DEFAULT_BG;
    let (tx, ty, tw, th) = tile_pixels(rt, id);
    assert!(
        tx + tw <= sw && ty + th <= sh,
        "tile must sit in the surface"
    );
    assert!(
        tw > 2 * SEAM_PX && th > 2 * SEAM_PX,
        "tile must exceed the seam inset"
    );
    let (x0, y0, w, h) = (
        tx + SEAM_PX,
        ty + SEAM_PX,
        tw - 2 * SEAM_PX,
        th - 2 * SEAM_PX,
    );
    let mut ink = 0usize;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let i = (y * sw + x) * 4;
            if rgba[i..i + 4] != bg {
                ink += 1;
            }
        }
    }
    ink
}

/// Writes a full-width marker row into the primary grid (cursor parks on the
/// next row, so row content and cursor ink never share a row).
fn write_primary_marker(rt: &mut Runtime, row_1based: u8, glyph: u8) {
    assert!((1..=9).contains(&row_1based), "single-digit rows only");
    let mut seq = vec![0x1b, b'[', b'0' + row_1based, b';', b'1', b'H'];
    seq.extend(std::iter::repeat_n(glyph, 80));
    rt.handle_pty_bytes(&seq);
    // Prove the CUP landed: the target row must read back full-width.
    let snap = rt.snapshot();
    let row = usize::from(row_1based - 1);
    assert!(
        snap.cells
            .chunks(snap.width)
            .nth(row)
            .is_some_and(|cells| cells.iter().all(|c| c.glyph as u8 == glyph)),
        "marker row {row_1based} must read back full-width"
    );
}

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
    let mut rt = instant_runtime();
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
    let mut rt = instant_runtime();
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
    let mut rt = instant_runtime();
    let _ = rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None);
    let _ = rt.reflow_layout();
    assert_full_present(rt.tick(), "reflow_layout");
    assert_eq!(rt.tick(), None);
}

#[test]
fn focus_moves_force_full_present() {
    let mut rt = instant_runtime();
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
    let mut rt = instant_runtime();
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
    let mut rt = instant_runtime();
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split before set_focus");
    assert_eq!(rt.tick(), None);
    let target = ViewId::new(2);
    assert!(rt.set_focus(target));
    assert_eq!(rt.focused_view(), Some(target));
    assert_full_present(rt.tick(), "set_focus");
    assert_eq!(rt.tick(), None);
}

#[test]
fn sessionless_unfocused_leaf_renders_blank_not_primary() {
    // CTX-0234 (live shots 10/12): with no pane session anywhere (the ctl
    // split shape — no shell is ever spawned), every leaf rendered the
    // shared primary viewport, duplicating one shell across all tiles.
    // CTX-0359: only the leaf that owns the primary shell may paint the
    // primary grid; the other leaf stays erased.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 3, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    assert_eq!(rt.tick(), None);
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_full_present(rt.tick(), "split");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "primary owner leaf keeps the primary grid"
    );
    assert!(
        !tile_has_ink(&rt, ViewId::new(2)),
        "session-less non-owner leaf must not duplicate primary (live dup columns)"
    );
    assert_eq!(rt.tick(), None);
}

#[test]
fn sessionless_primary_stays_with_owner_when_focus_moves() {
    // CTX-0359 (replaces the CTX-0234 focus-follow pin): the primary grid
    // belongs to the leaf that owns the primary shell, not to whatever leaf
    // is focused. Pre-fix, focusing a session-less `ctl view split` tile
    // cloned the primary grid into it and blanked the owner — while typing
    // in the clone still drove the owner's shell.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 3, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split");
    assert!(rt.set_focus(ViewId::new(2)));
    assert_full_present(rt.tick(), "focus moves off the primary owner");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "primary owner keeps its grid while unfocused"
    );
    // The focused empty leaf may carry a lone cursor cell (present gates the
    // cursor on focus), but never a cloned marker row: full-row cloning
    // would put v:2's ink within a few percent of v:1's.
    let owner_ink = tile_ink_pixels(&rt, ViewId::new(1));
    let focused_ink = tile_ink_pixels(&rt, ViewId::new(2));
    assert!(
        focused_ink * 8 < owner_ink,
        "focused session-less non-owner must stay erased apart from the cursor \
         (owner={owner_ink}px, focused={focused_ink}px)"
    );
    assert_eq!(rt.tick(), None);
}

#[cfg(unix)]
#[test]
fn mixed_shape_never_clones_primary_into_sessionless_leaf() {
    // CTX-0359 repro 2 (CTX-0358 findings): v:1 session-less primary owner,
    // v:2 live pane, v:3 session-less. Pre-fix the CTX-0255 co-paint arm
    // cloned the primary grid into v:3 (any session present), so two tiles
    // painted the same shell. Only the primary owner may paint primary.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 2, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    let three = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
        ),
    );
    rt.set_layout(three);
    assert!(
        rt.set_focus(ViewId::new(1)),
        "pin focus on the primary owner"
    );
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 24)
        .expect("pane shell must spawn");
    let mut seq = vec![0x1b, b'[', b'6', b';', b'1', b'H'];
    seq.extend(std::iter::repeat_n(b'P', 40));
    rt.handle_pane_bytes(ViewId::new(2), &seq);
    assert_full_present(rt.tick(), "mixed three-way split");
    assert!(rt.set_focus(ViewId::new(2)), "focus the live pane");
    assert_full_present(rt.tick(), "focus the live pane");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "primary owner co-paints while unfocused (CTX-0255 preserved)"
    );
    assert!(
        tile_has_ink(&rt, ViewId::new(2)),
        "live pane keeps its own grid"
    );
    assert!(
        !tile_has_ink(&rt, ViewId::new(3)),
        "session-less v:3 must not clone primary (repro: both session-less tiles inked)"
    );
    assert_eq!(rt.tick(), None);
}

#[test]
fn workspace_new_leaf_renders_empty_not_previous_primary() {
    // CTX-0359 repro 3a (CTX-0358 findings): the fresh workspace leaf is
    // focused and session-less, so the present fallback painted the previous
    // workspace's primary grid into it. A workspace with no shell yet must
    // render empty; the old workspace's primary stays with its owner.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 2, b'W');
    assert!(rt.tick().is_some(), "first tick presents");
    rt.workspace_new().expect("new workspace");
    let new_id = rt.focused_view().expect("fresh workspace leaf is focused");
    assert_ne!(new_id, ViewId::new(1), "fresh leaf must be a new view");
    assert_full_present(rt.tick(), "workspace switch");
    assert!(
        !tile_has_ink(&rt, new_id),
        "new workspace leaf must not paint the previous workspace's primary grid"
    );
    // Switching back restores the primary owner's grid untouched.
    assert!(rt.workspace_switch(0));
    assert_full_present(rt.tick(), "switch back to the primary workspace");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "primary owner keeps its grid after the workspace round trip"
    );
    assert_eq!(rt.tick(), None);
}

#[test]
fn zoom_off_does_not_duplicate_primary_into_sessionless_leaves() {
    // Live L8 shape: three session-less tiles side by side (ctl splits v4/v5
    // plus primary v1) rendered the same primary viewport three times.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 3, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    let three = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(5), 40, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(4), 40, 24)),
        ),
    );
    rt.set_layout(three);
    // Focus retains v1 (it exists in the new tree); pin it explicitly.
    assert!(rt.set_focus(ViewId::new(1)));
    assert_full_present(rt.tick(), "three-way session-less split");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "focused leaf shows primary"
    );
    assert!(
        !tile_has_ink(&rt, ViewId::new(5)),
        "session-less v5 must not repeat primary (shot 12 left repeat)"
    );
    assert!(
        !tile_has_ink(&rt, ViewId::new(4)),
        "session-less v4 must not repeat primary (shot 12 right repeat)"
    );
    assert_eq!(rt.tick(), None);
}

#[cfg(unix)]
#[test]
fn mixed_primary_plus_session_copaints_on_focus_v2() {
    // CTX-0255 live repro (02-focus-v2 left blank, 03-refocus-v1 both paint),
    // preserved by CTX-0359 ownership: keymap-split shape is mixed — v:1 is
    // the primary owner (session-less, paints the primary grid), v:2 owns a
    // pane session. Focusing v:2 must NOT blank the primary home tile; both
    // tiles co-paint. Refocus v:1 keeps both.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 2, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 24)
        .expect("pane shell must spawn");
    let mut seq = vec![0x1b, b'[', b'6', b';', b'1', b'H'];
    seq.extend(std::iter::repeat_n(b'P', 40));
    rt.handle_pane_bytes(ViewId::new(2), &seq);
    assert_full_present(rt.tick(), "pane bytes present");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "primary home keeps ink before focus move"
    );
    assert!(
        tile_has_ink(&rt, ViewId::new(2)),
        "pane tile keeps ink before focus move"
    );
    assert!(rt.set_focus(ViewId::new(2)));
    assert_full_present(rt.tick(), "focus v:2");
    assert!(
        tile_has_ink(&rt, ViewId::new(2)),
        "focused pane tile keeps its own grid"
    );
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "CTX-0255: unfocused primary home must co-paint, not blank"
    );
    assert_eq!(rt.tick(), None);
    assert!(rt.set_focus(ViewId::new(1)));
    assert_full_present(rt.tick(), "refocus v:1");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "refocused primary home keeps ink"
    );
    assert!(
        tile_has_ink(&rt, ViewId::new(2)),
        "pane tile keeps ink after refocus"
    );
    assert_eq!(rt.tick(), None);
}

#[cfg(unix)]
#[test]
fn pane_session_tile_shows_only_its_own_grid() {
    // Guard for the sessioned path (live L1 keymap splits): each tile shows
    // exactly its own grid — primary marker in the primary tile only, pane
    // marker in the pane tile only, never crossed.
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 2, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 24)
        .expect("pane shell must spawn");
    // Pane marker on its own row 6 (1-based), primary marker stays on row 2.
    let mut seq = vec![0x1b, b'[', b'6', b';', b'1', b'H'];
    seq.extend(std::iter::repeat_n(b'P', 40));
    rt.handle_pane_bytes(ViewId::new(2), &seq);
    assert_full_present(rt.tick(), "pane bytes present");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "primary tile keeps primary content"
    );
    assert!(
        tile_has_ink(&rt, ViewId::new(2)),
        "pane tile shows its own grid"
    );
    assert_eq!(rt.tick(), None);
}
