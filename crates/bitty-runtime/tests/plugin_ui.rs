//! `bitty.ui.mount` / `bitty.ui.update` host-bridge integration tests (CTX-0428).
//!
//! The first test is the red probe that recorded the pre-implementation state:
//! `bitty.ui` was absent, so the independent statusline/palette packages fell
//! back to command-only mode. The rest cover the accepted gates end to end:
//! grant gating, the overlay slot's `ui.overlay` requirement, the exclusive
//! `tabline` claim, bounded block registries, stale handles, and
//! generation-owned handles across reload.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::ui::{UI_MAX_BLOCKS, UiSlot};
use bitty_lua::{
    BridgeError, E_UI_ALREADY_CAPTURED, E_UI_NOT_OWNER, HostServices, LuaValue, UiNode,
};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, OVERLAY_CAPTURE_TIMEOUT_MS, PluginRuntime, PluginRuntimeConfig, SettingsSource,
    SnapshotSource, UiBlock,
};
use bitty_runtime::{BandEdge, ChromeBands, E_UI_UNAVAILABLE};

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
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
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
    });
    // W-146: disk-backed stores commit through an injected backend.
    common::install_stub_backend(&mut rt);
    rt
}

/// Write a plugin with explicit capability, claim, reserved-command, and
/// `[lazy].events` declarations (CTX-0941 adds the event list).
fn write_plugin_with_events(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    claims: &[&str],
    commands: &[&str],
    events: &[&str],
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
    let commands_toml = commands
        .iter()
        .map(|c| format!("\"{id}:{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let events_toml = events
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
commands = [{commands_toml}]
events = [{events_toml}]
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
        Self::activate_with_commands(tag, id, capabilities, claims, &[], init_src)
    }

    /// Activate a fixture that also reserves `commands` in `[lazy].commands`
    /// and registers them from `init_src` (used by the handle-invalidation
    /// probes that must re-enter the VM after activation).
    fn activate_with_commands(
        tag: &str,
        id: &str,
        capabilities: &[&str],
        claims: &[&str],
        commands: &[&str],
        init_src: &str,
    ) -> Self {
        Self::activate_with_commands_and_events(
            tag,
            id,
            capabilities,
            claims,
            commands,
            &[],
            init_src,
        )
    }

    /// Activate a fixture that additionally declares `events` in
    /// `[lazy].events` and subscribes them from `init_src` (CTX-0941: proves
    /// capture input is observed only through `poll`, never a callback).
    fn activate_with_commands_and_events(
        tag: &str,
        id: &str,
        capabilities: &[&str],
        claims: &[&str],
        commands: &[&str],
        events: &[&str],
        init_src: &str,
    ) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        write_plugin_with_events(&root, id, capabilities, claims, commands, events, init_src);
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
        local ok, err = pcall(bitty.ui.mount, "tabline", { kind = "Text", text = "x" })
        bitty.store.set("ok", ok)
        bitty.store.set("code", ok and "NONE" or err.code)
        return {}
        "#,
    );
    // CTX-0923: the claim gate passes, but `tabline` is reserved for PW-10
    // panel tabs and has no host surface, so the mount fails closed with a
    // typed error instead of being stored and never rendered.
    assert_eq!(
        store_value(&claimed.runtime, &claimed.id, "ok"),
        Some(LuaValue::Bool(false))
    );
    assert_eq!(
        store_value(&claimed.runtime, &claimed.id, "code"),
        Some(LuaValue::String(E_UI_UNAVAILABLE.to_string()))
    );
    claimed
        .runtime
        .services(&claimed.id)
        .expect("services")
        .with_ui_blocks(|blocks| assert!(blocks.is_empty()));
}

/// CTX-0941 supersedes the CTX-0923 "unhosted" assertion: the `overlay` slot
/// is now served by the Core focusable-overlay host, so a granted mount is
/// accepted and retained.
#[test]
fn overlay_slot_with_grant_is_hosted() {
    let fixture = Fixture::activate(
        "overlay-granted",
        "bitty-featured.uioverlay2",
        &["ui.rich", "ui.overlay"],
        &[],
        r#"
        local ok, err = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "x" })
        bitty.store.set("ok", ok)
        bitty.store.set("code", ok and "NONE" or err.code)
        return {}
        "#,
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "ok"),
        Some(LuaValue::Bool(true))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "code"),
        Some(LuaValue::String("NONE".to_string()))
    );
}

