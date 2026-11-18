#![forbid(unsafe_code)]
//! Panel restore on the session-restore path (bitty#3239 row 14).
//!
//! Row 14's closing condition: `plan_restore` invoked from the session-restore
//! path, minting a fresh `PanelId` per record, refusing `StableIdentity`, and
//! presenting the appropriate placeholder for a missing/denied/older driver.
//! The rows below drive the shipped path — `Runtime::load_session_from_path`,
//! the same step `restore_session_on_startup` runs — with a stub backend that
//! only replays a canned snapshot; nothing here reaches the filesystem.
//!
//! What each row pins:
//!
//! * the production chain: one record becomes a container with a **fresh**
//!   `PanelId` (the persisted id is advisory) and a first bind that grants the
//!   driver's whole resolution, on the controller generation the first bind
//!   derives from the allocation epoch (never the binding compared with itself);
//! * `StableIdentity` is refused for the whole restore before any mutation — the
//!   presented workspace id, the slot table, the layout, and the focus all stay
//!   as they were, and the row replays a canned world that is distinguishable
//!   from the live one so those assertions can fail;
//! * one row per driver refusal: missing → `unloaded`, denied → `denied`
//!   (`Unavailable`), older → `incompatible`. Each keeps the container, presents
//!   the Core-owned placeholder scene, and binds nothing;
//! * the persisted workspace ordinal: a two-workspace session restores each
//!   record on the restored slot at the ordinal the record names, not only the
//!   records of the one workspace the starting window happens to present;
//! * a second restore replaces the slot table, so the containers attributed to
//!   the replaced world's ids are released instead of surviving on a workspace
//!   id no edge can reach;
//! * the bounded remainder: a record whose ordinal names no restored slot is
//!   dropped rather than re-homed, and a runtime with no panel host defers the
//!   step instead of dropping it (the deferred commit is read in
//!   `tests/panel_restore_producers.rs`, and here from the restore that finds
//!   no host).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bitty_plugin_host::PluginId;
use bitty_runtime::registry::{
    PanelRegistryConfig, PanelRuntime, PanelState, PanelType, PlaceholderReason,
};
use bitty_runtime::runtime::panel_restore::{
    PanelDriverAnswer, PanelRestoreDrop, PanelRestoreInput, PanelRestoreOutcome,
};
use bitty_runtime::{
    LayoutNode, PaneAttachment, PaneRoute, PaneSnapshot, PresentationMode, Runtime, SessionError,
    SessionFileBackend, SessionSnapshot, View, ViewId, WorkspaceSnapshot,
};
use bitty_ui::{PanelId, PanelRestoreKey, PersistedPanel, PersistencePolicy, PersistentId};

const CREATE: &str = "panel.create";
const FOCUS: &str = "panel.focus";

/// Replays one canned snapshot; never touches the filesystem.
#[derive(Debug)]
struct CannedBackend {
    snapshot: SessionSnapshot,
}

impl SessionFileBackend for CannedBackend {
    fn encode_snapshot(&self, _snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
        Ok(Vec::new())
    }

    fn decode_snapshot(&self, _bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
        Ok(self.snapshot.clone())
    }

    fn commit_session_bytes(&self, _path: &Path, _bytes: &[u8]) -> Result<(), SessionError> {
        Ok(())
    }

    fn load_session_bytes(&self, path: &Path) -> Result<Vec<u8>, SessionError> {
        // The trait's contract is `NotFound` when no file exists. A path under
        // the panel manifest suffix is a file this canned backend never wrote,
        // so it answers the same way a real backend does for a missing sidecar
        // (bitty#3243); answering `Ok(empty)` there would claim an empty
        // document and clear the records a row configured.
        if path.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .ends_with(bitty_runtime::PANEL_MANIFEST_FILE_SUFFIX)
        }) {
            return Err(SessionError::NotFound);
        }
        Ok(Vec::new())
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

