//! Window/platform event handler for the composition root (`TerminalApp`).

use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;

use bitty_platform::{
    AppHandler, EventContext, EventWaker, LogicalKey, LogicalSize, MouseButton, NamedKey,
    PhysicalSize, PlatformEvent, PressState, WindowConfig, WindowEventKind, WindowHandle, WindowId,
};
use bitty_render::gpu::GpuContext;
use bitty_runtime::plugin_runtime::{LuaValue, PluginRuntime, WorkspaceRequest};
use bitty_runtime::{Runtime, WorkspaceSummary};

use crate::ctl;
use crate::layout_cmd::spawn_demo_pty_pump_with_theme;
use crate::logging::LogLevel;
use crate::spawn::SpawnSpec;

/// Window title carrying the resolved theme preset and its source layer.
///
/// Visible via `hyprctl clients` and (where decorations show) the title bar,
/// so screenshots plus class-check prove which config path the window took:
/// `... — bitty-dark (default)` vs `... — bitty-dark (file)`.
pub(crate) fn window_title_for_theme(theme_name: &str, source: &str) -> String {
    format!("bitty \u{2014} Correct Terminal \u{2014} {theme_name} ({source})")
}

/// Maximum number of characters kept from a terminal-reported window title
/// (CTX-0382).
///
/// The parser already bounds the OSC payload at 4 KiB; a titlebar needs far
/// less, so the app clips here before the OS ever sees the string.
pub(crate) const WINDOW_TITLE_MAX_CHARS: usize = 256;

/// One-shot latch bounding the "UI block in an unhosted slot" diagnostic
/// (CTX-0923) to a single line per process.
static UNPLACED_UI_BLOCK_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Strips control characters from a terminal-reported title (CTX-0382) and
/// bounds it to [`WINDOW_TITLE_MAX_CHARS`].
///
/// Untrusted PTY output must never inject escape sequences or control
/// characters into the OS titlebar: every control scalar (C0, DEL, C1 —
/// including ESC and the C1 CSI/ST bytes) is dropped. The remaining text is
/// truncated on a character boundary, so the result is always a safe,
/// printable prefix.
pub(crate) fn sanitize_window_title(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(WINDOW_TITLE_MAX_CHARS)
        .collect()
}

/// Bounded label for a captured key without produced text (CTX-0943).
///
/// Mirrors the runtime inspect label shape (`key:<name>`, text preferred):
/// the capture queue already bounds kind/text, so this only keeps the label
/// short and printable. Field-level key encodings stay parked with the
/// input-pointer contract owner.
pub(crate) fn overlay_key_text(key: &bitty_platform::KeyEvent) -> String {
    match &key.logical_key {
        LogicalKey::Character(s) => {
            let short: String = s.chars().take(8).collect();
            format!("key:{short}")
        }
        LogicalKey::Named(named) => format!("key:{named:?}"),
        _ => String::from("key:Unidentified"),
    }
}

// ---------------------------------------------------------------------------
// App handler
// ---------------------------------------------------------------------------

/// OS-window title handoff boundary (CTX-0570).
///
/// `WindowHandle::set_title` needs a live winit window, so the OS titlebar
/// itself cannot be asserted headlessly. Routing every title through this one
/// seam means the call sequence a test double records is exactly the
/// production call sequence: production installs the [`WindowHandle`] wrapper
/// (done when the window is created), and tests install a recording double
/// instead. There is no separate test-only branch in `apply_window_title`.
pub(crate) trait OsTitleSink {
    /// Hands a sanitized, bounded title to the OS window.
    fn set_os_title(&self, title: &str);
}

impl OsTitleSink for WindowHandle {
    fn set_os_title(&self, title: &str) {
        WindowHandle::set_title(self, title);
    }
}

/// OS-window bookkeeping coalesced out of `TerminalApp` (CTX-0481 state
/// slimming): the handle, its identity, the resolved static title, opacity,
/// and the IME/title synchronization counters all move together and share
/// one lifetime.
pub(crate) struct WindowState {
    /// Window title carrying the resolved theme preset + source layer.
    pub(crate) title: String,
    /// Source-layer label baked into [`Self::title`] at launch
    /// (`cli`/`file`/`profile`/`default`), kept so a live theme reload can
    /// rebuild the title with the new preset name (CTX-0898).
    pub(crate) theme_source: String,
    /// Window opacity from the effective config (CTX-0223
    /// `window.opacity`; default `1.0` = opaque). Applied to the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) at creation and to the
    /// renderer at GPU attach (CTX-0290): the platform requests compositor
    /// blending where supported, and the renderer scales its premultiplied
    /// output so the value has a visible effect. Platforms without
    /// premultiplied compositing stay opaque with a loud warning.
    pub(crate) opacity: f32,
    /// Background blur radius from the effective config (CTX-0832
    /// `window.blur_radius`; default `0` = no blur). Applied to the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) at creation.
    /// Platform-specific: supported on macOS, some Wayland compositors,
    /// and Windows 10+. Ignored where unsupported.
    pub(crate) blur_radius: u32,
    pub(crate) handle: Option<WindowHandle>,
    pub(crate) id: Option<WindowId>,
    /// Physical-pixel caret rect last pushed to the platform IME via
    /// `WindowHandle::set_ime_cursor_area` (CTX-0367). Change detection
    /// keeps the sync to one call per actual caret move instead of one per
    /// frame.
    pub(crate) ime_cursor_area: Option<bitty_runtime::ImeCursorArea>,
    /// Last OS window title applied from `ColdEvent::TitleChanged`
    /// (CTX-0382). `None` until the first OSC 0/2 arrives; the change gate
    /// keeps identical titles from churning the titlebar.
    pub(crate) last_applied_title: Option<String>,
    /// Count of sanitized title applications (CTX-0382 diagnostics): stays
    /// at one per distinct title, proving no per-frame churn.
    pub(crate) title_applies: u64,
    /// Title-handoff boundary (CTX-0570). `None` headlessly (no window), in
    /// which case application still records state but performs no OS call;
    /// production sets this when the window handle is created, and tests
    /// install a recording double through `TerminalApp::set_os_title_sink`.
    pub(crate) os_title_sink: Option<Box<dyn OsTitleSink>>,
}

impl WindowState {
    fn new(theme_name: &str, source: &str) -> Self {
        Self {
            title: window_title_for_theme(theme_name, source),
            theme_source: source.to_string(),
            opacity: 1.0,
            blur_radius: 0,
            handle: None,
            id: None,
            ime_cursor_area: None,
            last_applied_title: None,
            title_applies: 0,
            os_title_sink: None,
        }
    }
}

/// Last runtime state observed by plugin event delivery (CTX-0892).
///
/// Delivery is coalesced: at most one event per kind fires per tick, carrying
/// the latest value, so the per-tick bound is structural (one per tracked
/// kind) and a burst of state changes cannot queue work for the VMs.
///
/// CTX-0889: workspace events are coalesced per tick as a diff of the
/// previous and current Core workspace summaries, so each kind fires at most
/// once per workspace per tick (bounded by `MAX_WORKSPACES`), and an
/// intermediate state that a tick never committed is never reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventTracker {
    title: String,
    window_focused: bool,
    workspaces: Vec<WorkspaceSummary>,
}

/// Stable-id payload for a workspace event (identity only, no content).
fn workspace_id_value(seq: u64) -> LuaValue {
    LuaValue::Integer(i64::try_from(seq).unwrap_or(i64::MAX))
}

/// Coalesced workspace events between two committed summaries (CTX-0889).
///
/// Order: `closed`, `created`, `renamed`, `changed`, `focused`, each in
/// workspace order. Payloads are identity-only: `{ id }`, plus `name` for
/// `created`/`renamed`. `changed` means the panel list of a surviving
/// workspace changed (count or order) or the window scratchpad occupancy
/// flipped (CTX-0954: occupancy rides every row, so a put/take fires
/// `changed`; the bar re-lists for the count). Time O(w^2) over `w <= 16`
/// workspaces; allocates only when something changed.
fn workspace_changes(
    previous: &[WorkspaceSummary],
    current: &[WorkspaceSummary],
) -> Vec<(&'static str, LuaValue)> {
    let mut events = Vec::new();
    // Empty previous is the first snapshot: establish baseline, emit nothing.
    if previous.is_empty() || previous == current {
        return events;
    }
    let find = |rows: &[WorkspaceSummary], seq: u64| rows.iter().position(|row| row.seq == seq);
    for old in previous {
        if find(current, old.seq).is_none() {
            events.push((
                "workspace.closed",
                LuaValue::table([("id", workspace_id_value(old.seq))]),
            ));
        }
    }
    for new in current {
        if find(previous, new.seq).is_none() {
            events.push((
                "workspace.created",
                LuaValue::table([
                    ("id", workspace_id_value(new.seq)),
                    ("name", LuaValue::String(new.name.clone())),
                ]),
            ));
        }
    }
    for new in current {
        if let Some(old) = find(previous, new.seq).map(|index| &previous[index]) {
            if old.name != new.name {
                events.push((
                    "workspace.renamed",
                    LuaValue::table([
                        ("id", workspace_id_value(new.seq)),
                        ("name", LuaValue::String(new.name.clone())),
                    ]),
                ));
            }
        }
    }
    for new in current {
        if let Some(old) = find(previous, new.seq).map(|index| &previous[index]) {
            if old.panel_ids != new.panel_ids
                || old.scratchpad_count != new.scratchpad_count
                || old.scratchpad_occupied != new.scratchpad_occupied
            {
                events.push((
                    "workspace.changed",
                    LuaValue::table([("id", workspace_id_value(new.seq))]),
                ));
            }
        }
    }
    let active = |rows: &[WorkspaceSummary]| rows.iter().find(|row| row.active).map(|row| row.seq);
    if let Some(seq) = active(current) {
        if active(previous) != Some(seq) {
            events.push((
                "workspace.focused",
                LuaValue::table([("id", workspace_id_value(seq))]),
            ));
        }
    }
    events
}

impl EventTracker {
    fn from_runtime(runtime: &Runtime) -> Self {
        Self {
            title: runtime.state().title().to_string(),
            window_focused: runtime.is_window_focused(),
            workspaces: runtime.workspace_summaries(),
        }
    }

    /// Diffs the tracker against the current state, updates it, and returns
    /// the coalesced events to deliver in order. Allocates only on change.
    fn take_changes(
        &mut self,
        title: &str,
        window_focused: bool,
        workspaces: &[WorkspaceSummary],
    ) -> Vec<(&'static str, LuaValue)> {
        let mut events = workspace_changes(&self.workspaces, workspaces);
        if !events.is_empty() || self.workspaces.as_slice() != workspaces {
            self.workspaces = workspaces.to_vec();
        }
        if self.title != title {
            self.title = title.to_string();
            events.push((
                "terminal.title-changed",
                LuaValue::table([("title", LuaValue::String(self.title.clone()))]),
            ));
        }
        if self.window_focused != window_focused {
            self.window_focused = window_focused;
            events.push((
                "focus.changed",
                LuaValue::table([("focused", LuaValue::Bool(window_focused))]),
            ));
        }
        events
    }
}

