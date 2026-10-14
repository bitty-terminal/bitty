//! Service conformance: `bitty.services.get`/`provide` against the live
//! runtime directory (issue #1379, LUA-OQ-8).
//!
//! Two-plugin activation over real VMs: the provider publishes a
//! schema-typed interface, the consumer resolves by version requirement,
//! calls across VMs, and fails closed (`E_SERVICE_RESOLUTION`,
//! `E_SERVICE_GONE`, `E_SERVICE_INVALID`, `E_SERVICE_UNDECLARED`) through
//! suspend/resume/dispose and declaration violations. Headless and
//! Windows-safe (temp dirs only, no processes or sockets).

mod common;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use bitty_lua::LuaValue;
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, PluginStore, SettingsSource, SnapshotSource,
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-services-{tag}-{}-{}",
        std::process::id(),
        counter
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Runtime under test, with in-memory plugin stores.
///
/// These tests cover service routing, schemas, and lifecycle, not store
/// persistence. With a disk-backed store, the provider's `bitty.store.set`
/// inside a cross-VM call does an atomic temp-then-rename write; on a Windows
/// CI runner with filesystem scanning that write alone can exceed the 50 ms
/// RC-1 cheap-call budget, so the consumer saw the `-1` error sentinel
/// (CTX-0854). Committed store writes are now credited out of the RC-1 wall
/// clock (bitty #1518); an in-memory store still keeps these routing tests
/// independent of runner filesystem latency, and the disk-latency path is
/// covered deterministically by
/// `slow_disk_store_commit_does_not_fail_cross_vm_service_call`.
fn runtime(third_party_roots: Vec<PathBuf>) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots,
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    })
}

/// Write a plugin package with service sections under `root/<id>/`.
///
/// `capabilities_toml` holds the raw `[capabilities]` body (e.g.
/// `runtime.inspect = true`); empty means no capabilities section.
fn write_service_plugin(
    root: &Path,
    id: &str,
    services_toml: &str,
    capabilities_toml: &str,
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
    let capabilities_section = if capabilities_toml.is_empty() {
        String::new()
    } else {
        format!("[capabilities]\n{capabilities_toml}\n")
    };
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
{capabilities_section}[lazy]
commands = [{commands_toml}]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
    plugin
}

/// Backing Core head for every test service (dynapi gating, bitty#2005).
///
/// The head is arbitrary — no test exercises `runtime.inspect` itself — but
/// it must be a closed Core capability so manifests validate, and both the
/// provider and any resolving consumer must hold its grant.
const TEST_HEAD: &str = "runtime.inspect";

/// `[capabilities]` body granting the test head.
const TEST_GRANT: &str = "runtime.inspect = true";

const CALC_ARGS_SCHEMA: &str = "{\"type\": \"object\", \"properties\": {\"a\": {\"type\": \"integer\"}, \"b\": {\"type\": \"integer\"}}, \"required\": [\"a\", \"b\"], \"additionalProperties\": false}";
const CALC_RESULT_SCHEMA: &str = "{\"type\": \"object\", \"properties\": {\"sum\": {\"type\": \"integer\"}}, \"required\": [\"sum\"], \"additionalProperties\": false}";

