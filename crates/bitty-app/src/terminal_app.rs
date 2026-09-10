//! Window/platform event handler for the composition root (`TerminalApp`).

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;

use bitty_platform::{
    AppHandler, EventContext, EventWaker, LogicalKey, LogicalSize, MouseButton, NamedKey,
    PhysicalSize, PlatformEvent, PressState, WindowConfig, WindowEventKind, WindowHandle, WindowId,
};
use bitty_render::gpu::GpuContext;
use bitty_runtime::{LayoutNode, Runtime};

use crate::chrome_keys::AppModifiers;
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

// ---------------------------------------------------------------------------
// App handler
// ---------------------------------------------------------------------------

/// `AppModifiers` lives in [`crate::chrome_keys`] (CTX-0233 pure move).
/// The Correct Terminal handler: owns `Runtime`, an optional window, and the
/// real PTY pump via `Runtime::poll_pty` (plus an opt-in synthetic demo pump
/// only when explicitly attached for debug/tests).
/// All business stays in `bitty-runtime`; this type only wires
/// `PlatformEvent` → `Runtime` and `tick` → present, with real `GpuContext`
/// attachment for the single-window vertical slice.
pub(crate) struct TerminalApp {
    pub(crate) runtime: Runtime,
    /// Window title carrying the resolved theme preset + source layer.
    pub(crate) window_title: String,
    /// Window opacity from the effective config (CTX-0223
    /// `window.opacity`; default `1.0` = opaque). Applied to the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) at creation and to the
    /// renderer at GPU attach (CTX-0290): the platform requests compositor
    /// blending where supported, and the renderer scales its premultiplied
    /// output so the value has a visible effect. Platforms without
    /// premultiplied compositing stay opaque with a loud warning.
    pub(crate) window_opacity: f32,
    pub(crate) window: Option<WindowHandle>,
    pub(crate) window_id: Option<WindowId>,
    /// Demo pump channel when explicitly attached for debug/tests
    /// (`None` in real sessions — CTX-0167).
    pub(crate) pty_rx: Option<Receiver<Vec<u8>>>,
    pub(crate) _pty_thread: Option<JoinHandle<()>>,
    /// Count of `tick` calls that presented a frame.
    pub(crate) presented_frames: u64,
    /// Resolved keymap table (shipped defaults + user overrides).
    pub(crate) keymaps: Vec<bitty_config::ResolvedKeymap>,
    /// App-side modifier mirror for chord matching.
    pub(crate) app_mods: AppModifiers,
    /// Chrome-owned keys with an unreleased press (CTX-0229 press-to-release
    /// ownership). A consumed chord owns its key until the physical release:
    /// repeats/duplicates arriving after modifier decay stay swallowed
    /// instead of leaking shell bytes (e.g. `Ctrl+Shift+V` paste followed by
    /// a `V` repeat with `Ctrl` already released must not type `V`).
    /// Releases and focus transitions clear entries (same staleness bound as
    /// the CTX-0187 mirror clear); bounded by simultaneously held keys.
    pub(crate) chrome_held: HashSet<bitty_config::KeyName>,
    /// Layout stashed by `toggle_zoom`; `None` when not zoomed.
    pub(crate) zoom_backup: Option<LayoutNode>,
    /// Frozen startup spawn recipe so `new_split` leaves replay the exact
    /// program/shell resolution (CTX-0176).
    pub(crate) spawn_spec: SpawnSpec,
    /// Stderr verbosity gate (CTX-0190). Default [`LogLevel::Warn`] (quiet):
    /// per-frame `bitty tick` lines require `Debug`/`Trace`. User-facing key
    /// info (paste confirm/cancel, startup summary) and warnings/errors
    /// bypass this gate and always emit.
    pub(crate) log_level: LogLevel,
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
        Self {
            runtime,
            window_title: window_title_for_theme(theme_name, source),
            window_opacity: 1.0,
            window: None,
            window_id: None,
            pty_rx: None,
            _pty_thread: None,
            presented_frames: 0,
            keymaps,
            app_mods: AppModifiers::default(),
            chrome_held: HashSet::new(),
            zoom_backup: None,
            spawn_spec,
            log_level: LogLevel::default_level(),
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
        Self {
            runtime,
            window_title: window_title_for_theme(theme_name, source),
            window_opacity: 1.0,
            window: None,
            window_id: None,
            pty_rx: Some(pty_rx),
            _pty_thread: Some(handle),
            presented_frames: 0,
            keymaps,
            app_mods: AppModifiers::default(),
            chrome_held: HashSet::new(),
            zoom_backup: None,
            spawn_spec,
            log_level: LogLevel::default_level(),
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

    /// Sets the window opacity applied at creation and GPU attach
    /// (CTX-0223/CTX-0290). Call once at startup from the effective config;
    /// the value is sanitized by the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) and the renderer
    /// surface, so out-of-range inputs degrade instead of failing creation.
    pub(crate) fn with_window_opacity(mut self, opacity: f32) -> Self {
        self.window_opacity = opacity;
        self
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
    pub(crate) fn drive_tick(&mut self) -> Option<bitty_runtime::PresentStats> {
        // CTX-0171: drain IPC runtime-control queue before present so
        // `bitty ctl` mutations (send/split/focus/close/spawn/reload) apply
        // on the main thread — the sole `Runtime` owner — with server-side
        // scope enforcement (never ambient authority).
        let _ =
            ctl::drain_global_control_queue(&mut self.runtime, &ctl::granted_scopes_for_servo());
        // Ensure replies that were queued before tick are flushed before present:
        // the runtime's tick consumes snapshot+damage and composites.
        let stats = self.runtime.tick();
        if let Some(present) = stats {
            self.presented_frames += 1;
            if let Some(line) = self.maybe_format_tick(&present) {
                eprintln!("{line}");
            }
            if self.runtime.replies_overflowed() {
                eprintln!("warning: terminal reply queue overflowed (bounded cap)");
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
            let pending = self.runtime.cold_queue_len();
            if pending > 0 && self.tick_logging_enabled() {
                let events = self.runtime.drain_cold_events();
                eprintln!(
                    "bitty cold-queue: drained {} events, {} remain",
                    events.len(),
                    pending
                );
            } else if pending > 0 {
                // Quiet default still drains to keep the queue bounded, but
                // stays silent: no per-tick stderr noise.
                let _ = self.runtime.drain_cold_events();
            }
        }
        stats
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
            eprintln!("bitty: gpu attach skipped (zero-size surface)");
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
                    match surface.configure_with_opacity(&gpu, extent, self.window_opacity) {
                        Ok(()) => {
                            if self.window_opacity < 1.0 && !surface.opacity_alpha_supported() {
                                eprintln!(
                                    "bitty: window.opacity={:.3} unsupported on this GPU surface (no premultiplied alpha mode) — staying opaque",
                                    bitty_platform::sanitize_opacity(self.window_opacity)
                                );
                            }
                            self.runtime.attach_gpu(gpu, surface);
                            eprintln!(
                                "bitty: gpu attached (extent={}x{} scale={scale} dpi={} grid={}x{} crossfont={})",
                                extent.width(),
                                extent.height(),
                                self.runtime.dpi_scale(),
                                snap.width,
                                snap.height,
                                self.runtime.is_crossfont()
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "bitty: gpu surface configure failed ({err}) — staying headless"
                            );
                        }
                    }
                }
                Err(err) => {
                    eprintln!("bitty: gpu surface creation failed ({err}) — staying headless");
                }
            },
            Err(err) => {
                eprintln!("bitty: gpu initialize failed ({err}) — staying headless (CI fallback)");
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
        eprintln!("bitty: pty wakeup armed (event-loop proxy)");
    }

    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent) {
        // Bounded PTY pump: drain before handling the event so fresh bytes are
        // visible to the state machine before the tick.
        self.poll_pty_pump();

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
            eprintln!("bitty: exit requested ({event:?})");
            ctx.exit();
            return;
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
            if let Some(win) = self.window.as_ref() {
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
                if self.window.is_none() {
                    let default_size = LogicalSize::new(800.0, 600.0).unwrap_or_else(|_| {
                        // LogicalSize validation only fails for non-finite or
                        // negative inputs; hard-coded values are valid, so
                        // this fallback is unreachable but keeps the handler
                        // total.
                        LogicalSize::new(640.0, 480.0).expect("fallback size must be valid")
                    });
                    let config = WindowConfig::new()
                        .with_title(self.window_title.clone())
                        .with_inner_size(default_size)
                        .with_opacity(self.window_opacity)
                        .with_visible(true);
                    match ctx.create_window(config) {
                        Ok(handle) => {
                            let id = handle.id();
                            self.window_id = Some(id);
                            // Clone handle before moving into try_attach_gpu (which borrows self mutably)
                            let handle_for_gpu = handle.clone();
                            self.window = Some(handle);
                            // Single-window vertical slice: try real GPU attach with crossfont atlas.
                            // On headless CI this fails with NoCompatibleAdapter and we stay headless
                            // (deterministic fallback, no panic). On a real display we get a wgpu surface
                            // via winit's SurfaceTarget and present via tick.
                            self.try_attach_gpu(&handle_for_gpu);
                            eprintln!(
                                "bitty: window created id={} gpu={} crossfont={} focused={:?} leafs={}",
                                id.get(),
                                self.runtime.has_gpu(),
                                self.runtime.is_crossfont(),
                                self.runtime.focused_view(),
                                self.runtime.leaf_count()
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "bitty: window creation failed ({err}) — continuing headless (no GPU, no display)"
                            );
                        }
                    }
                }
                // Resumed is a good point to request the first redraw.
                if let Some(win) = self.window.as_ref() {
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
                            let physical = self.window.as_ref().map(|win| win.inner_size());
                            self.runtime.apply_dpi_scale(factor.get(), physical);
                            let snap = self.runtime.snapshot();
                            eprintln!(
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
                            );
                        }
                        if let Some(win) = self.window.as_ref() {
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
                if self.drive_tick().is_some() {
                    if let Some(win) = self.window.as_ref() {
                        win.request_redraw();
                    }
                }
            }
            PlatformEvent::PtyReadable => {
                // Evented PTY wakeup: the top-of-handler `poll_pty_pump`
                // already drained the bounded forwarder channel, so tick when
                // damage exists and request a redraw only on present
                // (frame-on-demand; quiet shells idle with no further wakes).
                if self.drive_tick().is_some() {
                    if let Some(win) = self.window.as_ref() {
                        win.request_redraw();
                    }
                }
            }
            PlatformEvent::Exiting => {
                ctx.exit();
            }
            _ => {}
        }
    }
}

// The `bitty-pty` bounded-channel seam uses `READ_CHUNK_SIZE` and
// `CHANNEL_CAPACITY_CHUNKS` constants, but we keep the demo pump's channel
// capacity literal (16) mirroring that constant without importing the crate
// directly — `bitty-app` wires `bitty-runtime` + `bitty-platform` +
// `bitty-render` + `bitty-config` as the thin composition root (ADR-0003
// entry point; no business logic beyond wiring). The literal is documented
// here to avoid a hidden dependency.
#[allow(dead_code)]
fn _assert_channel_capacity_is_documented() {
    const EXPECTED: usize = 16;
    const { assert!(EXPECTED > 0) }
}
