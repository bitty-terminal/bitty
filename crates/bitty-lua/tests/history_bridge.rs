//! `bitty.history` bridge shape tests (RFC-0004, CTX-1352 / issue #2247).
//!
//! The bridge must always expose the NEW read-only `history` root with the
//! three source namespaces (`transcript`, `commands`, `kv`) and one `query`
//! entry each. Calls validate shape, then cross the host-services boundary
//! where the Core history gate owns grants, scopes, capture, safe mode,
//! bounds, and budgets. Every failure is a typed catchable table; the
//! `search`/`copy-mode`/`history` nil-index regression is exactly an absent
//! root, so presence is asserted first.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, HistoryQueryOp, HistoryQueryRequest, HistoryQuerySource,
    HostServices, LuaValue, LuaVm, MarshallingLimits,
};

fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

/// Mock history backend with explicit per-source grants and availability.
#[derive(Default)]
struct FakeHistory {
    grants: RefCell<BTreeSet<String>>,
    available: Cell<bool>,
    empty: Cell<bool>,
    requests: RefCell<Vec<(HistoryQuerySource, HistoryQueryRequest)>>,
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl FakeHistory {
    fn granted(sources: &[HistoryQuerySource]) -> Self {
        let fake = Self {
            available: Cell::new(true),
            ..Self::default()
        };
        for source in sources {
            fake.grants
                .borrow_mut()
                .insert(source.capability_head().to_string());
        }
        fake
    }

    fn denied(kind: &'static str) -> BridgeError {
        BridgeError::new(
            "runtime",
            kind,
            format!("{kind} denied for level 'third-party-lua' (family 'history')"),
        )
    }

    fn page(&self, source: HistoryQuerySource) -> LuaValue {
        let records = if self.empty.get() {
            Vec::new()
        } else {
            vec![LuaValue::table([
                ("seq", LuaValue::Integer(0)),
                (
                    "body",
                    LuaValue::String(format!("redacted {} row", source.as_str())),
                ),
                ("truncated", LuaValue::Bool(false)),
                ("redacted", LuaValue::Bool(true)),
                (
                    "label",
                    LuaValue::String("untrusted-observation".to_string()),
                ),
                (
                    "attribution",
                    LuaValue::table([
                        ("panel", LuaValue::String("pane-a".to_string())),
                        ("workspace", LuaValue::String("ws-1".to_string())),
                        ("recorded_at", LuaValue::Integer(1)),
                    ]),
                ),
            ])]
        };
        LuaValue::table([
            ("ok", LuaValue::Bool(true)),
            ("records", LuaValue::array(records)),
            (
                "total_in_scope",
                LuaValue::Integer(if self.empty.get() { 0 } else { 1 }),
            ),
            (
                "freshness",
                LuaValue::String("point-in-time-no-guarantee".to_string()),
            ),
        ])
    }
}

impl HostServices for FakeHistory {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        self.store.borrow_mut().insert(key.to_string(), value);
        Ok(())
    }

    fn settings_get(&self, _key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(None)
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }

    fn history_query(
        &self,
        source: HistoryQuerySource,
        request: &HistoryQueryRequest,
    ) -> Result<LuaValue, BridgeError> {
        if !self.grants.borrow().contains(source.capability_head()) {
            return Err(Self::denied("E_HISTORY_MISSING_GRANT"));
        }
        if !self.available.get() {
            return Err(Self::denied("E_HISTORY_UNAVAILABLE"));
        }
        self.requests.borrow_mut().push((source, request.clone()));
        Ok(self.page(source))
    }
}

