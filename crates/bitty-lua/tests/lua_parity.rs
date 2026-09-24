//! Lua bridge parity suite for the accepted v1 surface (CTX-0707, ADR 0009).
//!
//! The SDK recon (CTX-0705 §1.3) found the `bitty-lua` bridge behind the
//! accepted spellings: `keymaps`, `services`, `tasks`, and `env` had no
//! tables at all. This suite pins the per-namespace verdicts:
//!
//! - `keymaps.suggest` — WIRED as a bridge capture (LUA-OQ-5).
//! - `tasks.spawn`/`cancel` — WIRED as a bridge capture (LUA-OQ-9, RC-4).
//! - `services.get`/`provide` — WIRED to the host backend (LUA-OQ-8):
//!   shape-validated and captured at the bridge, resolved/published/called
//!   through `HostServices::service_resolve`/`service_provide_check`/
//!   `service_call`. Hosts without a backend keep the typed
//!   `E_NOT_IMPLEMENTED` default, pinned below.
//! - `env.get`/`has` — GRANT-GATED via `HostServices::env_get`/`env_has`
//!   (CTX-0330, ADR 0006): shape-validated at the bridge, `E_NOT_IMPLEMENTED`
//!   until an `env.read:<KEY>` grant exists, values only for granted keys.
//! - `process.spawn` — v1-OUT ruling: kept serving (CTX-0445 consent-gated
//!   extra) but outside the v1 API guarantee.
//!
//! Follow-ups own the remaining host backends (keymap application, task
//! scheduling) and the SDK `pending-host` flags.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits,
    REGISTRATION_MAX_KEYMAP_SUGGESTIONS, REGISTRATION_MAX_TASKS, RegistrationCapture,
};

/// Gate-built VM with default RC budgets (replaces deprecated `LuaVm::new`).
fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

