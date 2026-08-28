//! Owned event vocabulary and the winit→owned translation seam.
//!
//! [`PlatformEvent`] is the single event type handed to
//! [`AppHandler`](crate::AppHandler). Translation from `winit` events happens
//! in this module behind two layers:
//!
//! 1. Field-level mapping functions ([`map_keyboard_input`],
//!    [`map_mouse_input`], [`map_cursor_moved`], [`map_mouse_wheel`],
//!    [`map_scale_factor_changed`], ...) that take **plain data** extracted
//!    from upstream payloads. This is the headless seam: payloads such as
//!    `DeviceId` or `InnerSizeWriter` wrap OS handles that cannot be
//!    constructed without a display server, so tests exercise the mapping on
//!    their constructible fields directly.
//! 2. [`translate_window_event`], which destructures the upstream enum and
//!    delegates to (1). Variants that carry no non-constructible fields are
//!    additionally covered end-to-end by unit tests; the remainder are covered
//!    by the display-gated integration tests.

use winit::event::{ElementState, MouseButton as WinitMouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, KeyLocation as WinitKeyLocation, NamedKey as WinitNamedKey, SmolStr};

use crate::dpi::{PhysicalSize, ScaleFactor};

/// Stable handle identifying a window created through this crate.
///
/// Values are assigned sequentially by the adapter and are unique per process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u64);

impl WindowId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Creates a `WindowId` from a raw value.
    ///
    /// Exposed for tests and for handlers that synthesize [`PlatformEvent`]s
    /// without a live window (e.g. headless `Runtime` integration tests).
    /// Production window identities remain those assigned by
    /// [`EventContext::create_window`](crate::EventContext::create_window).
    pub fn from_raw_public(raw: u64) -> Self {
        Self::from_raw(raw)
    }

    /// Returns the raw numeric identity (process-local).
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Keyboard-independent press/release state shared by keys and buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PressState {
    /// Key or button went down.
    Pressed,
    /// Key or button came up.
    Released,
}

impl From<ElementState> for PressState {
    fn from(state: ElementState) -> Self {
        match state {
            ElementState::Pressed => Self::Pressed,
            ElementState::Released => Self::Released,
        }
    }
}

/// Where on the keyboard a key sits (mirrors the upstream classification).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyLocation {
    /// Default position.
    Standard,
    /// Left of two duplicated keys (e.g. left Shift).
    Left,
    /// Right of two duplicated keys (e.g. right Ctrl).
    Right,
    /// On the numeric keypad.
    Numpad,
}

impl From<WinitKeyLocation> for KeyLocation {
    fn from(location: WinitKeyLocation) -> Self {
        match location {
            WinitKeyLocation::Standard => Self::Standard,
            WinitKeyLocation::Left => Self::Left,
            WinitKeyLocation::Right => Self::Right,
            WinitKeyLocation::Numpad => Self::Numpad,
        }
    }
}

/// Layout-dependent logical key identity.
///
/// Terminal-relevant named keys are modeled explicitly; anything outside the
/// subset collapses to [`NamedKey::Other`] so translation stays lossy in a
/// single, greppable place. Extend the subset before relying on a collapsed
/// key. Positional (`KeyCode`) identity is deferred to the input-domain slice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LogicalKey {
    /// A character-producing key with its layout-dependent text.
    Character(String),
    /// An explicitly modeled named key.
    Named(NamedKey),
    /// A dead key; carries the composed character when known.
    Dead(Option<char>),
    /// A key with no cross-platform logical identity.
    Unidentified,
}

/// Explicitly modeled subset of the upstream named-key set.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Escape,
    Enter,
    Tab,
    Space,
    Backspace,
    Delete,
    Insert,
    Clear,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    F25,
    F26,
    F27,
    F28,
    F29,
    F30,
    F31,
    F32,
    F33,
    F34,
    F35,
    Shift,
    Control,
    Alt,
    AltGraph,
    Meta,
    Super,
    Hyper,
    Fn,
    FnLock,
    CapsLock,
    NumLock,
    ScrollLock,
    Symbol,
    SymbolLock,
    PrintScreen,
    Pause,
    ContextMenu,
    Copy,
    Cut,
    Paste,
    Undo,
    Redo,
    Find,
    Select,
    Again,
    Props,
    Execute,
    Help,
    AudioVolumeMute,
    AudioVolumeDown,
    AudioVolumeUp,
    MediaPlay,
    MediaPause,
    MediaPlayPause,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    BrowserBack,
    BrowserForward,
    BrowserRefresh,
    BrowserStop,
    BrowserSearch,
    BrowserHome,
    BrowserFavorites,
    LaunchMail,
    LaunchApplication1,
    LaunchApplication2,
    Eject,
    Power,
    WakeUp,
    Standby,
    Hibernate,
    Soft1,
    Soft2,
    Soft3,
    Soft4,
    /// Catch-all for upstream named keys outside the modeled subset.
    Other,
}

