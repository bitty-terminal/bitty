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

pub mod audio;
pub mod component_requests;
pub mod debug;
pub mod device_providers;
pub mod fs;
pub mod fs_list;
pub mod history;
pub mod manifest_toml;
pub mod overlay;
pub mod package;
pub mod plugin_settings;
pub mod redaction;
pub mod resolution;
pub mod services;
pub mod spawn;
pub mod store;
pub mod system_sensors;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::host::DEFAULT_HOST_DEADLINE_MS;
use bitty_lua::ui::UiNode;
use bitty_lua::{
    CommandRegistration, HostServices, InvocationOutcome, LuaVm, MarshallingLimits,
    RegistrationCapture,
};
use bitty_plugin_host::DropPolicy;
use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::grant::GrantRecord;
use bitty_plugin_host::host::PluginHost;
use bitty_plugin_host::manifest::{LazyCommand, PluginId, PluginManifest};
use bitty_plugin_host::{
    parse_service_reference, validate_interface_schema, value_satisfies_schema,
};

pub use crate::runtime::fx::{FX_PULSE_QUEUE_CAPACITY, FxPulseQueue, QueuedFxPulse};
pub use bitty_lua::{
    WORKSPACE_LIST_MAX_ITEMS, WORKSPACE_NAME_MAX_CHARS as WORKSPACE_INFO_NAME_MAX_CHARS,
    WORKSPACE_RENAME_MAX_BYTES, WorkspaceAttention, WorkspaceInfo, WorkspaceRequest,
};
pub use component_requests::{
    COMPONENT_COMPLETION_CAPACITY, COMPONENT_IN_FLIGHT_PER_PLUGIN,
    COMPONENT_REQUEST_QUEUE_CAPACITY, ComponentPollOutcome, ComponentRequestQueue,
    QueuedComponentRequest, broker_request, outcome_value,
};
pub use fs::{
    FakeFileSystem, FileSystem, NativeFileSystem, STALE_TEMP_AGE_SECS, clean_temp_siblings,
    write_atomic_durably,
};
pub use fs_list::{FsListBackend, FsListEntry};
pub use history::{
    BoundedHistory, HISTORY_GRANT_SCOPE_CACHE_MAX, HISTORY_MAX_BYTES_PER_QUERY,
    HISTORY_MAX_BYTES_PER_ROW, HISTORY_MAX_BYTES_PER_WINDOW, HISTORY_MAX_QUERIES_PER_WINDOW,
    HISTORY_MAX_ROWS_PER_QUERY, HISTORY_RATE_WINDOW, HISTORY_SNAPSHOT_MAX_ROWS, HistoryProvider,
    TranscriptHistory, history_caps, history_page_value,
};
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
    CommandDirectory, CommandRecord, CoreTargetInfo, EmptyEnv, EmptySettings, EnvSource,
    MAX_ENV_GRANTS, MAX_ENV_VALUE_BYTES, MapEnv, MapSettings, Notification, NotificationQueue,
    OverlayPeers, PanelCreateHandler, PanelPinHandler, PanelPinQueue, PluginServices, ProcessEnv,
    QueuedPinRequest, QueuedSubmitRequest, QueuedThemeRequest, QueuedWorkspaceRequest,
    SERVICE_CALL_LEDGER_CAPACITY, ServiceCallLedger, ServiceCallRecord, ServiceDirectory,
    ServiceRecord, SettingsSource, SharedTargetAnnotations, SnapshotSource,
    TERMINAL_SUBMIT_WINDOW_BYTES, TargetSource, TerminalSubmitQueue, ThemePersistHandler,
    ThemeRequestQueue, UiAccess, UiBlock, UiBlocks, UnavailableSnapshot, UnavailableTargets,
    UnavailableWorkspaces, WorkspaceRequestQueue, WorkspaceSource,
};
pub use store::{KvCommitBackend, KvCommitError, PluginStore, STORE_FILE_MAX_BYTES};
// Bridge value/error types the host-service traits are expressed in, so the
// application can implement `SettingsSource`/`SnapshotSource` without taking a
// direct `bitty-lua` dependency.
pub use bitty_lua::{BridgeError, CommandCatalogEntry, LuaValue, OverlayKeyChord};

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

/// Bound on queued `bitty.panel.set_pinned` mutation intents across all
/// plugins between two app ticks (CTX-1115).
///
/// Same governance as [`WORKSPACE_REQUEST_QUEUE_CAPACITY`]: small enough
/// that a hostile loop cannot queue unbounded work, large enough for a
/// legitimate burst (one intent per panel per tick across the window).
/// Overflow drops the newest intent and the Lua call returns `false`.
pub const PANEL_PIN_QUEUE_CAPACITY: usize = 64;

/// Bound on queued `bitty.theme.preview`/`confirm` intents across all plugins
/// between two app ticks (CTX-1169, issue #1976).
///
/// Same governance as [`PANEL_PIN_QUEUE_CAPACITY`]: theme changes are rare
/// (one preview or confirm per user gesture), so 16 holds a legitimate
/// variant-generation burst while a hostile loop still cannot queue unbounded
/// work. Overflow drops the newest intent and the Lua call returns `false`.
pub const THEME_REQUEST_QUEUE_CAPACITY: usize = 16;

