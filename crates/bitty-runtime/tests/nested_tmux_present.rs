#![forbid(unsafe_code)]
//! CTX-0375 regression: a nested `tmux` status bar on the last grid row must
//! be painted by the live present path.
//!
//! Root cause (fixed here): `Runtime::reflow_to_grid` sized the primary
//! terminal grid and PTY to the *window* grid, while `Runtime::present_frames`
//! derives each leaf's *decorated content* grid (`gaps_out + border +
//! content_inset` px, CTX-0294/CTX-0333). `viewport_snapshot` then cropped the
//! grid to the smaller content frame, top-anchored by `cursor_follow_window_start`;
//! with the shell cursor near the top, the bottom rows — for example tmux's
//! status bar on the last row — never painted. The split path already followed
//! the owner leaf's content frame (CTX-0359); this pins the same invariant on
//! the DPI-adoption / window-resize path and proves the bottom row paints.
//!
//! Headless and deterministic: no window, adapter, or display server.

use bitty_platform::PhysicalSize;
use bitty_runtime::Runtime;
use bitty_runtime::RuntimeConfig;

/// Hyprland tiled physical extent from the live repro (scale 1.6).
const LIVE_EXTENT: PhysicalSize = PhysicalSize::new(2506, 1496);

/// tmux's startup shape: enter the alternate screen, reset scrolling, run the
/// capability queries it waits on (DA1 `CSI c`, DA2 `CSI > c`, XTVERSION
/// `CSI > q`, OSC 11 background), then clear. No reply is required for the
/// assertions below; this only mirrors the byte *shape* tmux emits first.
const TMUX_PREAMBLE: &[u8] = b"\x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\x1b[H\x1b[2J\
\x1b[c\x1b[>c\x1b[>q\x1b]11;?\x1b\\\x1b[?25l";

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

#[test]
fn primary_grid_matches_decorated_content_frame_after_dpi_adoption() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.apply_dpi_scale(1.6, Some(LIVE_EXTENT));
    let snap = rt.snapshot();
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 1, "single primary leaf");
    let frame = frames[0];
    // CTX-0269/CTX-0375 invariant: the primary grid is exactly the painted
    // content frame, so `viewport_snapshot` is the identity and no row or
    // column is cropped off the bottom/right.
    assert_eq!(
        (snap.width, snap.height),
        (usize::from(frame.cols), usize::from(frame.rows)),
        "primary grid must equal the decorated content frame"
    );
    // Sanity: the window grid is strictly larger (decoration insets it), so a
    // window-sized grid would have been cropped.
    assert!(usize::from(frame.cols) < rt.container().width as usize);
    assert!(usize::from(frame.rows) < rt.container().height as usize);
}

#[test]
fn tmux_status_bar_on_last_row_is_presented() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.apply_dpi_scale(1.6, Some(LIVE_EXTENT));
    let _ = rt.tick();
    let snap = rt.snapshot();
    let frame = rt.present_frames()[0];
    let (rows, cols) = (snap.height, snap.width);

    // Enter the alt screen like tmux, then paint its status bar on the last
    // grid row (`ESC[42m` green background, black text).
    rt.handle_pty_bytes(TMUX_PREAMBLE);
    let status = format!("\x1b[{rows};1H\x1b[30m\x1b[42m[0] 0:tmux*");
    rt.handle_pty_bytes(status.as_bytes());
    let stats = rt.tick().expect("alt-screen paint must present");
    assert!(stats.glyphs > 0, "status-bar glyphs must reach the frame");

    // The last row of the *state* grid must be inside the painted content
    // frame (pre-fix it was cropped by two window-grid rows).
    let first_painted_row = frame.rows as usize - 1; // only one leaf, full-height content
    assert_eq!(
        first_painted_row,
        rows - 1,
        "the frame covers the grid's last row"
    );

    // Prove pixels: scan the last content row band for any non-background
    // pixel (the green status band and its glyphs).
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let pad = usize::try_from(rt.window_padding_physical()).expect("usize");
    let content_x = usize::try_from(frame.content.x.max(0)).expect("usize");
    let content_y = usize::try_from(frame.content.y.max(0)).expect("usize");
    let cell_h = usize::try_from(frame.content.height / u32::from(frame.rows.max(1)))
        .expect("cell height fits usize")
        .max(1);
    let cell_w = usize::try_from(frame.content.width / u32::from(frame.cols.max(1)))
        .expect("cell width fits usize")
        .max(1);
    let row_y = pad + content_y + (rows - 1) * cell_h;
    let bg = bitty_render::grid::DEFAULT_BG;
    let mut found = false;
    'scan: for y in row_y..(row_y + cell_h).min(rgba.len() / (width * 4).max(1)) {
        for c in 0..cols {
            let x = pad + content_x + c * cell_w + cell_w / 2;
            if x >= width {
                break;
            }
            if probe(&rgba, width, x, y) != bg {
                found = true;
                break 'scan;
            }
        }
    }
    assert!(
        found,
        "the tmux status bar on the last grid row must be painted (not cropped)"
    );
}

#[test]
fn safe_decoration_also_matches_content_frame() {
    // `--safe` decoration (0/0/1/0) insets content by 1px per side; the grid
    // still follows the content frame instead of the window grid.
    let mut rt = Runtime::new(RuntimeConfig {
        decoration: bitty_runtime::Decoration::SAFE,
        ..RuntimeConfig::default()
    })
    .expect("safe runtime builds");
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("valid resize");
    let snap = rt.snapshot();
    let frame = rt.present_frames()[0];
    assert_eq!(
        (snap.width, snap.height),
        (usize::from(frame.cols), usize::from(frame.rows))
    );
}
