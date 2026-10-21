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
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_runtime::plugin_runtime::{
    BridgeError, CoreTargetInfo, EmptySettings, FsListBackend, LuaValue, PanelCreateHandler,
    PanelPinHandler, PanelPinQueue, PluginRuntime, PluginRuntimeConfig, QueuedPinRequest,
    SnapshotSource, TargetSource, TranscriptHistory, WorkspaceAttention, WorkspaceInfo,
    WorkspaceSource,
};
use bitty_runtime::registry::{PanelError, PanelRegistry, PanelRegistryConfig, PanelType};
use bitty_runtime::{TargetAnchorSummary, ViewId, WorkspaceSummary};

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

/// Live Core target/anchor source for `bitty.ui.targets` (issue #2253).
///
/// Same publish pattern as [`LiveWorkspaces`]: the tick loop publishes the
/// runtime's plain-data [`TargetAnchorSummary`] rows once per tick, and a
/// cold-path targeting session reads the last committed copy. Plugin calls
/// never reach the live runtime and never see terminal content. Rows are
/// already bounded by Core (`bitty_ui::MAX_HINT_ANNOTATIONS`).
#[derive(Default)]
pub(crate) struct LiveTargets {
    rows: RefCell<Vec<TargetAnchorSummary>>,
}

impl LiveTargets {
    /// Commit the given summaries (already bounded by Core).
    pub(crate) fn publish(&self, rows: &[TargetAnchorSummary]) {
        let mut current = self.rows.borrow_mut();
        if current.as_slice() != rows {
            *current = rows.to_vec();
        }
    }
}

impl TargetSource for LiveTargets {
    fn targets(&self) -> Result<Vec<CoreTargetInfo>, BridgeError> {
        Ok(self
            .rows
            .borrow()
            .iter()
            .map(|row| CoreTargetInfo {
                kind: row.kind.clone(),
                id: row.id,
                workspace: row.workspace,
                rect: row.rect,
            })
            .collect())
    }
}

/// Sample the active session's completed scrollback into the bounded history
/// ring (CTX-1352).
///
/// Only the runtime-global primary terminal state is sampled, and rows carry
/// the workspace that owns the primary view plus that view id as their scope
/// attribution. Inactive panes, background workspaces, and alternate content
/// are never captured into another scope. Capture-disabled ticks return before
/// building workspace summaries.
pub(crate) fn capture_history(runtime: &bitty_runtime::Runtime, history: &TranscriptHistory) {
    use bitty_runtime::plugin_runtime::HistoryProvider as _;

    if !history.capture_enabled(bitty_plugin_host::HistorySource::Transcript) {
        return;
    }
    let Some(view) = runtime.primary_view() else {
        return;
    };
    let Some(workspace) = runtime
        .workspace_summaries()
        .into_iter()
        .find(|summary| summary.panel_ids.contains(&view.0))
    else {
        return;
    };
    history.capture_state(
        runtime.state(),
        &view.0.to_string(),
        &workspace.seq.to_string(),
    );
}

/// Handles the startup wiring hands to the app tick loop.
pub(crate) struct PluginSession {
    /// Plugin runtime owning every VM.
    pub(crate) runtime: PluginRuntime,
    /// Live committed-snapshot source (CTX-0481).
    pub(crate) snapshot: Rc<LiveSnapshot>,
    /// Live workspace source (CTX-0889).
    pub(crate) workspaces: Rc<LiveWorkspaces>,
    /// Live Core target/anchor source (issue #2253).
    pub(crate) targets: Rc<LiveTargets>,
    /// Live pin-target snapshot (CTX-1115).
    pub(crate) pin_state: Rc<LivePinState>,
    /// Bounded transcript provider published by the tick loop (CTX-1352).
    pub(crate) history: Rc<TranscriptHistory>,
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
/// Only truly unknown targets (neither pinned nor a live leaf) fail closed
/// with `false` and touch nothing; redundant intents for known targets
/// queue normally because the snapshot is last-tick state and the drain
/// treats redundancies as no-ops against live state. A full queue also
/// returns `false`. The closure captures only shared handles, never the live
/// runtime, so it stays `'static`-compatible while `TerminalApp` keeps sole
/// `Runtime` ownership (DEC-0107).
pub(crate) fn startup_pin_backend(
    queue: Rc<RefCell<PanelPinQueue>>,
    state: Rc<LivePinState>,
) -> PanelPinHandler {
    Rc::new(move |panel_id, pinned| {
        let id = ViewId::new(panel_id);
        // Reject only truly unknown targets. A same-tick second intent
        // (pin then unpin, or a repeat) must not be shadowed by snapshot
        // staleness: the drain revalidates each intent against live state,
        // so redundancies land as no-ops in FIFO order there.
        if !state.is_pinned(id) && !state.is_live_leaf(id) {
            return Ok(false);
        }
        let accepted = queue
            .borrow_mut()
            .push(QueuedPinRequest { panel_id, pinned });
        Ok(accepted)
    })
}

/// Synchronous `bitty.panel.create` backend for production startup (CTX-1345).
///
/// Allocates a native (`rich`, `browser`, `helper`, or `canvas`) panel in the
/// process-owned [`PanelRegistry`] and returns the raw `(id, generation)`
/// pair the plugin API shapes into `{ id, generation }`. Mounting and
/// focusing stay host-driven, so the backend carries no layout authority and
/// creates no PTY: `terminal` (process-spawn authority the `panel.create`
/// grant never conveys) and unknown types fail closed with `E_DEF_INVALID`.
/// Registry exhaustion maps to the bounded `E_DEF_LIMIT`; an unservable
/// registry maps to `E_NOT_IMPLEMENTED`, like the sibling panel verbs.
pub(crate) fn startup_panel_create_backend(
    panels: Rc<RefCell<PanelRegistry>>,
) -> PanelCreateHandler {
    Rc::new(move |panel_type| {
        let parsed = PanelType::parse(panel_type).ok_or_else(|| {
            BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!("unknown panel type '{panel_type}'"),
            )
        })?;
        if parsed == PanelType::Terminal {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                "panel.create cannot create terminal panels: plugins create zero-PTY panels only",
            ));
        }
        panels
            .borrow_mut()
            .create_panel(parsed, None)
            .map(|handle| (handle.id.0, handle.generation.get()))
            .map_err(|error| match error {
                PanelError::TooManyPanels { max, .. } => BridgeError::new(
                    "budget",
                    "E_DEF_LIMIT",
                    format!("panel registry full (max {max} panels)"),
                ),
                PanelError::UnknownPanelType { value } => BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    format!("unknown panel type '{value}'"),
                ),
                _ => BridgeError::not_implemented("bitty.panel.create"),
            })
    })
}

