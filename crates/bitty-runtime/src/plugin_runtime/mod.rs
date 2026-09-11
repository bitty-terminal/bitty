//! Plugin host runtime: per-plugin VM lifecycle, bridge, and host services.
//!
//! This module implements the `bitty-runtime` half of the ratified
//! `plugin-host-runtime-rfc` Gap A plus the minimal Gap C host services. Policy
//! stays in `bitty-plugin-host` (manifest validation, capability grammar,
//! grants, registry, event pipeline); the VM seam stays in `bitty-lua`; this
//! module orchestrates one `!Send` piccolo VM per `(PluginId, generation)` on a
//! single owning thread.
//!
//! Lifecycle: `Unloaded -> Loading -> Activating -> Active -> Suspended ->
//! Disposing -> Disposed`, with `Failed` terminal for a generation. Activation
//! executes the fixed `init.lua`, captures registrations, validates them
//! against the manifest, and commits atomically. `bitty --safe` never creates a
//! third-party VM.
//!
//! Source resolution here is deliberately minimal (bundled roots only); the
//! full XDG store, integrity re-verification, and local-path flow are owned by
//! the Gap B source-resolution task, and the numeric bounds below are the
//! RFC's proposed defaults.

pub mod manifest_toml;
pub mod services;
pub mod store;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::host::DEFAULT_HOST_DEADLINE_MS;
use bitty_lua::{HostServices, LuaVm, MarshallingLimits, RegistrationCapture};
use bitty_plugin_host::DropPolicy;
use bitty_plugin_host::grant::GrantRecord;
use bitty_plugin_host::host::PluginHost;
use bitty_plugin_host::manifest::{PluginId, PluginManifest};

pub use services::{
    EmptySettings, Notification, NotificationQueue, PluginServices, SettingsSource, SnapshotSource,
    UnavailableSnapshot,
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

/// How a discovered package was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClass {
    /// Shipped alongside the application (read-only).
    Bundled,
    /// Any other discovered source; skipped under `--safe`.
    ThirdParty,
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
}

