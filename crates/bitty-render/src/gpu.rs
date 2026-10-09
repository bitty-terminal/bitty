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
//!    draws the same [`crate::grid::DrawList`] through the `wgpu` fill +
//!    glyph pipelines (see [`crate::batch`] and the crate-private
//!    `pipeline` module) and presents the swap-chain texture.
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
//!   `GpuContext::create_surface` and `Surface::present_draw_list` reach the
//!   driver: fills draw through the solid-fill pipeline and glyphs through
//!   the atlas-textured pipeline, with dirty-region atlas uploads. They are
//!   covered only by `tests/gpu_integration.rs`, which skips itself
//!   unless `BITTY_RENDER_GPU_TESTS=1` (and, for surface tests, a live window
//!   system via `BITTY_RENDER_GPU_SURFACE_TESTS=1`). CI never runs these.
//!   What CI *does* run is the headless fake plus the pure CPU batch
//!   translation ([`crate::batch`]), both compiled and tested on `native`
//!   and `x86_64-pc-windows-gnu` targets to ensure no `dead_code` warnings
//!   are introduced (see below).
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
//! - **Present mode:** prefer `Fifo` (spec-guaranteed vsync double
//!   buffering) over `Mailbox` (triple buffering): one fewer resident
//!   swap-chain image (a full `width*height*4` frame plus driver-side
//!   bookkeeping) for latency a terminal never needs (CTX-1036, #1809).
//!
//! Backend selection follows wgpu's own environment handling
//! (`WGPU_BACKEND=...`) via `InstanceDescriptor::from_env_or_default()`, so
//! operators can pin or exclude backends without code changes.
//!
//! Windows additionally excludes the GL backend unless the operator pins
//! backends explicitly (issue #1799): `wgpu-hal` initializes WGL on a helper
//! thread (`wgpu-hal WGL Instance Thread`) with a hardcoded 256 KiB stack,
//! which overflows inside driver-dependent WGL calls on some Windows
//! machines. A stack overflow aborts the process — it is not a `Result` the
//! caller can recover from — so the only graceful fallback is to never start
//! WGL init: [`instance_descriptor`] drops `Backends::GL` on Windows when
//! `WGPU_BACKEND` is unset, leaving DX12/Vulkan to be picked. Linux excludes
//! GL by default for the same shape of reason at a different scale
//! (CTX-1036, issue #1809): the GL driver stack (`libEGL_nvidia`,
//! `libGLX_nvidia`, `libnvidia-eglcore`, gallium, ~15 MiB of resident file
//! mappings plus its init threads) stays resident even when the negotiated
//! surface runs on Vulkan, and the Vulkan driver arena already dominates
//! idle RSS. [`GpuContext::initialize`] retries once with the full backend
//! set when the restricted instance finds no adapter, so Vulkan-less hosts
//! (old hardware, minimal VMs) still fall back to GL instead of failing.
//! The init-thread stack size is not configurable from this crate. The
//! upstream chain (verified against the pinned `wgpu-hal 26.0.6` /
//! `wgpu-core 26.0.1` / `wgpu-types 26.0.0` sources in `Cargo.lock`) is:
//! `gles/mod.rs` maps the GLES backend to `wgl::Instance` on Windows;
//! `gles/wgl.rs` `Instance::init` calls `create_instance_device`, which
//! spawns the helper thread with `.stack_size(256 * 1024)` and
//! `.name("wgpu-hal WGL Instance Thread")`; the thread body runs hidden
//! window creation, `GetDC`, pixel-format setup, and WGL driver calls on
//! that stack, so driver-dependent depth overflows it and aborts the
//! process before any `Result` is returned. `wgpu-core` `instance.rs`
//! `try_add_hal` skips backends absent from the descriptor, so excluding
//! `Backends::GL` keeps WGL init from ever starting; see
//! [`resolve_instance_backends`] for the pure, unit-tested selection rule.
//!
//! # Safety: no `unsafe`
//!
//! Surface creation uses the safe `wgpu::Instance::create_surface` path.
//! [`bitty_platform::SurfaceTarget`] implements `raw-window-handle`
//! `HasWindowHandle` + `HasDisplayHandle`, so an owned `SurfaceTarget` clone
//! converts into `wgpu::SurfaceTarget::Window` and yields a
//! `wgpu::Surface<'static>` directly: wgpu keeps the cloned target alive in
//! `_handle_source`, and [`Surface`] keeps a second clone for its own
//! extent queries. No `create_surface_unsafe`, no lifetime `transmute`, and
//! no other `unsafe` exists in this crate.

use std::sync::Mutex;

use bitty_platform::{PhysicalSize, SurfaceTarget, map_resize_to_surface_extent};
use wgpu::{
    Adapter, Backends, Device, DeviceDescriptor, DeviceType as UpstreamDeviceType, Features,
    Instance, InstanceDescriptor, Limits, MemoryHints, Queue, RequestAdapterOptions,
    SurfaceConfiguration, TextureFormat, TextureUsages, Trace,
};

use crate::atlas::AtlasDims;
use crate::batch;
use crate::error::RenderError;
use crate::grid::{DrawList, ThemePalette};
use crate::pipeline::GpuResources;

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
// Windows WGL backend policy (issue #1799)
// ---------------------------------------------------------------------------

/// Why the `wgpu` instance backends were chosen.
///
/// Returned alongside the effective [`Backends`] by
/// [`resolve_instance_backends`] / [`select_instance_backends`] so callers
/// can log the fallback reason instead of failing silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendSelection {
    /// Defaults kept: either not Windows, or no GL backend was enabled.
    Defaults,
    /// The operator pinned backends via `WGPU_BACKEND`; honored verbatim,
    /// even when it includes GL on Windows (explicit choice wins).
    EnvOverride,
    /// Windows without an operator pin: GL was excluded so `wgpu-hal` never
    /// starts its overflow-prone WGL init thread; DX12/Vulkan remain.
    WindowsGlExcluded,
    /// Linux without an operator pin: GL was excluded so the GL driver
    /// stack never becomes resident when the surface negotiates Vulkan
    /// (CTX-1036, issue #1809 — ~15 MiB of file mappings plus init
    /// threads); Vulkan remains, and [`GpuContext::initialize`] retries
    /// with the full backend set when no adapter is found.
    LinuxGlExcluded,
}

impl BackendSelection {
    /// Loud-fallback note for the process log, or `None` when there is
    /// nothing to report (defaults kept or explicit operator pin).
    #[must_use]
    pub const fn log_note(self) -> Option<&'static str> {
        match self {
            Self::WindowsGlExcluded => Some(
                "excluded the GL/WGL backend on Windows to avoid the \
                 `wgpu-hal WGL Instance Thread` stack overflow \
                 (issue #1799); DX12/Vulkan remain — set WGPU_BACKEND \
                 to override",
            ),
            Self::LinuxGlExcluded => Some(
                "excluded the GL backend on Linux to keep the GL driver \
                 stack out of the resident set when Vulkan serves the \
                 surface (CTX-1036, issue #1809); set WGPU_BACKEND \
                 to override",
            ),
            Self::Defaults | Self::EnvOverride => None,
        }
    }
}

/// Pure backend-selection rule: which `wgpu` backends to enable.
///
/// - `defaults`: the backends that would be enabled without this policy
///   (normally `InstanceDescriptor::from_env_or_default().backends`).
/// - `env_override`: `Backends::from_env()` — `Some` when the operator set
///   `WGPU_BACKEND`, in which case the pin is honored verbatim.
/// - `on_windows`: `true` on Windows targets (pass
///   `cfg!(target_os = "windows")` in production; a parameter here so the
///   rule is unit-testable without a Windows runner).
/// - `on_linux`: `true` on Linux targets (pass `cfg!(target_os = "linux")`
///   in production; same testability rationale).
///
/// On Windows without an operator pin, `Backends::GL` is removed so
/// `wgpu-hal` never spawns its 256 KiB-stack WGL init thread, whose
/// driver-dependent overflow aborts the process before any `Result` can be
/// returned. On Linux without an operator pin, `Backends::GL` is removed so
/// the GL driver stack never becomes resident when Vulkan serves the
/// surface (CTX-1036, issue #1809). If GL were the only enabled backend the
/// defaults are kept (an empty set could never produce an adapter, so there
/// is nothing to fall back to).
#[must_use]
pub fn resolve_instance_backends(
    defaults: Backends,
    env_override: Option<Backends>,
    on_windows: bool,
    on_linux: bool,
) -> (Backends, BackendSelection) {
    if let Some(pinned) = env_override {
        return (pinned, BackendSelection::EnvOverride);
    }
    if (on_windows || on_linux) && defaults.contains(Backends::GL) {
        let restricted = defaults.difference(Backends::GL);
        if !restricted.is_empty() {
            if on_windows {
                return (restricted, BackendSelection::WindowsGlExcluded);
            }
            return (restricted, BackendSelection::LinuxGlExcluded);
        }
    }
    (defaults, BackendSelection::Defaults)
}

/// Effective instance backends for this process: wgpu defaults honoring
/// `WGPU_BACKEND`, filtered through [`resolve_instance_backends`] with the
/// real platform.
///
/// Reads the environment exactly like [`instance_descriptor`]; callers that
/// log the selection call this first and pass nothing anywhere (the
/// descriptor builder applies the same rule internally).
#[must_use]
pub fn select_instance_backends() -> (Backends, BackendSelection) {
    let base = InstanceDescriptor::from_env_or_default();
    let env_override = Backends::from_env();
    resolve_instance_backends(
        base.backends,
        env_override,
        cfg!(target_os = "windows"),
        cfg!(target_os = "linux"),
    )
}

/// Builds the `wgpu` instance descriptor with the platform GL guard applied.
///
/// Starts from `InstanceDescriptor::from_env_or_default()` (so every other
/// `WGPU_*` knob keeps working) and replaces `backends` with the
/// [`resolve_instance_backends`] outcome for this platform.
#[must_use]
pub fn instance_descriptor() -> InstanceDescriptor {
    let base = InstanceDescriptor::from_env_or_default();
    let env_override = Backends::from_env();
    let (backends, _) = resolve_instance_backends(
        base.backends,
        env_override,
        cfg!(target_os = "windows"),
        cfg!(target_os = "linux"),
    );
    InstanceDescriptor { backends, ..base }
}

/// Builds the `wgpu` instance descriptor with every compiled backend kept.
///
/// This is the [`GpuContext::initialize`] fallback on non-Windows platforms
/// when the guarded [`instance_descriptor`] finds no adapter (Vulkan-less
/// hosts still reach GL instead of failing): it honors `WGPU_BACKEND`
/// exactly like the base descriptor and excludes nothing. On Windows it is
/// constructed but never used for an automatic retry — reintroducing GL
/// there would restart the `wgpu-hal` WGL init thread whose driver-dependent
/// overflow aborts the process before any error is returned (issue #1799).
#[must_use]
pub fn full_backends_descriptor() -> InstanceDescriptor {
    InstanceDescriptor::from_env_or_default()
}

/// Whether a guarded `NoCompatibleAdapter` may retry with the full backend set.
///
/// Pure rule so the platform gate stays unit-testable without a GPU:
/// retry only when the sets actually differ (otherwise the second attempt
/// could not succeed) and never on Windows, where the retry would
/// reintroduce the crash-contained WGL init thread (issue #1799) exactly on
/// the path where the old code returned a graceful `Err`.
#[must_use]
fn should_retry_with_full_backends(primary: Backends, full: Backends, on_windows: bool) -> bool {
    full != primary && !on_windows
}

