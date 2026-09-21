//! `bitty.ui.mount` / `bitty.ui.update` bridge probes (OQ-053 split gap, CTX-0428).
//!
//! The accepted surface (ADR-0009 `LUA-OQ-7`, Plugin API v1 Lua Surface RFC)
//! mounts declarative v1 scenes into the closed slot set. Before the
//! implementation the `bitty.ui` namespace was absent and the statusline and
//! palette packages degraded to command-only mode; the first test below is the
//! red probe that recorded that state.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bitty_lua::gate::{PluginVmBuilder, VmBudgets, build_plugin_vm};
use bitty_lua::ui::{UI_MAX_NODES, UI_MAX_TEXT_BYTES};
use bitty_lua::{
    BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits, UiNode,
};

/// Gate-built VM with default RC budgets (replaces deprecated `LuaVm::new`).
fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

#[derive(Default)]
struct UiServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    mounts: RefCell<Vec<(String, UiNode)>>,
    updates: RefCell<Vec<(i64, UiNode)>>,
    deny: Option<&'static str>,
    slow_ms: u64,
}

impl HostServices for UiServices {
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

    fn ui_mount(&self, slot: &str, component: &UiNode) -> Result<i64, BridgeError> {
        if let Some(capability) = self.deny {
            return Err(BridgeError::capability_denied(capability));
        }
        self.mounts
            .borrow_mut()
            .push((slot.to_string(), component.clone()));
        Ok(11)
    }

    fn ui_mount_with_expiry(
        &self,
        slot: &str,
        component: &UiNode,
        expiry: Instant,
    ) -> Result<i64, BridgeError> {
        if self.slow_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.slow_ms));
        }
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.ui_mount(slot, component)
    }

    fn ui_update(&self, handle: i64, component: &UiNode) -> Result<bool, BridgeError> {
        if let Some(capability) = self.deny {
            return Err(BridgeError::capability_denied(capability));
        }
        self.updates.borrow_mut().push((handle, component.clone()));
        Ok(handle == 11)
    }
}

fn install(vm: &mut LuaVm, services: Rc<UiServices>, deadline_ms: u64) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), deadline_ms)
        .expect("install");
}

/// Marshal byte headroom `UI_MARSHAL_LIMITS` grants over
/// [`UI_MAX_TEXT_BYTES`] (`bitty_lua::ui`). The probe below must stay under it
/// so the rejection is proven to come from the `SCN-3` scene walk, never from
/// the raw marshalling ceiling that runs first.
const UI_MARSHAL_BYTE_HEADROOM: usize = 64 * 1024;

/// Wall budget for the multi-input oversize probe, deliberately wider than
/// the RC-1 default ([`bitty_lua::RC1_WALL_CLOCK_BUDGET_MS`], 50 ms).
///
/// The probe marshals and validates two largest-accepted-shape inputs (a
/// 256 KiB+ text and an over-budget node scene) in one cold path. That path
/// costs ~2 ms of CPU locally, yet measured 50-58 ms on shared CI runners
/// under parallel test contention (the PX-2726 flake) - the inflation is
/// scheduling latency, not CPU work, so a cheaper probe alone cannot make the
/// fixed 50 ms window deterministic. The property under test is the
/// `SCN-1`/`SCN-3` size rejection, not the wall budget; the padded budget
/// keeps that rejection deterministic on slow runners while the instruction,
/// memory, and host-mutation deadlines keep their accepted defaults. The
/// hot-path wall-budget contract stays covered by `measurement_lua`, and the
/// assertions below pin the probe to one node past the `SCN-1` ceiling and
/// inside the marshalling headroom, so a weakened size bound cannot pass.
const OVERSIZE_PROBE_WALL_BUDGET_MS: u64 = 250;

/// Probe VM carrying [`OVERSIZE_PROBE_WALL_BUDGET_MS`] and otherwise default
/// RC budgets.
fn oversized_probe_vm(id: &str) -> LuaVm {
    PluginVmBuilder::new(id)
        .budgets(VmBudgets {
            wall_budget_ms: OVERSIZE_PROBE_WALL_BUDGET_MS,
            ..VmBudgets::default()
        })
        .build()
        .expect("padded wall budget is valid")
}

fn run(vm: &mut LuaVm, source: &str) {
    let outcome = vm.execute_bounded(source).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "script must complete: {outcome:?}"
    );
}

fn stored(services: &UiServices, key: &str) -> Option<LuaValue> {
    services.store.borrow().get(key).cloned()
}

/// RED PROBE 1: `bitty.ui` must be present with both functions (LUA-OQ-2
/// typed-denial stubs; before CTX-0428 it was `nil`).
#[test]
fn probe_bitty_ui_namespace_is_present() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("probe");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        bitty.store.set("ui_table", type(bitty.ui))
        bitty.store.set("mount_type", type(bitty.ui and bitty.ui.mount))
        bitty.store.set("update_type", type(bitty.ui and bitty.ui.update))
    "#,
    );
    assert_eq!(
        stored(&services, "ui_table"),
        Some(LuaValue::String("table".to_string()))
    );
    assert_eq!(
        stored(&services, "mount_type"),
        Some(LuaValue::String("function".to_string()))
    );
    assert_eq!(
        stored(&services, "update_type"),
        Some(LuaValue::String("function".to_string()))
    );
}

