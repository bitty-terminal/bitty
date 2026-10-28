//! Host-service boundary for one plugin generation (RFC Gap C, minimal slice).
//!
//! `PluginServices` is the object-safe [`HostServices`] implementation handed
//! to a plugin VM. It owns the plugin-scoped store and defers settings and
//! terminal snapshots to injected, generation-stable sources. Capability
//! gating is evaluated from the grant snapshot taken at activation; an absent
//! grant fails closed before any side effect.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use bitty_config::theme::CustomPalette;
use bitty_config::{ChromeAction, ResolvedKeymap};
use bitty_lua::keymaps::{
    KEYMAP_LIST_CALLS_PER_SECOND, KEYMAP_SOURCE_DEFAULT, KEYMAP_SOURCE_USER, KeymapPage,
    KeymapQuery, KeymapRow,
};
use bitty_lua::runtime_inspect::{
    RUNTIME_INSPECT_CALLS_PER_SECOND, RUNTIME_INSPECT_MAX_WALL_MS, RuntimeInspectSnapshot,
};
use bitty_lua::ui::UiSlot;
use bitty_lua::ui::{UI_MAX_AGGREGATED_TEXT_BYTES, UI_MAX_BLOCKS, UiNode, UiSceneScope};
use bitty_lua::{
    BridgeError, COMPONENT_CLASS_CAPABILITY_DENIED, CommandCatalogEntry, ComponentBridgeRequest,
    E_UI_NOT_OWNER, E_UI_UNAVAILABLE, FS_CONTENT_MAX_BYTES, FS_LIST_MAX_BYTES, FS_LIST_MAX_ENTRIES,
    HistoryQueryOp, HistoryQueryRequest, HistoryQuerySource, HostServices, LuaValue, LuaVm,
    OVERLAY_CALL_MAX_BYTES, OVERLAY_SPEC_PLACEHOLDER_MAX_BYTES, OVERLAY_SPEC_TITLE_MAX_BYTES,
    OverlayInput, OverlayPoll, SNAPSHOT_MAX_BYTES, ServiceRoute, StashedFunction,
    TargetSnapshotRow, ThemeTable, WORKSPACE_LIST_MAX_ITEMS, WorkspaceInfo, WorkspaceRequest,
    component_bridge_error, env_grant_shape_ok, validate_env_key,
};
use bitty_package::Version;
use bitty_rich::{
    BG_MAX_ENCODED_BYTES, BG_MAX_ROOTS, BackgroundFormat, ResourcePolicy, SubmitBudget, SubmitDeny,
    check_header_bounds, check_terminal_submit, expand_background_path, sniff_image,
    validate_resource_path,
};
use bitty_ui::{
    CommandBlockId, DerivedProvider, DispatchError, HintAnnotationLayer, HintAnnotationSpec,
    LabelAllocator, LabelPolicy, LinkId, ProviderError, ProviderMediator, ProviderTarget,
    QualifiedCommand, Rect, TargetDispatcher, TargetProvider, TargetRef, TargetRegistry,
};
use bitty_ui::{MAX_HINT_TARGETS, MAX_SNAPSHOT_TARGETS, MAX_TARGET_BINDINGS, MAX_TARGET_PROVIDERS};
use bitty_ui::{PanelId, Point, UiNodeId, WorkspaceId};

use crate::runtime::band_slots::{UiSlotPlacement, ui_slot_placement, unsupported_slot_error};
use crate::runtime::fx::{FxPulseQueue, QueuedFxPulse};
use bitty_plugin_host::{
    FsBridgeError, FsCaps, FsGate, HISTORY_READ_VERSION, HistoryDenialKind, HistoryGate,
    HistoryGrant, HistoryScope, HistorySource, PluginId, ProvidedService, QueryOp, SnapshotQuery,
    TrustLevel, parse_service_reference, service_version_satisfies, value_satisfies_schema,
};

use super::config_write::{
    CONFIG_PREVIEW_MAX_KEYS, CONFIG_PREVIEW_MAX_RENDERED_BYTES, CONFIG_SET_MAX_RENDERED_BYTES,
    CONFIG_SET_MAX_WALL_MS, ConfigApplyOutcome, ConfigChange, ConfigPreviewRendering,
    ConfigPreviewToken, ConfigSetOutcome, ConfigSettingValue, ConfigWriteBackend, ConfigWriteGate,
    approval_digest, check_result_rows, config_backend_error, config_key_error, config_unavailable,
    writable_setting,
};
use super::history::{
    HISTORY_GRANT_SCOPE_CACHE_MAX, HISTORY_RATE_WINDOW, HistoryProvider, history_caps,
    history_error, history_page_value, history_source, history_unavailable,
};

use super::component_requests::{ComponentPollOutcome, ComponentRequestQueue, outcome_value};
use super::debug::{self, DebugView, TraceHub, TraceRequest};
use super::fs_list::{FsListBackend, FsListEntry};
use super::overlay::{
    DEFAULT_RELEASE_REASON, OVERLAY_CAPTURE_TIMEOUT_MS, OverlayCapture, is_owner_release_reason,
};
use super::presentation_overlay::PresentationOverlay;
use super::store::{self, PluginStore};

/// Maximum characters of a provider failure message relayed to the consumer
/// (`E_SERVICE_FAILED`).
///
/// Provider errors cross as diagnostics, never as live values; the cap keeps
/// a hostile provider from stuffing an unbounded string into a consumer
/// error table (bridge error tables are themselves bounded downstream).
pub const SERVICE_FAILED_MESSAGE_LIMIT: usize = 256;

/// Maximum `env.read:<KEY>` grants held by one plugin generation (CTX-0330).
///
/// Precedent: `MAX_RESOLVED_ENV_VARS` (`64`) bounds secret env bindings in
/// the accepted host store; the `bitty.env` grant set reuses it so one
/// generation can never accumulate an unbounded allowlist.
pub const MAX_ENV_GRANTS: usize = 64;

/// Maximum bytes of one `bitty.env` value crossing into Lua (CTX-0330).
///
/// Precedent: `MAX_SECRET_VALUE_BYTES` (`4096`) bounds secret values in the
/// accepted IPC execution budgets; host environment values reuse it so an
/// unbounded variable (e.g. a dumped keyring) fails closed with
/// `E_DEF_LIMIT` instead of crossing the bridge.
pub const MAX_ENV_VALUE_BYTES: usize = 4096;

/// Per-generation `bitty.runtime.inspect` call budget (issue #2396).
///
/// One one-second window admits [`RUNTIME_INSPECT_CALLS_PER_SECOND`] calls;
/// the window resets lazily on the first call after it expires. Denials are
/// typed (`E_DEF_LIMIT`) and never touch the snapshot source.
#[derive(Debug)]
struct InspectCallBudget {
    window_start: Instant,
    calls: u32,
}

impl InspectCallBudget {
    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            calls: 0,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.calls = 0;
        }
        if self.calls >= RUNTIME_INSPECT_CALLS_PER_SECOND {
            return false;
        }
        self.calls = self.calls.saturating_add(1);
        true
    }
}

/// Per-generation `bitty.keymaps.list` call budget (issue #2405).
///
/// A strict sliding window: at most [`KEYMAP_LIST_CALLS_PER_SECOND`] accepted
/// calls in any one-second span. Expired timestamps are dropped from the front
/// before each decision, so a burst that crosses a wall-clock second boundary
/// cannot exceed the documented rate — a fixed reset-on-call window would
/// admit up to twice the rate around the boundary. Denials are typed
/// (`E_DEF_LIMIT`) and never touch the keymap source. A full 1024-row scan
/// costs at most four calls at the 256-row page cap, so the budget admits
/// every legitimate paging loop with headroom.
#[derive(Debug)]
struct KeymapCallBudget {
    /// Accept times of the calls inside the current window, oldest first.
    /// Bounded by [`KEYMAP_LIST_CALLS_PER_SECOND`].
    recent: VecDeque<Instant>,
}

impl KeymapCallBudget {
    fn new() -> Self {
        Self {
            recent: VecDeque::new(),
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        const WINDOW: Duration = Duration::from_secs(1);
        while self
            .recent
            .front()
            .is_some_and(|accepted| now.duration_since(*accepted) >= WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= KEYMAP_LIST_CALLS_PER_SECOND as usize {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// Per-plugin `bitty.fs.list` rate window.
///
/// Reuses the accepted RC-8 one-second rate window; the op and byte ceilings
/// are the accepted listing bridge bounds ([`FS_LIST_MAX_ENTRIES`] and
/// [`FS_LIST_MAX_BYTES`]), so the mechanism mints no new public limit.
const FS_LIST_WINDOW: Duration = crate::runtime::bell::RC8_WINDOW;

/// Accepted caps for the real-filesystem `bitty.fs.list` path.
///
/// Per-call bounds reuse the bridge constants; the per-plugin window reuses
/// the same accepted entry/byte bounds as its op and byte ceilings.
fn fs_list_caps() -> FsCaps {
    FsCaps::new(
        u32::try_from(FS_CONTENT_MAX_BYTES).unwrap_or(u32::MAX),
        u32::try_from(FS_CONTENT_MAX_BYTES).unwrap_or(u32::MAX),
        u32::try_from(FS_LIST_MAX_ENTRIES).unwrap_or(u32::MAX),
        u32::try_from(FS_LIST_MAX_BYTES).unwrap_or(u32::MAX),
        FS_LIST_MAX_ENTRIES as u64,
        FS_LIST_MAX_BYTES as u64,
    )
    .expect("accepted fs.list bridge bounds are nonzero")
}

/// Map a gate window denial to the oracle-tight bridge error.
fn fs_gate_error(error: FsBridgeError) -> BridgeError {
    match error.denial_kind() {
        Some(kind) => super::fs_list::denial(kind),
        None => super::fs_list::denial(bitty_plugin_host::FsBridgeDenialKind::ScopeMismatch),
    }
}

/// Canonical workspace claim (`workspaceline`).
///
/// #1572 / CTX-0994: the bundled `bitty-terminal.workspace` manifest is
/// retired, so this vocabulary no longer lives in
/// `bitty-plugin-host::bundled`. The claim itself stays valid: the future
/// first-party `bar` plugin (and any third-party presenter) declares it in
/// `[lazy].claims` to satisfy the `tabline` slot gate below.
pub const WORKSPACELINE_CLAIM: &str = "workspaceline";

/// Injected spawn backend for one plugin generation.
///
/// Receives the Lua-validated argv and returns the bounded result table
/// (`output`/`stderr`/`truncated`/`exit_code`/`execution_id`/`untrusted`). The production
/// backend is the consent-gated [`SpawnService`](super::spawn::SpawnService)
/// path wired at activation; tests inject canned closures. `None` (no
/// backend) fails closed with `E_SPAWN_UNAVAILABLE`.
///
/// The handler takes no working directory: CTX-1345 review F1 found that a
/// plugin-controlled `{ cwd }` is unconfined, so the bridge rejects the
/// option and the backend has no path to honor it.
pub type SpawnHandler = Rc<dyn Fn(&[String]) -> Result<LuaValue, BridgeError>>;

/// Read-only typed settings source (owned by `bitty-config` in the app).
pub trait SettingsSource {
    /// Read one setting; `None` when absent.
    fn get(&self, key: &str) -> Option<LuaValue>;
}

/// Bounded committed terminal-snapshot source.
pub trait SnapshotSource {
    /// Read a bounded snapshot for `scope`.
    ///
    /// # Errors
    ///
    /// Returns a typed `E_SNAPSHOT_*`/`E_CAPABILITY_DENIED` error.
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError>;
}

/// Read-only workspace source for `bitty.workspace.list()` (CTX-0889).
///
/// The application publishes Core workspace state into its implementation
/// once per tick (the `LiveSnapshot` pattern), so plugin reads never touch
/// the runtime directly and never see terminal content.
pub trait WorkspaceSource {
    /// Current workspaces in order, bounded by
    /// [`bitty_lua::WORKSPACE_LIST_MAX_ITEMS`].
    ///
    /// # Errors
    ///
    /// Returns a typed error when no workspace state is available.
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError>;
}

/// Workspace source for hosts without a workspace backend: fails closed
/// with `E_NOT_IMPLEMENTED` (a granted read still observes nothing).
#[derive(Debug, Default)]
pub struct UnavailableWorkspaces;

impl WorkspaceSource for UnavailableWorkspaces {
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        Err(BridgeError::not_implemented("bitty.workspace.list"))
    }
}

/// Read-only resolved-keymap source for `bitty.keymaps.list()` (issue #2405).
///
/// The application publishes its live resolved table into the implementation;
/// the call path clones that bounded table, converts it to canonical rows, and
/// releases the borrow before marshalling. No config path, raw user text,
/// argv, environment value, URL, terminal content, or plugin store value
/// crosses this trait.
pub trait KeymapSource {
    /// The resolved keymap table at call time, in resolved-table order.
    ///
    /// # Errors
    ///
    /// Returns a typed error when no keymap table is available.
    fn keymaps(&self) -> Result<Vec<ResolvedKeymap>, BridgeError>;
}

/// Keymap source for hosts without a registry: fails closed with
/// `E_NOT_IMPLEMENTED` (a granted read still observes nothing).
#[derive(Debug, Default)]
pub struct UnavailableKeymaps;

impl KeymapSource for UnavailableKeymaps {
    fn keymaps(&self) -> Result<Vec<ResolvedKeymap>, BridgeError> {
        Err(BridgeError::not_implemented("bitty.keymaps.list"))
    }
}

/// Convert one resolved binding into a canonical, bounded registry row.
///
/// The strings come from the typed [`ResolvedKeymap`] value (parsed chord,
/// parsed [`ChromeAction`]), never from raw configuration text. A Core action
/// contributes its static description as `title`; an
/// [`ChromeAction::InvokeCommand`] contributes only the validated qualified
/// target as `command`. Any row that fails [`KeymapRow::new`] validation is
/// dropped by [`keymap_rows`] instead of echoed.
#[must_use]
pub fn keymap_row(binding: &ResolvedKeymap) -> Option<KeymapRow> {
    let (title, command) = match &binding.action {
        ChromeAction::InvokeCommand(target) => (None, Some(target.clone())),
        action => (Some(action.description().to_string()), None),
    };
    KeymapRow::new(
        binding.chord.canonical(),
        binding.action.canonical(),
        if binding.from_default {
            KEYMAP_SOURCE_DEFAULT
        } else {
            KEYMAP_SOURCE_USER
        },
        binding.context.clone(),
        title,
        command,
    )
}

/// Convert the live resolved table into addressable rows, dropping malformed
/// entries.
///
/// Plugin-suggested bindings are deliberately absent from this table: the
/// read API serves the resolved user/default registry, while suggestions are
/// published through the separate generation-fenced dispatch directory
/// (`bitty-runtime` `SuggestedBindingDirectory`) and dispatch only the
/// owning generation's registered command. Advertising them here would
/// misreport them as resolvable user bindings, so they join this page only
/// if the read contract later defines a distinct suggestion source.
#[must_use]
pub fn keymap_rows(bindings: &[ResolvedKeymap]) -> Vec<KeymapRow> {
    bindings.iter().filter_map(keymap_row).collect()
}

/// One live Core target with its presentation anchor (W-29 successor).
///
/// Plain data published by the application once per tick (the `LiveSnapshot`
/// pattern): the runtime never reaches into live layout state from a plugin
/// call, and the plugin never sees terminal content. `rect` is in window cell
/// coordinates and may be absent for surfaces the host cannot anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreTargetInfo {
    /// Surface kind (`panel`, `workspace`, `block`, `node`, `link`).
    pub kind: String,
    /// Stable semantic id within `kind`.
    pub id: u64,
    /// Owning workspace id, when workspace-scoped.
    pub workspace: Option<u64>,
    /// Screen region in window cells (`x`, `y`, `width`, `height`).
    pub rect: Option<(u16, u16, u16, u16)>,
}

/// Read-only Core target/anchor source for target sessions (W-29 successor).
///
/// The application publishes live panel and workspace anchors into its
/// implementation once per tick. Collection is cold-path only: the mediator
/// reads this source when a session enters, never per frame or per input
/// event.
pub trait TargetSource {
    /// Current Core targets in deterministic order, bounded by
    /// [`MAX_SNAPSHOT_TARGETS`].
    ///
    /// # Errors
    ///
    /// Returns a typed error when the source cannot serve state.
    fn targets(&self) -> Result<Vec<CoreTargetInfo>, BridgeError>;
}

/// Target source for hosts without a target backend: an honest empty set.
///
/// The Core provider is then empty and plugin lenses still work; no plugin
/// gains authority from the absence of a source.
#[derive(Debug, Default)]
pub struct UnavailableTargets;

impl TargetSource for UnavailableTargets {
    fn targets(&self) -> Result<Vec<CoreTargetInfo>, BridgeError> {
        Ok(Vec::new())
    }
}

/// One queued workspace mutation with its requesting plugin (CTX-0889).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedWorkspaceRequest {
    /// Requesting plugin id (diagnostics only; authority was checked at
    /// enqueue time against the plugin's `workspace.control` grant).
    pub plugin_id: String,
    /// Validated request.
    pub request: WorkspaceRequest,
}

/// Bounded runtime-shared workspace request queue (CTX-0889).
///
/// Overflow drops the newest request and counts it (same governance as
/// [`NotificationQueue`]); the application drains it once per tick.
#[derive(Debug)]
pub struct WorkspaceRequestQueue {
    items: VecDeque<QueuedWorkspaceRequest>,
    capacity: usize,
    dropped: u64,
}

impl WorkspaceRequestQueue {
    /// Create a bounded queue (capacity at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push a request; returns whether it was accepted.
    pub fn push(&mut self, request: QueuedWorkspaceRequest) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(request);
        true
    }

    /// Drain all queued requests in FIFO order.
    pub fn drain(&mut self) -> Vec<QueuedWorkspaceRequest> {
        self.items.drain(..).collect()
    }

    /// Number of queued requests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// One queued pinned-float mutation (CTX-1115).
///
/// Carries no requesting plugin id: the shared [`PanelPinHandler`] backend
/// seam has no per-generation identity, so drain diagnostics name the panel
/// id instead (authority was checked at enqueue time against the generation's
/// `panel.focus` grant, and the snapshot check below ran before queueing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuedPinRequest {
    /// Lua-validated panel id (a `ViewId` raw value).
    pub panel_id: u64,
    /// Desired pinned state.
    pub pinned: bool,
}

/// Bounded runtime-shared pin request queue (CTX-1115).
///
/// Same governance as [`WorkspaceRequestQueue`]: overflow drops the newest
/// request and counts it, and the application drains it once per tick. The
/// queue holds validated intents only; the drain revalidates each intent
/// against live state, so a panel that vanished between enqueue and drain
/// fails closed with state untouched.
#[derive(Debug)]
pub struct PanelPinQueue {
    items: VecDeque<QueuedPinRequest>,
    capacity: usize,
    dropped: u64,
}

impl PanelPinQueue {
    /// Create a bounded queue (capacity at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push a request; returns whether it was accepted.
    pub fn push(&mut self, request: QueuedPinRequest) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(request);
        true
    }

    /// Drain all queued requests in FIFO order.
    pub fn drain(&mut self) -> Vec<QueuedPinRequest> {
        self.items.drain(..).collect()
    }

    /// Number of queued requests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Per-generation `bitty.terminal.submit` byte window (CTX-1239, issue #2101).
///
/// Matches the native-operation test window (1 MiB): the numeric budget
/// policy belongs to the Isolation/Resource lane, which this operation must
/// not invent (DEC-W103-4); this only enforces and attributes. Charged only
/// on enqueue success; denials and dropped (queue-full) intents leave it
/// untouched.
pub const TERMINAL_SUBMIT_WINDOW_BYTES: u64 = 1024 * 1024;

/// One queued submit intent (CTX-1239, issue #2101).
///
/// Carries the byte-exact bracketed-paste frame, validated and budgeted at
/// enqueue time against the generation's `terminal.input.submit` grant, so
/// the drain performs a mechanical PTY write with no further authorization.
/// Authority was checked at enqueue; the frame bytes are already-framed
/// output data, never a re-interpretable request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSubmitRequest {
    /// Byte-exact frame (`ESC[200~` content `ESC[201~` `CR`) for one PTY write.
    pub frame: Vec<u8>,
}

/// Bounded runtime-shared submit intent queue (CTX-1239, issue #2101).
///
/// Same governance as [`PanelPinQueue`]: overflow drops the newest intent
/// and counts it, and the application drains it once per tick. The queue
/// holds validated, budgeted frames only.
#[derive(Debug)]
pub struct TerminalSubmitQueue {
    items: VecDeque<QueuedSubmitRequest>,
    capacity: usize,
    dropped: u64,
}

impl TerminalSubmitQueue {
    /// Create a bounded queue (capacity at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push a request; returns whether it was accepted.
    pub fn push(&mut self, request: QueuedSubmitRequest) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(request);
        true
    }

    /// Drain all queued requests in FIFO order.
    pub fn drain(&mut self) -> Vec<QueuedSubmitRequest> {
        self.items.drain(..).collect()
    }

    /// Number of queued requests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Accepted `bitty.terminal.submit` outcome table (CTX-1239).
///
/// Shape contract the composer policy switches on: `status` is always
/// `"accepted"` here, `bytes` the framed byte count queued for one PTY
/// write.
fn submit_accepted(bytes: usize) -> LuaValue {
    LuaValue::table([
        ("status", LuaValue::String("accepted".to_string())),
        (
            "bytes",
            LuaValue::Integer(bytes.min(i64::MAX as usize) as i64),
        ),
    ])
}

/// Refused `bitty.terminal.submit` outcome table (CTX-1239).
///
/// `deny` names the violated rule, never the payload: `"too-large"` (over
/// the content cap), `"lease-denied"` (lease write refused — unreachable on
/// the grant-authorized path, kept for mapping completeness),
/// `"budget-exceeded"` (generation window exhausted), or `"queue-full"`
/// (shared queue saturated; nothing charged, retry later). Every denial
/// leaves the buffer and the modal session intact by composer contract.
fn submit_denied(deny: &str) -> LuaValue {
    LuaValue::table([
        ("status", LuaValue::String("denied".to_string())),
        ("deny", LuaValue::String(deny.to_string())),
    ])
}

/// One queued colorscheme intent (CTX-1169, issue #1976).///
/// Carries the validated palette plus its durability: `persist = false` is a
/// preview (live-apply only, never a file write), `persist = true` is a
/// confirm (live-apply after the durable commit lands). Authority was checked
/// at enqueue time against the generation's `runtime.configure` grant, and
/// the palette grammar was enforced by `CustomPalette::from_hex` before
/// queueing, so the drain trusts the shape and revalidates only liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedThemeRequest {
    /// Validated custom palette (canonical `#RRGGBB` leaves, 16 ANSI slots).
    pub palette: CustomPalette,
    /// Whether the drain must durably commit before live-applying.
    pub persist: bool,
}

/// Bounded runtime-shared theme intent queue (CTX-1169, issue #1976).
///
/// Same governance as [`PanelPinQueue`]: overflow drops the newest intent and
/// counts it, and the application drains it once per tick. Preview and
/// confirm intents share one FIFO so a confirm always lands after the preview
/// that preceded it.
#[derive(Debug)]
pub struct ThemeRequestQueue {
    items: VecDeque<QueuedThemeRequest>,
    capacity: usize,
    dropped: u64,
}

impl ThemeRequestQueue {
    /// Create a bounded queue (capacity at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push an intent; returns whether it was accepted.
    pub fn push(&mut self, request: QueuedThemeRequest) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(request);
        true
    }

    /// Drain all queued intents in FIFO order.
    pub fn drain(&mut self) -> Vec<QueuedThemeRequest> {
        self.items.drain(..).collect()
    }

    /// Number of queued intents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Durable-commit backend for one confirmed colorscheme (CTX-1169, issue
/// #1976).
///
/// Receives the validated palette a `bitty.theme.confirm` intent carries and
/// durably commits it (the application's config-file write); the drain
/// live-applies only after this returns success, so a failed commit leaves
/// both the file and the screen untouched. The production backend is wired at
/// activation by the application; tests inject canned closures. `None` (no
/// backend) leaves granted confirms failing closed at drain time with state
/// untouched, mirroring the pin-backend pattern.
pub type ThemePersistHandler = Rc<dyn Fn(&CustomPalette) -> Result<(), BridgeError>>;

/// Reads at most BG-1 plus one probe byte from `canonical` (CTX-1169, issue
/// #1976).
///
/// The cap is enforced on the actual bytes, so allocation stays bounded even
/// when pre-read metadata was stale (same probe discipline as the background
/// store's fd-pinned read, without the decode). Over-bound files fail closed
/// with `E_DEF_LIMIT`; unreadable files with `E_IMAGE_UNREADABLE`.
fn read_image_bytes_bounded(canonical: &Path) -> Result<Vec<u8>, BridgeError> {
    use std::io::Read;
    let shown = canonical.display().to_string();
    let file = std::fs::File::open(canonical).map_err(|err| {
        BridgeError::new(
            "io",
            "E_IMAGE_UNREADABLE",
            format!("image read failed for {shown}: {err}"),
        )
    })?;
    let mut bytes = Vec::new();
    file.take(BG_MAX_ENCODED_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| {
            BridgeError::new(
                "io",
                "E_IMAGE_UNREADABLE",
                format!("image read failed for {shown}: {err}"),
            )
        })?;
    if bytes.len() > BG_MAX_ENCODED_BYTES {
        return Err(BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!(
                "image is {} encoded bytes; at most {BG_MAX_ENCODED_BYTES} allowed",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// Canonical format label for one sniffed header (CTX-1169, issue #1976).
fn background_format_name(format: BackgroundFormat) -> &'static str {
    match format {
        BackgroundFormat::Png => "png",
        BackgroundFormat::Jpeg => "jpeg",
        BackgroundFormat::WebP => "webp",
    }
}

/// Synchronous pinned-float backend for one plugin generation (CTX-1083).
///
/// Receives the Lua-validated panel id (a `ViewId` raw value) plus the
/// desired pinned state and returns whether that state now holds (`true`
/// covers both an applied transition and an idempotent no-op; `false` is
/// the fail-closed answer for an unknown or unpinnable target with state
/// untouched). The production backend routes into the same
/// `pin_floating`/`unpin_floating` runtime path the `toggle_pinned` keymap
/// action uses; tests inject canned closures. `None` (no backend) fails
/// closed with `E_NOT_IMPLEMENTED`.
pub type PanelPinHandler = Rc<dyn Fn(u64, bool) -> Result<bool, BridgeError>>;

/// Synchronous panel-creation backend for one plugin generation (CTX-1214,
/// Issue #2085).
///
/// Receives the Lua-validated panel type name (`rich`, `browser`, `helper`,
/// or `canvas` — never `terminal`; the [`PluginServices::panel_create`]
/// gate rejects PTY-backed types before the backend runs, so a backend
/// never mints process-spawn authority) and returns the raw `(panel_id,
/// generation)` pair for the freshly allocated registry record. The record
/// starts in `Created` state: mounting and focusing stay host-driven
/// (layout authority never crosses into the plugin), so the backend
/// allocates only. Registry exhaustion maps to `E_DEF_LIMIT`; a disposed
/// or otherwise unservable host maps to `E_NOT_IMPLEMENTED`. Tests inject
/// canned closures over a real `PanelRegistry`; `None` (no backend) fails
/// closed with `E_NOT_IMPLEMENTED`.
pub type PanelCreateHandler = Rc<dyn Fn(&str) -> Result<(u64, u64), BridgeError>>;

/// Settings source that has no settings.
#[derive(Debug, Default)]
pub struct EmptySettings;

impl SettingsSource for EmptySettings {
    fn get(&self, _key: &str) -> Option<LuaValue> {
        None
    }
}

/// Fixed-map settings source (tests, embedding).
///
/// Serves owned values verbatim with no type checks: the caller (usually the
/// `plugin_settings` merge) is responsible for declaration agreement. The [`PluginServices`] declaration gate still
/// applies on top — a key outside the generation's declared set never
/// reaches this source.
#[derive(Debug, Default, Clone)]
pub struct MapSettings {
    values: BTreeMap<String, LuaValue>,
}

impl MapSettings {
    /// Build a source from owned pairs.
    #[must_use]
    pub fn new(values: BTreeMap<String, LuaValue>) -> Self {
        Self { values }
    }
}

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.values.get(key).cloned()
    }
}

/// Snapshot source that fails closed with `E_SNAPSHOT_UNAVAILABLE`.
#[derive(Debug, Default)]
pub struct UnavailableSnapshot;

impl SnapshotSource for UnavailableSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            "E_SNAPSHOT_UNAVAILABLE",
            "no committed terminal state is available for snapshot",
        ))
    }
}

/// Host environment source for `bitty.env` reads (CTX-0330).
///
/// Values are read host-side only; the grant gate in [`PluginServices`]
/// decides *which* keys a generation may see, while the source decides
/// *what* a granted key resolves to. Split so tests inject [`MapEnv`]
/// without touching the process environment.
pub trait EnvSource {
    /// Read one variable; `None` when absent.
    fn get(&self, key: &str) -> Option<String>;
}

/// Environment source that resolves nothing (default: deny by absence).
#[derive(Debug, Default)]
pub struct EmptyEnv;

impl EnvSource for EmptyEnv {
    fn get(&self, _key: &str) -> Option<String> {
        None
    }
}

/// Environment source reading the host process environment.
///
/// Wired at activation; values cross only for granted keys and within
/// [`MAX_ENV_VALUE_BYTES`]. Non-UTF-8 variables resolve as absent rather
/// than lossy: a plugin must never observe mojibake it could mistake for a
/// credential.
#[derive(Debug, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Fixed-map environment source (tests, embedding).
#[derive(Debug, Default, Clone)]
pub struct MapEnv {
    values: BTreeMap<String, String>,
}

impl MapEnv {
    /// Build a source from owned pairs.
    #[must_use]
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }
}

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }
}

/// One accepted notification handed to the platform asynchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Notification title.
    pub title: String,
    /// Notification body (bounded by the caller's bridge limits).
    pub body: String,
    /// Urgency hint.
    pub urgency: String,
}

/// Bounded notification queue (`RC-8` rate governance; overflow drops newest).
#[derive(Debug)]
pub struct NotificationQueue {
    items: VecDeque<Notification>,
    capacity: usize,
    dropped: u64,
}

impl NotificationQueue {
    /// Create a bounded queue.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push a notification; returns whether it was accepted.
    pub fn push(&mut self, notification: Notification) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(notification);
        true
    }

    /// Drain all queued notifications.
    pub fn drain(&mut self) -> Vec<Notification> {
        self.items.drain(..).collect()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// UI authorization snapshot for one plugin generation (CTX-0428).
///
/// Built from the activation grant (the same intersection the other gates
/// use) plus the manifest `[lazy].claims` list; absent grants fail closed at
/// call time and are never inferred from the plugin id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiAccess {
    /// `ui.rich` granted — declarative rich content.
    pub rich: bool,
    /// `ui.overlay` granted — the `overlay` slot additionally requires it.
    pub overlay: bool,
    /// `ui.overlay.focus` granted (accepted W-01 v2) — the focusable overlay
    /// and transient input-capture surface additionally requires it. Distinct
    /// from v1 `ui.overlay` (presentation-only, non-focusable); no
    /// implication either way.
    pub overlay_focus: bool,
    /// Manifest `[lazy].claims` (exclusive slot claims; `workspaceline` for the `tabline` slot).
    pub claims: Vec<String>,
}

/// One mounted, generation-owned declarative block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiBlock {
    slot: UiSlot,
    node: UiNode,
    version: u32,
}

impl UiBlock {
    /// Accepted slot this block was mounted into (canonical spelling).
    #[must_use]
    pub fn slot(&self) -> &'static str {
        self.slot.as_str()
    }

    /// Accepted slot this block was mounted into (typed).
    #[must_use]
    pub fn ui_slot(&self) -> UiSlot {
        self.slot
    }

    /// Current validated scene subtree.
    #[must_use]
    pub fn node(&self) -> &UiNode {
        &self.node
    }

    /// Monotonic version (1 at mount, incremented by update).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }
}

/// Bounded per-generation block registry (`SCN-4`/`SCN-5` numbers).
///
/// Handles are opaque, generation-owned integers encoded as
/// `(epoch << 32) | counter`: the registry lives inside one [`PluginServices`]
/// and the runtime pins `epoch` to the activation generation, so a handle from
/// a suspended, reloaded, or disposed generation is foreign to the next one
/// and `update` reports `false` instead of touching another generation's
/// blocks. Clearing the registry on suspend/dispose keeps the monotonic
/// counter, so handles never alias across a clear either.
///
/// The registry caps are host-owned fail-closed bounds for this bridge slice:
/// the accepted `SCN-4`/`SCN-5` budgets are terminal-wide (all plugins), and
/// the terminal-wide accounting is owned by the host composer. Exceeding a
/// registry cap fails closed with `E_UI_BLOCK_BUDGET` (`budget` class), a host
/// diagnostic pending an accepted stable plugin-visible code. The `SCN-4`
/// aggregate charges every retained string in a subtree (text, theme tokens,
/// command identifiers, argument keys, and string argument values), because
/// all of them stay resident in the registry (CTX-1340).
#[derive(Debug)]
pub struct UiBlocks {
    blocks: Vec<(i64, UiBlock)>,
    epoch: u32,
    next_counter: u32,
    /// Retained string bytes across every block (`Text` content, theme tokens,
    /// command identifiers, argument keys, and string argument values).
    aggregated_retained_bytes: usize,
}

/// Bits of the handle reserved for the generation epoch.
///
/// The epoch sits above bit 32, and bit 32 itself is reserved as the
/// terminal-slot scene discriminator (`bitty` CTX-1641): this registry never
/// sets it, so a v1 band handle can never be mistaken for a rich scene handle
/// by `ui.update`. Capped at 30 so every composed `i64` handle stays positive
/// (the surface types handles as opaque integers).
const UI_HANDLE_EPOCH_BITS: u32 = 30;

/// Mask selecting the generation epoch bits of a handle.
const UI_HANDLE_EPOCH_MASK: u32 = (1 << UI_HANDLE_EPOCH_BITS) - 1;

/// Position of the epoch field; bit 32 stays clear for the scene
/// discriminator.
const UI_HANDLE_EPOCH_SHIFT: u32 = 33;

impl Default for UiBlocks {
    fn default() -> Self {
        Self::new()
    }
}

impl UiBlocks {
    /// An empty registry (epoch `0`, first handle counter `1`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            epoch: 0,
            next_counter: 1,
            aggregated_retained_bytes: 0,
        }
    }

    /// Pin the generation epoch this registry mints handles for.
    ///
    /// Set once per activation/reload before the generation's `init.lua` runs;
    /// changing it after mounts would strand live handles, so the runtime only
    /// calls this on a freshly built registry.
    fn set_epoch(&mut self, epoch: u32) {
        self.epoch = epoch;
    }

    /// Drop every retained block (`suspend`/`dispose` invalidation) while
    /// keeping the epoch and the monotonic counter, so a handle minted before
    /// the clear can never alias one minted after it.
    fn clear(&mut self) {
        self.blocks.clear();
        self.aggregated_retained_bytes = 0;
    }

    /// Compose the next generation-owned handle, or fail closed when this
    /// generation has exhausted its counter (unreachable in practice).
    fn next_handle(&self) -> Result<i64, BridgeError> {
        if self.next_counter == u32::MAX {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block handle counter exhausted for this generation",
            ));
        }
        let epoch = i64::from(self.epoch & UI_HANDLE_EPOCH_MASK);
        Ok((epoch << UI_HANDLE_EPOCH_SHIFT) | i64::from(self.next_counter))
    }

    /// Number of retained blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether no block is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Look up one block by handle.
    #[must_use]
    pub fn get(&self, handle: i64) -> Option<&UiBlock> {
        self.blocks
            .iter()
            .find_map(|(candidate, block)| (*candidate == handle).then_some(block))
    }

    /// Iterate blocks in mount order.
    pub fn iter(&self) -> impl Iterator<Item = (i64, &UiBlock)> {
        self.blocks.iter().map(|(handle, block)| (*handle, block))
    }

    /// Retain one validated component, returning its generation-owned handle.
    fn mount(&mut self, slot: UiSlot, node: UiNode) -> Result<i64, BridgeError> {
        if self.blocks.len() >= UI_MAX_BLOCKS {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                format!("ui block registry is full ({UI_MAX_BLOCKS} blocks)"),
            ));
        }
        let bytes = node.retained_string_bytes();
        let total = self.aggregated_retained_bytes.saturating_add(bytes);
        if total > UI_MAX_AGGREGATED_TEXT_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block retained-string budget exceeded (2 MiB aggregated)",
            ));
        }
        let handle = self.next_handle()?;
        self.next_counter = self.next_counter.saturating_add(1);
        self.aggregated_retained_bytes = total;
        self.blocks.push((
            handle,
            UiBlock {
                slot,
                node,
                version: 1,
            },
        ));
        Ok(handle)
    }

    /// Replace one block's subtree; `Ok(false)` for a stale/foreign handle.
    fn update(&mut self, handle: i64, node: UiNode) -> Result<bool, BridgeError> {
        let Some(position) = self
            .blocks
            .iter()
            .position(|(candidate, _)| *candidate == handle)
        else {
            return Ok(false);
        };
        let old_bytes = self.blocks[position].1.node.retained_string_bytes();
        let total = self
            .aggregated_retained_bytes
            .saturating_sub(old_bytes)
            .saturating_add(node.retained_string_bytes());
        if total > UI_MAX_AGGREGATED_TEXT_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block retained-string budget exceeded (2 MiB aggregated)",
            ));
        }
        self.aggregated_retained_bytes = total;
        let block = &mut self.blocks[position].1;
        block.node = node;
        block.version = block.version.saturating_add(1);
        Ok(true)
    }

    /// Drop one block by handle, releasing its retained-string budget.
    ///
    /// Used to roll back a spec-acquired surface when capture acquisition
    /// fails, so a denied acquire leaves no orphan block behind. Returns
    /// whether a block was removed. The epoch and monotonic counter are
    /// kept, so removed handles never alias later ones.
    fn remove(&mut self, handle: i64) -> bool {
        let Some(position) = self
            .blocks
            .iter()
            .position(|(candidate, _)| *candidate == handle)
        else {
            return false;
        };
        let (_, block) = self.blocks.remove(position);
        self.aggregated_retained_bytes = self
            .aggregated_retained_bytes
            .saturating_sub(block.node.retained_string_bytes());
        true
    }
}

/// One published service record in the runtime service directory (LUA-OQ-8).
///
/// Published at activation from the provider manifest's `services.provided`
/// entry (version, schemas, and backing capability heads) plus the generation
/// capture (impl functions); version, schemas, and heads come from the
/// manifest, never from Lua. The provider VM is held weakly: dispose drops
/// the strong handle, so a stale consumer route upgrades to nothing and
/// fails closed with `E_SERVICE_GONE`.
///
/// A provided interface projects only already-existing Core capability heads
/// through a grant and never mints authority (dynapi gating, bitty#2005):
/// the manifest declares the heads, activation rejects unbacked provisions
/// and heads the provider was never granted, and consumers resolve only
/// when their own grant covers every head.
///
/// Cloned out of the directory before any VM invocation, so the directory
/// borrow never spans a (potentially re-entrant) provider call.
#[derive(Debug, Clone)]
pub struct ServiceRecord {
    /// Providing plugin id.
    pub provider: String,
    /// Activation generation that published the record.
    pub generation: u32,
    /// Service interface name.
    pub iface: String,
    /// Published interface version (exact SemVer from the manifest).
    pub version: String,
    /// Provided method names in impl-table order.
    pub methods: Vec<String>,
    /// Method name to stashed impl function (generation-scoped to the
    /// provider VM; functions never cross as values).
    pub funcs: BTreeMap<String, StashedFunction>,
    /// Optional JSON Schema for call arguments (manifest table form).
    pub args_schema: Option<String>,
    /// Optional JSON Schema for call results (manifest table form).
    pub result_schema: Option<String>,
    /// Backing Core capability heads from the manifest's `services.provided`
    /// entry (never empty on a published record; validated at install and
    /// re-checked at record time).
    pub heads: Vec<String>,
    /// Provider VM (weak: dispose invalidates without directory coordination).
    pub vm: Weak<RefCell<LuaVm>>,
    /// Set while the provider is suspended; suspended records resolve and
    /// serve nothing until resume republishes them in place.
    pub suspended: bool,
}

/// One brokered service call for the observability ledger (dynapi 4/4 input).
///
/// Recorded by the [`HostServices::service_call`] broker after every gate
/// passes and before the provider VM runs: the provider owner, the
/// negotiated interface version, plus the backing head(s) the call
/// projects. Plain metadata only (no args, no results); the argv-first
/// invocation, deadlines, and per-call budgets on the execution path
/// itself are unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCallRecord {
    /// Providing plugin id that served the call.
    pub provider: String,
    /// Service interface name.
    pub iface: String,
    /// Negotiated interface version that served the call (dynapi
    /// negotiation, bitty#2006).
    pub version: String,
    /// Called method name.
    pub method: String,
    /// Backing Core capability heads projected by this call.
    pub heads: Vec<String>,
}

/// Bounded brokered-call ledger (`RC-8` rate governance; overflow drops newest).
///
/// Runtime-shared like [`ServiceDirectory`]: every generation's services hold
/// the same handle, so the ledger spans plugins and survives reload. Entries
/// are snapshots (no VM references); suspend/dispose never prune history.
#[derive(Debug)]
pub struct ServiceCallLedger {
    items: VecDeque<ServiceCallRecord>,
    capacity: usize,
    dropped: u64,
}

/// Maximum brokered service calls retained for observability.
///
/// Precedent: the notification queue bounds one generation's async surface;
/// the call ledger spans the runtime, so it keeps an order of magnitude
/// more entries while staying headless-bounded.
pub const SERVICE_CALL_LEDGER_CAPACITY: usize = 256;

impl ServiceCallLedger {
    /// Create a bounded ledger.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Record one dispatched call; returns whether it was retained.
    pub fn push(&mut self, record: ServiceCallRecord) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(record);
        true
    }

    /// Cloned retained calls in dispatch order.
    #[must_use]
    pub fn calls(&self) -> Vec<ServiceCallRecord> {
        self.items.iter().cloned().collect()
    }

    /// Number retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the ledger is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Live per-runtime service directory (LUA-OQ-8).
///
/// Owned by [`PluginRuntime`](super::PluginRuntime) and shared with every
/// generation's [`PluginServices`]: activation publishes, suspend parks,
/// resume restores, dispose/reload revokes. Keyed by interface with one
/// record per (provider, version), so side-by-side versions of one
/// interface coexist and resolution picks deterministically (highest
/// satisfying version, ties broken by provider id).
#[derive(Debug, Default)]
pub struct ServiceDirectory {
    records: BTreeMap<String, Vec<ServiceRecord>>,
}

impl ServiceDirectory {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish one activation record, replacing the same (provider,
    /// version) prior record for the interface (a provider publishes each
    /// declared (interface, version) once per generation; side-by-side
    /// versions coexist, and re-activation after revoke starts clean).
    pub fn publish(&mut self, record: ServiceRecord) {
        let entry = self.records.entry(record.iface.clone()).or_default();
        entry.retain(|existing| {
            existing.provider != record.provider || existing.version != record.version
        });
        entry.push(record);
    }

    /// Park every record of `provider` (suspend): resolution skips them and
    /// live handles fail closed with `E_SERVICE_GONE` until resume.
    pub fn suspend_provider(&mut self, provider: &str) {
        for records in self.records.values_mut() {
            for record in records {
                if record.provider == provider {
                    record.suspended = true;
                }
            }
        }
    }

    /// Restore every parked record of `provider` (resume).
    pub fn resume_provider(&mut self, provider: &str) {
        for records in self.records.values_mut() {
            for record in records {
                if record.provider == provider {
                    record.suspended = false;
                }
            }
        }
    }

    /// Drop every record of `provider` (dispose/reload/failed activation).
    pub fn revoke_provider(&mut self, provider: &str) {
        for records in self.records.values_mut() {
            records.retain(|record| record.provider != provider);
        }
        self.records.retain(|_, records| !records.is_empty());
    }

    /// Cloned active (non-suspended) records for `iface`, in publish order.
    #[must_use]
    pub fn active_for(&self, iface: &str) -> Vec<ServiceRecord> {
        self.records
            .get(iface)
            .map(|records| {
                records
                    .iter()
                    .filter(|record| !record.suspended)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Cloned active record for (`iface`, `provider`, `version`), if
    /// published. The version pin keeps side-by-side records from serving
    /// through each other's handles: a consumer handle always drives the
    /// exact record its resolution selected, never a same-provider sibling.
    #[must_use]
    pub fn find_active(&self, iface: &str, provider: &str, version: &str) -> Option<ServiceRecord> {
        self.records.get(iface).and_then(|records| {
            records
                .iter()
                .find(|record| {
                    record.provider == provider && record.version == version && !record.suspended
                })
                .cloned()
        })
    }

    /// Total published records (suspended included; diagnostics and tests).
    #[must_use]
    pub fn published_count(&self) -> usize {
        self.records.values().map(Vec::len).sum()
    }
}

/// Truncate a provider failure message to [`SERVICE_FAILED_MESSAGE_LIMIT`]
/// characters (char-boundary safe).
fn truncate_service_message(message: String) -> String {
    if message.chars().count() <= SERVICE_FAILED_MESSAGE_LIMIT {
        return message;
    }
    message.chars().take(SERVICE_FAILED_MESSAGE_LIMIT).collect()
}

/// `E_SERVICE_RESOLUTION` for an unresolvable consumer lookup.
fn service_resolution_error(iface: &str, detail: String) -> BridgeError {
    BridgeError::new(
        "runtime",
        "E_SERVICE_RESOLUTION",
        format!("service '{iface}' cannot be resolved: {detail}"),
    )
}

/// Maximum versions named in a resolution-failure diagnostic.
///
/// Bounds the error detail: validated versions are at most 64 bytes each,
/// so the listing stays small while still naming real candidates.
const MAX_RESOLUTION_LISTED_VERSIONS: usize = 8;

/// Render the visible-version listing for a resolution-failure diagnostic
/// (dynapi negotiation, bitty#2006).
///
/// Callers pass grant-visible versions only, so the listing never leaks an
/// ungranted provider; beyond the cap it names the first entries plus the
/// remaining count.
fn list_visible_versions(versions: BTreeSet<String>) -> String {
    if versions.is_empty() {
        return "no versions available".to_string();
    }
    let total = versions.len();
    let listed: Vec<String> = versions
        .into_iter()
        .take(MAX_RESOLUTION_LISTED_VERSIONS)
        .collect();
    let rest = total.saturating_sub(listed.len());
    let mut text = listed.join(", ");
    if rest > 0 {
        text.push_str(&format!(" (and {rest} more)"));
    }
    text
}

/// `E_SERVICE_GONE` for a dead, stale, or parked provider record.
fn service_gone_error(detail: String) -> BridgeError {
    BridgeError::new("runtime", "E_SERVICE_GONE", detail)
}

/// One published command record in the runtime command directory (CTX-1035).
///
/// Published at activation from the generation capture (stashed `run`
/// function) plus the effective schemas (the manifest's
/// `lazy.commands` table-form entry wins when present, otherwise the
/// `commands.register` tables — mirroring the service version/schema
/// precedence). The owner VM is held weakly: dispose drops the strong
/// handle, so a stale record upgrades to nothing and fails closed with
/// `E_COMMAND_GONE`.
///
/// Cloned out of the directory before any VM invocation, so the directory
/// borrow never spans a (potentially re-entrant) callee call.
#[derive(Debug, Clone)]
pub struct CommandRecord {
    /// Owning plugin id.
    pub owner: String,
    /// Unqualified command id.
    pub id: String,
    /// Qualified command name (`owner:id`).
    pub qualified: String,
    /// Activation generation that published the record.
    pub generation: u32,
    /// Bounded display title.
    pub title: String,
    /// Effective JSON Schema for invocation arguments (when declared).
    pub args_schema: Option<String>,
    /// Effective JSON Schema for invocation results (when declared).
    pub result_schema: Option<String>,
    /// Stashed `run` function (generation-scoped to the owner VM; functions
    /// never cross as values).
    pub func: StashedFunction,
    /// Owner VM (weak: dispose invalidates without directory coordination).
    pub vm: Weak<RefCell<LuaVm>>,
    /// Set while the owner is suspended; suspended records list and serve
    /// nothing until resume republishes them in place.
    pub suspended: bool,
}

/// Live per-runtime command directory (CTX-1035).
///
/// Owned by [`PluginRuntime`](super::PluginRuntime) and shared with every
/// generation's [`PluginServices`]: activation publishes, suspend parks,
/// resume restores, dispose/reload revokes. Keyed by qualified name with one
/// record per command (qualified names are registry-unique by construction),
/// so any generation can invoke any active command through the host —
/// including the palette driving `bitty-featured.devtools:plugins` — while
/// undeclared and foreign names resolve to nothing and fail closed.
#[derive(Debug, Default)]
pub struct CommandDirectory {
    records: BTreeMap<String, CommandRecord>,
}

impl CommandDirectory {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish one activation record, replacing the same qualified name's
    /// prior record (re-activation after revoke starts clean).
    pub fn publish(&mut self, record: CommandRecord) {
        self.records.insert(record.qualified.clone(), record);
    }

    /// Park every record of `owner` (suspend): listing skips them and live
    /// invocations fail closed with `E_COMMAND_GONE` until resume.
    pub fn suspend_owner(&mut self, owner: &str) {
        for record in self.records.values_mut() {
            if record.owner == owner {
                record.suspended = true;
            }
        }
    }

    /// Restore every parked record of `owner` (resume).
    pub fn resume_owner(&mut self, owner: &str) {
        for record in self.records.values_mut() {
            if record.owner == owner {
                record.suspended = false;
            }
        }
    }

    /// Drop every record of `owner` (dispose/reload/failed activation).
    pub fn revoke_owner(&mut self, owner: &str) {
        self.records.retain(|_, record| record.owner != owner);
    }

    /// Cloned active (non-suspended) record for `qualified`, if published.
    #[must_use]
    pub fn find_active(&self, qualified: &str) -> Option<CommandRecord> {
        self.records.get(qualified).and_then(|record| {
            if record.suspended {
                None
            } else {
                Some(record.clone())
            }
        })
    }

    /// Whether any record (suspended included) names `qualified`.
    ///
    /// Lets the invoke path distinguish "never registered" (`E_COMMAND_UNKNOWN`)
    /// from "registered but unavailable" (`E_COMMAND_GONE`).
    #[must_use]
    pub fn knows(&self, qualified: &str) -> bool {
        self.records.contains_key(qualified)
    }

    /// Cloned active records in catalog order (owner, id).
    #[must_use]
    pub fn active_records(&self) -> Vec<CommandRecord> {
        let mut records: Vec<CommandRecord> = self
            .records
            .values()
            .filter(|record| !record.suspended)
            .cloned()
            .collect();
        records.sort_by(|a, b| (&a.owner, &a.id).cmp(&(&b.owner, &b.id)));
        records
    }

    /// Total published records (suspended included; diagnostics and tests).
    #[must_use]
    pub fn published_count(&self) -> usize {
        self.records.len()
    }
}

/// `E_COMMAND_UNKNOWN` for a qualified name no generation ever registered.
///
/// Deny-by-default: undeclared and foreign-qualified names are refused here,
/// before any callee code could run.
fn command_unknown_error(qualified: &str) -> BridgeError {
    BridgeError::new(
        "validation",
        "E_COMMAND_UNKNOWN",
        format!("command '{qualified}' is not registered"),
    )
}

/// `E_COMMAND_GONE` for a registered-but-unavailable command record.
///
/// The owner is suspended, disposed, reloaded, or failed: the name resolved
/// once, but no live generation serves it now.
fn command_gone_error(qualified: &str) -> BridgeError {
    BridgeError::new(
        "runtime",
        "E_COMMAND_GONE",
        format!("command '{qualified}' is unavailable"),
    )
}

/// `E_COMMAND_INVALID` for an args/result schema violation.
///
/// Args violations are refused before any callee code runs; result
/// violations contain a misbehaving callee after it ran.
fn command_invalid_error(qualified: &str, detail: &str) -> BridgeError {
    BridgeError::new(
        "validation",
        "E_COMMAND_INVALID",
        format!("command '{qualified}' {detail}"),
    )
}

/// `E_COMMAND_FAILED` for a callee that raised, suspended, or was busy.
///
/// Failures are contained to the invocation: the host never panics, and a
/// re-entrant call into the already-executing owner VM (including a command
/// invoking itself) fails closed here instead of aliasing the VM.
fn command_failed_error(qualified: &str, detail: String) -> BridgeError {
    BridgeError::new(
        "runtime",
        "E_COMMAND_FAILED",
        format!("command '{qualified}' failed: {detail}"),
    )
}

/// Encode the invocation-args document for schema validation (CTX-1035).
///
/// Lua has one table type, so an empty table is ambiguous: the bridge encodes
/// it as `[]`, but every command args schema in the wild (including the
/// devtools empty-object contract) describes an object. Canonicalize `nil`
/// and the empty table to the empty object `{}`; every other value encodes
/// as-is. A non-empty array stays an array and fails an object schema
/// loudly, exactly like a mistyped table.
pub(crate) fn command_args_document(args: &LuaValue) -> String {
    match args {
        LuaValue::Nil => "{}".to_string(),
        LuaValue::Table(pairs) if pairs.is_empty() => "{}".to_string(),
        _ => store::encode_json(args),
    }
}

/// Map a Core provider failure onto existing typed bridge errors (W-29).
///
/// No new error code is introduced: capability stays `E_CAPABILITY_DENIED`,
/// capacity stays `E_DEF_LIMIT`, stale stays `E_UI_NOT_OWNER`, and malformed
/// names stay `E_DEF_INVALID`.
fn map_targets_provider_error(error: ProviderError) -> BridgeError {
    match error {
        ProviderError::CapabilityDenied { name } => {
            BridgeError::capability_denied(&format!("ui.overlay ({name})"))
        }
        ProviderError::TooManyProviders { max, current } => BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("too many target lenses: max {max}, current {current}"),
        ),
        ProviderError::TooManyTargets { max, current } => BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("too many snapshot targets: max {max}, current {current}"),
        ),
        ProviderError::StaleSnapshot(detail) => BridgeError::new(
            "runtime",
            E_UI_NOT_OWNER,
            format!("stale snapshot: {detail}"),
        ),
        other => BridgeError::new("validation", "E_DEF_INVALID", other.to_string()),
    }
}

/// Map a Core label failure onto existing typed bridge errors (W-29).
fn map_targets_label_error(error: bitty_ui::LabelError) -> BridgeError {
    match error {
        bitty_ui::LabelError::TooManyTargets {
            requested,
            capacity,
        } => BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("too many targets for labels: requested {requested}, capacity {capacity}"),
        ),
        other => BridgeError::new("validation", "E_DEF_INVALID", other.to_string()),
    }
}

/// Map a Core dispatch failure onto existing typed bridge errors (W-29).
///
/// Unknown/expired labels are `E_DEF_INVALID`; stale targets are
/// `E_UI_NOT_OWNER` (the handle is no longer owned by the live registry);
/// over-capacity bindings are `E_DEF_LIMIT`.
fn map_targets_dispatch_error(error: DispatchError) -> BridgeError {
    match error {
        DispatchError::UnknownLabel(detail) => {
            BridgeError::new("validation", "E_DEF_INVALID", detail)
        }
        DispatchError::StaleTarget(detail) => BridgeError::new("runtime", E_UI_NOT_OWNER, detail),
        DispatchError::TooManyBindings { max, current } => BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("too many bindings: max {max}, current {current}"),
        ),
    }
}

/// Surface kind of a resolved target as a Lua string (W-29).
fn targets_kind_string(target: &TargetRef) -> String {
    match target {
        TargetRef::Panel(_) => "panel".to_string(),
        TargetRef::Workspace(_) => "workspace".to_string(),
        TargetRef::CommandBlock(_) => "block".to_string(),
        TargetRef::UiNode(_) => "node".to_string(),
        TargetRef::Link(_) => "link".to_string(),
    }
}

/// Stable semantic id of a resolved target within its kind (W-29 successor).
fn target_raw_id(target: &TargetRef) -> u64 {
    match target {
        TargetRef::Panel(r) => r.id.0,
        TargetRef::Workspace(r) => r.id.0,
        TargetRef::CommandBlock(r) => r.id.0,
        TargetRef::UiNode(r) => r.id.0,
        TargetRef::Link(r) => r.id.0,
    }
}

/// Generation captured inside a resolved target (W-29 successor).
fn target_generation(target: &TargetRef) -> u64 {
    match target {
        TargetRef::Panel(r) => r.generation,
        TargetRef::Workspace(r) => r.generation,
        TargetRef::CommandBlock(r) => r.generation,
        TargetRef::UiNode(r) => r.generation,
        TargetRef::Link(r) => r.generation,
    }
}

/// Build one Core offer from a Lua `(kind, id)` pair (W-29).
///
/// Kinds are the five `TargetRef` variants; `ViewId` is deliberately absent.
/// Malformed kinds/ids fail closed with existing `E_DEF_INVALID`.
fn targets_offer(kind: &str, id: u64) -> Result<ProviderTarget, BridgeError> {
    match kind {
        "panel" => Ok(ProviderTarget::Panel(PanelId::new(id))),
        "workspace" => Ok(ProviderTarget::Workspace(WorkspaceId::new(id))),
        "block" => Ok(ProviderTarget::CommandBlock(CommandBlockId::new(id))),
        "node" => Ok(ProviderTarget::UiNode(UiNodeId::new(id))),
        "link" => Ok(ProviderTarget::Link(LinkId::new(id))),
        _ => Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!("unknown target kind '{kind}'"),
        )),
    }
}

/// Per-generation host services for one plugin.
/// Runtime-shared overlay peer table for lazy-expiry disposal (CTX-0973).
///
/// Holds weak handles to every live generation's services so an acquire that
/// lazily expires another generation's session can dispose that session's
/// transient spec surface synchronously. Entries are weak: a generation gone
/// by disposal time simply has nothing to dispose. The table is bounded by
/// the live generation count; dead entries are pruned on activation and on
/// disposal.
pub type OverlayPeers = Rc<RefCell<BTreeMap<String, Vec<Weak<PluginServices>>>>>;

/// Runtime-shared present-time annotation channel (W-29 successor).
///
/// The current generation publishes its batched [`HintAnnotationLayer`] here
/// on session start, replaces it on presentation sync, and clears it on
/// dispatch/cancel/timeout/unload. The application reads it once per tick and
/// hands it to the scene/compositor; plugins never draw into the layer.
pub type SharedTargetAnnotations = Rc<RefCell<Option<Rc<HintAnnotationLayer>>>>;

#[allow(clippy::type_complexity)]
pub struct PluginServices {
    plugin_id: String,
    store: RefCell<PluginStore>,
    settings: Rc<dyn SettingsSource>,
    /// Manifest-declared settings keys for this generation (CTX-1195).
    ///
    /// `bitty.settings.get` serves only these keys; any other key fails
    /// closed with `E_SETTINGS_UNDECLARED`, so settings are never a side
    /// channel between plugins. Empty by default (deny-by-default: a
    /// generation with no `[settings]` declaration reads nothing).
    declared_settings: RefCell<BTreeSet<String>>,
    snapshot: Rc<dyn SnapshotSource>,
    notifications: Rc<RefCell<NotificationQueue>>,
    terminal_read: bool,
    /// Whether this generation holds `terminal.input.submit` (CTX-1239).
    ///
    /// Fixed at activation from the grant snapshot (declared capabilities
    /// intersected with consent); absent grants fail closed with
    /// `E_CAPABILITY_DENIED` at call time and with a nil `submit` key at
    /// the bridge (see `has_terminal_submit`).
    terminal_submit_granted: Cell<bool>,
    /// Per-generation submit byte window (CTX-1239, P0-AC-014 attribution).
    ///
    /// Charged only on enqueue success; denials and queue-full drops leave
    /// it untouched, exactly like the native operation's probe-then-commit.
    submit_budget: RefCell<SubmitBudget>,
    /// Runtime-shared submit intent queue (CTX-1239).
    ///
    /// Shared at activation only with `terminal.input.submit` generations
    /// (no queue without grant); `None` leaves granted calls failing closed
    /// with `E_NOT_IMPLEMENTED`, like every other verb without a host.
    submit_queue: RefCell<Option<Rc<RefCell<TerminalSubmitQueue>>>>,
    platform_notify: bool,
    spawn_git: Cell<bool>,
    spawn_backend: RefCell<Option<SpawnHandler>>,
    ui_access: RefCell<UiAccess>,
    ui_blocks: RefCell<UiBlocks>,
    /// Core-owned `terminal`-slot scene host (bitty#2785).
    ///
    /// Installed only for `ui.rich` generations; absent means a rich mount
    /// fails closed with `E_UI_UNAVAILABLE`, exactly like every other verb
    /// without a host. The host owns the scene panel, the admitted scenes,
    /// and their handles; this generation only calls it and releases its own
    /// scenes through `clear_ui_blocks`.
    ui_scene_backend: RefCell<Option<Rc<crate::scene_host::SceneHost>>>,
    /// Activation generation this registry mints handles for (`set_ui_epoch`).
    ///
    /// The scene host takes it as the block's attribution generation and as
    /// the release key on `clear_ui_blocks`, so a reload can never release the
    /// next generation's scene.
    ui_epoch: Cell<u32>,
    overlay_capture: RefCell<Option<Rc<RefCell<OverlayCapture>>>>,
    /// Runtime-shared presentation-only overlay session (CTX-1431, #2406).
    ///
    /// `None` until the runtime wires the shared state; every show then
    /// fails closed with `E_UI_UNAVAILABLE`. Presentation is paint-only: it
    /// never claims capture and never sets the modal authority.
    presentation_overlay: RefCell<Option<Rc<RefCell<PresentationOverlay>>>>,
    overlay_peers: RefCell<Option<OverlayPeers>>,
    /// Handles this generation mounted through spec acquire (CTX-0941).
    ///
    /// A spec acquire mounts its transient surface directly into the
    /// generation registry; the surface must be unmounted when its session
    /// ends (release, expiry, unload, crash) or the next session would
    /// present the previous session's content and every cycle would leak
    /// one `UI_MAX_BLOCKS` slot. Mechanism-path blocks (mounted through
    /// `ui.mount`) are never recorded here and survive release as the
    /// plugin's retained content.
    spec_overlay_handles: RefCell<BTreeSet<i64>>,
    /// Runtime-shared target registry (W-29, CTX-0942, existing `TargetRegistry`).
    ///
    /// One registry spans every generation so generations bump and stale
    /// handles by construction. `None` until the runtime wires the shared
    /// state; unwired calls fail closed with `E_UI_UNAVAILABLE`.
    target_registry: RefCell<Option<Rc<RefCell<TargetRegistry>>>>,
    /// Runtime-shared generic lenses (W-29, existing `DerivedProvider`).
    ///
    /// Each entry is `(plugin_id, lens)`; the mediator is rebuilt from the
    /// Core provider plus these lenses on every cold-path collection (the
    /// Core mediator has no removal API). Both `plugin` and `derived` tiers
    /// map onto the existing `Derived` lens (no `Plugin`-tier source exists
    /// in `bitty-ui`; reuse avoids any new targeting type).
    target_lenses: RefCell<Option<Rc<RefCell<Vec<(String, DerivedProvider)>>>>>,
    /// Runtime-shared label allocator (W-29, existing `LabelAllocator`).
    label_allocator: RefCell<Option<Rc<RefCell<LabelAllocator>>>>,
    /// Per-generation label-to-command bindings (W-29, existing `TargetDispatcher`).
    ///
    /// Built on session start, cleared on cancel/dispatch/unload. Never
    /// shared across generations and never published to the Event Bus.
    target_dispatcher: RefCell<TargetDispatcher>,
    /// Overlay handle of this generation's active targeting session, if any.
    ///
    /// Ownership is the existing W-28 overlay capture owner (no new session
    /// type); this handle only coordinates dispatcher lifetime with capture
    /// release. `None` means no session.
    target_session_overlay: RefCell<Option<i64>>,
    /// Read-only Core target/anchor source (W-29 successor).
    ///
    /// Wired at activation from the application's live publisher; `None`
    /// leaves the Core provider empty (plugin lenses still work). Reads are
    /// cold-path collection only.
    target_source: RefCell<Option<Rc<dyn TargetSource>>>,
    /// Runtime-shared present-time annotation channel (W-29 successor).
    ///
    /// `None` leaves the channel unwired; session start then fails closed with
    /// `E_UI_UNAVAILABLE` rather than presenting an unowned batch.
    target_annotations: RefCell<Option<SharedTargetAnnotations>>,
    /// The exact layer this generation published into the shared channel.
    ///
    /// Identity is the `Rc` allocation, so a later generation's layer can
    /// never be mistaken for this one's: cleanup clears the channel only when
    /// the published handle still matches. This also makes a stale owner
    /// (expired capture, then a new owner) harmless.
    target_annotations_published: RefCell<Option<Rc<HintAnnotationLayer>>>,
    env_grants: RefCell<BTreeSet<String>>,
    env_source: RefCell<Rc<dyn EnvSource>>,
    /// Safe-mode flag for this generation (CTX-0941, W-28 host API).
    ///
    /// Wired once at activation from the runtime config. While set, every
    /// focusable-overlay capture call fails closed with `E_UI_UNAVAILABLE`:
    /// safe mode never presents a focusable overlay and never starts a
    /// capture session. Release stays grant-free so cleanup can never wedge.
    safe_mode: Cell<bool>,
    service_provided: RefCell<Vec<ProvidedService>>,
    service_required: RefCell<Vec<(String, String)>>,
    service_directory: RefCell<Option<Rc<RefCell<ServiceDirectory>>>>,
    /// Runtime-shared brokered-call ledger (dynapi observability input).
    ///
    /// Wired once at activation; without it calls still serve (the ledger
    /// is observe-only) but nothing is recorded.
    service_call_ledger: RefCell<Option<Rc<RefCell<ServiceCallLedger>>>>,
    command_directory: RefCell<Option<Rc<RefCell<CommandDirectory>>>>,
    debug_inspect: Cell<bool>,
    debug_trace: Cell<bool>,
    /// Whether this generation holds `runtime.inspect` (issue #2396).
    ///
    /// Read never implies `runtime.configure`, and `runtime.configure` never
    /// implies this head; each is granted independently.
    runtime_inspect_granted: Cell<bool>,
    /// Prebuilt, already bounded snapshot for `bitty.runtime.inspect()`.
    ///
    /// Built by the application on its cold path (no file, process, network,
    /// or environment access on the call path) and installed before
    /// activation. `None` leaves granted calls failing closed with
    /// `E_NOT_IMPLEMENTED`.
    runtime_inspect_snapshot: RefCell<Option<RuntimeInspectSnapshot>>,
    /// Per-generation call budget (`16 calls/s`, issue #2396).
    runtime_inspect_budget: RefCell<InspectCallBudget>,
    /// Whether this generation holds `runtime.keymaps.read` (issue #2405).
    ///
    /// Deny by default; `runtime.inspect`, `runtime.configure`, and the UI
    /// heads never imply it. A generation under safe policy is denied
    /// regardless of the recorded grant.
    keymaps_read: Cell<bool>,
    /// Live resolved-keymap source for `bitty.keymaps.list()`.
    ///
    /// Installed for every generation; it carries no authority, and the
    /// per-generation grant plus the safe-policy check decide every call.
    /// `None` leaves granted calls failing closed with `E_NOT_IMPLEMENTED`.
    keymap_source: RefCell<Option<Rc<dyn KeymapSource>>>,
    /// Per-generation call budget (`16 calls/s`, issue #2405).
    keymap_budget: RefCell<KeymapCallBudget>,
    granted_capabilities: RefCell<Vec<String>>,
    declared_events: RefCell<BTreeSet<String>>,
    debug_view: RefCell<Option<Rc<RefCell<DebugView>>>>,
    trace_hub: RefCell<Option<Rc<RefCell<TraceHub>>>>,
    workspace_read: Cell<bool>,
    workspace_control: Cell<bool>,
    workspace_source: RefCell<Option<Rc<dyn WorkspaceSource>>>,
    workspace_requests: RefCell<Option<Rc<RefCell<WorkspaceRequestQueue>>>>,
    /// Composition-root history snapshot provider (CTX-1352, issue #2247).
    ///
    /// `None` leaves every granted history query failing closed with the
    /// typed `E_HISTORY_UNAVAILABLE` denial. Presence never grants access:
    /// the per-source capability head stays the first gate.
    history_provider: RefCell<Option<Rc<dyn HistoryProvider>>>,
    /// Per-generation Core history gate (grants, scopes, capture, safe mode,
    /// bounds, budgets). One gate per generation keeps rate/aggregate usage
    /// attributed to this plugin only (`P0-AC-014`).
    history_gate: RefCell<HistoryGate>,
    /// Query scopes already granted to the gate this rate window.
    ///
    /// The accepted history heads carry no grant parameter (scope travels
    /// with the host call), so the gate grant extent is the authorized call
    /// scope. Remembering issued scopes avoids growing the gate's grant list
    /// on every repeated query; the set is capped at
    /// [`HISTORY_GRANT_SCOPE_CACHE_MAX`] and overflow clears grants while
    /// keeping rate usage. Cleared when the rate window advances.
    history_grant_scopes: RefCell<BTreeSet<(HistorySource, Option<String>, Option<String>)>>,
    /// Start of the current history rate window.
    history_window_start: Cell<Instant>,
    panel_create_granted: Cell<bool>,
    panel_focus_granted: Cell<bool>,
    panel_create_backend: RefCell<Option<PanelCreateHandler>>,
    panel_pin_backend: RefCell<Option<PanelPinHandler>>,
    /// Whether this generation holds `platform.image-file` (CTX-1169).
    image_read_granted: Cell<bool>,
    /// Approved wallpaper roots for `bitty.image.read` (CTX-1169).
    ///
    /// Shared at activation only with `platform.image-file` generations from
    /// the live `background_image_roots`; `None` leaves granted reads failing
    /// closed with `E_NOT_IMPLEMENTED`, like every other verb without a host.
    image_roots: RefCell<Option<Vec<PathBuf>>>,
    /// Whether this generation holds `runtime.configure` (CTX-1169).
    theme_configure_granted: Cell<bool>,
    /// Runtime-shared `bitty.theme.preview`/`confirm` intent queue (CTX-1169).
    /// Shared at activation only with `runtime.configure` generations; `None`
    /// leaves granted calls failing closed with `E_NOT_IMPLEMENTED`.
    theme_requests: RefCell<Option<Rc<RefCell<ThemeRequestQueue>>>>,
    /// Whether this generation holds `config.manage` (CTX-1613, bitty#2393).
    ///
    /// Write-only: `runtime.inspect` never implies it and it never implies a
    /// read. Safe mode denies regardless of the recorded grant.
    config_manage_granted: Cell<bool>,
    /// Per-generation approval tokens, call budget, and byte budget.
    config_gate: RefCell<ConfigWriteGate>,
    /// Durable user-layer backend for `config.set`.
    ///
    /// Installed only for a granted generation (no backend without grant);
    /// `None` leaves granted calls failing closed with `E_CONFIG_UNAVAILABLE`.
    /// The backend returns only a path-free failure class, so plugin
    /// diagnostics never carry host paths.
    config_backend: RefCell<Option<Rc<dyn ConfigWriteBackend>>>,
    /// Runtime-shared `bitty.fx.pulse` intent queue (CTX-1139). Shared at
    /// activation only with `panel.focus` generations; `None` leaves granted
    /// calls failing closed with `E_NOT_IMPLEMENTED`.
    fx_requests: RefCell<Option<Rc<RefCell<FxPulseQueue>>>>,
    /// Runtime-shared native sensor host (CTX-1199, bitty#2019/#2011/#2016).
    ///
    /// Shared at activation with every generation; the broker dispatches
    /// `bitty-terminal.sensors:system.*.get()` through it host-side (no VM).
    /// `None` leaves native sensor calls failing closed with
    /// `E_SERVICE_GONE`, like a parked provider.
    sensor_host: RefCell<Option<Rc<RefCell<super::system_sensors::host::SensorHost>>>>,
    /// Runtime-shared native device host (CTX-1238, issue #2100).
    ///
    /// Shared at activation with every generation; the broker dispatches
    /// `bitty-terminal.devices:*` through it host-side (no VM).
    /// `None` leaves native device calls failing closed with
    /// `E_SERVICE_GONE`, like a parked provider.
    device_host: RefCell<Option<Rc<RefCell<super::device_providers::host::DeviceHost>>>>,
    /// Runtime-shared native audio host (CTX-1238, issue #2100).
    ///
    /// Shared at activation with every generation; the broker dispatches
    /// `bitty-terminal.audio:audio.*` through it host-side (no VM).
    /// `None` leaves native audio calls failing closed with
    /// `E_SERVICE_GONE`, like a parked provider.
    audio_host: RefCell<Option<Rc<RefCell<super::audio::AudioHost>>>>,
    /// Real-filesystem backend for `bitty.fs.list` (CTX-1348).
    ///
    /// Shared at activation with every generation; the call-time grant gate
    /// in [`HostServices::fs_list`] consults this generation's own `fs.read:`
    /// snapshot, so the backend itself carries no authority. `None` leaves
    /// the verb failing closed with `E_NOT_IMPLEMENTED`, like every other
    /// host-mediated verb without a wired backend.
    fs_backend: RefCell<Option<Rc<FsListBackend>>>,
    /// Per-plugin window accounting for `bitty.fs.list` (CTX-1348,
    /// `P0-AC-014`).
    ///
    /// This gate holds no grants: authorization stays in [`FsListBackend`]
    /// against this generation's own snapshot. Only the usage map, accepted
    /// caps, and the typed over-bound denial are used here, so an exhausted
    /// window denies with `E_FS_OVER_BOUND` before any directory I/O.
    fs_gate: RefCell<FsGate>,
    /// Start of the current `fs.list` rate window.
    fs_window_start: Cell<Instant>,
    /// Runtime-shared bounded component request surface (CTX-2254,
    /// bitty#2254).
    ///
    /// Shared with every generation; authorization stays with the runtime and
    /// the per-generation capability snapshot below, so the queue carries no
    /// authority. `None` leaves `bitty.component.*` failing closed with
    /// `E_NOT_IMPLEMENTED`.
    component_queue: RefCell<Option<Rc<RefCell<ComponentRequestQueue>>>>,
    /// Manifest generation this services object belongs to (CTX-2254).
    ///
    /// Set once at activation from the parsed manifest; component requests
    /// carry it so the runtime can refuse a stale generation instead of
    /// serving it under the new one.
    component_manifest_hash: RefCell<Option<String>>,
}

impl PluginServices {
    /// Create services for `plugin_id` with an explicit grant snapshot.
    #[must_use]
    pub fn new(
        plugin_id: impl Into<String>,
        store: PluginStore,
        settings: Rc<dyn SettingsSource>,
        snapshot: Rc<dyn SnapshotSource>,
        notifications: Rc<RefCell<NotificationQueue>>,
        terminal_read: bool,
        platform_notify: bool,
    ) -> Self {
        let plugin_id: String = plugin_id.into();
        let submit_budget = SubmitBudget::new(&plugin_id, TERMINAL_SUBMIT_WINDOW_BYTES);
        Self {
            plugin_id,
            store: RefCell::new(store),
            settings,
            declared_settings: RefCell::new(BTreeSet::new()),
            snapshot,
            notifications,
            terminal_read,
            terminal_submit_granted: Cell::new(false),
            submit_budget: RefCell::new(submit_budget),
            submit_queue: RefCell::new(None),
            platform_notify,
            spawn_git: Cell::new(false),
            spawn_backend: RefCell::new(None),
            ui_access: RefCell::new(UiAccess::default()),
            ui_blocks: RefCell::new(UiBlocks::new()),
            ui_scene_backend: RefCell::new(None),
            ui_epoch: Cell::new(0),
            overlay_capture: RefCell::new(None),
            presentation_overlay: RefCell::new(None),
            overlay_peers: RefCell::new(None),
            spec_overlay_handles: RefCell::new(BTreeSet::new()),
            target_registry: RefCell::new(None),
            target_lenses: RefCell::new(None),
            label_allocator: RefCell::new(None),
            target_dispatcher: RefCell::new(TargetDispatcher::new()),
            target_session_overlay: RefCell::new(None),
            target_source: RefCell::new(None),
            target_annotations: RefCell::new(None),
            target_annotations_published: RefCell::new(None),
            env_grants: RefCell::new(BTreeSet::new()),
            env_source: RefCell::new(Rc::new(EmptyEnv)),
            safe_mode: Cell::new(false),
            service_provided: RefCell::new(Vec::new()),
            service_required: RefCell::new(Vec::new()),
            service_directory: RefCell::new(None),
            service_call_ledger: RefCell::new(None),
            command_directory: RefCell::new(None),
            debug_inspect: Cell::new(false),
            debug_trace: Cell::new(false),
            runtime_inspect_granted: Cell::new(false),
            runtime_inspect_snapshot: RefCell::new(None),
            runtime_inspect_budget: RefCell::new(InspectCallBudget::new(Instant::now())),
            keymaps_read: Cell::new(false),
            keymap_source: RefCell::new(None),
            keymap_budget: RefCell::new(KeymapCallBudget::new()),
            granted_capabilities: RefCell::new(Vec::new()),
            declared_events: RefCell::new(BTreeSet::new()),
            debug_view: RefCell::new(None),
            trace_hub: RefCell::new(None),
            workspace_read: Cell::new(false),
            workspace_control: Cell::new(false),
            workspace_source: RefCell::new(None),
            workspace_requests: RefCell::new(None),
            history_provider: RefCell::new(None),
            history_gate: RefCell::new(HistoryGate::new(history_caps())),
            history_grant_scopes: RefCell::new(BTreeSet::new()),
            history_window_start: Cell::new(Instant::now()),
            panel_create_granted: Cell::new(false),
            panel_focus_granted: Cell::new(false),
            panel_create_backend: RefCell::new(None),
            panel_pin_backend: RefCell::new(None),
            image_read_granted: Cell::new(false),
            image_roots: RefCell::new(None),
            theme_configure_granted: Cell::new(false),
            theme_requests: RefCell::new(None),
            config_manage_granted: Cell::new(false),
            config_gate: RefCell::new(ConfigWriteGate::new(Instant::now())),
            config_backend: RefCell::new(None),
            fx_requests: RefCell::new(None),
            sensor_host: RefCell::new(None),
            device_host: RefCell::new(None),
            audio_host: RefCell::new(None),
            fs_backend: RefCell::new(None),
            fs_gate: RefCell::new(FsGate::new(fs_list_caps())),
            fs_window_start: Cell::new(Instant::now()),
            component_queue: RefCell::new(None),
            component_manifest_hash: RefCell::new(None),
        }
    }

    /// Attach the runtime-shared native sensor host (CTX-1199).
    ///
    /// Wired once at activation with every generation; without it native
    /// `bitty-terminal.sensors` calls fail closed with `E_SERVICE_GONE`.
    pub fn set_sensor_host(
        &self,
        host: Option<Rc<RefCell<super::system_sensors::host::SensorHost>>>,
    ) {
        *self.sensor_host.borrow_mut() = host;
    }

    /// Attach the runtime-shared native device host (CTX-1238).
    ///
    /// Wired once at activation with every generation; without it native
    /// `bitty-terminal.devices` calls fail closed with `E_SERVICE_GONE`.
    pub fn set_device_host(
        &self,
        host: Option<Rc<RefCell<super::device_providers::host::DeviceHost>>>,
    ) {
        *self.device_host.borrow_mut() = host;
    }

    /// Attach the runtime-shared native audio host (CTX-1238).
    ///
    /// Wired once at activation with every generation; without it native
    /// `bitty-terminal.audio` calls fail closed with `E_SERVICE_GONE`.
    pub fn set_audio_host(&self, host: Option<Rc<RefCell<super::audio::AudioHost>>>) {
        *self.audio_host.borrow_mut() = host;
    }

    /// Attach the real-filesystem backend for `bitty.fs.list` (CTX-1348).
    ///
    /// Shared at activation with every generation; the call-time gate in
    /// [`HostServices::fs_list`] reads this generation's own `fs.read:`
    /// snapshot, so installing the backend never widens authority. Without
    /// it the verb keeps failing closed with `E_NOT_IMPLEMENTED`.
    pub fn set_fs_backend(&self, backend: Option<Rc<FsListBackend>>) {
        *self.fs_backend.borrow_mut() = backend;
    }

    /// Share the runtime's bounded component request queue (CTX-2254).
    ///
    /// Shared with every generation; the call-time gate in
    /// [`HostServices::component_request`] reads this generation's own granted
    /// capabilities and manifest generation, so the queue itself carries no
    /// authority. `None` leaves the verb failing closed with
    /// `E_NOT_IMPLEMENTED`.
    pub fn set_component_queue(&self, queue: Option<Rc<RefCell<ComponentRequestQueue>>>) {
        *self.component_queue.borrow_mut() = queue;
    }

    /// Bind this generation's manifest hash for component attribution
    /// (CTX-2254).
    ///
    /// Set once at activation from the parsed manifest, never from Lua: the
    /// runtime compares it against the registered generation and refuses a
    /// request from a stale generation.
    pub fn set_component_manifest_hash(&self, hash: Option<String>) {
        *self.component_manifest_hash.borrow_mut() = hash;
    }

    /// Reset the per-plugin `fs.list` window once the accepted window elapsed.
    ///
    /// The explicit clock keeps the window boundary testable without a
    /// wall-clock sleep; production always passes [`Instant::now`].
    fn refresh_fs_window_at(&self, now: Instant) {
        if now.duration_since(self.fs_window_start.get()) >= FS_LIST_WINDOW {
            self.fs_gate.borrow_mut().advance_window();
            self.fs_window_start.set(now);
        }
    }

    /// Admit one `bitty.fs.list` call before any directory I/O.
    ///
    /// Charges one operation against the per-plugin window and fails with
    /// `E_FS_OVER_BOUND` once the window is exhausted. The explicit clock is
    /// the same instant used by [`Self::refresh_fs_window_at`]: production
    /// passes [`Instant::now`], tests freeze it so a long call loop cannot
    /// straddle the accepted window.
    fn admit_fs_list_at(&self, now: Instant) -> Result<(), BridgeError> {
        self.refresh_fs_window_at(now);
        self.fs_gate
            .borrow_mut()
            .admit_listing(&self.plugin_id)
            .map_err(fs_gate_error)
    }

    /// Charge the returned name bytes of one successful listing.
    fn charge_fs_list_bytes(&self, bytes: u64) -> Result<(), BridgeError> {
        self.fs_gate
            .borrow_mut()
            .charge_listing_bytes(&self.plugin_id, bytes)
            .map_err(fs_gate_error)
    }

    /// [`HostServices::fs_list_with_expiry`] at an explicit window instant.
    ///
    /// The explicit clock keeps the per-plugin rate window testable without
    /// sleeping a full window; production always passes [`Instant::now`].
    /// Authorization, caps, and the scan expiry are unchanged.
    fn fs_list_at(
        &self,
        prefix: &str,
        max_entries: usize,
        expiry: Instant,
        now: Instant,
    ) -> Result<LuaValue, BridgeError> {
        // CTX-1348: the backend resolves only paths covered by this
        // generation's own `fs.read:` grants. It reads the activation
        // snapshot directly, so a revoked generation cannot list through a
        // stale service handle. Without a wired backend the verb keeps its
        // historical fail-closed behavior.
        let granted = self.granted_capabilities.borrow().clone();
        let backend = self
            .fs_backend
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.fs.list"))?;
        // Per-plugin window (P0-AC-014): the operation is admitted before
        // any directory I/O, so an exhausted plugin cannot keep scanning.
        self.admit_fs_list_at(now)?;
        let entries: Vec<FsListEntry> =
            backend.list_with_expiry(&granted, prefix, max_entries, expiry)?;
        let bytes = entries
            .iter()
            .map(|entry| entry.name.len() as u64)
            .fold(0u64, u64::saturating_add);
        self.charge_fs_list_bytes(bytes)?;
        Ok(LuaValue::array(
            entries
                .into_iter()
                .map(|entry| {
                    LuaValue::table([
                        ("name", LuaValue::String(entry.name)),
                        ("kind", LuaValue::String(entry.kind.to_string())),
                        ("untrusted", LuaValue::Bool(true)),
                    ])
                })
                .collect(),
        ))
    }

    /// Grant `panel.create` and/or `panel.focus` from the activation snapshot
    /// (CTX-0915, Issue #1596). Independent grants: create allows panel
    /// creation, focus allows panel manipulation and state queries. Absent
    /// grants fail closed with `E_CAPABILITY_DENIED`.
    pub fn set_panel_access(&self, create: bool, focus: bool) {
        self.panel_create_granted.set(create);
        self.panel_focus_granted.set(focus);
    }

    /// Attach the synchronous pinned-float backend for `bitty.panel.set_pinned`
    /// (CTX-1083). Without it a granted call fails closed with
    /// `E_NOT_IMPLEMENTED`, like every other panel verb without a host.
    pub fn set_panel_pin_backend(&self, backend: Option<PanelPinHandler>) {
        *self.panel_pin_backend.borrow_mut() = backend;
    }

    /// Attach the synchronous panel-creation backend for `bitty.panel.create`
    /// (CTX-1214, Issue #2085). Without it a granted call fails closed with
    /// `E_NOT_IMPLEMENTED`, like every other panel verb without a host.
    pub fn set_panel_create_backend(&self, backend: Option<PanelCreateHandler>) {
        *self.panel_create_backend.borrow_mut() = backend;
    }

    /// Grant `terminal.input.submit` from the activation snapshot (CTX-1239,
    /// issue #2101). Absent grants fail closed with `E_CAPABILITY_DENIED`
    /// at call time and with a nil `submit` key at the bridge.
    pub fn set_terminal_submit_access(&self, granted: bool) {
        self.terminal_submit_granted.set(granted);
    }

    /// Share the runtime-shared submit intent queue for
    /// `bitty.terminal.submit` (CTX-1239, issue #2101). Without it a granted
    /// call fails closed with `E_NOT_IMPLEMENTED`, like every other verb
    /// without a host.
    pub fn set_submit_queue(&self, queue: Option<Rc<RefCell<TerminalSubmitQueue>>>) {
        *self.submit_queue.borrow_mut() = queue;
    }

    /// Grant `platform.image-file` from the activation snapshot (CTX-1169,
    /// issue #1976). Absent grants fail closed with `E_CAPABILITY_DENIED`.
    pub fn set_image_access(&self, granted: bool) {
        self.image_read_granted.set(granted);
    }

    /// Attach the approved wallpaper roots for `bitty.image.read` (CTX-1169,
    /// issue #1976). Without roots a granted read fails closed with
    /// `E_NOT_IMPLEMENTED`, like every other verb without a host.
    pub fn set_image_roots(&self, roots: Option<Vec<PathBuf>>) {
        *self.image_roots.borrow_mut() = roots;
    }

    /// Grant `runtime.configure` from the activation snapshot (CTX-1169, issue
    /// #1976). Preview and confirm share the grant: both change runtime
    /// configuration, only their durability differs. Absent grants fail
    /// closed with `E_CAPABILITY_DENIED`.
    pub fn set_theme_access(&self, granted: bool) {
        self.theme_configure_granted.set(granted);
    }

    /// Attach the runtime-shared theme intent queue for
    /// `bitty.theme.preview`/`confirm` (CTX-1169, issue #1976). Without it a
    /// granted call fails closed with `E_NOT_IMPLEMENTED`, like every other
    /// verb without a host.
    pub fn set_theme_queue(&self, queue: Option<Rc<RefCell<ThemeRequestQueue>>>) {
        *self.theme_requests.borrow_mut() = queue;
    }

    /// Grant `config.manage` from the activation snapshot (CTX-1613,
    /// bitty#2393). Write-only: it never grants `runtime.inspect` and
    /// `runtime.inspect` never grants it. Absent grants fail closed with
    /// `E_CAPABILITY_DENIED`; safe mode denies even when recorded.
    pub fn set_config_access(&self, granted: bool) {
        self.config_manage_granted.set(granted);
    }

    /// Attach the durable user-layer backend for `config.set` (CTX-1613,
    /// bitty#2393). Installed only for a granted generation; without it a
    /// granted call fails closed with `E_CONFIG_UNAVAILABLE`. The backend owns
    /// validation and the atomic replace and returns only a path-free failure
    /// class.
    pub fn set_config_write_backend(&self, backend: Option<Rc<dyn ConfigWriteBackend>>) {
        *self.config_backend.borrow_mut() = backend;
    }

    /// Shared enqueue core behind `theme_preview`/`theme_confirm` (CTX-1169,
    /// issue #1976).
    ///
    /// Grant gate first, then the pure grammar check (`CustomPalette`
    /// fail-closed, so malformed input never reaches shared state), then the
    /// bounded queue. `verb` names the calling verb for the unwired-host
    /// diagnostic only.
    fn enqueue_theme(
        &self,
        theme: &ThemeTable,
        persist: bool,
        verb: &'static str,
    ) -> Result<bool, BridgeError> {
        if !self.theme_configure_granted.get() {
            return Err(BridgeError::capability_denied("runtime.configure"));
        }
        let palette = CustomPalette::from_hex(
            &theme.background,
            &theme.foreground,
            &theme.cursor,
            &theme.selection,
            &theme.ansi,
        )
        .ok_or_else(|| {
            BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                "theme colors must be canonical #RRGGBB with exactly 16 ansi entries",
            )
        })?;
        let queue = self
            .theme_requests
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented(verb))?;
        Ok(queue
            .borrow_mut()
            .push(QueuedThemeRequest { palette, persist }))
    }

    /// Attach the runtime-shared fx intent queue for `bitty.fx.pulse`
    /// (CTX-1139). Without it a granted call fails closed with
    /// `E_NOT_IMPLEMENTED`, like every other verb without a host.
    pub fn set_fx_queue(&self, queue: Option<Rc<RefCell<FxPulseQueue>>>) {
        *self.fx_requests.borrow_mut() = queue;
    }

    /// Grant `workspace.read` and/or `workspace.control` from the activation
    /// snapshot (CTX-0889). Independent: read never implies control, and
    /// control does not imply read. Absent grants fail closed with
    /// `E_CAPABILITY_DENIED`.
    pub fn set_workspace_access(&self, read: bool, control: bool) {
        self.workspace_read.set(read);
        self.workspace_control.set(control);
    }

    /// Whether this generation holds `workspace.read` (also gates delivery
    /// of `workspace.*` events).
    #[must_use]
    pub fn has_workspace_read(&self) -> bool {
        self.workspace_read.get()
    }

    /// Whether this generation holds `workspace.control`.
    #[must_use]
    pub fn has_workspace_control(&self) -> bool {
        self.workspace_control.get()
    }

    /// Attach the workspace read source and the runtime-shared request
    /// queue (CTX-0889). Without them a granted call fails closed with
    /// `E_NOT_IMPLEMENTED`.
    pub fn set_workspace_backend(
        &self,
        source: Option<Rc<dyn WorkspaceSource>>,
        requests: Option<Rc<RefCell<WorkspaceRequestQueue>>>,
    ) {
        *self.workspace_source.borrow_mut() = source;
        *self.workspace_requests.borrow_mut() = requests;
    }

    /// Attach the live Core target/anchor source (W-29 successor).
    ///
    /// `None` leaves the Core provider empty (plugin lenses still work).
    /// Presence never grants authority: the session still requires the
    /// accepted `ui.overlay` capability, and dispatch still returns a typed
    /// command the registry executes under its owner's grants.
    pub fn set_target_source(&self, source: Option<Rc<dyn TargetSource>>) {
        *self.target_source.borrow_mut() = source;
    }

    /// Attach the composition-root history snapshot provider (CTX-1352).
    ///
    /// Only the provider supplies persisted rows; the per-source capability
    /// head still gates every call, and the gate still enforces capture,
    /// scope, safe mode, bounds, and budgets. Without a provider a granted
    /// call fails closed with the typed `E_HISTORY_UNAVAILABLE` denial.
    pub fn set_history_provider(&self, provider: Option<Rc<dyn HistoryProvider>>) {
        *self.history_provider.borrow_mut() = provider;
    }

    /// Grant the UI surfaces for this generation from the activation snapshot.
    ///
    /// Absent grants stay denied: the default [`UiAccess`] has every gate
    /// closed, so a caller that never sets this can never mount content.
    pub fn set_ui_access(&self, access: UiAccess) {
        *self.ui_access.borrow_mut() = access;
    }

    /// UI authorization currently applied to this generation.
    #[must_use]
    pub fn ui_access(&self) -> UiAccess {
        self.ui_access.borrow().clone()
    }

    /// Pin the UI handle epoch to the activation generation.
    ///
    /// Called once per activation/reload on the freshly built services, before
    /// the generation's `init.lua` runs, so handles minted by different
    /// generations never collide.
    pub fn set_ui_epoch(&self, epoch: u32) {
        self.ui_epoch.set(epoch);
        self.ui_blocks.borrow_mut().set_epoch(epoch);
    }

    /// Attach the Core-owned terminal-slot scene host (bitty#2785).
    ///
    /// Installed only for `ui.rich` generations; without it a rich
    /// `bitty.ui.mount("terminal", …)` keeps failing closed with
    /// `E_UI_UNAVAILABLE`. The call-time `ui.rich` gate stays as the second
    /// layer.
    pub fn set_ui_scene_backend(&self, backend: Option<Rc<crate::scene_host::SceneHost>>) {
        *self.ui_scene_backend.borrow_mut() = backend;
    }

    /// Attach the runtime-shared focusable-overlay capture switch (CTX-0941).
    ///
    /// The runtime wires one manager across every generation so the
    /// single-owner invariant holds across plugins and reload. A generation
    /// whose services were built without wiring keeps `None` and every overlay
    /// capture call fails closed with `E_UI_UNAVAILABLE`.
    pub fn set_overlay_capture(&self, capture: Rc<RefCell<OverlayCapture>>) {
        *self.overlay_capture.borrow_mut() = Some(capture);
    }

    /// Attach the runtime-shared presentation-only overlay session
    /// (CTX-1431, `bitty` #2406).
    ///
    /// One session spans every generation so the one-surface-per-window rule
    /// holds across plugins and reload. A generation whose services were
    /// built without wiring keeps `None` and every show fails closed with
    /// `E_UI_UNAVAILABLE`. Hide stays available without the shared state so
    /// cleanup can never fail spuriously.
    pub fn set_presentation_overlay(&self, overlay: Rc<RefCell<PresentationOverlay>>) {
        *self.presentation_overlay.borrow_mut() = Some(overlay);
    }

    /// The shared presentation session, if one was wired.
    fn presentation_overlay(&self) -> Option<Rc<RefCell<PresentationOverlay>>> {
        self.presentation_overlay.borrow().clone()
    }

    /// The shared capture manager, if one was wired.
    fn overlay_capture(&self) -> Option<Rc<RefCell<OverlayCapture>>> {
        self.overlay_capture.borrow().clone()
    }

    /// Attach the runtime-shared overlay peer table (CTX-0973).
    ///
    /// The runtime wires one table across every generation so an acquire that
    /// lazily expires another generation's session can dispose that session's
    /// transient spec surface synchronously. A generation built without
    /// wiring still disposes its own expired surfaces; only cross-generation
    /// disposal needs the table.
    pub fn set_overlay_peers(&self, peers: OverlayPeers) {
        *self.overlay_peers.borrow_mut() = Some(peers);
    }

    /// Live capture session as `(owner plugin, handle)`, if any.
    ///
    /// Snapshot before `acquire`: a successful acquire implies the previous
    /// owner (if any) was expired and lazily finished, so the caller disposes
    /// that session's transient surface.
    fn live_overlay_session(capture: &Rc<RefCell<OverlayCapture>>) -> Option<(String, i64)> {
        let guard = capture.borrow();
        Some((guard.owner_plugin()?.to_string(), guard.owner_handle()?))
    }

    /// Dispose one expired session's transient spec surface (CTX-0973).
    ///
    /// Same-generation sessions dispose directly (covers unwired unit paths);
    /// cross-generation sessions dispose through the shared peer table.
    /// Mechanism-path blocks are never recorded as spec surfaces, so
    /// disposing a mechanism session is a safe no-op. Dead peers are pruned
    /// so the table stays bounded by the live generation count.
    fn dispose_expired_spec_overlay(&self, plugin: &str, handle: i64) {
        if plugin == self.plugin_id {
            self.remove_spec_overlay_block(handle);
            // The expired owner's targeting session ends with its capture,
            // even when the surface was a mechanism block rather than a spec
            // surface. `clear_targeting_session` is identity-gated, so it
            // cannot clear a newer publisher's layer.
            self.clear_targeting_session();
            return;
        }
        let peers = self.overlay_peers.borrow().clone();
        let Some(peers) = peers else {
            return;
        };
        let weaks = peers.borrow().get(plugin).cloned().unwrap_or_default();
        for weak in weaks {
            if let Some(peer) = weak.upgrade() {
                peer.remove_spec_overlay_block(handle);
                peer.clear_targeting_session();
            }
        }
        let mut table = peers.borrow_mut();
        if let Some(list) = table.get_mut(plugin) {
            list.retain(|weak| weak.upgrade().is_some());
            if list.is_empty() {
                table.remove(plugin);
            }
        }
    }

    /// Drop every stale spec surface of this generation except `live`.
    ///
    /// Mechanism acquire never mounts a spec surface, so every recorded spec
    /// handle is stale once the new session holds capture. `live` is skipped
    /// so re-acquiring a spec surface through the mechanism path keeps the
    /// live block.
    fn clear_stale_spec_overlays_except(&self, live: i64) {
        let stale: Vec<i64> = self
            .spec_overlay_handles
            .borrow()
            .iter()
            .copied()
            .filter(|candidate| *candidate != live)
            .collect();
        for orphan in stale {
            self.remove_spec_overlay_block(orphan);
        }
    }

    /// Wire the safe-mode flag for this generation (CTX-0941).
    ///
    /// Called once at activation from the runtime config. Safe mode never
    /// presents a focusable overlay: acquire, update, and poll fail closed
    /// with `E_UI_UNAVAILABLE` while set.
    pub fn set_safe_mode(&self, safe_mode: bool) {
        self.safe_mode.set(safe_mode);
    }

    /// Whether this generation runs in safe mode.
    #[must_use]
    pub fn is_safe_mode(&self) -> bool {
        self.safe_mode.get()
    }

    /// The focusable-overlay capability gate (accepted W-01 v2, CTX-0941).
    ///
    /// Deny-by-default on `ui.overlay.focus`: without the grant every
    /// acquire, update, and poll fails with `E_CAPABILITY_DENIED` naming the
    /// capability. Release is deliberately ungated (like the target-session
    /// cancel path): a revoked grant must still free input.
    fn require_overlay_focus(&self) -> Result<(), BridgeError> {
        if !self.ui_access.borrow().overlay_focus {
            return Err(BridgeError::capability_denied("ui.overlay.focus"));
        }
        Ok(())
    }

    /// The safe-mode gate for the focusable surface (CTX-0941).
    ///
    /// Safe mode never presents a focusable overlay and never starts a
    /// capture session: acquire, update, and poll fail with
    /// `E_UI_UNAVAILABLE`. Release stays available so cleanup can never
    /// wedge.
    fn require_overlay_available(&self) -> Result<(), BridgeError> {
        if self.safe_mode.get() {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "focusable overlay is unavailable in safe mode",
            ));
        }
        Ok(())
    }

    /// The presentation-only overlay gate (CTX-1431, `bitty` #2406).
    ///
    /// Deny-by-default on the two v1 mount grants: a block mounted into the
    /// `overlay` slot already needed `ui.rich` + `ui.overlay`, and the
    /// presentation path deliberately does **not** require or imply
    /// `ui.overlay.focus` (no capture). Safe mode never presents a plugin
    /// surface, and a host without the shared session fails closed with
    /// `E_UI_UNAVAILABLE`.
    fn require_presentation_overlay(
        &self,
    ) -> Result<Rc<RefCell<PresentationOverlay>>, BridgeError> {
        if self.safe_mode.get() {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "presentation overlay is unavailable in safe mode",
            ));
        }
        {
            let access = self.ui_access.borrow();
            if !access.rich {
                return Err(BridgeError::capability_denied("ui.rich"));
            }
            if !access.overlay {
                return Err(BridgeError::capability_denied("ui.overlay"));
            }
        }
        self.presentation_overlay().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no presentation-only overlay surface",
            )
        })
    }

    /// Attach the runtime-shared targeting mechanism state (W-29, CTX-0942).
    ///
    /// The runtime wires one registry, lens set, allocator, and present-time
    /// annotation channel across every generation so the provider set spans
    /// plugins and survives reload. A generation built without wiring keeps
    /// `None` and every targeting call fails closed with
    /// `E_UI_UNAVAILABLE`. Uses only existing `bitty-ui` types; no new
    /// targeting type is introduced.
    pub fn set_targeting_state(
        &self,
        registry: Rc<RefCell<TargetRegistry>>,
        lenses: Rc<RefCell<Vec<(String, DerivedProvider)>>>,
        allocator: Rc<RefCell<LabelAllocator>>,
        annotations: SharedTargetAnnotations,
    ) {
        *self.target_registry.borrow_mut() = Some(registry);
        *self.target_lenses.borrow_mut() = Some(lenses);
        *self.label_allocator.borrow_mut() = Some(allocator);
        *self.target_annotations.borrow_mut() = Some(annotations);
        *self.target_annotations_published.borrow_mut() = None;
    }

    /// The `bitty.ui.targets`/`bitty.ui.labels` capability gate (W-29).
    ///
    /// Deny-by-default on the accepted `ui.overlay` identifier: a targeting
    /// session consumes the W-28 focusable overlay / transient-input-capture
    /// mechanism, which is itself gated there. No new capability identifier
    /// is introduced.
    fn require_targets_capability(&self) -> Result<(), BridgeError> {
        if !self.ui_access.borrow().overlay {
            return Err(BridgeError::capability_denied("ui.overlay"));
        }
        Ok(())
    }

    /// Shared registry or typed `E_UI_UNAVAILABLE`.
    fn require_target_registry(&self) -> Result<Rc<RefCell<TargetRegistry>>, BridgeError> {
        self.target_registry.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui targeting surface",
            )
        })
    }

    /// Shared lens set or typed `E_UI_UNAVAILABLE`.
    #[allow(clippy::type_complexity)]
    fn require_target_lenses(
        &self,
    ) -> Result<Rc<RefCell<Vec<(String, DerivedProvider)>>>, BridgeError> {
        self.target_lenses.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui targeting surface",
            )
        })
    }

    /// Shared allocator or typed `E_UI_UNAVAILABLE`.
    fn require_label_allocator(&self) -> Result<Rc<RefCell<LabelAllocator>>, BridgeError> {
        self.label_allocator.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui labeling surface",
            )
        })
    }

    /// Clear this generation's targeting session and bindings (suspend/dispose/rollback).
    ///
    /// Drops the per-generation dispatcher and session handle so no orphaned
    /// session or dangling binding survives the generation. Shared registry,
    /// lenses, and allocator are untouched (revocation of lenses is owned by
    /// [`Self::revoke_targeting_lenses`]).
    pub fn clear_targeting_session(&self) {
        let published = self.target_annotations_published.borrow_mut().take();
        *self.target_dispatcher.borrow_mut() = TargetDispatcher::new();
        *self.target_session_overlay.borrow_mut() = None;
        if let Some(published) = published {
            if let Some(annotations) = self.target_annotations.borrow().clone() {
                // Only the generation whose exact layer is still current may
                // clear the shared channel; a foreign or stale owner leaves
                // the new publisher's layer untouched.
                let current = annotations.borrow().clone();
                if current
                    .as_ref()
                    .is_some_and(|layer| Rc::ptr_eq(layer, &published))
                {
                    *annotations.borrow_mut() = None;
                }
            }
        }
    }

    /// Drop every lens this generation registered (suspend/dispose/rollback).
    ///
    /// Returns whether an active session was present so the caller can release
    /// the W-28 capture. Shared registry and allocator are untouched.
    pub fn revoke_targeting_lenses(&self) -> bool {
        if let Some(lenses) = self.target_lenses.borrow().clone() {
            lenses
                .borrow_mut()
                .retain(|(owner, _)| owner != &self.plugin_id);
        }
        let had_session = self.target_session_overlay.borrow().is_some();
        self.clear_targeting_session();
        had_session
    }

    /// Read the live Core target set from the injected source.
    ///
    /// Cold path only. The source is bounded by the provider snapshot cap;
    /// an over-cap or malformed set fails closed before any registry insert.
    fn core_target_infos(&self) -> Result<Vec<CoreTargetInfo>, BridgeError> {
        let Some(source) = self.target_source.borrow().clone() else {
            return Ok(Vec::new());
        };
        let targets = source.targets()?;
        if targets.len() > MAX_SNAPSHOT_TARGETS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many snapshot targets: max {MAX_SNAPSHOT_TARGETS}"),
            ));
        }
        Ok(targets)
    }

    /// Build the Core provider offers from live `infos`.
    ///
    /// Unknown kinds fail closed with the existing `E_DEF_INVALID`.
    fn core_target_offers(
        &self,
        infos: &[CoreTargetInfo],
    ) -> Result<Vec<ProviderTarget>, BridgeError> {
        infos
            .iter()
            .map(|info| targets_offer(&info.kind, info.id))
            .collect()
    }

    /// Build a fresh mediator from the Core provider plus shared lenses.
    ///
    /// `core_offers` is the live Core target set read once per collection
    /// (panels/workspaces today; command blocks, UI nodes, and links when
    /// their providers land). Lenses are collected in registration order.
    /// Registration errors propagate with existing codes.
    fn targeting_mediator(
        &self,
        core_offers: Vec<ProviderTarget>,
    ) -> Result<ProviderMediator, BridgeError> {
        let lenses = self.require_target_lenses()?;
        let mut mediator = ProviderMediator::with_core_targets(core_offers);
        for (_, lens) in lenses.borrow().iter() {
            let rebuilt = DerivedProvider::new(lens.name(), lens.collect())
                .map_err(map_targets_provider_error)?;
            mediator
                .register(Box::new(rebuilt), true)
                .map_err(map_targets_provider_error)?;
        }
        Ok(mediator)
    }

    /// Shared present-time annotation channel or typed `E_UI_UNAVAILABLE`.
    fn require_target_annotations(&self) -> Result<SharedTargetAnnotations, BridgeError> {
        self.target_annotations.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui annotation surface",
            )
        })
    }

    /// Find the live Core anchor metadata for a collected snapshot target.
    fn core_info_for<'a>(
        infos: &'a [CoreTargetInfo],
        target: &TargetRef,
    ) -> Option<&'a CoreTargetInfo> {
        let kind = targets_kind_string(target);
        let id = target_raw_id(target);
        infos.iter().find(|info| info.kind == kind && info.id == id)
    }

    /// Inner session start after the W-28 capture was acquired (W-29).
    ///
    /// Collects a fresh snapshot, validates lengths, allocates labels, builds
    /// one anchored annotation layer (Core rectangles from the live source,
    /// plugin anchors otherwise), publishes it to the present-time channel,
    /// and binds the dispatcher. All failures are typed with existing codes
    /// and leave no partial session; the caller releases capture on error.
    /// Nothing is published to the Event Bus.
    #[allow(clippy::type_complexity)]
    fn targets_session_start_inner(
        &self,
        width: u16,
        anchors: &[(u16, u16)],
        commands: &[String],
    ) -> Result<Vec<String>, BridgeError> {
        let registry = self.require_target_registry()?;
        let infos = self.core_target_infos()?;
        let offers = self.core_target_offers(&infos)?;
        let mediator = self.targeting_mediator(offers)?;
        let snapshot = mediator
            .collect(&mut registry.borrow_mut())
            .map_err(map_targets_provider_error)?;
        let entries = snapshot.entries();
        if anchors.len() != entries.len() || commands.len() != entries.len() {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!(
                    "targeting session expected {} anchors and commands, got {} and {}",
                    entries.len(),
                    anchors.len(),
                    commands.len()
                ),
            ));
        }
        if anchors.len() > MAX_HINT_TARGETS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many targets for labels: max {MAX_HINT_TARGETS}"),
            ));
        }
        let allocator = self.require_label_allocator()?;
        // A live Core panel anchor owns its label position; workspace anchors
        // and plugin targets keep the caller-supplied anchor so policy can
        // place workspace hints on the rail rather than over the panels.
        let points: Vec<Point> = entries
            .iter()
            .zip(anchors.iter())
            .map(
                |(entry, anchor)| match Self::core_info_for(&infos, &entry.target()) {
                    Some(info) if info.kind == "panel" => info
                        .rect
                        .map_or(Point::new(anchor.0, anchor.1), |(x, y, _, _)| {
                            Point::new(x, y)
                        }),
                    _ => Point::new(anchor.0, anchor.1),
                },
            )
            .collect();
        let labels = allocator
            .borrow()
            .assign(&points, width)
            .map_err(map_targets_label_error)?;
        let mut parsed = Vec::with_capacity(commands.len());
        for command in commands {
            match QualifiedCommand::parse(command) {
                Ok(id) => parsed.push(id),
                Err(_) => {
                    return Err(BridgeError::new(
                        "validation",
                        "E_DEF_INVALID",
                        "targeting session command id is not a qualified command",
                    ));
                }
            }
        }
        let specs: Vec<HintAnnotationSpec> = entries
            .iter()
            .zip(points.iter())
            .map(
                |(entry, point)| match Self::core_info_for(&infos, &entry.target()) {
                    Some(info) => HintAnnotationSpec {
                        target: entry.target(),
                        workspace: info.workspace.map(WorkspaceId::new),
                        rect: info
                            .rect
                            .map_or(Rect::zero(), |(x, y, w, h)| Rect::new(x, y, w, h)),
                    },
                    None => HintAnnotationSpec {
                        target: entry.target(),
                        workspace: None,
                        rect: Rect::new(point.x, point.y, 1, 1),
                    },
                },
            )
            .collect();
        let height = specs
            .iter()
            .map(|spec| spec.rect.bottom())
            .chain(
                points
                    .iter()
                    .map(|point| u32::from(point.y).saturating_add(1)),
            )
            .max()
            .unwrap_or(1)
            .min(u32::from(u16::MAX)) as u16;
        let layer = HintAnnotationLayer::build_anchored(
            &specs,
            &labels,
            &points,
            Rect::new(0, 0, width, height),
        )
        .map_err(|error| match error {
            bitty_ui::AnnotationLayerError::TooManyAnnotations { requested, max } => {
                BridgeError::new(
                    "budget",
                    "E_DEF_LIMIT",
                    format!("too many annotations: requested {requested}, max {max}"),
                )
            }
            other => BridgeError::new("validation", "E_DEF_INVALID", other.to_string()),
        })?;
        if layer.len() + self.target_dispatcher.borrow().len() > MAX_TARGET_BINDINGS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many bindings: max {MAX_TARGET_BINDINGS}"),
            ));
        }
        let mut dispatcher = TargetDispatcher::new();
        dispatcher
            .bind_layer(&layer, &parsed)
            .map_err(map_targets_dispatch_error)?;
        let annotations = self.require_target_annotations()?;
        let layer = Rc::new(layer);
        *annotations.borrow_mut() = Some(Rc::clone(&layer));
        *self.target_annotations_published.borrow_mut() = Some(layer);
        *self.target_dispatcher.borrow_mut() = dispatcher;
        Ok(labels)
    }

    /// Reconcile the active targeting session against the latest live targets.
    ///
    /// Cold-path only. Reads the same `TargetSource` (plus every registered
    /// lens) that a fresh collection would use, retires ids that vanished from
    /// the session registry, drops their annotations, revokes their bindings,
    /// and republishes the bounded layer. A closed panel therefore loses its
    /// label and its dispatch succeeds no more. No-op without an active
    /// session; returns whether the registry or layer changed.
    ///
    /// # Errors
    ///
    /// [`BridgeError`] when the targeting state is unwired or the source
    /// cannot serve live rows. On error nothing is retired.
    pub fn reconcile_target_session(&self) -> Result<bool, BridgeError> {
        if self.target_session_overlay.borrow().is_none() {
            return Ok(false);
        }
        let registry = self.require_target_registry()?;
        let infos = self.core_target_infos()?;
        let lenses = self.require_target_lenses()?;
        let mut live = Vec::with_capacity(infos.len());
        for info in &infos {
            live.push(targets_offer(&info.kind, info.id)?.to_ref());
        }
        for (_, lens) in lenses.borrow().iter() {
            live.extend(lens.collect().into_iter().map(ProviderTarget::to_ref));
        }
        let retired = registry.borrow_mut().retain_live(&live);
        let annotations = self.require_target_annotations()?;
        let Some(current) = annotations.borrow().clone() else {
            return Ok(retired > 0);
        };
        let mut updated = (*current).clone();
        let dropped = {
            let guard = registry.borrow();
            updated.retain_live(&guard)
        };
        if dropped == 0 {
            return Ok(retired > 0);
        }
        self.target_dispatcher
            .borrow_mut()
            .retain_presented(&updated);
        let updated = Rc::new(updated);
        *annotations.borrow_mut() = Some(Rc::clone(&updated));
        *self.target_annotations_published.borrow_mut() = Some(updated);
        Ok(true)
    }

    /// Invalidate every block handle this generation minted (suspend/dispose).
    ///
    /// The registry is host-side state: clearing it makes `bitty.ui.update`
    /// on a pre-suspend handle fail closed (`false`) after suspend and after
    /// resume until the plugin mounts again. Spec-acquired surfaces are
    /// covered: the whole registry (including every recorded spec handle)
    /// is dropped, so no orphan survives unload or crash.
    pub fn clear_ui_blocks(&self) {
        // bitty#2785: the generation's terminal-slot scene leaves with its
        // blocks. The scene host detaches the scene and disposes the
        // Core-owned panel, so neither a stale scene nor a placement bind
        // survives suspend, dispose, crash, or reload.
        if let Some(backend) = self.ui_scene_backend.borrow().clone() {
            backend.release_owner(&self.plugin_id, self.ui_epoch.get());
        }
        self.ui_blocks.borrow_mut().clear();
        self.spec_overlay_handles.borrow_mut().clear();
    }

    /// Drop one block by handle, releasing its retained-string budget.
    ///
    /// Rolls back a spec-acquired surface when capture acquisition fails, so
    /// a denied acquire leaves no orphan block behind.
    fn remove_ui_block(&self, handle: i64) -> bool {
        self.ui_blocks.borrow_mut().remove(handle)
    }

    /// Record a spec-acquired surface, dropping any stale spec surface of
    /// this generation.
    ///
    /// Stale entries arise only when a session ended without a
    /// services-mediated release: the runtime-driven paths (expiry, revoke)
    /// dispose the ended handle through [`Self::remove_spec_overlay_block`],
    /// and this is the backstop for the lazy-expiry-inside-acquire path,
    /// where the previous session ends synchronously within the new acquire.
    /// At most one spec surface per generation survives: a previous session's
    /// content can never shadow the new session.
    fn remember_spec_overlay(&self, handle: i64) {
        let stale: Vec<i64> = self
            .spec_overlay_handles
            .borrow()
            .iter()
            .copied()
            .filter(|candidate| *candidate != handle)
            .collect();
        for orphan in stale {
            self.remove_ui_block(orphan);
        }
        let mut spec = self.spec_overlay_handles.borrow_mut();
        spec.clear();
        spec.insert(handle);
    }

    /// Dispose a spec-acquired surface by handle (session end).
    ///
    /// No-op unless `handle` is a surface this generation mounted through
    /// spec acquire: mechanism-path blocks (mounted through `ui.mount`)
    /// survive release as the plugin's retained content. Returns whether a
    /// surface was unmounted. The runtime calls this for sessions that end
    /// on runtime-driven paths (expiry, focus-switch/cancel revoke); the
    /// services release path calls it for owner releases.
    pub fn remove_spec_overlay_block(&self, handle: i64) -> bool {
        if !self.spec_overlay_handles.borrow_mut().remove(&handle) {
            return false;
        }
        self.remove_ui_block(handle)
    }

    /// Read-only mounted-block view (tests, diagnostics, presentation wiring).
    pub fn with_ui_blocks<R>(&self, f: impl FnOnce(&UiBlocks) -> R) -> R {
        f(&self.ui_blocks.borrow())
    }

    /// Grant (or revoke) the `process.spawn:git` Layer-2 spawn surface.
    ///
    /// Set from the activation grant snapshot (`process.spawn:git` present);
    /// absent grants fail closed at call time. The execution backend itself
    /// is injected separately via [`Self::set_spawn_backend`].
    pub fn set_spawn_git(&self, granted: bool) {
        self.spawn_git.set(granted);
    }

    /// Whether the `process.spawn:git` spawn surface is granted.
    #[must_use]
    pub fn has_spawn_git(&self) -> bool {
        self.spawn_git.get()
    }

    /// Inject the spawn execution backend (`None` restores fail-closed
    /// `E_SPAWN_UNAVAILABLE`).
    pub fn set_spawn_backend(&self, backend: Option<SpawnHandler>) {
        *self.spawn_backend.borrow_mut() = backend;
    }

    /// Read-only store view (tests, diagnostics).
    pub fn with_store<R>(&self, f: impl FnOnce(&PluginStore) -> R) -> R {
        f(&self.store.borrow())
    }

    /// Grant `bitty.env` reads for exact keys and prefix wildcards (CTX-0330,
    /// CTX-0830).
    ///
    /// Set from the activation grant snapshot (`env.read:<KEY>` and
    /// `env.read:PREFIX*` entries with the prefix stripped); absent grants
    /// fail closed at call time with `E_NOT_IMPLEMENTED`. Every entry is
    /// shape-validated here via [`env_grant_shape_ok`] — a malformed
    /// recorded grant (including the bare-star allow-all `*`) fails the
    /// whole set rather than silently dropping — and the set is capped at
    /// [`MAX_ENV_GRANTS`].
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with `E_DEF_INVALID`/`E_DEF_LIMIT` when a grant is
    /// malformed or the set exceeds [`MAX_ENV_GRANTS`].
    pub fn set_env_grants(&self, keys: BTreeSet<String>) -> Result<(), BridgeError> {
        if keys.len() > MAX_ENV_GRANTS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("env grant set exceeds {MAX_ENV_GRANTS} keys"),
            ));
        }
        for key in &keys {
            if !env_grant_shape_ok(key) {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    format!("env grant '{key}' is not a valid key or prefix wildcard"),
                ));
            }
        }
        *self.env_grants.borrow_mut() = keys;
        Ok(())
    }

    /// Granted `bitty.env` keys for this generation (tests, diagnostics).
    #[must_use]
    pub fn env_grants(&self) -> BTreeSet<String> {
        self.env_grants.borrow().clone()
    }

    /// Inject the host environment source (`None` restores fail-by-absence
    /// [`EmptyEnv`]).
    pub fn set_env_source(&self, source: Option<Rc<dyn EnvSource>>) {
        *self.env_source.borrow_mut() = source.unwrap_or_else(|| Rc::new(EmptyEnv));
    }

    /// Plugin id this service set belongs to.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Attach the caller manifest's service declarations for this generation.
    ///
    /// Wired once at activation from the verified manifest: `provided` gates
    /// [`HostServices::service_provide_check`], `required` supplies the
    /// fallback requirement for [`HostServices::service_resolve`]. Absent
    /// entries fail closed at call time (undeclared provide/resolve is
    /// rejected, never inferred).
    pub fn set_service_manifest(
        &self,
        provided: Vec<ProvidedService>,
        required: Vec<(String, String)>,
    ) {
        *self.service_provided.borrow_mut() = provided;
        *self.service_required.borrow_mut() = required;
    }

    /// Declared provided services for this generation (tests, diagnostics).
    #[must_use]
    pub fn provided_services(&self) -> Vec<ProvidedService> {
        self.service_provided.borrow().clone()
    }

    /// Declared required services for this generation (tests, diagnostics).
    #[must_use]
    pub fn required_services(&self) -> Vec<(String, String)> {
        self.service_required.borrow().clone()
    }

    /// Attach the runtime-shared service directory for this generation.
    ///
    /// Wired once at activation; without it every service call fails closed
    /// with `E_NOT_IMPLEMENTED` (no service backend).
    pub fn set_service_directory(&self, directory: Rc<RefCell<ServiceDirectory>>) {
        *self.service_directory.borrow_mut() = Some(directory);
    }

    /// Attach the runtime-shared brokered-call ledger for this generation.
    ///
    /// Wired once at activation alongside the service directory. Observe-only:
    /// a missing ledger never blocks a call, it only leaves the call
    /// unrecorded.
    pub fn set_service_call_ledger(&self, ledger: Rc<RefCell<ServiceCallLedger>>) {
        *self.service_call_ledger.borrow_mut() = Some(ledger);
    }

    /// Whether this generation's activation grant snapshot covers every one
    /// of `heads` (exact id match, no implication — the same rule as
    /// [`Self::has_granted_capability`]).
    ///
    /// O(h log g) over the sorted snapshot (h heads, g grants).
    #[must_use]
    pub fn service_heads_covered(&self, heads: &[String]) -> bool {
        let granted = self.granted_capabilities.borrow();
        heads.iter().all(|head| {
            granted
                .binary_search_by(|granted| granted.as_str().cmp(head.as_str()))
                .is_ok()
        })
    }

    /// Attach the runtime-shared command directory for this generation
    /// (CTX-1035).
    ///
    /// Without it every `bitty.commands.list`/`invoke` call fails closed
    /// with `E_NOT_IMPLEMENTED`.
    pub fn set_command_directory(&self, directory: Rc<RefCell<CommandDirectory>>) {
        *self.command_directory.borrow_mut() = Some(directory);
    }

    /// Grant (or revoke) `bitty.debug.inspect` from the activation snapshot.
    ///
    /// Independent of [`Self::set_debug_trace`]: neither grant implies the
    /// other. Absent grants fail closed with `E_CAPABILITY_DENIED`.
    pub fn set_debug_inspect(&self, granted: bool) {
        self.debug_inspect.set(granted);
    }

    /// Grant (or revoke) `bitty.debug.trace`/`trace_get` from the activation
    /// snapshot. Absent grants fail closed with `E_CAPABILITY_DENIED`.
    pub fn set_debug_trace(&self, granted: bool) {
        self.debug_trace.set(granted);
    }

    /// Install the `runtime.inspect` grant and its prebuilt snapshot (issue
    /// #2396).
    ///
    /// The application builds the snapshot on its cold path and installs it
    /// before activation. `granted` is the only authority switch; a snapshot
    /// installed without the grant is unreachable, and a granted generation
    /// without a snapshot fails closed with `E_NOT_IMPLEMENTED`. Call budgets
    /// start at activation and never reset within the generation.
    pub fn set_runtime_inspect(&self, granted: bool, snapshot: Option<RuntimeInspectSnapshot>) {
        self.runtime_inspect_granted.set(granted);
        *self.runtime_inspect_snapshot.borrow_mut() = snapshot;
    }

    /// [`HostServices::runtime_inspect`] with an injectable start time.
    ///
    /// The public call starts the wall clock immediately before this method;
    /// tests pass an older `Instant` to exercise the 10 ms deadline without a
    /// sleep. Authority order is fixed: grant check, backend-presence check,
    /// rate budget, deadline. The unwired-backend check deliberately precedes
    /// the rate budget (a failed call reads nothing and must not consume the
    /// 16 calls/s allowance); the grant check stays first, so this is not an
    /// authority or observability widening. VM fuel/wall budgets still bound
    /// the surrounding Lua execution.
    fn runtime_inspect_at(&self, started: Instant) -> Result<RuntimeInspectSnapshot, BridgeError> {
        if !self.runtime_inspect_granted.get() {
            return Err(BridgeError::capability_denied("runtime.inspect"));
        }
        // An unwired backend fails before consuming a rate-budget slot, so a
        // granted-but-unavailable generation cannot exhaust its own budget on
        // a call that never reads anything.
        if self.runtime_inspect_snapshot.borrow().is_none() {
            return Err(BridgeError::not_implemented("bitty.runtime.inspect"));
        }
        if !self.runtime_inspect_budget.borrow_mut().allow(started) {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                "runtime.inspect exceeded 16 calls per second",
            ));
        }
        let snapshot = self
            .runtime_inspect_snapshot
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.runtime.inspect"))?;
        if started.elapsed() > Duration::from_millis(RUNTIME_INSPECT_MAX_WALL_MS) {
            return Err(BridgeError::timeout());
        }
        Ok(snapshot)
    }

    /// Install the `runtime.keymaps.read` grant and the live registry source
    /// (issue #2405).
    ///
    /// `granted` is the only authority switch: the source is installed for
    /// every generation and carries no authority, while an ungranted
    /// generation fails closed with `E_CAPABILITY_DENIED` before the source is
    /// touched. Safe policy denies the same way regardless of the grant. The
    /// call budget starts at activation and never resets within the
    /// generation.
    pub fn set_keymaps_access(&self, granted: bool, source: Option<Rc<dyn KeymapSource>>) {
        self.keymaps_read.set(granted);
        *self.keymap_source.borrow_mut() = source;
    }

    /// Record this generation's own granted capability ids (activation
    /// snapshot) for the `grants` inspect target. Stored sorted and
    /// de-duplicated; other plugins' grants are never reachable.
    ///
    /// The snapshot is fixed at activation: a live grant-store revocation
    /// takes effect for service gating at the next activation (a new
    /// generation snapshots the new grant set), not mid-generation.
    /// `service_resolve`/`service_call` re-check this snapshot on every
    /// lookup and every call, so a generation renewed after revocation
    /// fails closed immediately with no restart beyond re-activation.
    ///
    /// Narrowing the snapshot on a live generation takes effect at once:
    /// `service_resolve`/`service_call` re-read it on the next lookup or
    /// call, so no process restart is needed for the gate to close.
    pub fn set_granted_capabilities(&self, mut capabilities: Vec<String>) {
        capabilities.sort();
        capabilities.dedup();
        *self.granted_capabilities.borrow_mut() = capabilities;
    }

    /// Whether this generation's activation grant snapshot holds
    /// `capability` (exact id match, no implication). Used for per-recipient
    /// event payload redaction (CTX-0899); the default empty snapshot holds
    /// nothing, so a missing setter call fails closed.
    ///
    /// O(log g) over the sorted snapshot.
    #[must_use]
    pub fn has_granted_capability(&self, capability: &str) -> bool {
        self.granted_capabilities
            .borrow()
            .binary_search_by(|granted| granted.as_str().cmp(capability))
            .is_ok()
    }

    /// Record this generation's manifest `lazy.events` (activation snapshot).
    ///
    /// `bitty.debug.trace` opens traces scoped to exactly this set, the same
    /// precondition `bitty.events.subscribe` enforces; an empty set (the
    /// default) means traces never record anything.
    pub fn set_declared_events(&self, kinds: BTreeSet<String>) {
        *self.declared_events.borrow_mut() = kinds;
    }

    /// Record this generation's manifest `[settings]` keys (CTX-1195,
    /// activation snapshot).
    ///
    /// `bitty.settings.get` serves exactly this set; any other key fails
    /// closed with `E_SETTINGS_UNDECLARED`. An empty set (the default) reads
    /// nothing — deny-by-default, matching every other generation gate.
    pub fn set_declared_settings(&self, keys: BTreeSet<String>) {
        *self.declared_settings.borrow_mut() = keys;
    }

    /// Attach the runtime-shared sanitized debug view (`bitty.debug.inspect`).
    ///
    /// Without it the `plugins`/`commands`/`events` targets fail closed with
    /// `E_NOT_IMPLEMENTED`.
    pub fn set_debug_view(&self, view: Rc<RefCell<DebugView>>) {
        *self.debug_view.borrow_mut() = Some(view);
    }

    /// Attach the runtime-shared trace hub (`bitty.debug.trace`).
    ///
    /// Without it trace calls fail closed with `E_NOT_IMPLEMENTED`.
    pub fn set_trace_hub(&self, hub: Rc<RefCell<TraceHub>>) {
        *self.trace_hub.borrow_mut() = Some(hub);
    }

    fn trace_hub_or_unavailable(&self) -> Result<Rc<RefCell<TraceHub>>, BridgeError> {
        self.trace_hub
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.debug.trace"))
    }
}

/// Typed approval mismatch for `config.set` (CTX-1613, bitty#2393).
///
/// Never echoes the supplied or expected digest: a plugin learns only that
/// the approval did not match the exact rendered change.
fn config_approval_mismatch() -> BridgeError {
    BridgeError::new(
        "runtime",
        "E_CONFIG_APPROVAL_MISMATCH",
        "config.set approval does not match this exact rendered change",
    )
}

/// Inherent `config.manage` core (CTX-1613, bitty#2393).
///
/// Split from the [`HostServices`] impl so tests can inject a start
/// instant while the bridge keeps a thin, argument-checking surface.
impl PluginServices {
    /// Grant and safe-mode gate shared by `config.preview`/`config.set`
    /// (CTX-1613, bitty#2393).
    ///
    /// Safe mode denies even with the grant recorded, and an ungranted
    /// generation learns nothing about the allowlist or the backend.
    fn config_authorize(&self) -> Result<(), BridgeError> {
        if self.safe_mode.get() || !self.config_manage_granted.get() {
            return Err(BridgeError::capability_denied("config.manage"));
        }
        Ok(())
    }

    /// Charge one call and its rendered bytes after shape validation.
    ///
    /// Bounds are fail-closed: the call budget (16/s), the byte budget
    /// (4 KiB/s), and the 10 ms authorization wall budget refuse without
    /// clamping. The durable write is dispatched only after every check.
    fn config_charge(&self, started: Instant, bytes: usize) -> Result<(), BridgeError> {
        check_result_rows(1)?;
        if started.elapsed() > Duration::from_millis(CONFIG_SET_MAX_WALL_MS) {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                "config.manage exceeded its 10 ms authorization budget",
            ));
        }
        if !self.config_gate.borrow_mut().charge(Instant::now(), bytes) {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                "config.manage exceeded 16 calls or 4096 bytes per second",
            ));
        }
        Ok(())
    }

    /// Render one change through the backend and issue its approval token.
    fn config_preview_at(
        &self,
        key: &str,
        value: &LuaValue,
        started: Instant,
    ) -> Result<ConfigPreviewToken, BridgeError> {
        self.config_authorize()?;
        // Exact allowlist membership: no path, argv, URL, or family prefix
        // reaches the backend. The rejected key is never echoed.
        let setting = writable_setting(key).ok_or_else(config_key_error)?;
        let value = ConfigSettingValue::from_lua(setting.kind, value)?;
        let change = ConfigChange::new(setting, value);
        if change.rendered().len() > CONFIG_SET_MAX_RENDERED_BYTES {
            return Err(BridgeError::new(
                "validation",
                "E_CONFIG_VALUE",
                "rendered config change exceeds its byte bound",
            ));
        }
        let backend = self
            .config_backend
            .borrow()
            .clone()
            .ok_or_else(config_unavailable)?;
        self.config_charge(started, change.rendered().len())?;
        // The backend renders the exact candidate user-layer bytes,
        // including pinned-table seeds, so the token binds what will be
        // committed rather than the one leaf the plugin named.
        let rendering: ConfigPreviewRendering = backend
            .preview(&self.plugin_id, &change)
            .map_err(|failure| config_backend_error(failure, change.key()))?;
        if rendering.changed_keys.len() > CONFIG_PREVIEW_MAX_KEYS
            || rendering.rendered.len() > CONFIG_PREVIEW_MAX_RENDERED_BYTES
            || rendering.candidate_digest.len() != 64
            || !rendering
                .candidate_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                "config.preview rendering exceeds its bounds",
            ));
        }
        let digest = approval_digest(&self.plugin_id, &change, &rendering);
        self.config_gate.borrow_mut().issue(
            digest.clone(),
            change.key(),
            change.value().clone(),
            rendering.candidate_digest,
        );
        Ok(ConfigPreviewToken {
            digest,
            rendered: rendering.rendered,
            changed_keys: rendering.changed_keys,
        })
    }

    fn config_preview(
        &self,
        key: &str,
        value: &LuaValue,
    ) -> Result<ConfigPreviewToken, BridgeError> {
        self.config_preview_at(key, value, Instant::now())
    }

    /// Apply one change bound to a preview digest issued by this generation.
    ///
    /// `expiry` is the bridge's cheap-call deadline. It is checked before the
    /// approval token is consumed and before the backend is called, so an
    /// already-expired call never commits. Once the backend returns, the
    /// caller must not turn a slow `fsync` into a timeout: the bridge uses
    /// the mutating-call guard for exactly this reason.
    fn config_set_with_expiry(
        &self,
        key: &str,
        value: &LuaValue,
        digest: &str,
        started: Instant,
        expiry: Instant,
    ) -> Result<ConfigSetOutcome, BridgeError> {
        self.config_authorize()?;
        let setting = writable_setting(key).ok_or_else(config_key_error)?;
        let value = ConfigSettingValue::from_lua(setting.kind, value)?;
        let change = ConfigChange::new(setting, value);
        if change.rendered().len() > CONFIG_SET_MAX_RENDERED_BYTES {
            return Err(BridgeError::new(
                "validation",
                "E_CONFIG_VALUE",
                "rendered config change exceeds its byte bound",
            ));
        }
        // The digest is lowercase SHA-256 hex; anything else is a client
        // bug and refuses before the token lookup, without echoing the token.
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(config_approval_mismatch());
        }
        let backend = self
            .config_backend
            .borrow()
            .clone()
            .ok_or_else(config_unavailable)?;
        // Charge before consuming the token: a budget refusal leaves the
        // approval usable inside the window, while an accepted call spends
        // it exactly once.
        self.config_charge(started, change.rendered().len())?;
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        let Some(candidate_digest) =
            self.config_gate
                .borrow_mut()
                .consume(digest, change.key(), change.value())
        else {
            return Err(BridgeError::new(
                "runtime",
                "E_CONFIG_APPROVAL_REQUIRED",
                "config.set requires a fresh config.preview token for this exact change",
            ));
        };
        // The backend re-renders the candidate and refuses (without writing)
        // unless the exact bytes match the approved digest, so a profile or
        // concurrent edit between preview and set cannot commit a different
        // change under the same approval.
        match backend.apply(&self.plugin_id, &change, &candidate_digest) {
            Ok(ConfigApplyOutcome::Applied) => Ok(ConfigSetOutcome {
                applied: true,
                reload: "auto",
            }),
            Ok(ConfigApplyOutcome::Unchanged) => Ok(ConfigSetOutcome {
                applied: false,
                reload: "none",
            }),
            // Only the path-free failure class crosses the boundary; the
            // backend's own diagnostic may name a host path and stays local.
            Err(failure) => Err(config_backend_error(failure, change.key())),
        }
    }

    /// Test convenience: apply with the default 10 ms authorization budget.
    #[cfg(test)]
    fn config_set(
        &self,
        key: &str,
        value: &LuaValue,
        digest: &str,
    ) -> Result<ConfigSetOutcome, BridgeError> {
        self.config_set_with_expiry(
            key,
            value,
            digest,
            Instant::now(),
            Instant::now() + Duration::from_millis(CONFIG_SET_MAX_WALL_MS),
        )
    }
}

impl HostServices for PluginServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key))
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        self.store.borrow_mut().set(key, value)
    }

    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        if key.is_empty() || key.len() > 128 {
            return Err(BridgeError::new(
                "validation",
                "E_SETTINGS_KEY_INVALID",
                "settings key must be 1..128 bytes",
            ));
        }
        // CTX-1195: the manifest declares exactly the keys this generation
        // reads. An undeclared key fails closed here — settings are never a
        // side channel between plugins, and no config value grants anything.
        if !self.declared_settings.borrow().contains(key) {
            return Err(BridgeError::new(
                "validation",
                "E_SETTINGS_UNDECLARED",
                format!(
                    "settings key '{key}' is not declared in this plugin's manifest [settings]"
                ),
            ));
        }
        Ok(self.settings.get(key))
    }

    fn config_preview(
        &self,
        key: &str,
        value: &LuaValue,
    ) -> Result<(String, String, Vec<String>), BridgeError> {
        let preview = PluginServices::config_preview(self, key, value)?;
        Ok((
            preview.digest,
            preview.rendered,
            preview
                .changed_keys
                .into_iter()
                .map(str::to_string)
                .collect(),
        ))
    }

    fn config_set(
        &self,
        key: &str,
        value: &LuaValue,
        digest: &str,
        expiry: Instant,
    ) -> Result<(bool, &'static str), BridgeError> {
        let outcome = PluginServices::config_set_with_expiry(
            self,
            key,
            value,
            digest,
            Instant::now(),
            expiry,
        )?;
        Ok((outcome.applied, outcome.reload))
    }

    fn env_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        validate_env_key(key)?;
        // CTX-0830: Check wildcard grants
        let granted = self
            .env_grants
            .borrow()
            .iter()
            .any(|grant| bitty_lua::env_grant_authorizes(grant, key));
        if !granted {
            // Desensitized denial: ungranted keys share the backend-absent
            // code, so callers cannot probe which variables exist.
            return Err(BridgeError::not_implemented("bitty.env.get"));
        }
        let value = self.env_source.borrow().get(key);
        match value {
            None => Ok(None),
            Some(value) => {
                if value.len() > MAX_ENV_VALUE_BYTES {
                    return Err(BridgeError::new(
                        "budget",
                        "E_DEF_LIMIT",
                        format!("env value exceeds {MAX_ENV_VALUE_BYTES} bytes"),
                    ));
                }
                Ok(Some(LuaValue::String(value)))
            }
        }
    }

    fn env_has(&self, key: &str) -> Result<bool, BridgeError> {
        validate_env_key(key)?;
        // CTX-0830: Check wildcard grants
        let granted = self
            .env_grants
            .borrow()
            .iter()
            .any(|grant| bitty_lua::env_grant_authorizes(grant, key));
        if !granted {
            return Err(BridgeError::not_implemented("bitty.env.has"));
        }
        Ok(self.env_source.borrow().get(key).is_some())
    }

    fn fs_list(&self, prefix: &str, max_entries: usize) -> Result<LuaValue, BridgeError> {
        self.fs_list_with_expiry(
            prefix,
            max_entries,
            Instant::now() + super::fs_list::LIST_SCAN_BUDGET,
        )
    }

    /// Enqueue one bounded native-component request (CTX-2254, bitty#2254).
    ///
    /// Enqueue-time gate only: the generation must hold a granted
    /// `network.connect` capability and carry its manifest generation, and the
    /// queue must have room. The runtime re-authorizes canonically
    /// (`Runtime::submit_component_request`) before anything reaches the
    /// broker, so this check can never widen authority; it only keeps a
    /// generation that could never be authorized from consuming the queue.
    fn component_request(&self, request: &ComponentBridgeRequest) -> Result<u64, BridgeError> {
        let queue = self
            .component_queue
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.component.request"))?;
        let granted = self.granted_capabilities.borrow();
        if !granted
            .iter()
            .any(|capability| capability.starts_with("network.connect"))
        {
            return Err(component_bridge_error(
                COMPONENT_CLASS_CAPABILITY_DENIED,
                "the plugin generation holds no granted network.connect capability",
            ));
        }
        drop(granted);
        let manifest_hash = self
            .component_manifest_hash
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.component.request"))?;
        queue.borrow_mut().enqueue(
            &self.plugin_id,
            &manifest_hash,
            request.clone(),
            Instant::now(),
        )
    }

    /// Read one completed component request (CTX-2254, bitty#2254).
    fn component_poll(&self, ticket: u64) -> Result<Option<LuaValue>, BridgeError> {
        let queue = self
            .component_queue
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.component.poll"))?;
        match queue.borrow_mut().poll(&self.plugin_id, ticket)? {
            ComponentPollOutcome::Pending => Ok(None),
            ComponentPollOutcome::Ready(outcome) => outcome_value(&outcome).map(Some),
        }
    }

    fn fs_list_with_expiry(
        &self,
        prefix: &str,
        max_entries: usize,
        expiry: Instant,
    ) -> Result<LuaValue, BridgeError> {
        self.fs_list_at(prefix, max_entries, expiry, Instant::now())
    }

    fn terminal_snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if !self.terminal_read {
            return Err(BridgeError::capability_denied("terminal.semantic-read"));
        }
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "only the semantic scope is supported",
            ));
        }
        let snapshot = self.snapshot.snapshot(scope)?;
        if store::encode_json(&snapshot).len() > SNAPSHOT_MAX_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_SNAPSHOT_TOO_LARGE",
                "terminal snapshot exceeds the 256 KiB ceiling",
            ));
        }
        Ok(snapshot)
    }

    fn history_query(
        &self,
        source: HistoryQuerySource,
        request: &HistoryQueryRequest,
    ) -> Result<LuaValue, BridgeError> {
        // CTX-1352: the closed capability head is the first gate, exactly
        // like the other per-domain verbs. Absent grants deny with the
        // family's typed category; the gate below would also deny, but this
        // check keeps the no-backend and no-grant cases distinguishable and
        // makes the grant check independently mutation-testable.
        if !self.has_granted_capability(source.capability_head()) {
            return Err(BridgeError::new(
                "runtime",
                HistoryDenialKind::MissingGrant.code(),
                format!(
                    "{} denied for level 'third-party-lua' (family 'history')",
                    HistoryDenialKind::MissingGrant.code()
                ),
            ));
        }
        let provider = self
            .history_provider
            .borrow()
            .clone()
            .ok_or_else(history_unavailable)?;
        let core_source = history_source(source);
        // A provider may exist while one source has no backing
        // implementation (command history today). That is typed
        // unavailability for that source only, never a fabricated empty
        // page. Capture opt-in is a separate axis handled by the gate: an
        // available transcript with capture disabled denies with
        // E_HISTORY_CAPTURE_DISABLED.
        if !provider.source_available(core_source) {
            return Err(history_unavailable());
        }
        // Snapshot first: no store is a typed unavailability, never an
        // empty page and never a Lua nil-index.
        let snapshot = provider.snapshot()?;
        let scope = HistoryScope::new(
            core_source,
            request.panel.as_deref(),
            request.workspace.as_deref(),
        )
        .map_err(history_error)?;
        let mut gate = self.history_gate.borrow_mut();
        let now = Instant::now();
        if now.duration_since(self.history_window_start.get()) >= HISTORY_RATE_WINDOW {
            // A fresh gate resets per-window usage and drops the previous
            // window's per-call grants, so grant storage stays bounded by
            // one window's distinct scopes.
            *gate = HistoryGate::new(history_caps());
            self.history_grant_scopes.borrow_mut().clear();
            self.history_window_start.set(now);
        }
        gate.set_safe_mode(self.safe_mode.get());
        gate.set_capture(core_source, provider.capture_enabled(core_source));
        let plugin = PluginId::new(&self.plugin_id)
            .expect("plugin id validated before activation creates services");
        let grant_key = (
            core_source,
            request.panel.clone(),
            request.workspace.clone(),
        );
        // The scope above is validated before any grant is recorded. Grants
        // are cached per exact caller-named scope only up to a fixed bound;
        // overflow clears the grant list (never usage) and starts a fresh
        // bounded cache, so caller-chosen scopes cannot grow state without
        // limit inside one rate window.
        if !self.history_grant_scopes.borrow().contains(&grant_key) {
            if self.history_grant_scopes.borrow().len() >= HISTORY_GRANT_SCOPE_CACHE_MAX {
                gate.clear_grants();
                self.history_grant_scopes.borrow_mut().clear();
            }
            gate.issue_grant(HistoryGrant::standing(plugin.clone(), scope.clone()));
            self.history_grant_scopes.borrow_mut().insert(grant_key);
        }
        let op = match request.op {
            HistoryQueryOp::List => QueryOp::List,
            HistoryQueryOp::Tail => QueryOp::Tail,
            HistoryQueryOp::Search => QueryOp::Search {
                needle: request.needle.clone().unwrap_or_default(),
            },
        };
        let query = SnapshotQuery {
            client_version: HISTORY_READ_VERSION,
            scope,
            row_start: request.row_start,
            row_count: request.row_count,
            max_bytes: request.max_bytes,
            op,
        };
        let page = gate
            .query(&plugin, TrustLevel::ThirdPartyLua, &query, &snapshot)
            .map_err(history_error)?;
        Ok(history_page_value(&page))
    }

    fn has_terminal_submit(&self) -> bool {
        self.terminal_submit_granted.get() && self.submit_queue.borrow().is_some()
    }

    fn terminal_submit(&self, text: &str) -> Result<LuaValue, BridgeError> {
        // CTX-1239 (issue #2101): grant gate first, mirroring the snapshot
        // and panel verbs. The bridge additionally presence-gates the key,
        // so an ungranted Lua caller observes nil and never reaches this
        // error; this gate is the defense-in-depth layer for direct
        // callers. The grant IS the lease authorization for this path
        // (submit_host.rs: holder attribution arrives with the capability
        // grant lane): with the grant held, `check_terminal_submit` runs
        // with the lease satisfied and the budget charged only on enqueue
        // success, exactly like the native probe-then-commit. The drain
        // performs a mechanical PTY write of the already-authorized frame.
        if !self.terminal_submit_granted.get() {
            return Err(BridgeError::capability_denied("terminal.input.submit"));
        }
        let queue = self
            .submit_queue
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.terminal.submit"))?;
        // Validate (cap, framing) and price against a probe first so every
        // denial — and a full-queue drop below — leaves the generation's
        // budget untouched; the charge commits only after the frame queues.
        let mut probe = self.submit_budget.borrow().clone();
        let frame = match check_terminal_submit(text, true, &mut probe) {
            Ok(frame) => frame,
            Err(SubmitDeny::TooLarge { .. }) => {
                return Ok(submit_denied("too-large"));
            }
            Err(SubmitDeny::LeaseDenied) => {
                return Ok(submit_denied("lease-denied"));
            }
            Err(SubmitDeny::BudgetExceeded { .. }) => {
                return Ok(submit_denied("budget-exceeded"));
            }
        };
        let bytes = frame.len();
        if !queue.borrow_mut().push(QueuedSubmitRequest { frame }) {
            return Ok(submit_denied("queue-full"));
        }
        *self.submit_budget.borrow_mut() = probe;
        Ok(submit_accepted(bytes))
    }

    fn notify_show(&self, payload: &LuaValue) -> Result<bool, BridgeError> {
        if !self.platform_notify {
            return Err(BridgeError::capability_denied("platform.notify"));
        }
        let title = match payload.get("title") {
            Some(LuaValue::String(s)) => s.clone(),
            _ => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "notification title must be a string",
                ));
            }
        };
        let body = match payload.get("body") {
            Some(LuaValue::String(s)) => s.clone(),
            Some(LuaValue::Nil) | None => String::new(),
            _ => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "notification body must be a string",
                ));
            }
        };
        let urgency = match payload.get("urgency") {
            Some(LuaValue::String(s)) => s.clone(),
            _ => "normal".to_string(),
        };
        let accepted = self.notifications.borrow_mut().push(Notification {
            plugin_id: self.plugin_id.clone(),
            title,
            body,
            urgency,
        });
        Ok(accepted)
    }

    fn process_spawn(&self, args: &[String]) -> Result<LuaValue, BridgeError> {
        if !self.spawn_git.get() {
            return Err(BridgeError::capability_denied("process.spawn:git"));
        }
        let backend = self.spawn_backend.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                "E_SPAWN_UNAVAILABLE",
                "host spawn backend is not configured",
            )
        })?;
        backend(args)
    }

    fn process_spawn_with_expiry(
        &self,
        args: &[String],
        expiry: Instant,
    ) -> Result<LuaValue, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.process_spawn(args)
    }

    fn ui_mount(&self, slot: &str, component: &UiNode) -> Result<i64, BridgeError> {
        // The bridge already rejected unknown slots; re-check at the host
        // boundary so a direct caller can never reach the registry with one.
        let Some(ui_slot) = UiSlot::parse(slot) else {
            return Err(bitty_lua::ui::component_invalid(format!(
                "unknown UI slot '{slot}'"
            )));
        };
        {
            let access = self.ui_access.borrow();
            if !access.rich {
                return Err(BridgeError::capability_denied("ui.rich"));
            }
            if ui_slot == UiSlot::Overlay && !access.overlay {
                return Err(BridgeError::capability_denied("ui.overlay"));
            }
            // Accepted: `tabline` slot requires the canonical `workspaceline`
            // exclusive claim (ADR-0009 `LUA-OQ-7` plus the `[lazy].claims`
            // vocabulary; #1572 retired the bundled workspace manifest but
            // the claim stays valid for the future `bar` plugin).
            // Unclaimed mounts fail closed; the register/claim reservation is
            // owned by the plugin host. CTX-0974 purged the deprecated
            // `tabline` claim alias: only `workspaceline` satisfies the gate.
            let tabline_claimed = access
                .claims
                .iter()
                .any(|claim| claim.trim() == WORKSPACELINE_CLAIM);
            if ui_slot == UiSlot::Tabline && !tabline_claimed {
                return Err(BridgeError::new(
                    "validation",
                    "E_UI_CLAIM_REQUIRED",
                    "slot 'tabline' requires an exclusive claim declared in [lazy].claims",
                ));
            }
        }
        // CTX-0923: an accepted slot this host does not present fails closed
        // here, after the capability and claim gates, through the same
        // placement policy the band routing uses, so nothing is admitted and
        // then silently dropped at render time. CTX-1341: this includes
        // `left`/`right`, which have no vertical band host yet.
        if let UiSlotPlacement::Unsupported(reason) = ui_slot_placement(ui_slot) {
            return Err(unsupported_slot_error(ui_slot, reason));
        }
        self.ui_blocks
            .borrow_mut()
            .mount(ui_slot, component.clone())
    }

    /// Mount one validated rich tree into the `terminal`-attached scene slot
    /// (bitty#2785).
    ///
    /// Admission order: the call/claim gates run first, then the accepted
    /// scene admission (`SCN-1..5`) inside the Core scene host, then the
    /// generation-owned handle is recorded. A refused tree leaves the
    /// previous scene attached and mints nothing.
    fn ui_mount_scene(&self, slot: &str, node: &bitty_lua::UiRichNode) -> Result<i64, BridgeError> {
        let Some(ui_slot) = UiSlot::parse(slot) else {
            return Err(bitty_lua::ui::component_invalid(format!(
                "unknown UI slot '{slot}'"
            )));
        };
        if ui_slot != UiSlot::Terminal {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                format!("slot '{ui_slot}' has no rich scene surface"),
            ));
        }
        if !self.ui_access.borrow().rich {
            return Err(BridgeError::capability_denied("ui.rich"));
        }
        let backend = self.ui_scene_backend.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no terminal-attached scene surface",
            )
        })?;
        // A generation owns one terminal-slot scene: the host replaces the
        // previous scene atomically after admission and invalidates its
        // handle, so the slot's panel and handle space stay bounded by
        // generations, not by mount count.
        backend.attach(&self.plugin_id, self.ui_epoch.get(), node)
    }

    /// Whether `handle` is encoded as a terminal-slot scene handle.
    ///
    /// The tag lives in the handle value, so a released handle still routes
    /// through the rich reader and then reports `Ok(false)` instead of
    /// becoming a parse error for the v1 reader.
    fn ui_is_scene_handle(&self, handle: i64) -> bool {
        crate::scene_host::is_scene_handle(handle)
    }

    /// Replace one terminal-slot scene from a validated rich tree.
    fn ui_update_scene(
        &self,
        handle: i64,
        node: &bitty_lua::UiRichNode,
    ) -> Result<bool, BridgeError> {
        if !self.ui_access.borrow().rich {
            return Err(BridgeError::capability_denied("ui.rich"));
        }
        if !crate::scene_host::is_scene_handle(handle) {
            return Ok(false);
        }
        let Some(backend) = self.ui_scene_backend.borrow().clone() else {
            return Ok(false);
        };
        // Admission runs inside the host first: a refused tree keeps the last
        // good scene attached, so the scene never drops to a partial state.
        backend.update(handle, node)
    }

    /// Scene scope of a mounted block (issue #2411): only a horizontal band
    /// block has a row width that resolves group anchors.
    ///
    /// `None` for a stale or foreign handle: `ui.update` must keep returning
    /// its documented `Ok(false)` result for those instead of the bridge
    /// rejecting an anchored replacement tree as malformed.
    fn ui_scene_scope(&self, handle: i64) -> Option<UiSceneScope> {
        self.ui_blocks
            .borrow()
            .get(handle)
            .map(|block| match block.ui_slot() {
                UiSlot::Top | UiSlot::Bottom => UiSceneScope::Band,
                _ => UiSceneScope::Unanchored,
            })
    }

    fn ui_update(&self, handle: i64, component: &UiNode) -> Result<bool, BridgeError> {
        if !self.ui_access.borrow().rich {
            return Err(BridgeError::capability_denied("ui.rich"));
        }
        self.ui_blocks
            .borrow_mut()
            .update(handle, component.clone())
    }

    fn ui_overlay_acquire_with_expiry(
        &self,
        handle: i64,
        expiry: Instant,
    ) -> Result<(), BridgeError> {
        // Check-then-act: never transfer capture authority after the call
        // deadline (CTX-0464 path, CTX-0941).
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.require_overlay_available()?;
        let Some(capture) = self.overlay_capture() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui overlay capture surface",
            ));
        };
        // Accepted W-01 v2 gate: the focusable surface requires
        // `ui.overlay.focus` (deny-by-default, naming the capability). The
        // block itself was mounted through the v1 `overlay` slot, so the
        // mechanism path needs both grants; the spec path below needs only
        // the focus grant.
        self.require_overlay_focus()?;
        // Only a block this generation mounted into the focusable `overlay`
        // slot can own capture; a foreign or non-overlay handle is typed.
        let is_overlay = self
            .ui_blocks
            .borrow()
            .get(handle)
            .is_some_and(|block| block.ui_slot() == UiSlot::Overlay);
        if !is_overlay {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "handle is not a mounted overlay block of this generation",
            ));
        }
        let session_expiry = Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS);
        let previous = Self::live_overlay_session(&capture);
        capture
            .borrow_mut()
            .acquire(&self.plugin_id, handle, session_expiry)?;
        // CTX-0973: a successful acquire implies the previous owner (if any)
        // was expired and lazily finished inside `acquire` without a
        // runtime-mediated disposal. Dispose its transient spec surface so no
        // stale surface survives acquisition, then drop any remaining stale
        // spec surface of this generation. The live handle is skipped so
        // re-acquiring the same block keeps it.
        if let Some((plugin, expired)) = previous {
            if expired != handle {
                self.dispose_expired_spec_overlay(&plugin, expired);
            }
        }
        self.clear_stale_spec_overlays_except(handle);
        Ok(())
    }

    fn ui_overlay_acquire(&self, handle: i64) -> Result<(), BridgeError> {
        self.ui_overlay_acquire_with_expiry(
            handle,
            Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS),
        )
    }

    fn ui_overlay_acquire_with_spec_and_expiry(
        &self,
        title: &str,
        placeholder: &str,
        expiry: Instant,
    ) -> Result<i64, BridgeError> {
        // Check-then-act: never transfer capture authority after the call
        // deadline.
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.require_overlay_available()?;
        let Some(capture) = self.overlay_capture() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui overlay capture surface",
            ));
        };
        // Single-grant path: only `ui.overlay.focus` is required. The
        // surface is mounted directly into the generation registry (same
        // v1 scene budgets as `ui.mount`) without passing the v1
        // `ui.overlay` slot gate.
        self.require_overlay_focus()?;
        if title.len() > OVERLAY_SPEC_TITLE_MAX_BYTES {
            return Err(BridgeError::value(
                "E_VALUE_BYTES",
                "ui.overlay.acquire title exceeds size limit",
            ));
        }
        if placeholder.len() > OVERLAY_SPEC_PLACEHOLDER_MAX_BYTES {
            return Err(BridgeError::value(
                "E_VALUE_BYTES",
                "ui.overlay.acquire placeholder exceeds size limit",
            ));
        }
        if title.len() + placeholder.len() > OVERLAY_CALL_MAX_BYTES {
            return Err(BridgeError::value(
                "E_VALUE_BYTES",
                "ui.overlay.acquire spec exceeds size limit",
            ));
        }
        let mut children = Vec::new();
        if !title.is_empty() {
            children.push(UiNode::text(title.to_string()));
        }
        if !placeholder.is_empty() {
            children.push(UiNode::text(placeholder.to_string()));
        }
        if children.is_empty() {
            children.push(UiNode::text(String::new()));
        }
        let node = UiNode::column(children);
        let handle = self.ui_blocks.borrow_mut().mount(UiSlot::Overlay, node)?;
        let session_expiry = Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS);
        let previous = Self::live_overlay_session(&capture);
        if let Err(error) = capture
            .borrow_mut()
            .acquire(&self.plugin_id, handle, session_expiry)
        {
            // A second session cannot start while one is active: drop the
            // freshly mounted surface so a denied acquire leaves no orphan
            // block behind.
            self.remove_ui_block(handle);
            return Err(error);
        }
        // CTX-0973: a successful acquire implies the previous owner (if any)
        // was expired and lazily finished inside `acquire` without a
        // runtime-mediated disposal. The fresh handle never equals the expired
        // one, so disposing first cannot touch the live surface.
        if let Some((plugin, expired)) = previous {
            if expired != handle {
                self.dispose_expired_spec_overlay(&plugin, expired);
            }
        }
        self.remember_spec_overlay(handle);
        Ok(handle)
    }

    fn ui_overlay_acquire_with_spec(
        &self,
        title: &str,
        placeholder: &str,
    ) -> Result<i64, BridgeError> {
        self.ui_overlay_acquire_with_spec_and_expiry(
            title,
            placeholder,
            Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS),
        )
    }

    fn ui_overlay_update_with_expiry(
        &self,
        handle: i64,
        component: &UiNode,
        expiry: Instant,
    ) -> Result<bool, BridgeError> {
        // Check-then-act: an expired call leaves last-known-good content.
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.require_overlay_available()?;
        let Some(capture) = self.overlay_capture() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui overlay capture surface",
            ));
        };
        self.require_overlay_focus()?;
        // Only the live owner may replace content; a foreign, released, or
        // stale handle fails with `E_UI_NOT_OWNER` and keeps previous
        // content. The block must also be this generation's overlay block.
        if !capture.borrow().is_owner(&self.plugin_id, handle) {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "no focusable-overlay input capture is held for this handle",
            ));
        }
        let is_overlay = self
            .ui_blocks
            .borrow()
            .get(handle)
            .is_some_and(|block| block.ui_slot() == UiSlot::Overlay);
        if !is_overlay {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "handle is not a mounted overlay block of this generation",
            ));
        }
        let updated = self
            .ui_blocks
            .borrow_mut()
            .update(handle, component.clone())?;
        if updated {
            // Content replacement proves the session is live: extend the
            // idle deadline like a poll does.
            capture.borrow_mut().refresh_owner(&self.plugin_id, handle);
        }
        Ok(updated)
    }

    fn ui_overlay_update(&self, handle: i64, component: &UiNode) -> Result<bool, BridgeError> {
        self.ui_overlay_update_with_expiry(
            handle,
            component,
            Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS),
        )
    }

    fn ui_overlay_show_with_expiry(
        &self,
        handle: i64,
        expiry: Instant,
    ) -> Result<bool, BridgeError> {
        // Check-then-act: never admit a surface after the call deadline.
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        let overlay = self.require_presentation_overlay()?;
        // Only a block this generation mounted into the `overlay` slot can
        // be presented; a foreign or non-overlay handle is typed. The block
        // check also rejects a handle from a previous generation (epoch
        // handles never alias across a reload).
        let is_overlay = self
            .ui_blocks
            .borrow()
            .get(handle)
            .is_some_and(|block| block.ui_slot() == UiSlot::Overlay);
        if !is_overlay {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "handle is not a mounted overlay block of this generation",
            ));
        }
        overlay.borrow_mut().show(&self.plugin_id, handle)
    }

    fn ui_overlay_show(&self, handle: i64) -> Result<bool, BridgeError> {
        self.ui_overlay_show_with_expiry(
            handle,
            Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS),
        )
    }

    fn ui_overlay_hide(&self, handle: i64) -> Result<bool, BridgeError> {
        // Release-style cleanup path: no grant or safe-mode gate, so a
        // surface whose grant was revoked mid-session can still be removed.
        // No shared state means there is nothing this host could have shown.
        let Some(overlay) = self.presentation_overlay() else {
            return Ok(false);
        };
        Ok(overlay.borrow_mut().hide(&self.plugin_id, handle))
    }

    fn ui_overlay_release(&self, handle: i64) -> Result<bool, BridgeError> {
        self.ui_overlay_release_with_reason(handle, None)
    }

    fn ui_overlay_release_with_reason(
        &self,
        handle: i64,
        reason: Option<&str>,
    ) -> Result<bool, BridgeError> {
        // Release never checks the grant or safe mode: a capture whose grant
        // was revoked mid-session must still be releasable, and cleanup can
        // never wedge input.
        let Some(capture) = self.overlay_capture() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui overlay capture surface",
            ));
        };
        let disposition = match reason {
            None => DEFAULT_RELEASE_REASON.to_string(),
            Some(text) if is_owner_release_reason(text) => text.to_string(),
            Some(_) => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "ui.overlay.release reason must be 'submitted' or 'cancelled'",
                ));
            }
        };
        // Pre-change idempotent contract: releasing anything but the live
        // session is a success-without-effect `Ok(false)` — an already-ended
        // session of this generation, a handle never acquired, or a stale
        // handle from before another session started — so cleanup in
        // cancel/unload handlers never throws. Only ending somebody else's
        // *live* session denies with `E_UI_NOT_OWNER`.
        let mut guard = capture.borrow_mut();
        if guard.is_owner(&self.plugin_id, handle) {
            guard.release_with_reason(&self.plugin_id, handle, &disposition);
            drop(guard);
            // W-29 successor: a direct overlay release also ends this
            // generation's targeting session, so annotations, bindings, and
            // the session marker cannot outlive the capture.
            if self
                .target_session_overlay
                .borrow()
                .is_some_and(|owned| owned == handle)
            {
                self.clear_targeting_session();
            }
            // The ended session's transient surface leaves with it; a
            // retained mechanism-path block is untouched (never spec-created).
            self.remove_spec_overlay_block(handle);
            return Ok(true);
        }
        if guard.is_live_handle(handle) {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "no focusable-overlay input capture is held for this handle",
            ));
        }
        Ok(false)
    }

    fn ui_overlay_poll(&self, handle: i64, max: usize) -> Result<Vec<OverlayInput>, BridgeError> {
        let Some(capture) = self.overlay_capture() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui overlay capture surface",
            ));
        };
        capture.borrow_mut().poll(&self.plugin_id, handle, max)
    }

    fn ui_overlay_poll_detailed(
        &self,
        handle: i64,
        max: usize,
    ) -> Result<OverlayPoll, BridgeError> {
        self.require_overlay_available()?;
        let Some(capture) = self.overlay_capture() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no ui overlay capture surface",
            ));
        };
        self.require_overlay_focus()?;
        let detailed = capture
            .borrow_mut()
            .poll_detailed(&self.plugin_id, handle, max)?;
        Ok(OverlayPoll {
            active: detailed.status == "active",
            seq: detailed.seq,
            events: detailed.events,
            overflowed: detailed.overflowed,
            reason: detailed.reason,
        })
    }

    fn ui_targets_snapshot(&self, max: usize) -> Result<Vec<TargetSnapshotRow>, BridgeError> {
        self.require_targets_capability()?;
        // Read-only: collect into a scratch registry, never the shared one
        // that live sessions bind their generations against.
        let _ = self.require_target_registry()?;
        let infos = self.core_target_infos()?;
        let offers = self.core_target_offers(&infos)?;
        let mut scratch = TargetRegistry::new();
        let mediator = self.targeting_mediator(offers)?;
        let snapshot = mediator
            .collect(&mut scratch)
            .map_err(map_targets_provider_error)?;
        let entries = snapshot.entries();
        let take = max.min(entries.len()).min(MAX_SNAPSHOT_TARGETS);
        Ok(entries
            .iter()
            .take(take)
            .enumerate()
            .map(|(index, entry)| {
                let target = entry.target();
                let kind = targets_kind_string(&target);
                let id = target_raw_id(&target);
                let info = Self::core_info_for(&infos, &target);
                let workspace = info.map_or_else(
                    || (kind == "workspace").then_some(id),
                    |info| info.workspace,
                );
                let hint = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1);
                TargetSnapshotRow {
                    handle: hint,
                    id,
                    kind,
                    tier: entry.tier().to_string(),
                    workspace,
                    rect: info.and_then(|info| info.rect),
                    generation: target_generation(&target),
                }
            })
            .collect())
    }

    #[allow(clippy::type_complexity)]
    fn ui_targets_register(
        &self,
        name: &str,
        tier: &str,
        targets: &[(String, u64)],
    ) -> Result<bool, BridgeError> {
        self.require_targets_capability()?;
        if tier == "core" {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                "the 'core' provider tier is not registrable from Lua",
            ));
        }
        if tier != "plugin" && tier != "derived" {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!("unknown provider tier '{tier}'"),
            ));
        }
        if targets.len() > MAX_SNAPSHOT_TARGETS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many snapshot targets: max {MAX_SNAPSHOT_TARGETS}"),
            ));
        }
        let mut offers = Vec::with_capacity(targets.len());
        for (kind, id) in targets {
            offers.push(targets_offer(kind, *id)?);
        }
        // Both tiers map onto the existing Derived lens (no Plugin-tier source
        // exists in `bitty-ui`; reuse avoids any new targeting type).
        let lens = DerivedProvider::new(name, offers).map_err(map_targets_provider_error)?;
        let lenses = self.require_target_lenses()?;
        let mut guard = lenses.borrow_mut();
        if let Some(existing) = guard
            .iter_mut()
            .find(|(owner, lens)| owner == &self.plugin_id && lens.name() == name)
        {
            existing.1 = lens;
            return Ok(true);
        }
        if guard.iter().any(|(_, lens)| lens.name() == name) {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!("target lens '{name}' is already registered"),
            ));
        }
        if guard.len() >= MAX_TARGET_PROVIDERS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many target lenses: max {MAX_TARGET_PROVIDERS}"),
            ));
        }
        guard.push((self.plugin_id.clone(), lens));
        Ok(true)
    }

    fn ui_targets_unregister(&self, name: &str) -> Result<bool, BridgeError> {
        self.require_targets_capability()?;
        let lenses = self.require_target_lenses()?;
        let mut guard = lenses.borrow_mut();
        let before = guard.len();
        guard.retain(|(owner, lens)| !(owner == &self.plugin_id && lens.name() == name));
        Ok(guard.len() != before)
    }

    fn ui_labels_set_policy(&self, home: &str, overflow: &str) -> Result<(), BridgeError> {
        self.require_targets_capability()?;
        let policy = LabelPolicy::new(home, overflow).map_err(map_targets_label_error)?;
        let allocator = self.require_label_allocator()?;
        *allocator.borrow_mut() = LabelAllocator::new(policy);
        Ok(())
    }

    fn ui_labels_assign(
        &self,
        anchors: &[(u16, u16)],
        width: u16,
    ) -> Result<Vec<String>, BridgeError> {
        self.require_targets_capability()?;
        if anchors.len() > MAX_HINT_TARGETS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many targets for labels: max {MAX_HINT_TARGETS}"),
            ));
        }
        let allocator = self.require_label_allocator()?;
        let points: Vec<Point> = anchors.iter().map(|(x, y)| Point::new(*x, *y)).collect();
        allocator
            .borrow()
            .assign(&points, width)
            .map_err(map_targets_label_error)
    }

    #[allow(clippy::type_complexity)]
    fn ui_targets_session_start(
        &self,
        overlay_handle: i64,
        width: u16,
        anchors: &[(u16, u16)],
        commands: &[String],
    ) -> Result<Vec<String>, BridgeError> {
        self.require_targets_capability()?;
        // Acquire the W-28 transient capture first (no new session type).
        self.ui_overlay_acquire(overlay_handle)?;
        match self.targets_session_start_inner(width, anchors, commands) {
            Ok(labels) => {
                *self.target_session_overlay.borrow_mut() = Some(overlay_handle);
                Ok(labels)
            }
            Err(error) => {
                let _ = self.ui_overlay_release(overlay_handle);
                self.clear_targeting_session();
                Err(error)
            }
        }
    }

    fn ui_targets_session_cancel(&self, overlay_handle: i64) -> Result<bool, BridgeError> {
        // Idempotent and never capability-gated (a revoked grant must still
        // free input), mirroring `ui_overlay_release`.
        let owned = self
            .target_session_overlay
            .borrow()
            .is_some_and(|handle| handle == overlay_handle);
        if owned {
            self.clear_targeting_session();
        }
        let released = self.ui_overlay_release(overlay_handle)?;
        Ok(owned || released)
    }

    fn ui_targets_dispatch(&self, label: &str) -> Result<String, BridgeError> {
        self.require_targets_capability()?;
        if self.target_session_overlay.borrow().is_none() {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "no active targeting session for this generation",
            ));
        }
        let registry = self.require_target_registry()?;
        let outcome = {
            let dispatcher = self.target_dispatcher.borrow();
            let live = registry.borrow();
            dispatcher.dispatch(label, &live)
        };
        match outcome {
            Ok(command) => {
                // Single-shot: the annotation batch and bindings leave with
                // the selection, so a replayed label fails closed.
                self.clear_targeting_session();
                Ok(command.as_str().to_string())
            }
            Err(error) => Err(map_targets_dispatch_error(error)),
        }
    }

    fn service_provide_check(&self, iface: &str) -> Result<(), BridgeError> {
        if self.service_directory.borrow().is_none() {
            return Err(BridgeError::not_implemented("bitty.services.provide"));
        }
        // Dynapi arbitration (bitty#2003): provide-side names the short
        // interface only (the owner is implied — the providing plugin),
        // never a Core-reserved spelling. Manifest validation is primary;
        // this re-check covers direct host callers with the same naming
        // diagnostic.
        match parse_service_reference(iface, "services.provide") {
            Ok((None, _)) => {}
            Ok((Some(_), short)) => {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_UNDECLARED",
                    format!(
                        "service '{iface}' is not declared in services.provided (provide the short interface '{short}')"
                    ),
                ));
            }
            Err(error) => {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_UNDECLARED",
                    format!("service '{iface}' is not declared in services.provided ({error})"),
                ));
            }
        }
        let declared = self
            .service_provided
            .borrow()
            .iter()
            .find(|service| service.iface == iface)
            .cloned();
        let Some(declared) = declared else {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_UNDECLARED",
                format!("service '{iface}' is not declared in services.provided"),
            ));
        };
        // Dynapi gating (bitty#2005): the provide projects the manifest's
        // backing heads through this generation's grant. Unbacked provides
        // are rejected with a naming diagnostic; heads this generation was
        // never granted (head-spoofing) fail closed as capability denials.
        // No new authority is minted anywhere on this path.
        if declared.heads.is_empty() {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_UNDECLARED",
                format!(
                    "service '{iface}' declares no backing capability head (services.provided.{iface}.heads)"
                ),
            ));
        }
        for head in &declared.heads {
            if !self.service_heads_covered(std::slice::from_ref(head)) {
                return Err(BridgeError::capability_denied(head.as_str()));
            }
        }
        Ok(())
    }

    fn service_resolve(
        &self,
        iface: &str,
        req: Option<&str>,
        optional: bool,
    ) -> Result<Option<ServiceRoute>, BridgeError> {
        let directory = self.service_directory.borrow().clone();
        let Some(directory) = directory else {
            return Err(BridgeError::not_implemented("bitty.services.get"));
        };
        // Declaration discipline: the caller manifest's `services.required`
        // entry is mandatory. `opts.version` overrides its requirement text
        // but never substitutes for the declaration. An owner-qualified
        // lookup requires the qualified entry (`owner:iface`); a short
        // entry never authorizes a qualified lookup.
        let manifest_req = self
            .service_required
            .borrow()
            .iter()
            .find(|(name, _)| name == iface)
            .map(|(_, req)| req.clone());
        let Some(manifest_req) = manifest_req else {
            return Err(service_resolution_error(
                iface,
                "it is not declared in services.required".to_string(),
            ));
        };
        let effective = req.unwrap_or(&manifest_req).to_string();
        // Dynapi arbitration (bitty#2003): references are short
        // (`calc.add`) or owner-qualified (`xuepoo.calc:calc.add`).
        // Malformed or Core-reserved references fail closed with the same
        // resolution code. A qualified reference pins the owner; a short
        // reference shared by two or more visible owners fails closed
        // naming each owner (every claimant stays reachable through its
        // qualified form), even when `optional` is set — ambiguity is a
        // naming error, not absence.
        //
        // Deterministic pick over the visible records (one owner: the
        // pinned one, or the single unambiguous claimant): highest
        // satisfying version wins. Unparseable requirements
        // or versions satisfy nothing (fail-closed, same grammar as the
        // policy registry gate). Negotiation is pure matching, never
        // authority (dynapi negotiation, bitty#2006): the broker always
        // serves the highest compatible version — never a silent downgrade
        // to a lower pick — and an empty intersection fails closed with
        // the visible versions listed, never a nearest-version guess.
        //
        // Dynapi gating (bitty#2005): records whose backing heads this
        // generation's grant does not cover are skipped as if absent, so an
        // ungranted consume is indistinguishable from no provider
        // (`E_SERVICE_RESOLUTION`, or nil when optional — the same posture
        // as `bitty.env`, whose ungranted reads share the backend-absent
        // code). Ambiguity is judged over visible records only, and the
        // diagnostic below lists visible versions only, so neither ever
        // leaks an ungranted provider. No new authority is minted
        // anywhere on this path.
        let (owner, short) = parse_service_reference(iface, "services.get")
            .map_err(|error| service_resolution_error(iface, error.to_string()))?;
        let mut best: Option<(Version, String, ServiceRecord)> = None;
        let mut visible_owners: BTreeSet<String> = BTreeSet::new();
        let mut visible_versions: BTreeSet<String> = BTreeSet::new();
        for record in directory.borrow().active_for(&short) {
            if owner
                .as_deref()
                .is_some_and(|wanted| wanted != record.provider)
            {
                continue;
            }
            if !self.service_heads_covered(&record.heads) {
                continue;
            }
            visible_versions.insert(record.version.clone());
            if !service_version_satisfies(&record.version, &effective) {
                continue;
            }
            visible_owners.insert(record.provider.clone());
            let Ok(version) = Version::parse(&record.version) else {
                continue;
            };
            let replace = match &best {
                Some((best_version, best_provider, _)) => {
                    version > *best_version
                        || (version == *best_version && record.provider < *best_provider)
                }
                None => true,
            };
            if replace {
                best = Some((version, record.provider.clone(), record));
            }
        }
        if owner.is_none() && visible_owners.len() > 1 {
            let owners = visible_owners
                .into_iter()
                .map(|name| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(service_resolution_error(
                iface,
                format!(
                    "interface '{short}' is claimed by multiple owners ({owners}): use an owner-qualified name ('<owner>:{short}')"
                ),
            ));
        }
        let Some((_, _, record)) = best else {
            if optional {
                return Ok(None);
            }
            return Err(service_resolution_error(
                iface,
                format!(
                    "no provider satisfies '{effective}' (available: {})",
                    list_visible_versions(visible_versions)
                ),
            ));
        };
        Ok(Some(ServiceRoute {
            provider: record.provider,
            generation: record.generation,
            iface: record.iface,
            version: record.version,
            methods: record.methods,
        }))
    }

    fn service_call(
        &self,
        provider: &str,
        generation: u32,
        iface: &str,
        version: &str,
        method: &str,
        args: &LuaValue,
    ) -> Result<LuaValue, BridgeError> {
        let directory = self.service_directory.borrow().clone();
        let Some(directory) = directory else {
            return Err(BridgeError::not_implemented("bitty.services.get"));
        };
        // Snapshot the record under a short borrow: the directory borrow
        // must never span the (potentially re-entrant) provider VM call.
        // The version pin keeps side-by-side records on their own handles
        // (dynapi negotiation, bitty#2006): a handle resolved at one
        // version never drives a sibling record's functions.
        let record = directory.borrow().find_active(iface, provider, version);
        let Some(record) = record else {
            return Err(service_gone_error(format!(
                "service '{iface}' is unavailable"
            )));
        };
        if record.generation != generation {
            return Err(service_gone_error(format!(
                "service '{iface}' handle is stale"
            )));
        }
        if !record.methods.iter().any(|name| name == method) {
            return Err(service_gone_error(format!(
                "service method '{iface}.{method}' is not published"
            )));
        }
        // Dynapi gating (bitty#2005): the caller's grant is re-checked on
        // every call, so a generation renewed after revocation fails closed
        // here with no restart beyond re-activation. A revoked or never-held
        // head shares the parked-provider code (`E_SERVICE_GONE`), so the
        // denial is indistinguishable from the provider being gone. The
        // argv-first invocation, deadlines, and per-call budgets below are
        // unchanged; only authority is gated, never execution shape.
        if !self.service_heads_covered(&record.heads) {
            return Err(service_gone_error(format!(
                "service '{iface}' is unavailable"
            )));
        }
        // Args cross as JSON and are validated before the provider runs, so
        // a schema violation never executes callee code.
        if let Some(schema) = &record.args_schema {
            let json = store::encode_json(args);
            if !value_satisfies_schema(schema, &json) {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_INVALID",
                    format!("service '{iface}.{method}' args do not satisfy the interface schema"),
                ));
            }
        }
        // Broker observability (dynapi 4/4 input): record the provider owner,
        // the negotiated version, plus the backing heads projected by this
        // call. Only dispatched calls record (rejected calls never reach a
        // provider). Observe-only: a missing or full ledger never blocks
        // the call.
        if let Some(ledger) = self.service_call_ledger.borrow().clone() {
            ledger.borrow_mut().push(ServiceCallRecord {
                provider: record.provider.clone(),
                iface: record.iface.clone(),
                version: record.version.clone(),
                method: method.to_string(),
                heads: record.heads.clone(),
            });
        }
        // CTX-1199: native sensor dispatch (bitty#2019/#2011/#2016). Records
        // published by the runtime-owned `bitty-terminal.sensors` component
        // carry no VM: `get()` runs host-side through the shared sensor
        // host (ceilings and `stale_age_ms` preserved, hide as nil). The
        // grant gate above already passed, so `granted` is true here; the
        // host re-checks it for defense in depth. Unwired hosts fail closed
        // with `E_SERVICE_GONE`, like a parked provider. No new error code,
        // no control method (read-only by construction).
        if super::system_sensors::host::SensorHost::is_sensor_provider(&record.provider) {
            let host = self.sensor_host.borrow().clone();
            let Some(host) = host else {
                return Err(service_gone_error(format!(
                    "service '{iface}' is unavailable"
                )));
            };
            let granted = self.service_heads_covered(&record.heads);
            return host.borrow_mut().call(&record.iface, method, granted);
        }
        // CTX-1238: native device dispatch (issue #2100). Records published
        // by the runtime-owned `bitty-terminal.devices` component carry no
        // VM: reads and controls run host-side through the shared device
        // host (ceilings and `stale_age_ms` preserved, hide as nil). The
        // grant gate above already passed; the host re-checks the full
        // grant snapshot per call (ESSID sub-grant isolation, control
        // heads). Unwired hosts fail closed with `E_SERVICE_GONE`, like a
        // parked provider. No new error code, no new ambient authority.
        if super::device_providers::host::DeviceHost::is_device_provider(&record.provider) {
            let host = self.device_host.borrow().clone();
            let Some(host) = host else {
                return Err(service_gone_error(format!(
                    "service '{iface}' is unavailable"
                )));
            };
            let granted_heads: Vec<String> =
                self.granted_capabilities.borrow().iter().cloned().collect();
            return host
                .borrow_mut()
                .call(&record.iface, method, args, &granted_heads);
        }
        // CTX-1238: native audio dispatch (issue #2100). Records published
        // by the runtime-owned `bitty-terminal.audio` component carry no
        // VM: `get`/`spectrum`/`set_volume`/`toggle_mute` run host-side
        // through the shared audio host (gates and `stale_age_ms`
        // preserved, hide as `E_SERVICE_GONE`). The grant gate above
        // already passed, so `granted` is true here; the host re-checks it
        // for defense in depth. Unwired hosts fail closed with
        // `E_SERVICE_GONE`, like a parked provider. No new error code, no
        // new ambient authority (`pactl` fixed argv only).
        if super::audio::is_audio_provider(&record.provider) {
            let host = self.audio_host.borrow().clone();
            let Some(host) = host else {
                return Err(service_gone_error(format!(
                    "service '{iface}' is unavailable"
                )));
            };
            let granted = self.service_heads_covered(&record.heads);
            return host.borrow_mut().call(&record.iface, method, args, granted);
        }
        let vm = record
            .vm
            .upgrade()
            .ok_or_else(|| service_gone_error(format!("service provider '{provider}' is gone")))?;
        let func = record.funcs.get(method).cloned().ok_or_else(|| {
            service_gone_error(format!(
                "service method '{iface}.{method}' is not published"
            ))
        })?;
        // `try_borrow_mut`: a re-entrant call into the already-executing
        // provider VM (including a service calling itself) fails closed
        // instead of panicking the RefCell.
        let result = match vm.try_borrow_mut() {
            Ok(mut vm) => vm.call_function(&func, std::slice::from_ref(args)),
            Err(_) => {
                return Err(BridgeError::new(
                    "runtime",
                    "E_SERVICE_FAILED",
                    format!("service provider '{provider}' is busy"),
                ));
            }
        };
        let result = result.map_err(|error| {
            BridgeError::new(
                "runtime",
                "E_SERVICE_FAILED",
                truncate_service_message(error.to_string()),
            )
        })?;
        // Results are observations, never live handles: function results
        // already fail closed at the provider marshalling boundary, and the
        // declared result schema is re-checked here before crossing back.
        if let Some(schema) = &record.result_schema {
            let json = store::encode_json(&result);
            if !value_satisfies_schema(schema, &json) {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_INVALID",
                    format!(
                        "service '{iface}.{method}' result does not satisfy the interface schema"
                    ),
                ));
            }
        }
        Ok(result)
    }

    fn command_list(&self) -> Result<Vec<CommandCatalogEntry>, BridgeError> {
        let directory = self.command_directory.borrow().clone();
        let Some(directory) = directory else {
            return Err(BridgeError::not_implemented("bitty.commands.list"));
        };
        // Snapshot under a short borrow: entries are plain metadata, and the
        // borrow must never span a caller callback.
        let records = directory.borrow().active_records();
        let entries: Vec<CommandCatalogEntry> = records
            .into_iter()
            .take(bitty_lua::COMMAND_LIST_MAX_ENTRIES)
            .map(|record| CommandCatalogEntry {
                plugin: record.owner,
                id: record.id,
                qualified: record.qualified,
                title: record.title,
            })
            .collect();
        Ok(entries)
    }

    fn command_invoke(&self, qualified: &str, args: &LuaValue) -> Result<LuaValue, BridgeError> {
        let directory = self.command_directory.borrow().clone();
        let Some(directory) = directory else {
            return Err(BridgeError::not_implemented("bitty.commands.invoke"));
        };
        // Snapshot the record under a short borrow: the directory borrow
        // must never span the (potentially re-entrant) callee VM call.
        // Undeclared and foreign-qualified names resolve to nothing and are
        // refused here, before any callee code could run (deny-by-default).
        let record = directory.borrow().find_active(qualified);
        let Some(record) = record else {
            if directory.borrow().knows(qualified) {
                return Err(command_gone_error(qualified));
            }
            return Err(command_unknown_error(qualified));
        };
        // Args cross as JSON and are validated before the callee runs, so
        // a schema violation never executes callee code. `nil` and `{}` both
        // validate as the empty object (see `command_args_document`).
        if let Some(schema) = &record.args_schema {
            let json = command_args_document(args);
            if !value_satisfies_schema(schema, &json) {
                return Err(command_invalid_error(
                    qualified,
                    "args do not satisfy the command schema",
                ));
            }
        }
        let vm = record
            .vm
            .upgrade()
            .ok_or_else(|| command_gone_error(qualified))?;
        let func = record.func.clone();
        // `try_borrow_mut`: a re-entrant call into the already-executing
        // owner VM (including a command invoking itself) fails closed
        // instead of panicking the RefCell.
        let result = match vm.try_borrow_mut() {
            Ok(mut vm) => vm.call_function(&func, std::slice::from_ref(args)),
            Err(_) => {
                return Err(command_failed_error(
                    qualified,
                    format!("command owner '{}' is busy", record.owner),
                ));
            }
        };
        let result = result.map_err(|error| {
            command_failed_error(qualified, truncate_service_message(error.to_string()))
        })?;
        // Results are observations, never live handles, and the effective
        // result schema is re-checked here before crossing back, so a
        // misbehaving callee is contained to this invocation.
        if let Some(schema) = &record.result_schema {
            let json = store::encode_json(&result);
            if !value_satisfies_schema(schema, &json) {
                return Err(command_invalid_error(
                    qualified,
                    "result does not satisfy the command schema",
                ));
            }
        }
        Ok(result)
    }

    fn panel_create(&self, panel_type: &str) -> Result<(u64, u64), BridgeError> {
        if !self.panel_create_granted.get() {
            return Err(BridgeError::capability_denied("panel.create"));
        }
        let backend = self
            .panel_create_backend
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.panel.create"))?;
        // Zero-PTY policy (CTX-1214, Issue #2085): plugins allocate native
        // panels only. `terminal` would mint PTY/process-spawn authority the
        // grant never conveys, so it fails closed here even though the Lua
        // bridge grammar still spells the full Core closed set. The check
        // runs after the backend-presence gate so an unwired host keeps
        // reporting `E_NOT_IMPLEMENTED` for every type, matching the sibling
        // verbs.
        match panel_type {
            "rich" | "browser" | "helper" | "canvas" => {}
            "terminal" => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "panel.create cannot create terminal panels: plugins create zero-PTY panels only",
                ));
            }
            _ => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "panel.create type must be rich, browser, helper, or canvas",
                ));
            }
        }
        backend(panel_type)
    }

    fn panel_close(&self, panel_id: u64) -> Result<bool, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let _ = panel_id;
        Err(BridgeError::not_implemented("bitty.panel.close"))
    }

    fn panel_destroy(&self, panel_id: u64) -> Result<bool, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let _ = panel_id;
        Err(BridgeError::not_implemented("bitty.panel.destroy"))
    }

    fn panel_get_presentation(&self, panel_id: u64) -> Result<Option<String>, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let _ = panel_id;
        Err(BridgeError::not_implemented("bitty.panel.get_presentation"))
    }

    fn panel_set_presentation(
        &self,
        panel_id: u64,
        presentation: &str,
    ) -> Result<bool, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let _ = (panel_id, presentation);
        Err(BridgeError::not_implemented("bitty.panel.set_presentation"))
    }

    fn panel_toggle_floating(&self, panel_id: u64) -> Result<bool, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let _ = panel_id;
        Err(BridgeError::not_implemented("bitty.panel.toggle_floating"))
    }

    fn panel_set_pinned(&self, panel_id: u64, pinned: bool) -> Result<bool, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let backend = self
            .panel_pin_backend
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.panel.set_pinned"))?;
        backend(panel_id, pinned)
    }

    /// Enqueue one ring-pulse intent for `bitty.fx.pulse` (CTX-1139,
    /// DEC-0110).
    ///
    /// Grant-gated on `panel.focus` (`E_CAPABILITY_DENIED` otherwise): the
    /// pulse targets panel chrome, the same authority family as
    /// `set_pinned`, so the slice adds no new capability. Returns whether
    /// the bounded queue accepted the intent; acceptance means queued, not
    /// applied — the application arms the Core-owned pulse on its next tick
    /// after revalidating the target against the live layout. A full queue
    /// returns `false`. Without a shared queue a granted call fails closed
    /// with `E_NOT_IMPLEMENTED`.
    fn fx_pulse(&self, view_id: u64) -> Result<bool, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let queue = self
            .fx_requests
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.fx.pulse"))?;
        Ok(queue.borrow_mut().push(QueuedFxPulse { view: view_id }))
    }

    /// Read one wallpaper image under the approved roots for
    /// `bitty.image.read` (CTX-1169, issue #1976).
    ///
    /// Grant-gated on `platform.image-file` (`E_CAPABILITY_DENIED`
    /// otherwise). Runs the accepted background trust chain — `~`/absolute
    /// expansion, approved-root policy, regular-file check, BG-1-bounded
    /// read, header sniff — and returns a bounded descriptor table (`path`,
    /// `format`, `width`, `height`, `size`). Raw bytes stay host-side: the
    /// bridge cannot carry binary intact. Roots, readability, and trust
    /// failures are `E_IMAGE_DENIED`; post-validation I/O failures are
    /// `E_IMAGE_UNREADABLE`; over-bound files are `E_DEF_LIMIT`. Without
    /// shared roots a granted read fails closed with `E_NOT_IMPLEMENTED`.
    fn image_read(&self, path: &str) -> Result<LuaValue, BridgeError> {
        if !self.image_read_granted.get() {
            return Err(BridgeError::capability_denied("platform.image-file"));
        }
        let roots = self
            .image_roots
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.image.read"))?;
        let policy = ResourcePolicy::with_max_roots(roots, BG_MAX_ROOTS).map_err(|err| {
            BridgeError::new(
                "trust",
                "E_IMAGE_DENIED",
                format!("image roots refused: {err}"),
            )
        })?;
        let expanded = expand_background_path(path, None).map_err(|err| {
            BridgeError::new(
                "trust",
                "E_IMAGE_DENIED",
                format!("image path denied: {err}"),
            )
        })?;
        let canonical = validate_resource_path(&expanded, &policy).map_err(|err| {
            BridgeError::new(
                "trust",
                "E_IMAGE_DENIED",
                format!("image path denied: {err}"),
            )
        })?;
        let bytes = read_image_bytes_bounded(&canonical)?;
        let (format, width, height) = match sniff_image(&bytes) {
            Ok(header) => {
                // Bounds without decode: a lying header (gigapixel dims) must
                // not reach Lua as budgeting metadata.
                check_header_bounds(&header).map_err(|err| {
                    BridgeError::new(
                        "budget",
                        "E_DEF_LIMIT",
                        format!("image header refused: {err}"),
                    )
                })?;
                (
                    background_format_name(header.format).to_string(),
                    LuaValue::Integer(i64::from(header.width)),
                    LuaValue::Integer(i64::from(header.height)),
                )
            }
            Err(_) => ("unknown".to_string(), LuaValue::Nil, LuaValue::Nil),
        };
        Ok(LuaValue::table([
            ("path", LuaValue::String(canonical.display().to_string())),
            ("format", LuaValue::String(format)),
            ("width", width),
            ("height", height),
            (
                "size",
                LuaValue::Integer(i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
            ),
        ]))
    }

    /// Preview one theme table for `bitty.theme.preview` (CTX-1169, issue
    /// #1976).
    ///
    /// Grant-gated on `runtime.configure` (`E_CAPABILITY_DENIED` otherwise):
    /// the table grammar is enforced by `CustomPalette::from_hex`
    /// (`E_DEF_INVALID` for a non-canonical leaf) before queueing, and the
    /// intent carries `persist = false`, so the drain live-applies without
    /// ever writing a file. Returns whether the bounded queue accepted the
    /// intent; a full queue returns `false`. Without a shared queue a granted
    /// call fails closed with `E_NOT_IMPLEMENTED`.
    fn theme_preview(&self, theme: &ThemeTable) -> Result<bool, BridgeError> {
        self.enqueue_theme(theme, false, "bitty.theme.preview")
    }

    /// Confirm one theme table for `bitty.theme.confirm` (CTX-1169, issue
    /// #1976).
    ///
    /// Same grant and grammar gates as [`Self::theme_preview`], but the
    /// intent carries `persist = true`: the drain durably commits through
    /// the persist backend first and live-applies only on success, so a
    /// confirm is never a silent rewrite. Returns whether the bounded queue
    /// accepted the intent; a full queue returns `false`.
    fn theme_confirm(&self, theme: &ThemeTable) -> Result<bool, BridgeError> {
        self.enqueue_theme(theme, true, "bitty.theme.confirm")
    }

    fn panel_get_state(&self, panel_id: u64) -> Result<Option<LuaValue>, BridgeError> {
        if !self.panel_focus_granted.get() {
            return Err(BridgeError::capability_denied("panel.focus"));
        }
        let _ = panel_id;
        Err(BridgeError::not_implemented("bitty.panel.get_state"))
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        if !self.workspace_read.get() {
            return Err(BridgeError::capability_denied("workspace.read"));
        }
        let source = self
            .workspace_source
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.workspace.list"))?;
        let mut rows = source.workspaces()?;
        rows.truncate(WORKSPACE_LIST_MAX_ITEMS);
        Ok(rows)
    }

    fn workspace_request(&self, request: &WorkspaceRequest) -> Result<bool, BridgeError> {
        if !self.workspace_control.get() {
            return Err(BridgeError::capability_denied("workspace.control"));
        }
        let queue = self
            .workspace_requests
            .borrow()
            .clone()
            .ok_or_else(|| BridgeError::not_implemented("bitty.workspace"))?;
        let accepted = queue.borrow_mut().push(QueuedWorkspaceRequest {
            plugin_id: self.plugin_id.clone(),
            request: request.clone(),
        });
        Ok(accepted)
    }

    fn debug_inspect(&self, target: &str) -> Result<LuaValue, BridgeError> {
        if !self.debug_inspect.get() {
            return Err(BridgeError::capability_denied("debug.inspect"));
        }
        match target {
            "grants" => {
                let items = self
                    .granted_capabilities
                    .borrow()
                    .iter()
                    .map(|capability| LuaValue::String(capability.clone()))
                    .collect();
                Ok(debug::inspect_result(target, items))
            }
            "plugins" | "commands" | "events" => {
                let view = self
                    .debug_view
                    .borrow()
                    .clone()
                    .ok_or_else(|| BridgeError::not_implemented("bitty.debug.inspect"))?;
                let value = view.borrow().inspect(target);
                value.ok_or_else(|| BridgeError::not_implemented("bitty.debug.inspect"))
            }
            // The panel registry lives in `registry::host::PanelRuntime`,
            // which the plugin runtime cannot reach.
            "panels" => Err(BridgeError::not_implemented("bitty.debug.inspect panels")),
            _ => Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                "debug.inspect target must be one of plugins, commands, events, grants, panels",
            )),
        }
    }

    fn runtime_inspect(&self) -> Result<RuntimeInspectSnapshot, BridgeError> {
        self.runtime_inspect_at(Instant::now())
    }

    fn keymaps_list(&self, query: &KeymapQuery) -> Result<KeymapPage, BridgeError> {
        // Grant and safe policy first: an ungranted generation (or any VM
        // under safe policy) must not learn whether a keymap source exists,
        // and no registry row may cross the boundary. The read head is
        // independent of `runtime.inspect` and `runtime.configure`.
        if self.safe_mode.get() || !self.keymaps_read.get() {
            return Err(BridgeError::capability_denied("runtime.keymaps.read"));
        }
        let started = Instant::now();
        if !self.keymap_budget.borrow_mut().allow(started) {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                "runtime.keymaps.read exceeded 16 calls per second",
            ));
        }
        // Clone the live table out of the source and release its borrow
        // before converting or marshalling: no plugin or bridge code runs
        // while the source is borrowed, and no row is cached across calls.
        let bindings = {
            let source = self
                .keymap_source
                .borrow()
                .clone()
                .ok_or_else(|| BridgeError::not_implemented("bitty.keymaps.list"))?;
            source.keymaps()?
        };
        Ok(KeymapPage::assemble(keymap_rows(&bindings), *query))
    }

    fn debug_trace(&self, opts: &LuaValue) -> Result<i64, BridgeError> {
        if !self.debug_trace.get() {
            return Err(BridgeError::capability_denied("debug.trace"));
        }
        let request = debug::parse_trace_opts(opts)?;
        let hub = self.trace_hub_or_unavailable()?;
        let mut hub = hub.borrow_mut();
        match request {
            TraceRequest::Start(spec) => {
                // Grants are fixed per generation and traces never outlive
                // it, so snapshotting them at open cannot go stale.
                let granted = self.granted_capabilities.borrow().iter().cloned().collect();
                hub.start(
                    &self.plugin_id,
                    self.declared_events.borrow().clone(),
                    granted,
                    spec,
                )
            }
            TraceRequest::Stop(handle) => {
                // Unknown and foreign handles are indistinguishable: both
                // are rejected with the same code and message.
                if hub.stop(&self.plugin_id, handle) {
                    Ok(handle)
                } else {
                    Err(BridgeError::new(
                        "validation",
                        "E_DEF_INVALID",
                        "debug.trace handle is not an open trace of this plugin",
                    ))
                }
            }
        }
    }

    fn debug_trace_with_expiry(
        &self,
        opts: &LuaValue,
        expiry: Instant,
    ) -> Result<i64, BridgeError> {
        if !self.debug_trace.get() {
            return Err(BridgeError::capability_denied("debug.trace"));
        }
        // Validation runs first so the expiry check sits immediately before
        // the commit (trace open/close) and an expired call never commits.
        let _ = debug::parse_trace_opts(opts)?;
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.debug_trace(opts)
    }

    fn debug_trace_get(&self, handle: i64) -> Result<LuaValue, BridgeError> {
        if !self.debug_trace.get() {
            return Err(BridgeError::capability_denied("debug.trace"));
        }
        let hub = self.trace_hub_or_unavailable()?;
        let drained = hub.borrow_mut().drain(&self.plugin_id, handle);
        Ok(drained.map_or(LuaValue::Nil, |drain| drain.to_value()))
    }

    fn debug_control(&self, action: &str, target: &str) -> Result<LuaValue, BridgeError> {
        // High-risk lifecycle controls stay unimplemented (separate task).
        // Fail closed before reading anything so the call leaks neither the
        // grant state nor whether `target` exists.
        let _ = (action, target);
        Err(BridgeError::not_implemented("bitty.debug.control"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::store::PluginStore;
    use bitty_lua::E_UI_ALREADY_CAPTURED;
    use bitty_lua::ENV_KEY_MAX_BYTES;
    use bitty_lua::FS_LIST_MAX_ENTRIES;
    use bitty_lua::ui::{
        ClickArg, ClickCommand, UI_CLICK_MAX_ARGS, UI_MAX_NODES, UI_MAX_TEXT_BYTES,
    };
    use std::time::Duration;

    fn services() -> PluginServices {
        PluginServices::new(
            "xuepoo.test",
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(UnavailableSnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        )
    }

    fn debug_services(inspect: bool, trace: bool) -> PluginServices {
        let services = services();
        services.set_debug_inspect(inspect);
        services.set_debug_trace(trace);
        services.set_debug_view(Rc::new(RefCell::new(DebugView::new())));
        services.set_trace_hub(Rc::new(RefCell::new(TraceHub::new())));
        services.set_declared_events(["terminal.opened".to_string()].into());
        services
    }

    #[test]
    fn panel_entry_points_deny_without_grant() {
        let services = services();
        let error = services.panel_create("test").expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(error.message, "capability 'panel.create' is not granted");

        for (name, res) in [
            ("close", services.panel_close(1)),
            ("destroy", services.panel_destroy(1)),
            (
                "get_presentation",
                services.panel_get_presentation(1).map(|_| false),
            ),
            (
                "set_presentation",
                services.panel_set_presentation(1, "tab"),
            ),
            ("toggle_floating", services.panel_toggle_floating(1)),
            ("set_pinned", services.panel_set_pinned(1, true)),
            ("get_state", services.panel_get_state(1).map(|_| false)),
        ] {
            let error = res.expect_err("denied");
            assert_eq!(error.code, "E_CAPABILITY_DENIED", "{name}");
            assert_eq!(
                error.message, "capability 'panel.focus' is not granted",
                "{name}"
            );
        }
    }

    #[test]
    fn panel_grants_do_not_imply_each_other() {
        let create_only = services();
        create_only.set_panel_access(true, false);
        let error = create_only.panel_create("test").expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = create_only.panel_close(1).expect_err("focus denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");

        let focus_only = services();
        focus_only.set_panel_access(false, true);
        let error = focus_only.panel_create("test").expect_err("create denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        let error = focus_only.panel_close(1).expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = focus_only.panel_destroy(1).expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = focus_only
            .panel_get_presentation(1)
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = focus_only
            .panel_set_presentation(1, "tab")
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = focus_only
            .panel_toggle_floating(1)
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = focus_only
            .panel_set_pinned(1, true)
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = focus_only.panel_get_state(1).expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn panel_create_applies_through_backend_zero_pty_only() {
        // CTX-1214 (Issue #2085): a granted generation with a backend routes
        // native types through; `terminal` (PTY/process authority) and
        // unknown types fail closed with `E_DEF_INVALID` and never reach the
        // backend.
        let granted = services();
        granted.set_panel_access(true, false);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let tapped = seen.clone();
        granted.set_panel_create_backend(Some(Rc::new(move |panel_type: &str| {
            tapped.borrow_mut().push(panel_type.to_string());
            Ok((7, 3))
        })));
        assert_eq!(granted.panel_create("rich").expect("rich creates"), (7, 3));
        assert_eq!(
            granted.panel_create("helper").expect("helper creates"),
            (7, 3)
        );
        let error = granted.panel_create("terminal").expect_err("no PTY");
        assert_eq!(error.code, "E_DEF_INVALID");
        let error = granted.panel_create("bogus").expect_err("closed set");
        assert_eq!(error.code, "E_DEF_INVALID");
        assert_eq!(
            *seen.borrow(),
            vec!["rich".to_string(), "helper".to_string()],
            "rejected types never reach the backend"
        );
    }

    #[test]
    fn panel_create_denies_before_backend_without_grant() {
        // Grant gate first: even with a backend installed, an ungranted
        // generation fails closed with `E_CAPABILITY_DENIED` and the backend
        // never runs.
        let denied = services();
        let called = Rc::new(RefCell::new(false));
        let flag = called.clone();
        denied.set_panel_create_backend(Some(Rc::new(move |_: &str| {
            *flag.borrow_mut() = true;
            Ok((1, 1))
        })));
        let error = denied.panel_create("rich").expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(error.message, "capability 'panel.create' is not granted");
        assert!(!*called.borrow(), "denied calls never reach the backend");
    }

    fn submit_services() -> PluginServices {
        // CTX-1239: granted generation with a shared queue wired, the
        // activation shape for `terminal.input.submit` holders.
        let services = services();
        services.set_terminal_submit_access(true);
        services.set_submit_queue(Some(Rc::new(RefCell::new(TerminalSubmitQueue::new(8)))));
        services
    }

    fn outcome_string(outcome: &LuaValue, key: &str) -> Option<String> {
        match outcome.get(key)? {
            LuaValue::String(text) => Some(text.clone()),
            LuaValue::Integer(value) => Some(value.to_string()),
            LuaValue::Bool(value) => Some(value.to_string()),
            _ => None,
        }
    }

    #[test]
    fn terminal_submit_absent_without_grant_or_queue() {
        // CTX-1239 (issue #2101): presence requires BOTH the grant and a
        // wired queue. Either missing observes nil at the bridge, so the
        // composer nil path (`unavailable:no-submit`) cannot distinguish
        // ungranted from unwired from absent.
        let bare = services();
        assert!(!bare.has_terminal_submit(), "bare generation probes absent");
        let granted_only = services();
        granted_only.set_terminal_submit_access(true);
        assert!(
            !granted_only.has_terminal_submit(),
            "grant without a queue still probes absent"
        );
        let queued_only = services();
        queued_only.set_submit_queue(Some(Rc::new(RefCell::new(TerminalSubmitQueue::new(8)))));
        assert!(
            !queued_only.has_terminal_submit(),
            "queue without a grant still probes absent"
        );
        assert!(
            submit_services().has_terminal_submit(),
            "grant plus queue probes present"
        );
    }

    #[test]
    fn terminal_submit_denies_before_queue_without_grant() {
        // Grant gate first: even with a queue wired, an ungranted generation
        // fails closed with `E_CAPABILITY_DENIED` and nothing queues.
        let denied = services();
        denied.set_submit_queue(Some(Rc::new(RefCell::new(TerminalSubmitQueue::new(8)))));
        let error = denied.terminal_submit("cargo test").expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(
            error.message,
            "capability 'terminal.input.submit' is not granted"
        );
    }

    #[test]
    fn terminal_submit_granted_without_queue_is_not_implemented() {
        // A granted call without a shared queue fails closed with
        // `E_NOT_IMPLEMENTED`, like every other verb without a host.
        let granted = services();
        granted.set_terminal_submit_access(true);
        let error = granted
            .terminal_submit("cargo test")
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn terminal_submit_accepts_byte_exact_frame_and_charges() {
        // CTX-1239: a granted submit frames byte-exact (the same
        // bracketed-paste bytes the native operation writes), queues one
        // intent, charges the generation window, and reports accepted.
        let granted = submit_services();
        let outcome = granted.terminal_submit("cargo test").expect("accepted");
        assert_eq!(
            outcome_string(&outcome, "status").as_deref(),
            Some("accepted")
        );
        assert_eq!(outcome_string(&outcome, "bytes").as_deref(), Some("23"));
        assert_eq!(
            granted.submit_budget.borrow().used(),
            23,
            "accepted submit charges the window"
        );
        let queue = granted.submit_queue.borrow().clone().expect("queue");
        let drained = queue.borrow_mut().drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].frame,
            b"\x1b[200~cargo test\x1b[201~\r".as_slice(),
            "queued frame must be the byte-exact paste frame"
        );
    }

    #[test]
    fn terminal_submit_over_cap_denies_without_charge() {
        // Over-cap text is a denied table (buffer kept, session stays open
        // by composer contract), never an error, and charges nothing.
        let granted = submit_services();
        let big = "q".repeat(bitty_rich::host::COMPOSER_MAX_BYTES + 1);
        let outcome = granted.terminal_submit(&big).expect("denied table");
        assert_eq!(
            outcome_string(&outcome, "status").as_deref(),
            Some("denied")
        );
        assert_eq!(
            outcome_string(&outcome, "deny").as_deref(),
            Some("too-large")
        );
        assert_eq!(granted.submit_budget.borrow().used(), 0);
        let queue = granted.submit_queue.borrow().clone().expect("queue");
        assert!(queue.borrow().is_empty(), "denials queue nothing");
    }

    #[test]
    fn terminal_submit_exhausted_budget_denies_without_charge() {
        // A generation whose window cannot cover the frame is denied with
        // the prior usage untouched. The shared queue drains each round so
        // saturation never masks the budget denial.
        let granted = submit_services();
        let queue = granted.submit_queue.borrow().clone().expect("queue");
        let filler = "x".repeat(4096);
        loop {
            let outcome = granted.terminal_submit(&filler).expect("table");
            if outcome_string(&outcome, "status").as_deref() == Some("denied") {
                assert_eq!(
                    outcome_string(&outcome, "deny").as_deref(),
                    Some("budget-exceeded")
                );
                break;
            }
            queue.borrow_mut().drain();
        }
        let used = granted.submit_budget.borrow().used();
        assert!(
            used <= TERMINAL_SUBMIT_WINDOW_BYTES,
            "charged bytes never exceed the window"
        );
        assert!(queue.borrow().is_empty(), "denials queue nothing");
    }

    #[test]
    fn terminal_submit_full_queue_denies_without_charge() {
        // A saturated shared queue drops the newest intent, counts it, and
        // reports a denied table with the budget untouched (retry later).
        let granted = services();
        granted.set_terminal_submit_access(true);
        granted.set_submit_queue(Some(Rc::new(RefCell::new(TerminalSubmitQueue::new(1)))));
        granted.terminal_submit("first").expect("first queues");
        let used = granted.submit_budget.borrow().used();
        let outcome = granted.terminal_submit("second").expect("denied table");
        assert_eq!(
            outcome_string(&outcome, "status").as_deref(),
            Some("denied")
        );
        assert_eq!(
            outcome_string(&outcome, "deny").as_deref(),
            Some("queue-full")
        );
        assert_eq!(
            granted.submit_budget.borrow().used(),
            used,
            "dropped intents charge nothing"
        );
        let queue = granted.submit_queue.borrow().clone().expect("queue");
        assert_eq!(queue.borrow().dropped(), 1);
        assert_eq!(queue.borrow().len(), 1);
    }

    #[test]
    fn fx_pulse_denies_without_grant() {
        // CTX-1139: `bitty.fx.pulse` is deny-by-default on `panel.focus`,
        // naming the capability exactly like the sibling panel verbs.
        let denied = services();
        let error = denied.fx_pulse(1).expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(error.message, "capability 'panel.focus' is not granted");
        // Create-only does not imply the fx verb either.
        let create_only = services();
        create_only.set_panel_access(true, false);
        let error = create_only.fx_pulse(1).expect_err("focus denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
    }

    #[test]
    fn fx_pulse_granted_without_queue_is_not_implemented() {
        // CTX-1139: a granted call without a shared queue fails closed with
        // `E_NOT_IMPLEMENTED`, like every other verb without a host.
        let granted = services();
        granted.set_panel_access(false, true);
        let error = granted.fx_pulse(1).expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn fx_pulse_enqueues_bounded_intents_fifo() {
        // CTX-1139: granted calls enqueue raw view ids; overflow drops the
        // newest, counts it, and reports `false`. The drain validates
        // targets against live state, so enqueue carries no validation.
        use crate::runtime::fx::FX_PULSE_QUEUE_CAPACITY;
        let granted = services();
        granted.set_panel_access(false, true);
        granted.set_fx_queue(Some(Rc::new(RefCell::new(FxPulseQueue::new(2)))));
        assert!(granted.fx_pulse(1).expect("first intent queues"));
        assert!(granted.fx_pulse(404).expect("unknown id still queues"));
        assert!(
            !granted.fx_pulse(2).expect("overflow reports false"),
            "a full queue refuses the newest intent"
        );
        let queue = granted.fx_requests.borrow().clone().expect("queue");
        assert_eq!(queue.borrow().dropped(), 1);
        assert_eq!(
            queue.borrow_mut().drain(),
            vec![QueuedFxPulse { view: 1 }, QueuedFxPulse { view: 404 }]
        );
        assert_eq!(FX_PULSE_QUEUE_CAPACITY, 16);
    }

    fn theme_table() -> ThemeTable {
        ThemeTable {
            background: "#1e1e2e".to_string(),
            foreground: "#cdd6f4".to_string(),
            cursor: "#f5e0dc".to_string(),
            selection: "#313244".to_string(),
            ansi: vec!["#45475a".to_string(); 16],
        }
    }

    fn image_test_root(tag: &str) -> PathBuf {
        // Per-test unique roots: parallel tests share the process id, so the
        // tag alone cannot isolate them; the counter closes the gap while the
        // pre-remove keeps reruns hermetic. Virtual-clock only: no sleeps,
        // no wall waits.
        use std::sync::atomic::{AtomicU64, Ordering};
        static IMAGE_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = IMAGE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1169-image-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create image test root");
        dir
    }

    /// Minimal PNG carrying only signature, IHDR, and IEND (CTX-1169).
    ///
    /// The image hook sniffs headers without decoding, so no IDAT is needed;
    /// the sniff ignores CRC bytes, which stay zero here.
    fn tiny_png(width: u32, height: u32) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut chunk = |ctype: &[u8; 4], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(ctype);
            out.extend_from_slice(data);
            out.extend_from_slice(&[0u8; 4]);
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IEND", &[]);
        out
    }

    fn image_services_with_roots(roots: Vec<PathBuf>) -> PluginServices {
        let granted = services();
        granted.set_image_access(true);
        granted.set_image_roots(Some(roots));
        granted
    }

    fn descriptor_field(value: &LuaValue, key: &str) -> LuaValue {
        value
            .get(key)
            .unwrap_or_else(|| panic!("descriptor carries '{key}'"))
            .clone()
    }

    #[test]
    fn image_read_denies_without_grant() {
        // CTX-1169: `bitty.image.read` is deny-by-default on
        // `platform.image-file`, naming the capability exactly.
        let denied = services();
        let error = denied.image_read("/wall/paper.png").expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(
            error.message,
            "capability 'platform.image-file' is not granted"
        );
    }

    #[test]
    fn image_read_granted_without_roots_is_not_implemented() {
        // CTX-1169: a granted read without shared roots fails closed with
        // `E_NOT_IMPLEMENTED`, like every other verb without a host.
        let granted = services();
        granted.set_image_access(true);
        let error = granted
            .image_read("/wall/paper.png")
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn image_read_serves_a_valid_png_descriptor() {
        // CTX-1169: an approved-root PNG reads bounded and reports its
        // canonical path, sniffed format, header dims, and byte size. Raw
        // bytes stay host-side by design (the bridge cannot carry binary).
        let root = image_test_root("valid");
        let file = root.join("paper.png");
        let bytes = tiny_png(3, 2);
        std::fs::write(&file, &bytes).expect("write png");
        let granted = image_services_with_roots(vec![root.clone()]);
        let value = granted
            .image_read(file.to_str().expect("utf-8 path"))
            .expect("approved png reads");
        assert_eq!(
            descriptor_field(&value, "format"),
            LuaValue::String("png".to_string())
        );
        assert_eq!(descriptor_field(&value, "width"), LuaValue::Integer(3));
        assert_eq!(descriptor_field(&value, "height"), LuaValue::Integer(2));
        assert_eq!(
            descriptor_field(&value, "size"),
            LuaValue::Integer(bytes.len() as i64)
        );
        let canonical = file
            .canonicalize()
            .expect("canonicalize")
            .display()
            .to_string();
        assert_eq!(
            descriptor_field(&value, "path"),
            LuaValue::String(canonical)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn image_read_rejects_outside_roots_and_missing_files() {
        // CTX-1169: outside-root paths and absent files both fail closed
        // without touching state (missing files surface at the trust layer,
        // so absence is indistinguishable from denial).
        let approved = image_test_root("approved");
        let outside = image_test_root("outside");
        let secret = outside.join("secret.png");
        std::fs::write(&secret, tiny_png(1, 1)).expect("write outside");
        let granted = image_services_with_roots(vec![approved.clone()]);
        let error = granted
            .image_read(secret.to_str().expect("utf-8 path"))
            .expect_err("outside denied");
        assert_eq!(error.code, "E_IMAGE_DENIED");
        let missing = approved.join("gone.png");
        let error = granted
            .image_read(missing.to_str().expect("utf-8 path"))
            .expect_err("missing denied");
        assert_eq!(error.code, "E_IMAGE_DENIED");
        let _ = std::fs::remove_dir_all(&approved);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn image_read_rejects_oversize_files() {
        // CTX-1169: files over BG-1 fail closed with `E_DEF_LIMIT` on the
        // actual bytes, so a sparse or swapped length cannot smuggle a larger
        // allocation through.
        let root = image_test_root("oversize");
        let file = root.join("huge.png");
        let big = vec![0u8; BG_MAX_ENCODED_BYTES + 1];
        std::fs::write(&file, &big).expect("write oversize");
        let granted = image_services_with_roots(vec![root.clone()]);
        let error = granted
            .image_read(file.to_str().expect("utf-8 path"))
            .expect_err("oversize denied");
        assert_eq!(error.code, "E_DEF_LIMIT");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn image_read_rejects_symlink_escape() {
        // CTX-1169 (unix-gated: symlink live test): a link inside the
        // approved root pointing outside fails closed after canonicalization.
        let approved = image_test_root("link-approved");
        let outside = image_test_root("link-outside");
        let secret = outside.join("secret.png");
        std::fs::write(&secret, tiny_png(1, 1)).expect("write outside");
        let link = approved.join("escape.png");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");
        let granted = image_services_with_roots(vec![approved.clone()]);
        let error = granted
            .image_read(link.to_str().expect("utf-8 path"))
            .expect_err("escape denied");
        assert_eq!(error.code, "E_IMAGE_DENIED");
        let _ = std::fs::remove_dir_all(&approved);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn theme_verbs_deny_without_grant() {
        // CTX-1169: preview and confirm share the `runtime.configure` grant
        // and name it exactly when absent.
        let denied = services();
        let error = denied
            .theme_preview(&theme_table())
            .expect_err("preview denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(
            error.message,
            "capability 'runtime.configure' is not granted"
        );
        let error = denied
            .theme_confirm(&theme_table())
            .expect_err("confirm denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(
            error.message,
            "capability 'runtime.configure' is not granted"
        );
    }

    #[test]
    fn theme_verbs_granted_without_queue_are_not_implemented() {
        // CTX-1169: granted calls without a shared queue fail closed with
        // `E_NOT_IMPLEMENTED`, like every other verb without a host.
        let granted = services();
        granted.set_theme_access(true);
        let error = granted
            .theme_preview(&theme_table())
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = granted
            .theme_confirm(&theme_table())
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn theme_verbs_reject_malformed_palettes_without_queueing() {
        // CTX-1169: the `CustomPalette` grammar runs before queueing, so a
        // malformed leaf fails closed with `E_DEF_INVALID` and lodges
        // nothing for the drain.
        let granted = services();
        granted.set_theme_access(true);
        let queue = Rc::new(RefCell::new(ThemeRequestQueue::new(8)));
        granted.set_theme_queue(Some(Rc::clone(&queue)));
        let mut bad = theme_table();
        bad.foreground = "not-a-color".to_string();
        let error = granted.theme_preview(&bad).expect_err("preview refused");
        assert_eq!(error.code, "E_DEF_INVALID");
        let error = granted.theme_confirm(&bad).expect_err("confirm refused");
        assert_eq!(error.code, "E_DEF_INVALID");
        assert!(queue.borrow().is_empty(), "malformed palettes never queue");
    }

    #[test]
    fn theme_preview_and_confirm_enqueue_with_persist_flags_fifo() {
        // CTX-1169: preview enqueues `persist = false` and confirm
        // `persist = true` in FIFO order; overflow drops the newest, counts
        // it, and reports `false`. The drain (not enqueue) owns durability,
        // so a preview intent can never persist.
        let granted = services();
        granted.set_theme_access(true);
        granted.set_theme_queue(Some(Rc::new(RefCell::new(ThemeRequestQueue::new(2)))));
        assert!(
            granted
                .theme_preview(&theme_table())
                .expect("preview queues")
        );
        assert!(
            granted
                .theme_confirm(&theme_table())
                .expect("confirm queues")
        );
        assert!(
            !granted
                .theme_preview(&theme_table())
                .expect("overflow reports false"),
            "a full queue refuses the newest intent"
        );
        let queue = granted.theme_requests.borrow().clone().expect("queue");
        assert_eq!(queue.borrow().dropped(), 1);
        let drained = queue.borrow_mut().drain();
        assert_eq!(drained.len(), 2);
        assert!(!drained[0].persist, "preview never persists");
        assert!(drained[1].persist, "confirm persists");
        assert_eq!(drained[0].palette, drained[1].palette);
    }

    #[test]
    fn panel_set_pinned_routes_through_the_runtime_pin_path() {
        // CTX-1083: the `bitty.panel.set_pinned` service seam delegates to
        // the same `pin_floating`/`unpin_floating` runtime path the
        // `toggle_pinned` keymap action uses. The backend here closes over a
        // live runtime; production wires its own host backend while the seam
        // (grant gate plus fail-closed booleans) stays identical.
        use crate::{LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};
        let runtime = Rc::new(RefCell::new(
            Runtime::with_defaults().expect("defaults must build"),
        ));
        {
            let mut rt = runtime.borrow_mut();
            rt.set_layout(LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            ));
            rt.set_container(UiRect::new(0, 0, 80, 24));
        }
        let backend: PanelPinHandler = Rc::new(move |panel_id, pinned| {
            let mut rt = runtime.borrow_mut();
            let id = ViewId::new(panel_id);
            if pinned {
                if rt.pinned_views().contains(&id) {
                    return Ok(true);
                }
                match rt.pin_floating(id) {
                    Ok(()) => Ok(true),
                    Err(_) => Ok(false),
                }
            } else {
                if !rt.pinned_views().contains(&id) && rt.layout().leaf_ids().contains(&id) {
                    return Ok(true);
                }
                match rt.unpin_floating(id) {
                    Ok(_) => Ok(true),
                    Err(_) => Ok(false),
                }
            }
        });
        let granted = services();
        granted.set_panel_access(false, true);
        granted.set_panel_pin_backend(Some(backend));

        assert!(
            granted.panel_set_pinned(1, true).expect("pin applies"),
            "pinning a live leaf holds"
        );
        assert!(
            granted.panel_set_pinned(1, true).expect("idempotent"),
            "re-pinning an already pinned leaf still holds"
        );
        assert!(
            granted.panel_set_pinned(1, false).expect("unpin applies"),
            "unpinning returns the floating panel"
        );
        assert!(
            granted.panel_set_pinned(1, false).expect("idempotent"),
            "unpinning a live leaf still holds"
        );
        // Fail-closed on an unknown target: no panic, state untouched,
        // clear false result.
        assert!(
            !granted
                .panel_set_pinned(404, true)
                .expect("unknown pin fails closed"),
            "unknown id pins nothing"
        );
        assert!(
            !granted
                .panel_set_pinned(404, false)
                .expect("unknown unpin fails closed"),
            "unknown id unpins nothing"
        );
    }

    #[test]
    fn panel_pin_queue_drains_fifo_and_drops_newest_on_overflow() {
        // CTX-1115: same governance as the workspace request queue — FIFO
        // drain for the tick loop, newest-dropped plus counted on overflow.
        let mut queue = PanelPinQueue::new(2);
        assert!(queue.is_empty());
        assert!(queue.push(QueuedPinRequest {
            panel_id: 1,
            pinned: true,
        }));
        assert!(queue.push(QueuedPinRequest {
            panel_id: 1,
            pinned: false,
        }));
        assert_eq!(queue.len(), 2);
        assert!(!queue.push(QueuedPinRequest {
            panel_id: 2,
            pinned: true,
        }));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(
            queue.drain(),
            vec![
                QueuedPinRequest {
                    panel_id: 1,
                    pinned: true,
                },
                QueuedPinRequest {
                    panel_id: 1,
                    pinned: false,
                },
            ]
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn workspace_read_lists_scratchpad_without_panel_grants() {
        // CTX-0954 deny-proof: scratchpad occupancy rides `workspace.read`
        // alone; no panel grant is consulted in either direction.
        struct OccupiedWorkspaces;
        impl WorkspaceSource for OccupiedWorkspaces {
            fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
                Ok(vec![WorkspaceInfo {
                    id: 1,
                    name: String::from("ws1"),
                    active: true,
                    panel_count: 2,
                    scratchpad_count: 1,
                    scratchpad_occupied: true,
                    attention: bitty_lua::WorkspaceAttention::default(),
                }])
            }
        }

        let granted = services();
        granted.set_workspace_access(true, false);
        granted.set_workspace_backend(Some(Rc::new(OccupiedWorkspaces)), None);
        // No panel grants held: the read still serves occupancy.
        let rows = granted.workspace_list().expect("read lists occupancy");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scratchpad_count, 1);
        assert!(rows[0].scratchpad_occupied);
        // Panel grants change nothing about the read path.
        granted.set_panel_access(true, true);
        let rows = granted.workspace_list().expect("read still lists");
        assert!(rows[0].scratchpad_occupied);
        // Without workspace.read the list fails closed even when every panel
        // grant is held.
        let denied = services();
        denied.set_panel_access(true, true);
        denied.set_workspace_backend(Some(Rc::new(OccupiedWorkspaces)), None);
        let error = denied.workspace_list().expect_err("needs workspace.read");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(error.message, "capability 'workspace.read' is not granted");
    }

    #[test]
    fn debug_entry_points_deny_without_grant() {
        let services = debug_services(false, false);
        for target in ["plugins", "grants", "panels", "bogus"] {
            let error = services.debug_inspect(target).expect_err("denied");
            assert_eq!(error.code, "E_CAPABILITY_DENIED", "{target}");
        }
        let error = services.debug_trace(&LuaValue::Nil).expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        let error = services
            .debug_trace_with_expiry(&LuaValue::Nil, Instant::now() + Duration::from_secs(5))
            .expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        let error = services.debug_trace_get(1).expect_err("denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
    }

    #[test]
    fn debug_grants_do_not_imply_each_other() {
        let inspect_only = debug_services(true, false);
        assert!(inspect_only.debug_inspect("grants").is_ok());
        assert_eq!(
            inspect_only
                .debug_trace(&LuaValue::Nil)
                .expect_err("trace needs its own grant")
                .code,
            "E_CAPABILITY_DENIED"
        );
        let trace_only = debug_services(false, true);
        assert!(trace_only.debug_trace(&LuaValue::Nil).is_ok());
        assert_eq!(
            trace_only
                .debug_inspect("plugins")
                .expect_err("inspect needs its own grant")
                .code,
            "E_CAPABILITY_DENIED"
        );
    }

    #[test]
    fn debug_inspect_grants_returns_only_own_sorted_grants() {
        let services = debug_services(true, false);
        services.set_granted_capabilities(vec![
            "platform.notify".to_string(),
            "debug.inspect".to_string(),
            "platform.notify".to_string(),
        ]);
        let other = debug_services(true, false);
        other.set_granted_capabilities(vec!["clipboard.read".to_string()]);
        let value = services.debug_inspect("grants").expect("grants");
        assert_eq!(
            value.get("target"),
            Some(&LuaValue::String("grants".into()))
        );
        assert_eq!(value.get("truncated"), Some(&LuaValue::Bool(false)));
        assert_eq!(
            value.get("items"),
            Some(&LuaValue::array(vec![
                LuaValue::String("debug.inspect".to_string()),
                LuaValue::String("platform.notify".to_string()),
            ]))
        );
        assert!(!store::encode_json(&value).contains("clipboard.read"));
    }

    #[test]
    fn settings_get_serves_only_declared_keys() {
        // CTX-1195: the generation reads exactly its manifest `[settings]`
        // set; anything else fails closed, even when the shared source holds
        // a value for it.
        let services = PluginServices::new(
            "xuepoo.gated",
            PluginStore::in_memory(),
            Rc::new(MapSettings::new(BTreeMap::from([
                ("show".to_string(), LuaValue::Bool(false)),
                ("other".to_string(), LuaValue::Bool(true)),
            ]))),
            Rc::new(UnavailableSnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        );
        // No declarations yet: deny-by-default reads nothing.
        let error = services.settings_get("show").expect_err("undeclared");
        assert_eq!(error.code, "E_SETTINGS_UNDECLARED");
        services.set_declared_settings(BTreeSet::from(["show".to_string()]));
        assert_eq!(
            services.settings_get("show").expect("declared"),
            Some(LuaValue::Bool(false))
        );
        // Declared but absent serves None (the merge guarantees a default in
        // production; the gate only polices the key set).
        services.set_declared_settings(BTreeSet::from(["show".to_string(), "missing".to_string()]));
        assert_eq!(services.settings_get("missing").expect("declared"), None);
        // Present in the source but undeclared: fail closed.
        let error = services.settings_get("other").expect_err("undeclared");
        assert_eq!(error.code, "E_SETTINGS_UNDECLARED");
        // Key-shape errors keep their own code, before the gate.
        let error = services.settings_get("").expect_err("empty");
        assert_eq!(error.code, "E_SETTINGS_KEY_INVALID");
    }

    #[test]
    fn settings_cross_plugin_read_fails_closed() {
        // CTX-1195 negative test: two generations with disjoint declarations
        // and disjoint sources. Neither can observe the other's keys — the
        // gate denies (the sources do not even hold the foreign keys).
        fn generation(
            id: &str,
            declared: &[&str],
            values: Vec<(&str, LuaValue)>,
        ) -> PluginServices {
            let services = PluginServices::new(
                id,
                PluginStore::in_memory(),
                Rc::new(MapSettings::new(
                    values
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                )),
                Rc::new(UnavailableSnapshot),
                Rc::new(RefCell::new(NotificationQueue::new(8))),
                false,
                false,
            );
            services.set_declared_settings(declared.iter().map(|key| key.to_string()).collect());
            services
        }
        let bar = generation(
            "bitty-terminal.bar",
            &["show", "modules"],
            vec![
                ("show", LuaValue::Bool(false)),
                (
                    "modules",
                    LuaValue::array(vec![LuaValue::String("clock".to_string())]),
                ),
            ],
        );
        let other = generation(
            "xuepoo.other",
            &["theme"],
            vec![("theme", LuaValue::String("dark".to_string()))],
        );
        // Own keys serve.
        assert_eq!(
            bar.settings_get("show").expect("own"),
            Some(LuaValue::Bool(false))
        );
        assert_eq!(
            other.settings_get("theme").expect("own"),
            Some(LuaValue::String("dark".to_string()))
        );
        // Foreign keys fail closed in both directions.
        for key in ["theme", "grants", "capabilities"] {
            assert_eq!(
                bar.settings_get(key).expect_err("foreign").code,
                "E_SETTINGS_UNDECLARED",
                "bar must not read '{key}'"
            );
        }
        for key in ["show", "modules", "grants", "capabilities"] {
            assert_eq!(
                other.settings_get(key).expect_err("foreign").code,
                "E_SETTINGS_UNDECLARED",
                "other must not read '{key}'"
            );
        }
    }

    #[test]
    fn debug_inspect_rejects_unknown_and_reserves_panels() {
        let services = debug_services(true, false);
        let error = services.debug_inspect("settings").expect_err("unknown");
        assert_eq!(error.code, "E_DEF_INVALID");
        let error = services.debug_inspect("panels").expect_err("reserved");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let value = services.debug_inspect("plugins").expect("empty view");
        assert_eq!(value.get("items"), Some(&LuaValue::array(Vec::new())));
    }

    #[test]
    fn debug_trace_expired_call_never_opens_a_trace() {
        let services = debug_services(false, true);
        // An explicit instant in the past: no sleep needed for the deadline
        // to have passed.
        let past = Instant::now() - Duration::from_millis(1);
        let error = services
            .debug_trace_with_expiry(&LuaValue::Nil, past)
            .expect_err("expired");
        assert_eq!(error.code, "E_TIMEOUT");
        let hub = services.trace_hub.borrow().clone().expect("hub");
        assert_eq!(hub.borrow().trace_count("xuepoo.test"), 0);
        // Validation still wins over the deadline (no misleading timeout).
        let error = services
            .debug_trace_with_expiry(&LuaValue::Integer(1), past)
            .expect_err("invalid");
        assert_eq!(error.code, "E_DEF_INVALID");
    }

    #[test]
    fn debug_trace_open_drain_and_close() {
        let services = debug_services(false, true);
        let handle = services.debug_trace(&LuaValue::Nil).expect("open");
        assert_eq!(handle, 1);
        let hub = services.trace_hub.borrow().clone().expect("hub");
        hub.borrow_mut()
            .record("terminal.opened", 7, &LuaValue::Integer(1), |_| true);
        // Not in the owner's declared `lazy.events`: never recorded.
        hub.borrow_mut()
            .record("focus.changed", 8, &LuaValue::Integer(2), |_| true);
        let drained = services.debug_trace_get(handle).expect("drain");
        assert_eq!(drained.get("dropped"), Some(&LuaValue::Integer(0)));
        let encoded = store::encode_json(&drained);
        assert!(encoded.contains("terminal.opened"), "{encoded}");
        assert!(!encoded.contains("focus.changed"), "{encoded}");
        assert_eq!(services.debug_trace_get(99), Ok(LuaValue::Nil));
        let close = LuaValue::table([
            ("enabled", LuaValue::Bool(false)),
            ("handle", LuaValue::Integer(handle)),
        ]);
        assert_eq!(services.debug_trace(&close), Ok(handle));
        assert_eq!(services.debug_trace_get(handle), Ok(LuaValue::Nil));
        let error = services.debug_trace(&close).expect_err("already closed");
        assert_eq!(error.code, "E_DEF_INVALID");
    }

    #[test]
    fn debug_control_stays_unimplemented() {
        let services = debug_services(true, true);
        let error = services
            .debug_control("reload_plugin", "xuepoo.test")
            .expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn spawn_without_grant_denies_capability() {
        let services = services();
        let error = services
            .process_spawn(&["status".to_owned()])
            .expect_err("grant absent must deny");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
    }

    fn fs_list_test_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1348-services-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs")).expect("create docs");
        std::fs::write(dir.join("docs/a.txt"), "a").expect("write a");
        std::fs::write(dir.join("docs/b.txt"), "b").expect("write b");
        dir
    }

    /// Window instant used by the rate-window tests.
    ///
    /// The returned instant is a fixed offset from the services' window start,
    /// not [`Instant::now`]: every call in a test therefore charges the same
    /// window, so a slow runner cannot advance the window mid-loop and hand a
    /// plugin fresh capacity it did not earn.
    fn frozen_window(services: &PluginServices) -> Instant {
        services.fs_window_start.get() + Duration::from_millis(1)
    }

    /// Scan budget for tests that assert window accounting, not scan time.
    ///
    /// The production direct-call default is the RC-1 50 ms budget. A loaded
    /// runner can spend that before the first directory entry is read, so a
    /// rate-window test would abort with `E_TIMEOUT` before reaching the
    /// denial it asserts. These tests keep the production window semantics
    /// and only opt out of the scan deadline.
    const FIXTURE_SCAN_BUDGET: Duration = Duration::from_secs(60);

    /// One `fs.list` call at a frozen window instant.
    fn list_at(services: &PluginServices, now: Instant) -> Result<LuaValue, BridgeError> {
        services.fs_list_at("~/docs", 16, now + FIXTURE_SCAN_BUDGET, now)
    }

    /// One `fs.list` call with the production window clock and a fixture scan
    /// budget, for tests whose subject is neither the window nor the deadline.
    fn list_with_generous_budget(services: &PluginServices) -> Result<LuaValue, BridgeError> {
        services.fs_list_with_expiry("~/docs", 16, Instant::now() + FIXTURE_SCAN_BUDGET)
    }

    #[test]
    fn fs_list_serves_granted_entries_with_untrusted_shape() {
        let home = fs_list_test_home("granted");
        let services = services();
        services.set_fs_backend(Some(Rc::new(FsListBackend::new(&home))));
        services.set_granted_capabilities(vec!["fs.read:~/docs/**".to_string()]);

        let value = list_with_generous_budget(&services).expect("granted listing");
        let LuaValue::Table(entries) = value else {
            panic!("fs.list must return an array");
        };
        assert_eq!(entries.len(), 2);
        for (index, expected) in ["~/docs/a.txt", "~/docs/b.txt"].iter().enumerate() {
            let pair_key = LuaValue::Integer(index as i64 + 1);
            let (_, entry) = entries
                .iter()
                .find(|(key, _)| *key == pair_key)
                .expect("array entry");
            assert_eq!(
                entry.get("name"),
                Some(&LuaValue::String((*expected).to_string()))
            );
            assert_eq!(
                entry.get("kind"),
                Some(&LuaValue::String("file".to_string()))
            );
            assert_eq!(entry.get("untrusted"), Some(&LuaValue::Bool(true)));
        }
    }

    #[test]
    fn fs_list_without_backend_or_grant_fails_closed() {
        let home = fs_list_test_home("closed");
        let services = services();
        let error = list_with_generous_budget(&services).expect_err("no backend must fail");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");

        services.set_fs_backend(Some(Rc::new(FsListBackend::new(&home))));
        let error = list_with_generous_budget(&services).expect_err("no grant must fail");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");
    }

    #[test]
    fn fs_list_tight_loop_hits_the_per_plugin_window() {
        let home = fs_list_test_home("budget");
        let services = services();
        services.set_fs_backend(Some(Rc::new(FsListBackend::new(&home))));
        services.set_granted_capabilities(vec!["fs.read:~/docs/**".to_string()]);

        // Frozen window instant: the denial must come from the charges, not
        // from wall time elapsing mid-loop (CTX-1375 follow-up).
        let now = frozen_window(&services);
        let mut denial = None;
        for call in 1..=(FS_LIST_MAX_ENTRIES + 1) {
            match list_at(&services, now) {
                Ok(_) => {}
                Err(error) => {
                    denial = Some((call, error));
                    break;
                }
            }
        }
        let (call, error) = denial.expect("a tight loop must exhaust the per-plugin window");
        assert_eq!(error.code, "E_FS_OVER_BOUND");
        // Both window dimensions are charged: with the byte budget at one
        // full listing page, the returned name bytes exhaust the window well
        // before the operation cap. Removing the byte charge lets the loop
        // reach the op cap instead.
        assert!(
            call < FS_LIST_MAX_ENTRIES,
            "byte budget must deny before the op cap (denied at call {call})"
        );
    }

    #[test]
    fn fs_list_empty_pages_still_charge_the_op_window() {
        // Empty pages return no bytes, so only the operation window can
        // bound a polling loop. This is the admission path, distinct from
        // the returned-byte charge.
        let home = fs_list_test_home("budget-empty");
        for name in ["a.txt", "b.txt"] {
            std::fs::remove_file(home.join("docs").join(name)).expect("clear fixture");
        }
        let services = services();
        services.set_fs_backend(Some(Rc::new(FsListBackend::new(&home))));
        services.set_granted_capabilities(vec!["fs.read:~/docs/**".to_string()]);

        // Frozen window instant: 1025 real calls must not be able to outlive
        // the accepted window on a loaded runner.
        let now = frozen_window(&services);
        let mut denial = None;
        for call in 1..=(FS_LIST_MAX_ENTRIES + 1) {
            match list_at(&services, now) {
                Ok(value) => {
                    let LuaValue::Table(entries) = value else {
                        panic!("fs.list must return an array");
                    };
                    assert!(entries.is_empty());
                }
                Err(error) => {
                    denial = Some((call, error));
                    break;
                }
            }
        }
        let (call, error) = denial.expect("empty pages must still exhaust the op window");
        assert_eq!(error.code, "E_FS_OVER_BOUND");
        assert_eq!(call, FS_LIST_MAX_ENTRIES + 1);
    }

    #[test]
    fn fs_list_window_reopens_after_the_accepted_window() {
        // A plugin that exhausts the op window must regain capacity once the
        // accepted window elapses. Every call takes an explicit window
        // instant, so the boundary is exact and the test never sleeps: a
        // loaded runner cannot reset the window underneath the 1024-call
        // exhaustion loop.
        let home = fs_list_test_home("window-reset");
        for name in ["a.txt", "b.txt"] {
            std::fs::remove_file(home.join("docs").join(name)).expect("clear fixture");
        }
        let services = services();
        services.set_fs_backend(Some(Rc::new(FsListBackend::new(&home))));
        services.set_granted_capabilities(vec!["fs.read:~/docs/**".to_string()]);

        // Empty pages return no bytes, so the op window is what runs out.
        let start = services.fs_window_start.get();
        let inside = start + Duration::from_millis(1);
        for _ in 0..FS_LIST_MAX_ENTRIES {
            list_at(&services, inside).expect("window has capacity");
        }
        let error = list_at(&services, inside).expect_err("exhausted window must deny");
        assert_eq!(error.code, "E_FS_OVER_BOUND");

        // Still inside the same window: the usage is not cleared.
        let nearly_elapsed = start + (FS_LIST_WINDOW - Duration::from_millis(1));
        let error = list_at(&services, nearly_elapsed)
            .expect_err("usage must survive until the window elapses");
        assert_eq!(error.code, "E_FS_OVER_BOUND");

        // One full window later the usage clears and listing resumes.
        let reopened = start + FS_LIST_WINDOW;
        list_at(&services, reopened).expect("elapsed window must restore capacity");
    }

    fn env_services(grants: &[&str], values: &[(&str, &str)]) -> PluginServices {
        let services = services();
        let keys: BTreeSet<String> = grants.iter().map(|key| (*key).to_string()).collect();
        services
            .set_env_grants(keys)
            .expect("test grants are valid");
        let map: BTreeMap<String, String> = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        services.set_env_source(Some(Rc::new(MapEnv::new(map))));
        services
    }

    #[test]
    fn env_without_grant_is_not_implemented() {
        let services = env_services(&[], &[("HOME", "/home/tester")]);
        let error = services
            .env_get("HOME")
            .expect_err("grant absent must deny");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        assert_eq!(error.class, "runtime");
        let error = services
            .env_has("HOME")
            .expect_err("grant absent must deny");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn env_granted_key_resolves_and_absent_reads_none() {
        let services = env_services(&["HOME", "EMPTY_VAR"], &[("HOME", "/home/tester")]);
        assert_eq!(
            HostServices::env_get(&services, "HOME"),
            Ok(Some(LuaValue::String("/home/tester".to_string())))
        );
        assert_eq!(HostServices::env_has(&services, "HOME"), Ok(true));
        assert_eq!(HostServices::env_get(&services, "EMPTY_VAR"), Ok(None));
        assert_eq!(HostServices::env_has(&services, "EMPTY_VAR"), Ok(false));
    }

    #[test]
    fn env_prefix_wildcard_grant_authorizes_get_and_has() {
        // CTX-0830 (#1483): `APP_*` authorizes matching keys through
        // `env_get`/`env_has` while non-matching keys stay fail-closed.
        let services = env_services(&["APP_*"], &[("APP_TOKEN", "secret"), ("OTHER", "nope")]);
        assert_eq!(
            HostServices::env_get(&services, "APP_TOKEN"),
            Ok(Some(LuaValue::String("secret".to_string())))
        );
        assert_eq!(HostServices::env_has(&services, "APP_TOKEN"), Ok(true));
        let error = services
            .env_get("OTHER")
            .expect_err("non-matching key must deny");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        let error = services
            .env_has("OTHER")
            .expect_err("non-matching key must deny");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn env_exact_grant_does_not_match_longer_key() {
        // Exact-key behavior is unchanged: `HOME` does not authorize `HOMELY`.
        let services = env_services(&["HOME"], &[("HOME", "x"), ("HOMELY", "y")]);
        assert_eq!(
            HostServices::env_get(&services, "HOME"),
            Ok(Some(LuaValue::String("x".to_string())))
        );
        let error = services
            .env_get("HOMELY")
            .expect_err("longer key must deny");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn env_bare_star_grant_rejected_at_set() {
        // The bare-star allow-all is rejected at `set_env_grants`, and the
        // existing set is left untouched (fail-closed, fail-whole-set).
        let services = services();
        let bad: BTreeSet<String> = BTreeSet::from(["*".to_string()]);
        let error = services
            .set_env_grants(bad)
            .expect_err("bare star must fail");
        assert_eq!(error.code, "E_DEF_INVALID");
        assert!(services.env_grants().is_empty());
    }

    #[test]
    fn env_key_shape_rejected_before_grants() {
        // ADR 0006 normative (bitty#1751): mixed-case and over-64-byte keys
        // are rejected with the contract denial before any grant check.
        let services = env_services(&["HOME"], &[("HOME", "x")]);
        for key in [
            "",
            "has space",
            "9LIVES",
            "lower-ok?",
            "home",
            "Home",
            "_x1",
        ] {
            let error = HostServices::env_get(&services, key).expect_err("shape must deny");
            assert_eq!(error.code, "E_ENV_KEY_INVALID", "key '{key}'");
        }
        let long = "A".repeat(ENV_KEY_MAX_BYTES + 1);
        let error = HostServices::env_has(&services, &long).expect_err("over-bound must deny");
        assert_eq!(error.code, "E_ENV_KEY_INVALID");
    }

    #[test]
    fn env_oversize_value_fails_closed() {
        let big = "x".repeat(MAX_ENV_VALUE_BYTES + 1);
        let services = env_services(&["BIG_VAR"], &[("BIG_VAR", big.as_str())]);
        let error = services.env_get("BIG_VAR").expect_err("oversize must deny");
        assert_eq!(error.code, "E_DEF_LIMIT");
        assert_eq!(error.class, "budget");
        // Presence alone does not cross the value.
        assert_eq!(HostServices::env_has(&services, "BIG_VAR"), Ok(true));
    }

    #[test]
    fn env_grant_set_validates_and_bounds() {
        let services = services();
        let bad: BTreeSet<String> = BTreeSet::from(["9LIVES".to_string()]);
        let error = services
            .set_env_grants(bad)
            .expect_err("malformed grant must fail");
        assert_eq!(error.code, "E_DEF_INVALID");
        assert!(services.env_grants().is_empty());
        let many: BTreeSet<String> = (0..MAX_ENV_GRANTS + 1)
            .map(|index| format!("VAR_{index}"))
            .collect();
        let error = services
            .set_env_grants(many)
            .expect_err("over-limit must fail");
        assert_eq!(error.code, "E_DEF_LIMIT");
        assert!(services.env_grants().is_empty());
    }

    #[test]
    fn spawn_without_backend_is_unavailable() {
        let services = services();
        services.set_spawn_git(true);
        assert!(services.has_spawn_git());
        let error = services
            .process_spawn(&["status".to_owned()])
            .expect_err("backend absent must be unavailable");
        assert_eq!(error.code, "E_SPAWN_UNAVAILABLE");
    }

    #[test]
    fn spawn_with_backend_passes_argv_through() {
        let services = services();
        services.set_spawn_git(true);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen_clone = seen.clone();
        services.set_spawn_backend(Some(Rc::new(move |args: &[String]| {
            seen_clone.borrow_mut().push(args.to_vec());
            Ok(LuaValue::table([(
                "output",
                LuaValue::String("ok".to_owned()),
            )]))
        })));
        let result = services
            .process_spawn(&["status".to_owned(), "--porcelain".to_owned()])
            .expect("backend serves");
        assert_eq!(
            result.get("output"),
            Some(&LuaValue::String("ok".to_owned()))
        );
        assert_eq!(
            seen.borrow().as_slice(),
            &[vec!["status".to_owned(), "--porcelain".to_owned()]]
        );
    }

    fn ui_services(access: UiAccess) -> PluginServices {
        let services = services();
        services.set_ui_access(access);
        services
    }

    fn rich_only() -> UiAccess {
        UiAccess {
            rich: true,
            overlay: false,
            overlay_focus: false,
            claims: Vec::new(),
        }
    }

    /// Services with the focusable-overlay grants and a wired capture switch.
    fn focus_services() -> PluginServices {
        let services = ui_services(UiAccess {
            rich: true,
            overlay: true,
            overlay_focus: true,
            claims: Vec::new(),
        });
        services.set_overlay_capture(Rc::new(RefCell::new(OverlayCapture::new())));
        services
    }

    /// Mount one overlay block and acquire capture for it; returns the handle.
    fn mount_and_acquire(services: &PluginServices) -> i64 {
        let handle = services
            .ui_mount("overlay", &UiNode::text("modal"))
            .expect("overlay mount");
        services
            .ui_overlay_acquire(handle)
            .expect("capture acquire");
        handle
    }

    /// One generation of `id` sharing `capture`, with its own handle epoch
    /// so handles never alias across generations.
    fn generation_services(
        id: &str,
        epoch: u32,
        capture: &Rc<RefCell<OverlayCapture>>,
    ) -> PluginServices {
        let services = PluginServices::new(
            id,
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(UnavailableSnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        );
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: true,
            overlay_focus: true,
            claims: Vec::new(),
        });
        services.set_overlay_capture(Rc::clone(capture));
        services.set_ui_epoch(epoch);
        services
    }

    #[test]
    fn ui_mount_defaults_deny() {
        let services = services();
        let error = services
            .ui_mount("bottom", &UiNode::text("x"))
            .expect_err("default access must deny");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        services.with_ui_blocks(|blocks| assert!(blocks.is_empty()));
    }

    #[test]
    fn ui_mount_rejects_unknown_slot_at_host_boundary() {
        let services = ui_services(rich_only());
        for slot in ["nowhere", "statusline"] {
            let error = services
                .ui_mount(slot, &UiNode::text("x"))
                .expect_err("unknown slot must fail closed");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID", "{slot}");
        }
    }

    #[test]
    fn overlay_slot_requires_ui_overlay() {
        let services = ui_services(rich_only());
        services
            .ui_mount("bottom", &UiNode::text("ok"))
            .expect("bottom mount");
        let error = services
            .ui_mount("overlay", &UiNode::text("denied"))
            .expect_err("overlay needs ui.overlay");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: true,
            overlay_focus: false,
            claims: Vec::new(),
        });
        // CTX-0941: with the grant, the Core hosts the focusable overlay and
        // retains the block (not a band) so it can own the transient capture.
        let handle = services
            .ui_mount("overlay", &UiNode::text("hosted"))
            .expect("overlay is hosted");
        services.with_ui_blocks(|blocks| {
            assert_eq!(blocks.len(), 2);
            assert_eq!(
                blocks.get(handle).map(UiBlock::ui_slot),
                Some(UiSlot::Overlay)
            );
        });
    }

    #[test]
    fn overlay_focus_gate_denies_without_grant_and_names_capability() {
        // Accepted W-01 v2: without `ui.overlay.focus` every capture call
        // fails with `E_CAPABILITY_DENIED` naming the capability.
        let services = ui_services(UiAccess {
            rich: true,
            overlay: true,
            overlay_focus: false,
            claims: Vec::new(),
        });
        services.set_overlay_capture(Rc::new(RefCell::new(OverlayCapture::new())));
        let handle = services
            .ui_mount("overlay", &UiNode::text("modal"))
            .expect("v1 mount needs only ui.overlay");
        for error in [
            services.ui_overlay_acquire(handle).expect_err("acquire"),
            services
                .ui_overlay_acquire_with_spec("t", "p")
                .expect_err("spec acquire"),
            services
                .ui_overlay_update(handle, &UiNode::text("x"))
                .expect_err("update"),
            services
                .ui_overlay_poll_detailed(handle, 4)
                .expect_err("poll"),
        ] {
            assert_eq!(error.code, "E_CAPABILITY_DENIED");
            assert!(
                error.message.contains("ui.overlay.focus"),
                "denial names the capability: {}",
                error.message
            );
        }
        // Release is grant-free so cleanup can never wedge: with no live
        // session it is an ok-noop `false`, never a denial.
        assert!(
            !services
                .ui_overlay_release_with_reason(handle, None)
                .expect("release without a session is an ok-noop"),
            "no live session releases nothing"
        );
    }

    #[test]
    fn overlay_update_replaces_content_for_owner_only() {
        let services = focus_services();
        let handle = mount_and_acquire(&services);
        assert!(
            services
                .ui_overlay_update(handle, &UiNode::text("results"))
                .expect("owner update"),
            "owner update replaces content"
        );
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.get(handle).map(|block| block.node().clone()),
                Some(UiNode::text("results"))
            );
            assert_eq!(
                blocks.get(handle).map(UiBlock::version),
                Some(2),
                "update bumps the version"
            );
        });
        // A foreign handle fails with `E_UI_NOT_OWNER` and keeps content.
        let other = services
            .ui_mount("overlay", &UiNode::text("other"))
            .expect("second block");
        let error = services
            .ui_overlay_update(other, &UiNode::text("hijack"))
            .expect_err("non-owner update must fail");
        assert_eq!(error.code, E_UI_NOT_OWNER);
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.get(other).map(|block| block.node().clone()),
                Some(UiNode::text("other")),
                "previous content is kept"
            );
        });
        // After release the handle is dead: update fails, content kept.
        assert!(
            services
                .ui_overlay_release_with_reason(handle, None)
                .expect("release")
        );
        let error = services
            .ui_overlay_update(handle, &UiNode::text("stale"))
            .expect_err("released handle must fail");
        assert_eq!(error.code, E_UI_NOT_OWNER);
    }

    #[test]
    fn overlay_release_reason_is_validated_and_remembered() {
        let services = focus_services();
        let handle = mount_and_acquire(&services);
        // An invalid reason is a validation error and the session is
        // unchanged (still active).
        let error = services
            .ui_overlay_release_with_reason(handle, Some("bogus"))
            .expect_err("invalid reason must fail");
        assert_eq!(error.code, "E_DEF_INVALID");
        let live = services
            .ui_overlay_poll_detailed(handle, 4)
            .expect("session survives invalid reason");
        assert!(live.active);
        // A submitted disposition ends the session and is reported on the
        // next poll with no events.
        assert!(
            services
                .ui_overlay_release_with_reason(handle, Some("submitted"))
                .expect("submitted release")
        );
        let after = services
            .ui_overlay_poll_detailed(handle, 4)
            .expect("poll after release");
        assert!(!after.active);
        assert_eq!(after.reason.as_deref(), Some("submitted"));
        assert!(after.events.is_empty());
        // Idempotent: releasing the ended session again is a
        // success-without-effect `false`, never an error.
        assert!(
            !services
                .ui_overlay_release_with_reason(handle, None)
                .expect("idempotent release"),
            "an already-ended session releases nothing"
        );
    }

    #[test]
    fn overlay_safe_mode_denies_capture_but_not_release() {
        let services = focus_services();
        services.set_safe_mode(true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("modal"))
            .expect("v1 mount itself is not the focusable surface");
        let error = services
            .ui_overlay_acquire(handle)
            .expect_err("acquire in safe mode");
        assert_eq!(error.code, E_UI_UNAVAILABLE);
        let error = services
            .ui_overlay_acquire_with_spec("t", "p")
            .expect_err("spec acquire in safe mode");
        assert_eq!(error.code, E_UI_UNAVAILABLE);
        let error = services
            .ui_overlay_update(handle, &UiNode::text("x"))
            .expect_err("update in safe mode");
        assert_eq!(error.code, E_UI_UNAVAILABLE);
        let error = services
            .ui_overlay_poll_detailed(handle, 4)
            .expect_err("poll in safe mode");
        assert_eq!(error.code, E_UI_UNAVAILABLE);
        // Cleanup is never gated: a session acquired before safe mode still
        // releases.
        services.set_safe_mode(false);
        let live = mount_and_acquire(&services);
        services.set_safe_mode(true);
        assert!(
            services
                .ui_overlay_release_with_reason(live, Some("cancelled"))
                .expect("release in safe mode"),
            "release stays available so cleanup can never wedge"
        );
    }

    #[test]
    fn overlay_spec_acquire_mounts_surface_and_denies_second_owner() {
        let services = focus_services();
        let handle = services
            .ui_overlay_acquire_with_spec("Palette", "Type…")
            .expect("spec acquire");
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.get(handle).map(UiBlock::ui_slot),
                Some(UiSlot::Overlay),
                "spec acquire mounts the focusable surface"
            );
        });
        let error = services
            .ui_overlay_acquire_with_spec("Other", "")
            .expect_err("second acquire must fail");
        assert_eq!(error.code, E_UI_ALREADY_CAPTURED);
        services.with_ui_blocks(|blocks| {
            assert_eq!(blocks.len(), 1, "denied acquire leaves no orphan block");
        });
    }

    #[test]
    fn overlay_spec_acquire_unmounts_surface_on_session_end() {
        // A spec session's transient surface leaves with the session:
        // release unmounts it, so the next session presents only new content
        // and repeated open/close cycles leak no block slots.
        let services = focus_services();
        let first = services
            .ui_overlay_acquire_with_spec("A", "")
            .expect("first spec acquire");
        assert!(
            services
                .ui_overlay_release_with_reason(first, None)
                .expect("release"),
            "owner release ends the session"
        );
        services.with_ui_blocks(|blocks| {
            assert!(
                blocks.get(first).is_none(),
                "the released surface is unmounted"
            );
            assert!(blocks.is_empty(), "no orphan block survives release");
        });
        let second = services
            .ui_overlay_acquire_with_spec("B", "")
            .expect("second spec acquire");
        assert_ne!(first, second, "sessions mint distinct handles");
        services.with_ui_blocks(|blocks| {
            assert_eq!(blocks.len(), 1, "only the live surface is retained");
            assert!(
                blocks.get(second).is_some(),
                "the live surface presents the new session"
            );
        });
        assert!(
            services
                .ui_overlay_update(second, &UiNode::text("B"))
                .expect("owner update"),
            "update routes to the live surface only"
        );
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.get(second).map(|block| block.node().clone()),
                Some(UiNode::text("B")),
                "new content only, no stale block"
            );
        });
    }

    #[test]
    fn overlay_spec_reacquire_after_idle_timeout_drops_stale_surface() {
        // Backstop for the lazy-expiry-inside-acquire path: when the previous
        // session ends synchronously within the new acquire (no runtime tick
        // disposed it), the stale surface still leaves with it.
        let services = focus_services();
        let first = services
            .ui_overlay_acquire_with_spec("A", "")
            .expect("first spec acquire");
        assert!(
            services
                .overlay_capture()
                .expect("capture")
                .borrow_mut()
                .force_expire("xuepoo.test", first),
            "the live session is rewound past its deadline"
        );
        let second = services
            .ui_overlay_acquire_with_spec("B", "")
            .expect("second spec acquire");
        services.with_ui_blocks(|blocks| {
            assert!(
                blocks.get(first).is_none(),
                "the timed-out surface is unmounted"
            );
            assert_eq!(blocks.len(), 1, "only the live surface is retained");
            assert!(blocks.get(second).is_some());
        });
    }

    #[test]
    fn overlay_mechanism_acquire_disposes_expired_spec_surface() {
        // CTX-0973: lazy expiry inside mechanism acquire must not leave the
        // previous spec surface behind. The expired surface is disposed on
        // the acquire path, so no stale surface survives acquisition.
        let services = focus_services();
        let stale = services
            .ui_overlay_acquire_with_spec("Stale", "")
            .expect("first spec acquire");
        let live = services
            .ui_mount("overlay", &UiNode::text("retained"))
            .expect("mechanism mount while the spec session holds capture");
        assert!(
            services
                .overlay_capture()
                .expect("capture")
                .borrow_mut()
                .force_expire("xuepoo.test", stale),
            "the spec session is rewound past its deadline"
        );
        services
            .ui_overlay_acquire(live)
            .expect("mechanism acquire after expiry");
        services.with_ui_blocks(|blocks| {
            assert!(
                blocks.get(stale).is_none(),
                "the expired spec surface is unmounted on mechanism acquire"
            );
            assert!(
                blocks.get(live).is_some(),
                "the mechanism session block is retained"
            );
            assert_eq!(blocks.len(), 1, "no stale surface survives acquisition");
        });
    }

    #[test]
    fn overlay_cross_generation_spec_acquire_disposes_expired_surface() {
        // CTX-0973: lazy expiry inside spec acquire must dispose the previous
        // owner's surface even across generations. The peer table lets the
        // new generation reach the expired owner's registry.
        let capture = Rc::new(RefCell::new(OverlayCapture::new()));
        let peers: OverlayPeers = Rc::new(RefCell::new(BTreeMap::new()));
        let first = Rc::new(generation_services("xuepoo.alpha", 11, &capture));
        let second = Rc::new(generation_services("xuepoo.beta", 12, &capture));
        for (services, id) in [(&first, "xuepoo.alpha"), (&second, "xuepoo.beta")] {
            services.set_overlay_peers(peers.clone());
            peers
                .borrow_mut()
                .entry(id.to_string())
                .or_default()
                .push(Rc::downgrade(services));
        }
        let stale = first
            .ui_overlay_acquire_with_spec("A", "")
            .expect("first generation spec acquire");
        assert!(
            capture.borrow_mut().force_expire("xuepoo.alpha", stale),
            "the first session is rewound past its deadline"
        );
        let live = second
            .ui_overlay_acquire_with_spec("B", "")
            .expect("second generation spec acquire after expiry");
        first.with_ui_blocks(|blocks| {
            assert!(
                blocks.get(stale).is_none(),
                "the expired previous-owner surface is unmounted"
            );
            assert!(blocks.is_empty(), "no orphan survives in the old registry");
        });
        second.with_ui_blocks(|blocks| {
            assert_eq!(blocks.len(), 1, "only the live surface is retained");
            assert!(
                blocks.get(live).is_some(),
                "the new session presents its own surface"
            );
        });
        assert!(
            capture.borrow().is_owner("xuepoo.beta", live),
            "the new generation holds the capture"
        );
    }

    #[test]
    fn overlay_mixed_same_generation_shows_session_surface() {
        // CTX-0973: a same-generation mixed mount must present the session
        // surface, not the first-mounted stale block. The spec surface is
        // mounted first, the mechanism block second; after expiry the
        // mechanism acquire disposes the stale spec surface, so iteration
        // order (what presentation reads first) is the live session.
        let services = focus_services();
        let stale = services
            .ui_overlay_acquire_with_spec("Stale", "")
            .expect("spec surface mounted first");
        let live = services
            .ui_mount("overlay", &UiNode::text("retained"))
            .expect("mechanism block mounted second");
        assert!(
            services
                .overlay_capture()
                .expect("capture")
                .borrow_mut()
                .force_expire("xuepoo.test", stale),
            "the spec session is rewound past its deadline"
        );
        services
            .ui_overlay_acquire(live)
            .expect("mechanism acquire takes over");
        services.with_ui_blocks(|blocks| {
            assert!(
                blocks.get(stale).is_none(),
                "the first-mounted stale surface is gone"
            );
            let (first_handle, _) = blocks.iter().next().expect("one live block remains");
            assert_eq!(
                first_handle, live,
                "the first block is the session surface, not stale content"
            );
        });
    }

    #[test]
    fn overlay_mechanism_block_survives_release() {
        // Only the transient spec surface is disposed on session end: a
        // mechanism-path block (mounted through `ui.mount`) is the plugin's
        // retained content and stays mounted after release.
        let services = focus_services();
        let handle = mount_and_acquire(&services);
        assert!(
            services
                .ui_overlay_release_with_reason(handle, None)
                .expect("release")
        );
        services.with_ui_blocks(|blocks| {
            assert!(
                blocks.get(handle).is_some(),
                "mechanism content is retained after release"
            );
        });
    }

    #[test]
    fn overlay_release_is_noop_unless_live_and_denies_live_foreign() {
        // Pre-change contract with live-session enforcement: anything but
        // the live session is an ok-noop `false` (already-ended, never
        // acquired, or stale from before another session started), while
        // ending somebody else's live session denies with `E_UI_NOT_OWNER`.
        let capture = Rc::new(RefCell::new(OverlayCapture::new()));
        let first = generation_services("xuepoo.alpha", 1, &capture);
        let second = generation_services("xuepoo.beta", 2, &capture);

        // No live session: every handle is an ok-noop, never an error.
        assert!(
            !first.ui_overlay_release(999).expect("foreign noop"),
            "unowned handle with no live session releases nothing"
        );
        assert!(
            !second.ui_overlay_release(999).expect("never-acquired noop"),
            "never-acquired handle releases nothing"
        );

        // Live session: the owner releases; ending the live handle as a
        // foreign generation denies and the session survives.
        let owned = first
            .ui_mount("overlay", &UiNode::text("modal"))
            .expect("mount");
        first.ui_overlay_acquire(owned).expect("acquire");
        let foreign = second
            .ui_mount("overlay", &UiNode::text("other"))
            .expect("mount");
        let error = second
            .ui_overlay_release(owned)
            .expect_err("ending a foreign live session must deny");
        assert_eq!(error.code, E_UI_NOT_OWNER);
        assert!(
            capture.borrow().is_owner("xuepoo.alpha", owned),
            "the denied release leaves the live session undisturbed"
        );
        assert!(first.ui_overlay_release(owned).expect("owner release"));

        // Ended session: the owner's repeat is an ok-noop even after another
        // generation acquired in between — idempotency never depends on
        // global state.
        let next = second
            .ui_mount("overlay", &UiNode::text("next"))
            .expect("mount");
        second.ui_overlay_acquire(next).expect("re-acquire");
        assert!(
            !first.ui_overlay_release(owned).expect("stale repeat"),
            "a stale handle releases nothing once its session ended"
        );
        assert!(
            !second.ui_overlay_release(foreign).expect("foreign stale"),
            "a foreign handle that is not the live session releases nothing"
        );
        assert!(
            capture.borrow().is_owner("xuepoo.beta", next),
            "stale releases leave the live session undisturbed"
        );
    }

    #[test]
    fn tabline_slot_requires_exclusive_claim() {
        let services = ui_services(rich_only());
        let error = services
            .ui_mount("tabline", &UiNode::text("x"))
            .expect_err("unclaimed tabline must fail closed");
        assert_eq!(error.code, "E_UI_CLAIM_REQUIRED");
        // CTX-0974 (DEC-0100 waiver): deprecated `tabline` claim alias purged;
        // only the canonical `workspaceline` claim satisfies the gate.
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: false,
            overlay_focus: false,
            claims: vec!["tabline".to_string()],
        });
        let error = services
            .ui_mount("tabline", &UiNode::text("claimed"))
            .expect_err("purged tabline alias must fail the claim gate");
        assert_eq!(error.code, "E_UI_CLAIM_REQUIRED");
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: false,
            overlay_focus: false,
            claims: vec!["workspaceline".to_string()],
        });
        let error = services
            .ui_mount("tabline", &UiNode::text("canonical claim"))
            .expect_err("canonical claim passes the gate, slot still unhosted");
        assert_eq!(error.code, crate::E_UI_UNAVAILABLE);
        services.with_ui_blocks(|blocks| assert!(blocks.is_empty()));
    }

    #[test]
    fn every_v1_slot_mounts_or_fails_closed_per_placement_policy() {
        // CTX-0923: each accepted slot either lands in the registry (band
        // slots) or fails closed with a typed error; none is accepted and
        // then left unrendered.
        let services = ui_services(UiAccess {
            rich: true,
            overlay: true,
            overlay_focus: false,
            claims: vec!["workspaceline".to_string()],
        });
        for slot in UiSlot::ALL {
            let result = services.ui_mount(slot.as_str(), &UiNode::text(slot.as_str()));
            match ui_slot_placement(slot) {
                UiSlotPlacement::Band(_) | UiSlotPlacement::Overlay => {
                    let handle = result.unwrap_or_else(|e| panic!("{slot}: {e:?}"));
                    services.with_ui_blocks(|blocks| {
                        assert_eq!(blocks.get(handle).map(UiBlock::ui_slot), Some(slot));
                    });
                }
                UiSlotPlacement::Unsupported(_) => {
                    let error = result.expect_err("unhosted slot must fail closed");
                    assert_eq!(error.code, crate::E_UI_UNAVAILABLE, "{slot}");
                }
            }
        }
        // top, bottom, overlay; left/right/tabline/terminal fail closed
        // (CTX-1341: no vertical band host, so no vertical block is retained).
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 3));
    }

    #[test]
    fn mount_update_round_trip_and_stale_handle() {
        let services = ui_services(rich_only());
        let handle = services
            .ui_mount("bottom", &UiNode::row(vec![UiNode::text("v1")]))
            .expect("mount");
        assert!(handle > 0);
        services.with_ui_blocks(|blocks| {
            let block = blocks.get(handle).expect("block retained");
            assert_eq!(block.slot(), "bottom");
            assert_eq!(block.version(), 1);
            assert_eq!(block.node().text_bytes(), 2);
        });
        assert!(
            services
                .ui_update(handle, &UiNode::text("v2"))
                .expect("update served")
        );
        services.with_ui_blocks(|blocks| {
            let block = blocks.get(handle).expect("block retained");
            assert_eq!(block.version(), 2);
            assert_eq!(block.node(), &UiNode::text("v2"));
        });
        assert!(
            !services
                .ui_update(handle + 999, &UiNode::text("stale"))
                .expect("stale lookup is not an error"),
            "stale handle must report false"
        );
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    }

    #[test]
    fn generation_handles_are_foreign_across_instances() {
        let first = ui_services(rich_only());
        first.set_ui_epoch(7);
        let handle = first
            .ui_mount("bottom", &UiNode::text("gen1"))
            .expect("mount");
        let second = ui_services(rich_only());
        second.set_ui_epoch(8);
        let own = second
            .ui_mount("bottom", &UiNode::text("gen2"))
            .expect("mount");
        assert_ne!(
            handle, own,
            "handles minted by different generations must not alias"
        );
        assert!(
            !second
                .ui_update(handle, &UiNode::text("gen2"))
                .expect("foreign lookup is not an error"),
            "a handle from another generation must report false even when the
             next generation has its own live block"
        );
        assert!(
            second
                .ui_update(own, &UiNode::text("gen2b"))
                .expect("own handle serves"),
            "the next generation's own handle must still serve"
        );
        second.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    }

    #[test]
    fn clear_invalidates_handles_without_aliasing_remounts() {
        let services = ui_services(rich_only());
        services.set_ui_epoch(3);
        let first = services
            .ui_mount("bottom", &UiNode::text("gen1"))
            .expect("mount");
        services.clear_ui_blocks();
        services.with_ui_blocks(|blocks| assert!(blocks.is_empty()));
        assert!(
            !services
                .ui_update(first, &UiNode::text("stale"))
                .expect("stale lookup is not an error"),
            "a cleared handle must report false"
        );
        let second = services
            .ui_mount("bottom", &UiNode::text("gen2"))
            .expect("remount");
        assert_ne!(
            first, second,
            "a remount after a clear must mint a fresh handle"
        );
        assert!(
            services
                .ui_update(second, &UiNode::text("gen2b"))
                .expect("fresh handle serves"),
            "the remounted handle must serve"
        );
    }

    #[test]
    fn block_registry_budget_fails_closed() {
        let services = ui_services(rich_only());
        for index in 0..UI_MAX_BLOCKS {
            services
                .ui_mount("bottom", &UiNode::text(format!("{index}")))
                .expect("under the block cap");
        }
        let error = services
            .ui_mount("bottom", &UiNode::text("overflow"))
            .expect_err("block cap must fail closed");
        assert_eq!(error.code, "E_UI_BLOCK_BUDGET");
        assert_eq!(error.class, "budget");
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), UI_MAX_BLOCKS));
    }

    #[test]
    fn aggregated_text_budget_fails_closed() {
        let services = ui_services(rich_only());
        let chunk = "x".repeat(UI_MAX_TEXT_BYTES);
        for _ in 0..(UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES) {
            services
                .ui_mount("bottom", &UiNode::text(chunk.clone()))
                .expect("exactly at the aggregate cap");
        }
        let error = services
            .ui_mount("bottom", &UiNode::text("one byte over"))
            .expect_err("aggregate cap must fail closed");
        assert_eq!(error.code, "E_UI_BLOCK_BUDGET");
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.len(),
                UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES
            )
        });
    }

    /// A node whose retained strings span text, tokens, commands, arg keys,
    /// and a string arg value (CTX-1340).
    fn retained_string_node(text_len: usize) -> UiNode {
        UiNode::Text {
            text: "x".repeat(text_len),
            fg: Some("accent".to_string()),
            bg: Some("surface.1".to_string()),
            bold: None,
            on_click: Some(ClickCommand {
                command: "workspace.switch".to_string(),
                args: vec![
                    ("id".to_string(), ClickArg::String("dev".to_string())),
                    ("focus".to_string(), ClickArg::Bool(true)),
                ],
                region: None,
            }),
            on_scroll: Some(ClickCommand {
                command: "bar.scroll".to_string(),
                args: vec![("step".to_string(), ClickArg::Number(1.0))],
                region: None,
            }),
            anchor: None,
            on_hover: None,
        }
    }

    #[test]
    fn aggregated_retained_budget_counts_every_string() {
        let overhead = retained_string_node(0).retained_string_bytes();
        assert!(
            overhead < UI_MAX_TEXT_BYTES,
            "the mixed-string overhead must leave room for text"
        );
        let node = retained_string_node(UI_MAX_TEXT_BYTES - overhead);
        assert_eq!(
            node.retained_string_bytes(),
            UI_MAX_TEXT_BYTES,
            "text + tokens + identifiers must fill exactly one 256 KiB slice"
        );
        assert!(
            node.text_bytes() < node.retained_string_bytes(),
            "the aggregate must charge more than the text-only measure"
        );

        let services = ui_services(rich_only());
        for _ in 0..(UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES) {
            services
                .ui_mount("bottom", &node)
                .expect("exactly at the retained-string aggregate cap");
        }
        let error = services
            .ui_mount("bottom", &UiNode::text("x"))
            .expect_err("one byte over the aggregate cap must fail closed");
        assert_eq!(error.code, "E_UI_BLOCK_BUDGET");
        assert_eq!(error.class, "budget");
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.len(),
                UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES
            )
        });
    }

    /// A node with no text whose click argument keys alone exceed 2 MiB.
    ///
    /// 2047 leaf nodes (the `Row` makes the subtree exactly `UI_MAX_NODES`)
    /// each carry one command with 16 distinct 64-byte keys and no string
    /// values, so text bytes stay zero and only keys can drive the budget.
    fn large_key_node() -> UiNode {
        let args = (0..UI_CLICK_MAX_ARGS)
            .map(|index| (format!("{index:064}"), ClickArg::Bool(true)))
            .collect::<Vec<_>>();
        let child = UiNode::Text {
            text: String::new(),
            fg: None,
            bg: None,
            bold: None,
            on_click: Some(ClickCommand {
                command: "c".to_string(),
                args,
                region: None,
            }),
            on_scroll: None,
            anchor: None,
            on_hover: None,
        };
        UiNode::Row {
            children: (0..(UI_MAX_NODES - 1)).map(|_| child.clone()).collect(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
            anchor: None,
            on_hover: None,
        }
    }

    #[test]
    fn large_click_arg_keys_are_charged_and_fail_closed() {
        let payload = large_key_node();
        assert_eq!(payload.text_bytes(), 0, "the payload carries no text");
        assert!(
            payload.retained_string_bytes() > UI_MAX_AGGREGATED_TEXT_BYTES,
            "argument keys alone must exceed the aggregate"
        );

        let services = ui_services(rich_only());
        let handle = services
            .ui_mount("bottom", &UiNode::text("seed"))
            .expect("seed mount");
        let update = services
            .ui_update(handle, &payload)
            .expect_err("key-heavy update must fail closed");
        assert_eq!(update.code, "E_UI_BLOCK_BUDGET");
        assert_eq!(update.class, "budget");
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.get(handle).expect("seed retained").node(),
                &UiNode::text("seed"),
                "a rejected update must leave the old block intact"
            );
        });

        let mount = services
            .ui_mount("bottom", &payload)
            .expect_err("key-heavy mount must fail closed");
        assert_eq!(mount.code, "E_UI_BLOCK_BUDGET");
        services.with_ui_blocks(|blocks| {
            assert_eq!(blocks.len(), 1, "a rejected mount must not be retained");
        });
    }

    /// Backing head shared by the service unit-test fixtures.
    const TEST_HEAD: &str = "runtime.inspect";

    fn service_host() -> (Rc<RefCell<ServiceDirectory>>, PluginServices) {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        let host = services();
        host.set_service_directory(directory.clone());
        host.set_service_manifest(
            vec![ProvidedService {
                iface: "calc.add".to_string(),
                version: "1.0.0".to_string(),
                args_schema: None,
                result_schema: None,
                heads: vec![TEST_HEAD.to_string()],
            }],
            vec![("calc.add".to_string(), ">=1.0".to_string())],
        );
        host.set_granted_capabilities(vec![TEST_HEAD.to_string()]);
        (directory, host)
    }

    fn publish_record(
        directory: &Rc<RefCell<ServiceDirectory>>,
        provider: &str,
        generation: u32,
        iface: &str,
        version: &str,
        methods: &[&str],
    ) {
        publish_backed_record(
            directory,
            provider,
            generation,
            iface,
            version,
            methods,
            &[TEST_HEAD],
        );
    }

    fn publish_backed_record(
        directory: &Rc<RefCell<ServiceDirectory>>,
        provider: &str,
        generation: u32,
        iface: &str,
        version: &str,
        methods: &[&str],
        heads: &[&str],
    ) {
        directory.borrow_mut().publish(ServiceRecord {
            provider: provider.to_string(),
            generation,
            iface: iface.to_string(),
            version: version.to_string(),
            methods: methods.iter().map(|name| (*name).to_string()).collect(),
            funcs: BTreeMap::new(),
            args_schema: None,
            result_schema: None,
            heads: heads.iter().map(|head| (*head).to_string()).collect(),
            vm: Weak::new(),
            suspended: false,
        });
    }

    #[test]
    fn provide_check_declared_passes_and_undeclared_rejected() {
        let (_, host) = service_host();
        assert!(host.service_provide_check("calc.add").is_ok());
        assert_eq!(host.provided_services().len(), 1);
        let error = host
            .service_provide_check("calc.other")
            .expect_err("undeclared provision must fail");
        assert_eq!(error.code, "E_SERVICE_UNDECLARED");
        assert_eq!(error.class, "validation");
    }

    #[test]
    fn provide_check_unbacked_rejected_with_naming_diagnostic() {
        let (_, host) = service_host();
        host.set_service_manifest(
            vec![ProvidedService {
                iface: "calc.raw".to_string(),
                version: "1.0.0".to_string(),
                args_schema: None,
                result_schema: None,
                heads: Vec::new(),
            }],
            Vec::new(),
        );
        let error = host
            .service_provide_check("calc.raw")
            .expect_err("unbacked provision must fail");
        assert_eq!(error.code, "E_SERVICE_UNDECLARED");
        assert!(error.message.contains("calc.raw"), "names the iface");
        assert!(error.message.contains("heads"), "names the field");
    }

    #[test]
    fn provide_check_ungranted_head_denied_without_new_authority() {
        let (_, host) = service_host();
        host.set_granted_capabilities(Vec::new());
        let error = host
            .service_provide_check("calc.add")
            .expect_err("head-spoofing provide must fail");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(error.class, "runtime");
        // The denial names the head, never a minted authority.
        assert!(error.message.contains(TEST_HEAD));
    }

    #[test]
    fn resolve_skips_records_whose_heads_are_uncovered() {
        let (directory, host) = service_host();
        publish_record(
            &directory,
            "xuepoo.granted",
            1,
            "calc.add",
            "1.0.0",
            &["add"],
        );
        publish_backed_record(
            &directory,
            "xuepoo.other",
            1,
            "calc.add",
            "2.0.0",
            &["add"],
            &["workspace.read"],
        );
        // The higher version is unresolvable without its head: resolution
        // serves the covered record, minting nothing.
        let route = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect("resolution serves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.granted");

        // With no covered record the failure is indistinguishable from
        // absent: the same code as no provider, nil when optional.
        host.set_granted_capabilities(Vec::new());
        let error = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect_err("ungranted consume must fail closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert_eq!(
            HostServices::service_resolve(&host, "calc.add", None, true).expect("optional serves"),
            None,
            "ungranted optional consume resolves nil like an absent service"
        );
    }

    #[test]
    fn service_call_without_covering_grant_fails_closed_as_gone() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 7, "calc.add", "1.0.0", &["add"]);
        host.set_granted_capabilities(Vec::new());
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            7,
            "calc.add",
            "1.0.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("ungranted call must fail closed");
        // Indistinguishable from a parked provider: same code, no head names.
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    /// Backing heads for the audio provider fixtures (CTX-1196,
    /// bitty#2009/#2013): read and control are different heads so read
    /// works ungranted-control.
    const AUDIO_STATE_HEAD: &str = "audio.state.read";
    const AUDIO_CONTROL_HEAD: &str = "audio.state.control";
    const AUDIO_SPECTRUM_HEAD: &str = "audio.spectrum";

    fn audio_host(granted: &[&str]) -> (Rc<RefCell<ServiceDirectory>>, PluginServices) {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        let host = services();
        host.set_service_directory(directory.clone());
        host.set_service_manifest(
            vec![
                ProvidedService {
                    iface: "audio.state".to_string(),
                    version: "1.0.0".to_string(),
                    args_schema: None,
                    result_schema: None,
                    heads: vec![AUDIO_STATE_HEAD.to_string()],
                },
                ProvidedService {
                    iface: "audio.control".to_string(),
                    version: "1.0.0".to_string(),
                    args_schema: None,
                    result_schema: None,
                    heads: vec![AUDIO_CONTROL_HEAD.to_string()],
                },
                ProvidedService {
                    iface: "audio.spectrum".to_string(),
                    version: "1.0.0".to_string(),
                    args_schema: None,
                    result_schema: None,
                    heads: vec![AUDIO_SPECTRUM_HEAD.to_string()],
                },
            ],
            vec![
                ("audio.state".to_string(), ">=1.0".to_string()),
                ("audio.control".to_string(), ">=1.0".to_string()),
                ("audio.spectrum".to_string(), ">=1.0".to_string()),
            ],
        );
        host.set_granted_capabilities(granted.iter().map(|head| (*head).to_string()).collect());
        publish_backed_record(
            &directory,
            "xuepoo.audio",
            1,
            "audio.state",
            "1.0.0",
            &["get"],
            &[AUDIO_STATE_HEAD],
        );
        publish_backed_record(
            &directory,
            "xuepoo.audio",
            1,
            "audio.control",
            "1.0.0",
            &["set_volume", "toggle_mute"],
            &[AUDIO_CONTROL_HEAD],
        );
        publish_backed_record(
            &directory,
            "xuepoo.audio",
            1,
            "audio.spectrum",
            "1.0.0",
            &["spectrum"],
            &[AUDIO_SPECTRUM_HEAD],
        );
        (directory, host)
    }

    #[test]
    fn audio_read_resolves_while_control_stays_absent() {
        // bitty#2013 acceptance: read works ungranted-control. The
        // consumer holds the read and spectrum heads only.
        let (_, host) = audio_host(&[AUDIO_STATE_HEAD, AUDIO_SPECTRUM_HEAD]);
        let route = HostServices::service_resolve(&host, "audio.state", None, false)
            .expect("read resolves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.audio");
        let route = HostServices::service_resolve(&host, "audio.spectrum", None, false)
            .expect("spectrum resolves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.audio");
        // Control without its head fails closed exactly like an absent
        // service: same code, nil when optional, no head names leaked.
        let error = HostServices::service_resolve(&host, "audio.control", None, false)
            .expect_err("ungranted control must fail closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert!(
            !error.message.contains(AUDIO_CONTROL_HEAD),
            "denial names no head"
        );
        assert_eq!(
            HostServices::service_resolve(&host, "audio.control", None, true)
                .expect("optional degrades to nil"),
            None,
            "ungranted control resolves nil like an absent service"
        );
    }

    #[test]
    fn audio_control_call_without_head_fails_closed_as_gone() {
        // bitty#2013 acceptance: control without its head fails closed
        // (negative test). A pinned-handle call reports `E_SERVICE_GONE`,
        // indistinguishable from a parked provider.
        let (_, host) = audio_host(&[AUDIO_STATE_HEAD]);
        for method in ["set_volume", "toggle_mute"] {
            let error = HostServices::service_call(
                &host,
                "xuepoo.audio",
                1,
                "audio.control",
                "1.0.0",
                method,
                &LuaValue::Nil,
            )
            .expect_err("ungranted control call must fail closed");
            assert_eq!(error.code, "E_SERVICE_GONE");
        }
    }

    #[test]
    fn audio_spectrum_ungranted_consume_matches_absent() {
        // bitty#2009 acceptance: ungranted consume is indistinguishable
        // from absent (same posture as `bitty.env`).
        let (_, host) = audio_host(&[]);
        let error = HostServices::service_resolve(&host, "audio.spectrum", None, false)
            .expect_err("ungranted spectrum must fail closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert_eq!(
            HostServices::service_resolve(&host, "audio.spectrum", None, true)
                .expect("optional degrades to nil"),
            None
        );
    }

    /// Native sensor fixtures (CTX-1199, bitty#2019/#2011/#2016).
    const SENSOR_CPU_HEAD: &str = "platform.cpu-read";
    const SENSOR_MEMORY_HEAD: &str = "platform.memory-read";
    const SENSOR_BATTERY_HEAD: &str = "platform.battery-read";

    #[allow(clippy::type_complexity)]
    fn sensor_host_with_fixtures(
        granted: &[&str],
        cpu_texts: Vec<Option<String>>,
        mem_texts: Vec<Option<String>>,
        battery_capacity: Option<&str>,
        now_ms: u64,
    ) -> (
        Rc<RefCell<ServiceDirectory>>,
        PluginServices,
        Rc<std::cell::Cell<usize>>,
        Rc<std::cell::Cell<usize>>,
        PathBuf,
    ) {
        use crate::plugin_runtime::system_sensors::host::{SENSOR_GENERATION, SensorHost};
        use crate::plugin_runtime::system_sensors::{
            BATTERY_BACKING_HEAD, CPU_BACKING_HEAD, MEMORY_BACKING_HEAD, SENSOR_PROVIDER,
            SYSTEM_BATTERY_IFACE, SYSTEM_CPU_IFACE, SYSTEM_MEMORY_IFACE,
        };
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        let host = services();
        host.set_service_directory(directory.clone());
        host.set_service_manifest(
            Vec::new(),
            vec![
                (SYSTEM_CPU_IFACE.to_string(), "^1.0".to_string()),
                (SYSTEM_MEMORY_IFACE.to_string(), "^1.0".to_string()),
                (SYSTEM_BATTERY_IFACE.to_string(), "^1.0".to_string()),
            ],
        );
        host.set_granted_capabilities(granted.iter().map(|head| (*head).to_string()).collect());
        for (iface, head) in [
            (SYSTEM_CPU_IFACE, CPU_BACKING_HEAD),
            (SYSTEM_MEMORY_IFACE, MEMORY_BACKING_HEAD),
            (SYSTEM_BATTERY_IFACE, BATTERY_BACKING_HEAD),
        ] {
            publish_backed_record(
                &directory,
                SENSOR_PROVIDER,
                SENSOR_GENERATION,
                iface,
                "1.0.0",
                &["get"],
                &[head],
            );
        }
        let cpu_calls = Rc::new(std::cell::Cell::new(0));
        let mem_calls = Rc::new(std::cell::Cell::new(0));
        let cpu_remaining = Rc::new(RefCell::new(cpu_texts.into_iter()));
        let mem_remaining = Rc::new(RefCell::new(mem_texts.into_iter()));
        let cpu_probed = Rc::clone(&cpu_calls);
        let mem_probed = Rc::clone(&mem_calls);
        let cpu_source: Rc<dyn Fn() -> Option<String>> = Rc::new(move || {
            cpu_probed.set(cpu_probed.get() + 1);
            cpu_remaining.borrow_mut().next().unwrap_or(None)
        });
        let mem_source: Rc<dyn Fn() -> Option<String>> = Rc::new(move || {
            mem_probed.set(mem_probed.get() + 1);
            mem_remaining.borrow_mut().next().unwrap_or(None)
        });
        let sysfs_root = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "bitty-sensor-bridge-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp sysfs");
            if let Some(capacity) = battery_capacity {
                let bat = dir.join("BAT0");
                std::fs::create_dir_all(&bat).expect("bat dir");
                std::fs::write(bat.join("type"), "Battery\n").expect("type");
                std::fs::write(bat.join("capacity"), format!("{capacity}\n")).expect("capacity");
                std::fs::write(bat.join("status"), "Discharging\n").expect("status");
                std::fs::write(bat.join("power_now"), "5000000\n").expect("power");
            }
            dir
        };
        let sensor = Rc::new(RefCell::new(SensorHost::for_test(
            cpu_source,
            mem_source,
            sysfs_root.clone(),
            now_ms,
        )));
        host.set_sensor_host(Some(sensor));
        (directory, host, cpu_calls, mem_calls, sysfs_root)
    }

    #[test]
    fn sensors_granted_resolve_and_call_serve_live_tables() {
        use crate::plugin_runtime::system_sensors::{
            SENSOR_PROVIDER, SYSTEM_BATTERY_IFACE, SYSTEM_CPU_IFACE,
        };
        use crate::plugin_runtime::system_sensors::{SYSTEM_MEMORY_IFACE, host::SENSOR_GENERATION};
        const CPU_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0  50 0 50 400 0 0 0 0 0 0\n";
        const CPU_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\ncpu0  100 0 100 400 0 0 0 0 0 0\n";
        const MEMINFO: &str = "MemTotal:        2097152 kB\nMemAvailable:    1048576 kB\n";
        let (_, host, _, _, root) = sensor_host_with_fixtures(
            &[SENSOR_CPU_HEAD, SENSOR_MEMORY_HEAD, SENSOR_BATTERY_HEAD],
            vec![Some(CPU_A.to_string()), Some(CPU_B.to_string())],
            vec![Some(MEMINFO.to_string())],
            Some("63"),
            0,
        );
        for iface in [SYSTEM_CPU_IFACE, SYSTEM_MEMORY_IFACE, SYSTEM_BATTERY_IFACE] {
            let route = HostServices::service_resolve(&host, iface, None, false)
                .expect("granted resolves")
                .expect("route");
            assert_eq!(route.provider, SENSOR_PROVIDER);
            assert_eq!(route.generation, SENSOR_GENERATION);
            assert_eq!(route.methods, vec!["get".to_string()]);
        }
        // Memory serves immediately (single-shot); battery serves the sysfs
        // fallback live; cpu hides on its first poll (baseline, never zero).
        let memory = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_MEMORY_IFACE,
            "1.0.0",
            "get",
            &LuaValue::Nil,
        )
        .expect("memory serves");
        assert!(matches!(memory, LuaValue::Table(_)));
        let battery = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_BATTERY_IFACE,
            "1.0.0",
            "get",
            &LuaValue::Nil,
        )
        .expect("battery serves");
        match &battery {
            LuaValue::Table(pairs) => {
                let state = pairs
                    .iter()
                    .find_map(|(k, v)| match (k, v) {
                        (LuaValue::String(key), LuaValue::String(value)) if key == "state" => {
                            Some(value.clone())
                        }
                        _ => None,
                    })
                    .expect("battery carries state");
                assert_eq!(state, "discharging");
            }
            other => panic!("battery serves a table, got {other:?}"),
        }
        let cpu_first = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_CPU_IFACE,
            "1.0.0",
            "get",
            &LuaValue::Nil,
        )
        .expect("cpu first hides as nil");
        assert_eq!(cpu_first, LuaValue::Nil);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sensors_ungranted_consume_matches_absent() {
        use crate::plugin_runtime::system_sensors::{SENSOR_PROVIDER, host::SENSOR_GENERATION};
        use crate::plugin_runtime::system_sensors::{
            SYSTEM_BATTERY_IFACE, SYSTEM_CPU_IFACE, SYSTEM_MEMORY_IFACE,
        };
        let (_, host, cpu_calls, mem_calls, root) = sensor_host_with_fixtures(
            &[],
            vec![Some("cpu  1 2 3 4\n".to_string())],
            vec![Some(
                "MemTotal: 1024 kB\nMemAvailable: 512 kB\n".to_string(),
            )],
            Some("50"),
            0,
        );
        for iface in [SYSTEM_CPU_IFACE, SYSTEM_MEMORY_IFACE, SYSTEM_BATTERY_IFACE] {
            let error = HostServices::service_resolve(&host, iface, None, false)
                .expect_err("ungranted must fail closed");
            assert_eq!(error.code, "E_SERVICE_RESOLUTION");
            assert_eq!(
                HostServices::service_resolve(&host, iface, None, true)
                    .expect("optional degrades to nil"),
                None,
                "ungranted resolves nil like an absent service"
            );
            let error = HostServices::service_call(
                &host,
                SENSOR_PROVIDER,
                SENSOR_GENERATION,
                iface,
                "1.0.0",
                "get",
                &LuaValue::Nil,
            )
            .expect_err("ungranted call must fail closed as gone");
            assert_eq!(error.code, "E_SERVICE_GONE");
        }
        assert_eq!(cpu_calls.get(), 0, "ungranted never touches cpu source");
        assert_eq!(mem_calls.get(), 0, "ungranted never touches memory source");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sensors_burst_coalesces_and_stale_hides_as_nil() {
        use crate::plugin_runtime::system_sensors::{
            SENSOR_PROVIDER, SYSTEM_MEMORY_IFACE, host::SENSOR_GENERATION,
        };
        const MEMINFO: &str = "MemTotal:        2097152 kB\nMemAvailable:    1048576 kB\n";
        let (_, host, _, mem_calls, root) = sensor_host_with_fixtures(
            &[SENSOR_MEMORY_HEAD],
            Vec::new(),
            vec![Some(MEMINFO.to_string())],
            None,
            0,
        );
        let first = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_MEMORY_IFACE,
            "1.0.0",
            "get",
            &LuaValue::Nil,
        )
        .expect("serves");
        assert!(matches!(first, LuaValue::Table(_)));
        assert_eq!(mem_calls.get(), 1);
        // Burst inside the 2 s ceiling serves cached without re-reading.
        for _ in 0..10 {
            let cached = HostServices::service_call(
                &host,
                SENSOR_PROVIDER,
                SENSOR_GENERATION,
                SYSTEM_MEMORY_IFACE,
                "1.0.0",
                "get",
                &LuaValue::Nil,
            )
            .expect("burst serves cached");
            assert!(matches!(cached, LuaValue::Table(_)));
        }
        assert_eq!(mem_calls.get(), 1, "burst coalesces onto one attempt");
        // Drive the clock past the three-window horizon: the source now
        // fails every window (fixture exhausted to None), so last-good
        // serves stale once, then hides as nil (never zeros).
        {
            let sensor = host.sensor_host.borrow().clone().expect("wired");
            sensor.borrow_mut().set_manual_now_ms(Some(2000));
        }
        let stale = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_MEMORY_IFACE,
            "1.0.0",
            "get",
            &LuaValue::Nil,
        )
        .expect("stale serves");
        match &stale {
            LuaValue::Table(pairs) => {
                let age = pairs
                    .iter()
                    .find_map(|(k, v)| match (k, v) {
                        (LuaValue::String(key), LuaValue::Integer(age))
                            if key == "stale_age_ms" =>
                        {
                            Some(*age)
                        }
                        _ => None,
                    })
                    .expect("stale carries age");
                assert_eq!(age, 2000);
            }
            other => panic!("stale serves a table, got {other:?}"),
        }
        {
            let sensor = host.sensor_host.borrow().clone().expect("wired");
            sensor.borrow_mut().set_manual_now_ms(Some(6001));
        }
        // Advance once more past the window so the gate re-attempts (and
        // fails) before hiding.
        {
            let sensor = host.sensor_host.borrow().clone().expect("wired");
            sensor.borrow_mut().set_manual_now_ms(Some(8000));
        }
        let hidden = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_MEMORY_IFACE,
            "1.0.0",
            "get",
            &LuaValue::Nil,
        )
        .expect("hide surfaces as nil");
        assert_eq!(hidden, LuaValue::Nil);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sensors_carry_no_policy_strings_and_record_calls() {
        use crate::plugin_runtime::system_sensors::{SENSOR_PROVIDER, SYSTEM_BATTERY_IFACE};
        use crate::plugin_runtime::system_sensors::{SYSTEM_MEMORY_IFACE, host::SENSOR_GENERATION};
        const MEMINFO: &str = "MemTotal:        2097152 kB\nMemAvailable:    1048576 kB\n";
        let (_, host, _, _, root) = sensor_host_with_fixtures(
            &[SENSOR_MEMORY_HEAD, SENSOR_BATTERY_HEAD],
            Vec::new(),
            vec![Some(MEMINFO.to_string())],
            Some("80"),
            0,
        );
        let ledger = Rc::new(RefCell::new(ServiceCallLedger::new(8)));
        host.set_service_call_ledger(ledger.clone());
        for iface in [SYSTEM_MEMORY_IFACE, SYSTEM_BATTERY_IFACE] {
            let value = HostServices::service_call(
                &host,
                SENSOR_PROVIDER,
                SENSOR_GENERATION,
                iface,
                "1.0.0",
                "get",
                &LuaValue::Nil,
            )
            .expect("serves");
            let rendered = format!("{value:?}").to_lowercase();
            for forbidden in [
                "warning",
                "critical",
                "threshold",
                "alert",
                "icon",
                "glyph",
                "bucket",
            ] {
                assert!(
                    !rendered.contains(forbidden),
                    "sensor {iface} must not carry '{forbidden}': {rendered}"
                );
            }
        }
        let calls = ledger.borrow().calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(call.provider, SENSOR_PROVIDER);
            assert!(call.method == "get");
        }
        let heads: Vec<Vec<String>> = calls.iter().map(|call| call.heads.clone()).collect();
        assert!(
            heads
                .iter()
                .any(|heads| heads.contains(&SENSOR_MEMORY_HEAD.to_string()))
        );
        assert!(
            heads
                .iter()
                .any(|heads| heads.contains(&SENSOR_BATTERY_HEAD.to_string()))
        );
        // Read-only by construction: no control method is published, so any
        // control spelling fails closed as gone (never smuggled).
        let error = HostServices::service_call(
            &host,
            SENSOR_PROVIDER,
            SENSOR_GENERATION,
            SYSTEM_MEMORY_IFACE,
            "1.0.0",
            "set",
            &LuaValue::Nil,
        )
        .expect_err("control spelling must fail closed");
        assert_eq!(error.code, "E_SERVICE_GONE");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn narrowing_grant_snapshot_to_empty_fails_closed_without_restart() {
        // A generation's granted-capability snapshot narrowed to empty
        // makes the next lookup and call fail closed on the same live
        // host: only the in-memory snapshot gates, no restart is needed.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        assert!(
            HostServices::service_resolve(&host, "calc.add", None, false)
                .expect("granted resolves")
                .is_some()
        );
        // Narrow the snapshot to empty on the same live host.
        host.set_granted_capabilities(Vec::new());
        let error = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect_err("narrowed consume must fail closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert_eq!(error.class, "runtime");
        assert_eq!(
            HostServices::service_resolve(&host, "calc.add", None, true)
                .expect("optional degrades to nil"),
            None
        );
        // A call against the still-published record fails closed as gone:
        // the denial is indistinguishable from the provider being gone.
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("narrowed call must fail closed");
        assert_eq!(error.code, "E_SERVICE_GONE");
        // Re-widening the snapshot serves again with no restart: the
        // directory records never moved, only the in-memory snapshot gates.
        host.set_granted_capabilities(vec![TEST_HEAD.to_string()]);
        assert!(
            HostServices::service_resolve(&host, "calc.add", None, false)
                .expect("re-widened resolves")
                .is_some()
        );
    }

    #[test]
    fn service_call_records_provider_owner_and_backing_heads() {
        let (directory, host) = service_host();
        let ledger = Rc::new(RefCell::new(ServiceCallLedger::new(
            SERVICE_CALL_LEDGER_CAPACITY,
        )));
        host.set_service_call_ledger(ledger.clone());
        publish_backed_record(
            &directory,
            "xuepoo.calc",
            7,
            "calc.add",
            "1.0.0",
            &["add"],
            &[TEST_HEAD],
        );
        // No live VM behind the stashed record (Weak::new), so the call
        // itself fails closed — but the broker records the dispatch first.
        let _ = HostServices::service_call(
            &host,
            "xuepoo.calc",
            7,
            "calc.add",
            "1.0.0",
            "add",
            &LuaValue::Nil,
        );
        let calls = ledger.borrow().calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].provider, "xuepoo.calc");
        assert_eq!(calls[0].iface, "calc.add");
        assert_eq!(calls[0].version, "1.0.0");
        assert_eq!(calls[0].method, "add");
        assert_eq!(calls[0].heads, vec![TEST_HEAD.to_string()]);
        // Ungranted calls record nothing: the gate runs before the ledger.
        host.set_granted_capabilities(Vec::new());
        let _ = HostServices::service_call(
            &host,
            "xuepoo.calc",
            7,
            "calc.add",
            "1.0.0",
            "add",
            &LuaValue::Nil,
        );
        assert_eq!(ledger.borrow().len(), 1);
    }

    #[test]
    fn service_call_ledger_drops_newest_past_capacity() {
        let mut ledger = ServiceCallLedger::new(2);
        for method in ["a", "b"] {
            assert!(ledger.push(ServiceCallRecord {
                provider: "xuepoo.calc".to_string(),
                iface: "calc.add".to_string(),
                version: "1.0.0".to_string(),
                method: method.to_string(),
                heads: vec![TEST_HEAD.to_string()],
            }));
        }
        assert!(!ledger.push(ServiceCallRecord {
            provider: "xuepoo.calc".to_string(),
            iface: "calc.add".to_string(),
            version: "1.0.0".to_string(),
            method: "c".to_string(),
            heads: vec![TEST_HEAD.to_string()],
        }));
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.dropped(), 1);
        assert_eq!(ledger.calls()[1].method, "b");
    }

    #[test]
    fn provide_check_without_directory_is_not_implemented() {
        let host = services();
        host.set_service_manifest(
            vec![ProvidedService {
                iface: "calc.add".to_string(),
                version: "1.0.0".to_string(),
                args_schema: None,
                result_schema: None,
                heads: vec![TEST_HEAD.to_string()],
            }],
            Vec::new(),
        );
        let error = host
            .service_provide_check("calc.add")
            .expect_err("no backend must stay not-implemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn resolve_without_directory_is_not_implemented() {
        let host = services();
        let error = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect_err("no backend must stay not-implemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        assert_eq!(error.class, "runtime");
    }

    #[test]
    fn resolve_ambiguous_short_fails_closed_naming_owners() {
        // Dynapi arbitration (bitty#2003): one short name claimed by two
        // owners resolves through neither — the unqualified form fails
        // closed naming both owners, while each qualified form keeps
        // working. Ambiguity is a naming error, not absence, so `optional`
        // still fails.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.beta", 1, "calc.add", "1.5.0", &["add"]);
        publish_record(&directory, "xuepoo.alpha", 1, "calc.add", "1.2.0", &["add"]);
        for optional in [false, true] {
            let error = HostServices::service_resolve(&host, "calc.add", None, optional)
                .expect_err("ambiguous short must fail closed");
            assert_eq!(error.code, "E_SERVICE_RESOLUTION", "optional={optional}");
            assert_eq!(error.class, "runtime");
            assert!(
                error.message.contains("'xuepoo.alpha'"),
                "names first owner: {}",
                error.message
            );
            assert!(
                error.message.contains("'xuepoo.beta'"),
                "names second owner: {}",
                error.message
            );
            assert!(
                error.message.contains("owner-qualified"),
                "points at the qualified form: {}",
                error.message
            );
        }
        host.set_service_manifest(
            host.provided_services(),
            vec![
                ("calc.add".to_string(), ">=1.0".to_string()),
                ("xuepoo.alpha:calc.add".to_string(), ">=1.0".to_string()),
                ("xuepoo.beta:calc.add".to_string(), ">=1.0".to_string()),
            ],
        );
        let route = HostServices::service_resolve(&host, "xuepoo.alpha:calc.add", None, false)
            .expect("qualified alpha resolves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.alpha");
        assert_eq!(route.version, "1.2.0");
        let route = HostServices::service_resolve(&host, "xuepoo.beta:calc.add", None, false)
            .expect("qualified beta resolves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.beta");
        assert_eq!(route.version, "1.5.0");
    }

    #[test]
    fn resolve_qualified_ignores_other_owners_versions() {
        // A qualified lookup pins the owner even when another owner
        // publishes a higher version: negotiation never crosses owners.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.alpha", 1, "calc.add", "1.2.0", &["add"]);
        publish_record(&directory, "xuepoo.beta", 1, "calc.add", "2.0.0", &["add"]);
        host.set_service_manifest(
            host.provided_services(),
            vec![("xuepoo.alpha:calc.add".to_string(), ">=1.0".to_string())],
        );
        let route = HostServices::service_resolve(&host, "xuepoo.alpha:calc.add", None, false)
            .expect("qualified alpha resolves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.alpha");
        assert_eq!(route.version, "1.2.0");
    }

    #[test]
    fn resolve_single_owner_short_still_resolves() {
        // Backward-compatible spine: one owner per short name keeps the
        // historic behavior (highest satisfying version wins).
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect("single-owner short resolves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.calc");
        assert_eq!(route.version, "1.2.0");
    }

    #[test]
    fn resolve_opts_version_overrides_manifest_req() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", Some(">=1.2,<1.5"), false)
            .expect("resolution serves")
            .expect("route present");
        assert_eq!(route.version, "1.2.0");
    }

    #[test]
    fn provide_check_rejects_qualified_and_reserved_spellings() {
        // Dynapi arbitration (bitty#2003): provide-side names the short
        // interface only, never a Core-reserved spelling. Both fail closed
        // with the undeclared code and a naming diagnostic.
        let (_, host) = service_host();
        let error = host
            .service_provide_check("xuepoo.calc:calc.add")
            .expect_err("qualified provision must fail");
        assert_eq!(error.code, "E_SERVICE_UNDECLARED");
        assert!(
            error.message.contains("calc.add"),
            "names the short form: {}",
            error.message
        );
        let error = host
            .service_provide_check("bitty.echo")
            .expect_err("Core-reserved provision must fail");
        assert_eq!(error.code, "E_SERVICE_UNDECLARED");
        assert!(
            error.message.contains("bitty.echo"),
            "names the iface: {}",
            error.message
        );
        assert!(
            error.message.contains("Core-reserved"),
            "names the reservation: {}",
            error.message
        );
    }

    #[test]
    fn resolve_prefers_highest_compatible_across_side_by_side() {
        // Dynapi negotiation (bitty#2006): side-by-side versions of one
        // interface resolve to the highest compatible version — never a
        // silent downgrade to a lower pick.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "2.0.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect("resolution serves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.calc");
        assert_eq!(route.version, "2.0.0");
    }

    #[test]
    fn resolve_pinned_version_serves_pinned_release() {
        // Consumers pin independently: an exact pin serves its release even
        // when a higher sibling coexists.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "2.0.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", Some("1.0.0"), false)
            .expect("pinned resolution serves")
            .expect("route present");
        assert_eq!(route.version, "1.0.0");
    }

    #[test]
    fn resolve_empty_intersection_fails_closed_listing_available() {
        // An empty intersection fails closed with a diagnostic naming the
        // interface, the requested range, and the visible versions — never
        // a silent pick of nearest.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "2.0.0", &["add"]);
        let error = HostServices::service_resolve(&host, "calc.add", Some(">=9.0"), false)
            .expect_err("empty intersection must fail closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        let message = error.message;
        assert!(message.contains("calc.add"), "names the iface: {message}");
        assert!(message.contains(">=9.0"), "names the range: {message}");
        assert!(message.contains("1.0.0"), "lists v1: {message}");
        assert!(message.contains("2.0.0"), "lists v2: {message}");
        assert_eq!(
            HostServices::service_resolve(&host, "calc.add", Some(">=9.0"), true)
                .expect("optional degrades to nil"),
            None
        );
    }

    #[test]
    fn resolve_removing_only_compatible_version_flips_to_fail_closed() {
        // No silent downgrade path: when the only compatible version
        // leaves, consumption flips from OK to fail-closed instead of
        // rebinding to an incompatible sibling.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.old", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.new", 1, "calc.add", "2.0.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", Some(">=2.0"), false)
            .expect("v2 resolves")
            .expect("route present");
        assert_eq!(route.version, "2.0.0");
        directory.borrow_mut().revoke_provider("xuepoo.new");
        let error = HostServices::service_resolve(&host, "calc.add", Some(">=2.0"), false)
            .expect_err("removal must flip to fail-closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert!(
            error.message.contains("1.0.0"),
            "diagnostic lists the remaining version: {}",
            error.message
        );
    }

    #[test]
    fn service_call_version_pin_drives_exact_record() {
        // Side-by-side records never serve through each other's handles:
        // the call resolves the exact (provider, version) record. Both
        // records share a dead VM here, so the pin is observed through
        // the failure message: the pinned version reaches its (dead)
        // provider, an unknown version finds no record at all.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "2.0.0", &["add"]);
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.0.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("dead VM must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
        assert!(
            error.message.contains("is gone"),
            "pinned version reaches its record: {}",
            error.message
        );
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "9.9.9",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("unknown version must be unavailable");
        assert_eq!(error.code, "E_SERVICE_GONE");
        assert!(
            error.message.contains("is unavailable"),
            "unknown version finds no record: {}",
            error.message
        );
    }

    #[test]
    fn resolve_undeclared_iface_is_resolution_error() {
        let (directory, host) = service_host();
        publish_record(
            &directory,
            "xuepoo.other",
            1,
            "calc.other",
            "1.0.0",
            &["run"],
        );
        // Even with an explicit version override, consumption without a
        // manifest `services.required` entry fails closed.
        for optional in [false, true] {
            let error = HostServices::service_resolve(&host, "calc.other", Some("^1.0"), optional)
                .expect_err("undeclared consume must fail");
            assert_eq!(error.code, "E_SERVICE_RESOLUTION", "optional={optional}");
            assert_eq!(error.class, "runtime");
        }
    }

    #[test]
    fn resolve_failure_and_optional_nil() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.old", 1, "calc.add", "1.2.0", &["add"]);
        let error = HostServices::service_resolve(&host, "calc.add", Some(">=9.0"), false)
            .expect_err("unsatisfiable requirement must fail");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert_eq!(
            HostServices::service_resolve(&host, "calc.add", Some(">=9.0"), true)
                .expect("optional degrades to nil"),
            None
        );
        // No provider at all degrades the same way.
        directory.borrow_mut().revoke_provider("xuepoo.old");
        assert_eq!(
            HostServices::service_resolve(&host, "calc.add", None, true)
                .expect("optional degrades to nil"),
            None
        );
    }

    #[test]
    fn resolve_failure_listing_caps_versions_and_hides_ungranted() {
        // The available-versions listing stays bounded (first entries plus
        // the remaining count) and never names an ungranted provider's
        // versions: revoking the grant flips the listing to empty, the
        // same posture as no provider.
        let (directory, host) = service_host();
        for minor in 0..10 {
            publish_record(
                &directory,
                "xuepoo.calc",
                1,
                "calc.add",
                &format!("1.0.{minor}"),
                &["add"],
            );
        }
        let error = HostServices::service_resolve(&host, "calc.add", Some(">=9.0"), false)
            .expect_err("empty intersection must fail closed");
        assert!(
            error.message.contains("1.0.0"),
            "lists first versions: {}",
            error.message
        );
        assert!(
            error.message.contains("(and 2 more)"),
            "caps the listing with the remainder: {}",
            error.message
        );
        // Ungranted records stay invisible to the diagnostic.
        host.set_granted_capabilities(Vec::new());
        let error = HostServices::service_resolve(&host, "calc.add", Some(">=9.0"), false)
            .expect_err("ungranted consume must fail closed");
        assert!(
            error.message.contains("no versions available"),
            "hides ungranted versions: {}",
            error.message
        );
    }

    #[test]
    fn resolve_skips_suspended_records() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        directory.borrow_mut().suspend_provider("xuepoo.calc");
        let error = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect_err("suspended provider must not resolve");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        directory.borrow_mut().resume_provider("xuepoo.calc");
        assert!(
            HostServices::service_resolve(&host, "calc.add", None, false)
                .expect("resumed resolves")
                .is_some()
        );
    }

    #[test]
    fn directory_publish_coexists_side_by_side_replaces_same_version() {
        // Dynapi negotiation (bitty#2006): side-by-side versions of one
        // interface coexist; republishing the same (provider, version)
        // replaces in place.
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.1.0", &["add"]);
        assert_eq!(directory.borrow().published_count(), 2);
        let old = directory
            .borrow()
            .find_active("calc.add", "xuepoo.calc", "1.0.0")
            .expect("v1 present");
        assert_eq!(old.generation, 1);
        // Same (provider, version) republishes in place.
        publish_record(&directory, "xuepoo.calc", 2, "calc.add", "1.0.0", &["add"]);
        assert_eq!(directory.borrow().published_count(), 2);
        let current = directory
            .borrow()
            .find_active("calc.add", "xuepoo.calc", "1.0.0")
            .expect("republished record present");
        assert_eq!(current.generation, 2);
        assert_eq!(current.version, "1.0.0");
        // Version-pinned lookup finds no sibling: unknown versions resolve
        // to nothing.
        assert!(
            directory
                .borrow()
                .find_active("calc.add", "xuepoo.calc", "9.9.9")
                .is_none()
        );
    }

    #[test]
    fn call_without_directory_is_not_implemented() {
        let host = services();
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("no backend must stay not-implemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn call_unknown_iface_is_gone() {
        let (_, host) = service_host();
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.missing",
            "1.0.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("unpublished iface must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
        assert_eq!(error.class, "runtime");
    }

    #[test]
    fn call_stale_generation_is_gone() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 2, "calc.add", "1.2.0", &["add"]);
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("stale generation must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_unknown_method_is_gone() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "sub",
            &LuaValue::Nil,
        )
        .expect_err("unpublished method must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_suspended_provider_is_gone() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        directory.borrow_mut().suspend_provider("xuepoo.calc");
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("suspended provider must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_dead_provider_is_gone() {
        // `Weak::new` records never upgrade: models a disposed generation
        // whose VM is dropped.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "add",
            &LuaValue::Nil,
        )
        .expect_err("dead provider VM must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_args_schema_violation_is_invalid_before_execution() {
        let (directory, host) = service_host();
        directory.borrow_mut().publish(ServiceRecord {
            provider: "xuepoo.calc".to_string(),
            generation: 1,
            iface: "calc.add".to_string(),
            version: "1.2.0".to_string(),
            methods: vec!["add".to_string()],
            funcs: BTreeMap::new(),
            args_schema: Some(
                "{\"type\": \"object\", \"properties\": {\"a\": {\"type\": \"integer\"}}, \"required\": [\"a\"]}"
                    .to_string(),
            ),
            result_schema: None,
            heads: vec![TEST_HEAD.to_string()],
            vm: Weak::new(),
            suspended: false,
        });
        // A bare integer is not the declared object: rejected before any
        // provider code could run (the VM here is dead, proving no call
        // was attempted — a call would report `E_SERVICE_GONE` instead).
        let error = HostServices::service_call(
            &host,
            "xuepoo.calc",
            1,
            "calc.add",
            "1.2.0",
            "add",
            &LuaValue::Integer(5),
        )
        .expect_err("schema violation must be invalid");
        assert_eq!(error.code, "E_SERVICE_INVALID");
        assert_eq!(error.class, "validation");
    }

    #[test]
    fn truncate_service_message_caps_at_limit() {
        let long = "e".repeat(SERVICE_FAILED_MESSAGE_LIMIT + 40);
        let capped = truncate_service_message(long);
        assert_eq!(capped.chars().count(), SERVICE_FAILED_MESSAGE_LIMIT);
        let short = "boom".to_string();
        assert_eq!(truncate_service_message(short.clone()), short);
    }

    #[test]
    fn in_memory_store_writes_earn_no_rc1_credit() {
        // bitty #1518 review: only durable commit I/O of a disk-backed store
        // is credited. A near-quota in-memory store makes every `store.set`
        // pay for clone, quota check, and JSON encoding of the whole store;
        // that CPU is plugin-driven and must stay charged, so a write loop
        // followed by slice-crossing work ends in `WallClockExceeded` with
        // zero credit.
        use bitty_lua::gate::{VmBudgets, build_plugin_vm};
        use bitty_lua::{BoundedExecution, MarshallingLimits, RC1_WALL_CLOCK_BUDGET_MS};

        /// Filler entries that bring the store close to its byte quota.
        const FILL_ENTRIES: usize = 7;
        let value_bytes = store::STORE_MAX_VALUE_BYTES - 64;
        let services = Rc::new(services());
        for index in 0..FILL_ENTRIES {
            services
                .store_set(
                    &format!("fill{index}"),
                    LuaValue::String("x".repeat(value_bytes)),
                )
                .expect("fill stays within quota");
        }
        let mut vm = build_plugin_vm("xuepoo.test", Some(VmBudgets::default())).expect("vm");
        let host: Rc<dyn HostServices> = services.clone();
        vm.install_host_module(host, MarshallingLimits::default(), RC1_WALL_CLOCK_BUDGET_MS)
            .expect("install");
        let outcome = vm
            .execute_bounded(
                r#"
                local big = string.rep("y", 4096)
                for i = 1, 100000 do bitty.store.set("hot", big) end
                local n = 0
                for i = 1, 4096 do n = n + i end
                bitty.store.set("after", true)
                "#,
            )
            .expect("execute");
        assert!(
            matches!(
                outcome,
                BoundedExecution::Suspended(bitty_lua::SuspendReason::WallClockExceeded { .. })
            ),
            "{outcome:?}"
        );
        assert_eq!(vm.budget_snapshot().store_commit_credit_ms, 0);
        assert!(services.with_store(|store| store.get("after")).is_none());
    }

    /// CTX-0942: targeting services with the accepted `ui.overlay` grant and,
    /// optionally, the runtime-shared mechanism state wired. Uses only
    /// existing `bitty-ui` types; no new targeting type is introduced.
    #[allow(clippy::type_complexity)]
    fn targeting_services(overlay: bool, wire_state: bool) -> PluginServices {
        let services = ui_services(UiAccess {
            rich: true,
            overlay,
            overlay_focus: overlay,
            claims: Vec::new(),
        });
        if wire_state {
            services.set_targeting_state(
                Rc::new(RefCell::new(TargetRegistry::new())),
                Rc::new(RefCell::new(Vec::new())),
                Rc::new(RefCell::new(LabelAllocator::default())),
                Rc::new(RefCell::new(None::<Rc<HintAnnotationLayer>>)),
            );
            services.set_overlay_capture(Rc::new(RefCell::new(OverlayCapture::new())));
        }
        services
    }

    fn targeting_offers(n: u64) -> Vec<(String, u64)> {
        (1..=n).map(|id| ("link".to_string(), id)).collect()
    }

    fn targeting_anchors(n: u16) -> Vec<(u16, u16)> {
        (0..n).map(|i| (10 + i, 0)).collect()
    }

    fn targeting_commands(n: u64) -> Vec<String> {
        (0..n).map(|i| format!("acme.cmd:c{i}")).collect()
    }

    /// Fixed live Core target source for anchor/presentation tests.
    #[derive(Debug)]
    struct FixedTargets(Vec<CoreTargetInfo>);

    impl TargetSource for FixedTargets {
        fn targets(&self) -> Result<Vec<CoreTargetInfo>, BridgeError> {
            Ok(self.0.clone())
        }
    }

    /// Mutable live Core target source for reconciliation tests.
    #[derive(Clone, Debug)]
    struct MutableTargets(Rc<RefCell<Vec<CoreTargetInfo>>>);

    impl TargetSource for MutableTargets {
        fn targets(&self) -> Result<Vec<CoreTargetInfo>, BridgeError> {
            Ok(self.0.borrow().clone())
        }
    }

    fn published_layer(services: &PluginServices) -> Rc<HintAnnotationLayer> {
        services
            .target_annotations
            .borrow()
            .clone()
            .expect("annotation channel")
            .borrow()
            .clone()
            .expect("published layer")
    }

    #[test]
    fn targeting_surface_denies_without_overlay_grant() {
        // Deny-by-default: with no `ui.overlay` grant every entry point fails
        // closed before any side effect. No new capability identifier exists.
        let services = targeting_services(false, true);
        for code in [
            services.ui_targets_snapshot(4).expect_err("snapshot").code,
            services
                .ui_targets_register("acme.links", "plugin", &targeting_offers(1))
                .expect_err("register")
                .code,
            services
                .ui_targets_unregister("acme.links")
                .expect_err("unregister")
                .code,
            services
                .ui_labels_set_policy("asdfghjkl", "hjkl")
                .expect_err("policy")
                .code,
            services
                .ui_labels_assign(&targeting_anchors(1), 80)
                .expect_err("assign")
                .code,
            services
                .ui_targets_session_start(11, 80, &targeting_anchors(1), &targeting_commands(1))
                .expect_err("start")
                .code,
            services
                .ui_targets_dispatch("a")
                .expect_err("dispatch")
                .code,
        ] {
            assert_eq!(code, "E_CAPABILITY_DENIED");
        }
        // Cancel is never capability-gated (it must free input even after a
        // grant revoke), so it fails here only on the missing capture handle
        // path via the overlay release, never on a grant check.
        let _ = services.ui_targets_session_cancel(11);
    }

    #[test]
    fn targeting_surface_fails_closed_without_wired_state() {
        // A granted generation on a host without a targeting backend has no
        // ambient surface: every call is a typed `E_UI_UNAVAILABLE`.
        let services = targeting_services(true, false);
        assert_eq!(
            services.ui_targets_snapshot(4).expect_err("snapshot").code,
            E_UI_UNAVAILABLE
        );
        assert_eq!(
            services
                .ui_targets_dispatch("a")
                .expect_err("dispatch")
                .code,
            E_UI_NOT_OWNER
        );
    }

    #[test]
    fn targeting_core_source_publishes_anchored_annotations() {
        let services = targeting_services(true, true);
        services.set_target_source(Some(Rc::new(FixedTargets(vec![
            CoreTargetInfo {
                kind: "workspace".to_string(),
                id: 7,
                workspace: Some(7),
                rect: Some((0, 0, 80, 24)),
            },
            CoreTargetInfo {
                kind: "panel".to_string(),
                id: 11,
                workspace: Some(7),
                rect: Some((1, 2, 20, 10)),
            },
        ]))));
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        let rows = services.ui_targets_snapshot(16).expect("snapshot");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "workspace");
        assert_eq!(rows[0].id, 7);
        assert_eq!(rows[0].workspace, Some(7));
        assert_eq!(rows[0].rect, Some((0, 0, 80, 24)));
        assert_eq!(rows[1].kind, "panel");
        assert_eq!(rows[1].id, 11);
        assert_eq!(rows[1].workspace, Some(7));
        assert_eq!(rows[1].rect, Some((1, 2, 20, 10)));

        let labels = services
            .ui_targets_session_start(
                handle,
                80,
                &[(0, 0), (0, 0)],
                &[
                    "acme.cmd:workspace".to_string(),
                    "acme.cmd:panel".to_string(),
                ],
            )
            .expect("start");
        let layer = services
            .target_annotations
            .borrow()
            .clone()
            .expect("annotation channel")
            .borrow()
            .clone()
            .expect("published layer");
        assert_eq!(layer.len(), 2);
        let panel = layer
            .annotation_for_label(&labels[1])
            .expect("panel annotation");
        assert_eq!(panel.kind, bitty_ui::TargetKind::Panel);
        assert_eq!(panel.workspace, Some(WorkspaceId::new(7)));
        assert_eq!(panel.rect, Rect::new(1, 2, 20, 10));
        // The viewport covers the full anchored target, not just the label
        // anchor row, so pointer hits below the label still resolve.
        assert!(layer.viewport().height >= 12);
        assert!(layer.viewport().contains_point(Point::new(5, 5)));
        // The label anchor follows the live Core rectangle, not the
        // caller-supplied (0, 0) placeholder.
        assert_eq!(panel.anchor, Point::new(1, 2));
        assert_eq!(
            layer
                .hit_test(Point::new(5, 5))
                .map(|annotation| annotation.label.clone()),
            Some(labels[1].clone())
        );
    }

    #[test]
    fn targeting_reconcile_drops_closed_panel_and_revokes_label() {
        let services = targeting_services(true, true);
        let rows = Rc::new(RefCell::new(vec![
            CoreTargetInfo {
                kind: "panel".to_string(),
                id: 11,
                workspace: Some(7),
                rect: Some((1, 2, 20, 10)),
            },
            CoreTargetInfo {
                kind: "panel".to_string(),
                id: 12,
                workspace: Some(7),
                rect: Some((30, 2, 20, 10)),
            },
        ]));
        services.set_target_source(Some(Rc::new(MutableTargets(Rc::clone(&rows)))));
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        let labels = services
            .ui_targets_session_start(
                handle,
                80,
                &[(0, 0), (0, 0)],
                &["acme.cmd:p1".to_string(), "acme.cmd:p2".to_string()],
            )
            .expect("start");
        assert!(published_layer(&services).contains_label(&labels[0]));
        assert!(services.target_dispatcher.borrow().is_bound(&labels[0]));

        // A panel closes mid-session: the next cold tick publishes only the
        // surviving row and reconciles the session against it.
        rows.borrow_mut().remove(0);
        assert!(services.reconcile_target_session().expect("reconcile"));

        let layer = published_layer(&services);
        assert!(
            !layer.contains_label(&labels[0]),
            "closed panel label remains"
        );
        assert!(layer.contains_label(&labels[1]));
        assert!(!services.target_dispatcher.borrow().is_bound(&labels[0]));
        assert_eq!(
            services
                .ui_targets_dispatch(&labels[0])
                .expect_err("closed panel label must fail closed")
                .code,
            "E_DEF_INVALID"
        );
        // A second pass is a no-op.
        assert!(
            !services
                .reconcile_target_session()
                .expect("second reconcile")
        );
    }

    #[test]
    fn targeting_presented_annotation_drop_revokes_label() {
        let services = targeting_services(true, true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(2))
            .expect("register");
        let labels = services
            .ui_targets_session_start(handle, 80, &targeting_anchors(2), &targeting_commands(2))
            .expect("start");
        let shared = services
            .target_annotations
            .borrow()
            .clone()
            .expect("annotation channel");
        let dropped = labels[0].clone();
        let mut updated = (**shared.borrow().as_ref().expect("layer")).clone();
        assert!(updated.drop_annotation(&dropped));
        *shared.borrow_mut() = Some(Rc::new(updated.clone()));
        assert_eq!(
            services
                .target_dispatcher
                .borrow_mut()
                .retain_presented(&updated),
            1
        );
        assert_eq!(
            services
                .ui_targets_dispatch(&dropped)
                .expect_err("dropped label fails closed")
                .code,
            "E_DEF_INVALID"
        );
        // The sibling label still dispatches; the session remains live.
        assert!(
            services
                .ui_targets_dispatch(&labels[1])
                .expect("sibling dispatch")
                .starts_with("acme.cmd:")
        );
    }

    #[test]
    fn targeting_direct_overlay_release_ends_session() {
        let services = targeting_services(true, true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(1))
            .expect("register");
        let labels = services
            .ui_targets_session_start(handle, 80, &targeting_anchors(1), &targeting_commands(1))
            .expect("start");
        assert!(
            services
                .target_annotations
                .borrow()
                .as_ref()
                .expect("channel")
                .borrow()
                .is_some()
        );
        assert!(services.ui_overlay_release(handle).expect("release"));
        assert!(
            services
                .target_annotations
                .borrow()
                .as_ref()
                .expect("channel")
                .borrow()
                .is_none()
        );
        assert_eq!(
            services
                .ui_targets_dispatch(&labels[0])
                .expect_err("session ended")
                .code,
            E_UI_NOT_OWNER
        );
    }

    #[test]
    fn targeting_unload_clears_owned_annotations() {
        let services = targeting_services(true, true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(1))
            .expect("register");
        let labels = services
            .ui_targets_session_start(handle, 80, &targeting_anchors(1), &targeting_commands(1))
            .expect("start");
        let shared = services
            .target_annotations
            .borrow()
            .clone()
            .expect("channel");
        assert!(shared.borrow().is_some());
        assert!(services.revoke_targeting_lenses());
        assert!(
            shared.borrow().is_none(),
            "unload/suspend must clear the owned layer"
        );
        assert_eq!(
            services
                .ui_targets_dispatch(&labels[0])
                .expect_err("session ended")
                .code,
            E_UI_NOT_OWNER
        );
    }

    #[test]
    fn targeting_lazy_expiry_clears_previous_owner_session() {
        let registry = Rc::new(RefCell::new(TargetRegistry::new()));
        let lenses: Rc<RefCell<Vec<(String, DerivedProvider)>>> = Rc::new(RefCell::new(Vec::new()));
        let allocator = Rc::new(RefCell::new(LabelAllocator::default()));
        let annotations: SharedTargetAnnotations = Rc::new(RefCell::new(None));
        let capture = Rc::new(RefCell::new(OverlayCapture::new()));
        let peers: OverlayPeers = Rc::new(RefCell::new(BTreeMap::new()));
        let owner = Rc::new(generation_services("owner.plugin", 1, &capture));
        let next = Rc::new(generation_services("next.plugin", 2, &capture));
        for services in [&owner, &next] {
            services.set_targeting_state(
                Rc::clone(&registry),
                Rc::clone(&lenses),
                Rc::clone(&allocator),
                Rc::clone(&annotations),
            );
            services.set_overlay_peers(Rc::clone(&peers));
        }
        peers
            .borrow_mut()
            .entry("owner.plugin".to_string())
            .or_default()
            .push(Rc::downgrade(&owner));
        peers
            .borrow_mut()
            .entry("next.plugin".to_string())
            .or_default()
            .push(Rc::downgrade(&next));

        owner
            .ui_targets_register("owner.links", "plugin", &targeting_offers(1))
            .expect("register");
        let owner_handle = owner
            .ui_mount("overlay", &UiNode::text("owner"))
            .expect("owner overlay mount");
        let labels = owner
            .ui_targets_session_start(
                owner_handle,
                80,
                &targeting_anchors(1),
                &targeting_commands(1),
            )
            .expect("owner start");
        assert!(annotations.borrow().is_some());

        // The owner's capture expires; the next generation's session start
        // lazily finishes it. The expired layer must clear, and the stale
        // owner must not be able to clear the next publisher's layer.
        assert!(
            capture
                .borrow_mut()
                .force_expire("owner.plugin", owner_handle)
        );
        next.ui_targets_register("next.links", "plugin", &targeting_offers(1))
            .expect("next register");
        let next_handle = next
            .ui_mount("overlay", &UiNode::text("next"))
            .expect("next overlay mount");
        let next_labels = next
            .ui_targets_session_start(
                next_handle,
                80,
                &targeting_anchors(2),
                &targeting_commands(2),
            )
            .expect("next start");
        assert!(
            annotations.borrow().is_some(),
            "next session publishes its layer"
        );
        assert_eq!(
            owner
                .ui_targets_dispatch(&labels[0])
                .expect_err("expired session ended")
                .code,
            E_UI_NOT_OWNER
        );
        // A stale cancel from the expired owner cannot touch the next
        // owner's channel (session marker already cleared by expiry).
        assert!(
            !owner
                .ui_targets_session_cancel(owner_handle)
                .expect("stale cancel")
        );
        // Even an explicit stale clear is identity-gated.
        owner.clear_targeting_session();
        let current = annotations.borrow().clone().expect("next layer");
        assert!(current.contains_label(&next_labels[0]));
    }

    #[test]
    fn targeting_foreign_generation_dispose_keeps_active_annotations() {
        let registry = Rc::new(RefCell::new(TargetRegistry::new()));
        let lenses: Rc<RefCell<Vec<(String, DerivedProvider)>>> = Rc::new(RefCell::new(Vec::new()));
        let allocator = Rc::new(RefCell::new(LabelAllocator::default()));
        let annotations: SharedTargetAnnotations = Rc::new(RefCell::new(None));
        let capture = Rc::new(RefCell::new(OverlayCapture::new()));
        let owner = generation_services("owner.plugin", 1, &capture);
        let foreign = generation_services("foreign.plugin", 2, &capture);
        owner.set_targeting_state(
            Rc::clone(&registry),
            Rc::clone(&lenses),
            Rc::clone(&allocator),
            Rc::clone(&annotations),
        );
        foreign.set_targeting_state(registry, lenses, allocator, Rc::clone(&annotations));
        owner
            .ui_targets_register("owner.links", "plugin", &targeting_offers(1))
            .expect("register");
        let handle = owner
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        let labels = owner
            .ui_targets_session_start(handle, 80, &targeting_anchors(1), &targeting_commands(1))
            .expect("start");
        assert!(annotations.borrow().is_some());
        // The foreign generation has no session; its suspend/dispose cleanup
        // must not erase the owner's live annotation layer.
        assert!(!foreign.revoke_targeting_lenses());
        assert!(
            annotations.borrow().is_some(),
            "foreign lifecycle cleanup must not clear the active layer"
        );
        assert!(owner.ui_targets_dispatch(&labels[0]).is_ok());
    }

    #[test]
    fn targeting_session_lifecycle_over_overlay_capture() {
        let services = targeting_services(true, true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        services
            .ui_labels_set_policy("asdfghjkl", "hjkl")
            .expect("policy");
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(2))
            .expect("register");
        let entries = services.ui_targets_snapshot(16).expect("snapshot");
        assert_eq!(entries.len(), 2);
        let labels = services
            .ui_targets_session_start(handle, 80, &targeting_anchors(2), &targeting_commands(2))
            .expect("start");
        assert_eq!(labels.len(), 2);
        assert_eq!(
            services
                .overlay_capture()
                .expect("capture")
                .borrow()
                .owner_plugin(),
            Some("xuepoo.test"),
            "a started session owns the transient input capture"
        );
        let first = services.ui_targets_dispatch(&labels[0]).expect("dispatch");
        assert_eq!(first, "acme.cmd:c0");
        assert_eq!(
            services
                .ui_targets_dispatch("zz")
                .expect_err("single-shot session ended")
                .code,
            E_UI_NOT_OWNER
        );
        // No target or annotation internal is published: the lifecycle
        // touches no notification queue.
        assert!(
            services.notifications.borrow().is_empty(),
            "targeting must not publish notifications"
        );
        assert!(services.ui_targets_session_cancel(handle).expect("cancel"));
        assert!(
            services
                .overlay_capture()
                .expect("capture")
                .borrow()
                .owner_plugin()
                .is_none(),
            "cancel releases capture"
        );
        assert!(
            !services
                .ui_targets_session_cancel(handle)
                .expect("idempotent"),
            "cancel is idempotent: a second cancel succeeds without effect"
        );
        assert_eq!(
            services
                .ui_targets_dispatch("a")
                .expect_err("no session")
                .code,
            E_UI_NOT_OWNER
        );
    }

    #[test]
    fn targeting_start_failure_releases_capture() {
        let services = targeting_services(true, true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(2))
            .expect("register");
        // One anchor for two snapshot targets: a typed failure before binding.
        let error = services
            .ui_targets_session_start(handle, 80, &targeting_anchors(1), &targeting_commands(1))
            .expect_err("length mismatch");
        assert_eq!(error.code, "E_DEF_INVALID");
        assert!(
            services
                .overlay_capture()
                .expect("capture")
                .borrow()
                .owner_plugin()
                .is_none(),
            "a failed start must never leave input captured"
        );
    }

    #[test]
    fn targeting_snapshot_is_read_only_and_stale_dispatch_fails_closed() {
        let services = targeting_services(true, true);
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(2))
            .expect("register");
        let labels = services
            .ui_targets_session_start(handle, 80, &targeting_anchors(2), &targeting_commands(2))
            .expect("start");
        // A read-only snapshot collects into a scratch registry, so it never
        // bumps the generations the live session bound against: dispatch
        // keeps working after any number of snapshots.
        let _ = services.ui_targets_snapshot(16).expect("snapshot");
        let _ = services.ui_targets_snapshot(16).expect("snapshot");
        services
            .ui_targets_dispatch(&labels[0])
            .expect("dispatch after snapshot");
        // Ending the session drops the dispatcher bindings, so dispatch
        // fails closed with the existing `E_UI_NOT_OWNER` code (never a new
        // stale code).
        services.ui_targets_session_cancel(handle).expect("cancel");
        assert_eq!(
            services
                .ui_targets_dispatch(&labels[0])
                .expect_err("ended session")
                .code,
            E_UI_NOT_OWNER
        );
    }

    #[test]
    fn targeting_register_rejects_foreign_shadowing_and_core_tier() {
        let services = targeting_services(true, true);
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(1))
            .expect("register");
        // The same generation may replace its own lens.
        assert!(
            services
                .ui_targets_register("acme.links", "derived", &targeting_offers(2))
                .expect("replace")
        );
        assert!(
            services
                .ui_targets_unregister("acme.links")
                .expect("remove")
        );
        assert!(
            !services
                .ui_targets_unregister("acme.links")
                .expect("idempotent")
        );
        // The host-owned core tier is never registrable from Lua.
        assert_eq!(
            services
                .ui_targets_register("terminal", "core", &[])
                .expect_err("core denied")
                .code,
            "E_DEF_INVALID"
        );
        // Invalid charsets fail closed with the existing definition code.
        assert_eq!(
            services
                .ui_labels_set_policy("aA", "ab")
                .expect_err("charset")
                .code,
            "E_DEF_INVALID"
        );
    }

    #[test]
    fn w29_out_of_tree_consumer_drives_targeting_with_generic_primitives() {
        // CTX-0942 re-scoped acceptance (DEC-0085): an out-of-tree beacon
        // plugin functions using ONLY generic primitives — provider
        // registration, the read-only snapshot, the W-28 overlay capture,
        // per-generation label bindings, and typed command dispatch. Two
        // independent generations share one runtime-wired mechanism state
        // (one registry, lens set, allocator, and capture switch, exactly as
        // the runtime wires every loaded plugin via `set_targeting_state`).
        // No `bitty.beacon.*` namespace exists, no new capability is gated
        // (both generations carry the accepted `ui.overlay` grant), and Core
        // takes no private path: the first-party generation below uses the
        // same public host ops as the out-of-tree one.
        let registry = Rc::new(RefCell::new(TargetRegistry::new()));
        let lenses: Rc<RefCell<Vec<(String, DerivedProvider)>>> = Rc::new(RefCell::new(Vec::new()));
        let allocator = Rc::new(RefCell::new(LabelAllocator::default()));
        let annotations: SharedTargetAnnotations = Rc::new(RefCell::new(None));
        let capture = Rc::new(RefCell::new(OverlayCapture::new()));
        let first_party = generation_services("bitty.first", 1, &capture);
        let out_of_tree = generation_services("beacon.ext", 2, &capture);
        first_party.set_targeting_state(
            Rc::clone(&registry),
            Rc::clone(&lenses),
            Rc::clone(&allocator),
            Rc::clone(&annotations),
        );
        out_of_tree.set_targeting_state(
            Rc::clone(&registry),
            Rc::clone(&lenses),
            Rc::clone(&allocator),
            Rc::clone(&annotations),
        );

        // Provider composition is generic registration: each consumer's lens
        // joins the shared set and every snapshot derives from all of them.
        out_of_tree
            .ui_targets_register("beacon.links", "plugin", &[("link".to_string(), 7)])
            .expect("out-of-tree register");
        first_party
            .ui_targets_register("first.panels", "plugin", &[("panel".to_string(), 3)])
            .expect("first-party register");
        assert_eq!(
            out_of_tree.ui_targets_snapshot(16).expect("snapshot").len(),
            2,
            "snapshot derives from every registered lens"
        );
        assert_eq!(
            first_party.ui_targets_snapshot(16).expect("snapshot").len(),
            2,
            "the first-party generation sees the out-of-tree lens too"
        );

        // The out-of-tree consumer drives a full session over the W-28
        // capture: mount an overlay block, start the session, and resolve a
        // label to a typed command id from the accepted registry.
        let handle = out_of_tree
            .ui_mount("overlay", &UiNode::text("beacon"))
            .expect("overlay mount");
        let anchors = vec![(10u16, 0u16), (11u16, 0u16)];
        let commands = vec![
            "beacon.ext:jump".to_string(),
            "beacon.ext:focus".to_string(),
        ];
        let labels = out_of_tree
            .ui_targets_session_start(handle, 80, &anchors, &commands)
            .expect("session start");
        assert_eq!(labels.len(), 2);
        let resolved = out_of_tree
            .ui_targets_dispatch(&labels[0])
            .expect("dispatch");
        assert!(
            commands.contains(&resolved),
            "dispatch resolves to the consumer's own typed command id"
        );
        // The targeting flow emits no notifications.
        assert!(
            out_of_tree.notifications.borrow().is_empty(),
            "targeting must not emit notifications"
        );

        // Per-generation isolation: the other generation holds no session,
        // so dispatching the live label from it fails closed instead of
        // resolving a foreign binding.
        assert_eq!(
            first_party
                .ui_targets_dispatch(&labels[0])
                .expect_err("foreign dispatch")
                .code,
            E_UI_NOT_OWNER
        );

        // A newer collection epoch stales the bound handles: dispatch then
        // fails closed instead of resolving to a moved target.
        let _ = out_of_tree
            .targeting_mediator(Vec::new())
            .expect("mediator")
            .collect(&mut registry.borrow_mut())
            .expect("recollect bumps generations");
        assert_eq!(
            out_of_tree
                .ui_targets_dispatch(&labels[0])
                .expect_err("stale dispatch")
                .code,
            E_UI_NOT_OWNER
        );

        // Cancel ends the session and frees the capture; the other consumer
        // then drives its own session with the same generic ops, proving no
        // first-party bypass is needed for any consumer to function.
        assert!(
            out_of_tree
                .ui_targets_session_cancel(handle)
                .expect("cancel")
        );
        let other_handle = first_party
            .ui_mount("overlay", &UiNode::text("first"))
            .expect("overlay mount");
        let other_commands = vec![
            "first.panels:show".to_string(),
            "first.panels:hide".to_string(),
        ];
        // Distinct right-side anchors draw from the right label pool, so the
        // second session's labels are disjoint from the first session's
        // left-pool labels. Dispatching the first session's label while this
        // session is live must fail as an unknown label (not via the
        // no-session guard asserted above).
        let other_anchors = vec![(70u16, 0u16), (71u16, 0u16)];
        let other_labels = first_party
            .ui_targets_session_start(other_handle, 80, &other_anchors, &other_commands)
            .expect("second consumer session");
        assert_eq!(other_labels.len(), 2);
        assert_eq!(
            first_party
                .ui_targets_dispatch(&labels[0])
                .expect_err("foreign label in a live session")
                .code,
            "E_DEF_INVALID"
        );
        let other_resolved = first_party
            .ui_targets_dispatch(&other_labels[1])
            .expect("second consumer dispatch");
        assert!(
            other_commands.contains(&other_resolved),
            "the second consumer resolves its own commands independently"
        );
        assert!(
            first_party
                .ui_targets_session_cancel(other_handle)
                .expect("cancel")
        );
    }

    #[test]
    fn w29_targeting_session_stays_unavailable_in_safe_mode() {
        // Safe mode never presents the focusable overlay (CTX-0941): a
        // targeting session fails with the existing `E_UI_UNAVAILABLE` and
        // records no session, so the safe startup path is unaffected by W-29.
        let services = targeting_services(true, true);
        services.set_safe_mode(true);
        services
            .ui_targets_register("acme.links", "plugin", &targeting_offers(1))
            .expect("register");
        let handle = services
            .ui_mount("overlay", &UiNode::text("targets"))
            .expect("overlay mount");
        assert_eq!(
            services
                .ui_targets_session_start(handle, 80, &targeting_anchors(1), &targeting_commands(1))
                .expect_err("safe mode denies capture")
                .code,
            E_UI_UNAVAILABLE
        );
        assert_eq!(
            services
                .ui_targets_dispatch("a")
                .expect_err("no session recorded")
                .code,
            E_UI_NOT_OWNER
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use crate::plugin_runtime::config_write::{
        CONFIG_SET_CALLS_PER_SECOND, ConfigWriteFailure, outstanding_previews,
    };
    use crate::plugin_runtime::history::{
        BoundedHistory, HISTORY_GRANT_SCOPE_CACHE_MAX, HISTORY_MAX_QUERIES_PER_WINDOW,
        HISTORY_MAX_ROWS_PER_QUERY,
    };
    use bitty_plugin_host::{Attribution, HistorySnapshot, PluginId, StoredRow};
    use std::time::Instant;

    fn services() -> PluginServices {
        PluginServices::new(
            "xuepoo.test",
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(UnavailableSnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        )
    }

    fn row(owner: Option<&str>, panel: &str, seq: u64, body: &str) -> StoredRow {
        StoredRow {
            owner: owner.map(|id| PluginId::new(id).expect("plugin id")),
            panel: panel.to_string(),
            workspace: "ws-1".to_string(),
            seq,
            redacted_body: body.to_string(),
            attribution: Attribution {
                panel: panel.to_string(),
                workspace: "ws-1".to_string(),
                command: None,
                recorded_at: seq,
                actor: None,
            },
            purged: false,
        }
    }

    fn request(op: HistoryQueryOp) -> HistoryQueryRequest {
        HistoryQueryRequest {
            panel: None,
            workspace: Some("ws-1".to_string()),
            row_start: 0,
            row_count: 16,
            max_bytes: 4096,
            op,
            needle: None,
        }
    }

    fn grant(services: &PluginServices, source: HistoryQuerySource) {
        services.set_granted_capabilities(vec![source.capability_head().to_string()]);
    }

    fn provider_with(source: HistorySource, rows: Vec<StoredRow>) -> Rc<BoundedHistory> {
        let provider = Rc::new(BoundedHistory::empty(true, true));
        for row in rows {
            assert!(provider.push(source, row));
        }
        provider
    }

    #[test]
    fn transcript_query_returns_rows_through_the_gate() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        let provider = provider_with(
            HistorySource::Transcript,
            vec![row(None, "pane-a", 0, "secret-free transcript")],
        );
        services.set_history_provider(Some(provider));
        let page = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect("granted transcript page");
        assert_eq!(page.get("ok"), Some(&LuaValue::Bool(true)));
        assert_eq!(page.get("total_in_scope"), Some(&LuaValue::Integer(1)));
        let Some(LuaValue::Table(records)) = page.get("records") else {
            panic!("records array");
        };
        assert_eq!(records.len(), 1);
        let Some(LuaValue::Table(record)) = records.first().map(|(_, value)| value) else {
            panic!("record table");
        };
        let record = LuaValue::Table(record.clone());
        assert!(
            record.get("body") == Some(&LuaValue::String("secret-free transcript".to_string())),
            "unexpected transcript record body"
        );
        assert_eq!(
            record.get("label"),
            Some(&LuaValue::String("untrusted-observation".to_string()))
        );
    }

    #[test]
    fn commands_and_kv_queries_are_per_source_and_namespaced() {
        let services = services();
        services.set_granted_capabilities(vec![
            "history.commands.read".to_string(),
            "history.kv.read".to_string(),
        ]);
        let provider = Rc::new(BoundedHistory::empty(true, true));
        provider.push(
            HistorySource::CommandHistory,
            row(None, "pane-a", 0, "cargo test"),
        );
        provider.push(
            HistorySource::PluginKv,
            row(Some("xuepoo.test"), "", 0, "own value"),
        );
        provider.push(
            HistorySource::PluginKv,
            row(Some("other.plugin"), "", 1, "foreign value"),
        );
        services.set_history_provider(Some(provider));
        let mut kv_request = request(HistoryQueryOp::Tail);
        kv_request.workspace = None;
        let mut commands_request = request(HistoryQueryOp::Search);
        commands_request.needle = Some("cargo".to_string());
        let commands = services
            .history_query(HistoryQuerySource::CommandHistory, &commands_request)
            .expect("commands page");
        assert_eq!(commands.get("total_in_scope"), Some(&LuaValue::Integer(1)));
        let kv = services
            .history_query(HistoryQuerySource::PluginKv, &kv_request)
            .expect("kv page");
        assert_eq!(kv.get("total_in_scope"), Some(&LuaValue::Integer(1)));
        let Some(LuaValue::Table(records)) = kv.get("records") else {
            panic!("records");
        };
        assert_eq!(records.len(), 1);
        assert!(!format!("{kv:?}").contains("foreign value"));
    }

    #[test]
    fn missing_grant_denies_typed_without_touching_provider() {
        let services = services();
        let provider = Rc::new(BoundedHistory::empty(true, true));
        services.set_history_provider(Some(provider));
        let error = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect_err("ungranted");
        assert_eq!(error.class, "runtime");
        assert_eq!(error.code, "E_HISTORY_MISSING_GRANT");
    }

    #[test]
    fn absent_provider_denies_typed_unavailable() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        let error = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect_err("no provider");
        assert_eq!(error.code, "E_HISTORY_UNAVAILABLE");
        assert!(!error.message.contains("transcript"));
    }

    #[test]
    fn empty_result_is_a_successful_page() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(Rc::new(BoundedHistory::empty(true, true))));
        let page = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect("empty page");
        assert_eq!(page.get("ok"), Some(&LuaValue::Bool(true)));
        assert_eq!(page.get("total_in_scope"), Some(&LuaValue::Integer(0)));
    }

    #[test]
    fn over_bound_request_denies_without_clamping() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(provider_with(
            HistorySource::Transcript,
            (0..20)
                .map(|seq| row(None, "pane-a", seq, "line"))
                .collect(),
        )));
        let mut request = request(HistoryQueryOp::List);
        request.row_count = HISTORY_MAX_ROWS_PER_QUERY + 1;
        let error = services
            .history_query(HistoryQuerySource::Transcript, &request)
            .expect_err("over bound");
        assert_eq!(error.code, "E_HISTORY_OVER_BOUND");
    }

    #[test]
    fn snapshot_is_replaced_not_appended_between_calls() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        let provider = provider_with(
            HistorySource::Transcript,
            vec![row(None, "pane-a", 0, "first")],
        );
        services.set_history_provider(Some(provider.clone()));
        let first = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect("first page");
        assert!(format!("{first:?}").contains("first"));
        let mut next = HistorySnapshot::new();
        next.push(HistorySource::Transcript, row(None, "pane-a", 1, "second"));
        provider.publish(next);
        let second = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect("second page");
        let rendered = format!("{second:?}");
        assert!(rendered.contains("second"));
        assert!(!rendered.contains("first"));
        assert_eq!(
            second.get("freshness"),
            Some(&LuaValue::String("point-in-time-no-guarantee".to_string()))
        );
    }

    #[test]
    fn captured_transcript_rows_return_through_the_gate_bounded() {
        use bitty_term_state::{State, TerminalAction};
        use bitty_vt::{ControlChar, GraphemeCell};

        fn feed_line(state: &mut State, text: &str) {
            for ch in text.chars() {
                state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
            }
            state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
            state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
        }

        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        let provider = Rc::new(crate::plugin_runtime::history::TranscriptHistory::new(
            true, false,
        ));
        let mut state = State::with_scrollback_lines(100);
        for index in 0..40 {
            feed_line(&mut state, &format!("captured line {index:02}"));
        }
        let captured = state.scrollback_len();
        assert!(captured > 0);
        assert_eq!(provider.capture_state(&state, "pane-a", "ws-1"), captured);
        services.set_history_provider(Some(provider));
        let page = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::Tail),
            )
            .expect("captured rows page");
        assert_eq!(
            page.get("total_in_scope"),
            Some(&LuaValue::Integer(captured as i64))
        );
        let Some(LuaValue::Table(records)) = page.get("records") else {
            panic!("records array");
        };
        assert_eq!(
            records.len(),
            captured.min(HISTORY_MAX_ROWS_PER_QUERY as usize)
        );
        assert!(!format!("{page:?}").contains("sk-live"));
    }

    #[test]
    fn live_transcript_capture_off_denies_capture_disabled() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(Rc::new(
            crate::plugin_runtime::history::TranscriptHistory::new(false, false),
        )));
        let error = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect_err("capture off must deny with the taxonomy category");
        assert_eq!(error.code, "E_HISTORY_CAPTURE_DISABLED");
        assert!(!error.message.contains("transcript"));
    }

    #[test]
    fn command_source_is_typed_unavailable_when_no_span_source_exists() {
        let services = services();
        services.set_granted_capabilities(vec![
            "history.transcript.read".to_string(),
            "history.commands.read".to_string(),
        ]);
        services.set_history_provider(Some(Rc::new(
            crate::plugin_runtime::history::TranscriptHistory::new(true, true),
        )));
        let transcript = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect("transcript remains available");
        assert_eq!(transcript.get("ok"), Some(&LuaValue::Bool(true)));
        let error = services
            .history_query(
                HistoryQuerySource::CommandHistory,
                &request(HistoryQueryOp::List),
            )
            .expect_err("command source has no backing store");
        assert_eq!(error.code, "E_HISTORY_UNAVAILABLE");
    }

    #[test]
    fn command_source_serves_rows_when_a_provider_has_them() {
        let services = services();
        grant(&services, HistoryQuerySource::CommandHistory);
        services.set_history_provider(Some(provider_with(
            HistorySource::CommandHistory,
            vec![row(None, "pane-a", 0, "cargo test")],
        )));
        let page = services
            .history_query(
                HistoryQuerySource::CommandHistory,
                &request(HistoryQueryOp::List),
            )
            .expect("available command provider");
        assert_eq!(page.get("total_in_scope"), Some(&LuaValue::Integer(1)));
    }

    #[test]
    fn capture_off_denies_terminal_sources() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(Rc::new(BoundedHistory::empty(false, false))));
        let error = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect_err("capture off");
        assert_eq!(error.code, "E_HISTORY_CAPTURE_DISABLED");
    }

    #[test]
    fn safe_mode_denies_every_source() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(Rc::new(BoundedHistory::empty(true, true))));
        services.set_safe_mode(true);
        let error = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect_err("safe mode");
        assert_eq!(error.code, "E_HISTORY_SAFE_MODE");
    }

    #[test]
    fn per_plugin_window_budget_denies_after_four_queries() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(provider_with(
            HistorySource::Transcript,
            vec![row(None, "pane-a", 0, "line")],
        )));
        for index in 0..HISTORY_MAX_QUERIES_PER_WINDOW {
            services
                .history_query(
                    HistoryQuerySource::Transcript,
                    &request(HistoryQueryOp::List),
                )
                .unwrap_or_else(|error| panic!("query {index}: {error:?}"));
        }
        let error = services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect_err("over rate");
        assert_eq!(error.code, "E_HISTORY_OVER_BOUND");
    }

    #[test]
    fn kv_scope_extent_is_rejected() {
        let services = services();
        grant(&services, HistoryQuerySource::PluginKv);
        let provider = Rc::new(BoundedHistory::empty(true, true));
        services.set_history_provider(Some(provider));
        let error = services
            .history_query(HistoryQuerySource::PluginKv, &request(HistoryQueryOp::List))
            .expect_err("kv scope");
        assert_eq!(error.code, "E_DEF_INVALID");
    }

    #[test]
    fn cached_grant_scopes_and_gate_grants_stay_bounded() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(Rc::new(BoundedHistory::empty(true, true))));
        for index in 0..(HISTORY_GRANT_SCOPE_CACHE_MAX * 3) {
            let mut request = request(HistoryQueryOp::List);
            request.panel = Some(format!("pane-{index}"));
            // The per-window rate budget denies most calls; the cache must
            // stay bounded regardless of whether the query succeeds.
            let _ = services.history_query(HistoryQuerySource::Transcript, &request);
            assert!(services.history_grant_scopes.borrow().len() <= HISTORY_GRANT_SCOPE_CACHE_MAX);
            assert!(services.history_gate.borrow().grant_count() <= HISTORY_GRANT_SCOPE_CACHE_MAX);
        }
    }

    #[test]
    fn window_advance_restores_budget() {
        let services = services();
        grant(&services, HistoryQuerySource::Transcript);
        services.set_history_provider(Some(provider_with(
            HistorySource::Transcript,
            vec![row(None, "pane-a", 0, "line")],
        )));
        for _ in 0..HISTORY_MAX_QUERIES_PER_WINDOW {
            services
                .history_query(
                    HistoryQuerySource::Transcript,
                    &request(HistoryQueryOp::List),
                )
                .expect("within budget");
        }
        services
            .history_window_start
            .set(Instant::now() - HISTORY_RATE_WINDOW - std::time::Duration::from_secs(1));
        services
            .history_query(
                HistoryQuerySource::Transcript,
                &request(HistoryQueryOp::List),
            )
            .expect("budget restored");
    }

    #[test]
    fn runtime_inspect_requires_its_own_read_grant() {
        let denied = services();
        let error = denied.runtime_inspect().expect_err("no grant");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(error.message, "capability 'runtime.inspect' is not granted");

        // Holding the write-side colorscheme grant never implies the read.
        let write_only = services();
        write_only.set_theme_access(true);
        let error = write_only.runtime_inspect().expect_err("read denied");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");

        // The read grant never implies the write-side colorscheme grant.
        let read_only = services();
        read_only.set_runtime_inspect(true, Some(RuntimeInspectSnapshot::default()));
        assert!(
            !read_only.theme_configure_granted.get(),
            "runtime.inspect must not grant runtime.configure"
        );
        assert!(read_only.runtime_inspect().is_ok());

        // A granted generation without a wired snapshot fails closed.
        let unwired = services();
        unwired.set_runtime_inspect(true, None);
        let error = unwired.runtime_inspect().expect_err("no backend");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    }

    #[test]
    fn runtime_inspect_enforces_the_ten_millisecond_deadline() {
        let services = services();
        services.set_runtime_inspect(true, Some(RuntimeInspectSnapshot::default()));
        let started = Instant::now()
            .checked_sub(Duration::from_millis(RUNTIME_INSPECT_MAX_WALL_MS + 1))
            .expect("monotonic clock has enough history");
        let error = services
            .runtime_inspect_at(started)
            .expect_err("deadline exceeded");
        assert_eq!(error.code, "E_TIMEOUT");
        assert_eq!(error.class, "budget");
    }

    #[test]
    fn runtime_inspect_unwired_calls_do_not_consume_the_budget() {
        let services = services();
        services.set_runtime_inspect(true, None);
        for _ in 0..RUNTIME_INSPECT_CALLS_PER_SECOND * 2 {
            let error = services.runtime_inspect().expect_err("unwired");
            assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        }
        services.set_runtime_inspect(true, Some(RuntimeInspectSnapshot::default()));
        for call in 0..RUNTIME_INSPECT_CALLS_PER_SECOND {
            assert!(services.runtime_inspect().is_ok(), "call {call}");
        }
        assert_eq!(
            services.runtime_inspect().expect_err("over budget").code,
            "E_DEF_LIMIT"
        );
    }

    #[test]
    fn runtime_inspect_serves_and_enforces_sixteen_calls_per_second() {
        let services = services();
        services.set_runtime_inspect(true, Some(RuntimeInspectSnapshot::default()));
        for call in 0..RUNTIME_INSPECT_CALLS_PER_SECOND {
            assert!(services.runtime_inspect().is_ok(), "call {call}");
        }
        let error = services.runtime_inspect().expect_err("over budget");
        assert_eq!(error.code, "E_DEF_LIMIT");
        assert_eq!(error.class, "validation");
    }

    /// Fixed keymap source standing in for the app's `LiveKeymaps`.
    struct FixedKeymaps(RefCell<Vec<ResolvedKeymap>>);

    impl KeymapSource for FixedKeymaps {
        fn keymaps(&self) -> Result<Vec<ResolvedKeymap>, BridgeError> {
            Ok(self.0.borrow().clone())
        }
    }

    fn binding(chord: &str, action: &str, from_default: bool) -> ResolvedKeymap {
        ResolvedKeymap {
            chord: bitty_config::Chord::parse(chord).expect("chord"),
            action: ChromeAction::parse(action).expect("action"),
            context: "global".to_string(),
            from_default,
        }
    }

    fn keymap_fixture() -> Vec<ResolvedKeymap> {
        vec![
            binding("alt+h", "goto_split:left", true),
            binding("alt+1", "workspace_focus:1", true),
            binding("alt+p", "toggle_pinned", false),
            binding(
                "alt+shift+p",
                "command:bitty-terminal.help:toggle_help",
                false,
            ),
            binding("ctrl+shift+v", "paste_from_clipboard", true),
        ]
    }

    fn keymap_services(granted: bool) -> (PluginServices, Rc<FixedKeymaps>) {
        let services = services();
        let source = Rc::new(FixedKeymaps(RefCell::new(keymap_fixture())));
        services.set_keymaps_access(granted, Some(source.clone()));
        (services, source)
    }

    #[test]
    fn keymaps_list_requires_its_own_read_grant() {
        let (denied, _source) = keymap_services(false);
        let error = denied
            .keymaps_list(&KeymapQuery::default())
            .expect_err("no grant");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(
            error.message,
            "capability 'runtime.keymaps.read' is not granted"
        );

        // The runtime inspection read never implies the keymap read.
        let inspect_only = services();
        inspect_only.set_runtime_inspect(true, Some(RuntimeInspectSnapshot::default()));
        let error = inspect_only
            .keymaps_list(&KeymapQuery::default())
            .expect_err("inspect does not grant keymaps");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");

        // The write-side colorscheme grant never implies it either.
        let configure_only = services();
        configure_only.set_theme_access(true);
        let error = configure_only
            .keymaps_list(&KeymapQuery::default())
            .expect_err("configure does not grant keymaps");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");

        // A granted generation without a wired source fails closed.
        let unwired = services();
        unwired.set_keymaps_access(true, None);
        let error = unwired
            .keymaps_list(&KeymapQuery::default())
            .expect_err("no backend");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");

        // The keymap read never implies `runtime.configure`.
        let (read_only, _source) = keymap_services(true);
        assert!(read_only.keymaps_list(&KeymapQuery::default()).is_ok());
        assert!(
            !read_only.theme_configure_granted.get(),
            "runtime.keymaps.read must not grant runtime.configure"
        );
    }

    #[test]
    fn keymaps_list_safe_policy_denies_even_with_the_grant() {
        let (services, _source) = keymap_services(true);
        services.set_safe_mode(true);
        let error = services
            .keymaps_list(&KeymapQuery::default())
            .expect_err("safe policy");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(
            error.message,
            "capability 'runtime.keymaps.read' is not granted"
        );
    }

    #[test]
    fn keymaps_list_serves_a_bounded_page_with_canonical_fields() {
        let (services, _source) = keymap_services(true);
        let page = services
            .keymaps_list(&KeymapQuery::new(1, 2).expect("query"))
            .expect("list");
        assert_eq!(page.total, 5);
        assert_eq!(page.offset, 1);
        assert_eq!(page.entries.len(), 2);
        assert!(page.truncated, "rows 3..5 are behind the page");
        assert_eq!(page.entries[0].chord, "alt+1");
        assert_eq!(page.entries[0].action, "workspace_focus:1");
        assert_eq!(page.entries[0].source, "default");
        assert_eq!(page.entries[0].context, "global");
        assert_eq!(
            page.entries[0].title.as_deref(),
            Some("Jump to workspace N.")
        );
        assert!(page.entries[0].command.is_none());

        let full = services
            .keymaps_list(&KeymapQuery::default())
            .expect("full page");
        assert_eq!(full.total, 5);
        assert_eq!(full.entries.len(), 5);
        assert!(!full.truncated);
        assert!(full.fits_budget());
        // User override attribution and the `command:` row shape.
        assert_eq!(full.entries[2].source, "user");
        assert!(full.entries[2].title.is_some());
        let command = &full.entries[3];
        assert_eq!(command.action, "command:bitty-terminal.help:toggle_help");
        assert_eq!(
            command.command.as_deref(),
            Some("bitty-terminal.help:toggle_help")
        );
        assert!(command.title.is_none());
        assert_eq!(command.source, "user");

        // An offset at or past the total is an empty, untruncated page.
        let past = services
            .keymaps_list(&KeymapQuery::new(5, 256).expect("query"))
            .expect("past end");
        assert!(past.entries.is_empty());
        assert_eq!(past.total, 5);
        assert_eq!(past.offset, 5);
        assert!(!past.truncated);
        assert!(!page_contains(&full, "/"), "no path-shaped field leaks");
        assert!(!page_contains(&full, "http"), "no URL-shaped field leaks");
    }

    /// Look for a forbidden field value in a page's Lua conversion.
    fn page_contains(page: &KeymapPage, needle: &str) -> bool {
        format!("{:?}", page.to_lua()).contains(needle)
    }

    #[test]
    fn keymaps_list_drops_malformed_rows_instead_of_echoing_them() {
        let (services, source) = keymap_services(true);
        let mut rows = keymap_fixture();
        rows.push(ResolvedKeymap {
            chord: bitty_config::Chord::parse("alt+9").expect("chord"),
            action: ChromeAction::CloseView,
            // Only `"global"` is accepted today; a row outside it is dropped
            // rather than advertised under a context the dispatcher ignores.
            context: "unused-context".to_string(),
            from_default: true,
        });
        *source.0.borrow_mut() = rows;
        let page = services
            .keymaps_list(&KeymapQuery::default())
            .expect("list");
        assert_eq!(page.total, 5, "the malformed row is not addressable");
        assert_eq!(page.entries.len(), 5);
        assert!(!page_contains(&page, "unused-context"));
    }

    #[test]
    fn keymaps_list_addressable_cap_hides_the_unreachable_tail() {
        let (services, source) = keymap_services(true);
        let wide = (0..bitty_lua::keymaps::KEYMAP_LIST_MAX_TOTAL + 76)
            .map(|_| binding("alt+x", "close_view", true))
            .collect();
        *source.0.borrow_mut() = wide;
        let page = services
            .keymaps_list(&KeymapQuery::default())
            .expect("list");
        assert_eq!(page.total, bitty_lua::keymaps::KEYMAP_LIST_MAX_TOTAL);
        assert_eq!(page.entries.len(), bitty_lua::keymaps::KEYMAP_LIST_MAX_ROWS);
        assert!(page.truncated);
    }

    #[test]
    fn keymap_budget_is_a_strict_sliding_window() {
        // Deterministic synthetic clock: a burst may never exceed the rate,
        // even across a wall-clock second boundary, and each expired call
        // frees exactly one slot.
        let start = Instant::now();
        let mut budget = KeymapCallBudget::new();
        for call in 0..KEYMAP_LIST_CALLS_PER_SECOND {
            assert!(
                budget.allow(start + Duration::from_millis(u64::from(call))),
                "call {call}"
            );
        }
        let last = start + Duration::from_millis(u64::from(KEYMAP_LIST_CALLS_PER_SECOND - 1));
        assert!(
            !budget.allow(last + Duration::from_millis(1)),
            "a 17th call inside the window is denied"
        );
        assert!(
            !budget.allow(start + Duration::from_millis(999)),
            "the window is one second from the oldest accept"
        );
        // The first accepted call expires at exactly +1s; one slot opens.
        assert!(budget.allow(start + Duration::from_secs(1)));
        assert!(!budget.allow(start + Duration::from_secs(1)));
    }

    #[test]
    fn keymaps_list_enforces_sixteen_calls_per_second() {
        let (services, _source) = keymap_services(true);
        for call in 0..KEYMAP_LIST_CALLS_PER_SECOND {
            assert!(
                services.keymaps_list(&KeymapQuery::default()).is_ok(),
                "call {call}"
            );
        }
        let error = services
            .keymaps_list(&KeymapQuery::default())
            .expect_err("over budget");
        assert_eq!(error.code, "E_DEF_LIMIT");
        assert_eq!(error.class, "validation");
    }

    // ── config.manage (CTX-1613, bitty#2393) ──────────────────────────

    /// Recording backend: renders candidates, counts preview/apply calls, and
    /// can inject a typed failure or a moved candidate.
    struct RecordingConfigBackend {
        previews: RefCell<Vec<(String, ConfigChange)>>,
        calls: RefCell<Vec<(String, ConfigChange, String)>>,
        outcome: ConfigApplyOutcome,
        failure: Option<ConfigWriteFailure>,
        nonce: Cell<u32>,
        stale: Cell<bool>,
    }

    impl RecordingConfigBackend {
        fn applied() -> Rc<Self> {
            Rc::new(Self {
                previews: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
                outcome: ConfigApplyOutcome::Applied,
                failure: None,
                nonce: Cell::new(0),
                stale: Cell::new(false),
            })
        }

        fn failure(failure: ConfigWriteFailure) -> Rc<Self> {
            Rc::new(Self {
                previews: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
                outcome: ConfigApplyOutcome::Applied,
                failure: Some(failure),
                nonce: Cell::new(0),
                stale: Cell::new(false),
            })
        }

        fn unchanged() -> Rc<Self> {
            Rc::new(Self {
                previews: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
                outcome: ConfigApplyOutcome::Unchanged,
                failure: None,
                nonce: Cell::new(0),
                stale: Cell::new(false),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }

        fn preview_count(&self) -> usize {
            self.previews.borrow().len()
        }

        fn bump_nonce(&self) {
            self.nonce.set(self.nonce.get() + 1);
        }

        fn rendering_for(&self, change: &ConfigChange) -> ConfigPreviewRendering {
            let candidate = format!("{}\0nonce={}", change.rendered(), self.nonce.get());
            ConfigPreviewRendering {
                changed_keys: vec![change.key()],
                rendered: change.rendered().to_string(),
                candidate_digest: bitty_package::integrity::sha256_hex(candidate.as_bytes()),
            }
        }
    }

    impl ConfigWriteBackend for RecordingConfigBackend {
        fn preview(
            &self,
            plugin_id: &str,
            change: &ConfigChange,
        ) -> Result<ConfigPreviewRendering, ConfigWriteFailure> {
            self.previews
                .borrow_mut()
                .push((plugin_id.to_string(), change.clone()));
            Ok(self.rendering_for(change))
        }

        fn apply(
            &self,
            plugin_id: &str,
            change: &ConfigChange,
            expected_candidate_digest: &str,
        ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
            self.calls.borrow_mut().push((
                plugin_id.to_string(),
                change.clone(),
                expected_candidate_digest.to_string(),
            ));
            if self.stale.get() {
                return Err(ConfigWriteFailure::Stale);
            }
            if self.rendering_for(change).candidate_digest != expected_candidate_digest {
                return Err(ConfigWriteFailure::Stale);
            }
            match self.failure {
                Some(failure) => Err(failure),
                None => Ok(self.outcome),
            }
        }
    }

    fn config_services(backend: Option<Rc<RecordingConfigBackend>>) -> PluginServices {
        let services = services();
        services.set_config_access(true);
        if let Some(backend) = backend {
            services.set_config_write_backend(Some(backend));
        }
        services
    }

    fn theme_text(value: &str) -> LuaValue {
        LuaValue::String(value.to_string())
    }

    #[test]
    fn config_manage_requires_its_own_write_grant() {
        // CTX-1613: `config.manage` is deny-by-default and independent of
        // the read inspection head. A read-only dashboard stays read-only.
        let denied = services();
        for error in [
            denied
                .config_preview("appearance.theme", &theme_text("catppuccin"))
                .expect_err("no grant"),
            denied
                .config_set(
                    "appearance.theme",
                    &theme_text("catppuccin"),
                    &"0".repeat(64),
                )
                .expect_err("no grant"),
        ] {
            assert_eq!(error.code, "E_CAPABILITY_DENIED");
            assert_eq!(error.message, "capability 'config.manage' is not granted");
        }

        // Holding the read head never grants the write head.
        let read_only = services();
        read_only.set_runtime_inspect(true, Some(RuntimeInspectSnapshot::default()));
        let error = read_only
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect_err("read never implies write");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");

        // Holding the write head never grants the read head either.
        let write_only = config_services(None);
        assert!(
            !write_only.runtime_inspect_granted.get(),
            "config.manage must not grant runtime.inspect"
        );

        // A granted generation without a wired backend fails closed with the
        // typed unavailable class, never a generic not-implemented.
        let unwired = config_services(None);
        for error in [
            unwired
                .config_preview("appearance.theme", &theme_text("catppuccin"))
                .expect_err("no backend"),
            unwired
                .config_set(
                    "appearance.theme",
                    &theme_text("catppuccin"),
                    &"0".repeat(64),
                )
                .expect_err("no backend"),
        ] {
            assert_eq!(error.code, "E_CONFIG_UNAVAILABLE");
            assert_eq!(error.class, "runtime");
        }
    }

    #[test]
    fn config_set_applies_only_after_preview_and_reports_reload() {
        let backend = RecordingConfigBackend::applied();
        let services = config_services(Some(Rc::clone(&backend)));

        let preview = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("preview");
        assert_eq!(preview.rendered, "appearance.theme = \"catppuccin\"");
        assert_eq!(preview.changed_keys, vec!["appearance.theme"]);
        assert_eq!(preview.digest.len(), 64);
        assert_eq!(backend.preview_count(), 1, "preview renders, never writes");
        assert_eq!(backend.call_count(), 0, "preview never writes");

        let outcome = services
            .config_set(
                "appearance.theme",
                &theme_text("catppuccin"),
                &preview.digest,
            )
            .expect("applied");
        assert!(outcome.applied);
        assert_eq!(
            outcome.reload, "auto",
            "the watcher re-reads the user layer"
        );
        assert_eq!(backend.call_count(), 1);
        assert_eq!(
            backend.calls.borrow()[0].1.rendered(),
            "appearance.theme = \"catppuccin\""
        );
        assert_eq!(
            backend.calls.borrow()[0].0,
            "xuepoo.test",
            "the backend receives the plugin identity for attribution"
        );

        // One token authorizes one write: replaying the approved token can
        // never re-apply a stale row.
        let error = services
            .config_set(
                "appearance.theme",
                &theme_text("catppuccin"),
                &preview.digest,
            )
            .expect_err("replay refused");
        assert_eq!(error.code, "E_CONFIG_APPROVAL_REQUIRED");
        assert_eq!(backend.call_count(), 1, "no second write");
    }

    #[test]
    fn re_preview_replaces_the_outstanding_approval() {
        // Review B6: one exact change has one live approval. Re-previewing
        // replaces the token instead of stacking a second usable one.
        let backend = RecordingConfigBackend::applied();
        let services = config_services(Some(Rc::clone(&backend)));
        let first = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("first preview");
        backend.bump_nonce();
        let second = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("second preview");
        assert_ne!(first.digest, second.digest, "the candidate moved");
        assert_eq!(
            outstanding_previews(&services.config_gate.borrow()),
            1,
            "the repeated preview replaced the old approval"
        );
        let error = services
            .config_set("appearance.theme", &theme_text("catppuccin"), &first.digest)
            .expect_err("stale approval");
        assert_eq!(error.code, "E_CONFIG_APPROVAL_REQUIRED");
        services
            .config_set(
                "appearance.theme",
                &theme_text("catppuccin"),
                &second.digest,
            )
            .expect("current approval applies");
        assert_eq!(backend.call_count(), 1);
    }

    #[test]
    fn config_set_approval_is_bound_to_the_exact_change() {
        // The host cannot prove human intent: a granted plugin can call
        // `preview` itself. What it does enforce is that the applied bytes
        // equal the previewed candidate and that the token is one-shot. The
        // two-press confirmation is a dashboard UI contract, recorded in the
        // plugin, not a host guarantee.
        let backend = RecordingConfigBackend::applied();
        let services = config_services(Some(Rc::clone(&backend)));
        let preview = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("preview");

        // Same token, different value: the token is value-bound.
        let error = services
            .config_set("appearance.theme", &theme_text("dracula"), &preview.digest)
            .expect_err("value changed");
        assert_eq!(error.code, "E_CONFIG_APPROVAL_REQUIRED");

        // Same token, different key: key-bound.
        let error = services
            .config_set("font.family", &theme_text("catppuccin"), &preview.digest)
            .expect_err("key changed");
        assert_eq!(error.code, "E_CONFIG_APPROVAL_REQUIRED");

        // Malformed tokens (short, uppercase, non-hex) refuse as mismatches.
        for token in [
            "0".repeat(63),
            preview.digest.to_uppercase(),
            "z".repeat(64),
        ] {
            let error = services
                .config_set("appearance.theme", &theme_text("catppuccin"), &token)
                .expect_err("bad token");
            assert_eq!(error.code, "E_CONFIG_APPROVAL_MISMATCH");
        }

        // A well-formed token that was never issued is still refused.
        let fresh = config_services(Some(Rc::clone(&backend)));
        let error = fresh
            .config_set(
                "appearance.theme",
                &theme_text("catppuccin"),
                &"0".repeat(64),
            )
            .expect_err("unissued token");
        assert_eq!(error.code, "E_CONFIG_APPROVAL_REQUIRED");
        assert_eq!(backend.call_count(), 0, "nothing reached the writer");
    }

    #[test]
    fn config_set_refuses_a_moved_candidate() {
        // Review B3: `set` re-renders and verifies the exact bytes before
        // commit. A profile edit between preview and set refuses as stale,
        // leaving the previous bytes untouched.
        let backend = RecordingConfigBackend::applied();
        let services = config_services(Some(Rc::clone(&backend)));
        let preview = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("preview");
        backend.stale.set(true);
        let error = services
            .config_set(
                "appearance.theme",
                &theme_text("catppuccin"),
                &preview.digest,
            )
            .expect_err("candidate moved");
        assert_eq!(error.code, "E_CONFIG_STALE");
        assert_eq!(backend.call_count(), 1);
        assert_eq!(error.class, "runtime");
    }

    #[test]
    fn config_set_reports_unchanged_without_a_reload() {
        let backend = RecordingConfigBackend::unchanged();
        let services = config_services(Some(Rc::clone(&backend)));
        let preview = services
            .config_preview("font.size", &LuaValue::Number(12.5))
            .expect("preview");
        let outcome = services
            .config_set("font.size", &LuaValue::Number(12.5), &preview.digest)
            .expect("unchanged");
        assert!(!outcome.applied);
        assert_eq!(outcome.reload, "none");
        assert_eq!(backend.call_count(), 1);
    }

    #[test]
    fn config_set_refuses_injection_shapes_before_the_backend() {
        let backend = RecordingConfigBackend::applied();
        let services = config_services(Some(Rc::clone(&backend)));

        // No path, argv, shell, or family-prefix key is addressable.
        for key in [
            "terminal.shell",
            "/etc/bitty/init.lua",
            "../../init",
            "profiles/default",
            "appearance",
            "Appearance.Theme",
            "appearance.theme ",
        ] {
            let error = services
                .config_preview(key, &theme_text("catppuccin"))
                .expect_err("not allowlisted");
            assert_eq!(error.code, "E_CONFIG_KEY", "{key}");
            assert!(
                !error.message.contains(key),
                "rejected key must not be echoed"
            );
        }

        // Values are typed scalars: tables are never a loose key/value bag.
        for value in [
            LuaValue::Table(vec![(
                LuaValue::String("appearance.theme".to_string()),
                LuaValue::String("catppuccin".to_string()),
            )]),
            LuaValue::Bool(true),
            LuaValue::Nil,
        ] {
            let error = services
                .config_preview("appearance.theme", &value)
                .expect_err("wrong value shape");
            assert_eq!(error.code, "E_CONFIG_VALUE");
        }

        // Control characters never enter a persisted literal.
        let error = services
            .config_preview(
                "appearance.theme",
                &theme_text("catppuccin\nappearance = { theme = \"x\" }"),
            )
            .expect_err("control characters");
        assert_eq!(error.code, "E_CONFIG_VALUE");

        // A hostile-looking string stays data: it is quoted and escaped, and
        // the exact escaped rendering is what the approval binds.
        let hostile = "\"; os.execute('id'); --";
        let preview = services
            .config_preview("appearance.theme", &theme_text(hostile))
            .expect("text stays text");
        assert_eq!(
            preview.rendered,
            "appearance.theme = \"\\\"; os.execute('id'); --\""
        );
        let outcome = services
            .config_set("appearance.theme", &theme_text(hostile), &preview.digest)
            .expect("literal write");
        assert!(outcome.applied);
        assert_eq!(backend.call_count(), 1);
        assert_eq!(
            backend.calls.borrow()[0].1.rendered(),
            "appearance.theme = \"\\\"; os.execute('id'); --\""
        );
    }

    #[test]
    fn config_set_enforces_call_byte_and_wall_budgets() {
        // Calls: a 17th call in the one-second window refuses.
        let services = config_services(Some(RecordingConfigBackend::applied()));
        for call in 0..CONFIG_SET_CALLS_PER_SECOND {
            services
                .config_preview("appearance.theme", &theme_text(&format!("theme{call}")))
                .expect("budgeted preview");
        }
        let error = services
            .config_preview("appearance.theme", &theme_text("theme-last"))
            .expect_err("over call budget");
        assert_eq!(error.code, "E_DEF_LIMIT");

        // Bytes: four ~1 KiB previews fit; the fifth crosses 4 KiB.
        let bytes_services = config_services(Some(RecordingConfigBackend::applied()));
        let large = "x".repeat(1000);
        for _ in 0..4 {
            bytes_services
                .config_preview("font.family", &theme_text(&large))
                .expect("byte-budgeted preview");
        }
        let error = bytes_services
            .config_preview("font.family", &theme_text(&large))
            .expect_err("over byte budget");
        assert_eq!(error.code, "E_DEF_LIMIT");

        // Wall budget: an authorization path already past 10 ms refuses
        // before the token is consumed, so a deadline never commits a write.
        let backend = RecordingConfigBackend::applied();
        let wall = config_services(Some(Rc::clone(&backend)));
        let preview = wall
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("preview");
        let started = Instant::now()
            .checked_sub(Duration::from_millis(CONFIG_SET_MAX_WALL_MS + 1))
            .expect("monotonic clock has enough history");
        let error = wall
            .config_set_with_expiry(
                "appearance.theme",
                &theme_text("catppuccin"),
                &preview.digest,
                started,
                Instant::now() + Duration::from_secs(1),
            )
            .expect_err("deadline exceeded");
        assert_eq!(error.code, "E_DEF_LIMIT");
        assert_eq!(backend.call_count(), 0);

        // A bridge deadline already passed refuses before the token is
        // consumed, so the approval remains usable and nothing is written.
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("monotonic clock has enough history");
        let wall = config_services(Some(Rc::clone(&backend)));
        let preview = wall
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("preview");
        let error = wall
            .config_set_with_expiry(
                "appearance.theme",
                &theme_text("catppuccin"),
                &preview.digest,
                Instant::now(),
                expired,
            )
            .expect_err("expired call");
        assert_eq!(error.code, "E_TIMEOUT");
        assert_eq!(backend.call_count(), 0);
        wall.config_set(
            "appearance.theme",
            &theme_text("catppuccin"),
            &preview.digest,
        )
        .expect("the approval survived the timeout");
        assert_eq!(backend.call_count(), 1);

        // A value over 1 KiB is refused before any budget is consumed.
        let error = services
            .config_preview("font.family", &theme_text(&"y".repeat(1025)))
            .expect_err("oversize value");
        assert_eq!(error.code, "E_CONFIG_VALUE");
    }

    #[test]
    fn config_manage_safe_mode_denies_even_with_grant_and_backend() {
        let backend = RecordingConfigBackend::applied();
        let services = config_services(Some(Rc::clone(&backend)));
        let preview = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect("preview before safe mode");
        services.set_safe_mode(true);
        let error = services
            .config_preview("appearance.theme", &theme_text("catppuccin"))
            .expect_err("safe mode denies previews");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        let error = services
            .config_set(
                "appearance.theme",
                &theme_text("catppuccin"),
                &preview.digest,
            )
            .expect_err("safe mode denies writes");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        assert_eq!(backend.call_count(), 0);
    }

    #[test]
    fn config_set_backend_failures_are_path_free() {
        for (failure, expected, class) in [
            (
                ConfigWriteFailure::Invalid,
                "E_CONFIG_INVALID",
                "validation",
            ),
            (
                ConfigWriteFailure::Unavailable,
                "E_CONFIG_UNAVAILABLE",
                "runtime",
            ),
            (ConfigWriteFailure::Unsafe, "E_CONFIG_UNSAFE", "io"),
            (ConfigWriteFailure::Stale, "E_CONFIG_STALE", "runtime"),
            (ConfigWriteFailure::Io, "E_CONFIG_IO", "io"),
        ] {
            let backend = RecordingConfigBackend::failure(failure);
            let services = config_services(Some(Rc::clone(&backend)));
            let preview = services
                .config_preview("appearance.theme", &theme_text("catppuccin"))
                .expect("preview");
            let error = services
                .config_set(
                    "appearance.theme",
                    &theme_text("catppuccin"),
                    &preview.digest,
                )
                .expect_err("backend refused");
            assert_eq!(error.code, expected);
            assert_eq!(error.class, class);
            assert_eq!(backend.call_count(), 1, "the backend was reached once");
            assert!(
                !error.message.contains('/') && !error.message.contains('~'),
                "diagnostics must not carry host paths: {}",
                error.message
            );
            assert!(error.message.contains("appearance.theme"));
        }
    }
}
