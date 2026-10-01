//! `bitty.workspace` L1 domain over real plugin VMs (CTX-0889, #1569).
//!
//! ADR-0014: the workspace is a Core mechanism; plugins observe it through
//! `workspace.read` and request mutations through `workspace.control`.
//! Packages declare (and therefore grant) the capabilities under test; each
//! probe runs inside a registered command so calls cross the real Lua
//! bridge and grant snapshot. Headless and Windows-safe (temp dirs only).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    WORKSPACE_EVENT_KINDS, WORKSPACE_LIST_MAX_ITEMS, WORKSPACE_REQUEST_QUEUE_CAPACITY,
    WorkspaceAttention, WorkspaceInfo, WorkspaceRequest, WorkspaceSource,
};

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Fixed-row workspace source standing in for the app's `LiveWorkspaces`.
#[derive(Default)]
struct FixedWorkspaces(RefCell<Vec<WorkspaceInfo>>);

impl WorkspaceSource for FixedWorkspaces {
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        Ok(self.0.borrow().clone())
    }
}

fn row(id: u64, name: &str, active: bool, panel_count: usize) -> WorkspaceInfo {
    WorkspaceInfo {
        id,
        name: name.to_string(),
        active,
        panel_count,
        attention: WorkspaceAttention::default(),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-workspace-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf, source: Option<Rc<FixedWorkspaces>>) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    if let Some(source) = source {
        rt.set_workspace_source(source);
    }
    rt
}

