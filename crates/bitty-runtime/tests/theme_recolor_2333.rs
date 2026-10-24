#![forbid(unsafe_code)]
//! Theme recolor acceptance (CTX-1395, issue #2333).
//!
//! The owner report: changing `appearance.theme` must recolor the terminal
//! text, not only the chrome, and a custom `appearance.colors` palette must be
//! first-class. These tests drive the production headless path
//! (`RuntimeConfig.theme` -> `GridRenderer`/`Surface` -> `tick` ->
//! `headless_rgba`) and assert exact pixels for every role a theme owns:
//!
//! - default foreground and background,
//! - ANSI 0–15 as foreground (`SGR 38;5;N`) and background (`SGR 48;5;N`),
//! - cursor overlay,
//! - selection background (selected glyphs keep their own foreground),
//! - scrollback cells after paging into history,
//! - all of the above after a live switch (`set_theme_palette` and
//!   `apply_theme_request`).
//!
//! The strongest assertion is frame equality: identical content rendered
//! under theme B in one runtime must be byte-identical to the frame a runtime
//! that rendered A produces after switching to B. A single stale cell,
//! selection rect, cursor overlay, or retained leaf keeps A and fails.
//!
//! Issue #2340 follow-up: frame equality alone lets a chrome surface pass on
//! both sides of the comparison while every production call site reads the
//! wrong color. The pending/close-confirm banner, help panel, and IME preedit
//! tests therefore also probe the exact pixels of their theme-derived fills
//! after a live switch (`dracula` -> `github-light`), so reverting one fill to
//! a fixed constant fails the test that owns that surface.

use std::time::{Duration, Instant};

use bitty_config::theme::{CustomPalette, resolve_theme};
use bitty_render::grid::{HELP_PANEL_BG, HELP_PANEL_BORDER, PENDING_PASTE_BANNER_BG};
use bitty_runtime::plugin_runtime::QueuedThemeRequest;
use bitty_runtime::{AnimationPolicy, CloseConfirmMode, Runtime, RuntimeConfig, ThemePalette};
use bitty_ui::{CellPos, Selection};

const COLS: usize = 80;
const ROWS: usize = 24;
const ANSI_FG_ROW: u16 = 1;
const ANSI_BG_ROW: u16 = 3;
const PLAIN_ROW: u16 = 5;
const SELECTION_ROW: u16 = 7;
const CURSOR_ROW: u16 = 9;
const CURSOR_COL: u16 = 60;

fn runtime(theme: ThemePalette) -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        cols: COLS,
        rows: ROWS,
        theme,
        theme_resolved: true,
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("deterministic headless runtime")
}

/// Runtime with the close-confirm gate forced to `Always`, so a window-close
/// request arms the banner without a foreground job.
fn close_confirm_runtime(theme: ThemePalette) -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        cols: COLS,
        rows: ROWS,
        theme,
        theme_resolved: true,
        close_confirm: CloseConfirmMode::Always,
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("deterministic headless runtime")
}

fn dracula() -> ThemePalette {
    ThemePalette::from_theme(resolve_theme(Some("dracula")))
}

fn github_light() -> ThemePalette {
    ThemePalette::from_theme(resolve_theme(Some("github-light")))
}

fn custom() -> CustomPalette {
    let ansi: Vec<String> = (0..16)
        .map(|index| {
            // Distinct, deliberately non-preset values so a preset path cannot
            // accidentally satisfy the custom assertions.
            format!(
                "#{:02X}{:02X}{:02X}",
                0x10 + index,
                0x40 + index,
                0x80 + index
            )
        })
        .collect();
    CustomPalette::try_from_hex(
        Some("#0B1020"),
        Some("#E6E6FA"),
        Some("#FF2E88"),
        Some("#24304A"),
        Some(&ansi),
    )
    .expect("complete custom palette")
}

