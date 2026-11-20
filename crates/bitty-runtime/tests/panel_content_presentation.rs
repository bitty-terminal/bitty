//! Panel GUI content presentation: the first bounded Core slice of bitty#2388
//! (CTX-1895).
//!
//! Contract rows proven here (candidate
//! `specifications/panel-gui-content-presentation-candidate.md`):
//!
//! 1. **Content-kind swap, terminal truth intact.** A terminal-backed panel
//!    presents a `Rich` content layer; the displaced `Terminal` content keeps
//!    its `TerminalId` (the live session key), its child pid, its PTY winsize,
//!    its grid, and its scrollback. Release restores exactly the recorded
//!    content kind, on the same panel, at the same attachment.
//! 2. **Geometry frozen while hidden.** A window resize and a layout install
//!    reflow the tree but never enter the `LogicalRect -> PTY` step for the
//!    presented leaf: `TIOCGWINSZ` keeps returning the frozen size and no
//!    `SIGWINCH` is generated. The unpresented primary is the control: it does
//!    reflow, which proves the resize reached the runtime and only the
//!    presented leaf was skipped. The freeze covers the pixel axes too, so a
//!    font-zoom/DPI change (a pixel-only `TIOCSWINSZ`, which the kernel also
//!    answers with `SIGWINCH`) can no longer reach the hidden child.
//! 3. **One restore resize, then a full repaint.** Release measures the
//!    current content rect, issues the existing per-leaf resize path once when
//!    it differs from the frozen rect, and arms a full repaint. The
//!    `content_restore_resizes` counter is read on the kernel side (the
//!    `TIOCGWINSZ` movement), so the counter and the winsize assertion say the
//!    same thing.
//! 4. **Input ownership.** While the layer is presented the focused panel's
//!    keys are swallowed before the encoder on both keyboard entry points
//!    (`handle_key_event` and `handle_key_event_ref`), and every non-key input
//!    path is dropped at one of the two byte routers — `push_input_bytes`
//!    (paste, IME commit, a programmatic write, and the CTX-0243 snap that runs
//!    ahead of it) and `route_input_bytes_to` (pointer reports, focus reports):
//!    nothing is encoded, nothing is buffered, and the transport buffer stays
//!    empty. Release returns input to the terminal on the very next key,
//!    byte-identically.
//! 5. **Typed refusals, fail-closed.** Duplicate present, present on a
//!    non-terminal-backed panel, present with no panel mounted, release with no
//!    layer, a stale or foreign layer handle, and a scene detach behind a live
//!    layer all fail typed and change nothing. A presented record leaves by its
//!    owner's token release, by its generation's edge, or with the container.
//! 6. **A layer never outlives its generation.** The generation edge releases
//!    the record through the panel host's binding (no token: the presenting
//!    generation is the one ending), so the container comes back
//!    terminal-backed, the freeze and the input gate lift, and a later
//!    generation presents on the same panel. The release is fenced on that
//!    binding, so a displaced generation cannot reach the record.
//! 7. **A close gives the displaced terminal back.** A workspace close (and the
//!    app's close paths, covered by their own rows) releases a layer still
//!    presented on a panel of that workspace *before* the sweep kills the
//!    session and disposes the container, keyed on panel identity rather than a
//!    token the close never holds. The owner's row is pruned and its release is
//!    recorded as `closed`, so the layer is given back instead of silently
//!    dropped with its container.
//!
//! Every assertion reads observable state — the PTY's own winsize, the pane's
//! grid, the transport buffer, the presented frame report — never a value the
//! production code derived from the same container it asserted against.
//!
//! # Verification limits
//!
//! A row whose observable is a *live pane's* kernel winsize needs a real PTY:
//! a pane-backed panel is a `PaneSession`, which owns a `Pty`, so no build
//! without a PTY backend can construct one. Six rows read that observable and
//! therefore call `bitty_test_support::require_pty!()` as their **first
//! statement** — `present_swaps_…`, `geometry_stays_…`,
//! `primary_owner_freezes_…`, `a_pixel_only_…`, `a_foreign_handle_…`, and
//! `release_reconciles_…` — so the whole row skips on such a build (the
//! workspace-wide skip convention, not evidence). The gate lives in the row
//! and never in a helper: `require_pty!()` expands to a plain `return`, which
//! inside a helper only leaves the helper and lets the rest of the row run
//! against a pane that was never spawned. `BITTY_TEST_FORCE_NO_PTY=1 cargo test
//! -p bitty-runtime --test panel_content_presentation` is the check: the six
//! rows print `SKIP live-PTY test` (visible with `-- --nocapture`) and the run
//! reports `17 passed; 0 failed` — libtest counts a skipped row as passed, so
//! the SKIP notices are what tell the two apart.
//!
//! The PTY-independent halves of the same claims are proven here without the
//! gate: `grid_geometry_freeze_holds_without_a_pty_backend` covers the grid
//! half of the freeze, and the input rows — including the router-gate and
//! submit-outcome rows — need no PTY at all. The refusal row's second case
//! deliberately keeps no live pane, so it cannot skip.

#![cfg(unix)]

use bitty_platform::{
    CursorPosition, KeyEvent, KeyLocation, LogicalKey, MouseButton, MouseEvent, PhysicalSize,
    PressState,
};
use bitty_rich::host::SubmitBudget;
use bitty_rich::scene::{
    BlockAnchor, BlockId, RichBlock, Scene, SceneNode, ScrollBehavior, StyledSpan,
};
use bitty_runtime::registry::{
    LeaseHolder, PanelError, PanelLease, PanelRegistryConfig, PanelRuntime, PanelType, WorkspaceId,
};
use bitty_runtime::{
    LayoutNode, Runtime, SplitAxis, TerminalSubmitOutcome, TerminalSubmitUnavailable, UiRect, View,
    ViewId,
};
use bitty_ui::panel::ViewContent;

/// Primary leaf: paints the runtime-global primary grid (no pane session).
const PRIMARY: ViewId = ViewId::new(1);
/// Pane leaf: owns its own shell session.
const PANE: ViewId = ViewId::new(2);

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("headless defaults must build")
}

fn single_leaf() -> LayoutNode {
    LayoutNode::leaf(View::new(PRIMARY, 80, 24))
}

fn two_panes() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

