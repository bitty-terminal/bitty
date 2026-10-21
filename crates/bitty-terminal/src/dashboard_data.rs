//! App-side snapshot builder for the configuration dashboard
//! (issue #2380, CTX-1429).
//!
//! The runtime panel ([`bitty_runtime::dashboard`]) renders plain data and
//! never performs I/O. This module owns the cold-path reads — managed plugin
//! manifest, plugin store index, per-plugin settings files, the profiles
//! directory, and the extension inventory — and turns every failure into an
//! `unavailable` section instead of an empty one.
//!
//! # Authority
//!
//! Every read here is the same data `bitty config check`, `bitty plugin
//! list`, and `bitty extension list` already print; nothing is widened. No
//! environment dump, credential, token, or process command line is read, so
//! the panel cannot leak a secret by construction. Writes are not performed
//! here: the app applies the typed intents the panel returns.
//!
//! # Freshness
//!
//! The snapshot is rebuilt when the panel opens and on `r`. Live values
//! (theme, font, decoration) come from the runtime at build time; the
//! profile name and paths come from startup ([`DashboardSources`]), matching
//! the running process rather than a later `init.lua` edit.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use bitty_plugin_host::manifest::SettingDefault;
use bitty_runtime::component::{
    ActivationState, ComponentSummary, component_search_roots_for, component_state_root_for,
    data_home_for, discover_components, read_component_activation_with_state,
};
use bitty_runtime::plugin_runtime::LuaValue;
use bitty_runtime::plugin_runtime::SettingsSource;
use bitty_runtime::plugin_runtime::plugin_settings::{
    SettingLayer, load_plugin_settings_with_plugins,
};
use bitty_runtime::runtime::dashboard::{
    DASHBOARD_MAX_NOTE_BYTES, DashboardAction, DashboardRow, DashboardSectionData,
    DashboardSnapshot, bounded_text,
};

/// Startup-resolved paths the dashboard reads on demand.
///
/// Built once by `main` from the already-resolved startup values and never
/// refreshed from the environment, so a later `XDG_*` mutation cannot make
/// the panel read a different tree than the process actually uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DashboardSources {
    /// Resolved user config file, when one was found.
    pub(crate) config_path: Option<PathBuf>,
    /// Config root (`<config>/bitty`), when resolvable.
    pub(crate) config_dir: Option<PathBuf>,
    /// Requested profile name (`--profile` over `BITTY_PROFILE`).
    pub(crate) profile_name: Option<String>,
    /// Preset resolved at startup; a live difference means an unsaved
    /// dashboard preview, reported as such instead of the startup layer.
    pub(crate) startup_theme_name: Option<String>,
    /// Managed plugin manifest beside `init.lua`.
    pub(crate) plugin_state_path: Option<PathBuf>,
    /// Plugin package store root (`.../bitty/plugins`).
    pub(crate) store_root: Option<PathBuf>,
    /// User-tier extension root.
    pub(crate) extension_user_root: Option<PathBuf>,
    /// System-tier extension root.
    pub(crate) extension_system_root: Option<PathBuf>,
    /// Last-recorded activation state root (`<state-home>/bitty/extensions`).
    pub(crate) component_state_root: Option<PathBuf>,
    /// How the profile was selected (`cli`, `env`, or `none`).
    pub(crate) profile_source: String,
}

/// Application version shown in Diagnostics (crate version at build time).
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment values the startup source resolver reads.
///
/// Injected so path resolution is testable without mutating process
/// environment (`std::env::set_var` is unsafe and forbidden here).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DashboardEnv {
    /// `$XDG_CONFIG_HOME`.
    pub(crate) xdg_config_home: Option<String>,
    /// `$HOME`.
    pub(crate) home: Option<String>,
    /// `$XDG_DATA_HOME`.
    pub(crate) xdg_data_home: Option<String>,
    /// `$BITTY_COMPONENTS_DIR` (developer override).
    pub(crate) components_dir: Option<String>,
    /// `$BITTY_SYSTEM_COMPONENTS_DIR` (developer override).
    pub(crate) system_components_dir: Option<String>,
    /// `$BITTY_COMPONENT_STATE_DIR` (developer override).
    pub(crate) component_state_dir: Option<String>,
    /// `$XDG_STATE_HOME`.
    pub(crate) xdg_state_home: Option<String>,
    /// `$BITTY_CONFIG` (config path override).
    pub(crate) bitty_config: Option<String>,
}

