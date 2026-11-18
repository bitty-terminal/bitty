#![forbid(unsafe_code)]
//! The shipped provider-contract declaration, end to end (bitty#3243 row 13).
//!
//! `tests/panel_restore_producers.rs` proves the durable-record chain and
//! `tests/panel_provider_registration.rs` proves the registration the host
//! install performs from *observation*. Both leave the issue's closing
//! condition open by construction: every descriptor declaring a contract above
//! `PROVIDER_MANIFEST_VERSION_V1` in them is built by the test, so
//! `PlaceholderReason::Incompatible` is reachable only in a world a test
//! invented. Nothing a shipped binary could parse could say "my provider
//! contract is newer than yours", which is exactly what issue #3243 asks for.
//!
//! This target closes that gap from the outside in:
//!
//! * **Grammar** — `a_manifest_declares_its_provider_contract_and_drives`
//!   parses a real `bitty-plugin.toml` body carrying `[panel.provider]` and
//!   asserts the declaration it yields. Remove the section from the reader and
//!   the declaration is `None`, so every assertion below fails.
//! * **Backwards compatibility** —
//!   `a_manifest_without_the_section_declares_nothing` parses the pre-existing
//!   body and asserts no declaration: the absent section keeps today's
//!   V1-by-observation behaviour byte for byte, and
//!   `an_absent_declaration_keeps_the_install_observation` drives that
//!   behaviour through the whole chain.
//! * **The walls the declaration cannot cross** — the `declaration_*` rows pin
//!   contract `0`, a missing key, an unknown kind, the `terminal` kind, a
//!   duplicate kind, and a declaration that never asked for `panel.provider`.
//! * **Registration** — `a_declared_contract_above_v1_registers_verbatim` calls
//!   the same entry the composition root's activation edge calls
//!   (`bitty_runtime::register_declared_panel_provider`) and reads the contract
//!   back from the registry.
//! * **The closing condition** —
//!   `a_saved_panel_whose_declared_contract_outruns_the_runtime_restores_incompatible`
//!   is the whole shipped chain: a live, bound, titled panel is saved by one
//!   runtime, the restart manifest lands on disk, a fresh runtime loads it (no
//!   panel host yet, so the step defers), the plugin's own declaration is
//!   registered at activation, the composition root installs the host, and the
//!   restored container presents `incompatible`, keeps its allocation epoch,
//!   holds no controller generation, is granted nothing, and is placed nowhere.
//!   Reverting the save-side encode, the manifest read, or the registration
//!   call turns it red.
//! * **Its controls** —
//!   `the_same_manifest_at_the_read_contract_binds` runs the identical chain
//!   with `contract = 1` and binds, so the outcome is a version comparison and
//!   not a blanket refusal of declared providers;
//!   `a_generation_without_the_provider_grant_registers_nothing` shows the
//!   capability gate still decides; and
//!   `a_redeclaration_swaps_the_registered_contract` covers the reload swap.
//!
//! The filesystem is real: the backend below writes and reads the actual
//! session and manifest files under a per-test directory, so "on disk" means on
//! disk. Every test removes its directory.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use bitty_plugin_host::PluginId;
use bitty_runtime::plugin_runtime::manifest_toml::parse_manifest;
use bitty_runtime::registry::{
    PROVIDER_MANIFEST_VERSION_V1, PanelProviderManifest, PanelRegistryConfig, PanelRuntime,
    PanelType, PlaceholderReason, ProviderContractError, WorkspaceId,
};
use bitty_runtime::runtime::panel_restore::PanelRestoreOutcome;
use bitty_runtime::{
    LayoutNode, PaneAttachment, PaneRoute, PaneSnapshot, PanelDeclarationError, PresentationMode,
    Runtime, SessionError, SessionFileBackend, SessionSnapshot, View, ViewId, WorkspaceSnapshot,
    panel_manifest_path, register_declared_panel_provider,
};
use bitty_ui::{PanelId, PersistedPanel};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";
const PROVIDER: &str = "panel.provider";