/// Resolved XDG plugin store root (`$XDG_DATA_HOME/bitty/plugins`).
///
/// `pub(crate)` for the VM-less `bitty list services` catalog (dynapi
/// observability, bitty#2002), which discovers the same store without
/// loading any plugin VM.
pub(crate) fn store_root() -> Option<PathBuf> {
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

/// Process home used to resolve `~`-rooted `fs.read` grants (CTX-1348).
///
/// `HOME` is authoritative on Unix; `USERPROFILE` is the Windows fallback.
/// An absent, empty, or relative home leaves the backend unwired, so
/// `bitty.fs.list` fails closed with `E_NOT_IMPLEMENTED` rather than guessing
/// a root: an empty or relative home would otherwise resolve `~/docs` against
/// the process working directory.
fn home_dir() -> Option<PathBuf> {
    abs_home_for(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

/// First absolute, non-empty home candidate from explicit environment values.
///
/// Taking the values as arguments keeps the fallback (an empty `HOME` must not
/// mask `USERPROFILE`) testable without mutating the process environment.
fn abs_home_for(home: Option<&OsStr>, userprofile: Option<&OsStr>) -> Option<PathBuf> {
    [home, userprofile]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|path| path.is_absolute())
}

/// Untrusted local-path development roots from `$BITTY_PLUGIN_DIR`.
///
/// `pub(crate)` for the VM-less `bitty list services` catalog (dynapi
/// observability, bitty#2002), which discovers the same roots without
/// loading any plugin VM.
pub(crate) fn dev_roots() -> Vec<PathBuf> {
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
    /// Live Core target/anchor rows for the targeting source.
    pub(crate) targets: &'a [TargetAnchorSummary],
    /// Active-workspace leaf ids for the pin-target snapshot.
    pub(crate) leaves: &'a [ViewId],
    /// Window-global pinned ids for the pin-target snapshot.
    pub(crate) pinned: &'a [ViewId],
    /// Approved wallpaper roots for `bitty.image.read` (CTX-1169).
    pub(crate) image_roots: &'a [String],
    /// Effective opt-in history capture switches (CTX-1352).
    pub(crate) history_capture: bitty_config::HistoryCaptureConfig,
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
    discover_with_roots(
        safe_mode,
        cols,
        rows,
        seed,
        store_root(),
        dev_roots(),
        // CTX-1348: `~`-rooted `fs.read` grants resolve against the
        // environment-derived home. No literal host path is embedded.
        home_dir(),
        // CTX-1195: per-plugin user files resolve under the live
        // environment-derived config root (`$XDG_CONFIG_HOME/bitty`, …) —
        // never a literal home. `None` (no usable root) serves shipped
        // defaults plus the old home.
        bitty_config::file::config_dir(),
    )
}

/// Env-free startup wiring behind [`discover_and_activate`] (CTX-1115 test seam).
///
/// Production resolves the store, development, and filesystem-home roots from
/// the environment; tests pass explicit roots so the real backend installation
/// plus activation path runs without process-global environment mutation
/// (which `#![forbid(unsafe_code)]` rules out).
#[allow(clippy::too_many_arguments)]
pub(crate) fn discover_with_roots(
    safe_mode: bool,
    cols: usize,
    rows: usize,
    seed: &StartupSeed<'_>,
    store_root: Option<PathBuf>,
    dev_roots: Vec<PathBuf>,
    fs_home: Option<PathBuf>,
    settings_dir: Option<PathBuf>,
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
    // CTX-1195 (issue #2010): seed the per-plugin user-config root before
    // activation so every generation resolves its canonical
    // `plugins/<owner>.<name>.lua`, unambiguous short alias, or legacy
    // `plugins/<owner>.<name>.toml` when its services are built (the first
    // present file wins; absent files serve shipped defaults plus the old
    // home).
    runtime.set_settings_dir(settings_dir);
    // CTX-1345: install the production `bitty.panel.create` backend before
    // activation so panel-creating generations capture a real allocator.
    // The registry is process-owned and lives as long as the plugin runtime
    // (the handler owns the only handle); activation still gates the backend
    // on the `panel.create` grant, and the call-time gate stays as the
    // second layer. A registry that cannot be constructed leaves the backend
    // unwired, so granted calls keep failing closed with `E_NOT_IMPLEMENTED`.
    match PanelRegistry::new(PanelRegistryConfig::default()) {
        Ok(registry) => runtime.set_panel_create_backend(Some(startup_panel_create_backend(
            Rc::new(RefCell::new(registry)),
        ))),
        Err(error) => crate::logging::warn(|| {
            format!("bitty: panel registry unavailable, panel.create disabled ({error})")
        }),
    }
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
    // Issue #2253: seed the live Core target/anchor rows before activation so
    // `init.lua` already observes panels and workspaces through
    // `bitty.ui.targets.snapshot`. The tick loop keeps them fresh.
    let targets = Rc::new(LiveTargets::default());
    targets.publish(seed.targets);
    runtime.set_target_source(targets.clone());
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
    // CTX-1352 (issue #2247): install the bounded terminal-state history
    // provider before activation so `init.lua` already observes a present
    // `bitty.history` root. The tick loop samples completed scrollback lines
    // into its redacted ring when `history.capture.transcript` is enabled;
    // with capture off the provider is available but empty. Command history
    // and KV have no backing source in this slice and deny with the typed
    // `E_HISTORY_UNAVAILABLE`, never a nil-index. Installing the provider
    // grants nothing; the per-source capability head and the Core history
    // gate still decide every call.
    let history = Rc::new(TranscriptHistory::new(
        seed.history_capture.transcript,
        seed.history_capture.commands,
    ));
    let host_history: Rc<dyn bitty_runtime::plugin_runtime::HistoryProvider> = history.clone();
    runtime.set_history_provider(host_history);
    // CTX-1169 (issue #1976): seed the approved wallpaper roots before
    // activation so `init.lua` already observes a working
    // `bitty.image.read`. Activation captures the roots into
    // `platform.image-file` generations only; the call-time grant gate stays
    // as the second layer. No theme persist backend is installed in this
    // slice: confirms drain to a warning with state untouched until the
    // config-file write path lands with its consent design.
    runtime.set_image_roots(seed.image_roots.iter().map(PathBuf::from).collect());
    // CTX-1348: wire the real `bitty.fs.list` backend from the environment
    // before activation. The backend resolves only paths covered by each
    // generation's own `fs.read:` grants; no home root becomes ambient.
    if let Some(home) = fs_home {
        runtime.set_fs_backend(Some(Rc::new(FsListBackend::new(home))));
    }
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
                for diagnostic in runtime.settings_diagnostics(&id).unwrap_or_default() {
                    crate::logging::warn(|| format!("bitty: plugin '{id}' settings: {diagnostic}"));
                }
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
        targets,
        pin_state,
        history,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_runtime::Runtime;
    use bitty_runtime::registry::{Generation, PanelId, PanelState};
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

    fn fs_list_probe_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-startup-fs-list-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    /// `home_dir` rejects an unsafe root instead of letting grants resolve
    /// against the process working directory (CTX-1348 review).
    #[test]
    fn absent_empty_or_relative_home_is_rejected_with_fallback() {
        let absolute = std::env::temp_dir();
        assert!(abs_home_for(None, None).is_none());
        assert!(abs_home_for(Some(OsStr::new("")), None).is_none());
        assert!(abs_home_for(Some(OsStr::new("relative-home")), None).is_none());
        assert_eq!(
            abs_home_for(Some(absolute.as_os_str()), None),
            Some(absolute.clone())
        );
        // An empty `HOME` must not mask the `USERPROFILE` fallback.
        assert_eq!(
            abs_home_for(Some(OsStr::new("")), Some(absolute.as_os_str())),
            Some(absolute)
        );
    }

    /// Composition-root wiring for `bitty.fs.list` (CTX-1348): a development
    /// package granted `fs.read:~/docs/**` lists the seeded home through the
    /// production backend installed by [`discover_with_roots`].
    #[test]
    fn discover_wires_bounded_fs_list_backend_from_home() {
        let home = fs_list_probe_root("home");
        std::fs::create_dir_all(home.join("docs")).expect("docs");
        std::fs::write(home.join("docs/a.txt"), "a").expect("a");
        std::fs::write(home.join("docs/b.txt"), "b").expect("b");
        std::fs::create_dir_all(home.join("outside")).expect("outside");
        std::fs::write(home.join("outside/secret.txt"), "s").expect("secret");

        let root = fs_list_probe_root("plugins");
        let id = "xuepoo.fslist";
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
        std::fs::write(
            plugin.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"FS list\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\n\n[[capabilities.filesystem]]\naccess = \"read\"\npaths = [\"~/docs/**\"]\n\n[lazy]\ncommands = [\"{id}:list\"]\nevents = []\n"
            ),
        )
        .expect("manifest");
        std::fs::write(
            plugin.join("lua/init.lua"),
            r#"
            local function list()
              local entries = bitty.fs.list("~/docs")
              local out = {}
              for i, entry in ipairs(entries) do
                out[i] = entry.name .. ":" .. entry.kind .. ":" .. tostring(entry.untrusted)
              end
              return table.concat(out, ",")
            end
            bitty.commands.register({ id = "list", title = "List", run = list })
            "#,
        )
        .expect("init");

        let live = Runtime::with_defaults().expect("defaults");
        let summaries = live.workspace_summaries();
        let leaves = live.layout().leaf_ids();
        let pinned = live.pinned_views();
        let seed = StartupSeed {
            workspaces: &summaries,
            targets: &[],
            leaves: &leaves,
            pinned: &pinned,
            image_roots: &[],
            history_capture: bitty_config::HistoryCaptureConfig::default(),
        };
        let session = discover_with_roots(false, 80, 24, &seed, None, vec![root], Some(home), None)
            .expect("dev root must yield a session");
        let mut runtime = session.runtime;
        let plugin_id = bitty_plugin_host::manifest::PluginId::new(id).expect("id");
        let value = runtime
            .dispatch_command(&plugin_id, "list", &[])
            .expect("dispatch fs.list command");
        assert_eq!(
            value,
            LuaValue::String("~/docs/a.txt:file:true,~/docs/b.txt:file:true".to_string())
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
            targets: &[],
            leaves: &leaves,
            pinned: &pinned,
            image_roots: &[],
            history_capture: bitty_config::HistoryCaptureConfig::default(),
        };
        let session = discover_with_roots(false, 80, 24, &seed, None, vec![root], None, None)
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

    /// Upper bound on cold ticks spent settling one parked dispatch
    /// (bitty #2409).
    ///
    /// Each cold tick advances at most one RC-1 wall window and every callback
    /// is bounded by the 5250 ms ceiling, so a legitimate park cannot need
    /// more windows; the bound only turns a stuck park into a test failure
    /// instead of a hang.
    const PARK_RESUME_TICKS: usize = 200;

    /// One host-mediated dispatch after cold-tick settling (bitty #2409).
    struct SettledDispatch {
        /// The command's string result.
        value: String,
        /// Completed parked resumes observed while settling (`0` or `1`).
        resumes: usize,
    }

    /// Dispatch one probe command and settle an RC-1 park on the cold ticks.
    ///
    /// bitty #2314 made app-dispatched commands resumable at the 50 ms charged
    /// RC-1 wall budget, so a dispatch may legitimately return [`Parked`]
    /// instead of a value when the runner is loaded. A park is accepted work,
    /// not a refusal: this helper drives the documented recovery — the same
    /// `PluginRuntime::pump_parked_commands` step the app performs inside
    /// [`crate::terminal_app::TerminalApp::pump_cold_commands`] — until the
    /// parked invocation completes, then proves the completion happened
    /// exactly once and never replays. The dispatch itself is never retried,
    /// so the callback body still runs exactly once.
    ///
    /// [`Parked`]: bitty_runtime::plugin_runtime::PluginRuntimeError::Parked
    fn settle_dispatch(runtime: &mut PluginRuntime, id: &str, command: &str) -> SettledDispatch {
        use bitty_plugin_host::manifest::PluginId;
        let pid = PluginId::new(id).expect("id");
        match runtime.dispatch_command(&pid, command, &[]) {
            Ok(LuaValue::String(value)) => SettledDispatch { value, resumes: 0 },
            Ok(other) => panic!("{command}: expected a string result, got {other:?}"),
            Err(error) if error.is_parked() => {
                assert_eq!(
                    runtime.parked_command_count(),
                    1,
                    "{command}: the park must be recorded exactly once"
                );
                for _ in 0..PARK_RESUME_TICKS {
                    let mut completions = runtime.pump_parked_commands();
                    if completions.is_empty() {
                        // Still parked: the next cold tick advances the next
                        // RC-1 wall window.
                        continue;
                    }
                    assert_eq!(
                        completions.len(),
                        1,
                        "{command}: one parked invocation completes at most once per cold tick"
                    );
                    let completion = completions.remove(0);
                    assert_eq!(completion.plugin.as_str(), id);
                    assert_eq!(completion.command, command);
                    let value = match completion.outcome {
                        Ok(LuaValue::String(value)) => value,
                        Ok(other) => {
                            panic!("{command}: resumed with a non-string result {other:?}")
                        }
                        Err(error) => panic!("{command}: resume failed: {error:?}"),
                    };
                    assert_eq!(
                        runtime.parked_command_count(),
                        0,
                        "{command}: a completed park must be dropped"
                    );
                    assert!(
                        runtime.pump_parked_commands().is_empty(),
                        "{command}: a completed command must never replay"
                    );
                    return SettledDispatch { value, resumes: 1 };
                }
                panic!(
                    "{command}: the parked invocation did not resume within {PARK_RESUME_TICKS} cold ticks"
                )
            }
            Err(error) => panic!("{command}: dispatch failed: {error:?}"),
        }
    }

    fn dispatch_pin(app: &mut crate::terminal_app::TerminalApp, id: &str, command: &str) -> String {
        let runtime = app.plugin_runtime.as_mut().expect("plugin runtime");
        settle_dispatch(runtime, id, command).value
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
    fn startup_pin_backend_queues_same_tick_reversal_and_rejects_unknown() {
        // CodeRabbit 1928: the snapshot is last-tick state, so a same-tick
        // second intent must queue rather than be shadowed by staleness;
        // only truly unknown targets fail closed with nothing queued.
        let mut app = pin_test_app("reversal", "xuepoo.prodpinrev", &["panel.focus"]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpinrev", "pin"),
            "OK:true",
            "pin queues"
        );
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpinrev", "unpin"),
            "OK:true",
            "same-tick unpin queues instead of holding on stale state"
        );
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodpinrev", "pin_unknown"),
            "OK:false",
            "unknown id fails closed"
        );
        assert_eq!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .panel_pin_queue()
                .borrow()
                .len(),
            2,
            "the unknown intent queues nothing"
        );
        drain_pin_tick(&mut app);
        assert!(
            app.runtime.pinned_views().is_empty(),
            "pin-then-unpin in one tick ends unpinned"
        );
        assert_eq!(
            app.runtime.layout().leaf_ids(),
            vec![ViewId::new(1), ViewId::new(2)],
            "the transient pin leaves no layout trace"
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

    const FX_PROBE_COMMANDS: &[&str] = &["pulse", "pulse_unknown", "pulse_shape"];

    const FX_PROBE: &str = r#"
local function call(f, ...)
  local ok, result = pcall(f, ...)
  if ok then return "OK:" .. tostring(result) end
  return "ERR:" .. result.code
end

bitty.commands.register({ id = "pulse", title = "Pulse", run = function()
  return call(bitty.fx.pulse, 1) end })
bitty.commands.register({ id = "pulse_unknown", title = "Pulse unknown", run = function()
  return call(bitty.fx.pulse, 404) end })
bitty.commands.register({ id = "pulse_shape", title = "Pulse shape", run = function()
  return call(bitty.fx.pulse, "1") end })
"#;

    fn write_fx_probe(root: &Path, id: &str, capabilities: &[&str]) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
        let qualified: Vec<String> = FX_PROBE_COMMANDS
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
        std::fs::write(plugin.join("lua/init.lua"), FX_PROBE).expect("init");
    }

    fn fx_test_app(tag: &str, id: &str, capabilities: &[&str]) -> crate::terminal_app::TerminalApp {
        let root = pin_test_dir(tag);
        write_fx_probe(&root, id, capabilities);
        let live = two_leaf_runtime();
        let summaries = live.workspace_summaries();
        let leaves = live.layout().leaf_ids();
        let pinned = live.pinned_views();
        let seed = StartupSeed {
            workspaces: &summaries,
            targets: &[],
            leaves: &leaves,
            pinned: &pinned,
            image_roots: &[],
            history_capture: bitty_config::HistoryCaptureConfig::default(),
        };
        let session = discover_with_roots(false, 80, 24, &seed, None, vec![root], None, None)
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

    /// Run the fx half of the app tick: the fx drain plus a queue-emptiness
    /// proof, mirroring `drain_pin_tick` without the snapshot publish (the
    /// fx drain validates against live state, not a published snapshot).
    fn drain_fx_tick(app: &mut crate::terminal_app::TerminalApp) {
        app.apply_plugin_fx_requests();
        assert!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .fx_queue()
                .borrow()
                .is_empty(),
            "the drain must consume every queued intent"
        );
    }

    #[test]
    fn startup_fx_pulse_arms_through_lua_verb_on_tick() {
        // CTX-1139 prod path: activation shares the fx queue with
        // `panel.focus` generations, so a granted `bitty.fx.pulse` from Lua
        // queues and the tick drain arms the Core-owned pulse after
        // revalidating the target against the live layout.
        let mut app = fx_test_app("granted", "xuepoo.prodfx", &["panel.focus"]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodfx", "pulse"),
            "OK:true",
            "granted pulse queues"
        );
        assert!(
            app.runtime
                .fx_progress(ViewId::new(1), std::time::Instant::now())
                .is_none(),
            "nothing arms before the tick drain"
        );
        drain_fx_tick(&mut app);
        assert!(
            app.runtime
                .fx_progress(ViewId::new(1), std::time::Instant::now())
                .is_some(),
            "the drain arms the pulse on the live leaf"
        );
        // An unknown id queues (acceptance is queued, not applied) and the
        // drain drops it with state untouched.
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodfx", "pulse_unknown"),
            "OK:true",
            "unknown id still queues"
        );
        drain_fx_tick(&mut app);
        assert!(
            app.runtime
                .fx_progress(ViewId::new(404), std::time::Instant::now())
                .is_none(),
            "a vanished target never arms"
        );
        // Shape errors fail closed before any grant or queue is touched.
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodfx", "pulse_shape"),
            "ERR:E_DEF_INVALID"
        );
    }

    #[test]
    fn startup_fx_pulse_denied_without_panel_focus_grant() {
        // Grant gating stays: without `panel.focus` the generation shares no
        // queue and the call-time gate denies first, so the live runtime is
        // never touched.
        let mut app = fx_test_app("denied", "xuepoo.prodfxdenied", &[]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodfxdenied", "pulse"),
            "ERR:E_CAPABILITY_DENIED"
        );
        drain_fx_tick(&mut app);
        assert!(
            app.runtime
                .fx_progress(ViewId::new(1), std::time::Instant::now())
                .is_none()
        );
        assert_eq!(
            app.runtime.layout().leaf_ids(),
            vec![ViewId::new(1), ViewId::new(2)]
        );
    }

    const SUBMIT_PROBE_COMMANDS: &[&str] = &["go", "big"];

    const SUBMIT_PROBE: &str = r#"
