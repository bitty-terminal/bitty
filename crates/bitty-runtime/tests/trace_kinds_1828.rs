//! Issue #1828: remaining devtools-declared trace kinds.
//!
//! Decision under test: the host emits all seven missing kinds (bounded, off
//! the hot path, honoring the declared-kinds precondition) instead of
//! narrowing the devtools manifest. Covered here:
//!
//! - [`bell_observed_counts_every_bel_in_every_mode`]: the observed-`BEL`
//!   counter advances in every [`BellMode`](bitty_runtime::runtime::bell::BellMode),
//!   including `Off` (the user-visible policy still decides the flash/sink).
//! - [`headless_runtime_owns_no_live_terminals`]: without a PTY there is no
//!   terminal identity and no cwd report to emit (the app gates lifecycle and
//!   cwd diffs on live PTY state).
//! - [`trace_records_every_declared_kind`]: a trace whose owner declares the
//!   nine devtools kinds records all of them, including the seven that never
//!   fired before this issue.
//! - [`trace_records_nothing_undeclared`]: the least-privilege precondition —
//!   a trace records only kinds its owner declares.
//!
//! Headless and deterministic: no window, no GPU, no shell spawn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::Runtime;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use bitty_runtime::runtime::bell::BellMode;

/// The nine devtools-declared v1 observation kinds
/// (`bitty-plugin.toml` `[lazy] events` of `bitty-featured.devtools`).
const DEVTOOLS_EVENTS: &[&str] = &[
    "terminal.opened",
    "terminal.closed",
    "terminal.title-changed",
    "terminal.cwd-changed",
    "terminal.bell",
    "focus.changed",
    "selection.changed",
    "process.exited",
    "config.reloaded",
];

/// The seven kinds that never fired before issue #1828.
const MISSING_KINDS: &[&str] = &[
    "terminal.opened",
    "terminal.closed",
    "terminal.cwd-changed",
    "terminal.bell",
    "selection.changed",
    "process.exited",
    "config.reloaded",
];

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("bitty-1828-{tag}-{}-{counter}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_plugin(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    events: &[&str],
    init: &str,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let list = |items: &[&str]| {
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
    let qualified: Vec<&str> = qualified.iter().map(String::as_str).collect();
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Test"
version = "0.3.1"
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
            events = list(events),
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init).expect("init");
}

fn pid(id: &str) -> PluginId {
    PluginId::new(id).expect("id")
}

/// Tracer opening an unfiltered trace (`nil` opts: every declared kind) and
/// draining it to a comma-joined topic list.
const TRACER: &str = r#"
local handle = nil
bitty.commands.register({
  id = "start",
  title = "Start trace",
  run = function()
    handle = bitty.debug.trace(nil)
    return tostring(handle)
  end,
})
bitty.commands.register({
  id = "drain",
  title = "Drain trace",
  run = function()
    local result = bitty.debug.trace_get(handle)
    if result == nil then return "NIL" end
    local out = {}
    for _, record in ipairs(result.records) do out[#out + 1] = record.topic end
    return table.concat(out, ",")
  end,
})
"#;

const TRACER_ID: &str = "xuepoo.tracer-1828";

fn tracer_runtime(tag: &str, events: &[&str]) -> (PluginRuntime, PathBuf) {
    let root = temp_dir(tag);
    write_plugin(
        &root,
        TRACER_ID,
        &["debug.trace"],
        &["start", "drain"],
        events,
        TRACER,
    );
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(NoSnapshot),
    });
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(rt.state(&pid(TRACER_ID)), Some(&LifecycleState::Active));
    (rt, root)
}

fn run(rt: &mut PluginRuntime, command: &str) -> String {
    match rt
        .dispatch_command(&pid(TRACER_ID), command, &[])
        .unwrap_or_else(|error| panic!("{command}: dispatch failed: {error:?}"))
    {
        LuaValue::String(s) => s,
        other => panic!("{command}: expected string, got {other:?}"),
    }
}

fn rt() -> Runtime {
    Runtime::with_defaults().expect("headless runtime must build")
}

#[test]
fn bell_observed_counts_every_bel_in_every_mode() {
    for mode in [
        BellMode::Visual,
        BellMode::Audible,
        BellMode::Both,
        BellMode::Off,
    ] {
        let mut runtime = rt();
        runtime.set_bell_mode(mode);
        assert_eq!(runtime.bell_observed(), 0);
        runtime.handle_pty_bytes(b"\x07");
        assert_eq!(runtime.bell_observed(), 1, "mode {mode:?} must observe BEL");
        runtime.handle_pty_bytes(b"\x07\x07");
        assert_eq!(
            runtime.bell_observed(),
            3,
            "mode {mode:?} must count a burst"
        );
    }
}

#[test]
fn headless_runtime_owns_no_live_terminals() {
    let runtime = rt();
    assert!(!runtime.has_pty(), "headless runtime owns no primary PTY");
    assert!(runtime.live_terminal_ids().is_empty());
    assert!(runtime.terminal_cwds().is_empty());
}

#[test]
fn trace_records_every_declared_kind() {
    let (mut rt, root) = tracer_runtime("all-nine", DEVTOOLS_EVENTS);
    assert_eq!(run(&mut rt, "start"), "1");
    for kind in DEVTOOLS_EVENTS {
        rt.deliver_event(kind, &LuaValue::Table(Vec::new()));
    }
    let drained = run(&mut rt, "drain");
    for kind in MISSING_KINDS {
        assert!(
            drained.split(',').any(|topic| topic == *kind),
            "trace must record {kind}; drained: {drained}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trace_records_nothing_undeclared() {
    // The tracer declares only `terminal.opened`: every other delivered kind,
    // including the remaining devtools kinds, must stay out of its trace.
    let (mut rt, root) = tracer_runtime("least-privilege", &["terminal.opened"]);
    assert_eq!(run(&mut rt, "start"), "1");
    for kind in DEVTOOLS_EVENTS {
        rt.deliver_event(kind, &LuaValue::Table(Vec::new()));
    }
    assert_eq!(run(&mut rt, "drain"), "terminal.opened");
    let _ = std::fs::remove_dir_all(&root);
}
