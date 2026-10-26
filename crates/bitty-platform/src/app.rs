//! Application entry point: event-loop driver, handler trait, and window
//! management exposed through owned types only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize as WinitLogicalSize;
use winit::error::EventLoopError;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window as WinitWindow, WindowAttributes, WindowId as WinitWindowId};

use crate::dpi::{LogicalSize, PhysicalSize, ScaleFactor};
use crate::error::PlatformError;
use crate::event::{
    PlatformEvent, WindowEventKind, WindowId, map_occluded, translate_window_event,
};
use crate::surface::SurfaceTarget;

/// User-event payload carried by the winit event loop.
///
/// The loop is built with this type so a cross-thread
/// [`EventWaker`] can wake it when PTY bytes become readable (mirroring
/// alacritty's `event_proxy.send_event(Event::Wakeup)` pattern, implemented
/// natively here without copying reference code).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WakeEvent {
    /// PTY output is readable; the handler should drain its bounded channel.
    PtyReadable,
}

/// Thread-safe wakeup handle for PTY readability.
///
/// Cloned into the PTY forwarder thread; calling [`EventWaker::wake_pty`]
/// wakes the event loop exactly once per call (coalescing applies: many
/// queued chunks still surface as one [`PlatformEvent::PtyReadable`]
/// delivery at most, and the handler must drain all available chunks).
/// Sending after the loop closed is a silent no-op so pump shutdown never
/// panics.
#[derive(Clone, Debug)]
pub struct EventWaker {
    proxy: winit::event_loop::EventLoopProxy<WakeEvent>,
}

impl EventWaker {
    /// Wakes the loop with a PTY-readability signal (best-effort).
    pub fn wake_pty(&self) {
        let _ = self.proxy.send_event(WakeEvent::PtyReadable);
    }

    /// Alias for readability wakeup.
    pub fn wake(&self) {
        self.wake_pty();
    }
}

/// Receives translated [`PlatformEvent`]s for the lifetime of [`App::run`].
///
/// Implementations decide application behavior; the platform layer owns all
/// OS interaction. Every callback receives an [`EventContext`] through which
/// windows are created ([`EventContext::create_window`]) and the loop is
/// controlled ([`EventContext::exit`], [`EventContext::set_poll`],
/// [`EventContext::set_wait`]).
pub trait AppHandler {
    /// Handles one platform event.
    ///
    /// The first delivery is always [`PlatformEvent::Resumed`]; creating the
    /// initial window there is the intended startup pattern.
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent);

    /// Receives the cross-thread wakeup handle before the loop starts.
    ///
    /// The default is a no-op so headless/test handlers keep working;
    /// PTY-owning handlers forward it into their bounded drain pump.
    fn set_event_waker(&mut self, _waker: EventWaker) {}
}

/// Entry point that drives a handler on the OS event loop.
///
/// Consuming the returned error is mandatory: on machines without a window
/// system this returns [`PlatformError::DisplayUnavailable`] instead of
/// aborting the process, which keeps headless CI deterministic.
#[derive(Debug, Default)]
pub struct App;

impl App {
    /// Runs `handler` until [`EventContext::exit`] ends the loop or the OS
    /// terminates it.
    ///
    /// # Errors
    ///
    /// - [`PlatformError::DisplayUnavailable`] when no window system exists.
    /// - [`PlatformError::EventLoopRun`] when a created loop fails afterwards.
    pub fn run<H: AppHandler>(mut handler: H) -> Result<(), PlatformError> {
        let event_loop = EventLoop::<WakeEvent>::with_user_event()
            .build()
            .map_err(map_loop_error)?;
        // Hand the cross-thread waker to the handler before the loop starts
        // so a PTY forwarder thread can wake on readability. Handlers that
        // ignore it keep the previous incidental-wakeup behavior.
        let waker = EventWaker {
            proxy: event_loop.create_proxy(),
        };
        handler.set_event_waker(waker);
        let mut adapter = Adapter::new(handler);
        event_loop
            .run_app(&mut adapter)
            .map_err(|error| PlatformError::EventLoopRun(error.to_string()))?;
        Ok(())
    }
}

fn map_loop_error(error: EventLoopError) -> PlatformError {
    match &error {
        EventLoopError::NotSupported(_) | EventLoopError::Os(_) => {
            PlatformError::DisplayUnavailable(error.to_string())
        }
        // `RecreationAttempt` cannot occur (one loop per process here) and
        // `ExitFailure` only surfaces from `run_app`; both are runtime-loop
        // failures, so they map to the same owned variant.
        _ => PlatformError::EventLoopRun(error.to_string()),
    }
}

/// Loop- and window-facing operations granted to an [`AppHandler`].
///
/// Borrowed per callback; methods are cheap and never block.
#[derive(Debug)]
pub struct EventContext<'a> {
    backend: EventContextBackend<'a>,
}

#[derive(Debug)]
enum EventContextBackend<'a> {
    Live {
        event_loop: &'a ActiveEventLoop,
        registry: &'a mut Registry,
    },
    #[cfg(feature = "input-vm-test-support")]
    Recording(&'a std::cell::RefCell<Vec<RecordingOperation>>),
}

/// Loop requests recorded without constructing an OS event loop or window.
#[cfg(feature = "input-vm-test-support")]
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordingOperation {
    /// Window creation was requested and refused with `WindowCreation`.
    WindowCreationRefused,
    /// Finish the event loop.
    Exit,
    /// Busy-poll.
    Poll,
    /// Wait indefinitely.
    Wait,
    /// Wait until the given monotonic deadline.
    WaitUntil(std::time::Instant),
}

