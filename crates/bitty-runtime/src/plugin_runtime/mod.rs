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

pub mod fs;
pub mod manifest_toml;
pub mod package;
pub mod resolution;
pub mod services;
pub mod spawn;
pub mod store;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::host::DEFAULT_HOST_DEADLINE_MS;
use bitty_lua::{HostServices, LuaVm, MarshallingLimits, RegistrationCapture};
use bitty_plugin_host::DropPolicy;
use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::grant::GrantRecord;
use bitty_plugin_host::host::PluginHost;
use bitty_plugin_host::manifest::{PluginId, PluginManifest};

pub use fs::{FakeFileSystem, FileSystem, NativeFileSystem, write_atomic_durably};
pub use resolution::{
    CURRENT_POINTER_FILE, PLUGIN_INDEX_STATE_VERSION, PluginRecord, content_digest, load_index,
    load_index_with_fs, write_index, write_index_with_fs,
};
pub use services::{
    EmptyEnv, EmptySettings, EnvSource, MAX_ENV_GRANTS, MAX_ENV_VALUE_BYTES, MapEnv, Notification,
    NotificationQueue, PluginServices, ProcessEnv, SettingsSource, SnapshotSource, UiAccess,
    UiBlock, UiBlocks, UnavailableSnapshot,
};
pub use store::PluginStore;
// Bridge value/error types the host-service traits are expressed in, so the
// application can implement `SettingsSource`/`SnapshotSource` without taking a
// direct `bitty-lua` dependency.
pub use bitty_lua::{BridgeError, LuaValue};

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
    vm: Option<LuaVm>,
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
        }
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

    /// Drain accepted notifications (async hand-off side).
    pub fn drain_notifications(&mut self) -> Vec<Notification> {
        self.notifications.borrow_mut().drain()
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
            claims: manifest.lazy.claims.clone(),
        });
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
        let mut vm = match build_plugin_vm(id.as_str(), Some(VmBudgets::default())) {
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
        let entry = self.entries.get_mut(id).expect("entry exists");
        let source_class = entry.package.source_class;
        let unverified = entry.package.unverified;
        entry.registrations = capture;
        entry.vm = Some(vm);
        entry.state = LifecycleState::Active;
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
        }
        let _ = self.host.suspend(id);
        Ok(())
    }

    /// Resume a suspended plugin (`Suspended -> Active`).
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the transition is invalid.
    pub fn resume(&mut self, id: &PluginId) -> Result<(), PluginRuntimeError> {
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
        }
        entry.state = LifecycleState::Disposed;
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
        self.dispose(id)?;

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

        self.activate(id)
    }

    /// Dispatch one captured command, invoking its `run` function under budget.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError::Lifecycle`] when the plugin is not active;
    /// [`PluginRuntimeError::Capture`] when the command is unknown;
    /// [`PluginRuntimeError::Vm`] when the callback fails or suspends.
    pub fn dispatch_command(
        &mut self,
        id: &PluginId,
        command_id: &str,
        args: &[LuaValue],
    ) -> Result<LuaValue, PluginRuntimeError> {
        let run = {
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
            entry
                .registrations
                .commands
                .iter()
                .find(|command| format!("{}:{}", id.as_str(), command.id) == qualified)
                .map(|command| command.run.clone())
                .ok_or_else(|| PluginRuntimeError::Capture {
                    plugin: id.to_string(),
                    detail: format!("command '{command_id}' was not registered"),
                })?
        };
        let entry = self.entries.get_mut(id).expect("entry exists");
        let vm = entry
            .vm
            .as_mut()
            .ok_or_else(|| lifecycle_error(id, "no VM"))?;
        vm.call_function(&run, args)
            .map_err(|error| PluginRuntimeError::Vm(error.to_string()))
    }

    /// Deliver an observation/lifecycle event to every active subscriber.
    ///
    /// Returns the number of handler invocations that completed. Bounded and
    /// non-blocking; per-handler failures are contained to the owning plugin.
    pub fn deliver_event(&mut self, kind: &str, payload: &LuaValue) -> usize {
        self.event_sequence = self.event_sequence.saturating_add(1);
        let envelope = LuaValue::table([
            ("kind", LuaValue::String(kind.to_string())),
            ("sequence", LuaValue::Integer(self.event_sequence as i64)),
            ("payload", payload.clone()),
        ]);
        let ids: Vec<PluginId> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == LifecycleState::Active)
            .map(|(id, _)| id.clone())
            .collect();
        let mut delivered = 0usize;
        for id in ids {
            let handlers: Vec<_> = match self.entries.get(&id) {
                Some(entry) => entry
                    .registrations
                    .events
                    .iter()
                    .filter(|subscription| subscription.kind == kind)
                    .map(|subscription| subscription.handler.clone())
                    .collect(),
                None => continue,
            };
            let Some(entry) = self.entries.get_mut(&id) else {
                continue;
            };
            let Some(vm) = entry.vm.as_mut() else {
                continue;
            };
            for handler in handlers {
                if vm
                    .call_function(&handler, std::slice::from_ref(&envelope))
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
                PluginStore::load(path).map_err(PluginRuntimeError::Io)
            }
            None => Ok(PluginStore::in_memory()),
        }
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
}

fn lifecycle_error(id: &PluginId, detail: &str) -> PluginRuntimeError {
    PluginRuntimeError::Lifecycle {
        plugin: id.to_string(),
        detail: detail.to_string(),
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
        .map(|command| command.as_str())
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
    Ok(())
}

/// Verify a module tree against the RFC bounds, rejecting native artifacts and
/// symlink escapes. The walk is shared with [`resolution`] so the loaded tree
/// and the digested tree are always the same bounded input.
fn verify_module_tree(id: &PluginId, root: &Path) -> Result<(), PluginRuntimeError> {
    resolution::scan_module_tree(id.as_str(), root).map(|_| ())
}

/// Extract `bitty.env` keys from the activation grant snapshot (CTX-0330).
///
/// Only `env.read:<KEY>` grants contribute, and only with a well-shaped key
/// (`[A-Za-z_][A-Za-z0-9_]*`, `1..=ENV_KEY_MAX_BYTES` bytes, shared rule in
/// [`bitty_lua::env_key_shape_ok`]): a recorded grant with a malformed key
/// fails closed as store integrity before any VM exists, like any other
/// undeclared grant. The set is capped at [`services::MAX_ENV_GRANTS`].
fn env_grant_keys(
    id: &PluginId,
    granted: &BTreeSet<CapabilityId>,
) -> Result<BTreeSet<String>, PluginRuntimeError> {
    let mut keys = BTreeSet::new();
    for capability in granted {
        let Some(key) = capability.as_str().strip_prefix("env.read:") else {
            continue;
        };
        if !bitty_lua::env_key_shape_ok(key) {
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
        let granted = parsed_grants(&["terminal.semantic-read", "env.read:HOME", "env.read:PATH"]);
        assert_eq!(
            env_grant_keys(&id, &granted).expect("keys extract"),
            BTreeSet::from(["HOME".to_string(), "PATH".to_string()])
        );
        assert!(
            env_grant_keys(&id, &BTreeSet::new())
                .expect("empty")
                .is_empty()
        );
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
