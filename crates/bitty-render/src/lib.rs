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
//!   [`FontId`], and owned [`GlyphBitmap`] values. This is the **explicit
//!   fallback** backend (see `Backend selection` below), not the production
//!   default; it has no run-shaping surface, so ligatures and
//!   `font.features` are inert there.
//! - **`harfrust` (0.14.x) is wrapped, never adopted**, behind
//!   [`shaped::SwashSingle::shape_run`] (run shaping over `read-fonts`
//!   faces with per-face shape plans). Font bytes stay owned by this crate;
//!   callers only ever see [`ShapedCluster`] values.
//! - **`swash` (0.2.x) is wrapped, never adopted**, behind
//!   [`glyph::GlyphRasterizer`] via [`shaped::SwashSingle`]. Outline and
//!   bitmap strikes rasterize from the same font bytes the shaper uses
//!   (shared `skrifa`/`read-fonts` parser); callers only ever see
//!   [`FontQuery`], [`FontId`], and owned [`GlyphBitmap`] values.
//! - **`fontdb` (0.24.x, `memmap` off) is wrapped, never adopted**, inside
//!   [`shaped::SwashSingle`]. Faces load as owned bytes under
//!   [`shaped::MAX_FACE_BYTES`]; discovery uses system font directories
//!   plus fontconfig XML on Linux. 0.24 vendors its query-time parser
//!   subset, so the fontdb path carries no external `ttf-parser` edge.
//! - **`ttf-parser` (0.25.x) is wrapped, never adopted**, inside the
//!   private `font_index` module: the persistent discovery index reads only
//!   `name`/`OS/2`/`post` table metadata per face. `fontconfig-parser`
//!   (0.5.x) supplies the Linux/BSD directory and generic-alias list. Both
//!   crates were already in the workspace graph (through the winit
//!   decoration and `fontdb` paths); the index adds direct pinned edges but
//!   no upstream type appears in this crate's public API.
//! - **`skia-safe` is rejected** per ADR-0004 and must not be introduced.
//! - Per ADR-0004's fallback rule, if any upstream becomes unmaintained
//!   for more than twelve months while on this hot path it must be replaced
//!   or narrowly forked under rule 3 of that decision; only this crate's
//!   internals would change because no caller can observe upstream today.
//!
//! # Backend selection (CTX-1607, supersedes the CTX-0957 opt-in landing)
//!
//! The classic crossfont path is a **documented degraded fallback for
//! grapheme clusters** (CTX-1609, issue #1767): with no shaper, a multi-base
//! cluster paints its leading scalar plus its zero-width mark shapes, and the
//! printable continuations (an Indic matra or second conjunct consonant, a
//! Thai floating mark, the second regional indicator, decomposed Hangul jamo)
//! stay in Terminal Truth for copy, search, and accessibility. Nothing is
//! faked at the base's pen position. [`grid::GridRenderer::render_shaped`]
//! paints every shaped glyph of the cluster; the runtime tick diagnostics
//! report `crossfont=true` while that degraded path is the active one.
//!
//! The shaped stack ([`shaped::SwashSingle`]: `fontdb` discovery +
//! `harfrust` shaping + `swash` rasterization) is the **production
//! default**. Constructing it through [`fallback::FallbackRasterizer`] and
//! calling [`grid::GridRenderer::render_shaped`] enables programming
//! ligatures (`calt`/`liga`/`clig`/`dlig`), configured `font.features`, and
//! the `font.disable_ligatures` policy (`never`/`cursor`/`always`).
//!
//! [`crossfont_backend::CrossFontRasterizer`] remains the **explicit
//! fallback** for platforms or fonts the shaped stack cannot handle: the
//! runtime selects it when the shaped stack fails to initialize (missing
//! font database, face rejected by `fontdb`/`harfrust`) or when
//! `BITTY_TEXT_STACK=classic` is set. It is wrapped in the same
//! [`fallback::FallbackRasterizer`] chain and served by the per-cell
//! [`grid::GridRenderer::render`] path; the grid's shaped entry point routes
//! a non-shaping rasterizer there automatically
//! ([`shaped::ShapedRunSource::shaping_supported`]).
//!
//! The fallback is observable, never silent: the runtime records the
//! resolved stack plus a fallback note (`Runtime::text_stack` /
//! `Runtime::text_stack_note`), logs it at startup, and `bitty doctor`
//! reports it in the `text-stack` row. The deterministic headless
//! rasterizer used by golden tests is a test stack, not a production font
//! path.
//!
//! Font discovery is cached for the shaped stack in
//! `$XDG_CACHE_HOME/bitty/font-index-v2` (see `font_index`; the upstream
//! `fontdb` scan remains the fallback): a cold process walks the font
//! directories once and reads only the `name`/`OS/2`/`post` tables of each
//! face in parallel, then writes a versioned index atomically; every start
//! runs discovery again and reconciles the cache against the current path
//! set (new/removed/changed fonts, fontconfig changes, foreign entries),
//! reparsing only changed/new paths before building a metadata-only `fontdb`
//! database. A warm start reads no face bytes at all; the cold build's
//! bounded reader reads only those three metadata tables (never outline or
//! layout data), and a file it cannot name falls back to fontdb's loader,
//! which reads the whole file. The runtime's `GridRenderer::new` still loads
//! the primary face at startup to build its glyph cache. The v2 cache carries
//! an FNV-1a content hash over its generic/face lines, so valid-line
//! corruption is rejected and rebuilt. Face bytes still load lazily
//! for the faces a frame needs, and missing-glyph fallback loads a covering
//! face on demand. The cache is capped at 16,384 faces / 32 MiB: faces past
//! the cap are neither persisted nor visible to a cached start, and overflow
//! paths are reparsed on every start while the retained prefix keeps the file
//! read-only. `BITTY_FONT_INDEX=0` restores the upstream full scan for
//! diagnosis. A stale, corrupt, or unwritable cache never fails startup.
//!
//! Both stacks must keep identical **plain-ASCII cell metrics**: glyph
//! origins and the cell box come from the grid, and the shaped stack
//! reproduces FreeType's ascender/descender rounding for the pen baseline
//! so a font switch never shifts text by a pixel. Shaped output that breaks
//! grid tiling (overlapping or overrunning clusters) or the monospace
//! advance invariant degrades to per-cell unshaped rendering instead of
//! painting a misaligned glyph.
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
//! and the scene paint path ([`scene`]: an admitted `bitty_rich::scene::Scene`
//! laid out and clipped to a leaf content rect, with a bounded paint list,
//! one damage rectangle, and the same CPU text seam as cell content),
//! CPU batch translation ([`batch`]: `DrawList` -> bounded vertex batches +
//! atlas dirty-region bookkeeping), GPU presentation resources plus WGSL
//! fill/glyph pipelines (crate-private `pipeline` module, consumed by
//! [`gpu::Surface::present_draw_list`]), and — under the opt-in
//! `sw-fallback` feature — a CPU compositor that exercises the whole
//! pipeline headlessly (`snapshot -> RGBA`).
//!
//! Explicitly **out of scope** and not implemented yet: cursor visuals
//! and scrollback viewport rendering (deferred inside [`grid`]), and
//! subpixel RGB rendering (the *policy* is decided:
//! [`atlas::coverage_policy_for`] earns subpixel only on LCD + non-HiDPI
//! outputs, and every serving path implements the grayscale side; the
//! triplet-preserving renderer itself is still deferred). Presentation pipelines and WGSL shaders
//! **are** implemented: [`batch`] translates an owned [`DrawList`] into
//! bounded vertex batches plus atlas-upload bookkeeping on any CPU, and the
//! crate-private `pipeline` module owns the `wgpu` fill +
//! glyph pipelines, the `R8` atlas texture with dirty-region uploads, and
//! chunked draws consumed by [`gpu::Surface::present_draw_list`].
//! Window-surface attachment **is** implemented here as the owned [`gpu::Surface`] wrapper around
//! `bitty-platform`'s [`bitty_platform::SurfaceTarget`]; no `wgpu` type leaks
//! except through that owned wrapper. None of the remaining deferred items may
//! be described as existing until they land with evidence.
//!
//! # Scene paint path (OQ-051, CTX-1628 / bitty#2760)
//!
//! [`scene::paint_scene`] consumes the admitted `Scene` model from
//! `bitty-rich` and produces [`scene::ScenePaint`]: ordered fills, text lines,
//! and image placement references, all clipped to the leaf content rectangle
//! the compositor owns. Ops that would fall outside the rect are dropped by
//! clipping (never painted), and [`scene::ScenePaint::damage`] is the bounding
//! box of the emitted op rectangles intersected with that rect, so a scene
//! revision never damages outside its leaf. Over-budget scenes are refused
//! with a typed [`scene::ScenePaintError`]; nothing is clamped or silently
//! substituted. A text op stores exactly the cell run the glyph seam paints
//! (the stored line is clipped, not just clipped at rasterization), so the
//! plain-text and accessibility projections read painted bytes only.
//! Text becomes glyphs through the existing overlay text seam
//! ([`grid::GridRenderer::overlay_text_glyphs`]), so the path stays headless
//! and CPU-verifiable. Image nodes stay references: the admitted image plane
//! keeps owning decode and blit, and this crate adds no second decode seam.
//!
//! Integration status: the shipped present loop consumes this module. The
//! `bitty-runtime` present path resolves each visible panel leaf's content
//! rect and calls `PanelRuntime::scene_paint_for` (CTX-1630), so fills,
//! overlay-seam glyphs, and bounded damage reach a real frame. The remaining
//! slice of bitty#2760 is the `ui.rich` mount path: `bitty.ui.mount` still
//! validates through the v1 reader, so a plugin mount does not yet attach a
//! `Scene` to its panel in production. Do not describe plugin rich content as
//! shipping until that mount slice lands.
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
//! (a fake surface that holds a `PhysicalSize` extent and composites
//! `DrawList`+`Atlas` onto an in-memory RGBA buffer via the same
//! `software::draw_list_onto` path the GPU backend will share). Headless
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
//! refuses allocations that cannot fit, [`cache::GlyphCache`] enforces entry
//! and retained-pixel-byte caps with deterministic eviction, and the software surface caps its
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
mod block;
pub mod cache;
pub mod chrome;
pub mod crossfont_backend;
pub mod error;
pub mod fallback;
pub mod font_catalog;
mod font_index;
pub mod frame;
pub mod geometry;
pub mod glyph;
pub mod gpu;
pub mod grid;
pub mod hidpi;
pub(crate) mod pipeline;
pub mod scene;
pub mod shaped;
pub mod telemetry;
pub mod window;

