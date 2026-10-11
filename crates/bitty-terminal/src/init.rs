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
    /// `--plugins a,b` answer (CTX-1132, #1913): explicit first-batch bundled
    /// ids. `None` means absent (prompt in TTY, none with `--yes`); `Some`
    /// (possibly empty via `--no-plugins`) skips the prompt.
    pub(crate) plugins: Option<Vec<String>>,
    /// `--with-components` / `--no-*` answer (CTX-1132, #1913): component-seed
    /// opt-in. `None` means absent (prompt when plugins are selected in TTY,
    /// off with `--yes`); `Some` skips the prompt.
    pub(crate) with_components: Option<bool>,
    /// `--print-shell-init` / `--no-*` answer (CTX-1132, #1913): shell-init
    /// handoff. `None` means absent (default prints); `Some(false)` suppresses.
    pub(crate) print_shell_init: Option<bool>,
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
    // CTX-1132 (#1913): `--no-plugins` wins over `--plugins` (explicit none
    // for scripted setups); otherwise `--plugins` validates fail-closed
    // against the bundled source.
    if args.init_no_plugins {
        out.plugins = Some(Vec::new());
    } else if let Some(raw) = args.init_plugins.as_deref() {
        out.plugins = Some(init_parse_plugins_flag(raw)?);
    }
    if let Some(value) = args.init_with_components {
        out.with_components = Some(value);
    }
    if let Some(value) = args.init_print_shell_init {
        out.print_shell_init = Some(value);
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

/// Parses one first-batch plugin-selection answer (CTX-1132, #1913).
///
/// Empty means none. Otherwise a comma-separated list of bundled plugin ids,
/// each validated against the same bundled source `plugin install <id>` uses
/// ([`bitty_plugin_host::bundled::bundled_manifest_for`], no duplicate list):
/// unknown ids fail closed with the valid bundled ids as a hint (same shape
/// as `inspect` diagnostics). Duplicates collapse, order is catalog order.
/// English-only, no network, no secrets.
pub(crate) fn init_parse_plugins_answer(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut seen: Vec<String> = Vec::new();
    for part in trimmed.split(',') {
        let id = part.trim();
        if id.is_empty() {
            continue;
        }
        if id.len() > 256 || id.contains('\0') {
            return Err(format!("unknown plugin '{id}'"));
        }
        let Some(manifest) = bitty_plugin_host::bundled::bundled_manifest_for(id) else {
            let valid = bitty_plugin_host::bundled::bundled_ids_sorted().join(", ");
            let hint = if valid.is_empty() {
                "(none)".to_string()
            } else {
                valid
            };
            return Err(format!("unknown plugin '{id}' (bundled ids: {hint})"));
        };
        // Validate the manifest the same way `plugin install` does, so a
        // corrupt bundled manifest fails closed here instead of recording a
        // hash that install would reject.
        manifest
            .validate()
            .map_err(|error| format!("bundled manifest '{id}' is invalid: {error}"))?;
        let canonical = manifest.identity.id.as_str().to_string();
        if !seen.iter().any(|seen| seen == &canonical) {
            seen.push(canonical);
        }
    }
    // Deterministic catalog order (same as `bundled_ids_sorted`), so
    // `--plugins b,a` and interactive `a,b` record identically.
    let order = bitty_plugin_host::bundled::bundled_ids_sorted();
    seen.sort_by_key(|id| {
        order
            .iter()
            .position(|ordered| ordered == id)
            .unwrap_or(usize::MAX)
    });
    Ok(seen)
}

/// Parses one `--plugins` flag value (CTX-1132, #1913): like
/// [`init_parse_plugins_answer`] but an empty value fails closed (a missing
/// flag value is a usage error, never a silent none; use `--no-plugins` for
/// explicit none).
pub(crate) fn init_parse_plugins_flag(raw: &str) -> Result<Vec<String>, String> {
    if raw.trim().is_empty() {
        return Err("--plugins needs a comma-separated bundled id list".to_string());
    }
    init_parse_plugins_answer(raw)
}

/// Parses one yes/no opt-in answer (component-seed and plugin-fetch gates).
/// Empty means no (fail-closed default); `y`/`yes` means yes, `n`/`no`/empty
/// means no; anything else reprompts.
pub(crate) fn init_parse_yes_no_answer(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "n" | "no" => Ok(false),
        "y" | "yes" => Ok(true),
        _ => Err("answer y or n".to_string()),
    }
}

/// Maps a shell path or name to the canonical `shell-init` shell name
/// (CTX-1132, #1913).
///
/// Takes the basename (`/bin/bash` -> `bash`, `pwsh` stays `pwsh`), then
/// resolves through the same [`crate::completion::CompletionShell`] table
/// `shell-init` uses, so the handoff line and `shell-init` can never disagree
/// on names. Returns the canonical `shell-init` spelling (`bash`, `zsh`,
/// `fish`, `powershell`, `nushell`).
pub(crate) fn init_shell_name_from_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    if base.is_empty() {
        return None;
    }
    // Strip a trailing `.exe` so Windows shell paths still map.
    let stem = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(base);
    let shell = crate::completion::CompletionShell::parse(stem)?;
    let canonical = match shell {
        crate::completion::CompletionShell::Bash => "bash",
        crate::completion::CompletionShell::Zsh => "zsh",
        crate::completion::CompletionShell::Fish => "fish",
        crate::completion::CompletionShell::Powershell => "powershell",
        crate::completion::CompletionShell::Nushell => "nushell",
    };
    Some(canonical.to_string())
}

/// Detects the shell name for the handoff (CTX-1132, #1913): the wizard's
/// chosen shell wins, else `$SHELL`, else `bash` fallback. Never reads the
/// environment itself (pure over injected values, English-only).
pub(crate) fn init_detect_shell_for_handoff(
    chosen_shell: Option<&str>,
    shell_env: Option<&str>,
) -> String {
    if let Some(chosen) = chosen_shell {
        if let Some(name) = init_shell_name_from_path(chosen) {
            return name;
        }
    }
    if let Some(env) = shell_env {
        if let Some(name) = init_shell_name_from_path(env) {
            return name;
        }
    }
    "bash".to_string()
}

/// Renders the exact shell-integration handoff line for a shell name
/// (CTX-1132, #1913; PRINT, never eval).
///
/// `bash`/`zsh` use the `eval "$(bitty shell-init <shell>)"` form; the other
/// shells use their documented wiring from `shell-init --help` (fish sources,
/// PowerShell invokes, Nushell saves once then sources), so the printed line
/// is runnable verbatim in the named shell.
pub(crate) fn init_shell_init_line(shell: &str) -> String {
    match shell.trim().to_ascii_lowercase().as_str() {
        "fish" => "bitty shell-init fish | source".to_string(),
        "powershell" | "pwsh" => {
            "bitty shell-init powershell | Out-String | Invoke-Expression".to_string()
        }
        "nushell" | "nu" => {
            "bitty shell-init nushell | save -f ~/.config/nushell/bitty-shell-init.nu".to_string()
        }
        "zsh" => "eval \"$(bitty shell-init zsh)\"".to_string(),
        _ => "eval \"$(bitty shell-init bash)\"".to_string(),
    }
}

/// Second Nushell setup step (CTX-1132, #1913; CodeRabbit 4241676519).
///
/// Nushell parses `source` paths before `save -f` runs, so the save command
/// from [`init_shell_init_line`] alone leaves integration inactive. The
/// dispatch appends this hint for `nushell` only (other shells get `""`),
/// keeping their guidance unchanged.
pub(crate) fn init_shell_source_hint(shell: &str) -> &'static str {
    match shell.trim().to_ascii_lowercase().as_str() {
        "nushell" | "nu" => {
            " Then add `source ~/.config/nushell/bitty-shell-init.nu` to config.nu."
        }
        _ => "",
    }
}

