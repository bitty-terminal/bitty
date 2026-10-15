//! `bitty.terminal.submit` bridge tests (CTX-1239, issue #2101).
//!
//! The production Lua bridge served `snapshot` only, so every composer
//! `submit_now()` hit `unavailable:no-submit` headed while the mock harness
//! stayed green. This suite pins the bridged surface:
//!
//! - `submit` is present in `bitty.terminal` only when the host generations
//!   probes `has_terminal_submit()` (grant plus wired backend); otherwise
//!   the key is nil, indistinguishable from an absent surface.
//! - a granted call forwards its text to the host and returns the host
//!   outcome table verbatim (`accepted`/`denied`/`unavailable` shapes the
//!   composer policy switches on).
//! - a non-string argument fails closed with `E_DEF_INVALID` before any
//!   host call.
//! - hosts that do not override the new trait methods observe nil (the
//!   fail-closed default), so no new ambient authority appears.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits};

fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

fn accepted_outcome(bytes: i64) -> LuaValue {
    LuaValue::table([
        ("status", LuaValue::String("accepted".to_string())),
        ("bytes", LuaValue::Integer(bytes)),
    ])
}

fn denied_outcome(deny: &str) -> LuaValue {
    LuaValue::table([
        ("status", LuaValue::String("denied".to_string())),
        ("deny", LuaValue::String(deny.to_string())),
    ])
}

/// Host fake with an explicit submit presence probe and canned outcome.
struct SubmitServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    present: bool,
    outcome: RefCell<LuaValue>,
    calls: RefCell<Vec<String>>,
}

impl SubmitServices {
    fn new(present: bool, outcome: LuaValue) -> Self {
        Self {
            store: RefCell::new(BTreeMap::new()),
            present,
            outcome: RefCell::new(outcome),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl HostServices for SubmitServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        if matches!(value, LuaValue::Nil) {
            self.store.borrow_mut().remove(key);
        } else {
            self.store.borrow_mut().insert(key.to_string(), value);
        }
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

    fn has_terminal_submit(&self) -> bool {
        self.present
    }

    fn terminal_submit(&self, text: &str) -> Result<LuaValue, BridgeError> {
        self.calls.borrow_mut().push(text.to_string());
        Ok(self.outcome.borrow().clone())
    }
}

/// Host fake that never overrides the new submit methods: the fail-closed
/// defaults must leave the surface absent with no ambient authority.
#[derive(Default)]
struct LegacyServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl HostServices for LegacyServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        if matches!(value, LuaValue::Nil) {
            self.store.borrow_mut().remove(key);
        } else {
            self.store.borrow_mut().insert(key.to_string(), value);
        }
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

fn install(vm: &mut LuaVm, services: Rc<dyn HostServices>) {
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn store_string(services: &SubmitServices, key: &str) -> Option<String> {
    match services.store.borrow().get(key)? {
        LuaValue::String(text) => Some(text.clone()),
        LuaValue::Integer(value) => Some(value.to_string()),
        LuaValue::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

#[test]
fn submit_present_and_accepts_when_probed() {
    let services = Rc::new(SubmitServices::new(true, accepted_outcome(19)));
    let mut vm = gate_vm("submit-present");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            bitty.store.set("kind", type(bitty.terminal.submit))
            local result = bitty.terminal.submit("cargo test")
            bitty.store.set("status", result.status)
            bitty.store.set("bytes", result.bytes)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(store_string(&services, "kind").as_deref(), Some("function"));
    assert_eq!(
        store_string(&services, "status").as_deref(),
        Some("accepted")
    );
    assert_eq!(store_string(&services, "bytes").as_deref(), Some("19"));
    assert_eq!(services.calls.borrow().as_slice(), &["cargo test"]);
}

#[test]
fn submit_absent_when_probe_false() {
    let services = Rc::new(SubmitServices::new(false, accepted_outcome(19)));
    let mut vm = gate_vm("submit-absent");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            bitty.store.set("submit_nil", bitty.terminal.submit == nil)
            bitty.store.set("snapshot_kind", type(bitty.terminal.snapshot))
            local ok = pcall(function() return bitty.terminal.submit("cargo test") end)
            bitty.store.set("call_fails", not ok)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        store_string(&services, "submit_nil").as_deref(),
        Some("true")
    );
    assert_eq!(
        store_string(&services, "snapshot_kind").as_deref(),
        Some("function"),
        "snapshot stays while submit is absent"
    );
    assert_eq!(
        store_string(&services, "call_fails").as_deref(),
        Some("true")
    );
    assert!(
        services.calls.borrow().is_empty(),
        "an absent surface must never reach the host"
    );
}

#[test]
fn submit_rejects_non_string_shape() {
    let services = Rc::new(SubmitServices::new(true, accepted_outcome(1)));
    let mut vm = gate_vm("submit-shape");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.terminal.submit, 42)
            bitty.store.set("failed", not ok)
            if not ok then bitty.store.set("code", err.code) end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(store_string(&services, "failed").as_deref(), Some("true"));
    assert_eq!(
        store_string(&services, "code").as_deref(),
        Some("E_DEF_INVALID")
    );
    assert!(
        services.calls.borrow().is_empty(),
        "a shape refusal must never reach the host"
    );
}

#[test]
fn submit_forwards_denied_outcome_verbatim() {
    let services = Rc::new(SubmitServices::new(true, denied_outcome("too-large")));
    let mut vm = gate_vm("submit-denied");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local result = bitty.terminal.submit("q")
            bitty.store.set("status", result.status)
            bitty.store.set("deny", result.deny)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(store_string(&services, "status").as_deref(), Some("denied"));
    assert_eq!(
        store_string(&services, "deny").as_deref(),
        Some("too-large")
    );
}

#[test]
fn submit_absent_by_default_for_legacy_hosts() {
    // A host that never overrides the new trait methods gains no submit
    // surface: the defaults fail closed with the key absent.
    let services = Rc::new(LegacyServices::default());
    let mut vm = gate_vm("submit-legacy");
    let services_dyn: Rc<dyn HostServices> = services.clone();
    install(&mut vm, services_dyn);
    let outcome = vm
        .execute_bounded(r#"bitty.store.set("legacy_nil", bitty.terminal.submit == nil)"#)
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("legacy_nil"),
        Some(&LuaValue::Bool(true))
    );
}
