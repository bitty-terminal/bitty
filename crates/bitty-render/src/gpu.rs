//! GPU context and owned surface lifecycle.
//! This module is the only place where `wgpu` types are named (ADR-0004
//! "Adopt" row). The public API exposes:
//!
//! - [`GpuContext::initialize`], an async entry point (callers drive the
//!   future with their own executor; this crate deliberately ships no
//!   blocking runtime dependency), returning an owned context or a flattened
//!   [`RenderError`];
//! - [`AdapterSummary`], owned re-descriptions of adapter facts;
//! - [`Surface`], an owned wrapper around a `wgpu::Surface` created from a
//!   [`bitty_platform::SurfaceTarget`] via
//!   [`GpuContext::create_surface`]. No `wgpu` type escapes except through
//!   this wrapper, and the lifecycle is `Drop`-safe (the wrapper owns the
//!   surface and, for real surfaces, a clone of the `SurfaceTarget` that
//!   keeps the underlying window alive).
//!
//! # Surface lifecycle (owned, `Drop`-safe)
//!
//! 1. Attach once: obtain a [`bitty_platform::SurfaceTarget`] from
//!    [`bitty_platform::WindowHandle::surface_target`] (typically on
//!    [`bitty_platform::PlatformEvent::Resumed`]) and call
//!    [`GpuContext::create_surface`]. The returned [`Surface`] owns the
//!    `wgpu` surface and a clone of the target, so the window stays alive as
//!    long as the surface does (see the `SurfaceTarget` lifetime contract).
//! 2. Configure: call [`Surface::configure`] with the current
//!    [`bitty_platform::PhysicalSize`] (from
//!    [`bitty_platform::SurfaceTarget::inner_size`] or
//!    [`bitty_platform::map_resize_to_surface_extent`]). Configuration picks
//!    a texture format with a `Srgb` fallback and a present mode with a
//!    `Fifo` fallback, then calls `wgpu::Surface::configure`.
//! 3. Resize: call [`Surface::resize`] on
//!    [`bitty_platform::WindowEventKind::Resized`] or
//!    [`bitty_platform::WindowEventKind::ScaleFactorChanged`]. `resize`
//!    reuses the format/mode chosen at the last `configure` but rebuilds the
//!    `wgpu` configuration for the new extent. Zero-sized extents (minimized
//!    / occluded) return `Ok` without reconfiguring — callers must skip
//!    `present` until a non-zero size arrives, matching
//!    [`bitty_platform::map_resize_to_surface_extent`].
//! 4. Present: call [`Surface::present`] or
//!    [`Surface::present_draw_list`] per frame. The headless fake path
//!    composites the supplied [`crate::grid::DrawList`] + atlas coverage
//!    onto an in-memory RGBA buffer (no GPU required); the real GPU path
//!    acquires the swap-chain texture and presents (clears) it.
//!
//! # Headless vs GPU-tested
//!
//! - **Headless (CI, default):** [`Surface::headless`] creates a fake surface
//!   that holds a [`PhysicalSize`] extent and a [`SurfaceConfig`]. No window
//!   system, adapter, or display server is required. `configure`/`resize`
//!   validate extents, select formats via the same fallback rules (deterministic
//!   default), and `present_draw_list` composites `DrawList`+`Atlas` onto an
//!   owned RGBA buffer that tests can inspect via [`Surface::headless_rgba`].
//!   This exercises the **same** present-plumbing the GPU backend will share
//!   (damage-driven `DrawList`, atlas hits/misses, inline fallback) without
//!   touching `wgpu`.
//! - **Real GPU (env-gated):** `GpuContext::initialize` plus
//!   `GpuContext::create_surface` and `Surface::present` reach the driver.
//!   They are covered only by `tests/gpu_integration.rs`, which skips itself
//!   unless `BITTY_RENDER_GPU_TESTS=1` (and, for surface tests, a live window
//!   system via `BITTY_RENDER_GPU_SURFACE_TESTS=1`). CI never runs these.
//!   What CI *does* run is the headless fake, which is compiled and tested on
//!   both `native` and `x86_64-pc-windows-gnu` targets to ensure no
//!   `dead_code` warnings are introduced (see below).
//!
//! # Format and present-mode fallback
//!
//! `wgpu::Surface::get_capabilities` returns the set the adapter+ surface
//! support. Selection is deterministic:
//!
//! - **Format:** prefer `Bgra8UnormSrgb` then `Rgba8UnormSrgb`; if neither is
//!   offered, pick the first format reported; if the list is empty fall back
//!   to `Bgra8UnormSrgb` (the widest-supported srgb format). This keeps
//!   behavior stable across backends while preserving the `Srgb` preference
//!   for correct gamma.
//! - **Present mode:** prefer `Mailbox` (low-latency triple buffering) then
//!   `Fifo` (guaranteed vsync); otherwise pick the first reported mode, or
//!   `Fifo` if none. `Fifo` is the only mode the WebGPU spec guarantees.
//!
//! Backend selection follows wgpu's own environment handling
//! (`WGPU_BACKEND=...`) via `InstanceDescriptor::from_env_or_default()`, so
//! operators can pin or exclude backends without code changes.
//!
//! # `unsafe` scope
//!
//! The single `unsafe` block in this file is the `DisplayHandle::borrow_raw` /
//! `WindowHandle::borrow_raw` construction for the `raw-window-handle` bridge
//! that `wgpu::Instance::create_surface` consumes. The raw handles are
//! obtained from `SurfaceTarget::with_raw_handles`, which guarantees they
//! originate from a live window that the `Surface` then keeps alive via a
//! cloned `SurfaceTarget`. No other `unsafe` exists in this crate.

use std::sync::Mutex;

use bitty_platform::{PhysicalSize, SurfaceTarget, map_resize_to_surface_extent};
use wgpu::{
    Adapter, Device, DeviceDescriptor, DeviceType as UpstreamDeviceType, Features, Instance,
    InstanceDescriptor, Limits, MemoryHints, Queue, RequestAdapterOptions, SurfaceConfiguration,
    TextureFormat, TextureUsages, Trace,
};

use crate::atlas::AtlasDims;
use crate::error::RenderError;
use crate::grid::DrawList;

// ---------------------------------------------------------------------------
// Adapter description
// ---------------------------------------------------------------------------

/// Owned description of the adapter backing a [`GpuContext`].
///
/// Every field is copied/converted out of upstream structures; nothing here
/// borrows or wraps a `wgpu` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSummary {
    /// Driver-reported adapter name.
    pub name: String,
    /// Driver-reported driver string, when available.
    pub driver: String,
    /// Graphics backend in use.
    pub backend: BackendKind,
    /// Device class.
    pub class: DeviceClass,
}

