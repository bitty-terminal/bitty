//! User config-file loading and `bitty config` handling (CTX-0148, DEC-0011).

use crate::cli::{Args, ConfigCommand, help_text};

// ---------------------------------------------------------------------------
// User config-file loading (CTX-0148 Lua via bitty-lua sandbox, DEC-0011)
// ---------------------------------------------------------------------------

/// Owned result of loading the effective user configuration.
///
/// `source` is `"cli"` when `--theme` overrode, `"file"` when the config file
/// provided the theme, `"profile"` when the named profile provided it, else
/// `"default"`. The resolved `theme` preset is the single `bitty-config`
/// registry entry the window renders.
pub(crate) struct AppConfig {
    /// Merged effective config (`CLI > file > profile > defaults`).
    pub(crate) effective: bitty_config::EffectiveConfig,
    /// Config file path that was used, if any.
    pub(crate) file_path: Option<std::path::PathBuf>,
    /// Requested profile name (`--profile` over `BITTY_PROFILE`), if any.
    pub(crate) profile_name: Option<String>,
    /// Profile file path that was used, if any.
    pub(crate) profile_path: Option<std::path::PathBuf>,
    /// Resolved theme preset (static registry entry).
    pub(crate) theme: &'static bitty_config::theme::Theme,
    /// How the theme resolved (Default/Named/FallbackUnknown).
    pub(crate) resolution: bitty_config::theme::ThemeResolution,
    /// `"cli"` / `"file"` / `"profile"` / `"default"`: which layer won the theme.
    pub(crate) source: &'static str,
}

/// Resolved configuration bundle behind startup and `config check`.
pub(crate) struct LoadedConfig {
    /// Merged effective config.
    pub(crate) merged: bitty_config::MergedConfig,
    /// Probed user file (explicit or default), if a root exists.
    pub(crate) probed: Option<bitty_config::file::ProbedConfig>,
    /// Requested profile name (`--profile` over `BITTY_PROFILE`), if any.
    pub(crate) profile_name: Option<String>,
    /// Profile file that was loaded, if any.
    pub(crate) profile_path: Option<std::path::PathBuf>,
}

/// Builds the single CLI override input from parsed [`Args`] (CTX-0180).
///
/// Pure over `args` so the wiring stays hermetic: trims each raw and treats
/// `None`/empty/whitespace as absent. Numeric raws (`--font-size`,
/// `--opacity`) stay strings here; [`load_merged_config`] validates them
/// fail-closed (with usage) before the merge runs, and the merge validates
/// again, so direct API misuse fails closed too.
pub(crate) fn cli_overrides_from_args(args: &Args) -> bitty_config::file::CliOverrides {
    bitty_config::file::CliOverrides {
        theme: args
            .theme
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        font_family: args
            .font_family
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        font_size: args
            .font_size
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        opacity: args
            .opacity
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    }
}

/// Names the CLI flag behind an appearance-override validation error for the
/// fail-closed message. `theme` keeps its CTX-0169 merge-time path and never
/// reaches here; anything unexpected falls back to the flag group.
pub(crate) fn appearance_flag_for_field(field: Option<&str>) -> &'static str {
    match field {
        Some("font.family") => "--font-family",
        Some("font.size") => "--font-size",
        Some("window.opacity") => "--opacity",
        _ => "--font-family/--font-size/--opacity",
    }
}