// ---------------------------------------------------------------------------
// The manifest body a plugin ships
// ---------------------------------------------------------------------------

/// A real `bitty-plugin.toml` body, with the provider section appended when
/// `contract` names one.
///
/// The declaration is what a plugin author writes: the contract version it
/// speaks and the kinds it drives. `None` yields the pre-existing body, which
/// must keep parsing exactly as before.
fn manifest_body(id: &str, contract: Option<u32>) -> String {
    let tail = match contract {
        Some(version) => {
            format!("\n[panel.provider]\ncontract = {version}\ntypes = [\"helper\"]\n")
        }
        None => String::new(),
    };
    format!(
        "[plugin]\nid = \"{id}\"\nname = \"Notes\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\npanel.create = true\npanel.focus = true\npanel.provider = true\n{tail}"
    )
}

/// The declaration `manifest_body(id, Some(contract))` parses to.
fn declaration(id: &str, contract: u32) -> bitty_plugin_host::manifest::PanelProviderDeclaration {
    parse_manifest(manifest_body(id, Some(contract)).as_bytes())
        .expect("the manifest parses")
        .panel_provider
        .expect("the manifest declares a provider contract")
}

fn owner() -> PluginId {
    PluginId::new("acme.notes").expect("valid plugin id")
}

/// The `panel.*` resolution a provider generation holds.
fn resolution() -> BTreeSet<String> {
    [CREATE, FOCUS, PROVIDER]
        .iter()
        .map(|item| (*item).to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

/// The manifest grammar carries the declaration, and the parser yields it.
///
/// Revert the `[panel.provider]` arm of the reader and this row fails on its
/// first assertion (`None`): no shipped declaration could ever exceed
/// `PROVIDER_MANIFEST_VERSION_V1`, and the closing condition below would be
/// unreachable again.
#[test]
fn a_manifest_declares_its_provider_contract_and_drives() {
    let manifest = parse_manifest(manifest_body("acme.notes", Some(2)).as_bytes())
        .expect("a manifest with a provider declaration parses");
    let declared = manifest
        .panel_provider
        .as_ref()
        .expect("the declaration survives the parse");
    assert_eq!(
        declared.contract, 2,
        "the declared provider contract is carried verbatim"
    );
    assert_eq!(
        declared.types,
        vec![String::from("helper")],
        "the declared kinds are the canonical panel types the plugin drives"
    );
}

/// An absent section declares nothing: today's behaviour, unchanged.
///
/// This is the backwards-compatibility contract of the slice. A body that never
/// mentions `[panel.provider]` must still parse, and must still leave the
/// provider answers to the observation the host install performs.
#[test]
fn a_manifest_without_the_section_declares_nothing() {
    let manifest = parse_manifest(manifest_body("acme.notes", None).as_bytes())
        .expect("the pre-existing body still parses");
    assert!(
        manifest.panel_provider.is_none(),
        "no section, no declaration, no change to how a provider is registered"
    );
}

/// Every malformed declaration fails closed, with the section named.
#[test]
fn a_malformed_declaration_does_not_parse() {
    let cases: [(&str, &str); 7] = [
        (
            "[panel.provider]\ncontract = 0\ntypes = [\"helper\"]\n",
            "contract 0 is not a schema version",
        ),
        (
            "[panel.provider]\ncontract = 2\n",
            "a contract without the kinds it drives",
        ),
        (
            "[panel.provider]\ntypes = [\"helper\"]\n",
            "kinds without the contract they are served under",
        ),
        (
            "[panel.provider]\ncontract = 2\ntypes = [\"logs\"]\n",
            "a kind outside the closed panel vocabulary",
        ),
        (
            "[panel.provider]\ncontract = 2\ntypes = [\"terminal\"]\n",
            "the terminal kind belongs to the session",
        ),
        (
            "[panel.provider]\ncontract = 2\ntypes = [\"helper\", \"helper\"]\n",
            "one kind declared twice",
        ),
        (
            "[panel.provider]\ncontract = 2\ntypes = [\"helper\"]\nwat = true\n",
            "an unknown key in the section",
        ),
    ];
    for (section, why) in cases {
        let body = format!(
            "[plugin]\nid = \"acme.notes\"\nname = \"Notes\"\nversion = \"0.1.0\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\npanel.provider = true\n\n{section}"
        );
        assert!(
            parse_manifest(body.as_bytes()).is_err(),
            "must refuse {why}: {section}"
        );
    }
}

/// A declaration in a manifest that never asked for `panel.provider` is
/// refused: a plugin cannot describe provider behaviour it never requested.
#[test]
fn a_declaration_without_the_provider_capability_does_not_parse() {
    let body = "[plugin]\nid = \"acme.notes\"\nname = \"Notes\"\nversion = \"0.1.0\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\npanel.create = true\n\n[panel.provider]\ncontract = 2\ntypes = [\"helper\"]\n";
    let error = parse_manifest(body.as_bytes()).expect_err("the declaration is refused");
    assert!(
        error.contains("panel.provider"),
        "the refusal names the section: {error}"
    );
}

// ---------------------------------------------------------------------------
// Registration at the activation edge
// ---------------------------------------------------------------------------

/// The declaration becomes a registry descriptor, contract version and all.
///
/// This is the entry the composition root's activation edge calls
/// (`bitty-terminal`'s `plugin_runtime::discover_with_roots`), so the row fails
/// if that call is reverted or if the seam stops forwarding the declared
/// version.
#[test]
fn a_declared_contract_above_v1_registers_verbatim() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    let declared = declaration("acme.notes", PROVIDER_MANIFEST_VERSION_V1 + 1);

    let generation = register_declared_panel_provider(&mut host, "acme.notes", &declared, true)
        .expect("a granted generation's declaration registers");

    let manifest = host
        .provider_manifest("acme.notes")
        .expect("the declaration is registered");
    assert_eq!(
        manifest.version(),
        PROVIDER_MANIFEST_VERSION_V1 + 1,
        "the registered contract is the declared one, not the runtime's own ABI"
    );
    assert!(
        manifest.requires_newer_runtime(),
        "the registered descriptor is the skew the bind edge refuses"
    );
    assert_eq!(manifest.types(), &[PanelType::Helper]);
    assert_eq!(
        host.provider_generation_of("acme.notes"),
        Some(generation),
        "registration mints a generation like any other provider"
    );
}

/// The capability gate still decides: a generation without `panel.provider`
/// registers nothing, exactly as the observation path behaves.
#[test]
fn a_generation_without_the_provider_grant_registers_nothing() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    let declared = declaration("acme.notes", 2);

    let error = register_declared_panel_provider(&mut host, "acme.notes", &declared, false)
        .expect_err("a denied generation declares nothing");

    assert_eq!(
        error,
        PanelDeclarationError::Registry(ProviderContractError::CapabilityDenied {
            owner: String::from("acme.notes")
        }),
        "the registry's own capability gate answers"
    );
    assert!(
        host.provider_manifest("acme.notes").is_none(),
        "nothing is registered for a generation that holds no provider capability"
    );
}