/// Bound on queued `bitty.terminal.submit` frames across all plugins between
/// two app ticks (CTX-1239, issue #2101).
///
/// Same governance as [`PANEL_PIN_QUEUE_CAPACITY`], sized tighter: one
/// frame carries up to the 64 KiB content cap plus framing, so 16 bounds
/// the queued bytes near one full per-generation budget window while a
/// hostile loop still cannot queue unbounded work. Overflow drops the newest
/// intent, counts it, and the Lua call reports a `denied`/`queue-full`
/// outcome table with the budget untouched (retry later).
pub const TERMINAL_SUBMIT_QUEUE_CAPACITY: usize = 16;

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
    /// An app-dispatched command reached the RC-1 wall budget and is parked
    /// for the next cold tick (bitty #2314).
    ///
    /// This is **not** a terminal refusal: the owning VM keeps the executor
    /// and resumes it through [`PluginRuntime::pump_parked_commands`] until it
    /// completes or reaches the callback ceiling. Callers on the app cold path
    /// treat it as accepted work; nested `bitty.commands.invoke` dispatch never
    /// returns it (a sync caller cannot resume).
    Parked {
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
            Self::Parked { .. } => "E_TIMEOUT",
            Self::LimitExceeded { .. } => "E_DEF_LIMIT",
        }
    }

    /// Whether this "error" is a non-terminal park for the next cold tick
    /// (bitty #2314).
    #[must_use]
    pub fn is_parked(&self) -> bool {
        matches!(self, Self::Parked { .. })
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
            Self::Budget { .. } | Self::Timeout { .. } | Self::Parked { .. } => "budget",
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
            Self::Parked { plugin, detail } => {
                write!(f, "plugin '{plugin}' invocation parked: {detail}")
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
    settings_diagnostics: Vec<plugin_settings::PluginSettingsDiagnostic>,
}

/// One app-dispatched command parked at the RC-1 wall budget (bitty #2314).
///
/// The VM owns the executor/fuel/clock; this record only keeps the command
/// identity and result schema needed to validate the eventual result. At most
/// one record exists per plugin, so the parked-work set is bounded by the
/// active plugin count rather than by input rate.
struct ParkedCommand {
    command: String,
    result_schema: Option<String>,
}

/// Result of resuming one parked command on the cold tick.
#[derive(Debug)]
pub struct ParkedCommandCompletion {
    /// Owning plugin.
    pub plugin: PluginId,
    /// Command id (unqualified).
    pub command: String,
    /// Completed result or the terminal error that ended the park.
    pub outcome: Result<LuaValue, PluginRuntimeError>,
}

type DiscoveryResults = Vec<(PluginId, Result<(), PluginRuntimeError>)>;

/// A failed read-only owning-VM counter observation (never a zero fallback).
#[cfg(feature = "input-vm-test-support")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmExecutionCountError {
    /// No entry or no VM for this plugin.
    MissingVm,
    /// A mutable VM borrow is active on the owning thread.
    Busy,
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
    /// Config directory (`<root>/bitty`) for per-plugin user configuration
    /// files: canonical Lua, an unambiguous short Lua alias, and the legacy
    /// data-only TOML fallback (CTX-1195, issue #2010).
    ///
    /// `None` by default (no file layer): activation resolves every plugin
    /// from the shipped manifest defaults plus the shared [`settings`]
    /// source (the old home). Replaceable through
    /// [`PluginRuntime::set_settings_dir`] — the application seeds it from
    /// the environment-derived config root, tests from temp dirs.
    ///
    /// [`settings`]: PluginRuntime::settings
    settings_dir: Option<PathBuf>,
    /// Complete discovery snapshot used for short-name settings aliases.
    /// `None` means discovery was never completed or reported an error, so
    /// aliases fail closed rather than being inferred from activated entries.
    settings_alias_registry: Option<Vec<String>>,
    snapshot: Rc<dyn SnapshotSource>,
    notifications: Rc<RefCell<NotificationQueue>>,
    bundled_roots: Vec<PathBuf>,
    third_party_roots: Vec<PathBuf>,
    event_sequence: u64,
    entries: BTreeMap<PluginId, PluginEntry>,
    order: Vec<PluginId>,
    service_directory: Rc<RefCell<ServiceDirectory>>,
    /// Dynapi gating (bitty#2005): runtime-shared brokered-call ledger
    /// (provider owner plus backing heads per dispatched call, the input for
    /// the observability issue 4/4). Shared with every generation's services
    /// alongside the service directory; activation never clears it (history
    /// survives reload) and suspend/dispose never prune it (entries are
    /// snapshots with no VM references).
    service_call_ledger: Rc<RefCell<ServiceCallLedger>>,
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
    /// W-29 successor (issue #2253): live Core target/anchor source for
    /// targeting sessions. `None` leaves the Core provider empty.
    target_source: Option<Rc<dyn TargetSource>>,
    /// CTX-1352 (issue #2247): optional composition-root history provider.
    /// `None` leaves granted `bitty.history.*.query` calls failing closed
    /// with the typed `E_HISTORY_UNAVAILABLE` denial, never a nil-index.
    history_provider: Option<Rc<dyn history::HistoryProvider>>,
    /// CTX-0889: bounded queue of `workspace.control` mutations, drained by
    /// the application each tick ([`PluginRuntime::drain_workspace_requests`]).
    workspace_requests: Rc<RefCell<WorkspaceRequestQueue>>,
    /// CTX-1099: synchronous pinned-float backend for
    /// `bitty.panel.set_pinned`, shared with every activated generation
    /// that holds `panel.focus`. `None` (no host wired yet) leaves granted
    /// calls failing closed with `E_NOT_IMPLEMENTED`, mirroring the
    /// workspace-source pattern. CTX-1214 adds the sibling `panel.create`
    /// seam below; the remaining panel verbs stay `E_NOT_IMPLEMENTED`
    /// until their own seams land.
    ///
    /// CTX-1115: the production host supplies a deferred backend here. The
    /// closure validates against a per-tick published pin snapshot and
    /// enqueues into [`PanelPinQueue`]; the application drains it each tick
    /// through the same `pin_floating`/`unpin_floating` runtime path the
    /// `toggle_pinned` keymap action uses. A direct live-runtime closure
    /// would need shared `Runtime` ownership, which `TerminalApp` does not
    /// offer, so the deferred queue (mirroring CTX-0889 workspace requests)
    /// is the host-handle decision. Tests keep injecting direct closures.
    panel_pin_backend: Option<PanelPinHandler>,
    /// CTX-1214 (Issue #2085): synchronous panel-creation backend for
    /// `bitty.panel.create`, shared with every activated generation that
    /// holds `panel.create`. `None` (no host wired yet) leaves granted
    /// calls failing closed with `E_NOT_IMPLEMENTED`, mirroring the
    /// pin-backend pattern. Creation allocates a `Created` registry record
    /// only; mounting and focusing stay host-driven, so the backend carries
    /// no layout authority.
    panel_create_backend: Option<PanelCreateHandler>,
    /// CTX-1115: bounded queue of `bitty.panel.set_pinned` intents, drained
    /// by the application each tick
    /// ([`PluginRuntime::drain_panel_pin_requests`]).
    panel_pin_requests: Rc<RefCell<PanelPinQueue>>,
    /// CTX-1139: bounded queue of `bitty.fx.pulse` intents, drained by the
    /// application each tick ([`PluginRuntime::drain_fx_pulses`]). The drain
    /// revalidates each intent against the live layout and arms the
    /// Core-owned ring pulse; refused, instant, or vanished intents drop
    /// with state untouched.
    fx_requests: Rc<RefCell<FxPulseQueue>>,
    /// CTX-1169: approved wallpaper roots for `bitty.image.read`, seeded
    /// from the live `background_image_roots` through
    /// [`PluginRuntime::set_image_roots`]. Shared with activated generations
    /// that hold `platform.image-file` only; empty (including `--safe`,
    /// whose effective config clears the roots) denies every read.
    image_roots: Vec<PathBuf>,
    /// CTX-1348: real-filesystem backend for `bitty.fs.list`, seeded by the
    /// composition root through [`PluginRuntime::set_fs_backend`]. Shared
    /// with every generation; the per-generation `fs.read:` snapshot gates
    /// each call. `None` leaves the verb failing closed with
    /// `E_NOT_IMPLEMENTED`.
    fs_backend: Option<Rc<FsListBackend>>,
    /// CTX-1169: bounded queue of `bitty.theme.preview`/`confirm` intents,
    /// drained by the application each tick
    /// ([`PluginRuntime::drain_theme_requests`]). The drain live-applies
    /// previews through `Runtime::set_theme_palette` and commits confirms
    /// through the persist backend first, so a preview can never persist.
    theme_requests: Rc<RefCell<ThemeRequestQueue>>,
    /// CTX-1239 (issue #2101): bounded queue of `bitty.terminal.submit`
    /// frames, drained by the application each tick
    /// ([`PluginRuntime::drain_submit_requests`]). The drain writes each
    /// already-authorized frame through the single input router
    /// (`Runtime::push_input_bytes`); authorization (grant, cap, budget,
    /// framing) ran at enqueue time, so the drain is mechanical.
    submit_requests: Rc<RefCell<TerminalSubmitQueue>>,
    /// CTX-2254 (bitty#2254): bounded plugin-facing component request surface.
    ///
    /// Enqueued inside the VM callback and drained by the composition root
    /// (`bitty_terminal::component_bridge::pump_component_requests`) once per
    /// application tick, which authorizes each submission through the runtime
    /// and submits it to the existing component broker. Results return to the
    /// plugin through
    /// `bitty.component.poll`. Holds no authority: the runtime re-checks the
    /// lifecycle, manifest generation, and grant record for every request.
    component_requests: Rc<RefCell<ComponentRequestQueue>>,
    /// CTX-1169: durable-commit backend behind confirmed colorschemes.
    ///
    /// `None` by default (no persist configured): confirms drain to a
    /// warning with state untouched, mirroring the pin-backend pattern.
    /// Replaceable through [`PluginRuntime::set_theme_persist_backend`] so
    /// durability and failure can be injected deterministically.
    theme_persist_backend: Option<ThemePersistHandler>,
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
    /// W-29 successor (issue #2253): runtime-shared present-time annotation
    /// channel. The active targeting generation publishes its batched
    /// annotation layer here; the application reads it once per tick and
    /// hands it to the scene/compositor. `None` while no session is active.
    target_annotations: Rc<RefCell<Option<Rc<bitty_ui::HintAnnotationLayer>>>>,
    /// Durable-commit backend behind disk-backed plugin stores (`data_dir`).
    ///
    /// `None` by default (no durable store configured): stores opened while
    /// no backend is installed fail closed on write and load clean and
    /// empty. Replaceable through [`PluginRuntime::set_store_backend`] so
    /// latency and failure can be injected deterministically (bitty #1518).
    store_backend: Option<Arc<dyn store::KvCommitBackend>>,
    /// CTX-1199: runtime-owned native sensor host (bitty#2019/#2011/#2016).
    ///
    /// One host spans every generation, so the provider-side ceilings stay
    /// global. Published into the service directory at construction;
    /// activation wires the handle into each generation. Tests replace the
    /// contents through [`PluginRuntime::sensor_host`] (fixture sources
    /// plus a pinned clock) without republishing.
    sensor_host: Rc<RefCell<system_sensors::host::SensorHost>>,
    /// CTX-1238: runtime-owned native device host (issue #2100).
    ///
    /// One host spans every generation, so the provider-side ceilings stay
    /// global. Published into the service directory at construction;
    /// activation wires the handle into each generation. Tests replace the
    /// contents through [`PluginRuntime::device_host`] (fixture roots plus
    /// a pinned clock) without republishing.
    device_host: Rc<RefCell<device_providers::host::DeviceHost>>,
    /// CTX-1238: runtime-owned native audio host (issue #2100).
    ///
    /// One host spans every generation, so the provider-side gates stay
    /// global. Published into the service directory at construction;
    /// activation wires the handle into each generation. Tests replace the
    /// contents through [`PluginRuntime::audio_host`] (fixture spawner plus
    /// a pinned clock) without republishing.
    audio_host: Rc<RefCell<audio::AudioHost>>,
    /// Test-only wall-clock override for the activation VM in milliseconds.
    ///
    /// `None` by default (production): activation uses
    /// [`VmBudgets::default`] (RC-1 `RC1_WALL_CLOCK_BUDGET_MS`, 50ms).
    /// `Some(ms)` widens only the wall dimension for that activation;
    /// instruction and memory stay at RC defaults. Used by load-sensitive
    /// integration tests (bitty#1744, same treatment as bitty#1727);
    /// production code never sets this.
    vm_wall_budget_ms: Option<u64>,
    /// App-dispatched commands parked at the RC-1 wall budget (bitty #2314).
    ///
    /// At most one parked command per plugin and none per non-command
    /// callback: the VM holds exactly one executor, and this map only carries
    /// the command identity/result schema needed to validate the eventual
    /// result. Bounded by the active plugin count, never by input rate.
    /// Cancellation on suspend/dispose/reload removes the entry and drops the
    /// VM's parked executor.
    parked_commands: BTreeMap<PluginId, ParkedCommand>,
    /// CTX-1244 (issue #2107): host-owned one-shot timer deadlines.
    ///
    /// Keyed by `(plugin, handle)`; the value is the absolute firing
    /// deadline (`activation now + delay_ms`, or `callback-now + delay_ms`
    /// for timers created inside a timer callback). Armed at activation,
    /// fired once by [`Self::pump_due_timers_at`], removed on fire, cancel,
    /// or dispose. Only `Active` entries fire; [`Self::timer_deadline`]
    /// joins the app wake-min so idle wakes for timers while unarmed idle
    /// stays `None` (zero-submit preserved). Cold-tick only, never the hot
    /// path (P0-AC-015).
    timer_deadlines: BTreeMap<(PluginId, i64), Instant>,
    /// Handles already fired (one-shot containment fallback).
    ///
    /// CTX-1247: the pump prunes each fired row from the VM capture on fire
    /// (bridge `consume_timer`), so the `REGISTRATION_MAX_TIMERS` cap counts
    /// live timers only and this set stays empty in steady state. Entries are
    /// kept only when the consume borrow contends (fail-closed: the
    /// post-callback sync must never re-arm a fired handle) and are cleared
    /// per plugin on dispose/reload/rollback. Bounded by the live timer cap
    /// in practice (one entry per unconsumed fire, normally zero).
    timer_fired: BTreeSet<(PluginId, i64)>,
}

impl PluginRuntime {
    /// Reads actual owning-VM entries under a short, shared owning-thread borrow.
    /// No VM/reference escapes and no execution or dispatch occurs.
    ///
    /// # Errors
    ///
    /// Distinguishes missing VM from a busy mutable borrow.
    #[cfg(feature = "input-vm-test-support")]
    #[doc(hidden)]
    pub fn vm_total_executions(&self, id: &PluginId) -> Result<u64, VmExecutionCountError> {
        let vm = self
            .entries
            .get(id)
            .and_then(|entry| entry.vm.as_ref())
            .ok_or(VmExecutionCountError::MissingVm)?;
        let vm = vm.try_borrow().map_err(|_| VmExecutionCountError::Busy)?;
        Ok(vm.total_executions())
    }

    /// Create an empty runtime.
    #[must_use]
    pub fn new(config: PluginRuntimeConfig) -> Self {
        let service_directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        let sensor_host = Rc::new(RefCell::new(system_sensors::host::SensorHost::live()));
        let device_host = Rc::new(RefCell::new(device_providers::host::DeviceHost::live()));
        let audio_host = Rc::new(RefCell::new(audio::AudioHost::live()));
        Self::publish_native_sensors(&service_directory);
        Self::publish_native_devices(&service_directory);
        Self::publish_native_audio(&service_directory);
        Self {
            safe_mode: config.safe_mode,
            data_dir: config.data_dir,
            store_root: config.store_root,
            host: PluginHost::new(DropPolicy::DropOldest, crate::DEFAULT_PLUGIN_SIDE_CAPACITY),
            settings: config.settings,
            settings_dir: None,
            settings_alias_registry: None,
            snapshot: config.snapshot,
            notifications: Rc::new(RefCell::new(NotificationQueue::new(
                NOTIFICATION_QUEUE_CAPACITY,
            ))),
            bundled_roots: config.bundled_roots,
            third_party_roots: config.third_party_roots,
            event_sequence: 0,
            entries: BTreeMap::new(),
            order: Vec::new(),
            service_directory,
            service_call_ledger: Rc::new(RefCell::new(ServiceCallLedger::new(
                SERVICE_CALL_LEDGER_CAPACITY,
            ))),
            command_directory: Rc::new(RefCell::new(CommandDirectory::new())),
            debug_view: Rc::new(RefCell::new(debug::DebugView::new())),
            trace_hub: Rc::new(RefCell::new(debug::TraceHub::new())),
            workspace_source: None,
            target_source: None,
            history_provider: None,
            workspace_requests: Rc::new(RefCell::new(WorkspaceRequestQueue::new(
                WORKSPACE_REQUEST_QUEUE_CAPACITY,
            ))),
            panel_pin_backend: None,
            panel_create_backend: None,
            panel_pin_requests: Rc::new(RefCell::new(PanelPinQueue::new(PANEL_PIN_QUEUE_CAPACITY))),
            fx_requests: Rc::new(RefCell::new(FxPulseQueue::new(FX_PULSE_QUEUE_CAPACITY))),
            image_roots: Vec::new(),
            fs_backend: None,
            theme_requests: Rc::new(RefCell::new(ThemeRequestQueue::new(
                THEME_REQUEST_QUEUE_CAPACITY,
            ))),
            submit_requests: Rc::new(RefCell::new(TerminalSubmitQueue::new(
                TERMINAL_SUBMIT_QUEUE_CAPACITY,
            ))),
            component_requests: Rc::new(RefCell::new(ComponentRequestQueue::new())),
            theme_persist_backend: None,
            overlay_capture: Rc::new(RefCell::new(OverlayCapture::new())),
            overlay_peers: Rc::new(RefCell::new(BTreeMap::new())),
            target_registry: Rc::new(RefCell::new(bitty_ui::TargetRegistry::new())),
            target_lenses: Rc::new(RefCell::new(Vec::new())),
            label_allocator: Rc::new(RefCell::new(bitty_ui::LabelAllocator::default())),
            target_annotations: Rc::new(RefCell::new(None)),
            store_backend: None,
            sensor_host,
            device_host,
            audio_host,
            vm_wall_budget_ms: None,
            parked_commands: BTreeMap::new(),
            timer_deadlines: BTreeMap::new(),
            timer_fired: BTreeSet::new(),
        }
    }

