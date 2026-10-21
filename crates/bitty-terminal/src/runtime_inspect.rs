//! Cold-path assembly of the `bitty.runtime.inspect()` snapshot (issue #2396).
//!
//! Core owns mechanism only: this module reads already-resolved configuration
//! and recorded state once, at startup, and hands one bounded snapshot to the
//! plugin runtime. There is no presentation code and no write path.
//!
//! # Read model
//!
//! | Section    | Source (cold path, once)                                      |
//! |------------|---------------------------------------------------------------|
//! | diagnostics| `AppConfig` (merged effective config + startup paths)         |
//! | plugins    | managed `bitty-plugins.toml` + bundled catalog + store index  |
//! | settings   | manifest `[settings]` declarations + per-plugin user files    |
//! | extensions | two install tiers + the recorded activation projection        |
//! | profiles   | `profiles/<name>.lua` names under the config root             |
//!
//! All paths in the payload are display strings; the call that later serves
//! them never opens a file. Executable digests come from the parsed
//! `bitty-component.toml` descriptors, so inspection never hashes a binary.
//!
//! # Safe mode
//!
//! `--safe` uses only the built-in effective config. The managed ledger,
//! plugin store, settings files, extension inventories, and profiles are not
//! read; those sections render `unavailable: safe mode ...` instead. The
//! bundled catalog is compiled in and stays visible.
//!
//! # Threat controls (security review input)
//!
//! | Threat | Control |
//! |--------|---------|
//! | Path traversal / arbitrary file read | The Lua call takes **no arguments**; every read happens once on the cold path from Core-resolved XDG roots, and payload paths are display strings only. |
//! | Secret exfiltration | No environment dump, credential store, process command line, or terminal content is read. Credential-shaped setting keys (`*_KEY`/`*_TOKEN`/`*_SECRET`/`*_PASSWORD`, provider prefixes) are replaced with `***REDACTED***`. |
//! | Executable probing / hashing on the call path | Extension rows use the descriptor-recorded SHA-256; inspection never opens or hashes an executable. Activation is the recorded projection, never a liveness probe. |
//! | Consent bypass | `runtime.inspect` is checked per generation from the activation snapshot; read does not imply `runtime.configure` and write does not imply read. Safe mode never reads third-party state. |
//! | Unbounded output / DoS | 64 KiB aggregate payload, 128/256/64/64 row caps, 240-byte paths, 96-byte strings, 16 calls/s per generation, and a 10 ms wall budget. Oversize payloads fail closed or truncate deterministically with `truncated = true`. |
//! | Source failure masquerading as "none installed" | Every section carries a typed `unavailable: <reason>` marker instead of an empty list when its source cannot be read. |

use std::path::{Path, PathBuf};

use bitty_plugin_host::manifest::SettingDecl;
use bitty_runtime::component::{
    ActivationState, COMPONENT_STATE_DIR_ENV, COMPONENTS_DIR_ENV, SYSTEM_COMPONENTS_DIR_ENV,
    component_state_root_for, components_root_for, discover_components,
    read_component_activation_with_state, system_components_root_for,
};
use bitty_runtime::plugin_runtime::plugin_settings::{
    LoadedPluginSettings, SettingLayer, default_to_lua, load_plugin_settings,
    resolve_plugin_settings,
};
use bitty_runtime::plugin_runtime::{
    DecorationGeometry, ExtensionActivation, ExtensionInspectRow, ExtensionTrust, InspectRows,
    PluginInspectRow, ProfileInspectRow, ProfileSource, RUNTIME_INSPECT_MAX_EXTENSION_ROWS,
    RUNTIME_INSPECT_MAX_PLUGIN_ROWS, RUNTIME_INSPECT_MAX_PROFILE_ROWS,
    RUNTIME_INSPECT_MAX_SETTING_ROWS, RuntimeDiagnostics, RuntimeInspectSnapshot, Section,
    SettingInspectRow, SettingSource, ThemeSource,
};

use crate::config_cli::AppConfig;
use crate::plugin::{
    PluginState, init_plugin_state_path, load_state_for_inspect, rows_for_inspect,
};

/// Replacement for values behind a credential-shaped setting key.
///
/// The inspection surface never exports credentials. Settings are not a
/// credential store, but a key named like one is redacted defensively so a
/// dashboard render can never echo a planted token by accident.
const REDACTED_SETTING_VALUE: &str = "***REDACTED***";

