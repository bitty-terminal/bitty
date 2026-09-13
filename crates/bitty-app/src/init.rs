//! `bitty init` opt-in setup wizard (#243, CTX-0149).

use std::io::IsTerminal as _;

use crate::cli::Args;
use crate::spawn::FALLBACK_SHELL;

/// Hamster mascot art, vendored byte-identical from the workspace asset
/// `recording/bitty-mascot/ascii/bitty_ascii.txt` (DEC-0002). Pure text so
/// it renders anywhere stdout goes, including piped headless runs; the
/// sixel/block variants stay out of the binary.
pub(crate) const INIT_MASCOT_ART: &str = include_str!("../assets/mascot.txt");

/// One-line fallback when the window is too narrow for the art: fail closed
/// with an honest line instead of a wrapped mess.
pub(crate) const INIT_MASCOT_FALLBACK: &str =
    "bitty! (mascot skipped: window too narrow for the art)\n";

/// Maximum prompt attempts per wizard step before aborting. Bounded so piped
/// garbage or a stuck key can never spin the wizard forever.
pub(crate) const INIT_MAX_ATTEMPTS: usize = 3;

/// Maximum accepted stdin line length in bytes (mirrors the config
/// `MAX_LINE_BYTES` posture: overlong lines are truncated, never unbounded).
pub(crate) const INIT_MAX_LINE_BYTES: usize = 4096;

/// Common shells probed in order when building the shell menu.
pub(crate) const INIT_COMMON_SHELLS: &[&str] = &[
    "/bin/bash",
    "/usr/bin/bash",
    "/bin/zsh",
    "/usr/bin/zsh",
    "/usr/bin/fish",
    "/bin/fish",
    "/bin/sh",
];

/// Widest art line in bytes (the art is pure ASCII, so bytes == columns).
/// Computed from the vendored asset so an asset refresh cannot silently
/// break the narrow-window bound.
pub(crate) fn init_mascot_width() -> usize {
    INIT_MASCOT_ART
        .lines()
        .map(|line| line.len())
        .max()
        .unwrap_or(0)
}

/// Picks the greeting art for a known-or-unknown window width: full art
/// unless the window is provably too narrow, in which case the one-line
/// fallback. `None` (unknown width, e.g. piped headless) prints the full
/// pure-text art — always safe, tested headless.
pub(crate) fn init_greeting_art(columns: Option<u16>) -> &'static str {
    match columns {
        Some(width) if (width as usize) < init_mascot_width() => INIT_MASCOT_FALLBACK,
        _ => INIT_MASCOT_ART,
    }
}

/// Parses a `COLUMNS`-style width value. Pure over the injected string so
/// tests never touch the environment; `None`/garbage/zero means unknown.
pub(crate) fn init_columns_from_env(value: Option<&str>) -> Option<u16> {
    value?.trim().parse::<u16>().ok().filter(|width| *width > 0)
}

/// Keybinding preset choice (the vim step, CTX-0149 owner note).
///
/// The shipped defaults are already the Alt-as-Mod vim-style map (CTX-0178:
/// `Alt+h/j/k/l`, `Alt+1..9`, `Alt+u/i`, ...). `Default` leaves them
/// implicit (no `keymaps` section written); `Vim` writes the same map
/// explicitly as a tweakable starting point for vim users. Both agree by
/// construction: the explicit block is rendered FROM
/// [`bitty_config::keymap::DEFAULT_KEYMAPS`], never copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitKeyPreset {
    /// Shipped defaults apply; write no `keymaps` section.
    Default,
    /// Write the shipped map explicitly (one-click vim defaults).
    Vim,
}

/// Wizard answers: pure data, rendered to `init.lua` by [`render_init_lua`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InitAnswers {
    /// `terminal.shell` override; `None` leaves the startup default
    /// (`$SHELL` or `/bin/sh`).
    pub(crate) shell: Option<String>,
    /// `appearance.theme` value (always a known preset name).
    pub(crate) theme: String,
    /// `font.family` (trimmed, non-empty).
    pub(crate) font_family: String,
    /// `font.size` in points, within `(0, 128]`.
    pub(crate) font_size: f32,
    /// `decoration.gaps_in` in logical px, within `0..=32`.
    pub(crate) gaps_in: u32,
    /// `decoration.gaps_out` in logical px, within `0..=32`.
    pub(crate) gaps_out: u32,
    /// `decoration.border` in logical px, within `0..=8`.
    pub(crate) border: u32,
    /// `decoration.radius` in logical px, within `0..=16`.
    pub(crate) radius: u32,
    /// `terminal.scrollback` lines, within `0..=100000`.
    pub(crate) scrollback: u32,
    /// Top-level `close_confirm` mode.
    pub(crate) close_confirm: bitty_config::CloseConfirm,
    /// Keybinding preset choice.
    pub(crate) key_preset: InitKeyPreset,
}

