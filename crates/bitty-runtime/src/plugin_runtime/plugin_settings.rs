//! Per-plugin settings resolution: shipped defaults plus user files (CTX-1195).
//!
//! This module merges the three values-plane layers for one plugin
//! generation and serves them through the same [`SettingsSource`] that feeds
//! `bitty.settings.get`. Authority never passes through here: the merge only
//! reads `bitty-plugin-host` declarations and `bitty-config` file values, so
//! no config value, however explicit, can grant anything (configs flow,
//! informedness does not — deny-by-default is P0).
//!
//! # Precedence (per declared key, lowest first)
//!
//! ```text
//! manifest shipped default < old home (shared host source) < new file
//! (`<config-dir>/plugins/<owner>.<name>.toml`)
//! ```
//!
//! - The **manifest default** always exists for a declared key (declarations
//!   carry their default), so resolution is total: every declared key serves
//!   a value, never an error.
//! - The **old home** is the runtime-shared host source (whatever the
//!   application wired — today `EmptySettings`, tomorrow main-config
//!   values): a declared key served there keeps working, so the old four
//!   scalar settings keep working from either home.
//! - The **new file** wins per key when present and well-typed. A wrong-typed
//!   file value falls back to the next lower layer for that key only
//!   (garbage in, fail-to-default out — the `read_settings` pattern); a bad
//!   value can hide a module at most, never crash the host.
//! - A malformed or unreadable file falls back to the lower layers for every
//!   key (recorded in [`ResolvedPluginSettings::file_error`]); an absent
//!   file is not an error.
//! - File keys no declaration names are dropped (recorded in
//!   [`ResolvedPluginSettings::dropped`]): settings are never a side channel
//!   between plugins, and a `[capabilities]`-looking file key grants nothing.
//!
//! # Wiring
//!
//! [`load_plugin_settings`] reads the file (bounded by the `bitty-config`
//! parser caps) and returns the per-generation source plus diagnostics.
//! Activation installs the source into the generation's [`PluginServices`]
//! and pins the declared key set there, so reading an undeclared key fails
//! closed (`E_SETTINGS_UNDECLARED`) at the bridge. Each generation holds its
//! own resolved map: a cross-plugin read fails closed structurally (the key
//! is absent) and at the gate (the key is undeclared).
//!
//! [`PluginServices`]: super::services::PluginServices
//! [`SettingsSource`]: super::services::SettingsSource

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use bitty_config::plugin_file::{self, ParsedPluginFile, PluginFileValue};
use bitty_lua::{ConfigLuaVm, PlainConfigLimits, PlainConfigValue};
use bitty_plugin_host::manifest::{SettingDecl, SettingDefault, SettingKind};

use super::LuaValue;
use super::services::SettingsSource;

// ---------------------------------------------------------------------------
// Type agreement (strict: no coercion, ever)
// ---------------------------------------------------------------------------

/// Whether a file-layer value agrees with the declared kind.
#[must_use]
pub fn file_value_matches_kind(value: &PluginFileValue, kind: SettingKind) -> bool {
    matches!(
        (value, kind),
        (PluginFileValue::Bool(_), SettingKind::Bool)
            | (PluginFileValue::Int(_), SettingKind::Int)
            | (PluginFileValue::String(_), SettingKind::String)
            | (PluginFileValue::Strings(_), SettingKind::StringArray)
    )
}

/// Whether a host-layer [`LuaValue`] agrees with the declared kind.
///
/// Strict: an `Integer` is not a `Number` and a map table is not an array.
/// A float `32.0` for an int-declared key fails to the default — coercion
/// would turn representation accidents into configuration.
#[must_use]
pub fn lua_value_matches_kind(value: &LuaValue, kind: SettingKind) -> bool {
    match (value, kind) {
        (LuaValue::Bool(_), SettingKind::Bool) => true,
        (LuaValue::Integer(_), SettingKind::Int) => true,
        (LuaValue::String(_), SettingKind::String) => true,
        (LuaValue::Table(pairs), SettingKind::StringArray) => is_string_array_table(pairs),
        _ => false,
    }
}

/// Whether a [`LuaValue`] table is a string array: consecutive 1-based
/// integer keys with all-string values (the [`LuaValue::array`] shape). An
/// empty table is an empty array.
fn is_string_array_table(pairs: &[(LuaValue, LuaValue)]) -> bool {
    for (index, (key, value)) in pairs.iter().enumerate() {
        let want = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1);
        if *key != LuaValue::Integer(want) {
            return false;
        }
        if !matches!(value, LuaValue::String(_)) {
            return false;
        }
    }
    true
}

