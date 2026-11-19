//! Panel GUI content presentation **modes**: the second bounded Core slice of
//! bitty#2388 (CTX-1902).
//!
//! The first slice (CTX-1895) swapped a panel's content kind for a GUI layer and
//! kept the displaced terminal alive and frozen. This slice gives the presenting
//! panel a *place*: the layer may sit in the panel's own slot
//! ([`PresentationMode::Tiled`]) or float over the workspace
//! ([`PresentationMode::Floating`]), and release returns the panel to the mode
//! the layer displaced.
//!
//! Contract rows proven here:
//!
//! 1. **A presented panel can move to a float, and its hidden PTY cannot tell.**
//!    The mode change re-places the leaf's frame (different origin, size, and
//!    `Float` paint tier) while the content kind, the token, the scene slot, and
//!    the displaced terminal's `TIOCGWINSZ` stay exactly as they were. The
//!    freeze is a predicate over the layer record, so the layout pass that
//!    follows the change still skips every `LogicalRect -> PTY` step.
//! 2. **Release gives the mode back, and the geometry restore measures the
//!    returned placement.** The panel comes back to the mode recorded at present
//!    time, and because the mode is restored *before* the restore resize is
//!    measured, a layer that floated and came back to its own slot owes no
//!    resize at all (`content_restore_resizes` stays 0 and the winsize is
//!    untouched).
//! 3. **A foreign token cannot move another panel.** The token is the authority;
//!    a layer's handle used on a different panel's layer is refused typed and
//!    nothing moves.
//! 4. **A mode this runtime cannot give a presented panel is refused.** Stamp-
//!    only modes paint exactly like the mode they replaced (`Fullscreen`), or
//!    need a store that owns the hide/slot behavior (`Scratchpad`), so they are
//!    refused with `ContentPresentationUnsupported` before any state moves:
//!    leaf mode, frame, focus, winsize, and record are all unchanged.
//! 5. **No layer, no mode change.** A panel that presents no layer refuses with
//!    `ContentNotPresented`, and a view with no panel refuses with
//!    `ContentUnavailable`.
//! 6. **A redundant request is a true no-op, and release never undoes somebody
//!    else's placement.** Asking for the mode the leaf already has writes
//!    nothing (so it cannot claim an out-of-band change as the layer's own), and
//!    release only returns the mode the layer itself moved.
//! 7. **The prior mode is the one the layer displaced.** A layer presented on an
//!    already-floating panel records `Floating` as prior and gives it back.
//!
//! Every assertion reads observable state — the presented frame report, the
//! leaf's mode through `Runtime::view_presentation`, the PTY's own winsize — and
//! never a value the production code derived from the same container it asserted
//! against.
//!
//! # Verification limits
//!
//! Rows whose observable is a live pane's kernel winsize need a real PTY (a
//! pane-backed panel owns a `Pty`, so no PTY-less build can construct one). Rows
//! 1, 2, 3, 4, and 5 call `bitty_test_support::require_pty!()` as their **first
//! statement**, so they skip on such a build; the gate lives in the row and
//! never in a helper, because `require_pty!()` expands to a plain `return` that
//! inside a helper would only leave the helper. Rows 6 and 7 need no PTY (they
//! use the runtime-global primary owner) and therefore always run.

#![cfg(unix)]

use bitty_rich::scene::{
    BlockAnchor, BlockId, RichBlock, Scene, SceneNode, ScrollBehavior, StyledSpan,
};
use bitty_runtime::registry::{
    PanelError, PanelRegistryConfig, PanelRuntime, PanelType, WorkspaceId,
};
use bitty_runtime::{
    LayoutNode, OverlayTier, PresentationMode, Runtime, SplitAxis, UiRect, View, ViewId,
};
use bitty_ui::apply_presentation_command;
use bitty_ui::panel::ViewContent;

