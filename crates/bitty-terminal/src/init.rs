//! `bitty init` opt-in setup wizard (#243, CTX-0149).

use std::io::IsTerminal as _;

use crate::cli::Args;
use crate::spawn::FALLBACK_SHELL;

/// Wizard greeting art: thin alias over the single-owned mascot module
/// ([`crate::mascot`], issue #1318) so the `bitty init` greeting and the
/// startup splash can never disagree. The art stays vendored byte-identical
/// from the workspace asset `recording/bitty-mascot/ascii/bitty_ascii.txt`
/// (DEC-0002): pure text so it renders anywhere stdout goes, including
/// piped headless runs; the sixel/block variants stay out of the binary.
/// (Test-only: the wizard itself goes through [`init_greeting_art`].)
#[cfg(test)]
pub(crate) use crate::mascot::MASCOT_ART as INIT_MASCOT_ART;
/// One-line fallback when the window is too narrow for the art: fail closed
/// with an honest line instead of a wrapped mess (see [`crate::mascot`]).
#[cfg(test)]
pub(crate) use crate::mascot::MASCOT_FALLBACK as INIT_MASCOT_FALLBACK;
/// Greeting art for a known-or-unknown window width (see [`crate::mascot`]).
pub(crate) use crate::mascot::mascot_art_for_width as init_greeting_art;
/// Width of the vendored art in columns (see [`crate::mascot`]).
/// (Test-only: runtime code goes through [`init_greeting_art`].)
#[cfg(test)]
pub(crate) use crate::mascot::mascot_width as init_mascot_width;

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

/// Wizard line-editing escape outcome: one parsed `ESC`-led sequence mapped
/// to an editing action. The tables mirror the terminal input encoder
/// (`bitty-platform::keyboard`: [`encode_named_key`] legacy sequences plus
/// the kitty [`ext_functional_key`] codes framed by
/// [`encode_key_event_kitty_protocol`]) so every byte pattern the terminal
/// can emit for the listed keys decodes here instead of landing in answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitEscapeAction {
    /// Cursor one cell left.
    Left,
    /// Cursor one cell right.
    Right,
    /// Recall older history entry.
    Up,
    /// Recall newer history entry (or restore the saved buffer).
    Down,
    /// Cursor to the start of the buffer.
    Home,
    /// Cursor to the end of the buffer.
    End,
    /// Delete the cell under the cursor (DEL / `CSI 3 ~` / kitty `3 u`).
    DeleteAt,
    /// Delete the cell before the cursor (kitty `127 u`).
    Backspace,
    /// Insert one space (kitty `32 u`).
    InsertSpace,
    /// kitty `13 u` (Enter): submit the buffer immediately.
    Submit,
    /// Consumed bytes carry no editing meaning (INS, F-keys, PageUp/Down,
    /// unknown CSI/SS3/OSC, bare ESC, Alt prefix): drop, never insert.
    Ignore,
}

