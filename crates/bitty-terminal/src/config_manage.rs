//! Application backend for the `config.manage` read and management surface
//! (CTX-1647, bitty#2393).
//!
//! The runtime owns grants, bounds, approval tokens, and safe-mode denial.
//! This module owns the durable side:
//!
//! - `config.get` / `config.sources` read the same strict merged
//!   `bitty-config` plan `config check` uses and return logical source labels
//!   only (never a host path);
//! - `config.preview_enabled` / `config.set_enabled` render and atomically
//!   replace the managed plugin ledger through the same `bitty plugin`
//!   writer (`0600`, rolling `.bak`, re-parsed before commit);
//! - `profiles.preview_use` / `profiles.use` validate the prospective
//!   selector exactly like `bitty config profile use` and commit through the
//!   same atomic profile writer;
//! - `config.preview` / `config.set` delegate to the user-layer writer, so the
//!   original scalar write path keeps one implementation.

use std::path::PathBuf;

use bitty_config::{ConfigSource, LayerKind, MergedConfig};
use bitty_runtime::plugin_runtime::writable_setting;
use bitty_runtime::plugin_runtime::{
    CONFIG_LIST_MAX_ROWS, ConfigApplyOutcome, ConfigChange, ConfigPreviewRendering,
    ConfigReadFailure, ConfigReadValue, ConfigSourceLabel, ConfigSourceList, ConfigSourceRow,
    ConfigValueRow, ConfigWriteBackend, ConfigWriteFailure, PluginEnabledRendering, PluginList,
    PluginListRow, ProfileList, ProfileListRow, ProfileUseRendering, candidate_digest,
};

use crate::config_cli::ConfigValue;
use crate::init::render_selector_lua;
use crate::plugin::{self, PluginFailure};
use crate::profile::{
    PROFILE_SCAN_CAP, SchemaField, UserLayerConfigWriter, bounded_line_diff,
    effective_profile_name, resolved_profile_name, rewrite_selector_profile, scan_names,
    schema_field_entry, selector_profile_from_path, validate_prospective_selector_for_write,
};

/// One app-side backend implementing every `config.manage` method.
pub(crate) struct AppConfigManage {
    /// Resolved user-layer selector (`init.lua` or the `config.lua` alias).
    target: PathBuf,
    /// `profiles/` beside the selector.
    profiles_dir: PathBuf,
    /// Managed plugin ledger beside the selector.
    ledger_path: PathBuf,
    /// Profile startup actually loaded, when one was selected.
    active_profile: Option<String>,
    /// Installed plugin store root, if any.
    store_root: Option<PathBuf>,
    /// The scalar user-layer writer reused for `config.preview`/`config.set`.
    writer: UserLayerConfigWriter,
}

impl AppConfigManage {
    /// Build the backend for one resolved user-layer target.
    ///
    /// Returns `None` when the target is not a direct child of `config_dir`:
    /// an explicit `--config` outside the user root must never become a
    /// plugin write target.
    pub(crate) fn new(
        config_dir: PathBuf,
        target: PathBuf,
        active_profile: Option<String>,
        store_root: Option<PathBuf>,
    ) -> Option<Self> {
        let writer =
            UserLayerConfigWriter::new(config_dir.clone(), target.clone(), active_profile.clone())?;
        let profiles_dir = config_dir.join(bitty_config::file::PROFILES_DIR_NAME);
        let ledger_path = config_dir.join(bitty_config::plugin_ledger::LEDGER_FILE_NAME);
        Some(Self {
            target,
            profiles_dir,
            ledger_path,
            active_profile,
            store_root,
            writer,
        })
    }

