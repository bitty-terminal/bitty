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

use std::time::{Duration, Instant};

use bitty_config::theme::{CustomPalette, resolve_theme};
use bitty_runtime::plugin_runtime::QueuedThemeRequest;
use bitty_runtime::{AnimationPolicy, Runtime, RuntimeConfig, ThemePalette};
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
            content_x: frame.content.x + pad,
            content_y: frame.content.y + pad,
            cell_w: frame.content.width as usize / usize::from(frame.cols),
            cell_h: frame.content.height as usize / usize::from(frame.rows),
        }
    }

    fn pixel(&self, rgba: &[u8], row: u16, col: u16) -> [u8; 4] {
        let x = self.content_x + i32::from(col) * self.cell_w as i32 + self.cell_w as i32 / 2;
        let y = self.content_y + i32::from(row) * self.cell_h as i32 + self.cell_h as i32 / 2;
        assert!(
            x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height,
            "probe ({row},{col}) outside {}x{}",
            self.width,
            self.height
        );
        let offset = (y as usize * self.width + x as usize) * 4;
        rgba[offset..offset + 4].try_into().expect("four bytes")
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

    live.set_theme_palette(github_light());
    live.tick_at(t0 + Duration::from_millis(200))
        .expect("switch presents");
    let after = live.headless_rgba().expect("rgba after switch").to_vec();

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
fn help_panel_follows_the_theme() {
    let mut live = runtime(dracula());
    live.set_help_rows(vec!["alt+x  toggle_zoom".to_string()]);
    assert!(live.toggle_help(), "help toggles on");
    let _before = frame(&mut live);

    live.set_theme_palette(github_light());
    let after = frame(&mut live);

    let mut fresh = runtime(github_light());
    fresh.set_help_rows(vec!["alt+x  toggle_zoom".to_string()]);
    assert!(fresh.toggle_help(), "fresh help toggles on");
    assert_eq!(
        after,
        frame(&mut fresh),
        "the help panel must be recolored by the live switch"
    );
}
