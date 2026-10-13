#![forbid(unsafe_code)]
//! Colorscheme core hooks over a real plugin VM (CTX-1169, issue #1976).
//!
//! A granted plugin reads one wallpaper image under the approved
//! `background_image_roots` and enqueues preview/confirm theme intents
//! through the real Lua bridge. The drain live-applies previews with no
//! durable write and commits confirms through the persist backend first:
//! preview bytes never reach disk, confirm bytes land atomically via the
//! existing [`write_atomic_durably`] machinery. Headless and Windows-safe
//! (temp dirs only, no PTY spawn, no processes or sockets; the symlink case
//! stays in the unix-gated services unit test).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::Runtime;
use bitty_runtime::plugin_runtime::{
    EmptySettings, NativeFileSystem, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    ThemePersistHandler, write_atomic_durably,
};

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
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-colorscheme-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Minimal PNG carrying only signature, IHDR, and IEND: the image hook
/// sniffs headers without decoding, so no IDAT is needed.
fn tiny_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |ctype: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(ctype);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0u8; 4]);
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IEND", &[]);
    out
}

fn runtime_with_roots(root: PathBuf, image_roots: Vec<PathBuf>) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    rt.set_image_roots(image_roots);
    rt
}

/// Write a plugin package under `root/<id>/`; the probe `WALL` placeholder
/// is replaced with the wallpaper path before writing.
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

fn run(rt: &mut PluginRuntime, id: &PluginId, command: &str) -> String {
    string(
        rt.dispatch_command(id, command, &[])
            .unwrap_or_else(|error| panic!("dispatch {command}: {error}")),
    )
}

/// Probe plugin: every command reports `OK:<value>` or `ERR:<code>` across
/// the real Lua bridge. `WALL` is the wallpaper path substituted at write
/// time; `OUTSIDE` a path outside the approved roots.
const PROBE: &str = r##"
local function call(f, ...)
  local ok, result = pcall(f, ...)
  if ok then return "OK:" .. tostring(result) end
  return "ERR:" .. result.code
end

local theme_arg = {
  background = "#000000", foreground = "#ffffff",
  cursor = "#ff0000", selection = "#00ff00",
  ansi = {
    "#111111", "#222222", "#333333", "#444444",
    "#555555", "#666666", "#777777", "#888888",
    "#999999", "#aaaaaa", "#bbbbbb", "#cccccc",
    "#dddddd", "#eeeeee", "#abcdef", "#fedcba",
  },
}

bitty.commands.register({ id = "read_wall", title = "Read", run = function()
  local ok, d = pcall(bitty.image.read, "WALL")
  if not ok then return "ERR:" .. d.code end
  return "OK:" .. d.format .. ":" .. d.width .. "x" .. d.height .. ":" .. d.size
end })
bitty.commands.register({ id = "read_outside", title = "Outside", run = function()
  return call(bitty.image.read, "OUTSIDE") end })
bitty.commands.register({ id = "preview", title = "Preview", run = function()
  return call(bitty.theme.preview, theme_arg) end })
bitty.commands.register({ id = "confirm", title = "Confirm", run = function()
  return call(bitty.theme.confirm, theme_arg) end })
"##;

const PROBE_COMMANDS: &[&str] = &["read_wall", "read_outside", "preview", "confirm"];

/// Render `path` for embedding in Lua source (CTX-1169 Windows fix).
///
/// Backslashes are Lua escape introducers, so a native Windows path
/// (`C:\Temp\wall.png`) embeds as invalid escapes (`\T` fails the whole
/// `init.lua` with a parse error). Forward slashes survive Lua strings
/// verbatim and are accepted by Windows file APIs and the Core path trust
/// chain alike, so the probe normalizes unconditionally (a no-op on Unix).
fn lua_path(path: &Path) -> String {
    path.to_str()
        .expect("probe path is UTF-8")
        .replace('\\', "/")
}

fn probe_runtime(
    tag: &str,
    id: &str,
    capabilities: &[&str],
    wall: &Path,
    outside: &Path,
) -> (PluginRuntime, PluginId, PathBuf) {
    let root = temp_dir(tag);
    let src = PROBE
        .replace("WALL", &lua_path(wall))
        .replace("OUTSIDE", &lua_path(outside));
    write_plugin(&root, id, capabilities, PROBE_COMMANDS, &src);
    let mut rt = runtime_with_roots(
        root.clone(),
        vec![wall.parent().expect("parent").to_path_buf()],
    );
    activate_all(&mut rt);
    (rt, pid(id), root)
}

