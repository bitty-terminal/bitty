//! `bitty-render`: owned rendering for the Bitty microkernel core.
//!
//! The crate implements the render row of the Core Workspace Topology
//! (ADR-0003): it plans frames from damage descriptors, renders terminal
//! snapshots into owned draw records through the grid pipeline
//! ([`grid::GridRenderer`]), owns glyph atlas math and a bounded glyph
//! cache, wraps upstream rasterization behind a Bitty-owned trait, and
//! walks the documented font chain per glyph for TUI-graph coverage
//! ([`fallback::FallbackRasterizer`]).
//! Per ADR-0003 dependency rule 3, the grid pipeline reads **only** the
//! public `Snapshot`/`Damage` surface of `bitty-term-state`; no private
//! structure is reached into and terminal state is never mutated. Upstream
//! crates are permitted only by the accepted rows of ADR-0004.
//!
//! # Upstream boundary (ADR-0004 "Adopt" / "Wrap" rows)
//!
//! - **`wgpu` (~26.x line) is adopted** as the graphics abstraction inside
//!   [`gpu`]. Its types never appear anywhere in this crate's public API:
//!   every upstream failure is flattened into the owned [`RenderError`], and
//!   adapter facts are re-described by owned enums ([`gpu::AdapterSummary`]).
//! - **`crossfont` is wrapped, never adopted**, behind
//!   [`glyph::GlyphRasterizer`] via
//!   [`crossfont_backend::CrossFontRasterizer`]. Font discovery uses
//!   crossfont defaults (CoreText on macOS, DirectWrite on Windows,
//!   FreeType/fontconfig elsewhere); callers only ever see [`FontQuery`],
//!   [`FontId`], and owned [`GlyphBitmap`] values. This is the production
//!   default backend (see `Backend selection` below).
//! - **`harfrust` (0.14.x) is wrapped, never adopted**, behind
//!   [`shaped::SwashSingle::shape_run`] (run shaping over `read-fonts`
//!   faces with per-face shape plans). Font bytes stay owned by this crate;
//!   callers only ever see [`ShapedCluster`] values.
//! - **`swash` (0.2.x) is wrapped, never adopted**, behind
//!   [`glyph::GlyphRasterizer`] via [`shaped::SwashSingle`]. Outline and
//!   bitmap strikes rasterize from the same font bytes the shaper uses
//!   (shared `skrifa`/`read-fonts` parser); callers only ever see
//!   [`FontQuery`], [`FontId`], and owned [`GlyphBitmap`] values.
//! - **`fontdb` (0.23.x, `memmap` off) is wrapped, never adopted**, inside
//!   [`shaped::SwashSingle`]. Faces load as owned bytes under
//!   [`shaped::MAX_FACE_BYTES`]; discovery uses system font directories
//!   plus fontconfig XML on Linux.
//! - **`skia-safe` is rejected** per ADR-0004 and must not be introduced.
//! - Per ADR-0004's fallback rule, if any upstream becomes unmaintained
//!   for more than twelve months while on this hot path it must be replaced
//!   or narrowly forked under rule 3 of that decision; only this crate's
//!   internals would change because no caller can observe upstream today.
//!
//! # Backend selection (CTX-0957 additive landing, DEC-0095)
//!
//! Production default stays **`crossfont`**: every production construction
//! site builds [`crossfont_backend::CrossFontRasterizer`] (wrapped in
//! [`fallback::FallbackRasterizer::with_default_chain`] exactly as before),
//! so the additive landing changes zero production behavior — CJK scalars
//! such as U+6F22/U+5B57 keep the dynamic per-glyph fontconfig fallback
//! only the crossfont path provides today.
//!
//! The shaped stack ([`shaped::SwashSingle`]: `fontdb` discovery +
//! `harfrust` shaping + `swash` rasterization) is the **explicit opt-in**:
//! construct it directly via [`shaped::SwashSingle::new`] and wrap it in
//! [`fallback::FallbackRasterizer::with_default_chain`], exactly as
//! `tests/shaped_parity.rs` does. There is deliberately no flag, env var,
//! or silent default flip — callers that want shaping name it. Full removal
//! of the crossfont wrap (backend delete, runtime rewire, `deny.toml`
//! `dwrote` revoke replay) is deferred to **CTX-0961**, which owns the
//! CJK/script chain policy, the dynamic-fallback strategy, and the per-OS
//! discovery evidence that must land first.
//!
//! # Scope boundaries of this slice
//!
//! Implemented here: frame planning from pixel-domain damage
//! ([`frame::plan_frame`]), atlas layout math ([`atlas`]), the rasterizer
//! contract plus both backends and cache ([`glyph`], [`crossfont_backend`],
//! [`shaped`], [`cache`]), GPU context creation with owned errors ([`gpu::GpuContext`]),
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
//! and scrollback viewport rendering (deferred inside [`grid`]), grid run
//! shaping with ligature spans (Phase B extends the [`shaped`] skeleton:
//! run caches, cluster-to-cell emission, cursor-policy un-shaping), and
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
//! coalescing, shelf-pack atlas math, bitmap conversion invariants of both
//! backend wrappers, the [`glyph::GlyphRasterizer`] contract against an
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
//! This crate forbids `unsafe_code` at the crate level (stronger than the
//! workspace `deny`). Surface creation uses the safe
//! `wgpu::Instance::create_surface` path: [`bitty_platform::SurfaceTarget`]
//! implements `raw-window-handle` 0.6 `HasWindowHandle` + `HasDisplayHandle`,
//! so an owned target clone yields a `wgpu::Surface<'static>` with no
//! `create_surface_unsafe` and no lifetime `transmute` (see `Safety: no
//! `unsafe`` in [`gpu`]). Neither font stack (`crossfont`, nor
//! `harfrust`/`swash`/`skrifa`/`read-fonts`/`fontdb`) requires caller
//! `unsafe`; the shaped stack's internal parsing `unsafe` (byte casting in
//! `bytemuck` and `swash`'s table readers) is upstream-audited and recorded
//! in the CTX-0957 implementation evidence. `bytemuck` arrives only
//! transitively through the font stacks: vertex bytes are still serialized
//! with explicit little-endian `to_le_bytes` calls, so no `Pod` bit-casting
//! (and no further `unsafe`) is required.
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
pub mod batch;
pub mod cache;
pub mod crossfont_backend;
pub mod error;
pub mod fallback;
pub mod frame;
pub mod geometry;
pub mod glyph;
pub mod gpu;
pub mod grid;
pub mod hidpi;
pub(crate) mod pipeline;
pub mod shaped;
pub mod window;

