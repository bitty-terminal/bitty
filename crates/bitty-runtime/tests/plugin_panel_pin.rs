#![forbid(unsafe_code)]
//! `bitty.panel.set_pinned` over a real plugin VM (CTX-1099, follow-up to CTX-1083).
//!
//! The CTX-1083 seam plus grant gates landed without a host, so every panel
//! verb failed closed with `E_NOT_IMPLEMENTED`. CTX-1099 wires the
//! `set_panel_pin_backend` at activation: a `panel.focus` generation routes
//! `set_pinned` through the same pin/unpin runtime path the `toggle_pinned`
//! keymap action uses. Sibling panel verbs have no backend seam and stay
//! `E_NOT_IMPLEMENTED`. Headless and Windows-safe (temp dirs only, no PTY
//! spawn, no processes or sockets).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, PanelPinHandler, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};
use bitty_runtime::{LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};

mod common;

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Live-runtime pin backend standing in for the application host.
///
/// Mirrors the services-level seam test: pin/unpin route into the same
/// `pin_floating`/`unpin_floating` runtime path the `toggle_pinned` keymap
/// action uses, idempotent calls hold, and unknown targets fail closed
/// with state untouched.
fn pin_backend(live: Rc<RefCell<Runtime>>) -> PanelPinHandler {
    Rc::new(move |panel_id, pinned| {
        let mut rt = live.borrow_mut();
        let id = ViewId::new(panel_id);
        if pinned {
            if rt.pinned_views().contains(&id) {
                return Ok(true);
            }
            match rt.pin_floating(id) {
                Ok(()) => Ok(true),
                Err(_) => Ok(false),
            }
        } else {
            if !rt.pinned_views().contains(&id) && rt.layout().leaf_ids().contains(&id) {
                return Ok(true);
            }
            match rt.unpin_floating(id) {
                Ok(_) => Ok(true),
                Err(_) => Ok(false),
            }
        }
    })
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-pin-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf, backend: Option<PanelPinHandler>) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    rt.set_panel_pin_backend(backend);
    rt
}