impl EventContext<'_> {
    /// Constructs an in-memory context for explicitly opted-in handler tests.
    /// This creates no registry, window, OS loop, proxy, or desktop sink.
    #[cfg(feature = "input-vm-test-support")]
    #[doc(hidden)]
    pub fn recording(operations: &std::cell::RefCell<Vec<RecordingOperation>>) -> EventContext<'_> {
        EventContext {
            backend: EventContextBackend::Recording(operations),
        }
    }

    /// Creates a window according to `config`.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::WindowCreation`] when the platform refuses
    /// the request; the upstream diagnostic text is preserved.
    pub fn create_window(&mut self, config: WindowConfig) -> Result<WindowHandle, PlatformError> {
        let (event_loop, registry) = match &mut self.backend {
            EventContextBackend::Live {
                event_loop,
                registry,
            } => (event_loop, registry),
            #[cfg(feature = "input-vm-test-support")]
            EventContextBackend::Recording(operations) => {
                operations
                    .borrow_mut()
                    .push(RecordingOperation::WindowCreationRefused);
                return Err(PlatformError::WindowCreation(
                    "recording context refuses native window creation".to_string(),
                ));
            }
        };
        let blur_radius = config.blur_radius();
        let attributes = config.into_attributes();
        let window = event_loop
            .create_window(attributes)
            .map_err(|error| PlatformError::WindowCreation(error.to_string()))?;

        // Apply background blur if requested (CTX-0832)
        if blur_radius > 0 {
            crate::blur::apply_blur(&window, blur_radius);
        }

        let id = registry.register(&window.id());
        let occlusion_probe = Arc::new(Mutex::new(OcclusionProbe::default()));
        registry.register_probe(&window.id(), Arc::clone(&occlusion_probe));
        Ok(WindowHandle {
            id,
            window: Arc::new(window),
            occlusion_probe,
        })
    }

    /// Requests that the event loop finish after the current iteration.
    pub fn exit(&self) {
        match &self.backend {
            EventContextBackend::Live { event_loop, .. } => event_loop.exit(),
            #[cfg(feature = "input-vm-test-support")]
            EventContextBackend::Recording(operations) => {
                operations.borrow_mut().push(RecordingOperation::Exit);
            }
        }
    }

    /// Puts the loop into busy-polling mode (continuous redraw loops).
    pub fn set_poll(&self) {
        self.set_control_flow(ControlFlow::Poll);
    }

    /// Puts the loop back into energy-saving wait mode (default).
    pub fn set_wait(&self) {
        self.set_control_flow(ControlFlow::Wait);
    }

    /// Waits until `deadline` and then delivers
    /// [`PlatformEvent::AboutToWait`] (CTX-0334 timed activation).
    ///
    /// Lets the application schedule a single future wake without
    /// busy-polling — the hover dwell delay uses it so focus still applies
    /// when the pointer stops moving and no further input events arrive.
    pub fn set_wait_until(&self, deadline: std::time::Instant) {
        self.set_control_flow(ControlFlow::WaitUntil(deadline));
    }

    fn set_control_flow(&self, control_flow: ControlFlow) {
        match &self.backend {
            EventContextBackend::Live { event_loop, .. } => {
                event_loop.set_control_flow(control_flow)
            }
            #[cfg(feature = "input-vm-test-support")]
            EventContextBackend::Recording(operations) => {
                operations.borrow_mut().push(match control_flow {
                    ControlFlow::Poll => RecordingOperation::Poll,
                    ControlFlow::Wait => RecordingOperation::Wait,
                    ControlFlow::WaitUntil(deadline) => RecordingOperation::WaitUntil(deadline),
                });
            }
        }
    }
}

/// Point-in-time view of a window's redraw-starvation probe (issue #2570).
///
/// The probe is the Wayland occlusion source that remains when the compositor
/// never sets the xdg-shell v6 `suspended` state, which is the only case where
/// the vendored winit patch (CTX-1605) cannot deliver `Occluded`. Reporting
/// its state from a stalled main loop makes the next freeze
/// self-describing: a large `pending` span names a redraw the compositor
/// never answered, while `occluded` names an edge the application already
/// consumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OcclusionSnapshot {
    /// How long the most recent outstanding [`WindowHandle::request_redraw`]
    /// has been pending, if one is outstanding.
    pub pending: Option<std::time::Duration>,
    /// Whether a frame callback has ever been served: the probe is armed
    /// only after the surface proved it can accept redraws.
    pub redraw_seen: bool,
    /// Whether the synthetic or native occlusion edge is currently
    /// delivered.
    pub occluded: bool,
}

/// Owned handle to a live window.
///
/// Cloning shares the same window; the window closes when the last handle
/// drops or the process exits.
#[derive(Clone)]
pub struct WindowHandle {
    id: WindowId,
    window: Arc<WinitWindow>,
    /// Redraw-starvation probe shared with the loop adapter: `request_redraw`
    /// marks it, the adapter resolves the starved pending request into the
    /// synthetic Wayland `Occluded` edge (issue #2570 follow-up).
    occlusion_probe: Arc<Mutex<OcclusionProbe>>,
}

