//! Host-service boundary for one plugin generation (RFC Gap C, minimal slice).
//!
//! `PluginServices` is the object-safe [`HostServices`] implementation handed
//! to a plugin VM. It owns the plugin-scoped store and defers settings and
//! terminal snapshots to injected, generation-stable sources. Capability
//! gating is evaluated from the grant snapshot taken at activation; an absent
//! grant fails closed before any side effect.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use bitty_lua::ui::UiSlot;
use bitty_lua::ui::{UI_MAX_AGGREGATED_TEXT_BYTES, UI_MAX_BLOCKS, UiNode};
use bitty_lua::{
    BridgeError, E_UI_NOT_OWNER, E_UI_UNAVAILABLE, HostServices, LuaValue, LuaVm,
    OVERLAY_CALL_MAX_BYTES, OVERLAY_SPEC_PLACEHOLDER_MAX_BYTES, OVERLAY_SPEC_TITLE_MAX_BYTES,
    OverlayInput, OverlayPoll, SNAPSHOT_MAX_BYTES, ServiceRoute, StashedFunction,
    WORKSPACE_LIST_MAX_ITEMS, WorkspaceInfo, WorkspaceRequest, env_grant_shape_ok,
    validate_env_key,
};
use bitty_package::Version;
use bitty_plugin_host::bundled::{WORKSPACELINE_CLAIM, canonicalize_ui_claim};
use bitty_ui::{
    BeaconAnnotationLayer, BeaconDispatcher, CommandBlockId, DerivedProvider, DispatchError,
    LabelAllocator, LabelPolicy, LinkId, ProviderError, ProviderMediator, ProviderTarget,
    QualifiedCommand, Rect, TargetProvider, TargetRef, TargetRegistry,
};
use bitty_ui::{
    MAX_BEACON_BINDINGS, MAX_BEACON_TARGETS, MAX_SNAPSHOT_TARGETS, MAX_TARGET_PROVIDERS,
};
use bitty_ui::{PanelId, Point, UiNodeId, WorkspaceId};

use crate::runtime::band_slots::{UiSlotPlacement, ui_slot_placement, unsupported_slot_error};
use bitty_plugin_host::{ProvidedService, service_version_satisfies, value_satisfies_schema};

use super::debug::{self, DebugView, TraceHub, TraceRequest};
use super::overlay::{
    DEFAULT_RELEASE_REASON, OVERLAY_CAPTURE_TIMEOUT_MS, OverlayCapture, is_owner_release_reason,
};
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

/// Injected spawn backend for one plugin generation.
///
/// Receives the Lua-validated argv and returns the bounded result table
/// (`output`/`stderr`/`truncated`/`exit_code`/`execution_id`/`untrusted`). The production
/// backend is the consent-gated [`SpawnService`](super::spawn::SpawnService)
/// path wired at activation; tests inject canned closures. `None` (no
/// backend) fails closed with `E_SPAWN_UNAVAILABLE`.
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

/// Settings source that has no settings.
#[derive(Debug, Default)]
pub struct EmptySettings;

