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
//! CTX-0234 regression (same file: split-present mapping, live evidence
//! `recording/live-verify-0220/` shots 10/12): leaves WITHOUT a pane shell
//! session (ctl splits never spawn one; spawn failures) rendered the shared
//! primary top-left viewport, so one shell duplicated across N tiles
//! (three-column repeat + marker text in an unexpected tile after zoom-off).
//! Rule pinned here: a session-less leaf shows the primary grid ONLY while
//! focused (that is where multipane input routing sends typing with no
//! session — what-you-see-is-what-you-type); every other session-less leaf
//! presents erased. Pixel-asserted per tile through `headless_rgba`, no
//! wall clock, no PTY spawn except where noted.

use bitty_runtime::{FocusDirection, LayoutNode, Runtime, SplitAxis, View, ViewId};

/// Pixel bounds of one leaf tile: cell allocation translated exactly like
/// the present layer (live cell metrics at scale 1.0 + physical padding).
fn tile_pixels(rt: &Runtime, id: ViewId) -> (usize, usize, usize, usize) {
    let cfg = rt.config();
    let cw = usize::try_from(cfg.cell_width).expect("cell width fits");
    let ch = usize::try_from(cfg.cell_height).expect("cell height fits");
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let (_, rect) = rt
        .layout_allocations()
        .into_iter()
        .find(|(vid, _)| *vid == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have an allocation"));
    let x = usize::from(rect.x) * cw + pad;
    let y = usize::from(rect.y) * ch + pad;
    let w = usize::from(rect.width) * cw;
    let h = usize::from(rect.height) * ch;
    (x, y, w, h)
}

/// True when any pixel inside the leaf tile differs from the clear color.
fn tile_has_ink(rt: &Runtime, id: ViewId) -> bool {
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
    let (x0, y0, w, h) = tile_pixels(rt, id);
    assert!(x0 + w <= sw && y0 + h <= sh, "tile must sit in the surface");
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let i = (y * sw + x) * 4;
            if rgba[i..i + 4] != bg {
                return true;
            }
        }
    }
    false
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

#[test]
fn sessionless_unfocused_leaf_renders_blank_not_primary() {
    // CTX-0234 (live shots 10/12): with no pane session anywhere (the ctl
    // split shape — no shell is ever spawned), every leaf rendered the
    // shared primary viewport, duplicating one shell across all tiles.
    // Only the focused session-less leaf may show primary (input routes
    // there); the unfocused one must stay erased.
    let mut rt = Runtime::with_defaults().expect("build");
    write_primary_marker(&mut rt, 3, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    assert_eq!(rt.tick(), None);
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_full_present(rt.tick(), "split");
    assert!(
        tile_has_ink(&rt, ViewId::new(1)),
        "focused session-less leaf keeps the primary fallback"
    );
    assert!(
        !tile_has_ink(&rt, ViewId::new(2)),
        "unfocused session-less leaf must not duplicate primary (live dup columns)"
    );
    assert_eq!(rt.tick(), None);
}

#[test]
fn sessionless_primary_fallback_follows_focus() {
    // The primary fallback is input-routing truth: typing reaches the
    // primary shell only through the focused session-less leaf, so the
    // visible primary content must move with focus — never duplicate.
    let mut rt = Runtime::with_defaults().expect("build");
    write_primary_marker(&mut rt, 3, b'M');
    assert!(rt.tick().is_some(), "first tick presents");
    rt.set_layout(two_pane());
    assert_full_present(rt.tick(), "split");
    assert!(rt.set_focus(ViewId::new(2)));
    assert_full_present(rt.tick(), "focus moves primary fallback");
    assert!(
        tile_has_ink(&rt, ViewId::new(2)),
        "newly focused session-less leaf shows primary"
    );
    assert!(
        !tile_has_ink(&rt, ViewId::new(1)),
        "defocused session-less leaf must go blank, not keep a primary copy"
    );
    assert_eq!(rt.tick(), None);
}

#[test]
fn zoom_off_does_not_duplicate_primary_into_sessionless_leaves() {
    // Live L8 shape: three session-less tiles side by side (ctl splits v4/v5
    // plus primary v1) rendered the same primary viewport three times.
    let mut rt = Runtime::with_defaults().expect("build");
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
fn pane_session_tile_shows_only_its_own_grid() {
    // Guard for the sessioned path (live L1 keymap splits): each tile shows
    // exactly its own grid — primary marker in the primary tile only, pane
    // marker in the pane tile only, never crossed.
    let mut rt = Runtime::with_defaults().expect("build");
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
