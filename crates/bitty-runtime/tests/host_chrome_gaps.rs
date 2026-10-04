#![forbid(unsafe_code)]
//! W-104 host gaps C1-C3 (CTX-0946): Core-owned band click routing, paint
//! tokens + redraw gate, and exclusive-zone enforcement, pinned headlessly.
//!
//! - C1: a primary press on a band row is consumed as Core chrome (no focus
//!   move, no selection, no capture report) and the paired release resolves
//!   into the drain queue as the owning plugin's declared command; unclaimed
//!   spans and off-band clicks dispatch nothing; foreign-qualified commands
//!   are denied, never routed.
//! - C2: `fg`/`bg` theme tokens and `bold` change the paint; unknown tokens
//!   fall back with a diagnostic; geometry violations skip the whole band
//!   (fail-closed) and count a diagnostic; overflow clips to the band.
//! - C3: visible bands shrink the layout container through the normal reflow
//!   path (grids + PTY winsizes follow); hidden bands reserve nothing; tiny
//!   windows degrade to no bands; content-only updates damage without
//!   reflowing.
//!
//! Application dispatch (`PluginRuntime::dispatch_command`) is the app
//! layer's move over [`Runtime::drain_band_clicks`](bitty_runtime::Runtime);
//! the queue contents pinned here are its contract.

use bitty_lua::ui::{ClickArg, ClickCommand, UiNode, UiSlot};
use bitty_platform::{CursorPosition, MouseButton, PressState};
use bitty_runtime::{BandContent, BandEdge, ChromeBands, Runtime, UiRect, band_click_args_table};

fn minimal_runtime() -> Runtime {
    Runtime::with_defaults().expect("runtime must build")
}

fn text_band(plugin: &str, slot: UiSlot, root: UiNode) -> BandContent {
    BandContent {
        plugin_id: plugin.to_string(),
        slot,
        root,
        version: 1,
    }
}

fn plain(text: &str) -> UiNode {
    UiNode::Text {
        text: text.to_string(),
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
    }
}

fn pill(text: &str, id: f64) -> UiNode {
    UiNode::Text {
        text: text.to_string(),
        fg: None,
        bg: None,
        bold: None,
        on_click: Some(ClickCommand {
            command: "test-bar:focus".to_string(),
            args: vec![("id".to_string(), ClickArg::Number(id))],
        }),
    }
}

fn press() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

fn release() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released)
}

/// Physical center of window cell `(row, col)`.
fn cell_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn mount_bottom_bar(rt: &mut Runtime, root: UiNode) {
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![text_band("test-bar", UiSlot::Bottom, root)],
        ..Default::default()
    });
}

fn bar_row(rt: &Runtime) -> u16 {
    rt.plugin_band_row(BandEdge::Bottom, 0)
        .expect("bottom band fits")
}

/// Physical pixel at the center of leaf content cell `(row, col)`.
fn content_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let focused = rt.focused_view().expect("focused");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == focused)
        .expect("focused presented");
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y.max(0)) + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn primary_frame_rows(rt: &Runtime) -> u16 {
    let primary = rt.primary_view().expect("primary owner");
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .map(|frame| frame.rows)
        .expect("primary presented")
}

fn click_at(rt: &mut Runtime, row: u16, col: u16) {
    rt.handle_cursor_moved(cell_pixels(rt, row, col));
    rt.handle_mouse_input(press());
    rt.handle_mouse_input(release());
}

// ---------------------------------------------------------------------------
// C1: Core-owned click routing
// ---------------------------------------------------------------------------

#[test]
fn band_click_routes_own_claim_to_the_drain_queue() {
    let mut rt = minimal_runtime();
    let row = {
        mount_bottom_bar(
            &mut rt,
            UiNode::row(vec![plain("1:ws1 "), pill("2:ws2", 2.0)]),
        );
        rt.tick();
        bar_row(&rt)
    };
    let focused = rt.focused_view();
    // Column 7 sits on the `2:ws2` pill.
    click_at(&mut rt, row, 7);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "one claimed click routes once");
    assert_eq!(clicks[0].plugin_id, "test-bar");
    assert_eq!(clicks[0].command, "focus");
    assert_eq!(
        clicks[0].args,
        vec![("id".to_string(), ClickArg::Number(2.0))]
    );
    // Chrome owns the gesture: no focus move, no selection, no PTY bytes.
    assert_eq!(rt.focused_view(), focused, "band click moves no focus");
    assert!(!rt.has_selection(), "band click starts no selection");
    assert!(
        rt.drain_band_clicks().is_empty(),
        "drain consumes the queue"
    );
    let stats = rt.band_host_stats();
    assert_eq!(stats.clicks_routed, 1);
    assert_eq!(stats.clicks_denied, 0);
}