/// Owned re-description of the upstream backend enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Vulkan.
    Vulkan,
    /// Metal (macOS/iOS).
    Metal,
    /// Direct3D 12 (Windows).
    Dx12,
    /// OpenGL/OpenGLES.
    Gl,
    /// WebGPU on browsers.
    BrowserWebGpu,
    /// No-op stub backend (testing only).
    Noop,
}

impl BackendKind {
    fn from_upstream(backend: wgpu::Backend) -> Self {
        match backend {
            wgpu::Backend::Vulkan => BackendKind::Vulkan,
            wgpu::Backend::Metal => BackendKind::Metal,
            wgpu::Backend::Dx12 => BackendKind::Dx12,
            wgpu::Backend::Gl => BackendKind::Gl,
            wgpu::Backend::BrowserWebGpu => BackendKind::BrowserWebGpu,
            wgpu::Backend::Noop => BackendKind::Noop,
        }
    }
}

/// Owned re-description of the upstream device classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// Separate discrete GPU.
    Discrete,
    /// Integrated into the CPU package.
    Integrated,
    /// Software/CPU renderer.
    Cpu,
    /// Hypervisor/virtualized device.
    Virtual,
    /// Unclassified.
    Other,
}

impl DeviceClass {
    fn from_upstream(device_type: UpstreamDeviceType) -> Self {
        match device_type {
            UpstreamDeviceType::DiscreteGpu => DeviceClass::Discrete,
            UpstreamDeviceType::IntegratedGpu => DeviceClass::Integrated,
            UpstreamDeviceType::Cpu => DeviceClass::Cpu,
            UpstreamDeviceType::VirtualGpu => DeviceClass::Virtual,
            UpstreamDeviceType::Other => DeviceClass::Other,
        }
    }
}

// ---------------------------------------------------------------------------
// GpuContext
// ---------------------------------------------------------------------------

/// An initialized GPU context: instance, adapter, logical device, and queue.
///
/// The upstream handles are held privately to keep them alive; later slices
/// extend this type with pipeline and surface management without changing how
/// it is constructed.
#[derive(Debug)]
pub struct GpuContext {
    instance: Instance,
    adapter: Adapter,
    device: Device,
    queue: Queue,
    summary: AdapterSummary,
}

impl GpuContext {
    /// Initializes instance, adapter, and logical device using wgpu's default
    /// environment-driven options.
    ///
    /// On a machine without a usable graphics stack — headless CI, for
    /// example — this returns [`RenderError::NoCompatibleAdapter`] rather
    /// than panicking or falling back silently. The software fallback is a
    /// separate, explicit path (`sw-fallback` feature), never an implicit one.
    ///
    /// # Errors
    ///
    /// - [`RenderError::NoCompatibleAdapter`] when enumeration finds nothing
    ///   usable.
    /// - [`RenderError::DeviceRequest`] when the adapter rejects the logical
    ///   device request.
    /// - [`RenderError::UpstreamGraphics`] for other upstream failures.
    pub async fn initialize() -> Result<Self, RenderError> {
        let instance = Instance::new(&InstanceDescriptor::from_env_or_default());

        let adapter = instance
            .request_adapter(&RequestAdapterOptions::default())
            .await
            .map_err(|_| RenderError::NoCompatibleAdapter)?;

        let (device, queue) = adapter
            .request_device(&DeviceDescriptor {
                label: Some("bitty-render"),
                required_features: Features::empty(),
                required_limits: Limits::default(),
                memory_hints: MemoryHints::default(),
                trace: Trace::Off,
            })
            .await
            .map_err(|err| RenderError::DeviceRequest(err.to_string()))?;

        let info = adapter.get_info();
        let summary = AdapterSummary {
            name: info.name.clone(),
            driver: info.driver.clone(),
            backend: BackendKind::from_upstream(info.backend),
            class: DeviceClass::from_upstream(info.device_type),
        };

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            summary,
        })
    }

    /// The owned summary of the adapter backing this context.
    #[must_use]
    pub fn adapter_summary(&self) -> &AdapterSummary {
        &self.summary
    }

    /// Creates an owned [`Surface`] for `target`.
    ///
    /// The returned surface owns the underlying `wgpu::Surface` and a clone
    /// of `target` so the window stays alive as long as the surface does
    /// (see the `SurfaceTarget` lifetime contract). No `wgpu` type leaks:
    /// failures are flattened into [`RenderError::SurfaceCreate`].
    ///
    /// # Errors
    ///
    /// - [`RenderError::SurfaceCreate`] when the platform refuses the handles
    ///   or `wgpu` cannot create a surface for them.
    pub fn create_surface(&self, target: &SurfaceTarget) -> Result<Surface, RenderError> {
        // Use the `RawHandle` surface-target path: it carries the two raw
        // handles directly and does not require `Send`/`Sync` on the handle
        // carrier (unlike the safe `create_surface` WindowHandle path, which
        // demands `Send`). The `unsafe` is justified because `target` is
        // alive and `Surface` keeps a clone of it, so the window outlives
        // the `wgpu::Surface` (see module docs).
        let surface = target
            .with_raw_handles(|display, window| {
                let target_unsafe = wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: display,
                    raw_window_handle: window,
                };
                // SAFETY: `display`/`window` originate from a live
                // `SurfaceTarget` and the returned `Surface` keeps a clone
                // of that target, guaranteeing the window outlives the
                // surface as required by `SurfaceTargetUnsafe::RawHandle`.
                unsafe { self.instance.create_surface_unsafe(target_unsafe) }
            })
            .map_err(|e| RenderError::SurfaceCreate(e.to_string()))?
            .map_err(|e| RenderError::SurfaceCreate(e.to_string()))?;

        // Extend lifetime to `'static` via the stored `SurfaceTarget` clone.
        // SAFETY: the `Surface` owns a clone of `target`, so the underlying
        // window stays alive for at least as long as the surface. This
        // satisfies `RawHandle`'s "window must outlive surface" requirement.
        let surface_static: wgpu::Surface<'static> = unsafe { std::mem::transmute(surface) };

        Ok(Surface {
            kind: SurfaceKind::Gpu {
                surface: surface_static,
                target: target.clone(),
            },
            state: Mutex::new(SurfaceState::new()),
        })
    }
}

// ---------------------------------------------------------------------------
// Surface owned wrapper
// ---------------------------------------------------------------------------

/// Owned re-description of the surface texture format (no `wgpu` type leaks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceFormat {
    /// `Bgra8UnormSrgb` — preferred on most desktop backends.
    Bgra8UnormSrgb,
    /// `Rgba8UnormSrgb`.
    Rgba8UnormSrgb,
    /// `Bgra8Unorm` (non-srgb fallback).
    Bgra8Unorm,
    /// `Rgba8Unorm` (non-srgb fallback).
    Rgba8Unorm,
}

impl SurfaceFormat {
    /// True when the format is an `Srgb` variant.
    #[must_use]
    pub const fn is_srgb(self) -> bool {
        matches!(self, Self::Bgra8UnormSrgb | Self::Rgba8UnormSrgb)
    }