impl SettingsSource for EmptySettings {
    fn get(&self, _key: &str) -> Option<LuaValue> {
        None
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
    /// Manifest `[lazy].claims` (exclusive slot claims; `tabline` only).
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
/// diagnostic pending an accepted stable plugin-visible code.
#[derive(Debug)]
pub struct UiBlocks {
    blocks: Vec<(i64, UiBlock)>,
    epoch: u32,
    next_counter: u32,
    aggregated_text_bytes: usize,
}

/// Bits of the handle reserved for the generation epoch. Capped at 31 so every
/// composed `i64` handle stays positive (the surface types handles as opaque
/// integers; positivity is diagnostic only).
const UI_HANDLE_EPOCH_BITS: u32 = 31;

/// Mask selecting the generation epoch bits of a handle.
const UI_HANDLE_EPOCH_MASK: u32 = (1 << UI_HANDLE_EPOCH_BITS) - 1;

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
            aggregated_text_bytes: 0,
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
        self.aggregated_text_bytes = 0;
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
        Ok((epoch << 32) | i64::from(self.next_counter))
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
        let bytes = node.text_bytes();
        let total = self.aggregated_text_bytes.saturating_add(bytes);
        if total > UI_MAX_AGGREGATED_TEXT_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block text budget exceeded (2 MiB aggregated)",
            ));
        }
        let handle = self.next_handle()?;
        self.next_counter = self.next_counter.saturating_add(1);
        self.aggregated_text_bytes = total;
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
        let old_bytes = self.blocks[position].1.node.text_bytes();
        let total = self
            .aggregated_text_bytes
            .saturating_sub(old_bytes)
            .saturating_add(node.text_bytes());
        if total > UI_MAX_AGGREGATED_TEXT_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block text budget exceeded (2 MiB aggregated)",
            ));
        }
        self.aggregated_text_bytes = total;
        let block = &mut self.blocks[position].1;
        block.node = node;
        block.version = block.version.saturating_add(1);
        Ok(true)
    }

    /// Drop one block by handle, releasing its text budget.
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
        self.aggregated_text_bytes = self
            .aggregated_text_bytes
            .saturating_sub(block.node.text_bytes());
        true
    }
}

/// One published service record in the runtime service directory (LUA-OQ-8).
///
/// Published at activation from the provider manifest's `services.provided`
/// entry (version and schemas) plus the generation capture (impl functions);
/// version and schemas come from the manifest, never from Lua. The provider
/// VM is held weakly: dispose drops the strong handle, so a stale consumer
/// route upgrades to nothing and fails closed with `E_SERVICE_GONE`.
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
    /// Provider VM (weak: dispose invalidates without directory coordination).
    pub vm: Weak<RefCell<LuaVm>>,
    /// Set while the provider is suspended; suspended records resolve and
    /// serve nothing until resume republishes them in place.
    pub suspended: bool,
}