/// `AppModifiers` lives in [`crate::chrome_keys`] (CTX-0233 pure move).
/// The Correct Terminal handler: owns `Runtime`, an optional window, and the
/// real PTY pump via `Runtime::poll_pty` (plus an opt-in synthetic demo pump
/// only when explicitly attached for debug/tests).
/// All business stays in `bitty-runtime`; this type only wires
/// `PlatformEvent` → `Runtime` and `tick` → present, with real `GpuContext`
/// attachment for the single-window vertical slice.
///
/// CTX-0481 state slimming: cohesive state lives in [`WindowState`] and
/// [`crate::chrome_keys::ChromeState`] so this type keeps only the core
/// runtime, pump, and gate fields.
pub(crate) struct TerminalApp {
    pub(crate) runtime: Runtime,
    /// Window handle, title, opacity, and IME/title counters.
    pub(crate) window: WindowState,
    /// Demo pump channel when explicitly attached for debug/tests
    /// (`None` in real sessions — CTX-0167).
    pub(crate) pty_rx: Option<Receiver<Vec<u8>>>,
    pub(crate) _pty_thread: Option<JoinHandle<()>>,
    /// Count of `tick` calls that presented a frame.
    pub(crate) presented_frames: u64,
    /// Chrome-owned key state (keymaps, modifier mirror, held keys, zoom).
    pub(crate) chrome: crate::chrome_keys::ChromeState,
    /// Frozen startup spawn recipe so `new_split` leaves replay the exact
    /// program/shell resolution (CTX-0176).
    pub(crate) spawn_spec: SpawnSpec,
    /// Stderr verbosity gate (CTX-0190). Default [`LogLevel::Warn`] (quiet):
    /// per-frame `bitty tick` lines require `Debug`/`Trace`. User-facing key
    /// info (paste confirm/cancel, startup summary) and warnings/errors
    /// bypass this gate and always emit.
    pub(crate) log_level: LogLevel,
    /// Session persistence gate (CTX-0393): exit paths save the session
    /// best-effort when true. Startup clears it for `--safe` (recovery must
    /// neither read nor clobber the saved session) and `--headless`
    /// (deterministic CI must not clobber it either). Default true so tests
    /// using [`Self::with_theme`] keep the production behavior.
    pub(crate) session_persistence: bool,
    /// Shared live plugin snapshot (CTX-0481): `drive_tick` commits the
    /// runtime's committed generation here so `bitty.terminal.snapshot`
    /// never serves the frozen generation-1 view. `None` when no plugin VM
    /// exists or a test does not exercise the bridge.
    pub(crate) live_snapshot: Option<std::rc::Rc<crate::plugin_runtime::LiveSnapshot>>,
    /// Shared live workspace source (CTX-0889): `drive_tick` publishes
    /// Core's workspace summaries here so `bitty.workspace.list()` serves
    /// committed state. `None` without a plugin runtime.
    pub(crate) live_workspaces: Option<std::rc::Rc<crate::plugin_runtime::LiveWorkspaces>>,
    /// Plugin runtime (CTX-0892): owns all plugin VMs, delivers events,
    /// dispatches commands. Kept alive by the app loop.
    pub(crate) plugin_runtime: Option<PluginRuntime>,
    /// Safe recovery latch (W-103 S-5, CTX-0929): mirrors the process
    /// `--safe` flag so the composer cutover (and any later plugin-owned
    /// UX) selects the retained Core path without consulting a VM.
    /// Production startup sets it from the CLI flag; tests set it
    /// explicitly. Default false (normal startup).
    pub(crate) safe_mode: bool,
    /// Dispatch-failure fallback latch (W-103 S-5, CTX-0929): set when an
    /// ACTIVE-plugin open dispatch fails and the retained Core composer
    /// opens instead. While latched AND the Core session is open, key
    /// routing serves the Core session even though the plugin still owns
    /// the UX (prevents a stranded visible-but-dead session). The latch
    /// clears whenever no Core session is open, so the next open retries
    /// the plugin. Default false.
    pub(crate) composer_core_fallback_latched: bool,
    /// Previous runtime state for event change detection (CTX-0892).
    /// Updated in place by [`EventTracker::take_changes`] each tick; changes
    /// trigger plugin events.
    event_tracker: EventTracker,
}

/// Outcome of polling exited child processes across pane and primary sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellExitOutcome {
    /// No shell child exited during this reap pass.
    NoExit,
    /// At least one split pane shell exited and its panel was closed.
    PaneClosed,
    /// The last remaining shell exited; the session must close.
    AppExiting,
}

impl TerminalApp {
    /// Theme-aware constructor for real sessions (CTX-0167).
    ///
    /// Never attaches the synthetic demo pump: `pty_rx` stays `None` so
    /// startup shows only the shell (and shell init output). The window
    /// title still carries the resolved preset + source layer, so the
    /// config path remains visible without polluting the grid.
    pub(crate) fn with_theme(
        runtime: Runtime,
        theme_name: &str,
        source: &str,
        keymaps: Vec<bitty_config::ResolvedKeymap>,
        spawn_spec: SpawnSpec,
    ) -> Self {
        let event_tracker = EventTracker::from_runtime(&runtime);
        Self {
            runtime,
            window: WindowState::new(theme_name, source),
            pty_rx: None,
            _pty_thread: None,
            presented_frames: 0,
            chrome: crate::chrome_keys::ChromeState::new(keymaps),
            spawn_spec,
            log_level: LogLevel::default_level(),
            session_persistence: true,
            live_snapshot: None,
            live_workspaces: None,
            plugin_runtime: None,
            safe_mode: false,
            composer_core_fallback_latched: false,
            event_tracker,
        }
    }

    /// Test constructor with the synthetic demo pump attached.
    ///
    /// Same as [`Self::with_theme`] plus a bounded `spawn_demo_pty_pump`
    /// burst naming `theme_name`/`source`. Tests that legitimately need
    /// synthetic bytes use this instead of `with_theme`; production uses
    /// [`Self::attach_demo_pump`] behind [`crate::layout_cmd::demo_pump_enabled_from_env`].
    #[cfg(test)]
    pub(crate) fn with_demo_pump(
        runtime: Runtime,
        theme_name: &str,
        source: &str,
        keymaps: Vec<bitty_config::ResolvedKeymap>,
        spawn_spec: SpawnSpec,
    ) -> Self {
        let (pty_rx, handle) = spawn_demo_pty_pump_with_theme(theme_name, source);
        let event_tracker = EventTracker::from_runtime(&runtime);
        Self {
            runtime,
            window: WindowState::new(theme_name, source),
            pty_rx: Some(pty_rx),
            _pty_thread: Some(handle),
            presented_frames: 0,
            chrome: crate::chrome_keys::ChromeState::new(keymaps),
            spawn_spec,
            log_level: LogLevel::default_level(),
            session_persistence: true,
            live_snapshot: None,
            live_workspaces: None,
            plugin_runtime: None,
            safe_mode: false,
            composer_core_fallback_latched: false,
            event_tracker,
        }
    }

    /// Attaches the synthetic demo pump to an existing app (CTX-0167).
    ///
    /// Debug escape hatch for the real startup path: called only when
    /// [`crate::layout_cmd::demo_pump_enabled_from_env`] is true (`BITTY_DEMO_PUMP=1`).
    /// No-op when a pump is already attached.
    pub(crate) fn attach_demo_pump(&mut self, theme_name: &str, source: &str) {
        if self.pty_rx.is_some() {
            return;
        }
        let (pty_rx, handle) = spawn_demo_pty_pump_with_theme(theme_name, source);
        self.pty_rx = Some(pty_rx);
        self._pty_thread = Some(handle);
    }

    /// Sets the stderr verbosity gate (CTX-0190). Call once at startup from
    /// [`crate::logging::effective_log_level`]; tests set it explicitly to prove gating.
    pub(crate) fn set_log_level(&mut self, level: LogLevel) {
        self.log_level = level;
    }

    /// Enables or disables session save-on-exit (CTX-0393). Production
    /// startup passes `!safe && !headless`: safe recovery and headless
    /// smoke runs must never overwrite the saved session.
    pub(crate) fn set_session_persistence(&mut self, enabled: bool) {
        self.session_persistence = enabled;
    }

    /// Best-effort session save for exit paths (CTX-0393): one bounded
    /// capture plus one atomic write, no retries. Loud one-line outcome on
    /// stderr (counts only, never session contents); failures never block
    /// shutdown. Raw `SIGTERM`/`SIGHUP` that bypasses the event loop skips
    /// this hook (documented gap); the atomic format still rules out a
    /// partial file.
    fn save_session_best_effort(&self, reason: &'static str) {
        if !self.session_persistence {
            return;
        }
        match self.runtime.save_session_on_exit() {
            bitty_runtime::SessionExitSaveOutcome::Saved(summary) => {
                crate::logging::info(|| {
                    format!(
                        "bitty: session saved ({reason}: workspaces={} panes={} lines={} bytes={})",
                        summary.workspaces, summary.panes, summary.scrollback_lines, summary.bytes
                    )
                });
            }
            bitty_runtime::SessionExitSaveOutcome::Warned(err) => {
                crate::logging::warn(|| {
                    format!("bitty: session save failed ({err}) — exiting anyway")
                });
            }
            bitty_runtime::SessionExitSaveOutcome::SkippedNoStateDir => {}
        }
    }

    /// Sets the window opacity applied at creation and GPU attach
    /// (CTX-0223/CTX-0290). Call once at startup from the effective config;
    /// the value is sanitized by the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) and the renderer
    /// surface, so out-of-range inputs degrade instead of failing creation.
    pub(crate) fn with_window_opacity(mut self, opacity: f32) -> Self {
        self.window.opacity = opacity;
        self
    }

    /// Sets the background blur radius applied at window creation (CTX-0832).
    /// Call once at startup from the effective config; platform-specific
    /// support varies (macOS, some Wayland compositors, Windows 10+).
    /// Unsupported platforms silently ignore the request.
    pub(crate) fn with_blur_radius(mut self, blur_radius: u32) -> Self {
        self.window.blur_radius = blur_radius;
        self
    }

    /// Attaches the shared live plugin snapshot (CTX-0481). Production
    /// startup passes the handle returned by
    /// [`crate::plugin_runtime::discover_and_activate`] so
    /// `bitty.terminal.snapshot` tracks committed state; tests and
    /// plugin-less runs leave it `None`.
    pub(crate) fn with_live_snapshot(
        mut self,
        snapshot: Option<std::rc::Rc<crate::plugin_runtime::LiveSnapshot>>,
    ) -> Self {
        self.live_snapshot = snapshot;
        self
    }

    /// Attaches the shared live workspace source (CTX-0889). Production
    /// startup passes the handle returned by
    /// [`crate::plugin_runtime::discover_and_activate`]; plugin-less runs
    /// leave it `None`.
    pub(crate) fn with_live_workspaces(
        mut self,
        workspaces: Option<std::rc::Rc<crate::plugin_runtime::LiveWorkspaces>>,
    ) -> Self {
        self.live_workspaces = workspaces;
        self
    }

