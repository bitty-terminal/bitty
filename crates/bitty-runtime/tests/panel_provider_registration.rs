#![forbid(unsafe_code)]
//! The production provider-registration path of the panel restore chain
//! (bitty#3243).
//!
//! `tests/panel_restore_producers.rs` proves the producer/consumer pair for the
//! durable records. It leaves one half of the issue open by construction: every
//! provider descriptor in it is registered by the test on a host the test built,
//! so it proves the rule and not the wiring. Until this target, no shipped path
//! called `PanelRuntime::register_panel_provider` or
//! `Runtime::set_panel_restore_driver_source`, which meant a loaded manifest's
//! records could never find a provider and the restore chain was unimplemented
//! in a binary.
//!
//! Every row here drives the composition-root entry point —
//! `Runtime::install_shared_scene_host`, the same call `bitty-terminal` makes
//! when it installs the panel host `plugin_runtime::discover_and_activate`
//! returned — and nothing registers a descriptor by hand except the row that
//! proves a *newer* declaration is what the bind edge refuses:
//!
//! * `installing_the_host_registers_a_descriptor_for_a_live_bound_container` —
//!   the descriptors exist because the composition root observed them, so
//!   removing the registration call leaves `provider_manifest` at `None`;
//! * `a_container_whose_driver_lacks_the_provider_capability_registers_nothing` —
//!   the registration is gated on the committed `panel.provider` grant, not on
//!   "some plugin touched a panel";
//! * `a_loaded_record_binds_through_the_provider_the_host_install_registered` —
//!   the whole shipped order: save writes the manifest, a fresh runtime loads it
//!   (no host yet, so the step defers), the composition root installs the host,
//!   and the record is matched to the provider that drives its container kind
//!   and first-binds with the resolution this process already committed.
//!   Removing the registration, the driver-source install, or the plan-time
//!   re-resolution turns it into an `unloaded` placeholder;
//! * `a_declared_contract_newer_than_the_reader_presents_incompatible` — the
//!   same production entry, with the matched descriptor declaring a contract
//!   this runtime cannot read: the container is retained, presents
//!   `incompatible`, and is granted nothing (bitty#3239 row 13).
//!
//! The filesystem is real: the backend writes and reads the actual session and
//! manifest files under a per-test temporary directory. Every test removes it.

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
use bitty_runtime::runtime::panel_restore::PanelRestoreOutcome;
use bitty_runtime::{
    LayoutNode, PaneAttachment, PaneRoute, PaneSnapshot, PresentationMode, Runtime, SessionError,
    SessionFileBackend, SessionSnapshot, View, ViewId, WorkspaceSnapshot, panel_manifest_path,
};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const PROVIDER: &str = "panel.provider";

// ---------------------------------------------------------------------------
// A backend that really writes both documents
// ---------------------------------------------------------------------------

/// Writes and reads real files; replays one canned snapshot.
///
/// The session *body* is canned (`bitty-storage`'s codec is not this chain's
/// subject), but the bytes go through the same [`SessionFileBackend`] calls the
/// shipped backend implements, so the manifest a save commits is a real file a
/// fresh runtime reads back.
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
        "bitty-3243-providers-{label}-{}-{:?}",
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

/// The `panel.*` resolution a provider generation holds: it creates, focuses,
/// and declares itself a provider.
fn resolution() -> BTreeSet<String> {
    [CREATE, FOCUS, PROVIDER]
        .iter()
        .map(|item| (*item).to_string())
        .collect()
}

