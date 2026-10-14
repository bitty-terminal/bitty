#![forbid(unsafe_code)]
//! Per-plugin user config files end-to-end (CTX-1195, issue #2010).
//!
//! Bar is the first customer: its manifest declares the four scalar settings
//! it reads today (`show`, `edge`, `show_single`, `name_max_chars`) plus the
//! new `modules` string array, and its Lua `read_settings` pattern (garbage
//! in, fail-to-default out) runs unchanged on top. This suite wires that
//! contract through the real path — manifest TOML, user TOML file,
//! activation, and the `bitty.settings.get` bridge — with a fixture plugin
//! carrying bar's exact declaration, because `bar` itself lives in another
//! repository and this repository must prove the mechanism, not the plugin.
//!
//! Proven here:
//!
//! - absent file (or absent `plugins/` dir) serves shipped defaults;
//! - a well-formed file overrides per key while untouched keys keep working
//!   from either home (defaults and the shared host source);
//! - hostile files (malformed TOML, wrong types, unknown keys) fail to
//!   defaults without aborting activation or widening authority;
//! - the manifest `[settings]` declaration is what the bridge serves: the
//!   Lua side observes exactly the declared keys.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    UnavailableSnapshot,
};

const BAR_ID: &str = "bitty-terminal.bar";

/// Bar's Lua `read_settings` pattern, verbatim in spirit: every absent or
/// invalid value falls back to its default so a bad configuration never
/// hides or breaks the bar. Served through `bitty.settings.get`, which the
/// host now resolves from manifest defaults plus the user file.
const BAR_SRC: &str = r#"
local function get(key)
  local ok, value = pcall(bitty.settings.get, key)
  if ok then return value end
  return nil
end

local function is_integer(value)
  return type(value) == "number" and value == math.floor(value)
end

local DEFAULTS = { show = true, edge = "bottom", show_single = false, name_max_chars = 32 }

local function read_settings()
  local opts = { show = DEFAULTS.show, edge = DEFAULTS.edge,
    show_single = DEFAULTS.show_single, name_max_chars = DEFAULTS.name_max_chars }
  local show = get("show")
  if type(show) == "boolean" then opts.show = show end
  local edge = get("edge")
  if type(edge) == "string" then opts.edge = edge end
  local show_single = get("show_single")
  if type(show_single) == "boolean" then opts.show_single = show_single end
  local name_max = get("name_max_chars")
  if is_integer(name_max) and name_max >= 1 and name_max <= 32 then
    opts.name_max_chars = name_max
  end
  return opts
end

bitty.commands.register({ id = "read", title = "Read", run = function()
  local opts = read_settings()
  local modules = get("modules")
  local modstr = "absent"
  if type(modules) == "table" then
    local parts = {}
    for _, name in ipairs(modules) do parts[#parts + 1] = tostring(name) end
    modstr = table.concat(parts, ",")
  end
  return "show=" .. tostring(opts.show) .. ";edge=" .. opts.edge
    .. ";show_single=" .. tostring(opts.show_single)
    .. ";name_max_chars=" .. tostring(opts.name_max_chars)
    .. ";modules=" .. modstr
end })
"#;

/// Bar's manifest declaration: the four scalars plus `modules`.
const BAR_MANIFEST: &str = r#"[plugin]
id = "bitty-terminal.bar"
name = "Bar"
version = "0.1.0"
description = "bar-fidelity settings fixture"

[compat]
plugin-api = "^1.0"

[settings]
show = { type = "bool", default = true }
edge = { type = "string", default = "bottom" }
show_single = { type = "bool", default = false }
name_max_chars = { type = "int", default = 32 }
modules = { type = "string-array", default = ["workspaces"] }

[lazy]
commands = ["bitty-terminal.bar:read"]
events = []
"#;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-settings-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_bar_plugin(root: &Path) {
    let plugin = root.join(BAR_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(plugin.join("bitty-plugin.toml"), BAR_MANIFEST).expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), BAR_SRC).expect("init");
}

fn write_user_file(config_dir: &Path, body: &[u8]) {
    let plugins = config_dir.join("plugins");
    std::fs::create_dir_all(&plugins).expect("plugins dir");
    std::fs::write(plugins.join(format!("{BAR_ID}.toml")), body).expect("user file");
}

fn activate_bar(root: PathBuf, config_dir: Option<PathBuf>) -> PluginRuntime {
    let data = temp_dir("data");
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(UnavailableSnapshot) as Rc<dyn SnapshotSource>,
    });
    runtime.set_settings_dir(config_dir);
    runtime.discover();
    let id = PluginId::new(BAR_ID).expect("id");
    let report = runtime.activate(&id).expect("bar fixture must activate");
    assert_eq!(report.state, LifecycleState::Active);
    runtime
}

fn read_settings(runtime: &mut PluginRuntime) -> String {
    let id = PluginId::new(BAR_ID).expect("id");
    match runtime.dispatch_command(&id, "read", &[]) {
        Ok(LuaValue::String(text)) => text,
        Ok(other) => panic!("read must report a string, got {other:?}"),
        Err(error) => panic!("read must dispatch: {error:?}"),
    }
}

#[test]
fn absent_file_serves_shipped_defaults_end_to_end() {
    let root = temp_dir("root-absent");
    write_bar_plugin(&root);
    // No config dir at all: every key serves the shipped default.
    let mut runtime = activate_bar(root, None);
    assert_eq!(
        read_settings(&mut runtime),
        "show=true;edge=bottom;show_single=false;name_max_chars=32;modules=workspaces"
    );
}

#[test]
fn user_file_overrides_per_key_end_to_end() {
    let root = temp_dir("root-override");
    write_bar_plugin(&root);
    let config = temp_dir("config-override");
    write_user_file(
        &config,
        b"show = false\nmodules = [\"clock\", \"workspaces\"]\n",
    );
    let mut runtime = activate_bar(root, Some(config));
    // Overridden keys serve the file; untouched keys keep their defaults.
    assert_eq!(
        read_settings(&mut runtime),
        "show=false;edge=bottom;show_single=false;name_max_chars=32;modules=clock,workspaces"
    );
}

#[test]
fn hostile_file_fails_to_defaults_without_abort_end_to_end() {
    let root = temp_dir("root-hostile");
    write_bar_plugin(&root);
    let config = temp_dir("config-hostile");
    // Wrong types (`show`, `name_max_chars`, `modules`), an unknown key
    // (`frobnicate`), and a float the file layer cannot even shape
    // (`edge`): every one fails to its default while the good key applies.
    write_user_file(
        &config,
        b"show = \"yes\"\nedge = 3.5\nshow_single = true\nname_max_chars = \"32\"\nmodules = \"clock\"\nfrobnicate = 7\n",
    );
    let mut runtime = activate_bar(root, Some(config));
    assert_eq!(
        read_settings(&mut runtime),
        "show=true;edge=bottom;show_single=true;name_max_chars=32;modules=workspaces"
    );
}

#[test]
fn malformed_file_keeps_plugin_alive_on_defaults_end_to_end() {
    let root = temp_dir("root-malformed");
    write_bar_plugin(&root);
    let config = temp_dir("config-malformed");
    // Structural garbage (a section header): the whole file falls back, the
    // plugin still activates and serves defaults. A `[capabilities]`-looking
    // section grants nothing — it is not even parsed as config.
    write_user_file(&config, b"[capabilities]\nui.rich = true\n");
    let mut runtime = activate_bar(root, Some(config));
    assert_eq!(
        read_settings(&mut runtime),
        "show=true;edge=bottom;show_single=false;name_max_chars=32;modules=workspaces"
    );
}