    /// Attaches the plugin runtime (CTX-0892). Production startup passes
    /// the runtime returned by [`crate::plugin_runtime::discover_and_activate`];
    /// tests and plugin-less runs leave it `None`.
    pub(crate) fn with_plugin_runtime(mut self, runtime: Option<PluginRuntime>) -> Self {
        self.plugin_runtime = runtime;
        self
    }

    /// Sets the safe recovery latch (W-103 S-5, CTX-0929). Production
    /// startup passes the `--safe` flag; the composer cutover reads it to
    /// select the retained Core path without consulting a VM.
    pub(crate) fn with_safe_mode(mut self, safe_mode: bool) -> Self {
        self.safe_mode = safe_mode;
        self
    }

    /// Composer ownership for this input path (W-103 S-5 cutover rule).
    ///
    /// The ACTIVE composer plugin owns the editing UX via
    /// overlay/capture/submit/editor; every other state (safe mode,
    /// zero-plugin startup, uninstalled, not activated, version or
    /// capability mismatch) keeps the retained Core edit/submit path.
    pub(crate) fn composer_owner(&self) -> crate::composer_owner::ComposerOwner {
        crate::composer_owner::decide_composer_owner(self.safe_mode, self.plugin_runtime.as_ref())
    }

    /// True when the ACTIVE composer plugin owns the editing UX.
    pub(crate) fn composer_plugin_owns(&self) -> bool {
        self.composer_owner().plugin_owns()
    }

    /// Opens the retained Core composer session, latching fallback routing
    /// when the plugin owns the UX (W-103 S-5, CTX-0929): the open session
    /// must stay served even though ownership stays plugin, or it strands
    /// visible-but-dead. Every retained-Core open under plugin ownership
    /// (dispatch-error fallback, editor-exit reopen, vanished-leaf reopen)
    /// routes through here. In retained mode the latch is a harmless no-op
    /// (the guard serves retained sessions regardless).
    pub(crate) fn open_retained_composer(&mut self) {
        self.runtime.cw_composer_open();
        if self.composer_plugin_owns() {
            self.composer_core_fallback_latched = true;
        }
    }