#[test]
fn lua_band_slot_mounts_land_in_the_expected_band() {
    // CTX-0923: the statusline plugin's `ui.mount("statusline")` must reach
    // the bottom band, next to plain `bottom` mounts; `top` reaches the top.
    let fixture = Fixture::activate(
        "bands",
        "bitty-featured.uibands",
        &["ui.rich"],
        &[],
        r#"
        bitty.ui.mount("statusline", { kind = "Text", text = "status" })
        bitty.ui.mount("top", { kind = "Text", text = "top" })
        bitty.ui.mount("bottom", { kind = "Text", text = "bottom" })
        bitty.ui.mount("left", { kind = "Text", text = "left" })
        bitty.ui.mount("right", { kind = "Text", text = "right" })
        local ok, err = pcall(bitty.ui.mount, "nowhere", { kind = "Text", text = "x" })
        bitty.store.set("unknown_code", ok and "NONE" or err.code)
        return {}
        "#,
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "unknown_code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    let (bands, unplaced) = ChromeBands::from_mounts(
        fixture
            .runtime
            .ui_blocks()
            .into_iter()
            .map(|(id, slot, node, version)| (id.to_string(), slot, node, version)),
    );
    assert_eq!(unplaced, 0);
    let slots = |edge| {
        bands
            .edge(edge)
            .iter()
            .map(|band| band.slot)
            .collect::<Vec<_>>()
    };
    assert_eq!(slots(BandEdge::Top), [UiSlot::Top]);
    // One plugin, two bottom-edge mounts: mount order is kept.
    assert_eq!(
        slots(BandEdge::Bottom),
        [UiSlot::Statusline, UiSlot::Bottom]
    );
    assert_eq!(slots(BandEdge::Left), [UiSlot::Left]);
    assert_eq!(slots(BandEdge::Right), [UiSlot::Right]);
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
    let mut fixture = Fixture::activate_with_commands(
        "reload",
        "bitty-featured.uireload",
        &["ui.rich"],
        &[],
        &["probe"],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local ok = bitty.ui.update(bitty.store.get("old_handle"), { kind = "Text", text = "v2" })
            bitty.store.set(key, ok)
            return ok
          end,
        })
        local mounted, handle = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "gen" })
        bitty.store.set("handle", mounted and handle or -1)
        -- Remember the first generation's real handle across the reload.
        if bitty.store.get("old_handle") == nil then
          bitty.store.set("old_handle", mounted and handle or -1)
        end
        return {}
        "#,
    );
    let first = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    first.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    // A real generation-1 handle serves updates before the reload.
    assert_eq!(
        fixture
            .runtime
            .dispatch_command(
                &fixture.id,
                "probe",
                &[LuaValue::String("before".to_string())],
            )
            .expect("dispatch"),
        LuaValue::Bool(true)
    );
    let first_handle = match store_value(&fixture.runtime, &fixture.id, "old_handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("mount must return a handle, got {other:?}"),
    };

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
    // The next generation mounts its own block...
    second.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    let second_handle = second
        .with_ui_blocks(|blocks| blocks.iter().next().map(|(handle, _)| handle))
        .expect("second generation retains its own block");
    assert_ne!(
        first_handle, second_handle,
        "generation-owned handles must not alias across reload"
    );
    // ...and the real pre-reload handle is dead, not aliased onto it.
    assert_eq!(
        fixture
            .runtime
            .dispatch_command(
                &fixture.id,
                "probe",
                &[LuaValue::String("after_reload".to_string())],
            )
            .expect("dispatch"),
        LuaValue::Bool(false),
        "a real pre-reload handle must report false after reload, never error"
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "after_reload"),
        Some(LuaValue::Bool(false))
    );
}

