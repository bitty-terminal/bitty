//! GPU context creation with a fully owned error and info surface.
//!
//! This module is the only place where `wgpu` types are named (ADR-0004
//! "Adopt" row). The public API exposes:
//!
//! - [`GpuContext::initialize`], an async entry point (callers drive the
//!   future with their own executor; this crate deliberately ships no
//!   blocking runtime dependency), returning an owned context or a flattened
//!   [`RenderError`];
//! - [`AdapterSummary`], owned re-descriptions of adapter facts.
//!
//! # What this slice does *not* do
//!
//! Window-surface attachment is intentionally absent. Creating a surface
//! requires raw window/display handles, and the boundary for those (own
//! wrapper around `raw-window-handle`, or closure-passing) belongs to the
//! `bitty-platform` integration slice, where window ownership already lives.
//! Pipelines, shaders, vertex upload, and present are likewise later slices;
//! none of that may be implied as working by this skeleton.
//!
//! Backend selection follows wgpu's own environment handling
//! (`WGPU_BACKEND=...`) via `InstanceDescriptor::from_env_or_default()`, so
//! operators can pin or exclude backends without code changes.

use wgpu::{
    Adapter, Device, DeviceDescriptor, DeviceType as UpstreamDeviceType, Features, Instance,
    InstanceDescriptor, Limits, MemoryHints, Queue, RequestAdapterOptions, Trace,
};

use crate::error::RenderError;

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

/// An initialized GPU context: instance, adapter, logical device, and queue.
///
/// The upstream handles are held privately to keep them alive; this slice
/// issues no work on the queue. Later slices extend this type with pipeline
/// and surface management without changing how it is constructed.
#[derive(Debug)]
pub struct GpuContext {
    #[allow(dead_code)] // Kept alive for later surface/pipeline slices.
    instance: Instance,
    // The adapter is consumed by `request_device` per WebGPU rules but must
    // stay reachable for future re-requests (fresh adapters per device); it
    // is otherwise unread in this slice.
    #[allow(dead_code)]
    adapter: Adapter,
    #[allow(dead_code)] // Kept alive; unused until the pipeline slice.
    device: Device,
    #[allow(dead_code)] // Kept alive; unused until the pipeline slice.
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
}
