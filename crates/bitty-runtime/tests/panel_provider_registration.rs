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
//! * `a_kind_driven_by_two_owners_is_not_bound_to_either` and
//!   `the_same_world_resolves_the_same_in_either_allocation_order` — the
//!   resolution is owner-qualified and order-independent: two plugins driving
//!   one container kind make the record unattributable (`unloaded`), in either
//!   allocation order, instead of binding it to whichever provider the registry
//!   happened to enumerate first;
//! * `two_providers_declaring_the_same_kind_resolve_to_no_driver` and
//!   `a_descriptor_without_a_live_container_does_not_bind` — the two branches
//!   where no single answer exists: ambiguous descriptors, and a descriptor with
//!   no committed resolution behind it;
//! * `a_missing_manifest_and_an_empty_one_do_not_read_the_same` — the restore
//!   summary's panel field is an observable that distinguishes "no manifest"
//!   from "a manifest with no records".
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
use bitty_runtime::runtime::panel_restore::{PanelRestoreOutcome, PanelRestoreReport};
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

// ---------------------------------------------------------------------------
// Identity: owner and kind, never allocation order
// ---------------------------------------------------------------------------

/// A second plugin in the same process: it drives a container of the same kind
/// but does not declare itself a provider.
fn other_owner() -> PluginId {
    PluginId::new("beau.notes").expect("valid plugin id")
}

/// The world the allocation-order defect was about: two plugins driving one
/// `PanelType`, only one of them a provider.
///
/// `provider_first` chooses which container the registry is handed first and
/// changes nothing else, so the two hosts describe the same world in two
/// insertion orders.
fn host_with_two_drivers(provider_first: bool) -> Rc<RefCell<PanelRuntime>> {
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    let drivers: [(PluginId, BTreeSet<String>); 2] = if provider_first {
        [
            (owner(), resolution()),
            (other_owner(), non_provider_resolution()),
        ]
    } else {
        [
            (other_owner(), non_provider_resolution()),
            (owner(), resolution()),
        ]
    };
    for (plugin, grants) in drivers {
        let handle = host
            .borrow_mut()
            .create_panel(
                PanelType::Helper,
                Some(bitty_runtime::registry::WorkspaceId::new(1)),
            )
            .expect("the container allocates");
        let controller = handle.generation.next().expect("a controller generation");
        host.borrow_mut()
            .rebind_panel(handle.id, controller, &plugin, &grants, &BTreeSet::new())
            .expect("the driver binds");
    }
    host
}

