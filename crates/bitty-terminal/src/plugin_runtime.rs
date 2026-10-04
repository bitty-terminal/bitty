//! Startup wiring for the plugin host runtime (RFC Gap A + Gap B).
//!
//! Discovers plugin packages from configuration-derived roots, then activates
//! each in its own `!Send` VM on this thread. Policy and mechanism live in
//! `bitty-runtime::plugin_runtime`; this module only resolves environment/XDG
//! paths, supplies the live committed-snapshot source, and reports the
//! outcome.
//!
//! Installed packages are resolved from the atomic XDG store pointer
//! (`$XDG_DATA_HOME/bitty/plugins/current.json`) and re-verified fail-closed;
//! their provenance class comes from the resolved record, never a self-declared
//! id. The `BITTY_PLUGIN_DIR` override is an untrusted local-path development
//! root. Provenance, not the manifest id, decides `--safe` eligibility, so
//! `--safe` creates no VM for any non-bundled package. Paths come from the
//! environment (or `$HOME`), never literals.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_runtime::WorkspaceSummary;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    WorkspaceAttention, WorkspaceInfo, WorkspaceSource,
};

/// Environment override for a local-path development plugin root.
///
/// The path is not application-shipped, so packages found here are
/// `local-path`: read-only, re-digested, visibly unverified, and skipped by
/// `--safe`.
pub(crate) const PLUGIN_ROOT_ENV: &str = "BITTY_PLUGIN_DIR";

/// Committed snapshot values published from the live runtime (CTX-0481).
///
/// Small and `Copy`-free on purpose: the tick loop rebuilds it from cheap
/// `State` reads (generation, cursor, title) and commits it atomically, so
/// plugins never observe torn or frozen data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotState {
    pub(crate) generation: u64,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
    pub(crate) cursor_visible: bool,
    pub(crate) alternate_screen: bool,
    pub(crate) title: String,
}

/// Live committed-snapshot source for `bitty.terminal.snapshot` (CTX-0481).
///
/// The pre-0481 source froze generation 1 for the process lifetime; this
/// source advances with the runtime's committed generation (the same value
/// the IPC snapshot service publishes) and refuses to regress. Owned by the
/// app tick loop through [`Rc`] and shared with the plugin host services,
/// which run on the same thread.
pub(crate) struct LiveSnapshot {
    state: RefCell<SnapshotState>,
}

impl LiveSnapshot {
    /// Committed state before the first publish: generation 0, initial
    /// geometry, empty title.
    pub(crate) fn new(cols: usize, rows: usize) -> Self {
        Self {
            state: RefCell::new(SnapshotState {
                generation: 0,
                width: cols,
                height: rows,
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                alternate_screen: false,
                title: String::new(),
            }),
        }
    }

    /// Commit the runtime's current committed state; returns whether the
    /// generation advanced (or the very first publish committed).
    ///
    /// Monotonic: a candidate whose generation is older than the committed
    /// one is refused without touching the stored state (fail closed).
    pub(crate) fn publish(&self, runtime: &bitty_runtime::Runtime) -> bool {
        let state = runtime.state();
        let cursor = state.cursor();
        let next = SnapshotState {
            generation: state.generation(),
            // Live grid dims (the config keeps the startup geometry; a
            // window resize reflows the state, so read the state).
            width: state.width(),
            height: state.height(),
            cursor_row: usize::from(cursor.position.row),
            cursor_col: usize::from(cursor.position.col),
            cursor_visible: cursor.visible,
            alternate_screen: state.alt_screen_active(),
            title: state.title().to_string(),
        };
        self.publish_state(next)
    }

    /// Pure commit core (tests + [`Self::publish`]).
    ///
    /// Returns `false` (and leaves the committed state untouched) when
    /// `next.generation` is older than the committed generation.
    pub(crate) fn publish_state(&self, next: SnapshotState) -> bool {
        let mut current = self.state.borrow_mut();
        if next.generation < current.generation {
            return false;
        }
        let advanced = next.generation > current.generation;
        *current = next;
        advanced
    }

    /// Bounded Lua view of the committed state (`semantic` scope only).
    fn to_lua(&self) -> LuaValue {
        let state = self.state.borrow();
        LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
            ("runtime_id", LuaValue::Integer(1)),
            (
                "generation",
                LuaValue::Integer(state.generation.min(i64::MAX as u64) as i64),
            ),
            (
                "snapshot_generation",
                LuaValue::Integer(state.generation.min(i64::MAX as u64) as i64),
            ),
            (
                "width",
                LuaValue::Integer(state.width.min(i64::MAX as usize) as i64),
            ),
            (
                "height",
                LuaValue::Integer(state.height.min(i64::MAX as usize) as i64),
            ),
            ("rows", LuaValue::array(Vec::new())),
            (
                "cursor",
                LuaValue::table([
                    (
                        "row",
                        LuaValue::Integer(state.cursor_row.min(i64::MAX as usize) as i64),
                    ),
                    (
                        "col",
                        LuaValue::Integer(state.cursor_col.min(i64::MAX as usize) as i64),
                    ),
                    ("visible", LuaValue::Bool(state.cursor_visible)),
                ]),
            ),
            (
                "modes",
                LuaValue::table([("alternate_screen", LuaValue::Bool(state.alternate_screen))]),
            ),
            ("title", LuaValue::String(state.title.clone())),
            ("zones", LuaValue::array(Vec::new())),
        ])
    }
}