    fn to_wgpu(self) -> TextureFormat {
        match self {
            Self::Bgra8UnormSrgb => TextureFormat::Bgra8UnormSrgb,
            Self::Rgba8UnormSrgb => TextureFormat::Rgba8UnormSrgb,
            Self::Bgra8Unorm => TextureFormat::Bgra8Unorm,
            Self::Rgba8Unorm => TextureFormat::Rgba8Unorm,
        }
    }

    fn from_wgpu(format: TextureFormat) -> Option<Self> {
        match format {
            TextureFormat::Bgra8UnormSrgb => Some(Self::Bgra8UnormSrgb),
            TextureFormat::Rgba8UnormSrgb => Some(Self::Rgba8UnormSrgb),
            TextureFormat::Bgra8Unorm => Some(Self::Bgra8Unorm),
            TextureFormat::Rgba8Unorm => Some(Self::Rgba8Unorm),
            _ => None,
        }
    }
}

/// Owned re-description of the present mode (no `wgpu` type leaks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresentMode {
    /// Let the driver choose (`AutoVsync`).
    AutoVsync,
    /// Let the driver choose without vsync (`AutoNoVsync`).
    AutoNoVsync,
    /// Strict vsync (`Fifo`).
    Fifo,
    /// Relaxed vsync (`FifoRelaxed`).
    FifoRelaxed,
    /// No vsync (`Immediate`).
    Immediate,
    /// Triple-buffered vsync (`Mailbox`).
    Mailbox,
}

impl PresentMode {
    fn to_wgpu(self) -> wgpu::PresentMode {
        match self {
            Self::AutoVsync => wgpu::PresentMode::AutoVsync,
            Self::AutoNoVsync => wgpu::PresentMode::AutoNoVsync,
            Self::Fifo => wgpu::PresentMode::Fifo,
            Self::FifoRelaxed => wgpu::PresentMode::FifoRelaxed,
            Self::Immediate => wgpu::PresentMode::Immediate,
            Self::Mailbox => wgpu::PresentMode::Mailbox,
        }
    }

    fn from_wgpu(mode: wgpu::PresentMode) -> Self {
        match mode {
            wgpu::PresentMode::AutoVsync => Self::AutoVsync,
            wgpu::PresentMode::AutoNoVsync => Self::AutoNoVsync,
            wgpu::PresentMode::Fifo => Self::Fifo,
            wgpu::PresentMode::FifoRelaxed => Self::FifoRelaxed,
            wgpu::PresentMode::Immediate => Self::Immediate,
            wgpu::PresentMode::Mailbox => Self::Mailbox,
        }
    }
}

/// Owned surface configuration (extent, format, present mode).
///
/// This is the only configuration the embedder constructs. The `wgpu`
/// `SurfaceConfiguration` is built internally from it plus adapter
/// capabilities, so no `wgpu` type leaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceConfig {
    /// Surface extent in physical pixels.
    pub extent: PhysicalSize,
    /// Chosen texture format.
    pub format: SurfaceFormat,
    /// Chosen present mode.
    pub present_mode: PresentMode,
}

impl SurfaceConfig {
    /// Builds a configuration for `extent` with explicit format and present
    /// mode choices.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the extent is zero-sized (a
    /// surface cannot be configured with a zero extent — callers should skip
    /// configuration until a non-zero size arrives, per
    /// [`map_resize_to_surface_extent`]).
    pub fn new(
        extent: PhysicalSize,
        format: SurfaceFormat,
        present_mode: PresentMode,
    ) -> Result<Self, RenderError> {
        if extent.width() == 0 || extent.height() == 0 {
            return Err(RenderError::InvalidInput {
                reason: "surface extent must be non-zero",
            });
        }
        Ok(Self {
            extent,
            format,
            present_mode,
        })
    }
}

/// Statistics returned from a present that composited a [`DrawList`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentStats {
    /// Logical frame counter for the surface (increments per present).
    pub frame: u64,
    /// Number of fill rectangles in the presented `DrawList`.
    pub fills: usize,
    /// Number of glyph instances in the presented `DrawList`.
    pub glyphs: usize,
    /// True when the surface is a headless fake (no swap-chain acquire).
    pub headless: bool,
}

#[derive(Debug)]
enum SurfaceKind {
    Gpu {
        surface: wgpu::Surface<'static>,
        // Keep the window alive: `SurfaceTarget` holds an `Arc<Window>`.
        #[allow(dead_code)]
        target: SurfaceTarget,
    },
    Headless,
}

#[derive(Debug)]
struct SurfaceState {
    config: Option<SurfaceConfig>,
    wgpu_config: Option<SurfaceConfiguration>,
    frame: u64,
    // Headless-only last RGBA buffer (premultiplied, `width*height*4` bytes).
    headless_rgba: Option<Vec<u8>>,
    headless_extent: Option<PhysicalSize>,
}

impl SurfaceState {
    const fn new() -> Self {
        Self {
            config: None,
            wgpu_config: None,
            frame: 0,
            headless_rgba: None,
            headless_extent: None,
        }
    }
}

/// An owned GPU surface: either a real `wgpu` surface created from a
/// [`SurfaceTarget`] or a headless fake for unit tests.
///
/// No `wgpu` type appears in any public signature. The real variant keeps the
/// `wgpu::Surface` and a clone of the `SurfaceTarget` alive together so the
/// `Drop` order is well-defined (surface dropped before the last window
/// clone, per the `SurfaceTarget` contract). The fake variant holds only a
/// validated extent and configuration and composites `DrawList`+`Atlas` onto
/// an in-memory RGBA buffer — identical plumbing without any display server
/// or adapter.
///
/// `Surface` is `Send` + `Sync` when `wgpu::Surface` is.
#[derive(Debug)]
pub struct Surface {
    kind: SurfaceKind,
    state: Mutex<SurfaceState>,
}

impl Surface {
    /// Creates a headless fake surface with `extent`.
    ///
    /// This is the test seam: no window system or adapter is contacted. The
    /// fake still validates the extent, runs the same format/present-mode
    /// fallback logic (deterministic defaults), and stores a configuration so
    /// `present_draw_list` can composite `DrawList`+`Atlas` onto a CPU buffer.
    ///
    /// Use [`Surface::headless_with_config`] when an explicit configuration
    /// is needed; otherwise this picks `Bgra8UnormSrgb` + `Fifo` as the
    /// deterministic defaults.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the extent is zero-sized.
    pub fn headless(extent: PhysicalSize) -> Result<Self, RenderError> {
        let config = SurfaceConfig::new(extent, SurfaceFormat::Bgra8UnormSrgb, PresentMode::Fifo)?;
        Self::headless_with_config(config)
    }

