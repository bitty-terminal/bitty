//! Generic host-mediated command-invocation path (CTX-1035, issue #1829).
//!
//! The palette and keybindings reach `dispatch_command` through one generic
//! path instead of the UI-click-only callers: `bitty.commands.list` serves
//! the active catalog, `bitty.commands.invoke` drives a host-mediated
//! cross-VM call, and [`PluginRuntime::invoke_command`] is the
//! application-layer entry (`owner:command` plus one args value).
//!
//! Deny-by-default, pinned headlessly with a devtools-shaped owner
//! (`bitty-featured.devtools`, empty-args string-result commands plus one
//! chrome-fitting notice — the devtools#10 consumer contract) and a
//! palette-shaped caller (`bitty-terminal.palette`):
//! - the happy path serves `bitty-featured.devtools:plugins` from both the
//!   Lua bridge and the application entry;
//! - undeclared, foreign-qualified, and malformed names are refused before
//!   any callee code runs;
//! - args that violate the callee schema are refused before any callee code
//!   runs (a dead-VM fixture proves no call was attempted);
//! - raising, result-violating, and re-entrant (self-invoking) callees are
//!   contained with codes — the host never panics and serves the next call.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bitty-command-invoke-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(roots: Vec<PathBuf>, data_dir: PathBuf) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))),
    });
    // W-146: disk-backed stores commit through an injected backend.
    common::install_stub_backend(&mut rt);
    rt
}

