//! `bitty.component.*` bridge tests (CTX-2254, bitty#2254).
//!
//! The production producer is a two-call surface: `request` validates bounds
//! and enqueues, `poll` reads a completed result. These tests pin the bridge
//! contract — bounds before the host, closed method set, typed classes, and a
//! fail-closed default for hosts without a component backend — without
//! spawning anything.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, COMPONENT_TIMEOUT_DEFAULT_MS, COMPONENT_TIMEOUT_MAX_MS,
    ComponentBridgeRequest, HostServices, LuaValue, LuaVm, MarshallingLimits,
    component_response_value,
};

fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

/// Host fake recording the validated request and returning canned results.
struct ComponentServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    requests: RefCell<Vec<ComponentBridgeRequest>>,
    poll: RefCell<Vec<Result<Option<LuaValue>, BridgeError>>>,
    tickets: Cell<u64>,
}

impl ComponentServices {
    fn new() -> Self {
        Self {
            store: RefCell::new(BTreeMap::new()),
            requests: RefCell::new(Vec::new()),
            poll: RefCell::new(Vec::new()),
            tickets: Cell::new(0),
        }
    }

    fn with_poll(self, results: Vec<Result<Option<LuaValue>, BridgeError>>) -> Self {
        *self.poll.borrow_mut() = results;
        self
    }
}

impl HostServices for ComponentServices {
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
        Err(BridgeError::not_implemented("terminal.snapshot"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Ok(true)
    }

    fn component_request(&self, request: &ComponentBridgeRequest) -> Result<u64, BridgeError> {
        self.requests.borrow_mut().push(request.clone());
        let ticket = self.tickets.get() + 1;
        self.tickets.set(ticket);
        Ok(ticket)
    }

    fn component_poll(&self, _ticket: u64) -> Result<Option<LuaValue>, BridgeError> {
        let mut results = self.poll.borrow_mut();
        assert!(!results.is_empty(), "unexpected poll");
        results.remove(0)
    }
}

/// Host fake that never overrides the component methods (fail-closed default).
#[derive(Default)]
struct LegacyServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl HostServices for LegacyServices {
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
        Err(BridgeError::not_implemented("terminal.snapshot"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Ok(true)
    }
}

fn install<S: HostServices + 'static>(vm: &mut LuaVm, services: Rc<S>) {
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn stored(services: &impl HostServices, key: &str) -> Option<String> {
    match services.store_get(key).ok()? {
        Some(LuaValue::String(text)) => Some(text),
        Some(LuaValue::Integer(value)) => Some(value.to_string()),
        Some(LuaValue::Bool(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn run(vm: &mut LuaVm, source: &str) {
    let outcome = vm.execute_bounded(source).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn request_forwards_validated_fields() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-request");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local ticket = bitty.component.request({
          id = "net",
          method = "POST",
          url = "https://api.example.com/v1/items",
          headers = { ["content-type"] = "application/json" },
          body = "{\"n\":1}",
        })
        bitty.store.set("ticket", ticket)
        bitty.store.set("kind", type(bitty.component.poll))
    "#,
    );
    assert_eq!(stored(&*services, "ticket").as_deref(), Some("1"));
    assert_eq!(stored(&*services, "kind").as_deref(), Some("function"));
    let requests = services.requests.borrow();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.component, "net");
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(request.url, "https://api.example.com/v1/items");
    assert_eq!(
        request.headers,
        vec![("content-type".to_string(), "application/json".to_string())]
    );
    assert_eq!(request.body.as_deref(), Some("{\"n\":1}"));
    assert_eq!(request.timeout_ms, COMPONENT_TIMEOUT_DEFAULT_MS);
}

#[test]
fn request_rejects_unknown_method_before_the_host() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-method");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.component.request, {
          id = "net", method = "TRACE", url = "https://api.example.com/",
        })
        bitty.store.set("failed", not ok)
        bitty.store.set("code", err.code)
    "#,
    );
    assert_eq!(stored(&*services, "failed").as_deref(), Some("true"));
    assert_eq!(stored(&*services, "code").as_deref(), Some("E_DEF_INVALID"));
    assert!(services.requests.borrow().is_empty());
}