/// Declared native components for a bundled plugin id (CTX-1132, #1913).
///
/// Current bundled manifests declare no `[components]` section, so this
/// returns empty today (the component offer then prints a nothing-to-do
/// line). Kept as a function (not a constant) so a future manifest with a
/// `[components]` declaration flows into the offer without touching the
/// wizard: when that lands, return its component names here and the existing
/// offer installs the missing ones under the same opt-in gate.
pub(crate) fn init_bundled_plugin_components(id: &str) -> Vec<String> {
    let _ = id;
    Vec::new()
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
///
/// Retained for the bare-input contract and the output-error test; the
/// interactive wizard now prompts through [`init_ask_option`].
#[allow(dead_code)]
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
                // Esc cancels here too (see `init_ask_option`): bare `ESC`
                // aborts the prompt instead of parsing an empty answer.
                if raw == [0x1b] {
                    return Err("bitty init: aborted (cancelled)".to_string());
                }
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

/// One entry in an init option list (phase 1 of #1806).
///
/// `label` is shown verbatim (so rows stay greppable with and without
/// color); `value_text` is the text fed to the step parser when this entry
/// is selected via Enter or a number fast path. `style_fg`/`style_bg` carry
/// an optional preview style from the data model (theme presets); entries
/// without style fall back to a colored selection marker.
pub(crate) struct InitOptionItem {
    /// Display label (verbatim, greppable).
    pub(crate) label: String,
    /// Parser input selected by this entry.
    pub(crate) value_text: String,
    /// Optional preview foreground (theme presets).
    pub(crate) style_fg: Option<[u8; 3]>,
    /// Optional preview background (theme presets).
    pub(crate) style_bg: Option<[u8; 3]>,
}

/// Pure color decision for tests: mirrors [`crate::color::cli_color_enabled_impl`]
/// without touching the environment or probing the terminal.
pub(crate) fn init_color_enabled_impl(
    no_color_flag: bool,
    no_color_env: bool,
    term_is_dumb: bool,
    stdout_is_tty: bool,
) -> bool {
    crate::color::cli_color_enabled_impl(no_color_flag, no_color_env, term_is_dumb, stdout_is_tty)
}

/// Whether ANSI emphasis is allowed in init option lists.
///
/// Unified CLI gate only, no new mechanism: explicit `--no-color` (any of
/// the shared per-verb flags, set in lockstep for a pre-word flag),
/// `NO_COLOR`, `TERM=dumb`, or piped stdout disables. Mirrors
/// [`crate::color::cli_color_enabled`].
pub(crate) fn init_color_enabled(no_color_flag: bool) -> bool {
    let no_color_env = std::env::var("NO_COLOR").is_ok();
    let term_is_dumb =
        matches!(std::env::var("TERM"), Ok(term) if term.trim().eq_ignore_ascii_case("dumb"));
    let stdout_is_tty = std::io::stdout().is_terminal();
    init_color_enabled_impl(no_color_flag, no_color_env, term_is_dumb, stdout_is_tty)
}

/// Renders one option label: theme entries preview in their own style
/// (truecolor foreground on background) when `color` allows; other selected
/// entries render bold; everything else (including all output when `color`
/// is false) stays plain so piped output stays byte-identical.
fn init_style_label(item: &InitOptionItem, selected: bool, color: bool) -> String {
    if color {
        if let (Some(fg), Some(bg)) = (item.style_fg, item.style_bg) {
            return format!(
                "\u{1b}[38;2;{};{};{}m\u{1b}[48;2;{};{};{}m{}\u{1b}[0m",
                fg[0], fg[1], fg[2], bg[0], bg[1], bg[2], item.label
            );
        }
        if selected {
            return format!("\u{1b}[1m{}\u{1b}[0m", item.label);
        }
    }
    item.label.clone()
}

/// Option-list navigation direction (phase 1 of #1806).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitOptionNav {
    /// Move the cursor one entry up (wraps).
    Up,
    /// Move the cursor one entry down (wraps).
    Down,
}

/// Detects one pure navigation input for [`init_ask_option`].
///
/// `j`/`J` move down, `k`/`K` move up; a raw line that is exactly one
/// `ESC`-led sequence mapping to Up/Down via [`init_parse_escape_at`]
/// (arrows with any modifiers, application-cursor variants) navigates too.
/// Reuses the existing escape parser instead of reimplementing it; anything
/// else (including `h`/`l`, Left/Right, typed values) is not navigation and
/// falls through to the normal decode/parse path.
fn init_option_nav(raw: &[u8], trimmed: &str) -> Option<InitOptionNav> {
    if trimmed == "j" || trimmed == "J" {
        return Some(InitOptionNav::Down);
    }
    if trimmed == "k" || trimmed == "K" {
        return Some(InitOptionNav::Up);
    }
    if raw.is_empty() || raw[0] != 0x1b {
        return None;
    }
    let (action, consumed) = init_parse_escape_at(raw, 0);
    if consumed != raw.len() {
        return None;
    }
    match action {
        InitEscapeAction::Up => Some(InitOptionNav::Up),
        InitEscapeAction::Down => Some(InitOptionNav::Down),
        _ => None,
    }
}

/// Asks one wizard step from a visible option list (phase 1 of #1806).
///
/// Renders `prompt` plus every entry with a visible cursor line and
/// selection marker (`>`; cyan when `color` allows). Interaction:
/// - `Up`/`Down` arrows (any modifiers, application-cursor variants) and
///   `j`/`k` move the cursor (wrapping) and re-render without consuming an
///   attempt.
/// - `Enter` (empty line) selects the cursor entry.
/// - When `numbered`, `1..=N` selects directly (the shell/close-confirm/
///   preset fast path the codebase already uses); otherwise numbers fall
///   through to `parse` as values so numeric answers like `10` keep their
///   meaning.
/// - Any other text is parsed as today (custom paths, names, numbers as
///   values), preserving each step parser, its retry messages, and the
///   [`INIT_MAX_ATTEMPTS`] bound (navigation never counts as an attempt).
/// - Piped/non-TTY fallback is the numbered/typed fast path itself: a single
///   line with a number, name, or value (or empty for the default) selects
///   in one read with no navigation required, so piped input behaves exactly
///   as before minus the new list rendering.
///
/// Line decoding, history recall for non-navigation input, and the
/// display-vs-submitted redraw ([`init_line_needs_redraw`]) behave as in
/// [`init_ask`]: `history` is appended on successful parses only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn init_ask_option<T>(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    prompt: &str,
    options: &[InitOptionItem],
    default_index: usize,
    color: bool,
    numbered: bool,
    parse: impl Fn(&str) -> Result<T, String>,
    history: &mut Vec<String>,
) -> Result<T, String> {
    assert!(!options.is_empty(), "option list must not be empty");
    let mut cursor = default_index.min(options.len() - 1);
    let mut failures: usize = 0;
    loop {
        let _ = writeln!(output, "{prompt}");
        for (index, item) in options.iter().enumerate() {
            let selected = index == cursor;
            let marker = if selected {
                if color { "\u{1b}[36m>\u{1b}[0m" } else { ">" }
            } else {
                " "
            };
            let label = init_style_label(item, selected, color);
            if numbered {
                let _ = writeln!(output, "{marker} {}) {label}", index + 1);
            } else {
                let _ = writeln!(output, "{marker} {label}");
            }
        }
        if numbered {
            let _ = writeln!(
                output,
                "  (j/k or arrows move, Enter selects, number picks)"
            );
        } else {
            let _ = writeln!(
                output,
                "  (j/k or arrows move, Enter selects, or type a value)"
            );
        }
        let _ = output.flush();
        let (raw, line) = match init_read_line_with_history(input, history) {
            Some(pair) => pair,
            None => return Err("bitty init: aborted (end of input)".to_string()),
        };
        // Esc cancels the wizard: a bare `ESC` byte (pressed alone, then
        // Enter on a canonical TTY) aborts without writing anything, so the
        // caller restores the previous config. Arrow/kitty sequences still
        // navigate via `init_option_nav` below; only the lone byte cancels.
        if raw == [0x1b] {
            return Err("bitty init: aborted (cancelled)".to_string());
        }
        let trimmed = line.trim();
        if let Some(direction) = init_option_nav(&raw, trimmed) {
            match direction {
                InitOptionNav::Up => {
                    cursor = (cursor + options.len() - 1) % options.len();
                }
                InitOptionNav::Down => {
                    cursor = (cursor + 1) % options.len();
                }
            }
            continue;
        }
        if init_line_needs_redraw(&raw, &line)
            && (writeln!(output, "  (read as {line:?})").is_err() || output.flush().is_err())
        {
            return Err("bitty init: aborted (output error)".to_string());
        }
        let candidate: &str = if trimmed.is_empty() {
            &options[cursor].value_text
        } else if numbered {
            match trimmed.parse::<usize>() {
                Ok(number) if number >= 1 && number <= options.len() => {
                    &options[number - 1].value_text
                }
                _ => trimmed,
            }
        } else {
            trimmed
        };
        match parse(candidate) {
            Ok(value) => {
                if !line.is_empty() {
                    history.push(line);
                }
                return Ok(value);
            }
            Err(err) => {
                failures += 1;
                let _ = writeln!(
                    output,
                    "  ({err} — try again [{failures}/{INIT_MAX_ATTEMPTS}])"
                );
                if failures >= INIT_MAX_ATTEMPTS {
                    return Err(format!(
                        "bitty init: aborted (too many invalid answers, limit {INIT_MAX_ATTEMPTS})"
                    ));
                }
            }
        }
    }
}

