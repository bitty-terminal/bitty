//! Service conformance: `bitty.services.get`/`provide` against the live
//! runtime directory (issue #1379, LUA-OQ-8).
//!
//! Two-plugin activation over real VMs: the provider publishes a
//! schema-typed interface, the consumer resolves by version requirement,
//! calls across VMs, and fails closed (`E_SERVICE_RESOLUTION`,
//! `E_SERVICE_GONE`, `E_SERVICE_INVALID`, `E_SERVICE_UNDECLARED`) through
//! suspend/resume/dispose and declaration violations. Headless and
//! Windows-safe (temp dirs only, no processes or sockets).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::LuaValue;
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use std::collections::BTreeMap;

use bitty_lua::BridgeError;

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
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
        Ok(LuaValue::table([("version", LuaValue::Integer(1))]))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-services-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(third_party_roots: Vec<PathBuf>, data_dir: PathBuf) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots,
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    })
}

/// Write a plugin package with service sections under `root/<id>/`.
fn write_service_plugin(
    root: &Path,
    id: &str,
    services_toml: &str,
    commands: &[&str],
    init_src: &str,
) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let commands_toml = commands
        .iter()
        .map(|command| format!("\"{command}\""))
        .collect::<Vec<_>>()
        .join(", ");
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

{services_toml}
[lazy]
commands = [{commands_toml}]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
    plugin
}

const CALC_ARGS_SCHEMA: &str = "{\"type\": \"object\", \"properties\": {\"a\": {\"type\": \"integer\"}, \"b\": {\"type\": \"integer\"}}, \"required\": [\"a\", \"b\"], \"additionalProperties\": false}";
const CALC_RESULT_SCHEMA: &str = "{\"type\": \"object\", \"properties\": {\"sum\": {\"type\": \"integer\"}}, \"required\": [\"sum\"], \"additionalProperties\": false}";

fn calc_manifest() -> String {
    format!(
        "[services.provided]\n\"calc.add\" = {{ version = \"1.0.0\", args_schema = \"{args}\", result_schema = \"{result}\" }}\n",
        args = CALC_ARGS_SCHEMA.replace('"', "\\\""),
        result = CALC_RESULT_SCHEMA.replace('"', "\\\""),
    )
}

const CALC_INIT: &str = r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("calc.add", {
  add = function(args)
    local calls = bitty.store.get("calls") or 0
    bitty.store.set("calls", calls + 1)
    return { sum = args.a + args.b }
  end,
})
return {}
"#;

const BROKEN_MANIFEST: &str = r#"[services.provided]
"calc.broken" = { version = "1.0.0", result_schema = "{\"type\": \"object\", \"properties\": {\"sum\": {\"type\": \"integer\"}}, \"required\": [\"sum\"], \"additionalProperties\": false}" }
"#;

const BROKEN_INIT: &str = r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("calc.broken", {
  run = function(args) return 1 end,
})
return {}
"#;

const SHOP_MANIFEST: &str = r#"[services.required]
"calc.add" = ">=1.0"
"calc.broken" = "^1.0"
"#;

const SHOP_INIT: &str = r#"
local calc = bitty.services.get("calc.add")
local broken = bitty.services.get("calc.broken")
bitty.commands.register({
  id = "total",
  title = "Total",
  run = function()
    local ok, r = pcall(calc.add, { a = 10, b = 20 })
    if ok then
      bitty.store.set("last_code", "NONE")
      return r.sum
    else
      bitty.store.set("last_code", r.code)
      return -1
    end
  end,
})
local ok, result = pcall(calc.add, { a = 2, b = 3 })
bitty.store.set("sum", ok and result.sum or -1)
bitty.store.set("code", ok and "NONE" or result.code)
local bad_ok, bad_err = pcall(calc.add, { a = "nope" })
bitty.store.set("bad_code", bad_ok and "NONE" or bad_err.code)
local ok2, r2 = pcall(broken.run, { x = 1 })
bitty.store.set("broken_code", ok2 and "NONE" or r2.code)
return {}
"#;

fn calc_id() -> PluginId {
    PluginId::new("xuepoo.calc").expect("id")
}

fn broken_id() -> PluginId {
    PluginId::new("xuepoo.broken").expect("id")
}

fn shop_id() -> PluginId {
    PluginId::new("xuepoo.shop").expect("id")
}

fn store_string(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<String> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::String(text)) => Some(text),
            _ => None,
        })
    })
}

fn store_int(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<i64> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::Integer(value)) => Some(value),
            _ => None,
        })
    })
}

fn setup(tag: &str) -> (PluginRuntime, PathBuf) {
    let root = temp_dir(tag);
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.broken",
        BROKEN_MANIFEST,
        &["xuepoo.broken:ping"],
        BROKEN_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.shop",
        SHOP_MANIFEST,
        &["xuepoo.shop:total"],
        SHOP_INIT,
    );
    let rt = runtime(vec![third], data.clone());
    (rt, root)
}

