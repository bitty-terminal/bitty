//! `bitty.config.preview`/`bitty.config.set` over real plugin VMs
//! (CTX-1613, bitty#2393).
//!
//! The write-only `config.manage` head is exercised through the real Lua
//! bridge: deny-by-default without the grant, dev-root packages that merely
//! declare the head do not receive it, a preview/apply pair bound to the
//! rendered candidate, one-shot approval tokens, and shape refusals for paths,
//! tables, and extra arguments. The durable writer is a recording backend
//! here; the atomic user-layer writer is covered by the application's unit
//! tests. Headless and Windows-safe (temp dirs only).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    ConfigApplyOutcome, ConfigChange, ConfigPreviewRendering, ConfigWriteBackend,
    ConfigWriteFailure, EmptySettings, PluginRecord, PluginRuntime, PluginRuntimeConfig,
    SnapshotSource, SourceClass, candidate_digest, content_digest, manifest_toml, write_index,
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
    stale: bool,
}

impl ConfigWriteBackend for RecordingBackend {
    fn preview(
        &self,
        _plugin_id: &str,
        change: &ConfigChange,
    ) -> Result<ConfigPreviewRendering, ConfigWriteFailure> {
        Ok(ConfigPreviewRendering {
            changed_keys: vec![change.key()],
            rendered: change.rendered().to_string(),
            candidate_digest: candidate_digest(change.rendered().as_bytes()),
        })
    }

    fn apply(
        &self,
        _plugin_id: &str,
        change: &ConfigChange,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        self.calls.borrow_mut().push(change.clone());
        if self.stale {
            return Err(ConfigWriteFailure::Stale);
        }
        if candidate_digest(change.rendered().as_bytes()) != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
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

fn runtime(
    store_root: Option<PathBuf>,
    dev_root: Option<PathBuf>,
    backend: Option<Rc<RecordingBackend>>,
) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(temp_dir("state")),
        store_root,
        bundled_roots: Vec::new(),
        third_party_roots: dev_root.into_iter().collect(),
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
    write_package_at(&root.join(id), id, capabilities, commands, init_src);
}

/// Write one plugin package into exactly `plugin`.
fn write_package_at(
    plugin: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    init_src: &str,
) {
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

/// Build an installed (`Registry`) store record whose recorded grant carries
/// `config.manage`, then write the store index.
fn write_store_package(
    store: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    init_src: &str,
) -> PluginRecord {
    let version = "0.1.0";
    let relative = format!("packages/{id}/{version}");
    let package_root = store.join(&relative);
    write_package_at(&package_root, id, capabilities, commands, init_src);
    let manifest_bytes =
        std::fs::read(package_root.join("bitty-plugin.toml")).expect("manifest bytes");
    let manifest = manifest_toml::parse_manifest(&manifest_bytes).expect("manifest parses");
    PluginRecord {
        source_class: SourceClass::Registry,
        plugin_id: id.to_string(),
        version: version.to_string(),
        root: relative,
        manifest_hash: manifest.manifest_hash(),
        content_digest: content_digest(&package_root).expect("content digest"),
        enabled: true,
        granted: vec!["config.manage".to_string()],
    }
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
  return p.rendered .. "|" .. table.concat(p.keys, ",")
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

/// Granted store package: a recorded grant is the only way a plugin receives
/// `config.manage` (review B2).
fn granted_runtime() -> (PluginRuntime, Rc<RecordingBackend>) {
    let store = temp_dir("store");
    let record = write_store_package(
        &store,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    write_index(&store, &[record]).expect("store index");
    let backend = Rc::new(RecordingBackend::default());
    let mut rt = runtime(Some(store), None, Some(Rc::clone(&backend)));
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    (rt, backend)
}

/// Development package that declares the write head but has no recorded
/// grant: the head must be withheld (review B2).
fn dev_runtime_without_record() -> (PluginRuntime, PathBuf) {
    let root = temp_dir("dev");
    write_plugin(
        &root,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    let mut rt = runtime(None, Some(root.clone()), None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    (rt, root)
}

fn run(rt: &mut PluginRuntime, command: &str) -> String {
    string(
        rt.dispatch_command(&pid("xuepoo.configprobe"), command, &[])
            .unwrap_or_else(|error| panic!("dispatch {command}: {error}")),
    )
}

#[test]
fn dev_root_declaration_without_a_record_gets_no_write_grant() {
    // A `BITTY_PLUGIN_DIR` package declaring `config.manage` still activates,
    // but the head is withheld and every write fails closed.
    let (mut rt, root) = dev_runtime_without_record();
    assert_eq!(run(&mut rt, "preview"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CAPABILITY_DENIED");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn writes_are_denied_without_the_config_manage_grant() {
    // A package with no declared capability at all: the namespace is present
    // but every call fails closed, so a read-only dashboard cannot write.
    let root = temp_dir("dev-none");
    write_plugin(&root, "xuepoo.configprobe", &[], PROBE_COMMANDS, PROBE);
    let mut rt = runtime(None, Some(root.clone()), None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(run(&mut rt, "preview"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CAPABILITY_DENIED");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn preview_and_apply_carry_the_exact_rendered_change() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "preview"),
        "appearance.theme = \"catppuccin\"|appearance.theme",
        "the preview renders the candidate and lists the changed keys"
    );
    assert!(backend.calls.borrow().is_empty(), "preview never writes");
    assert_eq!(run(&mut rt, "apply"), "true|auto");
    let calls = backend.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].key(), "appearance.theme");
    assert_eq!(calls[0].rendered(), "appearance.theme = \"catppuccin\"");
}

#[test]
fn approval_tokens_are_one_shot_and_value_bound() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(run(&mut rt, "replay"), "E_CONFIG_APPROVAL_REQUIRED");
    assert_eq!(run(&mut rt, "changed"), "E_CONFIG_APPROVAL_REQUIRED");
    assert_eq!(
        backend.calls.borrow().len(),
        1,
        "only the first apply wrote"
    );
}

#[test]
fn bridge_rejects_extra_arguments_and_non_scalar_values() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "bad"),
        "ERR:E_DEF_INVALID,ERR:E_CONFIG_APPROVAL_MISMATCH,ERR:E_DEF_INVALID,ERR:E_CONFIG_KEY,\
ERR:E_CONFIG_VALUE,ERR:E_CONFIG_VALUE,ERR:E_CONFIG_VALUE,ERR:E_CONFIG_APPROVAL_MISMATCH"
    );
    assert!(backend.calls.borrow().is_empty(), "no malformed call wrote");
}

#[test]
fn no_path_or_command_key_is_addressable() {
    let (mut rt, backend) = granted_runtime();
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
    let store = temp_dir("unwired");
    let record = write_store_package(
        &store,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    write_index(&store, &[record]).expect("store index");
    let mut rt = runtime(Some(store), None, None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(
        run(&mut rt, "preview"),
        "ERR:E_CONFIG_UNAVAILABLE",
        "no writable layer is a typed unavailable, not a generic not-implemented"
    );
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CONFIG_UNAVAILABLE");
}
