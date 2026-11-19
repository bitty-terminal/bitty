#![cfg(unix)]
//! `bitty.ui.panel.*` over a real plugin VM, wired to the landed Core content
//! swap (bitty#2388 slice 3, CTX-1909).
//!
//! Contract rows proven here (candidate
//! `specifications/panel-gui-content-presentation-candidate.md`, sections
//! "Recommended API shape (candidate v2)", "PTY preservation semantics",
//! "Focus, close, and fail-closed paths", and "Per-placement and limits"):
//!
//! 1. **Present on a terminal-backed panel, release restores it.** A Lua
//!    `bitty.ui.panel.present` returns a host-derived handle, the panel's
//!    content kind flips to `Rich`, the displaced terminal keeps its child, its
//!    `TIOCGWINSZ`, and every byte it parsed while hidden, and
//!    `bitty.ui.panel.release` restores the recorded kind and issues exactly one
//!    resize.
//! 2. **The placement hint is recorded, not applied.** `placement = "floating"`
//!    leaves the leaf exactly where the user had it; only the plugin row
//!    remembers the request.
//! 3. **A non-terminal target is refused** with `E_UI_PANEL_UNAVAILABLE`.
//! 4. **One layer per panel, one per generation.** A second owner is
//!    `E_UI_PANEL_BUSY`; the same generation re-opening is `E_UI_PANEL_NESTED`.
//! 5. **Stale and foreign handles are refused** with `E_UI_NOT_OWNER`.
//! 6. **A bad scene and a bad `spec` are `E_UI_COMPONENT_INVALID`** — including
//!    the record's explicit `band` refusal and an unknown/extra `spec` key.
//! 7. **Capability gating.** Presenting needs both `ui.rich` (the scene) and
//!    `ui.panel` (content + input), each refused by name when missing, and a
//!    generation with the grant but no host backend fails closed with
//!    `E_UI_UNAVAILABLE`.
//! 8. **Update replaces content only.** `bitty.ui.panel.update` repaints the
//!    layer and leaves the displaced terminal, its winsize, and the record as
//!    they were.
//! 9. **A handle from another generation fences.** The same owner at a later
//!    generation is refused `E_UI_NOT_OWNER` on both content verbs, and the
//!    refusal leaves the row, the painted kind, the child, and the restore
//!    count untouched (fail-closed, not just an error string).
//! 10. **Suspend releases the layer.** Driving the real `PluginRuntime::suspend`
//!     edge restores the recorded terminal kind, drops the plugin row, and
//!     leaves the pre-suspend handle dead once the plugin resumes.
//! 11. **Reload releases the layer and advances the generation.** The real
//!     `PluginRuntime::reload` edge restores the terminal, advances the
//!     generation by exactly one, and neither generation can address the
//!     other's layer afterwards.
//!
//! # Verification limits
//!
//! Rows 1, 2, 4, 5, 8, 9, 10, and 11 read a live pane's kernel winsize or grid,
//! so they call `bitty_test_support::require_pty!()` as their **first
//! statement** and skip on a build without a PTY backend (the workspace skip
//! convention, not evidence). Rows 3, 6, and 7 need no PTY: they assert a typed
//! code produced before any terminal is touched.
//!
//! Row 9 addresses the fence through `Runtime::ui_panel_update` /
//! `Runtime::ui_panel_release` rather than through a Lua verb, because no
//! lifecycle edge leaves a row behind: `suspend`, `dispose`, `reload`, and the
//! failure rollback all release the dying generation's layers (rows 10 and 11
//! drive two of them). The only shape that leaves a row is the teardown whose
//! `release_owner` reported zero on a borrow conflict, and the fence's
//! generation arm exists for exactly that shape. Those two `Runtime` entries are
//! the ones `RuntimePanelContentBackend` calls, so the row exercises the real
//! fence, not a test-only path through it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_platform::PhysicalSize;
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig, RuntimePanelContentBackend,
    SnapshotSource,
};
use bitty_runtime::registry::{PanelRegistryConfig, PanelRuntime, PanelType, WorkspaceId};
use bitty_runtime::{LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};
use bitty_ui::panel::ViewContent;

mod common;

/// Primary leaf (paints the runtime-global primary grid, keyed by `primary_view`).
const PRIMARY: ViewId = ViewId::new(1);
/// Pane leaf (owns its own shell session).
const PANE: ViewId = ViewId::new(2);

/// The one plugin every single-plugin fixture uses.
const PLUGIN: &str = "xuepoo.panel";

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
        "bitty-ui-panel-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn two_panes() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    )
}