/// One workspace of a canned session: one leaf, one attached pane.
fn single_leaf_workspace(seq: u64, name: &str, view: ViewId) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        seq,
        name: String::from(name),
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
    }
}

/// One workspace, one leaf, one attached pane: the smallest snapshot the
/// restore accepts.
fn canned_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        version: bitty_runtime::SESSION_FORMAT_VERSION,
        workspaces: vec![single_leaf_workspace(1, "restore", ViewId::new(1))],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    }
}

/// Two workspaces, one leaf each, with `active` presented.
///
/// The workspaces carry their own leaf ids and names, so a row that compares the
/// live world before and after a refused restore compares two *different*
/// worlds: an assertion that the layout, the focus, or the presented workspace
/// id did not move can fail.
fn two_workspace_snapshot(active: usize) -> SessionSnapshot {
    let other = 1 - active;
    SessionSnapshot {
        version: bitty_runtime::SESSION_FORMAT_VERSION,
        workspaces: vec![
            single_leaf_workspace(1, "first", ViewId::new(1)),
            single_leaf_workspace(2, "second", ViewId::new(2)),
        ],
        active,
        mru: vec![active, other],
        pinned: Vec::new(),
    }
}

/// A runtime that replays `snapshot` and has an installed panel host.
fn runtime_with(snapshot: SessionSnapshot) -> (Runtime, Arc<CannedBackend>) {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let backend = Arc::new(CannedBackend { snapshot });
    rt.set_session_backend(Some(backend.clone()));
    let host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    rt.install_scene_host(host);
    (rt, backend)
}

/// A runtime with a session backend and an installed panel host.
fn runtime() -> (Runtime, Arc<CannedBackend>) {
    runtime_with(canned_snapshot())
}

/// The session path only needs a path to hand the backend; the stub ignores it.
fn session_path() -> PathBuf {
    PathBuf::from("panel-restore-path")
}

fn key(workspace: u64, persistent: &str) -> PanelRestoreKey {
    PanelRestoreKey::new(
        bitty_ui::WorkspaceId::new(workspace),
        PersistentId::new(persistent).expect("valid persistent id"),
    )
}

/// A record whose persisted (advisory) id is deliberately far from the ids the
/// registry mints, so a restore that reused it would be visible.
fn persisted(workspace: u64, persistent: &str, title: &str) -> PersistedPanel {
    PersistedPanel::new(PanelId::new(9), key(workspace, persistent), title).expect("record builds")
}

fn resolution() -> BTreeSet<String> {
    [CREATE, FOCUS]
        .iter()
        .map(|item| (*item).to_string())
        .collect()
}

fn owner() -> PluginId {
    PluginId::new("acme.notes").expect("valid plugin id")
}

/// Reads the placeholder and the recovery record set of a panel through the
/// installed host.
fn unbound_reason(rt: &Runtime, panel: PanelId) -> Option<PlaceholderReason> {
    let host = rt.scene_host().expect("panel host installed");
    let host = host.borrow();
    host.unbound_container_records(panel)
        .expect("container exists")
        .placeholder()
}