    /// Dispatches one composer session verb to the ACTIVE plugin
    /// (`<id>:<verb>` via the plugin runtime).
    ///
    /// Fails closed with a diagnostic when the plugin does not own the
    /// editing UX or the dispatch itself fails; the caller falls back to
    /// the retained Core path.
    pub(crate) fn dispatch_composer_command(&mut self, verb: &str) -> Result<(), String> {
        use crate::composer_owner::{COMPOSER_KNOWN_VERBS, COMPOSER_PLUGIN_ID, ComposerOwner};
        if !COMPOSER_KNOWN_VERBS.contains(&verb) {
            return Err(format!(
                "composer verb '{verb}' is not a known composer command"
            ));
        }
        if !self.composer_plugin_owns() {
            let ComposerOwner::RetainedCore(reason) = self.composer_owner() else {
                unreachable!("plugin_owns false implies retained");
            };
            return Err(reason.diagnostic());
        }
        let id = bitty_plugin_host::manifest::PluginId::new(COMPOSER_PLUGIN_ID)
            .map_err(|error| format!("composer plugin id invalid: {error}"))?;
        let Some(runtime) = self.plugin_runtime.as_mut() else {
            return Err(String::from("composer plugin runtime is gone"));
        };
        runtime
            .dispatch_command(&id, verb, &[])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Injects the effective-config Leader binding (CTX-0723 #981).
    ///
    /// Resolved once at startup from `leader_key` / `leader_timeout_ms`
    /// via [`bitty_config::resolve_leader_for`]; the input path arms the
    /// hint session on these chords.
    pub(crate) fn with_leader(mut self, leader: bitty_config::ResolvedLeader) -> Self {
        self.chrome = self.chrome.with_leader(leader);
        self
    }

    /// Injects the effective-config hint kill switch (CTX-0735 #981).
    ///
    /// Resolved once at startup from `hints_enabled` via
    /// [`bitty_config::resolve_hint_config`]; while disabled the Leader
    /// never arms a hint session.
    pub(crate) fn with_hints_enabled(mut self, enabled: bool) -> Self {
        self.chrome = self.chrome.with_hints_enabled(enabled);
        self
    }

    /// Injects the effective-config tiled resize step (CTX-0963 #1697).
    ///
    /// Resolved once at startup from `layout.resize_step`; adopted live on
    /// reload via [`Self::adopt_live_config`].
    pub(crate) fn with_resize_step(mut self, step: f32) -> Self {
        self.chrome = self.chrome.with_resize_step(step);
        self
    }

    /// Adopts the app-owned half of an accepted live reload (CTX-0898, #1522).
    ///
    /// Swaps the resolved keymap table (`keymaps` + `mod_key`), the Leader
    /// binding (`leader_key` + `leader_timeout_ms`), the hint kill switch
    /// (`hints_enabled`), the tiled resize step (`layout.resize_step`,
    /// CTX-0963), and re-applies the platform transparency hint for
    /// `window.opacity`. A Leader window or hint session armed under the old
    /// binding is cancelled when the binding changes or hints are disabled,
    /// so no stale chord or armed window outlives the reload. Held-key
    /// ownership is kept: a press consumed under the old table still owns its
    /// release (CTX-0229).
    pub(crate) fn adopt_live_config(&mut self, adoption: crate::config_reload::AppAdoption) {
        let leader_changed = self.chrome.leader != adoption.leader;
        let keymaps_changed = self.chrome.keymaps != adoption.keymaps;
        let hints_disabled = self.chrome.hints_enabled && !adoption.hints_enabled;
        self.chrome.keymaps = adoption.keymaps;
        self.chrome.leader = adoption.leader;
        self.chrome.hints_enabled = adoption.hints_enabled;
        self.chrome.resize_step = crate::chrome_keys::sanitize_resize_step(adoption.resize_step);
        // An armed Leader window or hint session was opened under the old
        // binding/table; its follow-up chord could now mean something else,
        // so cancel it (fail-open: keys route normally again).
        if leader_changed || keymaps_changed || hints_disabled {
            self.chrome.leader_state = bitty_config::LeaderState::Idle;
            self.runtime.cw_hint_disarm();
        }
        if (bitty_platform::sanitize_opacity(adoption.window_opacity)
            - bitty_platform::sanitize_opacity(self.window.opacity))
        .abs()
            >= f32::EPSILON
        {
            self.window.opacity = adoption.window_opacity;
            if let Some(handle) = self.window.handle.as_ref() {
                let _ = handle.set_opacity(adoption.window_opacity);
                handle.request_redraw();
            }
        }
        self.refresh_theme_title(adoption.theme_name);
    }

    /// Rebuilds the base window title for a live theme change (CTX-0898).
    ///
    /// The base title is the fallback shown while no terminal-reported
    /// (OSC 0/2) title is active; once an application has set its own title
    /// that title stays on the OS window and only the fallback updates.
    fn refresh_theme_title(&mut self, theme_name: &str) {
        let title = window_title_for_theme(theme_name, &self.window.theme_source);
        if title == self.window.title {
            return;
        }
        let previous = std::mem::replace(&mut self.window.title, title);
        let showing_base = match self.window.last_applied_title.as_deref() {
            None => true,
            Some(applied) => applied == previous,
        };
        if showing_base {
            if self.window.last_applied_title.is_some() {
                self.window.last_applied_title = Some(self.window.title.clone());
            }
            if let Some(sink) = self.window.os_title_sink.as_ref() {
                sink.set_os_title(&self.window.title);
            }
        }
    }

    /// True when per-frame `bitty tick` stderr lines are emitted.
    ///
    /// Hot-path guard: a single comparison, checked before any formatting so
    /// the disabled path pays no allocation. Delegates to
    /// [`LogLevel::tick_enabled`]; the devtools trace path (`Runtime::tick`
    /// return + inspect snapshots) is unaffected and keeps full fidelity.
    pub(crate) fn tick_logging_enabled(&self) -> bool {
        self.log_level.tick_enabled()
    }

    /// Pure tick-line renderer for tests (CTX-0190).
    ///
    /// Returns the exact `bitty tick: ...` line `drive_tick` emits when
    /// [`Self::tick_logging_enabled`] is true. Pure over its inputs so
    /// level-gating tests assert content without capturing stderr.
    pub(crate) fn format_tick_line(
        present: &bitty_runtime::PresentStats,
        presented_frames: u64,
        focused: Option<bitty_runtime::ViewId>,
        leafs: usize,
        gpu: bool,
        crossfont: bool,
    ) -> String {
        format!(
            "bitty tick: frame={} fills={} glyphs={} headless={} gen={} presented_frames={} focused={:?} leafs={} gpu={} crossfont={} images={} images_skipped={}",
            present.frame,
            present.fills,
            present.glyphs,
            present.headless,
            present.generation,
            presented_frames,
            focused,
            leafs,
            gpu,
            crossfont,
            present.images,
            present.images_skipped
        )
    }

    /// Returns the tick line when logging is enabled, else `None` (CTX-0190).
    ///
    /// `None` means the caller must not touch stderr: this is the bounded,
    /// no-format hot path for the default quiet run.
    pub(crate) fn maybe_format_tick(
        &self,
        present: &bitty_runtime::PresentStats,
    ) -> Option<String> {
        if !self.tick_logging_enabled() {
            return None;
        }
        Some(Self::format_tick_line(
            present,
            self.presented_frames,
            self.runtime.focused_view(),
            self.runtime.leaf_count(),
            self.runtime.has_gpu(),
            self.runtime.is_crossfont(),
        ))
    }

    /// Polls real PTY (`Runtime::poll_pty` bounded 128 KiB) plus the opt-in
    /// demo pump when attached (tests / `BITTY_DEMO_PUMP=1` only).
    /// Returns true when bytes were consumed.
    pub(crate) fn poll_pty_pump(&mut self) -> bool {
        let mut consumed = false;
        // Real PTY first: drain bounded channel via runtime; replies are flushed
        // via `Runtime::write_replies` inside `poll_pty` (bounded 4 KiB, fail-closed).
        let real = self.runtime.poll_pty();
        if real > 0 {
            consumed = true;
        }
        // Opt-in demo pump (bounded, debug/tests only — `None` in real
        // sessions so startup shows only the shell).
        if let Some(rx) = self.pty_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(chunk) => {
                        self.runtime.handle_pty_bytes(&chunk);
                        consumed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        // Flush any replies generated by PTY bytes (bounded, best-effort).
        // `write_replies` is no-op when no live writer (headless keeps replies for `take_replies`).
        let _ = self.runtime.write_replies();
        // CTX-0731 (#982): advance a hosted `$EDITOR` round trip, if any
        // (non-blocking exit poll; no-op when nothing is hosted).
        self.poll_external_editor();
        consumed
    }

    /// Drives one frame when damage exists, printing stats when a frame was
    /// presented. Returns the stats when a present occurred. Handles GPU vs headless.
    ///
    /// CTX-0190 quiet default: the per-frame `bitty tick` line (plus the
    /// per-tick reply/cold-queue diagnostics) emits only when
    /// [`Self::tick_logging_enabled`] (i.e. `--verbose` / `--log-level
    /// debug|trace`); the format is guarded so the quiet path pays no
    /// formatting cost. The reply-overflow warning stays unconditional
    /// (warn level), and paste/startup/user-facing lines elsewhere bypass
    /// the gate entirely. `Runtime::tick` itself is untouched so devtools
    /// keeps full fidelity.
    /// Pushes the runtime's focused caret rect to the platform IME
    /// (CTX-0367).
    ///
    /// No-op without a window (headless CI) or when the rect is unchanged
    /// since the last push. `None` (cursor hidden/unfocused) leaves the last
    /// platform rect in place: winit exposes no clear call, and the OS
    /// hides the candidate window on focus loss by itself.
    pub(crate) fn sync_ime_cursor_area(&mut self) {
        let Some(window) = self.window.handle.as_ref() else {
            return;
        };
        let area = self.runtime.ime_cursor_area();
        if area == self.window.ime_cursor_area {
            return;
        }
        self.window.ime_cursor_area = area;
        if let Some(area) = area {
            window.set_ime_cursor_area(area.x, area.y, area.width, area.height);
        }
    }

    pub(crate) fn drive_tick(&mut self) -> Option<bitty_runtime::PresentStats> {
        // CTX-0171: drain IPC runtime-control queue before present so
        // `bitty ctl` mutations (send/split/focus/close/spawn/reload) apply
        // on the main thread — the sole `Runtime` owner — with server-side
        // scope enforcement (never ambient authority).
        // CTX-0481 (#762): a layout-mutating ctl verb must never land on the
        // single-leaf zoom proxy — the pre-mutation hook restores the real
        // tree first (with the staleness/generation check inside
        // `ZoomState`), so the eventual zoom restore cannot drop the pane
        // the verb created.
        // CTX-0792 (#1403): the empty fallback means only entries carrying a
        // connection authorization snapshot can apply; the drain re-validates
        // each snapshot against the live connection authority.
        let _ = ctl::drain_global_control_queue_with(
            &mut self.runtime,
            &bitty_ipc::ScopeSet::new(),
            |runtime, method| {
                if ctl::method_mutates_layout(method) {
                    self.chrome.zoom.restore_for_mutation(runtime);
                }
            },
        );
        // CTX-0814 (#1397): adopt a live config change that landed on disk
        // before the tick commits, so the presented frame reflects the new
        // presentation values. The watcher is a per-tick poll; without an
        // installed context this is a no-op.
        let _ = crate::config_reload::poll_file(&mut self.runtime);
        // CTX-0898 (#1522): either reload path (ctl verb drained above or the
        // file poll) may have accepted chrome-owned fields; adopt them here
        // on the same tick so keys typed after this frame use the new table.
        if let Some(adoption) = crate::config_reload::take_app_adoption() {
            self.adopt_live_config(adoption);
        }
        // CTX-0889: apply plugin workspace mutations queued since the last
        // tick before it commits, so the presented frame reflects them.
        self.apply_plugin_workspace_requests();
        // CTX-0911 (issue #1570): read mounted UiBlocks from plugin runtime,
        // convert to ChromeBands, and push into Runtime before present so
        // bands render on this frame.
        self.update_chrome_bands();
        // CTX-0943 (W-28 follow-up): enforce the 30s transient timeout, then
        // sync the overlay surface to the (possibly revoked) owner before
        // present so a timed-out session clears on this frame. Both are
        // no-ops without a plugin runtime or an active capture.
        self.expire_overlay_captures();
        // CTX-0941 (accepted W-01): deliver pending `overlay.released`
        // observations on the cold tick path so session end is observable
        // without polling.
        self.deliver_overlay_released();
        self.update_plugin_overlay();
        // CTX-0382: drain cold-path events on every tick — including
        // deferred (synchronized update) and idle ticks — because a title
        // change produces no grid damage and would otherwise sit in the
        // bounded queue until unrelated output arrived. Title application
        // is sanitized and change-gated; the other events stay telemetry.
        self.apply_cold_events();
        // Ensure replies that were queued before tick are flushed before present:
        // the runtime's tick consumes snapshot+damage and composites.
        let stats = self.runtime.tick();
        // CTX-0481 (#762): after the tick commits, publish the live plugin
        // snapshot so plugins observe the committed generation instead of a
        // frozen generation-1 view. Monotonic: a regression is refused by
        // the source itself.
        if let Some(snapshot) = self.live_snapshot.as_ref() {
            snapshot.publish(&self.runtime);
        }
        // CTX-0367: the presented frame refreshed the focused caret; forward
        // it to the platform so the OS IME preedit/candidate window tracks
        // the terminal cursor (DPI-correct physical pixels, change-gated).
        self.sync_ime_cursor_area();
        if let Some(present) = stats {
            self.presented_frames += 1;
            // CTX-0592: emit the real-window first-frame marker exactly once
            // when the opt-in perf harness asked for it. The harness starts
            // its timer before spawning this process and reads the elapsed
            // time to this line as PB-1 launch-to-first-frame. Gated on the
            // env flag so normal sessions and CI emit nothing, and on
            // `!present.headless` so the headless fallback path (no window or
            // no GPU) never fabricates a real-window first frame.
            if self.presented_frames == 1
                && !present.headless
                && std::env::var_os("BITTY_PERF_STARTUP_MARKER").is_some()
            {
                println!("bitty perf: first-frame");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            if let Some(line) = self.maybe_format_tick(&present) {
                eprintln!("{line}");
            }
            if self.runtime.replies_overflowed() {
                crate::logging::warn(|| {
                    String::from("warning: terminal reply queue overflowed (bounded cap)")
                });
            }
            // Bounded reply loop: flush replies generated before this tick (if any) via PtyWriter.
            // When no writer is present (headless), replies stay queued for `take_replies` observation.
            let written = self.runtime.write_replies();
            if written > 0 && self.tick_logging_enabled() {
                eprintln!("bitty: {written} reply bytes written to PTY master (post-tick)");
            }
            // CTX-0230: same post-tick flush for every pane session, so a
            // split shell's query answers never wait for the next pump.
            // Bounded per pane (reply cap, fail-closed); no-op when quiet.
            let mut pane_written = 0usize;
            for id in self.runtime.pane_session_ids() {
                pane_written += self.runtime.write_pane_replies(id);
            }
            if pane_written > 0 && self.tick_logging_enabled() {
                eprintln!(
                    "bitty: {pane_written} pane reply bytes written to PTY masters (post-tick)"
                );
            }
        }
        // CTX-0892: deliver coalesced runtime events to plugin VMs last, after
        // present, IME sync, and the PTY reply flush, so a slow Lua handler
        // never delays terminal replies or the frame.
        self.deliver_runtime_events();
        stats
    }

    /// Drains cold-path events and applies the ones the app owns (CTX-0382).
    ///
    /// Bounded: one pass over at most the runtime's cold-queue capacity.
    /// Only `TitleChanged` has an app-side effect; the rest stays plugin
    /// telemetry (already bridged by the runtime).
    pub(crate) fn apply_cold_events(&mut self) {
        let events = self.runtime.drain_cold_events();
        if events.is_empty() {
            return;
        }
        if self.tick_logging_enabled() {
            eprintln!("bitty cold-queue: drained {} events", events.len());
        }
        for event in events {
            if let bitty_runtime::ColdEvent::TitleChanged(raw) = event {
                self.apply_window_title(&raw);
            }
        }
    }

    /// Delivers runtime state-change events to plugin VMs (CTX-0892).
    ///
    /// Runs once per tick at the end of [`Self::drive_tick`]. Coalesced per kind
    /// (see [`EventTracker::take_changes`]); handler failures are contained
    /// by the plugin runtime and never reach the app. No-op without plugins.
    fn deliver_runtime_events(&mut self) {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return;
        };
        // CTX-0889: one bounded summary pass serves both the live workspace
        // source and the event diff.
        let workspaces = self.runtime.workspace_summaries();
        if let Some(live) = self.live_workspaces.as_ref() {
            live.publish(&workspaces);
        }
        let events = self.event_tracker.take_changes(
            self.runtime.state().title(),
            self.runtime.is_window_focused(),
            &workspaces,
        );
        for (kind, payload) in &events {
            let _delivered = plugin_runtime.deliver_event(kind, payload);
        }
    }

    /// Reads mounted UiBlocks from plugin runtime and updates Runtime chrome
    /// bands (CTX-0911, issue #1570).
    ///
    /// Called once per tick after plugin runtime tick, before present. Converts
    /// `PluginRuntime::ui_blocks()` into plain `ChromeBands` struct following
    /// the LiveSnapshot pattern: bitty-runtime must not depend on plugin_runtime
    /// types directly.
    fn update_chrome_bands(&mut self) {
        let Some(plugin_runtime) = self.plugin_runtime.as_ref() else {
            return;
        };
        let blocks = plugin_runtime.ui_blocks();
        // CTX-0923: slot routing and stacking live in one place
        // (`bitty_runtime::ui_slot_placement` via `ChromeBands::from_mounts`),
        // the same policy the `ui.mount` gate uses: `statusline` joins the
        // bottom band, `tabline`/`terminal` mounts are rejected at mount time
        // with `E_UI_UNAVAILABLE`, and the focusable `overlay` slot (CTX-0941)
        // is hosted by the overlay surface rather than this band renderer.
        let (bands, unplaced) =
            bitty_runtime::ChromeBands::from_mounts(blocks.into_iter().map(
                |(plugin_id, slot, node, version)| (plugin_id.to_string(), slot, node, version),
            ));
        if unplaced > 0
            && !UNPLACED_UI_BLOCK_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            // Unreachable while the mount gate holds; keep it observable but
            // bounded (one diagnostic per process, not one per tick).
            crate::logging::warn(|| {
                format!("bitty: {unplaced} mounted UI block(s) in an unhosted slot were not placed")
            });
        }
        self.runtime.set_chrome_bands(bands);
    }

    /// Focusable-overlay transient input-capture application wiring (CTX-0943,
    /// W-28 follow-up; lifecycle/timeout/release per the accepted W-01
    /// `overlay-input-capture-contract.md`).
    ///
    /// The mechanism (`push_overlay_input`, `expire_overlay_captures`,
    /// `revoke_overlay_capture` on `PluginRuntime`) landed in CTX-0941; this
    /// is the application side only: no new Lua namespace, no new capability
    /// identifier, no Event-Bus exposure, safe-mode behavior unchanged. Every
    /// helper is a no-op without a plugin runtime or an active capture, so
    /// frames and input without a capture are byte-identical to before.
    pub(crate) fn overlay_capture_active(&self) -> bool {
        self.plugin_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.overlay_capture().borrow().is_active())
    }

    /// Enqueue one captured input event for the active overlay capture.
    ///
    /// Core input-path entry: appends to the bounded queue, never invokes
    /// plugin code (`P0-AC-015`). Returns `false` with no capture active, so
    /// the caller falls through to normal routing.
    pub(crate) fn push_overlay_input(&mut self, kind: &str, text: &str) -> bool {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return false;
        };
        plugin_runtime.push_overlay_input(kind, text)
    }