impl DashboardEnv {
    /// Read the live process environment once at startup.
    pub(crate) fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            xdg_config_home: var("XDG_CONFIG_HOME"),
            home: var("HOME"),
            xdg_data_home: var("XDG_DATA_HOME"),
            components_dir: var(bitty_runtime::component::COMPONENTS_DIR_ENV),
            system_components_dir: var(bitty_runtime::component::SYSTEM_COMPONENTS_DIR_ENV),
            component_state_dir: var(bitty_runtime::component::COMPONENT_STATE_DIR_ENV),
            xdg_state_home: var("XDG_STATE_HOME"),
            bitty_config: var("BITTY_CONFIG"),
        }
    }
}

impl DashboardSources {
    /// Resolve every source from the live environment, once at startup.
    ///
    /// `config_path` is the explicit `--config` operand (the resolved
    /// `file_path` alone is not enough: it is `None` when the user named a
    /// file that does not exist yet). `startup_theme_name` is the preset the
    /// process started with, so a later live preview is reported as unsaved.
    /// `profile_source` records which layer chose the profile (`cli`/`env`).
    pub(crate) fn discover(
        config_path: Option<&str>,
        file_path: Option<PathBuf>,
        profile_name: Option<String>,
        profile_source: &str,
        startup_theme_name: &str,
    ) -> Self {
        Self::discover_with(
            config_path,
            file_path,
            profile_name,
            profile_source,
            startup_theme_name,
            &DashboardEnv::from_process(),
        )
    }

    /// Environment-injected resolver behind [`Self::discover`].
    ///
    /// Component roots come from the same resolver the production extension
    /// host uses ([`component_host::roots_from_env`](crate::component_host)),
    /// including the `BITTY_COMPONENTS_DIR` / `BITTY_SYSTEM_COMPONENTS_DIR`
    /// overrides, so the panel can never show a different tree than the host.
    pub(crate) fn discover_with(
        config_path: Option<&str>,
        file_path: Option<PathBuf>,
        profile_name: Option<String>,
        profile_source: &str,
        startup_theme_name: &str,
        env: &DashboardEnv,
    ) -> Self {
        let data_home = data_home_for(env.xdg_data_home.as_deref(), env.home.as_deref());
        let (extension_user_root, extension_system_root) = component_search_roots_for(
            env.components_dir.as_deref(),
            data_home.as_deref(),
            env.system_components_dir.as_deref(),
        );
        Self {
            config_path: file_path,
            config_dir: bitty_config::file::config_dir_with_platform_env(
                env.xdg_config_home.as_deref(),
                env.home.as_deref(),
                None,
                None,
            ),
            profile_name,
            startup_theme_name: Some(startup_theme_name.to_string()),
            plugin_state_path: crate::plugin::live_state_path_with(
                config_path,
                env.bitty_config.as_deref(),
                env.xdg_config_home.as_deref(),
                env.home.as_deref(),
            ),
            store_root: crate::plugin_runtime::store_root_for(
                env.xdg_data_home.as_deref(),
                env.home.as_deref(),
            ),
            extension_user_root,
            extension_system_root,
            component_state_root: component_state_root_for(
                env.component_state_dir.as_deref(),
                env.xdg_state_home.as_deref(),
                env.home.as_deref(),
            ),
            profile_source: profile_source.to_string(),
        }
    }
}

/// Live values only the runtime/window know, sampled at build time.
pub(crate) struct DashboardView<'a> {
    /// Resolved theme preset name shown in the window title.
    pub(crate) theme_name: &'a str,
    /// Theme layer (`cli`, `file`, `profile`, `default`).
    pub(crate) theme_source: &'a str,
    /// Live font family.
    pub(crate) font_family: &'a str,
    /// Live font point size.
    pub(crate) font_size: f32,
    /// Live Core decoration geometry (logical pixels).
    pub(crate) decoration: DashboardDecoration,
}

/// The five decoration dimensions the Overview reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DashboardDecoration {
    /// Gap between adjacent views.
    pub(crate) gaps_in: u16,
    /// Gap between the tiling area and the window edge.
    pub(crate) gaps_out: u16,
    /// Border thickness.
    pub(crate) border: u16,
    /// Corner radius.
    pub(crate) radius: u16,
    /// Inner content padding.
    pub(crate) content_inset: u16,
}

/// Build one dashboard snapshot.
///
/// `sources` is `None` when startup could not resolve the config root (e.g.
/// `--safe` with no home): the Overview then reports exactly that instead of
/// inventing defaults.
pub(crate) fn build(
    sources: Option<&DashboardSources>,
    view: &DashboardView<'_>,
) -> DashboardSnapshot {
    let sources = sources.cloned().unwrap_or_default();
    DashboardSnapshot::new(
        overview(&sources, view),
        plugins(&sources),
        settings(&sources),
        profiles(&sources),
        extensions(&sources),
        diagnostics(&sources, view),
    )
}

// ── Overview ──────────────────────────────────────────────────────────────