/// A host with one `Terminal` panel mounted at `view`, installed on `rt`.
fn install_panel_at(rt: &mut Runtime, view: ViewId) {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host");
    let handle = host
        .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
        .expect("terminal panel");
    host.mount_panel(handle.id, handle.generation, view)
        .expect("panel mounts at the leaf");
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

fn pane_text(rt: &Runtime, view: ViewId) -> String {
    match rt.pane_snapshot(&view) {
        Some(snap) => snap.cells.iter().map(|cell| cell.glyph).collect(),
        None => String::new(),
    }
}

fn char_press(ch: char) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

/// Spawns `PANE`'s shell. Callers must gate themselves with
/// `bitty_test_support::require_pty!()` **first**: the macro expands to a plain
/// `return`, so a gate inside this helper would leave the helper and let the
/// caller continue against a pane that was never spawned (bitty#3250 review).
fn spawn_pane(rt: &mut Runtime) {
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell spawns");
}

// ---------------------------------------------------------------------------
// Row 1: content-kind swap keeps Terminal Truth, release restores it exactly
// ---------------------------------------------------------------------------

#[test]
fn present_swaps_content_kind_and_keeps_the_terminal_alive() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panel_at(&mut rt, PANE);
    rt.handle_pane_bytes(PANE, b"hidden-line-with-a-much-longer-tail\r\n");
    let grid_glyphs = rt.tick().expect("baseline frame").glyphs;
    assert!(
        grid_glyphs >= 20,
        "the baseline frame must paint the pane's text: {grid_glyphs} glyphs"
    );

    let pid = rt.pane_pid(&PANE).expect("live pane child");
    let winsize = rt.pane_pty_size(&PANE).expect("live pane winsize");
    let before = pane_text(&rt, PANE);
    assert!(
        before.contains("hidden-line-with-a-much-longer-tail"),
        "the pane grid must carry the pre-swap output: {before:?}"
    );
    assert_eq!(
        rt.panel_content_kind(PANE),
        Some(ViewContent::Terminal(PANE.0)),
        "a pane-backed panel starts on the terminal content kind"
    );

    let presented = rt
        .present_panel_content(PANE, text_layer("dashboard"))
        .expect("present is admitted on a terminal-backed panel");

    // The record names the panel, the attachment, and the exact displaced kind.
    assert_eq!(presented.view, PANE);
    assert_eq!(presented.displaced, ViewContent::Terminal(PANE.0));
    assert_eq!(
        presented.presented,
        ViewContent::Rich(presented.token.get())
    );
    assert_eq!(
        rt.panel_content_kind(PANE),
        Some(ViewContent::Rich(presented.token.get())),
        "the panel now presents GUI content"
    );
    assert!(rt.content_layer_presented(PANE));

    // Terminal Truth is untouched: same child, same winsize, same grid text.
    assert_eq!(
        rt.pane_pid(&PANE),
        Some(pid),
        "the child was never respawned"
    );
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(winsize),
        "presenting a layer must not resize the hidden PTY"
    );
    assert_eq!(
        pane_text(&rt, PANE),
        before,
        "the hidden grid must be byte-identical after the swap"
    );

    // Output written while hidden still parses into the live grid: the
    // displaced terminal is hidden, not suspended.
    rt.handle_pane_bytes(PANE, b"while-hidden\r\n");
    let hidden = pane_text(&rt, PANE);
    assert!(
        hidden.contains("while-hidden") && hidden.contains("hidden-line-with-a-much-longer-tail"),
        "a hidden pane keeps parsing: {hidden:?}"
    );

    // The layer replaces the grid on the paint path: the frame paints the
    // layer's text and none of the displaced grid's glyphs.
    let stats = rt.tick().expect("presented frame");
    assert!(stats.glyphs > 0, "the layer paints through the text seam");
    assert!(
        stats.glyphs < grid_glyphs,
        "the displaced grid must not paint: {} glyphs with the layer up vs {grid_glyphs} without it",
        stats.glyphs
    );
    assert_eq!(rt.scene_frame().views, 1, "one leaf paints the layer");
    assert_eq!(rt.scene_frame().refusals, 0);

    // Release restores exactly what was displaced, on the same panel/view.
    let restored = rt
        .release_panel_content(PANE, presented.token)
        .expect("release restores the terminal content");
    assert_eq!(restored.panel, presented.panel);
    assert_eq!(restored.view, PANE);
    assert_eq!(restored.content, presented.displaced);
    assert_eq!(restored.token, presented.token);
    assert_eq!(
        rt.panel_content_kind(PANE),
        Some(ViewContent::Terminal(PANE.0)),
        "release restores the previous content kind exactly"
    );
    assert!(!rt.content_layer_presented(PANE));
    assert_eq!(
        pane_text(&rt, PANE),
        hidden,
        "restore keeps every byte the hidden pane parsed"
    );
    assert_eq!(rt.pane_pid(&PANE), Some(pid));
}

// ---------------------------------------------------------------------------
// Row 2 + 3: frozen geometry while hidden, one restore resize afterwards
// ---------------------------------------------------------------------------