/// Parses one `ESC`-led sequence at `raw[pos]` (`raw[pos] == 0x1b`).
/// Returns the editing action plus the number of bytes consumed.
/// Never fails: truncated or unknown sequences consume through the line end
/// (or their terminator) as [`InitEscapeAction::Ignore`] so raw `ESC` bytes
/// can never leak into an answer.
fn init_parse_escape_at(raw: &[u8], pos: usize) -> (InitEscapeAction, usize) {
    use InitEscapeAction as A;
    let len = raw.len();
    if pos + 1 >= len {
        return (A::Ignore, 1);
    }
    let second = raw[pos + 1];
    // CSI: `ESC [ ... final(0x40..=0x7e)`.
    if second == b'[' {
        let mut end = pos + 2;
        while end < len && !(0x40..=0x7e).contains(&raw[end]) {
            // Bound the scan so a pasted megabyte without a final byte
            // cannot grow the parse: past 32 parameter bytes the rest of
            // the line is one ignored sequence.
            if end - (pos + 2) >= 32 {
                return (A::Ignore, len - pos);
            }
            end += 1;
        }
        if end >= len {
            return (A::Ignore, len - pos);
        }
        let final_byte = raw[end];
        let consumed = end - pos + 1;
        match final_byte {
            b'A' => return (A::Up, consumed),
            b'B' => return (A::Down, consumed),
            b'C' => return (A::Right, consumed),
            b'D' => return (A::Left, consumed),
            b'H' => return (A::Home, consumed),
            b'F' => return (A::End, consumed),
            b'P' | b'Q' | b'S' => return (A::Ignore, consumed),
            b'~' => {
                // `CSI <n> [;...] ~`: first number selects the key.
                // `1`/`7` Home, `4`/`8` End, `2` INS (ignore), `3` DEL,
                // `5`/`6` PageUp/PageDown (ignore), `11`/`12` F1/F2
                // (ignore); anything else (including bracketed-paste
                // `200`/`201`) is ignored.
                let params = &raw[pos + 2..end];
                let mut first: u32 = 0;
                let mut has_digit = false;
                for &b in params {
                    if b.is_ascii_digit() {
                        has_digit = true;
                        first = first.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                    } else {
                        break;
                    }
                }
                if !has_digit {
                    return (A::Ignore, consumed);
                }
                match first {
                    1 | 7 => return (A::Home, consumed),
                    4 | 8 => return (A::End, consumed),
                    3 => return (A::DeleteAt, consumed),
                    _ => return (A::Ignore, consumed),
                }
            }
            b'u' => {
                // Kitty `CSI <code> [;...] u`: first code selects the key
                // (`ext_functional_key` + `encode_key_event_kitty_protocol`
                // framing: `27` ESC, `13` Enter, `9` Tab, `127`
                // Backspace, `32` Space, `2` INS, `3` DEL, `11`/`12`
                // F1/F2; arrows use trailers `A`/`B`/`C`/`D` above).
                let params = &raw[pos + 2..end];
                let mut first: u32 = 0;
                let mut has_digit = false;
                for &b in params {
                    if b.is_ascii_digit() {
                        has_digit = true;
                        first = first.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                    } else {
                        break;
                    }
                }
                if !has_digit {
                    return (A::Ignore, consumed);
                }
                match first {
                    13 => return (A::Submit, consumed),
                    127 => return (A::Backspace, consumed),
                    32 => return (A::InsertSpace, consumed),
                    3 => return (A::DeleteAt, consumed),
                    _ => return (A::Ignore, consumed),
                }
            }
            _ => return (A::Ignore, consumed),
        }
    }
    // SS3: `ESC O <letter>` (application cursor + F1-F4 legacy).
    if second == b'O' {
        if pos + 2 >= len {
            // Bare `ESC O` at the end of the line is Alt+O, not SS3:
            // drop only the `ESC` and let `O` insert normally.
            return (A::Ignore, 1);
        }
        match raw[pos + 2] {
            b'A' => return (A::Up, 3),
            b'B' => return (A::Down, 3),
            b'C' => return (A::Right, 3),
            b'D' => return (A::Left, 3),
            b'H' => return (A::Home, 3),
            b'F' => return (A::End, 3),
            b'P' | b'Q' | b'R' | b'S' => return (A::Ignore, 3),
            _ => return (A::Ignore, 1),
        }
    }
    // OSC / DCS / SOS / PM / APC introductions (`ESC ] P X ^ _`):
    // consume through `BEL` or `ESC \` so a pasted OSC never leaks its
    // payload into the answer.
    if matches!(second, b']' | b'P' | b'X' | b'^' | b'_') {
        let mut i = pos + 2;
        while i < len {
            if raw[i] == 0x07 {
                return (A::Ignore, i - pos + 1);
            }
            if raw[i] == 0x1b && i + 1 < len && raw[i + 1] == b'\\' {
                return (A::Ignore, i - pos + 2);
            }
            // Bound the scan: past 4 KiB the rest of the line is one
            // ignored sequence.
            if i - (pos + 2) >= INIT_MAX_LINE_BYTES {
                return (A::Ignore, len - pos);
            }
            i += 1;
        }
        return (A::Ignore, len - pos);
    }
    // Charset / single-shift introductions (`ESC (`, `ESC )`, `ESC #`, ...):
    // three-byte ignored sequences, never Alt prefixes.
    if matches!(second, b'(' | b')' | b'#' | b'%') {
        let take = (len - pos).min(3);
        return (A::Ignore, take);
    }
    // Anything else (`ESC` + printable) is an Alt prefix
    // (`metaSendsEscape`): drop only the `ESC` so the following character
    // inserts without its modifier. Non-printable followers drop the `ESC`
    // too; the follower itself is handled (dropped) next iteration.
    (A::Ignore, 1)
}

