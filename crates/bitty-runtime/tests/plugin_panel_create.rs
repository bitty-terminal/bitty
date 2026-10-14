#![forbid(unsafe_code)]
//! `bitty.panel.create` over a real plugin VM (CTX-1214, Issue #2085).
//!
//! The CTX-0915 panel surface landed with every backend `not_implemented`,
//! so the registry half of Panel Claim 1 (a zero-PTY panel via the plugin
//! API, not test helpers) was unprovable. This file wires the
//! `set_panel_create_backend` seam at activation: a `panel.create`
//! generation routes `create` through a backend closing over a real
//! `PanelRegistry`, returning the `(id, generation)` pair the Lua bridge
//! shapes into `{ id, generation }`. Mounting and focusing stay host-driven
//! through the public `mount_panel`/`focus_panel` path, so the backend
//! carries no layout authority. Headless and Windows-safe (temp dirs only,
//! no PTY spawn, no processes or sockets).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, PanelCreateHandler, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};
use bitty_runtime::registry::{
    Generation, PanelError, PanelId, PanelRegistry, PanelRegistryConfig, PanelState,
    RegistryConfig, TerminalRegistry, WorkspaceId,
};
use bitty_ui::{ViewId, panel::PanelType};

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Live-registry create backend standing in for the application host.
///
/// Allocates a `Created` record in the shared [`PanelRegistry`] for native
/// (zero-PTY) types only. `terminal` would mint PTY/process-spawn authority
/// the grant never conveys, so it fails closed with `E_DEF_INVALID` here
/// too (the services gate enforces the same policy first, so this is
/// defense in depth for direct backend callers). Registry exhaustion maps
/// to the bounded `E_DEF_LIMIT`; an unservable host maps to
/// `E_NOT_IMPLEMENTED`.
fn create_backend(panels: Rc<RefCell<PanelRegistry>>) -> PanelCreateHandler {
    Rc::new(move |panel_type| {
        let parsed = PanelType::parse(panel_type).ok_or_else(|| {
            BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!("unknown panel type '{panel_type}'"),
            )
        })?;
        if parsed == PanelType::Terminal {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                "panel.create cannot create terminal panels: plugins create zero-PTY panels only",
            ));
        }
        panels
            .borrow_mut()
            .create_panel(parsed, None)
            .map(|handle| (handle.id.0, handle.generation.get()))
            .map_err(|error| match error {
                PanelError::TooManyPanels { max, .. } => BridgeError::new(
                    "budget",
                    "E_DEF_LIMIT",
                    format!("panel registry full (max {max} panels)"),
                ),
                PanelError::UnknownPanelType { value } => BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    format!("unknown panel type '{value}'"),
                ),
                _ => BridgeError::not_implemented("bitty.panel.create"),
            })
    })
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-create-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf, backend: Option<PanelCreateHandler>) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    rt.set_panel_create_backend(backend);
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

fn string(value: LuaValue) -> String {
    match value {
        LuaValue::String(s) => s,
        other => panic!("expected a string result, got {other:?}"),
    }
}

fn activate_all(rt: &mut PluginRuntime) {
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
}

/// Probe plugin: every command reports `OK:<id>/<generation>` or
/// `ERR:<code>` across the real Lua bridge (table-form `panel.create`, the
/// VM-adapted CTX-1211 shape).
const PROBE: &str = r#"
local function create_result(typ)
  local ok, res = pcall(bitty.panel.create, { type = typ })
  if not ok then return "ERR:" .. res.code end
  return "OK:" .. tostring(res.id) .. "/" .. tostring(res.generation)
end

bitty.commands.register({ id = "create_rich", title = "Create rich", run = function()
  return create_result("rich") end })
bitty.commands.register({ id = "create_helper", title = "Create helper", run = function()
  return create_result("helper") end })
bitty.commands.register({ id = "create_terminal", title = "Create terminal", run = function()
  return create_result("terminal") end })
bitty.commands.register({ id = "create_bogus", title = "Create bogus", run = function()
  return create_result("bogus") end })