fn overview(sources: &DashboardSources, view: &DashboardView<'_>) -> DashboardSectionData {
    let mut rows = vec![
        DashboardRow::info(
            "profile",
            sources
                .profile_name
                .as_deref()
                .unwrap_or("default (no profile)"),
        ),
        DashboardRow::info(
            "theme",
            format!("{} ({})", view.theme_name, view.theme_source),
        ),
        DashboardRow::info(
            "font",
            format!("{} {:.1}pt", view.font_family, view.font_size),
        ),
        DashboardRow::info(
            "decoration",
            format!(
                "gaps {}/{} border {} radius {} inset {}",
                view.decoration.gaps_in,
                view.decoration.gaps_out,
                view.decoration.border,
                view.decoration.radius,
                view.decoration.content_inset,
            ),
        ),
        DashboardRow::path(
            "config",
            sources.config_path.as_deref().map_or_else(
                || "(none found)".to_string(),
                |path| path.display().to_string(),
            ),
        ),
    ];
    rows.push(DashboardRow::info(
        "theme preview",
        "Enter applies live (not saved)",
    ));
    for theme in bitty_config::theme::ALL_PRESETS {
        if theme.name == view.theme_name {
            rows.push(DashboardRow::info(format!("  {}", theme.name), "(current)"));
            continue;
        }
        rows.push(DashboardRow::action(
            format!("  {}", theme.name),
            "",
            DashboardAction::PreviewTheme {
                name: theme.name.to_string(),
            },
        ));
    }
    DashboardSectionData::ready(rows)
}

// ── Plugins ───────────────────────────────────────────────────────────────

fn plugins(sources: &DashboardSources) -> DashboardSectionData {
    let rows = match crate::plugin::dashboard_plugins(
        sources.plugin_state_path.as_deref(),
        sources.store_root.as_deref(),
    ) {
        Ok(plugins) => plugins,
        Err(error) => return DashboardSectionData::unavailable(error),
    };
    if rows.is_empty() {
        return DashboardSectionData::ready(vec![DashboardRow::info(
            "plugins",
            "none recorded (bundled catalog empty)",
        )]);
    }
    let rows: Vec<DashboardRow> = rows
        .iter()
        .map(|plugin| {
            let value = format!("{} ({}, {})", plugin.state, plugin.source, plugin.version);
            let action = if plugin.bundled && plugin.recorded {
                DashboardAction::SetPluginEnabled {
                    id: plugin.id.clone(),
                    enabled: !plugin.enabled,
                }
            } else {
                DashboardAction::None
            };
            DashboardRow::action(plugin.id.clone(), value, action)
        })
        .collect();
    DashboardSectionData::ready(rows)
}

// ── Settings ──────────────────────────────────────────────────────────────

fn settings(sources: &DashboardSources) -> DashboardSectionData {
    let plugins = match crate::plugin::dashboard_plugins(
        sources.plugin_state_path.as_deref(),
        sources.store_root.as_deref(),
    ) {
        Ok(plugins) => plugins,
        Err(error) => return DashboardSectionData::unavailable(error),
    };
    let Some(config_dir) = sources.config_dir.as_deref() else {
        return DashboardSectionData::unavailable(
            "config root unavailable; settings files cannot be read",
        );
    };
    let discovered: Vec<String> = plugins.iter().map(|plugin| plugin.id.clone()).collect();
    let mut rows: Vec<DashboardRow> = Vec::new();
    for plugin in &plugins {
        if plugin.settings.is_empty() {
            continue;
        }
        let loaded = load_plugin_settings_with_plugins(
            Some(config_dir),
            &plugin.id,
            &plugin.settings,
            None,
            Some(&discovered),
        );
        let short = plugin
            .id
            .split_once('.')
            .map_or(plugin.id.as_str(), |(_, name)| name);
        for decl in &plugin.settings {
            let current = loaded.source.get(&decl.key);
            let current = current
                .as_ref()
                .map_or_else(|| default_label(&decl.default), value_label);
            // The settings resolver already attributes every declared key to
            // the layer that served it; the panel never re-derives it.
            let source = match loaded.layers.get(&decl.key) {
                Some(SettingLayer::UserFile) => "user",
                Some(SettingLayer::Host) => "host",
                _ => "default",
            };
            rows.push(DashboardRow::info(
                format!("{short}:{}", decl.key),
                format!(
                    "{current} (default {}, source {source})",
                    default_label(&decl.default)
                ),
            ));
        }
        if let Some(error) = &loaded.file_error {
            rows.push(DashboardRow::info(
                format!("{short}: settings file"),
                bounded_text(error, DASHBOARD_MAX_NOTE_BYTES),
            ));
        }
        for key in &loaded.dropped {
            rows.push(DashboardRow::info(
                format!("{short}:{key}"),
                "ignored (undeclared or wrong type)",
            ));
        }
    }
    if rows.is_empty() {
        rows.push(DashboardRow::info(
            "settings",
            "no installed plugin declares settings",
        ));
    }
    // Read-only in this slice: a persisted settings write needs the typed
    // `config.manage` API (#2393). Name the CLI that owns it.
    rows.push(DashboardRow::info(
        "settings are read-only here",
        "edit with bitty plugin config scaffold <id>",
    ));
    DashboardSectionData::ready(rows)
}