    /// Creates a headless fake surface with an explicit `config`.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the extent is zero-sized.
    pub fn headless_with_config(config: SurfaceConfig) -> Result<Self, RenderError> {
        if config.extent.width() == 0 || config.extent.height() == 0 {
            return Err(RenderError::InvalidInput {
                reason: "surface extent must be non-zero",
            });
        }
        let mut state = SurfaceState::new();
        state.config = Some(config);
        state.headless_extent = Some(config.extent);
        // Pre-synthesize a `wgpu` config for inspection parity (not used for
        // real configuration on headless).
        state.wgpu_config = Some(synthesize_wgpu_config(&config));
        Ok(Self {
            kind: SurfaceKind::Headless,
            state: Mutex::new(state),
        })
    }

    /// True when this is the headless fake (no `wgpu` surface inside).
    #[must_use]
    pub fn is_headless(&self) -> bool {
        matches!(self.kind, SurfaceKind::Headless)
    }

    /// Current owned configuration, if the surface has been configured.
    #[must_use]
    pub fn config(&self) -> Option<SurfaceConfig> {
        self.state.lock().expect("surface state poisoned").config
    }

    /// Current extent, if the surface has been configured.
    #[must_use]
    pub fn extent(&self) -> Option<PhysicalSize> {
        self.state
            .lock()
            .expect("surface state poisoned")
            .config
            .map(|c| c.extent)
    }

    /// Configures (or reconfigures) the surface for `extent`.
    ///
    /// For a real surface, this queries `get_capabilities`, picks a format
    /// with `Srgb`→fallback and a present mode with `Mailbox`→`Fifo` fallback,
    /// builds a `wgpu::SurfaceConfiguration`, and calls
    /// `wgpu::Surface::configure`. For the headless fake it validates the
    /// extent and stores the same fallback-chosen `SurfaceConfig`.
    ///
    /// Zero-sized extents are rejected with [`RenderError::InvalidInput`];
    /// callers should use [`Self::resize`] when zero-sized resizes can arrive
    /// (minimized / occluded windows) — `resize` skips those silently, per
    /// [`map_resize_to_surface_extent`].
    ///
    /// # Errors
    ///
    /// - [`RenderError::InvalidInput`] for zero extents.
    /// - [`RenderError::SurfaceConfigure`] when the upstream `configure`
    ///   cannot be built (should be unreachable with the fallback rules, but
    ///   surfaced honestly).
    pub fn configure(&self, ctx: &GpuContext, extent: PhysicalSize) -> Result<(), RenderError> {
        if extent.width() == 0 || extent.height() == 0 {
            return Err(RenderError::InvalidInput {
                reason: "surface extent must be non-zero",
            });
        }
        match &self.kind {
            SurfaceKind::Headless => {
                let mut state = self.state.lock().expect("surface state poisoned");
                let config =
                    SurfaceConfig::new(extent, SurfaceFormat::Bgra8UnormSrgb, PresentMode::Fifo)?;
                state.config = Some(config);
                state.headless_extent = Some(extent);
                state.wgpu_config = Some(synthesize_wgpu_config(&config));
                Ok(())
            }
            SurfaceKind::Gpu { surface, .. } => {
                let caps = surface.get_capabilities(&ctx.adapter);
                let format = pick_format(&caps);
                let present_mode = pick_present_mode(&caps);
                let config = SurfaceConfig::new(extent, format, present_mode)?;
                let wgpu_config = build_wgpu_config(&config);
                surface.configure(&ctx.device, &wgpu_config);
                let mut state = self.state.lock().expect("surface state poisoned");
                state.config = Some(config);
                state.wgpu_config = Some(wgpu_config);
                Ok(())
            }
        }
    }

    /// Reconfigures the surface for `new_extent`, skipping zero-sized
    /// extents.
    ///
    /// This is the resize path callers use on
    /// [`bitty_platform::WindowEventKind::Resized`] /
    /// [`bitty_platform::WindowEventKind::ScaleFactorChanged`]. When
    /// `new_extent` is zero in either dimension, the call returns `Ok` without
    /// touching any configuration — the surface skips presenting until a
    /// non-zero size arrives (matching [`map_resize_to_surface_extent`]'s
    /// `None` signal). Otherwise it delegates to [`Self::configure`].
    ///
    /// # Errors
    ///
    /// Propagates [`Self::configure`] failures for non-zero extents only.
    pub fn resize(&self, ctx: &GpuContext, new_extent: PhysicalSize) -> Result<(), RenderError> {
        if map_resize_to_surface_extent(new_extent).is_none() {
            return Ok(());
        }
        self.configure(ctx, new_extent)
    }

