//! Runtime Frame tick / present-path tests.
//!
//! Moved verbatim from the inline `runtime.rs` unit tests as part of
//! the CTX-0232 pure-move split. Adaptations are wiring only:
//! `super::*` became explicit imports and the private `layout` field
//! reads became the public `layout()` getter (identical semantics).
use bitty_platform::PlatformEvent;
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn mouse_headless_runtime(text: &str) -> Runtime {
    let mut rt = make_runtime();
    rt.force_headless_clipboard();
    rt.handle_pty_bytes(text.as_bytes());
    rt
}

#[test]
fn tick_is_idle_when_no_damage() {
    let mut rt = make_runtime();
    let first = rt.tick().expect("first tick must present full redraw");
    assert!(first.headless);
    assert_eq!(
        rt.tick(),
        None,
        "second tick with no new bytes must be idle"
    );
}

#[test]
fn handle_pty_bytes_flow_reaches_render() {
    let mut rt = make_runtime();
    assert!(rt.tick().is_some());
    rt.handle_pty_bytes(b"hello ");
    let stats = rt.tick().expect("damage from bytes must present");
    assert!(stats.glyphs > 0);
    assert_eq!(rt.tick(), None, "must return to idle after present");
}

#[test]
fn tick_cursor_overlay_uses_theme_cursor_hue() {
    // CTX-0219: the live cursor overlay paints the designed theme
    // cursor hue (rosewater) at the existing translucent alpha instead
    // of a hardcoded white, so the out-of-box cursor matches the
    // palette. Headless fills overwrite with premultiplied bytes:
    // cursor cell (col 1, row 0) after one printed cell, default live
    // cell 9x19 over the default 80x24 grid. CTX-0223: the window
    // padding inset (default 8px, physical 8px at scale 1.0) shifts
    // grid content by the inset inside the window surface (736x472 =
    // 720x456 grid plus 8px per side), so probe the padded origin.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"A");
    let stats = rt.tick().expect("damage from bytes must present");
    assert!(stats.headless);
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let cfg = RuntimeConfig::default();
    assert_eq!((cfg.cell_width, cfg.cell_height), (9, 19));
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    assert_eq!(pad, 8, "default padding inset is 8px at scale 1.0");
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    assert_eq!(width, 736, "window width is grid 720 plus 8px per side");
    let cx = pad + cfg.cell_width as usize + 4;
    let cy = pad + 9;
    let idx = (cy * width + cx) * 4;
    // Theme cursor #f5e0dc at 0xA0 alpha, premultiplied by the headless
    // composite: (245*160/255, 224*160/255, 220*160/255, 160).
    assert_eq!(
        &rgba[idx..idx + 4],
        &[153, 140, 138, 160],
        "cursor cell must carry the theme hue, not legacy white"
    );
    // Bridge agrees with the render default (single source of truth).
    assert_eq!(
        bitty_runtime::palette::theme_cursor_rgba(),
        bitty_render::grid::DEFAULT_CURSOR
    );
}

#[test]
fn cold_queue_is_bounded_and_observable() {
    let cfg = RuntimeConfig {
        cold_queue_capacity: 2,
        ..RuntimeConfig::default()
    };
    let mut rt = Runtime::new(cfg).expect("must build");
    rt.handle_pty_bytes(b"\x1b]0;first\x07");
    rt.handle_pty_bytes(b"\x1b]0;second\x07");
    rt.handle_pty_bytes(b"\x1b]0;third\x07");
    assert_eq!(rt.cold_queue_len(), 2);
    assert!(rt.cold_queue_dropped() > 0);
    let drained: Vec<_> = rt.drain_cold_events();
    assert_eq!(drained.len(), 2);
    assert_eq!(rt.cold_queue_len(), 0);
}

#[test]
fn focus_gain_and_resumed_force_full_redraw() {
    let mut rt = make_runtime();
    assert!(rt.tick().is_some(), "first tick presents");
    assert!(rt.tick().is_none(), "idle when no damage");
    rt.set_focused(false);
    assert!(rt.tick().is_none(), "focus loss alone stays idle");
    rt.set_focused(true);
    assert!(rt.tick().is_some(), "focus gain repaints");
    assert!(rt.tick().is_none(), "idle again after focus repaint");
    assert!(!rt.handle_platform_event(PlatformEvent::Resumed));
    assert!(rt.tick().is_some(), "resume repaints");
}

#[test]
fn tick_with_split_composites_both_leaves_headlessly() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt.set_layout(split);
    rt.handle_pty_bytes(b"hello");
    let stats = rt.tick().expect("split tick must present");
    assert!(stats.headless);
    assert!(stats.fills > 0);
    // Both leaves produce fills; total fills should be > single leaf.
    // Single leaf for 80x24 produces one fill per cell visited; split
    // produces per-leaf fills translated. So we expect fills roughly double
    // but at least more than one leaf's minimal.
    assert!(
        stats.fills >= 2,
        "split must composite at least two leaf fills"
    );
    let rgba = rt.headless_rgba().expect("rgba after split");
    assert!(!rgba.is_empty());
    // Deterministic: second runtime with same layout and bytes must be identical
    let mut rt2 = make_runtime();
    let split2 = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt2.set_layout(split2);
    rt2.handle_pty_bytes(b"hello");
    let stats2 = rt2.tick().expect("second split must present");
    let rgba2 = rt2.headless_rgba().expect("second rgba");
    assert_eq!(stats.fills, stats2.fills);
    assert_eq!(stats.glyphs, stats2.glyphs);
    assert_eq!(rgba, rgba2, "deterministic split composition");
}

