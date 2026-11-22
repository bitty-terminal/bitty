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
//! 8. **Release never overwrites a mode a third party stamped after the layer
//!    moved the panel.** Present `Tiled`, float through the verb (so the record's
//!    mode and the prior mode differ), stamp `Fullscreen` out of band, release:
//!    the leaf stays `Fullscreen` and only the content comes back. This is the
//!    second restore guard on its own — row 6 cannot reach it, because a layer
//!    that never moved the mode is answered by the first guard.
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
// Row 4: a mode nobody on this runtime owns is refused, nothing moves
// ---------------------------------------------------------------------------

#[test]
fn scratchpad_is_refused_and_unowned_fullscreen_keeps_the_typed_refusal() {
    // CTX-1936 premise correction: `Fullscreen` is no longer placement-less —
    // it is answerable through the installed app-level zoom owner, and this
    // fixture installs none (the posture of every runtime before the
    // composition root wires one, and of `--safe`/plugin-less runs). The row
    // keeps proving the fail-closed half: `Fullscreen` without an owner and
    // `Scratchpad` with or without one are refused with
    // `ContentPresentationUnsupported` before any state moves — leaf mode,
    // frame, focus, winsize, and record all unchanged.
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

    // The other half of the corrected premise: with an owner installed,
    // `Fullscreen` is answerable — but `Scratchpad` keeps its refusal, because
    // no scratchpad store owns the hide/slot behavior the mode names.
    let zoom = install_zoom_owner(&mut rt);
    let refused = rt
        .set_panel_content_presentation(PANE, presented.token, PresentationMode::Scratchpad)
        .expect_err("scratchpad stays refused even with a zoom owner installed");
    assert!(matches!(
        refused,
        PanelError::ContentPresentationUnsupported { .. }
    ));
    assert_eq!(
        zoom.state.borrow().engages,
        0,
        "the refused scratchpad never reached the zoom owner"
    );
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

// ---------------------------------------------------------------------------
// Row 8: the second restore guard, on its own — a third party moves the leaf
//        after the layer did, and release leaves that intent alone
// ---------------------------------------------------------------------------

#[test]
fn release_leaves_a_third_party_mode_alone_after_the_layer_floated() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panels_at(&mut rt, &[PRIMARY]);
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("layer"))
        .expect("primary presents");
    assert_eq!(presented.presentation, PresentationMode::Tiled);

    // The layer moves the panel itself, so the record's mode (`Floating`) and
    // the mode release would give back (`Tiled`) differ. Row 6 cannot reach the
    // second restore guard because the first one answers it: here the first one
    // no longer answers, and only the second can refuse the restore.
    let floated = rt
        .set_panel_content_presentation(PRIMARY, presented.token, PresentationMode::Floating)
        .expect("a presented panel may float");
    assert_eq!(floated.presentation, PresentationMode::Floating);
    assert_eq!(floated.prior_presentation, PresentationMode::Tiled);
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Floating)
    );

    // Third party, out of band, after the layer's own move: the leaf carries
    // somebody else's intent now, and that outranks the layer's.
    stamp_out_of_band(&mut rt, PRIMARY, PresentationMode::Fullscreen);
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Fullscreen)
    );

    let restored = rt
        .release_panel_content(PRIMARY, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(
        restored.presentation,
        PresentationMode::Floating,
        "the record still names the mode the layer last requested"
    );
    assert_eq!(
        restored.prior_presentation,
        PresentationMode::Tiled,
        "and the mode the layer displaced"
    );
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Fullscreen),
        "release restores content, never the mode a third party stamped after the layer moved the panel"
    );
    assert_eq!(
        rt.panel_content_kind(PRIMARY),
        Some(ViewContent::Terminal(PRIMARY.0)),
        "the content is restored either way"
    );
    assert!(
        !rt.content_layer_presented(PRIMARY),
        "and the layer is gone"
    );
}

// ---------------------------------------------------------------------------
// Row 9 (bitty#2388 residue 3, CTX-1931): the keybind gesture — the
// PRESENTATION_CMD_* workspace command names move the FOCUSED presented
// layer's mode through the string dispatch, the same single gate the ctl
// verb runs
// ---------------------------------------------------------------------------