    /// Enqueue a pointer-motion event with trailing-motion coalescing, so a
    /// mouse wiggle cannot evict queued key/text entries (CodeRabbit PR #1643).
    pub(crate) fn push_overlay_move(&mut self, text: &str) -> bool {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return false;
        };
        plugin_runtime.push_overlay_move(text)
    }

    /// Absolute monotonic deadline of the active overlay capture, if any.
    pub(crate) fn overlay_capture_deadline(&self) -> Option<std::time::Instant> {
        self.plugin_runtime
            .as_ref()
            .and_then(|runtime| runtime.overlay_capture_deadline())
    }

    /// Revoke the active overlay capture unconditionally (user focus-switch
    /// or cancel path). Idempotent no-op `false` with no active capture.
    /// Records the `focus_switched` reason for the owner's next poll.
    pub(crate) fn revoke_overlay_capture(&mut self) -> bool {
        self.revoke_overlay_capture_with_reason(
            bitty_runtime::plugin_runtime::FOCUS_SWITCHED_RELEASE_REASON,
        )
    }
    /// Revoke the active overlay capture with an explicit terminal reason
    /// (accepted W-01 vocabulary). The `cancelled` disposition is used for
    /// user cancel (bare `Esc`); focus moves use the default above.
    pub(crate) fn revoke_overlay_capture_with_reason(&mut self, reason: &str) -> bool {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return false;
        };
        plugin_runtime.revoke_overlay_capture_with_reason(reason)
    }

    /// Deliver pending `overlay.released` bus observations (CTX-0941).
    ///
    /// Called on the application tick after expiry handling: each ended
    /// session is observable without polling through one observation-only
    /// event with payload `{ owner, reason }`. Delivery runs on the cold
    /// tick path, never on the input hot path. No-op without a plugin
    /// runtime or with no ended sessions.
    fn deliver_overlay_released(&mut self) {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return;
        };
        let ended = plugin_runtime.drain_overlay_released();
        for event in ended {
            let payload = LuaValue::table([
                ("owner", LuaValue::String(event.owner)),
                ("reason", LuaValue::String(event.reason)),
            ]);
            plugin_runtime.deliver_event("overlay.released", &payload);
        }
    }

    /// Enforce the 30s transient timeout (contract idle timeout). No-op
    /// without a plugin runtime or with no expired capture.
    fn expire_overlay_captures(&mut self) -> bool {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return false;
        };
        plugin_runtime.expire_overlay_captures()
    }

    /// Whether a chrome action moves focus (CTX-0943).
    ///
    /// Pane/view/workspace focus moves end the transient capture per the
    /// contract focus-switch release (`focus_switched`); layout mutations
    /// (splits, closes, zoom, search, composer, palette) stay swallowed
    /// while the modal holds so no state mutates behind it. A broader
    /// taxonomy (panel focus, overlay-to-overlay) stays parked with its
    /// owner; this predicate covers the user focus chords that exist today.
    pub(crate) fn is_overlay_focus_switch(action: bitty_config::ChromeAction) -> bool {
        use bitty_config::ChromeAction as A;
        matches!(
            action,
            A::GotoSplit(_)
                | A::FocusNext
                | A::FocusPrev
                | A::FocusId(_)
                | A::WorkspacePrev
                | A::WorkspaceNext
                | A::WorkspaceLast
                | A::WorkspaceFocus(_)
                | A::WorkspaceMove(_)
        )
    }

    /// Sync the Core-hosted overlay surface to the capture owner (CTX-0943).
    ///
    /// Reads the retained `overlay`-slot blocks, keeps the single global
    /// owner's first block in discovery order as the surface (one session
    /// per plugin; the acquire gate already required the handle to be such
    /// a block), and clears the surface otherwise — capture end removes the
    /// surface per the contract. The [`Runtime`](bitty_runtime::Runtime)
    /// setter only marks redraw on actual change, so capture-less ticks
    /// stay idle.
    fn update_plugin_overlay(&mut self) {
        let Some(plugin_runtime) = self.plugin_runtime.as_ref() else {
            if self.runtime.plugin_overlay().is_some() {
                self.runtime.set_plugin_overlay(None);
            }
            return;
        };
        let owner = plugin_runtime
            .overlay_capture()
            .borrow()
            .owner_plugin()
            .map(str::to_string);
        let Some(owner) = owner else {
            if self.runtime.plugin_overlay().is_some() {
                self.runtime.set_plugin_overlay(None);
            }
            return;
        };
        let surface = plugin_runtime
            .ui_blocks()
            .into_iter()
            .filter(|(id, slot, _, _)| {
                id.as_str() == owner
                    && bitty_runtime::ui_slot_placement(*slot)
                        == bitty_runtime::UiSlotPlacement::Overlay
            })
            .map(|(id, slot, node, version)| bitty_runtime::BandContent {
                plugin_id: id.to_string(),
                slot,
                root: node,
                version,
            })
            .next();
        self.runtime.set_plugin_overlay(surface);
    }

    /// Dispatches one Core-routed plugin band click (CTX-0946 C1).
    ///
    /// The click carries the band owner's declared command verb plus its
    /// declared args as one named table (`run({id = 3})`). Fail-closed with
    /// a loud diagnostic when the plugin id is invalid, the runtime is
    /// gone, or the dispatch itself fails (unregistered verb, faulting
    /// handler): the click is dropped and terminal state is untouched.
    fn dispatch_band_click(&mut self, click: bitty_runtime::BandClickRequest) {
        let id = match bitty_plugin_host::manifest::PluginId::new(&click.plugin_id) {
            Ok(id) => id,
            Err(error) => {
                crate::logging::warn(|| {
                    format!(
                        "bitty: band click from '{}' dropped (invalid plugin id: {error})",
                        click.plugin_id
                    )
                });
                return;
            }
        };
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            crate::logging::warn(|| {
                format!(
                    "bitty: band click '{}:{}' dropped (plugin runtime is gone)",
                    click.plugin_id, click.command
                )
            });
            return;
        };
        let args = bitty_runtime::band_click_args_table(&click.args);
        if let Err(error) = plugin_runtime.dispatch_command(&id, &click.command, &[args]) {
            crate::logging::warn(|| {
                format!(
                    "bitty: band click '{}:{}' refused ({error})",
                    click.plugin_id, click.command
                )
            });
        }
    }

    /// Route post-intercept fall-through input into the capture queue
    /// (CTX-0943). Called by [`Self::handle_event`] after
    /// [`Self::intercept_chrome_key`](crate::chrome_keys::ChromeState)
    /// returns fall-through and before `Runtime::handle_platform_event`.
    ///
    /// Returns `true` when the event was captured (the caller must not route
    /// it further: no PTY bytes, no selection, no mouse capture). Releases,
    /// synthetic, and modifier-only keys keep routing so modifier mirrors
    /// and press-to-release ownership never desync; window focus loss
    /// revokes and still routes so `Runtime` records the unfocused state.
    /// `false` with no capture active (zero behavior change) or for event
    /// kinds without capture semantics. Paste-derived delivery stays parked
    /// to the paste path owner (needs the Core inspection decision).
    pub(crate) fn capture_fallthrough_input(&mut self, kind: &WindowEventKind) -> bool {
        if !self.overlay_capture_active() {
            return false;
        }
        match kind {
            WindowEventKind::KeyboardInput(key) => {
                if key.state != PressState::Pressed
                    || key.is_synthetic
                    || crate::chrome_keys::is_modifier_key(key)
                {
                    return false;
                }
                let text = key.text.clone().unwrap_or_else(|| overlay_key_text(key));
                self.push_overlay_input("key", &text)
            }
            WindowEventKind::Ime(bitty_platform::ImeEvent::Commit(text)) => {
                self.push_overlay_input("text", text)
            }
            // Preedit is uncommitted composition (presentation only) and
            // Enabled/Disabled carry no input bytes.
            WindowEventKind::Ime(_) => false,
            WindowEventKind::MouseInput(mouse) => {
                // Route releases so press-to-release ownership never desyncs:
                // only button presses enter the capture queue. A captured
                // release would leave Runtime drag/selection state armed with
                // no matching press ever arriving (CodeRabbit PR #1643).
                if mouse.state != PressState::Pressed {
                    return false;
                }
                let text = format!("{:?}:{:?}", mouse.button, mouse.state);
                self.push_overlay_input("pointer", &text)
            }
            WindowEventKind::CursorMoved(pos) => {
                let text = format!("move:{},{}", pos.x, pos.y);
                self.push_overlay_move(&text)
            }
            WindowEventKind::MouseWheel(delta) => {
                let text = format!("wheel:{delta:?}");
                self.push_overlay_input("pointer", &text)
            }
            WindowEventKind::Focused(false) => {
                self.revoke_overlay_capture();
                false
            }
            _ => false,
        }
    }

    /// Applies queued `bitty.workspace.*` requests (CTX-0889, ADR-0014).
    ///
    /// Bounded by the plugin runtime's request queue capacity. Each request
    /// runs through the exact handler its keybinding uses
    /// ([`Self::apply_chrome_action`]: capacity limit, never-empty last
    /// workspace, zoom restore before a move, kill-confirm arm), so plugin
    /// and keyboard behaviour cannot drift. Fail-closed rules:
    /// - while a capturing modal is up (workspace/view close confirm or a
    ///   panel overlay) every request is dropped, exactly like bound chords;
    /// - a plugin close only ever *arms* the kill confirm for a live
    ///   workspace; confirming stays a user gesture (repeat chord), so a
    ///   plugin can never kill live shells on its own;
    /// - a stable id that no longer exists drops the request.
    pub(crate) fn apply_plugin_workspace_requests(&mut self) {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return;
        };
        let requests = plugin_runtime.drain_workspace_requests();
        for queued in requests {
            if self.workspace_requests_blocked() {
                crate::logging::warn(|| {
                    format!(
                        "bitty: plugin '{}' workspace request dropped (confirmation pending)",
                        queued.plugin_id
                    )
                });
                continue;
            }
            if !self.apply_workspace_request(&queued.request) {
                crate::logging::warn(|| {
                    format!(
                        "bitty: plugin '{}' workspace request refused (unknown workspace id)",
                        queued.plugin_id
                    )
                });
            }
        }
    }

    /// Whether a capturing modal blocks workspace mutation (CTX-0889):
    /// the same set that swallows bound chords in `resolve_priority_for`.
    fn workspace_requests_blocked(&self) -> bool {
        self.runtime.has_pending_ws_close()
            || self.runtime.has_pending_close_confirm()
            || self.runtime.overlay_modal_active()
    }

    /// Applies one validated request; `false` when its target id is gone.
    fn apply_workspace_request(&mut self, request: &WorkspaceRequest) -> bool {
        use bitty_config::ChromeAction as A;
        let one_based = |index: usize| u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        match request {
            WorkspaceRequest::FocusId(seq) => {
                let Some(index) = self.runtime.workspace_index_by_seq(*seq) else {
                    return false;
                };
                self.apply_chrome_action(A::WorkspaceFocus(one_based(index)));
            }
            WorkspaceRequest::FocusIndex(position) => {
                self.apply_chrome_action(A::WorkspaceFocus(*position));
            }
            WorkspaceRequest::New => self.apply_chrome_action(A::WorkspaceNew),
            WorkspaceRequest::Next => self.apply_chrome_action(A::WorkspaceNext),
            WorkspaceRequest::Close(target) => {
                let index = match target {
                    None => self.runtime.active_workspace_index(),
                    Some(seq) => match self.runtime.workspace_index_by_seq(*seq) {
                        Some(index) => index,
                        None => return false,
                    },
                };
                if index == self.runtime.active_workspace_index() {
                    self.apply_chrome_action(A::WorkspaceClose);
                } else {
                    let _ = self.runtime.workspace_close_request_at(index);
                }
            }
            WorkspaceRequest::Rename { id: seq, name } => {
                let Some(index) = self.runtime.workspace_index_by_seq(*seq) else {
                    return false;
                };
                // Same Core handler as `bitty ctl workspace rename`.
                if let Err(err) = self.runtime.workspace_rename(index, name) {
                    crate::logging::warn(|| {
                        format!("bitty: plugin workspace rename refused ({err})")
                    });
                }
            }
            WorkspaceRequest::MovePanel(seq) => {
                let Some(index) = self.runtime.workspace_index_by_seq(*seq) else {
                    return false;
                };
                self.apply_chrome_action(A::WorkspaceMove(one_based(index)));
            }
        }
        true
    }

    /// Checks for exited shell processes across both split pane sessions and
    /// the primary session, reaping exited children and closing their panels.
    ///
    /// When the last remaining panel exits, returns [`ShellExitOutcome::AppExiting`]
    /// so the caller can save session state and signal `ctx.exit()`.
    pub(crate) fn reap_exited_shells(&mut self) -> ShellExitOutcome {
        // 1. Split pane sessions: poll each active session's child.
        let pane_ids: Vec<bitty_runtime::ViewId> = self.runtime.pane_session_ids();
        let mut closed_any = false;

        for view in pane_ids {
            if let Some(status) = self.runtime.pane_try_wait(&view) {
                // If there is only one leaf left, this was the last active pane.
                if self.runtime.leaf_count() <= 1 {
                    crate::logging::info(|| {
                        format!(
                            "bitty: last pane shell {view:?} exited (success={} code={} signal={:?}) — closing session",
                            status.is_success(),
                            status.code(),
                            status.signal()
                        )
                    });
                    return ShellExitOutcome::AppExiting;
                }

                crate::logging::info(|| {
                    format!(
                        "bitty: pane shell {view:?} exited (success={} code={} signal={:?}) — closing pane",
                        status.is_success(),
                        status.code(),
                        status.signal()
                    )
                });
                self.restore_zoom();
                let mut layout = self.runtime.layout().clone();
                if crate::chrome_keys::close_focused_leaf(&mut layout, view) {
                    self.runtime.set_layout_closing(layout, view);
                }
                self.runtime.close_pane_session(&view);
                closed_any = true;
            }
        }

        // 2. Primary shell session.
        if let Some(status) = self.runtime.primary_exit_status() {
            if self.runtime.pane_session_count() == 0 || self.runtime.leaf_count() <= 1 {
                crate::logging::info(|| {
                    format!(
                        "bitty: primary shell exited (success={} code={} signal={:?}) — closing session",
                        status.is_success(),
                        status.code(),
                        status.signal()
                    )
                });
                return ShellExitOutcome::AppExiting;
            }

            // Primary shell exited while split panes are still active:
            // Close the primary pane leaf so remaining panes take over.
            if let Some(primary_view) = self.runtime.primary_view() {
                crate::logging::info(|| {
                    format!(
                        "bitty: primary shell {primary_view:?} exited (success={} code={} signal={:?}) with {} panes running — closing primary pane",
                        status.is_success(),
                        status.code(),
                        status.signal(),
                        self.runtime.pane_session_count()
                    )
                });
                self.restore_zoom();
                let mut layout = self.runtime.layout().clone();
                if crate::chrome_keys::close_focused_leaf(&mut layout, primary_view) {
                    self.runtime.set_layout_closing(layout, primary_view);
                }
                closed_any = true;
            }
        }

        if closed_any {
            if let Some(win) = self.window.handle.as_ref() {
                win.request_redraw();
            }
            ShellExitOutcome::PaneClosed
        } else {
            ShellExitOutcome::NoExit
        }
    }

    /// Applies a sanitized, change-gated title to the OS window (CTX-0382,
    /// handoff seam CTX-0570).
    ///
    /// An empty sanitized title resets to the static theme title (matching
    /// xterm's reset-to-default behavior). Identical titles are dropped so
    /// the titlebar never churns per frame; `title_applies` counts real
    /// applications. The OS handoff goes through
    /// [`WindowState::os_title_sink`]: production installs the
    /// [`WindowHandle`] wrapper at window creation, so this method has one
    /// production code path that a recording test double observes exactly.
    /// Headlessly (no sink) state is still recorded but no OS call is made.
    pub(crate) fn apply_window_title(&mut self, raw: &str) {
        let sanitized = sanitize_window_title(raw);
        let title = if sanitized.is_empty() {
            self.window.title.clone()
        } else {
            sanitized
        };
        if self.window.last_applied_title.as_deref() == Some(title.as_str()) {
            return;
        }
        self.window.last_applied_title = Some(title.clone());
        self.window.title_applies += 1;
        if let Some(sink) = self.window.os_title_sink.as_ref() {
            sink.set_os_title(&title);
        }
    }

    /// Installs a title-handoff sink, replacing any previous one (CTX-0570).
    ///
    /// Production calls this once at window creation with the live
    /// [`WindowHandle`]; tests call it with a recording double so the exact
    /// call sequence applied to the OS can be asserted without a window
    /// system. Replacing is intentional: a re-created window must not leave a
    /// stale sink behind.
    #[cfg(test)]
    pub(crate) fn set_os_title_sink(&mut self, sink: Box<dyn OsTitleSink>) {
        self.window.os_title_sink = Some(sink);
    }

    /// Live OSC 8 click-to-open consumer (CTX-0577, M1-17 / issue #1143).
    ///
    /// Called after a mouse event when the runtime has armed a hyperlink
    /// activation; consumes the single-use gesture and opens the bound URI
    /// through the runtime opener seam. No plugin interceptors exist on this
    /// path today (an empty decision list means proceed); failures are
    /// reported loudly, never silently swallowed, because a refused click is
    /// user-visible intent.
    pub(crate) fn activate_pending_hyperlink_now(&mut self) {
        match self.runtime.activate_pending_hyperlink(&[], false) {
            Ok(uri) => {
                crate::logging::info(|| format!("bitty: opened hyperlink {uri}"));
            }
            Err(err) => {
                eprintln!("bitty: hyperlink activation refused ({err})");
            }
        }
    }

    /// Attempts to attach a real GPU surface after window creation (single-window slice).
    pub(crate) fn try_attach_gpu(&mut self, handle: &WindowHandle) {
        // Do not re-attach if already has GPU
        if self.runtime.has_gpu() {
            return;
        }
        let target = handle.surface_target();
        let inner = target.inner_size();
        // Only attempt GPU when we have a non-zero physical size
        if inner.width() == 0 || inner.height() == 0 {
            crate::logging::info(|| String::from("bitty: gpu attach skipped (zero-size surface)"));
            return;
        }
        // CTX-0142: adopt the live DPI scale at attach (the compositor may
        // have delivered fractional scale before this point while winit still
        // reported 1.0): rescale renderer font/atlas to scaled cells and
        // derive the grid from the physical inner_size — never the logical
        // size path (the suspected original sin behind #232).
        let scale = target.scale_factor().get();
        self.runtime.apply_dpi_scale(scale, Some(inner));
        let snap = self.runtime.snapshot();
        match pollster::block_on(GpuContext::initialize()) {
            Ok(gpu) => match gpu.create_surface(&target) {
                Ok(surface) => {
                    let extent = PhysicalSize::new(inner.width(), inner.height());
                    // Configure surface with current extent (bounded, validated)
                    // and the effective window opacity (CTX-0290): the
                    // renderer scales its premultiplied output so the
                    // compositor can blend the window at `window.opacity`.
                    match surface.configure_with_opacity(&gpu, extent, self.window.opacity) {
                        Ok(()) => {
                            if self.window.opacity < 1.0 && !surface.opacity_alpha_supported() {
                                crate::logging::warn(|| {
                                    format!(
                                        "bitty: window.opacity={:.3} unsupported on this GPU surface (no premultiplied alpha mode) — staying opaque",
                                        bitty_platform::sanitize_opacity(self.window.opacity)
                                    )
                                });
                            }
                            self.runtime.attach_gpu(gpu, surface);
                            crate::logging::info(|| {
                                format!(
                                    "bitty: gpu attached (extent={}x{} scale={scale} dpi={} grid={}x{} crossfont={})",
                                    extent.width(),
                                    extent.height(),
                                    self.runtime.dpi_scale(),
                                    snap.width,
                                    snap.height,
                                    self.runtime.is_crossfont()
                                )
                            });
                        }
                        Err(err) => {
                            crate::logging::warn(|| {
                                format!(
                                    "bitty: gpu surface configure failed ({err}) — staying headless"
                                )
                            });
                        }
                    }
                }
                Err(err) => {
                    crate::logging::warn(|| {
                        format!("bitty: gpu surface creation failed ({err}) — staying headless")
                    });
                }
            },
            Err(err) => {
                crate::logging::warn(|| {
                    format!("bitty: gpu initialize failed ({err}) — staying headless (CI fallback)")
                });
            }
        }
    }
}