#[test]
fn band_click_outside_any_claim_dispatches_nothing() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(
        &mut rt,
        UiNode::row(vec![plain("1:ws1 "), pill("2:ws2", 2.0)]),
    );
    rt.tick();
    let row = bar_row(&rt);
    // Column 1 sits on the unclaimed `1:ws1 ` separator span.
    click_at(&mut rt, row, 1);
    assert!(
        rt.drain_band_clicks().is_empty(),
        "separator click routes nothing"
    );
    assert!(!rt.has_selection(), "unclaimed span still owns its row");
    assert_eq!(rt.band_host_stats().clicks_unclaimed, 1);
    // Past the text entirely: same fail-closed outcome.
    click_at(&mut rt, row, 70);
    assert!(rt.drain_band_clicks().is_empty());
}

#[test]
fn grid_click_falls_through_to_selection() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(&mut rt, UiNode::row(vec![pill("bar", 1.0)]));
    rt.tick();
    // Content cell (0, 0) is grid content: a press-drag there is terminal
    // input, never band chrome.
    rt.handle_cursor_moved(content_pixels(&rt, 0, 0));
    rt.handle_mouse_input(press());
    rt.handle_cursor_moved(content_pixels(&rt, 0, 5));
    rt.handle_mouse_input(release());
    assert!(
        rt.drain_band_clicks().is_empty(),
        "grid click routes no band command"
    );
    assert!(rt.has_selection(), "grid press-drag still selects");
}

#[test]
fn overlapping_foreign_claim_is_denied_never_routed() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(
        &mut rt,
        UiNode::row(vec![UiNode::Text {
            text: "x".to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: Some(ClickCommand {
                command: "other-plugin:focus".to_string(),
                args: Vec::new(),
            }),
        }]),
    );
    rt.tick();
    let row = bar_row(&rt);
    click_at(&mut rt, row, 0);
    // No cross-plugin routing: the foreign qualifier is denied loudly.
    assert!(rt.drain_band_clicks().is_empty());
    let stats = rt.band_host_stats();
    assert_eq!(stats.clicks_denied, 1);
    assert_eq!(stats.clicks_routed, 0);
}

#[test]
fn band_press_is_chrome_before_mouse_capture() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(&mut rt, UiNode::row(vec![pill("bar", 1.0)]));
    rt.tick();
    let row = bar_row(&rt);
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.drain_pending_input();
    let focused = rt.focused_view();
    click_at(&mut rt, row, 0);
    assert_eq!(rt.focused_view(), focused, "no focus move");
    assert!(rt.pending_input().is_empty(), "no report to the app");
    assert!(!rt.has_selection());
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "band release mints no hyperlink gesture"
    );
    // The click itself still routed (claim present).
    assert_eq!(rt.drain_band_clicks().len(), 1);
}

#[test]
fn shift_press_on_a_band_keeps_the_selection_escape() {
    use bitty_platform::{ModifiersState, PlatformEvent, WindowEventKind, WindowId};
    let mut rt = minimal_runtime();
    mount_bottom_bar(&mut rt, UiNode::row(vec![pill("bar", 1.0)]));
    rt.tick();
    let row = bar_row(&rt);
    rt.handle_cursor_moved(cell_pixels(&rt, row, 0));
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ModifiersChanged(ModifiersState {
            shift: true,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });
    rt.handle_mouse_input(press());
    assert!(
        !rt.band_release_armed(),
        "Shift forces the selection path (CTX-0181)"
    );
    rt.handle_mouse_input(release());
    assert!(rt.drain_band_clicks().is_empty());
}

#[test]
fn trailing_press_is_consumed_as_chrome_not_selection() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(
        &mut rt,
        UiNode::row(vec![plain("1:ws1 "), pill("2:ws2", 2.0)]),
    );
    rt.tick();
    let row = bar_row(&rt);
    // Press on the trailing region past the text (col 70 of an 11-cell
    // line), drag into grid content, release: the band row owns the whole
    // gesture, so no selection can start.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 70));
    rt.handle_mouse_input(press());
    assert!(
        rt.band_release_armed(),
        "trailing press arms the band swallow"
    );
    rt.handle_cursor_moved(content_pixels(&rt, 0, 5));
    rt.handle_mouse_input(release());
    assert!(
        rt.drain_band_clicks().is_empty(),
        "trailing release claims nothing"
    );
    assert!(!rt.has_selection(), "trailing press starts no selection");
    assert_eq!(rt.band_host_stats().clicks_unclaimed, 1);
    assert_eq!(rt.band_host_stats().clicks_routed, 0);
}