#[cfg(feature = "sw-fallback")]
pub mod software;

pub use cache::GlyphCache;
pub use crossfont_backend::CrossFontRasterizer;
pub use error::RenderError;
pub use fallback::{
    BLOCK_FIRST, BLOCK_LAST, BRAILLE_FIRST, BRAILLE_LAST, FallbackRasterizer, ResolvedGlyph,
    is_block_element, is_braille_pattern, is_tui_graph_scalar,
};
pub use geometry::{ExtentPx, RectPx};
pub use glyph::{
    FontId, FontMetrics, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, RasterKey,
};
pub use grid::{
    AppliedDpiScale, CellMetrics, DrawList, FillRect, GlyphAtlas, GlyphInstance, GridRenderer,
    ImageBlit, RenderCounters, RoundedClip, RoundedFill, SnapshotDamage, ThemePalette,
};
pub use hidpi::{
    MAX_DPI_SCALE, MAX_SCALED_POINT_SIZE, MIN_DPI_SCALE, grid_from_surface_extent,
    sanitize_dpi_scale, scaled_cell_metrics, scaled_cell_side, scaled_point_size,
    surface_extent_for_grid,
};
pub use shaped::{
    CJK_ADVANCE_EPSILON_PX, GlyphSource, MAX_FACE_BYTES, MAX_LOADED_FACES, MAX_RUN_CACHE_ENTRIES,
    MAX_SHAPE_PLANS_PER_FACE, MAX_SHAPED_GLYPH_CACHE_ENTRIES, PROGRAMMING_LIGATURE_TAGS, RunAttrs,
    RunCacheKey, ShapePlanKey, ShapedCluster, ShapedGlyphKey, ShapedRun, SwashSingle,
    cells_for_range, collect_run_text, features_for_policy, form_runs_for_row, harfrust_features,
};
pub use window::{MAX_WINDOW_PADDING_PX, clamp_window_padding, padded_content_rect};
