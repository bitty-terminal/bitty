//! UX invariant guards and frame/behavioral regression evidence (CTX-0613).
//!
//! Executable checks for the uncovered rows of the candidate UI/UX invariant
//! set (`bitty-terminal-docs/specifications/ui-ux-invariant-set-candidate.md`)
//! and headless evidence artifacts in the form defined by the candidate
//! visual/behavioral evidence record
//! (`bitty-terminal-docs/specifications/visual-regression-evidence-candidate.md`).
//! Issue: `bitty`#1182 (backlog item `UX-41`).
//!
//! # Guard map (test -> invariant -> follow-up)
//!
//! - `ux_inv_9_*` -> `UX-INV-9` (tab projection/identity) -> `F-UX-1`
//! - `ux_inv_10_*` -> `UX-INV-10` (single modal authority) -> `F-UX-2`
//! - `ux_inv_11_*` -> `UX-INV-11` (tier paint order) -> `F-UX-2`
//! - `ux_inv_12_*` -> `UX-INV-12` (committed state before motion) -> `F-UX-3`
//! - `ux_inv_13_*` -> `UX-INV-13` (no content interpolation) -> `F-UX-3`
//! - `ux_inv_16_*` -> `UX-INV-16` (budget fails closed) -> `F-UX-3`
//! - `ux_inv_14_*` -> `UX-INV-14` (chrome cadence) -> `F-UX-4`
//!
//! Out of delegated scope: `UX-INV-15`/`UX-INV-18` (`F-UX-5`, scene path; no
//! scene types exist in this crate yet) and the live-soak supplement (the
//! evidence candidate keeps live evidence supplementary to this headless
//! baseline).
//!
//! # Evidence contract used here
//!
//! - Scopes are declared per artifact (`leaf` / `window`); exact-byte
//!   comparison is the tolerance for text and geometry (no antialiased or
//!   timed content exists headlessly, so no allowance is declared).
//! - Golden artifacts live in `tests/testdata/ux-evidence/` and are compared
//!   byte-exactly. Regenerate with `UX_GUARDS_UPDATE_GOLDENS=1`; a missing
//!   golden fails with the regeneration hint instead of claiming coverage.
//! - Artifact content is synthetic (`ux-41` probe strings, fixed ids); no
//!   clock, cursor blink, or async status participates, so nothing needs
//!   masking and nothing identifies a user.
//! - The `ChromeCadenceModel` in the `ux_inv_14_cadence_*` test is an
//!   executable statement of the `F-UX-4` contract, not product code: it pins
//!   the intended wakeup rule until a product revision source exists.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use bitty_term_state::{State, TerminalAction};
use bitty_ui::{
    CommandError, CommandRegistry, Decoration, Focus, LayoutNode, OverlayError, OverlayKind,
    OverlayLayer, OverlayManager, OverlayTier, PanelId, PresentationMode, Rect, SplitAxis, View,
    ViewId,
};
use bitty_vt::{ControlChar, GraphemeCell};

// ---------------------------------------------------------------------------
// Scenario helpers (deterministic; no clock, no randomness, no platform input)
// ---------------------------------------------------------------------------

fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

fn feed_line(state: &mut State, text: &str) {
    prints(state, text);
    newline(state);
}

/// LF then CR: next line starts at column 0 (LNM off keeps the column).
fn newline(state: &mut State) {
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
}

/// Allocations sorted by leaf identity: the window-scope frame with z-order
/// factored out, so equality means identical geometry per identity.
fn sorted_allocations(node: &LayoutNode, bounds: Rect) -> Vec<(ViewId, Rect)> {
    let mut allocs = node.layout(bounds);
    allocs.sort_by_key(|(id, _)| id.0);
    allocs
}