fn default_label(default: &SettingDefault) -> String {
    match default {
        SettingDefault::Bool(flag) => flag.to_string(),
        SettingDefault::Int(number) => number.to_string(),
        SettingDefault::String(text) => format!("\"{text}\""),
        SettingDefault::Strings(items) => format!("[{}]", items.join(",")),
    }
}

fn value_label(value: &LuaValue) -> String {
    match value {
        LuaValue::Nil => "nil".to_string(),
        LuaValue::Bool(flag) => flag.to_string(),
        LuaValue::Integer(number) => number.to_string(),
        LuaValue::Number(number) => format!("{number}"),
        LuaValue::String(text) => format!("\"{text}\""),
        LuaValue::Table(items) => {
            let rendered: Vec<String> = items
                .iter()
                .map(|(_, value)| value_label(value))
                .take(16)
                .collect();
            format!("[{}]", rendered.join(","))
        }
    }
}

// ── Profiles ──────────────────────────────────────────────────────────────

fn profiles(sources: &DashboardSources) -> DashboardSectionData {
    let Some(config_dir) = sources.config_dir.as_deref() else {
        return DashboardSectionData::unavailable("config root unavailable");
    };
    let dir = config_dir.join(bitty_config::file::PROFILES_DIR_NAME);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DashboardSectionData::ready(vec![DashboardRow::info(
                "profiles",
                "none (no profiles/ directory)",
            )]);
        }
        Err(error) => {
            return DashboardSectionData::unavailable(format!(
                "profiles directory unreadable: {error}"
            ));
        }
    };
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if let Ok(name) = bitty_config::file::validate_profile_name(stem) {
            names.insert(name);
        }
    }
    let active = sources.profile_name.as_deref();
    let mut rows = vec![
        DashboardRow::info(
            "default",
            if active.is_none() {
                "(active)"
            } else {
                "available"
            },
        ),
        DashboardRow::info("profiles are read-only here", "restart with --profile"),
    ];
    for name in names {
        rows.push(DashboardRow::info(
            name.clone(),
            if active == Some(name.as_str()) {
                "(active)"
            } else {
                "available"
            },
        ));
    }
    DashboardSectionData::ready(rows)
}

// ── Extensions ────────────────────────────────────────────────────────────

fn extensions(sources: &DashboardSources) -> DashboardSectionData {
    if sources.extension_user_root.is_none() && sources.extension_system_root.is_none() {
        return DashboardSectionData::unavailable("extension roots unavailable");
    }
    // `discover_components` is best-effort by contract: an unreadable root
    // yields no names. The panel must not turn that into "none installed",
    // so probe readability first and report the reason.
    for (label, root) in [
        ("user", sources.extension_user_root.as_deref()),
        ("system", sources.extension_system_root.as_deref()),
    ] {
        let Some(root) = root else {
            continue;
        };
        match std::fs::read_dir(root) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return DashboardSectionData::unavailable(format!(
                    "{label} extension root unreadable: {} ({error})",
                    root.display()
                ));
            }
        }
    }
    let summaries = discover_components(
        sources.extension_user_root.as_deref(),
        sources.extension_system_root.as_deref(),
    );
    if summaries.is_empty() {
        return DashboardSectionData::ready(vec![DashboardRow::info(
            "extensions",
            "none installed",
        )]);
    }
    let mut rows: Vec<DashboardRow> = Vec::new();
    for summary in &summaries {
        extensions_rows(summary, sources, &mut rows);
    }
    DashboardSectionData::ready(rows)
}