// Chrome-key cluster lives in `chrome_keys` (CTX-0233 pure move:
// `is_modifier_key`, `track_app_modifiers`, `clear_app_modifiers_on_focus`,
// `key_ref_from_event`, split/layout-surgery helpers).
/// Chrome actions live in [`crate::chrome_keys`] (CTX-0233 pure move).
/// Chrome intercept lives in [`crate::chrome_keys`] (CTX-0233 pure move).
impl AppHandler for TerminalApp {
    fn set_event_waker(&mut self, waker: EventWaker) {
        // Bridge the platform proxy into the runtime's bounded wakeup pump:
        // the forwarder thread owns its clone and wakes once per readability
        // signal (plus once on EOF). `Mutex` keeps the closure `Send + Sync`
        // even if the proxy is only `Send`.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(waker));
        let shared_pty = std::sync::Arc::clone(&shared);
        let pty_waker: bitty_runtime::PtyWaker = std::sync::Arc::new(move || {
            if let Ok(w) = shared_pty.lock() {
                w.wake_pty();
            }
        });
        self.runtime.set_pty_waker(pty_waker);
        // CTX-0235: wake the same event loop when an IPC control action is
        // enqueued, so an idle window (`ControlFlow::Wait`, no PTY damage)
        // drains the control queue promptly instead of letting every verb
        // time out. The wakeup reuses the PTY-readability signal because
        // that arm already drains the control queue first via `drive_tick`;
        // it grants nothing — `drain_global_control_queue` re-authorizes
        // every action against the servo scopes before applying.
        if let Ok(w) = shared.lock() {
            let control_proxy = w.clone();
            bitty_ipc::ctl::set_control_waker(Some(std::sync::Arc::new(move || {
                control_proxy.wake_pty();
            })));
        }
        crate::logging::info(|| String::from("bitty: pty wakeup armed (event-loop proxy)"));
    }

    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent) {
        // Bounded PTY pump: drain before handling the event so fresh bytes are
        // visible to the state machine before the tick.
        self.poll_pty_pump();

        // CTX-0943 (CodeRabbit PR #1643): expire captures before routing so
        // input arriving after the deadline falls through to the terminal
        // instead of being queued by a stale `is_active` check.
        self.expire_overlay_captures();

        // Issue #1356 / #1541: reap exited child processes across primary and
        // split pane sessions. When a split child exits, its pane is closed
        // and its sibling promoted. When the last child exits, the session
        // closes cleanly.
        if self.reap_exited_shells() == ShellExitOutcome::AppExiting {
            self.save_session_best_effort("shell-exit");
            self.runtime.shutdown();
            ctx.exit();
            return;
        }

        // CTX-0153 single-owner intercept with CTX-0275 explicit dispatch
        // priority (see `intercept_chrome_key` / `DispatchPriority`):
        // emergency/reserved > active overlay-modal > user-defined keymap >
        // plugin (inert, deny-by-default) > terminal encoding. A consumed
        // event never reaches `Runtime`.
        if let PlatformEvent::Window { window_id: _, kind } = &event {
            if self.intercept_chrome_key(kind) {
                return;
            }
        }

        // CTX-0943: while a transient capture holds, route key/IME/pointer
        // fall-through into the capture queue. A captured event never
        // reaches `Runtime`, so no PTY bytes and no terminal mutation occur;
        // with no capture active this is a single bool check per event.
        if let PlatformEvent::Window { window_id: _, kind } = &event {
            if self.capture_fallthrough_input(kind) {
                return;
            }
        }

        // Shutdown handling: PlatformEvent::Exiting or CloseRequested/Closed
        // ask the handler to exit the loop.
        //
        // CTX-0186: snapshot the paste-dialog state before routing so the
        // right/middle-click and Esc-cancel paths can report loudly afterwards
        // (a gated paste is never silent).
        let paste_probe = match &event {
            PlatformEvent::Window { window_id: _, kind } => match kind {
                WindowEventKind::MouseInput(mouse)
                    if mouse.state == PressState::Pressed
                        && matches!(mouse.button, MouseButton::Right | MouseButton::Middle) =>
                {
                    Some((
                        "mouse",
                        self.runtime.has_pending_paste(),
                        self.runtime.pending_input_len(),
                    ))
                }
                WindowEventKind::KeyboardInput(key)
                    if key.state == PressState::Pressed
                        && matches!(&key.logical_key, LogicalKey::Named(NamedKey::Escape)) =>
                {
                    Some((
                        "esc",
                        self.runtime.has_pending_paste(),
                        self.runtime.pending_input_len(),
                    ))
                }
                _ => None,
            },
            _ => None,
        };
        let should_exit = self.runtime.handle_platform_event(event.clone());
        if should_exit {
            crate::logging::info(|| format!("bitty: exit requested ({event:?})"));
            self.save_session_best_effort("exit");
            self.runtime.shutdown();
            ctx.exit();
            return;
        }
        // CTX-0577 (M1-17): the live OSC 8 click-to-open consumer. A primary
        // mouse release over a safe hyperlink cell mints a single-use
        // gesture; consume it here through the runtime opener seam so the
        // authorized URI is actually opened (not parse-only). Fail-closed:
        // anything without a gesture, or outside the scheme allowlist, is
        // refused and counted inside the runtime.
        if self.runtime.has_pending_hyperlink_activation() {
            self.activate_pending_hyperlink_now();
        }
        // CTX-0946 C1: Core-routed plugin band clicks. The runtime owns the
        // geometry (which band row, which declared command); the application
        // dispatches each through the normal `PluginRuntime::dispatch_command`
        // path, where registration and capability gates fail closed as
        // usual. Dispatch failures are loud (user-paced gestures, never a
        // hot loop) and never disturb terminal state.
        for click in self.runtime.drain_band_clicks() {
            self.dispatch_band_click(click);
        }
        // CTX-0370: a window-close request that did not exit armed a bounded
        // confirmation (or was superseded by one); report it loudly so the
        // pending gate is never silent in the log. `AboutToWait` presents the
        // overlay pill from `pending_full_redraw`.
        if matches!(
            &event,
            PlatformEvent::Window {
                window_id: _,
                kind: WindowEventKind::CloseRequested,
            }
        ) {
            if let Some(summary) = self.runtime.close_confirm_banner_text() {
                eprintln!("bitty: window close PENDING -> {summary}");
            }
        }
        // CTX-0186 loud paste-dialog reporting: pending shows the bounded
        // summary with confirm/cancel instructions; a cleared pending with new
        // input bytes means the repeat gesture confirmed delivery; an Esc
        // that cleared pending without new bytes means it was cancelled.
        if let Some((probe_kind, had_pending, before_len)) = paste_probe {
            let has_pending = self.runtime.has_pending_paste();
            let after_len = self.runtime.pending_input_len();
            if has_pending {
                if let Some(summary) = self.runtime.pending_paste_summary() {
                    eprintln!("bitty: paste -> {summary}");
                }
            } else if had_pending && after_len > before_len {
                eprintln!("bitty: paste confirmed -> delivered");
            } else if had_pending && probe_kind == "esc" {
                eprintln!("bitty: paste confirmation cancelled (Esc)");
            }
            if let Some(win) = self.window.handle.as_ref() {
                win.request_redraw();
            }
        }

        match event {
            PlatformEvent::Resumed => {
                // First delivery — intended startup window creation point.
                // Headless fallback: `ctx.create_window` returns
                // `PlatformError::WindowCreation` when no window system exists;
                // we keep running headlessly and still tick, rather than
                // aborting the process (mirrors `App::run`'s
                // `DisplayUnavailable` mapping).
                if self.window.handle.is_none() {
                    let default_size = LogicalSize::new(800.0, 600.0).unwrap_or_else(|_| {
                        // LogicalSize validation only fails for non-finite or
                        // negative inputs; hard-coded values are valid, so
                        // this fallback is unreachable but keeps the handler
                        // total.
                        LogicalSize::new(640.0, 480.0).expect("fallback size must be valid")
                    });
                    let config = WindowConfig::new()
                        .with_title(self.window.title.clone())
                        .with_inner_size(default_size)
                        .with_opacity(self.window.opacity)
                        .with_blur_radius(self.window.blur_radius)
                        .with_visible(true);
                    match ctx.create_window(config) {
                        Ok(handle) => {
                            let id = handle.id();
                            self.window.id = Some(id);
                            // CTX-0367: opt the window into platform IME
                            // events (winit defaults to IME disabled, which
                            // is exactly why fcitx5 could not compose).
                            // Wayland text-input-v3 / X11 XIM / macOS /
                            // Windows all route through this one call.
                            handle.set_ime_allowed(true);
                            // Clone handle before moving into try_attach_gpu (which borrows self mutably)
                            let handle_for_gpu = handle.clone();
                            // CTX-0570: install the title-handoff sink here
                            // (the one production site), so OSC 0/2 title
                            // application flows through the same seam tests
                            // observe with a recording double.
                            self.window.os_title_sink = Some(Box::new(handle.clone()));
                            self.window.handle = Some(handle);
                            // Single-window vertical slice: try real GPU attach with crossfont atlas.
                            // On headless CI this fails with NoCompatibleAdapter and we stay headless
                            // (deterministic fallback, no panic). On a real display we get a wgpu surface
                            // via winit's SurfaceTarget and present via tick.
                            self.try_attach_gpu(&handle_for_gpu);
                            crate::logging::info(|| {
                                format!(
                                    "bitty: window created id={} gpu={} crossfont={} focused={:?} leafs={} ime=allowed",
                                    id.get(),
                                    self.runtime.has_gpu(),
                                    self.runtime.is_crossfont(),
                                    self.runtime.focused_view(),
                                    self.runtime.leaf_count()
                                )
                            });
                        }
                        Err(err) => {
                            crate::logging::warn(|| {
                                format!(
                                    "bitty: window creation failed ({err}) — continuing headless (no GPU, no display)"
                                )
                            });
                        }
                    }
                }
                // Resumed is a good point to request the first redraw.
                if let Some(win) = self.window.handle.as_ref() {
                    win.request_redraw();
                } else {
                    // Headless: still drive one tick so CI-like smoke appears
                    // even when running without a window system but not in
                    // explicit --headless mode.
                    let _ = self.drive_tick();
                }
            }
            PlatformEvent::Window { window_id: _, kind } => {
                // `handle_platform_event` already routed Resized /
                // ScaleFactorChanged / CloseRequested / RedrawRequested /
                // KeyboardInput (unbound keys to the PTY; bound chords were
                // consumed by the single-owner intercept above and return
                // early, so no focus matcher lives here anymore).
                // Request a tick on redraw and after resize.
                match kind {
                    WindowEventKind::Resized(_) | WindowEventKind::ScaleFactorChanged(_) => {
                        // CTX-0142: ScaleFactorChanged carries no size (winit
                        // 0.30 drops the negotiation hook), so re-read the
                        // physical inner_size here — never the logical path —
                        // and adopt before the tick renders at scaled cells.
                        // Resized needs no extra work: runtime derives from
                        // the same live scaled cells.
                        if let WindowEventKind::ScaleFactorChanged(factor) = &kind {
                            let physical = self.window.handle.as_ref().map(|win| win.inner_size());
                            self.runtime.apply_dpi_scale(factor.get(), physical);
                            let snap = self.runtime.snapshot();
                            crate::logging::info(|| {
                                format!(
                                    "bitty: dpi adopted scale={} dpi={} grid={}x{} physical={} surface={:?}",
                                    factor.get(),
                                    self.runtime.dpi_scale(),
                                    snap.width,
                                    snap.height,
                                    physical.map_or(String::from("none"), |p| format!(
                                        "{}x{}",
                                        p.width(),
                                        p.height()
                                    )),
                                    self.runtime.surface_extent().map(|e| format!(
                                        "{}x{}",
                                        e.width(),
                                        e.height()
                                    )),
                                )
                            });
                        }
                        if let Some(win) = self.window.handle.as_ref() {
                            win.request_redraw();
                        } else {
                            let _ = self.drive_tick();
                        }
                    }
                    WindowEventKind::RedrawRequested => {
                        // Frame-on-demand: no damage → no present, no redraw
                        // loop. Keep Wait mode (default `ControlFlow::Wait`)
                        // so idle burns ≤ 1 % CPU (PB-7 budget).
                        let _ = self.drive_tick();
                    }
                    _ => {}
                }
            }
            PlatformEvent::AboutToWait => {
                // Per winit docs this is a good place to do per-frame work.
                // Poll PTY pump again and drive tick; request redraw only when
                // tick produced a present (frame-on-demand).
                self.poll_pty_pump();
                if self.reap_exited_shells() == ShellExitOutcome::AppExiting {
                    self.save_session_best_effort("shell-exit");
                    self.runtime.shutdown();
                    ctx.exit();
                    return;
                }
                if self.drive_tick().is_some() {
                    if let Some(win) = self.window.handle.as_ref() {
                        win.request_redraw();
                    }
                }
                // CTX-0334: arm a single timed wake for a pending hover
                // dwell so a stopped pointer still activates; otherwise
                // return to energy-saving wait (frame-on-demand).
                // RFC-0002 (CTX-0341): an active panel animation also arms a
                // bounded wake at its next frame; when the last animation
                // ends the deadline is `None` and the loop returns to wait
                // (zero periodic wakeups, PB-7).
                // CTX-0577 (review PX-3067): the bounded bell flash and
                // notification banner also expire on a timer, so arm a wake
                // at their deadline too; otherwise a quiet window would keep
                // the surface until unrelated activity forced a frame.
                let hover = self.runtime.hover_activation_deadline();
                let animation = self.runtime.animation_deadline();
                let bell = self.runtime.bell_notification_deadline();
                // CTX-0943 (CodeRabbit PR #1643): arm a wake at the capture
                // deadline too, otherwise an idle window holds an expired
                // capture (and its painted overlay) until unrelated activity.
                let capture = self.overlay_capture_deadline();
                let wake = [hover, animation, bell, capture]
                    .into_iter()
                    .flatten()
                    .min();
                match wake {
                    Some(deadline) => ctx.set_wait_until(deadline),
                    None => ctx.set_wait(),
                }
            }
            PlatformEvent::PtyReadable => {
                // Evented PTY wakeup: the top-of-handler `poll_pty_pump`
                // already drained the bounded forwarder channel, so tick when
                // damage exists and request a redraw only on present
                // (frame-on-demand; quiet shells idle with no further wakes).
                if self.drive_tick().is_some() {
                    if let Some(win) = self.window.handle.as_ref() {
                        win.request_redraw();
                    }
                }
            }
            // CTX-0951 (NEEDS-FIX PX-4669): live OS light/dark toggle. The
            // platform glue re-dispatches winit `ThemeChanged` as this
            // app-level event (macOS/Windows natively; backends without OS
            // theme support never emit it). Resolve against the live
            // effective config (reload engine's committed `current`, so a
            // file reload and an OS toggle compose) and swap via the
            // runtime seam, which dedups, repaints once, and preserves OSC
            // overrides; single themes and the already-active half are
            // no-ops. Without an installed reload context (tests, `--safe`)
            // there is no live config to resolve against, so this stays a
            // no-op instead of guessing.
            PlatformEvent::SystemAppearanceChanged(appearance) => {
                crate::config_reload::record_system_appearance(appearance);
                let Some(effective) = crate::config_reload::current_effective() else {
                    return;
                };
                if self.runtime.apply_system_appearance(&effective, appearance) {
                    let prefer_light =
                        matches!(appearance, bitty_platform::SystemAppearance::Light);
                    let active = effective.effective_theme_for(prefer_light);
                    self.refresh_theme_title(active.name);
                    crate::logging::info(|| {
                        format!(
                            "bitty: OS appearance -> {appearance} (theme={})",
                            active.name
                        )
                    });
                    if let Some(win) = self.window.handle.as_ref() {
                        win.request_redraw();
                    } else {
                        let _ = self.drive_tick();
                    }
                }
            }
            // P3-8 defense-in-depth: `PlatformEvent::Exiting` is already
            // saved-and-exited by the `should_exit` early return above
            // (`handle_platform_event` reports `true` for it), so this arm
            // is unreachable today. It stays as a backstop: if the router
            // ever stops reporting `Exiting`, the loop still saves (atomic,
            // idempotent) and exits instead of silently dropping the session.
            PlatformEvent::Exiting => {
                self.save_session_best_effort("loop-exiting");
                self.runtime.shutdown();
                ctx.exit();
            }
            _ => {}
        }
    }
}

