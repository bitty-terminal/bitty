//! `bitty.config.preview`/`bitty.config.set` over real plugin VMs
//! (CTX-1613, bitty#2393).
//!
//! The write-only `config.manage` head is exercised through the real Lua
//! bridge: deny-by-default without the grant, a digest-bound preview/apply
//! pair, one-shot consent tokens, and shape refusals for paths, tables, and
//! extra arguments. The durable writer is a recording backend here; the
//! atomic user-layer writer is covered by the application's unit tests.
//! Headless and Windows-safe (temp dirs only).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    ConfigApplyOutcome, ConfigChange, ConfigWriteBackend, ConfigWriteFailure, EmptySettings,
    PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Recording backend: the service must hand it the exact rendered change.
#[derive(Default)]
struct RecordingBackend {
    calls: RefCell<Vec<ConfigChange>>,
    failure: Option<ConfigWriteFailure>,
}

impl ConfigWriteBackend for RecordingBackend {
    fn apply(&self, change: &ConfigChange) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        self.calls.borrow_mut().push(change.clone());
        match self.failure {
            Some(failure) => Err(failure),
            None => Ok(ConfigApplyOutcome::Applied),
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-config-manage-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf, backend: Option<Rc<RecordingBackend>>) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    if let Some(backend) = backend {
        rt.set_config_write_backend(Some(backend));
    }
    rt
}