/// Primary leaf: paints the runtime-global primary grid (no pane session).
const PRIMARY: ViewId = ViewId::new(1);
/// Pane leaf: owns its own shell session.
const PANE: ViewId = ViewId::new(2);
/// Leaf with no panel mounted at all.
const BARE: ViewId = ViewId::new(3);

/// The window the rows present into, in cells.
const CONTAINER_COLS: usize = 80;
/// The window the rows present into, in cells.
const CONTAINER_ROWS: usize = 24;

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("headless defaults must build")
}

fn single_leaf() -> LayoutNode {
    LayoutNode::leaf(View::new(PRIMARY, CONTAINER_COLS, CONTAINER_ROWS))
}

fn two_panes() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, CONTAINER_COLS, CONTAINER_ROWS)),
        LayoutNode::leaf(View::new(PANE, CONTAINER_COLS, CONTAINER_ROWS)),
    )
}

/// `PRIMARY | (PANE / BARE)`: two panels and one leaf that hosts none.
fn panes_and_a_bare_leaf() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, CONTAINER_COLS, CONTAINER_ROWS)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(PANE, CONTAINER_COLS, CONTAINER_ROWS)),
            LayoutNode::leaf(View::new(BARE, CONTAINER_COLS, CONTAINER_ROWS)),
        ),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(
        0,
        0,
        u16::try_from(CONTAINER_COLS).expect("cols fit a cell rect"),
        u16::try_from(CONTAINER_ROWS).expect("rows fit a cell rect"),
    ));
}

/// A host with one `Terminal` panel mounted at each of `views`, installed on
/// `rt`.
fn install_panels_at(rt: &mut Runtime, views: &[ViewId]) {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host");
    for view in views {
        let handle = host
            .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
            .expect("terminal panel");
        host.mount_panel(handle.id, handle.generation, *view)
            .expect("panel mounts at the leaf");
    }
    rt.install_scene_host(host);
}

fn text_layer(text: &str) -> Scene {
    let mut scene = Scene::new();
    scene
        .insert(
            RichBlock::new(
                BlockId(1),
                BlockAnchor::Zone(1),
                SceneNode::Text(StyledSpan {
                    text: text.to_string(),
                    bold: false,
                    italic: false,
                }),
                ScrollBehavior::Inline,
                1,
                1,
                1,
            )
            .expect("block fits SCN-1..3"),
        )
        .expect("scene admits the block");
    scene
}

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Spawns `PANE`'s shell. Callers must gate themselves with
/// `bitty_test_support::require_pty!()` **first**: the macro expands to a plain
/// `return`, so a gate inside a helper would leave the helper and let the caller
/// continue against a pane that was never spawned.
fn spawn_pane(rt: &mut Runtime) {
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell spawns");
}

/// Moves the leaf out of band, the way a user gesture or another Core path does.
fn stamp_out_of_band(rt: &mut Runtime, view: ViewId, mode: PresentationMode) {
    let command = bitty_ui::presentation_command_for_mode(mode);
    let mut tree = rt.layout_mut();
    apply_presentation_command(&mut tree, command, view).expect("out-of-band stamp applies");
}

// ---------------------------------------------------------------------------
// Row 1: a presented panel moves to a float, the hidden PTY never notices
// ---------------------------------------------------------------------------