/// Live per-runtime service directory (LUA-OQ-8).
///
/// Owned by [`PluginRuntime`](super::PluginRuntime) and shared with every
/// generation's [`PluginServices`]: activation publishes, suspend parks,
/// resume restores, dispose/reload revokes. Keyed by interface with one
/// record per provider, so several plugins may provide the same interface
/// and resolution picks deterministically (highest satisfying version,
/// ties broken by provider id).
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

    /// Publish one activation record, replacing the same provider's prior
    /// record for the interface (a provider publishes each interface once
    /// per generation; re-activation after revoke starts clean).
    pub fn publish(&mut self, record: ServiceRecord) {
        let entry = self.records.entry(record.iface.clone()).or_default();
        entry.retain(|existing| existing.provider != record.provider);
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

    /// Cloned active record for (`iface`, `provider`), if published.
    #[must_use]
    pub fn find_active(&self, iface: &str, provider: &str) -> Option<ServiceRecord> {
        self.records.get(iface).and_then(|records| {
            records
                .iter()
                .find(|record| record.provider == provider && !record.suspended)
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

/// `E_SERVICE_GONE` for a dead, stale, or parked provider record.
fn service_gone_error(detail: String) -> BridgeError {
    BridgeError::new("runtime", "E_SERVICE_GONE", detail)
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

#[allow(clippy::type_complexity)]
pub struct PluginServices {
    plugin_id: String,
    store: RefCell<PluginStore>,
    settings: Rc<dyn SettingsSource>,
    snapshot: Rc<dyn SnapshotSource>,
    notifications: Rc<RefCell<NotificationQueue>>,
    terminal_read: bool,
    platform_notify: bool,
    spawn_git: Cell<bool>,
    spawn_backend: RefCell<Option<SpawnHandler>>,
    ui_access: RefCell<UiAccess>,
    ui_blocks: RefCell<UiBlocks>,
    overlay_capture: RefCell<Option<Rc<RefCell<OverlayCapture>>>>,
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
    /// in `bitty-ui`; reuse avoids any new Beacon type).
    target_lenses: RefCell<Option<Rc<RefCell<Vec<(String, DerivedProvider)>>>>>,
    /// Runtime-shared label allocator (W-29, existing `LabelAllocator`).
    label_allocator: RefCell<Option<Rc<RefCell<LabelAllocator>>>>,
    /// Per-generation label-to-command bindings (W-29, existing `BeaconDispatcher`).
    ///
    /// Built on session start, cleared on cancel/dispatch/unload. Never
    /// shared across generations and never published to the Event Bus.
    target_dispatcher: RefCell<BeaconDispatcher>,
    /// Overlay handle of this generation's active targeting session, if any.
    ///
    /// Ownership is the existing W-28 overlay capture owner (no new session
    /// type); this handle only coordinates dispatcher lifetime with capture
    /// release. `None` means no session.
    target_session_overlay: RefCell<Option<i64>>,
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
    debug_inspect: Cell<bool>,
    debug_trace: Cell<bool>,
    granted_capabilities: RefCell<Vec<String>>,
    declared_events: RefCell<BTreeSet<String>>,
    debug_view: RefCell<Option<Rc<RefCell<DebugView>>>>,
    trace_hub: RefCell<Option<Rc<RefCell<TraceHub>>>>,
    workspace_read: Cell<bool>,
    workspace_control: Cell<bool>,
    workspace_source: RefCell<Option<Rc<dyn WorkspaceSource>>>,
    workspace_requests: RefCell<Option<Rc<RefCell<WorkspaceRequestQueue>>>>,
    panel_create_granted: Cell<bool>,
    panel_focus_granted: Cell<bool>,
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
        Self {
            plugin_id: plugin_id.into(),
            store: RefCell::new(store),
            settings,
            snapshot,
            notifications,
            terminal_read,
            platform_notify,
            spawn_git: Cell::new(false),
            spawn_backend: RefCell::new(None),
            ui_access: RefCell::new(UiAccess::default()),
            ui_blocks: RefCell::new(UiBlocks::new()),
            overlay_capture: RefCell::new(None),
            overlay_peers: RefCell::new(None),
            spec_overlay_handles: RefCell::new(BTreeSet::new()),
            target_registry: RefCell::new(None),
            target_lenses: RefCell::new(None),
            label_allocator: RefCell::new(None),
            target_dispatcher: RefCell::new(BeaconDispatcher::new()),
            target_session_overlay: RefCell::new(None),
            env_grants: RefCell::new(BTreeSet::new()),
            env_source: RefCell::new(Rc::new(EmptyEnv)),
            safe_mode: Cell::new(false),
            service_provided: RefCell::new(Vec::new()),
            service_required: RefCell::new(Vec::new()),
            service_directory: RefCell::new(None),
            debug_inspect: Cell::new(false),
            debug_trace: Cell::new(false),
            granted_capabilities: RefCell::new(Vec::new()),
            declared_events: RefCell::new(BTreeSet::new()),
            debug_view: RefCell::new(None),
            trace_hub: RefCell::new(None),
            workspace_read: Cell::new(false),
            workspace_control: Cell::new(false),
            workspace_source: RefCell::new(None),
            workspace_requests: RefCell::new(None),
            panel_create_granted: Cell::new(false),
            panel_focus_granted: Cell::new(false),
        }
    }

    /// Grant `panel.create` and/or `panel.focus` from the activation snapshot
    /// (CTX-0915, Issue #1596). Independent grants: create allows panel
    /// creation, focus allows panel manipulation and state queries. Absent
    /// grants fail closed with `E_CAPABILITY_DENIED`.
    pub fn set_panel_access(&self, create: bool, focus: bool) {
        self.panel_create_granted.set(create);
        self.panel_focus_granted.set(focus);
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
        self.ui_blocks.borrow_mut().set_epoch(epoch);
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
            return;
        }
        let peers = self.overlay_peers.borrow().clone();
        let Some(peers) = peers else {
            return;
        };
        let weaks = peers.borrow().get(plugin).cloned().unwrap_or_default();
        for weak in weaks {
            if let Some(peer) = weak.upgrade() {
                if peer.remove_spec_overlay_block(handle) {
                    break;
                }
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

    /// Attach the runtime-shared targeting mechanism state (W-29, CTX-0942).
    ///
    /// The runtime wires one registry, lens set, and allocator across every
    /// generation so the provider set spans plugins and survives reload. A
    /// generation built without wiring keeps `None` and every targeting call
    /// fails closed with `E_UI_UNAVAILABLE`. Uses only existing `bitty-ui`
    /// types; no new Beacon type is introduced.
    pub fn set_targeting_state(
        &self,
        registry: Rc<RefCell<TargetRegistry>>,
        lenses: Rc<RefCell<Vec<(String, DerivedProvider)>>>,
        allocator: Rc<RefCell<LabelAllocator>>,
    ) {
        *self.target_registry.borrow_mut() = Some(registry);
        *self.target_lenses.borrow_mut() = Some(lenses);
        *self.label_allocator.borrow_mut() = Some(allocator);
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
        *self.target_dispatcher.borrow_mut() = BeaconDispatcher::new();
        *self.target_session_overlay.borrow_mut() = None;
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

    /// Build a fresh mediator from the Core provider plus shared lenses.
    ///
    /// The Core terminal provider is empty (no scrollback blocks are sourced
    /// in this thin slice; semantic derivation from Terminal Truth stays with
    /// `bitty.terminal.snapshot`); lenses are collected in registration order.
    /// Registration errors propagate with existing codes.
    fn targeting_mediator(&self) -> Result<ProviderMediator, BridgeError> {
        let lenses = self.require_target_lenses()?;
        let mut mediator = ProviderMediator::with_core(Vec::new());
        for (_, lens) in lenses.borrow().iter() {
            let rebuilt = DerivedProvider::new(lens.name(), lens.collect())
                .map_err(map_targets_provider_error)?;
            mediator
                .register(Box::new(rebuilt), true)
                .map_err(map_targets_provider_error)?;
        }
        Ok(mediator)
    }

    /// Inner session start after the W-28 capture was acquired (W-29).
    ///
    /// Collects a fresh snapshot, validates lengths, allocates labels,
    /// builds the annotation layer, and binds the dispatcher. All failures
    /// are typed with existing codes and leave no partial session; the caller
    /// releases capture on error. Nothing is published to the Event Bus.
    #[allow(clippy::type_complexity)]
    fn targets_session_start_inner(
        &self,
        width: u16,
        anchors: &[(u16, u16)],
        commands: &[String],
    ) -> Result<Vec<String>, BridgeError> {
        let registry = self.require_target_registry()?;
        let mediator = self.targeting_mediator()?;
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
        if anchors.len() > MAX_BEACON_TARGETS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many targets for labels: max {MAX_BEACON_TARGETS}"),
            ));
        }
        let allocator = self.require_label_allocator()?;
        let points: Vec<Point> = anchors.iter().map(|(x, y)| Point::new(*x, *y)).collect();
        let labels = allocator
            .borrow()
            .assign(&points, width)
            .map_err(map_targets_label_error)?;
        let targets: Vec<TargetRef> = entries.iter().map(|entry| entry.target()).collect();
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
        let height = points
            .iter()
            .map(|point| point.y)
            .max()
            .map_or(1, |y| y.saturating_add(1));
        let layer = BeaconAnnotationLayer::build(
            &targets,
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
        if layer.len() + self.target_dispatcher.borrow().len() > MAX_BEACON_BINDINGS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many bindings: max {MAX_BEACON_BINDINGS}"),
            ));
        }
        let mut dispatcher = BeaconDispatcher::new();
        dispatcher
            .bind_layer(&layer, &parsed)
            .map_err(map_targets_dispatch_error)?;
        *self.target_dispatcher.borrow_mut() = dispatcher;
        Ok(labels)
    }

    /// Invalidate every block handle this generation minted (suspend/dispose).
    ///
    /// The registry is host-side state: clearing it makes `bitty.ui.update`
    /// on a pre-suspend handle fail closed (`false`) after suspend and after
    /// resume until the plugin mounts again. Spec-acquired surfaces are
    /// covered: the whole registry (including every recorded spec handle)
    /// is dropped, so no orphan survives unload or crash.
    pub fn clear_ui_blocks(&self) {
        self.ui_blocks.borrow_mut().clear();
        self.spec_overlay_handles.borrow_mut().clear();
    }

    /// Drop one block by handle, releasing its text budget.
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

    /// Record this generation's own granted capability ids (activation
    /// snapshot) for the `grants` inspect target. Stored sorted and
    /// de-duplicated; other plugins' grants are never reachable.
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
        Ok(self.settings.get(key))
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
            // Accepted: `tabline` is an exclusive claim (ADR-0009 `LUA-OQ-7`
            // plus the shipped `[lazy].claims` vocabulary in
            // `bitty-plugin-host::bundled`). Unclaimed mounts fail closed;
            // the register/claim reservation is owned by the plugin host. The
            // shipped claim grammar canonicalizes the deprecated `tabline`
            // alias to `workspaceline`, so both spellings satisfy the slot.
            let tabline_claimed = access
                .claims
                .iter()
                .any(|claim| canonicalize_ui_claim(claim) == Some(WORKSPACELINE_CLAIM));
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
        // then silently dropped at render time.
        if let UiSlotPlacement::Unsupported(reason) = ui_slot_placement(ui_slot) {
            return Err(unsupported_slot_error(ui_slot, reason));
        }
        self.ui_blocks
            .borrow_mut()
            .mount(ui_slot, component.clone())
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

    #[allow(clippy::type_complexity)]
    fn ui_targets_snapshot(&self, max: usize) -> Result<Vec<(i64, String, String)>, BridgeError> {
        self.require_targets_capability()?;
        // Read-only: collect into a scratch registry, never the shared one
        // that live sessions bind their generations against.
        let _ = self.require_target_registry()?;
        let mut scratch = TargetRegistry::new();
        let mediator = self.targeting_mediator()?;
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
                let handle = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1);
                (
                    handle,
                    targets_kind_string(&entry.target()),
                    entry.tier().to_string(),
                )
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
        // exists in `bitty-ui`; reuse avoids any new Beacon type).
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
        if anchors.len() > MAX_BEACON_TARGETS {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("too many targets for labels: max {MAX_BEACON_TARGETS}"),
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
        match self
            .target_dispatcher
            .borrow()
            .dispatch(label, &registry.borrow())
        {
            Ok(command) => Ok(command.as_str().to_string()),
            Err(error) => Err(map_targets_dispatch_error(error)),
        }
    }

    fn service_provide_check(&self, iface: &str) -> Result<(), BridgeError> {
        if self.service_directory.borrow().is_none() {
            return Err(BridgeError::not_implemented("bitty.services.provide"));
        }
        if self
            .service_provided
            .borrow()
            .iter()
            .any(|service| service.iface == iface)
        {
            return Ok(());
        }
        Err(BridgeError::new(
            "validation",
            "E_SERVICE_UNDECLARED",
            format!("service '{iface}' is not declared in services.provided"),
        ))
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
        // but never substitutes for the declaration.
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
        // Deterministic pick over the live directory: highest satisfying
        // version wins, ties break by provider id. Unparseable requirements
        // or versions satisfy nothing (fail-closed, same grammar as the
        // policy registry gate).
        let mut best: Option<(Version, String, ServiceRecord)> = None;
        for record in directory.borrow().active_for(iface) {
            if !service_version_satisfies(&record.version, &effective) {
                continue;
            }
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
        let Some((_, _, record)) = best else {
            if optional {
                return Ok(None);
            }
            return Err(service_resolution_error(
                iface,
                format!("no provider satisfies '{effective}'"),
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
        method: &str,
        args: &LuaValue,
    ) -> Result<LuaValue, BridgeError> {
        let directory = self.service_directory.borrow().clone();
        let Some(directory) = directory else {
            return Err(BridgeError::not_implemented("bitty.services.get"));
        };
        // Snapshot the record under a short borrow: the directory borrow
        // must never span the (potentially re-entrant) provider VM call.
        let record = directory.borrow().find_active(iface, provider);
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

    fn panel_create(&self, panel_type: &str) -> Result<(u64, u64), BridgeError> {
        if !self.panel_create_granted.get() {
            return Err(BridgeError::capability_denied("panel.create"));
        }
        let _ = panel_type;
        Err(BridgeError::not_implemented("bitty.panel.create"))
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
    use bitty_lua::ui::UI_MAX_TEXT_BYTES;
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
        let error = focus_only.panel_get_state(1).expect_err("unimplemented");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
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
        let past = Instant::now();
        std::thread::sleep(Duration::from_millis(2));
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
        let services = env_services(&["HOME"], &[("HOME", "x")]);
        for key in ["", "has space", "9LIVES", "lower-ok?"] {
            let error = HostServices::env_get(&services, key).expect_err("shape must deny");
            assert_eq!(error.code, "E_DEF_INVALID", "key '{key}'");
        }
        let long = "A".repeat(ENV_KEY_MAX_BYTES + 1);
        let error = HostServices::env_has(&services, &long).expect_err("over-bound must deny");
        assert_eq!(error.code, "E_DEF_LIMIT");
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
            .ui_mount("statusline", &UiNode::text("x"))
            .expect_err("default access must deny");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        services.with_ui_blocks(|blocks| assert!(blocks.is_empty()));
    }

    #[test]
    fn ui_mount_rejects_unknown_slot_at_host_boundary() {
        let services = ui_services(rich_only());
        let error = services
            .ui_mount("nowhere", &UiNode::text("x"))
            .expect_err("unknown slot must fail closed");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    }

    #[test]
    fn overlay_slot_requires_ui_overlay() {
        let services = ui_services(rich_only());
        services
            .ui_mount("statusline", &UiNode::text("ok"))
            .expect("statusline mount");
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
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: false,
            overlay_focus: false,
            claims: vec!["tabline".to_string()],
        });
        // CTX-0923: the claim gate passes, but `tabline` is reserved for
        // PW-10 panel tabs and is not a band surface, so the mount fails
        // closed with the typed unsupported-slot error.
        let error = services
            .ui_mount("tabline", &UiNode::text("claimed"))
            .expect_err("claimed tabline is not hosted yet");
        assert_eq!(error.code, crate::E_UI_UNAVAILABLE);
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
        // top, bottom, left, right, statusline, overlay
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 6));
    }

    #[test]
    fn mount_update_round_trip_and_stale_handle() {
        let services = ui_services(rich_only());
        let handle = services
            .ui_mount("statusline", &UiNode::row(vec![UiNode::text("v1")]))
            .expect("mount");
        assert!(handle > 0);
        services.with_ui_blocks(|blocks| {
            let block = blocks.get(handle).expect("block retained");
            assert_eq!(block.slot(), "statusline");
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
            .ui_mount("statusline", &UiNode::text("gen1"))
            .expect("mount");
        let second = ui_services(rich_only());
        second.set_ui_epoch(8);
        let own = second
            .ui_mount("statusline", &UiNode::text("gen2"))
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
            .ui_mount("statusline", &UiNode::text("gen1"))
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
            .ui_mount("statusline", &UiNode::text("gen2"))
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
                .ui_mount("statusline", &UiNode::text(format!("{index}")))
                .expect("under the block cap");
        }
        let error = services
            .ui_mount("statusline", &UiNode::text("overflow"))
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
                .ui_mount("statusline", &UiNode::text(chunk.clone()))
                .expect("exactly at the aggregate cap");
        }
        let error = services
            .ui_mount("statusline", &UiNode::text("one byte over"))
            .expect_err("aggregate cap must fail closed");
        assert_eq!(error.code, "E_UI_BLOCK_BUDGET");
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.len(),
                UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES
            )
        });
    }

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
            }],
            vec![("calc.add".to_string(), ">=1.0".to_string())],
        );
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
        directory.borrow_mut().publish(ServiceRecord {
            provider: provider.to_string(),
            generation,
            iface: iface.to_string(),
            version: version.to_string(),
            methods: methods.iter().map(|name| (*name).to_string()).collect(),
            funcs: BTreeMap::new(),
            args_schema: None,
            result_schema: None,
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
    fn provide_check_without_directory_is_not_implemented() {
        let host = services();
        host.set_service_manifest(
            vec![ProvidedService {
                iface: "calc.add".to_string(),
                version: "1.0.0".to_string(),
                args_schema: None,
                result_schema: None,
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
    fn resolve_picks_highest_satisfying_version() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.old", 1, "calc.add", "1.2.0", &["add"]);
        publish_record(&directory, "xuepoo.new", 1, "calc.add", "1.5.0", &["add"]);
        publish_record(&directory, "xuepoo.next", 1, "calc.add", "2.0.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect("resolution serves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.next");
        assert_eq!(route.version, "2.0.0");
        assert_eq!(route.generation, 1);
        assert_eq!(route.methods, vec!["add".to_string()]);
    }

    #[test]
    fn resolve_tie_breaks_by_provider_id() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.b", 1, "calc.add", "1.2.0", &["add"]);
        publish_record(&directory, "xuepoo.a", 1, "calc.add", "1.2.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", None, false)
            .expect("resolution serves")
            .expect("route present");
        assert_eq!(route.provider, "xuepoo.a");
    }

    #[test]
    fn resolve_opts_version_overrides_manifest_req() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.old", 1, "calc.add", "1.2.0", &["add"]);
        publish_record(&directory, "xuepoo.new", 1, "calc.add", "1.5.0", &["add"]);
        let route = HostServices::service_resolve(&host, "calc.add", Some(">=1.2,<1.5"), false)
            .expect("resolution serves")
            .expect("route present");
        assert_eq!(route.version, "1.2.0");
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
    fn directory_publish_replaces_same_provider_record() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.0.0", &["add"]);
        publish_record(&directory, "xuepoo.calc", 2, "calc.add", "1.1.0", &["add"]);
        assert_eq!(directory.borrow().published_count(), 1);
        let record = directory
            .borrow()
            .find_active("calc.add", "xuepoo.calc")
            .expect("republished record present");
        assert_eq!(record.generation, 2);
        assert_eq!(record.version, "1.1.0");
    }

    #[test]
    fn call_without_directory_is_not_implemented() {
        let host = services();
        let error =
            HostServices::service_call(&host, "xuepoo.calc", 1, "calc.add", "add", &LuaValue::Nil)
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
        let error =
            HostServices::service_call(&host, "xuepoo.calc", 1, "calc.add", "add", &LuaValue::Nil)
                .expect_err("stale generation must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_unknown_method_is_gone() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        let error =
            HostServices::service_call(&host, "xuepoo.calc", 1, "calc.add", "sub", &LuaValue::Nil)
                .expect_err("unpublished method must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_suspended_provider_is_gone() {
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        directory.borrow_mut().suspend_provider("xuepoo.calc");
        let error =
            HostServices::service_call(&host, "xuepoo.calc", 1, "calc.add", "add", &LuaValue::Nil)
                .expect_err("suspended provider must be gone");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn call_dead_provider_is_gone() {
        // `Weak::new` records never upgrade: models a disposed generation
        // whose VM is dropped.
        let (directory, host) = service_host();
        publish_record(&directory, "xuepoo.calc", 1, "calc.add", "1.2.0", &["add"]);
        let error =
            HostServices::service_call(&host, "xuepoo.calc", 1, "calc.add", "add", &LuaValue::Nil)
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
    /// existing `bitty-ui` types; no new Beacon type is introduced.
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
                .expect_err("unknown")
                .code,
            "E_DEF_INVALID"
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
        let capture = Rc::new(RefCell::new(OverlayCapture::new()));
        let first_party = generation_services("bitty.first", 1, &capture);
        let out_of_tree = generation_services("beacon.ext", 2, &capture);
        first_party.set_targeting_state(
            Rc::clone(&registry),
            Rc::clone(&lenses),
            Rc::clone(&allocator),
        );
        out_of_tree.set_targeting_state(
            Rc::clone(&registry),
            Rc::clone(&lenses),
            Rc::clone(&allocator),
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
            .targeting_mediator()
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