impl From<WinitNamedKey> for NamedKey {
    fn from(key: WinitNamedKey) -> Self {
        match key {
            WinitNamedKey::Escape => Self::Escape,
            WinitNamedKey::Enter => Self::Enter,
            WinitNamedKey::Tab => Self::Tab,
            WinitNamedKey::Space => Self::Space,
            WinitNamedKey::Backspace => Self::Backspace,
            WinitNamedKey::Delete => Self::Delete,
            WinitNamedKey::Insert => Self::Insert,
            WinitNamedKey::Clear => Self::Clear,
            WinitNamedKey::Home => Self::Home,
            WinitNamedKey::End => Self::End,
            WinitNamedKey::PageUp => Self::PageUp,
            WinitNamedKey::PageDown => Self::PageDown,
            WinitNamedKey::ArrowUp => Self::ArrowUp,
            WinitNamedKey::ArrowDown => Self::ArrowDown,
            WinitNamedKey::ArrowLeft => Self::ArrowLeft,
            WinitNamedKey::ArrowRight => Self::ArrowRight,
            WinitNamedKey::F1 => Self::F1,
            WinitNamedKey::F2 => Self::F2,
            WinitNamedKey::F3 => Self::F3,
            WinitNamedKey::F4 => Self::F4,
            WinitNamedKey::F5 => Self::F5,
            WinitNamedKey::F6 => Self::F6,
            WinitNamedKey::F7 => Self::F7,
            WinitNamedKey::F8 => Self::F8,
            WinitNamedKey::F9 => Self::F9,
            WinitNamedKey::F10 => Self::F10,
            WinitNamedKey::F11 => Self::F11,
            WinitNamedKey::F12 => Self::F12,
            WinitNamedKey::F13 => Self::F13,
            WinitNamedKey::F14 => Self::F14,
            WinitNamedKey::F15 => Self::F15,
            WinitNamedKey::F16 => Self::F16,
            WinitNamedKey::F17 => Self::F17,
            WinitNamedKey::F18 => Self::F18,
            WinitNamedKey::F19 => Self::F19,
            WinitNamedKey::F20 => Self::F20,
            WinitNamedKey::F21 => Self::F21,
            WinitNamedKey::F22 => Self::F22,
            WinitNamedKey::F23 => Self::F23,
            WinitNamedKey::F24 => Self::F24,
            WinitNamedKey::F25 => Self::F25,
            WinitNamedKey::F26 => Self::F26,
            WinitNamedKey::F27 => Self::F27,
            WinitNamedKey::F28 => Self::F28,
            WinitNamedKey::F29 => Self::F29,
            WinitNamedKey::F30 => Self::F30,
            WinitNamedKey::F31 => Self::F31,
            WinitNamedKey::F32 => Self::F32,
            WinitNamedKey::F33 => Self::F33,
            WinitNamedKey::F34 => Self::F34,
            WinitNamedKey::F35 => Self::F35,
            WinitNamedKey::Shift => Self::Shift,
            WinitNamedKey::Control => Self::Control,
            WinitNamedKey::Alt => Self::Alt,
            WinitNamedKey::AltGraph => Self::AltGraph,
            WinitNamedKey::Meta => Self::Meta,
            WinitNamedKey::Super => Self::Super,
            WinitNamedKey::Hyper => Self::Hyper,
            WinitNamedKey::Fn => Self::Fn,
            WinitNamedKey::FnLock => Self::FnLock,
            WinitNamedKey::CapsLock => Self::CapsLock,
            WinitNamedKey::NumLock => Self::NumLock,
            WinitNamedKey::ScrollLock => Self::ScrollLock,
            WinitNamedKey::Symbol => Self::Symbol,
            WinitNamedKey::SymbolLock => Self::SymbolLock,
            WinitNamedKey::PrintScreen => Self::PrintScreen,
            WinitNamedKey::Pause => Self::Pause,
            WinitNamedKey::ContextMenu => Self::ContextMenu,
            WinitNamedKey::Copy => Self::Copy,
            WinitNamedKey::Cut => Self::Cut,
            WinitNamedKey::Paste => Self::Paste,
            WinitNamedKey::Undo => Self::Undo,
            WinitNamedKey::Redo => Self::Redo,
            WinitNamedKey::Find => Self::Find,
            WinitNamedKey::Select => Self::Select,
            WinitNamedKey::Again => Self::Again,
            WinitNamedKey::Props => Self::Props,
            WinitNamedKey::Execute => Self::Execute,
            WinitNamedKey::Help => Self::Help,
            WinitNamedKey::AudioVolumeMute => Self::AudioVolumeMute,
            WinitNamedKey::AudioVolumeDown => Self::AudioVolumeDown,
            WinitNamedKey::AudioVolumeUp => Self::AudioVolumeUp,
            WinitNamedKey::MediaPlay => Self::MediaPlay,
            WinitNamedKey::MediaPause => Self::MediaPause,
            WinitNamedKey::MediaPlayPause => Self::MediaPlayPause,
            WinitNamedKey::MediaStop => Self::MediaStop,
            WinitNamedKey::MediaTrackNext => Self::MediaTrackNext,
            WinitNamedKey::MediaTrackPrevious => Self::MediaTrackPrevious,
            WinitNamedKey::BrowserBack => Self::BrowserBack,
            WinitNamedKey::BrowserForward => Self::BrowserForward,
            WinitNamedKey::BrowserRefresh => Self::BrowserRefresh,
            WinitNamedKey::BrowserStop => Self::BrowserStop,
            WinitNamedKey::BrowserSearch => Self::BrowserSearch,
            WinitNamedKey::BrowserHome => Self::BrowserHome,
            WinitNamedKey::BrowserFavorites => Self::BrowserFavorites,
            WinitNamedKey::LaunchMail => Self::LaunchMail,
            WinitNamedKey::LaunchApplication1 => Self::LaunchApplication1,
            WinitNamedKey::LaunchApplication2 => Self::LaunchApplication2,
            WinitNamedKey::Eject => Self::Eject,
            WinitNamedKey::Power => Self::Power,
            WinitNamedKey::WakeUp => Self::WakeUp,
            WinitNamedKey::Standby => Self::Standby,
            WinitNamedKey::Hibernate => Self::Hibernate,
            WinitNamedKey::Soft1 => Self::Soft1,
            WinitNamedKey::Soft2 => Self::Soft2,
            WinitNamedKey::Soft3 => Self::Soft3,
            WinitNamedKey::Soft4 => Self::Soft4,
            _ => Self::Other,
        }
    }
}

