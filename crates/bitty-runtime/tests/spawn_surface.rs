//! CTX-0445 integration: the exact git-panel Lua call shape through the real
//! bridge and grant gate.
//!
//! The consumer (`recording/CTX-0400/git-panel/lua/git-panel/init.lua`) calls
//! `bitty.process.spawn({ "status", "--porcelain" })` and reads
//! `result.output`, failing with `E_SPAWN_UNAVAILABLE` when the surface is
//! missing. These tests pin that shape end to end: the `bitty.process` table
//! exists, the grant gate denies without `process.spawn:git`, and a granted
//! plugin reaches its backend. CTX-1715 adds the confined `{ cwd = "pane" }`
//! semantic: the plugin selects it, the host resolves it, and a
//! plugin-supplied path is still refused before any backend contact.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{BoundedExecution, HostServices, LuaValue, LuaVm, MarshallingLimits};

/// Gate-built VM with default RC budgets (replaces deprecated `LuaVm::new`).
fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}
use bitty_runtime::PaneCwd;
use bitty_runtime::plugin_runtime::services::{
    EmptySettings, NotificationQueue, PaneCwdSource, PluginServices, UnavailableSnapshot,
};
use bitty_runtime::plugin_runtime::spawn::{git_spawn_backend, git_spawn_backend_with_pane_cwd};
use bitty_runtime::plugin_runtime::store::PluginStore;

/// Test pane source with a caller-controlled directory.
#[derive(Default)]
struct StaticPaneCwd {
    pinned: RefCell<Option<PaneCwd>>,
}

impl StaticPaneCwd {
    fn new(path: PathBuf) -> Self {
        Self {
            pinned: RefCell::new(Some(
                PaneCwd::from_path(&path).expect("fixture directory must pin"),
            )),
        }
    }
}

impl PaneCwdSource for StaticPaneCwd {
    fn pane_cwd(&self) -> Option<PaneCwd> {
        self.pinned.borrow().clone()
    }
}

fn services_with(spawn_git: bool, backend: bool) -> Rc<PluginServices> {
    let services = Rc::new(PluginServices::new(
        "bitty-terminal.git-panel",
        PluginStore::in_memory(),
        Rc::new(EmptySettings),
        Rc::new(UnavailableSnapshot),
        Rc::new(RefCell::new(NotificationQueue::new(8))),
        true,
        false,
    ));
    if spawn_git {
        services.set_spawn_git(true);
    }
    if backend {
        let seen: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let seen_clone = seen.clone();
        services.set_spawn_backend(Some(Rc::new(move |args: &[String], _cwd| {
            seen_clone.borrow_mut().push(args.to_vec());
            Ok(LuaValue::table([
                ("output", LuaValue::String(" M staged.lua".to_string())),
                ("stderr", LuaValue::String(String::new())),
                ("truncated", LuaValue::Bool(false)),
                ("exit_code", LuaValue::Integer(0)),
                ("untrusted", LuaValue::Bool(true)),
            ]))
        })));
    }
    services
}

fn install(vm: &mut LuaVm, services: Rc<PluginServices>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn stored(services: &PluginServices, key: &str) -> Option<LuaValue> {
    services.with_store(|store| store.get(key))
}

#[test]
fn git_panel_call_shape_serves_output() {
    let services = services_with(true, true);
    let mut vm = gate_vm("git-panel-shape");
    install(&mut vm, services.clone());
    // Exact consumer shape: array argv, `.output` read.
    let outcome = vm
        .execute_bounded(
            r#"
            local process_ns = bitty.process
            local result = process_ns.spawn({ "status", "--porcelain" })
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
        stored(&services, "output"),
        Some(LuaValue::String(" M staged.lua".to_string()))
    );
    assert_eq!(stored(&services, "untrusted"), Some(LuaValue::Bool(true)));
}

#[test]
fn spawn_without_grant_is_capability_denied() {
    let services = services_with(false, true);
    let mut vm = gate_vm("git-panel-nogrant");
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
        stored(&services, "code"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
}

#[test]
fn granted_plugin_without_backend_is_unavailable() {
    let services = services_with(true, false);
    let mut vm = gate_vm("git-panel-nobackend");
    install(&mut vm, services.clone());
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
        stored(&services, "code"),
        Some(LuaValue::String("E_SPAWN_UNAVAILABLE".to_string()))
    );
}

#[test]
#[cfg(target_os = "linux")]
fn granted_production_backend_runs_confined_git() {
    // CTX-1376 regression, the allow arm of the grant/deny pair: the exact
    // production wiring (`process.spawn:git` granted + `git_spawn_backend`)
    // must actually start `git` under the inherited Landlock confinement.
    // The confinement hook used to fail with `EPERM` because it skipped
    // `no_new_privs`, so this call refused with `E_SPAWN_FAILED` on every
    // unprivileged host even though the grant was present.
    let services = services_with(true, false);
    services.set_spawn_backend(Some(git_spawn_backend("bitty-terminal.git-panel")));
    let mut vm = gate_vm("git-panel-granted-production");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, result = pcall(bitty.process.spawn, { "status", "--porcelain" })
            if ok then
                bitty.store.set("code", "OK")
                bitty.store.set("output", result.output)
                bitty.store.set("exit_code", result.exit_code)
                bitty.store.set("untrusted", result.untrusted)
            else
                bitty.store.set("code", result.code)
            end
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        stored(&services, "code"),
        Some(LuaValue::String("OK".to_string())),
        "granted production spawn must run confined"
    );
    assert!(
        matches!(stored(&services, "output"), Some(LuaValue::String(_))),
        "serves the bounded output string"
    );
    assert_eq!(stored(&services, "exit_code"), Some(LuaValue::Integer(0)));
    assert_eq!(stored(&services, "untrusted"), Some(LuaValue::Bool(true)));
}

