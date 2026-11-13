//! Panel-attributed presentation surfaces (bitty#2476 residual 8).
//!
//! The presentation session (`bitty.ui.overlay.show` / `bitty.present.show`)
//! used to admit its surface into a private `OverlayManager`, so
//! `PanelRegistry::create_panel_overlay` had no production writer and
//! `release_attributed_overlays` dismissed zero surfaces on the live path.
//! Here the same session attributes the surface to the presenting generation's
//! own Core-owned panel — the `terminal`-slot scene panel `SceneHost` owns for
//! `(owner, epoch)` — and the generation release that disposes that panel
//! dismisses the surface through `release_generation_scope`.
//!
//! Every case drives the production composition seam: the app-owned
//! `SceneHost` is installed through `PluginRuntime::set_ui_scene_backend`, and
//! the plugin calls the shipped Lua verbs over a real VM. Headless and
//! Windows-safe (temp dirs only, no PTY, no processes or sockets).
//!
//! Boundary the tests state rather than hide: a generation that mounted no
//! terminal-slot scene owns no panel, so its presentation surface still
//! presents and carries no attribution (the host's refusal is counted, never
//! silent). Attributing those surfaces would need a panel ownership relation
//! this record does not decide.

mod common;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, HostServices, LuaValue, RC1_WALL_CLOCK_BUDGET_MS};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};
use bitty_runtime::registry::{PanelRegistryConfig, PanelRuntime};
use bitty_runtime::scene_host::SceneHost;
use bitty_ui::ViewId;

/// Activation wall budget for this harness.
///
/// The fixture plugin allocates a scene panel at activation, which is more work
/// than a registration-only probe; RC-1 (50 ms) is calibrated for release
/// builds and a loaded runner can deschedule a mount across it. Panel
/// attribution, not runner speed, is under test, so only the wall dimension is
/// widened (the bitty#1744/#1727 treatment). Production keeps the accepted
/// budget; activation-budget enforcement stays pinned by `bitty-lua`'s own
/// suites.
const HARNESS_WALL_BUDGET_MS: u64 = 10 * RC1_WALL_CLOCK_BUDGET_MS;

/// Placement leaf the scene host presents on.
fn leaf() -> ViewId {
    ViewId::new(7)
}

struct StaticSnapshot;

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-presentation-attribution-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write one fixture plugin whose `init.lua` is `init_src`.
fn write_plugin(root: &Path, id: &str, capabilities: &[&str], init_src: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let caps = capabilities
        .iter()
        .map(|capability| format!("{capability} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Presentation attribution test"
version = "0.1.0"
description = "panel-attributed presentation surface test"

[compat]
plugin-api = "^1.0"

[capabilities]
{caps}

[lazy]
commands = []
events = []
claims = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

/// Mount a terminal-slot scene (which mints the generation's panel) and an
/// `overlay`-slot block the session can present.
const MOUNT_SCENE_AND_OVERLAY: &str = r#"
local scene_ok, scene_handle = pcall(bitty.ui.mount, "terminal", { kind = "Text", text = "scene" })
bitty.store.set("scene_ok", scene_ok)
bitty.store.set("scene_handle", scene_ok and scene_handle or -1)
local overlay_ok, overlay_handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "hint" })
bitty.store.set("overlay_ok", overlay_ok)
bitty.store.set("handle", overlay_ok and overlay_handle or -1)
return {}
"#;

/// Mount only the `overlay`-slot block: a generation that owns no panel.
const MOUNT_OVERLAY_ONLY: &str = r#"
local overlay_ok, overlay_handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "hint" })
bitty.store.set("overlay_ok", overlay_ok)
bitty.store.set("handle", overlay_ok and overlay_handle or -1)
return {}
"#;

/// `ui.rich` mounts, `ui.overlay` gates the `overlay` slot, `ui.present` draws.
const PRESENT_GRANTS: &[&str] = &["ui.rich", "ui.overlay", "ui.present"];

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    panels: Rc<RefCell<PanelRuntime>>,
    scene: Rc<SceneHost>,
}