/// A keyboard event translated to owned types.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Layout-dependent logical identity of the key.
    pub logical_key: LogicalKey,
    /// Text the key would produce under current modifiers, if any.
    pub text: Option<String>,
    /// Physical placement category of the key.
    pub location: KeyLocation,
    /// Whether the key went down or up.
    pub state: PressState,
    /// Whether this is an OS auto-repeat of a held key.
    pub repeat: bool,
    /// Whether winit synthesized the event for focus changes rather than real
    /// hardware input; synthetic events are dropped during translation.
    pub is_synthetic: bool,
}

/// A mouse button press/release event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    /// Which button changed state.
    pub button: MouseButton,
    /// New state of the button.
    pub state: PressState,
}

/// A physical mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Secondary button.
    Right,
    /// Middle button.
    Middle,
    /// Back navigation button.
    Back,
    /// Forward navigation button.
    Forward,
    /// Vendor-specific button number.
    Other(u16),
}

impl From<WinitMouseButton> for MouseButton {
    fn from(button: WinitMouseButton) -> Self {
        match button {
            WinitMouseButton::Left => Self::Left,
            WinitMouseButton::Right => Self::Right,
            WinitMouseButton::Middle => Self::Middle,
            WinitMouseButton::Back => Self::Back,
            WinitMouseButton::Forward => Self::Forward,
            WinitMouseButton::Other(n) => Self::Other(n),
        }
    }
}