/// Feeds the role fixture: plain/ANSI foreground blocks, ANSI background
/// cells, selectable blank cells, and a parked cursor outside every probe.
fn feed_roles(rt: &mut Runtime) {
    let mut bytes = Vec::new();
    // Row 0: default-foreground block at col 0 (full cell coverage).
    bytes.extend_from_slice("\x1b[1;1H█".as_bytes());
    // Row 1: one block per ANSI index 0..15 in the foreground.
    bytes.extend_from_slice(format!("\x1b[{};1H", ANSI_FG_ROW + 1).as_bytes());
    for index in 0..16 {
        bytes.extend_from_slice(format!("\x1b[38;5;{index}m█").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[0m");
    // Row 3: one space per ANSI index 0..15 as the cell background.
    bytes.extend_from_slice(format!("\x1b[{};1H", ANSI_BG_ROW + 1).as_bytes());
    for index in 0..16 {
        bytes.extend_from_slice(format!("\x1b[48;5;{index}m ").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[0m");
    // Row 5: ordinary text in the default foreground.
    bytes.extend_from_slice(format!("\x1b[{};1Hplain theme text", PLAIN_ROW + 1).as_bytes());
    // Park the cursor on a blank cell far from every probe.
    bytes.extend_from_slice(format!("\x1b[{};{}H", CURSOR_ROW + 1, CURSOR_COL + 1).as_bytes());
    rt.handle_pty_bytes(&bytes);
    rt.set_selection(Selection::simple(
        CellPos::new(SELECTION_ROW, 2),
        CellPos::new(SELECTION_ROW, 8),
    ));
    assert!(rt.has_selection(), "selection overlay must be live");
}

/// Feeds more lines than the viewport and pages one screen into history.
fn feed_scrollback(rt: &mut Runtime) {
    let mut bytes = Vec::new();
    for line in 0..(ROWS * 2 + 12) {
        let index = line % 16;
        bytes.extend_from_slice(format!("\x1b[38;5;{index}m█\x1b[0m line {line}\r\n").as_bytes());
    }
    rt.handle_pty_bytes(&bytes);
    assert!(rt.scrollback_len() >= ROWS, "fixture must fill scrollback");
    assert!(rt.scroll_focused_page(true), "page into history");
}

fn frame(rt: &mut Runtime) -> Vec<u8> {
    rt.tick().expect("themed frame presents");
    rt.headless_rgba().expect("headless rgba").to_vec()
}

struct Probe {
    width: usize,
    height: usize,
    rows: u16,
    cols: u16,
    content_x: i32,
    content_y: i32,
    cell_w: usize,
    cell_h: usize,
}

impl Probe {
    fn new(rt: &Runtime) -> Self {
        let frame = rt.present_frames().into_iter().next().expect("one leaf");
        let extent = rt.surface_extent().expect("surface extent");
        assert!(frame.cols > 0 && frame.rows > 0, "non-empty content frame");
        // `present_frames` is window-container relative; the present path
        // translates it by the window padding inset (`pad_px`).
        let pad = rt.config().window_padding as i32;
        Self {
            width: extent.width() as usize,
            height: extent.height() as usize,
            rows: frame.rows,
            cols: frame.cols,
            content_x: frame.content.x + pad,
            content_y: frame.content.y + pad,
            cell_w: frame.content.width as usize / usize::from(frame.cols),
            cell_h: frame.content.height as usize / usize::from(frame.rows),
        }
    }

    /// Pixel at window-relative coordinates.
    fn pixel_at(&self, rgba: &[u8], x: i32, y: i32) -> [u8; 4] {
        assert!(
            x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height,
            "probe ({x},{y}) outside {}x{}",
            self.width,
            self.height
        );
        let offset = (y as usize * self.width + x as usize) * 4;
        rgba[offset..offset + 4].try_into().expect("four bytes")
    }

    fn pixel(&self, rgba: &[u8], row: u16, col: u16) -> [u8; 4] {
        let x = self.content_x + i32::from(col) * self.cell_w as i32 + self.cell_w as i32 / 2;
        let y = self.content_y + i32::from(row) * self.cell_h as i32 + self.cell_h as i32 / 2;
        self.pixel_at(rgba, x, y)
    }

    /// Bounding box of every pixel equal to `color` (inclusive corners), or
    /// `None`. Used to locate the opaque help-panel ring without duplicating
    /// the panel layout math in the test.
    fn color_bounds(&self, rgba: &[u8], color: [u8; 4]) -> Option<(i32, i32, i32, i32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for y in 0..self.height as i32 {
            for x in 0..self.width as i32 {
                if self.pixel_at(rgba, x, y) == color {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x1 >= x0 && y1 >= y0).then_some((x0, y0, x1, y1))
    }

    /// Probes the bottom-right confirmation pill painted for `banner`.
    ///
    /// The pill is exactly as wide as the (right-aligned) banner text and one
    /// cell high on the focused view's last content row; the probe sits one
    /// pixel inside its top-left corner, above any glyph ink.
    fn banner_pixel(&self, rgba: &[u8], banner: &str) -> [u8; 4] {
        let text_cells = banner.chars().count().min(usize::from(self.cols)).max(1);
        let pill_w = (text_cells * self.cell_w) as i32;
        let full_w = (usize::from(self.cols) * self.cell_w) as i32;
        let x = self.content_x + (full_w - pill_w) + 1;
        let y = self.content_y + (i32::from(self.rows) - 1) * self.cell_h as i32 + 1;
        self.pixel_at(rgba, x, y)
    }
}

fn premultiply(color: [u8; 4], alpha: u8) -> [u8; 4] {
    [
        ((u16::from(color[0]) * u16::from(alpha)) / 255) as u8,
        ((u16::from(color[1]) * u16::from(alpha)) / 255) as u8,
        ((u16::from(color[2]) * u16::from(alpha)) / 255) as u8,
        alpha,
    ]
}

fn opaque(rgb: [u8; 3]) -> [u8; 4] {
    [rgb[0], rgb[1], rgb[2], 0xFF]
}

/// Expected pending/close-confirm pill ground (issue #2333 contract: ANSI 3).
///
/// Derived from the palette in the test, not from the production paint
/// helper, so a helper reverted to a fixed color fails the pixel assertion
/// rather than satisfying a self-referential comparison.
fn banner_ground(palette: &ThemePalette) -> [u8; 4] {
    opaque(palette.active_index(3))
}

/// Expected help-panel ring (issue #2333 contract: ANSI 12).
fn assert_role_probes(rt: &Runtime, rgba: &[u8], palette: &ThemePalette) {
    let probe = Probe::new(rt);
    let rgb = |value: [u8; 4]| [value[0], value[1], value[2], 0xFF];
    // Default foreground block (row 0, col 0).
    assert_eq!(
        probe.pixel(rgba, 0, 0),
        rgb(palette.foreground),
        "plain text must use the theme foreground"
    );
    // ANSI foreground blocks (row 1, columns 0..15).
    for (index, entry) in palette.ansi.iter().enumerate() {
        let expected = [entry[0], entry[1], entry[2], 0xFF];
        assert_eq!(
            probe.pixel(rgba, ANSI_FG_ROW, index as u16),
            expected,
            "ANSI {index} foreground must come from the theme"
        );
    }
    // ANSI background cells (row 3, columns 0..15).
    for (index, entry) in palette.ansi.iter().enumerate() {
        let expected = [entry[0], entry[1], entry[2], 0xFF];
        assert_eq!(
            probe.pixel(rgba, ANSI_BG_ROW, index as u16),
            expected,
            "ANSI {index} background must come from the theme"
        );
    }
    // Selection background: the overlay is opaque and covers the cell.
    assert_eq!(
        probe.pixel(rgba, SELECTION_ROW, 4),
        palette.selection,
        "selection background must use the theme selection"
    );
    // Cursor overlay: the present path paints the theme cursor at 0xA0 alpha.
    assert_eq!(
        probe.pixel(rgba, CURSOR_ROW, CURSOR_COL),
        premultiply(palette.cursor, 0xA0),
        "cursor must use the theme cursor"
    );
}

#[test]
fn two_presets_recolor_every_owned_role() {
    let mut dark = runtime(dracula());
    feed_roles(&mut dark);
    let dark_rgba = frame(&mut dark);
    assert_role_probes(&dark, &dark_rgba, &dracula());

    let mut light = runtime(github_light());
    feed_roles(&mut light);
    let light_rgba = frame(&mut light);
    assert_role_probes(&light, &light_rgba, &github_light());

    assert_ne!(
        dark_rgba, light_rgba,
        "two different presets must not present the same frame"
    );
}

#[test]
fn custom_palette_recolors_every_owned_role_without_a_preset() {
    let palette = ThemePalette::from_custom(&custom());
    let mut rt = runtime(palette);
    feed_roles(&mut rt);
    let rgba = frame(&mut rt);
    assert_role_probes(&rt, &rgba, &palette);
    assert_ne!(rgba, {
        let mut dark = runtime(dracula());
        feed_roles(&mut dark);
        frame(&mut dark)
    });
}

#[test]
fn live_switch_set_theme_palette_matches_a_fresh_render() {
    let mut live = runtime(dracula());
    feed_roles(&mut live);
    let before = frame(&mut live);
    assert_role_probes(&live, &before, &dracula());

    live.set_theme_palette(github_light());
    let after = frame(&mut live);
    assert_role_probes(&live, &after, &github_light());

    let mut fresh = runtime(github_light());
    feed_roles(&mut fresh);
    let expected = frame(&mut fresh);
    assert_eq!(
        after, expected,
        "live switch must equal a fresh render: no cell may keep the old theme"
    );
    assert_ne!(before, after, "the switch must actually repaint");
}

#[test]
fn live_switch_custom_request_matches_a_fresh_render() {
    let mut live = runtime(dracula());
    feed_roles(&mut live);
    let before = frame(&mut live);

    let palette = custom();
    let changed = live
        .apply_theme_request(
            &QueuedThemeRequest {
                palette,
                persist: false,
            },
            &None,
        )
        .expect("preview applies");
    assert!(changed, "custom palette must change the live theme");
    let after = frame(&mut live);
    assert_role_probes(&live, &after, &ThemePalette::from_custom(&palette));

    let mut fresh = runtime(ThemePalette::from_custom(&palette));
    feed_roles(&mut fresh);
    assert_eq!(
        after,
        frame(&mut fresh),
        "custom live switch must equal a fresh custom render"
    );
    assert_ne!(before, after);
}

#[test]
fn live_switch_recolors_scrollback_cells() {
    let mut live = runtime(dracula());
    feed_scrollback(&mut live);
    let before = frame(&mut live);
    let probe = Probe::new(&live);

    // Map every block pixel in the scrolled viewport to its ANSI index under
    // A, then require the same cell to carry B's entry for that index.
    let dark = dracula();
    let light = github_light();
    // Every content row in the scrolled viewport starts with a full-coverage
    // block; the fixture colors it by line index. Rows 0..probe.rows must all
    // resolve to one of A's ANSI entries.
    for row in 0..probe.rows {
        let a = probe.pixel(&before, row, 0);
        assert!(
            dark.ansi
                .iter()
                .any(|entry| a == [entry[0], entry[1], entry[2], 0xFF]),
            "scrollback row {row} must show an ANSI-colored block, got {a:?}"
        );
    }

    live.set_theme_palette(light);
    let after = frame(&mut live);
    for row in 0..probe.rows {
        let a = probe.pixel(&before, row, 0);
        let index = dark
            .ansi
            .iter()
            .position(|entry| a == [entry[0], entry[1], entry[2], 0xFF])
            .expect("precondition: A block maps to an ANSI index");
        let expected = &light.ansi[index];
        assert_eq!(
            probe.pixel(&after, row, 0),
            [expected[0], expected[1], expected[2], 0xFF],
            "scrollback row {row} kept the old ANSI {index} color"
        );
    }
}

#[test]
fn pending_banner_follows_the_theme() {
    let t0 = Instant::now();
    let mut live = runtime(dracula());
    assert!(
        live.paste_text_via_gate("line1\nline2".to_string()),
        "multi-line paste must gate"
    );
    let before = live
        .tick_at(t0 + Duration::from_millis(100))
        .expect("pending banner presents");
    assert!(before.fills > 0, "banner frame must paint");
    let probe = Probe::new(&live);
    let banner = live
        .paste_banner_text_at(t0 + Duration::from_millis(100))
        .expect("banner text while pending");
    let dark = dracula();
    let light = github_light();
    let dark_fill = banner_ground(&dark);
    let light_fill = banner_ground(&light);
    assert_ne!(
        dark_fill, light_fill,
        "precondition: the two presets must differ in the ANSI 3 banner ground"
    );
    assert_ne!(
        light_fill, PENDING_PASTE_BANNER_BG,
        "precondition: the switched ground must differ from the pre-#2333 fixed fill"
    );
    let rgba = live.headless_rgba().expect("rgba before switch").to_vec();
    assert_eq!(
        probe.banner_pixel(&rgba, &banner),
        dark_fill,
        "pending banner ground must use the active theme's ANSI 3, not a fixed fill"
    );

    live.set_theme_palette(light);
    live.tick_at(t0 + Duration::from_millis(200))
        .expect("switch presents");
    let after = live.headless_rgba().expect("rgba after switch").to_vec();
    assert_eq!(
        probe.banner_pixel(&after, &banner),
        light_fill,
        "the switched theme must recolor the pending banner ground"
    );
    assert_ne!(
        probe.banner_pixel(&after, &banner),
        PENDING_PASTE_BANNER_BG,
        "the switched banner must not keep the pre-#2333 fixed fill"
    );

    let mut fresh = runtime(github_light());
    assert!(fresh.paste_text_via_gate("line1\nline2".to_string()));
    fresh
        .tick_at(t0 + Duration::from_millis(200))
        .expect("fresh banner presents");
    assert_eq!(
        after,
        fresh.headless_rgba().expect("fresh rgba"),
        "the banner must be recolored by the live switch"
    );
}

#[test]
fn close_confirm_banner_pixels_follow_the_switched_theme() {
    let mut live = close_confirm_runtime(dracula());
    assert!(
        !live.window_close_request(),
        "the close request must arm instead of exiting"
    );
    let banner = live
        .close_confirm_banner_text()
        .expect("armed close-confirm banner");
    let _ = live.tick().expect("armed banner presents");
    let probe = Probe::new(&live);
    let dark = dracula();
    let light = github_light();
    let dark_fill = banner_ground(&dark);
    let light_fill = banner_ground(&light);
    let rgba = live.headless_rgba().expect("rgba before switch").to_vec();
    assert_eq!(
        probe.banner_pixel(&rgba, &banner),
        dark_fill,
        "close-confirm banner ground must use the active theme's ANSI 3"
    );

    live.set_theme_palette(light);
    let _ = live.tick().expect("switch presents");
    let after = live.headless_rgba().expect("rgba after switch").to_vec();
    assert_eq!(
        probe.banner_pixel(&after, &banner),
        light_fill,
        "the switched theme must recolor the close-confirm banner ground"
    );
    assert_ne!(
        probe.banner_pixel(&after, &banner),
        PENDING_PASTE_BANNER_BG,
        "the switched close-confirm banner must not keep the pre-#2333 fixed fill"
    );
}

#[test]
fn help_panel_follows_the_theme() {
    let mut live = runtime(dracula());
    live.set_help_rows(vec!["alt+x  toggle_zoom".to_string()]);
    assert!(live.toggle_help(), "help toggles on");
    let before = frame(&mut live);
    let probe = Probe::new(&live);
    let dark = dracula();
    let light = github_light();

    // Locate the opaque panel surface (selection ground), then probe the
    // one-pixel ring just outside it. CTX-1477 replaced the full-cell accent
    // border with the theme's muted idle outline; the SDF band composites at
    // partial coverage, so the assertion is "visible, distinct, and
    // recoloured", not a fixed raw colour.
    let (sx0, sy0, sx1, sy1) = probe
        .color_bounds(&before, dark.selection)
        .expect("help panel surface must use the active theme's selection fill");
    let (x0, y0, x1, y1) = (sx0 - 1, sy0 - 1, sx1 + 1, sy1 + 1);
    let mid_y = (y0 + y1) / 2;
    let dark_edge = probe.pixel_at(&before, x0, mid_y);
    assert_ne!(
        dark_edge, dark.selection,
        "help panel left edge must paint a ring above the surface"
    );
    assert_ne!(
        dark_edge, dark.background,
        "help panel ring must be visible against the ground"
    );
    assert_eq!(
        probe.pixel_at(&before, x0 - 1, mid_y),
        dark.background,
        "the thin ring must not bleed outside the frame"
    );
    // Top inner row, last inner cell: blank underlay, so the pixel is the
    // panel's own opaque ground.
    let ground_x = x1 - probe.cell_w as i32 - probe.cell_w as i32 / 2;
    let ground_y = y0 + probe.cell_h as i32 + probe.cell_h as i32 / 2;
    assert_eq!(
        probe.pixel_at(&before, ground_x, ground_y),
        dark.selection,
        "help panel ground must use the theme selection fill"
    );

    live.set_theme_palette(light);
    let after = frame(&mut live);
    assert_ne!(
        light.selection, HELP_PANEL_BG,
        "precondition: the switched ground must differ from the pre-#2333 fixed fill"
    );
    let light_surface = probe
        .color_bounds(&after, light.selection)
        .expect("switched help surface must use the new theme's selection fill");
    assert_eq!(
        light_surface,
        (sx0, sy0, sx1, sy1),
        "the theme switch must not move the help panel"
    );
    let light_edge = probe.pixel_at(&after, x0, mid_y);
    assert_ne!(
        dark_edge, light_edge,
        "the switched theme must recolor the help panel ring"
    );
    assert_eq!(
        probe.pixel_at(&after, ground_x, ground_y),
        light.selection,
        "the switched theme must recolor the help panel ground"
    );
    assert_ne!(
        probe.pixel_at(&after, ground_x, ground_y),
        HELP_PANEL_BG,
        "the switched panel ground must not keep the pre-#2333 fixed fill"
    );
    assert_ne!(
        probe.pixel_at(&after, x0, mid_y),
        HELP_PANEL_BORDER,
        "the switched panel border must not keep the pre-#2333 fixed accent"
    );

    let mut fresh = runtime(github_light());
    fresh.set_help_rows(vec!["alt+x  toggle_zoom".to_string()]);
    assert!(fresh.toggle_help(), "fresh help toggles on");
    assert_eq!(
        after,
        frame(&mut fresh),
        "the help panel must be recolored by the live switch"
    );
}

#[test]
fn ime_preedit_pixels_follow_the_switched_theme() {
    let mut live = runtime(dracula());
    let _ = live.tick().expect("first tick arms the IME caret");
    live.handle_ime_preedit(Some("ni ".to_string()), Some(0));
    let _ = live.tick().expect("preedit presents");
    let probe = Probe::new(&live);
    let area = live.ime_cursor_area().expect("preedit caret armed");
    let dark = dracula();
    let light = github_light();
    // Cell 2 is the preedit's trailing space: no glyph ink, so the middle of
    // the cell is the preedit ground and the last two pixel rows are the
    // underline. Cell 0 carries the caret bar, cell 1 the glyphs.
    let x = area.x + 2 * probe.cell_w as i32 + probe.cell_w as i32 / 2;
    let ground_y = area.y + probe.cell_h as i32 / 2;
    let underline_y = area.y + probe.cell_h as i32 - 1;
    let rgba = live.headless_rgba().expect("rgba before switch").to_vec();
    assert_eq!(
        probe.pixel_at(&rgba, x, ground_y),
        dark.selection,
        "IME preedit ground must use the active theme's selection fill"
    );
    assert_eq!(
        probe.pixel_at(&rgba, x, underline_y),
        dark.cursor,
        "IME preedit underline must use the active theme's cursor accent"
    );

    live.set_theme_palette(light);
    let _ = live.tick().expect("switch presents");
    assert_eq!(
        live.ime_cursor_area().expect("caret stays armed"),
        area,
        "the theme switch must not move the preedit"
    );
    let after = live.headless_rgba().expect("rgba after switch").to_vec();
    assert_ne!(
        dark.selection, light.selection,
        "precondition: the two presets must differ in the preedit ground"
    );
    assert_eq!(
        probe.pixel_at(&after, x, ground_y),
        light.selection,
        "the switched theme must recolor the IME preedit ground"
    );
    assert_eq!(
        probe.pixel_at(&after, x, underline_y),
        light.cursor,
        "the switched theme must recolor the IME preedit underline"
    );
}
