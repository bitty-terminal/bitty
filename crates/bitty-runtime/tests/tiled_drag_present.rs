#![forbid(unsafe_code)]
//! Issue #2573 (CTX-1485): tiled-drag presentation is clean and cheap.
//!
//! The owner-visible symptoms were a heavy/laggy tiled drag and stale-looking
//! preview/terminal copies behind the pointer. The pre-fix present path armed
//! a *full* per-leaf invalidation on every pointer motion (and for the whole
//! drag ring fade), so every motion re-examined every visible cell and
//! rebuilt every pane even though the gesture is paint-only.
//!
//! These regressions pin the public behaviour end to end:
//!
//! - every presented drag frame moves exactly the ghost (bounded by the
//!   pointer delta), keeps solver geometry and PTY winsizes byte-identical,
//!   and paints no stale ink outside the live ghost frame;
//! - a motion yields one frame; a repeated tick without motion stays idle;
//! - the drag frame's renderer work stays bounded by a small constant instead
//!   of scaling with the visible cell area;
//! - the same invariants hold at DPI scale 2 with gaps/border/radius, and for
//!   the Alt+drag floating path;
//! - cancelling restores the pre-drag pixels byte-for-byte.
//!
//! The deterministic headless rasterizer keeps the pixel probes stable across
//! hosts; no timing assertion lives here (frame-time evidence is reported
//! separately, work is asserted through `PresentStats`).

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PhysicalSize, PressState};
use bitty_runtime::{
    AnimationKind, Decoration, LayoutNode, PresentFrame, PresentStats, Runtime, RuntimeConfig,
    SplitAxis, View, ViewId,
};

fn named_key(named: NamedKey, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn press() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

fn release() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released)
}

struct Harness {
    rt: Runtime,
    start: std::time::Instant,
}

impl Harness {
    fn new(scale: f64, decoration: Decoration, physical: (u32, u32)) -> Self {
        let mut config = RuntimeConfig {
            decoration,
            scrollbar_mode: bitty_ui::ScrollbarMode::Hidden,
            ..RuntimeConfig::default()
        };
        // Instant transitions keep the fixture time-independent: the pixels
        // must not depend on how much of a ring fade elapsed.
        config.animations.duration_ms = [0; AnimationKind::COUNT];
        let mut rt = Runtime::with_deterministic_rasterizer(config).expect("runtime builds");
        if scale != 1.0 {
            rt.apply_dpi_scale(scale, Some(PhysicalSize::new(physical.0, physical.1)));
        }
        rt.set_layout(three_pane());
        rt.handle_resize(PhysicalSize::new(physical.0, physical.1))
            .expect("resize applies");
        for row in 0..22u8 {
            let line = format!("drag-row-{row:02} ABCDEFGHIJKLMNOP\r\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        // Hide the caret so no blink phase can differ between two ticks.
        rt.handle_pty_bytes(b"\x1b[?25l");
        rt.tick().expect("settle present");
        Self {
            rt,
            start: std::time::Instant::now(),
        }
    }

    fn tick(&mut self, step: u64) -> Option<PresentStats> {
        self.rt
            .tick_at(self.start + std::time::Duration::from_millis(step * 16))
    }

    fn now(&self, step: u64) -> std::time::Instant {
        self.start + std::time::Duration::from_millis(step * 16)
    }

    fn frame(&self, view: ViewId) -> PresentFrame {
        self.rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == view)
            .unwrap_or_else(|| panic!("{view:?} must be presented"))
    }

    fn content_point(&self, view: ViewId, fx: f64, fy: f64) -> CursorPosition {
        let frame = self.frame(view);
        let pad = f64::from(self.rt.window_padding_physical());
        CursorPosition {
            x: pad + f64::from(frame.content.x) + f64::from(frame.content.width) * fx,
            y: pad + f64::from(frame.content.y) + f64::from(frame.content.height) * fy,
        }
    }

    fn rgba(&self) -> Vec<u8> {
        self.rt.headless_rgba().expect("headless frame")
    }

    fn width(&self) -> usize {
        self.rt.present_plan_extent().width as usize
    }
}

fn three_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        ),
    )
}

fn background(rgba: &[u8]) -> [u8; 4] {
    // The first pixel sits in the window-padding band: always clear color.
    [rgba[0], rgba[1], rgba[2], rgba[3]]
}

fn px(rgba: &[u8], width: usize, x: i32, y: i32) -> [u8; 4] {
    let index = (y as usize * width + x as usize) * 4;
    [
        rgba[index],
        rgba[index + 1],
        rgba[index + 2],
        rgba[index + 3],
    ]
}