#[test]
fn the_presentation_command_names_move_the_focused_presented_layer() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panels_at(&mut rt, &[PRIMARY]);
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("layer"))
        .expect("primary presents");
    assert_eq!(presented.presentation, PresentationMode::Tiled);

    // The keybind resolves the opposite mode's command NAME; the string
    // dispatch applies it through the single transition gate.
    let moved = rt
        .apply_panel_content_mode_command(PRIMARY, bitty_ui::PRESENTATION_CMD_FLOATING)
        .expect("the float command moves the layer");
    assert_eq!(moved, PresentationMode::Floating);
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Floating),
        "the leaf itself carries the mode the command stamped"
    );
    assert_eq!(
        rt.panel_content_layer(PRIMARY)
            .map(|layer| layer.presentation),
        Some(PresentationMode::Floating),
        "the record carries it too, so release can tell the layer moved the leaf"
    );

    // Toggling back through the tiled command name.
    let back = rt
        .apply_panel_content_mode_command(PRIMARY, bitty_ui::PRESENTATION_CMD_TILED)
        .expect("the tiled command moves the layer back");
    assert_eq!(back, PresentationMode::Tiled);
    assert_eq!(rt.view_presentation(PRIMARY), Some(PresentationMode::Tiled));

    // A redundant command is the same no-op the direct verb is: nothing
    // stamps, and the call still reports the requested mode.
    let same = rt
        .apply_panel_content_mode_command(PRIMARY, bitty_ui::PRESENTATION_CMD_TILED)
        .expect("a redundant command is admitted as a no-op");
    assert_eq!(same, PresentationMode::Tiled);
    assert_eq!(
        rt.panel_content_layer(PRIMARY)
            .map(|layer| layer.presentation),
        Some(PresentationMode::Tiled),
        "a redundant command claims no change"
    );
    assert_eq!(
        rt.panel_content_layer(PRIMARY)
            .map(|layer| layer.prior_presentation),
        Some(PresentationMode::Tiled),
        "and never rewrites the mode release will restore"
    );
}

// ---------------------------------------------------------------------------
// Row 10 (bitty#2388 residue 3, CTX-1931): the same dispatch refuses typed —
// no layer, no panel, an unknown command name, and a placement-less mode all
// answer PanelError and leave the tree alone, never a fall-through mutation
// ---------------------------------------------------------------------------

#[test]
fn the_presentation_command_dispatch_refuses_typed_and_moves_nothing() {
    let mut rt = runtime();
    install(&mut rt, panes_and_a_bare_leaf());
    install_panels_at(&mut rt, &[PRIMARY]);

    // No layer presented: typed refusal, tree unchanged.
    match rt.apply_panel_content_mode_command(PRIMARY, bitty_ui::PRESENTATION_CMD_FLOATING) {
        Err(PanelError::ContentNotPresented { .. }) => {}
        other => panic!("no layer must refuse typed, got {other:?}"),
    }
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Tiled),
        "the refused command moved nothing"
    );

    // A leaf with no panel mounted at all answers in the same vocabulary.
    match rt.apply_panel_content_mode_command(BARE, bitty_ui::PRESENTATION_CMD_FLOATING) {
        Err(PanelError::ContentUnavailable { .. }) => {}
        other => panic!("a bare leaf must refuse typed, got {other:?}"),
    }
    assert_eq!(rt.view_presentation(BARE), Some(PresentationMode::Tiled));

    // An unknown command name is a typed refusal, not a silent drop.
    match rt.apply_panel_content_mode_command(PRIMARY, "bitty.workspace:not-a-command") {
        Err(PanelError::ContentUnavailable { .. }) => {}
        other => panic!("an unknown command must refuse typed, got {other:?}"),
    }

    // The placement-less mode commands (the other two registry names) are the
    // same honest typed refusal the direct verb answers, never a stamp that
    // paints exactly like the mode it replaced.
    rt.present_panel_content(PRIMARY, text_layer("layer"))
        .expect("primary presents");
    for command in [
        bitty_ui::PRESENTATION_CMD_FULLSCREEN,
        bitty_ui::PRESENTATION_CMD_SCRATCHPAD,
    ] {
        match rt.apply_panel_content_mode_command(PRIMARY, command) {
            Err(PanelError::ContentPresentationUnsupported { mode, .. }) => {
                assert!(
                    command.ends_with(mode),
                    "the refusal names the mode the command carried: {command} vs {mode}"
                );
            }
            other => panic!("{command} must be refused, got {other:?}"),
        }
        assert_eq!(
            rt.view_presentation(PRIMARY),
            Some(PresentationMode::Tiled),
            "the refused command moved nothing"
        );
        assert_eq!(
            rt.panel_content_layer(PRIMARY)
                .map(|layer| layer.presentation),
            Some(PresentationMode::Tiled),
            "and the record did not move either"
        );
    }
}