/// The same generation's handles must die at suspend and stay dead after a
/// resume until the plugin mounts again (issue #854 acceptance).
#[test]
fn suspend_invalidates_handles_until_remount() {
    let mut fixture = Fixture::activate_with_commands(
        "suspend",
        "bitty-featured.uisuspend",
        &["ui.rich"],
        &[],
        &["probe", "probe_old", "remount"],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local ok = bitty.ui.update(bitty.store.get("handle"), { kind = "Text", text = "v2" })
            bitty.store.set(key, ok)
            return ok
          end,
        })
        bitty.commands.register({
          id = "probe_old",
          title = "Probe old",
          run = function(key)
            local ok = bitty.ui.update(bitty.store.get("old_handle"), { kind = "Text", text = "v2" })
            bitty.store.set(key, ok)
            return ok
          end,
        })
        bitty.commands.register({
          id = "remount",
          title = "Remount",
          run = function()
            local mounted, handle = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "again" })
            if mounted then bitty.store.set("handle", handle) end
            return mounted
          end,
        })
        local mounted, handle = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "gen" })
        bitty.store.set("handle", mounted and handle or -1)
        bitty.store.set("old_handle", mounted and handle or -1)
        return {}
        "#,
    );
    let services = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    let first_handle = match store_value(&fixture.runtime, &fixture.id, "handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("mount must return a handle, got {other:?}"),
    };

    let probe = |fixture: &mut Fixture, command: &str, key: &str| {
        fixture
            .runtime
            .dispatch_command(&fixture.id, command, &[LuaValue::String(key.to_string())])
            .expect("dispatch")
    };

    // update-before-suspend: the live handle serves.
    assert_eq!(probe(&mut fixture, "probe", "before"), LuaValue::Bool(true));

    fixture.runtime.suspend(&fixture.id).expect("suspend");
    services.with_ui_blocks(|blocks| {
        assert!(
            blocks.is_empty(),
            "suspend must clear the generation's host-side block registry"
        )
    });

    // update-after-suspend: the same real handle is dead.
    assert_eq!(
        probe(&mut fixture, "probe", "during"),
        LuaValue::Bool(false)
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "during"),
        Some(LuaValue::Bool(false))
    );

    fixture.runtime.resume(&fixture.id).expect("resume");
    // update-after-resume: still dead until the plugin mounts again.
    assert_eq!(probe(&mut fixture, "probe", "after"), LuaValue::Bool(false));
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "after"),
        Some(LuaValue::Bool(false))
    );

    // Re-mount mints a fresh handle; the pre-suspend handle stays dead.
    assert_eq!(
        fixture
            .runtime
            .dispatch_command(&fixture.id, "remount", &[])
            .expect("dispatch"),
        LuaValue::Bool(true)
    );
    let second_handle = match store_value(&fixture.runtime, &fixture.id, "handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("remount must return a handle, got {other:?}"),
    };
    assert_ne!(
        first_handle, second_handle,
        "a remount after suspend must mint a fresh handle"
    );
    assert_eq!(
        probe(&mut fixture, "probe", "post_remount"),
        LuaValue::Bool(true),
        "the fresh handle must serve"
    );
    assert_eq!(
        probe(&mut fixture, "probe_old", "stale_after_remount"),
        LuaValue::Bool(false),
        "the pre-suspend handle must stay dead after a remount"
    );
    services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
}

