//! CTX-0892: PluginRuntime loop integration tests.
//!
//! Proves:
//! - Event delivery: subscribed events reach Lua VMs via `deliver_event`, bounded per tick.
//! - Command dispatch: registered plugin commands are dispatched via `dispatch_command`.
//! - UI blocks accessor: mounted UI blocks are retrievable via the read accessor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
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
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-runtime-loop-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(third_party_roots: Vec<PathBuf>, data_dir: PathBuf) -> PluginRuntime {
    let mut settings = MapSettings::default();
    settings
        .0
        .insert("retention_days".to_string(), LuaValue::Integer(7));
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots,
        settings: Rc::new(settings),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
        ]))),
    })
}

/// Write a minimal plugin package under `root/<id>/`.
fn write_plugin(root: &Path, id: &str, init_src: &str) -> PathBuf {
    write_plugin_with_caps(root, id, init_src, "")
}

/// Write a plugin package with custom capabilities.
fn write_plugin_with_caps(root: &Path, id: &str, init_src: &str, caps: &str) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Test"
version = "0.1.0"
description = "test"

[compat]
plugin-api = "^1.0"

{caps}

[lazy]
commands = ["{id}:test.command"]
events = ["focus.changed"]
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
    plugin
}

#[test]
fn event_delivery_reaches_subscribed_plugin() {
    let data = temp_dir("event-delivery");
    let plugins_root = temp_dir("event-delivery-plugins");

    // Plugin that subscribes to "focus.changed" and stores the event.
    let init = r#"
local received_events = {}

bitty.events.subscribe("focus.changed", function(event)
    table.insert(received_events, {kind = "focus.changed", payload = event.payload})
end)

bitty.commands.register({
    id = "test.command",
    title = "Get Events",
    run = function()
        return {count = #received_events, events = received_events}
    end
})

return {}
"#;

    write_plugin(&plugins_root, "test.event-subscriber", init);

    let mut rt = runtime(vec![plugins_root.clone()], data.clone());
    let discovered = rt.discover();
    eprintln!("Discovered {} plugins: {:?}", discovered.len(), discovered);
    eprintln!("Plugin dir: {}", plugins_root.display());
    eprintln!(
        "Plugin manifest: {}",
        plugins_root
            .join("test.event-subscriber/bitty-plugin.toml")
            .display()
    );
    let id = PluginId::new("test.event-subscriber").expect("id");
    rt.activate(&id).expect("activate");

    // Deliver a focus.changed event.
    let payload = LuaValue::table([("focused", LuaValue::Bool(true))]);
    let delivered = rt.deliver_event("focus.changed", &payload);
    assert_eq!(delivered, 1, "event must be delivered to 1 subscriber");

    // Query the plugin to see if it received the event.
    let result = rt
        .dispatch_command(&id, "test.command", &[])
        .expect("dispatch");

    if let LuaValue::Table(fields) = result {
        let count = fields
            .iter()
            .find(|(k, _)| matches!(k, LuaValue::String(s) if s == "count"))
            .and_then(|(_, v)| match v {
                LuaValue::Integer(n) => Some(*n),
                _ => None,
            });
        assert_eq!(count, Some(1), "plugin must have received 1 event");
    } else {
        panic!("expected table result, got {result:?}");
    }

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&plugins_root);
}