/// A runtime with one `Terminal` panel mounted at `mount`, laid out so `focus`
/// is the focused leaf.
///
/// `mount` and `focus` differ only for the refusal row that presents into a
/// panel which hosts no live terminal.
fn live_runtime(layout: LayoutNode, mount: ViewId, focus: ViewId) -> Rc<RefCell<Runtime>> {
    let mut runtime = Runtime::with_defaults().expect("headless defaults must build");
    runtime.set_layout(layout);
    runtime.set_container(UiRect::new(0, 0, 80, 24));
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host");
    let handle = host
        .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
        .expect("terminal panel");
    host.mount_panel(handle.id, handle.generation, mount)
        .expect("panel mounts at the leaf");
    runtime.install_scene_host(host);
    assert!(runtime.set_focus(focus), "the fixture focus must be live");
    Rc::new(RefCell::new(runtime))
}

/// A plugin runtime over `root`, with the shared-runtime content backend
/// installed (or without one, for the fail-closed row).
fn plugin_runtime(root: &Path, live: Rc<RefCell<Runtime>>, with_backend: bool) -> PluginRuntime {
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.to_path_buf()],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    if with_backend {
        runtime.set_panel_content_backend(Some(Rc::new(RuntimePanelContentBackend::new(live))));
    }
    common::install_harness_seams(&mut runtime);
    runtime
}

/// Commands every fixture plugin registers.
const COMMANDS: &[&str] = &[
    "present",
    "present_floating",
    "present_extra_key",
    "present_band",
    "present_bad_scene",
    "present_nested_spec",
    "present_message",
    "present_twice",
    "update",
    "update_handle",
    "release",
    "release_handle",
    "release_bad_reason",
    "release_twice",
    "release_unknown",
];

/// The plugin body shared by every fixture.
///
/// `present` keeps the handle in a plugin-local upvalue, so a later command can
/// update or release the layer the way a real plugin would. Every verb reports
/// `OK:<value>` or `ERR:<code>`: the two-value `handle, err` shape is the design
/// record's signature, so a refusal travels as a value and the code is the one
/// the record names.
const INIT: &str = r#"
local handle = nil

local function report(value, err)
  if err then return "ERR:" .. err.code end
  return "OK:" .. tostring(value)
end

local function present(scene, spec)
  local value, err = bitty.ui.panel.present(scene, spec)
  if not err then handle = value end
  return report(value, err)
end

local SCENE = { kind = "Text", text = "panel" }
local UPDATED = { kind = "Text", text = "updated-panel-content" }

bitty.commands.register({ id = "present", title = "Present", run = function()
  return present(SCENE, {})
end })

bitty.commands.register({ id = "present_floating", title = "Present floating", run = function()
  return present(SCENE, { placement = "floating" })
end })

bitty.commands.register({ id = "present_extra_key", title = "Present extra key", run = function()
  return present(SCENE, { mode = "fullscreen" })
end })

bitty.commands.register({ id = "present_band", title = "Present band", run = function()
  return present(SCENE, { placement = "band" })
end })

bitty.commands.register({ id = "present_bad_scene", title = "Present bad scene", run = function()
  return present({ kind = "Nope" }, {})
end })

bitty.commands.register({ id = "present_nested_spec", title = "Present nested spec", run = function()
  return present(SCENE, { placement = { mode = "floating" } })
end })

bitty.commands.register({ id = "present_message", title = "Present message", run = function()
  local value, err = bitty.ui.panel.present(SCENE, {})
  if err then return "ERR:" .. err.message end
  handle = value
  return "OK:" .. tostring(value)
end })

bitty.commands.register({ id = "present_twice", title = "Present twice", run = function()
  local first = present(SCENE, {})
  local second = present(SCENE, {})
  return first .. "|" .. second
end })

bitty.commands.register({ id = "update", title = "Update", run = function()
  local ok, err = bitty.ui.panel.update(handle, UPDATED)
  return report(ok, err)
end })

bitty.commands.register({ id = "update_handle", title = "Update handle", run = function(target)
  local ok, err = bitty.ui.panel.update(target, UPDATED)
  return report(ok, err)
end })

bitty.commands.register({ id = "release", title = "Release", run = function()
  local ok, err = bitty.ui.panel.release(handle)
  return report(ok, err)
end })

bitty.commands.register({ id = "release_handle", title = "Release handle", run = function(target)
  local ok, err = bitty.ui.panel.release(target, "released")
  return report(ok, err)
end })

bitty.commands.register({ id = "release_bad_reason", title = "Release bad reason", run = function()
  local ok, err = bitty.ui.panel.release(1, "whatever")
  return report(ok, err)
end })

bitty.commands.register({ id = "release_twice", title = "Release twice", run = function()
  local first = report(bitty.ui.panel.release(handle))
  local second = report(bitty.ui.panel.release(handle))
  return first .. "|" .. second
end })

bitty.commands.register({ id = "release_unknown", title = "Release unknown", run = function()
  local ok, err = bitty.ui.panel.release(424242, "released")
  return report(ok, err)
end })
"#;

