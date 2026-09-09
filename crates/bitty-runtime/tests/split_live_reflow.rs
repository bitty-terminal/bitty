//! CTX-0269: live split must resize pane grids (firing reflow), not just clip.
//!
//! Live proof (`recording/live-reflow/VERDICT.md`): headless `State::resize`
//! reflow passes, but a live split leaves the pane grid at the stale
//! pre-split width (~155 cols shown in a ~77-col pane) — long-line tails are
//! clipped, never rewrapped. Widen-back restores (no state loss), proving the
//! bytes survive and only the split-path resize is missing.
//!
//! Root direction: both live split paths (keymap `NewSplit` in
//! `bitty-app::chrome_keys`, `ctl view split` in `bitty-app::ctl`) funnel
//! into `Runtime::set_layout`, which re-syncs pane *sessions* but never
//! resizes the shared primary grid (or the primary PTY winsize). `tick` then
//! only clips via `viewport_snapshot` — the CTX-0266 reflow never fires.
//!
//! These tests drive the split through `set_layout` (the same funnel the
//! keymap arm uses — never a direct `State::resize`), so they fail while the
//! funnel skips the resize and pass once split resizes per-pane grids.

use bitty_runtime::{LayoutNode, Runtime, SplitAxis, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("default headless runtime must build")
}

/// Mirror of the live split shape: clone the tree, split the focused leaf
/// right (`Horizontal` axis, new pane last — the keymap `shift+alt+l` / `ctl
/// view split --right` geometry), install via `set_layout`.
fn live_split_focused_right(rt: &mut Runtime) -> ViewId {
    let focused = rt.focused_view().expect("live runtime always has focus");
    let new_id = ViewId::new(
        rt.layout()
            .leaf_ids()
            .iter()
            .map(|id| id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1),
    );
    let mut layout = rt.layout().clone();
    assert!(
        split_focused_leaf(&mut layout, focused, new_id),
        "focused leaf must be splittable"
    );
    rt.set_layout(layout);
    new_id
}

/// Minimal in-test copy of the focused-leaf split the keymap/`ctl` arms
/// perform (right-hand pane appended). Single-leaf roots hit the same
/// `LayoutNode::split` construction; nested trees recurse like the app.
fn split_focused_leaf(layout: &mut LayoutNode, focused: ViewId, new_id: ViewId) -> bool {
    match layout {
        LayoutNode::Leaf(view) if view.id() == focused => {
            let old = view.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            *layout = LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(old),
                LayoutNode::leaf(fresh),
            );
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_focused_leaf(first, focused, new_id)
                || split_focused_leaf(second, focused, new_id)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_focused_leaf(c, focused, new_id)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_focused_leaf(base, focused, new_id)
                || split_focused_leaf(overlay, focused, new_id)
        }
        _ => false,
    }
}

fn snapshot_text(snap: &bitty_term_state::Snapshot) -> String {
    let mut out = String::new();
    for (i, cell) in snap.cells.iter().enumerate() {
        if cell.spacer {
            continue;
        }
        out.push(cell.glyph);
        if (i + 1) % snap.width == 0 {
            out.push('\n');
        }
    }
    out
}

fn nonblank_rows(snap: &bitty_term_state::Snapshot) -> usize {
    snap.cells
        .chunks(snap.width)
        .filter(|row| {
            row.iter()
                .any(|c| !c.spacer && c.glyph != ' ' && c.glyph != '\0')
        })
        .count()
}