// ---------------------------------------------------------------------------
// Rows 11-13 (bitty#2388 slice, CTX-1936): `Fullscreen` through the installed
// app-level zoom owner — the support half row 4's corrected premise promises
// ---------------------------------------------------------------------------

/// Interior state of [`MiniZoom`]: one zoom pair plus the call counts the
/// rows assert against.
#[derive(Default)]
struct MiniZoomState {
    /// Times the gate asked the owner to engage.
    engages: u32,
    /// Times the gate asked the owner to restore.
    restores: u32,
    /// The captured live tree, held while the zoom is engaged.
    backup: Option<LayoutNode>,
}

/// A recording app-zoom double for the fullscreen rows: mirrors the host
/// application's zoom pair (capture the live tree, install the single-leaf
/// proxy, put the tree back on restore) so the runtime rows observe the real
/// composition shape, and counts every engage/restore the gate asks for —
/// the observable half of "the owner owns the change".
struct MiniZoom {
    state: std::rc::Rc<std::cell::RefCell<MiniZoomState>>,
}

impl MiniZoom {
    fn new() -> std::rc::Rc<Self> {
        std::rc::Rc::new(Self {
            state: std::rc::Rc::default(),
        })
    }

    /// The user's own unzoom, out of band: consumes the zoom without the
    /// layer's give-back running, the way `toggle_zoom` does.
    fn unzoom_out_of_band(&self, rt: &mut Runtime) {
        let backup = self.state.borrow_mut().backup.take();
        if let Some(backup) = backup {
            rt.set_layout(backup);
        }
    }
}

impl bitty_runtime::PanelFullscreenOwner for MiniZoom {
    fn engage(&self, rt: &mut Runtime, view: ViewId) -> bool {
        let mut state = self.state.borrow_mut();
        if state.backup.is_some() {
            return false;
        }
        let Some(leaf) = rt.layout().find_leaf(view).cloned() else {
            return false;
        };
        let backup = rt.layout().clone();
        rt.set_layout(LayoutNode::leaf(leaf));
        state.backup = Some(backup);
        state.engages = state.engages.saturating_add(1);
        true
    }

    fn restore(&self, rt: &mut Runtime, view: ViewId) -> bool {
        let _ = view;
        let mut state = self.state.borrow_mut();
        state.restores = state.restores.saturating_add(1);
        match state.backup.take() {
            Some(backup) => {
                rt.set_layout(backup);
                true
            }
            None => false,
        }
    }
}

/// Installs a [`MiniZoom`] owner on `rt` and returns the handle its rows
/// read the call counts through.
fn install_zoom_owner(rt: &mut Runtime) -> std::rc::Rc<MiniZoom> {
    let zoom = MiniZoom::new();
    rt.set_panel_fullscreen_owner(Some(
        std::rc::Rc::clone(&zoom) as std::rc::Rc<dyn bitty_runtime::PanelFullscreenOwner>
    ));
    zoom
}

// ---------------------------------------------------------------------------
// Row 11: entering fullscreen rides the owner, and the gate never re-engages
// ---------------------------------------------------------------------------