/// Write one fixture plugin whose `init.lua` registers [`COMMANDS`].
fn write_plugin(root: &Path, id: &str, capabilities: &[&str]) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let capabilities_toml = capabilities
        .iter()
        .map(|capability| format!("{capability} = true\n"))
        .collect::<String>();
    let qualified = COMMANDS
        .iter()
        .map(|command| format!("\"{id}:{command}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Panel content test"
version = "0.1.0"
description = "bitty.ui.panel host api test"

[compat]
plugin-api = "^1.0"

[capabilities]
{capabilities_toml}
[lazy]
commands = [{qualified}]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), INIT).expect("init");
}

struct Fixture {
    root: PathBuf,
    plugins: PluginRuntime,
    live: Rc<RefCell<Runtime>>,
}

impl Fixture {
    /// One plugin with `capabilities` over a two-pane runtime focused on `PANE`.
    fn new(tag: &str, capabilities: &[&str]) -> Self {
        Self::build(
            tag,
            &[(PLUGIN, capabilities)],
            two_panes(),
            PANE,
            PANE,
            true,
        )
    }

    /// One plugin with a custom runtime shape (`layout`, mounted panel, focus).
    fn shaped(
        tag: &str,
        capabilities: &[&str],
        layout: LayoutNode,
        mount: ViewId,
        focus: ViewId,
    ) -> Self {
        Self::build(tag, &[(PLUGIN, capabilities)], layout, mount, focus, true)
    }

    fn build(
        tag: &str,
        declared: &[(&str, &[&str])],
        layout: LayoutNode,
        mount: ViewId,
        focus: ViewId,
        with_backend: bool,
    ) -> Self {
        let root = temp_dir(tag);
        for (id, capabilities) in declared {
            write_plugin(&root, id, capabilities);
        }
        let live = live_runtime(layout, mount, focus);
        let mut plugins = plugin_runtime(&root, Rc::clone(&live), with_backend);
        plugins.discover();
        for (id, _) in declared {
            let activated = PluginId::new(id).expect("valid id");
            let report = plugins.activate(&activated).expect("activate");
            assert_eq!(report.state, LifecycleState::Active);
        }
        Self {
            root,
            plugins,
            live,
        }
    }

    /// Run one command on `PLUGIN`, returning its `OK:`/`ERR:` report.
    fn run(&mut self, command: &str) -> String {
        self.run_on(PLUGIN, command)
    }

    fn run_on(&mut self, plugin: &str, command: &str) -> String {
        self.run_on_args(plugin, command, &[])
    }

    /// Run one command on `plugin` with Lua arguments.
    fn run_on_args(&mut self, plugin: &str, command: &str, args: &[LuaValue]) -> String {
        let id = PluginId::new(plugin).expect("valid id");
        common::settle_dispatch(&mut self.plugins, &id, command, args).expect_string(command)
    }

    /// Run one command on `PLUGIN` that must succeed, returning its payload.
    fn run_ok(&mut self, command: &str) -> String {
        let report = self.run(command);
        match report.strip_prefix("OK:") {
            Some(value) => value.to_string(),
            None => panic!("{command} must succeed, got {report}"),
        }
    }

    /// Run one command on `PLUGIN` that must be refused, returning its code.
    fn run_err(&mut self, command: &str) -> String {
        let report = self.run(command);
        match report.strip_prefix("ERR:") {
            Some(code) if !code.contains(' ') => code.to_string(),
            _ => panic!("{command} must be refused with a code, got {report}"),
        }
    }

    fn spawn_pane(&self) {
        self.live
            .borrow_mut()
            .spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], 40, 12)
            .expect("pane shell spawns");
        self.live.borrow_mut().tick().expect("baseline frame");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn pane_text(live: &Rc<RefCell<Runtime>>, view: ViewId) -> String {
    match live.borrow().pane_snapshot(&view) {
        Some(snapshot) => snapshot.cells.iter().map(|cell| cell.glyph).collect(),
        None => String::new(),
    }
}

/// Whitespace-separated words of a pane grid.
///
/// The grid pads every row to its width, and a restore reflows to the restored
/// width, so "no byte the hidden pane parsed is lost" is asserted on words: the
/// content is the invariant, the trailing padding is layout.
fn grid_words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn pane_frame(live: &Rc<RefCell<Runtime>>, view: ViewId) -> (u16, u16) {
    let frame = live
        .borrow()
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .expect("the pane leaf is presented");
    (frame.cols, frame.rows)
}

/// Runs one command whose Lua body reports two halves: the first must succeed,
/// the second must be refused with `code`.
fn run_pair(fixture: &mut Fixture, command: &str) -> (String, String) {
    let report = fixture.run(command);
    let (first, second) = report
        .split_once('|')
        .unwrap_or_else(|| panic!("{command} must report both halves, got {report}"));
    let first = first
        .strip_prefix("OK:")
        .unwrap_or_else(|| panic!("{command} first half must succeed, got {report}"))
        .to_string();
    let second = second
        .strip_prefix("ERR:")
        .unwrap_or_else(|| panic!("{command} second half must be refused, got {report}"))
        .to_string();
    (first, second)
}