    /// Load the strict merged configuration the running app would use.
    fn load_merged(&self) -> Result<MergedConfig, ConfigReadFailure> {
        let user = if self.target.exists() {
            Some(
                bitty_config::file::load_user_layer(&self.target)
                    .map_err(|_| ConfigReadFailure::Invalid)?,
            )
        } else {
            None
        };
        // Startup precedence: `--profile`/`BITTY_PROFILE` wins over the
        // persisted selector. Reads must reflect the profile the running app
        // actually loaded, so both sides go through the one shared rule.
        let selected = resolved_profile_name(user.as_ref(), self.active_profile.as_deref());
        let chain = match selected.as_deref() {
            Some(name) => bitty_config::file::load_profile_chain_in_dir(name, &self.profiles_dir)
                .map_err(|_| ConfigReadFailure::Invalid)?,
            None => Vec::new(),
        };
        bitty_config::file::resolve_effective_with_profiles(
            user,
            chain,
            &bitty_config::file::CliOverrides::default(),
        )
        .map_err(|_| ConfigReadFailure::Invalid)
    }

    /// Logical source label for one effective key.
    fn source_label(merged: &MergedConfig, key: &str) -> ConfigSourceLabel {
        match merged.source_of(key).map(|source| source.layer) {
            Some(LayerKind::User | LayerKind::Profile | LayerKind::TrustedLocal) => {
                ConfigSourceLabel::User
            }
            // `load_merged` resolves with empty `CliOverrides`, so no `Cli`
            // layer exists to attribute; the arm keeps the match exhaustive
            // without advertising an `env` label the backend can never emit.
            Some(
                LayerKind::CoreDefaults
                | LayerKind::SystemDefaults
                | LayerKind::SystemPolicy
                | LayerKind::Distribution
                | LayerKind::Cli,
            )
            | None => ConfigSourceLabel::Default,
        }
    }

    /// Read the bounded selector bytes, refusing symlinks and special files.
    fn read_selector(&self) -> Result<Option<String>, ConfigWriteFailure> {
        match std::fs::symlink_metadata(&self.target) {
            Ok(meta) if meta.file_type().is_symlink() => Err(ConfigWriteFailure::Unsafe),
            Ok(meta) if !meta.is_file() => Err(ConfigWriteFailure::Unsafe),
            Ok(meta) => {
                if meta.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
                    return Err(ConfigWriteFailure::Invalid);
                }
                std::fs::read_to_string(&self.target)
                    .map(Some)
                    .map_err(|_| ConfigWriteFailure::Io)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(ConfigWriteFailure::Io),
        }
    }

    /// Render the exact prospective selector for one profile switch.
    fn prepare_profile_use(&self, name: &str) -> Result<PreparedProfileUse, ConfigWriteFailure> {
        bitty_config::file::validate_profile_name(name).map_err(|_| ConfigWriteFailure::Invalid)?;
        let profile = self.profiles_dir.join(format!("{name}.lua"));
        match std::fs::symlink_metadata(&profile) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Err(ConfigWriteFailure::Unsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigWriteFailure::Invalid);
            }
            Err(_) => return Err(ConfigWriteFailure::Io),
        }
        bitty_config::file::load_profile_chain_in_dir(name, &self.profiles_dir)
            .map_err(|_| ConfigWriteFailure::Invalid)?;

        let previous = self.read_selector()?;
        let already_active = selector_profile_from_path(&self.target).as_deref() == Some(name);
        let changed = !already_active;
        let content = if already_active {
            previous.clone().unwrap_or_default()
        } else if let Some(current) = previous.as_deref() {
            let source =
                ConfigSource::new(LayerKind::User, Some(self.target.display().to_string()));
            let layer = bitty_config::file::parse_lua_config(current, &source)
                .map_err(|_| ConfigWriteFailure::Invalid)?;
            if layer.selector.is_none() {
                return Err(ConfigWriteFailure::Invalid);
            }
            let rewritten =
                rewrite_selector_profile(current, name).ok_or(ConfigWriteFailure::Invalid)?;
            let rewritten_layer = bitty_config::file::parse_lua_config(&rewritten, &source)
                .map_err(|_| ConfigWriteFailure::Invalid)?;
            match rewritten_layer.selector {
                Some(found) if found.profile == name => {}
                _ => return Err(ConfigWriteFailure::Invalid),
            }
            validate_prospective_selector_for_write(
                &self.profiles_dir,
                name,
                &self.target,
                &rewritten,
            )?;
            rewritten
        } else {
            let rendered = render_selector_lua(name);
            validate_prospective_selector_for_write(
                &self.profiles_dir,
                name,
                &self.target,
                &rendered,
            )?;
            rendered
        };
        let rendered = bounded_line_diff(previous.as_deref().unwrap_or(""), &content)?;
        let digest = candidate_digest(content.as_bytes());
        Ok(PreparedProfileUse {
            content,
            previous_exists: previous.is_some(),
            rendered,
            candidate_digest: digest,
            changed,
        })
    }

    /// Render the exact prospective ledger for one enable/disable.
    fn prepare_plugin_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<PreparedPluginEnabled, ConfigWriteFailure> {
        let state = plugin::load_state_for_config_manage(&self.ledger_path)
            .map_err(plugin_write_failure)?;
        let current = state.render();
        let mut candidate = state;
        let changed = plugin::set_managed_enabled(&mut candidate, id, enabled)
            .map_err(plugin_write_failure)?;
        let rendered_bytes = candidate.render();
        let rendered = bounded_line_diff(&current, &rendered_bytes)?;
        Ok(PreparedPluginEnabled {
            state: candidate,
            rendered,
            candidate_digest: candidate_digest(rendered_bytes.as_bytes()),
            changed,
        })
    }
}