/// Tier chain of a nested `Overlay` spine, innermost (lowest tier) first.
fn tier_chain(node: &LayoutNode) -> Vec<OverlayTier> {
    match node {
        LayoutNode::Overlay { base, tier, .. } => {
            let mut chain = tier_chain(base);
            chain.push(*tier);
            chain
        }
        _ => Vec::new(),
    }
}

fn all_modes() -> [PresentationMode; 4] {
    use PresentationMode as M;
    [M::Tiled, M::Floating, M::Fullscreen, M::Scratchpad]
}

// ---------------------------------------------------------------------------
// Evidence artifact helpers
// ---------------------------------------------------------------------------

fn artifact_header(scenario: &str, scope: &str, tolerance: &str) -> String {
    format!(
        "# scenario: {scenario}\n# scope: {scope}\n# tolerance: {tolerance}\n\
         # generator: crates/bitty-ui/tests/ux_invariant_guards.rs\n"
    )
}

fn evidence_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/ux-evidence")
        .join(name)
}

fn check_or_update(name: &str, artifact: &str) {
    let path = evidence_path(name);
    if std::env::var("UX_GUARDS_UPDATE_GOLDENS").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create evidence dir");
        }
        std::fs::write(&path, artifact).expect("write golden artifact");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing golden artifact {name}; rerun with UX_GUARDS_UPDATE_GOLDENS=1")
    });
    assert_eq!(artifact, expected, "artifact drift for {name}");
}

// ---------------------------------------------------------------------------
// F-UX-1: UX-INV-9 — a tab is a projection of a panel
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_9_tab_reorder_preserves_identity_and_content() {
    let container = Rect::new(0, 0, 100, 40);
    let mut tabs = LayoutNode::stack(vec![leaf(11), leaf(12), leaf(13)]);
    // External projection record: panel identities behind each tab.
    let panel_of = vec![
        (ViewId::new(11), PanelId::new(101)),
        (ViewId::new(12), PanelId::new(102)),
        (ViewId::new(13), PanelId::new(103)),
    ];
    let ids_before = tabs.leaf_ids();
    assert_eq!(
        ids_before,
        vec![ViewId::new(11), ViewId::new(12), ViewId::new(13)]
    );
    let views_before: Vec<(ViewId, View)> = ids_before
        .iter()
        .map(|id| (*id, tabs.find_leaf(*id).expect("leaf present").clone()))
        .collect();
    let frames_before = sorted_allocations(&tabs, container);

    // Reorder tabs: move the first tab to the back (projection order only).
    if let LayoutNode::Stack(children) = &mut tabs {
        let first = children.remove(0);
        children.push(first);
    } else {
        panic!("tab strip is a Stack");
    }

    // Z-order follows projection order (last is top-most).
    assert_eq!(
        tabs.leaf_ids(),
        vec![ViewId::new(12), ViewId::new(13), ViewId::new(11)]
    );
    // Identity stability: the same identities, no rename, no loss, no gain.
    let mut sorted_before = ids_before.clone();
    sorted_before.sort();
    let mut sorted_after = tabs.leaf_ids();
    sorted_after.sort();
    assert_eq!(
        sorted_before, sorted_after,
        "UX-INV-9: reordering tabs must never rename or reorder an identity"
    );
    // Content stability: each leaf follows its identity.
    for (id, view) in &views_before {
        assert_eq!(
            tabs.find_leaf(*id)
                .expect("projected leaf survives reorder"),
            view,
            "UX-INV-9: leaf content follows its identity across reorder"
        );
    }
    // Projection record still resolves 1:1.
    assert_eq!(
        tabs.leaf_count(),
        panel_of.len(),
        "UX-INV-9: tab-to-panel projection stays 1:1"
    );
    for (view_id, _) in &panel_of {
        assert!(
            tabs.find_leaf(*view_id).is_some(),
            "UX-INV-9: every projected identity survives reorder"
        );
    }
    // Window-scope frame: identical geometry per identity; order never moves
    // a single rect.
    assert_eq!(
        sorted_allocations(&tabs, container),
        frames_before,
        "UX-INV-9: geometry is a function of the set, not the order"
    );
}

