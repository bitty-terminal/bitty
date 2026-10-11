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

use bitty_runtime::plugin_runtime::{
    EmptySettings, LuaValue, PanelPinHandler, PanelPinQueue, PluginRuntime, PluginRuntimeConfig,
    QueuedPinRequest, SnapshotSource, WorkspaceAttention, WorkspaceInfo, WorkspaceSource,
};
use bitty_runtime::{ViewId, WorkspaceSummary};

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
    /// Live pin-target snapshot (CTX-1115).
    pub(crate) pin_state: Rc<LivePinState>,
}

/// Live pin-target snapshot for `bitty.panel.set_pinned` (CTX-1115).
///
/// Same publish pattern as [`LiveWorkspaces`]: the tick loop publishes the
/// active-workspace leaf ids plus the window-global pinned ids once per
/// tick, and the deferred pin backend validates intents against the last
/// committed copy, so unknown ids fail closed synchronously without the
/// closure ever borrowing the live runtime. Pin targets resolve against the
/// active layout, exactly like the `toggle_pinned` keymap path (which pins
/// the focused pane); a leaf living on an inactive workspace is not pinnable
/// until its workspace is active. The drain revalidates against live state,
/// so a panel that vanished between publish and drain still fails closed
/// with state untouched.
#[derive(Default)]
pub(crate) struct LivePinState {
    leaves: RefCell<Vec<ViewId>>,
    pinned: RefCell<Vec<ViewId>>,
}

impl LivePinState {
    /// Commit the given leaf and pinned ids (already bounded by Core).
    pub(crate) fn publish(&self, leaves: &[ViewId], pinned: &[ViewId]) {
        let mut current_leaves = self.leaves.borrow_mut();
        if current_leaves.as_slice() != leaves {
            *current_leaves = leaves.to_vec();
        }
        let mut current_pinned = self.pinned.borrow_mut();
        if current_pinned.as_slice() != pinned {
            *current_pinned = pinned.to_vec();
        }
    }

    /// Whether the last published active layout holds `id` as a live leaf.
    pub(crate) fn is_live_leaf(&self, id: ViewId) -> bool {
        self.leaves.borrow().contains(&id)
    }

    /// Whether the last published state holds `id` as pinned.
    pub(crate) fn is_pinned(&self, id: ViewId) -> bool {
        self.pinned.borrow().contains(&id)
    }
}

/// Deferred `bitty.panel.set_pinned` backend for production startup (CTX-1115).
///
/// Validates each granted call against the last published [`LivePinState`]
/// and enqueues the intent into the runtime-shared [`PanelPinQueue`]; the
/// application drains it each tick through the same `pin_floating` /
/// `unpin_floating` runtime path the `toggle_pinned` keymap action uses.
/// `true` covers a queued transition plus an idempotent no-op; `false` is
/// the fail-closed answer for an unknown target or a full queue, with state
/// untouched. The closure captures only shared handles, never the live
/// runtime, so it stays `'static`-compatible while `TerminalApp` keeps sole
/// `Runtime` ownership (DEC-0107).
pub(crate) fn startup_pin_backend(
    queue: Rc<RefCell<PanelPinQueue>>,
    state: Rc<LivePinState>,
) -> PanelPinHandler {
    Rc::new(move |panel_id, pinned| {
        let id = ViewId::new(panel_id);
        if pinned {
            if state.is_pinned(id) {
                return Ok(true);
            }
            if !state.is_live_leaf(id) {
                return Ok(false);
            }
        } else if !state.is_pinned(id) {
            // Unpin of a live leaf holds without queueing; an unknown
            // target fails closed without touching the queue.
            if !state.is_live_leaf(id) {
                return Ok(false);
            }
            return Ok(true);
        }
        let accepted = queue
            .borrow_mut()
            .push(QueuedPinRequest { panel_id, pinned });
        Ok(accepted)
    })
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

/// Startup-committed state seeded into the plugin host before activation.
///
/// The workspace rows feed [`LiveWorkspaces`] (CTX-0889) while the leaf and
/// pinned ids seed [`LivePinState`] (CTX-1115), so `init.lua` already
/// observes the constructed runtime on every read path.
pub(crate) struct StartupSeed<'a> {
    /// Workspace summaries for the live workspace source.
    pub(crate) workspaces: &'a [WorkspaceSummary],
    /// Active-workspace leaf ids for the pin-target snapshot.
    pub(crate) leaves: &'a [ViewId],
    /// Window-global pinned ids for the pin-target snapshot.
    pub(crate) pinned: &'a [ViewId],
}

