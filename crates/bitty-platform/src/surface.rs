//! Owned GPU-surface attachment seam: hands a platform window to a renderer
//! for `wgpu` surface creation without leaking winit types.
//!
//! # Why this module exists
//!
//! ADR-0004 adopts `wgpu` (~25.x) in `bitty-render` and keeps OS integration
//! in `bitty-platform`. Creating a `wgpu::Surface` necessarily consumes raw
//! display/window handles (`raw-window-handle` 0.6). This module is the single
//! seam where that happens:
//!
//! - [`SurfaceTarget`] is Bitty-owned; no winit type appears in any public
//!   signature. The underlying window is kept alive by an internal shared
//!   handle, so cloning a target shares the same live window.
//! - [`SurfaceTarget::with_raw_handles`] lends the raw display/window handles
//!   to a caller-supplied closure. `raw-window-handle` itself cannot be
//!   avoided at this boundary (the handles *are* the payload wgpu consumes),
//!   so the exact version is pinned and re-exported as
//!   [`crate::raw_window_handle`] — see the crate-level ownership rules.
//!
//! # Renderer flow (resize -> surface extent)
//!
//! 1. Attach once: obtain a [`SurfaceTarget`] from
//!    [`WindowHandle::surface_target`](crate::WindowHandle::surface_target)
//!    (typically on [`PlatformEvent::Resumed`](crate::PlatformEvent::Resumed))
//!    and build the GPU surface inside `with_raw_handles`.
//! 2. On
//!    [`WindowEventKind::Resized`](crate::WindowEventKind::Resized), pass the
//!    payload through [`map_resize_to_surface_extent`] and reconfigure the
//!    surface with the result; `None` means the window collapsed to a
//!    zero-sized extent (minimized/occluded) and configuration must be
//!    skipped until a non-zero size arrives.
//! 3. On
//!    [`WindowEventKind::ScaleFactorChanged`](crate::WindowEventKind::ScaleFactorChanged),
//!    refresh DPI-dependent sizes: convert cached logical geometry with
//!    [`SurfaceTarget::logical_to_physical`] (or re-read
//!    [`SurfaceTarget::inner_size`]) and reconfigure the surface extent. A
//!    `Resized` event usually follows and takes precedence.
//!
//! # Lifetime contract
//!
//! The raw handles are valid only while the originating window lives. Because
//! [`SurfaceTarget`] and
//! [`WindowHandle`](crate::WindowHandle) share one reference-counted window,
//! the contract is: a GPU surface created from these handles must be dropped
//! before the last [`SurfaceTarget`] /
//! [`WindowHandle`](crate::WindowHandle) clone sharing that window drops.
//! Nothing enforces this across crates yet; the future renderer/GpuContext
//! slice owns that obligation (documented here as the boundary owner).

use std::fmt;
use std::sync::Arc;

use winit::window::Window as WinitWindow;

use crate::dpi::{LogicalSize, PhysicalSize, ScaleFactor};
use crate::error::PlatformError;
use crate::event::WindowId;
use crate::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

/// Internal provider of the platform data behind a [`SurfaceTarget`].
///
/// Exists so headless unit tests can exercise the seam's plumbing with
/// constructed inputs, mirroring the field-level mapping seam in
/// [`crate::event`]: everything OS-bound sits behind this trait, everything
/// testable operates on plain data.
trait SurfaceSource: Send + Sync + fmt::Debug {
    /// Raw handle of the windowing-system display/connection.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::SurfaceHandle`] when the platform refuses or
    /// cannot produce the handle.
    fn display_handle(&self) -> Result<RawDisplayHandle, PlatformError>;

    /// Raw handle of the window itself.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::SurfaceHandle`] when the platform refuses or
    /// cannot produce the handle.
    fn window_handle(&self) -> Result<RawWindowHandle, PlatformError>;

    /// Current inner size in physical pixels.
    fn inner_size(&self) -> PhysicalSize;

    /// Current DPI scale factor, sanitized to a valid range.
    fn scale_factor(&self) -> ScaleFactor;
}

#[derive(Debug)]
struct WindowSurfaceSource {
    window: Arc<WinitWindow>,
}

impl SurfaceSource for WindowSurfaceSource {
    fn display_handle(&self) -> Result<RawDisplayHandle, PlatformError> {
        self.window
            .display_handle()
            .map(|handle| handle.as_raw())
            .map_err(|error| PlatformError::SurfaceHandle(error.to_string()))
    }