// ---------------------------------------------------------------------------
// Row 1: present and release, terminal preserved across the swap
// ---------------------------------------------------------------------------

#[test]
fn present_then_release_restores_the_terminal_with_one_resize() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::new("roundtrip", &["ui.rich", "ui.panel"]);
    fixture.spawn_pane();
    fixture
        .live
        .borrow_mut()
        .handle_pane_bytes(PANE, b"before-the-layer\r\n");
    let pid = fixture.live.borrow().pane_pid(&PANE).expect("pane child");
    let winsize = fixture.live.borrow().pane_pty_size(&PANE).expect("winsize");
    let before = pane_text(&fixture.live, PANE);
    assert!(
        before.contains("before-the-layer"),
        "the pane grid must carry the pre-swap output: {before:?}"
    );

    let handle: i64 = fixture.run_ok("present").parse().expect("integer handle");
    assert!(handle > 0, "the host mints a positive handle");
    {
        let live = fixture.live.borrow();
        let token = live
            .ui_panel_layer(handle)
            .expect("the handle names a layer")
            .token
            .get();
        assert_eq!(
            live.panel_content_kind(PANE),
            Some(ViewContent::Rich(token)),
            "the panel presents GUI content under the layer's Core token"
        );
        assert!(live.content_layer_presented(PANE));
        assert_eq!(
            live.pane_pid(&PANE),
            Some(pid),
            "presenting a layer must not respawn the displaced child"
        );
        assert_eq!(
            live.pane_pty_size(&PANE),
            Some(winsize),
            "presenting a layer must not resize the hidden PTY"
        );
    }
    assert_eq!(
        pane_text(&fixture.live, PANE),
        before,
        "the hidden grid must be byte-identical after the swap"
    );

    // Hidden output still parses: the displaced terminal is hidden, not
    // suspended, so nothing it wrote is lost across the swap.
    fixture
        .live
        .borrow_mut()
        .handle_pane_bytes(PANE, b"while-hidden\r\n");
    let hidden = pane_text(&fixture.live, PANE);
    assert!(
        hidden.contains("while-hidden") && hidden.contains("before-the-layer"),
        "the displaced terminal keeps parsing while hidden: {hidden:?}"
    );
    let hidden_words = grid_words(&hidden);

    // A window resize while the layer is up freezes the hidden geometry, so the
    // restore is the only resize the PTY ever sees.
    fixture
        .live
        .borrow_mut()
        .handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");
    assert_eq!(
        fixture.live.borrow().pane_pty_size(&PANE),
        Some(winsize),
        "a window resize must not reach the hidden PTY"
    );

    assert_eq!(fixture.run_ok("release"), "true");
    let live = fixture.live.borrow();
    assert_eq!(
        live.panel_content_kind(PANE),
        Some(ViewContent::Terminal(PANE.0)),
        "release restores the recorded content kind exactly"
    );
    assert!(!live.content_layer_presented(PANE));
    assert_eq!(
        grid_words(&pane_text(&fixture.live, PANE)),
        hidden_words,
        "restore keeps every byte the hidden pane parsed (the grid reflows to \
         the restored width, so whole words are the invariant)"
    );
    assert_eq!(live.pane_pid(&PANE), Some(pid));
    assert_eq!(
        live.content_restore_resizes(),
        1,
        "exactly one restore resize, issued by the release"
    );
    assert_eq!(
        live.pane_pty_size(&PANE),
        Some(pane_frame(&fixture.live, PANE)),
        "the restored PTY agrees with the leaf's current content rect"
    );
    assert_eq!(
        live.ui_panel_layer_count(),
        0,
        "the handle died with the layer"
    );
}

// ---------------------------------------------------------------------------
// Row 2: the placement hint is recorded, never applied
// ---------------------------------------------------------------------------

#[test]
fn a_placement_hint_is_recorded_and_not_applied() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::new("hint", &["ui.rich", "ui.panel"]);
    fixture.spawn_pane();
    let frame_before = pane_frame(&fixture.live, PANE);
    let mode_before = fixture
        .live
        .borrow()
        .view_presentation(PANE)
        .expect("the pane leaf carries a mode");

    let handle: i64 = fixture
        .run_ok("present_floating")
        .parse()
        .expect("integer handle");
    {
        let live = fixture.live.borrow();
        assert_eq!(
            live.ui_panel_layer(handle)
                .expect("the handle names a layer")
                .spec
                .placement,
            Some(bitty_lua::UiPanelPlacement::Floating),
            "the descriptor is recorded exactly as the caller sent it"
        );
        assert_eq!(
            pane_frame(&fixture.live, PANE),
            frame_before,
            "a placement hint never moves or resizes the panel"
        );
        assert_eq!(
            live.view_presentation(PANE),
            Some(mode_before),
            "a placement hint never changes the panel's placement"
        );
    }
    assert_eq!(fixture.run_ok("release"), "true");
}