/// Shared probe + load + merge behind startup and `config check` (CTX-0169).
///
/// Resolution (per #271 and `lua-and-xdg.md` §Layers):
/// - User file: `--config` CLI wins over `BITTY_CONFIG` env, else the XDG
///   default probe (`init.lua`, then `config.lua` alias, then canonical
///   `init.lua`). Missing default-probe files yield defaults; missing
///   explicit (`--config`/`BITTY_CONFIG`) files fail closed.
/// - Profile: `--profile` CLI wins over `BITTY_PROFILE` env, loaded from
///   `$XDG_CONFIG_HOME/bitty/profiles/<name>.lua` as the `Profile` layer
///   UNDER the user file (`init.lua` still wins; `--theme` wins over both).
///   Requested-but-missing/invalid profiles fail closed (no fallback).
/// - Merge: `CLI (appearance flags) > user file > profile > defaults` via
///   `bitty-config::file::resolve_effective_full` (merge sorts by
///   `LayerKind::precedence`, never by load order).
/// - `--config` + `--profile` together: both layers load (explicit file wins
///   over the profile, same as `init.lua` over profile) with a stderr warning.
///
/// CTX-0180: CLI appearance overrides (`--font-family`/`--font-size`/
/// `--opacity`) extend the [`bitty_config::file::CliOverrides`] built here —
/// one `Cli` plan carrying every present CLI field — so this function's shape
/// stays stable. Theme/profile resolution is untouched.
///
/// Returns the merged layers plus the probed user path and the resolved
/// profile identity. impure (filesystem + env); total (all failures become
/// `Err(String)`).
pub(crate) fn load_merged_config(args: &Args) -> Result<LoadedConfig, String> {
    // CTX-0346 `--safe` (R-009/P0-AC-019, spec rule 5): select the built-in
    // safe effective config and do not read any external layer. User files,
    // `BITTY_CONFIG`, profiles, and CLI appearance overrides are ignored —
    // decoration geometry is forced to `0/0/1/0/0` and the outline pair to
    // the opaque `#FFFFFF`/`#808080` built-ins, consistent with the
    // `--safe` plugin-VM guarantee. This also means a hostile or invalid
    // user config can never abort safe startup or validation. Non-safe
    // behavior is unchanged.
    if args.safe {
        let merged = bitty_config::safe_merged()
            .map_err(|err| format!("bitty: invalid built-in safe config: {err}"))?;
        return Ok(LoadedConfig {
            merged,
            probed: None,
            profile_name: None,
            profile_path: None,
        });
    }
    // Env overrides (12-factor, test-friendly): BITTY_CONFIG (path),
    // BITTY_PROFILE (name). CLI flags win over env (pure resolvers).
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let bitty_profile_env = std::env::var("BITTY_PROFILE").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    let explicit_from_cli = args
        .config_path
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    let probed = bitty_config::file::probe_config_path(explicit.as_deref());
    let mut file_layer: Option<bitty_config::LayeredPlan> = None;
    let mut used_path: Option<std::path::PathBuf> = None;
    if let Some(probe) = probed.clone() {
        if probe.path.exists() {
            match bitty_config::file::load_user_layer(&probe.path) {
                Ok(layer) => {
                    used_path = Some(probe.path.clone());
                    file_layer = Some(layer);
                }
                Err(err) => {
                    return Err(format!(
                        "bitty: invalid config file '{}': {err}",
                        probe.path.display()
                    ));
                }
            }
        } else if probe.explicit {
            if explicit_from_cli {
                return Err(format!(
                    "bitty: --config '{}' not found",
                    probe.path.display()
                ));
            }
            return Err(format!(
                "bitty: BITTY_CONFIG '{}' not found",
                probe.path.display()
            ));
        }
    }
    // Named profile (fail-closed): validate, resolve under the user-level
    // XDG root (XDG_CONFIG_DIRS never consulted), load as Profile layer.
    let profile_request = bitty_config::file::resolve_profile_request(
        args.profile.as_deref(),
        bitty_profile_env.as_deref(),
    );
    let profile_from_cli = args
        .profile
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    let mut profile_layer: Option<bitty_config::LayeredPlan> = None;
    let mut profile_path: Option<std::path::PathBuf> = None;
    if let Some(requested) = profile_request.clone() {
        let resolved = bitty_config::file::profile_file_path(&requested).map_err(|err| {
            if profile_from_cli {
                format!("bitty: invalid --profile '{requested}': {err}")
            } else {
                format!("bitty: invalid BITTY_PROFILE '{requested}': {err}")
            }
        })?;
        if !resolved.exists() {
            if profile_from_cli {
                return Err(format!(
                    "bitty: --profile '{requested}' not found ('{}')",
                    resolved.display()
                ));
            }
            return Err(format!(
                "bitty: BITTY_PROFILE '{requested}' not found ('{}')",
                resolved.display()
            ));
        }
        match bitty_config::file::load_profile_layer(&resolved) {
            Ok(layer) => {
                profile_path = Some(resolved.clone());
                profile_layer = Some(layer);
            }
            Err(err) => {
                return Err(format!(
                    "bitty: invalid profile file '{}': {err}",
                    resolved.display()
                ));
            }
        }
        // `--config` + `--profile` together: both layers stay loaded; the
        // explicit file wins over the profile (init.lua semantics). Warn so
        // the composition is never silent.
        if used_path.is_some() && probed.as_ref().is_some_and(|p| p.explicit) {
            let name = profile_request.as_deref().unwrap_or_default();
            eprintln!(
                "bitty: warning: --config with --profile '{name}': profile layers under the explicit file (explicit file wins)"
            );
        }
    }
    // CTX-0180: one Cli layer for every present appearance flag (`--theme`
    // plus `--font-family`/`--font-size`/`--opacity`). Invalid appearance
    // raws fail closed here with usage (exit 2 at the caller); the merge
    // then only sees valid CLI input, so later merge errors stay
    // file/profile-caused and keep their existing messages.
    let cli = cli_overrides_from_args(args);
    if let Err(err) = cli.validate_appearance_overrides() {
        let flag = appearance_flag_for_field(err.field());
        return Err(format!("bitty: invalid {flag}: {err}\n{}", help_text()));
    }
    let merged = bitty_config::file::resolve_effective_full(file_layer, profile_layer, &cli)
        .map_err(|err| {
            if let Some(p) = &used_path {
                format!("bitty: invalid config '{}': {err}", p.display())
            } else if let Some(p) = &profile_path {
                format!("bitty: invalid profile '{}': {err}", p.display())
            } else {
                format!("bitty: invalid --theme: {err}")
            }
        })?;
    Ok(LoadedConfig {
        merged,
        probed,
        profile_name: profile_request,
        profile_path,
    })
}