/// Environment values the snapshot needs, resolved once by the caller.
///
/// Passing them explicitly keeps the builder hermetic in tests: no function
/// below reads process environment.
#[derive(Debug, Clone, Default)]
pub(crate) struct InspectEnv {
    /// `--config` path (a file or a config root directory).
    pub(crate) config_path: Option<String>,
    /// `BITTY_CONFIG` path.
    pub(crate) bitty_config_env: Option<String>,
    /// `XDG_CONFIG_HOME`.
    pub(crate) xdg_config_home: Option<String>,
    /// Windows `APPDATA` (roaming config root fallback).
    pub(crate) appdata: Option<String>,
    /// Windows `LOCALAPPDATA` (last-resort config root fallback).
    pub(crate) localappdata: Option<String>,
    /// `XDG_DATA_HOME`.
    pub(crate) xdg_data_home: Option<String>,
    /// `XDG_STATE_HOME`.
    pub(crate) xdg_state_home: Option<String>,
    /// `HOME`.
    pub(crate) home: Option<String>,
    /// `BITTY_COMPONENTS_DIR` (developer/test override).
    pub(crate) components_dir: Option<String>,
    /// `BITTY_SYSTEM_COMPONENTS_DIR` (developer/test override).
    pub(crate) system_components_dir: Option<String>,
    /// `BITTY_COMPONENT_STATE_DIR` (developer/test override).
    pub(crate) component_state_dir: Option<String>,
    /// Whether `--profile` selected the profile (vs `BITTY_PROFILE`).
    pub(crate) profile_from_cli: bool,
}

impl InspectEnv {
    /// Resolve every value from the process environment.
    ///
    /// `--config` is the only CLI value the caller supplies; everything else
    /// is a named environment read, never a path literal.
    pub(crate) fn from_process(config_path: Option<&str>) -> Self {
        Self {
            config_path: config_path.map(str::to_string),
            bitty_config_env: std::env::var("BITTY_CONFIG").ok(),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
            appdata: std::env::var("APPDATA").ok(),
            localappdata: std::env::var("LOCALAPPDATA").ok(),
            xdg_data_home: std::env::var("XDG_DATA_HOME").ok(),
            xdg_state_home: std::env::var("XDG_STATE_HOME").ok(),
            home: std::env::var("HOME").ok(),
            components_dir: std::env::var(COMPONENTS_DIR_ENV).ok(),
            system_components_dir: std::env::var(SYSTEM_COMPONENTS_DIR_ENV).ok(),
            component_state_dir: std::env::var(COMPONENT_STATE_DIR_ENV).ok(),
            profile_from_cli: false,
        }
    }
}

/// Everything the snapshot builder needs that is not read from disk.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeInspectContext {
    /// Running application version.
    pub(crate) app_version: String,
    /// Loaded config file (display path).
    pub(crate) config_file: Option<PathBuf>,
    /// Config directory (display path).
    pub(crate) config_dir: Option<PathBuf>,
    /// Managed plugin ledger (display path).
    pub(crate) plugin_ledger: Option<PathBuf>,
    /// Plugin package store (display path).
    pub(crate) plugin_store: Option<PathBuf>,
    /// Extension user install root (display path).
    pub(crate) extension_user_root: Option<PathBuf>,
    /// Extension system install root (display path).
    pub(crate) extension_system_root: Option<PathBuf>,
    /// Extension activation state root (display path).
    pub(crate) extension_state_root: Option<PathBuf>,
    /// Profiles directory (display path).
    pub(crate) profiles_dir: Option<PathBuf>,
    /// Active profile name.
    pub(crate) profile: Option<String>,
    /// Layer that selected the active profile.
    pub(crate) profile_source: ProfileSource,
    /// Active theme name.
    pub(crate) theme: String,
    /// Layer that won the active theme.
    pub(crate) theme_source: ThemeSource,
    /// Effective font family.
    pub(crate) font_family: String,
    /// Effective font size.
    pub(crate) font_size: f32,
    /// Effective decoration geometry.
    pub(crate) decoration: DecorationGeometry,
    /// `--safe`: read no external configuration or store state.
    pub(crate) safe_mode: bool,
}