local function call_submit(text)
  local submit = bitty.terminal.submit
  if submit == nil then return "ABSENT" end
  local ok, result = pcall(submit, text)
  if not ok then return "ERR:" .. result.code end
  if result.status == "accepted" then
    return "OK:accepted:" .. tostring(result.bytes)
  end
  return "OK:" .. tostring(result.status) .. ":" .. tostring(result.deny)
end

bitty.commands.register({ id = "go", title = "Go", run = function()
  return call_submit("cargo test") end })
bitty.commands.register({ id = "big", title = "Big", run = function()
  return call_submit(string.rep("q", 70000)) end })
"#;

    fn write_submit_probe(root: &Path, id: &str, capabilities: &[&str]) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
        let qualified: Vec<String> = SUBMIT_PROBE_COMMANDS
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
        std::fs::write(plugin.join("lua/init.lua"), SUBMIT_PROBE).expect("init");
    }

    fn submit_test_app(
        tag: &str,
        id: &str,
        capabilities: &[&str],
    ) -> crate::terminal_app::TerminalApp {
        let root = pin_test_dir(tag);
        write_submit_probe(&root, id, capabilities);
        let live = two_leaf_runtime();
        let summaries = live.workspace_summaries();
        let leaves = live.layout().leaf_ids();
        let pinned = live.pinned_views();
        let seed = StartupSeed {
            workspaces: &summaries,
            targets: &[],
            leaves: &leaves,
            pinned: &pinned,
            image_roots: &[],
            history_capture: bitty_config::HistoryCaptureConfig::default(),
        };
        let session = discover_with_roots(false, 80, 24, &seed, None, vec![root], None, None)
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

    #[test]
    fn startup_terminal_submit_lands_byte_exact_on_tick() {
        // CTX-1239 prod path: activation shares the submit queue with
        // `terminal.input.submit` generations, so a granted
        // `bitty.terminal.submit` from Lua queues the byte-exact frame and
        // the tick drain writes it through the single input router.
        // Headless (no live writer) the frame buffers in pending input;
        // headed the same drain delivers it to the live PTY (verify3b).
        //
        // bitty #2409: `#2314` makes the dispatch resumable at the RC-1 wall
        // budget, so under a loaded runner the startup `go` can legitimately
        // park instead of completing inline. Parking is accepted work — the
        // settle helper drives the documented cold-tick resume and reports
        // which path ran. Both paths must land the same byte-exact frame.
        let mut app = submit_test_app("granted", "xuepoo.prodsubmit", &["terminal.input.submit"]);
        let settled = {
            let runtime = app.plugin_runtime.as_mut().expect("plugin runtime");
            settle_dispatch(runtime, "xuepoo.prodsubmit", "go")
        };
        assert_eq!(
            settled.value, "OK:accepted:23",
            "granted submit reports the framed byte count"
        );
        assert!(
            settled.resumes <= 1,
            "a park resumes exactly once, observed {} resumed completions",
            settled.resumes
        );
        assert!(
            app.runtime.pending_input().is_empty(),
            "nothing lands before the tick drain"
        );
        app.apply_plugin_submit_requests();
        assert_eq!(
            app.runtime.pending_input(),
            b"\x1b[200~cargo test\x1b[201~\r".as_slice(),
            "the drain writes the byte-exact paste frame"
        );
        assert!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .submit_queue()
                .borrow()
                .is_empty(),
            "the drain must consume every queued frame"
        );
        // Over-cap text is a denied table with nothing queued.
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodsubmit", "big"),
            "OK:denied:too-large"
        );
        app.apply_plugin_submit_requests();
        assert_eq!(
            app.runtime.pending_input(),
            b"\x1b[200~cargo test\x1b[201~\r".as_slice(),
            "denials emit nothing"
        );
    }

    #[test]
    fn startup_terminal_submit_absent_without_grant() {
        // Fail-closed indistinguishability: without
        // `terminal.input.submit` the generation observes no `submit` key
        // at all (the composer nil path), so the Lua call can never reach
        // the queue and the runtime is never touched.
        let mut app = submit_test_app("denied", "xuepoo.prodsubmitdenied", &[]);
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodsubmitdenied", "go"),
            "ABSENT"
        );
        assert_eq!(
            dispatch_pin(&mut app, "xuepoo.prodsubmitdenied", "big"),
            "ABSENT"
        );
        app.apply_plugin_submit_requests();
        assert!(app.runtime.pending_input().is_empty());
    }

    // ── bitty #2409: deterministic Parked -> one resume -> byte-exact submit

    /// Snapshot source that charges real host wall time per call.
    ///
    /// The RC-1 hard limit accounts charged time, and `terminal.snapshot` host
    /// time stays charged (only supervised spawn waits and durable store
    /// commits are credited), so eight calls charge ~80 ms against the 50 ms
    /// budget. That parks a command on any runner, fast or loaded, without
    /// approaching the instruction budget — the #2314 fixture shape.
    struct ChargedSnapshot;

    impl SnapshotSource for ChargedSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    /// Charged-time park probe (bitty #2409).
    ///
    /// `stall_go` burns 80 ms of charged host time and then submits the same
    /// frame as the startup `go` probe, so it exercises the identical submit
    /// backend while parking deterministically. `runs` reports how many
    /// bodies completed, so a duplicated resume is visible as `2`.
    const SUBMIT_PARK_PROBE: &str = r#"
