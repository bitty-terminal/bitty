//! Visual + geometry evidence for the Hyprland tiling fixes (issues #2521,
//! #2526, #2528).
//!
//! Each scenario builds the exact deterministic tree, computes allocations
//! with the headless solver, asserts the geometry the fix promises, and
//! renders a byte-stable SVG of the before/after layouts under
//! `tests/testdata/tiling-evidence/`. The SVG goldens are the human-readable
//! screenshots: regenerate with `TILING_EVIDENCE_UPDATE=1`, then compare
//! byte-exactly on every run so layout drift cannot land silently.
//!
//! Synthetic content only: fixed ids, no clock, no randomness, no host
//! paths. The pane colour is a pure function of the `ViewId`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bitty_platform::{CursorPosition, MouseButton, MouseEvent, PressState};
use bitty_runtime::{
    LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId, drop_plan_for_point, largest_area_leaf,
    splittable_axis,
};

const SCALE: u16 = 10;
const HEADER: u16 = 34;
const FOOTER: u16 = 22;
const CONTAINER: UiRect = UiRect::new(0, 0, 80, 24);

fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

fn pane_colour(id: ViewId) -> &'static str {
    const PALETTE: [&str; 6] = [
        "#1f3b57", "#24492e", "#4a2f52", "#553b1f", "#1f4a4a", "#4a1f2e",
    ];
    PALETTE[(id.0 as usize) % PALETTE.len()]
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Renders one deterministic layout diagram: pane rectangles with ids and
/// sizes, an optional focus ring, an optional pointer marker, and the tree
/// shape as a caption. Pure string builder; integer geometry only.
fn render_svg(
    title: &str,
    note: &str,
    tree: &LayoutNode,
    allocations: &[(ViewId, UiRect)],
    focus: Option<ViewId>,
    marker: Option<(u16, u16, &str)>,
) -> String {
    let width = CONTAINER.width * SCALE;
    let height = CONTAINER.height * SCALE + HEADER + FOOTER;
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="monospace">"#
    );
    let _ = writeln!(
        svg,
        r##"<rect width="{width}" height="{height}" fill="#0d1117"/>"##
    );
    let _ = writeln!(
        svg,
        r##"<text x="4" y="14" fill="#e6edf3" font-size="12">{}</text>"##,
        escape(title)
    );
    let _ = writeln!(
        svg,
        r##"<text x="4" y="28" fill="#9aa7b4" font-size="10">{}</text>"##,
        escape(note)
    );
    for (id, rect) in allocations {
        let x = rect.x * SCALE;
        let y = rect.y * SCALE + HEADER;
        let w = rect.width * SCALE;
        let h = rect.height * SCALE;
        let stroke = if focus == Some(*id) {
            "#f0c674"
        } else {
            "#0b0e11"
        };
        let stroke_width = if focus == Some(*id) { 3 } else { 1 };
        let _ = writeln!(
            svg,
            r##"<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{}" stroke="{stroke}" stroke-width="{stroke_width}"/>"##,
            pane_colour(*id)
        );
        let _ = writeln!(
            svg,
            r##"<text x="{}" y="{}" fill="#e6edf3" font-size="11">p{}</text>"##,
            x + 4,
            y + 14,
            id.0
        );
        let _ = writeln!(
            svg,
            r##"<text x="{}" y="{}" fill="#c3ced9" font-size="9">{}x{}</text>"##,
            x + 4,
            y + 26,
            rect.width,
            rect.height
        );
    }
    if let Some((x, y, label)) = marker {
        let mx = u32::from(x) * u32::from(SCALE);
        let my = u32::from(y) * u32::from(SCALE) + u32::from(HEADER);
        let _ = writeln!(
            svg,
            r##"<circle cx="{mx}" cy="{my}" r="4" fill="none" stroke="#ff7b72" stroke-width="2"/>"##
        );
        // Keep the label inside the canvas: anchor it left on the left half
        // and to the left of the marker on the right half.
        let label_x = if mx > u32::from(width) / 2 {
            mx.saturating_sub(78)
        } else {
            mx + 7
        };
        let _ = writeln!(
            svg,
            r##"<text x="{label_x}" y="{}" fill="#ff7b72" font-size="10">{}</text>"##,
            my.saturating_sub(6),
            escape(label)
        );
    }
    let tree_text = format!(
        "tree {} | leaves {:?}",
        shape(tree),
        tree.leaf_ids().iter().map(|id| id.0).collect::<Vec<_>>()
    );
    let _ = writeln!(
        svg,
        r##"<text x="4" y="{}" fill="#8b949e" font-size="10">{}</text>"##,
        CONTAINER.height * SCALE + HEADER + 15,
        escape(&tree_text)
    );
    svg.push_str("</svg>\n");
    svg
}