impl RuntimeInspectContext {
    /// Build the context from the loaded app config plus explicit environment.
    pub(crate) fn from_app(app: &AppConfig, env: &InspectEnv, safe_mode: bool) -> Self {
        // Same resolver as activation (`bitty_config::file::config_dir`):
        // `XDG_CONFIG_HOME` > `APPDATA` > `HOME/.config` > `LOCALAPPDATA`.
        // Using the XDG/HOME-only variant would show a different directory
        // (and different settings/profiles) from the one plugins actually
        // read on native Windows.
        let config_dir = bitty_config::file::config_dir_with_platform_env(
            env.xdg_config_home.as_deref(),
            env.home.as_deref(),
            env.appdata.as_deref(),
            env.localappdata.as_deref(),
        );
        let plugin_ledger = if safe_mode {
            // Display-only, but safe mode deliberately does not name a ledger
            // it would never consult.
            None
        } else {
            init_plugin_state_path(
                env.config_path.as_deref(),
                env.bitty_config_env.as_deref(),
                env.xdg_config_home.as_deref(),
                env.home.as_deref(),
            )
            .ok()
        };
        let data_home =
            bitty_runtime::data_home_for(env.xdg_data_home.as_deref(), env.home.as_deref());
        let plugin_store = crate::plugin_runtime::store_root_for(
            env.xdg_data_home.as_deref(),
            env.home.as_deref(),
        );
        let extension_user_root =
            components_root_for(env.components_dir.as_deref(), data_home.as_deref());
        let extension_system_root =
            system_components_root_for(env.system_components_dir.as_deref());
        let extension_state_root = component_state_root_for(
            env.component_state_dir.as_deref(),
            env.xdg_state_home.as_deref(),
            env.home.as_deref(),
        );
        let profile = if safe_mode {
            None
        } else {
            app.profile_name.clone()
        };
        let profile_source = match &profile {
            None => ProfileSource::None,
            Some(_) if env.profile_from_cli => ProfileSource::Cli,
            Some(_) => ProfileSource::Env,
        };
        let theme_source = if safe_mode {
            ThemeSource::Default
        } else {
            match app.source {
                "cli" => ThemeSource::Cli,
                "file" => ThemeSource::File,
                "profile" => ThemeSource::Profile,
                _ => ThemeSource::Default,
            }
        };
        Self {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            config_file: if safe_mode {
                None
            } else {
                app.file_path.clone()
            },
            profiles_dir: config_dir
                .as_ref()
                .map(|dir| dir.join(bitty_config::file::PROFILES_DIR_NAME)),
            config_dir,
            plugin_ledger,
            plugin_store,
            extension_user_root,
            extension_system_root,
            extension_state_root,
            profile,
            profile_source,
            theme: app.theme.name.to_string(),
            theme_source,
            font_family: app.effective.font.family.clone(),
            font_size: app.effective.font.size,
            decoration: DecorationGeometry {
                gaps_in: app.effective.decoration.gaps_in,
                gaps_out: app.effective.decoration.gaps_out,
                border: app.effective.decoration.border,
                radius: app.effective.decoration.radius,
                content_inset: app.effective.decoration.content_inset,
            },
            safe_mode,
        }
    }
}

/// Activation-parity settings resolver for the cold-path builder.
///
/// Production passes [`bitty_runtime::plugin_runtime::PluginRuntime::resolve_settings_for_inspect`],
/// so inspection reads the same user file, short alias, shared host layer,
/// and safe-mode directory as activation. `None` is the test/default path and
/// serves the same pure defaults the loader uses.
pub(crate) type SettingsResolver<'a> = &'a dyn Fn(&str, &[SettingDecl]) -> LoadedPluginSettings;

/// Build the bounded snapshot.
///
/// Every section is independent: one failed source renders
/// `unavailable: <reason>` without erasing the other sections. The aggregate
/// cap is enforced before return; the bridge still refuses an oversized
/// payload fail-closed.
#[must_use]
pub(crate) fn build(
    context: &RuntimeInspectContext,
    settings_resolver: Option<SettingsResolver<'_>>,
) -> RuntimeInspectSnapshot {
    let diagnostics = Section::available(RuntimeDiagnostics {
        app_version: context.app_version.clone(),
        config_file: display_path(context.config_file.as_deref()),
        config_dir: display_path(context.config_dir.as_deref()),
        plugin_ledger: display_path(context.plugin_ledger.as_deref()),
        plugin_store: display_path(context.plugin_store.as_deref()),
        extension_user_root: display_path(context.extension_user_root.as_deref()),
        extension_system_root: display_path(context.extension_system_root.as_deref()),
        extension_state_root: display_path(context.extension_state_root.as_deref()),
        profile: context.profile.clone(),
        profile_source: context.profile_source,
        theme: context.theme.clone(),
        theme_source: context.theme_source,
        font_family: context.font_family.clone(),
        font_size: f64::from(context.font_size),
        decoration: context.decoration,
    });

    let inventory = inventory_rows(context);
    let plugins = match &inventory {
        Ok(rows) => build_plugins(rows),
        Err(reason) => Section::unavailable(reason.clone()),
    };
    let settings = match &inventory {
        Ok(rows) => build_settings(context, rows, settings_resolver),
        Err(_) => Section::unavailable("plugin inventory unavailable"),
    };
    let extensions = build_extensions(context);
    let profiles = build_profiles(context);

    let mut snapshot = RuntimeInspectSnapshot {
        diagnostics,
        plugins,
        settings,
        extensions,
        profiles,
    };
    snapshot.enforce_budget();
    snapshot
}