impl WindowHandle {
    /// This crate's stable identity for the window.
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// Sets the OS window title (CTX-0382).
    ///
    /// Wraps winit's `Window::set_title`. Callers pass an already-sanitized,
    /// bounded string (see the app-side title sanitizer) so untrusted PTY
    /// output cannot inject control sequences into the titlebar.
    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    /// Sets the OS mouse-pointer cursor icon (issue #1762, `OSC 22`).
    ///
    /// Wraps winit's `Window::set_cursor`: the owned [`crate::CursorIcon`]
    /// maps 1:1 to `winit::window::CursorIcon`, so untrusted PTY shape names
    /// (already validated to the CSS keyword set by `bitty-vt`) can never
    /// inject an invalid icon. Fail-soft by construction — winit's setter is
    /// infallible and platforms without a distinct icon fall back to the
    /// default pointer.
    pub fn set_cursor_icon(&self, icon: crate::CursorIcon) {
        self.window.set_cursor(icon.to_winit());
    }

    /// Schedules a redraw request delivery for this window.
    ///
    /// The application observes it as
    /// [`PlatformEvent::Window`]` with `[`WindowEventKind::RedrawRequested`].
    ///
    /// Issue #2570 follow-up: the call also marks the occlusion probe's
    /// pending request; the adapter resolves it into the synthetic Wayland
    /// `Occluded` edge when the frame callback never comes (a compositor
    /// stops serving frame callbacks for a suspended xdg surface).
    pub fn request_redraw(&self) {
        if let Ok(mut probe) = self.occlusion_probe.lock() {
            probe.note_redraw_request(std::time::Instant::now());
        }
        self.window.request_redraw();
    }

    /// Samples the redraw-starvation probe for diagnostics (issue #2570).
    ///
    /// Read-only: the snapshot never mutates probe state and never affects
    /// the synthetic occlusion edges. The lock may block the calling thread
    /// briefly while the loop adapter updates the probe; this accessor is
    /// intended for the event-loop thread (the watchdog context sampler).
    /// A monitor or watchdog caller that cannot afford to wait must use its
    /// own `try_lock` path. A poisoned probe yields the default snapshot.
    pub fn occlusion_snapshot(&self) -> OcclusionSnapshot {
        self.occlusion_probe
            .lock()
            .map(|probe| probe.snapshot(std::time::Instant::now()))
            .unwrap_or_default()
    }

    /// Enables or disables platform IME events for this window (CTX-0367).
    ///
    /// Wraps winit's `Window::set_ime_allowed`: `true` makes the window
    /// receive [`WindowEventKind::Ime`]
    /// preedit/commit events (Wayland text-input-v3, X11 XIM, macOS
    /// NSTextInputClient, Windows IMM) and makes the platform suppress raw
    /// keyboard events during composition. IME is disabled by default in
    /// winit, so a terminal must opt in for CJK input to work.
    pub fn set_ime_allowed(&self, allowed: bool) {
        self.window.set_ime_allowed(allowed);
    }

    /// Places the platform IME preedit/candidate window at a physical-pixel
    /// rectangle (CTX-0367).
    ///
    /// Wraps winit's `Window::set_ime_cursor_area`; callers pass the live
    /// cursor cell origin and cell size already scaled by the DPI factor.
    /// Coordinates are window-relative physical pixels, matching winit's
    /// `PhysicalPosition<i32>`/`PhysicalSize<u32>` contract.
    pub fn set_ime_cursor_area(&self, x: i32, y: i32, width: u32, height: u32) {
        self.window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(x, y),
            winit::dpi::PhysicalSize::new(width, height),
        );
    }

    /// Live-applies a window opacity without restart (CTX-0223).
    ///
    /// This is the `window.opacity` side of the `Live` reload class: it
    /// toggles the winit `transparent` flag to match the sanitized opacity
    /// (see [`opacity_requests_transparency`]). Fail-soft by construction —
    /// winit's setter is infallible and platforms without compositor
    /// blending ignore the flag, so an unsupported platform keeps an opaque
    /// window instead of erroring.
    ///
    /// This only declares the surface blendable; the pixels themselves must
    /// carry the alpha. Renderer alpha scaling (CTX-0290) is configured on
    /// the GPU surface at attach (`Surface::configure_with_opacity`); the
    /// `Live` reload path (CTX-0898) re-applies both halves at runtime: the
    /// renderer through `Runtime::set_window_opacity` and this hint from the
    /// app. winit can only set the hint at creation on X11, so there a window
    /// that started opaque keeps its opaque hint until restart.
    ///
    /// Returns whether transparency was requested (for logging/diagnostics).
    pub fn set_opacity(&self, opacity: f32) -> bool {
        let transparent = opacity_requests_transparency(opacity);
        self.window.set_transparent(transparent);
        transparent
    }

    /// Current inner size in physical pixels.
    pub fn inner_size(&self) -> PhysicalSize {
        let size = self.window.inner_size();
        PhysicalSize::new(size.width, size.height)
    }

    /// Current DPI scale factor, sanitized to a valid range.
    pub fn scale_factor(&self) -> ScaleFactor {
        ScaleFactor::new_sanitized(self.window.scale_factor())
    }

    /// Derives a [`SurfaceTarget`] for GPU renderer attachment.
    ///
    /// The target shares this handle's window, so it stays valid as long as
    /// any clone of it lives; see the [`surface`] module lifetime contract.
    ///
    /// [`surface`]: crate::surface
    pub fn surface_target(&self) -> SurfaceTarget {
        SurfaceTarget::from_window(self.id, Arc::clone(&self.window))
    }

    /// DPI-size refresh hook: converts a logical size into physical pixels at
    /// the current scale factor.
    ///
    /// Convenience twin of
    /// [`LogicalSize::to_physical`]`(`[`WindowHandle::scale_factor`]`())` for
    /// handlers that keep window geometry in logical units.
    pub fn logical_to_physical(&self, size: LogicalSize) -> PhysicalSize {
        size.to_physical(self.scale_factor())
    }
}

