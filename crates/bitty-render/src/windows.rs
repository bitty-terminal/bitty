//! Per-window presentation surfaces (bitty#1765, slice 2).
//!
//! The primary window's surface lives in `Runtime::surface` (or in the
//! off-thread presenter that owns it). Every *additional* OS window the
//! composition root admits through its `WindowRegistry` needs a surface of its
//! own, and this module is that set: [`WindowSurfaces`] holds one row per
//! secondary window.
//!
//! # One device, several swap chains
//!
//! A `wgpu` `Device` can own any number of surface swap chains, and every
//! surface is configured with its own extent. That is what makes a real second
//! window cheap: the process keeps one [`GpuContext`] (instance, adapter,
//! device, queue) and creates one more `wgpu::Surface` from it through
//! [`GpuContext::create_surface`]. The frame the primary painted is then
//! presented to each row as-is ([`WindowSurfaces::present_all`]), which is the
//! correct picture for a peer window: the same terminal scene, painted at that
//! window's own extent (the renderer derives its frame scale from
//! `surface_extent / plan_extent` per present, so a differently sized sibling
//! scales instead of clipping).
//!
//! # Fail-closed rules
//!
//! - The set is bounded by [`MAX_WINDOW_SURFACES`], which *is*
//!   [`bitty_platform::MAX_OS_WINDOWS`]: the renderer can never hold more
//!   surfaces than the app can admit windows.
//! - [`WindowSurfaces::attach`] creates *and configures* the surface before a
//!   row exists, so a driver that refuses either step leaves the set
//!   byte-identical — never a row that cannot be presented.
//! - A window has exactly one surface: duplicate admission is refused before
//!   any driver call, so no row can leak a swap chain.
//! - [`WindowSurfaces::present_all`] contains a failing sibling: a swap chain
//!   that is briefly unavailable (hidden, outdated, lost) keeps its row and
//!   retries on the next frame, and a permanently broken one is released and
//!   reported — so one bad window can never fail the primary's frame.
//!
//! # Honest scope
//!
//! Only the synchronous presentation path can create these surfaces: the
//! off-thread Wayland presenter (bitty#2830) owns the process's `GpuContext`
//! on its worker thread, so the composition root has nothing to create a
//! sibling surface through and refuses instead (see
//! `bitty_runtime::Runtime::attach_window_surface`). Giving that presenter a
//! second surface is separate work.

#![forbid(unsafe_code)]

use std::fmt;

use bitty_platform::{PhysicalSize, SurfaceTarget, WindowId, map_resize_to_surface_extent};

use crate::atlas::AtlasDims;
use crate::error::RenderError;
use crate::gpu::{GpuContext, PresentFailure, PresentFailureClass, Surface};
use crate::grid::{DrawList, ThemePalette};

/// Whether a present failure clears on its own.
///
/// A swap chain the compositor could not release this frame (`timeout`) or one
/// that needs reconfiguring (`outdated`, `lost`) is a state, not a broken
/// window: the row stays and the next frame retries it. Every other refusal is
/// repeated identically on the next frame, so the row is released instead.
fn is_transient(failure: &PresentFailure) -> bool {
    matches!(
        failure.class(),
        Some(
            PresentFailureClass::Timeout
                | PresentFailureClass::Outdated
                | PresentFailureClass::Lost
        )
    )
}

/// Upper bound on simultaneously attached secondary-window surfaces.
///
/// Deliberately the platform's own OS-window ceiling rather than a second
/// number: the renderer holding a surface for a window the app cannot admit
/// would be a resource the app can never address.
pub const MAX_WINDOW_SURFACES: usize = bitty_platform::MAX_OS_WINDOWS;

/// One secondary window's presentation surface.
#[derive(Debug)]
pub struct WindowSurface {
    /// Identity of the OS window this surface presents.
    window: WindowId,
    /// The `wgpu` swap chain (or its headless fake).
    surface: Surface,
    /// Extent the surface is currently configured for, in physical pixels.
    extent: PhysicalSize,
    /// Window opacity the surface was configured with.
    opacity: f32,
}