/// Compact tree shape (`H[1, V[2, 3]]`), deterministic and ASCII-only.
fn shape(node: &LayoutNode) -> String {
    match node {
        LayoutNode::Leaf(view) => format!("{}", view.id().0),
        LayoutNode::Split {
            axis,
            first,
            second,
            ..
        } => {
            let tag = match axis {
                SplitAxis::Horizontal => "H",
                SplitAxis::Vertical => "V",
            };
            format!("{tag}[{}, {}]", shape(first), shape(second))
        }
        LayoutNode::Stack(children) => format!(
            "S[{}]",
            children.iter().map(shape).collect::<Vec<_>>().join(", ")
        ),
        LayoutNode::Overlay { base, overlay, .. } => {
            format!("O[{}, {}]", shape(base), shape(overlay))
        }
    }
}

fn evidence_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/tiling-evidence")
        .join(name)
}

fn check_or_update(name: &str, artifact: &str) -> PathBuf {
    let path = evidence_path(name);
    if std::env::var("TILING_EVIDENCE_UPDATE").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create evidence dir");
        }
        std::fs::write(&path, artifact).expect("write golden artifact");
        return path;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing golden {name}; rerun with TILING_EVIDENCE_UPDATE=1"));
    assert_eq!(artifact, expected, "evidence drift for {name}");
    path
}

fn rect_of(allocations: &[(ViewId, UiRect)], id: u64) -> UiRect {
    allocations
        .iter()
        .find(|(leaf, _)| *leaf == ViewId::new(id))
        .map(|(_, rect)| *rect)
        .unwrap_or_else(|| panic!("p{} must be allocated", id))
}

// ---------------------------------------------------------------------------
// Issue #2521: drag insertion at the bottom-left column corner
// ---------------------------------------------------------------------------

#[test]
fn issue_2521_drag_insertion_before_after() {
    // 1 = full-height left column, 2 = top-right, 3 = bottom-right.
    let tree = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        leaf(1),
        LayoutNode::split(SplitAxis::Vertical, 0.5, leaf(2), leaf(3)),
    );
    // Pre-fix plan for the same pointer: nearest normalized edge picked the
    // left edge, carving a new column and pushing p1 into the middle.
    let mut before = tree.clone();
    assert!(before.reparent_leaf(
        ViewId::new(3),
        ViewId::new(1),
        SplitAxis::Horizontal,
        0.5,
        false
    ));
    // Fixed plan: corner drop stacks inside the target's column.
    let plan = drop_plan_for_point(
        &tree,
        CONTAINER,
        bitty_runtime::Gaps::ZERO,
        ViewId::new(3),
        bitty_runtime::UiPoint::new(2, 22),
    );
    let Some(bitty_runtime::DropPlan::Insert(spec)) = plan else {
        panic!("bottom-left corner must plan an insert: {plan:?}");
    };
    assert_eq!(spec.target, ViewId::new(1));
    assert_eq!(spec.axis, SplitAxis::Vertical);
    assert!(spec.after);
    let mut after = tree.clone();
    assert!(after.reparent_leaf(
        ViewId::new(3),
        spec.target,
        spec.axis,
        spec.ratio,
        spec.after
    ));
    assert_eq!(after.validate_structure(), Ok(3));

    let before_alloc = before.layout(CONTAINER);
    let after_alloc = after.layout(CONTAINER);
    // Buggy result: three full-height slivers, p1 displaced to the middle.
    assert_eq!(rect_of(&before_alloc, 1).width, 20);
    assert_eq!(rect_of(&before_alloc, 1).x, 20);
    assert_eq!(rect_of(&before_alloc, 2).height, 24);
    // Fixed result: left column stacked [p1, p3], p2 keeps the full right.
    assert_eq!(rect_of(&after_alloc, 1), UiRect::new(0, 0, 40, 12));
    assert_eq!(rect_of(&after_alloc, 3), UiRect::new(0, 12, 40, 12));
    assert_eq!(rect_of(&after_alloc, 2), UiRect::new(40, 0, 40, 24));

    let marker = (2, 22, "drop");
    let before_svg = render_svg(
        "#2521 drag move: before (0.0.26)",
        "bottom-left drop carved a new column; p1 re-homed to the middle",
        &before,
        &before_alloc,
        None,
        Some(marker),
    );
    let after_svg = render_svg(
        "#2521 drag move: after (fix)",
        "bottom-left drop splits the left column at the pointer edge",
        &after,
        &after_alloc,
        None,
        Some(marker),
    );
    let before_path = check_or_update("tiling-2521-before.svg", &before_svg);
    let after_path = check_or_update("tiling-2521-after.svg", &after_svg);
    eprintln!(
        "evidence: {} {}",
        before_path.display(),
        after_path.display()
    );
}