/// Discover and activate installed and development plugins, returning the
/// live runtime plus the shared live-snapshot (CTX-0481), live-workspace
/// (CTX-0889), and live-pin-state (CTX-1115) handles.
///
/// Returns `None` when neither the XDG store nor a development root exists (no
/// plugin work is attempted). The caller feeds the handles to the app tick
/// loop so `bitty.terminal.snapshot` reflects committed state, workspace
/// reads serve published rows, and `bitty.panel.set_pinned` validates plus
/// applies through the tick drain.
pub(crate) fn discover_and_activate(
    safe_mode: bool,
    cols: usize,
    rows: usize,
    seed: &StartupSeed<'_>,
) -> Option<PluginSession> {
    discover_with_roots(safe_mode, cols, rows, seed, store_root(), dev_roots())
}

/// Env-free startup wiring behind [`discover_and_activate`] (CTX-1115 test seam).
///
/// Production resolves both roots from the environment; tests pass explicit
/// roots so the real backend installation plus activation path runs without
/// process-global environment mutation (which `#![forbid(unsafe_code)]`
/// rules out).
pub(crate) fn discover_with_roots(
    safe_mode: bool,
    cols: usize,
    rows: usize,
    seed: &StartupSeed<'_>,
    store_root: Option<PathBuf>,
    dev_roots: Vec<PathBuf>,
) -> Option<PluginSession> {
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
    workspaces.publish(seed.workspaces);
    runtime.set_workspace_source(workspaces.clone());
    // CTX-1115: seed the pin snapshot and install the deferred pin backend
    // before activation so `init.lua` already observes a working
    // `set_pinned`. Activation captures the backend into `panel.focus`
    // generations only; the call-time grant gate stays as the second layer.
    let pin_state = Rc::new(LivePinState::default());
    pin_state.publish(seed.leaves, seed.pinned);
    runtime.set_panel_pin_backend(Some(startup_pin_backend(
        Rc::clone(runtime.panel_pin_queue()),
        Rc::clone(&pin_state),
    )));
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
        pin_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_runtime::Runtime;
    use bitty_runtime::{LayoutNode, SplitAxis, UiRect, View};

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

    const PIN_PROBE_COMMANDS: &[&str] = &["pin", "unpin", "pin_unknown"];

    const PIN_PROBE: &str = r#"
local function call(f, ...)
  local ok, result = pcall(f, ...)
  if ok then return "OK:" .. tostring(result) end
  return "ERR:" .. result.code
end

bitty.commands.register({ id = "pin", title = "Pin", run = function()
  return call(bitty.panel.set_pinned, 1, true) end })
bitty.commands.register({ id = "unpin", title = "Unpin", run = function()
  return call(bitty.panel.set_pinned, 1, false) end })
bitty.commands.register({ id = "pin_unknown", title = "Pin unknown", run = function()
  return call(bitty.panel.set_pinned, 404, true) end })
"#;

    fn pin_test_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-startup-pin-{tag}-{}-{counter}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_pin_probe(root: &Path, id: &str, capabilities: &[&str]) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
        let qualified: Vec<String> = PIN_PROBE_COMMANDS
            .iter()
            .map(|command| format!("\"{id}:{command}\""))
            .collect();
        let capabilities_toml = capabilities
            .iter()
            .map(|capability| format!("{capability} = true\n"))
            .collect::<String>();
        std::fs::write(
            plugin.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\n{capabilities_toml}[lazy]\ncommands = [{}]\nevents = []\n",
                qualified.join(", "),
            ),
        )
        .expect("manifest");
        std::fs::write(plugin.join("lua/init.lua"), PIN_PROBE).expect("init");
    }

    fn two_leaf_runtime() -> Runtime {
        let mut live = Runtime::with_defaults().expect("defaults must build");
        live.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ));
        live.set_container(UiRect::new(0, 0, 80, 24));
        live
    }

    fn pin_test_app(
        tag: &str,
        id: &str,
        capabilities: &[&str],
    ) -> crate::terminal_app::TerminalApp {
        let root = pin_test_dir(tag);
        write_pin_probe(&root, id, capabilities);
        let live = two_leaf_runtime();
        let summaries = live.workspace_summaries();
        let leaves = live.layout().leaf_ids();
        let pinned = live.pinned_views();
        let seed = StartupSeed {
            workspaces: &summaries,
            leaves: &leaves,
            pinned: &pinned,
        };
        let session = discover_with_roots(false, 80, 24, &seed, None, vec![root])
            .expect("dev root must yield a session");
        let pin_state = Rc::clone(&session.pin_state);
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        crate::terminal_app::TerminalApp::with_theme(
            live,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            crate::spawn::SpawnSpec::default(),
        )
        .with_plugin_runtime(Some(session.runtime))
        .with_live_pin_state(Some(pin_state))
    }

    fn dispatch_pin(app: &mut crate::terminal_app::TerminalApp, id: &str, command: &str) -> String {
        use bitty_plugin_host::manifest::PluginId;
        let pid = PluginId::new(id).expect("id");
        match app
            .plugin_runtime
            .as_mut()
            .expect("plugin runtime")
            .dispatch_command(&pid, command, &[])
        {
            Ok(LuaValue::String(value)) => value,
            Ok(other) => panic!("{command}: expected a string result, got {other:?}"),
            Err(error) => panic!("{command}: dispatch failed: {error:?}"),
        }
    }

    /// Run the pin half of the app tick without a full `drive_tick`.
    ///
    /// A full tick also drains the process-global control queue with empty
    /// scopes, which would poison the concurrent CTX-0220 socket tests, so
    /// these tests drive the pin apply plus snapshot publish directly in
    /// tick order (apply before publish, mirroring `drive_tick`).
    fn drain_pin_tick(app: &mut crate::terminal_app::TerminalApp) {
        app.apply_plugin_pin_requests();
        assert!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .panel_pin_queue()
                .borrow()
                .is_empty(),
            "the drain must consume every queued intent"
        );
        let pin_state = app.live_pin_state.clone().expect("pin state");
        pin_state.publish(
            &app.runtime.layout().leaf_ids(),
            &app.runtime.pinned_views(),
        );
    }

    #[test]
    fn startup_pin_backend_applies_through_lua_verb_on_tick() {
        // CTX-1115 prod path: startup installs the deferred backend before
        // activation, so a granted `bitty.panel.set_pinned` from Lua queues
        // and the tick drain applies it through the live pin runtime path.
        let mut app = pin_test_app("granted", "xuepoo.prodpin", &["panel.focus"]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpin", "pin"),
            "OK:true",
            "granted pin queues"
        );
        assert!(
            app.runtime.pinned_views().is_empty(),
            "nothing applies before the tick drain"
        );
        drain_pin_tick(&mut app);
        assert_eq!(app.runtime.pinned_views(), vec![ViewId::new(1)]);
        // The tick republished the snapshot, so a re-pin holds without
        // queueing and an unknown id still fails closed synchronously.
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpin", "pin"),
            "OK:true",
            "re-pin holds"
        );
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpin", "pin_unknown"),
            "OK:false",
            "unknown id fails closed"
        );
        drain_pin_tick(&mut app);
        assert_eq!(app.runtime.pinned_views(), vec![ViewId::new(1)]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpin", "unpin"),
            "OK:true",
            "granted unpin queues"
        );
        drain_pin_tick(&mut app);
        assert!(app.runtime.pinned_views().is_empty());
        assert_eq!(
            app.runtime.layout().leaf_ids(),
            vec![ViewId::new(1), ViewId::new(2)],
            "unpin restores the floating panel to the live layout"
        );
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpin", "unpin"),
            "OK:true",
            "unpin of a live leaf holds"
        );
    }

    #[test]
    fn startup_pin_backend_denied_without_panel_focus_grant() {
        // Grant gating stays: without `panel.focus` the generation captures
        // no backend and the call-time gate denies first, so the live
        // runtime is never touched.
        let mut app = pin_test_app("denied", "xuepoo.prodindenied", &[]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodindenied", "pin"),
            "ERR:E_CAPABILITY_DENIED"
        );
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodindenied", "unpin"),
            "ERR:E_CAPABILITY_DENIED"
        );
        drain_pin_tick(&mut app);
        assert!(app.runtime.pinned_views().is_empty());
        assert_eq!(
            app.runtime.layout().leaf_ids(),
            vec![ViewId::new(1), ViewId::new(2)]
        );
    }

    #[test]
    fn live_pin_state_publishes_leaves_and_pinned() {
        let state = LivePinState::default();
        assert!(!state.is_live_leaf(ViewId::new(1)));
        let live = two_leaf_runtime();
        state.publish(&live.layout().leaf_ids(), &live.pinned_views());
        assert!(state.is_live_leaf(ViewId::new(1)));
        assert!(state.is_live_leaf(ViewId::new(2)));
        assert!(!state.is_live_leaf(ViewId::new(404)));
        assert!(!state.is_pinned(ViewId::new(1)));
    }
}