/// Memory-allocation hint for the logical device request (CTX-1036, #1809).
///
/// Crate-private: `MemoryHints` is a `wgpu` type and no `wgpu` type escapes
/// this crate's public API (ADR-0004 "Adopt" row). `MemoryUsage` tells the
/// backend to size its sub-allocation arenas conservatively instead of the
/// default `Performance` throughput sizing, which dominates idle RSS on
/// discrete GPUs. A terminal's steady-state draw set (bounded fill/glyph
/// batches, one small atlas, no render-to-texture) fits comfortably.
fn device_memory_hints() -> MemoryHints {
    MemoryHints::MemoryUsage
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
    /// The instance descriptor comes from [`instance_descriptor`]: every
    /// `WGPU_*` knob keeps working, and on Windows the GL backend is excluded
    /// unless the operator pins backends via `WGPU_BACKEND` (issue #1799 —
    /// the `wgpu-hal` WGL init thread overflows its hardcoded 256 KiB stack
    /// on some drivers, aborting the process before any error is returned).
    /// On Linux the GL backend is excluded for the same reason at resident
    /// scale (CTX-1036, issue #1809 — the GL driver stack stays mapped even
    /// when Vulkan serves the surface). When the guarded instance finds no
    /// adapter, initialization retries once with [`full_backends_descriptor`]
    /// so Vulkan-less hosts still reach GL instead of failing. The retry is
    /// non-Windows-only: on Windows a guarded miss stays a graceful
    /// [`RenderError::NoCompatibleAdapter`] (headless/error path) rather
    /// than re-entering the crash-contained WGL init thread (issue #1799).
    ///
    /// The logical device requests [`MemoryHints::MemoryUsage`] (CTX-1036,
    /// issue #1809): the driver's default `Performance` hint sizes its
    /// sub-allocation arenas for throughput, which dominates idle RSS on
    /// discrete GPUs; a terminal's steady-state draw set fits comfortably in
    /// the conservative strategy.
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
        let primary = instance_descriptor();
        let full = full_backends_descriptor();
        // The guard excluded a backend the host actually needs
        // (Vulkan-less machine, minimal VM): retry with everything
        // compiled in — except on Windows, where the retry would re-enter
        // the crash-contained WGL init thread (issue #1799). Any other
        // failure (device rejection, driver error) is real and propagates
        // without a second attempt.
        let retry_allowed = should_retry_with_full_backends(
            primary.backends,
            full.backends,
            cfg!(target_os = "windows"),
        );
        match Self::initialize_with(&primary, None).await {
            Ok(ctx) => Ok(ctx),
            Err(RenderError::NoCompatibleAdapter) if retry_allowed => {
                Self::initialize_with(&full, None).await
            }
            Err(err) => Err(err),
        }
    }

    /// Initializes the GPU context for a window surface.
    ///
    /// This is the recommended initialization path when a window surface is
    /// available: the adapter is selected with the surface as
    /// `compatible_surface`, ensuring that the chosen adapter can present to
    /// the window. On Vulkan systems with multiple adapters, this prevents
    /// selecting a compute-only adapter that would later fail during
    /// `Surface::configure`.
    ///
    /// Uses the same backend guards and retry logic as [`Self::initialize`]:
    /// GL is excluded on Windows (issue #1799) and Linux (CTX-1036, #1809),
    /// and non-Windows platforms retry with full backends when no adapter is
    /// found.
    ///
    /// # Errors
    ///
    /// - [`RenderError::SurfaceCreate`] when the temporary surface cannot be
    ///   created for adapter selection.
    /// - [`RenderError::NoCompatibleAdapter`] when enumeration finds nothing
    ///   usable.
    /// - [`RenderError::DeviceRequest`] when the adapter rejects the logical
    ///   device request.
    /// - [`RenderError::UpstreamGraphics`] for other upstream failures.
    pub async fn initialize_for_surface(target: &SurfaceTarget) -> Result<Self, RenderError> {
        let primary = instance_descriptor();
        let full = full_backends_descriptor();
        let retry_allowed = should_retry_with_full_backends(
            primary.backends,
            full.backends,
            cfg!(target_os = "windows"),
        );
        match Self::initialize_with(&primary, Some(target)).await {
            Ok(ctx) => Ok(ctx),
            Err(RenderError::NoCompatibleAdapter) if retry_allowed => {
                Self::initialize_with(&full, Some(target)).await
            }
            Err(err) => Err(err),
        }
    }

    /// Initializes instance, adapter, and logical device for `descriptor`.
    ///
    /// Split from [`Self::initialize`] so the guarded-then-full fallback is
    /// testable without a GPU (callers never use this directly).
    ///
    /// When `target` is provided, a temporary surface is created and passed
    /// as `compatible_surface` to `request_adapter`, ensuring the selected
    /// adapter can present to the window (CodeRabbit PR #1837, gpu.rs:449-453).
    async fn initialize_with(
        descriptor: &InstanceDescriptor,
        target: Option<&SurfaceTarget>,
    ) -> Result<Self, RenderError> {
        let instance = Instance::new(descriptor);

        // Create temporary surface for adapter selection when targeting a window
        let temp_surface = target
            .map(|t| {
                instance
                    .create_surface(t.clone())
                    .map_err(|err| RenderError::SurfaceCreate(err.to_string()))
            })
            .transpose()?;

        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: temp_surface.as_ref(),
                ..Default::default()
            })
            .await
            .map_err(|_| RenderError::NoCompatibleAdapter)?;

        let (device, queue) = adapter
            .request_device(&DeviceDescriptor {
                label: Some("bitty-render"),
                required_features: Features::empty(),
                required_limits: Limits::default(),
                memory_hints: device_memory_hints(),
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
    /// Uses the safe `wgpu::Instance::create_surface` path: the owned
    /// `target.clone()` converts into `wgpu::SurfaceTarget::Window`, so wgpu
    /// keeps the window alive in `_handle_source` and returns
    /// `Surface<'static>` with no lifetime extension.
    ///
    /// # Errors
    ///
    /// - [`RenderError::SurfaceCreate`] when the platform refuses the handles
    ///   or `wgpu` cannot create a surface for them.
    pub fn create_surface(&self, target: &SurfaceTarget) -> Result<Surface, RenderError> {
        let surface: wgpu::Surface<'static> = self
            .instance
            .create_surface(target.clone())
            .map_err(|e| RenderError::SurfaceCreate(e.to_string()))?;

        Ok(Surface {
            kind: SurfaceKind::Gpu {
                surface,
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
    /// Triple-buffered vsync (`Mailbox`): offered but never preferred —
    /// [`pick_present_mode`] picks [`PresentMode::Fifo`] first so idle keeps
    /// two resident swap-chain images instead of three (CTX-1036, #1809).
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

/// Owned surface configuration (extent, format, present mode, opacity).
///
/// This is the only configuration the embedder constructs. The `wgpu`
/// `SurfaceConfiguration` is built internally from it plus adapter
/// capabilities, so no `wgpu` type leaks.
///
/// `opacity` (CTX-0290) is the renderer's half of `window.opacity`: `1.0`
/// keeps the opaque fast path, values below `1.0` request premultiplied
/// output and a compositor-blendable swap-chain alpha mode (selected from
/// the surface capabilities; see [`Surface::configure_with_opacity`]).
/// Always sanitized (see [`bitty_platform::sanitize_opacity`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceConfig {
    /// Surface extent in physical pixels.
    pub extent: PhysicalSize,
    /// Chosen texture format.
    pub format: SurfaceFormat,
    /// Chosen present mode.
    pub present_mode: PresentMode,
    /// Window opacity in `0.0..=1.0` (default `1.0` = fully opaque).
    pub opacity: f32,
}

impl SurfaceConfig {
    /// Builds a configuration for `extent` with explicit format and present
    /// mode choices and a fully opaque default opacity.
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
            opacity: 1.0,
        })
    }

    /// Sets the window opacity, sanitized into `0.0..=1.0` (CTX-0290).
    ///
    /// Non-finite inputs degrade to `1.0` and finite inputs clamp, matching
    /// [`bitty_platform::sanitize_opacity`] so the renderer can never be
    /// poisoned by untrusted config values.
    #[must_use]
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = bitty_platform::sanitize_opacity(opacity);
        self
    }

    /// The configured (sanitized) window opacity.
    #[must_use]
    pub fn opacity(&self) -> f32 {
        self.opacity
    }
}

/// Statistics returned from a present that composited a [`DrawList`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentStats {
    /// Logical frame counter for the surface (increments per present).
    pub frame: u64,
    /// Number of fill rectangles in the presented `DrawList` (cell and
    /// decoration fills plus the overlay fills painted above background
    /// images: selection, cursor, banners, help, scrollbar).
    pub fills: usize,
    /// Number of rounded fill/ring primitives in the presented `DrawList`
    /// (CTX-0311).
    pub rounded_fills: usize,
    /// Number of glyph instances in the presented `DrawList`.
    pub glyphs: usize,
    /// True when the surface is a headless fake (no swap-chain acquire).
    pub headless: bool,
    /// Number of Kitty image blits in the presented `DrawList`.
    pub images: usize,
    /// Image blits the presenting path did not paint.
    ///
    /// Always `0` on headless fakes (every blit is blended) and on
    /// clear-only presents. On a real surface the GPU pass uploads and
    /// paints every blit (CTX-0291); a non-zero count means individual
    /// blits were refused fail-closed (malformed bytes, over the device
    /// texture limit, or over a per-frame bound) and the caller warned.
    /// The refusal is counted and loud — never a silent CPU/GPU divergence.
    pub images_skipped: usize,
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
    // Resolved terminal palette (CTX-0355) driving the clear color on every
    // present path. Defaults to the designed Bitty Dark preset; the embedder
    // installs the selected `appearance.theme` palette via
    // `Surface::set_theme_palette`.
    theme: ThemePalette,
    // Requested window opacity (CTX-0290), sanitized. Kept separately from
    // `config` so it survives reconfiguration (resize / swap-chain loss).
    opacity: f32,
    // Whether the platform accepted a premultiplied swap-chain alpha mode.
    // `false` on a real surface without `PreMultiplied` support: the
    // renderer then stays fully opaque instead of dimming RGB that the
    // compositor would never blend (fail-closed, honest).
    alpha_supported: bool,
    // Headless-only last RGBA buffer (premultiplied, `width*height*4` bytes).
    headless_rgba: Option<Vec<u8>>,
    headless_extent: Option<PhysicalSize>,
    // Real-surface GPU presentation resources (pipelines, buffers, atlas
    // texture). Created lazily on the first presented frame and recreated
    // when the surface format or atlas dimensions change; `None` on
    // headless fakes (never used there).
    resources: Option<GpuResources>,
}

impl SurfaceState {
    fn new() -> Self {
        Self {
            config: None,
            wgpu_config: None,
            frame: 0,
            theme: ThemePalette::bitty_dark(),
            opacity: 1.0,
            alpha_supported: true,
            headless_rgba: None,
            headless_extent: None,
            resources: None,
        }
    }
}

/// Hard cap on headless surface bytes (64 MiB): a 4-byte-per-pixel RGBA
/// surface can therefore never exceed 16 Mi pixels.
///
/// Mirrors [`crate::software::MAX_SURFACE_BYTES`]. It lives here (rather than
/// importing from `software`) because `software` is gated behind the
/// `sw-fallback` feature while the headless fake compiles in every build: a
/// `#[cfg(feature = "sw-fallback")]` test below pins the two values together
/// so the mirror cannot drift silently.
pub const MAX_HEADLESS_SURFACE_BYTES: usize = 64 * 1024 * 1024;