impl WindowSurface {
    /// Identity of the window this row presents.
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// Extent the surface is configured for, in physical pixels.
    #[must_use]
    pub fn extent(&self) -> PhysicalSize {
        self.extent
    }

    /// Window opacity the surface was configured with (`1.0` = opaque).
    #[must_use]
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// The last composited frame of a *headless* row.
    ///
    /// `None` for a real swap chain — its pixels live on the GPU and are never
    /// read back — and for a headless row before its first present. This is the
    /// same readback the primary window's `headless_rgba` exposes, so a test
    /// can prove a sibling surface received a frame rather than inferring it
    /// from "no error".
    #[must_use]
    pub fn headless_rgba(&self) -> Option<Vec<u8>> {
        self.surface.headless_rgba()
    }
}

/// Why a secondary window could not be given, resized, or kept a surface.
#[derive(Debug)]
pub enum WindowSurfaceError {
    /// No owned [`GpuContext`] exists, so no surface can be created.
    ///
    /// The off-thread Wayland presenter holds the process's only context and
    /// the runtime keeps none, so a sibling surface cannot be created until
    /// that presenter grows multi-surface support. Headless runs have no
    /// context either.
    NoOwnedGpu,
    /// A surface is already attached for this window.
    Duplicate(WindowId),
    /// The set already holds [`MAX_WINDOW_SURFACES`] surfaces.
    OverCapacity {
        /// The enforced ceiling.
        max: usize,
    },
    /// No surface is attached for this window.
    Unknown(WindowId),
    /// The platform refused the surface (driver, compositor, or handles).
    Create(RenderError),
    /// The surface was created but could not be configured for its extent.
    Configure(RenderError),
    /// The requested extent is zero in at least one dimension: a surface
    /// cannot be configured for it (minimized/occluded window).
    ZeroExtent,
}

impl fmt::Display for WindowSurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOwnedGpu => write!(
                f,
                "no owned GPU context (off-thread presenter or headless run); a secondary surface cannot be created"
            ),
            Self::Duplicate(window) => {
                write!(f, "window {} already has a surface", window.get())
            }
            Self::OverCapacity { max } => {
                write!(f, "window surface set is at capacity ({max})")
            }
            Self::Unknown(window) => write!(f, "window {} has no surface", window.get()),
            Self::Create(error) => write!(f, "surface creation failed: {error}"),
            Self::Configure(error) => write!(f, "surface configure failed: {error}"),
            Self::ZeroExtent => write!(f, "surface extent is zero-sized"),
        }
    }
}

/// One window whose surface refused the frame it was asked to present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowPresentFailure {
    /// The window whose surface refused.
    window: WindowId,
    /// The refusal, already flattened to an owned string for logging.
    detail: String,
}

impl WindowPresentFailure {
    /// The window whose surface refused the frame.
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// The refusal detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// What one fan-out present did, per window.
///
/// `presented` and `skipped` are counts rather than lists so a fan-out with no
/// permanent failure allocates nothing on the present path; the dropped rows
/// are named because a drop is a lifecycle event. Every attached window is
/// therefore accounted for: `presented` accepted the frame, `skipped` refused
/// it for a reason that clears on its own, `failed` refused it permanently and
/// was released.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowPresentReport {
    /// Number of windows whose surface accepted the frame.
    presented: usize,
    /// Number of windows whose surface could not show this frame but is still
    /// attached (minimized/hidden swap chain, outdated or lost swap chain that
    /// the next frame retries).
    skipped: usize,
    /// Windows whose surface refused the frame permanently and were dropped.
    failed: Vec<WindowPresentFailure>,
}

impl WindowPresentReport {
    /// Number of windows whose surface accepted the frame.
    #[must_use]
    pub fn presented(&self) -> usize {
        self.presented
    }

    /// Number of windows whose surface skipped this frame but stayed attached.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Windows whose surface refused the frame permanently (and were dropped).
    #[must_use]
    pub fn failed(&self) -> &[WindowPresentFailure] {
        &self.failed
    }