#[test]
fn mount_and_update_round_trip_validated_scene() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("round-trip");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local handle = bitty.ui.mount("statusline", {
          kind = "Column",
          children = {
            { kind = "Row", children = { { kind = "Text", text = "cwd:/tmp" } } },
            { kind = "List", children = { { kind = "Text", text = "item" } } },
          },
        })
        bitty.store.set("handle", handle)
        bitty.store.set("updated", bitty.ui.update(handle, { kind = "Text", text = "v2" }))
    "#,
    );
    assert_eq!(stored(&services, "handle"), Some(LuaValue::Integer(11)));
    assert_eq!(stored(&services, "updated"), Some(LuaValue::Bool(true)));

    let mounts = services.mounts.borrow();
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].0, "statusline");
    assert_eq!(mounts[0].1.kind(), "Column");
    assert_eq!(mounts[0].1.count_nodes(), 5);
    assert_eq!(mounts[0].1.text_bytes(), "cwd:/tmpitem".len());
    drop(mounts);

    let updates = services.updates.borrow();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].0, 11);
    assert_eq!(updates[0].1, UiNode::text("v2"));
}

#[test]
fn ui_namespace_is_read_only() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("readonly");
    install(&mut vm, services, 50);
    let outcome = vm
        .execute_bounded("bitty.ui.mount = function() end")
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "assignment must fail: {outcome:?}"
    );
    let outcome = vm.execute_bounded("bitty.ui = {}").expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "namespace assignment must fail: {outcome:?}"
    );
}

#[test]
fn unknown_slot_is_typed_component_error() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("slot");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.ui.mount, "nowhere", { kind = "Text", text = "x" })
        bitty.store.set("ok", ok)
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(stored(&services, "ok"), Some(LuaValue::Bool(false)));
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert!(services.mounts.borrow().is_empty());
}

#[test]
fn excluded_unknown_and_malformed_components_rejected() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("shapes");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local cases = {
          { kind = "Image", src = "x" },
          { kind = "CodeBlock", content = "x" },
          { kind = "Table", rows = {} },
          { kind = "Rule" },
          { kind = "Block", child = { kind = "Text", text = "x" } },
          { kind = "Sparkle" },
          { kind = "Text" },
          { kind = "Text", text = 42 },
          { kind = "Row" },
          { kind = "Row", children = "nope" },
          { kind = "Column", children = { [2] = { kind = "Text", text = "gap" } } },
          { kind = "Row", children = { [1] = { kind = "Text", text = "x" }, note = "mixed" } },
          "not a table",
        }
        local failures = 0
        for _, component in ipairs(cases) do
          local ok, err = pcall(bitty.ui.mount, "top", component)
          if not ok and err.code == "E_UI_COMPONENT_INVALID" then
            failures = failures + 1
          end
        end
        bitty.store.set("failures", failures)
        bitty.store.set("cases", #cases)
    "#,
    );
    assert_eq!(stored(&services, "failures"), stored(&services, "cases"));
    assert!(services.mounts.borrow().is_empty());
}