/// A kind the runtime cannot map refuses the whole declaration.
///
/// The manifest reader already refuses such a name; this is the in-process
/// shape (a bundled catalog builds a `PluginManifest` directly) failing closed
/// the same way instead of registering a descriptor that declares less than the
/// plugin said it drives.
#[test]
fn a_declaration_naming_an_unknown_kind_registers_nothing() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    let declared = bitty_plugin_host::manifest::PanelProviderDeclaration {
        contract: 2,
        types: vec![String::from("logs")],
    };

    let error = register_declared_panel_provider(&mut host, "acme.notes", &declared, true)
        .expect_err("an unmappable kind is refused");
    assert_eq!(
        error,
        PanelDeclarationError::UnknownPanelType {
            panel_type: String::from("logs")
        }
    );
    assert!(host.provider_manifest("acme.notes").is_none());
}

/// A reload that re-declares a different contract swaps the descriptor instead
/// of being refused as a duplicate owner.
#[test]
fn a_redeclaration_swaps_the_registered_contract() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    register_declared_panel_provider(&mut host, "acme.notes", &declaration("acme.notes", 1), true)
        .expect("the first declaration registers");

    let swapped = register_declared_panel_provider(
        &mut host,
        "acme.notes",
        &declaration("acme.notes", 3),
        true,
    )
    .expect("a re-declaration swaps the contract");

    assert_eq!(
        host.provider_manifest("acme.notes").map(|m| m.version()),
        Some(3),
        "the generation's newest declaration is the registered one"
    );
    assert_eq!(
        host.provider_generation_of("acme.notes"),
        Some(swapped),
        "the swap mints a fresh registration generation"
    );
    assert_eq!(
        host.providers_for_type(PanelType::Helper),
        vec![String::from("acme.notes")],
        "the kind lookup still answers exactly one provider"
    );
}