/// Loads the effective config for `args`.
///
/// - User file: `--config` wins over `BITTY_CONFIG`, else `init.lua`, else
///   the `config.lua` alias, else the canonical `init.lua` path.
/// - Profile: `--profile` wins over `BITTY_PROFILE`, loaded from
///   `profiles/<name>.lua` UNDER the user file (`init.lua` still wins).
/// - A missing **probed** file yields defaults (no error); a missing
///   **explicit** `--config`/`BITTY_CONFIG` file, or a requested-but-missing
///   profile, fails closed.
/// - A present-but-invalid file (Lua syntax/runtime/budget/shape/validation)
///   fails closed with a user-facing message (caller prints to stderr and
///   exits non-zero; no panic, no silent ignore).
/// - Merges `CLI (appearance flags) > file > profile > defaults` via `bitty-config`
///   and resolves `appearance.theme` through the preset registry.
///
/// impure (filesystem reads + env vars XDG/HOME/BITTY_*); total (all failures
/// become `Err(String)`).
pub(crate) fn load_app_config(args: &Args) -> Result<AppConfig, String> {
    let loaded = load_merged_config(args)?;
    let merged = loaded.merged;
    let probed = loaded.probed;
    let profile_request = loaded.profile_name;
    let profile_path = loaded.profile_path;
    let mut file_path: Option<std::path::PathBuf> = None;
    if let Some(probe) = &probed {
        if probe.path.exists() {
            if probe.fallback_name {
                eprintln!(
                    "bitty: using fallback '{}' (canonical is 'init.lua')",
                    probe.path.display()
                );
            }
            file_path = Some(probe.path.clone());
        }
    }
    // Attribute the theme source for title/demo/log evidence. The merge
    // attribution answers which layer won `appearance.theme`; fall back to
    // CLI-vs-file presence when the field is at defaults.
    let source: &'static str = match merged.source_of("appearance.theme").map(|s| s.layer) {
        Some(bitty_config::LayerKind::Cli) => "cli",
        Some(bitty_config::LayerKind::User) => "file",
        Some(bitty_config::LayerKind::Profile) => "profile",
        _ => "default",
    };
    let effective = merged.effective;
    let (theme, resolution) =
        bitty_config::theme::resolve_theme_with_status(effective.appearance.theme.as_deref());
    // Log unknown-theme fallbacks (the pure resolver stays silent for tests).
    if resolution == bitty_config::theme::ThemeResolution::FallbackUnknown {
        let raw = effective.appearance.theme.as_deref().unwrap_or_default();
        eprintln!(
            "bitty: unknown theme '{raw}'; falling back to '{}'",
            bitty_config::theme::DEFAULT_THEME_NAME
        );
    }
    if let Some(p) = &file_path {
        eprintln!(
            "bitty: config loaded from '{}' (theme={:?} resolution={resolution:?} source={source})",
            p.display(),
            effective.appearance.theme.as_deref().unwrap_or("(default)")
        );
    } else if args.theme.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        eprintln!(
            "bitty: CLI theme {:?} (resolution={resolution:?} source={source})",
            args.theme.as_deref().unwrap_or_default()
        );
    }
    if let (Some(name), Some(path)) = (&profile_request, &profile_path) {
        eprintln!(
            "bitty: profile '{name}' from '{}' (source={source})",
            path.display()
        );
    }
    Ok(AppConfig {
        effective,
        file_path,
        profile_name: profile_request,
        profile_path,
        theme,
        resolution,
        source,
    })
}

/// Short usage for `bitty config` (stderr, fail-closed exit 2).
pub(crate) fn config_usage() -> String {
    "usage: bitty config <path|check|edit> [--config PATH] [--profile NAME]\n\
     \n\
     verbs:\n\
     \x20 path   print the resolved config file path\n\
     \x20 check  load + validate; print per-key sources (cli/file/profile/default)\n\
     \x20 edit   open the file in $VISUAL/$EDITOR (vi fallback)\n\
     \n\
     config file: $XDG_CONFIG_HOME/bitty/init.lua (fallback config.lua alias)\n\
     profiles: $XDG_CONFIG_HOME/bitty/profiles/<name>.lua (--profile, else BITTY_PROFILE)\n\
     env: BITTY_CONFIG (path), BITTY_PROFILE (name); CLI flags win over env"
        .to_string()
}