#[test]
fn request_rejects_malformed_urls_before_the_host() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-url");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local function code(spec)
          local ok, err = pcall(bitty.component.request, spec)
          if ok then return "ok" end
          return err.code
        end
        bitty.store.set("ftp", code({ id = "net", method = "GET", url = "ftp://api.example.com/x" }))
        bitty.store.set("userinfo", code({ id = "net", method = "GET", url = "https://user:pw@api.example.com/x" }))
        bitty.store.set("relative", code({ id = "net", method = "GET", url = "/x" }))
        bitty.store.set("control", code({ id = "net", method = "GET", url = "https://api.example.com/a\nb" }))
        bitty.store.set("id_upper", code({ id = "Net", method = "GET", url = "https://api.example.com/x" }))
      "#,
    );
    for key in ["ftp", "userinfo", "relative", "control", "id_upper"] {
        assert_eq!(
            stored(&*services, key).as_deref(),
            Some("E_DEF_INVALID"),
            "{key} must be refused"
        );
    }
    assert!(services.requests.borrow().is_empty());
}

#[test]
fn request_rejects_an_over_bound_timeout() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-timeout");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        &format!(
            r#"
            local ok, err = pcall(bitty.component.request, {{
              id = "net", method = "GET", url = "https://api.example.com/",
              timeout_ms = {},
            }})
            bitty.store.set("code", err.code)
        "#,
            COMPONENT_TIMEOUT_MAX_MS + 1
        ),
    );
    assert_eq!(stored(&*services, "code").as_deref(), Some("E_DEF_LIMIT"));
    assert!(services.requests.borrow().is_empty());
}

#[test]
fn request_rejects_an_over_bound_payload_before_the_host() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-payload");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.component.request, {
          id = "net", method = "POST", url = "https://api.example.com/",
          body = string.rep("x", 9216),
        })
        bitty.store.set("code", err.code)
    "#,
    );
    assert_eq!(
        stored(&*services, "code").as_deref(),
        Some("E_COMPONENT_PAYLOAD_TOO_LARGE")
    );
    assert!(services.requests.borrow().is_empty());
}

#[test]
fn request_rejects_undecodable_specs() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-shape");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local function code(value)
          local ok, err = pcall(bitty.component.request, value)
          if ok then return "ok" end
          return err.code
        end
        bitty.store.set("scalar", code("net"))
        bitty.store.set("headers", code({
          id = "net", method = "GET", url = "https://api.example.com/",
          headers = { "not-a-table" },
        }))
        bitty.store.set("body", code({
          id = "net", method = "GET", url = "https://api.example.com/", body = 7,
        }))
      "#,
    );
    for key in ["scalar", "headers", "body"] {
        assert_eq!(
            stored(&*services, key).as_deref(),
            Some("E_DEF_INVALID"),
            "{key} must be refused"
        );
    }
    assert!(services.requests.borrow().is_empty());
}

#[test]
fn poll_reports_pending_then_the_bounded_response() {
    let services = Rc::new(ComponentServices::new().with_poll(vec![
        Ok(None),
        Ok(Some(component_response_value(
            200,
            &[("content-type".to_string(), "text/plain".to_string())],
            b"hello",
        ))),
    ]));
    let mut vm = gate_vm("component-poll");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local pending = bitty.component.poll(1)
        bitty.store.set("pending", pending == nil)
        local ready = bitty.component.poll(1)
        bitty.store.set("status", ready.status)
        bitty.store.set("body", ready.body)
        bitty.store.set("bytes", ready.bytes)
        bitty.store.set("untrusted", ready.untrusted)
        bitty.store.set("header", ready.headers[1].name .. "=" .. ready.headers[1].value)
      "#,
    );
    assert_eq!(stored(&*services, "pending").as_deref(), Some("true"));
    assert_eq!(stored(&*services, "status").as_deref(), Some("200"));
    assert_eq!(stored(&*services, "body").as_deref(), Some("hello"));
    assert_eq!(stored(&*services, "bytes").as_deref(), Some("5"));
    assert_eq!(stored(&*services, "untrusted").as_deref(), Some("true"));
    assert_eq!(
        stored(&*services, "header").as_deref(),
        Some("content-type=text/plain")
    );
}