#[derive(Default)]
struct ParityServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl HostServices for ParityServices {
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

fn install(vm: &mut LuaVm, services: Rc<ParityServices>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

/// Run `call` (a Lua expression) under `pcall`, recording `err.code` /
/// `err.class` (or `"NONE"` on success) into the store under `code`/`class`.
fn record_call(vm: &mut LuaVm, call: &str) {
    let outcome = vm
        .execute_bounded(&format!(
            r#"
            local ok, err = pcall(function() return {call} end)
            if ok then
                bitty.store.set("code", "NONE")
                bitty.store.set("class", "NONE")
            else
                bitty.store.set("code", err.code)
                bitty.store.set("class", err.class)
            end
        "#
        ))
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{call}: chunk must complete via pcall: {outcome:?}"
    );
}

fn recorded(services: &Rc<ParityServices>, key: &str) -> Option<LuaValue> {
    services.store.borrow().get(key).cloned()
}

fn assert_code(services: &Rc<ParityServices>, want_code: &str, want_class: &str, what: &str) {
    assert_eq!(
        recorded(services, "code"),
        Some(LuaValue::String(want_code.to_string())),
        "{what}: typed code"
    );
    assert_eq!(
        recorded(services, "class"),
        Some(LuaValue::String(want_class.to_string())),
        "{what}: diagnostic class"
    );
}

#[test]
fn parity_namespaces_present_with_accepted_spellings() {
    let mut vm = gate_vm("parity-shape");
    install(&mut vm, Rc::new(ParityServices::default()));
    let outcome = vm
        .execute_bounded(
            r#"
            assert(type(bitty.keymaps) == "table")
            assert(type(bitty.keymaps.suggest) == "function")
            assert(type(bitty.services) == "table")
            assert(type(bitty.services.get) == "function")
            assert(type(bitty.services.provide) == "function")
            assert(type(bitty.tasks) == "table")
            assert(type(bitty.tasks.spawn) == "function")
            assert(type(bitty.tasks.cancel) == "function")
            assert(type(bitty.env) == "table")
            assert(type(bitty.env.get) == "function")
            assert(type(bitty.env.has) == "function")
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "accepted spellings must be present: {outcome:?}"
    );
}

#[test]
fn parity_tables_are_read_only() {
    let mut vm = gate_vm("parity-readonly");
    install(&mut vm, Rc::new(ParityServices::default()));
    for chunk in [
        "bitty.keymaps = {}",
        "bitty.keymaps.suggest = 1",
        "bitty.services = {}",
        "bitty.tasks = {}",
        "bitty.tasks.spawn = 1",
        "bitty.env = {}",
    ] {
        let outcome = vm.execute_bounded(chunk).expect("execute");
        assert!(
            matches!(outcome, BoundedExecution::RuntimeError(_)),
            "{chunk}: assignment must fail: {outcome:?}"
        );
    }
}

#[test]
fn keymaps_suggest_captures_with_global_default() {
    let mut vm = gate_vm("parity-keymaps");
    install(&mut vm, Rc::new(ParityServices::default()));
    let outcome = vm
        .execute_bounded(
            r#"
            local h1 = bitty.keymaps.suggest({ chord = "ctrl+k", command = "palette:open" })
            local h2 = bitty.keymaps.suggest({ chord = "ctrl+s", command = "palette:save", when = "global" })
            bitty.store.set("h1", h1)
            bitty.store.set("h2", h2)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let capture = vm.take_registrations();
    assert_eq!(capture.keymaps.len(), 2);
    assert_eq!(capture.keymaps[0].chord, "ctrl+k");
    assert_eq!(capture.keymaps[0].command, "palette:open");
    // Absent `when` normalizes to the only v1 context.
    assert_eq!(capture.keymaps[0].when, "global");
    assert_eq!(capture.keymaps[1].chord, "ctrl+s");
    assert_eq!(capture.keymaps[1].when, "global");
}

#[test]
fn keymaps_suggest_rejects_malformed() {
    let long_chord = "k".repeat(129);
    let long_command = "c".repeat(129);
    let cases = [
        (
            "non-table",
            "bitty.keymaps.suggest(\"ctrl+k\")".to_string(),
        ),
        (
            "missing-chord",
            "bitty.keymaps.suggest({ command = \"palette:open\" })".to_string(),
        ),
        (
            "empty-chord",
            "bitty.keymaps.suggest({ chord = \"\", command = \"palette:open\" })".to_string(),
        ),
        (
            "missing-command",
            "bitty.keymaps.suggest({ chord = \"ctrl+k\" })".to_string(),
        ),
        (
            "bad-when",
            "bitty.keymaps.suggest({ chord = \"ctrl+k\", command = \"palette:open\", when = \"editor\" })"
                .to_string(),
        ),
        (
            "long-chord",
            format!("bitty.keymaps.suggest({{ chord = \"{long_chord}\", command = \"palette:open\" }})"),
        ),
        (
            "long-command",
            format!("bitty.keymaps.suggest({{ chord = \"ctrl+k\", command = \"{long_command}\" }})"),
        ),
    ];
    for (tag, call) in cases {
        let services = Rc::new(ParityServices::default());
        let mut vm = gate_vm(format!("parity-keymaps-bad-{tag}"));
        install(&mut vm, services.clone());
        record_call(&mut vm, &call);
        assert_code(&services, "E_DEF_INVALID", "validation", tag);
        assert!(
            vm.take_registrations().keymaps.is_empty(),
            "{tag}: rejected suggestion must not capture"
        );
    }
}

#[test]
fn keymaps_suggest_enforces_cap_at_bridge() {
    let services = Rc::new(ParityServices::default());
    let mut vm = gate_vm("parity-keymaps-cap");
    install(&mut vm, services.clone());
    let mut body = String::new();
    for i in 0..REGISTRATION_MAX_KEYMAP_SUGGESTIONS {
        body.push_str(&format!(
            "bitty.keymaps.suggest({{ chord = \"ctrl+k{i}\", command = \"palette:cmd{i}\" }})\n"
        ));
    }
    let outcome = vm.execute_bounded(&body).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "at-cap suggestions must complete: {outcome:?}"
    );
    assert_eq!(
        vm.take_registrations().keymaps.len(),
        REGISTRATION_MAX_KEYMAP_SUGGESTIONS
    );
    // Handles are dense 1-based suggestion indices.
    record_call(
        &mut vm,
        "bitty.keymaps.suggest({ chord = \"ctrl+z\", command = \"palette:extra\" })",
    );
    assert_code(&services, "E_DEF_LIMIT", "validation", "over-cap");
    assert_eq!(
        vm.take_registrations().keymaps.len(),
        REGISTRATION_MAX_KEYMAP_SUGGESTIONS
    );
}

#[test]
fn tasks_spawn_cancel_round_trip() {
    let mut vm = gate_vm("parity-tasks");
    install(&mut vm, Rc::new(ParityServices::default()));
    let outcome = vm
        .execute_bounded(
            r#"
            local h1 = bitty.tasks.spawn(function() return true end)
            local h2 = bitty.tasks.spawn(function() return false end)
            bitty.store.set("h1", h1)
            bitty.store.set("h2", h2)
            bitty.store.set("cancelled", bitty.tasks.cancel(h1))
            bitty.store.set("cancelled_again", bitty.tasks.cancel(h1))
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let capture = vm.take_registrations();
    // Cancel releases the slot: only the second spawn stays captured.
    assert_eq!(capture.tasks.len(), 1);
    assert_eq!(capture.tasks[0].handle, 2);
    // The stashed entry is a live generation-scoped function.
    let entry = capture.tasks[0].entry.clone();
    let result = vm.call_function(&entry, &[]).expect("call entry");
    assert_eq!(result, LuaValue::Bool(false));
}

#[test]
fn tasks_spawn_rejects_malformed() {
    for (tag, call) in [
        ("non-function", "bitty.tasks.spawn(\"nope\")"),
        ("missing", "bitty.tasks.spawn()"),
        ("non-integer-cancel", "bitty.tasks.cancel(\"1\")"),
    ] {
        let services = Rc::new(ParityServices::default());
        let mut vm = gate_vm(format!("parity-tasks-bad-{tag}"));
        install(&mut vm, services.clone());
        record_call(&mut vm, call);
        assert_code(&services, "E_DEF_INVALID", "validation", tag);
        assert!(
            vm.take_registrations().tasks.is_empty(),
            "{tag}: rejected spawn must not capture"
        );
    }
}

#[test]
fn tasks_spawn_enforces_64_cap_with_budget_code() {
    let services = Rc::new(ParityServices::default());
    let mut vm = gate_vm("parity-tasks-cap");
    install(&mut vm, services.clone());
    let mut body = String::new();
    for _ in 0..REGISTRATION_MAX_TASKS {
        body.push_str("bitty.tasks.spawn(function() end)\n");
    }
    let outcome = vm.execute_bounded(&body).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "at-cap spawns must complete: {outcome:?}"
    );
    assert_eq!(vm.take_registrations().tasks.len(), REGISTRATION_MAX_TASKS);
    // Dense handles from 1 with no aliasing.
    let mut handles: Vec<i64> = vm
        .take_registrations()
        .tasks
        .iter()
        .map(|t| t.handle)
        .collect();
    handles.sort_unstable();
    assert_eq!(
        handles,
        (1..=REGISTRATION_MAX_TASKS as i64).collect::<Vec<_>>()
    );
    // The 65th spawn fails closed with the accepted RC-4 budget code, and
    // cancelling one slot lets the next spawn through.
    record_call(&mut vm, "bitty.tasks.spawn(function() end)");
    assert_code(&services, "E_BUDGET_TASK", "budget", "over-cap");
    assert_eq!(vm.take_registrations().tasks.len(), REGISTRATION_MAX_TASKS);
    let outcome = vm
        .execute_bounded("bitty.store.set(\"freed\", bitty.tasks.cancel(1))")
        .expect("execute");
    assert!(matches!(outcome, BoundedExecution::Completed));
    let outcome = vm
        .execute_bounded("bitty.store.set(\"h\", bitty.tasks.spawn(function() end))")
        .expect("execute");
    assert!(matches!(outcome, BoundedExecution::Completed));
    assert_eq!(vm.take_registrations().tasks.len(), REGISTRATION_MAX_TASKS);
}

#[test]
fn task_handle_allocation_is_checked_not_wrapping() {
    // Unit-level: the task handle counter fails closed at exhaustion instead
    // of wrapping (release-mode `+= 1` would alias handle 1 onto a live
    // task). `i64::MAX` is the reserved exhaustion sentinel, never issued.
    let mut capture = RegistrationCapture::new();
    capture.next_task_handle = i64::MAX - 1;
    assert_eq!(capture.alloc_task_handle(), Some(i64::MAX - 1));
    assert_eq!(capture.next_task_handle, i64::MAX);
    assert_eq!(
        capture.alloc_task_handle(),
        None,
        "exhausted handle space must be signalled, not wrapped"
    );
    assert_eq!(capture.next_task_handle, i64::MAX);
    assert_eq!(
        capture.alloc_task_handle(),
        None,
        "exhaustion must be sticky"
    );
}

#[test]
fn services_get_provide_are_not_implemented() {
    for (tag, call) in [
        ("get", "bitty.services.get(\"iface\", {})"),
        ("provide", "bitty.services.provide(\"iface\", {})"),
    ] {
        let services = Rc::new(ParityServices::default());
        let mut vm = gate_vm(format!("parity-services-{tag}"));
        install(&mut vm, services.clone());
        record_call(&mut vm, call);
        assert_code(&services, "E_NOT_IMPLEMENTED", "runtime", tag);
    }
}

#[test]
fn env_get_has_are_not_implemented() {
    for (tag, call) in [
        ("get", "bitty.env.get(\"HOME\")"),
        ("has", "bitty.env.has(\"HOME\")"),
    ] {
        let services = Rc::new(ParityServices::default());
        let mut vm = gate_vm(format!("parity-env-{tag}"));
        install(&mut vm, services.clone());
        record_call(&mut vm, call);
        assert_code(&services, "E_NOT_IMPLEMENTED", "runtime", tag);
    }
}

/// Grant-aware stub backend for the CTX-0330 env seam: only `granted`
/// keys resolve, everything else stays `E_NOT_IMPLEMENTED`.
struct GrantedEnv {
    store: RefCell<BTreeMap<String, LuaValue>>,
    values: BTreeMap<String, String>,
    granted: Vec<String>,
}

impl HostServices for GrantedEnv {
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