#[test]
fn a_presented_panel_moves_to_a_bounded_float_without_resizing_its_hidden_pty() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PANE]);
    assert!(rt.set_focus(PANE), "the pane holds focus");

    let presented = rt
        .present_panel_content(PANE, text_layer("dashboard"))
        .expect("present is admitted on a terminal-backed panel");
    assert_eq!(presented.presentation, PresentationMode::Tiled);
    assert_eq!(
        presented.prior_presentation,
        PresentationMode::Tiled,
        "the layer records the mode it displaced"
    );

    let winsize = rt.pane_pty_size(&PANE).expect("live pane winsize");
    let tiled = frame_of(&rt, PANE);
    assert_eq!(tiled.tier, None, "a tiled panel is base content");

    let changed = rt
        .set_panel_content_presentation(PANE, presented.token, PresentationMode::Floating)
        .expect("a presented panel may float");
    assert_eq!(changed.view, PANE);
    assert_eq!(changed.panel, presented.panel);
    assert_eq!(
        changed.token, presented.token,
        "the handle survives the move"
    );
    assert_eq!(
        changed.presented, presented.presented,
        "the content kind is unchanged"
    );
    assert_eq!(changed.displaced, presented.displaced);
    assert_eq!(changed.presentation, PresentationMode::Floating);
    assert_eq!(
        changed.prior_presentation,
        PresentationMode::Tiled,
        "a mode change never rewrites the mode release will restore"
    );
    assert_eq!(
        rt.view_presentation(PANE),
        Some(PresentationMode::Floating),
        "the leaf itself carries the new mode"
    );

    // The placement really moved: a different frame, a different content rect,
    // and the Float paint tier.
    let floated = frame_of(&rt, PANE);
    assert_ne!(floated.frame, tiled.frame, "the float is a different frame");
    assert_ne!(
        floated.content, tiled.content,
        "the content rect moved with it"
    );
    assert_eq!(
        floated.tier,
        Some(OverlayTier::Float),
        "a mode-floating leaf paints at the Float tier"
    );
    // Bounded: ~4/5 of the window per axis is still less than the window.
    assert!(
        usize::from(floated.cols) > usize::from(tiled.cols)
            && usize::from(floated.cols) < CONTAINER_COLS,
        "the float is wider than one half-slot and narrower than the window: {} vs {}",
        floated.cols,
        tiled.cols
    );
    assert!(
        usize::from(floated.rows) < usize::from(tiled.rows)
            && usize::from(floated.rows) < CONTAINER_ROWS,
        "the float is shorter than a full-height slot: {} vs {}",
        floated.rows,
        tiled.rows
    );

    // The hidden terminal is untouched: same winsize now, and still frozen
    // across the layout pass the change triggers.
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "moving the layer must not resize the hidden PTY"
    );
    assert_eq!(
        rt.panel_content_kind(PANE),
        Some(ViewContent::Rich(presented.token.get())),
        "the panel still presents the layer, not its terminal"
    );

    let stats = rt.tick().expect("frame in the new mode");
    assert!(stats.glyphs > 0, "the layer paints in its new placement");
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "the layout pass after the change still refuses to touch the frozen PTY"
    );
    assert!(rt.content_layer_presented(PANE));
}

// ---------------------------------------------------------------------------
// Row 2: release returns the panel to its own mode, and owes no resize
// ---------------------------------------------------------------------------

#[test]
fn release_returns_the_panel_to_the_mode_the_layer_displaced() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PANE]);
    let presented = rt
        .present_panel_content(PANE, text_layer("dashboard"))
        .expect("present is admitted");
    let winsize = rt.pane_pty_size(&PANE).expect("live pane winsize");
    let tiled = frame_of(&rt, PANE);

    rt.set_panel_content_presentation(PANE, presented.token, PresentationMode::Floating)
        .expect("a presented panel may float");
    assert_ne!(frame_of(&rt, PANE).frame, tiled.frame);

    let restored = rt
        .release_panel_content(PANE, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(restored.content, ViewContent::Terminal(PANE.0));
    assert_eq!(restored.presentation, PresentationMode::Floating);
    assert_eq!(restored.prior_presentation, PresentationMode::Tiled);
    assert_eq!(
        rt.view_presentation(PANE),
        Some(PresentationMode::Tiled),
        "release gives the mode back"
    );

    let back = frame_of(&rt, PANE);
    assert_eq!(back.frame, tiled.frame, "the panel is back in its own slot");
    assert_eq!(back.tier, None);
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "the restored slot is the frozen rect, so the winsize is already right"
    );
    assert_eq!(
        rt.content_restore_resizes(),
        0,
        "the mode is restored before the resize is measured, so no restore resize is owed"
    );
}

