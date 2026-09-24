//! Host bridge seam tests (RFC `plugin-host-runtime-rfc` Gap A).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits, ServiceRoute,
};

/// Gate-built VM with default RC budgets (replaces deprecated `LuaVm::new`).
fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

#[derive(Default)]
struct FakeServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    settings: RefCell<BTreeMap<String, LuaValue>>,
    notifications: RefCell<Vec<LuaValue>>,
    settings_delay_ms: u64,
    terminal_read: bool,
    platform_notify: bool,
}

impl HostServices for FakeServices {
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
        if self.settings_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.settings_delay_ms));
        }
        Ok(self.settings.borrow().get(key).cloned())
    }

    fn terminal_snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if !self.terminal_read {
            return Err(BridgeError::capability_denied("terminal.semantic-read"));
        }
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))
    }

    fn notify_show(&self, payload: &LuaValue) -> Result<bool, BridgeError> {
        if !self.platform_notify {
            return Err(BridgeError::capability_denied("platform.notify"));
        }
        self.notifications.borrow_mut().push(payload.clone());
        Ok(true)
    }
}

fn install(vm: &mut LuaVm, services: Rc<FakeServices>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-lua-host-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn bitty_namespace_is_read_only() {
    let mut vm = gate_vm("t");
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm.execute_bounded("bitty.settings = {}").expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "assignment must fail: {outcome:?}"
    );

    let mut vm = gate_vm("t2");
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm
        .execute_bounded("bitty.commands.register = 1")
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "function assignment must fail: {outcome:?}"
    );
}

#[test]
fn store_and_settings_round_trip() {
    let services = Rc::new(FakeServices::default());
    services
        .settings
        .borrow_mut()
        .insert("retention_days".to_string(), LuaValue::Integer(7));
    let mut vm = gate_vm("t");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            bitty.store.set("k", { a = 1, b = "two" })
            local value = bitty.store.get("k")
            bitty.store.set("flag", bitty.settings.get("retention_days"))
            result = value.a
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("flag"),
        Some(&LuaValue::Integer(7))
    );
}