    /// Publish the three batch-A native sensor records (CTX-1199).
    ///
    /// Runtime-owned (not a plugin activation): provider
    /// `bitty-terminal.sensors` at [`system_sensors::host::SENSOR_GENERATION`]
    /// serving `system.cpu`/`system.memory`/`system.battery` `get()` backed
    /// by the read-only heads. Version, methods, and heads come from the
    /// descriptors, never from Lua. Suspend/dispose filter by plugin id, so
    /// native records survive every plugin lifecycle transition.
    fn publish_native_sensors(directory: &Rc<RefCell<ServiceDirectory>>) {
        use std::collections::BTreeMap;
        use std::rc::Weak;
        for service in system_sensors::sensor_descriptors() {
            directory.borrow_mut().publish(ServiceRecord {
                provider: system_sensors::SENSOR_PROVIDER.to_string(),
                generation: system_sensors::host::SENSOR_GENERATION,
                iface: service.iface.clone(),
                version: service.version.clone(),
                methods: vec!["get".to_string()],
                funcs: BTreeMap::new(),
                args_schema: None,
                result_schema: None,
                heads: service.heads.clone(),
                vm: Weak::new(),
                suspended: false,
            });
        }
    }

    /// Publish the eight batch-B native device records (CTX-1238).
    ///
    /// Runtime-owned (not a plugin activation): provider
    /// `bitty-terminal.devices` at
    /// [`device_providers::host::DEVICE_GENERATION`] serving
    /// `network.state`/`network.essid`/`bluetooth.*`/`backlight.*`/`idle.*`
    /// backed by their broker heads. Version, methods, and heads come from
    /// the descriptors, never from Lua. Suspend/dispose filter by plugin
    /// id, so native records survive every plugin lifecycle transition.
    fn publish_native_devices(directory: &Rc<RefCell<ServiceDirectory>>) {
        use std::collections::BTreeMap;
        use std::rc::Weak;
        for service in device_providers::device_descriptors() {
            let methods = device_providers::device_methods(&service.iface);
            directory.borrow_mut().publish(ServiceRecord {
                provider: device_providers::DEVICE_PROVIDER.to_string(),
                generation: device_providers::host::DEVICE_GENERATION,
                iface: service.iface.clone(),
                version: service.version.clone(),
                methods,
                funcs: BTreeMap::new(),
                args_schema: None,
                result_schema: None,
                heads: service.heads.clone(),
                vm: Weak::new(),
                suspended: false,
            });
        }
    }