// ---------------------------------------------------------------------------
// Row 3: the token is the authority
// ---------------------------------------------------------------------------

#[test]
fn a_foreign_token_cannot_move_another_panels_layer() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PRIMARY, PANE]);
    let on_primary = rt
        .present_panel_content(PRIMARY, text_layer("primary"))
        .expect("primary presents");
    let on_pane = rt
        .present_panel_content(PANE, text_layer("pane"))
        .expect("pane presents");

    match rt.set_panel_content_presentation(PANE, on_primary.token, PresentationMode::Floating) {
        Err(PanelError::ContentTokenMismatch { panel_id, token }) => {
            assert_eq!(
                panel_id, on_pane.panel,
                "the refusal names the panel it found"
            );
            assert_eq!(token, on_primary.token.get(), "and the handle it refused");
        }
        other => panic!("a foreign handle must be refused, got {other:?}"),
    }
    assert_eq!(
        rt.view_presentation(PANE),
        Some(PresentationMode::Tiled),
        "the refused panel did not move"
    );
    assert_eq!(
        rt.panel_content_layer(PANE).map(|layer| layer.presentation),
        Some(PresentationMode::Tiled),
        "and its record did not move either"
    );
    assert!(
        rt.panel_content_layer(PRIMARY).is_some(),
        "the other layer is live"
    );
}

// ---------------------------------------------------------------------------
// Row 4: a mode this runtime cannot honor is refused, nothing moves
// ---------------------------------------------------------------------------

#[test]
fn a_mode_with_no_in_tree_placement_is_refused_before_anything_moves() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PANE]);
    assert!(rt.set_focus(PANE), "the pane holds focus");
    let presented = rt
        .present_panel_content(PANE, text_layer("dashboard"))
        .expect("present is admitted");
    let winsize = rt.pane_pty_size(&PANE).expect("live pane winsize");
    let tiled = frame_of(&rt, PANE);

    for mode in [PresentationMode::Fullscreen, PresentationMode::Scratchpad] {
        match rt.set_panel_content_presentation(PANE, presented.token, mode) {
            Err(PanelError::ContentPresentationUnsupported {
                view_id,
                mode: refused,
            }) => {
                assert_eq!(view_id, PANE);
                assert_eq!(refused, mode.as_str(), "the refusal names the mode");
            }
            other => panic!("{mode:?} must be refused, got {other:?}"),
        }
        assert_eq!(
            rt.view_presentation(PANE),
            Some(PresentationMode::Tiled),
            "{mode:?} must not stamp the leaf"
        );
        assert_eq!(
            frame_of(&rt, PANE).frame,
            tiled.frame,
            "{mode:?} must not re-place the leaf"
        );
        assert_eq!(
            rt.pane_pty_size(&PANE),
            Some(winsize),
            "{mode:?} must not resize the PTY"
        );
        assert_eq!(
            rt.focused_view(),
            Some(PANE),
            "{mode:?} must not move focus"
        );
        assert_eq!(
            rt.panel_content_layer(PANE).map(|layer| layer.presentation),
            Some(PresentationMode::Tiled),
            "{mode:?} must not touch the record"
        );
    }
}

// ---------------------------------------------------------------------------
// Row 5: no layer (and no panel) means no mode change
// ---------------------------------------------------------------------------

