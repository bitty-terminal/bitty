//! Presentation-only overlay host API (CTX-1431, `bitty` #2406).
//!
//! Phase-A coverage over the accepted design stage 2: `bitty.ui.overlay.show`
//! and `hide` present an `overlay`-slot block without any capture grant, admit
//! one `NonModal`/`Popup` surface per window under the accepted overlay
//! budget, keep `modal_active` false, fail closed for a second plugin, and
//! clear the surface on suspend, unload, crash, and generation change. `Esc`
//! queues exactly one `overlay.dismissed` observation for the cold tick.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bitty_lua::{HostServices, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot;

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, bitty_lua::BridgeError> {
        if scope != "semantic" {
            return Err(bitty_lua::BridgeError::new(
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
        "bitty-presentation-overlay-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write one fixture plugin whose `init.lua` mounts a block into `slot` and
/// stores the handle under `handle`.
fn write_plugin(root: &Path, id: &str, capabilities: &[&str], slot: &str) {
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
name = "Presentation overlay test"
version = "0.1.0"
description = "presentation-only overlay host api test"

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
    std::fs::write(
        plugin.join("lua/init.lua"),
        format!(
            r#"
            local ok, handle = pcall(bitty.ui.mount, "{slot}", {{ kind = "Text", text = "panel" }})
            bitty.store.set("mount_ok", ok)
            bitty.store.set("handle", ok and handle or -1)
            return {{}}
            "#
        ),
    )
    .expect("init");
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        let runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings: std::rc::Rc::new(MapSettings::default()),
            snapshot: std::rc::Rc::new(StaticSnapshot),
        });
        let mut runtime = runtime;
        common::install_stub_backend(&mut runtime);
        Self {
            root,
            data,
            runtime,
        }
    }

    fn activate(&mut self, id: &str) -> PluginId {
        let id = PluginId::new(id).expect("valid id");
        let report = self.runtime.activate(&id).expect("activate");
        assert_eq!(report.state, LifecycleState::Active);
        id
    }

    fn handle(&self, id: &PluginId) -> i64 {
        match self
            .runtime
            .services(id)
            .expect("services")
            .with_store(|store| store.get("handle"))
        {
            Some(LuaValue::Integer(handle)) => handle,
            other => panic!("mount must return a handle, got {other:?}"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

const V1_GRANTS: &[&str] = &["ui.rich", "ui.overlay"];

/// A v1 mount owner can present and hide its own panel with no capture grant,
/// no capture session, and no modal authority.
#[test]
fn show_and_hide_present_without_capture() {
    let mut fixture = Fixture::new("lifecycle");
    write_plugin(&fixture.root, "acme.help", V1_GRANTS, "overlay");
    fixture.runtime.discover();
    let id = fixture.activate("acme.help");
    let handle = fixture.handle(&id);
    assert!(handle > 0);
    assert!(
        !fixture
            .runtime
            .services(&id)
            .expect("services")
            .ui_access()
            .overlay_focus,
        "the fixture has no capture grant"
    );

    let services = fixture.runtime.services(&id).expect("services").clone();
    assert!(services.ui_overlay_show(handle).expect("show"));
    {
        let overlay = fixture.runtime.presentation_overlay().borrow();
        assert!(overlay.is_active());
        assert_eq!(overlay.session(), Some(("acme.help", handle)));
        assert_eq!(overlay.kind(), bitty_ui::OverlayKind::NonModal);
        assert_eq!(overlay.tier(), bitty_ui::OverlayTier::Popup);
        assert!(!overlay.modal_active(), "presentation is never modal");
        assert_eq!(overlay.admitted_slots(), 1);
    }
    assert!(
        !fixture.runtime.overlay_capture().borrow().is_active(),
        "show never claims input capture"
    );
    assert!(
        !fixture.runtime.push_overlay_input("key", "x"),
        "terminal input is unaffected by a presentation surface"
    );

    // Hide is idempotent and releases the admitted slot.
    assert!(services.ui_overlay_hide(handle).expect("hide"));
    assert!(!services.ui_overlay_hide(handle).expect("second hide"));
    assert!(!fixture.runtime.presentation_overlay().borrow().is_active());
    assert_eq!(
        fixture
            .runtime
            .presentation_overlay()
            .borrow()
            .admitted_slots(),
        0
    );
}

/// A second plugin's show fails closed with `E_UI_UNAVAILABLE` instead of
/// stacking; the owner can re-show to replace its own content.
#[test]
fn one_presentation_surface_per_window() {
    let mut fixture = Fixture::new("one-surface");
    write_plugin(&fixture.root, "acme.help", V1_GRANTS, "overlay");
    write_plugin(&fixture.root, "acme.other", V1_GRANTS, "overlay");
    fixture.runtime.discover();
    let owner = fixture.activate("acme.help");
    let other = fixture.activate("acme.other");
    let owner_handle = fixture.handle(&owner);
    let other_handle = fixture.handle(&other);

    let owner_services = fixture.runtime.services(&owner).expect("services").clone();
    let other_services = fixture.runtime.services(&other).expect("services").clone();
    assert!(owner_services.ui_overlay_show(owner_handle).expect("show"));
    let error = other_services
        .ui_overlay_show(other_handle)
        .expect_err("second plugin must fail closed");
    assert_eq!(error.class, "runtime");
    assert_eq!(error.code, "E_UI_UNAVAILABLE");
    assert_eq!(
        fixture.runtime.presentation_overlay_session(),
        Some((String::from("acme.help"), owner_handle)),
        "the live owner is unchanged"
    );
    assert_eq!(
        fixture
            .runtime
            .presentation_overlay()
            .borrow()
            .admitted_slots(),
        1,
        "the failed show consumed no slot"
    );

    // The owner re-show replaces the presented handle in place.
    let replacement = fixture
        .runtime
        .services(&owner)
        .expect("services")
        .ui_mount("overlay", &bitty_lua::ui::UiNode::text("replacement"))
        .expect("second mount");
    assert!(
        owner_services
            .ui_overlay_show(replacement)
            .expect("re-show")
    );
    assert_eq!(
        fixture.runtime.presentation_overlay_session(),
        Some((String::from("acme.help"), replacement))
    );
    assert!(
        !other_services
            .ui_overlay_hide(owner_handle)
            .expect("foreign hide")
    );
    assert!(
        fixture.runtime.presentation_overlay().borrow().is_active(),
        "a foreign hide never steals the surface"
    );
    assert!(owner_services.ui_overlay_hide(replacement).expect("hide"));
}

/// The presentation path requires the two v1 mount grants and never accepts a
/// non-`overlay` handle.
#[test]
fn show_requires_v1_grants_and_an_overlay_handle() {
    let mut fixture = Fixture::new("grants");
    write_plugin(&fixture.root, "acme.richonly", &["ui.rich"], "overlay");
    write_plugin(&fixture.root, "acme.top", V1_GRANTS, "top");
    write_plugin(&fixture.root, "acme.overlay", V1_GRANTS, "overlay");
    fixture.runtime.discover();
    let rich_only = fixture.activate("acme.richonly");
    let top = fixture.activate("acme.top");
    let overlay = fixture.activate("acme.overlay");

    // Without `ui.overlay` the mount already failed, so the show gate is
    // probed directly: the capability denial precedes any handle inspection.
    let error = fixture
        .runtime
        .services(&rich_only)
        .expect("services")
        .ui_overlay_show(1)
        .expect_err("missing ui.overlay must deny");
    assert_eq!(error.code, "E_CAPABILITY_DENIED");
    assert!(error.message.contains("ui.overlay"), "{}", error.message);

    // A retained `top` block is not presentable.
    let top_handle = fixture.handle(&top);
    let error = fixture
        .runtime
        .services(&top)
        .expect("services")
        .ui_overlay_show(top_handle)
        .expect_err("non-overlay handle must fail closed");
    assert_eq!(error.code, "E_UI_NOT_OWNER");

    // `ui.overlay.focus` is neither required nor implied: the v1 owner shows.
    assert!(
        fixture
            .runtime
            .services(&overlay)
            .expect("services")
            .ui_overlay_show(fixture.handle(&overlay))
            .expect("v1 grants are sufficient")
    );
    assert!(!fixture.runtime.overlay_capture().borrow().is_active());
}

/// `Esc` queues exactly one observation and clears the surface; lifecycle
/// teardown clears without an observation.
#[test]
fn escape_and_teardown_clear_the_surface() {
    let mut fixture = Fixture::new("escape");
    write_plugin(&fixture.root, "acme.help", V1_GRANTS, "overlay");
    fixture.runtime.discover();
    let id = fixture.activate("acme.help");
    let handle = fixture.handle(&id);
    let services = fixture.runtime.services(&id).expect("services").clone();
    assert!(services.ui_overlay_show(handle).expect("show"));

    assert!(fixture.runtime.dismiss_presentation_overlay());
    assert!(!fixture.runtime.dismiss_presentation_overlay());
    assert!(!fixture.runtime.presentation_overlay().borrow().is_active());
    let events = fixture.runtime.drain_overlay_dismissed();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].owner, "acme.help");
    assert_eq!(events[0].handle, handle);
    assert_eq!(events[0].reason, "escape");
    assert!(fixture.runtime.drain_overlay_dismissed().is_empty());

    // Re-show then suspend: the surface leaves with the generation.
    assert!(services.ui_overlay_show(handle).expect("re-show"));
    fixture.runtime.suspend(&id).expect("suspend");
    assert!(
        !fixture.runtime.presentation_overlay().borrow().is_active(),
        "suspend clears the presentation surface"
    );

    // Resume clears UI blocks; the retained handle is stale and a fresh mount
    // is required before presenting again.
    fixture.runtime.resume(&id).expect("resume");
    let error = fixture
        .runtime
        .services(&id)
        .expect("services")
        .ui_overlay_show(handle)
        .expect_err("a pre-suspend handle is stale");
    assert_eq!(error.code, "E_UI_NOT_OWNER");
}

/// Unload/crash and a generation change drop the surface; a reloaded
/// generation gets a fresh epoch and must present its own mount.
#[test]
fn dispose_and_generation_change_drop_the_surface() {
    let mut fixture = Fixture::new("teardown");
    write_plugin(&fixture.root, "acme.help", V1_GRANTS, "overlay");
    fixture.runtime.discover();
    let id = fixture.activate("acme.help");
    let first = fixture.handle(&id);
    let services = fixture.runtime.services(&id).expect("services").clone();
    assert!(services.ui_overlay_show(first).expect("show"));

    fixture.runtime.dispose(&id).expect("dispose");
    assert!(
        !fixture.runtime.presentation_overlay().borrow().is_active(),
        "unload/crash leaves no stale surface"
    );
    assert_eq!(
        fixture
            .runtime
            .presentation_overlay()
            .borrow()
            .admitted_slots(),
        0,
        "the admitted slot is released"
    );

    let id = fixture.activate("acme.help");
    let second = fixture.handle(&id);
    assert_ne!(first, second, "a reload mints a new generation handle");
    let services = fixture.runtime.services(&id).expect("services").clone();
    let error = services
        .ui_overlay_show(first)
        .expect_err("a previous generation's handle is foreign");
    assert_eq!(error.code, "E_UI_NOT_OWNER");
    assert!(
        services
            .ui_overlay_show(second)
            .expect("new generation shows")
    );
    assert_eq!(
        fixture.runtime.presentation_overlay_session(),
        Some((String::from("acme.help"), second))
    );
}