/// Cursor position in physical window coordinates (pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorPosition {
    /// Horizontal offset from the top-left corner.
    pub x: f64,
    /// Vertical offset from the top-left corner.
    pub y: f64,
}

impl CursorPosition {
    pub(crate) const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Scroll motion translated to owned units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollDelta {
    /// Row/column counts (typical mouse wheel steps).
    Lines(f32, f32),
    /// Precise pixel deltas (trackpads).
    Pixels(f64, f64),
}

/// Window-scoped events, fully owned.
///
/// # Renderer resize flow
///
/// The size-carrying variants feed the GPU surface reconfiguration sequence
/// documented on [`crate::surface::SurfaceTarget`]:
///
/// - [`Resized`](WindowEventKind::Resized): map the payload through
///   [`crate::surface::map_resize_to_surface_extent`] and configure the
///   attached surface with the resulting extent (`None` => zero-sized window,
///   skip configuration).
/// - [`ScaleFactorChanged`](WindowEventKind::ScaleFactorChanged): refresh
///   DPI-dependent sizes with
///   [`crate::surface::SurfaceTarget::logical_to_physical`] (or re-read
///   `inner_size`) before reconfiguring; a following `Resized` event takes
///   precedence.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowEventKind {
    /// The window's inner size changed to the given physical size.
    ///
    /// Renderer flow: pass the payload through
    /// [`crate::surface::map_resize_to_surface_extent`]; a `None` result means
    /// the window collapsed to a zero extent and surface configuration must
    /// be skipped until a non-zero size arrives. Prefer re-reading
    /// [`crate::surface::SurfaceTarget::inner_size`] over caching sizes.
    Resized(PhysicalSize),
    /// The DPI scale factor changed; the OS-suggested resize is kept.
    ///
    /// Renderer flow: treat as a DPI-size refresh — convert cached logical
    /// geometry with
    /// [`crate::surface::SurfaceTarget::logical_to_physical`] at the new
    /// factor, then reconfigure the surface extent. A `Resized` event usually
    /// follows and supersedes the computed value.
    ScaleFactorChanged(ScaleFactor),
    /// The window requests a redraw.
    RedrawRequested,
    /// The window gained (`true`) or lost (`false`) keyboard focus.
    Focused(bool),
    /// A non-synthetic keyboard event.
    KeyboardInput(KeyEvent),
    /// A mouse button changed state.
    MouseInput(MouseEvent),
    /// A scroll gesture delivered wheel motion.
    MouseWheel(ScrollDelta),
    /// The cursor moved within the window.
    CursorMoved(CursorPosition),
    /// The cursor left the window.
    CursorLeft,
    /// The user or system asked to close the window; the handler decides
    /// whether to tear down and call [`EventContext::exit`]
    /// (crate::app::EventContext::exit).
    CloseRequested,
    /// The window was destroyed by the platform; no further events arrive for
    /// it regardless of any pending close request.
    Closed,
}

/// Every event the platform layer delivers to an [`AppHandler`]
/// (crate::AppHandler), fully owned.
#[derive(Clone, Debug, PartialEq)]
pub enum PlatformEvent {
    /// An event scoped to one window.
    ///
    /// The size-carrying payloads drive the GPU surface reconfiguration flow
    /// documented on [`WindowEventKind`].
    Window {
        /// Window the event belongs to.
        window_id: WindowId,
        /// The window-scoped payload.
        kind: WindowEventKind,
    },
    /// The loop resumed (also delivered once at startup before first draw).
    Resumed,
    /// The loop suspended (mobile lifecycle; windows may be hidden).
    Suspended,
    /// All pending input has been processed; a good time to redraw.
    AboutToWait,
    /// The loop is about to exit after an [`EventContext::exit`]
    /// (crate::app::EventContext::exit) request.
    Exiting,
}