#[test]
fn a_panel_with_no_presented_layer_cannot_be_switched() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, panes_and_a_bare_leaf());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PRIMARY, PANE]);
    // The pane's panel presents nothing; use another live layer's handle so the
    // refusal is about the absent layer and not about a foreign handle.
    let on_primary = rt
        .present_panel_content(PRIMARY, text_layer("primary"))
        .expect("primary presents");

    match rt.set_panel_content_presentation(PANE, on_primary.token, PresentationMode::Floating) {
        Err(PanelError::ContentNotPresented { panel_id }) => {
            assert_ne!(
                panel_id, on_primary.panel,
                "the refusal names the panel it found, never the panel that owns the handle"
            );
        }
        other => panic!("a layer-less panel must be refused, got {other:?}"),
    }
    assert_eq!(rt.view_presentation(PANE), Some(PresentationMode::Tiled));

    match rt.set_panel_content_presentation(BARE, on_primary.token, PresentationMode::Floating) {
        Err(PanelError::ContentUnavailable { view_id, reason }) => {
            assert_eq!(view_id, BARE);
            assert_eq!(reason, "no panel is mounted at this view");
        }
        other => panic!("a view with no panel must be refused, got {other:?}"),
    }
    assert_eq!(rt.view_presentation(BARE), Some(PresentationMode::Tiled));
}

// ---------------------------------------------------------------------------
// Row 6: a redundant request writes nothing; release never undoes an outside move
// ---------------------------------------------------------------------------

#[test]
fn a_redundant_request_is_a_no_op_and_release_leaves_an_outside_mode_alone() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panels_at(&mut rt, &[PRIMARY]);
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("layer"))
        .expect("primary presents");

    // Asking for the mode the leaf already has: admitted, and nothing recorded.
    let same = rt
        .set_panel_content_presentation(PRIMARY, presented.token, PresentationMode::Tiled)
        .expect("a redundant request is admitted as a no-op");
    assert_eq!(same.presentation, PresentationMode::Tiled);

    // Somebody else floats the panel while the layer is up.
    stamp_out_of_band(&mut rt, PRIMARY, PresentationMode::Floating);
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Floating)
    );

    // The plugin asks for float, which is already true: still a no-op, so the
    // layer does not claim the outside change as its own.
    let same = rt
        .set_panel_content_presentation(PRIMARY, presented.token, PresentationMode::Floating)
        .expect("a redundant request is admitted as a no-op");
    assert_eq!(
        same.presentation,
        PresentationMode::Tiled,
        "a redundant request must not record a mode it did not stamp"
    );

    let restored = rt
        .release_panel_content(PRIMARY, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(restored.content, ViewContent::Terminal(PRIMARY.0));
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Floating),
        "release restores content, never somebody else's placement decision"
    );
}

// ---------------------------------------------------------------------------
// Row 7: the prior mode is the mode the layer actually displaced
// ---------------------------------------------------------------------------

#[test]
fn a_layer_presented_on_a_floating_panel_gives_floating_back() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panels_at(&mut rt, &[PRIMARY]);
    stamp_out_of_band(&mut rt, PRIMARY, PresentationMode::Floating);
    let floated = frame_of(&rt, PRIMARY);

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("layer"))
        .expect("a floating terminal-backed panel may present");
    assert_eq!(presented.presentation, PresentationMode::Floating);
    assert_eq!(
        presented.presentation,
        rt.view_presentation(PRIMARY).expect("leaf mode"),
        "the record and the leaf agree at present time"
    );
    assert_eq!(
        presented.prior_presentation,
        PresentationMode::Floating,
        "prior is read from the leaf, not assumed to be tiled"
    );

    let tiled = rt
        .set_panel_content_presentation(PRIMARY, presented.token, PresentationMode::Tiled)
        .expect("the layer may return the panel to its slot");
    assert_eq!(tiled.presentation, PresentationMode::Tiled);
    assert_eq!(
        tiled.prior_presentation,
        PresentationMode::Floating,
        "the mode the layer displaced never moves"
    );
    assert_ne!(frame_of(&rt, PRIMARY).frame, floated.frame);

    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Floating),
        "release returns the panel to the mode it presented from"
    );
    assert_eq!(
        frame_of(&rt, PRIMARY).frame,
        floated.frame,
        "and to the exact frame that mode gives it"
    );
}
