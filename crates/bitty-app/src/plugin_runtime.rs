//! Startup wiring for the plugin host runtime (RFC Gap A).
//!
//! Discovers bundled plugin packages from configuration-derived roots, then
//! activates each in its own `!Send` VM on this thread. Policy and mechanism
//! live in `bitty-runtime::plugin_runtime`; this module only resolves
//! environment/XDG paths, supplies the committed-snapshot source, and reports
//! the outcome. `--safe` skips every third-party VM.
//!
//! Source resolution here is the minimal bundled-root scan; the XDG package
//! store, integrity re-verification, and local-path flow arrive with the Gap B
//! task. Paths come from the environment (or `$HOME`), never literals.

use std::path::PathBuf;
use std::rc::Rc;

use bitty_runtime::plugin_runtime::{
    EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};

/// Environment override for the bundled plugin package root.
pub(crate) const BUNDLED_ROOT_ENV: &str = "BITTY_BUNDLED_PLUGIN_DIR";

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

/// Discover roots from `$BITTY_BUNDLED_PLUGIN_DIR` or the XDG data directory.
fn bundled_roots() -> Vec<PathBuf> {
    if let Ok(explicit) = std::env::var(BUNDLED_ROOT_ENV) {
        let path = PathBuf::from(explicit);
        return if path.is_dir() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    data_home()
        .map(|base| base.join("bitty").join("plugins"))
        .filter(|path| path.is_dir())
        .into_iter()
        .collect()
}

/// `$XDG_DATA_HOME` or `$HOME/.local/share`.
fn data_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".local").join("share"))
}

/// Discover and activate bundled plugins, returning the live runtime.
///
/// Returns `None` when no bundled root exists (no plugin work is attempted).
pub(crate) fn discover_and_activate(
    safe_mode: bool,
    cols: usize,
    rows: usize,
) -> Option<PluginRuntime> {
    let roots = bundled_roots();
    if roots.is_empty() {
        return None;
    }
    let data_dir = data_home().map(|base| base.join("bitty").join("plugins-state"));
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir,
        bundled_roots: roots,
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(CommittedSnapshot::new(cols, rows)),
    });
    let _ = runtime.discover();
    for (id, result) in runtime.activate_discovered() {
        match result {
            Ok(report) if report.skipped_safe_mode => {
                eprintln!("bitty: plugin '{id}' skipped (--safe, no third-party VM)");
            }
            Ok(report) => {
                eprintln!(
                    "bitty: plugin '{id}' active ({} commands, {} events)",
                    report.commands, report.events
                );
            }
            Err(error) => eprintln!("bitty: plugin '{id}' failed: {error}"),
        }
    }
    Some(runtime)
}