/// Decodes one raw wizard line (without the trailing newline) through the
/// terminal key tables into the final answer.
///
/// Editing model (headless-pure over the injected bytes, so piped-stdin
/// tests drive it byte-identically to a canonical-mode TTY whose kernel
/// delivered the same `ESC [` bytes for the listed keys):
///
/// - Printable ASCII and valid UTF-8 insert at the cursor.
/// - `BS`/`DEL` (`0x08`/`0x7f`) delete before the cursor; `CSI 3 ~` (and
///   kitty `3 u`) delete under the cursor.
/// - Arrows move (`CSI A`/`B`/`C`/`D` with any modifiers, application
///   `SS3` variants, kitty `CSI u` arrow trailers included); `Left`/`Right`
///   at the edges stay.
/// - `Home` (`CSI H`, `SS3 H`, `CSI 1 ~`/`7 ~`) and `End` (`CSI F`,
///   `SS3 F`, `CSI 4 ~`/`8 ~`) jump.
/// - `Up`/`Down` walk `history` (previous successful wizard answers in this
///   run) with the current buffer saved across the walk; empty history or
///   the walk ends stay gracefully.
/// - `INS` (`CSI 2 ~`), `F1`/`F2` (`SS3 P`/`Q`, `CSI 11 ~`/`12 ~`, kitty
///   `P`/`Q` trailers), PageUp/PageDown, bare `ESC`, unknown CSI/SS3/OSC,
///   Alt prefixes, and every other control byte are ignored.
///
/// Total: raw `ESC` (`0x1b`) is never inserted, so `^[[A`-style text can
/// never land in an answer. The result is truncated to
/// [`INIT_MAX_LINE_BYTES`] on a character boundary.
pub(crate) fn init_decode_line_bytes(raw: &[u8], history: &[String]) -> String {
    let mut buffer: Vec<char> = Vec::new();
    let mut cursor: usize = 0;
    let mut history_index: Option<usize> = None;
    let mut saved: Vec<char> = Vec::new();
    let mut i = 0;
    let len = raw.len();
    while i < len {
        let byte = raw[i];
        if byte == 0x1b {
            let (action, consumed) = init_parse_escape_at(raw, i);
            match action {
                InitEscapeAction::Left => {
                    cursor = cursor.saturating_sub(1);
                }
                InitEscapeAction::Right => {
                    cursor = cursor.saturating_add(1).min(buffer.len());
                }
                InitEscapeAction::Up => {
                    if !history.is_empty() {
                        match history_index {
                            None => {
                                saved = buffer.clone();
                                let index = history.len() - 1;
                                buffer = history[index].chars().collect();
                                cursor = buffer.len();
                                history_index = Some(index);
                            }
                            Some(0) => {}
                            Some(index) => {
                                let next = index - 1;
                                buffer = history[next].chars().collect();
                                cursor = buffer.len();
                                history_index = Some(next);
                            }
                        }
                    }
                }
                InitEscapeAction::Down => match history_index {
                    None => {}
                    Some(index) if index + 1 >= history.len() => {
                        buffer = saved.clone();
                        cursor = buffer.len();
                        history_index = None;
                    }
                    Some(index) => {
                        let next = index + 1;
                        buffer = history[next].chars().collect();
                        cursor = buffer.len();
                        history_index = Some(next);
                    }
                },
                InitEscapeAction::Home => cursor = 0,
                InitEscapeAction::End => cursor = buffer.len(),
                InitEscapeAction::DeleteAt => {
                    if cursor < buffer.len() {
                        buffer.remove(cursor);
                    }
                }
                InitEscapeAction::Backspace => {
                    if cursor > 0 {
                        cursor = cursor.saturating_sub(1);
                        buffer.remove(cursor);
                    }
                }
                InitEscapeAction::InsertSpace => {
                    buffer.insert(cursor, ' ');
                    cursor += 1;
                }
                InitEscapeAction::Submit => break,
                InitEscapeAction::Ignore => {}
            }
            i += consumed.max(1);
            continue;
        }
        if byte == 0x7f || byte == 0x08 {
            if cursor > 0 {
                cursor = cursor.saturating_sub(1);
                buffer.remove(cursor);
            }
            i += 1;
            continue;
        }
        if byte < 0x20 {
            // Remaining C0 controls (`\r`, `\t`, `Ctrl+letter` bytes,
            // `BEL`, ...) carry no answer text: drop so they can never
            // land in an answer (validators reprompt on nothing, not on
            // garbage).
            i += 1;
            continue;
        }
        if byte < 0x80 {
            buffer.insert(cursor, byte as char);
            cursor += 1;
            i += 1;
            continue;
        }
        // Multi-byte UTF-8: decode one character from the leading byte.
        let width = if byte >= 0xf0 {
            4
        } else if byte >= 0xe0 {
            3
        } else if byte >= 0xc0 {
            2
        } else {
            // Stray continuation byte: drop.
            i += 1;
            continue;
        };
        if i + width > len {
            break;
        }
        match std::str::from_utf8(&raw[i..i + width]) {
            Ok(text) => {
                if let Some(ch) = text.chars().next() {
                    buffer.insert(cursor, ch);
                    cursor += 1;
                }
                i += width;
            }
            Err(_) => {
                i += 1;
            }
        }
    }
    let mut out: String = buffer.into_iter().collect();
    if out.len() > INIT_MAX_LINE_BYTES {
        let mut boundary = INIT_MAX_LINE_BYTES;
        while boundary > 0 && !out.is_char_boundary(boundary) {
            boundary -= 1;
        }
        out.truncate(boundary);
    }
    out
}