/// Explicit value-flag answers (`bitty init --theme … --scrollback …`).
///
/// Parsed and validated ONCE by [`init_overrides_from_args`] with the same
/// fail-closed step parsers the interactive wizard uses, so an explicit flag
/// and an interactive answer can never disagree on validity. `None` means
/// "the flag was absent; ask (interactive) or take the default (`--yes`)".
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct InitOverrides {
    /// `--theme NAME` answer.
    pub(crate) theme: Option<String>,
    /// `--font-family NAME` answer.
    pub(crate) font_family: Option<String>,
    /// `--font-size PTS` answer.
    pub(crate) font_size: Option<f32>,
    /// `--scrollback LINES` answer.
    pub(crate) scrollback: Option<u32>,
    /// `--close-confirm MODE` answer.
    pub(crate) close_confirm: Option<bitty_config::CloseConfirm>,
    /// `--gaps-in PX` answer.
    pub(crate) gaps_in: Option<u32>,
    /// `--gaps-out PX` answer.
    pub(crate) gaps_out: Option<u32>,
    /// `--border PX` answer.
    pub(crate) border: Option<u32>,
    /// `--radius PX` answer.
    pub(crate) radius: Option<u32>,
}

/// Validates and normalizes one shell path: trims, rejects empty,
/// overlong, and control-character input (fail-closed; a shell path with
/// controls is never written into the config).
pub(crate) fn init_clean_shell(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("shell must not be empty".to_string());
    }
    if trimmed.len() > bitty_config::types::MAX_SHELL_LEN {
        return Err(format!(
            "shell path must be <= {} bytes",
            bitty_config::types::MAX_SHELL_LEN
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("shell path must not contain control characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// Sane non-interactive defaults for `--yes`: `$SHELL` when it is a clean
/// path (else unset so startup falls back), the dark preset, the shipped font
/// default, the shipped decoration geometry, the shipped scrollback, the
/// default close-confirm mode, and the implicit shipped keymap defaults.
pub(crate) fn init_yes_defaults(shell_env: Option<&str>) -> InitAnswers {
    let shell = shell_env
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| init_clean_shell(s).ok());
    InitAnswers {
        shell,
        theme: bitty_config::theme::DARK_THEME_ALIAS.to_string(),
        font_family: bitty_config::types::DEFAULT_FONT_FAMILY.to_string(),
        font_size: bitty_config::types::DEFAULT_FONT_SIZE,
        gaps_in: bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX,
        gaps_out: bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX,
        border: bitty_config::types::DEFAULT_DECORATION_BORDER_PX,
        radius: bitty_config::types::DEFAULT_DECORATION_RADIUS_PX,
        scrollback: bitty_config::types::TerminalConfig::default().scrollback,
        close_confirm: bitty_config::types::DEFAULT_CLOSE_CONFIRM,
        key_preset: InitKeyPreset::Default,
    }
}

/// Parses and validates the explicit value flags into [`InitOverrides`].
///
/// Every value goes through the same fail-closed step parser the interactive
/// wizard uses; the first invalid value aborts the whole run with a message
/// naming the flag. Init-only flags present without a value fail closed
/// (their empty raw is an error, never a silent default).
pub(crate) fn init_overrides_from_args(args: &Args) -> Result<InitOverrides, String> {
    let mut out = InitOverrides::default();
    if let Some(raw) = args.theme.as_deref().filter(|v| !v.trim().is_empty()) {
        out.theme = Some(init_parse_theme_answer(raw)?);
    }
    if let Some(raw) = args.font_family.as_deref().filter(|v| !v.trim().is_empty()) {
        out.font_family = Some(init_parse_font_family_answer(raw)?);
    }
    if let Some(raw) = args.font_size.as_deref().filter(|v| !v.trim().is_empty()) {
        out.font_size = Some(init_parse_font_size_answer(raw)?);
    }
    if let Some(raw) = args.init_scrollback.as_deref() {
        if raw.trim().is_empty() {
            return Err("--scrollback needs a line count".to_string());
        }
        out.scrollback = Some(init_parse_scrollback_answer(raw)?);
    }
    if let Some(raw) = args.init_close_confirm.as_deref() {
        if raw.trim().is_empty() {
            return Err("--close-confirm needs a mode".to_string());
        }
        out.close_confirm = Some(init_parse_close_confirm_answer(raw)?);
    }
    if let Some(raw) = args.init_gaps_in.as_deref() {
        if raw.trim().is_empty() {
            return Err("--gaps-in needs a value".to_string());
        }
        out.gaps_in = Some(init_parse_decoration_answer(
            raw,
            "gaps_in",
            bitty_config::types::MAX_DECORATION_GAP_PX,
            bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX,
        )?);
    }
    if let Some(raw) = args.init_gaps_out.as_deref() {
        if raw.trim().is_empty() {
            return Err("--gaps-out needs a value".to_string());
        }
        out.gaps_out = Some(init_parse_decoration_answer(
            raw,
            "gaps_out",
            bitty_config::types::MAX_DECORATION_GAP_PX,
            bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX,
        )?);
    }
    if let Some(raw) = args.init_border.as_deref() {
        if raw.trim().is_empty() {
            return Err("--border needs a value".to_string());
        }
        out.border = Some(init_parse_decoration_answer(
            raw,
            "border",
            bitty_config::types::MAX_DECORATION_BORDER_PX,
            bitty_config::types::DEFAULT_DECORATION_BORDER_PX,
        )?);
    }
    if let Some(raw) = args.init_radius.as_deref() {
        if raw.trim().is_empty() {
            return Err("--radius needs a value".to_string());
        }
        out.radius = Some(init_parse_decoration_answer(
            raw,
            "radius",
            bitty_config::types::MAX_DECORATION_RADIUS_PX,
            bitty_config::types::DEFAULT_DECORATION_RADIUS_PX,
        )?);
    }
    Ok(out)
}

/// Applies validated value-flag overrides to `answers` (CLI wins).
pub(crate) fn init_apply_overrides(answers: &mut InitAnswers, overrides: &InitOverrides) {
    if let Some(v) = &overrides.theme {
        answers.theme.clone_from(v);
    }
    if let Some(v) = &overrides.font_family {
        answers.font_family.clone_from(v);
    }
    if let Some(v) = overrides.font_size {
        answers.font_size = v;
    }
    if let Some(v) = overrides.scrollback {
        answers.scrollback = v;
    }
    if let Some(v) = overrides.close_confirm {
        answers.close_confirm = v;
    }
    if let Some(v) = overrides.gaps_in {
        answers.gaps_in = v;
    }
    if let Some(v) = overrides.gaps_out {
        answers.gaps_out = v;
    }
    if let Some(v) = overrides.border {
        answers.border = v;
    }
    if let Some(v) = overrides.radius {
        answers.radius = v;
    }
}

/// Builds the shell menu: `$SHELL` first when set, then the common shells
/// that exist, deduplicated. Always ends with the POSIX fallback so the
/// menu — and its default — is never empty. `exists` is injected so tests
/// stay hermetic (no filesystem).
pub(crate) fn init_shell_candidates(
    shell_env: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |value: &str| {
        let trimmed = value.trim();
        if !trimmed.is_empty() && !out.iter().any(|seen| seen == trimmed) {
            out.push(trimmed.to_string());
        }
    };
    if let Some(env) = shell_env {
        push_unique(env);
    }
    for candidate in INIT_COMMON_SHELLS {
        if exists(candidate) {
            push_unique(candidate);
        }
    }
    push_unique(FALLBACK_SHELL);
    out
}

/// Parses one shell-step answer: empty takes the menu default (index 0), a
/// `1`-based number picks a menu entry, anything else is a custom path
/// through [`init_clean_shell`]. Total: every input maps to a value or a
/// repromptable error, never a panic.
pub(crate) fn init_parse_shell_answer(
    raw: &str,
    candidates: &[String],
) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(candidates.first().cloned());
    }
    if let Ok(number) = trimmed.parse::<usize>() {
        if number >= 1 && number <= candidates.len() {
            return Ok(Some(candidates[number - 1].clone()));
        }
        return Err(format!(
            "pick 1..={} or type a shell path",
            candidates.len()
        ));
    }
    init_clean_shell(trimmed).map(Some)
}

/// Parses one font-family-step answer: empty takes the shipped default;
/// otherwise trims, rejects empty, overlong, and control-character input
/// fail-closed (mirrors [`bitty_config::types::FontConfig::validate`]'s
/// family bound so the wizard can never emit a family startup would reject).
pub(crate) fn init_parse_font_family_answer(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(bitty_config::types::DEFAULT_FONT_FAMILY.to_string());
    }
    if trimmed.len() > bitty_config::types::MAX_FONT_FAMILY_LEN {
        return Err(format!(
            "font family must be <= {} bytes",
            bitty_config::types::MAX_FONT_FAMILY_LEN
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("font family must not contain control characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// Parses one scrollback-step answer: empty takes the shipped default,
/// otherwise a `u32` within `0..=100000` (the
/// [`bitty_config::types::TerminalConfig::validate`] bound, so the wizard can
/// never emit a line count startup would reject).
pub(crate) fn init_parse_scrollback_answer(raw: &str) -> Result<u32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(bitty_config::types::TerminalConfig::default().scrollback);
    }
    match trimmed.parse::<u32>() {
        Ok(lines) if lines <= bitty_config::types::MAX_TERMINAL_SCROLLBACK => Ok(lines),
        _ => Err(format!(
            "scrollback must be an integer within [0, {}]",
            bitty_config::types::MAX_TERMINAL_SCROLLBACK
        )),
    }
}

/// Parses one close-confirm-step answer: empty/`1` take the default
/// (`when_busy`), `2`/`always` and `3`/`never` select their modes. Anything
/// else reprompts instead of writing a mode startup would only fall back.
pub(crate) fn init_parse_close_confirm_answer(
    raw: &str,
) -> Result<bitty_config::CloseConfirm, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "1" => Ok(bitty_config::types::DEFAULT_CLOSE_CONFIRM),
        "2" | "always" => Ok(bitty_config::CloseConfirm::Always),
        "3" | "never" => Ok(bitty_config::CloseConfirm::Never),
        _ => Err("pick 1 (when_busy), 2 (always), or 3 (never)".to_string()),
    }
}

/// Parses one decoration-scalar-step answer: empty takes `default`,
/// otherwise a `u32` within `0..=max` (the field's shipped bound, so the
/// wizard can never emit a value `DecorationConfig::validate` would reject).
pub(crate) fn init_parse_decoration_answer(
    raw: &str,
    field: &str,
    max: u32,
    default: u32,
) -> Result<u32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    match trimmed.parse::<u32>() {
        Ok(value) if value <= max => Ok(value),
        _ => Err(format!("{field} must be an integer within [0, {max}]")),
    }
}

