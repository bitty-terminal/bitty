//! `bitty init` opt-in setup wizard (#243, CTX-0149).

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
    /// `font.size` in points, within `(0, 128]`.
    pub(crate) font_size: f32,
    /// Keybinding preset choice.
    pub(crate) key_preset: InitKeyPreset,
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
/// path (else unset so startup falls back), the dark preset, the default
/// point size, and the implicit shipped keymap defaults.
pub(crate) fn init_yes_defaults(shell_env: Option<&str>) -> InitAnswers {
    let shell = shell_env
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| init_clean_shell(s).ok());
    InitAnswers {
        shell,
        theme: bitty_config::theme::DARK_THEME_ALIAS.to_string(),
        font_size: bitty_config::types::DEFAULT_FONT_SIZE,
        key_preset: InitKeyPreset::Default,
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

/// Parses one theme-step answer. Only the shipped preset exists today
/// (`dark`, canonical config value; `bitty-dark` accepted as the registry
/// name), so empty/`1`/either name resolves to `"dark"` and anything else
/// reprompts instead of writing a value the resolver would only fall back.
pub(crate) fn init_parse_theme_answer(raw: &str) -> Result<String, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "dark" | "bitty-dark" => Ok(bitty_config::theme::DARK_THEME_ALIAS.to_string()),
        _ => Err("unknown theme (only 'dark' is shipped today)".to_string()),
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
/// `init_rendered_config_parses`): `theme` + both `font` keys are always
/// present (the `font`/`window` tables require complete pairs), the
/// `terminal` table always carries `scrollback` alongside `shell` (the
/// parser requires `terminal.scrollback`), and the `keymaps` section appears
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
        init_lua_escape(bitty_config::types::DEFAULT_FONT_FAMILY),
        answers.font_size,
    ));
    if let Some(shell) = &answers.shell {
        out.push_str(&format!(
            "    terminal = {{ scrollback = {}, shell = \"{}\" }},\n",
            bitty_config::TerminalConfig::default().scrollback,
            init_lua_escape(shell),
        ));
    }
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

/// Runs the interactive wizard: mascot greeting, then shell / theme /
/// font-size / keybinding-preset picks. Pure over injected `input`,
/// `output`, `shell_env`, `columns`, and `shell_exists`, so the whole flow
/// is headless-testable with piped stdin.
pub(crate) fn run_init_interactive(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    shell_env: Option<&str>,
    columns: Option<u16>,
    shell_exists: &dyn Fn(&str) -> bool,
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
    let theme = init_ask(input, output, "Theme [dark]:", init_parse_theme_answer)?;
    let font_size = init_ask(
        input,
        output,
        &format!(
            "Font size in points [{}]:",
            bitty_config::types::DEFAULT_FONT_SIZE
        ),
        init_parse_font_size_answer,
    )?;
    let key_preset = init_ask(
        input,
        output,
        "Keybindings [1 default / 2 vim]:\n  1) default — shipped Alt-as-Mod map (Alt+h/j/k/l, Alt+1..9, Alt+u/i)\n  2) vim — write that map explicitly (tweakable starting point):",
        init_parse_preset_answer,
    )?;
    Ok(InitAnswers {
        shell,
        theme,
        font_size,
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
    "usage: bitty init [--yes] [--force] [--config PATH]\n\
     \n\
     Opt-in setup wizard (never auto-runs): mascot greeting, shell / theme /\n\
     font-size / keybinding-preset picks, then writes the config file.\n\
     \n\
     flags:\n\
     \x20 --yes     skip prompts; write sane defaults ($SHELL when clean,\n\
     \x20           dark theme, 12pt, shipped keymap defaults)\n\
     \x20 --force   overwrite an existing file (backs it up to init.lua.bak)\n\
     \n\
     target: --config PATH wins, else BITTY_CONFIG, else\n\
     $XDG_CONFIG_HOME/bitty/init.lua (fallback ~/.config/bitty/init.lua).\n\
     Without --force an existing file is never overwritten (exit 2).\n\
     Re-runs are idempotent: same answers write the same file.\n\
     Validate any file any time with `bitty config check`."
        .to_string()
}

/// Runs `bitty init`; returns the process exit code.
///
/// - `0`: wrote the file (prints the target plus what changed).
/// - `2`: usage-level refusal (unexpected args, existing file without
///   `--force`, invalid `$SHELL` handling aside) — nothing was overwritten.
/// - `1`: aborted prompts (EOF / too many retries) or filesystem failure.
pub(crate) fn run_init_subcommand(args: &Args) -> i32 {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let shell_env = std::env::var("SHELL").ok();
    run_init_subcommand_with_env(args, bitty_config_env.as_deref(), shell_env.as_deref())
}

/// [`run_init_subcommand`] with injected environment values so tests stay
/// hermetic (no process-env mutation): `bitty_config_env` stands in for
/// `BITTY_CONFIG`, `shell_env` for `SHELL`.
pub(crate) fn run_init_subcommand_with_env(
    args: &Args,
    bitty_config_env: Option<&str>,
    shell_env: Option<&str>,
) -> i32 {
    if !args.init_args.is_empty() {
        eprintln!(
            "bitty init: unexpected argument '{}'\n{}",
            args.init_args[0],
            init_usage()
        );
        return 2;
    }
    // BITTY_CONFIG env participates exactly like --config (CLI wins), same
    // as the `config` subcommand and startup.
    let explicit =
        bitty_config::file::resolve_config_explicit(args.config_path.as_deref(), bitty_config_env);
    let target = match bitty_config::file::probe_config_path(explicit.as_deref()) {
        Some(probed) => probed.path,
        None => {
            eprintln!("bitty init: no config root ($XDG_CONFIG_HOME or $HOME unset)");
            return 2;
        }
    };
    let answers = if args.init_yes {
        let answers = init_yes_defaults(shell_env);
        // Warn when $SHELL existed but was unusable, so the omission is
        // never silent (the written file simply leaves `shell` unset and
        // startup falls back to /bin/sh).
        let raw = shell_env.map(str::trim).unwrap_or_default();
        if !raw.is_empty() && answers.shell.is_none() {
            eprintln!("bitty init: ignoring unusable $SHELL {raw:?}; leaving shell unset");
        }
        answers
    } else {
        let columns = init_columns_from_env(std::env::var("COLUMNS").ok().as_deref());
        let stdin = std::io::stdin();
        let mut stdin_lock = stdin.lock();
        let stdout = std::io::stdout();
        let mut stdout_lock = stdout.lock();
        match run_init_interactive(
            &mut stdin_lock,
            &mut stdout_lock,
            shell_env,
            columns,
            &|path| std::path::Path::new(path).exists(),
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
                "bitty init: wrote '{}' (theme=\"{}\", shell={shell}, font.size={}, keymaps={keymaps})",
                outcome.path.display(),
                answers.theme,
                answers.font_size,
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