impl SnapshotSource for LiveSnapshot {
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
        Ok(self.to_lua())
    }
}

/// Live workspace source for `bitty.workspace.list()` (CTX-0889, ADR-0014).
///
/// Same pattern as [`LiveSnapshot`]: the tick loop publishes Core's
/// plain-data [`WorkspaceSummary`] rows once per tick and plugin reads
/// serve the last committed copy, so Lua never reaches the runtime and
/// never sees terminal content. Bounded by Core's `MAX_WORKSPACES`.
#[derive(Default)]
pub(crate) struct LiveWorkspaces {
    rows: RefCell<Vec<WorkspaceSummary>>,
}

impl LiveWorkspaces {
    /// Commit the given summaries (already bounded by Core).
    pub(crate) fn publish(&self, rows: &[WorkspaceSummary]) {
        let mut current = self.rows.borrow_mut();
        if current.as_slice() != rows {
            *current = rows.to_vec();
        }
    }
}

impl WorkspaceSource for LiveWorkspaces {
    /// Attention is all-`false`: Core has no per-workspace bell, activity,
    /// or exit source yet (tracked per session), so no flag is fabricated.
    ///
    /// CTX-0954: scratchpad occupancy rides the same row under the same
    /// `workspace.read` grant (window-global, identical on every row); no
    /// panel capability is consulted.
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, bitty_runtime::plugin_runtime::BridgeError> {
        Ok(self
            .rows
            .borrow()
            .iter()
            .map(|row| WorkspaceInfo {
                id: row.seq,
                name: row.name.clone(),
                active: row.active,
                panel_count: row.panel_ids.len(),
                scratchpad_count: row.scratchpad_count,
                scratchpad_occupied: row.scratchpad_occupied,
                attention: WorkspaceAttention::default(),
            })
            .collect())
    }
}