/// Parses one theme-step answer. Empty/`1` take the default (`dark`, the
/// canonical convenience alias). Any built-in preset name or alias resolves
/// to its canonical registry name; anything else reprompts instead of
/// writing a value the resolver would only fall back.
pub(crate) fn init_parse_theme_answer(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "" | "1" | "dark" | "bitty-dark" => Ok(bitty_config::theme::DARK_THEME_ALIAS.to_string()),
        _ => match bitty_config::theme::resolve_theme_with_status(Some(&trimmed)) {
            (theme, bitty_config::theme::ThemeResolution::Named) => Ok(theme.name.to_string()),
            _ => Err("unknown theme; run 'bitty list themes' to see the catalog".to_string()),
        },
    }
}

/// Parses one font-size-step answer: empty takes the default point size,
/// otherwise a finite number within `(0, 128]` (the `FontConfig` bound, so
/// the wizard can never emit a size startup would reject).
pub(crate) fn init_parse_font_size_answer(raw: &str) -> Result<f32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(bitty_config::types::DEFAULT_FONT_SIZE);
    }
    match trimmed.parse::<f32>() {
        Ok(size) if size.is_finite() && size > 0.0 && size <= 128.0 => Ok(size),
        _ => Err("font size must be a number within (0, 128]".to_string()),
    }
}