impl Fixture {
    /// A runtime whose panel host is the app-owned one, installed before
    /// activation exactly as the composition root does.
    fn new(tag: &str) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        let panels = Rc::new(RefCell::new(
            PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host"),
        ));
        let target = Rc::new(Cell::new(Some(leaf())));
        let scene = Rc::new(SceneHost::new(Rc::clone(&panels), target));
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings: Rc::new(EmptySettings),
            snapshot: Rc::new(StaticSnapshot),
        });
        common::install_stub_backend(&mut runtime);
        runtime.set_vm_wall_budget_ms(Some(HARNESS_WALL_BUDGET_MS));
        runtime.set_host_call_deadline_ms(Some(common::HARNESS_HOST_CALL_DEADLINE_MS));
        // The one production call that installs both the terminal-slot scene
        // host and this session's attribution sink.
        runtime.set_ui_scene_backend(Some(Rc::clone(&scene)));
        Self {
            root,
            data,
            runtime,
            panels,
            scene,
        }
    }

    fn activate(&mut self, id: &str) -> PluginId {
        let id = PluginId::new(id).expect("valid id");
        self.runtime.discover();
        let report = self.runtime.activate(&id).expect("activate");
        assert_eq!(report.state, LifecycleState::Active);
        id
    }

    fn stored(&self, id: &PluginId, key: &str) -> Option<LuaValue> {
        self.runtime
            .services(id)
            .expect("services")
            .with_store(|store| store.get(key))
    }

    fn handle(&self, id: &PluginId) -> i64 {
        match self.stored(id, "handle") {
            Some(LuaValue::Integer(handle)) => handle,
            other => panic!("mount must return a handle, got {other:?}"),
        }
    }

    fn show(&self, id: &PluginId) -> bool {
        let handle = self.handle(id);
        self.runtime
            .services(id)
            .expect("services")
            .clone()
            .ui_overlay_show(handle)
            .expect("show")
    }

    /// Live registry-attributed row of the presentation session, if any.
    fn attributed(&self) -> Option<u64> {
        self.runtime
            .presentation_overlay()
            .borrow()
            .attributed_overlay()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// A presented surface is attributed to the presenting generation's own panel
/// and dismissed with it (bitty#2476 residual 8).
///
/// Fails on the pre-fix tree at the first attribution assertion: the session
/// admitted into its private manager and the registry held zero attributed
/// overlays, so `create_panel_overlay` had no production writer.
#[test]
fn presented_surface_is_attributed_to_the_generation_panel() {
    let mut fixture = Fixture::new("attributed");
    write_plugin(
        &fixture.root,
        "acme.help",
        PRESENT_GRANTS,
        MOUNT_SCENE_AND_OVERLAY,
    );
    let id = fixture.activate("acme.help");
    assert_eq!(
        fixture.stored(&id, "scene_ok"),
        Some(LuaValue::Bool(true)),
        "the generation mounted its terminal-slot scene"
    );

    // The scene host is the panel owner: one Core-owned panel for this
    // generation, minted by `ui.mount("terminal", …)`, not by the plugin.
    let panel = fixture
        .scene
        .panel_handle_for("acme.help")
        .expect("the generation owns a panel");
    assert_eq!(fixture.panels.borrow().panel_count(), 1);
    assert_eq!(
        fixture.panels.borrow().attributed_overlay_count(),
        0,
        "nothing is attributed before a surface is presented"
    );

    assert!(fixture.show(&id), "the owner presents");

    // (a) production writer for `create_panel_overlay`: exactly one overlay is
    // attributed, to the presenting generation's panel and generation.
    assert_eq!(
        fixture.panels.borrow().attributed_overlay_count(),
        1,
        "the presented surface is attributed in the panel registry"
    );
    let overlay_id = fixture.attributed().expect("the session holds its row");
    assert_eq!(
        fixture.panels.borrow().overlay_attribution_of(overlay_id),
        Some((panel.id, panel.generation)),
        "attributed to the generation's panel and its generation"
    );
    assert_eq!(fixture.panels.borrow().overlay_count(), 1);

    // Hide dismisses the surface and its row together: no attribution outlives
    // the surface it names.
    let handle = fixture.handle(&id);
    assert!(
        fixture
            .runtime
            .services(&id)
            .expect("services")
            .clone()
            .ui_overlay_hide(handle)
            .expect("hide")
    );
    assert_eq!(fixture.panels.borrow().attributed_overlay_count(), 0);
    assert_eq!(fixture.panels.borrow().overlay_count(), 0);
    assert!(fixture.attributed().is_none());
}

/// The generation release dismisses the attributed surface through the registry
/// (bitty#2476 residual 8, limb (b)).
///
/// The disposal edge runs `release_generation_scope` for a mounted panel, which
/// dismisses the attributed overlays. The presentation session is deliberately
/// left live: it still holds the row id, so the row can only be gone because
/// the registry released it, never because the session's own `clear()` ran.
#[test]
fn generation_release_dismisses_the_attributed_surface() {
    let mut fixture = Fixture::new("release");
    write_plugin(
        &fixture.root,
        "acme.help",
        PRESENT_GRANTS,
        MOUNT_SCENE_AND_OVERLAY,
    );
    let id = fixture.activate("acme.help");
    let panel = fixture
        .scene
        .panel_handle_for("acme.help")
        .expect("the generation owns a panel");
    assert!(fixture.show(&id));
    let overlay_id = fixture.attributed().expect("attributed on show");
    assert_eq!(fixture.panels.borrow().attributed_overlay_count(), 1);

    // The generation's panel is retired by the host (workspace close, reload,
    // or a generation edge that disposes the container).
    fixture
        .panels
        .borrow_mut()
        .dispose_panel(panel.id, panel.generation)
        .expect("the host retires the generation panel");

    assert_eq!(
        fixture.panels.borrow().attributed_overlay_count(),
        0,
        "the release dismissed the attributed overlay"
    );
    assert_eq!(fixture.panels.borrow().overlay_count(), 0);
    assert!(
        fixture.attributed() == Some(overlay_id),
        "the session never ran its own release: the registry release did"
    );
    assert!(
        fixture.runtime.presentation_overlay_session().is_some(),
        "the presentation session is untouched by a panel disposal"
    );

    // The generation edge that follows settles the session for good; its
    // release of an already dismissed id is an idempotent no-op.
    fixture.runtime.suspend(&id).expect("suspend");
    assert!(fixture.runtime.presentation_overlay_session().is_none());
    assert_eq!(fixture.panels.borrow().attributed_overlay_count(), 0);
    assert_eq!(fixture.panels.borrow().overlay_count(), 0);
    assert_eq!(fixture.panels.borrow().panel_count(), 0);
}

/// Suspend is the shipped plugin generation edge: the attributed surface is
/// gone before the session is revoked, and nothing survives the generation.
#[test]
fn suspend_releases_the_attributed_surface() {
    let mut fixture = Fixture::new("suspend");
    write_plugin(
        &fixture.root,
        "acme.help",
        PRESENT_GRANTS,
        MOUNT_SCENE_AND_OVERLAY,
    );
    let id = fixture.activate("acme.help");
    assert!(fixture.show(&id));
    assert_eq!(fixture.panels.borrow().attributed_overlay_count(), 1);

    fixture.runtime.suspend(&id).expect("suspend");

    assert!(fixture.runtime.presentation_overlay_session().is_none());
    assert_eq!(
        fixture.panels.borrow().attributed_overlay_count(),
        0,
        "the generation release dismissed the attributed overlay"
    );
    assert_eq!(fixture.panels.borrow().overlay_count(), 0);
    assert_eq!(fixture.panels.borrow().panel_count(), 0);
    assert!(fixture.scene.panel_handle_for("acme.help").is_none());
}

/// A generation that owns no panel presents unattributed, and the refusal is
/// counted (stated boundary of residual 8, not a silent fallback).
#[test]
fn generation_without_a_panel_presents_unattributed() {
    let mut fixture = Fixture::new("unattributed");
    write_plugin(
        &fixture.root,
        "acme.help",
        PRESENT_GRANTS,
        MOUNT_OVERLAY_ONLY,
    );
    let id = fixture.activate("acme.help");
    assert!(
        fixture.scene.panel_handle_for("acme.help").is_none(),
        "an overlay-only generation owns no panel"
    );

    assert!(fixture.show(&id), "the surface still presents");

    assert!(fixture.runtime.presentation_overlay_session().is_some());
    assert_eq!(fixture.panels.borrow().attributed_overlay_count(), 0);
    assert_eq!(fixture.panels.borrow().panel_count(), 0);
    {
        let overlay = fixture.runtime.presentation_overlay().borrow();
        assert_eq!(overlay.admitted_slots(), 1, "one admitted slot");
        assert_eq!(
            overlay.attribution_refusals(),
            1,
            "the unavailable attribution is counted, never silent"
        );
        assert!(overlay.attributed_overlay().is_none());
    }
}