#[test]
fn geometry_stays_frozen_while_hidden_and_restores_with_one_resize() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panel_at(&mut rt, PANE);
    rt.handle_pane_bytes(PANE, b"line\r\n");
    rt.tick().expect("baseline frame");

    let frozen_winsize = rt.pane_pty_size(&PANE).expect("pane winsize");
    let frozen_cols = rt.pane_snapshot(&PANE).expect("pane grid").width;
    let primary_cols = rt.snapshot().width;
    let presented = rt
        .present_panel_content(PANE, text_layer("frozen"))
        .expect("present is admitted");
    let freezes_before = rt.content_geometry_freezes();

    // A window resize reflows the tree and both leaves' frames. The presented
    // leaf must keep its committed PTY geometry: no resize call, no SIGWINCH.
    rt.handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");

    assert!(
        rt.content_geometry_freezes() > freezes_before,
        "the per-leaf geometry sync ran and was frozen (not skipped entirely)"
    );
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(frozen_winsize),
        "TIOCGWINSZ must keep returning the frozen size while the layer is up"
    );
    assert_eq!(
        rt.pane_snapshot(&PANE).expect("pane grid").width,
        frozen_cols,
        "the hidden grid must not reflow"
    );
    // Control: the unpresented primary did reflow, so the resize really landed
    // and only the presented leaf was frozen.
    assert_ne!(
        rt.snapshot().width,
        primary_cols,
        "the control primary leaf must reflow on the same gesture"
    );

    // A layout install is the second `LogicalRect -> PTY` seam: still frozen.
    // Move the container first, then reinstall the tree, so the reflow inside
    // `set_layout` really changes this leaf's frame.
    rt.set_container(UiRect::new(0, 0, 60, 20));
    let before_install = rt.content_geometry_freezes();
    let layout = rt.layout().clone();
    rt.set_layout(layout);
    assert!(
        rt.content_geometry_freezes() > before_install,
        "the layout-install path is gated too"
    );
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some(frozen_winsize),
        "a layout install must not resize the hidden PTY"
    );

    // Release issues exactly one resize, to the leaf's current content rect.
    let frame = frame_of(&rt, PANE);
    assert_ne!(
        (frame.cols, frame.rows),
        (frozen_winsize.0, frozen_winsize.1),
        "the gesture must have moved the leaf, or the restore resize proves nothing"
    );
    rt.release_panel_content(PANE, presented.token)
        .expect("release restores geometry");
    assert_eq!(
        rt.pane_pty_size(&PANE),
        Some((frame.cols, frame.rows)),
        "release resizes the PTY to the current content rect"
    );
    assert_eq!(
        rt.pane_snapshot(&PANE).expect("pane grid").width,
        usize::from(frame.cols),
        "the restored grid reflows to the current content rect"
    );
    assert_eq!(
        rt.content_restore_resizes(),
        1,
        "exactly one restore resize per release"
    );

    // No second resize from the same gesture: a repeated sync with unchanged
    // geometry neither adds a restore resize nor moves the winsize again.
    let settled = rt.pane_pty_size(&PANE);
    let layout = rt.layout().clone();
    rt.set_layout(layout);
    assert_eq!(
        rt.content_restore_resizes(),
        1,
        "a repeated identical layout sync must not add a restore resize"
    );
    assert_eq!(
        rt.pane_pty_size(&PANE),
        settled,
        "the restored PTY stays at its content rect"
    );
}

#[test]
fn primary_owner_freezes_on_window_resize_and_restores_once() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("primary shell spawns");
    install_panel_at(&mut rt, PRIMARY);
    rt.handle_pty_bytes(b"primary-line\r\n");
    rt.tick().expect("baseline frame");
    assert_eq!(
        rt.primary_view(),
        Some(PRIMARY),
        "primary owner is the leaf"
    );

    let frozen = (rt.snapshot().width, rt.snapshot().height);
    let frozen_pty = rt.pty_size().expect("primary PTY winsize");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("primary-layer"))
        .expect("the primary owner is a terminal-backed panel");
    let freezes_before = rt.content_geometry_freezes();

    rt.handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");

    assert!(rt.content_geometry_freezes() > freezes_before);
    assert_eq!(
        (rt.snapshot().width, rt.snapshot().height),
        frozen,
        "the runtime-global primary grid keeps its frozen size while hidden"
    );
    assert_eq!(
        rt.pty_size(),
        Some(frozen_pty),
        "the runtime-global primary PTY keeps its frozen winsize while hidden"
    );

    let frame = frame_of(&rt, PRIMARY);
    let target = (frame.cols, frame.rows);
    assert_ne!(
        target, frozen_pty,
        "the gesture must have moved the leaf, or the restore resize proves nothing"
    );
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the primary");
    assert_eq!(
        (rt.snapshot().width, rt.snapshot().height),
        (usize::from(frame.cols), usize::from(frame.rows)),
        "release resizes the primary grid to the current content rect"
    );
    assert_eq!(
        rt.pty_size(),
        Some(target),
        "release resizes the primary PTY exactly once"
    );
    assert_eq!(rt.content_restore_resizes(), 1);
}

#[test]
fn a_pixel_only_winsize_change_never_reaches_the_hidden_pty() {
    bitty_test_support::require_pty!();
    // bitty#3250 R3: `TIOCSWINSZ` delivers `SIGWINCH` even when only the pixel
    // fields move, so the freeze has to cover the ioctl itself and not just
    // the cell axes. A DPI/font-zoom change at a fixed grid is exactly that
    // pixel-only write.
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("primary shell spawns");
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let extent = rt.surface_extent().expect("headless extent");

    // Control: with nothing presented the same gesture really does move the
    // kernel's pixel fields, so the frozen reading below is the gate and not a
    // gesture that never reached the ioctl.
    let before = rt.pty_pixel_size().expect("primary PTY pixel winsize");
    rt.apply_dpi_scale(1.5, Some(extent));
    let scaled = rt.pty_pixel_size().expect("primary PTY pixel winsize");
    assert_ne!(
        (scaled.2, scaled.3),
        (before.2, before.3),
        "an unpresented primary must take the pixel geometry the DPI change asks for"
    );

    rt.apply_dpi_scale(1.0, Some(extent));
    let frozen = rt.pty_pixel_size().expect("primary PTY pixel winsize");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("pixels"))
        .expect("the primary owner is a terminal-backed panel");

    rt.apply_dpi_scale(1.5, Some(extent));
    assert_eq!(
        rt.pty_pixel_size(),
        Some(frozen),
        "a pixel-only winsize change must not reach the hidden PTY while the layer is up"
    );

    // The freeze is a gate, not a latch: release and the next reflow deliver
    // the geometry the hidden gesture could not.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the primary");
    rt.handle_resize(PhysicalSize::new(1400, 900))
        .expect("window resize applies");
    let after = rt.pty_pixel_size().expect("primary PTY pixel winsize");
    assert_ne!(
        (after.2, after.3),
        (frozen.2, frozen.3),
        "the lifted freeze must resync the pixel axes"
    );
}

// ---------------------------------------------------------------------------
// Row 4: input ownership
// ---------------------------------------------------------------------------