#[test]
fn poll_raises_the_typed_failure_classes() {
    let services = Rc::new(ComponentServices::new().with_poll(vec![
        Err(bitty_lua::component_bridge_error(
            "capability_denied",
            "no grant",
        )),
        Err(bitty_lua::component_bridge_error("budget", "queue full")),
        Err(bitty_lua::component_bridge_error(
            "plugin_not_active",
            "suspended",
        )),
        Ok(Some(component_response_value(200, &[], &[0xff, 0xfe]))),
    ]));
    let mut vm = gate_vm("component-failures");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local function code()
          local ok, err = pcall(bitty.component.poll, 1)
          if ok then return "ok:" .. tostring(err) end
          return err.code
        end
        bitty.store.set("denied", code())
        bitty.store.set("budget", code())
        bitty.store.set("inactive", code())
        local ok, response = pcall(bitty.component.poll, 1)
        bitty.store.set("binary_ok", ok)
        bitty.store.set("binary_bytes", response.bytes)
      "#,
    );
    assert_eq!(
        stored(&*services, "denied").as_deref(),
        Some("E_COMPONENT_CAPABILITY_DENIED")
    );
    assert_eq!(
        stored(&*services, "budget").as_deref(),
        Some("E_COMPONENT_BUDGET")
    );
    assert_eq!(
        stored(&*services, "inactive").as_deref(),
        Some("E_COMPONENT_NOT_ACTIVE")
    );
    assert_eq!(stored(&*services, "binary_ok").as_deref(), Some("true"));
    assert_eq!(
        stored(&*services, "binary_bytes").as_deref(),
        Some("2"),
        "a non-UTF-8 body keeps its real byte count"
    );
}

#[test]
fn poll_rejects_a_non_ticket_argument() {
    let services = Rc::new(ComponentServices::new());
    let mut vm = gate_vm("component-ticket");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.component.poll, "1")
        bitty.store.set("code", err.code)
      "#,
    );
    assert_eq!(stored(&*services, "code").as_deref(), Some("E_DEF_INVALID"));
}

#[test]
fn legacy_host_fails_closed_without_ambient_requests() {
    let services = Rc::new(LegacyServices::default());
    let mut vm = gate_vm("component-legacy");
    install(&mut vm, services.clone());
    run(
        &mut vm,
        r#"
        bitty.store.set("request_kind", type(bitty.component.request))
        bitty.store.set("poll_kind", type(bitty.component.poll))
        local ok, err = pcall(bitty.component.request, {
          id = "net", method = "GET", url = "https://api.example.com/",
        })
        bitty.store.set("request_code", err.code)
        local poll_ok, poll_err = pcall(bitty.component.poll, 1)
        bitty.store.set("poll_code", poll_err.code)
        bitty.store.set("request_failed", not ok)
        bitty.store.set("poll_failed", not poll_ok)
      "#,
    );
    assert_eq!(
        stored(&*services, "request_kind").as_deref(),
        Some("function")
    );
    assert_eq!(stored(&*services, "poll_kind").as_deref(), Some("function"));
    assert_eq!(
        stored(&*services, "request_code").as_deref(),
        Some("E_NOT_IMPLEMENTED")
    );
    assert_eq!(
        stored(&*services, "poll_code").as_deref(),
        Some("E_NOT_IMPLEMENTED")
    );
    assert_eq!(
        stored(&*services, "request_failed").as_deref(),
        Some("true")
    );
    assert_eq!(stored(&*services, "poll_failed").as_deref(), Some("true"));
}