// ---------------------------------------------------------------------------
// Translation seam
//
// Functions below are `pub(crate)`. Each mapping function takes plain data so
// headless unit tests can drive them without a display server; upstream types
// that wrap OS handles (`DeviceId`, `InnerSizeWriter`, the `pub(crate)`
// payload of `winit::event::KeyEvent`) cannot be constructed off-platform, so
// `translate_window_event` is a thin destructuring layer over these mappers
// and is itself covered end-to-end for every fully constructible variant.
// ---------------------------------------------------------------------------

/// Translates a full upstream window event into the owned vocabulary.
///
/// Returns `None` for events this skeleton filters out (see crate docs for
/// the deferred list). Synthetic keyboard events are dropped here.
pub(crate) fn translate_window_event(event: WindowEvent) -> Option<WindowEventKind> {
    match event {
        WindowEvent::Resized(size) => Some(map_resized(size)),
        WindowEvent::ScaleFactorChanged {
            scale_factor,
            // Negotiation hook intentionally unused in this slice: the
            // OS-suggested inner size is kept. See crate-level docs.
            inner_size_writer: _,
        } => Some(map_scale_factor_changed(scale_factor)),
        WindowEvent::CloseRequested => Some(WindowEventKind::CloseRequested),
        WindowEvent::Destroyed => Some(WindowEventKind::Closed),
        WindowEvent::RedrawRequested => Some(WindowEventKind::RedrawRequested),
        WindowEvent::Focused(focused) => Some(map_focused(focused)),
        WindowEvent::KeyboardInput {
            event: key_event,
            is_synthetic,
            // Device ids are opaque OS handles; the owned vocabulary has no
            // use for them in this slice.
            device_id: _,
        } => translate_key_parts(
            key_event.logical_key,
            key_event.text,
            key_event.location,
            key_event.state,
            key_event.repeat,
            is_synthetic,
        )
        .map(WindowEventKind::KeyboardInput),
        WindowEvent::MouseInput { state, button, .. } => Some(map_mouse_input(state, button)),
        WindowEvent::MouseWheel { delta, .. } => {
            map_mouse_wheel(delta).map(WindowEventKind::MouseWheel)
        }
        WindowEvent::CursorMoved { position, .. } => map_cursor_moved(position.x, position.y),
        WindowEvent::CursorLeft { .. } => Some(WindowEventKind::CursorLeft),
        _ => None,
    }
}

pub(crate) fn map_resized(size: winit::dpi::PhysicalSize<u32>) -> WindowEventKind {
    WindowEventKind::Resized(PhysicalSize::new(size.width, size.height))
}

pub(crate) fn map_scale_factor_changed(scale_factor: f64) -> WindowEventKind {
    // Upstream guarantees a valid factor; sanitizing keeps the invariant local
    // even if a future upstream regression violated it.
    WindowEventKind::ScaleFactorChanged(ScaleFactor::new_sanitized(scale_factor))
}

pub(crate) const fn map_focused(focused: bool) -> WindowEventKind {
    WindowEventKind::Focused(focused)
}

/// Translates keyboard event fields into an owned [`KeyEvent`].
///
/// This is the headless seam for keyboard input: every parameter is plain
/// data. The adapter destructures the (externally non-constructible) upstream
/// `KeyEvent` into these fields before calling in.
///
/// Returns `None` when `is_synthetic` is set: focus-change bookkeeping is not
/// user input, so it never reaches application logic.
pub(crate) fn translate_key_parts(
    logical_key: Key,
    text: Option<SmolStr>,
    location: WinitKeyLocation,
    state: ElementState,
    repeat: bool,
    is_synthetic: bool,
) -> Option<KeyEvent> {
    if is_synthetic {
        return None;
    }
    let logical_key = match logical_key {
        Key::Character(value) => LogicalKey::Character(value.to_string()),
        Key::Named(named) => LogicalKey::Named(NamedKey::from(named)),
        Key::Dead(maybe_char) => LogicalKey::Dead(maybe_char),
        Key::Unidentified(_) => {
            // Native-key payloads are platform-specific identifiers; they stay
            // behind the boundary (documented lossy collapse).
            LogicalKey::Unidentified
        }
    };
    Some(KeyEvent {
        logical_key,
        text: text.map(|value| value.to_string()),
        location: KeyLocation::from(location),
        state: PressState::from(state),
        repeat,
        is_synthetic,
    })
}