/// Parses one keybinding-preset-step answer: empty/`1` keeps the implicit
/// shipped defaults, `2`/`vim` writes them explicitly.
pub(crate) fn init_parse_preset_answer(raw: &str) -> Result<InitKeyPreset, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "default" => Ok(InitKeyPreset::Default),
        "2" | "vim" => Ok(InitKeyPreset::Vim),
        _ => Err("pick 1 (default) or 2 (vim)".to_string()),
    }
}

/// Escapes a string for a double-quoted Lua value. Wizard inputs are
/// control-free by construction ([`init_clean_shell`]), generated values
/// harder still; backslash and quote are escaped so paths like
/// `C:\Tools\sh` stay one valid string.
pub(crate) fn init_lua_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Renders the one-click vim preset block FROM the shipped defaults
/// ([`bitty_config::keymap::DEFAULT_KEYMAPS`], CTX-0178) so the preset and
/// the binary defaults cannot drift: any future default change flows into
/// the wizard output automatically, and
/// `init_vim_preset_agrees_with_shipped_defaults` pins the agreement.
pub(crate) fn init_render_vim_keymaps() -> String {
    let mut out = String::from("    keymaps = {\n");
    for (chord, action) in bitty_config::keymap::DEFAULT_KEYMAPS {
        out.push_str(&format!(
            "        {{ chord = \"{chord}\", action = \"{action}\", context = \"global\" }},\n"
        ));
    }
    out.push_str("    },\n");
    out
}

/// Renders wizard answers as an `init.lua` return table. The output always
/// parses via `bitty-config::file::parse_lua_config` (pinned by
/// `init_rendered_config_parses_and_merges_to_effective`): `theme`, the
/// `font` pair, and the `decoration`/`terminal`/`close_confirm` basics are
/// always present with shipped keys only, and the `keymaps` section appears
/// only for the vim preset.
pub(crate) fn render_init_lua(answers: &InitAnswers) -> String {
    let mut out = String::from(
        "-- bitty user configuration (Lua, wezterm-style), written by `bitty init`.\n\
         -- Evaluated in the bitty-lua sandbox (same budgets as plugins; no io/os).\n\
         -- Unknown keys fail closed; validate with `bitty config check`.\n\
         return {\n",
    );
    out.push_str(&format!(
        "    theme = \"{}\",\n",
        init_lua_escape(&answers.theme)
    ));
    out.push_str(&format!(
        "    font = {{ family = \"{}\", size = {} }},\n",
        init_lua_escape(&answers.font_family),
        answers.font_size,
    ));
    out.push_str(&format!(
        "    decoration = {{ gaps_in = {}, gaps_out = {}, border = {}, radius = {} }},\n",
        answers.gaps_in, answers.gaps_out, answers.border, answers.radius,
    ));
    match &answers.shell {
        Some(shell) => out.push_str(&format!(
            "    terminal = {{ scrollback = {}, shell = \"{}\" }},\n",
            answers.scrollback,
            init_lua_escape(shell),
        )),
        None => out.push_str(&format!(
            "    terminal = {{ scrollback = {} }},\n",
            answers.scrollback,
        )),
    }
    out.push_str(&format!(
        "    close_confirm = \"{}\",\n",
        answers.close_confirm.as_str()
    ));
    match answers.key_preset {
        InitKeyPreset::Default => {
            out.push_str(
                "    -- Chrome keys: the shipped Alt-as-Mod defaults apply (Alt+h/j/k/l move,\n\
                 -- Alt+1..9 jump to view N, Alt+u/i page up/down, Shift+Alt+h/j/k/l split,\n\
                 -- Shift+Ctrl+h/j/k/l resize, Alt+w close, Alt+z/m/f zoom, Ctrl+Tab cycle,\n\
                 -- Ctrl+Shift+C/V copy/paste). Re-run `bitty init` and pick the vim preset\n\
                 -- to pin this map explicitly here.\n",
            );
        }
        InitKeyPreset::Vim => {
            out.push_str(
                "    -- Chrome keys: one-click vim preset (Alt+h/j/k/l move, Alt+1..9 jump,\n\
                 -- Alt+u/i page, Shift+Alt+h/j/k/l split, Alt+w close, Alt+z zoom).\n\
                 -- This is the map the binary ships; entries here replace defaults by\n\
                 -- context + chord identity, so tweak freely.\n",
            );
            out.push_str(&init_render_vim_keymaps());
        }
    }
    out.push_str("}\n");
    out
}