/// One component's read-only projection plus its documented CLI actions.
///
/// The action rows name the exact `bitty extension` command; the panel never
/// executes it (no process, network, or filesystem-write authority).
fn extensions_rows(
    summary: &ComponentSummary,
    sources: &DashboardSources,
    rows: &mut Vec<DashboardRow>,
) {
    let activation = read_component_activation_with_state(
        sources.extension_user_root.as_deref(),
        sources.extension_system_root.as_deref(),
        sources.component_state_root.as_deref(),
        &summary.name,
    );
    let active = summary.active_version.as_deref();
    let tier = summary
        .active_source
        .map_or("none", |source| source.label());
    rows.push(DashboardRow::info(
        summary.name.clone(),
        format!(
            "{} active ({tier}), {} version(s), ABI {}",
            active.unwrap_or("-"),
            summary.versions.len(),
            if summary.versions.iter().any(|version| version.compatible) {
                "compatible"
            } else {
                "incompatible"
            }
        ),
    ));
    let selected = active
        .and_then(|active| {
            summary
                .versions
                .iter()
                .find(|version| version.version == active)
        })
        .or_else(|| summary.versions.last());
    let digest = selected.map(|version| version.sha256.as_str());
    rows.push(DashboardRow::info(
        format!("  {}: digest", summary.name),
        digest.map_or_else(
            || "unknown (descriptor unreadable)".to_string(),
            shorten_digest,
        ),
    ));
    rows.push(DashboardRow::path(
        format!("  {}: target", summary.name),
        selected.map_or_else(
            || "no installed version".to_string(),
            |version| {
                // Lead with the executable name and tier: even a clipped row
                // keeps the identity, and the full path fits the path bound.
                let executable = version.executable_path.file_name().map_or_else(
                    || version.executable_path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                format!(
                    "{executable} ({}) at {}",
                    version.source.label(),
                    version.version_dir.display()
                )
            },
        ),
    ));
    rows.push(DashboardRow::info(
        format!("  {}: activation", summary.name),
        format!("{} (last recorded; not a live probe)", activation.label()),
    ));
    rows.push(DashboardRow::info(
        format!("  {}: liveness", summary.name),
        "not probed (no process is attached by this panel)",
    ));
    rows.push(DashboardRow::info(
        format!("  {}: trust", summary.name),
        trust_label(activation, digest.is_some()),
    ));
    rows.push(DashboardRow::info(
        format!("  {}: install/update", summary.name),
        format!("bitty extension install {}", summary.name),
    ));
    for version in &summary.versions {
        rows.push(DashboardRow::info(
            format!("  {}: remove {}", summary.name, version.version),
            format!(
                "bitty extension remove {} {}",
                summary.name, version.version
            ),
        ));
    }
}

/// First 16 hex characters of the descriptor digest, for a bounded row.
fn shorten_digest(digest: &str) -> String {
    let short: String = digest.chars().take(16).collect();
    format!("{short}\u{2026} (descriptor)")
}

/// Trust projection derived from the last recorded activation, never from a
/// fresh resolve: the panel must not hash executables on a hot keypress.
fn trust_label(activation: ActivationState, has_digest: bool) -> &'static str {
    if !has_digest {
        return "unknown (no readable descriptor)";
    }
    if activation.class() == Some("component_untrusted") {
        return "untrusted (last activation denied)";
    }
    match activation {
        ActivationState::Installed => "unverified (never activated)",
        ActivationState::Active | ActivationState::Stopped | ActivationState::Crashed => {
            "verified by the last activation"
        }
        ActivationState::Denied(_) => "not verified (last activation denied)",
    }
}

// ── Diagnostics ───────────────────────────────────────────────────────────

fn diagnostics(sources: &DashboardSources, view: &DashboardView<'_>) -> DashboardSectionData {
    let mut rows = vec![
        DashboardRow::info("bitty", APP_VERSION),
        DashboardRow::info(
            "profile",
            format!(
                "{} (source {})",
                sources.profile_name.as_deref().unwrap_or("default"),
                sources.profile_source
            ),
        ),
        DashboardRow::info("theme layer", view.theme_source),
        path_row("config file", sources.config_path.as_deref()),
        path_row("config dir", sources.config_dir.as_deref()),
        path_row("plugin ledger", sources.plugin_state_path.as_deref()),
        path_row("plugin store", sources.store_root.as_deref()),
        path_row("extensions user", sources.extension_user_root.as_deref()),
        path_row(
            "extensions system",
            sources.extension_system_root.as_deref(),
        ),
        path_row("extension state", sources.component_state_root.as_deref()),
    ];
    let installed = count_extensions(sources);
    rows.push(DashboardRow::info(
        "extensions",
        format!("{installed} installed record(s) discovered"),
    ));
    DashboardSectionData::ready(rows)
}

/// Count discovered components without duplicating the probe failure.
fn count_extensions(sources: &DashboardSources) -> usize {
    discover_components(
        sources.extension_user_root.as_deref(),
        sources.extension_system_root.as_deref(),
    )
    .len()
}

