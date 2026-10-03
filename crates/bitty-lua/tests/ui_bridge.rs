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
    BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits, OverlayInput,
    UiNode,
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
    overlay_deny: Option<&'static str>,
    overlay_acquires: RefCell<Vec<i64>>,
    overlay_releases: RefCell<Vec<i64>>,
    overlay_polls: RefCell<Vec<(i64, usize)>>,
    overlay_events: RefCell<Vec<OverlayInput>>,
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

    fn ui_overlay_acquire(&self, handle: i64) -> Result<(), BridgeError> {
        if let Some(capability) = self.overlay_deny {
            return Err(BridgeError::capability_denied(capability));
        }
        self.overlay_acquires.borrow_mut().push(handle);
        Ok(())
    }

    fn ui_overlay_release(&self, handle: i64) -> Result<bool, BridgeError> {
        self.overlay_releases.borrow_mut().push(handle);
        Ok(true)
    }

    fn ui_overlay_poll(&self, handle: i64, max: usize) -> Result<Vec<OverlayInput>, BridgeError> {
        self.overlay_polls.borrow_mut().push((handle, max));
        let take = max.min(self.overlay_events.borrow().len());
        Ok(self.overlay_events.borrow_mut().drain(..take).collect())
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

/// CTX-0941: the focusable-overlay capture surface is present, read-only, and
/// routes acquire/release/poll through the shared host boundary. The plugin
/// observes captured input only through `poll`; no callback is registered on
/// the input path.
#[test]
fn overlay_capture_surface_round_trips_through_host() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("overlay-capture");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        bitty.store.set("overlay_type", type(bitty.ui.overlay))
        local acquire_ok, acquire_err = pcall(bitty.ui.overlay.acquire, 11)
        local release_ok, release_err = pcall(bitty.ui.overlay.release, 11)
        local events = bitty.ui.overlay.poll(11, 4)
        bitty.store.set("acquire_ok", acquire_ok)
        bitty.store.set("release_ok", release_ok)
        bitty.store.set("poll_count", #events)
        bitty.store.set("acquire_code", acquire_ok and "NONE" or acquire_err.code)
        bitty.store.set("release_code", release_ok and "NONE" or release_err.code)
    "#,
    );
    assert_eq!(
        stored(&services, "overlay_type"),
        Some(LuaValue::String("table".to_string()))
    );
    assert_eq!(stored(&services, "acquire_ok"), Some(LuaValue::Bool(true)));
    assert_eq!(stored(&services, "release_ok"), Some(LuaValue::Bool(true)));
    assert_eq!(stored(&services, "poll_count"), Some(LuaValue::Integer(0)));
    assert_eq!(services.overlay_acquires.borrow().as_slice(), &[11]);
    assert_eq!(services.overlay_releases.borrow().as_slice(), &[11]);
    assert_eq!(services.overlay_polls.borrow().as_slice(), &[(11, 4)]);
}

/// CTX-0941: `bitty.ui.overlay` is nested in the read-only `bitty.ui` table.
#[test]
fn overlay_capture_table_is_read_only() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("overlay-readonly");
    install(&mut vm, services, 50);
    for chunk in [
        "bitty.ui.overlay = {}",
        "bitty.ui.overlay.acquire = function() end",
    ] {
        let outcome = vm.execute_bounded(chunk).expect("execute");
        assert!(
            matches!(outcome, BoundedExecution::RuntimeError(_)),
            "{chunk}: assignment must fail: {outcome:?}"
        );
    }
}

/// CTX-0941: captured input arrives as bounded `{sequence, kind, text}` rows.
#[test]
fn overlay_poll_returns_bounded_events() {
    let services = Rc::new(UiServices::default());
    services.overlay_events.borrow_mut().extend([
        OverlayInput {
            sequence: 1,
            kind: "text".to_string(),
            text: "hello".to_string(),
        },
        OverlayInput {
            sequence: 2,
            kind: "key".to_string(),
            text: "enter".to_string(),
        },
    ]);
    let mut vm = gate_vm("overlay-events");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local first = bitty.ui.overlay.poll(11)
        bitty.store.set("n", #first)
        bitty.store.set("kind1", first[1].kind)
        bitty.store.set("text1", first[1].text)
        bitty.store.set("seq1", first[1].sequence)
    "#,
    );
    assert_eq!(stored(&services, "n"), Some(LuaValue::Integer(2)));
    assert_eq!(
        stored(&services, "kind1"),
        Some(LuaValue::String("text".to_string()))
    );
    assert_eq!(
        stored(&services, "text1"),
        Some(LuaValue::String("hello".to_string()))
    );
    assert_eq!(stored(&services, "seq1"), Some(LuaValue::Integer(1)));
}

/// CTX-0941: a non-integer overlay handle is a typed component error before
/// any host call.
#[test]
fn overlay_rejects_non_integer_handle() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("overlay-handle");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.ui.overlay.acquire, "11")
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert!(services.overlay_acquires.borrow().is_empty());
}

