//! `bitty.keymaps.list` over real plugin VMs (issue #2405).
//!
//! The bounded read-only keymap/action registry is the data half of the
//! accepted help-overlay ownership split (bitty#2397): a presentation plugin
//! reads the live resolved bindings through `runtime.keymaps.read`, a narrow
//! head that never implies `runtime.inspect`, `runtime.configure`, or any UI
//! grant. Each probe runs inside a registered command so calls cross the real
//! Lua bridge and grant snapshot. Headless and Windows-safe (temp dirs only).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_config::{Chord, ChromeAction, ResolvedKeymap};
use bitty_lua::keymaps::{KEYMAP_LIST_MAX_ROWS, KEYMAP_LIST_MAX_TOTAL};
use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, KeymapSource, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Fixed-row keymap source standing in for the app's `LiveKeymaps`.
#[derive(Default)]
struct FixedKeymaps(RefCell<Vec<ResolvedKeymap>>);

impl KeymapSource for FixedKeymaps {
    fn keymaps(&self) -> Result<Vec<ResolvedKeymap>, BridgeError> {
        Ok(self.0.borrow().clone())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-keymaps-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf, source: Option<Rc<FixedKeymaps>>) -> PluginRuntime {
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
        rt.set_keymap_source(source);
    }
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

const PROBE: &str = r##"
local function code(f)
  local ok, err = pcall(f)
  return ok and "OK" or "ERR:" .. err.code
end
local function serialize(page)
  local defaults, users, commands, titles = 0, 0, 0, 0
  for _, row in ipairs(page.entries) do
    assert(type(row.chord) == "string" and type(row.action) == "string")
    assert(type(row.source) == "string" and type(row.context) == "string")
    if row.source == "default" then defaults = defaults + 1 else users = users + 1 end
    if row.command then commands = commands + 1 end
    if row.title then titles = titles + 1 end
    if row.action:sub(1, 8) == "command:" then
      assert(row.command and not row.title and row.action == "command:" .. row.command)
    else
      assert(row.title and not row.command)
    end
  end
  return page.total .. ":" .. page.offset .. ":" .. tostring(page.truncated) .. ":"
    .. #page.entries .. ":" .. defaults .. ":" .. users .. ":" .. commands .. ":" .. titles
end
bitty.commands.register({ id = "list", title = "List", run = function()
  local ok, page = pcall(bitty.keymaps.list)
  if not ok then return "ERR:" .. page.code end
  return serialize(page)
end })
bitty.commands.register({ id = "page", title = "Page", run = function()
  local ok, page = pcall(bitty.keymaps.list, { offset = 2, limit = 2 })
  if not ok then return "ERR:" .. page.code end
  return serialize(page)
end })
bitty.commands.register({ id = "bad", title = "Bad", run = function()
  return table.concat({
    code(function() return bitty.keymaps.list("all") end),
    code(function() return bitty.keymaps.list({ offset = "0" }) end),
    code(function() return bitty.keymaps.list({ limit = 1.5 }) end),
    code(function() return bitty.keymaps.list({ limit = 0 }) end),
    code(function() return bitty.keymaps.list({ offset = 1025 }) end),
    code(function() return bitty.keymaps.list({ limit = 257 }) end),
  }, ",")
end })
bitty.commands.register({ id = "configure", title = "Configure", run = function()
  local ansi = {}
  for i = 1, 16 do ansi[i] = "#000000" end
  local theme = {
    background = "#000000", foreground = "#ffffff", cursor = "#ffffff",
    selection = "#333333", ansi = ansi,
  }
  return code(function() return bitty.theme.preview(theme) end)
end })
bitty.commands.register({ id = "inspect", title = "Inspect", run = function()
  return code(function() return bitty.runtime.inspect() end)
end })
bitty.commands.register({ id = "flood", title = "Flood", run = function()
  local accepted, refused = 0, 0
  for _ = 1, 20 do
    local ok = pcall(bitty.keymaps.list)
    if ok then accepted = accepted + 1 else refused = refused + 1 end
  end
  return accepted .. "/" .. refused
end })
"##;

const PROBE_COMMANDS: &[&str] = &["list", "page", "bad", "configure", "inspect", "flood"];

fn default_table() -> Vec<ResolvedKeymap> {
    bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults")
}

fn probe_runtime(capabilities: &[&str]) -> (PluginRuntime, Rc<FixedKeymaps>) {
    let root = temp_dir("probe");
    write_plugin(
        &root,
        "xuepoo.keymapprobe",
        capabilities,
        PROBE_COMMANDS,
        PROBE,
    );
    let source = Rc::new(FixedKeymaps::default());
    *source.0.borrow_mut() = default_table();
    let mut rt = runtime(root, Some(source.clone()));
    activate_all(&mut rt);
    (rt, source)
}

fn run(rt: &mut PluginRuntime, command: &str) -> String {
    string(
        rt.dispatch_command(&pid("xuepoo.keymapprobe"), command, &[])
            .unwrap_or_else(|error| panic!("dispatch {command}: {error}")),
    )
}

#[test]
fn list_and_paging_denied_without_the_grant() {
    let (mut rt, _source) = probe_runtime(&[]);
    assert_eq!(run(&mut rt, "list"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, "page"), "ERR:E_CAPABILITY_DENIED");
}

#[test]
fn grant_reads_the_live_table_in_canonical_rows() {
    let (mut rt, source) = probe_runtime(&["runtime.keymaps.read"]);
    let table = default_table();
    let defaults = table.iter().filter(|binding| binding.from_default).count();
    let titled = table
        .iter()
        .filter(|binding| !matches!(binding.action, ChromeAction::InvokeCommand(_)))
        .count();
    assert_eq!(
        run(&mut rt, "list"),
        format!(
            "{}:0:false:{}:{defaults}:0:0:{titled}",
            table.len(),
            table.len()
        ),
        "the full page is served with only canonical registry rows"
    );
    // Registry data only: no config path, argv, environment value, URL, or
    // terminal content can appear in any field (the probe asserts the row
    // shape and the command/title exclusivity in Lua).
    assert!(!run(&mut rt, "list").contains('/'), "no path-shaped field");

    // Regeneration: the source is read per call, so a republished table is
    // visible on the very next call with no cache in between.
    *source.0.borrow_mut() = vec![ResolvedKeymap {
        chord: Chord::parse("alt+9").expect("chord"),
        action: ChromeAction::CloseView,
        context: "global".to_string(),
        from_default: true,
    }];
    assert_eq!(run(&mut rt, "list"), "1:0:false:1:1:0:0:1");
}

#[test]
fn paging_honors_offset_and_limit() {
    let (mut rt, _source) = probe_runtime(&["runtime.keymaps.read"]);
    let table = default_table();
    let page = &table[2..4];
    let defaults = page.iter().filter(|binding| binding.from_default).count();
    let commands = page
        .iter()
        .filter(|binding| matches!(binding.action, ChromeAction::InvokeCommand(_)))
        .count();
    assert_eq!(
        run(&mut rt, "page"),
        format!(
            "{}:2:true:{}:{defaults}:{}:{commands}:{}",
            table.len(),
            page.len(),
            page.len() - defaults,
            page.len() - commands,
        ),
        "offset 2 with limit 2 returns the third and fourth rows and flags the tail"
    );
}

#[test]
fn bad_arguments_fail_closed_before_the_host() {
    let (mut rt, _source) = probe_runtime(&["runtime.keymaps.read"]);
    assert_eq!(
        run(&mut rt, "bad"),
        [
            "ERR:E_VALUE_TYPE",
            "ERR:E_VALUE_TYPE",
            "ERR:E_VALUE_TYPE",
            "ERR:E_DEF_LIMIT",
            "ERR:E_DEF_LIMIT",
            "ERR:E_DEF_LIMIT",
        ]
        .join(",")
    );
}

#[test]
fn read_never_implies_configure_or_inspect() {
    let (mut rt, _source) = probe_runtime(&["runtime.keymaps.read"]);
    assert_eq!(
        run(&mut rt, "configure"),
        "ERR:E_CAPABILITY_DENIED",
        "a keymap read must not authorize runtime.configure"
    );
    assert_eq!(
        run(&mut rt, "inspect"),
        "ERR:E_CAPABILITY_DENIED",
        "a keymap read must not authorize runtime.inspect"
    );
}

#[test]
fn inspect_never_implies_the_keymap_read() {
    let (mut rt, _source) = probe_runtime(&["runtime.inspect"]);
    assert_eq!(run(&mut rt, "list"), "ERR:E_CAPABILITY_DENIED");
}

#[test]
fn addressable_rows_are_capped_at_the_registry_bound() {
    let (mut rt, source) = probe_runtime(&["runtime.keymaps.read"]);
    // Past the 1024 addressable cap: the tail is not reachable and
    // `truncated` reports it.
    *source.0.borrow_mut() = (0..KEYMAP_LIST_MAX_TOTAL + 76)
        .map(|_| ResolvedKeymap {
            chord: Chord::parse("alt+x").expect("chord"),
            action: ChromeAction::CloseView,
            context: "global".to_string(),
            from_default: true,
        })
        .collect();
    let rows = KEYMAP_LIST_MAX_ROWS;
    assert_eq!(
        run(&mut rt, "list"),
        format!("{KEYMAP_LIST_MAX_TOTAL}:0:true:{rows}:{rows}:0:0:{rows}")
    );
}

#[test]
fn per_generation_budget_stops_a_call_flood() {
    let (mut rt, _source) = probe_runtime(&["runtime.keymaps.read"]);
    assert_eq!(run(&mut rt, "flood"), "16/4");
}