#[test]
fn depth_sixteen_accepted_seventeen_rejected() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("depth");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local function chain(rows)
          local node = { kind = "Text", text = "leaf" }
          for _ = 1, rows do
            node = { kind = "Row", children = { node } }
          end
          return node
        end
        local ok16 = pcall(bitty.ui.mount, "top", chain(15))
        local ok17, err17 = pcall(bitty.ui.mount, "top", chain(16))
        bitty.store.set("ok16", ok16)
        bitty.store.set("ok17", ok17)
        bitty.store.set("code17", ok17 and "NONE" or err17.code)
    "#,
    );
    assert_eq!(stored(&services, "ok16"), Some(LuaValue::Bool(true)));
    assert_eq!(stored(&services, "ok17"), Some(LuaValue::Bool(false)));
    assert_eq!(
        stored(&services, "code17"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert_eq!(services.mounts.borrow().len(), 1);
}

#[test]
fn oversized_text_and_node_count_rejected() {
    let services = Rc::new(UiServices::default());
    let mut vm = oversized_probe_vm("budgets");
    install(&mut vm, services.clone(), 200);
    run(
        &mut vm,
        &format!(
            r#"
            -- 44 KiB linear chunk appended six times: over SCN-3, inside RC-1.
            local unit_parts = {{}}
            for index = 1, 1024 do unit_parts[index] = "x" end
            local unit = table.concat(unit_parts)
            local chunk_units = {{}}
            for index = 1, 44 do chunk_units[index] = unit end
            local chunk = table.concat(chunk_units)
            local text = chunk
            for _ = 1, 5 do text = text .. chunk end
            local ok_text, err_text = pcall(bitty.ui.mount, "top", {{ kind = "Text", text = text }})
            local children = {{}}
            for index = 1, {children} do
              children[index] = {{ kind = "Text", text = "" }}
            end
            local ok_nodes, err_nodes = pcall(bitty.ui.mount, "top", {{
              kind = "Row", children = children
            }})
            bitty.store.set("text_len", #text)
            bitty.store.set("node_count", #children + 1)
            bitty.store.set("text_code", ok_text and "NONE" or err_text.code)
            bitty.store.set("nodes_code", ok_nodes and "NONE" or err_nodes.code)
            "#,
            children = UI_MAX_NODES,
        ),
    );
    let length = match stored(&services, "text_len") {
        Some(LuaValue::Integer(length)) => length,
        other => panic!("unexpected length {other:?}"),
    };
    assert!(
        length as usize > UI_MAX_TEXT_BYTES,
        "probe text must exceed the SCN-3 ceiling, got {length}"
    );
    assert!(
        length as usize <= UI_MAX_TEXT_BYTES + UI_MARSHAL_BYTE_HEADROOM,
        "probe text must stay inside the marshalling headroom so the SCN-3
         scene walk, not the marshalling ceiling, rejects it; got {length}"
    );
    assert_eq!(
        stored(&services, "node_count"),
        Some(LuaValue::Integer((UI_MAX_NODES + 1) as i64)),
        "probe scene must be exactly one node past the SCN-1 ceiling"
    );
    assert_eq!(
        stored(&services, "text_code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert_eq!(
        stored(&services, "nodes_code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert!(services.mounts.borrow().is_empty());
}

#[test]
fn oversized_update_rejected_before_commit() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("update-budget");
    install(&mut vm, services.clone(), 200);
    run(
        &mut vm,
        r#"
        local handle = bitty.ui.mount("statusline", { kind = "Text", text = "ok" })
        bitty.store.set("handle", handle)
        -- 44 KiB linear chunk appended six times: over SCN-3, inside RC-1.
        local unit_parts = {}
        for index = 1, 1024 do unit_parts[index] = "x" end
        local unit = table.concat(unit_parts)
        local chunk_units = {}
        for index = 1, 44 do chunk_units[index] = unit end
        local chunk = table.concat(chunk_units)
        local text = chunk
        for _ = 1, 5 do text = text .. chunk end
        local ok, err = pcall(bitty.ui.update, handle, { kind = "Text", text = text })
        bitty.store.set("ok", ok)
        bitty.store.set("code", ok and "NONE" or err.code)
        "#,
    );
    assert_eq!(stored(&services, "handle"), Some(LuaValue::Integer(11)));
    assert_eq!(stored(&services, "ok"), Some(LuaValue::Bool(false)));
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert_eq!(services.mounts.borrow().len(), 1);
    assert!(
        services.updates.borrow().is_empty(),
        "an over-budget update must not reach the host"
    );
}

#[test]
fn cyclic_component_fails_closed() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("cyclic");
    install(&mut vm, services.clone(), 200);
    run(
        &mut vm,
        r#"
        local node = { kind = "Row" }
        node.children = { node }
        local ok, err = pcall(bitty.ui.mount, "top", node)
        bitty.store.set("ok", ok)
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(stored(&services, "ok"), Some(LuaValue::Bool(false)));
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert!(services.mounts.borrow().is_empty());
}

#[test]
fn update_rejects_non_integer_handle() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("handle");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.ui.update, "11", { kind = "Text", text = "x" })
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert!(services.updates.borrow().is_empty());
}

#[test]
fn host_without_ui_surface_fails_closed() {
    #[derive(Default)]
    struct Bare {
        store: RefCell<BTreeMap<String, LuaValue>>,
    }
    impl HostServices for Bare {
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
    }

    let services = Rc::new(Bare::default());
    let services_dyn: Rc<dyn HostServices> = services.clone();
    let mut vm = gate_vm("bare");
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 50)
        .expect("install");
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "x" })
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_UI_UNAVAILABLE".to_string()))
    );
}

#[test]
fn capability_denial_propagates_typed() {
    let services = Rc::new(UiServices {
        deny: Some("ui.rich"),
        ..UiServices::default()
    });
    let mut vm = gate_vm("denied");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local mount_ok, mount_err = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "x" })
        local update_ok, update_err = pcall(bitty.ui.update, 11, { kind = "Text", text = "x" })
        bitty.store.set("mount_code", mount_ok and "NONE" or mount_err.code)
        bitty.store.set("update_code", update_ok and "NONE" or update_err.code)
    "#,
    );
    assert_eq!(
        stored(&services, "mount_code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    assert_eq!(
        stored(&services, "update_code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    assert!(services.mounts.borrow().is_empty());
    assert!(services.updates.borrow().is_empty());
}

#[test]
fn expired_mount_returns_timeout_without_commit() {
    let services = Rc::new(UiServices {
        slow_ms: 30,
        ..UiServices::default()
    });
    let mut vm = gate_vm("expired");
    install(&mut vm, services.clone(), 5);
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.ui.mount, "statusline", { kind = "Text", text = "x" })
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_TIMEOUT".to_string()))
    );
    assert!(
        services.mounts.borrow().is_empty(),
        "an expired mount must not commit a block"
    );
}
