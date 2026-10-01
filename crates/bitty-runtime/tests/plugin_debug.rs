//! `bitty.debug` read-only backend over real plugin VMs (CTX-0897, #1568).
//!
//! Plugins are activated by a real [`PluginRuntime`] from temp-dir packages
//! whose manifests declare (and therefore grant) `debug.inspect` and/or
//! `debug.trace`. Each probe runs inside a registered command so the calls
//! go through the real Lua bridge, grant snapshot, and runtime-owned
//! `DebugView`/`TraceHub`. Headless and Windows-safe (temp dirs only).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
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

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-debug-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(NoSnapshot),
    })
}

/// Write a plugin package under `root/<id>/` with the given capability
/// lines, reserved commands, declared events, and `init.lua` source.
fn write_plugin(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    events: &[&str],
    init_src: &str,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let list = |items: &[&str]| {
        items
            .iter()
            .map(|item| format!("\"{item}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let capabilities_toml = capabilities
        .iter()
        .map(|capability| format!("{capability} = true\n"))
        .collect::<String>();
    let qualified: Vec<String> = commands
        .iter()
        .map(|command| format!("{id}:{command}"))
        .collect();
    let qualified: Vec<&str> = qualified.iter().map(String::as_str).collect();
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Test"
version = "0.3.1"
description = "test"

[compat]
plugin-api = "^1.0"

[capabilities]
{capabilities_toml}
[lazy]
commands = [{commands}]
events = [{events}]
"#,
            commands = list(&qualified),
            events = list(events),
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

fn pid(id: &str) -> PluginId {
    PluginId::new(id).expect("id")
}

fn string(value: LuaValue) -> String {
    match value {
        LuaValue::String(s) => s,
        other => panic!("expected a string result, got {other:?}"),
    }
}

/// Inspector: serializes `inspect(target)` rows to `a|b|c;...` strings.
const INSPECTOR: &str = r#"
local function rows(target, fields)
  local ok, result = pcall(bitty.debug.inspect, target)
  if not ok then return "ERR:" .. result.code end
  local out = {}
  for _, item in ipairs(result.items) do
    if type(item) == "string" then
      out[#out + 1] = item
    else
      local parts = {}
      for _, field in ipairs(fields) do parts[#parts + 1] = tostring(item[field]) end
      out[#out + 1] = table.concat(parts, "|")
    end
  end
  return result.target .. "=" .. table.concat(out, ";") .. ":" .. tostring(result.truncated)
end

bitty.commands.register({
  id = "plugins",
  title = "Inspect plugins",
  run = function() return rows("plugins", { "id", "version", "state", "generation" }) end,
})
bitty.commands.register({
  id = "commands",
  title = "Inspect commands",
  run = function() return rows("commands", { "plugin", "id", "title" }) end,
})
bitty.commands.register({
  id = "events",
  title = "Inspect events",
  run = function() return rows("events", { "plugin", "kind" }) end,
})
bitty.commands.register({
  id = "grants",
  title = "Inspect grants",
  run = function() return rows("grants", {}) end,
})
bitty.commands.register({
  id = "trace",
  title = "Trace without grant",
  run = function()
    local ok, err = pcall(bitty.debug.trace, {})
    if ok then return "OK" end
    return "ERR:" .. err.code
  end,
})
"#;

/// Tracer: opens a trace on `start`, drains on `drain`.
const TRACER: &str = r##"
local handle = nil

bitty.commands.register({
  id = "start",
  title = "Start trace",
  run = function()
    handle = bitty.debug.trace({ filter = "terminal.*", max_events = 2 })
    return tostring(handle)
  end,
})
bitty.commands.register({
  id = "drain",
  title = "Drain trace",
  run = function()
    local result = bitty.debug.trace_get(handle)
    if result == nil then return "NIL" end
    local out = {}
    for _, record in ipairs(result.records) do
      local payload = record.payload
      local detail = payload.truncated and ("truncated:" .. payload.bytes)
        or tostring(payload.terminal_id)
      out[#out + 1] = record.topic .. "#" .. record.sequence .. "@" .. detail
        .. (type(record.timestamp) == "number" and "" or "!ts")
    end
    return table.concat(out, ";") .. "|dropped=" .. result.dropped
  end,
})
bitty.commands.register({
  id = "probe",
  title = "Probe foreign handle",
  run = function(args)
    local result = bitty.debug.trace_get(args)
    return result == nil and "NIL" or "LEAK"
  end,
})
bitty.commands.register({
  id = "inspect",
  title = "Inspect without grant",
  run = function()
    local ok, err = pcall(bitty.debug.inspect, "plugins")
    if ok then return "OK" end
    return "ERR:" .. err.code
  end,
})
bitty.commands.register({
  id = "badopts",
  title = "Non-table opts",
  run = function()
    local ok, err = pcall(bitty.debug.trace, "terminal.*")
    if ok then return "OK" end
    return "ERR:" .. err.code
  end,
})
"##;

const PLAIN: &str = r#"
bitty.commands.register({
  id = "inspect",
  title = "Plain inspect",
  run = function()
    local ok, err = pcall(bitty.debug.inspect, "plugins")
    if ok then return "OK" end
    return "ERR:" .. err.code
  end,
})
bitty.commands.register({
  id = "trace",
  title = "Plain trace",
  run = function()
    local ok, err = pcall(bitty.debug.trace, nil)
    if ok then return "OK" end
    return "ERR:" .. err.code
  end,
})
bitty.commands.register({
  id = "get",
  title = "Plain trace_get",
  run = function()
    local ok, err = pcall(bitty.debug.trace_get, 1)
    if ok then return "OK" end
    return "ERR:" .. err.code
  end,
})
bitty.events.subscribe("terminal.opened", function() end)
"#;

/// The tracer's manifest `lazy.events`: the only kinds its traces may
/// record. It subscribes to none of them, so recording does not depend on a
/// handler. `terminal.bell` and `focus.changed` are deliberately absent.
const TRACER_EVENTS: &[&str] = &[
    "terminal.opened",
    "terminal.closed",
    "terminal.title-changed",
];

const INSPECTOR_ID: &str = "xuepoo.inspector";
const TRACER_ID: &str = "xuepoo.tracer";
const PLAIN_ID: &str = "xuepoo.plain";

fn setup(tag: &str) -> (PluginRuntime, PathBuf) {
    let root = temp_dir(tag);
    write_plugin(
        &root,
        INSPECTOR_ID,
        &["debug.inspect", "platform.notify"],
        &["plugins", "commands", "events", "grants", "trace"],
        &[],
        INSPECTOR,
    );
    write_plugin(
        &root,
        TRACER_ID,
        &["debug.trace"],
        &["start", "drain", "probe", "inspect", "badopts"],
        TRACER_EVENTS,
        TRACER,
    );
    write_plugin(
        &root,
        PLAIN_ID,
        &[],
        &["inspect", "trace", "get"],
        &["terminal.opened"],
        PLAIN,
    );
    let mut rt = runtime(root.clone());
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 3, "{discovered:?}");
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    (rt, root)
}

fn run(rt: &mut PluginRuntime, id: &str, command: &str) -> String {
    string(
        rt.dispatch_command(&pid(id), command, &[])
            .unwrap_or_else(|error| panic!("{id}:{command}: {error}")),
    )
}

#[test]
fn inspect_sees_plugins_commands_events_and_own_grants() {
    let (mut rt, root) = setup("inspect");

    assert_eq!(
        run(&mut rt, INSPECTOR_ID, "plugins"),
        "plugins=xuepoo.inspector|0.3.1|active|1;\
         xuepoo.plain|0.3.1|active|1;\
         xuepoo.tracer|0.3.1|active|1:false"
    );

    let commands = run(&mut rt, INSPECTOR_ID, "commands");
    assert!(commands.starts_with("commands="), "{commands}");
    assert!(
        commands.contains("xuepoo.inspector|plugins|Inspect plugins"),
        "{commands}"
    );
    assert!(
        commands.contains("xuepoo.tracer|start|Start trace"),
        "{commands}"
    );
    assert!(
        commands.contains("xuepoo.plain|get|Plain trace_get"),
        "{commands}"
    );

    assert_eq!(
        run(&mut rt, INSPECTOR_ID, "events"),
        "events=xuepoo.plain|terminal.opened:false"
    );

    // Own grants only: never the tracer's `debug.trace`.
    assert_eq!(
        run(&mut rt, INSPECTOR_ID, "grants"),
        "grants=debug.inspect;platform.notify:false"
    );

    // `debug.inspect` does not imply `debug.trace`.
    assert_eq!(
        run(&mut rt, INSPECTOR_ID, "trace"),
        "ERR:E_CAPABILITY_DENIED"
    );

    // Lifecycle changes are reflected; a disposed plugin's rows vanish.
    rt.suspend(&pid(TRACER_ID)).expect("suspend");
    let plugins = run(&mut rt, INSPECTOR_ID, "plugins");
    assert!(
        plugins.contains("xuepoo.tracer|0.3.1|suspended|1"),
        "{plugins}"
    );
    rt.resume(&pid(TRACER_ID)).expect("resume");
    rt.dispose(&pid(PLAIN_ID)).expect("dispose");
    let plugins = run(&mut rt, INSPECTOR_ID, "plugins");
    assert!(
        plugins.contains("xuepoo.plain|0.3.1|disposed|1"),
        "{plugins}"
    );
    assert_eq!(run(&mut rt, INSPECTOR_ID, "events"), "events=:false");
    let commands = run(&mut rt, INSPECTOR_ID, "commands");
    assert!(!commands.contains("xuepoo.plain"), "{commands}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trace_records_delivered_events_and_drops_on_dispose() {
    let (mut rt, root) = setup("trace");

    assert_eq!(run(&mut rt, TRACER_ID, "start"), "1");
    // `debug.trace` does not imply `debug.inspect`.
    assert_eq!(
        run(&mut rt, TRACER_ID, "inspect"),
        "ERR:E_CAPABILITY_DENIED"
    );
    assert_eq!(run(&mut rt, TRACER_ID, "badopts"), "ERR:E_DEF_INVALID");

    // Recorded once per event, before fan-out, whether or not the tracer
    // itself subscribes. Least privilege: only kinds the tracer declares in
    // `lazy.events` are recorded. `terminal.bell` matches the `terminal.*`
    // filter but is undeclared, so it is skipped even though the plain
    // plugin is subscribed to other events; `focus.changed` is both
    // undeclared and filtered out.
    let payload = |id: i64| LuaValue::table([("terminal_id", LuaValue::Integer(id))]);
    assert_eq!(rt.deliver_event("terminal.opened", &payload(1)), 1);
    rt.deliver_event("focus.changed", &payload(2));
    rt.deliver_event("terminal.bell", &payload(3));
    rt.deliver_event("terminal.closed", &payload(4));
    assert_eq!(
        run(&mut rt, TRACER_ID, "drain"),
        "terminal.opened#1@1;terminal.closed#4@4|dropped=0"
    );

    // Ring of 2: drop-oldest with a reported-then-reset counter, and an
    // oversized payload is replaced by a size marker.
    rt.deliver_event("terminal.opened", &payload(5));
    rt.deliver_event("terminal.opened", &payload(6));
    let big = LuaValue::String("x".repeat(8192));
    rt.deliver_event("terminal.title-changed", &big);
    assert_eq!(
        run(&mut rt, TRACER_ID, "drain"),
        "terminal.opened#6@6;terminal.title-changed#7@truncated:8194|dropped=1"
    );
    assert_eq!(run(&mut rt, TRACER_ID, "drain"), "|dropped=0");

    // Suspended owners record nothing; resume continues the same trace.
    rt.suspend(&pid(TRACER_ID)).expect("suspend");
    rt.deliver_event("terminal.opened", &payload(8));
    rt.resume(&pid(TRACER_ID)).expect("resume");
    rt.deliver_event("terminal.opened", &payload(9));
    assert_eq!(
        run(&mut rt, TRACER_ID, "drain"),
        "terminal.opened#9@9|dropped=0"
    );

    // Dispose drops the traces; a re-activated generation cannot read the
    // old handle (`nil`), and the new trace gets a fresh handle.
    rt.dispose(&pid(TRACER_ID)).expect("dispose");
    rt.deliver_event("terminal.opened", &payload(10));
    rt.activate(&pid(TRACER_ID)).expect("reactivate");
    let old = rt
        .dispatch_command(&pid(TRACER_ID), "probe", &[LuaValue::Integer(1)])
        .expect("probe");
    assert_eq!(old, LuaValue::String("NIL".to_string()));
    assert_eq!(run(&mut rt, TRACER_ID, "start"), "2");
    assert_eq!(run(&mut rt, TRACER_ID, "drain"), "|dropped=0");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trace_handles_are_isolated_between_plugins() {
    let root = temp_dir("isolation");
    let second = "xuepoo.tracer-two";
    write_plugin(
        &root,
        TRACER_ID,
        &["debug.trace"],
        &["start", "drain", "probe", "inspect", "badopts"],
        TRACER_EVENTS,
        TRACER,
    );
    write_plugin(
        &root,
        second,
        &["debug.trace"],
        &["start", "drain", "probe", "inspect", "badopts"],
        TRACER_EVENTS,
        TRACER,
    );
    let mut rt = runtime(root.clone());
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(run(&mut rt, TRACER_ID, "start"), "1");
    rt.deliver_event("terminal.opened", &LuaValue::Table(Vec::new()));
    let probe = rt
        .dispatch_command(&pid(second), "probe", &[LuaValue::Integer(1)])
        .expect("probe");
    assert_eq!(probe, LuaValue::String("NIL".to_string()));
    // The foreign probe consumed nothing.
    assert_eq!(
        run(&mut rt, TRACER_ID, "drain"),
        "terminal.opened#1@nil|dropped=0"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ungranted_plugin_is_denied_every_debug_read() {
    let (mut rt, root) = setup("denied");
    assert_eq!(run(&mut rt, PLAIN_ID, "inspect"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, PLAIN_ID, "trace"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, PLAIN_ID, "get"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(rt.state(&pid(PLAIN_ID)), Some(&LifecycleState::Active));
    let _ = std::fs::remove_dir_all(&root);
}