#[test]
fn production_backend_denies_unallowlisted_verbs_without_spawning() {
    // CTX-0439: the activation-wired backend enforces the CTX-0444
    // `[tools.git]` allowlist. A write verb fails as E_SPAWN_DENIED without
    // contacting any process (fail-closed before scope/consent/spawn).
    let services = Rc::new(PluginServices::new(
        "bitty-terminal.git-panel",
        PluginStore::in_memory(),
        Rc::new(EmptySettings),
        Rc::new(UnavailableSnapshot),
        Rc::new(RefCell::new(NotificationQueue::new(8))),
        true,
        false,
    ));
    services.set_spawn_git(true);
    services.set_spawn_backend(Some(git_spawn_backend("bitty-terminal.git-panel")));
    let mut vm = gate_vm("git-panel-production");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.process.spawn, { "commit", "-m", "nope" })
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
        stored(&services, "code"),
        Some(LuaValue::String("E_SPAWN_DENIED".to_string()))
    );
}

#[test]
fn pane_cwd_without_a_binding_is_typed_unavailable() {
    // CTX-1715 acceptance: a production backend with no pane source refuses
    // the semantic with its own code; it never silently inherits the host
    // process cwd and never falls through to the allowlist diagnostic.
    let services = services_with(true, false);
    services.set_spawn_backend(Some(git_spawn_backend("bitty-terminal.git-panel")));
    let mut vm = gate_vm("pane-cwd-unbound");
    install(&mut vm, services.clone());
    let outcome = vm
        .execute_bounded(
            r#"
            local ok, err = pcall(bitty.process.spawn, { "status" }, { cwd = "pane" })
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
        stored(&services, "code"),
        Some(LuaValue::String("E_SPAWN_CWD_UNAVAILABLE".to_string()))
    );
}

#[test]
fn pane_cwd_keeps_git_repo_escape_flags_and_paths_denied() {
    // CTX-1715 acceptance: the pane directory does not widen the argv
    // allowlist. `-C`, `--git-dir`, and `--work-tree` remain denied (the host
    // chooses the repository through the resolved cwd, never argv), and a
    // plugin path is refused at the bridge.
    let pane = StaticPaneCwd::new(std::env::temp_dir());
    let services = services_with(true, false);
    services.set_spawn_backend(Some(git_spawn_backend_with_pane_cwd(
        "bitty-terminal.git-panel",
        Some(Rc::new(pane)),
    )));
    let mut vm = gate_vm("pane-cwd-escape");
    install(&mut vm, services.clone());
    let cases = [
        (
            r#"bitty.process.spawn({ "status", "-C", "/tmp" }, { cwd = "pane" })"#,
            "E_SPAWN_DENIED",
        ),
        (
            r#"bitty.process.spawn({ "log", "--git-dir=/tmp/.git" }, { cwd = "pane" })"#,
            "E_SPAWN_DENIED",
        ),
        (
            r#"bitty.process.spawn({ "status", "--work-tree", "/tmp" }, { cwd = "pane" })"#,
            "E_SPAWN_DENIED",
        ),
        (
            r#"bitty.process.spawn({ "status" }, { cwd = "/tmp" })"#,
            "E_DEF_INVALID",
        ),
        (
            r#"bitty.process.spawn({ "status" }, { cwd = "../outside" })"#,
            "E_DEF_INVALID",
        ),
    ];
    for (index, (call, expected)) in cases.iter().enumerate() {
        let script = format!(
            r#"local ok, err = pcall(function() {call} end)
               bitty.store.set("code", ok and "NONE" or err.code)"#
        );
        let outcome = vm.execute_bounded(&script).expect("execute");
        assert!(
            matches!(outcome, BoundedExecution::Completed),
            "{index}: {outcome:?}"
        );
        assert_eq!(
            stored(&services, "code"),
            Some(LuaValue::String((*expected).to_string())),
            "{index}: {call}"
        );
    }
}