#[test]
fn trailing_press_sends_no_capture_report() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(
        &mut rt,
        UiNode::row(vec![plain("1:ws1 "), pill("2:ws2", 2.0)]),
    );
    rt.tick();
    let row = bar_row(&rt);
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.drain_pending_input();
    let focused = rt.focused_view();
    // Trailing press under mouse tracking: chrome consumes it before the
    // capture decision, so the app never sees a report.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 70));
    rt.handle_mouse_input(press());
    assert_eq!(rt.focused_view(), focused, "no focus move");
    assert!(rt.pending_input().is_empty(), "no report to the app");
    rt.handle_mouse_input(release());
    assert!(rt.drain_band_clicks().is_empty());
    assert!(rt.pending_input().is_empty(), "paired release swallowed");
}

#[test]
fn trailing_release_never_routes_the_final_span() {
    // Caution case: the column-to-char walk saturates at the last char, so
    // the width guard (not the walk) must own the past-text decision.
    let mut rt = minimal_runtime();
    mount_bottom_bar(&mut rt, UiNode::row(vec![pill("XY", 1.0)]));
    rt.tick();
    let row = bar_row(&rt);
    // Sanity: a release inside the pill still routes.
    click_at(&mut rt, row, 1);
    assert_eq!(rt.drain_band_clicks().len(), 1);
    // Past the 2-cell text: NoClaim, never the pill's owner.
    click_at(&mut rt, row, 40);
    assert!(rt.drain_band_clicks().is_empty());
    let stats = rt.band_host_stats();
    assert_eq!(stats.clicks_routed, 1);
    assert_eq!(stats.clicks_unclaimed, 1);
    assert_eq!(stats.clicks_denied, 0);
}

#[test]
fn press_drag_across_spans_fires_nothing() {
    // Standard button semantics: press on pill A, drag to pill B, release
    // must not fire B's command (the press never armed it).
    let mut rt = minimal_runtime();
    mount_bottom_bar(
        &mut rt,
        UiNode::row(vec![pill("A", 1.0), plain(" "), pill("B", 2.0)]),
    );
    rt.tick();
    let row = bar_row(&rt);
    rt.handle_cursor_moved(cell_pixels(&rt, row, 0));
    rt.handle_mouse_input(press());
    assert!(rt.band_release_armed(), "press on A arms the swallow");
    rt.handle_cursor_moved(cell_pixels(&rt, row, 2));
    rt.handle_mouse_input(release());
    assert!(
        rt.drain_band_clicks().is_empty(),
        "drag A->B routes neither command"
    );
    let stats = rt.band_host_stats();
    assert_eq!(stats.clicks_routed, 0);
    assert_eq!(stats.clicks_unclaimed, 1);
    assert!(!rt.has_selection(), "the whole gesture stays chrome");
}

#[test]
fn press_drag_across_bands_fires_nothing() {
    // Same across band rows: press on band 0's pill, release on band 1's
    // row — the release target differs, so nothing routes.
    let mut rt = minimal_runtime();
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![
            text_band(
                "test-bar",
                UiSlot::Bottom,
                UiNode::row(vec![pill("A", 1.0)]),
            ),
            text_band(
                "other-bar",
                UiSlot::Bottom,
                UiNode::row(vec![pill("B", 2.0)]),
            ),
        ],
        ..Default::default()
    });
    rt.tick();
    let first = rt
        .plugin_band_row(BandEdge::Bottom, 0)
        .expect("first band fits");
    let second = rt
        .plugin_band_row(BandEdge::Bottom, 1)
        .expect("second band fits");
    assert_ne!(first, second);
    rt.handle_cursor_moved(cell_pixels(&rt, first, 0));
    rt.handle_mouse_input(press());
    rt.handle_cursor_moved(cell_pixels(&rt, second, 0));
    rt.handle_mouse_input(release());
    assert!(
        rt.drain_band_clicks().is_empty(),
        "drag across bands routes nothing"
    );
    let stats = rt.band_host_stats();
    assert_eq!(stats.clicks_routed, 0);
    assert_eq!(stats.clicks_unclaimed, 1);
}