// ---------------------------------------------------------------------------
// F-UX-2: UX-INV-10 — exactly one modal authority per Window
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_10_second_modal_fails_and_leaves_first_unchanged() {
    let bounds = Rect::new(0, 0, 80, 24);
    let mut mgr = OverlayManager::new();
    let first = mgr
        .create_overlay(OverlayKind::Modal, bounds, "first modal", None, 1)
        .expect("first modal takes the single authority");
    assert!(mgr.modal_active());
    let snapshot = mgr.overlays().to_vec();

    let err = mgr
        .create_overlay(OverlayKind::Modal, bounds, "second modal", None, 2)
        .expect_err("exactly one modal authority exists per Window");
    assert_eq!(err, OverlayError::OverlayBusy);
    assert_eq!(
        mgr.overlays(),
        snapshot.as_slice(),
        "UX-INV-10: a refused request leaves the first overlay unchanged"
    );
    assert_eq!(mgr.len(), snapshot.len());
    assert!(
        mgr.modal_active(),
        "UX-INV-10: the authority stays with the first modal"
    );

    // Recovery: dismissing the first releases the authority (fail-closed,
    // not fail-stuck).
    assert_eq!(mgr.dismiss(first).map(|o| o.id), Some(first));
    assert!(!mgr.modal_active());
    mgr.create_overlay(OverlayKind::Modal, bounds, "replacement", None, 3)
        .expect("released authority is grantable again");
}

// ---------------------------------------------------------------------------
// F-UX-2: UX-INV-11 — overlay paint order is pure in (tier, construction order)
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_11_tier_paint_order_is_pure_function_of_tier_and_construction_order() {
    let container = Rect::new(0, 0, 100, 40);
    let layer = |tier: OverlayTier, id: u64, x: u16, y: u16| {
        OverlayLayer::new(tier, leaf(id), Rect::new(x, y, 30, 10))
    };
    // Same layers, two different insertion orders.
    let insertion_a = vec![
        layer(OverlayTier::Messages, 4, 40, 20),
        layer(OverlayTier::Float, 2, 10, 5),
        layer(OverlayTier::Popup, 3, 25, 12),
        layer(OverlayTier::Editor, 5, 5, 25),
    ];
    let insertion_b = vec![
        layer(OverlayTier::Editor, 5, 5, 25),
        layer(OverlayTier::Popup, 3, 25, 12),
        layer(OverlayTier::Messages, 4, 40, 20),
        layer(OverlayTier::Float, 2, 10, 5),
    ];
    let tree_a = LayoutNode::overlay_stack(leaf(1), insertion_a);
    let tree_b = LayoutNode::overlay_stack(leaf(1), insertion_b);

    assert_eq!(
        tier_chain(&tree_a),
        vec![
            OverlayTier::Editor,
            OverlayTier::Float,
            OverlayTier::Popup,
            OverlayTier::Messages,
        ]
    );
    assert_eq!(
        tier_chain(&tree_b),
        tier_chain(&tree_a),
        "UX-INV-11: no pass depends on insertion order"
    );
    assert_eq!(
        tree_a.layout(container),
        tree_b.layout(container),
        "UX-INV-11: identical (tier, construction order) paints identically"
    );

    // Same-tier conflict rule is stable across runs: later-constructed
    // paints above (after) the earlier one.
    let same_tier = |order: [u64; 2]| {
        let layers: Vec<OverlayLayer> = order
            .into_iter()
            .map(|id| OverlayLayer::new(OverlayTier::Float, leaf(id), Rect::new(10, 5, 30, 10)))
            .collect();
        LayoutNode::overlay_stack(leaf(1), layers)
    };
    let first = same_tier([7, 8]);
    assert_eq!(
        first.layout(container),
        same_tier([7, 8]).layout(container),
        "UX-INV-11: same-tier order is deterministic across runs"
    );
    let paint: Vec<ViewId> = first
        .layout(container)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let position = |id: u64| {
        paint
            .iter()
            .position(|v| *v == ViewId::new(id))
            .expect("painted")
    };
    assert!(
        position(7) < position(8),
        "UX-INV-11: later-constructed same-tier overlay paints above"
    );
}