/// Live preview overlay for the init wizard (phase 2 of #1806, seeded p3 CTX-1071,
/// per-step live hot-update p4 CTX-1119 DEC-0108).
///
/// Holds the exact previous effective config (`baseline`, never mutated)
/// plus the preview overlay (`current`) the wizard updates after every
/// successful prompt. The user config file is never touched until confirm:
/// selections apply to the preview session in memory only, so a crash
/// mid-wizard leaves no partial file behind and cancel/EOF rollback is a
/// trivial discard (`current` dropped, `baseline` kept).
///
/// Only wizard keys are overlaid (theme, font family/size, decoration
/// geometry, shell, scrollback, close-confirm, key preset); every other
/// field is cloned from `baseline` untouched. Of those, the `Live` subset
/// (`appearance.theme`, `font.family`/`font.size`, `decoration.*`,
/// `keymaps` — see `bitty-config::ReloadClass` and the `runtime_adopts`
/// list in `crate::config_reload`) is validated per step through the reload
/// `apply_live` path against a wizard-owned headless `Runtime`
/// ([`init_preview_push_live`]: fail-soft validation and producer state with
/// no visible effect on its own); the user-visible preview is the per-step
/// stdout echo from [`init_emit_preview_echo`], which renders the running
/// overlay appearance in the running preview theme. The restart-required
/// remainder (`terminal.shell`, `terminal.scrollback`, `close_confirm`) is
/// staged in the same overlay and takes effect on confirm/next start. TTY
/// seeds `baseline` from the loaded effective config via
/// [`init_preview_baseline`] (`CLI > file > profile > defaults`; empty layers
/// stay on the shipped `fallback_builtin` headless/test default). `bitty
/// init` stays a standalone pre-`Runtime` subcommand with its line-prompt
/// flow (no fullscreen TUI library): the wizard builds its own headless
/// preview `Runtime` from the baseline when [`init_live_preview_enabled`]
/// holds and pushes `current` per step as non-persistent producer state (no
/// file write until confirm, no new process or env authority, fail-soft
/// otherwise). Esc rollback restores the baseline both in memory (`rollback`,
/// plus a baseline push into the headless session) and on screen (the echo
/// simply stops; see [`run_init_interactive_with_preview`]).
///
/// `snapshots` records one `current` clone per successful prompted step
/// (skipped override steps apply silently first), so headless tests prove
/// mid-wizard preview without a TTY. `None` (no preview) is the
/// piped/non-TTY path: nothing to see, values still land on confirm.
/// Reuses the existing `color` convention; no new flags or env.
#[derive(Debug, Clone)]
pub(crate) struct InitPreview {
    /// Exact previous effective config; never mutated after construction.
    baseline: bitty_config::EffectiveConfig,
    /// Preview overlay: `baseline` plus every answered wizard key so far.
    current: bitty_config::EffectiveConfig,
    /// One `current` clone per successful prompted step, in prompt order.
    snapshots: Vec<bitty_config::EffectiveConfig>,
    /// False for piped/non-TTY: every apply/rollback is a no-op.
    enabled: bool,
}

impl InitPreview {
    /// Disabled preview (piped/non-TTY): applies and rollbacks do nothing,
    /// snapshots stay empty, and no file is ever touched.
    /// (Test-only today: production passes `None` for the no-preview path.)
    #[allow(dead_code)]
    pub(crate) fn disabled() -> Self {
        let baseline = bitty_config::fallback_builtin();
        Self {
            current: baseline.clone(),
            baseline,
            snapshots: Vec::new(),
            enabled: false,
        }
    }

    /// Enabled preview seeded from the exact previous effective config.
    /// Tests inject `fallback_builtin` (or a tweaked clone); production TTY
    /// seeds via [`init_preview_baseline`] (loaded effective when present,
    /// else `fallback_builtin`).
    pub(crate) fn enabled(baseline: bitty_config::EffectiveConfig) -> Self {
        Self {
            current: baseline.clone(),
            baseline,
            snapshots: Vec::new(),
            enabled: true,
        }
    }

    /// The exact previous effective config (rollback target).
    /// (Headless-proof accessor; the headed adopter reads `current`.)
    #[allow(dead_code)]
    pub(crate) fn baseline(&self) -> &bitty_config::EffectiveConfig {
        &self.baseline
    }

    /// The current preview overlay.
    #[allow(dead_code)]
    pub(crate) fn current(&self) -> &bitty_config::EffectiveConfig {
        &self.current
    }

    /// One snapshot per successful prompted step, in prompt order.
    #[allow(dead_code)]
    pub(crate) fn snapshots(&self) -> &[bitty_config::EffectiveConfig] {
        &self.snapshots
    }

    /// Discard the overlay and restore the exact previous config.
    /// Cancel/EOF paths call this before propagating the abort; the user
    /// file was never written, so restoration is total by construction.
    pub(crate) fn rollback(&mut self) {
        if self.enabled {
            self.current = self.baseline.clone();
        }
    }

    /// Record the current overlay after one successful prompted step.
    fn commit_step(&mut self) {
        if self.enabled {
            self.snapshots.push(self.current.clone());
        }
    }

    /// Seed skipped (value-flag) steps silently: no snapshots, so snapshot
    /// count always equals the prompted-step count.
    pub(crate) fn apply_overrides_silent(&mut self, overrides: &InitOverrides) {
        if !self.enabled {
            return;
        }
        if let Some(theme) = &overrides.theme {
            self.current.appearance.theme = Some(theme.clone());
        }
        if let Some(family) = &overrides.font_family {
            self.current.font.family.clone_from(family);
        }
        if let Some(size) = overrides.font_size {
            self.current.font.size = size;
        }
        if let Some(value) = overrides.gaps_in {
            self.current.decoration.gaps_in = value;
        }
        if let Some(value) = overrides.gaps_out {
            self.current.decoration.gaps_out = value;
        }
        if let Some(value) = overrides.border {
            self.current.decoration.border = value;
        }
        if let Some(value) = overrides.radius {
            self.current.decoration.radius = value;
        }
        if let Some(value) = overrides.scrollback {
            self.current.terminal.scrollback = value;
        }
        if let Some(mode) = overrides.close_confirm {
            self.current.close_confirm = mode;
        }
    }

    pub(crate) fn apply_shell(&mut self, shell: Option<&str>) {
        if !self.enabled {
            return;
        }
        self.current.terminal.shell = shell.map(str::to_string);
        self.commit_step();
    }

    pub(crate) fn apply_theme(&mut self, theme: &str) {
        if !self.enabled {
            return;
        }
        self.current.appearance.theme = Some(theme.to_string());
        self.commit_step();
    }

    pub(crate) fn apply_font_family(&mut self, family: &str) {
        if !self.enabled {
            return;
        }
        self.current.font.family = family.to_string();
        self.commit_step();
    }

    pub(crate) fn apply_font_size(&mut self, size: f32) {
        if !self.enabled {
            return;
        }
        self.current.font.size = size;
        self.commit_step();
    }

