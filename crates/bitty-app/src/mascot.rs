//! Bittie mascot splash (issue #1318, CTX-0729).
//!
//! Single owner of the vendored mascot art and the first-run splash
//! policy. `crates/bitty-app/assets/mascot.txt` is the only text asset
//! compiled into the binary (pure ASCII, bounded: see [`MASCOT_MAX_LINES`]
//! / [`MASCOT_MAX_WIDTH`]); the sixel/block variants in
//! `recording/bitty-mascot/` stay out. `init.rs` re-exports these names
//! for the `bitty init` wizard greeting so both surfaces agree.
//!
//! Policy: `--mascot` prints the art and exits 0 (local class: no config,
//! no instance, no plugin VM, no network, no stdin read). Normal startup
//! shows the splash once (first-run marker file under the XDG data root),
//! suppressed for one launch by `--no-splash` and always skipped for the
//! machine flows (`--headless`, `--test-mode`). Marker writes are
//! best-effort and swallowed: the splash never blocks shell spawn.

use std::path::{Path, PathBuf};

/// Vendored mascot art, byte-identical to the accepted source portrait.
/// Pure text so it renders anywhere stdout goes, including piped runs.
pub(crate) const MASCOT_ART: &str = include_str!("../assets/mascot.txt");

/// One-line fallback when the window is provably too narrow for the art.
pub(crate) const MASCOT_FALLBACK: &str = "bitty! (mascot skipped: window too narrow for the art)\n";

/// Marker file name under `$XDG_DATA_HOME/bitty/` (or the `$HOME` fallback).
/// Absent marker means first run; presence means the splash was shown.
pub(crate) const SPLASH_MARKER_FILE: &str = "splash-shown";

/// Upper bound on vendored art lines (keeps the splash a splash).
/// Test-only: enforced by unit test, not consulted at runtime (the width
/// fallback is computed dynamically from the asset).
#[cfg(test)]
pub(crate) const MASCOT_MAX_LINES: usize = 32;

/// Upper bound on vendored art width in columns (fits an 80-column window).
/// Test-only: enforced by unit test, not consulted at runtime.
#[cfg(test)]
pub(crate) const MASCOT_MAX_WIDTH: usize = 80;

/// Widest art line in bytes (the art is pure ASCII, so bytes == columns).
/// Computed from the vendored asset so an asset refresh cannot silently
/// break the narrow-window bound.
pub(crate) fn mascot_width() -> usize {
    MASCOT_ART.lines().map(|line| line.len()).max().unwrap_or(0)
}

/// Picks the splash art for a known-or-unknown window width: full art
/// unless the window is provably too narrow, in which case the one-line
/// fallback. `None` (unknown width, e.g. piped headless) prints the full
/// pure-text art — always safe.
pub(crate) fn mascot_art_for_width(columns: Option<u16>) -> &'static str {
    match columns {
        Some(width) if (width as usize) < mascot_width() => MASCOT_FALLBACK,
        _ => MASCOT_ART,
    }
}

/// Prints the width-appropriate splash art to stdout and flushes.
/// Best-effort: flush errors are swallowed (startup must continue).
pub(crate) fn print_mascot_art(columns: Option<u16>) {
    print!("{}", mascot_art_for_width(columns));
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