    fn env_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        if !self.granted.iter().any(|granted| granted == key) {
            return Err(BridgeError::not_implemented("bitty.env.get"));
        }
        Ok(self.values.get(key).cloned().map(LuaValue::String))
    }

    fn env_has(&self, key: &str) -> Result<bool, BridgeError> {
        if !self.granted.iter().any(|granted| granted == key) {
            return Err(BridgeError::not_implemented("bitty.env.has"));
        }
        Ok(self.values.contains_key(key))
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }
}

fn install_env(vm: &mut LuaVm, services: Rc<GrantedEnv>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

#[test]
fn env_bridge_delegates_to_grant_aware_backend() {
    let services = Rc::new(GrantedEnv {
        store: RefCell::new(BTreeMap::new()),
        values: BTreeMap::from([("HOME".to_string(), "/home/tester".to_string())]),
        granted: vec!["HOME".to_string(), "EMPTY_VAR".to_string()],
    });
    let mut vm = gate_vm("parity-env-granted");
    install_env(&mut vm, services.clone());
    // Granted + present: value crosses; presence is true. Granted but
    // absent from the host environment reads nil/false
    // (absent-unless-declared carve-out).
    let outcome = vm
        .execute_bounded(
            r#"
            assert(bitty.env.get("HOME") == "/home/tester")
            assert(bitty.env.has("HOME") == true)
            assert(bitty.env.get("EMPTY_VAR") == nil)
            assert(bitty.env.has("EMPTY_VAR") == false)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Granted-but-absent reads nil/false (absent-unless-declared carve-out).
    let outcome = vm
        .execute_bounded("assert(bitty.env.get(\"EMPTY_VAR\") == nil)")
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn env_bridge_denies_ungranted_keys_without_leak() {
    let services = Rc::new(GrantedEnv {
        store: RefCell::new(BTreeMap::new()),
        values: BTreeMap::from([("SECRET_KEY".to_string(), "s3cr3t".to_string())]),
        granted: Vec::new(),
    });
    let mut vm = gate_vm("parity-env-denied");
    install_env(&mut vm, services.clone());
    // Unimplemented backend and ungranted key share one code: callers cannot
    // probe which keys exist.
    for (tag, call) in [
        ("get", "bitty.env.get(\"SECRET_KEY\")"),
        ("has", "bitty.env.has(\"SECRET_KEY\")"),
    ] {
        let outcome = vm
            .execute_bounded(&format!(
                r#"
            local ok, err = pcall(function() return {call} end)
            assert(not ok)
            assert(err.code == "E_NOT_IMPLEMENTED")
            assert(err.class == "runtime")
            bitty.store.set("code-{tag}", err.code)
        "#
            ))
            .expect("execute");
        assert!(
            matches!(outcome, BoundedExecution::Completed),
            "{tag}: {outcome:?}"
        );
    }
    assert_eq!(
        services.store.borrow().get("code-get"),
        Some(&LuaValue::String("E_NOT_IMPLEMENTED".to_string()))
    );
}

#[test]
fn env_bridge_rejects_malformed_keys_before_grants() {
    let services = Rc::new(GrantedEnv {
        store: RefCell::new(BTreeMap::new()),
        values: BTreeMap::new(),
        granted: vec!["HOME".to_string()],
    });
    let mut vm = gate_vm("parity-env-shape");
    install_env(&mut vm, services.clone());
    // Non-string, empty, bad-shape, and over-bound keys fail with validation
    // codes before any grant check.
    let outcome = vm
        .execute_bounded(
            r#"
            local function code_of(call)
                local ok, err = pcall(call)
                assert(not ok)
                return err.code
            end
            assert(code_of(function() return bitty.env.get(42) end) == "E_DEF_INVALID")
            assert(code_of(function() return bitty.env.get("") end) == "E_DEF_INVALID")
            assert(code_of(function() return bitty.env.get("has space") end) == "E_DEF_INVALID")
            assert(code_of(function() return bitty.env.get("9LIVES") end) == "E_DEF_INVALID")
            assert(code_of(function() return bitty.env.has(string.rep("A", 129)) end) == "E_DEF_LIMIT")
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn process_spawn_stays_available_but_outside_v1() {
    // CTX-0707 ruling pin: `process.spawn` keeps serving as the consent-gated
    // CTX-0445 extra (default host fails closed typed), but it is v1-OUT —
    // outside the accepted Plugin API v1 guarantee.
    let services = Rc::new(ParityServices::default());
    let mut vm = gate_vm("parity-spawn");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded("assert(type(bitty.process.spawn) == \"function\")")
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    record_call(&mut vm, "bitty.process.spawn({ \"status\" })");
    assert_code(&services, "E_SPAWN_UNAVAILABLE", "runtime", "spawn-default");
}

#[test]
fn not_implemented_constructor_shape() {
    let error = BridgeError::not_implemented("bitty.services.get");
    assert_eq!(error.class, "runtime");
    assert_eq!(error.code, "E_NOT_IMPLEMENTED");
    assert!(
        error.message.contains("bitty.services.get"),
        "message names the deferred item: {}",
        error.message
    );
}