/// CTX-0941: the previously unhosted `overlay` slot is served: with a
/// `ui.overlay` grant the mount is retained (not a band), it is not counted as
/// unplaced, and a `ui.rich`-only plugin still fails closed.
#[test]
fn focusable_overlay_slot_is_hosted_and_not_a_band() {
    let fixture = Fixture::activate(
        "overlay-hosted",
        "bitty-featured.uioverlayhost",
        &["ui.rich", "ui.overlay"],
        &[],
        r#"
        local ok, handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "modal" })
        bitty.store.set("ok", ok)
        bitty.store.set("handle", ok and handle or -1)
        return {}
        "#,
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "ok"),
        Some(LuaValue::Bool(true))
    );
    let handle = match store_value(&fixture.runtime, &fixture.id, "handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("overlay mount must return a handle, got {other:?}"),
    };
    fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.get(handle).map(UiBlock::ui_slot),
                Some(UiSlot::Overlay)
            );
        });
    let blocks = fixture.runtime.ui_blocks();
    assert_eq!(blocks.len(), 1);
    let (bands, unplaced) = ChromeBands::from_mounts(
        blocks
            .into_iter()
            .map(|(id, slot, node, version)| (id.to_string(), slot, node, version)),
    );
    assert_eq!(unplaced, 0, "a hosted overlay is not an unplaced block");
    assert!(bands.top.is_empty() && bands.bottom.is_empty());

    let denied = Fixture::activate(
        "overlay-rich-only",
        "bitty-featured.uioverlayrich",
        &["ui.rich"],
        &[],
        r#"
        local ok, err = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "x" })
        bitty.store.set("code", ok and "NONE" or err.code)
        return {}
        "#,
    );
    assert_eq!(
        store_value(&denied.runtime, &denied.id, "code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
}

/// CTX-0941: capture lifecycle across two plugins — single owner, typed
/// already-captured, idempotent release, and non-owner poll.
#[test]
fn overlay_capture_is_single_owner_and_release_is_idempotent() {
    let mut fixture = Fixture::activate_with_commands_and_events(
        "overlay-capture",
        "bitty-featured.uiocapture",
        &["ui.rich", "ui.overlay", "ui.overlay.focus"],
        &[],
        &["probe"],
        &["key"],
        r#"
        local mount_ok, handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "modal" })
        bitty.store.set("handle", mount_ok and handle or -1)
        -- A hot-path callback trap: if Core ever invoked a plugin on the input
        -- path, this handler would fire and bump `callback_count`.
        bitty.events.subscribe("key", function(envelope)
          bitty.store.set("callback_count", (bitty.store.get("callback_count") or 0) + 1)
        end)
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local h = bitty.store.get("handle")
            local a_ok, a_err = pcall(bitty.ui.overlay.acquire, h)
            local a2_ok, a2_err = pcall(bitty.ui.overlay.acquire, h)
            local r1 = bitty.ui.overlay.release(h)
            local r2 = bitty.ui.overlay.release(h)
            local a3_ok = pcall(bitty.ui.overlay.acquire, h)
            bitty.store.set(key .. "_a", a_ok)
            bitty.store.set(key .. "_a2", a2_ok and "NONE" or a2_err.code)
            bitty.store.set(key .. "_r1", r1)
            bitty.store.set(key .. "_r2", r2)
            bitty.store.set(key .. "_a3", a3_ok)
            return a_ok
          end,
        })
        return {}
        "#,
    );
    let handle = match store_value(&fixture.runtime, &fixture.id, "handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("overlay mount must return a handle, got {other:?}"),
    };

    // The runtime-shared manager is a single owner: a second generation is
    // blocked while another plugin holds the capture (typed already-captured)
    // and cannot poll a capture it does not own (typed not-owner).
    let shared = fixture.runtime.overlay_capture().clone();
    let services = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    let other_plugin = PluginId::new("bitty-featured.uiocapture2").expect("valid id");
    shared
        .borrow_mut()
        .acquire(
            other_plugin.as_str(),
            handle,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .expect("first acquire by the other plugin");
    let error = shared
        .borrow_mut()
        .acquire(
            fixture.id.as_str(),
            handle,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .expect_err("the plugin must be blocked by the active owner");
    assert_eq!(error.code, E_UI_ALREADY_CAPTURED);
    let error = services
        .ui_overlay_poll(handle, 4)
        .expect_err("a non-owner poll must fail");
    assert_eq!(error.code, E_UI_NOT_OWNER);
    assert!(shared.borrow_mut().release(other_plugin.as_str(), handle));
    assert!(
        !shared.borrow_mut().release(other_plugin.as_str(), handle),
        "release is idempotent"
    );
    let error = shared
        .borrow_mut()
        .poll(fixture.id.as_str(), handle, 4)
        .expect_err("a released capture must not serve polls");
    assert_eq!(error.code, E_UI_NOT_OWNER);

    // Through Lua: acquire, release twice, acquire again (single generation
    // release/acquire round trip; the cross-owner conflict is proven above).
    assert_eq!(
        fixture
            .runtime
            .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
            .expect("dispatch"),
        LuaValue::Bool(true)
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_a"),
        Some(LuaValue::Bool(true))
    );
    // The second acquire from the same generation fails typed with
    // `E_UI_ALREADY_CAPTURED` (single generation owns at most one capture);
    // the cross-plugin conflict above fails the same way.
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_a2"),
        Some(LuaValue::String(E_UI_ALREADY_CAPTURED.to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_r1"),
        Some(LuaValue::Bool(true))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_r2"),
        Some(LuaValue::Bool(false)),
        "release is idempotent: a repeated release succeeds without effect"
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_a3"),
        Some(LuaValue::Bool(true))
    );

    // No-hot-path guarantee: pushing captured input never runs a plugin
    // callback. The capture is active here (the Lua `probe` ended with a
    // successful acquire), so a real enqueue must occur; only an explicit
    // `deliver_event` may run the subscribed handler.
    assert!(
        fixture.runtime.overlay_capture().borrow().is_active(),
        "capture must be active for the no-hot-path probe"
    );
    assert!(
        fixture.runtime.push_overlay_input("key", "k"),
        "captured input must enqueue"
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "callback_count"),
        None,
        "capture must not fire a plugin callback"
    );
    fixture
        .runtime
        .deliver_event("key", &LuaValue::String("k".to_string()));
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "callback_count"),
        Some(LuaValue::Integer(1)),
        "the explicit subscription path still works (proving the trap is live)"
    );

    // Timeout is bounded: a capture that reaches its deadline is revoked by
    // the Core tick, so a blocked or silent plugin cannot pin input. The
    // `probe` left this generation holding capture (`x_a3`); force its live
    // deadline into the past and prove the tick revokes it.
    let expired = fixture.runtime.overlay_capture().clone();
    assert!(
        expired.borrow().is_active(),
        "the Lua acquire holds capture"
    );
    assert!(
        expired
            .borrow_mut()
            .force_expire(fixture.id.as_str(), handle),
        "the live capture must be rewound"
    );
    assert!(
        fixture.runtime.expire_overlay_captures(),
        "a capture past its deadline must be revoked"
    );
    assert!(!expired.borrow().is_active());
}