/// Description of a window to create, mapped onto upstream attributes inside
/// the crate boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowConfig {
    title: String,
    inner_size: Option<LogicalSize>,
    min_inner_size: Option<LogicalSize>,
    max_inner_size: Option<LogicalSize>,
    resizable: bool,
    visible: bool,
    /// Window opacity `0.0..=1.0` (CTX-0223 `window.opacity`; default `1.0`
    /// = opaque). Always sanitized (see [`sanitize_opacity`]).
    opacity: f32,
    /// Background blur radius in logical pixels `0..=128` (CTX-0832).
    /// Platform support varies: Wayland (KDE/Hyprland), macOS NSVisualEffectView.
    /// Default `0` = no blur.
    blur_radius: u32,
}

/// Coerces any opacity into the honored range (CTX-0223).
///
/// Non-finite inputs become `1.0` (opaque); finite values clamp to
/// `[0.0, 1.0]`. Total for every `f32`, so untrusted config values can
/// never poison window creation or a live update.
#[must_use]
pub fn sanitize_opacity(opacity: f32) -> f32 {
    if !opacity.is_finite() {
        1.0
    } else {
        opacity.clamp(0.0, 1.0)
    }
}

/// Whether `opacity` needs a transparent window (CTX-0223).
///
/// winit 0.30 exposes no per-window opacity knob — translucency flows
/// through the `transparent` window flag (compositor-blended where the
/// platform supports it, ignored where it does not). Fully opaque windows
/// stay non-transparent so compositors keep the fast path.
///
/// On its own this flag only clears the Wayland opaque region; a visible
/// effect needs renderer-side alpha as well (CTX-0290), which the app
/// configures on the GPU surface with the same sanitized opacity.
#[must_use]
pub fn opacity_requests_transparency(opacity: f32) -> bool {
    sanitize_opacity(opacity) < 1.0
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: String::new(),
            inner_size: None,
            min_inner_size: None,
            max_inner_size: None,
            resizable: true,
            visible: true,
            opacity: 1.0,
            blur_radius: 0,
        }
    }
}

impl WindowConfig {
    /// Defaults: empty title, platform-chosen size, resizable, visible, and
    /// opaque.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the window title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Sets the requested inner size (logical pixels).
    pub fn with_inner_size(mut self, size: LogicalSize) -> Self {
        self.inner_size = Some(size);
        self
    }

    /// Sets the minimum inner size (logical pixels).
    pub fn with_min_inner_size(mut self, size: LogicalSize) -> Self {
        self.min_inner_size = Some(size);
        self
    }

    /// Sets the maximum inner size (logical pixels).
    pub fn with_max_inner_size(mut self, size: LogicalSize) -> Self {
        self.max_inner_size = Some(size);
        self
    }

    /// Enables or disables user resizing (default: enabled).
    pub fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Enables or disables initial visibility (default: visible).
    pub fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Sets the window opacity (CTX-0223 `window.opacity`).
    ///
    /// Sanitized via [`sanitize_opacity`], so out-of-range and non-finite
    /// inputs degrade to the nearest honored value instead of failing
    /// window creation. Values below `1.0` mark the window transparent at
    /// creation (see [`opacity_requests_transparency`]); `1.0` keeps the
    /// opaque fast path. The renderer must be given the same opacity
    /// (CTX-0290) for the value to have a visible effect.
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = sanitize_opacity(opacity);
        self
    }

    /// The configured (sanitized) window opacity.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Whether this config requests a transparent window.
    pub fn is_transparent(&self) -> bool {
        opacity_requests_transparency(self.opacity)
    }

    /// Sets the background blur radius (CTX-0832 `window.blur_radius`).
    ///
    /// Clamped to `0..=128` logical pixels. OQ-038 has no accepted blur
    /// contract, so `bitty-config` accepts only `0` today and this seam is
    /// reserved for a future implementation. Platform support varies:
    /// Wayland (KDE/Hyprland via blur protocol), macOS (NSVisualEffectView).
    pub fn with_blur_radius(mut self, radius: u32) -> Self {
        self.blur_radius = radius.min(128);
        self
    }

    /// The configured blur radius in logical pixels.
    pub fn blur_radius(&self) -> u32 {
        self.blur_radius
    }

    fn into_attributes(self) -> WindowAttributes {
        let transparent = opacity_requests_transparency(self.opacity);
        let mut attributes = WinitWindow::default_attributes()
            .with_title(self.title)
            .with_resizable(self.resizable)
            .with_visible(self.visible)
            .with_transparent(transparent);
        attributes = apply_optional_size(attributes, self.inner_size, SizeKind::Inner);
        attributes = apply_optional_size(attributes, self.min_inner_size, SizeKind::Min);
        attributes = apply_optional_size(attributes, self.max_inner_size, SizeKind::Max);
        attributes = apply_app_id(attributes);
        attributes
    }
}

/// The fixed Linux application ID (CTX-0452 / 034 item 7): Wayland `app_id`,
/// X11 `WM_CLASS`, the `.desktop` file name, and the AppStream metainfo `<id>`
/// all use this value. Recorded in `packaging/README.md`; the ID is never
/// renamed silently.
#[cfg(target_os = "linux")]
const APP_ID: &str = "run.bitty.Bitty";