local runs = 0

bitty.commands.register({ id = "stall_go", title = "Stall go", run = function()
  for _ = 1, 8 do
    pcall(bitty.terminal.snapshot, { scope = "semantic" })
  end
  runs = runs + 1
  local submit = bitty.terminal.submit
  if submit == nil then return "ABSENT" end
  local ok, result = pcall(submit, "cargo test")
  if not ok then return "ERR:" .. result.code end
  if result.status == "accepted" then
    return "OK:accepted:" .. tostring(result.bytes)
  end
  return "OK:" .. tostring(result.status) .. ":" .. tostring(result.deny)
end })

bitty.commands.register({ id = "runs", title = "Runs", run = function()
  return tostring(runs) end })
"#;

    const SUBMIT_PARK_COMMANDS: &[&str] = &["stall_go", "runs"];

    fn write_park_probe(root: &Path, id: &str) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
        let qualified: Vec<String> = SUBMIT_PARK_COMMANDS
            .iter()
            .map(|command| format!("\"{id}:{command}\""))
            .collect();
        std::fs::write(
            plugin.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\nterminal.semantic-read = true\nterminal.input.submit = true\n\n[lazy]\ncommands = [{}]\nevents = []\n",
                qualified.join(", "),
            ),
        )
        .expect("manifest");
        std::fs::write(plugin.join("lua/init.lua"), SUBMIT_PARK_PROBE).expect("init");
    }

    /// Headless app whose plugin VM charges real host wall time through
    /// `terminal.snapshot`, so a command parks deterministically.
    fn parked_submit_app(tag: &str, id: &str) -> crate::terminal_app::TerminalApp {
        let root = pin_test_dir(tag);
        write_park_probe(&root, id);
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            third_party_roots: vec![root],
            snapshot: Rc::new(ChargedSnapshot),
            ..PluginRuntimeConfig::default()
        });
        runtime.discover();
        let plugin_id = bitty_plugin_host::manifest::PluginId::new(id).expect("id");
        let report = runtime.activate(&plugin_id).expect("activation");
        assert_eq!(
            report.state,
            bitty_runtime::plugin_runtime::LifecycleState::Active
        );
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        crate::terminal_app::TerminalApp::with_theme(
            Runtime::with_defaults().expect("defaults must build"),
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            crate::spawn::SpawnSpec::default(),
        )
        .with_plugin_runtime(Some(runtime))
    }

    #[test]
    fn startup_terminal_submit_park_resumes_once_and_lands_byte_exact() {
        // bitty #2409 regression: reaching the RC-1 wall budget parks the
        // command instead of refusing it, and the documented recovery resumes
        // the same executor on the cold ticks. The body must run exactly once,
        // the terminal frame must stay byte-exact, and nothing may replay.
        let id = "xuepoo.parksubmit";
        let mut app = parked_submit_app("park", id);
        let plugin_id = bitty_plugin_host::manifest::PluginId::new(id).expect("id");

        let error = app
            .plugin_runtime
            .as_mut()
            .expect("plugin runtime")
            .dispatch_command(&plugin_id, "stall_go", &[])
            .expect_err("80 ms of charged work must park at the 50 ms budget");
        assert!(error.is_parked(), "expected a park, got {error:?}");
        assert_eq!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .parked_command_count(),
            1,
            "the park is recorded exactly once"
        );
        assert!(
            app.runtime.pending_input().is_empty(),
            "a park lands no terminal bytes"
        );
        assert!(
            app.runtime.notification_banner().is_none(),
            "a park surfaces no result before the cold-tick resume"
        );

        // Drive the cold tick exactly as the app event loop does until the
        // parked invocation resolves.
        let mut pumps = 0usize;
        let mut resumed = 0usize;
        for _ in 0..PARK_RESUME_TICKS {
            let completed = app.pump_cold_commands();
            pumps = pumps.saturating_add(1);
            resumed = resumed.saturating_add(completed);
            if completed > 0 {
                break;
            }
        }
        assert_eq!(
            resumed, 1,
            "the park must complete exactly once (after {pumps} cold ticks)"
        );
        assert_eq!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .parked_command_count(),
            0,
            "the completed park is dropped"
        );
        assert_eq!(
            app.pump_cold_commands(),
            0,
            "a completed command must never replay"
        );
        // Exactly one body ran. The park that reaches the budget on the window
        // that finishes the body keeps that body's side effects (a real
        // startup command has already queued its frame), which is exactly why
        // the resume must extract the result instead of re-running anything.
        assert_eq!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .submit_queue()
                .borrow()
                .len(),
            1,
            "the body queued its frame exactly once"
        );
        let runs = {
            let runtime = app.plugin_runtime.as_mut().expect("plugin runtime");
            settle_dispatch(runtime, id, "runs")
        };
        assert_eq!(runs.value, "1", "the callback body must run exactly once");

        app.apply_plugin_submit_requests();
        assert_eq!(
            app.runtime.pending_input(),
            b"\x1b[200~cargo test\x1b[201~\r".as_slice(),
            "the resumed submit is still the byte-exact paste frame"
        );
        assert!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .submit_queue()
                .borrow()
                .is_empty(),
            "the drain consumes the single queued frame"
        );
        let banner = app
            .runtime
            .notification_banner()
            .expect("the resumed result surfaces exactly once");
        assert!(
            banner.contains("stall_go") && banner.contains("OK:accepted:23"),
            "unexpected resumed banner: {banner}"
        );

        // The settle helper every startup probe uses must drive the same
        // recovery: one park, exactly one resumed completion, same value.
        let settled = {
            let runtime = app.plugin_runtime.as_mut().expect("plugin runtime");
            settle_dispatch(runtime, id, "stall_go")
        };
        assert_eq!(settled.value, "OK:accepted:23");
        assert_eq!(
            settled.resumes, 1,
            "the settle helper resumes a park exactly once"
        );
        assert_eq!(
            app.plugin_runtime
                .as_ref()
                .expect("plugin runtime")
                .submit_queue()
                .borrow()
                .len(),
            1,
            "the second run queues exactly one more frame"
        );
    }

    // ── CTX-1345: production panel.create backend ───────────────────────

    const PANEL_PROBE_COMMANDS: &[&str] = &["create_rich", "create_helper", "create_terminal"];

    /// Probe plugin: table-form `bitty.panel.create` reports
    /// `OK:<id>/<generation>` or `ERR:<code>` across the real Lua bridge.
    const PANEL_PROBE: &str = r#"
