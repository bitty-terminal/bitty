//! `bitty.ui.mount` / `bitty.ui.update` host-bridge integration tests (CTX-0428).
//!
//! The first test is the red probe that recorded the pre-implementation state:
//! `bitty.ui` was absent, so the independent statusline/palette packages fell
//! back to command-only mode. The rest cover the accepted gates end to end:
//! grant gating, the overlay slot's `ui.overlay` requirement, the exclusive
//! `tabline` claim, bounded block registries, stale handles, and
//! generation-owned handles across reload.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::ui::UI_MAX_BLOCKS;
use bitty_lua::{BridgeError, LuaValue, UiNode};
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

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-plugin-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(roots: Vec<PathBuf>, data_dir: PathBuf) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))),
    })
}

/// Write a plugin with explicit capability and claim declarations.
fn write_plugin(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    claims: &[&str],
    init_src: &str,
) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let caps_toml = capabilities
        .iter()
        .map(|c| format!("{c} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    let claims_toml = claims
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "UI Test"
version = "0.1.0"
description = "ui bridge test"

[compat]
plugin-api = "^1.0"

[capabilities]
{caps_toml}

[lazy]
commands = []
events = []
claims = [{claims_toml}]
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
    plugin
}

fn plugin_id(id: &str) -> PluginId {
    PluginId::new(id).expect("valid id")
}

fn store_value(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<LuaValue> {
    rt.services(id)
        .expect("services")
        .with_store(|store| store.get(key))
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    id: PluginId,
}

impl Fixture {
    fn activate(
        tag: &str,
        id: &str,
        capabilities: &[&str],
        claims: &[&str],
        init_src: &str,
    ) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        write_plugin(&root, id, capabilities, claims, init_src);
        let mut runtime = runtime(vec![root.clone()], data.clone());
        runtime.discover();
        let id = plugin_id(id);
        let report = runtime.activate(&id).expect("activate");
        assert_eq!(report.state, LifecycleState::Active);
        Self {
            root,
            data,
            runtime,
            id,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// RED PROBE 2: before CTX-0428 this failed with `type error, expected table,
/// found nil` because `bitty.ui` was absent on a full activation.
#[test]
fn probe_plugin_mounts_statusline_block() {
    let fixture = Fixture::activate(
        "probe",
        "bitty-featured.uiprobe",
        &["ui.rich"],
        &[],
        r#"
        local ok, handle = pcall(bitty.ui.mount, "statusline", {
          kind = "Row",
          children = { { kind = "Text", text = "cwd:/tmp" } },
        })
        bitty.store.set("ui_ok", ok)
        if ok then
          bitty.store.set("handle", handle)
          bitty.store.set("updated", bitty.ui.update(handle, { kind = "Text", text = "v2" }))
          bitty.store.set("stale", bitty.ui.update(handle + 1000, { kind = "Text", text = "x" }))
        end
        return {}
        "#,
    );

    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "ui_ok"),
        Some(LuaValue::Bool(true)),
        "bitty.ui.mount must be present and accepted for a ui.rich grant"
    );
    let handle = match store_value(&fixture.runtime, &fixture.id, "handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("mount must return a positive handle, got {other:?}"),
    };
    assert!(handle > 0);
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "updated"),
        Some(LuaValue::Bool(true)),
        "ui.update on a live handle must return true"
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "stale"),
        Some(LuaValue::Bool(false)),
        "ui.update on a stale handle must return false"
    );

    fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .with_ui_blocks(|blocks| {
            let block = blocks.get(handle).expect("block retained");
            assert_eq!(block.slot(), "statusline");
            assert_eq!(block.version(), 2, "one mount plus one served update");
            assert_eq!(block.node(), &UiNode::text("v2"));
        });
}

#[test]
fn mount_denied_without_ui_rich_is_typed() {
    let fixture = Fixture::activate(
        "denied",
        "bitty-featured.uidenied",
        &[],
        &[],
        r#"
        local mount_ok, mount_err = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "x" })
        local update_ok, update_err = pcall(bitty.ui.update, 1, { kind = "Text", text = "x" })
        bitty.store.set("mount_code", mount_ok and "NONE" or mount_err.code)
        bitty.store.set("update_code", update_ok and "NONE" or update_err.code)
        return {}
        "#,
    );

    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "mount_code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "update_code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .with_ui_blocks(|blocks| assert!(blocks.is_empty()));
}