/// Save one record from `owner()`'s helper container, load it in a fresh
/// runtime, install `host` through the composition-root entry point, and answer
/// what the deferred commit did.
fn restore_into(host: Rc<RefCell<PanelRuntime>>, label: &str) -> PanelRestoreReport {
    let root = temp_root(label);
    let path = session_path(&root);
    let (writer_runtime, _writer_host, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");
    let mut second = loader(backend);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    second.install_shared_scene_host(host);
    let report = second.panel_restore_report().clone();
    let _ = fs::remove_dir_all(&root);
    report
}

/// One restore's outcome as a comparable, content-free shape: the variant each
/// record reached and, for a placeholder, why.
fn outcome_shape(report: &PanelRestoreReport) -> Vec<String> {
    report
        .panels()
        .iter()
        .map(|panel| match &panel.outcome {
            PanelRestoreOutcome::Bound { .. } => String::from("bound"),
            PanelRestoreOutcome::Placeholder { reason, .. } => format!("placeholder:{reason:?}"),
            PanelRestoreOutcome::Dropped { reason, .. } => format!("dropped:{reason:?}"),
        })
        .collect()
}

/// A record whose kind two plugins drive is bound to neither of them.
///
/// The durable record carries no owner (`RFC-OQ-9`, bitty#3239 row 16), so when
/// more than one plugin drives its container kind there is no single creator to
/// restore it to. The provider here allocated its container **first**, which is
/// exactly the ordering the round-2 review found binding the record: before the
/// fix this row reported `bound == 1`, attributing the other plugin's panel to
/// `acme.notes` and committing `acme.notes`' resolution on it.
#[test]
fn a_kind_driven_by_two_owners_is_not_bound_to_either() {
    let host = host_with_two_drivers(true);

    let report = restore_into(Rc::clone(&host), "two-owners-provider-first");

    assert_eq!(
        host.borrow().providers_for_type(PanelType::Helper),
        vec![owner().as_str().to_string()],
        "the ambiguity is in the live containers, not in the descriptors: exactly one \
         provider declares the kind, so the answer cannot come from the descriptor set"
    );
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(
        report.bound(),
        0,
        "a record with no owner of its own may not be handed to one plugin's provider: {report:?}"
    );
    let PanelRestoreOutcome::Placeholder { panel, reason, .. } = report.panels()[0].outcome else {
        panic!("an unattributable record keeps its container: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Unloaded,
        "the honest answer is `unloaded`, not another plugin's resolution"
    );
    assert_eq!(
        placeholder_of(&host, panel),
        Some(PlaceholderReason::Unloaded),
        "the retained container presents the placeholder"
    );
}

/// The same world in both allocation orders resolves the same way.
///
/// This is the invariant the round-2 defect broke: resolution used to take the
/// first live container of the kind and compare owners afterwards, so
/// provider-first answered `bound` and bystander-first answered `unloaded` for
/// one and the same world. Both orders are constructed here and their outcomes
/// compared, so an implementation that reads containers in enumeration order
/// fails one of the two.
#[test]
fn the_same_world_resolves_the_same_in_either_allocation_order() {
    let provider_first = restore_into(host_with_two_drivers(true), "order-provider-first");
    let bystander_first = restore_into(host_with_two_drivers(false), "order-bystander-first");

    assert_eq!(
        outcome_shape(&provider_first),
        outcome_shape(&bystander_first),
        "the same world with the allocation order swapped must resolve the same: \
         provider-first={provider_first:?} bystander-first={bystander_first:?}"
    );
    assert_eq!(
        provider_first.bound(),
        bystander_first.bound(),
        "the bind count may not depend on which container was allocated first"
    );
    // Spelled out so the row cannot pass by both orders being wrong in the same
    // way: an unattributable record is bound to nobody in either order.
    assert_eq!(provider_first.bound(), 0, "{provider_first:?}");
    assert_eq!(bystander_first.bound(), 0, "{bystander_first:?}");
}

/// Two providers declaring one kind are an ambiguity the restore does not
/// resolve by picking one: the record keeps its container, `unloaded`.
#[test]
fn two_providers_declaring_the_same_kind_resolve_to_no_driver() {
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    for plugin in [owner(), other_owner()] {
        let handle = host
            .borrow_mut()
            .create_panel(
                PanelType::Helper,
                Some(bitty_runtime::registry::WorkspaceId::new(1)),
            )
            .expect("the container allocates");
        let controller = handle.generation.next().expect("a controller generation");
        host.borrow_mut()
            .rebind_panel(
                handle.id,
                controller,
                &plugin,
                &resolution(),
                &BTreeSet::new(),
            )
            .expect("the driver binds");
    }

    let report = restore_into(Rc::clone(&host), "two-providers");

    assert_eq!(
        host.borrow().providers_for_type(PanelType::Helper).len(),
        2,
        "both plugins declared the kind"
    );
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(report.bound(), 0, "an ambiguous kind binds to no driver");
    let PanelRestoreOutcome::Placeholder { reason, .. } = report.panels()[0].outcome else {
        panic!("an ambiguous record keeps its container: {report:?}");
    };
    assert_eq!(reason, PlaceholderReason::Unloaded);
}

/// A readable descriptor with no live container behind it does not bind: there
/// is no committed resolution to read, and a first bind may not invent one.
#[test]
fn a_descriptor_without_a_live_container_does_not_bind() {
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    let declared = PanelProviderManifest::parse(
        owner().as_str(),
        vec![PanelType::Helper],
        PROVIDER_MANIFEST_VERSION_V1,
    )
    .expect("the declaration parses");
    host.borrow_mut()
        .register_panel_provider(declared, true)
        .expect("the declaration registers");

    let report = restore_into(Rc::clone(&host), "descriptor-only");

    assert!(
        host.borrow().provider_manifest(owner().as_str()).is_some(),
        "the row exercises the descriptor-without-container branch, not the no-descriptor one"
    );
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(
        report.bound(),
        0,
        "no live container, no resolution to bind"
    );
    let PanelRestoreOutcome::Placeholder { reason, .. } = report.panels()[0].outcome else {
        panic!("a record with no readable resolution keeps its container: {report:?}");
    };
    assert_eq!(reason, PlaceholderReason::Unloaded);
}

/// A missing manifest and a manifest with zero records are different facts and
/// must not read the same.
///
/// The counts alone cannot tell them apart — both are `0` records over `0`
/// configured — so the shipped startup line prints the presence flag as well.
/// This is the consumer of [`PanelManifestLoad::present`] that check 3 of the
/// round-2 review found missing.
#[test]
fn a_missing_manifest_and_an_empty_one_do_not_read_the_same() {
    let root = temp_root("presence");
    let path = session_path(&root);
    let mut writer_runtime = Runtime::with_defaults().expect("defaults build");
    let backend = Arc::new(DiskBackend {
        snapshot: canned_snapshot(),
    });
    writer_runtime.set_session_backend(Some(backend.clone()));
    // An installed host that holds no container: a complete observation of an
    // empty world, which is the one case that legitimately writes an empty
    // document.
    writer_runtime.install_shared_scene_host(Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    )));
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let mut second = loader(backend);
    let empty = second
        .load_session_from_path(&path)
        .expect("the session loads");
    assert!(
        empty.panels.present,
        "a zero-record sidecar is still present"
    );
    assert_eq!(empty.panels.records, 0);

    fs::remove_file(panel_manifest_path(&path)).expect("the manifest removes");
    let mut third = loader(Arc::new(DiskBackend {
        snapshot: canned_snapshot(),
    }));
    let missing = third
        .load_session_from_path(&path)
        .expect("a missing manifest never costs the session");
    assert!(!missing.panels.present, "no sidecar was read");

    assert_eq!(
        (empty.panels.records, empty.panels.configured),
        (missing.panels.records, missing.panels.configured),
        "the counts are identical, so they cannot be the whole observable"
    );
    assert_ne!(
        empty.panels.to_string(),
        missing.panels.to_string(),
        "the shipped line must distinguish an absent manifest from an empty one"
    );
    assert_eq!(
        missing.panels.to_string(),
        "present=false records=0 configured=0"
    );
    assert_eq!(
        empty.panels.to_string(),
        "present=true records=0 configured=0"
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