    /// Whether the fan-out reached no surface at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.presented == 0 && self.skipped == 0 && self.failed.is_empty()
    }

    /// Number of surfaces dropped by this fan-out.
    #[must_use]
    pub fn dropped(&self) -> usize {
        self.failed.len()
    }
}

/// Bounded set of secondary-window presentation surfaces.
#[derive(Debug, Default)]
pub struct WindowSurfaces {
    rows: Vec<WindowSurface>,
}

impl WindowSurfaces {
    /// Empty set: no secondary window has a surface.
    #[must_use]
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// Number of attached secondary surfaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no secondary window has a surface.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether another surface may be attached.
    #[must_use]
    pub fn has_capacity(&self) -> bool {
        self.rows.len() < MAX_WINDOW_SURFACES
    }

    /// Whether `window` has an attached surface.
    #[must_use]
    pub fn contains(&self, window: WindowId) -> bool {
        self.rows.iter().any(|row| row.window == window)
    }

    /// The row attached for `window`.
    #[must_use]
    pub fn get(&self, window: WindowId) -> Option<&WindowSurface> {
        self.rows.iter().find(|row| row.window == window)
    }

    /// Every attached window, in attachment order.
    #[must_use]
    pub fn windows(&self) -> Vec<WindowId> {
        self.rows.iter().map(|row| row.window).collect()
    }