/// A refused replacement keeps the descriptor the owner already holds
/// (bitty#3243 review F2).
///
/// The swap is unregister + register, so an ungated replacement would drop the
/// old descriptor before the registry refuses the new one. Revert the
/// capability pre-check in `register_declared_panel_provider` and this row
/// fails on the last assertion (`None`): the owner ends up with no provider at
/// all, which would silently downgrade a live generation to the placeholder.
#[test]
fn a_refused_replacement_keeps_the_registered_descriptor() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    register_declared_panel_provider(&mut host, "acme.notes", &declaration("acme.notes", 2), true)
        .expect("the granted declaration registers");

    let refused = register_declared_panel_provider(
        &mut host,
        "acme.notes",
        &declaration("acme.notes", 3),
        false,
    );
    assert!(
        refused.is_err(),
        "a generation without the grant must not replace a descriptor"
    );
    assert_eq!(
        host.provider_manifest("acme.notes").map(|m| m.version()),
        Some(2),
        "the refused replacement must leave the registered contract in place"
    );
}

// ---------------------------------------------------------------------------
// A backend that really writes both documents
// ---------------------------------------------------------------------------

/// Writes and reads real files; replays one canned snapshot.
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

/// One workspace, one leaf, one attached pane.
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

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "bitty-3243-declaration-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

fn session_path(root: &Path) -> PathBuf {
    root.join("session.v3")
}

// ---------------------------------------------------------------------------
// The writer world and the restarting world
// ---------------------------------------------------------------------------

/// A runtime with a session backend, an installed panel host, and one live
/// helper panel attributed to the presented workspace and driven by `owner()`.
///
/// This is the state the save captures: the container is what the manifest
/// records, and the binding is what makes the record restorable.
fn writer(title: &str) -> (Runtime, Arc<DiskBackend>) {
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
    bind_live(&host, handle.id, handle.generation);
    (rt, backend)
}

/// Binds `panel` to `owner()` at its allocation epoch's successor.
fn bind_live(
    host: &Rc<RefCell<PanelRuntime>>,
    panel: PanelId,
    epoch: bitty_runtime::registry::Generation,
) {
    let controller = epoch.next().expect("a controller generation");
    host.borrow_mut()
        .rebind_panel(panel, controller, &owner(), &resolution(), &BTreeSet::new())
        .expect("the driver binds");
}

/// The panel host the restarting process builds at plugin activation, with one
/// live helper container of its own — what a plugin that creates a panel in
/// `init.lua` does before the composition root installs the host.
fn activation_host() -> (Rc<RefCell<PanelRuntime>>, PanelId) {
    let host = Rc::new(RefCell::new(
        PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds"),
    ));
    let handle = host
        .borrow_mut()
        .create_panel(PanelType::Helper, Some(WorkspaceId::new(1)))
        .expect("the activation container allocates");
    bind_live(&host, handle.id, handle.generation);
    (host, handle.id)
}