/// Write a plugin package under `root/<id>/`.
fn write_plugin(root: &Path, id: &str, capabilities: &[&str], commands: &[&str], init_src: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let capabilities_toml = capabilities
        .iter()
        .map(|capability| format!("{capability} = true\n"))
        .collect::<String>();
    let qualified: Vec<String> = commands
        .iter()
        .map(|command| format!("\"{id}:{command}\""))
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
            commands = qualified.join(", "),
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

const PROBE: &str = r##"
local function errcode(f)
  local ok, result = pcall(f)
  return ok and "OK" or "ERR:" .. result.code
end
local function preview()
  local ok, p = pcall(bitty.config.preview, "appearance.theme", "catppuccin")
  if not ok then return nil, "ERR:" .. p.code end
  return p
end
bitty.commands.register({ id = "preview", title = "Preview", run = function()
  local p, err = preview()
  if not p then return err end
  return p.rendered
end })
bitty.commands.register({ id = "apply", title = "Apply", run = function()
  local p, err = preview()
  if not p then return err end
  local ok, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
  if not ok then return "ERR:" .. out.code end
  return tostring(out.applied) .. "|" .. out.reload
end })
bitty.commands.register({ id = "replay", title = "Replay", run = function()
  local p, err = preview()
  if not p then return err end
  local ok, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
  if not ok then return "ERR:" .. out.code end
  local again_ok, again = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
  if again_ok then return "REPLAYED" end
  return again.code
end })
bitty.commands.register({ id = "changed", title = "Changed", run = function()
  local p, err = preview()
  if not p then return err end
  local ok, out = pcall(bitty.config.set, "appearance.theme", "dracula", p.digest)
  if ok then return "APPLIED" end
  return out.code
end })
bitty.commands.register({ id = "bad", title = "Bad shapes", run = function()
  return table.concat({
    errcode(function() return bitty.config.preview("appearance.theme", "x", "extra") end),
    errcode(function() return bitty.config.set("appearance.theme", "x") end),
    errcode(function() return bitty.config.set("appearance.theme", "x", "d", "extra") end),
    errcode(function() return bitty.config.preview(1, "x") end),
    errcode(function() return bitty.config.preview("appearance.theme", {}) end),
    errcode(function() return bitty.config.preview("appearance.theme", true) end),
    errcode(function() return bitty.config.preview("appearance.theme", "line\nbreak") end),
    errcode(function() return bitty.config.set("appearance.theme", "catppuccin", "deadbeef") end),
  }, ",")
end })
bitty.commands.register({ id = "keys", title = "Bad keys", run = function()
  return table.concat({
    errcode(function() return bitty.config.preview("terminal.shell", "/bin/sh") end),
    errcode(function() return bitty.config.preview("/etc/bitty/init.lua", "x") end),
    errcode(function() return bitty.config.preview("../../init", "x") end),
    errcode(function() return bitty.config.preview("appearance", "x") end),
  }, ",")
end })
"##;

const PROBE_COMMANDS: &[&str] = &["preview", "apply", "replay", "changed", "bad", "keys"];

fn probe_runtime(
    declared: &[&str],
    backend: Option<Rc<RecordingBackend>>,
) -> (PluginRuntime, Rc<RecordingBackend>) {
    let root = temp_dir("probe");
    write_plugin(&root, "xuepoo.configprobe", declared, PROBE_COMMANDS, PROBE);
    let backend = backend.unwrap_or_else(|| Rc::new(RecordingBackend::default()));
    let mut rt = runtime(root, Some(Rc::clone(&backend)));
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    (rt, backend)
}

fn run(rt: &mut PluginRuntime, command: &str) -> String {
    string(
        rt.dispatch_command(&pid("xuepoo.configprobe"), command, &[])
            .unwrap_or_else(|error| panic!("dispatch {command}: {error}")),
    )
}

#[test]
fn writes_are_denied_without_the_config_manage_grant() {
    // The plugin declares no capability at all: the namespace is present but
    // every call fails closed, so a read-only dashboard cannot write.
    let (mut rt, backend) = probe_runtime(&[], None);
    assert_eq!(run(&mut rt, "preview"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CAPABILITY_DENIED");
    assert!(
        backend.calls.borrow().is_empty(),
        "nothing reached the writer"
    );
}

#[test]
fn preview_and_apply_carry_the_exact_rendered_change() {
    let (mut rt, backend) = probe_runtime(&["config.manage"], None);
    assert_eq!(
        run(&mut rt, "preview"),
        "appearance.theme = \"catppuccin\"",
        "the preview renders the exact change, not plugin-provided text"
    );
    assert!(backend.calls.borrow().is_empty(), "preview never writes");
    assert_eq!(run(&mut rt, "apply"), "true|auto");
    let calls = backend.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].key(), "appearance.theme");
    assert_eq!(calls[0].rendered(), "appearance.theme = \"catppuccin\"");
}

#[test]
fn consent_tokens_are_one_shot_and_value_bound() {
    let (mut rt, backend) = probe_runtime(&["config.manage"], None);
    assert_eq!(run(&mut rt, "replay"), "E_CONFIG_CONSENT_REQUIRED");
    assert_eq!(run(&mut rt, "changed"), "E_CONFIG_CONSENT_MISMATCH");
    assert_eq!(
        backend.calls.borrow().len(),
        1,
        "only the first apply wrote"
    );
}

#[test]
fn bridge_rejects_extra_arguments_and_non_scalar_values() {
    let (mut rt, backend) = probe_runtime(&["config.manage"], None);
    assert_eq!(
        run(&mut rt, "bad"),
        "ERR:E_DEF_INVALID,ERR:E_CONFIG_CONSENT_MISMATCH,ERR:E_DEF_INVALID,ERR:E_CONFIG_KEY,\
ERR:E_CONFIG_VALUE,ERR:E_CONFIG_VALUE,ERR:E_CONFIG_VALUE,ERR:E_CONFIG_CONSENT_MISMATCH"
    );
    assert!(backend.calls.borrow().is_empty(), "no malformed call wrote");
}

#[test]
fn no_path_or_command_key_is_addressable() {
    let (mut rt, backend) = probe_runtime(&["config.manage"], None);
    assert_eq!(
        run(&mut rt, "keys"),
        "ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY"
    );
    assert!(
        backend.calls.borrow().is_empty(),
        "no path-shaped key wrote"
    );
}

#[test]
fn granted_generation_without_a_backend_fails_closed() {
    let root = temp_dir("unwired");
    write_plugin(
        &root,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    let mut rt = runtime(root, None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(
        run(&mut rt, "preview"),
        "appearance.theme = \"catppuccin\"",
        "preview is host-side and still renders"
    );
    assert_eq!(run(&mut rt, "apply"), "ERR:E_NOT_IMPLEMENTED");
}