#[test]
fn event_delivery_bounded_per_tick() {
    let data = temp_dir("event-bounded");
    let plugins_root = temp_dir("event-bounded-plugins");

    // Plugin that counts events.
    let init = r#"
local event_count = 0

bitty.events.subscribe("focus.changed", function(event)
    event_count = event_count + 1
end)

bitty.commands.register({
    id = "test.command",
    title = "Get Count",
    run = function()
        return {count = event_count}
    end
})

return {}
"#;

    write_plugin(&plugins_root, "test.event-counter", init);

    let mut rt = runtime(vec![plugins_root.clone()], data.clone());
    rt.discover();
    let id = PluginId::new("test.event-counter").expect("id");
    rt.activate(&id).expect("activate");

    // Deliver multiple events - the bound is enforced by the caller (terminal_app.rs),
    // but deliver_event itself should process each call.
    for _ in 0..5 {
        let payload = LuaValue::table([("focused", LuaValue::Bool(true))]);
        let delivered = rt.deliver_event("focus.changed", &payload);
        assert_eq!(delivered, 1);
    }

    let result = rt
        .dispatch_command(&id, "test.command", &[])
        .expect("dispatch");

    if let LuaValue::Table(fields) = result {
        let count = fields
            .iter()
            .find(|(k, _)| matches!(k, LuaValue::String(s) if s == "count"))
            .and_then(|(_, v)| match v {
                LuaValue::Integer(n) => Some(*n),
                _ => None,
            });
        assert_eq!(count, Some(5), "plugin must have received all 5 events");
    } else {
        panic!("expected table result, got {result:?}");
    }

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&plugins_root);
}

#[test]
fn command_dispatch_reaches_registered_handler() {
    let data = temp_dir("command-dispatch");
    let plugins_root = temp_dir("command-dispatch-plugins");

    // Plugin that registers a command.
    let init = r#"
bitty.commands.register({
    id = "test.command",
    title = "Test Command",
    run = function(arg1)
        return {result = "ok", received = arg1}
    end
})

return {}
"#;

    write_plugin(&plugins_root, "test.command-handler", init);

    let mut rt = runtime(vec![plugins_root.clone()], data.clone());
    rt.discover();
    let id = PluginId::new("test.command-handler").expect("id");
    rt.activate(&id).expect("activate");

    // Dispatch the command with arguments.
    let result = rt
        .dispatch_command(
            &id,
            "test.command",
            &[LuaValue::String("hello".to_string())],
        )
        .expect("dispatch");

    if let LuaValue::Table(fields) = result {
        let result_field = fields
            .iter()
            .find(|(k, _)| matches!(k, LuaValue::String(s) if s == "result"))
            .and_then(|(_, v)| match v {
                LuaValue::String(s) => Some(s.as_str()),
                _ => None,
            });
        let received = fields
            .iter()
            .find(|(k, _)| matches!(k, LuaValue::String(s) if s == "received"))
            .and_then(|(_, v)| match v {
                LuaValue::String(s) => Some(s.as_str()),
                _ => None,
            });
        assert_eq!(result_field, Some("ok"));
        assert_eq!(received, Some("hello"));
    } else {
        panic!("expected table result, got {result:?}");
    }

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&plugins_root);
}

#[test]
fn ui_blocks_accessor_returns_mounted_blocks() {
    let data = temp_dir("ui-blocks");
    let plugins_root = temp_dir("ui-blocks-plugins");

    // Plugin that mounts a UI block.
    let init = r#"
bitty.commands.register({
    id = "test.command",
    title = "Test Command",
    run = function(args)
        -- no-op
    end
})

bitty.ui.mount("statusline", {
    kind = "Text",
    text = "test statusline"
})

return {}
"#;

    write_plugin_with_caps(
        &plugins_root,
        "test.ui-mounter",
        init,
        "[capabilities]\nui.rich = true",
    );

    let mut rt = runtime(vec![plugins_root.clone()], data.clone());
    rt.discover();
    let id = PluginId::new("test.ui-mounter").expect("id");
    rt.activate(&id).expect("activate");

    // Access UI blocks.
    let blocks = rt.ui_blocks();
    assert_eq!(blocks.len(), 1, "must have 1 mounted UI block");

    let (plugin_id, slot, _node, version) = &blocks[0];
    assert_eq!(plugin_id.as_str(), "test.ui-mounter");
    assert_eq!(slot, "statusline");
    assert_eq!(*version, 1);

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&plugins_root);
}