/// Counts non-background pixels inside `rect`, excluding `skip`. The exclusion
/// keeps a translated ghost that still overlaps its old slot from counting as
/// stale; any leftover copy outside the live ghost frame is a real defect.
fn stale_ink(
    rgba: &[u8],
    width: usize,
    rect: (i32, i32, i32, i32),
    skip: (i32, i32, i32, i32),
    bg: [u8; 4],
) -> usize {
    let (x, y, w, h) = rect;
    let (sx, sy, sw, sh) = skip;
    let mut ink = 0;
    for yy in y.max(0)..(y + h) {
        for xx in x.max(0)..(x + w) {
            if xx >= sx && xx < sx + sw && yy >= sy && yy < sy + sh {
                continue;
            }
            if px(rgba, width, xx, yy) != bg {
                ink += 1;
            }
        }
    }
    ink
}

fn window_rect(rt: &Runtime, rect: bitty_render::geometry::RectPx) -> (i32, i32, i32, i32) {
    let pad = rt.window_padding_physical() as i32;
    (
        pad + rect.x,
        pad + rect.y,
        rect.width as i32,
        rect.height as i32,
    )
}

/// The runtime's bounded ghost translation for a raw pointer delta: the
/// dragged frame may never leave the container pixel area. Mirrors the
/// public contract (`tiled_drag_preview_frame` clamps at paint time) without
/// reaching into runtime internals.
fn clamped_delta(
    rt: &Runtime,
    frame: bitty_render::geometry::RectPx,
    raw: (i32, i32),
) -> (i32, i32) {
    let container = rt.container();
    let (cell_w, cell_h) = rt.live_cell_size();
    let (cell_w, cell_h) = (cell_w as i32, cell_h as i32);
    let area_x = container.x as i32 * cell_w;
    let area_y = container.y as i32 * cell_h;
    let area_w = container.width as i32 * cell_w;
    let area_h = container.height as i32 * cell_h;
    let min_dx = area_x - frame.x;
    let max_dx = (area_x + area_w - frame.width as i32 - frame.x).max(min_dx);
    let min_dy = area_y - frame.y;
    let max_dy = (area_y + area_h - frame.height as i32 - frame.y).max(min_dy);
    (raw.0.clamp(min_dx, max_dx), raw.1.clamp(min_dy, max_dy))
}

fn assert_solver_unchanged(rt: &Runtime, layout: &LayoutNode, frames: &[PresentFrame]) {
    assert_eq!(rt.layout(), layout, "drag must not mutate the solver tree");
    assert_eq!(
        rt.present_frames(),
        frames,
        "drag must not reflow solver frames (PTY stays put)"
    );
}