// ---------------------------------------------------------------------------
// Row 3: a non-terminal target is refused
// ---------------------------------------------------------------------------

#[test]
fn present_on_a_non_terminal_target_is_unavailable() {
    // No PTY and no spawned pane: `PANE` owns no session and is not the
    // primary owner, so the panel it hosts presents no live terminal — the
    // refusal under test.
    let mut fixture = Fixture::shaped(
        "no-target",
        &["ui.rich", "ui.panel"],
        two_panes(),
        PANE,
        PANE,
    );
    assert_eq!(fixture.run_err("present"), "E_UI_PANEL_UNAVAILABLE");
    let live = fixture.live.borrow();
    assert!(
        !live.content_layer_presented(PANE),
        "a refused present leaves no layer"
    );
    assert_eq!(live.ui_panel_layer_count(), 0, "and no handle");
}

// ---------------------------------------------------------------------------
// Row 4: one layer per panel, one per generation
// ---------------------------------------------------------------------------

#[test]
fn a_second_owner_is_busy_and_the_same_generation_is_nested() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::build(
        "busy",
        &[
            (PLUGIN, &["ui.rich", "ui.panel"]),
            ("xuepoo.second", &["ui.rich", "ui.panel"]),
        ],
        two_panes(),
        PANE,
        PANE,
        true,
    );
    fixture.spawn_pane();
    // The same generation re-opening its own panel is a nesting bug, not
    // contention: the first present succeeds and the second is refused.
    let (handle, nested) = run_pair(&mut fixture, "present_twice");
    assert_eq!(nested, "E_UI_PANEL_NESTED");
    let handle: i64 = handle.parse().expect("integer handle");

    // A different owner on the same focused panel: busy.
    let busy = fixture.run_on("xuepoo.second", "present");
    assert_eq!(busy, "ERR:E_UI_PANEL_BUSY");

    // The owner's layer is untouched by both refusals.
    let live = fixture.live.borrow();
    assert_eq!(live.ui_panel_layer_count(), 1);
    assert_eq!(
        live.ui_panel_layer(handle).map(|layer| layer.view),
        Some(PANE)
    );
}

// ---------------------------------------------------------------------------
// Row 5: stale and foreign handles are refused
// ---------------------------------------------------------------------------

#[test]
fn a_stale_or_foreign_handle_is_not_owner() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::build(
        "owner",
        &[
            (PLUGIN, &["ui.rich", "ui.panel"]),
            ("xuepoo.foreign", &["ui.rich", "ui.panel"]),
        ],
        two_panes(),
        PANE,
        PANE,
        true,
    );
    fixture.spawn_pane();
    let handle: i64 = fixture.run_ok("present").parse().expect("integer handle");

    // While the layer is live, another generation holding its handle value is
    // refused on both content verbs, and the owner's layer survives.
    let foreign = fixture.run_on_args(
        "xuepoo.foreign",
        "release_handle",
        &[LuaValue::Integer(handle)],
    );
    assert_eq!(foreign, "ERR:E_UI_NOT_OWNER");
    let foreign = fixture.run_on_args(
        "xuepoo.foreign",
        "update_handle",
        &[LuaValue::Integer(handle)],
    );
    assert_eq!(foreign, "ERR:E_UI_NOT_OWNER");
    assert_eq!(
        fixture.live.borrow().ui_panel_layer_count(),
        1,
        "a foreign refusal never removes the owner's layer"
    );

    // The owner's own handle still works, then dies with the release: a second
    // release, and every stale value, names nothing.
    assert_eq!(fixture.run_ok("update"), "true");
    let (released, stale) = run_pair(&mut fixture, "release_twice");
    assert_eq!(released, "true");
    assert_eq!(stale, "E_UI_NOT_OWNER", "a released handle names nothing");
    assert_eq!(fixture.run_err("release"), "E_UI_NOT_OWNER");
    assert_eq!(fixture.run_err("release_unknown"), "E_UI_NOT_OWNER");
    assert_eq!(
        fixture.live.borrow().ui_panel_layer_count(),
        0,
        "the owner's release dropped the row"
    );
}

// ---------------------------------------------------------------------------
// Row 6: a bad scene and a bad spec are typed refusals
// ---------------------------------------------------------------------------

#[test]
fn an_invalid_scene_or_spec_is_component_invalid() {
    let mut fixture = Fixture::new("invalid", &["ui.rich", "ui.panel"]);
    assert_eq!(
        fixture.run_err("present_extra_key"),
        "E_UI_COMPONENT_INVALID",
        "an unknown spec key is a refusal, never ignored"
    );
    assert_eq!(
        fixture.run_err("present_band"),
        "E_UI_COMPONENT_INVALID",
        "the record refuses `band` as panel content"
    );
    assert_eq!(
        fixture.run_err("present_bad_scene"),
        "E_UI_COMPONENT_INVALID",
        "a tree admission rejects is the component code"
    );
    assert_eq!(
        fixture.run_err("present_nested_spec"),
        "E_UI_COMPONENT_INVALID",
        "a nested table where the flat spec carries a string is a refusal"
    );
    assert_eq!(
        fixture.run_err("release_bad_reason"),
        "E_UI_COMPONENT_INVALID",
        "the release disposition set is closed, refused before the host call"
    );
    assert_eq!(fixture.live.borrow().ui_panel_layer_count(), 0);
}