    fn window_handle(&self) -> Result<RawWindowHandle, PlatformError> {
        self.window
            .window_handle()
            .map(|handle| handle.as_raw())
            .map_err(|error| PlatformError::SurfaceHandle(error.to_string()))
    }

    fn inner_size(&self) -> PhysicalSize {
        let size = self.window.inner_size();
        PhysicalSize::new(size.width, size.height)
    }

    fn scale_factor(&self) -> ScaleFactor {
        ScaleFactor::new_sanitized(self.window.scale_factor())
    }
}

/// Bitty-owned attachment point for a GPU renderer surface.
///
/// Derived from a live [`WindowHandle`](crate::WindowHandle) via
/// [`WindowHandle::surface_target`](crate::WindowHandle::surface_target).
/// Cloning shares the same underlying window; see the module-level lifetime
/// contract before creating surfaces from it.
#[derive(Clone, Debug)]
pub struct SurfaceTarget {
    id: WindowId,
    source: Arc<dyn SurfaceSource>,
}

impl SurfaceTarget {
    pub(crate) fn from_window(id: WindowId, window: Arc<WinitWindow>) -> Self {
        Self {
            id,
            source: Arc::new(WindowSurfaceSource { window }),
        }
    }

    #[cfg(test)]
    fn from_source(id: WindowId, source: Arc<dyn SurfaceSource>) -> Self {
        Self { id, source }
    }

    /// Identity of the window this target was derived from.
    ///
    /// Equals the originating [`WindowHandle::id`](crate::WindowHandle::id),
    /// so renderer-side state can be keyed per window.
    pub fn window_id(&self) -> WindowId {
        self.id
    }

    /// Lends the raw display/window handles of this window to `configure`.
    ///
    /// This is the only place where `wgpu`
    /// (`Surface::create_surface_unsafe`) receives what it needs; both handle
    /// types come from the pinned [`crate::raw_window_handle`] re-export, so
    /// consumers never add their own version of that dependency.
    ///
    /// The window is kept alive for the duration of the call; the returned
    /// value passes through unchanged. Handles remain valid afterwards only
    /// under the module lifetime contract.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::SurfaceHandle`] when either handle cannot be
    /// obtained from the platform; `configure` is not invoked in that case.
    pub fn with_raw_handles<R>(
        &self,
        configure: impl FnOnce(RawDisplayHandle, RawWindowHandle) -> R,
    ) -> Result<R, PlatformError> {
        let display = self.source.display_handle()?;
        let window = self.source.window_handle()?;
        Ok(configure(display, window))
    }

    /// Current inner size in physical pixels.
    ///
    /// Use after a resize/DPI change instead of caching sizes across events.
    pub fn inner_size(&self) -> PhysicalSize {
        self.source.inner_size()
    }

    /// Current DPI scale factor, sanitized to a valid range.
    pub fn scale_factor(&self) -> ScaleFactor {
        self.source.scale_factor()
    }

    /// DPI-size refresh hook: converts a logical size into physical pixels at
    /// the *current* scale factor.
    ///
    /// Call after
    /// [`WindowEventKind::ScaleFactorChanged`](crate::WindowEventKind::ScaleFactorChanged)
    /// to recompute cached logical geometry before reconfiguring the surface.
    pub fn logical_to_physical(&self, size: LogicalSize) -> PhysicalSize {
        size.to_physical(self.source.scale_factor())
    }
}