#[test]
fn drag_frames_translate_the_ghost_and_leave_no_stale_ink() {
    let mut h = Harness::new(1.0, Decoration::default(), (1200, 800));
    let source = ViewId::new(1);
    let target = ViewId::new(3);
    let before_layout = h.rt.layout().clone();
    let before_frames = h.rt.present_frames();
    let before_grab = h.rgba();
    let source_frame = h.frame(source);
    let old_content = source_frame.content;

    let grab = h.content_point(source, 0.5, 0.5);
    let end = h.content_point(target, 0.5, 0.5);
    h.rt.handle_cursor_moved(grab);
    h.rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    h.rt.handle_mouse_input(press());
    assert!(
        h.rt.tiled_drag_active(),
        "Mod+press must grab the tiled pane"
    );
    // The grab itself invalidates fully once; its work is the reference for
    // "a full pane render", not a per-motion cost.
    let grab_stats = h.tick(0).expect("grab presents");
    assert!(
        grab_stats.cells_examined > 1000,
        "grab frame must be a full leaf pass, got {} cells",
        grab_stats.cells_examined
    );

    let steps = 24u64;
    for step in 1..=steps {
        let f = step as f64 / steps as f64;
        let pos = CursorPosition {
            x: grab.x + (end.x - grab.x) * f,
            y: grab.y + (end.y - grab.y) * f,
        };
        h.rt.handle_cursor_moved(pos);
        let stats = h.tick(step).expect("every motion presents");
        assert!(
            stats.cells_examined <= 4,
            "drag motion must reuse retained leaves, got {} cells at step {step}",
            stats.cells_examined
        );
        // A repeated tick with no motion and no damage must stay idle: one
        // motion owns exactly one frame, so pointer storms cannot multiply
        // full-frame work.
        assert!(
            h.tick(step).is_none(),
            "an unchanged drag tick must not present again (step {step})"
        );

        let ghost = h.rt.tiled_drag_preview_frame().expect("ghost frame");
        let expected = clamped_delta(
            &h.rt,
            source_frame.frame,
            (
                (pos.x.round() as i32) - (grab.x.round() as i32),
                (pos.y.round() as i32) - (grab.y.round() as i32),
            ),
        );
        assert_eq!(
            h.rt.tiled_drag_ghost_offset(),
            Some(expected),
            "ghost follows the pointer one-to-one inside the container (step {step})"
        );
        assert_eq!(
            ghost.frame.width, source_frame.frame.width,
            "the ghost translates; it never resizes (step {step})"
        );
        assert_eq!(
            ghost.frame.height, source_frame.frame.height,
            "the ghost translates; it never resizes (step {step})"
        );
        assert_eq!(
            ghost.content.x,
            source_frame.content.x + expected.0,
            "ghost content keeps its inset and translation (step {step})"
        );
        assert_eq!(
            ghost.content.y,
            source_frame.content.y + expected.1,
            "ghost content keeps its inset and translation (step {step})"
        );
        // PTY winsizes are synced only by the layout install funnel
        // (`set_layout`), so an unchanged tree + frame list proves no
        // per-motion reflow or resize signal.
        assert_solver_unchanged(&h.rt, &before_layout, &before_frames);

        let painted = h.rt.paint_frames_at(h.now(step));
        assert_eq!(
            painted.last().map(|frame| frame.view),
            Some(source),
            "the ghost paints above the untouched tiled layout (step {step})"
        );
        let painted_ghost = painted
            .iter()
            .find(|frame| frame.view == source)
            .expect("painted ghost");
        assert_eq!(painted_ghost.frame, ghost.frame);
        assert_eq!(painted_ghost.content, ghost.content);
        for frame in &painted[..painted.len() - 1] {
            let baseline = before_frames
                .iter()
                .find(|candidate| candidate.view == frame.view)
                .expect("sibling existed before the drag");
            assert_eq!(
                frame, baseline,
                "every non-dragged pane keeps its committed frame (step {step})"
            );
        }

        let rgba = h.rgba();
        let width = h.width();
        let bg = background(&rgba);
        let ghost_window = window_rect(&h.rt, ghost.frame);
        let stale = stale_ink(
            &rgba,
            width,
            window_rect(&h.rt, old_content),
            ghost_window,
            bg,
        );
        assert_eq!(
            stale, 0,
            "old preview/content rect must be fully repainted (step {step})"
        );
        assert!(
            stale_ink(
                &rgba,
                width,
                window_rect(&h.rt, ghost.content),
                (0, 0, 0, 0),
                bg
            ) > 0,
            "the live ghost content must actually paint (step {step})"
        );
        // The background clear plus recomposite must also clear the previous
        // ghost frame: no ring/border copy may remain at the old slot.
        let old_frame = window_rect(&h.rt, source_frame.frame);
        assert_eq!(
            stale_ink(&rgba, width, old_frame, ghost_window, bg),
            0,
            "no stale ghost or border remains at the source slot (step {step})"
        );
    }

    h.rt.handle_mouse_input(release());
    h.rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    assert!(!h.rt.tiled_drag_active(), "release ends the drag");
    h.tick(steps + 1).expect("commit presents");
    assert!(
        h.rt.tiled_drag_preview_frame().is_none(),
        "no ghost survives the release"
    );
    assert_ne!(
        h.rgba(),
        before_grab,
        "release commits the drop, so the frame is not the pre-drag frame"
    );
}

#[test]
fn drag_cancel_restores_the_pre_drag_pixels_byte_exactly() {
    let mut h = Harness::new(1.0, Decoration::default(), (1200, 800));
    let source = ViewId::new(1);
    let before_layout = h.rt.layout().clone();
    let before_frames = h.rt.present_frames();
    let before = h.rgba();

    let grab = h.content_point(source, 0.5, 0.5);
    let moved = h.content_point(ViewId::new(2), 0.5, 0.5);
    h.rt.handle_cursor_moved(grab);
    h.rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    h.rt.handle_mouse_input(press());
    h.tick(0).expect("grab presents");
    h.rt.handle_cursor_moved(moved);
    h.tick(1).expect("motion presents");
    assert!(h.rt.tiled_drag_active());

    // Esc cancels: the tree was never mutated, so the next frame must be the
    // pre-drag frame byte-for-byte (ghost, target ring, and cursor cleaned).
    h.rt.handle_key_event(named_key(NamedKey::Escape, PressState::Pressed));
    assert!(!h.rt.tiled_drag_active(), "Esc must cancel the drag");
    h.tick(2).expect("cancel presents");
    assert_solver_unchanged(&h.rt, &before_layout, &before_frames);
    assert_eq!(
        h.rgba(),
        before,
        "cancelling must repaint the pre-drag frame exactly, with no ghost"
    );
}