#[test]
fn live_split_resizes_primary_grid_to_focused_allocation_and_reflows() {
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    assert_eq!(rt.snapshot().width, 80, "headless baseline is 80 cols");

    // Long line longer than either post-split pane: 100 Q's + 9-char tail
    // = 109 cells (the live 280-char marker scaled to the 80-col grid).
    let mut line = "Q".repeat(100);
    line.push_str("<TAIL100>");
    assert_eq!(line.len(), 109);
    let mut bytes = line.into_bytes();
    bytes.extend_from_slice(b"\r\n");
    rt.handle_pty_bytes(&bytes);
    assert_eq!(
        nonblank_rows(&rt.snapshot()),
        2,
        "109 cells at 80 cols wrap to 2 rows pre-split"
    );

    // LIVE SHAPE: split through the keymap/`ctl` funnel (set_layout).
    let new_id = live_split_focused_right(&mut rt);
    assert_eq!(rt.leaf_count(), 2);
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));

    let focused_alloc = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(1))
        .map(|(_, r)| r)
        .expect("focused allocation");
    assert_eq!(focused_alloc.width, 40, "80-col container splits 40/40");
    let new_alloc = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == new_id)
        .map(|(_, r)| r)
        .expect("new leaf allocation");
    assert_eq!(new_alloc.width, 40);

    // THE CTX-0269 INVARIANT: every pane grid matches its allocation.
    // Pre-fix the primary grid keeps the stale 80-col width (present clips
    // to the 40-col tile, tails invisible — the live verdict).
    let snap = rt.snapshot();
    assert_eq!(
        snap.width, focused_alloc.width as usize,
        "primary grid must shrink to the focused pane allocation on live split"
    );

    // Reflow ran at the new width with the CTX-0266 (#441) rewrap
    // semantic (identical to window narrowing via `handle_resize`):
    // `reflow_primary` (state.rs:586) unwraps via soft-wrap flags and
    // rewraps each logical line via `rewrap_one_logical` (state.rs:2307),
    // so the overlapping prefix survives and the tail — inside the kept
    // region — stays visible. The live complaint about invisible tails is
    // fixed by the grid becoming honest: new output wraps at the narrow
    // width below.
    let snap = rt.snapshot();
    let row0: String = snap.cells[0..40]
        .iter()
        .filter(|c| !c.spacer)
        .map(|c| c.glyph)
        .collect();
    assert_eq!(row0, "Q".repeat(40), "narrow keeps the overlapping prefix");
    assert!(
        snapshot_text(&rt.snapshot()).contains("<TAIL100>"),
        "tail inside the kept region must stay visible after reflow"
    );

    // Split geometry forces a present even with no new PTY bytes.
    assert!(
        rt.tick().is_some(),
        "split reflow must force a full present"
    );
}

#[test]
fn live_split_fresh_output_wraps_at_narrow_width() {
    // Live-05 analog: a long line printed AFTER the split must wrap at the
    // narrowed pane width. Pre-fix the grid keeps the stale 80-col width
    // (fresh output wraps at 80, only ~2 rows paint in the 40-col tile).
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    live_split_focused_right(&mut rt);
    assert_eq!(rt.snapshot().width, 40);

    let mut line = "Q".repeat(100);
    line.push_str("<TAIL100>");
    let mut bytes = line.into_bytes();
    bytes.extend_from_slice(b"\r\n");
    rt.handle_pty_bytes(&bytes);

    assert_eq!(
        nonblank_rows(&rt.snapshot()),
        3,
        "109 cells at the narrowed 40 cols must wrap to 3 rows"
    );
    assert!(
        snapshot_text(&rt.snapshot()).contains("<TAIL100>"),
        "fresh tail must be visible in the narrowed pane"
    );
    assert!(rt.tick().is_some(), "fresh output must present");
}

#[test]
fn live_split_then_close_restores_primary_grid_without_loss() {
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    let mut line = "Q".repeat(100);
    line.push_str("<TAIL100>");
    let mut bytes = line.into_bytes();
    bytes.extend_from_slice(b"\r\n");
    rt.handle_pty_bytes(&bytes);

    live_split_focused_right(&mut rt);
    assert_eq!(rt.snapshot().width, 40);

    // LIVE widen-back shape: close the new leaf through the same funnel.
    let focused = rt.focused_view().expect("focus");
    let mut layout = rt.layout().clone();
    close_leaf(&mut layout, focused);
    rt.set_layout(layout);
    assert_eq!(rt.leaf_count(), 1);

    // Grid widens back to the container. CTX-0266 (#441) unwrap/rewrap
    // semantic (same as window widen): the narrowed rows unwrap via
    // soft-wrap flags and rewrap into fewer, fuller rows — no content
    // loss, no blank pad where content rewraps.
    assert_eq!(rt.snapshot().width, 80, "close must restore full width");
    let snap = rt.snapshot();
    let row0: String = snap.cells[0..80]
        .iter()
        .filter(|c| !c.spacer)
        .map(|c| c.glyph)
        .collect();
    assert_eq!(
        row0,
        "Q".repeat(80),
        "widen-back unwraps (#441) and rewraps the logical line into full rows"
    );
    assert!(
        snapshot_text(&rt.snapshot()).contains("<TAIL100>"),
        "widen-back must not lose the tail"
    );
    assert_eq!(
        nonblank_rows(&rt.snapshot()),
        2,
        "109 cells at 80 cols wrap back to 2 rows"
    );
}

/// Minimal close-leaf mirror (sibling promotion) for the widen-back shape.
fn close_leaf(layout: &mut LayoutNode, target: ViewId) {
    match layout {
        LayoutNode::Split { first, second, .. } => {
            // Test shape is a single root split: promote the non-target side.
            let promoted = if first.leaf_ids().contains(&target) {
                (**second).clone()
            } else {
                (**first).clone()
            };
            *layout = promoted;
        }
        _ => panic!("test shape is a single root split"),
    }
}