/// One prepared profile switch (no write performed).
struct PreparedProfileUse {
    content: String,
    previous_exists: bool,
    rendered: String,
    candidate_digest: String,
    changed: bool,
}

/// One prepared ledger change (no write performed).
struct PreparedPluginEnabled {
    state: plugin::PluginState,
    rendered: String,
    candidate_digest: String,
    changed: bool,
}

impl ConfigWriteBackend for AppConfigManage {
    fn preview(
        &self,
        plugin_id: &str,
        change: &ConfigChange,
    ) -> Result<ConfigPreviewRendering, ConfigWriteFailure> {
        self.writer.preview(plugin_id, change)
    }

    fn apply(
        &self,
        plugin_id: &str,
        change: &ConfigChange,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        self.writer
            .apply(plugin_id, change, expected_candidate_digest)
    }

    fn config_get(&self, _plugin_id: &str, key: &str) -> Result<ConfigValueRow, ConfigReadFailure> {
        let setting = writable_setting(key).ok_or(ConfigReadFailure::Invalid)?;
        let field = schema_field_entry(setting.key).ok_or(ConfigReadFailure::Invalid)?;
        let merged = self.load_merged()?;
        let value = read_value(field, &merged);
        Ok(ConfigValueRow {
            key: setting.key.to_string(),
            value,
            source: Self::source_label(&merged, setting.key),
            redacted: false,
        })
    }

    fn config_sources(&self, _plugin_id: &str) -> Result<ConfigSourceList, ConfigReadFailure> {
        let merged = self.load_merged()?;
        let mut rows = Vec::with_capacity(32);
        for setting in bitty_runtime::plugin_runtime::CONFIG_WRITABLE_SETTINGS {
            if schema_field_entry(setting.key).is_none() {
                return Err(ConfigReadFailure::Invalid);
            }
            rows.push(ConfigSourceRow {
                key: setting.key.to_string(),
                source: Self::source_label(&merged, setting.key),
            });
        }
        Ok(ConfigSourceList {
            rows,
            truncated: false,
        })
    }