fn calc_manifest() -> String {
    format!(
        "[services.provided]\n\"calc.add\" = {{ version = \"1.0.0\", args_schema = \"{args}\", result_schema = \"{result}\", heads = [\"{TEST_HEAD}\"] }}\n",
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
"calc.broken" = { version = "1.0.0", heads = ["runtime.inspect"], result_schema = "{\"type\": \"object\", \"properties\": {\"sum\": {\"type\": \"integer\"}}, \"required\": [\"sum\"], \"additionalProperties\": false}" }
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
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        TEST_GRANT,
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.broken",
        BROKEN_MANIFEST,
        TEST_GRANT,
        &["xuepoo.broken:ping"],
        BROKEN_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.shop",
        SHOP_MANIFEST,
        TEST_GRANT,
        &["xuepoo.shop:total"],
        SHOP_INIT,
    );
    let rt = runtime(vec![third]);
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
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        TEST_GRANT,
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.opt",
        "[services.required]\n\"calc.add\" = \">=1.0\"\n",
        TEST_GRANT,
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
    let mut rt = runtime(vec![third]);
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
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.rogue",
        "",
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
    let mut rt = runtime(vec![third]);
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
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        TEST_GRANT,
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.picky",
        "[services.required]\n\"calc.add\" = \">=9.0\"\n",
        TEST_GRANT,
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
    let mut rt = runtime(vec![third]);
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

/// Injected commit latency for the disk-backed regression (bitty #1518).
///
/// Past the 50 ms RC-1 wall-clock budget on its own, so a callback that is
/// still charged for its store commit always fails, and well under the
/// 200 ms per-callback credit cap for the single provider commit each
/// consumer callback waits on.
const SLOW_COMMIT_MS: u64 = 60;

/// Plugin whose store commits are slow; every other store stays fast.
const SLOW_STORE_PLUGIN: &str = "xuepoo.calc";

/// Slow-commit stub backend: the provider store commit point sleeps for
/// the provider store only, standing in for a scanned Windows CI
/// filesystem without real slow disks.
#[derive(Debug, Clone)]
struct SlowCommitBackend {
    inner: common::MemoryKvBackend,
}

impl SlowCommitBackend {
    fn new() -> Self {
        Self {
            inner: common::MemoryKvBackend::new(),
        }
    }
}

impl bitty_runtime::plugin_runtime::KvCommitBackend for SlowCommitBackend {
    fn commit_store_bytes(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), bitty_runtime::plugin_runtime::KvCommitError> {
        let provider_store = path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir == SLOW_STORE_PLUGIN);
        if provider_store {
            std::thread::sleep(std::time::Duration::from_millis(SLOW_COMMIT_MS));
        }
        self.inner.commit_store_bytes(path, bytes)
    }

    fn load_store_bytes(
        &self,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, bitty_runtime::plugin_runtime::KvCommitError> {
        self.inner.load_store_bytes(path)
    }
}

/// Consumer for the slow-disk regression: every service call is followed by
/// Lua work spanning many `SLICE_FUEL` slices, so the RC-1 wall check runs
/// after the provider's credited commit instead of the callback finishing
/// inside the slice that made the call.
const SLOW_SHOP_INIT: &str = r#"
local calc = bitty.services.get("calc.add")
local function cross_slices()
  local n = 0
  for i = 1, 4096 do n = n + i end
  return n
end
bitty.commands.register({
  id = "total",
  title = "Total",
  run = function()
    local ok, r = pcall(calc.add, { a = 10, b = 20 })
    cross_slices()
    return ok and r.sum or -1
  end,
})
local ok, result = pcall(calc.add, { a = 2, b = 3 })
cross_slices()
bitty.store.set("sum", ok and result.sum or -1)
bitty.store.set("code", ok and "NONE" or result.code)
return {}
"#;

const SLOW_SHOP_MANIFEST: &str = r#"[services.required]
"calc.add" = ">=1.0"
"#;

#[test]
fn slow_disk_store_commit_does_not_fail_cross_vm_service_call() {
    // bitty #1518: the provider's `bitty.store.set` commits to a disk-backed
    // store slower than the RC-1 wall-clock budget. The committed write is
    // credited out of both callbacks' wall clocks, so the consumer still
    // receives the value and nothing reports the error sentinel.
    let root = temp_dir("slow-disk");
    let third = root.join("third");
    let state = root.join("state");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        TEST_GRANT,
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.shop",
        SLOW_SHOP_MANIFEST,
        TEST_GRANT,
        &["xuepoo.shop:total"],
        SLOW_SHOP_INIT,
    );
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(state.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![third],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    });
    let commit_backend = Arc::new(SlowCommitBackend::new());
    rt.set_store_backend(Some(
        commit_backend.clone() as Arc<dyn bitty_runtime::plugin_runtime::KvCommitBackend>
    ));
    rt.discover();
    rt.activate(&calc_id()).expect("provider activates");
    rt.activate(&shop_id()).expect("consumer activates");

    // The consumer receives the provider's real result, not the -1 sentinel.
    assert_eq!(store_int(&rt, &shop_id(), "sum"), Some(5));
    assert_eq!(
        store_string(&rt, &shop_id(), "code"),
        Some("NONE".to_string())
    );
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(30)
    );
    assert_eq!(store_int(&rt, &calc_id(), "calls"), Some(2));

    // The committed provider value is persisted and readable back through
    // the backend (on-disk durability rides the real backend in the wiring
    // crate; the stub proves the commit/read round trip through the seam).
    let reloaded = PluginStore::load_with_backend(
        state.join("xuepoo.calc").join("store.json"),
        Some(commit_backend.clone() as Arc<dyn bitty_runtime::plugin_runtime::KvCommitBackend>),
    )
    .expect("reload store");
    assert_eq!(reloaded.get("calls"), Some(LuaValue::Integer(2)));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unbacked_provide_rejected_at_install_with_naming_diagnostic() {
    // bitty#2005: a provided interface without backing heads never records.
    // The table form without `heads` is rejected at install (discover) time.
    let root = temp_dir("unbacked");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.solo",
        "[services.provided]\n\"calc.solo\" = { version = \"1.0.0\" }\n",
        TEST_GRANT,
        &["xuepoo.solo:ping"],
        r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("calc.solo", { run = function(args) return args end })
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    let err = format!(
        "{}",
        results[0]
            .1
            .as_ref()
            .expect_err("unbacked provide must reject at install")
    );
    assert!(err.contains("calc.solo"), "names the iface: {err}");
    assert!(err.contains("heads"), "names the field: {err}");
    assert_eq!(rt.service_directory().borrow().published_count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn head_spoofing_provide_naming_ungranted_head_rejected_at_record() {
    // bitty#2005: naming a valid Core head the provider was never granted
    // denies the provide with E_CAPABILITY_DENIED, so a listed extension can
    // never widen its own authority through a provide. The denial is probed
    // with pcall (an uncaught provide fails the whole activation instead);
    // either way nothing is recorded.
    let root = temp_dir("spoof");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.spoof",
        "[services.provided]\n\"calc.spoof\" = { version = \"1.0.0\", heads = [\"workspace.read\"] }\n",
        "",
        &["xuepoo.spoof:ping"],
        r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
local ok, err = pcall(bitty.services.provide, "calc.spoof", { run = function(args) return args end })
bitty.store.set("code", ok and "NONE" or err.code)
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    rt.discover();
    let spoof_id = PluginId::new("xuepoo.spoof").expect("id");
    rt.activate(&spoof_id).expect("probe activates");
    assert_eq!(
        store_string(&rt, &spoof_id, "code"),
        Some("E_CAPABILITY_DENIED".to_string())
    );
    assert_eq!(rt.service_directory().borrow().published_count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn confused_deputy_consume_without_grant_fails_closed_indistinguishable_from_absent() {
    // bitty#2005: A provides a backed interface; B declares the requirement
    // but holds no grant for the backing head. B's consume fails closed with
    // exactly the code an absent service yields, while a granted sibling
    // resolves and calls through the same live record.
    let root = temp_dir("deputy");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.calc",
        &calc_manifest(),
        TEST_GRANT,
        &["xuepoo.calc:ping"],
        CALC_INIT,
    );
    write_service_plugin(
        &third,
        "xuepoo.deputy",
        "[services.required]\n\"calc.add\" = \">=1.0\"\n",
        "",
        &["xuepoo.deputy:probe"],
        r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local ok, err = pcall(bitty.services.get, "calc.add")
bitty.store.set("code", ok and "NONE" or err.code)
local opt = bitty.services.get("calc.add", { optional = true })
bitty.store.set("opt_nil", opt == nil)
return {}
"#,
    );
    write_service_plugin(
        &third,
        "xuepoo.friend",
        "[services.required]\n\"calc.add\" = \">=1.0\"\n",
        TEST_GRANT,
        &["xuepoo.friend:probe"],
        r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local calc = bitty.services.get("calc.add")
local ok, result = pcall(calc.add, { a = 1, b = 2 })
bitty.store.set("sum", ok and result.sum or -1)
bitty.store.set("code", ok and "NONE" or result.code)
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    rt.discover();
    rt.activate(&calc_id()).expect("provider activates");
    let deputy_id = PluginId::new("xuepoo.deputy").expect("id");
    rt.activate(&deputy_id).expect("deputy activates");
    // Indistinguishable from absent: the same code the unsatisfiable-version
    // consumer observes, nil when optional.
    assert_eq!(
        store_string(&rt, &deputy_id, "code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    assert_eq!(
        rt.services(&deputy_id)
            .expect("services")
            .with_store(|store| store.get("opt_nil")),
        Some(LuaValue::Bool(true))
    );
    // The same record serves a granted consumer: only the grant differs.
    let friend_id = PluginId::new("xuepoo.friend").expect("id");
    rt.activate(&friend_id).expect("granted consumer activates");
    assert_eq!(store_int(&rt, &friend_id, "sum"), Some(3));
    assert_eq!(
        store_string(&rt, &friend_id, "code"),
        Some("NONE".to_string())
    );
    // The broker recorded only the granted call (owner + backing head).
    let calls = rt.service_call_ledger().borrow().calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].provider, "xuepoo.calc");
    assert_eq!(calls[0].heads, vec![TEST_HEAD.to_string()]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn granted_calls_record_provider_owner_and_backing_heads() {
    // bitty#2005 broker observability: every dispatched call records the
    // provider owner plus the backing heads projected by the call.
    let (mut rt, root) = setup("ledger");
    rt.discover();
    rt.activate(&calc_id()).expect("provider activates");
    rt.activate(&broken_id())
        .expect("broken provider activates");
    rt.activate(&shop_id()).expect("consumer activates");
    // Init dispatched two calls (the valid calc.add and the result-invalid
    // calc.broken, which still reached its provider); the schema-rejected
    // args call never dispatched and records nothing. Budgets and the
    // argv-first call shape are unchanged by the gating.
    let calls = rt.service_call_ledger().borrow().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].provider, "xuepoo.calc");
    assert_eq!(calls[0].iface, "calc.add");
    assert_eq!(calls[0].method, "add");
    assert_eq!(calls[0].heads, vec![TEST_HEAD.to_string()]);
    assert_eq!(calls[1].provider, "xuepoo.broken");
    assert_eq!(calls[1].iface, "calc.broken");
    assert_eq!(calls[1].heads, vec![TEST_HEAD.to_string()]);
    // A pinned-handle dispatch records the same owner and heads.
    assert_eq!(
        rt.dispatch_command(&shop_id(), "total", &[])
            .expect("dispatch"),
        LuaValue::Integer(30)
    );
    let calls = rt.service_call_ledger().borrow().calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].provider, "xuepoo.calc");
    assert_eq!(calls[2].method, "add");
    assert_eq!(calls[2].heads, vec![TEST_HEAD.to_string()]);
    let _ = std::fs::remove_dir_all(&root);
}

fn duet_manifest() -> String {
    format!(
        "[services.provided]\n\"duet.play\" = {{ version = \"1.0.0\", heads = [\"{TEST_HEAD}\"] }}\n",
    )
}

fn duet_init(marker: &str) -> String {
    format!(
        r#"
bitty.commands.register({{
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
}})
bitty.services.provide("duet.play", {{
  play = function(args) return "{marker}" end,
}})
return {{}}
"#
    )
}

#[test]
fn shared_short_name_resolves_only_through_qualified_forms() {
    // bitty#2003: two extensions claim the same short name. Both qualified
    // forms work end to end; the unqualified form fails closed with a
    // diagnostic naming both owners.
    let root = temp_dir("arbitration");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.alpha",
        &duet_manifest(),
        TEST_GRANT,
        &["xuepoo.alpha:ping"],
        &duet_init("alpha"),
    );
    write_service_plugin(
        &third,
        "xuepoo.beta",
        &duet_manifest(),
        TEST_GRANT,
        &["xuepoo.beta:ping"],
        &duet_init("beta"),
    );
    write_service_plugin(
        &third,
        "xuepoo.duet",
        "[services.required]\n\"xuepoo.alpha:duet.play\" = \">=1.0\"\n\"xuepoo.beta:duet.play\" = \">=1.0\"\n\"duet.play\" = \">=1.0\"\n",
        TEST_GRANT,
        &["xuepoo.duet:probe"],
        r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local alpha = bitty.services.get("xuepoo.alpha:duet.play")
local beta = bitty.services.get("xuepoo.beta:duet.play")
bitty.store.set("alpha", alpha.play({}))
bitty.store.set("beta", beta.play({}))
local ok, err = pcall(bitty.services.get, "duet.play")
bitty.store.set("short_code", ok and "NONE" or err.code)
bitty.store.set("short_msg", ok and "" or err.message)
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    rt.discover();
    let alpha_id = PluginId::new("xuepoo.alpha").expect("id");
    let beta_id = PluginId::new("xuepoo.beta").expect("id");
    let duet_id = PluginId::new("xuepoo.duet").expect("id");
    rt.activate(&alpha_id).expect("alpha activates");
    rt.activate(&beta_id).expect("beta activates");
    // A short-only consumer cannot activate against an ambiguous name...
    // (declared below); the qualified consumer activates and both handles
    // serve their own owner.
    rt.activate(&duet_id).expect("qualified consumer activates");
    assert_eq!(
        store_string(&rt, &duet_id, "alpha"),
        Some("alpha".to_string())
    );
    assert_eq!(
        store_string(&rt, &duet_id, "beta"),
        Some("beta".to_string())
    );
    assert_eq!(
        store_string(&rt, &duet_id, "short_code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    let msg = store_string(&rt, &duet_id, "short_msg").expect("diagnostic message");
    assert!(msg.contains("xuepoo.alpha"), "names first owner: {msg}");
    assert!(msg.contains("xuepoo.beta"), "names second owner: {msg}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn side_by_side_versions_coexist_and_pins_survive_republication() {
    // bitty#2006: two versions of one interface coexist; a range serves
    // the highest compatible (never a silent downgrade), an exact pin
    // serves its release, an empty intersection fails closed listing the
    // available versions, and a pinned consumer survives republication
    // (dispose + re-activate) still pinned.
    let root = temp_dir("negotiation");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.duet",
        "[services.provided]\n\"duet.play\" = { version = [\"1.0.0\", \"2.0.0\"], heads = [\"runtime.inspect\"] }\n",
        TEST_GRANT,
        &["xuepoo.duet:ping"],
        r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("duet.play", { play = function(args) return "v1" end })
bitty.services.provide("duet.play", { play = function(args) return "v2" end })
return {}
"#,
    );
    write_service_plugin(
        &third,
        "xuepoo.duetfan",
        "[services.required]\n\"duet.play\" = \">=1.0\"\n",
        TEST_GRANT,
        &["xuepoo.duetfan:probe"],
        r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local latest = bitty.services.get("duet.play")
bitty.store.set("latest", latest.play({}))
local pinned = bitty.services.get("duet.play", { version = "1.0.0" })
bitty.store.set("pinned", pinned.play({}))
local ok, err = pcall(bitty.services.get, "duet.play", { version = ">=9.0" })
bitty.store.set("empty_code", ok and "NONE" or err.code)
bitty.store.set("empty_msg", ok and "" or err.message)
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    rt.discover();
    let duet_id = PluginId::new("xuepoo.duet").expect("id");
    let fan_id = PluginId::new("xuepoo.duetfan").expect("id");
    rt.activate(&duet_id).expect("provider activates");
    assert_eq!(rt.service_directory().borrow().published_count(), 2);
    rt.activate(&fan_id).expect("consumer activates");
    assert_eq!(store_string(&rt, &fan_id, "latest"), Some("v2".to_string()));
    assert_eq!(store_string(&rt, &fan_id, "pinned"), Some("v1".to_string()));
    assert_eq!(
        store_string(&rt, &fan_id, "empty_code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    let msg = store_string(&rt, &fan_id, "empty_msg").expect("diagnostic message");
    assert!(msg.contains(">=9.0"), "names the range: {msg}");
    assert!(msg.contains("1.0.0"), "lists v1: {msg}");
    assert!(msg.contains("2.0.0"), "lists v2: {msg}");
    // Negotiation results are recorded per call for observability (4/4):
    // the range call served v2, the pinned call served v1.
    let calls = rt.service_call_ledger().borrow().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].version, "2.0.0");
    assert_eq!(calls[1].version, "1.0.0");
    // Republication (the upgrade path: dispose + re-activate) keeps the
    // pin on its release instead of rebinding or breaking.
    rt.dispose(&duet_id).expect("dispose");
    rt.dispose(&fan_id).expect("consumer dispose");
    assert_eq!(rt.service_directory().borrow().published_count(), 0);
    rt.activate(&duet_id).expect("re-activate after dispose");
    rt.activate(&fan_id).expect("consumer re-activates");
    assert_eq!(store_string(&rt, &fan_id, "latest"), Some("v2".to_string()));
    assert_eq!(store_string(&rt, &fan_id, "pinned"), Some("v1".to_string()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn core_reserved_provide_rejected_at_install() {
    // bitty#2003: the `bitty.*` namespace stays frozen — an extension
    // manifest claiming it fails closed at install (discover) time.
    let root = temp_dir("reserved");
    let third = root.join("third");
    write_service_plugin(
        &third,
        "xuepoo.greedy",
        "[services.provided]\n\"bitty.echo\" = { version = \"1.0.0\", heads = [\"runtime.inspect\"] }\n",
        TEST_GRANT,
        &["xuepoo.greedy:ping"],
        r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("bitty.echo", { run = function(args) return args end })
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    let err = format!(
        "{}",
        results[0]
            .1
            .as_ref()
            .expect_err("reserved provide must reject at install")
    );
    assert!(err.contains("bitty.echo"), "names the iface: {err}");
    assert!(err.contains("Core-reserved"), "names the rule: {err}");
    assert_eq!(rt.service_directory().borrow().published_count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}