/// Resolves the editor for `config edit` without touching the process:
/// `$VISUAL`, then `$EDITOR`, then `vi`. Pure over injected values for
/// headless tests.
pub(crate) fn resolve_editor_with_env(visual: Option<&str>, editor: Option<&str>) -> String {
    for cmd in [visual, editor].into_iter().flatten() {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "vi".to_string()
}

/// Live-environment editor resolution.
fn resolve_editor() -> String {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    resolve_editor_with_env(visual.as_deref(), editor.as_deref())
}

/// Starter `init.lua` written by `config edit` only when the file is missing.
/// Never used to overwrite existing content.
pub(crate) fn starter_init_lua() -> &'static str {
    "-- bitty user configuration (Lua, wezterm-style).\n\
     -- Evaluated in the bitty-lua sandbox (same budgets as plugins; no io/os).\n\
     -- Unknown keys fail closed; validate with `bitty config check`.\n\
     -- Chrome keys are keymap-driven (single-owner rule): a bound chord is\n\
      -- consumed by its action and never reaches the shell; unbound keys\n\
      -- (Tab, arrows, plain letters) always go to the shell. Alt is the Mod\n\
      -- (Hyprland keeps Super; bitty uses Alt). Flip one setting to rebind\n\
      -- the shipped map to Super (Alt+h/j/k/l -> Super+h/j/k/l, ...):\n\
      -- mod_key = \"super\",\n\
      -- Shipped defaults:\n\
     --   Alt+h/j/k/l or Alt+arrows + Ctrl+Alt+arrows  move focus (vim hjkl)\n\
     --   Alt+1..9                       jump to view id N\n\
     --   Alt+u / Alt+i                  page up / down (less-like)\n\
     --   Shift+Alt+h/j/k/l or Shift+Alt+arrows  split focused pane\n\
     --   Shift+Ctrl+h/j/k/l or Shift+Ctrl+arrows  resize focused pane (vim hjkl)\n\
     --   Alt+w                          close focused pane\n\
     --   Alt+z / Alt+m / Alt+f          toggle single-pane zoom\n\
     --   Ctrl+Tab / Ctrl+Shift+Tab      focus next / previous\n\
     --   Ctrl+Shift+C/V                 copy/paste (fish never sees the chord);\n\
     -- uncomment to override (context + chord identity replaces the default):\n\
     --\n\
     -- Mouse select auto-copies to the clipboard by default (ghostty-class\n\
     -- copy-on-select, syncs primary on Linux). Uncomment to opt out: the\n\
     -- highlight stays and only Ctrl+Shift+C copies.\n\
     -- selection = { auto_copy = false },\n\
     return {\n\
     \x20\x20theme = \"dark\",\n\
     \x20\x20-- Hyprland-like panel gaps in cells (0 = edge-to-edge tiling).\n\
     \x20\x20-- gaps_in spaces sibling panes, gaps_out insets the outer edge;\n\
     \x20\x20-- both render as background-colored spacing (0..=16 cells).\n\
     \x20\x20-- layout = { gaps_in = 1, gaps_out = 2 },\n\
     \x20\x20-- Core-owned workspace decoration in logical px: gaps between/\n\
     \x20\x20-- around views (unified 6/6 so panel-panel matches panel-\n\
     \x20\x20-- terminal), border inside each View frame, corner radius, and\n\
     \x20\x20-- content_inset padding between the border and the text.\n\
     \x20\x20-- Defaults 6/6/2/6/6; safe mode forces 0/0/1/0/0.\n\
     \x20\x20-- decoration = { gaps_in = 6, gaps_out = 6, border = 2, radius = 6, content_inset = 6 },\n\
     \x20\x20-- Focused/idle outline colors (CTX-0340): '#RRGGBB' or\n\
     \x20\x20-- '#RRGGBBAA'. border_color sets both states; the focused/idle\n\
     \x20\x20-- keys override it per state. Defaults #33CCFF / #595959AA.\n\
     \x20\x20-- decoration = { border_color_focused = \"#33CCFF\", border_color_idle = \"#595959AA\" },\n\
     \x20\x20-- Focused/idle outline width in logical px (0..=16). The base\n\
     \x20\x20-- inherits decoration.border; focused/idle override it. A\n\
     \x20\x20-- focused width >= idle + 1 supplies a non-color focus cue.\n\
     \x20\x20-- decoration = { border_width = 2, border_width_focused = 3, border_width_idle = 1 },\n\
      \x20\x20-- Overlay scrollback scrollbar (hidden by default: zero pixels,\n\
      \x20\x20-- zero geometry change). Uncomment to reveal on mouse proximity:\n\
      \x20\x20-- scrollbar = { mode = \"auto\", width = 8 },\n\
     \x20\x20-- Hover activation (off by default: click-to-focus preserved).\n\
     \x20\x20-- Uncomment to opt in; the optional delay in milliseconds makes\n\
     \x20\x20-- the pointer dwell in a pane before focus moves (0 = immediate,\n\
     \x20\x20-- max 2000). Alt+drag moves a floating pane where the layout\n\
     \x20\x20-- model permits.\n\
     \x20\x20-- mouse = { focus_follows_mouse = true, focus_follows_mouse_delay_ms = 0 },\n\
     \x20\x20-- keymaps = {\n\
     \x20\x20--     { chord = \"alt+h\", action = \"goto_split:left\", context = \"global\" },\n\
     \x20\x20--     { chord = \"alt+1\", action = \"focus:1\", context = \"global\" },\n\
     \x20\x20--     { chord = \"alt+u\", action = \"scroll_page_up\", context = \"global\" },\n\
     \x20\x20--     { chord = \"alt+z\", action = \"toggle_zoom\", context = \"global\" },\n\
     \x20\x20-- },\n\
     }\n"
}

/// Formats one `config check` row: `dotted.key = value (source)`.
fn check_row(key: &str, value: String, source: &str) -> String {
    format!("{key} = {value} ({source})")
}

/// Source label for a merged layer: `cli`, `file: <path>`,
/// `profile: <path>`, `default`, or the raw layer label for future layers.
pub(crate) fn layer_source_label(
    merged: &bitty_config::MergedConfig,
    field: &str,
    file_path: Option<&std::path::Path>,
    profile_path: Option<&std::path::Path>,
) -> String {
    match merged.source_of(field).map(|s| s.layer) {
        Some(bitty_config::LayerKind::Cli) => "cli".to_string(),
        Some(bitty_config::LayerKind::User) => match file_path {
            Some(p) => format!("file: {}", p.display()),
            None => "file".to_string(),
        },
        Some(bitty_config::LayerKind::Profile) => match profile_path {
            Some(p) => format!("profile: {}", p.display()),
            None => "profile".to_string(),
        },
        Some(layer) => {
            if layer == bitty_config::LayerKind::CoreDefaults {
                "default".to_string()
            } else {
                layer.label().to_string()
            }
        }
        None => "default".to_string(),
    }
}