#[test]
fn click_args_encode_as_one_named_table() {
    let table = band_click_args_table(&[
        ("id".to_string(), ClickArg::Number(2.0)),
        ("name".to_string(), ClickArg::String("ws".to_string())),
    ]);
    let bitty_lua::host::LuaValue::Table(pairs) = table else {
        panic!("click args must encode as one table");
    };
    assert_eq!(pairs.len(), 2);
}

// ---------------------------------------------------------------------------
// C2: paint tokens + redraw gate
// ---------------------------------------------------------------------------

/// Pixels of the centre third of window cell row `row` after a tick.
fn window_row_pixels(rt: &Runtime, row: u16) -> Vec<u8> {
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let (_, ch) = rt.live_cell_size();
    let ch = ch as usize;
    let pad = usize::try_from(rt.window_padding_physical()).expect("usize");
    let stride = width * 4;
    let top = pad + usize::from(row) * ch + ch / 3;
    let bottom = (top + ch / 3).min(rgba.len() / stride.max(1));
    rgba[top * stride..bottom * stride].to_vec()
}

#[test]
fn accent_fg_and_bold_change_the_paint() {
    let styled = UiNode::row(vec![UiNode::Text {
        text: "2:ws2*".to_string(),
        fg: Some("accent".to_string()),
        bg: None,
        bold: Some(true),
        on_click: None,
    }]);
    let mut rt_styled = minimal_runtime();
    mount_bottom_bar(&mut rt_styled, styled);
    let stats_styled = rt_styled.tick().expect("styled band presents");
    assert!(stats_styled.glyphs > 0);

    let mut rt_plain = minimal_runtime();
    mount_bottom_bar(&mut rt_plain, plain("2:ws2*"));
    rt_plain.tick().expect("plain band presents");

    let row = bar_row(&rt_styled);
    assert_eq!(row, bar_row(&rt_plain));
    assert_ne!(
        window_row_pixels(&rt_styled, row),
        window_row_pixels(&rt_plain, row),
        "accent fg + bold must paint differently from the default"
    );
    assert_eq!(rt_styled.band_host_stats().unknown_tokens, 0);
    assert_eq!(rt_styled.band_host_stats().paint_violations, 0);
}

#[test]
fn unknown_tokens_fall_back_with_a_diagnostic() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(
        &mut rt,
        UiNode::row(vec![UiNode::Text {
            text: "hi".to_string(),
            fg: Some("not-a-token".to_string()),
            bg: Some("also-not-a-token".to_string()),
            bold: None,
            on_click: None,
        }]),
    );
    let stats = rt.tick().expect("unknown tokens still present");
    assert!(stats.glyphs > 0, "fallback paints the default pair");
    assert_eq!(rt.band_host_stats().unknown_tokens, 2);
    assert_eq!(rt.band_host_stats().paint_violations, 0);
}

#[test]
fn stacked_bands_never_share_a_row() {
    let mut rt = minimal_runtime();
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![
            text_band("a", UiSlot::Bottom, plain("A")),
            text_band("b", UiSlot::Statusline, plain("B")),
        ],
        ..Default::default()
    });
    rt.tick().expect("stacked bands present");
    let first = rt
        .plugin_band_row(BandEdge::Bottom, 0)
        .expect("first band fits");
    let second = rt
        .plugin_band_row(BandEdge::Bottom, 1)
        .expect("second band fits");
    assert_ne!(first, second, "stacked bands own distinct rows");
    assert!(!rt.band_geometry_violated());
    assert_eq!(rt.band_host_stats().paint_violations, 0);
}

#[test]
fn band_content_update_presents_without_reflowing() {
    let mut rt = minimal_runtime();
    mount_bottom_bar(&mut rt, plain("v1"));
    rt.tick().expect("first present");
    let container = rt.container();
    let height = rt.snapshot().height;
    // Same thickness (one visible band), new version: damage only.
    let mut bands = ChromeBands::default();
    let mut band = text_band("test-bar", UiSlot::Bottom, plain("v2"));
    band.version = 2;
    bands.bottom.push(band);
    rt.set_chrome_bands(bands);
    let stats = rt
        .tick()
        .expect("content update must present on a quiet grid");
    assert!(stats.glyphs > 0);
    assert_eq!(rt.container(), container, "content update reflows nothing");
    assert_eq!(rt.snapshot().height, height);
}