// ---------------------------------------------------------------------------
// F-UX-3: UX-INV-12 — motion never delays the committed state
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_12_committed_state_identical_across_animation_modes() {
    let container = Rect::new(0, 0, 100, 40);
    let mut state = State::new();
    feed_line(&mut state, "committed content");
    assert!(state.check_invariants().is_ok());

    let mut committed = LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(21), leaf(22));
    committed.reflow(container);
    let committed_allocs = committed.layout(container);
    let committed_chrome = committed.layout_with_decoration(container, Decoration::default());
    let committed_text: Vec<Vec<String>> = [21_u64, 22_u64]
        .iter()
        .map(|id| {
            committed
                .find_leaf(ViewId::new(*id))
                .expect("leaf present")
                .visible_text_rows(&state)
        })
        .collect();

    // Disabled (Tiled), completed, and every gated end-mode show the same
    // committed layout, content, and chrome: the state is final before any
    // animation starts.
    for mode in all_modes() {
        let mut variant = committed.clone();
        for id in [21_u64, 22_u64] {
            variant
                .find_leaf_mut(ViewId::new(id))
                .expect("leaf present")
                .set_presentation(mode);
        }
        assert_eq!(
            variant.layout(container),
            committed_allocs,
            "UX-INV-12: mode {mode:?} must not move committed layout"
        );
        for (index, id) in [21_u64, 22_u64].iter().enumerate() {
            assert_eq!(
                variant
                    .find_leaf(ViewId::new(*id))
                    .expect("leaf present")
                    .visible_text_rows(&state),
                committed_text[index],
                "UX-INV-12: mode {mode:?} must not move committed content"
            );
        }
        assert_eq!(
            variant.layout_with_decoration(container, Decoration::default()),
            committed_chrome,
            "UX-INV-12: mode {mode:?} must not move committed chrome"
        );
    }

    // Interrupted animation: stamped mid-flight, then reverted — the
    // committed state never flickers in between.
    let mut interrupted = committed.clone();
    for id in [21_u64, 22_u64] {
        interrupted
            .find_leaf_mut(ViewId::new(id))
            .expect("leaf present")
            .set_presentation(PresentationMode::Floating);
    }
    assert_eq!(
        interrupted.layout(container),
        committed_allocs,
        "UX-INV-12: interrupted animation shows the committed state"
    );
    for id in [21_u64, 22_u64] {
        interrupted
            .find_leaf_mut(ViewId::new(id))
            .expect("leaf present")
            .set_presentation(PresentationMode::Tiled);
    }
    assert_eq!(
        interrupted.layout(container),
        committed_allocs,
        "UX-INV-12: reverted animation shows the committed state"
    );
}