// ---------------------------------------------------------------------------
// Row 7: capability gating and the fail-closed host
// ---------------------------------------------------------------------------

#[test]
fn presenting_needs_the_ui_panel_grant_beside_ui_rich() {
    let mut fixture = Fixture::new("rich-only", &["ui.rich"]);
    assert_eq!(fixture.run_err("present"), "E_CAPABILITY_DENIED");
    assert!(
        fixture.run("present_message").contains("ui.panel"),
        "the refusal names the missing grant: {}",
        fixture.run("present_message")
    );
    assert_eq!(fixture.live.borrow().ui_panel_layer_count(), 0);
}

#[test]
fn a_generation_without_the_ui_rich_grant_cannot_present_a_scene() {
    let mut fixture = Fixture::new("panel-only", &["ui.panel"]);
    assert_eq!(fixture.run_err("present"), "E_CAPABILITY_DENIED");
    assert!(
        fixture.run("present_message").contains("ui.rich"),
        "the scene grant is refused first and by name: {}",
        fixture.run("present_message")
    );
}

#[test]
fn a_generation_without_a_host_backend_fails_closed() {
    let fixture = Fixture::build(
        "no-backend",
        &[(PLUGIN, &["ui.rich", "ui.panel"])],
        two_panes(),
        PANE,
        PANE,
        false,
    );
    let mut fixture = fixture;
    assert_eq!(fixture.run_err("present"), "E_UI_UNAVAILABLE");
}

// ---------------------------------------------------------------------------
// Row 8: update replaces content only
// ---------------------------------------------------------------------------

#[test]
fn update_repaints_the_layer_and_leaves_the_terminal_alone() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::new("update", &["ui.rich", "ui.panel"]);
    fixture.spawn_pane();
    fixture
        .live
        .borrow_mut()
        .handle_pane_bytes(PANE, b"kept\r\n");
    let pid = fixture.live.borrow().pane_pid(&PANE).expect("pane child");
    let winsize = fixture.live.borrow().pane_pty_size(&PANE).expect("winsize");
    let handle: i64 = fixture.run_ok("present").parse().expect("integer handle");
    let token = fixture
        .live
        .borrow()
        .ui_panel_layer(handle)
        .expect("layer")
        .token;
    let before = fixture
        .live
        .borrow_mut()
        .tick()
        .expect("presented frame")
        .glyphs;

    assert_eq!(fixture.run_ok("update"), "true");
    let after = fixture
        .live
        .borrow_mut()
        .tick()
        .expect("updated frame")
        .glyphs;
    assert!(
        after > before,
        "the longer updated text must repaint ({after} vs {before} glyphs)"
    );
    {
        let live = fixture.live.borrow();
        assert_eq!(
            live.ui_panel_layer(handle).map(|layer| layer.token),
            Some(token),
            "update keeps the same layer record and token"
        );
        assert_eq!(
            live.panel_content_layer(PANE).map(|layer| layer.token),
            Some(token),
            "the Core record is untouched by an update"
        );
        assert_eq!(live.pane_pid(&PANE), Some(pid), "no respawn on update");
        assert_eq!(
            live.pane_pty_size(&PANE),
            Some(winsize),
            "no resize on update"
        );
    }
    assert_eq!(fixture.run_ok("release"), "true");
}

// ---------------------------------------------------------------------------
// Row 9: a previous generation's handle cannot touch a live layer
// ---------------------------------------------------------------------------