/// Runs `bitty config <verb>`; returns the process exit code.
///
/// - `path`: print resolved path (0) or fail closed (2) when no root exists.
/// - `check`: load + validate via the startup path, print per-key sources
///   (0); invalid files reuse the startup error verbatim (2).
/// - `edit`: mkdir parents + starter-when-missing, open `$VISUAL`/`$EDITOR`
///   (0 on editor success; 1 on spawn failure; editor non-zero propagates).
pub(crate) fn run_config_subcommand(cmd: ConfigCommand, args: &Args) -> i32 {
    if !args.config_args.is_empty() {
        eprintln!(
            "bitty config {}: unexpected argument '{}'\n{}",
            cmd.name(),
            args.config_args[0],
            config_usage()
        );
        return 2;
    }
    // BITTY_CONFIG env participates exactly like --config (CLI wins).
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    match cmd {
        ConfigCommand::Path => match bitty_config::file::probe_config_path(explicit.as_deref()) {
            Some(probed) => {
                println!("{}", probed.path.display());
                0
            }
            None => {
                eprintln!("bitty config path: no config root ($XDG_CONFIG_HOME or $HOME unset)");
                2
            }
        },
        ConfigCommand::Check => match load_merged_config(args) {
            Ok(loaded) => {
                let merged = loaded.merged;
                let probed = loaded.probed;
                let profile_request = loaded.profile_name;
                let profile_path = loaded.profile_path;
                let file_path = probed
                    .as_ref()
                    .filter(|p| p.path.exists())
                    .map(|p| p.path.clone());
                // MSRV 1.85: no let-chains; nest instead of `if let ... && ...`.
                if let Some(probe) = &probed {
                    if probe.fallback_name && probe.path.exists() {
                        eprintln!(
                            "bitty: using fallback '{}' (canonical is 'init.lua')",
                            probe.path.display()
                        );
                    }
                }
                // Profile identity first so per-key `profile: <path>`
                // sources have context even when the profile loses every key.
                if let (Some(name), Some(path)) = (&profile_request, &profile_path) {
                    println!(
                        "{}",
                        check_row(
                            "profile",
                            format!("\"{name}\""),
                            &format!("profile: {}", path.display()),
                        )
                    );
                } else {
                    println!(
                        "{}",
                        check_row("profile", String::from("(none)"), "default")
                    );
                }
                let e = &merged.effective;
                let src = |field: &str| {
                    layer_source_label(
                        &merged,
                        field,
                        file_path.as_deref(),
                        profile_path.as_deref(),
                    )
                };
                let theme = e
                    .appearance
                    .theme
                    .as_deref()
                    .map_or(String::from("(default)"), |t| format!("\"{t}\""));
                println!(
                    "{}",
                    check_row("appearance.theme", theme, &src("appearance.theme"))
                );
                println!(
                    "{}",
                    check_row(
                        "font.family",
                        format!("\"{}\"", e.font.family),
                        &src("font.family")
                    )
                );
                println!(
                    "{}",
                    check_row("font.size", format!("{}", e.font.size), &src("font.size"))
                );
                println!(
                    "{}",
                    check_row(
                        "window.opacity",
                        format!("{}", e.window.opacity),
                        &src("window.opacity")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "window.padding",
                        format!("{}", e.window.padding),
                        &src("window.padding")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "window.radius_px",
                        format!("{}", e.window.radius_px),
                        &src("window.radius_px")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.scrollback",
                        format!("{}", e.terminal.scrollback),
                        &src("terminal.scrollback")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.shell",
                        e.terminal
                            .shell
                            .as_deref()
                            .map_or(String::from("(unset)"), |v| format!("\"{v}\"")),
                        &src("terminal.shell")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.scroll_lines_per_notch",
                        format!("{}", e.terminal.scroll_lines_per_notch),
                        &src("terminal.scroll_lines_per_notch")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.scroll_pixels_per_notch",
                        format!("{}", e.terminal.scroll_pixels_per_notch),
                        &src("terminal.scroll_pixels_per_notch")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "selection.auto_copy",
                        format!("{}", e.selection.auto_copy),
                        &src("selection.auto_copy")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "layout.gaps_in",
                        format!("{}", e.layout.gaps_in),
                        &src("layout.gaps_in")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "layout.gaps_out",
                        format!("{}", e.layout.gaps_out),
                        &src("layout.gaps_out")
                    )
                );
                for (field, value) in [
                    ("decoration.gaps_in", e.decoration.gaps_in),
                    ("decoration.gaps_out", e.decoration.gaps_out),
                    ("decoration.border", e.decoration.border),
                    ("decoration.radius", e.decoration.radius),
                    ("decoration.content_inset", e.decoration.content_inset),
                ] {
                    println!("{}", check_row(field, format!("{value}"), &src(field)));
                }
                // CTX-0344: report the resolved outline-width triple. The
                // base inherits `decoration.border`; the focused/idle rows are
                // the resolved values with their source.
                let resolved_widths = e.decoration.resolve_outline_width();
                let base_width = e.decoration.border_width.unwrap_or(e.decoration.border);
                println!(
                    "{}",
                    check_row(
                        "decoration.border_width",
                        e.decoration
                            .border_width
                            .map_or_else(|| format!("{base_width} (inherited)"), |v| v.to_string()),
                        &src("decoration.border_width"),
                    )
                );
                let width_pair_source = |explicit: bool| -> String {
                    if explicit {
                        String::new()
                    } else if e.decoration.border_width.is_some() {
                        String::from("inherited decoration.border_width")
                    } else {
                        String::from("inherited decoration.border")
                    }
                };
                for (field, value, explicit) in [
                    (
                        "decoration.border_width_focused",
                        resolved_widths.focused,
                        e.decoration.border_width_focused.is_some(),
                    ),
                    (
                        "decoration.border_width_idle",
                        resolved_widths.idle,
                        e.decoration.border_width_idle.is_some(),
                    ),
                ] {
                    let source = width_pair_source(explicit);
                    let source = if source.is_empty() {
                        src(field)
                    } else {
                        format!("resolved: {source}")
                    };
                    println!("{}", check_row(field, value.to_string(), &source));
                }
                // CTX-0340: report the resolved focused/idle outline pair
                // (theme token / base / explicit pair) with its source, plus
                // the advisory AC-3 idle-contrast note when it is below the
                // 1.5:1 floor. AC-3 never changes the exit code.
                let theme = bitty_config::theme::resolve_theme(e.appearance.theme.as_deref());
                let outline = e.decoration.resolve_outline(theme);
                println!(
                    "{}",
                    check_row(
                        "decoration.border_color",
                        e.decoration
                            .border_color
                            .map_or_else(|| String::from("(unset)"), |c| c.to_hex()),
                        &src("decoration.border_color"),
                    )
                );
                // The focused/idle rows are the *resolved* values; their
                // source is the explicit key when set, else the base, else the
                // theme token.
                let pair_source = |explicit: bool| -> String {
                    if explicit {
                        String::new()
                    } else if e.decoration.border_color.is_some() {
                        String::from("inherited decoration.border_color")
                    } else {
                        String::from("theme token")
                    }
                };
                let focused_explicit = e.decoration.border_color_focused.is_some();
                let idle_explicit = e.decoration.border_color_idle.is_some();
                for (field, value, explicit) in [
                    (
                        "decoration.border_color_focused",
                        outline.focused.to_hex(),
                        focused_explicit,
                    ),
                    (
                        "decoration.border_color_idle",
                        outline.idle.to_hex(),
                        idle_explicit,
                    ),
                ] {
                    let source = pair_source(explicit);
                    let source = if source.is_empty() {
                        src(field)
                    } else {
                        format!("resolved: {source}")
                    };
                    println!("{}", check_row(field, value, &source));
                }
                if let Some(warning) = e.decoration.idle_contrast_warning(theme) {
                    println!(
                        "{}",
                        check_row("decoration.border_color_idle.advisory", warning, "advisory")
                    );
                }
                // CTX-0341 (RFC-0002): report the resolved animation contract
                // (durations/easings are already `spring`-resolved and
                // bounded; the reload diff is live).
                for (field, value) in [
                    (
                        "appearance.animations.enabled",
                        e.animations.enabled.to_string(),
                    ),
                    (
                        "appearance.animations.reduced_motion",
                        e.animations.reduced_motion.as_str().to_string(),
                    ),
                    (
                        "appearance.animations.duration_ms.open",
                        e.animations.duration_ms.open.to_string(),
                    ),
                    (
                        "appearance.animations.duration_ms.close",
                        e.animations.duration_ms.close.to_string(),
                    ),
                    (
                        "appearance.animations.duration_ms.focus",
                        e.animations.duration_ms.focus.to_string(),
                    ),
                    (
                        "appearance.animations.duration_ms.workspace",
                        e.animations.duration_ms.workspace.to_string(),
                    ),
                    (
                        "appearance.animations.easing.open",
                        e.animations.easing.open.as_str().to_string(),
                    ),
                    (
                        "appearance.animations.easing.close",
                        e.animations.easing.close.as_str().to_string(),
                    ),
                    (
                        "appearance.animations.easing.focus",
                        e.animations.easing.focus.as_str().to_string(),
                    ),
                    (
                        "appearance.animations.easing.workspace",
                        e.animations.easing.workspace.as_str().to_string(),
                    ),
                ] {
                    println!("{}", check_row(field, value, &src(field)));
                }
                println!(
                    "{}",
                    check_row(
                        "scrollbar.mode",
                        e.scrollbar.mode.as_str().to_string(),
                        &src("scrollbar.mode")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "scrollbar.width",
                        format!("{}", e.scrollbar.width),
                        &src("scrollbar.width")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "mouse.focus_follows_mouse",
                        format!("{}", e.mouse.focus_follows_mouse),
                        &src("mouse.focus_follows_mouse")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "mouse.focus_follows_mouse_delay_ms",
                        format!("{}", e.mouse.focus_follows_mouse_delay_ms),
                        &src("mouse.focus_follows_mouse_delay_ms")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "mod_key",
                        format!("\"{}\"", e.mod_key.canonical()),
                        &src("mod_key")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "keymaps",
                        format!("{} entries", e.keymaps.len()),
                        &src("keymaps")
                    )
                );
                // CLI-printable keymap introspection (DEC-0007): one row per
                // resolved binding, `user` entries from the config file and
                // the rest from the shipped defaults (CTX-0153).
                match bitty_config::keymap::resolve_keymaps(e) {
                    Ok(maps) => {
                        for m in &maps {
                            let origin = if m.from_default {
                                String::from("default")
                            } else {
                                match &file_path {
                                    Some(p) => format!("file: {}", p.display()),
                                    None => String::from("file"),
                                }
                            };
                            println!(
                                "{}",
                                check_row(
                                    &format!("keymaps[{}]", m.id()),
                                    format!(
                                        "\"{}\" -> {} ({})",
                                        m.chord.canonical(),
                                        m.action.canonical(),
                                        m.context
                                    ),
                                    &origin
                                )
                            );
                        }
                    }
                    Err(err) => {
                        eprintln!("bitty config check: invalid keymaps: {err}");
                        return 2;
                    }
                }
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                2
            }
        },
        ConfigCommand::Edit => {
            let probed = bitty_config::file::probe_config_path(explicit.as_deref());
            let target = match probed {
                Some(p) => p.path,
                None => {
                    eprintln!(
                        "bitty config edit: no config root ($XDG_CONFIG_HOME or $HOME unset)"
                    );
                    return 2;
                }
            };
            if !target.exists() {
                // MSRV 1.85: no let-chains; nest instead of `if let ... && let ...`.
                if let Some(parent) = target.parent() {
                    if let Err(err) = std::fs::create_dir_all(parent) {
                        eprintln!(
                            "bitty config edit: cannot create '{}': {err}",
                            parent.display()
                        );
                        return 1;
                    }
                }
                // Starter only when missing: never clobbers existing content.
                if let Err(err) = std::fs::write(&target, starter_init_lua()) {
                    eprintln!(
                        "bitty config edit: cannot write '{}': {err}",
                        target.display()
                    );
                    return 1;
                }
                eprintln!("bitty config edit: created '{}'", target.display());
            }
            let editor = resolve_editor();
            match std::process::Command::new(&editor).arg(&target).status() {
                Ok(status) if status.success() => 0,
                Ok(status) => status.code().unwrap_or(1),
                Err(err) => {
                    eprintln!("bitty config edit: cannot run editor '{editor}': {err}");
                    1
                }
            }
        }
    }
}
/// Derives a [`bitty_runtime::RuntimeConfig`] from the effective config.
///
/// Cell geometry applies the configured breathing room
/// (`font.line_height`/`font.letter_spacing` over the legacy `8x16` base via
/// [`bitty_config::types::FontConfig::effective_cell`], defaults `10x22`);
/// grid/queue geometry stays at compiled defaults; font family/size, scroll
/// speed, selection auto-copy, panel gaps, and hover-focus come from the
/// file/CLI/default chain (already validated by `bitty-config`, so
/// construction is expected to succeed — failures stay fail-closed).
pub(crate) fn runtime_config_from_effective(
    effective: &bitty_config::EffectiveConfig,
) -> Result<bitty_runtime::RuntimeConfig, String> {
    let defaults = bitty_runtime::RuntimeConfig::default();
    let (cell_width, cell_height) = effective.font.default_effective_cell();
    // CTX-0177: `bitty-config` validates `0..=MAX_LAYOUT_GAP_CELLS` (u32) and
    // `bitty-runtime` mirrors the bound in u16; the clamp below is
    // defense-in-depth so a future bound drift can never wrap the cast.
    let gaps_in = effective
        .layout
        .gaps_in
        .min(u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS)) as u16;
    let gaps_out = effective
        .layout
        .gaps_out
        .min(u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS)) as u16;
    // CTX-0223: `window.padding` flows file -> effective -> runtime the same
    // way (validated `0..=64` by `bitty-config`; clamped here so a future
    // bound drift can never wrap the cast).
    let window_padding = effective
        .window
        .padding
        .min(bitty_runtime::config::MAX_WINDOW_PADDING);
    // CTX-0241 S0: `window.radius_px` flows the same way (validated
    // `0..=24` by `bitty-config`; clamped here so a future bound drift can
    // never wrap the cast). S0 is a parsed no-op: stored on the runtime
    // config with zero render effect (default 0 = zero-cost everywhere).
    // CTX-0181: `scrollbar` flows the same way. The mode enum is paired by
    // value (`bitty-runtime` owns no `bitty-config` dependency); the match
    // is total with a hidden-default fallback so a future variant drift can
    // never misroute chrome into visibility.
    let scrollbar_mode = match effective.scrollbar.mode.as_str() {
        "always" => bitty_runtime::ScrollbarMode::Always,
        "auto" => bitty_runtime::ScrollbarMode::Auto,
        _ => bitty_runtime::ScrollbarMode::Hidden,
    };
    let scrollbar_width = effective
        .scrollbar
        .width
        .min(bitty_runtime::config::MAX_SCROLLBAR_WIDTH_PX);
    let window_radius_px = effective
        .window
        .radius_px
        .min(bitty_runtime::config::MAX_WINDOW_RADIUS_PX);
    // CTX-0292: Core-owned workspace decoration flows file -> effective ->
    // runtime (`bitty-config` validates the accepted CTX-0118 ranges; the
    // clamps below are defense-in-depth so a future bound drift can never
    // wrap the u16 cast).
    let decoration = bitty_runtime::Decoration::new(
        effective
            .decoration
            .gaps_in
            .min(u32::from(bitty_runtime::config::MAX_DECORATION_GAP_PX)) as u16,
        effective
            .decoration
            .gaps_out
            .min(u32::from(bitty_runtime::config::MAX_DECORATION_GAP_PX)) as u16,
        effective
            .decoration
            .border
            .min(u32::from(bitty_runtime::config::MAX_DECORATION_BORDER_PX)) as u16,
        effective
            .decoration
            .radius
            .min(u32::from(bitty_runtime::config::MAX_DECORATION_RADIUS_PX)) as u16,
        effective.decoration.content_inset.min(u32::from(
            bitty_runtime::config::MAX_DECORATION_CONTENT_INSET_PX,
        )) as u16,
    );
    // CTX-0297: `terminal.scrollback` bounds retained history at terminal
    // creation. Unlike the clamped geometry knobs above, an out-of-range
    // value fails closed here (and in `RuntimeConfig::validate`) instead of
    // silently clamping, so a future `bitty-config`/`bitty-runtime` bound
    // drift cannot quietly change retention semantics.
    let scrollback = usize::try_from(effective.terminal.scrollback)
        .map_err(|_| "bitty: terminal.scrollback out of range".to_string())?;
    if scrollback > bitty_runtime::config::MAX_SCROLLBACK_LINES {
        return Err(format!(
            "bitty: terminal.scrollback {} exceeds the supported maximum {}",
            scrollback,
            bitty_runtime::config::MAX_SCROLLBACK_LINES
        ));
    }
    bitty_runtime::RuntimeConfig::new(
        defaults.cols,
        defaults.rows,
        cell_width,
        cell_height,
        defaults.cold_queue_capacity,
        effective.font.family.clone(),
        effective.font.size,
        effective.terminal.scroll_lines_per_notch,
        effective.terminal.scroll_pixels_per_notch,
        effective.selection.auto_copy,
        gaps_in,
        gaps_out,
        window_padding,
        window_radius_px,
        scrollbar_mode,
        scrollbar_width,
    )
    .map(|mut cfg| {
        // CTX-0260: hover-focus flows file -> effective -> runtime the same
        // way (booleans are total; default off preserves click-to-focus).
        cfg.focus_follows_mouse = effective.mouse.focus_follows_mouse;
        // CTX-0334: the dwell delay flows the same way. `bitty-config`
        // already bounds it fail-closed; clamp here as defense-in-depth so
        // a future bound drift can never exceed the runtime's timer bound.
        cfg.focus_follows_mouse_delay = std::time::Duration::from_millis(u64::from(
            effective
                .mouse
                .focus_follows_mouse_delay_ms
                .min(bitty_runtime::config::MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS),
        ));
        // CTX-0292: Core-owned workspace decoration is carried onto the
        // validated runtime config (same post-construction pattern as
        // `focus_follows_mouse`).
        cfg.decoration = decoration;
        // CTX-0340: resolve the focused/idle outline pair from the theme
        // token / `decoration.border_color` / explicit pair and carry it onto
        // the runtime config. The `bitty-config` validation already enforced
        // AC-1/AC-2 fail-closed against the same theme background.
        let theme = bitty_config::theme::resolve_theme(effective.appearance.theme.as_deref());
        let outline = effective.decoration.resolve_outline(theme);
        cfg.outline_focused = outline.focused.0;
        cfg.outline_idle = outline.idle.0;
        // CTX-0344 (RFC-0001/OQ-045): resolve the focus/idle outline-width
        // pair (`decoration.border` -> `decoration.border_width` -> explicit
        // pair). Bounds were already enforced fail-closed in `bitty-config`;
        // clamp as defense-in-depth so a future bound drift cannot exceed the
        // runtime's own bound.
        let widths = effective.decoration.resolve_outline_width();
        cfg.outline_width_focused = Some(
            widths
                .focused
                .min(bitty_runtime::config::MAX_OUTLINE_WIDTH_PX),
        );
        cfg.outline_width_idle = Some(widths.idle.min(bitty_runtime::config::MAX_OUTLINE_WIDTH_PX));
        // RFC-0002 (CTX-0341): map the resolved effective animation contract
        // onto the runtime policy. Durations are already bounded by
        // `bitty-config` (fail-closed `0..=500`); `spring` was already mapped
        // to `ease_in_out` by the resolver. `safe_mode` is encoded into the
        // effective durations (`with_safe_decoration` zeroes them), so the
        // runtime policy needs no separate flag here.
        cfg.animations = animation_policy_from_effective(effective);
        // CTX-0297: effective `terminal.scrollback` is carried the same way;
        // terminal creation captures it as the retention cap.
        cfg.scrollback = scrollback;
        cfg
    })
    .map_err(|err| format!("bitty: invalid effective config for runtime: {err}"))
}