bitty.commands.register({ id = "close", title = "Close", run = function()
  local ok, res = pcall(bitty.panel.close, 1)
  if ok then return "OK:" .. tostring(res) end
  return "ERR:" .. res.code end })
"#;

const PROBE_COMMANDS: &[&str] = &[
    "create_rich",
    "create_helper",
    "create_terminal",
    "create_bogus",
    "close",
];

fn probe_runtime(
    tag: &str,
    id: &str,
    capabilities: &[&str],
    with_backend: bool,
    panels: &Rc<RefCell<PanelRegistry>>,
) -> (PluginRuntime, PluginId) {
    let root = temp_dir(tag);
    write_plugin(&root, id, capabilities, PROBE_COMMANDS, PROBE);
    let backend = with_backend.then(|| create_backend(panels.clone()));
    let mut rt = runtime(root, backend);
    activate_all(&mut rt);
    (rt, pid(id))
}

fn run(rt: &mut PluginRuntime, id: &PluginId, command: &str) -> String {
    string(
        rt.dispatch_command(id, command, &[])
            .unwrap_or_else(|error| panic!("dispatch {command}: {error}")),
    )
}

/// Split an `OK:<id>/<generation>` probe result into raw handle parts.
fn parse_handle(result: &str) -> (u64, Generation) {
    let rest = result
        .strip_prefix("OK:")
        .unwrap_or_else(|| panic!("ok result: {result}"));
    let (id, generation) = rest
        .split_once('/')
        .unwrap_or_else(|| panic!("id/generation result: {result}"));
    (
        id.parse().expect("numeric panel id"),
        Generation(generation.parse().expect("numeric generation")),
    )
}

#[test]
fn granted_create_focuses_zero_pty_panel_end_to_end() {
    // CTX-1214 (Issue #2085): the Claim-1 registry proof through the real
    // plugin API. A granted `panel.create` call allocates a native panel in
    // the shared registry with zero terminals anywhere; the host then mounts
    // and focuses it through the public path.
    let panels = Rc::new(RefCell::new(
        PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
    ));
    let terminals = TerminalRegistry::new(RegistryConfig::default()).expect("terminal registry");
    assert_eq!(terminals.terminal_count(), 0, "no PTY exists");
    let (mut rt, id) = probe_runtime(
        "granted",
        "xuepoo.createprobe",
        &["panel.create"],
        true,
        &panels,
    );

    let created = run(&mut rt, &id, "create_rich");
    let (raw_id, generation) = parse_handle(&created);
    let panel_id = PanelId(raw_id);
    assert_eq!(panels.borrow().panel_count(), 1);
    assert_eq!(
        terminals.terminal_count(),
        0,
        "plugin creation allocates no PTY-backed terminal"
    );
    assert_eq!(
        panels
            .borrow()
            .panel_state(panel_id, generation)
            .expect("live handle"),
        PanelState::Created,
        "creation allocates only; the host mounts"
    );

    // Host-driven mount + focus through the public path (layout authority
    // never crosses into the plugin).
    let workspace = WorkspaceId::new(1);
    let view = ViewId::new(900);
    panels
        .borrow_mut()
        .mount_panel(panel_id, generation, view)
        .expect("host mounts the created panel");
    panels
        .borrow_mut()
        .focus_panel(panel_id, generation, workspace)
        .expect("host focuses the mounted panel");
    assert_eq!(panels.borrow().focused_panel(workspace), Some(panel_id));
    assert_eq!(
        panels
            .borrow()
            .panel_state(panel_id, generation)
            .expect("live handle"),
        PanelState::Focused,
    );

    // A second creation allocates a distinct record; the first stays focused.
    let second = run(&mut rt, &id, "create_helper");
    let (raw_second, _) = parse_handle(&second);
    assert_ne!(raw_second, raw_id, "each create allocates a fresh panel");
    assert_eq!(panels.borrow().panel_count(), 2);
    assert_eq!(terminals.terminal_count(), 0, "still zero PTY");
    assert_eq!(panels.borrow().focused_panel(workspace), Some(panel_id));
}