    /// Publish the three native audio records (CTX-1238).
    ///
    /// Runtime-owned (not a plugin activation): provider
    /// `bitty-terminal.audio` at [`audio::AUDIO_GENERATION`] serving
    /// `audio.state`/`audio.control`/`audio.spectrum` backed by their
    /// capability heads. Version, methods, and heads come from the
    /// descriptors, never from Lua. Suspend/dispose filter by plugin id, so
    /// native records survive every plugin lifecycle transition.
    fn publish_native_audio(directory: &Rc<RefCell<ServiceDirectory>>) {
        use std::collections::BTreeMap;
        use std::rc::Weak;
        for service in audio::audio_descriptors() {
            let methods = audio::audio_methods(&service.iface);
            directory.borrow_mut().publish(ServiceRecord {
                provider: audio::AUDIO_PROVIDER.to_string(),
                generation: audio::AUDIO_GENERATION,
                iface: service.iface.clone(),
                version: service.version.clone(),
                methods,
                funcs: BTreeMap::new(),
                args_schema: None,
                result_schema: None,
                heads: service.heads.clone(),
                vm: Weak::new(),
                suspended: false,
            });
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

    /// Borrow one discovered package's manifest, provenance, and recorded
    /// grants, if discovered.
    ///
    /// Read-only introspection for VM-less surfaces (e.g. `bitty list
    /// services`, dynapi observability bitty#2002): manifests parse at
    /// discovery, so this never loads a plugin VM. Returns `None` for
    /// undiscovered ids.
    #[must_use]
    pub fn discovered_package(&self, id: &PluginId) -> Option<&PluginPackage> {
        self.entries.get(id).map(|entry| &entry.package)
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

    /// Iterate over all mounted UI blocks with their generation-owned handle
    /// and activation generation (CTX-1332, C-D4).
    ///
    /// Additive `#[doc(hidden)]` projection used by the application to
    /// capture a [`crate::BandOrigin`] at presentation. Order is discovery
    /// order, identical to [`Self::ui_blocks`]. No VM is entered and no
    /// reference escapes.
    #[doc(hidden)]
    #[must_use]
    pub fn ui_blocks_with_handles(
        &self,
    ) -> Vec<(PluginId, bitty_lua::ui::UiSlot, UiNode, u32, u32, i64)> {
        let mut result = Vec::new();
        for id in &self.order {
            if let Some(entry) = self.entries.get(id) {
                if let Some(svc) = &entry.services {
                    let generation = entry.generation;
                    svc.with_ui_blocks(|blocks| {
                        for (handle, block) in blocks.iter() {
                            result.push((
                                id.clone(),
                                block.ui_slot(),
                                block.node().clone(),
                                block.version(),
                                generation,
                                handle,
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

    /// Install the live Core target/anchor source for targeting sessions
    /// (W-29 successor, issue #2253).
    ///
    /// Call before activation: each generation captures the source when its
    /// services are built. Without it the Core provider is empty and plugin
    /// lenses still work; presence grants no authority.
    pub fn set_target_source(&mut self, source: Rc<dyn TargetSource>) {
        self.target_source = Some(source);
    }

    /// Install the composition-root history snapshot provider (CTX-1352).
    ///
    /// Shared with every activated generation; the per-source capability
    /// head and the Core history gate remain authoritative per call. Without
    /// a provider, granted queries deny with `E_HISTORY_UNAVAILABLE`.
    pub fn set_history_provider(&mut self, provider: Rc<dyn history::HistoryProvider>) {
        self.history_provider = Some(provider);
    }

    /// Seed the config directory for per-plugin user configuration files
    /// (CTX-1195, issue #2010). Call before activation with the
    /// environment-derived `<root>/bitty` directory: each generation resolves
    /// its canonical `plugins/<owner>.<name>.lua`, then an unambiguous
    /// `plugins/<name>.lua` short alias, then the legacy
    /// `plugins/<owner>.<name>.toml` when its services are built; the first
    /// present file wins. Settings are pinned to that generation (no hot
    /// reload); absent files or `None` serve shipped defaults plus the old
    /// home, and safe mode passes `None`. Paths always derive from the
    /// environment; never pass a literal home.
    pub fn set_settings_dir(&mut self, dir: Option<PathBuf>) {
        self.settings_dir = dir;
    }

    /// Install the synchronous pinned-float backend for
    /// `bitty.panel.set_pinned` (CTX-1099). Call before activation: each
    /// generation captures the backend when its services are built, and
    /// only when `panel.focus` is granted (no backend without grant).
    /// Without it a granted call fails closed with `E_NOT_IMPLEMENTED`,
    /// like every other panel verb without a host.
    pub fn set_panel_pin_backend(&mut self, backend: Option<PanelPinHandler>) {
        self.panel_pin_backend = backend;
    }

    /// Install the synchronous panel-creation backend for
    /// `bitty.panel.create` (CTX-1214, Issue #2085). Call before activation:
    /// each generation captures the backend when its services are built, and
    /// only when `panel.create` is granted (no backend without grant).
    /// Without it a granted call fails closed with `E_NOT_IMPLEMENTED`,
    /// like every other panel verb without a host.
    pub fn set_panel_create_backend(&mut self, backend: Option<PanelCreateHandler>) {
        self.panel_create_backend = backend;
    }

    /// Seed the approved wallpaper roots for `bitty.image.read` (CTX-1169,
    /// issue #1976). Call before activation with the live
    /// `background_image_roots`; each generation captures the roots when its
    /// services are built, and only when `platform.image-file` is granted
    /// (no roots without grant). Empty (including `--safe`) denies every
    /// read.
    pub fn set_image_roots(&mut self, roots: Vec<PathBuf>) {
        self.image_roots = roots;
    }

    /// Install the real-filesystem backend for `bitty.fs.list` (CTX-1348).
    ///
    /// Call before activation with the environment-derived home directory;
    /// each generation captures the backend when its services are built.
    /// The backend carries no authority by itself: every call re-reads the
    /// generation's own `fs.read:` snapshot, so an ungranted generation
    /// fails closed with `E_FS_MISSING_GRANT`. `None` leaves granted calls
    /// failing closed with `E_NOT_IMPLEMENTED`.
    pub fn set_fs_backend(&mut self, backend: Option<Rc<FsListBackend>>) {
        self.fs_backend = backend;
    }

    /// Install the durable-commit backend for confirmed colorschemes
    /// (CTX-1169, issue #1976). Call before or after activation: the drain
    /// reads the current backend on every tick, so rotation never strands a
    /// queued confirm. `None` leaves confirms failing closed at drain time
    /// with state untouched.
    pub fn set_theme_persist_backend(&mut self, backend: Option<ThemePersistHandler>) {
        self.theme_persist_backend = backend;
    }

    /// Current durable-commit backend for confirmed colorschemes, if any.
    #[must_use]
    pub fn theme_persist_backend(&self) -> Option<ThemePersistHandler> {
        self.theme_persist_backend.clone()
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

    /// Runtime-shared pin intent queue handle (CTX-1115).
    ///
    /// The production host builds its deferred [`PanelPinHandler`] backend
    /// over this handle before activation, so granted `set_pinned` calls
    /// from `panel.focus` generations enqueue here. Diagnostics and tests
    /// read the live queue through this handle.
    #[must_use]
    pub fn panel_pin_queue(&self) -> &Rc<RefCell<PanelPinQueue>> {
        &self.panel_pin_requests
    }

    /// Drain queued `bitty.panel.set_pinned` intents in FIFO order (CTX-1115).
    ///
    /// The application applies each intent on its own thread through the
    /// same `pin_floating`/`unpin_floating` runtime path the `toggle_pinned`
    /// keymap action uses, revalidating against live state. Bounded by
    /// [`PANEL_PIN_QUEUE_CAPACITY`].
    pub fn drain_panel_pin_requests(&mut self) -> Vec<QueuedPinRequest> {
        self.panel_pin_requests.borrow_mut().drain()
    }

    /// Pin intents dropped by queue overflow since creation.
    #[must_use]
    pub fn panel_pin_requests_dropped(&self) -> u64 {
        self.panel_pin_requests.borrow().dropped()
    }

    /// Runtime-shared fx intent queue handle (CTX-1139).
    ///
    /// Shared with every activated generation that holds `panel.focus`, so
    /// granted `bitty.fx.pulse` calls from `panel.focus` generations enqueue
    /// here. Diagnostics and tests read the live queue through this handle.
    #[must_use]
    pub fn fx_queue(&self) -> &Rc<RefCell<FxPulseQueue>> {
        &self.fx_requests
    }

    /// Drain queued `bitty.fx.pulse` intents in FIFO order (CTX-1139).
    ///
    /// The application arms each intent on its own thread through
    /// `Runtime::trigger_fx_pulse`, revalidating the target against the live
    /// layout first. Bounded by [`FX_PULSE_QUEUE_CAPACITY`].
    pub fn drain_fx_pulses(&mut self) -> Vec<QueuedFxPulse> {
        self.fx_requests.borrow_mut().drain()
    }

    /// Fx intents dropped by queue overflow since creation.
    #[must_use]
    pub fn fx_requests_dropped(&self) -> u64 {
        self.fx_requests.borrow().dropped()
    }

    /// Runtime-shared theme intent queue handle (CTX-1169, issue #1976).
    ///
    /// Shared with every activated generation that holds `runtime.configure`,
    /// so granted `preview`/`confirm` calls from those generations enqueue
    /// here. Diagnostics and tests read the live queue through this handle.
    #[must_use]
    pub fn theme_queue(&self) -> &Rc<RefCell<ThemeRequestQueue>> {
        &self.theme_requests
    }

    /// Drain queued `bitty.theme.preview`/`confirm` intents in FIFO order
    /// (CTX-1169, issue #1976).
    ///
    /// The application applies each intent on its own thread: previews
    /// through `Runtime::set_theme_palette` with no durable write, confirms
    /// through the persist backend first and `set_theme_palette` only on
    /// commit success. Bounded by [`THEME_REQUEST_QUEUE_CAPACITY`].
    pub fn drain_theme_requests(&mut self) -> Vec<QueuedThemeRequest> {
        self.theme_requests.borrow_mut().drain()
    }

    /// Theme intents dropped by queue overflow since creation.
    #[must_use]
    pub fn theme_requests_dropped(&self) -> u64 {
        self.theme_requests.borrow().dropped()
    }

    /// Runtime-shared submit intent queue handle (CTX-1239, issue #2101).
    ///
    /// Shared with every activated generation that holds
    /// `terminal.input.submit`, so granted `bitty.terminal.submit` calls
    /// from those generations enqueue here. Diagnostics and tests read the
    /// live queue through this handle.
    #[must_use]
    pub fn submit_queue(&self) -> &Rc<RefCell<TerminalSubmitQueue>> {
        &self.submit_requests
    }

    /// Drain queued `bitty.terminal.submit` frames in FIFO order (CTX-1239,
    /// issue #2101).
    ///
    /// The application writes each frame on its own thread through the
    /// single input router (`Runtime::push_input_bytes`); every frame
    /// drained here was already authorized (grant, cap, budget, framing) at
    /// enqueue time, so the drain is mechanical. Bounded by
    /// [`TERMINAL_SUBMIT_QUEUE_CAPACITY`].
    pub fn drain_submit_requests(&mut self) -> Vec<QueuedSubmitRequest> {
        self.submit_requests.borrow_mut().drain()
    }

    /// Submit intents dropped by queue overflow since creation.
    #[must_use]
    pub fn submit_requests_dropped(&self) -> u64 {
        self.submit_requests.borrow().dropped()
    }

    /// Runtime-shared component request surface (CTX-2254, bitty#2254).
    #[must_use]
    pub fn component_request_queue(&self) -> &Rc<RefCell<ComponentRequestQueue>> {
        &self.component_requests
    }

    /// Take the component submissions enqueued since the last tick.
    ///
    /// Drained by the composition-root pump, which authorizes and submits each
    /// one through the runtime's component broker.
    pub fn drain_component_requests(&mut self) -> Vec<QueuedComponentRequest> {
        self.component_requests.borrow_mut().take_submissions()
    }

    /// Bind one accepted submission to its broker request id.
    pub fn bind_component_request(
        &mut self,
        ticket: u64,
        plugin_id: &str,
        id: crate::component::RequestId,
        deadline: Instant,
    ) {
        self.component_requests
            .borrow_mut()
            .bind(ticket, plugin_id, id, deadline);
    }

    /// Complete one submission with a typed pre-broker failure.
    pub fn fail_component_request(
        &mut self,
        ticket: u64,
        plugin_id: &str,
        class: &str,
        message: &str,
    ) {
        self.component_requests
            .borrow_mut()
            .fail(ticket, plugin_id, class, message);
    }

    /// Deliver broker events into the request surface.
    pub fn deliver_component_events(&mut self, events: &[crate::component::BrokerEvent]) {
        let mut queue = self.component_requests.borrow_mut();
        for event in events {
            queue.deliver(event);
        }
    }

    /// Fail in-flight requests past their deadline; returns broker ids to
    /// cancel.
    pub fn expire_component_requests(&mut self, now: Instant) -> Vec<crate::component::RequestId> {
        self.component_requests.borrow_mut().expire(now)
    }

    /// Broker ids completed by a bridge-side bound (response budget) that the
    /// caller must cancel.
    pub fn take_cancelled_component_requests(&mut self) -> Vec<crate::component::RequestId> {
        self.component_requests.borrow_mut().take_cancelled()
    }

    /// Component submissions dropped for lack of queue room.
    #[must_use]
    pub fn component_requests_dropped(&self) -> u64 {
        self.component_requests.borrow().dropped()
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

    /// Current present-time annotation batch, if a targeting session is
    /// active (W-29 successor, issue #2253).
    ///
    /// The application reads this once per tick on the cold path and hands it
    /// to the scene/compositor. The batch is already bounded by
    /// [`bitty_ui::MAX_HINT_ANNOTATIONS`]; the handle is shared, so the
    /// per-tick read is a pointer clone, never a copy of the labels. It is
    /// `None` with no session, after dispatch, and after cancel/timeout/unload.
    #[must_use]
    pub fn target_annotations(&self) -> Option<Rc<bitty_ui::HintAnnotationLayer>> {
        self.target_annotations.borrow().clone()
    }

    /// Reconcile the active targeting session against the latest published
    /// live target rows (issue #2253 review).
    ///
    /// Called on the cold tick after `LiveTargets` publishes the present
    /// solve. Retires vanished panel/workspace ids, drops their labels and
    /// dispatch bindings, and republishes the layer. No-op without an active
    /// capture/session. Returns whether anything changed; source errors leave
    /// the session untouched.
    pub fn reconcile_target_session(&self) -> bool {
        let Some(owner) = self
            .overlay_capture
            .borrow()
            .owner_plugin()
            .map(str::to_string)
        else {
            return false;
        };
        let Ok(id) = PluginId::new(&owner) else {
            return false;
        };
        self.services(&id)
            .is_some_and(|services| services.reconcile_target_session().unwrap_or(false))
    }

    /// Enqueue one captured input event for the active overlay capture, if any.
    ///
    /// This is the Core input-path entry: it appends to the capture queue and
    /// never invokes plugin code (`P0-AC-015`). Returns `false` when no capture
    /// is active, so input falls through to the terminal as before.
    pub fn push_overlay_input(&mut self, kind: &str, text: &str) -> bool {
        self.overlay_capture.borrow_mut().enqueue(kind, text)
    }

    /// Enqueue one captured `key` event with its chord identity (issue #2098).
    ///
    /// A flat key event carries only produced text, so `ctrl+enter` and a bare
    /// `enter` arrive identically; `chord` is the missing channel that lets the
    /// owner tell the default submit chord from plain Enter. `None` queues an
    /// unmodified key, byte-identical to before. Returns `false` when no
    /// capture is active.
    pub fn push_overlay_key(&mut self, text: &str, chord: Option<OverlayKeyChord>) -> bool {
        self.overlay_capture.borrow_mut().enqueue_key(text, chord)
    }

    /// Enqueue a pointer-motion event with trailing-motion coalescing (see
    /// [`OverlayCapture::enqueue_move`]).
    pub fn push_overlay_move(&mut self, text: &str) -> bool {
        self.overlay_capture.borrow_mut().enqueue_move(text)
    }

    /// Enqueue a wheel event as the first-class `wheel` kind with
    /// trailing-tick coalescing (see [`OverlayCapture::enqueue_wheel`]).
    pub fn push_overlay_wheel(&mut self, text: &str) -> bool {
        self.overlay_capture.borrow_mut().enqueue_wheel(text)
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
            // W-29 successor: a runtime-ended session also clears the
            // present-time annotation batch and dispatcher bindings, so a
            // timed-out or focus-switched session cannot leave hints on
            // screen or accept a late label.
            services.clear_targeting_session();
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

    /// Earliest pending one-shot timer deadline, if any (CTX-1244).
    ///
    /// `Some(min)` while an `Active` generation holds un-fired timers;
    /// `None` when no timers are armed (uninstalled, unarmed, suspended-only,
    /// disposed, or all fired/cancelled). The app arms `set_wait_until` at
    /// it so idle wakes for timers; unarmed idle stays on `set_wait`
    /// (zero-submit preserved). Cold-tick read only, never the hot path.
    #[must_use]
    pub fn timer_deadline(&self) -> Option<Instant> {
        self.timer_deadlines
            .iter()
            .filter(|((plugin, _), _)| {
                self.entries
                    .get(plugin)
                    .is_some_and(|entry| entry.state == LifecycleState::Active)
            })
            .map(|(_, deadline)| *deadline)
            .min()
    }

    /// Fire due one-shot timers at the current instant (CTX-1244).
    ///
    /// Cold-tick pump only (the app calls it from `drive_tick` before
    /// present, never on the input hot path). Returns the number of
    /// callbacks that ran. One-shot: each due timer fires at most once;
    /// failures are contained (a faulting callback still consumes its
    /// one shot). Bounded: at most the armed timers plus at most one
    /// optimistic re-arm chain per fire, capped fail-closed.
    pub fn pump_due_timers(&mut self) -> usize {
        self.pump_due_timers_at(Instant::now())
    }

    /// Deterministic timer pump at `now` (CTX-1244).
    ///
    /// Fake-clock entry point for tests: arm at activation `T`, then
    /// `pump_due_timers_at(T + delay)` fires. Production calls
    /// [`Self::pump_due_timers`]. Same bounds and containment as the
    /// wall-clock pump; `now` frozen for the whole pump so a `0`-delay
    /// re-arm fires in the same pump (bounded) while future delays stay
    /// pending.
    pub fn pump_due_timers_at(&mut self, now: Instant) -> usize {
        self.sync_timer_cancels();
        self.sync_new_timers(now);
        let mut fired = 0usize;
        // Contain a hostile `0`-delay re-arm loop: one pump fires at most
        // the armed set plus a bounded re-arm chain.
        while let Some((plugin, handle)) = self.earliest_due_timer(now) {
            if fired >= 512 {
                break;
            }
            self.timer_deadlines.remove(&(plugin.clone(), handle));
            self.timer_fired.insert((plugin.clone(), handle));
            let callback = self.live_timer_callback(&plugin, handle);
            let vm = self.entries.get(&plugin).and_then(|entry| entry.vm.clone());
            let active = self
                .entries
                .get(&plugin)
                .is_some_and(|entry| entry.state == LifecycleState::Active);
            // CTX-1247: prune the fired row before invoking the callback so
            // a callback-created timer reuses the freed 64-cap slot (at-cap
            // chains would otherwise fail `E_DEF_LIMIT` even though the
            // one-shot deadline is gone). The callback was cloned above, so
            // pruning cannot lose it; on consume contention the
            // `timer_fired` entry is kept fail-closed.
            self.consume_fired_timer(&plugin, handle);
            if active {
                if let (Some(vm), Some(callback)) = (vm, callback) {
                    if let Ok(mut vm_ref) = vm.try_borrow_mut() {
                        let _ = vm_ref.call_function(&callback, &[]);
                    }
                    fired += 1;
                }
            }
            self.sync_timer_cancels();
            self.sync_new_timers(now);
        }
        fired
    }

    /// Earliest due timer at `now` among `Active` generations, if any.
    fn earliest_due_timer(&self, now: Instant) -> Option<(PluginId, i64)> {
        self.timer_deadlines
            .iter()
            .filter(|((plugin, _), deadline)| {
                **deadline <= now
                    && self
                        .entries
                        .get(plugin)
                        .is_some_and(|entry| entry.state == LifecycleState::Active)
            })
            .min_by_key(|(_, deadline)| **deadline)
            .map(|((plugin, handle), _)| (plugin.clone(), *handle))
    }

    /// Live callback for `(plugin, handle)`, if still registered.
    ///
    /// Reads the VM capture (which holds post-activation creates) with a
    /// fallback to the committed snapshot, so activation-armed and
    /// callback-armed timers both fire. `None` when cancelled or gone.
    fn live_timer_callback(
        &self,
        plugin: &PluginId,
        handle: i64,
    ) -> Option<bitty_lua::StashedFunction> {
        let entry = self.entries.get(plugin)?;
        if let Some(vm) = entry.vm.as_ref() {
            if let Ok(vm_ref) = vm.try_borrow() {
                let live = vm_ref.take_registrations();
                if let Some(timer) = live.timers.iter().find(|timer| timer.handle == handle) {
                    return Some(timer.callback.clone());
                }
            }
        }
        entry
            .registrations
            .timers
            .iter()
            .find(|timer| timer.handle == handle)
            .map(|timer| timer.callback.clone())
    }

    /// Prune a fired one-shot row from the VM capture (CTX-1247).
    ///
    /// Frees the bridge 64-cap slot so the cap covers live timers only
    /// (one-shot semantics preserved: the host deadline is already removed
    /// and handles never reuse). Drops the `timer_fired` containment entry
    /// when the row is confirmed absent — removed here or already gone
    /// (for example the callback previously cancelled its own handle) —
    /// keeping that set bounded (normally empty). Retains the entry only on
    /// missing VM or borrow contention fail-closed, where the row might
    /// still be present and the sync must never re-arm it.
    /// Cold-tick only, never the hot path.
    fn consume_fired_timer(&mut self, plugin: &PluginId, handle: i64) {
        let outcome: Option<bool> = self
            .entries
            .get(plugin)
            .and_then(|entry| entry.vm.clone())
            .and_then(|vm| {
                vm.try_borrow()
                    .ok()
                    .and_then(|vm_ref| vm_ref.consume_timer(handle))
            });
        if outcome.is_some() {
            self.timer_fired.remove(&(plugin.clone(), handle));
        }
    }

    /// Drop deadlines whose handle left the VM capture (Lua `cancel`).
    ///
    /// Cancelled handles are removed without joining `timer_fired` (handles
    /// never reuse, so the post-callback sync cannot re-arm them).
    fn sync_timer_cancels(&mut self) {
        let keys: Vec<(PluginId, i64)> = self.timer_deadlines.keys().cloned().collect();
        for (plugin, handle) in keys {
            let Some(entry) = self.entries.get(&plugin) else {
                self.timer_deadlines.remove(&(plugin, handle));
                continue;
            };
            let Some(vm) = entry.vm.as_ref() else {
                self.timer_deadlines.remove(&(plugin, handle));
                continue;
            };
            let live_handles: Option<Vec<i64>> = vm.try_borrow().ok().map(|vm_ref| {
                vm_ref
                    .take_registrations()
                    .timers
                    .iter()
                    .map(|timer| timer.handle)
                    .collect()
            });
            let Some(live_handles) = live_handles else {
                continue;
            };
            if !live_handles.contains(&handle) {
                self.timer_deadlines.remove(&(plugin, handle));
            }
        }
    }

    /// Arm VM-capture timers not yet tracked (activation chain + callback
    /// re-arms) at `now + delay_ms`.
    ///
    /// Skips already-pending and already-fired handles (one-shot: fired rows
    /// are pruned from the capture on fire, `timer_fired` is the
    /// fail-closed fallback for contended consumes). Enforces the live timer
    /// cap per plugin fail-closed (extra handles stay unarmed). Only
    /// `Active` generations arm; `checked_add` overflow arms at `now` (fire
    /// immediately, fail-closed liveness).
    fn sync_new_timers(&mut self, now: Instant) {
        let ids: Vec<PluginId> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == LifecycleState::Active)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let live: Vec<(i64, u64)> = {
                let Some(entry) = self.entries.get(&id) else {
                    continue;
                };
                let Some(vm) = entry.vm.as_ref() else {
                    continue;
                };
                let Ok(vm_ref) = vm.try_borrow() else {
                    continue;
                };
                vm_ref
                    .take_registrations()
                    .timers
                    .iter()
                    .map(|timer| (timer.handle, timer.delay_ms))
                    .collect()
            };
            for (handle, delay_ms) in live {
                let key = (id.clone(), handle);
                if self.timer_deadlines.contains_key(&key) || self.timer_fired.contains(&key) {
                    continue;
                }
                let pending = self
                    .timer_deadlines
                    .keys()
                    .filter(|(plugin, _)| plugin == &id)
                    .count();
                if pending >= bitty_lua::REGISTRATION_MAX_TIMERS {
                    continue;
                }
                let deadline = now
                    .checked_add(Duration::from_millis(delay_ms))
                    .unwrap_or(now);
                self.timer_deadlines.insert(key, deadline);
            }
        }
    }

    /// Arm a plugin's committed timers at `now` (activation path).
    ///
    /// Clears any stale deadlines/fired rows for `id` first (fresh
    /// generation), then arms every committed timer at `now + delay_ms`.
    fn arm_plugin_timers_at(&mut self, id: &PluginId, now: Instant) {
        self.clear_plugin_timers(id);
        let timers: Vec<(i64, u64)> = self
            .entries
            .get(id)
            .map(|entry| {
                entry
                    .registrations
                    .timers
                    .iter()
                    .map(|timer| (timer.handle, timer.delay_ms))
                    .collect()
            })
            .unwrap_or_default();
        for (handle, delay_ms) in timers {
            let deadline = now
                .checked_add(Duration::from_millis(delay_ms))
                .unwrap_or(now);
            self.timer_deadlines.insert((id.clone(), handle), deadline);
        }
    }

    /// Drop every deadline and fired row for `id` (dispose/reload/rollback).
    fn clear_plugin_timers(&mut self, id: &PluginId) {
        self.timer_deadlines.retain(|(plugin, _), _| plugin != id);
        self.timer_fired.retain(|(plugin, _)| plugin != id);
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
        // A new pass invalidates any previous alias snapshot immediately. It
        // is restored only after this pass proves that every configured source
        // was available and every candidate was resolved successfully.
        self.settings_alias_registry = None;
        let (results, registry_ids) = self.discover_inner();
        let roots_available = self
            .bundled_roots
            .iter()
            .chain(self.third_party_roots.iter())
            .all(|root| root.is_dir());
        let store_available = match self.store_root.as_ref() {
            // Without a configured store there is no unenumerated source that
            // could introduce an alias collision.
            None => true,
            // Safe mode deliberately does not enumerate the configured store,
            // so its records cannot be proven absent from the alias registry.
            Some(_) if self.safe_mode => false,
            // Normal discovery includes the configured store when its root is
            // available; preserve the existing directory availability check.
            Some(root) => root.is_dir(),
        };
        self.settings_alias_registry = (roots_available && store_available)
            .then(|| Self::complete_discovery_ids(registry_ids, &results))
            .flatten();
        self.sync_debug_view();
        results
    }

    fn discover_inner(&mut self) -> (DiscoveryResults, Vec<String>) {
        let mut results = Vec::new();
        // This registry is intentionally independent of activation: disabled
        // installed records still claim their short names and must participate
        // in collision checks.
        let mut registry_ids = Vec::new();

        for root in self.bundled_roots.clone() {
            self.collect_dir_root(&root, SourceClass::Bundled, &mut results, &mut registry_ids);
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
                        // A valid index record is a candidate even when its
                        // desired state is disabled. Do not resolve disabled
                        // packages merely to decide whether an alias exists.
                        registry_ids.push(id.as_str().to_string());
                        if !record.enabled {
                            continue;
                        }
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
            self.collect_dir_root(
                &root,
                SourceClass::LocalPath,
                &mut results,
                &mut registry_ids,
            );
        }

        (results, registry_ids)
    }

    /// Build the alias registry from one complete discovery pass.
    ///
    /// Failed or unavailable candidates remain in `results` as error rows, so
    /// a short name is never declared unique from the successful subset.
    fn complete_discovery_ids(
        mut registry_ids: Vec<String>,
        results: &DiscoveryResults,
    ) -> Option<Vec<String>> {
        if results.iter().any(|(_, result)| result.is_err()) {
            return None;
        }
        registry_ids.sort();
        registry_ids.dedup();
        Some(registry_ids)
    }

    fn collect_dir_root(
        &mut self,
        root: &Path,
        source_class: SourceClass,
        results: &mut Vec<(PluginId, Result<(), PluginRuntimeError>)>,
        registry_ids: &mut Vec<String>,
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
            registry_ids.push(id.as_str().to_string());
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
                settings_diagnostics: Vec::new(),
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

    /// Return bounded settings diagnostics for the currently stored plugin
    /// generation. Diagnostics stay on the host-owned entry rather than on
    /// [`ActivationReport`], preserving source compatibility for downstream
    /// exhaustive struct literals and destructuring.
    #[must_use]
    pub fn settings_diagnostics(
        &self,
        id: &PluginId,
    ) -> Option<&[plugin_settings::PluginSettingsDiagnostic]> {
        self.entries
            .get(id)
            .map(|entry| entry.settings_diagnostics.as_slice())
    }

    fn activate_inner(&mut self, id: &PluginId) -> Result<ActivationReport, PluginRuntimeError> {
        let (manifest, module_root, init_path, recorded_grant, source_class) = {
            let entry = self
                .entries
                .get_mut(id)
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
            entry.settings_diagnostics.clear();
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
        // bitty #2314: a previous generation's park can never survive into a
        // fresh activation (defensive; dispose/reload already cancel it). The
        // state validation above refused a still-active generation before
        // this point, so an active plugin's park is never disturbed here.
        self.cancel_parked_command(id);
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
            self.host.insert_grant(GrantRecord::granted(
                id.clone(),
                hash.clone(),
                granted.clone(),
                0,
            ));
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
        // CTX-1239 (issue #2101): the submit grant is the lease
        // authorization for the Lua path (submit_host.rs: holder
        // attribution arrives with the capability grant lane). Read never
        // implies submit and submit never implies read: the composer holds
        // both, each gated independently.
        let terminal_submit = granted
            .iter()
            .any(|capability| capability.as_str() == "terminal.input.submit");
        // CTX-0889: the workspace domain grants are independent (read never
        // implies control, control never implies read).
        let workspace_read = granted
            .iter()
            .any(|capability| capability.as_str() == "workspace.read");
        let workspace_control = granted
            .iter()
            .any(|capability| capability.as_str() == "workspace.control");
        // CTX-1169 (issue #1976): the colorscheme grants are independent
        // (image input never implies theme output). Both ride closed heads
        // that already exist in the capability grammar with zero prior
        // implementation; wiring them here changes no other verb.
        let image_file = granted
            .iter()
            .any(|capability| capability.as_str() == "platform.image-file");
        let runtime_configure = granted
            .iter()
            .any(|capability| capability.as_str() == "runtime.configure");
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
        let settings_diagnostics;
        let plugin_services = {
            // CTX-1195 (issue #2010): per-generation settings — manifest
            // shipped defaults, then the old home (the shared host source),
            // then the per-plugin user file (canonical Lua, an unambiguous
            // short Lua alias, or the legacy data-only TOML fallback; the
            // first present file wins). Each generation holds its own
            // resolved map plus its declared key set, so a cross-plugin read
            // fails closed structurally and at the gate. A malformed file
            // resolves to lower layers (never aborts activation); grants are
            // untouched by construction (this path names no capability).
            let loaded = plugin_settings::load_plugin_settings_with_plugins_and_diagnostics(
                if self.safe_mode {
                    None
                } else {
                    self.settings_dir.as_deref()
                },
                id.as_str(),
                &manifest.settings,
                Some(&*self.settings),
                self.settings_alias_registry.as_deref(),
            );
            let (loaded, diagnostics) = loaded;
            settings_diagnostics = diagnostics;
            let services = Rc::new(PluginServices::new(
                id.as_str(),
                store,
                Rc::new(loaded.source),
                self.snapshot.clone(),
                self.notifications.clone(),
                terminal_read,
                platform_notify,
            ));
            services.set_declared_settings(plugin_settings::declared_keys(&manifest.settings));
            services
        };
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
            self.target_annotations.clone(),
        );
        // W-29 successor (issue #2253): the live Core target/anchor source.
        // `None` leaves the Core provider empty; plugin lenses still work.
        plugin_services.set_target_source(self.target_source.clone());
        // LUA-OQ-8: service backend wiring. The verified manifest's
        // `services.provided`/`services.required` become this generation's
        // declaration gate and resolution fallback; the runtime-shared
        // directory is where activation publishes and suspend/dispose
        // revokes. Without both, `services.get`/`provide` fail closed with
        // `E_NOT_IMPLEMENTED`.
        plugin_services.set_service_directory(self.service_directory.clone());
        plugin_services.set_service_call_ledger(self.service_call_ledger.clone());
        // CTX-1199: native sensor host (bitty#2019/#2011/#2016). Shared with
        // every generation; the broker dispatches
        // `bitty-terminal.sensors:system.*.get()` host-side. The call-time
        // grant gate in `service_call` stays as the second layer, so
        // ungranted consumption stays indistinguishable from absent.
        plugin_services.set_sensor_host(Some(self.sensor_host.clone()));
        // CTX-1238: native device + audio hosts (issue #2100). Shared with
        // every generation; the broker dispatches
        // `bitty-terminal.devices:*` and `bitty-terminal.audio:audio.*`
        // host-side. The call-time grant gates in `service_call` stay as
        // the second layer, so ungranted consumption stays
        // indistinguishable from absent.
        plugin_services.set_device_host(Some(self.device_host.clone()));
        plugin_services.set_audio_host(Some(self.audio_host.clone()));
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
        // CTX-1099: the `bitty.panel.set_pinned` host backend. Installed
        // only for `panel.focus` generations (no backend without grant);
        // the call-time grant gate in `panel_set_pinned` stays as the
        // second layer.
        if panel_focus {
            plugin_services.set_panel_pin_backend(self.panel_pin_backend.clone());
        }
        // CTX-1214 (Issue #2085): the `bitty.panel.create` host backend.
        // Installed only for `panel.create` generations (no backend without
        // grant); the call-time grant gate in `panel_create` stays as the
        // second layer, and the zero-PTY type policy there stays as the
        // third. Remaining sibling panel verbs have no backend seam, so
        // they keep failing closed with `E_NOT_IMPLEMENTED` unchanged.
        if panel_create {
            plugin_services.set_panel_create_backend(self.panel_create_backend.clone());
        }
        // CTX-1239 (issue #2101): the `bitty.terminal.submit` intent queue.
        // Shared only with `terminal.input.submit` generations (no queue
        // without grant); the call-time grant gate in `terminal_submit`
        // stays as the second layer, and the bridge presence probe
        // (`has_terminal_submit`) requires both, so ungranted generations
        // observe nil — indistinguishable from absent.
        plugin_services.set_terminal_submit_access(terminal_submit);
        if terminal_submit {
            plugin_services.set_submit_queue(Some(self.submit_requests.clone()));
        }
        // CTX-1139: the `bitty.fx.pulse` intent queue. Shared only with
        // `panel.focus` generations (no queue without grant); the call-time
        // grant gate in `fx_pulse` stays as the second layer. Without it a
        // granted call fails closed with `E_NOT_IMPLEMENTED`.
        if panel_focus {
            plugin_services.set_fx_queue(Some(self.fx_requests.clone()));
        }
        // CTX-1169 (issue #1976): the colorscheme backends. Roots ride only
        // with `platform.image-file` generations and the theme queue only
        // with `runtime.configure` generations (no backend without grant);
        // the call-time grant gates stay as the second layer. The persist
        // backend is read at drain time, so it needs no per-generation
        // capture here.
        plugin_services.set_image_access(image_file);
        if image_file {
            plugin_services.set_image_roots(Some(self.image_roots.clone()));
        }
        // CTX-1348: real-filesystem `bitty.fs.list` backend. Shared with
        // every generation; the call-time gate reads this generation's own
        // `fs.read:` activation snapshot, so a generation without a read
        // grant fails closed with `E_FS_MISSING_GRANT` and the backend never
        // becomes ambient authority.
        plugin_services.set_fs_backend(self.fs_backend.clone());
        plugin_services.set_theme_access(runtime_configure);
        if runtime_configure {
            plugin_services.set_theme_queue(Some(self.theme_requests.clone()));
        }
        plugin_services.set_granted_capabilities(
            granted
                .iter()
                .map(|capability| capability.as_str().to_string())
                .collect(),
        );
        // CTX-2254 (bitty#2254): the production component request surface.
        // The queue is shared with every generation (it carries no authority);
        // the manifest generation is pinned host-side so a request from a
        // stale generation is refused by the runtime instead of served under
        // the new one.
        plugin_services.set_component_queue(Some(self.component_requests.clone()));
        plugin_services.set_component_manifest_hash(Some(hash.clone()));
        plugin_services.set_declared_events(manifest.lazy.events.iter().cloned().collect());
        plugin_services.set_debug_view(self.debug_view.clone());
        plugin_services.set_trace_hub(self.trace_hub.clone());
        // CTX-0889: workspace L1 domain gates plus the shared backend.
        plugin_services.set_workspace_access(workspace_read, workspace_control);
        plugin_services.set_workspace_backend(
            self.workspace_source.clone(),
            Some(self.workspace_requests.clone()),
        );
        // CTX-1352 (issue #2247): the composition-root history provider.
        // Wired for every generation; the per-source grant head and the
        // Core history gate still decide each call, so a provider alone
        // grants nothing and an absent provider denies typed-unavailable.
        plugin_services.set_history_provider(self.history_provider.clone());
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
        if let Err(error) = validate_capture(id, &manifest, &capture, &granted) {
            self.rollback(id, error.to_string());
            return Err(error);
        }

        let (commands, events) = (capture.commands.len(), capture.events.len());
        // LUA-OQ-8: publish captured provisions into the live directory
        // before committing the generation. `validate_capture` already
        // rejected undeclared/duplicate/over-limit/unbacked/ungranted provisions, so the
        // manifest lookup below is infallible; version, schemas, and heads come from
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
            entry.settings_diagnostics = settings_diagnostics;
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
        // CTX-1244 (issue #2107): arm one-shot timers at activation `now +
        // delay_ms` so idle wakes for them. Without this the host pumps
        // zero ticks when idle and armed plugin timers never fire.
        self.arm_plugin_timers_at(id, Instant::now());
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
        // bitty #2314: a suspended generation must not keep a parked executor
        // or resume it after the lifecycle transition. Drop the park first;
        // the callback body is never restarted.
        self.cancel_parked_command(id);
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
        // CTX-2254: a parked generation may not receive component results it
        // can no longer poll under its authority; every held ticket fails
        // closed with the typed `plugin_not_active` class.
        self.component_requests
            .borrow_mut()
            .drop_plugin(id.as_str());
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
        // bitty #2314: teardown cancels any parked command before the VM is
        // dropped, so no resume can observe a half-disposed generation.
        self.cancel_parked_command(id);
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
        // CTX-2254: a disposed generation holds no request authority; every
        // ticket still open for it fails closed with `plugin_not_active`.
        self.component_requests
            .borrow_mut()
            .drop_plugin(id.as_str());
        // CTX-1244: a disposed generation's timers are dead; drop deadlines
        // and fired rows so a stale handle can never fire or wake.
        self.clear_plugin_timers(id);
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
    /// re-entered while busy; [`PluginRuntimeError::Parked`] when it reached
    /// the RC-1 wall budget and is resumable on the next cold tick.
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
            if self.parked_commands.contains_key(id) {
                // One parked executor per plugin: a second invocation cannot
                // start while the first is suspended mid-body.
                return Err(PluginRuntimeError::Vm(format!(
                    "command '{command_id}' owner '{}' has a parked invocation",
                    id.as_str()
                )));
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
            Ok(mut vm) => vm.call_function_resumable(&run, args),
            Err(_) => {
                return Err(PluginRuntimeError::Vm(format!(
                    "command '{command_id}' owner '{}' is busy",
                    id.as_str()
                )));
            }
        };
        let result = match result {
            Ok(InvocationOutcome::Completed(value)) => value,
            Ok(InvocationOutcome::Parked) => {
                // Keep the owning generation alive and the command identity
                // needed to validate the result. The next cold tick resumes
                // the same executor; the body is never restarted.
                self.parked_commands.insert(
                    id.clone(),
                    ParkedCommand {
                        command: command_id.to_string(),
                        result_schema: result_schema.clone(),
                    },
                );
                return Err(PluginRuntimeError::Parked {
                    plugin: id.to_string(),
                    detail: format!(
                        "command '{command_id}' parked at the RC-1 wall budget; resumes on the next cold tick"
                    ),
                });
            }
            Err(error) => return Err(PluginRuntimeError::Vm(error.to_string())),
        };
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

    /// Dispatch one command admitted from a presented band origin
    /// (CTX-1332, C-D5/C-D6).
    ///
    /// Host-only, additive, `#[doc(hidden)]` entry for the cold delivery
    /// phase. It (1) requires the owning generation to be `Active`, (2)
    /// re-validates the supplied [`crate::BandOrigin`] against the live mount
    /// registry (generation, handle, slot placement, exact version), and
    /// (3) **delegates** to [`Self::dispatch_command`] so registration, args
    /// schema, result schema, busy containment, VM budget, and downstream
    /// capability checks stay on the single existing body. Generic
    /// `dispatch_command`/`invoke_command` semantics — including the
    /// deliberate `Suspended` probe allowance — are unchanged.
    ///
    /// No new error variant is introduced: every refusal reuses the existing
    /// [`PluginRuntimeError::Lifecycle`] class.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the origin is missing from the
    /// live registry, the generation/handle/version/slot changed, or the
    /// plugin is not `Active`; otherwise the delegated
    /// [`dispatch_command`](Self::dispatch_command) errors.
    #[doc(hidden)]
    pub fn dispatch_ui_command(
        &mut self,
        id: &PluginId,
        command_id: &str,
        args: &[LuaValue],
        origin: &crate::BandOrigin,
    ) -> Result<LuaValue, PluginRuntimeError> {
        self.validate_ui_origin(id, origin)?;
        self.dispatch_command(id, command_id, args)
    }

    /// Live origin/authorization re-query immediately before the VM borrow
    /// (CTX-1332, C-D5).
    ///
    /// The "authorization currently true" check is this live registry query
    /// plus the registration/schema checks `dispatch_command` performs at call
    /// time; it is never a cached boolean. Persistent-store edits are **not**
    /// treated as live revocation: only app-observed lifecycle transitions
    /// (suspend, dispose, reload, unmount, replacement) invalidate a
    /// generation.
    fn validate_ui_origin(
        &self,
        id: &PluginId,
        origin: &crate::BandOrigin,
    ) -> Result<(), PluginRuntimeError> {
        if origin.plugin_id != id.as_str() {
            return Err(lifecycle_error(
                id,
                "band origin does not match the command owner",
            ));
        }
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| lifecycle_error(id, "not found"))?;
        if entry.state != LifecycleState::Active {
            return Err(lifecycle_error(id, "plugin is not active"));
        }
        if entry.generation != origin.generation {
            return Err(lifecycle_error(id, "band origin generation is stale"));
        }
        let services = entry
            .services
            .as_ref()
            .ok_or_else(|| lifecycle_error(id, "no services"))?;
        let live = services.with_ui_blocks(|blocks| {
            blocks
                .get(origin.handle)
                .map(|block| (block.ui_slot(), block.version()))
        });
        let Some((slot, version)) = live else {
            return Err(lifecycle_error(
                id,
                "band origin handle is no longer mounted",
            ));
        };
        if slot != origin.slot {
            return Err(lifecycle_error(id, "band origin slot changed"));
        }
        if version != origin.version {
            return Err(lifecycle_error(id, "band origin version is stale"));
        }
        if crate::runtime::band_slots::ui_slot_placement(slot)
            != crate::runtime::band_slots::UiSlotPlacement::Band(origin.edge)
        {
            return Err(lifecycle_error(id, "band origin placement changed"));
        }
        Ok(())
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

    /// Resume every parked command once on the cold tick (bitty #2314).
    ///
    /// Called by the application's exclusive cold phase, never by input,
    /// redraw, PTY, or event delivery. Each call advances at most one RC-1
    /// wall window per parked plugin; a callback that still needs work stays
    /// parked for the next tick, and cumulative active wall time is bounded by
    /// the VM's callback ceiling. Completed results (and terminal failures)
    /// are returned so the caller can surface them exactly once.
    ///
    /// Cancelled or no-longer-active generations never resume: their parks are
    /// removed and the executor is dropped.
    pub fn pump_parked_commands(&mut self) -> Vec<ParkedCommandCompletion> {
        let ids: Vec<PluginId> = self.parked_commands.keys().cloned().collect();
        let mut completed = Vec::new();
        for id in ids {
            let Some(parked) = self.parked_commands.remove(&id) else {
                continue;
            };
            if !self
                .entries
                .get(&id)
                .is_some_and(|entry| entry.state == LifecycleState::Active)
            {
                if !self.clear_vm_park(&id) {
                    // Keep the record if the VM borrow contends; the next
                    // cold tick retries the cancellation. Never resume an
                    // inactive generation.
                    self.parked_commands.insert(id, parked);
                }
                continue;
            }
            let Some(vm) = self.entries.get(&id).and_then(|entry| entry.vm.clone()) else {
                continue;
            };
            let outcome = match vm.try_borrow_mut() {
                Ok(mut vm) => vm.resume_parked_call(),
                Err(_) => {
                    // A live borrow is unexpected on the cold path; keep the
                    // park for the next tick instead of guessing.
                    self.parked_commands.insert(id, parked);
                    continue;
                }
            };
            match outcome {
                Ok(InvocationOutcome::Parked) => {
                    self.parked_commands.insert(id, parked);
                }
                Ok(InvocationOutcome::Completed(value)) => {
                    let outcome = match parked.result_schema.as_deref() {
                        Some(schema) => {
                            let json = store::encode_json(&value);
                            if value_satisfies_schema(schema, &json) {
                                Ok(value)
                            } else {
                                Err(PluginRuntimeError::Capture {
                                    plugin: id.to_string(),
                                    detail: format!(
                                        "command '{}' result does not satisfy the command schema",
                                        parked.command
                                    ),
                                })
                            }
                        }
                        None => Ok(value),
                    };
                    completed.push(ParkedCommandCompletion {
                        plugin: id,
                        command: parked.command,
                        outcome,
                    });
                }
                Err(error) => {
                    completed.push(ParkedCommandCompletion {
                        plugin: id,
                        command: parked.command,
                        outcome: Err(PluginRuntimeError::Vm(error.to_string())),
                    });
                }
            }
        }
        completed
    }

    /// Number of commands currently parked for the next cold tick.
    ///
    /// Bounded by the active plugin count (one per plugin); exposed for the
    /// cold-path diagnostics and cancellation tests.
    #[must_use]
    pub fn parked_command_count(&self) -> usize {
        self.parked_commands.len()
    }

    /// Drop one plugin's parked invocation without running its body.
    fn cancel_parked_command(&mut self, id: &PluginId) {
        if self.clear_vm_park(id) {
            self.parked_commands.remove(id);
        }
    }

    /// Drop the VM's parked executor. Returns `false` only when a live borrow
    /// prevents the cancellation; the caller then keeps the durable record so
    /// the next cold tick can retry.
    fn clear_vm_park(&self, id: &PluginId) -> bool {
        match self.entries.get(id).and_then(|entry| entry.vm.clone()) {
            Some(vm) => match vm.try_borrow_mut() {
                Ok(mut vm) => {
                    vm.cancel_parked_call();
                    true
                }
                Err(_) => false,
            },
            None => true,
        }
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
    /// every provision here is declared in the manifest, uniquely versioned,
    /// backed by at least one granted head, and within bounds. Version,
    /// schemas, and backing heads come from the manifest's
    /// `services.provided` entry (never from Lua); impl functions come from
    /// the capture; the provider VM is held weakly so dispose invalidates
    /// consumer handles without further coordination. Side-by-side versions
    /// (dynapi negotiation, bitty#2006) bind by occurrence: the nth
    /// provision of one interface publishes the nth declared (interface,
    /// version), mirroring [`validate_capture`].
    fn publish_services(
        &self,
        id: &PluginId,
        generation: u32,
        manifest: &PluginManifest,
        capture: &RegistrationCapture,
        vm: &Rc<RefCell<LuaVm>>,
    ) {
        let mut directory = self.service_directory.borrow_mut();
        let mut service_occurrences: BTreeMap<String, usize> = BTreeMap::new();
        for provision in &capture.services {
            let occurrence = service_occurrences
                .entry(provision.iface.clone())
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(0);
            let occurrence = *occurrence;
            let declared =
                declared_provision_slot(&manifest.provided_services, &provision.iface, occurrence)
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
                heads: declared.heads.clone(),
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
        // CTX-1244: a failed activation arms nothing; drop any stale timer
        // rows so a dead handle can never fire or wake.
        self.clear_plugin_timers(id);
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

    /// Shared brokered-call ledger (diagnostics, tests, dynapi 4/4 input).
    #[must_use]
    pub fn service_call_ledger(&self) -> &Rc<RefCell<ServiceCallLedger>> {
        &self.service_call_ledger
    }

    /// Shared live command directory (diagnostics, tests).
    #[must_use]
    pub fn command_directory(&self) -> &Rc<RefCell<CommandDirectory>> {
        &self.command_directory
    }

    /// Shared native sensor host (CTX-1199; diagnostics, tests).
    ///
    /// Tests replace the contents with fixture sources plus a pinned clock
    /// without republishing: publication is structural (descriptors only),
    /// dispatch reads the live host contents.
    #[must_use]
    pub fn sensor_host(&self) -> &Rc<RefCell<system_sensors::host::SensorHost>> {
        &self.sensor_host
    }

    /// Shared native device host (CTX-1238; diagnostics, tests).
    ///
    /// Tests replace the contents with fixture roots plus a pinned clock
    /// without republishing: publication is structural (descriptors only),
    /// dispatch reads the live host contents.
    #[must_use]
    pub fn device_host(&self) -> &Rc<RefCell<device_providers::host::DeviceHost>> {
        &self.device_host
    }

    /// Shared native audio host (CTX-1238; diagnostics, tests).
    ///
    /// Tests replace the contents with a fixture spawner plus a pinned
    /// clock without republishing: publication is structural (descriptors
    /// only), dispatch reads the live host contents.
    #[must_use]
    pub fn audio_host(&self) -> &Rc<RefCell<audio::AudioHost>> {
        &self.audio_host
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

/// Bind the nth provision of one interface to the nth declared
/// (interface, version) in manifest order.
///
/// Side-by-side versions (dynapi negotiation, bitty#2006) share one short
/// interface: each `provide` call binds by occurrence, versions resolve
/// from the manifest (never from Lua), and a provision past the declared
/// count finds no slot (`None` → undeclared). Shared by
/// [`validate_capture`] and [`PluginRuntime::publish_services`] so the
/// gate and the publisher can never disagree on the binding.
fn declared_provision_slot<'a>(
    provided: &'a [bitty_plugin_host::manifest::ProvidedService],
    iface: &str,
    occurrence: usize,
) -> Option<&'a bitty_plugin_host::manifest::ProvidedService> {
    provided
        .iter()
        .filter(|service| service.iface == iface)
        .nth(occurrence)
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
    granted: &BTreeSet<CapabilityId>,
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
    // (version, schemas, and backing heads resolve from the manifest, never
    // from Lua). Unlike commands there is no reverse equivalence: a declared
    // service with no provision simply never publishes, and resolution fails
    // closed (`E_SERVICE_RESOLUTION`) instead of activation failing.
    //
    // Dynapi gating (bitty#2005): the declared entry must name at least one
    // backing head and every head must sit in this activation's grant set.
    // Unbacked provisions and head-spoofing (naming a head never granted)
    // fail activation here with a naming diagnostic, so no unbacked or
    // ungranted record ever reaches the directory.
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
    let mut seen_services = BTreeSet::new();
    let mut service_occurrences: BTreeMap<String, usize> = BTreeMap::new();
    for provision in &capture.services {
        // Dynapi arbitration (bitty#2003): provisions name the short
        // interface only, never a Core-reserved spelling. A hand-built
        // capture bypasses manifest validation, so the commit gate
        // re-checks the reference grammar here; the owner stays implied
        // (this plugin) and pinned by registry identity.
        match parse_service_reference(&provision.iface, "services.provided") {
            Ok((None, _)) => {}
            Ok((Some(_), short)) => {
                return Err(PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!(
                        "service '{}' must name the short interface '{short}' (owner is implied)",
                        provision.iface
                    ),
                });
            }
            Err(error) => {
                return Err(PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!("service '{}' is not valid ({error})", provision.iface),
                });
            }
        }
        // Side-by-side versions (dynapi negotiation, bitty#2006): the nth
        // provision of one interface binds to the nth declared
        // (interface, version) via [`declared_provision_slot`]. Versions
        // come from the manifest, never from Lua; a provision past the
        // declared count fails closed as undeclared.
        let occurrence = service_occurrences
            .entry(provision.iface.clone())
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(0);
        let occurrence = *occurrence;
        let declared =
            declared_provision_slot(&manifest.provided_services, &provision.iface, occurrence);
        let Some(declared) = declared else {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "service '{}' is not declared in services.provided",
                    provision.iface
                ),
            });
        };
        if declared.heads.is_empty() {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "service '{}' declares no backing capability head (services.provided.{}.heads)",
                    provision.iface, provision.iface
                ),
            });
        }
        for head in &declared.heads {
            let covered = granted
                .iter()
                .any(|capability| capability.as_str() == head.as_str());
            if !covered {
                return Err(PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!(
                        "service '{}' backing head '{head}' is not granted",
                        provision.iface
                    ),
                });
            }
        }
        if !seen_services.insert((provision.iface.clone(), declared.version.clone())) {
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
        self.settings_alias_registry = None;
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
        self.settings_alias_registry = None;
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

    #[cfg(feature = "input-vm-test-support")]
    #[test]
    fn input_vm_boundary_counter_distinguishes_missing_busy_and_zero() {
        // Constructor-only control: no Lua source is evaluated or dispatched.
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig::default());
        let manifest = manifest_toml::parse_manifest(
            br#"[plugin]
id = "bitty-featured.input-counter"
name = "Input counter fixture"
version = "0.0.1"
description = "Read-only owning-VM accounting"
[compat]
plugin-api = "^1.0"
"#,
        )
        .expect("in-memory manifest");
        let id = manifest.id().clone();
        assert_eq!(
            runtime.vm_total_executions(&id),
            Err(VmExecutionCountError::MissingVm)
        );
        runtime.insert_package(
            id.clone(),
            PluginPackage {
                manifest,
                module_root: PathBuf::new(),
                source_class: SourceClass::LocalPath,
                unverified: true,
                granted: None,
            },
        );
        assert_eq!(
            runtime.vm_total_executions(&id),
            Err(VmExecutionCountError::MissingVm)
        );
        let vm = Rc::new(RefCell::new(
            build_plugin_vm(id.as_str(), Some(VmBudgets::default())).expect("bounded pure VM"),
        ));
        runtime.entries.get_mut(&id).expect("entry").vm = Some(vm.clone());
        assert_eq!(runtime.vm_total_executions(&id), Ok(0));
        let busy = vm.borrow_mut();
        assert_eq!(
            runtime.vm_total_executions(&id),
            Err(VmExecutionCountError::Busy)
        );
        drop(busy);
        assert_eq!(runtime.vm_total_executions(&id), Ok(0));
    }

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

    fn service_test_manifest(
        provided: Vec<bitty_plugin_host::manifest::ProvidedService>,
    ) -> PluginManifest {
        use bitty_plugin_host::manifest::{
            CapabilityRequests, Compat, LazyTriggers, PluginIdentity, PluginLimits,
        };
        PluginManifest {
            identity: PluginIdentity {
                id: PluginId::new("xuepoo.test").expect("id"),
                name: "Test".to_string(),
                version: "0.1.0".to_string(),
                description: "test".to_string(),
                license: None,
            },
            compat: Compat {
                bitty: None,
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            provided_services: provided,
            required_services: Vec::new(),
            capabilities: CapabilityRequests::default(),
            tools: Vec::new(),
            network: Vec::new(),
            limits: PluginLimits::default(),
            lazy: LazyTriggers {
                commands: Vec::new(),
                events: Vec::new(),
                claims: Vec::new(),
            },
            settings: Vec::new(),
            next_steps: Vec::new(),
            raw_bytes_len: 512,
        }
    }

    fn backed_service(
        iface: &str,
        heads: Vec<&str>,
    ) -> bitty_plugin_host::manifest::ProvidedService {
        bitty_plugin_host::manifest::ProvidedService {
            iface: iface.to_string(),
            version: "1.0.0".to_string(),
            args_schema: None,
            result_schema: None,
            heads: heads.iter().map(|head| (*head).to_string()).collect(),
        }
    }

    #[test]
    fn validate_capture_rejects_unbacked_and_ungranted_provisions() {
        use bitty_lua::ServiceProvision;
        let id = PluginId::new("xuepoo.test").expect("id");
        // Method validation runs after the heads gate, so an empty method
        // list isolates the gate under test (granted heads fall through to
        // the method-count failure).
        let mut capture = RegistrationCapture::new();
        capture.services.push(ServiceProvision {
            iface: "calc.add".to_string(),
            methods: Vec::new(),
        });
        let granted = parsed_grants(&["runtime.inspect"]);

        let manifest = service_test_manifest(vec![backed_service("calc.add", Vec::new())]);
        let err = validate_capture(&id, &manifest, &capture, &granted)
            .expect_err("unbacked provision must fail");
        let text = format!("{err}");
        assert!(text.contains("calc.add"), "names the iface: {text}");
        assert!(text.contains("heads"), "names the field: {text}");

        let manifest =
            service_test_manifest(vec![backed_service("calc.add", vec!["workspace.read"])]);
        let err = validate_capture(&id, &manifest, &capture, &granted)
            .expect_err("spoofed provision must fail");
        assert!(
            format!("{err}").contains("workspace.read"),
            "names the head: {err}"
        );

        let manifest =
            service_test_manifest(vec![backed_service("calc.add", vec!["runtime.inspect"])]);
        let err = validate_capture(&id, &manifest, &capture, &granted)
            .expect_err("empty methods still fail");
        assert!(
            format!("{err}").contains("method count"),
            "granted heads pass the gate: {err}"
        );
    }

    #[test]
    fn validate_capture_rejects_reserved_and_qualified_provisions() {
        // Dynapi arbitration (bitty#2003): the commit gate re-checks the
        // reference grammar because a hand-built capture bypasses manifest
        // validation. Core-reserved and owner-qualified provisions fail
        // activation with a naming diagnostic.
        use bitty_lua::ServiceProvision;
        let id = PluginId::new("xuepoo.test").expect("id");
        let granted = parsed_grants(&["runtime.inspect"]);
        for (iface, want) in [
            ("bitty.echo", "Core-reserved"),
            ("xuepoo.test:calc.add", "short interface"),
        ] {
            let mut capture = RegistrationCapture::new();
            capture.services.push(ServiceProvision {
                iface: iface.to_string(),
                methods: Vec::new(),
            });
            let manifest =
                service_test_manifest(vec![backed_service(iface, vec!["runtime.inspect"])]);
            let err = validate_capture(&id, &manifest, &capture, &granted)
                .expect_err("arbitration violation must fail");
            let text = format!("{err}");
            assert!(text.contains(iface), "names the iface: {text}");
            assert!(text.contains(want), "names the rule: {text}");
        }
    }

    #[test]
    fn declared_provision_slot_binds_by_occurrence() {
        // Dynapi negotiation (bitty#2006): the nth provision of one
        // interface binds to the nth declared (interface, version) in
        // manifest order; past the declared count there is no slot.
        // (Stashed functions need a VM, so the binding rule is pinned
        // here on plain declarations; end-to-end activation covers the
        // full path in `plugin_services`.)
        let mut one = backed_service("duet.play", vec!["runtime.inspect"]);
        one.version = "1.0.0".to_string();
        let mut two = backed_service("duet.play", vec!["workspace.read"]);
        two.version = "2.0.0".to_string();
        let mut other = backed_service("other.svc", vec!["runtime.inspect"]);
        other.version = "3.0.0".to_string();
        let provided = vec![one, other, two];
        assert_eq!(
            declared_provision_slot(&provided, "duet.play", 0)
                .expect("first slot")
                .version,
            "1.0.0"
        );
        assert_eq!(
            declared_provision_slot(&provided, "duet.play", 1)
                .expect("second slot")
                .version,
            "2.0.0"
        );
        assert_eq!(
            declared_provision_slot(&provided, "other.svc", 0)
                .expect("other iface unaffected")
                .version,
            "3.0.0"
        );
        assert!(
            declared_provision_slot(&provided, "duet.play", 2).is_none(),
            "past the declared count there is no slot"
        );
        assert!(
            declared_provision_slot(&provided, "missing.svc", 0).is_none(),
            "unknown interfaces have no slot"
        );
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

    #[test]
    fn alias_registry_requires_complete_current_discovery() {
        let first = PluginId::new("xuepoo.bar").expect("id");
        let second = PluginId::new("other.bar").expect("id");
        let complete = vec![(first.clone(), Ok(())), (second, Ok(()))];
        assert_eq!(
            PluginRuntime::complete_discovery_ids(
                vec!["xuepoo.bar".to_string(), "other.bar".to_string()],
                &complete,
            ),
            Some(vec!["other.bar".to_string(), "xuepoo.bar".to_string()])
        );

        let incomplete = vec![
            (first, Ok(())),
            (
                invalid_plugin_id(),
                Err(PluginRuntimeError::Io("index unavailable".to_string())),
            ),
        ];
        assert_eq!(
            PluginRuntime::complete_discovery_ids(vec!["xuepoo.bar".to_string()], &incomplete),
            None
        );
        let empty: DiscoveryResults = Vec::new();
        assert_eq!(
            PluginRuntime::complete_discovery_ids(Vec::new(), &empty),
            Some(Vec::new())
        );
    }
}