/// Maps a resize-event size onto the extent a GPU surface should be
/// configured with.
///
/// Returns `None` when either dimension is zero (minimized/occluded windows):
/// GPU surfaces cannot take a zero extent, so the renderer skips
/// reconfiguration until a non-zero size arrives.
///
/// See the [module flow](self#renderer-flow-resize---surface-extent) for the
/// full resize -> reconfigure sequence.
pub fn map_resize_to_surface_extent(size: PhysicalSize) -> Option<PhysicalSize> {
    (size.width() > 0 && size.height() > 0).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_window_handle::{WebDisplayHandle, WebWindowHandle};

    /// Cross-platform fixture handles: every `Raw*Handle` variant exists on
    /// every target, and the Web variants are plain ids constructible without
    /// an OS connection.
    fn fixture_display() -> RawDisplayHandle {
        RawDisplayHandle::Web(WebDisplayHandle::new())
    }

    fn fixture_window() -> RawWindowHandle {
        RawWindowHandle::Web(WebWindowHandle::new(42))
    }

    #[derive(Debug)]
    struct FakeSource {
        fail_display: bool,
        fail_window: bool,
        size: PhysicalSize,
        scale: ScaleFactor,
    }

    impl FakeSource {
        fn ok() -> Self {
            Self {
                fail_display: false,
                fail_window: false,
                size: PhysicalSize::new(800, 600),
                scale: ScaleFactor::ONE,
            }
        }
    }

    impl SurfaceSource for FakeSource {
        fn display_handle(&self) -> Result<RawDisplayHandle, PlatformError> {
            if self.fail_display {
                Err(PlatformError::SurfaceHandle("no display".into()))
            } else {
                Ok(fixture_display())
            }
        }

        fn window_handle(&self) -> Result<RawWindowHandle, PlatformError> {
            if self.fail_window {
                Err(PlatformError::SurfaceHandle("no window".into()))
            } else {
                Ok(fixture_window())
            }
        }

        fn inner_size(&self) -> PhysicalSize {
            self.size
        }

        fn scale_factor(&self) -> ScaleFactor {
            self.scale
        }
    }

    fn target(source: FakeSource) -> SurfaceTarget {
        SurfaceTarget::from_source(WindowId::from_raw(7), Arc::new(source))
    }

    #[test]
    fn raw_handles_reach_the_configuration_closure() {
        let delivered = target(FakeSource::ok())
            .with_raw_handles(|display, window| (display, window))
            .expect("fixture source yields both handles");
        assert_eq!(delivered, (fixture_display(), fixture_window()));
    }

    #[test]
    fn closure_result_passes_through_unchanged() {
        let marker = "configured";
        let outcome = target(FakeSource::ok())
            .with_raw_handles(|_, _| marker)
            .expect("fixture source yields both handles");
        assert_eq!(outcome, marker);
    }

    #[test]
    fn display_handle_failure_maps_to_owned_error_without_configuring() {
        let mut called = false;
        let outcome = target(FakeSource {
            fail_display: true,
            ..FakeSource::ok()
        })
        .with_raw_handles(|_, _| {
            called = true;
        });
        assert!(matches!(
            outcome,
            Err(PlatformError::SurfaceHandle(detail)) if detail.contains("no display")
        ));
        assert!(!called, "configure must not run when a handle fails");
    }

    #[test]
    fn window_handle_failure_maps_to_owned_error_without_configuring() {
        let outcome = target(FakeSource {
            fail_window: true,
            ..FakeSource::ok()
        })
        .with_raw_handles(|_, _| ());
        assert!(matches!(
            outcome,
            Err(PlatformError::SurfaceHandle(detail)) if detail.contains("no window")
        ));
    }

    #[test]
    fn clones_share_identity_and_deliver_the_same_handles() {
        let original = target(FakeSource::ok());
        let clone = original.clone();
        assert_eq!(clone.window_id(), original.window_id());
        assert_eq!(clone.window_id().get(), 7);
        let via_clone = clone.with_raw_handles(|d, w| (d, w)).expect("handles");
        let via_original = original.with_raw_handles(|d, w| (d, w)).expect("handles");
        assert_eq!(via_clone, via_original);
    }

    #[test]
    fn queries_reflect_the_current_source_state() {
        let source = FakeSource {
            size: PhysicalSize::new(1024, 768),
            scale: ScaleFactor::new_sanitized(2.0),
            ..FakeSource::ok()
        };
        let attached = target(source);
        assert_eq!(attached.inner_size(), PhysicalSize::new(1024, 768));
        assert_eq!(attached.scale_factor().get(), 2.0);
    }

    #[test]
    fn dpi_refresh_hook_converts_logical_size_at_current_scale() {
        let attached = target(FakeSource {
            scale: ScaleFactor::new_sanitized(2.0),
            ..FakeSource::ok()
        });
        let logical = LogicalSize::new(100.0, 50.5).expect("valid");
        assert_eq!(
            attached.logical_to_physical(logical),
            PhysicalSize::new(200, 101)
        );
    }

    #[test]
    fn resize_extent_mapping_keeps_non_zero_sizes() {
        let size = PhysicalSize::new(640, 480);
        assert_eq!(map_resize_to_surface_extent(size), Some(size));
    }

    #[test]
    fn resize_extent_mapping_drops_zero_dimensions() {
        assert_eq!(
            map_resize_to_surface_extent(PhysicalSize::new(0, 480)),
            None
        );
        assert_eq!(
            map_resize_to_surface_extent(PhysicalSize::new(640, 0)),
            None
        );
        assert_eq!(map_resize_to_surface_extent(PhysicalSize::new(0, 0)), None);
    }
}