/// Maps the resolved `bitty-config` animation contract onto the runtime
/// policy (RFC-0002, CTX-0341).
///
/// Durations are clamped defensively to the accepted `0..=500` ms hard bound
/// (already enforced fail-closed by `bitty-config`), and `spring` easings are
/// resolved to `ease_in_out` at the boundary so the runtime engine has no
/// deferred curve. `safe_mode` and `platform_reduced` are left false here:
/// safe mode is already encoded as zero effective durations, and the platform
/// reduced-motion signal is owned by the app/embedder (no platform query
/// exists in this slice, documented as an honest gap in RFC-0002 verification).
fn animation_policy_from_effective(
    effective: &bitty_config::EffectiveConfig,
) -> bitty_runtime::AnimationPolicy {
    use bitty_config::types::AnimationEasing as CEasing;
    let map_curve = |e: CEasing| match e.resolved() {
        CEasing::Linear => bitty_runtime::AnimationCurve::Linear,
        CEasing::EaseIn => bitty_runtime::AnimationCurve::EaseIn,
        CEasing::EaseOut => bitty_runtime::AnimationCurve::EaseOut,
        CEasing::EaseInOut | CEasing::Spring => bitty_runtime::AnimationCurve::EaseInOut,
    };
    let reduced = match effective.animations.reduced_motion {
        bitty_config::types::ReducedMotion::Always => bitty_runtime::ReducedMotionMode::Always,
        bitty_config::types::ReducedMotion::Never => bitty_runtime::ReducedMotionMode::Never,
        bitty_config::types::ReducedMotion::Auto => bitty_runtime::ReducedMotionMode::Auto,
    };
    let clamped = |ms: u32| ms.min(bitty_runtime::config::MAX_ANIMATION_DURATION_MS);
    bitty_runtime::AnimationPolicy {
        enabled: effective.animations.enabled,
        duration_ms: [
            clamped(effective.animations.duration_ms.open),
            clamped(effective.animations.duration_ms.close),
            clamped(effective.animations.duration_ms.focus),
            clamped(effective.animations.duration_ms.workspace),
        ],
        curves: [
            map_curve(effective.animations.easing.open),
            map_curve(effective.animations.easing.close),
            map_curve(effective.animations.easing.focus),
            map_curve(effective.animations.easing.workspace),
        ],
        reduced_motion: reduced,
        safe_mode: false,
        platform_reduced: false,
    }
}