#[test]
fn overlay_slot_requires_ui_overlay_grant() {
    let fixture = Fixture::activate(
        "overlay",
        "bitty-featured.uioverlay",
        &["ui.rich"],
        &[],
        r#"
        local overlay_ok, overlay_err = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "x" })
        local status_ok, status = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "ok" })
        bitty.store.set("overlay_code", overlay_ok and "NONE" or overlay_err.code)
        bitty.store.set("status_ok", status_ok)
        bitty.store.set("status_handle", status_ok and status or -1)
        return {}
        "#,
    );

    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "overlay_code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "status_ok"),
        Some(LuaValue::Bool(true))
    );
    fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
}

#[test]
fn tabline_slot_requires_exclusive_claim() {
    let denied = Fixture::activate(
        "tab-unclaimed",
        "bitty-featured.uitab1",
        &["ui.rich"],
        &[],
        r#"
        local ok, err = pcall(bitty.ui.mount, "tabline", { kind = "Text", text = "x" })
        bitty.store.set("code", ok and "NONE" or err.code)
        return {}
        "#,
    );
    assert_eq!(
        store_value(&denied.runtime, &denied.id, "code"),
        Some(LuaValue::String("E_UI_CLAIM_REQUIRED".to_string()))
    );

    let claimed = Fixture::activate(
        "tab-claimed",
        "bitty-featured.uitab2",
        &["ui.rich"],
        &["tabline"],
        r#"
        local ok, handle = pcall(bitty.ui.mount, "tabline", { kind = "Text", text = "x" })
        bitty.store.set("ok", ok)
        bitty.store.set("handle", ok and handle or -1)
        return {}
        "#,
    );
    assert_eq!(
        store_value(&claimed.runtime, &claimed.id, "ok"),
        Some(LuaValue::Bool(true))
    );
}

#[test]
fn mount_loop_hits_block_budget_fail_closed() {
    let fixture = Fixture::activate(
        "budget",
        "bitty-featured.uibudget",
        &["ui.rich"],
        &[],
        r#"
        local mounted = 0
        local last_code = "NONE"
        for _ = 1, 65 do
          local ok, err = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "x" })
          if ok then
            mounted = mounted + 1
          else
            last_code = err.code
          end
        end
        bitty.store.set("mounted", mounted)
        bitty.store.set("last_code", last_code)
        return {}
        "#,
    );

    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "mounted"),
        Some(LuaValue::Integer(UI_MAX_BLOCKS as i64))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "last_code"),
        Some(LuaValue::String("E_UI_BLOCK_BUDGET".to_string()))
    );
    fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .with_ui_blocks(|blocks| assert_eq!(blocks.len(), UI_MAX_BLOCKS));
}

#[test]
fn oversized_component_is_rejected_before_any_mount() {
    let fixture = Fixture::activate(
        "oversize",
        "bitty-featured.uiover",
        &["ui.rich"],
        &[],
        r#"
        -- 44 KiB linear chunk appended six times: over SCN-3, inside RC-1.
        local unit_parts = {}
        for index = 1, 1024 do unit_parts[index] = "x" end
        local unit = table.concat(unit_parts)
        local chunk_units = {}
        for index = 1, 44 do chunk_units[index] = unit end
        local chunk = table.concat(chunk_units)
        local text = chunk
        for _ = 1, 5 do text = text .. chunk end
        local ok, err = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = text })
        bitty.store.set("ok", ok)
        bitty.store.set("code", ok and "NONE" or err.code)
        return {}
        "#,
    );

    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "ok"),
        Some(LuaValue::Bool(false))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .with_ui_blocks(|blocks| assert!(blocks.is_empty()));
}

#[test]
fn reload_starts_the_next_generation_with_a_fresh_registry() {
    let mut fixture = Fixture::activate(
        "reload",
        "bitty-featured.uireload",
        &["ui.rich"],
        &[],
        r#"
        local ok, handle = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "gen" })
        bitty.store.set("stale", ok and bitty.ui.update(handle + 1000, { kind = "Text", text = "x" }))
        return {}
        "#,
    );
    let first = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    first.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));

    fixture
        .runtime
        .reload(&fixture.id)
        .expect("reload must succeed for a local package");
    let second = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    assert!(
        !Rc::ptr_eq(&first, &second),
        "reload must build a new generation-owned services instance"
    );
    second.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "stale"),
        Some(LuaValue::Bool(false)),
        "stale handles report false, never error"
    );
}