#[test]
fn require_resolves_rooted_source_only() {
    let dir = unique_dir("require");
    std::fs::write(dir.join("agg.lua"), "local M = {}; M.v = 42; return M").expect("write module");

    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("t");
    vm.with_module_root(dir.clone());
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(r#"bitty.store.set("out", require("agg").v)"#)
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("out"),
        Some(&LuaValue::Integer(42))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn require_rejects_traversal() {
    let parent = unique_dir("traverse");
    std::fs::write(parent.join("outside.lua"), "return 1").expect("write escape");
    let root = parent.join("root");
    std::fs::create_dir_all(&root).expect("root");
    let mut vm = gate_vm("t");
    vm.with_module_root(root.clone());
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm
        .execute_bounded(r#"require("../outside")"#)
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "traversal must fail: {outcome:?}"
    );
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn registrations_are_captured() {
    let mut vm = gate_vm("t");
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm
        .execute_bounded(
            r#"
            bitty.commands.register({ id = "summary", title = "S", run = function() return "ok" end })
            bitty.events.subscribe("terminal.opened", function() end)
            bitty.events.subscribe("terminal.closed", function() end)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let capture = vm.take_registrations();
    assert_eq!(capture.commands.len(), 1);
    assert_eq!(capture.commands[0].id, "summary");
    assert_eq!(capture.events.len(), 2);
    assert_eq!(capture.events[0].kind, "terminal.opened");
}

#[test]
fn captured_command_is_callable() {
    let mut vm = gate_vm("t");
    install(&mut vm, Rc::new(FakeServices::default()));
    vm.execute_bounded(
        r#"bitty.commands.register({ id = "echo", title = "E", run = function(args) return args.who .. "!" end })"#,
    )
    .expect("execute");
    let run = vm.take_registrations().commands[0].run.clone();
    let args = LuaValue::table([("who", LuaValue::String("bitty".to_string()))]);
    let result = vm.call_function(&run, &[args]).expect("call");
    assert_eq!(result, LuaValue::String("bitty!".to_string()));
}

#[test]
fn capability_absent_fails_closed() {
    let services = Rc::new(FakeServices::default()); // terminal_read = false
    let mut vm = gate_vm("t");
    install(&mut vm, services);
    let outcome = vm
        .execute_bounded(r#"local ok = pcall(bitty.terminal.snapshot, { scope = "semantic" }); bitty.store.set("denied", not ok)"#)
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn host_call_deadline_returns_typed_timeout() {
    let services = Rc::new(FakeServices {
        settings_delay_ms: 30,
        ..FakeServices::default()
    });
    let mut vm = gate_vm("t");
    let services_dyn: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 1)
        .expect("install");
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.settings.get, "x")
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_TIMEOUT".to_string()))
    );
}

struct SpawnServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    calls: RefCell<Vec<Vec<String>>>,
    deny: bool,
}

impl HostServices for SpawnServices {
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

    fn process_spawn(&self, args: &[String]) -> Result<LuaValue, BridgeError> {
        self.calls.borrow_mut().push(args.to_vec());
        if self.deny {
            return Err(BridgeError::capability_denied("process.spawn:git"));
        }
        Ok(LuaValue::table([
            ("output", LuaValue::String("M  staged.lua".to_string())),
            ("stderr", LuaValue::String(String::new())),
            ("truncated", LuaValue::Bool(false)),
            ("exit_code", LuaValue::Integer(0)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }
}

fn install_spawn(vm: &mut LuaVm, services: Rc<SpawnServices>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

#[test]
fn process_spawn_unavailable_by_default() {
    // FakeServices does not override process_spawn: the default fails closed
    // with E_SPAWN_UNAVAILABLE (the CTX-0400 git-panel gap).
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("spawn-unavailable");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.process.spawn, { "status", "--porcelain" })
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_SPAWN_UNAVAILABLE".to_string()))
    );
}

#[test]
fn process_spawn_serves_bounded_table() {
    let services = Rc::new(SpawnServices {
        store: RefCell::new(BTreeMap::new()),
        calls: RefCell::new(Vec::new()),
        deny: false,
    });
    let mut vm = gate_vm("spawn-ok");
    install_spawn(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local result = bitty.process.spawn({ "status", "--porcelain" })
            bitty.store.set("output", result.output)
            bitty.store.set("untrusted", result.untrusted)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("output"),
        Some(&LuaValue::String("M  staged.lua".to_string()))
    );
    assert_eq!(
        services.store.borrow().get("untrusted"),
        Some(&LuaValue::Bool(true))
    );
    assert_eq!(
        services.calls.borrow().as_slice(),
        &[vec!["status".to_string(), "--porcelain".to_string()]]
    );
}

#[test]
fn process_spawn_denied_stays_typed() {
    let services = Rc::new(SpawnServices {
        store: RefCell::new(BTreeMap::new()),
        calls: RefCell::new(Vec::new()),
        deny: true,
    });
    let mut vm = gate_vm("spawn-denied");
    install_spawn(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.process.spawn, { "status" })
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
}

#[test]
fn process_spawn_rejects_malformed_argv() {
    for (tag, chunk) in [
        (
            "non-table",
            r#"local ok, err = pcall(bitty.process.spawn, "status")"#.to_string(),
        ),
        (
            "empty",
            r#"local ok, err = pcall(bitty.process.spawn, {})"#.to_string(),
        ),
        (
            "non-string",
            r#"local ok, err = pcall(bitty.process.spawn, { "status", 42 })"#.to_string(),
        ),
        (
            "sparse",
            r#"local t = {} t[1] = "status" t[3] = "x" local ok, err = pcall(bitty.process.spawn, t)"#
                .to_string(),
        ),
    ] {
        let services = Rc::new(SpawnServices {
            store: RefCell::new(BTreeMap::new()),
            calls: RefCell::new(Vec::new()),
            deny: false,
        });
        let mut vm = gate_vm(format!("spawn-bad-{tag}"));
        install_spawn(&mut vm, services.clone());
        let outcome = vm
            .execute_bounded(&format!(
                r#"
            {chunk}
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#
            ))
            .expect("execute");
        assert!(
            matches!(outcome, BoundedExecution::Completed),
            "{tag}: {outcome:?}"
        );
        let code = services.store.borrow().get("code").cloned();
        assert!(
            matches!(
                code,
                Some(LuaValue::String(ref s))
                if s == "E_VALUE_TYPE" || s == "E_VALUE_NODES" || s == "E_VALUE_BYTES"
            ),
            "{tag}: got {code:?}"
        );
        assert!(
            services.calls.borrow().is_empty(),
            "{tag}: malformed argv must not reach the service"
        );
    }
}

#[test]
fn process_namespace_is_read_only() {
    let mut vm = gate_vm("spawn-readonly");
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm
        .execute_bounded("bitty.process.spawn = 1")
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "assignment must fail: {outcome:?}"
    );
}

/// Slow-but-successful spawn service: past the cheap-call deadline, within
/// the spawn timeout contract.
struct SlowSpawnServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    delay_ms: u64,
}

impl HostServices for SlowSpawnServices {
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

    fn process_spawn(&self, _args: &[String]) -> Result<LuaValue, BridgeError> {
        std::thread::sleep(Duration::from_millis(self.delay_ms));
        Ok(LuaValue::table([
            ("output", LuaValue::String("slow-ok".to_string())),
            ("stderr", LuaValue::String(String::new())),
            ("truncated", LuaValue::Bool(false)),
            ("exit_code", LuaValue::Integer(0)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }
}

#[test]
fn slow_spawn_is_delivered_not_timed_out() {
    // FIX 2 (CTX-0445 review) pin: `process.spawn` keeps the re-entrancy
    // guard but is exempt from the post-hoc cheap-call deadline, so a
    // slow-but-successful spawn is delivered instead of being run to
    // completion and then discarded as `E_TIMEOUT` — which would orphan a
    // 64-slot registry entry Lua can never reconcile (64 such orphans =
    // self-DoS via `LimitExceeded`). The 30 ms delay exceeds the 1 ms bridge
    // deadline (old code returned `E_TIMEOUT` here) yet stays inside the
    // default 50 ms VM wall budget, mirroring the existing
    // `host_call_deadline_returns_typed_timeout` timing shape.
    let services = Rc::new(SlowSpawnServices {
        store: RefCell::new(BTreeMap::new()),
        delay_ms: 30,
    });
    let mut vm = gate_vm("spawn-slow");
    let services_dyn: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 1)
        .expect("install");
    let outcome = vm
        .execute_bounded(
            r#"
            local result = bitty.process.spawn({ "status" })
            bitty.store.set("output", result.output)
            bitty.store.set("untrusted", result.untrusted)
            bitty.store.set("exit_code", result.exit_code)
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("output"),
        Some(&LuaValue::String("slow-ok".to_string()))
    );
    assert_eq!(
        services.store.borrow().get("untrusted"),
        Some(&LuaValue::Bool(true))
    );
    assert_eq!(
        services.store.borrow().get("exit_code"),
        Some(&LuaValue::Integer(0))
    );
}

// ── CTX-0464 sandbox gaps: pre-commit timeout + spawn bridge timeout ─────

/// Slow mutating store: sleeps 30 ms on key `k`, fast otherwise.
///
/// Old `store_set` (pre-fix bridge path) sleeps then writes unconditionally,
/// so a 1 ms deadline still commits `k` before returning `E_TIMEOUT`
/// (applied-then-timeout). The new `store_set_with_expiry` sleeps then checks
/// the bridge expiry before committing, so post-deadline effects never commit
/// (check-then-act inside the budget).
struct SlowStoreServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl HostServices for SlowStoreServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        if key == "k" {
            std::thread::sleep(Duration::from_millis(30));
        }
        self.store.borrow_mut().insert(key.to_string(), value);
        Ok(())
    }

    fn store_set_with_expiry(
        &self,
        key: &str,
        value: LuaValue,
        expiry: std::time::Instant,
    ) -> Result<(), BridgeError> {
        if key == "k" {
            std::thread::sleep(Duration::from_millis(30));
        }
        if std::time::Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
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

#[test]
fn post_deadline_effects_never_commit() {
    // CTX-0464 gap 3: bounded() ran f() to completion then checked the
    // deadline, so slow store_set committed `k` even when returning
    // E_TIMEOUT. After the fix the bridge passes its expiry to the service
    // and the service checks before committing, so `k` is absent while the
    // typed timeout is still delivered via the fast `code` write.
    let services = Rc::new(SlowStoreServices {
        store: RefCell::new(BTreeMap::new()),
    });
    let mut vm = gate_vm("pre-commit");
    let services_dyn: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 1)
        .expect("install");
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.store.set, "k", "v")
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_TIMEOUT".to_string())),
        "slow write must report typed timeout"
    );
    assert!(
        services.store.borrow().get("k").is_none(),
        "post-deadline write must never commit"
    );
}

/// Slow-but-committing mutation whose write itself is the commit.
///
/// This mirrors the real plugin store: the service performs a fast
/// pre-commit expiry check (per the `*_with_expiry` contract) and then does
/// bounded but non-instant atomic temp-then-rename I/O that can exceed the
/// cheap-call budget on slow filesystems. It reports success because the
/// effect landed.
struct SlowCommitServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl HostServices for SlowCommitServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        if key == "k" {
            std::thread::sleep(Duration::from_millis(30));
        }
        self.store.borrow_mut().insert(key.to_string(), value);
        Ok(())
    }

    fn store_set_with_expiry(
        &self,
        key: &str,
        value: LuaValue,
        expiry: std::time::Instant,
    ) -> Result<(), BridgeError> {
        if std::time::Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.store_set(key, value)
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

#[test]
fn committed_mutation_is_not_reported_as_timeout() {
    // CTX-0477: `bitty.store.set` persists atomically (temp-then-rename) and
    // on the Windows CI filesystem that write can exceed the 50 ms cheap-call
    // budget after committing. Before the fix `bounded()` re-checked the
    // deadline after `f` returned and raised E_TIMEOUT on an effect that had
    // already landed (applied-then-timeout), failing the plugin callback.
    // Mutating calls must deliver the committed result instead.
    let services = Rc::new(SlowCommitServices {
        store: RefCell::new(BTreeMap::new()),
    });
    let mut vm = gate_vm("commit");
    let services_dyn: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 1)
        .expect("install");
    let outcome = vm
        .execute_bounded(
            r#"
            local ok = pcall(bitty.store.set, "k", "v")
            bitty.store.set("code", ok and "OK" or "ERR")
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("OK".to_string())),
        "a committed slow write must not be reported as a timeout"
    );
    assert_eq!(
        services.store.borrow().get("k"),
        Some(&LuaValue::String("v".to_string())),
        "the committed value must be observable"
    );
}

/// Slow spawn with expiry-aware backend for the spawn bridge timeout path.
struct TimeoutSpawnServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    delay_ms: u64,
}