/// CTX-0941: a host without a capture backend fails closed typed, and the
/// capture grant denial propagates typed.
#[test]
fn overlay_capture_fails_closed_without_grant_or_backend() {
    let denied = Rc::new(UiServices {
        overlay_deny: Some("ui.overlay"),
        ..UiServices::default()
    });
    let mut vm = gate_vm("overlay-denied");
    install(&mut vm, denied.clone(), 50);
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.ui.overlay.acquire, 11)
        bitty.store.set("code", ok and "NONE" or err.code)
    "#,
    );
    assert_eq!(
        stored(&denied, "code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );

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
    let mut vm = gate_vm("overlay-bare");
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 50)
        .expect("install");
    run(
        &mut vm,
        r#"
        local acquire_ok, acquire_err = pcall(bitty.ui.overlay.acquire, 11)
        local release_ok, release_err = pcall(bitty.ui.overlay.release, 11)
        local poll_ok, poll_err = pcall(bitty.ui.overlay.poll, 11)
        bitty.store.set("acquire_code", acquire_ok and "NONE" or acquire_err.code)
        bitty.store.set("release_code", release_ok and "NONE" or release_err.code)
        bitty.store.set("poll_code", poll_ok and "NONE" or poll_err.code)
    "#,
    );
    for key in ["acquire_code", "release_code", "poll_code"] {
        assert_eq!(
            services.store.borrow().get(key),
            Some(&LuaValue::String("E_UI_UNAVAILABLE".to_string())),
            "{key}"
        );
    }
}

/// CTX-0942: the `bitty.ui.targets`/`bitty.ui.labels` thin surface is present
/// under the existing `bitty.ui` namespace (no `bitty.beacon.*` namespace),
/// nested in the read-only root, and fails closed typed on a host with no
/// targeting backend. The surface exposes no event/observe entry point.
#[test]
fn targets_surface_fails_closed_and_has_no_event_bus() {
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
    let mut vm = gate_vm("targets-bare");
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 50)
        .expect("install");
    run(
        &mut vm,
        r#"
        bitty.store.set("targets_type", type(bitty.ui.targets))
        bitty.store.set("labels_type", type(bitty.ui.labels))
        bitty.store.set("beacon_nil", bitty.beacon == nil)
        local snapshot_ok, snapshot_err = pcall(bitty.ui.targets.snapshot, 4)
        local dispatch_ok, dispatch_err = pcall(bitty.ui.targets.dispatch, "a")
        local assign_ok, assign_err = pcall(bitty.ui.labels.assign, {}, 80)
        bitty.store.set("snapshot_code", snapshot_ok and "NONE" or snapshot_err.code)
        bitty.store.set("dispatch_code", dispatch_ok and "NONE" or dispatch_err.code)
        bitty.store.set("assign_code", assign_ok and "NONE" or assign_err.code)
        local has_event_api = bitty.ui.targets.events ~= nil
            or bitty.ui.targets.subscribe ~= nil
            or bitty.ui.targets.observe ~= nil
        bitty.store.set("has_event_api", has_event_api)
    "#,
    );
    assert_eq!(
        services.store.borrow().get("targets_type"),
        Some(&LuaValue::String("table".to_string()))
    );
    assert_eq!(
        services.store.borrow().get("labels_type"),
        Some(&LuaValue::String("table".to_string()))
    );
    assert_eq!(
        services.store.borrow().get("beacon_nil"),
        Some(&LuaValue::Bool(true))
    );
    for key in ["snapshot_code", "dispatch_code", "assign_code"] {
        assert_eq!(
            services.store.borrow().get(key),
            Some(&LuaValue::String("E_UI_UNAVAILABLE".to_string())),
            "{key}"
        );
    }
    assert_eq!(
        services.store.borrow().get("has_event_api"),
        Some(&LuaValue::Bool(false))
    );
}

/// CTX-0942: `bitty.ui.targets` and `bitty.ui.labels` are read-only.
#[test]
fn targets_tables_are_read_only() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("targets-readonly");
    install(&mut vm, services, 50);
    for chunk in [
        "bitty.ui.targets = {}",
        "bitty.ui.targets.snapshot = function() end",
        "bitty.ui.targets.dispatch = 1",
        "bitty.ui.labels.assign = nil",
    ] {
        let outcome = vm.execute_bounded(chunk).expect("execute");
        assert!(
            matches!(outcome, BoundedExecution::RuntimeError(_)),
            "{chunk}: assignment must fail: {outcome:?}"
        );
    }
}

/// CTX-0942: malformed `bitty.ui.targets` arguments are typed validation
/// errors raised before any host call, and the reserved `core` tier is not
/// registrable from Lua.
#[test]
fn targets_argument_validation_is_typed() {
    let services = Rc::new(UiServices::default());
    let mut vm = gate_vm("targets-validation");
    install(&mut vm, services.clone(), 50);
    run(
        &mut vm,
        r#"
        local start_ok, start_err =
            pcall(bitty.ui.targets.session_start, "11", 80, {}, {})
        local dispatch_ok, dispatch_err = pcall(bitty.ui.targets.dispatch, 1)
        local core_ok, core_err = pcall(bitty.ui.targets.register, {
            name = "acme.links", tier = "core", targets = {},
        })
        local kind_ok, kind_err = pcall(bitty.ui.targets.register, {
            name = "acme.links", tier = "plugin",
            targets = { { kind = "view", id = 1 } },
        })
        bitty.store.set("start_code", start_ok and "NONE" or start_err.code)
        bitty.store.set("dispatch_code", dispatch_ok and "NONE" or dispatch_err.code)
        bitty.store.set("core_code", core_ok and "NONE" or core_err.code)
        bitty.store.set("kind_code", kind_ok and "NONE" or kind_err.code)
    "#,
    );
    // Shape errors use the existing UI component code; semantic tier/kind
    // rejections use the existing definition code.
    assert_eq!(
        stored(&services, "start_code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert_eq!(
        stored(&services, "dispatch_code"),
        Some(LuaValue::String("E_UI_COMPONENT_INVALID".to_string()))
    );
    assert_eq!(
        stored(&services, "core_code"),
        Some(LuaValue::String("E_DEF_INVALID".to_string()))
    );
    assert_eq!(
        stored(&services, "kind_code"),
        Some(LuaValue::String("E_DEF_INVALID".to_string()))
    );
}