#[test]
fn presented_layer_swallows_keys_and_release_returns_the_encoder() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");

    // Baseline: the encoder path reaches the transport buffer.
    assert_eq!(
        rt.handle_key_event(char_press('x')),
        Some(vec![b'x']),
        "the terminal owns its keys before any layer"
    );
    assert_eq!(rt.drain_pending_input(), vec![b'x']);

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("keys"))
        .expect("present is admitted");
    let swallowed_before = rt.content_input_swallowed();

    // Both keyboard entry points swallow. The owned event is dropped and
    // counted; it is *not* retained for a later delivery (the owner-polled
    // queue is a later slice), so no presented-time key is ever returned.
    assert_eq!(
        rt.handle_key_event(char_press('y')),
        None,
        "a presented layer swallows the key before the encoder"
    );
    assert_eq!(
        rt.handle_key_event_ref(&char_press('w')),
        None,
        "the borrowed entry point reads the same gate (bitty#3250 R1)"
    );
    assert_eq!(
        rt.content_input_swallowed(),
        swallowed_before + 2,
        "every swallowed key is counted, not silently dropped"
    );
    assert_eq!(
        rt.pending_input_len(),
        0,
        "no key byte may reach the displaced terminal's transport"
    );
    assert_eq!(rt.drain_pending_input(), Vec::<u8>::new());

    // Release hands the keyboard back: only post-release keys are encoded, and
    // they are byte-identical to the pre-present encoding.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores input ownership");
    assert_eq!(
        rt.handle_key_event(char_press('z')),
        Some(vec![b'z']),
        "keys reach the terminal again after release"
    );
    assert_eq!(
        rt.handle_key_event_ref(&char_press('v')),
        Some(vec![b'v']),
        "the borrowed entry point returns the key too once the layer is gone"
    );
    assert_eq!(rt.drain_pending_input(), vec![b'z', b'v']);
    assert_eq!(
        rt.content_input_swallowed(),
        swallowed_before + 2,
        "only the presented keypresses were swallowed"
    );
}

#[test]
fn paste_ime_and_programmatic_writes_cannot_reach_a_hidden_shell() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    // Focus reporting is on (DECSET 1004), so the window focus transitions
    // below really produce protocol bytes for the primary owner.
    rt.set_focused(true);
    rt.handle_pty_bytes(b"\x1b[?1004h");

    // Control: with no layer every path below reaches the transport, so the
    // assertions after present are about the gate, not about a path that was
    // never live.
    rt.write_input(b"raw");
    assert_eq!(rt.drain_pending_input(), b"raw".to_vec());
    rt.handle_ime_commit("composed".to_string());
    assert_eq!(rt.drain_pending_input(), b"composed".to_vec());
    rt.paste_text("pasted");
    assert_eq!(rt.drain_pending_input(), b"pasted".to_vec());

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("no-bytes"))
        .expect("present is admitted");

    // bitty#3250 R2: paste, IME commit, and a programmatic write share the one
    // router, so one gate covers all three.
    let dropped_before = rt.content_input_dropped_bytes();
    rt.write_input(b"raw-bytes");
    assert_eq!(
        rt.content_input_dropped_bytes(),
        dropped_before + b"raw-bytes".len() as u64,
        "a programmatic write is dropped byte for byte"
    );
    let after_write = rt.content_input_dropped_bytes();
    rt.handle_ime_commit("composed".to_string());
    assert!(
        rt.content_input_dropped_bytes() > after_write,
        "a composed IME commit must not reach the hidden shell"
    );
    let after_ime = rt.content_input_dropped_bytes();
    rt.paste_text("pasted");
    assert!(
        rt.content_input_dropped_bytes() > after_ime,
        "a paste must not reach the hidden shell"
    );
    assert!(
        !rt.push_input_bytes(b"submit"),
        "the router reports the bytes as not delivered"
    );
    // Focus-report protocol bytes take the explicit-target router rather than
    // `push_input_bytes`; the same gate has to cover them, or a workspace
    // switch would still hand `CSI O`/`CSI I` to the hidden shell.
    let after_submit = rt.content_input_dropped_bytes();
    rt.set_focused(false);
    assert!(
        rt.content_input_dropped_bytes() > after_submit,
        "a focus report must not reach the hidden shell"
    );
    rt.set_focused(true);
    assert_eq!(
        rt.pending_input_len(),
        0,
        "dropped bytes are not buffered anywhere"
    );
    assert_eq!(
        rt.pending_input_dropped(),
        0,
        "this is the content gate, not the overflow path"
    );
    assert_eq!(rt.drain_pending_input(), Vec::<u8>::new());

    // Release restores the paths.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores input ownership");
    rt.write_input(b"back");
    assert_eq!(rt.drain_pending_input(), b"back".to_vec());
}

#[test]
fn a_dropped_write_never_snaps_a_scrolled_viewport_to_live() {
    // bitty#3250 R2, second half: `push_input_bytes` carries a content gate of
    // its own, and that hunk is load-bearing on an axis `route_input_bytes_to`
    // cannot cover — the CTX-0243 snap-to-live runs *before* the router, so a
    // paste or a programmatic write into a presented panel would yank a
    // scrolled viewport to the bottom even though the byte assertion in
    // `paste_ime_and_programmatic_writes_cannot_reach_a_hidden_shell` still
    // passes on the router gate. This row pins that half.
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    // Real scrollback, so paging up moves the viewport the snap would reset.
    for i in 0..80 {
        rt.handle_pty_bytes(format!("line-{i}\r\n").as_bytes());
    }
    rt.tick().expect("baseline frame");
    assert!(rt.scroll_focused_page(true), "the focused leaf pages");
    let scrolled = rt
        .presented_scroll_offset(PRIMARY)
        .expect("the primary owner is a live leaf");
    assert!(
        scrolled > 0,
        "the scroll gesture must move the viewport, or the snap assert proves nothing"
    );

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("scrolled"))
        .expect("present is admitted on the primary owner");
    assert_eq!(
        rt.presented_scroll_offset(PRIMARY),
        Some(scrolled),
        "presenting a layer must not disturb the recorded viewport"
    );

    let dropped_before = rt.content_input_dropped_bytes();
    assert!(
        !rt.push_input_bytes(b"paste"),
        "the router gate reports the bytes as not delivered"
    );
    assert_eq!(
        rt.content_input_dropped_bytes(),
        dropped_before + b"paste".len() as u64,
        "the router gate counts what it dropped"
    );
    assert_eq!(
        rt.presented_scroll_offset(PRIMARY),
        Some(scrolled),
        "a write the gate dropped must not snap the viewport to live (CTX-0243 snap)"
    );
    assert_eq!(
        rt.pending_input_len(),
        0,
        "a dropped write is not buffered on the way out"
    );

    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores input ownership");
    // Control: the same write is no longer dropped, so the CTX-0243 snap runs
    // exactly as it did before the layer existed. (The bytes themselves buffer
    // headless here: this fixture has no PTY writer, which is exactly why the
    // drop has to be proven by its own counter and not by the return value.)
    rt.push_input_bytes(b"live");
    assert_eq!(
        rt.presented_scroll_offset(PRIMARY),
        Some(0),
        "after release the same write reaches the snap-to-live path again"
    );
}

