//! End-to-end `bitty.history` surface test (CTX-1352 / issue #2247).
//!
//! Unlike the bridge-only tests, this installs the real `PluginServices`
//! host implementation over a `BoundedHistory` provider and drives the exact
//! Lua call shape the three official plugins use. It proves:
//!
//! - granted transcript/commands/kv calls cross the bridge, the Core
//!   `HistoryGate`, and the provider and return rows;
//! - an ungranted source returns a catchable typed code;
//! - a host without a provider returns `E_HISTORY_UNAVAILABLE`, never a
//!   nil-index or a fabricated page.

use std::cell::RefCell;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{BoundedExecution, HostServices, LuaValue, LuaVm, MarshallingLimits};
use bitty_plugin_host::{Attribution, HistorySource, PluginId, StoredRow};
use bitty_runtime::plugin_runtime::{
    BoundedHistory, EmptySettings, NotificationQueue, PluginServices, PluginStore,
    UnavailableSnapshot,
};

fn vm_for(services: Rc<PluginServices>) -> LuaVm {
    let mut vm = build_plugin_vm("bitty-terminal.history", Some(VmBudgets::default()))
        .expect("default budgets");
    let host: Rc<dyn HostServices> = services;
    vm.install_host_module(host, MarshallingLimits::default(), 50)
        .expect("install host module");
    vm
}

fn services(grants: &[&str], provider: Option<Rc<BoundedHistory>>) -> Rc<PluginServices> {
    let services = PluginServices::new(
        "bitty-terminal.history",
        PluginStore::in_memory(),
        Rc::new(EmptySettings),
        Rc::new(UnavailableSnapshot),
        Rc::new(RefCell::new(NotificationQueue::new(8))),
        false,
        false,
    );
    services.set_granted_capabilities(grants.iter().map(|head| (*head).to_string()).collect());
    if let Some(provider) = provider {
        services.set_history_provider(Some(provider));
    }
    Rc::new(services)
}

fn row(owner: Option<&str>, panel: &str, seq: u64, body: &str) -> StoredRow {
    StoredRow {
        owner: owner.map(|id| PluginId::new(id).expect("plugin id")),
        panel: panel.to_string(),
        workspace: "ws-1".to_string(),
        seq,
        redacted_body: body.to_string(),
        attribution: Attribution {
            panel: panel.to_string(),
            workspace: "ws-1".to_string(),
            command: None,
            recorded_at: seq,
            actor: None,
        },
        purged: false,
    }
}

fn provider() -> Rc<BoundedHistory> {
    let provider = Rc::new(BoundedHistory::empty(true, true));
    provider.push(
        HistorySource::Transcript,
        row(None, "pane-a", 0, "transcript row"),
    );
    provider.push(
        HistorySource::CommandHistory,
        row(None, "pane-a", 1, "cargo test"),
    );
    provider.push(
        HistorySource::PluginKv,
        row(Some("bitty-terminal.history"), "", 2, "own value"),
    );
    provider
}

fn run(vm: &mut LuaVm, code: &str) {
    let outcome = vm.execute_bounded(code).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

fn stored(services: &Rc<PluginServices>, key: &str) -> Option<LuaValue> {
    HostServices::store_get(&**services, key).expect("store read")
}

#[test]
fn three_plugin_call_shapes_return_rows_for_all_sources() {
    let services = services(
        &[
            "history.transcript.read",
            "history.commands.read",
            "history.kv.read",
        ],
        Some(provider()),
    );
    let mut vm = vm_for(services.clone());
    run(
        &mut vm,
        r#"
        local transcript = bitty.history.transcript.query({
          scope = { workspace = "ws-1" },
          row_start = 0,
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("transcript_rows", #transcript.records)
        bitty.store.set("transcript_body", transcript.records[1].body)

        local commands = bitty.history.commands.query({
          scope = { panel = "pane-a", workspace = "ws-1" },
          row_start = 0,
          row_count = 16,
          max_bytes = 4096,
          op = "search",
          needle = "cargo",
        })
        bitty.store.set("commands_rows", #commands.records)

        local kv = bitty.history.kv.query({
          scope = {},
          row_start = 0,
          row_count = 16,
          max_bytes = 4096,
          op = "tail",
        })
        bitty.store.set("kv_rows", #kv.records)
        bitty.store.set("kv_body", kv.records[1].body)
        bitty.store.set("freshness", transcript.freshness)
        bitty.store.set("label", transcript.records[1].label)
        "#,
    );
    assert_eq!(
        stored(&services, "transcript_rows"),
        Some(LuaValue::Integer(1))
    );
    assert_eq!(
        stored(&services, "transcript_body"),
        Some(LuaValue::String("transcript row".to_string()))
    );
    assert_eq!(
        stored(&services, "commands_rows"),
        Some(LuaValue::Integer(1))
    );
    assert_eq!(stored(&services, "kv_rows"), Some(LuaValue::Integer(1)));
    assert_eq!(
        stored(&services, "kv_body"),
        Some(LuaValue::String("own value".to_string()))
    );
    assert_eq!(
        stored(&services, "freshness"),
        Some(LuaValue::String("point-in-time-no-guarantee".to_string()))
    );
    assert_eq!(
        stored(&services, "label"),
        Some(LuaValue::String("untrusted-observation".to_string()))
    );
}

#[test]
fn ungranted_source_returns_typed_code() {
    let services = services(&["history.transcript.read"], Some(provider()));
    let mut vm = vm_for(services.clone());
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.history.commands.query, {
          scope = { workspace = "ws-1" },
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("ok", ok)
        bitty.store.set("code", err and err.code or "none")
        "#,
    );
    assert_eq!(stored(&services, "ok"), Some(LuaValue::Bool(false)));
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_HISTORY_MISSING_GRANT".to_string()))
    );
}

#[test]
fn absent_source_returns_typed_unavailable_not_nil_index() {
    let services = services(&["history.transcript.read"], None);
    let mut vm = vm_for(services.clone());
    run(
        &mut vm,
        r#"
        local history = bitty.history
        local transcript = history.transcript
        local query = transcript.query
        bitty.store.set("surface", type(query))
        local ok, err = pcall(query, {
          scope = { workspace = "ws-1" },
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("ok", ok)
        bitty.store.set("code", err and err.code or "none")
        "#,
    );
    assert_eq!(
        stored(&services, "surface"),
        Some(LuaValue::String("function".to_string()))
    );
    assert_eq!(stored(&services, "ok"), Some(LuaValue::Bool(false)));
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_HISTORY_UNAVAILABLE".to_string()))
    );
}

#[test]
fn empty_available_provider_returns_empty_success_page() {
    let services = services(
        &["history.transcript.read"],
        Some(Rc::new(BoundedHistory::empty(true, true))),
    );
    let mut vm = vm_for(services.clone());
    run(
        &mut vm,
        r#"
        local page = bitty.history.transcript.query({
          scope = { workspace = "ws-1" },
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("ok", page.ok)
        bitty.store.set("rows", #page.records)
        bitty.store.set("total", page.total_in_scope)
        "#,
    );
    assert_eq!(stored(&services, "ok"), Some(LuaValue::Bool(true)));
    assert_eq!(stored(&services, "rows"), Some(LuaValue::Integer(0)));
    assert_eq!(stored(&services, "total"), Some(LuaValue::Integer(0)));
}