/// Reads one stdin line without the trailing newline. `None` on EOF or I/O
/// error (the wizard aborts rather than guessing). Overlong lines are
/// truncated to [`INIT_MAX_LINE_BYTES`] so a pasted megabyte cannot grow
/// the answer buffer.
pub(crate) fn init_read_line(input: &mut dyn std::io::BufRead) -> Option<String> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            if line.len() > INIT_MAX_LINE_BYTES {
                line.truncate(INIT_MAX_LINE_BYTES);
            }
            Some(line.trim_end_matches(['\r', '\n']).to_string())
        }
        Err(_) => None,
    }
}

/// Asks one wizard step: prints `prompt`, reads a line, parses it.
/// Reprompts up to [`INIT_MAX_ATTEMPTS`] on parse errors, then aborts;
/// EOF aborts immediately. Prompts go to `output` (stdout at runtime) so
/// piped-stdin runs still show the questions.
pub(crate) fn init_ask<T>(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    prompt: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    for attempt in 1..=INIT_MAX_ATTEMPTS {
        let _ = writeln!(output, "{prompt}");
        let _ = output.flush();
        match init_read_line(input) {
            Some(line) => match parse(&line) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    let _ = writeln!(
                        output,
                        "  ({err} — try again [{attempt}/{INIT_MAX_ATTEMPTS}])"
                    );
                }
            },
            None => return Err("bitty init: aborted (end of input)".to_string()),
        }
    }
    Err(format!(
        "bitty init: aborted (too many invalid answers, limit {INIT_MAX_ATTEMPTS})"
    ))
}