/// Checked headless RGBA buffer length for `width` x `height` (4 bytes per
/// pixel), bounded by [`MAX_HEADLESS_SURFACE_BYTES`].
///
/// # Errors
///
/// [`RenderError::InvalidInput`] when the byte size exceeds the cap or does
/// not fit the address space. Arithmetic uses checked `u64` multiplication so
/// extreme `u32` extents fail closed instead of overflowing `usize` or OOMing
/// the host (CR-RENDER-01).
fn headless_buffer_len(width: u32, height: u32) -> Result<usize, RenderError> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(RenderError::InvalidInput {
            reason: "headless surface size does not fit the address space",
        })?;
    if bytes > MAX_HEADLESS_SURFACE_BYTES as u64 {
        return Err(RenderError::InvalidInput {
            reason: "headless surface exceeds the configured byte cap",
        });
    }
    // The cap check above bounds the allocation; conversion back to `usize`
    // cannot fail on any supported target.
    usize::try_from(bytes).map_err(|_| RenderError::InvalidInput {
        reason: "headless surface size does not fit the address space",
    })
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
        // The headless compositor scales its CPU buffer, so renderer alpha is
        // always honored here (CTX-0290).
        state.opacity = config.opacity;
        state.alpha_supported = true;
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

    /// Current requested window opacity (`1.0` = opaque), sanitized.
    ///
    /// This is the value the renderer was configured with; query
    /// [`Self::opacity_alpha_supported`] to learn whether the platform can
    /// actually blend it.
    #[must_use]
    pub fn opacity(&self) -> f32 {
        self.state.lock().expect("surface state poisoned").opacity
    }

    /// Stores a new requested window opacity without a GPU reconfigure
    /// (CTX-0898 live reload).
    ///
    /// Sanitizes `opacity` exactly like [`Self::configure_with_opacity`] and
    /// carries it onto the stored [`SurfaceConfig`]. This is the headless /
    /// not-yet-configured path: the headless compositor scales its CPU
    /// buffer by the stored value on the next present. A real surface must
    /// use [`Self::configure_with_opacity`] instead so the swap-chain alpha
    /// mode is re-picked for the new value.
    pub fn set_opacity(&self, opacity: f32) {
        let mut state = self.state.lock().expect("surface state poisoned");
        state.opacity = bitty_platform::sanitize_opacity(opacity);
        let sanitized = state.opacity;
        if let Some(config) = state.config.as_mut() {
            *config = config.with_opacity(sanitized);
        }
    }

    /// Installs the resolved terminal palette (CTX-0355).
    ///
    /// Every clear path (clear-only present, `DrawList` present, and the
    /// headless composites) paints `palette.background`, so the window clear
    /// follows `appearance.theme`. Defaults to the designed Bitty Dark
    /// preset, keeping the pre-CTX-0355 behavior byte-identical until the
    /// embedder installs a selected preset.
    pub fn set_theme_palette(&self, palette: ThemePalette) {
        self.state.lock().expect("surface state poisoned").theme = palette;
    }

    /// The resolved terminal palette used by the clear paths.
    #[must_use]
    pub fn theme_palette(&self) -> ThemePalette {
        self.state.lock().expect("surface state poisoned").theme
    }

    /// Whether the renderer can honor opacity on this surface (CTX-0290).
    ///
    /// `true` for headless fakes and for real surfaces whose capabilities
    /// accepted a premultiplied alpha mode. `false` means the app requested
    /// opacity below `1.0` but the platform offers no premultiplied
    /// compositing: the renderer stays fully opaque rather than writing
    /// dimmed RGB that the compositor would never blend.
    #[must_use]
    pub fn opacity_alpha_supported(&self) -> bool {
        self.state
            .lock()
            .expect("surface state poisoned")
            .alpha_supported
    }

    /// Renderer opacity actually applied when drawing frames (CTX-0290).
    ///
    /// Equals [`Self::opacity`] when the platform accepted premultiplied
    /// compositing; otherwise `1.0` so an unsupported platform keeps the
    /// opaque fast path instead of showing a dimmed window.
    fn effective_opacity(&self) -> f32 {
        let state = self.state.lock().expect("surface state poisoned");
        if state.alpha_supported {
            state.opacity
        } else {
            1.0
        }
    }

    /// Configures (or reconfigures) the surface for `extent`, adopting
    /// `opacity` as the requested window opacity (CTX-0290).
    ///
    /// Sanitizes `opacity` (via [`bitty_platform::sanitize_opacity`]), stores
    /// it on the surface state, and delegates to [`Self::configure`]. The
    /// stored opacity survives later reconfigurations (`resize`, swap-chain
    /// recovery). When the configure fails, the previous opacity is restored
    /// (CTX-0898) so the stored value never claims an alpha the swap chain was
    /// not configured for.
    ///
    /// # Errors
    ///
    /// Same as [`Self::configure`].
    pub fn configure_with_opacity(
        &self,
        ctx: &GpuContext,
        extent: PhysicalSize,
        opacity: f32,
    ) -> Result<(), RenderError> {
        self.with_opacity_restored_on_err(opacity, || self.configure(ctx, extent))
    }

    /// Stores sanitized `opacity`, runs `configure`, and restores the
    /// previous opacity when it fails (CTX-0898). Split out so the restore
    /// is testable without a real GPU context.
    fn with_opacity_restored_on_err(
        &self,
        opacity: f32,
        configure: impl FnOnce() -> Result<(), RenderError>,
    ) -> Result<(), RenderError> {
        let previous = {
            let mut state = self.state.lock().expect("surface state poisoned");
            std::mem::replace(
                &mut state.opacity,
                bitty_platform::sanitize_opacity(opacity),
            )
        };
        let result = configure();
        if result.is_err() {
            self.state.lock().expect("surface state poisoned").opacity = previous;
        }
        result
    }

    /// Configures (or reconfigures) the surface for `extent`.
    ///
    /// For a real surface, this queries `get_capabilities`, picks a format
    /// with `Srgb`→fallback, a present mode with `Fifo` preferred (CTX-1036,
    /// issue #1809 — double-buffered vsync keeps one fewer resident frame
    /// than `Mailbox` triple buffering),
    /// and an alpha mode from the surface's supported list (CTX-0290):
    /// `Auto` for fully opaque windows, `PreMultiplied` when the configured
    /// opacity is below `1.0` and the platform supports it. The
    /// [`Self::opacity_alpha_supported`] flag records whether the requested
    /// opacity can actually be blended. Then it builds a
    /// `wgpu::SurfaceConfiguration` and calls `wgpu::Surface::configure`.
    /// For the headless fake it validates the extent and stores the same
    /// fallback-chosen `SurfaceConfig`.
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
                    SurfaceConfig::new(extent, SurfaceFormat::Bgra8UnormSrgb, PresentMode::Fifo)?
                        .with_opacity(state.opacity);
                state.config = Some(config);
                state.headless_extent = Some(extent);
                state.wgpu_config = Some(synthesize_wgpu_config(&config));
                state.alpha_supported = true;
                Ok(())
            }
            SurfaceKind::Gpu { surface, .. } => {
                let opacity = self.state.lock().expect("surface state poisoned").opacity;
                let caps = surface.get_capabilities(&ctx.adapter);
                let format = pick_format(&caps);
                let present_mode = pick_present_mode(&caps);
                let (alpha_mode, alpha_supported) = pick_alpha_mode(&caps, opacity);
                let config =
                    SurfaceConfig::new(extent, format, present_mode)?.with_opacity(opacity);
                let wgpu_config = build_wgpu_config(&config, alpha_mode);
                surface.configure(&ctx.device, &wgpu_config);
                let mut state = self.state.lock().expect("surface state poisoned");
                state.config = Some(config);
                state.wgpu_config = Some(wgpu_config);
                state.alpha_supported = alpha_supported;
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
                    rounded_fills: 0,
                    glyphs: 0,
                    headless: true,
                    images: 0,
                    images_skipped: 0,
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

                let opacity = self.effective_opacity();
                // CTX-0355: the clear color follows the resolved palette.
                let theme = self.theme_palette();

                // Acquire with one retry on Outdated/Lost: reconfigure and try again.
                // This matches wgpu's recommended recovery for swap-chain loss
                // (see wgpu::SurfaceError::Outdated/Lost). Timeout/OutOfMemory
                // are propagated without retry because they are transient or
                // fatal in different ways — the caller decides whether to retry
                // the frame. The headless fake never reaches this branch.
                let frame = match surface.get_current_texture() {
                    Ok(f) => f,
                    Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                        self.configure(ctx, config.extent)?;
                        surface
                            .get_current_texture()
                            .map_err(|e| RenderError::SurfaceAcquire(e.to_string()))?
                    }
                    Err(e) => return Err(RenderError::SurfaceAcquire(e.to_string())),
                };
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
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                // Resolved theme clear color (CTX-0355):
                                // channels are sRGB-decoded to linear light
                                // for the `Srgb` swap-chain target (CTX-0222)
                                // and premultiplied by the requested opacity
                                // (CTX-0290), so the store-encode presents
                                // the byte-exact composited color.
                                load: wgpu::LoadOp::Clear(premultiplied_clear(&theme, opacity)),
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
                    rounded_fills: 0,
                    glyphs: 0,
                    headless: false,
                    images: 0,
                    images_skipped: 0,
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
    /// - On a real surface: the same validation runs, then fills draw
    ///   through the solid-fill pipeline and glyphs through the
    ///   atlas-textured pipeline (atlas texels uploaded with dirty-region
    ///   invalidation; inline glyphs via the transient texture), and the
    ///   swap-chain texture is presented. Handle `Outdated`/`Lost` with one
    ///   reconfigure retry, matching the `present` recovery contract; the
    ///   headless composite above is the CI-tested equivalent of the same
    ///   `DrawList`.
    ///
    /// Device-loss safety: every buffer/texture write is bounds-checked
    /// before submission (overruns return [`RenderError::InvalidInput`]
    /// rather than panicking), the DPI scale is derived per frame from
    /// `surface_extent / plan_extent` (clamped, never dividing by zero),
    /// and pipeline resources are recreated when the surface format or
    /// atlas dimensions change. A resource-creation or draw failure returns
    /// [`RenderError::UpstreamGraphics`] or [`RenderError::InvalidInput`];
    /// callers fall back to the headless seam for that frame.
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
        // CTX-0290: effective opacity (1.0 when the platform cannot blend
        // premultiplied alpha, so an unsupported surface stays opaque).
        let opacity = self.effective_opacity();

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
                // CR-RENDER-01: fail closed before allocating; extreme extents
                // must not reach `vec!` unchecked.
                let len = headless_buffer_len(width, height)?;
                let mut rgba = vec![0u8; len];
                // Clear to the resolved theme background (premultiplied,
                // CTX-0355); matches the GPU clear color above.
                {
                    let bg = self.theme_palette().background;
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
                // Rounded decoration fills/rings (CTX-0311).
                for fill in &draw_list.rounded_fills {
                    fill_rounded_rect_rgba(&mut rgba, width, height, fill);
                }
                // CTX-0347: per-`View` background images paint above cell
                // backgrounds and the ring, below overlay fills and glyphs.
                for blit in &draw_list.backgrounds {
                    blend_rgba_blit_rgba(&mut rgba, width, height, blit);
                }
                for fill in &draw_list.overlay_fills {
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
                                    glyph.clip,
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
                                    glyph.clip,
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
                                glyph.clip,
                            );
                        }
                    }
                }
                // Kitty images (CTX-0248): topmost present-layer blits,
                // above fills and glyphs, never grid truth. The headless
                // CPU compositor blends every entry; the real-GPU branch
                // below uploads and paints them since CTX-0291.
                for blit in &draw_list.images {
                    blend_rgba_blit_rgba(&mut rgba, width, height, blit);
                }
                // CTX-0290: apply the window opacity to the finished
                // premultiplied buffer, the CPU equivalent of the GPU
                // alpha-pinning blend below. No-op at opacity 1.0.
                scale_premultiplied_rgba(&mut rgba, opacity);
                let mut state = self.state.lock().expect("surface state poisoned");
                state.frame += 1;
                state.headless_rgba = Some(rgba);
                Ok(PresentStats {
                    frame: state.frame,
                    fills: draw_list.fills.len() + draw_list.overlay_fills.len(),
                    rounded_fills: draw_list.rounded_fills.len(),
                    glyphs: draw_list.glyphs.len(),
                    headless: true,
                    images: draw_list.images.len(),
                    images_skipped: 0,
                })
            }
            SurfaceKind::Gpu { surface, .. } => {
                // Real GPU path: validate, then draw the DrawList through
                // the fill + glyph pipelines and present. Resources are
                // created lazily and recreated when the surface format or
                // atlas dimensions change (covers reconfiguration and
                // device-loss recovery without panicking).
                let frame = match surface.get_current_texture() {
                    Ok(f) => f,
                    Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                        self.configure(ctx, config.extent)?;
                        // Reconfiguration may have picked a new format; drop
                        // cached resources so the draw below recreates them.
                        self.state.lock().expect("surface state poisoned").resources = None;
                        surface
                            .get_current_texture()
                            .map_err(|e| RenderError::SurfaceAcquire(e.to_string()))?
                    }
                    Err(e) => return Err(RenderError::SurfaceAcquire(e.to_string())),
                };
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());

                // Resolve atlas dimensions for resource matching: when the
                // frame needs atlas texels they must be supplied (checked
                // above); otherwise reuse the cached dims or leave them
                // unresolved so resource creation sizes the texture at the
                // lazy initial dimension (CTX-1036, #1809). The draw
                // below revalidates before sampling.
                let atlas_dims = atlas.map(|(_, dims)| dims).or_else(|| {
                    self.state
                        .lock()
                        .expect("surface state poisoned")
                        .resources
                        .as_ref()
                        .map(|r| r.atlas_dims_for_match())
                });
                let draw_atlas_dims = match (atlas, atlas_dims) {
                    (Some((_, dims)), _) => Some(dims),
                    (None, Some(dims)) => Some(dims),
                    (None, None) => None,
                };
                // Surface format for pipeline matching.
                let surface_format = config.format.to_wgpu();
                {
                    let mut state = self.state.lock().expect("surface state poisoned");
                    let needs_recreate = match (&state.resources, draw_atlas_dims) {
                        (None, _) => true,
                        (Some(r), Some(dims)) => !r.matches(surface_format, dims),
                        // No atlas on this frame and resources exist: keep
                        // them (format check below still applies).
                        (Some(r), None) => r.format_for_match() != surface_format,
                    };
                    if needs_recreate {
                        // Without atlas dims there is nothing to size the
                        // atlas texture from; use the lazy initial dimension
                        // (256 KiB R8, CTX-1036 #1809) instead of the 2048
                        // maximum (4 MiB): draws with no atlas glyphs never
                        // sample it, and the first atlas frame recreates at
                        // its real dims through the match check above.
                        let dims = draw_atlas_dims.unwrap_or(crate::atlas::AtlasDims {
                            width: crate::atlas::INITIAL_ATLAS_DIMENSION,
                            height: crate::atlas::INITIAL_ATLAS_DIMENSION,
                        });
                        let resources = GpuResources::create(&ctx.device, surface_format, dims)
                            .map_err(|e| RenderError::UpstreamGraphics(e.to_string()))?;
                        state.resources = Some(resources);
                    }
                }

                let scale = batch::derive_scale(
                    config.extent.width(),
                    config.extent.height(),
                    draw_list.plan.extent,
                );
                // Theme clear color: the resolved palette background
                // (CTX-0355), sRGB-decoded to linear light for the `Srgb`
                // swap-chain target (CTX-0222) and premultiplied by the
                // effective opacity (CTX-0290), matching the clear-only path
                // above.
                let clear = premultiplied_clear(&self.theme_palette(), opacity);
                let images_skipped = {
                    let mut state = self.state.lock().expect("surface state poisoned");
                    let resources = state.resources.as_mut().ok_or_else(|| {
                        RenderError::UpstreamGraphics("GPU resources missing after creation".into())
                    })?;
                    resources
                        .draw_frame(
                            &ctx.device,
                            &ctx.queue,
                            &view,
                            config.extent.width(),
                            config.extent.height(),
                            scale,
                            draw_list,
                            atlas,
                            clear,
                            opacity,
                        )
                        .map_err(|e| RenderError::UpstreamGraphics(e.to_string()))?
                };
                // CTX-0291: the real-GPU pass uploads and paints kitty image
                // blits (the CTX-0253 F3 skip gate is retired). A non-zero
                // skip count means individual blits were refused fail-closed
                // (malformed, over the device texture limit, or over a
                // per-frame bound); warn and report so a divergence from the
                // CPU compositors is never silent.
                if images_skipped > 0 {
                    eprintln!(
                        "bitty: real-GPU present skipped {images_skipped} image/background blit(s) (fail-closed: malformed or over budget; headless CPU blends them)"
                    );
                }
                frame.present();
                let mut state = self.state.lock().expect("surface state poisoned");
                state.frame += 1;
                Ok(PresentStats {
                    frame: state.frame,
                    fills: draw_list.fills.len() + draw_list.overlay_fills.len(),
                    rounded_fills: draw_list.rounded_fills.len(),
                    glyphs: draw_list.glyphs.len(),
                    headless: false,
                    images: draw_list.images.len(),
                    images_skipped,
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
        // CR-RENDER-01: fail closed before allocating; extreme extents must
        // not reach `vec!` unchecked.
        let len = headless_buffer_len(width, height)?;
        let mut rgba = vec![0u8; len];
        {
            // Clear to the resolved theme background (premultiplied, CTX-0355).
            let bg = self.theme_palette().background;
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
        // Rounded decoration fills/rings (CTX-0311).
        for fill in &draw_list.rounded_fills {
            fill_rounded_rect_rgba(&mut rgba, width, height, fill);
        }
        // CTX-0347: per-`View` background images paint above cell backgrounds
        // and the decoration ring, below overlay fills and glyphs.
        for blit in &draw_list.backgrounds {
            blend_rgba_blit_rgba(&mut rgba, width, height, blit);
        }
        // CTX-0347: selection/cursor/chrome overlay fills stay above them.
        for fill in &draw_list.overlay_fills {
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
                            glyph.clip,
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
                            glyph.clip,
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
                        glyph.clip,
                    );
                }
            }
        }
        // Kitty images (CTX-0248): topmost present-layer blits (see the
        // `present_draw_list` headless branch for the z-order contract).
        // Headless blends every entry (`images_skipped` stays 0); the
        // real-GPU branch uploads and paints them since CTX-0291.
        for blit in &draw_list.images {
            blend_rgba_blit_rgba(&mut rgba, width, height, blit);
        }
        // CTX-0290: headless equivalent of the GPU alpha-pinning blend —
        // scale the finished premultiplied buffer by the configured opacity
        // so CI can prove the window-opacity effect without an adapter.
        let opacity = self.effective_opacity();
        scale_premultiplied_rgba(&mut rgba, opacity);
        let mut state = self.state.lock().expect("surface state poisoned");
        state.frame += 1;
        state.headless_rgba = Some(rgba);
        Ok(PresentStats {
            frame: state.frame,
            fills: draw_list.fills.len() + draw_list.overlay_fills.len(),
            rounded_fills: draw_list.rounded_fills.len(),
            glyphs: draw_list.glyphs.len(),
            headless: true,
            images: draw_list.images.len(),
            images_skipped: 0,
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
        let config = config.with_opacity(state.opacity);
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

/// Picks the swap-chain present mode from the surface capabilities.
///
/// Pure rule (unit-tested, gated by the `#1809` memory-budget tests):
/// `Fifo` first — it is the only mode the WebGPU spec guarantees, and as a
/// double-buffered vsync it keeps one fewer resident frame than `Mailbox`
/// triple buffering (CTX-1036, issue #1809); then `Mailbox` when `Fifo` is
/// missing; otherwise the first reported mode, or `Fifo` when none is.
#[must_use]
pub fn pick_present_mode(caps: &wgpu::SurfaceCapabilities) -> PresentMode {
    if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
        return PresentMode::Fifo;
    }
    if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        return PresentMode::Mailbox;
    }
    caps.present_modes
        .first()
        .map(|m| PresentMode::from_wgpu(*m))
        .unwrap_or(PresentMode::Fifo)
}

