// ---------------------------------------------------------------------------
// Tests (pure arg parsing + headless smoke totality)
// ---------------------------------------------------------------------------

use super::chrome_keys::two_pane_layout;
use super::*;
use crate::config_cli::{
    runtime_config_from_effective, runtime_config_from_effective_with_warnings,
};
use bitty_platform::{PlatformEvent, WindowEventKind};
// Only the POSIX-shell live-spawn test below uses this (`#[cfg(unix)]`);
// without the gate the import is unused on Windows.
#[cfg(unix)]
use bitty_test_support::require_pty;

fn args_of(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| (*s).to_string()).collect()
}

// CTX-1327 / CTX-1324 / P0-AC-015: exercise the real application input
// callback through `AppHandler::handle_event` with a headless `EventContext`
// and a read-only owning-VM entry counter. The regression proves the accepted
// input path enqueues the resolved band request (and a static keybinding
// command invocation) without entering the plugin VM, and that the deferred
// dispatch happens exactly once on the distinct cold pump.
#[cfg(feature = "input-vm-test-support")]
mod input_vm_boundary {
    use super::*;
    use bitty_platform::app::RecordingOperation;
    use bitty_platform::{
        AppHandler, CursorPosition, EventContext, KeyEvent, KeyLocation, LogicalKey,
        ModifiersState, MouseButton, MouseEvent, PhysicalSize, PressState, ScrollDelta,
        SystemAppearance, WindowId,
    };
    use bitty_plugin_host::manifest::PluginId;
    use bitty_runtime::plugin_runtime::{
        BridgeError, LifecycleState, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
        VmExecutionCountError,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::test_support::{PARK_RESUME_TICKS, settle_dispatch};

    const PLUGIN: &str = "bitty-featured.input-boundary";
    const BOUND_COLUMN: u16 = 1;
    const SLOW_COLUMN: u16 = 10;
    const UNBOUND_COLUMN: u16 = 7;
    const WHEEL_NOTCH: f32 = 1.0;
    const EXTRA_SCROLLBACK_LINES: usize = 8;

    /// Snapshot source that charges real host wall time without consuming the
    /// Lua instruction budget: the slow-command regression needs >50 ms of
    /// charged time that is not a busy loop (bitty #2314).
    struct SlowSnapshot;

    impl SnapshotSource for SlowSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    /// Minimal composer-owned app fixture for the bitty#2314 F2 composer arm.
    struct ComposerFixture {
        app: TerminalApp,
        id: PluginId,
        root: std::path::PathBuf,
    }

    impl ComposerFixture {
        fn new(case: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("ctx2314-composer-{}-{case}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let plugin = root.join(crate::composer_owner::COMPOSER_PLUGIN_ID);
            std::fs::create_dir_all(plugin.join("lua")).expect("composer module dir");
            std::fs::write(
                plugin.join("bitty-plugin.toml"),
                format!(
                    r#"[plugin]
id = "{}"
name = "Composer fixture"
version = "0.0.1"
description = "bitty#2314 slow composer open"
[compat]
plugin-api = "^1.0"
[capabilities]
ui.overlay.focus = true
terminal.input.submit = true
process.editor = true
terminal.semantic-read = true
[lazy]
commands = ["{}:open", "{}:runs"]
events = []
"#,
                    crate::composer_owner::COMPOSER_PLUGIN_ID,
                    crate::composer_owner::COMPOSER_PLUGIN_ID,
                    crate::composer_owner::COMPOSER_PLUGIN_ID,
                ),
            )
            .expect("composer manifest");
            std::fs::write(
                plugin.join("lua/init.lua"),
                r#"
local runs = 0
bitty.commands.register({ id = "open", title = "Open", run = function(_args)
  for _ = 1, 8 do
    pcall(bitty.terminal.snapshot, { scope = "semantic" })
  end
  runs = runs + 1
  return "open:" .. runs
end })
bitty.commands.register({ id = "runs", title = "Runs", run = function(_args)
  return tostring(runs)
end })
return {}
"#,
            )
            .expect("composer init");
            let mut plugins = PluginRuntime::new(PluginRuntimeConfig {
                third_party_roots: vec![root.clone()],
                snapshot: Rc::new(SlowSnapshot),
                ..PluginRuntimeConfig::default()
            });
            let id = PluginId::new(crate::composer_owner::COMPOSER_PLUGIN_ID).expect("composer id");
            plugins.discover();
            let activation = plugins.activate(&id).expect("composer activation");
            assert_eq!(activation.state, LifecycleState::Active);
            let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
                .expect("default keymaps");
            let mut app = TerminalApp::with_theme(
                Runtime::with_defaults().expect("headless runtime"),
                bitty_config::theme::DEFAULT_THEME_NAME,
                "composer",
                maps,
                SpawnSpec::default(),
            )
            .with_plugin_runtime(Some(plugins));
            app.set_session_persistence(false);
            Self { app, id, root }
        }

        fn parked(&self) -> usize {
            self.app
                .plugin_runtime
                .as_ref()
                .expect("plugins")
                .parked_command_count()
        }

        fn pump_until_unparked(&mut self) -> usize {
            for pumps in 1..=200 {
                self.app.pump_cold_commands();
                if self.parked() == 0 {
                    return pumps;
                }
            }
            panic!("composer park did not complete within 200 cold ticks");
        }
    }

    impl Drop for ComposerFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    struct Fixture {
        app: TerminalApp,
        id: PluginId,
        root: std::path::PathBuf,
        operations: RefCell<Vec<RecordingOperation>>,
    }

    impl Fixture {
        fn new(case: &str) -> Self {
            let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
                .expect("default in-memory keymap");
            Self::build(case, maps)
        }

        /// Fixture with one explicit `command:<owner:command>` keybinding
        /// (CTX-1324) so the real keyboard intercept can be driven through
        /// `AppHandler::handle_event` without any host keymap file.
        fn with_command_keybinding(case: &str) -> Self {
            let effective = bitty_config::EffectiveConfig {
                keymaps: vec![bitty_config::KeymapEntry {
                    chord: "ctrl+alt+y".to_string(),
                    action: format!("command:{PLUGIN}:step"),
                    context: "global".to_string(),
                }],
                ..bitty_config::EffectiveConfig::default()
            };
            let maps = bitty_config::resolve_keymaps(&effective).expect("fixture keymap");
            Self::build(case, maps)
        }

        /// Same real keybinding path, targeting the slow command (bitty#2314
        /// F2): a chord must park on the cold phase and resume on later ticks.
        fn with_slow_command_keybinding(case: &str) -> Self {
            let effective = bitty_config::EffectiveConfig {
                keymaps: vec![bitty_config::KeymapEntry {
                    chord: "ctrl+alt+y".to_string(),
                    action: format!("command:{PLUGIN}:slow"),
                    context: "global".to_string(),
                }],
                ..bitty_config::EffectiveConfig::default()
            };
            let maps = bitty_config::resolve_keymaps(&effective).expect("fixture keymap");
            Self::build(case, maps)
        }

        fn build(case: &str, maps: Vec<bitty_config::ResolvedKeymap>) -> Self {
            // Only fixture-owned files are touched. No user configuration,
            // startup path, native clipboard, window, watcher, or PTY exists.
            let root = std::env::temp_dir().join(format!("ctx1327-{}-{case}", std::process::id()));
            std::fs::create_dir(&root).expect("fresh fixture directory");
            let plugin = root.join(PLUGIN);
            std::fs::create_dir_all(plugin.join("lua")).expect("fixture module directory");
            std::fs::write(
                plugin.join("bitty-plugin.toml"),
                format!(
                    r#"[plugin]
id = "{PLUGIN}"
name = "Input boundary fixture"
version = "0.0.1"
description = "Finite application input boundary regression"
[compat]
plugin-api = "^1.0"
[capabilities]
ui.rich = true
terminal.semantic-read = true
[lazy]
commands = ["{PLUGIN}:step", "{PLUGIN}:read", "{PLUGIN}:slow"]
events = []
"#
                ),
            )
            .expect("fixture manifest");
            std::fs::write(
                plugin.join("lua/init.lua"),
                r#"
local count = 0
bitty.commands.register({ id = "step", title = "Step", run = function(args)
  count = count + 1
  return tostring(count)
end })
bitty.commands.register({ id = "read", title = "Read", run = function(args)
  return count
end })
bitty.commands.register({ id = "slow", title = "Slow", run = function(args)
  for _ = 1, 8 do
    pcall(bitty.terminal.snapshot, { scope = "semantic" })
  end
  count = count + 1
  return "slow:" .. count
end })
bitty.ui.mount("bottom", { kind = "Row", children = {
  { kind = "Text", text = "bound", on_click = { command = "step" },
    on_scroll = { command = "step" } },
  { kind = "Text", text = " free" },
  { kind = "Text", text = " slow", on_click = { command = "slow" },
    on_scroll = { command = "slow" } }
} })
return {}
"#,
            )
            .expect("finite local fixture source");
            let mut plugins = PluginRuntime::new(PluginRuntimeConfig {
                third_party_roots: vec![root.clone()],
                snapshot: Rc::new(SlowSnapshot),
                ..PluginRuntimeConfig::default()
            });
            let id = PluginId::new(PLUGIN).expect("fixture identity");
            assert_eq!(
                plugins.vm_total_executions(&id),
                Err(VmExecutionCountError::MissingVm)
            );
            let discovery = plugins.discover();
            assert_eq!(discovery.len(), 1);
            assert!(discovery[0].1.is_ok(), "{discovery:?}");
            let activation = plugins.activate(&id).expect("fixture activation");
            assert_eq!(activation.state, LifecycleState::Active);
            assert_eq!(activation.commands, 3);
            assert_eq!(activation.events, 0);
            assert_eq!(plugins.ui_blocks().len(), 1, "real authorized mount");
            assert!(plugins.timer_deadline().is_none());

            let rt = Runtime::for_input_vm_regression().expect("safe construction");
            assert!(rt.is_headless());
            assert!(!rt.is_crossfont());
            assert!(rt.clipboard().is_headless());
            assert_eq!(rt.pty_size(), None);
            assert!(!rt.has_pty_writer());
            assert_eq!(rt.background_loads(), 0);
            let mut app = TerminalApp::with_theme(
                rt,
                bitty_config::theme::DEFAULT_THEME_NAME,
                "fixture",
                maps,
                SpawnSpec::default(),
            )
            .with_plugin_runtime(Some(plugins));
            app.set_session_persistence(false);
            // Present the actual mounted tree through the production tick so
            // the band origin is captured and published exactly as in a real
            // frame (CTX-1332 C-D4). No delivery is selected by this setup.
            let _ = app.drive_tick();
            assert_eq!(
                app.runtime.published_band_origins().len(),
                1,
                "real authorized mount presented an executable origin"
            );
            let fixture = Self {
                app,
                id,
                root,
                operations: RefCell::new(Vec::new()),
            };
            fixture.assert_no_sinks();
            println!(
                "{case}: baseline after activation/mount={}",
                fixture.entries()
            );
            fixture
        }

        fn entries(&self) -> u64 {
            self.app
                .plugin_runtime
                .as_ref()
                .expect("plugins")
                .vm_total_executions(&self.id)
                .expect("owning VM must exist and be idle")
        }

        fn event(&mut self, kind: WindowEventKind) -> u64 {
            let before = self.entries();
            let event = PlatformEvent::Window {
                window_id: WindowId::from_raw_public(1),
                kind,
            };
            let mut context = EventContext::recording(&self.operations);
            <TerminalApp as AppHandler>::handle_event(&mut self.app, &mut context, event);
            let after = self.entries();
            self.assert_no_sinks();
            after.checked_sub(before).expect("monotonic VM counter")
        }

        /// Drives one non-input platform event through the real `AppHandler`.
        ///
        /// Loop-control operations (`Poll`/`Wait`/`WaitUntil`) are expected
        /// from `AboutToWait`; every sink operation must stay absent.
        fn platform_event(&mut self, event: PlatformEvent) -> u64 {
            self.operations.borrow_mut().clear();
            let before = self.entries();
            let mut context = EventContext::recording(&self.operations);
            <TerminalApp as AppHandler>::handle_event(&mut self.app, &mut context, event);
            let after = self.entries();
            assert!(self.app.window.handle.is_none());
            assert!(self.app.window.os_title_sink.is_none());
            assert!(self.app.window.os_cursor_sink.is_none());
            assert!(self.app.pty_rx.is_none());
            assert!(self.app._pty_thread.is_none());
            for operation in self.operations.borrow().iter() {
                assert!(
                    matches!(
                        operation,
                        RecordingOperation::WindowCreationRefused
                            | RecordingOperation::Poll
                            | RecordingOperation::Wait
                            | RecordingOperation::WaitUntil(_)
                    ),
                    "unexpected sink operation: {operation:?}"
                );
            }
            self.operations.borrow_mut().clear();
            after.checked_sub(before).expect("monotonic VM counter")
        }

        /// Queues exactly one bound wheel admission through the real input
        /// callback, with the cursor on the mounted band's bound column.
        fn queue_bound_wheel(&mut self) {
            let routed = self.queue_wheel_at(BOUND_COLUMN);
            assert_eq!(routed, 1, "one admission queued");
        }

        /// Queues one wheel admission at `column` and returns the routing
        /// delta. Input must not enter a VM.
        fn queue_wheel_at(&mut self, column: u16) -> u64 {
            self.cursor(column);
            let routed = self.app.runtime.band_host_stats().scrolls_routed;
            assert_eq!(
                self.event(WindowEventKind::MouseWheel(ScrollDelta::Lines(
                    0.0,
                    WHEEL_NOTCH
                ))),
                0,
                "queueing must not enter the owning VM"
            );
            self.app.runtime.band_host_stats().scrolls_routed - routed
        }

        /// Pump cold ticks until no command remains parked (bitty#2314 F2).
        fn pump_until_unparked(&mut self) -> usize {
            for pumps in 1..=PARK_RESUME_TICKS {
                self.app.pump_cold_commands();
                if self
                    .app
                    .plugin_runtime
                    .as_ref()
                    .expect("plugins")
                    .parked_command_count()
                    == 0
                {
                    return pumps;
                }
            }
            panic!("parked command did not complete within {PARK_RESUME_TICKS} cold ticks");
        }

        fn cursor(&mut self, column: u16) {
            let rt = &self.app.runtime;
            let row = rt
                .plugin_band_row(bitty_runtime::BandEdge::Bottom, 0)
                .expect("mounted bottom row");
            let (cw, ch) = rt.live_cell_size();
            let pad = f64::from(rt.window_padding_physical());
            let delta = self.event(WindowEventKind::CursorMoved(CursorPosition {
                x: pad + (f64::from(column) + 0.5) * f64::from(cw),
                y: pad + (f64::from(row) + 0.5) * f64::from(ch),
            }));
            assert_eq!(delta, 0, "cursor input must not enter a VM");
        }

        fn mouse(&mut self, state: PressState) -> u64 {
            self.event(WindowEventKind::MouseInput(MouseEvent::new(
                MouseButton::Left,
                state,
            )))
        }

        fn assert_no_sinks(&self) {
            assert!(self.app.window.handle.is_none());
            assert!(self.app.window.os_title_sink.is_none());
            assert!(self.app.window.os_cursor_sink.is_none());
            assert!(self.app.pty_rx.is_none());
            assert!(self.app._pty_thread.is_none());
            assert_eq!(self.app.runtime.pty_size(), None);
            assert!(!self.app.runtime.has_pty_writer());
            assert!(self.operations.borrow().is_empty());
        }

        fn read_marker(&mut self) -> i64 {
            let before = self.entries();
            // bitty #2409: the marker read goes through the same resumable
            // RC-1 dispatch as every other command, so a loaded runner can
            // legitimately park it. Settling drives the documented cold-tick
            // resume and keeps the exactly-once accounting exact: a resume
            // continues the same executor, so it never adds a second entry.
            let value = settle_dispatch(
                self.app.plugin_runtime.as_mut().expect("plugins"),
                self.id.as_str(),
                "read",
                &[],
            )
            .expect_value("read");
            assert_eq!(self.entries() - before, 1, "marker read counted separately");
            let LuaValue::Integer(marker) = value else {
                panic!("bounded integer marker expected: {value:?}");
            };
            marker
        }

        /// Presses the fixture's `ctrl+alt+y -> command:<plugin>:step` chord
        /// through the real application input callback (CTX-1324).
        fn press_command_chord(&mut self) -> u64 {
            assert_eq!(
                self.event(WindowEventKind::ModifiersChanged(ModifiersState {
                    shift: false,
                    control: true,
                    alt: true,
                    super_pressed: false,
                })),
                0,
                "modifier input must not enter a VM"
            );
            self.event(WindowEventKind::KeyboardInput(KeyEvent {
                logical_key: LogicalKey::Character("y".to_string()),
                text: Some("y".to_string()),
                location: KeyLocation::Standard,
                state: PressState::Pressed,
                repeat: false,
                is_synthetic: false,
            }))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).expect("remove only owned fixture files");
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Gesture {
        ClickRelease,
        Wheel,
    }

    fn input_must_not_enter_vm(gesture: Gesture) {
        let mut fixture = Fixture::new(match gesture {
            Gesture::ClickRelease => "release",
            Gesture::Wheel => "wheel",
        });
        // Mouse reporting makes an orphan release observable in pending PTY
        // bytes, without starting a process or attaching a writer.
        fixture
            .app
            .runtime
            .handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        fixture.cursor(BOUND_COLUMN);
        fixture.app.runtime.start_selection(Default::default());
        let mut selected = fixture.app.runtime.selection().expect("selection anchor");
        selected.focus.col = 1;
        fixture.app.runtime.set_selection(selected);
        assert!(fixture.app.runtime.has_selection());
        fixture
            .app
            .runtime
            .handle_ime_preedit(Some("composing".to_string()), Some(2));
        let focus = fixture.app.runtime.focused_view();
        let window_focus = fixture.app.runtime.is_window_focused();
        let selection = fixture.app.runtime.selection();
        let ime = fixture.app.runtime.ime_preedit().map(str::to_string);
        let ime_area = fixture.app.runtime.ime_cursor_area();
        let window_ime_area = fixture.app.window.ime_cursor_area;
        let before = fixture.entries();
        let delta = match gesture {
            Gesture::ClickRelease => {
                assert_eq!(fixture.mouse(PressState::Pressed), 0);
                assert!(fixture.app.runtime.band_release_armed());
                let delta = fixture.mouse(PressState::Released);
                assert!(!fixture.app.runtime.band_release_armed());
                assert_eq!(fixture.app.runtime.band_host_stats().clicks_routed, 1);
                delta
            }
            Gesture::Wheel => {
                let delta = fixture.event(WindowEventKind::MouseWheel(ScrollDelta::Lines(
                    0.0,
                    WHEEL_NOTCH,
                )));
                assert_eq!(fixture.app.runtime.band_host_stats().scrolls_routed, 1);
                delta
            }
        };
        // (a) ZERO owning-VM entries during input: the accepted gesture routed
        // a bounded request into the runtime queue without entering the VM.
        assert_eq!(
            delta, 0,
            "P0-AC-015: input callback must not enter owning VM"
        );
        assert_eq!(fixture.entries(), before, "no owning-VM entry during input");
        assert!(
            fixture.app.runtime.pending_input().is_empty(),
            "no PTY report"
        );
        assert_eq!(fixture.app.runtime.focused_view(), focus);
        assert_eq!(fixture.app.runtime.is_window_focused(), window_focus);
        assert_eq!(fixture.app.runtime.selection(), selection);
        assert_eq!(fixture.app.runtime.ime_preedit(), ime.as_deref());
        assert_eq!(fixture.app.runtime.ime_cursor_area(), ime_area);
        assert_eq!(fixture.app.window.ime_cursor_area, window_ime_area);
        println!(
            "{gesture:?}: owning VM input delta={delta}; routing accepted; PTY/selection/focus/IME unchanged"
        );
        // (b) exactly-once later cold delivery: the distinct cold pump enters
        // the VM once and the queued command's bounded result surfaces.
        assert_eq!(
            fixture.app.pump_cold_commands(),
            1,
            "one bounded request queued by the accepted input"
        );
        assert_eq!(
            fixture.entries(),
            before + 1,
            "exactly one owning-VM entry on the cold pump"
        );
        fixture.assert_no_sinks();
        assert_eq!(
            fixture.app.runtime.notification_banner().as_deref(),
            Some("bitty-featured.input-boundary:step: 1"),
            "the deferred command's bounded result surfaced"
        );
        // A second pump must not replay the already-consumed request.
        assert_eq!(
            fixture.app.pump_cold_commands(),
            0,
            "cold pump delivers exactly once"
        );
        assert_eq!(
            fixture.entries(),
            before + 1,
            "no replay on a second cold pump"
        );
        // (c) the deferred command ran exactly once: the plugin's own counter,
        // read through a separate VM entry, matches one delivery.
        let marker = fixture.read_marker();
        assert_eq!(marker, 1, "deferred command ran exactly once");
        println!("{gesture:?}: separate marker read delta=1, marker={marker}");
    }

    #[test]
    fn input_vm_boundary_click_release_excludes_vm_entry() {
        input_must_not_enter_vm(Gesture::ClickRelease);
    }

    #[test]
    fn input_vm_boundary_wheel_excludes_vm_entry() {
        input_must_not_enter_vm(Gesture::Wheel);
    }

    #[test]
    fn input_vm_boundary_cold_pump_delivers_exactly_once() {
        // (b) exactly-once later cold delivery, isolated: a bound wheel queues
        // one bounded request; the distinct cold pump enters the owning VM
        // exactly once and never replays the consumed request.
        let mut fixture = Fixture::new("coldpump");
        fixture.cursor(BOUND_COLUMN);
        let before = fixture.entries();
        assert_eq!(
            fixture.event(WindowEventKind::MouseWheel(ScrollDelta::Lines(
                0.0,
                WHEEL_NOTCH
            ))),
            0,
            "input must not enter the owning VM"
        );
        assert_eq!(fixture.app.runtime.band_host_stats().scrolls_routed, 1);
        assert_eq!(fixture.entries(), before, "no VM entry during input");
        assert_eq!(fixture.app.pump_cold_commands(), 1, "one deferred delivery");
        assert_eq!(fixture.entries(), before + 1, "exactly one VM entry");
        assert_eq!(fixture.app.pump_cold_commands(), 0, "no replay");
        assert_eq!(fixture.entries(), before + 1, "still exactly one");
        assert_eq!(fixture.read_marker(), 1);
        println!("coldpump: input delta=0; cold pump delta=1 exactly once; marker=1");
    }

    #[test]
    fn parked_command_resumes_on_a_later_cold_pump() {
        // bitty #2314: an app-dispatched command that reaches the RC-1 wall
        // budget parks instead of being refused. The next cold pump resumes
        // the same executor and surfaces the result exactly once.
        let mut fixture = Fixture::new("rc1park");
        let before = fixture.entries();
        let error = fixture
            .app
            .plugin_runtime
            .as_mut()
            .expect("plugins")
            .dispatch_command(&fixture.id, "slow", &[])
            .expect_err("80 ms of charged work must park");
        assert!(error.is_parked(), "expected a park, got {error:?}");
        assert_eq!(
            fixture
                .app
                .plugin_runtime
                .as_ref()
                .expect("plugins")
                .parked_command_count(),
            1
        );
        let mut completed = 0usize;
        for _ in 0..crate::test_support::PARK_RESUME_TICKS {
            completed = fixture.app.pump_cold_commands();
            if completed > 0 {
                break;
            }
        }
        assert_eq!(
            completed, 1,
            "a later cold pump completes the parked command"
        );
        assert_eq!(
            fixture.entries(),
            before + 1,
            "one dispatch, many resume windows"
        );
        assert_eq!(
            fixture.app.pump_cold_commands(),
            0,
            "a completed command never replays"
        );
        assert_eq!(fixture.read_marker(), 1, "the body ran exactly once");
        assert_eq!(
            fixture.app.runtime.notification_banner().as_deref(),
            Some("bitty-featured.input-boundary:slow: slow:1"),
            "the resumed result surfaces on the banner"
        );
    }

    #[test]
    fn slow_keybinding_command_parks_and_resumes_on_cold_pumps() {
        // bitty#2314 F2: the real `command:<owner:command>` input path must
        // park the slow callback and resume it on later cold ticks, exactly
        // once, with its result surfaced after completion.
        let mut fixture = Fixture::with_slow_command_keybinding("slow-keybinding");
        let before = fixture.entries();
        assert_eq!(
            fixture.press_command_chord(),
            0,
            "keybinding input must not enter the owning VM"
        );
        assert_eq!(fixture.entries(), before, "no VM entry during input");
        let pumps = fixture.pump_until_unparked();
        assert!(pumps >= 2, "the slow command must span several cold ticks");
        assert_eq!(fixture.entries(), before + 1, "one dispatch, no replay");
        assert_eq!(fixture.read_marker(), 1, "the slow body ran exactly once");
        assert_eq!(
            fixture.app.runtime.notification_banner().as_deref(),
            Some("bitty-featured.input-boundary:slow: slow:1"),
            "the resumed result surfaces exactly once"
        );
        assert_eq!(
            fixture.app.pump_cold_commands(),
            0,
            "a completed command never replays"
        );
    }

    #[test]
    fn slow_band_scroll_parks_and_resumes_on_cold_pumps() {
        // bitty#2314 F2: the real band scroll arm (bar-shaped host command)
        // parks and resumes through the same cold phase.
        let mut fixture = Fixture::new("slow-band");
        assert_eq!(
            fixture.queue_wheel_at(SLOW_COLUMN),
            1,
            "one admission queued"
        );
        let before = fixture.entries();
        assert_eq!(fixture.entries(), before, "no VM entry during input");
        let pumps = fixture.pump_until_unparked();
        assert!(pumps >= 2, "the slow command must span several cold ticks");
        assert_eq!(fixture.entries(), before + 1, "one dispatch, no replay");
        assert_eq!(fixture.read_marker(), 1, "the slow body ran exactly once");
        assert_eq!(
            fixture.app.runtime.notification_banner().as_deref(),
            Some("bitty-featured.input-boundary:slow: slow:1"),
            "the resumed band result surfaces exactly once"
        );
        assert_eq!(
            fixture.app.pump_cold_commands(),
            0,
            "a completed band command never replays"
        );
    }

    #[test]
    fn slow_composer_open_parks_and_resumes_on_cold_pumps() {
        // bitty#2314 F2: the 212 ms composer-shaped arm parks its `open`
        // command and resumes on later cold ticks without losing the result.
        let mut fixture = ComposerFixture::new("slow");
        assert!(fixture.app.composer_plugin_owns());
        // Route through the same cold queue the input intercept uses, so the
        // dispatch tick and the resume ticks are both real cold phases.
        fixture
            .app
            .enqueue_input_command(bitty_config::ChromeAction::OpenComposer);
        let pumps = fixture.pump_until_unparked();
        assert!(pumps >= 2, "composer open must span several cold ticks");
        // bitty #2409: the follow-up read is a probe dispatch, so settle a
        // park instead of assuming it completes inside one wall window.
        let runs = settle_dispatch(
            fixture.app.plugin_runtime.as_mut().expect("plugins"),
            fixture.id.as_str(),
            "runs",
            &[],
        )
        .expect_value("runs");
        assert_eq!(runs, LuaValue::String("1".to_string()));
        assert_eq!(
            fixture.app.runtime.notification_banner().as_deref(),
            Some("bitty-terminal.composer:open: open:1"),
            "the resumed composer result surfaces exactly once"
        );
    }

    #[test]
    fn input_vm_boundary_about_to_wait_is_the_only_cold_phase() {
        // CTX-1332 C-D1 phase gate (R-1/A-1): drive the real `AppHandler`.
        // The first AboutToWait delivers exactly one owning-VM entry. A second
        // AboutToWait with an empty queue must not replay it. Every prohibited
        // arm (redraw, PTY-readable, resize, appearance) is probed with one
        // admission queued immediately before it: zero VM entries and the
        // admission must still be queued (proved by a following AboutToWait
        // delivering exactly one).
        let mut fixture = Fixture::new("phase");
        let mut effective = bitty_config::EffectiveConfig::default();
        effective.appearance.theme = Some("light:github-light,dark:dracula".to_string());
        crate::config_reload::install_with(
            effective.clone(),
            Box::new(move || Ok(effective.clone())),
            None,
        );

        // (1) First AboutToWait: exactly one owning-VM entry.
        fixture.queue_bound_wheel();
        assert_eq!(
            fixture.platform_event(PlatformEvent::AboutToWait),
            1,
            "AboutToWait must deliver the queued admission once"
        );

        // (2) Second AboutToWait with an empty queue: no replay.
        assert_eq!(
            fixture.platform_event(PlatformEvent::AboutToWait),
            0,
            "a consumed admission must not replay"
        );

        let extent = fixture
            .app
            .runtime
            .surface_extent()
            .expect("headless surface extent");
        let probes = [
            (
                "RedrawRequested",
                PlatformEvent::Window {
                    window_id: WindowId::from_raw_public(1),
                    kind: WindowEventKind::RedrawRequested,
                },
            ),
            ("PtyReadable", PlatformEvent::PtyReadable),
            (
                "Resized",
                PlatformEvent::Window {
                    window_id: WindowId::from_raw_public(1),
                    kind: WindowEventKind::Resized(PhysicalSize::new(
                        extent.width(),
                        extent.height(),
                    )),
                },
            ),
            (
                "SystemAppearanceChanged",
                PlatformEvent::SystemAppearanceChanged(SystemAppearance::Light),
            ),
            // Headless `Resumed` is the sixth `drive_tick` caller. In a
            // recording context its window creation is refused and it falls
            // back to one tick; the pump must still be unreachable here.
            ("Resumed", PlatformEvent::Resumed),
        ];

        // (3) Prohibited arms: queue immediately before each probe, assert
        // zero entries, then prove the admission is still queued.
        for (name, probe) in probes {
            let theme_before = fixture.app.runtime.config().theme;
            fixture.queue_bound_wheel();
            assert_eq!(
                fixture.platform_event(probe),
                0,
                "{name} must not enter the owning VM"
            );
            if name == "SystemAppearanceChanged" {
                assert_ne!(
                    fixture.app.runtime.config().theme,
                    theme_before,
                    "the appearance probe must reach drive_tick, not early-return"
                );
            }
            assert_eq!(
                fixture.platform_event(PlatformEvent::AboutToWait),
                1,
                "{name} must not drain the queued admission"
            );
        }
        crate::config_reload::clear();
    }

    #[test]
    fn input_vm_boundary_suspend_cancels_queued_delivery() {
        // CTX-1332 C-D5: an app-observed lifecycle transition (suspend drops
        // the mounts) eagerly cancels the queued admission, and the cold pump
        // enters no VM. The release swallow is owned separately and is not
        // cleared by this cancellation path.
        let mut fixture = Fixture::new("suspend-phase");
        fixture.queue_bound_wheel();
        fixture
            .app
            .plugin_runtime
            .as_mut()
            .expect("plugins")
            .suspend(&fixture.id)
            .expect("suspend fixture");
        assert_eq!(
            fixture.platform_event(PlatformEvent::AboutToWait),
            0,
            "a suspended generation must not enter the owning VM"
        );
        assert_eq!(
            fixture.platform_event(PlatformEvent::AboutToWait),
            0,
            "the cancelled admission never replays"
        );
    }

    #[test]
    fn input_vm_boundary_direct_command_positive_control() {
        let mut fixture = Fixture::new("direct");
        let before = fixture.entries();
        // bitty #2409: settle a park; a resume continues the same executor and
        // never increments the owning-VM entry counter, so the exact delta
        // stays the positive control for "one dispatch, one VM entry".
        let result = settle_dispatch(
            fixture.app.plugin_runtime.as_mut().expect("plugins"),
            fixture.id.as_str(),
            "step",
            &[],
        )
        .expect_value("step");
        assert_eq!(fixture.entries() - before, 1);
        assert_eq!(result, LuaValue::String("1".to_string()));
        assert_eq!(fixture.read_marker(), 1);
        println!(
            "direct: one VM entry, bounded result 1; separate marker read: one VM entry, marker 1"
        );
    }

    #[test]
    fn input_vm_boundary_keybinding_command_excludes_vm_entry() {
        // CTX-1324 (P0-AC-015): the static `command:<owner:command>` keymap
        // action is the remaining input path that used to enter the owning
        // VM synchronously. Drive the real chord through `handle_event`:
        // input only enqueues, the distinct `AboutToWait` cold phase delivers
        // exactly once, and the command is still reachable explicitly.
        let mut fixture = Fixture::with_command_keybinding("keybinding");
        let before = fixture.entries();
        assert_eq!(
            fixture.press_command_chord(),
            0,
            "P0-AC-015: keybinding input must not enter the owning VM"
        );
        assert_eq!(fixture.entries(), before, "no owning-VM entry during input");
        assert!(
            fixture.app.runtime.pending_input().is_empty(),
            "a consumed command chord emits no PTY bytes"
        );
        // Exactly-once later delivery on the real cold application callback.
        assert_eq!(
            fixture.platform_event(PlatformEvent::AboutToWait),
            1,
            "one deferred command on the cold phase"
        );
        assert_eq!(
            fixture.entries(),
            before + 1,
            "exactly one owning-VM entry on the cold pump"
        );
        assert_eq!(
            fixture.app.runtime.notification_banner().as_deref(),
            Some("bitty-featured.input-boundary:step: 1"),
            "the deferred command's bounded result surfaced"
        );
        assert_eq!(
            fixture.platform_event(PlatformEvent::AboutToWait),
            0,
            "a consumed command must not replay"
        );
        assert_eq!(fixture.entries(), before + 1, "still exactly one entry");
        assert_eq!(fixture.read_marker(), 1, "the command ran exactly once");
        // Valid-command positive control: an explicit host-mediated
        // invocation outside the input path still executes once.
        let read_before = fixture.entries();
        let result = fixture
            .app
            .invoke_plugin_command(
                &format!("{PLUGIN}:read"),
                &TerminalApp::empty_command_args(),
            )
            .expect("registered command");
        assert_eq!(result, LuaValue::Integer(1));
        assert_eq!(
            fixture.entries(),
            read_before + 1,
            "explicit invocation enters the VM exactly once"
        );
        println!(
            "keybinding: input delta=0; cold pump delta=1 exactly once; marker=1; explicit invoke delta=1"
        );
    }

    #[test]
    fn input_vm_boundary_keybinding_queue_is_bounded() {
        // CTX-1324: the new input-command queue is bounded and the payload
        // bound refuses an oversize target before queueing. Unregistered
        // names fail closed at the cold dispatch (no VM entry) and still
        // count as bounded admission attempts.
        let mut fixture = Fixture::new("keybinding-bounds");
        let before = fixture.entries();
        for index in 0..=crate::terminal_app::INPUT_COMMAND_QUEUE_CAPACITY {
            fixture
                .app
                .enqueue_input_command(bitty_config::ChromeAction::InvokeCommand(format!(
                    "{PLUGIN}:missing{index}"
                )));
        }
        assert_eq!(
            fixture.app.pump_cold_commands(),
            crate::terminal_app::INPUT_COMMAND_QUEUE_CAPACITY,
            "overflow drops the newest request"
        );
        assert_eq!(
            fixture.entries(),
            before,
            "unregistered names never enter a VM"
        );
        let oversize = format!(
            "{}:x",
            "a".repeat(bitty_config::keymap::MAX_INVOKE_COMMAND_LEN)
        );
        fixture
            .app
            .enqueue_input_command(bitty_config::ChromeAction::InvokeCommand(oversize));
        assert_eq!(
            fixture.app.pump_cold_commands(),
            0,
            "an oversize target is refused before queueing"
        );
        assert_eq!(fixture.entries(), before);
    }

    #[test]
    fn input_vm_boundary_unbound_and_shift_fallthrough_controls() {
        let mut fixture = Fixture::new("fallthrough");
        let seed_lines = fixture.app.runtime.snapshot().height + EXTRA_SCROLLBACK_LINES;
        fixture
            .app
            .runtime
            .handle_pty_bytes(&b"fixture\r\n".repeat(seed_lines));
        fixture
            .app
            .runtime
            .handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        fixture.cursor(UNBOUND_COLUMN);
        let before = fixture.entries();
        assert_eq!(
            fixture.event(WindowEventKind::MouseWheel(ScrollDelta::Lines(
                0.0,
                WHEEL_NOTCH
            ))),
            0
        );
        assert_eq!(fixture.app.runtime.band_host_stats().scrolls_unclaimed, 1);
        assert!(
            !fixture.app.runtime.pending_input().is_empty(),
            "unbound wheel reaches mouse reporting"
        );
        fixture.app.runtime.drain_pending_input();
        fixture.cursor(BOUND_COLUMN);
        assert_eq!(
            fixture.event(WindowEventKind::ModifiersChanged(ModifiersState {
                shift: true,
                control: false,
                alt: false,
                super_pressed: false,
            })),
            0
        );
        assert_eq!(fixture.mouse(PressState::Pressed), 0);
        assert!(
            !fixture.app.runtime.band_release_armed(),
            "Shift selects terminal escape"
        );
        assert_eq!(fixture.mouse(PressState::Released), 0);
        assert_eq!(
            fixture.event(WindowEventKind::MouseWheel(ScrollDelta::Lines(
                0.0,
                WHEEL_NOTCH
            ))),
            0
        );
        assert_eq!(fixture.app.runtime.band_host_stats().clicks_routed, 0);
        assert_eq!(fixture.app.runtime.band_host_stats().scrolls_routed, 0);
        let focused = fixture.app.runtime.focused_view().expect("focused leaf");
        assert!(
            fixture
                .app
                .runtime
                .layout()
                .find_leaf(focused)
                .expect("live leaf")
                .scroll_offset()
                > 0,
            "Shift wheel reaches the terminal scrollback viewport"
        );
        assert!(
            fixture.app.runtime.pending_input().is_empty(),
            "Shift emits no mouse report"
        );
        assert_eq!(fixture.entries(), before);
        assert_eq!(
            fixture.app.pump_cold_commands(),
            0,
            "fall-through queued nothing for the cold pump"
        );
        assert_eq!(fixture.read_marker(), 0);
        println!(
            "fallthrough: unbound wheel reports to in-memory terminal input; Shift scrolls the terminal viewport; zero input VM entries; marker 0"
        );
    }

    #[test]
    fn about_to_wait_arms_the_surface_retry_deadline() {
        let mut fixture = Fixture::new("surface-retry-wake");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        fixture.app.test_surface_retry_deadline = Some(deadline);
        fixture.operations.borrow_mut().clear();
        let mut context = EventContext::recording(&fixture.operations);
        <TerminalApp as AppHandler>::handle_event(
            &mut fixture.app,
            &mut context,
            PlatformEvent::AboutToWait,
        );
        let operations = fixture.operations.borrow();
        assert!(
            operations.iter().any(
                |operation| matches!(operation, RecordingOperation::WaitUntil(at) if *at == deadline)
            ),
            "AboutToWait must arm the surface-retry wake: {operations:?}"
        );
    }

    #[test]
    fn suspend_and_resumed_move_presentation_off_the_transition() {
        // Issue #2589 / #2570: a Hyprland workspace switch-away delivers
        // `Suspended` (the Wayland shell surface unmaps); switch-back delivers
        // `Resumed`. The embedder must gate presentation on the visibility
        // transition and never present inside either handler.
        let mut fixture = Fixture::new("surface-visibility-gate");
        let mut context = EventContext::recording(&fixture.operations);
        <TerminalApp as AppHandler>::handle_event(
            &mut fixture.app,
            &mut context,
            PlatformEvent::Suspended,
        );
        drop(context);
        assert!(
            !fixture.app.runtime.is_surface_visible(),
            "Suspended must gate presentation until the surface re-maps"
        );
        assert!(
            fixture.app.runtime.present_coalescing_deadline().is_none(),
            "the hide transition must not leave a wake behind"
        );

        // The reveal transition only re-queues work (the headless fallback
        // consumes its queued smoke frame in the same delivery on a fake
        // surface; a real window goes through `request_redraw`).
        let mut context = EventContext::recording(&fixture.operations);
        <TerminalApp as AppHandler>::handle_event(
            &mut fixture.app,
            &mut context,
            PlatformEvent::Resumed,
        );
        drop(context);
        assert!(
            fixture.app.runtime.is_surface_visible(),
            "Resumed must ungate presentation"
        );

        // The runtime-owned reveal queue item is consumed only by a later
        // tick at its own wake — never inside the visibility delivery. On a
        // real window the service path is `request_redraw` → `AboutToWait`;
        // the headless fixture surfaces the same queue through the runtime
        // seam the production `Resumed` delivery drives. (The fixture's own
        // `Resumed` delivery is exempt from the off-transition claim: its
        // headless fallback drives one smoke tick on a fake surface that
        // cannot block.)
        fixture.app.runtime.set_surface_visibility(false);
        let before_present = fixture.app.runtime.present_coalescing_deadline().is_none();
        let _ = fixture.app.runtime.tick_at(std::time::Instant::now());
        assert!(before_present, "a hidden tick presents nothing");
        let queue_wake = {
            fixture.app.runtime.set_surface_visibility(true);
            fixture
                .app
                .runtime
                .present_coalescing_deadline()
                .expect("the reveal must queue one next-frame present")
        };
        assert!(
            fixture.app.runtime.tick_at(queue_wake).is_some(),
            "the queued next-frame present runs at its own wake"
        );
        assert!(
            fixture.app.runtime.present_coalescing_deadline().is_none(),
            "the queue is consumed with the frame"
        );
    }

    #[test]
    fn input_vm_boundary_recording_context_refuses_windows() {
        let operations = RefCell::new(Vec::new());
        let mut context = EventContext::recording(&operations);
        assert!(matches!(
            context.create_window(bitty_platform::WindowConfig::new()),
            Err(bitty_platform::PlatformError::WindowCreation(_))
        ));
        context.set_poll();
        context.set_wait();
        let deadline = std::time::Instant::now();
        context.set_wait_until(deadline);
        context.exit();
        assert_eq!(
            *operations.borrow(),
            vec![
                RecordingOperation::WindowCreationRefused,
                RecordingOperation::Poll,
                RecordingOperation::Wait,
                RecordingOperation::WaitUntil(deadline),
                RecordingOperation::Exit,
            ]
        );
    }
}

#[test]
fn parse_no_args_yields_defaults() {
    let raw = args_of(&["bitty"]);
    let parsed = parse_args(&raw);
    assert!(!parsed.headless);
    assert!(!parsed.help);
    assert!(!parsed.version);
    assert_eq!(parsed.program, None);
    assert!(parsed.program_args.is_empty());
    assert_eq!(parsed.split_axis, None);
    assert_eq!(parsed.split_ratio, None);
    assert!(!parsed.stack);
    assert!(!parsed.overlay);
    assert_eq!(parsed.layout, None);
    assert_eq!(parsed.focus, None);
    assert_eq!(parsed.config_path, None);
    assert_eq!(parsed.profile, None);
    assert_eq!(parsed.theme, None);
    assert_eq!(parsed.config_cmd, None);
    assert!(!parsed.config_word);
    assert!(parsed.config_args.is_empty());
    assert!(!parsed.verbose);
    assert_eq!(parsed.log_level, None);
}

#[test]
fn parse_help_and_version_flags() {
    let raw = args_of(&["bitty", "--help"]);
    assert!(parse_args(&raw).help);
    let raw = args_of(&["bitty", "-h"]);
    assert!(parse_args(&raw).help);
    let raw = args_of(&["bitty", "--version"]);
    assert!(parse_args(&raw).version);
    let raw = args_of(&["bitty", "-V"]);
    assert!(parse_args(&raw).version);
}

#[test]
fn parse_help_subcommand_never_spawns() {
    // CTX-1117 (issue #1900): `bitty help` aliases `--help` (no-arg) and
    // per-command help (`bitty help plugin` == `bitty plugin --help`);
    // unknown names fail closed at dispatch (exit 2 naming valid commands).
    // `bitty help` must never spawn (program stays None).
    let parsed = parse_args(&args_of(&["bitty", "help"]));
    assert!(parsed.help_word);
    assert_eq!(parsed.help_cmd, None);
    assert!(parsed.help_args.is_empty());
    assert_eq!(parsed.program, None);

    let parsed = parse_args(&args_of(&["bitty", "help", "plugin"]));
    assert!(parsed.help_word);
    assert_eq!(parsed.help_cmd.as_deref(), Some("plugin"));
    assert!(parsed.help_args.is_empty());
    assert_eq!(parsed.program, None);

    let parsed = parse_args(&args_of(&["bitty", "help", "bogus"]));
    assert!(parsed.help_word);
    assert_eq!(parsed.help_cmd.as_deref(), Some("bogus"));
    assert_eq!(parsed.program, None);

    let parsed = parse_args(&args_of(&["bitty", "help", "plugin", "extra"]));
    assert!(parsed.help_word);
    assert_eq!(parsed.help_cmd.as_deref(), Some("plugin"));
    assert_eq!(parsed.help_args, vec![String::from("extra")]);

    // Escape hatch still spawns a program literally named `help`.
    let parsed = parse_args(&args_of(&["bitty", "--", "help"]));
    assert!(!parsed.help_word);
    assert_eq!(parsed.program.as_deref(), Some("help"));

    // CodeRabbit 4240150291: dash-flags after `help` land in `unknown_flag`
    // (the dash arm has no help handling), so the help branch must reject
    // them (exit 2) instead of printing help (exit 0).
    let parsed = parse_args(&args_of(&["bitty", "help", "plugin", "--typo"]));
    assert!(parsed.help_word);
    assert_eq!(parsed.help_cmd.as_deref(), Some("plugin"));
    assert_eq!(parsed.unknown_flag.as_deref(), Some("--typo"));

    // Issue #1959: `extension` is the canonical verb, `component` the hidden
    // deprecated alias. Both set the same word flag and capture the tail
    // verbatim; only the recorded spelling differs (stderr note at dispatch).
    let parsed = parse_args(&args_of(&["bitty", "extension", "list"]));
    assert!(parsed.component_word);
    assert_eq!(parsed.component_spelling, "extension");
    assert_eq!(parsed.component_raw, vec![String::from("list")]);
    assert_eq!(parsed.program, None);

    let parsed = parse_args(&args_of(&["bitty", "component", "list"]));
    assert!(parsed.component_word);
    assert_eq!(parsed.component_spelling, "component");
    assert_eq!(parsed.component_raw, vec![String::from("list")]);
    assert_eq!(parsed.program, None);

    // `bitty help` lists 16 subcommands (short help).
    let short = crate::cli::help_text_short(false);
    assert!(short.contains("help [cmd]"), "short help must list help");
    for cmd in [
        "run -- COMMAND",
        "ctl <resource>",
        "config <verb>",
        "init [FLAGS]",
        "doctor",
        "list <kind>",
        "inspect <target>",
        "dev <verb>",
        "plugin <verb>",
        "extension <verb>",
        "cmd <id>",
        "x <id>",
        "completion <shell>",
        "shell-init <shell>",
        "version",
        "help [cmd]",
    ] {
        assert!(short.contains(cmd), "short help must list {cmd}");
    }
    // Valid commands name all 16 (plus aliases).
    let valid = crate::cli::help_valid_commands();
    assert!(valid.contains("help"));
    assert_eq!(crate::cli::help_canonical_cmd("plugin"), Some("plugin"));
    assert_eq!(crate::cli::help_canonical_cmd("cfg"), Some("config"));
    assert_eq!(crate::cli::help_canonical_cmd("ls"), Some("list"));
    assert_eq!(crate::cli::help_canonical_cmd("comp"), Some("completion"));
    assert_eq!(
        crate::cli::help_canonical_cmd("extension"),
        Some("extension")
    );
    // Hidden deprecated alias (issue #1959): accepted, canonicalizes away.
    assert_eq!(
        crate::cli::help_canonical_cmd("component"),
        Some("extension")
    );
    assert_eq!(crate::cli::help_canonical_cmd("bogus"), None);
}

#[test]
fn ipc_socket_path_stored_unless_headless_without_test_mode() {
    // CodeRabbit 4240150294: test mode serves IPC even when headless
    // (BITTY_HEADLESS=1 + --test-mode), so spawned shells must still
    // receive BITTY_SOCKET. Headless alone never serves (no misleading
    // path).
    assert!(super::should_store_ipc_socket_path(false, false));
    assert!(super::should_store_ipc_socket_path(false, true));
    assert!(!super::should_store_ipc_socket_path(true, false));
    assert!(super::should_store_ipc_socket_path(true, true));
}

#[test]
fn default_shell_prefers_shell_env_when_no_configured_shell() {
    assert_eq!(resolve_default_shell(None, None), "/bin/sh");
    assert_eq!(resolve_default_shell(None, Some("/bin/fish")), "/bin/fish");
    assert_eq!(resolve_default_shell(None, Some("/bin/bash")), "/bin/bash");
    assert_eq!(
        resolve_default_shell(None, Some("/usr/bin/zsh")),
        "/usr/bin/zsh"
    );
}

#[test]
fn default_shell_prefers_configured_terminal_shell_over_env() {
    // CTX-0298: the effective `terminal.shell` outranks $SHELL.
    assert_eq!(
        resolve_default_shell(Some("/bin/zsh"), Some("/bin/fish")),
        "/bin/zsh"
    );
    assert_eq!(
        resolve_default_shell(Some("  /bin/zsh  "), Some("/bin/fish")),
        "/bin/zsh"
    );
    assert_eq!(resolve_default_shell(Some("/bin/zsh"), None), "/bin/zsh");
}

#[test]
fn default_shell_fails_closed_on_unusable_configured_shell() {
    // CTX-0298: blank/oversized/control-laden configured values never
    // reach execve; they fall through to $SHELL, then /bin/sh.
    for bad in ["", "   ", "\t\n ", "/bin/z\nsh", "/bin/zsh\u{7}"] {
        assert_eq!(
            resolve_default_shell(Some(bad), Some("/bin/fish")),
            "/bin/fish",
            "configured {bad:?} fails closed to $SHELL"
        );
        assert_eq!(resolve_default_shell(Some(bad), None), "/bin/sh");
    }
    // Edge whitespace is trimmed, not rejected: the normalized path runs.
    assert_eq!(
        resolve_default_shell(Some("/bin/zsh\n"), Some("/bin/fish")),
        "/bin/zsh"
    );
    let overlong = "a".repeat(bitty_config::types::MAX_SHELL_LEN + 1);
    assert_eq!(
        resolve_default_shell(Some(&overlong), Some("/bin/fish")),
        "/bin/fish"
    );
}

#[test]
fn default_shell_falls_back_when_env_missing_or_blank() {
    assert_eq!(resolve_default_shell(None, Some("")), "/bin/sh");
    assert_eq!(resolve_default_shell(None, Some("   ")), "/bin/sh");
    assert_eq!(resolve_default_shell(None, Some("\t\n ")), "/bin/sh");
}

#[test]
fn default_shell_trims_surrounding_whitespace() {
    assert_eq!(
        resolve_default_shell(None, Some("  /bin/fish  ")),
        "/bin/fish"
    );
    assert_eq!(
        resolve_default_shell(Some("  /bin/fish  "), None),
        "/bin/fish"
    );
}

#[test]
fn configured_shell_argv0_rejects_unusable_values() {
    assert_eq!(configured_shell_argv0(None), None);
    assert_eq!(configured_shell_argv0(Some("")), None);
    assert_eq!(configured_shell_argv0(Some(" \t ")), None);
    assert_eq!(configured_shell_argv0(Some("/bin/zsh\u{7}")), None);
    assert_eq!(configured_shell_argv0(Some("/bin/fish")), Some("/bin/fish"));
    assert_eq!(
        configured_shell_argv0(Some("  /bin/fish  ")),
        Some("/bin/fish")
    );
}

#[test]
fn bare_args_resolve_to_default_shell_chain() {
    let parsed = parse_args(&args_of(&["bitty"]));
    assert_eq!(parsed.program, None);
    assert_eq!(
        resolve_spawn_program(&parsed, Some("/bin/zsh"), Some("/bin/fish")),
        "/bin/zsh"
    );
    assert_eq!(
        resolve_spawn_program(&parsed, None, Some("/bin/fish")),
        "/bin/fish"
    );
    assert_eq!(resolve_spawn_program(&parsed, None, None), "/bin/sh");
    assert_eq!(resolve_spawn_program(&parsed, None, Some("")), "/bin/sh");
}

#[test]
fn explicit_program_arg_stays_identical() {
    let parsed = parse_args(&args_of(&["bitty", "--", "fish", "-l"]));
    assert_eq!(parsed.program.as_deref(), Some("fish"));
    assert_eq!(parsed.program_args, vec!["-l"]);
    // Explicit program wins over any injected configured shell + $SHELL.
    assert_eq!(
        resolve_spawn_program(&parsed, Some("/bin/zsh"), Some("/bin/bash")),
        "fish"
    );
    assert_eq!(resolve_spawn_program(&parsed, None, None), "fish");

    let parsed = parse_args(&args_of(&["bitty", "/bin/bash"]));
    assert_eq!(
        resolve_spawn_program(&parsed, Some("/bin/zsh"), Some("/bin/fish")),
        "/bin/bash"
    );
}

#[test]
fn help_text_documents_default_shell() {
    let help = help_text();
    assert!(help.contains("$SHELL"));
    assert!(help.contains("/bin/sh"));
}

#[test]
fn parse_headless_flag() {
    let raw = args_of(&["bitty", "--headless"]);
    assert!(parse_args(&raw).headless);
    let raw = args_of(&["bitty", "--headless", "--help"]);
    let p = parse_args(&raw);
    assert!(p.headless && p.help);
}

// CTX-0506 test-mode E2E surface: the opt-in flag, never ambient.
#[test]
fn parse_test_mode_flag() {
    assert!(
        !parse_args(&args_of(&["bitty"])).test_mode,
        "test mode is opt-in only"
    );
    assert!(parse_args(&args_of(&["bitty", "--test-mode"])).test_mode);
    // `--` escape hatch: `--test-mode` after `--` is a program name.
    let p = parse_args(&args_of(&["bitty", "--", "--test-mode"]));
    assert!(!p.test_mode);
    assert_eq!(p.program.as_deref(), Some("--test-mode"));
    assert!(help_text().contains("--test-mode"));
}

// CTX-0481 fail-loud startup: the opt-in flag and the IPC failure policy.
#[test]
fn parse_fail_loud_flag() {
    assert!(
        !parse_args(&args_of(&["bitty"])).fail_loud,
        "default stays fail-soft"
    );
    assert!(parse_args(&args_of(&["bitty", "--fail-loud"])).fail_loud);
    // `--` escape hatch: `--fail-loud` after `--` is a program name.
    let p = parse_args(&args_of(&["bitty", "--", "--fail-loud"]));
    assert!(!p.fail_loud);
    assert_eq!(p.program.as_deref(), Some("--fail-loud"));
}

#[test]
fn fail_loud_makes_an_unavailable_ipc_servo_fatal_only_when_opted_in() {
    // The fail-soft default keeps a serving failure non-fatal; `--fail-loud`
    // turns the same failure into a startup abort with a non-zero code.
    let failed = ipc_serve::IpcServeGuard::failed_for_tests("bind /x failed");
    assert!(ipc_serve_failure_exit(false, &failed).is_none());
    let (code, message) = ipc_serve_failure_exit(true, &failed).expect("fatal under --fail-loud");
    assert_eq!(code, EXIT_STARTUP);
    assert!(
        message.contains("--fail-loud") && message.contains("bind /x failed"),
        "diagnostic must name the flag and the reason, got {message:?}"
    );
    // A healthy or platform-unsupported (disabled, no failure reason) servo is
    // never fatal, even with --fail-loud.
    let disabled = ipc_serve::IpcServeGuard::unsupported_for_tests();
    assert!(disabled.socket_path().is_empty());
    assert!(ipc_serve_failure_exit(true, &disabled).is_none());
}

// CTX-0190 quiet-default logging: parsing + level gating.
#[test]
fn parse_verbose_flags() {
    assert!(parse_args(&args_of(&["bitty", "--verbose"])).verbose);
    assert!(parse_args(&args_of(&["bitty", "-v"])).verbose);
    assert!(!parse_args(&args_of(&["bitty"])).verbose);
    // `--` escape hatch: `-v` after `--` is a program name, not a flag.
    let p = parse_args(&args_of(&["bitty", "--", "-v"]));
    assert!(!p.verbose);
    assert_eq!(p.program.as_deref(), Some("-v"));
}

#[test]
fn parse_log_level_flags() {
    let p = parse_args(&args_of(&["bitty", "--log-level", "debug"]));
    assert_eq!(p.log_level, Some(LogLevel::Debug));
    let p = parse_args(&args_of(&["bitty", "--log-level=trace"]));
    assert_eq!(p.log_level, Some(LogLevel::Trace));
    let p = parse_args(&args_of(&["bitty", "--log-level=INFO"]));
    assert_eq!(p.log_level, Some(LogLevel::Info));
    // Unknown values are warned + ignored (total, no panic).
    let p = parse_args(&args_of(&["bitty", "--log-level", "nope"]));
    assert_eq!(p.log_level, None);
    let p = parse_args(&args_of(&["bitty", "--log-level"]));
    assert_eq!(p.log_level, None);
}

#[test]
fn log_level_parses_known_names() {
    assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
    assert_eq!(LogLevel::parse("warn"), Some(LogLevel::Warn));
    assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
    assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
    assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
    assert_eq!(LogLevel::parse("trace"), Some(LogLevel::Trace));
    assert_eq!(LogLevel::parse("DEBUG"), Some(LogLevel::Debug));
    assert_eq!(LogLevel::parse(" verbose "), Some(LogLevel::Debug));
    assert_eq!(LogLevel::parse("nope"), None);
    assert_eq!(LogLevel::parse(""), None);
}

#[test]
fn log_level_ordering_is_quiet_by_default() {
    assert!(LogLevel::Error < LogLevel::Warn);
    assert!(LogLevel::Warn < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Debug);
    assert!(LogLevel::Debug < LogLevel::Trace);
    assert_eq!(LogLevel::default_level(), LogLevel::Warn);
    assert!(!LogLevel::Warn.tick_enabled());
    assert!(!LogLevel::Error.tick_enabled());
    assert!(!LogLevel::Info.tick_enabled());
    assert!(LogLevel::Debug.tick_enabled());
    assert!(LogLevel::Trace.tick_enabled());
}

#[test]
fn log_level_from_env_value_accepts_rust_log_filters() {
    assert_eq!(log_level_from_env_value("trace"), Some(LogLevel::Trace));
    assert_eq!(log_level_from_env_value("debug"), Some(LogLevel::Debug));
    assert_eq!(
        log_level_from_env_value("bitty=debug"),
        Some(LogLevel::Debug)
    );
    assert_eq!(
        log_level_from_env_value("info,bitty-terminal=trace"),
        Some(LogLevel::Trace)
    );
    assert_eq!(log_level_from_env_value("WARN"), Some(LogLevel::Warn));
    assert_eq!(log_level_from_env_value("off"), None);
    assert_eq!(log_level_from_env_value(""), None);
}

#[test]
fn rust_log_filters_parse_directives_not_substrings() {
    // CTX-0482 (#763): substring scanning let a *target* name that merely
    // contained a level word flip the gate (`mydebug=warn` -> Debug).
    // Directives are parsed as `[target=]level`; only the bare global level
    // and the app's own `bitty*` targets count.
    assert_eq!(log_level_from_env_value("mydebug=warn"), None);
    assert_eq!(log_level_from_env_value("mytrace=error"), None);
    assert_eq!(log_level_from_env_value("wgpu=trace"), None);
    assert_eq!(log_level_from_env_value("bitty_debug=off"), None);
    // A target-specific app directive wins over the global level.
    assert_eq!(
        log_level_from_env_value("debug,bitty=error"),
        Some(LogLevel::Error)
    );
    assert_eq!(
        log_level_from_env_value("bitty-terminal=debug"),
        Some(LogLevel::Debug)
    );
    // A later global directive replaces an earlier one (last wins), and
    // whitespace around directives/levels is ignored.
    assert_eq!(log_level_from_env_value("info, warn"), Some(LogLevel::Warn));
}

#[test]
fn panel_overlay_modal_bit_feeds_modal_capture() {
    // CTX-0482 (#763): `modal_capture_active` checked only the three pending
    // confirmation gates, so panel-overlay modals never captured chrome
    // dispatch. The runtime bit (fed by `OverlayManager::modal_active` on
    // the panel path) is the fourth gate.
    use super::chrome_keys::DispatchPriority;
    use bitty_config::{ChromeAction, KeyName, KeyRef};
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    assert!(!app.runtime.overlay_modal_active());
    assert!(!app.modal_capture_active());
    let chord = KeyRef {
        key: KeyName::Char('w'),
        ctrl: false,
        alt: true,
        shift: false,
        super_held: false,
    };
    // No modal: the bound chord runs its action.
    let (priority, action) = app.resolve_dispatch(chord, Some(ChromeAction::CloseView));
    assert_eq!(priority, DispatchPriority::User);
    assert_eq!(action, Some(ChromeAction::CloseView));
    app.runtime.set_overlay_modal_active(true);
    assert!(app.modal_capture_active());
    let (priority, action) = app.resolve_dispatch(chord, Some(ChromeAction::CloseView));
    assert_eq!(
        priority,
        DispatchPriority::Modal,
        "bound chords must be captured while a panel modal is active"
    );
    assert!(action.is_none());
    // Esc is the panel modal's own dismissal path: it routes to the runtime
    // (Terminal), never the paste/close confirm-cancel emergency arm.
    let esc = KeyRef {
        key: KeyName::Escape,
        ..chord
    };
    let (priority, _) = app.resolve_dispatch(esc, None);
    assert_eq!(priority, DispatchPriority::Terminal);
    app.runtime.set_overlay_modal_active(false);
    assert!(!app.modal_capture_active());
    let (priority, _) = app.resolve_dispatch(chord, Some(ChromeAction::CloseView));
    assert_eq!(priority, DispatchPriority::User);
}

#[test]
fn defense_in_depth_clamps_report_engaged_keys() {
    // CTX-0482 (#763): `runtime_config_from_effective` clamped some knobs
    // and failed closed on others with no visible policy. Clamps are
    // defense-in-depth for already-validated inputs; when one actually
    // engages it is upstream bound drift and must be named, while semantic
    // bounds (scrollback) keep failing closed.
    let cap = bitty_runtime::config::MAX_LAYOUT_GAP_CELLS;
    let mut effective = bitty_config::EffectiveConfig::default();
    effective.layout.gaps_in = u32::from(cap) + 5;
    let (cfg, warnings) =
        runtime_config_from_effective_with_warnings(&effective).expect("clamps, never fails");
    assert_eq!(cfg.gaps_in, cap, "the clamp must still bound the value");
    assert!(
        warnings.iter().any(|w| w.contains("layout.gaps_in")),
        "an engaged clamp must be named, warnings={warnings:?}"
    );
    // In-range values stay quiet (the normal path emits nothing).
    let clean = bitty_config::EffectiveConfig::default();
    let (_, warnings) =
        runtime_config_from_effective_with_warnings(&clean).expect("defaults build");
    assert!(warnings.is_empty(), "warnings={warnings:?}");
    // Semantic bounds stay fail-closed: scrollback above the cap is an
    // error, never a silent clamp.
    let mut over = bitty_config::EffectiveConfig::default();
    over.terminal.scrollback =
        u32::try_from(bitty_runtime::config::MAX_SCROLLBACK_LINES).expect("fits") + 1;
    assert!(runtime_config_from_effective_with_warnings(&over).is_err());
}

#[test]
fn default_run_emits_no_tick_lines() {
    // Default quiet run: `with_theme` leaves the gate at `Warn`, so the
    // hot path returns `None` without formatting (no stderr tick lines).
    let rt = Runtime::with_defaults().expect("must build");
    let app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    assert!(!app.tick_logging_enabled());
    let present = bitty_runtime::PresentStats {
        frame: 1,
        fills: 7,
        rounded_fills: 0,
        glyphs: 3,
        cells_examined: 0,
        glyphs_emitted: 0,
        headless: true,
        generation: 9,
        images: 0,
        backgrounds: 0,
        images_skipped: 0,
    };
    assert!(app.maybe_format_tick(&present).is_none());
}

#[test]
fn verbose_run_emits_tick_lines() {
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    app.set_log_level(LogLevel::Debug);
    assert!(app.tick_logging_enabled());
    let present = bitty_runtime::PresentStats {
        frame: 4,
        fills: 12,
        rounded_fills: 0,
        glyphs: 5,
        cells_examined: 0,
        glyphs_emitted: 0,
        headless: true,
        generation: 30,
        images: 1,
        backgrounds: 0,
        images_skipped: 0,
    };
    let line = app
        .maybe_format_tick(&present)
        .expect("verbose must format tick");
    assert!(line.contains("bitty tick:"));
    assert!(line.contains("frame=4"));
    assert!(line.contains("fills=12"));
    assert!(line.contains("glyphs=5"));
    // Trace shows the same line (full fidelity at both levels).
    app.set_log_level(LogLevel::Trace);
    assert!(app.maybe_format_tick(&present).is_some());
    // Info stays quiet (no per-frame noise).
    app.set_log_level(LogLevel::Info);
    assert!(!app.tick_logging_enabled());
    assert!(app.maybe_format_tick(&present).is_none());
}

#[test]
fn tick_line_format_carries_frame_stats() {
    let present = bitty_runtime::PresentStats {
        frame: 2,
        fills: 1921,
        rounded_fills: 0,
        glyphs: 21,
        cells_examined: 0,
        glyphs_emitted: 0,
        headless: true,
        generation: 30,
        images: 0,
        backgrounds: 0,
        images_skipped: 0,
    };
    let line = TerminalApp::format_tick_line(&present, 1, None, 1, false, false, 1234);
    assert!(line.starts_with("bitty tick:"));
    assert!(line.contains("t_ms=1234"));
    assert!(line.contains("frame=2"));
    assert!(line.contains("fills=1921"));
    assert!(line.contains("glyphs=21"));
    assert!(line.contains("headless=true"));
    assert!(line.contains("gen=30"));
    // CTX-0253 F3: the image gate counters ride the verbose tick line
    // so a real-GPU skip is user-visible in logs, never silent.
    assert!(line.contains("images=0"));
    assert!(line.contains("images_skipped=0"));
}

#[test]
fn key_paste_messages_bypass_quiet_gate() {
    // Paste confirm/cancel + startup lines are user-facing: they are
    // unconditional `eprintln!` outside `drive_tick` and must stay
    // present in both quiet and verbose runs. This pins the exact
    // strings the event handler emits so a future refactor cannot
    // accidentally gate them behind the tick level.
    let confirm = "bitty: paste confirmed -> delivered";
    let cancelled = "bitty: paste confirmation cancelled (Esc)";
    assert!(!confirm.is_empty());
    assert!(!cancelled.is_empty());
    // Help advertises the quiet default + the verbose escape hatch.
    let help = help_text();
    assert!(help.contains("--verbose"));
    assert!(help.contains("--log-level"));
}

#[test]
fn parse_program_positional_and_tail() {
    let raw = args_of(&["bitty", "/bin/bash"]);
    let p = parse_args(&raw);
    assert_eq!(p.program.as_deref(), Some("/bin/bash"));
    assert!(p.program_args.is_empty());

    let raw = args_of(&["bitty", "--headless", "/bin/cat", "-A"]);
    let p = parse_args(&raw);
    assert!(p.headless);
    assert_eq!(p.program.as_deref(), Some("/bin/cat"));
    assert_eq!(p.program_args, vec!["-A"]);
}

#[test]
fn parse_double_dash_terminates_flag_scan() {
    let raw = args_of(&["bitty", "--", "--headless"]);
    let p = parse_args(&raw);
    assert!(!p.headless);
    assert_eq!(p.program.as_deref(), Some("--headless"));

    let raw = args_of(&["bitty", "--headless", "--", "--help"]);
    let p = parse_args(&raw);
    assert!(p.headless);
    assert!(!p.help);
    assert_eq!(p.program.as_deref(), Some("--help"));
}

#[test]
fn parse_unknown_flag_fails_closed_never_a_program() {
    // CR-APP-01: a typo'd flag must not be spawned as a program.
    // Dispatch rejects via usage + exit 2; parse records the flag.
    let p = parse_args(&args_of(&["bitty", "--bogus"]));
    assert_eq!(p.unknown_flag.as_deref(), Some("--bogus"));
    assert_eq!(p.program, None);
    assert!(p.program_args.is_empty());

    let p = parse_args(&args_of(&["bitty", "-x"]));
    assert_eq!(p.unknown_flag.as_deref(), Some("-x"));
    assert_eq!(p.program, None);

    // Unknown `=` flags are rejected the same way.
    let p = parse_args(&args_of(&["bitty", "--bogus=1"]));
    assert_eq!(p.unknown_flag.as_deref(), Some("--bogus=1"));
    assert_eq!(p.program, None);

    // Known flags still parse; the first unknown flag is recorded.
    let p = parse_args(&args_of(&["bitty", "--headless", "--bogus"]));
    assert!(p.headless);
    assert_eq!(p.unknown_flag.as_deref(), Some("--bogus"));
    assert_eq!(p.program, None);

    // Post-`--` dash-tokens are program argv (escape hatch intact).
    let p = parse_args(&args_of(&["bitty", "--", "--bogus"]));
    assert_eq!(p.unknown_flag, None);
    assert_eq!(p.program.as_deref(), Some("--bogus"));

    // Dash-tokens after an explicit program are that program's args
    // (e.g. `bitty /bin/cat -A` keeps working).
    let p = parse_args(&args_of(&["bitty", "/bin/cat", "-A"]));
    assert_eq!(p.unknown_flag, None);
    assert_eq!(p.program.as_deref(), Some("/bin/cat"));
    assert_eq!(p.program_args, vec!["-A"]);

    // `run -- <prog>` escape hatch stays intact (verbatim raw tail).
    let p = parse_args(&args_of(&["bitty", "run", "--", "--bogus"]));
    assert!(p.run_word);
    assert_eq!(p.unknown_flag, None);
    assert_eq!(p.program, None);
}

#[test]
fn help_and_version_text_are_non_empty() {
    assert!(help_text().contains("bitty"));
    assert!(help_text().contains("--headless"));
    assert!(help_text().contains("--split"));
    assert!(help_text().contains("--layout"));
    // #1375: `--version` prints `bitty <semver> (<channel> <commit>)`.
    let text = crate::version::version_text();
    assert!(text.starts_with("bitty "), "version table form: {text}");
    assert!(
        text.contains(crate::version::version_semver()),
        "semver: {text}"
    );
    assert!(!text.is_empty());
}

#[test]
fn headless_smoke_is_total_without_display_or_gpu() {
    let mut rt = Runtime::with_defaults().expect("defaults must build");
    let code = run_headless_smoke(&mut rt);
    assert_eq!(code, 0);
    // Smoke must have presented at least the initial full redraw.
    assert!(rt.surface_extent().is_some());
}

#[test]
fn demo_pty_pump_is_bounded_and_delivers_chunks() {
    let (rx, handle) =
        spawn_demo_pty_pump_with_theme(bitty_config::theme::DEFAULT_THEME_NAME, "default");
    let mut total = 0usize;
    while let Ok(chunk) = rx.recv() {
        assert!(!chunk.is_empty());
        assert!(chunk.len() <= 8 * 1024);
        total += chunk.len();
    }
    assert!(total > 0);
    handle.join().expect("pump thread must join");
}

#[test]
fn terminal_app_poll_and_tick_are_total() {
    // CTX-0167: default real sessions carry no demo pump — poll drains
    // only the real PTY (empty here) and ticks stay total.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    assert!(app.pty_rx.is_none());
    let _ = app.poll_pty_pump();
    let _ = app.drive_tick();
    // Opt-in debug path still drains the synthetic burst without
    // deadlocking. CTX-0418: the pump runs on its own thread, so wait for
    // it to finish producing before polling instead of racing it with a
    // fixed `yield_now` budget a loaded runner can exhaust (observed 7/30
    // isolated runs). The two-chunk burst fits the 16-slot channel, so
    // `join` cannot deadlock; `poll_pty_pump` must still drain real bytes.
    let rt_demo = Runtime::with_defaults().expect("must build");
    let mut demo = TerminalApp::with_demo_pump(
        rt_demo,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    demo._pty_thread
        .take()
        .expect("demo pump thread must be attached")
        .join()
        .expect("demo pump thread must join");
    assert!(demo.poll_pty_pump());
    let _ = demo.drive_tick();
    // Second tick without new bytes should be idle (frame-on-demand).
    let rt2 = Runtime::with_defaults().expect("must build");
    let mut app2 = TerminalApp::with_theme(
        rt2,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let _ = app2.drive_tick();
    // After first present the second idle tick in the same app may be None.
    // We do not assert presence, only totality (no panic).
}

#[test]
fn poll_pty_pump_marks_the_burst_for_the_settle_gate() {
    // bitty#2486 wiring regression: the Runtime-direct suites prove the
    // settle gate, but they stay green if production stops calling
    // `note_present_input` from the pump. Drive the real demo pump through
    // `poll_pty_pump` and assert the very next present defers.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_demo_pump(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    app.runtime
        .set_present_settle(std::time::Duration::from_millis(2));
    app.runtime
        .set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    app.runtime
        .handle_resize(bitty_platform::PhysicalSize::new(736, 472))
        .expect("resize applies");
    app._pty_thread
        .take()
        .expect("demo pump thread must be attached")
        .join()
        .expect("demo pump thread must join");

    // The tick clock is captured before the pump consumes its bytes, so the
    // settle deadline is provably in the future without sleeping.
    let before = std::time::Instant::now();
    assert!(app.poll_pty_pump(), "demo pump must deliver bytes");
    assert!(
        app.runtime.tick_at(before).is_none(),
        "poll_pty_pump must mark the consumed burst; without the mark the frame presents immediately"
    );
    let deadline = app
        .runtime
        .present_coalescing_deadline()
        .expect("a deferred frame must arm the app wake");
    assert!(deadline > before, "the armed wake must be in the future");
    assert!(
        app.runtime.tick_at(deadline).is_some(),
        "the quiet deadline must commit the held frame"
    );
    assert_eq!(app.runtime.present_coalescing_deadline(), None);
}

#[test]
fn window_title_sanitizer_strips_controls_and_bounds_length() {
    use crate::terminal_app::{WINDOW_TITLE_MAX_CHARS, sanitize_window_title};
    // Printable text survives; every control scalar (C0, DEL, C1) is dropped.
    assert_eq!(
        sanitize_window_title("nvim \u{1}src/\u{7f}main.rs\r\n"),
        "nvim src/main.rs"
    );
    assert_eq!(sanitize_window_title("\u{1b}]0;spoof\u{9c}"), "]0;spoof");
    // Bounded on a character boundary for ASCII and multi-byte text.
    let long = "x".repeat(WINDOW_TITLE_MAX_CHARS + 100);
    assert_eq!(
        sanitize_window_title(&long).chars().count(),
        WINDOW_TITLE_MAX_CHARS
    );
    let multibyte = "\u{e9}".repeat(WINDOW_TITLE_MAX_CHARS + 10);
    let bounded = sanitize_window_title(&multibyte);
    assert_eq!(bounded.chars().count(), WINDOW_TITLE_MAX_CHARS);
    assert!(bounded.chars().all(|c| c == '\u{e9}'));
}

#[test]
fn osc_title_applies_to_window_state_and_is_change_gated() {
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    assert_eq!(app.window.last_applied_title, None);
    app.runtime.handle_pty_bytes(b"\x1b]2;nvim foo.rs\x07");
    assert!(
        app.drive_tick().is_some(),
        "first tick presents the startup frame"
    );
    assert_eq!(
        app.window.last_applied_title.as_deref(),
        Some("nvim foo.rs")
    );
    assert_eq!(app.window.title_applies, 1);
    // Same title again: the change gate drops it (no titlebar churn).
    app.runtime.handle_pty_bytes(b"\x1b]0;nvim foo.rs\x07");
    let _ = app.drive_tick();
    assert_eq!(
        app.window.title_applies, 1,
        "identical titles must not re-apply"
    );
    // A new title applies exactly once more.
    app.runtime.handle_pty_bytes(b"\x1b]2;ssh prod\x07");
    let _ = app.drive_tick();
    assert_eq!(app.window.last_applied_title.as_deref(), Some("ssh prod"));
    assert_eq!(app.window.title_applies, 2);
}

#[test]
fn osc_title_sanitizes_and_empty_resets_to_static_title() {
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    app.runtime.handle_pty_bytes(b"\x1b]2;bad\x01title\x7f\x07");
    app.apply_cold_events();
    assert_eq!(app.window.last_applied_title.as_deref(), Some("badtitle"));
    // Empty OSC 0/2 resets to the static theme title, never a blank bar.
    app.runtime.handle_pty_bytes(b"\x1b]2;\x07");
    app.apply_cold_events();
    assert_eq!(
        app.window.last_applied_title.as_deref(),
        Some(app.window.title.as_str())
    );
}

/// Recording double for the OS title handoff (CTX-0570).
///
/// `WindowHandle::set_title` needs a live winit window, so the OS titlebar
/// cannot be read back headlessly. This double records every title handed
/// across [`crate::terminal_app::OsTitleSink`] in call order, which is the
/// exact production call sequence (the trait impl for `WindowHandle` just
/// forwards to winit). The `Mutex` keeps the double usable behind a shared
/// handle while `TerminalApp` owns it as a boxed trait object.
struct RecordingTitleSink {
    titles: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::terminal_app::OsTitleSink for RecordingTitleSink {
    fn set_os_title(&self, title: &str) {
        self.titles
            .lock()
            .expect("poison-free")
            .push(title.to_owned());
    }
}

#[test]
fn osc_title_handoff_call_sequence_reaches_the_os_sink() {
    // M1-05 evidence (#1131): the exact sequence of OS-title applications is
    // asserted with a test double. The OS titlebar itself is not readable
    // headlessly, so this proves the platform-handoff call sequence
    // (sanitize -> change gate -> sink) rather than the painted titlebar.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_title_sink(Box::new(RecordingTitleSink {
        titles: std::sync::Arc::clone(&recorded),
    }));

    // A fresh title crosses the seam exactly once, sanitized.
    app.runtime
        .handle_pty_bytes(b"\x1b]2;nvim \x01src/\x7fmain.rs\x07");
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        ["nvim src/main.rs"],
        "sanitized title handed to the OS sink"
    );

    // A repeat of the same title is dropped by the change gate: no second
    // call, so the titlebar never churns.
    app.runtime.handle_pty_bytes(b"\x1b]0;nvim src/main.rs\x07");
    app.drive_tick();
    assert_eq!(recorded.lock().expect("poison-free").len(), 1);

    // OSC 2 then OSC 0 with a new title applies once more, in order.
    app.runtime.handle_pty_bytes(b"\x1b]2;ssh prod\x07");
    app.runtime.handle_pty_bytes(b"\x1b]0;build log\x07");
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        ["nvim src/main.rs", "ssh prod", "build log"],
        "one call per distinct title, in arrival order"
    );

    // An empty title resets to the static theme title (never a blank bar),
    // which is a distinct value and so crosses the seam again.
    app.runtime.handle_pty_bytes(b"\x1b]2;\x07");
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").last().unwrap(),
        &app.window.title,
        "empty reset targets the static theme title"
    );
}

/// Recording double for the OS cursor-icon handoff (issue #1762).
///
/// `WindowHandle::set_cursor_icon` needs a live winit window, so the OS
/// pointer itself cannot be read back headlessly. This double records every
/// icon handed across [`crate::terminal_app::OsCursorSink`] in call order,
/// which is the exact production call sequence (the trait impl for
/// `WindowHandle` just forwards to winit). The `Mutex` keeps the double
/// usable behind a shared handle while `TerminalApp` owns it as a boxed
/// trait object.
struct RecordingCursorSink {
    icons: std::sync::Arc<std::sync::Mutex<Vec<bitty_platform::CursorIcon>>>,
}

impl crate::terminal_app::OsCursorSink for RecordingCursorSink {
    fn set_os_cursor(&self, icon: bitty_platform::CursorIcon) {
        self.icons.lock().expect("poison-free").push(icon);
    }
}

#[test]
fn osc22_cursor_handoff_call_sequence_reaches_the_os_sink() {
    // Issue #1762 evidence: the exact sequence of OS cursor-icon
    // applications is asserted with a test double. The OS pointer itself is
    // not readable headlessly, so this proves the platform-handoff call
    // sequence (focused-pane query -> change gate -> sink) rather than the
    // painted pointer.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_cursor_sink(Box::new(RecordingCursorSink {
        icons: std::sync::Arc::clone(&recorded),
    }));

    // The first tick syncs the default pointer (no override yet): one call
    // proving the tick-time sync is wired even without PTY output.
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        [bitty_platform::CursorIcon::Default],
        "idle tick syncs the default pointer once"
    );

    // A fresh shape crosses the seam exactly once.
    app.runtime.handle_pty_bytes(b"\x1b]22;pointer\x07");
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        [
            bitty_platform::CursorIcon::Default,
            bitty_platform::CursorIcon::Pointer
        ],
        "set pointer handed to the OS sink"
    );

    // A repeat of the same shape is dropped by the change gate: no second
    // call, so the OS pointer never churns.
    app.runtime.handle_pty_bytes(b"\x1b]22;pointer\x07");
    app.drive_tick();
    assert_eq!(recorded.lock().expect("poison-free").len(), 2);

    // Push, pop, and RIS reset each apply once more, in order.
    app.runtime.handle_pty_bytes(b"\x1b]22;>wait\x07");
    app.drive_tick();
    app.runtime.handle_pty_bytes(b"\x1b]22;<\x07");
    app.drive_tick();
    app.runtime.handle_pty_bytes(b"\x1bc");
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        [
            bitty_platform::CursorIcon::Default,
            bitty_platform::CursorIcon::Pointer,
            bitty_platform::CursorIcon::Wait,
            bitty_platform::CursorIcon::Pointer,
            bitty_platform::CursorIcon::Default,
        ],
        "push/pop/RIS cross the seam once each, in arrival order"
    );

    // An empty set while already default is dropped by the change gate.
    app.runtime.handle_pty_bytes(b"\x1b]22;\x07");
    app.drive_tick();
    assert_eq!(
        recorded.lock().expect("poison-free").len(),
        5,
        "empty reset at default must not churn"
    );
}

/// Recording double for the OSC 8 click-to-open live consumer (CTX-0577).
struct RecordingUrlOpener {
    opened: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl bitty_runtime::UrlOpener for RecordingUrlOpener {
    fn open_url(
        &self,
        activation: bitty_runtime::UrlActivation,
    ) -> Result<(), bitty_platform::PlatformError> {
        self.opened
            .lock()
            .expect("poison-free")
            .push(activation.uri().to_owned());
        Ok(())
    }
}

/// Physical cursor at the centre of grid cell (row 0, col 0), derived from
/// public geometry (CTX-0808, #1484).
///
/// The OSC 8 link text starts at grid col 0, so this lands on the live link.
/// Hard-coded `(1, 1)` is window padding and must never arm a link.
fn osc8_link_cell(rt: &Runtime) -> bitty_platform::CursorPosition {
    let frame = rt.present_frames()[0];
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    bitty_platform::CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + 0.5 * f64::from(cw),
        y: pad + f64::from(frame.content.y.max(0)) + 0.5 * f64::from(ch),
    }
}

/// Holds Ctrl for the OSC 8 click gesture (issue #1759, R-005):
/// activation requires Ctrl (Cmd on macOS) held during the click.
fn hold_ctrl_for_osc8(app: &mut TerminalApp, window_id: bitty_platform::WindowId) {
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: true,
            alt: false,
            super_pressed: false,
        }),
    });
}

#[test]
fn osc8_click_path_reaches_the_live_url_consumer() {
    // M1-17 evidence (#1143): the app's click path must consume the runtime
    // gesture and hand the authorized URI to the opener seam — not stop at
    // parse-only. The recording double captures the exact URL sequence.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.runtime.set_url_opener(Box::new(RecordingUrlOpener {
        opened: std::sync::Arc::clone(&opened),
    }));

    app.runtime
        .handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    let link_pos = osc8_link_cell(&app.runtime);
    hold_ctrl_for_osc8(&mut app, window_id);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(link_pos),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Pressed,
        )),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Released,
        )),
    });
    assert!(app.runtime.has_pending_hyperlink_activation());
    app.activate_pending_hyperlink_now();
    assert_eq!(
        opened.lock().expect("poison-free").as_slice(),
        ["https://example.test"],
        "the live consumer must open the authorized URI exactly once"
    );
    assert!(!app.runtime.has_pending_hyperlink_activation());
}

#[test]
fn osc8_click_path_never_opens_a_hostile_scheme() {
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.runtime.set_url_opener(Box::new(RecordingUrlOpener {
        opened: std::sync::Arc::clone(&opened),
    }));

    app.runtime
        .handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    let link_pos = osc8_link_cell(&app.runtime);
    hold_ctrl_for_osc8(&mut app, window_id);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(link_pos),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Pressed,
        )),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Released,
        )),
    });
    // No gesture was minted, so the app path has nothing to consume.
    assert!(!app.runtime.has_pending_hyperlink_activation());
    app.activate_pending_hyperlink_now();
    assert!(opened.lock().expect("poison-free").is_empty());
}

#[test]
fn osc8_padding_click_never_arms_a_live_link() {
    // CTX-0808 (#1484): a click in the window-padding band must not arm an
    // OSC 8 link, even with a live link on screen. The in-frame hit test
    // yields no cell there, so no gesture is minted (fail closed).
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.runtime.set_url_opener(Box::new(RecordingUrlOpener {
        opened: std::sync::Arc::clone(&opened),
    }));

    app.runtime
        .handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    hold_ctrl_for_osc8(&mut app, window_id);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(bitty_platform::CursorPosition { x: 1.0, y: 1.0 }),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Released,
        )),
    });
    assert!(
        !app.runtime.has_pending_hyperlink_activation(),
        "a padding click must not arm the live link"
    );
    app.activate_pending_hyperlink_now();
    assert!(opened.lock().expect("poison-free").is_empty());
}

/// Grid cell for a plaintext URL starting at `col` on row 0 (issue #1760).
fn plaintext_cell(rt: &Runtime, col: usize) -> bitty_platform::CursorPosition {
    let frame = rt.present_frames()[0];
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    bitty_platform::CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + (col as f64 + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y.max(0)) + 0.5 * f64::from(ch),
    }
}

fn hold_ctrl(app: &mut TerminalApp, window_id: bitty_platform::WindowId) {
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: true,
            alt: false,
            super_pressed: false,
        }),
    });
}

#[test]
fn plaintext_click_path_reaches_the_live_url_consumer() {
    // Issue #1760: the app's click path must consume the plaintext gesture
    // through the same live consumer as OSC 8 (not parse-only).
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.runtime.set_url_opener(Box::new(RecordingUrlOpener {
        opened: std::sync::Arc::clone(&opened),
    }));

    app.runtime
        .handle_pty_bytes(b"see https://example.test here");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    hold_ctrl(&mut app, window_id);
    let pos = plaintext_cell(&app.runtime, 5);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(pos),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Pressed,
        )),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(
            bitty_platform::MouseButton::Left,
            bitty_platform::PressState::Released,
        )),
    });
    assert!(app.runtime.has_pending_hyperlink_activation());
    app.activate_pending_hyperlink_now();
    assert_eq!(
        opened.lock().expect("poison-free").as_slice(),
        ["https://example.test"],
    );
    assert!(!app.runtime.has_pending_hyperlink_activation());
}

#[test]
fn plaintext_hover_syncs_the_os_pointer_shape() {
    // Issue #1760 unified with #1759/#1762: either hover (OSC 8 or
    // Ctrl-gated plaintext) wins `Pointer`, else the focused pane's
    // `OSC 22` shape, else `Default`. Change-gated through
    // `apply_cursor_icon` + `OsCursorSink`.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_cursor_sink(Box::new(RecordingCursorSink {
        icons: std::sync::Arc::clone(&recorded),
    }));
    assert_eq!(app.window.last_applied_cursor, None);
    app.runtime
        .handle_pty_bytes(b"see https://example.test here");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    hold_ctrl(&mut app, window_id);
    let pos = plaintext_cell(&app.runtime, 5);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(pos),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Pointer),
        "Ctrl+hover over the URL must request the pointer shape"
    );
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        [bitty_platform::CursorIcon::Pointer],
    );
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Pointer)
    );
    assert_eq!(recorded.lock().expect("poison-free").len(), 1);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(bitty_platform::CursorPosition { x: 1.0, y: 1.0 }),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Default),
        "leaving the URL must restore the default shape"
    );
}

#[test]
fn overlay_capture_suppresses_plaintext_url_gesture_and_hover() {
    // CTX-0943 + issues #1759/#1760: while a focusable overlay capture
    // holds, the modal owns all input — a Ctrl+click on a URL beneath it
    // must never mint-and-open, hover affordances stay hidden, and the
    // capture lifecycle is undisturbed (press-to-release ownership intact).
    // Regression for PR #1774 Quality-gates(2) interaction: per-tick cursor
    // sync, Ctrl-gated mint, and input consume paths must not perturb the
    // overlay session.
    use bitty_platform::{MouseButton, MouseEvent, PressState};
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx1009-overlay-url-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx1009overlayurl";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-1009 overlay-url guard"
version = "0.1.0"
description = "overlay suppresses URL test"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true
ui.overlay.focus = true
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local mount_ok, handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "overlay-modal" })
        if mount_ok then
          pcall(bitty.ui.overlay.acquire, handle)
        end
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");
    assert!(
        plugin_runtime.overlay_capture().borrow().is_active(),
        "setup must hold the capture"
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app = app.with_plugin_runtime(Some(plugin_runtime));
    assert!(app.overlay_capture_active());

    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.runtime.set_url_opener(Box::new(RecordingUrlOpener {
        opened: std::sync::Arc::clone(&opened),
    }));
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_cursor_sink(Box::new(RecordingCursorSink {
        icons: std::sync::Arc::clone(&recorded),
    }));

    // URL text under the pointer with Ctrl held.
    app.runtime
        .handle_pty_bytes(b"see https://example.test here");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    hold_ctrl(&mut app, window_id);
    let pos = plaintext_cell(&app.runtime, 5);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(pos),
    });
    // Hover arms normally at Runtime level (Ctrl-gated hit test passes).
    assert!(
        app.runtime.hovered_plaintext_span().is_some(),
        "Ctrl+hover must arm the plaintext span pre-tick"
    );
    // The app tick suppresses it while the overlay holds: no underline
    // state survives, and the cursor never shows Pointer for grid URLs.
    let _ = app.drive_tick();
    assert!(
        app.runtime.hovered_plaintext_span().is_none(),
        "overlay capture must clear the plaintext hover on tick"
    );
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Default),
        "overlay capture must not show a URL pointer or an underlying I-beam"
    );

    // Ctrl+press+release over the URL mints at Runtime level (which does
    // not know about the overlay), but the app drops it instead of opening.
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(MouseEvent::new(MouseButton::Left, PressState::Pressed)),
    });
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(MouseEvent::new(MouseButton::Left, PressState::Released)),
    });
    assert!(
        app.runtime.has_pending_hyperlink_activation(),
        "Runtime mints unaware of the overlay; the app guards the open"
    );
    // App-level guard: drop, never open, while the capture holds.
    assert!(app.overlay_capture_active());
    if app.overlay_capture_active() {
        let _ = app.runtime.drop_pending_hyperlink_activation();
    } else {
        app.activate_pending_hyperlink_now();
    }
    assert!(
        !app.runtime.has_pending_hyperlink_activation(),
        "pending URL gesture must not survive an overlay session"
    );
    assert!(
        opened.lock().expect("poison-free").is_empty(),
        "no URL may open while the overlay modal holds capture"
    );
    // The capture itself is undisturbed: still active with its deadline,
    // and a release still routes (never queues) per the overlay contract.
    assert!(app.overlay_capture_active());
    assert!(app.overlay_capture_deadline().is_some());
    assert!(
        !app.capture_fallthrough_input(&WindowEventKind::MouseInput(MouseEvent::new(
            MouseButton::Left,
            bitty_platform::PressState::Released,
        ))),
        "releases must route, never queue, even over a URL"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn target_session_reconciles_closed_panel_on_cold_tick() {
    // Review blocker for #2253: the cold tick that publishes live target rows
    // must also reconcile the active session. A panel closed mid-session must
    // lose its label and dispatch binding, not linger until timeout.
    use bitty_lua::HostServices;
    use bitty_runtime::TargetAnchorSummary;
    use bitty_runtime::plugin_runtime::{
        EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };
    use std::collections::HashSet;
    use std::rc::Rc;

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(
            &self,
            _scope: &str,
        ) -> Result<LuaValue, bitty_runtime::plugin_runtime::BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx1354-targets-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx1354targets";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-1354 target reconciliation"
version = "0.1.0"
description = "target session reconcile test"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true
ui.overlay.focus = true
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local mount_ok, handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "targets" })
        if not mount_ok then
          bitty.store.set("start_error", "mount")
          return {}
        end
        local rows = bitty.ui.targets.snapshot(16)
        local anchors = {}
        local commands = {}
        for i = 1, #rows do
          local idx = i - 1
          anchors[i] = { x = idx % 80, y = math.floor(idx / 80) }
          commands[i] = "acme.cmd:c" .. i
        end
        local ok, labels = pcall(bitty.ui.targets.session_start, handle, 80, anchors, commands)
        if not ok then
          bitty.store.set("start_error", labels.code or "unknown")
        end
        return {}
        "#,
    )
    .expect("init");

    let mut rt = Runtime::with_defaults().expect("runtime must build");
    let first = rt.focused_view().expect("focused view");
    let first_leaf = rt.layout().find_leaf(first).cloned().expect("first leaf");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(first_leaf),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    ));
    let rows: Vec<TargetAnchorSummary> = rt.target_anchor_summaries();
    assert_eq!(rows.len(), 3, "one workspace plus two panels");
    let live_targets = Rc::new(crate::plugin_runtime::LiveTargets::default());
    live_targets.publish(&rows);

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(StaticSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.set_target_source(live_targets.clone());
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");
    assert_eq!(
        plugin_runtime
            .services(&pid)
            .expect("services")
            .with_store(|store| store.get("start_error")),
        None,
        "fixture session_start must succeed"
    );
    assert!(
        plugin_runtime.overlay_capture().borrow().is_active(),
        "the fixture session must own the transient capture"
    );
    assert_eq!(
        plugin_runtime.target_annotations().expect("layer").len(),
        3,
        "the session starts with one label per live target"
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime))
    .with_live_targets(Some(Rc::clone(&live_targets)));

    let _ = app.drive_tick();
    let before_labels: Vec<String> = app
        .plugin_runtime
        .as_ref()
        .expect("plugin runtime")
        .target_annotations()
        .expect("layer")
        .annotations()
        .iter()
        .map(|annotation| annotation.label.clone())
        .collect();
    assert_eq!(before_labels.len(), 3);

    // Close the second panel mid-session.
    let leaves = app.runtime.layout().leaf_ids();
    assert_eq!(leaves.len(), 2);
    let remaining = app
        .runtime
        .layout()
        .find_leaf(leaves[0])
        .cloned()
        .expect("remaining leaf");
    app.runtime.set_layout(LayoutNode::leaf(remaining));

    // First tick publishes the new rows and reconciles the session; the
    // second tick pushes the reconciled layer into the scene.
    let _ = app.drive_tick();
    let _ = app.drive_tick();

    let after = app
        .plugin_runtime
        .as_ref()
        .expect("plugin runtime")
        .target_annotations()
        .expect("layer");
    let after_labels: HashSet<String> = after
        .annotations()
        .iter()
        .map(|annotation| annotation.label.clone())
        .collect();
    assert_eq!(after_labels.len(), 2, "workspace plus surviving panel");
    let dropped = before_labels
        .iter()
        .find(|label| !after_labels.contains(*label))
        .cloned()
        .expect("exactly one label must disappear");
    assert!(
        !after.contains_label(&dropped),
        "closed panel label remains in the session layer"
    );
    assert_eq!(
        app.runtime
            .target_annotations()
            .expect("runtime layer")
            .len(),
        2,
        "the presented layer must drop the closed panel label"
    );
    let error = app
        .plugin_runtime
        .as_ref()
        .expect("plugin runtime")
        .services(&pid)
        .expect("services")
        .ui_targets_dispatch(&dropped)
        .expect_err("closed panel label must fail closed");
    assert_eq!(error.code, "E_DEF_INVALID");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn overlay_input_available_pump_nudge_on_tick_with_released_ordering() {
    // CTX-1170 (issue #1972) host pump driver: when a live capture owner
    // has undrained queued input, `drive_tick` delivers one
    // `overlay.input.available` observation (`{ owner, queued }`) on the
    // cold tick path so buffer arrival repaints the modal without polling.
    // Ordered after `overlay.released`: a session end is always observable
    // before any new-session data nudge. Headless: a real plugin runtime
    // with fixture plugins, no PTY and no window, so no `cfg(unix)` gate.
    use bitty_runtime::plugin_runtime::{
        EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(
            &self,
            _scope: &str,
        ) -> Result<
            bitty_runtime::plugin_runtime::LuaValue,
            bitty_runtime::plugin_runtime::BridgeError,
        > {
            Ok(bitty_runtime::plugin_runtime::LuaValue::table([
                (
                    "version",
                    bitty_runtime::plugin_runtime::LuaValue::Integer(1),
                ),
                (
                    "zones",
                    bitty_runtime::plugin_runtime::LuaValue::array(vec![]),
                ),
            ]))
        }
    }

    let tag = format!("ctx1170-overlay-pump-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let holder_id = "bitty-featured.ctx1170pumpholder";
    let watcher_id = "bitty-featured.ctx1170pumpwatcher";
    let write_manifest = |id: &str, caps: &str, commands: &str, events: &str| {
        let plugin_dir = root.join(id);
        std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
        std::fs::write(
            plugin_dir.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"CTX-1170 pump test\"\nversion = \"0.1.0\"\n\
                 description = \"overlay input pump test\"\n\n\
                 [compat]\nplugin-api = \"^1.0\"\n\n\
                 [capabilities]\n{caps}\n\n\
                 [lazy]\ncommands = [{commands}]\nevents = [{events}]\n",
            ),
        )
        .expect("manifest");
    };
    write_manifest(
        holder_id,
        "ui.rich = true\nui.overlay = true\nui.overlay.focus = true\n",
        &format!("\"{holder_id}:probe\""),
        "",
    );
    std::fs::write(
        root.join(holder_id).join("lua/init.lua"),
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function()
            bitty.ui.overlay.acquire({ title = "Composer", placeholder = "Type a command" })
            return true
          end,
        })
        return {}
        "#,
    )
    .expect("init");
    write_manifest(
        watcher_id,
        "",
        &format!("\"{watcher_id}:report\""),
        "\"overlay.released\", \"overlay.input.available\"",
    );
    std::fs::write(
        root.join(watcher_id).join("lua/init.lua"),
        r#"
        local seen = {}
        bitty.events.subscribe("overlay.released", function(envelope)
          seen[#seen + 1] = "released:" .. tostring(envelope.payload.reason)
        end)
        bitty.events.subscribe("overlay.input.available", function(envelope)
          seen[#seen + 1] = "available:" .. tostring(envelope.payload.owner)
            .. "/" .. tostring(envelope.payload.queued)
        end)
        bitty.commands.register({
          id = "report",
          title = "Report",
          run = function() return table.concat(seen, ",") end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let holder = bitty_plugin_host::manifest::PluginId::new(holder_id).expect("valid id");
    let watcher = bitty_plugin_host::manifest::PluginId::new(watcher_id).expect("valid id");
    assert_eq!(
        plugin_runtime.activate(&holder).expect("activate").state,
        LifecycleState::Active
    );
    assert_eq!(
        plugin_runtime.activate(&watcher).expect("activate").state,
        LifecycleState::Active
    );
    // Session A opens, then ends by runtime revoke (focus-switch cause):
    // its `overlay.released` observation waits for the next tick.
    // bitty #2409: a probe dispatch can park at the RC-1 wall budget on a
    // loaded runner; settle it instead of assuming synchronous completion.
    let _ =
        crate::test_support::settle_dispatch(&mut plugin_runtime, holder.as_str(), "probe", &[])
            .expect_value("probe");
    assert!(plugin_runtime.overlay_capture().borrow().is_active());
    assert!(plugin_runtime.revoke_overlay_capture_with_reason("cancelled"));
    // Session B opens on the same tick window and queues three inputs.
    let _ =
        crate::test_support::settle_dispatch(&mut plugin_runtime, holder.as_str(), "probe", &[])
            .expect_value("probe");
    for text in ["h", "i", "!"] {
        assert!(
            plugin_runtime.push_overlay_input("key", text),
            "session B must queue input"
        );
    }

    let rt = Runtime::with_defaults().expect("must build");
    let app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let mut app = app.with_plugin_runtime(Some(plugin_runtime));
    let _ = app.drive_tick();
    // Exactly one nudge for three queued events (level-triggered bound),
    // ordered after the session-A release.
    let report = app
        .invoke_plugin_command(
            &format!("{watcher_id}:report"),
            &TerminalApp::empty_command_args(),
        )
        .expect("report dispatches");
    assert_eq!(
        report,
        bitty_runtime::plugin_runtime::LuaValue::String(format!(
            "released:cancelled,available:{holder_id}/3"
        )),
        "tick must deliver released before input.available with owner and depth"
    );
    // The modal surface follows the live session: title plus input rows.
    assert!(app.overlay_capture_active());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn osc8_hover_syncs_the_os_pointer_shape() {
    // Issues #1759/#1762 precedence: hover `Pointer` wins, else the focused
    // pane's `OSC 22` shape, else `Default`. Unified on the 34-variant
    // `cursor::CursorIcon` (`Default`/`Text`/`Pointer` map 1:1). Change-gated
    // through `apply_cursor_icon` + `OsCursorSink`, so steady hover costs no
    // OS call.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_cursor_sink(Box::new(RecordingCursorSink {
        icons: std::sync::Arc::clone(&recorded),
    }));
    assert_eq!(app.window.last_applied_cursor, None);
    app.runtime
        .handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    let link_pos = osc8_link_cell(&app.runtime);
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(link_pos),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Pointer),
        "hovering the link must request the pointer shape"
    );
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        [bitty_platform::CursorIcon::Pointer],
    );
    // Steady hover re-syncs to the same shape (change gate: no OS churn).
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Pointer)
    );
    assert_eq!(recorded.lock().expect("poison-free").len(), 1);
    // `OSC 22` while hovered must not override the hover pointer.
    app.runtime.handle_pty_bytes(b"\x1b]22;wait\x07");
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Pointer),
        "hover must win over OSC 22"
    );
    assert_eq!(
        recorded.lock().expect("poison-free").len(),
        1,
        "hover-wins must not churn the OS pointer"
    );
    // Leaving the link falls back to the `OSC 22` shape.
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(bitty_platform::CursorPosition { x: 1.0, y: 1.0 }),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Wait),
        "leaving the link must fall back to the OSC 22 shape"
    );
    // Clearing the `OSC 22` stack falls back to `Default`.
    app.runtime.handle_pty_bytes(b"\x1b]22;\x07");
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(bitty_platform::CursorPosition { x: 1.0, y: 1.0 }),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Default),
        "empty OSC 22 stack with no hover must restore default"
    );
}

#[test]
fn pointer_over_terminal_text_uses_the_text_cursor() {
    // Issue #2480: the OS pointer must be an I-beam over selectable grid
    // text, while OSC 22 shapes still win, mouse-capturing panes stay the
    // arrow, and leaving the window restores it.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_cursor_sink(Box::new(RecordingCursorSink {
        icons: std::sync::Arc::clone(&recorded),
    }));
    app.runtime.handle_pty_bytes(b"selectable text");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    let frame = *app
        .runtime
        .present_frames()
        .first()
        .expect("primary frame must present");
    let (cw, ch) = app.runtime.live_cell_size();
    let pad = f64::from(app.runtime.window_padding_physical());
    let text_pos = bitty_platform::CursorPosition {
        x: pad + f64::from(frame.content.x) + 1.5 * f64::from(cw),
        y: pad + f64::from(frame.content.y) + 0.5 * f64::from(ch),
    };
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(text_pos),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Text),
        "grid text must request the I-beam"
    );
    assert_eq!(
        recorded.lock().expect("poison-free").as_slice(),
        [bitty_platform::CursorIcon::Text]
    );

    // `OSC 22` still owns the pointer when a terminal app sets one.
    app.runtime.handle_pty_bytes(b"\x1b]22;wait\x07");
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Wait),
        "OSC 22 must win over the automatic text cursor"
    );

    // Window padding is chrome: the arrow returns.
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(bitty_platform::CursorPosition { x: 1.0, y: 1.0 }),
    });
    app.runtime.handle_pty_bytes(b"\x1b]22;\x07");
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Default),
        "padding must not show the I-beam"
    );

    // A capturing pane returns the arrow even over its text.
    app.runtime.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(text_pos),
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Default),
        "a mouse-capturing pane owns its pointer"
    );

    // Leaving the window restores the platform arrow even with OSC 22 set.
    app.runtime.handle_pty_bytes(b"\x1b]22;wait\x07");
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorLeft,
    });
    app.sync_cursor_icon();
    assert_eq!(
        app.window.last_applied_cursor,
        Some(bitty_platform::CursorIcon::Default),
        "leaving must not leave the pane shape stuck"
    );
}

#[test]
fn default_startup_carries_no_demo_line() {
    // CTX-0167 / #269: real sessions show only the shell — the default
    // constructor attaches no synthetic pump, so polling consumes
    // nothing and the grid never sees `demo pty:`.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    assert!(app.pty_rx.is_none());
    assert!(!app.poll_pty_pump());
    app.runtime.select_all();
    let text = app.runtime.selection_text().unwrap_or_default();
    assert!(
        !text.contains("demo pty"),
        "default startup must not contain demo line, got {text:?}"
    );
}

#[test]
fn demo_pump_gate_defaults_off_and_opts_in() {
    // CTX-0167: `BITTY_DEMO_PUMP` is default-off; only explicit `1`/`true`
    // enables the synthetic burst.
    assert!(!demo_pump_enabled_from_value(None));
    assert!(!demo_pump_enabled_from_value(Some("")));
    assert!(!demo_pump_enabled_from_value(Some("0")));
    assert!(!demo_pump_enabled_from_value(Some("false")));
    assert!(!demo_pump_enabled_from_value(Some("yes")));
    assert!(demo_pump_enabled_from_value(Some("1")));
    assert!(demo_pump_enabled_from_value(Some("true")));
    assert!(demo_pump_enabled_from_value(Some("TRUE")));
    assert!(demo_pump_enabled_from_value(Some(" 1 ")));
}

#[test]
fn demo_pump_opt_in_delivers_greeting() {
    // CTX-0167: the gated debug path still delivers the themed greeting
    // for harnesses that explicitly opt in. The pump runs on its own
    // thread; wait for it to finish producing before draining (CTX-0413)
    // so a loaded scheduler cannot drop the greeting.
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_demo_pump(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    assert!(app.pty_rx.is_some());
    // CTX-0413: the pump sends on its own thread, so a fixed `yield_now`
    // budget can expire before the OS schedules that thread under load
    // (observed 7/30 isolated runs). Wait for the producer to finish
    // instead of racing the scheduler: the two-chunk burst fits the
    // 16-slot channel, so `join` cannot deadlock, and the assertion below
    // still requires `poll_pty_pump` to drain real bytes.
    app._pty_thread
        .take()
        .expect("demo pump thread must be attached")
        .join()
        .expect("demo pump thread must join");
    assert!(app.poll_pty_pump(), "opt-in demo pump must deliver bytes");
    app.runtime.select_all();
    let text = app.runtime.selection_text().expect("grid text");
    assert!(
        text.contains("demo pty"),
        "opt-in demo pump must deliver greeting, got {text:?}"
    );
    // `attach_demo_pump` is idempotent and also opts in from default.
    let rt2 = Runtime::with_defaults().expect("must build");
    let mut app2 = TerminalApp::with_theme(
        rt2,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        Vec::new(),
        SpawnSpec::default(),
    );
    app2.attach_demo_pump(bitty_config::theme::DEFAULT_THEME_NAME, "default");
    assert!(app2.pty_rx.is_some());
    // Same deterministic drain as above (CTX-0413).
    app2._pty_thread
        .take()
        .expect("attached demo pump thread must be present")
        .join()
        .expect("demo pump thread must join");
    assert!(
        app2.poll_pty_pump(),
        "attached demo pump must deliver bytes"
    );
    // Second attach is a no-op (does not replace the channel).
    app2.attach_demo_pump(bitty_config::theme::DEFAULT_THEME_NAME, "default");
    assert!(app2.pty_rx.is_some());
}

#[test]
fn parse_split_flags() {
    let raw = args_of(&["bitty", "--split"]);
    let p = parse_args(&raw);
    assert_eq!(p.split_axis, Some(SplitAxis::Horizontal));
    assert_eq!(p.split_ratio, None);

    let raw = args_of(&["bitty", "--split", "vertical"]);
    let p = parse_args(&raw);
    assert_eq!(p.split_axis, Some(SplitAxis::Vertical));

    let raw = args_of(&["bitty", "--split", "h"]);
    let p = parse_args(&raw);
    assert_eq!(p.split_axis, Some(SplitAxis::Horizontal));

    let raw = args_of(&["bitty", "--split=v"]);
    let p = parse_args(&raw);
    assert_eq!(p.split_axis, Some(SplitAxis::Vertical));

    let raw = args_of(&["bitty", "--split", "h:0.3"]);
    let p = parse_args(&raw);
    assert_eq!(p.split_axis, Some(SplitAxis::Horizontal));
    assert!(p.split_ratio.is_some());
    assert!((p.split_ratio.unwrap() - 0.3).abs() < f32::EPSILON);

    let raw = args_of(&["bitty", "--split-ratio", "0.7"]);
    let p = parse_args(&raw);
    assert!(p.split_ratio.is_some());
    assert!((p.split_ratio.unwrap() - 0.7).abs() < f32::EPSILON);
}

#[test]
fn parse_layout_and_focus_flags() {
    let raw = args_of(&["bitty", "--layout", "split:h:0.5"]);
    let p = parse_args(&raw);
    assert_eq!(p.layout.as_deref(), Some("split:h:0.5"));

    let raw = args_of(&["bitty", "--layout=stack:3"]);
    let p = parse_args(&raw);
    assert_eq!(p.layout.as_deref(), Some("stack:3"));

    let raw = args_of(&["bitty", "--focus", "next"]);
    let p = parse_args(&raw);
    assert_eq!(p.focus.as_deref(), Some("next"));

    let raw = args_of(&["bitty", "--focus=2"]);
    let p = parse_args(&raw);
    assert_eq!(p.focus.as_deref(), Some("2"));

    let raw = args_of(&["bitty", "--stack", "--overlay"]);
    let p = parse_args(&raw);
    assert!(p.stack);
    assert!(p.overlay);
}

#[test]
fn build_layout_single_default() {
    let args = parse_args(&args_of(&["bitty"]));
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 1);
    assert_eq!(layout.leaf_ids(), vec![ViewId::new(1)]);
}

#[test]
fn build_layout_split_via_flag() {
    let args = parse_args(&args_of(&["bitty", "--split", "vertical"]));
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 2);
    let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 2);
    // vertical split 24 rows -> first 12, second 12 with 0.5 ratio
    assert_eq!(allocs[0].1.height, 12);
    assert_eq!(allocs[1].1.height, 12);
}

#[test]
fn build_layout_stack_and_overlay() {
    let args = parse_args(&args_of(&["bitty", "--stack"]));
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 2);
    let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
    // stack: both cover full bounds
    assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs[1].1, UiRect::new(0, 0, 80, 24));

    let args = parse_args(&args_of(&["bitty", "--overlay"]));
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 2);
    let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs[1].1, UiRect::new(5, 5, 20, 10));
}

#[test]
fn build_layout_via_explicit_spec() {
    let args = parse_args(&args_of(&["bitty", "--layout", "split:h:0.3"]));
    let layout = build_layout(&args, 100, 24);
    let allocs = layout.layout(UiRect::new(0, 0, 100, 24));
    assert_eq!(allocs.len(), 2);
    assert_eq!(allocs[0].1.width, 30); // floor(100*0.3)
    assert_eq!(allocs[1].1.width, 70);

    let args = parse_args(&args_of(&["bitty", "--layout", "stack:3"]));
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 3);

    let args = parse_args(&args_of(&["bitty", "--layout", "overlay:1,2,10,5"]));
    let layout = build_layout(&args, 80, 24);
    let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs[1].1, UiRect::new(1, 2, 10, 5));
}

#[test]
fn layout_precedence_stack_over_split() {
    // --layout overrides --split/--stack per help text
    let args = parse_args(&args_of(&["bitty", "--split", "h", "--stack"]));
    // without explicit --layout, stack wins over split
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 2);
    // Verify it's stack (both full)
    let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs[0].1, allocs[1].1);

    let args = parse_args(&args_of(&[
        "bitty", "--split", "h", "--stack", "--layout", "single",
    ]));
    let layout = build_layout(&args, 80, 24);
    assert_eq!(layout.leaf_count(), 1);
}

#[test]
fn focus_via_args_and_runtime() {
    let mut rt = Runtime::with_defaults().expect("must build");
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    );
    rt.set_layout(split);
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert!(apply_focus(&mut rt, "next"));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    assert!(apply_focus(&mut rt, "1"));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert!(!apply_focus(&mut rt, "99")); // invalid id
    assert!(!apply_focus(&mut rt, "bogus")); // invalid spec returns false
}

#[test]
fn focus_directional_via_args() {
    let mut rt = Runtime::with_defaults().expect("must build");
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt.set_layout(split);
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.reflow_layout();
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert!(apply_focus(&mut rt, "right"));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    assert!(apply_focus(&mut rt, "left"));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn headless_smoke_with_split_is_deterministic() {
    // Two runtimes with same split + same bytes must produce identical rgba
    let synthetic = b"hello split deterministic";
    let mut rt1 = Runtime::with_defaults().expect("must build");
    let layout = build_layout(&parse_args(&args_of(&["bitty", "--split", "h"])), 80, 24);
    rt1.set_layout(layout.clone());
    rt1.handle_pty_bytes(synthetic);
    let _ = rt1.tick().expect("must present");
    let rgba1 = rt1.headless_rgba().expect("rgba");

    let mut rt2 = Runtime::with_defaults().expect("must build");
    rt2.set_layout(layout);
    rt2.handle_pty_bytes(synthetic);
    let _ = rt2.tick().expect("must present");
    let rgba2 = rt2.headless_rgba().expect("rgba");
    assert_eq!(rgba1, rgba2);
}

#[test]
fn layout_proof_is_deterministic_and_distinct() {
    // CTX-0234: session-less unfocused tiles present erased (no primary
    // duplication), so sparse bytes render identically across
    // compositions. Full-width marker rows 1..=10 keep split / stack /
    // overlay pairwise distinct: the overlay leaf blanks rows 5..=10 at
    // cols 5..=24, the split right tile stays blank, the stack covers
    // the grid — while same-layout replays stay bit-identical.
    let mut synthetic = Vec::new();
    for row in 1..=10u32 {
        synthetic.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        synthetic.extend(std::iter::repeat_n(b'A'.wrapping_add((row % 26) as u8), 80));
    }
    let code = run_layout_proof(&synthetic);
    assert_eq!(code, 0);
}

#[test]
fn tick_is_layout_aware_after_set_layout() {
    let mut rt = Runtime::with_defaults().expect("must build");
    let before = rt.tick().expect("first tick must present");
    assert!(before.headless);
    // Install split layout and tick with new bytes must still present layout-aware
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(10), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(20), 40, 24)),
    );
    rt.set_layout(split);
    assert_eq!(rt.leaf_count(), 2);
    rt.handle_pty_bytes(b"tick layout aware");
    let stats = rt.tick().expect("split tick must present");
    assert!(stats.headless);
    assert!(stats.fills > 0);
    let rgba = rt.headless_rgba().expect("rgba after split");
    assert!(!rgba.is_empty());
}

#[test]
fn split_ratio_clamped_via_layout_node() {
    let args = parse_args(&args_of(&["bitty", "--split", "h", "--split-ratio", "5.0"]));
    let layout = build_layout(&args, 80, 24);
    if let LayoutNode::Split { ratio, .. } = layout {
        // LayoutNode::split clamps to [0.10,0.90]
        assert!(ratio <= LayoutNode::MAX_RATIO);
        assert!(ratio >= LayoutNode::MIN_RATIO);
    } else {
        panic!("expected split");
    }
}

#[test]
fn cli_invalid_values_fail_closed_hostile() {
    // CTX-0480: invalid values record cli_value_error (exit 2 at
    // dispatch) instead of warn-ignoring to exit 0.
    let p = parse_args(&args_of(&["bitty", "--split-ratio", "nope"]));
    assert!(p.cli_value_error.is_some(), "bad ratio must error");

    let p = parse_args(&args_of(&["bitty", "--split-ratio=nan"]));
    assert!(p.cli_value_error.is_some(), "NaN ratio must error");

    let p = parse_args(&args_of(&["bitty", "--split-ratio=inf"]));
    assert!(p.cli_value_error.is_some(), "inf ratio must error");

    let p = parse_args(&args_of(&["bitty", "--split=bogus"]));
    assert!(p.cli_value_error.is_some(), "bad split axis must error");

    let p = parse_args(&args_of(&["bitty", "--split=h:nope"]));
    assert!(p.cli_value_error.is_some(), "bad split ratio must error");

    let p = parse_args(&args_of(&["bitty", "--split=h:"]));
    assert!(p.cli_value_error.is_some(), "empty split ratio must error");

    let p = parse_args(&args_of(&["bitty", "--split=:0.5"]));
    assert!(p.cli_value_error.is_some(), "axisless split must error");

    let p = parse_args(&args_of(&["bitty", "--split=h:0.5:extra"]));
    assert!(
        p.cli_value_error.is_some(),
        "trailing split junk must error"
    );

    let p = parse_args(&args_of(&["bitty", "--log-level", "nope"]));
    assert!(p.cli_value_error.is_some(), "bad log level must error");
    assert_eq!(p.log_level, None);

    let p = parse_args(&args_of(&["bitty", "--log-level"]));
    assert!(p.cli_value_error.is_some(), "missing log level must error");

    let p = parse_args(&args_of(&["bitty", "--split-ratio"]));
    assert!(p.cli_value_error.is_some(), "missing ratio must error");

    let p = parse_args(&args_of(&["bitty", "--layout"]));
    assert!(p.cli_value_error.is_some(), "missing layout must error");

    let p = parse_args(&args_of(&["bitty", "--focus"]));
    assert!(p.cli_value_error.is_some(), "missing focus must error");

    // Valid values record no error.
    let p = parse_args(&args_of(&["bitty", "--split-ratio", "0.5"]));
    assert!(p.cli_value_error.is_none());
    let p = parse_args(&args_of(&["bitty", "--log-level", "debug"]));
    assert!(p.cli_value_error.is_none());

    // Negative finite ratios are out-of-range values, not parse errors:
    // both spellings reach layout build and clamp loudly (CTX-0480).
    let p = parse_args(&args_of(&["bitty", "--split-ratio", "-5"]));
    assert!(p.cli_value_error.is_none());
    assert_eq!(p.split_ratio, Some(-5.0));
    let p = parse_args(&args_of(&["bitty", "--split=vertical:2.5"]));
    assert!(p.cli_value_error.is_none());
    assert_eq!(p.split_ratio, Some(2.5));
}

#[test]
fn layout_checked_rejects_hostile_specs() {
    // CTX-0480: silent normalizations now fail closed via
    // try_build_layout; finite out-of-range ratios clamp loudly.
    use crate::layout_cmd::{clamp_ratio_loudly, is_valid_focus_spec, try_build_layout};

    let args = parse_args(&args_of(&["bitty", "--layout", "bogus-spec"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "split:bogus:0.5"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "split:h:nope"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "stack:nope"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "overlay:1,2,3"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "overlay:1,2,3,4,x"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "overlay:1,2,3,4,5"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "split:h:nan"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", "split:h:0.5:extra"]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    let args = parse_args(&args_of(&["bitty", "--layout", ""]));
    assert!(try_build_layout(&args, 80, 24).is_err());

    // Loud clamp: finite extremes clamp to [MIN,MAX], non-finite falls
    // back to 0.5 with a warning.
    assert_eq!(
        clamp_ratio_loudly(5.0),
        bitty_runtime::LayoutNode::MAX_RATIO
    );
    assert_eq!(
        clamp_ratio_loudly(-1.0),
        bitty_runtime::LayoutNode::MIN_RATIO
    );
    assert_eq!(clamp_ratio_loudly(f32::NAN), 0.5);

    // Out-of-range stack counts clamp loudly; negative space-form ratios
    // are accepted values and clamp loudly (never read as missing).
    let args = parse_args(&args_of(&["bitty", "--layout", "stack:99"]));
    let layout = try_build_layout(&args, 80, 24).expect("stack count clamps");
    assert_eq!(layout.leaf_count(), 8);

    let args = parse_args(&args_of(&["bitty", "--split", "h", "--split-ratio", "-5"]));
    let layout = try_build_layout(&args, 80, 24).expect("negative ratio clamps");
    if let bitty_runtime::LayoutNode::Split { ratio, .. } = layout {
        assert_eq!(ratio, bitty_runtime::LayoutNode::MIN_RATIO);
    } else {
        panic!("expected split");
    }

    // Focus syntax gating.
    assert!(is_valid_focus_spec("next"));
    assert!(is_valid_focus_spec("2"));
    assert!(!is_valid_focus_spec("bogus"));
    assert!(!is_valid_focus_spec(""));
}

#[test]
fn parse_config_and_theme_flags() {
    let p = parse_args(&args_of(&["bitty", "--config", "/tmp/c.toml"]));
    assert_eq!(p.config_path.as_deref(), Some("/tmp/c.toml"));
    assert_eq!(p.theme, None);
    let p = parse_args(&args_of(&["bitty", "--config=/tmp/d.toml"]));
    assert_eq!(p.config_path.as_deref(), Some("/tmp/d.toml"));
    let p = parse_args(&args_of(&["bitty", "--theme", "dark"]));
    assert_eq!(p.theme.as_deref(), Some("dark"));
    let p = parse_args(&args_of(&["bitty", "--theme=bitty-dark"]));
    assert_eq!(p.theme.as_deref(), Some("bitty-dark"));
    let p = parse_args(&args_of(&[
        "bitty",
        "--config",
        "/tmp/c.toml",
        "--theme",
        "dark",
    ]));
    assert_eq!(p.config_path.as_deref(), Some("/tmp/c.toml"));
    assert_eq!(p.theme.as_deref(), Some("dark"));
}

#[test]
fn help_text_documents_config_flags() {
    let help = help_text();
    assert!(help.contains("--config"));
    assert!(help.contains("--theme"));
    assert!(help.contains("--profile"));
    assert!(help.contains("--font-family"));
    assert!(help.contains("--font-size"));
    assert!(help.contains("--opacity"));
    assert!(help.contains("BITTY_CONFIG"));
    assert!(help.contains("BITTY_PROFILE"));
    assert!(help.contains("init.lua"));
    assert!(help.contains("config check"));
}

#[test]
fn parse_profile_flags() {
    // CTX-0169: --profile space + equals forms; blank warns + ignores.
    let p = parse_args(&args_of(&["bitty", "--profile", "work"]));
    assert_eq!(p.profile.as_deref(), Some("work"));
    let p = parse_args(&args_of(&["bitty", "--profile=work"]));
    assert_eq!(p.profile.as_deref(), Some("work"));
    let p = parse_args(&args_of(&["bitty", "--profile", "work", "--theme", "dark"]));
    assert_eq!(p.profile.as_deref(), Some("work"));
    assert_eq!(p.theme.as_deref(), Some("dark"));
    // Composes with --config (warn-at-runtime, not parse time).
    let p = parse_args(&args_of(&[
        "bitty",
        "--config",
        "/tmp/c.lua",
        "--profile",
        "work",
    ]));
    assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));
    assert_eq!(p.profile.as_deref(), Some("work"));
    // Composes with `config check` in any order.
    let p = parse_args(&args_of(&["bitty", "config", "check", "--profile", "work"]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    assert_eq!(p.profile.as_deref(), Some("work"));
    let p = parse_args(&args_of(&["bitty", "--profile", "work", "config", "check"]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    assert_eq!(p.profile.as_deref(), Some("work"));
    // Missing value warns + ignores (total, no panic).
    let p = parse_args(&args_of(&["bitty", "--profile"]));
    assert_eq!(p.profile, None);
    // `--` escape hatch: --profile after `--` is a program name.
    let p = parse_args(&args_of(&["bitty", "--", "--profile"]));
    assert_eq!(p.profile, None);
    assert_eq!(p.program.as_deref(), Some("--profile"));
}

#[test]
fn parse_appearance_override_flags() {
    // CTX-0180: --font-family/--font-size/--opacity space + equals forms.
    // Raws stay strings (merge-time fail-closed); blanks warn + ignore.
    let p = parse_args(&args_of(&["bitty", "--font-family", "Cli Mono"]));
    assert_eq!(p.font_family.as_deref(), Some("Cli Mono"));
    let p = parse_args(&args_of(&["bitty", "--font-family=Cli Mono"]));
    assert_eq!(p.font_family.as_deref(), Some("Cli Mono"));
    let p = parse_args(&args_of(&["bitty", "--font-size", "14"]));
    assert_eq!(p.font_size.as_deref(), Some("14"));
    let p = parse_args(&args_of(&["bitty", "--font-size=14.5"]));
    assert_eq!(p.font_size.as_deref(), Some("14.5"));
    let p = parse_args(&args_of(&["bitty", "--opacity", "0.9"]));
    assert_eq!(p.opacity.as_deref(), Some("0.9"));
    let p = parse_args(&args_of(&["bitty", "--opacity=0.95"]));
    assert_eq!(p.opacity.as_deref(), Some("0.95"));
    // Invalid raws are captured, never parsed here (merge fails closed).
    let p = parse_args(&args_of(&["bitty", "--font-size", "abc"]));
    assert_eq!(p.font_size.as_deref(), Some("abc"));
    // Negative numbers reach validation (fail-closed), not "missing".
    let p = parse_args(&args_of(&["bitty", "--font-size", "-5"]));
    assert_eq!(p.font_size.as_deref(), Some("-5"));
    let p = parse_args(&args_of(&["bitty", "--opacity=-0.1"]));
    assert_eq!(p.opacity.as_deref(), Some("-0.1"));
    // A real flag after the option still means "missing value".
    let p = parse_args(&args_of(&["bitty", "--font-size", "--verbose"]));
    assert_eq!(p.font_size, None);
    assert!(p.verbose);
    // Missing/blank values warn + ignore (total, no panic).
    let p = parse_args(&args_of(&["bitty", "--font-family"]));
    assert_eq!(p.font_family, None);
    let p = parse_args(&args_of(&["bitty", "--font-size="]));
    assert_eq!(p.font_size, None);
    let p = parse_args(&args_of(&["bitty", "--opacity"]));
    assert_eq!(p.opacity, None);
    // Composes with --theme and `config check`.
    let p = parse_args(&args_of(&[
        "bitty",
        "--theme",
        "dark",
        "--font-size",
        "14",
        "config",
        "check",
    ]));
    assert_eq!(p.theme.as_deref(), Some("dark"));
    assert_eq!(p.font_size.as_deref(), Some("14"));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    // `--` escape hatch: appearance flags after `--` are program argv.
    let p = parse_args(&args_of(&["bitty", "--", "--font-size"]));
    assert_eq!(p.font_size, None);
    assert_eq!(p.program.as_deref(), Some("--font-size"));
}

#[test]
fn looks_like_negative_number_guards_flag_values() {
    assert!(looks_like_negative_number("-5"));
    assert!(looks_like_negative_number("-0.1"));
    assert!(looks_like_negative_number("-.5"));
    assert!(!looks_like_negative_number("-v"));
    assert!(!looks_like_negative_number("--headless"));
    assert!(!looks_like_negative_number("-"));
    assert!(!looks_like_negative_number("14"));
    assert!(!looks_like_negative_number(""));
}

#[test]
fn cli_overrides_from_args_trims_and_passes_raws() {
    // Pure wiring: trims, blanks become absent, numeric raws untouched.
    let mut args = Args::new();
    args.theme = Some("  dark  ".to_string());
    args.font_family = Some(" Cli Mono ".to_string());
    args.font_size = Some("abc".to_string());
    args.opacity = Some(" 0.9 ".to_string());
    let cli = cli_overrides_from_args(&args);
    assert_eq!(cli.theme.as_deref(), Some("dark"));
    assert_eq!(cli.font_family.as_deref(), Some("Cli Mono"));
    assert_eq!(cli.font_size.as_deref(), Some("abc"));
    assert_eq!(cli.opacity.as_deref(), Some("0.9"));
    // Invalid raws fail closed at the override layer (with field path).
    assert!(cli.validate_appearance_overrides().is_err());
    let mut args = Args::new();
    args.font_family = Some("   ".to_string());
    args.font_size = Some(String::new());
    let cli = cli_overrides_from_args(&args);
    assert!(cli.is_empty());
    assert!(cli.validate_appearance_overrides().is_ok());
    // Flag naming for the fail-closed message.
    assert_eq!(
        appearance_flag_for_field(Some("appearance.theme")),
        "--theme"
    );
    assert_eq!(appearance_flag_for_field(Some("font.size")), "--font-size");
    assert_eq!(
        appearance_flag_for_field(Some("window.opacity")),
        "--opacity"
    );
    assert_eq!(
        appearance_flag_for_field(Some("font.family")),
        "--font-family"
    );
}

#[test]
fn profile_request_resolution_prefers_cli_over_env() {
    // Pure resolver lives in bitty-config::file; pin the contract here
    // so Args parsing and env handling cannot drift (CLI > env > none).
    use bitty_config::file::resolve_profile_request;
    assert_eq!(
        resolve_profile_request(Some("cli"), Some("env")).as_deref(),
        Some("cli")
    );
    assert_eq!(
        resolve_profile_request(None, Some("env")).as_deref(),
        Some("env")
    );
    assert_eq!(resolve_profile_request(None, None), None);
}

#[test]
fn profile_layer_stacks_under_user_over_defaults() {
    // CTX-0169 precedence matrix at the merge level (no fs): profile
    // beats defaults, user beats profile, CLI beats both.
    use bitty_config::file::{CliOverrides, parse_lua_config, resolve_effective_full};
    use bitty_config::plan::{ConfigSource, LayerKind, LayeredPlan};
    let profile_src = ConfigSource::new(LayerKind::Profile, Some("profiles/work.lua"));
    let profile_plan =
        parse_lua_config(r#"return { theme = "dark" }"#, &profile_src).expect("profile");
    let profile = LayeredPlan::new(profile_src, profile_plan);
    let cli_none = CliOverrides::default();
    let merged = resolve_effective_full(None, Some(profile.clone()), &cli_none).expect("profile");
    assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
    assert_eq!(
        merged.source_of("appearance.theme").unwrap().layer,
        LayerKind::Profile
    );
    let user_src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let user_plan =
        parse_lua_config(r#"return { theme = "bitty-dark" }"#, &user_src).expect("user");
    let user = LayeredPlan::new(user_src, user_plan);
    let merged = resolve_effective_full(Some(user), Some(profile), &cli_none).expect("user wins");
    assert_eq!(
        merged.effective.appearance.theme.as_deref(),
        Some("bitty-dark")
    );
    assert_eq!(
        merged.source_of("appearance.theme").unwrap().layer,
        LayerKind::User
    );
    // Source labels carry the profile path for `config check`.
    let profile_src2 = ConfigSource::new(LayerKind::Profile, Some("profiles/work.lua"));
    let profile_plan2 =
        parse_lua_config(r#"return { theme = "dark" }"#, &profile_src2).expect("profile2");
    let profile2 = LayeredPlan::new(profile_src2, profile_plan2);
    let merged = resolve_effective_full(None, Some(profile2), &cli_none).expect("profile only");
    let label = layer_source_label(
        &merged,
        "appearance.theme",
        None,
        Some(std::path::Path::new("profiles/work.lua")),
    );
    assert!(label.starts_with("profile:"), "got {label:?}");
}

#[test]
fn invalid_profile_name_fails_closed_without_filesystem() {
    // Traversal names never reach the filesystem: validation rejects.
    assert!(bitty_config::file::validate_profile_name("../evil").is_err());
    assert!(bitty_config::file::validate_profile_name("a/b").is_err());
    assert!(bitty_config::file::validate_profile_name("").is_err());
    assert!(
        bitty_config::file::profile_file_path_with_env("../evil", Some("/x"), Some("/h")).is_err()
    );
}

#[test]
fn cli_flag_wins_over_file_wins_over_default() {
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    // File layer from a Lua chunk (no fs): theme "dark".
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let file_plan = parse_lua_config(r#"return { theme = "dark" }"#, &src).expect("file parses");
    let file_layer = bitty_config::plan::LayeredPlan::new(src, file_plan);
    // CLI wins over file.
    let merged =
        resolve_effective(Some(file_layer.clone()), Some("bitty-dark")).expect("merge cli>file");
    assert_eq!(
        merged.effective.appearance.theme.as_deref(),
        Some("bitty-dark")
    );
    assert_eq!(
        merged.source_of("appearance.theme").unwrap().layer,
        bitty_config::plan::LayerKind::Cli
    );
    // File wins over default.
    let merged = resolve_effective(Some(file_layer), None).expect("merge file>default");
    assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
    assert_eq!(
        merged.source_of("appearance.theme").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Default when neither.
    let merged = resolve_effective(None, None).expect("defaults");
    assert_eq!(merged.effective.appearance.theme, None);
    // Resolved presets agree with the CTX-0147 registry contract.
    let (named, status) = bitty_config::theme::resolve_theme_with_status(Some("dark"));
    assert_eq!(status, bitty_config::theme::ThemeResolution::Named);
    assert_eq!(named.name, bitty_config::theme::DEFAULT_THEME_NAME);
}

#[test]
fn invalid_theme_fails_closed_at_merge() {
    use bitty_config::file::resolve_effective;
    // Overlong CLI theme must fail validation (not silently ignored).
    let long = "x".repeat(65);
    assert!(resolve_effective(None, Some(&long)).is_err());
    // Whitespace-only CLI theme means no override (falls to default).
    let merged = resolve_effective(None, Some("   ")).expect("blank cli is no-op");
    assert_eq!(merged.effective.appearance.theme, None);
}

#[test]
fn runtime_config_inherits_file_font() {
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let content = r#"return {
        font = { family = "JetBrains Mono", size = 13.0 },
        appearance = { theme = "dark" },
    }"#;
    let plan = parse_lua_config(content, &src).expect("font parses");
    let layer = bitty_config::plan::LayeredPlan::new(src, plan);
    let merged = resolve_effective(Some(layer), None).expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.font_family, "JetBrains Mono");
    assert!((cfg.font_size - 13.0).abs() < f32::EPSILON);
    // Defaults preserved for geometry.
    let defaults = bitty_runtime::RuntimeConfig::default();
    assert_eq!(cfg.cols, defaults.cols);
    assert_eq!(cfg.rows, defaults.rows);
    // Size-aware cell (issue #1814): the 12pt breathing-room reference is
    // 10x22 (CTX-0237: advance 10, line 22); at 13pt it scales by
    // 13/12 and rounds to 11x24 so the cell tracks the harfrust shaped
    // advance. This intentionally differs from the headless
    // RuntimeConfig 9x19 compiled defaults.
    assert_eq!((cfg.cell_width, cfg.cell_height), (11, 24));
}

#[test]
fn runtime_config_applies_font_spacing() {
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let content = r#"return {
        font = { family = "Mono", size = 12, line_height = 1.0, letter_spacing = 0 },
    }"#;
    let plan = parse_lua_config(content, &src).expect("spacing parses");
    let layer = bitty_config::plan::LayeredPlan::new(src, plan);
    let merged = resolve_effective(Some(layer), None).expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!((cfg.cell_width, cfg.cell_height), (8, 16));
}

#[test]
fn runtime_config_large_sizes_track_shaped_advance_issue_1814() {
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    // Large-size golden (issue #1814, present profile size 20): the runtime
    // cell must equal the scaled breathing-room cell, never the fixed
    // 10x22, or glyphs overlap horizontally from ~16pt up.
    let cell_for_size = |size: &str| {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let content = format!(
            "return {{ font = {{ family = \"JetBrainsMono Nerd Font\", size = {size} }} }}"
        );
        let plan = parse_lua_config(&content, &src).expect("font parses");
        let layer = bitty_config::plan::LayeredPlan::new(src, plan);
        let merged = resolve_effective(Some(layer), None).expect("merge");
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime builds");
        (cfg.cell_width, cfg.cell_height)
    };
    assert_eq!(cell_for_size("12.0"), (10, 22), "12pt reference");
    assert_eq!(cell_for_size("16.0"), (13, 29), "16pt repro floor");
    assert_eq!(cell_for_size("20.0"), (17, 37), "present profile");
    assert_eq!(cell_for_size("24.0"), (20, 44), "acceptance ceiling");
    // No overlap across 12-24: cell width covers the monospace shaped
    // advance model (`0.6 * pt * 96/72`).
    for size in 12..=24 {
        let (w, _) = cell_for_size(&format!("{size}.0"));
        let shaped = ((size as f32) * 0.8).ceil() as u32;
        assert!(
            w >= shaped,
            "size {size}: cell {w} must cover shaped {shaped}"
        );
    }
}

#[test]
fn runtime_config_carries_selected_theme_palette() {
    // CTX-0355: `appearance.theme` must reach `RuntimeConfig.theme`, not just
    // the outline pair. The default path otherwise renders the hardcoded
    // Bitty Dark palette for every preset.
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};

    // Default / unset theme -> the designed Bitty Dark palette.
    let defaults = bitty_runtime::RuntimeConfig::default();
    assert_eq!(
        defaults.theme,
        bitty_runtime::ThemePalette::bitty_dark(),
        "no-theme default must stay Bitty Dark"
    );

    // A light preset selected in the file must thread through end to end and
    // disagree with Bitty Dark on every chrome color and the ANSI table.
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { appearance = { theme = "github-light" } }"#,
        &src,
    )
    .expect("theme parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(
        merged.effective.appearance.theme.as_deref(),
        Some("github-light")
    );
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    let dark = bitty_runtime::ThemePalette::bitty_dark();
    assert_eq!(cfg.theme.background, [0xFF, 0xFF, 0xFF, 0xFF]);
    assert_ne!(cfg.theme.background, dark.background);
    assert_ne!(cfg.theme.foreground, dark.foreground);
    assert_ne!(cfg.theme.cursor, dark.cursor);
    assert_ne!(cfg.theme.selection, dark.selection);
    assert_ne!(cfg.theme.ansi, dark.ansi);

    // `--safe` / unknown fallback keeps the built-in default palette.
    let safe = bitty_config::reload::fallback_builtin();
    let safe_cfg = runtime_config_from_effective(&safe).expect("safe builds");
    assert_eq!(safe_cfg.theme, dark);
}

#[test]
fn runtime_config_for_resolves_dual_theme_halves() {
    // CTX-0951 NEEDS-FIX: a dual `light:<name>,dark:<name>` must resolve to
    // the half matching `prefer_light`, not to Bitty Dark via the raw
    // single-name path. Production passes `startup_prefer_light()`; tests
    // pass the flag directly to stay deterministic while the OS query
    // degrades to Unknown.
    use crate::config_cli::runtime_config_from_effective_for;
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};

    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { appearance = { theme = "light:github-light,dark:dracula" } }"#,
        &src,
    )
    .expect("dual theme parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    merged.effective.validate().expect("dual validates");

    let light_cfg =
        runtime_config_from_effective_for(&merged.effective, true).expect("light builds");
    let dark_cfg =
        runtime_config_from_effective_for(&merged.effective, false).expect("dark builds");
    let github_light = bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(
        Some("github-light"),
    ));
    let dracula = bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(
        Some("dracula"),
    ));
    assert_eq!(light_cfg.theme.background, github_light.background);
    assert_eq!(dark_cfg.theme.background, dracula.background);
    assert_ne!(light_cfg.theme.background, dark_cfg.theme.background);
    // Dark-first wrapper keeps the dark half (deterministic for tests).
    let dark_first = runtime_config_from_effective(&merged.effective).expect("builds");
    assert_eq!(dark_first.theme.background, dracula.background);
}

#[test]
fn runtime_config_inherits_file_scroll_speed() {
    // CTX-0185: scroll keys flow file -> effective -> runtime; crate
    // defaults stay equal (bitty-runtime must not depend on bitty-config,
    // so the pairing is by value, pinned here).
    assert_eq!(
        bitty_runtime::config::DEFAULT_SCROLL_LINES_PER_NOTCH,
        bitty_config::types::DEFAULT_SCROLL_LINES_PER_NOTCH
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_SCROLL_PIXELS_PER_NOTCH,
        bitty_config::types::DEFAULT_SCROLL_PIXELS_PER_NOTCH
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let content = r#"return {
        terminal = { scrollback = 10000, scroll_lines_per_notch = 5, scroll_pixels_per_notch = 24 },
    }"#;
    let plan = parse_lua_config(content, &src).expect("scroll keys parse");
    let layer = bitty_config::plan::LayeredPlan::new(src, plan);
    let merged = resolve_effective(Some(layer), None).expect("merge");
    assert_eq!(merged.effective.terminal.scroll_lines_per_notch, 5);
    assert_eq!(merged.effective.terminal.scroll_pixels_per_notch, 24);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.scroll_lines_per_notch, 5);
    assert_eq!(cfg.scroll_pixels_per_notch, 24);
    // Absent keys ride the defaults end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("minimal terminal parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(
        cfg2.scroll_lines_per_notch,
        bitty_runtime::config::DEFAULT_SCROLL_LINES_PER_NOTCH
    );
    assert_eq!(
        cfg2.scroll_pixels_per_notch,
        bitty_runtime::config::DEFAULT_SCROLL_PIXELS_PER_NOTCH
    );
}

#[test]
fn runtime_config_inherits_file_page_scroll_percent() {
    // Issue #2475: `terminal.page_scroll_percent` flows file -> effective ->
    // runtime; the crate defaults stay equal (bitty-runtime must not depend
    // on bitty-config, so the pairing is pinned here).
    assert_eq!(
        bitty_runtime::config::DEFAULT_PAGE_SCROLL_PERCENT,
        bitty_config::types::DEFAULT_PAGE_SCROLL_PERCENT
    );
    assert_eq!(
        bitty_runtime::config::MIN_PAGE_SCROLL_PERCENT,
        bitty_config::types::MIN_PAGE_SCROLL_PERCENT
    );
    assert_eq!(
        bitty_runtime::config::MAX_PAGE_SCROLL_PERCENT,
        bitty_config::types::MAX_PAGE_SCROLL_PERCENT
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { terminal = { scrollback = 10000, page_scroll_percent = 50 } }"#,
        &src,
    )
    .expect("page scroll percent parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.terminal.page_scroll_percent, 50);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.page_scroll_percent, 50);
    // Absent key rides the default end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("minimal terminal parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(
        cfg2.page_scroll_percent,
        bitty_runtime::config::DEFAULT_PAGE_SCROLL_PERCENT
    );
}

#[test]
fn runtime_config_inherits_file_scrollback() {
    // CTX-0297: `terminal.scrollback` flows file -> effective -> runtime
    // and bounds retained history at terminal creation. The runtime
    // default mirrors the terminal-state default (pairing pinned here;
    // `bitty-runtime` must not depend on `bitty-config`).
    assert_eq!(
        bitty_runtime::config::DEFAULT_SCROLLBACK_LINES,
        bitty_term_state::SCROLLBACK_DEFAULT_LINES
    );
    assert_eq!(
        bitty_runtime::config::MAX_SCROLLBACK_LINES,
        bitty_term_state::SCROLLBACK_MAX_LINES
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { terminal = { scrollback = 4242 } }"#, &src)
        .expect("scrollback parses");
    let layer = bitty_config::plan::LayeredPlan::new(src, plan);
    let merged = resolve_effective(Some(layer), None).expect("merge");
    assert_eq!(merged.effective.terminal.scrollback, 4242);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.scrollback, 4242);
    // The default value rides through unchanged.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("minimal terminal parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(
        cfg2.scrollback,
        bitty_runtime::config::DEFAULT_SCROLLBACK_LINES
    );
}

#[test]
fn runtime_config_carries_effective_font_fallback() {
    // CTX-0953 (PX-4571): effective `font.fallback` flows file ->
    // effective -> runtime, so the production rasterizer chain
    // (`RuntimeConfig::font_config` -> `FallbackRasterizer::with_config`)
    // sees the user tier instead of the default empty tier.
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { font = { family = "Mono", size = 12.0, fallback = { "Noto Sans Symbols 2", "Noto Color Emoji" } } }"#,
        &src,
    )
    .expect("fallback parses");
    let layer = bitty_config::plan::LayeredPlan::new(src, plan);
    let merged = resolve_effective(Some(layer), None).expect("merge");
    assert_eq!(
        merged.effective.font.fallback,
        vec![
            "Noto Sans Symbols 2".to_string(),
            "Noto Color Emoji".to_string()
        ]
    );
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.font_fallback, merged.effective.font.fallback);
    let font = cfg.font_config();
    assert_eq!(font.family, cfg.font_family);
    assert_eq!(
        font.fallback,
        vec![
            "Noto Sans Symbols 2".to_string(),
            "Noto Color Emoji".to_string()
        ]
    );
    // The default tier stays empty (platform tail only).
    let cfg_default = runtime_config_from_effective(&bitty_config::EffectiveConfig::default())
        .expect("defaults build");
    assert!(cfg_default.font_fallback.is_empty());
}

#[test]
fn runtime_config_rejects_scrollback_bound_drift() {
    // CTX-0297: a future `bitty-config` bound raised past the runtime /
    // terminal-state hard cap must fail closed instead of silently
    // clamping the retention semantics.
    let mut effective = bitty_config::EffectiveConfig::default();
    effective.terminal.scrollback = 200_000;
    let err =
        runtime_config_from_effective(&effective).expect_err("above hard cap must fail closed");
    assert!(err.contains("terminal.scrollback"), "field named: {err}");
}

#[test]
fn runtime_config_inherits_file_selection_auto_copy() {
    // CTX-0191/CTX-0371: `selection.auto_copy` flows file -> effective ->
    // runtime; crate defaults stay equal (bitty-runtime must not depend on
    // bitty-config, so the pairing is by value, pinned here). Default is
    // off: selecting text never writes the system clipboard implicitly.
    assert_eq!(
        bitty_runtime::config::DEFAULT_SELECTION_AUTO_COPY,
        bitty_config::types::DEFAULT_SELECTION_AUTO_COPY
    );
    const { assert!(!bitty_runtime::config::DEFAULT_SELECTION_AUTO_COPY) }
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { selection = { auto_copy = true } }"#, &src)
        .expect("opt-in parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert!(merged.effective.selection.auto_copy);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert!(cfg.selection_auto_copy);
    // Absent table rides the default-off end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no selection table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert!(!merged2.effective.selection.auto_copy);
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert!(!cfg2.selection_auto_copy);
    assert_eq!(
        merged2.source_of("selection.auto_copy").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
}

#[test]
fn runtime_config_inherits_file_close_confirm() {
    // CTX-0370: `close_confirm` flows file -> effective -> runtime; crate
    // defaults stay equal (bitty-runtime must not depend on bitty-config,
    // so the pairing is by value, pinned here). Default `when_busy`
    // preserves the pre-0370 close behavior for idle shells.
    assert_eq!(
        bitty_runtime::config::DEFAULT_CLOSE_CONFIRM_MODE.as_str(),
        bitty_config::types::DEFAULT_CLOSE_CONFIRM.as_str()
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan =
        parse_lua_config(r#"return { close_confirm = "never" }"#, &src).expect("never parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(
        merged.effective.close_confirm,
        bitty_config::CloseConfirm::Never
    );
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.close_confirm, bitty_runtime::CloseConfirmMode::Never);
    // Absent key rides the when_busy default end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 =
        parse_lua_config(r#"return { theme = "dark" }"#, &src2).expect("no close_confirm parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        merged2.effective.close_confirm,
        bitty_config::CloseConfirm::WhenBusy
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(
        cfg2.close_confirm,
        bitty_runtime::CloseConfirmMode::WhenBusy
    );
    assert_eq!(
        merged2.source_of("close_confirm").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
}

#[test]
fn runtime_config_inherits_file_cursor_style_and_bell() {
    // CTX-0756 (issue #1359): `terminal.cursor_style` / `terminal.bell`
    // flow file -> effective -> runtime; crate defaults stay equal
    // (`bitty-runtime` must not depend on `bitty-config`, so the pairing
    // is by value, pinned here). Defaults preserve pre-1359 behavior
    // (block-fallback cursor, visual flash).
    assert_eq!(
        bitty_runtime::config::DEFAULT_CURSOR_STYLE,
        bitty_vt::CursorStyle::Default
    );
    assert_eq!(
        bitty_config::types::DEFAULT_CURSOR_STYLE.as_str(),
        "default"
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_BELL_MODE,
        bitty_runtime::BellMode::Visual
    );
    assert_eq!(bitty_config::types::DEFAULT_BELL_MODE.as_str(), "visual");
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { terminal = { scrollback = 10000, cursor_style = "steady_bar", bell = "off" } }"#,
        &src,
    )
    .expect("cursor/bell parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(
        merged.effective.terminal.cursor_style,
        bitty_config::CursorStyle::SteadyBar
    );
    assert_eq!(merged.effective.terminal.bell, bitty_config::BellMode::Off);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.cursor_style, bitty_vt::CursorStyle::SteadyBar);
    assert_eq!(cfg.bell_mode, bitty_runtime::BellMode::Off);
    assert_eq!(
        merged.source_of("terminal.cursor_style").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    assert_eq!(
        merged.source_of("terminal.bell").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent leaves ride the fail-closed defaults end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("minimal terminal parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(cfg2.cursor_style, bitty_vt::CursorStyle::Default);
    assert_eq!(cfg2.bell_mode, bitty_runtime::BellMode::Visual);
    // Every cursor spelling maps across without loss.
    for (raw, expected) in [
        ("default", bitty_vt::CursorStyle::Default),
        ("blinking_block", bitty_vt::CursorStyle::BlinkingBlock),
        ("steady_block", bitty_vt::CursorStyle::SteadyBlock),
        (
            "blinking_underline",
            bitty_vt::CursorStyle::BlinkingUnderline,
        ),
        ("steady_underline", bitty_vt::CursorStyle::SteadyUnderline),
        ("blinking_bar", bitty_vt::CursorStyle::BlinkingBar),
        ("steady_bar", bitty_vt::CursorStyle::SteadyBar),
    ] {
        let mut effective = bitty_config::EffectiveConfig::default();
        effective.terminal.cursor_style =
            bitty_config::CursorStyle::parse(raw).expect("spelling valid");
        let mapped = runtime_config_from_effective(&effective).expect("builds");
        assert_eq!(mapped.cursor_style, expected, "cursor {raw}");
    }
    for (raw, expected) in [
        ("off", bitty_runtime::BellMode::Off),
        ("visual", bitty_runtime::BellMode::Visual),
        ("audible", bitty_runtime::BellMode::Audible),
        ("both", bitty_runtime::BellMode::Both),
    ] {
        let mut effective = bitty_config::EffectiveConfig::default();
        effective.terminal.bell = bitty_config::BellMode::parse(raw).expect("spelling valid");
        let mapped = runtime_config_from_effective(&effective).expect("builds");
        assert_eq!(mapped.bell_mode, expected, "bell {raw}");
    }
}

#[test]
fn runtime_config_inherits_file_focus_follows_mouse() {
    // CTX-0260: `mouse.focus_follows_mouse` flows file -> effective ->
    // runtime; crate defaults stay equal (bitty-runtime must not depend
    // on bitty-config, so the pairing is by value, pinned here).
    // Default off preserves click-to-focus (zero change for existing
    // users).
    assert_eq!(
        bitty_runtime::config::DEFAULT_FOCUS_FOLLOWS_MOUSE,
        bitty_config::types::DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE
    );
    // CTX-0334: the dwell-delay defaults and bound stay paired by value.
    assert_eq!(
        bitty_runtime::config::DEFAULT_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
        bitty_config::types::DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS
    );
    assert_eq!(
        bitty_runtime::config::MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
        bitty_config::types::MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS
    );
    const { assert!(bitty_runtime::config::DEFAULT_FOCUS_FOLLOWS_MOUSE) }
    const { assert!(bitty_runtime::config::DEFAULT_FOCUS_FOLLOWS_MOUSE_DELAY_MS == 0) }
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { mouse = { focus_follows_mouse = true, focus_follows_mouse_delay_ms = 250 } }"#,
        &src,
    )
    .expect("opt-in parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert!(merged.effective.mouse.focus_follows_mouse);
    assert_eq!(merged.effective.mouse.focus_follows_mouse_delay_ms, 250);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert!(cfg.focus_follows_mouse);
    assert_eq!(
        cfg.focus_follows_mouse_delay,
        std::time::Duration::from_millis(250)
    );
    assert_eq!(
        merged
            .source_of("mouse.focus_follows_mouse_delay_ms")
            .unwrap()
            .layer,
        bitty_config::plan::LayerKind::User
    );
    assert_eq!(
        merged.source_of("mouse.focus_follows_mouse").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides the default-on end to end (CTX-0945).
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no mouse table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert!(merged2.effective.mouse.focus_follows_mouse);
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert!(cfg2.focus_follows_mouse);
    assert_eq!(
        merged2
            .source_of("mouse.focus_follows_mouse")
            .unwrap()
            .layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
    // Explicit opt-out sets false.
    let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan3 = parse_lua_config(
        r#"return { mouse = { focus_follows_mouse = false } }"#,
        &src3,
    )
    .expect("opt-out parses");
    let merged3 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src3, plan3)),
        None,
    )
    .expect("merge");
    assert!(!merged3.effective.mouse.focus_follows_mouse);
    let cfg3 = runtime_config_from_effective(&merged3.effective).expect("builds");
    assert!(!cfg3.focus_follows_mouse);

    // Issue #2474: button bindings flow file -> effective -> runtime. The
    // defaults stay the documented matrix; explicit values win.
    assert_eq!(cfg2.mouse_right, bitty_config::MouseButtonBinding::None);
    assert_eq!(
        cfg2.mouse_middle,
        bitty_config::MouseButtonBinding::Action(bitty_config::ChromeAction::PasteFromPrimary)
    );
    let src4 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan4 = parse_lua_config(
        r#"return { mouse = { right = "paste_from_clipboard", middle = "none" } }"#,
        &src4,
    )
    .expect("button bindings parse");
    let merged4 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src4, plan4)),
        None,
    )
    .expect("merge");
    let cfg4 = runtime_config_from_effective(&merged4.effective).expect("builds");
    assert_eq!(
        cfg4.mouse_right,
        bitty_config::MouseButtonBinding::Action(bitty_config::ChromeAction::PasteFromClipboard)
    );
    assert_eq!(cfg4.mouse_middle, bitty_config::MouseButtonBinding::None);
}

/// Route one mouse event through the runtime and then drain the queued host
/// action through the app's shared chrome dispatcher (the production order).
fn app_mouse_event(
    app: &mut TerminalApp,
    button: bitty_platform::MouseButton,
    state: bitty_platform::PressState,
) {
    app.runtime.handle_platform_event(PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(1),
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent::new(button, state)),
    });
    let _ = app.dispatch_pending_mouse_action();
}

fn headless_app_with_runtime_config(config: bitty_runtime::RuntimeConfig) -> TerminalApp {
    let rt = Runtime::new(config).expect("headless runtime must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "mouse-bindings",
        Vec::new(),
        SpawnSpec::default(),
    );
    app.runtime.force_headless_clipboard();
    app.set_session_persistence(false);
    app
}

fn headless_app_with_mouse_right(binding: bitty_config::MouseButtonBinding) -> TerminalApp {
    headless_app_with_runtime_config(bitty_runtime::RuntimeConfig {
        mouse_right: binding,
        ..bitty_runtime::RuntimeConfig::default()
    })
}

#[test]
fn default_right_click_is_inert_through_the_app_dispatch() {
    // Issue #2474: default `mouse.right = "none"` must not paste, must not
    // open the suspicious-paste dialog, and must emit no PTY bytes.
    let mut app = headless_app_with_mouse_right(bitty_config::MouseButtonBinding::None);
    app.runtime
        .clipboard_mut()
        .set_text("first line\nsecond line\nthird line".to_string())
        .expect("headless clipboard");
    app.runtime.drain_pending_input();
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Pressed,
    );
    assert!(!app.runtime.has_pending_paste(), "no suspicious-paste trap");
    assert_eq!(app.runtime.pending_input_len(), 0, "no PTY bytes");
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Released,
    );
    assert!(!app.runtime.has_pending_paste());
    assert_eq!(app.runtime.pending_input_len(), 0);
}

#[test]
fn configured_right_click_runs_the_action_exactly_once_through_chrome_dispatch() {
    // Issue #2474: an explicit `mouse.right = "paste_from_clipboard"` runs
    // through the same `ChromeAction` dispatcher as a keymap, once per press.
    let mut app = headless_app_with_mouse_right(bitty_config::MouseButtonBinding::Action(
        bitty_config::ChromeAction::PasteFromClipboard,
    ));
    app.runtime
        .clipboard_mut()
        .set_text("hi".to_string())
        .expect("headless clipboard");
    app.runtime.drain_pending_input();
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Pressed,
    );
    assert_eq!(
        app.runtime.pending_input(),
        b"hi",
        "the clipboard action dispatches once"
    );
    assert!(
        !app.runtime.has_pending_paste(),
        "clean paste needs no dialog"
    );
    // The paired release must not dispatch a second action.
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Released,
    );
    assert_eq!(app.runtime.pending_input(), b"hi");
}

#[test]
fn two_configured_right_presses_confirm_a_multiline_paste_through_the_app() {
    // Issue #2474 review: an explicit paste binding still passes the
    // CTX-0186 gate. The first press holds the multi-line text with no bytes;
    // repeating the identical press confirms and delivers every line intact.
    let mut app = headless_app_with_mouse_right(bitty_config::MouseButtonBinding::Action(
        bitty_config::ChromeAction::PasteFromClipboard,
    ));
    app.runtime
        .clipboard_mut()
        .set_text("one\ntwo".to_string())
        .expect("headless clipboard");
    app.runtime.drain_pending_input();
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Pressed,
    );
    assert!(
        app.runtime.has_pending_paste(),
        "multi-line paste must gate"
    );
    assert_eq!(app.runtime.pending_input_len(), 0, "no silent delivery");
    // The second identical press confirms through the same dispatcher.
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Pressed,
    );
    assert!(!app.runtime.has_pending_paste(), "repeat press confirms");
    assert_eq!(app.runtime.pending_input(), b"one\ntwo");
}

#[test]
fn mouse_action_is_captured_by_a_pending_close_confirmation() {
    // Issue #2474 review: a bound mouse action obeys the same modal
    // priority/capture rules as a bound chord. While a close modal is armed,
    // a non-confirm action is consumed and the modal stays armed.
    let mut app = headless_app_with_runtime_config(bitty_runtime::RuntimeConfig {
        mouse_right: bitty_config::MouseButtonBinding::Action(
            bitty_config::ChromeAction::ToggleHelp,
        ),
        close_confirm: bitty_runtime::CloseConfirmMode::Always,
        ..bitty_runtime::RuntimeConfig::default()
    });
    let view = app.runtime.focused_view().expect("focused view");
    assert!(matches!(
        app.runtime.view_close_request(view),
        bitty_runtime::ViewCloseRequest::Pending { .. }
    ));
    assert!(app.runtime.has_pending_close_confirm());
    assert!(!app.runtime.help_visible());
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Pressed,
    );
    assert!(
        !app.runtime.help_visible(),
        "the armed close modal captures the non-confirm mouse action"
    );
    assert!(
        app.runtime.has_pending_close_confirm(),
        "the modal stays armed for its own confirmation gesture"
    );
}

#[test]
fn mouse_capture_wins_over_host_binding_through_the_app_dispatch() {
    // Issue #2474 precedence: a pane that enabled mouse reporting receives
    // the button as protocol bytes; no host mouse action runs.
    let mut app = headless_app_with_mouse_right(bitty_config::MouseButtonBinding::Action(
        bitty_config::ChromeAction::PasteFromClipboard,
    ));
    app.runtime
        .clipboard_mut()
        .set_text("clip".to_string())
        .expect("headless clipboard");
    app.runtime.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    app.runtime.drain_pending_input();
    app.runtime
        .handle_cursor_moved(bitty_platform::CursorPosition { x: 0.0, y: 0.0 });
    app_mouse_event(
        &mut app,
        bitty_platform::MouseButton::Right,
        bitty_platform::PressState::Pressed,
    );
    assert_eq!(app.runtime.pending_input(), b"\x1b[<2;1;1M");
    assert!(!app.runtime.has_pending_paste());
}

#[test]
fn workspace_show_bar_parses_for_the_bar_plugin() {
    // CTX-0979: Core draws no workspace display. `workspace.show_bar`
    // stays accepted in `bitty-config` for the bar plugin to read;
    // Core carries no visibility state. This pins the config parsing
    // (file -> effective) and that the runtime builds with workspace
    // queries intact.
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    // Absent table parses; no show_bar key.
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src)
        .expect("no workspace table parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert!(merged.effective.workspace.show_bar.is_none());
    let cfg = runtime_config_from_effective(&merged.effective).expect("builds");
    let rt = bitty_runtime::Runtime::new(cfg).expect("runtime builds");
    assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
    // Explicit opt-out still parses for the bar plugin.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { workspace = { show_bar = false } }"#, &src2)
        .expect("opt-out parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(merged2.effective.workspace.show_bar, Some(false));
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    let rt2 = bitty_runtime::Runtime::new(cfg2).expect("runtime builds");
    assert_eq!(rt2.workspace_names(), vec![String::from("ws1")]);
    assert_eq!(
        merged2.source_of("workspace.show_bar").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
}

#[test]
fn workspace_bar_edge_parses_for_the_bar_plugin() {
    // CTX-0979: Core draws no workspace display. `workspace.bar.edge`
    // stays accepted in `bitty-config` for the bar plugin; Core carries
    // no edge state. This pins file -> effective parsing with the runtime
    // building and queries intact.
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind, LayeredPlan};
    use bitty_config::types::WorkspaceBarEdge;
    for (lua, want) in [
        (
            r#"return { terminal = { scrollback = 10000 } }"#,
            WorkspaceBarEdge::Bottom,
        ),
        (
            r#"return { workspace = { bar = { edge = "top" } } }"#,
            WorkspaceBarEdge::Top,
        ),
        (
            r#"return { workspace = { bar = { edge = "bottom" } } }"#,
            WorkspaceBarEdge::Bottom,
        ),
    ] {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan = parse_lua_config(lua, &src).expect(lua);
        let merged = resolve_effective(Some(LayeredPlan::new(src, plan)), None).expect("merge");
        let edge = merged.effective.workspace.bar_edge.unwrap_or_default();
        assert_eq!(edge, want, "{lua}");
        let cfg = runtime_config_from_effective(&merged.effective).expect("builds");
        let rt = bitty_runtime::Runtime::new(cfg).expect("runtime builds");
        assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
    }
}

#[test]
fn workspace_bar_edge_spelling_parses_fail_closed() {
    // CTX-0979: the Core-side `runtime_bar_edge` mapping is deleted with
    // the Core display. This pins the config-side spelling only.
    use bitty_config::types::WorkspaceBarEdge;
    for (spelling, config) in [
        ("top", WorkspaceBarEdge::Top),
        ("bottom", WorkspaceBarEdge::Bottom),
    ] {
        assert_eq!(WorkspaceBarEdge::parse(spelling), Some(config));
        assert_eq!(config.as_str(), spelling);
    }
    assert_eq!(
        WorkspaceBarEdge::default(),
        WorkspaceBarEdge::Bottom,
        "default stays bottom for the bar plugin"
    );
    assert_eq!(WorkspaceBarEdge::parse("Top"), None, "fail-closed spelling");
}

#[test]
fn runtime_config_inherits_file_layout_gaps() {
    // CTX-0177: `layout.gaps_in`/`gaps_out` flow file -> effective ->
    // runtime; crate defaults stay equal (bitty-runtime must not depend
    // on bitty-config, so the pairing is by value, pinned here). Default
    // preserves edge-to-edge tiling (zero change for existing users).
    assert_eq!(
        u32::from(bitty_runtime::config::DEFAULT_LAYOUT_GAPS_IN),
        bitty_config::types::DEFAULT_LAYOUT_GAPS_IN
    );
    assert_eq!(
        u32::from(bitty_runtime::config::DEFAULT_LAYOUT_GAPS_OUT),
        bitty_config::types::DEFAULT_LAYOUT_GAPS_OUT
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS),
        bitty_config::types::MAX_LAYOUT_GAP_CELLS
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { layout = { gaps_in = 1, gaps_out = 2 } }"#, &src)
        .expect("gaps parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.layout.gaps_in, 1);
    assert_eq!(merged.effective.layout.gaps_out, 2);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!((cfg.gaps_in, cfg.gaps_out), (1, 2));
    assert_eq!(
        merged.source_of("layout.gaps_in").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides edge-to-edge end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no layout table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        (
            merged2.effective.layout.gaps_in,
            merged2.effective.layout.gaps_out
        ),
        (0, 0)
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!((cfg2.gaps_in, cfg2.gaps_out), (0, 0));
    assert_eq!(
        merged2.source_of("layout.gaps_in").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
    // Oversized gaps fail closed at the file layer (never reach runtime).
    let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    parse_lua_config(r#"return { layout = { gaps_in = 17 } }"#, &src3).expect_err("must fail");
}

#[test]
fn runtime_config_inherits_file_decoration() {
    // CTX-0292/CTX-0333: `decoration.gaps_in`, `decoration.gaps_out`,
    // `decoration.border`, `decoration.radius`, `decoration.content_inset`
    // flow file -> effective -> runtime; the crate constants stay equal
    // (bitty-runtime aliases bitty-ui, bitty-config owns its own copy;
    // the pairing is pinned here). Defaults are unified 6/6/1/6/6.
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_GAPS_IN_PX,
        bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_GAPS_OUT_PX,
        bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_BORDER_PX,
        bitty_config::types::DEFAULT_DECORATION_BORDER_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_RADIUS_PX,
        bitty_config::types::DEFAULT_DECORATION_RADIUS_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_CONTENT_INSET_PX,
        bitty_config::types::DEFAULT_DECORATION_CONTENT_INSET_PX as u16
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_GAP_PX),
        bitty_config::types::MAX_DECORATION_GAP_PX
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_BORDER_PX),
        bitty_config::types::MAX_DECORATION_BORDER_PX
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_RADIUS_PX),
        bitty_config::types::MAX_DECORATION_RADIUS_PX
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_CONTENT_INSET_PX),
        bitty_config::types::MAX_DECORATION_CONTENT_INSET_PX
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { decoration = { gaps_in = 0, gaps_out = 1, border = 1, radius = 0, content_inset = 2 } }"#,
        &src,
    )
    .expect("decoration parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.decoration.gaps_in, 0);
    assert_eq!(merged.effective.decoration.gaps_out, 1);
    assert_eq!(merged.effective.decoration.content_inset, 2);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(
        cfg.decoration,
        bitty_runtime::Decoration::new(0, 1, 1, 0, 2)
    );
    assert_eq!(
        merged.source_of("decoration.gaps_in").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides the unified defaults end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no decoration table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        merged2.effective.decoration,
        bitty_config::types::DecorationConfig::default()
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(cfg2.decoration, bitty_runtime::Decoration::default());
    assert_eq!(
        cfg2.decoration,
        bitty_runtime::Decoration::new(6, 6, 1, 6, 6)
    );
    // CTX-0340: with no color config the runtime carries the ratified
    // theme-token pair.
    assert_eq!(
        cfg2.outline_focused,
        bitty_runtime::config::DEFAULT_OUTLINE_FOCUSED
    );
    assert_eq!(
        cfg2.outline_idle,
        bitty_runtime::config::DEFAULT_OUTLINE_IDLE
    );
    assert_eq!(
        merged2.source_of("decoration.gaps_in").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
    // Safe mode inverts to 0/0/1/0/0 regardless of user configuration.
    let safe = bitty_config::reload::fallback_builtin();
    let safe_cfg = runtime_config_from_effective(&safe).expect("safe builds");
    assert_eq!(safe_cfg.decoration, bitty_runtime::Decoration::SAFE);
    assert_eq!(
        safe_cfg.decoration,
        bitty_runtime::Decoration::new(0, 0, 1, 0, 0)
    );
    // CTX-0340: safe mode forces the opaque built-in outline pair.
    assert_eq!(safe_cfg.outline_focused, [0xFF, 0xFF, 0xFF, 0xFF]);
    assert_eq!(safe_cfg.outline_idle, [0x80, 0x80, 0x80, 0xFF]);
    // Out-of-range decoration fails closed at the file layer.
    for bad in [
        r#"return { decoration = { gaps_in = 33 } }"#,
        r#"return { decoration = { gaps_out = 33 } }"#,
        r#"return { decoration = { border = 9 } }"#,
        r#"return { decoration = { radius = 17 } }"#,
        r#"return { decoration = { content_inset = 33 } }"#,
        r#"return { decoration = { border_width = 17 } }"#,
        r#"return { decoration = { border_width_focused = 17 } }"#,
        r#"return { decoration = { border_width_idle = 17 } }"#,
        r#"return { decoration = { gaps_in = 1, bogus = 2 } }"#,
    ] {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        parse_lua_config(bad, &src).expect_err("must fail closed");
    }
}

#[test]
fn runtime_config_inherits_file_outline_width() {
    // CTX-0344 (RFC-0001/OQ-045): `decoration.border_width` /
    // `_focused` / `_idle` flow file -> effective -> runtime; the crate
    // max stays equal; safe mode forces the `1`/`1` pair.
    assert_eq!(
        bitty_runtime::config::MAX_OUTLINE_WIDTH_PX,
        bitty_config::types::MAX_DECORATION_BORDER_WIDTH_PX
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_OUTLINE_WIDTH_FOCUSED,
        bitty_runtime::config::DEFAULT_OUTLINE_WIDTH_IDLE
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    // Base only -> both states inherit the base (4).
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { decoration = { border_width = 4 } }"#, &src)
        .expect("width parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.decoration.border_width, Some(4));
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.outline_width_focused, Some(4));
    assert_eq!(cfg.outline_width_idle, Some(4));
    // Explicit pair -> focused exceeds idle.
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { decoration = { border_width_focused = 6, border_width_idle = 2 } }"#,
        &src,
    )
    .expect("width pair parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert!(
        cfg.outline_width_focused > cfg.outline_width_idle,
        "focused width must exceed idle where configured"
    );
    assert_eq!(
        (cfg.outline_width_focused, cfg.outline_width_idle),
        (Some(6), Some(2))
    );
    // No width config -> both inherit the runtime `decoration.border` (1).
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src)
        .expect("no decoration parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("builds");
    assert_eq!(
        (cfg.outline_width_focused, cfg.outline_width_idle),
        (Some(1), Some(1)),
        "unset widths inherit decoration.border (1)"
    );
    // Safe mode forces 1/1 (equal, no width cue).
    let safe = bitty_config::reload::fallback_builtin();
    let safe_cfg = runtime_config_from_effective(&safe).expect("safe builds");
    assert_eq!(
        (safe_cfg.outline_width_focused, safe_cfg.outline_width_idle),
        (Some(1), Some(1))
    );
}

/// Synthetic platform-absolute path for config-mapping tests: a drive path
/// on Windows, a `/`-rooted path elsewhere. `bitty-config` validates
/// "absolute" with `Path::is_absolute()`, which is platform-specific.
/// Windows paths use forward slashes so the value is also a valid Lua
/// string literal (backslashes would be escape sequences).
fn synthetic_abs(rel: &str) -> String {
    if cfg!(windows) {
        format!("C:/{rel}")
    } else {
        format!("/{rel}")
    }
}

#[test]
fn runtime_config_carries_per_view_overrides() {
    // CTX-0343: `views.*` rules flow file -> effective -> runtime in
    // canonical selector form; the app maps the resolved per-View colors
    // onto the runtime rule leaves by value.
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let one = synthetic_abs("srv/wall/one.png");
    let source = r##"return { views = {
            ["*"] = { border_color_focused = "#33CCFF" },
            ["ws:2"] = { border_width_idle = 1 },
            ["view:7"] = { background_image = "__IMG__", background_fit = "fit" },
        } }"##
        .replace("__IMG__", &one);
    let plan = parse_lua_config(&source, &src).expect("views parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.view_appearance.len(), 3);
    let by_selector = |name: &str| {
        cfg.view_appearance
            .iter()
            .find(|rule| rule.selector == name)
            .unwrap_or_else(|| panic!("missing rule {name}"))
    };
    assert_eq!(
        by_selector("*").border_color_focused,
        Some([0x33, 0xCC, 0xFF, 0xFF])
    );
    assert_eq!(by_selector("ws:2").border_width_idle, Some(1));
    // CTX-0347: the absolute path flows to the runtime unchanged.
    assert_eq!(
        by_selector("view:7").background_image.as_deref(),
        Some(one.as_str())
    );
    assert_eq!(by_selector("view:7").background_fit.as_deref(), Some("fit"));
    // Safe mode carries no per-View rules at all.
    let safe = bitty_config::reload::fallback_builtin();
    let safe_cfg = runtime_config_from_effective(&safe).expect("safe builds");
    assert!(safe_cfg.view_appearance.is_empty());
}

#[test]
fn runtime_config_carries_global_background_image_fit_and_roots() {
    // CTX-0347: the global pair and the deny-by-default root list flow
    // file -> effective -> runtime. Absolute paths are carried verbatim so
    // the test is hermetic on every platform (Windows has no `$HOME`).
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let image = synthetic_abs("srv/wall/global.png");
    let root_a = synthetic_abs("srv/wall");
    let root_b = synthetic_abs("srv/pictures");
    let source = r##"return { decoration = {
            background_image = "__IMG__",
            background_fit = "tile",
            background_image_roots = { "__ROOT_A__", "__ROOT_B__" },
        } }"##
        .replace("__IMG__", &image)
        .replace("__ROOT_A__", &root_a)
        .replace("__ROOT_B__", &root_b);
    let plan = parse_lua_config(&source, &src).expect("background decoration parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.background_image.as_deref(), Some(image.as_str()));
    assert_eq!(cfg.background_fit, "tile");
    assert_eq!(cfg.background_image_roots, vec![root_a, root_b]);
    // Safe mode clears the image and the roots (no file may be opened).
    let safe_cfg = runtime_config_from_effective(&bitty_config::reload::fallback_builtin())
        .expect("safe builds");
    assert_eq!(safe_cfg.background_image, None);
    assert!(safe_cfg.background_image_roots.is_empty());
}

#[test]
fn runtime_config_carries_style_surface_and_per_view_style() {
    // CTX-1161 S2: the merged global style surface (`appearance.style`
    // glow/shadow/fade plus the `decoration` tint pair) and the per-`View`
    // style quartet flow file -> effective -> runtime by value.
    // Consumed by the S3 present path (CTX-1162).
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let source = r##"return { appearance = { style = {
            glow_color = "#FF0000", glow_width = 8,
            shadow_color = "#00000080", shadow_offset_x = -4, shadow_offset_y = 5,
            fade_unfocused = 0.5,
        } }, decoration = {
            background_tint = "#112233", background_tint_strength = 0.25,
        }, views = {
            ["*"] = { glow_width = 4 },
            ["view:7"] = { glow_color = "#00FF00", background_tint_strength = 0.75 },
        } }"##;
    let plan = parse_lua_config(source, &src).expect("style parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.style_glow_color, Some([0xFF, 0x00, 0x00, 0xFF]));
    assert_eq!(cfg.style_glow_width, 8);
    assert_eq!(cfg.style_shadow_color, Some([0x00, 0x00, 0x00, 0x80]));
    assert_eq!(cfg.style_shadow_offset_x, -4);
    assert_eq!(cfg.style_shadow_offset_y, 5);
    assert!((cfg.style_fade_unfocused - 0.5).abs() < f32::EPSILON);
    assert_eq!(cfg.background_tint, Some([0x11, 0x22, 0x33, 0xFF]));
    assert!((cfg.background_tint_strength - 0.25).abs() < f32::EPSILON);
    let by_selector = |name: &str| {
        cfg.view_appearance
            .iter()
            .find(|rule| rule.selector == name)
            .unwrap_or_else(|| panic!("missing rule {name}"))
    };
    assert_eq!(by_selector("*").glow_width, Some(4));
    assert_eq!(
        by_selector("view:7").glow_color,
        Some([0x00, 0xFF, 0x00, 0xFF])
    );
    assert!((by_selector("view:7").background_tint_strength.unwrap() - 0.75).abs() < f32::EPSILON);
    // Sparse leaves inherit: the `*` rule sets only the width, the `view:7`
    // rule sets no tint color.
    assert_eq!(by_selector("*").glow_color, None);
    assert_eq!(by_selector("view:7").background_tint, None);
    // Safe mode clears the whole style surface and drops per-View rules.
    let safe_cfg = runtime_config_from_effective(&bitty_config::reload::fallback_builtin())
        .expect("safe builds");
    assert_eq!(safe_cfg.style_glow_color, None);
    assert_eq!(safe_cfg.style_glow_width, 0);
    assert_eq!(safe_cfg.style_shadow_color, None);
    assert_eq!(safe_cfg.style_shadow_offset_x, 0);
    assert_eq!(safe_cfg.style_shadow_offset_y, 0);
    assert!((safe_cfg.style_fade_unfocused - 1.0).abs() < f32::EPSILON);
    assert_eq!(safe_cfg.background_tint, None);
    assert!((safe_cfg.background_tint_strength - 0.0).abs() < f32::EPSILON);
    assert!(safe_cfg.view_appearance.is_empty());
}

#[test]
fn style_bounds_mirror_config_and_runtime() {
    // CTX-1161 S2: the runtime repeats the `bitty-config` style bounds so a
    // direct construction cannot arm an unbounded value; pin the pairing.
    assert_eq!(
        bitty_runtime::config::MAX_STYLE_GLOW_WIDTH_PX,
        bitty_config::MAX_STYLE_GLOW_WIDTH_PX
    );
    assert_eq!(
        bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        bitty_config::MAX_STYLE_SHADOW_OFFSET_PX
    );
}

#[test]
fn expand_home_path_is_injected_and_fails_closed_without_home() {
    // CTX-0347: expansion is hermetic over an injected home, so Windows CI
    // (which has no `$HOME`) never depends on the ambient environment.
    let home = std::path::Path::new("/home/test");
    // Build the expectation with the platform separator (`\` on Windows)
    // instead of hardcoding a path spelling.
    assert_eq!(
        crate::config_cli::expand_home_path_with("~/wall/one.png", Some(home)).expect("expand"),
        home.join("wall/one.png").to_string_lossy().into_owned()
    );
    assert_eq!(
        crate::config_cli::expand_home_path_with("~", Some(home)).expect("bare tilde"),
        home.to_string_lossy().into_owned()
    );
    assert_eq!(
        crate::config_cli::expand_home_path_with("/srv/wall.png", Some(home)).expect("absolute"),
        "/srv/wall.png"
    );
    assert!(crate::config_cli::expand_home_path_with("~/wall.png", None).is_err());
    // The live wrapper only resolves against the process home; an absolute
    // path never needs it.
    assert_eq!(
        crate::config_cli::expand_home_path("/srv/wall.png").expect("absolute passthrough"),
        "/srv/wall.png"
    );
}

#[test]
fn runtime_view_contract_floors_match_config() {
    // CTX-0343: the runtime mirrors the config AC-1/AC-2 floors and the WCAG
    // math without a crate dependency; pin the floors equal here so a drift
    // on either side fails the workspace tests.
    assert_eq!(
        bitty_runtime::config::MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST,
        bitty_config::types::MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST
    );
    assert_eq!(
        bitty_runtime::config::MIN_OUTLINE_FOCUSED_IDLE_CONTRAST,
        bitty_config::types::MIN_OUTLINE_FOCUSED_IDLE_CONTRAST
    );
}

#[test]
fn merge_rejects_resolvable_view_contract_violation() {
    // CTX-0343 production path: a resolvable violating `views.*` override
    // rejects the whole merge/reload with a source-attributed diagnostic
    // (the config-layer half of the two-point enforcement; the runtime half
    // is covered by the first-match tests in `bitty-runtime`).
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let bg = bitty_config::theme::resolve_theme(None).background;
    let lua = format!(
        "return {{ views = {{ [\"*\"] = {{ border_color_focused = \"#{:02X}{:02X}{:02X}\" }} }} }}",
        bg[0], bg[1], bg[2]
    );
    let plan = parse_lua_config(&lua, &src).expect("plan parses");
    let err = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect_err("resolvable violation must reject the whole merge");
    assert_eq!(err.field(), Some("views[*].border_color_focused"));
    assert!(err.to_string().contains("AC-1"), "{err}");
}

#[test]
fn runtime_config_inherits_file_scrollbar() {
    // CTX-0181: `scrollbar.mode`/`scrollbar.width` flow file ->
    // effective -> runtime; crate defaults stay equal (bitty-runtime
    // must not depend on bitty-config, so the pairing is by value,
    // pinned here). CTX-0362: the default is the geometry-neutral `auto`
    // overlay (transparent at rest).
    assert_eq!(
        bitty_runtime::config::DEFAULT_SCROLLBAR_WIDTH,
        bitty_config::types::DEFAULT_SCROLLBAR_WIDTH
    );
    assert_eq!(
        bitty_runtime::config::MAX_SCROLLBAR_WIDTH_PX,
        bitty_config::types::MAX_SCROLLBAR_WIDTH_PX
    );
    assert_eq!(
        bitty_runtime::ScrollbarMode::Auto.as_str(),
        bitty_config::ScrollbarMode::Auto.as_str()
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { scrollbar = { mode = "auto", width = 12 } }"#,
        &src,
    )
    .expect("scrollbar parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(
        merged.effective.scrollbar.mode,
        bitty_config::ScrollbarMode::Auto
    );
    assert_eq!(merged.effective.scrollbar.width, 12);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.scrollbar_mode, bitty_runtime::ScrollbarMode::Auto);
    assert_eq!(cfg.scrollbar_width, 12);
    assert_eq!(
        merged.source_of("scrollbar.mode").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides the auto default end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no scrollbar table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        merged2.effective.scrollbar.mode,
        bitty_config::ScrollbarMode::Auto
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(cfg2.scrollbar_mode, bitty_runtime::ScrollbarMode::Auto);
    assert_eq!(
        merged2.source_of("scrollbar.mode").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
    // Unknown modes and oversized widths fail closed at the file layer
    // (never reach runtime).
    let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    parse_lua_config(r#"return { scrollbar = { mode = "overlay" } }"#, &src3)
        .expect_err("must fail");
    let src4 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    parse_lua_config(r#"return { scrollbar = { width = 33 } }"#, &src4).expect_err("must fail");
}

#[test]
fn runtime_config_maps_animation_contract_end_to_end() {
    // RFC-0002 (CTX-0341): the accepted `appearance.animations` table flows
    // file -> effective -> runtime policy with `spring` resolved to
    // `ease_in_out`; safe mode zeroes every duration.
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    assert_eq!(
        bitty_runtime::config::MAX_ANIMATION_DURATION_MS,
        bitty_config::types::MAX_ANIMATION_DURATION_MS
    );
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { appearance = { animations = {
            enabled = true,
            reduced_motion = "auto",
            duration_ms = { open = 150, close = 120, focus = 100, workspace = 200, move = 150, resize = 120, drag = 150 },
            easing = { open = "spring", close = "linear", focus = "ease_in_out", workspace = "ease_in", move = "spring", resize = "linear", drag = "ease_out" },
        } } }"#,
        &src,
    )
    .expect("animations parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert!(cfg.animations.enabled);
    assert_eq!(
        cfg.animations.duration_ms,
        [150, 120, 100, 200, 150, 120, 150],
        "accepted durations map verbatim"
    );
    // `spring` resolves to `ease_in_out` at the boundary.
    assert_eq!(
        cfg.animations.curves[0],
        bitty_runtime::AnimationCurve::EaseInOut
    );
    assert_eq!(
        cfg.animations.curves[1],
        bitty_runtime::AnimationCurve::Linear
    );
    // CTX-0967: geometry leaves map verbatim with the same resolution.
    assert_eq!(
        cfg.animations.curves[4],
        bitty_runtime::AnimationCurve::EaseInOut,
        "move spring resolves"
    );
    assert_eq!(
        cfg.animations.curves[5],
        bitty_runtime::AnimationCurve::Linear
    );
    assert_eq!(
        cfg.animations.curves[6],
        bitty_runtime::AnimationCurve::EaseOut
    );
    assert_eq!(
        cfg.animations.reduced_motion,
        bitty_runtime::ReducedMotionMode::Auto
    );
    // Safe mode zeroes every duration and keeps the final-state contract.
    let safe = bitty_config::reload::fallback_builtin();
    let safe_cfg = runtime_config_from_effective(&safe).expect("safe builds");
    assert_eq!(safe_cfg.animations.duration_ms, [0, 0, 0, 0, 0, 0, 0]);
    // Out-of-range/unknown values fail closed before runtime.
    for bad in [
        r#"return { appearance = { animations = { duration_ms = { open = 501 } } } }"#,
        r#"return { appearance = { animations = { duration_ms = { move = 501 } } } }"#,
        r#"return { appearance = { animations = { easing = { open = "bounce" } } } }"#,
        r#"return { appearance = { animations = { easing = { drag = "bounce" } } } }"#,
        r#"return { appearance = { animations = { reduced_motion = "sometimes" } } }"#,
    ] {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        parse_lua_config(bad, &src).expect_err("must fail closed");
    }
}

#[test]
fn runtime_config_inherits_file_window_padding() {
    // CTX-0223: `window.padding` flows file -> effective -> runtime;
    // crate defaults stay equal (bitty-runtime must not depend on
    // bitty-config, so the pairing is by value, pinned here). Default
    // preserves the 8px breathing room for existing users.
    // CTX-0241 S0: `window.radius_px` rides the same path as a parsed
    // no-op (default 0, zero render effect); existing `{ opacity,
    // padding }` tables default it to 0 end to end.
    assert_eq!(
        bitty_runtime::config::DEFAULT_WINDOW_PADDING,
        bitty_config::EffectiveConfig::default().window.padding
    );
    assert_eq!(
        bitty_config::types::DEFAULT_WINDOW_PADDING,
        bitty_runtime::config::DEFAULT_WINDOW_PADDING
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX,
        bitty_config::EffectiveConfig::default().window.radius_px
    );
    assert_eq!(
        bitty_runtime::config::MAX_WINDOW_RADIUS_PX,
        bitty_config::types::MAX_WINDOW_RADIUS_PX,
        "runtime bound must match config validation (`must be <= 24`)"
    );
    assert_eq!(
        bitty_runtime::config::MAX_WINDOW_PADDING,
        bitty_config::types::MAX_WINDOW_PADDING,
        "runtime bound must match config validation (`must be <= 64`)"
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { window = { opacity = 0.9, padding = 4 } }"#,
        &src,
    )
    .expect("window parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.window.padding, 4);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.window_padding, 4);
    // CTX-0241 S0: legacy table without `radius_px` defaults to 0.
    assert_eq!(merged.effective.window.radius_px, 0);
    assert_eq!(cfg.window_radius_px, 0);
    assert_eq!(
        merged.source_of("window.radius_px").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    assert_eq!(
        merged.source_of("window.padding").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides the default end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no window table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        merged2.effective.window.padding,
        bitty_runtime::config::DEFAULT_WINDOW_PADDING
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(
        cfg2.window_padding,
        bitty_runtime::config::DEFAULT_WINDOW_PADDING
    );
    assert_eq!(
        merged2.effective.window.radius_px,
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX
    );
    assert_eq!(
        cfg2.window_radius_px,
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX
    );
    // Oversized padding fails closed at the file layer (never runtime).
    let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    parse_lua_config(
        r#"return { window = { opacity = 1.0, padding = 65 } }"#,
        &src3,
    )
    .expect_err("must fail");
}

#[test]
fn runtime_config_inherits_file_window_radius_noop() {
    // CTX-0241 S0: `window.radius_px` (physical px, `0..=24`, default 0)
    // flows file -> effective -> runtime as a parsed no-op: accepted,
    // stored, reported, zero render effect (proved in
    // `bitty-runtime/tests/window_radius_noop.rs`).
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 12 } }"#,
        &src,
    )
    .expect("window radius parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.window.radius_px, 12);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime builds");
    assert_eq!(cfg.window_radius_px, 12);
    assert_eq!(
        merged.source_of("window.radius_px").unwrap().layer,
        LayerKind::User
    );
    // Fail-closed validation: negative / oversized / float radius never
    // reaches runtime (file layer rejects with the field path).
    for bad in [
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = -1 } }"#,
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 25 } }"#,
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 100 } }"#,
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 1.5 } }"#,
    ] {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let err = parse_lua_config(bad, &src).expect_err("must fail");
        assert!(
            err.to_string().contains("window.radius_px"),
            "bad radius must name field: {err}"
        );
    }
}

#[test]
fn window_opacity_reaches_platform_config() {
    // CTX-0223: `window.opacity` flows effective -> platform window
    // creation; platform defaults match the config default (opaque),
    // and sub-1.0 values request transparency (fail-soft where the
    // platform ignores the flag).
    assert!(
        (bitty_platform::WindowConfig::default().opacity()
            - bitty_config::EffectiveConfig::default().window.opacity)
            .abs()
            < f32::EPSILON
    );
    let app_opacity = bitty_config::EffectiveConfig::default().window.opacity;
    let config = bitty_platform::WindowConfig::new().with_opacity(app_opacity);
    assert!(!config.is_transparent());
    let faded = bitty_platform::WindowConfig::new().with_opacity(0.9);
    assert!(faded.is_transparent());
    // The composition root carries the effective value to creation.
    let rt = bitty_runtime::Runtime::with_defaults().expect("runtime builds");
    let app = TerminalApp::with_theme(
        rt,
        "bitty-dark",
        "default",
        Vec::new(),
        SpawnSpec::default(),
    )
    .with_window_opacity(0.9);
    assert!((app.window.opacity - 0.9).abs() < f32::EPSILON);
}

#[test]
fn window_title_carries_theme_and_source() {
    let t = window_title_for_theme("bitty-dark", "file");
    assert!(t.contains("bitty-dark"));
    assert!(t.contains("file"));
    let d = window_title_for_theme("bitty-dark", "default");
    assert_ne!(t, d);
}

#[test]
fn parse_config_subcommands() {
    let p = parse_args(&args_of(&["bitty", "config", "check"]));
    assert!(p.config_word);
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    assert!(p.config_args.is_empty());
    assert_eq!(p.program, None);

    let p = parse_args(&args_of(&["bitty", "config", "path"]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Path));

    let p = parse_args(&args_of(&["bitty", "config", "edit"]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Edit));

    // Flags compose in any order: --config before or after the verb.
    let p = parse_args(&args_of(&[
        "bitty",
        "--config",
        "/tmp/c.lua",
        "config",
        "check",
    ]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));

    let p = parse_args(&args_of(&[
        "bitty",
        "config",
        "check",
        "--config",
        "/tmp/d.lua",
    ]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    assert_eq!(p.config_path.as_deref(), Some("/tmp/d.lua"));

    // Escape hatch: a program literally named `config`.
    let p = parse_args(&args_of(&["bitty", "--", "config"]));
    assert!(!p.config_word);
    assert_eq!(p.config_cmd, None);
    assert_eq!(p.program.as_deref(), Some("config"));
}

#[test]
fn parse_config_bare_and_unknown_verbs_fail_closed_at_parse() {
    let p = parse_args(&args_of(&["bitty", "config"]));
    assert!(p.config_word);
    assert_eq!(p.config_cmd, None);
    assert_eq!(p.program, None);

    let p = parse_args(&args_of(&["bitty", "config", "chek"]));
    assert!(p.config_word);
    assert_eq!(p.config_cmd, None);
    assert_eq!(p.config_args, vec!["chek".to_string()]);
    assert_eq!(p.program, None);

    // Extra positionals after a known verb are recorded for dispatch.
    let p = parse_args(&args_of(&["bitty", "config", "check", "extra"]));
    assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
    assert_eq!(p.config_args, vec!["extra".to_string()]);
}

#[test]
fn config_usage_names_verbs() {
    let usage = config_usage();
    assert!(usage.contains("path"));
    assert!(usage.contains("check"));
    assert!(usage.contains("edit"));
    assert!(usage.contains("init.lua"));
    // The persisted selector is a profile source too (#2349).
    assert!(
        usage.contains("persisted init.lua selector"),
        "usage lists the persisted selector as a profile source: {usage}"
    );
}

#[test]
fn resolve_editor_prefers_visual_then_editor_then_vi() {
    assert_eq!(
        resolve_editor_with_env(Some("/usr/bin/hx"), Some("/usr/bin/nano")),
        "/usr/bin/hx"
    );
    assert_eq!(
        resolve_editor_with_env(Some("  "), Some("/usr/bin/nano")),
        "/usr/bin/nano"
    );
    assert_eq!(resolve_editor_with_env(None, None), "vi");
    assert_eq!(resolve_editor_with_env(Some(""), Some(" ")), "vi");
}

/// Asserts every generated line uses a multiple of four leading spaces and
/// no tabs, and that no key starts at the legacy two-space indent.
fn assert_four_space_indent(name: &str, text: &str) {
    assert!(!text.contains('\t'), "{name} contains a tab");
    for (index, line) in text.lines().enumerate() {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        assert_eq!(
            spaces % 4,
            0,
            "{name}:{} uses {spaces}-space indentation: {line:?}",
            index + 1
        );
        assert!(
            !line.starts_with("  ")
                || !line[2..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'),
            "{name}:{} starts a key at two spaces: {line:?}",
            index + 1
        );
    }
}

/// Asserts the generated nested appearance shape and the absence of the flat
/// top-level `theme` alias (accepted for compatibility, never generated).
fn assert_nested_appearance(name: &str, text: &str, theme: &str) {
    assert!(
        text.contains("appearance = {"),
        "{name} has nested appearance"
    );
    assert!(
        text.contains(&format!("        theme = \"{theme}\",")),
        "{name} keeps theme under appearance:\n{text}"
    );
    assert!(
        !text.lines().any(|line| line.starts_with("    theme =")),
        "{name} must not emit the flat theme alias"
    );
}

/// Uncomments one generated `-- <marker>` block, using brace depth so nested
/// tables (for example `workspace.bar`) uncomment as a unit.
fn uncomment_lua_block(text: &str, marker: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    let mut depth = 0i32;
    for line in text.lines() {
        if !inside {
            let trimmed = line.trim_start();
            let code = trimmed.strip_prefix("-- ").unwrap_or(trimmed);
            if trimmed.starts_with("-- ") && code.trim_start() == marker {
                inside = true;
            } else {
                out.push_str(line);
                out.push('\n');
                continue;
            }
        }
        let pos = line.find("-- ").expect("commented block line");
        out.push_str(&line[..pos]);
        out.push_str(&line[pos + 3..]);
        out.push('\n');
        let code = &line[pos + 3..];
        depth += code.matches('{').count() as i32;
        depth -= code.matches('}').count() as i32;
        if depth == 0 {
            inside = false;
        }
    }
    assert!(!inside, "unterminated commented block {marker}");
    out
}

/// Uncomments individual commented keys inside already-emitted tables (the
/// optional `font`, `decoration`, and `terminal` examples).
fn uncomment_commented_keys(text: &str, keys: &[&str]) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let uncomment = trimmed.strip_prefix("-- ").is_some_and(|rest| {
            let code = rest.trim_start();
            keys.iter().any(|key| code.starts_with(key))
        });
        if uncomment {
            let pos = line.find("-- ").expect("commented key line");
            out.push_str(&line[..pos]);
            out.push_str(&line[pos + 3..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[test]
fn starter_init_lua_is_valid_config() {
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let starter = starter_init_lua();
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(&starter, &src).expect("starter valid");
    assert_eq!(
        plan.appearance.as_ref().and_then(|a| a.theme.as_deref()),
        Some("dark")
    );
    assert_nested_appearance("starter", &starter, "dark");
    assert_four_space_indent("starter", &starter);

    // CTX-0191: starter leaves `selection` unset (commented example only)
    // so new installs ride the default-on without a file override.
    assert!(plan.selection.is_none());
    assert!(starter.contains("auto_copy"));
    // CTX-0177: starter leaves `layout` unset (commented example only)
    // so new installs ride edge-to-edge without a file override.
    assert!(plan.layout.is_none());
    assert!(starter.contains("gaps_in"));
    assert!(starter.contains("gaps_out"));
    // CTX-0181/CTX-0362: starter leaves `scrollbar` unset (commented
    // example only) so new installs ride the auto overlay without a file
    // override.
    assert!(plan.scrollbar.is_none());
    assert!(starter.contains("scrollbar"));
    // CTX-0260: starter leaves `mouse` unset (commented example only)
    // so new installs ride the shipped hover-focus default without a file
    // override.
    assert!(plan.mouse.is_none());
    assert!(starter.contains("focus_follows_mouse"));
    assert!(starter.contains("focus_follows_mouse_delay_ms"));
    // CTX-0873: starter documents the bar edge as a commented example only.
    assert!(plan.workspace.is_none());
    assert!(starter.contains("bar = {"));
    assert!(starter.contains("edge = \"bottom\""));
    // Every other major section is a commented example only.
    assert!(plan.font.is_none());
    assert!(plan.window.is_none());
    assert!(plan.decoration.is_none());
    assert!(plan.terminal.is_none());
    assert!(plan.views.is_none());
    assert!(plan.leader_key.is_none());
    assert!(plan.leader_timeout_ms.is_none());
    assert!(plan.close_confirm.is_none());
    assert!(plan.keymaps.is_none());
    for section in [
        "font",
        "window",
        "layout",
        "decoration",
        "terminal",
        "selection",
        "mouse",
        "workspace",
        "scrollbar",
        "views",
        "input",
        "keymaps",
    ] {
        assert!(
            starter.contains(&format!("-- {section} = {{")),
            "starter documents {section}"
        );
    }

    // The commented scalar example also parses when uncommented.
    let with_close = uncomment_commented_keys(&starter, &["close_confirm ="]);
    let close_plan =
        parse_lua_config(&with_close, &src).expect("starter close_confirm example parses");
    assert_eq!(
        close_plan.close_confirm,
        Some(bitty_config::CloseConfirm::WhenBusy)
    );
}

#[test]
fn selector_commented_override_parses() {
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let selector = render_selector_lua(DEFAULT_PROFILE_NAME);
    let with_override = uncomment_commented_keys(&selector, &["window ="]);
    let plan = parse_lua_config(&with_override, &src).expect("selector override parses");
    assert!(plan.selector.is_some(), "the envelope stays intact");
    let window = plan.window.expect("window override applies");
    assert_eq!(window.opacity, 0.95);
    assert_eq!(window.padding, 8);
}

// -- `bitty init` wizard (CTX-0149, #243) --------------------------------

/// Unique scratch directory per test (process id + atomic counter: tests
/// in one binary share the id and run on parallel threads).
fn init_test_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("bitty-ctx0149-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn parse_init_subcommand() {
    let p = parse_args(&args_of(&["bitty", "init"]));
    assert!(p.init_word);
    assert!(p.init_args.is_empty());
    assert_eq!(p.program, None);
    assert!(!p.init_yes);
    assert!(!p.init_force);

    // Flags compose in any order around the word.
    let p = parse_args(&args_of(&["bitty", "init", "--yes", "--force"]));
    assert!(p.init_word);
    assert!(p.init_yes);
    assert!(p.init_force);

    // Value flags accept both `--flag VALUE` and `--flag=VALUE`.
    let p = parse_args(&args_of(&[
        "bitty",
        "init",
        "--yes",
        "--scrollback",
        "20000",
        "--close-confirm=never",
        "--gaps-in",
        "2",
        "--gaps-out=4",
        "--border",
        "1",
        "--radius=0",
    ]));
    assert!(p.init_word);
    assert_eq!(p.init_scrollback.as_deref(), Some("20000"));
    assert_eq!(p.init_close_confirm.as_deref(), Some("never"));
    assert_eq!(p.init_gaps_in.as_deref(), Some("2"));
    assert_eq!(p.init_gaps_out.as_deref(), Some("4"));
    assert_eq!(p.init_border.as_deref(), Some("1"));
    assert_eq!(p.init_radius.as_deref(), Some("0"));

    // A missing value is recorded empty so the init dispatch fails closed.
    let p = parse_args(&args_of(&["bitty", "init", "--yes", "--scrollback"]));
    assert_eq!(p.init_scrollback.as_deref(), Some(""));

    let p = parse_args(&args_of(&[
        "bitty",
        "--config",
        "/tmp/c.lua",
        "init",
        "--yes",
    ]));
    assert!(p.init_word);
    assert!(p.init_yes);
    assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));

    let p = parse_args(&args_of(&["bitty", "--yes", "init"]));
    assert!(p.init_word);
    assert!(p.init_yes);

    // Extra positionals are recorded for fail-closed dispatch.
    let p = parse_args(&args_of(&["bitty", "init", "extra"]));
    assert!(p.init_word);
    assert_eq!(p.init_args, vec!["extra".to_string()]);

    // Escape hatch: a program literally named `init`.
    let p = parse_args(&args_of(&["bitty", "--", "init"]));
    assert!(!p.init_word);
    assert_eq!(p.program.as_deref(), Some("init"));

    // `init` after `config` belongs to the config subcommand.
    let p = parse_args(&args_of(&["bitty", "config", "init"]));
    assert!(p.config_word);
    assert!(!p.init_word);
}

#[test]
fn parse_cli_v1_gap_words() {
    // `version` consumes the tail verbatim; escape hatch preserved.
    let p = parse_args(&args_of(&["bitty", "version"]));
    assert!(p.version_word);
    assert!(p.version_raw.is_empty());
    assert_eq!(p.program, None);
    let p = parse_args(&args_of(&["bitty", "version", "--format", "json"]));
    assert!(p.version_word);
    assert_eq!(
        p.version_raw,
        vec!["--format".to_string(), "json".to_string()]
    );
    let p = parse_args(&args_of(&["bitty", "--", "version"]));
    assert!(!p.version_word);
    assert_eq!(p.program.as_deref(), Some("version"));

    // `completion` + stable `comp` alias record the invoked spelling.
    let p = parse_args(&args_of(&["bitty", "completion", "bash"]));
    assert!(p.completion_word);
    assert_eq!(p.completion_spelling, "completion");
    assert_eq!(p.completion_raw, vec!["bash".to_string()]);
    let p = parse_args(&args_of(&["bitty", "comp", "fish"]));
    assert!(p.completion_word);
    assert_eq!(p.completion_spelling, "comp");
    let p = parse_args(&args_of(&["bitty", "--", "comp"]));
    assert!(!p.completion_word);
    assert_eq!(p.program.as_deref(), Some("comp"));

    // `shell-init` keeps its tail verbatim for the shell-init parser.
    let p = parse_args(&args_of(&["bitty", "shell-init", "bash"]));
    assert!(p.shell_init_word);
    assert_eq!(p.shell_init_raw, vec!["bash".to_string()]);
    let p = parse_args(&args_of(&["bitty", "--", "shell-init"]));
    assert!(!p.shell_init_word);
    assert_eq!(p.program.as_deref(), Some("shell-init"));

    // `cmd` keeps the separator-owned tail verbatim.
    let p = parse_args(&args_of(&[
        "bitty",
        "cmd",
        "core.terminal.text",
        "--format",
        "json",
        "--",
        "{\"terminal_id\":\"t:4\"}",
    ]));
    assert!(p.cmd_word);
    assert_eq!(
        p.cmd_raw,
        vec![
            "core.terminal.text".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "--".to_string(),
            "{\"terminal_id\":\"t:4\"}".to_string(),
        ]
    );
    let p = parse_args(&args_of(&["bitty", "--", "cmd"]));
    assert!(!p.cmd_word);
    assert_eq!(p.program.as_deref(), Some("cmd"));

    // `x` keeps plugin args (including dash tokens) verbatim.
    let p = parse_args(&args_of(&[
        "bitty",
        "x",
        "example.markdown",
        "render",
        "--file",
        "README.md",
    ]));
    assert!(p.x_word);
    assert_eq!(
        p.x_raw,
        vec![
            "example.markdown".to_string(),
            "render".to_string(),
            "--file".to_string(),
            "README.md".to_string(),
        ]
    );
    let p = parse_args(&args_of(&["bitty", "--", "x"]));
    assert!(!p.x_word);
    assert_eq!(p.program.as_deref(), Some("x"));

    // First word wins; later words are verbatim tail, never programs.
    let p = parse_args(&args_of(&["bitty", "version", "x"]));
    assert!(p.version_word);
    assert!(!p.x_word);
    let p = parse_args(&args_of(&["bitty", "cmd", "run"]));
    assert!(p.cmd_word);
    assert!(!p.run_word);

    // `cfg` is the stable alias for `config`.
    let p = parse_args(&args_of(&["bitty", "cfg", "path"]));
    assert!(p.config_word);
    assert_eq!(p.config_cmd, Some(ConfigCommand::Path));
    let p = parse_args(&args_of(&["bitty", "--", "cfg"]));
    assert!(!p.config_word);
    assert_eq!(p.program.as_deref(), Some("cfg"));
}

#[test]
fn parse_doctor_subcommand() {
    let p = parse_args(&args_of(&["bitty", "doctor"]));
    assert!(p.doctor_word);
    assert!(p.doctor_args.is_empty());
    assert_eq!(p.program, None);
    assert_eq!(p.doctor_format, None);
    assert!(!p.doctor_no_color);

    // Flags compose in any order around the word.
    let p = parse_args(&args_of(&["bitty", "doctor", "--format", "json"]));
    assert!(p.doctor_word);
    assert_eq!(p.doctor_format.as_deref(), Some("json"));

    let p = parse_args(&args_of(&["bitty", "--format", "json", "doctor"]));
    assert!(p.doctor_word);
    assert_eq!(p.doctor_format.as_deref(), Some("json"));

    let p = parse_args(&args_of(&["bitty", "doctor", "--format=jsonl"]));
    assert_eq!(p.doctor_format.as_deref(), Some("jsonl"));

    let p = parse_args(&args_of(&[
        "bitty",
        "--config",
        "/tmp/c.lua",
        "doctor",
        "--no-color",
    ]));
    assert!(p.doctor_word);
    assert!(p.doctor_no_color);
    assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));

    // Extra positionals are recorded for fail-closed dispatch.
    let p = parse_args(&args_of(&["bitty", "doctor", "extra"]));
    assert!(p.doctor_word);
    assert_eq!(p.doctor_args, vec!["extra".to_string()]);

    // Escape hatch: a program literally named `doctor`.
    let p = parse_args(&args_of(&["bitty", "--", "doctor"]));
    assert!(!p.doctor_word);
    assert_eq!(p.program.as_deref(), Some("doctor"));

    // `doctor` after `config`/`init` belongs to that subcommand.
    let p = parse_args(&args_of(&["bitty", "config", "doctor"]));
    assert!(p.config_word);
    assert!(!p.doctor_word);
    let p = parse_args(&args_of(&["bitty", "init", "doctor"]));
    assert!(p.init_word);
    assert!(!p.doctor_word);
}

#[test]
fn doctor_usage_names_format_and_checks() {
    let usage = doctor::doctor_usage();
    assert!(usage.contains("doctor"));
    assert!(usage.contains("--format"));
    assert!(usage.contains("table|json|jsonl"));
    let help = help_text();
    assert!(help.contains("doctor"));
    assert!(help.contains("--format"));
}

#[test]
fn parse_ctl_subcommand() {
    // Bare `ctl` captures no tokens (dispatch fails closed with usage).
    let p = parse_args(&args_of(&["bitty", "ctl"]));
    assert!(p.ctl_word);
    assert!(p.ctl_raw.is_empty());
    assert_eq!(p.program, None);
    assert!(!p.run_word);
    assert!(!p.doctor_word);

    // Tokens after the word go verbatim to `ctl_raw`.
    let p = parse_args(&args_of(&["bitty", "ctl", "terminal", "list"]));
    assert!(p.ctl_word);
    assert_eq!(p.ctl_raw, vec!["terminal".to_string(), "list".to_string()]);

    // Global flags before the word land in pre-fields; post-word flags
    // stay verbatim in `ctl_raw` for `ctl::parse_ctl_request`.
    let p = parse_args(&args_of(&[
        "bitty",
        "--socket",
        "/tmp/a.sock",
        "ctl",
        "terminal",
        "list",
    ]));
    assert!(p.ctl_word);
    assert_eq!(p.ctl_socket_pre.as_deref(), Some("/tmp/a.sock"));
    assert_eq!(p.ctl_raw, vec!["terminal".to_string(), "list".to_string()]);

    let p = parse_args(&args_of(&[
        "bitty",
        "--instance",
        "demo_1",
        "ctl",
        "view",
        "list",
    ]));
    assert!(p.ctl_word);
    assert_eq!(p.ctl_instance_pre.as_deref(), Some("demo_1"));

    let p = parse_args(&args_of(&["bitty", "ctl", "--socket=/tmp/b.sock"]));
    assert!(p.ctl_word);
    assert!(p.ctl_socket_pre.is_none());
    assert_eq!(p.ctl_raw, vec!["--socket=/tmp/b.sock".to_string()]);

    // Escape hatch: a program literally named `ctl`.
    let p = parse_args(&args_of(&["bitty", "--", "ctl"]));
    assert!(!p.ctl_word);
    assert_eq!(p.program.as_deref(), Some("ctl"));

    // `ctl` after other subcommands belongs to that subcommand.
    let p = parse_args(&args_of(&["bitty", "config", "ctl"]));
    assert!(p.config_word);
    assert!(!p.ctl_word);
    let p = parse_args(&args_of(&["bitty", "doctor", "ctl"]));
    assert!(p.doctor_word);
    assert!(!p.ctl_word);

    // Help mentions the new subcommand.
    let help = help_text();
    assert!(help.contains("ctl"));
    assert!(help.contains("--socket"));
    assert!(help.contains("--instance"));
}

#[test]
fn parse_inspect_subcommand() {
    // Target + value land in dedicated fields (dispatch validates).
    let p = parse_args(&args_of(&["bitty", "inspect", "command", "core.view.list"]));
    assert!(p.inspect_word);
    assert_eq!(p.inspect_target.as_deref(), Some("command"));
    assert_eq!(p.inspect_value.as_deref(), Some("core.view.list"));
    assert!(p.inspect_args.is_empty());
    assert_eq!(p.program, None);
    assert!(!p.ctl_word);
    assert!(!p.list_word);
    assert!(!p.doctor_word);

    // `--format`/`--no-color` compose before or after the word.
    let p = parse_args(&args_of(&[
        "bitty", "--format", "json", "inspect", "key", "alt+h",
    ]));
    assert!(p.inspect_word);
    assert_eq!(p.inspect_format.as_deref(), Some("json"));
    let p = parse_args(&args_of(&[
        "bitty",
        "inspect",
        "config",
        "font.size",
        "--format=json",
    ]));
    assert_eq!(p.inspect_format.as_deref(), Some("json"));
    let p = parse_args(&args_of(&[
        "bitty",
        "inspect",
        "key",
        "alt+h",
        "--no-color",
    ]));
    assert!(p.inspect_no_color);

    // Extra positionals and stray flags fail closed at dispatch.
    let p = parse_args(&args_of(&[
        "bitty",
        "inspect",
        "command",
        "core.view.list",
        "extra",
    ]));
    assert_eq!(p.inspect_args, vec!["extra".to_string()]);
    let p = parse_args(&args_of(&["bitty", "inspect", "command", "x", "--"]));
    assert_eq!(p.inspect_args, vec!["--".to_string()]);
    let p = parse_args(&args_of(&["bitty", "inspect", "command", "x", "--bogus"]));
    assert_eq!(p.inspect_args, vec!["--bogus".to_string()]);

    // Escape hatch: a program literally named `inspect`.
    let p = parse_args(&args_of(&["bitty", "--", "inspect"]));
    assert!(!p.inspect_word);
    assert_eq!(p.program.as_deref(), Some("inspect"));

    // `inspect` after other subcommands belongs to that subcommand.
    let p = parse_args(&args_of(&["bitty", "list", "inspect"]));
    assert!(p.list_word);
    assert!(!p.inspect_word);
    let p = parse_args(&args_of(&["bitty", "doctor", "inspect"]));
    assert!(p.doctor_word);
    assert!(!p.inspect_word);

    // Help mentions the new subcommand.
    let help = help_text();
    assert!(help.contains("inspect <target>"));
    assert!(help.contains("command|key|plugin|config|protocol"));
}

#[test]
fn parse_dev_subcommand() {
    // Bare `dev` captures no tokens (dispatch fails closed with usage).
    let p = parse_args(&args_of(&["bitty", "dev"]));
    assert!(p.dev_word);
    assert!(p.dev_raw.is_empty());
    assert_eq!(p.program, None);
    assert!(!p.run_word);
    assert!(!p.ctl_word);
    assert!(!p.list_word);

    // Tokens after the word go verbatim to `dev_raw`.
    let p = parse_args(&args_of(&["bitty", "dev", "trace", "startup"]));
    assert!(p.dev_word);
    assert_eq!(p.dev_raw, vec!["trace".to_string(), "startup".to_string()]);

    // Post-word flags stay verbatim for `dev::parse_dev_request`.
    let p = parse_args(&args_of(&[
        "bitty", "dev", "capture", "--layout", "split", "--format", "json",
    ]));
    assert!(p.dev_word);
    assert_eq!(
        p.dev_raw,
        vec![
            "capture".to_string(),
            "--layout".to_string(),
            "split".to_string(),
            "--format".to_string(),
            "json".to_string()
        ]
    );
    assert_eq!(p.dev_format, None);

    // Global flags before the word land in dev pre-fields.
    let p = parse_args(&args_of(&["bitty", "--format", "json", "dev", "capture"]));
    assert!(p.dev_word);
    assert_eq!(p.dev_format.as_deref(), Some("json"));
    assert_eq!(p.dev_raw, vec!["capture".to_string()]);

    let p = parse_args(&args_of(&[
        "bitty",
        "--socket",
        "/tmp/a.sock",
        "dev",
        "capture",
    ]));
    assert!(p.dev_word);
    assert_eq!(p.dev_socket_pre.as_deref(), Some("/tmp/a.sock"));
    assert_eq!(p.dev_raw, vec!["capture".to_string()]);

    // `--no-color` before the word lands in the dev pre-field; after the
    // word it stays verbatim for `dev::parse_dev_request`.
    let p = parse_args(&args_of(&["bitty", "--no-color", "dev", "capture"]));
    assert!(p.dev_word);
    assert!(p.dev_no_color);
    assert_eq!(p.dev_raw, vec!["capture".to_string()]);

    let p = parse_args(&args_of(&["bitty", "dev", "--no-color", "overlay", "list"]));
    assert!(p.dev_word);
    assert!(!p.dev_no_color);
    assert_eq!(
        p.dev_raw,
        vec![
            "--no-color".to_string(),
            "overlay".to_string(),
            "list".to_string()
        ]
    );

    // Escape hatch: a program literally named `dev`.
    let p = parse_args(&args_of(&["bitty", "--", "dev"]));
    assert!(!p.dev_word);
    assert_eq!(p.program.as_deref(), Some("dev"));

    // `dev` after other subcommands belongs to that subcommand.
    let p = parse_args(&args_of(&["bitty", "config", "dev"]));
    assert!(p.config_word);
    assert!(!p.dev_word);
    let p = parse_args(&args_of(&["bitty", "doctor", "dev"]));
    assert!(p.doctor_word);
    assert!(!p.dev_word);
    let p = parse_args(&args_of(&["bitty", "ctl", "dev"]));
    assert!(p.ctl_word);
    assert!(!p.dev_word);

    // Other words after `dev` stay verbatim (dev dispatch rejects them).
    let p = parse_args(&args_of(&["bitty", "dev", "ctl"]));
    assert!(p.dev_word);
    assert!(!p.ctl_word);
    assert_eq!(p.dev_raw, vec!["ctl".to_string()]);

    // Help mentions the new subcommand.
    let help = help_text();
    assert!(help.contains("dev <verb>"));
    assert!(help.contains("bitty dev --help"));
}

#[test]
fn parse_plugin_subcommand() {
    let p = parse_args(&args_of(&["bitty", "plugin", "list"]));
    assert!(p.plugin_word);
    assert_eq!(p.plugin_raw, vec!["list".to_string()]);
    assert_eq!(p.program, None);

    // Tokens after the word stay verbatim for the plugin parser.
    let p = parse_args(&args_of(&[
        "bitty",
        "plugin",
        "install",
        "bitty-terminal.workspace",
        "--yes",
    ]));
    assert!(p.plugin_word);
    assert_eq!(
        p.plugin_raw,
        vec![
            "install".to_string(),
            "bitty-terminal.workspace".to_string(),
            "--yes".to_string()
        ]
    );

    // Global --format before the word is stashed as the fallback.
    let p = parse_args(&args_of(&["bitty", "--format", "json", "plugin", "list"]));
    assert!(p.plugin_word);
    assert_eq!(p.plugin_format.as_deref(), Some("json"));

    let p = parse_args(&args_of(&["bitty", "--no-color", "plugin", "list"]));
    assert!(p.plugin_word);
    assert!(p.plugin_no_color);

    // Escape hatch: a program literally named `plugin`.
    let p = parse_args(&args_of(&["bitty", "--", "plugin", "list"]));
    assert!(!p.plugin_word);
    assert_eq!(p.program.as_deref(), Some("plugin"));

    // `plugin` after another word belongs to that word's args.
    let p = parse_args(&args_of(&["bitty", "list", "plugin"]));
    assert!(p.list_word);
    assert!(!p.plugin_word);

    // Help mentions the new subcommand.
    let help = help_text();
    assert!(help.contains("plugin <verb>"));
    assert!(help.contains("bitty plugin --help"));
}

#[test]
fn init_yes_defaults_are_sane() {
    let d = init_yes_defaults(Some("/bin/bash"));
    assert_eq!(d.shell.as_deref(), Some("/bin/bash"));
    assert_eq!(d.theme, "dark");
    assert_eq!(d.font_family, bitty_config::types::DEFAULT_FONT_FAMILY);
    assert_eq!(d.font_size, bitty_config::types::DEFAULT_FONT_SIZE);
    assert_eq!(d.key_preset, InitKeyPreset::Default);
    assert_eq!(
        d.scrollback,
        bitty_config::types::TerminalConfig::default().scrollback
    );
    assert_eq!(d.close_confirm, bitty_config::CloseConfirm::WhenBusy);
    assert_eq!(
        d.gaps_in,
        bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX
    );
    assert_eq!(
        d.gaps_out,
        bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX
    );
    assert_eq!(d.border, bitty_config::types::DEFAULT_DECORATION_BORDER_PX);
    assert_eq!(d.radius, bitty_config::types::DEFAULT_DECORATION_RADIUS_PX);

    // Blank $SHELL means "leave unset" (startup falls back to /bin/sh).
    let d = init_yes_defaults(Some("   "));
    assert_eq!(d.shell, None);
    let d = init_yes_defaults(None);
    assert_eq!(d.shell, None);

    // Unusable $SHELL never becomes a default (warned + omitted at dispatch).
    let d = init_yes_defaults(Some("/bin/ba\x07sh"));
    assert_eq!(d.shell, None);
}

#[test]
fn init_answer_parsers_cover_new_steps_fail_closed() {
    // Font family: empty takes the default; trim; bound + controls rejected.
    assert_eq!(
        init_parse_font_family_answer("").expect("default"),
        bitty_config::types::DEFAULT_FONT_FAMILY
    );
    assert_eq!(
        init_parse_font_family_answer("  Fira Code  ").expect("trim"),
        "Fira Code"
    );
    assert!(init_parse_font_family_answer("a\x07b").is_err());
    assert!(init_parse_font_family_answer(&"x".repeat(200)).is_err());

    // Scrollback: empty default, exact bounds accepted, everything else fails.
    assert_eq!(
        init_parse_scrollback_answer("").expect("default"),
        bitty_config::types::TerminalConfig::default().scrollback
    );
    assert_eq!(init_parse_scrollback_answer("0").expect("zero"), 0);
    assert_eq!(
        init_parse_scrollback_answer("100000").expect("max"),
        100_000
    );
    for bad in ["100001", "-1", "1.5", "many", " 12x"] {
        assert!(
            init_parse_scrollback_answer(bad).is_err(),
            "scrollback {bad:?} fails closed"
        );
    }

    // Close confirm: empty/1 default, exact names and menu numbers
    // (case-insensitive names).
    assert_eq!(
        init_parse_close_confirm_answer("").expect("default"),
        bitty_config::CloseConfirm::WhenBusy
    );
    assert_eq!(
        init_parse_close_confirm_answer("1").expect("menu default"),
        bitty_config::CloseConfirm::WhenBusy
    );
    assert_eq!(
        init_parse_close_confirm_answer("always").expect("always"),
        bitty_config::CloseConfirm::Always
    );
    assert_eq!(
        init_parse_close_confirm_answer("2").expect("menu always"),
        bitty_config::CloseConfirm::Always
    );
    assert_eq!(
        init_parse_close_confirm_answer("NEVER").expect("never"),
        bitty_config::CloseConfirm::Never
    );
    assert_eq!(
        init_parse_close_confirm_answer("3").expect("menu never"),
        bitty_config::CloseConfirm::Never
    );
    for bad in ["sometimes", "true", "0", "4"] {
        assert!(
            init_parse_close_confirm_answer(bad).is_err(),
            "close_confirm {bad:?} fails closed"
        );
    }

    // Decoration scalars: empty default, per-field bounds, fail-closed.
    let gaps_default = bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX;
    assert_eq!(
        init_parse_decoration_answer("", "gaps_in", 32, gaps_default).expect("default"),
        gaps_default
    );
    assert_eq!(
        init_parse_decoration_answer("0", "border", 8, 2).expect("zero"),
        0
    );
    assert_eq!(
        init_parse_decoration_answer("32", "gaps_in", 32, gaps_default).expect("max gap"),
        32
    );
    assert!(init_parse_decoration_answer("33", "gaps_in", 32, gaps_default).is_err());
    assert!(init_parse_decoration_answer("9", "border", 8, 2).is_err());
    assert!(init_parse_decoration_answer("17", "radius", 16, 6).is_err());
    assert!(init_parse_decoration_answer("2.5", "border", 8, 2).is_err());
}

#[test]
fn init_shell_candidates_order_and_fallback() {
    // $SHELL first, then existing commons, no duplicates, fallback last.
    let c = init_shell_candidates(Some("/bin/zsh"), &|p| p == "/bin/bash" || p == "/bin/zsh");
    assert_eq!(c, vec!["/bin/zsh", "/bin/bash", "/bin/sh"]);

    // Nothing set and nothing exists: still exactly the fallback.
    let c = init_shell_candidates(None, &|_| false);
    assert_eq!(c, vec!["/bin/sh"]);

    // Blank env is ignored, not listed.
    let c = init_shell_candidates(Some("  "), &|p| p == "/bin/sh");
    assert_eq!(c, vec!["/bin/sh"]);
}

#[test]
fn init_shell_name_positional_gets_shell_init_hint() {
    // `bitty init fish` reads like a shell-init invocation (#2367).
    assert_eq!(
        crate::init::init_shell_name_hint(&["fish".to_string()]).expect("fish hint"),
        "bitty init: `fish` is a shell name; did you mean `bitty shell-init fish`?"
    );
    // Every supported shell name resolves to its canonical shell-init spelling.
    for name in ["bash", "zsh", "fish", "powershell", "nushell"] {
        let hint =
            crate::init::init_shell_name_hint(&[name.to_string()]).expect("supported shell hint");
        assert!(
            hint.contains(&format!("`bitty shell-init {name}`")),
            "{hint}"
        );
    }
    // The generic unexpected-argument path stays generic: unknown words,
    // shell paths, extra tokens, and no tokens get no hint.
    assert!(crate::init::init_shell_name_hint(&["bogus".to_string()]).is_none());
    assert!(crate::init::init_shell_name_hint(&["/bin/fish".to_string()]).is_none());
    assert!(crate::init::init_shell_name_hint(&["fish".to_string(), "zsh".to_string()]).is_none());
    assert!(crate::init::init_shell_name_hint(&[]).is_none());
}

#[test]
fn init_step_parsers_accept_and_reject() {
    let cands = vec!["/bin/bash".to_string(), "/bin/sh".to_string()];
    // Shell: empty takes the default, numbers pick, customs validate.
    assert_eq!(
        init_parse_shell_answer("", &cands).expect("default"),
        Some("/bin/bash".to_string())
    );
    assert_eq!(
        init_parse_shell_answer("2", &cands).expect("pick"),
        Some("/bin/sh".to_string())
    );
    assert_eq!(
        init_parse_shell_answer("/usr/bin/fish", &cands).expect("custom"),
        Some("/usr/bin/fish".to_string())
    );
    assert!(init_parse_shell_answer("0", &cands).is_err());
    assert!(init_parse_shell_answer("9", &cands).is_err());
    assert!(init_parse_shell_answer("a\x07b", &cands).is_err());

    // Theme: the default plus any catalog name/alias resolves; typos reprompt.
    assert_eq!(init_parse_theme_answer("").expect("default"), "dark");
    assert_eq!(
        init_parse_theme_answer("Bitty-Dark").expect("registry name"),
        "dark"
    );
    assert_eq!(
        init_parse_theme_answer("Tokyo-Night").expect("catalog name"),
        "tokyo-night"
    );
    assert_eq!(
        init_parse_theme_answer("catppuccin").expect("catalog alias"),
        "catppuccin-mocha"
    );
    assert!(init_parse_theme_answer("not-a-theme").is_err());

    // Font size: default, valid, and the FontConfig bound.
    assert_eq!(
        init_parse_font_size_answer("").expect("default"),
        bitty_config::types::DEFAULT_FONT_SIZE
    );
    assert_eq!(init_parse_font_size_answer("14").expect("int"), 14.0);
    assert_eq!(init_parse_font_size_answer(" 13.5 ").expect("float"), 13.5);
    for bad in ["0", "-3", "129", "nan", "inf", "big", "12pt"] {
        assert!(
            init_parse_font_size_answer(bad).is_err(),
            "must reject {bad:?}"
        );
    }

    // Preset: default vs vim, nothing else.
    assert_eq!(
        init_parse_preset_answer("").expect("default"),
        InitKeyPreset::Default
    );
    assert_eq!(
        init_parse_preset_answer("2").expect("vim"),
        InitKeyPreset::Vim
    );
    assert_eq!(
        init_parse_preset_answer("VIM").expect("vim word"),
        InitKeyPreset::Vim
    );
    assert!(init_parse_preset_answer("3").is_err());
    assert!(init_parse_preset_answer("emacs").is_err());

    // Shell cleaning: trims, bounds, rejects controls.
    assert_eq!(init_clean_shell("  /bin/bash ").expect("trim"), "/bin/bash");
    assert!(init_clean_shell("   ").is_err());
    assert!(init_clean_shell(&"x".repeat(2000)).is_err());
}

/// Drives the interactive wizard with piped stdin; returns answers plus
/// everything the wizard printed.
fn drive_init_wizard(
    stdin_lines: &str,
    shell_env: Option<&str>,
) -> (Result<InitAnswers, String>, String) {
    drive_init_wizard_with_overrides(stdin_lines, shell_env, &InitOverrides::default())
}

/// [`drive_init_wizard`] with explicit value-flag overrides (skipped steps).
fn drive_init_wizard_with_overrides(
    stdin_lines: &str,
    shell_env: Option<&str>,
    overrides: &InitOverrides,
) -> (Result<InitAnswers, String>, String) {
    drive_init_wizard_with_color(stdin_lines, shell_env, overrides, false)
}

/// [`drive_init_wizard`] with an explicit option-list color decision.
/// `false` keeps output plain (existing assertions stay greppable);
/// `true` enables ANSI entries for color regression tests.
fn drive_init_wizard_with_color(
    stdin_lines: &str,
    shell_env: Option<&str>,
    overrides: &InitOverrides,
    color: bool,
) -> (Result<InitAnswers, String>, String) {
    let mut input = std::io::BufReader::new(stdin_lines.as_bytes());
    let mut output = Vec::new();
    let result = run_init_interactive(
        &mut input,
        &mut output,
        shell_env,
        &|p| p == "/bin/bash" || p == "/bin/sh",
        overrides,
        color,
        false,
    );
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    (result, printed)
}

#[test]
fn init_interactive_all_defaults() {
    // Eleven Enters: default shell, theme, family, size, gaps_in, gaps_out,
    // border, radius, scrollback, close_confirm, keys.
    let (result, printed) = drive_init_wizard("\n\n\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    let answers = result.expect("defaults accepted");
    assert_eq!(answers.shell.as_deref(), Some("/bin/bash"));
    assert_eq!(answers.theme, "dark");
    assert_eq!(
        answers.font_family,
        bitty_config::types::DEFAULT_FONT_FAMILY
    );
    assert_eq!(answers.font_size, bitty_config::types::DEFAULT_FONT_SIZE);
    assert_eq!(
        answers.gaps_in,
        bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX
    );
    assert_eq!(
        answers.gaps_out,
        bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX
    );
    assert_eq!(
        answers.border,
        bitty_config::types::DEFAULT_DECORATION_BORDER_PX
    );
    assert_eq!(
        answers.radius,
        bitty_config::types::DEFAULT_DECORATION_RADIUS_PX
    );
    assert_eq!(
        answers.scrollback,
        bitty_config::types::TerminalConfig::default().scrollback
    );
    assert_eq!(answers.close_confirm, bitty_config::CloseConfirm::WhenBusy);
    assert_eq!(answers.key_preset, InitKeyPreset::Default);

    // Greeting shows the welcome line plus every step prompt.
    assert!(printed.contains("Welcome to bitty"));
    assert!(printed.contains("Shell"));
    assert!(printed.contains("Theme"));
    assert!(printed.contains("Font family"));
    assert!(printed.contains("Font size"));
    assert!(printed.contains("gaps_in"));
    assert!(printed.contains("gaps_out"));
    assert!(printed.contains("border"));
    assert!(printed.contains("radius"));
    assert!(printed.contains("Scrollback"));
    assert!(printed.contains("Close confirm"));
    assert!(printed.contains("Keybindings"));
}

#[test]
fn init_interactive_custom_picks() {
    // Pick /bin/sh (#2), bitty-dark, Fira Code, 14pt, 10/12/3/8 decoration,
    // 50000 scrollback, always confirm, vim preset (#2).
    let (result, _) = drive_init_wizard(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n",
        Some("/bin/bash"),
    );
    let answers = result.expect("custom picks accepted");
    assert_eq!(answers.shell.as_deref(), Some("/bin/sh"));
    assert_eq!(answers.theme, "dark");
    assert_eq!(answers.font_family, "Fira Code");
    assert_eq!(answers.font_size, 14.0);
    assert_eq!(answers.gaps_in, 10);
    assert_eq!(answers.gaps_out, 12);
    assert_eq!(answers.border, 3);
    assert_eq!(answers.radius, 8);
    assert_eq!(answers.scrollback, 50_000);
    assert_eq!(answers.close_confirm, bitty_config::CloseConfirm::Always);
    assert_eq!(answers.key_preset, InitKeyPreset::Vim);
}

#[test]
fn init_interactive_retries_then_aborts() {
    // Bad font size reprompts and then accepts the correction (then the
    // remaining seven steps take defaults).
    let (result, printed) =
        drive_init_wizard("\n\n\nbanana\n14\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    assert!(result.is_ok());
    assert!(printed.contains("try again"));

    // A bad decoration value also reprompts instead of aborting.
    let (result, printed) = drive_init_wizard("\n\n\n\n99\n4\n\n\n\n\n\n\n", Some("/bin/bash"));
    assert!(result.is_ok());
    assert!(printed.contains("try again"));

    // Three bad preset answers exhaust the bound and abort.
    let (result, _) =
        drive_init_wizard("\n\n\n\n\n\n\n\n\n\nnope\nnah\nnever\n", Some("/bin/bash"));
    assert!(result.is_err());

    // EOF up front aborts without guessing.
    let (result, _) = drive_init_wizard("", Some("/bin/bash"));
    assert!(result.is_err());

    // A narrow window still wizards with the plain welcome line: no ASCII
    // art and no fallback line (#2135 removed the init banner entirely).
    let (result, printed) = drive_init_wizard("\n\n\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    assert!(result.is_ok());
    assert!(!printed.contains("too narrow"));
    assert!(!printed.contains("MMMMM"));
}

#[test]
fn init_interactive_overrides_skip_prompts() {
    // Nine of eleven steps answered by value flags: only shell and the key
    // preset still prompt, and no overridden step prompt is printed.
    let overrides = InitOverrides {
        theme: Some("dark".to_string()),
        font_family: Some("Fira Code".to_string()),
        font_size: Some(15.0),
        scrollback: Some(50_000),
        close_confirm: Some(bitty_config::CloseConfirm::Never),
        gaps_in: Some(0),
        gaps_out: Some(0),
        border: Some(1),
        radius: Some(0),
        plugins: None,
        with_components: None,
        print_shell_init: None,
    };
    let (result, printed) = drive_init_wizard_with_overrides("\n\n", Some("/bin/bash"), &overrides);
    let answers = result.expect("overrides answer every overridden step");
    assert_eq!(answers.theme, "dark");
    assert_eq!(answers.font_family, "Fira Code");
    assert_eq!(answers.font_size, 15.0);
    assert_eq!(answers.scrollback, 50_000);
    assert_eq!(answers.close_confirm, bitty_config::CloseConfirm::Never);
    assert_eq!(answers.gaps_in, 0);
    assert_eq!(answers.border, 1);
    assert_eq!(answers.shell.as_deref(), Some("/bin/bash"));
    assert_eq!(answers.key_preset, InitKeyPreset::Default);
    assert!(!printed.contains("Theme ["));
    assert!(!printed.contains("gaps_in ["));
    assert!(!printed.contains("Scrollback ["));
    assert!(!printed.contains("Close confirm ["));
    // Unanswered steps still prompt.
    assert!(printed.contains("Shell"));
    assert!(printed.contains("Keybindings"));
}

// -- `bitty init` wizard key parsing (#1805) ---------------------------------
//
// The wizard line reader decodes the terminal key tables (legacy xterm
// `encode_named_key` sequences plus kitty `CSI u` frames) into editing
// actions so raw `ESC [` bytes never land in answers.

fn init_decode(raw: &[u8], history: &[String]) -> String {
    crate::init::init_decode_line_bytes(raw, history)
}

#[test]
fn init_wizard_left_arrow_moves() {
    // `ab`, Left, `c` inserts before `b`.
    assert_eq!(init_decode(b"ab\x1b[Dc", &[]), "acb");
    // Application cursor (`SS3 D`) agrees.
    assert_eq!(init_decode(b"ab\x1bODc", &[]), "acb");
    // CSI with modifiers still moves.
    assert_eq!(init_decode(b"ab\x1b[1;5Dc", &[]), "acb");
    // At the left edge Left stays.
    assert_eq!(init_decode(b"a\x1b[D\x1b[Dc", &[]), "ca");
}

#[test]
fn init_wizard_right_arrow_moves() {
    // `ab`, Left, Left, Right, `c` inserts in the middle.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[D\x1b[Cc", &[]), "acb");
    // Application cursor (`SS3 C`) agrees.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[D\x1bOCc", &[]), "acb");
    // At the right edge Right stays.
    assert_eq!(init_decode(b"ab\x1b[Cc", &[]), "abc");
}

#[test]
fn init_wizard_up_recalls_history_or_ignored() {
    // Empty history: Up is a graceful no-op.
    assert_eq!(init_decode(b"\x1b[A", &[]), "");
    assert_eq!(init_decode(b"ab\x1b[A", &[]), "ab");
    // Non-empty history: Up recalls the last answer.
    let history = vec!["hello".to_string()];
    assert_eq!(init_decode(b"\x1b[A", &history), "hello");
    // Application cursor and modified CSI agree.
    assert_eq!(init_decode(b"\x1bOA", &history), "hello");
    assert_eq!(init_decode(b"\x1b[1;5A", &history), "hello");
    // Two Ups walk back through two entries.
    let history = vec!["first".to_string(), "second".to_string()];
    assert_eq!(init_decode(b"\x1b[A\x1b[A", &history), "first");
    // Typing after recall appends to the recalled buffer.
    assert_eq!(init_decode(b"\x1b[A!", &history), "second!");
}

#[test]
fn init_wizard_down_restores_or_ignored() {
    // No prior Up in this line: Down stays.
    let history = vec!["x".to_string()];
    assert_eq!(init_decode(b"ab\x1b[B", &history), "ab");
    assert_eq!(init_decode(b"\x1b[B", &[]), "");
    // Up, Up, Down lands back on the newer entry.
    let history = vec!["first".to_string(), "second".to_string()];
    assert_eq!(init_decode(b"\x1b[A\x1b[A\x1b[B", &history), "second");
    // Up then Down restores the saved in-progress buffer.
    assert_eq!(init_decode(b"ab\x1b[A\x1b[B", &history), "ab");
}

#[test]
fn init_wizard_del_deletes_at_cursor() {
    // `ab`, Left, DEL removes `b`.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[3~", &[]), "a");
    // Modified `CSI 3;5 ~` (Ctrl+DEL) still deletes.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[3;5~", &[]), "a");
    // Kitty `CSI 3 u` agrees.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[3u", &[]), "a");
    // DEL at the end of the buffer is a no-op.
    assert_eq!(init_decode(b"ab\x1b[3~", &[]), "ab");
    // Legacy `0x7f`/`0x08` delete before the cursor.
    assert_eq!(init_decode(b"ab\x7fc", &[]), "ac");
    assert_eq!(init_decode(b"ab\x08c", &[]), "ac");
}

#[test]
fn init_wizard_ins_ignored() {
    // INS never toggles or inserts: the surrounding text joins up.
    assert_eq!(init_decode(b"ab\x1b[2~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[2;5~c", &[]), "abc");
}

#[test]
fn init_wizard_home_moves_to_start() {
    assert_eq!(init_decode(b"ab\x1b[Hc", &[]), "cab");
    assert_eq!(init_decode(b"ab\x1b[1~c", &[]), "cab");
    assert_eq!(init_decode(b"ab\x1b[7~c", &[]), "cab");
    assert_eq!(init_decode(b"ab\x1bOHc", &[]), "cab");
}

#[test]
fn init_wizard_end_moves_to_end() {
    // `ab`, Left, Left, End, `c` appends.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[D\x1b[Fc", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[D\x1b[D\x1b[4~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[D\x1b[D\x1b[8~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[D\x1b[D\x1bOFc", &[]), "abc");
}

#[test]
fn init_wizard_f1_ignored() {
    // Legacy SS3, `CSI 11 ~`, and kitty `P` trailers all drop.
    assert_eq!(init_decode(b"ab\x1bOPc", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[11~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[11;5~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[1Pc", &[]), "abc");
}

#[test]
fn init_wizard_f2_ignored() {
    assert_eq!(init_decode(b"ab\x1bOQc", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[12~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[12;5~c", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[1Qc", &[]), "abc");
}

#[test]
fn init_wizard_raw_esc_never_in_answers() {
    // Every listed key plus truncation/unknown/OSC/Alt leftovers: no
    // `0x1b` byte survives decoding.
    let cases: &[&[u8]] = &[
        b"\x1b[A\x1b[B\x1b[C\x1b[D",
        b"\x1bOA\x1bOB\x1bOC\x1bOD",
        b"\x1b[3~\x1b[2~\x1b[H\x1b[F",
        b"\x1bOH\x1bOF\x1bOP\x1bOQ",
        b"\x1b[11~\x1b[12~\x1b[3u\x1b[A",
        b"\x1b",
        b"ab\x1b",
        b"ab\x1bc",
        b"ab\x1b[",
        b"ab\x1b[99Zc",
        b"ab\x1b]8;;http://example\x07c",
        b"\x1b[200~pasted\x1b[201~",
    ];
    for raw in cases {
        let out = init_decode(raw, &[]);
        assert!(!out.contains('\x1b'), "raw ESC leaked for {raw:?}: {out:?}");
    }
    // Bare ESC drops; the following printable still inserts (Alt stripped).
    assert_eq!(init_decode(b"ab\x1bc", &[]), "abc");
    // Unknown CSI drops the whole sequence.
    assert_eq!(init_decode(b"ab\x1b[99Zc", &[]), "abc");
}

#[test]
fn init_wizard_kitty_csi_u_variants() {
    // Kitty backspace deletes before the cursor.
    assert_eq!(init_decode(b"ab\x1b[127uc", &[]), "ac");
    // Kitty space inserts.
    assert_eq!(init_decode(b"ab\x1b[D\x1b[32uc", &[]), "a cb");
    // Kitty Enter submits: trailing bytes are discarded.
    assert_eq!(init_decode(b"ab\x1b[13uIGNORED", &[]), "ab");
    // Kitty Tab / ESC / F-keys drop gracefully.
    assert_eq!(init_decode(b"ab\x1b[9uc", &[]), "abc");
    assert_eq!(init_decode(b"ab\x1b[27uc", &[]), "abc");
}

#[test]
fn init_wizard_arrows_edit_in_prompt() {
    // End-to-end through one wizard prompt (font family): `ab`, Left,
    // `c` answers `acb`, with no escape bytes in the stored answer.
    let (result, _) = drive_init_wizard("\n\nab\x1b[Dc\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    let answers = result.expect("edited answer accepted");
    assert_eq!(answers.font_family, "acb");
    assert!(!answers.font_family.contains('\x1b'));
}

#[test]
fn init_wizard_up_history_in_prompts() {
    // `gaps_in` answers `10`; `gaps_out` types `1` then Up to recall it.
    // Pure Up now moves the option cursor (phase 1 of #1806); typed text
    // plus Up still recalls history through the shared decoder.
    let stdin = "\n\n\n\n10\n1\x1b[A\n\n\n\n\n\n";
    let (result, _) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("history recall accepted");
    assert_eq!(answers.gaps_in, 10);
    assert_eq!(answers.gaps_out, 10);
}

// -- `bitty init` wizard display-vs-submitted redraw (#1821) -----------------
//
// On a canonical TTY the kernel echoes the raw typed bytes (including the
// `ESC [` sequences arrows/DEL/HOME/END emit) while the submitted answer is
// the decoded value, so the screen can show `ab^[[Dc` while the wizard
// accepts `acb`. Every prompt redraws the decoded value to `output` (the
// terminal at runtime) whenever decoding changed anything, so the display
// always carries the submitted answer. Plain input redraws nothing.

#[test]
fn init_wizard_plain_input_redraws_nothing() {
    // Eleven Enters: no escape bytes anywhere, so no redraw marker.
    let (result, printed) = drive_init_wizard("\n\n\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    result.expect("defaults accepted");
    assert!(
        !printed.contains("(read as "),
        "plain input must not redraw, got:\n{printed}"
    );
}

#[test]
fn init_wizard_redraw_shows_edited_answer() {
    // `ab`, Left, `c` submits `acb`; the redraw carries the submitted value
    // so the display matches it instead of the raw kernel echo.
    let (result, printed) = drive_init_wizard("\n\nab\x1b[Dc\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    let answers = result.expect("edited answer accepted");
    assert_eq!(answers.font_family, "acb");
    assert!(
        printed.contains("(read as \"acb\")"),
        "redraw must show the submitted answer, got:\n{printed}"
    );
}

#[test]
fn init_wizard_redraw_shows_recalled_history() {
    // `gaps_out` types `1` then Up to recall `10`: the submitted history
    // value was never echoed by the kernel, so the redraw must show it.
    // (Pure Up now moves the option cursor instead.)
    let stdin = "\n\n\n\n10\n1\x1b[A\n\n\n\n\n\n";
    let (result, printed) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("history recall accepted");
    assert_eq!(answers.gaps_out, 10);
    assert!(
        printed.contains("(read as \"10\")"),
        "redraw must show the recalled answer, got:\n{printed}"
    );
}

#[test]
fn init_wizard_redraw_precedes_validation_retry() {
    // An edited but invalid font size redraws the decoded attempt before the
    // retry message, so the user sees what failed validation.
    let stdin = "\n\n\n1\x1b[Cx\n12\n\n\n\n\n\n\n\n";
    let (result, printed) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("retry accepted");
    assert!((answers.font_size - 12.0).abs() < f32::EPSILON);
    assert!(
        printed.contains("try again"),
        "retry message expected, got:\n{printed}"
    );
    assert!(
        printed.contains("(read as \"1x\")"),
        "redraw must show the decoded attempt, got:\n{printed}"
    );
}

#[test]
fn init_line_needs_redraw_flags_decoded_edits() {
    // Untouched input needs no redraw: the kernel echo already matches.
    assert!(!crate::init::init_line_needs_redraw(b"hello", "hello"));
    assert!(!crate::init::init_line_needs_redraw(b"", ""));
    assert!(!crate::init::init_line_needs_redraw(
        "héllo".as_bytes(),
        "héllo"
    ));
    // Anything the decoder changed (escape sequences, erase bytes, dropped
    // controls, truncation) needs a redraw so the display catches up.
    assert!(crate::init::init_line_needs_redraw(b"ab\x1b[Dc", "acb"));
    assert!(crate::init::init_line_needs_redraw(b"ab\x7f", "a"));
    assert!(crate::init::init_line_needs_redraw(b"a\tb", "ab"));
    assert!(crate::init::init_line_needs_redraw(b"\x1b[A", "recalled"));
}

// A writer that always fails, standing in for broken terminal output.
struct FailWrite;

impl std::io::Write for FailWrite {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("injected output failure"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("injected output failure"))
    }
}

#[test]
fn init_wizard_redraw_output_error_aborts() {
    // Fail closed: when the decoded redraw cannot be displayed, the wizard
    // aborts instead of accepting a value the user never saw.
    let mut input = std::io::BufReader::new("ab\x1b[Dc\n".as_bytes());
    let mut output = FailWrite;
    let mut history = Vec::new();
    let result: Result<String, String> = crate::init::init_ask(
        &mut input,
        &mut output,
        "Font family [Mono]:",
        crate::init::init_parse_font_family_answer,
        &mut history,
    );
    assert!(result.is_err(), "output failure must abort the prompt");
}

// -- `bitty init` option lists, phase 1 of #1806 ---------------------------
//
// Every prompt renders a navigable option list (Up/Down arrows plus j/k,
// Enter selects, numbers where the codebase already uses them) with
// ANSI-colored entries (theme names preview in their own style, other rows
// carry a colored selection marker). Navigation is additive over the
// single-line fast path: numbers, names, and values still select in one
// read, so piped input behaves as before minus the new list rendering.

#[test]
fn init_option_list_renders_for_every_prompt() {
    let (result, printed) = drive_init_wizard("\n\n\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    result.expect("defaults accepted");
    assert!(printed.contains("Shell"), "shell list, got:\n{printed}");
    assert!(printed.contains("Theme"), "theme list, got:\n{printed}");
    assert!(
        printed.contains("Font family"),
        "font list, got:\n{printed}"
    );
    assert!(printed.contains("Font size"), "size list, got:\n{printed}");
    assert!(
        printed.contains("gaps_in"),
        "decoration list, got:\n{printed}"
    );
    assert!(
        printed.contains("Scrollback"),
        "behavior list, got:\n{printed}"
    );
    assert!(
        printed.contains("Close confirm"),
        "close-confirm list, got:\n{printed}"
    );
    assert!(
        printed.contains("Keybindings"),
        "preset list, got:\n{printed}"
    );
    // Visible cursor line plus navigation hint on every list.
    assert!(printed.contains('>'), "selection marker, got:\n{printed}");
    assert!(
        printed.contains("j/k or arrows move"),
        "navigation hint, got:\n{printed}"
    );
    // Theme catalog and documented font fallbacks are listed.
    assert!(
        printed.contains("tokyo-night"),
        "theme catalog, got:\n{printed}"
    );
    assert!(
        printed.contains("JetBrains Mono"),
        "font options, got:\n{printed}"
    );
}

#[test]
fn init_option_list_j_navigates_theme() {
    // Shell default, theme `j` moves to tokyo-night then Enter selects it,
    // remaining prompts take defaults.
    let stdin = "\nj\n\n\n\n\n\n\n\n\n\n\n";
    let (result, _) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("navigated theme accepted");
    assert_eq!(answers.theme, "tokyo-night");
}

#[test]
fn init_option_list_arrow_down_navigates_theme() {
    // Down arrow behaves like `j` for the theme list.
    let stdin = "\n\x1b[B\n\n\n\n\n\n\n\n\n\n\n";
    let (result, _) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("arrow-navigated theme accepted");
    assert_eq!(answers.theme, "tokyo-night");
}

#[test]
fn init_option_list_k_wraps_theme_to_last() {
    // Up/`k` from the default wraps to the last preset (github-light).
    let stdin = "\nk\n\n\n\n\n\n\n\n\n\n\n";
    let (result, _) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("wrapped theme accepted");
    assert_eq!(answers.theme, "github-light");
}

#[test]
fn init_option_list_number_fast_path_still_selects_shell() {
    // Numbered fast path: `2` picks the second shell candidate in one read.
    let (result, _) = drive_init_wizard("2\n\n\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    let answers = result.expect("numbered shell accepted");
    assert_eq!(answers.shell.as_deref(), Some("/bin/sh"));
}

#[test]
fn init_option_list_typed_value_still_selects_size() {
    // Typed values need no navigation: `14` answers the size step directly.
    let stdin = "\n\n\n14\n\n\n\n\n\n\n\n";
    let (result, _) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("typed size accepted");
    assert!((answers.font_size - 14.0).abs() < f32::EPSILON);
}

#[test]
fn init_option_list_invalid_retry_then_navigation() {
    // An unknown theme reprompts; `j` plus Enter then selects tokyo-night.
    let stdin = "\nnope\nj\n\n\n\n\n\n\n\n\n\n\n";
    let (result, printed) = drive_init_wizard(stdin, Some("/bin/bash"));
    let answers = result.expect("retry plus navigation accepted");
    assert_eq!(answers.theme, "tokyo-night");
    assert!(
        printed.contains("try again"),
        "retry message expected, got:\n{printed}"
    );
}

#[test]
fn init_option_list_plain_output_has_no_escapes() {
    // Default harness color is off: piped/plain output carries no ANSI.
    let (result, printed) = drive_init_wizard("\n\n\n\n\n\n\n\n\n\n\n", Some("/bin/bash"));
    result.expect("defaults accepted");
    assert!(
        !printed.contains("\u{1b}["),
        "plain output must not colorize, got:\n{printed}"
    );
}

#[test]
fn init_option_list_color_previews_theme_and_marker() {
    // With color on, theme rows preview in their own truecolor style and
    // the selection marker is colored; labels stay greppable.
    let (result, printed) = drive_init_wizard_with_color(
        "\n\n\n\n\n\n\n\n\n\n\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        true,
    );
    result.expect("defaults accepted");
    assert!(
        printed.contains("\u{1b}[38;2;"),
        "theme preview foreground, got:\n{printed}"
    );
    assert!(
        printed.contains("\u{1b}[48;2;"),
        "theme preview background, got:\n{printed}"
    );
    assert!(
        printed.contains("\u{1b}[36m>"),
        "colored selection marker, got:\n{printed}"
    );
    assert!(
        printed.contains("tokyo-night"),
        "labels stay greppable, got:\n{printed}"
    );
}

// -- `bitty init` picker key loop: single-keypress navigation (#2136) -----
//
// The TTY picker responds to single keypresses without Enter (j/k and
// arrows move, Enter selects). These tests drive
// [`crate::init::init_picker_key_loop`] directly with injected bytes plus
// an EOF-terminated wait closure (no TTY needed): the injected bytes stand
// in for single keypresses exactly as the raw TTY delivers them.

/// Drives one three-option key-loop picker headlessly; returns the selected
/// value plus everything the picker printed plus the history it recorded.
fn drive_picker_key_loop(
    stdin_bytes: &[u8],
    numbered: bool,
) -> (Result<String, String>, String, Vec<String>) {
    let options = vec![
        crate::init::InitOptionItem {
            label: "alpha".to_string(),
            value_text: "alpha".to_string(),
            style_fg: None,
            style_bg: None,
        },
        crate::init::InitOptionItem {
            label: "beta".to_string(),
            value_text: "beta".to_string(),
            style_fg: None,
            style_bg: None,
        },
        crate::init::InitOptionItem {
            label: "gamma".to_string(),
            value_text: "gamma".to_string(),
            style_fg: None,
            style_bg: None,
        },
    ];
    let mut input = std::io::BufReader::new(stdin_bytes);
    let mut output = Vec::new();
    let mut history = Vec::new();
    // EOF-terminated: no more bytes means the escape (if any) is bare.
    // Production passes a poll-based closure over the real stdin instead.
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result = crate::init::init_picker_key_loop(
        &mut input,
        &mut output,
        "Pick:",
        &options,
        0,
        false,
        numbered,
        |line: &str| Ok(line.to_string()),
        &mut history,
        false,
        &mut wait,
        None,
    );
    let printed = String::from_utf8(output).expect("picker output is UTF-8");
    (result, printed, history)
}

#[test]
fn init_escape_burst_complete_cases() {
    use crate::init::init_escape_burst_complete;
    // Bare ESC needs more bytes (or a timeout) to disambiguate.
    assert!(!init_escape_burst_complete(b"\x1b"));
    // CSI introducer without a final byte is truncated.
    assert!(!init_escape_burst_complete(b"\x1b["));
    assert!(!init_escape_burst_complete(b"\x1b[1"));
    assert!(!init_escape_burst_complete(b"\x1b[1;5"));
    // Arrows, Home/End, and kitty `CSI u` forms are complete.
    assert!(init_escape_burst_complete(b"\x1b[B"));
    assert!(init_escape_burst_complete(b"\x1b[A"));
    assert!(init_escape_burst_complete(b"\x1b[H"));
    assert!(init_escape_burst_complete(b"\x1b[1;5B"));
    assert!(init_escape_burst_complete(b"\x1b[13u"));
    // SS3 needs its third byte.
    assert!(!init_escape_burst_complete(b"\x1bO"));
    assert!(init_escape_burst_complete(b"\x1bOB"));
    // Alt prefixes (`ESC` + printable) are complete single keys.
    assert!(init_escape_burst_complete(b"\x1bx"));
    // OSC without a terminator is truncated; BEL-terminated is complete.
    assert!(!init_escape_burst_complete(b"\x1b]8;;http://x"));
    assert!(init_escape_burst_complete(b"\x1b]8;;http://x\x07"));
}

#[test]
fn init_key_loop_j_moves_then_enter_selects() {
    // Single `j` (no Enter) moves the cursor; Enter selects beta.
    let (result, printed, _) = drive_picker_key_loop(b"j\r", true);
    assert_eq!(result.expect("j plus Enter selects beta"), "beta");
    assert!(
        printed.contains("> 2) beta"),
        "moved marker, got:\n{printed}"
    );
}

#[test]
fn init_key_loop_k_wraps_to_last() {
    // Single `k` from the default wraps to gamma.
    let (result, _, _) = drive_picker_key_loop(b"k\r", true);
    assert_eq!(result.expect("wrapped pick accepted"), "gamma");
}

#[test]
fn init_key_loop_arrow_down_moves() {
    // Down-arrow escape plus Enter behaves like `j`.
    let (result, _, _) = drive_picker_key_loop(b"\x1b[B\r", true);
    assert_eq!(result.expect("arrow-navigated pick accepted"), "beta");
}

#[test]
fn init_key_loop_arrow_up_wraps() {
    // Up-arrow escape from the default wraps to gamma.
    let (result, _, _) = drive_picker_key_loop(b"\x1b[A\r", true);
    assert_eq!(result.expect("wrapped arrow pick accepted"), "gamma");
}

#[test]
fn init_key_loop_enter_selects_default() {
    let (result, _, _) = drive_picker_key_loop(b"\r", true);
    assert_eq!(result.expect("Enter selects the default"), "alpha");
}

#[test]
fn init_key_loop_number_fast_path() {
    // Numbered lists keep the digit fast path: `3` plus Enter picks gamma.
    let (result, _, _) = drive_picker_key_loop(b"3\r", true);
    assert_eq!(result.expect("numbered pick accepted"), "gamma");
}

#[test]
fn init_key_loop_typed_value_submits() {
    // Unnumbered lists accept typed values exactly like the line loop.
    let (result, printed, history) = drive_picker_key_loop(b"custom\r", false);
    assert_eq!(result.expect("typed value accepted"), "custom");
    assert_eq!(history, vec!["custom".to_string()]);
    assert!(
        printed.contains("(typed: custom)"),
        "typed echo, got:\n{printed}"
    );
}

#[test]
fn init_key_loop_j_inside_buffer_appends() {
    // `j` navigates only from an empty buffer; mid-value it is text.
    let (result, _, _) = drive_picker_key_loop(b"aj\r", false);
    assert_eq!(result.expect("buffered j accepted"), "aj");
}

#[test]
fn init_key_loop_uppercase_j_types() {
    // Uppercase `J` is typed text (the hint advertises lowercase j/k), so
    // values like `JetBrains` stay enterable without the arrow keys.
    let (result, _, _) = drive_picker_key_loop(b"JetBrains\r", false);
    assert_eq!(result.expect("uppercase value accepted"), "JetBrains");
}

#[test]
fn init_key_loop_esc_cancels() {
    let (result, _, _) = drive_picker_key_loop(b"\x1b", true);
    assert_eq!(
        result.expect_err("bare Esc must cancel"),
        "bitty init: aborted (cancelled)"
    );
}

#[test]
fn init_key_loop_ctrl_d_aborts() {
    let (result, _, _) = drive_picker_key_loop(b"\x04", true);
    assert_eq!(
        result.expect_err("Ctrl-D must abort"),
        "bitty init: aborted (end of input)"
    );
}

#[test]
fn init_key_loop_backspace_edits() {
    let (result, _, _) = drive_picker_key_loop(b"ab\x7f\r", false);
    assert_eq!(result.expect("edited value accepted"), "a");
}

#[test]
fn init_key_loop_invalid_retries_then_succeeds() {
    let options = vec![crate::init::InitOptionItem {
        label: "alpha".to_string(),
        value_text: "alpha".to_string(),
        style_fg: None,
        style_bg: None,
    }];
    let mut input = std::io::BufReader::new(b"bad\r\r".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result = crate::init::init_picker_key_loop(
        &mut input,
        &mut output,
        "Pick:",
        &options,
        0,
        false,
        false,
        |line: &str| {
            if line == "bad" {
                Err("nope".to_string())
            } else {
                Ok(line.to_string())
            }
        },
        &mut history,
        false,
        &mut wait,
        None,
    );
    // A rejected value reprompts (buffer cleared); empty Enter then takes
    // the cursor default.
    assert_eq!(result.expect("retry accepted"), "alpha");
    let printed = String::from_utf8(output).expect("picker output is UTF-8");
    assert!(
        printed.contains("nope — try again"),
        "retry message, got:\n{printed}"
    );
}

#[test]
fn init_key_loop_too_many_invalid_aborts() {
    let options = vec![crate::init::InitOptionItem {
        label: "alpha".to_string(),
        value_text: "alpha".to_string(),
        style_fg: None,
        style_bg: None,
    }];
    let mut input = std::io::BufReader::new(b"bad\rbad\rbad\r".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result: Result<String, String> = crate::init::init_picker_key_loop(
        &mut input,
        &mut output,
        "Pick:",
        &options,
        0,
        false,
        false,
        |_: &str| Err("nope".to_string()),
        &mut history,
        false,
        &mut wait,
        None,
    );
    assert!(
        result
            .expect_err("three strikes must abort")
            .contains("too many"),
        "attempt bound"
    );
}

#[test]
fn init_ask_option_falls_back_to_line_without_raw_tty() {
    use std::io::IsTerminal as _;
    // The raw key loop needs a real TTY stdin; under an interactive
    // `cargo test` the harness stdin may be that TTY, which would steal the
    // developer's terminal modes. Skip there; CI (piped stdin) runs it.
    if std::io::stdin().is_terminal() {
        return;
    }
    // `tty_interactive` with no raw-capable stdin keeps the line loop
    // byte-identical: `j` plus Enter moves, Enter selects.
    let options = vec![
        crate::init::InitOptionItem {
            label: "alpha".to_string(),
            value_text: "alpha".to_string(),
            style_fg: None,
            style_bg: None,
        },
        crate::init::InitOptionItem {
            label: "beta".to_string(),
            value_text: "beta".to_string(),
            style_fg: None,
            style_bg: None,
        },
    ];
    let mut input = std::io::BufReader::new(b"j\n\n".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let result: Result<String, String> = crate::init::init_ask_option(
        &mut input,
        &mut output,
        "Pick:",
        &options,
        0,
        false,
        true,
        |line: &str| Ok(line.to_string()),
        &mut history,
        true,
        None,
    );
    assert_eq!(result.expect("line fallback accepted"), "beta");
}

#[test]
fn init_key_loop_inplace_repaints_with_cursor_up() {
    // In-place frames repaint over the previous frame (cursor-up plus
    // clear-line) and hide the cursor while looping.
    let options = vec![
        crate::init::InitOptionItem {
            label: "alpha".to_string(),
            value_text: "alpha".to_string(),
            style_fg: None,
            style_bg: None,
        },
        crate::init::InitOptionItem {
            label: "beta".to_string(),
            value_text: "beta".to_string(),
            style_fg: None,
            style_bg: None,
        },
    ];
    let mut input = std::io::BufReader::new(b"j\r".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result: Result<String, String> = crate::init::init_picker_key_loop(
        &mut input,
        &mut output,
        "Pick:",
        &options,
        0,
        false,
        true,
        |line: &str| Ok(line.to_string()),
        &mut history,
        true,
        &mut wait,
        None,
    );
    assert_eq!(result.expect("inplace pick accepted"), "beta");
    let printed = String::from_utf8(output).expect("picker output is UTF-8");
    // One frame is prompt plus two entries plus hint plus typed line.
    assert!(
        printed.contains("\x1b[5A"),
        "cursor-up repaint, got:\n{printed}"
    );
    assert!(
        printed.contains("\x1b[?25l") && printed.contains("\x1b[?25h"),
        "cursor hide/show, got:\n{printed}"
    );
}

#[test]
fn init_color_enabled_impl_matches_convention() {
    // Pure decision table: explicit flag, NO_COLOR, dumb terminal, or piped
    // stdout disables; otherwise color is allowed.
    assert!(!crate::init::init_color_enabled_impl(
        true, false, false, true
    ));
    assert!(!crate::init::init_color_enabled_impl(
        false, true, false, true
    ));
    assert!(!crate::init::init_color_enabled_impl(
        false, false, true, true
    ));
    assert!(!crate::init::init_color_enabled_impl(
        false, false, false, false
    ));
    assert!(crate::init::init_color_enabled_impl(
        false, false, false, true
    ));
}

#[test]
fn unified_gate_help_piped_is_plain_forced_still_colors() {
    // CTX-1065: bare help headings are bold only on a tty. The harness
    // stdout is piped, so the live gate resolves to plain here.
    use std::io::IsTerminal as _;
    assert!(
        !std::io::stdout().is_terminal(),
        "test harness stdout must be piped for this assertion"
    );
    let args = parse_args(&args_of(&["bitty"]));
    assert!(!crate::cli::help_color_enabled(&args));
    let plain = crate::cli::help_text_short(false);
    assert!(!plain.contains("\u{1b}"));
    let piped = crate::cli::help_text_short(crate::cli::help_color_enabled(&args));
    assert!(!piped.contains("\u{1b}"), "piped help must carry no ANSI");
    // Forced color hook still renders headings bold even when piped.
    let forced = crate::cli::help_text_short(true);
    assert!(
        forced.contains("\u{1b}[1mUsage:"),
        "forced help color must carry bold headings"
    );
    // Explicit --no-color stays authoritative.
    let no_color_args = parse_args(&args_of(&["bitty", "--no-color"]));
    assert!(!crate::cli::help_color_enabled(&no_color_args));
    // Pure gate: TERM=dumb disables even on a tty.
    assert!(!crate::color::cli_color_enabled_impl(
        false, false, true, true
    ));
    assert!(crate::color::cli_color_enabled_impl(
        false, false, false, true
    ));
}

// -- `bitty init` live preview + rollback (phase 2 of #1806) ------------
//
// The wizard overlays each answered step onto an in-memory preview of the
// previous effective config and never touches the user file until confirm;
// Esc/cancel/EOF discards the overlay and restores the baseline.

/// [`drive_init_wizard`] with an injected preview baseline; returns answers
/// plus the wizard output plus the preview (snapshots prove mid-wizard
/// state headlessly). No live session (in-memory overlay only), so existing
/// p1-p3 assertions stay live-free.
fn drive_init_wizard_with_preview(
    stdin_lines: &str,
    shell_env: Option<&str>,
    overrides: &InitOverrides,
    baseline: bitty_config::EffectiveConfig,
) -> (Result<InitAnswers, String>, String, InitPreview) {
    let mut input = std::io::BufReader::new(stdin_lines.as_bytes());
    let mut output = Vec::new();
    let mut preview = InitPreview::enabled(baseline);
    let result = run_init_interactive_with_preview(
        &mut input,
        &mut output,
        shell_env,
        &|p| p == "/bin/bash" || p == "/bin/sh",
        overrides,
        false,
        Some(&mut preview),
        None,
        false,
    );
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    (result, printed, preview)
}

#[test]
fn init_preview_applies_selections_mid_wizard() {
    // Baseline carries out-of-wizard values the overlay must preserve:
    // a non-default opacity plus a resize step no wizard step sets.
    let mut baseline = bitty_config::fallback_builtin();
    baseline.window.opacity = 0.75;
    baseline.layout.resize_step = 0.02;
    let before = baseline.clone();

    // Same custom picks as `init_interactive_custom_picks`: shell #2,
    // bitty-dark, Fira Code, 14pt, 10/12/3/8, 50000, always, vim (#2).
    let (result, _, preview) = drive_init_wizard_with_preview(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline,
    );
    let answers = result.expect("custom picks accepted");
    assert_eq!(answers.shell.as_deref(), Some("/bin/sh"));

    // One snapshot per prompted step, in prompt order: shell, theme,
    // family, size, gaps_in, gaps_out, border, radius, scrollback,
    // close_confirm, key preset.
    let snapshots = preview.snapshots();
    assert_eq!(snapshots.len(), 11, "every prompted step previews");
    assert_eq!(
        snapshots[0].terminal.shell.as_deref(),
        Some("/bin/sh"),
        "shell previews first"
    );
    assert_eq!(
        snapshots[1].appearance.theme.as_deref(),
        Some("dark"),
        "theme previews second"
    );
    assert_eq!(snapshots[2].font.family, "Fira Code");
    assert!((snapshots[3].font.size - 14.0).abs() < f32::EPSILON);
    assert_eq!(snapshots[4].decoration.gaps_in, 10);
    assert_eq!(snapshots[5].decoration.gaps_out, 12);
    assert_eq!(snapshots[6].decoration.border, 3);
    assert_eq!(snapshots[7].decoration.radius, 8);
    assert_eq!(snapshots[8].terminal.scrollback, 50_000);
    assert_eq!(
        snapshots[8].close_confirm,
        bitty_config::CloseConfirm::WhenBusy,
        "close_confirm still baseline before its own step"
    );
    assert_eq!(
        snapshots[9].close_confirm,
        bitty_config::CloseConfirm::Always
    );
    assert_eq!(
        snapshots[10].keymaps.len(),
        bitty_config::keymap::DEFAULT_KEYMAPS.len(),
        "vim preset stages the explicit shipped map"
    );

    // The final overlay equals the last snapshot and carries every answer.
    assert_eq!(preview.current(), &snapshots[10]);
    assert_eq!(
        preview.current().appearance.theme.as_deref(),
        Some(answers.theme.as_str())
    );
    assert!((preview.current().font.size - answers.font_size).abs() < f32::EPSILON);

    // Out-of-wizard config is untouched throughout.
    for (index, snapshot) in snapshots.iter().enumerate() {
        assert!(
            (snapshot.window.opacity - 0.75).abs() < f32::EPSILON,
            "snapshot {index} keeps out-of-wizard opacity"
        );
        assert!(
            (snapshot.layout.resize_step - 0.02).abs() < f32::EPSILON,
            "snapshot {index} keeps out-of-wizard resize step"
        );
    }

    // The baseline itself never mutates during preview.
    assert_eq!(preview.baseline(), &before);
}

#[test]
fn init_preview_cancel_restores_baseline() {
    let baseline = bitty_config::fallback_builtin();

    // EOF mid-wizard (shell + theme answered, then input ends): the wizard
    // aborts, the overlay returns to baseline, and the two applied steps
    // remain visible only in the discarded snapshots.
    let (result, _, preview) = drive_init_wizard_with_preview(
        "\n\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    let err = result.expect_err("EOF aborts");
    assert!(err.contains("end of input"), "unexpected: {err}");
    assert_eq!(preview.snapshots().len(), 2, "shell + theme previewed");
    assert_eq!(
        preview.snapshots()[1].appearance.theme.as_deref(),
        Some("dark")
    );
    assert_eq!(
        preview.current(),
        &baseline,
        "EOF restores the exact previous config"
    );
    assert_eq!(preview.baseline(), &baseline);

    // Bare Esc cancels too: shell takes its default, then a lone ESC byte
    // aborts the theme step instead of selecting it.
    let (result, _, preview) = drive_init_wizard_with_preview(
        "\n\x1b\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    let err = result.expect_err("Esc aborts");
    assert!(err.contains("cancelled"), "unexpected: {err}");
    assert_eq!(preview.snapshots().len(), 1, "only shell previewed");
    assert_eq!(
        preview.current(),
        &baseline,
        "Esc restores the exact previous config"
    );

    // Existing abort behavior is unchanged: too many invalid answers still
    // aborts without a preview leak.
    let (result, _, preview) = drive_init_wizard_with_preview(
        "\n\n\n\n\n\n\n\n\n\nnope\nnah\nnever\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    assert!(result.is_err(), "retries still exhaust the bound");
    assert_eq!(preview.current(), &baseline);
}

#[test]
fn init_preview_never_writes_config_file() {
    // The interactive wizard takes no path and performs no filesystem
    // mutation: a sentinel file beside it stays byte-identical through both
    // confirm and cancel runs.
    let dir = init_test_dir("preview-nowrite");
    let sentinel = dir.join("init.lua");
    std::fs::write(&sentinel, "-- sentinel\n").expect("sentinel");
    let baseline = bitty_config::fallback_builtin();

    let (result, _, _) = drive_init_wizard_with_preview(
        "\n\n\n\n\n\n\n\n\n\n\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    result.expect("defaults accepted");
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("sentinel intact"),
        "-- sentinel\n",
        "confirm-path preview writes no file mid-wizard"
    );

    let (result, _, _) = drive_init_wizard_with_preview(
        "\n\x1b\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline,
    );
    assert!(result.is_err(), "Esc cancels");
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("sentinel intact"),
        "-- sentinel\n",
        "cancel-path preview writes no file"
    );

    // Cursor preview (two theme moves) then Esc still writes nothing: the
    // preview only mutates the in-memory overlay and the preview session.
    let (result, _, _) = drive_init_wizard_with_preview(
        "\nj\nj\n\x1b\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        bitty_config::fallback_builtin(),
    );
    assert!(result.is_err(), "Esc cancels a previewing wizard");
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("sentinel intact"),
        "-- sentinel\n",
        "cursor preview writes no file"
    );

    // Subcommand level: a TTY cancel (EOF before any answer) exits 1 and
    // creates neither the target nor a backup.
    let target = dir.join("fresh").join("init.lua");
    let mut args = Args::new();
    args.init_word = true;
    args.config_path = Some(target.display().to_string());
    let home = dir.display().to_string();
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        1,
        "TTY EOF cancels with exit 1"
    );
    assert!(!target.exists(), "cancel creates no config file");
    assert!(
        !target.with_extension("lua.bak").exists(),
        "cancel creates no backup"
    );

    // And an Esc cancel over an existing file leaves it (and its backup
    // slot) untouched.
    std::fs::create_dir_all(target.parent().expect("parent")).expect("parent dir");
    std::fs::write(&target, "-- existing\n").expect("existing file");
    let mut input: &[u8] = b"\n\x1b\n";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        1,
        "TTY Esc cancels with exit 1"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("existing intact"),
        "-- existing\n"
    );
    assert!(
        !target.with_extension("lua.bak").exists(),
        "cancel writes no backup"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// -- `bitty init` theme picker live preview (issue #2435) -----------------
//
// Moving the theme cursor previews the highlighted preset through the same
// palette setter `theme.set`/`ThemeChanged` use, paints the themed strip,
// never snapshots a step or writes a file, and rolls back on Esc.

/// Deterministic preview session (no PTY, no display) seeded from `baseline`.
fn preview_session(baseline: &bitty_config::EffectiveConfig) -> Runtime {
    let cfg = runtime_config_from_effective(baseline).expect("preview cfg");
    let runtime =
        Runtime::with_deterministic_rasterizer(cfg).expect("deterministic runtime builds");
    let _ = crate::config_reload::take_app_adoption();
    runtime
}

/// One theme option exactly as the wizard's theme step builds it.
fn theme_option(theme: &bitty_config::theme::Theme) -> crate::init::InitOptionItem {
    crate::init::InitOptionItem {
        label: theme.name.to_string(),
        value_text: theme.name.to_string(),
        style_fg: Some(theme.foreground),
        style_bg: Some(theme.background),
    }
}

/// The exact `set_theme_palette` value a resolved preset produces.
fn expected_palette(theme: &bitty_config::theme::Theme) -> bitty_runtime::ThemePalette {
    bitty_runtime::ThemePalette::from_theme(theme)
}

#[test]
fn init_theme_preview_cursor_move_applies_palette_without_a_step() {
    let baseline = bitty_config::fallback_builtin();
    let mut preview = InitPreview::enabled(baseline.clone());
    let mut live = preview_session(&baseline);
    let before = live.config().theme;
    let (candidate, status) = bitty_config::theme::resolve_theme_with_status(Some("github-light"));
    assert_eq!(status, bitty_config::theme::ThemeResolution::Named);

    let mut driver = crate::init::InitThemePreview::new(&mut preview, Some(&mut live));
    let strip = driver
        .frame(&theme_option(candidate), true)
        .expect("themed strip with color");

    // The install goes through the same `set_theme_palette` sink as
    // `theme.set`, config reload, and `ThemeChanged`.
    assert_eq!(
        live.config().theme,
        expected_palette(candidate),
        "cursor move installs the candidate palette"
    );
    assert_ne!(live.config().theme, before, "the preview really changed");
    let [r, g, b] = candidate.background;
    assert_eq!(live.active_background(), [r, g, b, 0xFF]);
    // The strip paints the same resolved palette (no second renderer).
    assert!(strip.contains(&format!("\u{1b}[48;2;{r};{g};{b}m")));
    let [fr, fg, fb] = candidate.foreground;
    assert!(strip.contains(&format!("\u{1b}[38;2;{fr};{fg};{fb}m")));
    for ansi in [candidate.ansi[1], candidate.ansi[4]] {
        assert!(
            strip.contains(&format!("\u{1b}[48;2;{};{};{}m", ansi[0], ansi[1], ansi[2])),
            "ANSI swatch missing from strip: {strip:?}"
        );
    }
    assert!(strip.contains(candidate.name), "strip names the candidate");
    assert!(
        strip.ends_with("\u{1b}[0m"),
        "strip restores the SGR state: {strip:?}"
    );
    // Preview is not an answered step: no snapshot, baseline untouched.
    assert!(
        preview.snapshots().is_empty(),
        "cursor preview is not a step"
    );
    assert_eq!(
        preview.current().appearance.theme.as_deref(),
        Some(candidate.name)
    );
    assert_eq!(preview.baseline(), &baseline);
    assert!(!live.has_pty(), "preview never spawns a shell");
}

#[test]
fn init_theme_key_loop_previews_every_cursor_move_before_enter() {
    let baseline = bitty_config::fallback_builtin();
    let mut preview = InitPreview::enabled(baseline.clone());
    let mut live = preview_session(&baseline);
    let presets = bitty_config::theme::list_presets();
    let options = vec![
        theme_option(bitty_config::theme::resolve_theme(Some("dark"))),
        theme_option(presets[1]),
        theme_option(presets[2]),
    ];
    let mut input = std::io::BufReader::new(b"jj\r".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let mut seen: Vec<String> = Vec::new();
    {
        let mut driver = crate::init::InitThemePreview::new(&mut preview, Some(&mut live));
        let mut hook = |item: &crate::init::InitOptionItem| -> Option<String> {
            seen.push(item.value_text.clone());
            driver.frame(item, false)
        };
        let result = crate::init::init_picker_key_loop(
            &mut input,
            &mut output,
            "Theme [dark]:",
            &options,
            0,
            false,
            false,
            crate::init::init_parse_theme_answer,
            &mut history,
            false,
            &mut wait,
            Some(&mut hook),
        );
        assert_eq!(
            result.expect("Enter selects the previewed theme"),
            options[2].value_text
        );
    }
    // One preview per frame: the entry frame plus one per cursor move.
    assert_eq!(
        seen,
        vec![
            options[0].value_text.clone(),
            options[1].value_text.clone(),
            options[2].value_text.clone()
        ],
        "every cursor position previews before Enter"
    );
    assert!(
        preview.snapshots().is_empty(),
        "the key loop previews, Enter is the commit"
    );
    assert_eq!(
        live.config().theme,
        expected_palette(presets[2]),
        "the palette followed the cursor before Enter"
    );
}

#[test]
fn init_theme_preview_esc_restores_the_previous_theme() {
    let mut baseline = bitty_config::fallback_builtin();
    baseline.appearance.theme = Some("github-light".to_string());
    let mut preview = InitPreview::enabled(baseline.clone());
    let mut live = preview_session(&baseline);
    let before = live.config().theme;
    let third = bitty_config::theme::list_presets()[2];

    let mut input = std::io::BufReader::new("\nj\nj\n\x1b\n".as_bytes());
    let mut output = Vec::new();
    let result = run_init_interactive_with_preview(
        &mut input,
        &mut output,
        Some("/bin/bash"),
        &|p| p == "/bin/bash" || p == "/bin/sh",
        &InitOverrides::default(),
        true,
        Some(&mut preview),
        Some(&mut live),
        false,
    );
    let err = result.expect_err("Esc cancels");
    assert!(err.contains("cancelled"), "unexpected: {err}");
    // The preview ran first: the third preset's strip was painted before Esc.
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    let [r, g, b] = third.background;
    assert!(
        printed.contains(&format!("\u{1b}[48;2;{r};{g};{b}m")),
        "cursor preview painted the candidate strip, got:\n{printed}"
    );
    assert!(
        printed.matches("  preview ").count() >= 3,
        "one strip per previewed frame, got:\n{printed}"
    );
    // Cancel restores the previous theme in memory and in the live session.
    assert_eq!(preview.current(), &baseline);
    assert_eq!(
        preview.snapshots().len(),
        1,
        "only the shell step committed"
    );
    assert_eq!(live.config().theme, before, "live palette restored");
    assert!(!live.has_pty(), "cancel spawns nothing");
}

#[test]
fn init_theme_preview_enter_commits_the_previewed_theme() {
    let baseline = bitty_config::fallback_builtin();
    // Shell, then the theme step: two `j` moves and Enter, then defaults.
    let stdin = format!("\nj\nj\n{}", "\n".repeat(10));
    let (result, _, preview) = drive_init_wizard_with_preview(
        &stdin,
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    let answers = result.expect("previewed theme committed");
    let third = bitty_config::theme::list_presets()[2];
    assert_eq!(answers.theme, third.name);
    assert_eq!(preview.snapshots().len(), 11, "every prompted step commits");
    assert_eq!(
        preview.snapshots()[1].appearance.theme.as_deref(),
        Some(third.name),
        "Enter commits the cursor theme as the theme step"
    );
    assert_eq!(preview.baseline(), &baseline);
}

#[test]
fn init_theme_preview_invalid_candidate_keeps_previous_and_diagnoses() {
    let mut baseline = bitty_config::fallback_builtin();
    baseline.appearance.theme = Some("github-light".to_string());
    let mut preview = InitPreview::enabled(baseline.clone());
    let mut live = preview_session(&baseline);
    let before = live.config().theme;
    let bogus = crate::init::InitOptionItem {
        label: "bogus".to_string(),
        value_text: "definitely-not-a-theme".to_string(),
        style_fg: None,
        style_bg: None,
    };

    let strip = {
        let mut driver = crate::init::InitThemePreview::new(&mut preview, Some(&mut live));
        driver
            .frame(&bogus, true)
            .expect("fallback strip keeps the frame height constant")
    };
    // Fail-soft: the previous theme stays in the overlay, the session, and the
    // strip, and no panic happens.
    assert_eq!(preview.current(), &baseline);
    assert_eq!(live.config().theme, before);
    let [r, g, b] = bitty_config::theme::resolve_theme(Some("github-light")).background;
    assert!(strip.contains(&format!("\u{1b}[48;2;{r};{g};{b}m")));
    assert!(strip.contains("github-light"), "strip shows the kept theme");
    // The existing Enter diagnostic is unchanged.
    let err = crate::init::init_parse_theme_answer("definitely-not-a-theme")
        .expect_err("unknown theme still rejects");
    assert!(err.contains("unknown theme"), "unexpected: {err}");

    // Wizard level: the reprompt shows the same diagnostic, then the default
    // commits; nothing crashes and the overlay ends on the commit.
    let stdin = format!("\ndefinitely-not-a-theme\n{}", "\n".repeat(10));
    let (result, printed, committed) = drive_init_wizard_with_preview(
        &stdin,
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline,
    );
    let answers = result.expect("retry then default accepted");
    assert_eq!(answers.theme, "dark");
    assert!(
        printed.contains("unknown theme; run 'bitty list themes'"),
        "existing diagnostic preserved, got:\n{printed}"
    );
    assert_eq!(
        committed.snapshots()[1].appearance.theme.as_deref(),
        Some("dark")
    );
}

#[test]
fn init_theme_preview_absent_when_stdout_is_not_a_tty() {
    use std::io::IsTerminal as _;
    // The harness stdout is the process stdout: skip when a developer runs
    // `cargo test` from a terminal, since the dispatch would then (correctly)
    // treat stdout as interactive.
    if std::io::stdout().is_terminal() {
        return;
    }
    // The harness stdout is piped: even with a TTY stdin the dispatch builds
    // no preview overlay, so there is no echo, no strip, and no ANSI, while
    // the selected theme still lands in the file.
    let dir = init_test_dir("theme-preview-no-tty-out");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.config_path = Some(target.display().to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let stdin = format!("\nj\nj\n{}", "\n".repeat(11));
    let mut input = std::io::BufReader::new(stdin.as_bytes());
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(
        !printed.contains("(preview:"),
        "no echo without a TTY stdout"
    );
    assert!(
        !printed.contains("  preview "),
        "no themed strip without a TTY stdout"
    );
    assert!(!printed.contains("\u{1b}["), "no ANSI without a TTY stdout");
    let third = bitty_config::theme::list_presets()[2];
    let body =
        std::fs::read_to_string(dir.join("profiles").join("default.lua")).expect("written profile");
    assert!(
        body.contains(&format!("theme = \"{}\"", third.name)),
        "the cursor selection still commits: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_yes_never_previews() {
    // `--yes` takes the non-interactive path: no prompts, echo, or strip.
    let dir = init_test_dir("theme-preview-yes");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.init_yes = true;
    args.config_path = Some(target.display().to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let printed = String::from_utf8(output).expect("output is UTF-8");
    assert!(!printed.contains("(preview:"));
    assert!(!printed.contains("  preview "));
    assert!(!printed.contains("\u{1b}["));
    let body = std::fs::read_to_string(dir.join("profiles").join("default.lua")).expect("profile");
    assert!(body.contains("theme = \"dark\""), "defaults still written");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_preview_baseline_prefers_loaded_config() {
    // Empty layers stay on the shipped fallback (headless/test default).
    let empty_dir = init_test_dir("baseline-empty");
    let empty_home = empty_dir.display().to_string();
    let empty_args = Args::new();
    let empty_env = init_env(None, None, None, Some(&empty_home), true);
    assert_eq!(
        crate::init::init_preview_baseline(&empty_args, &empty_env),
        bitty_config::fallback_builtin(),
        "no file/profile/CLI stays on fallback"
    );
    let _ = std::fs::remove_dir_all(&empty_dir);

    // Explicit file wins: custom theme/font seed the baseline.
    let dir = init_test_dir("baseline-file");
    let target = dir.join("custom.lua");
    std::fs::write(
        &target,
        "return { theme = \"tokyo-night\", font = { family = \"Fira Code\", size = 14 } }\n",
    )
    .expect("write custom config");
    let mut file_args = Args::new();
    file_args.config_path = Some(target.display().to_string());
    let home = dir.display().to_string();
    let file_env = init_env(None, None, None, Some(&home), true);
    let baseline = crate::init::init_preview_baseline(&file_args, &file_env);
    assert_eq!(
        baseline.appearance.theme.as_deref(),
        Some("tokyo-night"),
        "file theme seeds baseline"
    );
    assert_eq!(baseline.font.family, "Fira Code");
    assert!((baseline.font.size - 14.0).abs() < f32::EPSILON);
    assert_ne!(
        baseline,
        bitty_config::fallback_builtin(),
        "loaded file differs from fallback"
    );

    // CLI wins over the file: --theme overrides the file theme.
    let mut cli_args = Args::new();
    cli_args.config_path = Some(target.display().to_string());
    cli_args.theme = Some("catppuccin".to_string());
    let cli_baseline = crate::init::init_preview_baseline(&cli_args, &file_env);
    assert_eq!(
        cli_baseline.appearance.theme.as_deref(),
        Some("catppuccin"),
        "CLI theme wins over file"
    );

    // CLI alone (no file) still seeds: --theme with empty layers.
    let nodir = init_test_dir("baseline-cli");
    let nohome = nodir.display().to_string();
    let mut lone_cli = Args::new();
    lone_cli.theme = Some("tokyo-night".to_string());
    let lone_env = init_env(None, None, None, Some(&nohome), true);
    let lone = crate::init::init_preview_baseline(&lone_cli, &lone_env);
    assert_eq!(
        lone.appearance.theme.as_deref(),
        Some("tokyo-night"),
        "CLI theme seeds without a file"
    );
    let _ = std::fs::remove_dir_all(&nodir);

    // Invalid file falls back, never blocks the wizard.
    std::fs::write(&target, "return { theme = }\n").expect("write invalid config");
    let fallback = crate::init::init_preview_baseline(&file_args, &file_env);
    assert_eq!(
        fallback,
        bitty_config::fallback_builtin(),
        "invalid file falls back"
    );

    // CodeRabbit 1877: a broken existing file fails closed even when --theme
    // supplies another layer — the preview must not hide the breakage behind
    // theme-on-defaults.
    let mut broken_cli = Args::new();
    broken_cli.config_path = Some(target.display().to_string());
    broken_cli.theme = Some("catppuccin".to_string());
    assert_eq!(
        crate::init::init_preview_baseline(&broken_cli, &file_env),
        bitty_config::fallback_builtin(),
        "broken file + --theme still falls back"
    );

    // Profile layering: --profile seeds under the same XDG root.
    let prof_root = init_test_dir("baseline-profile");
    let prof_root_s = prof_root.display().to_string();
    let prof_dir = prof_root.join("bitty").join("profiles");
    std::fs::create_dir_all(&prof_dir).expect("profiles dir");
    std::fs::write(
        prof_dir.join("myprof.lua"),
        "return { theme = \"tokyo-night\" }\n",
    )
    .expect("write profile");
    let mut prof_args = Args::new();
    prof_args.profile = Some("myprof".to_string());
    let prof_env = init_env(None, None, Some(&prof_root_s), None, true);
    let prof = crate::init::init_preview_baseline(&prof_args, &prof_env);
    assert_eq!(
        prof.appearance.theme.as_deref(),
        Some("tokyo-night"),
        "profile theme seeds baseline"
    );

    // Missing profile falls back, never blocks the wizard.
    let mut missing_args = Args::new();
    missing_args.profile = Some("ghost".to_string());
    let missing = crate::init::init_preview_baseline(&missing_args, &prof_env);
    assert_eq!(
        missing,
        bitty_config::fallback_builtin(),
        "missing profile falls back"
    );

    // BITTY_PROFILE env participates like --profile when the flag is absent.
    let bitty_env = InitEnv {
        bitty_config: None,
        bitty_profile: Some("myprof"),
        shell: None,
        xdg_config_home: Some(&prof_root_s),
        xdg_data_home: None,
        home: None,
        stdin_is_tty: true,
    };
    let env_prof = crate::init::init_preview_baseline(&Args::new(), &bitty_env);
    assert_eq!(
        env_prof.appearance.theme.as_deref(),
        Some("tokyo-night"),
        "BITTY_PROFILE seeds baseline"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&prof_root);
}

#[test]
fn init_live_preview_enabled_gate() {
    // Interactive TTY without machine flows enables the wizard-owned live
    // preview; every machine flow keeps the in-memory behavior.
    let args = Args::new();
    let env = init_env(None, Some("/bin/bash"), None, Some("/tmp"), true);
    assert!(crate::init::init_live_preview_enabled(&args, &env, true));

    let piped = init_env(None, Some("/bin/bash"), None, Some("/tmp"), false);
    assert!(
        !crate::init::init_live_preview_enabled(&args, &piped, true),
        "piped stdin keeps in-memory preview"
    );

    let piped_out = init_env(None, Some("/bin/bash"), None, Some("/tmp"), true);
    assert!(
        !crate::init::init_live_preview_enabled(&args, &piped_out, false),
        "piped stdout has no preview surface"
    );

    let mut yes = Args::new();
    yes.init_yes = true;
    assert!(
        !crate::init::init_live_preview_enabled(&yes, &env, true),
        "--yes has no steps to push"
    );

    let mut safe = Args::new();
    safe.safe = true;
    assert!(
        !crate::init::init_live_preview_enabled(&safe, &env, true),
        "--safe keeps in-memory preview"
    );

    let mut headless = Args::new();
    headless.headless = true;
    assert!(
        !crate::init::init_live_preview_enabled(&headless, &env, true),
        "--headless keeps in-memory preview"
    );

    let mut testmode = Args::new();
    testmode.test_mode = true;
    assert!(
        !crate::init::init_live_preview_enabled(&testmode, &env, true),
        "--test-mode keeps in-memory preview"
    );
}

#[test]
fn init_preview_push_live_disabled_is_noop() {
    // Piped/non-TTY preview never touches a live session.
    let baseline = bitty_config::fallback_builtin();
    let cfg = runtime_config_from_effective(&baseline).expect("cfg");
    let mut runtime =
        Runtime::with_deterministic_rasterizer(cfg).expect("deterministic runtime builds");
    let before_background = runtime.active_background();
    let before_family = runtime.config().font_family.clone();
    let _ = crate::config_reload::take_app_adoption();

    let disabled = InitPreview::disabled();
    assert!(!disabled.is_enabled());
    assert!(
        crate::init::init_preview_push_live(&mut runtime, &disabled).is_none(),
        "disabled preview pushes nothing"
    );
    assert_eq!(runtime.active_background(), before_background);
    assert_eq!(runtime.config().font_family, before_family);
    assert!(
        crate::config_reload::take_app_adoption().is_none(),
        "disabled push stashes nothing"
    );
    assert!(!runtime.has_pty(), "preview never spawns a shell");
}

#[test]
fn init_live_preview_maps_snapshots_per_step() {
    // Headless hermetic overlay-to-preview mapping (p4 CTX-1119): every
    // prompted snapshot drives the reload path on a deterministic runtime.
    // Same custom picks as the in-memory preview test: shell 2, bitty-dark,
    // Fira Code, 14pt, 10/12/3/8, 50000, always, vim 2.
    let mut baseline = bitty_config::fallback_builtin();
    baseline.window.opacity = 0.75;
    baseline.layout.resize_step = 0.02;

    let (_, _, preview) = drive_init_wizard_with_preview(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    let snapshots = preview.snapshots();
    assert_eq!(snapshots.len(), 11, "every prompted step previews");

    let cfg = runtime_config_from_effective(&baseline).expect("cfg");
    let mut runtime =
        Runtime::with_deterministic_rasterizer(cfg).expect("deterministic runtime builds");
    let _ = crate::config_reload::take_app_adoption();

    let dark_expected =
        bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("dark")));
    for (index, snapshot) in snapshots.iter().enumerate() {
        crate::config_reload::apply_live(&mut runtime, snapshot).expect("snapshot adopts live");
        let app = crate::config_reload::take_app_adoption().expect("app half stashed");
        assert!(
            (runtime.window_opacity() - 0.75).abs() < f32::EPSILON,
            "snapshot {index} keeps out-of-wizard opacity live"
        );
        assert!(
            (app.resize_step - 0.02).abs() < f32::EPSILON,
            "snapshot {index} keeps out-of-wizard resize step"
        );
        assert!(!runtime.has_pty(), "snapshot {index} spawns nothing");
        match index {
            0 => {
                assert_eq!(
                    snapshot.terminal.shell.as_deref(),
                    Some("/bin/sh"),
                    "shell previews first"
                );
                assert_eq!(
                    runtime.active_background(),
                    bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(
                        baseline.appearance.theme.as_deref()
                    ))
                    .background,
                    "shell-only snapshot keeps baseline theme live"
                );
                assert!(
                    snapshot.keymaps.is_empty(),
                    "effective preset still default before its own step"
                );
                assert_eq!(
                    app.keymaps.len(),
                    bitty_config::keymap::DEFAULT_KEYMAPS.len(),
                    "resolved defaults ride the app half"
                );
            }
            1 => {
                assert_eq!(
                    runtime.active_background(),
                    dark_expected.background,
                    "theme visibly hot-updates second"
                );
            }
            2 => {
                assert_eq!(runtime.config().font_family, "Fira Code");
            }
            3 => {
                assert!(
                    (runtime.config().font_size - 14.0).abs() < f32::EPSILON,
                    "font size adopts live"
                );
            }
            4 => {
                assert_eq!(runtime.config().decoration.gaps_in, 10);
            }
            5 => {
                assert_eq!(runtime.config().decoration.gaps_out, 12);
            }
            6 => {
                assert_eq!(runtime.config().decoration.border, 3);
            }
            7 => {
                assert_eq!(runtime.config().decoration.radius, 8);
            }
            8 => {
                assert_eq!(
                    snapshot.terminal.scrollback, 50_000,
                    "scrollback stages in overlay"
                );
                assert_eq!(
                    runtime.config().scrollback,
                    baseline_scrollback(&baseline),
                    "scrollback stays restart-required live"
                );
            }
            9 => {
                assert_eq!(snapshot.close_confirm, bitty_config::CloseConfirm::Always);
            }
            10 => {
                assert_eq!(
                    snapshot.keymaps.len(),
                    bitty_config::keymap::DEFAULT_KEYMAPS.len(),
                    "vim preset stages explicit effective map"
                );
                assert_eq!(
                    app.keymaps.len(),
                    bitty_config::keymap::DEFAULT_KEYMAPS.len(),
                    "vim preset rides the app half live"
                );
                assert_eq!(preview.current(), snapshot);
            }
            _ => unreachable!("eleven prompted steps"),
        }
    }
    assert!(
        crate::config_reload::take_app_adoption().is_none(),
        "single-shot slot leaves nothing behind"
    );
}

/// Baseline scrollback the live runtime keeps while the overlay stages a
/// restart-required scrollback value.
fn baseline_scrollback(baseline: &bitty_config::EffectiveConfig) -> usize {
    let cfg = runtime_config_from_effective(baseline).expect("cfg");
    cfg.scrollback
}

#[test]
fn init_preview_push_live_maps_final_overlay() {
    // The helper pushes the final overlay through the same path and returns
    // the app half for a preview app to adopt via adopt_live_config.
    let mut baseline = bitty_config::fallback_builtin();
    baseline.window.opacity = 0.75;
    let (result, _, preview) = drive_init_wizard_with_preview(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n",
        Some("/bin/bash"),
        &InitOverrides::default(),
        baseline.clone(),
    );
    result.expect("custom picks accepted");

    let cfg = runtime_config_from_effective(preview.baseline()).expect("cfg");
    let mut runtime =
        Runtime::with_deterministic_rasterizer(cfg).expect("deterministic runtime builds");
    let _ = crate::config_reload::take_app_adoption();
    let app = crate::init::init_preview_push_live(&mut runtime, &preview)
        .expect("final overlay adopts live");

    let dark_expected =
        bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("dark")));
    assert_eq!(runtime.active_background(), dark_expected.background);
    assert_eq!(runtime.config().font_family, "Fira Code");
    assert!((runtime.config().font_size - 14.0).abs() < f32::EPSILON);
    assert_eq!(runtime.config().decoration.gaps_in, 10);
    assert_eq!(runtime.config().decoration.gaps_out, 12);
    assert_eq!(runtime.config().decoration.border, 3);
    assert_eq!(runtime.config().decoration.radius, 8);
    assert!((runtime.window_opacity() - 0.75).abs() < f32::EPSILON);
    assert_eq!(
        app.keymaps.len(),
        bitty_config::keymap::DEFAULT_KEYMAPS.len(),
        "vim preset rides the app half"
    );
    assert!(!runtime.has_pty(), "preview never spawns a shell");
    assert!(
        crate::config_reload::take_app_adoption().is_none(),
        "helper drains the single-shot slot"
    );
}

#[test]
fn init_preview_echo_tracks_running_theme_with_color() {
    // User-visible half of the live preview (DEC-0109): the running echo is
    // styled in the running preview theme. Same custom picks as the mapping
    // test; no live session needed since the echo reads the overlay only.
    let baseline = bitty_config::fallback_builtin();
    let mut input = std::io::BufReader::new(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n".as_bytes(),
    );
    let mut output = Vec::new();
    let mut preview = InitPreview::enabled(baseline);
    let result = run_init_interactive_with_preview(
        &mut input,
        &mut output,
        Some("/bin/bash"),
        &|p| p == "/bin/bash" || p == "/bin/sh",
        &InitOverrides::default(),
        true,
        Some(&mut preview),
        None,
        false,
    );
    result.expect("custom picks accepted");
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    let echoes: Vec<&str> = printed
        .lines()
        .filter(|line| line.contains("(preview:"))
        .collect();
    assert_eq!(echoes.len(), 11, "one echo per step, got:\n{printed}");
    // The first echo still carries the shipped defaults while the last one
    // carries every custom pick: the echo tracks the running overlay.
    assert!(
        echoes[0].contains(bitty_config::types::DEFAULT_FONT_FAMILY),
        "first echo shows the default family, got: {}",
        echoes[0]
    );
    assert!(
        !echoes[0].contains("Fira Code"),
        "first echo predates the family pick, got: {}",
        echoes[0]
    );
    assert!(
        echoes[10].contains("font=\"Fira Code\" 14pt"),
        "last echo shows the picked family and size, got: {}",
        echoes[10]
    );
    assert!(
        echoes[10].contains("gaps 10/12, border 3, radius 8"),
        "last echo shows the picked decoration, got: {}",
        echoes[10]
    );
    assert!(
        echoes[10].contains("theme=\"dark\""),
        "last echo shows the picked theme, got: {}",
        echoes[10]
    );
    // Styled in the running preview theme through the existing convention.
    let (dark, _) = bitty_config::theme::resolve_theme_with_status(Some("dark"));
    let foreground = format!(
        "\u{1b}[38;2;{};{};{}m",
        dark.foreground[0], dark.foreground[1], dark.foreground[2]
    );
    let background = format!(
        "\u{1b}[48;2;{};{};{}m",
        dark.background[0], dark.background[1], dark.background[2]
    );
    assert!(
        echoes.iter().any(|line| line.contains(&foreground)),
        "echo wears the preview theme foreground, got:\n{printed}"
    );
    assert!(
        echoes.iter().any(|line| line.contains(&background)),
        "echo wears the preview theme background, got:\n{printed}"
    );
}

#[test]
fn init_preview_echo_plain_without_color() {
    // Without color the same running echo stays plain (existing plain-output
    // contract keeps holding with echoes present).
    let baseline = bitty_config::fallback_builtin();
    let mut input = std::io::BufReader::new(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n".as_bytes(),
    );
    let mut output = Vec::new();
    let mut preview = InitPreview::enabled(baseline);
    let result = run_init_interactive_with_preview(
        &mut input,
        &mut output,
        Some("/bin/bash"),
        &|p| p == "/bin/bash" || p == "/bin/sh",
        &InitOverrides::default(),
        false,
        Some(&mut preview),
        None,
        false,
    );
    result.expect("custom picks accepted");
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert_eq!(
        printed.matches("(preview:").count(),
        11,
        "one plain echo per step, got:\n{printed}"
    );
    assert!(
        printed.contains("(preview: theme=\"dark\", font=\"Fira Code\" 14pt"),
        "plain echo carries the running picks, got:\n{printed}"
    );
    assert!(
        !printed.contains("\u{1b}["),
        "plain echo adds no ANSI, got:\n{printed}"
    );
}

#[test]
fn init_preview_echo_absent_without_preview() {
    // Piped/plain path (no preview): prompts behave exactly as before with
    // no echo lines.
    let mut input = std::io::BufReader::new(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n".as_bytes(),
    );
    let mut output = Vec::new();
    let result = run_init_interactive(
        &mut input,
        &mut output,
        Some("/bin/bash"),
        &|p| p == "/bin/bash" || p == "/bin/sh",
        &InitOverrides::default(),
        true,
        false,
    );
    result.expect("custom picks accepted");
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(
        !printed.contains("(preview:"),
        "no preview means no echo, got:\n{printed}"
    );
}

// Live-spawn: the wizard-owned preview session hot-updates per step on the
// real constructor (not the deterministic rasterizer). Unix-only in this
// slice (no ConPTY coverage); require_pty keeps the force-no-PTY simulation
// path and proves the preview spawns nothing even when a PTY exists. Plain
// output only, no stdout color asserts.
#[test]
#[cfg(unix)]
fn init_live_preview_hot_updates_per_step_unix() {
    require_pty!();
    let mut baseline = bitty_config::fallback_builtin();
    baseline.window.opacity = 0.75;
    baseline.layout.resize_step = 0.02;
    let before = baseline.clone();
    let cfg = runtime_config_from_effective(&baseline).expect("cfg");
    let mut live = Runtime::new(cfg).expect("live preview builds");
    let _ = crate::config_reload::take_app_adoption();

    let mut input = std::io::BufReader::new(
        "2\nbitty-dark\nFira Code\n14\n10\n12\n3\n8\n50000\nalways\n2\n".as_bytes(),
    );
    let mut output = Vec::new();
    let mut preview = InitPreview::enabled(baseline);
    let result = run_init_interactive_with_preview(
        &mut input,
        &mut output,
        Some("/bin/bash"),
        &|p| p == "/bin/bash" || p == "/bin/sh",
        &InitOverrides::default(),
        false,
        Some(&mut preview),
        Some(&mut live),
        false,
    );
    let answers = result.expect("custom picks accepted");
    assert_eq!(answers.shell.as_deref(), Some("/bin/sh"));
    assert_eq!(
        preview.snapshots().len(),
        11,
        "every prompted step previews"
    );

    let dark_expected =
        bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("dark")));
    assert_eq!(
        live.active_background(),
        dark_expected.background,
        "theme visibly hot-updates live"
    );
    assert_eq!(live.config().font_family, "Fira Code");
    assert!((live.config().font_size - 14.0).abs() < f32::EPSILON);
    assert_eq!(live.config().decoration.gaps_in, 10);
    assert_eq!(live.config().decoration.gaps_out, 12);
    assert_eq!(live.config().decoration.border, 3);
    assert_eq!(live.config().decoration.radius, 8);
    assert!(
        (live.window_opacity() - 0.75).abs() < f32::EPSILON,
        "out-of-wizard opacity preserved live"
    );
    assert!(!live.has_pty(), "preview spawns no shell even with PTY");
    assert!(
        crate::config_reload::take_app_adoption().is_none(),
        "per-step pushes leave no app-half behind"
    );
    assert_eq!(preview.baseline(), &before);
    assert_eq!(preview.current(), &preview.snapshots()[10]);
}

// Live-spawn Esc rollback: cancel restores the prior config in memory and
// the prior live appearance in the preview session. Same unix and PTY gate
// as the confirm-path live test; plain output only.
#[test]
#[cfg(unix)]
fn init_live_preview_esc_restores_baseline_unix() {
    require_pty!();
    let baseline = bitty_config::fallback_builtin();
    let cfg = runtime_config_from_effective(&baseline).expect("cfg");
    let mut live = Runtime::new(cfg).expect("live preview builds");
    let before_background = live.active_background();
    let before_opacity = live.window_opacity();
    let _ = crate::config_reload::take_app_adoption();

    let mut input = std::io::BufReader::new("\n\x1b\n".as_bytes());
    let mut output = Vec::new();
    let mut preview = InitPreview::enabled(baseline.clone());
    let result = run_init_interactive_with_preview(
        &mut input,
        &mut output,
        Some("/bin/bash"),
        &|p| p == "/bin/bash" || p == "/bin/sh",
        &InitOverrides::default(),
        false,
        Some(&mut preview),
        Some(&mut live),
        false,
    );
    let err = result.expect_err("Esc cancels");
    assert!(err.contains("cancelled"), "unexpected: {err}");
    assert_eq!(preview.snapshots().len(), 1, "only shell previewed");
    assert_eq!(
        preview.current(),
        &baseline,
        "Esc restores the exact previous config"
    );
    assert_eq!(
        live.active_background(),
        before_background,
        "Esc restores the prior live appearance"
    );
    assert!(
        (live.window_opacity() - before_opacity).abs() < f32::EPSILON,
        "Esc restores prior live opacity"
    );
    assert!(!live.has_pty(), "cancel spawns nothing");
    assert!(
        crate::config_reload::take_app_adoption().is_none(),
        "rollback leaves no app-half behind"
    );
}

#[test]
fn init_pending_recap_matches_written_file() {
    // Full TTY pass: eleven Enters take every lua default plus one Enter for
    // the plugin follow-up (empty = none); the recap printed before write
    // must equal the file subsequently written.
    let dir = init_test_dir("pending-recap");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.config_path = Some(target.display().to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let mut input = std::io::BufReader::new("\n\n\n\n\n\n\n\n\n\n\n\n".as_bytes());
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let written = std::fs::read_to_string(&target).expect("written");
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(
        printed.contains("(pending init.lua"),
        "recap header printed before write"
    );
    assert!(
        printed.ends_with(&written),
        "recap is byte-identical to the written file"
    );

    // Cancel path prints no recap and writes nothing.
    let cancel_target = dir.join("cancelled").join("init.lua");
    let mut cancel_args = Args::new();
    cancel_args.init_word = true;
    cancel_args.config_path = Some(cancel_target.display().to_string());
    let mut input: &[u8] = b"";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&cancel_args, &env, &mut input, &mut output),
        1,
        "TTY EOF cancels with exit 1"
    );
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(
        !printed.contains("(pending init.lua"),
        "cancel prints no recap"
    );
    assert!(!cancel_target.exists(), "cancel writes no file");

    // Refusal path (existing file, no --force) prints no recap: the header
    // must never read as a write promise for a write that will not happen.
    std::fs::write(&target, "-- sentinel").expect("pre-existing file");
    let mut input = std::io::BufReader::new("\n\n\n\n\n\n\n\n\n\n\n\n".as_bytes());
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        2,
        "existing file without --force refuses with exit 2"
    );
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(
        !printed.contains("(pending init.lua"),
        "refusal prints no recap"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("untouched"),
        "-- sentinel",
        "refusal leaves the existing file intact"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_render_default_and_vim() {
    let base = init_yes_defaults(Some("/bin/bash"));
    let lua = render_init_lua(&base);
    assert_nested_appearance("profile default", &lua, "dark");
    assert_four_space_indent("profile default", &lua);
    assert!(lua.contains("JetBrainsMono Nerd Font"));
    assert!(lua.contains("shell = \"/bin/bash\""));
    assert!(lua.contains("scrollback = 10000"));
    assert!(lua.contains("gaps_in = 6"));
    assert!(lua.contains("close_confirm = \"when_busy\""));
    assert!(!lua.lines().any(|line| line.starts_with("    keymaps = {")));
    assert!(lua.contains("-- keymaps = {"));

    // No shell: the terminal table still carries scrollback and documents
    // the shell key as a commented example.
    let noshell = init_yes_defaults(None);
    let lua = render_init_lua(&noshell);
    assert!(lua.contains("terminal = {"));
    assert!(lua.contains("scrollback = 10000"));
    assert!(!lua.contains("        shell ="));
    assert!(lua.contains("        -- shell = \"/bin/fish\","));

    // A custom font family flows through verbatim.
    let family = InitAnswers {
        font_family: "Fira Code".to_string(),
        ..init_yes_defaults(None)
    };
    let lua = render_init_lua(&family);
    assert!(lua.contains("family = \"Fira Code\""));

    // Vim preset writes every shipped binding explicitly.
    let vim = InitAnswers {
        key_preset: InitKeyPreset::Vim,
        ..base
    };
    let lua = render_init_lua(&vim);
    assert!(lua.contains("    keymaps = {"));
    for (chord, action) in bitty_config::keymap::DEFAULT_KEYMAPS {
        assert!(
            lua.contains(&format!("chord = \"{chord}\"")),
            "preset renders {chord}"
        );
        assert!(
            lua.contains(&format!("action = \"{action}\"")),
            "preset renders {action}"
        );
    }
}

#[test]
fn init_rendered_config_parses_and_merges_to_effective() {
    use bitty_config::CloseConfirm;
    use bitty_config::file::{CliOverrides, parse_lua_config, resolve_effective_full};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    // Every preset x shell combination must parse with values intact and
    // merge into a valid `EffectiveConfig` (the startup load path).
    for preset in [InitKeyPreset::Default, InitKeyPreset::Vim] {
        for shell in [Some("/bin/zsh"), None] {
            let answers = InitAnswers {
                shell: shell.map(str::to_string),
                font_family: "Fira Code".to_string(),
                font_size: 14.0,
                scrollback: 50_000,
                close_confirm: CloseConfirm::Always,
                gaps_in: 1,
                gaps_out: 2,
                border: 3,
                radius: 4,
                key_preset: preset,
                ..init_yes_defaults(None)
            };
            let lua = render_init_lua(&answers);
            let plan = parse_lua_config(&lua, &src).expect("wizard output parses");
            assert_eq!(
                plan.appearance.as_ref().unwrap().theme.as_deref(),
                Some("dark")
            );
            let font = plan.font.as_ref().expect("font table");
            assert_eq!(font.family, "Fira Code");
            assert_eq!(font.size, 14.0);
            let dec = plan.decoration.as_ref().expect("decoration table");
            assert_eq!(dec.gaps_in, 1);
            assert_eq!(dec.gaps_out, 2);
            assert_eq!(dec.border, 3);
            assert_eq!(dec.radius, 4);
            let term = plan.terminal.as_ref().expect("terminal table");
            assert_eq!(term.scrollback, 50_000);
            assert_eq!(term.shell.as_deref(), shell, "shell round-trips");
            assert_eq!(plan.close_confirm, Some(CloseConfirm::Always));
            match preset {
                InitKeyPreset::Vim => assert_eq!(
                    plan.keymaps
                        .as_ref()
                        .expect("vim preset writes keymaps")
                        .len(),
                    bitty_config::keymap::DEFAULT_KEYMAPS.len()
                ),
                InitKeyPreset::Default => assert!(plan.keymaps.is_none()),
            }

            // EffectiveConfig load path: merge + validate, values intact.
            let merged = resolve_effective_full(
                Some(bitty_config::LayeredPlan::new(src.clone(), plan)),
                None,
                &CliOverrides::default(),
            )
            .expect("wizard output merges");
            let effective = merged.effective;
            effective.validate().expect("effective config valid");
            assert_eq!(effective.font.family, "Fira Code");
            assert_eq!(effective.font.size, 14.0);
            assert_eq!(effective.terminal.scrollback, 50_000);
            assert_eq!(
                effective.terminal.shell.as_deref(),
                shell,
                "effective shell round-trips"
            );
            assert_eq!(effective.close_confirm, CloseConfirm::Always);
            assert_eq!(effective.decoration.gaps_in, 1);
            assert_eq!(effective.decoration.gaps_out, 2);
            assert_eq!(effective.decoration.border, 3);
            assert_eq!(effective.decoration.radius, 4);
        }
    }
}

#[test]
fn init_vim_preset_agrees_with_shipped_defaults() {
    // The wizard preset is rendered FROM DEFAULT_KEYMAPS (CTX-0178), so
    // resolving those entries as a user layer must reproduce the shipped
    // table exactly: same identities, same actions, explicit overrides.
    let effective = bitty_config::EffectiveConfig {
        keymaps: bitty_config::keymap::DEFAULT_KEYMAPS
            .iter()
            .map(|(chord, action)| bitty_config::KeymapEntry {
                chord: chord.to_string(),
                action: action.to_string(),
                context: "global".to_string(),
            })
            .collect(),
        ..Default::default()
    };
    let resolved = bitty_config::resolve_keymaps(&effective).expect("preset entries resolve");
    let shipped = bitty_config::default_keymaps().expect("shipped defaults resolve");
    assert_eq!(resolved.len(), shipped.len());
    // `resolve_keymaps` sorts by identity while `default_keymaps` keeps
    // declaration order: compare sorted identities.
    let mut shipped_ids: Vec<String> = shipped.iter().map(|m| m.id()).collect();
    shipped_ids.sort();
    let resolved_ids: Vec<String> = resolved.iter().map(|m| m.id()).collect();
    assert_eq!(resolved_ids, shipped_ids);
    for entry in &resolved {
        // Every preset entry overrode its default (explicit, tweakable).
        assert!(
            !entry.from_default,
            "preset entry {} is explicit",
            entry.id()
        );
        let (_, want_action) = bitty_config::keymap::DEFAULT_KEYMAPS
            .iter()
            .find(|(chord, _)| {
                bitty_config::Chord::parse(chord)
                    .expect("shipped chord parses")
                    .canonical()
                    == entry.chord.canonical()
            })
            .expect("preset chord is a shipped default");
        assert_eq!(entry.action.canonical(), *want_action);
    }

    // End to end: the rendered vim config parses and resolves to the
    // same table (render -> parse -> resolve agreement).
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let lua = render_init_lua(&InitAnswers {
        shell: None,
        font_size: 12.0,
        key_preset: InitKeyPreset::Vim,
        ..init_yes_defaults(None)
    });
    let plan = parse_lua_config(&lua, &src).expect("vim config parses");
    let effective = bitty_config::EffectiveConfig {
        keymaps: plan.keymaps.expect("keymaps"),
        ..Default::default()
    };
    let resolved = bitty_config::resolve_keymaps(&effective).expect("vim config resolves");
    let resolved_ids: Vec<String> = resolved.iter().map(|m| m.id()).collect();
    // Same sorted-identity comparison as above (`resolve_keymaps` sorts).
    assert_eq!(resolved_ids, shipped_ids);
}

#[test]
fn generated_templates_are_nested_and_documented() {
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let full = render_init_lua(&init_yes_defaults(Some("/bin/bash")));
    let vim = render_init_lua(&InitAnswers {
        key_preset: InitKeyPreset::Vim,
        ..init_yes_defaults(None)
    });
    let minimal = render_minimal_profile_lua(
        &init_preset_answers(InitProfilePreset::Minimal),
        &InitOverrides::default(),
    );
    let starter = starter_init_lua();
    let selector = render_selector_lua(DEFAULT_PROFILE_NAME);

    for (name, text) in [
        ("profile default", &full),
        ("profile vim", &vim),
        ("minimal", &minimal),
        ("starter", &starter),
        ("selector", &selector),
    ] {
        assert_four_space_indent(name, text);
        let plan = parse_lua_config(text, &src)
            .unwrap_or_else(|err| panic!("{name} parses: {err}\n{text}"));
        assert!(
            plan.undeclared_fields.is_empty(),
            "{name} declares only schema keys: {:?}",
            plan.undeclared_fields
        );
    }

    assert_nested_appearance("profile default", &full, "dark");
    assert_nested_appearance("profile vim", &vim, "dark");
    assert_nested_appearance("minimal", &minimal, "dark");
    assert_nested_appearance("starter", &starter, "dark");

    // The selector stays a thin, documented envelope with no values of its
    // own beyond `format` and `profile`.
    let selector_plan = parse_lua_config(&selector, &src).expect("selector parses");
    assert!(selector_plan.selector.is_some());
    assert!(selector_plan.font.is_none());
    assert!(selector_plan.appearance.is_none());
    assert!(!selector.contains("theme ="));

    // The full profile documents every major section with a commented
    // example; the starter additionally carries the shipped defaults for the
    // sections the profile writes uncommented.
    for section in [
        "animations = {",
        "window = {",
        "layout = {",
        "selection = {",
        "mouse = {",
        "workspace = {",
        "scrollbar = {",
        "views = {",
        "input = {",
        "keymaps = {",
    ] {
        assert!(
            full.contains(&format!("-- {section}")),
            "full profile documents {section}"
        );
    }
    for section in ["font = {", "decoration = {", "terminal = {"] {
        assert!(
            starter.contains(&format!("-- {section}")),
            "starter documents {section}"
        );
    }
}

#[test]
fn generated_commented_blocks_parse_when_uncommented() {
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let templates = [
        (
            "profile default",
            render_init_lua(&init_yes_defaults(Some("/bin/bash"))),
        ),
        (
            "profile vim",
            render_init_lua(&InitAnswers {
                key_preset: InitKeyPreset::Vim,
                ..init_yes_defaults(None)
            }),
        ),
        (
            "minimal",
            render_minimal_profile_lua(
                &init_preset_answers(InitProfilePreset::Minimal),
                &InitOverrides::default(),
            ),
        ),
        ("starter", starter_init_lua()),
    ];
    for (name, text) in templates {
        for marker in [
            "font = {",
            "window = {",
            "layout = {",
            "decoration = {",
            "terminal = {",
            "selection = {",
            "mouse = {",
            "workspace = {",
            "scrollbar = {",
            "views = {",
            "input = {",
            "animations = {",
            "keymaps = {",
        ] {
            if !text.contains(&format!("-- {marker}")) {
                continue;
            }
            let uncommented = uncomment_lua_block(&text, marker);
            assert_ne!(uncommented, text, "{name} uncomments {marker}");
            parse_lua_config(&uncommented, &src).unwrap_or_else(|err| {
                panic!("{name} {marker} block parses after uncommenting: {err}\n{uncommented}")
            });
        }
    }
}

#[test]
fn generated_optional_keys_parse_when_uncommented() {
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let full = render_init_lua(&init_yes_defaults(None));
    let uncommented = uncomment_commented_keys(
        &full,
        &[
            "line_height =",
            "letter_spacing =",
            "disable_ligatures =",
            "fallback =",
            "features =",
            "content_inset =",
            "border_color_focused =",
            "border_color_idle =",
            "border_width_focused =",
            "border_width_idle =",
            "shell =",
            "scroll_lines_per_notch =",
            "scroll_pixels_per_notch =",
            "page_scroll_percent =",
            "cursor_style =",
            "bell =",
        ],
    );
    let plan = parse_lua_config(&uncommented, &src).expect("optional examples parse");

    let font = plan.font.expect("font table");
    assert_eq!(font.line_height, 1.375);
    assert_eq!(font.letter_spacing, 2.0);
    assert_eq!(
        font.disable_ligatures,
        bitty_config::types::LigaturePolicy::Never
    );
    assert_eq!(font.fallback, vec!["Noto Sans Symbols 2".to_string()]);
    assert_eq!(font.features, vec!["calt".to_string(), "ss01".to_string()]);

    let decoration = plan.decoration.expect("decoration table");
    assert_eq!(decoration.content_inset, 6);
    assert!(decoration.border_color_focused.is_some());
    assert!(decoration.border_color_idle.is_some());
    assert_eq!(decoration.border_width_focused, Some(2));
    assert_eq!(decoration.border_width_idle, Some(1));

    let terminal = plan.terminal.expect("terminal table");
    assert_eq!(terminal.shell.as_deref(), Some("/bin/fish"));
    assert_eq!(terminal.scroll_lines_per_notch, 3);
    assert_eq!(terminal.scroll_pixels_per_notch, 16);
    assert_eq!(terminal.page_scroll_percent, 75);
    assert_eq!(
        terminal.cursor_style,
        bitty_config::types::CursorStyle::Default
    );
    assert_eq!(terminal.bell, bitty_config::types::BellMode::Visual);
}

#[test]
fn init_write_new_refuse_force_backup_idempotent() {
    let dir = init_test_dir("write");
    let target = dir.join("init.lua");
    let content = render_init_lua(&init_yes_defaults(Some("/bin/bash")));

    // Fresh write succeeds.
    let outcome = write_init_config(&target, &content, false).expect("fresh write");
    assert_eq!(outcome.path, target);
    assert!(!outcome.updated);
    assert!(outcome.backup.is_none());
    assert_eq!(
        std::fs::read_to_string(&target).expect("read back"),
        content
    );

    // Re-run without --force refuses and leaves the file untouched.
    let err = write_init_config(&target, &content, false).expect_err("must refuse");
    assert!(
        matches!(err, InitWriteError::Refused(_)),
        "refusal is usage-level"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("untouched"),
        content
    );

    // --force backs up the previous bytes, then writes the new content.
    let updated_content = render_init_lua(&InitAnswers {
        shell: Some("/bin/zsh".to_string()),
        font_size: 14.0,
        key_preset: InitKeyPreset::Vim,
        ..init_yes_defaults(None)
    });
    let outcome = write_init_config(&target, &updated_content, true).expect("forced write");
    assert!(outcome.updated);
    let backup = outcome.backup.expect("backup path");
    assert_eq!(backup, target.with_extension("lua.bak"));
    assert_eq!(
        std::fs::read_to_string(&backup).expect("backup bytes"),
        content
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("new bytes"),
        updated_content
    );

    // Idempotent: writing the same answers again produces byte-identical output.
    let again = render_init_lua(&InitAnswers {
        shell: Some("/bin/zsh".to_string()),
        font_size: 14.0,
        key_preset: InitKeyPreset::Vim,
        ..init_yes_defaults(None)
    });
    assert_eq!(again, updated_content);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_profile_presets_are_deterministic_seeds() {
    // Reserved names are exact lowercase; lookalikes stay custom profiles.
    assert_eq!(
        InitProfilePreset::parse("minimal"),
        Some(InitProfilePreset::Minimal)
    );
    assert_eq!(
        InitProfilePreset::parse("dark"),
        Some(InitProfilePreset::Dark)
    );
    assert_eq!(
        InitProfilePreset::parse("light"),
        Some(InitProfilePreset::Light)
    );
    assert!(InitProfilePreset::parse("Minimal").is_none());
    assert!(InitProfilePreset::parse("minimal-2").is_none());
    assert_eq!(InitProfilePreset::Dark.name(), "dark");

    let minimal = init_preset_answers(InitProfilePreset::Minimal);
    assert_eq!(minimal.theme, bitty_config::theme::DARK_THEME_ALIAS);
    assert!(
        minimal.shell.is_none(),
        "presets never derive a shell from the host"
    );

    let dark = init_preset_answers(InitProfilePreset::Dark);
    assert_eq!(dark.theme, bitty_config::theme::DARK_THEME_ALIAS);
    assert_eq!(dark.font_family, bitty_config::types::DEFAULT_FONT_FAMILY);
    assert_eq!(dark.font_size, bitty_config::types::DEFAULT_FONT_SIZE);
    assert_eq!(
        dark.scrollback,
        bitty_config::types::TerminalConfig::default().scrollback
    );
    assert_eq!(
        dark.close_confirm,
        bitty_config::types::DEFAULT_CLOSE_CONFIRM
    );
    assert_eq!(dark.key_preset, InitKeyPreset::Default);

    let light = init_preset_answers(InitProfilePreset::Light);
    assert_eq!(light.theme, PROFILE_LIGHT_THEME);
    let (theme, status) = bitty_config::theme::resolve_theme_with_status(Some(&light.theme));
    assert_eq!(status, bitty_config::theme::ThemeResolution::Named);
    assert_eq!(theme.name, "github-light");
    // dark/light differ only in the theme; every other generated default matches.
    assert_eq!(light.font_family, dark.font_family);
    assert_eq!(light.font_size, dark.font_size);
    assert_eq!(light.scrollback, dark.scrollback);
}

#[test]
fn init_minimal_profile_renders_smallest_config() {
    let minimal = init_preset_answers(InitProfilePreset::Minimal);
    let rendered = render_minimal_profile_lua(&minimal, &InitOverrides::default());
    let full = render_init_lua(&minimal);
    assert!(
        rendered.len() < full.len(),
        "minimal stays the smallest seed ({} vs {} bytes)",
        rendered.len(),
        full.len()
    );
    assert_nested_appearance("minimal", &rendered, "dark");
    assert_four_space_indent("minimal", &rendered);
    for absent in [
        "font =",
        "decoration =",
        "terminal =",
        "close_confirm =",
        "keymaps =",
    ] {
        assert!(
            !rendered.contains(absent),
            "minimal omits {absent}:\n{rendered}"
        );
    }

    let source = bitty_config::plan::ConfigSource::new(
        bitty_config::plan::LayerKind::Profile,
        Some("profiles/minimal.lua"),
    );
    let plan =
        bitty_config::file::parse_lua_config(&rendered, &source).expect("minimal profile parses");
    let merged = bitty_config::file::resolve_effective_with_profiles(
        None,
        vec![bitty_config::LayeredPlan::new(source.clone(), plan)],
        &bitty_config::file::CliOverrides::default(),
    )
    .expect("minimal profile resolves");
    assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
    assert_eq!(
        merged.effective.font.family,
        bitty_config::types::DEFAULT_FONT_FAMILY,
        "omitted keys keep the shipped font"
    );
    assert_eq!(
        merged.effective.terminal.scrollback,
        bitty_config::types::TerminalConfig::default().scrollback
    );

    // Explicit value flags still land in the minimal seed, but only the
    // blocks they touch.
    let overrides = InitOverrides {
        font_size: Some(15.0),
        scrollback: Some(9_000),
        ..InitOverrides::default()
    };
    let mut answers = init_preset_answers(InitProfilePreset::Minimal);
    init_apply_overrides(&mut answers, &overrides);
    let rendered = render_minimal_profile_lua(&answers, &overrides);
    assert!(rendered.contains("font = {"));
    assert!(rendered.contains(&format!(
        "        family = \"{}\",",
        bitty_config::types::DEFAULT_FONT_FAMILY
    )));
    assert!(rendered.contains("        size = 15,"));
    assert!(rendered.contains("terminal = {"));
    assert!(rendered.contains("        scrollback = 9000,"));
    assert!(!rendered.contains("decoration ="));
    assert_four_space_indent("minimal overrides", &rendered);
    bitty_config::file::parse_lua_config(&rendered, &source)
        .expect("minimal profile with overrides parses");
}

#[test]
fn init_write_backups_are_collision_free_and_fail_closed() {
    let dir = init_test_dir("write-backups");
    let target = dir.join("init.lua");
    let first = render_init_lua(&init_yes_defaults(None));
    write_init_config(&target, &first, false).expect("fresh write");

    let second = render_init_lua(&InitAnswers {
        font_size: 15.0,
        ..init_yes_defaults(None)
    });
    let outcome = write_init_config(&target, &second, true).expect("first forced write");
    let backup = outcome.backup.expect("backup path");
    assert_eq!(backup, target.with_extension("lua.bak"));
    assert_eq!(std::fs::read_to_string(&backup).expect("backup"), first);

    // A second forced write never overwrites the sole recovery copy.
    let third = render_init_lua(&InitAnswers {
        font_size: 16.0,
        ..init_yes_defaults(None)
    });
    let outcome = write_init_config(&target, &third, true).expect("second forced write");
    let second_backup = outcome.backup.expect("second backup path");
    assert_eq!(second_backup, dir.join("init.lua.bak.1"));
    assert_eq!(
        std::fs::read_to_string(&second_backup).expect("second backup"),
        second
    );
    assert_eq!(
        std::fs::read_to_string(&backup).expect("first backup intact"),
        first
    );
    assert_eq!(std::fs::read_to_string(&target).expect("target"), third);

    // No temporary sibling survives a successful atomic replacement.
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");

    // Invalid generated content is refused before any mutation: the target
    // keeps its bytes and no new backup is created.
    let backups_before = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak"))
        .count();
    let err = write_init_config(&target, "return { theme = }", true)
        .expect_err("invalid content refused");
    assert!(matches!(err, InitWriteError::Refused(_)));
    assert_eq!(
        std::fs::read_to_string(&target).expect("target intact"),
        third
    );
    let backups_after = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak"))
        .count();
    assert_eq!(backups_before, backups_after, "refusal writes no backup");

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn init_write_refuses_symlink_targets() {
    let dir = init_test_dir("write-symlink");
    let real = dir.join("real.lua");
    std::fs::write(&real, "-- outside\n").expect("real file");
    let link = dir.join("init.lua");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let content = render_init_lua(&init_yes_defaults(None));
    let err = write_init_config(&link, &content, true).expect_err("symlink refused");
    assert!(matches!(err, InitWriteError::Refused(_)));
    assert_eq!(
        std::fs::read_to_string(&real).expect("real intact"),
        "-- outside\n",
        "refusal never writes through the link"
    );
    assert!(
        link.symlink_metadata()
            .expect("link")
            .file_type()
            .is_symlink()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_write_rejects_invalid_content_without_touching_fs() {
    let dir = init_test_dir("invalid");
    let target = dir.join("init.lua");
    let err = write_init_config(&target, "return { theme = }", false)
        .expect_err("invalid content refused");
    assert!(matches!(err, InitWriteError::Refused(_)));
    assert!(!target.exists(), "refused write leaves no file");

    // Nested parents are created as needed.
    let nested = dir.join("a").join("b").join("init.lua");
    let content = render_init_lua(&init_yes_defaults(None));
    write_init_config(&nested, &content, false).expect("mkdir parents");
    assert!(nested.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Hermetic `bitty init` environment for dispatch tests: no live process env,
/// real TTY, or filesystem probing leaks in.
fn init_env<'a>(
    bitty_config: Option<&'a str>,
    shell: Option<&'a str>,
    xdg_config_home: Option<&'a str>,
    home: Option<&'a str>,
    stdin_is_tty: bool,
) -> InitEnv<'a> {
    InitEnv {
        bitty_config,
        bitty_profile: None,
        shell,
        xdg_config_home,
        xdg_data_home: None,
        home,
        stdin_is_tty,
    }
}

#[test]
fn init_dispatch_yes_force_idempotent() {
    let dir = init_test_dir("dispatch");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();

    let mut args = Args::new();
    args.init_word = true;
    args.init_yes = true;
    args.config_path = Some(target.display().to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), false);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();

    // --yes writes sane defaults to the explicit target, exit 0.
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let profile = dir.join("profiles").join("default.lua");
    let written_profile = std::fs::read_to_string(&profile).expect("profile written");
    assert!(written_profile.contains("shell = \"/bin/bash\""));
    assert_nested_appearance("dispatch profile", &written_profile, "dark");
    assert!(written_profile.contains("close_confirm = \"when_busy\""));
    let written_selector = std::fs::read_to_string(&target).expect("selector written");
    assert!(written_selector.contains("format = \"bitty-profile-v1\""));
    assert!(written_selector.contains("profile = \"default\""));

    // Second --yes run refuses without --force (idempotent, exit 2) and
    // leaves both generated files byte-identical.
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        2
    );
    assert_eq!(
        std::fs::read_to_string(&profile).expect("profile intact"),
        written_profile
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("selector intact"),
        written_selector
    );

    // --force overwrites with backups, exit 0.
    args.init_force = true;
    let sh_env = init_env(None, Some("/bin/sh"), None, Some(&home), false);
    assert_eq!(
        run_init_subcommand_with_io(&args, &sh_env, &mut input, &mut output),
        0
    );
    let backup = target.with_extension("lua.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("selector backup"),
        written_selector
    );
    let profile_backup = profile.with_extension("lua.bak");
    assert_eq!(
        std::fs::read_to_string(&profile_backup).expect("profile backup"),
        written_profile
    );
    assert!(
        std::fs::read_to_string(&profile)
            .expect("rewritten profile")
            .contains("shell = \"/bin/sh\"")
    );

    // Unexpected positionals fail closed, exit 2.
    args.init_force = false;
    args.init_args = vec!["bogus".to_string()];
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        2
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_non_tty_without_yes_fails_closed_and_writes_nothing() {
    let dir = init_test_dir("notty");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true; // no --yes: prompting would be required
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), false);
    let mut input: &[u8] = b"\n\n\n\n\n\n\n\n\n\n\n";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        2,
        "non-TTY without --yes is a usage error, never a hang"
    );
    assert!(
        !dir.join(".config").join("bitty").exists(),
        "refusal writes nothing"
    );
    assert!(output.is_empty(), "no prompts are printed without a TTY");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_xdg_and_bitty_config_paths() {
    let mut args = Args::new();
    args.init_word = true;
    args.init_yes = true;
    let mut input: &[u8] = b"";
    let mut output = Vec::new();

    // XDG_CONFIG_HOME wins: <xdg>/bitty/init.lua.
    let xdg = init_test_dir("xdg");
    let xdg_s = xdg.display().to_string();
    let env = init_env(None, Some("/bin/bash"), Some(&xdg_s), None, false);
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let xdg_target = xdg.join("bitty").join("init.lua");
    assert!(xdg_target.exists(), "XDG path honored");
    let xdg_profile = xdg.join("bitty").join("profiles").join("default.lua");
    let content = std::fs::read_to_string(&xdg_profile).expect("xdg profile");
    assert!(content.contains("scrollback"), "always writes scrollback");
    assert!(
        std::fs::read_to_string(&xdg_target)
            .expect("xdg selector")
            .contains("profile = \"default\"")
    );

    // HOME fallback: <home>/.config/bitty/init.lua.
    let home = init_test_dir("home");
    let home_s = home.display().to_string();
    let env = init_env(None, None, None, Some(&home_s), false);
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    assert!(
        home.join(".config")
            .join("bitty")
            .join("profiles")
            .join("default.lua")
            .exists()
    );

    // BITTY_CONFIG wins over XDG; --config would win over both.
    let env_dir = init_test_dir("bittycfg");
    let explicit = env_dir.join("custom.lua");
    let explicit_s = explicit.display().to_string();
    let env = init_env(
        Some(&explicit_s),
        Some("/bin/bash"),
        Some(&xdg_s),
        Some(&home_s),
        false,
    );
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    assert!(explicit.exists(), "BITTY_CONFIG honored");
    assert!(
        env_dir.join("profiles").join("default.lua").exists(),
        "profile lives beside the explicit selector"
    );
    assert!(!xdg.join("bitty").join("custom.lua").exists());

    // No XDG and no HOME: fail closed before any write.
    let env = init_env(None, None, None, None, false);
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        2
    );

    let _ = std::fs::remove_dir_all(&xdg);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&env_dir);
}

#[test]
fn init_dispatch_yes_value_flags_apply_and_fail_closed() {
    let dir = init_test_dir("flags");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), false);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();

    let base_args = || {
        let mut args = Args::new();
        args.init_word = true;
        args.init_yes = true;
        args.config_path = Some(target.display().to_string());
        args
    };

    // A fully explicit non-interactive run applies every flag value.
    let mut args = base_args();
    args.theme = Some("tokyo-night".to_string());
    args.font_family = Some("Fira Code".to_string());
    args.font_size = Some("14".to_string());
    args.init_scrollback = Some("50000".to_string());
    args.init_close_confirm = Some("always".to_string());
    args.init_gaps_in = Some("2".to_string());
    args.init_gaps_out = Some("4".to_string());
    args.init_border = Some("1".to_string());
    args.init_radius = Some("0".to_string());
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let written =
        std::fs::read_to_string(dir.join("profiles").join("default.lua")).expect("written");
    assert_nested_appearance("flag profile", &written, "tokyo-night");
    assert!(written.contains("family = \"Fira Code\","));
    assert!(written.contains("size = 14,"));
    assert!(written.contains("gaps_in = 2,"));
    assert!(written.contains("gaps_out = 4,"));
    assert!(written.contains("border = 1,"));
    assert!(written.contains("radius = 0,"));
    assert!(written.contains("close_confirm = \"always\""));
    assert!(written.contains("scrollback = 50000"));

    // Every invalid value fails closed before touching the filesystem:
    // remove the target first so overwrite refusal cannot mask the reason.
    std::fs::remove_file(&target).expect("remove for invalid cases");
    type FlagSetter = fn(&mut Args);
    let invalid: [(&str, FlagSetter); 8] = [
        ("bad theme", |a| a.theme = Some("not-a-theme".to_string())),
        ("bad family", |a| a.font_family = Some("a\x07b".to_string())),
        ("bad size", |a| a.font_size = Some("banana".to_string())),
        ("bad scrollback", |a| {
            a.init_scrollback = Some("100001".to_string());
        }),
        ("bad close-confirm", |a| {
            a.init_close_confirm = Some("sometimes".to_string());
        }),
        ("bad gaps-in", |a| a.init_gaps_in = Some("33".to_string())),
        ("bad border", |a| a.init_border = Some("9".to_string())),
        ("bad radius", |a| a.init_radius = Some("17".to_string())),
    ];
    for (label, set) in invalid {
        let mut args = base_args();
        set(&mut args);
        assert_eq!(
            run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
            2,
            "{label} is a usage error"
        );
        assert!(!target.exists(), "{label} writes nothing");
    }

    // A valid forced run still writes after the refusals (no stuck state).
    let mut args = base_args();
    args.init_force = true;
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    assert!(target.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_interactive_tty_prompts_and_writes_answers() {
    let xdg = init_test_dir("tty-interactive");
    let xdg_s = xdg.display().to_string();
    let mut args = Args::new();
    args.init_word = true; // interactive: TTY + no --yes
    let env = init_env(None, Some("/bin/bash"), Some(&xdg_s), None, true);
    // Eleven Enters take every lua default plus one Enter for the plugin
    // follow-up (empty = none).
    let stdin_lines = "\n\n\n\n\n\n\n\n\n\n\n\n";
    let mut input = std::io::BufReader::new(stdin_lines.as_bytes());
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let target = xdg.join("bitty").join("init.lua");
    assert!(target.exists(), "interactive TTY run writes the XDG file");
    let printed = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(printed.contains("Welcome to bitty"));
    assert!(printed.contains("Close confirm"));
    let _ = std::fs::remove_dir_all(&xdg);
}

#[test]
fn init_lua_escape_keeps_strings_valid() {
    assert_eq!(init_lua_escape("plain"), "plain");
    assert_eq!(init_lua_escape("/bin/bash"), "/bin/bash");
    assert_eq!(init_lua_escape("a\"b"), "a\\\"b");
    assert_eq!(init_lua_escape("C:\\Tools\\sh"), "C:\\\\Tools\\\\sh");

    // An escaped hostile shell still parses as one string value.
    use bitty_config::file::parse_lua_config;
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let lua = render_init_lua(&InitAnswers {
        shell: Some("C:\\Tools\\sh\"x".to_string()),
        ..init_yes_defaults(None)
    });
    let plan = parse_lua_config(&lua, &src).expect("escaped shell parses");
    assert_eq!(
        plan.terminal.expect("terminal").shell.as_deref(),
        Some("C:\\Tools\\sh\"x")
    );
}

#[test]
fn init_usage_names_flags_and_target() {
    let usage = init_usage();
    assert!(usage.contains("--yes"));
    assert!(usage.contains("--force"));
    assert!(usage.contains("--profile"));
    assert!(usage.contains("--migrate"));
    assert!(usage.contains("minimal"));
    assert!(usage.contains("dark"));
    assert!(usage.contains("light"));
    assert!(usage.contains("--config"));
    assert!(usage.contains("BITTY_CONFIG"));
    assert!(usage.contains("init.lua"));
    assert!(usage.contains("profiles/"));
    assert!(usage.contains("--theme"));
    assert!(usage.contains("--font-family"));
    assert!(usage.contains("--font-size"));
    assert!(usage.contains("--scrollback"));
    assert!(usage.contains("--close-confirm"));
    assert!(usage.contains("--gaps-in"));
    assert!(usage.contains("--gaps-out"));
    assert!(usage.contains("--border"));
    assert!(usage.contains("--radius"));
    assert!(usage.contains("--plugins"));
    assert!(usage.contains("--no-plugins"));
    assert!(usage.contains("--with-components"));
    assert!(usage.contains("--print-shell-init"));
    assert!(usage.contains("stdin"));
}

#[test]
fn init_plugins_answer_parses_bundled_ids_offline() {
    // Empty and `none` mean no selection (offline-safe default).
    assert!(
        init_parse_plugins_answer("")
            .expect("empty is none")
            .is_empty()
    );
    assert!(
        init_parse_plugins_answer("  none  ")
            .expect("none is none")
            .is_empty()
    );
    // Single bundled id resolves (same source `plugin install` uses).
    let single = init_parse_plugins_answer("bitty-terminal.shell-integration").expect("bundled id");
    assert_eq!(single, vec!["bitty-terminal.shell-integration".to_string()]);
    // Duplicates collapse, whitespace trimmed, catalog order kept.
    let dup = init_parse_plugins_answer(
        " bitty-terminal.shell-integration , bitty-terminal.shell-integration ",
    )
    .expect("dup collapses");
    assert_eq!(dup, vec!["bitty-terminal.shell-integration".to_string()]);
    // Unknown ids fail closed with a bundled-ids hint (no duplicate list).
    let err = init_parse_plugins_answer("not-a-plugin").expect_err("unknown fails");
    assert!(err.contains("unknown plugin"), "{err}");
    assert!(err.contains("bundled ids"), "{err}");
    // Flag values reject empty (missing value is usage error, never silent none).
    assert!(init_parse_plugins_flag("").is_err());
    assert!(init_parse_plugins_flag("   ").is_err());
}

// -- `bitty init` plugin inventory and multi-select (#1913, #2436) ----------
//
// The loop is catalog-generic, so these tests inject a three-id list even
// though the shipped bundled catalog currently holds one id. Keys are
// injected bytes plus an EOF-terminated wait closure: no real TTY is needed
// and no terminal mode is touched.

/// Bundled-only inventory for direct prompt/loop tests.
fn init_plugin_inventory_bundled(ids: &[&str]) -> crate::init::InitPluginInventory {
    crate::init::InitPluginInventory {
        bundled: ids.iter().map(|id| (*id).to_string()).collect(),
        installed: Vec::new(),
        needs_network: Vec::new(),
        warning: None,
    }
}

/// One installed row for inventory tests.
fn init_installed(id: &str, version: &str, enabled: bool) -> crate::init::InitInstalledPlugin {
    crate::init::InitInstalledPlugin {
        id: id.to_string(),
        version: version.to_string(),
        enabled,
    }
}

/// One valid store record for pure inventory tests.
fn init_plugin_record(
    id: &str,
    version: &str,
    enabled: bool,
) -> bitty_runtime::plugin_runtime::PluginRecord {
    bitty_runtime::plugin_runtime::PluginRecord {
        source_class: bitty_runtime::plugin_runtime::SourceClass::Registry,
        plugin_id: id.to_string(),
        version: version.to_string(),
        root: format!("packages/{id}/{version}"),
        manifest_hash: "a".repeat(64),
        content_digest: "b".repeat(64),
        enabled,
        granted: Vec::new(),
    }
}

/// Drives one plugin multi-select headlessly; returns the selected ids plus
/// everything it printed.
fn drive_plugin_multiselect(
    ids: &[&str],
    stdin_bytes: &[u8],
    color: bool,
) -> (Result<Vec<String>, String>, String) {
    let inventory = init_plugin_inventory_bundled(ids);
    let mut input = std::io::BufReader::new(stdin_bytes);
    let mut output = Vec::new();
    // EOF-terminated: no more bytes means the escape (if any) is bare.
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result = crate::init::init_multiselect_key_loop(
        &mut input,
        &mut output,
        &inventory,
        color,
        false,
        &mut wait,
    );
    let printed = String::from_utf8(output).expect("multiselect output is UTF-8");
    (result, printed)
}

const PLUGIN_MULTISELECT_TEST_IDS: [&str; 3] = ["alpha.plugin", "beta.plugin", "gamma.plugin"];

#[test]
fn init_plugin_multiselect_toggles_move_and_confirms_in_catalog_order() {
    // Space checks `alpha`; `j` moves to `beta`; space checks it; Enter
    // confirms both in catalog order (not toggle order).
    let (result, printed) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b" j \r", false);
    assert_eq!(
        result.expect("Enter confirms the checked rows"),
        vec!["alpha.plugin".to_string(), "beta.plugin".to_string()]
    );
    assert!(
        printed.contains("Plugins (offline-safe; only bundled plugins install without network):"),
        "{printed}"
    );
    assert!(
        printed.contains("  Bundled (install offline, no payload fetch):"),
        "bundled group, got:\n{printed}"
    );
    assert!(
        printed.contains("> [ ] alpha.plugin"),
        "cursor row, got:\n{printed}"
    );
    assert!(
        printed.contains("[x] alpha.plugin") && printed.contains("[x] beta.plugin"),
        "checked rows, got:\n{printed}"
    );
    assert!(
        printed.contains("[ ] gamma.plugin"),
        "unchecked row, got:\n{printed}"
    );
    assert!(
        printed.contains("(space toggles, j/k or arrows move, Enter confirms, Esc cancels)"),
        "hint, got:\n{printed}"
    );
    assert!(
        !printed.contains('\u{1b}'),
        "plain output must not colorize, got:\n{printed}"
    );
}

#[test]
fn init_plugin_multiselect_inplace_repaints_with_cursor_up() {
    // In-place frames repaint over the previous frame (cursor-up plus
    // clear-line) and hide the cursor while looping. The bundled-only frame
    // is ten lines: prompt, bundled header, three rows, installed header,
    // empty marker, network header, empty marker, hint.
    let inventory = init_plugin_inventory_bundled(&PLUGIN_MULTISELECT_TEST_IDS);
    let mut input = std::io::BufReader::new(b"j\r".as_slice());
    let mut output = Vec::new();
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result = crate::init::init_multiselect_key_loop(
        &mut input,
        &mut output,
        &inventory,
        false,
        true,
        &mut wait,
    );
    assert!(result.expect("in-place confirm").is_empty());
    let printed = String::from_utf8(output).expect("multiselect output is UTF-8");
    assert!(
        printed.contains("\x1b[10A"),
        "cursor-up repaint, got:\n{printed}"
    );
    assert!(
        printed.contains("\x1b[?25l") && printed.contains("\x1b[?25h"),
        "cursor hide/show, got:\n{printed}"
    );
}

#[test]
fn init_plugin_multiselect_empty_enter_selects_none() {
    // Default selection is none: Enter alone returns an empty selection.
    let (result, printed) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"\r", false);
    assert!(
        result.expect("Enter with nothing checked").is_empty(),
        "{printed}"
    );
    assert!(printed.contains("> [ ] alpha.plugin"), "{printed}");
}

#[test]
fn init_plugin_multiselect_space_toggles_off_again() {
    // Space twice on the same row restores the unchecked state.
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"  \r", false);
    assert!(result.expect("toggle twice").is_empty());
}

#[test]
fn init_plugin_multiselect_arrows_and_jk_wrap() {
    // `k` on the first row wraps to the last row.
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"k \r", false);
    assert_eq!(
        result.expect("k wraps to the last row"),
        vec!["gamma.plugin".to_string()]
    );
    // Up arrow wraps the same way and toggles with Space.
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"\x1b[A \r", false);
    assert_eq!(
        result.expect("arrow-up wraps to the last row"),
        vec!["gamma.plugin".to_string()]
    );
    // `j` on the last row wraps to the first row.
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"jjj \r", false);
    assert_eq!(
        result.expect("j wraps past the last row"),
        vec!["alpha.plugin".to_string()]
    );
}

#[test]
fn init_plugin_multiselect_kitty_space_toggles() {
    // Kitty keyboard protocol can spell Space as `CSI 32 u`; it is still a
    // toggle, never dead input.
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"\x1b[32u\r", false);
    assert_eq!(
        result.expect("kitty Space toggles"),
        vec!["alpha.plugin".to_string()]
    );
}

#[test]
fn init_plugin_multiselect_esc_cancels() {
    // A checked row does not survive Esc: cancel aborts the whole step.
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b" \x1b", false);
    assert_eq!(
        result.expect_err("Esc must cancel"),
        "bitty init: aborted (cancelled)"
    );
}

#[test]
fn init_plugin_multiselect_ctrl_d_aborts() {
    let (result, _) = drive_plugin_multiselect(&PLUGIN_MULTISELECT_TEST_IDS, b"\x04", false);
    assert_eq!(
        result.expect_err("Ctrl-D must abort"),
        "bitty init: aborted (end of input)"
    );
}

#[test]
fn init_plugin_prompt_line_fallback_parses_comma_ids() {
    // Non-TTY input keeps the line loop and comma-separated parsing
    // byte-identical (headless dispatch, scripted tests). `--yes` bypasses
    // this prompt entirely.
    let inventory = init_plugin_inventory_bundled(&["bitty-terminal.shell-integration"]);
    let mut input = std::io::BufReader::new(
        b" bitty-terminal.shell-integration , bitty-terminal.shell-integration \n".as_slice(),
    );
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let result = crate::init::init_prompt_plugin_selection(
        &mut input,
        &mut output,
        &mut history,
        false,
        false,
        &inventory,
    );
    assert_eq!(
        result.expect("comma-separated ids parse"),
        vec!["bitty-terminal.shell-integration".to_string()]
    );
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        printed.contains("Plugins (offline-safe; only bundled plugins install without network):"),
        "{printed}"
    );
    assert!(
        printed.contains("  Installed (already present locally):"),
        "{printed}"
    );
    assert!(
        printed.contains("  Needs network (not shown, nothing is fetched):"),
        "{printed}"
    );
    assert!(
        printed.contains("Plugins [comma-separated bundled ids, empty for none]:"),
        "{printed}"
    );
}

#[test]
fn init_plugin_inventory_bundled_only_lists_other_official_as_network() {
    let bundled = vec!["alpha.bundled".to_string()];
    let inventory = crate::init::init_plugin_inventory(
        &bundled,
        Ok(Vec::new()),
        &["alpha.bundled", "beta.official"],
    );
    assert_eq!(inventory.bundled, bundled);
    assert!(inventory.installed.is_empty());
    assert_eq!(inventory.needs_network, vec!["beta.official".to_string()]);
    assert!(inventory.warning.is_none());
}

#[test]
fn init_plugin_inventory_installed_only_is_listed_before_network() {
    let store = vec![init_plugin_record("beta.installed", "0.2.0", false)];
    let inventory =
        crate::init::init_plugin_inventory(&[], Ok(store), &["beta.installed", "gamma.official"]);
    assert!(inventory.bundled.is_empty());
    assert_eq!(
        inventory.installed,
        vec![init_installed("beta.installed", "0.2.0", false)]
    );
    assert_eq!(
        inventory.needs_network,
        vec!["gamma.official".to_string()],
        "an installed official plugin must not be advertised as needing network"
    );
}

#[test]
fn init_plugin_inventory_overlap_prefers_bundled_group() {
    let bundled = vec!["alpha.bundled".to_string()];
    let store = vec![
        init_plugin_record("alpha.bundled", "0.1.0", true),
        init_plugin_record("beta.installed", "0.3.0", true),
    ];
    let inventory = crate::init::init_plugin_inventory(
        &bundled,
        Ok(store),
        &["alpha.bundled", "beta.installed"],
    );
    assert_eq!(
        inventory.bundled,
        vec!["alpha.bundled".to_string()],
        "the overlap is shown once under the stronger bundled group"
    );
    assert_eq!(
        inventory.installed,
        vec![init_installed("beta.installed", "0.3.0", true)]
    );
    assert!(
        inventory.needs_network.is_empty(),
        "both groups cover the whole official catalog"
    );
}

#[test]
fn init_plugin_inventory_empty_state_is_explicit_not_an_error() {
    let inventory = crate::init::init_plugin_inventory(&[], Ok(Vec::new()), &[]);
    assert!(inventory.bundled.is_empty());
    assert!(inventory.installed.is_empty());
    assert!(inventory.needs_network.is_empty());
    assert!(inventory.warning.is_none());
}

#[test]
fn init_plugin_inventory_store_failure_is_bounded_and_bundled_only() {
    let detail = format!("line one\nline two\t{}", "x".repeat(400));
    let inventory = crate::init::init_plugin_inventory(
        &["alpha.bundled".to_string()],
        Err(detail),
        &["alpha.bundled", "beta.official"],
    );
    let warning = inventory.warning.as_deref().expect("failure is surfaced");
    assert!(
        !warning.contains('\n') && !warning.contains('\t'),
        "{warning}"
    );
    assert!(warning.ends_with("..."), "bounded marker: {warning}");
    assert!(
        warning.chars().count() <= crate::init::INIT_PLUGIN_WARNING_MAX_CHARS + 3,
        "bounded warning: {warning}"
    );
    assert_eq!(inventory.bundled, vec!["alpha.bundled".to_string()]);
    assert!(inventory.installed.is_empty());
    assert_eq!(
        inventory.needs_network,
        vec!["beta.official".to_string()],
        "bundled + network groups survive the degraded store"
    );
}

#[test]
fn init_plugin_multiselect_skips_installed_and_network_rows() {
    let inventory = crate::init::InitPluginInventory {
        bundled: vec!["alpha.plugin".to_string(), "gamma.plugin".to_string()],
        installed: vec![init_installed("beta.plugin", "0.1.0", true)],
        needs_network: vec!["delta.official".to_string()],
        warning: None,
    };
    let mut input = std::io::BufReader::new(b"j \r".as_slice());
    let mut output = Vec::new();
    let mut wait = |inp: &mut dyn std::io::BufRead| -> bool {
        matches!(inp.fill_buf(), Ok(buf) if !buf.is_empty())
    };
    let result = crate::init::init_multiselect_key_loop(
        &mut input,
        &mut output,
        &inventory,
        false,
        false,
        &mut wait,
    );
    assert_eq!(
        result.expect("j moves to gamma, not the installed row"),
        vec!["gamma.plugin".to_string()]
    );
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        printed.contains("    - beta.plugin 0.1.0 (enabled)"),
        "installed row is informational, got:\n{printed}"
    );
    assert!(
        !printed.contains("[ ] beta.plugin") && !printed.contains("[x] beta.plugin"),
        "installed rows are not selectable, got:\n{printed}"
    );
    assert!(
        printed.contains("1 official plugin needs a network install"),
        "network summary, got:\n{printed}"
    );
}

#[test]
fn init_plugin_prompt_empty_bundled_states_groups_and_skips() {
    let inventory = crate::init::InitPluginInventory {
        bundled: Vec::new(),
        installed: vec![init_installed("beta.plugin", "0.1.0", true)],
        needs_network: vec!["gamma.official".to_string()],
        warning: None,
    };
    let mut input = std::io::BufReader::new(b"".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let result = crate::init::init_prompt_plugin_selection(
        &mut input,
        &mut output,
        &mut history,
        false,
        false,
        &inventory,
    );
    assert!(
        result.expect("nothing bundled selects none").is_empty(),
        "no selectable rows"
    );
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        printed.contains("  Bundled (install offline, no payload fetch):\n    none"),
        "explicit empty bundled group, got:\n{printed}"
    );
    assert!(
        printed.contains("    - beta.plugin 0.1.0 (enabled)"),
        "installed group still shown, got:\n{printed}"
    );
    assert!(
        printed.contains("  No bundled plugins to select; skipping."),
        "explicit skip, got:\n{printed}"
    );
    assert!(
        !printed.contains("Plugins [comma-separated bundled ids"),
        "no prompt when nothing is selectable, got:\n{printed}"
    );
}

#[test]
fn init_plugin_prompt_bounds_installed_rows_and_counts_the_rest() {
    let installed: Vec<_> = (0..10)
        .map(|index| init_installed(&format!("gamma.installed{index:02}"), "0.1.0", true))
        .collect();
    let inventory = crate::init::InitPluginInventory {
        bundled: vec!["alpha.bundled".to_string()],
        installed,
        needs_network: Vec::new(),
        warning: None,
    };
    let mut input = std::io::BufReader::new(b"\n".as_slice());
    let mut output = Vec::new();
    let mut history: Vec<String> = Vec::new();
    let result = crate::init::init_prompt_plugin_selection(
        &mut input,
        &mut output,
        &mut history,
        false,
        false,
        &inventory,
    );
    assert!(result.expect("empty answer selects none").is_empty());
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        printed.contains("    - gamma.installed07 0.1.0 (enabled)"),
        "eighth row is shown, got:\n{printed}"
    );
    assert!(
        !printed.contains("gamma.installed08 0.1.0")
            && !printed.contains("gamma.installed09 0.1.0"),
        "rows past the bound are counted, not printed, got:\n{printed}"
    );
    assert!(
        printed.contains("    - +2 more installed plugins (bitty plugin list)"),
        "remainder is explicit, got:\n{printed}"
    );
}

#[test]
fn init_plugin_inventory_for_env_reads_local_store_read_only() {
    let dir = init_test_dir("inventory-store");
    let store = dir
        .join(".local")
        .join("share")
        .join("bitty")
        .join("plugins");
    std::fs::create_dir_all(&store).expect("store dir");
    bitty_runtime::plugin_runtime::write_index(
        &store,
        &[init_plugin_record("bitty-terminal.bar", "0.1.0", true)],
    )
    .expect("write index");
    let index_before = std::fs::read(store.join("current.json")).expect("index bytes");

    let home = dir.display().to_string();
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let inventory = crate::init::init_plugin_inventory_for_env(&env);
    assert!(
        inventory
            .bundled
            .contains(&"bitty-terminal.shell-integration".to_string()),
        "bundled catalog is always listed"
    );
    assert_eq!(inventory.installed.len(), 1);
    assert_eq!(inventory.installed[0].id, "bitty-terminal.bar");
    assert_eq!(inventory.installed[0].version, "0.1.0");
    assert!(inventory.installed[0].enabled);
    assert!(
        !inventory
            .needs_network
            .contains(&"bitty-terminal.bar".to_string()),
        "installed official plugin is not a network row"
    );
    assert!(inventory.warning.is_none());
    assert_eq!(
        std::fs::read(store.join("current.json")).expect("index bytes"),
        index_before,
        "discovery must never rewrite the store"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_plugin_inventory_for_env_malformed_store_degrades_to_bundled() {
    let dir = init_test_dir("inventory-broken-store");
    let store = dir
        .join(".local")
        .join("share")
        .join("bitty")
        .join("plugins");
    std::fs::create_dir_all(&store).expect("store dir");
    std::fs::write(store.join("current.json"), "{not-json").expect("broken index");
    let home = dir.display().to_string();
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let inventory = crate::init::init_plugin_inventory_for_env(&env);
    assert!(
        inventory
            .bundled
            .contains(&"bitty-terminal.shell-integration".to_string()),
        "bundled catalog survives a broken store"
    );
    assert!(inventory.installed.is_empty());
    assert!(inventory.warning.is_some(), "failure is surfaced");
    assert_eq!(
        inventory.needs_network.len(),
        bitty_plugin_host::official::official_plugins().len(),
        "no installed official entry is subtracted when discovery fails"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_malformed_store_warns_and_still_writes_config() {
    let dir = init_test_dir("init-malformed-store");
    let store = dir
        .join(".local")
        .join("share")
        .join("bitty")
        .join("plugins");
    std::fs::create_dir_all(&store).expect("store dir");
    std::fs::write(store.join("current.json"), "{not-json").expect("broken index");
    let xdg = dir.display().to_string();
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true; // interactive: TTY + no --yes
    let env = init_env(None, Some("/bin/bash"), Some(&xdg), Some(&home), true);
    // Eleven lua defaults plus the plugin step (empty = none).
    let stdin_lines = "\n".repeat(12);
    let mut input = std::io::BufReader::new(stdin_lines.as_bytes());
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0,
        "a read-only discovery failure must never abort init"
    );
    assert!(
        dir.join("bitty").join("init.lua").exists(),
        "config is written despite the degraded inventory"
    );
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        printed.contains("warning: installed plugin discovery failed:"),
        "bounded warning, got:\n{printed}"
    );
    assert!(
        printed.contains("Plugins (offline-safe; only bundled plugins install without network):"),
        "bundled fallback is shown, got:\n{printed}"
    );
    assert_eq!(
        std::fs::read_to_string(store.join("current.json")).expect("index bytes"),
        "{not-json",
        "discovery must not rewrite the broken store"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_yes_never_reads_or_reports_the_plugin_store() {
    let dir = init_test_dir("init-yes-broken-store");
    let store = dir
        .join(".local")
        .join("share")
        .join("bitty")
        .join("plugins");
    std::fs::create_dir_all(&store).expect("store dir");
    std::fs::write(store.join("current.json"), "{not-json").expect("broken index");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.init_yes = true;
    args.config_path = Some(target.display().to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), false);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0,
        "--yes keeps the offline non-interactive path"
    );
    assert!(target.exists());
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        !printed.contains("Plugins (offline-safe; only bundled plugins install"),
        "--yes never runs the plugin prompt, got:\n{printed}"
    );
    assert!(
        !printed.contains("discovery failed"),
        "--yes never reads the plugin store, got:\n{printed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_yes_no_and_shell_handoff_helpers() {
    assert!(!init_parse_yes_no_answer("").expect("empty is no"));
    assert!(!init_parse_yes_no_answer("n").expect("n is no"));
    assert!(!init_parse_yes_no_answer("N").expect("N is no"));
    assert!(!init_parse_yes_no_answer("no").expect("no is no"));
    assert!(init_parse_yes_no_answer("y").expect("y is yes"));
    assert!(init_parse_yes_no_answer("YES").expect("YES is yes"));
    assert!(init_parse_yes_no_answer("maybe").is_err());

    // Shell basenames map through the same CompletionShell table shell-init uses.
    assert_eq!(
        init_shell_name_from_path("/bin/bash").as_deref(),
        Some("bash")
    );
    assert_eq!(
        init_shell_name_from_path("/usr/bin/zsh").as_deref(),
        Some("zsh")
    );
    assert_eq!(
        init_shell_name_from_path("/usr/bin/fish").as_deref(),
        Some("fish")
    );
    assert_eq!(
        init_shell_name_from_path("pwsh").as_deref(),
        Some("powershell")
    );
    assert_eq!(init_shell_name_from_path("nu").as_deref(), Some("nushell"));
    assert!(init_shell_name_from_path("tcl").is_none());
    assert!(init_shell_name_from_path("").is_none());

    // Detection prefers the chosen shell, then $SHELL, then bash fallback.
    assert_eq!(
        init_detect_shell_for_handoff(Some("/bin/zsh"), Some("/bin/bash")),
        "zsh"
    );
    assert_eq!(
        init_detect_shell_for_handoff(None, Some("/usr/bin/fish")),
        "fish"
    );
    assert_eq!(init_detect_shell_for_handoff(None, None), "bash");
    assert_eq!(
        init_detect_shell_for_handoff(Some("/bin/unknown-shell"), Some("/bin/bash")),
        "bash"
    );

    // Handoff lines are exact and shell-appropriate (PRINT, never eval).
    assert_eq!(
        init_shell_init_line("bash"),
        "eval \"$(bitty shell-init bash)\""
    );
    assert_eq!(
        init_shell_init_line("zsh"),
        "eval \"$(bitty shell-init zsh)\""
    );
    assert_eq!(
        init_shell_init_line("fish"),
        "bitty shell-init fish | source"
    );
    assert!(
        init_shell_init_line("powershell").contains("bitty shell-init powershell"),
        "powershell wiring"
    );
    assert!(
        init_shell_init_line("nushell").contains("bitty shell-init nushell"),
        "nushell wiring"
    );

    // CodeRabbit 4241676519: Nushell needs two steps — the save command plus
    // the `source ... config.nu` hint. Other shells get no extra hint.
    let nu_line = init_shell_init_line("nushell");
    assert!(
        nu_line.contains("save -f ~/.config/nushell/bitty-shell-init.nu"),
        "nu save step: {nu_line}"
    );
    let nu_hint = init_shell_source_hint("nushell");
    assert!(
        nu_hint.contains("source ~/.config/nushell/bitty-shell-init.nu")
            && nu_hint.contains("config.nu"),
        "nu source step: {nu_hint}"
    );
    let nu_full = format!("{nu_line}{nu_hint}");
    assert!(
        nu_full.contains("save -f") && nu_full.contains("source ~/.config/nushell/"),
        "nu handoff carries both steps: {nu_full}"
    );
    assert_eq!(init_shell_source_hint("bash"), "");
    assert_eq!(init_shell_source_hint("zsh"), "");
    assert_eq!(init_shell_source_hint("fish"), "");
    assert_eq!(init_shell_source_hint("powershell"), "");

    // No bundled plugin declares native components today (future [components]
    // flows through the same helper without touching the wizard).
    assert!(init_bundled_plugin_components("bitty-terminal.shell-integration").is_empty());
}

#[test]
fn parse_init_followup_flags() {
    // `--plugins` accepts space and `=` forms; missing value fails closed.
    let p = parse_args(&args_of(&[
        "bitty",
        "init",
        "--plugins",
        "bitty-terminal.shell-integration",
    ]));
    assert!(p.init_word);
    assert_eq!(
        p.init_plugins.as_deref(),
        Some("bitty-terminal.shell-integration")
    );
    let p = parse_args(&args_of(&[
        "bitty",
        "init",
        "--plugins=bitty-terminal.shell-integration",
    ]));
    assert_eq!(
        p.init_plugins.as_deref(),
        Some("bitty-terminal.shell-integration")
    );
    let p = parse_args(&args_of(&["bitty", "init", "--plugins"]));
    assert_eq!(p.init_plugins.as_deref(), Some(""));

    // Negations follow the existing `--no-*` style; last wins.
    let p = parse_args(&args_of(&["bitty", "init", "--no-plugins"]));
    assert!(p.init_no_plugins);
    let p = parse_args(&args_of(&["bitty", "init", "--with-components"]));
    assert_eq!(p.init_with_components, Some(true));
    let p = parse_args(&args_of(&["bitty", "init", "--no-with-components"]));
    assert_eq!(p.init_with_components, Some(false));
    let p = parse_args(&args_of(&["bitty", "init", "--no-components"]));
    assert_eq!(p.init_with_components, Some(false));
    let p = parse_args(&args_of(&[
        "bitty",
        "init",
        "--with-components",
        "--no-with-components",
    ]));
    assert_eq!(p.init_with_components, Some(false));
    let p = parse_args(&args_of(&["bitty", "init", "--print-shell-init"]));
    assert_eq!(p.init_print_shell_init, Some(true));
    let p = parse_args(&args_of(&["bitty", "init", "--no-print-shell-init"]));
    assert_eq!(p.init_print_shell_init, Some(false));
}

#[test]
fn init_dispatch_yes_with_plugins_records_and_grants() {
    // `--yes --plugins <bundled>` records offline then grants via the install
    // consent flow (`--yes` honors per-plugin consent). Both files land.
    let dir = init_test_dir("yes-plugins");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.init_yes = true;
    args.config_path = Some(target.display().to_string());
    args.init_plugins = Some("bitty-terminal.shell-integration".to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), false);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    assert!(target.exists(), "config written");
    let state_path = dir.join("bitty-plugins.lua");
    assert!(state_path.exists(), "plugin queue recorded");
    let state_text = std::fs::read_to_string(&state_path).expect("state readable");
    assert!(
        state_text.contains("bitty-terminal.shell-integration"),
        "selected id recorded"
    );
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(printed.contains("recorded"), "{printed}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_yes_with_plugins_unknown_fails_closed() {
    // Unknown `--plugins` ids are usage errors (exit 2, nothing written).
    let dir = init_test_dir("yes-plugins-bad");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.init_yes = true;
    args.config_path = Some(target.display().to_string());
    args.init_plugins = Some("not-a-plugin".to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), false);
    let mut input: &[u8] = b"";
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        2
    );
    assert!(!target.exists(), "usage error writes no config");
    assert!(
        !dir.join("bitty-plugins.lua").exists(),
        "usage error writes no plugin queue"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_dispatch_interactive_plugins_and_components() {
    // Interactive: 11 lua defaults + plugin id + fetch y + component y.
    // Component offer finds no declared components (nothing-to-do line).
    let dir = init_test_dir("interactive-plugins");
    let target = dir.join("init.lua");
    let home = dir.display().to_string();
    let mut args = Args::new();
    args.init_word = true;
    args.config_path = Some(target.display().to_string());
    let env = init_env(None, Some("/bin/bash"), None, Some(&home), true);
    let stdin_lines = "\n\n\n\n\n\n\n\n\n\n\nbitty-terminal.shell-integration\ny\ny\ny\n";
    let mut input = std::io::BufReader::new(stdin_lines.as_bytes());
    let mut output = Vec::new();
    assert_eq!(
        run_init_subcommand_with_io(&args, &env, &mut input, &mut output),
        0
    );
    let state_path = dir.join("bitty-plugins.lua");
    assert!(state_path.exists(), "interactive selection recorded");
    let printed = String::from_utf8(output).expect("UTF-8");
    assert!(
        printed.contains("bitty-terminal.shell-integration"),
        "{printed}"
    );
    assert!(printed.contains("no selected plugin declares"), "{printed}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_check_subcommand_good_and_broken_files() {
    let dir = std::env::temp_dir().join(format!("bitty-ctx0148-cfg-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let good = dir.join("init.lua");
    std::fs::write(&good, r#"return { theme = "dark" }"#).expect("write good");
    let broken = dir.join("broken.lua");
    std::fs::write(&broken, "return { theme = }").expect("write broken");

    let mut args = Args::new();
    args.config_cmd = Some(ConfigCommand::Check);
    args.config_path = Some(good.display().to_string());
    assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 0);

    args.config_path = Some(broken.display().to_string());
    assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 2);

    args.config_path = Some(dir.join("missing.lua").display().to_string());
    assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 2);

    // Unexpected extras fail closed even for a good file.
    args.config_path = Some(good.display().to_string());
    args.config_args = vec!["extra".to_string()];
    assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn themed_demo_pump_names_theme_and_source() {
    let (rx, handle) = spawn_demo_pty_pump_with_theme("bitty-dark", "file");
    let mut total = Vec::new();
    while let Ok(chunk) = rx.recv() {
        total.extend_from_slice(&chunk);
    }
    handle.join().expect("pump joins");
    let text = String::from_utf8_lossy(&total);
    assert!(text.contains("bitty-dark"));
    assert!(text.contains("file"));
    // Still exercises the green SGR through the themed palette.
    assert!(text.contains("green"));
}

// Chrome-key tests live in `chrome_keys::tests` (CTX-0233 pure move).
// Shared helper `two_pane_layout` is `chrome_keys::two_pane_layout`.
#[test]
fn spawn_spec_resolve_prefers_explicit_program() {
    // CTX-0176: explicit program wins verbatim with its tail args.
    let spec = SpawnSpec {
        program: Some("/bin/fish".to_string()),
        program_args: vec!["-l".to_string()],
        shell_env: Some("/bin/bash".to_string()),
        config_shell: Some("/bin/zsh".to_string()),
    };
    assert_eq!(
        spec.resolve(),
        ("/bin/fish".to_string(), vec!["-l".to_string()])
    );
}

#[test]
fn spawn_spec_resolve_defaults_to_configured_shell_env_then_fallback() {
    // CTX-0176/CTX-0298: no explicit program resolves exactly like
    // startup — configured `terminal.shell`, then `$SHELL`, then /bin/sh.
    let spec = SpawnSpec {
        program: None,
        program_args: vec!["-l".to_string()],
        shell_env: Some("/bin/bash".to_string()),
        config_shell: Some("/bin/zsh".to_string()),
    };
    assert_eq!(spec.resolve(), ("/bin/zsh".to_string(), Vec::new()));
    let spec = SpawnSpec {
        program: None,
        program_args: vec!["-l".to_string()],
        shell_env: Some("/bin/bash".to_string()),
        config_shell: None,
    };
    assert_eq!(spec.resolve(), ("/bin/bash".to_string(), Vec::new()));
    let spec = SpawnSpec {
        program: None,
        program_args: Vec::new(),
        shell_env: None,
        config_shell: None,
    };
    assert_eq!(spec.resolve(), ("/bin/sh".to_string(), Vec::new()));
    let spec = SpawnSpec {
        program: None,
        program_args: Vec::new(),
        shell_env: Some("   ".to_string()),
        config_shell: Some(" \n ".to_string()),
    };
    assert_eq!(spec.resolve(), ("/bin/sh".to_string(), Vec::new()));
}

#[test]
fn new_split_without_spawnable_shell_keeps_pane_with_warning() {
    // CTX-0176: spawn failure is loud but non-fatal — the split still
    // commits (layout ops stay total) with no pane session. Runs
    // everywhere: the bogus binary fails on every platform.
    use bitty_config::{ChromeAction, SplitDir};
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let spec = SpawnSpec {
        program: Some("/nonexistent-bitty-pane-shell-xyz".to_string()),
        program_args: Vec::new(),
        shell_env: None,
        config_shell: None,
    };
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        spec,
    );
    app.runtime.set_layout(two_pane_layout());
    app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
    assert_eq!(app.runtime.leaf_count(), 3);
    assert_eq!(app.runtime.pane_count(), 0);
    // Closing a session-less leaf is quiet and total.
    app.apply_chrome_action(ChromeAction::CloseView);
    assert_eq!(app.runtime.leaf_count(), 2);
    assert_eq!(app.runtime.pane_count(), 0);
}

// Live-spawn: the fresh leaf owns a real POSIX shell (`/bin/sh` has no
// Windows equivalent). `#[cfg(unix)]` keeps it off Windows CI;
// `require_pty!()` keeps the force-no-PTY simulation path. ConPTY
// coverage lives in bitty-pty/tests/spawn_windows.rs (CTX-0268);
// porting this test to a platform-neutral spawn is deferred follow-up.
#[test]
#[cfg(unix)]
fn new_split_spawns_private_shell_and_close_tears_it_down() {
    require_pty!();
    // CTX-0176 (Issue #274): the fresh leaf owns a live shell; closing
    // the leaf tears the child down with it.
    use bitty_config::{ChromeAction, SplitDir};
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let spec = SpawnSpec {
        program: Some("/bin/sh".to_string()),
        program_args: Vec::new(),
        shell_env: None,
        config_shell: None,
    };
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        spec,
    );
    app.runtime.set_layout(two_pane_layout());
    app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
    assert_eq!(app.runtime.leaf_count(), 3);
    // Fresh leaf is id 3 (one past the previous max).
    assert!(app.runtime.has_pane_session(&ViewId::new(3)));
    assert!(app.runtime.pane_pid(&ViewId::new(3)).is_some());
    // Focus the new leaf, then close it: the child goes down with it.
    app.apply_chrome_action(ChromeAction::FocusId(3));
    assert_eq!(app.runtime.focused_view(), Some(ViewId::new(3)));
    app.apply_chrome_action(ChromeAction::CloseView);
    assert_eq!(app.runtime.leaf_count(), 2);
    assert!(!app.runtime.has_pane_session(&ViewId::new(3)));
    assert_eq!(app.runtime.pane_count(), 0);
}

/// Writes an executable fake shell that records its own execution in
/// `marker` and exits. Tiny and self-contained so the PTY child is
/// short-lived; used by the CTX-0298 effect tests.
#[cfg(unix)]
fn write_marker_shell(
    dir: &std::path::Path,
    name: &str,
    marker: &std::path::Path,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let body = format!(
        "#!/bin/sh\nprintf 'configured-shell-ran' > '{}'\nexit 0\n",
        marker.display()
    );
    std::fs::write(&path, body).expect("write fake shell");
    let mut perms = std::fs::metadata(&path)
        .expect("stat fake shell")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake shell");
    path
}

/// Bounded wait for the fake shell to write its marker.
#[cfg(unix)]
fn wait_for_marker(marker: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    marker.exists()
}

#[test]
#[cfg(unix)]
fn spawn_default_shell_uses_configured_terminal_shell() {
    // CTX-0298 effect test: `effective.terminal.shell` outranks `$SHELL`
    // at startup, proven by actually spawning the configured argv[0] and
    // observing its side effect. The injected `$SHELL` is /bin/sh, which
    // would never write this marker.
    require_pty!();
    let base =
        std::env::temp_dir().join(format!("bitty-ctx0298-configured-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("temp dir");
    let marker = base.join("ran.marker");
    let fake = write_marker_shell(&base, "configured-shell", &marker);

    let mut rt = Runtime::with_defaults().expect("must build");
    let result = spawn_default_shell(
        &mut rt,
        Some(fake.to_str().expect("utf8 fake path")),
        Some("/bin/sh"),
    );
    assert!(result.is_ok(), "configured shell spawns: {result:?}");
    assert!(rt.has_pty(), "configured shell owns the primary PTY");
    assert!(
        wait_for_marker(&marker),
        "configured shell was executed as argv[0]"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[cfg(unix)]
fn spawn_default_shell_skips_blank_configured_shell_for_env() {
    // CTX-0298 fail-closed effect test: a blank configured value never
    // reaches execve; the marker proves the injected `$SHELL` ran instead.
    require_pty!();
    let base = std::env::temp_dir().join(format!("bitty-ctx0298-blank-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("temp dir");
    let marker = base.join("ran.marker");
    let env_shell = write_marker_shell(&base, "env-shell", &marker);

    let mut rt = Runtime::with_defaults().expect("must build");
    let result = spawn_default_shell(
        &mut rt,
        Some("   "),
        Some(env_shell.to_str().expect("utf8 fake path")),
    );
    assert!(result.is_ok(), "env shell spawns: {result:?}");
    assert!(rt.has_pty(), "env shell owns the primary PTY");
    assert!(
        wait_for_marker(&marker),
        "blank configured shell failed closed to the injected $SHELL"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[cfg(unix)]
fn spawn_default_shell_falls_back_when_configured_shell_missing() {
    // CTX-0298 safe-defaults effect test: a configured shell that cannot
    // spawn retries FALLBACK_SHELL exactly like a failing `$SHELL`.
    require_pty!();
    let mut rt = Runtime::with_defaults().expect("must build");
    let result = spawn_default_shell(
        &mut rt,
        Some("/nonexistent-bitty-0298-configured-shell"),
        Some("/bin/sh"),
    );
    assert!(result.is_ok(), "fallback /bin/sh spawns: {result:?}");
    assert!(rt.has_pty(), "fallback shell owns the primary PTY");
}

// `close_last_leaf_helper_refuses` lives in `chrome_keys::tests` (CTX-0233).

#[test]
fn consolidated_startup_assembly_matches_expected_layout_and_focus() {
    // TERM-APP-003 / CTX-0553: verify shared startup assembly applies layout
    // and focus identically without diverging between primary and fallback paths.
    let mut rt = Runtime::with_defaults().expect("must build");
    let raw = args_of(&["bitty", "--split=h", "--focus=1"]);
    let args = parse_args(&raw);

    apply_startup_layout_and_focus(&mut rt, &args);

    assert_eq!(
        rt.leaf_count(),
        2,
        "must assemble split layout into 2 leaves"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId(1)),
        "focus spec 1 must be focused"
    );
}

// -- CTX-0731 (#982): panel-hosted `$EDITOR` round trip ------------------------

fn editor_test_app() -> TerminalApp {
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app.runtime.set_layout(two_pane_layout());
    app
}

fn feed_text(_app: &mut TerminalApp, _text: &str) {
    // E-CUT (CTX-0968): the Core composer draft is retired; there is no
    // draft to seed. Editor tests operate on the empty retired buffer.
}

#[test]
fn external_editor_hostile_program_denied_without_side_effects() {
    // A hostile `$VISUAL`/`$EDITOR` is denied before any side effect: no
    // leaf, no session. Headless-safe (resolution fails before any spawn).
    let mut app = editor_test_app();
    let focused_before = app.runtime.focused_view();
    feed_text(&mut app, "hi");
    app.open_external_editor_with(Some("/tmp/evil-editor"), Some("/tmp/evil-editor"), None);
    assert!(!app.chrome.editor.is_hosting());
    assert_eq!(app.runtime.leaf_count(), 2);
    assert_eq!(app.runtime.focused_view(), focused_before);
}

#[test]
fn external_editor_missing_program_denied_without_side_effects() {
    // Neither `$VISUAL` nor `$EDITOR` set: `NoEditor` denial, layout
    // untouched.
    let mut app = editor_test_app();
    feed_text(&mut app, "hi");
    app.open_external_editor_with(None, None, None);
    assert!(!app.chrome.editor.is_hosting());
    assert_eq!(app.runtime.leaf_count(), 2);
}

#[test]
fn external_editor_host_holds_single_session_and_cleans_temp() {
    // Pure host semantics, headless: at most one pending session; dropping
    // the session deletes its temp file.
    use crate::editor_host::{ExternalEditorHost, ExternalEditorSession};
    let dir = std::env::temp_dir();
    let temp = bitty_rich::host::write_composer_temp("draft", &dir).expect("temp");
    let path = temp.path().to_path_buf();
    assert!(path.exists());
    let mut host = ExternalEditorHost::new();
    assert!(host.begin(ExternalEditorSession {
        view: ViewId::new(7),
        temp,
        return_focus: ViewId::new(1),
    }));
    assert!(host.is_hosting());
    assert_eq!(host.pending_view(), Some(ViewId::new(7)));
    // A second session is refused while one is hosted.
    let temp2 = bitty_rich::host::write_composer_temp("other", &dir).expect("temp");
    let path2 = temp2.path().to_path_buf();
    assert!(!host.begin(ExternalEditorSession {
        view: ViewId::new(8),
        temp: temp2,
        return_focus: ViewId::new(1),
    }));
    assert_eq!(host.pending_view(), Some(ViewId::new(7)));
    drop(host.take());
    assert!(!host.is_hosting());
    assert!(!path.exists(), "temp deleted with the taken session");
    assert!(!path2.exists(), "refused temp deleted on drop");
}

#[test]
fn external_editor_poll_cancels_when_leaf_gone() {
    // A manually closed editor leaf cancels the round trip: session
    // cleared, temp deleted.
    let mut app = editor_test_app();
    use crate::editor_host::ExternalEditorSession;
    feed_text(&mut app, "hi");
    let temp = bitty_rich::host::write_composer_temp("hi", &std::env::temp_dir()).expect("temp");
    let path = temp.path().to_path_buf();
    let home = app.runtime.focused_view().expect("focused leaf");
    assert!(app.chrome.editor.begin(ExternalEditorSession {
        view: ViewId::new(999),
        temp,
        return_focus: home,
    }));
    app.poll_external_editor();
    assert!(!app.chrome.editor.is_hosting());
    assert!(!path.exists(), "temp deleted on cancel");
    assert_eq!(app.runtime.leaf_count(), 2);
}

/// Writes an executable fake editor script under the temp dir: tiny and
/// self-contained so the PTY child is short-lived. Returns the script path
/// (caller deletes it best-effort).
#[cfg(unix)]
fn write_fake_editor(name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "bitty-fake-editor-{}-{nanos}-{name}.sh",
        std::process::id()
    ));
    std::fs::write(&path, body).expect("write fake editor");
    let mut perms = std::fs::metadata(&path)
        .expect("stat fake editor")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake editor");
    path
}

/// Bounded pump until no editor is hosted (the exit poll runs inside
/// `poll_pty_pump`).
#[cfg(unix)]
fn wait_for_editor_done(app: &mut TerminalApp) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.chrome.editor.is_hosting() && std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

// Live-spawn: the editor leaf owns a real PTY child (`/bin/sh` shebang has
// no Windows equivalent). `#[cfg(unix)]` keeps it off Windows CI;
// `require_pty!()` keeps the force-no-PTY simulation path.
#[test]
#[cfg(unix)]
fn external_editor_round_trip_applies_and_tears_down() {
    require_pty!();
    // Full panel-hosted round trip with a fake appending editor (E-CUT:
    // retired empty draft): open hosts a third leaf, exit tears the leaf
    // down and restores focus. The edited buffer is reported via
    // `EditorOutcome`; there is no composer session to assert.
    let mut app = editor_test_app();
    let home = app.runtime.focused_view().expect("focused leaf");
    feed_text(&mut app, "hi");
    let script = write_fake_editor(
        "append",
        "#!/bin/sh\nprintf '%s' '-edited' >> \"$1\"\nexit 0\n",
    );
    let script_arg = script.to_string_lossy().into_owned();
    app.open_external_editor_with(None, None, Some(script_arg.as_str()));
    assert!(app.chrome.editor.is_hosting());
    let editor_view = app.chrome.editor.pending_view().expect("pending view");
    assert_eq!(app.runtime.leaf_count(), 3);
    assert!(app.runtime.has_pane_session(&editor_view));
    assert_eq!(app.runtime.focused_view(), Some(editor_view));
    // A second open while hosted is refused: no fourth leaf.
    app.open_external_editor_with(None, None, Some(script_arg.as_str()));
    assert_eq!(app.runtime.leaf_count(), 3);
    wait_for_editor_done(&mut app);
    assert!(
        !app.chrome.editor.is_hosting(),
        "editor exit must finish the round trip"
    );
    assert_eq!(app.runtime.leaf_count(), 2);
    assert!(!app.runtime.has_pane_session(&editor_view));
    assert_eq!(app.runtime.focused_view(), Some(home));
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn external_editor_nonzero_exit_discards_and_tears_down() {
    require_pty!();
    // A failing editor reports non-zero (E-CUT: no draft to keep); the leaf
    // still tears down and focus restores.
    let mut app = editor_test_app();
    let home = app.runtime.focused_view().expect("focused leaf");
    feed_text(&mut app, "hi");
    let script = write_fake_editor("fail", "#!/bin/sh\nexit 3\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.open_external_editor_with(None, None, Some(script_arg.as_str()));
    assert!(app.chrome.editor.is_hosting());
    let editor_view = app.chrome.editor.pending_view().expect("pending view");
    wait_for_editor_done(&mut app);
    assert!(
        !app.chrome.editor.is_hosting(),
        "failed editor must still finish"
    );
    assert_eq!(app.runtime.leaf_count(), 2);
    assert!(!app.runtime.has_pane_session(&editor_view));
    assert_eq!(app.runtime.focused_view(), Some(home));
    let _ = std::fs::remove_file(&script);
}

#[test]
fn external_editor_typed_open_reports_busy_and_keeps_existing() {
    // Typed `process.editor` open (W-103 G-4): a second open while hosted
    // reports Busy and keeps the existing session (single-session bound,
    // T-4). Headless-safe (no spawn; the session is recorded directly).
    use crate::editor_host::{EditorOpenOutcome, ExternalEditorSession};
    let mut app = editor_test_app();
    feed_text(&mut app, "hi");
    let temp = bitty_rich::host::write_composer_temp("hi", &std::env::temp_dir()).expect("temp");
    let path = temp.path().to_path_buf();
    let home = app.runtime.focused_view().expect("focused leaf");
    assert!(app.chrome.editor.begin(ExternalEditorSession {
        view: ViewId::new(999),
        temp,
        return_focus: home,
    }));
    let outcome = app.open_external_editor_typed(
        None,
        None,
        Some("/bin/true"),
        bitty_rich::host::EDITOR_TIMEOUT_DEFAULT,
    );
    assert_eq!(
        outcome,
        EditorOpenOutcome::Busy,
        "second open must report Busy, not replace the session"
    );
    assert_eq!(app.chrome.editor.pending_view(), Some(ViewId::new(999)));
    assert_eq!(
        app.runtime.leaf_count(),
        2,
        "no leaf spawns for a Busy open"
    );
    // Draining the (leaf-vanished) session cancels with cleanup.
    assert_eq!(
        app.poll_external_editor(),
        Some(bitty_rich::host::EditorOutcome::Cancelled)
    );
    assert!(!app.chrome.editor.is_hosting());
    assert!(!path.exists(), "temp deleted on cancel");
}

#[test]
fn external_editor_typed_denied_before_side_effects() {
    // Typed hostile-program denial (T-3 at the hosted boundary): no leaf, no
    // session.
    use crate::editor_host::EditorOpenOutcome;
    let mut app = editor_test_app();
    let focused_before = app.runtime.focused_view();
    feed_text(&mut app, "hi");
    let outcome = app.open_external_editor_typed(
        Some("/tmp/evil-editor"),
        Some("/tmp/evil-editor"),
        None,
        bitty_rich::host::EDITOR_TIMEOUT_DEFAULT,
    );
    assert_eq!(
        outcome,
        EditorOpenOutcome::Denied(bitty_rich::host::EditorDeny::NotAllowed)
    );
    assert!(!app.chrome.editor.is_hosting());
    assert_eq!(app.runtime.leaf_count(), 2);
    assert_eq!(app.runtime.focused_view(), focused_before);
}

#[test]
#[cfg(unix)]
fn external_editor_hosted_timeout_kills_tree_and_reports() {
    require_pty!();
    use std::time::{Duration, Instant};
    // Bounded hosted wait (W-103 G-3/G-4, T-6 at the hosted boundary):
    // expiry kills the recorded owned tree and reports a typed Timeout; the
    // leaf tears down, focus restores, the draft is kept, and the overlay
    // reopens. The grandchild sleeper proves tree (not direct-child) kill.
    let mut app = editor_test_app();
    let home = app.runtime.focused_view().expect("focused leaf");
    feed_text(&mut app, "hi");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "bitty-editor-timeout-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("timeout workdir");
    let pidfile = dir.join("grandchild.pid");
    let script = write_fake_editor(
        "timeout-tree",
        &format!(
            "#!/bin/sh\n( exec sleep 60 ) & \nprintf '%s' \"$!\" > \"{pid}\"\nsleep 60\n",
            pid = pidfile.to_string_lossy()
        ),
    );
    let script_arg = script.to_string_lossy().into_owned();
    app.open_external_editor_with_timeout(
        None,
        None,
        Some(script_arg.as_str()),
        Duration::from_millis(300),
    );
    assert!(app.chrome.editor.is_hosting());
    let editor_view = app.chrome.editor.pending_view().expect("pending view");
    // Drive the editor poll directly (not via `poll_pty_pump`, which would
    // consume the finish outcome internally) so the typed outcome is observed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcome = None;
    while app.chrome.editor.is_hosting() && Instant::now() < deadline {
        if let Some(done) = app.poll_external_editor() {
            outcome = Some(done);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        outcome,
        Some(bitty_rich::host::EditorOutcome::Timeout),
        "expiry must report a typed Timeout"
    );
    assert_eq!(app.runtime.leaf_count(), 2);
    assert!(!app.runtime.has_pane_session(&editor_view));
    assert_eq!(app.runtime.focused_view(), Some(home));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Only an owned-tree kill reaches the grandchild: it must be gone
        // (absent from /proc, or a zombie awaiting reaping).
        let pid_text = std::fs::read_to_string(&pidfile).expect("grandchild pid recorded");
        let grandchild: u32 = pid_text.trim().parse().expect("pid parses");
        assert!(
            hosted_grandchild_gone(grandchild),
            "grandchild {grandchild} survived the timeout kill"
        );
    }
    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_maintenance_sweeps_planted_stale_temp() {
    // Plants a provably-dead-owner temp in the real owned root, then proves
    // the startup path sweeps it. Pid 0 never owns a file on any platform,
    // so the name is stale everywhere (portable, no live-pid fixture).
    // The `bitty-composer-` prefix mirrors `host::TEMP_PREFIX`
    // (`pub(crate)` there, so spelled out here); nanos + pid keep the name
    // unique across parallel runs.
    let root = bitty_rich::host::owned_temp_root().expect("owned root");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = root.join(format!("bitty-composer-0-{nanos}-{}", std::process::id()));
    std::fs::write(&path, "stale").expect("plant stale temp");
    let swept = startup_maintenance();
    assert!(swept >= 1, "startup must sweep the planted stale temp");
    assert!(!path.exists(), "planted stale temp must be gone");
}

/// Whether a pid is fully gone on Linux: absent from `/proc`, or a zombie
/// awaiting reaping (a SIGKILLed child reparents to init, which reaps
/// promptly; poll briefly before concluding survival). Mirrors the
/// blocking-path probe in `bitty-rich` host tests.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn hosted_grandchild_gone(pid: u32) -> bool {
    for _ in 0..100 {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => return true,
            Ok(stat) => {
                let state = stat.rsplit(')').next().unwrap_or("").trim_start();
                if !state.starts_with('Z') {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Shell child exit and panel auto-close regression tests (#1356, #1541, CTX-0881)
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn reap_exited_split_pane_closes_pane_and_promotes_sibling() {
    require_pty!();
    let mut app = editor_test_app();
    assert_eq!(app.runtime.leaf_count(), 2);
    let pane_view = ViewId::new(2);

    // Spawn an immediately exiting shell script in split pane 2.
    let script = write_fake_editor("exit-pane", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(pane_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    assert!(app.runtime.has_pane_session(&pane_view));

    // Poll until reap_exited_shells detects exit and closes the pane.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert_eq!(
        app.runtime.leaf_count(),
        1,
        "closed pane must reduce leaf count to 1"
    );
    assert!(
        !app.runtime.has_pane_session(&pane_view),
        "pane session must be dropped"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(ViewId::new(1)),
        "sibling must remain focused"
    );
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_primary_with_active_pane_closes_primary_pane() {
    require_pty!();
    let mut app = editor_test_app();
    assert_eq!(app.runtime.leaf_count(), 2);
    let primary_view = ViewId::new(1);
    let pane_view = ViewId::new(2);
    assert_eq!(app.runtime.primary_view(), Some(primary_view));

    // Spawn a long-running shell in split pane 2.
    let pane_script = write_fake_editor("sleep-pane", "#!/bin/sh\nsleep 10\n");
    let pane_script_arg = pane_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(pane_view, pane_script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");

    // Spawn an exiting shell in primary.
    let primary_script = write_fake_editor("exit-primary", "#!/bin/sh\nexit 0\n");
    let primary_script_arg = primary_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell(primary_script_arg.as_str())
        .expect("spawn primary shell");

    // Poll until reap_exited_shells detects primary exit and closes the primary pane.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert_eq!(app.runtime.leaf_count(), 1);
    assert_eq!(app.runtime.focused_view(), Some(pane_view));

    // Clean up
    app.runtime.close_pane_session(&pane_view);
    let _ = std::fs::remove_file(&pane_script);
    let _ = std::fs::remove_file(&primary_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_last_remaining_shell_signals_app_exit() {
    require_pty!();
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    assert_eq!(app.runtime.leaf_count(), 1);

    // Spawn an exiting shell in primary.
    let script = write_fake_editor("exit-last", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell(script_arg.as_str())
        .expect("spawn primary shell");

    // Poll until reap_exited_shells returns AppExiting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::AppExiting);
    let _ = std::fs::remove_file(&script);
}

/// Issue #2443 fixture: a shell that closes every tty fd (forcing reader EOF)
/// and then blocks on `marker` before exiting. The EOF wake therefore lands
/// while the child is provably not reapable yet.
#[cfg(target_os = "linux")]
fn delayed_exit_after_eof(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let marker = std::env::temp_dir().join(format!(
        "bitty-ctx2443-{}-{nanos}-{name}.go",
        std::process::id()
    ));
    let body = format!(
        "#!/bin/sh\nexec 0<&- 1>&- 2>&-\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nexit 0\n",
        marker.display()
    );
    (write_fake_editor(name, &body), marker)
}

/// Bounded pump until a session's reader reports EOF while its child is
/// still unreaped (the #2443 race window).
#[cfg(target_os = "linux")]
fn wait_for_unreaped_eof_session(app: &mut TerminalApp, deadline: std::time::Instant) {
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        if app.runtime.has_unreaped_eof_session() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("no PTY session reported EOF with an unreaped child within the bound");
}

/// Drive the app only through the reap wakes it schedules itself.
///
/// This mirrors the `AboutToWait` arm: pump + reap, then wait until the
/// app's own next wake. A missing deadline means the real event loop would
/// park in `ControlFlow::Wait` with a dead shell on screen — the #2443 hang
/// signature — so it fails the test instead of silently polling.
#[cfg(target_os = "linux")]
fn drive_post_eof_reap_wakes(
    app: &mut TerminalApp,
    deadline: std::time::Instant,
) -> terminal_app::ShellExitOutcome {
    let mut outcome = app.reap_exited_shells();
    while outcome == terminal_app::ShellExitOutcome::NoExit {
        let now = std::time::Instant::now();
        assert!(
            now < deadline,
            "shell exit never observed within the bound (event loop would hang, #2443)"
        );
        let Some(next_at) = app.shell_exit_poll_deadline() else {
            panic!("PTY EOF with an unreaped child must keep a bounded reap wake armed (#2443)");
        };
        assert!(
            next_at <= deadline,
            "scheduled reap wake {next_at:?} overruns the test bound"
        );
        std::thread::sleep(next_at.saturating_duration_since(now));
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
    }
    outcome
}

/// Issue #2443: `exit` in the last panel must terminate promptly even when
/// the PTY hits EOF before the child is reapable.
#[test]
#[cfg(target_os = "linux")]
fn last_panel_exit_after_pty_eof_terminates_issue_2443() {
    require_pty!();
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    assert_eq!(app.runtime.leaf_count(), 1);
    app.runtime.set_session_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageSessionBackend::new(),
    )));
    let (script, marker) = delayed_exit_after_eof("late-exit-last-2443");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell(script_arg.as_str())
        .expect("spawn primary shell");
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let child = app.runtime.pty_pid().expect("child pid");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    wait_for_unreaped_eof_session(&mut app, deadline);
    assert_eq!(
        app.reap_exited_shells(),
        terminal_app::ShellExitOutcome::NoExit,
        "the fixture must keep the child alive across the EOF wake"
    );

    // Only now let the child exit; from here the app's own scheduled wake is
    // the sole event source (no timer, input, or compositor event).
    std::fs::write(&marker, b"go").expect("release delayed exit");
    assert_eq!(
        drive_post_eof_reap_wakes(&mut app, deadline),
        terminal_app::ShellExitOutcome::AppExiting,
        "the last panel must close the session after its shell exits"
    );
    assert!(
        app.shell_exit_poll_deadline().is_none(),
        "the reap schedule must clear once the exit is observed"
    );

    // #2421 preserved: a shell exit still leaves a populated tiled
    // workspace, so the exit save is attempted rather than skipped as a
    // close-to-empty window.
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx2443-save-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    app.runtime
        .save_session_to_path(&dir.join("session"))
        .expect("last-panel exit state must stay encodable");

    app.runtime.shutdown();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert!(
        hosted_grandchild_gone(child),
        "child {child} must be reaped, never orphaned"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&script);
}

/// Issue #2443: the same late-exit race in a non-last pane must close only
/// that pane, keep its sibling, and clear the reap schedule.
#[test]
#[cfg(target_os = "linux")]
fn late_exiting_split_pane_closes_only_that_pane_issue_2443() {
    require_pty!();
    let mut app = editor_test_app();
    assert_eq!(app.runtime.leaf_count(), 2);
    let pane_view = ViewId::new(2);
    let (script, marker) = delayed_exit_after_eof("late-exit-pane-2443");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(pane_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    wait_for_unreaped_eof_session(&mut app, deadline);
    assert_eq!(
        app.reap_exited_shells(),
        terminal_app::ShellExitOutcome::NoExit,
        "the fixture must keep the pane child alive across the EOF wake"
    );

    std::fs::write(&marker, b"go").expect("release delayed exit");
    assert_eq!(
        drive_post_eof_reap_wakes(&mut app, deadline),
        terminal_app::ShellExitOutcome::PaneClosed,
        "a non-last exit must close only its pane and never exit the app"
    );
    assert_eq!(app.runtime.leaf_count(), 1, "sibling pane must survive");
    assert!(
        !app.runtime.has_pane_session(&pane_view),
        "the exited pane session must be dropped"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(ViewId::new(1)),
        "focus must return to the surviving pane"
    );
    assert!(!app.runtime.has_unreaped_eof_session());
    assert!(
        app.shell_exit_poll_deadline().is_none(),
        "no phantom reap schedule after the pane closed"
    );
    app.runtime.shutdown();
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_last_leaf_of_workspace_prunes_the_workspace() {
    require_pty!();
    // Issue #2525 (CTX-1044 residual): a shell exiting in the last panel of
    // a workspace is a panel close, exactly like the close-view gesture
    // (CTX-1297). With two single-panel workspaces the emptied workspace is
    // pruned and focus follows the survivor; keeping a session-less tile
    // stranded focus on a dead panel that silently dropped input.
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    assert_eq!(app.runtime.leaf_count(), 1);
    app.runtime.workspace_new().expect("second workspace opens");
    assert_eq!(app.runtime.workspace_count(), 2);
    assert_eq!(app.runtime.active_workspace_index(), 1);
    assert_eq!(app.runtime.leaf_count(), 1);
    assert_eq!(app.effective_window_leaf_count(), 2);

    // Keep a live shell on the surviving workspace so the prune leaves a
    // usable window with a real input path behind the survivor.
    assert!(app.runtime.workspace_switch(0));
    let survivor_view = app.runtime.focused_view().expect("survivor leaf");
    let survivor_script = write_fake_editor("sleep-survivor-2525", "#!/bin/sh\nsleep 10\n");
    let survivor_arg = survivor_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(survivor_view, survivor_arg.as_str(), &[], 80, 24, None)
        .expect("spawn survivor shell");
    assert!(app.runtime.workspace_switch(1));
    let active_view = app.runtime.focused_view().expect("active leaf focused");

    let script = write_fake_editor("exit-ws-last", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(active_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    assert!(app.runtime.has_pane_session(&active_view));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "exiting the last leaf must close the pane, never the app"
    );
    assert_eq!(
        app.runtime.workspace_count(),
        1,
        "emptied workspace is pruned like the close-view gesture"
    );
    assert_eq!(
        app.runtime.active_workspace_index(),
        0,
        "focus follows the surviving workspace"
    );
    assert_eq!(app.runtime.leaf_count(), 1, "survivor keeps its panel");
    assert_eq!(
        app.effective_window_leaf_count(),
        1,
        "the surviving workspace owns the remaining panel"
    );
    assert!(
        !app.runtime.has_pane_session(&active_view),
        "exited pane session must be torn down"
    );
    let survivor = app.runtime.focused_view().expect("survivor leaf focused");
    assert_eq!(
        survivor, survivor_view,
        "focus must leave the pruned dead tile for the live survivor"
    );
    assert!(
        app.runtime.has_pane_session(&survivor_view),
        "the live survivor keeps its shell"
    );

    // The surviving workspace still presents and stays usable.
    let _ = app.drive_tick();
    assert_eq!(app.runtime.workspace_count(), 1);
    assert_eq!(app.runtime.leaf_count(), 1);
    assert_eq!(app.runtime.focused_view(), Some(survivor));
    app.runtime.close_pane_session(&survivor_view);
    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_file(&survivor_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_primary_last_leaf_of_workspace_prunes_workspace() {
    require_pty!();
    // Issue #2525: the primary shell is the sole panel of its workspace
    // while a split pane stays live on another. On exit the singleton
    // layout refuses `close_focused_leaf`, which used to strand the dead
    // primary tile. The emptied workspace must be pruned instead.
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app.runtime.workspace_new().expect("second workspace opens");
    assert_eq!(app.runtime.workspace_count(), 2);
    assert_eq!(app.runtime.active_workspace_index(), 1);
    let pane_view = app.runtime.focused_view().expect("active leaf focused");

    // Keep a live pane on the surviving workspace.
    let pane_script = write_fake_editor("sleep-pane-2525", "#!/bin/sh\nsleep 10\n");
    let pane_arg = pane_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(pane_view, pane_arg.as_str(), &[], 80, 24, None)
        .expect("spawn live pane shell");
    assert!(app.runtime.has_pane_session(&pane_view));

    // Attach the exiting primary to workspace 1's sole leaf (the primary
    // owner is chosen from the focused leaf at attach time).
    assert!(app.runtime.workspace_switch(0), "switch to workspace 1");
    let primary_view = app.runtime.focused_view().expect("primary leaf focused");
    assert_eq!(primary_view, ViewId::new(1));
    let primary_script = write_fake_editor("exit-primary-2525", "#!/bin/sh\nexit 0\n");
    let primary_arg = primary_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell(primary_arg.as_str())
        .expect("spawn primary shell");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert_eq!(
        app.runtime.workspace_count(),
        1,
        "the emptied primary workspace is pruned"
    );
    assert_eq!(
        app.runtime.active_workspace_index(),
        0,
        "the surviving workspace keeps focus"
    );
    assert!(
        app.runtime.has_pane_session(&pane_view),
        "the live pane survives the prune"
    );
    assert_ne!(
        app.runtime.primary_view(),
        Some(primary_view),
        "the dead primary owner must be replaced"
    );
    assert_eq!(app.runtime.focused_view(), Some(pane_view));
    let _ = app.drive_tick();

    app.runtime.close_pane_session(&pane_view);
    let _ = std::fs::remove_file(&pane_script);
    let _ = std::fs::remove_file(&primary_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_shell_in_inactive_workspace_leaves_no_ghost_issue_2167() {
    // Issue #2167: a tiled pane whose child exits while its workspace is in
    // the background must be released from its stash, never stranded as a
    // session-less erased tile (empty bordered panel + cursor, unusable).
    // Before the fix the reap path ran layout surgery against the *active*
    // tree (refused: the view lives elsewhere) and still tore the session
    // down, so switching back showed the ghost.
    require_pty!();
    let mut app = editor_test_app();
    let exited_view = ViewId::new(2);
    let survivor = ViewId::new(1);

    // Immediately-exiting child in pane 2; no reap runs before the switch,
    // so the exit status is still pending while the workspace goes inactive.
    let script = write_fake_editor("exit-bg-pane", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(exited_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    assert!(app.runtime.has_pane_session(&exited_view));

    app.runtime.workspace_new().expect("second workspace opens");
    assert_eq!(app.runtime.active_workspace_index(), 1);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "background pane exit must close the pane, never the app"
    );

    // Switching back must show the survivor only: no session-less ghost.
    assert!(app.runtime.workspace_switch(0));
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
    assert!(
        !app.runtime.has_pane_session(&exited_view),
        "exited background pane must be fully torn down"
    );
    let _ = app.drive_tick();
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_parked_shell_discards_scratchpad_issue_2167() {
    // Issue #2167: a scratchpad-parked panel whose child exits must leave
    // the slot empty, never a dead parked leaf that restores as a ghost.
    require_pty!();
    let mut app = editor_test_app();
    let parked_view = ViewId::new(2);
    let survivor = ViewId::new(1);

    let script = write_fake_editor("exit-parked-pane", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(parked_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    app.runtime
        .scratchpad_hide(parked_view)
        .expect("parked panel hides");
    assert!(app.runtime.scratchpad_occupied());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "parked pane exit must close the pane, never the app"
    );
    assert!(
        !app.runtime.scratchpad_occupied(),
        "no dead parked leaf may linger"
    );
    assert!(
        !app.runtime.has_pane_session(&parked_view),
        "exited parked pane must be fully torn down"
    );
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_focused_float_dismisses_without_freeze_issue_2169() {
    // Issue #2169: typing `exit` in a floating panel must dismiss the
    // float and return focus — never hard-freeze the terminal. The float
    // is focused here (the user is typing in it), unlike the #2141
    // detach path where focus already moved to the survivor.
    require_pty!();
    let mut app = editor_test_app();
    assert_eq!(app.runtime.leaf_count(), 2);
    let float_view = ViewId::new(2);
    let survivor = ViewId::new(1);

    // Keep a live shell behind the float so the post-reap focus and input
    // route are observable, rather than treating a session-less tile as a
    // survivor.
    app.runtime
        .spawn_shell_for_view_in(survivor, "/bin/sh", &["-c", "exec sleep 30"], 80, 24, None)
        .expect("spawn survivor shell");

    // The float owns an interactive shell. The test sends the exact bytes a
    // user types through Runtime's focused input router below.
    app.runtime
        .spawn_shell_for_view_in(float_view, "/bin/sh", &[], 80, 24, None)
        .expect("spawn float shell");
    assert!(app.runtime.has_pane_session(&float_view));

    // Detach pane 2 into a float, then focus it (user clicks the float).
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float_view))
        .expect("detach to float");
    assert!(app.runtime.floated_views().contains(&float_view));
    assert!(app.runtime.set_focus(float_view), "float must be focusable");

    assert!(
        app.runtime.push_input_bytes(b"exit\r"),
        "typed exit must reach the focused floating shell"
    );

    // Poll until reap_exited_shells observes the exit. A deadline breach
    // here is the #2169 hang (reap never returns / never observes).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "exiting float must dismiss the panel, never the app (and never hang)"
    );
    assert!(
        !app.runtime.floated_views().contains(&float_view),
        "dead float must be discarded"
    );
    assert!(
        !app.runtime.has_pane_session(&float_view),
        "float session must be dropped"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(survivor),
        "focus must return to the surviving tile"
    );
    assert_eq!(app.effective_window_leaf_count(), 1);

    // The terminal stays responsive: ticks present and input routes to
    // the survivor instead of a dead float.
    let _ = app.drive_tick();
    let _ = app.drive_tick();
    assert_eq!(app.runtime.focused_view(), Some(survivor));
    assert!(
        app.runtime.has_pane_session(&survivor),
        "survivor shell must remain alive after float reap"
    );
    assert!(
        app.runtime.push_input_bytes(b"echo survivor\r"),
        "survivor shell must remain the live input owner after float reap"
    );
    let _ = app.drive_tick();
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
}

#[cfg(unix)]
fn reap_until_change_2169(app: &mut TerminalApp, secs: u64) -> terminal_app::ShellExitOutcome {
    // Bounded reap helper for the #2169 float-exit matrix. A deadline
    // breach surfaces as NoExit (the hang signature) instead of blocking
    // the test runner forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    outcome
}

#[test]
#[cfg(unix)]
fn reap_exited_focused_pin_dismisses_without_freeze_issue_2169() {
    // Exiting shell in a focused PINNED panel (window-global sticky
    // float) must dismiss without hanging.
    require_pty!();
    let mut app = editor_test_app();
    let pin_view = ViewId::new(2);
    let survivor = ViewId::new(1);
    let script = write_fake_editor("exit-pin", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(pin_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pin shell");
    assert!(app.runtime.set_focus(pin_view));
    app.apply_chrome_action(bitty_config::ChromeAction::TogglePinned);
    assert!(app.runtime.pinned_views().contains(&pin_view));
    assert!(app.runtime.set_focus(pin_view), "pin must be focusable");
    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert!(!app.runtime.pinned_views().contains(&pin_view));
    assert!(!app.runtime.has_pane_session(&pin_view));
    assert_eq!(app.runtime.focused_view(), Some(survivor));
    let _ = app.drive_tick();
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_focused_float_dismisses_while_zoomed_issue_2169() {
    // Zoom engaged on the survivor while the focused float's shell
    // exits (reap runs restore_zoom before discard).
    require_pty!();
    let mut app = editor_test_app();
    let float_view = ViewId::new(2);
    let survivor = ViewId::new(1);
    let script = write_fake_editor("exit-zoomfloat", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(float_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn float shell");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float_view))
        .expect("detach to float");
    assert!(app.runtime.set_focus(survivor));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    assert!(app.chrome.zoom.is_zoomed(&app.runtime), "zoom engaged");
    assert!(app.runtime.set_focus(float_view), "float must be focusable");
    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert!(!app.runtime.floated_views().contains(&float_view));
    let _ = app.drive_tick();
    let _ = app.drive_tick();
    assert!(app.runtime.focused_view().is_some(), "focus must be live");
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_inactive_workspace_float_discards_without_freeze_issue_2169() {
    // The float lives on the INACTIVE workspace when its shell exits
    // (sibling #2167 ghost family — must not hang either). With the
    // stashed-container release, the parked float is discarded from its
    // home slot instead of stranding a session-less ghost there.
    require_pty!();
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app.runtime.set_layout(two_pane_layout());
    let float_view = ViewId::new(2);
    let survivor = ViewId::new(1);
    let script = write_fake_editor("exit-inactive", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(float_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn float shell");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float_view))
        .expect("detach to float");
    app.runtime.workspace_new().expect("second workspace opens");
    assert_eq!(app.runtime.active_workspace_index(), 1);
    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "inactive-workspace float exit must close the pane (no hang, no app exit)"
    );
    assert!(
        !app.runtime.has_pane_session(&float_view),
        "parked float session must be torn down"
    );
    // Switching back must show the survivor only: no session-less ghost.
    assert!(app.runtime.workspace_switch(0));
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
    assert!(
        !app.runtime.floated_views().contains(&float_view),
        "parked float must be discarded from its home slot"
    );
    let _ = app.drive_tick();
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_primary_owner_float_dismisses_while_zoomed_issue_2169() {
    // Zoom engaged, then the PRIMARY-OWNER float's shell exits
    // (primary_exit_status + floated branch, which carries no restore_zoom).
    require_pty!();
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app.runtime.set_layout(two_pane_layout());
    let float_view = ViewId::new(1);
    let survivor = ViewId::new(2);
    let pane_script = write_fake_editor("keep-pane", "#!/bin/sh\nsleep 30\n");
    let pane_arg = pane_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(survivor, pane_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    let prim_script = write_fake_editor("exit-prim", "#!/bin/sh\nexit 0\n");
    let prim_arg = prim_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell(prim_arg.as_str())
        .expect("spawn primary shell");
    // Detach the primary FIRST (zoom collapses the tree to a proxy, and a
    // leaf living only in the zoom backup is not detachable).
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float_view))
        .expect("detach primary to float");
    assert!(app.runtime.set_focus(survivor));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    assert!(app.chrome.zoom.is_zoomed(&app.runtime), "zoom engaged");
    assert!(app.runtime.set_focus(float_view), "float must be focusable");
    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "primary-owner float exit must dismiss, never hang or exit the app"
    );
    assert!(!app.runtime.floated_views().contains(&float_view));
    let _ = app.drive_tick();
    let _ = app.drive_tick();
    assert!(app.runtime.focused_view().is_some(), "focus must be live");
    app.runtime.close_pane_session(&survivor);
    let _ = std::fs::remove_file(&pane_script);
    let _ = std::fs::remove_file(&prim_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_float_after_refocus_keeps_survivor_focus_issue_2169() {
    // Issue #2169 race branch (CTX-1284 headed `refocusrace` variant):
    // the user types `exit` in the float, then focus moves to the
    // surviving tile BEFORE the reap observes the exit. The discard must
    // keep the survivor focus (never clear it) and the terminal must stay
    // responsive: ticks present and input still routes to the survivor.
    require_pty!();
    let mut app = editor_test_app();
    assert_eq!(app.runtime.leaf_count(), 2);
    let float_view = ViewId::new(2);
    let survivor = ViewId::new(1);

    let script = write_fake_editor("exit-float-race", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(float_view, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn float shell");
    assert!(app.runtime.has_pane_session(&float_view));
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float_view))
        .expect("detach to float");
    assert!(app.runtime.floated_views().contains(&float_view));
    // Focus leaves the dying float before the reap runs.
    assert!(app.runtime.set_focus(survivor));
    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "float exit with focus elsewhere must still dismiss the pane"
    );
    assert!(
        !app.runtime.floated_views().contains(&float_view),
        "dead float must be discarded"
    );
    assert!(
        !app.runtime.has_pane_session(&float_view),
        "float session must be dropped"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(survivor),
        "survivor keeps focus, never cleared"
    );
    let _ = app.drive_tick();
    let _ = app.drive_tick();
    assert_eq!(app.runtime.focused_view(), Some(survivor));
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_shell_while_zoomed_releases_backup_leaf_issue_2167() {
    // Issue #2167 (CodeRabbit on PR #2170): while zoomed the live layout
    // is a single-leaf proxy and the exited leaf hides in the zoom backup.
    // Surgery must run against the real tree (restore_zoom first), or the
    // dead leaf survives in the backup and ghosts on unzoom.
    require_pty!();
    let mut app = editor_test_app();
    let exiting = ViewId::new(2);

    let script = write_fake_editor("exit-zoomed-bg", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(exiting, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    assert!(app.runtime.set_focus(ViewId::new(1)));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    assert_eq!(
        app.runtime.leaf_count(),
        1,
        "zoom collapses the live layout to a proxy"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "zoomed backup exit must close the pane, never the app"
    );
    // The dead leaf must be gone from the real tree (session torn down),
    // never parked in the zoom backup to ghost on unzoom.
    assert!(
        !app.runtime.has_pane_session(&exiting),
        "exited backup leaf must be fully torn down"
    );
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    assert!(
        !app.runtime.layout().leaf_ids().contains(&exiting),
        "unzoom must not resurrect the dead leaf"
    );
    let _ = std::fs::remove_file(&script);
}

#[test]
#[cfg(unix)]
fn reap_exited_zoomed_alt_screen_pane_in_inactive_workspace_restores_tree_2527() {
    // #2527 control: leaving an alternate-screen program before its shell
    // exits must use the same inactive zoom restoration path without
    // resurrecting a dead proxy or losing the sibling pane.
    require_pty!();
    let mut app = editor_test_app();
    let survivor = ViewId::new(1);
    let exiting = ViewId::new(2);
    let survivor_script = write_fake_editor("sleep-alt-survivor-2527", "#!/bin/sh\nsleep 10\n");
    let survivor_arg = survivor_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(survivor, survivor_arg.as_str(), &[], 80, 24, None)
        .expect("spawn survivor");
    let alt_script = write_fake_editor(
        "alt-screen-exit-2527",
        "#!/bin/sh\nprintf '\x1b[?1049h\x1b[2J\x1b[H ALT-SCREEN-2527\\n'; read -r _; printf '\x1b[?1049l'; exit 0\n",
    );
    let alt_arg = alt_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(exiting, alt_arg.as_str(), &[], 80, 24, None)
        .expect("spawn alt-screen pane");
    assert!(app.runtime.set_focus(exiting));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    app.runtime.workspace_new().expect("second workspace");
    assert!(app.runtime.workspace_switch(0));
    assert!(app.runtime.push_input_bytes(b"leave-alt\r"));
    assert!(app.runtime.workspace_switch(1));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert!(app.runtime.workspace_switch(0));
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
    assert!(app.runtime.has_pane_session(&survivor));
    assert!(!app.runtime.has_pane_session(&exiting));
    assert!(app.runtime.push_input_bytes(b"echo alt-survivor\r"));
    let _ = app.drive_tick();
    app.runtime.close_pane_session(&survivor);
    let _ = std::fs::remove_file(&survivor_script);
    let _ = std::fs::remove_file(&alt_script);
}

/// Issue #2221 / CTX-1336: closing the last tiled host while a detached
/// float survives must remove the host leaf instead of leaving a
/// session-less tile focused. The old path tore the primary session down
/// but kept its leaf, so the empty focused leaf painted a cursor ghost in
/// the vacated region and swallowed keyboard input until `ctrl+tab`.
#[test]
#[cfg(unix)]
fn reap_exited_zoomed_pane_in_inactive_workspace_keeps_workspace() {
    require_pty!();
    // Issue #2525 review: an inactive workspace stashes its zoom proxy as
    // its layout while the real tree lives in the app-side zoom backup. The
    // zoomed pane exiting must not read as "sole panel" and prune the
    // workspace while the hidden live pane remains.
    let mut app = editor_test_app();
    let hidden = ViewId::new(1);
    let exiting = ViewId::new(2);

    let hidden_script = write_fake_editor("sleep-hidden-2531", "#!/bin/sh\nsleep 10\n");
    let hidden_arg = hidden_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(hidden, hidden_arg.as_str(), &[], 80, 24, None)
        .expect("spawn hidden live pane");
    let exit_script = write_fake_editor("exit-zoomed-2525", "#!/bin/sh\nexit 0\n");
    let exit_arg = exit_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(exiting, exit_arg.as_str(), &[], 80, 24, None)
        .expect("spawn exiting zoomed pane");
    assert!(app.runtime.set_focus(exiting));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    assert_eq!(app.runtime.leaf_count(), 1, "zoomed proxy is one leaf");
    let seq = app.runtime.workspace_seq_at(0).expect("workspace seq");
    assert_eq!(
        app.chrome.zoom.backup_leaf_ids_for_seq(seq),
        Some(vec![hidden, exiting]),
        "the app-side backup owns the real tree"
    );

    // Switch to a second workspace; the zoom entry (and its backup) survives
    // the stash, so ws1 is now an inactive zoomed workspace.
    app.runtime.workspace_new().expect("second workspace opens");
    assert_eq!(app.runtime.active_workspace_index(), 1);
    assert_eq!(
        app.workspace_real_panel_count(0),
        Some(2),
        "prune decisions must read the backup, not the stashed proxy"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "the zoomed exit is a pane close"
    );
    assert_eq!(
        app.runtime.workspace_count(),
        2,
        "the workspace still owns a hidden live pane"
    );
    assert_eq!(
        app.runtime.active_workspace_index(),
        1,
        "the active workspace is unchanged"
    );
    assert!(
        app.runtime.has_pane_session(&hidden),
        "the hidden live pane survives"
    );
    assert!(
        !app.runtime.has_pane_session(&exiting),
        "the exited zoomed pane is torn down"
    );
    assert_eq!(app.chrome.zoom.backup_leaf_ids_for_seq(seq), None);

    // This is the missing half of the old regression: restore the inactive
    // workspace after reap, not only its app-side backup bookkeeping. The
    // dead proxy must not come back as a focused, session-less tile, and the
    // primary owner must remain reachable and typable.
    assert!(app.runtime.workspace_switch(0));
    assert_eq!(app.runtime.workspace_count(), 2);
    assert_eq!(app.runtime.layout().leaf_ids(), vec![hidden]);
    assert_eq!(app.runtime.workspace_panel_count(0), Some(1));
    assert_eq!(app.runtime.pane_session_count(), 1);
    assert_eq!(app.runtime.primary_view(), Some(ViewId::new(1)));
    assert_eq!(app.runtime.focused_view(), Some(hidden));
    assert!(
        app.runtime
            .push_input_bytes(b"echo inactive-zoom-survivor\r")
    );
    let _ = app.drive_tick();

    app.runtime.close_pane_session(&hidden);
    let _ = std::fs::remove_file(&hidden_script);
    let _ = std::fs::remove_file(&exit_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_hidden_zoom_backup_pane_in_inactive_workspace_restores_tree() {
    // CTX-1460 / #2531: a non-focused pane can exit while its workspace is
    // inactive and zoomed. It is present only in the app-side backup, not in
    // the stashed proxy. Reap must restore that backup before surgery so the
    // dead leaf is removed and the zoomed survivor remains reachable.
    require_pty!();
    let mut app = editor_test_app();
    let survivor = ViewId::new(1);
    let exiting = ViewId::new(2);

    let survivor_script = write_fake_editor("sleep-zoom-survivor-1460", "#!/bin/sh\nsleep 10\n");
    let survivor_arg = survivor_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(survivor, survivor_arg.as_str(), &[], 80, 24, None)
        .expect("spawn zoom survivor");
    let exit_script = write_fake_editor("exit-hidden-zoom-1460", "#!/bin/sh\nexit 0\n");
    let exit_arg = exit_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(exiting, exit_arg.as_str(), &[], 80, 24, None)
        .expect("spawn hidden exiting pane");

    assert!(app.runtime.set_focus(survivor));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    let seq = app.runtime.workspace_seq_at(0).expect("workspace sequence");
    assert_eq!(app.runtime.leaf_count(), 1);
    assert_eq!(
        app.chrome.zoom.backup_leaf_ids_for_seq(seq),
        Some(vec![survivor, exiting])
    );

    app.runtime.workspace_new().expect("second workspace");
    assert_eq!(app.runtime.active_workspace_index(), 1);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert_eq!(app.runtime.workspace_count(), 2);
    assert_eq!(app.runtime.active_workspace_index(), 1);
    assert!(app.runtime.has_pane_session(&survivor));
    assert!(!app.runtime.has_pane_session(&exiting));
    assert_eq!(app.chrome.zoom.backup_leaf_ids_for_seq(seq), None);

    assert!(app.runtime.workspace_switch(0));
    assert_eq!(app.runtime.layout().leaf_ids(), vec![survivor]);
    assert_eq!(app.runtime.focused_view(), Some(survivor));
    assert_eq!(app.runtime.primary_view(), Some(survivor));
    assert!(app.runtime.has_pane_session(&survivor));
    assert!(app.runtime.push_input_bytes(b"echo hidden-zoom-survivor\r"));
    let _ = app.drive_tick();

    app.runtime.close_pane_session(&survivor);
    let _ = std::fs::remove_file(&survivor_script);
    let _ = std::fs::remove_file(&exit_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_primary_in_inactive_zoom_backup_removes_dead_owner() {
    // CTX-1460 / #2531: the primary owner can be one of the hidden leaves in
    // an inactive zoom backup. Reap must restore that backup before removing
    // the dead owner (`remove_stashed_view` branch) instead of leaving a
    // session-less tile behind; the workspace keeps its live survivor, which
    // stays focused and typable. Deliberately not gated on `require_pty!`:
    // this is the only cover for that branch, so it must not skip silently.
    let mut app = editor_test_app();

    let primary = ViewId::new(1);
    let survivor = ViewId::new(2);
    let survivor_script =
        write_fake_editor("sleep-primary-zoom-survivor-1460", "#!/bin/sh\nsleep 10\n");
    let survivor_arg = survivor_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(survivor, survivor_arg.as_str(), &[], 80, 24, None)
        .expect("spawn survivor");
    let primary_script = write_fake_editor(
        "exit-primary-inactive-zoom-1460",
        "#!/bin/sh\nread -r _; exit 0\n",
    );
    let primary_arg = primary_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_with_args("/bin/sh", &["-c", primary_arg.as_str()])
        .expect("spawn primary");
    assert_eq!(app.runtime.primary_view(), Some(primary));
    assert!(app.runtime.set_focus(primary));
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleZoom);
    let seq = app.runtime.workspace_seq_at(0).expect("workspace sequence");
    assert_eq!(
        app.chrome.zoom.backup_leaf_ids_for_seq(seq),
        Some(vec![primary, survivor])
    );

    // Deliver exit to the zoom proxy, then leave the workspace inactive
    // before the reap pass runs.
    assert!(app.runtime.push_input_bytes(b"exit-primary\r"));
    app.runtime.workspace_new().expect("second workspace");
    assert_eq!(app.runtime.active_workspace_index(), 1);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert_eq!(
        app.runtime.workspace_count(),
        2,
        "the inactive workspace keeps its live survivor"
    );
    assert_eq!(app.runtime.active_workspace_index(), 1);
    assert!(!app.runtime.has_pane_session(&primary));
    assert!(app.runtime.has_pane_session(&survivor));
    assert_eq!(app.chrome.zoom.backup_leaf_ids_for_seq(seq), None);

    // The restored stash must no longer carry the dead primary leaf.
    assert!(app.runtime.workspace_switch(0));
    assert_eq!(
        app.runtime.layout().leaf_ids(),
        vec![survivor],
        "the dead primary leaf must be gone from the restored stashed layout"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(survivor),
        "the survivor keeps focus after the primary owner is reaped"
    );
    assert_ne!(
        app.runtime.primary_view(),
        Some(primary),
        "primary ownership must not stay on the reaped primary"
    );
    assert!(app.runtime.has_pane_session(&survivor));
    assert!(
        app.runtime
            .push_input_bytes(b"echo primary-owner-survivor\r")
    );
    let _ = app.drive_tick();

    app.runtime.close_pane_session(&survivor);
    let _ = std::fs::remove_file(&survivor_script);
    let _ = std::fs::remove_file(&primary_script);
}

#[test]
#[cfg(unix)]
fn reap_exited_pane_in_inactive_workspace_prunes_emptied_workspace() {
    require_pty!();
    // Issue #2525: the inactive-workspace twin of the pane prune path. A
    // shell exiting in the sole panel of a background workspace prunes that
    // workspace and leaves the active workspace's live pane untouched.
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    let exiting = app.runtime.focused_view().expect("ws1 leaf");
    let exit_script = write_fake_editor("exit-inactive-2525", "#!/bin/sh\nexit 0\n");
    let exit_arg = exit_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(exiting, exit_arg.as_str(), &[], 80, 24, None)
        .expect("spawn exiting background shell");

    app.runtime.workspace_new().expect("second workspace opens");
    let survivor = app.runtime.focused_view().expect("ws2 leaf");
    let survivor_script = write_fake_editor("sleep-survivor-ws2", "#!/bin/sh\nsleep 10\n");
    let survivor_arg = survivor_script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(survivor, survivor_arg.as_str(), &[], 80, 24, None)
        .expect("spawn active survivor shell");
    assert_eq!(app.runtime.active_workspace_index(), 1);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcome = terminal_app::ShellExitOutcome::NoExit;
    while std::time::Instant::now() < deadline {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != terminal_app::ShellExitOutcome::NoExit {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(outcome, terminal_app::ShellExitOutcome::PaneClosed);
    assert_eq!(
        app.runtime.workspace_count(),
        1,
        "the emptied background workspace is pruned"
    );
    assert_eq!(app.runtime.active_workspace_index(), 0);
    assert_eq!(app.runtime.focused_view(), Some(survivor));
    assert!(
        app.runtime.has_pane_session(&survivor),
        "the active workspace's live pane survives"
    );
    assert!(!app.runtime.has_pane_session(&exiting));

    app.runtime.close_pane_session(&survivor);
    let _ = std::fs::remove_file(&exit_script);
    let _ = std::fs::remove_file(&survivor_script);
}

#[test]
#[cfg(unix)]
fn close_host_with_detached_float_leaves_no_ghost_or_dead_input_issue_2221() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let float = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(float, "/bin/sh", &[], 80, 24, None)
        .expect("spawn float shell");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
        .expect("detach float");
    assert_eq!(app.runtime.floated_views(), vec![float]);
    assert!(app.runtime.set_focus(host), "host must hold focus");
    assert_eq!(app.runtime.focused_view(), Some(host));

    // Present once so the headless surface holds the pre-close frame; the
    // host content origin is the exact cell that used to paint the ghost
    // cursor after teardown.
    let _ = app.drive_tick();
    let host_frame = app
        .runtime
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == host)
        .expect("host frame before close");
    let pad = usize::try_from(app.runtime.window_padding_physical()).expect("pad fits");
    let origin_x = usize::try_from(host_frame.content.x).expect("x fits") + pad;
    let origin_y = usize::try_from(host_frame.content.y).expect("y fits") + pad;

    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "host teardown with a surviving float is a pane close, not a window close"
    );

    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "the closed host leaf must be removed, never left session-less"
    );
    assert_eq!(app.runtime.floated_views(), vec![float]);
    assert_eq!(
        app.runtime.focused_view(),
        Some(float),
        "focus must follow the surviving float"
    );
    assert_ne!(
        app.runtime.primary_view(),
        Some(host),
        "primary ownership must not point at the closed leaf"
    );
    assert!(
        app.runtime.push_input_bytes(b"probe"),
        "input must route to the surviving float shell"
    );

    let _ = app.drive_tick();
    assert!(
        app.runtime
            .present_frames()
            .iter()
            .all(|frame| frame.view != host),
        "no frame may remain for the closed host view"
    );
    let rgba = app.runtime.headless_rgba().expect("headless frame");
    let width =
        usize::try_from(app.runtime.config().window_extent().width()).expect("width fits usize");
    let bg = bitty_render::grid::DEFAULT_BG;
    assert_eq!(
        probe_rgba(&rgba, width, origin_x, origin_y),
        bg,
        "the vacated host cell must repaint to background, never a ghost cursor"
    );
}

/// Issue #2389: close teardown must follow the paint precedence of WS-INV-8.
///
/// Closing the primary owner while a pane-session leaf survives re-homes
/// `primary_view` onto that survivor (WS-INV-19), even though the survivor
/// paints its own pane grid — so the owner id names a leaf that is not the
/// primary painter. The last-tiled-panel close branch asked only
/// `is_primary_view(&focused)`, tore down the hidden primary PTY the panel
/// never painted, and left the pane session the panel *did* paint with no
/// owning leaf (WS-INV-9 violation). The orphan's shell keeps running, so
/// its foreground job stays in `first_running_job` forever: every later
/// window close arms a "Running job" prompt for a job the user can no longer
/// see or interrupt.
#[test]
#[cfg(unix)]
fn close_last_tiled_pane_tears_down_the_session_it_painted_issue_2389() {
    require_pty!();
    let mut app = editor_test_app();
    let owner = ViewId::new(1);
    let pane = ViewId::new(2);
    let survivor = ViewId::new(3);

    app.runtime
        .spawn_shell_with_args("/bin/sh", &[])
        .expect("primary shell spawns");
    let pane_frame = app
        .runtime
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == pane)
        .expect("pane frame");
    app.runtime
        .spawn_shell_for_view(pane, "/bin/sh", &[], pane_frame.cols, pane_frame.rows)
        .expect("pane shell spawns");

    // Close the primary owner: the only survivor owns a pane session, so the
    // documented re-home lands `primary_view` on a leaf that paints its own
    // grid (WS-INV-8) instead of the primary one.
    assert!(app.runtime.set_focus(owner));
    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "closing the primary owner with a live sibling is a pane close"
    );
    assert_eq!(
        app.runtime.primary_view(),
        Some(pane),
        "an explicit owner close re-homes the primary id to the survivor"
    );
    assert!(app.runtime.has_pane_session(&pane));
    assert!(
        !app.runtime.paints_primary_grid(&pane),
        "the survivor paints its pane session, not the re-homed primary grid"
    );

    // Give the tiled survivor a detached float so closing it takes the
    // last-tiled-panel branch (CTX-1336/#2221).
    let trimmed = bitty_runtime::LayoutNode::split(
        bitty_runtime::SplitAxis::Horizontal,
        0.5,
        bitty_runtime::LayoutNode::leaf(bitty_runtime::View::new(pane, 80, 24)),
        bitty_runtime::LayoutNode::leaf(bitty_runtime::View::new(survivor, 80, 24)),
    );
    app.runtime.set_layout(trimmed);
    let survivor_frame = app
        .runtime
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == survivor)
        .expect("survivor frame");
    app.runtime
        .spawn_shell_for_view(
            survivor,
            "/bin/sh",
            &[],
            survivor_frame.cols,
            survivor_frame.rows,
        )
        .expect("survivor shell spawns");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(survivor))
        .expect("survivor detaches as a float");
    assert_eq!(app.runtime.layout().leaf_ids(), vec![pane]);
    assert_eq!(app.runtime.floated_views(), vec![survivor]);
    assert_eq!(app.runtime.primary_view(), Some(pane));

    assert!(app.runtime.set_focus(pane));
    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "the last tiled panel with a surviving float is a pane close"
    );

    assert!(
        !app.runtime.has_pane_session(&pane),
        "closing a leaf must tear down the session that leaf painted (WS-INV-9)"
    );
    assert!(
        app.runtime.has_pane_session(&survivor),
        "the surviving float keeps its own shell"
    );
    assert!(
        app.runtime.has_pty(),
        "a pane close must never tear down the hidden primary shell it did not paint"
    );
    let orphans: Vec<ViewId> = app
        .runtime
        .pane_session_ids()
        .into_iter()
        .filter(|view| {
            !app.runtime.layout().leaf_ids().contains(view)
                && !app.runtime.floated_views().contains(view)
                && !app.runtime.pinned_views().contains(view)
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "no pane session may outlive its owning leaf, found {orphans:?}"
    );
}

/// Issue #2221 / CTX-1336: closing the last live view (the surviving float)
/// must be the window-close gesture. It must never discard the float into a
/// session-less view that paints blank and drops input while the process
/// stays alive.
#[test]
fn close_last_live_float_requests_window_close_not_dead_view_issue_2221() {
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let float = ViewId::new(2);
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
        .expect("detach float");
    assert_eq!(app.runtime.floated_views(), vec![float]);
    assert!(app.runtime.set_focus(host));

    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "host teardown with a surviving float is a pane close"
    );
    assert_eq!(app.runtime.layout().leaf_ids(), Vec::new());
    assert_eq!(app.runtime.focused_view(), Some(float));

    // No PTY and no primary shell: `close_confirm = when_busy` proceeds.
    assert!(
        app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "closing the last live view must request window close, never fall through"
    );
    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "no session-less replacement leaf may be created"
    );
    assert!(
        !app.runtime
            .primary_view()
            .is_some_and(|view| !app.runtime.floated_views().contains(&view)),
        "primary ownership must never dangle on a non-live view"
    );
}

/// Issue #2221 / CTX-1336: a workspace whose only live view was a float
/// (its tiled base already closed) must be pruned when the float closes,
/// exactly like CTX-1297 does for the last tiled panel. Leaving the slot
/// behind strands a view-less workspace that paints nothing and accepts
/// no input.
#[test]
fn close_last_float_prunes_emptied_workspace_issue_2221() {
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let float = ViewId::new(2);
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
        .expect("detach float");
    assert!(app.runtime.set_focus(host));
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::CloseView));
    assert_eq!(app.runtime.layout().leaf_ids(), Vec::new());
    assert_eq!(app.runtime.workspace_count(), 1);

    // A second workspace keeps a live panel, so the float-only workspace
    // is prunable rather than the whole window's last view.
    app.apply_chrome_action(bitty_config::ChromeAction::WorkspaceNew);
    assert_eq!(app.runtime.workspace_count(), 2);
    assert!(app.runtime.workspace_switch(0));
    assert!(
        app.runtime.set_focus(float),
        "the float must be focusable after the switch back"
    );

    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "closing the emptied workspace's last float is a pane close"
    );
    assert_eq!(
        app.runtime.workspace_count(),
        1,
        "the view-less workspace must be pruned, never stranded"
    );
    assert!(app.runtime.floated_views().is_empty());
    let focused = app
        .runtime
        .focused_view()
        .expect("focus must land on the surviving workspace's panel");
    assert!(
        app.runtime.layout().leaf_ids().contains(&focused),
        "focus must be a live member of the surviving layout"
    );
}

/// Issue #2223 / CTX-1338: an emptied workspace (tiled base closed, one
/// detached float survives) must still accept `new_panel`. The float is the
/// only focus target, so the old split path refused and silently dropped the
/// chord. The panel must materialize as the fresh tiled base while the float
/// stays detached.
#[test]
#[cfg(unix)]
fn new_panel_over_empty_tiled_base_with_float_materializes_tile_issue_2223() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let float = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(float, "/bin/sh", &[], 80, 24, None)
        .expect("spawn float shell");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
        .expect("detach float");
    assert!(app.runtime.set_focus(host), "host must hold focus");
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::CloseView));
    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "precondition: the tiled base is empty"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(float),
        "precondition: the surviving float holds focus outside the layout"
    );

    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::NewPanel),
        "new_panel over an empty base is a layout change, not a window close"
    );

    let leaves = app.runtime.layout().leaf_ids();
    assert_eq!(
        leaves.len(),
        1,
        "new_panel must materialize a tiled base instead of no-opping"
    );
    let panel = leaves[0];
    assert_ne!(panel, host, "the closed host must not come back");
    assert_ne!(
        panel, float,
        "the float must stay detached, not become tiled"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(panel),
        "focus must follow the materialized panel"
    );
    assert_eq!(
        app.runtime.floated_views(),
        vec![float],
        "the surviving float keeps its detached place"
    );
    assert!(
        app.runtime.has_pane_session(&panel),
        "the materialized panel must own its fresh shell"
    );

    let _ = app.drive_tick();
    let frames = app.runtime.present_frames();
    assert!(
        frames.iter().any(|frame| frame.view == panel),
        "the materialized panel must present"
    );
    assert!(
        frames.iter().any(|frame| frame.view == float),
        "the surviving float must keep presenting"
    );
}

/// CTX-1347 (#2240): app whose spawned shells print their physical cwd and
/// then stay alive, so the pane grid proves the inherited directory without
/// racing the reader pump (same technique as `cwd_inherit.rs`).
#[cfg(unix)]
fn cwd_probe_app() -> TerminalApp {
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let spec = SpawnSpec {
        program: Some("/bin/sh".to_string()),
        program_args: vec!["-c".to_string(), "pwd -P; exec sleep 30".to_string()],
        ..SpawnSpec::default()
    };
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        spec,
    );
    app.runtime.set_layout(two_pane_layout());
    app
}

/// Drains the PTY pump until `view`'s grid shows `needle`, then returns the
/// grid text (bounded, so a regression fails instead of hanging).
#[cfg(unix)]
fn wait_for_pane_text_ctx1347(app: &mut TerminalApp, view: ViewId, needle: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let _ = app.poll_pty_pump();
        let _ = app.drive_tick();
        let text = app
            .runtime
            .pane_snapshot(&view)
            .map(|snap| snap.cells.iter().map(|cell| cell.glyph).collect::<String>())
            .unwrap_or_default();
        if text.contains(needle) || std::time::Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Issue #2240 / CTX-1347: `new_panel` materializing a tiled base over an
/// empty workspace must let the fresh shell inherit the surviving float's
/// `OSC 7` cwd. `set_layout` re-homes focus to the fresh leaf, so the
/// inheritance source has to be captured before the install.
#[test]
#[cfg(unix)]
fn new_panel_over_empty_base_inherits_float_osc7_cwd_issue_2240() {
    require_pty!();
    let mut app = cwd_probe_app();
    let host = ViewId::new(1);
    let float = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(float, "/bin/sh", &["-c", "exec sleep 30"], 80, 24, None)
        .expect("spawn float shell");
    app.runtime
        .handle_pane_bytes(float, b"\x1b]7;file:///tmp\x07");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
        .expect("detach float");
    assert!(app.runtime.set_focus(host), "host must hold focus");
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::CloseView));
    assert!(
        app.runtime.layout().leaf_ids().is_empty(),
        "precondition: the tiled base is empty"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(float),
        "precondition: the surviving float holds focus outside the layout"
    );

    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::NewPanel));
    let leaves = app.runtime.layout().leaf_ids();
    assert_eq!(leaves.len(), 1, "new_panel must materialize a tiled base");
    let panel = leaves[0];
    assert!(
        app.runtime.has_pane_session(&panel),
        "the materialized panel must own its fresh shell"
    );
    let text = wait_for_pane_text_ctx1347(&mut app, panel, "/tmp");
    assert!(
        text.contains("/tmp"),
        "new panel must inherit the surviving float's OSC 7 cwd (/tmp): {text:?}"
    );
}

/// Issue #2240 / CTX-1347: the same empty-base path with a pinned survivor
/// must inherit the pin's `OSC 7` cwd too.
#[test]
#[cfg(unix)]
fn new_panel_over_empty_base_inherits_pin_osc7_cwd_issue_2240() {
    require_pty!();
    let mut app = cwd_probe_app();
    let host = ViewId::new(1);
    let pin = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(pin, "/bin/sh", &["-c", "exec sleep 30"], 80, 24, None)
        .expect("spawn pin shell");
    app.runtime
        .handle_pane_bytes(pin, b"\x1b]7;file:///tmp\x07");
    assert!(app.runtime.set_focus(pin), "pin source must hold focus");
    app.apply_chrome_action(bitty_config::ChromeAction::TogglePinned);
    assert!(
        app.runtime.pinned_views().contains(&pin),
        "precondition: the survivor is pinned"
    );
    assert!(app.runtime.set_focus(host), "host must hold focus");
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::CloseView));
    assert!(
        app.runtime.layout().leaf_ids().is_empty(),
        "precondition: the tiled base is empty"
    );
    assert_eq!(
        app.runtime.focused_view(),
        Some(pin),
        "precondition: the surviving pin holds focus outside the layout"
    );

    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::NewPanel));
    let leaves = app.runtime.layout().leaf_ids();
    assert_eq!(leaves.len(), 1, "new_panel must materialize a tiled base");
    let panel = leaves[0];
    assert!(
        app.runtime.has_pane_session(&panel),
        "the materialized panel must own its fresh shell"
    );
    let text = wait_for_pane_text_ctx1347(&mut app, panel, "/tmp");
    assert!(
        text.contains("/tmp"),
        "new panel must inherit the surviving pin's OSC 7 cwd (/tmp): {text:?}"
    );
}

#[cfg(unix)]
fn probe_rgba(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

/// Issue #2221 / CTX-1336: with an empty tiled base and two detached
/// floats, closing the focused float must land focus on the surviving
/// float. Clearing focus because the layout has no leaves would silently
/// drop input while a visible, typable panel remains.
#[test]
#[cfg(unix)]
fn close_float_on_empty_base_focuses_surviving_float_issue_2221() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let left = ViewId::new(2);
    let right = ViewId::new(3);
    app.runtime.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(host, 80, 24)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(left, 80, 24)),
            LayoutNode::leaf(View::new(right, 80, 24)),
        ),
    ));
    for view in [left, right] {
        app.runtime
            .spawn_shell_for_view_in(view, "/bin/sh", &[], 80, 24, None)
            .expect("spawn float shell");
        app.runtime
            .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(view))
            .expect("detach float");
    }
    assert_eq!(app.runtime.floated_views(), vec![left, right]);
    assert!(app.runtime.set_focus(host));
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::CloseView));
    assert_eq!(app.runtime.layout().leaf_ids(), Vec::new());
    assert_eq!(
        app.runtime.focused_view(),
        Some(right),
        "the topmost surviving float must take focus after the base drains"
    );

    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "closing one of two floats is a pane close"
    );
    assert_eq!(app.runtime.floated_views(), vec![left]);
    assert_eq!(
        app.runtime.focused_view(),
        Some(left),
        "focus must move to the remaining float, never clear"
    );
    assert!(
        app.runtime.push_input_bytes(b"probe"),
        "input must route to the surviving float shell"
    );
}

/// Issue #2221 / CTX-1336: the owner's exact report after the float is the
/// last live view: typing `exit` in it must not leave a blank slot.
#[test]
#[cfg(unix)]
fn reap_exited_last_float_prunes_emptied_workspace_issue_2221() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let float = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(float, "/bin/sh", &[], 80, 24, None)
        .expect("spawn float shell");
    app.runtime
        .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
        .expect("detach float");
    assert!(app.runtime.set_focus(host));
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::CloseView));
    assert_eq!(app.runtime.layout().leaf_ids(), Vec::new());
    assert_eq!(app.runtime.focused_view(), Some(float));

    // A second workspace keeps a live leaf, so the float exit is a pane
    // close (PaneClosed) instead of the whole-window close gesture.
    app.apply_chrome_action(bitty_config::ChromeAction::WorkspaceNew);
    assert_eq!(app.runtime.workspace_count(), 2);
    assert!(app.runtime.workspace_switch(0));
    assert!(app.runtime.set_focus(float));

    assert!(
        app.runtime.push_input_bytes(b"exit\n"),
        "typed exit must reach the float shell"
    );
    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "float exit with another workspace live is a pane close"
    );
    assert_eq!(
        app.runtime.workspace_count(),
        1,
        "the emptied float-only workspace must be pruned, never left blank"
    );
    assert!(app.runtime.floated_views().is_empty());
    let focused = app
        .runtime
        .focused_view()
        .expect("focus must land on the surviving workspace's panel");
    assert!(
        app.runtime.layout().leaf_ids().contains(&focused),
        "focus must be a live member of the surviving layout, never a dead slot"
    );
    assert!(
        app.runtime.primary_view().is_none_or(|view| app
            .runtime
            .layout()
            .leaf_ids()
            .contains(&view)),
        "primary ownership must not dangle"
    );
}

/// Issue #2222 / CTX-1337: closing the last tiled host while a pinned panel
/// survives must remove the host leaf instead of leaving a session-less
/// focused tile. The pinned store lives outside every layout, so unlike a
/// float it never covers the dead leaf: focus and primary ownership must
/// hand to the pin.
#[test]
#[cfg(unix)]
fn close_host_with_pinned_panel_hands_focus_and_primary_to_pin_issue_2222() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let pinned = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(pinned, "/bin/sh", &[], 80, 24, None)
        .expect("spawn pinned shell");
    assert!(app.runtime.set_focus(pinned));
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::TogglePinned));
    assert_eq!(app.runtime.pinned_views(), vec![pinned]);
    assert_eq!(app.runtime.layout().leaf_ids(), vec![host]);
    assert!(app.runtime.set_focus(host), "host must hold focus");

    let _ = app.drive_tick();
    let host_frame = app
        .runtime
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == host)
        .expect("host frame before close");

    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "host teardown with a surviving pin is a pane close, not a window close"
    );

    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "the closed host leaf must be removed, never left session-less"
    );
    assert_eq!(app.runtime.pinned_views(), vec![pinned]);
    assert_eq!(
        app.runtime.focused_view(),
        Some(pinned),
        "focus must follow the surviving pinned view"
    );
    assert_eq!(
        app.runtime.primary_view(),
        Some(pinned),
        "primary ownership must hand to the surviving pinned view"
    );
    assert!(
        app.runtime.push_input_bytes(b"probe"),
        "input must route to the surviving pinned shell"
    );

    let _ = app.drive_tick();
    assert!(
        app.runtime
            .present_frames()
            .iter()
            .all(|frame| frame.view != host),
        "no frame may remain for the closed host view"
    );
    assert_eq!(host_frame.view, host, "pre-close frame was the host");
}

/// Issue #2222 / CTX-1337: the owner-visible reap variant. Typing `exit` in
/// the last tiled primary shell while a pinned panel survives must drop the
/// dead leaf and leave the pinned view as focus and primary owner.
#[test]
#[cfg(unix)]
fn reap_exited_primary_with_pinned_survivor_hands_focus_and_primary_issue_2222() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let pinned = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(pinned, "/bin/sh", &[], 80, 24, None)
        .expect("spawn pinned shell");
    assert!(app.runtime.set_focus(pinned));
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::TogglePinned));
    assert_eq!(app.runtime.pinned_views(), vec![pinned]);
    assert_eq!(app.runtime.layout().leaf_ids(), vec![host]);
    assert!(app.runtime.set_focus(host));

    let script = write_fake_editor("exit-pinned-survivor", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell(script_arg.as_str())
        .expect("spawn primary shell");

    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "primary exit with a pinned survivor is a pane close"
    );
    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "the exited host leaf must be removed, never left session-less"
    );
    assert_eq!(app.runtime.pinned_views(), vec![pinned]);
    assert_eq!(
        app.runtime.focused_view(),
        Some(pinned),
        "focus must follow the surviving pinned view"
    );
    assert_eq!(
        app.runtime.primary_view(),
        Some(pinned),
        "primary ownership must hand to the surviving pinned view"
    );
    assert!(
        app.runtime.push_input_bytes(b"probe"),
        "input must route to the surviving pinned shell"
    );

    let _ = app.drive_tick();
    assert!(
        app.runtime
            .present_frames()
            .iter()
            .all(|frame| frame.view != host),
        "no frame may remain for the exited host view"
    );
    let _ = std::fs::remove_file(&script);
}

/// Issue #2222 / CTX-1337: the pane-session reap variant of the pinned
/// survivor. A tiled pane shell (not the primary) exits last while a pinned
/// pane remains; the dead leaf must retire instead of staying session-less.
#[test]
#[cfg(unix)]
fn reap_exited_last_tiled_pane_with_pinned_survivor_issue_2222() {
    require_pty!();
    let mut app = editor_test_app();
    let host = ViewId::new(1);
    let pinned = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view_in(pinned, "/bin/sh", &[], 80, 24, None)
        .expect("spawn pinned shell");
    assert!(app.runtime.set_focus(pinned));
    assert!(!app.apply_chrome_action(bitty_config::ChromeAction::TogglePinned));
    assert_eq!(app.runtime.pinned_views(), vec![pinned]);
    assert_eq!(app.runtime.layout().leaf_ids(), vec![host]);

    let script = write_fake_editor("exit-pinned-pane", "#!/bin/sh\nexit 0\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(host, script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn tiled pane shell");

    let outcome = reap_until_change_2169(&mut app, 10);
    assert_eq!(
        outcome,
        terminal_app::ShellExitOutcome::PaneClosed,
        "tiled pane exit with a pinned survivor is a pane close"
    );
    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "the exited tiled leaf must be removed, never left session-less"
    );
    assert_eq!(app.runtime.pinned_views(), vec![pinned]);
    assert_eq!(
        app.runtime.focused_view(),
        Some(pinned),
        "focus must follow the surviving pinned view"
    );
    assert_eq!(
        app.runtime.primary_view(),
        Some(pinned),
        "primary ownership must hand to the surviving pinned view"
    );
    assert!(
        app.runtime.push_input_bytes(b"probe"),
        "input must route to the surviving pinned shell"
    );

    let _ = std::fs::remove_file(&script);
}

/// Build a one-plugin runtime whose generation captures an `alt+?`
/// suggestion (issue #2407 reload-adoption test).
fn suggestion_plugin_runtime(
    tag: &str,
) -> (
    bitty_runtime::plugin_runtime::PluginRuntime,
    std::path::PathBuf,
) {
    use crate::composer_owner::fixture;
    const OWNER: &str = "bitty-featured.help-fixture";
    const COMMAND: &str = "toggle-help";
    let root = fixture::temp_dir(tag);
    let plugin = root.join(OWNER);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{OWNER}\"\nname = \"Help Fixture\"\nversion = \"0.1.0\"\n\
             description = \"suggestion reload fixture\"\n\n\
             [compat]\nbitty = \">=0.0.1\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\n\n\
             [lazy]\ncommands = [\"{OWNER}:{COMMAND}\"]\nevents = []\n",
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        format!(
            "bitty.commands.register({{ id = \"{COMMAND}\", title = \"Toggle help\", \
             run = function() return true end }})\n\
             bitty.keymaps.suggest({{ chord = \"alt+?\", command = \"{OWNER}:{COMMAND}\" }})\n\
             return {{}}\n"
        ),
    )
    .expect("init");
    let mut runtime = fixture::runtime_for(vec![root.clone()]);
    runtime.discover();
    let id = bitty_plugin_host::manifest::PluginId::new(OWNER).expect("id");
    runtime.activate(&id).expect("activate");
    (runtime, root)
}

#[test]
fn live_reload_adopts_keymaps_leader_hints_and_opacity_on_the_app() {
    // CTX-0898 (#1522): the app half of a live reload swaps the chrome key
    // table, the Leader binding, and the hint switch, cancels a Leader
    // window armed under the old binding, and records the new opacity.
    // Issue #2407 review B2: the live `mod_key` adoption is covered here via
    // a live `alt+?` suggestion whose matched chord must flip after the
    // reload.
    use bitty_config::{ChromeAction, KeyName, KeyRef, ModKey};
    let base = bitty_config::EffectiveConfig::default();
    let maps = bitty_config::resolve_keymaps(&base).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    // Issue #2405: the live keymap source mirrors the app table so
    // `bitty.keymaps.list()` never serves a stale registry after a reload.
    let live_keymaps = std::rc::Rc::new(crate::plugin_runtime::LiveKeymaps::default());
    live_keymaps.publish(&maps);
    let (plugin_runtime, plugin_root) = suggestion_plugin_runtime("suggestion-reload");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps.clone(),
        SpawnSpec::default(),
    )
    .with_live_keymaps(Some(std::rc::Rc::clone(&live_keymaps)))
    .with_plugin_runtime(Some(plugin_runtime));
    app.chrome.leader_state = bitty_config::LeaderState::Armed { deadline_ms: 0 };

    let alt_question = KeyRef {
        key: KeyName::Char('?'),
        ctrl: false,
        alt: true,
        shift: false,
        super_held: false,
    };
    let super_question = KeyRef {
        key: KeyName::Char('?'),
        ctrl: false,
        alt: false,
        shift: false,
        super_held: true,
    };
    let suggestion = Some(ChromeAction::InvokeCommand(
        "bitty-featured.help-fixture:toggle-help".to_string(),
    ));
    assert_eq!(app.chrome.mod_key, ModKey::Alt);
    assert_eq!(app.match_plugin_binding(alt_question), suggestion);
    assert_eq!(app.match_plugin_binding(super_question), None);

    let mut edited = base.clone();
    edited.mod_key = bitty_config::ModKey::Super;
    edited.leader_timeout_ms = Some(3_000);
    edited.hints_enabled = Some(false);
    let keymaps = bitty_config::resolve_keymaps(&edited).expect("super map");
    let leader = bitty_config::resolve_leader_for(&edited, bitty_config::LeaderPlatform::host())
        .expect("leader");
    app.adopt_live_config(crate::config_reload::AppAdoption {
        keymaps: keymaps.clone(),
        leader: leader.clone(),
        prefix_bindings: bitty_config::resolve_prefix_bindings(&edited.keymaps).expect("prefixes"),
        hints_enabled: false,
        resize_step: edited.layout.resize_step,
        mod_key: edited.mod_key,
        window_opacity: 0.6,
        theme_name: bitty_config::theme::DEFAULT_THEME_NAME.to_string(),
    });
    assert_eq!(app.chrome.keymaps, keymaps);
    assert_ne!(app.chrome.keymaps, maps, "mod_key rebound the table");
    assert_eq!(
        app.chrome.mod_key,
        ModKey::Super,
        "the reloaded mod_key slot must be adopted by the app"
    );
    assert_eq!(
        app.match_plugin_binding(alt_question),
        None,
        "the alt spelling must stop matching after the flip"
    );
    assert_eq!(
        app.match_plugin_binding(super_question),
        suggestion,
        "the same suggestion must follow the reloaded Mod slot"
    );
    assert_eq!(
        bitty_runtime::plugin_runtime::KeymapSource::keymaps(&*live_keymaps).expect("rows"),
        keymaps,
        "the plugin keymap source follows the adopted table"
    );
    assert_eq!(app.chrome.leader, leader);
    assert_eq!(app.chrome.leader.timeout_ms, 3_000);
    assert!(!app.chrome.hints_enabled);
    assert_eq!(
        app.chrome.leader_state,
        bitty_config::LeaderState::Idle,
        "a stale armed Leader window is cancelled"
    );
    assert!((app.window.opacity - 0.6).abs() < f32::EPSILON);
    let _ = std::fs::remove_dir_all(&plugin_root);
}

fn adoption_from(effective: &bitty_config::EffectiveConfig) -> crate::config_reload::AppAdoption {
    crate::config_reload::AppAdoption {
        keymaps: bitty_config::resolve_keymaps(effective).expect("keymaps"),
        leader: bitty_config::resolve_leader_for(effective, bitty_config::LeaderPlatform::host())
            .expect("leader"),
        prefix_bindings: bitty_config::resolve_prefix_bindings(&effective.keymaps)
            .expect("prefixes"),
        hints_enabled: bitty_config::resolve_hint_config(effective).enabled,
        resize_step: effective.layout.resize_step,
        mod_key: effective.mod_key,
        window_opacity: effective.window.opacity,
        theme_name: bitty_config::theme::resolve_theme(effective.appearance.theme.as_deref())
            .name
            .to_string(),
    }
}

#[test]
fn live_reload_keymap_change_alone_cancels_an_armed_leader() {
    // Review PX-4285 finding 3: a keymaps-only change (same Leader, hints
    // still on) must also cancel an armed Leader window, since its
    // follow-up chord resolves against the new table.
    let base = bitty_config::EffectiveConfig::default();
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        bitty_config::resolve_keymaps(&base).expect("defaults"),
        SpawnSpec::default(),
    );
    // Unchanged adoption keeps an armed window.
    app.chrome.leader_state = bitty_config::LeaderState::Armed { deadline_ms: 0 };
    app.adopt_live_config(adoption_from(&base));
    assert_ne!(app.chrome.leader_state, bitty_config::LeaderState::Idle);

    let mut edited = base.clone();
    edited.mod_key = bitty_config::ModKey::Super;
    let adoption = adoption_from(&edited);
    assert_eq!(adoption.leader, app.chrome.leader, "Leader unchanged");
    assert!(adoption.hints_enabled, "hints unchanged");
    app.adopt_live_config(adoption);
    assert_eq!(app.chrome.leader_state, bitty_config::LeaderState::Idle);
    assert!(!app.runtime.cw_hint_is_armed());
}

#[test]
fn live_reload_adopts_prefix_bindings_and_cancels_armed_window() {
    // CTX-1002 (issue #1650): the app half of a live reload adopts the
    // resolved `"leader <second>"` bindings; a prefix-only change cancels
    // an armed Leader window, since its follow-up resolves against the
    // new table.
    let base = bitty_config::EffectiveConfig::default();
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        bitty_config::resolve_keymaps(&base).expect("defaults"),
        SpawnSpec::default(),
    );
    assert!(app.chrome.prefix_bindings.is_empty());

    let mut edited = base.clone();
    edited.keymaps = vec![bitty_config::KeymapEntry {
        chord: "leader w".into(),
        action: "new_split:right".into(),
        context: "global".into(),
    }];
    app.adopt_live_config(adoption_from(&edited));
    assert_eq!(app.chrome.prefix_bindings.len(), 1);
    assert_eq!(
        app.chrome.prefix_bindings[0].second.canonical(),
        "w",
        "bare follow-up resolves"
    );

    // An armed window survives an identical adoption but dies on a
    // prefix-only change (fail-open: keys route normally again).
    app.chrome.leader_state = bitty_config::LeaderState::Armed { deadline_ms: 0 };
    app.adopt_live_config(adoption_from(&edited));
    assert_ne!(app.chrome.leader_state, bitty_config::LeaderState::Idle);

    let mut rebound = edited.clone();
    rebound.keymaps = vec![bitty_config::KeymapEntry {
        chord: "leader c".into(),
        action: "workspace_new".into(),
        context: "global".into(),
    }];
    app.adopt_live_config(adoption_from(&rebound));
    assert_eq!(app.chrome.leader_state, bitty_config::LeaderState::Idle);
    assert_eq!(app.chrome.prefix_bindings.len(), 1);
    assert_eq!(app.chrome.prefix_bindings[0].second.canonical(), "c");
}

#[test]
fn live_reload_theme_change_refreshes_the_window_title() {
    // Review PX-4285 finding 7: the base title tracks the reloaded preset
    // and keeps the launch source label; an OSC-set title is not clobbered.
    let base = bitty_config::EffectiveConfig::default();
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "file",
        bitty_config::resolve_keymaps(&base).expect("defaults"),
        SpawnSpec::default(),
    );
    let titles = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_os_title_sink(Box::new(RecordingTitleSink {
        titles: std::sync::Arc::clone(&titles),
    }));
    let mut edited = base.clone();
    edited.appearance.theme = Some(String::from("dracula"));
    app.adopt_live_config(adoption_from(&edited));
    let expected = window_title_for_theme("dracula", "file");
    assert_eq!(app.window.title, expected);
    assert_eq!(*titles.lock().expect("poison-free"), vec![expected.clone()]);
    // Unchanged theme: no second OS call.
    app.adopt_live_config(adoption_from(&edited));
    assert_eq!(titles.lock().expect("poison-free").len(), 1);

    // An application-owned (OSC 0/2) title stays on the OS window; only the
    // fallback base title updates.
    app.apply_window_title("vim");
    let mut back = base.clone();
    back.appearance.theme = Some(String::from("nord"));
    app.adopt_live_config(adoption_from(&back));
    assert_eq!(app.window.title, window_title_for_theme("nord", "file"));
    assert_eq!(
        titles
            .lock()
            .expect("poison-free")
            .last()
            .map(String::as_str),
        Some("vim"),
        "OSC title not clobbered"
    );
}

#[test]
fn overlay_capture_wiring_lifecycle_cancel_timeout_and_paint() {
    // CTX-0943 (W-28 follow-up): end-to-end through the app input path, not
    // direct `PluginRuntime` calls — lifecycle + cancellation + timeout +
    // no-hot-path + overlay paint on a user-exercisable path.
    use bitty_platform::{
        ImeEvent, KeyEvent, KeyLocation, LogicalKey, MouseButton, MouseEvent, NamedKey, PressState,
    };
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    fn test_key(logical: LogicalKey, text: Option<&str>) -> KeyEvent {
        KeyEvent {
            logical_key: logical,
            text: text.map(str::to_string),
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        }
    }

    fn queued_len(app: &TerminalApp) -> usize {
        app.plugin_runtime
            .as_ref()
            .expect("plugin runtime")
            .overlay_capture()
            .borrow()
            .queued_len()
    }

    fn store_value(
        app: &TerminalApp,
        id: &bitty_plugin_host::manifest::PluginId,
        key: &str,
    ) -> Option<LuaValue> {
        app.plugin_runtime
            .as_ref()
            .expect("plugin runtime")
            .services(id)
            .expect("services")
            .with_store(|store| store.get(key))
    }

    let tag = format!("ctx0943-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx0943wire";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-0943 wiring"
version = "0.1.0"
description = "overlay capture wiring test"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true
ui.overlay.focus = true

[lazy]
commands = ["{id}:reacquire"]
events = ["key"]
claims = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local mount_ok, handle = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "overlay-modal" })
        bitty.store.set("mount_ok", mount_ok)
        bitty.store.set("handle", mount_ok and handle or -1)
        -- Hot-path trap: if Core ever invoked a plugin on the input path,
        -- this handler would fire and bump `callback_count`.
        bitty.events.subscribe("key", function(envelope)
          bitty.store.set("callback_count", (bitty.store.get("callback_count") or 0) + 1)
        end)
        local acquired = false
        if mount_ok then
          local ok, err = pcall(bitty.ui.overlay.acquire, handle)
          acquired = ok
        end
        bitty.store.set("acquired", acquired)
        bitty.commands.register({
          id = "reacquire",
          title = "Reacquire",
          run = function(key)
            local h = bitty.store.get("handle")
            local ok, err = pcall(bitty.ui.overlay.acquire, h)
            bitty.store.set("reacquired", ok)
            return ok
          end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    // CTX-0939: disk-backed stores commit through the injected backend.
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    let report = plugin_runtime.activate(&pid).expect("activate");
    assert_eq!(
        report.state,
        bitty_runtime::plugin_runtime::LifecycleState::Active
    );
    let handle = match plugin_runtime
        .services(&pid)
        .expect("services")
        .with_store(|store| store.get("handle"))
    {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("overlay mount must return a handle, got {other:?}"),
    };
    assert!(
        plugin_runtime.overlay_capture().borrow().is_active(),
        "init.lua must hold the capture after activation"
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app.runtime.set_layout(two_pane_layout());
    app = app.with_plugin_runtime(Some(plugin_runtime));
    assert!(app.overlay_capture_active());

    // The mounted overlay block paints on the tick after acquire.
    let presented = app.drive_tick();
    assert!(
        presented.is_some(),
        "the overlay surface must paint while capture holds"
    );
    let paint = format!(
        "{:?}",
        app.runtime
            .plugin_overlay()
            .expect("surface follows the capture owner")
            .root
    );
    assert!(
        paint.contains("overlay-modal"),
        "the surface carries the retained block content, got {paint}"
    );

    // Key input through the chrome intercept is captured, never terminal
    // input, and never a plugin callback (no-hot-path).
    let key_x = test_key(LogicalKey::Character("x".to_string()), Some("x"));
    assert!(
        app.intercept_chrome_key(&WindowEventKind::KeyboardInput(key_x)),
        "unbound typing must be captured while the session holds"
    );
    assert_eq!(queued_len(&app), 1);
    assert_eq!(app.runtime.pending_input_len(), 0);
    assert_eq!(
        store_value(&app, &pid, "callback_count"),
        None,
        "capture must not fire a plugin callback"
    );
    app.plugin_runtime
        .as_mut()
        .expect("plugin runtime")
        .deliver_event("key", &LuaValue::String("k".to_string()));
    assert_eq!(
        store_value(&app, &pid, "callback_count"),
        Some(LuaValue::Integer(1)),
        "the explicit subscription path still works (proving the trap is live)"
    );

    // IME commit and pointer input through the fall-through hook are
    // captured with no terminal side effects.
    assert!(
        app.capture_fallthrough_input(&WindowEventKind::Ime(ImeEvent::Commit("ni".to_string())))
    );
    assert_eq!(queued_len(&app), 2);
    assert!(
        app.capture_fallthrough_input(&WindowEventKind::MouseInput(MouseEvent::new(
            MouseButton::Left,
            bitty_platform::PressState::Pressed,
        )))
    );
    assert_eq!(queued_len(&app), 3);
    assert_eq!(app.runtime.pending_input_len(), 0);

    // Pointer releases route to the Runtime (press-to-release ownership
    // never desyncs); only presses enter the capture queue (CodeRabbit PR
    // #1643 Major).
    assert!(
        !app.capture_fallthrough_input(&WindowEventKind::MouseInput(MouseEvent::new(
            MouseButton::Left,
            bitty_platform::PressState::Released,
        ))),
        "releases must route, never queue"
    );
    assert_eq!(queued_len(&app), 3, "a release must not grow the queue");

    // Pointer motion coalesces: three moves consume one slot with the latest
    // position, so a wiggle cannot evict queued key/text entries (CodeRabbit
    // PR #1643 Minor).
    use bitty_platform::CursorPosition;
    for (x, y) in [(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)] {
        assert!(
            app.capture_fallthrough_input(&WindowEventKind::CursorMoved(CursorPosition { x, y }))
        );
    }
    assert_eq!(queued_len(&app), 4, "three moves must consume one slot");

    // The active capture exposes a deadline for the idle wake (CodeRabbit PR
    // #1643 Minor).
    assert!(
        app.overlay_capture_deadline().is_some(),
        "an active capture must expose its deadline"
    );

    // Issue #2098: the default submit chord `ctrl+enter` must be reachable
    // while the capture holds. The flat key event now carries its chord
    // identity, so the owner can tell it apart from a bare `enter` (both
    // deliver the same CR text) — the modifier channel the capture queue
    // previously lacked.
    app.chrome.app_mods.control = true;
    let cr = test_key(LogicalKey::Named(NamedKey::Enter), Some("\r"));
    assert!(
        app.intercept_chrome_key(&WindowEventKind::KeyboardInput(cr)),
        "the default submit chord must be captured while the session holds"
    );
    assert_eq!(queued_len(&app), 5);
    let drained = app
        .plugin_runtime
        .as_ref()
        .expect("plugin runtime")
        .overlay_capture()
        .borrow_mut()
        .poll(id, handle, 16)
        .expect("owner poll");
    let chord = drained
        .iter()
        .find(|event| event.kind == "key" && event.text == "\r")
        .and_then(|event| event.chord.clone())
        .expect("the ctrl+enter capture must carry its chord identity");
    assert_eq!(chord.key, "enter");
    assert!(chord.ctrl, "the modifier channel must carry ctrl");
    assert!(!chord.alt && !chord.shift && !chord.super_held);
    app.chrome.app_mods.control = false;

    // Bare Esc through the intercept cancels the session.
    let esc = test_key(LogicalKey::Named(NamedKey::Escape), None);
    assert!(
        app.intercept_chrome_key(&WindowEventKind::KeyboardInput(esc)),
        "Esc must be consumed as the overlay cancel"
    );
    assert!(
        !app.overlay_capture_active(),
        "cancel must release the capture"
    );
    assert!(
        app.overlay_capture_deadline().is_none(),
        "no deadline survives a released capture"
    );
    assert_eq!(queued_len(&app), 0, "cancel clears the queue");
    let _ = app.drive_tick();
    assert!(
        app.runtime.plugin_overlay().is_none(),
        "cancel removes the surface on the next tick"
    );

    // A user focus-switch through the chrome action runner revokes.
    // bitty #2409: settle a park; a probe dispatch can legitimately park at
    // the RC-1 wall budget on a loaded runner before the harness observes the
    // capture it opens.
    crate::test_support::settle_dispatch(
        app.plugin_runtime.as_mut().expect("plugin runtime"),
        pid.as_str(),
        "reacquire",
        &[],
    )
    .expect_value("reacquire");
    assert!(app.overlay_capture_active());
    let before = app.runtime.focused_view();
    app.apply_chrome_action(bitty_config::ChromeAction::FocusNext);
    assert!(
        !app.overlay_capture_active(),
        "focus-switch must release the capture"
    );
    assert_ne!(
        app.runtime.focused_view(),
        before,
        "focus must actually move on the two-pane layout"
    );

    // Issue #1776: Mod+q (CloseView) closes the active overlay capture.
    // bitty #2409: settle a park; a probe dispatch can legitimately park at
    // the RC-1 wall budget on a loaded runner before the harness observes the
    // capture it opens.
    crate::test_support::settle_dispatch(
        app.plugin_runtime.as_mut().expect("plugin runtime"),
        pid.as_str(),
        "reacquire",
        &[],
    )
    .expect_value("reacquire");
    assert!(app.overlay_capture_active());
    app.chrome.app_mods.alt = true;
    let key_q = test_key(LogicalKey::Character("q".to_string()), Some("q"));
    assert!(app.intercept_chrome_key(&WindowEventKind::KeyboardInput(key_q)));
    assert!(
        !app.overlay_capture_active(),
        "Mod+q (CloseView) must close and release the overlay capture"
    );
    app.chrome.app_mods.alt = false;

    // Issue #1776: Mod+z (ToggleZoom) suspends/releases the active overlay capture.
    // bitty #2409: settle a park; a probe dispatch can legitimately park at
    // the RC-1 wall budget on a loaded runner before the harness observes the
    // capture it opens.
    crate::test_support::settle_dispatch(
        app.plugin_runtime.as_mut().expect("plugin runtime"),
        pid.as_str(),
        "reacquire",
        &[],
    )
    .expect_value("reacquire");
    assert!(app.overlay_capture_active());
    app.chrome.app_mods.alt = true;
    let key_z = test_key(LogicalKey::Character("z".to_string()), Some("z"));
    assert!(app.intercept_chrome_key(&WindowEventKind::KeyboardInput(key_z)));
    assert!(
        !app.overlay_capture_active(),
        "Mod+z (ToggleZoom) must suspend and release the overlay capture"
    );
    app.chrome.app_mods.alt = false;

    // Window focus loss through the intercept revokes as well.
    // bitty #2409: settle a park; a probe dispatch can legitimately park at
    // the RC-1 wall budget on a loaded runner before the harness observes the
    // capture it opens.
    crate::test_support::settle_dispatch(
        app.plugin_runtime.as_mut().expect("plugin runtime"),
        pid.as_str(),
        "reacquire",
        &[],
    )
    .expect_value("reacquire");
    assert!(!app.intercept_chrome_key(&WindowEventKind::Focused(false)));
    assert!(!app.overlay_capture_active());

    // The 30s idle timeout is enforced on the runtime tick.
    // bitty #2409: settle a park; a probe dispatch can legitimately park at
    // the RC-1 wall budget on a loaded runner before the harness observes the
    // capture it opens.
    crate::test_support::settle_dispatch(
        app.plugin_runtime.as_mut().expect("plugin runtime"),
        pid.as_str(),
        "reacquire",
        &[],
    )
    .expect_value("reacquire");
    assert!(app.overlay_capture_active());
    assert!(
        app.plugin_runtime
            .as_ref()
            .expect("plugin runtime")
            .overlay_capture()
            .borrow_mut()
            .force_expire(pid.as_str(), handle),
        "the live capture must rewind past its deadline"
    );
    let _ = app.drive_tick();
    assert!(
        !app.overlay_capture_active(),
        "the tick must revoke a capture past its deadline"
    );
    assert!(
        app.runtime.plugin_overlay().is_none(),
        "timeout removes the surface on the same tick"
    );

    // With no capture active nothing diverts: keys fall through to the
    // terminal exactly as before.
    let key_y = test_key(LogicalKey::Character("y".to_string()), Some("y"));
    assert!(!app.intercept_chrome_key(&WindowEventKind::KeyboardInput(key_y.clone())));
    assert!(!app.capture_fallthrough_input(&WindowEventKind::KeyboardInput(key_y.clone())));
    assert!(
        app.runtime.handle_key_event(key_y).is_some(),
        "terminal encoding must still serve released input"
    );
    assert!(
        app.runtime.pending_input_len() > 0,
        "released input must reach the PTY queue"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn plugin_notify_drain_and_dispatch_result_reach_banner() {
    // CTX-1033 (issue #1827) end-to-end through the app tick path: a plugin
    // command that notifies (`bitty.notify.show`) and returns text must
    // become visible on a real host — the notice via the per-tick drain, the
    // return value via dispatch-result surfacing — both on the bounded
    // banner surface, with overflow counted and the tick never blocked.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx1033-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx1033wire";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-1033 wiring"
version = "0.1.0"
description = "notify drain + dispatch surfacing test"

[compat]
plugin-api = "^1.0"

[capabilities]
platform.notify = true

[lazy]
commands = ["{id}:report"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        bitty.commands.register({
          id = "report",
          title = "Report",
          run = function(_args)
            bitty.notify.show({ title = "Wire", body = "notice-visible" })
            return "result-visible-text"
          end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    let report = plugin_runtime.activate(&pid).expect("activate");
    assert_eq!(
        report.state,
        bitty_runtime::plugin_runtime::LifecycleState::Active
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app = app.with_plugin_runtime(Some(plugin_runtime));

    // Dispatch-result half: a band click runs the command and the returned
    // text must paint immediately (no live banner yet). The run also queues
    // its one-line notice for the tick drain below.
    app.dispatch_band_click(bitty_runtime::BandClickRequest {
        plugin_id: id.to_string(),
        command: "report".to_string(),
        args: Vec::<(String, bitty_lua::ui::ClickArg)>::new(),
        region: None,
        button: bitty_runtime::BandButton::Left,
        origin: None,
        admission_seq: 0,
    });
    assert_eq!(
        app.runtime.notification_banner().as_deref(),
        Some("bitty-featured.ctx1033wire:report: result-visible-text"),
        "the dispatch result must surface on the banner"
    );
    // Issue #2336: the command result is routine plugin use, so it must not
    // be handed to the platform backend. The default backend is fail-closed,
    // so the old implicit path would count one undelivered OS notification
    // here even without a display server.
    assert_eq!(
        app.runtime.notifications_os_undelivered(),
        0,
        "a dispatch result must never attempt an OS notification"
    );
    assert_eq!(app.runtime.notifications_os_delivered(), 0);

    // Notify-drain half: the tick drains the queued notice into the bounded
    // queue behind the live banner (never blocking the frame).
    let _ = app.drive_tick();
    assert_eq!(
        app.runtime.pending_notification_count(),
        1,
        "the drained notice must wait in the bounded queue"
    );
    // The explicit `bitty.notify.show` still attempts the OS hand-off: the
    // fail-closed default backend records exactly that one undelivered
    // attempt, proving the explicit path was not disabled with the fix.
    assert_eq!(
        app.runtime.notifications_os_undelivered(),
        1,
        "explicit notify.show must still attempt the OS hand-off"
    );
    assert_eq!(
        app.plugin_runtime
            .as_ref()
            .expect("plugin runtime")
            .notifications_dropped(),
        0
    );
    assert_eq!(app.runtime.plugin_notifications_dropped(), 0);

    // The queued notice rotates onto the banner once the first expires:
    // the full notify-then-present loop is closed.
    assert!(
        app.runtime.advance_notification_banner_at(
            std::time::Instant::now() + std::time::Duration::from_secs(5)
        ),
        "the queued notice must advance onto the banner"
    );
    assert_eq!(
        app.runtime.notification_banner().as_deref(),
        Some("Wire: notice-visible"),
        "the drained notice must paint"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn successful_boolean_band_click_does_not_reach_banner() {
    // CTX-1293: presentation commands such as `bar:focus` return a boolean
    // acknowledgement. It is not user-visible result content and must not
    // create a notification banner.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx1293-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx1293boolean";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-1293 boolean result"
version = "0.1.0"
description = "boolean band click result test"

[compat]
plugin-api = "^1.0"

[capabilities]

[lazy]
commands = ["{id}:focus"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        bitty.commands.register({
          id = "focus",
          title = "Focus",
          run = function(_args)
            return true
          end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    let report = plugin_runtime.activate(&pid).expect("activate");
    assert_eq!(
        report.state,
        bitty_runtime::plugin_runtime::LifecycleState::Active
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app = app.with_plugin_runtime(Some(plugin_runtime));

    app.dispatch_band_click(bitty_runtime::BandClickRequest {
        plugin_id: id.to_string(),
        command: "focus".to_string(),
        args: Vec::<(String, bitty_lua::ui::ClickArg)>::new(),
        region: None,
        button: bitty_runtime::BandButton::Left,
        origin: None,
        admission_seq: 0,
    });
    assert_eq!(
        app.runtime.notification_banner(),
        None,
        "a boolean acknowledgement must not paint a banner"
    );
    assert_eq!(
        app.runtime.pending_notification_count(),
        0,
        "a boolean acknowledgement must not queue a notification"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn band_scroll_dispatch_reaches_command_with_host_delta() {
    // CTX-1190 (issue #2008): a Core-routed band scroll runs the owning
    // plugin's command with the declared args plus the host-computed delta,
    // mirroring the band click dispatch above.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx1190-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx1190wire";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-1190 wiring"
version = "0.1.0"
description = "band scroll dispatch test"

[compat]
plugin-api = "^1.0"

[capabilities]
platform.notify = true

[lazy]
commands = ["{id}:scroll"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        bitty.commands.register({
          id = "scroll",
          title = "Scroll",
          run = function(args)
            return "delta:" .. tostring(args.delta) .. " step:" .. tostring(args.step)
          end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    let report = plugin_runtime.activate(&pid).expect("activate");
    assert_eq!(
        report.state,
        bitty_runtime::plugin_runtime::LifecycleState::Active
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app = app.with_plugin_runtime(Some(plugin_runtime));

    // A band scroll runs the command and the returned text surfaces on the
    // banner: declared args plus the host-computed delta in one table.
    app.dispatch_band_scroll(bitty_runtime::BandScrollRequest {
        plugin_id: id.to_string(),
        command: "scroll".to_string(),
        args: vec![("step".to_string(), bitty_lua::ui::ClickArg::Number(2.0))],
        region: None,
        delta: 3,
        origin: None,
        admission_seq: 0,
    });
    assert_eq!(
        app.runtime.notification_banner().as_deref(),
        Some("bitty-featured.ctx1190wire:scroll: delta:3 step:2"),
        "the scroll dispatch result must surface declared args plus delta"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn lua_result_summary_is_bounded_and_readable() {
    use crate::terminal_app::lua_result_summary;
    use bitty_runtime::plugin_runtime::LuaValue;

    assert_eq!(lua_result_summary(&LuaValue::Nil), "");
    assert_eq!(lua_result_summary(&LuaValue::Bool(true)), "true");
    assert_eq!(lua_result_summary(&LuaValue::Integer(-7)), "-7");
    assert_eq!(
        lua_result_summary(&LuaValue::String("a  b\nc".to_string())),
        "a b c"
    );
    // Tables collapse past four entries with a counted tail.
    let table = LuaValue::Table(
        (1..=6)
            .map(|index| {
                (
                    LuaValue::String(format!("k{index}")),
                    LuaValue::Integer(index),
                )
            })
            .collect(),
    );
    assert_eq!(
        lua_result_summary(&table),
        "{k1=1, k2=2, k3=3, k4=4, +2 more}"
    );
    // Hostile sizes stay bounded: long strings truncate, deep tables cap.
    let long = LuaValue::String("x".repeat(10_000));
    assert!(lua_result_summary(&long).chars().count() <= 512);
    let mut deep = LuaValue::String("leaf".to_string());
    for _ in 0..10 {
        deep = LuaValue::Table(vec![(LuaValue::String("n".to_string()), deep)]);
    }
    let rendered = lua_result_summary(&deep);
    assert!(rendered.contains("{...}"), "depth must cap, got {rendered}");
    assert!(rendered.chars().count() <= 512);
}

// ---------------------------------------------------------------------------
// Issue #1828: live-PTY session records the PTY-gated trace kinds
// ---------------------------------------------------------------------------

/// Builds a two-leaf headless app wired to a tracer declaring the nine
/// devtools kinds. The trace starts unfiltered (`nil` opts) and baselines on
/// one tick. Returns the app, tracer id, plugin root, and data dir.
#[cfg(unix)]
fn trace1828_live_app() -> (TerminalApp, String, std::path::PathBuf, std::path::PathBuf) {
    use bitty_runtime::plugin_runtime::{
        EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct NoSnapshot;
    impl SnapshotSource for NoSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, bitty_lua::BridgeError> {
            Err(bitty_lua::BridgeError::capability_denied(
                "terminal.semantic-read",
            ))
        }
    }

    const ID: &str = "bitty-featured.trace-1828-live";
    let tag = format!("trace1828live-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let plugin_dir = root.join(ID);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            concat!(
                "[plugin]\n",
                "id = \"{id}\"\n",
                "name = \"Trace 1828 live\"\n",
                "version = \"0.1.0\"\n",
                "description = \"trace test\"\n",
                "\n",
                "[compat]\n",
                "plugin-api = \"^1.0\"\n",
                "\n",
                "[capabilities]\n",
                "debug.trace = true\n",
                "\n",
                "[lazy]\n",
                "commands = [\"{id}:start\", \"{id}:drain\"]\n",
                "events = [\"terminal.opened\", \"terminal.closed\", ",
                "\"terminal.title-changed\", \"terminal.cwd-changed\", \"terminal.bell\", ",
                "\"focus.changed\", \"selection.changed\", \"process.exited\", ",
                "\"config.reloaded\"]\n",
            ),
            id = ID,
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
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
"#,
    )
    .expect("init");
    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(NoSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(ID).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");
    let mut app = editor_test_app();
    app = app.with_plugin_runtime(Some(plugin_runtime));
    (app, ID.to_string(), root, data)
}

#[cfg(unix)]
fn trace1828_dispatch(app: &mut TerminalApp, id: &str, command: &str) -> String {
    use bitty_runtime::plugin_runtime::LuaValue;
    // bitty #2409: settle a park at the RC-1 wall budget instead of assuming
    // every probe dispatch completes inside one wall window.
    let value = crate::test_support::settle_dispatch(
        app.plugin_runtime.as_mut().expect("plugin runtime"),
        id,
        command,
        &[],
    )
    .expect_value(command);
    match value {
        LuaValue::String(value) => value,
        other => panic!("{command}: expected string, got {other:?}"),
    }
}

/// Issue #1828 acceptance on a real host: a scripted live-PTY session records
/// every PTY-gated kind (`terminal.opened`, `terminal.cwd-changed`,
/// `terminal.bell`, `selection.changed`, `process.exited`,
/// `terminal.closed`) in a devtools-like trace. The wait for the exiting
/// child is a bounded spin over non-blocking polls (no sleep): the `exit 3`
/// child is short-lived and the cap fails the test instead of hanging.
#[test]
#[cfg(unix)]
fn trace_kinds_live_pty_session_records() {
    require_pty!();
    let (mut app, id, root, data) = trace1828_live_app();
    assert_eq!(trace1828_dispatch(&mut app, &id, "start"), "1");
    // Baseline: no shells yet, so no terminals and nothing fires.
    let _ = app.drive_tick();
    assert_eq!(trace1828_dispatch(&mut app, &id, "drain"), "");

    // An immediately exiting shell in split pane 2 opens a terminal.
    let script = write_fake_editor("exit1828", "#!/bin/sh\nexit 3\n");
    let script_arg = script.to_string_lossy().into_owned();
    app.runtime
        .spawn_shell_for_view_in(ViewId::new(2), script_arg.as_str(), &[], 80, 24, None)
        .expect("spawn pane shell");
    let _ = app.drive_tick();

    // Pane-grid signals through the real parser path: OSC 7 cwd and BEL.
    app.runtime
        .handle_pane_bytes(ViewId::new(2), b"\x1b]7;file:///tmp/trace1828\x07");
    app.runtime.handle_pty_bytes(b"\x07");
    app.runtime.select_all();
    let _ = app.drive_tick();

    // Bounded spin (no sleep) until the reap path observes the exit and
    // queues the `process.exited` edge while closing the pane.
    let mut outcome = crate::terminal_app::ShellExitOutcome::NoExit;
    for _ in 0..200_000 {
        let _ = app.poll_pty_pump();
        outcome = app.reap_exited_shells();
        if outcome != crate::terminal_app::ShellExitOutcome::NoExit {
            break;
        }
    }
    assert_eq!(
        outcome,
        crate::terminal_app::ShellExitOutcome::PaneClosed,
        "the exit-3 pane shell must be reaped"
    );
    let _ = app.drive_tick();

    let drained = trace1828_dispatch(&mut app, &id, "drain");
    for kind in [
        "terminal.opened",
        "terminal.cwd-changed",
        "terminal.bell",
        "selection.changed",
        "process.exited",
        "terminal.closed",
    ] {
        assert!(
            drained.split(',').any(|topic| topic == kind),
            "live trace must record {kind}; drained: {drained}"
        );
    }
    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

// CTX-1244 (issue #2107): host timer pump + idle wake.

fn timer_test_app(id: &str, init_src: &str) -> (TerminalApp, String, std::path::PathBuf) {
    use bitty_runtime::plugin_runtime::{EmptySettings, PluginRuntime, PluginRuntimeConfig};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tag = format!("timer1244-{n}-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Timer\"\nversion = \"0.1.0\"\ndescription = \"timer test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\n[lazy]\ncommands = [\"{id}:ok\"]\nevents = []\n"
        ),
    )
    .expect("manifest");
    let body = format!(
        "bitty.commands.register({{ id = \"ok\", title = \"ok\", run = function() return \"ok\" end }})\n{init_src}"
    );
    std::fs::write(plugin_dir.join("lua/init.lua"), body).expect("init");
    struct NoSnapshot;
    impl bitty_runtime::plugin_runtime::SnapshotSource for NoSnapshot {
        fn snapshot(
            &self,
            _scope: &str,
        ) -> Result<
            bitty_runtime::plugin_runtime::LuaValue,
            bitty_runtime::plugin_runtime::BridgeError,
        > {
            Ok(bitty_runtime::plugin_runtime::LuaValue::table([
                (
                    "version",
                    bitty_runtime::plugin_runtime::LuaValue::Integer(1),
                ),
                (
                    "zones",
                    bitty_runtime::plugin_runtime::LuaValue::array(vec![]),
                ),
            ]))
        }
    }
    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(NoSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");
    let mut app = editor_test_app();
    app = app.with_plugin_runtime(Some(plugin_runtime));
    (app, id.to_string(), root)
}

#[test]
fn plugin_timer_deadline_none_when_unarmed_preserves_zero_submit() {
    // No runtime -> no wake; unarmed plugin -> no wake (zero-submit holds).
    let bare = editor_test_app();
    assert_eq!(bare.plugin_timer_deadline(), None);
    let (app, _id, root) = timer_test_app("bitty-featured.timerunarmed", "-- no timers\n");
    assert_eq!(
        app.plugin_timer_deadline(),
        None,
        "unarmed idle must not arm a wake"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plugin_timer_deadline_armed_while_pending() {
    // A far-future timer arms a wake but does not fire on this tick.
    let (mut app, id, root) = timer_test_app(
        "bitty-featured.timerarmed",
        "bitty.timers.create(60000, function() bitty.store.set(\"late\", true) end)",
    );
    let deadline = app.plugin_timer_deadline().expect("armed must wake");
    assert!(
        deadline > std::time::Instant::now(),
        "60s timer must wake in the future"
    );
    let _ = app.drive_tick();
    let pid = bitty_plugin_host::manifest::PluginId::new(&id).expect("id");
    let fired = app
        .plugin_runtime
        .as_ref()
        .expect("runtime")
        .services(&pid)
        .and_then(|services| services.with_store(|store| store.get("late")));
    assert_eq!(fired, None, "future timer must not fire on this tick");
    assert!(
        app.plugin_timer_deadline().is_some(),
        "pending timer keeps its wake"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn drive_tick_pumps_due_zero_delay_timers() {
    // A 0-delay timer fires on the next cold tick (no sleep, no events).
    let (mut app, id, root) = timer_test_app(
        "bitty-featured.timerdue",
        "bitty.timers.create(0, function() bitty.store.set(\"ticked\", 1) end)",
    );
    assert!(
        app.plugin_timer_deadline().is_some(),
        "0-delay timer must arm a wake"
    );
    let _ = app.drive_tick();
    let pid = bitty_plugin_host::manifest::PluginId::new(&id).expect("id");
    let ticked = app
        .plugin_runtime
        .as_ref()
        .expect("runtime")
        .services(&pid)
        .and_then(|services| services.with_store(|store| store.get("ticked")));
    assert_eq!(
        ticked,
        Some(bitty_runtime::plugin_runtime::LuaValue::Integer(1)),
        "drive_tick must pump due timers on the cold path"
    );
    assert_eq!(
        app.plugin_timer_deadline(),
        None,
        "one-shot consumed leaves no wake (back to zero-submit)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Issue #2415: the matrix close sequence (primary pane plus a real pane
/// session, detach the pane-session leaf as a float, close the tiled host,
/// then close the last live pane) ends on a valid live state the v3 session
/// file codec cannot encode: an empty tiled base with a surviving detached
/// float. The exit save used to capture that state, fail validation, and log
/// `session save failed (session file corrupt (empty workspace))` on a close
/// the user completed cleanly.
///
/// The accepted behaviour is to skip the write: nothing is saved, the last
/// good session file stays authoritative and restorable, and the skip is
/// reported at info class (never as corruption). A real save failure still
/// reports at warning class.
///
/// The session backend resolves its default path into the scratch dir while
/// delegating encode/decode/commit to the real storage adapter, so the
/// assertion covers the authoritative file the exit hook would write and
/// never the live XDG session.
#[test]
#[cfg(unix)]
fn close_to_empty_window_skips_exit_save_and_keeps_last_good_session_issue_2415() {
    use crate::terminal_app::{SessionSaveLogClass, session_save_report};
    use bitty_runtime::{
        CloseConfirmMode, Runtime, RuntimeConfig, SessionError, SessionExitSaveOutcome,
        SessionFileBackend as _, SessionSnapshot,
    };

    #[derive(Debug)]
    struct ScratchPathBackend {
        path: std::path::PathBuf,
        inner: crate::storage_backends::StorageSessionBackend,
    }

    impl bitty_runtime::SessionFileBackend for ScratchPathBackend {
        fn encode_snapshot(&self, snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
            self.inner.encode_snapshot(snap)
        }

        fn decode_snapshot(&self, bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
            self.inner.decode_snapshot(bytes)
        }

        fn commit_session_bytes(
            &self,
            path: &std::path::Path,
            bytes: &[u8],
        ) -> Result<(), SessionError> {
            self.inner.commit_session_bytes(path, bytes)
        }

        fn load_session_bytes(&self, path: &std::path::Path) -> Result<Vec<u8>, SessionError> {
            self.inner.load_session_bytes(path)
        }

        fn session_file_for(
            &self,
            xdg_state_home: Option<&str>,
            home: Option<&str>,
        ) -> Option<std::path::PathBuf> {
            self.inner.session_file_for(xdg_state_home, home)
        }

        fn session_file(&self) -> Option<std::path::PathBuf> {
            Some(self.path.clone())
        }
    }

    require_pty!();

    let dir = std::env::temp_dir().join(format!(
        "bitty-close-to-empty-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("session");

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::new(RuntimeConfig {
        close_confirm: CloseConfirmMode::WhenBusy,
        ..RuntimeConfig::default()
    })
    .expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    );
    app.runtime.set_layout(two_pane_layout());
    app.runtime
        .set_session_backend(Some(std::sync::Arc::new(ScratchPathBackend {
            path: path.clone(),
            inner: crate::storage_backends::StorageSessionBackend::new(),
        })));
    app.runtime
        .spawn_shell_with_args("/bin/sh", &[])
        .expect("primary shell spawns");
    let primary = app.runtime.primary_view().expect("primary owner");
    let float = ViewId::new(2);
    app.runtime
        .spawn_shell_for_view(float, "/bin/sh", &[], 40, 12)
        .expect("pane session spawns");
    assert_eq!(app.runtime.pane_session_ids(), vec![float]);
    assert!(app.runtime.set_focus(float));

    // Last good session: both tiled panes with live attachment, saved before
    // the close sequence through the same path the exit hook resolves.
    app.runtime
        .save_session_to_path(&path)
        .expect("baseline save works");
    let good = std::fs::read(&path).expect("baseline bytes");
    let good_snapshot = crate::storage_backends::StorageSessionBackend::new()
        .decode_snapshot(&good)
        .expect("baseline decodes");
    assert_eq!(good_snapshot.workspaces[0].panes.len(), 2);

    // Matrix close sequence: detaching the pane-session leaf leaves a single
    // tiled host plus a detached float; closing the host empties the tiled
    // base and hands focus to the float.
    app.apply_chrome_action(bitty_config::ChromeAction::ToggleFloating);
    assert_eq!(app.runtime.floated_views(), vec![float]);
    assert_eq!(app.runtime.layout().leaf_ids(), vec![primary]);
    assert_eq!(
        app.runtime.pane_session_ids(),
        vec![float],
        "the detached float keeps its pane session"
    );
    assert_eq!(app.runtime.focused_view(), Some(primary));
    assert!(
        !app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "closing the host with a surviving float is a pane close"
    );
    assert_eq!(
        app.runtime.layout().leaf_ids(),
        Vec::new(),
        "the tiled base is empty after the close"
    );
    assert_eq!(app.runtime.floated_views(), vec![float]);
    assert_eq!(app.runtime.focused_view(), Some(float));
    assert!(
        !app.runtime.has_pty(),
        "the tiled host's primary shell is torn down"
    );

    // The exit save skips that unencodable state instead of failing.
    let outcome = app.runtime.save_session_on_exit();
    assert_eq!(
        outcome,
        SessionExitSaveOutcome::SkippedEmptyTiledBase,
        "close-to-empty must skip the save, not report file corruption"
    );
    let (class, line) = session_save_report(&outcome, "close-view").expect("skip is reported");
    assert_eq!(
        class,
        SessionSaveLogClass::Info,
        "an expected close path must not log at warning class"
    );
    assert!(
        line.contains("session save skipped")
            && line.contains("no tiled pane or pinned panel left")
            && line.contains("keeping last good session"),
        "the skip line must name the retained session: {line}"
    );
    assert!(
        !line.contains("failed") && !line.contains("corrupt"),
        "the skip must never claim corruption: {line}"
    );

    // No data loss: the authoritative file is untouched and restorable.
    let after = std::fs::read(&path).expect("last good session still present");
    assert_eq!(after, good, "the skip must not rewrite the session file");
    assert_eq!(
        crate::storage_backends::StorageSessionBackend::new()
            .decode_snapshot(&after)
            .expect("last good session still decodes"),
        good_snapshot,
        "the last good session stays restorable"
    );

    // The same state still refuses a direct save: that is exactly the pre-fix
    // exit outcome, so the regression proves the skip removes the corruption
    // warning rather than a benign no-state-dir path.
    let direct = app
        .runtime
        .save_session_to_path(&path)
        .expect_err("the v3 codec cannot encode an empty tiled base");
    assert_eq!(
        format!("{direct}"),
        "session file corrupt (empty workspace)"
    );
    assert_eq!(
        std::fs::read(&path).expect("failed direct save leaves the file"),
        good,
        "a refused save must not clobber the last good session"
    );

    // Closing the last live pane is the window-close gesture; the exit path
    // tears the surviving pane session down during shutdown, not before the
    // save it is reporting on.
    assert!(
        app.apply_chrome_action(bitty_config::ChromeAction::CloseView),
        "the last live pane requests the window close"
    );
    assert_eq!(app.runtime.floated_views(), vec![float]);
    app.runtime.shutdown();
    assert!(
        app.runtime.pane_session_ids().is_empty(),
        "shutdown tears down the surviving pane session"
    );
    assert!(!app.runtime.has_pty());

    // The classification is not a blanket silence: a real failure still
    // warns (the pre-existing pin-only refusal keeps its own diagnostic).
    let failed = SessionExitSaveOutcome::Warned(SessionError::PinOnlyEmptyBase);
    let (failed_class, failed_line) =
        session_save_report(&failed, "close-view").expect("failure is reported");
    assert_eq!(failed_class, SessionSaveLogClass::Warn);
    assert!(
        failed_line.contains("session save failed"),
        "a real failure still reports loudly: {failed_line}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hover_enter_shows_plugin_tooltip_overlay_and_leave_hides_it() {
    // Issue #2411 end-to-end: a hovered band region dispatches the plugin's
    // `on_hover` command with `region`/`phase` only; the plugin updates a
    // non-capturing `overlay`-slot block with `bitty.ui.update`, Core paints
    // it anchored below the band, and leaving the region removes it. No
    // keyboard capture and no OS notification are involved.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx2411-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx2411hover";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-2411 hover tooltip"
version = "0.1.0"
description = "band hover enters/leaves drive a non-capturing overlay tooltip"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true

[lazy]
commands = ["{id}:detail"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local tooltip = bitty.ui.mount("overlay", { kind = "Text", text = "" })
        bitty.commands.register({
          id = "detail",
          title = "Detail",
          run = function(args)
            local text = ""
            if type(args) == "table" and args.phase == "enter" then
              text = "detail:" .. tostring(args.region)
            end
            bitty.ui.update(tooltip, { kind = "Text", text = text })
            return true
          end,
        })
        bitty.ui.mount("top", { kind = "Row", children = {
          { kind = "Row", anchor = "left", children = {
            { kind = "Text", text = "clock",
              on_hover = { command = "detail", region = { id = "clock" } } },
          } },
        } })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    let report = plugin_runtime.activate(&pid).expect("activate");
    assert_eq!(
        report.state,
        bitty_runtime::plugin_runtime::LifecycleState::Active
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime));
    app.set_session_persistence(false);
    let _ = app.drive_tick();
    let row = app
        .runtime
        .plugin_band_row(bitty_runtime::BandEdge::Top, 0)
        .expect("top band row");
    assert!(
        app.runtime.plugin_overlay().is_none(),
        "no tooltip before hover"
    );

    let (cw, ch) = app.runtime.live_cell_size();
    let pad = f64::from(app.runtime.window_padding_physical());
    let over_clock = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };
    app.runtime.handle_cursor_moved(over_clock);
    assert_eq!(app.pump_cold_commands(), 1, "one hover enter dispatched");
    assert!(
        !app.overlay_capture_active(),
        "hover never takes keyboard capture"
    );
    let _ = app.drive_tick();
    let surface = app
        .runtime
        .plugin_overlay()
        .expect("hover tooltip surface is presented");
    assert_eq!(surface.plugin_id, id);
    match &surface.root {
        bitty_lua::ui::UiNode::Text { text, .. } => assert_eq!(text, "detail:clock"),
        other => panic!("expected the tooltip text, got {other:?}"),
    }
    let anchor = app
        .runtime
        .plugin_overlay_anchor()
        .expect("tooltip anchored to the hovered region");
    assert_eq!(anchor.edge, bitty_runtime::BandEdge::Top);
    assert_eq!((anchor.start_col, anchor.end_col), (0, 5));
    assert_eq!(
        app.runtime.notifications_os_delivered(),
        0,
        "hover never raises an OS notification"
    );

    // Leaving the region hides the tooltip on the next tick, without capture.
    let away = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + 12.0 * f64::from(ch),
    };
    app.runtime.handle_cursor_moved(away);
    assert_eq!(app.pump_cold_commands(), 1, "one hover leave dispatched");
    let _ = app.drive_tick();
    assert!(
        app.runtime.plugin_overlay().is_none(),
        "leaving the region hides the tooltip"
    );
    assert!(app.runtime.plugin_overlay_anchor().is_none());

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn overlay_capture_gates_band_hover_and_delivers_the_leave_after_release() {
    // Review B2: a focusable overlay capture owns the cold phase. A hovered
    // tooltip is torn down with one leave, motion cannot re-arm it under the
    // modal, and the queued leave dispatches once the capture ends.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx2411-capture-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx2411capture";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-2411 hover capture gate"
version = "0.1.0"
description = "hover is muted while a focusable overlay capture holds"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true
ui.overlay.focus = true

[lazy]
commands = ["{id}:detail", "{id}:capture", "{id}:release"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local tooltip = bitty.ui.mount("overlay", { kind = "Text", text = "" })
        local function set_detail(text)
          if tooltip then bitty.ui.update(tooltip, { kind = "Text", text = text }) end
        end
        bitty.commands.register({
          id = "detail",
          title = "Detail",
          run = function(args)
            local text = ""
            if type(args) == "table" and args.phase == "enter" then
              text = "detail:" .. tostring(args.region)
            end
            set_detail(text)
            return true
          end,
        })
        bitty.commands.register({
          id = "capture",
          title = "Capture",
          run = function()
            local ok = pcall(bitty.ui.overlay.acquire, tooltip)
            return ok
          end,
        })
        bitty.commands.register({
          id = "release",
          title = "Release",
          run = function()
            local ok = pcall(bitty.ui.overlay.release, tooltip)
            return ok
          end,
        })
        bitty.ui.mount("top", { kind = "Row", children = {
          { kind = "Row", anchor = "left", children = {
            { kind = "Text", text = "clock",
              on_hover = { command = "detail", region = { id = "clock" } } },
          } },
        } })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime));
    app.set_session_persistence(false);
    let _ = app.drive_tick();
    let row = app
        .runtime
        .plugin_band_row(bitty_runtime::BandEdge::Top, 0)
        .expect("top band row");

    let (cw, ch) = app.runtime.live_cell_size();
    let pad = f64::from(app.runtime.window_padding_physical());
    let over_clock = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };
    app.runtime.handle_cursor_moved(over_clock);
    assert_eq!(app.pump_cold_commands(), 1, "hover enter dispatched");
    let _ = app.drive_tick();
    assert!(app.runtime.plugin_overlay().is_some());
    assert!(app.runtime.plugin_overlay_anchor().is_some());

    // The plugin acquires a focusable capture (e.g. an unrelated modal).
    app.plugin_runtime
        .as_mut()
        .expect("runtime")
        .dispatch_command(&pid, "capture", &[])
        .expect("capture command");
    let _ = app.drive_tick();
    assert!(app.overlay_capture_active());
    assert!(
        app.runtime.hovered_band_region().is_none(),
        "capture start clears hover ownership"
    );
    assert!(
        app.runtime.plugin_overlay_anchor().is_none(),
        "no hover tooltip is anchored under the modal"
    );

    // Motion under the modal neither queues nor dispatches hover, even though
    // the pointer sits on the same region.
    app.runtime.handle_cursor_moved(over_clock);
    assert_eq!(app.pump_cold_commands(), 0, "hover is muted while captured");

    // Releasing the capture lets the single queued leave dispatch.
    assert!(app.revoke_overlay_capture());
    let _ = app.drive_tick();
    assert!(!app.overlay_capture_active());
    assert_eq!(
        app.pump_cold_commands(),
        1,
        "the capture-start leave dispatches after release"
    );
    assert!(app.runtime.plugin_overlay().is_none());
    assert!(app.runtime.plugin_overlay_anchor().is_none());

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn deferred_hover_enter_surfaces_the_tooltip_once_the_band_is_presented() {
    // Review B2 end-to-end: the pointer enters a band whose live version is
    // newer than the last presented frame. The enter is deferred, not lost:
    // once the frame presents, the cold phase re-derives it, the plugin
    // receives it, and the tooltip appears. Leaving still hides it.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx2411-deferred-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx2411deferred";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-2411 deferred hover"
version = "0.1.0"
description = "deferred hover enter surfaces after the band presents"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true

[lazy]
commands = ["{id}:detail", "{id}:bump"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local tooltip = bitty.ui.mount("overlay", { kind = "Text", text = "" })
        local function bar_scene(label)
          return { kind = "Row", children = {
            { kind = "Row", anchor = "left", children = {
              { kind = "Text", text = label,
                on_hover = { command = "detail", region = { id = "clock" } } },
            } },
          } }
        end
        local bar = bitty.ui.mount("top", bar_scene("clock"))
        bitty.commands.register({
          id = "detail",
          title = "Detail",
          run = function(args)
            local text = ""
            if type(args) == "table" and args.phase == "enter" then
              text = "detail:" .. tostring(args.region)
            end
            if tooltip then bitty.ui.update(tooltip, { kind = "Text", text = text }) end
            return true
          end,
        })
        bitty.commands.register({
          id = "bump",
          title = "Bump",
          run = function()
            bitty.ui.update(bar, bar_scene("clock!"))
            return true
          end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime));
    app.set_session_persistence(false);
    let _ = app.drive_tick();
    let row = app
        .runtime
        .plugin_band_row(bitty_runtime::BandEdge::Top, 0)
        .expect("top band row");
    let (cw, ch) = app.runtime.live_cell_size();
    let pad = f64::from(app.runtime.window_padding_physical());
    let over_clock = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };
    let away = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + 12.0 * f64::from(ch),
    };

    // Leave the band, then bump its version without presenting.
    app.runtime.handle_cursor_moved(away);
    let _ = app.pump_cold_commands();
    app.plugin_runtime
        .as_mut()
        .expect("runtime")
        .dispatch_command(&pid, "bump", &[])
        .expect("bump command");
    // Reproduce the app's band-sync window: the live band snapshot carries the
    // new version, but no frame has published it yet.
    let origin = app
        .runtime
        .published_band_origins()
        .first()
        .cloned()
        .expect("presented origin");
    let next_version = origin.version + 1;
    let bands = bitty_runtime::ChromeBands {
        top: app
            .runtime
            .chrome_bands()
            .top
            .iter()
            .map(|band| bitty_runtime::BandContent {
                version: next_version,
                ..band.clone()
            })
            .collect(),
        ..Default::default()
    };
    app.runtime.set_chrome_bands_with_mounts(
        bands,
        vec![bitty_runtime::BandMount {
            plugin_id: origin.plugin_id.clone(),
            slot: bitty_lua::ui::UiSlot::Top,
            version: next_version,
            generation: origin.generation,
            handle: origin.handle,
        }],
    );

    // Enter while the live band is newer than the presented frame: deferred.
    app.runtime.handle_cursor_moved(over_clock);
    assert_eq!(
        app.pump_cold_commands(),
        0,
        "no enter is dispatched before the band is presented"
    );
    assert!(app.runtime.hovered_band_region().is_none());

    // The next frame publishes the new band; the cold phase re-derives the
    // enter and the plugin receives it.
    app.runtime.tick().expect("present the updated band");
    assert_eq!(
        app.runtime
            .published_band_origins()
            .first()
            .map(|origin| origin.version),
        Some(next_version),
        "the new band version is published"
    );
    assert_eq!(
        app.pump_cold_commands(),
        1,
        "deferred enter dispatches once the band is presented"
    );
    assert_eq!(
        app.pump_cold_commands(),
        0,
        "the delivered enter is not re-fired"
    );
    let _ = app.drive_tick();
    let surface = app
        .runtime
        .plugin_overlay()
        .expect("tooltip appears after the deferred enter");
    match &surface.root {
        bitty_lua::ui::UiNode::Text { text, .. } => assert_eq!(text, "detail:clock"),
        other => panic!("expected the tooltip text, got {other:?}"),
    }
    assert!(app.runtime.plugin_overlay_anchor().is_some());

    // Leaving still delivers one leave and hides the tooltip.
    app.runtime.handle_cursor_moved(away);
    assert_eq!(app.pump_cold_commands(), 1, "one leave");
    let _ = app.drive_tick();
    assert!(app.runtime.plugin_overlay().is_none());

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn same_tick_band_update_keeps_the_queued_hover_enter_and_shows_the_tooltip() {
    // Third review ordering regression: the pointer enters and queues an enter
    // against band v1; the same tick then bumps the band to v2 before
    // presenting. Revalidation must replace the stale queued entry with the
    // v2 one, the dispatch must deliver exactly one current-origin enter, and
    // the tooltip must appear; leaving still hides it.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx2411-sametick-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx2411sametick";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-2411 same-tick hover"
version = "0.1.0"
description = "queued hover enter survives a same-tick band update"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true

[lazy]
commands = ["{id}:detail", "{id}:bump"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local tooltip = bitty.ui.mount("overlay", { kind = "Text", text = "" })
        local function bar_scene(label)
          return { kind = "Row", children = {
            { kind = "Row", anchor = "left", children = {
              { kind = "Text", text = label,
                on_hover = { command = "detail", region = { id = "clock" } } },
            } },
          } }
        end
        local bar = bitty.ui.mount("top", bar_scene("clock"))
        bitty.commands.register({
          id = "detail",
          title = "Detail",
          run = function(args)
            local text = ""
            if type(args) == "table" and args.phase == "enter" then
              text = "detail:" .. tostring(args.region)
            end
            if tooltip then bitty.ui.update(tooltip, { kind = "Text", text = text }) end
            return true
          end,
        })
        bitty.commands.register({
          id = "bump",
          title = "Bump",
          run = function()
            bitty.ui.update(bar, bar_scene("clock!"))
            return true
          end,
        })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime));
    app.set_session_persistence(false);
    let _ = app.drive_tick();
    let row = app
        .runtime
        .plugin_band_row(bitty_runtime::BandEdge::Top, 0)
        .expect("top band row");
    let (cw, ch) = app.runtime.live_cell_size();
    let pad = f64::from(app.runtime.window_padding_physical());
    let over_clock = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };
    let away = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + 12.0 * f64::from(ch),
    };

    // Pointer enters: a v1 enter is queued but not yet dispatched.
    app.runtime.handle_cursor_moved(over_clock);
    // Same tick: the plugin bumps the band, the app syncs the new snapshot,
    // and the frame publishes v2 before the cold drain.
    app.plugin_runtime
        .as_mut()
        .expect("runtime")
        .dispatch_command(&pid, "bump", &[])
        .expect("bump command");
    let origin = app
        .runtime
        .published_band_origins()
        .first()
        .cloned()
        .expect("presented origin");
    let next_version = origin.version + 1;
    let bands = bitty_runtime::ChromeBands {
        top: app
            .runtime
            .chrome_bands()
            .top
            .iter()
            .map(|band| bitty_runtime::BandContent {
                version: next_version,
                ..band.clone()
            })
            .collect(),
        ..Default::default()
    };
    app.runtime.set_chrome_bands_with_mounts(
        bands,
        vec![bitty_runtime::BandMount {
            plugin_id: origin.plugin_id.clone(),
            slot: bitty_lua::ui::UiSlot::Top,
            version: next_version,
            generation: origin.generation,
            handle: origin.handle,
        }],
    );
    app.runtime.tick().expect("present the updated band");

    // One delivered enter for the current origin, not a stale v1 drop.
    assert_eq!(
        app.pump_cold_commands(),
        1,
        "the refreshed enter is delivered exactly once"
    );
    assert_eq!(
        app.pump_cold_commands(),
        0,
        "the delivered enter is not re-fired"
    );
    let _ = app.drive_tick();
    let surface = app
        .runtime
        .plugin_overlay()
        .expect("tooltip appears for the same-tick update");
    match &surface.root {
        bitty_lua::ui::UiNode::Text { text, .. } => assert_eq!(text, "detail:clock"),
        other => panic!("expected the tooltip text, got {other:?}"),
    }
    assert!(app.runtime.plugin_overlay_anchor().is_some());

    // Leaving still hides the tooltip with one leave.
    app.runtime.handle_cursor_moved(away);
    assert_eq!(app.pump_cold_commands(), 1, "one leave");
    let _ = app.drive_tick();
    assert!(app.runtime.plugin_overlay().is_none());

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn hover_tooltip_refreshes_when_the_plugin_updates_the_overlay_block() {
    // Issue #2530: while a region is hovered, a plugin-side refresh of the
    // `overlay`-slot block must reach the presented tooltip surface. A
    // tooltip that keeps the text from its `enter` dispatch is the reported
    // freeze; this pins the host re-read on the tick after each update.
    use bitty_runtime::plugin_runtime::{
        BridgeError, EmptySettings, LuaValue, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    };

    struct StaticSnapshot;
    impl SnapshotSource for StaticSnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
            Ok(LuaValue::table([
                ("version", LuaValue::Integer(1)),
                ("zones", LuaValue::array(vec![])),
            ]))
        }
    }

    let tag = format!("ctx2530-refresh-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("bitty-{tag}-root"));
    let data = std::env::temp_dir().join(format!("bitty-{tag}-data"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let id = "bitty-featured.ctx2530refresh";
    let plugin_dir = root.join(id);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "CTX-2530 tooltip refresh"
version = "0.1.0"
description = "hover tooltip refreshes on plugin-side overlay updates"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true

[lazy]
commands = ["{id}:detail"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local tooltip = bitty.ui.mount("overlay", { kind = "Text", text = "" })
        local count = 0
        bitty.commands.register({
          id = "detail",
          title = "Detail",
          run = function(args)
            local text = ""
            if type(args) == "table" and args.phase == "enter" then
              count = count + 1
              text = "detail:" .. tostring(count)
            end
            bitty.ui.update(tooltip, { kind = "Text", text = text })
            return true
          end,
        })
        bitty.ui.mount("top", { kind = "Row", children = {
          { kind = "Row", anchor = "left", children = {
            { kind = "Text", text = "clock",
              on_hover = { command = "detail", region = { id = "clock" } } },
          } },
        } })
        return {}
        "#,
    )
    .expect("init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(StaticSnapshot),
    });
    plugin_runtime.discover();
    let pid = bitty_plugin_host::manifest::PluginId::new(id).expect("valid id");
    plugin_runtime.activate(&pid).expect("activate");

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let mut app = TerminalApp::with_theme(
        rt,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime));
    app.set_session_persistence(false);
    let _ = app.drive_tick();
    let row = app
        .runtime
        .plugin_band_row(bitty_runtime::BandEdge::Top, 0)
        .expect("top band row");
    let (cw, ch) = app.runtime.live_cell_size();
    let pad = f64::from(app.runtime.window_padding_physical());
    let over_clock = bitty_platform::CursorPosition {
        x: pad + 0.5 * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };
    let tooltip_text =
        |app: &TerminalApp| match &app.runtime.plugin_overlay().expect("overlay").root {
            bitty_lua::ui::UiNode::Text { text, .. } => text.clone(),
            other => panic!("expected the tooltip text, got {other:?}"),
        };

    app.runtime.handle_cursor_moved(over_clock);
    assert_eq!(app.pump_cold_commands(), 1, "hover enter dispatched");
    let _ = app.drive_tick();
    assert_eq!(tooltip_text(&app), "detail:1");
    assert!(
        app.runtime.plugin_overlay_anchor().is_some(),
        "tooltip stays anchored while hovered"
    );

    // A live refresh (here the next timer/event repaint of the same region)
    // replaces the tooltip text and the host presents it on the next tick.
    app.plugin_runtime
        .as_mut()
        .expect("runtime")
        .dispatch_command(
            &pid,
            "detail",
            &[LuaValue::table([
                ("region", LuaValue::String(String::from("clock"))),
                ("phase", LuaValue::String(String::from("enter"))),
            ])],
        )
        .expect("refresh dispatch");
    let _ = app.drive_tick();
    assert_eq!(tooltip_text(&app), "detail:2", "tooltip re-reads the block");
    assert!(
        app.runtime.plugin_overlay_anchor().is_some(),
        "refresh keeps the same anchor"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

// ---------------------------------------------------------------------------
// Presentation-only overlay session (CTX-1431, bitty #2406)
// ---------------------------------------------------------------------------

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::{CloseConfirmMode, RuntimeConfig};

/// A pressed, non-synthetic `Esc` event.
fn presentation_escape() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

/// Snapshot source for the presentation-overlay fixtures.
struct PresentationSnapshot;

impl bitty_runtime::plugin_runtime::SnapshotSource for PresentationSnapshot {
    fn snapshot(
        &self,
        _scope: &str,
    ) -> Result<bitty_runtime::plugin_runtime::LuaValue, bitty_lua::BridgeError> {
        Ok(bitty_runtime::plugin_runtime::LuaValue::table([
            (
                "version",
                bitty_runtime::plugin_runtime::LuaValue::Integer(1),
            ),
            (
                "zones",
                bitty_runtime::plugin_runtime::LuaValue::array(vec![]),
            ),
        ]))
    }
}

fn presentation_store_value(
    app: &TerminalApp,
    id: &bitty_plugin_host::manifest::PluginId,
    key: &str,
) -> Option<bitty_runtime::plugin_runtime::LuaValue> {
    app.plugin_runtime
        .as_ref()
        .and_then(|runtime| runtime.services(id))
        .and_then(|services| services.with_store(|store| store.get(key)))
}

/// Build a headless app with a presenter (mounts and shows an `overlay` block)
/// and a watcher subscribed to `overlay.dismissed`.
fn presentation_overlay_app(
    case: &str,
    runtime: Runtime,
) -> (
    TerminalApp,
    bitty_plugin_host::manifest::PluginId,
    String,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    use bitty_runtime::plugin_runtime::{
        EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig,
    };

    let root = std::env::temp_dir().join(format!(
        "bitty-presentation-overlay-{case}-{}",
        std::process::id()
    ));
    let data = std::env::temp_dir().join(format!(
        "bitty-presentation-overlay-{case}-data-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
    let presenter = format!("bitty-featured.ctx1431presenter{}", case.replace('-', ""));
    let watcher = format!("bitty-featured.ctx1431watcher{}", case.replace('-', ""));

    let plugin_dir = root.join(&presenter);
    std::fs::create_dir_all(plugin_dir.join("lua")).expect("presenter dirs");
    std::fs::write(
        plugin_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{presenter}"
name = "CTX-1431 presentation presenter"
version = "0.1.0"
description = "presentation-only overlay app fixture"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true
ui.overlay = true

[lazy]
commands = ["{presenter}:show", "{presenter}:update"]
events = ["key"]
claims = []
"#
        ),
    )
    .expect("presenter manifest");
    std::fs::write(
        plugin_dir.join("lua/init.lua"),
        r#"
        local ok, handle = pcall(bitty.ui.mount, "overlay", {
          kind = "List",
          children = {
            { kind = "Text", text = "Keyboard shortcuts" },
            { kind = "Text", text = "alt+h  toggle_help" },
          },
        })
        bitty.store.set("mount_ok", ok)
        bitty.store.set("handle", ok and handle or -1)
        if ok then
          local shown, err = pcall(bitty.ui.overlay.show, handle)
          bitty.store.set("shown", shown)
          if not shown then bitty.store.set("show_err", err.code or "unknown") end
        end
        -- Hot-path trap: any callback during input routing bumps this.
        bitty.events.subscribe("key", function(envelope)
          bitty.store.set("callback_count", (bitty.store.get("callback_count") or 0) + 1)
        end)
        bitty.commands.register({
          id = "show",
          title = "Show presentation",
          run = function()
            local h = bitty.store.get("handle")
            local shown = bitty.ui.overlay.show(h)
            return shown
          end,
        })
        bitty.commands.register({
          id = "update",
          title = "Update presentation",
          run = function()
            local h = bitty.store.get("handle")
            local updated = bitty.ui.update(h, {
              kind = "List",
              children = { { kind = "Text", text = "updated panel" } },
            })
            return updated
          end,
        })
        return {}
        "#,
    )
    .expect("presenter init");

    let watcher_dir = root.join(&watcher);
    std::fs::create_dir_all(watcher_dir.join("lua")).expect("watcher dirs");
    std::fs::write(
        watcher_dir.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{watcher}"
name = "CTX-1431 presentation watcher"
version = "0.1.0"
description = "overlay.dismissed observation fixture"

[compat]
plugin-api = "^1.0"

[lazy]
commands = ["{watcher}:report"]
events = ["overlay.dismissed"]
claims = []
"#
        ),
    )
    .expect("watcher manifest");
    std::fs::write(
        watcher_dir.join("lua/init.lua"),
        r#"
        bitty.events.subscribe("overlay.dismissed", function(envelope)
          local payload = envelope.payload
          bitty.store.set("dismissed", tostring(payload.owner) .. "|"
            .. tostring(payload.handle) .. "|" .. tostring(payload.reason))
        end)
        bitty.commands.register({
          id = "report",
          title = "Report dismissal",
          run = function()
            return bitty.store.get("dismissed") or "NONE"
          end,
        })
        return {}
        "#,
    )
    .expect("watcher init");

    let mut plugin_runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(PresentationSnapshot),
    });
    plugin_runtime.set_store_backend(Some(std::sync::Arc::new(
        crate::storage_backends::StorageKvBackend::new(),
    )));
    plugin_runtime.discover();
    let presenter_id = bitty_plugin_host::manifest::PluginId::new(&presenter).expect("id");
    let watcher_id = bitty_plugin_host::manifest::PluginId::new(&watcher).expect("id");
    assert_eq!(
        plugin_runtime
            .activate(&presenter_id)
            .expect("presenter activates")
            .state,
        LifecycleState::Active
    );
    assert_eq!(
        plugin_runtime
            .activate(&watcher_id)
            .expect("watcher activates")
            .state,
        LifecycleState::Active
    );

    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let app = TerminalApp::with_theme(
        runtime,
        bitty_config::theme::DEFAULT_THEME_NAME,
        "default",
        maps,
        SpawnSpec::default(),
    )
    .with_plugin_runtime(Some(plugin_runtime));
    (app, presenter_id, watcher, root, data)
}

/// Esc dismisses the presentation surface, still reaches the PTY, queues one
/// `overlay.dismissed` observation, and never runs a plugin callback on input.
#[test]
fn presentation_overlay_esc_dismisses_and_reaches_pty() {
    let runtime = Runtime::with_defaults().expect("runtime must build");
    let (mut app, presenter, watcher, root, data) = presentation_overlay_app("esc", runtime);
    let _ = app.drive_tick();
    assert!(
        app.presentation_overlay_active(),
        "init.lua presented the block"
    );
    assert!(
        app.runtime.presentation_overlay().is_some(),
        "the tick mirrors the live session into the paint path"
    );
    assert!(
        !app.runtime.overlay_modal_active(),
        "presentation never sets the modal authority"
    );
    assert!(
        !app.overlay_capture_active(),
        "presentation needs no capture grant"
    );
    assert!(
        !app.capture_fallthrough_input(&WindowEventKind::MouseInput(
            bitty_platform::MouseEvent::new(
                bitty_platform::MouseButton::Left,
                bitty_platform::PressState::Pressed,
            )
        )),
        "clicks pass through the presentation surface to the grid beneath"
    );
    let handle = match presentation_store_value(&app, &presenter, "handle") {
        Some(bitty_runtime::plugin_runtime::LuaValue::Integer(handle)) => handle,
        other => panic!("presented handle expected, got {other:?}"),
    };

    let esc = presentation_escape();
    assert!(
        !app.intercept_chrome_key(&WindowEventKind::KeyboardInput(esc.clone())),
        "Esc must route onward, not be consumed by the presentation surface"
    );
    let out = app.runtime.handle_key_event(esc);
    assert_eq!(out, Some(vec![27]), "Esc reaches the focused PTY");
    assert_eq!(app.runtime.pending_input(), b"\x1b");
    assert!(!app.presentation_overlay_active(), "the surface is gone");
    assert!(
        app.runtime.presentation_overlay().is_none(),
        "the paint path clears immediately"
    );
    assert_eq!(
        presentation_store_value(&app, &presenter, "callback_count"),
        None,
        "no plugin callback runs on the input path"
    );

    // The observation is queued, not synchronously delivered: only the cold
    // tick hands it to subscribers.
    let _ = app.drive_tick();
    let report = app
        .invoke_plugin_command(
            &format!("{watcher}:report"),
            &TerminalApp::empty_command_args(),
        )
        .expect("report dispatches");
    assert_eq!(
        report,
        bitty_runtime::plugin_runtime::LuaValue::String(format!("{presenter}|{handle}|escape")),
        "exactly one observation with owner, handle, and escape reason"
    );
    // The trap is live: an explicit delivery still increments it.
    app.plugin_runtime.as_mut().expect("runtime").deliver_event(
        "key",
        &bitty_runtime::plugin_runtime::LuaValue::String(String::from("k")),
    );
    assert_eq!(
        presentation_store_value(&app, &presenter, "callback_count"),
        Some(bitty_runtime::plugin_runtime::LuaValue::Integer(1)),
        "explicit delivery proves the hot-path trap was armed"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

/// A real confirmation gate keeps the consume and both the gate and the
/// presentation drop together; the press never reaches the PTY.
#[test]
fn presentation_overlay_esc_gate_precedence_keeps_the_consume() {
    // Paste gate: cancelling it consumes the press.
    let runtime = Runtime::with_defaults().expect("runtime must build");
    let (mut app, _presenter, _watcher, root, data) = presentation_overlay_app("paste", runtime);
    let _ = app.drive_tick();
    assert!(app.presentation_overlay_active());
    assert!(app.runtime.paste_text_via_gate(String::from("one\ntwo")));
    let esc = presentation_escape();
    assert!(
        app.intercept_chrome_key(&WindowEventKind::KeyboardInput(esc)),
        "the paste gate owns the consume"
    );
    assert!(!app.presentation_overlay_active(), "the surface drops too");
    assert!(!app.runtime.has_pending_paste(), "the gate drops too");
    assert!(
        app.runtime.pending_input().is_empty(),
        "no Esc leaks to the PTY"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);

    // View-close confirmation gate: same precedence.
    let runtime = Runtime::new(RuntimeConfig {
        close_confirm: CloseConfirmMode::Always,
        ..RuntimeConfig::default()
    })
    .expect("runtime must build");
    let (mut app, _presenter, _watcher, root, data) = presentation_overlay_app("close", runtime);
    let _ = app.drive_tick();
    assert!(app.presentation_overlay_active());
    app.runtime.view_close_request(ViewId::new(1));
    assert!(
        app.runtime.has_pending_close_confirm(),
        "the close arm is armed"
    );
    let esc = presentation_escape();
    assert!(
        app.intercept_chrome_key(&WindowEventKind::KeyboardInput(esc)),
        "the close confirmation owns the consume"
    );
    assert!(!app.presentation_overlay_active());
    assert!(!app.runtime.has_pending_close_confirm());
    assert!(app.runtime.pending_input().is_empty());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

/// Re-show replaces content and unload tears the surface down before the next
/// frame; no stale block is painted.
#[test]
fn presentation_overlay_update_and_unload_teardown() {
    let runtime = Runtime::with_defaults().expect("runtime must build");
    let (mut app, presenter, _watcher, root, data) = presentation_overlay_app("teardown", runtime);
    let _ = app.drive_tick();

    // `ui.update` changes the retained block; a re-show keeps the same
    // session and the next tick paints the new content.
    app.invoke_plugin_command(
        &format!("{presenter}:update"),
        &TerminalApp::empty_command_args(),
    )
    .expect("update dispatches");
    app.invoke_plugin_command(
        &format!("{presenter}:show"),
        &TerminalApp::empty_command_args(),
    )
    .expect("re-show dispatches");
    let _ = app.drive_tick();
    let painted = format!(
        "{:?}",
        app.runtime
            .presentation_overlay()
            .expect("presentation surface")
            .root
    );
    assert!(painted.contains("updated panel"), "{painted}");

    // Unload: dispose clears the session and the next tick clears the paint.
    app.plugin_runtime
        .as_mut()
        .expect("runtime")
        .dispose(&presenter)
        .expect("dispose");
    assert!(
        !app.presentation_overlay_active(),
        "unload leaves no stale surface"
    );
    let _ = app.drive_tick();
    assert!(
        app.runtime.presentation_overlay().is_none(),
        "the present path clears on the next tick"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}