/// Runs the interactive wizard: mascot greeting, then shell / theme / font
/// (family, size) / decoration (gaps_in, gaps_out, border, radius) /
/// scrollback / close-confirm / keybinding-preset picks. Pure over injected
/// `input`, `output`, `shell_env`, `columns`, `shell_exists`, and
/// `overrides`, so the whole flow is headless-testable with piped stdin.
///
/// A step answered by a value flag ([`InitOverrides`]) is skipped entirely
/// (no prompt, no stdin read); the remaining steps prompt with their shipped
/// default and reprompt fail-closed on invalid input.
pub(crate) fn run_init_interactive(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    shell_env: Option<&str>,
    columns: Option<u16>,
    shell_exists: &dyn Fn(&str) -> bool,
    overrides: &InitOverrides,
) -> Result<InitAnswers, String> {
    let _ = write!(output, "{}", init_greeting_art(columns));
    let _ = writeln!(
        output,
        "Welcome to bitty! This wizard writes your init.lua. (Enter takes the default.)"
    );
    let candidates = init_shell_candidates(shell_env, shell_exists);
    let _ = writeln!(output, "\nShell:");
    for (index, candidate) in candidates.iter().enumerate() {
        let marker = if index == 0 { " (default)" } else { "" };
        let _ = writeln!(output, "  {}) {candidate}{marker}", index + 1);
    }
    let shell = init_ask(
        input,
        output,
        "Shell [Enter for default, number, or custom path]:",
        |line| init_parse_shell_answer(line, &candidates),
    )?;
    let theme = match &overrides.theme {
        Some(theme) => theme.clone(),
        None => init_ask(input, output, "Theme [dark]:", init_parse_theme_answer)?,
    };
    let font_family = match &overrides.font_family {
        Some(family) => family.clone(),
        None => init_ask(
            input,
            output,
            &format!(
                "Font family [{}]:",
                bitty_config::types::DEFAULT_FONT_FAMILY
            ),
            init_parse_font_family_answer,
        )?,
    };
    let font_size = match overrides.font_size {
        Some(size) => size,
        None => init_ask(
            input,
            output,
            &format!(
                "Font size in points [{}]:",
                bitty_config::types::DEFAULT_FONT_SIZE
            ),
            init_parse_font_size_answer,
        )?,
    };
    let _ = writeln!(
        output,
        "\nDecoration (logical pixels; 0 disables; see RFC-0001):"
    );
    let gaps_in = match overrides.gaps_in {
        Some(value) => value,
        None => init_ask(
            input,
            output,
            &format!(
                "gaps_in (space between panes) [{}]:",
                bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX
            ),
            |line| {
                init_parse_decoration_answer(
                    line,
                    "gaps_in",
                    bitty_config::types::MAX_DECORATION_GAP_PX,
                    bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX,
                )
            },
        )?,
    };
    let gaps_out = match overrides.gaps_out {
        Some(value) => value,
        None => init_ask(
            input,
            output,
            &format!(
                "gaps_out (space around the window edge) [{}]:",
                bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX
            ),
            |line| {
                init_parse_decoration_answer(
                    line,
                    "gaps_out",
                    bitty_config::types::MAX_DECORATION_GAP_PX,
                    bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX,
                )
            },
        )?,
    };
    let border = match overrides.border {
        Some(value) => value,
        None => init_ask(
            input,
            output,
            &format!(
                "border (frame thickness) [{}]:",
                bitty_config::types::DEFAULT_DECORATION_BORDER_PX
            ),
            |line| {
                init_parse_decoration_answer(
                    line,
                    "border",
                    bitty_config::types::MAX_DECORATION_BORDER_PX,
                    bitty_config::types::DEFAULT_DECORATION_BORDER_PX,
                )
            },
        )?,
    };
    let radius = match overrides.radius {
        Some(value) => value,
        None => init_ask(
            input,
            output,
            &format!(
                "radius (corner rounding) [{}]:",
                bitty_config::types::DEFAULT_DECORATION_RADIUS_PX
            ),
            |line| {
                init_parse_decoration_answer(
                    line,
                    "radius",
                    bitty_config::types::MAX_DECORATION_RADIUS_PX,
                    bitty_config::types::DEFAULT_DECORATION_RADIUS_PX,
                )
            },
        )?,
    };
    let _ = writeln!(output, "\nBehavior:");
    let scrollback = match overrides.scrollback {
        Some(value) => value,
        None => init_ask(
            input,
            output,
            &format!(
                "Scrollback lines [{}]:",
                bitty_config::types::TerminalConfig::default().scrollback
            ),
            init_parse_scrollback_answer,
        )?,
    };
    let close_confirm = match overrides.close_confirm {
        Some(mode) => mode,
        None => init_ask(
            input,
            output,
            "Close confirm [1 when_busy]:\n  \
             1) when_busy — confirm only while a foreground job runs (default)\n  \
             2) always — confirm every view/window close\n  \
             3) never — never confirm:",
            init_parse_close_confirm_answer,
        )?,
    };
    let key_preset = init_ask(
        input,
        output,
        "Keybindings [1 default / 2 vim]:\n  1) default — shipped Alt-as-Mod map (Alt+h/j/k/l, Alt+1..9, Alt+u/i)\n  2) vim — write that map explicitly (tweakable starting point):",
        init_parse_preset_answer,
    )?;
    Ok(InitAnswers {
        shell,
        theme,
        font_family,
        font_size,
        gaps_in,
        gaps_out,
        border,
        radius,
        scrollback,
        close_confirm,
        key_preset,
    })
}

/// Outcome of [`write_init_config`].
#[derive(Debug)]
pub(crate) struct InitWriteOutcome {
    /// File that was written.
    pub(crate) path: std::path::PathBuf,
    /// Backup of the overwritten file, if any (`<file>.lua.bak`).
    pub(crate) backup: Option<std::path::PathBuf>,
    /// True when an existing file was replaced via `--force`.
    pub(crate) updated: bool,
}

/// Why [`write_init_config`] refused or failed.
#[derive(Debug)]
pub(crate) enum InitWriteError {
    /// Usage-level refusal (exists without `--force`, generated content
    /// invalid): the caller exits 2.
    Refused(String),
    /// Filesystem failure (mkdir/read/backup/write): the caller exits 1.
    Io(String),
}

