//! Bittie pixel-art splash (issue #1318, CTX-1217).
//!
//! Single owner of the vendored mascot pixels and the first-run splash
//! policy. The sprite is the neutral-pose standalone chibi from the
//! workspace pixel-art suite
//! (`recording/bitty-mascot/pixel-art/bitty_mascot_32x32/`, canonical
//! unmirrored `> _` eyes per `CHARACTER.md`); the only bytes compiled into
//! the binary are the checked-in `../assets/mascot_pixels.rs` table
//! (palette + one index byte per pixel, `255` = transparent), produced by
//! `../assets/gen_mascot_pixels.py` from the vendored
//! `../assets/bittie_mascot_32x32.png` master. No image crate, no PNG
//! decoder, and no new dependency touch the release build: rendering is a
//! few hundred bytes of half-block cell output built at startup.
//!
//! Why cells, not Kitty/Sixel: the splash prints to the *parent* terminal
//! bitty was launched from, whose image-protocol support bitty cannot
//! assume, and bitty's own Kitty pipeline is intake-only (full rendering
//! is a separate epic; see `bitty-rich`). Half-block cells (`▀`/`▄` with
//! truecolor foreground/background, one cell per two sprite pixels) render
//! the pixel art on any UTF-8 terminal and degrade to an uncolored
//! silhouette where the unified CLI color gate
//! ([`crate::color::cli_color_enabled`]: piped stdout, `NO_COLOR`,
//! `TERM=dumb`, `--no-color`) says no color. Geometry is fixed: 32 columns
//! by 16 rows, so the splash is strictly smaller than the ASCII placeholder
//! it replaces and can never push content off-screen by wrapping on a
//! standard 80-column window.
//!
//! Policy: `--mascot` prints the art and exits 0 (local class: no config,
//! no instance, no plugin VM, no network, no stdin read). Normal startup
//! shows the splash once (first-run marker file under the XDG data root),
//! suppressed for one launch by `--no-splash` and always skipped for the
//! machine flows (`--headless`, `--test-mode`). Marker writes are
//! best-effort and swallowed: the splash never blocks shell spawn.

use std::path::{Path, PathBuf};

include!("../assets/mascot_pixels.rs");

/// Sprite width in pixels, which equals the splash width in terminal
/// columns (one cell per pixel horizontally).
pub(crate) const MASCOT_COLS: usize = 32;

/// Sprite height in pixels; the splash prints half that many rows.
pub(crate) const MASCOT_ROWS: usize = 32;

/// Splash rows on screen: two sprite pixels per cell (`▀`/`▄`).
pub(crate) const MASCOT_SPLASH_LINES: usize = MASCOT_ROWS / 2;

/// One-line fallback when the window is provably too narrow for the art.
pub(crate) const MASCOT_FALLBACK: &str = "bitty! (mascot skipped: window too narrow for the art)\n";

/// Marker file name under `$XDG_DATA_HOME/bitty/` (or the `$HOME` fallback).
/// Absent marker means first run; presence means the splash was shown.
pub(crate) const SPLASH_MARKER_FILE: &str = "splash-shown";

/// Upper bound on splash lines (keeps the splash a splash).
/// Test-only: enforced by unit test, not consulted at runtime (the row
/// count is fixed by [`MASCOT_SPLASH_LINES`]).
#[cfg(test)]
pub(crate) const MASCOT_MAX_LINES: usize = MASCOT_SPLASH_LINES;

/// Upper bound on splash width in columns (fits an 80-column window).
/// Test-only: enforced by unit test, not consulted at runtime.
#[cfg(test)]
pub(crate) const MASCOT_MAX_WIDTH: usize = MASCOT_COLS;

/// Splash width in terminal columns.
pub(crate) fn mascot_width() -> usize {
    MASCOT_COLS
}

/// Palette color for a pixel index (indices come from [`MASCOT_PIXELS`];
/// [`MASCOT_TRANSPARENT`] has no color).
fn mascot_rgb(index: u8) -> [u8; 3] {
    MASCOT_PALETTE[index as usize]
}

/// Whether a pixel index is opaque.
fn mascot_opaque(index: u8) -> bool {
    index != MASCOT_TRANSPARENT
}