#[test]
fn scaled_gapped_tiled_drag_keeps_physical_geometry_and_clean_pixels() {
    let decoration = Decoration::new(6, 10, 2, 8, 4);
    let mut h = Harness::new(2.0, decoration, (2400, 1600));
    let source = ViewId::new(1);
    let target = ViewId::new(2);
    let before_layout = h.rt.layout().clone();
    let before_frames = h.rt.present_frames();
    let source_frame = h.frame(source);
    let old_frame = window_rect(&h.rt, source_frame.frame);
    let old_content = window_rect(&h.rt, source_frame.content);

    let grab = h.content_point(source, 0.4, 0.4);
    let end = h.content_point(target, 0.5, 0.5);
    h.rt.handle_cursor_moved(grab);
    h.rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    h.rt.handle_mouse_input(press());
    h.tick(0).expect("grab presents");

    for step in 1..=12u64 {
        let f = step as f64 / 12.0;
        let pos = CursorPosition {
            x: grab.x + (end.x - grab.x) * f,
            y: grab.y + (end.y - grab.y) * f,
        };
        h.rt.handle_cursor_moved(pos);
        let stats = h.tick(step).expect("motion presents");
        assert!(
            stats.cells_examined <= 4,
            "DPI-scaled drag must reuse retained leaves, got {}",
            stats.cells_examined
        );
        let ghost = h.rt.tiled_drag_preview_frame().expect("ghost");
        let expected = clamped_delta(
            &h.rt,
            source_frame.frame,
            (
                (pos.x.round() as i32) - (grab.x.round() as i32),
                (pos.y.round() as i32) - (grab.y.round() as i32),
            ),
        );
        assert_eq!(h.rt.tiled_drag_ghost_offset(), Some(expected));
        assert_eq!(ghost.frame.width, source_frame.frame.width);
        assert_eq!(
            ghost.content.x - ghost.frame.x,
            source_frame.content.x - source_frame.frame.x,
            "scale 2 keeps the decoration inset in physical pixels"
        );
        assert_eq!(
            ghost.content.y - ghost.frame.y,
            source_frame.content.y - source_frame.frame.y
        );
        assert_solver_unchanged(&h.rt, &before_layout, &before_frames);

        let rgba = h.rgba();
        let width = h.width();
        let bg = background(&rgba);
        let ghost_frame = window_rect(&h.rt, ghost.frame);
        // With gaps the old frame band is background too; subtract only the
        // live ghost. Nothing else can legitimately paint in the old slot.
        assert_eq!(
            stale_ink(&rgba, width, old_frame, ghost_frame, bg),
            0,
            "scaled drag leaves no stale frame at the old slot (step {step})"
        );
        assert_eq!(
            stale_ink(&rgba, width, old_content, ghost_frame, bg),
            0,
            "scaled drag leaves no stale content at the old slot (step {step})"
        );
    }
    h.rt.handle_mouse_input(release());
    h.rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    h.tick(13).expect("commit presents");
}