/// Convert a file-layer value to a [`LuaValue`].
#[must_use]
pub fn file_value_to_lua(value: &PluginFileValue) -> LuaValue {
    match value {
        PluginFileValue::Bool(flag) => LuaValue::Bool(*flag),
        PluginFileValue::Int(number) => LuaValue::Integer(*number),
        PluginFileValue::String(text) => LuaValue::String(text.clone()),
        PluginFileValue::Strings(items) => LuaValue::array(
            items
                .iter()
                .map(|item| LuaValue::String(item.clone()))
                .collect(),
        ),
    }
}

/// Convert a shipped manifest default to a [`LuaValue`].
#[must_use]
pub fn default_to_lua(default: &SettingDefault) -> LuaValue {
    match default {
        SettingDefault::Bool(flag) => LuaValue::Bool(*flag),
        SettingDefault::Int(number) => LuaValue::Integer(*number),
        SettingDefault::String(text) => LuaValue::String(text.clone()),
        SettingDefault::Strings(items) => LuaValue::array(
            items
                .iter()
                .map(|item| LuaValue::String(item.clone()))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Resolution (pure: no I/O)
// ---------------------------------------------------------------------------

/// Resolved settings for one plugin generation (plain data).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPluginSettings {
    /// Effective value per declared key (total: every declared key serves).
    pub values: BTreeMap<String, LuaValue>,
    /// File keys that changed nothing, sorted: undeclared keys, wrong-typed
    /// values for declared keys, and parser-skipped keys.
    pub dropped: Vec<String>,
}

/// Merge the three values-plane layers for one plugin (pure, no I/O).
///
/// `declared` is the manifest `[settings]` set (already validated);
/// `file` is the parsed user file when one was read; `base` is the shared
/// host source (the old home) when the application wired one. Precedence per
/// declared key: default < base < file, each step gated on strict type
/// agreement (no coercion). Undeclared file keys never enter `values`.
#[must_use]
pub fn resolve_plugin_settings(
    declared: &[SettingDecl],
    file: Option<&ParsedPluginFile>,
    base: Option<&dyn SettingsSource>,
) -> ResolvedPluginSettings {
    let mut values = BTreeMap::new();
    let mut dropped = BTreeSet::new();
    // Parser-skipped keys are dropped whatever they name.
    if let Some(parsed) = file {
        dropped.extend(parsed.skipped.iter().cloned());
    }
    for decl in declared {
        let mut value = default_to_lua(&decl.default);
        if let Some(source) = base {
            if let Some(candidate) = source.get(&decl.key) {
                if lua_value_matches_kind(&candidate, decl.kind) {
                    value = candidate;
                }
            }
        }
        if let Some(parsed) = file {
            match parsed.values.get(&decl.key) {
                Some(candidate) if file_value_matches_kind(candidate, decl.kind) => {
                    value = file_value_to_lua(candidate);
                }
                Some(_) => {
                    dropped.insert(decl.key.clone());
                }
                None => {}
            }
        }
        values.insert(decl.key.clone(), value);
    }
    // Declared keys consumed above; every remaining file key is undeclared.
    if let Some(parsed) = file {
        for key in parsed.values.keys() {
            if !values.contains_key(key) {
                dropped.insert(key.clone());
            }
        }
    }
    ResolvedPluginSettings {
        values,
        dropped: dropped.into_iter().collect(),
    }
}

/// Declared key set for the [`PluginServices`] read gate: reading any other
/// key fails closed (`E_SETTINGS_UNDECLARED`).
///
/// [`PluginServices`]: super::services::PluginServices
#[must_use]
pub fn declared_keys(declared: &[SettingDecl]) -> BTreeSet<String> {
    declared.iter().map(|decl| decl.key.clone()).collect()
}

// ---------------------------------------------------------------------------
// Per-generation source
// ---------------------------------------------------------------------------

/// [`SettingsSource`] serving one generation's resolved map.
///
/// Each generation holds its own instance: values never cross plugin
/// boundaries, so a cross-plugin read fails closed structurally before any
/// gate runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginSettingsSource {
    values: BTreeMap<String, LuaValue>,
}

impl PluginSettingsSource {
    /// Build a source from resolved values.
    #[must_use]
    pub fn new(values: BTreeMap<String, LuaValue>) -> Self {
        Self { values }
    }

    /// The resolved map (diagnostics, tests).
    #[must_use]
    pub fn values(&self) -> &BTreeMap<String, LuaValue> {
        &self.values
    }
}

impl SettingsSource for PluginSettingsSource {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.values.get(key).cloned()
    }
}