/// Application identifier for window-system grouping.
///
/// Sets the Wayland `app_id` and the X11 `WM_CLASS` instance/general pair to
/// [`APP_ID`] so compositors (`hyprctl clients` → `class: run.bitty.Bitty`)
/// and the shipped `run.bitty.Bitty.desktop` (`StartupWMClass=run.bitty.Bitty`)
/// agree, and so the window identity matches the AppStream metainfo component
/// ID. Verified against the vendored winit 0.30.13 API: both
/// `winit::platform::wayland::WindowAttributesExtWayland::with_name` and
/// `winit::platform::x11::WindowAttributesExtX11::with_name` set the same
/// shared `platform_specific.name` field, so one value covers both
/// backends; both calls below are idempotent with identical arguments.
#[cfg(target_os = "linux")]
fn apply_app_id(attributes: WindowAttributes) -> WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland as WaylandExt;
    use winit::platform::x11::WindowAttributesExtX11 as X11Ext;
    let attributes = WaylandExt::with_name(attributes, APP_ID, APP_ID);
    X11Ext::with_name(attributes, APP_ID, APP_ID)
}

/// Non-Linux targets have no Wayland/X11 app-id concept; keep attributes
/// unchanged there (this also keeps the Windows cross-check green, where
/// neither `winit::platform` module exists).
#[cfg(not(target_os = "linux"))]
fn apply_app_id(attributes: WindowAttributes) -> WindowAttributes {
    attributes
}

enum SizeKind {
    Inner,
    Min,
    Max,
}

fn apply_optional_size(
    mut attributes: WindowAttributes,
    size: Option<LogicalSize>,
    kind: SizeKind,
) -> WindowAttributes {
    if let Some(size) = size {
        let logical = WinitLogicalSize::new(size.width().get(), size.height().get());
        match kind {
            SizeKind::Inner => attributes = attributes.with_inner_size(logical),
            SizeKind::Min => attributes = attributes.with_min_inner_size(logical),
            SizeKind::Max => attributes = attributes.with_max_inner_size(logical),
        }
    }
    attributes
}

/// Redraw-starvation probe for the synthetic Wayland occlusion edge
/// (issue #2570 follow-up for the `#2589` surface-visibility gate).
///
/// Unpatched winit 0.30.13 delivers no `WindowEvent::Occluded` on Wayland and
/// no `Suspended` lifecycle either, so the embedder's visibility gate cannot
/// engage on the platform #2570 reproduces on. CTX-1605 vendors that release
/// with the upstream #4710 backport, which maps the xdg-shell v6 `suspended`
/// state to `Occluded`; the probe stays as the fallback for compositors that
/// never set that state and for a surface whose frame callbacks keep arriving.
/// It exploits what the Wayland backend *does* guarantee: a `RedrawRequested`
/// dispatch is gated on the compositor's frame callback (`refresh_frame` /
/// `frame_callback_state`), and a compositor stops serving frame callbacks
/// for a suspended xdg surface (a Hyprland workspace switch-away). A pending
/// `request_redraw` that is unanswered for [`OCCLUSION_STARVE`] therefore
/// names an occluded surface without any protocol-level introspection.
#[derive(Debug, Clone, Copy, Default)]
struct OcclusionProbe {
    /// The last unanswered `request_redraw`.
    last_request: Option<std::time::Instant>,
    /// Whether a `RedrawRequested` was ever observed: the surface proved it
    /// can serve frame callbacks. The probe never fires before the first
    /// observation, so a surface that has not yet completed one frame cannot
    /// starve into a false occlusion.
    redraw_seen: bool,
    /// Whether the occluded edge is the currently delivered state.
    occluded: bool,
}

/// Redraw starvation beyond this span names an occluded surface.
///
/// A live Wayland surface serves its frame callback within one repaint cycle
/// (16 ms at 60 Hz); 300 ms tolerates dropped frames, compositor stalls, and
/// scheduling jitter with a large margin while staying far below the
/// seconds-long spans a Hyprland workspace switch-away suspends for.
const OCCLUSION_STARVE: std::time::Duration = std::time::Duration::from_millis(300);

impl OcclusionProbe {
    /// Read-only diagnostic snapshot (issue #2570).
    fn snapshot(&self, now: std::time::Instant) -> OcclusionSnapshot {
        OcclusionSnapshot {
            pending: self
                .last_request
                .map(|at| now.saturating_duration_since(at)),
            redraw_seen: self.redraw_seen,
            occluded: self.occluded,
        }
    }

    /// Records a new unanswered `request_redraw` (issue #2570 probe).
    fn note_redraw_request(&mut self, now: std::time::Instant) {
        self.last_request = Some(now);
    }

    /// Records a `RedrawRequested` delivery: the frame callback was served,
    /// so the surface can accept redraws again.
    fn note_redraw_delivered(&mut self) {
        self.last_request = None;
        self.redraw_seen = true;
    }

    /// Records a delivered occlusion edge (native delivery, e.g. iOS, X11, or
    /// the Wayland xdg-shell v6 `suspended` edge, or a synthetic starve edge
    /// from [`Self::starvation_edge`]).
    ///
    /// A reveal also clears the starvation baseline: the request that was
    /// outstanding while the surface was occluded must not be re-read as a
    /// fresh starvation after the native reveal, or the synthetic edge would
    /// immediately re-close the gate the reveal just opened. A reveal does not
    /// imply that a redraw was delivered, so `redraw_seen` is left untouched.
    fn note_occluded_state(&mut self, occluded: bool) {
        self.occluded = occluded;
        if !occluded {
            self.last_request = None;
        }
    }

    /// The synthetic occlusion edge when the pending redraw request starved
    /// long enough to name a suspended surface — at most one discovery.
    fn starvation_edge(&mut self, now: std::time::Instant) -> Option<bool> {
        if self.occluded || !self.redraw_seen {
            return None;
        }
        let requested_at = self.last_request?;
        if now.duration_since(requested_at) < OCCLUSION_STARVE {
            return None;
        }
        self.occluded = true;
        Some(true)
    }

