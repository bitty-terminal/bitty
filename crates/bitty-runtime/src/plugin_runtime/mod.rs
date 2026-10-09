//! Plugin host runtime: per-plugin VM lifecycle, bridge, and host services.
//!
//! This module implements the `bitty-runtime` half of the ratified
//! `plugin-host-runtime-rfc` Gap A plus the minimal Gap C host services. Policy
//! stays in `bitty-plugin-host` (manifest validation, capability grammar,
//! grants, registry, event pipeline); the VM seam stays in `bitty-lua`; this
//! module orchestrates one `!Send` Phodopus VM per `(PluginId, generation)` on a
//! single owning thread.
//!
//! Lifecycle: `Unloaded -> Loading -> Activating -> Active -> Suspended ->
//! Disposing -> Disposed`, with `Failed` terminal for a generation. Activation
//! executes the fixed `init.lua`, captures registrations, validates them
//! against the manifest, and commits atomically. `bitty --safe` never creates a
//! third-party VM.
//!
//! Source resolution implements the ratified Gap B store
//! (`$XDG_DATA_HOME/bitty/plugins/`): the manifest body and Lua module tree are
//! staged under `packages/`, the atomic `current.json` pointer names the active
//! revision per plugin, and loading re-verifies `manifest_hash` and
//! `content_digest` fail-closed before any VM is created. Local-path
//! development packages are read-only, re-digested, and visibly unverified.
//! The numeric bounds below are the RFC's ratified defaults.

pub mod debug;
pub mod fs;
pub mod manifest_toml;
pub mod overlay;
pub mod package;
pub mod redaction;
pub mod resolution;
pub mod services;
pub mod spawn;
pub mod store;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::host::DEFAULT_HOST_DEADLINE_MS;
use bitty_lua::ui::UiNode;
use bitty_lua::{CommandRegistration, HostServices, LuaVm, MarshallingLimits, RegistrationCapture};
use bitty_plugin_host::DropPolicy;
use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::grant::GrantRecord;
use bitty_plugin_host::host::PluginHost;
use bitty_plugin_host::manifest::{LazyCommand, PluginId, PluginManifest};
use bitty_plugin_host::{validate_interface_schema, value_satisfies_schema};

pub use bitty_lua::{
    WORKSPACE_LIST_MAX_ITEMS, WORKSPACE_NAME_MAX_CHARS as WORKSPACE_INFO_NAME_MAX_CHARS,
    WORKSPACE_RENAME_MAX_BYTES, WorkspaceAttention, WorkspaceInfo, WorkspaceRequest,
};
pub use fs::{FakeFileSystem, FileSystem, NativeFileSystem, write_atomic_durably};
pub use overlay::{
    CRASHED_RELEASE_REASON, CapturePoll, DEFAULT_RELEASE_REASON, FOCUS_SWITCHED_RELEASE_REASON,
    OVERLAY_CAPTURE_TIMEOUT_MS, OWNER_RELEASE_REASONS, OverlayCapture, RELEASE_REASONS,
    ReleasedEvent, TIMEOUT_RELEASE_REASON, UNLOADED_RELEASE_REASON, is_owner_release_reason,
    is_release_reason,
};
pub use resolution::{
    CURRENT_POINTER_FILE, PLUGIN_INDEX_STATE_VERSION, PluginRecord, content_digest, load_index,
    load_index_with_fs, write_index, write_index_with_fs,
};
pub use services::{
    CommandDirectory, CommandRecord, EmptyEnv, EmptySettings, EnvSource, MAX_ENV_GRANTS,
    MAX_ENV_VALUE_BYTES, MapEnv, Notification, NotificationQueue, OverlayPeers, PluginServices,
    ProcessEnv, QueuedWorkspaceRequest, ServiceDirectory, ServiceRecord, SettingsSource,
    SnapshotSource, UiAccess, UiBlock, UiBlocks, UnavailableSnapshot, UnavailableWorkspaces,
    WorkspaceRequestQueue, WorkspaceSource,
};
pub use store::{KvCommitBackend, KvCommitError, PluginStore, STORE_FILE_MAX_BYTES};
// Bridge value/error types the host-service traits are expressed in, so the
// application can implement `SettingsSource`/`SnapshotSource` without taking a
// direct `bitty-lua` dependency.
pub use bitty_lua::{BridgeError, CommandCatalogEntry, LuaValue};

/// Maximum stored manifest body bytes (RFC proposed default).
pub const PLUGIN_MANIFEST_MAX_BYTES: usize = 256 * 1024;
/// Maximum files in one module tree (RFC proposed default).
pub const PLUGIN_MODULE_MAX_FILES: usize = 4096;
/// Maximum aggregate module tree bytes (RFC proposed default).
pub const PLUGIN_MODULE_TREE_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Maximum canonical module path bytes (RFC proposed default).
pub const PLUGIN_MODULE_PATH_MAX_BYTES: usize = 1024;
/// Maximum `init.lua` source bytes.
pub const PLUGIN_INIT_MAX_BYTES: usize = 1024 * 1024;
/// Notification queue capacity (`RC-8` rate governance candidate).
pub const NOTIFICATION_QUEUE_CAPACITY: usize = 64;

/// Bound on queued `bitty.workspace.*` mutation requests across all plugins
/// between two app ticks (CTX-0889).
///
/// Twice [`WORKSPACE_LIST_MAX_ITEMS`]: enough for a plugin to create or
/// close every workspace in one tick, small enough that a hostile loop
/// cannot queue unbounded work. Overflow drops the newest request and the
/// Lua call returns `false`.
pub const WORKSPACE_REQUEST_QUEUE_CAPACITY: usize = 2 * WORKSPACE_LIST_MAX_ITEMS;

/// Event-kind prefix of the workspace domain (CTX-0889, ADR-0014).
///
/// Kinds under this prefix carry workspace identity, so declaring,
/// subscribing to, tracing, or receiving them requires `workspace.read`.
pub const WORKSPACE_EVENT_PREFIX: &str = "workspace.";

/// Workspace event kinds the host publishes (CTX-0889). Spellings are
/// candidates pending OQ-056.
pub const WORKSPACE_EVENT_KINDS: &[&str] = &[
    "workspace.created",
    "workspace.closed",
    "workspace.renamed",
    "workspace.focused",
    "workspace.changed",
];

// The Lua-side workspace bounds mirror the Core slot-table bounds exactly;
// drift would let `bitty.workspace.list()` silently drop real workspaces.
const _: () = assert!(WORKSPACE_LIST_MAX_ITEMS == crate::MAX_WORKSPACES);
const _: () = assert!(WORKSPACE_INFO_NAME_MAX_CHARS == crate::WORKSPACE_NAME_MAX_CHARS);

/// Whether `kind` is a workspace-domain event kind (prefix match, so a
/// future kind under the prefix is gated before it is published).
#[must_use]
pub fn is_workspace_event(kind: &str) -> bool {
    kind.starts_with(WORKSPACE_EVENT_PREFIX)
}

/// Closed source-class set (RFC B.1): `bundled`, `registry`, `git`, `local-path`.
///
/// Provenance is derived from the resolved record, never from the manifest id.
/// Only [`SourceClass::Bundled`] is first-party; every other class is
/// third-party and is skipped by `--safe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceClass {
    /// Shipped alongside the application (read-only, first-party).
    Bundled,
    /// Resolved from a package registry and staged in the XDG store.
    Registry,
    /// Resolved from a Git source and staged in the XDG store.
    Git,
    /// Local-path development source; never treated as verified.
    LocalPath,
}

impl SourceClass {
    /// Stable lower-case label used by the persisted index record.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Registry => "registry",
            Self::Git => "git",
            Self::LocalPath => "local-path",
        }
    }

    /// Parse the persisted label; unknown labels fail closed.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "bundled" => Some(Self::Bundled),
            "registry" => Some(Self::Registry),
            "git" => Some(Self::Git),
            "local-path" => Some(Self::LocalPath),
            _ => None,
        }
    }

    /// Whether this is the first-party `bundled` class.
    #[must_use]
    pub fn is_bundled(self) -> bool {
        matches!(self, Self::Bundled)
    }

    /// Whether `--safe` must skip a VM for this class (RFC A.4 rule 6).
    #[must_use]
    pub fn is_third_party(self) -> bool {
        !self.is_bundled()
    }
}

/// Runtime lifecycle state for one `(PluginId, generation)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleState {
    /// Declared but no VM exists.
    Unloaded,
    /// VM and bridge being created.
    Loading,
    /// `init.lua` executing and registrations being captured.
    Activating,
    /// Activated and committed.
    Active,
    /// Suspended; VM retained with detached registrations.
    Suspended,
    /// Generation teardown in progress.
    Disposing,
    /// Generation memory and handles released.
    Disposed,
    /// Terminal failure for the generation.
    Failed(String),
}

/// One discoverable plugin package: manifest plus its Lua module root.
#[derive(Debug, Clone)]
pub struct PluginPackage {
    /// Parsed, validated manifest.
    pub manifest: PluginManifest,
    /// Directory containing the `lua/` module tree (`require` root).
    pub module_root: PathBuf,
    /// Source provenance class.
    pub source_class: SourceClass,
    /// Whether the package is visibly unverified (RFC B.5: always true for a
    /// `local-path` development source, including when re-digestion detects
    /// drift). Installed classes are verified or fail closed during discovery.
    pub unverified: bool,
    /// Capabilities consented for this exact manifest hash, when the package
    /// came from a resolved store record. `None` means the package's declared
    /// set is granted in full (bundled and development sources have no
    /// persisted grant record of their own).
    pub granted: Option<Vec<String>>,
}

/// Configuration for a [`PluginRuntime`].
pub struct PluginRuntimeConfig {
    /// `bitty --safe`: never create a third-party VM.
    pub safe_mode: bool,
    /// Root for plugin persistent state (`$XDG_DATA_HOME/bitty/plugins-state`).
    pub data_dir: Option<PathBuf>,
    /// Resolved plugin store root (`$XDG_DATA_HOME/bitty/plugins`).
    ///
    /// When set, discovery reads the atomic `current.json` pointer and
    /// re-verifies each enabled record's `manifest_hash` and `content_digest`
    /// before the package joins activation (RFC B.2/B.3). A missing store root
    /// resolves to no installed packages.
    pub store_root: Option<PathBuf>,
    /// Trusted roots for application-shipped (`bundled`) packages.
    ///
    /// Provenance is derived from the root, never from the manifest id: a
    /// package can only be `bundled` when it is discovered under one of these
    /// roots. `--safe` still loads these (RFC A.4 rule 6 only forbids
    /// third-party VMs).
    pub bundled_roots: Vec<PathBuf>,
    /// Untrusted development roots (for example the `BITTY_PLUGIN_DIR`
    /// override). Packages found here are [`SourceClass::LocalPath`] regardless
    /// of their self-declared id, are read-only, re-digested, and visibly
    /// unverified, and `--safe` never creates a VM for them.
    pub third_party_roots: Vec<PathBuf>,
    /// Read-only settings source.
    pub settings: Rc<dyn SettingsSource>,
    /// Bounded terminal snapshot source.
    pub snapshot: Rc<dyn SnapshotSource>,
}

impl Default for PluginRuntimeConfig {
    fn default() -> Self {
        Self {
            safe_mode: false,
            data_dir: None,
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: Vec::new(),
            settings: Rc::new(EmptySettings),
            snapshot: Rc::new(UnavailableSnapshot),
        }
    }
}

/// Outcome of one activation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationReport {
    /// Plugin id.
    pub plugin: PluginId,
    /// Resulting lifecycle state.
    pub state: LifecycleState,
    /// Captured command count.
    pub commands: usize,
    /// Captured event subscription count.
    pub events: usize,
    /// Whether `--safe` skipped a third-party plugin (no VM created).
    pub skipped_safe_mode: bool,
    /// Resolved source provenance class.
    pub source_class: SourceClass,
    /// Whether the package is visibly unverified (always true for a
    /// `local-path` development source; RFC B.5).
    pub unverified: bool,
}