#[test]
fn a_content_gate_drop_reports_a_drop_not_a_headless_buffer() {
    // bitty#3250 R2: `push_input_bytes` answers `false` both for a frame that
    // only buffered headless and for one the content gate dropped. A caller
    // that maps every `false` to "buffered" mislabels the drop, so
    // `terminal.submit` reports the two apart and never charges the budget for
    // a frame that was thrown away.
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("composer"))
        .expect("present is admitted on the primary owner");

    let mut lease = PanelLease::idle();
    let holder = LeaseHolder(7);
    lease.acquire(holder, 100, 0).expect("lease acquires");
    let mut budget = SubmitBudget::new("composer.test", 1024 * 1024);
    let dropped_before = rt.content_input_dropped_bytes();
    let outcome = rt.terminal_submit("cargo test", &lease, holder, 1, &mut budget);

    assert_eq!(
        outcome,
        TerminalSubmitOutcome::Unavailable(TerminalSubmitUnavailable::ContentLayerPresented),
        "a content-gate drop is not a headless buffer"
    );
    assert_eq!(
        budget.used(),
        0,
        "a dropped frame is never charged against the submit window"
    );
    assert_eq!(
        rt.pending_input_len(),
        0,
        "a dropped frame is not buffered headless"
    );
    assert_eq!(
        rt.content_input_dropped_bytes(),
        dropped_before + b"\x1b[200~cargo test\x1b[201~\r".len() as u64,
        "the dropped frame is counted byte for byte"
    );

    // Control: with no layer the same call still reports `Unavailable` on this
    // headless fixture (no PTY writer), but now for the *other* reason — it
    // buffered the frame instead of dropping it. That is the distinction the
    // row is about: same `false` from the router, two different outcomes.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores input ownership");
    let mut budget = SubmitBudget::new("composer.test", 1024 * 1024);
    let outcome = rt.terminal_submit("cargo test", &lease, holder, 1, &mut budget);
    assert_eq!(
        outcome,
        TerminalSubmitOutcome::Unavailable(TerminalSubmitUnavailable::BufferedOnly),
        "with the layer gone the frame buffers headless, it is not dropped"
    );
    assert_eq!(
        rt.pending_input(),
        b"\x1b[200~cargo test\x1b[201~\r".as_slice(),
        "the buffered frame is kept for a later delivery, which the dropped one was not"
    );
}

#[test]
fn grid_geometry_freeze_holds_without_a_pty_backend() {
    // The PTY-backed freeze rows skip on a build without a PTY. This row keeps
    // the same claim's grid half — the reflow gate, not the ioctl — observable
    // on every build (bitty#3250 R9).
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let frozen = (rt.snapshot().width, rt.snapshot().height);

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("grid"))
        .expect("present is admitted on the primary owner");
    let freezes_before = rt.content_geometry_freezes();

    rt.handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");

    assert!(
        rt.content_geometry_freezes() > freezes_before,
        "the primary reflow ran and was frozen (not skipped entirely)"
    );
    assert_eq!(
        (rt.snapshot().width, rt.snapshot().height),
        frozen,
        "the frozen primary grid keeps its committed size"
    );

    // Release lifts the freeze, so the same grid follows the current frame.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the primary");
    let frame = frame_of(&rt, PRIMARY);
    assert_eq!(
        (rt.snapshot().width, rt.snapshot().height),
        (usize::from(frame.cols), usize::from(frame.rows)),
        "the restored grid takes the current content rect"
    );
    assert_ne!(
        (usize::from(frame.cols), usize::from(frame.rows)),
        frozen,
        "the gesture must have moved the leaf, or the restore proves nothing"
    );
    // The restore counter is read on the PTY side (bitty#3250 R10): the grid
    // above did move, but no `TIOCGWINSZ` exists here to move, so a counter
    // that reported the *intent* would be claiming a resize the kernel never
    // saw.
    assert_eq!(
        rt.content_restore_resizes(),
        0,
        "without a PTY winsize there is no restore resize to count"
    );
}

// ---------------------------------------------------------------------------
// Row 5: typed refusals change nothing
// ---------------------------------------------------------------------------

#[test]
fn present_refuses_without_a_host_without_a_panel_and_off_terminal_content() {
    // No panel host installed at all.
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    match rt.present_panel_content(PRIMARY, text_layer("none")) {
        Err(PanelError::ContentUnavailable { view_id, reason }) => {
            assert_eq!(view_id, PRIMARY);
            assert_eq!(reason, "no panel host installed");
        }
        other => panic!("expected ContentUnavailable, got {other:?}"),
    }

    // A host installed, but no panel mounted at this view. No live pane is
    // spawned: the refusal happens before any session is consulted, so this
    // row keeps its assertion on a build without a PTY backend (bitty#3250 R9).
    let mut rt = runtime();
    install(&mut rt, two_panes());
    install_panel_at(&mut rt, PANE);
    match rt.present_panel_content(PRIMARY, text_layer("none")) {
        Err(PanelError::ContentUnavailable { view_id, reason }) => {
            assert_eq!(view_id, PRIMARY);
            assert_eq!(reason, "no panel is mounted at this view");
        }
        other => panic!("expected ContentUnavailable, got {other:?}"),
    }

    // A panel mounted at a leaf with no terminal content owner: an inert leaf
    // that is neither a pane session nor the primary owner.
    let mut rt = runtime();
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    );
    install(&mut rt, layout);
    // No spawn: PANE is neither primary nor session-backed.
    install_panel_at(&mut rt, PANE);
    assert_eq!(rt.panel_content_kind(PANE), None);
    match rt.present_panel_content(PANE, text_layer("none")) {
        Err(PanelError::ContentUnavailable { view_id, reason }) => {
            assert_eq!(view_id, PANE);
            assert_eq!(reason, "no live terminal content owner");
        }
        other => panic!("expected ContentUnavailable, got {other:?}"),
    }
    assert_eq!(
        rt.panel_content_layer(PANE),
        None,
        "a refused present writes no record"
    );
}