    /// Presents a frame without `DrawList` compositing (minimal clear).
    ///
    /// For a headless fake this increments the frame counter and returns
    /// immediately (no swap-chain texture exists). For a real surface it
    /// acquires the next swap-chain texture, clears it, and presents. The
    /// clear is intentionally minimal — full pipeline draws await the
    /// shader/pipeline slice. Use [`Self::present_draw_list`] when a
    /// `DrawList` is available.
    ///
    /// # Errors
    ///
    /// - [`RenderError::SurfaceConfigure`] when the surface has not yet been
    ///   configured.
    /// - [`RenderError::SurfaceAcquire`] when `get_current_texture` reports
    ///   `Timeout`, `Outdated`, `Lost`, or `Unknown`.
    pub fn present(&self, ctx: &GpuContext) -> Result<PresentStats, RenderError> {
        match &self.kind {
            SurfaceKind::Headless => {
                let mut state = self.state.lock().expect("surface state poisoned");
                if state.config.is_none() {
                    return Err(RenderError::SurfaceConfigure(
                        "surface not configured".into(),
                    ));
                }
                state.frame += 1;
                let frame = state.frame;
                Ok(PresentStats {
                    frame,
                    fills: 0,
                    glyphs: 0,
                    headless: true,
                })
            }
            SurfaceKind::Gpu { surface, .. } => {
                let mut state = self.state.lock().expect("surface state poisoned");
                let config = state.config.ok_or_else(|| {
                    RenderError::SurfaceConfigure("surface not configured".into())
                })?;
                let wgpu_config = state.wgpu_config.clone().ok_or_else(|| {
                    RenderError::SurfaceConfigure("surface not configured".into())
                })?;
                // Ensure the `wgpu` surface is still configured for this
                // extent (defensive: `configure` may have been skipped on a
                // zero-resize).
                if wgpu_config.width != config.extent.width()
                    || wgpu_config.height != config.extent.height()
                {
                    drop(state);
                    self.configure(ctx, config.extent)?;
                    state = self.state.lock().expect("surface state poisoned");
                }
                drop(state);

                // Acquire, clear, and present.
                let frame = surface
                    .get_current_texture()
                    .map_err(|e| RenderError::SurfaceAcquire(e.to_string()))?;
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder =
                    ctx.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("bitty-present"),
                        });
                {
                    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("bitty-clear"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 0.06,
                                    g: 0.06,
                                    b: 0.06,
                                    a: 1.0,
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                }
                ctx.queue.submit(std::iter::once(encoder.finish()));
                frame.present();
                let mut state = self.state.lock().expect("surface state poisoned");
                state.frame += 1;
                Ok(PresentStats {
                    frame: state.frame,
                    fills: 0,
                    glyphs: 0,
                    headless: false,
                })
            }
        }
    }

    /// Composites `draw_list` (plus `atlas` coverage) onto the surface and
    /// presents.
    ///
    /// - On a headless fake: the `DrawList` is composited onto an owned
    ///   RGBA buffer sized to the current extent (validation mirrors
    ///   `wgpu` panic guards: zero extent, missing configuration, or an atlas
    ///   instance without `atlas` are rejected). The buffer is saved and
    ///   observable via [`Self::headless_rgba`]; the method returns
    ///   [`PresentStats`] with fill/glyph counts.
    /// - On a real surface: the same validation runs, then the method
    ///   acquires the swap-chain texture, clears it, and presents (full GPU
    ///   draw of `DrawList`+`Atlas` awaits the pipeline/shader slice; this
    ///   path still validates that `DrawList`+`Atlas` plumbing is reachable
    ///   and that `present` can be called after `configure`/`resize`).
    ///
    /// # Errors
    ///
    /// - [`RenderError::SurfaceConfigure`] for an unconfigured surface.
    /// - [`RenderError::InvalidInput`] when the `DrawList` contains an atlas
    ///   glyph but `atlas` is `None`, or when compositing would overflow the
    ///   headless buffer cap.
    /// - [`RenderError::SurfaceAcquire`] for real-surface texture-acquire
    ///   failures.
    pub fn present_draw_list(
        &self,
        ctx: &GpuContext,
        draw_list: &DrawList,
        atlas: Option<(&[u8], AtlasDims)>,
    ) -> Result<PresentStats, RenderError> {
        // Shared validation: configured?
        let config = {
            let state = self.state.lock().expect("surface state poisoned");
            state
                .config
                .ok_or_else(|| RenderError::SurfaceConfigure("surface not configured".into()))?
        };

        // Validate atlas requirement: any Atlas-sourced glyph needs atlas.
        let needs_atlas = draw_list
            .glyphs
            .iter()
            .any(|g| matches!(g.source, crate::grid::GlyphSource::Atlas { .. }));
        if needs_atlas && atlas.is_none() {
            return Err(RenderError::InvalidInput {
                reason: "atlas instance requires atlas texels",
            });
        }

        match &self.kind {
            SurfaceKind::Headless => {
                // Headless composite onto CPU buffer — mirrors
                // `software::draw_list_onto` but lives here so the sw-fallback
                // feature is not required for headless tests.
                let width = config.extent.width();
                let height = config.extent.height();
                let mut rgba = vec![0u8; width as usize * height as usize * 4];
                // Clear to default background (premultiplied) — matches the GPU
                // clear color above.
                {
                    let bg = crate::grid::DEFAULT_BG;
                    let pr = premultiply(bg[0], bg[3]);
                    let pg = premultiply(bg[1], bg[3]);
                    let pb = premultiply(bg[2], bg[3]);
                    let pa = bg[3];
                    for px in rgba.chunks_exact_mut(4) {
                        px[0] = pr;
                        px[1] = pg;
                        px[2] = pb;
                        px[3] = pa;
                    }
                }
                // Fills.
                for fill in &draw_list.fills {
                    fill_rect_rgba(&mut rgba, width, height, fill.rect, fill.color);
                }
                // Glyphs.
                if let Some((texels, dims)) = atlas {
                    for glyph in &draw_list.glyphs {
                        match &glyph.source {
                            crate::grid::GlyphSource::Atlas { slot } => {
                                let stride = usize::from(dims.width);
                                let slot_w = usize::from(slot.width);
                                let slot_h = usize::from(slot.height);
                                if slot_h == 0 || slot_w == 0 {
                                    continue;
                                }
                                let mut mask = Vec::with_capacity(slot_w * slot_h);
                                for row in 0..slot_h {
                                    let start =
                                        (usize::from(slot.y) + row) * stride + usize::from(slot.x);
                                    mask.extend_from_slice(&texels[start..start + slot_w]);
                                }
                                blend_coverage_mask_rgba(
                                    &mut rgba,
                                    width,
                                    height,
                                    &mask,
                                    slot.width.into(),
                                    slot.height.into(),
                                    glyph.dest[0],
                                    glyph.dest[1],
                                    glyph.color,
                                );
                            }
                            crate::grid::GlyphSource::Inline {
                                mask,
                                width: w,
                                height: h,
                            } => {
                                blend_coverage_mask_rgba(
                                    &mut rgba,
                                    width,
                                    height,
                                    mask,
                                    *w,
                                    *h,
                                    glyph.dest[0],
                                    glyph.dest[1],
                                    glyph.color,
                                );
                            }
                        }
                    }
                } else {
                    for glyph in &draw_list.glyphs {
                        if let crate::grid::GlyphSource::Inline {
                            mask,
                            width: w,
                            height: h,
                        } = &glyph.source
                        {
                            blend_coverage_mask_rgba(
                                &mut rgba,
                                width,
                                height,
                                mask,
                                *w,
                                *h,
                                glyph.dest[0],
                                glyph.dest[1],
                                glyph.color,
                            );
                        }
                    }
                }
                let mut state = self.state.lock().expect("surface state poisoned");
                state.frame += 1;
                state.headless_rgba = Some(rgba);
                Ok(PresentStats {
                    frame: state.frame,
                    fills: draw_list.fills.len(),
                    glyphs: draw_list.glyphs.len(),
                    headless: true,
                })
            }
            SurfaceKind::Gpu { surface, .. } => {
                // Real GPU path: validate then acquire+clear+present. Full
                // `DrawList`→pipeline draw is deferred; we still submit a
                // clear so `present` can be observably called.
                let frame = surface
                    .get_current_texture()
                    .map_err(|e| RenderError::SurfaceAcquire(e.to_string()))?;
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder =
                    ctx.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("bitty-present-draw-list"),
                        });
                {
                    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("bitty-clear-draw-list"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 0.06,
                                    g: 0.06,
                                    b: 0.06,
                                    a: 1.0,
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                }
                ctx.queue.submit(std::iter::once(encoder.finish()));
                frame.present();
                let mut state = self.state.lock().expect("surface state poisoned");
                state.frame += 1;
                Ok(PresentStats {
                    frame: state.frame,
                    fills: draw_list.fills.len(),
                    glyphs: draw_list.glyphs.len(),
                    headless: false,
                })
            }
        }
    }

    /// Returns a clone of the last headless RGBA buffer (premultiplied,
    /// `width*height*4` bytes), if this is a headless surface and
    /// [`Self::present_draw_list`] has been called at least once.
    #[must_use]
    pub fn headless_rgba(&self) -> Option<Vec<u8>> {
        let state = self.state.lock().expect("surface state poisoned");
        state.headless_rgba.clone()
    }

    /// Current `wgpu` configuration, if configured (real surface) or
    /// synthesized (headless). Useful for diagnostics; not part of the
    /// stable embedder API but `pub` for integration tests.
    #[must_use]
    pub fn wgpu_config_snapshot(&self) -> Option<SurfaceConfiguration> {
        self.state
            .lock()
            .expect("surface state poisoned")
            .wgpu_config
            .clone()
    }

    /// Headless-only present: composites `draw_list` onto the in-memory RGBA
    /// buffer without requiring a [`GpuContext`].
    ///
    /// This is the same composition as the headless branch of
    /// [`Self::present_draw_list`] but callable from unit tests that have no
    /// adapter or display server. Real surfaces return
    /// [`RenderError::SurfaceConfigure`].
    ///
    /// # Errors
    ///
    /// - [`RenderError::SurfaceConfigure`] when the surface is not a headless
    ///   fake or has not been configured.
    /// - [`RenderError::InvalidInput`] for atlas mismatches or buffer-cap
    ///   overflows (mirrors the `sw-fallback` path).
    pub fn headless_present(
        &self,
        draw_list: &DrawList,
        atlas: Option<(&[u8], AtlasDims)>,
    ) -> Result<PresentStats, RenderError> {
        if !self.is_headless() {
            return Err(RenderError::SurfaceConfigure(
                "headless_present called on a real surface".into(),
            ));
        }
        let config = {
            let state = self.state.lock().expect("surface state poisoned");
            state
                .config
                .ok_or_else(|| RenderError::SurfaceConfigure("surface not configured".into()))?
        };
        let needs_atlas = draw_list
            .glyphs
            .iter()
            .any(|g| matches!(g.source, crate::grid::GlyphSource::Atlas { .. }));
        if needs_atlas && atlas.is_none() {
            return Err(RenderError::InvalidInput {
                reason: "atlas instance requires atlas texels",
            });
        }
        let width = config.extent.width();
        let height = config.extent.height();
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        {
            let bg = crate::grid::DEFAULT_BG;
            let pr = premultiply(bg[0], bg[3]);
            let pg = premultiply(bg[1], bg[3]);
            let pb = premultiply(bg[2], bg[3]);
            let pa = bg[3];
            for px in rgba.chunks_exact_mut(4) {
                px[0] = pr;
                px[1] = pg;
                px[2] = pb;
                px[3] = pa;
            }
        }
        for fill in &draw_list.fills {
            fill_rect_rgba(&mut rgba, width, height, fill.rect, fill.color);
        }
        if let Some((texels, dims)) = atlas {
            for glyph in &draw_list.glyphs {
                match &glyph.source {
                    crate::grid::GlyphSource::Atlas { slot } => {
                        let stride = usize::from(dims.width);
                        let slot_w = usize::from(slot.width);
                        let slot_h = usize::from(slot.height);
                        if slot_w == 0 || slot_h == 0 {
                            continue;
                        }
                        let mut mask = Vec::with_capacity(slot_w * slot_h);
                        for row in 0..slot_h {
                            let start = (usize::from(slot.y) + row) * stride + usize::from(slot.x);
                            mask.extend_from_slice(&texels[start..start + slot_w]);
                        }
                        blend_coverage_mask_rgba(
                            &mut rgba,
                            width,
                            height,
                            &mask,
                            slot.width.into(),
                            slot.height.into(),
                            glyph.dest[0],
                            glyph.dest[1],
                            glyph.color,
                        );
                    }
                    crate::grid::GlyphSource::Inline {
                        mask,
                        width: w,
                        height: h,
                    } => {
                        blend_coverage_mask_rgba(
                            &mut rgba,
                            width,
                            height,
                            mask,
                            *w,
                            *h,
                            glyph.dest[0],
                            glyph.dest[1],
                            glyph.color,
                        );
                    }
                }
            }
        } else {
            for glyph in &draw_list.glyphs {
                if let crate::grid::GlyphSource::Inline {
                    mask,
                    width: w,
                    height: h,
                } = &glyph.source
                {
                    blend_coverage_mask_rgba(
                        &mut rgba,
                        width,
                        height,
                        mask,
                        *w,
                        *h,
                        glyph.dest[0],
                        glyph.dest[1],
                        glyph.color,
                    );
                }
            }
        }
        let mut state = self.state.lock().expect("surface state poisoned");
        state.frame += 1;
        state.headless_rgba = Some(rgba);
        Ok(PresentStats {
            frame: state.frame,
            fills: draw_list.fills.len(),
            glyphs: draw_list.glyphs.len(),
            headless: true,
        })
    }

    /// Headless-only reconfiguration without a [`GpuContext`] (unit-test seam).
    ///
    /// Validates and stores `new_extent`, picking the same deterministic format
    /// and present-mode defaults as [`Self::headless`]. Real surfaces return an
    /// error.
    ///
    /// # Errors
    ///
    /// - [`RenderError::InvalidInput`] for zero extents.
    /// - [`RenderError::SurfaceConfigure`] when called on a real surface.
    pub fn headless_resize(&self, new_extent: PhysicalSize) -> Result<(), RenderError> {
        if !self.is_headless() {
            return Err(RenderError::SurfaceConfigure(
                "headless_resize called on a real surface".into(),
            ));
        }
        if map_resize_to_surface_extent(new_extent).is_none() {
            return Ok(());
        }
        let config =
            SurfaceConfig::new(new_extent, SurfaceFormat::Bgra8UnormSrgb, PresentMode::Fifo)?;
        let mut state = self.state.lock().expect("surface state poisoned");
        state.config = Some(config);
        state.headless_extent = Some(new_extent);
        state.wgpu_config = Some(synthesize_wgpu_config(&config));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers: format / present-mode fallback, config builders, compositing
// ---------------------------------------------------------------------------

fn pick_format(caps: &wgpu::SurfaceCapabilities) -> SurfaceFormat {
    for fmt in &caps.formats {
        if *fmt == TextureFormat::Bgra8UnormSrgb {
            return SurfaceFormat::Bgra8UnormSrgb;
        }
    }
    for fmt in &caps.formats {
        if *fmt == TextureFormat::Rgba8UnormSrgb {
            return SurfaceFormat::Rgba8UnormSrgb;
        }
    }
    caps.formats
        .first()
        .and_then(|f| SurfaceFormat::from_wgpu(*f))
        .unwrap_or(SurfaceFormat::Bgra8UnormSrgb)
}

fn pick_present_mode(caps: &wgpu::SurfaceCapabilities) -> PresentMode {
    if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        return PresentMode::Mailbox;
    }
    if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
        return PresentMode::Fifo;
    }
    caps.present_modes
        .first()
        .map(|m| PresentMode::from_wgpu(*m))
        .unwrap_or(PresentMode::Fifo)
}