// ---------------------------------------------------------------------------
// Loading (file I/O at the boundary)
// ---------------------------------------------------------------------------

/// Outcome of [`load_plugin_settings`].
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPluginSettings {
    /// Per-generation source for the activation.
    pub source: PluginSettingsSource,
    /// Merge diagnostics (see [`ResolvedPluginSettings::dropped`]).
    pub dropped: Vec<String>,
    /// Whole-file failure, if any: malformed, oversize, non-UTF-8, or
    /// unreadable. `None` when the file applied (or was absent, which is not
    /// an error). Bounded; carries key names at most, never file bytes.
    pub file_error: Option<String>,
}

fn plain_value_to_lua(value: &PlainConfigValue) -> Option<LuaValue> {
    match value {
        PlainConfigValue::Bool(value) => Some(LuaValue::Bool(*value)),
        PlainConfigValue::Integer(value) => Some(LuaValue::Integer(*value)),
        PlainConfigValue::String(value) => Some(LuaValue::String(value.clone())),
        PlainConfigValue::Array(values)
            if values
                .iter()
                .all(|value| matches!(value, PlainConfigValue::String(_))) =>
        {
            Some(LuaValue::array(
                values
                    .iter()
                    .map(|value| match value {
                        PlainConfigValue::String(value) => LuaValue::String(value.clone()),
                        _ => unreachable!(),
                    })
                    .collect(),
            ))
        }
        PlainConfigValue::Array(_) => None,
        PlainConfigValue::Table(values) if values.is_empty() => Some(LuaValue::array(Vec::new())),
        PlainConfigValue::Number(_) | PlainConfigValue::Table(_) => None,
    }
}

fn plain_settings_to_lua(
    value: PlainConfigValue,
) -> Result<(BTreeMap<String, LuaValue>, Vec<String>), String> {
    let PlainConfigValue::Table(values) = value else {
        return Err("plugin configuration must return a table".to_string());
    };
    let mut output = BTreeMap::new();
    let mut dropped = Vec::new();
    for (key, value) in values {
        if let Some(value) = plain_value_to_lua(&value) {
            output.insert(key, value);
        } else {
            dropped.push(key);
        }
    }
    Ok((output, dropped))
}

fn merge_lua_settings(
    declared: &[SettingDecl],
    lua: Option<&BTreeMap<String, LuaValue>>,
    base: Option<&dyn SettingsSource>,
) -> ResolvedPluginSettings {
    let mut values = BTreeMap::new();
    let mut dropped = BTreeSet::new();
    for decl in declared {
        let mut value = default_to_lua(&decl.default);
        if let Some(source) = base {
            if let Some(candidate) = source.get(&decl.key) {
                if lua_value_matches_kind(&candidate, decl.kind) {
                    value = candidate;
                }
            }
        }
        if let Some(source) = lua {
            match source.get(&decl.key) {
                Some(candidate) if lua_value_matches_kind(candidate, decl.kind) => {
                    value = candidate.clone();
                }
                Some(_) => {
                    dropped.insert(decl.key.clone());
                }
                None => {}
            }
        }
        values.insert(decl.key.clone(), value);
    }
    if let Some(source) = lua {
        for key in source.keys() {
            if !values.contains_key(key) {
                dropped.insert(key.clone());
            }
        }
    }
    ResolvedPluginSettings {
        values,
        dropped: dropped.into_iter().collect(),
    }
}