#[test]
fn duplicate_present_release_without_layer_and_stale_handle_are_typed() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");

    let first = rt
        .present_panel_content(PRIMARY, text_layer("one"))
        .expect("first present is admitted");
    let kind = rt.panel_content_kind(PRIMARY);

    // Duplicate present on the same panel: nothing changes.
    match rt.present_panel_content(PRIMARY, text_layer("two")) {
        Err(PanelError::ContentBusy { panel_id }) => assert_eq!(panel_id, first.panel),
        other => panic!("expected ContentBusy, got {other:?}"),
    }
    assert_eq!(
        rt.panel_content_layer(PRIMARY).map(|layer| layer.token),
        Some(first.token),
        "the first layer survives the refusal"
    );
    assert_eq!(rt.panel_content_kind(PRIMARY), kind);

    // Release without a presented layer, on a panel that has none.
    let mut rt_empty = runtime();
    install(&mut rt_empty, single_leaf());
    install_panel_at(&mut rt_empty, PRIMARY);
    match rt_empty.release_panel_content(PRIMARY, first.token) {
        Err(PanelError::ContentNotPresented { .. }) => {}
        other => panic!("expected ContentNotPresented, got {other:?}"),
    }

    // A stale handle: the layer was released and a new one minted.
    rt.release_panel_content(PRIMARY, first.token)
        .expect("first release restores");
    let second = rt
        .present_panel_content(PRIMARY, text_layer("three"))
        .expect("a fresh present is admitted after release");
    assert_ne!(second.token, first.token, "tokens are never reused");
    match rt.release_panel_content(PRIMARY, first.token) {
        Err(PanelError::ContentTokenMismatch { panel_id, token }) => {
            assert_eq!(panel_id, second.panel);
            assert_eq!(token, first.token.get());
        }
        other => panic!("expected ContentTokenMismatch, got {other:?}"),
    }
    assert_eq!(
        rt.panel_content_layer(PRIMARY).map(|layer| layer.token),
        Some(second.token),
        "a stale handle cannot release the live layer"
    );

    // Releasing twice with the live handle: the second is typed, not a panic.
    rt.release_panel_content(PRIMARY, second.token)
        .expect("second release restores");
    match rt.release_panel_content(PRIMARY, second.token) {
        Err(PanelError::ContentNotPresented { .. }) => {}
        other => panic!("expected ContentNotPresented, got {other:?}"),
    }
}

#[test]
fn a_foreign_handle_cannot_release_another_panels_layer() {
    bitty_test_support::require_pty!();
    let mut rt = runtime();
    install(&mut rt, two_panes());
    spawn_pane(&mut rt);
    install_panel_at(&mut rt, PRIMARY);
    // Mount a second panel on the pane leaf, in the same host.
    {
        let host = rt.scene_host().expect("host installed");
        let mut host = host.try_borrow_mut().expect("host is free");
        let handle = host
            .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
            .expect("second terminal panel");
        host.mount_panel(handle.id, handle.generation, PANE)
            .expect("second panel mounts at the pane leaf");
    }

    let on_primary = rt
        .present_panel_content(PRIMARY, text_layer("primary"))
        .expect("primary layer presents");
    let on_pane = rt
        .present_panel_content(PANE, text_layer("pane"))
        .expect("pane layer presents");
    assert_ne!(on_primary.panel, on_pane.panel);
    assert_eq!(
        rt.scene_host()
            .expect("host")
            .try_borrow()
            .expect("host is free")
            .presented_content_count(),
        2
    );

    // The pane's handle must not release the primary's layer.
    let pane_winsize = rt.pane_pty_size(&PANE);
    match rt.release_panel_content(PRIMARY, on_pane.token) {
        Err(PanelError::ContentTokenMismatch { panel_id, token }) => {
            assert_eq!(panel_id, on_primary.panel);
            assert_eq!(token, on_pane.token.get());
        }
        other => panic!("expected ContentTokenMismatch, got {other:?}"),
    }
    assert_eq!(
        rt.panel_content_layer(PRIMARY).map(|layer| layer.token),
        Some(on_primary.token),
        "the foreign handle leaves the live layer presented"
    );
    assert_eq!(
        rt.pane_pty_size(&PANE),
        pane_winsize,
        "a refused release restores no geometry"
    );
    assert_eq!(rt.content_restore_resizes(), 0);

    // Each panel releases with its own handle.
    rt.release_panel_content(PRIMARY, on_primary.token)
        .expect("primary releases");
    rt.release_panel_content(PANE, on_pane.token)
        .expect("pane releases");
}

#[test]
fn a_panel_owned_scene_makes_the_content_slot_busy() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    // Another owner's content in the panel's scene slot: a `ui.rich` mount
    // paints there today. A content layer must never overwrite it, because the
    // release would then detach it.
    let host = rt.scene_host().expect("host installed");
    {
        let mut host = host.try_borrow_mut().expect("host is free");
        let panel = host
            .placement_panel_of(PRIMARY)
            .expect("panel mounted at the leaf");
        let generation = host
            .registry()
            .panel_allocation_generation(panel)
            .expect("live panel generation");
        host.attach_panel_scene(
            bitty_runtime::registry::PanelHandle {
                id: panel,
                generation,
            },
            text_layer("rich-mount"),
        )
        .expect("foreign scene attaches");
    }

    match rt.present_panel_content(PRIMARY, text_layer("layer")) {
        Err(PanelError::ContentBusy { .. }) => {}
        other => panic!("expected ContentBusy, got {other:?}"),
    }
    assert_eq!(rt.panel_content_layer(PRIMARY), None);
    let host = rt.scene_host().expect("host");
    let host = host.try_borrow().expect("host is free");
    let panel = host
        .placement_panel_of(PRIMARY)
        .expect("panel mounted at the leaf");
    assert!(
        host.panel_scene(panel).is_some(),
        "the refusal must leave the other owner's content attached"
    );
}

#[test]
fn disposing_the_panel_drops_its_layer_record() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("doomed"))
        .expect("present is admitted");

    let host = rt.scene_host().expect("host installed");
    {
        let mut host = host.try_borrow_mut().expect("host is free");
        let generation = host
            .registry()
            .panel_allocation_generation(presented.panel)
            .expect("live panel generation");
        host.dispose_panel(presented.panel, generation)
            .expect("panel disposes");
        assert_eq!(
            host.presented_content_count(),
            0,
            "a layer record never outlives its container"
        );
        assert!(host.panel_scene(presented.panel).is_none());
    }
    // The token is dead after the container is gone, and nothing restores.
    assert_eq!(rt.panel_content_layer(PRIMARY), None);
    assert!(rt.release_panel_content(PRIMARY, presented.token).is_err());
}

