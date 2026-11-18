#![forbid(unsafe_code)]
//! The production producer chain for panel restore records (bitty#3243).
//!
//! `crates/bitty-runtime/tests/panel_restore_path.rs` proves what the restore
//! *does* with a record. Until bitty#3243 nothing in a shipped binary *produced*
//! one: `Runtime::set_panel_restore_records` had no caller outside these tests,
//! no save encoded a `bitty_ui::RestartManifest`, no load decoded one, and the
//! shipped order restored the session before the panel host existed — so the
//! restore planned zero records and would have dropped every one it was given.
//!
//! These rows drive the real chain, and each one is paired with the producer it
//! depends on, so reverting a single producer turns a row red:
//!
//! * **save** — `Runtime::save_session_to_path` commits a manifest beside the
//!   session file; `a_save_writes_a_manifest_for_every_live_panel` reads the
//!   document back off disk and asserts its records, so removing the encode
//!   call leaves no document to read;
//! * **load** — `Runtime::load_session_from_path` decodes that document into
//!   configured records before the snapshot applies;
//!   `a_saved_manifest_becomes_a_restore_record_on_load` asserts the record
//!   exists before any restore could have read it, so removing the decode call
//!   leaves nothing configured;
//! * **order** — a restore that finds no panel host defers its panel step and
//!   the host install commits it;
//!   `the_panel_step_waits_for_the_host_a_restore_finds_none` asserts the empty
//!   report *and* the commit that follows the install, so reverting the
//!   deferral turns the row red with `PanelRestoreDrop::NoPanelHost`;
//! * **the outcome** — `a_save_load_restore_round_trip_presents_incompatible_for_a_newer_provider_contract`
//!   is the whole chain: a live panel saved in one runtime, a restart manifest
//!   on disk, a fresh runtime that loads it, a host whose provider registry
//!   declares a schema above `PROVIDER_MANIFEST_VERSION_V1`, and a restored
//!   container that presents `incompatible`, keeps its allocation epoch, holds
//!   no controller generation, and is granted nothing.
//!   `the_same_chain_binds_when_the_declared_contract_is_the_one_this_runtime_reads`
//!   is its version-comparison control on the registration side.
//!
//! The filesystem is real: the backend below writes and reads the actual session
//! and manifest files under a per-test temporary directory, so "on disk" means
//! on disk. Every test removes its directory.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use bitty_plugin_host::PluginId;
use bitty_runtime::registry::{
    PROVIDER_MANIFEST_VERSION_V1, PanelProviderManifest, PanelRegistryConfig, PanelRuntime,
    PanelType, PlaceholderReason,
};
use bitty_runtime::runtime::panel_restore::{
    PanelDriverAnswer, PanelRestoreBinding, PanelRestoreDriverSource, PanelRestoreOutcome,
};
use bitty_runtime::{
    LayoutNode, PaneAttachment, PaneRoute, PaneSnapshot, PresentationMode, Runtime, SessionError,
    SessionFileBackend, SessionSnapshot, View, ViewId, WorkspaceSnapshot, panel_manifest_path,
};
use bitty_ui::{PanelId, PersistedPanel, decode_manifest};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";

// ---------------------------------------------------------------------------
// A backend that really writes both documents
// ---------------------------------------------------------------------------

/// Writes and reads real files; replays one canned snapshot.
///
/// The session *body* is canned (`bitty-storage`'s codec is not this chain's
/// subject) but the bytes go through the same
/// [`SessionFileBackend`](bitty_runtime::SessionFileBackend) calls the shipped
/// backend implements, so the manifest a save commits is a real file a fresh
/// runtime reads back.
#[derive(Debug)]
struct DiskBackend {
    snapshot: SessionSnapshot,
}

impl SessionFileBackend for DiskBackend {
    fn encode_snapshot(&self, _snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
        Ok(b"session".to_vec())
    }

    fn decode_snapshot(&self, _bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
        Ok(self.snapshot.clone())
    }