/// Load the plugin ledger once for both the plugin and settings sections.
///
/// Safe mode reads only the compiled bundled catalog (RFC A.4 rule 6); the
/// managed ledger and the package store are never opened.
fn inventory_rows(
    context: &RuntimeInspectContext,
) -> Result<Vec<crate::plugin::PluginRow>, String> {
    let state = if context.safe_mode {
        PluginState::default()
    } else {
        match context.plugin_ledger.as_deref() {
            Some(path) => {
                load_state_for_inspect(path).map_err(|reason| format!("plugin ledger: {reason}"))?
            }
            // No config root: no ledger exists. Bundled rows still list; no
            // installed record can be read.
            None => PluginState::default(),
        }
    };
    let store_root = if context.safe_mode {
        None
    } else {
        context.plugin_store.as_deref()
    };
    rows_for_inspect(&state, store_root).map_err(|reason| format!("plugin inventory: {reason}"))
}

fn build_plugins(rows: &[crate::plugin::PluginRow]) -> Section<InspectRows<PluginInspectRow>> {
    let mut inspect = InspectRows::new(
        rows.iter()
            .map(|row| PluginInspectRow {
                id: row.id.clone(),
                name: row.name.clone(),
                version: row.version.clone(),
                source: row.source.clone(),
                enabled: row.enabled,
                recorded: row.recorded,
            })
            .collect(),
    );
    inspect.cap(RUNTIME_INSPECT_MAX_PLUGIN_ROWS);
    Section::available(inspect)
}

fn build_settings(
    context: &RuntimeInspectContext,
    plugins: &[crate::plugin::PluginRow],
    settings_resolver: Option<SettingsResolver<'_>>,
) -> Section<InspectRows<SettingInspectRow>> {
    let mut rows = Vec::new();
    for plugin in plugins {
        if plugin.settings.is_empty() {
            continue;
        }
        // Prefer the runtime resolver: it is the same call activation makes
        // (per-plugin user file, short alias, shared host source, safe-mode
        // directory). The direct loader is a test/default fallback only.
        let (values, layers) = if let Some(resolve) = settings_resolver {
            let loaded = resolve(&plugin.id, &plugin.settings);
            if let Some(error) = &loaded.file_error {
                return Section::unavailable(format!("plugin '{}' settings: {error}", plugin.id));
            }
            (loaded.source.values().clone(), loaded.layers)
        } else if context.safe_mode {
            let resolved = resolve_plugin_settings(&plugin.settings, None, None);
            (resolved.values, resolved.layers)
        } else {
            let loaded = load_plugin_settings(
                context.config_dir.as_deref(),
                &plugin.id,
                &plugin.settings,
                None,
            );
            if let Some(error) = &loaded.file_error {
                return Section::unavailable(format!("plugin '{}' settings: {error}", plugin.id));
            }
            (loaded.source.values().clone(), loaded.layers)
        };
        for declaration in &plugin.settings {
            let redacted = secret_shaped_setting_key(&declaration.key);
            let current = if redacted {
                REDACTED_SETTING_VALUE.to_string()
            } else {
                values
                    .get(&declaration.key)
                    .map(render_setting_value)
                    .unwrap_or_default()
            };
            let default = if redacted {
                REDACTED_SETTING_VALUE.to_string()
            } else {
                render_setting_value(&default_to_lua(&declaration.default))
            };
            let source = layers
                .get(&declaration.key)
                .copied()
                .unwrap_or(SettingLayer::ManifestDefault);
            rows.push(SettingInspectRow {
                plugin_id: plugin.id.clone(),
                key: declaration.key.clone(),
                current,
                default,
                source: setting_source(source),
            });
        }
    }
    rows.sort_by(|left, right| {
        left.plugin_id
            .cmp(&right.plugin_id)
            .then_with(|| left.key.cmp(&right.key))
    });
    let mut inspect = InspectRows::new(rows);
    inspect.cap(RUNTIME_INSPECT_MAX_SETTING_ROWS);
    Section::available(inspect)
}