impl HostServices for TimeoutSpawnServices {
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

    fn process_spawn(&self, _args: &[String]) -> Result<LuaValue, BridgeError> {
        std::thread::sleep(Duration::from_millis(self.delay_ms));
        Ok(LuaValue::table([
            ("output", LuaValue::String("slow-ok".to_string())),
            ("stderr", LuaValue::String(String::new())),
            ("truncated", LuaValue::Bool(false)),
            ("exit_code", LuaValue::Integer(0)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }

    fn process_spawn_with_expiry(
        &self,
        _args: &[String],
        expiry: std::time::Instant,
    ) -> Result<LuaValue, BridgeError> {
        std::thread::sleep(Duration::from_millis(self.delay_ms));
        if std::time::Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        Ok(LuaValue::table([
            ("output", LuaValue::String("slow-ok".to_string())),
            ("stderr", LuaValue::String(String::new())),
            ("truncated", LuaValue::Bool(false)),
            ("exit_code", LuaValue::Integer(0)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }
}

#[test]
fn spawn_timeout_honored() {
    // CTX-0464 gap 4: bounded_spawn kept only the re-entrancy guard with no
    // bridge timeout handle, so slow spawns never timed out. After the fix
    // spawn flows through the bridge timeout path with its own deadline
    // (default 5 s, configurable down to 1 ms for tests): a 50 ms spawn with
    // a 10 ms spawn deadline must fail-closed with typed E_TIMEOUT and no
    // result delivered.
    let services = Rc::new(TimeoutSpawnServices {
        store: RefCell::new(BTreeMap::new()),
        delay_ms: 50,
    });
    let mut vm = gate_vm("spawn-timeout");
    let services_dyn: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(services_dyn, MarshallingLimits::default(), 50)
        .expect("install");
    vm.set_spawn_deadline_ms(10).expect("spawn deadline");
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, res = pcall(bitty.process.spawn, { "status" })
            if ok then
                bitty.store.set("output", res.output)
            else
                bitty.store.set("code", res.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_TIMEOUT".to_string())),
        "slow spawn past its deadline must report typed timeout"
    );
    assert!(
        services.store.borrow().get("output").is_none(),
        "timed-out spawn result must never be delivered"
    );
}

// ── HOST-002 registration admission quotas ──────────────────────────────

use bitty_lua::{
    REGISTRATION_MAX_COMMANDS, REGISTRATION_MAX_DESCRIPTION_BYTES,
    REGISTRATION_MAX_EVENT_KIND_BYTES, REGISTRATION_MAX_EVENTS, REGISTRATION_MAX_ID_BYTES,
    REGISTRATION_MAX_TIMER_DELAY_MS, REGISTRATION_MAX_TIMERS, REGISTRATION_MAX_TITLE_BYTES,
    RegistrationCapture,
};

fn assert_bridge_code(vm: &mut LuaVm, services: &Rc<FakeServices>, chunk: &str, want: &str) {
    let outcome = vm.execute_bounded(chunk).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{want}: chunk must complete via pcall: {outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String(want.to_string())),
        "{want}: typed code"
    );
}

fn register_chunk(id: &str) -> String {
    format!("bitty.commands.register({{ id = \"{id}\", title = \"T\", run = function() end }})")
}

#[test]
fn command_registration_count_capped_at_bridge() {
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("reg-cap-commands");
    install(&mut vm, services.clone());
    let mut body = String::new();
    for i in 0..REGISTRATION_MAX_COMMANDS {
        body.push_str(&register_chunk(&format!("cmd{i}")));
        body.push('\n');
    }
    let outcome = vm.execute_bounded(&body).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "at-cap registration must complete: {outcome:?}"
    );
    assert_eq!(
        vm.take_registrations().commands.len(),
        REGISTRATION_MAX_COMMANDS
    );

    // The 129th registration fails closed with E_DEF_LIMIT and no push.
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.commands.register, { id = "one-too-many", title = "T", run = function() end })
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_DEF_LIMIT".to_string()))
    );
    assert_eq!(
        vm.take_registrations().commands.len(),
        REGISTRATION_MAX_COMMANDS,
        "over-limit registration must not grow the capture"
    );
}

#[test]
fn command_field_lengths_capped_at_bridge() {
    // Oversized payloads are built host-side: the restricted stdlib has no
    // `string.rep`, and interpolating keeps the Lua chunk trivially small.
    let big_id = "i".repeat(REGISTRATION_MAX_ID_BYTES + 1);
    let big_title = "t".repeat(REGISTRATION_MAX_TITLE_BYTES + 1);
    let big_description = "d".repeat(REGISTRATION_MAX_DESCRIPTION_BYTES + 1);
    for (tag, field) in [
        ("id", format!("id = \"{big_id}\", title = \"T\"")),
        ("title", format!("id = \"ok\", title = \"{big_title}\"")),
        (
            "description",
            format!("id = \"ok\", title = \"T\", description = \"{big_description}\""),
        ),
    ] {
        let services = Rc::new(FakeServices::default());
        let mut vm = gate_vm(format!("reg-cap-field-{tag}"));
        install(&mut vm, services.clone());
        assert_bridge_code(
            &mut vm,
            &services,
            &format!(
                "local ok, err = pcall(bitty.commands.register, {{ {field}, run = function() end }})\nif ok then bitty.store.set(\"code\", \"NONE\") else bitty.store.set(\"code\", err.code) end"
            ),
            "E_DEF_INVALID",
        );
        assert!(
            vm.take_registrations().commands.is_empty(),
            "{tag}: oversized field must not be captured"
        );
    }

    // Boundary values (exactly at cap) still register.
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("reg-cap-field-boundary");
    install(&mut vm, services.clone());
    let at_id = "i".repeat(REGISTRATION_MAX_ID_BYTES);
    let at_title = "t".repeat(REGISTRATION_MAX_TITLE_BYTES);
    let at_description = "d".repeat(REGISTRATION_MAX_DESCRIPTION_BYTES);
    let outcome = vm
        .execute_bounded(&format!(
            "bitty.commands.register({{ id = \"{at_id}\", title = \"{at_title}\", description = \"{at_description}\", run = function() end }})"
        ))
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(vm.take_registrations().commands.len(), 1);
}

#[test]
fn event_subscription_count_and_kind_capped_at_bridge() {
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("reg-cap-events");
    install(&mut vm, services.clone());
    let mut body = String::new();
    for i in 0..REGISTRATION_MAX_EVENTS {
        body.push_str(&format!(
            "bitty.events.subscribe(\"kind{i}\", function() end)\n"
        ));
    }
    let outcome = vm.execute_bounded(&body).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "at-cap subscriptions must complete: {outcome:?}"
    );
    assert_eq!(
        vm.take_registrations().events.len(),
        REGISTRATION_MAX_EVENTS
    );

    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.events.subscribe, "one-too-many", function() end)
            if ok then
                bitty.store.set("code", "NONE")
            else
                bitty.store.set("code", err.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String("E_DEF_LIMIT".to_string()))
    );
    assert_eq!(
        vm.take_registrations().events.len(),
        REGISTRATION_MAX_EVENTS
    );

    // Oversized kind fails closed with E_DEF_INVALID and no capture.
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("reg-cap-event-kind");
    install(&mut vm, services.clone());
    let big_kind = "k".repeat(REGISTRATION_MAX_EVENT_KIND_BYTES + 1);
    assert_bridge_code(
        &mut vm,
        &services,
        &format!(
            "local ok, err = pcall(bitty.events.subscribe, \"{big_kind}\", function() end)\nif ok then bitty.store.set(\"code\", \"NONE\") else bitty.store.set(\"code\", err.code) end",
        ),
        "E_DEF_INVALID",
    );
    assert!(vm.take_registrations().events.is_empty());
}

#[test]
fn timer_count_delay_and_handle_exhaustion_capped_at_bridge() {
    // Count cap: the 65th create fails closed with E_DEF_LIMIT.
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("reg-cap-timers");
    install(&mut vm, services.clone());
    let mut body = String::new();
    for _ in 0..REGISTRATION_MAX_TIMERS {
        body.push_str("bitty.timers.create(10, function() end)\n");
    }
    let outcome = vm.execute_bounded(&body).expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "at-cap timers must complete: {outcome:?}"
    );
    {
        let capture = vm.take_registrations();
        assert_eq!(capture.timers.len(), REGISTRATION_MAX_TIMERS);
        // Handles are dense from 1 with no aliasing.
        let mut handles: Vec<i64> = capture.timers.iter().map(|t| t.handle).collect();
        handles.sort_unstable();
        assert_eq!(
            handles,
            (1..=REGISTRATION_MAX_TIMERS as i64).collect::<Vec<_>>()
        );
    }
    assert_bridge_code(
        &mut vm,
        &services,
        r#"local ok, err = pcall(bitty.timers.create, 10, function() end)
            if ok then bitty.store.set("code", "NONE") else bitty.store.set("code", err.code) end"#,
        "E_DEF_LIMIT",
    );
    assert_eq!(
        vm.take_registrations().timers.len(),
        REGISTRATION_MAX_TIMERS
    );

