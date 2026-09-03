//! Application entry point: event-loop driver, handler trait, and window
//! management exposed through owned types only.

use std::collections::HashMap;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize as WinitLogicalSize;
use winit::error::EventLoopError;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window as WinitWindow, WindowAttributes, WindowId as WinitWindowId};

use crate::dpi::{LogicalSize, PhysicalSize, ScaleFactor};
use crate::error::PlatformError;
use crate::event::{PlatformEvent, WindowId, translate_window_event};
use crate::surface::SurfaceTarget;

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
    pub fn run<H: AppHandler>(handler: H) -> Result<(), PlatformError> {
        let event_loop = EventLoop::builder().build().map_err(map_loop_error)?;
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
    event_loop: &'a ActiveEventLoop,
    registry: &'a mut Registry,
}

impl EventContext<'_> {
    /// Creates a window according to `config`.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::WindowCreation`] when the platform refuses
    /// the request; the upstream diagnostic text is preserved.
    pub fn create_window(&mut self, config: WindowConfig) -> Result<WindowHandle, PlatformError> {
        let attributes = config.into_attributes();
        let window = self
            .event_loop
            .create_window(attributes)
            .map_err(|error| PlatformError::WindowCreation(error.to_string()))?;
        let id = self.registry.register(&window.id());
        Ok(WindowHandle {
            id,
            window: Arc::new(window),
        })
    }

    /// Requests that the event loop finish after the current iteration.
    pub fn exit(&self) {
        self.event_loop.exit();
    }

    /// Puts the loop into busy-polling mode (continuous redraw loops).
    pub fn set_poll(&self) {
        self.event_loop.set_control_flow(ControlFlow::Poll);
    }

    /// Puts the loop back into energy-saving wait mode (default).
    pub fn set_wait(&self) {
        self.event_loop.set_control_flow(ControlFlow::Wait);
    }
}

/// Owned handle to a live window.
///
/// Cloning shares the same window; the window closes when the last handle
/// drops or the process exits.
#[derive(Clone)]
pub struct WindowHandle {
    id: WindowId,
    window: Arc<WinitWindow>,
}

impl WindowHandle {
    /// This crate's stable identity for the window.
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// Schedules a redraw request delivery for this window.
    ///
    /// The application observes it as
    /// [`PlatformEvent::Window`]` with `[`WindowEventKind::RedrawRequested`].
    pub fn request_redraw(&self) {
        self.window.request_redraw();
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
        }
    }
}

impl WindowConfig {
    /// Defaults: empty title, platform-chosen size, resizable and visible.
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

    fn into_attributes(self) -> WindowAttributes {
        let mut attributes = WinitWindow::default_attributes()
            .with_title(self.title)
            .with_resizable(self.resizable)
            .with_visible(self.visible);
        attributes = apply_optional_size(attributes, self.inner_size, SizeKind::Inner);
        attributes = apply_optional_size(attributes, self.min_inner_size, SizeKind::Min);
        attributes = apply_optional_size(attributes, self.max_inner_size, SizeKind::Max);
        attributes = apply_app_id(attributes);
        attributes
    }
}

/// Application identifier for window-system grouping.
///
/// Sets the Wayland `app_id` and the X11 `WM_CLASS` instance/general pair
/// to `bitty` so compositors (`hyprctl clients` → `class: bitty`) and the
/// shipped `bitty.desktop` (`StartupWMClass=bitty`) agree. Verified against
/// the vendored winit 0.30.13 API: both
/// `winit::platform::wayland::WindowAttributesExtWayland::with_name` and
/// `winit::platform::x11::WindowAttributesExtX11::with_name` set the same
/// shared `platform_specific.name` field, so one value covers both
/// backends; both calls below are idempotent with identical arguments.
#[cfg(target_os = "linux")]
fn apply_app_id(attributes: WindowAttributes) -> WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland as WaylandExt;
    use winit::platform::x11::WindowAttributesExtX11 as X11Ext;
    let attributes = WaylandExt::with_name(attributes, "bitty", "bitty");
    X11Ext::with_name(attributes, "bitty", "bitty")
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

/// Bidirectional mapping between upstream window ids and owned ids.
#[derive(Debug, Default)]
struct Registry {
    ids: HashMap<WinitWindowId, WindowId>,
    next_raw_id: u64,
}

impl Registry {
    fn register(&mut self, upstream: &WinitWindowId) -> WindowId {
        self.next_raw_id += 1;
        let owned = WindowId::from_raw(self.next_raw_id);
        self.ids.insert(*upstream, owned);
        owned
    }

    fn unregister(&mut self, upstream: &WinitWindowId) {
        self.ids.remove(upstream);
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
            event_loop,
            registry,
        };
        handler.handle_event(&mut ctx, event);
    }
}

impl<H: AppHandler> ApplicationHandler for Adapter<H> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch(event_loop, PlatformEvent::Resumed);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch(event_loop, PlatformEvent::Suspended);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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
        let Some(kind) = translate_window_event(event) else {
            return;
        };
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
            .with_visible(false);

        assert_eq!(
            config,
            WindowConfig {
                title: String::from("bitty"),
                inner_size: Some(LogicalSize::new(800.0, 600.0).expect("valid")),
                min_inner_size: Some(LogicalSize::new(320.0, 240.0).expect("valid")),
                max_inner_size: Some(LogicalSize::new(3840.0, 2160.0).expect("valid")),
                resizable: false,
                visible: false,
            }
        );
        assert_eq!(WindowConfig::default(), WindowConfig::new());
    }
}