/// The generation half of the handle fence (`ui_panel_update` and
/// `ui_panel_release`).
///
/// No lifecycle edge leaves a row behind: suspend, dispose, reload, and the
/// failure rollback all reach `clear_ui_blocks`, which releases the dying
/// generation's layers (rows 10 and 11). The generation arm therefore defends
/// the one shape that can leave a row — a teardown whose release reported zero
/// because the shared runtime was borrowed
/// (`RuntimePanelContentBackend::release_owner`). This row builds that shape on
/// the public `Runtime` entries the backend calls: a live row, addressed by the
/// same owner at a later generation.
#[test]
fn a_previous_generations_handle_cannot_update_or_release_a_live_layer() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::new("generation-fence", &["ui.rich", "ui.panel"]);
    fixture.spawn_pane();
    let handle: i64 = fixture.run_ok("present").parse().expect("integer handle");
    let (generation, token, pid) = {
        let live = fixture.live.borrow();
        let layer = live
            .ui_panel_layer(handle)
            .expect("the handle names a layer");
        assert_eq!(
            live.panel_content_kind(PANE),
            Some(ViewContent::Rich(layer.token.get())),
            "the fixture must present a live layer"
        );
        (
            layer.generation,
            layer.token,
            live.pane_pid(&PANE).expect("pane child"),
        )
    };
    let successor = generation + 1;
    let owner = PluginId::new(PLUGIN).expect("valid id");
    let node = bitty_lua::UiRichNode::Text {
        text: "successor-generation".to_string(),
        bold: false,
    };
    // Move the leaf's content rect while the layer is up, so a restore would
    // issue exactly one resize: the refusal assertions below then prove *no*
    // restore ran, rather than leaning on a counter that is zero by default.
    fixture
        .live
        .borrow_mut()
        .handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");

    // The same owner at a later generation is a stranger to this layer.
    let updated = fixture
        .live
        .borrow_mut()
        .ui_panel_update(&owner, successor, handle, &node)
        .map_err(|error| error.code);
    assert_eq!(
        updated,
        Err("E_UI_NOT_OWNER"),
        "a later generation must not repaint an earlier generation's layer"
    );
    let released = fixture
        .live
        .borrow_mut()
        .ui_panel_release(&owner, successor, handle, "released")
        .map_err(|error| error.code);
    assert_eq!(
        released,
        Err("E_UI_NOT_OWNER"),
        "a later generation must not release an earlier generation's layer"
    );

    // Fail-closed is *unchanged*, not merely an error code: the refusal must
    // leave the row, the painted kind, and the displaced child exactly as they
    // were.
    {
        let live = fixture.live.borrow();
        assert_eq!(
            live.ui_panel_layer_count(),
            1,
            "a refused call drops no row and adds none"
        );
        assert_eq!(
            live.ui_panel_layer(handle)
                .map(|layer| (layer.generation, layer.token)),
            Some((generation, token)),
            "the row still names the generation that presented it"
        );
        assert_eq!(
            live.panel_content_kind(PANE),
            Some(ViewContent::Rich(token.get())),
            "the panel still presents the layer it presented before"
        );
        assert!(live.content_layer_presented(PANE), "the layer is still up");
        assert_eq!(
            live.pane_pid(&PANE),
            Some(pid),
            "a refused release restored nothing (and respawned nothing)"
        );
        assert_eq!(
            live.content_restore_resizes(),
            0,
            "a refused release issued no restore resize"
        );
    }

    // The generation that presented it still holds it: the fence refuses the
    // stranger, not the owner.
    assert_eq!(fixture.run_ok("release"), "true");
    assert_eq!(
        fixture.live.borrow().content_restore_resizes(),
        1,
        "the owner's release is the one restore"
    );
}

// ---------------------------------------------------------------------------
// Row 10: suspend releases the layer before the plugin parks
// ---------------------------------------------------------------------------

/// `PluginRuntime::suspend` reaches `PluginServices::clear_ui_blocks`, which is
/// the generation edge that must hand the panel back before the placeholder is
/// presented. A deleted `PanelContentBackend::release_owner` call there leaves
/// the layer painting over a terminal nobody owns, and this row is what sees it.
#[test]
fn suspend_releases_the_layer_and_the_stale_handle_cannot_restore() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::new("teardown-suspend", &["ui.rich", "ui.panel"]);
    fixture.spawn_pane();
    let pid = fixture.live.borrow().pane_pid(&PANE).expect("pane child");
    let handle: i64 = fixture.run_ok("present").parse().expect("integer handle");
    let token = fixture
        .live
        .borrow()
        .ui_panel_layer(handle)
        .expect("the handle names a layer")
        .token;

    // Move the content rect while the layer is up so the teardown restore is
    // observable: a teardown that did *not* release the layer issues no resize.
    fixture
        .live
        .borrow_mut()
        .handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");

    let id = PluginId::new(PLUGIN).expect("valid id");
    fixture
        .plugins
        .suspend(&id)
        .expect("an active plugin suspends");

    // The teardown release ran before the plugin parked: the recorded terminal
    // is back, and the row died with the generation.
    {
        let live = fixture.live.borrow();
        assert_eq!(
            live.panel_content_kind(PANE),
            Some(ViewContent::Terminal(PANE.0)),
            "a suspended generation's layer must not stay on screen"
        );
        assert_eq!(
            live.ui_panel_layer_count(),
            0,
            "the dead generation's row must not survive the teardown"
        );
        assert!(!live.content_layer_presented(PANE), "no layer is presented");
        assert_eq!(
            live.content_restore_resizes(),
            1,
            "the teardown restore is the only resize the hidden PTY sees"
        );
        assert_eq!(live.pane_pid(&PANE), Some(pid), "the same child came back");
        assert!(
            live.panel_content_layer(PANE).map(|layer| layer.token) != Some(token),
            "the Core record no longer carries the released token"
        );
    }

    // After resume the plugin is live again, and the pre-suspend handle is a
    // dead value: refusing it must restore nothing a second time.
    fixture.plugins.resume(&id).expect("resume");
    let stale = fixture.run_on_args(PLUGIN, "release_handle", &[LuaValue::Integer(handle)]);
    assert_eq!(
        stale, "ERR:E_UI_NOT_OWNER",
        "a handle from before the teardown names nothing"
    );
    let live = fixture.live.borrow();
    assert_eq!(
        live.panel_content_kind(PANE),
        Some(ViewContent::Terminal(PANE.0)),
        "the stale release did not put the layer back"
    );
    assert_eq!(
        live.content_restore_resizes(),
        1,
        "the refused release restored nothing"
    );
}