#[test]
fn a_presented_layer_fullscreens_through_the_installed_zoom_owner() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PANE]);
    assert!(rt.set_focus(PANE), "the pane holds focus");
    let zoom = install_zoom_owner(&mut rt);
    let presented = rt
        .present_panel_content(PANE, text_layer("dashboard"))
        .expect("present is admitted");
    let winsize = rt.pane_pty_size(&PANE).expect("live pane winsize");
    let tiled = frame_of(&rt, PANE);

    // The direct verb hands the mode to the owner: the zoom engages before
    // anything stamps, the leaf carries `Fullscreen`, and the record carries
    // both the mode and the leaf mode it displaced.
    let changed = rt
        .set_panel_content_presentation(PANE, presented.token, PresentationMode::Fullscreen)
        .expect("a presented panel may fullscreen through the owner");
    assert_eq!(zoom.state.borrow().engages, 1, "the owner engaged once");
    assert_eq!(zoom.state.borrow().restores, 0);
    assert_eq!(changed.view, PANE);
    assert_eq!(
        changed.token, presented.token,
        "the handle survives the move"
    );
    assert_eq!(changed.presentation, PresentationMode::Fullscreen);
    assert_eq!(
        changed.prior_presentation,
        PresentationMode::Tiled,
        "a mode change never rewrites the mode release will restore"
    );
    assert_eq!(
        changed.pre_fullscreen_presentation,
        Some(PresentationMode::Tiled),
        "the record carries the leaf mode the fullscreen displaced"
    );
    assert_eq!(
        rt.view_presentation(PANE),
        Some(PresentationMode::Fullscreen),
        "the live (proxy) leaf carries the new mode"
    );
    // The zoom is real: the live tree collapsed to the single-leaf proxy and
    // the view paints full-bleed base content, wider than its half-slot.
    assert_eq!(
        rt.layout().leaf_ids().len(),
        1,
        "the owner installed the single-leaf proxy"
    );
    let full = frame_of(&rt, PANE);
    assert_eq!(full.tier, None, "a fullscreen leaf is base content");
    assert!(
        usize::from(full.cols) > usize::from(tiled.cols),
        "the fullscreen frame is wider than the half-slot: {} vs {}",
        full.cols,
        tiled.cols
    );

    // The hidden terminal is untouched: same winsize now, and still frozen
    // across the layout pass the change triggers.
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "fullscreening the layer must not resize the hidden PTY"
    );
    assert_eq!(
        rt.panel_content_kind(PANE),
        Some(ViewContent::Rich(presented.token.get())),
        "the panel still presents the layer, not its terminal"
    );
    let stats = rt.tick().expect("frame in the new mode");
    assert!(stats.glyphs > 0, "the layer paints full-bleed");
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "the layout pass after the change still refuses to touch the frozen PTY"
    );

    // The string dispatch — the keybind's spelling — rides the same owner.
    let again = rt
        .apply_panel_content_mode_command(PANE, bitty_ui::PRESENTATION_CMD_FULLSCREEN)
        .expect("the fullscreen command is answerable with an owner");
    assert_eq!(again, PresentationMode::Fullscreen);
    assert_eq!(
        zoom.state.borrow().engages,
        1,
        "a redundant request never re-engages the zoom"
    );

    // Leaving fullscreen gives the zoom back before the stamp: the real tree
    // returns, and the stamp lands on the leaf the disengage restored.
    let floated = rt
        .set_panel_content_presentation(PANE, presented.token, PresentationMode::Floating)
        .expect("a fullscreen layer may return to a float");
    assert_eq!(zoom.state.borrow().engages, 1);
    assert_eq!(
        zoom.state.borrow().restores,
        1,
        "leaving fullscreen restores the zoom exactly once"
    );
    assert_eq!(rt.layout().leaf_ids().len(), 2, "the real tree is back");
    assert_eq!(floated.presentation, PresentationMode::Floating);
    assert_eq!(
        floated.pre_fullscreen_presentation, None,
        "the record drops the displaced leaf mode when fullscreen ends"
    );
    assert_eq!(rt.view_presentation(PANE), Some(PresentationMode::Floating));
    assert_eq!(frame_of(&rt, PANE).tier, Some(OverlayTier::Float));
}

// ---------------------------------------------------------------------------
// Row 12: release of a fullscreen layer restores the zoom AND the mode, and
// owes exactly one restore resize
// ---------------------------------------------------------------------------