/// Writes wizard output to `target` (idempotent contract):
///
/// - The rendered content is validated via `parse_lua_config` BEFORE any
///   filesystem mutation, so the wizard never writes a file startup would
///   reject.
/// - An existing file is never overwritten without `force` ([`InitWriteError::Refused`]).
/// - With `force`, the previous bytes are copied to `<file>.lua.bak` first
///   (overwriting any older backup), then the new content is written.
/// - Parent directories are created as needed.
///
/// Total: every failure maps to [`InitWriteError`], never a panic.
pub(crate) fn write_init_config(
    target: &std::path::Path,
    content: &str,
    force: bool,
) -> Result<InitWriteOutcome, InitWriteError> {
    {
        let source = bitty_config::plan::ConfigSource::new(
            bitty_config::plan::LayerKind::User,
            Some(target.display().to_string()),
        );
        bitty_config::file::parse_lua_config(content, &source).map_err(|err| {
            InitWriteError::Refused(format!(
                "bitty init: generated config is invalid ({err}) — refusing to write"
            ))
        })?;
    }
    if target.exists() && !force {
        return Err(InitWriteError::Refused(format!(
            "bitty init: '{}' already exists (re-run with --force to overwrite; a .bak backup is kept)",
            target.display()
        )));
    }
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                InitWriteError::Io(format!(
                    "bitty init: cannot create '{}': {err}",
                    parent.display()
                ))
            })?;
        }
    }
    let mut backup = None;
    let mut updated = false;
    if target.exists() {
        let backup_path = target.with_extension("lua.bak");
        let previous = std::fs::read(target).map_err(|err| {
            InitWriteError::Io(format!(
                "bitty init: cannot read '{}': {err}",
                target.display()
            ))
        })?;
        std::fs::write(&backup_path, previous).map_err(|err| {
            InitWriteError::Io(format!(
                "bitty init: cannot write backup '{}': {err}",
                backup_path.display()
            ))
        })?;
        backup = Some(backup_path);
        updated = true;
    }
    std::fs::write(target, content).map_err(|err| {
        InitWriteError::Io(format!(
            "bitty init: cannot write '{}': {err}",
            target.display()
        ))
    })?;
    Ok(InitWriteOutcome {
        path: target.to_path_buf(),
        backup,
        updated,
    })
}

/// Short usage for `bitty init` (stdout on `--help`-style flows, stderr on
/// fail-closed exit 2).
pub(crate) fn init_usage() -> String {
    "usage: bitty init [--yes] [--force] [VALUE FLAGS] [--config PATH]\n\
     \n\
     Opt-in setup wizard (never auto-runs): mascot greeting, then shell /\n\
     theme / font family+size / decoration (gaps_in, gaps_out, border,\n\
     radius) / scrollback / close_confirm / keybinding-preset picks, then\n\
     writes the config file with shipped keys only (no secrets, no network).\n\
     \n\
     flags:\n\
     \x20 --yes     skip prompts; write sane defaults ($SHELL when clean, dark\n\
     \x20           theme, shipped font/decoration/scrollback/close_confirm,\n\
     \x20           shipped keymap defaults)\n\
     \x20 --force   overwrite an existing file (backs it up to init.lua.bak)\n\
     \n\
     value flags (answer one step, skip its prompt; validated fail-closed):\n\
     \x20 --theme NAME         preset name or alias (e.g. dark, tokyo-night)\n\
     \x20 --font-family NAME   font family (<= 128 bytes)\n\
     \x20 --font-size PTS      point size within (0, 128]\n\
     \x20 --scrollback LINES   scrollback lines within [0, 100000]\n\
     \x20 --close-confirm MODE always | when_busy | never\n\
     \x20 --gaps-in PX         pane gap within [0, 32] (decoration.gaps_in)\n\
     \x20 --gaps-out PX        edge gap within [0, 32] (decoration.gaps_out)\n\
     \x20 --border PX          frame thickness within [0, 8]\n\
     \x20 --radius PX          corner radius within [0, 16]\n\
     \n\
     target: --config PATH wins, else BITTY_CONFIG, else\n\
     $XDG_CONFIG_HOME/bitty/init.lua (fallback ~/.config/bitty/init.lua).\n\
     Without --force an existing file is never overwritten (exit 2).\n\
     Without a TTY on stdin, prompts are skipped only with --yes (exit 2\n\
     otherwise, never a hang). Re-runs are idempotent: same answers write\n\
     the same file. Validate any file any time with `bitty config check`."
        .to_string()
}

/// Hermetic environment for [`run_init_subcommand_with_io`]: every value the
/// dispatch reads is injected so tests touch neither process env, the real
/// TTY, nor the host filesystem root policy.
pub(crate) struct InitEnv<'a> {
    /// Stands in for `BITTY_CONFIG`.
    pub(crate) bitty_config: Option<&'a str>,
    /// Stands in for `SHELL`.
    pub(crate) shell: Option<&'a str>,
    /// Stands in for `XDG_CONFIG_HOME`.
    pub(crate) xdg_config_home: Option<&'a str>,
    /// Stands in for `HOME` (XDG fallback root).
    pub(crate) home: Option<&'a str>,
    /// Stands in for `COLUMNS` (greeting width).
    pub(crate) columns: Option<u16>,
    /// Whether stdin is a TTY; `false` without `--yes` fails closed.
    pub(crate) stdin_is_tty: bool,
}

/// Runs `bitty init`; returns the process exit code.
///
/// - `0`: wrote the file (prints the target plus what changed).
/// - `2`: usage-level refusal (unexpected args, invalid value flag, non-TTY
///   without `--yes`, existing file without `--force`) — nothing was written.
/// - `1`: aborted prompts (EOF / too many retries) or filesystem failure.
pub(crate) fn run_init_subcommand(args: &Args) -> i32 {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let shell_env = std::env::var("SHELL").ok();
    run_init_subcommand_with_env(args, bitty_config_env.as_deref(), shell_env.as_deref())
}