// ---------------------------------------------------------------------------
// F-UX-3: UX-INV-13 — motion never interpolates terminal content
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_13_terminal_content_never_interpolated() {
    let mut state = State::new();
    state.resize(40, 10);
    feed_line(&mut state, "motion probe A");
    prints(&mut state, "wide ");
    prints(&mut state, "あ");
    newline(&mut state);
    feed_line(&mut state, "motion probe B");
    assert!(state.check_invariants().is_ok());

    let view = View::new(ViewId::new(31), 40, 10);
    let reference = view.visible_cells(&state);
    assert!(!reference.is_empty());

    // Terminal bytes are a pure function of (state, viewport geometry):
    // stamping any presentation mode changes nothing.
    for mode in all_modes() {
        let mut variant = view.clone();
        variant.set_presentation(mode);
        assert_eq!(
            variant.visible_cells(&state),
            reference,
            "UX-INV-13: mode {mode:?} must not alter terminal bytes"
        );
        assert_eq!(
            variant.allocation(),
            view.allocation(),
            "UX-INV-13: mode {mode:?} must not alter the viewport"
        );
    }

    // Rebuilding the same frame twice is byte-identical (no jitter source).
    assert_eq!(
        view.visible_cells(&state),
        reference,
        "UX-INV-13: repeated frames are byte-identical"
    );

    // Scroll offset is viewport geometry, not motion: exact after scrolling.
    let mut scrolled = view.clone();
    scrolled.scroll_by(1, state.scrollback_len());
    let scrolled_cells = scrolled.visible_cells(&state);
    let mut scrolled_mode = scrolled.clone();
    scrolled_mode.set_presentation(PresentationMode::Floating);
    assert_eq!(
        scrolled_mode.visible_cells(&state),
        scrolled_cells,
        "UX-INV-13: scrolled content is exact under any mode"
    );
}

// ---------------------------------------------------------------------------
// F-UX-3: UX-INV-16 — budget overflow fails closed
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_16_budget_overflow_fails_closed_with_prior_state_intact() {
    // Overlay budget: 4 non-modal + 1 modal.
    let bounds = Rect::new(0, 0, 80, 24);
    let mut mgr = OverlayManager::new();
    for i in 0..4 {
        mgr.create_overlay(OverlayKind::NonModal, bounds, format!("note {i}"), None, 1)
            .expect("within budget");
    }
    mgr.create_overlay(OverlayKind::Modal, bounds, "modal", None, 1)
        .expect("4+1 envelope");
    let overlays_before = mgr.overlays().to_vec();
    let err = mgr
        .create_overlay(OverlayKind::NonModal, bounds, "overflow", None, 1)
        .expect_err("budget overflow must refuse, never substitute");
    assert!(
        matches!(err, OverlayError::TooManyOverlays { .. }),
        "UX-INV-16: refusal is reported, never silent"
    );
    assert_eq!(
        mgr.overlays(),
        overlays_before.as_slice(),
        "UX-INV-16: prior overlay state intact after refusal"
    );
    assert_eq!(mgr.len(), 5);

    // Command budget: 32 per panel.
    let mut reg = CommandRegistry::new();
    let panel = PanelId::new(41);
    for i in 0..32 {
        reg.register(panel, &format!("xuepoo.test:cmd{i:02}"))
            .expect("within budget");
    }
    let cmds_before = reg.commands_for(panel);
    let err = reg
        .register(panel, "xuepoo.test:overflow")
        .expect_err("command budget overflow must refuse");
    assert!(
        matches!(err, CommandError::TooManyCommands { .. }),
        "UX-INV-16: refusal is reported, never silent"
    );
    assert_eq!(
        reg.commands_for(panel),
        cmds_before,
        "UX-INV-16: no partial application on refusal"
    );
    assert_eq!(reg.len(), 32);

    // Decoration budget: out-of-range values fail validation closed.
    let bad = Decoration::new(33, 6, 2, 6, 6);
    assert!(
        bad.validate().is_err(),
        "UX-INV-16: gaps_in above MAX_GAP_PX must refuse"
    );
}

// ---------------------------------------------------------------------------
// F-UX-4: UX-INV-14 — chrome cadence (no per-keystroke recomputation)
// ---------------------------------------------------------------------------