/// The resolution of a plugin that drives a panel without declaring itself a
/// provider.
fn non_provider_resolution() -> BTreeSet<String> {
    [CREATE, FOCUS]
        .iter()
        .map(|item| (*item).to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The two ends of the shipped order
// ---------------------------------------------------------------------------

/// A runtime with a session backend, an installed panel host, and one live
/// helper panel attributed to the presented workspace and driven by `owner()`.
///
/// This is the state a save captures: the container is what the manifest
/// records, and the binding is what the next process can observe.
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

/// A fresh runtime that has loaded the session: no panel host yet, which is the
/// shipped order (the restore runs before plugin activation builds the host).
fn loader(backend: Arc<DiskBackend>) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_session_backend(Some(backend));
    rt
}

/// A host with one live helper container driven by `owner()`, holding `grants`.
fn host_with_live_container(
    workspace: u64,
    grants: &BTreeSet<String>,
) -> Rc<RefCell<PanelRuntime>> {
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    let handle = host
        .borrow_mut()
        .create_panel(
            PanelType::Helper,
            Some(bitty_runtime::registry::WorkspaceId::new(workspace)),
        )
        .expect("the container allocates");
    let controller = handle.generation.next().expect("a controller generation");
    host.borrow_mut()
        .rebind_panel(handle.id, controller, &owner(), grants, &BTreeSet::new())
        .expect("the driver binds");
    host
}

/// The placeholder a container presents, read through `host`.
fn placeholder_of(
    host: &Rc<RefCell<PanelRuntime>>,
    panel: bitty_ui::PanelId,
) -> Option<PlaceholderReason> {
    host.borrow()
        .unbound_container_records(panel)
        .expect("container exists")
        .placeholder()
}

// ---------------------------------------------------------------------------
// Registration: the composition root's own descriptors
// ---------------------------------------------------------------------------

/// Installing the panel host registers a descriptor for the plugin driving a
/// live container, and the registry answers with it afterwards.
///
/// Revert the registration call in `Runtime::install_shared_scene_host` and both
/// assertions fail: no descriptor exists, so `provider_manifest` answers `None`
/// for every owner and no restored record can ever match a provider.
#[test]
fn installing_the_host_registers_a_descriptor_for_a_live_bound_container() {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let host = host_with_live_container(rt.active_workspace_id().get(), &resolution());

    // Before the install there is nothing to answer from: the host is not the
    // runtime's world yet.
    assert_eq!(
        host.borrow().provider_manifest(owner().as_str()),
        None,
        "no descriptor exists before the composition root installs the host"
    );

    rt.install_shared_scene_host(Rc::clone(&host));

    let manifest = host
        .borrow()
        .provider_manifest(owner().as_str())
        .cloned()
        .expect("the composition root registered the plugin that drives a container");
    assert_eq!(manifest.owner(), owner().as_str());
    assert_eq!(
        manifest.types(),
        &[PanelType::Helper],
        "the descriptor declares the container kinds the plugin was observed driving"
    );
    assert_eq!(
        manifest.version(),
        PROVIDER_MANIFEST_VERSION_V1,
        "the declared contract is the one this runtime read the container under"
    );
    assert_eq!(
        host.borrow().providers_for_type(PanelType::Helper),
        vec![owner().as_str().to_string()],
        "the kind lookup a restore matches on answers the registered provider"
    );
}

/// A plugin driving a container without the `panel.provider` grant is not a
/// provider: the install mints no descriptor for it.
#[test]
fn a_container_whose_driver_lacks_the_provider_capability_registers_nothing() {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let host = host_with_live_container(rt.active_workspace_id().get(), &non_provider_resolution());

    rt.install_shared_scene_host(Rc::clone(&host));

    assert_eq!(
        host.borrow().provider_manifest(owner().as_str()),
        None,
        "an observed container is not a declaration of `panel.provider`"
    );
    assert!(
        host.borrow()
            .providers_for_type(PanelType::Helper)
            .is_empty(),
        "nothing declares the kind"
    );
}

// ---------------------------------------------------------------------------
// The whole shipped order
// ---------------------------------------------------------------------------

/// Save → disk → fresh load (deferring) → composition-root install → matched
/// provider → first bind.
///
/// The record is not hand-built: the writer runtime saves it, the fresh runtime
/// decodes it, and the only registration in the row is the one the install
/// performs. Three wirings feed the outcome, and removing any one of them leaves
/// the report with an `unloaded` placeholder instead of a bind: the descriptor
/// registration, the host-backed driver source, and the plan-time re-resolution
/// that reads both.
#[test]
fn a_loaded_record_binds_through_the_provider_the_host_install_registered() {
    let root = temp_root("bind");
    let path = session_path(&root);
    let (writer_runtime, _writer_host, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let mut second = loader(backend);
    let summary = second
        .load_session_from_path(&path)
        .expect("the session loads");
    assert!(
        summary.panels.present,
        "the restore summary reports the manifest it decoded"
    );
    assert_eq!(summary.panels.records, 1, "one durable record");
    assert_eq!(
        summary.panels.configured, 1,
        "the record's key names a container kind this runtime mints"
    );
    assert!(
        second.panel_restore_pending(),
        "no host yet: the panel step is deferred, not decided"
    );

    // The composition root installs the host once plugin activation built it.
    // The live container is what the plugin does in this process; it is the
    // only fact that makes the record's kind match a provider.
    let host = host_with_live_container(1, &resolution());
    second.install_shared_scene_host(Rc::clone(&host));

    assert!(!second.panel_restore_pending(), "the deferred step ran");
    let report = second.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(
        report.bound(),
        1,
        "the record is matched to the provider that drives its kind: {report:?}"
    );
    let PanelRestoreOutcome::Bound {
        panel, ref grants, ..
    } = report.panels()[0].outcome
    else {
        panic!("an installed provider binds: {report:?}");
    };
    assert_eq!(
        grants,
        &resolution(),
        "the first bind commits the resolution this process already granted the owner"
    );
    assert_eq!(
        placeholder_of(&host, panel),
        None,
        "a bound container presents no placeholder"
    );
    assert_eq!(
        host.borrow()
            .provider_manifest(owner().as_str())
            .map(|m| m.version()),
        Some(PROVIDER_MANIFEST_VERSION_V1),
        "the descriptor the bind was matched through is the install's"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The same chain with the matched descriptor declaring a contract newer than
/// this runtime reads: the container is retained, presents `incompatible`, mints
/// no controller generation, and is granted nothing (bitty#3239 row 13).
///
/// The declaration is registered on the host before the install, which is what a
/// plugin-declared contract will look like once the package grammar can carry
/// one; the install observes the same owner and leaves the existing descriptor
/// alone, so the row proves the entry point's ordering rather than a hand-built
/// restore input.
#[test]
fn a_declared_contract_newer_than_the_reader_presents_incompatible() {
    let root = temp_root("newer");
    let path = session_path(&root);
    let (writer_runtime, _writer_host, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let mut second = loader(backend);
    second
        .load_session_from_path(&path)
        .expect("the session loads");

    let host = host_with_live_container(1, &resolution());
    let newer = PanelProviderManifest::parse(
        owner().as_str(),
        vec![PanelType::Helper],
        PROVIDER_MANIFEST_VERSION_V1 + 1,
    )
    .expect("the newer declaration parses");
    host.borrow_mut()
        .register_panel_provider(newer, true)
        .expect("the newer declaration registers");
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
        "the matched provider outruns this runtime: `incompatible`, not `unloaded`"
    );
    assert_eq!(
        placeholder_of(&host, panel),
        Some(PlaceholderReason::Incompatible),
        "the retained container presents the placeholder"
    );

    let host = host.borrow();
    assert_eq!(host.panel_count(), 2, "the restored container is retained");
    assert_eq!(
        host.registry().controller_generation(panel),
        None,
        "no controller generation was minted by the refused bind"
    );
    let epoch = host
        .panel_allocation_generation(panel)
        .expect("the container keeps its allocation epoch");
    for capability in resolution() {
        assert!(
            !host
                .registry()
                .is_panel_capability_granted(panel, epoch, &capability),
            "{capability} must not be granted by a refused bind"
        );
    }
    assert_eq!(
        host.provider_manifest(owner().as_str())
            .map(|m| m.version()),
        Some(PROVIDER_MANIFEST_VERSION_V1 + 1),
        "the install kept the declaration it observed instead of shadowing it"
    );
    drop(host);

    let _ = fs::remove_dir_all(&root);
}

/// A record whose kind no provider declares is matched to nothing and comes
/// back `unloaded`, not lost: the chain degrades to the placeholder rather than
/// dropping the container.
#[test]
fn a_record_with_no_matching_provider_comes_back_unloaded() {
    let root = temp_root("unmatched");
    let path = session_path(&root);
    let (writer_runtime, _writer_host, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let mut second = loader(backend);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    // A host with no live container and no descriptor: the install registers
    // nothing, so the record's kind declares no provider.
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    second.install_shared_scene_host(Rc::clone(&host));

    let report = second.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    let PanelRestoreOutcome::Placeholder { panel, reason, .. } = report.panels()[0].outcome else {
        panic!("a record with no provider keeps the container: {report:?}");
    };
    assert_eq!(reason, PlaceholderReason::Unloaded);
    assert_eq!(
        placeholder_of(&host, panel),
        Some(PlaceholderReason::Unloaded),
        "the retained container presents `unloaded`, not nothing"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A missing manifest is a clean start for panels, and the restore summary says
/// so instead of the caller having to guess.
#[test]
fn a_missing_manifest_is_reported_on_the_restore_summary() {
    let root = temp_root("missing");
    let path = session_path(&root);
    let (writer_runtime, _writer_host, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");
    fs::remove_file(panel_manifest_path(&path)).expect("the manifest removes");

    let mut second = loader(backend);
    let summary = second
        .load_session_from_path(&path)
        .expect("a missing manifest never costs the session");

    assert_eq!(summary.workspaces, 1, "the session restored");
    assert!(
        !summary.panels.present,
        "the summary reports that no manifest was read"
    );
    assert_eq!(summary.panels.records, 0);
    assert_eq!(summary.panels.configured, 0);

    let _ = fs::remove_dir_all(&root);
}