#[test]
fn release_reconciles_a_pixel_only_winsize_after_a_dpi_change() {
    bitty_test_support::require_pty!();
    // A durable fixed-size leaf comes back to the same `cols`/`rows` after a
    // font-zoom/DPI change, so a release that only compared the cell rect
    // would leave the PTY advertising stale pixels — the exact ioctl the
    // freeze deferred. The release has to reconcile the owner's own winsize.
    let mut rt = runtime();
    let mut view = View::new(PRIMARY, 80, 24);
    view.set_fixed_size(Some(bitty_ui::Size::new(40, 12)));
    install(&mut rt, LayoutNode::leaf(view));
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("primary shell spawns");
    install_panel_at(&mut rt, PRIMARY);
    // The spawn takes the container grid; one reflow puts the PTY on the fixed
    // leaf's flag, which is the size the layer freezes.
    rt.handle_resize(rt.surface_extent().expect("headless extent"))
        .expect("window resize applies");
    rt.tick().expect("baseline frame");
    let extent = rt.surface_extent().expect("headless extent");
    assert_eq!(
        rt.pty_size(),
        Some((40, 12)),
        "the fixed-size leaf pins the primary grid, not the container"
    );
    let frozen = rt.pty_pixel_size().expect("primary PTY pixel winsize");

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("fixed"))
        .expect("present is admitted");
    rt.apply_dpi_scale(1.5, Some(extent));
    assert_eq!(
        rt.pty_pixel_size(),
        Some(frozen),
        "the freeze defers the pixel-only ioctl while the layer is up"
    );
    assert_eq!(rt.content_restore_resizes(), 0);

    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the primary");
    assert_eq!(
        rt.pty_size(),
        Some((40, 12)),
        "the cell rect did not move, so this release owes no cell resize"
    );
    let restored = rt.pty_pixel_size().expect("primary PTY pixel winsize");
    assert_ne!(
        (restored.2, restored.3),
        (frozen.2, frozen.3),
        "release must reconcile the pixel axes the hidden gesture could not send"
    );
    assert_eq!(
        rt.content_restore_resizes(),
        1,
        "the pixel-only reconciliation is counted as the one restore resize"
    );
}

#[test]
fn a_pointer_press_dropped_by_the_gate_does_not_arm_a_drag() {
    // A press the content gate drops never reached the application, so it must
    // not latch a drag either: a `?1002` app reports motion only while a
    // button is held, which makes a phantom latch observable after the layer
    // is gone.
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    rt.handle_pty_bytes(b"\x1b[?1002h\x1b[?1006h");
    rt.handle_cursor_moved(CursorPosition { x: 22.0, y: 22.0 });
    rt.drain_pending_input();

    let presented = rt
        .present_panel_content(PRIMARY, text_layer("pointer"))
        .expect("present is admitted");
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Pressed));
    assert_eq!(
        rt.pending_input_len(),
        0,
        "a press report must not reach the hidden shell"
    );
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the terminal content");
    rt.drain_pending_input();

    rt.handle_cursor_moved(CursorPosition {
        x: 22.0 + 18.0,
        y: 22.0,
    });
    assert_eq!(
        rt.drain_pending_input(),
        Vec::<u8>::new(),
        "motion after the layer is gone must not report a drag the app never started"
    );
}

#[test]
fn detaching_a_scene_cannot_strand_a_presented_layer() {
    // bitty#3250 R5: the plugin unmount path detaches a panel's scene for any
    // owned panel. Behind a live content layer that would remove the paint half
    // and leave the record naming a `Rich` kind nothing can restore, so the
    // detach has to refuse while the layer is presented.
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("owned"))
        .expect("present is admitted");

    let host = rt.scene_host().expect("host installed");
    {
        let mut host = host.try_borrow_mut().expect("host is free");
        match host.detach_panel_scene(presented.panel) {
            Err(PanelError::ContentBusy { panel_id }) => assert_eq!(panel_id, presented.panel),
            other => panic!("a detach behind a live layer must refuse, got {other:?}"),
        }
        assert!(
            host.panel_scene(presented.panel).is_some(),
            "the refusal leaves the layer's scene attached"
        );
        assert_eq!(
            host.presented_content_count(),
            1,
            "the refusal leaves the record presented, never stranded"
        );
    }

    // Release detaches the layer's own scene; a later detach is then a no-op.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("release restores the terminal content");
    let host = rt.scene_host().expect("host installed");
    let mut host = host.try_borrow_mut().expect("host is free");
    assert!(
        !host
            .detach_panel_scene(presented.panel)
            .expect("a released panel detaches freely"),
        "release already detached the layer's scene"
    );
    assert_eq!(host.presented_content_count(), 0);
}

// ---------------------------------------------------------------------------
// Row 6: a presented layer cannot outlive the generation that presented it
// ---------------------------------------------------------------------------