/// Reads one stdin line without the trailing newline. `None` on EOF or I/O
/// error (the wizard aborts rather than guessing). Overlong lines are
/// truncated to [`INIT_MAX_LINE_BYTES`] so a pasted megabyte cannot grow
/// the answer buffer.
///
/// Key-aware: the raw bytes (including the `ESC [` sequences a
/// canonical-mode TTY delivers for arrows/DEL/INS/HOME/END/F-keys) run
/// through [`init_decode_line_bytes`] with an empty history, so raw `ESC`
/// bytes never land in the returned answer.
#[allow(dead_code)]
pub(crate) fn init_read_line(input: &mut dyn std::io::BufRead) -> Option<String> {
    init_read_line_with_history(input, &[]).map(|(_, line)| line)
}

/// Whether the decoded answer must be redrawn to `output` so the display
/// matches the submitted value.
///
/// On a canonical-mode TTY the kernel echoes the raw typed bytes before this
/// code ever runs: arrow/history/erase escape sequences (`ESC [ D`, ...),
/// dropped control bytes, and silently truncated tails all stay on screen
/// while [`init_decode_line_bytes`] submits something else. Byte-inequality
/// between the cooked line and the decoded answer is exactly that case, so
/// a redraw is needed; identical bytes mean the kernel echo already shows
/// the submitted value and nothing is printed.
pub(crate) fn init_line_needs_redraw(raw: &[u8], decoded: &str) -> bool {
    raw != decoded.as_bytes()
}