/// A fresh runtime that has loaded the session: no panel host yet, which is the
/// shipped order (the restore runs before activation builds the host).
fn loader(backend: Arc<DiskBackend>) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_session_backend(Some(backend));
    rt
}

fn placeholder_of(host: &Rc<RefCell<PanelRuntime>>, panel: PanelId) -> Option<PlaceholderReason> {
    host.borrow()
        .unbound_container_records(panel)
        .expect("container exists")
        .placeholder()
}

// ---------------------------------------------------------------------------
// The closing condition
// ---------------------------------------------------------------------------

/// Save → disk → fresh load → the plugin's own declaration → install → the
/// restored container presents `incompatible`.
///
/// This is issue #3243's closing condition, and every producer it names is
/// load-bearing:
///
/// * the **save** writes the restart manifest (revert
///   `Runtime::save_session_to_path`'s encode and the load has no record);
/// * the **load** decodes it before the snapshot applies (revert it and the
///   panel step has nothing to plan);
/// * the **deferral** waits for the host (revert it and the record is dropped
///   `NoPanelHost` before the install);
/// * the **grammar** carries the declaration (revert the section and it is
///   `None` — the row cannot even build its input);
/// * the **registration** at the activation edge puts it on the host (revert
///   `register_declared_panel_provider` and the descriptor is absent, so the
///   restore answers `unloaded` instead of `incompatible`).
#[test]
fn a_saved_panel_whose_declared_contract_outruns_the_runtime_restores_incompatible() {
    let root = temp_root("closing");
    let path = session_path(&root);
    let (writer_runtime, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    // The restarting process: the plugin's shipped manifest, parsed by the same
    // reader a binary uses, then handed to the runtime's activation edge.
    let declared = declaration("acme.notes", PROVIDER_MANIFEST_VERSION_V1 + 1);

    let mut second = loader(backend);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    assert!(
        second.panel_restore_pending(),
        "no host yet: the panel step waits for the composition root"
    );

    let (host, activation_panel) = activation_host();
    {
        let mut host_borrow = host.borrow_mut();
        register_declared_panel_provider(&mut host_borrow, "acme.notes", &declared, true)
            .expect("the plugin's own declaration registers at activation");
    }
    assert_eq!(
        host.borrow()
            .provider_manifest("acme.notes")
            .map(PanelProviderManifest::version),
        Some(PROVIDER_MANIFEST_VERSION_V1 + 1),
        "the declared contract is registered before the install observes anything"
    );

    // The composition root installs the host the activation built. Its
    // observation finds the owner already registered and leaves the declared
    // contract alone, which is what keeps the skew visible to the restore.
    second.install_shared_scene_host(Rc::clone(&host));

    assert!(
        !second.panel_restore_pending(),
        "the deferred panel step commits at the install"
    );
    let report = second.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(
        report.bound(),
        0,
        "a provider contract this runtime cannot read is never bound: {report:?}"
    );
    let PanelRestoreOutcome::Placeholder { panel, reason, .. } = report.panels()[0].outcome else {
        panic!("an unreadable provider contract presents a placeholder: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Incompatible,
        "the declared contract outruns this runtime: `incompatible`, not `unloaded`"
    );
    assert_eq!(
        placeholder_of(&host, panel),
        Some(PlaceholderReason::Incompatible),
        "the retained container presents it"
    );
    assert_eq!(
        host.borrow()
            .provider_manifest("acme.notes")
            .map(PanelProviderManifest::version),
        Some(PROVIDER_MANIFEST_VERSION_V1 + 1),
        "the install's observation did not shadow the plugin's declaration with V1"
    );

    {
        let host = host.borrow();
        assert_eq!(
            host.panel_count(),
            2,
            "the restored container is retained beside the activation one"
        );
        let epoch = host
            .panel_allocation_generation(panel)
            .expect("the restored container keeps its allocation epoch");
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
                "{capability} must not be granted to a container nothing bound"
            );
        }
        assert_eq!(
            host.placement_view_of(panel),
            None,
            "nothing was applied: the placeholder container is placed nowhere"
        );
        assert_eq!(
            host.registry().controller_generation(activation_panel),
            Some(
                host.panel_allocation_generation(activation_panel)
                    .expect("the activation container keeps its epoch")
                    .next()
                    .expect("a controller generation")
            ),
            "the activation container this process already bound is untouched"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// The control for the row above: the identical chain, the identical manifest,
/// and the identical registration, with `contract = 1`.
///
/// The record binds, so the `incompatible` outcome is a version comparison and
/// not a blanket refusal of every declared provider.
#[test]
fn the_same_manifest_at_the_read_contract_binds() {
    let root = temp_root("control");
    let path = session_path(&root);
    let (writer_runtime, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let declared = declaration("acme.notes", PROVIDER_MANIFEST_VERSION_V1);

    let mut second = loader(backend);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    let (host, _activation_panel) = activation_host();
    {
        let mut host_borrow = host.borrow_mut();
        register_declared_panel_provider(&mut host_borrow, "acme.notes", &declared, true)
            .expect("the declaration registers");
    }
    second.install_shared_scene_host(Rc::clone(&host));

    let report = second.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome: {report:?}");
    assert_eq!(
        report.bound(),
        1,
        "the contract this runtime reads binds: {report:?}"
    );
    let PanelRestoreOutcome::Bound {
        panel, ref grants, ..
    } = report.panels()[0].outcome
    else {
        panic!("the read contract binds: {report:?}");
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

    let _ = fs::remove_dir_all(&root);
}

/// Backwards compatibility end to end: a plugin that declares nothing keeps
/// today's behaviour, where the install's observation registers the V1
/// descriptor and the restored record binds through it.
#[test]
fn an_absent_declaration_keeps_the_install_observation() {
    let root = temp_root("absent");
    let path = session_path(&root);
    let (writer_runtime, backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let manifest = parse_manifest(manifest_body("acme.notes", None).as_bytes())
        .expect("the pre-existing body parses");
    assert!(
        manifest.panel_provider.is_none(),
        "no declaration to register"
    );

    let mut second = loader(backend);
    second
        .load_session_from_path(&path)
        .expect("the session loads");
    let (host, _activation_panel) = activation_host();
    assert_eq!(
        host.borrow().provider_manifest("acme.notes"),
        None,
        "nothing is registered before the install"
    );

    second.install_shared_scene_host(Rc::clone(&host));

    assert_eq!(
        host.borrow()
            .provider_manifest("acme.notes")
            .map(PanelProviderManifest::version),
        Some(PROVIDER_MANIFEST_VERSION_V1),
        "with no declaration the install's observation still mints V1"
    );
    let report = second.panel_restore_report();
    assert_eq!(
        report.bound(),
        1,
        "the observation path is unchanged: {report:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The saved record still carries only identity: the declaration is not
/// smuggled into the durable document.
///
/// The manifest on disk is decoded here directly, so a producer that started
/// writing the contract (or the kinds) into the record would fail this row —
/// that field set is `RFC-OQ-9`'s pending persistence contract, not this
/// slice's to widen.
#[test]
fn the_durable_record_does_not_carry_the_declaration() {
    let root = temp_root("durable");
    let path = session_path(&root);
    let (writer_runtime, _backend) = writer("logs");
    writer_runtime
        .save_session_to_path(&path)
        .expect("the session saves");

    let document = fs::read_to_string(panel_manifest_path(&path)).expect("the manifest is written");
    let decoded = bitty_ui::decode_manifest(&document).expect("the document decodes");
    assert_eq!(decoded.panels().len(), 1);
    let record: &PersistedPanel = &decoded.panels()[0];
    assert_eq!(
        record
            .key
            .persistent
            .as_str()
            .split_once('-')
            .map(|(p, _)| p),
        Some("helper"),
        "the record's key still names only the container kind"
    );
    assert!(
        !document.contains("contract"),
        "the durable record grammar carries no provider-contract field: {document}"
    );

    let _ = fs::remove_dir_all(&root);
}