/// CTX-0941: a failed activation (`rollback`) revokes a capture acquired
/// during `init.lua`, so a crashed/failed generation can never pin input.
#[test]
fn overlay_capture_is_revoked_when_activation_rolls_back() {
    let root = temp_dir("overlay-rollback-root");
    let data = temp_dir("overlay-rollback-data");
    let id = "bitty-featured.uiorollback";
    // Acquire the capture, then throw: the activation must roll back and the
    // capture must not survive the failure.
    write_plugin_with_events(
        &root,
        id,
        &["ui.rich", "ui.overlay", "ui.overlay.focus"],
        &[],
        &[],
        &[],
        r#"
        local _, h = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "modal" })
        pcall(bitty.ui.overlay.acquire, h)
        error("activation fails after acquiring the capture")
        "#,
    );
    let mut rt = runtime(vec![root.clone()], data.clone());
    rt.discover();
    let pid = plugin_id(id);
    let _ = rt.activate(&pid);
    assert!(
        matches!(rt.state(&pid), Some(LifecycleState::Failed(_))),
        "activation must fail"
    );
    let capture = rt.overlay_capture().clone();
    assert!(
        !capture.borrow().is_active(),
        "rollback must revoke a capture acquired before the failure"
    );
    assert!(
        !rt.push_overlay_input("key", "k"),
        "a failed generation must not keep capturing input"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

/// CTX-0941: suspend and dispose revoke a live capture (unload/crash are
/// release), and input never reaches a plugin callback on the hot path.
#[test]
fn overlay_capture_is_revoked_on_suspend_and_dispose() {
    let mut fixture = Fixture::activate(
        "overlay-revoke",
        "bitty-featured.uiorevoke",
        &["ui.rich", "ui.overlay"],
        &[],
        r#"
        local ok, h = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "modal" })
        bitty.store.set("handle", ok and h or -1)
        return {}
        "#,
    );
    let handle = match store_value(&fixture.runtime, &fixture.id, "handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("overlay mount must return a handle, got {other:?}"),
    };
    let shared = fixture.runtime.overlay_capture().clone();
    shared
        .borrow_mut()
        .acquire(
            fixture.id.as_str(),
            handle,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .expect("acquire");
    // The input path only enqueues; it never invokes plugin code.
    assert!(fixture.runtime.push_overlay_input("text", "hello"));
    assert_eq!(shared.borrow().queued_len(), 1);

    fixture.runtime.suspend(&fixture.id).expect("suspend");
    assert!(!shared.borrow().is_active(), "suspend must revoke capture");
    assert_eq!(shared.borrow().queued_len(), 0, "revoke clears the queue");
    // A suspended generation's input is no longer captured.
    assert!(!fixture.runtime.push_overlay_input("text", "after"));

    fixture.runtime.resume(&fixture.id).expect("resume");
    shared
        .borrow_mut()
        .acquire(
            fixture.id.as_str(),
            handle,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .expect("re-acquire after resume");
    fixture.runtime.dispose(&fixture.id).expect("dispose");
    assert!(!shared.borrow().is_active(), "dispose must revoke capture");
}

/// CTX-0941: the host-side capture timeout bound is finite and explicit.
#[test]
fn overlay_capture_timeout_bound_is_finite() {
    // The production timeout is a bounded transient session, not an unbounded
    // hold: it must be positive and inside a reviewed ceiling.
    const { assert!(OVERLAY_CAPTURE_TIMEOUT_MS > 0) };
    const { assert!(OVERLAY_CAPTURE_TIMEOUT_MS <= 300_000) };
}