pub(crate) fn map_mouse_input(state: ElementState, button: WinitMouseButton) -> WindowEventKind {
    WindowEventKind::MouseInput(MouseEvent {
        button: MouseButton::from(button),
        state: PressState::from(state),
    })
}

pub(crate) fn map_mouse_wheel(delta: MouseScrollDelta) -> Option<ScrollDelta> {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => Some(ScrollDelta::Lines(x, y)),
        MouseScrollDelta::PixelDelta(position) => Some(ScrollDelta::Pixels(position.x, position.y)),
    }
}

pub(crate) const fn map_cursor_moved(x: f64, y: f64) -> Option<WindowEventKind> {
    Some(WindowEventKind::CursorMoved(CursorPosition::new(x, y)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpi::LogicalSize;
    use winit::dpi::PhysicalPosition;
    use winit::event::DeviceId;
    use winit::keyboard::{NativeKey, SmolStr};

    const DUMMY_DEVICE: DeviceId = DeviceId::dummy();

    #[test]
    fn resized_translates_payload() {
        assert_eq!(
            translate_window_event(WindowEvent::Resized(winit::dpi::PhysicalSize::new(
                800, 600
            ))),
            Some(WindowEventKind::Resized(PhysicalSize::new(800, 600)))
        );
    }

    #[test]
    fn close_request_and_destroyed_translate_distinctly() {
        assert_eq!(
            translate_window_event(WindowEvent::CloseRequested),
            Some(WindowEventKind::CloseRequested)
        );
        assert_eq!(
            translate_window_event(WindowEvent::Destroyed),
            Some(WindowEventKind::Closed)
        );
    }

    #[test]
    fn redraw_and_focus_translate() {
        assert_eq!(
            translate_window_event(WindowEvent::RedrawRequested),
            Some(WindowEventKind::RedrawRequested)
        );
        assert_eq!(
            translate_window_event(WindowEvent::Focused(true)),
            Some(WindowEventKind::Focused(true))
        );
        assert_eq!(map_focused(false), WindowEventKind::Focused(false));
    }

    #[test]
    fn filtered_events_yield_none() {
        // Constructible representatives of the filtered families: window move
        // and occlusion are not part of the skeleton surface.
        assert_eq!(
            translate_window_event(WindowEvent::Moved(PhysicalPosition::new(10, 20))),
            None
        );
        assert_eq!(translate_window_event(WindowEvent::Occluded(true)), None);
    }

    #[test]
    fn scale_factor_mapper_preserves_valid_values_and_sanitizes() {
        let valid = ScaleFactor::new(1.25).expect("valid");
        assert_eq!(
            map_scale_factor_changed(1.25),
            WindowEventKind::ScaleFactorChanged(valid)
        );
        assert_eq!(
            map_scale_factor_changed(-4.2),
            WindowEventKind::ScaleFactorChanged(ScaleFactor::new_sanitized(-4.2))
        );
        assert_ne!(
            map_scale_factor_changed(-4.2),
            WindowEventKind::ScaleFactorChanged(valid),
        );
    }

    #[test]
    fn keyboard_character_parts_translate() {
        let translated = translate_key_parts(
            Key::Character(SmolStr::from("a")),
            Some(SmolStr::from("a")),
            WinitKeyLocation::Standard,
            ElementState::Pressed,
            false,
            false,
        )
        .expect("real input");
        assert_eq!(
            translated.logical_key,
            LogicalKey::Character(String::from("a"))
        );
        assert_eq!(translated.text.as_deref(), Some("a"));
        assert_eq!(translated.location, KeyLocation::Standard);
        assert_eq!(translated.state, PressState::Pressed);
        assert!(!translated.repeat);
        assert!(!translated.is_synthetic);
    }

    #[test]
    fn synthetic_keyboard_events_are_dropped() {
        assert!(
            translate_key_parts(
                Key::Character(SmolStr::from("x")),
                None,
                WinitKeyLocation::Left,
                ElementState::Released,
                false,
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn named_dead_and_unidentified_keys_translate() {
        let named = translate_key_parts(
            Key::Named(WinitNamedKey::ArrowUp),
            None,
            WinitKeyLocation::Numpad,
            ElementState::Released,
            true,
            false,
        )
        .expect("real input");
        assert_eq!(named.logical_key, LogicalKey::Named(NamedKey::ArrowUp));
        assert_eq!(named.text, None);
        assert_eq!(named.location, KeyLocation::Numpad);
        assert_eq!(named.state, PressState::Released);
        assert!(named.repeat);

        let dead = translate_key_parts(
            Key::Dead(Some('^')),
            None,
            WinitKeyLocation::Standard,
            ElementState::Pressed,
            false,
            false,
        )
        .expect("real input");
        assert_eq!(dead.logical_key, LogicalKey::Dead(Some('^')));

        let unknown = translate_key_parts(
            Key::Unidentified(NativeKey::Unidentified),
            None,
            WinitKeyLocation::Standard,
            ElementState::Pressed,
            false,
            false,
        )
        .expect("real input");
        assert_eq!(unknown.logical_key, LogicalKey::Unidentified);

        let unmodeled = translate_key_parts(
            Key::Named(WinitNamedKey::KanaMode),
            None,
            WinitKeyLocation::Standard,
            ElementState::Pressed,
            false,
            false,
        )
        .expect("real input");
        assert_eq!(
            unmodeled.logical_key,
            LogicalKey::Named(NamedKey::Other),
            "unmodeled named keys collapse to Other"
        );
    }

    #[test]
    fn mouse_button_events_translate_end_to_end() {
        assert_eq!(
            translate_window_event(WindowEvent::MouseInput {
                device_id: DUMMY_DEVICE,
                state: ElementState::Pressed,
                button: WinitMouseButton::Left,
            }),
            Some(WindowEventKind::MouseInput(MouseEvent {
                button: MouseButton::Left,
                state: PressState::Pressed,
            }))
        );
        assert_eq!(
            map_mouse_input(ElementState::Released, WinitMouseButton::Other(9)),
            WindowEventKind::MouseInput(MouseEvent {
                button: MouseButton::Other(9),
                state: PressState::Released,
            })
        );
    }

    #[test]
    fn scroll_wheel_deltas_translate() {
        assert_eq!(
            map_mouse_wheel(MouseScrollDelta::LineDelta(-1.0, 3.0)),
            Some(ScrollDelta::Lines(-1.0, 3.0))
        );
        assert_eq!(
            translate_window_event(WindowEvent::MouseWheel {
                device_id: DUMMY_DEVICE,
                delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -42.5)),
                phase: winit::event::TouchPhase::Moved,
            }),
            Some(WindowEventKind::MouseWheel(ScrollDelta::Pixels(0.0, -42.5)))
        );
    }

    #[test]
    fn cursor_motion_translates_to_physical_coordinates() {
        assert_eq!(
            map_cursor_moved(12.5, 7.25),
            Some(WindowEventKind::CursorMoved(CursorPosition::new(
                12.5, 7.25
            )))
        );
        assert_eq!(
            translate_window_event(WindowEvent::CursorMoved {
                device_id: DUMMY_DEVICE,
                position: PhysicalPosition::new(-1.0, 3.5),
            }),
            Some(WindowEventKind::CursorMoved(CursorPosition::new(-1.0, 3.5)))
        );
        assert_eq!(
            translate_window_event(WindowEvent::CursorLeft {
                device_id: DUMMY_DEVICE
            }),
            Some(WindowEventKind::CursorLeft)
        );
    }

    #[test]
    fn close_request_semantics_documented_flow() {
        // Contract under test (owned level): CloseRequested arrives first and
        // lets the application decide whether to exit; the platform may then
        // destroy the window regardless of that decision, yielding Closed as
        // the final event for the window.
        let requested = translate_window_event(WindowEvent::CloseRequested);
        let closed = translate_window_event(WindowEvent::Destroyed);
        assert_eq!(requested, Some(WindowEventKind::CloseRequested));
        assert_eq!(closed, Some(WindowEventKind::Closed));
        assert_ne!(requested, closed);
    }

    #[test]
    fn owned_size_types_flow_through_translation_unchanged() {
        let size = LogicalSize::new(100.0, 50.0).expect("valid");
        let physical = size.to_physical(ScaleFactor::ONE);
        assert_eq!(
            translate_window_event(WindowEvent::Resized(winit::dpi::PhysicalSize::new(
                physical.width(),
                physical.height()
            ))),
            Some(WindowEventKind::Resized(physical))
        );
    }
}