// ---------------------------------------------------------------------------
// Issue #2526: Mod+j from the top-right pane in a 1+1+2 layout
// ---------------------------------------------------------------------------

/// Pre-fix directional focus: nearest perpendicular center, then leaf
/// order. Returns the old `Down` target from the top-right pane.
fn legacy_down_target(allocations: &[(ViewId, UiRect)], from: ViewId) -> ViewId {
    let current = rect_of(allocations, from.0);
    let mut best: Option<(i64, usize, ViewId)> = None;
    for (order, (id, rect)) in allocations.iter().enumerate() {
        if *id == from || rect.y < current.y + current.height {
            continue;
        }
        let current_center = i64::from(current.x) * 2 + i64::from(current.width);
        let candidate_center = i64::from(rect.x) * 2 + i64::from(rect.width);
        let distance = (candidate_center - current_center).abs();
        let candidate = (distance, order, *id);
        if best.is_none_or(|best| candidate < best) {
            best = Some(candidate);
        }
    }
    best.map(|(_, _, id)| id).expect("a pane below")
}

#[test]
fn issue_2526_directional_focus_before_after() {
    // 1 + 1 + 2: left full height, top-right, bottom row 40/60 so the
    // bottom-right pane is wider and has the closer centre.
    let tree = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        leaf(1),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            leaf(2),
            LayoutNode::split(SplitAxis::Horizontal, 0.4, leaf(3), leaf(4)),
        ),
    );
    let allocations = tree.layout(CONTAINER);
    assert_eq!(rect_of(&allocations, 2), UiRect::new(40, 0, 40, 12));
    assert_eq!(rect_of(&allocations, 3), UiRect::new(40, 12, 16, 12));
    assert_eq!(rect_of(&allocations, 4), UiRect::new(56, 12, 24, 12));

    let from = ViewId::new(2);
    let before_target = legacy_down_target(&allocations, from);
    assert_eq!(
        before_target,
        ViewId::new(4),
        "pre-fix centre distance picked the wider bottom-right pane"
    );
    let mut focus = bitty_runtime::Focus::with_focus(from);
    let after_target = focus
        .move_focus(&tree, CONTAINER, bitty_runtime::FocusDirection::Down)
        .expect("down target");
    assert_eq!(
        after_target,
        ViewId::new(3),
        "fix follows the tree edge to the bottom-left pane"
    );

    let before_svg = render_svg(
        "#2526 Mod+j: before (0.0.26)",
        "down from p2 landed on p4 (nearest centre)",
        &tree,
        &allocations,
        Some(before_target),
        Some((60, 12, "Mod+j -> p4")),
    );
    let after_svg = render_svg(
        "#2526 Mod+j: after (fix)",
        "down from p2 lands on p3 (same split column edge)",
        &tree,
        &allocations,
        Some(after_target),
        Some((46, 12, "Mod+j -> p3")),
    );
    let before_path = check_or_update("tiling-2526-before.svg", &before_svg);
    let after_path = check_or_update("tiling-2526-after.svg", &after_svg);
    eprintln!(
        "evidence: {} {}",
        before_path.display(),
        after_path.display()
    );
}

// ---------------------------------------------------------------------------
// Issues #2521/#2528: eight default new-panel presses
// ---------------------------------------------------------------------------

/// Applies one split of `target` exactly like the runtime funnel.
fn split(
    node: &mut LayoutNode,
    target: ViewId,
    axis: SplitAxis,
    new_id: ViewId,
    first: bool,
) -> bool {
    match node {
        LayoutNode::Leaf(view) if view.id() == target => {
            let old = view.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            let (a, b) = if first {
                (LayoutNode::leaf(fresh), LayoutNode::leaf(old))
            } else {
                (LayoutNode::leaf(old), LayoutNode::leaf(fresh))
            };
            *node = LayoutNode::split(axis, 0.5, a, b);
            true
        }
        LayoutNode::Split {
            first: a,
            second: b,
            ..
        } => split(a, target, axis, new_id, first) || split(b, target, axis, new_id, first),
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|child| split(child, target, axis, new_id, first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split(base, target, axis, new_id, first) || split(overlay, target, axis, new_id, first)
        }
        LayoutNode::Leaf(_) => false,
    }
}