fn install(vm: &mut LuaVm, services: Rc<FakeHistory>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn run(vm: &mut LuaVm, code: &str) -> BoundedExecution {
    vm.execute_bounded(code).expect("execute")
}

#[test]
fn history_namespace_is_present_with_three_sources() {
    let services = Rc::new(FakeHistory::granted(&[]));
    let mut vm = gate_vm("bitty-terminal.history");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        bitty.store.set("history_type", type(bitty.history))
        bitty.store.set("transcript", type(bitty.history.transcript.query))
        bitty.store.set("commands", type(bitty.history.commands.query))
        bitty.store.set("kv", type(bitty.history.kv.query))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // The read-only proxy still reports a table to plugins.
    assert_eq!(
        services.store.borrow().get("history_type"),
        Some(&LuaValue::String("table".to_string()))
    );
    for key in ["transcript", "commands", "kv"] {
        assert_eq!(
            services.store.borrow().get(key),
            Some(&LuaValue::String("function".to_string())),
            "{key}"
        );
    }
}

#[test]
fn transcript_list_call_shape_reaches_host() {
    let services = Rc::new(FakeHistory::granted(&[HistoryQuerySource::Transcript]));
    let mut vm = gate_vm("bitty-terminal.search");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        local page = bitty.history.transcript.query({
          scope = { workspace = "ws-1" },
          row_start = 0,
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("ok", page.ok)
        bitty.store.set("rows", #page.records)
        bitty.store.set("body", page.records[1].body)
        bitty.store.set("freshness", page.freshness)
        bitty.store.set("label", page.records[1].label)
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let requests = services.requests.borrow();
    assert_eq!(requests.len(), 1);
    let (source, request) = &requests[0];
    assert_eq!(*source, HistoryQuerySource::Transcript);
    assert_eq!(request.workspace.as_deref(), Some("ws-1"));
    assert_eq!(request.panel, None);
    assert_eq!(request.row_start, 0);
    assert_eq!(request.row_count, 16);
    assert_eq!(request.max_bytes, 4096);
    assert_eq!(request.op, HistoryQueryOp::List);
}

#[test]
fn commands_search_and_kv_shapes_reach_host() {
    let services = Rc::new(FakeHistory::granted(&[
        HistoryQuerySource::CommandHistory,
        HistoryQuerySource::PluginKv,
    ]));
    let mut vm = gate_vm("bitty-terminal.history");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        bitty.history.commands.query({
          scope = { panel = "pane-a", workspace = "ws-1" },
          row_start = 2,
          row_count = 4,
          max_bytes = 512,
          op = "search",
          needle = "error",
        })
        bitty.history.kv.query({
          scope = {},
          row_start = 0,
          row_count = 2,
          max_bytes = 256,
          op = "tail",
        })
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let requests = services.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, HistoryQuerySource::CommandHistory);
    assert_eq!(requests[0].1.op, HistoryQueryOp::Search);
    assert_eq!(requests[0].1.needle.as_deref(), Some("error"));
    assert_eq!(requests[0].1.panel.as_deref(), Some("pane-a"));
    assert_eq!(requests[1].0, HistoryQuerySource::PluginKv);
    assert_eq!(requests[1].1.op, HistoryQueryOp::Tail);
    assert_eq!(requests[1].1.workspace, None);
}

#[test]
fn ungranted_source_denies_with_typed_code() {
    let services = Rc::new(FakeHistory::granted(&[HistoryQuerySource::Transcript]));
    let mut vm = gate_vm("bitty-terminal.history");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.history.commands.query, {
          scope = { workspace = "ws-1" },
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("ok", ok)
        if not ok then
          bitty.store.set("code", err.code)
          bitty.store.set("text", tostring(err))
        end
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("ok"),
        Some(&LuaValue::Bool(false))
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_HISTORY_MISSING_GRANT".to_string()))
    );
    let text = services.store.borrow().get("text").cloned();
    let Some(LuaValue::String(text)) = text else {
        panic!("tostring(err)");
    };
    assert!(text.contains("E_HISTORY_MISSING_GRANT"), "{text}");
}

#[test]
fn absent_source_denies_with_typed_code_not_nil_index() {
    let services = Rc::new(FakeHistory {
        grants: RefCell::new(BTreeSet::from(["history.transcript.read".to_string()])),
        available: Cell::new(false),
        ..FakeHistory::default()
    });
    let mut vm = gate_vm("bitty-terminal.copy-mode");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        -- Mirrors the copy-mode/search guard: the full path is indexed only
        -- after each link exists, then pcall wraps the call itself.
        local function transcript_query_fn()
          if bitty == nil then return nil end
          local history = bitty.history
          if history == nil then return nil end
          local transcript = history.transcript
          if transcript == nil then return nil end
          local query = transcript.query
          if type(query) ~= "function" then return nil end
          return query
        end
        local query = transcript_query_fn()
        local ok, err = pcall(query, {
          scope = { workspace = "ws-1" },
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("guard", type(query))
        bitty.store.set("ok", ok)
        if not ok then
          bitty.store.set("code", err.code)
        end
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("guard"),
        Some(&LuaValue::String("function".to_string()))
    );
    assert_eq!(
        services.store.borrow().get("ok"),
        Some(&LuaValue::Bool(false))
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_HISTORY_UNAVAILABLE".to_string()))
    );
}

#[test]
fn empty_result_is_a_successful_page() {
    let services = Rc::new(FakeHistory::granted(&[HistoryQuerySource::Transcript]));
    services.empty.set(true);
    let mut vm = gate_vm("bitty-terminal.history");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        local page = bitty.history.transcript.query({
          scope = { panel = "pane-a" },
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("ok", page.ok)
        bitty.store.set("total", page.total_in_scope)
        bitty.store.set("rows", #page.records)
        bitty.store.set("freshness", page.freshness)
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("ok"),
        Some(&LuaValue::Bool(true))
    );
    assert_eq!(
        services.store.borrow().get("total"),
        Some(&LuaValue::Integer(0))
    );
    assert_eq!(
        services.store.borrow().get("rows"),
        Some(&LuaValue::Integer(0))
    );
}

#[test]
fn malformed_query_shape_fails_typed() {
    let services = Rc::new(FakeHistory::granted(&[HistoryQuerySource::Transcript]));
    let mut vm = gate_vm("bitty-terminal.history");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.history.transcript.query, {
          row_count = 16,
          max_bytes = 4096,
          op = "list",
        })
        bitty.store.set("missing_scope", err and err.code or "none")
        local ok2, err2 = pcall(bitty.history.transcript.query, {
          scope = {},
          row_count = 16,
          max_bytes = 4096,
          op = "search",
          needle = string.rep("x", 257),
        })
        bitty.store.set("long_needle", err2 and err2.code or "none")
        local ok3, err3 = pcall(bitty.history.transcript.query, {
          scope = {},
          row_count = 16,
          max_bytes = 4096,
          op = "unknown",
        })
        bitty.store.set("bad_op", err3 and err3.code or "none")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let store = services.store.borrow();
    assert_eq!(
        store.get("missing_scope"),
        Some(&LuaValue::String("E_DEF_INVALID".to_string()))
    );
    assert_eq!(
        store.get("long_needle"),
        Some(&LuaValue::String("E_HISTORY_OVER_BOUND".to_string()))
    );
    assert_eq!(
        store.get("bad_op"),
        Some(&LuaValue::String("E_DEF_INVALID".to_string()))
    );
}

#[test]
fn history_namespace_is_read_only() {
    for assignment in [
        "bitty.history = {}",
        "bitty.history.transcript = {}",
        "bitty.history.kv.query = function() end",
    ] {
        let mut vm = gate_vm("bitty-terminal.history");
        install(&mut vm, Rc::new(FakeHistory::granted(&[])));
        let outcome = run(&mut vm, assignment);
        assert!(
            matches!(outcome, BoundedExecution::RuntimeError(_)),
            "{assignment}: {outcome:?}"
        );
    }
}