#[test]
fn create_denied_without_grant_touches_nothing() {
    // Fail-closed ungranted: the backend is wired but the call-time grant
    // gate denies first, so no record is allocated.
    let panels = Rc::new(RefCell::new(
        PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
    ));
    let (mut rt, id) = probe_runtime("denied", "xuepoo.createdenied", &[], true, &panels);
    assert_eq!(run(&mut rt, &id, "create_rich"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(
        run(&mut rt, &id, "create_helper"),
        "ERR:E_CAPABILITY_DENIED"
    );
    assert_eq!(panels.borrow().panel_count(), 0);
}

#[test]
fn granted_create_without_host_backend_stays_not_implemented() {
    // Fail-closed where unwired: no runtime-level backend installed, so a
    // granted call reports `E_NOT_IMPLEMENTED` instead of touching state.
    let panels = Rc::new(RefCell::new(
        PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
    ));
    let (mut rt, id) = probe_runtime(
        "unwired",
        "xuepoo.createunwired",
        &["panel.create"],
        false,
        &panels,
    );
    assert_eq!(run(&mut rt, &id, "create_rich"), "ERR:E_NOT_IMPLEMENTED");
    assert_eq!(panels.borrow().panel_count(), 0);
}

#[test]
fn terminal_and_unknown_types_fail_closed_without_allocating() {
    // Zero-PTY policy: `terminal` (PTY/process authority) and unknown types
    // fail closed with `E_DEF_INVALID` and allocate nothing.
    let panels = Rc::new(RefCell::new(
        PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
    ));
    let (mut rt, id) = probe_runtime(
        "types",
        "xuepoo.createtypes",
        &["panel.create"],
        true,
        &panels,
    );
    assert_eq!(run(&mut rt, &id, "create_terminal"), "ERR:E_DEF_INVALID");
    assert_eq!(run(&mut rt, &id, "create_bogus"), "ERR:E_DEF_INVALID");
    assert_eq!(
        panels.borrow().panel_count(),
        0,
        "rejected types allocate nothing"
    );
    // Native types still serve in the same generation afterwards.
    parse_handle(&run(&mut rt, &id, "create_rich"));
    assert_eq!(panels.borrow().panel_count(), 1);
}

#[test]
fn create_grant_does_not_imply_focus_verbs() {
    // Independent grants: `panel.create` never implies `panel.focus`
    // manipulation, and `panel.focus` alone never implies creation.
    let panels = Rc::new(RefCell::new(
        PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
    ));
    let (mut rt, id) = probe_runtime(
        "create-only",
        "xuepoo.createonly",
        &["panel.create"],
        true,
        &panels,
    );
    parse_handle(&run(&mut rt, &id, "create_rich"));
    assert_eq!(run(&mut rt, &id, "close"), "ERR:E_CAPABILITY_DENIED");

    let focus_panels = Rc::new(RefCell::new(
        PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry"),
    ));
    let (mut focus_rt, focus_id) = probe_runtime(
        "focus-only",
        "xuepoo.createfocusonly",
        &["panel.focus"],
        true,
        &focus_panels,
    );
    assert_eq!(
        run(&mut focus_rt, &focus_id, "create_rich"),
        "ERR:E_CAPABILITY_DENIED"
    );
    assert_eq!(focus_panels.borrow().panel_count(), 0);
}

#[test]
fn registry_exhaustion_fails_closed_bounded() {
    // Bounded allocation: a full registry refuses with `E_DEF_LIMIT` and
    // leaves existing records untouched.
    let config = PanelRegistryConfig {
        max_panels_per_workspace: 1,
        max_panels_per_window: 1,
        ..PanelRegistryConfig::default()
    };
    let panels = Rc::new(RefCell::new(
        PanelRegistry::new(config).expect("panel registry"),
    ));
    let (mut rt, id) = probe_runtime(
        "full",
        "xuepoo.createfull",
        &["panel.create"],
        true,
        &panels,
    );
    parse_handle(&run(&mut rt, &id, "create_rich"));
    assert_eq!(run(&mut rt, &id, "create_helper"), "ERR:E_DEF_LIMIT");
    assert_eq!(panels.borrow().panel_count(), 1);
}