/// [`run_init_subcommand`] with injected `BITTY_CONFIG`/`SHELL` values; the
/// remaining environment (XDG root, COLUMNS, TTY-ness) is read live. Tests
/// use [`run_init_subcommand_with_io`] for full hermeticity.
pub(crate) fn run_init_subcommand_with_env(
    args: &Args,
    bitty_config_env: Option<&str>,
    shell_env: Option<&str>,
) -> i32 {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let columns = init_columns_from_env(std::env::var("COLUMNS").ok().as_deref());
    let env = InitEnv {
        bitty_config: bitty_config_env,
        shell: shell_env,
        xdg_config_home: xdg.as_deref(),
        home: home.as_deref(),
        columns,
        stdin_is_tty: std::io::stdin().is_terminal(),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_init_subcommand_with_io(args, &env, &mut stdin.lock(), &mut stdout.lock())
}

/// [`run_init_subcommand`] over injected IO and environment. `input` is read
/// only by the interactive wizard; `output` receives prompts only. Writes go
/// to the resolved config path and diagnostics to stderr.
pub(crate) fn run_init_subcommand_with_io(
    args: &Args,
    env: &InitEnv<'_>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    if !args.init_args.is_empty() {
        eprintln!(
            "bitty init: unexpected argument '{}'\n{}",
            args.init_args[0],
            init_usage()
        );
        return 2;
    }
    // Validate every explicit value flag before touching the filesystem or
    // prompting: an invalid flag is a usage error, never a silent default.
    let overrides = match init_overrides_from_args(args) {
        Ok(overrides) => overrides,
        Err(message) => {
            eprintln!("bitty init: {message}\n{}", init_usage());
            return 2;
        }
    };
    // BITTY_CONFIG env participates exactly like --config (CLI wins), same
    // as the `config` subcommand and startup.
    let explicit =
        bitty_config::file::resolve_config_explicit(args.config_path.as_deref(), env.bitty_config);
    let target = match bitty_config::file::probe_config_path_with_env(
        explicit.as_deref(),
        env.xdg_config_home,
        env.home,
        &|path| path.exists(),
    ) {
        Some(probed) => probed.path,
        None => {
            eprintln!("bitty init: no config root ($XDG_CONFIG_HOME or $HOME unset)");
            return 2;
        }
    };
    let answers = if args.init_yes {
        let mut answers = init_yes_defaults(env.shell);
        init_apply_overrides(&mut answers, &overrides);
        // Warn when $SHELL existed but was unusable, so the omission is
        // never silent (the written file simply leaves `shell` unset and
        // startup falls back to /bin/sh).
        let raw = env.shell.map(str::trim).unwrap_or_default();
        if !raw.is_empty() && answers.shell.is_none() {
            eprintln!("bitty init: ignoring unusable $SHELL {raw:?}; leaving shell unset");
        }
        answers
    } else {
        // Fail closed instead of blocking on a pipe that will never answer.
        if !env.stdin_is_tty {
            eprintln!(
                "bitty init: stdin is not a terminal; re-run with --yes (and value flags) for non-interactive defaults\n{}",
                init_usage()
            );
            return 2;
        }
        match run_init_interactive(
            input,
            output,
            env.shell,
            env.columns,
            &|path| std::path::Path::new(path).exists(),
            &overrides,
        ) {
            Ok(answers) => answers,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    };
    let content = render_init_lua(&answers);
    match write_init_config(&target, &content, args.init_force) {
        Ok(outcome) => {
            if outcome.updated {
                match &outcome.backup {
                    Some(backup) => println!(
                        "bitty init: backed up existing config to '{}'",
                        backup.display()
                    ),
                    None => println!("bitty init: replaced existing config"),
                }
            }
            let shell = answers
                .shell
                .as_deref()
                .map_or("(default)".to_string(), |s| format!("\"{s}\""));
            let keymaps = match answers.key_preset {
                InitKeyPreset::Default => "default (shipped)".to_string(),
                InitKeyPreset::Vim => format!(
                    "vim ({} explicit entries)",
                    bitty_config::keymap::DEFAULT_KEYMAPS.len()
                ),
            };
            println!(
                "bitty init: wrote '{}' (theme=\"{}\", shell={shell}, font={} {}pt, \
                 scrollback={}, close_confirm={}, keymaps={keymaps})",
                outcome.path.display(),
                answers.theme,
                answers.font_family,
                answers.font_size,
                answers.scrollback,
                answers.close_confirm.as_str(),
            );
            println!("bitty init: validate any time with `bitty config check`");
            0
        }
        Err(InitWriteError::Refused(message)) => {
            eprintln!("{message}");
            2
        }
        Err(InitWriteError::Io(message)) => {
            eprintln!("{message}");
            1
        }
    }
}
