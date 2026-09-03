//! `bitty-render`: owned rendering for the Bitty microkernel core.
//!
//! The crate implements the render row of the Core Workspace Topology
//! (ADR-0003): it plans frames from damage descriptors, renders terminal
//! snapshots into owned draw records through the grid pipeline
//! ([`grid::GridRenderer`]), owns glyph atlas math and a bounded glyph
//! cache, and wraps upstream rasterization behind a Bitty-owned trait.
//! Per ADR-0003 dependency rule 3, the grid pipeline reads **only** the
//! public `Snapshot`/`Damage` surface of `bitty-term-state`; no private
//! structure is reached into and terminal state is never mutated. Exactly
//! two third-party dependencies are permitted, both by the accepted rows of
//! ADR-0004.
//!
//! # Upstream boundary (ADR-0004 "Adopt" / "Wrap" rows)
//!
//! - **`wgpu` (~26.x line) is adopted** as the graphics abstraction inside
//!   [`gpu`]. Its types never appear anywhere in this crate's public API:
//!   every upstream failure is flattened into the owned [`RenderError`], and
//!   adapter facts are re-described by owned enums ([`gpu::AdapterSummary`]).
//! - **`crossfont` is wrapped, never adopted**, behind [`glyph::GlyphRasterizer`]
//!   via [`crossfont_backend::CrossFontRasterizer`]. Font discovery uses
//!   crossfont defaults (CoreText on macOS, DirectWrite on Windows,
//!   FreeType/fontconfig elsewhere); callers only ever see [`FontQuery`],
//!   [`FontId`], and owned [`GlyphBitmap`] values.
//! - **`skia-safe` is rejected** per ADR-0004 and must not be introduced.
//! - Per ADR-0004's fallback rule, if either upstream becomes unmaintained
//!   for more than twelve months while on this hot path it must be replaced
//!   or narrowly forked under rule 3 of that decision; only this crate's
//!   internals would change because no caller can observe upstream today.
//!
//! # Scope boundaries of this slice
//!
//! Implemented here: frame planning from pixel-domain damage
//! ([`frame::plan_frame`]), atlas layout math ([`atlas`]), the rasterizer
//! contract plus wrapper and cache ([`glyph`], [`crossfont_backend`],
//! [`cache`]), GPU context creation with owned errors ([`gpu::GpuContext`]),
//! the owned GPU surface lifecycle ([`gpu::Surface`] created from
//! [`bitty_platform::SurfaceTarget`] via [`gpu::GpuContext::create_surface`],
//! with `configure`/`resize`/`present` paths), the grid pipeline
//! ([`grid::GridRenderer`] `Snapshot`/`Damage` -> `DrawList`/`Atlas`),
//! CPU batch translation ([`batch`]: `DrawList` -> bounded vertex batches +
//! atlas dirty-region bookkeeping), GPU presentation resources plus WGSL
//! fill/glyph pipelines (crate-private `pipeline` module, consumed by
//! [`gpu::Surface::present_draw_list`]), and — under the opt-in
//! `sw-fallback` feature — a CPU compositor that exercises the whole
//! pipeline headlessly (`snapshot -> RGBA`).
//!
//! Explicitly **out of scope** and not implemented yet: cursor visuals
//! and scrollback viewport rendering (deferred inside [`grid`]), text
//! shaping/HarfBuzz (deferred to the text RFC named in ADR-0004), and
//! subpixel RGB rendering policy. Presentation pipelines and WGSL shaders
//! **are** implemented: [`batch`] translates an owned [`DrawList`] into
//! bounded vertex batches plus atlas-upload bookkeeping on any CPU, and the
//! crate-private [`pipeline`](crate::pipeline) module owns the `wgpu` fill +
//! glyph pipelines, the `R8` atlas texture with dirty-region uploads, and
//! chunked draws consumed by [`gpu::Surface::present_draw_list`].
//! Window-surface attachment **is** implemented here as the owned [`gpu::Surface`] wrapper around
//! `bitty-platform`'s [`bitty_platform::SurfaceTarget`]; no `wgpu` type leaks
//! except through that owned wrapper. None of the remaining deferred items may
//! be described as existing until they land with evidence.
//!
//! # Headless friendliness and what CI does and does not verify
//!
//! CI runs on GPU-less Linux runners. Everything in this crate except
//! actually requesting a live adapter/device or a live window surface is pure
//! logic and is unit-tested there: rect algebra, frame-plan decisions and
//! coalescing, shelf-pack atlas math, bitmap conversion invariants of the
//! crossfont wrapper, the [`glyph::GlyphRasterizer`] contract against an
//! in-crate fake rasterizer, the full grid pipeline including output
//! determinism (`snapshot + damage -> DrawList`) against deterministic fake
//! fonts, and the **headless GPU-surface seam**: [`gpu::Surface::headless`]
//! (a fake surface that holds a [`PhysicalSize`] extent and composites
//! `DrawList`+`Atlas` onto an in-memory RGBA buffer via the same
//! [`software::draw_list_onto`] path the GPU backend will share). Headless
//! surface tests exercise configuration, resize, and present composition
//! without any display server or adapter.
//!
//! What plain CI **cannot** verify: any code path that reaches a real GPU
//! (adapter enumeration, device creation, real surface creation from a
//! [`bitty_platform::SurfaceTarget`], and present of the swap-chain texture).
//! Those paths are exercised only by the integration test in
//! `tests/gpu_integration.rs`, which skips itself unless the environment
//! variable `BITTY_RENDER_GPU_TESTS=1` is set on a machine with a working
//! driver (and a window system when surface tests run). The `sw-fallback`
//! software path is also outside the default feature set and is therefore
//! compiled and tested locally
//! (`cargo test -p bitty-render --features sw-fallback`); under that flag
//! the same pipeline runs end to end from snapshot bytes to RGBA bytes.
//!
//! # Memory bounds
//!
//! All buffers are bounded at construction: [`GlyphBitmap::try_new`] rejects
//! length mismatches and capacity overflows, [`atlas::AtlasLayout::allocate`]
//! refuses allocations that cannot fit, [`cache::GlyphCache`] enforces an
//! entry cap with deterministic eviction, and the software surface caps its
//! byte size. Unbounded growth on untrusted input is forbidden by the
//! security corpus.
//!
//! # Unsafe code policy
//!
//! This crate denies `unsafe_code` at the workspace and crate level. The only
//! exception is [`gpu`]'s surface creation path: `wgpu::Instance::create_surface`
//! consumes `raw-window-handle` 0.6 handles supplied by
//! [`bitty_platform::SurfaceTarget::with_raw_handles`]. That call bridges raw
//! handles into a `wgpu::Surface`; it requires `unsafe` to borrow the raw
//! `DisplayHandle`/`WindowHandle` (see `GPU Surface Seam` in [`gpu`]). The
//! `unsafe` is confined to `gpu::Surface` construction (a single `unsafe`
//! block with a safety comment) and does not leak. `crossfont` still requires
//! no caller unsafe. `bytemuck` is intentionally not introduced: vertex bytes
//! are serialized with explicit little-endian `to_le_bytes` calls, so no
//! `Pod` bit-casting (and no second `unsafe` scope) is required.
//!
//! # Example
//!
//! ```
//! use bitty_render::frame::{DamageDescriptor, plan_frame};
//! use bitty_render::geometry::{ExtentPx, RectPx};
//!
//! struct Blink {
//!     extent: ExtentPx,
//!     cells: Vec<RectPx>,
//! }
//!
//! impl DamageDescriptor for Blink {
//!     fn extent(&self) -> ExtentPx { self.extent }
//!     fn damaged_regions(&self) -> &[RectPx] { &self.cells }
//! }
//!
//! let frame = Blink {
//!     extent: ExtentPx::new(800, 600),
//!     cells: vec![
//!         RectPx::new(0, 0, 10, 10),
//!         RectPx::new(5, 5, 10, 10), // touches the first region: coalesced
//!     ],
//! };
//!
//! let plan = plan_frame(&frame);
//! assert_eq!(plan.dirty_rects.len(), 1);
//! assert_eq!(plan.dirty_rects[0], RectPx::new(0, 0, 15, 15));
//! ```

#![deny(unsafe_code)]

pub mod atlas;
pub mod batch;
pub mod cache;
pub mod crossfont_backend;
pub mod error;
pub mod frame;
pub mod geometry;
pub mod glyph;
#[allow(unsafe_code)]
pub mod gpu;
pub mod grid;
pub(crate) mod pipeline;

#[cfg(feature = "sw-fallback")]
pub mod software;

pub use cache::GlyphCache;
pub use crossfont_backend::CrossFontRasterizer;
pub use error::RenderError;
pub use geometry::{ExtentPx, RectPx};
pub use glyph::{FontId, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, RasterKey};
pub use grid::{
    CellMetrics, DrawList, FillRect, GlyphAtlas, GlyphInstance, GridRenderer, RenderCounters,
    SnapshotDamage,
};