fn path_row(label: &str, path: Option<&std::path::Path>) -> DashboardRow {
    let value = path.map_or_else(
        || "(unresolved)".to_string(),
        |path| {
            let state = if path.exists() { "present" } else { "missing" };
            format!("{} ({state})", path.display())
        },
    );
    DashboardRow::path(label, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> DashboardView<'static> {
        DashboardView {
            theme_name: "bitty-dark",
            theme_source: "default",
            font_family: "JetBrainsMono Nerd Font",
            font_size: 12.0,
            decoration: DashboardDecoration::default(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-dashboard-data-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn overview_reports_profile_theme_font_and_paths() {
        let dir = scratch("overview");
        let config = dir.join("init.lua");
        let sources = DashboardSources {
            config_path: Some(config.clone()),
            config_dir: Some(dir.clone()),
            profile_name: Some("work".to_string()),
            profile_source: "cli".to_string(),
            ..DashboardSources::default()
        };
        let section = overview(&sources, &view());
        let rows = section.rows();
        assert!(
            rows.iter()
                .any(|row| row.label == "profile" && row.value == "work")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "theme" && row.value == "bitty-dark (default)")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "font" && row.value.contains("12.0pt"))
        );
        let expected = bitty_runtime::runtime::dashboard::bounded_text(
            &config.display().to_string(),
            bitty_runtime::runtime::dashboard::DASHBOARD_MAX_PATH_BYTES,
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "config" && row.value == expected)
        );
        assert!(
            rows.iter()
                .any(|row| row.value == "(current)" && row.label.contains("bitty-dark"))
        );
        assert!(rows.iter().any(|row| matches!(
            &row.action,
            DashboardAction::PreviewTheme { name } if name != "bitty-dark"
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sources_fail_closed_without_inventing_paths() {
        let section = overview(&DashboardSources::default(), &view());
        assert!(
            section
                .rows()
                .iter()
                .any(|row| row.label == "profile" && row.value.contains("default"))
        );
        assert!(
            section
                .rows()
                .iter()
                .any(|row| row.label == "config" && row.value == "(none found)")
        );
        assert!(!profiles(&DashboardSources::default()).is_ready());
        assert!(!extensions(&DashboardSources::default()).is_ready());
    }

    #[test]
    fn plugins_section_reports_empty_catalog_not_failure() {
        let dir = scratch("plugins-empty");
        let sources = DashboardSources {
            plugin_state_path: Some(dir.join("bitty-plugins.toml")),
            ..DashboardSources::default()
        };
        let section = plugins(&sources);
        assert!(section.is_ready());
        assert!(
            section
                .rows()
                .iter()
                .any(|row| row.label.contains("shell-integration"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_manifest_is_unavailable_not_empty() {
        let dir = scratch("plugins-corrupt");
        let path = dir.join("bitty-plugins.toml");
        std::fs::write(&path, b"not a manifest").expect("write");
        let sources = DashboardSources {
            plugin_state_path: Some(path),
            ..DashboardSources::default()
        };
        assert!(!plugins(&sources).is_ready());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiles_list_is_bounded_to_valid_lua_names() {
        let dir = scratch("profiles");
        let profiles_dir = dir.join(bitty_config::file::PROFILES_DIR_NAME);
        std::fs::create_dir_all(&profiles_dir).expect("profiles");
        std::fs::write(profiles_dir.join("work.lua"), b"return {}").expect("write");
        std::fs::write(profiles_dir.join("bad name.lua"), b"return {}").expect("write");
        std::fs::write(profiles_dir.join("notes.txt"), b"x").expect("write");
        let sources = DashboardSources {
            config_dir: Some(dir.clone()),
            profile_name: Some("work".to_string()),
            ..DashboardSources::default()
        };
        let rows = profiles(&sources).rows().to_vec();
        assert!(
            rows.iter()
                .any(|row| row.label == "work" && row.value == "(active)")
        );
        assert!(!rows.iter().any(|row| row.label == "bad name"));
        assert!(!rows.iter().any(|row| row.label == "notes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_show_defaults_when_no_user_file_exists() {
        let dir = scratch("settings-default");
        let config_dir = dir.join("bitty");
        std::fs::create_dir_all(&config_dir).expect("config");
        let state = config_dir.join("bitty-plugins.toml");
        std::fs::write(
            &state,
            b"state_version = 1\n\n[plugins.\"bitty-terminal.shell-integration\"]\n\
              source = \"bundled\"\n\
              manifest_hash = \"0000000000000000000000000000000000000000000000000000000000000000\"\n\
              enabled = false\n\
              granted = []\n\
              denied = []\n",
        )
        .expect("write");
        let sources = DashboardSources {
            config_dir: Some(config_dir.clone()),
            plugin_state_path: Some(state),
            ..DashboardSources::default()
        };
        // shell-integration declares no settings, so the section reports the
        // honest empty shape rather than fabricating rows.
        let section = settings(&sources);
        assert!(section.is_ready(), "{section:?}");
        assert!(
            section
                .rows()
                .iter()
                .any(|row| row.label == "settings" && row.value.contains("declares settings"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn install_component(root: &std::path::Path, name: &str, digest: &str) {
        let version_dir = root.join(name).join("0.0.1");
        std::fs::create_dir_all(&version_dir).expect("component dir");
        std::fs::write(root.join(name).join("current"), b"0.0.1").expect("current");
        std::fs::write(
            version_dir.join(bitty_runtime::component::COMPONENT_DESCRIPTOR_FILE),
            format!(
                "[component]\nname = \"{name}\"\nversion = \"0.0.1\"\nprotocol = [1, 1]\nexecutable = \"bitty-{name}\"\nsha256 = \"{digest}\"\n"
            ),
        )
        .expect("descriptor");
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .map(|status| {
                status.lines().any(|line| {
                    line.starts_with("Uid:")
                        && line.contains('\t')
                        && line.split_whitespace().nth(1) == Some("0")
                })
            })
            .unwrap_or(false)
    }

    #[test]
    fn discover_with_honors_component_root_overrides() {
        let env = DashboardEnv {
            xdg_config_home: Some("/tmp/xdg-config".to_string()),
            home: Some("/tmp/home".to_string()),
            xdg_data_home: Some("/tmp/xdg-data".to_string()),
            components_dir: Some("/custom/user-components".to_string()),
            system_components_dir: Some("/custom/system-components".to_string()),
            component_state_dir: Some("/custom/state".to_string()),
            xdg_state_home: Some("/tmp/xdg-state".to_string()),
            bitty_config: None,
        };
        let sources = DashboardSources::discover_with(
            None,
            None,
            Some("work".to_string()),
            "env",
            "bitty-dark",
            &env,
        );
        assert_eq!(
            sources.extension_user_root,
            Some(PathBuf::from("/custom/user-components"))
        );
        assert_eq!(
            sources.extension_system_root,
            Some(PathBuf::from("/custom/system-components"))
        );
        assert_eq!(
            sources.component_state_root,
            Some(PathBuf::from("/custom/state"))
        );
        assert_eq!(
            sources.config_dir,
            Some(PathBuf::from("/tmp/xdg-config/bitty"))
        );
        assert_eq!(sources.profile_source, "env");
    }

    #[test]
    fn diagnostics_report_platform_paths_and_versions() {
        // Hermetic on every platform: the sources are built from injected
        // HOME/XDG values, never the runner's environment, so a macOS temp
        // root cannot change the shape under test.
        let env = DashboardEnv {
            xdg_config_home: Some("/xdg-config".to_string()),
            home: Some("/home/test".to_string()),
            xdg_data_home: Some("/xdg-data".to_string()),
            components_dir: Some("/xdg-data/components".to_string()),
            system_components_dir: Some("/opt/components".to_string()),
            component_state_dir: Some("/xdg-state/extensions".to_string()),
            xdg_state_home: Some("/xdg-state".to_string()),
            bitty_config: None,
        };
        let config_file = PathBuf::from("/xdg-config/bitty/init.lua");
        let sources = DashboardSources::discover_with(
            None,
            Some(config_file.clone()),
            Some("work".to_string()),
            "cli",
            "bitty-dark",
            &env,
        );
        let rows = diagnostics(&sources, &view()).rows().to_vec();
        let value = |label: &str| {
            rows.iter()
                .find(|row| row.label == label)
                .map(|row| row.value.clone())
                .unwrap_or_default()
        };
        assert_eq!(value("bitty"), APP_VERSION);
        assert!(value("profile").contains("work"));
        assert!(value("profile").contains("cli"));
        assert_eq!(value("config file"), "/xdg-config/bitty/init.lua (missing)");
        assert_eq!(value("config dir"), "/xdg-config/bitty (missing)");
        assert_eq!(
            value("plugin ledger"),
            "/xdg-config/bitty/bitty-plugins.toml (missing)"
        );
        assert_eq!(value("plugin store"), "/xdg-data/bitty/plugins (missing)");
        assert_eq!(value("extensions user"), "/xdg-data/components (missing)");
        assert_eq!(value("extensions system"), "/opt/components (missing)");
        assert_eq!(value("extension state"), "/xdg-state/extensions (missing)");
        assert!(value("theme layer").contains("default"));
    }

    #[test]
    fn diagnostics_mark_existing_paths_present() {
        let dir = scratch("diagnostics");
        let sources = DashboardSources {
            config_path: Some(dir.join("init.lua")),
            config_dir: Some(dir.clone()),
            ..DashboardSources::default()
        };
        let rows = diagnostics(&sources, &view()).rows().to_vec();
        let value = |label: &str| {
            rows.iter()
                .find(|row| row.label == label)
                .map(|row| row.value.clone())
                .unwrap_or_default()
        };
        assert!(!value("config file").is_empty());
        assert!(value("config file").ends_with("(missing)"));
        assert!(value("config dir").ends_with("(present)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_with_home_fallback_uses_the_platform_config_base() {
        // The resolver is platform-aware but not macOS-special: without XDG
        // (or Windows' APPDATA/LOCALAPPDATA) every platform uses
        // `$HOME/.config/bitty`. Pinned here so a future platform base lands
        // with an intentional test change.
        let env = DashboardEnv {
            home: Some("/home/test".to_string()),
            ..DashboardEnv::default()
        };
        let sources = DashboardSources::discover_with(None, None, None, "none", "bitty-dark", &env);
        assert_eq!(
            sources.config_dir,
            Some(PathBuf::from("/home/test/.config/bitty"))
        );
    }

    #[test]
    fn extensions_surface_digest_target_activation_and_commands() {
        let dir = scratch("extensions");
        let user = dir.join("user");
        let state = dir.join("state");
        std::fs::create_dir_all(&user).expect("user root");
        let digest = "a".repeat(64);
        install_component(&user, "net", &digest);
        std::fs::create_dir_all(state.join("net")).expect("state dir");
        std::fs::write(state.join("net").join("activation"), b"v1 active\n").expect("activation");
        let sources = DashboardSources {
            extension_user_root: Some(user.clone()),
            extension_system_root: None,
            component_state_root: Some(state),
            ..DashboardSources::default()
        };
        let section = extensions(&sources);
        assert!(section.is_ready(), "{section:?}");
        let rows = section.rows();
        let value = |label: &str| {
            rows.iter()
                .find(|row| row.label.trim() == label)
                .map(|row| row.value.clone())
                .unwrap_or_default()
        };
        assert!(value("net").contains("0.0.1 active (user)"));
        assert!(value("net: digest").starts_with("aaaaaaaaaaaaaaaa"));
        let target = value("net: target");
        assert!(target.starts_with("bitty-net (user) at "), "{target}");
        assert!(target.ends_with("/net/0.0.1"), "{target}");
        assert!(value("net: activation").contains("active"));
        assert!(value("net: activation").contains("not a live probe"));
        assert!(value("net: liveness").contains("not probed"));
        assert!(value("net: trust").contains("verified"));
        assert!(value("net: install/update").contains("bitty extension install net"));
        // Every extension row is informational: the panel has no exec,
        // network, or uninstall authority.
        assert!(rows.iter().all(|row| !row.is_actionable()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_extension_root_is_unavailable_not_empty() {
        use std::os::unix::fs::PermissionsExt as _;
        if running_as_root() {
            return;
        }
        let dir = scratch("extensions-unreadable");
        let user = dir.join("user");
        std::fs::create_dir_all(&user).expect("user root");
        std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let sources = DashboardSources {
            extension_user_root: Some(user.clone()),
            extension_system_root: None,
            ..DashboardSources::default()
        };
        let section = extensions(&sources);
        assert!(!section.is_ready(), "{section:?}");
        match &section {
            DashboardSectionData::Unavailable { reason } => {
                assert!(
                    reason.contains("user extension root unreadable"),
                    "{reason}"
                );
            }
            DashboardSectionData::Ready(_) => panic!("must not read as empty"),
        }
        std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o755)).expect("restore");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_rows_stay_read_only_and_name_the_cli() {
        let dir = scratch("settings-readonly");
        let config_dir = dir.join("bitty");
        let plugins = config_dir.join(bitty_config::plugin_file::PLUGINS_DIR_NAME);
        std::fs::create_dir_all(&plugins).expect("plugins");
        // Unknown plugin id: the projection still emits the read-only hint.
        let sources = DashboardSources {
            config_dir: Some(config_dir.clone()),
            plugin_state_path: Some(config_dir.join("bitty-plugins.toml")),
            ..DashboardSources::default()
        };
        let rows = settings(&sources).rows().to_vec();
        assert!(rows.iter().all(|row| !row.is_actionable()));
        assert!(
            rows.iter()
                .any(|row| row.label == "settings are read-only here"
                    && row.value.contains("config scaffold"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sources_keep_extensions_and_diagnostics_read_only() {
        let rows = extensions(&DashboardSources::default()).rows().to_vec();
        assert!(rows.is_empty());
        let diagnostics = diagnostics(&DashboardSources::default(), &view());
        assert!(diagnostics.is_ready());
        assert!(diagnostics.rows().iter().all(|row| !row.is_actionable()));
    }

    #[test]
    fn value_and_default_labels_are_stable() {
        assert_eq!(default_label(&SettingDefault::Bool(true)), "true");
        assert_eq!(default_label(&SettingDefault::Int(3)), "3");
        assert_eq!(
            default_label(&SettingDefault::String("x".to_string())),
            "\"x\""
        );
        assert_eq!(
            default_label(&SettingDefault::Strings(vec!["a".to_string()])),
            "[a]"
        );
        assert_eq!(value_label(&LuaValue::Integer(7)), "7");
        assert_eq!(value_label(&LuaValue::String("hi".to_string())), "\"hi\"");
    }
}