    /// The reveal edge for a qualifying event, at most one discovery.
    ///
    /// Reveal events are the ones a re-shown surface delivers: a served frame
    /// callback (`RedrawRequested` — the same event also clears the pending
    /// request), keyboard focus regain, or a real size change.
    fn reveal_edge(&mut self, event: &winit::event::WindowEvent) -> Option<bool> {
        if !self.occluded {
            return None;
        }
        let reveals = match event {
            winit::event::WindowEvent::RedrawRequested => {
                self.note_redraw_delivered();
                true
            }
            winit::event::WindowEvent::Focused(true)
            | winit::event::WindowEvent::Resized(_)
            | winit::event::WindowEvent::ScaleFactorChanged { .. } => true,
            _ => false,
        };
        if reveals {
            self.occluded = false;
            Some(false)
        } else {
            None
        }
    }
}

/// Bidirectional mapping between upstream window ids and owned ids.
#[derive(Debug, Default)]
struct Registry {
    ids: HashMap<WinitWindowId, WindowId>,
    probes: HashMap<WinitWindowId, Arc<Mutex<OcclusionProbe>>>,
    next_raw_id: u64,
}

impl Registry {
    fn register(&mut self, upstream: &WinitWindowId) -> WindowId {
        self.next_raw_id += 1;
        let owned = WindowId::from_raw(self.next_raw_id);
        self.ids.insert(*upstream, owned);
        owned
    }

    fn register_probe(&mut self, upstream: &WinitWindowId, probe: Arc<Mutex<OcclusionProbe>>) {
        self.probes.insert(*upstream, probe);
    }

    fn unregister(&mut self, upstream: &WinitWindowId) {
        self.ids.remove(upstream);
        self.probes.remove(upstream);
    }

    fn lookup(&self, upstream: &WinitWindowId) -> Option<WindowId> {
        self.ids.get(upstream).copied()
    }
}

/// Internal glue implementing the upstream application protocol by
/// translating every event before it reaches the user handler.
struct Adapter<H: AppHandler> {
    handler: H,
    registry: Registry,
}

impl<H: AppHandler> Adapter<H> {
    fn new(handler: H) -> Self {
        Self {
            handler,
            registry: Registry::default(),
        }
    }

    fn dispatch(&mut self, event_loop: &ActiveEventLoop, event: PlatformEvent) {
        let Self { handler, registry } = self;
        let mut ctx = EventContext {
            backend: EventContextBackend::Live {
                event_loop,
                registry,
            },
        };
        handler.handle_event(&mut ctx, event);
    }

    /// Delivers the synthetic Wayland occlusion edges for every window whose
    /// pending redraw request starved past [`OCCLUSION_STARVE`].
    ///
    /// Native `Occluded` deliveries (iOS, X11, and Wayland when the compositor
    /// sets xdg-shell v6 `suspended`) are translated straight to the same
    /// [`WindowEventKind::Occluded`] event, so the embedder sees one edge
    /// vocabulary on every platform. The synthetic delivery below is the
    /// fallback for a surface that starves without a native edge; the probe
    /// state keeps the two from ever double-delivering.
    fn synthesize_occlusion(&mut self, event_loop: &ActiveEventLoop) {
        // Collect first: the probes and the id lookup are separate registry
        // maps, and `dispatch` borrows the registry mutably again.
        let mut edges: Vec<(WindowId, bool)> = Vec::new();
        for (upstream, probe) in self.registry.probes.iter() {
            let edge = match probe.lock() {
                Ok(mut state) => state.starvation_edge(std::time::Instant::now()),
                Err(_) => None,
            };
            if edge == Some(true) {
                if let Some(owned) = self.registry.lookup(upstream) {
                    edges.push((owned, true));
                }
            }
        }
        for (window_id, occluded) in edges {
            self.dispatch(
                event_loop,
                PlatformEvent::Window {
                    window_id,
                    kind: map_occluded(occluded),
                },
            );
        }
    }
}