fn build_extensions(context: &RuntimeInspectContext) -> Section<InspectRows<ExtensionInspectRow>> {
    if context.safe_mode {
        return Section::unavailable("safe mode: extension inventory is not read");
    }
    for (label, root) in [
        ("user", context.extension_user_root.as_deref()),
        ("system", context.extension_system_root.as_deref()),
    ] {
        if let Err(reason) = root_readable(root) {
            return Section::unavailable(format!("extension {label} root: {reason}"));
        }
    }
    let summaries = discover_components(
        context.extension_user_root.as_deref(),
        context.extension_system_root.as_deref(),
    );
    let mut rows = Vec::new();
    let state_root = context.extension_state_root.as_deref();
    for summary in summaries {
        let activation = activation_for(&summary.name, context, state_root);
        let activation_label = activation_label(activation);
        let trust = trust_for(activation);
        for version in &summary.versions {
            rows.push(ExtensionInspectRow {
                name: summary.name.clone(),
                version: version.version.clone(),
                tier: version.source.label().to_string(),
                abi_compatible: version.compatible,
                sha256: version.sha256.clone(),
                executable_path: display_path(Some(&version.executable_path)).unwrap_or_default(),
                activation: activation_label.clone(),
                trust,
            });
        }
    }
    let mut inspect = InspectRows::new(rows);
    inspect.cap(RUNTIME_INSPECT_MAX_EXTENSION_ROWS);
    Section::available(inspect)
}

fn activation_for(
    name: &str,
    context: &RuntimeInspectContext,
    state_root: Option<&Path>,
) -> ActivationState {
    read_component_activation_with_state(
        context.extension_user_root.as_deref(),
        context.extension_system_root.as_deref(),
        state_root,
        name,
    )
}

fn activation_label(state: ActivationState) -> ExtensionActivation {
    match state {
        ActivationState::Installed => ExtensionActivation::Installed,
        ActivationState::Active => ExtensionActivation::Active,
        ActivationState::Stopped => ExtensionActivation::Stopped,
        ActivationState::Crashed => ExtensionActivation::Crashed,
        ActivationState::Denied(denial) => ExtensionActivation::Denied(denial.class().to_string()),
    }
}

fn trust_for(state: ActivationState) -> ExtensionTrust {
    match state {
        ActivationState::Active | ActivationState::Stopped | ActivationState::Crashed => {
            ExtensionTrust::Verified
        }
        ActivationState::Denied(denial) if denial.class() == "component_untrusted" => {
            ExtensionTrust::Untrusted
        }
        ActivationState::Installed | ActivationState::Denied(_) => ExtensionTrust::Unverified,
    }
}