    fn plugins_list(&self, _plugin_id: &str) -> Result<PluginList, ConfigReadFailure> {
        let state =
            plugin::load_state_for_config_manage(&self.ledger_path).map_err(plugin_read_failure)?;
        let mut rows = plugin::rows_for_config_manage(&state, self.store_root.as_deref())
            .map_err(plugin_read_failure)?;
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        let truncated = rows.len() > CONFIG_LIST_MAX_ROWS;
        rows.truncate(CONFIG_LIST_MAX_ROWS);
        Ok(PluginList {
            rows: rows
                .into_iter()
                .map(|row| PluginListRow {
                    id: row.id,
                    enabled: row.enabled,
                    managed: row.recorded,
                    source: row.source,
                    version: row.version,
                })
                .collect(),
            truncated,
        })
    }

    fn profiles_list(&self, _plugin_id: &str) -> Result<ProfileList, ConfigReadFailure> {
        let (names, truncated) =
            scan_names(&self.profiles_dir, PROFILE_SCAN_CAP, CONFIG_LIST_MAX_ROWS)
                .map_err(|_| ConfigReadFailure::Io)?;
        // The running chain wins: `--profile`/`BITTY_PROFILE` first, then the
        // persisted selector. A successful `profiles.use` updates the
        // selector immediately, but the running table is not re-merged, so
        // the caller learns from `pending_restart` that `active` still names
        // the startup profile until the next launch.
        let active = effective_profile_name(
            self.active_profile.as_deref(),
            selector_profile_from_path(&self.target),
        );
        Ok(ProfileList {
            rows: names
                .into_iter()
                .map(|name| ProfileListRow {
                    active: active.as_deref() == Some(name.as_str()),
                    name,
                })
                .collect(),
            active,
            truncated,
        })
    }

    fn preview_plugin_enabled(
        &self,
        _plugin_id: &str,
        id: &str,
        enabled: bool,
    ) -> Result<PluginEnabledRendering, ConfigWriteFailure> {
        let prepared = self.prepare_plugin_enabled(id, enabled)?;
        Ok(PluginEnabledRendering {
            changed_keys: if prepared.changed {
                vec![format!("plugins.{id}.enabled")]
            } else {
                Vec::new()
            },
            rendered: prepared.rendered,
            candidate_digest: prepared.candidate_digest,
        })
    }

    fn apply_plugin_enabled(
        &self,
        plugin_id: &str,
        id: &str,
        enabled: bool,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        let prepared = self.prepare_plugin_enabled(id, enabled)?;
        if prepared.candidate_digest != expected_candidate_digest {
            log_refusal(plugin_id, "plugins.set_enabled", ConfigWriteFailure::Stale);
            return Err(ConfigWriteFailure::Stale);
        }
        if !prepared.changed {
            return Ok(ConfigApplyOutcome::Unchanged);
        }
        plugin::save_state_for_config_manage(&self.ledger_path, &prepared.state).map_err(
            |failure| {
                let mapped = plugin_write_failure(failure);
                log_refusal(plugin_id, "plugins.set_enabled", mapped);
                mapped
            },
        )?;
        Ok(ConfigApplyOutcome::Applied)
    }

    fn preview_profile_use(
        &self,
        _plugin_id: &str,
        name: &str,
    ) -> Result<ProfileUseRendering, ConfigWriteFailure> {
        let prepared = self.prepare_profile_use(name)?;
        Ok(ProfileUseRendering {
            changed_keys: if prepared.changed {
                vec![String::from("profile")]
            } else {
                Vec::new()
            },
            rendered: prepared.rendered,
            candidate_digest: prepared.candidate_digest,
            changed: prepared.changed,
        })
    }

    fn apply_profile_use(
        &self,
        plugin_id: &str,
        name: &str,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        let prepared = self.prepare_profile_use(name)?;
        if prepared.candidate_digest != expected_candidate_digest {
            log_refusal(plugin_id, "profiles.use", ConfigWriteFailure::Stale);
            return Err(ConfigWriteFailure::Stale);
        }
        if !prepared.changed {
            return Ok(ConfigApplyOutcome::Unchanged);
        }
        crate::init::write_init_config(&self.target, &prepared.content, prepared.previous_exists)
            .map_err(|error| {
            let mapped = match error {
                crate::init::InitWriteError::Refused(_) => ConfigWriteFailure::Invalid,
                crate::init::InitWriteError::Io(_) => ConfigWriteFailure::Io,
            };
            log_refusal(plugin_id, "profiles.use", mapped);
            mapped
        })?;
        Ok(ConfigApplyOutcome::Applied)
    }
}