    /// One-line lifecycle summary: `id:WxH` per row, comma separated.
    ///
    /// Reads the extent the surface is actually configured for, so a log line
    /// can never describe a window as painting at a size its swap chain does
    /// not have.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{}:{}x{}",
                row.window.get(),
                row.extent.width(),
                row.extent.height()
            ));
        }
        out
    }

    /// Registers an already-created surface for `window`.
    ///
    /// This is the seam that keeps the fan-out testable without an adapter (a
    /// `Surface::headless` row composites in-process, exactly like the
    /// runtime's own headless present) and lets an embedder install a surface
    /// it created itself — that caller owns configuring it, including the
    /// palette the clear colour comes from. Capacity, duplicate, and
    /// zero-extent checks all run before the row is inserted, so a refusal
    /// leaves the set unchanged.
    ///
    /// # Errors
    ///
    /// - [`WindowSurfaceError::Duplicate`] when `window` already has a row.
    /// - [`WindowSurfaceError::OverCapacity`] at [`MAX_WINDOW_SURFACES`].
    /// - [`WindowSurfaceError::ZeroExtent`] for a zero-sized extent.
    pub fn adopt(
        &mut self,
        window: WindowId,
        surface: Surface,
        extent: PhysicalSize,
        opacity: f32,
    ) -> Result<(), WindowSurfaceError> {
        self.check_admission(window, extent)?;
        self.rows.push(WindowSurface {
            window,
            surface,
            extent,
            opacity,
        });
        Ok(())
    }

    /// Creates and configures a surface for `window` through `gpu`.
    ///
    /// Both driver steps happen before the row exists: `create_surface`
    /// against the shared [`GpuContext`], then `configure_with_opacity` for the
    /// window's own extent and opacity. A refusal from either returns a typed
    /// error and leaves the set unchanged.
    ///
    /// # Errors
    ///
    /// - [`WindowSurfaceError::Duplicate`] / [`WindowSurfaceError::OverCapacity`]
    ///   / [`WindowSurfaceError::ZeroExtent`] before any driver call.
    /// - [`WindowSurfaceError::Create`] when the platform refuses the surface.
    /// - [`WindowSurfaceError::Configure`] when the swap chain rejects the
    ///   configuration.
    pub fn attach(
        &mut self,
        window: WindowId,
        target: &SurfaceTarget,
        extent: PhysicalSize,
        opacity: f32,
        palette: ThemePalette,
        gpu: &GpuContext,
    ) -> Result<(), WindowSurfaceError> {
        self.check_admission(window, extent)?;
        let surface = gpu
            .create_surface(target)
            .map_err(WindowSurfaceError::Create)?;
        // The clear colour follows the live palette (including OSC 4/10/11
        // overrides), never `config.theme`: a sibling window must not paint a
        // different background than the primary.
        surface.set_theme_palette(palette);
        surface
            .configure_with_opacity(gpu, extent, opacity)
            .map_err(WindowSurfaceError::Configure)?;
        self.rows.push(WindowSurface {
            window,
            surface,
            extent,
            opacity,
        });
        Ok(())
    }

    /// Reconfigures an attached window's surface for a new extent.
    ///
    /// Called on the window's own `Resized` / `ScaleFactorChanged` event. A
    /// zero extent keeps the previous configuration (the window is
    /// minimized/occluded; the surface must not be configured for it) and
    /// reports `Ok`, matching [`map_resize_to_surface_extent`]'s contract.
    ///
    /// `gpu` is the device the surface belongs to. When the runtime owns no
    /// context it can still reconfigure a *headless* row (the fake's
    /// configuration is plain bookkeeping); a real surface without its device
    /// is refused rather than silently left at the old size.
    ///
    /// # Errors
    ///
    /// - [`WindowSurfaceError::Unknown`] when `window` has no surface.
    /// - [`WindowSurfaceError::Configure`] when the swap chain refuses it.
    /// - [`WindowSurfaceError::NoOwnedGpu`] for a real surface with no device.
    pub fn resize(
        &mut self,
        window: WindowId,
        extent: PhysicalSize,
        gpu: Option<&GpuContext>,
    ) -> Result<(), WindowSurfaceError> {
        let row = self
            .rows
            .iter_mut()
            .find(|row| row.window == window)
            .ok_or(WindowSurfaceError::Unknown(window))?;
        if map_resize_to_surface_extent(extent).is_none() {
            return Ok(());
        }
        match gpu {
            Some(gpu) => row
                .surface
                .configure_with_opacity(gpu, extent, row.opacity)
                .map_err(WindowSurfaceError::Configure)?,
            None if row.surface.is_headless() => row
                .surface
                .configure_headless_with_opacity(extent, row.opacity)
                .map_err(WindowSurfaceError::Configure)?,
            None => return Err(WindowSurfaceError::NoOwnedGpu),
        }
        row.extent = extent;
        Ok(())
    }

    /// Removes one window's surface, returning its row.
    ///
    /// The surviving rows keep their identity, extent, and order. Dropping the
    /// row releases the swap chain, which is what makes the OS window go away
    /// once its handle is released too.
    pub fn release(&mut self, window: WindowId) -> Option<WindowSurface> {
        let index = self.rows.iter().position(|row| row.window == window)?;
        Some(self.rows.remove(index))
    }

    /// Releases every surface, returning how many rows were dropped.
    ///
    /// Used when the primary GPU context goes away (detach, or the transfer of
    /// the primary surface to the off-thread presenter): a row created through
    /// that context can no longer be presented, and presenting a swap chain to
    /// a device that did not create it is a validation failure. Dropping the
    /// rows is the fail-closed answer.
    pub fn release_all(&mut self) -> usize {
        let dropped = self.rows.len();
        self.rows.clear();
        dropped
    }

    /// Presents one already-built frame to every attached surface.
    ///
    /// `gpu` selects the route exactly as the primary present does: `Some`
    /// presents through the real swap chain, `None` composites on the headless
    /// fake. Failure policy follows what the refusal means for the window:
    ///
    /// - a transient acquire failure (`timeout`, `outdated`, `lost`) keeps the
    ///   row — a hidden or briefly collapsed swap chain must not blank a live
    ///   window for good, and the next frame retries; it is reported as
    ///   [`WindowPresentReport::skipped`];
    /// - any other failure (out of memory, an invalid frame, a real surface
    ///   with no device) drops the row and names it in
    ///   [`WindowPresentReport::failed`], because retrying it every frame would
    ///   only repeat the same failure.
    ///
    /// Either way the primary's present is unaffected: nothing here can clear
    /// its confirmation, arm its retry, or fail its frame.
    pub fn present_all(
        &mut self,
        gpu: Option<&GpuContext>,
        draw_list: &DrawList,
        atlas: Option<(&[u8], AtlasDims)>,
        color_atlas: Option<(&[u8], AtlasDims)>,
    ) -> WindowPresentReport {
        let mut report = WindowPresentReport::default();
        let mut index = 0;
        while index < self.rows.len() {
            let outcome = match gpu {
                Some(gpu) => self.rows[index]
                    .surface
                    .present_draw_list_color_classified(gpu, draw_list, atlas, color_atlas)
                    .map(|_| ()),
                None => self.rows[index]
                    .surface
                    .headless_present_color(draw_list, atlas, color_atlas)
                    .map(|_| ())
                    .map_err(PresentFailure::Error),
            };
            match outcome {
                Ok(()) => {
                    report.presented += 1;
                    index += 1;
                }
                Err(failure) if is_transient(&failure) => {
                    report.skipped += 1;
                    index += 1;
                }
                Err(failure) => {
                    let row = self.rows.remove(index);
                    report.failed.push(WindowPresentFailure {
                        window: row.window,
                        detail: failure.detail().to_owned(),
                    });
                }
            }
        }
        report
    }

    /// Admission checks shared by [`Self::adopt`] and [`Self::attach`].
    ///
    /// Runs before any driver work so a refused window never leaves a
    /// half-created swap chain behind.
    fn check_admission(
        &self,
        window: WindowId,
        extent: PhysicalSize,
    ) -> Result<(), WindowSurfaceError> {
        if self.contains(window) {
            return Err(WindowSurfaceError::Duplicate(window));
        }
        if !self.has_capacity() {
            return Err(WindowSurfaceError::OverCapacity {
                max: MAX_WINDOW_SURFACES,
            });
        }
        if map_resize_to_surface_extent(extent).is_none() {
            return Err(WindowSurfaceError::ZeroExtent);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{ExtentPx, RectPx};
    use crate::grid::FillRect;

    fn window(raw: u64) -> WindowId {
        WindowId::from_raw_public(raw)
    }

    fn headless(extent: PhysicalSize) -> Surface {
        Surface::headless(extent).expect("non-zero headless extent")
    }

    /// One opaque fill covering the whole frame, so a painted surface is
    /// distinguishable from an unpainted one by a single pixel.
    fn fill_frame(color: [u8; 4]) -> DrawList {
        DrawList {
            generation: 1,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: ExtentPx::new(8, 4),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![RectPx::new(0, 0, 8, 4)],
            },
            fills: vec![FillRect {
                rect: RectPx::new(0, 0, 8, 4),
                color,
            }],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            overlay_card_rounded_fills: vec![],
            overlay_content_fills: vec![],
            overlay_rounded_fills: vec![],
            glyphs: vec![],
            images: vec![],
        }
    }

    #[test]
    fn empty_set_holds_no_surface() {
        let surfaces = WindowSurfaces::new();
        assert!(surfaces.is_empty());
        assert_eq!(surfaces.len(), 0);
        assert!(surfaces.has_capacity());
        assert!(surfaces.windows().is_empty());
        assert_eq!(surfaces.summary(), "");
    }

    #[test]
    fn each_window_gets_its_own_extent() {
        let mut surfaces = WindowSurfaces::new();
        surfaces
            .adopt(
                window(2),
                headless(PhysicalSize::new(640, 480)),
                PhysicalSize::new(640, 480),
                1.0,
            )
            .expect("first secondary window");
        surfaces
            .adopt(
                window(3),
                headless(PhysicalSize::new(300, 200)),
                PhysicalSize::new(300, 200),
                0.9,
            )
            .expect("second secondary window");
        assert_eq!(surfaces.windows(), vec![window(2), window(3)]);
        assert_eq!(
            surfaces.get(window(3)).map(WindowSurface::extent),
            Some(PhysicalSize::new(300, 200)),
            "a row keeps the extent its own swap chain was configured for"
        );
        assert!((surfaces.get(window(3)).map(WindowSurface::opacity).unwrap() - 0.9).abs() < 1e-6);
        assert_eq!(surfaces.summary(), "2:640x480,3:300x200");
    }

    #[test]
    fn every_attached_surface_paints_the_presented_frame() {
        let mut surfaces = WindowSurfaces::new();
        surfaces
            .adopt(
                window(2),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        surfaces
            .adopt(
                window(3),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        let frame = fill_frame([0x00, 0x00, 0xFF, 0xFF]);
        let report = surfaces.present_all(None, &frame, None, None);
        assert_eq!(
            report.presented(),
            2,
            "the fan-out reports every attached window"
        );
        assert!(!report.is_empty());
        assert!(report.failed().is_empty());
        assert_eq!(report.dropped(), 0);
        // Both fakes composite the same scene: the proof that the second
        // window's surface really received the frame.
        for id in [window(2), window(3)] {
            let surface = surfaces.get(id).expect("row survives");
            let rgba = surface.headless_rgba().expect("composited pixels");
            assert_eq!(&rgba[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        }
        assert_eq!(
            surfaces.windows(),
            vec![window(2), window(3)],
            "a successful present keeps every row"
        );
    }

    #[test]
    fn a_failing_surface_is_dropped_without_touching_its_siblings() {
        let mut surfaces = WindowSurfaces::new();
        surfaces
            .adopt(
                window(2),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        surfaces
            .adopt(
                window(3),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        // A draw list that needs atlas texels it was not given is refused by
        // every surface (bitty#3055), which is a reachable present failure
        // without an adapter.
        let mut frame = fill_frame([0xFF, 0x00, 0x00, 0xFF]);
        frame.glyphs = vec![crate::grid::GlyphInstance {
            dest: [0, 0],
            size: [4, 4],
            uv: [0.0; 4],
            color: [0xFF; 4],
            clip: None,
            source: crate::grid::GlyphSource::Atlas {
                slot: crate::atlas::AtlasSlot {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
            },
        }];
        let report = surfaces.present_all(None, &frame, None, None);
        assert_eq!(
            report.dropped(),
            2,
            "both rows refuse the inconsistent frame"
        );
        assert_eq!(report.presented(), 0);
        assert_eq!(
            report.skipped(),
            0,
            "an invalid frame is not a transient swap-chain state"
        );
        assert!(!report.is_empty());
        assert_eq!(
            report
                .failed()
                .iter()
                .map(WindowPresentFailure::window)
                .collect::<Vec<_>>(),
            vec![window(2), window(3)]
        );
        assert!(
            surfaces.is_empty(),
            "a refusing row is released instead of retrying forever"
        );
        for failure in report.failed() {
            assert!(!failure.detail().is_empty());
        }
    }

    #[test]
    fn admission_is_bounded_and_refuses_duplicates_before_any_row_exists() {
        let mut surfaces = WindowSurfaces::new();
        for raw in 1..=MAX_WINDOW_SURFACES as u64 {
            let extent = PhysicalSize::new(10, 10);
            surfaces
                .adopt(window(raw), headless(extent), extent, 1.0)
                .expect("attachment within the bound");
        }
        assert_eq!(surfaces.len(), MAX_WINDOW_SURFACES);
        assert!(!surfaces.has_capacity());
        match surfaces.adopt(
            window(999),
            headless(PhysicalSize::new(10, 10)),
            PhysicalSize::new(10, 10),
            1.0,
        ) {
            Err(WindowSurfaceError::OverCapacity { max }) => assert_eq!(max, MAX_WINDOW_SURFACES),
            other => panic!("over-capacity admission must fail closed: {other:?}"),
        }
        assert_eq!(surfaces.len(), MAX_WINDOW_SURFACES, "nothing was added");

        let mut single = WindowSurfaces::new();
        single
            .adopt(
                window(7),
                headless(PhysicalSize::new(4, 4)),
                PhysicalSize::new(4, 4),
                1.0,
            )
            .unwrap();
        match single.adopt(
            window(7),
            headless(PhysicalSize::new(4, 4)),
            PhysicalSize::new(4, 4),
            1.0,
        ) {
            Err(WindowSurfaceError::Duplicate(duplicate)) => assert_eq!(duplicate, window(7)),
            other => panic!("a duplicate window must fail closed: {other:?}"),
        }
        assert_eq!(single.len(), 1);
        // A zero extent cannot be configured, so it is refused up front
        // instead of creating a swap chain nobody can present.
        match single.adopt(
            window(8),
            headless(PhysicalSize::new(4, 4)),
            PhysicalSize::new(0, 4),
            1.0,
        ) {
            Err(WindowSurfaceError::ZeroExtent) => {}
            other => panic!("a zero extent must fail closed: {other:?}"),
        }
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn releasing_one_window_leaves_the_other_painting() {
        let mut surfaces = WindowSurfaces::new();
        surfaces
            .adopt(
                window(2),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        surfaces
            .adopt(
                window(3),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        let released = surfaces.release(window(2)).expect("first secondary closes");
        assert_eq!(released.window(), window(2));
        assert_eq!(surfaces.windows(), vec![window(3)]);
        assert!(
            surfaces.release(window(2)).is_none(),
            "release is idempotent"
        );
        let report = surfaces.present_all(None, &fill_frame([0x11, 0x22, 0x33, 0xFF]), None, None);
        assert_eq!(report.presented(), 1, "only the surviving window paints");
    }

    #[test]
    fn releasing_all_drops_every_row() {
        let mut surfaces = WindowSurfaces::new();
        for raw in [2, 3] {
            surfaces
                .adopt(
                    window(raw),
                    headless(PhysicalSize::new(8, 4)),
                    PhysicalSize::new(8, 4),
                    1.0,
                )
                .unwrap();
        }
        assert_eq!(surfaces.release_all(), 2);
        assert!(surfaces.is_empty());
        assert_eq!(surfaces.release_all(), 0, "an empty set releases nothing");
    }

    #[test]
    fn a_resize_reconfigures_only_the_named_window() {
        let mut surfaces = WindowSurfaces::new();
        surfaces
            .adopt(
                window(2),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        surfaces
            .adopt(
                window(3),
                headless(PhysicalSize::new(8, 4)),
                PhysicalSize::new(8, 4),
                1.0,
            )
            .unwrap();
        surfaces
            .resize(window(2), PhysicalSize::new(16, 12), None)
            .expect("headless rows reconfigure without an adapter");
        assert_eq!(
            surfaces.get(window(2)).map(WindowSurface::extent),
            Some(PhysicalSize::new(16, 12))
        );
        assert_eq!(
            surfaces.get(window(3)).map(WindowSurface::extent),
            Some(PhysicalSize::new(8, 4)),
            "the sibling keeps its own extent"
        );
        // The reconfigured row really is at the new extent.
        let frame = fill_frame([0xFF, 0xFF, 0x00, 0xFF]);
        let report = surfaces.present_all(None, &frame, None, None);
        assert_eq!(report.presented(), 2);
        let rgba = surfaces
            .get(window(2))
            .unwrap()
            .headless_rgba()
            .expect("composited pixels");
        assert_eq!(rgba.len(), 16 * 12 * 4, "the swap chain follows the resize");
        // A zero extent is a minimized window: keep the last configuration.
        surfaces
            .resize(window(2), PhysicalSize::new(0, 0), None)
            .expect("zero extents are skipped");
        assert_eq!(
            surfaces.get(window(2)).map(WindowSurface::extent),
            Some(PhysicalSize::new(16, 12))
        );
        assert!(matches!(
            surfaces.resize(window(9), PhysicalSize::new(4, 4), None),
            Err(WindowSurfaceError::Unknown(_))
        ));
    }
}
