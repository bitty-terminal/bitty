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

/// Whether the startup palette resolves a dual `appearance.theme` to its
/// light half (CTX-0951 NEEDS-FIX, issue #1669).
///
/// Maps the cold-path [`bitty_platform::query_system_appearance`] signal:
/// `Light` → `true`, `Dark`/`Unknown` → `false` (dark-first default, matching
/// `EffectiveConfig::effective_theme`). The query degrades to `Unknown` on
/// every platform in this slice (no native portal/AppKit/registry seam yet),
/// so startup stays dark-first until those seams land; live `ThemeChanged`
/// events drive later swaps via `Runtime::apply_system_appearance`.
pub(crate) fn startup_prefer_light() -> bool {
    matches!(
        bitty_platform::query_system_appearance(),
        bitty_platform::SystemAppearance::Light
    )
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

/// A config-load failure plus whether the first-start fallback may absorb it.
///
/// Only a *present* user config file that cannot be loaded or merged is
/// recoverable (`broken`). Explicitly missing `--config`/`BITTY_CONFIG` paths,
/// missing or invalid profiles, CLI usage errors, and an invalid built-in
/// plan stay fail-closed (`fatal`), matching `lua-and-xdg.md` §Layers.
struct LoadFailure {
    /// User-facing message; unchanged from the pre-fallback behavior.
    message: String,
    /// True only for a present-but-unusable external user file.
    fallback_allowed: bool,
}

impl LoadFailure {
    /// A present external layer is unusable; first start may use built-ins.
    fn broken(message: String) -> Self {
        Self {
            message,
            fallback_allowed: true,
        }
    }

    /// Missing input, usage error, profile selection, or internal defect.
    fn fatal(message: String) -> Self {
        Self {
            message,
            fallback_allowed: false,
        }
    }

    fn into_message(self) -> String {
        self.message
    }
}

/// Attributes a merge failure to the layer that actually caused it.
///
/// `user_file_causal` is the outcome of re-merging without the user layer:
/// when that re-merge succeeds, the user file's contribution is what makes
/// the combined config invalid, so first start may fall back. When it still
/// fails, a profile or CLI layer owns the error and startup stays
/// fail-closed (CTX-1339 review finding: file presence alone is not causal).
fn classify_merge_failure(
    err: &bitty_config::ConfigError,
    user_path: Option<&std::path::Path>,
    profile_path: Option<&std::path::Path>,
    user_file_causal: bool,
) -> LoadFailure {
    if user_file_causal {
        if let Some(path) = user_path {
            return LoadFailure::broken(format!(
                "bitty: invalid config '{}': {err}",
                path.display()
            ));
        }
    }
    if let Some(path) = profile_path {
        return LoadFailure::fatal(format!(
            "bitty: invalid profile '{}': {err}",
            path.display()
        ));
    }
    LoadFailure::fatal(format!("bitty: invalid --theme: {err}"))
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
/// fail-closed message. `theme` is pre-validated by CTX-1339 (it used to fail
/// only at merge); any unexpected field falls back to the flag group.
pub(crate) fn appearance_flag_for_field(field: Option<&str>) -> &'static str {
    match field {
        Some("appearance.theme") => "--theme",
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
    resolve_merged_config(args).map_err(LoadFailure::into_message)
}

/// Classified form of [`load_merged_config`] for the startup fallback.
fn resolve_merged_config(args: &Args) -> Result<LoadedConfig, LoadFailure> {
    // CTX-0346 `--safe` (R-009/P0-AC-019, spec rule 5): select the built-in
    // safe effective config and do not read any external layer. User files,
    // `BITTY_CONFIG`, profiles, and CLI appearance overrides are ignored —
    // decoration geometry is forced to `0/0/1/0/0` and the outline pair to
    // the opaque `#FFFFFF`/`#808080` built-ins, consistent with the
    // `--safe` plugin-VM guarantee. This also means a hostile or invalid
    // user config can never abort safe startup or validation. Non-safe
    // behavior is unchanged.
    if args.safe {
        let merged = bitty_config::safe_merged().map_err(|err| {
            LoadFailure::fatal(format!("bitty: invalid built-in safe config: {err}"))
        })?;
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
                    return Err(LoadFailure::broken(format!(
                        "bitty: invalid config file '{}': {err}",
                        probe.path.display()
                    )));
                }
            }
        } else if probe.explicit {
            if explicit_from_cli {
                return Err(LoadFailure::fatal(format!(
                    "bitty: --config '{}' not found",
                    probe.path.display()
                )));
            }
            return Err(LoadFailure::fatal(format!(
                "bitty: BITTY_CONFIG '{}' not found",
                probe.path.display()
            )));
        }
    }
    // Named profile (fail-closed): validate, resolve under the user-level
    // XDG root (XDG_CONFIG_DIRS never consulted), load as Profile layer.
    //
    // CTX-1404 selection order: `--profile` (C) > `BITTY_PROFILE` (E) >
    // persisted User selector (S). An explicit one-launch selection always
    // wins and never rewrites the selector; a persisted selector is used
    // only when neither C nor E is present. A broken selected chain fails
    // closed instead of falling back to the other source.
    let cli_profile_request = bitty_config::file::resolve_profile_request(
        args.profile.as_deref(),
        bitty_profile_env.as_deref(),
    );
    let selector_request = file_layer
        .as_ref()
        .and_then(|layer| layer.plan.selector.as_ref())
        .map(|selector| selector.profile.clone());
    let profile_from_selector = cli_profile_request.is_none() && selector_request.is_some();
    let profile_request = cli_profile_request.or(selector_request);
    let profile_from_cli = args
        .profile
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    // A persisted selector resolves beside the user file that carries it:
    // the default root yields `<config>/bitty/profiles`, while an explicit
    // `--config /custom/init.lua` yields `/custom/profiles` (matching
    // `bitty init`, which writes profiles beside the explicit selector).
    // One-launch `--profile`/`BITTY_PROFILE` keep the user-root resolver.
    let selector_profile_dir = used_path
        .as_ref()
        .and_then(|path| path.parent())
        .map(|parent| parent.join(bitty_config::file::PROFILES_DIR_NAME));
    let mut profile_layers: Vec<bitty_config::LayeredPlan> = Vec::new();
    let mut profile_path: Option<std::path::PathBuf> = None;
    if let Some(requested) = profile_request.clone() {
        let resolved = if profile_from_selector {
            match &selector_profile_dir {
                Some(dir) => Ok(dir.join(format!("{requested}.lua"))),
                None => bitty_config::file::profile_file_path(&requested),
            }
        } else {
            bitty_config::file::profile_file_path(&requested)
        }
        .map_err(|err| {
            if profile_from_cli {
                LoadFailure::fatal(format!("bitty: invalid --profile '{requested}': {err}"))
            } else if profile_from_selector {
                LoadFailure::fatal(format!(
                    "bitty: invalid selected profile '{requested}': {err}"
                ))
            } else {
                LoadFailure::fatal(format!("bitty: invalid BITTY_PROFILE '{requested}': {err}"))
            }
        })?;
        if !resolved.exists() {
            if profile_from_cli {
                return Err(LoadFailure::fatal(format!(
                    "bitty: --profile '{requested}' not found ('{}')",
                    resolved.display()
                )));
            }
            if profile_from_selector {
                return Err(LoadFailure::fatal(format!(
                    "bitty: selected profile '{requested}' not found ('{}')",
                    resolved.display()
                )));
            }
            return Err(LoadFailure::fatal(format!(
                "bitty: BITTY_PROFILE '{requested}' not found ('{}')",
                resolved.display()
            )));
        }
        // CTX-0759 (#1366): resolve the single-parent `extends` chain
        // (base-first `Profile` layers); the child overrides the base per
        // field. Chain failures (cycle, missing parent, invalid name) fail
        // closed with the `extends` field path.
        let chain = match (&selector_profile_dir, profile_from_selector) {
            (Some(dir), true) => bitty_config::file::load_profile_chain_in_dir(&requested, dir),
            _ => bitty_config::file::load_profile_chain(&requested),
        };
        match chain {
            Ok(layers) => {
                profile_path = Some(resolved.clone());
                profile_layers = layers;
            }
            Err(err) => {
                return Err(LoadFailure::fatal(format!(
                    "bitty: invalid profile file '{}': {err}",
                    resolved.display()
                )));
            }
        }
        // `--config` + `--profile` together: both layers stay loaded; the
        // explicit file wins over the profile (init.lua semantics). Warn so
        // the composition is never silent.
        if !profile_from_selector
            && used_path.is_some()
            && probed.as_ref().is_some_and(|p| p.explicit)
        {
            let name = profile_request.as_deref().unwrap_or_default();
            eprintln!(
                "bitty: warning: --config with --profile '{name}': profile layers under the explicit file (explicit file wins)"
            );
        }
    }
    // CTX-0180: one Cli layer for every present appearance flag (`--theme`
    // plus `--font-family`/`--font-size`/`--opacity`). Invalid appearance
    // raws (including an unknown `--theme`, CTX-1339) fail closed here with
    // usage (exit 2 at the caller), so later merge errors are attributable
    // to the merged layers rather than to CLI input.
    let cli = cli_overrides_from_args(args);
    if let Err(err) = cli.validate_appearance_overrides() {
        let flag = appearance_flag_for_field(err.field());
        return Err(LoadFailure::fatal(format!(
            "bitty: invalid {flag}: {err}\n{}",
            help_text()
        )));
    }
    // CTX-1339: a user file is only blameable for a merge failure when the
    // merge succeeds without it. Re-merging without the user layer (profiles
    // and CLI only) distinguishes a broken file from a profile/CLI error that
    // merely surfaced while a valid file was present. The retry only runs on
    // the failure path and only when a user file actually participated.
    let retry_profiles = used_path.as_ref().map(|_| profile_layers.clone());
    let merged =
        match bitty_config::file::resolve_effective_with_profiles(file_layer, profile_layers, &cli)
        {
            Ok(merged) => merged,
            Err(err) => {
                let user_file_causal = retry_profiles.is_some_and(|profiles| {
                    bitty_config::file::resolve_effective_with_profiles(None, profiles, &cli)
                        .is_ok()
                });
                return Err(classify_merge_failure(
                    &err,
                    used_path.as_deref(),
                    profile_path.as_deref(),
                    user_file_causal,
                ));
            }
        };
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
    resolve_app_config(args).map_err(LoadFailure::into_message)
}