/// Plugin runtime error, bounded and fail-closed.
#[derive(Debug)]
pub enum PluginRuntimeError {
    /// Manifest read/parse/validation failure.
    Manifest {
        /// Plugin or path identifier.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Module tree rejected (bounds, native artifact, traversal).
    ModuleTree {
        /// Plugin id.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Filesystem failure.
    Io(String),
    /// An installed source is missing (retained record with no package body).
    NotFound {
        /// Plugin id or path identifier.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Fail-closed integrity failure (hash, digest, path escape, consent).
    Integrity {
        /// Plugin id or path identifier.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Declared compatibility does not include the running host (CTX-0416).
    ///
    /// Surfaced at `resolve_record` and at `activate` before any VM is
    /// created, so an installed package whose range no longer includes the
    /// host after an upgrade fails closed instead of activating. Bundled
    /// packages skip this check: their `>=0.1` floor stays as written and the
    /// `0.0.x` dev host keeps loading them (DIR-019 versioning).
    Incompatible {
        /// Plugin id.
        plugin: String,
        /// Field that failed (`compat.bitty` or `compat.plugin-api`).
        field: String,
        /// Declared range.
        requested: String,
        /// Host version evaluated.
        host: String,
    },
    /// Lifecycle state violation.
    Lifecycle {
        /// Plugin id.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// `bitty-plugin-host` policy failure.
    Host(String),
    /// VM execution failure.
    Vm(String),
    /// Registration capture failed manifest validation.
    Capture {
        /// Plugin id.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Resource quota exceeded (budgets, queue ceilings, grant-set bounds).
    ///
    /// Quota-shaped rejections carry `E_BUDGET_*`/`E_DEF_LIMIT` codes via
    /// [`PluginRuntimeError::code`]; the counters are host-authored and
    /// bounded, never untrusted content.
    Budget {
        /// Plugin id or path identifier.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Operation exceeded its wall-clock deadline.
    ///
    /// Maps to `E_TIMEOUT`, the same code the bridge emits for host-call and
    /// spawn timeouts, so budget timeouts and bridge timeouts share one
    /// class and one code.
    Timeout {
        /// Plugin id.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// A hard count/size limit was exceeded.
    LimitExceeded {
        /// Plugin id or path identifier.
        plugin: String,
        /// Field or resource.
        field: String,
        /// Configured limit.
        limit: usize,
        /// Actual value.
        actual: usize,
    },
}

impl PluginRuntimeError {
    /// Stable `E_*` code for this failure (CTX-0330 taxonomy).
    ///
    /// Every variant maps to exactly one code so diagnostics, doctor
    /// surfaces, and Lua bridge errors stay joinable on a closed set.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Manifest { .. } => "E_MANIFEST",
            Self::ModuleTree { .. } => "E_MODULE_TREE",
            Self::Io(_) => "E_IO",
            Self::NotFound { .. } => "E_NOT_FOUND",
            Self::Integrity { .. } => "E_INTEGRITY",
            Self::Incompatible { .. } => "E_INCOMPATIBLE",
            Self::Lifecycle { .. } => "E_LIFECYCLE",
            Self::Host(_) => "E_HOST",
            Self::Vm(_) => "E_VM",
            Self::Capture { .. } => "E_CAPTURE",
            Self::Budget { .. } => "E_BUDGET_EXCEEDED",
            Self::Timeout { .. } => "E_TIMEOUT",
            Self::LimitExceeded { .. } => "E_DEF_LIMIT",
        }
    }

    /// Diagnostic class for this failure (CTX-0330 taxonomy).
    ///
    /// Mirrors the bridge classes: `validation` for malformed/over-bound
    /// input, `budget` for quota and time failures, `runtime` for
    /// lifecycle, host, and VM failures.
    #[must_use]
    pub fn error_class(&self) -> &'static str {
        match self {
            Self::Manifest { .. }
            | Self::ModuleTree { .. }
            | Self::LimitExceeded { .. }
            | Self::Incompatible { .. } => "validation",
            Self::Budget { .. } | Self::Timeout { .. } => "budget",
            Self::Io(_)
            | Self::NotFound { .. }
            | Self::Integrity { .. }
            | Self::Lifecycle { .. }
            | Self::Host(_)
            | Self::Vm(_)
            | Self::Capture { .. } => "runtime",
        }
    }
}

impl std::fmt::Display for PluginRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest { plugin, detail } => {
                write!(f, "manifest error for '{plugin}': {detail}")
            }
            Self::ModuleTree { plugin, detail } => {
                write!(f, "module tree error for '{plugin}': {detail}")
            }
            Self::Io(detail) => write!(f, "plugin runtime I/O: {detail}"),
            Self::NotFound { plugin, detail } => {
                write!(f, "plugin '{plugin}' source not found: {detail}")
            }
            Self::Integrity { plugin, detail } => {
                write!(f, "plugin '{plugin}' integrity failure: {detail}")
            }
            Self::Incompatible {
                plugin,
                field,
                requested,
                host,
            } => write!(
                f,
                "'{plugin}' declares {field} = '{requested}', which does not include host version {host}"
            ),
            Self::Lifecycle { plugin, detail } => {
                write!(f, "plugin '{plugin}' lifecycle error: {detail}")
            }
            Self::Host(detail) => write!(f, "plugin host error: {detail}"),
            Self::Vm(detail) => write!(f, "plugin vm error: {detail}"),
            Self::Capture { plugin, detail } => {
                write!(f, "registration capture error for '{plugin}': {detail}")
            }
            Self::Budget { plugin, detail } => {
                write!(f, "plugin '{plugin}' budget exceeded: {detail}")
            }
            Self::Timeout { plugin, detail } => {
                write!(f, "plugin '{plugin}' timed out: {detail}")
            }
            Self::LimitExceeded {
                plugin,
                field,
                limit,
                actual,
            } => {
                write!(
                    f,
                    "plugin '{plugin}' {field}: limit {limit} exceeded (actual {actual})"
                )
            }
        }
    }
}

impl std::error::Error for PluginRuntimeError {}

impl From<bitty_plugin_host::PluginError> for PluginRuntimeError {
    fn from(error: bitty_plugin_host::PluginError) -> Self {
        Self::Host(error.to_string())
    }
}

struct PluginEntry {
    package: PluginPackage,
    generation: u32,
    state: LifecycleState,
    vm: Option<Rc<RefCell<LuaVm>>>,
    services: Option<Rc<PluginServices>>,
    registrations: RegistrationCapture,
}

/// Orchestrates discovery, activation, and dispatch for plugin VMs.
///
/// The struct is `!Send` because it owns Phodopus VMs; callers use it on the
/// single owning thread (the application's cold path).
pub struct PluginRuntime {
    safe_mode: bool,
    data_dir: Option<PathBuf>,
    store_root: Option<PathBuf>,
    host: PluginHost,
    settings: Rc<dyn SettingsSource>,
    snapshot: Rc<dyn SnapshotSource>,
    notifications: Rc<RefCell<NotificationQueue>>,
    bundled_roots: Vec<PathBuf>,
    third_party_roots: Vec<PathBuf>,
    event_sequence: u64,
    entries: BTreeMap<PluginId, PluginEntry>,
    order: Vec<PluginId>,
    service_directory: Rc<RefCell<ServiceDirectory>>,
    /// CTX-1035: runtime-shared command directory for the generic
    /// host-mediated invocation path (`bitty.commands.list`/`invoke`,
    /// keybinding `command:<qualified>`, palette selection). Shared with
    /// every generation's services so any plugin can invoke any active
    /// command through the host; activation publishes, suspend parks,
    /// resume restores, dispose/reload/rollback revokes.
    command_directory: Rc<RefCell<CommandDirectory>>,
    /// CTX-0897: sanitized lifecycle/registration snapshot shared with every
    /// generation's services for `bitty.debug.inspect`; rebuilt by
    /// [`PluginRuntime::sync_debug_view`] after each lifecycle transition.
    debug_view: Rc<RefCell<debug::DebugView>>,
    /// CTX-0897: per-owner event traces for `bitty.debug.trace`, fed by
    /// [`PluginRuntime::deliver_event`].
    trace_hub: Rc<RefCell<debug::TraceHub>>,
    /// CTX-0889: optional workspace read source for `bitty.workspace.list`.
    /// `None` leaves granted reads failing closed with `E_NOT_IMPLEMENTED`.
    workspace_source: Option<Rc<dyn WorkspaceSource>>,
    /// CTX-0889: bounded queue of `workspace.control` mutations, drained by
    /// the application each tick ([`PluginRuntime::drain_workspace_requests`]).
    workspace_requests: Rc<RefCell<WorkspaceRequestQueue>>,
    /// CTX-0941: runtime-shared single-owner focusable-overlay capture switch
    /// and bounded input queue. Shared with every generation's services so the
    /// single-owner invariant holds across plugins and reload.
    overlay_capture: Rc<RefCell<OverlayCapture>>,
    /// CTX-0973: runtime-shared overlay peer table for lazy-expiry disposal.
    /// Shared with every generation's services so an acquire that lazily
    /// expires another generation's session can dispose that session's
    /// transient spec surface synchronously.
    overlay_peers: services::OverlayPeers,
    /// W-29 (CTX-0942): runtime-shared target registry (existing
    /// `TargetRegistry`). One registry spans every generation so generations
    /// bump and stale handles by construction.
    target_registry: Rc<RefCell<bitty_ui::TargetRegistry>>,
    /// W-29: runtime-shared generic lenses (existing `DerivedProvider` as
    /// `(plugin_id, lens)`). The provider set spans plugins and survives
    /// reload; suspend/dispose revoke one generation's lenses.
    #[allow(clippy::type_complexity)]
    target_lenses: Rc<RefCell<Vec<(String, bitty_ui::DerivedProvider)>>>,
    /// W-29: runtime-shared label allocator (existing `LabelAllocator`).
    label_allocator: Rc<RefCell<bitty_ui::LabelAllocator>>,
    /// Durable-commit backend behind disk-backed plugin stores (`data_dir`).
    ///
    /// `None` by default (no durable store configured): stores opened while
    /// no backend is installed fail closed on write and load clean and
    /// empty. Replaceable through [`PluginRuntime::set_store_backend`] so
    /// latency and failure can be injected deterministically (bitty #1518).
    store_backend: Option<Arc<dyn store::KvCommitBackend>>,
    /// Test-only wall-clock override for the activation VM in milliseconds.
    ///
    /// `None` by default (production): activation uses
    /// [`VmBudgets::default`] (RC-1 `RC1_WALL_CLOCK_BUDGET_MS`, 50ms).
    /// `Some(ms)` widens only the wall dimension for that activation;
    /// instruction and memory stay at RC defaults. Used by load-sensitive
    /// integration tests (bitty#1744, same treatment as bitty#1727);
    /// production code never sets this.
    vm_wall_budget_ms: Option<u64>,
}

impl PluginRuntime {
    /// Create an empty runtime.
    #[must_use]
    pub fn new(config: PluginRuntimeConfig) -> Self {
        Self {
            safe_mode: config.safe_mode,
            data_dir: config.data_dir,
            store_root: config.store_root,
            host: PluginHost::new(DropPolicy::DropOldest, crate::DEFAULT_PLUGIN_SIDE_CAPACITY),
            settings: config.settings,
            snapshot: config.snapshot,
            notifications: Rc::new(RefCell::new(NotificationQueue::new(
                NOTIFICATION_QUEUE_CAPACITY,
            ))),
            bundled_roots: config.bundled_roots,
            third_party_roots: config.third_party_roots,
            event_sequence: 0,
            entries: BTreeMap::new(),
            order: Vec::new(),
            service_directory: Rc::new(RefCell::new(ServiceDirectory::new())),
            command_directory: Rc::new(RefCell::new(CommandDirectory::new())),
            debug_view: Rc::new(RefCell::new(debug::DebugView::new())),
            trace_hub: Rc::new(RefCell::new(debug::TraceHub::new())),
            workspace_source: None,
            workspace_requests: Rc::new(RefCell::new(WorkspaceRequestQueue::new(
                WORKSPACE_REQUEST_QUEUE_CAPACITY,
            ))),
            overlay_capture: Rc::new(RefCell::new(OverlayCapture::new())),
            overlay_peers: Rc::new(RefCell::new(BTreeMap::new())),
            target_registry: Rc::new(RefCell::new(bitty_ui::TargetRegistry::new())),
            target_lenses: Rc::new(RefCell::new(Vec::new())),
            label_allocator: Rc::new(RefCell::new(bitty_ui::LabelAllocator::default())),
            store_backend: None,
            vm_wall_budget_ms: None,
        }
    }

    /// Replace the durable-commit backend used by disk-backed plugin stores.
    ///
    /// Applies to stores opened by later activations. The backend only
    /// changes how `store.json` is read and committed; quotas, validation,
    /// and the RC-1 accounting stay in Core.
    pub fn set_store_backend(&mut self, backend: Option<Arc<dyn store::KvCommitBackend>>) {
        self.store_backend = backend;
    }

    /// Override the activation VM wall-clock budget (test-only).
    ///
    /// `None` restores the production default (`VmBudgets::default`, 50ms
    /// wall). `Some(ms)` widens only the wall dimension; instruction and
    /// memory stay at RC defaults. Load-sensitive integration tests use this
    /// to absorb scheduler stalls under sharded CI load (bitty#1744 mirrors
    /// the bitty#1727 driver treatment); production never calls this.
    pub fn set_vm_wall_budget_ms(&mut self, wall_budget_ms: Option<u64>) {
        self.vm_wall_budget_ms = wall_budget_ms;
    }

    /// Whether safe mode is enabled.
    #[must_use]
    pub fn safe_mode(&self) -> bool {
        self.safe_mode
    }

    /// Number of discovered packages.
    #[must_use]
    pub fn package_count(&self) -> usize {
        self.entries.len()
    }

    /// Discovered plugin ids in discovery order.
    #[must_use]
    pub fn discovered_ids(&self) -> Vec<PluginId> {
        self.order.clone()
    }

    /// Lifecycle state for `id`, if discovered.
    #[must_use]
    pub fn state(&self, id: &PluginId) -> Option<&LifecycleState> {
        self.entries.get(id).map(|entry| &entry.state)
    }

    /// Captured registrations for `id`, if activated.
    #[must_use]
    pub fn registrations(&self, id: &PluginId) -> Option<&RegistrationCapture> {
        self.entries.get(id).map(|entry| &entry.registrations)
    }

    /// Services handle for `id`, if activated (store/diagnostics access).
    #[must_use]
    pub fn services(&self, id: &PluginId) -> Option<&Rc<PluginServices>> {
        self.entries
            .get(id)
            .and_then(|entry| entry.services.as_ref())
    }

    /// Iterate over all mounted UI blocks across all activated plugins (CTX-0892).
    ///
    /// Returns (plugin_id, slot, node, version) tuples. Order is discovery
    /// order; band placement and stacking are owned by
    /// [`crate::ChromeBands::from_mounts`]. Read-only access.
    pub fn ui_blocks(&self) -> Vec<(PluginId, bitty_lua::ui::UiSlot, UiNode, u32)> {
        let mut result = Vec::new();
        for id in &self.order {
            if let Some(entry) = self.entries.get(id) {
                if let Some(svc) = &entry.services {
                    svc.with_ui_blocks(|blocks| {
                        for (_handle, block) in blocks.iter() {
                            result.push((
                                id.clone(),
                                block.ui_slot(),
                                block.node().clone(),
                                block.version(),
                            ));
                        }
                    });
                }
            }
        }
        result
    }

    /// Drain accepted notifications (async hand-off side).
    pub fn drain_notifications(&mut self) -> Vec<Notification> {
        self.notifications.borrow_mut().drain()
    }

    /// Notifications dropped by queue overflow since creation (CTX-1033,
    /// issue #1827): the application reports the delta across ticks so a
    /// saturated `platform.notify` producer stays observable.
    #[must_use]
    pub fn notifications_dropped(&self) -> u64 {
        self.notifications.borrow().dropped()
    }

    /// Install the workspace read source for `bitty.workspace.list`
    /// (CTX-0889). Call before activation: each generation captures the
    /// source when its services are built.
    pub fn set_workspace_source(&mut self, source: Rc<dyn WorkspaceSource>) {
        self.workspace_source = Some(source);
    }

    /// Drain queued `bitty.workspace.*` mutations in FIFO order (CTX-0889).
    ///
    /// The application applies each request on its own thread through the
    /// Core handlers the keybindings use. Bounded by
    /// [`WORKSPACE_REQUEST_QUEUE_CAPACITY`].
    pub fn drain_workspace_requests(&mut self) -> Vec<QueuedWorkspaceRequest> {
        self.workspace_requests.borrow_mut().drain()
    }

    /// Workspace requests dropped by queue overflow since creation.
    #[must_use]
    pub fn workspace_requests_dropped(&self) -> u64 {
        self.workspace_requests.borrow().dropped()
    }

    /// Runtime-shared focusable-overlay capture switch (CTX-0941).
    ///
    /// One owner and one bounded queue span every generation, so the
    /// single-owner invariant holds across plugins and reload. Diagnostics and
    /// tests read the live owner/queue through this handle.
    #[must_use]
    pub fn overlay_capture(&self) -> &Rc<RefCell<OverlayCapture>> {
        &self.overlay_capture
    }

    /// Runtime-shared targeting mechanism state (W-29, CTX-0942).
    ///
    /// One registry, lens set, and allocator span every generation, so the
    /// provider set survives reload and handles stale across sessions by
    /// construction. Uses only existing `bitty-ui` types. Diagnostics and
    /// tests read the live state through these handles.
    #[must_use]
    pub fn target_registry(&self) -> &Rc<RefCell<bitty_ui::TargetRegistry>> {
        &self.target_registry
    }

    /// Runtime-shared generic lenses (W-29, existing `DerivedProvider`).
    #[allow(clippy::type_complexity)]
    #[must_use]
    pub fn target_lenses(&self) -> &Rc<RefCell<Vec<(String, bitty_ui::DerivedProvider)>>> {
        &self.target_lenses
    }

    /// Runtime-shared label allocator (W-29, existing `LabelAllocator`).
    #[must_use]
    pub fn label_allocator(&self) -> &Rc<RefCell<bitty_ui::LabelAllocator>> {
        &self.label_allocator
    }

    /// Enqueue one captured input event for the active overlay capture, if any.
    ///
    /// This is the Core input-path entry: it appends to the capture queue and
    /// never invokes plugin code (`P0-AC-015`). Returns `false` when no capture
    /// is active, so input falls through to the terminal as before.
    pub fn push_overlay_input(&mut self, kind: &str, text: &str) -> bool {
        self.overlay_capture.borrow_mut().enqueue(kind, text)
    }

    /// Enqueue a pointer-motion event with trailing-motion coalescing (see
    /// [`OverlayCapture::enqueue_move`]).
    pub fn push_overlay_move(&mut self, text: &str) -> bool {
        self.overlay_capture.borrow_mut().enqueue_move(text)
    }

    /// Absolute monotonic deadline of the active overlay capture, if any
    /// (application idle-wake arming; CodeRabbit PR #1643).
    pub fn overlay_capture_deadline(&self) -> Option<std::time::Instant> {
        self.overlay_capture.borrow().expiry_deadline()
    }

    /// Revoke the active overlay capture unconditionally (focus switch,
    /// cancel, or an application-side release).
    ///
    /// Returns whether a capture was dropped. Release is guaranteed and
    /// idempotent: a call with no active capture is a no-op `false`.
    /// Records the `focus_switched` reason for the owner's next poll.
    pub fn revoke_overlay_capture(&mut self) -> bool {
        self.revoke_overlay_capture_with_reason(FOCUS_SWITCHED_RELEASE_REASON)
    }

    /// Revoke the active overlay capture with an explicit terminal reason
    /// (one of the accepted W-01 release reasons).
    ///
    /// Used by the application to attribute the release path: `focus_switched`
    /// for focus moves, `cancelled` for user cancel. Returns whether a
    /// capture was dropped.
    pub fn revoke_overlay_capture_with_reason(&mut self, reason: &str) -> bool {
        let Some((plugin, handle)) = self.live_capture_session() else {
            return false;
        };
        let revoked = self
            .overlay_capture
            .borrow_mut()
            .revoke_plugin_with_reason(&plugin, reason);
        if revoked {
            self.drop_ended_spec_overlay(&plugin, handle);
        }
        revoked
    }

    /// Live capture session as `(owner plugin, handle)`, if any.
    fn live_capture_session(&self) -> Option<(String, i64)> {
        let capture = self.overlay_capture.borrow();
        Some((capture.owner_plugin()?.to_string(), capture.owner_handle()?))
    }

    /// Dispose the spec-acquired surface of an ended session (CTX-0941).
    ///
    /// Runtime-driven session ends (expiry, focus-switch/cancel revoke) drop
    /// the capture without passing through the owning generation's release
    /// path; the transient surface must still leave with the session or the
    /// next session would present stale content and every cycle would leak
    /// one block slot. Unload/crash/suspend need no handling here: those
    /// paths invalidate the whole generation registry. Best-effort: a
    /// generation gone by cleanup time simply has nothing to dispose.
    fn drop_ended_spec_overlay(&self, plugin: &str, handle: i64) {
        let Ok(id) = PluginId::new(plugin) else {
            return;
        };
        if let Some(services) = self.services(&id) {
            services.remove_spec_overlay_block(handle);
        }
    }

    /// Drain pending `overlay.released` bus observations in order.
    ///
    /// The application delivers each entry via the event pipeline on its
    /// cold path (never on the input hot path): any subscriber may observe
    /// session end without polling, and no phase may intercept or veto it.
    pub fn drain_overlay_released(&mut self) -> Vec<ReleasedEvent> {
        self.overlay_capture.borrow_mut().drain_released_events()
    }

    /// Revoke an overlay capture that has reached its Core-side deadline at
    /// `now`, returning whether one was dropped.
    ///
    /// The deterministic entry point for the transient/bounded guarantee (the
    /// application calls [`Self::expire_overlay_captures`] each tick).
    pub fn expire_overlay_captures_at(&mut self, now: Instant) -> bool {
        let session = self.live_capture_session();
        let expired = self.overlay_capture.borrow_mut().revoke_expired_at(now);
        if expired {
            if let Some((plugin, handle)) = session {
                self.drop_ended_spec_overlay(&plugin, handle);
            }
        }
        expired
    }

    /// Revoke any overlay capture past its deadline at the current instant.
    pub fn expire_overlay_captures(&mut self) -> bool {
        self.expire_overlay_captures_at(Instant::now())
    }

    /// Scan the configured roots and register every valid package.
    ///
    /// Order is fixed so provenance is deterministic: first-party `bundled`
    /// roots, then installed packages resolved through the XDG store's atomic
    /// `current.json` pointer (RFC B.2/B.3), then untrusted development
    /// (`local-path`) roots. Every source fails closed: an invalid manifest or
    /// an integrity mismatch is reported and skipped, never loaded, and the
    /// host never falls back to a different revision or to bundled content.
    /// Returns `(id, result)` pairs in discovery order.
    pub fn discover(&mut self) -> Vec<(PluginId, Result<(), PluginRuntimeError>)> {
        let results = self.discover_inner();
        self.sync_debug_view();
        results
    }

    fn discover_inner(&mut self) -> Vec<(PluginId, Result<(), PluginRuntimeError>)> {
        let mut results = Vec::new();

        for root in self.bundled_roots.clone() {
            self.collect_dir_root(&root, SourceClass::Bundled, &mut results);
        }

        // RFC A.4 rule 6 / R-009: `--safe` never reads the third-party store
        // tree, so installed records are not even enumerated; only the trusted
        // bundled roots scanned above may load.
        let store_root = if self.safe_mode {
            None
        } else {
            self.store_root.clone()
        };
        if let Some(store_root) = store_root {
            match resolution::load_index(&store_root) {
                Ok(records) => {
                    for record in records {
                        if !record.enabled {
                            continue;
                        }
                        let Ok(id) = PluginId::new(&record.plugin_id) else {
                            results.push((
                                invalid_plugin_id(),
                                Err(PluginRuntimeError::Integrity {
                                    plugin: record.plugin_id.clone(),
                                    detail: "record plugin_id is not a valid plugin id".to_string(),
                                }),
                            ));
                            continue;
                        };
                        if self.entries.contains_key(&id) {
                            continue;
                        }
                        match resolution::resolve_record(&store_root, &record) {
                            Ok(package) => {
                                let id = package.manifest.id().clone();
                                if self.entries.contains_key(&id) {
                                    continue;
                                }
                                self.insert_package(id.clone(), package);
                                results.push((id, Ok(())));
                            }
                            Err(error) => results.push((id, Err(error))),
                        }
                    }
                }
                Err(error) => results.push((invalid_plugin_id(), Err(error))),
            }
        }

        for root in self.third_party_roots.clone() {
            self.collect_dir_root(&root, SourceClass::LocalPath, &mut results);
        }

        results
    }

    fn collect_dir_root(
        &mut self,
        root: &Path,
        source_class: SourceClass,
        results: &mut Vec<(PluginId, Result<(), PluginRuntimeError>)>,
    ) {
        let packages = match discover_root(root, source_class) {
            Ok(packages) => packages,
            Err(error) => {
                results.push((invalid_plugin_id(), Err(error)));
                return;
            }
        };
        for package in packages {
            let id = package.manifest.id().clone();
            if self.entries.contains_key(&id) {
                continue;
            }
            self.insert_package(id.clone(), package);
            results.push((id, Ok(())));
        }
    }

    fn insert_package(&mut self, id: PluginId, package: PluginPackage) {
        self.entries.insert(
            id.clone(),
            PluginEntry {
                package,
                generation: 0,
                state: LifecycleState::Unloaded,
                vm: None,
                services: None,
                registrations: RegistrationCapture::new(),
            },
        );
        self.order.push(id);
    }

    /// Activate one discovered plugin.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError`] for lifecycle, manifest, module-tree, host,
    /// capture-validation, or VM failures. Failure leaves no partial
    /// activation.
    pub fn activate(&mut self, id: &PluginId) -> Result<ActivationReport, PluginRuntimeError> {
        let result = self.activate_inner(id);
        self.sync_debug_view();
        result
    }

    fn activate_inner(&mut self, id: &PluginId) -> Result<ActivationReport, PluginRuntimeError> {
        let (manifest, module_root, init_path, recorded_grant, source_class) = {
            let entry = self
                .entries
                .get(id)
                .ok_or_else(|| PluginRuntimeError::Lifecycle {
                    plugin: id.to_string(),
                    detail: "plugin not discovered".to_string(),
                })?;
            if !matches!(
                entry.state,
                LifecycleState::Unloaded | LifecycleState::Disposed | LifecycleState::Failed(_)
            ) {
                return Err(PluginRuntimeError::Lifecycle {
                    plugin: id.to_string(),
                    detail: format!("cannot activate from state {:?}", entry.state),
                });
            }
            // `--safe` is decided by discovery provenance, never by the
            // self-declared id: a third-party package cannot claim bundled
            // trust by naming itself `bitty.*` (RFC A.4 rule 6). This check
            // precedes any VM or host-reservation work.
            if self.safe_mode && entry.package.source_class.is_third_party() {
                return Ok(ActivationReport {
                    plugin: id.clone(),
                    state: LifecycleState::Unloaded,
                    commands: 0,
                    events: 0,
                    skipped_safe_mode: true,
                    source_class: entry.package.source_class,
                    unverified: entry.package.unverified,
                });
            }
            verify_module_tree(id, &entry.package.module_root)?;
            let init_path = entry_point(&entry.package.module_root, id).ok_or_else(|| {
                PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: "no init.lua entry point found".to_string(),
                }
            })?;
            (
                entry.package.manifest.clone(),
                entry.package.module_root.clone(),
                init_path,
                entry.package.granted.clone(),
                entry.package.source_class,
            )
        };
        // CTX-0416 host-upgrade re-check at activation: the closed compat
        // grammar is evaluated against the running host before any policy or
        // VM work. Bundled packages skip (their `>=0.1` floor stays and the
        // `0.0.x` dev host keeps loading them per DIR-019); every other class
        // fails closed with a typed incompatible state and no VM.
        if !source_class.is_bundled() {
            if let Err(error) = resolution::check_host_compat(
                &manifest,
                package::host_bitty_version(),
                package::host_api_version(),
            ) {
                self.rollback(id, error.to_string());
                return Err(error);
            }
        }
        let source = read_init(&init_path)?;

        // Policy half: declare, resolve, register, grant, activate. The grant
        // is the intersection of the manifest's declared capabilities with the
        // consented record (RFC B.4: grants are bound to the manifest hash);
        // development and bundled packages without a record grant in full.
        let hash = manifest.manifest_hash();
        let declared =
            manifest
                .capabilities
                .all_ids()
                .map_err(|error| PluginRuntimeError::Manifest {
                    plugin: id.to_string(),
                    detail: error.to_string(),
                })?;
        let granted = effective_granted(id, &declared, recorded_grant.as_deref())?;
        let policy = (|| -> Result<(), PluginRuntimeError> {
            self.host.declare(manifest.clone())?;
            self.host.resolve(id)?;
            self.host.register(id)?;
            self.host
                .insert_grant(GrantRecord::granted(id.clone(), hash, granted.clone(), 0));
            self.host.activate(id)?;
            Ok(())
        })();
        if let Err(error) = policy {
            self.rollback(id, error.to_string());
            return Err(error);
        }

        // Mechanism half: VM, bridge, and bounded activation. Every failure
        // below goes through `self.rollback`, which purges the policy-host
        // generation (identity, command ownership, subscriptions) and records
        // a terminal failure locally, so no partial activation survives and a
        // retry starts from a clean host state (RFC A.4 rule 4).
        let store = match self.open_store(id) {
            Ok(store) => store,
            Err(error) => {
                self.rollback(id, error.to_string());
                return Err(error);
            }
        };
        let terminal_read = granted
            .iter()
            .any(|capability| capability.as_str() == "terminal.semantic-read");
        let platform_notify = granted
            .iter()
            .any(|capability| capability.as_str() == "platform.notify");
        let spawn_git = granted
            .iter()
            .any(|capability| capability.as_str() == "process.spawn:git");
        let ui_rich = granted
            .iter()
            .any(|capability| capability.as_str() == "ui.rich");
        let ui_overlay = granted
            .iter()
            .any(|capability| capability.as_str() == "ui.overlay");
        // CTX-0941 (accepted W-01 v2): the focusable overlay and transient
        // input-capture surface requires `ui.overlay.focus`. Distinct from
        // v1 `ui.overlay` (presentation-only, non-focusable); no implication
        // either way and no wildcard.
        let ui_overlay_focus = granted
            .iter()
            .any(|capability| capability.as_str() == "ui.overlay.focus");
        let debug_inspect = granted
            .iter()
            .any(|capability| capability.as_str() == "debug.inspect");
        let debug_trace = granted
            .iter()
            .any(|capability| capability.as_str() == "debug.trace");
        let panel_create = granted
            .iter()
            .any(|capability| capability.as_str() == "panel.create");
        let panel_focus = granted
            .iter()
            .any(|capability| capability.as_str() == "panel.focus");
        // CTX-0889: the workspace domain grants are independent (read never
        // implies control, control never implies read).
        let workspace_read = granted
            .iter()
            .any(|capability| capability.as_str() == "workspace.read");
        let workspace_control = granted
            .iter()
            .any(|capability| capability.as_str() == "workspace.control");
        // CTX-0889 fail-closed: declaring a `workspace.*` event kind (which
        // admits both subscription and `debug.trace` observation) requires
        // `workspace.read`. Reject before any VM exists instead of letting
        // the subscription silently never fire.
        if !workspace_read {
            if let Some(kind) = manifest
                .lazy
                .events
                .iter()
                .find(|kind| is_workspace_event(kind))
            {
                let error = PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!("event '{kind}' requires the 'workspace.read' capability"),
                };
                self.rollback(id, error.to_string());
                return Err(error);
            }
        }
        let plugin_services = Rc::new(PluginServices::new(
            id.as_str(),
            store,
            self.settings.clone(),
            self.snapshot.clone(),
            self.notifications.clone(),
            terminal_read,
            platform_notify,
        ));
        // CTX-0428: accepted Plugin API v1 `ui.mount`/`ui.update` gates. The
        // grant snapshot decides `ui.rich`/`ui.overlay`; the manifest's
        // `[lazy].claims` list decides the exclusive `tabline` slot. Defaults
        // stay closed, so a missing setter call can never widen authority.
        plugin_services.set_ui_access(UiAccess {
            rich: ui_rich,
            overlay: ui_overlay,
            overlay_focus: ui_overlay_focus,
            claims: manifest.lazy.claims.clone(),
        });
        // CTX-0941: safe mode never presents a focusable overlay. The flag
        // travels with the generation so every capture call fails closed
        // with `E_UI_UNAVAILABLE` while set.
        plugin_services.set_safe_mode(self.safe_mode);
        // CTX-0941: the one runtime-shared focusable-overlay capture switch,
        // so the single-owner invariant spans every plugin and survives
        // reload; suspend/dispose revoke this generation's capture.
        plugin_services.set_overlay_capture(self.overlay_capture.clone());
        // CTX-0973: the runtime-shared overlay peer table, so an acquire
        // that lazily expires another generation's session can dispose that
        // session's transient spec surface synchronously.
        plugin_services.set_overlay_peers(self.overlay_peers.clone());
        {
            use std::rc::Weak;
            let mut peers = self.overlay_peers.borrow_mut();
            for list in peers.values_mut() {
                list.retain(|weak: &Weak<PluginServices>| weak.upgrade().is_some());
            }
            peers.retain(|_, list: &mut Vec<Weak<PluginServices>>| !list.is_empty());
            peers
                .entry(id.as_str().to_string())
                .or_default()
                .push(Rc::downgrade(&plugin_services));
        }
        // W-29 (CTX-0942): the runtime-shared targeting mechanism state, so
        // the provider set spans every plugin and survives reload. Without
        // it every `bitty.ui.targets`/`bitty.ui.labels` call fails closed
        // with `E_UI_UNAVAILABLE`. Uses only existing `bitty-ui` types.
        plugin_services.set_targeting_state(
            self.target_registry.clone(),
            self.target_lenses.clone(),
            self.label_allocator.clone(),
        );
        // LUA-OQ-8: service backend wiring. The verified manifest's
        // `services.provided`/`services.required` become this generation's
        // declaration gate and resolution fallback; the runtime-shared
        // directory is where activation publishes and suspend/dispose
        // revokes. Without both, `services.get`/`provide` fail closed with
        // `E_NOT_IMPLEMENTED`.
        plugin_services.set_service_directory(self.service_directory.clone());
        // CTX-1035: command directory wiring for the generic host-mediated
        // invocation path. Without it `commands.list`/`invoke` fail closed
        // with `E_NOT_IMPLEMENTED`.
        plugin_services.set_command_directory(self.command_directory.clone());
        // CTX-0897: `bitty.debug` read-only backend. Each entry point needs
        // its own grant (`debug.inspect` vs `debug.trace`, no implication);
        // the `grants` target serves only this generation's own snapshot.
        plugin_services.set_debug_inspect(debug_inspect);
        plugin_services.set_debug_trace(debug_trace);
        plugin_services.set_panel_access(panel_create, panel_focus);
        plugin_services.set_granted_capabilities(
            granted
                .iter()
                .map(|capability| capability.as_str().to_string())
                .collect(),
        );
        plugin_services.set_declared_events(manifest.lazy.events.iter().cloned().collect());
        plugin_services.set_debug_view(self.debug_view.clone());
        plugin_services.set_trace_hub(self.trace_hub.clone());
        // CTX-0889: workspace L1 domain gates plus the shared backend.
        plugin_services.set_workspace_access(workspace_read, workspace_control);
        plugin_services.set_workspace_backend(
            self.workspace_source.clone(),
            Some(self.workspace_requests.clone()),
        );
        plugin_services.set_service_manifest(
            manifest.provided_services.clone(),
            manifest.required_services.clone(),
        );
        // CTX-0445: Layer-2 spawn surface. The grant gate lives here; the
        // execution backend closes over the consent ledger and (until
        // CTX-0444) a deny-all allowlist, so granted-but-unenforced tools
        // still fail closed as E_SPAWN_DENIED, never ambient.
        if spawn_git {
            plugin_services.set_spawn_git(true);
            plugin_services.set_spawn_backend(Some(spawn::git_spawn_backend(id.as_str())));
        }
        // CTX-0330: `bitty.env` grant gate. Per-key `env.read:<KEY>` grants
        // from the activation snapshot become the generation allowlist;
        // malformed or over-limit sets fail closed with rollback before any
        // VM exists. Values resolve from the host process environment.
        match env_grant_keys(id, &granted) {
            Ok(env_keys) => {
                plugin_services.set_env_source(Some(Rc::new(services::ProcessEnv)));
                if let Err(error) = plugin_services.set_env_grants(env_keys) {
                    let error = PluginRuntimeError::Integrity {
                        plugin: id.to_string(),
                        detail: error.to_string(),
                    };
                    self.rollback(id, error.to_string());
                    return Err(error);
                }
            }
            Err(error) => {
                self.rollback(id, error.to_string());
                return Err(error);
            }
        }
        // RC-1/RC-2 enter through the fail-closed gate: no VM exists without
        // explicit budgets (the deprecated `LuaVm::new` default path is sealed).
        // Production uses `VmBudgets::default` (50ms wall). Tests may widen
        // only the wall dimension via `set_vm_wall_budget_ms` to absorb
        // scheduler stalls under sharded CI load (bitty#1744, same treatment
        // as bitty#1727); instruction/memory stay at RC defaults and the
        // production budget itself is untouched.
        let vm_budgets = match self.vm_wall_budget_ms {
            Some(wall_budget_ms) => VmBudgets {
                wall_budget_ms,
                ..VmBudgets::default()
            },
            None => VmBudgets::default(),
        };
        let mut vm = match build_plugin_vm(id.as_str(), Some(vm_budgets)) {
            Ok(vm) => vm,
            Err(error) => {
                let error = PluginRuntimeError::Vm(error.to_string());
                self.rollback(id, error.to_string());
                return Err(error);
            }
        };
        vm.with_module_root(module_root.clone());
        {
            let entry = self.entries.get_mut(id).expect("entry exists");
            entry.state = LifecycleState::Loading;
            entry.generation = entry.generation.saturating_add(1);
            entry.services = Some(plugin_services.clone());
            // CTX-0428: bind the UI handle epoch to this activation generation
            // before `init.lua` runs, so handles cannot alias across reload.
            plugin_services.set_ui_epoch(entry.generation);
        }
        let services: Rc<dyn HostServices> = plugin_services.clone();
        if let Err(error) = vm.install_host_module(
            services,
            MarshallingLimits::default(),
            DEFAULT_HOST_DEADLINE_MS,
        ) {
            let error = PluginRuntimeError::Vm(error.to_string());
            self.rollback(id, error.to_string());
            return Err(error);
        }

        {
            let entry = self.entries.get_mut(id).expect("entry exists");
            entry.state = LifecycleState::Activating;
        }
        let outcome = match vm.execute_bounded(&source) {
            Ok(outcome) => outcome,
            Err(error) => {
                let error = PluginRuntimeError::Vm(error.to_string());
                self.rollback(id, error.to_string());
                return Err(error);
            }
        };
        match outcome {
            bitty_lua::BoundedExecution::Completed => {}
            bitty_lua::BoundedExecution::Suspended(reason) => {
                let message = format!("init.lua suspended: {reason:?}");
                let error = PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: message.clone(),
                };
                self.rollback(id, error.to_string());
                return Err(error);
            }
            bitty_lua::BoundedExecution::RuntimeError(message) => {
                let error = PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: message.clone(),
                };
                self.rollback(id, error.to_string());
                return Err(error);
            }
        }
        let capture = vm.take_registrations();
        if let Err(error) = validate_capture(id, &manifest, &capture) {
            self.rollback(id, error.to_string());
            return Err(error);
        }

        let (commands, events) = (capture.commands.len(), capture.events.len());
        // LUA-OQ-8: publish captured provisions into the live directory
        // before committing the generation. `validate_capture` already
        // rejected undeclared/duplicate/over-limit provisions, so the
        // manifest lookup below is infallible; version and schemas come from
        // the manifest, impl functions from the capture, the VM weakly.
        let vm = Rc::new(RefCell::new(vm));
        // Store the strong VM reference in the entry BEFORE publishing services
        // so the Weak pointers in ServiceRecords always have a valid strong
        // reference to upgrade to. On Windows, delaying the store until after
        // publish_services caused the Weak upgrade to fail intermittently during
        // cross-plugin service calls in consumer init scripts (issue #1452).
        let (source_class, unverified, generation) = {
            let entry = self.entries.get_mut(id).expect("entry exists");
            entry.vm = Some(vm.clone());
            entry.registrations = capture;
            entry.state = LifecycleState::Active;
            (
                entry.package.source_class,
                entry.package.unverified,
                entry.generation,
            )
        };
        // Publish services after the VM is safely stored
        {
            let entry = self.entries.get(id).expect("entry exists");
            self.publish_services(id, generation, &manifest, &entry.registrations, &vm);
        }
        // CTX-1035: publish captured commands into the live directory
        // before committing the generation, so the palette catalog and the
        // host-mediated invoke path serve this generation immediately.
        // `validate_capture` already rejected undeclared/duplicate/over-limit
        // registrations and malformed schemas, so the manifest lookup below
        // is infallible; effective schemas resolve manifest-first.
        {
            let entry = self.entries.get(id).expect("entry exists");
            self.publish_commands(id, generation, &manifest, &entry.registrations, &vm);
        }
        Ok(ActivationReport {
            plugin: id.clone(),
            state: LifecycleState::Active,
            commands,
            events,
            skipped_safe_mode: false,
            source_class,
            unverified,
        })
    }

    /// Activate every discovered plugin in discovery order.
    pub fn activate_discovered(
        &mut self,
    ) -> Vec<(PluginId, Result<ActivationReport, PluginRuntimeError>)> {
        let ids = self.order.clone();
        ids.into_iter()
            .map(|id| {
                let result = self.activate(&id);
                (id, result)
            })
            .collect()
    }

    /// Suspend an active plugin (`Active -> Suspended`), retaining its VM.
    ///
    /// CTX-0428: the generation's UI block handles are invalidated host-side
    /// (`UiBlocks` cleared), so `bitty.ui.update` on a pre-suspend handle
    /// fails closed with `false` after the suspend and after a resume, until
    /// the plugin mounts again.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the transition is invalid.
    pub fn suspend(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
        let result = self.suspend_inner(id);
        self.sync_debug_view();
        result
    }

    fn suspend_inner(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| lifecycle_error(id, "not found"))?;
        if entry.state != LifecycleState::Active {
            return Err(lifecycle_error(
                id,
                "only an active plugin can be suspended",
            ));
        }
        entry.state = LifecycleState::Suspended;
        if let Some(services) = entry.services.as_ref() {
            services.clear_ui_blocks();
            // W-29: suspend ends this generation's targeting session and drops
            // its lenses; outstanding handles go stale by construction.
            services.revoke_targeting_lenses();
        }
        // CTX-0941: suspend is a release; a suspended generation must never
        // keep the transient input capture (honored even when the VM is
        // parked rather than disposed).
        self.overlay_capture.borrow_mut().revoke_plugin(id.as_str());
        // LUA-OQ-8: park this generation's publications in place. Records
        // stay keyed for resume but resolve and serve nothing while parked,
        // so live consumer handles fail closed with `E_SERVICE_GONE`.
        self.service_directory
            .borrow_mut()
            .suspend_provider(id.as_str());
        // CTX-1035: park this generation's commands in place. Records stay
        // keyed for resume but list and serve nothing while parked, so
        // invocations fail closed with `E_COMMAND_GONE`.
        self.command_directory
            .borrow_mut()
            .suspend_owner(id.as_str());
        let _ = self.host.suspend(id);
        Ok(())
    }

    /// Resume a suspended plugin (`Suspended -> Active`).
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the transition is invalid.
    pub fn resume(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
        let result = self.resume_inner(id);
        self.sync_debug_view();
        result
    }

    fn resume_inner(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| lifecycle_error(id, "not found"))?;
        if entry.state != LifecycleState::Suspended {
            return Err(lifecycle_error(
                id,
                "only a suspended plugin can be resumed",
            ));
        }
        entry.state = LifecycleState::Active;
        // LUA-OQ-8: restore the parked publications (same generation, so
        // pre-suspend consumer handles serve again).
        self.service_directory
            .borrow_mut()
            .resume_provider(id.as_str());
        // CTX-1035: restore the parked command publications (same
        // generation, so the catalog and invoke path serve again).
        self.command_directory
            .borrow_mut()
            .resume_owner(id.as_str());
        let _ = self.host.resume(id);
        Ok(())
    }

    /// Dispose a plugin generation (`* -> Disposing -> Disposed`), dropping its
    /// VM and releasing host ownership.
    ///
    /// The host identity is purged (not merely marked `Disposed`) so a
    /// subsequent [`PluginRuntime::activate`] starts from a clean registry
    /// entry, matching the `Disposed -> activate` transition this module
    /// permits.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the plugin is unknown.
    pub fn dispose(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
        let result = self.dispose_inner(id);
        self.sync_debug_view();
        result
    }

    fn dispose_inner(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| lifecycle_error(id, "not found"))?;
        entry.state = LifecycleState::Disposing;
        entry.vm = None;
        entry.registrations = RegistrationCapture::new();
        // CTX-0428: a disposed generation's handles are dead; clear the host
        // registry so a stale handle cannot be served while the entry lingers.
        if let Some(services) = entry.services.as_ref() {
            services.clear_ui_blocks();
            // W-29: unload/crash/dispose ends the session and drops the
            // generation's lenses, so no orphaned session or dangling lens
            // survives the generation.
            services.revoke_targeting_lenses();
        }
        // CTX-0941: unload/crash/dispose is a release. Core drops the capture
        // so a faulty or dead generation can never pin input.
        self.overlay_capture.borrow_mut().revoke_plugin(id.as_str());
        entry.state = LifecycleState::Disposed;
        // CTX-0897: traces never outlive the generation (also on reload,
        // where dispose and the next activation share one public call).
        self.drop_traces(id);
        // LUA-OQ-8: revoke this generation's publications. The entry VM is
        // already dropped, so even a lingering record could never upgrade;
        // revocation additionally makes resolution fail closed immediately.
        self.service_directory
            .borrow_mut()
            .revoke_provider(id.as_str());
        // CTX-1035: revoke this generation's command publications, so no
        // qualified name outlives its owner.
        self.command_directory
            .borrow_mut()
            .revoke_owner(id.as_str());
        let _ = self.host.remove(id);
        Ok(())
    }

    /// Reload one plugin generation (teardown-and-rebuild).
    ///
    /// Generation N is disposed (VM, registrations, and host ownership
    /// released) before generation N+1 is resolved and activated: the accepted
    /// model, with no mixed-generation authority and no in-memory state
    /// handoff (persisted `bitty.store` data survives). Store-backed packages
    /// are re-resolved from the atomic `current.json` pointer and re-verified
    /// fail-closed; a development package is re-validated against the ratified
    /// module-tree bounds. Bundled sources are application-shipped and are not
    /// hot-swapped.
    ///
    /// A failed re-resolve or activation leaves the plugin cleanly disposed or
    /// `Failed` (FS-6); the host never falls back silently to another revision
    /// or to bundled content.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError`] when the plugin is unknown, bundled, or fails
    /// re-resolution/activation.
    pub fn reload(&mut self, id: &PluginId) -> Result<ActivationReport, PluginRuntimeError> {
        let result = self.reload_inner(id);
        self.sync_debug_view();
        result
    }

    fn reload_inner(&mut self, id: &PluginId) -> Result<ActivationReport, PluginRuntimeError> {
        let (source_class, module_root) = {
            let entry = self
                .entries
                .get(id)
                .ok_or_else(|| lifecycle_error(id, "not found"))?;
            (
                entry.package.source_class,
                entry.package.module_root.clone(),
            )
        };
        if source_class.is_bundled() {
            return Err(PluginRuntimeError::Lifecycle {
                plugin: id.to_string(),
                detail: "bundled sources are not hot-swapped".to_string(),
            });
        }
        self.dispose_inner(id)?;

        if let Some(store_root) = self.store_root.clone() {
            let records = resolution::load_index(&store_root)?;
            if let Some(record) = records
                .iter()
                .find(|record| record.plugin_id == id.as_str())
            {
                let package = resolution::resolve_record(&store_root, record)?;
                let entry = self.entries.get_mut(id).expect("entry exists");
                entry.package = package;
            } else if source_class == SourceClass::LocalPath {
                verify_module_tree(id, &module_root)?;
            }
        } else if source_class == SourceClass::LocalPath {
            verify_module_tree(id, &module_root)?;
        }

        self.activate_inner(id)
    }

    /// Dispatch one captured command, invoking its `run` function under budget.
    ///
    /// The generic host-mediated invocation core behind the palette, the
    /// keybinding `command:<qualified>` action, and
    /// [`PluginRuntime::invoke_command`]. Deny-by-default: the command must
    /// be registered by `id` (undeclared and foreign names are refused
    /// before any callee code runs), positional args are validated against
    /// the callee's effective args schema before the callee runs, and the
    /// result is validated against the effective result schema before it
    /// returns — failures are contained as typed errors, never a host
    /// panic (a re-entrant call into the already-executing owner VM,
    /// including a command invoking itself, fails closed instead of
    /// aliasing the VM).
    ///
    /// Positional args carry at most one args table in v1 (the band-click
    /// single-table and composer empty shapes): an empty slice validates as
    /// the empty object `{}`, a one-element slice validates its element,
    /// and a longer slice is refused while a schema is declared (schemeless
    /// callees keep the legacy pass-through for band-click compatibility).
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the plugin is not active;
    /// [`PluginRuntimeError::Capture`] when the command is unknown or an
    /// args/result schema is violated;
    /// [`PluginRuntimeError::Vm`] when the callback fails, suspends, or is
    /// re-entered while busy.
    pub fn dispatch_command(
        &mut self,
        id: &PluginId,
        command_id: &str,
        args: &[LuaValue],
    ) -> Result<LuaValue, PluginRuntimeError> {
        let (run, args_schema, result_schema) = {
            let entry = self
                .entries
                .get(id)
                .ok_or_else(|| lifecycle_error(id, "not found"))?;
            if !matches!(
                entry.state,
                LifecycleState::Active | LifecycleState::Suspended
            ) {
                return Err(lifecycle_error(id, "plugin is not active"));
            }
            let qualified = format!("{}:{}", id.as_str(), command_id);
            let registration = entry
                .registrations
                .commands
                .iter()
                .find(|command| format!("{}:{}", id.as_str(), command.id) == qualified)
                .ok_or_else(|| PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!("command '{command_id}' was not registered"),
                })?;
            let declared = entry
                .package
                .manifest
                .lazy
                .commands
                .iter()
                .find(|command| command.id.as_str() == qualified);
            let (args_schema, result_schema) = match declared {
                Some(declared) => effective_command_schemas(declared, registration),
                None => (None, None),
            };
            (registration.run.clone(), args_schema, result_schema)
        };
        // Args are validated before the callee runs, so a schema violation
        // never executes callee code.
        if let Some(schema) = args_schema.as_deref() {
            let document = command_positional_args_document(args).ok_or_else(|| {
                PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!(
                        "command '{command_id}' takes at most one args table ({} passed)",
                        args.len()
                    ),
                }
            })?;
            if !value_satisfies_schema(schema, &document) {
                return Err(PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!(
                        "command '{command_id}' args do not satisfy the command schema"
                    ),
                });
            }
        }
        let entry = self.entries.get_mut(id).expect("entry exists");
        // Shared VM ownership: `try_borrow_mut` fails a re-entrant command
        // invocation into this same VM closed (busy) instead of panicking
        // the `RefCell` — a command invoking itself can never crash the host.
        let vm = entry
            .vm
            .as_ref()
            .cloned()
            .ok_or_else(|| lifecycle_error(id, "no VM"))?;
        let result = match vm.try_borrow_mut() {
            Ok(mut vm) => vm.call_function(&run, args),
            Err(_) => {
                return Err(PluginRuntimeError::Vm(format!(
                    "command '{command_id}' owner '{}' is busy",
                    id.as_str()
                )));
            }
        }
        .map_err(|error| PluginRuntimeError::Vm(error.to_string()))?;
        // The effective result schema is re-checked before the result
        // returns, so a misbehaving callee is contained to this invocation.
        if let Some(schema) = result_schema.as_deref() {
            let json = store::encode_json(&result);
            if !value_satisfies_schema(schema, &json) {
                return Err(PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!(
                        "command '{command_id}' result does not satisfy the command schema"
                    ),
                });
            }
        }
        Ok(result)
    }

    /// Invoke one command by qualified name (`owner:command`, CTX-1035).
    ///
    /// The application-layer entry of the generic host-mediated invocation
    /// path: the palette selection and the keybinding
    /// `command:<qualified>` action both land here with a single args value
    /// (`nil`-equivalent empty table for argument-free commands such as
    /// `bitty-featured.devtools:plugins`). The owner half of the name must
    /// own the command half (deny-by-default: undeclared and
    /// foreign-qualified names are refused before any callee code runs);
    /// args validation, re-entrancy containment, and result validation ride
    /// [`PluginRuntime::dispatch_command`].
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Capture`] when the name is malformed, unknown,
    /// or foreign-qualified; otherwise the [`dispatch_command`](Self::dispatch_command)
    /// errors.
    pub fn invoke_command(
        &mut self,
        qualified: &str,
        args: &LuaValue,
    ) -> Result<LuaValue, PluginRuntimeError> {
        let (owner, command) =
            split_qualified_command(qualified).ok_or_else(|| PluginRuntimeError::Capture {
                plugin: qualified.to_string(),
                detail: format!("command '{qualified}' must be 'owner:command'"),
            })?;
        let id = PluginId::new(owner).map_err(|_| PluginRuntimeError::Capture {
            plugin: qualified.to_string(),
            detail: format!("command '{qualified}' names an invalid plugin id"),
        })?;
        // Foreign-qualified names are refused here: the owner half must be a
        // live generation holding the command, which `dispatch_command`
        // re-checks against the registration before running anything.
        if self
            .entries
            .get(&id)
            .is_none_or(|entry| entry.state != LifecycleState::Active)
        {
            // Suspended generations park their commands (list hides them);
            // invoking one is a lifecycle refusal, not an unknown name, so
            // the diagnosis stays joinable with suspend/resume state.
            if self
                .entries
                .get(&id)
                .is_some_and(|entry| entry.state == LifecycleState::Suspended)
            {
                return Err(lifecycle_error(&id, "plugin is suspended"));
            }
            return Err(PluginRuntimeError::Capture {
                plugin: qualified.to_string(),
                detail: format!("command '{qualified}' is not registered"),
            });
        }
        let owned = self.entries.get(&id).is_some_and(|entry| {
            entry
                .registrations
                .commands
                .iter()
                .any(|registration| registration.id == command)
        });
        if !owned {
            return Err(PluginRuntimeError::Capture {
                plugin: qualified.to_string(),
                detail: format!("command '{qualified}' is not registered"),
            });
        }
        // The args value crosses positionally: involutions with no arguments
        // pass the empty table, which validates as `{}` (see
        // `command_positional_args_document`).
        let positional = [args.clone()];
        let slice: &[LuaValue] = match args {
            LuaValue::Nil => &[],
            LuaValue::Table(pairs) if pairs.is_empty() => &[],
            _ => &positional,
        };
        self.dispatch_command(&id, command, slice)
    }

    /// List the active command catalog in (`plugin`, `id`) order (CTX-1035).
    ///
    /// The host-side catalog behind the palette entry list and the
    /// `bitty.commands.list` bridge: public registration metadata only
    /// (plugin, unqualified id, qualified name, title — the same rows
    /// `bitty.debug.inspect` serves). Suspended, disposed, and failed
    /// generations contribute nothing, so the catalog never names a command
    /// that would not serve.
    #[must_use]
    pub fn list_commands(&self) -> Vec<CommandCatalogEntry> {
        let mut entries = Vec::new();
        for (id, entry) in &self.entries {
            if entry.state != LifecycleState::Active {
                continue;
            }
            for command in &entry.registrations.commands {
                entries.push(CommandCatalogEntry {
                    plugin: id.as_str().to_string(),
                    id: command.id.clone(),
                    qualified: format!("{}:{}", id.as_str(), command.id),
                    title: command.title.clone(),
                });
            }
        }
        entries.sort_by(|a, b| (&a.plugin, &a.id).cmp(&(&b.plugin, &b.id)));
        entries
    }

    /// Deliver an observation/lifecycle event to every active subscriber.
    ///
    /// Returns the number of handler invocations that completed. Bounded and
    /// non-blocking; per-handler failures are contained to the owning plugin.
    ///
    /// Each subscriber receives the payload redacted for its own activation
    /// grant snapshot ([`redaction::recipient_view`], CTX-0899): for example
    /// `intercept.paste` text reaches only `clipboard.read` holders, and a
    /// kind without a reviewed policy is withheld. The trace hub applies the
    /// same function per owner, so `debug.trace` never sees more than a
    /// subscriber with the same grants. One envelope is built per distinct
    /// view (at most three) and passed to handlers by reference, so the
    /// common ungated case builds exactly one envelope per event.
    pub fn deliver_event(&mut self, kind: &str, payload: &LuaValue) -> usize {
        self.event_sequence = self.event_sequence.saturating_add(1);
        let sequence = self.event_sequence;
        self.record_trace(kind, payload);
        // CTX-0889: workspace events reach only generations holding
        // `workspace.read` (defense in depth: activation already rejects
        // undeclared-grant workspace kinds).
        let workspace_kind = is_workspace_event(kind);
        let ids: Vec<PluginId> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == LifecycleState::Active)
            .filter(|(_, entry)| {
                !workspace_kind
                    || entry
                        .services
                        .as_ref()
                        .is_some_and(|services| services.has_workspace_read())
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut envelopes: Vec<(redaction::RecipientView, LuaValue)> = Vec::new();
        let mut delivered = 0usize;
        for id in ids {
            let (handlers, view) = match self.entries.get(&id) {
                Some(entry) => {
                    let handlers: Vec<_> = entry
                        .registrations
                        .events
                        .iter()
                        .filter(|subscription| subscription.kind == kind)
                        .map(|subscription| subscription.handler.clone())
                        .collect();
                    // No services means no grant snapshot: fail closed.
                    let view = redaction::recipient_view(kind, |capability| {
                        entry
                            .services
                            .as_ref()
                            .is_some_and(|services| services.has_granted_capability(capability))
                    });
                    (handlers, view)
                }
                None => continue,
            };
            if handlers.is_empty() {
                continue;
            }
            // Build each distinct view's envelope once and hand handlers a
            // reference to it: no per-subscriber clone.
            let index = match envelopes.iter().position(|(seen, _)| *seen == view) {
                Some(index) => index,
                None => {
                    envelopes.push((
                        view,
                        LuaValue::table([
                            ("kind", LuaValue::String(kind.to_string())),
                            (
                                "sequence",
                                LuaValue::Integer(i64::try_from(sequence).unwrap_or(i64::MAX)),
                            ),
                            ("payload", redaction::apply_view(view, payload).into_owned()),
                        ]),
                    ));
                    envelopes.len() - 1
                }
            };
            let envelope = &envelopes[index].1;
            let Some(entry) = self.entries.get_mut(&id) else {
                continue;
            };
            let Some(vm) = entry.vm.as_ref().cloned() else {
                continue;
            };
            // One mutable borrow per plugin per event: a handler that
            // re-enters this same VM through services fails closed host-side
            // (`E_SERVICE_FAILED`) instead of aliasing it.
            let mut vm = vm.borrow_mut();
            for handler in handlers {
                if vm
                    .call_function(&handler, std::slice::from_ref(envelope))
                    .is_ok()
                {
                    delivered += 1;
                }
            }
        }
        delivered
    }

    fn open_store(&self, id: &PluginId) -> Result<PluginStore, PluginRuntimeError> {
        match &self.data_dir {
            Some(root) => {
                let path = root.join(id.as_str()).join("store.json");
                PluginStore::load_with_backend(path, self.store_backend.clone())
                    .map_err(PluginRuntimeError::Io)
            }
            None => Ok(PluginStore::in_memory()),
        }
    }

    /// Publish a generation's captured service provisions (LUA-OQ-8).
    ///
    /// Called after [`validate_capture`] and before the atomic commit, so
    /// every provision here is declared in the manifest, uniquely named, and
    /// within bounds. Version and schemas come from the manifest's
    /// `services.provided` entry (never from Lua); impl functions come from
    /// the capture; the provider VM is held weakly so dispose invalidates
    /// consumer handles without further coordination.
    fn publish_services(
        &self,
        id: &PluginId,
        generation: u32,
        manifest: &PluginManifest,
        capture: &RegistrationCapture,
        vm: &Rc<RefCell<LuaVm>>,
    ) {
        let mut directory = self.service_directory.borrow_mut();
        for provision in &capture.services {
            let declared = manifest
                .provided_services
                .iter()
                .find(|service| service.iface == provision.iface)
                .expect("validate_capture guarantees declared provisions");
            let mut funcs = BTreeMap::new();
            for method in &provision.methods {
                funcs.insert(method.name.clone(), method.func.clone());
            }
            directory.publish(ServiceRecord {
                provider: id.as_str().to_string(),
                generation,
                iface: provision.iface.clone(),
                version: declared.version.clone(),
                methods: provision.methods.iter().map(|m| m.name.clone()).collect(),
                funcs,
                args_schema: declared.args_schema.clone(),
                result_schema: declared.result_schema.clone(),
                vm: Rc::downgrade(vm),
                suspended: false,
            });
        }
    }

    /// Publish a generation's captured command registrations (CTX-1035).
    ///
    /// Called after [`validate_capture`] and before the atomic commit, so
    /// every registration here is declared in the manifest, uniquely named,
    /// within bounds, and schema-valid. Effective schemas resolve
    /// manifest-first (the manifest's `lazy.commands` table-form entry wins
    /// when present, otherwise the `commands.register` tables — mirroring
    /// the service version/schema precedence); the owner VM is held weakly
    /// so dispose invalidates invocation without further coordination.
    fn publish_commands(
        &self,
        id: &PluginId,
        generation: u32,
        manifest: &PluginManifest,
        capture: &RegistrationCapture,
        vm: &Rc<RefCell<LuaVm>>,
    ) {
        let mut directory = self.command_directory.borrow_mut();
        for registration in &capture.commands {
            let qualified = format!("{}:{}", id.as_str(), registration.id);
            let declared = manifest
                .lazy
                .commands
                .iter()
                .find(|command| command.id.as_str() == qualified)
                .expect("validate_capture guarantees declared commands");
            let (args_schema, result_schema) = effective_command_schemas(declared, registration);
            directory.publish(CommandRecord {
                owner: id.as_str().to_string(),
                id: registration.id.clone(),
                qualified,
                generation,
                title: registration.title.clone(),
                args_schema,
                result_schema,
                func: registration.run.clone(),
                vm: Rc::downgrade(vm),
                suspended: false,
            });
        }
    }

    /// Record one delivered event into the trace hub (CTX-0897).
    ///
    /// Runs once per event before fan-out, whether or not any handler is
    /// subscribed. Only traces whose owner is currently `Active` receive the
    /// record: a suspended owner's traces are paused, and disposed/failed
    /// owners have no traces (pruned by [`Self::sync_debug_view`]).
    ///
    /// Least privilege: each trace records only the kinds its owner declares
    /// in its manifest `lazy.events` (snapshotted when the trace opens), the
    /// same precondition `bitty.events.subscribe` enforces, so a
    /// `debug.trace` holder never observes a topic it could not subscribe
    /// to. Payloads are redacted per owner grant snapshot (the same
    /// [`redaction`] policy as fan-out), then bounded by
    /// [`debug::TRACE_PAYLOAD_MAX_BYTES`].
    fn record_trace(&mut self, kind: &str, payload: &LuaValue) {
        if self.trace_hub.borrow().is_empty() {
            return;
        }
        let active: BTreeSet<&str> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == LifecycleState::Active)
            .map(|(id, _)| id.as_str())
            .collect();
        self.trace_hub
            .borrow_mut()
            .record(kind, self.event_sequence, payload, |owner| {
                active.contains(owner)
            });
    }

    /// Drop every trace owned by `id` (dispose and failed activation).
    fn drop_traces(&mut self, id: &PluginId) {
        self.trace_hub
            .borrow_mut()
            .retain_owners(|owner| owner != id.as_str());
    }

    /// Rebuild the shared [`debug::DebugView`] from `entries` and prune the
    /// traces of owners that are no longer live (CTX-0897).
    ///
    /// Called at the end of every public lifecycle-changing method
    /// (`discover`, `activate`, `suspend`, `resume`, `dispose`, `reload`), on
    /// success and failure alike. Time O(p + c + e) to collect plus
    /// O(n log n) to sort the rows (p plugins, c commands, e subscriptions),
    /// plus O(t) over open traces; space O(p + c + e) for the new snapshot.
    fn sync_debug_view(&mut self) {
        let mut plugins = Vec::with_capacity(self.entries.len());
        let mut commands = Vec::new();
        let mut events = Vec::new();
        for (id, entry) in &self.entries {
            plugins.push(debug::DebugPlugin {
                id: id.as_str().to_string(),
                version: entry.package.manifest.identity.version.clone(),
                state: debug::lifecycle_label(&entry.state),
                generation: entry.generation,
            });
            for command in &entry.registrations.commands {
                commands.push(debug::DebugCommand {
                    plugin: id.as_str().to_string(),
                    id: command.id.clone(),
                    title: command.title.clone(),
                });
            }
            for subscription in &entry.registrations.events {
                events.push(debug::DebugEvent {
                    plugin: id.as_str().to_string(),
                    kind: subscription.kind.clone(),
                });
            }
        }
        self.debug_view
            .borrow_mut()
            .replace(plugins, commands, events);
        // Traces survive suspend (paused) but never outlive the generation:
        // dispose, failure, and reload drop every trace the owner held.
        let live: BTreeSet<&str> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.state,
                    LifecycleState::Active | LifecycleState::Suspended
                )
            })
            .map(|(id, _)| id.as_str())
            .collect();
        self.trace_hub
            .borrow_mut()
            .retain_owners(|owner| live.contains(owner));
    }

    /// Roll back a failed activation attempt atomically.
    ///
    /// Purges the policy-host generation (identity, command ownership, and
    /// per-generation event queues) and records a terminal [`Failed`] state
    /// locally, clearing the VM and services. Idempotent: safe whether or not
    /// the policy half committed, and whether or not the host entry exists.
    ///
    /// [`Failed`]: LifecycleState::Failed
    fn rollback(&mut self, id: &PluginId, message: String) {
        let _ = self.host.remove(id);
        if let Some(entry) = self.entries.get_mut(id) {
            entry.state = LifecycleState::Failed(message);
            entry.vm = None;
            entry.services = None;
        }
        // CTX-0941: a failed activation is a release. A generation that
        // acquired the transient capture during `init.lua` must never keep it
        // after the activation rolls back, or a dead/failed plugin would pin
        // input forever. The reason is `crashed`: the generation never
        // reached a runnable state.
        self.overlay_capture
            .borrow_mut()
            .revoke_plugin_with_reason(id.as_str(), CRASHED_RELEASE_REASON);
        // W-29: a failed activation also drops the generation's lenses (the
        // entry services are dropped above, but the shared lens set must not
        // retain a failed generation's registrations).
        self.target_lenses
            .borrow_mut()
            .retain(|(owner, _)| owner != id.as_str());
        // CTX-1035: a failed activation also revokes the generation's
        // command publications. Commands are invoked by qualified name with
        // no generation pin (unlike service routes), so a lingering record
        // would name a dead VM; revocation makes the name unknown
        // immediately.
        self.command_directory
            .borrow_mut()
            .revoke_owner(id.as_str());
        self.drop_traces(id);
    }

    /// Whether the policy host still holds a registry entry for `id`
    /// (diagnostics; a rolled-back generation leaves none).
    #[must_use]
    pub fn host_has_plugin(&self, id: &PluginId) -> bool {
        self.host.registry().get(id).is_some()
    }

    /// Whether the policy host still owns the qualified command (diagnostics;
    /// a rolled-back generation releases ownership).
    #[must_use]
    pub fn host_owns_command(&self, qualified: &str) -> bool {
        self.host.registry().is_command_owned(qualified)
    }

    /// Shared live service directory (diagnostics, tests).
    #[must_use]
    pub fn service_directory(&self) -> &Rc<RefCell<ServiceDirectory>> {
        &self.service_directory
    }

    /// Shared live command directory (diagnostics, tests).
    #[must_use]
    pub fn command_directory(&self) -> &Rc<RefCell<CommandDirectory>> {
        &self.command_directory
    }
}

fn lifecycle_error(id: &PluginId, detail: &str) -> PluginRuntimeError {
    PluginRuntimeError::Lifecycle {
        plugin: id.to_string(),
        detail: detail.to_string(),
    }
}

/// Resolve the effective invocation schemas for one registered command
/// (CTX-1035).
///
/// The manifest's `lazy.commands` table-form entry wins when present;
/// otherwise the `commands.register` tables apply (mirroring the service
/// version/schema precedence: provider-declared metadata over Lua). Lua
/// tables encode to JSON here; malformed schemas are rejected by
/// [`validate_capture`] before [`PluginRuntime::publish_commands`] runs, so
/// encoding here is infallible in practice and an unparsable schema resolves
/// to `None` (fail-open only for a record that could never have activated).
fn effective_command_schemas(
    declared: &LazyCommand,
    registration: &CommandRegistration,
) -> (Option<String>, Option<String>) {
    let args_schema = declared
        .args_schema
        .clone()
        .or_else(|| registration.args_schema.as_ref().map(store::encode_json));
    let result_schema = declared
        .result_schema
        .clone()
        .or_else(|| registration.result_schema.as_ref().map(store::encode_json));
    (args_schema, result_schema)
}

/// Split one `owner:command` qualified name (CTX-1035).
///
/// Returns the (`owner`, `command`) halves when the name holds exactly one
/// `:` with non-empty sides; otherwise `None` (malformed, never dispatched).
fn split_qualified_command(qualified: &str) -> Option<(&str, &str)> {
    match qualified.split_once(':') {
        Some((owner, command))
            if !owner.is_empty() && !command.is_empty() && !command.contains(':') =>
        {
            Some((owner, command))
        }
        _ => None,
    }
}

/// Encode the positional dispatch slice as the single args document for
/// schema validation (CTX-1035).
///
/// Commands take at most one args table in v1: an empty slice validates as
/// the empty object `{}`, a one-element slice validates its element (with
/// the `nil`/empty-table canonicalization of
/// [`services::command_args_document`]), and a longer slice is `None`
/// (refused while a schema is declared; schemeless callees keep the legacy
/// pass-through).
fn command_positional_args_document(args: &[LuaValue]) -> Option<String> {
    match args {
        [] => Some(services::command_args_document(
            &LuaValue::Table(Vec::new()),
        )),
        [single] => Some(services::command_args_document(single)),
        _ => None,
    }
}

/// Stable placeholder identity for a discovery failure that has no valid id.
fn invalid_plugin_id() -> PluginId {
    PluginId::new("invalid.root").expect("static id is valid")
}

/// Validate a registration capture against the manifest, fail closed.
///
/// Defense-in-depth complement to the bridge admission caps (`HOST-002`):
/// the bridge enforces the same count/length/timer bounds before the push,
/// but a capture built without the bridge (or from a future bridge that
/// forgets a bound) must still fail closed here — `validate_capture` is the
/// commit gate before atomic activation. Field-length violations surface as
/// `Capture` details naming the bound, never echoing untrusted content.
///
/// Static-vs-registration equivalence holds after canonicalization
/// (`owner:resource` strings): an undeclared registration fails, and a
/// declared command with no registration fails — activation commits only the
/// exact declared set.
fn validate_capture(
    id: &PluginId,
    manifest: &PluginManifest,
    capture: &RegistrationCapture,
) -> Result<(), PluginRuntimeError> {
    if capture.commands.len() > bitty_lua::REGISTRATION_MAX_COMMANDS {
        return Err(PluginRuntimeError::Capture {
            plugin: id.to_string(),
            detail: format!(
                "command registration count {} exceeds limit {}",
                capture.commands.len(),
                bitty_lua::REGISTRATION_MAX_COMMANDS
            ),
        });
    }
    if capture.events.len() > bitty_lua::REGISTRATION_MAX_EVENTS {
        return Err(PluginRuntimeError::Capture {
            plugin: id.to_string(),
            detail: format!(
                "event subscription count {} exceeds limit {}",
                capture.events.len(),
                bitty_lua::REGISTRATION_MAX_EVENTS
            ),
        });
    }
    if capture.timers.len() > bitty_lua::REGISTRATION_MAX_TIMERS {
        return Err(PluginRuntimeError::Capture {
            plugin: id.to_string(),
            detail: format!(
                "timer count {} exceeds limit {}",
                capture.timers.len(),
                bitty_lua::REGISTRATION_MAX_TIMERS
            ),
        });
    }
    let declared: BTreeSet<&str> = manifest
        .lazy
        .commands
        .iter()
        .map(|command| command.id.as_str())
        .collect();
    let declared_events: BTreeSet<&str> = manifest
        .lazy
        .events
        .iter()
        .map(|event| event.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for command in &capture.commands {
        if command.id.len() > bitty_lua::REGISTRATION_MAX_ID_BYTES {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "command id exceeds {} bytes",
                    bitty_lua::REGISTRATION_MAX_ID_BYTES
                ),
            });
        }
        if command.title.len() > bitty_lua::REGISTRATION_MAX_TITLE_BYTES {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "command title exceeds {} bytes",
                    bitty_lua::REGISTRATION_MAX_TITLE_BYTES
                ),
            });
        }
        if command.description.len() > bitty_lua::REGISTRATION_MAX_DESCRIPTION_BYTES {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "command description exceeds {} bytes",
                    bitty_lua::REGISTRATION_MAX_DESCRIPTION_BYTES
                ),
            });
        }
        let qualified = format!("{}:{}", id.as_str(), command.id);
        if !declared.contains(qualified.as_str()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("command '{qualified}' is not reserved in the manifest"),
            });
        }
        if !seen.insert(qualified.clone()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("duplicate command registration '{qualified}'"),
            });
        }
        // CTX-1035: the commit gate re-checks captured schema tables (a
        // hand-built capture bypasses the bridge). Tables encode to JSON
        // and must satisfy the interface-schema contract; manifest
        // table-form schemas are validated at manifest parse, so only the
        // Lua-declared side is checked here.
        for (field, schema) in [
            ("args_schema", command.args_schema.as_ref()),
            ("result_schema", command.result_schema.as_ref()),
        ] {
            if let Some(schema) = schema {
                let json = store::encode_json(schema);
                if let Err(error) = validate_interface_schema(&json, field) {
                    return Err(PluginRuntimeError::Capture {
                        plugin: id.to_string(),
                        detail: format!(
                            "command '{qualified}' {field} is not a valid schema ({error})"
                        ),
                    });
                }
            }
        }
    }
    // Equivalence, second direction: every declared command must have a
    // registration. A lazy command with no runtime entry would load the
    // plugin and then fail dispatch, so activation fails instead.
    for qualified in &declared {
        if !seen.contains(*qualified) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("declared command '{qualified}' was not registered"),
            });
        }
    }
    let mut seen_events = BTreeSet::new();
    for subscription in &capture.events {
        if subscription.kind.is_empty()
            || subscription.kind.len() > bitty_lua::REGISTRATION_MAX_EVENT_KIND_BYTES
        {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "event kind exceeds {} bytes",
                    bitty_lua::REGISTRATION_MAX_EVENT_KIND_BYTES
                ),
            });
        }
        if !declared_events.contains(subscription.kind.as_str()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "event '{}' is not declared in the manifest",
                    subscription.kind
                ),
            });
        }
        if !seen_events.insert(subscription.kind.clone()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("duplicate event subscription '{}'", subscription.kind),
            });
        }
    }
    let mut seen_handles = BTreeSet::new();
    for timer in &capture.timers {
        if timer.delay_ms > bitty_lua::REGISTRATION_MAX_TIMER_DELAY_MS {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "timer delay {} ms exceeds limit {} ms",
                    timer.delay_ms,
                    bitty_lua::REGISTRATION_MAX_TIMER_DELAY_MS
                ),
            });
        }
        if !seen_handles.insert(timer.handle) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("duplicate timer handle {}", timer.handle),
            });
        }
    }
    // CTX-0707: the bridge captures keymap suggestions and task spawns, so
    // the commit gate re-checks their bounds here like commands/events/timers
    // (a hand-built capture must still fail closed).
    if capture.keymaps.len() > bitty_lua::REGISTRATION_MAX_KEYMAP_SUGGESTIONS {
        return Err(PluginRuntimeError::Capture {
            plugin: id.to_string(),
            detail: format!(
                "keymap suggestion count {} exceeds limit {}",
                capture.keymaps.len(),
                bitty_lua::REGISTRATION_MAX_KEYMAP_SUGGESTIONS
            ),
        });
    }
    for suggestion in &capture.keymaps {
        if suggestion.chord.is_empty()
            || suggestion.chord.len() > bitty_lua::REGISTRATION_MAX_KEYMAP_CHORD_BYTES
        {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "keymap chord exceeds {} bytes",
                    bitty_lua::REGISTRATION_MAX_KEYMAP_CHORD_BYTES
                ),
            });
        }
        if suggestion.command.is_empty()
            || suggestion.command.len() > bitty_lua::REGISTRATION_MAX_KEYMAP_COMMAND_BYTES
        {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "keymap command exceeds {} bytes",
                    bitty_lua::REGISTRATION_MAX_KEYMAP_COMMAND_BYTES
                ),
            });
        }
        if suggestion.when != "global" {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: "keymap suggestion context must be \"global\" in v1".to_string(),
            });
        }
    }
    if capture.tasks.len() > bitty_lua::REGISTRATION_MAX_TASKS {
        return Err(PluginRuntimeError::Capture {
            plugin: id.to_string(),
            detail: format!(
                "task count {} exceeds limit {}",
                capture.tasks.len(),
                bitty_lua::REGISTRATION_MAX_TASKS
            ),
        });
    }
    let mut seen_task_handles = BTreeSet::new();
    for task in &capture.tasks {
        if !seen_task_handles.insert(task.handle) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("duplicate task handle {}", task.handle),
            });
        }
    }
    // LUA-OQ-8: the bridge captures service provisions, so the commit gate
    // re-checks their bounds here like commands/events/timers/tasks. Every
    // provision must be declared in the manifest's `services.provided`
    // (version and schemas resolve from the manifest, never from Lua).
    // Unlike commands there is no reverse equivalence: a declared service
    // with no provision simply never publishes, and resolution fails closed
    // (`E_SERVICE_RESOLUTION`) instead of activation failing.
    if capture.services.len() > bitty_lua::REGISTRATION_MAX_SERVICES {
        return Err(PluginRuntimeError::Capture {
            plugin: id.to_string(),
            detail: format!(
                "service provision count {} exceeds limit {}",
                capture.services.len(),
                bitty_lua::REGISTRATION_MAX_SERVICES
            ),
        });
    }
    let declared_services: BTreeSet<&str> = manifest
        .provided_services
        .iter()
        .map(|service| service.iface.as_str())
        .collect();
    let mut seen_services = BTreeSet::new();
    for provision in &capture.services {
        if !declared_services.contains(provision.iface.as_str()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "service '{}' is not declared in services.provided",
                    provision.iface
                ),
            });
        }
        if !seen_services.insert(provision.iface.clone()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!("duplicate service provision '{}'", provision.iface),
            });
        }
        if provision.methods.is_empty()
            || provision.methods.len() > bitty_lua::REGISTRATION_MAX_SERVICE_METHODS
        {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "service '{}' method count {} exceeds limit {}",
                    provision.iface,
                    provision.methods.len(),
                    bitty_lua::REGISTRATION_MAX_SERVICE_METHODS
                ),
            });
        }
        for method in &provision.methods {
            if method.name.is_empty() || method.name.len() > bitty_lua::SERVICE_MAX_METHOD_BYTES {
                return Err(PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!(
                        "service method name exceeds {} bytes",
                        bitty_lua::SERVICE_MAX_METHOD_BYTES
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Verify a module tree against the RFC bounds, rejecting native artifacts and
/// symlink escapes. The walk is shared with [`resolution`] so the loaded tree
/// and the digested tree are always the same bounded input.
fn verify_module_tree(id: &PluginId, root: &Path) -> Result<(), PluginRuntimeError> {
    resolution::scan_module_tree(id.as_str(), root).map(|_| ())
}

/// Extract `bitty.env` grant suffixes from the activation grant snapshot
/// (CTX-0330, CTX-0830; ADR 0006 normative, bitty#1751).
///
/// `env.read:<KEY>` grants contribute their exact key and
/// `env.read:PREFIX*` grants contribute their prefix-wildcard suffix, each
/// validated by the shared [`bitty_lua::env_grant_shape_ok`] rule
/// (`^[A-Z_][A-Z0-9_]*$` prefix, `1..=ENV_KEY_MAX_BYTES` bytes, one
/// literal trailing `*` for wildcards). The bare-star allow-all (`env.read:*`)
/// is rejected like any other malformed recorded grant: a recorded grant with
/// a malformed suffix fails closed as store integrity before any VM exists,
/// like any other undeclared grant. The set is capped at
/// [`services::MAX_ENV_GRANTS`].
fn env_grant_keys(
    id: &PluginId,
    granted: &BTreeSet<CapabilityId>,
) -> Result<BTreeSet<String>, PluginRuntimeError> {
    let mut keys = BTreeSet::new();
    for capability in granted {
        let Some(key) = capability.as_str().strip_prefix("env.read:") else {
            continue;
        };
        if !bitty_lua::env_grant_shape_ok(key) {
            return Err(PluginRuntimeError::Integrity {
                plugin: id.to_string(),
                detail: format!("recorded env grant for '{key}' is not a valid env key"),
            });
        }
        keys.insert(key.to_string());
    }
    if keys.len() > services::MAX_ENV_GRANTS {
        return Err(PluginRuntimeError::LimitExceeded {
            plugin: id.to_string(),
            field: "env.read grants".to_string(),
            limit: services::MAX_ENV_GRANTS,
            actual: keys.len(),
        });
    }
    Ok(keys)
}

/// The capabilities a generation may exercise: the declared set intersected
/// with the consented store record, or the full declared set when the package
/// has no record (bundled and development sources).
///
/// A record grant that names an undeclared capability is store tampering and
/// fails closed; grants are bound to the manifest hash that resolution already
/// re-verified, so a replacement manifest cannot silently widen authority.
fn effective_granted(
    id: &PluginId,
    declared: &BTreeSet<CapabilityId>,
    recorded: Option<&[String]>,
) -> Result<BTreeSet<CapabilityId>, PluginRuntimeError> {
    let Some(recorded) = recorded else {
        return Ok(declared.clone());
    };
    let mut effective = BTreeSet::new();
    for raw in recorded {
        let capability =
            CapabilityId::parse(raw).map_err(|error| PluginRuntimeError::Integrity {
                plugin: id.to_string(),
                detail: format!("recorded grant is not a capability identifier: {error}"),
            })?;
        if !declared.contains(&capability) {
            return Err(PluginRuntimeError::Integrity {
                plugin: id.to_string(),
                detail: format!(
                    "recorded grant '{}' is not declared by the manifest",
                    capability.as_str()
                ),
            });
        }
        effective.insert(capability);
    }
    Ok(effective)
}

/// Resolve the fixed `init.lua`: `<root>/init.lua` or `<root>/<module>/init.lua`.
pub(crate) fn entry_point(module_root: &Path, id: &PluginId) -> Option<PathBuf> {
    let direct = module_root.join("init.lua");
    if direct.is_file() {
        return Some(direct);
    }
    let module = id.as_str().rsplit('.').next().unwrap_or_default();
    let nested = module_root.join(module).join("init.lua");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

fn read_init(path: &Path) -> Result<String, PluginRuntimeError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| PluginRuntimeError::Io(format!("init.lua metadata: {error}")))?;
    if metadata.len() as usize > PLUGIN_INIT_MAX_BYTES {
        return Err(PluginRuntimeError::ModuleTree {
            plugin: "init.lua".to_string(),
            detail: "init.lua exceeds the 1 MiB ceiling".to_string(),
        });
    }
    std::fs::read_to_string(path)
        .map_err(|error| PluginRuntimeError::Io(format!("init.lua read: {error}")))
}

/// Discover packages under one root (`<root>/<name>/bitty-plugin.toml`).
///
/// Every package is stamped with `source_class`, the provenance of the root it
/// was found under; the manifest id never influences its own trust class.
fn discover_root(
    root: &Path,
    source_class: SourceClass,
) -> Result<Vec<PluginPackage>, PluginRuntimeError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut packages = Vec::new();
    let mut directories: Vec<PathBuf> = Vec::new();
    // A root may itself be one package (its own `bitty-plugin.toml`).
    if root.join("bitty-plugin.toml").is_file() {
        directories.push(root.to_path_buf());
    }
    let entries = std::fs::read_dir(root)
        .map_err(|error| PluginRuntimeError::Io(format!("discovery root: {error}")))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| PluginRuntimeError::Io(format!("discovery entry: {error}")))?;
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        }
    }
    directories.sort();
    directories.dedup();
    for directory in directories {
        let manifest_path = directory.join("bitty-plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let metadata = std::fs::metadata(&manifest_path)
            .map_err(|error| PluginRuntimeError::Io(format!("manifest metadata: {error}")))?;
        if metadata.len() as usize > PLUGIN_MANIFEST_MAX_BYTES {
            return Err(PluginRuntimeError::Manifest {
                plugin: directory.display().to_string(),
                detail: "manifest exceeds the 256 KiB ceiling".to_string(),
            });
        }
        let bytes = std::fs::read(&manifest_path)
            .map_err(|error| PluginRuntimeError::Io(format!("manifest read: {error}")))?;
        let manifest = manifest_toml::parse_manifest(&bytes).map_err(|detail| {
            PluginRuntimeError::Manifest {
                plugin: directory.display().to_string(),
                detail,
            }
        })?;
        let module_root = if directory.join("lua").is_dir() {
            directory.join("lua")
        } else {
            directory.clone()
        };
        packages.push(PluginPackage {
            manifest,
            module_root,
            source_class,
            unverified: source_class == SourceClass::LocalPath,
            granted: None,
        });
    }
    Ok(packages)
}

/// Re-export the bridge error for callers that build custom sources.
pub use bitty_lua::BridgeError as HostBridgeError;

impl PluginRuntime {
    /// Replace the discovery roots after construction.
    pub fn set_roots(&mut self, bundled_roots: Vec<PathBuf>, third_party_roots: Vec<PathBuf>) {
        self.bundled_roots = bundled_roots;
        self.third_party_roots = third_party_roots;
    }

    /// Trusted (`bundled`) discovery roots.
    #[must_use]
    pub fn bundled_roots_ref(&self) -> &[PathBuf] {
        &self.bundled_roots
    }

    /// Untrusted discovery roots.
    #[must_use]
    pub fn third_party_roots_ref(&self) -> &[PathBuf] {
        &self.third_party_roots
    }

    /// Replace the resolved plugin store root after construction.
    pub fn set_store_root(&mut self, store_root: Option<PathBuf>) {
        self.store_root = store_root;
    }

    /// Resolved plugin store root, if any.
    #[must_use]
    pub fn store_root_ref(&self) -> Option<&Path> {
        self.store_root.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_error_taxonomy_codes_and_classes() {
        let plugin = "xuepoo.test".to_string();
        let cases: Vec<(PluginRuntimeError, &str, &str)> = vec![
            (
                PluginRuntimeError::Manifest {
                    plugin: plugin.clone(),
                    detail: "bad".to_string(),
                },
                "E_MANIFEST",
                "validation",
            ),
            (
                PluginRuntimeError::ModuleTree {
                    plugin: plugin.clone(),
                    detail: "bad".to_string(),
                },
                "E_MODULE_TREE",
                "validation",
            ),
            (PluginRuntimeError::Io("io".to_string()), "E_IO", "runtime"),
            (
                PluginRuntimeError::NotFound {
                    plugin: plugin.clone(),
                    detail: "missing".to_string(),
                },
                "E_NOT_FOUND",
                "runtime",
            ),
            (
                PluginRuntimeError::Integrity {
                    plugin: plugin.clone(),
                    detail: "tampered".to_string(),
                },
                "E_INTEGRITY",
                "runtime",
            ),
            (
                PluginRuntimeError::Incompatible {
                    plugin: plugin.clone(),
                    field: "compat.bitty".to_string(),
                    requested: ">=9".to_string(),
                    host: "0.0.1".to_string(),
                },
                "E_INCOMPATIBLE",
                "validation",
            ),
            (
                PluginRuntimeError::Lifecycle {
                    plugin: plugin.clone(),
                    detail: "state".to_string(),
                },
                "E_LIFECYCLE",
                "runtime",
            ),
            (
                PluginRuntimeError::Host("host".to_string()),
                "E_HOST",
                "runtime",
            ),
            (PluginRuntimeError::Vm("vm".to_string()), "E_VM", "runtime"),
            (
                PluginRuntimeError::Capture {
                    plugin: plugin.clone(),
                    detail: "capture".to_string(),
                },
                "E_CAPTURE",
                "runtime",
            ),
            (
                PluginRuntimeError::Budget {
                    plugin: plugin.clone(),
                    detail: "quota".to_string(),
                },
                "E_BUDGET_EXCEEDED",
                "budget",
            ),
            (
                PluginRuntimeError::Timeout {
                    plugin: plugin.clone(),
                    detail: "deadline".to_string(),
                },
                "E_TIMEOUT",
                "budget",
            ),
            (
                PluginRuntimeError::LimitExceeded {
                    plugin: plugin.clone(),
                    field: "env.read grants".to_string(),
                    limit: 64,
                    actual: 65,
                },
                "E_DEF_LIMIT",
                "validation",
            ),
        ];
        for (error, code, class) in cases {
            assert_eq!(error.code(), code, "code for {error}");
            assert_eq!(error.error_class(), class, "class for {error}");
            assert!(!error.to_string().is_empty());
        }
    }

    fn parsed_grants(raw: &[&str]) -> BTreeSet<CapabilityId> {
        raw.iter()
            .map(|grant| CapabilityId::parse(grant).expect("test grant parses"))
            .collect()
    }

    #[test]
    fn env_grant_keys_extract_only_well_shaped_keys() {
        let id = PluginId::new("xuepoo.test").expect("id");
        let granted = parsed_grants(&[
            "terminal.semantic-read",
            "env.read:HOME",
            "env.read:PATH",
            "env.read:APP_*",
        ]);
        assert_eq!(
            env_grant_keys(&id, &granted).expect("keys extract"),
            BTreeSet::from(["APP_*".to_string(), "HOME".to_string(), "PATH".to_string()])
        );
        assert!(
            env_grant_keys(&id, &BTreeSet::new())
                .expect("empty")
                .is_empty()
        );
    }

    #[test]
    fn env_grant_keys_reject_bare_star_allow_all() {
        // CTX-0830 (#1483): `env.read:*` is not a prefix wildcard (empty
        // prefix) and must fail closed as store integrity.
        let id = PluginId::new("xuepoo.test").expect("id");
        for raw in ["env.read:*", "env.read:9LIVES*"] {
            let granted = parsed_grants(&[raw]);
            let error = env_grant_keys(&id, &granted).expect_err("must fail");
            assert_eq!(error.code(), "E_INTEGRITY");
        }
    }

    #[test]
    fn env_grant_keys_reject_malformed_recorded_key() {
        let id = PluginId::new("xuepoo.test").expect("id");
        let granted = parsed_grants(&["env.read:9LIVES"]);
        let error = env_grant_keys(&id, &granted).expect_err("malformed key must fail");
        assert_eq!(error.code(), "E_INTEGRITY");
        assert!(error.to_string().contains("9LIVES"));
    }

    #[test]
    fn env_grant_keys_enforce_count_limit() {
        let id = PluginId::new("xuepoo.test").expect("id");
        let raw: Vec<String> = (0..services::MAX_ENV_GRANTS + 1)
            .map(|index| format!("env.read:VAR_{index}"))
            .collect();
        let borrowed: Vec<&str> = raw.iter().map(String::as_str).collect();
        let granted = parsed_grants(&borrowed);
        match env_grant_keys(&id, &granted).expect_err("over-limit must fail") {
            PluginRuntimeError::LimitExceeded {
                plugin,
                field,
                limit,
                actual,
            } => {
                assert_eq!(plugin, "xuepoo.test");
                assert_eq!(field, "env.read grants");
                assert_eq!(limit, services::MAX_ENV_GRANTS);
                assert_eq!(actual, services::MAX_ENV_GRANTS + 1);
            }
            other => panic!("expected LimitExceeded, got {other}"),
        }
    }
}