#[test]
fn ux_inv_14_chrome_ignores_keystroke_stream() {
    let container = Rect::new(0, 0, 120, 40);
    let tree = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::stack(vec![leaf(51), leaf(52)]),
        leaf(53),
    );
    let decoration = Decoration::default();
    let frames_before = tree.layout_with_decoration(container, decoration);

    // ZERO decoration reproduces the undecorated solver exactly: chrome is a
    // pure function of (tree, bounds, decoration).
    let undecorated = tree.layout(container);
    let zero_frames = tree.layout_with_decoration(container, Decoration::ZERO);
    assert_eq!(zero_frames.len(), undecorated.len());
    for ((id, decorated), (plain_id, plain_rect)) in zero_frames.iter().zip(undecorated.iter()) {
        assert_eq!(id, plain_id);
        assert_eq!(
            decorated.frame, *plain_rect,
            "UX-INV-14: ZERO decoration is the undecorated fast path"
        );
    }

    // Pump a keystroke/PTY burst through terminal state. Chrome inputs
    // exclude that stream, so the frames must be bit-identical.
    let mut state = State::new();
    for i in 0..200 {
        prints(&mut state, &format!("keystroke {i} "));
        if i % 10 == 0 {
            state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
        }
    }
    prints(&mut state, "あいうえお");
    assert!(state.check_invariants().is_ok());
    assert_eq!(
        tree.layout_with_decoration(container, decoration),
        frames_before,
        "UX-INV-14: keystrokes and PTY reads are not chrome wakeup sources"
    );
}

/// Executable statement of the `F-UX-4` cadence contract: a chrome segment
/// recomputes only on a revision bump or while an animation is active.
/// Test-local contract model, not product code — it pins the intended wakeup
/// rule until a product revision source exists.
#[derive(Debug)]
struct ChromeCadenceModel {
    revision: u64,
    animation_active: bool,
    recomputes: u64,
    computed_revision: Option<u64>,
}

impl ChromeCadenceModel {
    fn new() -> Self {
        Self {
            revision: 0,
            animation_active: false,
            recomputes: 0,
            computed_revision: None,
        }
    }

    /// Keystroke wakeup: must never schedule a recompute.
    fn note_keystroke(&mut self) {}

    /// PTY-read wakeup: must never schedule a recompute.
    fn note_pty_read(&mut self) {}

    /// State revision (tree/bounds/decoration change): the only content
    /// wakeup besides an active animation.
    fn note_revision(&mut self) {
        self.revision += 1;
    }

    fn set_animation(&mut self, active: bool) {
        self.animation_active = active;
    }

    /// Runs one frame; returns true when it recomputed.
    fn frame(&mut self) -> bool {
        let stale = self.computed_revision != Some(self.revision);
        if stale || self.animation_active {
            self.computed_revision = Some(self.revision);
            self.recomputes += 1;
            true
        } else {
            false
        }
    }
}

#[test]
fn ux_inv_14_cadence_contract_only_revision_or_animation_recomputes() {
    let mut chrome = ChromeCadenceModel::new();
    assert!(chrome.frame(), "first frame computes");
    for _ in 0..100 {
        chrome.note_keystroke();
        assert!(
            !chrome.frame(),
            "UX-INV-14: no per-keystroke segment recomputation"
        );
    }
    for _ in 0..50 {
        chrome.note_pty_read();
        assert!(
            !chrome.frame(),
            "UX-INV-14: no per-PTY-read segment recomputation"
        );
    }
    assert_eq!(chrome.recomputes, 1, "keystroke/PTY burst adds no work");
    chrome.note_revision();
    assert!(chrome.frame(), "a revision is a wakeup source");
    assert!(
        !chrome.frame(),
        "a revision coalesces to exactly one recompute"
    );
    chrome.set_animation(true);
    assert!(chrome.frame(), "active animation recomputes");
    assert!(chrome.frame(), "active animation recomputes every frame");
    chrome.set_animation(false);
    assert!(
        !chrome.frame(),
        "UX-INV-14: idle chrome with no revision reuses the cached frame"
    );
}

// ---------------------------------------------------------------------------
// Evidence: leaf-scope frame artifact (exact text)
// ---------------------------------------------------------------------------