/// Classified form of [`load_app_config`] for the startup fallback.
fn resolve_app_config(args: &Args) -> Result<AppConfig, LoadFailure> {
    let loaded = resolve_merged_config(args)?;
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
    // CTX-0951: dual `light:<name>,dark:<name>` resolves to the active half
    // via the startup OS query (dark-first while the query degrades to
    // Unknown). Single names keep the existing status path; unknown halves
    // stay FallbackUnknown observers (validation already failed closed).
    let prefer_light = startup_prefer_light();
    let (theme, resolution) = bitty_config::theme::resolve_selection_with_status(
        effective.appearance.theme.as_deref(),
        prefer_light,
    );
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

/// Result of the process-startup config resolution (CTX-1339, issue #2225).
pub(crate) struct StartupConfig {
    /// The config startup continues with.
    pub(crate) app: AppConfig,
    /// User-facing diagnostics to print before startup proceeds; `None` when
    /// the external configuration loaded normally.
    pub(crate) diagnostics: Option<String>,
}

/// Whether this process has no last-good config plan (R-009 first start).
///
/// The predicate is existing state, not a launch counter: the reload context
/// owns the last-good plan and is installed only after a successful startup
/// load (`config_reload::install`). At startup `current_effective()` is
/// `None`, so there is no plan to retain and the first-start fallback applies.
/// Once the context exists, every later failure keeps `engine.current()`
/// (`reload_into` reports `load-error` without replacing the plan), so the
/// fallback never runs there.
fn is_first_start() -> bool {
    crate::config_reload::current_effective().is_none()
}

/// Resolve the startup config, degrading to [`bitty_config::LayerKind::CoreDefaults`]
/// on first start.
///
/// Pure over the classified load result and the predicate so both branches are
/// deterministically testable. `first_start` is [`is_first_start`] in
/// production; a later failure (an installed last-good plan) stays fail-closed
/// and returns the original message unchanged.
///
/// Only a present-but-unusable user config file is recoverable. Explicitly
/// missing `--config`/`BITTY_CONFIG` paths, profile selection failures, CLI
/// usage errors, and an invalid built-in plan keep their fail-closed exit.
///
/// The fallback is the unmodified built-in plan: no file, profile, CLI, or
/// safe-mode decoration participates, so startup never exits on unusable
/// external configuration and never claims values the user did not provide.
///
/// # Errors
///
/// Returns the original load message when no first-start fallback applies, or
/// a diagnostic if the built-in plan itself is invalid (defense in depth).
fn resolve_startup_config(
    loaded: Result<AppConfig, LoadFailure>,
    first_start: bool,
) -> Result<StartupConfig, String> {
    match loaded {
        Ok(app) => Ok(StartupConfig {
            app,
            diagnostics: None,
        }),
        Err(failure) if first_start && failure.fallback_allowed => Ok(StartupConfig {
            app: builtin_app_config()?,
            diagnostics: Some(format!(
                "{}\nbitty: first start with unusable configuration; continuing with the built-in defaults (fix the configuration and restart; 'bitty config check' reports details)",
                failure.message
            )),
        }),
        Err(failure) => Err(failure.message),
    }
}

/// Loads the config for the process-startup path, applying the R-009
/// first-start fallback.
///
/// Startup-only: the reload resolver keeps calling [`load_app_config`], so a
/// later broken reload retains its last-good plan instead of replacing it with
/// built-ins.
///
/// # Errors
///
/// Same as [`resolve_startup_config`].
pub(crate) fn load_startup_config(args: &Args) -> Result<StartupConfig, String> {
    resolve_startup_config(resolve_app_config(args), is_first_start())
}

/// Builds the unmodified `CoreDefaults` plan used by the first-start fallback.
fn builtin_app_config() -> Result<AppConfig, String> {
    // An empty layer stack merges to built-in defaults with `CoreDefaults`
    // attribution and runs the same validation as the normal merge.
    let merged = bitty_config::merge_layers(Vec::new())
        .map_err(|err| format!("bitty: invalid built-in config: {err}"))?;
    let effective = merged.effective;
    let (theme, resolution) = bitty_config::theme::resolve_selection_with_status(
        effective.appearance.theme.as_deref(),
        startup_prefer_light(),
    );
    Ok(AppConfig {
        effective,
        file_path: None,
        profile_name: None,
        profile_path: None,
        theme,
        resolution,
        source: "default",
    })
}

/// Short usage for `bitty config` (stderr, fail-closed exit 2).
pub(crate) fn config_usage() -> String {
    "usage: bitty config <path|check|edit|profile> [--config PATH] [--profile NAME]\n\
     \n\
     verbs:\n\
     \x20 path     print the resolved config file path\n\
     \x20 check    load + validate; print per-key sources (cli/file/profile/default)\n\
     \x20 edit     open the file in $VISUAL/$EDITOR (vi fallback)\n\
     \x20 profile  manage named profiles (list|show|use|path NAME)\n\
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

/// Expands one `~`-anchored background-image path against `$HOME`
/// (CTX-0347, RFC-0001/OQ-042).
///
/// `bitty-config` validates the accepted syntax (`absolute | ~`); the app is
/// the environment boundary, so it resolves the home anchor once at config
/// mapping and the runtime/loader only ever see absolute paths. A `~` path
/// with `$HOME` unset fails closed; absolute paths pass through unchanged.
pub(crate) fn expand_home_path(raw: &str) -> Result<String, String> {
    expand_home_path_with(
        raw,
        std::env::var_os("HOME")
            .as_deref()
            .map(std::path::Path::new),
    )
}

/// [`expand_home_path`] with the home directory injected, so tests are
/// hermetic across platforms (Windows CI has no `$HOME`).
pub(crate) fn expand_home_path_with(
    raw: &str,
    home: Option<&std::path::Path>,
) -> Result<String, String> {
    if raw != "~" && !raw.starts_with("~/") {
        return Ok(raw.to_string());
    }
    let home =
        home.ok_or_else(|| format!("bitty: '{raw}' is '~'-anchored but $HOME is not set"))?;
    let joined = if raw == "~" {
        home.to_path_buf()
    } else {
        home.join(&raw[2..])
    };
    Ok(joined.to_string_lossy().into_owned())
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
     --   Shift+Alt+PageUp/PageDown      jump to previous / next shell prompt (OSC 133)\n\
     --   Alt+o                          select last command output\n\
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
     \x20\x20-- Defaults 6/6/1/6/6; safe mode forces 0/0/1/0/0.\n\
     \x20\x20-- decoration = { gaps_in = 6, gaps_out = 6, border = 1, radius = 6, content_inset = 6 },\n\
     \x20\x20-- Focused/idle outline colors (CTX-0340): '#RRGGBB' or\n\
     \x20\x20-- '#RRGGBBAA'. border_color sets both states; the focused/idle\n\
     \x20\x20-- keys override it per state. Defaults #33CCFF / #595959AA.\n\
     \x20\x20-- decoration = { border_color_focused = \"#33CCFF\", border_color_idle = \"#595959AA\" },\n\
     \x20\x20-- Focused/idle outline width in logical px (0..=16). The base\n\
     \x20\x20-- inherits decoration.border; focused/idle override it. A\n\
     \x20\x20-- focused width >= idle + 1 supplies a non-color focus cue.\n\
     \x20\x20-- decoration = { border_width = 2, border_width_focused = 3, border_width_idle = 1 },\n\
     \x20\x20-- Per-View appearance overrides (RFC-0001/OQ-041). Selector is\n\
     \x20\x20-- \"*\", a content type (empty/terminal/rich/browser), \"ws:1..16\",\n\
     \x20\x20-- or \"view:<ViewId>\"; tiers apply * < content < ws < view per\n\
     \x20\x20-- field. Accepted fields: border_color[_focused|_idle],\n\
     \x20\x20-- border_width[_focused|_idle], background_image, background_fit.\n\
     \x20\x20-- views = { [\"*\"] = { border_color_focused = \"#33CCFF\" }, [\"ws:2\"] = { border_width_idle = 1 } },\n\
      \x20\x20-- Overlay scrollback scrollbar (auto by default: transparent at rest,\n\
      \x20\x20-- revealed on mouse proximity/hover). Uncomment to pin it visible,\n\
      \x20\x20-- or set mode = \"hidden\" to disable: scrollbar = { mode = \"always\", width = 8 },\n\
     \x20\x20-- Hover activation (off by default: click-to-focus preserved).\n\
     \x20\x20-- Uncomment to opt in; the optional delay in milliseconds makes\n\
     \x20\x20-- the pointer dwell in a pane before focus moves (0 = immediate,\n\
     \x20\x20-- max 2000). Alt+drag moves a floating pane where the layout\n\
     \x20\x20-- model permits.\n\
     \x20\x20-- mouse = { focus_follows_mouse = true, focus_follows_mouse_delay_ms = 0 },\n\
     \x20\x20-- Workspace switcher bar (on by default: lists workspaces,\n\
     \x20\x20-- click/keyboard switching). Uncomment to opt out:\n\
     \x20\x20-- workspace = { show_bar = false },\n\
     \x20\x20-- The bar reserves its own row (bottom by default) so it never\n\
     \x20\x20-- covers terminal content. Uncomment to move it to the top:\n\
     \x20\x20-- workspace = { bar = { edge = \"top\" } },\n\
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

/// CTX-0979: Core draws no workspace display. `workspace.show_bar` and
/// `workspace.bar.edge` stay accepted in `bitty-config` for the bar plugin
/// to read; Core no longer maps them onto a runtime band.
/// Runs `bitty config <verb>`; returns the process exit code.
///
/// - `path`: print resolved path (0) or fail closed (2) when no root exists.
/// - `check`: load + validate via the startup path, print per-key sources
///   (0); invalid files reuse the startup error verbatim (2).
/// - `edit`: mkdir parents + starter-when-missing, open `$VISUAL`/`$EDITOR`
///   (0 on editor success; 1 on spawn failure; editor non-zero propagates).
pub(crate) fn run_config_subcommand(cmd: ConfigCommand, args: &Args) -> i32 {
    if cmd == ConfigCommand::Profile {
        return crate::profile::run_profile_subcommand(args);
    }
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
                        "window.background_image",
                        e.window
                            .background_image
                            .as_deref()
                            .map_or(String::from("(unset)"), |v| format!("\"{v}\"")),
                        &src("window.background_image")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "window.background_fit",
                        e.window
                            .background_fit
                            .map(|f| f.as_str().to_string())
                            .unwrap_or_else(|| String::from("(default fill)")),
                        &src("window.background_fit")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "window.background_opacity",
                        e.window
                            .background_opacity
                            .map_or(String::from("(default 1.0)"), |v| format!("{v}")),
                        &src("window.background_opacity")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "window.background_position",
                        e.window
                            .background_position
                            .map(|p| p.as_str().to_string())
                            .unwrap_or_else(|| String::from("(default center)")),
                        &src("window.background_position")
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
                        "terminal.cursor_style",
                        e.terminal.cursor_style.as_str().to_string(),
                        &src("terminal.cursor_style")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.bell",
                        e.terminal.bell.as_str().to_string(),
                        &src("terminal.bell")
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
                        "history.capture.transcript",
                        format!("{}", e.history.capture.transcript),
                        &src("history.capture.transcript")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "history.capture.commands",
                        format!("{}", e.history.capture.commands),
                        &src("history.capture.commands")
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
                println!(
                    "{}",
                    check_row(
                        "layout.resize_step",
                        // `{}` (shortest round-trip Display), not `{:.3}`:
                        // the check output must report the effective value
                        // (e.g. 0.0104), not a rounded one (CodeRabbit PR #1708).
                        format!("{}", e.layout.resize_step),
                        &src("layout.resize_step")
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
                // CTX-0951: dual themes show the startup-active half
                // (dark-first while the OS query degrades to Unknown).
                let theme = e.effective_theme_for(startup_prefer_light());
                let outline = e.decoration.resolve_outline(&theme);
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
                if let Some(warning) = e.decoration.idle_contrast_warning(&theme) {
                    println!(
                        "{}",
                        check_row("decoration.border_color_idle.advisory", warning, "advisory")
                    );
                }
                // CTX-1418 (#2370): report the built-in preset readability
                // policy. The summary keeps one stable row; violations and
                // documented exemptions are printed individually so a
                // regression names the exact role and measurement. The same
                // rows back the `bitty-config` catalog test, so `config check`
                // and CI can never disagree about what "readable" means.
                let contrast = bitty_config::theme::contrast_checks(&theme);
                let ratio_of = |role: bitty_config::theme::ContrastRole| {
                    contrast
                        .iter()
                        .find(|row| row.role == role)
                        .map_or(0.0, |row| row.ratio)
                };
                let violations: Vec<_> = contrast.iter().filter(|row| !row.passed).collect();
                println!(
                    "{}",
                    check_row(
                        "theme.contrast",
                        format!(
                            "fg {:.2}:1, min ANSI {:.2}:1, cursor {:.2}:1, selection {:.2}:1/{:.2}:1, ring {:.2}:1",
                            ratio_of(bitty_config::theme::ContrastRole::Foreground),
                            bitty_config::theme::min_ansi_text_contrast(&theme),
                            ratio_of(bitty_config::theme::ContrastRole::Cursor),
                            ratio_of(bitty_config::theme::ContrastRole::SelectionText),
                            ratio_of(bitty_config::theme::ContrastRole::SelectionSurface),
                            ratio_of(bitty_config::theme::ContrastRole::HelpRing),
                        ),
                        "policy",
                    )
                );
                for row in contrast.iter().filter(|row| row.exemption.is_some()) {
                    println!(
                        "{}",
                        check_row(
                            &format!("theme.contrast.exempt.{}", row.role.label()),
                            format!(
                                "{:.2}:1 (floor {:.1}:1) - {}",
                                row.ratio,
                                row.floor,
                                row.exemption.unwrap_or_default()
                            ),
                            "documented",
                        )
                    );
                }
                for row in &violations {
                    println!(
                        "{}",
                        check_row(
                            &format!("theme.contrast.violation.{}", row.role.label()),
                            format!("{:.2}:1 (floor {:.1}:1)", row.ratio, row.floor),
                            "policy",
                        )
                    );
                }
                let hue_violations = bitty_config::theme::hue_violations(&theme);
                for row in &hue_violations {
                    println!(
                        "{}",
                        check_row(
                            &format!("theme.hue.violation.ansi{}-ansi{}", row.first, row.second),
                            format!(
                                "dE {:.1} (floor {:.1}), hue {:.0}deg (floor {:.0}deg)",
                                row.delta_e,
                                bitty_config::theme::MIN_ANSI_FAMILY_DELTA_E,
                                row.hue_delta,
                                bitty_config::theme::MIN_ANSI_FAMILY_HUE_DEGREES,
                            ),
                            "policy",
                        )
                    );
                }
                println!(
                    "{}",
                    check_row(
                        "theme.contrast.result",
                        if violations.is_empty() && hue_violations.is_empty() {
                            String::from("pass")
                        } else {
                            format!(
                                "{} contrast violation(s), {} hue collision(s)",
                                violations.len(),
                                hue_violations.len()
                            )
                        },
                        "policy",
                    )
                );
                // CTX-0343 (RFC-0001/OQ-041): report the merged per-View
                // override set, one row per selector-qualified field, with
                // per-field source attribution. The set is presentation-only
                // and live-reconciled; per-View AC-1/AC-2 runs at first match.
                for entry in &e.views {
                    for (leaf, value) in [
                        (
                            "border_color",
                            entry
                                .overrides
                                .border_color
                                .map(|c| c.to_hex())
                                .unwrap_or_else(|| String::from("(unset)")),
                        ),
                        (
                            "border_color_focused",
                            entry
                                .overrides
                                .border_color_focused
                                .map(|c| c.to_hex())
                                .unwrap_or_else(|| String::from("(unset)")),
                        ),
                        (
                            "border_color_idle",
                            entry
                                .overrides
                                .border_color_idle
                                .map(|c| c.to_hex())
                                .unwrap_or_else(|| String::from("(unset)")),
                        ),
                        (
                            "border_width",
                            entry
                                .overrides
                                .border_width
                                .map_or_else(|| String::from("(unset)"), |v| v.to_string()),
                        ),
                        (
                            "border_width_focused",
                            entry
                                .overrides
                                .border_width_focused
                                .map_or_else(|| String::from("(unset)"), |v| v.to_string()),
                        ),
                        (
                            "border_width_idle",
                            entry
                                .overrides
                                .border_width_idle
                                .map_or_else(|| String::from("(unset)"), |v| v.to_string()),
                        ),
                        (
                            "background_image",
                            entry
                                .overrides
                                .background_image
                                .clone()
                                .unwrap_or_else(|| String::from("(unset)")),
                        ),
                        (
                            "background_fit",
                            entry
                                .overrides
                                .background_fit
                                .map(|f| f.as_str().to_string())
                                .unwrap_or_else(|| String::from("(unset)")),
                        ),
                    ] {
                        let field = entry.field_path(leaf);
                        println!("{}", check_row(&field, value, &src(&field)));
                    }
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
                        "appearance.animations.duration_ms.move",
                        e.animations.duration_ms.r#move.to_string(),
                    ),
                    (
                        "appearance.animations.duration_ms.resize",
                        e.animations.duration_ms.resize.to_string(),
                    ),
                    (
                        "appearance.animations.duration_ms.drag",
                        e.animations.duration_ms.drag.to_string(),
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
                    (
                        "appearance.animations.easing.move",
                        e.animations.easing.r#move.as_str().to_string(),
                    ),
                    (
                        "appearance.animations.easing.resize",
                        e.animations.easing.resize.as_str().to_string(),
                    ),
                    (
                        "appearance.animations.easing.drag",
                        e.animations.easing.drag.as_str().to_string(),
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
                        "close_confirm",
                        format!("\"{}\"", e.close_confirm.as_str()),
                        &src("close_confirm")
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
                // CTX-0347 (RFC-0001/OQ-042): the background-image pipeline is
                // a validation step, not a paint step. `config check` runs the
                // exact startup gate — approved-root policy, canonicalize,
                // regular file, BG-1..BG-3, format/animation sniff, decode,
                // BG-4/BG-5 admission — so a missing file, an unapproved
                // root, an animated container, or an over-limit image fails
                // here with a source-attributed key before anything paints.
                // CTX-0951: same startup-appearance rule as a fresh launch.
                match runtime_config_from_effective_for(e, startup_prefer_light()) {
                    Ok(cfg) => {
                        if let Err(err) = bitty_runtime::validate_background_images(&cfg) {
                            eprintln!("bitty config check: {err}");
                            return 2;
                        }
                    }
                    Err(err) => {
                        eprintln!("{err}");
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
        // Handled before the positional rejection above; kept exhaustive.
        ConfigCommand::Profile => crate::profile::run_profile_subcommand(args),
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
/// Cell geometry applies the configured breathing room scaled with point size
/// (`font.line_height`/`font.letter_spacing` over the legacy `8x16` base via
/// [`bitty_config::types::FontConfig::default_effective_cell`], `10x22` at
/// 12pt, linearly scaled at other sizes so the cell tracks the harfrust
/// shaped advance — issue #1814);
/// grid/queue geometry stays at compiled defaults; font family/size, scroll
/// speed, selection auto-copy, panel gaps, and hover-focus come from the
/// file/CLI/default chain (already validated by `bitty-config`, so
/// construction is expected to succeed — failures stay fail-closed).
/// Engaged defense-in-depth clamps are collected as warnings (CTX-0482);
/// semantic bounds (scrollback) stay fail-closed.
///
/// CTX-0951: `prefer_light` selects the active half of a dual
/// `appearance.theme` (`true` = light OS appearance, `false` = dark or
/// unknown, the dark-first default). Production passes
/// [`startup_prefer_light`]; the dark-first wrapper below keeps unit tests
/// deterministic while the OS query degrades to `Unknown`.
pub(crate) fn runtime_config_from_effective_for_with_warnings(
    effective: &bitty_config::EffectiveConfig,
    prefer_light: bool,
) -> Result<(bitty_runtime::RuntimeConfig, Vec<String>), String> {
    let mut warnings: Vec<String> = Vec::new();
    let defaults = bitty_runtime::RuntimeConfig::default();
    let (cell_width, cell_height) = effective.font.default_effective_cell();
    // CTX-0177: `bitty-config` validates `0..=MAX_LAYOUT_GAP_CELLS` (u32) and
    // `bitty-runtime` mirrors the bound in u16; the clamp below is
    // defense-in-depth so a future bound drift can never wrap the cast.
    let gaps_in = clamp_in_depth(
        effective.layout.gaps_in,
        u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS),
        "layout.gaps_in",
        &mut warnings,
    ) as u16;
    let gaps_out = clamp_in_depth(
        effective.layout.gaps_out,
        u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS),
        "layout.gaps_out",
        &mut warnings,
    ) as u16;
    // CTX-0223: `window.padding` flows file -> effective -> runtime the same
    // way (validated `0..=64` by `bitty-config`; clamped here so a future
    // bound drift can never wrap the cast).
    let window_padding = clamp_in_depth(
        effective.window.padding,
        bitty_runtime::config::MAX_WINDOW_PADDING,
        "window.padding",
        &mut warnings,
    );
    // CTX-0241 S0: `window.radius_px` flows the same way (validated
    // `0..=24` by `bitty-config`; clamped here so a future bound drift can
    // never wrap the cast). S0 is a parsed no-op: stored on the runtime
    // config with zero render effect (default 0 = zero-cost everywhere).
    // CTX-0181: `scrollbar` flows the same way. The mode enum is paired by
    // value (`bitty-runtime` owns no `bitty-config` dependency); the match
    // is total with a fail-closed `hidden` fallback so a future variant
    // drift can never misroute chrome into visibility (upstream
    // `bitty-config` only ever yields `hidden`/`always`/`auto`).
    let scrollbar_mode = match effective.scrollbar.mode.as_str() {
        "always" => bitty_runtime::ScrollbarMode::Always,
        "auto" => bitty_runtime::ScrollbarMode::Auto,
        _ => bitty_runtime::ScrollbarMode::Hidden,
    };
    let scrollbar_width = clamp_in_depth(
        effective.scrollbar.width,
        bitty_runtime::config::MAX_SCROLLBAR_WIDTH_PX,
        "scrollbar.width",
        &mut warnings,
    );
    // CTX-0370: `close_confirm` flows file -> effective -> runtime by value
    // (`bitty-runtime` owns no `bitty-config` dependency); the match is
    // total with a fail-closed `when_busy` fallback so a future variant
    // drift can never disable the gate (upstream only ever yields
    // `always`/`when_busy`/`never`).
    let close_confirm = match effective.close_confirm {
        bitty_config::CloseConfirm::Always => bitty_runtime::CloseConfirmMode::Always,
        bitty_config::CloseConfirm::Never => bitty_runtime::CloseConfirmMode::Never,
        bitty_config::CloseConfirm::WhenBusy => bitty_runtime::CloseConfirmMode::WhenBusy,
    };
    let window_radius_px = clamp_in_depth(
        effective.window.radius_px,
        bitty_runtime::config::MAX_WINDOW_RADIUS_PX,
        "window.radius_px",
        &mut warnings,
    );
    // CTX-0292: Core-owned workspace decoration flows file -> effective ->
    // runtime (`bitty-config` validates the accepted CTX-0118 ranges; the
    // clamps below are defense-in-depth so a future bound drift can never
    // wrap the u16 cast).
    let decoration = bitty_runtime::Decoration::new(
        clamp_in_depth(
            effective.decoration.gaps_in,
            u32::from(bitty_runtime::config::MAX_DECORATION_GAP_PX),
            "decoration.gaps_in",
            &mut warnings,
        ) as u16,
        clamp_in_depth(
            effective.decoration.gaps_out,
            u32::from(bitty_runtime::config::MAX_DECORATION_GAP_PX),
            "decoration.gaps_out",
            &mut warnings,
        ) as u16,
        clamp_in_depth(
            effective.decoration.border,
            u32::from(bitty_runtime::config::MAX_DECORATION_BORDER_PX),
            "decoration.border",
            &mut warnings,
        ) as u16,
        clamp_in_depth(
            effective.decoration.radius,
            u32::from(bitty_runtime::config::MAX_DECORATION_RADIUS_PX),
            "decoration.radius",
            &mut warnings,
        ) as u16,
        clamp_in_depth(
            effective.decoration.content_inset,
            u32::from(bitty_runtime::config::MAX_DECORATION_CONTENT_INSET_PX),
            "decoration.content_inset",
            &mut warnings,
        ) as u16,
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
    // CTX-0347 (RFC-0001/OQ-042): `~`-anchored background paths are expanded
    // here, at the environment boundary, so `bitty-runtime` stays free of
    // environment reads and the loader only ever sees absolute paths. A
    // `~` path with `$HOME` unset fails closed. Per-`View` rules expand the
    // same way below.
    let background_image = match effective.decoration.background_image.as_deref() {
        Some(path) => Some(expand_home_path(path)?),
        None => None,
    };
    let background_fit = effective.decoration.resolve_background_fit();
    let background_image_roots = effective
        .decoration
        .background_image_roots
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|root| expand_home_path(root))
        .collect::<Result<Vec<_>, String>>()?;
    // CTX-1076 (issue #1815): `~`-anchored window background paths expand at
    // the same environment boundary; trust reuses the decoration roots.
    let window_background_image = match effective.window.background_image.as_deref() {
        Some(path) => Some(expand_home_path(path)?),
        None => None,
    };
    let window_background_fit = effective.window.resolve_background_fit();
    let window_background_opacity = effective.window.resolve_background_opacity();
    let window_background_position = effective.window.resolve_background_position();
    let view_appearance = effective
        .views
        .iter()
        .map(|entry| {
            let background_image = match entry.overrides.background_image.as_deref() {
                Some(path) => Some(expand_home_path(path)?),
                None => None,
            };
            Ok(bitty_runtime::ViewAppearanceRule {
                selector: entry.selector.canonical(),
                border_color: entry.overrides.border_color.map(|c| c.0),
                border_color_focused: entry.overrides.border_color_focused.map(|c| c.0),
                border_color_idle: entry.overrides.border_color_idle.map(|c| c.0),
                border_width: entry.overrides.border_width,
                border_width_focused: entry.overrides.border_width_focused,
                border_width_idle: entry.overrides.border_width_idle,
                background_image,
                background_fit: entry
                    .overrides
                    .background_fit
                    .map(|f| f.as_str().to_string()),
                // CTX-1161 S2: the per-`View` style quartet rides the same
                // rule in canonical form. Colors are already `Rgba8` bytes;
                // bounds were enforced fail-closed in `bitty-config` and are
                // mirrored by `RuntimeConfig::validate`.
                glow_color: entry.overrides.glow_color.map(|c| c.0),
                glow_width: entry.overrides.glow_width,
                background_tint: entry.overrides.background_tint.map(|c| c.0),
                background_tint_strength: entry.overrides.background_tint_strength,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
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
        cfg.focus_follows_mouse_delay =
            std::time::Duration::from_millis(u64::from(clamp_in_depth(
                effective.mouse.focus_follows_mouse_delay_ms,
                bitty_runtime::config::MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
                "mouse.focus_follows_mouse_delay_ms",
                &mut warnings,
            )));
        // CTX-0370: the close-confirmation mode rides the validated runtime
        // config (same post-construction pattern as hover-focus).
        cfg.close_confirm = close_confirm;
        // CTX-0292: Core-owned workspace decoration is carried onto the
        // validated runtime config (same post-construction pattern as
        // `focus_follows_mouse`).
        cfg.decoration = decoration;
        // CTX-0979: Core draws no workspace display. `workspace.show_bar`
        // and `workspace.bar.edge` stay accepted in `bitty-config` for the
        // bar plugin; Core carries no visibility or edge state.
        // CTX-0340: resolve the focused/idle outline pair from the theme
        // token / `decoration.border_color` / explicit pair and carry it onto
        // the runtime config. The `bitty-config` validation already enforced
        // AC-1/AC-2 fail-closed against the same theme background.
        // CTX-0392: the effective theme folds in `appearance.colors` when
        // present (preset tokens, custom background/ANSI), so outlines and
        // the runtime palette stay on the same ground.
        // CTX-0951: dual themes resolve to the half matching `prefer_light`
        // (production passes the startup OS query; dark-first while it
        // degrades to Unknown).
        let theme = effective.effective_theme_for(prefer_light);
        let outline = effective.decoration.resolve_outline(&theme);
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
        // CTX-0343 (RFC-0001/OQ-041): carry the per-`View` override rules
        // onto the runtime config in canonical selector form. `bitty-config`
        // enforces the grammar, field bounds, and the per-`View` AC-1/AC-2
        // contract for every resolvable target during merge/reconcile; the
        // runtime mirrors the selector/width bounds and repeats the AC-1/AC-2
        // check on first match (View creation/bind/move) so a previously
        // inert `ws:`/`view:` entry also fails closed. `bitty-config` cannot
        // be named here, so the rule is rebuilt from the public `ViewOverride`
        // accessors by value. CTX-0347: the same map carries the expanded
        // background-image leaf and the accepted fit spelling.
        cfg.view_appearance = view_appearance;
        // CTX-0347 (RFC-0001/OQ-042): carry the global background image/fit
        // and the deny-by-default approved-root list. `Runtime::new` loads
        // every configured image fail-closed (and `bitty config check` runs
        // the same pipeline via `validate_background_images`), so a bad
        // path/format/bound never reaches the present path.
        cfg.background_image = background_image;
        cfg.background_fit = background_fit.as_str().to_string();
        cfg.background_image_roots = background_image_roots;
        // CTX-1161 S2: carry the merged global style surface
        // (`appearance.style` glow/shadow/fade plus the `decoration` tint
        // pair) onto the runtime config. Bounds were already enforced
        // fail-closed in `bitty-config`; `RuntimeConfig::validate` mirrors
        // them so a direct construction cannot arm an unbounded value.
        // Consumed by the S3 present path (CTX-1162).
        let empty_style = bitty_config::StyleConfig::default();
        let global_style = effective.appearance.style.as_ref().unwrap_or(&empty_style);
        cfg.style_glow_color = global_style.glow_color.map(|c| c.0);
        cfg.style_glow_width = global_style
            .glow_width
            .unwrap_or(bitty_runtime::config::DEFAULT_STYLE_GLOW_WIDTH);
        cfg.style_shadow_color = global_style.shadow_color.map(|c| c.0);
        cfg.style_shadow_offset_x = global_style
            .shadow_offset_x
            .unwrap_or(bitty_runtime::config::DEFAULT_STYLE_SHADOW_OFFSET);
        cfg.style_shadow_offset_y = global_style
            .shadow_offset_y
            .unwrap_or(bitty_runtime::config::DEFAULT_STYLE_SHADOW_OFFSET);
        cfg.style_fade_unfocused = global_style
            .fade_unfocused
            .unwrap_or(bitty_runtime::config::DEFAULT_STYLE_FADE_UNFOCUSED);
        cfg.background_tint = effective.decoration.background_tint.map(|c| c.0);
        cfg.background_tint_strength = effective
            .decoration
            .background_tint_strength
            .unwrap_or(bitty_runtime::config::DEFAULT_STYLE_TINT_STRENGTH);
        // CTX-1076 (issue #1815): carry the window background image and its
        // placement (`background_fit` cover/contain/tile vocabulary,
        // `background_opacity` dim, `background_position`). `Runtime::new`
        // loads it fail-closed through the same BG-1..BG-5 pipeline.
        cfg.window_background_image = window_background_image;
        cfg.window_background_fit = window_background_fit.as_str().to_string();
        cfg.window_background_opacity = window_background_opacity;
        cfg.window_background_position = window_background_position.as_str().to_string();
        // CTX-0355: carry the same resolved preset's terminal palette
        // (background/foreground/cursor/selection + 16 ANSI) onto the runtime
        // config so the default-path renderer and clear color follow
        // `appearance.theme` instead of the hardcoded Bitty Dark fallback.
        // CTX-0392: when `appearance.colors` is present the custom palette
        // replaces the preset's chrome/ANSI (same fixed shape, validated
        // hex, 16 entries); outline tokens stay preset-owned.
        // CTX-0951: dual themes resolve to the half matching `prefer_light`
        // (previously `resolve_theme` on the raw dual string fell back to
        // Bitty Dark instead of the dark half).
        // CTX-1395 (issue #2333): both paths resolve through the same merged
        // `Theme`, so a custom palette and a preset feed `ThemePalette` from
        // one mapping. A future theme-owned role cannot be silently dropped
        // on the custom path, and "preset is shorthand over the same roles"
        // stays true by construction.
        cfg.theme = bitty_runtime::ThemePalette::from_theme(&theme);
        // CTX-0381/CTX-0392: mark the palette as user-resolved so OSC 10/11
        // and OSC 4 queries are answered from it; headless/unit configs keep
        // the false default and stay silent (no reply before theme
        // resolution).
        cfg.theme_resolved = true;
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
        // CTX-0953 (PX-4571): effective `font.fallback` rides the validated
        // runtime config the same way (already bounded fail-closed by
        // `bitty-config`; `RuntimeConfig::validate` mirrors the bounds).
        // Without this the production rasterizer chain always saw the
        // default empty tier.
        cfg.font_fallback = effective.font.fallback.clone();
        // CTX-0756 (issue #1359): `terminal.cursor_style` flows file ->
        // effective -> runtime by value (`bitty-runtime` owns no
        // `bitty-config` dependency); the match is total with a fail-closed
        // `Default` fallback so a future variant drift can never misroute
        // the pane seed into a shape the renderer does not know (upstream
        // `bitty-config` only ever yields the seven `DECSCUSR` spellings).
        cfg.cursor_style = match effective.terminal.cursor_style {
            bitty_config::CursorStyle::BlinkingBlock => bitty_vt::CursorStyle::BlinkingBlock,
            bitty_config::CursorStyle::SteadyBlock => bitty_vt::CursorStyle::SteadyBlock,
            bitty_config::CursorStyle::BlinkingUnderline => {
                bitty_vt::CursorStyle::BlinkingUnderline
            }
            bitty_config::CursorStyle::SteadyUnderline => bitty_vt::CursorStyle::SteadyUnderline,
            bitty_config::CursorStyle::BlinkingBar => bitty_vt::CursorStyle::BlinkingBar,
            bitty_config::CursorStyle::SteadyBar => bitty_vt::CursorStyle::SteadyBar,
            bitty_config::CursorStyle::Default => bitty_vt::CursorStyle::Default,
        };
        // CTX-0756 (issue #1359): `terminal.bell` flows the same way; the
        // match is total with a fail-closed `Visual` fallback so a future
        // variant drift can never silence the bell into `Off` (upstream
        // only ever yields `off`/`visual`/`audible`/`both`).
        cfg.bell_mode = match effective.terminal.bell {
            bitty_config::BellMode::Off => bitty_runtime::BellMode::Off,
            bitty_config::BellMode::Audible => bitty_runtime::BellMode::Audible,
            bitty_config::BellMode::Both => bitty_runtime::BellMode::Both,
            bitty_config::BellMode::Visual => bitty_runtime::BellMode::Visual,
        };
        (cfg, warnings)
    })
    .map_err(|err| format!("bitty: invalid effective config for runtime: {err}"))
}

/// Dark-first [`runtime_config_from_effective_for_with_warnings`] wrapper.
///
/// Test-only determinism shim (while the OS query degrades to `Unknown`):
/// production calls the `_for` form with [`startup_prefer_light`].
#[cfg(test)]
pub(crate) fn runtime_config_from_effective_with_warnings(
    effective: &bitty_config::EffectiveConfig,
) -> Result<(bitty_runtime::RuntimeConfig, Vec<String>), String> {
    runtime_config_from_effective_for_with_warnings(effective, false)
}

/// [`runtime_config_from_effective_with_warnings`] with engaged
/// defense-in-depth clamps reported on stderr through the log gate
/// (CTX-0482, issue #763). Values keep their clamp semantics, so an
/// engaged clamp is a visible warning naming the key, never a silent
/// normalization.
///
/// Test-only dark-first wrapper; production calls the `_for` form with
/// [`startup_prefer_light`] (CTX-0951).
#[cfg(test)]
pub(crate) fn runtime_config_from_effective(
    effective: &bitty_config::EffectiveConfig,
) -> Result<bitty_runtime::RuntimeConfig, String> {
    runtime_config_from_effective_for(effective, false)
}

/// Appearance-aware [`runtime_config_from_effective`] (CTX-0951).
///
/// Same mapping but a dual `appearance.theme` resolves to the half matching
/// `prefer_light` (`true` = light OS appearance). Production passes
/// [`startup_prefer_light`].
pub(crate) fn runtime_config_from_effective_for(
    effective: &bitty_config::EffectiveConfig,
    prefer_light: bool,
) -> Result<bitty_runtime::RuntimeConfig, String> {
    let (config, warnings) =
        runtime_config_from_effective_for_with_warnings(effective, prefer_light)?;
    for warning in warnings {
        crate::logging::warn(move || warning);
    }
    Ok(config)
}

/// Defense-in-depth clamp (CTX-0482): `bitty-config` already validated the
/// value, so a clamp that engages means upstream bound drift. The value is
/// still bounded (never a wrapped cast), but the engaged key is named in
/// `warnings` instead of changing silently.
fn clamp_in_depth<T: Ord + Copy + std::fmt::Display>(
    value: T,
    max: T,
    key: &str,
    warnings: &mut Vec<String>,
) -> T {
    if value > max {
        warnings.push(format!(
            "bitty: {key} = {value} exceeds the runtime bound {max} — clamped (upstream validation drift)"
        ));
        max
    } else {
        value
    }
}

/// Maps the resolved `bitty-config` animation contract onto the runtime
/// policy (RFC-0002, CTX-0341; CTX-0967 adds the move/resize/drag leaves).
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
            clamped(effective.animations.duration_ms.r#move),
            clamped(effective.animations.duration_ms.resize),
            clamped(effective.animations.duration_ms.drag),
        ],
        curves: [
            map_curve(effective.animations.easing.open),
            map_curve(effective.animations.easing.close),
            map_curve(effective.animations.easing.focus),
            map_curve(effective.animations.easing.workspace),
            map_curve(effective.animations.easing.r#move),
            map_curve(effective.animations.easing.resize),
            map_curve(effective.animations.easing.drag),
        ],
        reduced_motion: reduced,
        safe_mode: false,
        platform_reduced: false,
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use bitty_config::EffectiveConfig;

    /// Builds the app bundle for a valid-config control without touching the
    /// filesystem or process environment.
    fn app_config(effective: EffectiveConfig) -> AppConfig {
        let (theme, resolution) = bitty_config::theme::resolve_selection_with_status(
            effective.appearance.theme.as_deref(),
            false,
        );
        AppConfig {
            effective,
            file_path: None,
            profile_name: None,
            profile_path: None,
            theme,
            resolution,
            source: "default",
        }
    }

    /// Unique scratch directory for classifier tests (no sleeps).
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1339-unit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn first_start_broken_config_uses_builtin_defaults_and_reports_diagnostics() {
        let broken = String::from("bitty: invalid config file '/xdg/bitty/init.lua': syntax error");
        let startup = resolve_startup_config(Err(LoadFailure::broken(broken.clone())), true)
            .expect("first start must proceed with the built-in config");

        assert_eq!(
            startup.app.effective,
            EffectiveConfig::default(),
            "the fallback must be the unmodified CoreDefaults plan"
        );
        assert_eq!(startup.app.source, "default");
        assert!(startup.app.file_path.is_none());
        assert!(startup.app.profile_name.is_none());
        let diagnostics = startup
            .diagnostics
            .expect("fallback must report diagnostics");
        assert!(diagnostics.contains(&broken), "diagnostics: {diagnostics}");
        assert!(
            diagnostics.contains("first start"),
            "diagnostics: {diagnostics}"
        );
        assert!(
            diagnostics.contains("built-in defaults"),
            "diagnostics: {diagnostics}"
        );
    }

    #[test]
    fn later_failure_with_a_last_good_plan_stays_fail_closed() {
        let broken = String::from("bitty: invalid config file '/xdg/bitty/init.lua': syntax error");
        match resolve_startup_config(Err(LoadFailure::broken(broken.clone())), false) {
            Ok(_) => panic!("an existing plan must be retained, never replaced by built-ins"),
            Err(error) => assert_eq!(error, broken),
        }
    }

    #[test]
    fn first_start_fatal_load_errors_stay_fail_closed() {
        // Classification, not a fabricated LoadFailure: a CLI-layer merge
        // error stays fatal even though first start has no last-good plan.
        let err = bitty_config::ConfigError::validation("appearance.theme", "unknown theme 'x'");
        let failure = classify_merge_failure(&err, None, None, false);
        let message = failure.message.clone();
        match resolve_startup_config(Err(failure), true) {
            Ok(_) => panic!("a CLI-layer failure must never fall back to built-ins"),
            Err(error) => assert_eq!(error, message),
        }
    }

    #[test]
    fn merge_failure_attribution_never_blames_a_valid_user_file() {
        let err = bitty_config::ConfigError::validation("appearance.theme", "unknown theme 'x'");
        let file = std::path::Path::new("/xdg/bitty/init.lua");
        let profile = std::path::Path::new("/xdg/bitty/profiles/bad.lua");

        // CLI error while a valid file participates: file is not causal.
        let cli = classify_merge_failure(&err, Some(file), None, false);
        assert!(!cli.fallback_allowed, "{}", cli.message);
        assert!(
            cli.message.starts_with("bitty: invalid --theme:"),
            "{}",
            cli.message
        );

        // Profile error while a valid file participates: attributed to the
        // profile, never to the file.
        let profile_failure = classify_merge_failure(&err, Some(file), Some(profile), false);
        assert!(
            !profile_failure.fallback_allowed,
            "{}",
            profile_failure.message
        );
        assert!(
            profile_failure.message.contains("invalid profile"),
            "{}",
            profile_failure.message
        );
        assert!(
            profile_failure.message.contains("profiles/bad.lua"),
            "{}",
            profile_failure.message
        );

        // A re-merge without the user layer succeeded: the file is causal.
        let broken = classify_merge_failure(&err, Some(file), None, true);
        assert!(broken.fallback_allowed, "{}", broken.message);
        assert!(broken.message.contains("init.lua"), "{}", broken.message);
    }

    #[test]
    fn classifier_marks_a_present_broken_file_recoverable() {
        let dir = scratch_dir("broken-file");
        let path = dir.join("init.lua");
        std::fs::write(&path, "return { font = }\n").expect("write broken init.lua");
        let mut args = Args::new();
        args.config_path = Some(path.to_string_lossy().into_owned());

        let failure = resolve_app_config(&args)
            .err()
            .expect("a present-but-broken file must fail to load");
        assert!(failure.fallback_allowed, "{}", failure.message);
        assert!(
            failure.message.contains("invalid config file"),
            "{}",
            failure.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifier_keeps_a_missing_explicit_path_fatal() {
        let dir = scratch_dir("missing-path");
        let mut args = Args::new();
        args.config_path = Some(dir.join("missing.lua").to_string_lossy().into_owned());

        let failure = resolve_app_config(&args)
            .err()
            .expect("an explicitly missing path must fail");
        assert!(!failure.fallback_allowed, "{}", failure.message);
        assert!(failure.message.contains("not found"), "{}", failure.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifier_keeps_invalid_cli_theme_fatal_with_a_valid_file() {
        let dir = scratch_dir("bad-theme");
        let path = dir.join("init.lua");
        std::fs::write(&path, "return { appearance = { theme = \"dracula\" } }\n")
            .expect("write valid init.lua");
        let mut args = Args::new();
        args.config_path = Some(path.to_string_lossy().into_owned());
        args.theme = Some(String::from("not-a-theme"));

        let failure = resolve_app_config(&args)
            .err()
            .expect("an unknown --theme must fail closed");
        assert!(!failure.fallback_allowed, "{}", failure.message);
        assert!(
            failure.message.contains("invalid --theme"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("appearance.theme"),
            "{}",
            failure.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_config_is_returned_unchanged() {
        let mut effective = EffectiveConfig::default();
        effective.font.size = 15.0;
        let startup = resolve_startup_config(Ok(app_config(effective.clone())), true)
            .expect("valid config must load");
        assert_eq!(startup.app.effective, effective);
        assert!(startup.diagnostics.is_none());
    }

    #[test]
    fn first_start_predicate_tracks_the_installed_last_good_plan() {
        crate::config_reload::clear();
        assert!(is_first_start(), "startup has no plan to retain");

        let plan = EffectiveConfig::default();
        crate::config_reload::install_with(plan, Box::new(|| Err(String::from("unused"))), None);
        assert!(
            !is_first_start(),
            "an installed last-good plan means later failures retain it"
        );
        crate::config_reload::clear();
    }
}
