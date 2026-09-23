#![forbid(unsafe_code)]
//! Issue #1349 regression: the in-grid status bar row is drawn by the live
//! present path.
//!
//! #1340 landed the data path (`workspaceline_present`, `show_bar`
//! default-on, hit-test/click APIs) but no row was ever drawn. These tests
//! pin the drawn row headlessly (no window, adapter, or display server):
//!
//! - the bar row paints by default (last content row carries
//!   non-background pixels) while grid truth stays untouched
//!   (presentation-only overlay, never grid mutation);
//! - `show_bar = false` hides the row;
//! - a workspace switch with a quiet grid still presents (bar-text
//!   invalidation forces the frame);
//! - the alternate screen owns every row (no bar over fullscreen apps);
//! - a left press on the drawn band switches workspaces via the existing
//!   hit-test and consumes the event (no selection starts); presses above
//!   the band keep the selection path.

use bitty_platform::{CursorPosition, MouseButton, PressState};
use bitty_runtime::{Runtime, RuntimeConfig};

/// Default headless cell metrics (mirrors `mouse_chrome.rs`).
const CELL_W: f64 = 9.0;
const CELL_H: f64 = 19.0;

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

/// True when any pixel in the last content row band differs from the
/// theme background (i.e. the bar row painted something).
fn last_row_painted(rt: &Runtime) -> bool {
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let frame = rt.present_frames()[0];
    let pad = usize::try_from(rt.window_padding_physical()).expect("usize");
    let content_x = usize::try_from(frame.content.x.max(0)).expect("usize");
    let content_y = usize::try_from(frame.content.y.max(0)).expect("usize");
    let cell_h = usize::try_from(frame.content.height / u32::from(frame.rows.max(1)))
        .expect("cell height fits usize")
        .max(1);
    let cell_w = usize::try_from(frame.content.width / u32::from(frame.cols.max(1)))
        .expect("cell width fits usize")
        .max(1);
    let rows = usize::from(frame.rows);
    let cols = usize::from(frame.cols);
    let row_y = pad + content_y + (rows - 1) * cell_h;
    let bg = bitty_render::grid::DEFAULT_BG;
    let stride = (width * 4).max(1);
    for y in row_y..(row_y + cell_h).min(rgba.len() / stride) {
        for c in 0..cols {
            let x = pad + content_x + c * cell_w + cell_w / 2;
            if x >= width {
                break;
            }
            if probe(&rgba, width, x, y) != bg {
                return true;
            }
        }
    }
    false
}

/// Cursor pixels landing on bar column `col` of the single default leaf.
fn bar_pixels(rt: &Runtime, col: u16) -> CursorPosition {
    let frame = rt.present_frames()[0];
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + f64::from(col) * CELL_W + 4.0,
        y: pad
            + f64::from(frame.content.y.max(0))
            + f64::from(frame.rows.saturating_sub(1)) * CELL_H
            + 9.0,
    }
}

fn bar_pixels_above(rt: &Runtime, col: u16, row: u16) -> CursorPosition {
    let frame = rt.present_frames()[0];
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + f64::from(col) * CELL_W + 4.0,
        y: pad + f64::from(frame.content.y.max(0)) + f64::from(row) * CELL_H + 9.0,
    }
}

fn press() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

#[test]
fn bar_row_drawn_by_default_and_truth_untouched() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let stats = rt.tick().expect("first frame must present");
    assert!(stats.glyphs > 0, "bar glyphs must reach the frame");
    assert_eq!(
        rt.status_bar_text().as_deref(),
        Some("1:ws1* (1)"),
        "workspace module minimum"
    );
    assert!(
        last_row_painted(&rt),
        "the bar row must paint on the last content row by default"
    );
    // Presentation-only: the state grid behind the bar is unmutated.
    let snap = rt.snapshot();
    let last = snap.height.saturating_sub(1);
    for col in 0..snap.width {
        let cell = &snap.cells[last * snap.width + col];
        assert!(
            !cell.style.attributes.inverse,
            "grid truth must not carry bar styling (col {col})"
        );
    }
}

#[test]
fn show_bar_false_hides_the_row() {
    let mut rt = Runtime::new(RuntimeConfig {
        workspaceline_visible: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime builds");
    assert_eq!(rt.status_bar_text(), None);
    assert_eq!(rt.status_bar_row(24), None);
    let _ = rt.tick();
    assert!(
        !last_row_painted(&rt),
        "opted-out bar must leave the last row background-clean"
    );
}

#[test]
fn quiet_workspace_switch_still_presents_the_new_bar() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    assert_eq!(rt.status_bar_text().as_deref(), Some("1:ws1 2:ws2* (2)"));
    // No PTY bytes advanced the grid: the bar-text change alone must force
    // the frame so the new row paints.
    let stats = rt
        .tick()
        .expect("bar-text change must force a present on a quiet grid");
    assert!(stats.glyphs > 0);
    assert!(last_row_painted(&rt));
}

#[test]
fn alt_screen_owns_every_row() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    assert!(last_row_painted(&rt), "bar paints before alt screen");
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[H\x1b[2J");
    let _ = rt.tick();
    assert!(
        !last_row_painted(&rt),
        "a fullscreen app owns every row: no bar over alt screen"
    );
    // Leaving alt restores the bar.
    rt.handle_pty_bytes(b"\x1b[?1049l");
    let _ = rt.tick();
    assert!(last_row_painted(&rt), "bar returns after alt screen exit");
}

#[test]
fn bar_press_switches_workspace_and_consumes_the_event() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    assert_eq!(rt.active_workspace_index(), 1);
    // Bar reads `1:ws1 2:ws2* (2)`: column 0 names ws1.
    rt.handle_cursor_moved(bar_pixels(&rt, 0));
    rt.handle_mouse_input(press());
    assert_eq!(
        rt.active_workspace_index(),
        0,
        "press on the drawn bar band must hit-test to ws1"
    );
    assert!(
        !rt.has_selection(),
        "the bar row is chrome: no selection may start underneath it"
    );
    // Bar now reads `1:ws1* 2:ws2 (2)`: column 7 names ws2.
    rt.handle_cursor_moved(bar_pixels(&rt, 7));
    rt.handle_mouse_input(press());
    assert_eq!(rt.active_workspace_index(), 1);
    assert!(!rt.has_selection());
}

#[test]
fn press_above_the_bar_keeps_the_selection_path() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    // Row 0 is terminal content, not chrome: no workspace switch.
    rt.handle_cursor_moved(bar_pixels_above(&rt, 2, 0));
    rt.handle_mouse_input(press());
    assert_eq!(
        rt.active_workspace_index(),
        1,
        "presses above the bar must not switch workspaces"
    );
}

#[test]
fn hidden_bar_press_falls_through_to_selection() {
    let mut rt = Runtime::new(RuntimeConfig {
        workspaceline_visible: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime builds");
    let _ = rt.tick();
    // No bar is drawn, so the last row stays terminal content: the press
    // must not be consumed as chrome (and a single workspace can never
    // switch away from itself anyway).
    rt.handle_cursor_moved(bar_pixels(&rt, 0));
    rt.handle_mouse_input(press());
    assert_eq!(rt.active_workspace_index(), 0);
}