#[test]
fn a_session_restore_mints_a_fresh_panel_id_and_first_binds_the_installed_driver() {
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    let record = persisted(workspace, "logs", "logs");
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        record.clone(),
        PanelType::Helper,
        PanelDriverAnswer::Installed {
            owner: owner(),
            resolution: resolution(),
            consented: BTreeSet::new(),
        },
    )]);

    let summary = rt
        .load_session_from_path(&session_path())
        .expect("the restore applies");

    assert_eq!(summary.workspaces, 1, "the session itself restored");
    let report = rt.panel_restore_report();
    assert_eq!(report.len(), 1, "one record, one outcome");
    assert_eq!(report.bound(), 1);
    let PanelRestoreOutcome::Bound {
        panel,
        workspace: restored_workspace,
        grants,
    } = report.panels()[0].outcome.clone()
    else {
        panic!("the installed driver must bind: {:?}", report.panels()[0]);
    };
    assert_ne!(
        panel, record.id,
        "restart re-mints identity: the persisted id is advisory and never the container id"
    );
    assert_eq!(
        report.panels()[0].persisted_id,
        record.id,
        "the record's advisory id is reported so a caller can correlate"
    );
    assert_eq!(report.panels()[0].title, "logs", "the title travels across");
    assert_eq!(
        report.panels()[0].key,
        record.key,
        "the (WorkspaceId, PersistentId) key travels across"
    );
    assert_eq!(
        grants,
        resolution(),
        "a first bind commits the driver's whole resolution"
    );
    assert_ne!(
        restored_workspace.get(),
        workspace,
        "the container is attributed to the freshly minted slot id"
    );

    // The registry agrees: the container is live, bound to a controller
    // generation the first bind derived from the allocation epoch, and holding
    // the granted set.
    let host = rt.scene_host().expect("panel host installed");
    let host = host.borrow();
    assert_eq!(
        host.panel_count(),
        1,
        "one record restores exactly one container"
    );
    let epoch = host
        .panel_allocation_generation(panel)
        .expect("the restored container has an allocation epoch");
    let controller = host
        .registry()
        .controller_generation(panel)
        .expect("the first bind committed a controller generation");
    assert_eq!(
        Some(controller),
        epoch.next().ok(),
        "the first bind's controller generation is the allocation epoch's successor"
    );
    assert_ne!(
        controller, epoch,
        "a controller generation is never the allocation epoch"
    );
    for capability in [CREATE, FOCUS] {
        assert!(
            host.registry()
                .is_panel_capability_granted(panel, controller, capability),
            "{capability} must be granted by the first bind"
        );
    }
    assert_eq!(
        unbound_reason(&rt, panel),
        None,
        "a bound container presents no placeholder"
    );
    assert!(
        rt.panel_restore_records().is_empty(),
        "the restore consumed the records it applied, so a replay cannot mint a second container"
    );
}

#[test]
fn a_stable_identity_policy_refuses_the_whole_restore_before_any_mutation() {
    // The canned world presents its second workspace, with its own leaf id and
    // its own slot names: a restore that ran would move the presented workspace
    // id, the slot table, the layout, and the focus.
    let snapshot = two_workspace_snapshot(1);
    let (mut rt, _backend) = runtime_with(snapshot.clone());
    let workspace_before = rt.active_workspace_id();
    let leaves_before = rt.layout().leaf_ids();
    let focus_before = rt.focused_view();
    let names_before = rt.workspace_names();
    let canned_leaves: Vec<ViewId> = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.layout.leaf_ids())
        .collect();
    assert_ne!(
        canned_leaves, leaves_before,
        "the canned world is distinguishable from the live one"
    );

    rt.set_panel_persistence_policy(PersistencePolicy::StableIdentity);
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(1, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Missing,
    )]);

    let error = rt
        .load_session_from_path(&session_path())
        .expect_err("stable-identity cannot key a restore");

    assert_eq!(
        error,
        SessionError::PanelRestoreRefused(bitty_ui::ManifestError::StableIdentityUnsupported),
        "the refusal is the plan's own typed error"
    );
    assert!(
        error.to_string().contains("stable-identity"),
        "the diagnostic names the refused policy: {error}"
    );
    assert_eq!(
        rt.workspace_count(),
        1,
        "a refused restore installs no restored slot"
    );
    assert_eq!(
        rt.workspace_names(),
        names_before,
        "a refused restore replaces no slot, so the presented world keeps its name"
    );
    assert_eq!(
        rt.active_workspace_id(),
        workspace_before,
        "a refused restore mints no slot id: the presented workspace is the pre-restore one"
    );
    assert_eq!(
        rt.layout().leaf_ids(),
        leaves_before,
        "a refused restore leaves the live layout (its leaf ids) unchanged"
    );
    assert_eq!(
        rt.focused_view(),
        focus_before,
        "a refused restore leaves the focus unchanged"
    );
    assert!(
        !rt.session_restored(),
        "no restore marker is set: nothing was applied"
    );
    let host = rt.scene_host().expect("panel host installed");
    assert_eq!(
        host.borrow().panel_count(),
        0,
        "a refused restore allocates no container"
    );
    assert!(
        rt.panel_restore_report().is_empty(),
        "a refused restore reports nothing applied"
    );
}