/// The presenting generation's edge releases the layer through the panel host's
/// binding (bitty#3253).
///
/// Fails for the pre-fix tree four ways at once: the record survives the edge,
/// the layer's scene stays attached to a dead generation, [`Runtime`] keeps
/// reporting a `Rich` content kind and a frozen content owner, and a fresh
/// present on the same panel is refused with `ContentBusy` until the container
/// is retired. The assertions are the four observables that matter — the record
/// count, the scene slot, the content kind the input and geometry gates read,
/// and the token a second present mints.
#[test]
fn panel_content_layer_is_released_by_the_generation_edge() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("generation-owned"))
        .expect("present is admitted on the primary owner");
    // The presenting generation, in the controller domain the generation port
    // uses, bound to the container the layer lives on.
    let owner = bitty_plugin_host::PluginId::new("acme.dashboard").expect("valid plugin id");
    let ended = bitty_runtime::plugin_runtime::controller_generation(1);

    let host = rt.scene_host().expect("host installed");
    let restored = {
        let mut host = host.try_borrow_mut().expect("host is free");
        host.rebind_panel(
            presented.panel,
            ended,
            &owner,
            &std::collections::BTreeSet::new(),
            &std::collections::BTreeSet::new(),
        )
        .expect("the generation binds the container");
        let restored = host
            .release_content_layer_at_generation_end(presented.panel, &owner, ended)
            .expect("the ending generation releases its own layer")
            .expect("a layer was presented");
        assert_eq!(
            restored.content,
            ViewContent::Terminal(PRIMARY.0),
            "the record still names the terminal content it displaced"
        );
        assert_eq!(restored.token, presented.token);
        assert_eq!(
            host.presented_content_count(),
            0,
            "the record does not survive the generation that presented it"
        );
        assert!(
            host.panel_scene(presented.panel).is_none(),
            "the layer's scene goes with the record, so the detach above cannot refuse"
        );
        restored
    };
    assert_eq!(restored.view, PRIMARY);

    // The gates are predicates over the record, so the container is
    // terminal-backed again the moment the record is gone: no freeze, no input
    // ownership, and the ordinary paths resume without a release token.
    assert_eq!(
        rt.panel_content_kind(PRIMARY),
        Some(ViewContent::Terminal(PRIMARY.0))
    );
    assert!(!rt.content_layer_presented(PRIMARY));
    assert!(!rt.input_owner_content_frozen());
    let swallowed_before = rt.content_input_swallowed();
    assert_eq!(
        rt.handle_key_event(char_press('g')),
        Some(vec![b'g']),
        "the terminal takes its keys back once the layer's generation is gone"
    );
    assert_eq!(rt.drain_pending_input(), vec![b'g']);
    assert_eq!(
        rt.content_input_swallowed(),
        swallowed_before,
        "no key is swallowed for a layer that is no longer presented"
    );

    // The payoff: a later generation presents on the same container, which the
    // pre-fix tree refuses with `ContentBusy` until the panel is retired.
    let again = rt
        .present_panel_content(PRIMARY, text_layer("successor"))
        .expect("the panel presents again once the dead layer is released");
    assert!(
        again.token > presented.token,
        "a released token is never reused: {:?} then {:?}",
        presented.token,
        again.token
    );
}

/// The edge release is fenced on the binding, not on a token (bitty#3253).
///
/// A generation that does not bind the container cannot reach the record: the
/// successor inherits nothing merely by inheriting the container, and the
/// displaced generation loses its reach the moment the bind replaces it. The
/// live token path is untouched by the fence, which is what keeps the three
/// content refusals meaning what they meant before the edge existed.
#[test]
fn panel_content_layer_release_is_fenced_on_the_ending_generation() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    rt.tick().expect("baseline frame");
    let presented = rt
        .present_panel_content(PRIMARY, text_layer("fenced"))
        .expect("present is admitted on the primary owner");
    let owner = bitty_plugin_host::PluginId::new("acme.dashboard").expect("valid plugin id");
    let ended = bitty_runtime::plugin_runtime::controller_generation(1);
    let successor = bitty_runtime::plugin_runtime::controller_generation(2);

    let host = rt.scene_host().expect("host installed");
    {
        let mut host = host.try_borrow_mut().expect("host is free");
        host.rebind_panel(
            presented.panel,
            ended,
            &owner,
            &std::collections::BTreeSet::new(),
            &std::collections::BTreeSet::new(),
        )
        .expect("the ending generation binds the container");
        host.rebind_panel(
            presented.panel,
            successor,
            &owner,
            &std::collections::BTreeSet::new(),
            &std::collections::BTreeSet::new(),
        )
        .expect("a successor binds the same container");
        match host.release_content_layer_at_generation_end(presented.panel, &owner, ended) {
            Err(PanelError::ControllerConflict {
                panel_id,
                bound_generation,
                presented_generation,
            }) => {
                assert_eq!(panel_id, presented.panel);
                assert_eq!(bound_generation, Some(successor));
                assert_eq!(presented_generation, ended);
            }
            other => panic!("a displaced generation must be refused, got {other:?}"),
        }
        assert_eq!(
            host.presented_content_count(),
            1,
            "a refused edge release leaves the record presented"
        );
        assert!(
            host.panel_scene(presented.panel).is_some(),
            "a refused edge release leaves the layer's scene attached"
        );
    }

    // The token path still releases it, exactly once, with the meaning it had:
    // a live caller releases with the token it holds.
    rt.release_panel_content(PRIMARY, presented.token)
        .expect("the owner still releases through its own token");
    assert_eq!(rt.panel_content_layer(PRIMARY), None);
    match rt.release_panel_content(PRIMARY, presented.token) {
        Err(PanelError::ContentNotPresented { panel_id }) => assert_eq!(panel_id, presented.panel),
        other => panic!("a spent token must report ContentNotPresented, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Row 7: a close gives the displaced terminal back (CTX-1912)
// ---------------------------------------------------------------------------

/// A workspace close is a teardown of every panel the slot owns: the sessions
/// die in the close and the containers are disposed, so a layer still presented
/// at that moment is content covering a shell about to be killed.
///
/// The layer must be given back *before* that sweep, through the Core seam keyed
/// on panel identity (`Runtime::release_presented_panel_content`), so the
/// owner's row is pruned and its release is recorded as `closed` instead of the
/// layer being dropped with its container. The row reads the plugin row and the
/// remembered release rather than the presented record, on purpose: the
/// disposal would clear the presented record either way, so only the row and the
/// remembered reason prove the release actually ran.
#[test]
fn a_workspace_close_releases_the_layer_a_plugin_presented() {
    let mut rt = runtime();
    install(&mut rt, single_leaf());
    install_panel_at(&mut rt, PRIMARY);
    assert!(rt.set_focus(PRIMARY));
    rt.tick().expect("the primary frame");

    let owner = bitty_plugin_host::manifest::PluginId::new("xuepoo.widget").expect("plugin id");
    let node = bitty_lua::UiRichNode::Text {
        text: String::from("dashboard"),
        bold: false,
    };
    let handle = rt
        .ui_panel_present(&owner, 1, &bitty_lua::UiPanelSpec::new(), &node)
        .expect("the plugin presents on the focused panel");
    assert!(rt.content_layer_presented(PRIMARY));
    assert_eq!(rt.ui_panel_layer_count(), 1);

    rt.workspace_close_at(0).expect("the only workspace closes");

    assert!(
        !rt.content_layer_presented(PRIMARY),
        "the layer does not outlive the workspace that presented it"
    );
    assert_eq!(
        rt.ui_panel_layer_count(),
        0,
        "the close released the layer, so the owner's row is gone"
    );
    assert_eq!(
        rt.ui_panel_release_record(handle)
            .map(|record| record.reason.as_str()),
        Some("closed"),
        "the owner learns its layer was released because the panel closed"
    );
}