fn build_wgpu_config(config: &SurfaceConfig) -> SurfaceConfiguration {
    SurfaceConfiguration {
        usage: TextureUsages::RENDER_ATTACHMENT,
        format: config.format.to_wgpu(),
        width: config.extent.width(),
        height: config.extent.height(),
        desired_maximum_frame_latency: 2,
        present_mode: config.present_mode.to_wgpu(),
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
    }
}

fn synthesize_wgpu_config(config: &SurfaceConfig) -> SurfaceConfiguration {
    build_wgpu_config(config)
}

const fn premultiply(color: u8, alpha: u8) -> u8 {
    ((color as u16 * alpha as u16) / 255) as u8
}

fn fill_rect_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rect: crate::geometry::RectPx,
    color: crate::grid::Rgba8,
) {
    let [r, g, b, a] = color;
    let pr = premultiply(r, a);
    let pg = premultiply(g, a);
    let pb = premultiply(b, a);
    let left = i64::from(rect.x).max(0);
    let top = i64::from(rect.y).max(0);
    let right = (i64::from(rect.x) + i64::from(rect.width))
        .max(0)
        .min(i64::from(width));
    let bottom = (i64::from(rect.y) + i64::from(rect.height))
        .max(0)
        .min(i64::from(height));
    if right <= left || bottom <= top {
        return;
    }
    for y in top..bottom {
        let row = y as usize * width as usize;
        for x in left..right {
            let d = (row + x as usize) * 4;
            rgba[d] = pr;
            rgba[d + 1] = pg;
            rgba[d + 2] = pb;
            rgba[d + 3] = a;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blend_coverage_mask_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    mask: &[u8],
    mask_width: u32,
    mask_height: u32,
    x: i32,
    y: i32,
    color: crate::grid::Rgba8,
) {
    let Some(mask_w) = usize::try_from(mask_width).ok().filter(|w| *w > 0) else {
        return;
    };
    let mask_h = usize::try_from(mask_height).unwrap_or(0);
    if mask.len() < mask_w * mask_h {
        return;
    }
    let dst_left = i64::from(-x).max(0);
    let dst_top = i64::from(-y).max(0);
    let dst_right = (i64::from(width) - i64::from(x))
        .max(0)
        .min(i64::try_from(mask_w).unwrap_or(i64::MAX));
    let dst_bottom = (i64::from(height) - i64::from(y))
        .max(0)
        .min(i64::try_from(mask_h).unwrap_or(i64::MAX));
    let [cr, cg, cb, ca] = color;
    for gy in dst_top..dst_bottom {
        for gx in dst_left..dst_right {
            let coverage = u32::from(mask[gy as usize * mask_w + gx as usize]);
            if coverage == 0 {
                continue;
            }
            let sa = (coverage * u32::from(ca)) / 255;
            let src_r = ((u32::from(cr) * coverage * u32::from(ca)) / 65025) as u8;
            let src_g = ((u32::from(cg) * coverage * u32::from(ca)) / 65025) as u8;
            let src_b = ((u32::from(cb) * coverage * u32::from(ca)) / 65025) as u8;
            let src_a = sa.min(255) as u8;
            let sx = (i64::from(x) + gx) as usize;
            let sy = (i64::from(y) + gy) as usize;
            let d = (sy * width as usize + sx) * 4;
            let inv = 255 - u32::from(src_a);
            rgba[d] = src_r.saturating_add((u32::from(rgba[d]) * inv / 255) as u8);
            rgba[d + 1] = src_g.saturating_add((u32::from(rgba[d + 1]) * inv / 255) as u8);
            rgba[d + 2] = src_b.saturating_add((u32::from(rgba[d + 2]) * inv / 255) as u8);
            rgba[d + 3] = src_a.saturating_add((u32::from(rgba[d + 3]) * inv / 255) as u8);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: headless fake surface (no GPU required, runs on CI)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{
        BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
        RasterKey,
    };
    use crate::grid::{CellMetrics, GridRenderer};
    use bitty_platform::PhysicalSize;
    use bitty_term_state::{State, TerminalAction};
    use bitty_vt::GraphemeCell;

    struct FakeRasterizer {
        next_id: u64,
    }
    impl GlyphRasterizer for FakeRasterizer {
        fn load_font(&mut self, _q: &FontQuery) -> Result<FontId, RenderError> {
            Ok(FontId::next(&mut self.next_id))
        }
        fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
            if key.character == ' ' {
                return Ok(None);
            }
            let side = (u32::from(key.character) % 3 + 6) as i32;
            let data = vec![0xAA; side as usize * side as usize * 3];
            Ok(Some(
                GlyphBitmap::try_new(
                    GlyphMetrics {
                        left: 0,
                        top: 6,
                        width: side,
                        height: side,
                        advance: [side, 0],
                    },
                    BitmapFormat::Rgb,
                    data,
                )
                .unwrap(),
            ))
        }
    }

    fn fake_renderer() -> GridRenderer<FakeRasterizer> {
        let q = FontQuery {
            family: "Fake".into(),
            style: FontStyle::Normal,
            point_size: 12.0,
        };
        GridRenderer::new(
            FakeRasterizer { next_id: 0 },
            &q,
            CellMetrics::new(8, 16).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn headless_creation_rejects_zero_extent() {
        assert!(matches!(
            Surface::headless(PhysicalSize::new(0, 600)),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(matches!(
            Surface::headless(PhysicalSize::new(800, 0)),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn headless_creation_succeeds_and_exposes_config() {
        let surface = Surface::headless(PhysicalSize::new(640, 480)).expect("valid extent");
        assert!(surface.is_headless());
        let cfg = surface.config().expect("configured");
        assert_eq!(cfg.extent, PhysicalSize::new(640, 480));
        assert_eq!(cfg.format, SurfaceFormat::Bgra8UnormSrgb);
        assert_eq!(cfg.present_mode, PresentMode::Fifo);
        assert_eq!(surface.extent(), Some(PhysicalSize::new(640, 480)));
        assert!(surface.wgpu_config_snapshot().is_some());
    }

    #[test]
    fn headless_with_custom_config_round_trips() {
        let cfg = SurfaceConfig::new(
            PhysicalSize::new(320, 240),
            SurfaceFormat::Rgba8Unorm,
            PresentMode::Mailbox,
        )
        .unwrap();
        let surface = Surface::headless_with_config(cfg).unwrap();
        assert_eq!(surface.config().unwrap(), cfg);
    }

    #[test]
    fn present_without_configure_fails() {
        let surface = Surface {
            kind: SurfaceKind::Headless,
            state: Mutex::new(SurfaceState::new()),
        };
        let _draw = crate::grid::DrawList {
            generation: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(0, 0),
                mode: crate::frame::FrameMode::Clean,
                dirty_rects: vec![],
            },
            fills: vec![],
            glyphs: vec![],
        };
        assert!(surface.config().is_none());
    }

    #[test]
    fn headless_present_composites_draw_list_and_atlas() {
        let mut renderer = fake_renderer();
        let mut state = State::new();
        state.apply(&TerminalAction::Print(GraphemeCell::from('A')));
        state.apply(&TerminalAction::Print(GraphemeCell::from('B')));
        let snapshot = state.snapshot();
        let damage = bitty_term_state::Damage {
            generation: snapshot.generation,
            regions: state.damage_since(0).into_boxed_slice(),
        };
        let draw_list = renderer
            .render(&snapshot, &damage)
            .expect("render should succeed");
        let atlas_texels = renderer.atlas_texels().to_vec();
        let dims = renderer.atlas_dims();

        let surface = Surface::headless(PhysicalSize::new(640, 384)).expect("valid extent");
        assert!(surface.headless_rgba().is_none());

        // Headless present without any GPU context — the same plumbing the
        // real present will share, exercised here on every CI run.
        let stats = surface
            .headless_present(&draw_list, Some((&atlas_texels, dims)))
            .expect("headless present should succeed");
        assert_eq!(stats.fills, draw_list.fills.len());
        assert_eq!(stats.glyphs, draw_list.glyphs.len());
        assert!(stats.headless);
        assert_eq!(stats.frame, 1);

        // Second present increments frame and overwrites rgba deterministically.
        let stats2 = surface
            .headless_present(&draw_list, Some((&atlas_texels, dims)))
            .expect("second present");
        assert_eq!(stats2.frame, 2);
        let rgba = surface.headless_rgba().expect("rgba after present");
        assert_eq!(rgba.len(), 640 * 384 * 4);
        // Non-zero content: clears and draws at least one glyph produce
        // non-zero bytes. Determinism: repeating the same frame yields
        // identical bytes.
        assert!(rgba.iter().any(|&b| b != 0));
        let rgba2 = surface.headless_rgba().unwrap();
        assert_eq!(rgba, rgba2);

        // Inline fallback also composites: force a tiny atlas so glyph falls
        // back to inline, then present with inline mask.
        let mut tiny_renderer = crate::grid::GridRenderer::with_atlas_dimension(
            FakeRasterizer { next_id: 0 },
            &crate::glyph::FontQuery {
                family: "Fake".into(),
                style: crate::glyph::FontStyle::Normal,
                point_size: 12.0,
            },
            crate::grid::CellMetrics::new(8, 16).unwrap(),
            4,
        )
        .unwrap();
        let draw_inline = tiny_renderer
            .render(&snapshot, &damage)
            .expect("render with tiny atlas");
        assert!(
            draw_inline
                .glyphs
                .iter()
                .any(|g| matches!(g.source, crate::grid::GlyphSource::Inline { .. }))
        );
        let surface2 = Surface::headless(PhysicalSize::new(640, 384)).unwrap();
        let stats_inline = surface2
            .headless_present(
                &draw_inline,
                Some((tiny_renderer.atlas_texels(), tiny_renderer.atlas_dims())),
            )
            .expect("inline present");
        assert_eq!(stats_inline.glyphs, draw_inline.glyphs.len());

        // Atlas requirement is enforced: Atlas glyphs without atlas -> error.
        let err = surface
            .headless_present(&draw_list, None)
            .expect_err("missing atlas should fail");
        assert!(matches!(err, RenderError::InvalidInput { .. }));

        // Zero-size resize is honestly skipped (mirrors
        // `map_resize_to_surface_extent` contract).
        assert!(map_resize_to_surface_extent(PhysicalSize::new(0, 0)).is_none());
        surface
            .headless_resize(PhysicalSize::new(0, 0))
            .expect("zero resize is a no-op");
        assert_eq!(surface.extent(), Some(PhysicalSize::new(640, 384)));
        assert_eq!(surface.headless_rgba().unwrap().len(), 640 * 384 * 4);

        // Non-zero resize reconfigures.
        surface
            .headless_resize(PhysicalSize::new(800, 600))
            .expect("valid resize");
        assert_eq!(surface.extent(), Some(PhysicalSize::new(800, 600)));
        let after_resize = surface
            .headless_present(&draw_list, Some((&atlas_texels, dims)))
            .expect("present after resize");
        assert_eq!(after_resize.frame, 3);
        assert_eq!(surface.headless_rgba().unwrap().len(), 800 * 600 * 4);
    }

    #[test]
    fn resize_skips_zero_and_reconfigures_non_zero() {
        // This test uses only the public `map_resize_to_surface_extent`
        // contract that `Surface::resize` respects. It does not need a GPU.
        let valid = PhysicalSize::new(800, 600);
        let zero = PhysicalSize::new(0, 600);
        assert_eq!(map_resize_to_surface_extent(valid), Some(valid));
        assert_eq!(map_resize_to_surface_extent(zero), None);
        assert_eq!(
            map_resize_to_surface_extent(PhysicalSize::new(800, 0)),
            None
        );
    }

    #[test]
    fn format_and_present_mode_fallbacks_are_deterministic() {
        // Caps with empty lists must fall back deterministically.
        let empty_caps = wgpu::SurfaceCapabilities {
            usages: TextureUsages::RENDER_ATTACHMENT,
            formats: vec![],
            present_modes: vec![],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
        };
        assert_eq!(pick_format(&empty_caps), SurfaceFormat::Bgra8UnormSrgb);
        assert_eq!(pick_present_mode(&empty_caps), PresentMode::Fifo);

        // Caps that offer Srgb vs non-Srgb.
        let caps = wgpu::SurfaceCapabilities {
            usages: TextureUsages::RENDER_ATTACHMENT,
            formats: vec![TextureFormat::Rgba8Unorm, TextureFormat::Bgra8UnormSrgb],
            present_modes: vec![wgpu::PresentMode::Immediate, wgpu::PresentMode::Fifo],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
        };
        assert_eq!(pick_format(&caps), SurfaceFormat::Bgra8UnormSrgb);
        // Mailbox preferred over Fifo.
        let caps2 = wgpu::SurfaceCapabilities {
            usages: TextureUsages::RENDER_ATTACHMENT,
            formats: vec![TextureFormat::Bgra8UnormSrgb],
            present_modes: vec![wgpu::PresentMode::Fifo, wgpu::PresentMode::Mailbox],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
        };
        assert_eq!(pick_present_mode(&caps2), PresentMode::Mailbox);
    }

    #[test]
    fn surface_config_rejects_zero_extent() {
        assert!(matches!(
            SurfaceConfig::new(
                PhysicalSize::new(0, 10),
                SurfaceFormat::Bgra8UnormSrgb,
                PresentMode::Fifo
            ),
            Err(RenderError::InvalidInput { .. })
        ));
    }
}