#[test]
fn a_missing_driver_keeps_the_container_with_an_unloaded_placeholder() {
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Missing,
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the restore applies");

    let report = rt.panel_restore_report();
    let PanelRestoreOutcome::Placeholder {
        panel,
        workspace: restored_workspace,
        reason,
    } = report.panels()[0].outcome.clone()
    else {
        panic!("a missing driver presents a placeholder: {report:?}");
    };
    assert_eq!(reason, PlaceholderReason::Unloaded);
    assert_eq!(
        unbound_reason(&rt, panel),
        Some(PlaceholderReason::Unloaded),
        "the container presents the unloaded placeholder"
    );
    assert_placeholder_kept_the_container(&rt, panel, restored_workspace.get());
}

#[test]
fn a_denied_driver_keeps_the_container_with_a_denied_placeholder() {
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Denied,
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the restore applies");

    let report = rt.panel_restore_report();
    let PanelRestoreOutcome::Placeholder {
        panel,
        workspace: restored_workspace,
        reason,
    } = report.panels()[0].outcome.clone()
    else {
        panic!("a denied driver presents a placeholder: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Denied,
        "a denial is `denied`, which Decision E classes as `unavailable`"
    );
    assert_eq!(
        unbound_reason(&rt, panel),
        Some(PlaceholderReason::Denied),
        "the container presents the denied placeholder"
    );
    assert_placeholder_kept_the_container(&rt, panel, restored_workspace.get());
}

#[test]
fn an_older_driver_keeps_the_container_with_an_incompatible_placeholder() {
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Older,
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the restore applies");

    let report = rt.panel_restore_report();
    let PanelRestoreOutcome::Placeholder {
        panel,
        workspace: restored_workspace,
        reason,
    } = report.panels()[0].outcome.clone()
    else {
        panic!("an older driver presents a placeholder: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Incompatible,
        "an older driver is `incompatible`: nothing is downgraded or partially applied"
    );
    assert_eq!(
        unbound_reason(&rt, panel),
        Some(PlaceholderReason::Incompatible)
    );
    assert_placeholder_kept_the_container(&rt, panel, restored_workspace.get());
}

/// bitty#3239 row 13: an installed driver whose registered provider manifest
/// declares a schema newer than the runtime reads never binds. The container
/// stays, presents `incompatible`, and applies nothing.
///
/// Discriminates the skew gate in `Runtime::restore_one_panel`
/// (`runtime/panel_restore.rs`, the `provider_contract_outruns_runtime` branch
/// of the `PanelDriverAnswer::Installed` arm): reverted to bind unconditionally,
/// this row restores a `Bound` outcome and no placeholder. It also needs
/// `PanelProviderManifest::parse` to admit a version above
/// `PROVIDER_MANIFEST_VERSION_V1`, because the manifest could not be registered
/// at all before.
#[test]
fn a_provider_manifest_newer_than_the_runtime_keeps_the_container_with_an_incompatible_placeholder()
{
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    register_provider(&rt, owner().as_str(), 2);
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Installed {
            owner: owner(),
            resolution: resolution(),
            consented: BTreeSet::new(),
        },
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the session still restores");

    let report = rt.panel_restore_report();
    assert_eq!(
        report.bound(),
        0,
        "a contract this runtime cannot read is never committed: {report:?}"
    );
    let PanelRestoreOutcome::Placeholder {
        panel,
        workspace: restored_workspace,
        reason,
    } = report.panels()[0].outcome.clone()
    else {
        panic!("an unreadable provider contract presents a placeholder: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Incompatible,
        "the driver's provider outruns this runtime: `incompatible`, not `unloaded`"
    );
    assert_eq!(
        unbound_reason(&rt, panel),
        Some(PlaceholderReason::Incompatible),
        "the retained container presents the placeholder"
    );
    assert_placeholder_kept_the_container(&rt, panel, restored_workspace.get());
    assert_nothing_was_granted(&rt, panel, &resolution());
}

/// The skew gate is a version comparison, not a blanket refusal: a driver whose
/// provider registered the schema this runtime reads still first-binds.
///
/// Discriminates the same line as the row above from the other side — a gate
/// that refused every registered provider, or that ignored `version()`, turns
/// this row red.
#[test]
fn a_provider_manifest_at_the_supported_schema_still_first_binds() {
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    register_provider(
        &rt,
        owner().as_str(),
        bitty_runtime::registry::PROVIDER_MANIFEST_VERSION_V1,
    );
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Installed {
            owner: owner(),
            resolution: resolution(),
            consented: BTreeSet::new(),
        },
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the restore applies");

    let report = rt.panel_restore_report();
    assert_eq!(report.bound(), 1, "the current schema binds: {report:?}");
    let PanelRestoreOutcome::Bound { panel, .. } = report.panels()[0].outcome else {
        panic!("the current schema binds: {report:?}");
    };
    assert_eq!(
        unbound_reason(&rt, panel),
        None,
        "a bound container presents no placeholder"
    );
}

/// Registers one provider manifest on the installed panel host.
fn register_provider(rt: &Runtime, owner: &str, version: u32) {
    let host = rt.scene_host().expect("panel host installed");
    let manifest = bitty_runtime::registry::PanelProviderManifest::parse(
        owner,
        vec![PanelType::Helper],
        version,
    )
    .expect("the manifest parses");
    host.borrow_mut()
        .register_panel_provider(manifest, true)
        .expect("the provider registers");
}

/// Every capability the refused driver offered is ungranted on the container's
/// allocation epoch, so nothing was applied by the refused bind.
fn assert_nothing_was_granted(rt: &Runtime, panel: PanelId, resolution: &BTreeSet<String>) {
    let host = rt.scene_host().expect("panel host installed");
    let host = host.borrow();
    let epoch = host
        .panel_allocation_generation(panel)
        .expect("the container has an allocation epoch");
    for capability in resolution {
        assert!(
            !host
                .registry()
                .is_panel_capability_granted(panel, epoch, capability),
            "{capability} must not be granted by a refused bind"
        );
    }
}

/// Shared assertion for the three refusal rows: the container exists, is
/// unbound, never mounted, keeps the record's workspace attribution, and
/// renders the Core-owned placeholder scene.
fn assert_placeholder_kept_the_container(rt: &Runtime, panel: PanelId, workspace: u64) {
    let host = rt.scene_host().expect("panel host installed");
    let host = host.borrow();
    assert_eq!(host.panel_count(), 1, "the container is retained");
    assert_eq!(
        host.registry().controller_generation(panel),
        None,
        "the container is unbound: nothing was applied"
    );
    let epoch = host
        .panel_allocation_generation(panel)
        .expect("the container has an allocation epoch");
    assert_eq!(
        host.panel_state(panel, epoch).expect("container exists"),
        PanelState::Created,
        "the restored container was never mounted by a driver"
    );
    assert_eq!(
        host.registry()
            .panels_for_workspace(bitty_runtime::registry::WorkspaceId::new(workspace)),
        vec![panel],
        "the record's workspace attribution survives the refusal"
    );
    assert!(
        host.registry()
            .placeholder_block(panel)
            .expect("the placeholder scene builds")
            .is_some(),
        "the Core-owned placeholder surface is present"
    );
}

#[test]
fn a_two_workspace_session_restores_each_record_on_the_slot_its_ordinal_names() {
    let (mut rt, _backend) = runtime_with(two_workspace_snapshot(0));
    assert_eq!(
        rt.workspace_count(),
        1,
        "the live window presents one workspace, so the second persisted workspace has no live id to match"
    );
    rt.set_panel_restore_records(vec![
        PanelRestoreInput::new(
            persisted(1, "logs", "logs"),
            PanelType::Helper,
            PanelDriverAnswer::Missing,
        ),
        PanelRestoreInput::new(
            persisted(2, "notes", "notes"),
            PanelType::Helper,
            PanelDriverAnswer::Missing,
        ),
    ]);

    let summary = rt
        .load_session_from_path(&session_path())
        .expect("the restore applies");
    assert_eq!(summary.workspaces, 2, "both persisted workspaces restored");

    let (logs_panel, logs_slot, notes_panel, notes_slot) = {
        let report = rt.panel_restore_report();
        assert_eq!(report.len(), 2, "both records are considered");
        let PanelRestoreOutcome::Placeholder {
            panel: logs_panel,
            workspace: logs_slot,
            reason: logs_reason,
        } = report.panels()[0].outcome.clone()
        else {
            panic!("ordinal 1 must restore a container: {report:?}");
        };
        assert_eq!(logs_reason, PlaceholderReason::Unloaded);
        let PanelRestoreOutcome::Placeholder {
            panel: notes_panel,
            workspace: notes_slot,
            reason: notes_reason,
        } = report.panels()[1].outcome.clone()
        else {
            panic!(
                "ordinal 2 must restore a container rather than drop as `WorkspaceGone`: {report:?}"
            );
        };
        assert_eq!(notes_reason, PlaceholderReason::Unloaded);
        (logs_panel, logs_slot, notes_panel, notes_slot)
    };
    assert_ne!(
        logs_slot, notes_slot,
        "each record lands on its own restored slot"
    );

    // Each container sits on a slot the window presents — resolved through the
    // record's ordinal, never through an id the record itself supplied.
    assert_eq!(
        rt.active_workspace_id(),
        logs_slot,
        "ordinal 1 is the restored slot the snapshot presents"
    );
    assert!(
        rt.workspace_switch(1),
        "the second restored slot switches in"
    );
    assert_eq!(
        rt.active_workspace_id(),
        notes_slot,
        "ordinal 2 is the second restored slot, and it is a workspace the window presents"
    );

    let host = rt.scene_host().expect("panel host installed");
    let host = host.borrow();
    assert_eq!(
        host.registry().panels_for_workspace(logs_slot),
        vec![logs_panel],
        "the ordinal-1 record's container is attributed to the first restored slot"
    );
    assert_eq!(
        host.registry().panels_for_workspace(notes_slot),
        vec![notes_panel],
        "the ordinal-2 record's container is attributed to the second restored slot"
    );
    assert_eq!(
        host.registry().attributed_workspace_count(),
        2,
        "both restored slots own a bucket"
    );
}

#[test]
fn a_second_restore_releases_containers_attributed_to_the_replaced_world() {
    let (mut rt, _backend) = runtime();
    let workspace = rt.active_workspace_id().get();
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Installed {
            owner: owner(),
            resolution: resolution(),
            consented: BTreeSet::new(),
        },
    )]);
    rt.load_session_from_path(&session_path())
        .expect("the first restore applies");

    let (panel, first_slot) = {
        let report = rt.panel_restore_report();
        let PanelRestoreOutcome::Bound {
            panel, workspace, ..
        } = report.panels()[0].outcome
        else {
            panic!("the installed driver must bind: {report:?}");
        };
        (panel, workspace)
    };
    {
        let host = rt.scene_host().expect("panel host installed");
        let host = host.borrow();
        assert_eq!(
            host.registry().panels_for_workspace(first_slot),
            vec![panel],
            "the first restore attributes its container to the restored slot"
        );
    }

    // Replay the same session with no new records: the restore still installs a
    // whole new slot table, so the id the container is attributed to dies.
    rt.load_session_from_path(&session_path())
        .expect("the replay applies");

    let live_slot = rt.active_workspace_id();
    assert_ne!(
        live_slot, first_slot,
        "the replay mints a fresh slot id, so the container's attribution is dead"
    );
    assert!(
        rt.panel_restore_report().is_empty(),
        "the replay had no records to apply"
    );
    let host = rt.scene_host().expect("panel host installed");
    let host = host.borrow();
    assert_eq!(
        host.panel_count(),
        0,
        "the container attributed to the replaced world was released, not orphaned"
    );
    assert_eq!(
        host.panel_allocation_generation(panel),
        None,
        "the released container is disposed, so it no longer answers an allocation epoch"
    );
    assert!(
        host.registry().panels_for_workspace(first_slot).is_empty(),
        "nothing is left attributed to the workspace that no longer exists"
    );
    assert!(
        host.registry().panels_for_workspace(live_slot).is_empty(),
        "the replay restored no container of its own"
    );
    assert_eq!(
        host.registry().attributed_workspace_count(),
        0,
        "a disposed workspace leaves no empty bucket behind"
    );
}

