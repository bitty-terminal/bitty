//! `bitty-platform`: window/event-loop adapter adopting winit behind a
//! strictly Bitty-owned API.
//!
//! This crate implements the `bitty-platform` row of the accepted crate graph
//! (ADR-0003: *Window/event loop adapter, clipboard primitives, DPI, monitors,
//! notification primitives*; depends on no other workspace crate) and the
//! ADR-0004 decision row *ADOPT `winit` (~0.30.x) inside `bitty-platform`;
//! do not adopt `crossterm` as a runtime input path*.
//!
//! # Ownership rules
//!
//! - **No winit type escapes this crate.** Every public item is Bitty-owned;
//!   `winit` appears only in private signatures and internal glue. Upstream
//!   additions surface here as translation changes, not as API breaks.
//! - **Single upstream-type exception: [`raw_window_handle`].** GPU surface
//!   creation (`wgpu` 25, adopted in `bitty-render`) necessarily consumes raw
//!   display/window handles, so those types are unavoidable at the
//!   [`surface::SurfaceTarget`] boundary. The dependency is pinned to the
//!   exact version wgpu consumes and re-exported from this crate root, so
//!   every consumer shares one version instead of adding its own. No other
//!   upstream type may appear in a public signature.
//! - **No business semantics.** There is no grid, render, terminal-state, or
//!   plugin coupling; this crate knows nothing about higher layers (ADR-0003
//!   dependency rule 1). Input *encoding policy* (keymaps, IME, paste rules)
//!   deliberately lives elsewhere; its placement is an open question tracked
//!   in ADR-0003 ("Input-domain placement").
//! - **Zero unsafe.** The workspace denies `unsafe_code`; this crate adds
//!   `#![forbid(unsafe_code)]`. Consuming `winit` 0.30 requires no consumer
//!   `unsafe`, so no exception is claimed.
//!
//! # What the skeleton provides
//!
//! - [`App::run`]: drives a [`AppHandler`] on the OS event loop.
//! - [`EventContext`]: window creation ([`WindowHandle`]), loop exit, and
//!   wake/poll control exposed to the handler.
//! - [`PlatformEvent`]: the single owned event vocabulary (window
//!   resize/DPI-change/close lifecycle, keyboard, mouse, cursor, redraw, plus
//!   loop resume/suspend/wait/exit phases).
//! - [`dpi`] module: validated DPI-aware size types ([`ScaleFactor`],
//!   [`LogicalPixel`], [`LogicalSize`], [`PhysicalSize`]).
//! - [`surface`] module: the owned [`SurfaceTarget`] GPU-attachment seam
//!   ([`WindowHandle::surface_target`]), the resize→surface-extent mapper
//!   [`map_resize_to_surface_extent`], and the DPI-size refresh hook
//!   ([`SurfaceTarget::logical_to_physical`]); see the module docs for the
//!   renderer flow and lifetime contract.
//!
//! # Headless-friendly test seam
//!
//! CI has no display server. The design therefore separates:
//!
//! 1. **Pure logic** (owned event construction, winit→owned field mapping,
//!    DPI arithmetic, close-request semantics) exercised by unit tests that
//!    never touch a display server. Field-level mapping functions exist
//!    precisely because some `winit` event payloads (e.g. `DeviceId`,
//!    `InnerSizeWriter`) carry OS handles that cannot be constructed
//!    off-platform; those fields enter translation as plain data.
//! 2. **Display-dependent integration tests** gated behind the default-off
//!    `gui-tests` cargo feature (`cargo test -p bitty-platform --features
//!    gui-tests`). These open a real window and are **not executed in CI**;
//!    they are local verification evidence only.
//! 3. **Graceful headless degradation**: [`App::run`] returns an owned
//!    [`PlatformError::DisplayUnavailable`] when no window system is present
//!    instead of panicking. Because winit requires event-loop creation on the
//!    OS main thread, this path is exercised by the `headless_run`
//!    integration target (`harness = false`, runs as a real process entry
//!    point on the main thread); it accepts both outcomes — a clean loop run
//!    when a display exists, an owned display-unavailable error otherwise.
//!
//! # Skeleton limitations (documented, deliberate)
//!
//! - `WindowEventKind::KeyboardInput` carries layout-dependent logical keys
//!   (a terminal-relevant subset of winit's `NamedKey`) plus text, location,
//!   state, repeat, and synthetic flags. **Positional (`KeyCode`) identity is
//!   omitted** pending the open input-domain placement decision; unmodeled
//!   named keys collapse to [`LogicalKey::Named`]`(`[`NamedKey::Other`]`)`.
//!   Extend [`NamedKey`] before relying on any collapsed key.
//! - `ScaleFactorChanged` reports the new [`ScaleFactor`] and keeps the
//!   OS-suggested resize (winit's `InnerSizeWriter` negotiation hook is not
//!   re-exposed yet).
//! - [`SurfaceTarget`] keeps its window alive via shared ownership, but the
//!   obligation to drop a GPU surface before the last window/target clone
//!   drops is documented rather than enforced across crates; the future
//!   renderer/GpuContext slice owns that contract.
//! - IME, modifiers state, raw device events, touch/gesture, drag-and-drop,
//!   theme, and occlusion events are currently filtered out (documented in
//!   [`event`]); clipboard, monitors, notifications, and URL primitives from
//!   the ADR-0003 row land in later slices.

#![forbid(unsafe_code)]

pub mod app;
pub mod clipboard;
pub mod dpi;
pub mod error;
pub mod event;
pub mod keyboard;
pub mod surface;
pub mod url;

// ADR-0004 wrapper-rule exception: raw display/window handles are the payload
// of GPU surface creation, so the exact pinned upstream version is re-exported
// for consumers (bitty-render/wgpu) instead of each adding its own. See the
// crate-level ownership rules and `surface` module docs.
pub use raw_window_handle;

pub use app::{App, AppHandler, EventContext, WindowConfig, WindowHandle};
pub use clipboard::Clipboard;
pub use dpi::{LogicalPixel, LogicalSize, PhysicalSize, ScaleFactor};
pub use error::PlatformError;
pub use event::{
    CursorPosition, KeyEvent, KeyLocation, LogicalKey, MouseButton, MouseEvent, NamedKey,
    PlatformEvent, PressState, ScrollDelta, WindowEventKind, WindowId,
};
pub use keyboard::{encode_key_event, encode_named_key};
pub use surface::{SurfaceTarget, map_resize_to_surface_extent};
pub use url::{URL_MAX_LEN, ValidatedUrl, validate_file_url, validate_url};