/// Write one dev-root package with explicit capabilities, reserved commands,
/// and init source.
fn write_plugin(root: &Path, id: &str, capabilities: &[&str], commands: &[&str], init_src: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let caps_toml = capabilities
        .iter()
        .map(|c| format!("{c} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    let commands_toml = commands
        .iter()
        .map(|c| format!("\"{id}:{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Invoke Test"
version = "0.1.0"
description = "command invocation test"

[compat]
plugin-api = "^1.0"

[capabilities]
{caps_toml}

[lazy]
commands = [{commands_toml}]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

const OWNER_ID: &str = "bitty-featured.devtools";
const CALLER_ID: &str = "bitty-terminal.palette";

/// Devtools-shaped owner: empty-args string-result commands plus one
/// chrome-fitting notice per successful command (the devtools#10 consumer
/// contract), a filtered command with an optional string arg, a raising
/// command, and a result-schema violator.
const OWNER_SRC: &str = r#"
local EMPTY = { type = "object", properties = {}, additionalProperties = false }
local STRING_RESULT = { type = "string" }

local function show(text)
  pcall(bitty.notify.show, { title = "Bitty DevTools", body = text, urgency = "low" })
  return text
end

bitty.commands.register({
  id = "plugins",
  title = "DevTools: list plugins",
  description = "Summarize plugin ids, versions, lifecycle states, and generations.",
  args_schema = EMPTY,
  result_schema = STRING_RESULT,
  run = function(_args)
    return show("2 plugins active")
  end,
})

bitty.commands.register({
  id = "trace-start",
  title = "DevTools: start event trace",
  description = "Start tracing the declared event kinds, optionally narrowed by a filter.",
  args_schema = {
    type = "object",
    properties = { filter = { type = "string", minLength = 1, maxLength = 128 } },
    additionalProperties = false,
  },
  result_schema = STRING_RESULT,
  run = function(args)
    local a = args or {}
    if a.filter ~= nil then
      return show("trace started (filter " .. a.filter .. ")")
    end
    return show("trace started (all declared kinds)")
  end,
})

bitty.commands.register({
  id = "boom",
  title = "DevTools: fail loudly",
  description = "Always raises, to prove failure containment.",
  args_schema = EMPTY,
  result_schema = STRING_RESULT,
  run = function(_args)
    error("boom went off")
  end,
})

bitty.commands.register({
  id = "bad-result",
  title = "DevTools: mistyped result",
  description = "Returns an integer against a string result schema.",
  args_schema = EMPTY,
  result_schema = STRING_RESULT,
  run = function(_args)
    return 42
  end,
})

bitty.commands.register({
  id = "plain",
  title = "DevTools: schemaless",
  description = "No schemas declared, legacy pass-through.",
  run = function(args)
    local a = args or {}
    return "plain:" .. tostring(a.note or "none")
  end,
})

return {}
"#;

/// Palette-shaped caller: `go` invokes whatever qualified target its args
/// name and reports the outcome as a `ok:`/`err:<code>` string; `ls`
/// reports the catalog size and first qualified name.
const CALLER_SRC: &str = r#"
bitty.commands.register({
  id = "go",
  title = "Palette: invoke target",
  description = "Invoke one qualified command through the host.",
  run = function(args)
    local a = args or {}
    local ok, result = pcall(bitty.commands.invoke, a.target, a.payload)
    if ok then
      return "ok:" .. tostring(result)
    end
    if type(result) == "table" and result.code ~= nil then
      return "err:" .. tostring(result.code)
    end
    return "err:raise"
  end,
})

bitty.commands.register({
  id = "ls",
  title = "Palette: list commands",
  description = "Report the host command catalog.",
  run = function(_args)
    local entries = bitty.commands.list()
    local first = "none"
    if #entries > 0 then
      first = entries[1].qualified
    end
    return #entries .. ":" .. first
  end,
})

return {}
"#;

struct World {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    owner: PluginId,
    caller: PluginId,
}

impl World {
    fn activate(tag: &str) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        write_plugin(
            &root,
            OWNER_ID,
            &["platform.notify"],
            &["plugins", "trace-start", "boom", "bad-result", "plain"],
            OWNER_SRC,
        );
        write_plugin(&root, CALLER_ID, &[], &["go", "ls"], CALLER_SRC);
        let mut runtime = runtime(vec![root.clone()], data.clone());
        runtime.discover();
        let owner = PluginId::new(OWNER_ID).expect("owner id");
        let caller = PluginId::new(CALLER_ID).expect("caller id");
        for id in [&owner, &caller] {
            let report = runtime.activate(id).expect("activate");
            assert_eq!(report.state, LifecycleState::Active);
        }
        Self {
            root,
            data,
            runtime,
            owner,
            caller,
        }
    }

    /// Dispatch the caller `go` command with a target (and optional payload
    /// table); the caller reports the outcome as a string.
    fn go(&mut self, target: &str, payload: LuaValue) -> String {
        let args = LuaValue::table([
            ("target", LuaValue::String(target.to_string())),
            ("payload", payload),
        ]);
        let result = self
            .runtime
            .dispatch_command(&self.caller.clone(), "go", &[args])
            .expect("caller go must dispatch");
        match result {
            LuaValue::String(text) => text,
            other => panic!("caller go must report a string, got {other:?}"),
        }
    }

    fn go_no_payload(&mut self, target: &str) -> String {
        let args = LuaValue::table([("target", LuaValue::String(target.to_string()))]);
        let result = self
            .runtime
            .dispatch_command(&self.caller.clone(), "go", &[args])
            .expect("caller go must dispatch");
        match result {
            LuaValue::String(text) => text,
            other => panic!("caller go must report a string, got {other:?}"),
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn empty_args() -> LuaValue {
    LuaValue::Table(Vec::new())
}

// ---------------------------------------------------------------------------
// Acceptance: palette -> devtools through the generic path
// ---------------------------------------------------------------------------

#[test]
fn palette_caller_invokes_devtools_plugins_from_the_bridge() {
    let mut world = World::activate("palette-bridge");
    // The palette-shaped caller invokes the devtools command through the
    // host-mediated bridge with empty args, exactly like a palette
    // selection would.
    let outcome = world.go_no_payload("bitty-featured.devtools:plugins");
    assert_eq!(outcome, "ok:2 plugins active");
    // Consumer contract: a string result plus exactly one chrome-fitting
    // notice from the callee.
    let notices = world.runtime.drain_notifications();
    assert_eq!(notices.len(), 1, "one notice per command, got {notices:?}");
    assert_eq!(notices[0].plugin_id, OWNER_ID);
    assert_eq!(notices[0].body, "2 plugins active");
}

#[test]
fn invoke_command_app_entry_serves_devtools_plugins() {
    let mut world = World::activate("app-entry");
    // The application-layer entry (palette selection / keybinding action)
    // serves the same command with the same contract.
    let result = world
        .runtime
        .invoke_command("bitty-featured.devtools:plugins", &empty_args())
        .expect("invoke");
    assert_eq!(result, LuaValue::String("2 plugins active".to_string()));
    let notices = world.runtime.drain_notifications();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].plugin_id, OWNER_ID);
}

#[test]
fn list_commands_catalog_serves_active_commands_sorted() {
    let mut world = World::activate("catalog");
    // Host-side catalog: every active command, sorted by (plugin, id).
    let catalog = world.runtime.list_commands();
    let qualified: Vec<&str> = catalog
        .iter()
        .map(|entry| entry.qualified.as_str())
        .collect();
    assert_eq!(
        qualified,
        vec![
            "bitty-featured.devtools:bad-result",
            "bitty-featured.devtools:boom",
            "bitty-featured.devtools:plain",
            "bitty-featured.devtools:plugins",
            "bitty-featured.devtools:trace-start",
            "bitty-terminal.palette:go",
            "bitty-terminal.palette:ls",
        ]
    );
    // The Lua bridge serves the same catalog to the palette caller.
    let result = world
        .runtime
        .dispatch_command(&world.caller.clone(), "ls", &[])
        .expect("caller ls must dispatch");
    assert_eq!(
        result,
        LuaValue::String("7:bitty-featured.devtools:bad-result".to_string())
    );
    // Suspended generations contribute nothing: the catalog never names a
    // command that would not serve.
    world
        .runtime
        .suspend(&world.owner.clone())
        .expect("suspend");
    let catalog = world.runtime.list_commands();
    let qualified: Vec<&str> = catalog
        .iter()
        .map(|entry| entry.qualified.as_str())
        .collect();
    assert_eq!(
        qualified,
        vec!["bitty-terminal.palette:go", "bitty-terminal.palette:ls"]
    );
    world.runtime.resume(&world.owner.clone()).expect("resume");
    assert_eq!(world.runtime.list_commands().len(), 7);
    // Disposed generations are revoked outright.
    world
        .runtime
        .dispose(&world.owner.clone())
        .expect("dispose");
    assert_eq!(world.runtime.list_commands().len(), 2);
}

// ---------------------------------------------------------------------------
// Deny-by-default: undeclared / foreign / malformed names
// ---------------------------------------------------------------------------

#[test]
fn undeclared_and_foreign_names_are_refused_without_execution() {
    let mut world = World::activate("deny-names");
    // Unknown plugin, unknown command, and foreign-qualified names (the
    // palette does not own `plugins`) are refused on both entries, and the
    // refusal emits no callee notice.
    for target in [
        "bitty-featured.missing:plugins",
        "bitty-featured.devtools:missing",
        "bitty-terminal.palette:plugins",
        "bitty-terminal.palette:trace-start",
    ] {
        let outcome = world.go_no_payload(target);
        assert_eq!(outcome, "err:E_COMMAND_UNKNOWN", "target {target}");
        let error = world
            .runtime
            .invoke_command(target, &empty_args())
            .expect_err("app entry must refuse");
        assert!(
            error.to_string().contains("not registered"),
            "target {target}: {error}"
        );
    }
    assert!(
        world.runtime.drain_notifications().is_empty(),
        "refusals must never execute callee code"
    );
    // The host still serves after the refusals.
    assert_eq!(
        world.go_no_payload("bitty-featured.devtools:plugins"),
        "ok:2 plugins active"
    );
}

#[test]
fn malformed_names_are_refused_at_both_layers() {
    let mut world = World::activate("deny-malformed");
    // The bridge shape check fires first on the Lua path.
    for target in ["nope", ":plugins", "owner:", "a:b:c", "has space:x"] {
        let outcome = world.go_no_payload(target);
        assert_eq!(outcome, "err:E_DEF_INVALID", "target {target}");
    }
    // The application entry refuses the same shapes without dispatching.
    for target in ["nope", ":plugins", "owner:", "a:b:c"] {
        let error = world
            .runtime
            .invoke_command(target, &empty_args())
            .expect_err("malformed must fail");
        assert!(
            error.to_string().contains("owner:command")
                || error.to_string().contains("not registered"),
            "target {target}: {error}"
        );
    }
    assert!(world.runtime.drain_notifications().is_empty());
}

// ---------------------------------------------------------------------------
// Deny-by-default: args validated against the callee schema
// ---------------------------------------------------------------------------

#[test]
fn args_schema_violation_is_refused_before_execution() {
    let mut world = World::activate("deny-args");
    // A mistyped `filter` violates the trace-start schema: refused before
    // any callee code runs (no notice), on both entries.
    let payload = LuaValue::table([("filter", LuaValue::Integer(7))]);
    let outcome = world.go("bitty-featured.devtools:trace-start", payload);
    assert_eq!(outcome, "err:E_COMMAND_INVALID");
    let error = world
        .runtime
        .invoke_command(
            "bitty-featured.devtools:trace-start",
            &LuaValue::table([("filter", LuaValue::Integer(7))]),
        )
        .expect_err("schema violation must fail");
    assert!(error.to_string().contains("schema"), "got {error}");
    assert!(
        world.runtime.drain_notifications().is_empty(),
        "schema refusals must never execute callee code"
    );
    // A well-typed filter serves; an absent filter serves (optional field).
    let payload = LuaValue::table([("filter", LuaValue::String("terminal.*".to_string()))]);
    assert_eq!(
        world.go("bitty-featured.devtools:trace-start", payload),
        "ok:trace started (filter terminal.*)"
    );
    assert_eq!(
        world.go_no_payload("bitty-featured.devtools:trace-start"),
        "ok:trace started (all declared kinds)"
    );
}

#[test]
fn multi_positional_args_are_refused_while_a_schema_is_declared() {
    let mut world = World::activate("deny-multi");
    let owner = world.owner.clone();
    let error = world
        .runtime
        .dispatch_command(&owner, "plugins", &[empty_args(), empty_args()])
        .expect_err("two positionals must fail");
    assert!(
        error.to_string().contains("at most one args table"),
        "got {error}"
    );
    assert!(world.runtime.drain_notifications().is_empty());
}

// ---------------------------------------------------------------------------
// Containment: raising / mistyped-result / re-entrant callees
// ---------------------------------------------------------------------------

#[test]
fn raising_callee_is_contained_and_the_host_serves_on() {
    let mut world = World::activate("contain-raise");
    let outcome = world.go_no_payload("bitty-featured.devtools:boom");
    assert_eq!(outcome, "err:E_COMMAND_FAILED");
    let error = world
        .runtime
        .invoke_command("bitty-featured.devtools:boom", &empty_args())
        .expect_err("raising callee must fail");
    assert!(error.to_string().contains("boom went off"), "got {error}");
    // The host is intact: the next invocation serves.
    assert_eq!(
        world.go_no_payload("bitty-featured.devtools:plugins"),
        "ok:2 plugins active"
    );
}

#[test]
fn result_schema_violation_is_contained() {
    let mut world = World::activate("contain-result");
    let outcome = world.go_no_payload("bitty-featured.devtools:bad-result");
    assert_eq!(outcome, "err:E_COMMAND_INVALID");
    let error = world
        .runtime
        .invoke_command("bitty-featured.devtools:bad-result", &empty_args())
        .expect_err("mistyped result must fail");
    assert!(error.to_string().contains("result"), "got {error}");
    assert_eq!(
        world.go_no_payload("bitty-featured.devtools:plugins"),
        "ok:2 plugins active"
    );
}

#[test]
fn self_invoke_is_contained_without_a_host_panic() {
    let mut world = World::activate("contain-reentrant");
    // A command invoking itself re-enters its already-borrowed VM: the
    // host fails the inner call closed (busy) instead of panicking the
    // `RefCell`, the outer call still returns, and the host serves on.
    let outcome = world.go_no_payload("bitty-terminal.palette:go");
    assert_eq!(outcome, "err:E_COMMAND_FAILED");
    assert_eq!(
        world.go_no_payload("bitty-featured.devtools:plugins"),
        "ok:2 plugins active"
    );
}

#[test]
fn suspended_owner_is_refused_then_serves_after_resume() {
    let mut world = World::activate("suspend-resume");
    world
        .runtime
        .suspend(&world.owner.clone())
        .expect("suspend");
    let outcome = world.go_no_payload("bitty-featured.devtools:plugins");
    assert_eq!(outcome, "err:E_COMMAND_GONE");
    let error = world
        .runtime
        .invoke_command("bitty-featured.devtools:plugins", &empty_args())
        .expect_err("suspended must fail");
    assert!(error.to_string().contains("suspended"), "got {error}");
    world.runtime.resume(&world.owner.clone()).expect("resume");
    assert_eq!(
        world.go_no_payload("bitty-featured.devtools:plugins"),
        "ok:2 plugins active"
    );
}

#[test]
fn schemaless_commands_keep_legacy_pass_through() {
    let mut world = World::activate("schemeless");
    // No schemas declared: any single args value passes through untouched.
    let result = world
        .runtime
        .invoke_command(
            "bitty-featured.devtools:plain",
            &LuaValue::table([("note", LuaValue::String("hi".to_string()))]),
        )
        .expect("schemeless invoke");
    assert_eq!(result, LuaValue::String("plain:hi".to_string()));
    let result = world
        .runtime
        .invoke_command("bitty-featured.devtools:plain", &empty_args())
        .expect("schemeless empty invoke");
    assert_eq!(result, LuaValue::String("plain:none".to_string()));
}

// ---------------------------------------------------------------------------
// Registration: schemas captured, malformed schemas fail activation
// ---------------------------------------------------------------------------

#[test]
fn non_table_schema_fails_closed_at_registration() {
    let root = temp_dir("bad-schema-root");
    let data = temp_dir("bad-schema-data");
    write_plugin(
        &root,
        "xuepoo.bad",
        &[],
        &["run"],
        r#"
        local ok, err = pcall(bitty.commands.register, {
          id = "run",
          title = "Bad schema",
          args_schema = "not-a-table",
          run = function() return true end,
        })
        if not ok then
          error(err.code)
        end
        return {}
        "#,
    );
    let mut runtime = runtime(vec![root.clone()], data.clone());
    runtime.discover();
    let id = PluginId::new("xuepoo.bad").expect("id");
    let error = runtime
        .activate(&id)
        .expect_err("non-table schema must fail");
    assert!(error.to_string().contains("E_DEF_INVALID"), "got {error}");
    // The failed generation publishes nothing: the name stays unknown.
    assert!(!runtime.command_directory().borrow().knows("xuepoo.bad:run"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn invalid_schema_table_fails_closed_at_activation() {
    let root = temp_dir("invalid-schema-root");
    let data = temp_dir("invalid-schema-data");
    write_plugin(
        &root,
        "xuepoo.invalid",
        &[],
        &["run"],
        r#"
        bitty.commands.register({
          id = "run",
          title = "Invalid schema",
          args_schema = { type = "object", properties = { a = { type = "object" } } },
          run = function() return true end,
        })
        return {}
        "#,
    );
    let mut runtime = runtime(vec![root.clone()], data.clone());
    runtime.discover();
    let id = PluginId::new("xuepoo.invalid").expect("id");
    // `properties` without an explicit `additionalProperties` is a
    // validation hole, so the commit gate refuses the activation.
    let error = runtime.activate(&id).expect_err("open schema must fail");
    assert!(error.to_string().contains("args_schema"), "got {error}");
    assert!(
        !runtime
            .command_directory()
            .borrow()
            .knows("xuepoo.invalid:run")
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}