// ---------------------------------------------------------------------------
// C3: exclusive-zone enforcement
// ---------------------------------------------------------------------------

#[test]
fn visible_band_shrinks_the_container_and_grid() {
    let mut rt = minimal_runtime();
    let window = rt.window_cells();
    assert_eq!(rt.container(), window, "no bands: no reservation");
    let full_leaf_rows = primary_frame_rows(&rt);

    mount_bottom_bar(&mut rt, plain("bar"));
    rt.tick().expect("band tick");
    assert_eq!(
        rt.container(),
        UiRect::new(0, 0, window.width, window.height - 1),
        "container = window minus the band"
    );
    assert_eq!(
        primary_frame_rows(&rt),
        full_leaf_rows - 1,
        "leaf content rows lose exactly the band row"
    );
    let row = bar_row(&rt);
    assert_eq!(row, window.height - 1);
    // The band row sits outside the container, never over grid cells.
    assert!(row >= rt.container().y + rt.container().height);
    assert!(!rt.band_geometry_violated());
}

#[test]
fn hidden_band_reserves_nothing() {
    let mut rt = minimal_runtime();
    let window = rt.window_cells();
    mount_bottom_bar(&mut rt, plain(""));
    rt.tick();
    assert_eq!(rt.visible_band_count(BandEdge::Bottom), 0);
    assert_eq!(rt.container(), window, "hidden band takes no row");
    assert_eq!(rt.plugin_band_row(BandEdge::Bottom, 0), None);
    assert_eq!(rt.snapshot().height, usize::from(window.height));
}

#[test]
fn hidden_band_between_visible_bands_takes_no_row() {
    let mut rt = minimal_runtime();
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![
            text_band("a", UiSlot::Bottom, plain("A")),
            text_band("hidden", UiSlot::Bottom, plain("")),
            text_band("b", UiSlot::Bottom, plain("B")),
        ],
        ..Default::default()
    });
    rt.tick();
    assert_eq!(rt.visible_band_count(BandEdge::Bottom), 2);
    let first = rt.plugin_band_row(BandEdge::Bottom, 0).expect("row");
    let second = rt.plugin_band_row(BandEdge::Bottom, 1).expect("row");
    assert_eq!(second, first - 1, "hidden band collapses the stack");
    assert_eq!(
        rt.container().height,
        rt.window_cells().height - 2,
        "exactly the visible bands reserve rows"
    );
}

#[test]
fn tiny_window_degrades_to_no_bands() {
    use bitty_platform::PhysicalSize;
    let mut rt = minimal_runtime();
    mount_bottom_bar(&mut rt, plain("bar"));
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch + pad * 2))
        .expect("resize to one row");
    assert_eq!(rt.window_cells().height, 1);
    assert_eq!(
        rt.plugin_band_budget(),
        (0, 0),
        "no room: plugin bands degrade away"
    );
    assert_eq!(rt.container(), rt.window_cells());
    assert_eq!(rt.plugin_band_row(BandEdge::Bottom, 0), None);
    let _ = rt.tick();
    assert_eq!(rt.band_host_stats().paint_violations, 0);
}

// POSIX-only: spawns /bin/sh (same gate as `statusbar_row.rs`).
#[cfg(unix)]
#[test]
fn band_reservation_resizes_the_primary_pty_winsize() {
    let mut rt = minimal_runtime();
    rt.tick().expect("first full redraw");
    rt.spawn_shell("/bin/sh").expect("primary shell must spawn");
    let leaf_before = primary_frame_rows(&rt);
    mount_bottom_bar(&mut rt, plain("bar"));
    rt.tick().expect("band tick reflows");
    // The leaf loses exactly the band row through the normal reflow path.
    assert_eq!(primary_frame_rows(&rt), leaf_before - 1);
    let snap = rt.snapshot();
    assert_eq!(
        usize::from(leaf_before - 1),
        snap.height,
        "primary grid follows the reduced leaf"
    );
    let snap_cols = u16::try_from(snap.width).expect("u16");
    let snap_rows = u16::try_from(snap.height).expect("u16");
    assert_eq!(
        rt.pty_size(),
        Some((snap_cols, snap_rows)),
        "kernel winsize equals the band-reduced primary grid"
    );
    // Hiding the band gives the row back, like the Core bar.
    rt.set_chrome_bands(ChromeBands::default());
    rt.tick().expect("unmount tick reflows");
    assert_eq!(primary_frame_rows(&rt), leaf_before);
}
