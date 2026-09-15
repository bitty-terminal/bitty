//! Host bridge seam tests (RFC `plugin-host-runtime-rfc` Gap A).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use bitty_lua::{BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits};

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
    let mut vm = LuaVm::new("t");
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm.execute_bounded("bitty.settings = {}").expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "assignment must fail: {outcome:?}"
    );

    let mut vm = LuaVm::new("t2");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("t");
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
    let mut vm = LuaVm::new("spawn-unavailable");
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
    let mut vm = LuaVm::new("spawn-ok");
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
    let mut vm = LuaVm::new("spawn-denied");
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
        let mut vm = LuaVm::new(format!("spawn-bad-{tag}"));
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
    let mut vm = LuaVm::new("spawn-readonly");
    install(&mut vm, Rc::new(FakeServices::default()));
    let outcome = vm
        .execute_bounded("bitty.process.spawn = 1")
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::RuntimeError(_)),
        "assignment must fail: {outcome:?}"
    );
}