#[test]
fn activation_wires_image_roots_and_theme_queue_for_granted_plugin() {
    // CTX-1169: a plugin holding both closed grants reads the approved
    // wallpaper and enqueues preview/confirm intents over the real bridge.
    let walls = temp_dir("walls");
    let wall = walls.join("paper.png");
    let bytes = tiny_png(3, 2);
    std::fs::write(&wall, &bytes).expect("write png");
    let outside_dir = temp_dir("outside");
    let outside = outside_dir.join("secret.png");
    std::fs::write(&outside, tiny_png(1, 1)).expect("write outside");
    let (mut rt, id, _root) = probe_runtime(
        "granted",
        "xuepoo.colorprobe",
        &["platform.image-file", "runtime.configure"],
        &wall,
        &outside,
    );
    assert_eq!(
        run(&mut rt, &id, "read_wall"),
        format!("OK:png:3x2:{}", bytes.len())
    );
    assert_eq!(run(&mut rt, &id, "read_outside"), "ERR:E_IMAGE_DENIED");
    assert_eq!(run(&mut rt, &id, "preview"), "OK:true");
    assert_eq!(run(&mut rt, &id, "confirm"), "OK:true");
    let intents = rt.drain_theme_requests();
    assert_eq!(intents.len(), 2);
    assert!(!intents[0].persist, "preview never persists");
    assert!(intents[1].persist, "confirm persists");
}

#[test]
fn preview_applies_live_without_disk_and_confirm_commits_atomically() {
    // CTX-1169: the drain reuses the existing live-apply plus the existing
    // atomic file-write machinery (no fork). Preview bytes never reach
    // disk; confirm bytes land as one atomic `appearance.colors` write.
    let walls = temp_dir("persist");
    let wall = walls.join("paper.png");
    std::fs::write(&wall, tiny_png(1, 1)).expect("write png");
    let outside = temp_dir("persist-outside").join("x.png");
    let (mut rt, id, root) = probe_runtime(
        "persist",
        "xuepoo.colorpersist",
        &["platform.image-file", "runtime.configure"],
        &wall,
        &outside,
    );
    assert_eq!(run(&mut rt, &id, "preview"), "OK:true");
    assert_eq!(run(&mut rt, &id, "confirm"), "OK:true");
    let intents = rt.drain_theme_requests();
    assert_eq!(intents.len(), 2);
    let mut live = Runtime::with_defaults().expect("runtime");
    let before = live.config().theme;
    let target = root.join("colorscheme.lua");
    let target_write = target.clone();
    let backend: ThemePersistHandler = Rc::new(move |palette| {
        let ansi = palette
            .ansi
            .iter()
            .map(|slot| bitty_config::theme::CustomPalette::hex_entry(*slot))
            .map(|entry| format!("\"{entry}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let content = format!(
            "return {{ appearance = {{ colors = {{ background = \"{}\", foreground = \"{}\", cursor = \"{}\", selection = \"{}\", ansi = {{ {} }} }} }} }}\n",
            bitty_config::theme::CustomPalette::hex_entry(palette.background),
            bitty_config::theme::CustomPalette::hex_entry(palette.foreground),
            bitty_config::theme::CustomPalette::hex_entry(palette.cursor),
            bitty_config::theme::CustomPalette::hex_entry(palette.selection),
            ansi,
        );
        let temp = target_write.with_extension("lua.tmp");
        write_atomic_durably(&NativeFileSystem, &target_write, content.as_bytes(), &temp)
            .map_err(|err| BridgeError::new("io", "E_IO", format!("persist refused: {err}")))
    });
    live.apply_theme_request(&intents[0], &Some(backend.clone()))
        .expect("preview applies");
    assert_ne!(live.config().theme, before, "preview installs live");
    assert!(
        !target.exists(),
        "a preview never writes the persist target"
    );
    live.apply_theme_request(&intents[1], &Some(backend))
        .expect("confirm applies");
    let written = std::fs::read_to_string(&target).expect("confirm wrote the target");
    assert!(
        written.contains("background = \"#000000\""),
        "confirm commits the palette, got {written:?}"
    );
    assert!(
        written.contains("appearance"),
        "confirm writes a config shape"
    );
}

#[test]
fn colorscheme_verbs_denied_without_grants() {
    // CTX-1169: without either closed grant every verb denies first and the
    // shared queue stays empty.
    let walls = temp_dir("denied");
    let wall = walls.join("paper.png");
    std::fs::write(&wall, tiny_png(1, 1)).expect("write png");
    let outside = temp_dir("denied-outside").join("x.png");
    let (mut rt, id, _root) = probe_runtime("denied", "xuepoo.colordenied", &[], &wall, &outside);
    assert_eq!(run(&mut rt, &id, "read_wall"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, &id, "preview"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, &id, "confirm"), "ERR:E_CAPABILITY_DENIED");
    assert!(rt.drain_theme_requests().is_empty());
    assert_eq!(rt.theme_requests_dropped(), 0);
}

#[test]
fn lua_probe_paths_embed_without_backslash_escapes() {
    // CTX-1169 Windows fix: a native Windows path carries backslashes that
    // Lua source parses as escapes (`\T` in `\Temp` fails the whole
    // `init.lua` with a parse error, which surfaced as the Windows-only
    // registration capture failure). The probe normalizes to forward
    // slashes, so pin the property here with a literal Windows-style string:
    // Linux CI guards the Windows path too.
    let windows = Path::new(r"C:\Users\tester\Temp\wall paper.png");
    let embedded = lua_path(windows);
    assert_eq!(embedded, "C:/Users/tester/Temp/wall paper.png");
    assert!(
        !embedded.contains('\\'),
        "no raw backslash may reach Lua source, got {embedded:?}"
    );
    let src = PROBE.replace("WALL", &embedded);
    assert!(
        !src.contains('\\'),
        "the substituted probe must stay backslash-free"
    );
}