// The `bitty-pty` bounded-channel seam uses `READ_CHUNK_SIZE` and
// `CHANNEL_CAPACITY_CHUNKS` constants, but we keep the demo pump's channel
// capacity literal (16) mirroring that constant without importing the crate
// directly — `bitty-terminal` wires `bitty-runtime` + `bitty-platform` +
// `bitty-render` + `bitty-config` as the thin composition root (ADR-0003
// entry point; no business logic beyond wiring). The literal is documented
// here to avoid a hidden dependency.
#[allow(dead_code)]
fn _assert_channel_capacity_is_documented() {
    const EXPECTED: usize = 16;
    const { assert!(EXPECTED > 0) }
}

#[cfg(test)]
mod event_tracker_tests {
    use super::*;

    fn tracker(title: &str, window_focused: bool) -> EventTracker {
        EventTracker {
            title: title.to_string(),
            window_focused,
            workspaces: Vec::new(),
        }
    }

    #[test]
    fn unchanged_state_delivers_nothing() {
        let mut t = tracker("same", true);
        assert!(t.take_changes("same", true, &[]).is_empty());
    }

    #[test]
    fn title_change_coalesces_to_latest_and_updates_tracker() {
        let mut t = tracker("old", false);
        let changes = t.take_changes("newest", false, &[]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "terminal.title-changed");
        assert_eq!(
            changes[0].1,
            LuaValue::table([("title", LuaValue::String("newest".into()))])
        );
        assert_eq!(t.title, "newest");
        assert!(t.take_changes("newest", false, &[]).is_empty());
    }

