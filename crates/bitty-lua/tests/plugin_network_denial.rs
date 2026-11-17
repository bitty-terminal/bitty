//! Plugin network denial (bitty#2921, CTX-1865).
//!
//! The security corpus records that a plugin attempting `connect()` has no
//! test proving the refusal, and this target is the Lua half of that cell:
//! a gate-built plugin VM — the same `bitty_lua::gate::build_plugin_vm` the
//! runtime uses — is proved to expose **no** socket or connect binding, both
//! as an ambient global and on the always-present read-only `bitty` host
//! module, and a `require` of a socket-shaped module fails with the typed
//! [`BoundedExecution::RuntimeError`] instead of loading anything.
//!
//! # What this target deliberately does not re-prove
//!
//! The capability and egress layers already have coverage, and it is cited
//! here rather than duplicated:
//!
//! - the plugin-facing typed denial class is `capability_denied`
//!   (`crates/bitty-runtime/tests/component_host.rs:232`
//!   `missing_capability_grant_denies_before_spawn`, and `:705`
//!   `denied_request_records_denied_with_the_named_class`, which records
//!   `ActivationDenial::CapabilityDenied`). `bitty.component.request` is the
//!   only network-shaped call a plugin has, and those two tests drive the
//!   production host to that refusal without spawning anything;
//! - a manifest cannot declare `network.connect:HOST:PORT` without a
//!   covering `[[network.egress]]` entry, or the reverse
//!   (`crates/bitty-plugin-host/src/manifest.rs:4483`
//!   `network_requires_pairing_both_directions`);
//! - Core ships no network egress API and links no network implementation
//!   crate (`crates/bitty-plugin-host/tests/core_boundary.rs:107` and `:175`).
//!
//! # What this target does not claim
//!
//! It is not an acceptance record, and it does not claim an `AC-0xx` id: the
//! daemon corpus currently maps `P0-AC-024` to the MCP/Agent read-only default
//! (`crates/bitty-daemon/tests/agent_readonly.rs:1`) and `P0-AC-023` to child
//! scope limitation (`crates/bitty-daemon/README.md:719`), and which criterion
//! the plugin-denial cell rolls up into is the security reviewer's call. The
//! criterion also wants the runtime-host denial recorded with its class and the
//! live daemon campaign, which stay open on bitty#2921. Run it with
//! `cargo test -p bitty-lua --test plugin_network_denial --offline -- --nocapture`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits};

/// Minimal host services: only the three required methods are implemented,
/// and all of them are inert. The subject of this target is the *surface* a
/// plugin VM exposes, so no test-double decision is allowed to stand in for a
/// denial.
#[derive(Default)]
struct InertServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl HostServices for InertServices {
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

    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(Some(LuaValue::String(key.to_string())))
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }
}

/// A gate-built plugin VM with the production `bitty` host module installed.
fn plugin_vm(id: &str) -> LuaVm {
    let mut vm = build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets");
    let services: Rc<dyn HostServices> = Rc::new(InertServices::default());
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install host module");
    vm
}

/// A private temp module root, removed on drop.
struct ModuleRoot(PathBuf);

impl ModuleRoot {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("bitty-lua-netdenial-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("module root");
        Self(root)
    }
}

impl Drop for ModuleRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `source`, requiring it to complete inside budget.
///
/// Every probe asserts inside Lua (the VM exposes no result channel to an
/// integration test), so a failure surfaces as a typed
/// [`BoundedExecution::RuntimeError`] carrying the probe's own diagnostic.
fn probe(vm: &mut LuaVm, what: &str, source: &str) -> BoundedExecution {
    let outcome = vm.execute_bounded(source).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{what} failed: {outcome:?}"
    );
    outcome
}