/// Map one `bitty-config` projected value onto the wire scalar.
fn read_value(field: &SchemaField, merged: &MergedConfig) -> ConfigReadValue {
    match (field.kind, (field.get)(&merged.effective)) {
        (crate::profile::FieldKind::Text, ConfigValue::Text(text)) => ConfigReadValue::Text(text),
        (crate::profile::FieldKind::Integer, ConfigValue::Number(number)) => number
            .parse::<u32>()
            .map_or(ConfigReadValue::Absent, ConfigReadValue::Integer),
        (crate::profile::FieldKind::Number, ConfigValue::Number(number)) => number
            .parse::<f32>()
            .map_or(ConfigReadValue::Absent, ConfigReadValue::Number),
        (crate::profile::FieldKind::Bool, ConfigValue::Bool(value)) => ConfigReadValue::Bool(value),
        _ => ConfigReadValue::Absent,
    }
}

/// Map one plugin-state failure onto the path-free write class.
fn plugin_write_failure(failure: PluginFailure) -> ConfigWriteFailure {
    match failure.code {
        "UnsafePath" => ConfigWriteFailure::Unsafe,
        "IoError" => ConfigWriteFailure::Io,
        _ => ConfigWriteFailure::Invalid,
    }
}

/// Map one plugin-state failure onto the path-free read class.
fn plugin_read_failure(failure: PluginFailure) -> ConfigReadFailure {
    match failure.code {
        "UnsafePath" => ConfigReadFailure::Unsafe,
        "IoError" => ConfigReadFailure::Io,
        _ => ConfigReadFailure::Invalid,
    }
}