    pub(crate) fn apply_gaps_in(&mut self, value: u32) {
        if !self.enabled {
            return;
        }
        self.current.decoration.gaps_in = value;
        self.commit_step();
    }

    pub(crate) fn apply_gaps_out(&mut self, value: u32) {
        if !self.enabled {
            return;
        }
        self.current.decoration.gaps_out = value;
        self.commit_step();
    }

    pub(crate) fn apply_border(&mut self, value: u32) {
        if !self.enabled {
            return;
        }
        self.current.decoration.border = value;
        self.commit_step();
    }

    pub(crate) fn apply_radius(&mut self, value: u32) {
        if !self.enabled {
            return;
        }
        self.current.decoration.radius = value;
        self.commit_step();
    }

    pub(crate) fn apply_scrollback(&mut self, value: u32) {
        if !self.enabled {
            return;
        }
        self.current.terminal.scrollback = value;
        self.commit_step();
    }

    pub(crate) fn apply_close_confirm(&mut self, mode: bitty_config::CloseConfirm) {
        if !self.enabled {
            return;
        }
        self.current.close_confirm = mode;
        self.commit_step();
    }

    /// Preview the keybinding preset: `Default` keeps the implicit shipped
    /// map (`keymaps` empty); `Vim` stages the explicit shipped map (built
    /// FROM `DEFAULT_KEYMAPS`, never copied) the confirm file write pins.
    pub(crate) fn apply_key_preset(&mut self, preset: InitKeyPreset) {
        if !self.enabled {
            return;
        }
        match preset {
            InitKeyPreset::Default => self.current.keymaps.clear(),
            InitKeyPreset::Vim => {
                self.current.keymaps = bitty_config::keymap::DEFAULT_KEYMAPS
                    .iter()
                    .map(|(chord, action)| bitty_config::KeymapEntry {
                        chord: (*chord).to_string(),
                        action: (*action).to_string(),
                        context: String::from("global"),
                    })
                    .collect();
            }
        }
        self.commit_step();
    }