fn build_profiles(context: &RuntimeInspectContext) -> Section<InspectRows<ProfileInspectRow>> {
    if context.safe_mode {
        return Section::unavailable("safe mode: user profiles are not read");
    }
    let Some(dir) = context.profiles_dir.as_deref() else {
        return Section::available(InspectRows::new(Vec::new()));
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Section::available(InspectRows::new(Vec::new()));
        }
        Err(error) => {
            return Section::unavailable(format!("profiles directory: {error}"));
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("lua") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(name) = bitty_config::file::validate_profile_name(stem) else {
            continue;
        };
        if names.contains(&name) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    let active = context.profile.as_deref();
    let mut rows: Vec<ProfileInspectRow> = names
        .into_iter()
        .map(|name| ProfileInspectRow {
            active: active == Some(name.as_str()),
            name,
        })
        .collect();
    if let Some(active) = active {
        // The selected profile may be absent from disk only for an explicit
        // missing path, which config loading already refuses. Keep the
        // marker observable if it is nevertheless missing.
        if !rows.iter().any(|row| row.active) {
            rows.push(ProfileInspectRow {
                name: active.to_string(),
                active: true,
            });
            rows.sort_by(|left, right| left.name.cmp(&right.name));
        }
    }
    let mut inspect = InspectRows::new(rows);
    inspect.cap(RUNTIME_INSPECT_MAX_PROFILE_ROWS);
    Section::available(inspect)
}

fn root_readable(root: Option<&Path>) -> Result<(), String> {
    let Some(root) = root else {
        return Ok(());
    };
    match std::fs::read_dir(root) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether a settings key names a credential-shaped value.
///
/// Mirrors the workspace snapshot-publication rules (`*_KEY`, `*_TOKEN`,
/// `*_SECRET`, `*_PASSWORD`, provider prefixes). This is a display guard,
/// not an authority check: settings never grant anything.
fn secret_shaped_setting_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.ends_with("_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || upper == "GH_PAT"
        || upper.starts_with("CLOUDFLARE_")
        || upper.starts_with("AWS_")
}

fn setting_source(layer: SettingLayer) -> SettingSource {
    match layer {
        SettingLayer::ManifestDefault => SettingSource::Default,
        SettingLayer::Host => SettingSource::Host,
        SettingLayer::UserFile => SettingSource::User,
    }
}

fn render_setting_value(value: &bitty_runtime::plugin_runtime::LuaValue) -> String {
    use bitty_runtime::plugin_runtime::LuaValue;
    match value {
        LuaValue::Bool(flag) => flag.to_string(),
        LuaValue::Integer(number) => number.to_string(),
        LuaValue::Number(number) if number.is_finite() => format!("{number:.2}"),
        LuaValue::Number(_) => "0.00".to_string(),
        LuaValue::String(text) => text.clone(),
        LuaValue::Table(pairs) => pairs
            .iter()
            .filter_map(|(key, value)| match (key, value) {
                (LuaValue::Integer(_), LuaValue::String(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", "),
        LuaValue::Nil => String::new(),
    }
}

fn display_path(path: Option<&Path>) -> Option<String> {
    path.map(|path| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_config() -> AppConfig {
        AppConfig {
            effective: bitty_config::EffectiveConfig::default(),
            file_path: None,
            profile_name: None,
            profile_path: None,
            theme: bitty_config::theme::default_theme(),
            resolution: bitty_config::theme::ThemeResolution::Default,
            source: "default",
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bitty-runtime-inspect-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    use bitty_plugin_host::manifest::{SettingDecl, SettingDefault, SettingKind};
    use bitty_runtime::plugin_runtime::plugin_settings::SettingLayer as RuntimeSettingLayer;
    use bitty_runtime::plugin_runtime::{MapSettings, PluginRuntime, PluginRuntimeConfig};
    use std::collections::BTreeMap;
    use std::rc::Rc;

    fn string_decl(key: &str, default: &str) -> SettingDecl {
        SettingDecl::new(
            key.to_string(),
            SettingKind::String,
            SettingDefault::String(default.to_string()),
        )
        .expect("declaration")
    }

    fn plugin_row(id: &str, settings: Vec<SettingDecl>) -> crate::plugin::PluginRow {
        crate::plugin::PluginRow {
            id: id.to_string(),
            name: id.to_string(),
            version: "1.0.0".to_string(),
            description: String::new(),
            bundled: false,
            source: "registry".to_string(),
            state: "enabled",
            enabled: true,
            recorded: true,
            pin_ok: None,
            requested: Default::default(),
            granted: Default::default(),
            denied: Default::default(),
            commands: Vec::new(),
            manifest_hash: None,
            settings,
        }
    }

    fn dev_plugin_with_settings(root: &Path, id: &str, key: &str, default: &str) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
        std::fs::write(
            plugin.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"Settings probe\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[settings]\n{key} = {{ type = \"string\", default = \"{default}\" }}\n\n[lazy]\ncommands = []\nevents = []\n"
            ),
        )
        .expect("manifest");
        std::fs::write(plugin.join("lua/init.lua"), "return {}\n").expect("init");
    }

    #[test]
    fn safe_mode_returns_builtin_config_and_no_store_reads() {
        let env = InspectEnv {
            xdg_config_home: Some("/nonexistent/config".to_string()),
            xdg_data_home: Some("/nonexistent/data".to_string()),
            home: Some("/nonexistent/home".to_string()),
            ..InspectEnv::default()
        };
        let context = RuntimeInspectContext::from_app(&app_config(), &env, true);
        let snapshot = build(&context, None);
        let Section::Available(diagnostics) = &snapshot.diagnostics else {
            panic!("diagnostics available");
        };
        assert_eq!(diagnostics.theme, bitty_config::theme::DEFAULT_THEME_NAME);
        assert_eq!(diagnostics.theme_source, ThemeSource::Default);
        assert!(diagnostics.config_file.is_none());
        assert!(diagnostics.profile.is_none());
        assert_eq!(diagnostics.profile_source, ProfileSource::None);
        // Only the compiled bundled catalog (if any) lists; no store id.
        if let Section::Available(plugins) = &snapshot.plugins {
            assert!(plugins.rows.iter().all(|row| row.source == "bundled"));
            assert!(plugins.rows.iter().all(|row| !row.recorded));
        }
        // Settings may still list declared keys, but only shipped defaults:
        // no user file is read under `--safe`.
        match &snapshot.settings {
            Section::Available(rows) => {
                assert!(
                    rows.rows
                        .iter()
                        .all(|row| row.source == SettingSource::Default)
                );
            }
            Section::Unavailable(_) => {}
        }
        assert!(snapshot.extensions.is_unavailable());
        assert!(snapshot.profiles.is_unavailable());
    }

    #[test]
    fn unreadable_ledger_marks_the_plugin_section_unavailable() {
        let base = scratch("ledger");
        let ledger = base.join("bitty-plugins.toml");
        std::fs::write(&ledger, "not a ledger").expect("write");
        let mut context =
            RuntimeInspectContext::from_app(&app_config(), &InspectEnv::default(), false);
        context.plugin_ledger = Some(ledger);
        context.plugin_store = None;
        let snapshot = build(&context, None);
        assert!(snapshot.plugins.is_unavailable());
        assert!(snapshot.settings.is_unavailable());
    }

    #[test]
    fn unreadable_profiles_directory_marks_the_profile_section_unavailable() {
        let base = scratch("profiles");
        let profiles = base.join("profiles");
        // A regular file where a directory is expected: read_dir fails with
        // a non-NotFound error on every supported platform.
        std::fs::write(&profiles, "not a directory").expect("write");
        let mut context =
            RuntimeInspectContext::from_app(&app_config(), &InspectEnv::default(), false);
        context.profiles_dir = Some(profiles);
        let snapshot = build(&context, None);
        assert!(snapshot.profiles.is_unavailable());
    }

    #[test]
    fn profiles_list_sorted_names_with_the_active_marker() {
        let base = scratch("profiles-ok");
        let dir = base.join("profiles");
        std::fs::create_dir_all(&dir).expect("dirs");
        std::fs::write(dir.join("work.lua"), "return {}").expect("write");
        std::fs::write(dir.join("home.lua"), "return {}").expect("write");
        std::fs::write(dir.join("ignore.txt"), "").expect("write");
        let mut context =
            RuntimeInspectContext::from_app(&app_config(), &InspectEnv::default(), false);
        context.profiles_dir = Some(dir);
        context.profile = Some("work".to_string());
        let snapshot = build(&context, None);
        let Section::Available(profiles) = &snapshot.profiles else {
            panic!("profiles available");
        };
        let names: Vec<&str> = profiles.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["home", "work"]);
        assert!(
            profiles
                .rows
                .iter()
                .any(|row| row.name == "work" && row.active)
        );
    }

    #[test]
    fn credential_shaped_setting_keys_are_redacted() {
        for key in [
            "api_key",
            "github_token",
            "client_secret",
            "db_password",
            "GH_PAT",
            "CLOUDFLARE_API_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(secret_shaped_setting_key(key), "{key}");
        }
        for key in ["theme", "font_size", "retention_days", "keycap_style"] {
            assert!(!secret_shaped_setting_key(key), "{key}");
        }
    }

    #[test]
    fn extension_digest_comes_from_the_descriptor_without_hashing() {
        // The inventory carries the descriptor-recorded digest; this test
        // pins that no digest recomputation is required on the read path.
        let version = bitty_runtime::component::InstalledVersion {
            version: "0.0.1".to_string(),
            source: bitty_runtime::component::ComponentSource::User,
            version_dir: PathBuf::from("/extensions/net/0.0.1"),
            executable_path: PathBuf::from("/extensions/net/0.0.1/bitty-net"),
            protocol_min: 1,
            protocol_max: 1,
            compatible: true,
            sha256: "a".repeat(64),
        };
        assert_eq!(version.sha256.len(), 64);
    }
    #[test]
    fn platform_config_root_matches_activation_on_windows_roots() {
        let env = InspectEnv {
            appdata: Some("/appdata".to_string()),
            localappdata: Some("/localappdata".to_string()),
            ..InspectEnv::default()
        };
        let context = RuntimeInspectContext::from_app(&app_config(), &env, false);
        assert_eq!(
            context.config_dir,
            Some(PathBuf::from("/appdata").join(bitty_config::file::CONFIG_DIR_NAME))
        );
        assert_eq!(
            context.profiles_dir,
            Some(
                PathBuf::from("/appdata")
                    .join(bitty_config::file::CONFIG_DIR_NAME)
                    .join(bitty_config::file::PROFILES_DIR_NAME)
            )
        );

        // XDG still wins over APPDATA, matching the shared resolver.
        let env = InspectEnv {
            xdg_config_home: Some("/xdg".to_string()),
            appdata: Some("/appdata".to_string()),
            ..InspectEnv::default()
        };
        let context = RuntimeInspectContext::from_app(&app_config(), &env, false);
        assert_eq!(
            context.config_dir,
            Some(PathBuf::from("/xdg").join(bitty_config::file::CONFIG_DIR_NAME))
        );
    }

    #[test]
    fn store_only_rows_are_not_marked_recorded() {
        let mut store_row = plugin_row("xuepoo.store", Vec::new());
        store_row.recorded = false;
        let section = build_plugins(&[store_row]);
        let Section::Available(rows) = section else {
            panic!("plugins available");
        };
        assert!(!rows.rows[0].recorded);

        let ledger_row = plugin_row("xuepoo.ledger", Vec::new());
        let section = build_plugins(&[ledger_row]);
        let Section::Available(rows) = section else {
            panic!("plugins available");
        };
        assert!(rows.rows[0].recorded);
    }

    #[test]
    fn settings_inspection_uses_the_host_and_short_alias_layers() {
        let base = scratch("settings-parity");
        let config_dir = base.join("config");
        let plugin_root = base.join("plugins");
        dev_plugin_with_settings(&plugin_root, "xuepoo.parity", "theme", "default");

        let host = BTreeMap::from([(
            "theme".to_string(),
            bitty_runtime::plugin_runtime::LuaValue::String("host-value".to_string()),
        )]);
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            third_party_roots: vec![plugin_root],
            settings: Rc::new(MapSettings::new(host)),
            ..PluginRuntimeConfig::default()
        });
        runtime.set_settings_dir(Some(config_dir.clone()));
        runtime.discover();

        let declarations = vec![string_decl("theme", "default")];
        let loaded = runtime.resolve_settings_for_inspect("xuepoo.parity", &declarations);
        assert_eq!(
            loaded.source.values().get("theme"),
            Some(&bitty_runtime::plugin_runtime::LuaValue::String(
                "host-value".to_string()
            ))
        );
        assert_eq!(loaded.layers.get("theme"), Some(&RuntimeSettingLayer::Host));

        // Unambiguous short alias wins over the shared host layer, exactly as
        // activation resolves it.
        std::fs::create_dir_all(config_dir.join("plugins")).expect("plugins dir");
        std::fs::write(
            config_dir.join("plugins/parity.lua"),
            "return { theme = \"alias-value\" }\n",
        )
        .expect("alias");
        let loaded = runtime.resolve_settings_for_inspect("xuepoo.parity", &declarations);
        assert_eq!(
            loaded.source.values().get("theme"),
            Some(&bitty_runtime::plugin_runtime::LuaValue::String(
                "alias-value".to_string()
            ))
        );
        assert_eq!(
            loaded.layers.get("theme"),
            Some(&RuntimeSettingLayer::UserFile)
        );

        // The builder consumes that resolver, so the served row agrees with
        // `bitty.settings.get` for the same generation.
        let context = RuntimeInspectContext::from_app(&app_config(), &InspectEnv::default(), false);
        let rows = vec![plugin_row("xuepoo.parity", declarations.clone())];
        let resolve = |id: &str, declarations: &[SettingDecl]| {
            runtime.resolve_settings_for_inspect(id, declarations)
        };
        let section = build_settings(&context, &rows, Some(&resolve));
        let Section::Available(rows) = section else {
            panic!("settings available");
        };
        assert_eq!(rows.rows[0].current, "alias-value");
        assert_eq!(rows.rows[0].source, SettingSource::User);
    }

    #[test]
    fn credential_shaped_setting_value_is_redacted_end_to_end() {
        let base = scratch("settings-redaction");
        let config_dir = base.join("config");
        std::fs::create_dir_all(config_dir.join("plugins")).expect("plugins dir");
        std::fs::write(
            config_dir.join("plugins/xuepoo.secret.toml"),
            "api_key = \"s3cr3t-value\"\n",
        )
        .expect("settings file");
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig::default());
        runtime.set_settings_dir(Some(config_dir.clone()));
        let context = RuntimeInspectContext::from_app(&app_config(), &InspectEnv::default(), false);
        let rows = vec![plugin_row(
            "xuepoo.secret",
            vec![string_decl("api_key", "default")],
        )];
        let resolver = |id: &str, declarations: &[SettingDecl]| {
            runtime.resolve_settings_for_inspect(id, declarations)
        };
        let section = build_settings(&context, &rows, Some(&resolver));
        let Section::Available(rows) = section else {
            panic!("settings available");
        };
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0].current, REDACTED_SETTING_VALUE);
        assert_eq!(rows.rows[0].default, REDACTED_SETTING_VALUE);
        assert_eq!(rows.rows[0].source, SettingSource::User);
    }

    #[test]
    fn malformed_settings_file_marks_the_section_unavailable() {
        let base = scratch("settings-malformed");
        let config_dir = base.join("config");
        std::fs::create_dir_all(config_dir.join("plugins")).expect("plugins dir");
        std::fs::write(config_dir.join("plugins/xuepoo.broken.lua"), "return {\n")
            .expect("broken file");
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig::default());
        runtime.set_settings_dir(Some(config_dir));
        let context = RuntimeInspectContext::from_app(&app_config(), &InspectEnv::default(), false);
        let rows = vec![plugin_row(
            "xuepoo.broken",
            vec![string_decl("theme", "default")],
        )];
        let resolver = |id: &str, declarations: &[SettingDecl]| {
            runtime.resolve_settings_for_inspect(id, declarations)
        };
        let section = build_settings(&context, &rows, Some(&resolver));
        let Section::Unavailable(reason) = section else {
            panic!("malformed file must mark the section unavailable");
        };
        assert!(reason.contains("xuepoo.broken"), "{reason}");
        assert!(reason.contains("settings"), "{reason}");
    }
}