    fn commit_session_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| SessionError::Io(err.to_string()))?;
        }
        fs::write(path, bytes).map_err(|err| SessionError::Io(err.to_string()))
    }

    fn load_session_bytes(&self, path: &Path) -> Result<Vec<u8>, SessionError> {
        match fs::read(path) {
            Ok(bytes) => Ok(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(SessionError::NotFound),
            Err(err) => Err(SessionError::Io(err.to_string())),
        }
    }

    fn session_file_for(
        &self,
        _xdg_state_home: Option<&str>,
        _home: Option<&str>,
    ) -> Option<PathBuf> {
        None
    }

    fn session_file(&self) -> Option<PathBuf> {
        None
    }
}

/// One workspace, one leaf, one attached pane: the smallest world the restore
/// accepts.
fn canned_snapshot() -> SessionSnapshot {
    let view = ViewId::new(1);
    SessionSnapshot {
        version: bitty_runtime::SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: String::from("restore"),
            layout: LayoutNode::leaf(View::new(view, 80, 24)),
            focus: Some(view),
            panes: vec![PaneSnapshot {
                view,
                cwd: None,
                scrollback: Vec::new(),
                attach: Some(PaneAttachment::Session),
                route: PaneRoute::Terminal,
                mode: PresentationMode::Tiled,
            }],
            floats: Vec::new(),
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    }
}

/// A per-test temporary root under the process temp directory.
fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "bitty-3243-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

/// The session path these rows hand their backends.
fn session_path(root: &Path) -> PathBuf {
    root.join("session.v3")
}

fn owner() -> PluginId {
    PluginId::new("acme.notes").expect("valid plugin id")
}