local function create_result(typ)
  local ok, res = pcall(bitty.panel.create, { type = typ })
  if not ok then return "ERR:" .. res.code end
  return "OK:" .. tostring(res.id) .. "/" .. tostring(res.generation)
end

bitty.commands.register({ id = "create_rich", title = "Create rich", run = function()
  return create_result("rich") end })
bitty.commands.register({ id = "create_helper", title = "Create helper", run = function()
  return create_result("helper") end })
bitty.commands.register({ id = "create_terminal", title = "Create terminal", run = function()
  return create_result("terminal") end })
"#;

    fn write_panel_probe(root: &Path, id: &str, capabilities: &[&str]) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
        let qualified: Vec<String> = PANEL_PROBE_COMMANDS
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
        std::fs::write(plugin.join("lua/init.lua"), PANEL_PROBE).expect("init");
    }

    /// Activate a panel probe through the real startup composition root.
    fn panel_probe_runtime(tag: &str, id: &str, capabilities: &[&str]) -> PluginRuntime {
        let root = pin_test_dir(tag);
        write_panel_probe(&root, id, capabilities);
        let live = Runtime::with_defaults().expect("defaults must build");
        let summaries = live.workspace_summaries();
        let leaves = live.layout().leaf_ids();
        let pinned = live.pinned_views();
        let seed = StartupSeed {
            workspaces: &summaries,
            targets: &[],
            leaves: &leaves,
            pinned: &pinned,
            image_roots: &[],
            history_capture: bitty_config::HistoryCaptureConfig::default(),
        };
        discover_with_roots(false, 80, 24, &seed, None, vec![root], None, None)
            .expect("dev root must yield a session")
            .runtime
    }

    fn dispatch_panel(runtime: &mut PluginRuntime, id: &str, command: &str) -> String {
        settle_dispatch(runtime, id, command).value
    }

    #[test]
    fn startup_panel_create_backend_serves_granted_lua_verb() {
        // CTX-1345: the startup wiring must install a real allocator, not
        // the unwired seam (which returned E_NOT_IMPLEMENTED for every
        // granted call). Distinct ids prove each call mints a fresh record.
        let mut runtime = panel_probe_runtime("granted", "xuepoo.prodcreate", &["panel.create"]);
        let first = dispatch_panel(&mut runtime, "xuepoo.prodcreate", "create_rich");
        let (first_id, first_generation) = parse_panel_handle(&first, "granted create");
        let second = dispatch_panel(&mut runtime, "xuepoo.prodcreate", "create_helper");
        let (second_id, second_generation) = parse_panel_handle(&second, "second create");
        assert_ne!(
            first_id, second_id,
            "each create allocates a distinct panel id"
        );
        assert!(
            second_generation > first_generation,
            "the registry generation advances ({first_generation} -> {second_generation})"
        );
    }

    #[test]
    fn startup_panel_create_backend_denies_ungranted_generation() {
        // Grant gating stays: without `panel.create` activation captures no
        // backend and the call-time gate denies before allocation.
        let mut runtime = panel_probe_runtime("denied", "xuepoo.prodcreatedenied", &[]);
        assert_eq!(
            dispatch_panel(&mut runtime, "xuepoo.prodcreatedenied", "create_rich"),
            "ERR:E_CAPABILITY_DENIED"
        );
    }

    #[test]
    fn startup_panel_create_backend_rejects_terminal_without_allocating() {
        // Zero-PTY policy: `terminal` fails closed with the existing typed
        // error, and the same generation still serves a native type.
        let mut runtime =
            panel_probe_runtime("terminal", "xuepoo.prodcreateterminal", &["panel.create"]);
        assert_eq!(
            dispatch_panel(&mut runtime, "xuepoo.prodcreateterminal", "create_terminal"),
            "ERR:E_DEF_INVALID"
        );
        let created = dispatch_panel(&mut runtime, "xuepoo.prodcreateterminal", "create_rich");
        parse_panel_handle(&created, "rich after terminal rejection");
    }

    #[test]
    fn startup_panel_create_backend_allocates_registry_records() {
        // Handler contract: allocate a `Created` record of the parsed type
        // and return its raw `(id, generation)`; unsupported types allocate
        // nothing.
        let panels = Rc::new(RefCell::new(
            PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
        ));
        let backend = startup_panel_create_backend(Rc::clone(&panels));
        let (id, generation) = backend("rich").expect("rich allocates");
        assert_eq!(panels.borrow().panel_count(), 1);
        assert_eq!(
            panels
                .borrow()
                .panel_state(PanelId(id), Generation(generation))
                .expect("live handle"),
            PanelState::Created,
            "creation allocates only; the host mounts"
        );
        for unsupported in ["terminal", "bogus"] {
            let error = backend(unsupported).expect_err("unsupported type must fail");
            assert_eq!(error.code, "E_DEF_INVALID", "{unsupported}");
        }
        assert_eq!(
            panels.borrow().panel_count(),
            1,
            "rejected types allocate nothing"
        );
    }

    fn parse_panel_handle(result: &str, context: &str) -> (u64, u64) {
        let rest = result
            .strip_prefix("OK:")
            .unwrap_or_else(|| panic!("{context}: expected OK result, got {result}"));
        let (id, generation) = rest
            .split_once('/')
            .unwrap_or_else(|| panic!("{context}: expected id/generation, got {result}"));
        (
            id.parse().expect("numeric panel id"),
            generation.parse().expect("numeric generation"),
        )
    }
}