#[test]
fn tick_with_stack_and_overlay_prove_composition() {
    let mut rt = make_runtime();
    // Stack: two children full size (second on top)
    let stack = LayoutNode::stack(vec![
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ]);
    rt.set_layout(stack);
    rt.handle_pty_bytes(b"stack");
    let stats = rt.tick().expect("stack tick must present");
    assert!(stats.headless);
    assert!(stats.fills > 0);
    let rgba_stack = rt.headless_rgba().expect("stack rgba").clone();

    // Overlay: base plus floating overlay
    let overlay = LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(10), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(20), 20, 10)),
        UiRect::new(5, 5, 20, 10),
    );
    let mut rt2 = make_runtime();
    rt2.set_layout(overlay);
    rt2.handle_pty_bytes(b"overlay");
    let stats2 = rt2.tick().expect("overlay tick must present");
    assert!(stats2.headless);
    let rgba_overlay = rt2.headless_rgba().expect("overlay rgba");
    assert_ne!(
        rgba_stack, rgba_overlay,
        "different compositions produce different pixels"
    );
    // Overlay must still be deterministic
    let mut rt3 = make_runtime();
    let overlay2 = LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(10), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(20), 20, 10)),
        UiRect::new(5, 5, 20, 10),
    );
    rt3.set_layout(overlay2);
    rt3.handle_pty_bytes(b"overlay");
    rt3.tick().expect("overlay replay");
    assert_eq!(rgba_overlay, rt3.headless_rgba().unwrap());
}

#[test]
fn tick_with_empty_stack_is_idle() {
    let mut rt = make_runtime();
    rt.set_layout(LayoutNode::stack(vec![]));
    assert_eq!(rt.leaf_count(), 0);
    assert_eq!(rt.focused_view(), None);
    // Even with pending full redraw, empty layout has no leaf to present
    assert_eq!(rt.tick(), None);
}

#[test]
fn wheel_fling_coalesces_to_one_present_then_idles() {
    // CTX-0185 profile evidence: wheel events only set
    // `pending_full_redraw`; an N-event fling without intermediate ticks
    // costs exactly one present, and the next tick idles (frame-on-demand
    // preserved — scroll adds no wakeups).
    let mut rt = make_runtime();
    for i in 0..60 {
        let line = format!("line {i:02}\n");
        rt.handle_pty_bytes(line.as_bytes());
    }
    for _ in 0..5 {
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
    }
    let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
    let offset = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .unwrap_or(usize::MAX);
    assert_eq!(
        offset,
        5 * RuntimeConfig::default().scroll_lines_per_notch as usize
    );
    assert!(
        rt.tick().is_some(),
        "fling must present exactly once on the next tick"
    );
    assert!(
        rt.tick().is_none(),
        "must idle after presenting (no scroll wakeups)"
    );
}

#[test]
fn selection_highlight_renders_end_to_end() {
    // Render-side primitive: one opaque row rect in the theme color.
    let cell = bitty_render::grid::CellMetrics::new(8, 16).expect("non-zero cell");
    let single = bitty_render::grid::selection_fill_rects((0, 0), (0, 4), 80, 24, cell);
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].color, bitty_render::grid::selection_fill());
    assert_eq!(single[0].rect.x, 0);
    assert_eq!(single[0].rect.y, 0);
    assert_eq!(single[0].rect.width, 5 * 8);
    assert_eq!(single[0].rect.height, 16);
    // Multi-row spans one rect per row, row-major.
    let multi = bitty_render::grid::selection_fill_rects((0, 1), (1, 1), 80, 24, cell);
    assert_eq!(multi.len(), 2);
    assert!(
        multi
            .iter()
            .all(|f| f.color == bitty_render::grid::selection_fill())
    );
    // Collapsed and empty grids paint nothing.
    assert!(bitty_render::grid::selection_fill_rects((2, 2), (2, 2), 80, 24, cell).is_empty());
    assert!(bitty_render::grid::selection_fill_rects((0, 0), (0, 4), 0, 24, cell).is_empty());

    // Runtime end-to-end: a committed selection forces the next tick to
    // present (selection-only changes bump the generation gate via
    // pending_full_redraw, otherwise the highlight would never paint).
    let mut rt = mouse_headless_runtime("hello world");
    assert!(rt.tick().is_some(), "first tick presents");
    assert_eq!(rt.tick(), None, "idle with no changes");
    rt.start_selection(bitty_ui::CellPos::new(0, 0));
    rt.end_selection(bitty_ui::CellPos::new(0, 4));
    let stats = rt.tick().expect("selection must force a present");
    assert!(stats.fills > 0);
    assert!(rt.headless_rgba().is_some());
}

// Ensures missing `allow` does not leak `dead_code` on the Window target.
#[cfg(target_os = "windows")]
#[test]
fn windows_build_still_compiles_with_queue_and_tick() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hi");
    let _ = rt.tick();
    assert!(rt.is_headless());
}