impl<H: AppHandler> ApplicationHandler<WakeEvent> for Adapter<H> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch(event_loop, PlatformEvent::Resumed);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WakeEvent) {
        match event {
            WakeEvent::PtyReadable => {
                self.dispatch(event_loop, PlatformEvent::PtyReadable);
            }
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch(event_loop, PlatformEvent::Suspended);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Issue #2570 follow-up: resolve starved redraw requests into the
        // synthetic Wayland occlusion edge before the handler runs, so its
        // `Occluded` handling sits ahead of any present path this turn.
        self.synthesize_occlusion(event_loop);
        self.dispatch(event_loop, PlatformEvent::AboutToWait);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch(event_loop, PlatformEvent::Exiting);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WinitWindowId,
        event: winit::event::WindowEvent,
    ) {
        let destroyed = matches!(event, winit::event::WindowEvent::Destroyed);
        if destroyed {
            // Stop routing further events to a dead window even if delivery
            // below is filtered out for some reason.
            self.registry.unregister(&window_id);
        }
        // Issue #2570 follow-up: run the occlusion probe before the event
        // moves into translation. A frame callback served while occluded
        // clears the pending request and yields the reveal edge; reveal
        // events (focus regain, size change) yield it without a callback.
        let reveal_edge = {
            let probe = self.registry.probes.get(&window_id).cloned();
            probe.and_then(|probe| {
                let mut state = probe.lock().ok()?;
                // Every served frame callback proves the surface can accept
                // redraws and clears the pending request — the probe's arm
                // point and its un-mark, regardless of occlusion state.
                if matches!(event, winit::event::WindowEvent::RedrawRequested) {
                    state.note_redraw_delivered();
                }
                state.reveal_edge(&event)
            })
        };
        if reveal_edge == Some(false) {
            if let Some(owned_id) = self.registry.lookup(&window_id) {
                self.dispatch(
                    event_loop,
                    PlatformEvent::Window {
                        window_id: owned_id,
                        kind: map_occluded(false),
                    },
                );
            }
        }
        // CTX-0951: OS appearance is app-global, not window-scoped.
        // `translate_window_event` filters `ThemeChanged`; re-dispatch it
        // here as one app-level event per reporting window (downstream
        // apply dedups, so multi-window duplicates are no-ops). Foreign
        // windows stay dropped like every other event: no registry entry,
        // no dispatch.
        if let winit::event::WindowEvent::ThemeChanged(theme) = &event {
            if self.registry.lookup(&window_id).is_some() {
                self.dispatch(
                    event_loop,
                    crate::appearance::map_appearance_changed(*theme),
                );
            }
            return;
        }
        let Some(kind) = translate_window_event(event) else {
            return;
        };
        // Issue #2570 follow-up: the probe tracks the delivered occlusion
        // state exactly, so a native edge (iOS, X11, Wayland `suspended`) and
        // the synthetic starve edge never double-deliver; a native reveal also
        // clears the starvation baseline so the stale request cannot resurface.
        if let WindowEventKind::Occluded(occluded) = &kind {
            if let Some(probe) = self.registry.probes.get(&window_id) {
                if let Ok(mut state) = probe.lock() {
                    state.note_occluded_state(*occluded);
                }
            }
        }
        let Some(owned_id) = self.registry.lookup(&window_id) else {
            // Events for windows this crate did not create are dropped; there
            // is no stable way to name them for the application.
            return;
        };
        self.dispatch(
            event_loop,
            PlatformEvent::Window {
                window_id: owned_id,
                kind,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_config_defaults_and_builder_chain() {
        let config = WindowConfig::new()
            .with_title("bitty")
            .with_inner_size(LogicalSize::new(800.0, 600.0).expect("valid"))
            .with_min_inner_size(LogicalSize::new(320.0, 240.0).expect("valid"))
            .with_max_inner_size(LogicalSize::new(3840.0, 2160.0).expect("valid"))
            .with_resizable(false)
            .with_visible(false)
            .with_opacity(0.9);

        assert_eq!(
            config,
            WindowConfig {
                title: String::from("bitty"),
                inner_size: Some(LogicalSize::new(800.0, 600.0).expect("valid")),
                min_inner_size: Some(LogicalSize::new(320.0, 240.0).expect("valid")),
                max_inner_size: Some(LogicalSize::new(3840.0, 2160.0).expect("valid")),
                resizable: false,
                visible: false,
                opacity: 0.9,
                blur_radius: 0,
            }
        );
        assert_eq!(WindowConfig::default(), WindowConfig::new());
        // Default is opaque with the fast (non-transparent) path.
        assert_eq!(WindowConfig::default().opacity(), 1.0);
        assert!(!WindowConfig::default().is_transparent());
        assert!(config.is_transparent());
    }

    #[test]
    fn opacity_sanitize_is_total_and_fail_soft() {
        // CTX-0223: every f32 maps somewhere honored; non-finite degrades
        // to opaque, out-of-range clamps, and only sub-1.0 requests
        // transparency (unsupported platforms ignore the flag and stay
        // opaque — the fail-soft path).
        assert_eq!(sanitize_opacity(1.0), 1.0);
        assert_eq!(sanitize_opacity(0.0), 0.0);
        assert_eq!(sanitize_opacity(0.95), 0.95);
        assert_eq!(sanitize_opacity(-0.5), 0.0);
        assert_eq!(sanitize_opacity(2.0), 1.0);
        assert_eq!(sanitize_opacity(f32::NAN), 1.0);
        assert_eq!(sanitize_opacity(f32::INFINITY), 1.0);
        assert_eq!(sanitize_opacity(f32::NEG_INFINITY), 1.0);
        assert!(!opacity_requests_transparency(1.0));
        assert!(!opacity_requests_transparency(2.0));
        assert!(!opacity_requests_transparency(f32::NAN));
        assert!(opacity_requests_transparency(0.99));
        assert!(opacity_requests_transparency(0.0));
        // The builder sanitizes at the boundary, so creation never fails.
        assert_eq!(WindowConfig::new().with_opacity(2.0).opacity(), 1.0);
        assert_eq!(WindowConfig::new().with_opacity(f32::NAN).opacity(), 1.0);
        assert!(!WindowConfig::new().with_opacity(2.0).is_transparent());
        assert!(WindowConfig::new().with_opacity(0.5).is_transparent());
    }

    /// Installs a probe through the live `request_redraw` path in memento
    /// form: `window.request_redraw + time passed` is the only way a real
    /// starvation develops.
    #[test]
    fn probe_never_fires_before_first_delivered_redraw() {
        let mut probe = OcclusionProbe::default();
        let t0 = std::time::Instant::now();
        probe.note_redraw_request(t0);
        assert_eq!(
            probe.starvation_edge(t0 + OCCLUSION_STARVE * 10),
            None,
            "a surface that has not yet served one redraw cannot starve into occlusion"
        );
        probe.note_redraw_delivered();
        assert_eq!(
            probe.starvation_edge(t0 + OCCLUSION_STARVE * 10),
            None,
            "with no pending request the probe stays quiet"
        );
    }

    #[test]
    fn native_reveal_clears_the_stale_starve_baseline() {
        // Issue #2570 follow-up (CTX-1605 review F3): the Wayland backend now
        // delivers native `Occluded` edges, so a request that was outstanding
        // when the surface suspended must not resurface as a synthetic
        // occlusion right after the native reveal. Otherwise the next
        // `about_to_wait` closes the gate the reveal just opened and keeps it
        // closed until the resume frame callback arrives.
        let mut probe = OcclusionProbe::default();
        let t0 = std::time::Instant::now();
        probe.note_redraw_request(t0);
        probe.note_redraw_delivered();
        let pending = t0 + std::time::Duration::from_millis(1);
        probe.note_redraw_request(pending);
        probe.note_occluded_state(true);
        probe.note_occluded_state(false);
        assert!(
            !probe.occluded,
            "the native reveal must win over the stale in-flight request"
        );
        assert_eq!(
            probe.starvation_edge(pending + OCCLUSION_STARVE * 2),
            None,
            "a stale pre-suspension request must not synthesize a second occlusion"
        );
    }

    #[test]
    fn probe_starve_edge_and_single_fire() {
        let mut probe = OcclusionProbe::default();
        let t0 = std::time::Instant::now();
        probe.note_redraw_request(t0);
        probe.note_redraw_delivered();
        let pending = t0 + std::time::Duration::from_millis(1);
        probe.note_redraw_request(pending);
        assert_eq!(
            probe.starvation_edge(pending),
            None,
            "a fresh request inside the starve span is healthy"
        );
        assert_eq!(
            probe.starvation_edge(pending + OCCLUSION_STARVE),
            Some(true),
            "one repaint cycle cannot reach the span; starvation past it names occlusion"
        );
        assert_eq!(
            probe.starvation_edge(pending + OCCLUSION_STARVE * 2),
            None,
            "the edge is discovered once"
        );
    }

    #[test]
    fn probe_snapshot_names_the_pending_redraw_and_state() {
        // Issue #2570 observability: a watchdog sample must distinguish "no
        // redraw outstanding", "redraw pending for N ms", and "edge already
        // delivered" without touching the probe's transition state.
        let mut probe = OcclusionProbe::default();
        let t0 = std::time::Instant::now();
        probe.note_redraw_request(t0);
        let pending = probe.snapshot(t0 + std::time::Duration::from_millis(750));
        assert_eq!(pending.pending, Some(std::time::Duration::from_millis(750)));
        assert!(!pending.redraw_seen);
        assert!(!pending.occluded);

        probe.note_redraw_delivered();
        let served = probe.snapshot(t0 + std::time::Duration::from_millis(900));
        assert_eq!(served.pending, None);
        assert!(served.redraw_seen);
        assert!(!served.occluded);

        let maybe_request = t0 + std::time::Duration::from_millis(900);
        probe.note_redraw_request(maybe_request);
        assert!(
            probe
                .starvation_edge(maybe_request + OCCLUSION_STARVE)
                .unwrap_or(false)
        );
        let occluded = probe.snapshot(maybe_request + OCCLUSION_STARVE);
        assert!(occluded.occluded);
        assert_eq!(occluded.pending, Some(OCCLUSION_STARVE));

        // Sampling is read-only: the starvation edge cannot fire twice.
        assert_eq!(
            probe.starvation_edge(maybe_request + OCCLUSION_STARVE * 2),
            None,
            "a snapshot must not consume or re-arm the edge"
        );
    }

    #[test]
    fn probe_reveal_edges() {
        let mut probe = OcclusionProbe::default();
        probe.note_redraw_delivered();
        probe.note_redraw_request(std::time::Instant::now());
        assert!(
            probe
                .starvation_edge(std::time::Instant::now() + OCCLUSION_STARVE)
                .unwrap_or(false)
        );
        assert!(
            probe.occluded,
            "starvation must arm the probe before reveal edges apply"
        );

        // A served frame callback clears the pending request and reveals.
        assert_eq!(
            probe.reveal_edge(&winit::event::WindowEvent::RedrawRequested),
            Some(false)
        );
        assert!(!probe.occluded);
        assert_eq!(
            probe.last_request, None,
            "the served callback must clear the pending request"
        );

        // Focus regain reveals without a callback.
        probe.occluded = true;
        assert_eq!(
            probe.reveal_edge(&winit::event::WindowEvent::Focused(true)),
            Some(false)
        );
        assert!(!probe.occluded);

        // A size change reveals without a callback.
        probe.occluded = true;
        assert_eq!(
            probe.reveal_edge(&winit::event::WindowEvent::Resized(
                winit::dpi::PhysicalSize::new(1280, 800)
            )),
            Some(false)
        );
        assert!(!probe.occluded);

        // Non-reveal events keep the occluded state.
        probe.occluded = true;
        assert_eq!(
            probe.reveal_edge(&winit::event::WindowEvent::Focused(false)),
            None
        );
        assert_eq!(
            probe.reveal_edge(&winit::event::WindowEvent::CursorLeft {
                device_id: winit::event::DeviceId::dummy(),
            }),
            None
        );
        assert!(probe.occluded);
    }

    #[test]
    fn probe_native_occluded_state_syncs_the_starve_edge() {
        let mut probe = OcclusionProbe::default();
        probe.note_occluded_state(true);
        assert_eq!(
            probe.starvation_edge(std::time::Instant::now() + OCCLUSION_STARVE),
            None,
            "a natively delivered occlusion must not double-deliver via the starve edge"
        );
        probe.note_occluded_state(false);
        assert_eq!(
            probe.reveal_edge(&winit::event::WindowEvent::Focused(true)),
            None,
            "an un-occluded probe has no reveal edge to deliver"
        );
    }
}