/// Pre-fix `Mod+N`: split the focused pane, axis from its own allocation,
/// side from the leaf-count spiral phase.
fn legacy_eight_panels() -> LayoutNode {
    let mut tree = leaf(1);
    let mut focused = ViewId::new(1);
    for step in 2..=9u64 {
        let allocations = tree.layout(CONTAINER);
        let rect = rect_of(&allocations, focused.0);
        let axis = bitty_runtime::smart_split_axis(rect, 1.0);
        let place_first = step
            .saturating_sub(2)
            .checked_rem(4)
            .is_some_and(|phase| phase >= 2);
        assert!(split(
            &mut tree,
            focused,
            axis,
            ViewId::new(step),
            place_first
        ));
        focused = ViewId::new(step);
    }
    tree
}

/// Fixed `Mod+N`: the runtime's balanced plan (largest slot, balanced axis,
/// after).
fn balanced_eight_panels() -> LayoutNode {
    let mut tree = leaf(1);
    for next in 2..=9u64 {
        let allocations = tree.layout(CONTAINER);
        let (target, rect) = largest_area_leaf(&allocations).expect("a leaf");
        let axis = splittable_axis(rect, 0).expect("usable split");
        assert!(split(&mut tree, target, axis, ViewId::new(next), false));
    }
    tree
}

#[test]
fn issue_2521_2528_eight_new_panels_before_after() {
    let before = legacy_eight_panels();
    let after = balanced_eight_panels();
    let before_alloc = before.layout(CONTAINER);
    let after_alloc = after.layout(CONTAINER);
    assert_eq!(before.validate_structure(), Ok(9));
    assert_eq!(after.validate_structure(), Ok(9));

    let smallest = |allocations: &[(ViewId, UiRect)]| {
        allocations
            .iter()
            .map(|(_, rect)| u32::from(rect.width) * u32::from(rect.height))
            .min()
            .expect("panes")
    };
    let largest = |allocations: &[(ViewId, UiRect)]| {
        allocations
            .iter()
            .map(|(_, rect)| u32::from(rect.width) * u32::from(rect.height))
            .max()
            .expect("panes")
    };
    // Pre-fix: a 5x1 sliver nested five levels deep (128:1 area spread).
    assert_eq!(smallest(&before_alloc), 5);
    assert!(largest(&before_alloc) / smallest(&before_alloc) > 60);
    // Fixed: every pane stays usable and within a 3x area band.
    for (id, rect) in &after_alloc {
        assert!(
            rect.width >= bitty_runtime::MIN_PANE_COLS
                && rect.height >= bitty_runtime::MIN_PANE_ROWS,
            "p{id:?} below the minimum: {rect:?}"
        );
    }
    assert!(largest(&after_alloc) <= smallest(&after_alloc) * 3);

    let before_svg = render_svg(
        "#2521/#2528 eight Mod+N: before (0.0.26)",
        "focused-pane halving nested 5x1 slivers by the sixth split",
        &before,
        &before_alloc,
        None,
        None,
    );
    let after_svg = render_svg(
        "#2521/#2528 eight Mod+N: after (fix)",
        "balanced bisect-largest keeps every pane usable",
        &after,
        &after_alloc,
        None,
        None,
    );
    let before_path = check_or_update("tiling-2528-before.svg", &before_svg);
    let after_path = check_or_update("tiling-2528-after.svg", &after_svg);
    eprintln!(
        "evidence: {} {}",
        before_path.display(),
        after_path.display()
    );
}

/// The runtime plan and the pure evidence builder must agree for every
/// press, so the screenshots cannot drift from the shipped algorithm.
#[test]
fn runtime_plan_matches_balanced_evidence_builder() {
    let mut rt = Runtime::with_defaults().expect("runtime");
    let mut tree = leaf(rt.focused_view().expect("focus").0);
    // Seed the runtime with a single leaf and verify each plan/split pair.
    rt.set_layout(tree.clone());
    for next in 2..=9u64 {
        let plan = rt.panel_split_plan().expect("plan");
        assert!(!plan.place_new_first, "balanced always places after");
        let allocations = tree.layout(CONTAINER);
        let (target, rect) = largest_area_leaf(&allocations).expect("a leaf");
        assert_eq!(plan.target, target);
        assert_eq!(plan.axis, splittable_axis(rect, 0).expect("usable"));
        assert!(split(
            &mut tree,
            plan.target,
            plan.axis,
            ViewId::new(next),
            plan.place_new_first
        ));
        rt.set_layout(tree.clone());
        rt.set_focus(ViewId::new(next));
    }
    assert_eq!(tree.leaf_ids(), balanced_eight_panels().leaf_ids());
}