#[test]
fn evidence_leaf_frame_exact_and_reproducible() {
    let build = || {
        let mut state = State::new();
        state.resize(40, 10);
        feed_line(&mut state, "ux-41 leaf scenario");
        feed_line(&mut state, "tab-A identity stable");
        prints(&mut state, "wide ");
        prints(&mut state, "あ");
        newline(&mut state);
        feed_line(&mut state, "0123456789");
        assert!(state.check_invariants().is_ok());
        state
    };
    let render = |state: &State| -> String {
        let view = View::new(ViewId::new(7), 40, 10);
        let mut out = artifact_header("leaf-text-exact", "leaf", "exact");
        out.push_str(&format!(
            "view {} origin {:?} size {}x{}\n",
            view.id(),
            view.origin(),
            view.cols(),
            view.rows()
        ));
        for (index, row) in view.visible_text_rows(state).iter().enumerate() {
            out.push_str(&format!("row {index:02}: '{row}'\n"));
        }
        out
    };
    let first = render(&build());
    let second = render(&build());
    assert_eq!(
        first, second,
        "evidence rule 1: two runs on one revision produce comparable artifacts"
    );
    check_or_update("frame-leaf-exact.txt", &first);
}

// ---------------------------------------------------------------------------
// Evidence: window-scope frame + behavioral log artifacts
// ---------------------------------------------------------------------------

#[test]
fn evidence_window_frame_and_behavioral_log() {
    let container = Rect::new(0, 0, 100, 40);
    let tree = || {
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::stack(vec![leaf(61), leaf(62)]),
            leaf(63),
        )
    };

    // Window-scope frame artifact: allocations sorted by identity (exact).
    let render_frame = || {
        let mut out = artifact_header("window-tiling-exact", "window", "exact");
        out.push_str("container Rect { x: 0, y: 0, width: 100, height: 40 }\n");
        for (id, rect) in sorted_allocations(&tree(), container) {
            out.push_str(&format!("{id} {rect:?}\n"));
        }
        out
    };
    let frame = render_frame();
    assert_eq!(
        frame,
        render_frame(),
        "evidence rule 1: frame evidence is reproducible"
    );
    check_or_update("frame-window-exact.txt", &frame);

    // Behavioral artifact: ordered observable events for the scenario.
    let run_scenario = || {
        let scenario_tree = tree();
        let mut events: Vec<String> = Vec::new();
        events.push(format!(
            "layout committed leaves={}",
            scenario_tree.leaf_count()
        ));
        let mut focus = Focus::new();
        let first = focus.focus_first(&scenario_tree);
        events.push(format!("focus first={first:?}"));
        focus.set(first.expect("non-empty tree has a first leaf"));
        events.push(format!("focus next={:?}", focus.next(&scenario_tree)));
        let mut mgr = OverlayManager::new();
        let overlay_bounds = Rect::new(10, 5, 40, 12);
        let created = mgr.create_overlay(OverlayKind::Modal, overlay_bounds, "confirm", None, 1);
        events.push(format!("overlay created={created:?}"));
        let refused = mgr.create_overlay(OverlayKind::Modal, overlay_bounds, "second", None, 2);
        events.push(format!("overlay refused={refused:?}"));
        for _ in 0..4 {
            mgr.create_overlay(OverlayKind::NonModal, overlay_bounds, "note", None, 1)
                .expect("within budget");
        }
        let budget = mgr.create_overlay(OverlayKind::NonModal, overlay_bounds, "overflow", None, 1);
        events.push(format!("budget refused={budget:?}"));
        events
    };
    let events = run_scenario();
    assert_eq!(
        events,
        run_scenario(),
        "evidence rule 1: behavioral evidence is reproducible"
    );
    let mut log = artifact_header("modal-focus-order", "window", "exact");
    for (index, event) in events.iter().enumerate() {
        log.push_str(&format!("event {index:02}: {event}\n"));
    }
    check_or_update("behavior-modal-focus-order.txt", &log);
}