#[cfg(feature = "sw-fallback")]
pub mod software;

pub use cache::{DEFAULT_GLYPH_CACHE_BYTE_CAPACITY, GlyphCache};
pub use chrome::{
    CHROME_BORDER_PX, CHROME_CORNER_MASKS, CHROME_CORNERS_BOX, CHROME_CORNERS_ROUNDED,
    CHROME_ELEVATE_MIX, CHROME_PADDING_PX, CHROME_RADIUS_PX, CORNER_ORDER, ChromeCornerGlyphs,
    ChromeElevation, ChromeFrame, ChromeGlyphCoverage, ChromePaint, ChromeState, ChromeTokens,
    MIN_CHROME_FOCUS_CONTRAST, inset, outline_ring,
};
pub use crossfont_backend::CrossFontRasterizer;
pub use error::RenderError;
pub use fallback::{
    BLOCK_FIRST, BLOCK_LAST, BRAILLE_FIRST, BRAILLE_LAST, FallbackRasterizer, ResolvedGlyph,
    is_block_element, is_braille_pattern, is_tui_graph_scalar,
};
pub use geometry::{ExtentPx, RectPx};
pub use glyph::{
    FontId, FontMetrics, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer,
    MAX_CACHED_BITMAP_BYTES, RasterKey, ShapedBitmap,
};
pub use grid::{
    AppliedDpiScale, CellMetrics, DrawList, FillRect, GlyphAtlas, GlyphInstance, GridRenderer,
    ImageBlit, RenderCounters, RoundedClip, RoundedFill, ScrollCopy, SnapshotDamage, ThemePalette,
    curly_amplitude_px, curly_segment_offset_px, curly_sine_offset, curly_step_px,
    curly_wavelength_px, dashed_params_px, dotted_params_px, double_gap_px, pattern_thickness,
};
pub use hidpi::{
    MAX_DPI_SCALE, MAX_SCALED_POINT_SIZE, MIN_DPI_SCALE, grid_from_surface_extent,
    sanitize_dpi_scale, scaled_cell_metrics, scaled_cell_side, scaled_point_size,
    surface_extent_for_grid,
};
pub use pipeline::UNDERLINE_WGSL;
pub use scene::{
    SCENE_PAINT_MAX_BORDER_CELLS, SCENE_PAINT_MAX_OPS, ScenePaint, ScenePaintBudget,
    ScenePaintCache, ScenePaintError, ScenePaintOp, ScenePaintStyle, SceneSurface, SceneTextLine,
    paint_scene, paint_scene_prefix, paint_scene_with_style,
};
pub use shaped::{
    ASCII_PRINTABLE_FIRST, ASCII_PRINTABLE_LAST, CJK_ADVANCE_EPSILON_PX, GRID_ADVANCE_EPSILON_PX,
    GlyphSource, MAX_DYNAMIC_CACHE_ENTRIES, MAX_FACE_BYTES, MAX_LOADED_FACES, MAX_RUN_CACHE_BYTES,
    MAX_RUN_CACHE_ENTRIES, MAX_SHAPE_PLANS_PER_FACE, MAX_SHAPED_GLYPH_CACHE_BYTES,
    MAX_SHAPED_GLYPH_CACHE_ENTRIES, PROGRAMMING_LIGATURE_TAGS, RunAttrs, RunCacheKey, RunDirection,
    ShapePlanKey, ShapedCluster, ShapedGlyphKey, ShapedRun, ShapedRunSource, SwashSingle,
    ascii_fast_path_eligible, cells_for_range, cells_for_range_in, collect_run_text,
    cursor_intersects_ligature, features_are_unshaped, features_for_policy, form_runs_for_row,
    form_runs_for_row_at_levels, harfrust_features, is_printable_ascii_byte,
    is_printable_ascii_str, run_is_ascii_candidate,
};
pub use window::{MAX_WINDOW_PADDING_PX, clamp_window_padding, padded_content_rect};