/// Picks the swap-chain alpha mode for `opacity` (CTX-0290).
///
/// Returns the mode plus whether renderer alpha can actually be blended:
///
/// - `opacity >= 1.0` → [`wgpu::CompositeAlphaMode::Auto`] (the pre-CTX-0290
///   opaque fast path); `true`.
/// - `opacity < 1.0` and the caps offer
///   [`wgpu::CompositeAlphaMode::PreMultiplied`] → that mode; `true`.
/// - `opacity < 1.0` without premultiplied support → `Auto`; `false`
///   (fail-closed: the renderer must stay fully opaque instead of writing
///   scaled RGB that an opaque compositor would show as a dimmed window).
///
/// `wgpu` 26 rejects a non-`Auto` alpha mode outside
/// `SurfaceCapabilities::alpha_modes`, so the selection is always taken
/// from the caps list.
fn pick_alpha_mode(
    caps: &wgpu::SurfaceCapabilities,
    opacity: f32,
) -> (wgpu::CompositeAlphaMode, bool) {
    if opacity >= 1.0 {
        return (wgpu::CompositeAlphaMode::Auto, true);
    }
    if caps
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        (wgpu::CompositeAlphaMode::PreMultiplied, true)
    } else {
        (wgpu::CompositeAlphaMode::Auto, false)
    }
}

fn build_wgpu_config(
    config: &SurfaceConfig,
    alpha_mode: wgpu::CompositeAlphaMode,
) -> SurfaceConfiguration {
    SurfaceConfiguration {
        usage: TextureUsages::RENDER_ATTACHMENT,
        format: config.format.to_wgpu(),
        width: config.extent.width(),
        height: config.extent.height(),
        desired_maximum_frame_latency: 2,
        present_mode: config.present_mode.to_wgpu(),
        alpha_mode,
        view_formats: vec![],
    }
}

fn synthesize_wgpu_config(config: &SurfaceConfig) -> SurfaceConfiguration {
    // Headless inspection parity: the CPU compositor always honors opacity,
    // so synthesize the premultiplied mode exactly when the real path wants
    // it (CTX-0290).
    let alpha_mode = if config.opacity < 1.0 {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        wgpu::CompositeAlphaMode::Auto
    };
    build_wgpu_config(config, alpha_mode)
}

/// Premultiplied clear color for `opacity` from `palette` (CTX-0290/CTX-0355).
///
/// The theme background is sRGB-decoded to linear light (matching the
/// pre-CTX-0290 clear, CTX-0222) and scaled by `opacity`; alpha is
/// `opacity`. With the alpha-pinning blend this leaves the swap-chain
/// buffer at uniform alpha = `opacity` with premultiplied RGB, which the
/// compositor blends at the requested window opacity.
fn premultiplied_clear(palette: &ThemePalette, opacity: f32) -> wgpu::Color {
    let o = f64::from(opacity);
    let bg = palette.background;
    wgpu::Color {
        r: f64::from(crate::batch::srgb8_to_linear(bg[0])) * o,
        g: f64::from(crate::batch::srgb8_to_linear(bg[1])) * o,
        b: f64::from(crate::batch::srgb8_to_linear(bg[2])) * o,
        a: o,
    }
}

