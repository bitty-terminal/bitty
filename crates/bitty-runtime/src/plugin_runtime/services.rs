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

use bitty_lua::ui::{UI_MAX_AGGREGATED_TEXT_BYTES, UI_MAX_BLOCKS, UiNode};
use bitty_lua::{
    BridgeError, HostServices, LuaValue, LuaVm, SNAPSHOT_MAX_BYTES, ServiceRoute, StashedFunction,
    validate_env_key,
};
use bitty_package::Version;
use bitty_plugin_host::bundled::{WORKSPACELINE_CLAIM, canonicalize_ui_claim};
use bitty_plugin_host::{ProvidedService, service_version_satisfies, value_satisfies_schema};

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
    /// Manifest `[lazy].claims` (exclusive slot claims; `tabline` only).
    pub claims: Vec<String>,
}

/// One mounted, generation-owned declarative block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiBlock {
    slot: String,
    node: UiNode,
    version: u32,
}

impl UiBlock {
    /// Accepted slot this block was mounted into.
    #[must_use]
    pub fn slot(&self) -> &str {
        &self.slot
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
    fn mount(&mut self, slot: &str, node: UiNode) -> Result<i64, BridgeError> {
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
                slot: slot.to_string(),
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

/// Per-generation host services for one plugin.
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
    env_grants: RefCell<BTreeSet<String>>,
    env_source: RefCell<Rc<dyn EnvSource>>,
    service_provided: RefCell<Vec<ProvidedService>>,
    service_required: RefCell<Vec<(String, String)>>,
    service_directory: RefCell<Option<Rc<RefCell<ServiceDirectory>>>>,
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
            env_grants: RefCell::new(BTreeSet::new()),
            env_source: RefCell::new(Rc::new(EmptyEnv)),
            service_provided: RefCell::new(Vec::new()),
            service_required: RefCell::new(Vec::new()),
            service_directory: RefCell::new(None),
        }
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

    /// Invalidate every block handle this generation minted (suspend/dispose).
    ///
    /// The registry is host-side state: clearing it makes `bitty.ui.update`
    /// on a pre-suspend handle fail closed (`false`) after suspend and after
    /// resume until the plugin mounts again.
    pub fn clear_ui_blocks(&self) {
        self.ui_blocks.borrow_mut().clear();
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

    /// Grant `bitty.env` reads for exact keys (CTX-0330).
    ///
    /// Set from the activation grant snapshot (`env.read:<KEY>` entries with
    /// the prefix stripped); absent grants fail closed at call time with
    /// `E_NOT_IMPLEMENTED`. Every key is shape-validated here — a malformed
    /// recorded grant fails the whole set rather than silently dropping —
    /// and the set is capped at [`MAX_ENV_GRANTS`].
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with `E_DEF_INVALID`/`E_DEF_LIMIT` when a key is
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
            validate_env_key(key)?;
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
        if !self.env_grants.borrow().contains(key) {
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
        if !self.env_grants.borrow().contains(key) {
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
        if !bitty_lua::ui::is_ui_slot(slot) {
            return Err(bitty_lua::ui::component_invalid(format!(
                "unknown UI slot '{slot}'"
            )));
        }
        {
            let access = self.ui_access.borrow();
            if !access.rich {
                return Err(BridgeError::capability_denied("ui.rich"));
            }
            if slot == "overlay" && !access.overlay {
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
            if slot == "tabline" && !tabline_claimed {
                return Err(BridgeError::new(
                    "validation",
                    "E_UI_CLAIM_REQUIRED",
                    "slot 'tabline' requires an exclusive claim declared in [lazy].claims",
                ));
            }
        }
        self.ui_blocks.borrow_mut().mount(slot, component.clone())
    }

    fn ui_update(&self, handle: i64, component: &UiNode) -> Result<bool, BridgeError> {
        if !self.ui_access.borrow().rich {
            return Err(BridgeError::capability_denied("ui.rich"));
        }
        self.ui_blocks
            .borrow_mut()
            .update(handle, component.clone())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::store::PluginStore;
    use bitty_lua::ENV_KEY_MAX_BYTES;
    use bitty_lua::ui::UI_MAX_TEXT_BYTES;

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
            claims: Vec::new(),
        }
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
            claims: Vec::new(),
        });
        services
            .ui_mount("overlay", &UiNode::text("allowed"))
            .expect("overlay mount after grant");
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 2));
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
            claims: vec!["tabline".to_string()],
        });
        services
            .ui_mount("tabline", &UiNode::text("claimed"))
            .expect("claimed tabline mount");
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: false,
            claims: vec!["workspaceline".to_string()],
        });
        services
            .ui_mount("tabline", &UiNode::text("canonical claim"))
            .expect("canonical workspaceline claim");
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
}