/// Log a refusal with plugin attribution; the plugin sees only the class.
fn log_refusal(plugin_id: &str, verb: &str, failure: ConfigWriteFailure) {
    crate::logging::warn(|| {
        format!("bitty: config.manage {verb} by '{plugin_id}' refused ({failure:?})")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-config-manage-app-{tag}-{}-{counter}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn backend(dir: &Path, active: Option<&str>) -> AppConfigManage {
        AppConfigManage::new(
            dir.to_path_buf(),
            dir.join("init.lua"),
            active.map(str::to_string),
            None,
        )
        .expect("backend")
    }

    #[test]
    fn config_get_reads_the_strict_merged_plan_with_logical_sources() {
        let dir = scratch("get");
        std::fs::write(
            dir.join("init.lua"),
            "return { format = \"bitty-profile-v1\", profile = \"work\", window = { opacity = 0.9 } }\n",
        )
        .expect("selector");
        std::fs::create_dir_all(dir.join("profiles")).expect("profiles");
        std::fs::write(
            dir.join("profiles/work.lua"),
            "return { appearance = { theme = \"dracula\" } }\n",
        )
        .expect("profile");
        let backend = backend(&dir, Some("work"));

        let theme = backend.config_get("p", "appearance.theme").expect("theme");
        assert_eq!(theme.value, ConfigReadValue::Text(String::from("dracula")));
        assert_eq!(theme.source, ConfigSourceLabel::User);

        let opacity = backend.config_get("p", "window.opacity").expect("opacity");
        assert_eq!(opacity.value, ConfigReadValue::Number(0.9));
        assert_eq!(opacity.source, ConfigSourceLabel::User);

        let padding = backend
            .config_get("p", "decoration.gaps_in")
            .expect("decoration");
        assert_eq!(padding.source, ConfigSourceLabel::Default);

        let sources = backend.config_sources("p").expect("sources");
        assert!(
            sources
                .rows
                .iter()
                .all(|row| row.key != "history.capture.transcript")
        );
        assert!(
            sources
                .rows
                .iter()
                .any(|row| row.key == "appearance.theme" && row.source == ConfigSourceLabel::User)
        );
        // `history.capture.*` is deliberately outside the writable allowlist,
        // so the read path refuses it too (defense in depth behind the
        // runtime's own key gate).
        assert_eq!(
            backend
                .config_get("p", "history.capture.transcript")
                .expect_err("excluded key"),
            ConfigReadFailure::Invalid
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn merged_reads_prefer_the_startup_profile_over_the_selector() {
        let dir = scratch("precedence");
        std::fs::write(
            dir.join("init.lua"),
            "return { format = \"bitty-profile-v1\", profile = \"default\" }\n",
        )
        .expect("selector");
        std::fs::create_dir_all(dir.join("profiles")).expect("profiles");
        std::fs::write(
            dir.join("profiles/default.lua"),
            "return { appearance = { theme = \"catppuccin\" } }\n",
        )
        .expect("default");
        std::fs::write(
            dir.join("profiles/work.lua"),
            "return { appearance = { theme = \"dracula\" } }\n",
        )
        .expect("work");
        // `--profile work` won at startup; reads must not fall back to the
        // persisted selector's `default`.
        let backend = backend(&dir, Some("work"));
        let theme = backend.config_get("p", "appearance.theme").expect("theme");
        assert_eq!(theme.value, ConfigReadValue::Text(String::from("dracula")));
        let listed = backend.profiles_list("p").expect("profiles");
        assert_eq!(
            listed.active.as_deref(),
            Some("work"),
            "the list reports the running chain, not the selector"
        );
        assert!(
            listed
                .rows
                .iter()
                .any(|row| row.name == "work" && row.active)
        );
        assert!(
            listed
                .rows
                .iter()
                .any(|row| row.name == "default" && !row.active)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_enable_preview_binds_exact_bytes_and_writes_a_backup() {
        let dir = scratch("enabled");
        let backend = backend(&dir, None);
        // A ledger with one disabled bundled record.
        let manifest = bitty_plugin_host::bundled::all_bundled_manifests()
            .into_iter()
            .next()
            .expect("at least one bundled plugin");
        let mut state = plugin::PluginState::new();
        state.insert(
            manifest.id().as_str().to_string(),
            plugin::PluginRecord {
                source: String::from("bundled"),
                manifest_hash: manifest.manifest_hash(),
                enabled: false,
                granted: manifest.capabilities.all_ids().expect("declared"),
                denied: std::collections::BTreeSet::new(),
            },
        );
        plugin::save_state_for_config_manage(&dir.join("bitty-plugins.lua"), &state).expect("seed");
        let id = manifest.id().as_str().to_string();

        let preview = backend
            .preview_plugin_enabled("p", &id, true)
            .expect("preview");
        assert!(
            preview
                .changed_keys
                .iter()
                .any(|key| key.ends_with(".enabled"))
        );
        assert!(!preview.rendered.is_empty());

        let outcome = backend
            .apply_plugin_enabled("p", &id, true, &preview.candidate_digest)
            .expect("apply");
        assert_eq!(outcome, ConfigApplyOutcome::Applied);
        let saved = std::fs::read_to_string(dir.join("bitty-plugins.lua")).expect("ledger");
        assert!(saved.contains("enabled = true"));
        assert!(dir.join("bitty-plugins.lua.bak").exists());

        // Replaying the same token after the change re-renders the same
        // candidate and reports a no-op; the runtime's one-shot gate is what
        // prevents the replay from reaching here twice.
        let replay = backend
            .apply_plugin_enabled("p", &id, true, &preview.candidate_digest)
            .expect("unchanged");
        assert_eq!(replay, ConfigApplyOutcome::Unchanged);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_enable_rechecks_the_manifest_pin_and_grants() {
        let dir = scratch("stale");
        let backend = backend(&dir, None);
        let manifest = bitty_plugin_host::bundled::all_bundled_manifests()
            .into_iter()
            .next()
            .expect("at least one bundled plugin");
        let id = manifest.id().as_str().to_string();
        let mut state = plugin::PluginState::new();
        state.insert(
            id.clone(),
            plugin::PluginRecord {
                source: String::from("bundled"),
                manifest_hash: "00".repeat(32),
                enabled: false,
                granted: std::collections::BTreeSet::new(),
                denied: std::collections::BTreeSet::new(),
            },
        );
        plugin::save_state_for_config_manage(&dir.join("bitty-plugins.lua"), &state).expect("seed");
        assert_eq!(
            backend
                .preview_plugin_enabled("p", &id, true)
                .expect_err("stale pin"),
            ConfigWriteFailure::Invalid
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn profile_use_preview_and_apply_rewrite_only_the_selector() {
        let dir = scratch("profile");
        std::fs::write(
            dir.join("init.lua"),
            "-- keep\nreturn { format = \"bitty-profile-v1\", profile = \"default\", -- tail\n window = { opacity = 0.9 } }\n",
        )
        .expect("selector");
        std::fs::create_dir_all(dir.join("profiles")).expect("profiles");
        std::fs::write(dir.join("profiles/default.lua"), "return {}\n").expect("default");
        std::fs::write(
            dir.join("profiles/work.lua"),
            "return { window = { opacity = 0.8 } }\n",
        )
        .expect("work");
        let backend = backend(&dir, Some("default"));

        let preview = backend.preview_profile_use("p", "work").expect("preview");
        assert!(preview.changed);
        assert!(preview.rendered.contains("work"));
        let outcome = backend
            .apply_profile_use("p", "work", &preview.candidate_digest)
            .expect("apply");
        assert_eq!(outcome, ConfigApplyOutcome::Applied);
        let selector = std::fs::read_to_string(dir.join("init.lua")).expect("selector");
        assert!(selector.contains("profile = \"work\""));
        assert!(selector.contains("-- keep"));
        assert!(selector.contains("window = { opacity = 0.9 }"));
        let listed = backend.profiles_list("p").expect("profiles");
        assert_eq!(
            listed.active.as_deref(),
            Some("default"),
            "the running table stays on the startup profile until restart"
        );
        assert!(
            listed
                .rows
                .iter()
                .any(|row| row.name == "default" && row.active)
        );
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak"))
            .collect();
        assert!(!backups.is_empty(), "the previous selector is backed up");

        // A moved selector between preview and apply refuses as stale.
        let preview = backend
            .preview_profile_use("p", "default")
            .expect("preview");
        std::fs::write(
            dir.join("init.lua"),
            "return { format = \"bitty-profile-v1\", profile = \"work\", window = { opacity = 0.5 } }\n",
        )
        .expect("concurrent edit");
        let error = backend
            .apply_profile_use("p", "default", &preview.candidate_digest)
            .expect_err("stale");
        assert_eq!(error, ConfigWriteFailure::Stale);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reserved_default_is_only_the_existing_init_profile() {
        let dir = scratch("default");
        let backend = backend(&dir, None);
        // No `profiles/default.lua`: the name is not a magic reset.
        assert_eq!(
            backend
                .preview_profile_use("p", "default")
                .expect_err("missing"),
            ConfigWriteFailure::Invalid
        );
        std::fs::create_dir_all(dir.join("profiles")).expect("profiles");
        std::fs::write(dir.join("profiles/default.lua"), "return {}\n").expect("default");
        let preview = backend
            .preview_profile_use("p", "default")
            .expect("preview");
        assert!(preview.changed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_list_is_bounded_and_never_carries_grants() {
        let dir = scratch("list");
        let backend = backend(&dir, None);
        let list = backend.plugins_list("p").expect("list");
        assert!(list.rows.len() <= CONFIG_LIST_MAX_ROWS);
        // The row type has no grant field at all; this pins the projection.
        for row in &list.rows {
            assert!(!row.id.is_empty());
            assert!(row.source.len() <= 128);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_list_marks_catalog_only_rows_as_unmanaged() {
        let dir = scratch("list-unmanaged");
        let backend = backend(&dir, None);
        let list = backend.plugins_list("p").expect("list");
        assert!(!list.rows.is_empty(), "the bundled catalog is listed");
        for row in &list.rows {
            assert!(!row.managed, "no managed ledger record exists");
            assert!(
                !row.enabled,
                "a catalog-only row is never reported as an enabled plugin"
            );
        }
        // Toggling a catalog-only row is refused, which is exactly why the
        // row must not look like a recorded disabled plugin.
        let id = list.rows[0].id.clone();
        assert_eq!(
            backend
                .preview_plugin_enabled("p", &id, true)
                .expect_err("unrecorded row"),
            ConfigWriteFailure::Invalid
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_list_marks_a_recorded_row_as_managed() {
        let dir = scratch("list-managed");
        let backend = backend(&dir, None);
        let manifest = bitty_plugin_host::bundled::all_bundled_manifests()
            .into_iter()
            .next()
            .expect("at least one bundled plugin");
        let id = manifest.id().as_str().to_string();
        let mut state = plugin::PluginState::new();
        state.insert(
            id.clone(),
            plugin::PluginRecord {
                source: String::from("bundled"),
                manifest_hash: manifest.manifest_hash(),
                enabled: true,
                granted: manifest.capabilities.all_ids().expect("declared"),
                denied: std::collections::BTreeSet::new(),
            },
        );
        plugin::save_state_for_config_manage(&dir.join("bitty-plugins.lua"), &state).expect("seed");
        let list = backend.plugins_list("p").expect("list");
        let row = list
            .rows
            .iter()
            .find(|row| row.id == id)
            .expect("recorded row");
        assert!(row.managed, "the ledger records this id");
        assert!(row.enabled);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn profile_use_creates_a_fresh_selector_owner_only() {
        let dir = scratch("profile-fresh");
        std::fs::create_dir_all(dir.join("profiles")).expect("profiles");
        std::fs::write(dir.join("profiles/default.lua"), "return {}\n").expect("default");
        std::fs::write(dir.join("profiles/work.lua"), "return {}\n").expect("work");
        let backend = backend(&dir, None);
        let preview = backend.preview_profile_use("p", "work").expect("preview");
        let outcome = backend
            .apply_profile_use("p", "work", &preview.candidate_digest)
            .expect("apply");
        assert_eq!(outcome, ConfigApplyOutcome::Applied);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join("init.lua"))
                .expect("selector")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "a fresh selector is owner-only, got {mode:o}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn profile_use_reports_an_io_failure_from_the_atomic_writer() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("profile-io");
        std::fs::write(
            dir.join("init.lua"),
            "return { format = \"bitty-profile-v1\", profile = \"default\" }\n",
        )
        .expect("selector");
        std::fs::create_dir_all(dir.join("profiles")).expect("profiles");
        std::fs::write(dir.join("profiles/default.lua"), "return {}\n").expect("default");
        std::fs::write(dir.join("profiles/work.lua"), "return {}\n").expect("work");
        let backend = backend(&dir, Some("default"));
        let preview = backend.preview_profile_use("p", "work").expect("preview");
        // A read-only config root lets the preview read the selector but the
        // atomic replace cannot stage its recovery copy; the writer reports
        // the path-free Io class (root bypasses this, like the other
        // permission-based tests in this suite).
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
            .expect("read-only root");
        let error = backend
            .apply_profile_use("p", "work", &preview.candidate_digest)
            .expect_err("atomic write fails");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore root");
        assert_eq!(error, ConfigWriteFailure::Io);
        let selector = std::fs::read_to_string(dir.join("init.lua")).expect("selector");
        assert!(
            selector.contains("profile = \"default\""),
            "a failed write leaves the selector intact"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