#[test]
fn plugin_vm_exposes_no_socket_or_connect_binding() {
    let mut vm = plugin_vm("netdenial.surface");

    // Nothing in the restricted stdlib can open a socket: no ambient
    // socket/tcp/udp/connect name, no `io`, no file loaders, and the
    // preload-only `require` has no filesystem or native search path.
    probe(
        &mut vm,
        "ambient surface",
        r#"
        for _, name in ipairs({ "socket", "sockets", "net", "connect", "tcp", "udp", "io", "loadfile", "dofile" }) do
          assert(_G[name] == nil, "ambient binding " .. name .. " must be absent, got " .. tostring(_G[name]))
        end
        assert(type(package) == "table", "package must exist (narrowed)")
        assert(package.path == "", "package.path must be empty, got " .. tostring(package.path))
        assert(package.cpath == nil, "package.cpath must be absent")
        assert(package.loadlib == nil, "package.loadlib must be absent")
        "#,
    );

    // The `bitty` host module is installed and carries no network namespace
    // either: `component` is the only egress-shaped call.
    probe(
        &mut vm,
        "bitty namespace surface",
        r#"
        assert(type(bitty) == "table", "host module missing")
        assert(bitty.api_version ~= nil, "host module must be versioned")
        for _, head in ipairs({ "net", "socket", "sockets", "connect", "http", "tcp", "udp" }) do
          assert(bitty[head] == nil, "bitty." .. head .. " must be absent")
        end
        -- Control: the namespace really is wired, so the absences above are
        -- not the absence of the whole module.
        assert(type(bitty.component) == "table", "bitty.component must exist")
        assert(type(bitty.component.request) == "function", "bitty.component.request must exist")
        assert(type(bitty.fs) == "table", "bitty.fs must exist")
        assert(type(bitty.process) == "table", "bitty.process must exist")
        "#,
    );
    println!(
        "DENY plugin VM surface: no socket/connect binding in the stdlib, and no network head on the bitty host module"
    );
}

#[test]
fn require_of_a_socket_module_fails_typed() {
    // Core preload-only `require`: no path and no native loader, so a socket
    // module cannot resolve and the failure is a typed runtime error naming
    // the module, with no partial global left behind.
    let mut vm = plugin_vm("netdenial.require");
    for module in ["socket", "socket.core", "lua-socket", "bitty.net"] {
        let head = module.split('.').next().expect("module head");
        probe(
            &mut vm,
            &format!("require({module})"),
            &format!(
                r#"
                local head = "{head}"
                local before = rawget(_G, head)
                local ok, err = pcall(require, "{module}")
                assert(ok == false, "require(\"{module}\") must fail, but it loaded")
                local message = tostring(err)
                assert(string.find(message, "{module}", 1, true) ~= nil,
                       "the refusal must name the module: " .. message)
                assert(rawget(_G, head) == before,
                       "a failed require must neither create nor replace the '" .. head .. "' global")
                "#,
            ),
        );
        println!("DENY require({module}) refused with a typed RuntimeError naming the module");
    }

    // The production rooted source-only `require`: a module that exists on
    // disk still has no socket binding to reach, and the refusal is typed.
    let root = ModuleRoot::new("netmodule");
    std::fs::write(
        root.0.join("attempt.lua"),
        "return { connect_binding = type(connect), socket_binding = type(socket) }\n",
    )
    .expect("plant module");
    // The module root is part of the host-module install, so it is set before
    // the install call (a later `with_module_root` would leave the base
    // preload-only `require` in place).
    let mut vm =
        build_plugin_vm("netdenial.rooted", Some(VmBudgets::default())).expect("default budgets");
    vm.with_module_root(root.0.clone());
    let services: Rc<dyn HostServices> = Rc::new(InertServices::default());
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install host module");
    probe(
        &mut vm,
        "rooted module connect()",
        r#"
        local attempt = require("attempt")
        assert(type(attempt) == "table", "the planted module must load")
        -- Read from inside a loaded module: the sandbox presents no binding.
        assert(attempt.connect_binding == "nil",
               "a loaded module must see no connect binding, saw " .. attempt.connect_binding)
        assert(attempt.socket_binding == "nil",
               "a loaded module must see no socket binding, saw " .. attempt.socket_binding)
        -- And a call through the absent binding is a typed runtime error, not
        -- a silently ignored no-op.
        local ok, err = pcall(function() return connect("example.com", 443) end)
        assert(ok == false, "connect() must not succeed from a plugin")
        local message = tostring(err)
        assert(#message > 0, "the refusal must carry a diagnostic")
        "#,
    );
    println!(
        "DENY rooted source-only require: a loaded module sees connect/socket as nil and a call through the absent binding is a typed RuntimeError"
    );
}