fn resolution() -> BTreeSet<String> {
    [CREATE, FOCUS]
        .iter()
        .map(|item| (*item).to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The writer runtime: one live, driven, titled panel
// ---------------------------------------------------------------------------

/// A runtime with a session backend, an installed panel host, and one live
/// helper panel attributed to the presented workspace and driven by `owner()`.
///
/// This is the state a save captures: the container is what the manifest
/// records, and the driver binding is why a load can resolve it at all.
fn writer(title: &str) -> (Runtime, Rc<RefCell<PanelRuntime>>, Arc<DiskBackend>) {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let backend = Arc::new(DiskBackend {
        snapshot: canned_snapshot(),
    });
    rt.set_session_backend(Some(backend.clone()));
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    rt.install_shared_scene_host(Rc::clone(&host));
    let workspace = rt.active_workspace_id();
    let handle = host
        .borrow_mut()
        .create_panel(PanelType::Helper, Some(workspace))
        .expect("the container allocates");
    host.borrow_mut()
        .set_panel_description(handle.id, handle.generation, title, "persisted panel")
        .expect("the title stores");
    let controller = handle.generation.next().expect("a controller generation");
    host.borrow_mut()
        .rebind_panel(
            handle.id,
            controller,
            &owner(),
            &resolution(),
            &BTreeSet::new(),
        )
        .expect("the driver binds");
    (rt, host, backend)
}

/// A fresh runtime that loads `session_path`, with no panel host installed yet
/// (the shipped order: the host arrives after the restore).
fn loader(backend: Arc<DiskBackend>, source: Option<Rc<dyn PanelRestoreDriverSource>>) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_session_backend(Some(backend));
    rt.set_panel_restore_driver_source(source);
    rt
}

/// The driver answer a plugin host would give for a record it serves.
#[derive(Debug)]
struct Drivers {
    owner: PluginId,
    resolution: BTreeSet<String>,
}

impl PanelRestoreDriverSource for Drivers {
    fn resolve(&self, _record: &PersistedPanel) -> PanelRestoreBinding {
        PanelRestoreBinding::new(
            PanelType::Helper,
            PanelDriverAnswer::Installed {
                owner: self.owner.clone(),
                resolution: self.resolution.clone(),
                consented: BTreeSet::new(),
            },
        )
    }
}

fn drivers() -> Rc<dyn PanelRestoreDriverSource> {
    Rc::new(Drivers {
        owner: owner(),
        resolution: resolution(),
    })
}

/// Registers one provider manifest on `host`'s provider registry.
fn register_provider(host: &Rc<RefCell<PanelRuntime>>, version: u32) {
    let manifest = PanelProviderManifest::parse(owner().as_str(), vec![PanelType::Helper], version)
        .expect("the manifest parses");
    host.borrow_mut()
        .register_panel_provider(manifest, true)
        .expect("the provider registers");
}

/// A panel host whose provider registry already declares `version`.
fn host_with_provider(version: u32) -> Rc<RefCell<PanelRuntime>> {
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    register_provider(&host, version);
    host
}

/// The placeholder a container presents, read through `host`.
fn placeholder_of(host: &Rc<RefCell<PanelRuntime>>, panel: PanelId) -> Option<PlaceholderReason> {
    host.borrow()
        .unbound_container_records(panel)
        .expect("container exists")
        .placeholder()
}

// ---------------------------------------------------------------------------
// Producer: save
// ---------------------------------------------------------------------------

/// The save side writes a manifest, and the manifest carries the live container.
///
/// Revert the encode call in `Runtime::save_session_to_path` and there is no
/// document at all, so every assertion below fails.
#[test]
fn a_save_writes_a_manifest_for_every_live_panel() {
    let root = temp_root("save");
    let path = session_path(&root);
    let (rt, host, _backend) = writer("logs");
    let live = host
        .borrow()
        .live_panel_records()
        .into_iter()
        .next()
        .expect("one live container");

    rt.save_session_to_path(&path).expect("the session saves");

    let manifest_path = panel_manifest_path(&path);
    let document = fs::read_to_string(&manifest_path)
        .expect("the save committed a restart manifest beside the session file");
    let manifest = decode_manifest(&document).expect("the document decodes");
    assert_eq!(manifest.len(), 1, "one record per live container");
    let record = &manifest.panels()[0];
    assert_eq!(
        record.key.persistent.as_str(),
        format!("helper-{}", live.id.0),
        "the durable key names the container kind and its allocation handle"
    );
    assert_eq!(
        record.key.workspace.get(),
        1,
        "the record carries the persisted workspace ordinal, not the live slot id"
    );
    assert_eq!(record.title, "logs", "the stored title travels");
    assert_eq!(
        record.id,
        PanelId::new(live.id.0),
        "the advisory id is the allocation handle the panel had before the restart"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A world with no panels writes an empty document rather than no document.
#[test]
fn a_save_with_no_live_panels_writes_an_empty_manifest() {
    let root = temp_root("save-empty");
    let path = session_path(&root);
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let backend = Arc::new(DiskBackend {
        snapshot: canned_snapshot(),
    });
    rt.set_session_backend(Some(backend));

    rt.save_session_to_path(&path).expect("the session saves");

    let document = fs::read_to_string(panel_manifest_path(&path))
        .expect("the manifest is written even when it is empty");
    assert!(
        document.is_empty(),
        "no records, and no stale document left"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Producer: load
// ---------------------------------------------------------------------------

/// The load side decodes the manifest into configured records, before any
/// restore could read them.
///
/// Revert the decode call in `Runtime::load_session_from_path` and the record
/// set stays empty, so this row fails on its first assertion.
#[test]
fn a_saved_manifest_becomes_a_restore_record_on_load() {
    let root = temp_root("load");
    let path = session_path(&root);
    let (rt, _host, backend) = writer("logs");
    rt.save_session_to_path(&path).expect("the session saves");

    let mut second = loader(backend, None);
    second
        .load_session_from_path(&path)
        .expect("the session loads");

    let records = second.panel_restore_records();
    assert_eq!(records.len(), 1, "the decoded record is configured");
    assert_eq!(records[0].record.title, "logs");
    assert_eq!(
        records[0].panel_type,
        PanelType::Helper,
        "the durable key's prefix names the container kind"
    );
    assert_eq!(
        records[0].driver,
        PanelDriverAnswer::Missing,
        "no driver source installed is the shipped state: the container comes back unloaded"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A missing manifest is a clean start for panels, and only for panels.
#[test]
fn a_missing_manifest_leaves_the_session_restore_untouched() {
    let root = temp_root("load-missing");
    let path = session_path(&root);
    let (rt, _host, backend) = writer("logs");
    rt.save_session_to_path(&path).expect("the session saves");
    // The session file stays; the manifest is gone (an older build wrote the
    // session, or the sidecar was removed).
    fs::remove_file(panel_manifest_path(&path)).expect("the manifest removes");

    let mut rt = loader(backend, Some(drivers()));
    let summary = rt
        .load_session_from_path(&path)
        .expect("a missing manifest never costs the session");
    assert_eq!(summary.workspaces, 1);
    assert!(rt.panel_restore_records().is_empty());
    assert!(rt.panel_restore_report().is_empty());
    assert!(!rt.panel_restore_pending());
    let _ = fs::remove_dir_all(&root);
}

/// A record whose durable key names no container kind this runtime mints is not
/// restored: a record is not a data channel for types.
#[test]
fn a_record_without_a_container_kind_is_not_configured() {
    let root = temp_root("load-kind");
    let path = session_path(&root);
    let backend = Arc::new(DiskBackend {
        snapshot: canned_snapshot(),
    });
    fs::create_dir_all(&root).expect("the root creates");
    fs::write(
        panel_manifest_path(&path),
        "v=2;id=9;ws=1;pid=logs;title=logs",
    )
    .expect("the document writes");
    let mut rt = loader(backend, None);

    let load = rt.load_panel_manifest_from_path(&path);

    assert!(load.present, "the document was read");
    assert_eq!(load.records, 1);
    assert_eq!(load.configured, 0, "no prefix, no restorable kind");
    assert!(rt.panel_restore_records().is_empty());
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Producer: order
// ---------------------------------------------------------------------------

/// A restore that finds no panel host defers its panel step; the install
/// commits it.
///
/// Revert the deferral (`Runtime::apply_panel_restore` committing regardless) and
/// the report is no longer empty before the install: every record comes back
/// `Dropped { reason: NoPanelHost }`, which is the failure this row pins.
#[test]
fn the_panel_step_waits_for_the_host_a_restore_finds_none() {
    let root = temp_root("order");
    let path = session_path(&root);
    let (rt, _host, backend) = writer("logs");
    rt.save_session_to_path(&path).expect("the session saves");
    let mut second = loader(backend, None);

    second
        .load_session_from_path(&path)
        .expect("the session loads");

    assert!(
        second.panel_restore_pending(),
        "no host yet: the panel step is deferred, not decided"
    );
    assert!(
        second.panel_restore_report().is_empty(),
        "nothing was dropped: {:#?}",
        second.panel_restore_report()
    );
    assert_eq!(
        second.panel_restore_records().len(),
        1,
        "the records are still the caller's until the step runs"
    );

    // The composition root installs the host once plugin activation built it.
    let host = host_with_provider(PROVIDER_MANIFEST_VERSION_V1);
    second.install_shared_scene_host(Rc::clone(&host));

    assert!(!second.panel_restore_pending(), "the deferred step ran");
    let report = second.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert!(
        second.panel_restore_records().is_empty(),
        "the records are consumed by the step that applied them"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// The whole chain
// ---------------------------------------------------------------------------

/// Save → disk → fresh load → restore, with a provider contract newer than this
/// runtime reads: the container is retained, presents `incompatible`, keeps its
/// allocation epoch, holds no controller generation, and is granted nothing.
///
/// Three producers feed this row, and reverting any one of them turns it red:
/// the save's encode (no document), the load's decode (no record), and the
/// deferral (the record is dropped `NoPanelHost` before the host installs). The
/// registration side is the control row below.
#[test]
fn a_save_load_restore_round_trip_presents_incompatible_for_a_newer_provider_contract() {
    let root = temp_root("chain-newer");
    let path = session_path(&root);
    let (rt, _host, backend) = writer("logs");
    rt.save_session_to_path(&path).expect("the session saves");

    let mut second = loader(backend, Some(drivers()));
    // The host exists with a provider contract newer than this runtime reads,
    // but it is not installed yet: the shipped order.
    let host = host_with_provider(PROVIDER_MANIFEST_VERSION_V1 + 1);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    assert!(
        second.panel_restore_pending(),
        "the step waits for the host"
    );

    second.install_shared_scene_host(Rc::clone(&host));

    let report = second.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(report.bound(), 0, "a newer contract is never bound");
    let PanelRestoreOutcome::Placeholder { panel, reason, .. } = report.panels()[0].outcome else {
        panic!("an unreadable provider contract presents a placeholder: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Incompatible,
        "the driver's provider outruns this runtime: `incompatible`, not `unloaded`"
    );
    assert_eq!(
        placeholder_of(&host, panel),
        Some(PlaceholderReason::Incompatible),
        "the retained container presents the placeholder"
    );

    let host = host.borrow();
    assert_eq!(host.panel_count(), 1, "the container is retained");
    let epoch = host
        .panel_allocation_generation(panel)
        .expect("the container keeps its allocation epoch");
    assert_eq!(
        host.registry().controller_generation(panel),
        None,
        "no controller generation was minted by the refused bind"
    );
    for capability in resolution() {
        assert!(
            !host
                .registry()
                .is_panel_capability_granted(panel, epoch, &capability),
            "{capability} must not be granted by a refused bind"
        );
    }
    drop(host);

    let _ = fs::remove_dir_all(&root);
}

/// Registration-side control for the row above: the same saved manifest, the
/// same driver, and a provider manifest at the schema this runtime reads bind
/// instead.
///
/// This is what makes the `incompatible` row a version comparison rather than a
/// blanket refusal of every registered provider: remove the registration and
/// the chain binds; declare a newer contract and it does not.
#[test]
fn the_same_chain_binds_when_the_declared_contract_is_the_one_this_runtime_reads() {
    let root = temp_root("chain-current");
    let path = session_path(&root);
    let (rt, _host, backend) = writer("logs");
    rt.save_session_to_path(&path).expect("the session saves");

    let mut second = loader(backend, Some(drivers()));
    let host = host_with_provider(PROVIDER_MANIFEST_VERSION_V1);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    second.install_shared_scene_host(Rc::clone(&host));

    let report = second.panel_restore_report();
    assert_eq!(report.bound(), 1, "the current schema binds: {report:?}");
    let PanelRestoreOutcome::Bound {
        panel, ref grants, ..
    } = report.panels()[0].outcome
    else {
        panic!("the current schema binds: {report:?}");
    };
    assert_eq!(
        grants,
        &resolution(),
        "a first bind commits the driver's whole resolution"
    );
    assert_eq!(
        placeholder_of(&host, panel),
        None,
        "a bound container presents no placeholder"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A record whose driver is absent restores as an `unloaded` container, not as
/// nothing at all — the shipped outcome while no driver source is installed.
#[test]
fn a_record_with_no_installed_driver_comes_back_unloaded() {
    let root = temp_root("chain-unloaded");
    let path = session_path(&root);
    let (rt, _host, backend) = writer("logs");
    rt.save_session_to_path(&path).expect("the session saves");

    let mut second = loader(backend, None);
    let host = host_with_provider(PROVIDER_MANIFEST_VERSION_V1);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    second.install_shared_scene_host(Rc::clone(&host));

    let report = second.panel_restore_report();
    let PanelRestoreOutcome::Placeholder { panel, reason, .. } = report.panels()[0].outcome else {
        panic!("a missing driver keeps the container: {report:?}");
    };
    assert_eq!(reason, PlaceholderReason::Unloaded);
    assert_eq!(
        placeholder_of(&host, panel),
        Some(PlaceholderReason::Unloaded)
    );

    let _ = fs::remove_dir_all(&root);
}
