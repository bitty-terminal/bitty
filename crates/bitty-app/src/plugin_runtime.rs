//! Startup wiring for the plugin host runtime (RFC Gap A + Gap B).
//!
//! Discovers plugin packages from configuration-derived roots, then activates
//! each in its own `!Send` VM on this thread. Policy and mechanism live in
//! `bitty-runtime::plugin_runtime`; this module only resolves environment/XDG
//! paths, supplies the committed-snapshot source, and reports the outcome.
//!
//! Installed packages are resolved from the atomic XDG store pointer
//! (`$XDG_DATA_HOME/bitty/plugins/current.json`) and re-verified fail-closed;
//! their provenance class comes from the resolved record, never a self-declared
//! id. The `BITTY_PLUGIN_DIR` override is an untrusted local-path development
//! root. Provenance, not the manifest id, decides `--safe` eligibility, so
//! `--safe` creates no VM for any non-bundled package. Paths come from the
//! environment (or `$HOME`), never literals.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_runtime::plugin_runtime::{
    EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};

/// Environment override for a local-path development plugin root.
///
/// The path is not application-shipped, so packages found here are
/// `local-path`: read-only, re-digested, visibly unverified, and skipped by
/// `--safe`.
pub(crate) const PLUGIN_ROOT_ENV: &str = "BITTY_PLUGIN_DIR";

/// Read-only snapshot of the committed core surface for `bitty.terminal.snapshot`.
struct CommittedSnapshot {
    cols: usize,
    rows: usize,
}

impl CommittedSnapshot {
    fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl SnapshotSource for CommittedSnapshot {
    fn snapshot(
        &self,
        scope: &str,
    ) -> Result<LuaValue, bitty_runtime::plugin_runtime::BridgeError> {
        if scope != "semantic" {
            return Err(bitty_runtime::plugin_runtime::BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "only the semantic scope is supported",
            ));
        }
        Ok(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
            ("runtime_id", LuaValue::Integer(1)),
            ("generation", LuaValue::Integer(1)),
            ("snapshot_generation", LuaValue::Integer(1)),
            ("width", LuaValue::Integer(self.cols as i64)),
            ("height", LuaValue::Integer(self.rows as i64)),
            ("rows", LuaValue::array(Vec::new())),
            (
                "cursor",
                LuaValue::table([
                    ("row", LuaValue::Integer(0)),
                    ("col", LuaValue::Integer(0)),
                    ("visible", LuaValue::Bool(true)),
                ]),
            ),
            (
                "modes",
                LuaValue::table([("alternate_screen", LuaValue::Bool(false))]),
            ),
            ("title", LuaValue::String(String::new())),
            ("zones", LuaValue::array(Vec::new())),
        ]))
    }
}

/// Resolved XDG plugin store root (`$XDG_DATA_HOME/bitty/plugins`).
fn store_root() -> Option<PathBuf> {
    store_root_for(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Resolved XDG plugin store root from explicit environment values.
///
/// Shared by startup discovery and the `bitty plugin` package-manager CLI so
/// both resolve the exact same store without reading process environment
/// inside the CLI (hermetic tests pass the values in).
pub(crate) fn store_root_for(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    data_home_for(xdg_data_home, home).map(|base| base.join("bitty").join("plugins"))
}

/// `$XDG_DATA_HOME` or `$HOME/.local/share` from explicit environment values.
pub(crate) fn data_home_for(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_data_home {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    home.filter(|home| !home.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".local").join("share"))
}

/// `$XDG_DATA_HOME` or `$HOME/.local/share`.
fn data_home() -> Option<PathBuf> {
    data_home_for(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Untrusted local-path development roots from `$BITTY_PLUGIN_DIR`.
fn dev_roots() -> Vec<PathBuf> {
    let Ok(explicit) = std::env::var(PLUGIN_ROOT_ENV) else {
        return Vec::new();
    };
    let path = PathBuf::from(explicit);
    if path.is_dir() {
        vec![path]
    } else {
        Vec::new()
    }
}

/// Discover and activate installed and development plugins, returning the live
/// runtime.
///
/// Returns `None` when neither the XDG store nor a development root exists (no
/// plugin work is attempted).
pub(crate) fn discover_and_activate(
    safe_mode: bool,
    cols: usize,
    rows: usize,
) -> Option<PluginRuntime> {
    let store_root = store_root();
    let dev_roots = dev_roots();
    let store_present = store_root.as_deref().is_some_and(Path::is_dir);
    if !store_present && dev_roots.is_empty() {
        return None;
    }
    let data_dir = data_home().map(|base| base.join("bitty").join("plugins-state"));
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir,
        store_root,
        // No application-shipped bundle root exists in this slice; genuine
        // first-party catalogs arrive with the packaging follow-up.
        bundled_roots: Vec::new(),
        third_party_roots: dev_roots,
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(CommittedSnapshot::new(cols, rows)),
    });
    let _ = runtime.discover();
    for (id, result) in runtime.activate_discovered() {
        match result {
            Ok(report) if report.skipped_safe_mode => {
                crate::logging::info(|| {
                    format!(
                        "bitty: plugin '{id}' skipped (--safe, {} source, no VM)",
                        report.source_class.as_str()
                    )
                });
            }
            Ok(report) => {
                let verification = if report.unverified {
                    "unverified"
                } else {
                    "verified"
                };
                crate::logging::info(|| {
                    format!(
                        "bitty: plugin '{id}' active ({} commands, {} events, {} source, {verification})",
                        report.commands,
                        report.events,
                        report.source_class.as_str(),
                    )
                });
            }
            Err(error) => crate::logging::warn(|| format!("bitty: plugin '{id}' failed: {error}")),
        }
    }
    Some(runtime)
}