#[test]
fn a_record_whose_workspace_is_gone_is_dropped_not_rehomed() {
    let (mut rt, _backend) = runtime();
    // `99` is past the last workspace the persisted session carries.
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(99, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Missing,
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the session still restores");

    let report = rt.panel_restore_report();
    assert_eq!(
        report.panels()[0].outcome,
        PanelRestoreOutcome::Dropped {
            reason: PanelRestoreDrop::WorkspaceGone
        },
        "an unresolvable workspace is a bounded drop, never a re-home"
    );
    let host = rt.scene_host().expect("panel host installed");
    assert_eq!(
        host.borrow().panel_count(),
        0,
        "a dropped record allocates nothing"
    );
}

/// A restore that finds no panel host defers its panel step instead of
/// bounding every record out; the host install that follows commits it
/// (bitty#3243).
///
/// The shipped order restores the session before `bitty-terminal` has built the
/// panel host, so a step that ran immediately would drop every record with
/// `PanelRestoreDrop::NoPanelHost`. Nothing is decided — and nothing is lost —
/// until a host exists.
#[test]
fn a_runtime_without_a_panel_host_defers_the_record_until_one_is_installed() {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let backend = Arc::new(CannedBackend {
        snapshot: canned_snapshot(),
    });
    rt.set_session_backend(Some(backend));
    let workspace = rt.active_workspace_id().get();
    rt.set_panel_restore_records(vec![PanelRestoreInput::new(
        persisted(workspace, "logs", "logs"),
        PanelType::Helper,
        PanelDriverAnswer::Missing,
    )]);

    rt.load_session_from_path(&session_path())
        .expect("the session still restores without a panel host");

    assert!(
        rt.panel_restore_pending(),
        "no host yet: the step is deferred, not decided"
    );
    assert!(
        rt.panel_restore_report().is_empty(),
        "no record was dropped: {:#?}",
        rt.panel_restore_report()
    );
    assert_eq!(
        rt.panel_restore_records().len(),
        1,
        "the records stay configured for the host that arrives later"
    );

    // The composition root installs the host once it has one.
    let host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host builds");
    rt.install_scene_host(host);

    let report = rt.panel_restore_report();
    assert_eq!(report.len(), 1, "the deferred step ran: {report:?}");
    let PanelRestoreOutcome::Placeholder { reason, .. } = report.panels()[0].outcome else {
        panic!("a missing driver keeps the container: {report:?}");
    };
    assert_eq!(
        reason,
        PlaceholderReason::Unloaded,
        "the deferred record restored as a placeholder instead of dropping"
    );
    assert!(
        rt.panel_restore_records().is_empty(),
        "the step consumed the records it applied"
    );
}