// ---------------------------------------------------------------------------
// Row 11: a reloaded generation starts released, and owns nothing from before
// ---------------------------------------------------------------------------

/// The reload edge runs the same teardown and then rebuilds the generation.
/// This row proves three things the suite previously could not see: the layer
/// is released at the teardown, the real reload path advances the generation by
/// one, and neither direction of the fence crosses generations afterwards (the
/// dead generation cannot release the live layer, and the live generation does
/// not adopt the dead handle).
#[test]
fn a_reloaded_generation_releases_the_layer_and_the_predecessor_owns_nothing() {
    bitty_test_support::require_pty!();
    let mut fixture = Fixture::new("teardown-reload", &["ui.rich", "ui.panel"]);
    fixture.spawn_pane();
    let handle: i64 = fixture.run_ok("present").parse().expect("integer handle");
    let (generation, pid) = {
        let live = fixture.live.borrow();
        let layer = live
            .ui_panel_layer(handle)
            .expect("the handle names a layer");
        (layer.generation, live.pane_pid(&PANE).expect("pane child"))
    };

    // Move the content rect while the layer is up so the reload teardown's
    // restore is observable.
    fixture
        .live
        .borrow_mut()
        .handle_resize(PhysicalSize::new(1200, 700))
        .expect("window resize applies");

    let id = PluginId::new(PLUGIN).expect("valid id");
    let report = fixture.plugins.reload(&id).expect("reload");
    assert_eq!(
        report.state,
        LifecycleState::Active,
        "the rebuilt generation is live"
    );

    {
        let live = fixture.live.borrow();
        assert_eq!(
            live.panel_content_kind(PANE),
            Some(ViewContent::Terminal(PANE.0)),
            "a reloaded generation's layer must not stay on screen"
        );
        assert_eq!(
            live.ui_panel_layer_count(),
            0,
            "the predecessor's row must not survive the reload"
        );
        assert_eq!(
            live.content_restore_resizes(),
            1,
            "the reload teardown is the only restore"
        );
        assert_eq!(live.pane_pid(&PANE), Some(pid), "the same child came back");
    }

    // The rebuilt generation holds the panel: it presents its own layer, from a
    // real advanced generation, and the old handle value is not it.
    let fresh: i64 = fixture.run_ok("present").parse().expect("integer handle");
    assert_ne!(fresh, handle, "a reload never reuses a handle");
    let fresh_generation = fixture
        .live
        .borrow()
        .ui_panel_layer(fresh)
        .expect("the new layer")
        .generation;
    assert_eq!(
        fresh_generation,
        generation + 1,
        "the reload path advanced the plugin generation by exactly one"
    );
    // Move the rect again while the successor's layer is up: a refusal that
    // restored the terminal would show up as a second restore resize.
    fixture
        .live
        .borrow_mut()
        .handle_resize(PhysicalSize::new(1000, 640))
        .expect("second window resize applies");

    // The predecessor generation cannot reach the successor's layer.
    let owner = PluginId::new(PLUGIN).expect("valid id");
    let stolen = fixture
        .live
        .borrow_mut()
        .ui_panel_release(&owner, generation, fresh, "released")
        .map_err(|error| error.code);
    assert_eq!(
        stolen,
        Err("E_UI_NOT_OWNER"),
        "a dead generation must not release the live layer"
    );

    // And the successor does not adopt the predecessor's handle.
    let stale = fixture.run_on_args(PLUGIN, "release_handle", &[LuaValue::Integer(handle)]);
    assert_eq!(
        stale, "ERR:E_UI_NOT_OWNER",
        "a handle from the displaced generation names nothing"
    );
    let live = fixture.live.borrow();
    assert_eq!(
        live.ui_panel_layer_count(),
        1,
        "neither stale call touched the live row"
    );
    assert_eq!(
        live.panel_content_kind(PANE),
        Some(ViewContent::Rich(
            live.ui_panel_layer(fresh).expect("live layer").token.get()
        )),
        "the successor's layer is still presented after both refusals"
    );
    assert_eq!(
        live.content_restore_resizes(),
        1,
        "neither refused call restored the terminal"
    );
}