/// Configuration for a [`PluginRuntime`].
pub struct PluginRuntimeConfig {
    /// `bitty --safe`: never create a third-party VM.
    pub safe_mode: bool,
    /// Root for plugin persistent state (`$XDG_DATA_HOME/bitty/plugins-state`).
    pub data_dir: Option<PathBuf>,
    /// Trusted roots for application-shipped (`bundled`) packages.
    ///
    /// Provenance is derived from the root, never from the manifest id: a
    /// package can only be `bundled` when it is discovered under one of these
    /// roots. `--safe` still loads these (RFC A.4 rule 6 only forbids
    /// third-party VMs).
    pub bundled_roots: Vec<PathBuf>,
    /// Untrusted roots (`local-path`, registry, git). Packages found here are
    /// [`SourceClass::ThirdParty`] regardless of their self-declared id, and
    /// `--safe` never creates a VM for them.
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
            Self::Lifecycle { plugin, detail } => {
                write!(f, "plugin '{plugin}' lifecycle error: {detail}")
            }
            Self::Host(detail) => write!(f, "plugin host error: {detail}"),
            Self::Vm(detail) => write!(f, "plugin vm error: {detail}"),
            Self::Capture { plugin, detail } => {
                write!(f, "registration capture error for '{plugin}': {detail}")
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
/// The struct is `!Send` because it owns piccolo VMs; callers use it on the
/// single owning thread (the application's cold path).
pub struct PluginRuntime {
    safe_mode: bool,
    data_dir: Option<PathBuf>,
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
    /// Provenance (and therefore `--safe` eligibility) comes from the root the
    /// package was found under, never from its self-declared id. Bundled roots
    /// are scanned first so a bundled package wins an id collision over a
    /// third-party one. Fail-closed: an invalid manifest is reported and
    /// skipped, never loaded. Returns `(id, result)` pairs in discovery order.
    pub fn discover(&mut self) -> Vec<(PluginId, Result<(), PluginRuntimeError>)> {
        let mut roots: Vec<(SourceClass, PathBuf)> = self
            .bundled_roots
            .iter()
            .cloned()
            .map(|root| (SourceClass::Bundled, root))
            .collect();
        roots.extend(
            self.third_party_roots
                .iter()
                .cloned()
                .map(|root| (SourceClass::ThirdParty, root)),
        );
        let mut results = Vec::new();
        for (source_class, root) in roots {
            let packages = match discover_root(&root, source_class) {
                Ok(packages) => packages,
                Err(error) => {
                    results.push((
                        PluginId::new("invalid.root").expect("static id is valid"),
                        Err(error),
                    ));
                    continue;
                }
            };
            for package in packages {
                let id = package.manifest.id().clone();
                if self.entries.contains_key(&id) {
                    continue;
                }
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
                self.order.push(id.clone());
                results.push((id, Ok(())));
            }
        }
        results
    }

    /// Activate one discovered plugin.
    ///
    /// # Errors
    ///
    /// [`PluginRuntimeError`] for lifecycle, manifest, module-tree, host,
    /// capture-validation, or VM failures. Failure leaves no partial
    /// activation.
    pub fn activate(&mut self, id: &PluginId) -> Result<ActivationReport, PluginRuntimeError> {
        let (manifest, module_root, init_path) = {
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
            if self.safe_mode && entry.package.source_class == SourceClass::ThirdParty {
                return Ok(ActivationReport {
                    plugin: id.clone(),
                    state: LifecycleState::Unloaded,
                    commands: 0,
                    events: 0,
                    skipped_safe_mode: true,
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
            )
        };
        let source = read_init(&init_path)?;

        // Policy half: declare, resolve, register, grant, activate.
        let hash = manifest.manifest_hash();
        let required =
            manifest
                .capabilities
                .all_ids()
                .map_err(|error| PluginRuntimeError::Manifest {
                    plugin: id.to_string(),
                    detail: error.to_string(),
                })?;
        let policy = (|| -> Result<(), PluginRuntimeError> {
            self.host.declare(manifest.clone())?;
            self.host.resolve(id)?;
            self.host.register(id)?;
            self.host
                .insert_grant(GrantRecord::granted(id.clone(), hash, required.clone(), 0));
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
        let terminal_read = required
            .iter()
            .any(|capability| capability.as_str() == "terminal.semantic-read");
        let platform_notify = required
            .iter()
            .any(|capability| capability.as_str() == "platform.notify");
        let plugin_services = Rc::new(PluginServices::new(
            id.as_str(),
            store,
            self.settings.clone(),
            self.snapshot.clone(),
            self.notifications.clone(),
            terminal_read,
            platform_notify,
        ));
        let mut vm = LuaVm::new(id.as_str());
        vm.with_module_root(module_root.clone());
        {
            let entry = self.entries.get_mut(id).expect("entry exists");
            entry.state = LifecycleState::Loading;
            entry.generation = entry.generation.saturating_add(1);
            entry.services = Some(plugin_services.clone());
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
        entry.registrations = capture;
        entry.vm = Some(vm);
        entry.state = LifecycleState::Active;
        Ok(ActivationReport {
            plugin: id.clone(),
            state: LifecycleState::Active,
            commands,
            events,
            skipped_safe_mode: false,
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
        entry.state = LifecycleState::Disposed;
        let _ = self.host.remove(id);
        Ok(())
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

/// Validate a registration capture against the manifest, fail closed.
fn validate_capture(
    id: &PluginId,
    manifest: &PluginManifest,
    capture: &RegistrationCapture,
) -> Result<(), PluginRuntimeError> {
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
        if !declared_events.contains(subscription.kind.as_str()) {
            return Err(PluginRuntimeError::Capture {
                plugin: id.to_string(),
                detail: format!(
                    "event '{}' is not declared in the manifest",
                    subscription.kind
                ),
            });
        }
        seen_events.insert(subscription.kind.clone());
    }
    Ok(())
}

/// Verify a module tree against the RFC bounds, rejecting native artifacts and
/// symlink escapes.
fn verify_module_tree(id: &PluginId, root: &Path) -> Result<(), PluginRuntimeError> {
    let canonical =
        std::fs::canonicalize(root).map_err(|error| PluginRuntimeError::ModuleTree {
            plugin: id.to_string(),
            detail: format!("module root unreadable: {error}"),
        })?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![canonical.clone()];
    while let Some(directory) = stack.pop() {
        let entries =
            std::fs::read_dir(&directory).map_err(|error| PluginRuntimeError::ModuleTree {
                plugin: id.to_string(),
                detail: format!("module directory unreadable: {error}"),
            })?;
        for entry in entries {
            let entry = entry.map_err(|error| PluginRuntimeError::ModuleTree {
                plugin: id.to_string(),
                detail: format!("module entry unreadable: {error}"),
            })?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|error| PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: format!("module metadata unreadable: {error}"),
                })?;
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            files += 1;
            if files > PLUGIN_MODULE_MAX_FILES {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: "module tree exceeds the 4096-file ceiling".to_string(),
                });
            }
            bytes = bytes.saturating_add(metadata.len());
            if bytes > PLUGIN_MODULE_TREE_MAX_BYTES as u64 {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: "module tree exceeds the 16 MiB ceiling".to_string(),
                });
            }
            if path.as_os_str().len() > PLUGIN_MODULE_PATH_MAX_BYTES {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: "module path exceeds the 1024-byte ceiling".to_string(),
                });
            }
            if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
                if matches!(extension, "so" | "dll" | "dylib" | "node") {
                    return Err(PluginRuntimeError::ModuleTree {
                        plugin: id.to_string(),
                        detail: "native artifacts are not loadable modules".to_string(),
                    });
                }
            }
            let resolved =
                std::fs::canonicalize(&path).map_err(|error| PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: format!("module path unresolved: {error}"),
                })?;
            if !resolved.starts_with(&canonical) {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: id.to_string(),
                    detail: "module path escapes the plugin root".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Resolve the fixed `init.lua`: `<root>/init.lua` or `<root>/<module>/init.lua`.
fn entry_point(module_root: &Path, id: &PluginId) -> Option<PathBuf> {
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
}