    /// Whether live pushes apply (false for the piped/non-TTY path).
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Whether the wizard maintains a live preview `Runtime` (p4 of #1806,
/// CTX-1119 DEC-0108).
///
/// Pure over `args` plus `env`, so headless tests assert the gate without a
/// TTY or a display. True only for the interactive line-prompt wizard
/// (`stdin_is_tty` without `--yes`) outside the machine flows: `--safe`
/// stays on the built-in safe config, `--headless`/`--test-mode` stay on the
/// headless smoke, and piped/non-TTY keeps the in-memory overlay only. When
/// false the wizard behaves exactly as p1-p3 (in-memory overlay, file write
/// on confirm, in-memory rollback); when true the caller builds a
/// wizard-owned preview `Runtime` and pushes per step (see
/// [`init_preview_push_live`]), fail-soft on any construction or adoption
/// error. No new flags, no new env, no new process authority.
pub(crate) fn init_live_preview_enabled(args: &Args, env: &InitEnv<'_>) -> bool {
    env.stdin_is_tty && !args.init_yes && !args.safe && !args.headless && !args.test_mode
}

/// Push the preview overlay through the existing reload path (p4 of #1806,
/// CTX-1119 DEC-0108 shape a, rescoped by DEC-0109).
///
/// Calls `config_reload::apply_live` with `preview.current()` (runtime half
/// validates theme, font, decoration; the app half is stashed for a future
/// headed owner via `TerminalApp::adopt_live_config`) and returns the
/// stashed `AppAdoption`, or `None` when preview is disabled or the adoption
/// failed. Headless validation and producer state only: the session has no
/// window and is never presented, so this has no visible effect on its own —
/// the user-visible preview is the stdout echo from
/// [`init_emit_preview_echo`]. Fail-soft by construction: any `apply_live`
/// error is discarded via `ok`, so a live rejection can never abort the
/// wizard — the in-memory overlay stays the commit truth and the file write
/// on confirm stays the sole persistence. Never writes the user file, never
/// spawns a shell, never touches the session store.
///
/// The caller pushes after every successful prompted step (plus once after
/// the silent override seeding) and pushes the baseline again after an
/// in-memory `rollback` on cancel/EOF, so the headless session ends on the
/// prior state as well as the overlay.
pub(crate) fn init_preview_push_live(
    runtime: &mut bitty_runtime::Runtime,
    preview: &InitPreview,
) -> Option<crate::config_reload::AppAdoption> {
    if !preview.is_enabled() {
        return None;
    }
    crate::config_reload::apply_live(runtime, preview.current()).ok()?;
    crate::config_reload::take_app_adoption()
}

/// Build the wizard-owned preview `Runtime` from the seeded baseline.
///
/// Uses the same mapping plus constructor as startup
/// (`runtime_config_from_effective_for` with `startup_prefer_light` plus
/// `Runtime::new`), so the preview computes the presentation values a fresh
/// launch would compute. Fail-soft: `None` on any mapping or construction
/// error, in which case the wizard continues with the in-memory overlay only.
/// No new process or env authority beyond the existing startup path; the
/// preview never spawns shells and never writes the user file. The session
/// stays headless (no window, no tick, no present): it validates Live
/// adoption and stages producer state, while the user-visible preview is the
/// stdout echo from [`init_emit_preview_echo`].
fn init_preview_runtime(
    baseline: &bitty_config::EffectiveConfig,
) -> Option<bitty_runtime::Runtime> {
    let cfg = crate::config_cli::runtime_config_from_effective_for(
        baseline,
        crate::config_cli::startup_prefer_light(),
    )
    .ok()?;
    bitty_runtime::Runtime::new(cfg).ok()
}

/// One-line running appearance preview printed after each wizard step (p4 of
/// #1806, CTX-1119 DEC-0109, CodeRabbit 1931 verdict B).
///
/// The `bitty init` process owns no window: the outer terminal renders every
/// visible pixel, so this stdout line is the user-visible half of the live
/// preview. It summarizes the running overlay appearance (theme, font
/// family/size, decoration geometry) and is styled in the running preview
/// theme through the existing option-list convention
/// ([`init_style_label`]: truecolor foreground on background when `color`
/// allows, plain otherwise). Print-only and fail-soft: write errors are
/// ignored, the overlay stays the commit truth, and the user file is never
/// touched. Gated on an enabled preview; a disabled preview prints nothing.
/// Styling resolves with `resolve_theme_with_status` (no stderr), so an
/// unknown seeded theme still echoes its stored name in default colors.
fn init_emit_preview_echo(output: &mut dyn std::io::Write, preview: &InitPreview, color: bool) {
    if !preview.is_enabled() {
        return;
    }
    let current = preview.current();
    let (resolved, _) =
        bitty_config::theme::resolve_theme_with_status(current.appearance.theme.as_deref());
    let size_text = if current.font.size.fract() == 0.0 {
        format!("{}", current.font.size as u32)
    } else {
        format!("{}", current.font.size)
    };
    let line = format!(
        "  (preview: theme={:?}, font={:?} {}pt, gaps {}/{}, border {}, radius {})",
        current.appearance.theme.as_deref().unwrap_or("default"),
        current.font.family,
        size_text,
        current.decoration.gaps_in,
        current.decoration.gaps_out,
        current.decoration.border,
        current.decoration.radius,
    );
    let item = InitOptionItem {
        label: line,
        value_text: String::new(),
        style_fg: Some(resolved.foreground),
        style_bg: Some(resolved.background),
    };
    let _ = writeln!(output, "{}", init_style_label(&item, false, color));
}

/// Runs the interactive wizard: mascot greeting, then shell / theme / font
/// (family, size) / decoration (gaps_in, gaps_out, border, radius) /
/// scrollback / close-confirm / keybinding-preset picks. Pure over injected
/// `input`, `output`, `shell_env`, `columns`, `shell_exists`,
/// `overrides`, and `color`, so the whole flow is headless-testable with
/// piped stdin.
///
/// A step answered by a value flag ([`InitOverrides`]) is skipped entirely
/// (no prompt, no stdin read); the remaining steps prompt from a navigable
/// option list (phase 1 of #1806) with their shipped default and reprompt
/// fail-closed on invalid input.
///
/// Live preview (phase 2 of #1806, per-step live p4 CTX-1119, stdout echo
/// DEC-0109): when `preview` is `Some`, each answered step overlays its value
/// onto the preview's in-memory config (no file writes), prints the running
/// appearance preview via [`init_emit_preview_echo`] (the user-visible half),
/// and, when `live` is `Some`, pushes the overlay through
/// [`init_preview_push_live`] as headless validation/producer state (no
/// visible effect on its own); cancel/EOF discards the overlay, restores the
/// baseline in memory via `rollback`, and pushes the baseline into the
/// headless session again (see [`InitPreview`]). `None` preview disables
/// overlay and echo (piped/non-TTY); `None` live keeps the in-memory overlay
/// plus echo only (headless, safe, piped, or construction failed —
/// fail-soft).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_init_interactive_with_preview(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    shell_env: Option<&str>,
    columns: Option<u16>,
    shell_exists: &dyn Fn(&str) -> bool,
    overrides: &InitOverrides,
    color: bool,
    preview: Option<&mut InitPreview>,
    live: Option<&mut bitty_runtime::Runtime>,
) -> Result<InitAnswers, String> {
    // Rollback guard: any abort below restores the exact previous config
    // before the error propagates, so Esc/cancel/EOF can never leave a
    // partial overlay behind (the user file was never written either). The
    // baseline push after the in-memory rollback returns the headless
    // session to the prior state on the same fail-soft path as the per-step
    // pushes; on screen the echo simply stops with the abort message.
    let mut preview = preview;
    let mut live = live;
    // Reborrow the inner `&mut` for the callee; a plain move would lose
    // the rollback handle below.
    #[allow(clippy::needless_option_as_deref)]
    let inner_preview = preview.as_deref_mut();
    #[allow(clippy::needless_option_as_deref)]
    let inner_live = live.as_deref_mut();
    let result = run_init_interactive_impl(
        input,
        output,
        shell_env,
        columns,
        shell_exists,
        overrides,
        color,
        inner_preview,
        inner_live,
    );
    if result.is_err() {
        if let Some(active) = preview.as_mut() {
            active.rollback();
            if let Some(runtime) = live.as_mut() {
                let _ = init_preview_push_live(runtime, active);
            }
        }
    }
    result
}

/// [`run_init_interactive_with_preview`] without preview (existing
/// piped/plain callers and the output-error contract).
/// Retained for the no-preview contract; the TTY dispatch now previews
/// through [`run_init_interactive_with_preview`].
#[allow(dead_code)]
pub(crate) fn run_init_interactive(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    shell_env: Option<&str>,
    columns: Option<u16>,
    shell_exists: &dyn Fn(&str) -> bool,
    overrides: &InitOverrides,
    color: bool,
) -> Result<InitAnswers, String> {
    run_init_interactive_with_preview(
        input,
        output,
        shell_env,
        columns,
        shell_exists,
        overrides,
        color,
        None,
        None,
    )
}

/// Wizard body shared by [`run_init_interactive`] and
/// [`run_init_interactive_with_preview`]; `preview` overlays each answered
/// step in memory, prints the running appearance echo, and `live` pushes each
/// overlay through headless validation (see [`InitPreview`],
/// [`init_emit_preview_echo`], and [`init_preview_push_live`]).
#[allow(clippy::too_many_arguments)]
fn run_init_interactive_impl(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    shell_env: Option<&str>,
    columns: Option<u16>,
    shell_exists: &dyn Fn(&str) -> bool,
    overrides: &InitOverrides,
    color: bool,
    mut preview: Option<&mut InitPreview>,
    mut live: Option<&mut bitty_runtime::Runtime>,
) -> Result<InitAnswers, String> {
    let _ = write!(output, "{}", init_greeting_art(columns));
    let _ = writeln!(
        output,
        "Welcome to bitty! This wizard writes your init.lua. (Enter takes the default.)"
    );
    let candidates = init_shell_candidates(shell_env, shell_exists);
    // Per-run recall for line editing: previous successful answers in this
    // wizard invocation (failed attempts never pollute it). Pure Up/Down
    // now moves the option cursor; typed text plus Up still recalls history
    // through the shared decoder.
    let mut history: Vec<String> = Vec::new();
    // Seed skipped (value-flag) steps silently so the overlay already
    // carries them; prompted steps append one snapshot each below. The live
    // push after seeding carries the skipped values into the preview session
    // before the first prompt, fail-soft when no live session exists.
    if let Some(active) = preview.as_mut() {
        active.apply_overrides_silent(overrides);
    }
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    let _ = writeln!(output, "\nShell:");
    let shell_options: Vec<InitOptionItem> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| InitOptionItem {
            label: if index == 0 {
                format!("{candidate} (default)")
            } else {
                candidate.clone()
            },
            value_text: candidate.clone(),
            style_fg: None,
            style_bg: None,
        })
        .collect();
    let shell = init_ask_option(
        input,
        output,
        "Shell [Enter for default, number, or custom path]:",
        &shell_options,
        0,
        color,
        true,
        |line| init_parse_shell_answer(line, &candidates),
        &mut history,
    )?;
    if let Some(active) = preview.as_mut() {
        active.apply_shell(shell.as_deref());
    };
    // Per-step preview: push the overlay through the headless reload path
    // (fail-soft validation/producer with no visible effect on its own) and
    // echo the running appearance preview to stdout (the user-visible half).
    // The shell itself is restart-required and stays staged for confirm.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let theme = match &overrides.theme {
        Some(theme) => theme.clone(),
        None => {
            let presets = bitty_config::theme::list_presets();
            let theme_options: Vec<InitOptionItem> = presets
                .iter()
                .enumerate()
                .map(|(index, preset)| {
                    let label = if index == 0 {
                        format!(
                            "{} ({}, default)",
                            preset.name,
                            bitty_config::theme::DARK_THEME_ALIAS
                        )
                    } else if preset.aliases.is_empty() {
                        preset.name.to_string()
                    } else {
                        format!("{} ({})", preset.name, preset.aliases.join(","))
                    };
                    // The default entry answers with the alias so stored
                    // answers stay `dark` as before; other entries answer
                    // with their canonical registry name.
                    let value_text = if index == 0 {
                        bitty_config::theme::DARK_THEME_ALIAS.to_string()
                    } else {
                        preset.name.to_string()
                    };
                    InitOptionItem {
                        label,
                        value_text,
                        style_fg: Some(preset.foreground),
                        style_bg: Some(preset.background),
                    }
                })
                .collect();
            init_ask_option(
                input,
                output,
                "Theme [dark]:",
                &theme_options,
                0,
                color,
                false,
                init_parse_theme_answer,
                &mut history,
            )
            .inspect(|theme| {
                if let Some(active) = preview.as_mut() {
                    active.apply_theme(theme);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let font_family = match &overrides.font_family {
        Some(family) => family.clone(),
        None => {
            // Documented monospace stack only (no invented catalog): the
            // shipped default plus its documented fallbacks; any other name
            // is still accepted as typed custom text.
            let families = [
                bitty_config::types::DEFAULT_FONT_FAMILY,
                "JetBrains Mono",
                "monospace",
                "DejaVu Sans Mono",
            ];
            let family_options: Vec<InitOptionItem> = families
                .iter()
                .enumerate()
                .map(|(index, family)| InitOptionItem {
                    label: if index == 0 {
                        format!("{family} (default)")
                    } else {
                        (*family).to_string()
                    },
                    value_text: (*family).to_string(),
                    style_fg: None,
                    style_bg: None,
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!(
                    "Font family [{}]:",
                    bitty_config::types::DEFAULT_FONT_FAMILY
                ),
                &family_options,
                0,
                color,
                false,
                init_parse_font_family_answer,
                &mut history,
            )
            .inspect(|family| {
                if let Some(active) = preview.as_mut() {
                    active.apply_font_family(family);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let font_size = match overrides.font_size {
        Some(size) => size,
        None => {
            let sizes = [
                bitty_config::types::DEFAULT_FONT_SIZE,
                11.0,
                13.0,
                14.0,
                16.0,
            ];
            let size_options: Vec<InitOptionItem> = sizes
                .iter()
                .enumerate()
                .map(|(index, size)| {
                    let text = if size.fract() == 0.0 {
                        format!("{}", *size as u32)
                    } else {
                        format!("{size}")
                    };
                    InitOptionItem {
                        label: if index == 0 {
                            format!("{text} (default)")
                        } else {
                            text.clone()
                        },
                        value_text: text,
                        style_fg: None,
                        style_bg: None,
                    }
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!(
                    "Font size in points [{}]:",
                    bitty_config::types::DEFAULT_FONT_SIZE
                ),
                &size_options,
                0,
                color,
                false,
                init_parse_font_size_answer,
                &mut history,
            )
            .inspect(|size| {
                if let Some(active) = preview.as_mut() {
                    active.apply_font_size(*size);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let _ = writeln!(
        output,
        "\nDecoration (logical pixels; 0 disables; see RFC-0001):"
    );
    let gaps_in = match overrides.gaps_in {
        Some(value) => value,
        None => {
            let values = [
                bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX,
                0,
                2,
                4,
                8,
                12,
            ];
            let items: Vec<InitOptionItem> = values
                .iter()
                .enumerate()
                .map(|(index, value)| InitOptionItem {
                    label: if index == 0 {
                        format!("{value} (default)")
                    } else if *value == 0 {
                        "0 (disabled)".to_string()
                    } else {
                        format!("{value}")
                    },
                    value_text: format!("{value}"),
                    style_fg: None,
                    style_bg: None,
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!(
                    "gaps_in (space between panes) [{}]:",
                    bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX
                ),
                &items,
                0,
                color,
                false,
                |line| {
                    init_parse_decoration_answer(
                        line,
                        "gaps_in",
                        bitty_config::types::MAX_DECORATION_GAP_PX,
                        bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX,
                    )
                },
                &mut history,
            )
            .inspect(|value| {
                if let Some(active) = preview.as_mut() {
                    active.apply_gaps_in(*value);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let gaps_out = match overrides.gaps_out {
        Some(value) => value,
        None => {
            let values = [
                bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX,
                0,
                2,
                4,
                8,
                12,
            ];
            let items: Vec<InitOptionItem> = values
                .iter()
                .enumerate()
                .map(|(index, value)| InitOptionItem {
                    label: if index == 0 {
                        format!("{value} (default)")
                    } else if *value == 0 {
                        "0 (disabled)".to_string()
                    } else {
                        format!("{value}")
                    },
                    value_text: format!("{value}"),
                    style_fg: None,
                    style_bg: None,
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!(
                    "gaps_out (space around the window edge) [{}]:",
                    bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX
                ),
                &items,
                0,
                color,
                false,
                |line| {
                    init_parse_decoration_answer(
                        line,
                        "gaps_out",
                        bitty_config::types::MAX_DECORATION_GAP_PX,
                        bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX,
                    )
                },
                &mut history,
            )
            .inspect(|value| {
                if let Some(active) = preview.as_mut() {
                    active.apply_gaps_out(*value);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let border = match overrides.border {
        Some(value) => value,
        None => {
            let values = [
                bitty_config::types::DEFAULT_DECORATION_BORDER_PX,
                0,
                2,
                4,
                8,
            ];
            let items: Vec<InitOptionItem> = values
                .iter()
                .enumerate()
                .map(|(index, value)| InitOptionItem {
                    label: if index == 0 {
                        format!("{value} (default)")
                    } else if *value == 0 {
                        "0 (disabled)".to_string()
                    } else {
                        format!("{value}")
                    },
                    value_text: format!("{value}"),
                    style_fg: None,
                    style_bg: None,
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!(
                    "border (frame thickness) [{}]:",
                    bitty_config::types::DEFAULT_DECORATION_BORDER_PX
                ),
                &items,
                0,
                color,
                false,
                |line| {
                    init_parse_decoration_answer(
                        line,
                        "border",
                        bitty_config::types::MAX_DECORATION_BORDER_PX,
                        bitty_config::types::DEFAULT_DECORATION_BORDER_PX,
                    )
                },
                &mut history,
            )
            .inspect(|value| {
                if let Some(active) = preview.as_mut() {
                    active.apply_border(*value);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let radius = match overrides.radius {
        Some(value) => value,
        None => {
            let values = [
                bitty_config::types::DEFAULT_DECORATION_RADIUS_PX,
                0,
                4,
                8,
                12,
                16,
            ];
            let items: Vec<InitOptionItem> = values
                .iter()
                .enumerate()
                .map(|(index, value)| InitOptionItem {
                    label: if index == 0 {
                        format!("{value} (default)")
                    } else if *value == 0 {
                        "0 (disabled)".to_string()
                    } else {
                        format!("{value}")
                    },
                    value_text: format!("{value}"),
                    style_fg: None,
                    style_bg: None,
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!(
                    "radius (corner rounding) [{}]:",
                    bitty_config::types::DEFAULT_DECORATION_RADIUS_PX
                ),
                &items,
                0,
                color,
                false,
                |line| {
                    init_parse_decoration_answer(
                        line,
                        "radius",
                        bitty_config::types::MAX_DECORATION_RADIUS_PX,
                        bitty_config::types::DEFAULT_DECORATION_RADIUS_PX,
                    )
                },
                &mut history,
            )
            .inspect(|value| {
                if let Some(active) = preview.as_mut() {
                    active.apply_radius(*value);
                }
            })?
        }
    };
    // Per-step preview: headless reload-path push (fail-soft, no visible
    // effect on its own) plus the user-visible stdout echo in the running
    // preview theme.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let _ = writeln!(output, "\nBehavior:");
    let scrollback = match overrides.scrollback {
        Some(value) => value,
        None => {
            let default_scroll = bitty_config::types::TerminalConfig::default().scrollback;
            let mut values = vec![default_scroll, 0, 5_000, 20_000, 50_000, 100_000];
            values.dedup();
            let items: Vec<InitOptionItem> = values
                .iter()
                .enumerate()
                .map(|(index, value)| InitOptionItem {
                    label: if index == 0 {
                        format!("{value} (default)")
                    } else if *value == 0 {
                        "0 (disabled)".to_string()
                    } else {
                        format!("{value}")
                    },
                    value_text: format!("{value}"),
                    style_fg: None,
                    style_bg: None,
                })
                .collect();
            init_ask_option(
                input,
                output,
                &format!("Scrollback lines [{default_scroll}]:"),
                &items,
                0,
                color,
                false,
                init_parse_scrollback_answer,
                &mut history,
            )
            .inspect(|value| {
                if let Some(active) = preview.as_mut() {
                    active.apply_scrollback(*value);
                }
            })?
        }
    };
    // Per-step preview: the scrollback value is restart-required and stays
    // staged for confirm; the headless push is a fail-soft no-op without a
    // live session while the stdout echo still shows the running appearance
    // preview (the user-visible half).
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let close_confirm = match overrides.close_confirm {
        Some(mode) => mode,
        None => {
            // Value texts use the parser spellings (`1` for the default;
            // `when_busy` is display-only and is rejected by the parser).
            let items = vec![
                InitOptionItem {
                    label: "when_busy — confirm only while a foreground job runs (default)"
                        .to_string(),
                    value_text: "1".to_string(),
                    style_fg: None,
                    style_bg: None,
                },
                InitOptionItem {
                    label: "always — confirm every view/window close".to_string(),
                    value_text: "always".to_string(),
                    style_fg: None,
                    style_bg: None,
                },
                InitOptionItem {
                    label: "never — never confirm".to_string(),
                    value_text: "never".to_string(),
                    style_fg: None,
                    style_bg: None,
                },
            ];
            init_ask_option(
                input,
                output,
                "Close confirm [1 when_busy]:",
                &items,
                0,
                color,
                true,
                init_parse_close_confirm_answer,
                &mut history,
            )
            .inspect(|mode| {
                if let Some(active) = preview.as_mut() {
                    active.apply_close_confirm(*mode);
                }
            })?
        }
    };
    // Per-step preview: close-confirm takes effect on confirm; the headless
    // push is a fail-soft no-op without a live session while the stdout echo
    // still shows the running appearance preview (the user-visible half).
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
    let preset_options = vec![
        InitOptionItem {
            label: "default — shipped Alt-as-Mod map (Alt+h/j/k/l, Alt+1..9, Alt+u/i)".to_string(),
            value_text: "default".to_string(),
            style_fg: None,
            style_bg: None,
        },
        InitOptionItem {
            label: "vim — write that map explicitly (tweakable starting point)".to_string(),
            value_text: "vim".to_string(),
            style_fg: None,
            style_bg: None,
        },
    ];
    let key_preset = init_ask_option(
        input,
        output,
        "Keybindings [1 default / 2 vim]:",
        &preset_options,
        0,
        color,
        true,
        init_parse_preset_answer,
        &mut history,
    )?;
    if let Some(active) = preview.as_mut() {
        active.apply_key_preset(key_preset);
    }
    // Final per-step preview: the key preset rides the app half of the reload
    // path (stashed for a future headed owner via adopt_live_config); the
    // stdout echo shows the running appearance preview. Fail-soft without a
    // live session.
    if let (Some(active), Some(runtime)) = (preview.as_deref(), live.as_mut()) {
        let _ = init_preview_push_live(runtime, active);
    }
    if let Some(active) = preview.as_deref() {
        init_emit_preview_echo(output, active, color);
    }
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
     \x20 --plugins IDS        comma-separated bundled ids (e.g. bitty-terminal.shell-integration)\n\
     \x20 --no-plugins         select no plugins (wins over --plugins)\n\
     \n\
     follow-ups (#1913, offline-safe, no network/secrets without opt-in):\n\
     \x20 --with-components / --no-with-components  offer component installs\n\
     \x20 --print-shell-init / --no-print-shell-init  print the shell-init handoff line\n\
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
    /// Stands in for `BITTY_PROFILE` (named profile request).
    pub(crate) bitty_profile: Option<&'a str>,
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
/// remaining environment (XDG root, `BITTY_PROFILE`, COLUMNS, TTY-ness) is
/// read live. Tests use [`run_init_subcommand_with_io`] for full hermeticity.
pub(crate) fn run_init_subcommand_with_env(
    args: &Args,
    bitty_config_env: Option<&str>,
    shell_env: Option<&str>,
) -> i32 {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let profile = std::env::var("BITTY_PROFILE").ok();
    let columns = init_columns_from_env(std::env::var("COLUMNS").ok().as_deref());
    let env = InitEnv {
        bitty_config: bitty_config_env,
        bitty_profile: profile.as_deref(),
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

/// Baseline for the TTY live preview (p3 of #1806, CTX-1071; live wiring p4
/// CTX-1119 DEC-0108).
///
/// Best-effort load of the effective config with the same layering as
/// startup (`CLI > file > profile > defaults`):
/// explicit `--config`/`BITTY_CONFIG` plus the XDG default probe for the
/// user file, `--profile`/`BITTY_PROFILE` for the profile chain (via
/// `load_profile_chain_with_env` with the injected XDG roots), and the CLI
/// appearance overrides (`--theme`/`--font-family`/`--font-size`/`--opacity`
/// via `crate::config_cli::cli_overrides_from_args`). `--safe` stays on the
/// built-in safe config (which equals `fallback_builtin`).
///
/// Never blocks the wizard: a missing file, an unreadable/invalid file or
/// profile, or a merge failure all fall back to `fallback_builtin`. Empty
/// layers (no file, no profile, no CLI override) also stay on
/// `fallback_builtin`, so headless runs and fresh-machine TTY runs keep the
/// exact previous baseline byte-for-byte.
///
/// Live wiring (p4 CTX-1119, shape a, rescoped by DEC-0109 CodeRabbit 1931):
/// `bitty init` stays a standalone pre-`Runtime` subcommand with its
/// line-prompt flow (no fullscreen TUI, no new IPC or threads) in the outer
/// terminal, which owns every visible pixel. The user-visible preview is the
/// per-step stdout echo from [`init_emit_preview_echo`], driven by
/// `InitPreview::current` and styled in the running preview theme. When
/// [`init_live_preview_enabled`] holds, the dispatch additionally builds a
/// wizard-owned headless preview `Runtime` from this baseline via
/// [`init_preview_runtime`] and pushes each `InitPreview::current` step
/// through the existing reload path as validation/producer state (no file
/// write until confirm, no visible effect on its own):
/// `config_reload::apply_live` (`config_reload.rs`) for the runtime half
/// plus the stashed `AppAdoption` taken via `take_app_adoption` for a future
/// headed owner through `TerminalApp::adopt_live_config`
/// (`terminal_app.rs`), reusing the `runtime_adopts` adopter list and the
/// `ReloadClass::Live` policy in `bitty-config`. Cancel/EOF rolls the
/// overlay back in memory (the echo stops with the abort message) and pushes
/// the baseline into the headless session again; the file write on confirm
/// stays the only persistence.
pub(crate) fn init_preview_baseline(
    args: &Args,
    env: &InitEnv<'_>,
) -> bitty_config::EffectiveConfig {
    if args.safe {
        return bitty_config::fallback_builtin();
    }
    let explicit =
        bitty_config::file::resolve_config_explicit(args.config_path.as_deref(), env.bitty_config);
    let probed = bitty_config::file::probe_config_path_with_env(
        explicit.as_deref(),
        env.xdg_config_home,
        env.home,
        &|path| path.exists(),
    );
    // CodeRabbit 1877: preserve the load error instead of discarding it
    // with `.ok()`. An existing file that fails to parse is user error the
    // real load will also reject, so fail closed to the fallback even when
    // another layer (e.g. --theme) is present — a theme-on-defaults preview
    // would otherwise hide the breakage. Missing files stay soft (None).
    let mut file_broken = false;
    let file_layer = probed.as_ref().and_then(|found| {
        if found.path.exists() {
            match bitty_config::file::load_user_layer(&found.path) {
                Ok(layer) => Some(layer),
                Err(_) => {
                    file_broken = true;
                    None
                }
            }
        } else {
            None
        }
    });
    if file_broken {
        return bitty_config::fallback_builtin();
    }
    let profile_request =
        bitty_config::file::resolve_profile_request(args.profile.as_deref(), env.bitty_profile);
    let mut profile_layers = Vec::new();
    if let Some(requested) = profile_request {
        match bitty_config::file::load_profile_chain_with_env(
            &requested,
            env.xdg_config_home,
            env.home,
            None,
            None,
        ) {
            Ok(layers) => profile_layers = layers,
            Err(_) => return bitty_config::fallback_builtin(),
        }
    }
    let cli = crate::config_cli::cli_overrides_from_args(args);
    if file_layer.is_none() && profile_layers.is_empty() && cli.is_empty() {
        return bitty_config::fallback_builtin();
    }
    match bitty_config::file::resolve_effective_with_profiles(file_layer, profile_layers, &cli) {
        Ok(merged) => merged.effective,
        Err(_) => bitty_config::fallback_builtin(),
    }
}

/// Prompts the optional first-batch plugin selection (CTX-1132, #1913).
///
/// Lists bundled ids from the same source `plugin install <id>` uses
/// ([`bitty_plugin_host::bundled::bundled_ids_sorted`], no duplicate list),
/// then reads one comma-separated answer via [`init_ask`] (reprompt
/// fail-closed, EOF aborts, Esc cancels). Empty means none. Offline-safe:
/// no fetch, no consent, no secrets — recording happens later in the dispatch.
/// Returns the selected ids in catalog order.
fn init_prompt_plugin_selection(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    history: &mut Vec<String>,
) -> Result<Vec<String>, String> {
    let ids = bitty_plugin_host::bundled::bundled_ids_sorted();
    if ids.is_empty() {
        let _ = writeln!(output, "\nPlugins (none bundled; skipping):");
        let _ = output.flush();
        return Ok(Vec::new());
    }
    let _ = writeln!(output, "\nPlugins (optional first batch, offline-safe):");
    for id in &ids {
        let _ = writeln!(output, "  - {id}");
    }
    let _ = writeln!(
        output,
        "Selections are recorded offline without fetching; fetching needs explicit opt-in."
    );
    let _ = output.flush();
    init_ask(
        input,
        output,
        "Plugins [comma-separated ids, empty for none]:",
        init_parse_plugins_answer,
        history,
    )
}

/// Prompts one yes/no opt-in gate (plugin-fetch and component-seed share this
/// shape; CTX-1132, #1913). Empty means no (fail-closed default).
fn init_prompt_yes_no(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    prompt: &str,
    history: &mut Vec<String>,
) -> Result<bool, String> {
    init_ask(input, output, prompt, init_parse_yes_no_answer, history)
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
        // Existing color convention only (no new flag): any shared
        // `--no-color` (set in lockstep for a pre-word flag), `NO_COLOR`,
        // or `TERM=dumb` disables init list colors.
        let no_color = args.doctor_no_color
            || args.list_no_color
            || args.inspect_no_color
            || args.dev_no_color
            || args.plugin_no_color
            || args.component_no_color;
        let color = init_color_enabled(no_color);
        // Live preview (phase 2 of #1806, seeded p3 CTX-1071, per-step live p4
        // CTX-1119, stdout echo DEC-0109): on a TTY the wizard overlays each
        // answer onto an in-memory preview seeded from the loaded effective
        // config (`init_preview_baseline`: `CLI > file > profile > defaults`,
        // else the shipped fallback) and echoes the running appearance
        // preview to stdout after each step (the user-visible half, styled in
        // the running preview theme when color allows). The user file is
        // never written until confirm below; cancel/EOF rolls the overlay
        // back and returns 1 with nothing written. Piped/non-TTY stays
        // preview-free (nothing to see). When `init_live_preview_enabled`
        // holds, the wizard additionally owns a headless preview `Runtime`
        // built from the baseline and pushes each overlay per step through
        // the reload `apply_live` path as validation/producer state (no
        // visible effect on its own, fail-soft without a live display path);
        // headless, safe, and piped modes keep the in-memory-plus-echo
        // behavior unchanged.
        let mut preview = InitPreview::enabled(init_preview_baseline(args, env));
        let mut live_runtime = if init_live_preview_enabled(args, env) {
            init_preview_runtime(preview.baseline())
        } else {
            None
        };
        match run_init_interactive_with_preview(
            input,
            output,
            env.shell,
            env.columns,
            &|path| std::path::Path::new(path).exists(),
            &overrides,
            color,
            Some(&mut preview),
            live_runtime.as_mut(),
        ) {
            Ok(answers) => answers,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    };
    // CTX-1132 (#1913) follow-up decisions (no filesystem writes yet):
    // plugin selection, fetch opt-in, component opt-in, shell-init print.
    // Prompts fail closed (EOF/cancel/too many invalid -> exit 1, nothing
    // written); `--yes` takes flag values or offline-safe defaults without
    // prompting. Offline contract: no network/secrets here, only decisions.
    let mut followup_history: Vec<String> = Vec::new();
    let plugin_ids: Vec<String> = if let Some(list) = overrides.plugins.clone() {
        list
    } else if args.init_yes {
        Vec::new()
    } else {
        match init_prompt_plugin_selection(input, output, &mut followup_history) {
            Ok(ids) => ids,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    };
    let fetch_approved: bool = if plugin_ids.is_empty() {
        false
    } else if args.init_yes {
        // `--yes` honors per-plugin consent (same as `plugin install --yes`).
        true
    } else {
        match init_prompt_yes_no(
            input,
            output,
            "Install selected plugins now (grant capabilities)? [y/N]:",
            &mut followup_history,
        ) {
            Ok(approved) => approved,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    };
    let with_components: bool = if plugin_ids.is_empty() {
        false
    } else if let Some(value) = overrides.with_components {
        value
    } else if args.init_yes {
        false
    } else {
        match init_prompt_yes_no(
            input,
            output,
            "Install missing native components for selected plugins? [y/N]:",
            &mut followup_history,
        ) {
            Ok(approved) => approved,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    };
    let print_shell_init: bool = overrides.print_shell_init.unwrap_or(true);
    let content = render_init_lua(&answers);
    // Visible preview: show the exact pending config before write.
    // Print-only (no confirm prompt); abort paths return above, so this
    // runs only on the commit path. Plain header (no color): the wizard
    // `color` only styles option lists, and plain keeps piped output
    // greppable. Skipped when the write below will refuse (existing file
    // without --force) so the recap never reads as a write promise.
    if !(target.exists() && !args.init_force) {
        let _ = writeln!(output, "\n  (pending init.lua — about to be written)");
        let _ = write!(output, "{content}");
        let _ = output.flush();
    }
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
            // CTX-1132 (#1913) follow-up commits (after the config write, so a
            // config refusal writes nothing at all, preserving --yes
            // idempotence). All offline-safe: plugin records without fetching,
            // per-plugin consent only on explicit opt-in, component hints
            // only (never silent fetch), shell-init printed (never evaled).
            if !plugin_ids.is_empty() {
                let state_path = match crate::plugin::init_plugin_state_path(
                    args.config_path.as_deref(),
                    env.bitty_config,
                    env.xdg_config_home,
                    env.home,
                ) {
                    Ok(path) => Some(path),
                    Err(message) => {
                        eprintln!(
                            "bitty init: {message} (plugin queue skipped; re-run `bitty plugin install <id>` later)"
                        );
                        None
                    }
                };
                if let Some(state_path) = state_path {
                    match crate::plugin::init_record_bundled_offline(&state_path, &plugin_ids) {
                        Ok(recorded) => {
                            if recorded > 0 {
                                let _ = writeln!(
                                    output,
                                    "bitty init: recorded {} plugin{} offline (disabled, no fetch)",
                                    recorded,
                                    if recorded == 1 { "" } else { "s" },
                                );
                            } else {
                                let _ = writeln!(
                                    output,
                                    "bitty init: plugins already recorded (nothing new)"
                                );
                            }
                            let _ = output.flush();
                        }
                        Err(message) => {
                            eprintln!(
                                "bitty init: {message} (re-run `bitty plugin install <id>` later)"
                            );
                        }
                    }
                    if fetch_approved {
                        for id in &plugin_ids {
                            match crate::plugin::init_install_bundled_with_consent(
                                &state_path,
                                id,
                                args.init_yes,
                                input,
                                output,
                            ) {
                                Ok(summary) => {
                                    let _ = writeln!(output, "bitty init: {summary}");
                                    let _ = output.flush();
                                }
                                Err(message) => {
                                    let _ = writeln!(
                                        output,
                                        "bitty init: {message} (kept offline record; re-run `bitty plugin install {id}` later)"
                                    );
                                    let _ = output.flush();
                                }
                            }
                        }
                    } else {
                        let _ = writeln!(
                            output,
                            "bitty init: skipped fetching (offline record kept; re-run `bitty plugin install <id>` to grant)"
                        );
                        let _ = output.flush();
                    }
                }
            }
            if with_components && !plugin_ids.is_empty() {
                let mut declared: Vec<String> = Vec::new();
                for id in &plugin_ids {
                    for component in init_bundled_plugin_components(id) {
                        if !declared.iter().any(|seen| seen == &component) {
                            declared.push(component);
                        }
                    }
                }
                if declared.is_empty() {
                    let _ = writeln!(
                        output,
                        "bitty init: no selected plugin declares native components; nothing to install"
                    );
                } else {
                    for component in &declared {
                        let _ = writeln!(
                            output,
                            "bitty init: component '{component}' is missing; re-run `bitty component install {component} --version <X.Y.Z>` (offline, nothing fetched; or stage via `bitty component add <path>`)"
                        );
                    }
                }
                let _ = output.flush();
            }
            if print_shell_init {
                let detected = init_detect_shell_for_handoff(answers.shell.as_deref(), env.shell);
                let line = init_shell_init_line(&detected);
                let source_hint = init_shell_source_hint(&detected);
                println!(
                    "bitty init: wire Tab completion + prompt hooks with `{line}`{source_hint} (detected shell: {detected}; `bitty shell-init --help` lists all five)"
                );
            }
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