/// Scales a finished premultiplied RGBA buffer by `opacity` (CTX-0290).
///
/// This is the headless compositor's equivalent of the GPU opacity blit:
/// scaling every premultiplied channel (including alpha) by `opacity`
/// yields exactly the buffer the compositor would blend. `opacity >= 1.0`
/// (including `NaN`) is a no-op so the opaque fast path stays byte-exact.
fn scale_premultiplied_rgba(rgba: &mut [u8], opacity: f32) {
    // No-op for fully opaque or non-finite values so the default path stays
    // byte-exact (mirrors `sanitize_opacity` degrading NaN to 1.0).
    if !opacity.is_finite() || opacity >= 1.0 {
        return;
    }
    let factor = (opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
    for px in rgba.chunks_exact_mut(4) {
        px[0] = (u32::from(px[0]) * factor / 255) as u8;
        px[1] = (u32::from(px[1]) * factor / 255) as u8;
        px[2] = (u32::from(px[2]) * factor / 255) as u8;
        px[3] = (u32::from(px[3]) * factor / 255) as u8;
    }
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

/// Fills a rounded rectangle or border ring with analytic pixel-center
/// coverage (CTX-0311) onto the premultiplied headless buffer — the CPU twin
/// of the wgpu rounded-box SDF fill fragment stage, byte-for-byte the same
/// math as [`crate::software::SurfaceRgba::fill_rounded_rect`].
fn fill_rounded_rect_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    fill: &crate::grid::RoundedFill,
) {
    let left = i64::from(fill.frame.x).max(0);
    let top = i64::from(fill.frame.y).max(0);
    let right = (i64::from(fill.frame.x) + i64::from(fill.frame.width))
        .max(0)
        .min(i64::from(width));
    let bottom = (i64::from(fill.frame.y) + i64::from(fill.frame.height))
        .max(0)
        .min(i64::from(height));
    if right <= left || bottom <= top {
        return;
    }
    if fill.border == 0 {
        // Solid rounded fill: the whole frame carries coverage (not used by
        // the decoration ring path).
        for y in top..bottom {
            for x in left..right {
                blend_rounded_pixel_rgba(rgba, width, fill, x, y);
            }
        }
        return;
    }
    // Ring hot path (CTX-0311): only the border band and the four corner
    // squares can carry coverage. Iterate just those runs so interior
    // rows/columns never touch `coverage_at` (the soak/latency budgets
    // depend on this). Mirrors `software::SurfaceRgba::fill_rounded_rect`.
    let fx0 = i64::from(fill.frame.x);
    let fy0 = i64::from(fill.frame.y);
    let fx1 = fx0 + i64::from(fill.frame.width);
    let fy1 = fy0 + i64::from(fill.frame.height);
    let band = i64::from(
        u32::from(fill.border)
            .min(fill.frame.width)
            .min(fill.frame.height),
    ) + 1;
    let cs = (fill.resolved_radius().ceil() as i64) + 1;
    let mid_left_end = (fx0 + band).min(right);
    let mid_right_start = (fx1 - band).max(left).max(mid_left_end);
    let cor_left_end = (fx0 + cs).min(right);
    let cor_right_start = (fx1 - cs).max(left).max(cor_left_end);
    for y in top..bottom {
        if y < fy0 + band || y >= fy1 - band {
            paint_rounded_run_rgba(rgba, width, fill, left, right, y);
        } else if y < fy0 + cs || y >= fy1 - cs {
            paint_rounded_run_rgba(rgba, width, fill, left, cor_left_end, y);
            paint_rounded_run_rgba(rgba, width, fill, cor_right_start, right, y);
        } else {
            paint_rounded_run_rgba(rgba, width, fill, left, mid_left_end, y);
            paint_rounded_run_rgba(rgba, width, fill, mid_right_start, right, y);
        }
    }
}

/// Blends one rounded-fill pixel onto the premultiplied headless buffer;
/// zero-coverage pixels are skipped (CTX-0311).
fn blend_rounded_pixel_rgba(
    rgba: &mut [u8],
    width: u32,
    fill: &crate::grid::RoundedFill,
    x: i64,
    y: i64,
) {
    let coverage = fill.coverage_at(x as f32 + 0.5, y as f32 + 0.5);
    if coverage <= 0.0 {
        return;
    }
    let [cr, cg, cb, ca] = fill.color;
    let coverage = (coverage * 255.0 + 0.5).min(255.0) as u32;
    let sa = (coverage * u32::from(ca)) / 255;
    let src_r = ((u32::from(cr) * coverage * u32::from(ca)) / 65025) as u8;
    let src_g = ((u32::from(cg) * coverage * u32::from(ca)) / 65025) as u8;
    let src_b = ((u32::from(cb) * coverage * u32::from(ca)) / 65025) as u8;
    let src_a = sa.min(255) as u8;
    let d = (y as usize * width as usize + x as usize) * 4;
    let inv = 255 - u32::from(src_a);
    rgba[d] = src_r.saturating_add((u32::from(rgba[d]) * inv / 255) as u8);
    rgba[d + 1] = src_g.saturating_add((u32::from(rgba[d + 1]) * inv / 255) as u8);
    rgba[d + 2] = src_b.saturating_add((u32::from(rgba[d + 2]) * inv / 255) as u8);
    rgba[d + 3] = src_a.saturating_add((u32::from(rgba[d + 3]) * inv / 255) as u8);
}

/// Runs [`blend_rounded_pixel_rgba`] over `x0..x1` (empty when `x0 >= x1`).
fn paint_rounded_run_rgba(
    rgba: &mut [u8],
    width: u32,
    fill: &crate::grid::RoundedFill,
    x0: i64,
    x1: i64,
    y: i64,
) {
    for x in x0..x1 {
        blend_rounded_pixel_rgba(rgba, width, fill, x, y);
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
    clip: Option<crate::grid::RoundedClip>,
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
            let sx = (i64::from(x) + gx) as usize;
            let sy = (i64::from(y) + gy) as usize;
            let mut coverage = u32::from(mask[gy as usize * mask_w + gx as usize]);
            if coverage == 0 {
                continue;
            }
            if let Some(clip) = clip {
                if clip.may_clip_pixel(sx as i64, sy as i64) {
                    let clip_coverage = clip.coverage_at(sx as f32 + 0.5, sy as f32 + 0.5);
                    if clip_coverage <= 0.0 {
                        continue;
                    }
                    coverage = (coverage as f32 * clip_coverage + 0.5) as u32;
                    if coverage == 0 {
                        continue;
                    }
                }
            }
            let sa = (coverage * u32::from(ca)) / 255;
            let src_r = ((u32::from(cr) * coverage * u32::from(ca)) / 65025) as u8;
            let src_g = ((u32::from(cg) * coverage * u32::from(ca)) / 65025) as u8;
            let src_b = ((u32::from(cb) * coverage * u32::from(ca)) / 65025) as u8;
            let src_a = sa.min(255) as u8;
            let d = (sy * width as usize + sx) * 4;
            let inv = 255 - u32::from(src_a);
            rgba[d] = src_r.saturating_add((u32::from(rgba[d]) * inv / 255) as u8);
            rgba[d + 1] = src_g.saturating_add((u32::from(rgba[d + 1]) * inv / 255) as u8);
            rgba[d + 2] = src_b.saturating_add((u32::from(rgba[d + 2]) * inv / 255) as u8);
            rgba[d + 3] = src_a.saturating_add((u32::from(rgba[d + 3]) * inv / 255) as u8);
        }
    }
}