/// Renders the vendored sprite as half-block terminal cells: 32 columns by
/// 16 rows, one `▀`/`▄`/space per two vertical sprite pixels. With `color`,
/// opaque pixels set truecolor foreground/background (`38;2`/`48;2`) with
/// per-line resets; without, the same glyphs print uncolored so piped and
/// `NO_COLOR` output stays escape-free. Transparent pixels are always
/// spaces over the terminal's own background, so the splash adapts to
/// light and dark themes alike.
pub(crate) fn render_mascot_art(color: bool) -> String {
    let mut out = String::with_capacity(MASCOT_SPLASH_LINES * (MASCOT_COLS * 4 + 8));
    for pair in MASCOT_PIXELS.chunks_exact(2) {
        let mut current_fg: Option<[u8; 3]> = None;
        let mut current_bg: Option<[u8; 3]> = None;
        for (&top, &bottom) in pair[0].iter().zip(pair[1].iter()) {
            let (glyph, fg, bg) = match (mascot_opaque(top), mascot_opaque(bottom)) {
                (true, true) => ('\u{2580}', Some(mascot_rgb(top)), Some(mascot_rgb(bottom))),
                (true, false) => ('\u{2580}', Some(mascot_rgb(top)), None),
                (false, true) => ('\u{2584}', Some(mascot_rgb(bottom)), None),
                (false, false) => (' ', None, None),
            };
            if color {
                if fg != current_fg {
                    match fg {
                        Some([r, g, b]) => {
                            out.push_str(&format!("\u{1b}[38;2;{r};{g};{b}m"));
                        }
                        None => out.push_str("\u{1b}[39m"),
                    }
                    current_fg = fg;
                }
                if bg != current_bg {
                    match bg {
                        Some([r, g, b]) => {
                            out.push_str(&format!("\u{1b}[48;2;{r};{g};{b}m"));
                        }
                        None => out.push_str("\u{1b}[49m"),
                    }
                    current_bg = bg;
                }
            }
            out.push(glyph);
        }
        if color {
            out.push_str("\u{1b}[0m");
        }
        out.push('\n');
    }
    out
}

/// Picks the splash art for a known-or-unknown window width and the unified
/// CLI color decision: full pixel art unless the window is provably too
/// narrow, in which case the one-line fallback. `None` (unknown width, e.g.
/// piped headless) prints the full art — always safe.
pub(crate) fn mascot_art_for_width(columns: Option<u16>, color: bool) -> String {
    match columns {
        Some(width) if (width as usize) < mascot_width() => MASCOT_FALLBACK.to_string(),
        _ => render_mascot_art(color),
    }
}

/// Prints the width-appropriate splash art to stdout and flushes.
/// Best-effort: flush errors are swallowed (startup must continue).
pub(crate) fn print_mascot_art(columns: Option<u16>, color: bool) {
    print!("{}", mascot_art_for_width(columns, color));
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Resolves the splash marker path from injected environment values
/// (`$XDG_DATA_HOME/bitty/splash-shown`, fallback
/// `~/.local/share/bitty/splash-shown`); `None` when no data root exists.
/// Same root policy as the plugin store
/// ([`crate::plugin_runtime::data_home_for`]).
pub(crate) fn splash_marker_path(
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    crate::plugin_runtime::data_home_for(xdg_data_home, home)
        .map(|base| base.join("bitty").join(SPLASH_MARKER_FILE))
}

/// Live-environment [`splash_marker_path`]: reads process env (startup only;
/// tests inject values through [`splash_marker_path`]).
pub(crate) fn splash_marker_path_live() -> Option<PathBuf> {
    splash_marker_path(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Whether the normal-startup splash should print: suppressed by
/// `--no-splash`, once-only via the marker, and skipped entirely when no
/// marker path resolves (fail closed: never splash every launch when the
/// data root is missing). Pure over injected values; never touches the
/// filesystem beyond the [`Path::exists`] probe the caller passes in.
pub(crate) fn should_show_splash(no_splash: bool, marker: Option<&Path>) -> bool {
    if no_splash {
        return false;
    }
    match marker {
        Some(path) => !path.exists(),
        None => false,
    }
}

/// Records the splash as shown: creates parents and writes the marker.
/// Best-effort by contract — every I/O error is swallowed so a read-only
/// or missing data root can never block shell spawn or change the exit
/// code. Re-recording is idempotent.
pub(crate) fn record_splash_shown(marker: &Path) {
    if let Some(parent) = marker.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::write(marker, "shown\n");
}