/// Write a plugin package under `root/<id>/`.
fn write_plugin(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    events: &[&str],
    init_src: &str,
) {
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
    let events: Vec<String> = events.iter().map(|event| (*event).to_string()).collect();
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
events = [{events}]
"#,
            commands = list(&qualified),
            events = list(&events),
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

/// Probe plugin: `list` serializes rows; every mutation reports
/// `OK:<accepted>` or `ERR:<code>`.
const PROBE: &str = r#"
local function call(f, ...)
  local ok, result = pcall(f, ...)
  if ok then return "OK:" .. tostring(result) end
  return "ERR:" .. result.code
end

bitty.commands.register({
  id = "list",
  title = "List",
  run = function()
    local ok, rows = pcall(bitty.workspace.list)
    if not ok then return "ERR:" .. rows.code end
    local out = {}
    for _, row in ipairs(rows) do
      local a = row.attention
      out[#out + 1] = row.id .. "|" .. row.name .. "|" .. tostring(row.active) .. "|"
        .. row.panel_count .. "|" .. tostring(a.bell) .. tostring(a.activity)
        .. tostring(a.exited)
    end
    return #rows .. "=" .. table.concat(out, ";")
  end,
})
bitty.commands.register({ id = "new", title = "New", run = function()
  return call(bitty.workspace.new) end })
bitty.commands.register({ id = "next", title = "Next", run = function()
  return call(bitty.workspace.next) end })
bitty.commands.register({ id = "focus", title = "Focus", run = function()
  return call(bitty.workspace.focus, 2) end })
bitty.commands.register({ id = "focus_index", title = "Focus index", run = function()
  return call(bitty.workspace.focus, { index = 3 }) end })
bitty.commands.register({ id = "close", title = "Close", run = function()
  return call(bitty.workspace.close) end })
bitty.commands.register({ id = "close_id", title = "Close id", run = function()
  return call(bitty.workspace.close, 2) end })
bitty.commands.register({ id = "rename", title = "Rename", run = function()
  return call(bitty.workspace.rename, 1, "editor") end })
bitty.commands.register({ id = "move", title = "Move", run = function()
  return call(bitty.workspace.move_panel, 2) end })
bitty.commands.register({ id = "bad", title = "Bad args", run = function()
  local out = {}
  out[#out + 1] = call(bitty.workspace.focus, 0)
  out[#out + 1] = call(bitty.workspace.focus, "ws1")
  out[#out + 1] = call(bitty.workspace.focus, { index = 0 })
  out[#out + 1] = call(bitty.workspace.close, -1)
  out[#out + 1] = call(bitty.workspace.rename, 1, "   ")
  out[#out + 1] = call(bitty.workspace.rename, 1, "a\nb")
  out[#out + 1] = call(bitty.workspace.rename, 1, string.rep("x", 257))
  out[#out + 1] = call(bitty.workspace.rename, 1, 42)
  out[#out + 1] = call(bitty.workspace.move_panel, nil)
  return table.concat(out, ",")
end })
bitty.commands.register({ id = "flood", title = "Flood", run = function()
  local accepted, refused = 0, 0
  for _ = 1, 100 do
    if bitty.workspace.new() then accepted = accepted + 1 else refused = refused + 1 end
  end
  return accepted .. "/" .. refused
end })
"#;

const PROBE_COMMANDS: &[&str] = &[
    "list",
    "new",
    "next",
    "focus",
    "focus_index",
    "close",
    "close_id",
    "rename",
    "move",
    "bad",
    "flood",
];

const MUTATIONS: &[&str] = &[
    "new",
    "next",
    "focus",
    "focus_index",
    "close",
    "close_id",
    "rename",
    "move",
];

fn probe_runtime(capabilities: &[&str]) -> (PluginRuntime, Rc<FixedWorkspaces>) {
    let root = temp_dir("probe");
    write_plugin(
        &root,
        "xuepoo.wsprobe",
        capabilities,
        PROBE_COMMANDS,
        &[],
        PROBE,
    );
    let source = Rc::new(FixedWorkspaces::default());
    *source.0.borrow_mut() = vec![row(1, "ws1", false, 2), row(2, "ws2", true, 1)];
    let mut rt = runtime(root, Some(source.clone()));
    activate_all(&mut rt);
    (rt, source)
}

fn run(rt: &mut PluginRuntime, command: &str) -> String {
    string(
        rt.dispatch_command(&pid("xuepoo.wsprobe"), command, &[])
            .unwrap_or_else(|error| panic!("dispatch {command}: {error}")),
    )
}

#[test]
fn list_and_every_mutation_denied_without_grants() {
    let (mut rt, _source) = probe_runtime(&[]);
    assert_eq!(run(&mut rt, "list"), "ERR:E_CAPABILITY_DENIED");
    for command in MUTATIONS {
        assert_eq!(
            run(&mut rt, command),
            "ERR:E_CAPABILITY_DENIED",
            "{command} must be denied without workspace.control"
        );
    }
    assert!(rt.drain_workspace_requests().is_empty());
}

#[test]
fn read_grant_lists_but_never_implies_control() {
    let (mut rt, _source) = probe_runtime(&["workspace.read"]);
    assert_eq!(
        run(&mut rt, "list"),
        "2=1|ws1|false|2|falsefalsefalse;2|ws2|true|1|falsefalsefalse"
    );
    for command in MUTATIONS {
        assert_eq!(
            run(&mut rt, command),
            "ERR:E_CAPABILITY_DENIED",
            "{command} must stay denied with workspace.read only"
        );
    }
    assert!(rt.drain_workspace_requests().is_empty());
}

#[test]
fn control_grant_enqueues_validated_requests_without_read() {
    let (mut rt, _source) = probe_runtime(&["workspace.control"]);
    // Control does not imply read either.
    assert_eq!(run(&mut rt, "list"), "ERR:E_CAPABILITY_DENIED");
    for command in MUTATIONS {
        assert_eq!(run(&mut rt, command), "OK:true", "{command} must enqueue");
    }
    let drained: Vec<WorkspaceRequest> = rt
        .drain_workspace_requests()
        .into_iter()
        .map(|queued| {
            assert_eq!(queued.plugin_id, "xuepoo.wsprobe");
            queued.request
        })
        .collect();
    assert_eq!(
        drained,
        vec![
            WorkspaceRequest::New,
            WorkspaceRequest::Next,
            WorkspaceRequest::FocusId(2),
            WorkspaceRequest::FocusIndex(3),
            WorkspaceRequest::Close(None),
            WorkspaceRequest::Close(Some(2)),
            WorkspaceRequest::Rename {
                id: 1,
                name: "editor".to_string()
            },
            WorkspaceRequest::MovePanel(2),
        ]
    );
    assert!(rt.drain_workspace_requests().is_empty(), "drain empties");
}

#[test]
fn invalid_mutation_arguments_fail_closed_before_enqueue() {
    let (mut rt, _source) = probe_runtime(&["workspace.control"]);
    assert_eq!(
        run(&mut rt, "bad"),
        [
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_LIMIT",
            "ERR:E_DEF_INVALID",
            "ERR:E_DEF_INVALID",
        ]
        .join(",")
    );
    assert!(rt.drain_workspace_requests().is_empty());
}

#[test]
fn request_queue_is_bounded_and_drops_newest() {
    let (mut rt, _source) = probe_runtime(&["workspace.control"]);
    let expected = format!(
        "{WORKSPACE_REQUEST_QUEUE_CAPACITY}/{}",
        100 - WORKSPACE_REQUEST_QUEUE_CAPACITY
    );
    assert_eq!(run(&mut rt, "flood"), expected);
    assert_eq!(
        rt.drain_workspace_requests().len(),
        WORKSPACE_REQUEST_QUEUE_CAPACITY
    );
    assert_eq!(
        rt.workspace_requests_dropped(),
        (100 - WORKSPACE_REQUEST_QUEUE_CAPACITY) as u64
    );
}

#[test]
fn list_is_bounded_at_max_workspaces_with_bounded_names() {
    let (mut rt, source) = probe_runtime(&["workspace.read"]);
    let long = "n".repeat(200);
    *source.0.borrow_mut() = (1..=(WORKSPACE_LIST_MAX_ITEMS as u64 + 4))
        .map(|id| row(id, &long, id == 1, 1))
        .collect();
    let out = run(&mut rt, "list");
    let (count, rows) = out.split_once('=').expect("count=rows");
    assert_eq!(count, WORKSPACE_LIST_MAX_ITEMS.to_string());
    let rows: Vec<&str> = rows.split(';').collect();
    assert_eq!(rows.len(), WORKSPACE_LIST_MAX_ITEMS);
    for line in rows {
        let name = line.split('|').nth(1).expect("name column");
        assert_eq!(
            name.chars().count(),
            bitty_runtime::WORKSPACE_NAME_MAX_CHARS
        );
    }
}

#[test]
fn granted_list_without_source_is_not_implemented() {
    let root = temp_dir("nosource");
    write_plugin(
        &root,
        "xuepoo.wsprobe",
        &["workspace.read"],
        PROBE_COMMANDS,
        &[],
        PROBE,
    );
    let mut rt = runtime(root, None);
    activate_all(&mut rt);
    assert_eq!(run(&mut rt, "list"), "ERR:E_NOT_IMPLEMENTED");
}

/// Listener: counts each workspace event kind it receives.
const LISTENER: &str = r##"
local seen = {}
local kinds = { "workspace.created", "workspace.closed", "workspace.renamed",
  "workspace.focused", "workspace.changed" }
for _, kind in ipairs(kinds) do
  bitty.events.subscribe(kind, function(event)
    seen[#seen + 1] = event.kind .. "#" .. tostring(event.payload.id)
  end)
end
bitty.commands.register({ id = "seen", title = "Seen", run = function()
  return table.concat(seen, ",")
end })
"##;

#[test]
fn workspace_events_require_read_grant_and_deliver_identity_only() {
    let root = temp_dir("events");
    write_plugin(
        &root,
        "xuepoo.wslisten",
        &["workspace.read"],
        &["seen"],
        WORKSPACE_EVENT_KINDS,
        LISTENER,
    );
    let mut rt = runtime(root, None);
    activate_all(&mut rt);
    for kind in WORKSPACE_EVENT_KINDS {
        let delivered = rt.deliver_event(kind, &LuaValue::table([("id", LuaValue::Integer(7))]));
        assert_eq!(
            delivered, 1,
            "{kind} must reach the read-granted subscriber once"
        );
    }
    let seen = string(
        rt.dispatch_command(&pid("xuepoo.wslisten"), "seen", &[])
            .expect("seen"),
    );
    let expected: Vec<String> = WORKSPACE_EVENT_KINDS
        .iter()
        .map(|kind| format!("{kind}#7"))
        .collect();
    assert_eq!(seen, expected.join(","));
}

#[test]
fn declaring_workspace_events_without_read_grant_fails_activation() {
    // Fail closed: a plugin cannot even subscribe (or trace) workspace
    // events without `workspace.read`; control alone is not enough.
    for capabilities in [&[][..], &["workspace.control"][..]] {
        let root = temp_dir("eventsdenied");
        write_plugin(
            &root,
            "xuepoo.wslisten",
            capabilities,
            &["seen"],
            WORKSPACE_EVENT_KINDS,
            LISTENER,
        );
        let mut rt = runtime(root, None);
        rt.discover();
        let results = rt.activate_discovered();
        assert_eq!(results.len(), 1);
        let error = results[0].1.as_ref().expect_err("activation must fail");
        assert!(
            error.to_string().contains("workspace.read"),
            "error must name the missing grant: {error}"
        );
        assert!(matches!(
            rt.state(&pid("xuepoo.wslisten")),
            Some(LifecycleState::Failed(_))
        ));
        assert_eq!(
            rt.deliver_event("workspace.created", &LuaValue::Nil),
            0,
            "a failed plugin never receives workspace events"
        );
    }
}