// ---------------------------------------------------------------------------
// Issue #2572: 2x2 center-junction cross drag
// ---------------------------------------------------------------------------

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry: 8px window padding, 6px decoration outer inset at
/// scale 1, 9x19 cells, zero gaps.
fn junction_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 6.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 6.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn press_event() -> MouseEvent {
    MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

fn release_event() -> MouseEvent {
    MouseEvent::new(MouseButton::Left, PressState::Released)
}

/// Exact 2x2 cross: `H[V[1,2], V[3,4]]`.
fn four_pane_grid() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::split(SplitAxis::Vertical, 0.5, leaf(1), leaf(2)),
        LayoutNode::split(SplitAxis::Vertical, 0.5, leaf(3), leaf(4)),
    )
}

/// Issue #2572 acceptance evidence: a diagonal drag from the exact center
/// of `H[V[1,2], V[3,4]]` moves the shared vertical divider and both row
/// segments, resizing all four panes on both axes. The SVGs are the
/// human-readable screenshots; regenerate with
/// `TILING_EVIDENCE_UPDATE=1`, then compare byte-exactly.
#[test]
fn issue_2572_center_junction_cross_drag_before_after() {
    let mut rt = Runtime::with_defaults().expect("runtime");
    let tree = four_pane_grid();
    rt.set_layout(tree.clone());
    let before = rt.layout_allocations();
    assert_eq!(rect_of(&before, 1), UiRect::new(0, 0, 40, 12));
    assert_eq!(rect_of(&before, 2), UiRect::new(0, 12, 40, 12));
    assert_eq!(rect_of(&before, 3), UiRect::new(40, 0, 40, 12));
    assert_eq!(rect_of(&before, 4), UiRect::new(40, 12, 40, 12));

    // The exact layout crossing (40,12) is covered by the pure UI hit
    // test; the headed center gap maps to the adjacent boundary cell.
    rt.handle_cursor_moved(junction_pixels(39, 11));
    rt.handle_mouse_input(press_event());
    assert!(rt.border_drag_active(), "center junction must grab");
    let splits = rt.border_drag_splits().expect("active drag exposes splits");
    assert_eq!(
        splits.len(),
        3,
        "cross must grab the column split and both row segments: {splits:?}"
    );
    assert!(splits.contains(&(Vec::new(), SplitAxis::Horizontal)));
    assert!(splits.contains(&(vec![0], SplitAxis::Vertical)));
    assert!(splits.contains(&(vec![1], SplitAxis::Vertical)));

    // Diagonal pointer motion (+8 columns, +4 rows).
    rt.handle_cursor_moved(junction_pixels(47, 15));
    rt.handle_mouse_input(release_event());
    assert!(!rt.border_drag_active());

    let after = rt.layout_allocations();
    assert_eq!(rect_of(&after, 1), UiRect::new(0, 0, 48, 16));
    assert_eq!(rect_of(&after, 2), UiRect::new(0, 16, 48, 8));
    assert_eq!(rect_of(&after, 3), UiRect::new(48, 0, 32, 16));
    assert_eq!(
        rect_of(&after, 4),
        UiRect::new(48, 16, 32, 8),
        "right row segment must follow the shared horizontal line"
    );
    assert_eq!(rt.layout().validate_structure(), Ok(4));

    let before_svg = render_svg(
        "#2572 center junction: before",
        "H[V[1,2], V[3,4]]; pointer on the four-pane inner junction",
        &tree,
        &before,
        None,
        Some((39, 11, "junction")),
    );
    let after_svg = render_svg(
        "#2572 center junction: after cross drag",
        "diagonal drag +8 cols / +4 rows resizes all four panes",
        rt.layout(),
        &after,
        None,
        Some((47, 15, "pointer")),
    );
    let before_path = check_or_update("tiling-2572-before.svg", &before_svg);
    let after_path = check_or_update("tiling-2572-after.svg", &after_svg);
    eprintln!(
        "evidence: {} {}",
        before_path.display(),
        after_path.display()
    );
}