    // Delay cap: absurd delays fail closed with E_DEF_INVALID.
    let services = Rc::new(FakeServices::default());
    let mut vm = gate_vm("reg-cap-timer-delay");
    install(&mut vm, services.clone());
    assert_bridge_code(
        &mut vm,
        &services,
        &format!(
            "local ok, err = pcall(bitty.timers.create, {}, function() end)\nif ok then bitty.store.set(\"code\", \"NONE\") else bitty.store.set(\"code\", err.code) end",
            REGISTRATION_MAX_TIMER_DELAY_MS + 1
        ),
        "E_DEF_INVALID",
    );
    assert!(vm.take_registrations().timers.is_empty());

    // Boundary delay (exactly at cap) still creates.
    let outcome = vm
        .execute_bounded(&format!(
            "bitty.timers.create({REGISTRATION_MAX_TIMER_DELAY_MS}, function() end)"
        ))
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(vm.take_registrations().timers.len(), 1);
}

#[test]
fn timer_handle_allocation_is_checked_not_wrapping() {
    // Unit-level: the handle counter fails closed at exhaustion instead of
    // wrapping (release-mode `+= 1` would alias handle 1 onto a live timer).
    // `i64::MAX` is the reserved exhaustion sentinel and is never issued.
    let mut capture = RegistrationCapture::new();
    capture.next_timer_handle = i64::MAX - 1;
    assert_eq!(capture.alloc_timer_handle(), Some(i64::MAX - 1));
    assert_eq!(capture.next_timer_handle, i64::MAX);
    assert_eq!(
        capture.alloc_timer_handle(),
        None,
        "exhausted handle space must be signalled, not wrapped"
    );
    assert_eq!(capture.next_timer_handle, i64::MAX);
    assert_eq!(
        capture.alloc_timer_handle(),
        None,
        "exhaustion must be sticky"
    );
}