/// [`init_read_line`] with `Up`/`Down` history recall over the previous
/// successful wizard answers in this run; empty history stays gracefully.
/// History itself is only appended by [`init_ask`] on successful parses —
/// reading never mutates it.
///
/// Returns the cooked stdin bytes alongside the decoded answer so the caller
/// can redraw the display when decoding changed what the kernel echoed (see
/// [`init_line_needs_redraw`]).
pub(crate) fn init_read_line_with_history(
    input: &mut dyn std::io::BufRead,
    history: &[String],
) -> Option<(Vec<u8>, String)> {
    let mut raw: Vec<u8> = Vec::new();
    match input.read_until(b'\n', &mut raw) {
        Ok(0) => None,
        Ok(_) => {
            if raw.ends_with(b"\n") {
                raw.pop();
            }
            if raw.ends_with(b"\r") {
                raw.pop();
            }
            // Bound the raw scan: escape overhead past the answer cap is
            // dropped before decoding (the decoded answer is capped again
            // in `init_decode_line_bytes`).
            if raw.len() > INIT_MAX_LINE_BYTES + 256 {
                raw.truncate(INIT_MAX_LINE_BYTES + 256);
            }
            let line = init_decode_line_bytes(&raw, history);
            Some((raw, line))
        }
        Err(_) => None,
    }
}

/// Asks one wizard step: prints `prompt`, reads a line, parses it.
/// Reprompts up to [`INIT_MAX_ATTEMPTS`] on parse errors, then aborts;
/// EOF aborts immediately. Prompts go to `output` (stdout at runtime) so
/// piped-stdin runs still show the questions.
///
/// Whenever decoding changed what the canonical-TTY kernel echoed (see
/// [`init_line_needs_redraw`]), the decoded value is redrawn to `output`
/// before parsing, so the display always carries the submitted answer
/// instead of the raw echo. A redraw that cannot be written aborts the
/// prompt fail-closed rather than accepting a value the user never saw.
///
/// `history` carries the previous successful answers in this wizard run
/// for `Up`/`Down` recall (appended on success only; failed attempts never
/// pollute recall).
pub(crate) fn init_ask<T>(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    prompt: &str,
    parse: impl Fn(&str) -> Result<T, String>,
    history: &mut Vec<String>,
) -> Result<T, String> {
    for attempt in 1..=INIT_MAX_ATTEMPTS {
        let _ = writeln!(output, "{prompt}");
        let _ = output.flush();
        match init_read_line_with_history(input, history) {
            Some((raw, line)) => {
                if init_line_needs_redraw(&raw, &line)
                    && (writeln!(output, "  (read as {line:?})").is_err()
                        || output.flush().is_err())
                {
                    return Err("bitty init: aborted (output error)".to_string());
                }
                match parse(&line) {
                    Ok(value) => {
                        if !line.is_empty() {
                            history.push(line);
                        }
                        return Ok(value);
                    }
                    Err(err) => {
                        let _ = writeln!(
                            output,
                            "  ({err} — try again [{attempt}/{INIT_MAX_ATTEMPTS}])"
                        );
                    }
                }
            }
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
    // Per-run recall for `Up`/`Down`: previous successful answers in this
    // wizard invocation (failed attempts never pollute it).
    let mut history: Vec<String> = Vec::new();
    let shell = init_ask(
        input,
        output,
        "Shell [Enter for default, number, or custom path]:",
        |line| init_parse_shell_answer(line, &candidates),
        &mut history,
    )?;
    let theme = match &overrides.theme {
        Some(theme) => theme.clone(),
        None => init_ask(
            input,
            output,
            "Theme [dark]:",
            init_parse_theme_answer,
            &mut history,
        )?,
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
            &mut history,
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
            &mut history,
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
            &mut history,
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
            &mut history,
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
            &mut history,
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
            &mut history,
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
            &mut history,
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
            &mut history,
        )?,
    };
    let key_preset = init_ask(
        input,
        output,
        "Keybindings [1 default / 2 vim]:\n  1) default — shipped Alt-as-Mod map (Alt+h/j/k/l, Alt+1..9, Alt+u/i)\n  2) vim — write that map explicitly (tweakable starting point):",
        init_parse_preset_answer,
        &mut history,
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
     Shell integration (Tab completion + OSC 7/133 prompt hooks) is separate:\n\
     after init, run `bitty shell-init --help` and eval the line for your shell.\n\
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
            println!(
                "bitty init: wire Tab completion + prompt hooks with `eval \"$(bitty shell-init bash)\"` (pick your shell; `bitty shell-init --help` lists all five)"
            );
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
