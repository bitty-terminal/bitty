//! Unified `is_terminal` gate for CLI color output (CTX-1065).
//!
//! Single source of truth for human-table and help color decisions:
//! color is enabled iff stdout is a terminal, `TERM` is not `dumb`,
//! `NO_COLOR` is unset, and the explicit `--no-color` flag is off.
//! Explicit `--no-color` stays authoritative everywhere it exists; no new
//! environment variables are introduced. Callers that already take an
//! explicit `color: bool` (for example `format_themes_table_with_color`
//! and `help_text_short`) keep that hook so forced color still renders
//! even when piped.

use std::io::IsTerminal as _;

/// Pure color decision for tests: no environment or terminal probing.
///
/// - `no_color_flag`: explicit `--no-color` (any shared per-verb flag).
/// - `no_color_env`: `NO_COLOR` is set.
/// - `term_is_dumb`: `TERM` trims to `dumb` (case-insensitive).
/// - `stdout_is_tty`: `stdout().is_terminal()`.
#[must_use]
pub(crate) fn cli_color_enabled_impl(
    no_color_flag: bool,
    no_color_env: bool,
    term_is_dumb: bool,
    stdout_is_tty: bool,
) -> bool {
    !no_color_flag && !no_color_env && !term_is_dumb && stdout_is_tty
}

/// Whether ANSI color is enabled for CLI human output.
///
/// Reads the existing conventions (`--no-color`, `NO_COLOR`, `TERM=dumb`)
/// plus the unified `stdout().is_terminal()` gate. Piped output stays
/// byte-identical plain text.
#[must_use]
pub(crate) fn cli_color_enabled(no_color: bool) -> bool {
    let no_color_env = std::env::var("NO_COLOR").is_ok();
    let term_is_dumb =
        matches!(std::env::var("TERM"), Ok(term) if term.trim().eq_ignore_ascii_case("dumb"));
    let stdout_is_tty = std::io::stdout().is_terminal();
    cli_color_enabled_impl(no_color, no_color_env, term_is_dumb, stdout_is_tty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_gate_needs_tty_without_opt_outs() {
        assert!(cli_color_enabled_impl(false, false, false, true));
        assert!(!cli_color_enabled_impl(true, false, false, true));
        assert!(!cli_color_enabled_impl(false, true, false, true));
        assert!(!cli_color_enabled_impl(false, false, true, true));
        assert!(!cli_color_enabled_impl(false, false, false, false));
        assert!(!cli_color_enabled_impl(true, true, true, false));
    }

    // NOTE (CodeRabbit 1874): the live `cli_color_enabled` probe reads the
    // real process stdout, so no unit test may call it here — that would
    // couple the suite to the runner's stdout configuration. Live-gate
    // behavior (piped => plain) is pinned by the subprocess integration
    // tests in tests/cli_help.rs and tests/cli_list.rs instead.
}