// ── LUA-OQ-8 services bridge (get/provide ↔ HostServices) ────────────────

/// One recorded `service_call`: provider, generation, iface, method, args.
type ServiceCall = (String, u32, String, String, LuaValue);

/// Canned `HostServices` service backend: scripted routes and gate errors,
/// recorded invocations. Proves the bridge delegates shape-valid calls to
/// the host and maps every failure to its typed code.
struct ServiceStub {
    store: RefCell<BTreeMap<String, LuaValue>>,
    routes: RefCell<BTreeMap<String, ServiceRoute>>,
    resolve_error: RefCell<Option<BridgeError>>,
    provide_error: RefCell<Option<BridgeError>>,
    call_error: RefCell<Option<BridgeError>>,
    call_result: RefCell<LuaValue>,
    calls: RefCell<Vec<ServiceCall>>,
}

impl Default for ServiceStub {
    fn default() -> Self {
        Self {
            store: RefCell::new(BTreeMap::new()),
            routes: RefCell::new(BTreeMap::new()),
            resolve_error: RefCell::new(None),
            provide_error: RefCell::new(None),
            call_error: RefCell::new(None),
            call_result: RefCell::new(LuaValue::Nil),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl HostServices for ServiceStub {
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

    fn service_provide_check(&self, _iface: &str) -> Result<(), BridgeError> {
        match self.provide_error.borrow().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn service_resolve(
        &self,
        iface: &str,
        _req: Option<&str>,
        _optional: bool,
    ) -> Result<Option<ServiceRoute>, BridgeError> {
        match self.resolve_error.borrow().clone() {
            Some(error) => Err(error),
            None => Ok(self.routes.borrow().get(iface).cloned()),
        }
    }

    fn service_call(
        &self,
        provider: &str,
        generation: u32,
        iface: &str,
        method: &str,
        args: &LuaValue,
    ) -> Result<LuaValue, BridgeError> {
        self.calls.borrow_mut().push((
            provider.to_string(),
            generation,
            iface.to_string(),
            method.to_string(),
            args.clone(),
        ));
        match self.call_error.borrow().clone() {
            Some(error) => Err(error),
            None => Ok(self.call_result.borrow().clone()),
        }
    }
}

fn install_services(vm: &mut LuaVm, services: Rc<ServiceStub>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn assert_service_code(vm: &mut LuaVm, services: &Rc<ServiceStub>, call: &str, want: &str) {
    let outcome = vm
        .execute_bounded(&format!(
            "local ok, err = pcall(function() return {call} end)\nif ok then bitty.store.set(\"code\", \"NONE\") else bitty.store.set(\"code\", err.code) end"
        ))
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{want}: chunk must complete via pcall: {outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("code"),
        Some(&LuaValue::String(want.to_string())),
        "{want}: typed code for `{call}`"
    );
}

#[test]
fn services_get_rejects_malformed_shape() {
    for (tag, call) in [
        ("non-string", "bitty.services.get(42)"),
        ("empty", "bitty.services.get(\"\")"),
        ("space", "bitty.services.get(\"has space\")"),
        ("empty-segment", "bitty.services.get(\"calc..add\")"),
        ("non-table-opts", "bitty.services.get(\"calc.add\", 42)"),
        (
            "unknown-opt",
            "bitty.services.get(\"calc.add\", { bogus = true })",
        ),
        (
            "bad-version",
            "bitty.services.get(\"calc.add\", { version = 42 })",
        ),
        (
            "bad-optional",
            "bitty.services.get(\"calc.add\", { optional = 1 })",
        ),
    ] {
        let services = Rc::new(ServiceStub::default());
        let mut vm = gate_vm(format!("svc-shape-{tag}"));
        install_services(&mut vm, services.clone());
        assert_service_code(&mut vm, &services, call, "E_DEF_INVALID");
        assert!(
            services.calls.borrow().is_empty(),
            "{tag}: malformed shape must never reach the host"
        );
    }
}

#[test]
fn services_provide_rejects_malformed_impl() {
    for (tag, call) in [
        ("non-table", "bitty.services.provide(\"calc.add\", 42)"),
        (
            "non-function",
            "bitty.services.provide(\"calc.add\", { add = 42 })",
        ),
        ("empty", "bitty.services.provide(\"calc.add\", {})"),
        ("non-string-iface", "bitty.services.provide(42, {})"),
    ] {
        let services = Rc::new(ServiceStub::default());
        let mut vm = gate_vm(format!("svc-provide-shape-{tag}"));
        install_services(&mut vm, services.clone());
        assert_service_code(&mut vm, &services, call, "E_DEF_INVALID");
        assert!(
            vm.take_registrations().services.is_empty(),
            "{tag}: malformed impl must not be captured"
        );
    }
}

#[test]
fn services_provide_captures_impl_functions() {
    let services = Rc::new(ServiceStub::default());
    let mut vm = gate_vm("svc-provide-capture");
    install_services(&mut vm, services.clone());
    assert_service_code(
        &mut vm,
        &services,
        "bitty.services.provide(\"calc.add\", { add = function(a) return a end, sub = function(a) return a end })",
        "NONE",
    );
    let capture = vm.take_registrations();
    assert_eq!(capture.services.len(), 1);
    let provision = &capture.services[0];
    assert_eq!(provision.iface, "calc.add");
    // Impl-table iteration order is hash order, not source order.
    let mut methods = provision
        .methods
        .iter()
        .map(|method| method.name.clone())
        .collect::<Vec<_>>();
    methods.sort();
    assert_eq!(methods, vec!["add".to_string(), "sub".to_string()]);
}

#[test]
fn services_provide_propagates_host_gate() {
    let services = Rc::new(ServiceStub::default());
    *services.provide_error.borrow_mut() = Some(BridgeError::new(
        "validation",
        "E_SERVICE_UNDECLARED",
        "service 'calc.add' is not declared in services.provided",
    ));
    let mut vm = gate_vm("svc-provide-gate");
    install_services(&mut vm, services.clone());
    assert_service_code(
        &mut vm,
        &services,
        "bitty.services.provide(\"calc.add\", { add = function(a) return a end })",
        "E_SERVICE_UNDECLARED",
    );
    assert!(
        vm.take_registrations().services.is_empty(),
        "rejected provision must not be captured"
    );
}

#[test]
fn services_get_builds_pinned_handle() {
    let services = Rc::new(ServiceStub::default());
    services.routes.borrow_mut().insert(
        "calc.add".to_string(),
        ServiceRoute {
            provider: "xuepoo.calc".to_string(),
            generation: 7,
            iface: "calc.add".to_string(),
            version: "1.0.0".to_string(),
            methods: vec!["add".to_string()],
        },
    );
    *services.call_result.borrow_mut() = LuaValue::Integer(3);
    let mut vm = gate_vm("svc-get-handle");
    install_services(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local calc = bitty.services.get("calc.add")
            bitty.store.set("kind", type(calc.add))
            bitty.store.set("out", calc.add({ a = 1 }))
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("kind"),
        Some(&LuaValue::String("function".to_string()))
    );
    assert_eq!(
        services.store.borrow().get("out"),
        Some(&LuaValue::Integer(3))
    );
    // The closure pins provider, generation, interface, and method; the
    // marshalled args cross as values.
    assert_eq!(
        services.calls.borrow().as_slice(),
        &[(
            "xuepoo.calc".to_string(),
            7,
            "calc.add".to_string(),
            "add".to_string(),
            LuaValue::table([("a", LuaValue::Integer(1))]),
        )]
    );
}

#[test]
fn services_get_optional_missing_returns_nil() {
    let services = Rc::new(ServiceStub::default());
    let mut vm = gate_vm("svc-get-nil");
    install_services(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"bitty.store.set("got", bitty.services.get("calc.add", { optional = true }))"#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(services.store.borrow().get("got"), None);
}

#[test]
fn services_get_propagates_resolution_failure() {
    let services = Rc::new(ServiceStub::default());
    *services.resolve_error.borrow_mut() = Some(BridgeError::new(
        "runtime",
        "E_SERVICE_RESOLUTION",
        "service 'calc.add' cannot be resolved: no provider satisfies '>=9.0'",
    ));
    let mut vm = gate_vm("svc-get-resolution");
    install_services(&mut vm, services.clone());
    assert_service_code(
        &mut vm,
        &services,
        "bitty.services.get(\"calc.add\", { version = \">=9.0\" })",
        "E_SERVICE_RESOLUTION",
    );
}

#[test]
fn services_call_propagates_provider_failure() {
    let services = Rc::new(ServiceStub::default());
    services.routes.borrow_mut().insert(
        "calc.add".to_string(),
        ServiceRoute {
            provider: "xuepoo.calc".to_string(),
            generation: 1,
            iface: "calc.add".to_string(),
            version: "1.0.0".to_string(),
            methods: vec!["add".to_string()],
        },
    );
    *services.call_error.borrow_mut() = Some(BridgeError::new(
        "runtime",
        "E_SERVICE_GONE",
        "service 'calc.add' is unavailable",
    ));
    let mut vm = gate_vm("svc-call-gone");
    install_services(&mut vm, services.clone());
    assert_service_code(
        &mut vm,
        &services,
        "bitty.services.get(\"calc.add\").add({ a = 1 })",
        "E_SERVICE_GONE",
    );
    assert_eq!(services.calls.borrow().len(), 1);
}