    #[test]
    fn focus_change_carries_new_value() {
        let mut t = tracker("x", false);
        let changes = t.take_changes("x", true, &[]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "focus.changed");
        assert_eq!(
            changes[0].1,
            LuaValue::table([("focused", LuaValue::Bool(true))])
        );
        assert!(t.window_focused);
    }

    #[test]
    fn both_changes_are_bounded_to_one_event_per_kind() {
        let mut t = tracker("a", false);
        let kinds: Vec<_> = t
            .take_changes("b", true, &[])
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(kinds, ["terminal.title-changed", "focus.changed"]);
    }

    #[test]
    fn workspace_changes_coalesce_and_order_closed_created_renamed_changed_focused() {
        use bitty_runtime::WorkspaceSummary;
        let ws = |seq: u64, name: &str, active: bool, panels: Vec<u64>| WorkspaceSummary {
            seq,
            name: name.to_string(),
            active,
            panel_ids: panels,
            scratchpad_count: 0,
            scratchpad_occupied: false,
        };
        let mut t = tracker("t", true);
        let old = vec![ws(1, "ws1", true, vec![10]), ws(2, "ws2", false, vec![20])];
        assert!(t.take_changes("t", true, &old).is_empty(), "first snapshot");
        // Workspace 1 closed, 3 created, 2 renamed+changed, 2 focused.
        let new = vec![
            ws(2, "edited", true, vec![20, 21]),
            ws(3, "ws3", false, vec![30]),
        ];
        let events = t.take_changes("t", true, &new);
        let kinds: Vec<_> = events.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            [
                "workspace.closed",
                "workspace.created",
                "workspace.renamed",
                "workspace.changed",
                "workspace.focused"
            ]
        );
        let id_of = |payload: &LuaValue| match payload {
            LuaValue::Table(pairs) => pairs
                .iter()
                .find_map(|(k, v)| match (k, v) {
                    (LuaValue::String(s), LuaValue::Integer(id)) if s == "id" => Some(*id),
                    _ => None,
                })
                .expect("id not found or not an integer"),
            _ => panic!("payload not a table"),
        };
        assert_eq!(id_of(&events[0].1), 1, "closed event carries seq 1");
        assert_eq!(id_of(&events[1].1), 3, "created event carries seq 3");
        assert_eq!(id_of(&events[2].1), 2, "renamed event carries seq 2");
        assert_eq!(id_of(&events[3].1), 2, "changed event carries seq 2");
        assert_eq!(id_of(&events[4].1), 2, "focused event carries seq 2");
        assert_eq!(t.workspaces, new, "tracker holds the committed snapshot");
    }

    #[test]
    fn workspace_events_fire_at_most_once_per_workspace_per_tick() {
        use bitty_runtime::WorkspaceSummary;
        let ws = |seq: u64, name: &str, active: bool, panels: Vec<u64>| WorkspaceSummary {
            seq,
            name: name.to_string(),
            active,
            panel_ids: panels,
            scratchpad_count: 0,
            scratchpad_occupied: false,
        };
        let mut t = tracker("t", true);
        let old = vec![ws(1, "ws1", true, vec![10]), ws(2, "ws2", false, vec![20])];
        t.take_changes("t", true, &old);
        // Two workspaces closed, two created: four events bounded by workspace count.
        let new = vec![ws(3, "ws3", true, vec![30]), ws(4, "ws4", false, vec![40])];
        let events = t.take_changes("t", true, &new);
        assert_eq!(
            events.len(),
            5,
            "2 closed + 2 created + 1 focused = 5 events"
        );
    }

    #[test]
    fn workspace_events_skip_unchanged_kinds() {
        use bitty_runtime::WorkspaceSummary;
        let ws = |seq: u64, name: &str, active: bool, panels: Vec<u64>| WorkspaceSummary {
            seq,
            name: name.to_string(),
            active,
            panel_ids: panels,
            scratchpad_count: 0,
            scratchpad_occupied: false,
        };
        let mut t = tracker("t", true);
        let old = vec![ws(1, "ws1", true, vec![10])];
        t.take_changes("t", true, &old);
        let same = vec![ws(1, "ws1", true, vec![10])];
        assert!(
            t.take_changes("t", true, &same).is_empty(),
            "identical summary fires nothing"
        );
        // Name change only: one renamed event.
        let renamed = vec![ws(1, "editor", true, vec![10])];
        let events = t.take_changes("t", true, &renamed);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "workspace.renamed");
        // Panel change only: one changed event.
        let changed = vec![ws(1, "editor", true, vec![10, 11])];
        let events = t.take_changes("t", true, &changed);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "workspace.changed");
    }

    #[test]
    fn workspace_changed_fires_on_scratchpad_put_and_take() {
        // CTX-0954: a scratchpad put/take flips occupancy with identical
        // panels and still fires the existing `workspace.changed` (no new
        // event family); steady occupancy fires nothing.
        use bitty_runtime::WorkspaceSummary;
        let ws = |occupied: bool| WorkspaceSummary {
            seq: 1,
            name: "ws1".to_string(),
            active: true,
            panel_ids: vec![10],
            scratchpad_count: usize::from(occupied),
            scratchpad_occupied: occupied,
        };
        let mut t = tracker("t", true);
        let empty = vec![ws(false)];
        assert!(
            t.take_changes("t", true, &empty).is_empty(),
            "first snapshot"
        );
        // Put: identical panels, occupancy flipped -> one changed event.
        let events = t.take_changes("t", true, &[ws(true)]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "workspace.changed");
        // Take: flips back -> one changed event.
        let events = t.take_changes("t", true, &empty);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "workspace.changed");
        // Steady occupied: nothing fires.
        t.take_changes("t", true, &[ws(true)]);
        assert!(t.take_changes("t", true, &[ws(true)]).is_empty());
    }
}