#[test]
fn floating_drag_translates_the_float_without_stale_copies() {
    let mut config = RuntimeConfig {
        scrollbar_mode: bitty_ui::ScrollbarMode::Hidden,
        ..RuntimeConfig::default()
    };
    config.animations.duration_ms = [0; AnimationKind::COUNT];
    let mut rt = Runtime::with_deterministic_rasterizer(config).expect("runtime builds");
    let base = LayoutNode::leaf(View::new(ViewId::new(1), 80, 24));
    let float = LayoutNode::leaf(View::new(ViewId::new(2), 30, 10));
    rt.set_layout(LayoutNode::overlay(
        base,
        float,
        bitty_runtime::UiRect::new(20, 8, 30, 10),
    ));
    rt.handle_resize(PhysicalSize::new(1200, 800))
        .expect("resize");
    rt.handle_pty_bytes(b"float-body-text");
    rt.handle_pty_bytes(b"\x1b[?25l");
    rt.tick().expect("settle present");

    let float_id = ViewId::new(2);
    let before_layout = rt.layout().clone();
    let before_frames = rt.present_frames();
    let float_frame = *before_frames
        .iter()
        .find(|frame| frame.view == float_id)
        .expect("float frame");
    let old_content = window_rect(&rt, float_frame.content);
    let old_frame = window_rect(&rt, float_frame.frame);
    let pty_before = rt.pane_pty_pixel_size(&float_id);

    let grab = CursorPosition {
        x: f64::from(rt.window_padding_physical())
            + f64::from(float_frame.content.x)
            + f64::from(float_frame.content.width) / 2.0,
        y: f64::from(rt.window_padding_physical())
            + f64::from(float_frame.content.y)
            + f64::from(float_frame.content.height) / 2.0,
    };
    rt.handle_cursor_moved(grab);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press());
    assert!(rt.alt_drag_active(), "Alt+press must grab the float");
    rt.tick().expect("grab presents");
    let moved = CursorPosition {
        x: grab.x + 9.0 * 6.0,
        y: grab.y + 19.0 * 3.0,
    };
    rt.handle_cursor_moved(moved);
    let stats = rt.tick().expect("float motion presents");
    assert!(stats.cells_examined > 0, "the moved float must repaint");
    let moved_frame = rt
        .paint_frames_at(std::time::Instant::now())
        .into_iter()
        .find(|frame| frame.view == float_id)
        .expect("painted float");
    assert_eq!(moved_frame.frame.width, float_frame.frame.width);
    assert_eq!(moved_frame.frame.height, float_frame.frame.height);
    let rgba = rt.headless_rgba().expect("headless frame");
    let width = rt.present_plan_extent().width as usize;
    let ghost_window = window_rect(&rt, moved_frame.frame);
    assert_eq!(
        stale_ink(&rgba, width, old_content, ghost_window, background(&rgba)),
        0,
        "the float's old content slot must be repainted"
    );
    assert_eq!(
        stale_ink(&rgba, width, old_frame, ghost_window, background(&rgba)),
        0,
        "the float's old frame must be repainted"
    );
    assert_eq!(
        rt.pane_pty_pixel_size(&float_id),
        pty_before,
        "a float move must never resize its PTY"
    );
    // An overlay move commits the float's live bounds (cell-quantized) but
    // never reflows the tiled base or resizes any PTY.
    match (rt.layout(), &before_layout) {
        (
            LayoutNode::Overlay {
                base,
                bounds: after_bounds,
                ..
            },
            LayoutNode::Overlay {
                base: base_before,
                bounds: before_bounds,
                ..
            },
        ) => {
            assert_eq!(
                base.as_ref().find_leaf(ViewId::new(1)),
                base_before.as_ref().find_leaf(ViewId::new(1)),
                "the tiled base keeps its committed allocation"
            );
            assert_eq!(after_bounds.x, before_bounds.x + 6);
            assert_eq!(after_bounds.y, before_bounds.y + 3);
        }
        other => panic!("expected an overlay tree, got {other:?}"),
    }
    assert_eq!(
        rt.present_frames()
            .into_iter()
            .find(|frame| frame.view == ViewId::new(1)),
        before_frames
            .into_iter()
            .find(|frame| frame.view == ViewId::new(1)),
        "the base tile keeps its committed frame"
    );
}

#[test]
fn drag_text_content_is_preserved_while_reusing_retained_primitives() {
    // A busy source pane must re-render only itself on damage while the drag
    // ghost is live; untouched siblings stay retained. This also proves the
    // translation path keeps new terminal output visible at the pointer.
    let mut h = Harness::new(1.0, Decoration::default(), (1200, 800));
    let source = ViewId::new(1);
    let grab = h.content_point(source, 0.5, 0.5);
    h.rt.handle_cursor_moved(grab);
    h.rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    h.rt.handle_mouse_input(press());
    let grab_stats = h.tick(0).expect("grab presents");

    let moved = CursorPosition {
        x: grab.x + 220.0,
        y: grab.y + 140.0,
    };
    h.rt.handle_cursor_moved(moved);
    h.tick(1).expect("motion presents");
    // New output on the dragged pane while the ghost is translated.
    h.rt.handle_pty_bytes(b"\x1b[1;1HFRESH");
    let stats = h.tick(2).expect("damage presents");
    assert!(
        stats.cells_examined > 100,
        "damaged source must re-render its own grid, got {}",
        stats.cells_examined
    );
    assert!(
        stats.cells_examined < grab_stats.cells_examined,
        "untouched siblings must stay retained: damaged {} vs full grab {}",
        stats.cells_examined,
        grab_stats.cells_examined
    );
    let ghost = h.rt.tiled_drag_preview_frame().expect("ghost");
    let rgba = h.rgba();
    let width = h.width();
    let bg = background(&rgba);
    let stale = stale_ink(
        &rgba,
        width,
        window_rect(&h.rt, ghost.content),
        (0, 0, 0, 0),
        bg,
    );
    assert!(stale > 0, "the translated ghost must paint its content");
}
