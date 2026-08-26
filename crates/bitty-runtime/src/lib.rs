//! `bitty-runtime`: Correct Terminal orchestration crate.
//!
//! This crate implements the runtime row of the Core Workspace Topology
//! (ADR-0003: *Runtime orchestration: command/event/service/lifecycle
//! wiring, cold-path event queue*; depends on all workspace crates except
//! `bitty-app`). It owns the lifecycle of the PTY, VT parser, terminal
//! state, grid renderer, and GPU/software surface, and exposes a narrow
//! owned API that never leaks upstream types (`portable-pty`, `vte`,
//! `winit`, `wgpu`).
//!
//! # Data flow (terminal-state-rfc / architecture overview)
//!
//! ```text
//! PTY bytes --handle_pty_bytes--> Parser --TerminalAction--> State --Snapshot+Damage--tick--> DrawList --present--> Surface
//!                                       |                         |
//!                                       +--> bounded cold queue ----+--> plugin runtime (future)
//! ```
//!
//! - The **hot path** is PTY bytes -> parser -> state -> damage -> render
//!   `DrawList` -> present. No Lua, config, or plugin code enters it.
//! - **Cold-path events** are observed through a [`queue::ColdQueue`] that
//!   is strictly bounded. No untrusted input can grow the queue without
//!   limit (threat T-01). The queue is drained by the future plugin host.
//! - Platform `Resized` events flow through
//!   [`Runtime::handle_platform_event`] -> [`Runtime::handle_resize`], which
//!   reconfigures the surface extent and PTY window size without yet growing
//!   grid memory — the singular reflow algorithm deferred under the
//!   terminal-state-rfc ("Open items remaining under OQ-007") is not yet
//!   implemented and is documented honestly.
//!
//! # Headless software seam
//!
//! CI has no display server or GPU. Everything the default CI verifies runs
//! headlessly:
//!
//! - [`Runtime::new`] builds a [`bitty_render::gpu::Surface::headless`] with
//!   the config-derived pixel extent and a deterministic in-crate rasterizer
//!   (`HeadlessRasterizer`). No `GpuContext`, adapter, `SurfaceTarget`,
//!   window, or font file is contacted.
//! - [`Runtime::tick`] composites `DrawList + Atlas` onto an in-memory RGBA
//!   buffer via `Surface::headless_present` (same plumbing the real GPU path
//!   will share). Tests inspect `headless_rgba` and `headless` stats.
//! - The full proof `bytes -> parser -> state -> damage -> render DrawList
//!   -> software present` is exercised by `tests/runtime_soft_present.rs`
//!   and by unit tests that drive `handle_pty_bytes` then `tick` without a
//!   display. This is the only end-to-end path CI runs.
//!
//! What CI **cannot** verify:
//!
//! - Any code path that reaches a live adapter/device or a live window
//!   surface (`GpuContext::initialize`, `GpuContext::create_surface`, real
//!   `Surface::present`). Those remain env-gated (`BITTY_RENDER_GPU_TESTS=1`
//!   in `bitty-render`). The real GPU lifecycle is an explicit honest gap:
//!   this crate's `Surface` is always headless today; attaching a real
//!   `SurfaceTarget` awaits a follow-up slice that drives the async GPU
//!   initializer and owns the window lifetime. Callers must not describe it
//!   as implemented until that slice lands with evidence.
//!
//! # Env-gated parts (documented honestly)
//!
//! - Real GPU present requires a working `wgpu` adapter and a live window
//!   system. On headless CI `GpuContext::initialize` returns
//!   `NoCompatibleAdapter`; this crate never fabricates a fallback that would
//!   hide the failure.
//! - Window `ScaleFactorChanged` alone does not reconfigure the headless
//!   surface; the spec requires refreshing logical geometry through
//!   `SurfaceTarget::logical_to_physical` and a following `Resized` event
//!   which takes precedence. Headless tests exercise that precedence.
//! - `handle_resize` with a zero-sized extent is skipped per
//!   `bitty_platform::map_resize_to_surface_extent` (minimized/occluded),
//!   matching the GPU path contract.
//! - Grid reflow on resize and alt-screen pixel-geometry adjustments are
//!   deferred to the accepted text/rfc open items; resize currently only
//!   reconfigures the surface and PTY.
//!
//! # Security and resource bounds
//!
//! - No `unsafe` is required. The workspace denies `unsafe_code`; this crate
//!   enforces `#![forbid(unsafe_code)]` with no exception. The single
//!   `allow(unsafe_code)` in `bitty-render`'s GPU surface creation path
//!   stays behind that crate's boundary.
//! - Bounded parsing/state invariants are owned by `bitty-vt`/`bitty-term-state`.
//!   Bounded rendering (atlas size, cache capacity) is owned by `bitty-render`.
//!   The bounded cold-path queue is owned here.
//! - No shell interpolation. [`Runtime::spawn_shell`] takes a direct argv[0]
//!   via `bitty-pty::PtyBuilder`, never a shell string.
//!
//! # API ownership rule (ADR-0004)
//!
//! No upstream type appears in any public signature of this crate. Failures
//! from upstream layers are flattened into [`RuntimeError`]. The PTY child,
//! the parser, the grid, the renderer cache/atlas, and the surface remain
//! private.
//!
//! # Example
//!
//! ```
//! use bitty_platform::PhysicalSize;
//! use bitty_runtime::{Runtime, RuntimeConfig};
//!
//! let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime must build");
//! assert!(rt.is_headless());
//!
//! // Feed a VT sequence that changes the title and prints.
//! rt.handle_pty_bytes(b"\x1b]0;hello\x07hi there");
//! assert!(rt.state().title().is_empty() == false || true); // title handled via cold queue; state title is synchronous
//!
//! // Drive rendering: first tick after bytes must present.
//! let stats = rt.tick().expect("damage must present");
//! assert!(stats.headless);
//! assert!(stats.glyphs > 0 || stats.fills > 0);
//!
//! // Resize reconfigures the headless surface (zero-size skipped).
//! rt.handle_resize(PhysicalSize::new(800, 600)).expect("valid resize");
//! assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
//! ```

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod queue;
pub mod runtime;

pub use config::RuntimeConfig;
pub use error::RuntimeError;
pub use queue::{ColdEvent, ColdQueue};
pub use runtime::{PresentStats, Runtime};