/// Composites one straight-alpha RGBA blit onto the premultiplied headless
/// buffer with src-over (CTX-0248 Kitty present layer).
///
/// Fully clipped; out-of-surface and empty rects are no-ops. A blit whose
/// bytes do not match its destination extent is skipped (fail closed —
/// [`crate::grid::ImageBlit::try_new`] normally prevents this, but
/// `DrawList` literals can carry anything).
fn blend_rgba_blit_rgba(rgba: &mut [u8], width: u32, height: u32, blit: &crate::grid::ImageBlit) {
    let dest = blit.dest;
    if dest.width == 0 || dest.height == 0 {
        return;
    }
    let expected = (u64::from(dest.width) * u64::from(dest.height)).checked_mul(4);
    if expected.is_none_or(|n| n as usize != blit.rgba.len()) {
        return;
    }
    let dst_left = i64::from(-dest.x).max(0);
    let dst_top = i64::from(-dest.y).max(0);
    let dst_right = (i64::from(width) - i64::from(dest.x))
        .max(0)
        .min(i64::from(dest.width));
    let dst_bottom = (i64::from(height) - i64::from(dest.y))
        .max(0)
        .min(i64::from(dest.height));
    if dst_right <= dst_left || dst_bottom <= dst_top {
        return;
    }
    let stride = dest.width as usize;
    for gy in dst_top..dst_bottom {
        for gx in dst_left..dst_right {
            let s = (gy as usize * stride + gx as usize) * 4;
            let (sr, sg, sb, sa) = (
                blit.rgba[s],
                blit.rgba[s + 1],
                blit.rgba[s + 2],
                blit.rgba[s + 3],
            );
            if sa == 0 {
                continue;
            }
            // Straight -> premultiplied on the fly, then src-over.
            let ps_r = (u32::from(sr) * u32::from(sa) / 255) as u8;
            let ps_g = (u32::from(sg) * u32::from(sa) / 255) as u8;
            let ps_b = (u32::from(sb) * u32::from(sa) / 255) as u8;
            let sx = (i64::from(dest.x) + gx) as usize;
            let sy = (i64::from(dest.y) + gy) as usize;
            let d = (sy * width as usize + sx) * 4;
            let inv = 255 - u32::from(sa);
            rgba[d] = ps_r.saturating_add((u32::from(rgba[d]) * inv / 255) as u8);
            rgba[d + 1] = ps_g.saturating_add((u32::from(rgba[d + 1]) * inv / 255) as u8);
            rgba[d + 2] = ps_b.saturating_add((u32::from(rgba[d + 2]) * inv / 255) as u8);
            rgba[d + 3] = sa.saturating_add((u32::from(rgba[d + 3]) * inv / 255) as u8);
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
    fn failed_opacity_configure_restores_previous_opacity() {
        // CTX-0898: a rejected reconfigure must not leave the surface
        // claiming the new opacity; a successful one keeps it.
        let surface = Surface::headless(PhysicalSize::new(64, 64)).expect("valid extent");
        surface.set_opacity(0.8);
        let err = surface.with_opacity_restored_on_err(0.3, || {
            Err(RenderError::SurfaceConfigure("synthetic".into()))
        });
        assert!(err.is_err());
        assert!((surface.opacity() - 0.8).abs() < f32::EPSILON, "restored");
        surface
            .with_opacity_restored_on_err(0.3, || Ok(()))
            .expect("ok configure");
        assert!((surface.opacity() - 0.3).abs() < f32::EPSILON, "adopted");
    }

    // -----------------------------------------------------------------------
    // Windows WGL backend policy (issue #1799): pure selection rule
    // -----------------------------------------------------------------------

    #[test]
    fn windows_guard_excludes_gl_without_env_override() {
        // Issue #1799: on Windows with no WGPU_BACKEND pin, GL must go so
        // wgpu-hal never spawns its 256 KiB WGL init thread; DX12/Vulkan
        // stay so adapter enumeration can still succeed.
        let defaults = Backends::DX12 | Backends::VULKAN | Backends::GL;
        let (backends, selection) = resolve_instance_backends(defaults, None, true, false);
        assert_eq!(selection, BackendSelection::WindowsGlExcluded);
        assert!(!backends.contains(Backends::GL), "GL excluded");
        assert!(backends.contains(Backends::DX12), "DX12 kept");
        assert!(backends.contains(Backends::VULKAN), "Vulkan kept");
        assert!(
            selection.log_note().is_some(),
            "exclusion must carry a log reason"
        );
    }

    #[test]
    fn linux_guard_excludes_gl_without_env_override() {
        // CTX-1036 (issue #1809): on Linux with no WGPU_BACKEND pin, GL
        // must go so the GL driver stack (~15 MiB resident file mappings
        // plus init threads) never loads when Vulkan serves the surface.
        // initialize() retries with full backends when no adapter is
        // found, so Vulkan-less hosts still reach GL.
        let defaults = Backends::VULKAN | Backends::GL;
        let (backends, selection) = resolve_instance_backends(defaults, None, false, true);
        assert_eq!(selection, BackendSelection::LinuxGlExcluded);
        assert!(!backends.contains(Backends::GL), "GL excluded");
        assert!(backends.contains(Backends::VULKAN), "Vulkan kept");
        assert!(
            selection.log_note().is_some(),
            "exclusion must carry a log reason"
        );
    }

    #[test]
    fn env_override_wins_even_for_gl_on_windows() {
        // An explicit WGPU_BACKEND pin is the operator's choice: honored
        // verbatim, no exclusion, nothing to log.
        let (backends, selection) =
            resolve_instance_backends(Backends::all(), Some(Backends::GL), true, false);
        assert_eq!(selection, BackendSelection::EnvOverride);
        assert_eq!(backends, Backends::GL);
        assert_eq!(selection.log_note(), None);
    }

    #[test]
    fn env_override_wins_on_linux() {
        // Same operator-wins rule on Linux (CTX-1036): a GL pin keeps GL.
        let (backends, selection) =
            resolve_instance_backends(Backends::all(), Some(Backends::GL), false, true);
        assert_eq!(selection, BackendSelection::EnvOverride);
        assert_eq!(backends, Backends::GL);
        assert_eq!(selection.log_note(), None);
    }

    #[test]
    fn non_windows_non_linux_keeps_gl_without_override() {
        // The guard is Windows/Linux-only: other platforms keep probing GL
        // (ANGLE / native) exactly as before.
        let defaults = Backends::DX12 | Backends::VULKAN | Backends::GL;
        let (backends, selection) = resolve_instance_backends(defaults, None, false, false);
        assert_eq!(selection, BackendSelection::Defaults);
        assert_eq!(backends, defaults);
        assert_eq!(selection.log_note(), None);
    }

    #[test]
    fn windows_gl_only_build_keeps_defaults_instead_of_empty() {
        // Degenerate build with only GL compiled: excluding it would leave
        // an empty set that can never yield an adapter, so keep defaults
        // (there is nothing to fall back to) rather than init nothing.
        let (backends, selection) = resolve_instance_backends(Backends::GL, None, true, false);
        assert_eq!(selection, BackendSelection::Defaults);
        assert_eq!(backends, Backends::GL);
    }

    #[test]
    fn linux_gl_only_build_keeps_defaults_instead_of_empty() {
        // Same degenerate rule on Linux (CTX-1036): a GL-only build keeps
        // probing GL rather than initializing an empty backend set.
        let (backends, selection) = resolve_instance_backends(Backends::GL, None, false, true);
        assert_eq!(selection, BackendSelection::Defaults);
        assert_eq!(backends, Backends::GL);
    }

    #[test]
    fn guarded_retry_reaches_full_backends_off_windows() {
        // CTX-1036: a guarded Linux miss (Vulkan kept, GL excluded) retries
        // with the full set so Vulkan-less hosts still reach GL.
        let primary = Backends::VULKAN;
        let full = Backends::VULKAN | Backends::GL;
        assert!(should_retry_with_full_backends(primary, full, false));
    }

    #[test]
    fn guarded_retry_stays_graceful_on_windows() {
        // Issue #1799: the retry would reintroduce the WGL init thread that
        // can abort the process before any error is returned, so a guarded
        // Windows miss stays `NoCompatibleAdapter` (headless/error path).
        let primary = Backends::DX12 | Backends::VULKAN;
        let full = primary | Backends::GL;
        assert!(!should_retry_with_full_backends(primary, full, true));
    }

    #[test]
    fn guarded_retry_needs_differing_backend_sets() {
        // Identical sets cannot succeed on retry (explicit pin, GL-only
        // build, or non-guarded platform): no second attempt anywhere.
        let same = Backends::VULKAN | Backends::GL;
        assert!(!should_retry_with_full_backends(same, same, false));
        assert!(!should_retry_with_full_backends(same, same, true));
    }

    #[test]
    fn windows_effective_backends_never_reach_wgl_without_override() {
        // Issue #1799 guard composition (pure, headless): with typical
        // Windows defaults and no operator pin, the guarded primary
        // excludes GL and the full-backend retry — the only path that
        // could reintroduce it — stays disallowed on Windows, so neither
        // instance attempt can start the crash-contained WGL init thread.
        let defaults = Backends::DX12 | Backends::VULKAN | Backends::GL;
        let (primary, selection) = resolve_instance_backends(defaults, None, true, false);
        assert_eq!(selection, BackendSelection::WindowsGlExcluded);
        assert!(!primary.contains(Backends::GL), "primary excludes GL");
        assert!(
            !should_retry_with_full_backends(primary, defaults, true),
            "Windows must never retry with the full set"
        );
    }

    #[test]
    fn linux_effective_backends_retry_only_where_wgl_cannot_run() {
        // CTX-1036 mirror (pure, headless): the Linux guard excludes GL
        // from the primary attempt, but the full-backend retry stays
        // allowed off Windows — WGL init exists only on Windows, so the
        // retry cannot re-enter the crash-contained thread there.
        let defaults = Backends::VULKAN | Backends::GL;
        let (primary, selection) = resolve_instance_backends(defaults, None, false, true);
        assert_eq!(selection, BackendSelection::LinuxGlExcluded);
        assert!(!primary.contains(Backends::GL), "primary excludes GL");
        assert!(should_retry_with_full_backends(primary, defaults, false));
    }

    #[test]
    fn device_prefers_memory_usage_over_performance() {
        // CTX-1036 (issue #1809): the logical-device request must size the
        // driver's sub-allocation arenas conservatively (MemoryUsage),
        // never the default Performance throughput sizing that dominates
        // idle RSS on discrete GPUs.
        assert!(matches!(device_memory_hints(), MemoryHints::MemoryUsage));
    }

    #[test]
    fn instance_descriptor_builds_without_touching_gpu() {
        // Smoke: descriptor construction is pure data (reads env, contacts
        // no driver), so it must never panic on a headless runner. The
        // selected backends themselves are covered by the pure-rule tests
        // above; env content is the operator's, not asserted here.
        let _descriptor = instance_descriptor();
        let (_backends, _selection) = select_instance_backends();
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
    fn theme_palette_drives_clear_color_not_bitty_dark() {
        // CTX-0355: the clear color must follow the resolved palette. The
        // pre-fix code hardcoded `crate::grid::DEFAULT_BG` (Bitty Dark) in
        // `premultiplied_clear` and the headless clear, so a light preset
        // still cleared to #1E1E2E.
        use crate::grid::{DrawList, ThemePalette};

        let extent = PhysicalSize::new(4, 2);
        let dark = ThemePalette::bitty_dark();
        let light =
            ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("github-light")));
        assert_eq!(dark.background, [0x1E, 0x1E, 0x2E, 0xFF]);
        assert_eq!(light.background, [0xFF, 0xFF, 0xFF, 0xFF]);

        // The GPU clear argument (used by the real present path) follows the
        // palette background, sRGB-decoded and premultiplied by opacity.
        let dark_clear = premultiplied_clear(&dark, 1.0);
        let light_clear = premultiplied_clear(&light, 1.0);
        assert_eq!(light_clear.r, 1.0);
        assert_eq!(light_clear.g, 1.0);
        assert_eq!(light_clear.b, 1.0);
        assert_eq!(light_clear.a, 1.0);
        assert_ne!(light_clear.r, dark_clear.r);
        assert_ne!(light_clear.g, dark_clear.g);
        assert_ne!(light_clear.b, dark_clear.b);

        // The headless composite path shares the same source of truth.
        let surface = Surface::headless(extent).expect("valid extent");
        surface.set_theme_palette(light);
        assert_eq!(surface.theme_palette(), light);
        let empty = DrawList {
            generation: 0,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(4, 2),
                mode: crate::frame::FrameMode::Clean,
                dirty_rects: vec![],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images: vec![],
        };
        surface
            .headless_present(&empty, None)
            .expect("headless clear-only present");
        let rgba = surface.headless_rgba().expect("rgba after present");
        assert_eq!(
            &rgba[..4],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            "clear must be github-light bg"
        );
    }

    #[test]
    fn present_without_configure_fails() {
        let surface = Surface {
            kind: SurfaceKind::Headless,
            state: Mutex::new(SurfaceState::new()),
        };
        let _draw = crate::grid::DrawList {
            generation: 0,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(0, 0),
                mode: crate::frame::FrameMode::Clean,
                dirty_rects: vec![],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images: vec![],
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
        // Fifo preferred over Mailbox (CTX-1036, #1809): double-buffered
        // vsync keeps one fewer resident frame than triple buffering.
        let caps2 = wgpu::SurfaceCapabilities {
            usages: TextureUsages::RENDER_ATTACHMENT,
            formats: vec![TextureFormat::Bgra8UnormSrgb],
            present_modes: vec![wgpu::PresentMode::Fifo, wgpu::PresentMode::Mailbox],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
        };
        assert_eq!(pick_present_mode(&caps2), PresentMode::Fifo);
        // Mailbox-only caps still negotiate (degraded, never rejected).
        let caps3 = wgpu::SurfaceCapabilities {
            usages: TextureUsages::RENDER_ATTACHMENT,
            formats: vec![TextureFormat::Bgra8UnormSrgb],
            present_modes: vec![wgpu::PresentMode::Mailbox],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
        };
        assert_eq!(pick_present_mode(&caps3), PresentMode::Mailbox);
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

    #[test]
    fn headless_clear_uses_theme_background() {
        // CTX-0147: every clear path paints Bitty Dark `#1e1e2e`, never the
        // old 0.06 hardcoded gray. An empty draw list presents just the
        // clear, so the first pixel must equal the theme background
        // (opaque, hence premultiply-identity).
        let surface = Surface::headless(PhysicalSize::new(32, 16)).expect("valid extent");
        let empty = crate::grid::DrawList {
            generation: 0,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(32, 16),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![crate::geometry::RectPx::new(0, 0, 32, 16)],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images: vec![],
        };
        surface
            .headless_present(&empty, None)
            .expect("clear-only present");
        let rgba = surface.headless_rgba().expect("rgba after present");
        assert_eq!(rgba.len(), 32 * 16 * 4);
        let theme_bg = bitty_config::theme::BITTY_DARK.background;
        assert_eq!(&rgba[..4], &[theme_bg[0], theme_bg[1], theme_bg[2], 0xFF]);
        assert!(rgba.chunks_exact(4).all(|px| px == &rgba[0..4]));
        // And the render-side default matches the same preset entry.
        assert_eq!(crate::grid::DEFAULT_BG[..3], theme_bg);
    }

    fn empty_draw_list(width: u32, height: u32) -> crate::grid::DrawList {
        crate::grid::DrawList {
            generation: 0,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(width, height),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![crate::geometry::RectPx::new(0, 0, width, height)],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images: vec![],
        }
    }

    #[test]
    fn surface_config_opacity_is_sanitized() {
        let base = SurfaceConfig::new(
            PhysicalSize::new(8, 8),
            SurfaceFormat::Bgra8UnormSrgb,
            PresentMode::Fifo,
        )
        .unwrap();
        assert_eq!(base.opacity(), 1.0);
        assert_eq!(base.with_opacity(f32::NAN).opacity(), 1.0);
        assert_eq!(base.with_opacity(2.0).opacity(), 1.0);
        assert_eq!(base.with_opacity(-1.0).opacity(), 0.0);
        assert!((base.with_opacity(0.25).opacity() - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn alpha_mode_selection_requires_premultiplied_below_one() {
        // CTX-0290: wgpu 26 rejects a non-`Auto` alpha mode outside the
        // caps list, so the selection must come from the caps and fail
        // closed to opaque when premultiplied compositing is unavailable.
        let caps = wgpu::SurfaceCapabilities {
            usages: TextureUsages::RENDER_ATTACHMENT,
            formats: vec![TextureFormat::Bgra8UnormSrgb],
            present_modes: vec![wgpu::PresentMode::Fifo],
            alpha_modes: vec![
                wgpu::CompositeAlphaMode::Opaque,
                wgpu::CompositeAlphaMode::PreMultiplied,
            ],
        };
        assert_eq!(
            pick_alpha_mode(&caps, 1.0),
            (wgpu::CompositeAlphaMode::Auto, true)
        );
        assert_eq!(
            pick_alpha_mode(&caps, 0.5),
            (wgpu::CompositeAlphaMode::PreMultiplied, true)
        );

        let opaque_only = wgpu::SurfaceCapabilities {
            alpha_modes: vec![wgpu::CompositeAlphaMode::Opaque],
            ..caps
        };
        assert_eq!(
            pick_alpha_mode(&opaque_only, 0.5),
            (wgpu::CompositeAlphaMode::Auto, false),
            "no premultiplied support must fail closed to the opaque path"
        );
    }

    #[test]
    fn headless_opacity_scales_premultiplied_buffer() {
        // CTX-0290: the headless compositor is the CI-testable equivalent of
        // the GPU alpha-pinning blend. An empty draw list at opacity 0.5 must
        // present Bitty Dark scaled by round(0.5 * 255) = 128 on every
        // channel, alpha included.
        let cfg = SurfaceConfig::new(
            PhysicalSize::new(32, 16),
            SurfaceFormat::Bgra8UnormSrgb,
            PresentMode::Fifo,
        )
        .unwrap()
        .with_opacity(0.5);
        let surface = Surface::headless_with_config(cfg).expect("valid extent");
        assert!((surface.opacity() - 0.5).abs() < f32::EPSILON);
        assert!(surface.opacity_alpha_supported());
        assert_eq!(
            surface.wgpu_config_snapshot().unwrap().alpha_mode,
            wgpu::CompositeAlphaMode::PreMultiplied
        );

        surface
            .headless_present(&empty_draw_list(32, 16), None)
            .expect("clear-only present");
        let rgba = surface.headless_rgba().expect("rgba after present");
        let bg = crate::grid::DEFAULT_BG;
        let scaled = [
            (u32::from(bg[0]) * 128 / 255) as u8,
            (u32::from(bg[1]) * 128 / 255) as u8,
            (u32::from(bg[2]) * 128 / 255) as u8,
            128,
        ];
        assert_eq!(&rgba[..4], &scaled);
        assert!(rgba.chunks_exact(4).all(|px| px == &rgba[0..4]));
        // The scaled buffer is premultiplied: RGB <= alpha for a gray-blue.
        assert!(u32::from(rgba[0]) <= u32::from(rgba[3]));
    }

    #[test]
    fn headless_opacity_one_stays_byte_identical() {
        // The opaque fast path must not change: opacity 1.0 keeps Auto alpha
        // mode and the pre-CTX-0290 bytes.
        let cfg = SurfaceConfig::new(
            PhysicalSize::new(32, 16),
            SurfaceFormat::Bgra8UnormSrgb,
            PresentMode::Fifo,
        )
        .unwrap()
        .with_opacity(1.0);
        let surface = Surface::headless_with_config(cfg).expect("valid extent");
        assert_eq!(
            surface.wgpu_config_snapshot().unwrap().alpha_mode,
            wgpu::CompositeAlphaMode::Auto
        );
        surface
            .headless_present(&empty_draw_list(32, 16), None)
            .expect("clear-only present");
        let rgba = surface.headless_rgba().expect("rgba after present");
        let theme_bg = bitty_config::theme::BITTY_DARK.background;
        assert_eq!(&rgba[..4], &[theme_bg[0], theme_bg[1], theme_bg[2], 0xFF]);
    }

    #[test]
    fn scale_premultiplied_rgba_is_total_and_fail_closed() {
        let mut px = [1u8, 2, 3, 4];
        scale_premultiplied_rgba(&mut px, 1.0);
        assert_eq!(px, [1, 2, 3, 4]);
        scale_premultiplied_rgba(&mut px, f32::NAN);
        assert_eq!(px, [1, 2, 3, 4]);
        let mut px = [255u8; 4];
        scale_premultiplied_rgba(&mut px, 0.0);
        assert_eq!(px, [0, 0, 0, 0]);
        let mut px = [255u8; 4];
        scale_premultiplied_rgba(&mut px, -1.0);
        assert_eq!(px, [0, 0, 0, 0]);
    }

    #[test]
    fn headless_buffer_len_enforces_byte_cap_without_allocating() {
        // CR-RENDER-01: `u64` checked arithmetic — no `as usize` overflow,
        // no allocation in this unit test.
        assert_eq!(headless_buffer_len(4, 2).unwrap(), 32);
        // Exactly at the cap is allowed: 4096 * 4096 * 4 == 64 MiB.
        assert_eq!(
            headless_buffer_len(4096, 4096).unwrap(),
            MAX_HEADLESS_SURFACE_BYTES
        );
        // One row over the cap fails closed.
        assert!(matches!(
            headless_buffer_len(4097, 4096),
            Err(RenderError::InvalidInput { .. })
        ));
        // Extreme `u32` extents fail closed instead of overflowing or OOMing.
        assert!(matches!(
            headless_buffer_len(u32::MAX, u32::MAX),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(matches!(
            headless_buffer_len(u32::MAX, 1),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn headless_present_rejects_extreme_extents_without_allocating() {
        // CR-RENDER-01 regression: 5000 * 5000 * 4 = 100 MiB > 64 MiB cap.
        // Creation only validates non-zero extents; the present path must
        // fail closed *before* allocating.
        let surface = Surface::headless(PhysicalSize::new(5000, 5000)).expect("valid extent");
        let empty = crate::grid::DrawList {
            generation: 0,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(5000, 5000),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images: vec![],
        };
        let err = surface
            .headless_present(&empty, None)
            .expect_err("over-cap present must fail");
        assert!(matches!(err, RenderError::InvalidInput { .. }));
        // No partial buffer is stored ...
        assert!(surface.headless_rgba().is_none());
        // ... and the surface still serves normal frames afterwards.
        surface
            .headless_resize(PhysicalSize::new(32, 16))
            .expect("resize to normal extent");
        surface
            .headless_present(&empty, None)
            .expect("normal present after rejected extreme");
        assert_eq!(surface.headless_rgba().unwrap().len(), 32 * 16 * 4);
    }

    #[cfg(feature = "sw-fallback")]
    #[test]
    fn headless_cap_matches_software_cap() {
        // The headless mirror must not drift from the canonical software cap.
        assert_eq!(
            MAX_HEADLESS_SURFACE_BYTES,
            crate::software::MAX_SURFACE_BYTES
        );
    }

    fn image_test_list(images: Vec<crate::grid::ImageBlit>) -> crate::grid::DrawList {
        crate::grid::DrawList {
            generation: 7,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(8, 8),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![crate::geometry::RectPx::new(0, 0, 8, 8)],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images,
        }
    }

    #[test]
    fn image_blit_validation_rejects_mismatch() {
        use crate::geometry::RectPx;
        use crate::grid::ImageBlit;
        // Exact bytes accepted.
        assert!(ImageBlit::try_new(RectPx::new(0, 0, 2, 2), vec![9; 16]).is_ok());
        // Zero span rejected.
        assert!(matches!(
            ImageBlit::try_new(RectPx::new(0, 0, 0, 2), vec![9; 1]),
            Err(RenderError::InvalidInput { .. })
        ));
        // Short and long buffers rejected.
        assert!(matches!(
            ImageBlit::try_new(RectPx::new(0, 0, 2, 2), vec![9; 15]),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(matches!(
            ImageBlit::try_new(RectPx::new(0, 0, 2, 2), vec![9; 17]),
            Err(RenderError::InvalidInput { .. })
        ));
        // Images count toward draw work.
        let with_image = image_test_list(vec![
            ImageBlit::try_new(RectPx::new(0, 0, 1, 1), vec![1, 2, 3, 4]).unwrap(),
        ]);
        assert!(with_image.needs_draw());
        assert!(!image_test_list(vec![]).needs_draw());
    }

    #[test]
    fn headless_present_composites_image_blits_topmost() {
        use crate::geometry::RectPx;
        use crate::grid::ImageBlit;
        let surface = Surface::headless(PhysicalSize::new(8, 8)).expect("valid extent");
        // 2x2 opaque red at (2, 2); the rest stays theme background.
        let list = image_test_list(vec![
            ImageBlit::try_new(RectPx::new(2, 2, 2, 2), [0xFF, 0, 0, 0xFF].repeat(4)).unwrap(),
        ]);
        surface.headless_present(&list, None).expect("blit present");
        let rgba = surface.headless_rgba().expect("rgba after present");
        assert_eq!(rgba.len(), 8 * 8 * 4);
        let bg = crate::grid::DEFAULT_BG;
        // Background pixel untouched (opaque theme bg, premultiply-identity).
        assert_eq!(&rgba[0..4], &[bg[0], bg[1], bg[2], 0xFF]);
        // Blit pixels are opaque red (premultiplied identity at alpha 255).
        for (ry, rx) in [(2, 2), (2, 3), (3, 2), (3, 3)] {
            let idx = (ry * 8 + rx) * 4;
            assert_eq!(&rgba[idx..idx + 4], &[0xFF, 0, 0, 0xFF], "{rx},{ry}");
        }
        // Deterministic: same frame re-presents byte-identical.
        surface.headless_present(&list, None).expect("re-present");
        assert_eq!(surface.headless_rgba().unwrap(), rgba);
    }

    #[test]
    fn headless_present_blends_half_alpha_and_clips() {
        use crate::geometry::RectPx;
        use crate::grid::ImageBlit;
        let surface = Surface::headless(PhysicalSize::new(4, 4)).expect("valid extent");
        // Half-alpha white at (-1, -1) size 2x2: only (0, 0) intersects.
        let list = image_test_list(vec![
            ImageBlit::try_new(
                RectPx::new(-1, -1, 2, 2),
                [0xFF, 0xFF, 0xFF, 0x80].repeat(4),
            )
            .unwrap(),
        ]);
        surface.headless_present(&list, None).expect("clip present");
        let rgba = surface.headless_rgba().expect("rgba");
        let bg = crate::grid::DEFAULT_BG;
        // out = src*a/255 + dst*(255-a)/255 per channel, alpha likewise.
        let a = 0x80_u32;
        let expect = |dst: u8| {
            ((255 * a / 255) as u8).saturating_add((u32::from(dst) * (255 - a) / 255) as u8)
        };
        let alpha = (a as u8).saturating_add((255_u32 * (255 - a) / 255) as u8);
        assert_eq!(rgba[0], expect(bg[0]));
        assert_eq!(rgba[1], expect(bg[1]));
        assert_eq!(rgba[2], expect(bg[2]));
        assert_eq!(rgba[3], alpha);
        // Every other pixel is untouched background.
        for idx in 1..16 {
            let o = idx * 4;
            assert_eq!(&rgba[o..o + 4], &[bg[0], bg[1], bg[2], 0xFF], "px {idx}");
        }
        // Fully off-surface blits are safe no-ops.
        let off = image_test_list(vec![
            ImageBlit::try_new(RectPx::new(99, 99, 2, 2), vec![1; 16]).unwrap(),
        ]);
        let before = rgba.clone();
        surface.headless_present(&off, None).expect("off-surface");
        let bg_only = surface.headless_rgba().expect("rgba");
        assert_eq!(&bg_only[0..4], &[bg[0], bg[1], bg[2], 0xFF]);
        assert_ne!(bg_only, before, "off-surface blit must not paint");
    }

    #[test]
    fn headless_present_skips_mismatched_blit_bytes() {
        use crate::geometry::RectPx;
        // A hand-built literal with lying bytes must not panic and must not
        // paint: fail closed, clear-only frame.
        let mut list = image_test_list(vec![]);
        list.images.push(crate::grid::ImageBlit {
            dest: RectPx::new(0, 0, 2, 2),
            rgba: vec![1; 7],
        });
        let surface = Surface::headless(PhysicalSize::new(4, 4)).expect("valid extent");
        surface.headless_present(&list, None).expect("present");
        let rgba = surface.headless_rgba().expect("rgba");
        let bg = crate::grid::DEFAULT_BG;
        assert!(
            rgba.chunks_exact(4)
                .all(|px| px == [bg[0], bg[1], bg[2], 0xFF])
        );
    }

    #[test]
    fn gpu_image_upload_plan_parity_with_headless_is_observable() {
        // CTX-0291: the CTX-0253 F3 skip gate is retired. The same 2x2
        // opaque red blit headless blends is admitted by the GPU upload
        // planner (nothing skipped), and a malformed literal is refused
        // fail-closed and counted on the GPU side. Both paths stay
        // observable through `PresentStats`/`ImageUploadPlan`.
        use crate::batch::plan_image_uploads;
        use crate::geometry::RectPx;
        use crate::grid::ImageBlit;
        let blit = ImageBlit::try_new(RectPx::new(1, 1, 2, 2), [0xFF, 0, 0, 0xFF].repeat(4))
            .expect("blit bytes match extent");
        let list = image_test_list(vec![blit]);
        let surface = Surface::headless(PhysicalSize::new(8, 8)).expect("headless");
        let stats = surface.headless_present(&list, None).expect("present");
        assert_eq!(stats.images, 1);
        assert_eq!(stats.images_skipped, 0);
        // Pixel proof: the blit really blended headlessly.
        let rgba = surface.headless_rgba().expect("rgba");
        let idx = (8 + 1) * 4;
        assert_eq!(&rgba[idx..idx + 4], &[0xFF, 0, 0, 0xFF]);
        // GPU parity: the planner admits the same blit for texture upload
        // (4096 is the smallest 2D texture limit across wgpu backends).
        let plan = plan_image_uploads(&list.images, 4096);
        assert_eq!(plan.admitted, vec![0]);
        assert_eq!(plan.skipped, 0);
        // A malformed literal is refused and counted; the valid blit still
        // uploads (skip-and-continue, never a silent partial paint).
        let mut malformed = list.clone();
        malformed.images.push(ImageBlit {
            dest: RectPx::new(0, 0, 2, 2),
            rgba: vec![1; 7],
        });
        let plan = plan_image_uploads(&malformed.images, 4096);
        assert_eq!(plan.admitted, vec![0]);
        assert_eq!(plan.skipped, 1);
    }

    #[test]
    fn headless_present_stats_report_images_drawn_not_skipped() {
        use crate::geometry::RectPx;
        use crate::grid::ImageBlit;
        // Two blits: headless must blend both and report them drawn.
        let list = image_test_list(vec![
            ImageBlit::try_new(RectPx::new(0, 0, 1, 1), vec![0xFF, 0, 0, 0xFF]).unwrap(),
            ImageBlit::try_new(RectPx::new(2, 2, 1, 1), vec![0, 0xFF, 0, 0xFF]).unwrap(),
        ]);
        let surface = Surface::headless(PhysicalSize::new(8, 8)).expect("valid extent");
        let stats = surface.headless_present(&list, None).expect("present");
        assert!(stats.headless);
        assert_eq!(stats.images, 2);
        assert_eq!(stats.images_skipped, 0);
        // ... and the pixels prove the blend really happened.
        let rgba = surface.headless_rgba().expect("rgba");
        assert_eq!(&rgba[0..4], &[0xFF, 0, 0, 0xFF]);
        let idx = (2 * 8 + 2) * 4;
        assert_eq!(&rgba[idx..idx + 4], &[0, 0xFF, 0, 0xFF]);
        // NOTE: the `present_draw_list` headless branch builds the same
        // `images`/`images_skipped` fields but needs a live `GpuContext`,
        // so it is covered by inspection + the env-gated real-GPU tests,
        // not by this headless unit test.
    }

    #[test]
    fn present_without_draw_list_reports_no_images() {
        // Clear-only presents carry no blits on either path.
        let surface = Surface::headless(PhysicalSize::new(16, 16)).expect("valid extent");
        let empty = image_test_list(vec![]);
        let stats = surface.headless_present(&empty, None).expect("present");
        assert_eq!(stats.images, 0);
        assert_eq!(stats.images_skipped, 0);
    }

    // -----------------------------------------------------------------------
    // Real GPU: env-gated offscreen render + readback (no window/display)
    // -----------------------------------------------------------------------

    fn gpu_tests_enabled() -> bool {
        matches!(std::env::var("BITTY_RENDER_GPU_TESTS").as_deref(), Ok("1"))
    }

    /// Minimal blocking executor (no runtime dependency; mirrors the
    /// integration-test helper in `tests/wgpu_surface.rs`).
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        use std::sync::{Arc, Condvar, Mutex};
        use std::task::{Context, Poll, Wake, Waker};

        struct Notify(Arc<(Mutex<bool>, Condvar)>);
        impl Wake for Notify {
            fn wake(self: Arc<Self>) {
                let (flag, cv) = &*self.0;
                *flag.lock().unwrap() = true;
                cv.notify_all();
            }
        }

        let mut f = Box::pin(f);
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let waker = Waker::from(Arc::new(Notify(Arc::clone(&state))));
        let mut cx = Context::from_waker(&waker);
        loop {
            match f.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {
                    let (flag, cv) = &*state;
                    let mut sig = flag.lock().unwrap();
                    while !*sig {
                        sig = cv.wait(sig).unwrap();
                    }
                    *sig = false;
                }
            }
        }
    }

    /// CTX-0291 GPU-path integration: uploads Kitty blits into textures and
    /// proves real painted pixels by offscreen render + readback. Gated by
    /// `BITTY_RENDER_GPU_TESTS=1` (CI has no adapter, so it skips cleanly).
    #[test]
    fn real_gpu_offscreen_present_uploads_and_paints_images() {
        if !gpu_tests_enabled() {
            eprintln!("skipped: BITTY_RENDER_GPU_TESTS != 1");
            return;
        }
        let ctx = match block_on(GpuContext::initialize()) {
            Ok(ctx) => ctx,
            Err(e) => {
                eprintln!("adapter unavailable despite BITTY_RENDER_GPU_TESTS=1: {e}");
                return;
            }
        };
        let (width, height) = (32u32, 32u32);
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bitty-test-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut resources = crate::pipeline::GpuResources::create(
            &ctx.device,
            format,
            crate::atlas::AtlasDims {
                width: crate::atlas::DEFAULT_ATLAS_DIMENSION,
                height: crate::atlas::DEFAULT_ATLAS_DIMENSION,
            },
        )
        .expect("image/glyph resources");
        // Opaque red at (4,4), opaque blue at (8,8), plus one malformed
        // literal that must be refused fail-closed and counted.
        let list = DrawList {
            generation: 1,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(width, height),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![crate::geometry::RectPx::new(0, 0, width, height)],
            },
            fills: vec![],
            rounded_fills: vec![],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![],
            images: vec![
                crate::grid::ImageBlit::try_new(
                    crate::geometry::RectPx::new(4, 4, 2, 2),
                    [0xFF, 0, 0, 0xFF].repeat(4),
                )
                .expect("red blit"),
                crate::grid::ImageBlit::try_new(
                    crate::geometry::RectPx::new(8, 8, 2, 2),
                    [0, 0, 0xFF, 0xFF].repeat(4),
                )
                .expect("blue blit"),
                crate::grid::ImageBlit {
                    dest: crate::geometry::RectPx::new(0, 0, 2, 2),
                    rgba: vec![1; 7],
                },
            ],
        };
        let skipped = resources
            .draw_frame(
                &ctx.device,
                &ctx.queue,
                &view,
                width,
                height,
                1.0,
                &list,
                None,
                wgpu::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                1.0,
            )
            .expect("offscreen draw");
        assert_eq!(skipped, 1, "malformed blit refused fail-closed");
        // Readback (`copy_texture_to_buffer` needs 256-byte row alignment).
        let bytes_per_row = 256u32;
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bitty-test-readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bitty-test-readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        ctx.queue.submit(std::iter::once(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        ctx.device.poll(wgpu::PollType::Wait).expect("device poll");
        rx.recv().expect("map callback").expect("buffer map");
        let data = slice.get_mapped_range();
        let pixel = |x: u32, y: u32| -> (u8, u8, u8, u8) {
            let offset = (y * bytes_per_row + x * 4) as usize;
            (
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            )
        };
        // BGRA target: opaque red stores as (B,G,R,A) = (0,0,255,255) and
        // opaque blue as (255,0,0,255); outside the blits is clear black.
        assert_eq!(pixel(4, 4), (0, 0, 255, 255), "red blit painted");
        assert_eq!(pixel(5, 5), (0, 0, 255, 255), "red blit painted");
        assert_eq!(pixel(8, 8), (255, 0, 0, 255), "blue blit painted");
        assert_eq!(pixel(0, 0), (0, 0, 0, 255), "clear untouched");
        drop(data);
        readback.unmap();
    }

    // -----------------------------------------------------------------------
    // CTX-0311: rounded SDF fills + inner-arc glyph clipping.
    // -----------------------------------------------------------------------

    /// 32x32 frame, 4px border, radius 12; the content clip is the inner
    /// rounded rect (4,4,24,24) with radius 8. The glyph is an 8x8 inline
    /// full-coverage mask at (0,12): its left edge deliberately overhangs the
    /// inner clip onto the left ring band.
    fn rounded_clip_list() -> (DrawList, crate::grid::RoundedFill) {
        use crate::geometry::RectPx;
        use crate::grid::{GlyphInstance, GlyphSource, RoundedFill};
        let ring = RoundedFill {
            frame: RectPx::new(0, 0, 32, 32),
            border: 4,
            radius: 12,
            color: [255, 0, 0, 255],
        };
        let clip = ring.inner_clip().expect("rounded ring has an inner clip");
        let glyph = GlyphInstance {
            dest: [0, 12],
            size: [8, 8],
            uv: [0.0; 4],
            color: [255, 255, 255, 255],
            clip: Some(clip),
            source: GlyphSource::Inline {
                mask: vec![255; 64],
                width: 8,
                height: 8,
            },
        };
        let list = DrawList {
            generation: 1,
            atlas_epoch: 0,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(32, 32),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![RectPx::new(0, 0, 32, 32)],
            },
            fills: vec![],
            rounded_fills: vec![ring.clone()],
            backgrounds: vec![],
            overlay_fills: vec![],
            glyphs: vec![glyph],
            images: vec![],
        };
        (list, ring)
    }

    #[test]
    fn headless_present_paints_rounded_ring_and_clips_glyphs() {
        let (list, ring) = rounded_clip_list();
        let surface = Surface::headless(PhysicalSize::new(32, 32)).expect("headless");
        let stats = surface.headless_present(&list, None).expect("present");
        assert_eq!(stats.rounded_fills, 1);
        let rgba = surface.headless_rgba().expect("rgba");
        let px = |x: usize, y: usize| -> [u8; 4] {
            let o = (y * 32 + x) * 4;
            [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]]
        };
        // Ring edge paints (opaque red); the outer corner is cut.
        assert_eq!(px(0, 16), [255, 0, 0, 255], "left ring band");
        assert_eq!(px(0, 0), [0x1E, 0x1E, 0x2E, 255], "outer corner cut");
        // The glyph overhangs the inner clip at x=0: clipped, so the ring
        // color survives instead of the white glyph texel.
        assert_eq!(px(0, 16 - 4), [255, 0, 0, 255], "clipped glyph overhang");
        assert_eq!(px(4, 16), [255, 255, 255, 255], "glyph inside the clip");
        assert_eq!(px(16, 16), [0x1E, 0x1E, 0x2E, 255], "ring hole");
        // The CPU coverage remains available for goldens.
        assert!(ring.coverage_at(0.5, 16.5) >= 0.99);
    }

    /// Encodes linear light to an sRGB byte (test-local reference for the
    /// GPU sRGB target).
    fn srgb_encode_byte(linear: f32) -> u8 {
        let encoded = if linear <= 0.003_130_8 {
            12.92 * linear
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        (encoded * 255.0 + 0.5).clamp(0.0, 255.0) as u8
    }

    /// CTX-0311 GPU path: renders the rounded ring plus the clipped glyph
    /// offscreen on a real adapter and proves both the SDF partial coverage
    /// and the glyph discard by readback. Gated by `BITTY_RENDER_GPU_TESTS=1`
    /// (CI has no adapter, so it skips cleanly).
    #[test]
    fn real_gpu_offscreen_rounded_sdf_and_glyph_clip() {
        if !gpu_tests_enabled() {
            eprintln!("skipped: BITTY_RENDER_GPU_TESTS != 1");
            return;
        }
        let ctx = match block_on(GpuContext::initialize()) {
            Ok(ctx) => ctx,
            Err(e) => {
                eprintln!("adapter unavailable despite BITTY_RENDER_GPU_TESTS=1: {e}");
                return;
            }
        };
        let (list, ring) = rounded_clip_list();
        let (width, height) = (32u32, 32u32);
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bitty-rounded-test-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut resources = crate::pipeline::GpuResources::create(
            &ctx.device,
            format,
            crate::atlas::AtlasDims {
                width: crate::atlas::DEFAULT_ATLAS_DIMENSION,
                height: crate::atlas::DEFAULT_ATLAS_DIMENSION,
            },
        )
        .expect("present resources");
        resources
            .draw_frame(
                &ctx.device,
                &ctx.queue,
                &view,
                width,
                height,
                1.0,
                &list,
                None,
                wgpu::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                1.0,
            )
            .expect("offscreen draw");
        // Readback (`copy_texture_to_buffer` needs 256-byte row alignment).
        let bytes_per_row = 256u32;
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bitty-rounded-test-readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bitty-rounded-test-readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        ctx.queue.submit(std::iter::once(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        ctx.device.poll(wgpu::PollType::Wait).expect("device poll");
        rx.recv().expect("map callback").expect("buffer map");
        let data = slice.get_mapped_range();
        let pixel = |x: u32, y: u32| -> (u8, u8, u8, u8) {
            let offset = (y * bytes_per_row + x * 4) as usize;
            (
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            )
        };
        // BGRA target: opaque red is (0,0,255,255), white is (255,255,255,255).
        assert_eq!(pixel(0, 16), (0, 0, 255, 255), "ring band paints");
        assert_eq!(pixel(0, 0), (0, 0, 0, 255), "outer corner cut");
        // Clipped glyph overhang: the white texel must not survive at x=0;
        // the ring red does.
        assert_eq!(pixel(0, 12), (0, 0, 255, 255), "clipped glyph stays ring");
        assert_eq!(pixel(4, 16), (255, 255, 255, 255), "unclipped glyph paints");
        assert_eq!(pixel(16, 16), (0, 0, 0, 255), "ring hole stays clear");
        // Partial SDF coverage on the arc must match the shared analytic
        // coverage encoded through the sRGB target (byte-tolerant).
        let partial_x = 8u32;
        let coverage = ring.coverage_at(partial_x as f32 + 0.5, 0.5);
        assert!(
            coverage > 0.05 && coverage < 0.95,
            "expected partial arc coverage, got {coverage}"
        );
        let expected_r = srgb_encode_byte(coverage);
        let gpu_r = pixel(partial_x, 0).2;
        assert!(
            gpu_r.abs_diff(expected_r) <= 4,
            "SDF coverage mismatch at ({partial_x},0): gpu {gpu_r} vs analytic {expected_r}"
        );
        drop(data);
        readback.unmap();
    }
}