// POSIX-only: spawns /bin/sh, which does not exist on windows-latest
// (same gate as the pane-session live tests in `workspaces.rs`).
#[cfg(unix)]
#[test]
fn live_split_resizes_primary_and_pane_pty_winsize() {
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    rt.spawn_shell("/bin/sh").expect("primary shell must spawn");
    assert_eq!(
        rt.pty_size(),
        Some((80, 24)),
        "primary winsize starts at the container geometry"
    );

    // Keymap shape: split, then size the fresh leaf's shell to its
    // allocation (exactly what the `NewSplit` arm passes to spawn).
    let new_id = live_split_focused_right(&mut rt);
    let allocs = rt.layout_allocations();
    let (cols, rows) = allocs
        .iter()
        .find(|(id, _)| *id == new_id)
        .map(|(_, r)| (r.width, r.height))
        .expect("new leaf allocation");
    rt.spawn_shell_for_view(new_id, "/bin/sh", &[], cols, rows)
        .expect("pane shell must spawn");
    assert_eq!(rt.pane_pty_size(&new_id), Some((cols, rows)));

    // CTX-0269: the primary PTY winsize (SIGWINCH path) follows the focused
    // allocation on the split funnel — pre-fix it kept the stale 80x24.
    assert_eq!(
        rt.pty_size(),
        Some((40, 24)),
        "primary winsize must follow the focused pane allocation"
    );

    // Second split of the pane leaf: `sync_pane_geometry` on the same
    // funnel must shrink the pre-existing pane session grid + winsize.
    assert!(rt.set_focus(new_id), "focus must move to the pane leaf");
    let third_id = live_split_focused_right(&mut rt);
    let _ = third_id;
    let pane_alloc = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == new_id)
        .map(|(_, r)| r)
        .expect("shrunk pane allocation");
    assert_eq!(pane_alloc.width, 20, "40-col pane splits 20/20");
    let pane_snap = rt
        .pane_snapshot(&new_id)
        .expect("pane session survives the second split");
    assert_eq!(
        pane_snap.width, pane_alloc.width as usize,
        "pane session grid must follow its leaf on re-split"
    );
    assert_eq!(
        rt.pane_pty_size(&new_id),
        Some((pane_alloc.width, pane_alloc.height)),
        "pane winsize must follow its leaf on re-split"
    );
}

// POSIX-only (`/bin/sh` for the mixed shape; same gate as above).
//
// CTX-0269 requirement 3: the unfocused-blank / v:2-blank reports are a
// DIFFERENT root cause from the reflow bug — the deliberate CTX-0234
// fallback (pure session-less splits present unfocused leaves erased so one
// grid never duplicates across tiles) versus the CTX-0255 co-paint (any
// live pane session keeps primary painted in every session-less leaf).
// This test pins both arms behaviorally so the follow-up has a baseline.
#[cfg(unix)]
#[test]
fn unfocused_blank_is_session_shape_not_reflow() {
    // Pure session-less shape (`ctl view split`: no shell spawns): focus
    // the new leaf; the old leaf is erased by design.
    let mut pure_rt = make_runtime();
    pure_rt.tick().expect("first full redraw");
    pure_rt.handle_pty_bytes(b"HELLO-0269");
    let pure_new = live_split_focused_right(&mut pure_rt);
    assert_eq!(pure_rt.pane_count(), 0, "ctl shape spawns no session");
    assert!(pure_rt.set_focus(pure_new));
    let pure_stats = pure_rt.tick().expect("pure split must present");

    // Mixed shape (keymap split: new leaf owns a shell): focusing the pane
    // keeps primary co-painted in the session-less home leaf.
    let mut mixed_rt = make_runtime();
    mixed_rt.tick().expect("first full redraw");
    mixed_rt.handle_pty_bytes(b"HELLO-0269");
    let mixed_new = live_split_focused_right(&mut mixed_rt);
    mixed_rt
        .spawn_shell_for_view(mixed_new, "/bin/sh", &[], 40, 24)
        .expect("pane shell must spawn");
    mixed_rt.handle_pane_bytes(mixed_new, b"PANE");
    assert!(mixed_rt.set_focus(mixed_new));
    let mixed_stats = mixed_rt.tick().expect("mixed split must present");

    // Mixed paints primary (10 glyphs) in the home leaf PLUS the pane grid
    // (4 glyphs); pure paints primary (10) in the focused leaf and erases
    // the other (0). The gap pins the two different present arms.
    assert!(
        mixed_stats.glyphs > pure_stats.glyphs,
        "mixed shape must paint more glyphs than pure erased shape (mixed={} pure={})",
        mixed_stats.glyphs,
        pure_stats.glyphs,
    );
}