/// Handles the startup wiring hands to the app tick loop.
pub(crate) struct PluginSession {
    /// Plugin runtime owning every VM.
    pub(crate) runtime: PluginRuntime,
    /// Live committed-snapshot source (CTX-0481).
    pub(crate) snapshot: Rc<LiveSnapshot>,
    /// Live workspace source (CTX-0889).
    pub(crate) workspaces: Rc<LiveWorkspaces>,
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
///
/// Delegates to [`bitty_runtime::data_home_for`], the single resolver shared
/// with the native component root (DIR-030).
pub(crate) fn data_home_for(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    bitty_runtime::data_home_for(xdg_data_home, home)
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

/// Discover and activate installed and development plugins, returning the
/// live runtime plus the shared live-snapshot (CTX-0481) and live-workspace
/// (CTX-0889) handles.
///
/// Returns `None` when neither the XDG store nor a development root exists (no
/// plugin work is attempted). The caller feeds the handle to the app tick
/// loop so `bitty.terminal.snapshot` reflects committed state.
pub(crate) fn discover_and_activate(
    safe_mode: bool,
    cols: usize,
    rows: usize,
    initial_workspaces: &[WorkspaceSummary],
) -> Option<PluginSession> {
    let store_root = store_root();
    let dev_roots = dev_roots();
    let store_present = store_root.as_deref().is_some_and(Path::is_dir);
    if !store_present && dev_roots.is_empty() {
        return None;
    }
    let data_dir = data_home().map(|base| base.join("bitty").join("plugins-state"));
    let snapshot = Rc::new(LiveSnapshot::new(cols, rows));
    let host_snapshot: Rc<dyn SnapshotSource> = snapshot.clone();
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir,
        store_root,
        // No application-shipped bundle root exists in this slice; genuine
        // first-party catalogs arrive with the packaging follow-up.
        bundled_roots: Vec::new(),
        third_party_roots: dev_roots,
        settings: Rc::new(EmptySettings),
        snapshot: host_snapshot,
    });
    // CTX-0939 (W-146): disk-backed plugin stores commit through the
    // extracted storage mechanics. Core keeps quotas, validation, and the
    // RC-1 accounting; only the atomic-commit bytes flow through here.
    runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    // CTX-0889: seed the workspace source before activation so `init.lua`
    // already observes the startup workspaces.
    let workspaces = Rc::new(LiveWorkspaces::default());
    workspaces.publish(initial_workspaces);
    runtime.set_workspace_source(workspaces.clone());
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
    Some(PluginSession {
        runtime,
        snapshot,
        workspaces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_runtime::Runtime;

    fn generation_of(snapshot: &LuaValue) -> u64 {
        match snapshot.get("generation") {
            Some(LuaValue::Integer(value)) => u64::try_from(*value).expect("non-negative"),
            other => panic!("snapshot generation must be an integer, got {other:?}"),
        }
    }

    fn integer_of(snapshot: &LuaValue, key: &str) -> u64 {
        match snapshot.get(key) {
            Some(LuaValue::Integer(value)) => u64::try_from(*value).expect("non-negative"),
            other => panic!("snapshot {key} must be an integer, got {other:?}"),
        }
    }

    #[test]
    fn live_snapshot_advances_generation_from_runtime_state() {
        // CTX-0481 (#762): the old `CommittedSnapshot` froze generation at 1
        // for the process lifetime, so plugins never observed committed
        // state advancing. The live source tracks the committed runtime
        // generation.
        let snapshot = LiveSnapshot::new(80, 24);
        let before = generation_of(&snapshot.snapshot("semantic").expect("scope ok"));
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.handle_pty_bytes(b"ctx-0481-live-snapshot\r\n");
        let _ = rt.tick();
        let live = rt.state().generation();
        assert!(
            live > before,
            "feeding bytes must advance the runtime generation ({live} vs {before})"
        );
        assert!(snapshot.publish(&rt), "publish must commit the new state");
        let after = snapshot.snapshot("semantic").expect("scope ok");
        assert_eq!(
            generation_of(&after),
            live,
            "snapshot must carry the generation"
        );
        assert_eq!(integer_of(&after, "width"), 80);
        assert_eq!(integer_of(&after, "height"), 24);
        assert_eq!(
            integer_of(&after, "snapshot_generation"),
            live,
            "snapshot_generation must track the same committed generation"
        );
    }

    #[test]
    fn live_workspaces_serve_published_rows_without_content() {
        // CTX-0889: rows carry identity/order/panel count only; attention
        // stays all-false because Core has no per-workspace source.
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.workspace_new().expect("second workspace");
        rt.workspace_rename(0, "editor").expect("rename");
        let source = LiveWorkspaces::default();
        assert!(source.workspaces().expect("rows").is_empty());
        source.publish(&rt.workspace_summaries());
        let rows = source.workspaces().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].name, "editor");
        assert!(!rows[0].active);
        assert_eq!(rows[1].name, "ws2");
        assert!(rows[1].active);
        assert!(rows.iter().all(|row| row.panel_count == 1));
        assert!(
            rows.iter()
                .all(|row| row.attention == WorkspaceAttention::default())
        );
    }

    #[test]
    fn live_workspaces_bounded_at_capacity_with_long_names() {
        let mut rt = Runtime::with_defaults().expect("must build");
        while rt.workspace_new().is_ok() {}
        assert_eq!(rt.workspace_count(), bitty_runtime::MAX_WORKSPACES);
        let long = "x".repeat(4 * bitty_runtime::WORKSPACE_NAME_MAX_CHARS);
        for index in 0..rt.workspace_count() {
            rt.workspace_rename(index, &long).expect("rename");
        }
        let source = LiveWorkspaces::default();
        source.publish(&rt.workspace_summaries());
        let rows = source.workspaces().expect("rows");
        assert_eq!(rows.len(), bitty_runtime::MAX_WORKSPACES);
        assert!(
            rows.iter()
                .all(|row| { row.name.chars().count() == bitty_runtime::WORKSPACE_NAME_MAX_CHARS })
        );
        let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids must be unique");
    }

    #[test]
    fn live_snapshot_refuses_generation_regression() {
        let snapshot = LiveSnapshot::new(80, 24);
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.handle_pty_bytes(b"advance\r\n");
        let _ = rt.tick();
        assert!(snapshot.publish(&rt));
        let committed = generation_of(&snapshot.snapshot("semantic").expect("scope ok"));
        assert!(committed > 0, "runtime generation must have advanced");
        // A stale source (generation went backwards) must fail closed: the
        // committed snapshot stays put instead of regressing.
        let stale = SnapshotState {
            generation: committed - 1,
            width: 10,
            height: 5,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            alternate_screen: true,
            title: String::from("stale"),
        };
        assert!(!snapshot.publish_state(stale));
        let after = snapshot.snapshot("semantic").expect("scope ok");
        assert_eq!(
            generation_of(&after),
            committed,
            "regression must not commit"
        );
        assert_eq!(integer_of(&after, "width"), 80);
    }
}
