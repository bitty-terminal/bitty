//! `bitty-render`: owned rendering skeleton for the Bitty microkernel core.
//!
//! The crate implements the render row of the Core Workspace Topology
//! (ADR-0003): it plans frames from damage descriptors, owns glyph atlas
//! math and a bounded glyph cache, and wraps upstream rasterization behind a
//! Bitty-owned trait. It has **no workspace-crate dependencies** in this
//! slice: the damage-aware frame plan consumes a generic descriptor defined
//! here, and the real wiring to `bitty-term-state` render snapshots arrives
//! with the grid-integration slice. Exactly two third-party dependencies are
//! permitted, both by the accepted rows of ADR-0004.
//!
//! # Upstream boundary (ADR-0004 "Adopt" / "Wrap" rows)
//!
//! - **`wgpu` (~25.x line) is adopted** as the graphics abstraction inside
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
//! This is deliberately a skeleton. Implemented here: frame planning from
//! pixel-domain damage ([`frame::plan_frame`]), atlas layout math
//! ([`atlas`]), the rasterizer contract plus wrapper and cache ([`glyph`],
//! [`crossfont_backend`], [`cache`]), GPU context creation with owned errors
//! ([`gpu`]), and an opt-in CPU fallback seed (`sw-fallback` feature).
//!
//! Explicitly **out of scope** and not implemented yet: window-surface
//! attachment (needs the `bitty-platform` integration slice to define the
//! raw-window-handle boundary), pipelines/shaders and vertex upload (the
//! first place where `bytemuck` would be required — see below), real grid
//! snapshot wiring from `bitty-term-state`, text shaping/HarfBuzz (deferred
//! to the text RFC named in ADR-0004), and subpixel RGB rendering policy.
//! None of these may be described as existing until they land with evidence.
//!
//! # Headless friendliness and what CI does and does not verify
//!
//! CI runs on GPU-less Linux runners. Everything in this crate except
//! actually requesting a live adapter/device is pure logic and is unit-tested
//! there: rect algebra, frame-plan decisions and coalescing, shelf-pack atlas
//! math, bitmap conversion invariants of the crossfont wrapper, and the
//! [`glyph::GlyphRasterizer`] contract against an in-crate fake rasterizer.
//! What plain CI **cannot** verify: any code path that reaches a real GPU
//! (adapter enumeration, device creation, eventual present). Those paths are
//! exercised only by the integration test in `tests/gpu_integration.rs`,
//! which skips itself unless the environment variable
//! `BITTY_RENDER_GPU_TESTS=1` is set on a machine with a working driver. The
//! `sw-fallback` software path is also outside the default feature set and
//! is therefore compiled and tested locally (`--features sw-fallback`), not
//! in the default CI matrix.
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
//! This crate carries `#![forbid(unsafe_code)]`. Neither wgpu nor crossfont
//! requires unsafe code in *callers* for anything implemented in this slice;
//! `bytemuck` is intentionally not introduced. The future vertex-upload
//! slice will need `Pod` bit-casting and must revisit this lint then, with
//! the narrowest possible exception (module-scoped `allow(unsafe_code)` plus
//! a reviewed justification) rather than dropping the forbid wholesale.
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

#![forbid(unsafe_code)]

pub mod atlas;
pub mod cache;
pub mod crossfont_backend;
pub mod error;
pub mod frame;
pub mod geometry;
pub mod glyph;
pub mod gpu;

#[cfg(feature = "sw-fallback")]
pub mod software;

pub use cache::GlyphCache;
pub use crossfont_backend::CrossFontRasterizer;
pub use error::RenderError;
pub use geometry::{ExtentPx, RectPx};
pub use glyph::{FontId, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, RasterKey};