#[test]
fn provide_resolve_call_round_trip_with_schemas() {
    let (mut rt, root) = setup("round-trip");
    assert_eq!(rt.discover().len(), 3);

    rt.activate(&calc_id()).expect("provider activates");
    assert_eq!(rt.service_directory().borrow().published_count(), 1);
    rt.activate(&broken_id())
        .expect("broken provider activates");
    assert_eq!(rt.service_directory().borrow().published_count(), 2);
    rt.activate(&shop_id()).expect("consumer activates");

    // Consumer init resolved and called across VMs with schema validation.
    assert_eq!(store_int(&rt, &shop_id(), "sum"), Some(5));
    assert_eq!(
        store_string(&rt, &shop_id(), "code"),
        Some("NONE".to_string())
    );
    // Schema-violating args never execute the callee.
    assert_eq!(
        store_string(&rt, &shop_id(), "bad_code"),
        Some("E_SERVICE_INVALID".to_string())
    );
    // Schema-violating results fail closed at the boundary.
    assert_eq!(
        store_string(&rt, &shop_id(), "broken_code"),
        Some("E_SERVICE_INVALID".to_string())
    );
    // The callee ran exactly once under its own grants: the valid init
    // call. (Invalid-args and invalid-result calls never commit callee
    // effects — the former never runs, the latter validates after.)
    assert_eq!(store_int(&rt, &calc_id(), "calls"), Some(1));

    // A pinned handle serves through dispatch.
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(30)
    );
    assert_eq!(
        store_string(&rt, &shop_id(), "last_code"),
        Some("NONE".to_string())
    );
    assert_eq!(store_int(&rt, &calc_id(), "calls"), Some(2));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn suspend_resume_dispose_fail_closed_with_gone() {
    let (mut rt, root) = setup("lifecycle");
    rt.discover();
    rt.activate(&calc_id()).expect("provider activates");
    rt.activate(&broken_id())
        .expect("broken provider activates");
    rt.activate(&shop_id()).expect("consumer activates");

    rt.suspend(&calc_id()).expect("suspend");
    assert_eq!(rt.service_directory().borrow().published_count(), 2);
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(-1)
    );
    assert_eq!(
        store_string(&rt, &shop_id(), "last_code"),
        Some("E_SERVICE_GONE".to_string())
    );

    rt.resume(&calc_id()).expect("resume");
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(30)
    );

    rt.dispose(&calc_id()).expect("dispose");
    assert_eq!(rt.service_directory().borrow().published_count(), 1);
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(-1)
    );
    assert_eq!(
        store_string(&rt, &shop_id(), "last_code"),
        Some("E_SERVICE_GONE".to_string())
    );

    // Reload publishes a new generation: the pre-dispose pinned handle is
    // stale and never hijacked by the replacement.
    rt.activate(&calc_id()).expect("re-activate after dispose");
    assert_eq!(rt.service_directory().borrow().published_count(), 2);
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(-1)
    );
    assert_eq!(
        store_string(&rt, &shop_id(), "last_code"),
        Some("E_SERVICE_GONE".to_string())
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn optional_missing_resolves_nil_while_provider_suspended() {
    let root = temp_dir("optional");
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.opt",
        "[services.required]\n\"calc.add\" = \">=1.0\"\n",
        &["xuepoo.opt:probe"],
        r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local handle = bitty.services.get("calc.add", { optional = true })
bitty.store.set("is_nil", handle == nil)
return {}
"#,
    );
    let mut rt = runtime(vec![third], data);
    rt.discover();
    rt.activate(&calc_id()).expect("provider activates");
    rt.suspend(&calc_id()).expect("suspend");
    // The provider stays declared in the policy host while suspended, so
    // the consumer activates; the parked record resolves to nil.
    let opt_id = PluginId::new("xuepoo.opt").expect("id");
    rt.activate(&opt_id).expect("optional consumer activates");
    assert_eq!(
        rt.services(&opt_id)
            .expect("services")
            .with_store(|store| store.get("is_nil")),
        Some(LuaValue::Bool(true))
    );
    rt.resume(&calc_id()).expect("resume");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn undeclared_provide_fails_activation() {
    let root = temp_dir("undeclared");
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.rogue",
        "",
        &["xuepoo.rogue:ping"],
        r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("calc.add", { add = function(args) return args end })
return {}
"#,
    );
    let mut rt = runtime(vec![third], data);
    rt.discover();
    let rogue_id = PluginId::new("xuepoo.rogue").expect("id");
    assert!(
        rt.activate(&rogue_id).is_err(),
        "undeclared provision must fail activation"
    );
    assert!(matches!(
        rt.state(&rogue_id),
        Some(LifecycleState::Failed(_))
    ));
    assert_eq!(rt.service_directory().borrow().published_count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unsatisfiable_requirement_resolves_to_error() {
    let root = temp_dir("unsat");
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.picky",
        "[services.required]\n\"calc.add\" = \">=9.0\"\n",
        &["xuepoo.picky:probe"],
        r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
-- The single-plugin policy gate does not enforce service versions (that
-- is the `resolve_all` backstop); the live directory fails the lookup
-- closed at call time.
local ok, err = pcall(bitty.services.get, "calc.add")
bitty.store.set("code", ok and "NONE" or err.code)
local opt = bitty.services.get("calc.add", { optional = true })
bitty.store.set("opt_nil", opt == nil)
return {}
"#,
    );
    let mut rt = runtime(vec![third], data);
    rt.discover();
    rt.activate(&calc_id()).expect("provider activates");
    let picky_id = PluginId::new("xuepoo.picky").expect("id");
    rt.activate(&picky_id).expect("consumer activates");
    assert_eq!(
        store_string(&rt, &picky_id, "code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    assert_eq!(
        rt.services(&picky_id)
            .expect("services")
            .with_store(|store| store.get("opt_nil")),
        Some(LuaValue::Bool(true))
    );
    let _ = std::fs::remove_dir_all(&root);
}