/// Write a plugin package under `root/<id>/`.
fn write_plugin(root: &Path, id: &str, capabilities: &[&str], commands: &[&str], init_src: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let list = |items: &[String]| {
        items
            .iter()
            .map(|item| format!("\"{item}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let capabilities_toml = capabilities
        .iter()
        .map(|capability| format!("{capability} = true\n"))
        .collect::<String>();
    let qualified: Vec<String> = commands
        .iter()
        .map(|command| format!("{id}:{command}"))
        .collect();
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Test"
version = "0.1.0"
description = "test"

[compat]
plugin-api = "^1.0"

[capabilities]
{capabilities_toml}
[lazy]
commands = [{commands}]
events = []
"#,
            commands = list(&qualified),
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

fn pid(id: &str) -> PluginId {
    PluginId::new(id).expect("id")
}

fn activate_all(rt: &mut PluginRuntime) {
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
}

/// Probe plugin: every command reports `OK:<value>` or `ERR:<code>` across
/// the real Lua bridge.
const PROBE: &str = r#"
local function call(f, ...)
  local ok, result = pcall(f, ...)
  if ok then return "OK:" .. tostring(result) end
  return "ERR:" .. result.code
end

bitty.commands.register({ id = "pin", title = "Pin", run = function()
  return call(bitty.panel.set_pinned, 1, true) end })
bitty.commands.register({ id = "unpin", title = "Unpin", run = function()
  return call(bitty.panel.set_pinned, 1, false) end })
bitty.commands.register({ id = "pin_unknown", title = "Pin unknown", run = function()
  return call(bitty.panel.set_pinned, 404, true) end })
bitty.commands.register({ id = "create", title = "Create", run = function()
  return call(bitty.panel.create, { type = "terminal" }) end })
bitty.commands.register({ id = "close", title = "Close", run = function()
  return call(bitty.panel.close, 1) end })
bitty.commands.register({ id = "destroy", title = "Destroy", run = function()
  return call(bitty.panel.destroy, 1) end })
bitty.commands.register({ id = "get_presentation", title = "Get presentation", run = function()
  return call(bitty.panel.get_presentation, 1) end })
bitty.commands.register({ id = "set_presentation", title = "Set presentation", run = function()
  return call(bitty.panel.set_presentation, 1, "tab") end })
bitty.commands.register({ id = "toggle_floating", title = "Toggle floating", run = function()
  return call(bitty.panel.toggle_floating, 1) end })
bitty.commands.register({ id = "get_state", title = "Get state", run = function()
  return call(bitty.panel.get_state, 1) end })
"#;

const PROBE_COMMANDS: &[&str] = &[
    "pin",
    "unpin",
    "pin_unknown",
    "create",
    "close",
    "destroy",
    "get_presentation",
    "set_presentation",
    "toggle_floating",
    "get_state",
];

const SIBLINGS: &[&str] = &[
    "create",
    "close",
    "destroy",
    "get_presentation",
    "set_presentation",
    "toggle_floating",
    "get_state",
];

fn probe_runtime(
    tag: &str,
    id: &str,
    capabilities: &[&str],
    with_backend: bool,
) -> (PluginRuntime, PluginId, Rc<RefCell<Runtime>>) {
    let root = temp_dir(tag);
    write_plugin(&root, id, capabilities, PROBE_COMMANDS, PROBE);
    let live = Rc::new(RefCell::new(
        Runtime::with_defaults().expect("defaults must build"),
    ));
    {
        let mut rt = live.borrow_mut();
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ));
        rt.set_container(UiRect::new(0, 0, 80, 24));
    }
    let backend = with_backend.then(|| pin_backend(live.clone()));
    let mut rt = runtime(root, backend);
    // bitty#2781: probe the capability, not the wall clock. Raises only the
    // host-call and activation/dispatch wall dimensions through the documented
    // test-only seams; a park is still settled on the cold-tick resume.
    rt.set_host_call_deadline_ms(Some(common::HARNESS_HOST_CALL_DEADLINE_MS));
    rt.set_vm_wall_budget_ms(Some(common::HARNESS_VM_WALL_BUDGET_MS));
    activate_all(&mut rt);
    (rt, pid(id), live)
}

fn run(rt: &mut PluginRuntime, id: &PluginId, command: &str) -> String {
    // bitty#2781: RC-1 charges wall-clock time, so a loaded runner can park a
    // body that used microseconds of CPU. Settle the documented cold-tick
    // resume instead of requiring completion inside one default wall window.
    common::settle_dispatch(rt, id, command, &[]).expect_string(command)
}

fn live_leaves(live: &Rc<RefCell<Runtime>>) -> Vec<ViewId> {
    live.borrow().layout().leaf_ids()
}

#[test]
fn activation_installs_pin_backend_and_lua_round_trips() {
    // CTX-1099: activation captures the runtime-level pin backend into the
    // generation services, so `bitty.panel.set_pinned` from Lua routes
    // through the live pin/unpin runtime path.
    let (mut rt, id, live) = probe_runtime("granted", "xuepoo.pinprobe", &["panel.focus"], true);
    assert_eq!(run(&mut rt, &id, "pin"), "OK:true");
    assert_eq!(live.borrow().pinned_views(), vec![ViewId::new(1)]);
    assert_eq!(run(&mut rt, &id, "pin"), "OK:true", "re-pin holds");
    assert_eq!(run(&mut rt, &id, "unpin"), "OK:true");
    assert!(live.borrow().pinned_views().is_empty());
    assert_eq!(
        live_leaves(&live),
        vec![ViewId::new(1), ViewId::new(2)],
        "unpin restores the floating panel to the live layout"
    );
    assert_eq!(
        run(&mut rt, &id, "unpin"),
        "OK:true",
        "live leaf unpin holds"
    );
    assert_eq!(run(&mut rt, &id, "pin_unknown"), "OK:false");
    assert!(live.borrow().pinned_views().is_empty());
    assert_eq!(live_leaves(&live), vec![ViewId::new(1), ViewId::new(2)]);
}

#[test]
fn set_pinned_denied_without_panel_focus_grant() {
    // Grant gating stays: the backend is installed for `panel.focus`
    // generations only, and the call-time gate denies first, so an
    // ungranted plugin never reaches the live runtime.
    let (mut rt, id, live) = probe_runtime("denied", "xuepoo.pindenied", &[], true);
    assert_eq!(run(&mut rt, &id, "pin"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, &id, "unpin"), "ERR:E_CAPABILITY_DENIED");
    assert!(live.borrow().pinned_views().is_empty());
    assert_eq!(live_leaves(&live), vec![ViewId::new(1), ViewId::new(2)]);
}

#[test]
fn granted_set_pinned_without_host_backend_stays_not_implemented() {
    // Fail-closed where unwired: no runtime-level backend installed, so a
    // granted call reports `E_NOT_IMPLEMENTED` instead of touching state.
    let (mut rt, id, live) = probe_runtime("unwired", "xuepoo.pinunwired", &["panel.focus"], false);
    assert_eq!(run(&mut rt, &id, "pin"), "ERR:E_NOT_IMPLEMENTED");
    assert!(live.borrow().pinned_views().is_empty());
    assert_eq!(live_leaves(&live), vec![ViewId::new(1), ViewId::new(2)]);
}

#[test]
fn unwired_sibling_verbs_stay_not_implemented() {
    // Scope boundary: only `set_pinned` has a backend seam, so the sibling
    // panel verbs keep failing closed with `E_NOT_IMPLEMENTED` even when
    // fully granted, while `set_pinned` in the same generation applies.
    let (mut rt, id, _live) = probe_runtime(
        "siblings",
        "xuepoo.pinsiblings",
        &["panel.create", "panel.focus"],
        true,
    );
    for command in SIBLINGS {
        assert_eq!(
            run(&mut rt, &id, command),
            "ERR:E_NOT_IMPLEMENTED",
            "{command} has no backend seam"
        );
    }
    assert_eq!(run(&mut rt, &id, "pin"), "OK:true");
}
