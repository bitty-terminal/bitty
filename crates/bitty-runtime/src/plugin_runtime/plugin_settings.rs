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
use std::path::Path;

use bitty_config::plugin_file::{self, ParsedPluginFile, PluginFileValue};
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
    let path = match (config_dir, plugin_file::valid_plugin_file_id(plugin_id)) {
        // Validated ids match `owner.name` with no separators, so joining
        // the file name cannot escape the `plugins/` directory (fail-closed
        // `None` otherwise).
        (Some(dir), true) => Some(
            dir.join(plugin_file::PLUGINS_DIR_NAME)
                .join(format!("{plugin_id}.toml")),
        ),
        _ => None,
    };
    let Some(path) = path else {
        let resolved = resolve_plugin_settings(declared, None, base);
        return LoadedPluginSettings {
            source: PluginSettingsSource::new(resolved.values),
            dropped: resolved.dropped,
            file_error: None,
        };
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let resolved = resolve_plugin_settings(declared, None, base);
            return LoadedPluginSettings {
                source: PluginSettingsSource::new(resolved.values),
                dropped: resolved.dropped,
                file_error: None,
            };
        }
        Err(error) => {
            let resolved = resolve_plugin_settings(declared, None, base);
            return LoadedPluginSettings {
                source: PluginSettingsSource::new(resolved.values),
                dropped: resolved.dropped,
                file_error: Some(format!("unreadable plugin config: {error}")),
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
}