fn lua_config_paths(
    config_dir: &Path,
    plugin_id: &str,
    discovered_ids: Option<&[String]>,
) -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
    if !plugin_file::valid_plugin_file_id(plugin_id) {
        return None;
    }
    let plugins = config_dir.join(plugin_file::PLUGINS_DIR_NAME);
    let canonical = plugins.join(format!("{plugin_id}.lua"));
    let alias = discovered_ids.and_then(|ids| {
        if ids.iter().any(|id| !plugin_file::valid_plugin_file_id(id)) {
            return None;
        }
        if !ids.iter().any(|id| id == plugin_id) {
            return None;
        }
        let (_, short_name) = plugin_id.split_once('.')?;
        let owners: BTreeSet<&str> = ids
            .iter()
            .filter_map(|id| {
                let (owner, name) = id.split_once('.')?;
                (name == short_name && plugin_file::valid_plugin_file_id(id)).then_some(owner)
            })
            .collect();
        (owners.len() == 1).then(|| plugins.join(format!("{short_name}.lua")))
    });
    Some((canonical, alias))
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigReadError {
    NotFound,
    Rejected(String),
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn read_bounded_regular_file(path: &Path) -> Result<Vec<u8>, ConfigReadError> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let file = {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let result = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        );
        match result {
            Ok(fd) => std::fs::File::from(fd),
            Err(error) if error == rustix::io::Errno::NOENT => {
                return Err(ConfigReadError::NotFound);
            }
            Err(error) => {
                return Err(ConfigReadError::Rejected(format!(
                    "unreadable plugin config: {error}"
                )));
            }
        }
    };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    let file = {
        let result = nix::fcntl::open(
            path,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_NONBLOCK
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        );
        match result {
            Ok(fd) => std::fs::File::from(fd),
            Err(error) if error == nix::errno::Errno::ENOENT => {
                return Err(ConfigReadError::NotFound);
            }
            Err(error) => {
                return Err(ConfigReadError::Rejected(format!(
                    "unreadable plugin config: {error}"
                )));
            }
        }
    };

    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))
    ))]
    return Err(ConfigReadError::Rejected(
        "plugin Lua config loading is unsupported on this platform".to_string(),
    ));

    let metadata = file
        .metadata()
        .map_err(|error| ConfigReadError::Rejected(format!("unreadable plugin config: {error}")))?;
    if !metadata.is_file() {
        return Err(ConfigReadError::Rejected(
            "plugin config must be a regular file".to_string(),
        ));
    }
    if metadata.len() > bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES as u64 {
        return Err(ConfigReadError::Rejected(
            "plugin config exceeds the byte ceiling".to_string(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigReadError::Rejected(format!("unreadable plugin config: {error}")))?;
    if bytes.len() as u64 != metadata.len()
        || bytes.len() > bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES
    {
        return Err(ConfigReadError::Rejected(
            "plugin config changed while reading".to_string(),
        ));
    }
    Ok(bytes)
}

#[cfg(windows)]
fn read_bounded_regular_file(path: &Path) -> Result<Vec<u8>, ConfigReadError> {
    use std::os::windows::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ConfigReadError::NotFound
            } else {
                ConfigReadError::Rejected(format!("unreadable plugin config: {error}"))
            }
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| ConfigReadError::Rejected(format!("unreadable plugin config: {error}")))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ConfigReadError::Rejected(
            "plugin config must be a regular file".to_string(),
        ));
    }
    if metadata.len() > bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES as u64 {
        return Err(ConfigReadError::Rejected(
            "plugin config exceeds the byte ceiling".to_string(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigReadError::Rejected(format!("unreadable plugin config: {error}")))?;
    if bytes.len() as u64 != metadata.len()
        || bytes.len() > bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES
    {
        return Err(ConfigReadError::Rejected(
            "plugin config changed while reading".to_string(),
        ));
    }
    Ok(bytes)
}

#[cfg(all(
    not(windows),
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
fn read_bounded_regular_file(_path: &Path) -> Result<Vec<u8>, ConfigReadError> {
    Err(ConfigReadError::Rejected(
        "plugin config loading is unsupported on this platform".to_string(),
    ))
}

/// Load and resolve one plugin's settings (file I/O at the boundary).
///
/// `config_dir` is the `<config-root>/bitty` directory (env-derived by the
/// application, never a literal here); `None` skips the file layer. An
/// absent file is not an error (`file_error` stays `None` and lower layers
/// serve). A malformed/oversize/unreadable file resolves every key from the
/// lower layers with `file_error` set — the plugin keeps running on
/// defaults, never aborts.
///
/// `base` is the shared host source (the old home); `None` serves an empty
/// old home.
pub fn load_plugin_settings(
    config_dir: Option<&Path>,
    plugin_id: &str,
    declared: &[SettingDecl],
    base: Option<&dyn SettingsSource>,
) -> LoadedPluginSettings {
    load_plugin_settings_with_plugins(config_dir, plugin_id, declared, base, None)
}

/// Load settings with the complete discovered-plugin map used for short-name
/// alias admission. `None` deliberately disables aliases.
pub fn load_plugin_settings_with_plugins(
    config_dir: Option<&Path>,
    plugin_id: &str,
    declared: &[SettingDecl],
    base: Option<&dyn SettingsSource>,
    discovered_ids: Option<&[String]>,
) -> LoadedPluginSettings {
    let path = match (config_dir, plugin_file::valid_plugin_file_id(plugin_id)) {
        // Validated ids match `owner.name` with no separators, so joining
        // the file name cannot escape the `plugins/` directory (fail-closed
        // `None` otherwise).
        (Some(dir), true) => lua_config_paths(dir, plugin_id, discovered_ids),
        _ => None,
    };
    let lua_bytes = match path {
        Some((canonical_path, alias_path)) => match read_bounded_regular_file(&canonical_path) {
            Ok(bytes) => Some(bytes),
            Err(ConfigReadError::NotFound) => match alias_path {
                Some(alias_path) => match read_bounded_regular_file(&alias_path) {
                    Ok(bytes) => Some(bytes),
                    Err(ConfigReadError::NotFound) => None,
                    Err(ConfigReadError::Rejected(error)) => {
                        let resolved = merge_lua_settings(declared, None, base);
                        return LoadedPluginSettings {
                            source: PluginSettingsSource::new(resolved.values),
                            dropped: resolved.dropped,
                            file_error: Some(error),
                        };
                    }
                },
                None => None,
            },
            Err(ConfigReadError::Rejected(error)) => {
                let resolved = merge_lua_settings(declared, None, base);
                return LoadedPluginSettings {
                    source: PluginSettingsSource::new(resolved.values),
                    dropped: resolved.dropped,
                    file_error: Some(error),
                };
            }
        },
        None => None,
    };
    if let Some(bytes) = lua_bytes {
        let limits = PlainConfigLimits {
            source_bytes: bitty_config::plugin_file::MAX_PLUGIN_FILE_BYTES,
            ..PlainConfigLimits::default()
        };
        let lua_result = ConfigLuaVm::with_limits(plugin_id, limits)
            .map_err(|error| error.to_string())
            .and_then(|mut vm| {
                let source = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
                vm.eval_plain_config(source)
                    .map_err(|error| error.to_string())
            });
        match lua_result.and_then(plain_settings_to_lua) {
            Ok((values, mut dropped)) => {
                let resolved = merge_lua_settings(declared, Some(&values), base);
                dropped.extend(resolved.dropped);
                dropped.sort();
                dropped.dedup();
                return LoadedPluginSettings {
                    source: PluginSettingsSource::new(resolved.values),
                    dropped,
                    file_error: None,
                };
            }
            Err(error) => {
                let resolved = merge_lua_settings(declared, None, base);
                return LoadedPluginSettings {
                    source: PluginSettingsSource::new(resolved.values),
                    dropped: resolved.dropped,
                    file_error: Some(format!("plugin Lua config rejected: {error}")),
                };
            }
        }
    }

    let toml_path = match (config_dir, plugin_file::valid_plugin_file_id(plugin_id)) {
        (Some(dir), true) => Some(
            dir.join(plugin_file::PLUGINS_DIR_NAME)
                .join(format!("{plugin_id}.toml")),
        ),
        _ => None,
    };
    let Some(path) = toml_path else {
        let resolved = resolve_plugin_settings(declared, None, base);
        return LoadedPluginSettings {
            source: PluginSettingsSource::new(resolved.values),
            dropped: resolved.dropped,
            file_error: None,
        };
    };
    let bytes = match read_bounded_regular_file(&path) {
        Ok(bytes) => bytes,
        Err(ConfigReadError::NotFound) => {
            let resolved = resolve_plugin_settings(declared, None, base);
            return LoadedPluginSettings {
                source: PluginSettingsSource::new(resolved.values),
                dropped: resolved.dropped,
                file_error: None,
            };
        }
        Err(ConfigReadError::Rejected(error)) => {
            let resolved = resolve_plugin_settings(declared, None, base);
            return LoadedPluginSettings {
                source: PluginSettingsSource::new(resolved.values),
                dropped: resolved.dropped,
                file_error: Some(error),
            };
        }
    };
    match plugin_file::parse_plugin_file(&bytes) {
        Ok(parsed) => {
            let resolved = resolve_plugin_settings(declared, Some(&parsed), base);
            LoadedPluginSettings {
                source: PluginSettingsSource::new(resolved.values),
                dropped: resolved.dropped,
                file_error: None,
            }
        }
        Err(error) => {
            let resolved = resolve_plugin_settings(declared, None, base);
            LoadedPluginSettings {
                source: PluginSettingsSource::new(resolved.values),
                dropped: resolved.dropped,
                file_error: Some(error.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_plugin_host::manifest::{SettingDecl, SettingDefault, SettingKind};

    /// Bar-fidelity declarations (mirrors the host-side contract test).
    fn bar_decls() -> Vec<SettingDecl> {
        vec![
            SettingDecl::new(
                "show".to_string(),
                SettingKind::Bool,
                SettingDefault::Bool(true),
            )
            .expect("decl"),
            SettingDecl::new(
                "edge".to_string(),
                SettingKind::String,
                SettingDefault::String("bottom".to_string()),
            )
            .expect("decl"),
            SettingDecl::new(
                "show_single".to_string(),
                SettingKind::Bool,
                SettingDefault::Bool(false),
            )
            .expect("decl"),
            SettingDecl::new(
                "name_max_chars".to_string(),
                SettingKind::Int,
                SettingDefault::Int(32),
            )
            .expect("decl"),
            SettingDecl::new(
                "modules".to_string(),
                SettingKind::StringArray,
                SettingDefault::Strings(vec!["workspaces".to_string()]),
            )
            .expect("decl"),
        ]
    }

    fn parse_body(body: &str) -> ParsedPluginFile {
        plugin_file::parse_plugin_file(body.as_bytes()).expect("fixture must parse")
    }

    #[test]
    fn absent_file_serves_shipped_defaults() {
        let resolved = resolve_plugin_settings(&bar_decls(), None, None);
        assert_eq!(resolved.values.get("show"), Some(&LuaValue::Bool(true)));
        assert_eq!(
            resolved.values.get("edge"),
            Some(&LuaValue::String("bottom".to_string()))
        );
        assert_eq!(
            resolved.values.get("name_max_chars"),
            Some(&LuaValue::Integer(32))
        );
        assert_eq!(
            resolved.values.get("modules"),
            Some(&LuaValue::array(vec![LuaValue::String(
                "workspaces".to_string()
            )]))
        );
        assert!(resolved.dropped.is_empty());
    }

    #[test]
    fn file_overrides_defaults_per_key() {
        let file = parse_body("show = false\nmodules = [\"clock\", \"workspaces\"]\n");
        let resolved = resolve_plugin_settings(&bar_decls(), Some(&file), None);
        assert_eq!(resolved.values.get("show"), Some(&LuaValue::Bool(false)));
        // Untouched keys keep their defaults.
        assert_eq!(
            resolved.values.get("edge"),
            Some(&LuaValue::String("bottom".to_string()))
        );
        assert_eq!(
            resolved.values.get("modules"),
            Some(&LuaValue::array(vec![
                LuaValue::String("clock".to_string()),
                LuaValue::String("workspaces".to_string()),
            ]))
        );
        assert!(resolved.dropped.is_empty());
    }

    #[test]
    fn wrong_typed_values_fail_to_default_per_key() {
        // `show` wants bool, `name_max_chars` wants int, `modules` wants an
        // array: every mismatch falls back while the good key applies.
        let file = parse_body(
            "show = \"yes\"\nedge = \"top\"\nname_max_chars = 3.5\nmodules = \"clock\"\n",
        );
        let resolved = resolve_plugin_settings(&bar_decls(), Some(&file), None);
        assert_eq!(resolved.values.get("show"), Some(&LuaValue::Bool(true)));
        assert_eq!(
            resolved.values.get("edge"),
            Some(&LuaValue::String("top".to_string()))
        );
        assert_eq!(
            resolved.values.get("name_max_chars"),
            Some(&LuaValue::Integer(32))
        );
        assert_eq!(
            resolved.values.get("modules"),
            Some(&LuaValue::array(vec![LuaValue::String(
                "workspaces".to_string()
            )]))
        );
        // `3.5` is not even a file-layer shape (skipped at parse);
        // `"yes"` and `"clock"` parse but mistype at merge.
        assert!(resolved.dropped.contains(&"show".to_string()));
        assert!(resolved.dropped.contains(&"name_max_chars".to_string()));
        assert!(resolved.dropped.contains(&"modules".to_string()));
        assert!(!resolved.dropped.contains(&"edge".to_string()));
    }

    #[test]
    fn old_home_serves_file_wins() {
        // The shared host source (old home) serves declared keys; the file
        // overrides per key.
        struct MapBase(BTreeMap<String, LuaValue>);
        impl SettingsSource for MapBase {
            fn get(&self, key: &str) -> Option<LuaValue> {
                self.0.get(key).cloned()
            }
        }
        let base = MapBase(BTreeMap::from([
            ("show".to_string(), LuaValue::Bool(false)),
            ("edge".to_string(), LuaValue::String("top".to_string())),
            // Wrong-typed base value: falls back, never trusted.
            ("name_max_chars".to_string(), LuaValue::Number(8.0)),
        ]));
        // Base alone: old home keeps working.
        let resolved = resolve_plugin_settings(&bar_decls(), None, Some(&base));
        assert_eq!(resolved.values.get("show"), Some(&LuaValue::Bool(false)));
        assert_eq!(
            resolved.values.get("edge"),
            Some(&LuaValue::String("top".to_string()))
        );
        assert_eq!(
            resolved.values.get("name_max_chars"),
            Some(&LuaValue::Integer(32))
        );
        // File wins over the old home per key.
        let file = parse_body("show = true\n");
        let resolved = resolve_plugin_settings(&bar_decls(), Some(&file), Some(&base));
        assert_eq!(resolved.values.get("show"), Some(&LuaValue::Bool(true)));
        assert_eq!(
            resolved.values.get("edge"),
            Some(&LuaValue::String("top".to_string()))
        );
    }

    #[test]
    fn undeclared_file_keys_dropped_granting_nothing() {
        // A `[capabilities]`-looking key is an unknown key like any other:
        // dropped, recorded, granting nothing.
        let file = parse_body("show = false\nfrobnicate = 7\ncapabilities = true\n");
        let resolved = resolve_plugin_settings(&bar_decls(), Some(&file), None);
        assert_eq!(resolved.values.get("show"), Some(&LuaValue::Bool(false)));
        assert!(!resolved.values.contains_key("frobnicate"));
        assert!(!resolved.values.contains_key("capabilities"));
        assert!(resolved.dropped.contains(&"frobnicate".to_string()));
        assert!(resolved.dropped.contains(&"capabilities".to_string()));
    }

    #[test]
    fn strict_kind_agreement_no_coercion() {
        // Floats are not ints, map tables are not arrays, ints are not bools.
        assert!(!lua_value_matches_kind(
            &LuaValue::Number(32.0),
            SettingKind::Int
        ));
        assert!(!lua_value_matches_kind(
            &LuaValue::Integer(1),
            SettingKind::Bool
        ));
        assert!(!lua_value_matches_kind(
            &LuaValue::table([("a", LuaValue::String("x".to_string()))]),
            SettingKind::StringArray
        ));
        // Gapped integer keys are not an array.
        assert!(!lua_value_matches_kind(
            &LuaValue::Table(vec![
                (LuaValue::Integer(1), LuaValue::String("a".to_string())),
                (LuaValue::Integer(3), LuaValue::String("b".to_string())),
            ]),
            SettingKind::StringArray
        ));
        // Non-string items are not a string array.
        assert!(!lua_value_matches_kind(
            &LuaValue::array(vec![LuaValue::Integer(1)]),
            SettingKind::StringArray
        ));
        // Empty table is an empty array; well-formed arrays pass.
        assert!(lua_value_matches_kind(
            &LuaValue::Table(Vec::new()),
            SettingKind::StringArray
        ));
        assert!(lua_value_matches_kind(
            &LuaValue::array(vec![LuaValue::String("a".to_string())]),
            SettingKind::StringArray
        ));
    }

    #[test]
    fn malformed_file_falls_back_wholesale_with_diagnostic() {
        let dir =
            std::env::temp_dir().join(format!("bitty-plugin-settings-{}", std::process::id()));
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(
            plugins.join("xuepoo.broken.toml"),
            b"[capabilities]\nui.rich = true\n",
        )
        .expect("fixture");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.broken", &bar_decls(), None);
        // Every key serves the shipped default; the failure is recorded.
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_file_absent_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-absent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.gone", &bar_decls(), None);
        assert_eq!(
            loaded.source.get("edge"),
            Some(LuaValue::String("bottom".to_string()))
        );
        assert!(loaded.file_error.is_none());
        assert!(loaded.dropped.is_empty());
        // No config dir at all: same total-default outcome.
        let loaded = load_plugin_settings(None, "xuepoo.gone", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_applies_well_formed_file() {
        let dir =
            std::env::temp_dir().join(format!("bitty-plugin-settings-good-{}", std::process::id()));
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(
            plugins.join("bitty-terminal.bar.toml"),
            b"show = false\nmodules = [\"clock\"]\n",
        )
        .expect("fixture");
        let loaded = load_plugin_settings(Some(&dir), "bitty-terminal.bar", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(false)));
        assert_eq!(
            loaded.source.get("modules"),
            Some(LuaValue::array(vec![LuaValue::String("clock".to_string())]))
        );
        assert!(loaded.file_error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lua_canonical_wins_over_toml_and_alias() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-precedence-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(plugins.join("xuepoo.bar.toml"), b"show = true\n").expect("toml");
        std::fs::write(plugins.join("bar.lua"), b"return { show = false }\n").expect("alias");
        std::fs::write(
            plugins.join("xuepoo.bar.lua"),
            b"return { show = true, edge = 'top' }\n",
        )
        .expect("canonical");
        let ids = vec!["xuepoo.bar".to_string()];
        let loaded = load_plugin_settings_with_plugins(
            Some(&dir),
            "xuepoo.bar",
            &bar_decls(),
            None,
            Some(&ids),
        );
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert_eq!(
            loaded.source.get("edge"),
            Some(LuaValue::String("top".to_string()))
        );
        assert!(loaded.file_error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_alias_applies_and_collisions_disable_it() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-alias-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(plugins.join("bar.lua"), b"return { show = false }\n").expect("alias");
        let unique = vec!["xuepoo.bar".to_string()];
        let loaded = load_plugin_settings_with_plugins(
            Some(&dir),
            "xuepoo.bar",
            &bar_decls(),
            None,
            Some(&unique),
        );
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(false)));
        let collision = vec!["xuepoo.bar".to_string(), "other.bar".to_string()];
        let loaded = load_plugin_settings_with_plugins(
            Some(&dir),
            "xuepoo.bar",
            &bar_decls(),
            None,
            Some(&collision),
        );
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_lua_does_not_fall_through_to_toml() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-invalid-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(plugins.join("xuepoo.bar.lua"), b"return io\n").expect("lua");
        std::fs::write(plugins.join("xuepoo.bar.toml"), b"show = false\n").expect("toml");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.bar", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lua_declared_keys_and_types_are_strictly_filtered() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-types-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(
            plugins.join("xuepoo.bar.lua"),
            b"return { show = 'bad', edge = 'top', name_max_chars = 3.5, hidden = true }\n",
        )
        .expect("lua");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.bar", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert_eq!(
            loaded.source.get("edge"),
            Some(LuaValue::String("top".to_string()))
        );
        assert_eq!(
            loaded.source.get("name_max_chars"),
            Some(LuaValue::Integer(32))
        );
        assert!(loaded.dropped.iter().any(|key| key == "hidden"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_plugin_id_never_constructs_a_config_path() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-path-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(plugins.join("escape.lua"), b"return { show = false }\n").expect("lua");
        let loaded = load_plugin_settings(Some(&dir), "../escape", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn config_loader_rejects_symlink_directory_and_fifo() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-file-types-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");

        let target = plugins.join("target.lua");
        std::fs::write(&target, b"return { show = false }\n").expect("target");
        std::os::unix::fs::symlink(&target, plugins.join("xuepoo.bar.lua")).expect("symlink");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.bar", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_some());

        std::fs::remove_file(plugins.join("xuepoo.bar.lua")).expect("remove symlink");
        std::fs::create_dir(plugins.join("xuepoo.bar.lua")).expect("directory");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.bar", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_some());

        std::fs::remove_dir(plugins.join("xuepoo.bar.lua")).expect("remove directory");

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let fifo = plugins.join("xuepoo.bar.lua");
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &fifo,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .expect("fifo");
            let loaded = load_plugin_settings(Some(&dir), "xuepoo.bar", &bar_decls(), None);
            assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
            assert!(loaded.file_error.is_some());
            std::fs::remove_file(&fifo).expect("remove fifo");
        }

        std::os::unix::fs::symlink(&target, plugins.join("xuepoo.bar.toml")).expect("toml symlink");
        let loaded = load_plugin_settings(Some(&dir), "xuepoo.bar", &bar_decls(), None);
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        assert!(loaded.file_error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn aliases_fail_closed_without_a_complete_unique_registry() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-plugin-settings-lua-alias-registry-{}",
            std::process::id()
        ));
        let plugins = dir.join("plugins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&plugins).expect("temp dir");
        std::fs::write(plugins.join("bar.lua"), b"return { show = false }\n").expect("alias");

        for ids in [None, Some(&[][..]), Some(&["other.bar".to_string()][..])] {
            let loaded = load_plugin_settings_with_plugins(
                Some(&dir),
                "xuepoo.bar",
                &bar_decls(),
                None,
                ids,
            );
            assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
            assert!(loaded.file_error.is_none());
        }
        let invalid = ["xuepoo.bar".to_string(), "bad/id".to_string()];
        let loaded = load_plugin_settings_with_plugins(
            Some(&dir),
            "xuepoo.bar",
            &bar_decls(),
            None,
            Some(&invalid),
        );
        assert_eq!(loaded.source.get("show"), Some(LuaValue::Bool(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