#[test]
fn release_of_a_fullscreen_layer_restores_the_zoom_the_mode_and_owes_one_resize() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panels_at(&mut rt, &[PANE]);
    let zoom = install_zoom_owner(&mut rt);
    let presented = rt
        .present_panel_content(PANE, text_layer("dashboard"))
        .expect("present is admitted");
    let winsize = rt.pane_pty_size(&PANE).expect("live pane winsize");
    let restores_before = rt.content_restore_resizes();

    // Tiled -> Floating -> Fullscreen: the record the release reads says the
    // layer last requested Fullscreen and displaced a Tiled leaf.
    rt.set_panel_content_presentation(PANE, presented.token, PresentationMode::Floating)
        .expect("a presented panel may float");
    rt.set_panel_content_presentation(PANE, presented.token, PresentationMode::Fullscreen)
        .expect("a floated layer may fullscreen through the owner");
    assert_eq!(zoom.state.borrow().engages, 1);

    // The container grows while the layer is fullscreen: the freeze keeps the
    // hidden PTY at the frozen winsize across the proxy's relayout.
    rt.set_container(UiRect::new(0, 0, 100, 30));
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "the freeze keeps the hidden PTY at the frozen winsize across the relayout"
    );

    let restored = rt
        .release_panel_content(PANE, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(restored.content, ViewContent::Terminal(PANE.0));
    assert_eq!(restored.presentation, PresentationMode::Fullscreen);
    assert_eq!(restored.prior_presentation, PresentationMode::Tiled);
    assert_eq!(
        restored.pre_fullscreen_presentation,
        Some(PresentationMode::Floating),
        "the release carries the leaf mode the fullscreen displaced"
    );

    // Both halves came back atomically, before the resize was measured:
    // the zoom (the real tree returns) and the mode (the panel is Tiled
    // again, not Floating and not Fullscreen).
    assert_eq!(
        zoom.state.borrow().restores,
        1,
        "the release gave the zoom back exactly once"
    );
    assert_eq!(rt.layout().leaf_ids().len(), 2, "the real tree is back");
    assert_eq!(
        rt.view_presentation(PANE),
        Some(PresentationMode::Tiled),
        "release gives the mode the layer displaced back"
    );
    let back = frame_of(&rt, PANE);
    assert_eq!(back.tier, None, "the panel is back in its own slot");
    assert_eq!(
        (back.cols, back.rows),
        (47, 28),
        "the returned slot reflects the relayouted container"
    );
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some((back.cols, back.rows)),
        "the restore resized the PTY to the returned slot exactly once"
    );
    assert_eq!(
        rt.content_restore_resizes(),
        restores_before + 1,
        "exactly one measured restore resize: the release's own sync delivered \
         the movement (the zoom give-back's layout install had re-placed the \
         leaf at the float frame on its way back; the mode give-back then \
         returned it to the tiled slot, and this is the one measured hop)"
    );
    assert!(!rt.content_layer_presented(PANE), "the layer is gone");
}

// ---------------------------------------------------------------------------
// Row 13: after the user unzooms out of band and a third party moves the
// leaf, release restores the content and leaves that intent alone
// ---------------------------------------------------------------------------

#[test]
fn release_leaves_a_third_party_mode_alone_after_the_user_unzoomed() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panels_at(&mut rt, &[PRIMARY]);
    let zoom = install_zoom_owner(&mut rt);
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("layer"))
        .expect("primary presents");
    // Float first so the record's displaced leaf mode (`Floating`) differs
    // from the mode release would give back (`Tiled`): only then can the
    // third-party guard refuse on its own, exactly like row 8's ordinary twin.
    rt.set_panel_content_presentation(PRIMARY, presented.token, PresentationMode::Floating)
        .expect("the layer floats through the verb");
    rt.set_panel_content_presentation(PRIMARY, presented.token, PresentationMode::Fullscreen)
        .expect("the floated layer fullscreens through the owner");
    assert_eq!(zoom.state.borrow().engages, 1);

    // The user unzooms out of band: the zoom gives back the tree the engage
    // captured, so the leaf returns to the mode it carried before the layer
    // went fullscreen — Floating, not the proxy's Fullscreen stamp.
    zoom.unzoom_out_of_band(&mut rt);
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Floating),
        "the out-of-band unzoom returns the captured tree, leaf at the displaced mode"
    );
    assert!(
        zoom.state.borrow().backup.is_none(),
        "the zoom left with the user"
    );

    // Third party, out of band, after the zoom left: the leaf carries
    // somebody else's intent now, and that outranks the layer's.
    stamp_out_of_band(&mut rt, PRIMARY, PresentationMode::Scratchpad);
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Scratchpad)
    );

    let restored = rt
        .release_panel_content(PRIMARY, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(restored.presentation, PresentationMode::Fullscreen);
    assert_eq!(
        restored.pre_fullscreen_presentation,
        Some(PresentationMode::Floating)
    );
    assert_eq!(
        rt.view_presentation(PRIMARY),
        Some(PresentationMode::Scratchpad),
        "release restores content, never the mode a third party stamped after the zoom left"
    );
    assert_eq!(
        zoom.state.borrow().restores,
        1,
        "the release still asked the owner once — it answered that nothing was held"
    );
    assert_eq!(
        rt.panel_content_kind(PRIMARY),
        Some(ViewContent::Terminal(PRIMARY.0)),
        "the content is restored either way"
    );
    assert!(
        !rt.content_layer_presented(PRIMARY),
        "and the layer is gone"
    );
}
